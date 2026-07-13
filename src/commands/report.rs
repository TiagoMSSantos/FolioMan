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

    // (round 115) one quote fetch for the valuation line: close_native pairs with the native EPS so
    // the printed earnings_yield is the SAME number the live enrich feeds growth_score (fetch.rs) —
    // no new math, no currency skew. A failed/short fetch just leaves the close absent ("-").
    let equities: Vec<String> = tickers.iter().filter(|t| !picks::is_currency_quoted(t)).cloned().collect();
    let closes: std::collections::HashMap<String, f64> = if equities.is_empty() {
        Default::default()
    } else {
        let fx_cache = fetch::fx_cache();
        fetch::quotes(&client, &settings.urls, &fx_cache, &equities, settings.dip_days, settings.high_days, false, false, &settings.anchor_windows, None)
            .await
            .into_iter()
            .filter_map(|q| q.close_native.map(|c| (q.ticker.clone(), c)))
            .collect()
    };

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
        let close = closes.get(ticker.as_str()).copied();
        let (block, has_table) = render_annual(ticker, source, &rows, today, close, &settings.buy_heuristic);
        print!("{block}");
        if has_table {
            tables_printed += 1;
        }
    }
    if equity_requested > 0 && tables_printed == 0 {
        std::process::exit(1);
    }
}

/// Render one ticker's annual table + grower verdict; pure so the real branches — the +inf% YoY
/// guard, the incomplete-year `*` mark, the empty-rollup note — are unit-testable offline (same
/// seam split as the trading212 render_summary). Returns (text, whether a fiscal-year table
/// rendered) for run()'s exit-code rule.
fn render_annual(
    ticker: &str, source: &str, rows: &[core::FundRow], today: chrono::NaiveDate,
    close_native: Option<f64>, tuning: &config::BuyHeuristic,
) -> (String, bool) {
    let annual = core::annual_rollup(rows);
    let mut out = format!("\n{ticker} — annual income statements (fiscal-year rollup, newest first · source: {source})\n");
    // an empty rollup under a bare header reads like a rendering bug — say why it's empty
    // (statements exist but no fiscal year completed yet), and don't count it as a table.
    if annual.is_empty() {
        out.push_str("  (statements fetched, but no complete fiscal year to roll up)\n");
    }
    out.push_str(&format!(
        "{:>6} {:>10} {:>8} {:>7} {:>7} {:>7} {:>9} {:>8} {:>8}\n",
        "YEAR", "REVENUE", "REV-YoY", "GROSS%", "OP%", "NET%", "EPS", "EPS-YoY", "SHΔ%",
    ));
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
        out.push_str(&format!(
            "{:>5}{} {:>10} {:>8} {:>7} {:>7} {:>7} {:>9} {:>8} {:>8}\n",
            a.year, mark, humanize(a.revenue), yoy(rev_yoy),
            level(a.gross_margin), level(a.op_margin), level(a.net_margin),
            a.eps.map(|e| format!("{e:.2}")).unwrap_or_else(|| "-".into()), yoy(eps_yoy),
            yoy(sh_delta(a.shares, older.and_then(|o| o.shares))),
        ));
    }
    if annual.iter().any(|a| a.quarters < 4) {
        out.push_str("  (* = incomplete fiscal year: fewer than 4 quarters reported)\n");
    }
    if annual.iter().any(|a| a.shares.is_some()) {
        out.push_str("  (SHΔ% = YoY share-count change, sign-flipped: + = buying back, - = diluting; a swing >40% (split/M&A) prints \"-\")\n");
    }

    // grower verdict — the EXACT as-of factors growth_score weighs (5y lookback matches the live enrich)
    let ff = core::fund_factors(rows, today, 5);
    out.push_str("--- grower verdict (what screen bets on) ---\n");
    out.push_str(&format!(
        "  rev_cagr {}  eps_growth {}  rev_accel {}  margin_trend {}  gross_margin {}  op_margin {}  roe {}  buyback_yield {}\n",
        yoy(ff.rev_cagr), yoy(ff.eps_growth), pts(ff.rev_accel), pts(ff.margin_trend),
        level(ff.gross_margin), level(ff.op_margin), level(ff.roe), yoy(ff.buyback_yield),
    ));
    out.push_str(&format!(
        "  survival (judgment — measured no-edge as gates): fcf_margin {}  interest_cover {}  net_cash_rev {}  margin_stability {}\n",
        level(ff.fcf_margin), cover(ff.interest_cover), level(ff.net_cash_rev), pts(ff.margin_stability),
    ));

    // valuation — the SAME native-close × as-of EPS ratio the live enrich feeds growth_score
    // (fetch.rs) when growth_fund_factor is "earnings_yield"; printed regardless of which factor is
    // selected (info line), the "-> pts" clause only when it's the one actually weighed.
    let ey = close_native.and_then(|p| core::earnings_yield(ff.eps_ttm, p));
    out.push_str(&format!(
        "  valuation: eps_ttm {}  close {} (native ccy)  earnings_yield {}",
        ff.eps_ttm.map(|e| format!("{e:.2}")).unwrap_or_else(|| "-".into()),
        close_native.map(|c| format!("{c:.2}")).unwrap_or_else(|| "-".into()),
        yoy(ey),
    ));
    if tuning.growth_fund_factor == "earnings_yield" && tuning.growth_fund_weight > 0.0 {
        let contrib = ey.map(|e| tuning.growth_fund_weight * e.clamp(0.0, tuning.growth_fund_cap));
        out.push_str(&format!(
            " -> {} pts in growth_score (weight {:.1}, cap {:.0})",
            pts(contrib), tuning.growth_fund_weight, tuning.growth_fund_cap,
        ));
    }
    out.push('\n');

    let has_table = !annual.is_empty();
    (out, has_table)
}

/// YoY share-count change, sign-flipped (+ = shrinking count = buying back). Same `|Δ|>40%`
/// split/M&A guard as `core::fund_factors`' `buyback_yield` — a GOOG 20:1 split must print "-",
/// never a fabricated -95%.
fn sh_delta(current: Option<f64>, older: Option<f64>) -> Option<f64> {
    match (current, older) {
        (Some(a), Some(b)) if b > 0.0 => {
            let d = (a / b - 1.0) * 100.0;
            (d.abs() <= 40.0).then_some(-d)
        }
        _ => None,
    }
}

use crate::core::humanize; // shared with screen's fundamentals footer since it grew a second user

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

/// a coverage multiple (interest_cover), e.g. "4.2x" — "-" when absent
fn cover(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}x")).unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn quarter(y: i32, m: u32, revenue: Option<f64>, eps: Option<f64>) -> core::FundRow {
        let d = NaiveDate::from_ymd_opt(y, m, 15).unwrap();
        core::FundRow { filed: d, period_end: d, revenue, eps, ..Default::default() }
    }

    fn quarter_sh(y: i32, m: u32, revenue: Option<f64>, shares: Option<f64>) -> core::FundRow {
        let d = NaiveDate::from_ymd_opt(y, m, 15).unwrap();
        core::FundRow { filed: d, period_end: d, revenue, shares, ..Default::default() }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    }

    fn render(ticker: &str, source: &str, rows: &[core::FundRow]) -> (String, bool) {
        render_annual(ticker, source, rows, today(), None, &config::BuyHeuristic::default())
    }

    /// Happy path: title carries ticker + source, a full 4-quarter year renders with its YoY vs the
    /// older year (4×120 = 480 vs 4×100 = 400 -> +20.0%), and has_table is true.
    #[test]
    fn renders_table_with_source_and_yoy() {
        let mut rows: Vec<core::FundRow> = (1..=4).map(|q| quarter(2024, q * 3, Some(100.0), Some(1.0))).collect();
        rows.extend((1..=4).map(|q| quarter(2025, q * 3, Some(120.0), Some(1.5))));
        let (out, has_table) = render("ACME", "FMP", &rows);
        assert!(out.contains("ACME — annual income statements"), "{out}");
        assert!(out.contains("source: FMP"), "{out}");
        assert!(out.contains("+20.0%"), "rev YoY missing: {out}");
        assert!(has_table);
    }

    /// (round 69 guard) a zero-revenue older year must render REV-YoY as "-", never "+inf%".
    #[test]
    fn zero_revenue_older_year_is_dash_not_inf() {
        let rows = vec![quarter(2024, 6, Some(0.0), None), quarter(2025, 6, Some(50.0), None)];
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(!out.contains("inf"), "{out}");
    }

    /// 2-3 quarters = a genuinely partial fiscal year: the row carries `*` and the footnote prints.
    #[test]
    fn partial_year_carries_incomplete_mark() {
        let mut rows: Vec<core::FundRow> = (1..=4).map(|q| quarter(2024, q * 3, Some(100.0), None)).collect();
        rows.push(quarter(2025, 3, Some(110.0), None));
        rows.push(quarter(2025, 6, Some(110.0), None));
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(out.contains(" 2025*"), "mark missing: {out}");
        assert!(out.contains("incomplete fiscal year"), "footnote missing: {out}");
        assert!(!out.contains(" 2024*"), "full year must not be marked: {out}");
    }

    /// (round 89 note) no rows to roll up -> the explanatory note prints under the header and the
    /// block does NOT count as a table for the exit-code rule.
    #[test]
    fn empty_rollup_prints_note_not_bare_header() {
        let (out, has_table) = render("ACME", "SEC EDGAR", &[]);
        assert!(out.contains("no complete fiscal year to roll up"), "{out}");
        assert!(!has_table);
    }

    /// (round 115) survival line prints the R107 as-of levels with the "-" guard for absent fields
    /// (FMP free tier leaves fcf_margin/interest_cover/net_cash_rev/margin_stability unpopulated —
    /// FundRow::default() here — so every field must render "-", never a fabricated 0).
    #[test]
    fn survival_line_renders_dash_when_absent() {
        let rows = vec![quarter(2024, 6, Some(100.0), Some(1.0))];
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(out.contains("survival (judgment — measured no-edge as gates): fcf_margin -  interest_cover -  net_cash_rev -  margin_stability -"), "{out}");
    }

    /// (round 115) valuation/tilt receipt: eps_ttm 2.0 over close 100 -> earnings_yield +2.0%; with
    /// the shipped tilt config (factor "earnings_yield", weight 1.0, cap 30) the pts clause mirrors
    /// the score's own clamp (picks.rs: weight * ey.clamp(0.0, cap)) -> "+2.0 pts". Weight 0 (the
    /// struct default) must print the ratio WITHOUT a pts clause — it isn't actually weighed.
    #[test]
    fn tilt_receipt_mirrors_score_clamp() {
        let rows = vec![quarter(2024, 6, Some(100.0), Some(2.0))];
        let tuned = config::BuyHeuristic { growth_fund_factor: "earnings_yield".to_string(), growth_fund_weight: 1.0, ..Default::default() };
        let (out, _) = render_annual("ACME", "FMP", &rows, today(), Some(100.0), &tuned);
        assert!(out.contains("earnings_yield +2.0% -> +2.0 pts in growth_score (weight 1.0, cap 30)"), "{out}");

        // default weight 0.0 -> the SAME ratio shown (still info), but no pts clause (it isn't weighed)
        let (off, _) = render_annual("ACME", "FMP", &rows, today(), Some(100.0), &config::BuyHeuristic::default());
        assert!(off.contains("earnings_yield +2.0%"), "{off}");
        assert!(!off.contains("-> "), "weight-0 must not claim a score contribution: {off}");
    }

    /// (round 115) missing close -> "-", never a fabricated ratio.
    #[test]
    fn valuation_line_dash_on_missing_close() {
        let rows = vec![quarter(2024, 6, Some(100.0), Some(2.0))];
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(out.contains("valuation: eps_ttm 2.00  close - (native ccy)  earnings_yield -"), "{out}");
    }

    /// (round 115) SHΔ% column: 100 -> 90 shares (buyback) prints the sign-flipped +10.0%; a >40%
    /// swing (100 -> 30, a 3:1-style split) prints "-", never a fabricated -70.0%; a year missing
    /// shares entirely also prints "-".
    #[test]
    fn share_delta_column_semantics() {
        assert!((sh_delta(Some(90.0), Some(100.0)).unwrap() - 10.0).abs() < 1e-9); // shrank 10% -> +10 (buyback)
        assert_eq!(sh_delta(Some(30.0), Some(100.0)), None);       // -70% swing -> split/M&A guard
        assert_eq!(sh_delta(Some(90.0), None), None);
        assert_eq!(sh_delta(None, Some(100.0)), None);

        let rows = vec![quarter_sh(2024, 6, Some(100.0), Some(100.0)), quarter_sh(2025, 6, Some(100.0), Some(90.0))];
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(out.contains("+10.0%"), "buyback SHΔ% missing: {out}");
        assert!(out.contains("share-count change"), "footnote missing: {out}");
    }

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
