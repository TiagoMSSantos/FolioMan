//! `alert [TICKERS]` — ntfy.sh push for tickers >= drop_pct below their trailing high.

use crate::{config, fetch};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    for q in fetch::quotes(&client, &settings.urls, &fx, &tickers, settings.dip_days, settings.high_days, false).await {
        if q.drop_pct >= settings.drop_pct {
            fetch::push(
                &client,
                &settings.urls,
                &settings.ntfy_topic,
                &format!("{} {} (buy-dip?)", q.ticker, q.dip),
                &format!(
                    "{} is {:.1}% below its {}d high.\n{}",
                    q.ticker, q.drop_pct, settings.dip_days, q.news_block
                ),
            )
            .await;
        }
    }
}
