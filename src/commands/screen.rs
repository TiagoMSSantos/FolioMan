//! `screen [TICKERS]` — scan a LIVE universe (top-N crypto from CoinGecko + S&P 500
//! constituents, see `fetch::fetch_universe`; `screen TICKER...` overrides) and rank the
//! 20yr+ buy-and-hold growth candidates per asset class (stocks / ETFs / crypto). The
//! growth lane is the only one with a validated forward edge (walk-forward rho +0.26,
//! top-vs-bottom-half +108 pts); the old on-sale / ATH-ATL / fallers / dividend tables
//! were dropped — their selection edge was zero-to-negative for a multi-decade hold.

use crate::core::Quote;
use crate::picks::{eu_buyable, exit_review_lines, gate_failures, growth_near_miss, growth_score, render};
use crate::{config, fetch};

/// (X) Watchlist gate-state persisted between `screen` runs so the EXIT-review footer can flag a
/// holding that PASSED every growth gate last run but fails now — the transition the backtest's
/// exit probe measures (newly-failing names lag kept-passing names by ~14 pts forward). Lives in
/// `.screen_state.json` in the working dir (same local-file pattern as `.fmp_cache`), gitignored.
#[derive(serde::Serialize, serde::Deserialize)]
struct ScreenState {
    date: String,         // YYYY-MM-DD of the run that wrote it
    passing: Vec<String>, // watchlist tickers that cleared every growth gate on that run
}

const SCREEN_STATE_FILE: &str = ".screen_state.json";

pub async fn run(args: Vec<String>) {
    // `--explain [TICKER]`: after the tables, print the SCORE arithmetic for TICKER (a flag with no
    // ticker, or no flag at all, still explains the #1 row — that footer is always on). The named ticker
    // is also added to the scan, so `screen --explain NVDA` ranks + explains just NVDA. Strip the flag
    // out of the positional tickers first, else it gets fetched as a junk symbol.
    let mut explain: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        if a == "--explain" {
            if it.peek().is_some_and(|t| !t.starts_with('-')) {
                let t = it.next().unwrap();
                positional.push(t.clone()); // ensure the target is fetched/scanned
                explain = Some(t);
            } // a bare `--explain` falls through: the default #1 footer covers it
        } else {
            positional.push(a);
        }
    }
    let args = positional;

    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    // live universe (CoinGecko + S&P 500), not a hand-kept list; explicit args override it.
    // etf_tickers = Xetra-ETF source set, used below to fix Yahoo mislabeling them as EQUITY.
    let (mut universe, etf_tickers, sector_of) = if args.is_empty() {
        fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, &settings.sectors).await
    } else {
        (args, std::collections::HashSet::new(), std::collections::HashMap::new())
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
    let mut quotes = fetch::quotes(&client, &settings.urls, &fx_cache, &universe, settings.dip_days, settings.high_days, true, false, &settings.anchor_windows, eu_infl.as_ref()).await; // intraday on (picks shows 1h/6h/12h), news off (screen never prints headlines)
    // anything from the Xetra ETF feed IS an ETF, even if Yahoo tags it EQUITY (structured products
    // like BNP Paribas Issuance) — force it so it can't leak into the stocks table past the sector filter
    for quote in &mut quotes {
        if etf_tickers.contains(&quote.ticker) {
            quote.instrument_type = "ETF".into();
        }
    }
    // (G) route the validated as-of fundamental onto the live quotes so the growth ranking weighs it —
    // only when the tilt is on (weight 0 default = no fetch, no change). Across the ~750-name universe the
    // FMP daily budget caps cold fetches; the rest serve from the disk cache, warming over runs.
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic.growth_fund_factor).await;
    }
    // keep only what an EU-retail investor can actually buy (drops any non-European-listed ETF,
    // Asian-only stock listings) so the growth ranking below is actionable.
    let before = quotes.len();
    let quotes: Vec<Quote> = quotes.into_iter().filter(eu_buyable).collect();
    eprintln!("screen: {} of {before} instruments are EU-buyable (rest filtered out)", quotes.len());

    // (D) drop STALE listings — a name whose newest close bar is older than `stale_days` CALENDAR days is a
    // halted/dead listing frozen at an old price, so its "near-high" range_pct is fake. 0 = off (keep all).
    let fresh_before = quotes.len();
    let (quotes, stale): (Vec<Quote>, Vec<Quote>) = if settings.stale_days > 0 {
        let today = chrono::Local::now().date_naive();
        quotes.into_iter().partition(|q| match q.last_close_date {
            Some(d) => (today - d).num_days() <= settings.stale_days,
            None => true, // no date (shouldn't happen live) -> keep, don't silently drop
        })
    } else {
        (quotes, Vec::new())
    };
    if !stale.is_empty() {
        let today = chrono::Local::now().date_naive();
        let names: Vec<String> = stale.iter()
            .map(|q| format!("{} ({}d)", q.ticker, q.last_close_date.map_or(-1, |d| (today - d).num_days())))
            .collect();
        eprintln!("screen: dropped {} stale listing(s) (>{}d since last close): {}", stale.len(), settings.stale_days, names.join(", "));
    }
    println!("Scanned {} instruments.", quotes.len());

    // Income-statement snapshot (REV-YoY / EPS-YoY / NET%) for the DISPLAYED stock rows only: the
    // ranked top-N plus pinned stocks — enriching all ~500 S&P names cold would burn the shared FMP
    // daily budget for columns nobody sees. Display-only fields, so pre-ranking here to learn WHICH
    // tickers will print cannot change the ranking render() computes. Cache-first: warm runs are free.
    let mut quotes = quotes;
    let targets: std::collections::HashSet<String> = {
        let is_stock = |q: &&Quote| !q.ticker.contains('-') && !crate::picks::quote_is_etf(q);
        let mut ranked: Vec<(&Quote, f64)> = quotes.iter().filter(is_stock)
            .filter_map(|q| growth_score(q, &settings.buy_heuristic).map(|s| (q, s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.iter().take(settings.top_picks).map(|(q, _)| q.ticker.clone())
            .chain(quotes.iter().filter(is_stock).filter(|q| settings.tickers.contains(&q.ticker)).map(|q| q.ticker.clone()))
            .collect()
    };
    fetch::enrich_income_stmt(&client, &settings.urls, &mut quotes, &targets).await;

    // (C) DATA-QUALITY audit: surface the n/a holes (a missing/wrong column) as one number instead of
    // finding them one row at a time. Counts by asset class so a stock with no P/E or an ETF with no TER
    // is visible at a glance.
    let stocks_no_pe = quotes.iter().filter(|q| q.instrument_type.eq_ignore_ascii_case("EQUITY") && q.pe_ratio.is_none()).count();
    let etfs_no_ter = quotes.iter().filter(|q| q.instrument_type.eq_ignore_ascii_case("ETF") && q.expense_ratio.is_none()).count();
    println!(
        "Data quality: {} names | {stocks_no_pe} stocks missing P/E | {etfs_no_ter} ETFs missing TER | {} stale dropped (>{}d)",
        quotes.len(), fresh_before - quotes.len(), settings.stale_days
    );

    // Bitcoin NUPL: whole-market crypto sentiment gauge. Fetched BEFORE render so it can damp the
    // crypto rows (high NUPL = euphoric top), then also printed as the footer line.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;

    // the 20yr+ growth ranking, split per asset class (stocks / ETFs / crypto); sectors filters ETFs
    // by fund name (stocks were already sector-filtered before fetch)
    render(&quotes, settings.top_picks, &settings.buy_heuristic, &settings.widths, nupl, &settings.sectors, &sector_of, &settings.tickers, explain.as_deref());

    // (X) EXIT review — WATCHLIST names that cleared every growth gate on the previous screen run
    // but fail one now. The backtest's exit probe measures this exact transition: newly-failing
    // names lag kept-passing names by ~14 pts forward — a mild REVIEW signal, not an auto-sell.
    // Watchlist only (the holdings — actionable); universe names churn with fetch batches and
    // would spam. First run (no state file) prints nothing and just seeds the state.
    let watch: Vec<&Quote> = quotes.iter().filter(|q| settings.tickers.contains(&q.ticker)).collect();
    let prior: Option<ScreenState> =
        std::fs::read_to_string(SCREEN_STATE_FILE).ok().and_then(|s| serde_json::from_str(&s).ok());
    if let Some(prev) = &prior {
        let lines = exit_review_lines(&prev.passing, &watch, &settings.buy_heuristic, settings.widths.ticker);
        if !lines.is_empty() {
            println!(
                "\nExit review — watchlist names that PASSED all growth gates on {} but fail now\n(measured: newly-failing names lag kept-passing by ~14 pts fwd — review, not auto-sell):",
                prev.date
            );
            for l in &lines {
                println!("{l}");
            }
        }
    }
    let state = ScreenState {
        date: chrono::Local::now().date_naive().to_string(),
        passing: watch
            .iter()
            .filter(|q| gate_failures(q, &settings.buy_heuristic).is_some_and(|f| f.is_empty()))
            .map(|q| q.ticker.clone())
            .collect(),
    };
    let _ = std::fs::write(SCREEN_STATE_FILE, serde_json::to_string(&state).unwrap_or_default());

    // (B) NEAR-MISS tail: names the growth lane rejected on EXACTLY one gate — a compounder one notch
    // outside the fence (e.g. a great name 25% off its high failing only the range gate). Makes the silent
    // exclusions visible so a dropped winner can be eyeballed, without loosening any gate. Empty -> nothing.
    let mut near: Vec<(&Quote, &'static str, String)> = quotes.iter()
        .filter_map(|q| growth_near_miss(q, &settings.buy_heuristic).map(|(g, why)| (q, g, why)))
        .collect();
    if !near.is_empty() {
        near.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.ticker.cmp(&b.0.ticker)));
        println!("\nNear-miss — rejected on ONE growth gate (not ranked above), loosen intentionally if wanted:");
        for (q, gate, why) in near.iter() {
            println!("  {:<8} {:<24.24} {:<10} {why}", q.ticker, q.name, gate);
        }
    }

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
