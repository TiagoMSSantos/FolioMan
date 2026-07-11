//! `report [TICKERS]` — inspect a company's ANNUAL income-statement trajectory (revenue, margins,
//! EPS, year-over-year growth) plus the SAME fundamental "grower verdict" the buy heuristic ranks on.
//! Workflow: run `screen` to surface the best growers, then `report AAPL` to drill into one before
//! betting. Sources the FMP `stable/income-statement` pipeline first, falling back to SEC EDGAR XBRL
//! (free, no key, no daily cap; US filers) when FMP is throttled/keyless. The verdict comes straight
//! from `core::fund_factors`, so the inspection can't drift from what `screen`/`check` actually weigh.

use crate::{config, core, fetch, picks};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };
    let today = chrono::Local::now().date_naive();
    // tailor the empty-data line: a missing key is a different fix than FMP's daily 250-call cap (429)
    // or an uncovered ticker. Without this, the message tells you to set a key you already have.
    // FMP feeds the global path; SEC EDGAR (no key) covers US filers as fallback. So an empty line means
    // a foreign/ETF name with no US XBRL (and FMP throttled/keyless), not necessarily a missing key.
    let no_data = if std::env::var("FMP_API_KEY").is_ok_and(|k| !k.is_empty()) {
        "no statements (US filers fall back to SEC EDGAR; this is likely a non-US/ETF name, a dead/delisted ticker, or FMP is throttled)"
    } else {
        "no statements (set FMP_API_KEY for global coverage; US filers still resolve via SEC EDGAR — or the ticker is dead/delisted)"
    };
    // exit-code contract: asked about ≥1 equity and produced ZERO tables -> exit 1 so a script/cron
    // can tell total failure from success. Partial success (some tickers resolve) stays 0 — some
    // data beats none. Crypto/FX-only invocations stay 0 (there is nothing to fetch by design).
    let mut equity_requested = 0u32;
    let mut tables_printed = 0u32;

    for ticker in &tickers {
        // crypto/FX carry no income statement -> don't waste an FMP budget slot probing.
        // Suffix check, NOT contains('-'): share-class tickers are dash-normalized (BRK.B -> BRK-B)
        // and must fall through to the fetch.
        if picks::is_currency_quoted(ticker) {
            println!("\n{ticker}: no income statement (crypto/FX)");
            continue;
        }
        equity_requested += 1;
        let (rows, source) = match fetch::fetch_fundamentals_report(&client, &settings.urls, ticker).await {
            Some(r) => r,
            None => {
                println!("\n{ticker}: {no_data}");
                continue;
            }
        };
        let annual = core::annual_rollup(&rows);

        println!("\n{ticker} — annual income statements (fiscal-year rollup, newest first · source: {source})");
        // an empty rollup under a bare header reads like a rendering bug — say why it's empty
        // (quarters exist but no fiscal year completed yet), and don't count it as a table.
        if annual.is_empty() {
            println!("  (statements fetched, but no complete fiscal year to roll up)");
        } else {
            tables_printed += 1;
        }
        println!("{:>6} {:>10} {:>8} {:>7} {:>7} {:>7} {:>9} {:>8}", "YEAR", "REVENUE", "REV-YoY", "GROSS%", "OP%", "NET%", "EPS", "EPS-YoY");
        for (i, a) in annual.iter().enumerate() {
            let older = annual.get(i + 1); // next row is the previous (older) fiscal year
            // zero-revenue older row (missing/partial data) would print "+inf%" — same guard the
            // EPS column below already has ("-" instead).
            let rev_yoy = older.filter(|o| o.revenue != 0.0).map(|o| (a.revenue / o.revenue - 1.0) * 100.0);
            let eps_yoy = match (a.eps, older.and_then(|o| o.eps)) {
                (Some(c), Some(p)) if p != 0.0 => Some((c / p - 1.0) * 100.0),
                _ => None,
            };
            // 2-3 quarters = a genuinely partial quarterly (FMP) year; mark it. 1 = an annual filing
            // (SEC EDGAR rolls a fiscal year into one row) OR the rare newest-FMP-year-with-only-Q1, which
            // we don't flag. 4+ = a full quarterly year. ponytail: can't tell source per-row, this is close.
            let mark = if (2..4).contains(&a.quarters) { "*" } else { "" };
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
    if equity_requested > 0 && tables_printed == 0 {
        std::process::exit(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// (round 61) formatter semantics: unit tier picked by magnitude (sign preserved, tier chosen
    /// on |v|), signed vs unsigned styles, "-" for absent values across all three Option formatters.
    #[test]
    fn formatter_semantics() {
        assert_eq!(humanize(2.34e12), "2.34T");
        assert_eq!(humanize(391.04e9), "391.0B");
        assert_eq!(humanize(-1.5e9), "-1.5B"); // negative revenue: tier on |v|, sign kept
        assert_eq!(humanize(25.6e6), "25.6M");
        assert_eq!(humanize(1_500.0), "1.5K");
        assert_eq!(humanize(999.4), "999"); // below 1K: plain, no decimals
        assert_eq!(yoy(Some(8.06)), "+8.1%");
        assert_eq!(yoy(Some(-12.3)), "-12.3%");
        assert_eq!(yoy(None), "-");
        assert_eq!(pts(Some(3.25)), "+3.2"); // points, signed, no % (a difference, not a rate)
        assert_eq!(pts(None), "-");
        assert_eq!(level(Some(48.04)), "48.0"); // margins are levels: unsigned
        assert_eq!(level(None), "-");
    }
}
