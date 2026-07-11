//! `alert [TICKERS]` — ntfy.sh push for tickers >= drop_pct below their trailing high.

use crate::{config, fetch};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    for quote in fetch::quotes(&client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, true, &settings.anchor_windows, None).await { // news on: alert body shows headlines; keys on price drop, not returns
        if quote.drop_pct >= settings.drop_pct {
            let delivered = fetch::push(
                &client,
                &settings.urls,
                &settings.ntfy_topic,
                &format!("{} {} (buy-dip?)", quote.ticker, quote.dip),
                &format!(
                    "{} is {:.1}% below its {}d high.\n{}",
                    quote.ticker, quote.drop_pct, settings.dip_days, quote.news_block
                ),
            )
            .await;
            // cron pipes stderr to the log (see README) — a dropped push must leave a trace there.
            // Keep going: one failed push must not cost the remaining tickers their alerts.
            if !delivered {
                eprintln!("WARNING: ntfy push failed for {} — dip alert NOT delivered", quote.ticker);
            }
        }
    }
}
