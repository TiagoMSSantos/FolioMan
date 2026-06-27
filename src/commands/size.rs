//! `size [TICKERS]` — suggested position sizes for the growth picks: weight ∝ score ÷ volatility
//! (vol-target). READ-ONLY, never trades — you still type the qty into `trade` yourself. No TICKERS
//! -> the watchlist. Names that fail the growth gate are dropped (nothing to size). NOT advice.

use crate::picks::{crypto_adjust, growth_score, nupl_factor, perf_pct, size_weights};
use crate::{config, fetch};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let eu_infl = if settings.inflation_adjust.enabled {
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    // same fetch shape as `perf`/`screen`; intraday + news off (sizing needs neither).
    let mut quotes = fetch::quotes(
        &client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, false,
        &settings.anchor_windows, eu_infl.as_ref(),
    )
    .await;

    // (Item 15) same fund-tilt enrichment `screen`/`check` do, so sizing ranks the names the way `screen`
    // shows them when the tilt is on. Inert when growth_fund_weight == 0 (default) -> no extra fetches.
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic.growth_fund_factor).await;
    }

    // (Item 17) apply the SAME crypto NUPL + BTC-relative adjustments `screen`/`check` do at render time,
    // so crypto sizes rank the way the picks tables showed them, not on the raw price-only score. Whole-
    // market NUPL fetched once; equities pass through crypto_adjust unchanged.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;
    let cfactor = nupl_factor(nupl, &settings.buy_heuristic);
    let btc_1y = quotes.iter().find(|q| q.ticker.starts_with("BTC-")).and_then(|q| perf_pct(q, "1Y"));

    // score with the SAME growth lane `screen` uses; None = the name failed the growth gate -> not sized.
    let mut scored: Vec<_> = quotes
        .iter()
        .filter_map(|q| {
            growth_score(q, &settings.buy_heuristic)
                .map(|s| (q, crypto_adjust(q, s, &settings.buy_heuristic, cfactor, btc_1y)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first

    if scored.is_empty() {
        println!("No names pass the growth gate — nothing to size. (try `screen` for candidates)");
        return;
    }

    // (Item 6) pass the asset class as the cluster key so a correlated block (all crypto) is one risk
    // bucket, not N independent bets.
    let weights = size_weights(
        &scored.iter().map(|(q, s)| (*s, q.volatility_pct, q.instrument_type.as_str())).collect::<Vec<_>>(),
    );

    // (Item 7) regime gross scaler: dial TOTAL exposure down when the broad market (S&P 500) is below its
    // long (~200-week) SMA. Purely multiplicative on the whole basket -> the SIZE% split below is
    // unchanged (never re-orders names); only how much capital to deploy moves. A failed fetch -> full.
    let spx = fetch::quotes(
        &client, &settings.urls, &fx_cache, &["^GSPC".to_string()], settings.dip_days, settings.high_days,
        false, false, &settings.anchor_windows, eu_infl.as_ref(),
    )
    .await;
    let risk_off = spx.first().is_some_and(|q| q.below_ma_pct > 0.0);
    let gross = if risk_off { 0.6 } else { 1.0 };

    println!("Suggested sizes — weight ∝ score ÷ volatility (vol-target, cluster-capped, READ-ONLY, NOT advice):");
    println!(
        "Suggested gross exposure: {:.0}% of capital — S&P 500 {} its ~200-week SMA ({}).\n",
        gross * 100.0,
        if risk_off { "below" } else { "at/above" },
        if risk_off { "risk-off" } else { "risk-on" },
    );
    println!("  {:<10} {:>7} {:>7} {:>7} {:>7}", "TICKER", "SCORE", "VOL", "SIZE%", "ALLOC%");
    for ((q, s), w) in scored.iter().zip(&weights) {
        println!(
            "  {:<10} {:>7.1} {:>7} {:>6.1}% {:>6.1}%",
            q.ticker,
            s,
            q.volatility_pct.map_or("n/a".to_string(), |v| format!("{v:.1}%")),
            w,        // within-basket relative split (sums to 100)
            w * gross, // ALLOC% = the slice of TOTAL capital after the regime scaler
        );
    }
}
