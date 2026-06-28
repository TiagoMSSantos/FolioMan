//! All network I/O for folioman. One shared `reqwest::Client` (keep-alive pool,
//! HTTP/2, gzip) drives every request; fan-out is async via `join_all`. Every fetch
//! fails soft — a bad ticker or a down API yields a fallback/err row, never a crash.
//! Universe-scale fan-out is concurrency-bounded (`FETCH_CONCURRENCY`) AND rate-paced (`throttle`,
//! `fetch_requests_per_second`) so launches stay spaced and it can't 429-storm.
//! All URLs come from config (`Urls`); templates use `{ticker}`/`{range}`/`{topic}`.

use crate::config::Urls;
use crate::core::{
    self, asof, extreme_flags, headline_titles, horizon_changes, market_of, name_of,
    pct_from_high, slice_since, trend_streak, Quote,
};
use chrono::{DateTime, NaiveDate};
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant, SystemTime};
use tokio::sync::Mutex;

/// Currency -> EUR rate cache (None = no FX pair found, cached to avoid re-hitting).
pub type FxCache = Arc<Mutex<HashMap<String, Option<f64>>>>;

/// One shared client: connection pooling, HTTP/2 multiplexing, gzip, bounded timeouts.
pub fn client() -> Client {
    Client::builder()
        .user_agent("Mozilla/5.0")
        // 8s, not 3s: the 10y daily payload (~3652 pts) is ~25× the old monthly `max` body, and
        // under the screen fan-out a tight 3s budget timed out big coins (BTC) into err stubs.
        .timeout(StdDuration::from_secs(15))
        .connect_timeout(StdDuration::from_secs(3))
        .gzip(true)
        .build()
        .expect("failed to build HTTP client")
}

pub fn fx_cache() -> FxCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Client for broker order APIs: longer timeout (orders aren't snappy quotes) and a cookie
/// store (Trade Republic's login hands back a session cookie). Separate from `client()` so
/// the read-only quote path stays on its tighter 8s budget.
pub fn client_long() -> Client {
    Client::builder()
        .user_agent("Mozilla/5.0")
        .timeout(StdDuration::from_secs(15))
        .connect_timeout(StdDuration::from_secs(3))
        .cookie_store(true)
        .gzip(true)
        .build()
        .expect("failed to build order HTTP client")
}

async fn get_json(client: &Client, url: &str) -> Option<Value> {
    throttle().await;
    client.get(url).send().await.ok()?.json::<Value>().await.ok()
}

async fn get_text(client: &Client, url: &str) -> Option<String> {
    throttle().await;
    client.get(url).send().await.ok()?.text().await.ok()
}

async fn post_json(client: &Client, url: &str, body: &Value) -> Option<Value> {
    throttle().await;
    client.post(url).json(body).send().await.ok()?.json::<Value>().await.ok()
}

/// Global outbound-request pacer. The concurrency cap bounds how many requests are *in flight*, but
/// nothing stopped 64 of them launching in the same millisecond — that burst is what Yahoo 429s into
/// err stubs (forcing the reactive re-fetch pass). This proactively spaces request *launches* ≥
/// `min_interval` apart: a single shared "next free slot" instant that each request claims-and-bumps
/// under a short lock, then releases the lock and sleeps until its slot. N concurrent tasks therefore
/// leave the gate evenly spaced at the configured rate instead of all at once. Rate from config
/// (`fetch_requests_per_second`); 0 disables it. note: one Mutex<Instant>, no token-bucket crate.
static THROTTLE: std::sync::OnceLock<(Mutex<Instant>, StdDuration)> = std::sync::OnceLock::new();

/// Pure slot math (testable, no clock/lock): claim the launch instant for a request arriving at `now`
/// against gate `next`, returning (launch, new_next). Launch is never in the past, and successive
/// claims are forced ≥ `interval` apart.
fn claim_slot(next: Instant, now: Instant, interval: StdDuration) -> (Instant, Instant) {
    let launch = next.max(now);
    (launch, launch + interval)
}

async fn throttle() {
    let (gate, interval) = THROTTLE.get_or_init(|| {
        let rps = crate::config::load().fetch_requests_per_second;
        let interval = if rps > 0.0 {
            StdDuration::from_secs_f64(1.0 / rps)
        } else {
            StdDuration::ZERO
        };
        (Mutex::new(Instant::now()), interval)
    });
    if interval.is_zero() {
        return;
    }
    let launch = {
        let mut next = gate.lock().await;
        let (launch, new_next) = claim_slot(*next, Instant::now(), *interval);
        *next = new_next; // claim the slot, then drop the lock BEFORE sleeping (else fully serialized)
        launch
    };
    let wait = launch.saturating_duration_since(Instant::now());
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

struct Chart {
    dates: Vec<NaiveDate>,
    closes: Vec<f64>,
    volumes: Vec<f64>, // parallel to closes (0.0 where no volume reported); liquidity proxy
    currency: String,
    name: String,
    instrument_type: String, // Yahoo meta.instrumentType ("ETF"/"EQUITY"/...); "" if absent
    divs: Vec<(NaiveDate, f64)>, // (ex-date, amount/share) from events.dividends
}

async fn chart_json(client: &Client, urls: &Urls, ticker: &str, range: &str) -> Option<Value> {
    let url = urls.yahoo_chart.replace("{ticker}", ticker).replace("{range}", range);
    // Fallback retry. The global `throttle()` pacer now spaces launches so the fan-out shouldn't 429 in
    // the first place; this stays as cheap insurance for an isolated timeout / transient rate-limit body
    // (parses to JSON but carries no chart.result). Both drop the name to an err stub that then vanishes
    // from the universe (NVDA disappeared this way). note: fixed 400ms, one extra try — rarely fires
    // now that requests are paced.
    for attempt in 0..2 {
        if let Some(v) = get_json(client, &url).await {
            if v.pointer("/chart/result/0/timestamp").is_some() {
                return Some(v);
            }
        }
        if attempt == 0 {
            tokio::time::sleep(StdDuration::from_millis(400)).await;
        }
    }
    None
}

/// Full-history chart at MONTHLY resolution. Yahoo coarsens interval=1d to ~monthly bars once the
/// span passes ~10y anyway (which silently breaks 1D/1W/1M), so ask for 1mo explicitly and use this
/// ONLY to back-fill horizons older than the ~10y daily window (the 20Y column / long dividend sums).
/// Short/mid horizons, turnover, SMA, range and R² all stay on the precise daily series.
async fn chart_json_long(client: &Client, urls: &Urls, ticker: &str) -> Option<Value> {
    let url = urls
        .yahoo_chart
        .replace("{ticker}", ticker)
        .replace("{range}", "max")
        .replace("interval=1d", "interval=1mo");
    get_json(client, &url).await
}

/// Parse a Yahoo chart payload into aligned dates+closes (null closes dropped) + meta.
fn parse_chart(j: &Value, ticker: &str) -> Option<Chart> {
    let result = j.get("chart")?.get("result")?.get(0)?;
    let ts = result.get("timestamp")?.as_array()?;
    let quote0 = result.get("indicators")?.get("quote")?.get(0)?;
    // (Item 21) PROBE: when the flag is on, prefer Yahoo's adjclose (split+DIVIDEND adjusted) so every
    // price signal (long CAGR, range_pct near-high gate, drawdown, overext brake) measures TOTAL return,
    // not price-only. Same parse site for live + backtest -> no train-serve skew. Crypto/FX have no
    // adjclose -> falls back to raw close (no effect). Length parallels close; the ts.zip below truncates
    // safely if Yahoo ever returns a short array.
    let raw_closes = quote0.get("close")?.as_array()?;
    let closes_arr = if crate::config::use_adjusted_close() {
        result.pointer("/indicators/adjclose/0/adjclose").and_then(|v| v.as_array()).unwrap_or(raw_closes)
    } else {
        raw_closes
    };
    let vols_arr = quote0.get("volume").and_then(|v| v.as_array());
    let meta = result.get("meta").cloned().unwrap_or(Value::Null);

    let mut dates = Vec::new();
    let mut closes = Vec::new();
    let mut volumes = Vec::new();
    for (i, (t, c)) in ts.iter().zip(closes_arr).enumerate() {
        if let (Some(secs), Some(close)) = (t.as_i64(), c.as_f64()) {
            if let Some(dt) = DateTime::from_timestamp(secs, 0) {
                dates.push(dt.date_naive());
                closes.push(close);
                // volume parallel to closes; missing/null -> 0.0 (avg_turnover skips those days)
                volumes.push(vols_arr.and_then(|a| a.get(i)).and_then(|v| v.as_f64()).unwrap_or(0.0));
            }
        }
    }
    let currency = meta
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("USD")
        .to_string();

    // events.dividends = { "<ts>": {"amount": x, "date": ts}, ... } (present via events=div)
    let mut divs: Vec<(NaiveDate, f64)> = result
        .pointer("/events/dividends")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.values()
                .filter_map(|d| {
                    let amount = d.get("amount")?.as_f64()?;
                    let secs = d.get("date")?.as_i64()?;
                    Some((DateTime::from_timestamp(secs, 0)?.date_naive(), amount))
                })
                .collect()
        })
        .unwrap_or_default();
    divs.sort_by_key(|(d, _)| *d);

    Some(Chart {
        dates,
        closes,
        volumes,
        currency,
        name: name_of(&meta, ticker),
        // Yahoo's own asset-class tag — reliable where the name string isn't (ETF shortNames like
        // "ISHARES III PLC ISHRS CORE MSCI" carry no "ETF"/"UCITS" marker). Drives the ETF table split.
        instrument_type: meta.get("instrumentType").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        divs,
    })
}

/// Intraday hourly closes (chronological) from the configured intraday chart URL. None on
/// failure; null closes dropped. Used only for the 1h/6h/12h columns (bar-count, not time).
async fn intraday_closes(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<f64>> {
    let url = urls.yahoo_intraday.replace("{ticker}", ticker);
    let j = get_json(client, &url).await?;
    let closes = j
        .get("chart")?.get("result")?.get(0)?
        .get("indicators")?.get("quote")?.get(0)?.get("close")?.as_array()?;
    Some(closes.iter().filter_map(|c| c.as_f64()).collect())
}

async fn fetch_news(client: &Client, urls: &Urls, ticker: &str) -> Vec<String> {
    let url = urls.yahoo_search.replace("{ticker}", ticker);
    let items: Vec<Value> = get_json(client, &url)
        .await
        .and_then(|j| j.get("news").and_then(|n| n.as_array()).cloned())
        .unwrap_or_default();
    headline_titles(&items).into_iter().take(3).collect()
}

async fn last_close(client: &Client, urls: &Urls, symbol: &str) -> Option<f64> {
    let j = chart_json(client, urls, symbol, "5d").await?;
    let arr = j
        .get("chart")?
        .get("result")?
        .get(0)?
        .get("indicators")?
        .get("quote")?
        .get(0)?
        .get("close")?
        .as_array()?;
    arr.iter().rev().find_map(|v| v.as_f64())
}

/// EUR per 1 unit of `cur`. 1.0 for EUR; None if Yahoo has no FX pair. Cached.
pub async fn eur_rate(client: &Client, urls: &Urls, cur: &str, cache: &FxCache) -> Option<f64> {
    let cur = if cur.is_empty() { "EUR".to_string() } else { cur.to_uppercase() };
    if cur == "EUR" {
        return Some(1.0);
    }
    if let Some(v) = cache.lock().await.get(&cur) {
        return *v;
    }
    let mut rate = None;
    for (sym, invert) in [(format!("{cur}EUR=X"), false), (format!("EUR{cur}=X"), true)] {
        if let Some(px) = last_close(client, urls, &sym).await {
            rate = Some(if invert { 1.0 / px } else { px });
            break;
        }
    }
    cache.lock().await.insert(cur, rate); // cache misses too
    rate
}

/// Fetch a single Quote. Self-swallows failures: a bad ticker yields an "err"/"no data"
/// row instead of killing the batch. Chart + news are fetched concurrently.
pub async fn quote_one(client: &Client, urls: &Urls, fx_cache: &FxCache, ticker: &str, dip_days: i64, high_days: i64, intraday: bool, news: bool, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Quote {
    let (chart_j, chart_long_j, titles, intra) = tokio::join!(
        // 10y, NOT max: Yahoo coarsens interval=1d to monthly bars once the span passes ~10y, which
        // makes 1D/1W/1M meaningless (only month-boundary points exist). 10y keeps TRUE daily bars
        // (~3652) for 1D..10Y, plus turnover/SMA/range/R². The pre-10y span (the 20Y column) is
        // back-filled from the separate monthly series below.
        chart_json(client, urls, ticker, "10y"),
        // monthly full history, ONLY to back-fill the >10y horizons (20Y) without breaking the daily ones.
        chart_json_long(client, urls, ticker),
        // news headlines are displayed ONLY by `check`/`alert`; `screen`/`perf` ignore them, so skip the
        // per-name Yahoo search there (~25% fewer requests across a 3800-name screen -> proportionally faster).
        async { if news { fetch_news(client, urls, ticker).await } else { Vec::new() } },
        async { if intraday { intraday_closes(client, urls, ticker).await } else { None } },
    );

    let parsed = chart_j.as_ref().and_then(|j| parse_chart(j, ticker));
    let chart = match parsed {
        Some(c) if !c.closes.is_empty() => c,
        other => {
            // Crypto -EUR with no Yahoo data: many alts (APT, SUI, NEAR…) only carry a -USD pair on
            // Yahoo, not -EUR. Retry once in USD before gating it out — the price still renders in €
            // via the USD->EUR fx_cache rate, and dedup keys on the underlying so the -USD leg slots in
            // cleanly. note: boxed recursion for the single retry; -USD can't re-trigger this.
            if let Some(base) = ticker.strip_suffix("-EUR") {
                return Box::pin(quote_one(client, urls, fx_cache, &format!("{base}-USD"), dip_days, high_days, intraday, news, windows, infl)).await;
            }
            return match other {
                Some(c) => Quote::stub(ticker, "no data", "", &c.name),
                None => Quote::stub(ticker, "err", "", ticker),
            };
        }
    };

    // Live fundamentals, gated by ASSET CLASS so a column only fetches where it applies (and we don't
    // waste FMP's 250/day free budget on no-op calls): P/E + ROE for EQUITIES only, expense ratio for
    // ETFs only, nothing for crypto/FX. Each is disk-cached (weekly TTL) + budget-capped, so a wide
    // `screen` can't blow the limit and a daily `check` of your holdings reads free from cache.
    let is_etf = chart.instrument_type.eq_ignore_ascii_case("ETF");
    let is_equity = chart.instrument_type.eq_ignore_ascii_case("EQUITY");
    let (pe, roe) = if is_equity {
        // SEC EDGAR first: free, no key, no daily cap — the reliable P/E+ROE source for US filers (FMP's
        // 250/day US-centric free tier is what left these columns n/a). A non-US filer has no CIK -> SEC
        // None -> fall back to FMP (covers ADRs / foreign listings SEC doesn't).
        match fetch_ratios_sec(client, urls, ticker, *chart.closes.last().unwrap()).await {
            (None, None) => fetch_ratios(client, urls, ticker).await,
            got => got,
        }
    } else {
        (None, None)
    };
    let ter = if is_etf { fetch_expense(client, urls, ticker).await } else { None };

    // Back-fill history older than the ~10y daily window from the monthly series, so the 20Y column and
    // long dividend sums populate for old names. Prepend only the monthly bars that predate the daily
    // window (strictly < the first daily date) then the full daily tail — no overlap. Used ONLY for
    // horizon_changes + dividend_sums; every other metric stays on the precise daily `chart`. Falls back
    // to daily-only (20Y stays n/a) if the monthly fetch failed.
    let cut = chart.dates[0];
    let (long_dates, long_closes, long_divs) =
        match chart_long_j.as_ref().and_then(|j| parse_chart(j, ticker)) {
            Some(lc) => {
                let keep = lc.dates.iter().take_while(|d| **d < cut).count();
                let mut dates = lc.dates[..keep].to_vec();
                let mut closes = lc.closes[..keep].to_vec();
                dates.extend_from_slice(&chart.dates);
                closes.extend_from_slice(&chart.closes);
                let mut divs: Vec<(NaiveDate, f64)> =
                    lc.divs.iter().filter(|(d, _)| *d < cut).cloned().collect();
                divs.extend(chart.divs.iter().cloned());
                (dates, closes, divs)
            }
            None => (chart.dates.clone(), chart.closes.clone(), chart.divs.clone()),
        };

    let cur_close = *chart.closes.last().unwrap();
    let rate = eur_rate(client, urls, &chart.currency, fx_cache).await;
    let price = match rate {
        Some(r) => format!("€{}", core::fmt_money2(cur_close * r)),
        None => format!("{} {}?", core::fmt_money2(cur_close), chart.currency),
    };

    let window = slice_since(&chart.dates, &chart.closes, dip_days);
    let d = if window.is_empty() { 0.0 } else { pct_from_high(&window) };
    // OFF-HI: drawdown off the high (picks "on sale" signal — a real pullback, not the 30d dip
    // which is ~0 for anything making new highs). high_days <= 0 -> anchor on the all-time high over
    // the fetched ~10y history (best for a decades hold: discount vs the proven peak, not last year's);
    // high_days > 0 -> trailing-N-day high.
    let hi_window = if high_days <= 0 {
        chart.closes.clone()
    } else {
        slice_since(&chart.dates, &chart.closes, high_days)
    };
    let drawdown_pct = if hi_window.is_empty() { 0.0 } else { pct_from_high(&hi_window) };

    let (arrow, dur, _) = trend_streak(&chart.dates, &chart.closes);
    let (at_ath, at_atl) = extreme_flags(&chart.closes, 0.001);

    let last_date = *chart.dates.last().unwrap();
    let month_ago = asof(&chart.dates, &chart.closes, last_date - chrono::Duration::days(30));
    let mom_pct = match month_ago {
        Some(m) if m != 0.0 => Some((cur_close - m) / m * 100.0),
        _ => None,
    };

    Quote {
        ticker: ticker.to_string(),
        price,
        dip: format!("-{:.1}%", d),
        drop_pct: d,
        market: market_of(ticker),
        instrument_type: chart.instrument_type,
        head: titles.first().cloned().unwrap_or_default(),
        news_block: titles.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n"),
        perf: horizon_changes(&long_dates, &long_closes, rate, windows, infl),
        name: chart.name,
        trend: format!("{arrow} {dur}"),
        at_ath,
        at_atl,
        mom_pct,
        div_eur: core::dividend_sums(&long_divs, &long_dates, rate),
        price_eur: rate.map(|r| cur_close * r),
        close_native: Some(cur_close), // (Item 19) native-currency close for a currency-consistent earnings_yield
        drawdown_pct,
        intraday: intra.map_or([None; 3], |cs| core::intraday_changes(&cs)),
        // avg daily turnover in native currency -> EUR (×rate). Crypto: Yahoo "volume" is already
        // a notional amount, so use it raw (close×volume would double-count). Equities: close×volume.
        avg_turnover_eur: if ticker.contains('-') {
            core::avg_volume(&chart.volumes, 30).map(|v| v * rate.unwrap_or(1.0))
        } else {
            core::avg_turnover(&chart.closes, &chart.volumes, 30).map(|v| v * rate.unwrap_or(1.0))
        },
        // the asset's "normal" daily swing (~1 trading year) so picks can tell a deep-for-this-asset
        // dip from everyday noise; a ratio of returns, so no FX conversion needed.
        volatility_pct: core::volatility_pct(&chart.closes, 252),
        // (C) % below the ~200wk SMA — structural "cheap vs long trend" entry signal (FX-agnostic ratio).
        below_ma_pct: core::below_long_ma_pct(&chart.closes, crate::config::LONG_MA_SESSIONS),
        // (1) % ABOVE the ~200wk SMA — overextension brake for the growth lane (far above trend = stretched).
        above_ma_pct: core::above_long_ma_pct(&chart.closes, crate::config::LONG_MA_SESSIONS),
        // (E) trailing P/E (equities w/ FMP_API_KEY only; None -> neutral value tilt).
        pe_ratio: pe,
        // (F) trailing ROE % (equities w/ FMP_API_KEY only; None -> neutral quality tilt).
        roe,
        // (TER) ETF annual expense ratio % (ETFs w/ FMP_API_KEY only; None -> n/a column).
        expense_ratio: ter,
        // (A) percentile rank of today's price in its OWN ~10y history; picks discount = 100-this.
        // Self-normalizes amplitude so BTC-near-its-range-top and a deep alt don't both peg the cap.
        range_pct: core::price_pct_rank(&chart.closes),
        // (A/C) zero-extra-fetch quality scalars from the SAME closes: log-trend R² (steadiness) and
        // worst historical drawdown (pain). Feed the consistency multiplier + Calmar reward.
        trend_r2: core::trend_r2(&chart.closes),
        // (#14) annualized log-trend slope CAGR (endpoint-robust); daily live bars -> cadence 252,
        // matching backtest_quote's per-cadence call so the live rank and the validation agree.
        trend_cagr: core::trend_cagr(&chart.closes, 252),
        max_drawdown_pct: core::max_drawdown_pct(&chart.closes),
        fund_factor: None, // (G) live screen leaves this None (neutral); only the small/check-scale path (A3) populates it
    }
}

// LIVE fundamentals (P/E, ROE, expense ratio) are quarterly-ish, so a 1-week cache is plenty fresh and
// keeps the daily `check`/`screen` from re-spending FMP's 250/day free budget on data that rarely moves.
const LIVE_FUND_TTL: StdDuration = StdDuration::from_secs(7 * 24 * 3600);

/// Disk-cached, budget-capped GET of one live-fundamentals JSON object. Serves a <1wk-old cache file for
/// free; on a miss it spends ONE unit of the shared FMP daily budget, fetches, and caches a REAL payload
/// (never an FMP "Limit Reach"/error object — that must not poison the cache). None on no key / over
/// budget / error. This is what stops a wide `screen` (hundreds of names × P/E+ROE+TER) from blowing the
/// 250/day limit — the exact failure that left every column n/a. `tag` namespaces the cache per endpoint.
async fn cached_fund_json(client: &Client, url_tmpl: &str, ticker: &str, tag: &str) -> Option<Value> {
    use std::sync::atomic::Ordering;
    let path = std::path::Path::new(".fmp_cache").join(format!("live_{tag}_{}.json", ticker.replace(['/', '\\'], "_")));
    evict_if_stale(&path, LIVE_FUND_TTL);
    if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        return Some(v); // cache hit -> no network, no budget spend
    }
    let key = std::env::var("FMP_API_KEY").ok().filter(|k| !k.is_empty())?;
    if FUND_FETCHES.fetch_add(1, Ordering::Relaxed) >= FUND_FETCH_BUDGET {
        return None; // over the daily budget -> degrade to n/a for the rest of this run
    }
    let url = url_tmpl.replace("{ticker}", ticker).replace("{key}", &key);
    let v = get_json(client, &url).await?;
    let real = v.get(0).unwrap_or(&v).get("Error Message").is_none() && !v.is_null();
    if real {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, v.to_string());
    }
    real.then_some(v)
}

/// (E/F) Trailing P/E + ROE for an EQUITY from FMP `stable/ratios-ttm` — ONE cached call serves both.
/// P/E = `priceToEarningsRatioTTM`: the old `stable/quote` source did NOT carry a `pe` field (P/E was
/// always n/a because of it); ratios-ttm does. ROE = `returnOnEquityTTM`, a FRACTION (0.42 -> 42%), ×100.
/// Both None unless FMP_API_KEY is set AND the free-tier endpoint returns them.
/// note: UNVERIFIED field name `priceToEarningsRatioTTM` — confirm once the daily FMP budget resets.
async fn fetch_ratios(client: &Client, urls: &Urls, ticker: &str) -> (Option<f64>, Option<f64>) {
    let Some(v) = cached_fund_json(client, &urls.fundamentals_quality, ticker, "ratios").await else {
        return (None, None);
    };
    let o = v.get(0).unwrap_or(&v);
    let pe = o.get("priceToEarningsRatioTTM").and_then(|x| x.as_f64()).filter(|p| p.is_finite() && *p > 0.0);
    let roe = o.get("returnOnEquityTTM").and_then(|x| x.as_f64()).filter(|r| r.is_finite()).map(|r| r * 100.0);
    (pe, roe)
}

/// (E/F) Trailing P/E + ROE for a US EQUITY from SEC EDGAR — free, no key, no daily cap (unlike FMP).
/// P/E = native-currency close ÷ latest annual diluted EPS (US filers report & trade in USD, so it's
/// currency-consistent); ROE is read straight off the SEC-derived `FundRow`. None for a non-US filer
/// (no CIK) so the caller can fall back to FMP. Filling `pe_ratio` also un-blanks the PEG column, which
/// derives from it downstream. The SEC fetch is itself disk-cached + budget-capped (see
/// `fetch_fundamentals_sec`), so a wide `screen` cold-fetches once then reads free forever.
async fn fetch_ratios_sec(client: &Client, urls: &Urls, ticker: &str, close_native: f64) -> (Option<f64>, Option<f64>) {
    let rows = fetch_fundamentals_sec(client, urls, ticker).await.unwrap_or_default();
    let Some(latest) = rows.last() else {
        return (None, None); // rows are BTreeMap-ordered by period_end -> last = newest fiscal year
    };
    let pe = latest.eps.filter(|e| *e > 0.0).map(|e| close_native / e);
    (pe, latest.roe)
}

/// (TER) ETF annual expense ratio (%) from FMP `stable/etf/info` (`expenseRatio`, a FRACTION -> ×100).
/// Disk-cached + budget-capped via [`cached_fund_json`]. None unless FMP_API_KEY is set AND the symbol is
/// an FMP-covered ETF — FMP's free tier is US-centric, so EU-listed UCITS ETFs (e.g. VUAA.DE) often
/// return nothing and the column stays n/a for them.
/// ponytail: scale is the FMP convention; if a known US ETF prints 100× off, drop the ×100 here.
async fn fetch_expense(client: &Client, urls: &Urls, ticker: &str) -> Option<f64> {
    // Börse Frankfurt TER (captured for free during the universe build) first — it covers the EU UCITS
    // ETFs FMP's US-centric free tier leaves n/a. FMP fallback for US-listed ETFs not in the BF list.
    if let Some(t) = BF_TER.get().and_then(|m| m.get(ticker)).copied() {
        return Some(t);
    }
    let v = cached_fund_json(client, &urls.fund_expense, ticker, "etf").await?;
    let ter = v.get(0).unwrap_or(&v).get("expenseRatio")?.as_f64()?;
    (ter.is_finite() && ter > 0.0).then_some(ter * 100.0)
}

// (G) Cold-fetch budget for the historical-fundamentals lane: FMP free tier = 250 calls/day, so cap
// NEW network fetches per run and serve everything else from the disk cache. note: process-wide
// counter, no cross-run persistence — the disk cache is what actually amortizes the budget over days.
static FUND_FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const FUND_FETCH_BUDGET: usize = 200; // leave headroom under 250/day for the live P/E/ROE calls

fn fund_cache_path(ticker: &str) -> std::path::PathBuf {
    std::path::Path::new(".fmp_cache").join(format!("{}.json", ticker.replace(['/', '\\'], "_")))
}

/// (G) One FMP income-statement row -> FundRow. Margins are derived (FMP's free tier doesn't serve the
/// ratios endpoint), so gross/op/net margin = the matching income line / revenue. `filingDate` (when
/// it went public) is the as-of key — NOT period-end `date`. None if the row lacks a parseable filing.
fn parse_fund_row(v: &Value) -> Option<core::FundRow> {
    let filed = NaiveDate::parse_from_str(v.get("filingDate")?.as_str()?, "%Y-%m-%d").ok()?;
    let revenue = v.get("revenue").and_then(|x| x.as_f64()).filter(|r| *r != 0.0);
    let margin = |field: &str| match (v.get(field).and_then(|x| x.as_f64()), revenue) {
        (Some(n), Some(r)) => Some(n / r * 100.0),
        _ => None,
    };
    // period-end `date` is DISPLAY-ONLY (report's fiscal-year grouping); fall back to `filed` if FMP
    // ever omits it. The as-of join keys on `filed`, so this never affects the backtest.
    let period_end = v
        .get("date")
        .and_then(|x| x.as_str())
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(filed);
    Some(core::FundRow {
        filed,
        period_end,
        revenue,
        gross_margin: margin("grossProfit"),
        op_margin: margin("operatingIncome"),
        net_margin: margin("netIncome"),
        eps: v.get("eps").and_then(|x| x.as_f64()),
        ..Default::default()
    })
}

/// (G) Historical income statements -> as-of FundRows for the backtest's fundamental lane. Sources FMP
/// `stable/income-statement` (free tier; quarterly w/ filingDate + revenue/grossProfit/operatingIncome/
/// netIncome/eps). DISK-CACHED under `.fmp_cache/{ticker}.json`: financial history is append-only, so a
/// cached file is reused forever (only the newest quarter goes stale, which old backtest cutoffs never
/// see) — this + the per-run budget cap is what keeps a wide run under FMP's 250-calls/day free limit.
/// None unless FMP_API_KEY is set AND real rows parse (an error/premium object caches nothing).
/// note: flat-file cache, no TTL; add expiry only if a stale newest-quarter ever matters.
pub async fn fetch_fundamentals_history(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::FundRow>> {
    use std::sync::atomic::Ordering;
    let v = match std::fs::read_to_string(fund_cache_path(ticker)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        Some(v) => v, // cache hit -> no network, no budget spend
        None => {
            let key = std::env::var("FMP_API_KEY").ok().filter(|k| !k.is_empty())?;
            if FUND_FETCHES.fetch_add(1, Ordering::Relaxed) >= FUND_FETCH_BUDGET {
                return None; // over the daily budget -> degrade to price-only for the rest of this run
            }
            let url = urls.fundamentals_history.replace("{ticker}", ticker).replace("{key}", &key);
            let v = get_json(client, &url).await?;
            if v.as_array().is_some_and(|a| !a.is_empty()) {
                let p = fund_cache_path(ticker);
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&p, v.to_string()); // only cache a real array, never an error object
            }
            v
        }
    };
    let rows: Vec<core::FundRow> = v.as_array()?.iter().filter_map(parse_fund_row).collect();
    (!rows.is_empty()).then_some(rows)
}

/// (Item 14) Is a cached file older than `ttl`? Pure. A modified-time in the FUTURE (clock skew) reads
/// NOT stale (`duration_since` errs -> false), so a skewed clock never forces an endless refetch loop.
fn is_stale(modified: SystemTime, now: SystemTime, ttl: StdDuration) -> bool {
    now.duration_since(modified).is_ok_and(|age| age > ttl)
}

/// (Item 14) LIVE-path freshness: delete a cached file older than `ttl` so the next read refetches. The
/// backtest never calls this (its as-of historical quarters never go stale); only the live enrich does.
fn evict_if_stale(path: &std::path::Path, ttl: StdDuration) {
    if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) {
        if is_stale(modified, SystemTime::now(), ttl) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// (G) Populate the LIVE quotes' `fund_factor` from the config-selected as-of fundamental, so the
/// validated fundamental tilt re-ranks `screen`/`check` — not just the backtest. Reuses the disk-cached
/// `fetch_fundamentals_ranked` (Item 22: same `fund_source` the backtest uses — FMP or SEC) and the SAME
/// `fund_factors`/`select_fund_factor` path the backtest validates, so the live and tested signal can't drift. A '-' ticker (crypto/FX) or a
/// name with no statements stays neutral (None). Caller gates on growth_fund_weight > 0, so the default
/// config never pays the fetch. note: ~5y lookback to match the backtest's default forward `years`.
///
/// (Item 13) When the selected factor IS the insider lane (`insider_net_buys_90d`), ALSO pull the shipped
/// Form-4 data and merge `insider_net_buys` onto the as-of factors before selecting — without this the live
/// screen would score that factor `None` (the FMP path never touches SEC), so a backtest-validated insider
/// tilt could never reach what you buy. Works even with no FMP key (insider stands alone). (Item 16) The
/// `composite` factor deliberately does NOT trigger the SEC pull here: the validated `backtest … fund`
/// composite is FMP-only, so adding insider live would be a train-serve skew (score on a blend you never
/// tested). Want insider in the composite? Validate it in the backtest first, then re-add here. (Item 14)
/// Both caches get a LIVE freshness gate so a screen run weeks later doesn't rank on stale fundamentals.
pub async fn enrich_fund_factor(client: &Client, urls: &Urls, quotes: &mut [core::Quote], factor: &str) {
    const LIVE_TTL: StdDuration = StdDuration::from_secs(7 * 24 * 3600); // refetch weekly -> catches new filings
    let today = chrono::Local::now().date_naive();
    let needs_insider = factor == "insider_net_buys_90d"; // (Item 16) composite stays FMP-only (no skew)
    for q in quotes.iter_mut() {
        if q.ticker.contains('-') {
            continue; // crypto/FX -> no income statement, don't spend a budget slot probing
        }
        evict_if_stale(&fund_cache_path(&q.ticker), LIVE_TTL); // (Item 14) drop a stale newest-quarter
        let mut ff = fetch_fundamentals_ranked(client, urls, &q.ticker)
            .await
            .map(|rows| core::fund_factors(&rows, today, 5))
            .unwrap_or_default(); // no rows/key -> empty factors; insider can still fill in below
        if needs_insider {
            evict_if_stale(&sec_cache_path(&q.ticker), LIVE_TTL);
            if let Some(txns) = fetch_insider_history(client, urls, &q.ticker).await {
                ff.insider_net_buys_90d = core::insider_net_buys(&txns, today, 90);
            }
        }
        // (Item 19) as-of earnings yield from the NATIVE close (currency-consistent with native EPS),
        // mirroring the backtest's `f.earnings_yield = earnings_yield(eps_ttm, closes[i])` so the live
        // and validated valuation share one definition. None when there's no EPS (crypto/ETF/no source)
        // or no native price -> select_fund_factor("earnings_yield") then stays neutral. Inert unless
        // `growth_fund_factor: earnings_yield` is set; the caller already gates on growth_fund_weight > 0.
        ff.earnings_yield = q.close_native.and_then(|p| core::earnings_yield(ff.eps_ttm, p));
        q.fund_factor = core::select_fund_factor(&ff, factor);
    }
}

// ── (Item 4) SEC EDGAR insider (Form 4) lane ───────────────────────────────────────────────────────
// Inert by default: only reached under `backtest … insider`, factor off unless `growth_fund_factor:
// insider_net_buys_90d`. Free, no key, but SEC fair-access needs a descriptive User-Agent (config).
static SEC_FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const SEC_FETCH_BUDGET: usize = 600; // per-run cap so one wide backtest can't hammer SEC's ~10 req/s
const SEC_FORM4_CAP: usize = 40; // per ticker: only the N newest Form 4 XML docs (older cutoffs may miss)
static CIK_MAP: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();

fn sec_cache_path(ticker: &str) -> std::path::PathBuf {
    std::path::Path::new(".sec_cache").join(format!("{}.json", ticker.replace(['/', '\\'], "_")))
}

async fn sec_get_text(client: &Client, url: &str, ua: &str) -> Option<String> {
    throttle().await;
    client.get(url).header("User-Agent", ua).send().await.ok()?.text().await.ok()
}
async fn sec_get_json(client: &Client, url: &str, ua: &str) -> Option<Value> {
    throttle().await;
    client.get(url).header("User-Agent", ua).send().await.ok()?.json::<Value>().await.ok()
}

/// First `open`..`close` slice; `between_all` = every non-overlapping one in document order. Tiny manual
/// scan so the Form-4 parse needs no XML/regex dependency. note: the tags in Form-4 primary XML are
/// unprefixed (`<transactionCode>`), so a literal find is enough.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(&s[start..end])
}
fn between_all<'a>(s: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        match after.find(close) {
            Some(end) => {
                out.push(&after[..end]);
                rest = &after[end + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// (Item 4) Parse a Form-4 ownership XML into open-market transactions. Pairs the i-th
/// `<transactionDate><value>…</value>` with the i-th `<transactionCode>…</transactionCode>` (Form 4 emits
/// one of each per transaction, in order); keeps only `P` (purchase) / `S` (sale). Pure -> unit-tested.
/// ceiling: assumes well-formed ordering; a malformed filing yields fewer pairs, never a panic.
fn parse_form4_txns(xml: &str) -> Vec<core::InsiderTx> {
    let dates: Vec<NaiveDate> = between_all(xml, "<transactionDate>", "</transactionDate>")
        .into_iter()
        .filter_map(|seg| between(seg, "<value>", "</value>"))
        .filter_map(|d| NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok())
        .collect();
    let codes = between_all(xml, "<transactionCode>", "</transactionCode>");
    dates
        .into_iter()
        .zip(codes)
        .filter_map(|(date, code)| match code.trim() {
            "P" => Some(core::InsiderTx { date, buy: true }),
            "S" => Some(core::InsiderTx { date, buy: false }),
            _ => None,
        })
        .collect()
}

/// (Item 4) ticker -> 10-digit zero-padded CIK from SEC's `company_tickers.json` (fetched once, disk-
/// cached, then parsed into a process-wide map). A non-US ticker (".DE", "-USD") never appears -> None ->
/// the insider factor simply skips it. An unreachable map caches empty for the run (degrades to no
/// coverage, never panics).
async fn sec_cik(client: &Client, urls: &Urls, ticker: &str) -> Option<String> {
    let path = sec_cache_path("_tickers");
    if !path.exists() {
        if let Some(txt) = sec_get_text(client, &urls.sec_ticker_cik, &urls.sec_user_agent).await {
            if txt.contains("cik_str") {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, txt);
            }
        }
    }
    let map = CIK_MAP.get_or_init(|| {
        let mut m = HashMap::new();
        let parsed = std::fs::read_to_string(sec_cache_path("_tickers")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(obj) = parsed.as_ref().and_then(|v| v.as_object()) {
            for e in obj.values() {
                if let (Some(t), Some(cik)) = (e.get("ticker").and_then(|x| x.as_str()), e.get("cik_str").and_then(|x| x.as_u64())) {
                    m.insert(t.to_uppercase(), format!("{cik:010}"));
                }
            }
        }
        m
    });
    map.get(&ticker.to_uppercase()).cloned()
}

/// (Item 4) Open-market insider transactions for a US ticker, newest first, from SEC Form-4 filings.
/// DISK-CACHED under `.sec_cache/{ticker}.json` (append-only history -> reuse forever). Flow: ticker→CIK,
/// then `submissions/CIK….json` `filings.recent` filtered to form "4", then each Form-4 primary XML
/// parsed for P/S transactions. Caps fetches per ticker (`SEC_FORM4_CAP`) and per run (`SEC_FETCH_BUDGET`)
/// to respect SEC fair-access. None unless real transactions parse. ceiling: only the submissions
/// `recent` block (~1000 newest filings) is read, so very old backtest cutoffs may get no coverage.
pub async fn fetch_insider_history(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::InsiderTx>> {
    use std::sync::atomic::Ordering;
    let cache = sec_cache_path(ticker);
    if let Some(txns) = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str::<Vec<(String, bool)>>(&s).ok()) {
        return Some(
            txns.iter()
                .filter_map(|(d, b)| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok().map(|date| core::InsiderTx { date, buy: *b }))
                .collect(),
        ); // cache hit -> no network, no budget spend
    }
    let cik = sec_cik(client, urls, ticker).await?; // non-US / unknown ticker -> None
    let v = sec_get_json(client, &urls.sec_submissions.replace("{cik}", &cik), &urls.sec_user_agent).await?;
    let recent = v.get("filings")?.get("recent")?;
    let forms = recent.get("form")?.as_array()?;
    let accs = recent.get("accessionNumber")?.as_array()?;
    let docs = recent.get("primaryDocument")?.as_array()?;
    let cik_trim = cik.trim_start_matches('0');
    let mut txns: Vec<core::InsiderTx> = Vec::new();
    let mut fetched = 0;
    for (i, form) in forms.iter().enumerate() {
        if form.as_str() != Some("4") {
            continue;
        }
        if fetched >= SEC_FORM4_CAP || SEC_FETCHES.fetch_add(1, Ordering::Relaxed) >= SEC_FETCH_BUDGET {
            break;
        }
        fetched += 1;
        let (Some(acc), Some(doc)) = (accs.get(i).and_then(|x| x.as_str()), docs.get(i).and_then(|x| x.as_str())) else {
            continue;
        };
        let url = format!("https://www.sec.gov/Archives/edgar/data/{cik_trim}/{}/{doc}", acc.replace('-', ""));
        if let Some(xml) = sec_get_text(client, &url, &urls.sec_user_agent).await {
            txns.extend(parse_form4_txns(&xml)); // an xsl-HTML primaryDocument yields nothing -> harmless
        }
    }
    if !txns.is_empty() {
        let serial: Vec<(String, bool)> = txns.iter().map(|t| (t.date.format("%Y-%m-%d").to_string(), t.buy)).collect();
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&cache, serde_json::to_string(&serial).unwrap_or_default());
    }
    (!txns.is_empty()).then_some(txns)
}

// ── (report) SEC XBRL company-facts — FREE, no key, no daily cap fundamentals fallback ──────────────
// The income-statement source for `report` when FMP is throttled/keyless. Pulls one `companyfacts`
// JSON (every us-gaap concept's full history with filingDate), keeps ANNUAL (10-K, ~12-month) figures,
// and de-dupes each fiscal period to its EARLIEST filing so a later 10-K's restated comparative can't
// post-date the as-of `filed`. US filers only (a non-US ticker has no CIK -> None).

// (filed, period_end, revenue, gross_margin, op_margin, net_margin, eps, roe). Adding roe bumped the
// arity: a pre-roe 7-tuple cache file fails to deserialize -> treated as a miss -> refetched + rewritten
// (SEC is uncapped, so a one-time rebuild is free).
type SecCacheRow = (String, String, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>);

/// Parse a SEC `companyfacts` payload into ANNUAL `FundRow`s (one per fiscal year). Pure -> unit-tested.
/// Revenue is merged across the concepts different eras/filers use; each annual line is joined to the
/// others by exact period-end date (a 10-K's income lines all share one period end). Margins derived
/// (line / revenue), matching `parse_fund_row`. A missing line -> None (neutral), never a fake 0.
fn parse_sec_facts(j: &Value) -> Vec<core::FundRow> {
    let g = match j.pointer("/facts/us-gaap") {
        Some(v) => v,
        None => return Vec::new(),
    };
    // annual datapoints for a set of equivalent concept names: period-end -> (earliest filed, value)
    let collect = |tags: &[&str], unit: &str| -> std::collections::BTreeMap<NaiveDate, (NaiveDate, f64)> {
        let mut m: std::collections::BTreeMap<NaiveDate, (NaiveDate, f64)> = std::collections::BTreeMap::new();
        for tag in tags {
            // chained get (NOT json pointer): the "USD/shares" unit key contains a '/', which a JSON
            // pointer would mis-split into two tokens -> EPS silently lost.
            let arr = match g.get(tag).and_then(|t| t.get("units")).and_then(|u| u.get(unit)).and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };
            for x in arr {
                if !x.get("form").and_then(|v| v.as_str()).is_some_and(|f| f.starts_with("10-K")) {
                    continue; // annual filing only
                }
                let (Some(s), Some(e)) = (x.get("start").and_then(|v| v.as_str()), x.get("end").and_then(|v| v.as_str())) else {
                    continue;
                };
                let (Ok(sd), Ok(ed)) = (NaiveDate::parse_from_str(s, "%Y-%m-%d"), NaiveDate::parse_from_str(e, "%Y-%m-%d")) else {
                    continue;
                };
                if !(350..=380).contains(&(ed - sd).num_days()) {
                    continue; // ~12-month period (skips quarterly/YTD slices that share the tag)
                }
                let (Some(filed), Some(val)) = (
                    x.get("filed").and_then(|v| v.as_str()).and_then(|f| NaiveDate::parse_from_str(f, "%Y-%m-%d").ok()),
                    x.get("val").and_then(|v| v.as_f64()),
                ) else {
                    continue;
                };
                // keep the ORIGINAL report (lowest filed) for this period end, not a later restatement
                m.entry(ed).and_modify(|cur| {
                    if filed < cur.0 {
                        *cur = (filed, val);
                    }
                }).or_insert((filed, val));
            }
        }
        m
    };
    // Balance-sheet items (equity) are INSTANT (as-of period_end, no 12-month duration), so the
    // 350-380 day filter in `collect` drops them — a separate point-in-time collector keyed on `end`,
    // 10-K only, earliest-filed wins (matches `collect`).
    let collect_instant = |tags: &[&str], unit: &str| -> std::collections::BTreeMap<NaiveDate, (NaiveDate, f64)> {
        let mut m: std::collections::BTreeMap<NaiveDate, (NaiveDate, f64)> = std::collections::BTreeMap::new();
        for tag in tags {
            let arr = match g.get(tag).and_then(|t| t.get("units")).and_then(|u| u.get(unit)).and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };
            for x in arr {
                if !x.get("form").and_then(|v| v.as_str()).is_some_and(|f| f.starts_with("10-K")) {
                    continue;
                }
                let Some(ed) = x.get("end").and_then(|v| v.as_str()).and_then(|e| NaiveDate::parse_from_str(e, "%Y-%m-%d").ok()) else {
                    continue;
                };
                let (Some(filed), Some(val)) = (
                    x.get("filed").and_then(|v| v.as_str()).and_then(|f| NaiveDate::parse_from_str(f, "%Y-%m-%d").ok()),
                    x.get("val").and_then(|v| v.as_f64()),
                ) else {
                    continue;
                };
                m.entry(ed).and_modify(|cur| {
                    if filed < cur.0 {
                        *cur = (filed, val);
                    }
                }).or_insert((filed, val));
            }
        }
        m
    };
    let rev = collect(&["Revenues", "RevenueFromContractWithCustomerExcludingAssessedTax", "SalesRevenueNet"], "USD");
    let gp = collect(&["GrossProfit"], "USD");
    let op = collect(&["OperatingIncomeLoss"], "USD");
    let ni = collect(&["NetIncomeLoss"], "USD");
    let eps = collect(&["EarningsPerShareDiluted"], "USD/shares");
    let eq = collect_instant(&["StockholdersEquity", "StockholdersEquityIncludingPortionAttributableToNoncontrollingInterest"], "USD");
    rev.into_iter()
        .map(|(end, (filed, revenue))| {
            let at = |m: &std::collections::BTreeMap<NaiveDate, (NaiveDate, f64)>| m.get(&end).map(|(_, v)| *v);
            let margin = |line: Option<f64>| match line {
                Some(l) if revenue != 0.0 => Some(l / revenue * 100.0),
                _ => None,
            };
            core::FundRow {
                filed,
                period_end: end,
                revenue: Some(revenue),
                gross_margin: margin(at(&gp)),
                op_margin: margin(at(&op)),
                net_margin: margin(at(&ni)),
                eps: at(&eps),
                // ROE = net income ÷ shareholders' equity (%), both as-of this period end. Free from
                // SEC — no premium ratios endpoint needed.
                roe: at(&ni).zip(at(&eq)).and_then(|(n, e)| (e != 0.0).then_some(n / e * 100.0)),
                ..Default::default()
            }
        })
        .collect()
}

/// Annual `FundRow`s for a US ticker from SEC XBRL company-facts. DISK-CACHED as compact parsed rows
/// (`.sec_cache/{ticker}_facts.json`) — NOT the multi-MB raw payload — append-only history reused
/// forever. Budget-capped (`SEC_FETCH_BUDGET`). None for a non-US/unknown ticker or no annual data.
pub async fn fetch_fundamentals_sec(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::FundRow>> {
    use std::sync::atomic::Ordering;
    let cache = sec_cache_path(&format!("{ticker}_facts"));
    if let Some(cached) = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str::<Vec<SecCacheRow>>(&s).ok()) {
        let rows: Vec<core::FundRow> = cached
            .into_iter()
            .filter_map(|(f, e, rev, gm, op, net, eps, roe)| {
                Some(core::FundRow {
                    filed: NaiveDate::parse_from_str(&f, "%Y-%m-%d").ok()?,
                    period_end: NaiveDate::parse_from_str(&e, "%Y-%m-%d").ok()?,
                    revenue: rev,
                    gross_margin: gm,
                    op_margin: op,
                    net_margin: net,
                    eps,
                    roe,
                    ..Default::default()
                })
            })
            .collect();
        return (!rows.is_empty()).then_some(rows); // cache hit -> no network, no budget spend
    }
    let cik = sec_cik(client, urls, ticker).await?; // non-US / unknown -> None
    if SEC_FETCHES.fetch_add(1, Ordering::Relaxed) >= SEC_FETCH_BUDGET {
        return None;
    }
    let v = sec_get_json(client, &urls.sec_companyfacts.replace("{cik}", &cik), &urls.sec_user_agent).await?;
    let rows = parse_sec_facts(&v);
    if !rows.is_empty() {
        let serial: Vec<SecCacheRow> = rows
            .iter()
            .map(|r| {
                (r.filed.format("%Y-%m-%d").to_string(), r.period_end.format("%Y-%m-%d").to_string(),
                 r.revenue, r.gross_margin, r.op_margin, r.net_margin, r.eps, r.roe)
            })
            .collect();
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&cache, serde_json::to_string(&serial).unwrap_or_default());
    }
    (!rows.is_empty()).then_some(rows)
}

/// (report) Fundamentals for the `report` view: FMP first (global coverage / ADRs), SEC EDGAR fallback
/// when FMP yields nothing (429 daily cap, no key, or not covered). Kept SEPARATE from
/// `fetch_fundamentals_history` so the validated backtest/live-enrich data source stays FMP-only (no
/// silent train-serve drift). SEC covers US filers; a foreign ADR with no US XBRL still degrades to None.
pub async fn fetch_fundamentals_report(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::FundRow>> {
    if let Some(rows) = fetch_fundamentals_history(client, urls, ticker).await {
        return Some(rows);
    }
    fetch_fundamentals_sec(client, urls, ticker).await
}

/// (Item 22) The fundamentals feed the RANKING fund lane reads — routed by `buy_heuristic.fund_source`
/// so the SAME source backs the `backtest <set> fund` validation AND the live `screen`/`check` enrich
/// (no train-serve skew). "sec" = SEC EDGAR XBRL (free, no key, no daily cap, ~19y annual, US filers);
/// anything else (default "fmp") = the FMP quarterly path, unchanged. UNLIKE `fetch_fundamentals_report`
/// this does NOT mix the two per-ticker (a per-name source flip would skew the rank) — one source, whole
/// run. Switching sources demands a fresh `backtest <set> fund` re-validation before the tilt is trusted.
pub async fn fetch_fundamentals_ranked(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::FundRow>> {
    match crate::config::fund_source().as_str() {
        "sec" => fetch_fundamentals_sec(client, urls, ticker).await,
        _ => fetch_fundamentals_history(client, urls, ticker).await,
    }
}

/// Latest Bitcoin NUPL (net unrealized profit/loss) from bitcoin-data.com. None on failure.
pub async fn fetch_nupl(client: &Client, urls: &Urls) -> Option<f64> {
    get_json(client, &urls.nupl).await?.get("nupl")?.as_f64()
}

/// Sign one Börse Frankfurt API request the same way their web client does (reverse-engineered from
/// the bundle): `Client-Date` = an ISO-8601 UTC instant we also hash; `X-Client-TraceId` =
/// md5(ClientDate + full-url + salt); `X-Security` = md5(UTC `yyyyMMddHHmm`). The server re-hashes
/// the Client-Date we send, so its timezone is free; only X-Security must match the server's minute
/// (UTC, with skew tolerance). NB: send NO `Origin` header — their gateway 403s a browser origin.
fn borse_frankfurt_sign(url: &str, salt: &str) -> [(&'static str, String); 3] {
    use md5::{Digest, Md5};
    let md5_hex = |s: &str| hex::encode(Md5::digest(s.as_bytes()));
    let now = chrono::Utc::now();
    let client_date = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let trace = md5_hex(&format!("{client_date}{url}{salt}"));
    let security = md5_hex(&now.format("%Y%m%d%H%M").to_string());
    [("Client-Date", client_date), ("X-Client-TraceId", trace), ("X-Security", security)]
}

/// Signed POST to a Börse Frankfurt endpoint -> JSON. None on any failure (the whole ETF leg then
/// degrades to empty — never a crash; the rest of the universe still builds).
async fn borse_frankfurt_post(client: &Client, url: &str, salt: &str, body: &Value) -> Option<Value> {
    let mut req = client.post(url).header("Accept", "application/json, text/plain, */*").json(body);
    for (k, v) in borse_frankfurt_sign(url, salt) {
        req = req.header(k, v);
    }
    req.send().await.ok()?.json::<Value>().await.ok()
}

/// Resolved-Yahoo-symbol -> total expense ratio (%), captured from the Börse Frankfurt ETF search (the
/// SAME call that builds the universe — no extra request). Set ONCE per process in `fetch_xetra_etfs`;
/// `fetch_expense` reads it to fill TER for EU UCITS ETFs that FMP's US-centric free tier never covers.
static BF_TER: std::sync::OnceLock<HashMap<String, f64>> = std::sync::OnceLock::new();

/// Pull the expense ratio out of one BF `etp_search` row. Tries the known key names (their schema drifts
/// over time); the value is taken as a PERCENT (BF reports e.g. 0.2 = 0.20%). None if absent / nonsense.
/// ponytail: if a known ETF (VUAA.DE = 0.07%) prints 100× off, BF sent a fraction -> multiply by 100 here.
fn bf_row_ter(row: &Value) -> Option<f64> {
    const KEYS: &[&str] = &["ter", "totalExpenseRatio", "ongoingCharges", "ongoingCharge", "totalExpenseRatioInPercent"];
    let obj = row.as_object()?;
    KEYS.iter()
        .find_map(|k| obj.iter().find(|(rk, _)| rk.eq_ignore_ascii_case(k)).and_then(|(_, v)| v.as_f64()))
        .filter(|t| t.is_finite() && *t > 0.0 && *t < 5.0)
}

/// The EU-buyable UCITS ETF universe: ask Börse Frankfurt for the top-`cap` ETFs by turnover (real
/// EU-listed, PRIIPs-compliant funds — unlike the US-domiciled NASDAQ-Trader ETFs an EU broker can't
/// sell), then resolve each ISIN to a Yahoo symbol via Yahoo search (first hit = the liquid EU
/// listing, e.g. `.MI`/`.L`/`.DE`). Concurrency-bounded. Empty (with a warning) if the signed API
/// rejects us — salt rotated / endpoint moved; refresh `bf_salt`/`bf_etf_search` in settings.yaml.
pub async fn fetch_xetra_etfs(client: &Client, urls: &Urls, cap: usize) -> Vec<String> {
    let body = serde_json::json!({
        "indices": [], "regions": [], "countries": [], "issuer": [], "types": [],
        "benchmarks": [], "currency": [], "strategy": [], "replicationType": [], "distributionType": [],
        "page": 0, "pageSize": cap, "sorting": "TURNOVER", "sortOrder": "DESC"
    });
    // Capture (isin, TER) per row — the TER is on the SAME search response, so EU UCITS expense ratios
    // come free here (no per-name FMP call, which doesn't cover them anyway). first_keys = the first
    // row's field names, logged ONLY if zero TERs parse, so a renamed BF field self-diagnoses in one run.
    let mut first_keys = String::new();
    let rows: Vec<(String, Option<f64>)> = match borse_frankfurt_post(client, &urls.bf_etf_search, &urls.bf_salt, &body).await {
        Some(j) => match j.get("data").and_then(|d| d.as_array()) {
            Some(arr) => {
                if let Some(o) = arr.first().and_then(|r| r.as_object()) {
                    first_keys = o.keys().cloned().collect::<Vec<_>>().join(",");
                }
                arr.iter().filter_map(|r| Some((r.get("isin")?.as_str()?.to_string(), bf_row_ter(r)))).collect()
            }
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    if rows.is_empty() {
        eprintln!("fetch: Börse Frankfurt ETF search returned nothing (salt rotated? refresh bf_salt) — ETF tables will be empty");
        return Vec::new();
    }
    // BF ignores our pageSize and dumps the whole list (~3430); it's TURNOVER-DESC, so the top `cap`
    // are the most-liquid ETFs — keep only those, both to match universe_size and to avoid firing
    // thousands of Yahoo searches (which DO rate-limit).
    let total = rows.len();
    let top: Vec<(String, Option<f64>)> = rows.into_iter().take(cap).collect();
    // resolve ISIN -> Yahoo symbol (first quote = the liquid EU listing), bounded fan-out, carrying the
    // captured TER alongside. yahoo_search is tuned for news (quotesCount=0) — flip it to quotes here.
    let resolved: Vec<(String, Option<f64>)> = stream::iter(top)
        .map(|(isin, ter)| async move {
            let url = urls
                .yahoo_search
                .replace("{ticker}", isin.as_str())
                .replace("quotesCount=0", "quotesCount=1")
                .replace("newsCount=3", "newsCount=0");
            let sym = get_json(client, &url).await?.pointer("/quotes/0/symbol")?.as_str()?.to_string();
            // Yahoo's fallback symbol for an ISIN whose only listing it indexes is Stuttgart is
            // `<ISIN>.SG` — a chart-less venue, so EVERY such resolution is a guaranteed dead fetch
            // (716 of the 783 old "no Yahoo data" gate-outs). A real liquid listing (.DE/.MI/.L/.AS…)
            // would have ranked first, so there's nothing to rescue: drop it. note: only .SG shows
            // up in practice; add the suffix to this check if another chart-less regional venue appears.
            (!sym.ends_with(".SG")).then_some((sym, ter))
        })
        .buffer_unordered(fetch_concurrency())
        .filter_map(|x| async move { x })
        .collect()
        .await;
    // split into the ticker list + the symbol->TER map (stored once for fetch_expense to read).
    let mut ter_map: HashMap<String, f64> = HashMap::new();
    let tickers: Vec<String> = resolved
        .into_iter()
        .map(|(sym, ter)| {
            if let Some(t) = ter {
                ter_map.insert(sym.clone(), t);
            }
            sym
        })
        .collect();
    let ter_n = ter_map.len();
    let _ = BF_TER.set(ter_map);
    // conclusive diagnostic: distinguishes "BF gave 0 ISINs" from "BF ok but Yahoo bridge resolved
    // none" — the two ways the ETF tables silently empty.
    eprintln!("fetch: Börse Frankfurt returned {total} ETF ISINs (kept top {} by turnover); {} resolved to Yahoo tickers (TER for {ter_n})", total.min(cap), tickers.len());
    if ter_n == 0 && !first_keys.is_empty() {
        eprintln!("fetch: no TER parsed from BF rows — add the right key to bf_row_ter. First-row fields: {first_keys}");
    }
    if tickers.is_empty() {
        eprintln!("fetch: ISIN->Yahoo resolution returned nothing (Yahoo search rate-limited?) — ETF tables will be empty");
    }
    tickers
}

/// Euronext Lisbon equities -> Yahoo `.LS` tickers for the screen universe (Portugal stocks). POSTs
/// the live DataTables endpoint (`urls.euronext_lisbon`, MIC-scoped to XLIS) with the column
/// datapoints the renderer needs — WITHOUT `args[display_datapoints]` the server returns the right
/// row COUNT but empty cells. Symbol+`.LS` is the Yahoo form for the liquid Lisbon names; a wrong/
/// thin one just returns no Yahoo data downstream and self-gates. Degrades to an empty Vec (with a
/// diagnostic) on any failure, so the rest of the universe still builds.
/// note: symbol->`.LS` direct map (no ISIN->Yahoo-search bridge); add the bridge fetch_xetra_etfs
/// already has only if coverage turns out poor.
pub async fn fetch_euronext_lisbon(client: &Client, urls: &Urls) -> Vec<String> {
    // the table's columns, in order; index 2 (symbol) is what core::euronext_lisbon_symbols reads.
    // Raw body (not reqwest `.form()`) so the `args[...]` key keeps its literal brackets, matching the
    // request the page's JS sends.
    let body = "args[display_datapoints]=name,isin,symbol,market,lastPrice,precentDayChange,lastTradeTime\
                &draw=1&start=0&length=1000&iDisplayLength=1000&iDisplayStart=0";
    let j = async {
        client
            .post(&urls.euronext_lisbon)
            .header("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
            .header("X-Requested-With", "XMLHttpRequest")
            .body(body)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    }
    .await;
    let tickers = j.map(|v| core::euronext_lisbon_symbols(&v)).unwrap_or_default();
    if tickers.is_empty() {
        eprintln!("fetch: Euronext Lisbon returned no tickers (endpoint moved / datapoints changed?) — Lisbon stocks absent from the screen");
    }
    tickers
}

/// Build the `screen` universe LIVE (no hand-kept list): top-`cap` crypto by market cap from
/// CoinGecko + the S&P 500 constituents CSV (single companies) + the top-`cap` EU-buyable UCITS ETFs
/// by turnover from Börse Frankfurt (`fetch_xetra_etfs`). Symbols normalised to Yahoo form (`btc` ->
/// `BTC-EUR`/`BTC-USD`, `BRK.B` -> `BRK-B`). Crypto quote currency follows `prefer_eur`. The old
/// US-listed NASDAQ-Trader ETFs are dropped: none are EU-buyable, so they only wasted fetches.
/// Sorted + deduped; empty if all sources fail. Also returns the Xetra-ETF ticker set so the caller
/// can force-classify them as ETF — Yahoo mislabels some (e.g. structured products) as `EQUITY`, which
/// would otherwise leak them into the stocks table past the sector filter.
pub async fn fetch_universe(client: &Client, urls: &Urls, cap: usize, prefer_eur: bool, sectors: &[String]) -> (Vec<String>, std::collections::HashSet<String>) {
    let cg_url = urls.coingecko_markets.replace("{n}", &cap.to_string());
    let (cg, etfs, lisbon) = tokio::join!(
        get_json(client, &cg_url),
        fetch_xetra_etfs(client, urls, cap),
        fetch_euronext_lisbon(client, urls),
    );
    // (Item 18) equity ponds = S&P 500 + any extra same-format constituent CSVs from config. Sequential
    // (1–3 URLs, negligible vs the universe fan-out); a failed/empty CSV just drops its pond, never crashes.
    let mut csv_texts = Vec::new();
    for url in std::iter::once(&urls.sp500_csv).chain(urls.constituents_csv.iter()) {
        csv_texts.push(get_text(client, url).await);
    }
    let crypto_cur = if prefer_eur { "EUR" } else { "USD" };
    let mut out: Vec<String> = Vec::new();
    // crypto: CoinGecko market-cap-ranked array -> SYMBOL-<EUR|USD> (Yahoo crypto form)
    if let Some(arr) = cg.as_ref().and_then(|v| v.as_array()) {
        out.extend(arr.iter().take(cap).filter_map(|c| {
            c.get("symbol").and_then(|s| s.as_str()).map(|s| format!("{}-{crypto_cur}", s.to_uppercase()))
        }));
    }
    // stocks: each constituent CSV -> Yahoo symbol, kept only if the row's GICS sector passes `sectors`
    // (empty = all). Filtering HERE means a sector-restricted screen never even fetches the other
    // sectors' companies. `.take(cap)` AFTER the filter so cap counts matching names per pond, not raw rows.
    for text in csv_texts.into_iter().flatten() {
        out.extend(text.lines().skip(1).filter_map(|l| core::sector_symbol(l, sectors)).take(cap));
    }
    // Euronext Lisbon equities (Yahoo `.LS`). note: NOT sector-filtered — the set is ~33 names,
    // so a sector-restricted screen could leak a few Lisbon stocks; tighten only if that ever bites
    // (the payload doesn't carry GICS anyway).
    out.extend(lisbon);
    let etf_set: std::collections::HashSet<String> = etfs.iter().cloned().collect();
    out.extend(etfs); // EU-buyable UCITS ETFs (Yahoo symbols)
    out.sort();
    out.dedup();
    (out, etf_set)
}

/// Network-module asserts (no live calls): the pure, breakable bit of the Börse Frankfurt signer is
/// the MD5 — pin it to known answers so a bad refactor is caught offline. (The concatenation order is
/// verified against the live server, not here.)
#[cfg(test)]
mod tests {
    use super::*;

    /// Signing + concurrency + throttle asserts (no live calls). White-box via `use super::*`.
    #[test]
    fn bf_ter_parse() {
    use serde_json::json;
    assert_eq!(bf_row_ter(&json!({"isin": "X", "ter": 0.07})), Some(0.07)); // primary key, % as-is
    assert_eq!(bf_row_ter(&json!({"totalExpenseRatio": 0.20})), Some(0.20)); // fallback key
    assert_eq!(bf_row_ter(&json!({"name": "fund"})), None); // no fee field -> None
    assert_eq!(bf_row_ter(&json!({"ter": 12.0})), None); // out of sane TER range -> rejected
    assert_eq!(bf_row_ter(&json!({"ter": 0.0})), None); // zero -> None, never a fake 0%
    }

    #[test]
    fn signing_and_pacing() {
    use md5::{Digest, Md5};
    let md5_hex = |s: &str| hex::encode(Md5::digest(s.as_bytes()));
    assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    // signer emits exactly the three headers the gateway needs, TraceId folds in the url + salt
    let h = borse_frankfurt_sign("https://x/y", "saltz");
    assert_eq!(h.len(), 3);
    assert_eq!([h[0].0, h[1].0, h[2].0], ["Client-Date", "X-Client-TraceId", "X-Security"]);
    assert_eq!(h[1].1, md5_hex(&format!("{}https://x/ysaltz", h[0].1))); // trace = md5(date+url+salt)

    // concurrency = cores × multiplier, both floored at 1 (a 0 anywhere can't stall the fan-out)
    assert_eq!(concurrency_for(8, 8), 64);
    assert_eq!(concurrency_for(4, 0), 4); // multiplier 0 -> treated as 1
    assert_eq!(concurrency_for(0, 8), 8); // cores 0 -> treated as 1

    // throttle slot math: a request never launches in the past, and back-to-back claims at the same
    // instant come out ≥ interval apart (this is what turns a 64-wide burst into a paced stream).
    let base = Instant::now();
    let iv = StdDuration::from_millis(100);
    let (l1, n1) = claim_slot(base, base + 2 * iv, iv); // gate behind `now` -> launch == now
    assert_eq!(l1, base + 2 * iv);
    assert_eq!(n1, base + 3 * iv);
    let (l2, _) = claim_slot(n1, base + 2 * iv, iv); // next claim same `now` -> pushed to the slot
    assert_eq!(l2, base + 3 * iv);
    assert!(l2.duration_since(l1) >= iv);
    }

    /// (Item 14) `is_stale`: a cache older than the TTL is stale; younger isn't; a FUTURE mtime (clock
    /// skew) is NOT stale (never forces an endless refetch). This gates the live-enrich cache freshness.
    #[test]
    fn is_stale_ttl() {
        let now = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_000_000);
        let ttl = StdDuration::from_secs(100);
        assert!(is_stale(now - StdDuration::from_secs(200), now, ttl)); // 200s old > 100s ttl
        assert!(!is_stale(now - StdDuration::from_secs(50), now, ttl)); // 50s old < ttl
        assert!(!is_stale(now + StdDuration::from_secs(50), now, ttl)); // future mtime -> skew-safe, not stale
    }

    /// (report) `parse_sec_facts`: keeps ANNUAL (10-K, ~12mo) lines only, de-dupes a fiscal period to its
    /// EARLIEST filing (a later restated comparative can't post-date `filed`), merges revenue across
    /// concepts, and derives margins. Quarterly slices and a non-10-K form are dropped.
    #[test]
    fn sec_facts_parse_annual_dedup() {
        use serde_json::json;
        let j = json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"USD": [
                // FY2021 original 10-K (filed 2021-11)
                {"start": "2020-10-01", "end": "2021-09-30", "val": 1000.0, "form": "10-K", "filed": "2021-11-01"},
                // SAME FY2021 period RESTATED as a comparative in the 2023 10-K -> later filed, must be ignored
                {"start": "2020-10-01", "end": "2021-09-30", "val": 1234.0, "form": "10-K", "filed": "2023-11-01"},
                // a quarterly slice (~3mo) on the same tag -> dropped (not annual)
                {"start": "2021-07-01", "end": "2021-09-30", "val": 250.0, "form": "10-Q", "filed": "2021-11-01"},
                // FY2022
                {"start": "2021-10-01", "end": "2022-09-30", "val": 1200.0, "form": "10-K", "filed": "2022-11-01"}
            ]}},
            "GrossProfit": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 400.0, "form": "10-K", "filed": "2021-11-01"},
                {"start": "2021-10-01", "end": "2022-09-30", "val": 600.0, "form": "10-K", "filed": "2022-11-01"}
            ]}},
            "EarningsPerShareDiluted": {"units": {"USD/shares": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 3.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "NetIncomeLoss": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 150.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            // INSTANT balance item (only `end`, no 12-month duration) -> exercises collect_instant for ROE
            "StockholdersEquity": {"units": {"USD": [
                {"end": "2021-09-30", "val": 1000.0, "form": "10-K", "filed": "2021-11-01"}
            ]}}
        }}});
        let mut rows = parse_sec_facts(&j);
        rows.sort_by_key(|r| r.period_end);
        assert_eq!(rows.len(), 2); // two fiscal years
        // FY2021: original value kept (1000, not the 1234 restatement), original filing date
        assert_eq!(rows[0].period_end, NaiveDate::from_ymd_opt(2021, 9, 30).unwrap());
        assert_eq!(rows[0].filed, NaiveDate::from_ymd_opt(2021, 11, 1).unwrap());
        assert_eq!(rows[0].revenue, Some(1000.0));
        assert_eq!(rows[0].gross_margin, Some(40.0)); // 400/1000
        assert_eq!(rows[0].eps, Some(3.0));
        assert_eq!(rows[0].roe, Some(15.0)); // NetIncome 150 ÷ StockholdersEquity 1000 (instant)
        // FY2022: no EPS / NI / equity line -> None (neutral, never a fake 0)
        assert_eq!(rows[1].revenue, Some(1200.0));
        assert_eq!(rows[1].gross_margin, Some(50.0)); // 600/1200
        assert_eq!(rows[1].eps, None);
        assert_eq!(rows[1].roe, None);
        assert!(parse_sec_facts(&json!({})).is_empty()); // no facts -> empty, never panics
    }

    /// (Item 4) `parse_form4_txns` pairs each transaction's date with its code and keeps only P/S. Two
    /// transactions here: a purchase (P -> buy) and a sale (S -> sale); an option-grant code (A) is
    /// dropped. Pins the manual XML scan that has no schema to lean on.
    #[test]
    fn form4_parse_keeps_open_market() {
        let xml = "\
            <nonDerivativeTransaction>\
              <transactionDate><value>2021-03-04</value></transactionDate>\
              <transactionCoding><transactionCode>P</transactionCode></transactionCoding>\
            </nonDerivativeTransaction>\
            <nonDerivativeTransaction>\
              <transactionDate><value>2021-03-10</value></transactionDate>\
              <transactionCoding><transactionCode>S</transactionCode></transactionCoding>\
            </nonDerivativeTransaction>\
            <nonDerivativeTransaction>\
              <transactionDate><value>2021-04-01</value></transactionDate>\
              <transactionCoding><transactionCode>A</transactionCode></transactionCoding>\
            </nonDerivativeTransaction>";
        let txns = parse_form4_txns(xml);
        assert_eq!(txns.len(), 2); // the A (option grant) is dropped
        assert!(txns[0].buy && txns[0].date == NaiveDate::from_ymd_opt(2021, 3, 4).unwrap());
        assert!(!txns[1].buy && txns[1].date == NaiveDate::from_ymd_opt(2021, 3, 10).unwrap());
        assert!(parse_form4_txns("<x/>").is_empty()); // no transactions -> empty, never panics
    }

    /// Pure JSON parsers against synthetic API payloads (no network). Guards the field extraction +
    /// edge handling that silently breaks if Yahoo/FMP change shape.
    #[test]
    fn parsers() {
        use serde_json::json;
        // --- parse_chart: Yahoo chart -> Chart (dates/closes/volumes/currency/name/divs) ---
        // ts in unix seconds: 2020-01-01, -02, -03. The middle close is null -> that bar is dropped
        // entirely (date AND volume skip with it). Volume array is short (len 2) -> the 3rd bar's
        // volume falls back to 0.0. Dividend events are out of date order -> must come out sorted.
        // adjclose is PRESENT but differs from close: with use_adjusted_close defaulting false (no
        // settings.yaml in CI -> false), parse_chart must IGNORE it and use raw close (Item 21 inert
        // default — the validated edge is raw-close-calibrated; the flag is the only thing that flips it).
        let j = json!({"chart": {"result": [{
            "timestamp": [1577836800, 1577923200, 1578009600],
            "indicators": {
                "quote": [{"close": [10.0, null, 30.0], "volume": [100, 200]}],
                "adjclose": [{"adjclose": [9.0, null, 27.0]}]
            },
            "meta": {"currency": "EUR", "instrumentType": "EQUITY"},
            "events": {"dividends": {
                "a": {"amount": 2.0, "date": 1593561600}, // 2020-07-01
                "b": {"amount": 1.0, "date": 1585699200}  // 2020-04-01
            }}
        }]}});
        let c = parse_chart(&j, "TST").unwrap();
        assert_eq!(c.closes, vec![10.0, 30.0]); // null close bar dropped
        assert_eq!(c.dates.len(), 2);
        assert_eq!(c.dates[0], NaiveDate::from_ymd_opt(2020, 1, 1).unwrap());
        assert_eq!(c.dates[1], NaiveDate::from_ymd_opt(2020, 1, 3).unwrap());
        assert_eq!(c.volumes, vec![100.0, 0.0]); // 3rd bar has no volume entry -> 0.0
        assert_eq!(c.currency, "EUR");
        assert_eq!(c.name, "TST"); // meta carries no name -> falls back to the ticker
        assert_eq!(c.instrument_type, "EQUITY");
        assert_eq!(c.divs, vec![ // sorted ascending by date despite the out-of-order events object
            (NaiveDate::from_ymd_opt(2020, 4, 1).unwrap(), 1.0),
            (NaiveDate::from_ymd_opt(2020, 7, 1).unwrap(), 2.0),
        ]);
        // currency defaults to USD when meta omits it; malformed json -> None
        let no_meta = json!({"chart": {"result": [{
            "timestamp": [1577836800],
            "indicators": {"quote": [{"close": [10.0]}]}
        }]}});
        assert_eq!(parse_chart(&no_meta, "X").unwrap().currency, "USD");
        assert!(parse_chart(&json!({}), "X").is_none());

        // --- parse_fund_row: FMP income-statement row -> FundRow ---
        let row = json!({
            "filingDate": "2022-02-01", "date": "2021-12-31", "revenue": 200.0,
            "grossProfit": 100.0, "operatingIncome": 50.0, "netIncome": 40.0, "eps": 2.5
        });
        let fr = parse_fund_row(&row).unwrap();
        assert_eq!(fr.filed, NaiveDate::from_ymd_opt(2022, 2, 1).unwrap());
        assert_eq!(fr.period_end, NaiveDate::from_ymd_opt(2021, 12, 31).unwrap()); // period-end `date`, not filing
        // `date` absent -> period_end falls back to `filed` (never fails the row)
        assert_eq!(
            parse_fund_row(&json!({"filingDate": "2022-02-01", "revenue": 5.0})).unwrap().period_end,
            NaiveDate::from_ymd_opt(2022, 2, 1).unwrap()
        );
        assert_eq!(fr.revenue, Some(200.0));
        assert_eq!(fr.gross_margin, Some(50.0)); // 100/200*100
        assert_eq!(fr.op_margin, Some(25.0));
        assert_eq!(fr.net_margin, Some(20.0));
        assert_eq!(fr.eps, Some(2.5));
        // no filingDate -> None; bad date -> None
        assert!(parse_fund_row(&json!({"revenue": 100.0})).is_none());
        assert!(parse_fund_row(&json!({"filingDate": "nope"})).is_none());
        // revenue 0 -> treated as absent, so margins go None (never a divide-by-zero garbage value)
        let zero_rev = parse_fund_row(&json!({"filingDate": "2022-02-01", "revenue": 0.0, "grossProfit": 10.0})).unwrap();
        assert_eq!(zero_rev.revenue, None);
        assert_eq!(zero_rev.gross_margin, None);
    }
}

/// At most this many quote fetches in flight at once. Unbounded `join_all` over the ~750-ticker
/// screen universe stampeded Yahoo into 429s/timeouts (dropping random coins like BTC to err
/// stubs); a bounded window keeps each request uncontended.
///
/// Value = CPU cores × `fetch_concurrency_multiplier` (settings.yaml, default 8). Read once from
/// config and cached — the closure runs on the first fetch and never again, so all call sites stay
/// signature-free. note: lazy global, no threading; bump the multiplier to scan faster or drop it
/// if Yahoo tightens limits.
static FETCH_CONCURRENCY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Pure, testable core: cores × multiplier, each floored at 1 so a 0 in config (or no detectable
/// cores) can't stall the fan-out to zero in-flight requests.
fn concurrency_for(cores: usize, multiplier: usize) -> usize {
    cores.max(1) * multiplier.max(1)
}

pub fn fetch_concurrency() -> usize {
    *FETCH_CONCURRENCY.get_or_init(|| {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        concurrency_for(cores, crate::config::load().fetch_concurrency_multiplier)
    })
}

/// Raw (dates, closes) for one ticker — the 10y daily series, for the `backtest` command. Same single
/// chart call the live path already makes (no EXTRA per-ticker fetch). None on fetch/parse fail or
/// empty history.
pub async fn fetch_history(client: &Client, urls: &Urls, ticker: &str) -> Option<(Vec<NaiveDate>, Vec<f64>)> {
    let j = chart_json(client, urls, ticker, "10y").await?;
    let chart = parse_chart(&j, ticker)?;
    if chart.closes.is_empty() {
        return None;
    }
    Some((chart.dates, chart.closes))
}

/// Raw (dates, closes) from the MAX monthly history — the long-horizon `backtest` path. Decades of
/// monthly bars (vs fetch_history's 10y daily), so forward windows of 10y+ exist for old names and a
/// genuine multi-decade hold can be measured. Same single chart call quote_one already makes for the
/// 20Y backfill (no new fetch type). None on fetch/parse fail or empty history.
pub async fn fetch_history_long(client: &Client, urls: &Urls, ticker: &str) -> Option<(Vec<NaiveDate>, Vec<f64>)> {
    let chart = parse_chart(&chart_json_long(client, urls, ticker).await?, ticker)?;
    if chart.closes.is_empty() {
        return None;
    }
    Some((chart.dates, chart.closes))
}

/// One Quote per ticker, concurrent (≤`FETCH_CONCURRENCY` in flight), input order preserved.
pub async fn quotes(client: &Client, urls: &Urls, fx_cache: &FxCache, tickers: &[String], dip_days: i64, high_days: i64, intraday: bool, news: bool, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Vec<Quote> {
    // Warm the USD rate once up front. Otherwise every US stock races its own USDEUR=X call in the
    // fan-out; one gets rate-limited -> None cached -> all USD names print "USD?" instead of €.
    // note: USD only (dominant case); rare currencies (GBP/CHF) still race, fine at this scale.
    let _ = eur_rate(client, urls, "USD", fx_cache).await;
    // progress to stderr (stdout stays a clean table): a big `screen` is minutes of silent network,
    // so log every PROGRESS_EVERY completions + the final total. note: atomic counter, no bar lib.
    let total = tickers.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let concurrency = fetch_concurrency();
    eprintln!("fetch: {total} quotes (≤{concurrency} concurrent)…");
    const PROGRESS_EVERY: usize = 50;
    // `buffered` (ordered) caps concurrency yet preserves input order (`check` prints in that order).
    let out: Vec<Quote> = stream::iter(tickers.iter())
        .map(|tk| {
            let done = &done;
            async move {
                let quote = quote_one(client, urls, fx_cache, tk, dip_days, high_days, intraday, news, windows, infl).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(PROGRESS_EVERY) || n == total {
                    eprintln!("fetch: {n}/{total} quotes fetched");
                }
                quote
            }
        })
        .buffered(concurrency)
        .collect()
        .await;
    // No second re-fetch pass. The `throttle()` pacer caps the GLOBAL request rate (default 10/s)
    // independent of universe size, so the instantaneous load Yahoo sees is the same for 250 names or
    // 4000 — it just runs longer. That removes the burst-429s that used to drop valid names like NVDA
    // into err stubs (the reason the pass existed). The residual `err`/`no data` rows are structurally
    // dead symbols — CoinGecko top-N coins with no Yahoo listing (stablecoins, brand-new tokens) and
    // `.SG`/illiquid ETF listings Yahoo doesn't carry — which a retry can NEVER recover (measured:
    // recovered 0/57 once paced). They gate out the same as before; we just don't waste a pass on them.
    let dead: Vec<&str> = out
        .iter()
        .filter(|quote| quote.price == "err" || quote.price == "no data")
        .map(|quote| quote.ticker.as_str())
        .collect();
    if !dead.is_empty() {
        let shown = dead.iter().take(20).cloned().collect::<Vec<_>>().join(" ");
        let more = if dead.len() > 20 { format!(" …(+{} more)", dead.len() - 20) } else { String::new() };
        eprintln!("fetch: {} symbol(s) returned no Yahoo data, gated out: {shown}{more}", dead.len());
    }
    out
}

/// Best-effort live 3-month Euribor (%). Returns (rate, is_live); falls back on failure.
/// note: scrapes euribor-rates.eu HTML — fragile, hence the config fallback.
/// Live 3-month Euribor (%) scraped from euribor-rates.eu. `None` on any failure — NO config
/// fallback: a stale hand-entered rate silently poisons the Certificados de Aforro table, so the
/// caller must surface the error instead.
pub async fn fetch_euribor_3m(client: &Client, urls: &Urls) -> Option<f64> {
    let html = get_text(client, &urls.euribor).await?;
    let re = regex::Regex::new(r"(-?\d+\.\d+)\s*%").unwrap();
    re.captures(&html)?[1].parse::<f64>().ok()
}

/// {year -> annual CPI %} for the USA from the BLS public API (CPI-U index, converted to a
/// YoY rate in `core::parse_bls_cpi`); empty on failure. Monthly source, so it reaches the
/// current year — unlike the World Bank's ~1.5y-lagged annual series it replaced.
pub async fn fetch_us_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    use chrono::Datelike;
    // BLS year windows are honored only via POST (seriesid in the body, base /data/ URL). The keyless v1
    // API caps at 25 requests/DAY (shared per-IP) and 10 years/call, so we make ONE 10y call — enough
    // for the 5Y/10Y columns; the 20Y column then mirrors ~10y. Set BLS_API_KEY (free, instant signup at
    // data.bls.gov/registrationEngine) to use v2: 500 req/day and 20y/call, so a single call fills the
    // full 20Y. note: 1 call either way — the old 3-call keyless version exhausted the 25/day cap.
    let now = chrono::Utc::now().year();
    let key = std::env::var("BLS_API_KEY").ok().filter(|k| !k.is_empty());
    let (url, start, mut body) = match &key {
        Some(k) => {
            let mut b = serde_json::Map::new();
            b.insert("registrationkey".into(), k.clone().into());
            (urls.us_cpi.replace("/v1/", "/v2/"), now - 19, b) // v2: up to 20y/call
        }
        None => (urls.us_cpi.clone(), now - 9, serde_json::Map::new()), // v1: max 10y/call
    };
    body.insert("seriesid".into(), serde_json::json!(["CUUR0000SA0"]));
    body.insert("startyear".into(), start.to_string().into());
    body.insert("endyear".into(), now.to_string().into());
    match post_json(client, &url, &serde_json::Value::Object(body)).await {
        Some(d) => core::parse_bls_cpi(&d),
        None => BTreeMap::new(),
    }
}

/// {year -> annual CPI %} for Portugal from Banco de Portugal (series 5721550), each
/// year = its last available month. JSON-stat: value list parallels the date index.
pub async fn fetch_pt_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    match get_json(client, &urls.pt_cpi).await {
        Some(d) => core::parse_pt_series(&d), // index is a JSON array; parse lives in core (tested)
        None => BTreeMap::new(),
    }
}

/// {year -> annual HICP %} for the EU27 from Eurostat, each year = its last month.
/// JSON-stat: value is a sparse {position: rate} map keyed off the time {time: position} index.
pub async fn fetch_eu_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let Some(d) = get_json(client, &urls.eu_hicp).await else { return out; };
    let idx = d.pointer("/dimension/time/category/index").and_then(|v| v.as_object());
    let val = d.get("value");
    if let (Some(idx), Some(val)) = (idx, val) {
        let mut pairs: Vec<(&String, i64)> =
            idx.iter().map(|(k, v)| (k, v.as_i64().unwrap_or(0))).collect();
        pairs.sort_by_key(|(_, p)| *p);
        for (tm, pos) in pairs {
            if let (Ok(year), Some(rate)) =
                (tm[..4].parse::<i32>(), val.get(pos.to_string()).and_then(|v| v.as_f64()))
            {
                out.insert(year, rate); // last month of a year wins
            }
        }
    }
    out
}

/// (label, series) — Portugal (BPstat), USA (BLS CPI-U), EU (Eurostat). Async fetched.
pub async fn inflation_all(client: &Client, urls: &Urls) -> Vec<(&'static str, BTreeMap<i32, f64>)> {
    let (pt, us, eu) = tokio::join!(
        fetch_pt_inflation(client, urls),
        fetch_us_inflation(client, urls),
        fetch_eu_inflation(client, urls),
    );
    vec![("Portugal", pt), ("USA", us), ("EU", eu)]
}

/// Push a notification to the configured ntfy URL (`{topic}` filled in).
pub async fn push(client: &Client, urls: &Urls, topic: &str, title: &str, msg: &str) {
    let _ = client
        .post(urls.ntfy.replace("{topic}", topic))
        .header("Title", title)
        .header("Tags", "chart_with_downwards_trend")
        .body(msg.to_string())
        .send()
        .await;
}
