//! `screen [TICKERS]` — scan a LIVE universe (top-N crypto from CoinGecko + S&P 500
//! constituents, see `fetch::fetch_universe`; `screen TICKER...` overrides) and rank the
//! 20yr+ buy-and-hold growth candidates per asset class (stocks / ETFs / crypto). The
//! growth lane is the only one with a validated forward edge (walk-forward rho +0.26,
//! top-vs-bottom-half +108 pts); the old on-sale / ATH-ATL / fallers / dividend tables
//! were dropped — their selection edge was zero-to-negative for a multi-decade hold.

use crate::core::Quote;
use crate::picks::{eu_buyable, render};
use crate::{config, fetch};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx = fetch::fx_cache();
    // live universe (CoinGecko + S&P 500), not a hand-kept list; explicit args override it.
    // etf_tickers = Xetra-ETF source set, used below to fix Yahoo mislabeling them as EQUITY.
    let (mut universe, etf_tickers) = if args.is_empty() {
        fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, &settings.sectors).await
    } else {
        (args, std::collections::HashSet::new())
    };
    // watchlist tickers are ALWAYS fetched so they show in their table for comparison (sector filter or not)
    universe.extend(settings.tickers.iter().cloned());
    universe.sort();
    universe.dedup();

    eprintln!("screen: {} tickers in universe (crypto + S&P 500 + Xetra UCITS ETFs)", universe.len());

    // live EU HICP series to inflation-adjust long-horizon returns, only when enabled
    let eu_infl = if settings.inflation_adjust.enabled {
        eprintln!("screen: fetching EU HICP inflation series…");
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    let mut qs = fetch::quotes(&client, &settings.urls, &fx, &universe, settings.dip_days, settings.high_days, true, false, &settings.anchor_windows, eu_infl.as_ref()).await; // intraday on (picks shows 1h/6h/12h), news off (screen never prints headlines)
    // anything from the Xetra ETF feed IS an ETF, even if Yahoo tags it EQUITY (structured products
    // like BNP Paribas Issuance) — force it so it can't leak into the stocks table past the sector filter
    for q in &mut qs {
        if etf_tickers.contains(&q.ticker) {
            q.instrument_type = "ETF".into();
        }
    }
    // keep only what an EU-retail investor can actually buy (drops any non-European-listed ETF,
    // Asian-only stock listings) so the growth ranking below is actionable.
    let before = qs.len();
    let qs: Vec<Quote> = qs.into_iter().filter(eu_buyable).collect();
    eprintln!("screen: {} of {before} instruments are EU-buyable (rest filtered out)", qs.len());
    println!("Scanned {} instruments.", qs.len());

    // Bitcoin NUPL: whole-market crypto sentiment gauge. Fetched BEFORE render so it can damp the
    // crypto rows (high NUPL = euphoric top), then also printed as the footer line.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;

    // the 20yr+ growth ranking, split per asset class (stocks / ETFs / crypto); sectors filters ETFs
    // by fund name (stocks were already sector-filtered before fetch)
    render(&qs, settings.top_picks, &settings.buy_heuristic, &settings.widths, nupl, &settings.sectors, &settings.tickers);

    if let Some(n) = nupl {
        println!(
            "\nBitcoin NUPL: {n:.3} ({}) — net unrealized profit/loss, whole-market sentiment (damps the crypto tables above). NOT advice.",
            crate::core::nupl_zone(n)
        );
    }

    // Euribor / Certificados de Aforro / inflation — fixed-income + macro baselines to compare the
    // asset tables against
    crate::commands::print_macro_footer(&client, &settings.urls).await;
}
