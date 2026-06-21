//! `check [TICKERS]` — price(EUR) + horizon % + market + trend + headline, then the
//! buy-candidate ranking and the Euribor / Certificados / inflation footer.

use crate::commands::{print_macro_footer, truncate};
use crate::core::HORIZONS;
use crate::{config, core, fetch, picks};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let hdr = HORIZONS
        .iter()
        .map(|(l, _)| format!("{:>8}", l))
        .collect::<Vec<_>>()
        .join(" ");
    let w = &settings.widths;
    let (nw, tw, mw, pw) = (w.name, w.ticker, w.market, w.price);
    println!(
        "{:<nw$} {:<tw$} {:>pw$} {hdr}  {:<mw$} {:<8} HEADLINE",
        truncate("NAME", nw), truncate("TICKER", tw), "PRICE(EUR)", truncate("MARKET", mw), "TREND"
    );

    // live EU HICP series for inflation-adjusting the long-horizon returns (only when enabled).
    // ponytail: this refetches EU HICP that the footer also pulls below; one extra call, only when on.
    let eu_infl = if settings.inflation_adjust.enabled {
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    let qs = fetch::quotes(&client, &settings.urls, &fx, &tickers, settings.dip_days, settings.high_days, false, true, &settings.anchor_windows, eu_infl.as_ref()).await; // news on: check prints headlines
    for q in &qs {
        let cells = HORIZONS
            .iter()
            .enumerate()
            .map(|(i, _)| format!("{:>8}", core::pct_cell(q.perf.get(i).and_then(|o| o.as_ref()))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:<nw$} {:<tw$} {:>pw$} {cells}  {:<mw$} {:<8} {}",
            truncate(&q.name, nw),
            truncate(&q.ticker, tw),
            q.price,
            truncate(&q.market, mw),
            q.trend,
            truncate(&q.head, w.headline),
        );
    }

    // best growth candidates (heuristic, derived from the table above — no extra fetch). Empty sector
    // filter: the watchlist is hand-picked, never sector-culled.
    picks::render(&qs, settings.top_picks, &settings.buy_heuristic, w, None, &[]);

    // Euribor / Certificados de Aforro / inflation — the macro backdrop, shared with `screen`
    print_macro_footer(&client, &settings.urls).await;
}
