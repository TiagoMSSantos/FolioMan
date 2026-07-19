//! Pure logic for folioman: types, formatting, market/trend/inflation math.
//! No network here — all I/O lives in `fetch.rs`. Read-only, never trades.
//! Acronyms (CAGR, NUPL, SMA, GICS, R², CdA, …): see the Glossary in README.md.

use chrono::{Datelike, Duration, NaiveDate};
use serde_json::Value;
use std::collections::BTreeMap;

/// label -> calendar days back
pub const HORIZONS: &[(&str, i64)] = &[
    ("1D", 1),
    ("1W", 7),
    ("1M", 30),
    ("3M", 91),
    ("6M", 182),
    ("1Y", 365),
    ("5Y", 1825),
    ("10Y", 3650),
    ("20Y", 7300),
];

/// Certificados de Aforro: base = Euribor3M + spread, capped, floored at 0; a permanence
/// premium (NOT capped) is added on top per holding year — `premium_early` in years 2-5,
/// `premium_late` from year 6 on. All in percentage points.
pub struct CaSeries {
    pub name: &'static str,
    pub spread: f64,
    pub cap: f64,
    pub premium_early: f64, // yr 2-5
    pub premium_late: f64,  // yr 6+
}

pub const CA_SERIES: &[CaSeries] = &[
    CaSeries { name: "E", spread: 1.0, cap: 3.5, premium_early: 0.50, premium_late: 1.00 },
    CaSeries { name: "F", spread: 0.0, cap: 2.5, premium_early: 0.25, premium_late: 0.50 },
];

/// Cumulative % gain on €1 held for `years` whole years, compounding each holding year's
/// rate = base + that year's permanence premium (yr1 none, yr2-5 early, yr6+ late).
/// note: annual compounding, ignores intra-year capitalisation — close enough for a
/// footer estimate, and Euribor (so base) drifts anyway. Assumes today's base holds.
pub fn ca_cumulative_gain(base: f64, premium_early: f64, premium_late: f64, years: i64) -> f64 {
    let mut factor = 1.0;
    for y in 1..=years {
        let premium = if y >= 6 { premium_late } else if y >= 2 { premium_early } else { 0.0 };
        factor *= 1.0 + (base + premium) / 100.0;
    }
    (factor - 1.0) * 100.0
}

/// Yahoo ticker suffix -> market country (listing venue, not legal domicile).
fn suffix_country(suf: &str) -> Option<&'static str> {
    Some(match suf {
        "DE" => "Germany", "L" => "UK", "PA" => "France", "AS" => "Netherlands",
        "MI" => "Italy", "MC" => "Spain", "SW" => "Switzerland", "VI" => "Austria",
        "LS" => "Portugal", "BR" => "Belgium", "HE" => "Finland", "ST" => "Sweden",
        "OL" => "Norway", "CO" => "Denmark", "IR" => "Ireland", "TO" => "Canada",
        "HK" => "Hong Kong", "T" => "Japan", "AX" => "Australia", "SA" => "Brazil",
        "NS" => "India", "SS" => "China", "SZ" => "China", "KS" => "South Korea",
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct Quote {
    pub ticker: String,
    pub price: String,   // "€123.45", "123.45 USD?" (FX unknown), "err", "no data"
    pub dip: String,     // "-3.2%"
    pub drop_pct: f64,   // numeric, for alert threshold
    pub market: String,
    pub instrument_type: String, // Yahoo chart-meta instrumentType ("ETF"/"EQUITY"/"CRYPTOCURRENCY"/...); the reliable asset-class signal, vs the name-substring guess. "" if absent.
    pub head: String,        // first headline ("" if none)
    pub news_block: String,  // up to 3 headlines, "- ..\n- .." (alert body)
    pub perf: Vec<Option<(String, f64)>>, // aligned to HORIZONS: (past_eur_str, pct) or None
    pub name: String,    // human-readable instrument name (falls back to ticker)
    pub trend: String,   // "↑ 2w" / "↓ 5d": current direction + how long it has held
    pub at_ath: bool,    // at/near all-time high (within tol of max seen)
    pub at_atl: bool,    // at/near all-time low (within tol of min seen)
    pub mom_pct: Option<f64>, // % change vs ~1 month ago (None if no data); <0 = falling
    pub div_eur: Vec<Option<f64>>, // total dividends/share (EUR) per DIV_HORIZONS; None = short history
    pub price_eur: Option<f64>, // current close in EUR (None if FX unknown); for dividend yield
    pub close_native: Option<f64>, // (Item 19) latest close in the listing's OWN currency (NOT FX-converted) — paired with native EPS for a currency-consistent earnings_yield, same number the backtest scores on
    pub last_close_date: Option<NaiveDate>, // (D) date of the most recent close bar — stale (old) = a halted/dead listing frozen at an old price; LIVE-only, None in stub/backtest (staleness is a live-fetch data-quality gate)
    pub drawdown_pct: f64, // % below the high of the last ~high_days (picks "on sale" signal)
    pub intraday: [Option<f64>; 3], // % change over [1h, 6h, 12h] = 1/6/12 hourly bars back; None if too few bars
    pub avg_turnover_eur: Option<f64>, // avg daily turnover (close*volume, EUR) ~last 30 sessions; liquidity proxy
    pub volatility_pct: Option<f64>,   // daily-return stdev (%) ~last year; the asset's "normal swing" for the picks score
    pub below_ma_pct: f64,             // % below the ~200-week SMA (structural "cheap vs long trend"); 0 if at/above or history too short
    pub above_ma_pct: f64,             // % ABOVE the ~200-week SMA (overextension "how far it ran"); 0 if at/below or history too short. Growth-lane brake on blow-off tops
    pub pe_ratio: Option<f64>,         // trailing P/E for the valuation tilt; None for crypto/ETF/no-earnings/no source (-> neutral)
    pub roe: Option<f64>,              // (F) trailing return-on-equity (%) — the core profitability/QUALITY factor; None for crypto/ETF/no-earnings/no source (-> neutral). BACKTEST-BLIND (point-in-time, can't reconstruct as-of)
    pub expense_ratio: Option<f64>,    // (TER) ETF annual expense ratio (%); None for stocks/crypto/no source. DISPLAY-ONLY (`ter` column) — the one cost that compounds against a decades hold
    pub range_pct: f64,                // percentile rank (0..100) of the last close in its own ~10y history; 100=at high. picks discount = 100-this
    pub trend_r2: f64,                 // (A) R² (0..1) of the log-price trend — how steadily it compounds; damps CAGR endpoint-luck. 0 = no/short history
    pub trend_cagr: Option<f64>,       // (#14) annualized CAGR from the log-price trend SLOPE (endpoint-robust); precomputed at build, ranked on only when `use_trend_cagr`. None = <2 points
    pub max_drawdown_pct: f64,         // (C) worst peak-to-trough decline (%) in its history; feeds the Calmar (return-per-pain) reward. 0 = never down/no history
    pub roll5y_pos_pct: Option<f64>,   // (consistency) % of rolling ~5y windows with a positive NOMINAL return, from the same closes. DISPLAY-ONLY footer ("how often did 5 patient years pay?"); None = <5y of history — never a fake 100%
    pub underwater_yrs: Option<f64>,   // (underwater) longest stretch below the prior peak, in years (~252 sessions/yr), ongoing stretch counts. DISPLAY-ONLY footer — MAXDD's missing twin: depth says how far down, this says how LONG the pain lasted. None = <2 usable closes
    pub worst_5y_pct: Option<f64>,     // (worst-5y) single worst rolling ~5y NOMINAL outcome (%), severity twin of roll5y_pos_pct's frequency. DISPLAY-ONLY footer; None = <5y of history — no claim
    pub fund_factor: Option<f64>,      // (G) the ONE as-of fundamental factor folded into growth_score (e.g. revenue accel). Set in the backtest (from fund_factors) so the term is ablatable, and live only on the small/check-scale path; None -> neutral (universe screen & price-only backtest)
    pub age_years: Option<f64>,        // listing age in years from the FULL (monthly-backfilled) history; DISPLAY-ONLY (`yrs` column). None = no data / stub / backtest
    pub life_cagr: Option<f64>,        // whole-life endpoint CAGR (%) over that full history; DISPLAY-ONLY (`cagr` column). Ranking/gates stay on the validated fixed-horizon ladder. None = <6mo history / stub / backtest
    pub tr_cagr: Option<f64>,          // (TR-CAGR) life_cagr + the whole-life dividend sum added to the endpoint — LOWER-BOUND total return (payouts added, not reinvested). DISPLAY-ONLY (`trcagr` column), never scored; ≈ life_cagr for Acc funds/non-payers
    pub history_proxied: bool,         // (history_proxy) closes bridged from a configured older same-strategy twin — CAGR/YRS describe the STRATEGY, not this listing; rendered as `~` so the bridge is never invisible
    pub aum_eur: Option<f64>,          // (AUM) fund size from the Börse Frankfurt universe payload, EUR-approximate (BF mixes fund currencies; ±FX is immaterial vs the order-of-magnitude gate). ETFs/ETPs only; None = not a fund / not in BF / backtest -> gate inert
    pub ter_fallback: Option<f64>,     // Yahoo quoteSummary TER (%) for funds with NO BF facts (venue/regulatory-only rows). Read ONLY via ter_shown() for display + H/CORE — kept out of expense_ratio because ter_damp SCORES that field (a merged run moved live ranks; scoring lane closed)
    pub aum_fallback: Option<f64>,     // Yahoo quoteSummary totalAssets for the same funds, quote-currency ≈ EUR. Read ONLY via aum_shown() for display + H/CORE — the closure-risk AUM gate stays on BF aum_eur
    pub use_of_profits: Option<&'static str>, // (USE) share class from the same BF row: "Acc"/"Dist". DISPLAY-ONLY — never scored: the price-only CAGR already prices the Dist payout drag (payouts leave the NAV), so Acc twins win by construction
    pub replication: Option<&'static str>,    // (REPL) replication method, same BF row: "Swap"/"Full"/"Opt"/"Hybr"/"Samp". DISPLAY-ONLY counterparty-structure legibility (swap-based US-index funds also legally dodge dividend withholding — why they track so well)
    pub benchmark: Option<String>,     // BF benchmark-index name, lowercased at capture (BF normalizes it: same-index funds share the literal string, hedged classes differ). Used ONLY for history_proxy twin HINTS — never scored, never a match key beyond exact `==`
    pub domicile: Option<String>,      // (DOM) fund legal domicile from the ISIN prefix ("IE"/"LU"/"DE"…). DISPLAY + CORE-shortlist ordering (IE first: 15% US-dividend withholding treaty vs LU's 30% ≈ +0.2%/yr on a US/world fund) — never scored; None for stocks/crypto, watchlist-only runs and backtest
    pub rev_yoy: Option<f64>,          // newest COMPLETE-fiscal-year revenue growth (%) vs the prior FY, from the same income-statement pipeline `report` prints. DISPLAY-ONLY (stocks) — the fund-factor family measured null for ranking; enriched only for the displayed top rows, None otherwise/backtest
    pub eps_yoy: Option<f64>,          // newest complete-FY EPS growth (%) vs the prior FY. DISPLAY-ONLY, same scoping as rev_yoy
    pub net_margin_fy: Option<f64>,    // newest complete-FY net margin (%). DISPLAY-ONLY, same scoping as rev_yoy
    pub buyback_yoy: Option<f64>,      // newest complete-FY net share-count change, sign-flipped (+ = buying back, − = diluting). DISPLAY-ONLY (stocks), same scoping as rev_yoy
    pub annual_brief: Option<String>,  // (B) one-line multi-year trajectory (rev chain + margin move + EPS CAGR + source) from the SAME rollup the snapshot above uses — screen's fundamentals footer. DISPLAY-ONLY, same scoping as rev_yoy
}

impl Quote {
    /// A bare row for error/no-data cases (mirrors Python's "err"/"no data" Quote).
    pub fn stub(ticker: &str, price: &str, head: &str, name: &str) -> Quote {
        Quote {
            ticker: ticker.to_string(),
            price: price.to_string(),
            dip: String::new(),
            drop_pct: 0.0,
            market: market_of(ticker),
            instrument_type: String::new(),
            head: head.to_string(),
            news_block: String::new(),
            perf: Vec::new(),
            name: name.to_string(),
            trend: String::new(),
            at_ath: false,
            at_atl: false,
            mom_pct: None,
            div_eur: Vec::new(),
            price_eur: None,
            close_native: None,
            last_close_date: None,
            drawdown_pct: 0.0,
            intraday: [None; 3],
            avg_turnover_eur: None,
            volatility_pct: None,
            below_ma_pct: 0.0,
            above_ma_pct: 0.0,
            pe_ratio: None,
            roe: None,
            expense_ratio: None,
            range_pct: 0.0,
            trend_r2: 0.0,
            trend_cagr: None,
            max_drawdown_pct: 0.0,
            roll5y_pos_pct: None,
            underwater_yrs: None,
            worst_5y_pct: None,
            fund_factor: None,
            age_years: None,
            life_cagr: None,
            tr_cagr: None,
            history_proxied: false,
            aum_eur: None,
            ter_fallback: None,
            aum_fallback: None,
            use_of_profits: None,
            replication: None,
            benchmark: None,
            domicile: None,
            rev_yoy: None,
            eps_yoy: None,
            net_margin_fy: None,
            annual_brief: None,
            buyback_yoy: None,
        }
    }

    /// TER as SHOWN: BF first, Yahoo fallback second. For display cells + the H/CORE flag only —
    /// never feed this into scoring/gates (they stay on the raw BF `expense_ratio` so momentum ranks
    /// are byte-identical with pre-fallback runs).
    pub fn ter_shown(&self) -> Option<f64> {
        self.expense_ratio.or(self.ter_fallback)
    }

    /// AUM as SHOWN: BF first, Yahoo fallback second. Same display/H-CORE-only stance as `ter_shown`.
    pub fn aum_shown(&self) -> Option<f64> {
        self.aum_eur.or(self.aum_fallback)
    }
}

/// Compound annual growth rate (%) implied by a cumulative % over `years`: +285% over 10y ≈
/// 14.4%/yr. Annualizing makes returns over different spans comparable (a 5y vs a 10y vs a 20y
/// leg). Clamps the growth factor just above 0 so a near-total loss can't NaN the fractional root.
pub fn cagr(cumulative_pct: f64, years: f64) -> f64 {
    if years <= 0.0 {
        return cumulative_pct;
    }
    let factor = (1.0 + cumulative_pct / 100.0).max(1e-9);
    (factor.powf(1.0 / years) - 1.0) * 100.0
}

/// % the latest close sits below the simple moving average of the last `n` sessions (~a long-term
/// trend line; n≈1000 ≈ 200 weeks). 0 if at/above the average or history shorter than `n`. A
/// structural "cheap vs its own long trend" entry signal, distinct from the recency-biased 1Y-high
/// drawdown — buying below the multi-year trend, not just below last year's peak.
pub fn below_long_ma_pct(closes: &[f64], n: usize) -> f64 {
    if n == 0 || closes.len() < n {
        return 0.0;
    }
    let ma = closes[closes.len() - n..].iter().sum::<f64>() / n as f64;
    if ma <= 0.0 {
        return 0.0;
    }
    // (#19) deliberately the RAW last close, NOT measure_endpoint: smoothing this endpoint was
    // measured WORSE (backtest A/B at the 5-bar window: edge +115.7 smoothed vs +120.2 raw) — a
    // smoothed endpoint under-reads a fresh spike, so the overext brake docks parabolic names less.
    f64::max(0.0, (ma - *closes.last().expect("closes non-empty: closes.len() >= n >= 1 guarded above")) / ma * 100.0)
}

/// % the latest close sits ABOVE the moving average of the last `n` sessions — the mirror of
/// `below_long_ma_pct`. How far a name has run past its own long-term trend line; an
/// overextension/blow-off gauge for the growth lane (price 100% above its 200wk SMA = stretched).
/// 0 if at/below the average or history shorter than `n`.
pub fn above_long_ma_pct(closes: &[f64], n: usize) -> f64 {
    if n == 0 || closes.len() < n {
        return 0.0;
    }
    let ma = closes[closes.len() - n..].iter().sum::<f64>() / n as f64;
    if ma <= 0.0 {
        return 0.0;
    }
    // (#19) raw last close on purpose — see below_long_ma_pct; the brake must see the spike.
    f64::max(0.0, (*closes.last().expect("closes non-empty: closes.len() >= n >= 1 guarded above") - ma) / ma * 100.0)
}

/// (A) R² (0..1) of a straight-line fit to LOG price over time — how STEADILY the asset compounds. A
/// smooth exponential compounder → ~1; a lumpy path that mooned-then-chopped to the same endpoint →
/// lower. Damps CAGR's endpoint-luck (a lucky start/end pair on a jagged path isn't a durable trend).
/// 0 for <2 usable points; non-positive closes are skipped (log undefined). Flat = 1 (zero residual).
pub fn trend_r2(closes: &[f64]) -> f64 {
    let ys: Vec<f64> = closes.iter().filter(|&&c| c > 0.0).map(|c| c.ln()).collect();
    let n = ys.len();
    if n < 2 {
        return 0.0;
    }
    let xmean = (n as f64 - 1.0) / 2.0; // x = 0..n-1
    let ymean = ys.iter().sum::<f64>() / n as f64;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for (i, &y) in ys.iter().enumerate() {
        let dx = i as f64 - xmean;
        let dy = y - ymean;
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    if syy <= 0.0 || sxx <= 0.0 {
        return 1.0; // flat log-price = zero residual variance = perfectly "consistent"
    }
    (sxy * sxy / (sxx * syy)).clamp(0.0, 1.0)
}

/// (#14) Annualized CAGR (%) from the SLOPE of the least-squares log-price line — the same fit
/// `trend_r2` makes, returning the trend itself instead of its R². Robust to endpoint luck: one freak
/// start/end close barely moves a fitted line, unlike `cagr`, which is pure endpoint-to-endpoint and
/// so hostage to the exact first/last day. `cadence` = bars/year (252 daily, 12 monthly) annualizes
/// the per-bar log slope: CAGR = exp(slope × cadence) − 1. None for <2 usable points (log undefined /
/// degenerate fit); non-positive closes are skipped. Mirrors `trend_r2`'s loop so the two stay aligned.
pub fn trend_cagr(closes: &[f64], cadence: usize) -> Option<f64> {
    let ys: Vec<f64> = closes.iter().filter(|&&c| c > 0.0).map(|c| c.ln()).collect();
    let n = ys.len();
    if n < 2 {
        return None;
    }
    let xmean = (n as f64 - 1.0) / 2.0; // x = 0..n-1
    let ymean = ys.iter().sum::<f64>() / n as f64;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (i, &y) in ys.iter().enumerate() {
        let dx = i as f64 - xmean;
        sxx += dx * dx;
        sxy += dx * (y - ymean);
    }
    if sxx <= 0.0 {
        return None; // all x identical (n<2 already handled) -> no slope
    }
    let slope = sxy / sxx; // log-price per bar
    Some(((slope * cadence as f64).exp() - 1.0) * 100.0)
}

/// (C) Worst peak-to-trough decline (%) ever seen in the series — the deepest pain a holder endured.
/// One forward pass tracking the running peak. 0 for empty / never-down. Feeds the Calmar
/// (return-per-worst-pain) reward: a name that compounds hard with a SHALLOW max drawdown is durable.
pub fn max_drawdown_pct(closes: &[f64]) -> f64 {
    let (mut peak, mut worst) = (f64::MIN, 0.0_f64);
    for &c in closes {
        if c > peak {
            peak = c;
        }
        if peak > 0.0 {
            worst = worst.max((peak - c) / peak * 100.0);
        }
    }
    worst
}

/// (consistency footer) % of rolling ~5-year windows (1260 sessions) whose endpoint return is
/// positive, stepped weekly (5 sessions) so overlapping duplicates don't drown short histories.
/// The literal buy-and-hold question: how often did 5 patient years pay? NOMINAL, not
/// inflation-adjusted — the footer label says so. None = history shorter than one window (no
/// windows means no claim, never a fake 100%); a non-positive close on either end skips that
/// window (halted/bad bars).
pub fn rolling_5y_positive_pct(closes: &[f64]) -> Option<f64> {
    const WIN: usize = 5 * 252;
    const STEP: usize = 5;
    let (mut pos, mut n) = (0usize, 0usize);
    let mut i = 0;
    while i + WIN < closes.len() {
        let (a, b) = (closes[i], closes[i + WIN]);
        if a > 0.0 && b > 0.0 {
            n += 1;
            pos += (b > a) as usize;
        }
        i += STEP;
    }
    (n > 0).then(|| 100.0 * pos as f64 / n as f64)
}

/// (underwater) Longest stretch of sessions spent below the running peak, in years (~252
/// sessions/yr) — MAXDD's missing twin: depth says how far down, this says how LONG until back
/// to even. An ongoing stretch counts at its elapsed length (whether it's underwater NOW is the
/// OFF-HI column's job). Non-positive closes (data holes) are filtered out first; a monotonic
/// riser legitimately reports 0.0; fewer than 2 usable closes -> None.
pub fn longest_underwater_yrs(closes: &[f64]) -> Option<f64> {
    let px: Vec<f64> = closes.iter().copied().filter(|c| *c > 0.0).collect();
    if px.len() < 2 {
        return None;
    }
    let (mut peak_i, mut peak, mut worst) = (0usize, px[0], 0usize);
    for (i, &c) in px.iter().enumerate().skip(1) {
        if c >= peak {
            peak = c;
            peak_i = i;
        } else {
            worst = worst.max(i - peak_i);
        }
    }
    Some(worst as f64 / 252.0)
}

/// (worst-5y) The single worst rolling ~5y (nominal) outcome — severity twin of
/// `rolling_5y_positive_pct`'s frequency ("97% of windows positive; the worst one did −12%").
/// Same WIN/STEP walk and skip rules: a window with a non-positive close on either end is
/// skipped; `None` when no full window exists — no claim, never a fake number.
pub fn worst_rolling_5y_pct(closes: &[f64]) -> Option<f64> {
    const WIN: usize = 5 * 252;
    const STEP: usize = 5;
    let mut worst: Option<f64> = None;
    let mut i = 0;
    while i + WIN < closes.len() {
        let (a, b) = (closes[i], closes[i + WIN]);
        if a > 0.0 && b > 0.0 {
            let r = 100.0 * (b / a - 1.0);
            worst = Some(worst.map_or(r, |w: f64| w.min(r)));
        }
        i += STEP;
    }
    worst
}

/// Format a number with comma thousands separators and 2 decimals (Python `{:,.2f}`).
pub fn fmt_money2(x: f64) -> String {
    let neg = x < 0.0;
    let s = format!("{:.2}", x.abs());
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), "00"));
    let bytes = int_part.as_bytes();
    let len = bytes.len();
    let mut grouped = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*b as char);
    }
    format!("{}{}.{}", if neg { "-" } else { "" }, grouped, frac)
}

/// (#18) Bars-per-year of the series the measurement fns are currently fed: 252 (daily, the live
/// screen — the default) or 12 (the long-horizon backtest's monthly bars). The backtest sets it once
/// per run so `measure_endpoint` can convert the config's TRADING-DAYS span into the same calendar
/// span in bars — the validated smoothing window means the same amount of TIME on either cadence
/// (train == serve). ponytail: process-wide atomic, fine because one backtest run = one cadence.
static MEASURE_CADENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(252);

/// Called by the backtest before scoring cutoffs built on non-daily bars (12 = monthly).
pub fn set_measure_cadence(bars_per_year: usize) {
    MEASURE_CADENCE.store(bars_per_year, std::sync::atomic::Ordering::Relaxed);
}

/// (#17/#18) The MEASUREMENT endpoint: mean of the closes in the last `endpoint_smooth_days`
/// TRADING DAYS (config; 1 = the raw last close). The span converts to bars by the current cadence
/// (`span × cadence / 252`, min 1 — e.g. 105 trading days ≈ 5 months = 105 daily closes live, 5
/// monthly bars in the 12y backtest), so the smoothing covers the same calendar time on both sides
/// -> train == serve. Long-horizon measurements — ≥1Y perf/CAGR legs (`horizon_changes`), the range
/// position (`price_pct_rank`), the drawdown (`pct_from_high`) — read their "current price" here, so
/// ONE bad print (or one hot week) on screen day can't flip the range gate or swing a rank. The
/// DISPLAYED price and the short legs (1D/1W/1M, incl. the 1M knife) stay the true last close, and
/// the overext brake deliberately reads raw too (see `above_long_ma_pct`). Panics on empty input,
/// same as the `.last().unwrap()` it replaces.
pub fn measure_endpoint(closes: &[f64]) -> f64 {
    let cadence = MEASURE_CADENCE.load(std::sync::atomic::Ordering::Relaxed);
    endpoint_avg(closes, span_to_bars(crate::config::endpoint_smooth_days(), cadence))
}

/// Trading-days span -> bar count at `cadence` bars/year (252 = daily -> identity), min 1.
fn span_to_bars(span_days: usize, cadence: usize) -> usize {
    (span_days * cadence / 252).max(1)
}

/// Mean of the last `n` closes (n clamped to [1, len]). Split from `measure_endpoint` so the math is
/// unit-testable without the process-wide config read.
fn endpoint_avg(closes: &[f64], n: usize) -> f64 {
    let n = n.clamp(1, closes.len());
    closes[closes.len() - n..].iter().sum::<f64>() / n as f64
}

/// Percentile rank (0..100) of the LAST close within the asset's OWN fetched history: ~0 = at its
/// all-time low, ~100 = at its all-time high. Self-normalizing across assets of wildly different
/// amplitude (BTC vs a penny alt) and robust to a single blow-off top — it's a rank, not a linear
/// (max−last)/(max−min) range one spike would distort. The buy "discount" uses 100−this (how deep in
/// its own history it trades). 0 for empty/one-point history.
pub fn price_pct_rank(closes: &[f64]) -> f64 {
    if closes.len() < 2 {
        return 0.0;
    }
    let last = measure_endpoint(closes);
    let below = closes.iter().filter(|&&c| c < last).count();
    below as f64 / (closes.len() - 1) as f64 * 100.0
}

/// % latest price sits below the period high. 0 if at/above high.
pub fn pct_from_high(prices: &[f64]) -> f64 {
    let high = prices.iter().cloned().fold(f64::MIN, f64::max);
    let last = measure_endpoint(prices);
    f64::max(0.0, (high - last) / high * 100.0)
}

/// Country/market from the ticker suffix. Crypto = global; no suffix = USA.
pub fn market_of(ticker: &str) -> String {
    if ticker.contains('.') {
        let suf = ticker.rsplit('.').next().unwrap().to_uppercase();
        return suffix_country(&suf).unwrap_or(&suf).to_string();
    }
    if crate::picks::is_currency_quoted(ticker) {
        // just "Crypto": "(global)" added nothing and a tight MARKET column clipped it to "Crypto ("
        // Suffix check, NOT any dash — BRK-B/BF-B are US share classes, not coins.
        return "Crypto".to_string();
    }
    "USA".to_string()
}

/// NUPL (Bitcoin Net Unrealized Profit/Loss) market-sentiment zone. Standard bands: below 0 the
/// market is underwater (capitulation); above ~0.75 it's historically frothy (euphoria). A whole-
/// market gauge, not a per-coin signal.
pub fn nupl_zone(nupl: f64) -> &'static str {
    match nupl {
        x if x < 0.0 => "Capitulation",
        x if x < 0.25 => "Hope/Fear",
        x if x < 0.5 => "Optimism/Anxiety",
        x if x < 0.75 => "Belief/Denial",
        _ => "Euphoria/Greed",
    }
}

/// GICS sectors counted as "tech" for screen's tech-only buy table. Apple/MSFT/NVDA are
/// Information Technology; Google/Meta/Netflix are Communication Services. (Amazon & Tesla are
/// GICS Consumer Discretionary, so they DON'T appear — add that sector string here to include them.)
/// Does `haystack` pass the configured `sectors` filter? Empty filter = keep everything (the default,
/// "fetch all sectors"); otherwise a case-insensitive substring match against ANY keyword. Used on
/// BOTH a stock's GICS sector string and an ETF's fund name (funds carry no GICS), so a single
/// keyword like "Technology" catches the GICS "Information Technology" AND an ETF named
/// "...Technology...". To fetch only tech, set `sectors: [Technology, Communication, Semiconductor]`.
pub fn sector_matches(haystack: &str, sectors: &[String]) -> bool {
    sectors.is_empty()
        || sectors.iter().any(|s| haystack.to_lowercase().contains(&s.trim().to_lowercase()))
}

/// Parse one S&P-500 constituents CSV row -> (Yahoo symbol, GICS sector), keeping it only if the
/// sector passes the `sectors` filter (empty = all sectors). Columns: Symbol, Security,
/// "GICS Sector", ... — Symbol and Sector carry no commas in this dataset, but the Security NAME
/// can (quoted, e.g. `"Casey's General Stores, Inc."`), which shifts the sector one column right
/// under a naive split. The sector rides along so the screen can print the top table's sector mix.
pub fn sector_symbol(csv_line: &str, sectors: &[String]) -> Option<(String, String)> {
    let cols: Vec<&str> = csv_line.splitn(5, ',').collect();
    let sym = cols.first()?.trim();
    // ponytail: only the ONE quoted comma seen in this dataset is handled; a two-comma name would
    // need a real CSV parser — add one only if the sector mix ever prints another garbage label.
    let name = cols.get(1)?.trim();
    let shifted = name.starts_with('"') && !name.ends_with('"');
    // a 2-column list (Symbol,Name — e.g. the nasdaq-100 CSV) carries no sector: keep the row
    // under "other" instead of dropping it (a sector-restricted screen still excludes it).
    let sector = match cols.get(if shifted { 3 } else { 2 }).map(|s| s.trim()) {
        Some(s) if !s.is_empty() => s,
        _ => "other",
    };
    if sym.is_empty() || !sector_matches(sector, sectors) {
        return None;
    }
    Some((yahoo_equity_symbol(sym), sector.to_string()))
}

/// Yahoo symbol form for a constituent-CSV ticker. US class-share dots become dashes
/// (BRK.B -> BRK-B), but a recognized European venue suffix is ALREADY Yahoo form and must keep
/// its dot — blanket replacement turned every FTSE/DAX pond name (AAF.L, ADS.DE) into a dead
/// symbol that fetched nothing. US class letters (A/B/C) don't collide with this venue list.
fn yahoo_equity_symbol(sym: &str) -> String {
    const VENUES: [&str; 12] = ["L", "DE", "PA", "AS", "MI", "MC", "SW", "ST", "CO", "OL", "HE", "LS"];
    match sym.rsplit_once('.') {
        Some((_, suffix)) if VENUES.contains(&suffix) => sym.to_string(),
        _ => sym.replace('.', "-"),
    }
}

/// (Item 32) Extract (Yahoo symbol, GICS sector) rows from a Wikipedia "List of S&P N companies"
/// page (the maintained source for the MidCap 400 — no living CSV exists). Anchors on the
/// `id="constituents"` table; per row, cell 0's text = ticker, cell 2's = sector. The tag-strip is
/// a dumb <...>-skipper — plenty for wiki cells. Malformed rows are skipped; an unrecognizable
/// page yields an empty vec, so the pond just drops like a failed CSV fetch (never crashes).
pub fn wiki_constituents(html: &str, sectors: &[String]) -> Vec<(String, String)> {
    let Some((_, rest)) = html.split_once("id=\"constituents\"") else { return Vec::new() };
    let table = rest.split("</table>").next().unwrap_or("");
    let strip = |cell: &str| {
        let mut out = String::new();
        let mut in_tag = false;
        for c in cell.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.trim().to_string()
    };
    table
        .split("<tr")
        .skip(2) // fragment before the first <tr> + the header row
        .filter_map(|row| {
            let cells: Vec<String> =
                row.split("<td").skip(1).filter_map(|c| c.split_once('>')).map(|(_, body)| strip(body)).collect();
            let (sym, sector) = (cells.first()?.as_str(), cells.get(2)?.as_str());
            if sym.is_empty() || sector.is_empty() || !sector_matches(sector, sectors) {
                return None;
            }
            Some((sym.replace('.', "-"), sector.to_string()))
        })
        .collect()
}

/// Parse the Euronext Lisbon equities DataTables payload -> Yahoo `.LS` tickers. The request
/// (`fetch_euronext_lisbon`) asks for columns `name,isin,symbol,market,...`, so each `aaData` row is
/// an array with the bare symbol at index 2 (e.g. "GALP"); append `.LS` for the Yahoo form
/// ("GALP.LS"). Keeps only plain A-Z0-9 symbols (drops empty/odd cells). Empty Vec on a missing or
/// reshaped payload — the caller then degrades to an empty leg, never a crash.
pub fn euronext_lisbon_symbols(payload: &Value) -> Vec<String> {
    payload
        .get("aaData")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let sym = r.get(2)?.as_str()?.trim();
                    (!sym.is_empty() && sym.chars().all(|c| c.is_ascii_alphanumeric()))
                        .then(|| format!("{sym}.LS"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ISIN shape check (2-letter country + 9 alphanumerics + check digit) — the venue-list parsers
/// use it to drop header junk / reshaped cells without a regex dependency.
fn is_isin(s: &str) -> bool {
    s.len() == 12
        && s.chars().take(2).all(|c| c.is_ascii_uppercase())
        && s.chars().skip(2).take(9).all(|c| c.is_ascii_alphanumeric())
        && s.chars().nth(11).is_some_and(|c| c.is_ascii_digit())
}

/// Parse the SIX fund-list payload (`fqs/snap.json`, `rowData` = `[ISIN, ShortName]` arrays) ->
/// ISINs of rows that LOOK like exchange-traded funds. The FU segment mixes real ETFs with Swiss
/// mutual funds (LGT PB / Robeco share classes) and CHF-hedged clones; resolvable mutual funds
/// would be force-classified as ETFs downstream, so keep only names carrying an "etf" or "ucits"
/// token. Misses short-named clones ("X GL GOV 3D CHF") — CHF classes of strategies whose main
/// line the pond already has. Empty Vec on a missing/reshaped payload.
pub fn six_fund_isins(payload: &Value) -> Vec<String> {
    payload
        .get("rowData")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let isin = r.get(0)?.as_str()?.trim();
                    let name = r.get(1)?.as_str()?.to_lowercase();
                    (is_isin(isin) && (name.contains("etf") || name.contains("ucits")))
                        .then(|| isin.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the Euronext ETF-list ("track") DataTables payload -> ISINs. Same request shape as
/// `euronext_lisbon_symbols` (columns `name,isin,symbol,market`), but here the useful cell is the
/// ISIN at index 1 — the symbol is venue-local and useless to Yahoo, so the caller bridges
/// ISIN -> Yahoo symbol exactly like the Börse Frankfurt rows. Keeps only ISIN-shaped cells
/// (2 letters + 9 alphanumerics + check digit). Empty Vec on a missing/reshaped payload.
pub fn euronext_track_isins(payload: &Value) -> Vec<String> {
    payload
        .get("aaData")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let isin = r.get(1)?.as_str()?.trim();
                    is_isin(isin).then(|| isin.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Is this ETF name a BROAD market index (the kind you anchor a 20-year hold on), as opposed to a
/// single-sector / thematic tilt? True = carries a broad-index token AND no sector/thematic token,
/// so "S&P 500 Information Technology" (has "s&p 500" but also "information"/"technolog") is
/// correctly NOT broad, while "Vanguard S&P 500 UCITS ETF" is. Name-token heuristic — lowercased,
/// substring match, same style as the venue-list funnels above.
pub fn is_broad_index_name(name: &str) -> bool {
    let n = name.to_lowercase();
    const BROAD: [&str; 5] = ["s&p 500", "msci world", "ftse all-world", "all-country", "acwi"];
    // sector/thematic/tilt tokens that disqualify a plain broad core: single sectors (Nasdaq-100 is a
    // tech concentration), regional exclusions ("World ex USA" is a bet, not the whole market), ESG/SRI
    // screens (a filtered subset), factor tilts (value/momentum/quality/min-vol/equal-weight), and
    // currency-hedged classes (hedge-cost drag ≈ the interest-rate differential/yr — not the canonical hold).
    // "minimum vol" + " pab": live CORE receipts — funds spell the tilt out ("MSCI World Minimum
    // Volatility") or abbreviate the ESG screen ("MSCI World PAB" = Paris-Aligned Benchmark), and the
    // "min vol"/"paris" tokens miss both. " pab" keeps its leading space so a name merely containing
    // the letters (e.g. a provider string) can't false-positive.
    const NARROW: [&str; 33] = [
        "technolog", "information", "info tech", "financ", "semiconduct", "health", "energy",
        "sector", "select", "nasdaq", "small", "mid cap", "communicat", "biotech",
        "world ex", "acwi ex", "ex-usa", "esg", "sri ", "socially responsible", "screened",
        "sustainab", "paris", " pab", "climate", "islamic", "value", "momentum", "quality",
        "equal weight", "min vol", "minimum vol", "hedged",
    ];
    BROAD.iter().any(|t| n.contains(t)) && !NARROW.iter().any(|t| n.contains(t))
}

/// Diversification tier of a broad-index fund, for ranking a single buy-and-hold-forever CORE (broader
/// = one fund covers more of the world = better default). 0 = all-world / ACWI (whole planet, DM+EM),
/// 1 = MSCI World (developed only), 2 = S&P 500 (US only). Assumes `is_broad_index_name` already held.
pub fn hold_breadth_tier(name: &str) -> u8 {
    let n = name.to_lowercase();
    if n.contains("all-world") || n.contains("all-country") || n.contains("acwi") {
        0
    } else if n.contains("msci world") {
        1
    } else {
        2 // s&p 500
    }
}

/// Is this quote a genuine buy-and-hold-20yr CORE holding — independent of the momentum SCORE, which
/// ranks recent runners and buries broad index funds at 0.0? Display-only `H` flag driver: broad
/// index + cheap + physical + accumulating + large + EU-domiciled, all read from facts already on the
/// row (no fetch, no scoring). The numeric legs (Acc / Full / AUM Some) only hold on BF-fund rows, so
/// stocks/crypto and factless venue funds return false naturally.
/// The "ucits" name token gates UCITS-ness (the wrapper), while the real country now rides
/// `Quote.domicile` (ISIN prefix) and orders the CORE shortlist. Deliberately NO domicile hard gate
/// here: watchlist-only runs have `domicile: None` and missing data must not kill the flag — the
/// same stance as the AUM gate.
pub fn hold_suitable(q: &Quote) -> bool {
    hold_miss_reason(q).is_none()
}

/// (round 49) The FIRST hold-core leg this quote fails, as a printable reason — None = passes all
/// (i.e. `hold_suitable`). Single source of truth: hold_suitable IS this function's is_none(), so
/// the H flag and the printed reason can never disagree. Leg order = cheapest check first, and the
/// TER cap note lives here: 0.25 so FTSE All-World (VWCE/VWRL, 0.22%) — the canonical one-fund
/// hold — qualifies; below that is S&P/World territory (0.03–0.20%). ter_shown/aum_shown: Yahoo
/// fallback counts here (display-side flag), the score does NOT see it.
pub fn hold_miss_reason(q: &Quote) -> Option<String> {
    if !is_broad_index_name(&q.name) {
        return Some("not a broad-index name (sector/thematic/factor tilt)".into());
    }
    if !q.name.to_lowercase().contains("ucits") {
        return Some("no UCITS token in the name".into());
    }
    match q.ter_shown() {
        None => return Some("TER unknown".into()),
        Some(t) if t > 0.25 => return Some(format!("TER {t:.2}% > 0.25% cap")),
        _ => {}
    }
    // (round 53) physical FAMILY, not literal "Full": this leg exists to exclude swap counterparty
    // risk over a decades hold, but requiring Full replication structurally excluded every large
    // all-world fund — VWRA (€43B), iShares ACWI (€29B), SPDR ACWI (€14B) all sample ("Optimised",
    // the norm for a 3000+ name index; BF verified live 2026-07) — keeping the CORE tier-0 slot
    // permanently empty. Optimised/Sample/Hybrid hold the stocks; Swap and unknown still fail.
    if !matches!(q.replication, Some("Full" | "Opt" | "Samp" | "Hybr")) {
        return Some(format!("replication {} (needs physical)", q.replication.unwrap_or("unknown")));
    }
    if q.use_of_profits != Some("Acc") {
        return Some(format!("share class {} (needs Acc)", q.use_of_profits.unwrap_or("unknown")));
    }
    if !q.aum_shown().is_some_and(|a| a >= 1e9) {
        return Some(match q.aum_shown() {
            Some(a) => format!("AUM €{:.1}B < €1B floor", a / 1e9),
            None => "AUM unknown".into(),
        });
    }
    None
}

/// Pick the newest FULINS_C download link out of a FIRDS registry payload. Handles both registry
/// shapes: ESMA's Solr (`response.docs[].{file_name,download_link}`) and the FCA's Elasticsearch
/// (`hits.hits[]._source.{file_name,download_link}`). Newest = max file_name — the date is
/// embedded (`FULINS_C_YYYYMMDD_…`) so lexicographic order IS date order, no date parsing needed.
/// None on a missing/reshaped/empty payload — the caller then degrades to the cached list.
pub fn firds_latest_fulins_link(payload: &Value) -> Option<String> {
    let docs = payload
        .pointer("/response/docs")
        .or_else(|| payload.pointer("/hits/hits"))?
        .as_array()?;
    docs.iter()
        .filter_map(|d| {
            let d = d.get("_source").unwrap_or(d);
            let name = d.get("file_name")?.as_str()?;
            let link = d.get("download_link")?.as_str()?;
            name.starts_with("FULINS_C_")
                .then(|| (name.to_string(), link.to_string()))
        })
        .max()
        .map(|(_, link)| link)
}

/// Scan a FIRDS FULINS_C reference-data XML (ESMA or FCA weekly full dump) for exchange-traded
/// fund ISINs. Each `FinInstrmGnlAttrbts` record carries Id (ISIN) / FullNm / optional ShrtNm /
/// ClssfctnTp (CFI). Kept rows need ALL of: CFI class `CE*` (exchange-traded collective
/// investment vehicles — the class also covers Danish/Swiss listed mutual funds, same pollution
/// as the SIX FU segment, hence the same "etf"/"ucits" name funnel), an ETF/UCITS name token,
/// and a domicile prefix outside the non-EU blocklist (the dumps carry thousands of US/CA/Asia
/// funds traded on EU MTFs that an EU retail account can't buy — PRIIPs). Read as plain text
/// (ESMA is single-line, the FCA file pretty-printed — hence `\s*`); a real XML parser buys
/// nothing here. Sorted + deduped (an ISIN appears once per trading venue).
pub fn firds_etf_isins(xml: &str) -> Vec<String> {
    const NON_EU: [&str; 17] = [
        "US", "CA", "HK", "JP", "SG", "KY", "AU", "IL", "ZA", "TW", "KR", "IN", "TH", "MY",
        "CN", "BM", "VG",
    ];
    let re = regex::Regex::new(
        r"<FinInstrmGnlAttrbts>\s*<Id>([A-Z]{2}[0-9A-Z]{9}[0-9])</Id>\s*<FullNm>([^<]*)</FullNm>\s*(?:<ShrtNm>[^<]*</ShrtNm>\s*)?<ClssfctnTp>CE",
    )
    .unwrap();
    let mut out: Vec<String> = re
        .captures_iter(xml)
        .filter_map(|c| {
            let isin = c.get(1)?.as_str();
            let name = c.get(2)?.as_str().to_lowercase();
            ((name.contains("etf") || name.contains("ucits")) && !NON_EU.contains(&&isin[..2]))
                .then(|| isin.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Titles across yfinance/Yahoo schemas (flat `title`, nested `content.title`).
pub fn headline_titles(news_items: &[Value]) -> Vec<String> {
    let nonempty = |v: &Value, key: &str| -> Option<String> {
        v.get(key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let mut out = Vec::new();
    for n in news_items {
        let title = nonempty(n, "title")
            .or_else(|| n.get("content").and_then(|c| nonempty(c, "title")));
        if let Some(t) = title {
            out.push(t);
        }
    }
    out
}

/// Where the price comes from: the configured quote-page template with `{ticker}` filled.
pub fn source_url(template: &str, ticker: &str) -> String {
    template.replace("{ticker}", ticker)
}

/// Human-readable name from a Yahoo info/meta value; ticker if absent.
pub fn name_of(info: &Value, ticker: &str) -> String {
    let pick = |key: &str| info.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    // longName first: it carries the real fund name ("iShares Core MSCI World UCITS ETF") where the
    // shortName is a truncated registrant blob ("ISHARES III PLC ISHRS CORE MSCI"). shortName is the
    // fallback when meta omits longName (common for crypto/FX). Equities read the same either way.
    pick("longName")
        .or_else(|| pick("shortName"))
        .unwrap_or(ticker)
        .trim()
        .to_string()
}

/// CA base rate: 3-month Euribor + series spread, capped, floored at 0.
/// The permanence premium is added on top per holding year, not capped.
pub fn ca_base_rate(euribor_3m: f64, spread: f64, cap: f64) -> f64 {
    f64::max(0.0, f64::min(euribor_3m + spread, cap))
}

/// (latest_year, latest_rate, avg_last_10y, avg_last_30y) from {year->rate}.
/// Averages use however many of the last N years are present. Nones if empty.
pub fn inflation_summary(
    series: &BTreeMap<i32, f64>,
) -> (Option<i32>, Option<f64>, Option<f64>, Option<f64>) {
    if series.is_empty() {
        return (None, None, None, None);
    }
    let years: Vec<i32> = series.keys().cloned().collect(); // BTreeMap keys are sorted
    let avg = |n: usize| -> f64 {
        let tail = &years[years.len().saturating_sub(n)..];
        tail.iter().map(|y| series[y]).sum::<f64>() / tail.len() as f64
    };
    let last = *years.last().expect("years non-empty: series.is_empty() guarded above");
    (Some(last), Some(series[&last]), Some(avg(10)), Some(avg(30)))
}

/// Cumulative price rise over the last `years` years, compounding each year's annual CPI
/// rate (the "true" erosion: +3%/yr for 10y ≈ +34%, not +30%). `None` when the series can't
/// reasonably cover the horizon — so we don't pass a much shorter span off as the full one (that's
/// what made the keyless 10y US window report an identical 10Y and 20Y). One year of slack is
/// allowed: a level→YoY series ALWAYS loses its earliest in-window year (no prior-year base to
/// divide by), so a true N-year horizon yields N−1 rates at best; n/a only kicks in at ≥2 short.
pub fn inflation_compounded(series: &BTreeMap<i32, f64>, years: usize) -> Option<f64> {
    if series.len() + 1 < years {
        return None;
    }
    let vals: Vec<f64> = series.values().cloned().collect(); // BTreeMap -> year-ascending
    let tail = &vals[vals.len().saturating_sub(years)..]; // saturating: the 1yr-slack case has years == len+1
    let factor = tail.iter().fold(1.0, |f, r| f * (1.0 + r / 100.0));
    Some((factor - 1.0) * 100.0)
}

/// Parse the BLS public API (v1) CPI-U response into {year -> annual %}. The series is the
/// index LEVEL (e.g. CUUR0000SA0) by month, so convert to a rate: for each year, the rate is
/// (its latest month with a prior-year same-month) / (that prior-year value) − 1. A complete
/// year resolves to Dec-over-Dec; the current partial year to its newest month YoY — matching
/// how the EU/PT series use "last month of the year". Empty on a malformed payload.
/// Called once per POST year-window by `fetch_us_inflation`, results merged — so it only needs
/// each year's predecessor present within the window it's handed (windows overlap by 1 year).
pub fn parse_bls_cpi(d: &Value) -> BTreeMap<i32, f64> {
    let mut idx: BTreeMap<(i32, u32), f64> = BTreeMap::new(); // (year, month) -> index level
    let rows = d.pointer("/Results/series/0/data").and_then(|v| v.as_array());
    for r in rows.into_iter().flatten() {
        let year = r.get("year").and_then(|v| v.as_str()).and_then(|s| s.parse::<i32>().ok());
        let value = r.get("value").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
        let month = r
            .get("period")
            .and_then(|v| v.as_str())
            .and_then(|p| p.strip_prefix('M'))
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|m| (1..=12).contains(m)); // skip M13 (annual average)
        if let (Some(y), Some(m), Some(v)) = (year, month, value) {
            idx.insert((y, m), v);
        }
    }
    let mut out = BTreeMap::new();
    for (&(y, m), &v) in &idx {
        // BTreeMap is key-sorted -> larger m for a year is seen later -> latest month wins
        if let Some(&prev) = idx.get(&(y - 1, m)).filter(|&&p| p > 0.0) {
            out.insert(y, (v / prev - 1.0) * 100.0);
        }
    }
    out
}

/// Parse a BPstat-style JSON-stat payload into {year -> rate}, last month of each year
/// winning. The date index may be a JSON **array** of date strings (BPstat's actual
/// shape — already chronological and parallel to `value`) OR a `{date: position}` object;
/// `value` is an array. Handling only the object form was the Portugal-inflation bug.
pub fn parse_pt_series(d: &Value) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let Some(idx) = d.pointer("/dimension/reference_date/category/index") else {
        return out;
    };
    let Some(values) = d.get("value").and_then(|v| v.as_array()) else {
        return out;
    };
    let dates: Vec<&str> = if let Some(arr) = idx.as_array() {
        arr.iter().filter_map(|v| v.as_str()).collect()
    } else if let Some(obj) = idx.as_object() {
        let mut pairs: Vec<(&str, i64)> =
            obj.iter().filter_map(|(k, v)| Some((k.as_str(), v.as_i64()?))).collect();
        pairs.sort_by_key(|(_, p)| *p); // chronological
        pairs.into_iter().map(|(k, _)| k).collect()
    } else {
        return out;
    };
    for (date, v) in dates.iter().zip(values) {
        if date.len() >= 4 {
            if let (Ok(year), Some(rate)) = (date[..4].parse::<i32>(), v.as_f64()) {
                out.insert(year, rate); // ascending -> later month of a year wins
            }
        }
    }
    out
}

/// Closes whose date >= last_date - days (ascending input). [] if empty.
pub fn slice_since(dates: &[NaiveDate], closes: &[f64], days: i64) -> Vec<f64> {
    if dates.is_empty() {
        return Vec::new();
    }
    let cutoff = *dates.last().expect("dates non-empty: dates.is_empty() guarded above") - Duration::days(days);
    dates.iter()
        .zip(closes)
        .filter(|(d, _)| **d >= cutoff)
        .map(|(_, c)| *c)
        .collect()
}

/// Last close with date <= target (ascending input). None if before history.
pub fn asof(dates: &[NaiveDate], closes: &[f64], target: NaiveDate) -> Option<f64> {
    let mut res = None;
    for (d, c) in dates.iter().zip(closes) {
        if *d <= target {
            res = Some(*c);
        } else {
            break;
        }
    }
    res
}

/// Average close within ±`half` days of `target` — a smoothed anchor so one outlier day (a spike
/// or gap landing exactly on the horizon date) doesn't skew a long-horizon % change. None if no
/// close falls in the window.
pub fn asof_avg(dates: &[NaiveDate], closes: &[f64], target: NaiveDate, half: i64) -> Option<f64> {
    let (lo, hi) = (target - Duration::days(half), target + Duration::days(half));
    let vals: Vec<f64> =
        dates.iter().zip(closes).filter(|(d, _)| **d >= lo && **d <= hi).map(|(_, c)| *c).collect();
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Extend a young listing's series with a configured older twin's history (`history_proxy`):
/// rebase the proxy so its close as-of the listing's first bar equals the listing's first close,
/// prepend only proxy bars strictly BEFORE that first bar, then the listing's own series
/// unchanged. None when the proxy doesn't overlap the listing's start or a rebase anchor is
/// non-positive — a splice with no common bar would fabricate a level jump.
pub fn splice_history(
    own_dates: &[NaiveDate],
    own_closes: &[f64],
    proxy_dates: &[NaiveDate],
    proxy_closes: &[f64],
) -> Option<(Vec<NaiveDate>, Vec<f64>)> {
    let (&own_first_date, &own_first_close) = (own_dates.first()?, own_closes.first()?);
    let proxy_at_start = asof(proxy_dates, proxy_closes, own_first_date)?;
    if own_first_close <= 0.0 || proxy_at_start <= 0.0 {
        return None;
    }
    let factor = own_first_close / proxy_at_start;
    let keep = proxy_dates.iter().take_while(|d| **d < own_first_date).count();
    if keep == 0 {
        return None; // proxy adds nothing older -> caller keeps the plain series
    }
    let mut dates = proxy_dates[..keep].to_vec();
    let mut closes: Vec<f64> = proxy_closes[..keep].iter().map(|c| c * factor).collect();
    dates.extend_from_slice(own_dates);
    closes.extend_from_slice(own_closes);
    Some((dates, closes))
}

/// Built-in ±days averaging window for a horizon, by its calendar length. Smoothing the anchor
/// hides a single outlier day; the further back the horizon, the wider the window. 1D = exact (a
/// 1-day move is a single point). Overridable per-label in settings.yaml `anchor_windows`.
pub fn default_anchor_half(days: i64) -> i64 {
    match days {
        d if d >= 1825 => 365, // 5Y/10Y/20Y: ±12 months
        d if d >= 182 => 90,   // 6M/1Y: ±3 months
        d if d >= 30 => 30,    // 1M: ±30 days
        d if d >= 7 => 7,      // 1W: ±7 days
        _ => 0,                // 1D: exact day
    }
}

/// A nominal % gain converted to a REAL (inflation-adjusted) one: +50% over a span that saw +10%
/// cumulative inflation is only ~+36% in purchasing power. `cum_infl_pct` = cumulative inflation %
/// over the same span. real = (1+nominal) / (1+infl) − 1.
pub fn real_pct(nominal_pct: f64, cum_infl_pct: f64) -> f64 {
    ((1.0 + nominal_pct / 100.0) / (1.0 + cum_infl_pct / 100.0) - 1.0) * 100.0
}

/// (past_price_eur_str, pct_change) or None for each HORIZON, in HORIZONS order. `windows` maps a
/// horizon label to a ±days averaging window, overriding `default_anchor_half`; missing = default.
/// `infl` = Some(year->YoY% series, e.g. EU HICP) to show inflation-adjusted returns on horizons
/// >=1Y (deflated by the real cumulative inflation over each horizon), or None for raw nominal %.
pub fn horizon_changes(dates: &[NaiveDate], closes: &[f64], rate: Option<f64>, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Vec<Option<(String, f64)>> {
    // (#18) two endpoints: the LONG legs (>=1Y — the CAGR/rank inputs) use the smoothed measurement
    // endpoint; the SHORT legs (1D/1W/1M, incl. the 1M-knife gate) keep the true last close — a
    // months-wide average would make "this month's move" meaningless as both a gate and a display.
    let cur_smooth = measure_endpoint(closes);
    let cur_raw = *closes.last().expect("closes non-empty: callers pass a fetched chart (quote_one guards !closes.is_empty(), fetch.rs)");
    let last = *dates.last().expect("dates non-empty: parallel to closes (same fetched chart)");
    HORIZONS
        .iter()
        .map(|(label, days)| {
            let cur = if *days >= 365 { cur_smooth } else { cur_raw };
            let target = last - Duration::days(*days);
            let half = windows.get(*label).copied().unwrap_or_else(|| default_anchor_half(*days));
            let past = if half > 0 {
                asof_avg(dates, closes, target, half).or_else(|| asof(dates, closes, target))
            } else {
                asof(dates, closes, target)
            };
            match past {
                None => None,
                Some(0.0) => None,
                Some(p) => {
                    let eur = match rate {
                        Some(r) => format!("€{}", fmt_money2(p * r)),
                        None => format!("{}?", fmt_money2(p)),
                    };
                    let mut pct = (cur - p) / p * 100.0;
                    // inflation-adjust the longer horizons only (>=1Y); short ones are noise. Deflate
                    // by the ACTUAL cumulative inflation over that many years (compounded YoY series).
                    if let Some(series) = infl {
                        if *days >= 365 {
                            if let Some(cum) = inflation_compounded(series, (*days / 365) as usize) {
                                pct = real_pct(pct, cum);
                            }
                        }
                    }
                    Some((eur, pct))
                }
            }
        })
        .collect()
}

/// % change vs `bars` hourly bars ago (index-based, NOT wall-clock). Counting trading bars
/// ignores overnight/weekend gaps, so it always resolves when enough bars exist — for a stock
/// "N bars ago" is the last N *trading* hours (may span a close); for 24/7 crypto it's real
/// wall-clock hours. None if fewer than `bars`+1 closes. A ratio, so FX-agnostic.
pub fn intraday_pct(closes: &[f64], bars: usize) -> Option<f64> {
    let len = closes.len();
    if len <= bars {
        return None;
    }
    let cur = *closes.last()?;
    let past = closes[len - 1 - bars];
    if past == 0.0 {
        return None;
    }
    Some((cur - past) / past * 100.0)
}

/// Pearson correlation of two equal-length series. None if <2 points, length mismatch, or either
/// series has zero variance (a flat series has no correlation to anything).
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return None;
    }
    let nf = n as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / nf, ys.iter().sum::<f64>() / nf);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        let (dx, dy) = (x - mx, y - my);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return None;
    }
    Some(sxy / (sxx * syy).sqrt())
}

/// Spearman rank correlation = Pearson on the fractional ranks. Robust to the wild magnitude outliers
/// a few crypto names inject (it measures monotone agreement, not size). None if <2 points.
pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    pearson(&ranks(xs), &ranks(ys))
}

/// Fractional ranks (1-based; tied values share the average of their ranks), in original order.
/// note: O(n log n) sort + linear tie-merge; fine for the backtest's handful-to-hundreds of names.
fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut r = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1; // group of ties [i..=j]
        }
        let avg = (i + j) as f64 / 2.0 + 1.0; // mean of the 1-based ranks (i+1)..=(j+1)
        for k in i..=j {
            r[idx[k]] = avg;
        }
        i = j + 1;
    }
    r
}

/// One filed fundamentals statement, point-in-time. `filed` = FMP `filingDate` (when the numbers
/// became PUBLIC), deliberately NOT the period-end `date` — joining on period-end would let the
/// backtest read a quarter before it was reported (look-ahead bias). Every ratio is Option: a factor
/// the free tier can't source (roic/debt = premium-gated) stays None and scores NEUTRAL, never zero.
/// revenue/margins/eps come from the free `stable/income-statement`; the rest await a paid tier.
#[derive(Clone, Debug, Default)]
pub struct FundRow {
    pub filed: NaiveDate,
    pub period_end: NaiveDate,         // FMP period-end `date` — DISPLAY-ONLY (groups quarters by fiscal year in `report`). The as-of join (`fund_as_of`) keys on `filed`, never this, so it can't leak look-ahead into the backtest
    pub revenue: Option<f64>,
    pub gross_margin: Option<f64>,    // % = grossProfit/revenue
    pub op_margin: Option<f64>,       // % = operatingIncome/revenue
    pub net_margin: Option<f64>,      // % = netIncome/revenue
    pub eps: Option<f64>,
    pub shares: Option<f64>,          // diluted weighted-avg shares outstanding — DISPLAY-ONLY (buyback column); None on the free tier / when the source omits it
    pub roe: Option<f64>,             // % — PREMIUM (key-metrics/ratios), None on free tier
    pub roic: Option<f64>,            // % — PREMIUM
    pub net_debt_ebitda: Option<f64>, // ratio, lower=safer — PREMIUM
    pub fcf_ps: Option<f64>,          // free cash flow / share — PREMIUM
    // (round 107) SURVIVAL levels, SEC-computed per 10-K (None on FMP free tier). All oriented
    // high = safer so factor ranks and reject-bottom gates read one direction.
    pub fcf_margin: Option<f64>,     // % = (op cash flow − capex) / revenue; negative = burning cash
    pub interest_cover: Option<f64>, // × = operating income / interest expense; low = one bad year from distress. None when no interest expense filed (debt-free reads NEUTRAL, not great)
    pub net_cash_rev: Option<f64>,   // % = (cash − debt) / revenue; negative = levered. Revenue-scaled (not EBITDA) so loss-makers stay defined instead of None-ing out of the gate
    // (EV/EBITDA probe) raw as-of LEVELS for the enterprise-value valuation factor. SEC-computed, None on
    // the FMP free tier. ebitda = operating income + D&A (BOTH required — a partial is garbage, so None if
    // either is missing). net_debt = total debt − cash (cash the anchor, missing debt reads 0 like
    // net_cash_rev). Combined with the as-of PRICE into ebitda_yield in the backtest loop (price-dependent,
    // exactly like earnings_yield — so no live currency skew).
    pub ebitda: Option<f64>,
    pub net_debt: Option<f64>,
}

/// As-of (point-in-time) join: the latest statement that was already FILED on or before `cutoff`.
/// THE look-ahead guard for the fundamentals backtest — at a given cutoff a strategy could only have
/// seen filings public by then. `None` if nothing was filed yet. O(n), order-independent (FMP returns
/// newest-first; don't assume it). Compose it twice (cutoff and cutoff−Ny) to get as-of growth/trend.
pub fn fund_as_of(rows: &[FundRow], cutoff: NaiveDate) -> Option<&FundRow> {
    rows.iter().filter(|r| r.filed <= cutoff).max_by_key(|r| r.filed)
}

/// As-of fundamental factors derived from filed statements at a cutoff — the backtest's fundamental
/// lane scores each STANDALONE against the forward return. All Option: None when the as-of history is
/// too short to span the lookback, or the source field is premium-gated (roic/debt never populate on
/// the free tier). Growth in %/yr, margins in %, trend/accel in points.
#[derive(Clone, Debug, Default)]
pub struct FundFactors {
    pub rev_cagr: Option<f64>,     // revenue CAGR over the lookback (proven top-line compounding)
    pub rev_accel: Option<f64>,    // last-1y revenue growth minus that long CAGR (top-line accelerating)
    pub gross_margin: Option<f64>, // current gross margin level (pricing power / moat)
    pub op_margin: Option<f64>,    // current operating margin level (operating efficiency)
    pub margin_trend: Option<f64>, // op-margin now minus ~1y ago (margin expanding = strengthening)
    pub eps_growth: Option<f64>,   // EPS CAGR over the lookback (bottom-line compounding; both ends must be +)
    pub roe: Option<f64>,          // as-of return-on-equity level, % (quality of capital). SEC feed computes it per row (NetIncome ÷ StockholdersEquity); FMP free tier leaves it None
    pub insider_net_buys_90d: Option<f64>, // (Item 4) open-market buys minus sales (Form 4 P−S) in the 90d before the cutoff; populated only under `backtest … insider`, derived in the backtest loop (not here — needs SEC, not FMP)
    pub eps_ttm: Option<f64>,      // (Item 19) the as-of EPS level (not a growth) — the numerator for earnings_yield
    pub earnings_yield: Option<f64>, // (Item 19) EPS ÷ as-of price, % (valuation level, high = cheap). PROBE-ONLY: set in the backtest loop from the native as-of close; left None by the live path (currency skew — see `earnings_yield` fn)
    // (EV/EBITDA probe) capital-structure-neutral value cousin of earnings_yield. The three as-of LEVELS
    // are price-free (set here from the latest filed row); ebitda_yield itself is EBITDA ÷ enterprise value
    // (EV = shares·price + net_debt), so it needs the as-of price -> filled in the backtest loop like
    // earnings_yield, left None by the live path. Distinct from earnings_yield because EV folds in leverage
    // (the one axis EPS/price misses).
    pub ebitda_ttm: Option<f64>,     // (EV/EBITDA) as-of EBITDA level = operating income + D&A
    pub shares_ttm: Option<f64>,     // (EV/EBITDA) as-of diluted share count — the market-cap leg of EV
    pub net_debt: Option<f64>,       // (EV/EBITDA) as-of net debt (total debt − cash) — the leverage leg of EV
    pub ebitda_yield: Option<f64>,   // (EV/EBITDA) EBITDA ÷ EV, % (high = cheap). PROBE-ONLY, None live (price skew, same as earnings_yield)
    pub peg_yield: Option<f64>,      // (PEG) 1/PEG = earnings_yield · as-of CAGR (high = cheap-for-its-growth). PROBE-ONLY, None live (needs the native as-of close, same skew as earnings_yield)
    pub buyback_yield: Option<f64>, // as-of net share-count change over the last year, sign-flipped (+ = shrinking share count = buying back). Fully as-of from the FundRows (no price needed), unlike earnings_yield — so it populates in both the backtest AND the live enrich
    // (round 107) as-of SURVIVAL levels straight off the latest filed row (like op_margin/roe) —
    // price-free, so they populate in both the backtest and the live enrich. High = safer.
    pub fcf_margin: Option<f64>,     // % (op cash flow − capex) / revenue
    pub interest_cover: Option<f64>, // × operating income / interest expense
    pub net_cash_rev: Option<f64>,   // % (cash − debt) / revenue
    // (round 109) the cyclical detector: NEGATED sample stddev of net_margin across the as-of
    // lookback rows (higher = stabler). Margin LEVEL and 1y TREND are swept elsewhere; the
    // dispersion is what a peak-cycle name (fertilizer, refiner) hides behind a good level.
    pub margin_stability: Option<f64>,
}

/// (Item 4) One open-market insider transaction parsed from an SEC Form 4: the transaction date (the
/// look-ahead guard) and its direction. Only `P` (purchase) and `S` (sale) are kept — option grants,
/// gifts, tax-withholding (codes A/G/F/M…) are noise for a "conviction buying" signal.
#[derive(Clone, Copy, Debug)]
pub struct InsiderTx {
    pub date: NaiveDate,
    pub buy: bool, // true = open-market purchase (P), false = open-market sale (S)
}

/// (Item 4) Net open-market insider conviction in the `window_days` BEFORE `cutoff`: (#buys − #sales),
/// counting each `InsiderTx` ±1. The transaction date is the look-ahead guard — a filing dated on/after
/// the cutoff can't leak in. None when no transaction falls in the window (no coverage -> the factor stays
/// neutral, never a fabricated 0). Pure -> unit-tested without touching SEC.
pub fn insider_net_buys(txns: &[InsiderTx], cutoff: NaiveDate, window_days: i64) -> Option<f64> {
    let start = cutoff - Duration::days(window_days);
    let net: i64 = txns
        .iter()
        .filter(|t| t.date >= start && t.date < cutoff)
        .map(|t| if t.buy { 1 } else { -1 })
        .sum();
    let any = txns.iter().any(|t| t.date >= start && t.date < cutoff);
    any.then_some(net as f64)
}

/// (Item 3) A per-name blend of the available as-of factors for the `"composite"` `growth_fund_factor`.
/// ponytail: a plain mean of the factors present — they're all growth-%/points of similar magnitude, so
/// averaging is a defensible first cut. CEILING: a true cross-sectional rank-normalisation (0..1 across
/// the cutoff's universe) would be scale-clean, but `select_fund_factor` sees ONE name with no peer
/// context; lift it to a universe rank in the backtest layer IF the sweep shows the composite earns its
/// place. None until ≥2 factors are present (a 1-factor "composite" IS that factor — route it directly).
fn composite_factor(f: &FundFactors) -> Option<f64> {
    let vals: Vec<f64> =
        [f.rev_cagr, f.rev_accel, f.gross_margin, f.op_margin, f.margin_trend, f.eps_growth].into_iter().flatten().collect();
    (vals.len() >= 2).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Derive the as-of fundamental factors at `cutoff` from filed statements, looking back ~`yrs`. Every
/// read goes through `fund_as_of` so NOTHING after the cutoff's filing leaks in (look-ahead guard). A
/// growth needs a positive base to be meaningful, so a non-positive denominator -> None, never a
/// garbage ratio. note: EPS CAGR only when both endpoints are positive (a sign flip isn't a CAGR).
pub fn fund_factors(rows: &[FundRow], cutoff: NaiveDate, yrs: i64) -> FundFactors {
    let now = fund_as_of(rows, cutoff);
    let long_ago = fund_as_of(rows, cutoff - Duration::days(yrs * 365));
    let yr_ago = fund_as_of(rows, cutoff - Duration::days(365));
    let grow = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) if b > 0.0 => Some((a / b - 1.0) * 100.0),
        _ => None,
    };
    let rev_cagr = grow(now.and_then(|r| r.revenue), long_ago.and_then(|r| r.revenue)).map(|c| cagr(c, yrs as f64));
    let rev_1y = grow(now.and_then(|r| r.revenue), yr_ago.and_then(|r| r.revenue));
    let rev_accel = match (rev_1y, rev_cagr) {
        (Some(a), Some(c)) => Some(a - c),
        _ => None,
    };
    let margin_trend = match (now.and_then(|r| r.op_margin), yr_ago.and_then(|r| r.op_margin)) {
        (Some(a), Some(b)) => Some(a - b),
        _ => None,
    };
    let eps_growth = match (now.and_then(|r| r.eps), long_ago.and_then(|r| r.eps)) {
        (Some(a), Some(b)) if a > 0.0 && b > 0.0 => Some(cagr((a / b - 1.0) * 100.0, yrs as f64)),
        _ => None,
    };
    // as-of buyback yield: 1y share-count change, sign-flipped (shares shrank -> positive = buying back).
    // Same |Δ|>40% split/M&A guard as income_snapshot. Needs only the FundRows -> fully as-of (no price).
    let buyback_yield = match (now.and_then(|r| r.shares), yr_ago.and_then(|r| r.shares)) {
        (Some(a), Some(b)) if b > 0.0 => {
            let d = (a / b - 1.0) * 100.0;
            (d.abs() <= 40.0).then_some(-d)
        }
        _ => None,
    };
    // (round 109) margin stability: negated sample stddev of net_margin over the last `yrs`+1 as-of
    // rows, oldest-first so the take() grabs the most RECENT filings. ≥3 values required (2 points =
    // a line, not a dispersion). CAVEAT: FMP rows are QUARTERLY — seasonality inflates the std; the
    // validated lane (fund_source sec) files one annual row per year, which is what the sweep grades.
    let margin_stability = {
        let mut ms: Vec<(NaiveDate, f64)> =
            rows.iter().filter(|r| r.filed <= cutoff).filter_map(|r| r.net_margin.map(|m| (r.period_end, m))).collect();
        ms.sort_by_key(|(e, _)| *e);
        let vals: Vec<f64> = ms.iter().rev().take(yrs as usize + 1).map(|(_, m)| *m).collect();
        (vals.len() >= 3).then(|| {
            let n = vals.len() as f64;
            let mean = vals.iter().sum::<f64>() / n;
            -(vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
        })
    };
    FundFactors {
        rev_cagr,
        rev_accel,
        gross_margin: now.and_then(|r| r.gross_margin),
        op_margin: now.and_then(|r| r.op_margin),
        margin_trend,
        eps_growth,
        roe: now.and_then(|r| r.roe), // as-of level through fund_as_of, same look-ahead guard as the margins
        insider_net_buys_90d: None, // (Item 4) SEC-sourced, set in the backtest loop, not from FMP rows
        eps_ttm: now.and_then(|r| r.eps), // (Item 19) as-of EPS level; earnings_yield needs price, set by caller
        earnings_yield: None,             // (Item 19) needs the as-of price -> filled in the backtest loop, not here
        // (EV/EBITDA) as-of levels through the same fund_as_of guard; ebitda_yield needs price -> caller fills
        ebitda_ttm: now.and_then(|r| r.ebitda),
        shares_ttm: now.and_then(|r| r.shares),
        net_debt: now.and_then(|r| r.net_debt),
        ebitda_yield: None,
        peg_yield: None, // (PEG probe) needs the as-of price AND CAGR -> filled in the backtest loop, not here
        buyback_yield,
        // (round 107) survival levels: same as-of join as the margins, no derivation needed
        fcf_margin: now.and_then(|r| r.fcf_margin),
        interest_cover: now.and_then(|r| r.interest_cover),
        net_cash_rev: now.and_then(|r| r.net_cash_rev),
        margin_stability,
    }
}

/// (Item 19) As-of earnings yield = EPS ÷ price, in % — a VALIDATABLE valuation level (high = cheap), the
/// honest counterpart to the live-only `pe_ratio` damp (which is backtest-blind). PROBE-ONLY: computed in
/// the backtest from the native as-of close + native EPS (same currency, so the ratio is clean). NOT wired
/// into the live screen — live `price_eur` is EUR while FMP EPS is USD, so a live computation would be a
/// currency train-serve skew (the Item 16 trap); wire live only once a native live price exists AND the
/// backtest probe shows both OOS halves +. None on a missing EPS or a non-positive price (no div-by-zero,
/// no garbage ratio).
pub fn earnings_yield(eps: Option<f64>, price: f64) -> Option<f64> {
    match eps {
        Some(e) if price > 0.0 => Some(e / price * 100.0),
        _ => None,
    }
}

/// (EV/EBITDA probe) As-of EBITDA yield = EBITDA ÷ enterprise value, in % — the capital-structure-neutral
/// cousin of `earnings_yield`. EV = market cap + net debt = shares·price + net_debt. PROBE-ONLY, same
/// currency discipline as earnings_yield: computed in the backtest from the native as-of close + native
/// SEC levels (clean ratio), left None by the live path (EUR price vs USD levels would skew). None unless
/// EBITDA is POSITIVE (EV/EBITDA is meaningless for a loss-maker — a negative multiple isn't "cheap", so
/// it None-outs rather than fabricating a signal), shares are positive, and EV ends up positive.
/// ponytail: net_debt None (rare — cash is the SEC anchor) degrades EV to market-cap only; the leverage
/// leg simply drops for that name. Tighten to require net_debt only if the probe shows an edge worth it.
pub fn ev_ebitda_yield(ebitda: Option<f64>, shares: Option<f64>, net_debt: Option<f64>, price: f64) -> Option<f64> {
    match (ebitda, shares) {
        (Some(e), Some(sh)) if e > 0.0 && sh > 0.0 && price > 0.0 => {
            let ev = sh * price + net_debt.unwrap_or(0.0);
            (ev > 0.0).then_some(e / ev * 100.0)
        }
        _ => None,
    }
}

/// (PEG probe) 1/PEG as a higher-is-better "yield" so it slots into the same sweep as earnings_yield:
/// PEG = (P/E) ÷ CAGR, so 1/PEG = (eps/price)·CAGR = `earnings_yield` · CAGR (unit-consistent with the
/// textbook PEG, where growth is the % NUMBER). PEG < 1 ⇔ this > (100·earnings_yield form)… i.e. higher =
/// cheaper for its growth. PROBE-ONLY, same native-close discipline as earnings_yield (None on the live
/// path). None unless earnings_yield is POSITIVE (a loss-maker isn't "cheap for growth" — no fabricated
/// signal) AND CAGR is positive (negative growth makes PEG sign-nonsense). Deliberately mirrors the
/// EV/EBITDA loss-maker None-out.
pub fn peg_yield(eps: Option<f64>, cagr: Option<f64>, price: f64) -> Option<f64> {
    let ey = earnings_yield(eps, price).filter(|&y| y > 0.0)?; // eps>0 (earnings_yield itself allows eps<0)
    let g = cagr.filter(|&g| g > 0.0)?;                        // %/yr; negative growth -> PEG meaningless
    Some(ey * g)
}

/// One fiscal year of an income statement, rolled up from the quarterly `FundRow`s — the `report`
/// command's display row. Margins are %, revenue/eps in native units. `quarters` < 4 = an incomplete
/// fiscal year (most-recent partial, or a non-December fiscal-year-end straddling the calendar split);
/// the print layer flags it so a half-year isn't misread as a revenue cliff.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnualReport {
    pub year: i32,
    pub revenue: f64,
    pub gross_margin: Option<f64>,
    pub op_margin: Option<f64>,
    pub net_margin: Option<f64>,
    pub eps: Option<f64>,
    pub shares: Option<f64>,          // diluted weighted-avg shares outstanding for the FY (mean of the year's rows) — feeds the buyback column
    pub quarters: usize,
}

/// Roll the quarterly `FundRow`s up to one row per fiscal year for the `report` view. Newest year
/// first. Annual revenue = Σ quarter revenue; annual EPS = Σ quarter eps (None if no quarter reports
/// it); each annual margin = the REVENUE-WEIGHTED mean of the quarter margins, which equals
/// Σ(profit)/Σ(revenue) exactly (a quarter's margin% is profit/revenue), so no absolute profit line is
/// needed. A quarter missing a margin (or revenue) just drops out of that margin's weighting, never
/// fabricating a 0. ponytail: groups by `period_end.year()` — a non-Dec fiscal year can straddle the
/// calendar split; the `quarters` count exposes it, true fiscal-period grouping deferred until it bites.
pub fn annual_rollup(rows: &[FundRow]) -> Vec<AnnualReport> {
    let mut by_year: BTreeMap<i32, Vec<&FundRow>> = BTreeMap::new();
    for r in rows {
        by_year.entry(r.period_end.year()).or_default().push(r);
    }
    by_year
        .into_iter()
        .rev() // newest year first
        .map(|(year, qs)| {
            let revenue: f64 = qs.iter().filter_map(|r| r.revenue).sum();
            // revenue-weighted margin: Σ(margin·rev)/Σ(rev) over quarters that carry BOTH
            let wmargin = |pick: fn(&FundRow) -> Option<f64>| {
                let (num, den) = qs.iter().copied().fold((0.0, 0.0), |(n, d), r| match (pick(r), r.revenue) {
                    (Some(m), Some(rev)) => (n + m * rev, d + rev),
                    _ => (n, d),
                });
                (den > 0.0).then(|| num / den)
            };
            let eps_vals: Vec<f64> = qs.iter().filter_map(|r| r.eps).collect();
            // shares is a LEVEL, not a flow: MEAN the year's rows (don't sum). SEC gives 1 annual row/yr
            // -> mean = that value; FMP gives ~4 quarters of per-quarter weighted-avg diluted -> their mean
            // approximates the annual weighted-avg diluted share count. Good enough for a display column.
            let share_vals: Vec<f64> = qs.iter().filter_map(|r| r.shares).collect();
            AnnualReport {
                year,
                revenue,
                gross_margin: wmargin(|r| r.gross_margin),
                op_margin: wmargin(|r| r.op_margin),
                net_margin: wmargin(|r| r.net_margin),
                eps: (!eps_vals.is_empty()).then(|| eps_vals.iter().sum::<f64>()),
                shares: (!share_vals.is_empty()).then(|| share_vals.iter().sum::<f64>() / share_vals.len() as f64),
                quarters: qs.len(),
            }
        })
        .collect()
}

/// The screen table's income-statement snapshot: (rev_yoy %, eps_yoy %, net_margin %, buyback %) of the
/// newest COMPLETE fiscal year, each vs the next-older year — the same math the `report` rows print, so
/// the two views can't disagree. "Complete" mirrors report's `*` mark: 1 quarter = an annual filing (SEC
/// rolls a fiscal year into one row), 4+ = a full quarterly year; 2-3 = genuinely partial, skipped so
/// a half-year isn't misread as a revenue cliff. YoY needs the older row too: last year in the data
/// has nothing to compare against -> that component is None, never 0. `buyback` is the net share-count
/// change sign-flipped (shares SHRANK -> positive = buying back = tax-deferred capital return).
pub fn income_snapshot(annual: &[AnnualReport]) -> Option<(Option<f64>, Option<f64>, Option<f64>, Option<f64>)> {
    let idx = annual.iter().position(|a| a.quarters == 1 || a.quarters >= 4)?;
    let a = &annual[idx];
    let older = annual.get(idx + 1);
    let rev_yoy = older.filter(|o| o.revenue > 0.0).map(|o| (a.revenue / o.revenue - 1.0) * 100.0);
    let eps_yoy = match (a.eps, older.and_then(|o| o.eps)) {
        (Some(c), Some(p)) if p != 0.0 => Some((c / p - 1.0) * 100.0),
        _ => None,
    };
    let buyback = match (a.shares, older.and_then(|o| o.shares)) {
        (Some(c), Some(p)) if p > 0.0 => {
            let shares_yoy = (c / p - 1.0) * 100.0;
            // ponytail: as-reported shares aren't split-adjusted; |Δ|>40%/yr is a split or M&A/secondary,
            // never an organic buyback -> None (matches the repo's existing as-reported tolerance on eps_yoy).
            (shares_yoy.abs() <= 40.0).then_some(-shares_yoy)
        }
        _ => None,
    };
    Some((rev_yoy, eps_yoy, a.net_margin, buyback))
}

/// Compact number for money-scale displays: 2.34T / 391.0B / 25.6M / 1.5K, plain below. Tier on |v|,
/// sign kept. Shared by `report`'s annual table and the screen fundamentals footer.
pub(crate) fn humanize(v: f64) -> String {
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

/// (B) One-line fundamentals trajectory for screen's footer: the newest ≤5 COMPLETE fiscal years
/// (income_snapshot's completeness rule) as an oldest→newest revenue chain, plus the net-margin move
/// and EPS CAGR over the same window. The human "is the growth real, or one good year?" view —
/// DISPLAY-ONLY: every multi-year fundamental measured null as a rank input (fund-lane audit).
/// None with <2 complete years (no trajectory to show). EPS CAGR only when both endpoints are
/// profitable (a loss endpoint makes the ratio meaningless).
pub fn annual_brief(annual: &[AnnualReport]) -> Option<String> {
    let mut years: Vec<&AnnualReport> =
        annual.iter().filter(|a| a.quarters == 1 || a.quarters >= 4).take(5).collect();
    if years.len() < 2 {
        return None;
    }
    years.reverse(); // rollup is newest-first; a trajectory reads oldest→newest
    let (first, last, n) = (years[0], years[years.len() - 1], years.len());
    let chain = years.iter().map(|a| humanize(a.revenue)).collect::<Vec<_>>().join("→");
    let mut out = format!("rev {n}y {chain}");
    if first.revenue > 0.0 && last.revenue > 0.0 {
        let cagr = ((last.revenue / first.revenue).powf(1.0 / (n - 1) as f64) - 1.0) * 100.0;
        out.push_str(&format!(" ({cagr:+.0}%/yr)"));
    }
    if let (Some(a), Some(b)) = (first.net_margin, last.net_margin) {
        out.push_str(&format!(" · net {a:.0}%→{b:.0}%"));
    }
    if let (Some(a), Some(b)) = (first.eps, last.eps) {
        // as-reported EPS spans splits UN-adjusted (NVDA's 2024 10:1 read as flat EPS growth), so
        // the leg prints only over a VERIFIABLE share history: every adjacent year carries a count
        // AND none jumps >40% (income_snapshot's split tolerance). A missing count is not "no
        // split" — Alphabet files pre-2022 counts per share class only, so the undimensioned tag
        // is None and its 20:1 split printed as eps -44%/yr. Unverifiable -> drop the leg rather
        // than print a confidently wrong number.
        let verifiable = years.windows(2).all(|w| match (w[0].shares, w[1].shares) {
            (Some(p), Some(c)) if p > 0.0 => (c / p - 1.0).abs() <= 0.4,
            _ => false,
        });
        if a > 0.0 && b > 0.0 && verifiable {
            let cagr = ((b / a).powf(1.0 / (n - 1) as f64) - 1.0) * 100.0;
            out.push_str(&format!(" · eps {cagr:+.0}%/yr"));
        }
    }
    Some(out)
}

/// Pick ONE named as-of factor out of `FundFactors` for the growth lane's fund tilt. The name comes
/// from config (`growth_fund_factor`), so the user can route whichever factor the `backtest … fund`
/// probe shows predicts best WITHOUT a recompile. An unknown name -> None (neutral) so a typo degrades
/// to the price-only score instead of panicking. Keep this match in sync with `FundFactors` and the
/// `report_fund_lane` probe list so the backtest and the live screen always weigh the SAME factor.
pub fn select_fund_factor(f: &FundFactors, name: &str) -> Option<f64> {
    match name {
        "rev_cagr" => f.rev_cagr,
        "rev_accel" => f.rev_accel,
        "gross_margin" => f.gross_margin,
        "op_margin" => f.op_margin,
        "margin_trend" => f.margin_trend,
        "eps_growth" => f.eps_growth,
        "roe" => f.roe,                                   // quality of capital (SEC feed; FMP free tier = None)
        "insider_net_buys_90d" => f.insider_net_buys_90d, // (Item 4) SEC Form-4 conviction, `backtest … insider`
        "earnings_yield" => f.earnings_yield,             // (Item 19) as-of valuation; PROBE-ONLY (None live)
        "ebitda_yield" => f.ebitda_yield,                 // (EV/EBITDA) capital-structure-neutral valuation; PROBE-ONLY (None live)
        "peg_yield" => f.peg_yield,                        // (PEG) 1/PEG = earnings_yield · CAGR, cheap-for-growth; PROBE-ONLY (None live)
        "buyback_yield" => f.buyback_yield,               // as-of 1y share-count shrink (+ = buying back); backtest-testable candidate
        "fcf_margin" => f.fcf_margin,                     // (round 107) survival: cash generation
        "interest_cover" => f.interest_cover,             // (round 107) survival: debt-service headroom
        "net_cash_rev" => f.net_cash_rev,                 // (round 107) survival: balance-sheet cushion
        "margin_stability" => f.margin_stability,         // (round 109) cyclical detector: −std(net_margin)
        "composite" => composite_factor(f),               // (Item 3) blend of the present factors
        _ => None,
    }
}

/// Build a Quote AS OF index `as_of` (inclusive) from the full history, filling ONLY the price-derived
/// fields the buy score reads — reusing the exact same horizon/SMA/vol/R²/drawdown fns on the `[..=as_of]`
/// slices, so the backtest scores a name exactly as the live tool would have on that day. note:
/// dividends / turnover / P/E / ROE are NOT reconstructed (no clean as-of source), so those score
/// terms go neutral here; the backtest validates the PRICE-based heuristic, which is the bulk of it.
pub fn backtest_quote(ticker: &str, dates: &[NaiveDate], closes: &[f64], as_of: usize, cadence: usize) -> Quote {
    let (d, c) = (&dates[..=as_of], &closes[..=as_of]);
    let mut quote = Quote::stub(ticker, "", "", ticker);
    quote.perf = horizon_changes(d, c, None, &BTreeMap::new(), None); // calendar-based -> cadence-agnostic
    quote.drawdown_pct = pct_from_high(c); // all-time anchor as of the `as_of` index
    quote.range_pct = price_pct_rank(c);
    // cadence = bars/year (252 daily, 12 monthly): vol over ~1y of bars; the long MA window scaled
    // from its daily session count so the SAME ~4y/200wk span is used on either cadence (cadence=252
    // reproduces the daily path exactly). note: monthly bars APPROXIMATE the daily vol/MA, not
    // equal them — fine, a backtest run is single-cadence so the cross-sectional ranks stay consistent.
    quote.volatility_pct = volatility_pct(c, cadence);
    let long_ma = crate::config::LONG_MA_SESSIONS * cadence / 252;
    quote.below_ma_pct = below_long_ma_pct(c, long_ma);
    quote.above_ma_pct = above_long_ma_pct(c, long_ma);
    quote.trend_r2 = trend_r2(c);
    quote.trend_cagr = trend_cagr(c, cadence); // (#14) same fit, annualized by the run's cadence -> train==serve
    quote.max_drawdown_pct = max_drawdown_pct(c);
    // closes-derived risk stats for the standalone PRICE-RISK probes (backtest report only —
    // no score path reads these). Window constants are daily-session-based (5×252, /252), so
    // fill on the daily cadence only; a monthly run leaves them None (probes then skip).
    if cadence == 252 {
        quote.roll5y_pos_pct = rolling_5y_positive_pct(c);
        quote.underwater_yrs = longest_underwater_yrs(c);
        quote.worst_5y_pct = worst_rolling_5y_pct(c);
    }
    // (#20) the LIVE growth lane excludes a name whose turnover is UNKNOWN (an untradeable/dead listing
    // like 0Y72.L). backtest_quote can't reconstruct turnover, but that absence is NOT a liquidity signal
    // here — mark it "liquid enough" so the exclusion stays a LIVE-ONLY gate (never fires in the backtest).
    // Uniform across every backtest name -> the additive liq_bonus is a constant offset -> cross-sectional
    // rank unchanged, validated edge untouched.
    quote.avg_turnover_eur = Some(1e15);
    quote
}

/// [1h, 6h, 12h] % changes = 1/6/12 hourly bars back. With ~hourly bars over several days this
/// fills for stocks too (was always n/a past a close when matched by wall-clock time).
pub fn intraday_changes(closes: &[f64]) -> [Option<f64>; 3] {
    [1, 6, 12].map(|b| intraday_pct(closes, b))
}

/// Average daily turnover (close × volume) over the last `n` sessions — a liquidity proxy.
/// Skips zero-turnover days (no volume reported); None if none usable. A thin name (tiny
/// turnover) is a riskier "opportunity" than a deep-liquid one, so picks can gate on it.
pub fn avg_turnover(closes: &[f64], volumes: &[f64], n: usize) -> Option<f64> {
    let len = closes.len().min(volumes.len());
    let start = len.saturating_sub(n);
    let vals: Vec<f64> = (start..len).map(|i| closes[i] * volumes[i]).filter(|x| *x > 0.0).collect();
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Average daily volume over the last `n` sessions, zero days skipped. For crypto the Yahoo
/// "volume" is ALREADY a notional currency amount (not a coin count), so this is turnover as-is
/// — no ×close (that would double-count). None if no usable day.
pub fn avg_volume(volumes: &[f64], n: usize) -> Option<f64> {
    let start = volumes.len().saturating_sub(n);
    let vals: Vec<f64> = volumes[start..].iter().copied().filter(|x| *x > 0.0).collect();
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Daily volatility: standard deviation of daily % returns over the last `n` sessions — the
/// asset's "normal swing". Lets the picks score judge whether a drawdown is unusually deep for
/// THIS asset (a real sale) or just its everyday noise. None if too few sessions. FX-agnostic.
pub fn volatility_pct(closes: &[f64], n: usize) -> Option<f64> {
    let len = closes.len();
    let start = len.saturating_sub(n + 1); // n returns need n+1 closes
    let rets: Vec<f64> = (start + 1..len)
        .map(|i| (closes[i] - closes[i - 1]) / closes[i - 1] * 100.0)
        .filter(|r| r.is_finite())
        .collect();
    if rets.len() < 2 {
        return None;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rets.len() as f64;
    Some(var.sqrt())
}

/// Horizons over which `screen` totals dividends (label -> calendar days back).
pub const DIV_HORIZONS: &[(&str, i64)] =
    &[("1Y", 365), ("5Y", 1825), ("10Y", 3650), ("20Y", 7300)];

/// Sum of dividend amounts paid in `(last - days, last]`. `None` if the history doesn't
/// reach back `days` (window not fully covered → "n/a", like the perf horizons), so a
/// partial window never understates a payer. `divs` = (ex-date, amount/share).
pub fn dividends_in_window(divs: &[(NaiveDate, f64)], dates: &[NaiveDate], days: i64) -> Option<f64> {
    let last = *dates.last()?;
    let first = *dates.first()?;
    let start = last - Duration::days(days);
    if first > start {
        return None; // history too short to cover the whole window
    }
    Some(divs.iter().filter(|(d, _)| *d > start && *d <= last).map(|(_, a)| a).sum())
}

/// Total dividends/share in EUR for each DIV_HORIZON (native sum × `rate`; left native if
/// FX unknown, mirroring the price column). `None` per horizon = history too short.
pub fn dividend_sums(divs: &[(NaiveDate, f64)], dates: &[NaiveDate], rate: Option<f64>) -> Vec<Option<f64>> {
    DIV_HORIZONS
        .iter()
        .map(|(_, days)| dividends_in_window(divs, dates, *days).map(|s| s * rate.unwrap_or(1.0)))
        .collect()
}

/// Average annual dividend yield (%) per DIV_HORIZON: total EUR paid in the window /
/// years in the window / current EUR price × 100. `None` per horizon = short history or
/// no/zero EUR price. `div_eur` must be the `dividend_sums` output (aligned to DIV_HORIZONS).
pub fn dividend_yields(div_eur: &[Option<f64>], price_eur: Option<f64>) -> Vec<Option<f64>> {
    let px = price_eur.filter(|p| *p > 0.0);
    DIV_HORIZONS
        .iter()
        .enumerate()
        .map(|(i, (_, days))| {
            let total = div_eur.get(i).copied().flatten()?;
            let years = *days as f64 / 365.0;
            Some(total / years / px? * 100.0)
        })
        .collect()
}

/// Signed % from a horizon entry; "n/a" if missing. ≥1000% drops the decimal so a +26522% 20Y cell
/// still fits the 8-char horizon column instead of overflowing it.
pub fn pct_cell(entry: Option<&(String, f64)>) -> String {
    match entry {
        Some((_, pct)) if pct.abs() >= 1000.0 => format!("{:+.0}%", pct),
        Some((_, pct)) => format!("{:+.1}%", pct),
        None => "n/a".to_string(),
    }
}

/// Largest single unit of a duration: s/m/h/d/w/M/Y (M=30d, Y=365d approx).
pub fn fmt_duration(td: Duration) -> String {
    let s = td.num_seconds();
    for (unit, sec) in [
        ("Y", 31_536_000i64), ("M", 2_592_000), ("w", 604_800),
        ("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1),
    ] {
        if s >= sec {
            return format!("{}{}", s / sec, unit);
        }
    }
    "0s".to_string()
}

/// ('↑'/'↓'/'→', duration_str, days) for the current consecutive price run.
/// Span = first close of the run to the latest (calendar time).
pub fn trend_streak(dates: &[NaiveDate], closes: &[f64]) -> (&'static str, String, i64) {
    let sign = |a: f64, b: f64| -> i32 { (a > b) as i32 - (a < b) as i32 };
    if closes.len() < 2 {
        return ("→", "0s".to_string(), 0);
    }
    let n = closes.len();
    let direction = sign(closes[n - 1], closes[n - 2]);
    if direction == 0 {
        return ("→", "0s".to_string(), 0);
    }
    let mut i = n - 1;
    while i >= 1 && sign(closes[i], closes[i - 1]) == direction {
        i -= 1;
    }
    let arrow = if direction > 0 { "↑" } else { "↓" };
    let span = *dates.last().expect("dates non-empty: lockstep with closes, len >= 2 guarded above") - dates[i];
    (arrow, fmt_duration(span), span.num_days())
}

/// (at_all_time_high, at_all_time_low): latest close within tol of the max/min seen.
/// 'All-time' = the fetched history window (Yahoo range=max).
pub fn extreme_flags(closes: &[f64], tol: f64) -> (bool, bool) {
    if closes.is_empty() {
        return (false, false);
    }
    let last = *closes.last().expect("closes non-empty: closes.is_empty() guarded above");
    let hi = closes.iter().cloned().fold(f64::MIN, f64::max);
    let lo = closes.iter().cloned().fold(f64::MAX, f64::min);
    (last >= hi * (1.0 - tol), last <= lo * (1.0 + tol))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (consistency) rolling_5y_positive_pct: an always-rising series scores 100%, always-falling
    /// 0%; exactly one window of history (or less) is None — no windows means NO CLAIM, never a
    /// fake 100%; a non-positive close at a window start skips that window instead of counting it.
    #[test]
    fn rolling_consistency_semantics() {
        const WIN: usize = 5 * 252;
        let rising: Vec<f64> = (1..=WIN + 50).map(|i| i as f64).collect();
        assert_eq!(rolling_5y_positive_pct(&rising), Some(100.0));
        let falling: Vec<f64> = (1..=WIN + 50).rev().map(|i| i as f64).collect();
        assert_eq!(rolling_5y_positive_pct(&falling), Some(0.0));
        assert!(rolling_5y_positive_pct(&rising[..WIN]).is_none()); // no full window -> no claim
        assert!(rolling_5y_positive_pct(&[]).is_none());
        // bad bar: zero close at the ONLY window's start -> that window skips -> nothing counted
        let mut bad = rising[..WIN + 1].to_vec();
        bad[0] = 0.0;
        assert!(rolling_5y_positive_pct(&bad).is_none());
    }

    /// (underwater) longest_underwater_yrs: rising series never dips (0.0 — a legit value, not
    /// None); a falling series is underwater its whole length; a recovery resets the peak so only
    /// the longest run wins; data-hole closes are filtered before indexing; <2 usable -> None.
    #[test]
    fn underwater_semantics() {
        let rising: Vec<f64> = (1..=300).map(|i| i as f64).collect();
        assert_eq!(longest_underwater_yrs(&rising), Some(0.0));
        let falling: Vec<f64> = (1..=5).rev().map(|i| i as f64).collect();
        assert_eq!(longest_underwater_yrs(&falling), Some(4.0 / 252.0)); // 4 sessions since peak
        // dip (1) -> recovery at 101 resets the peak -> second, longer run (3) wins
        assert_eq!(longest_underwater_yrs(&[100.0, 90.0, 101.0, 95.0, 96.0, 97.0]), Some(3.0 / 252.0));
        // leading data hole filtered out, not treated as a 0.0 peak
        assert_eq!(longest_underwater_yrs(&[0.0, 100.0, 90.0, 101.0]), Some(1.0 / 252.0));
        assert_eq!(longest_underwater_yrs(&[100.0]), None);
        assert_eq!(longest_underwater_yrs(&[]), None);
    }

    /// (worst-5y) worst_rolling_5y_pct: the MINIMUM window return wins (window A +10% vs window
    /// B −10% -> −10); a non-positive window endpoint skips that window, not the whole series;
    /// no full window -> None (no claim).
    #[test]
    fn worst_rolling_semantics() {
        const WIN: usize = 5 * 252;
        let mut closes = vec![1.0; WIN + 6];
        closes[0] = 100.0;
        closes[WIN] = 110.0; // window A (i=0): +10%
        closes[5] = 100.0;
        closes[5 + WIN] = 90.0; // window B (i=5): -10%
        assert!((worst_rolling_5y_pct(&closes).unwrap() + 10.0).abs() < 1e-9);
        closes[0] = 0.0; // bad endpoint skips window A only
        assert!((worst_rolling_5y_pct(&closes).unwrap() + 10.0).abs() < 1e-9);
        assert!(worst_rolling_5y_pct(&vec![1.0; WIN]).is_none()); // no full window -> no claim
        assert!(worst_rolling_5y_pct(&[]).is_none());
    }

    /// (#17/Step 4) `endpoint_avg`: n=1 = the raw last close (the inert default must be byte-identical);
    /// n>1 averages the last n; n beyond the history clamps to the whole series. Pure math, no config.
    #[test]
    fn endpoint_avg_smooths_last_n() {
        let closes = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(endpoint_avg(&closes, 1), 40.0); // default: raw last close
        assert_eq!(endpoint_avg(&closes, 2), 35.0); // mean of last 2
        assert_eq!(endpoint_avg(&closes, 0), 40.0); // 0 clamps up to 1
        assert_eq!(endpoint_avg(&closes, 99), 25.0); // clamps down to the full series
    }

    /// (#18) `span_to_bars`: a trading-days span means the same calendar time on any cadence —
    /// identity on daily bars, ÷21 on monthly bars, never 0.
    #[test]
    fn span_to_bars_converts_by_cadence() {
        assert_eq!(span_to_bars(105, 252), 105); // live daily: identity
        assert_eq!(span_to_bars(105, 12), 5); // 12y backtest monthly: ~5 months = 5 bars
        assert_eq!(span_to_bars(1, 252), 1); // inert default stays raw…
        assert_eq!(span_to_bars(1, 12), 1); // …on both cadences (min 1)
    }

    /// `select_fund_factor`: each config name maps to its FundFactors field; an unknown name -> None
    /// (neutral) so a typo'd config can never panic the score. Pure, no network.
    #[test]
    fn select_fund_factor_maps_names() {
        let f = FundFactors {
            rev_cagr: Some(1.0),
            rev_accel: Some(2.0),
            gross_margin: Some(3.0),
            op_margin: Some(4.0),
            margin_trend: Some(5.0),
            eps_growth: Some(6.0),
            roe: Some(11.0),
            insider_net_buys_90d: Some(7.0),
            eps_ttm: Some(8.0),
            earnings_yield: Some(9.0),
            ebitda_ttm: Some(50.0),
            shares_ttm: Some(2.0),
            net_debt: Some(-10.0),
            ebitda_yield: Some(16.0),
            peg_yield: Some(17.0),
            buyback_yield: Some(10.0),
            fcf_margin: Some(12.0),
            interest_cover: Some(13.0),
            net_cash_rev: Some(14.0),
            margin_stability: Some(15.0),
        };
        assert_eq!(select_fund_factor(&f, "rev_accel"), Some(2.0));
        assert_eq!(select_fund_factor(&f, "margin_trend"), Some(5.0));
        assert_eq!(select_fund_factor(&f, "eps_growth"), Some(6.0));
        assert_eq!(select_fund_factor(&f, "rev_cagr"), Some(1.0));
        assert_eq!(select_fund_factor(&f, "insider_net_buys_90d"), Some(7.0)); // (Item 4)
        assert_eq!(select_fund_factor(&f, "earnings_yield"), Some(9.0)); // (Item 19)
        assert_eq!(select_fund_factor(&f, "ebitda_yield"), Some(16.0)); // (EV/EBITDA)
        assert_eq!(select_fund_factor(&f, "peg_yield"), Some(17.0)); // (PEG probe)
        assert_eq!(select_fund_factor(&f, "buyback_yield"), Some(10.0));
        assert_eq!(select_fund_factor(&f, "roe"), Some(11.0)); // quality of capital; NOT in composite (a level, and the blend already failed the lane)
        assert_eq!(select_fund_factor(&f, "fcf_margin"), Some(12.0)); // (round 107) survival levels; NOT in composite either
        assert_eq!(select_fund_factor(&f, "interest_cover"), Some(13.0));
        assert_eq!(select_fund_factor(&f, "net_cash_rev"), Some(14.0));
        assert_eq!(select_fund_factor(&f, "margin_stability"), Some(15.0)); // (round 109)
        assert_eq!(select_fund_factor(&f, "composite"), Some(3.5)); // (Item 3) mean(1..6) = 21/6, valuation excluded (buyback/valuation not blended)
        assert_eq!(select_fund_factor(&f, "nope"), None); // unknown -> neutral, never panics
        // (Item 19) earnings_yield helper: EPS/price in %, guarded against div-by-zero / missing EPS
        assert_eq!(earnings_yield(Some(5.0), 100.0), Some(5.0)); // 5/100 = 5%
        assert_eq!(earnings_yield(Some(-2.0), 50.0), Some(-4.0)); // loss-maker -> negative yield (floored later in score)
        assert_eq!(earnings_yield(Some(5.0), 0.0), None); // non-positive price -> None, no div-by-zero
        assert_eq!(earnings_yield(None, 100.0), None); // no EPS -> None
        // (EV/EBITDA) ebitda_yield = EBITDA / (shares·price + net_debt), %, high = cheap
        assert_eq!(ev_ebitda_yield(Some(50.0), Some(2.0), Some(50.0), 25.0), Some(50.0)); // EV = 2*25 + 50 = 100 -> 50/100 = 50%
        assert_eq!(ev_ebitda_yield(Some(20.0), Some(2.0), Some(-10.0), 15.0), Some(100.0)); // net CASH: EV = 30 − 10 = 20 -> 20/20 = 100%
        assert_eq!(ev_ebitda_yield(Some(30.0), Some(3.0), None, 10.0), Some(100.0)); // no net_debt -> EV = mkt cap only (30) -> 30/30
        assert_eq!(ev_ebitda_yield(Some(-5.0), Some(2.0), Some(0.0), 10.0), None); // negative EBITDA -> None (multiple meaningless)
        assert_eq!(ev_ebitda_yield(Some(50.0), Some(2.0), Some(0.0), 0.0), None); // non-positive price -> None
        assert_eq!(ev_ebitda_yield(Some(50.0), None, Some(0.0), 10.0), None); // no shares -> no market cap -> None
        assert_eq!(ev_ebitda_yield(Some(10.0), Some(1.0), Some(-100.0), 10.0), None); // net cash swamps mkt cap -> EV<=0 -> None
        // (PEG probe) peg_yield = earnings_yield(%) · CAGR(%-number) = 1/PEG · 100. peg_yield == 100 ⇔ PEG == 1; > 100 ⇔ PEG < 1 (cheap for growth)
        assert_eq!(peg_yield(Some(5.0), Some(20.0), 100.0), Some(100.0)); // ey 5% · g 20 = PEG (20/20)=1 marker
        assert_eq!(peg_yield(Some(5.0), Some(40.0), 100.0), Some(200.0)); // faster growth same price -> PEG 0.5 -> yield 200 (>100)
        assert_eq!(peg_yield(Some(-2.0), Some(20.0), 50.0), None); // loss-maker -> earnings_yield<0 filtered -> None (no fabricated "cheap")
        assert_eq!(peg_yield(Some(5.0), Some(-10.0), 100.0), None); // negative growth -> PEG sign-nonsense -> None
        assert_eq!(peg_yield(Some(5.0), Some(0.0), 100.0), None); // zero growth -> PEG infinite -> None
        assert_eq!(peg_yield(Some(5.0), None, 100.0), None); // no CAGR -> None
        assert_eq!(peg_yield(Some(5.0), Some(20.0), 0.0), None); // non-positive price -> earnings_yield None -> None
    }

    /// (Item 3) `composite_factor` = mean of the factors that are `Some`; <2 present -> None (a 1-factor
    /// composite would just be that factor, so route it directly instead). insider_net_buys is NOT in the
    /// blend (different units / source), only the six FMP-derived growth factors.
    #[test]
    fn composite_factor_means_present() {
        let two = FundFactors { rev_cagr: Some(10.0), op_margin: Some(20.0), ..Default::default() };
        assert_eq!(composite_factor(&two), Some(15.0)); // mean(10,20)
        let one = FundFactors { eps_growth: Some(9.0), ..Default::default() };
        assert_eq!(composite_factor(&one), None); // only 1 factor -> None
        assert_eq!(composite_factor(&FundFactors::default()), None); // nothing -> None
    }

    /// `annual_rollup`: quarters group by period_end YEAR (newest first), revenue + eps SUM, margins are
    /// revenue-weighted, and an incomplete year reports its real `quarters` count so the print layer flags it.
    #[test]
    fn annual_rollup_groups_and_weights() {
        let q = |y: i32, m: u32, rev: f64, gm: f64, eps: f64| FundRow {
            period_end: NaiveDate::from_ymd_opt(y, m, 28).unwrap(),
            revenue: Some(rev),
            gross_margin: Some(gm),
            eps: Some(eps),
            ..Default::default()
        };
        // 2022: 4 quarters; 2023: 3 quarters (partial). Out of order on purpose.
        let rows = vec![
            q(2022, 3, 100.0, 40.0, 1.0),
            q(2023, 9, 200.0, 60.0, 4.0),
            q(2022, 6, 100.0, 50.0, 2.0),
            q(2023, 3, 200.0, 50.0, 3.0),
            q(2022, 9, 200.0, 50.0, 1.5),
            q(2022, 12, 100.0, 50.0, 1.5),
            q(2023, 6, 200.0, 55.0, 3.5),
        ];
        let out = annual_rollup(&rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].year, 2023); // newest first
        assert_eq!(out[1].year, 2022);
        // 2022 revenue = 100+100+200+100 = 500; eps summed = 6.0; 4 quarters
        assert_eq!(out[1].revenue, 500.0);
        assert_eq!(out[1].eps, Some(6.0));
        assert_eq!(out[1].quarters, 4);
        // 2022 gross margin = Σ(gm·rev)/Σrev = (40·100+50·100+50·200+50·100)/500 = 24000/500 = 48.0
        assert!((out[1].gross_margin.unwrap() - 48.0).abs() < 1e-9);
        // 2023 is the partial year: 3 quarters, revenue 600, eps 10.5
        assert_eq!(out[0].quarters, 3);
        assert_eq!(out[0].revenue, 600.0);
        assert_eq!(out[0].eps, Some(10.5));
        // a missing margin/eps drops out, never fabricates 0
        let sparse = vec![FundRow { period_end: NaiveDate::from_ymd_opt(2024, 3, 28).unwrap(), revenue: Some(10.0), ..Default::default() }];
        let s = annual_rollup(&sparse);
        assert_eq!(s[0].gross_margin, None);
        assert_eq!(s[0].eps, None);
        assert_eq!(s[0].revenue, 10.0);
    }

    /// (round 109) `margin_stability` = negated sample stddev of net_margin over the as-of rows:
    /// 10/20/30 -> mean 20, sample variance 100, std 10 -> factor −10. Fewer than 3 values -> None
    /// (2 points define a line, not a dispersion), and rows filed after the cutoff never leak in.
    #[test]
    fn margin_stability_stddev() {
        let r = |y: i32, nm: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(y, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y - 1, 12, 31).unwrap(),
            net_margin: Some(nm),
            ..Default::default()
        };
        let rows = vec![r(2022, 10.0), r(2023, 20.0), r(2024, 30.0)];
        let cutoff = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap();
        let f = fund_factors(&rows, cutoff, 5);
        assert!((f.margin_stability.unwrap() + 10.0).abs() < 1e-9);
        // only 2 net_margin values -> None
        assert_eq!(fund_factors(&rows[..2], cutoff, 5).margin_stability, None);
        // as-of guard: cutoff before the 2024 filing leaves 2 rows -> None, no look-ahead
        assert_eq!(fund_factors(&rows, NaiveDate::from_ymd_opt(2023, 6, 1).unwrap(), 5).margin_stability, None);
    }

    /// `income_snapshot`: picks the newest COMPLETE year (1 = annual filing, 4+ = full quarterly year;
    /// 2-3 = partial, skipped), YoY vs the next-older row with report's exact math, and refuses to
    /// fabricate a number: no older row / zero prior EPS -> that component None; no complete year -> None.
    #[test]
    fn income_snapshot_complete_year_and_yoy() {
        let a = |year: i32, revenue: f64, eps: Option<f64>, quarters: usize| AnnualReport {
            year, revenue, gross_margin: None, op_margin: None, net_margin: Some(50.0), eps, shares: None, quarters,
        };
        // shares-carrying variant for the buyback leg
        let s = |year: i32, revenue: f64, eps: Option<f64>, shares: Option<f64>, quarters: usize| AnnualReport {
            year, revenue, gross_margin: None, op_margin: None, net_margin: Some(50.0), eps, shares, quarters,
        };
        // newest year partial (3 quarters) -> skipped; snapshot = 2023 vs 2022
        let rows = vec![a(2024, 900.0, Some(9.0), 3), a(2023, 600.0, Some(6.0), 4), a(2022, 500.0, Some(4.0), 4)];
        let (rev, eps, net, bb) = income_snapshot(&rows).unwrap();
        assert!((rev.unwrap() - 20.0).abs() < 1e-9); // 600/500
        assert!((eps.unwrap() - 50.0).abs() < 1e-9); // 6/4
        assert_eq!(net, Some(50.0));
        assert_eq!(bb, None); // no shares on these rows
        // SEC-style annual filing (quarters == 1) counts as complete
        let sec = vec![a(2023, 600.0, Some(6.0), 1), a(2022, 500.0, Some(6.0), 1)];
        assert!((income_snapshot(&sec).unwrap().0.unwrap() - 20.0).abs() < 1e-9);
        // buyback: shares shrank 100->95 -> −(−5%) = +5% (buying back); split-size jump -> None
        let buy = vec![s(2023, 600.0, Some(6.0), Some(95.0), 1), s(2022, 500.0, Some(6.0), Some(100.0), 1)];
        assert!((income_snapshot(&buy).unwrap().3.unwrap() - 5.0).abs() < 1e-9);
        let split = vec![s(2023, 600.0, Some(6.0), Some(200.0), 1), s(2022, 500.0, Some(6.0), Some(100.0), 1)];
        assert_eq!(income_snapshot(&split).unwrap().3, None); // +100% shares = split, not dilution signal
        // oldest year in the data: nothing older to compare -> YoY components None, margin still real
        let lone = vec![a(2023, 600.0, Some(6.0), 4)];
        assert_eq!(income_snapshot(&lone).unwrap(), (None, None, Some(50.0), None));
        // zero prior EPS -> eps_yoy None (never a divide blow-up); prior zero revenue -> rev_yoy None
        let zeroes = vec![a(2023, 600.0, Some(6.0), 4), a(2022, 0.0, Some(0.0), 4)];
        assert_eq!(income_snapshot(&zeroes).unwrap(), (None, None, Some(50.0), None));
        // only partial years -> no snapshot at all
        assert_eq!(income_snapshot(&[a(2024, 900.0, None, 2)]), None);
        assert_eq!(income_snapshot(&[]), None);
    }

    /// (B) `annual_brief`: newest ≤5 complete years, partial years dropped, oldest→newest chain with
    /// rev/EPS CAGR; <2 complete years -> None; loss-year EPS endpoint -> EPS leg omitted, never a
    /// nonsense negative-ratio CAGR; EPS leg needs a verifiable share chain (present + no split jump).
    #[test]
    fn annual_brief_trajectory() {
        let y = |year: i32, revenue: f64, nm: Option<f64>, eps: Option<f64>, quarters: usize| AnnualReport {
            year, revenue, gross_margin: None, op_margin: None, net_margin: nm, eps, shares: Some(16.0e9), quarters,
        };
        // newest-first like annual_rollup; 2024 partial (2 quarters) must be dropped from the chain
        let rows = vec![
            y(2024, 100e9, Some(30.0), Some(9.9), 2),
            y(2023, 391e9, Some(25.0), Some(6.1), 1),
            y(2022, 383e9, Some(25.0), Some(6.1), 4),
            y(2021, 394e9, Some(25.0), Some(6.1), 1),
            y(2020, 366e9, Some(21.0), Some(3.3), 1),
            y(2019, 274e9, Some(21.0), Some(3.0), 1),
        ];
        let b = annual_brief(&rows).unwrap();
        assert!(b.starts_with("rev 5y 274.0B→366.0B→394.0B→383.0B→391.0B"), "{b}");
        assert!(b.contains("(+9%/yr)"), "{b}"); // (391/274)^(1/4)−1
        assert!(b.contains("net 21%→25%"), "{b}");
        assert!(b.contains("eps +19%/yr"), "{b}"); // (6.1/3.0)^(1/4)−1
        // one complete year -> no trajectory; loss endpoint -> EPS leg omitted, rev leg stays
        assert_eq!(annual_brief(&rows[..2]), None); // 2024 partial + 2023 = only 1 complete
        let loss = vec![y(2023, 600.0, None, Some(2.0), 1), y(2022, 500.0, None, Some(-1.0), 1)];
        let lb = annual_brief(&loss).unwrap();
        assert!(!lb.contains("eps"), "{lb}");
        assert!(lb.contains("rev 2y 500→600 (+20%/yr)"), "{lb}");
        // a >40% share-count jump (split) makes as-reported EPS CAGR a lie -> leg omitted
        let ys = |year: i32, eps: f64, shares: f64| AnnualReport {
            year, revenue: 500.0, gross_margin: None, op_margin: None, net_margin: None,
            eps: Some(eps), shares: Some(shares), quarters: 1,
        };
        let split = vec![ys(2023, 2.0, 1000.0), ys(2022, 15.0, 100.0)];
        assert!(!annual_brief(&split).unwrap().contains("eps"));
        let nosplit = vec![ys(2023, 18.0, 102.0), ys(2022, 15.0, 100.0)];
        assert!(annual_brief(&nosplit).unwrap().contains("eps +20%/yr"));
        // (GOOG shape) a missing share count anywhere in the window = UNVERIFIABLE split history ->
        // leg omitted even with profitable endpoints (Alphabet's pre-2022 counts are per-class only,
        // so the 2022 20:1 split was invisible and eps -44%/yr printed against rev +12%/yr).
        let noshares = AnnualReport {
            year: 2021, revenue: 257.6e9, gross_margin: None, op_margin: None, net_margin: None,
            eps: Some(112.2), shares: None, quarters: 1,
        };
        let unverifiable = vec![ys(2025, 10.81, 12.2e9), noshares];
        let ub = annual_brief(&unverifiable).unwrap();
        assert!(!ub.contains("eps"), "{ub}");
    }

    /// (Item 4) `insider_net_buys` counts P(+1)/S(−1) only in [cutoff−window, cutoff): a same-day or later
    /// filing is excluded (look-ahead guard), and an empty window -> None (no coverage, never a fake 0).
    #[test]
    fn insider_net_buys_windows_and_guards() {
        let d = |m, day| NaiveDate::from_ymd_opt(2020, m, day).unwrap();
        let txns = vec![
            InsiderTx { date: d(1, 10), buy: true },  // in window for a Mar cutoff
            InsiderTx { date: d(2, 15), buy: true },  // in window
            InsiderTx { date: d(2, 20), buy: false }, // in window (a sale, −1)
            InsiderTx { date: d(3, 1), buy: true },   // ON the cutoff -> excluded (look-ahead)
        ];
        let cutoff = d(3, 1);
        assert_eq!(insider_net_buys(&txns, cutoff, 90), Some(1.0)); // +1 +1 −1 = +1; the d(3,1) buy excluded
        assert_eq!(insider_net_buys(&txns, cutoff, 5), None); // nothing in the 5d before -> no coverage
        assert_eq!(insider_net_buys(&[], cutoff, 90), None); // no data -> None
    }

    /// Pure-logic asserts (no network). White-box: reaches `core` privates via `use super::*`.
    #[test]
    fn pure_logic() {
    assert!((pct_from_high(&[100.0, 80.0, 95.0]) - 5.0).abs() < 1e-9);
    assert_eq!(pct_from_high(&[90.0, 100.0]), 0.0);
    // backtest correlation helpers: perfect monotone -> +1, reversed -> -1, robust ranks
    assert!((pearson(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]).unwrap() - 1.0).abs() < 1e-9);
    assert!((pearson(&[1.0, 2.0, 3.0], &[6.0, 4.0, 2.0]).unwrap() + 1.0).abs() < 1e-9);
    assert!(pearson(&[1.0, 1.0], &[1.0, 1.0]).is_none()); // zero variance
    assert!((spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 30.0, 1000.0, 99999.0]).unwrap() - 1.0).abs() < 1e-9); // monotone order -> +1, outlier magnitude ignored
    assert!((spearman(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]).unwrap() + 1.0).abs() < 1e-9);
    assert_eq!(ranks(&[10.0, 30.0, 20.0]), vec![1.0, 3.0, 2.0]);
    assert_eq!(ranks(&[5.0, 5.0, 9.0]), vec![1.5, 1.5, 3.0]); // ties share the average rank
    assert_eq!(market_of("VWCE.DE"), "Germany");
    assert_eq!(market_of("AAPL"), "USA");
    assert_eq!(market_of("BTC-USD"), "Crypto");
    assert_eq!(market_of("BRK-B"), "USA"); // share-class dash is not a coin (same trap as report's r73 fix)
    // nupl_zone: band edges
    assert_eq!(nupl_zone(-0.1), "Capitulation");
    assert_eq!(nupl_zone(0.16), "Hope/Fear");
    assert_eq!(nupl_zone(0.6), "Belief/Denial");
    assert_eq!(nupl_zone(0.8), "Euphoria/Greed");
    // sector_matches: empty filter keeps all; else case-insensitive substring on ANY keyword
    let tech = vec!["Technology".to_string(), "Communication".to_string()];
    assert!(sector_matches("Industrials", &[])); // no filter -> keep everything
    assert!(sector_matches("Information Technology", &tech)); // "Technology" is a substring
    assert!(sector_matches("iShares Tech Sector Technology UCITS", &tech)); // ETF name path
    assert!(!sector_matches("Industrials", &tech));
    // sector_symbol: keep only filter-matching sectors (Yahoo-normalized), all when filter empty
    assert_eq!(
        sector_symbol("AAPL,Apple Inc.,Information Technology,Tech HW,x,y", &tech),
        Some(("AAPL".to_string(), "Information Technology".to_string()))
    );
    assert_eq!(sector_symbol("GOOGL,Alphabet,Communication Services,x", &tech).map(|(s, _)| s), Some("GOOGL".to_string()));
    assert_eq!(sector_symbol("BF.B,Brown-Forman,Information Technology,x", &tech).map(|(s, _)| s), Some("BF-B".to_string())); // '.'->'-'
    // European venue suffixes are already Yahoo form — the class-share dash rewrite must NOT touch them
    assert_eq!(sector_symbol("AAF.L,Airtel Africa PLC", &[]).map(|(s, _)| s), Some("AAF.L".to_string()));
    assert_eq!(sector_symbol("ADS.DE,adidas AG", &[]).map(|(s, _)| s), Some("ADS.DE".to_string()));
    assert_eq!(sector_symbol("MMM,3M,Industrials,x", &tech), None);
    assert_eq!(sector_symbol("AMZN,Amazon,Consumer Discretionary,x", &tech), None); // GICS quirk: not tech
    assert_eq!(sector_symbol("MMM,3M,Industrials,x", &[]).map(|(s, _)| s), Some("MMM".to_string())); // empty filter -> all sectors
    // quoted comma in the Security NAME shifts the sector one column right — must still read the sector
    assert_eq!(
        sector_symbol("CASY,\"Casey's General Stores, Inc.\",Consumer Staples,x", &[]),
        Some(("CASY".to_string(), "Consumer Staples".to_string()))
    );
    // (Item 32) a 2-column list (Symbol,Name) has no sector -> kept under "other" (not dropped),
    // but a sector-restricted filter still excludes it
    assert_eq!(sector_symbol("PDD,PDD Holdings", &[]), Some(("PDD".to_string(), "other".to_string())));
    assert_eq!(sector_symbol("PDD,PDD Holdings", &tech), None);
    // (Item 32) wiki_constituents: real shape of the Wikipedia constituents table — symbol anchor in
    // cell 0, sector text in cell 2; header row skipped; no table id -> empty (pond drops, no crash)
    let wiki = r#"<table class="wikitable sortable" id="constituents">
<tbody><tr><th>Symbol</th><th>Security</th><th>GICS Sector</th></tr>
<tr><td style="x"><a href="y" data-mw='{"params":{"1":{"wt":"AA"}}}'>AA</a></td>
<td><a href="//w/Alcoa">Alcoa</a></td><td>Materials</td><td>Aluminum</td></tr>
<tr><td><a>BRK.B</a></td><td><a>Berkshire</a></td><td>Financials</td><td>x</td></tr></tbody></table>"#;
    assert_eq!(
        wiki_constituents(wiki, &[]),
        vec![("AA".to_string(), "Materials".to_string()), ("BRK-B".to_string(), "Financials".to_string())]
    );
    assert_eq!(wiki_constituents(wiki, &["Financials".to_string()]).len(), 1); // sector filter applies
    assert!(wiki_constituents("<html>no table here</html>", &[]).is_empty());
    // euronext_lisbon_symbols: symbol at row index 2 -> `<SYM>.LS`; odd/empty cells dropped; no aaData -> []
    let es = serde_json::json!({"aaData": [
        ["<a href=x>GALP ENERGIA</a>", "PTGAL0AM0009", "GALP", "<div>XLIS</div>", "EUR 18", "0.1%", "12:00"],
        ["<a>EDP</a>", "PTEDP0AM0009", "EDP", "<div>XLIS</div>", "EUR 3", "-0.2%", "12:00"],
        ["bad", "isin", "", "x", "y", "z", "w"],   // empty symbol -> dropped
        ["bad", "isin", "FOO BAR", "x", "y", "z", "w"], // non-alnum symbol -> dropped
    ]});
    assert_eq!(euronext_lisbon_symbols(&es), vec!["GALP.LS".to_string(), "EDP.LS".to_string()]);
    assert!(euronext_lisbon_symbols(&serde_json::json!({})).is_empty()); // no aaData -> empty leg, not a crash
    // euronext_track_isins: ISIN at row index 1; non-ISIN-shaped cells dropped; no aaData -> []
    let et = serde_json::json!({"aaData": [
        ["<a data-title-hover=\"iShares X\">X</a>", "IE0007G78AC4", "ASIG", "<div>XAMC</div>"],
        ["<a>Y</a>", "LU2870272650", "DXUSH", "<div>ETFP</div>"],
        ["bad", "not-an-isin", "Z", "x"],       // wrong shape -> dropped
        ["bad", "ie0007g78ac4", "Z", "x"],      // lowercase prefix -> dropped
    ]});
    assert_eq!(euronext_track_isins(&et), vec!["IE0007G78AC4".to_string(), "LU2870272650".to_string()]);
    assert!(euronext_track_isins(&serde_json::json!({})).is_empty()); // no aaData -> empty leg, not a crash
    // six_fund_isins: [ISIN, ShortName] rows; ETF/UCITS-named kept, mutual funds + bad ISINs dropped
    let six = serde_json::json!({"rowData": [
        ["IE00B5BMR087", "iShares Core S&P 500 ETF"],
        ["LU2870272650", "D-X MSCI USA Screened UCITS"],
        ["AT0000A255C8", "LGT PB Conservative USD R"],  // mutual fund -> dropped
        ["not-an-isin", "Some ETF"],                     // bad ISIN -> dropped
    ]});
    assert_eq!(six_fund_isins(&six), vec!["IE00B5BMR087".to_string(), "LU2870272650".to_string()]);
    assert!(six_fund_isins(&serde_json::json!({})).is_empty()); // no rowData -> empty leg, not a crash
    // firds_latest_fulins_link: both registry shapes, newest FULINS_C wins, non-C files ignored
    let esma = serde_json::json!({"response": {"docs": [
        {"file_name": "FULINS_C_20260627_01of01.zip", "download_link": "https://x/old.zip"},
        {"file_name": "FULINS_C_20260704_01of01.zip", "download_link": "https://x/new.zip"},
        {"file_name": "FULINS_D_20260711_01of01.zip", "download_link": "https://x/wrong-class.zip"},
    ]}});
    assert_eq!(firds_latest_fulins_link(&esma).as_deref(), Some("https://x/new.zip"));
    let fca = serde_json::json!({"hits": {"hits": [
        {"_source": {"file_name": "FULINS_C_20260606_01of01.zip", "download_link": "https://y/old.zip"}},
        {"_source": {"file_name": "FULINS_C_20260620_01of01.zip", "download_link": "https://y/new.zip"}},
    ]}});
    assert_eq!(firds_latest_fulins_link(&fca).as_deref(), Some("https://y/new.zip"));
    assert!(firds_latest_fulins_link(&serde_json::json!({})).is_none()); // reshaped -> None, not a crash
    // firds_etf_isins: CFI CE* + ETF/UCITS name + EU domicile kept; mutual funds (CI*), US funds,
    // non-ETF names dropped; single-line (ESMA) and pretty-printed (FCA) records both parse
    let xml = "<FinInstrmGnlAttrbts><Id>IE000HN2PIB9</Id><FullNm>AXA IM Nasdaq 100 UCITS ETF</FullNm><ShrtNm>AXA/NDX</ShrtNm><ClssfctnTp>CEOGBS</ClssfctnTp></FinInstrmGnlAttrbts>\
         <FinInstrmGnlAttrbts><Id>LU0334293981</Id><FullNm>Acatis Champions UCITS</FullNm><ShrtNm>ACATIS</ShrtNm><ClssfctnTp>CIOIES</ClssfctnTp></FinInstrmGnlAttrbts>\
         <FinInstrmGnlAttrbts><Id>US46437F1027</Id><FullNm>iShares ESG Aware ETF</FullNm><ClssfctnTp>CEOGBS</ClssfctnTp></FinInstrmGnlAttrbts>\
         <FinInstrmGnlAttrbts><Id>DK0060749877</Id><FullNm>Sydinvest Formue Akk A</FullNm><ClssfctnTp>CEOGBS</ClssfctnTp></FinInstrmGnlAttrbts>\n\
         <FinInstrmGnlAttrbts>\n  <Id>LU2523866023</Id>\n  <FullNm>Xtrackers Global Bond UCITS ETF</FullNm>\n  <ShrtNm>XGLB</ShrtNm>\n  <ClssfctnTp>CEOGBS</ClssfctnTp>\n</FinInstrmGnlAttrbts>";
    assert_eq!(
        firds_etf_isins(xml),
        vec!["IE000HN2PIB9".to_string(), "LU2523866023".to_string()]
    );
    assert!(firds_etf_isins("").is_empty()); // empty/garbage file -> empty leg, not a crash
    // hold_suitable: broad + cheap + physical + Acc + large + UCITS -> H; any leg failing -> no H
    let hold = |name: &str, ter: Option<f64>, repl: Option<&'static str>, use_: Option<&'static str>, aum: Option<f64>| {
        let mut q = Quote::stub("X", "", "", name);
        q.expense_ratio = ter;
        q.replication = repl;
        q.use_of_profits = use_;
        q.aum_eur = aum;
        hold_suitable(&q)
    };
    assert!(hold("Vanguard S&P 500 UCITS ETF USD Acc", Some(0.07), Some("Full"), Some("Acc"), Some(28.8e9))); // VUAA
    assert!(hold("State Street SPDR S&P 500 UCITS ETF", Some(0.03), Some("Full"), Some("Acc"), Some(15.0e9))); // SPYL
    assert!(!hold("Amundi S&P 500 Swap UCITS ETF", Some(0.15), Some("Swap"), Some("Acc"), Some(2.6e9))); // AUM5: swap
    assert!(!hold("iShares S&P 500 Information Technology UCITS", Some(0.15), Some("Full"), Some("Acc"), Some(16.1e9))); // sector
    assert!(!hold("Amundi Nasdaq-100 UCITS ETF", Some(0.22), Some("Full"), Some("Acc"), Some(5.8e9))); // nasdaq = not broad
    assert!(!hold("Vanguard S&P 500 UCITS ETF", None, Some("Full"), Some("Acc"), Some(28.8e9))); // no TER (venue fund) -> not vouched cheap
    assert!(!hold("Apple", None, None, None, None)); // a stock -> false
    assert!(hold("Vanguard FTSE All-World UCITS ETF USD Acc", Some(0.22), Some("Full"), Some("Acc"), Some(15e9))); // VWCE: 0.22% all-world under the 0.25 cap
    assert!(!hold("Vanguard FTSE All-World UCITS ETF USD Acc", Some(0.30), Some("Full"), Some("Acc"), Some(15e9))); // 0.30% too dear for a core
    assert!(!hold("iShares MSCI World EUR Hedged UCITS ETF Acc", Some(0.20), Some("Full"), Some("Acc"), Some(5e9))); // hedged class: hedge-cost drag, not the canonical core
    assert!(!hold("Xtrackers MSCI World Minimum Volatility UCITS ETF", Some(0.25), Some("Full"), Some("Acc"), Some(1.1e9))); // spelled-out factor tilt (live CORE receipt)
    assert!(!hold("BNP PARIBAS EASY II MSCI World PAB UCITS ETF Acc", Some(0.20), Some("Full"), Some("Acc"), Some(1.5e9))); // PAB = Paris-Aligned Benchmark, an ESG screen (live CORE receipt)

    // (round 47) Yahoo fallback facts count for the H flag via ter_shown/aum_shown — a venue fund with
    // no BF TER/AUM but Yahoo facts qualifies; BF stays authoritative when both are present.
    let mut q = Quote::stub("X", "", "", "Vanguard S&P 500 UCITS ETF USD Acc");
    q.replication = Some("Full");
    q.use_of_profits = Some("Acc");
    q.ter_fallback = Some(0.07);
    q.aum_fallback = Some(5e9);
    assert!(hold_suitable(&q));
    q.expense_ratio = Some(0.30); // BF answers dear -> fallback must NOT mask it
    assert_eq!(q.ter_shown(), Some(0.30));
    assert!(!hold_suitable(&q));

    // (round 49) hold_miss_reason: first failing leg, printable; None == hold_suitable by construction
    let miss = |name: &str, ter: Option<f64>, repl: Option<&'static str>, use_: Option<&'static str>, aum: Option<f64>| {
        let mut q = Quote::stub("X", "", "", name);
        q.expense_ratio = ter;
        q.replication = repl;
        q.use_of_profits = use_;
        q.aum_eur = aum;
        hold_miss_reason(&q)
    };
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF USD Acc", Some(0.07), Some("Full"), Some("Acc"), Some(28.8e9)), None); // VUAA passes all
    assert_eq!(miss("Amundi Nasdaq-100 UCITS ETF", Some(0.22), Some("Full"), Some("Acc"), Some(5.8e9)).as_deref(), Some("not a broad-index name (sector/thematic/factor tilt)"));
    assert_eq!(miss("Vanguard S&P 500 ETF", Some(0.03), Some("Full"), Some("Acc"), Some(2e9)).as_deref(), Some("no UCITS token in the name"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF", None, Some("Full"), Some("Acc"), Some(2e9)).as_deref(), Some("TER unknown"));
    assert_eq!(miss("Vanguard FTSE All-World UCITS ETF", Some(0.30), Some("Full"), Some("Acc"), Some(15e9)).as_deref(), Some("TER 0.30% > 0.25% cap"));
    assert_eq!(miss("Amundi S&P 500 Swap UCITS ETF", Some(0.15), Some("Swap"), Some("Acc"), Some(2.6e9)).as_deref(), Some("replication Swap (needs physical)")); // AUM5
    // (round 53) sampling IS physical: VWRA-shape all-world fund ("Optimised") must pass — the old
    // literal-Full leg kept the CORE tier-0 slot empty (every big all-world fund samples).
    assert_eq!(miss("Vanguard FTSE All-World UCITS ETF USD Acc", Some(0.22), Some("Opt"), Some("Acc"), Some(42.9e9)), None);
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF Acc", Some(0.07), None, Some("Acc"), Some(2e9)).as_deref(), Some("replication unknown (needs physical)"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF", Some(0.07), Some("Full"), Some("Dist"), Some(2e9)).as_deref(), Some("share class Dist (needs Acc)"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF Acc", Some(0.07), Some("Full"), Some("Acc"), Some(0.3e9)).as_deref(), Some("AUM €0.3B < €1B floor"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF Acc", Some(0.07), Some("Full"), Some("Acc"), None).as_deref(), Some("AUM unknown"));

    // hold_breadth_tier: broadest (all-world/ACWI) sorts first, S&P 500 last
    assert_eq!(hold_breadth_tier("Vanguard FTSE All-World UCITS ETF"), 0);
    assert_eq!(hold_breadth_tier("SPDR MSCI ACWI UCITS ETF"), 0);
    assert_eq!(hold_breadth_tier("iShares Core MSCI World UCITS ETF"), 1);
    assert_eq!(hold_breadth_tier("Vanguard S&P 500 UCITS ETF"), 2);
    assert_eq!(
        source_url("https://finance.yahoo.com/quote/{ticker}", "BTC-USD"),
        "https://finance.yahoo.com/quote/BTC-USD"
    );

    assert_eq!(headline_titles(&[serde_json::json!({"title": "flat"})]), vec!["flat"]);
    assert_eq!(headline_titles(&[serde_json::json!({"content": {"title": "nested"}})]), vec!["nested"]);
    assert!(headline_titles(&[serde_json::json!({}), serde_json::json!({"content": {}})]).is_empty());

    let ds = vec![
        NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2020, 1, 2).unwrap(),
        NaiveDate::from_ymd_opt(2020, 1, 3).unwrap(),
    ];
    let cs = vec![10.0, 20.0, 30.0];
    assert_eq!(asof(&ds, &cs, NaiveDate::from_ymd_opt(2020, 1, 2).unwrap()), Some(20.0));
    assert_eq!(asof(&ds, &cs, NaiveDate::from_ymd_opt(2019, 6, 1).unwrap()), None);
    assert_eq!(asof(&ds, &cs, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()), Some(30.0));
    // asof_avg: ±2d window over Jan-2 averages all 3 days (smooths an outlier); window with no close = None
    assert_eq!(asof_avg(&ds, &cs, NaiveDate::from_ymd_opt(2020, 1, 2).unwrap(), 2), Some(20.0));
    // splice_history: proxy bars before the listing's start are rebased so the chain is continuous
    // at the boundary (proxy 50 as-of own-first 10 -> factor 0.2), own series unchanged after it
    let pd = vec![
        NaiveDate::from_ymd_opt(2019, 12, 30).unwrap(),
        NaiveDate::from_ymd_opt(2019, 12, 31).unwrap(),
        NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
    ];
    let pc = vec![40.0, 45.0, 50.0];
    let (sd, sc) = splice_history(&ds, &cs, &pd, &pc).unwrap();
    assert_eq!(sd.len(), 5); // 2 prepended proxy bars (strictly < 2020-01-01) + 3 own
    assert_eq!(sc, vec![8.0, 9.0, 10.0, 20.0, 30.0]); // 40*0.2, 45*0.2, then own untouched
    assert_eq!(sd[0], pd[0]);
    assert_eq!(sd[2..], ds[..]);
    // proxy with nothing older than the listing -> None (splice adds nothing)
    assert!(splice_history(&ds, &cs, &ds, &cs).is_none());
    // proxy that doesn't reach the listing's start -> None (no rebase anchor)
    let late = vec![NaiveDate::from_ymd_opt(2020, 6, 1).unwrap()];
    assert!(splice_history(&ds, &cs, &late, &[99.0]).is_none());
    assert_eq!(asof_avg(&ds, &cs, NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), 0), Some(10.0));
    assert_eq!(asof_avg(&ds, &cs, NaiveDate::from_ymd_opt(2019, 1, 1).unwrap(), 5), None);
    // backtest_quote on a synthetic rising MONTHLY series (cadence=12): the cadence window math must
    // still populate volatility (from monthly returns) and put a monotone climber at the top of its
    // range. Guards the long-horizon path against a zero/oversized window silently nulling the metrics.
    let mdates: Vec<NaiveDate> =
        (0..60).map(|m| NaiveDate::from_ymd_opt(2015, 1, 1).unwrap() + chrono::Duration::days(30 * m)).collect();
    let mcloses: Vec<f64> = (0..60).map(|m| 100.0 * 1.01_f64.powi(m)).collect();
    let mq = backtest_quote("X", &mdates, &mcloses, mdates.len() - 1, 12);
    assert!(mq.volatility_pct.is_some());
    assert!(mq.range_pct > 90.0); // rising every bar -> sits at its range high
    // fund_as_of point-in-time join: latest row FILED on/before the cutoff, NEVER a future filing
    // (the look-ahead guard). Rows out of order on purpose to prove order-independence.
    let frows = vec![
        FundRow { filed: NaiveDate::from_ymd_opt(2022, 2, 1).unwrap(), revenue: Some(200.0), roe: Some(18.0), ..Default::default() },
        FundRow { filed: NaiveDate::from_ymd_opt(2020, 2, 1).unwrap(), revenue: Some(100.0), ..Default::default() },
        FundRow { filed: NaiveDate::from_ymd_opt(2021, 2, 1).unwrap(), revenue: Some(150.0), roe: Some(12.0), ..Default::default() },
    ];
    // cutoff between the 2021 and 2022 filings -> sees 2021, NOT the unfiled 2022 (no look-ahead)
    assert_eq!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2021, 6, 1).unwrap()).unwrap().revenue, Some(150.0));
    assert_eq!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2023, 1, 1).unwrap()).unwrap().revenue, Some(200.0)); // after all -> latest
    assert!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2019, 1, 1).unwrap()).is_none()); // before any filing -> nothing public
    assert_eq!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2021, 2, 1).unwrap()).unwrap().revenue, Some(150.0)); // exact filing date visible (<=)
    // fund_factors: revenue 100 -> 200 over 2y (filed 2020 vs 2022) = ~41.4%/yr CAGR, all as-of (no
    // look-ahead). margin/eps None here (rows carry only revenue) -> a premium/absent field stays neutral.
    let ff = fund_factors(&frows, NaiveDate::from_ymd_opt(2022, 3, 1).unwrap(), 2);
    assert!((ff.rev_cagr.unwrap() - 41.42).abs() < 0.1); // sqrt(2)-1 ≈ 41.4%/yr
    assert!(ff.op_margin.is_none() && ff.eps_growth.is_none()); // absent fields -> None, never a garbage value
    assert!(fund_factors(&frows, NaiveDate::from_ymd_opt(2020, 6, 1).unwrap(), 2).rev_cagr.is_none()); // no row 2y before -> None
    // as-of roe: the level rides the same fund_as_of look-ahead guard — a cutoff between the 2021
    // and 2022 filings sees 12.0, NEVER the unfiled 18.0; after both, the latest level.
    assert_eq!(ff.roe, Some(18.0));
    assert_eq!(fund_factors(&frows, NaiveDate::from_ymd_opt(2021, 6, 1).unwrap(), 2).roe, Some(12.0));
    // default_anchor_half: window widens with horizon length; 1D exact
    assert_eq!(default_anchor_half(1), 0);
    assert_eq!(default_anchor_half(7), 7);
    assert_eq!(default_anchor_half(365), 90);
    assert_eq!(default_anchor_half(3650), 365);
    // real_pct: 0% cumulative inflation = unchanged; flat nominal under +10% inflation = ~-9% real
    assert_eq!(real_pct(100.0, 0.0), 100.0);
    assert!((real_pct(0.0, 10.0) - (-9.0909091)).abs() < 1e-4);
    assert!((real_pct(50.0, 10.0) - 36.3636363).abs() < 1e-4); // +50% nominal, +10% infl -> ~+36% real
    assert_eq!(slice_since(&ds, &cs, 1), vec![20.0, 30.0]);

    // intraday: bar-count back. 7 bars, last=110. 1 bar back=105 -> +4.76%; 6 bars back=100 -> +10%
    let ics = vec![100.0, 101.0, 102.0, 103.0, 104.0, 105.0, 110.0];
    assert!((intraday_pct(&ics, 1).unwrap() - (110.0 - 105.0) / 105.0 * 100.0).abs() < 1e-9);
    assert!((intraday_pct(&ics, 6).unwrap() - 10.0).abs() < 1e-9);
    assert_eq!(intraday_pct(&ics, 12), None); // only 7 bars -> 12 back unavailable
    assert_eq!(intraday_changes(&ics)[2], None); // 12h slot n/a (short history)
    assert!(intraday_changes(&ics)[0].is_some()); // 1h slot present

    // turnover: avg of close*volume over last n, zero-volume days skipped; None if all zero/empty
    assert_eq!(avg_turnover(&[10.0, 20.0], &[100.0, 200.0], 30), Some((1000.0 + 4000.0) / 2.0));
    assert_eq!(avg_turnover(&[10.0, 20.0], &[0.0, 200.0], 30), Some(4000.0)); // zero-vol day skipped
    assert_eq!(avg_turnover(&[], &[], 30), None);
    assert_eq!(avg_turnover(&[10.0], &[0.0], 30), None); // no usable turnover
    // avg_volume: crypto notional volume used raw (no ×close), zero days skipped
    assert_eq!(avg_volume(&[100.0, 0.0, 300.0], 30), Some(200.0));
    assert_eq!(avg_volume(&[0.0], 30), None);

    // volatility: stdev of daily % returns. Steady +1%/day -> 0 stdev; alternating moves -> >0
    assert!(volatility_pct(&[100.0, 101.0, 102.01, 103.0301], 30).unwrap() < 1e-9); // ~0 (float dust)
    assert!(volatility_pct(&[100.0, 110.0, 100.0, 110.0], 30).unwrap() > 0.0);
    assert_eq!(volatility_pct(&[100.0], 30), None); // too few sessions

    assert_eq!(pct_cell(Some(&("€10.00".to_string(), 5.0))), "+5.0%");
    assert_eq!(pct_cell(None), "n/a");

    // dividends: sum within window; None when history doesn't cover it; EUR via rate
    let ddates = vec![
        NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), // ~2y of history
    ];
    let divs = vec![
        (NaiveDate::from_ymd_opt(2022, 3, 1).unwrap(), 1.0),
        (NaiveDate::from_ymd_opt(2023, 9, 1).unwrap(), 2.0),
    ];
    assert_eq!(dividends_in_window(&divs, &ddates, 365), Some(2.0)); // only the 2023 one is <1y back
    assert_eq!(dividends_in_window(&divs, &ddates, 1825), None); // 5y not covered by ~2y history
    let sums = dividend_sums(&divs, &ddates, Some(2.0)); // rate 2.0 -> EUR
    assert_eq!(sums[0], Some(4.0)); // 1Y: 2.0 * 2.0
    assert_eq!(sums[1], None); // 5Y short history
    // yield: 1Y €4 paid / 1yr / €40 price = 10%; 5Y short -> None; no price -> all None
    let yields = dividend_yields(&sums, Some(40.0));
    assert!((yields[0].unwrap() - 10.0).abs() < 1e-9);
    assert_eq!(yields[1], None);
    assert_eq!(dividend_yields(&sums, None)[0], None);

    assert_eq!(name_of(&serde_json::json!({"shortName": "Apple Inc."}), "AAPL"), "Apple Inc.");
    assert_eq!(name_of(&serde_json::json!({"longName": "NVIDIA Corp"}), "NVDA"), "NVIDIA Corp");
    assert_eq!(name_of(&serde_json::json!({}), "BTC-USD"), "BTC-USD");
    assert_eq!(name_of(&Value::Null, "MSFT"), "MSFT");
    // both present -> longName wins (the real ETF name, not the truncated registrant shortName)
    assert_eq!(
        name_of(&serde_json::json!({"shortName": "ISHARES III PLC", "longName": "iShares Core MSCI World UCITS ETF"}), "IWDA.L"),
        "iShares Core MSCI World UCITS ETF"
    );

    assert_eq!(ca_base_rate(2.1, 0.0, 2.5), 2.1); // Série F
    assert!((ca_base_rate(2.1, 1.0, 3.5) - 3.1).abs() < 1e-9); // Série E
    assert_eq!(ca_base_rate(3.0, 1.0, 3.5), 3.5); // capped
    assert_eq!(ca_base_rate(-2.0, 1.0, 3.5), 0.0); // floored

    // CA cumulative gain: yr1 = base only; compounds with premium thereafter
    assert!((ca_cumulative_gain(2.1, 0.25, 0.50, 1) - 2.1).abs() < 1e-9); // yr1 = base
    // 2yr: (1.021)(1.0235) - 1 = 0.0449935 -> 4.49935%
    assert!((ca_cumulative_gain(2.1, 0.25, 0.50, 2) - 4.49935).abs() < 1e-4);
    assert_eq!(ca_cumulative_gain(2.0, 0.5, 1.0, 0), 0.0); // no holding -> no gain

    let series: BTreeMap<i32, f64> = [(2018, 1.0), (2019, 2.0), (2020, 3.0)].into();
    let (ly, lv, a10, a30) = inflation_summary(&series);
    assert_eq!(ly, Some(2020));
    assert_eq!(lv, Some(3.0));
    assert!((a10.unwrap() - 2.0).abs() < 1e-9 && (a30.unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(inflation_summary(&BTreeMap::new()), (None, None, None, None));

    // compounded: last 2 = (1.02)(1.03)-1 = 5.06%; exactly-len = full product
    assert!((inflation_compounded(&series, 2).unwrap() - 5.06).abs() < 1e-9);
    assert!((inflation_compounded(&series, 3).unwrap() - (1.01 * 1.02 * 1.03 - 1.0) * 100.0).abs() < 1e-9);
    // 1yr slack (level->YoY always loses the earliest in-window year): a 4Y ask off 3 rates renders;
    // >=2 short -> None, so a far-too-long horizon isn't faked from a short span
    assert!((inflation_compounded(&series, 4).unwrap() - (1.01 * 1.02 * 1.03 - 1.0) * 100.0).abs() < 1e-9);
    assert_eq!(inflation_compounded(&series, 5), None); // 3 rates, ask 5 -> n/a
    assert_eq!(inflation_compounded(&series, 10), None);
    assert_eq!(inflation_compounded(&BTreeMap::new(), 5), None);

    // BLS CPI-U parse: index level -> YoY %. 2025 = (103/100-1)*100 = 3%; 2024 has no 2023
    // pair -> absent; M13 (annual avg) skipped without crashing.
    let bls = serde_json::json!({"Results":{"series":[{"data":[
        {"year":"2024","period":"M12","value":"100.0"},
        {"year":"2025","period":"M12","value":"103.0"},
        {"year":"2025","period":"M13","value":"999.0"}
    ]}]}});
    let us = parse_bls_cpi(&bls);
    assert!((us[&2025] - 3.0).abs() < 1e-9);
    assert!(!us.contains_key(&2024)); // no prior-year same month to compare
    assert_eq!(parse_bls_cpi(&serde_json::json!({})), BTreeMap::new());

    assert_eq!(fmt_duration(Duration::days(14)), "2w");
    assert_eq!(fmt_duration(Duration::days(400)), "1Y");
    assert_eq!(fmt_duration(Duration::seconds(90)), "1m");

    let dd = vec![
        NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
    ];
    assert_eq!(trend_streak(&dd, &[10.0, 11.0, 12.0]), ("↑", "2d".to_string(), 2));
    assert_eq!(trend_streak(&dd, &[12.0, 11.0, 10.0]).0, "↓");
    assert_eq!(trend_streak(&dd, &[10.0, 10.0, 10.0]), ("→", "0s".to_string(), 0));
    assert_eq!(trend_streak(&dd[..1], &[10.0]), ("→", "0s".to_string(), 0));

    assert_eq!(extreme_flags(&[1.0, 2.0, 3.0], 0.001), (true, false));
    assert_eq!(extreme_flags(&[3.0, 2.0, 1.0], 0.001), (false, true));
    assert_eq!(extreme_flags(&[2.0, 1.0, 3.0, 2.0], 0.001), (false, false));
    assert_eq!(extreme_flags(&[], 0.001), (false, false));

    // money formatting (Python {:,.2f})
    assert_eq!(fmt_money2(1234567.5), "1,234,567.50");
    assert_eq!(fmt_money2(12.3), "12.30");

    // PT inflation parse: BPstat index is a JSON ARRAY (the bug was only handling objects)
    let pt = serde_json::json!({
        "dimension": {"reference_date": {"category": {"index": ["2024-11-30", "2024-12-31", "2025-01-31"]}}},
        "value": [2.1, 2.4, 2.6]
    });
    let s = parse_pt_series(&pt);
    assert_eq!(s.get(&2024), Some(&2.4)); // last month of 2024 wins
    assert_eq!(s.get(&2025), Some(&2.6));
    // object-form index (sorted by position) still works
    let pt_obj = serde_json::json!({
        "dimension": {"reference_date": {"category": {"index": {"2024-12-31": 1, "2024-11-30": 0}}}},
        "value": [2.1, 2.4]
    });
    assert_eq!(parse_pt_series(&pt_obj).get(&2024), Some(&2.4));
    assert!(parse_pt_series(&Value::Null).is_empty());
    }
}
