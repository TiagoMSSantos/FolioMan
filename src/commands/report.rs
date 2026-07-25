//! `report [TICKERS]` — inspect a company's ANNUAL income-statement trajectory (revenue, margins,
//! EPS, year-over-year growth) plus the as-of fundamental "grower profile" (of which only the
//! valuation tilt is score-weighed — the rest of the fund lane measured no-edge and prints as info).
//! Workflow: run `screen` to surface the best growers, then `report AAPL` to drill into one before
//! betting. Sources the FMP `stable/income-statement` pipeline first, falling back to SEC EDGAR XBRL
//! (free, no key, no daily cap; US filers) when FMP is throttled/keyless. The profile comes straight
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
    // exit-code contract: asked about ≥1 equity and produced ZERO tables/fund blocks -> exit 1 so a
    // script/cron can tell total failure from success. Partial success (some tickers resolve) stays
    // 0 — some data beats none. Crypto/FX-only invocations stay 0 (nothing to fetch by design).
    // The market line alone does NOT count: it renders from the quote even for dead statements
    // pipes, and a run that only echoed prices back is a failed drill-in.
    let mut equity_requested = 0u32;
    let mut tables_printed = 0u32;

    // (round 115) one quote fetch for the valuation line: close_native pairs with the native EPS so
    // the printed earnings_yield is the SAME number the live enrich feeds growth_score (fetch.rs) —
    // no new math, no currency skew. A failed/short fetch just leaves the close absent ("-").
    // (data round) the whole Quote is kept now, not just the close: the market line prints the
    // screen row's essentials (CAGR/1Y/vol/maxdd/R²/extension/turnover) beside the fundamentals —
    // the data was already fetched and discarded. Currency-quoted names (crypto/FX) are fetched
    // too: they carry no statements, but their price history is exactly as real, and excluding
    // them left `report BTC-EUR` with nothing but a shrug.
    let quotes_by: std::collections::HashMap<String, core::Quote> = if tickers.is_empty() {
        Default::default()
    } else {
        let fx_cache = fetch::fx_cache();
        // real (inflation-adjusted) 1Y+ returns, SAME as screen/check — otherwise the market line
        // and the verdict score would read nominal while the ranking is real, disagreeing with the
        // screen table the reader came from. One cached HICP call, only when the adjustment is on.
        let eu_infl = if settings.inflation_adjust.enabled {
            Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
        } else {
            None
        };
        fetch::quotes(&client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, false, &settings.anchor_windows, eu_infl.as_ref())
            .await
            .into_iter()
            .map(|q| (q.ticker.clone(), q))
            .collect()
    };

    for ticker in &tickers {
        // crypto/FX carry no income statement -> don't waste an FMP budget slot probing.
        // Suffix check, NOT contains('-'): share-class tickers are dash-normalized (BRK.B -> BRK-B)
        // and must fall through to the fetch.
        let q = quotes_by.get(ticker.as_str());
        if let Some(q) = q {
            println!("{}", market_line(q, today, settings.stale_days));
        }
        if picks::is_currency_quoted(ticker) {
            println!("\n{ticker}: no income statement (crypto/FX)");
            continue;
        }
        equity_requested += 1;
        let (rows, source) = match fetch::fetch_fundamentals_report(&client, &settings.urls, ticker).await {
            Some(r) => r,
            None => {
                // (data round) no statements ≠ nothing to say: the book is mostly ETFs, which
                // carry no income statement at all. Fall through to the fund side via the cacheless
                // seams (TER/AUM/category + holdings + sector weights + equity/bond split) before
                // giving up; a crumb/transport failure is environmental and leaves the no-data line.
                let (ter, aum, category) =
                    fetch::fund_facts_ext_live(&client, ticker).await.unwrap_or((None, None, None));
                let (holdings, sectors, stock_bond, _fund_pe) =
                    fetch::fund_composition_live(&client, ticker).await.unwrap_or_default();
                if ter.is_some() || aum.is_some() || !holdings.is_empty() || !sectors.is_empty() {
                    print!(
                        "{}",
                        render_fund(ticker, ter, aum, category.as_deref(), &holdings, &sectors, stock_bond)
                    );
                    tables_printed += 1;
                } else {
                    println!("\n{ticker}: {no_data}");
                }
                // ETFs/funds carry no fund_factor on either side (no statements) -> the score
                // matches the screen table exactly, no tilt mirror needed.
                if let Some(q) = q {
                    println!("{}", verdict_line(q, &settings.buy_heuristic));
                }
                continue;
            }
        };
        let close = q.and_then(|q| q.close_native);
        let tcagr = q.and_then(|q| q.trend_cagr);
        let (block, has_table) =
            render_annual(ticker, source, &rows, today, close, tcagr, &settings.buy_heuristic);
        print!("{block}");
        if has_table {
            tables_printed += 1;
        }
        // verdict last (conclusion position). Mirror the live fund tilt so the printed score
        // matches the valuation cell's `-> pts` just above it: report's Quote arrives with
        // fund_factor None (the enrich runs in screen/check, not here).
        if let Some(q) = q {
            let mut scored = q.clone();
            if scored.fund_factor.is_none() {
                let mut ff = core::fund_factors(&rows, today, 5); // pure, cheap; same as the valuation line
                ff.earnings_yield = close.and_then(|p| core::earnings_yield(ff.eps_ttm, p));
                // ponytail: annual EPS like the valuation cell above — skips fetch.rs's sec_ttm_eps
                // TTM override, so report stays self-consistent; diverges from screen only for
                // mid-ramp US growers (where the valuation line already shows the same annual ey).
                scored.fund_factor =
                    core::select_fund_factor(&ff, &settings.buy_heuristic.growth_fund_factor);
            }
            println!("{}", verdict_line(&scored, &settings.buy_heuristic));
        }
    }
    if equity_requested > 0 && tables_printed == 0 {
        std::process::exit(1);
    }
}

/// Render one ticker's annual table + grower profile; pure so the real branches — the +inf% YoY
/// guard, the incomplete-year `*` mark, the empty-rollup note — are unit-testable offline (same
/// seam split as the trading212 render_summary). Returns (text, whether a fiscal-year table
/// rendered) for run()'s exit-code rule.
fn render_annual(
    ticker: &str, source: &str, rows: &[core::FundRow], today: chrono::NaiveDate,
    close_native: Option<f64>, trend_cagr: Option<f64>, tuning: &config::BuyHeuristic,
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

    // grower profile — the as-of factors (5y lookback matches the live enrich). Of everything on
    // these two lines, ONLY the valuation tilt below is score-weighed: the fund lane measured
    // no-edge and is closed, so these print as info, never as score inputs.
    let ff = core::fund_factors(rows, today, 5);
    out.push_str("--- grower profile (info — only the valuation tilt below is score-weighed) ---\n");
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

    // (round 5) the two probe valuation cousins, from the SAME native close + SEC levels the
    // backtest fills them with; helpers None-out loss-makers/no-SEC rows -> "-", never a
    // fabricated signal. Info cells only — neither is ever score-weighed live.
    let evy = close_native
        .and_then(|p| core::ev_ebitda_yield(ff.ebitda_ttm, ff.shares_ttm, ff.net_debt, p));
    let peg = close_native.and_then(|p| core::peg_yield(ff.eps_ttm, trend_cagr, p));
    out.push_str(&format!("  ebitda_yield {}  peg_yield {}", yoy(evy), pts(peg)));
    // (#3) peg_yield became the SHIPPED tilt on 2026-07-25, so "never scored" is no longer true of it —
    // show the same `-> pts` clause the valuation line above gets, from the same weight×clamp arithmetic
    // growth_score runs. ebitda_yield stays info-only. Weight 0 or another factor selected -> the old
    // info wording, so a drill-in never claims a contribution the score isn't actually making.
    if tuning.growth_fund_factor == "peg_yield" && tuning.growth_fund_weight > 0.0 {
        let contrib = peg.map(|v| tuning.growth_fund_weight * v.clamp(0.0, tuning.growth_fund_cap));
        out.push_str(&format!(
            " -> {} pts in growth_score (weight {:.3}, cap {:.0}); ebitda_yield info only",
            pts(contrib), tuning.growth_fund_weight, tuning.growth_fund_cap,
        ));
    } else {
        out.push_str(" (info — probe factors, never scored)");
    }
    out.push('\n');

    let has_table = !annual.is_empty();
    (out, has_table)
}

/// (data round) One compact price-side line per ticker, from the Quote run() already fetches for
/// the valuation close — the screen row's essentials beside the fundamentals at drill-in time,
/// zero new network. Zeroed trend fields (stub/short history) print "-"/"at high", never a
/// fabricated 0. Report MARKS a stale price instead of dropping the listing like `screen` does —
/// the user asked for this ticker by name; `~` mirrors the screen table's bridged-history glyph.
/// Pure formatting.
fn market_line(q: &core::Quote, today: chrono::NaiveDate, stale_days: i64) -> String {
    let h = |label: &str| {
        core::HORIZONS
            .iter()
            .position(|(l, _)| *l == label)
            .and_then(|i| q.perf.get(i))
            .and_then(|p| p.as_ref().map(|(_, pct)| *pct))
    };
    // same threshold + calendar-day arithmetic as screen's (D) stale gate; 0 = off
    let stale = q
        .last_close_date
        .map(|d| (today - d).num_days())
        .filter(|days| stale_days > 0 && *days > stale_days)
        .map(|days| format!(" (stale {days}d)"))
        .unwrap_or_default();
    format!(
        "\n{} — market: {}{}  cagr {}{} ({})  1y {}  5y {}  10y {}  vol {}  maxdd {}  r2 {:.2}  abv-200wk {}  off-hi {}  turnover {}",
        q.ticker,
        q.price_eur.map(|p| format!("€{p:.2}")).unwrap_or_else(|| "-".into()),
        stale,
        if q.history_proxied { "~" } else { "" },
        yoy(q.life_cagr),
        q.age_years.map(|y| format!("{y:.0}y")).unwrap_or_else(|| "-".into()),
        yoy(h("1Y")),
        yoy(h("5Y")),
        yoy(h("10Y")),
        q.volatility_pct.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "-".into()),
        if q.max_drawdown_pct > 0.0 { format!("-{:.0}%", q.max_drawdown_pct) } else { "-".into() },
        q.trend_r2,
        if q.above_ma_pct > 0.0 { format!("+{:.0}%", q.above_ma_pct) } else { "-".into() },
        if q.drawdown_pct > 0.0 { format!("-{:.1}%", q.drawdown_pct) } else { "at high".into() },
        q.avg_turnover_eur.map(|t| format!("€{}", humanize(t))).unwrap_or_else(|| "-".into()),
    )
}

/// (data round) Fund drill-in for no-statement names — the ETF book: TER/AUM + top-10 holdings via
/// the round-3 cacheless seams. Rendered only when at least one field arrived; the caller keeps the
/// tailored no-data line for the true-nothing case. AUM is quote-currency ≈ EUR (same approximation
/// the screen's AUM column already makes). Pure formatting.
fn render_fund(
    ticker: &str,
    ter: Option<f64>,
    aum: Option<f64>,
    category: Option<&str>,
    holdings: &[(String, f64)],
    sectors: &[(String, f64)],
    stock_bond: Option<(f64, f64)>,
) -> String {
    let mut out = format!("\n{ticker} — fund profile (no income statements: ETF/fund)\n");
    out.push_str(&format!(
        "  TER {}  AUM {}{}\n",
        ter.map(|t| format!("{t:.2}%")).unwrap_or_else(|| "-".into()),
        aum.map(|a| format!("€{}", humanize(a))).unwrap_or_else(|| "-".into()),
        category.map(|c| format!("  category {c}")).unwrap_or_default(),
    ));
    if let Some((stock, bond)) = stock_bond {
        // (round 7) fractions -> %; equity vs bond is the buy-and-hold class question the sector-tech
        // table can't answer. bond only when it actually holds some (an equity ETF reads ~100/0).
        out.push_str(&format!("  assets: equity {:.0}%", stock * 100.0));
        if bond * 100.0 >= 0.5 {
            out.push_str(&format!("  bond {:.0}%", bond * 100.0));
        }
        out.push('\n');
    }
    if !sectors.is_empty() {
        // (round 7) the discriminator top-10 holdings hide: concentrated sector bet vs broad core.
        out.push_str("  sectors:");
        for (name, w) in sectors {
            out.push_str(&format!("  {name} {:.0}%", w * 100.0));
        }
        out.push('\n');
    }
    if !holdings.is_empty() {
        out.push_str("  top holdings:");
        for (name, w) in holdings.iter().take(10) {
            // seam weights are FRACTIONS (holdingPercent.raw, 0.058 = 5.8%) — scale for display
            out.push_str(&format!("  {name} {:.1}%", w * 100.0));
        }
        out.push('\n');
    }
    out
}

/// (round 6) The growth lane's VERDICT on this one name — the thing a report drill-in never said,
/// so the reader round-tripped back to the screen table for it. Reproduces the SAME per-quote
/// machinery the screen ranks with: `picks::growth_score` (Some = ranked, incl. a floored 0.0;
/// None = unrankable), the late-cycle brake flag (`above_ma_pct >= growth_overext_cap`, screen's
/// `!`), the hold-suitable core flag (screen's `H`), and `growth_near_miss` / `gate_failures` for
/// the "why isn't this in my screen" answer. A scored name is NOT called REJECT — a late-cycle
/// name still RANKS (VVSM is #10 in the list), so it prints its score + the same "conviction is the
/// score" note the legend uses. Display-only; the caller mirrors the live fund tilt before scoring
/// so the number matches the valuation cell above it. Pure.
fn verdict_line(q: &core::Quote, tuning: &config::BuyHeuristic) -> String {
    let cap = tuning.growth_overext_cap; // equities/ETFs only here — crypto lane isn't verdicted
    match picks::growth_score(q, tuning) {
        Some(s) => {
            let tail = if cap > 0.0 && q.above_ma_pct >= cap {
                format!(
                    " — late-cycle: price {:.0}% above 200wk trend, brake floored (conviction is the score)",
                    q.above_ma_pct,
                )
            } else if core::hold_suitable(q) {
                " — hold-suitable 20yr core (broad + cheap + physical + Acc + large)".to_string()
            } else {
                String::new()
            };
            format!("\n{} — verdict: growth_score {s:.1}{tail}", q.ticker)
        }
        None => {
            let why = picks::growth_near_miss(q, tuning)
                .map(|(gate, w)| format!("near miss on {gate}: {w}"))
                .or_else(|| {
                    picks::gate_failures(q, tuning).filter(|f| !f.is_empty()).map(|fails| {
                        let gates: Vec<&str> = fails.iter().map(|(g, _, _)| *g).collect();
                        format!("fails {}", gates.join(", "))
                    })
                })
                .unwrap_or_else(|| {
                    "not a growth-lane candidate (leveraged / stablecoin / unknown turnover)".to_string()
                });
            format!("\n{} — verdict: not ranked — {why}", q.ticker)
        }
    }
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
        render_annual(ticker, source, rows, today(), None, None, &config::BuyHeuristic::default())
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
    /// Pinned to the rendered "inf%" cell, not bare "inf" — prose like "info" must not trip it.
    #[test]
    fn zero_revenue_older_year_is_dash_not_inf() {
        let rows = vec![quarter(2024, 6, Some(0.0), None), quarter(2025, 6, Some(50.0), None)];
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(!out.contains("inf%"), "{out}");
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
        let (out, _) = render_annual("ACME", "FMP", &rows, today(), Some(100.0), None, &tuned);
        assert!(out.contains("earnings_yield +2.0% -> +2.0 pts in growth_score (weight 1.0, cap 30)"), "{out}");

        // default weight 0.0 -> the SAME ratio shown (still info), but no pts clause (it isn't weighed)
        let (off, _) = render_annual("ACME", "FMP", &rows, today(), Some(100.0), None, &config::BuyHeuristic::default());
        assert!(off.contains("earnings_yield +2.0%"), "{off}");
        assert!(!off.contains("-> "), "weight-0 must not claim a score contribution: {off}");
    }

    /// (round 115) missing close -> "-", never a fabricated ratio.
    #[test]
    fn valuation_line_dash_on_missing_close() {
        let rows = vec![quarter(2024, 6, Some(100.0), Some(2.0))];
        let (out, _) = render("ACME", "FMP", &rows);
        assert!(out.contains("valuation: eps_ttm 2.00  close - (native ccy)  earnings_yield -"), "{out}");
        assert!(out.contains("ebitda_yield -  peg_yield - (info — probe factors, never scored)"), "{out}");
    }

    /// (round 6) verdict line reproduces the screen's per-quote verdict at drill-in time. Build a
    /// scoring quote from the stub + the handful of fields growth_score reads (range clears the 80
    /// gate, known turnover admits, 10Y 200% ≈ 11.6%/yr clears the 8 floor). Branches: ranked ->
    /// score; late-cycle (above_ma ≥ cap) -> the floored-brake tail; range-only miss -> near miss;
    /// two-gate reject -> fails list; bare stub (no turnover/history) -> not-a-candidate. Plus the
    /// tilt-mirror premise: a set fund_factor moves the rendered score.
    #[test]
    fn verdict_line_branches_and_tilt() {
        let scoring = || {
            let mut q = core::Quote::stub("IITU.L", "€1", "", "n");
            q.range_pct = 95.0; // clears the 80 range gate
            q.avg_turnover_eur = Some(1e9); // known turnover -> assessable
            q.perf = vec![None; core::HORIZONS.len()];
            // 8Y carries the same 11.6%/yr as the 10Y leg — the 20/8/5 ladder reads 8Y first, and real
            // history is contiguous, so a 10Y record without an 8Y one is an impossible quote.
            for (label, pct) in [("1Y", 10.0), ("5Y", 40.0), ("8Y", 140.8), ("10Y", 200.0)] {
                let i = core::HORIZONS.iter().position(|(l, _)| *l == label).unwrap();
                q.perf[i] = Some(("x".to_string(), pct));
            }
            q
        };
        let t = config::BuyHeuristic::default();

        // ranked -> plain score, no reject wording
        let ranked = verdict_line(&scoring(), &t);
        assert!(ranked.contains("IITU.L — verdict: growth_score "), "{ranked}");
        assert!(!ranked.contains("not ranked") && !ranked.contains("late-cycle"), "{ranked}");

        // late-cycle: price past the overextension cap -> the floored-brake tail (still ranked)
        let mut late = scoring();
        late.above_ma_pct = t.growth_overext_cap + 68.0;
        let lc = verdict_line(&late, &t);
        assert!(lc.contains("growth_score ") && lc.contains("late-cycle: price"), "{lc}");
        assert!(lc.contains("brake floored (conviction is the score)"), "{lc}");

        // range-only miss (range 75 < 80, everything else clears) -> near miss on that one gate
        let mut miss = scoring();
        miss.range_pct = 75.0;
        let nm = verdict_line(&miss, &t);
        assert!(nm.contains("not ranked — near miss on range:"), "{nm}");

        // two gates fail (range AND cagr) -> fails-list, not a near miss. The 20/8/5 ladder reads the
        // 8Y leg, so that is the one that has to be weak: +30.7% over 8y ≈ 3.4%/yr, under the 8 floor.
        let mut two = scoring();
        two.range_pct = 75.0;
        for (label, pct) in [("8Y", 30.7), ("10Y", 40.0)] {
            let i = core::HORIZONS.iter().position(|(l, _)| *l == label).unwrap();
            two.perf[i] = Some(("x".to_string(), pct));
        }
        let fl = verdict_line(&two, &t);
        assert!(fl.contains("not ranked — fails ") && fl.contains("range"), "{fl}");

        // bare stub: no turnover, no history -> not assessable as a candidate at all
        let bare = verdict_line(&core::Quote::stub("X", "err", "", "X"), &t);
        assert!(bare.contains("not ranked — not a growth-lane candidate"), "{bare}");

        // tilt-mirror premise: the run() call sets fund_factor before scoring, so the verdict must
        // MOVE when it's present (guards the mirror from silently feeding None — round-5 lesson).
        let tilt = config::BuyHeuristic {
            growth_fund_factor: "earnings_yield".to_string(),
            growth_fund_weight: 1.0,
            ..Default::default()
        };
        let flat = verdict_line(&scoring(), &tilt);
        let mut tilted_q = scoring();
        tilted_q.fund_factor = Some(20.0);
        let tilted = verdict_line(&tilted_q, &tilt);
        assert_ne!(flat, tilted, "a set fund_factor must move the verdict score: {flat} vs {tilted}");
    }

    /// (round 5) probe valuation cousins from the SEC levels + native close: EV = 10·100 + 100
    /// net debt = 1100 -> ebitda_yield 50/1100 = +4.5%; peg = earnings_yield 2.0% × trend CAGR 15
    /// -> +30.0. No-SEC rows (levels None) keep "-" even with a close — no fabricated signal.
    #[test]
    fn probe_valuation_cousins_render_and_dash() {
        let rows = vec![core::FundRow {
            ebitda: Some(50.0),
            shares: Some(10.0),
            net_debt: Some(100.0),
            ..quarter(2024, 6, Some(100.0), Some(2.0))
        }];
        let (out, _) =
            render_annual("ACME", "SEC", &rows, today(), Some(100.0), Some(15.0), &config::BuyHeuristic::default());
        assert!(out.contains("ebitda_yield +4.5%  peg_yield +30.0 (info — probe factors, never scored)"), "{out}");

        // (#3) with peg_yield SELECTED (the shipped tilt) the same cell must claim its score
        // contribution instead of "never scored": peg 30.0 × weight 0.018 = +0.5 pts, cap 300 not
        // binding. Guards the drill-in against under-reporting the live tilt (round-5 lesson, mirrored).
        let peg_tilt = config::BuyHeuristic {
            growth_fund_factor: "peg_yield".to_string(),
            growth_fund_weight: 0.018,
            growth_fund_cap: 300.0,
            ..Default::default()
        };
        let (tilted, _) =
            render_annual("ACME", "SEC", &rows, today(), Some(100.0), Some(15.0), &peg_tilt);
        assert!(tilted.contains("peg_yield +30.0 -> +0.5 pts in growth_score (weight 0.018, cap 300)"), "{tilted}");
        assert!(!tilted.contains("never scored"), "selected tilt must not read as unscored: {tilted}");

        // FMP free-tier rows carry no EV/EBITDA levels -> both cells dash despite a live close
        let bare = vec![quarter(2024, 6, Some(100.0), Some(2.0))];
        let (out, _) =
            render_annual("ACME", "FMP", &bare, today(), Some(100.0), Some(15.0), &config::BuyHeuristic::default());
        assert!(out.contains("ebitda_yield -"), "{out}");
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

    /// (data round) market line: a populated quote renders every cell in screen-column semantics
    /// (maxdd/off-hi negative-signed, abv-200wk positive-signed); a bare stub (empty perf, zeroed
    /// trend fields) renders "-"/"at high", never a fabricated 0%.
    #[test]
    fn market_line_cells_and_dashes() {
        let mut q = core::Quote::stub("IITU.L", "€42.84", "", "iShares IT");
        q.life_cagr = Some(25.0);
        q.age_years = Some(11.0);
        q.volatility_pct = Some(1.3);
        q.max_drawdown_pct = 28.0;
        q.trend_r2 = 0.98;
        q.above_ma_pct = 61.0;
        q.drawdown_pct = 17.1;
        q.price_eur = Some(42.84);
        q.perf = vec![None; core::HORIZONS.len()];
        for (label, pct) in [("1Y", 17.1), ("5Y", 98.7), ("10Y", 438.5)] {
            let i = core::HORIZONS.iter().position(|(l, _)| *l == label).unwrap();
            q.perf[i] = Some((String::new(), pct));
        }
        q.avg_turnover_eur = Some(12e6);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        q.last_close_date = Some(today - chrono::Duration::days(2)); // fresh under a 5d gate
        let line = market_line(&q, today, 5);
        assert!(
            line.contains("IITU.L — market: €42.84  cagr +25.0% (11y)  1y +17.1%  5y +98.7%  10y +438.5%  vol 1.3%  maxdd -28%  r2 0.98  abv-200wk +61%  off-hi -17.1%  turnover €12.0M"),
            "{line}"
        );

        // (round 4) integrity markers: stale close flagged (not dropped), bridged history tilded
        q.last_close_date = Some(today - chrono::Duration::days(47));
        q.history_proxied = true;
        let marked = market_line(&q, today, 5);
        assert!(marked.contains("market: €42.84 (stale 47d)  cagr ~+25.0% (11y)"), "{marked}");
        let gate_off = market_line(&q, today, 0); // stale_days 0 = off, same as screen
        assert!(!gate_off.contains("stale"), "{gate_off}");

        let bare = market_line(&core::Quote::stub("X", "err", "", "X"), today, 5);
        assert!(bare.contains("market: -  cagr - (-)  1y -  5y -  10y -  vol -  maxdd -"), "{bare}");
        assert!(bare.contains("abv-200wk -  off-hi at high  turnover -"), "{bare}");
        assert!(!bare.contains("stale"), "{bare}"); // no close date -> no marker, never a false flag
    }

    /// (data round / round 7) fund profile: TER/AUM/category on the facts line; equity/bond split
    /// (bond suppressed near 0); sector spread; holdings join. Absent TER prints "-" (never 0.00%);
    /// a missing category/holdings/sectors/split drops its whole line — no empty scaffolding.
    #[test]
    fn fund_profile_renders_facts_and_holdings() {
        // weights arrive as fractions from the seam (0.225 = 22.5%)
        let holds = vec![("AAPL".to_string(), 0.225), ("MSFT".to_string(), 0.21)];
        let sectors = vec![("Technology".to_string(), 0.62), ("Financial Services".to_string(), 0.18)];
        let out =
            render_fund("IITU.L", Some(0.15), Some(16.1e9), Some("Technology"), &holds, &sectors, Some((0.998, 0.002)));
        assert!(out.contains("IITU.L — fund profile (no income statements: ETF/fund)"), "{out}");
        assert!(out.contains("TER 0.15%  AUM €16.1B  category Technology"), "{out}");
        assert!(out.contains("assets: equity 100%"), "{out}"); // 0.998 -> 100%
        assert!(!out.contains("bond"), "{out}"); // 0.2% bond suppressed (< 0.5%)
        assert!(out.contains("sectors:  Technology 62%  Financial Services 18%"), "{out}");
        assert!(out.contains("top holdings:  AAPL 22.5%  MSFT 21.0%"), "{out}");

        // sparse: no category/holdings/sectors/split -> only the TER/AUM line survives.
        let sparse = render_fund("X.L", None, Some(1e9), None, &[], &[], None);
        assert!(sparse.contains("TER -  AUM €1.0B"), "{sparse}");
        assert!(!sparse.contains("category"), "{sparse}");
        assert!(!sparse.contains("top holdings"), "{sparse}");
        assert!(!sparse.contains("sectors:"), "{sparse}");
        assert!(!sparse.contains("assets:"), "{sparse}");

        // a real bond holding IS shown (mixed fund).
        let mixed = render_fund("AGGH.L", Some(0.1), None, None, &[], &[], Some((0.4, 0.6)));
        assert!(mixed.contains("assets: equity 40%  bond 60%"), "{mixed}");
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
