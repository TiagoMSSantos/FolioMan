//! `size [TICKERS]` — suggested position sizes for the growth picks: weight ∝ score ÷ volatility
//! (vol-target). READ-ONLY, never trades — you still type the qty into `trade` yourself. No TICKERS
//! -> the watchlist. Names that fail the growth gate are dropped (nothing to size). NOT advice.

use crate::picks::{growth_score, size_weights};
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
    let quotes = fetch::quotes(
        &client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, false,
        &settings.anchor_windows, eu_infl.as_ref(),
    )
    .await;

    // score with the SAME growth lane `screen` uses; None = the name failed the growth gate -> not sized.
    let mut scored: Vec<_> =
        quotes.iter().filter_map(|q| growth_score(q, &settings.buy_heuristic).map(|s| (q, s))).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first

    if scored.is_empty() {
        println!("No names pass the growth gate — nothing to size. (try `screen` for candidates)");
        return;
    }

    let weights = size_weights(&scored.iter().map(|(q, s)| (*s, q.volatility_pct)).collect::<Vec<_>>());
    println!("Suggested sizes — weight ∝ score ÷ volatility (vol-target, READ-ONLY, NOT advice):\n");
    println!("  {:<10} {:>7} {:>7} {:>7}", "TICKER", "SCORE", "VOL", "SIZE%");
    for ((q, s), w) in scored.iter().zip(&weights) {
        println!(
            "  {:<10} {:>7.1} {:>7} {:>6.1}%",
            q.ticker,
            s,
            q.volatility_pct.map_or("n/a".to_string(), |v| format!("{v:.1}%")),
            w,
        );
    }
}
