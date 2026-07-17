//! `perf [TICKERS]` — per-ticker block: past EUR price + % at each horizon + source URL.
//! With `inflation_adjust` on, >=1Y % are real (HICP-deflated) while past prices stay nominal;
//! a header line says so whenever the adjustment is actually in effect.

use crate::core::HORIZONS;
use crate::{config, core, fetch};

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
    // Label the nominal/real mix: the past-price column is NOMINAL EUR while >=1Y % are
    // HICP-deflated (core::horizon_changes) — unlabeled, the visible arithmetic looks wrong
    // ((now-past)/past no longer matches the printed %). Gated on a non-empty series: a failed
    // HICP fetch leaves every % nominal (empty map -> no deflation), and the label must not lie.
    if eu_infl.as_ref().is_some_and(|s| !s.is_empty()) {
        println!("(1Y+ % inflation-adjusted — real EUR terms, EU HICP; past prices stay nominal)");
    }
    for quote in fetch::quotes(&client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, false, &settings.anchor_windows, eu_infl.as_ref()).await { // news off: perf prints only % columns
        println!(
            "\n{} [{}]  now {}  ({})  {}",
            quote.name, quote.ticker, quote.price, quote.market, core::source_url(&settings.urls.yahoo_quote, &quote.ticker)
        );
        for (i, (lbl, _)) in HORIZONS.iter().enumerate() {
            match quote.perf.get(i).and_then(|o| o.as_ref()) {
                Some((past, p)) => println!("  {:<4} {:>14}  {:+.1}%", lbl, past, p),
                None => println!("  {:<4} {:>14}", lbl, "n/a"),
            }
        }
    }
}
