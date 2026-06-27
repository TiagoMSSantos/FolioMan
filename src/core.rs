//! Pure logic for folioman: types, formatting, market/trend/inflation math.
//! No network here — all I/O lives in `fetch.rs`. Read-only, never trades.
//! Acronyms (CAGR, NUPL, SMA, GICS, R², CdA, …): see the Glossary in README.md.

use chrono::{Duration, NaiveDate};
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
    pub drawdown_pct: f64, // % below the high of the last ~high_days (picks "on sale" signal)
    pub intraday: [Option<f64>; 3], // % change over [1h, 6h, 12h] = 1/6/12 hourly bars back; None if too few bars
    pub avg_turnover_eur: Option<f64>, // avg daily turnover (close*volume, EUR) ~last 30 sessions; liquidity proxy
    pub volatility_pct: Option<f64>,   // daily-return stdev (%) ~last year; the asset's "normal swing" for the picks score
    pub below_ma_pct: f64,             // % below the ~200-week SMA (structural "cheap vs long trend"); 0 if at/above or history too short
    pub above_ma_pct: f64,             // % ABOVE the ~200-week SMA (overextension "how far it ran"); 0 if at/below or history too short. Growth-lane brake on blow-off tops
    pub pe_ratio: Option<f64>,         // trailing P/E for the valuation tilt; None for crypto/ETF/no-earnings/no source (-> neutral)
    pub roe: Option<f64>,              // (F) trailing return-on-equity (%) — the core profitability/QUALITY factor; None for crypto/ETF/no-earnings/no source (-> neutral). BACKTEST-BLIND (point-in-time, can't reconstruct as-of)
    pub range_pct: f64,                // percentile rank (0..100) of the last close in its own ~10y history; 100=at high. picks discount = 100-this
    pub trend_r2: f64,                 // (A) R² (0..1) of the log-price trend — how steadily it compounds; damps CAGR endpoint-luck. 0 = no/short history
    pub max_drawdown_pct: f64,         // (C) worst peak-to-trough decline (%) in its history; feeds the Calmar (return-per-pain) reward. 0 = never down/no history
    pub fund_factor: Option<f64>,      // (G) the ONE as-of fundamental factor folded into growth_score (e.g. revenue accel). Set in the backtest (from fund_factors) so the term is ablatable, and live only on the small/check-scale path; None -> neutral (universe screen & price-only backtest)
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
            drawdown_pct: 0.0,
            intraday: [None; 3],
            avg_turnover_eur: None,
            volatility_pct: None,
            below_ma_pct: 0.0,
            above_ma_pct: 0.0,
            pe_ratio: None,
            roe: None,
            range_pct: 0.0,
            trend_r2: 0.0,
            max_drawdown_pct: 0.0,
            fund_factor: None,
        }
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
    f64::max(0.0, (ma - *closes.last().unwrap()) / ma * 100.0)
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
    f64::max(0.0, (*closes.last().unwrap() - ma) / ma * 100.0)
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

/// Percentile rank (0..100) of the LAST close within the asset's OWN fetched history: ~0 = at its
/// all-time low, ~100 = at its all-time high. Self-normalizing across assets of wildly different
/// amplitude (BTC vs a penny alt) and robust to a single blow-off top — it's a rank, not a linear
/// (max−last)/(max−min) range one spike would distort. The buy "discount" uses 100−this (how deep in
/// its own history it trades). 0 for empty/one-point history.
pub fn price_pct_rank(closes: &[f64]) -> f64 {
    let last = match closes.last() {
        Some(&x) => x,
        None => return 0.0,
    };
    if closes.len() < 2 {
        return 0.0;
    }
    let below = closes.iter().filter(|&&c| c < last).count();
    below as f64 / (closes.len() - 1) as f64 * 100.0
}

/// % latest price sits below the period high. 0 if at/above high.
pub fn pct_from_high(prices: &[f64]) -> f64 {
    let high = prices.iter().cloned().fold(f64::MIN, f64::max);
    let last = *prices.last().unwrap();
    f64::max(0.0, (high - last) / high * 100.0)
}

/// Country/market from the ticker suffix. Crypto = global; no suffix = USA.
pub fn market_of(ticker: &str) -> String {
    if ticker.contains('.') {
        let suf = ticker.rsplit('.').next().unwrap().to_uppercase();
        return suffix_country(&suf).unwrap_or(&suf).to_string();
    }
    if ticker.contains('-') {
        return "Crypto (global)".to_string();
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

/// Parse one S&P-500 constituents CSV row -> Yahoo symbol, keeping it only if its GICS sector (col 2)
/// passes the `sectors` filter (empty = all sectors). Columns: Symbol, Security, "GICS Sector", ... —
/// Symbol (0) and Sector (2) carry no commas in this dataset, so a naive split is enough.
pub fn sector_symbol(csv_line: &str, sectors: &[String]) -> Option<String> {
    let cols: Vec<&str> = csv_line.splitn(4, ',').collect();
    let sym = cols.first()?.trim();
    let sector = cols.get(2)?.trim();
    if sym.is_empty() || !sector_matches(sector, sectors) {
        return None;
    }
    Some(sym.replace('.', "-")) // BRK.B -> BRK-B (Yahoo form)
}

/// Parse a NASDAQ Trader SymDir file (pipe-delimited) -> ETF symbols (Yahoo form): keep col-0 symbols
/// whose `etf_col` flag is "Y". Skips the header row and the "File Creation Time" footer; drops
/// `$`-bearing (preferred/test) tickers. ETFs span two files with different layouts, hence the
/// column-index arg (nasdaqlisted = 6, otherlisted = 4). No free AUM rank exists, so this is ALL of
/// them — the turnover gate culls the illiquid tail after fetch.
pub fn etf_symbols(text: &str, etf_col: usize) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter(|l| !l.starts_with("File Creation Time"))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            let sym = f.first()?.trim();
            if f.get(etf_col)?.trim() != "Y" || sym.is_empty() || sym.contains('$') {
                return None;
            }
            Some(sym.replace('.', "-"))
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
    let last = *years.last().unwrap();
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
    let cutoff = *dates.last().unwrap() - Duration::days(days);
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
    let cur = *closes.last().unwrap();
    let last = *dates.last().unwrap();
    HORIZONS
        .iter()
        .map(|(label, days)| {
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
    pub revenue: Option<f64>,
    pub gross_margin: Option<f64>,    // % = grossProfit/revenue
    pub op_margin: Option<f64>,       // % = operatingIncome/revenue
    pub net_margin: Option<f64>,      // % = netIncome/revenue
    pub eps: Option<f64>,
    pub roe: Option<f64>,             // % — PREMIUM (key-metrics/ratios), None on free tier
    pub roic: Option<f64>,            // % — PREMIUM
    pub net_debt_ebitda: Option<f64>, // ratio, lower=safer — PREMIUM
    pub fcf_ps: Option<f64>,          // free cash flow / share — PREMIUM
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
    FundFactors {
        rev_cagr,
        rev_accel,
        gross_margin: now.and_then(|r| r.gross_margin),
        op_margin: now.and_then(|r| r.op_margin),
        margin_trend,
        eps_growth,
    }
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
    quote.max_drawdown_pct = max_drawdown_pct(c);
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

/// Signed % from a horizon entry; "n/a" if missing.
pub fn pct_cell(entry: Option<&(String, f64)>) -> String {
    match entry {
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
    let span = *dates.last().unwrap() - dates[i];
    (arrow, fmt_duration(span), span.num_days())
}

/// (at_all_time_high, at_all_time_low): latest close within tol of the max/min seen.
/// 'All-time' = the fetched history window (Yahoo range=max).
pub fn extreme_flags(closes: &[f64], tol: f64) -> (bool, bool) {
    if closes.is_empty() {
        return (false, false);
    }
    let last = *closes.last().unwrap();
    let hi = closes.iter().cloned().fold(f64::MIN, f64::max);
    let lo = closes.iter().cloned().fold(f64::MAX, f64::min);
    (last >= hi * (1.0 - tol), last <= lo * (1.0 + tol))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        assert_eq!(select_fund_factor(&f, "rev_accel"), Some(2.0));
        assert_eq!(select_fund_factor(&f, "margin_trend"), Some(5.0));
        assert_eq!(select_fund_factor(&f, "eps_growth"), Some(6.0));
        assert_eq!(select_fund_factor(&f, "rev_cagr"), Some(1.0));
        assert_eq!(select_fund_factor(&f, "nope"), None); // unknown -> neutral, never panics
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
    assert_eq!(market_of("BTC-USD"), "Crypto (global)");
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
    assert_eq!(sector_symbol("AAPL,Apple Inc.,Information Technology,Tech HW,x,y", &tech), Some("AAPL".to_string()));
    assert_eq!(sector_symbol("GOOGL,Alphabet,Communication Services,x", &tech), Some("GOOGL".to_string()));
    assert_eq!(sector_symbol("BF.B,Brown-Forman,Information Technology,x", &tech), Some("BF-B".to_string())); // '.'->'-'
    assert_eq!(sector_symbol("MMM,3M,Industrials,x", &tech), None);
    assert_eq!(sector_symbol("AMZN,Amazon,Consumer Discretionary,x", &tech), None); // GICS quirk: not tech
    assert_eq!(sector_symbol("MMM,3M,Industrials,x", &[]), Some("MMM".to_string())); // empty filter -> all sectors
    // etf_symbols: keep ETF flag = Y at the given column, skip header/footer/$/non-ETF; '.'->'-'
    let nasdaq = "Symbol|Name|Cat|Test|Fin|Lot|ETF|NS\nQQQ|Invesco QQQ|Q|N|N|100|Y|N\nAAPL|Apple|Q|N|N|100|N|N\nFOO$|x|Q|N|N|100|Y|N\nFile Creation Time: now";
    assert_eq!(etf_symbols(nasdaq, 6), vec!["QQQ".to_string()]); // AAPL=N dropped, FOO$ dropped, footer skipped
    let other = "ACT Symbol|Name|Exch|CQS|ETF|Lot|Test|NASDAQ\nSPY|SPDR S&P 500|P|SPY|Y|100|N|SPY\nBRK.A|Berkshire|N|BRK.A|N|1|N|";
    assert_eq!(etf_symbols(other, 4), vec!["SPY".to_string()]); // BRK.A is ETF=N -> dropped
    // euronext_lisbon_symbols: symbol at row index 2 -> `<SYM>.LS`; odd/empty cells dropped; no aaData -> []
    let es = serde_json::json!({"aaData": [
        ["<a href=x>GALP ENERGIA</a>", "PTGAL0AM0009", "GALP", "<div>XLIS</div>", "EUR 18", "0.1%", "12:00"],
        ["<a>EDP</a>", "PTEDP0AM0009", "EDP", "<div>XLIS</div>", "EUR 3", "-0.2%", "12:00"],
        ["bad", "isin", "", "x", "y", "z", "w"],   // empty symbol -> dropped
        ["bad", "isin", "FOO BAR", "x", "y", "z", "w"], // non-alnum symbol -> dropped
    ]});
    assert_eq!(euronext_lisbon_symbols(&es), vec!["GALP.LS".to_string(), "EDP.LS".to_string()]);
    assert!(euronext_lisbon_symbols(&serde_json::json!({})).is_empty()); // no aaData -> empty leg, not a crash
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
        FundRow { filed: NaiveDate::from_ymd_opt(2022, 2, 1).unwrap(), revenue: Some(200.0), ..Default::default() },
        FundRow { filed: NaiveDate::from_ymd_opt(2020, 2, 1).unwrap(), revenue: Some(100.0), ..Default::default() },
        FundRow { filed: NaiveDate::from_ymd_opt(2021, 2, 1).unwrap(), revenue: Some(150.0), ..Default::default() },
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
