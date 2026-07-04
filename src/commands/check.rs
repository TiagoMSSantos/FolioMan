//! `check [TICKERS]` — price(EUR) + horizon % + market + trend + headline, then the
//! buy-candidate ranking and the Euribor / Certificados / inflation footer.

use crate::commands::{print_macro_footer, truncate};
use crate::core::HORIZONS;
use crate::{config, core, fetch, picks};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let hdr = HORIZONS
        .iter()
        .map(|(l, _)| format!("{:>8}", l))
        .collect::<Vec<_>>()
        .join(" ");
    let widths = &settings.widths;
    let (name_w, ticker_w, market_w, price_w) = (widths.name, widths.ticker, widths.market, widths.price);
    println!(
        "{:<name_w$} {:<ticker_w$} {:>price_w$} {hdr}  {:<market_w$} {:<8} HEADLINE",
        truncate("NAME", name_w), truncate("TICKER", ticker_w), "PRICE(EUR)", truncate("MARKET", market_w), "TREND"
    );

    // live EU HICP series for inflation-adjusting the long-horizon returns (only when enabled).
    // note: this refetches EU HICP that the footer also pulls below; one extra call, only when on.
    let eu_infl = if settings.inflation_adjust.enabled {
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    let mut quotes = fetch::quotes(&client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, true, &settings.anchor_windows, eu_infl.as_ref()).await; // news on: check prints headlines
    // (G) route the validated as-of fundamental onto the live quotes so the buy ranking weighs it — only
    // when the tilt is on (weight 0 default = no fetch, no change). `check` scale keeps the FMP budget easy.
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic.growth_fund_factor).await;
    }
    for quote in &quotes {
        let cells = HORIZONS
            .iter()
            .enumerate()
            .map(|(i, _)| format!("{:>8}", core::pct_cell(quote.perf.get(i).and_then(|o| o.as_ref()))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:<name_w$} {:<ticker_w$} {:>price_w$} {cells}  {:<market_w$} {:<8} {}",
            truncate(&picks::clean_name(quote), name_w),
            truncate(&quote.ticker, ticker_w),
            quote.price,
            truncate(&quote.market, market_w),
            quote.trend,
            truncate(&quote.head, widths.headline),
        );
    }

    // best growth candidates (heuristic, derived from the table above — no extra fetch). Empty sector
    // filter: the watchlist is hand-picked, never sector-culled.
    picks::render(&quotes, settings.top_picks, &settings.buy_heuristic, widths, None, &[], &std::collections::HashMap::new(), &[], None);

    // held-name gate review: a watchlist name that would no longer clear today's growth gates is an
    // exit-review candidate — the screen would never surface it again, so `check` has to say so.
    // Not-assessable names (leveraged / stablecoin / missing data) are skipped, not flagged.
    let flagged: Vec<String> = quotes
        .iter()
        .filter_map(|q| {
            let fails = picks::gate_failures(q, &settings.buy_heuristic)?;
            if fails.is_empty() {
                return None;
            }
            let why = fails.iter().map(|(gate, why, _)| format!("{gate}: {why}")).collect::<Vec<_>>().join("; ");
            Some(format!("  {:<ticker_w$} {}", q.ticker, why))
        })
        .collect();
    if !flagged.is_empty() {
        println!("\ngate review — these would NOT rank in today's screen (review, not auto-sell):");
        for line in &flagged {
            println!("{line}");
        }
    }

    // Euribor / Certificados de Aforro / inflation — the macro backdrop, shared with `screen`
    print_macro_footer(&client, &settings.urls).await;
}
