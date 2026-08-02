//! `check [TICKERS]` — price(EUR) + horizon % + market + trend + headline, then the
//! buy-candidate ranking and the Euribor / Certificados / inflation footer.

use crate::commands::{print_macro_footer, truncate};
use crate::core::HORIZONS;
use crate::{config, core, fetch, picks};

/// (round 58) One line per checked fund: top-5 holdings with weights, "+N" for the rest, and the
/// top-10 total as a share of the fund (omitted when Yahoo sent no weights). Input order kept —
/// it mirrors the table above. Funds with no known holdings print nothing.
fn holdings_lines(
    holdings: &std::collections::HashMap<String, Vec<(String, f64)>>,
    order: &[String],
) -> Vec<String> {
    order
        .iter()
        .filter_map(|t| {
            let hs = holdings.get(t).filter(|hs| !hs.is_empty())?;
            let cell = |(s, p): &(String, f64)| {
                if *p > 0.0 { format!("{s} {:.1}%", p * 100.0) } else { s.clone() }
            };
            let head: Vec<String> = hs.iter().take(5).map(cell).collect();
            let more = if hs.len() > 5 { format!(" +{}", hs.len() - 5) } else { String::new() };
            let sum: f64 = hs.iter().map(|(_, p)| p).sum();
            let total = if sum > 0.0 { format!(" (top-10 = {:.0}% of fund)", sum * 100.0) } else { String::new() };
            Some(format!("  {t}: {}{more}{total}", head.join(", ")))
        })
        .collect()
}

pub async fn run(args: Vec<String>) {
    // `--explain [TICKER]`: print the SCORE arithmetic for TICKER without narrowing the view —
    // the whole watchlist still renders (unlike `screen`, where the target narrows the scan).
    // Kills the doubt-a-mid-list-row workflow's second fetch round-trip: one run shows the full
    // table AND the named decomposition. The target is fetched even if it's not on the watchlist.
    let (explain, positional) = crate::commands::parse_explain("check", args);
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let mut tickers = if positional.is_empty() { settings.tickers.clone() } else { positional };
    if let Some(t) = &explain {
        if !tickers.contains(t) {
            tickers.push(t.clone());
        }
    }

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
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic).await;
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

    // (round 58) what's inside the wrapper: top-10 holdings with weights for the checked ETFs —
    // `check` is the single-fund buy-decision view and showed nothing below the fund name. Same
    // weekly-cached Yahoo topHoldings the screen's overlap footer uses (direct by symbol, no BF
    // dependency, so it works on this path). Display-only; funds Yahoo has no holdings for are
    // simply absent.
    let etfs: Vec<String> =
        quotes.iter().filter(|q| picks::quote_is_etf(q)).map(|q| q.ticker.clone()).collect();
    if !etfs.is_empty() {
        let (holdings, _mix) = fetch::yahoo_top_holdings(&client, &etfs).await;
        let lines = holdings_lines(&holdings, &etfs);
        if !lines.is_empty() {
            println!("\nTop-10 holdings — what's inside the wrapper:");
            for l in &lines {
                println!("{l}");
            }
        }
    }

    // best growth candidates (heuristic, derived from the table above — no extra fetch). Empty sector
    // filter: the watchlist is hand-picked, never sector-culled.
    // (round 68) the ranked-ticker list feeds the SCREEN's run-to-run membership diff; check has
    // no state file, so it drops it.
    // show_hold_core = false: `check` inspects hand-picked watchlist names (H flagged inline per row);
    // the consolidated core shortlist is a wide-hunt affordance, not wanted here.
    let (explain_text, _) = picks::render(&quotes, settings.top_picks, &settings.buy_heuristic, widths, picks::RenderCtx {
        nupl: None,
        sectors: &[],
        sector_of: &std::collections::HashMap::new(),
        pinned: &[],
        owned: &Default::default(),
        explain: explain.as_deref(),
        show_hold_core: false,
        // `check` inspects named watchlist tickers and fetches no fund holdings, so there is no
        // look-through P/E here and the ETF PEG trim is a no-op. Deliberate: check reports on the
        // names you asked about, it does not cut them from a table.
        // (#45) same for crypto: `check` never calls `fetch_mvrv`, so `quote.mvrv` stays None and
        // `crypto_max_mvrv` passes every coin free. A coin `screen` would reject as expensive still
        // reports here — which is the point, since you asked about that coin by name.
        fund_pe: &std::collections::HashMap::new(),
    });

    // held-name gate review: a watchlist name that would no longer clear today's growth gates is an
    // exit-review candidate — the screen would never surface it again, so `check` has to say so.
    // Not-assessable names (leveraged / stablecoin / missing data) are skipped, not flagged.
    let all: Vec<&core::Quote> = quotes.iter().collect();
    let flagged = picks::gate_review_lines(&all, &settings.buy_heuristic, ticker_w);
    if !flagged.is_empty() {
        println!("\ngate review — these would NOT rank in today's screen (review, not auto-sell):");
        for line in &flagged {
            println!("{line}");
        }
    }

    // (round 52) score-math walkthrough after the actionable gate review, same order as `screen`.
    if let Some(text) = explain_text {
        println!("{}", text.trim_end());
    }

    // Euribor / Certificados de Aforro / inflation — the macro backdrop, shared with `screen`
    print_macro_footer(&client, &settings.urls).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// (round 58) holdings line semantics: top-5 with weights then "+N", top-10 total suffix,
    /// weightless holdings print name-only and no total, unknown/empty funds print nothing,
    /// input order kept.
    #[test]
    fn holdings_lines_semantics() {
        let mut h: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        h.insert(
            "SEMI.DE".into(),
            (0..10).map(|i| (format!("H{i}"), 0.10 - i as f64 * 0.01)).collect(), // 10%..1% = 55%
        );
        h.insert("NOW.DE".into(), vec![("AAA".into(), 0.0), ("BBB".into(), 0.0)]);
        h.insert("EMPTY.DE".into(), vec![]);
        let order = vec!["SEMI.DE".to_string(), "NOW.DE".to_string(), "EMPTY.DE".to_string(), "GONE.DE".to_string()];
        assert_eq!(holdings_lines(&h, &order), vec![
            "  SEMI.DE: H0 10.0%, H1 9.0%, H2 8.0%, H3 7.0%, H4 6.0% +5 (top-10 = 55% of fund)".to_string(),
            "  NOW.DE: AAA, BBB".to_string(),
        ]);
    }
}
