//! `perf [TICKERS]` — per-ticker block: past EUR price + % at each horizon + source URL.

use crate::core::HORIZONS;
use crate::{config, core, fetch};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let eu_infl = if settings.inflation_adjust.enabled {
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    for q in fetch::quotes(&client, &settings.urls, &fx, &tickers, settings.dip_days, settings.high_days, false, false, &settings.anchor_windows, eu_infl.as_ref()).await { // news off: perf prints only % columns
        println!(
            "\n{} [{}]  now {}  ({})  {}",
            q.name, q.ticker, q.price, q.market, core::source_url(&settings.urls.yahoo_quote, &q.ticker)
        );
        for (i, (lbl, _)) in HORIZONS.iter().enumerate() {
            match q.perf.get(i).and_then(|o| o.as_ref()) {
                Some((past, p)) => println!("  {:<4} {:>14}  {:+.1}%", lbl, past, p),
                None => println!("  {:<4} {:>14}", lbl, "n/a"),
            }
        }
    }
}
