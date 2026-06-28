//! `report [TICKERS]` — inspect a company's ANNUAL income-statement trajectory (revenue, margins,
//! EPS, year-over-year growth) plus the SAME fundamental "grower verdict" the buy heuristic ranks on.
//! Workflow: run `screen` to surface the best growers, then `report AAPL` to drill into one before
//! betting. Reuses the disk-cached FMP `stable/income-statement` pipeline — no extra fetch type, and
//! the verdict comes straight from `core::fund_factors`, so the inspection can't drift from what
//! `screen`/`check` actually weigh. Needs FMP_API_KEY (free tier); without it, a graceful line, no crash.

use crate::{config, core, fetch};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };
    let today = chrono::Local::now().date_naive();

    for ticker in &tickers {
        // crypto/FX carry no income statement -> don't waste an FMP budget slot probing
        if ticker.contains('-') {
            println!("\n{ticker}: no income statement (crypto/FX)");
            continue;
        }
        let rows = match fetch::fetch_fundamentals_history(&client, &settings.urls, ticker).await {
            Some(r) => r,
            None => {
                println!("\n{ticker}: no fundamentals (set FMP_API_KEY, over daily budget, or not covered)");
                continue;
            }
        };
        let annual = core::annual_rollup(&rows);

        println!("\n{ticker} — annual income statements (fiscal-year rollup, newest first)");
        println!("{:>6} {:>10} {:>8} {:>7} {:>7} {:>7} {:>9} {:>8}", "YEAR", "REVENUE", "REV-YoY", "GROSS%", "OP%", "NET%", "EPS", "EPS-YoY");
        for (i, a) in annual.iter().enumerate() {
            let older = annual.get(i + 1); // next row is the previous (older) fiscal year
            let rev_yoy = older.map(|o| (a.revenue / o.revenue - 1.0) * 100.0);
            let eps_yoy = match (a.eps, older.and_then(|o| o.eps)) {
                (Some(c), Some(p)) if p != 0.0 => Some((c / p - 1.0) * 100.0),
                _ => None,
            };
            let mark = if a.quarters < 4 { "*" } else { "" }; // incomplete fiscal year
            println!(
                "{:>5}{} {:>10} {:>8} {:>7} {:>7} {:>7} {:>9} {:>8}",
                a.year, mark, humanize(a.revenue), yoy(rev_yoy),
                level(a.gross_margin), level(a.op_margin), level(a.net_margin),
                a.eps.map(|e| format!("{e:.2}")).unwrap_or_else(|| "-".into()), yoy(eps_yoy),
            );
        }
        if annual.iter().any(|a| a.quarters < 4) {
            println!("  (* = incomplete fiscal year: fewer than 4 quarters reported)");
        }

        // grower verdict — the EXACT as-of factors growth_score weighs (5y lookback matches the live enrich)
        let ff = core::fund_factors(&rows, today, 5);
        println!("--- grower verdict (what screen bets on) ---");
        println!(
            "  rev_cagr {}  eps_growth {}  rev_accel {}  margin_trend {}  gross_margin {}  op_margin {}",
            yoy(ff.rev_cagr), yoy(ff.eps_growth), pts(ff.rev_accel), pts(ff.margin_trend),
            level(ff.gross_margin), level(ff.op_margin),
        );
    }
}

// ponytail: tiny local number formatters — no shared humanize helper exists and these are display-only.
fn humanize(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// signed growth/return, e.g. "+8.1%" — for CAGR/YoY style numbers; "-" when absent
fn yoy(v: Option<f64>) -> String {
    v.map(|x| format!("{x:+.1}%")).unwrap_or_else(|| "-".into())
}

/// signed points (accel/trend are differences, not %) — "+3.2" / "-"
fn pts(v: Option<f64>) -> String {
    v.map(|x| format!("{x:+.1}")).unwrap_or_else(|| "-".into())
}

/// an unsigned level (a margin %), e.g. "48.0" — "-" when absent
fn level(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into())
}
