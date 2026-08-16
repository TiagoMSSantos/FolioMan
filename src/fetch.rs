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
use serde_json::value::RawValue;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// (#56) HARD OFFLINE SWITCH. `FOLIOMAN_OFFLINE=1` makes every outbound helper below return `None`
/// without opening a socket, and makes the long-history disk cache immortal and read-only (see
/// `long_cache_fresh` / `long_cache_save`). Exists so the frozen-data backtest pin is deterministic
/// BY CONSTRUCTION rather than by luck: a fixture ticker missing from the committed cache must show
/// up as a changed sample count, never as a silent live fetch that quietly re-baselines the golden.
///
/// Guarded HERE, at the three helpers, rather than at the ~dozen call sites — every remote read in
/// the project funnels through one of them, so this is both the smaller diff and the stronger claim.
/// A `None` from these is already the fetch-failed path every caller handles, so nothing downstream
/// needs to learn a new state.
///
/// Read once: the variable cannot change mid-process, and a per-call `var()` would sit inside the
/// wide fetch's hot loop.
pub fn offline() -> bool {
    static OFFLINE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFFLINE.get_or_init(|| std::env::var("FOLIOMAN_OFFLINE").is_ok_and(|v| !v.is_empty()))
}

async fn get_json(client: &Client, url: &str) -> Option<Value> {
    if offline() {
        return None;
    }
    throttle().await;
    client.get(url).send().await.ok()?.json::<Value>().await.ok()
}

async fn get_text(client: &Client, url: &str) -> Option<String> {
    if offline() {
        return None;
    }
    throttle().await;
    client.get(url).send().await.ok()?.text().await.ok()
}

async fn post_json(client: &Client, url: &str, body: &Value) -> Option<Value> {
    if offline() {
        return None;
    }
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

/// (round 56) Run counters behind the screen diagnostics footer: paced HTTP launches plus the two
/// monthly-series shortcut paths (round-53 cache hit / round-51 too-young skip). The caches are
/// otherwise invisible — the first sign of one silently breaking would be Yahoo 429s coming back.
/// Relaxed ordering: approximate counts are fine for a footer.
static HTTP_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LONG_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LONG_SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// (paced HTTP calls, monthly-series cache hits, monthly-series too-young skips) so far this run.
pub fn fetch_stats() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (HTTP_CALLS.load(Relaxed), LONG_CACHE_HITS.load(Relaxed), LONG_SKIPS.load(Relaxed))
}

/// `pub` so the network drift net's raw status probes go through the SAME gate as production instead
/// of opening a second, unpaced path that exists only in tests — which also makes them visible to
/// `fetch_stats`, where an unpaced call is invisible by construction.
pub async fn throttle() {
    HTTP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// Parsed chart response. `pub` (rather than the fetch-internal tuple it used to be reduced to) because
/// the BACKTEST needs `name`/`instrument_type` to class a name: it reconstructs quotes from price
/// history alone, and `Quote::stub` leaves both blank, so every fund used to classify as a single stock
/// (`quote_is_etf` reads exactly these two fields). They come from the same response the backtest
/// already fetches — surfacing them costs no extra request.
pub struct Chart {
    pub dates: Vec<NaiveDate>,
    pub closes: Vec<f64>,
    pub volumes: Vec<f64>, // parallel to closes (0.0 where no volume reported); liquidity proxy
    pub currency: String,
    pub name: String,
    pub instrument_type: String, // Yahoo meta.instrumentType ("ETF"/"EQUITY"/...); "" if absent
    pub divs: Vec<(NaiveDate, f64)>, // (ex-date, amount/share) from events.dividends
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
///
/// (round 53) 7-day raw-JSON disk cache — monthly bars only change on month boundaries, so within the
/// TTL the cached payload IS what Yahoo would return. The cache lives HERE rather than in `quote_one`
/// (where it started) because `fetch_history_long` is the other caller and it was walking straight
/// past a file `screen` had just filled: a wide `backtest universe` re-downloaded ~3000 payloads that
/// were already on disk, minutes old. Both callers hardcode the same max/1mo URL, so one entry serves
/// either. Flushed by `long_cache_save`, which every command that uses this must call once at the end.
/// `RawValue`, not `Value`: a cache hit used to DEEP-COPY the whole parsed payload — every decade of
/// monthly bars, per ticker, ~4900 times on a wide `backtest universe` — only for `parse_chart` to read
/// it once and drop it. `RawValue` is the payload's BYTE SPAN, so loading the cache never builds the
/// 5057 inner trees at all and the parse happens once, where the caller wants it (see
/// `long_cache_load`). Both arms carry the RAW payload, so both converge on one `parse_chart` exactly
/// as before — see the format note on `long_cache_save`.
///
/// `Cow`, because the two arms genuinely differ in ownership and pretending otherwise costs a copy: a
/// cache hit is a pointer into the mapped-in file buffer that outlives everything (`Borrowed`, free),
/// while a live fetch is a fresh allocation this run made (`Owned`). An `Arc` would have forced the
/// borrowed arm to allocate just to have something to refcount.
///
/// `pub` for `backtest`'s two wide fan-outs, which want the payload UNPARSED so the 5057 parses can
/// happen inside their rayon walk instead of on the single thread that polls a `buffer_unordered`
/// stream. They own the `parse_chart_raw` call and the empty-`closes` skip that `fetch_history_long`
/// applies for everyone else.
pub async fn chart_json_long(client: &Client, urls: &Urls, ticker: &str) -> Option<Cow<'static, RawValue>> {
    let today = chrono::Local::now().date_naive();
    if let Some((recorded, v)) = long_cache_load().get(ticker) {
        if long_cache_fresh(Some(recorded), today) {
            LONG_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Some(Cow::Borrowed(v));
        }
    }
    let url = urls
        .yahoo_chart
        .replace("{ticker}", ticker)
        .replace("{range}", "max")
        .replace("interval=1d", "interval=1mo");
    // via `Value` deliberately: `get_json` already parses to validate the body, and re-serializing that
    // tree is what keeps a freshly fetched entry byte-for-byte what `long_cache_save` used to write.
    let fetched = get_json(client, &url).await.and_then(|v| serde_json::value::to_raw_value(&v).ok());
    if let Some(v) = &fetched {
        LONG_CACHE_NEW.lock().unwrap().push((ticker.to_string(), v.clone()));
    }
    fetched.map(Cow::Owned)
}

/// `parse_chart` against a payload still in its raw bytes — the one place the deferred parse happens.
///
/// Separate from `parse_chart` because that one is also the network drift net's entry point, which
/// holds an already-parsed `Value`. A failed parse is `None`, the same as a failed fetch, which is the
/// arm every caller already handles.
pub fn parse_chart_raw(raw: &RawValue, ticker: &str) -> Option<Chart> {
    parse_chart(&serde_json::from_str::<Value>(raw.get()).ok()?, ticker)
}

/// Parse a Yahoo chart payload into aligned dates+closes (null closes dropped) + meta.
///
/// `pub` for the network drift net, which health-probes the chart endpoint and then has to assert the
/// parser still eats what came back. Handing it THIS fn instead of `fetch_history` means the body it
/// classified as healthy is the body it parses — one GET instead of two, and no window in which the
/// endpoint is healthy at probe time and throttled at fetch time (which read as drift and failed the
/// build for an environmental reason).
pub fn parse_chart(j: &Value, ticker: &str) -> Option<Chart> {
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

    // (splice) drop everything before the last redenomination splice — an MXN head glued onto a GBP
    // tail feeds every downstream metric (life_cagr, MAXDD, R², year_returns) a fiction. One parse
    // site = live + backtest see identical data, the same no-train-serve-skew argument adjclose uses
    // above. All parallel arrays cut together; pre-splice dividends go too (they're denominated in
    // the discarded currency — summing them into tr_cagr would mix units). Crypto exempt: a real
    // 13×/wk week exists there (SHIB), so the trimmer would amputate genuine history.
    if !crate::picks::is_currency_quoted(ticker) {
        let start =
            core::splice_trim_start(&dates, &closes, crate::config::splice_max_weekly_rate());
        if start > 0 {
            let cut = dates[start];
            dates.drain(..start);
            closes.drain(..start);
            volumes.drain(..start);
            divs.retain(|(d, _)| *d >= cut);
        }
    }

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

pub async fn fetch_news(client: &Client, urls: &Urls, ticker: &str) -> Vec<String> {
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

/// Pence-quote scale: Yahoo reports LSE listings in pence ("GBp", sometimes "GBX"), so the pound
/// FX rate must be divided by 100. Uppercasing alone would silently apply the pound rate to a
/// pence price — 100× EUR inflation on every `.L` name's price and turnover.
fn fx_scale(cur: &str) -> f64 {
    if cur == "GBp" || cur == "GBX" { 0.01 } else { 1.0 }
}

/// EUR per 1 unit of `cur`. 1.0 for EUR; None if Yahoo has no FX pair. Cached — the cache holds
/// the plain pound rate under "GBP"; the pence scale is applied on read, never stored.
pub async fn eur_rate(client: &Client, urls: &Urls, cur: &str, cache: &FxCache) -> Option<f64> {
    let scale = fx_scale(cur);
    let cur = if cur.is_empty() { "EUR".to_string() } else { cur.to_uppercase() };
    if cur == "EUR" {
        return Some(1.0);
    }
    if let Some(v) = cache.lock().await.get(&cur) {
        return v.map(|r| r * scale);
    }
    let mut rate = None;
    for (sym, invert) in [(format!("{cur}EUR=X"), false), (format!("EUR{cur}=X"), true)] {
        if let Some(px) = last_close(client, urls, &sym).await {
            rate = Some(if invert { 1.0 / px } else { px });
            break;
        }
    }
    cache.lock().await.insert(cur, rate); // cache misses too
    rate.map(|r| r * scale)
}

/// (FX) [`eur_rate`]'s as-of twin: EUR per 1 unit of `cur` for every date Yahoo quotes the pair on.
/// The backtest scores cutoffs going back a decade and USD/EUR moved ~30% across it, so reusing today's
/// SPOT rate there would both misprice every old cutoff AND leak the present into the walk-forward
/// split — the one thing that lane exists to prevent.
///
/// `None` = `cur` IS the euro (the identity; nothing to fetch). `Some(empty)` = the pair has no history
/// and the caller must drop the ratio, never guess. Same `{CUR}EUR=X` / `EUR{CUR}=X` pair and the same
/// GBp÷100 pence scale `eur_rate` uses, so spot and as-of can't disagree about what a rate means.
///
/// `long` mirrors the backtest's own history choice. It has to: the ≥8y lane walks MAX monthly bars back
/// decades, and a 10y daily rate series would leave every cutoff before it with NO rate — silently
/// dropping the oldest two thirds of exactly the foreign sample this is meant to measure.
async fn eur_rate_series(client: &Client, urls: &Urls, cur: &str, long: bool) -> Option<BTreeMap<NaiveDate, f64>> {
    let scale = fx_scale(cur);
    let cur = cur.to_uppercase();
    if cur.is_empty() || cur == "EUR" {
        return None;
    }
    for (sym, invert) in [(format!("{cur}EUR=X"), false), (format!("EUR{cur}=X"), true)] {
        let hist = if long {
            fetch_history_long(client, urls, &sym).await
        } else {
            fetch_history(client, urls, &sym).await
        };
        if let Some(chart) = hist {
            return Some(
                chart
                    .dates
                    .into_iter()
                    .zip(chart.closes)
                    .filter(|(_, px)| px.is_finite() && *px > 0.0) // a 0 close would invert to +inf
                    .map(|(d, px)| (d, if invert { scale / px } else { px * scale }))
                    .collect(),
            );
        }
    }
    Some(BTreeMap::new())
}

/// (FX) The `from -> to` price factor per DATE — what turns a native as-of close into the filer's own
/// books at the cutoff that close belongs to. Empty map = a leg has no history; the caller then drops
/// the price-joined factors for that ticker rather than falling back to spot.
///
/// Callers MUST short-circuit `from == to` before calling. That path has to stay bit-identical (it is
/// every US filer, and the proof that this change is additive), and this one would introduce a
/// rate-lookup and a multiply where the answer is exactly 1.0.
///
/// `long` must match the price history the caller is walking, or the rates and the closes cover
/// different eras — see [`eur_rate_series`].
pub async fn fx_factor_series(client: &Client, urls: &Urls, from: &str, to: &str, long: bool) -> BTreeMap<NaiveDate, f64> {
    match (eur_rate_series(client, urls, from, long).await, eur_rate_series(client, urls, to, long).await) {
        (Some(a), None) => a,                                                  // X -> EUR
        (None, Some(b)) => b.into_iter().map(|(d, r)| (d, 1.0 / r)).collect(),  // EUR -> X
        // both legs quoted: hop through EUR, keeping only dates BOTH have (no cross-date rate splicing)
        (Some(a), Some(b)) => a.iter().filter_map(|(d, ra)| b.get(d).map(|rb| (*d, ra / rb))).collect(),
        (None, None) => BTreeMap::new(), // both EUR — caller was supposed to skip; empty = no conversion claimed
    }
}

/// (FX) `close_native` re-expressed in the `to` currency, fetching only the rates it actually needs.
/// Same-currency short-circuits inside [`core::convert_price`] BEFORE either rate is looked up, so the
/// US-filer path (and every other listing that trades in its filer's own currency) costs nothing and is
/// bit-for-bit unchanged. None when a rate is missing — the caller then drops the price-joined ratio.
pub async fn price_in(client: &Client, urls: &Urls, fx: &FxCache, close_native: f64, from: &str, to: &str) -> Option<f64> {
    if !core::needs_fx(from, to) {
        return Some(close_native); // avoid even touching the cache on the common path
    }
    let eur_from = eur_rate(client, urls, from, fx).await;
    let eur_to = eur_rate(client, urls, to, fx).await;
    core::convert_price(close_native, from, to, eur_from, eur_to)
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
        // monthly full history, ONLY to back-fill the >10y horizons (20Y) without breaking the daily
        // ones. (round 51) skipped for tickers a previous run proved too young to have any (the
        // None path below == the fetch-failed path, so output is identical).
        async {
            let today = chrono::Local::now().date_naive();
            if long_skip_fresh(long_skip_load().get(ticker), today) {
                LONG_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            }
            // the 7-day disk cache used to sit here; it now lives inside `chart_json_long` so the
            // backtest's `fetch_history_long` shares it. The too-young skip above stays — it's a
            // different mechanism (never-fetch, not fetch-once) and only `quote_one` records it.
            chart_json_long(client, urls, ticker).await
        },
        // news headlines are displayed ONLY by `check`/`alert`; `screen`/`perf` ignore them, so skip the
        // per-name Yahoo search there (~25% fewer requests across a 3800-name screen -> proportionally faster).
        async { if news { fetch_news(client, urls, ticker).await } else { Vec::new() } },
        async { if intraday { intraday_closes(client, urls, ticker).await } else { None } },
    );

    let parsed = chart_j.as_ref().and_then(|j| parse_chart(j, ticker));
    let mut chart = match parsed {
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

    // (history_proxy) young listing of an old strategy: splice the configured older twin's closes
    // (rebased at the listing's first bar) UNDER the listing's own series, so every downstream metric
    // (5Y/10Y legs, range, SMA, R², age) reads the strategy's proven history while price/TER/turnover
    // stay the listing's own (turnover reads a 30-bar TRAILING window, untouched by a prepend).
    // Same-currency twins only — a cross-currency splice would bake FX drift into the CAGR.
    let mut history_proxied = false;
    if let Some(proxy) = crate::config::history_proxy().get(ticker) {
        match chart_json(client, urls, proxy, "10y").await.as_ref().and_then(|j| parse_chart(j, proxy)) {
            Some(p) if p.currency == chart.currency => {
                if let Some((dates, closes)) =
                    core::splice_history(&chart.dates, &chart.closes, &p.dates, &p.closes)
                {
                    let added = dates.len() - chart.dates.len();
                    let mut volumes = vec![0.0; added]; // proxy liquidity is NOT this listing's
                    volumes.extend_from_slice(&chart.volumes);
                    (chart.dates, chart.closes, chart.volumes) = (dates, closes, volumes);
                    history_proxied = true;
                } else {
                    eprintln!("fetch: history_proxy {proxy} for {ticker} has no bars predating the listing (or no overlap) — splice skipped");
                }
            }
            Some(p) => eprintln!("fetch: history_proxy {proxy} ({}) and {ticker} ({}) trade in different currencies — splice skipped (pick a same-currency twin)", p.currency, chart.currency),
            None => eprintln!("fetch: history_proxy {proxy} for {ticker} returned no chart — splice skipped"),
        }
    }

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
        match fetch_ratios_sec(client, urls, fx_cache, ticker, *chart.closes.last().unwrap(), &chart.currency).await {
            (None, None) => fetch_ratios(client, urls, ticker).await,
            got => got,
        }
    } else {
        (None, None)
    };
    // Yahoo labels physical ETCs (gold etc.) EQUITY, but an exact hit in the BF map proves ETP — fill
    // the TER anyway (map-only: no FMP etf/info call wasted on true equities).
    let ter = if is_etf { fetch_expense(client, urls, ticker, &chart.name).await } else { bf_ter_exact(ticker) };
    // (AUM) fund size, same BF payload + same proof-of-ETP stance for mislabeled ETCs.
    let aum_eur = if is_etf { bf_aum(ticker, &chart.name) } else { bf_aum_exact(ticker) };
    // (USE/REPL/bench) share-class + replication tokens + benchmark index, same BF payload —
    // display columns + history_proxy twin hints, never scored.
    let mut meta = if is_etf { bf_meta(ticker, &chart.name) } else { BfMeta::default() };
    // (round 49) USE fallback from the listing name when BF is silent (venue/regulatory funds carry
    // no BF row): UCITS names spell the share class as a word token ("… USD (Acc)"). BF stays
    // authoritative via or_else. Display-only; H still requires a BF replication token, so no H flips.
    if is_etf {
        meta.use_of = meta.use_of.or_else(|| use_from_name(&chart.name));
    }

    // Back-fill history older than the ~10y daily window from the monthly series, so the 20Y column and
    // long dividend sums populate for old names. Prepend only the monthly bars that predate the daily
    // window (strictly < the first daily date) then the full daily tail — no overlap. Used ONLY for
    // horizon_changes + dividend_sums; every other metric stays on the precise daily `chart`. Falls back
    // to daily-only (20Y stays n/a) if the monthly fetch failed.
    let cut = chart.dates[0];
    let (mut long_dates, mut long_closes, mut long_divs) =
        match chart_long_j.as_deref().and_then(|j| parse_chart_raw(j, ticker)) {
            Some(lc) => {
                let keep = lc.dates.iter().take_while(|d| **d < cut).count();
                // (round 51) monthly series contributed nothing (young listing) — flag the ticker so
                // the next 30 days of runs skip its useless second fetch.
                if keep == 0 {
                    LONG_SKIP_NEW.lock().unwrap().push(ticker.to_string());
                }
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

    // (splice) second pass over the MERGED series: each payload was trimmed inside parse_chart, but
    // the monthly-head/daily-tail seam is a joint neither payload contains alone — a redenomination
    // exactly there only becomes a visible step once the two are glued.
    if !crate::picks::is_currency_quoted(ticker) {
        let start = core::splice_trim_start(&long_dates, &long_closes, crate::config::splice_max_weekly_rate());
        if start > 0 {
            let cut_d = long_dates[start];
            long_dates.drain(..start);
            long_closes.drain(..start);
            long_divs.retain(|(d, _)| *d >= cut_d);
        }
    }

    // Whole-life age + endpoint CAGR from the SAME merged series. `age_years` is display-only (`yrs`);
    // `life_cagr` feeds the `cagr` column, the `growth_min_cagr` whole-life bar, and — when
    // `use_life_cagr` is on — the growth rank itself, so it is no longer display-only and must be
    // computed the SAME way the backtest computes it: `core::life_cagr` is that shared definition,
    // carrying the >=6mo / positive-first-close guards this site used to spell out inline.
    let age_years = long_dates.first().zip(long_dates.last())
        .map(|(first, last)| (*last - *first).num_days() as f64 / 365.25);
    let life_cagr = core::life_cagr(&long_dates, &long_closes);
    // (#3l) same merged series, same free-accessor knob as `core::backtest_quote` -> train==serve.
    let capped_cagr =
        core::capped_life_cagr(&long_dates, &long_closes, crate::config::life_cagr_max_years());
    // Same series and the same `infl` the perf legs get, so this stands in for a missing long rung in
    // the SAME units the neighbouring cells are printed in (real, not nominal, whenever the legs are).
    // Display-only — `picks::perf_fill` is its only reader.
    let life_return_pct = core::life_return(&long_dates, &long_closes, infl);
    // (#41) 36 trailing month-over-month returns for the growth_corr_cap redundancy skip. Built from the
    // DAILY `chart`, not the merged long series: the merge is monthly-head + daily-tail, so its cadence
    // changes mid-series, and only the tail covers the recent 36 months this needs anyway.
    // `core::monthly_returns_tail` resamples to month-end, which is what makes this comparable to the
    // backtest's already-monthly slice through the same fn.
    let trail_monthly = core::monthly_returns_tail(&chart.dates, &chart.closes, 36);
    // (TR-CAGR) same endpoints + the whole-life dividend sum: closes are price-only, so a payer's CAGR
    // hides the cash it returned. LOWER BOUND — the payout is added, not reinvested (true total return
    // with reinvestment compounds higher). Display-only, same guards as life_cagr; ≈ CAGR for Acc funds.
    let tr_cagr = match (long_closes.first(), long_closes.last(), age_years) {
        (Some(&first), Some(&last), Some(age)) if first > 0.0 && age >= 0.5 => {
            let divs_sum: f64 = long_divs.iter().map(|(_, d)| d).sum();
            Some((((last + divs_sum) / first).powf(1.0 / age) - 1.0) * 100.0)
        }
        _ => None,
    };

    // (S-8Y) the same price stats over the LAST 8 YEARS only, for the 8Y-pinned diagnostic column. This
    // is the one place the closes still exist — `Quote` carries derived scalars only — so a consumer
    // downstream cannot re-slice, it has to be precomputed here. `i == 0` means nothing in the payload
    // predates the cutoff, i.e. the name's whole record already IS its 8-year window: leave None and the
    // column falls back to the full-window stats, exactly like `long_leg_fixed` falls back on its CAGR leg.
    let stats_8y = chart
        .dates
        .last()
        .map(|last| chart.dates.partition_point(|d| *d < *last - chrono::Duration::days(2920)))
        .filter(|&i| i > 0)
        .map(|i| core::Stats8 {
            range_pct: core::price_pct_rank(&chart.closes[i..]),
            trend_r2: core::trend_r2(&chart.closes[i..]),
            max_drawdown_pct: core::max_drawdown_pct(&chart.closes[i..]),
            underwater_yrs: core::longest_underwater_yrs(&chart.closes[i..]),
        });

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

    let last_date = chart.dates.last().copied();
    let month_ago = last_date.and_then(|ld| asof(&chart.dates, &chart.closes, ld - chrono::Duration::days(30)));
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
        quote_currency: Some(chart.currency.clone()), // (FX) what close_native is denominated in — the proof a filer's EPS may be divided by it
        last_close_date: last_date, // (D) newest bar's date -> screen flags/drops stale (halted/dead) listings
        drawdown_pct,
        intraday: intra.map_or([None; 3], |cs| core::intraday_changes(&cs)),
        // avg daily turnover in native currency -> EUR (×rate). Crypto: Yahoo "volume" is already
        // a notional amount, so use it raw (close×volume would double-count). Equities: close×volume.
        // Suffix check, NOT any dash: BRK-B is an equity — the crypto arm fed its raw share count
        // (~4M) into the scored liquidity term instead of ~€1.5B of close×volume.
        avg_turnover_eur: if crate::picks::is_currency_quoted(ticker) {
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
        // (#45) MVRV is stamped LATER, in the screen enrichment block, not here: it comes from one bulk
        // CoinMetrics request for the whole crypto lane, so fetching it per-quote would be one HTTP
        // call per coin for a number the batch already has. None here = "not filled yet", never "no data".
        mvrv: None,
        // (F) trailing ROE % (equities w/ FMP_API_KEY only; None -> neutral quality tilt).
        roe,
        // (TER) ETF annual expense ratio % (ETFs w/ FMP_API_KEY only; None -> n/a column).
        expense_ratio: ter,
        // (#44) GICS sector is NOT a quote-endpoint field — no Yahoo/FMP call here carries one. It is
        // joined in `screen` from the universe CSV's sector_of map, so every quote leaves this fn None.
        sector: None,
        // (AUM) fund size from the BF universe payload (ETFs/ETPs only; None -> gate inert, n/a column).
        aum_eur,
        // (round 47) Yahoo quoteSummary facts for funds WITHOUT BF facts — display/H-flag only via
        // ter_shown()/aum_shown(); the scored fields above stay BF-only so ranks don't move.
        ter_fallback: if is_etf { yh_ter_exact(ticker) } else { None },
        aum_fallback: if is_etf { yh_aum_exact(ticker) } else { None },
        use_of_profits: meta.use_of,
        replication: meta.repl,
        benchmark: meta.bench,
        domicile: meta.dom,
        // (REV-YoY/EPS-YoY/NET%/BUYBK + trend line) filled later by enrich_income_stmt for the DISPLAYED stock rows only
        rev_yoy: None,
        eps_yoy: None,
        net_margin_fy: None,
        buyback_yoy: None,
        annual_brief: None,
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
        downside_dev_pct: None, // (r39) backtest-probe-only — no score path or footer reads it, so the live fetch doesn't pay for it
        // (consistency) % of rolling 5y/10y windows positive, same closes — the screen's footer
        // stats; the 10y pair (r16) is the decade horizon the book is actually held for.
        roll5y_pos_pct: core::rolling_positive_pct(&chart.closes, 5, 252),
        underwater_yrs: core::longest_underwater_yrs(&chart.closes),
        worst_5y_pct: core::worst_rolling_pct(&chart.closes, 5, 252),
        roll10y_pos_pct: core::rolling_positive_pct(&chart.closes, 10, 252),
        worst_10y_pct: core::worst_rolling_pct(&chart.closes, 10, 252),
        year_returns: core::calendar_year_returns(&chart.dates, &chart.closes),
        fund_factor: None, // (G) live screen leaves this None (neutral); only the small/check-scale path (A3) populates it
        fund: None,        // (G+) same: `enrich_fund_factor` fills it on the paths that fetch fundamentals
        age_years,
        life_cagr,
        capped_cagr,
        life_return_pct,
        trail_monthly,
        tr_cagr,
        history_proxied,
        stats_8y,
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
    let path = crate::config::data_path(".fmp_cache").join(format!("live_{tag}_{}.json", ticker.replace(['/', '\\'], "_")));
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

/// (E/F) Trailing P/E + ROE for an EQUITY from SEC EDGAR — free, no key, no daily cap (unlike FMP).
/// P/E = close ÷ latest annual diluted EPS, both forced into the FILER'S REPORTING CURRENCY first: a US
/// filer reports and trades in USD so nothing is converted, but a foreign private issuer need not (ASML
/// keeps its books in EUR and its ADR trades USD), and dividing across two currencies would print a
/// plausible-looking P/E that is wrong by the whole FX rate. The quality level is read off the SEC-derived
/// `FundRow` (ROE, or ROA where equity is negative or collapsed) and is a ratio, so it needs no conversion. None when there's no CIK or no rate, so the
/// caller can fall back to FMP. Filling `pe_ratio` also un-blanks the PEG column, which derives from it
/// downstream. The SEC fetch is itself disk-cached + budget-capped (see `fetch_fundamentals_sec`), so a
/// wide `screen` cold-fetches once then reads free forever.
async fn fetch_ratios_sec(
    client: &Client,
    urls: &Urls,
    fx: &FxCache,
    ticker: &str,
    close_native: f64,
    quote_ccy: &str,
) -> (Option<f64>, Option<f64>) {
    let rows = fetch_fundamentals_sec(client, urls, ticker).await.unwrap_or_default();
    let Some(latest) = rows.last() else {
        return (None, None); // rows are BTreeMap-ordered by period_end -> last = newest fiscal year
    };
    // TRAILING-TWELVE-MONTH EPS, not the latest ANNUAL EPS: a fast grower's last 10-K goes stale the
    // moment it reports a quarter (e.g. LITE FY EPS $0.37 vs TTM ~$5.7 mid-ramp -> P/E 2319 vs the real
    // ~150). Roll TTM from the quarterly concept; fall back to the annual EPS when there's no newer
    // quarter (just-filed 10-K / annual-only filer) so it's never worse than before, never a fake value.
    let eps = sec_ttm_eps(client, urls, ticker).await.or(latest.eps);
    // (FX) put the price in the same books as the EPS before dividing. Same-currency (every US filer)
    // returns the close untouched and fetches no rate; a mismatch with no rate yields None, so the P/E
    // column stays n/a rather than showing a number that is silently off by the exchange rate.
    let price = match latest.currency.as_deref() {
        Some(fund) => price_in(client, urls, fx, close_native, quote_ccy, fund).await,
        None => Some(close_native), // FMP-sourced rows carry no currency -> legacy behaviour
    };
    let pe = eps
        .filter(|e| *e > 0.0)
        .zip(price)
        .map(|(e, p)| p / e);
    // ROE where equity is a credible denominator, ROA where it isn't — the SAME resolver the backtest scores through
    // (core::fund_factors), so the live column and the validated factor can't drift apart.
    (pe, core::quality_return(latest.roe, latest.roa, latest.net_margin))
}

/// Trailing-twelve-month diluted EPS for a US filer from SEC XBRL's single-concept `companyconcept`
/// endpoint (tiny vs the multi-MB companyfacts). Disk-cached as one float, budget-capped. None for a
/// non-US/unknown ticker or when TTM can't be rolled (caller then falls back to the annual EPS).
async fn sec_ttm_eps(client: &Client, urls: &Urls, ticker: &str) -> Option<f64> {
    use std::sync::atomic::Ordering;
    // `_ttmeps3`, not `_ttmeps2`, for the same reason v2 replaced v1: the file is a BARE FLOAT with no
    // version field and no expiry, so a wrong roll already on disk is served forever no matter what this
    // fn later learns. v1 held stale rolls (MNST's 2.73, off a 2010 annual); v2 holds split-mixed ones
    // (ORLY's −5.66, see `ttm_eps_from_concept`). A cache bump is the only way a fix here reaches a box
    // that has already run — and a negative cached EPS is invisible, since the `pe` cell just drops it.
    let cache = sec_cache_path(&format!("{ticker}_ttmeps3"));
    if let Some(v) = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str::<f64>(&s).ok()) {
        return Some(v); // cache hit -> no network, no budget spend
    }
    let cik = sec_cik(client, urls, ticker).await?; // non-US / unknown -> None
    let today = chrono::Utc::now().date_naive();
    // Walk the SAME tag list `parse_sec_facts` uses, in the same order, taking the first roll that
    // passes the guards inside `ttm_eps_from_concept`. A filer that switched concepts has a dead first
    // tag whose roll is stale by years, and only the fallback carries a current one. Healthy filers
    // still cost exactly ONE fetch — the loop only advances when a tag yields nothing usable.
    for concept in US_GAAP_TAGS.eps {
        if SEC_FETCHES.fetch_add(1, Ordering::Relaxed) >= SEC_FETCH_BUDGET {
            return None;
        }
        let url = urls.sec_companyconcept.replace("{cik}", &cik).replace("{concept}", concept);
        let Some(j) = sec_get_json(client, &url, &urls.sec_user_agent).await else {
            continue; // concept absent for this filer (404) -> try the next name
        };
        if let Some(ttm) = ttm_eps_from_concept(&j, today) {
            let _ = std::fs::write(&cache, serde_json::to_string(&ttm).ok()?);
            return Some(ttm);
        }
    }
    None // every tag stale, insane or missing -> caller falls back to the annual EPS
}

/// Roll a trailing-twelve-month EPS from a SEC `companyconcept` (EarningsPerShareDiluted) payload:
///   TTM = latest full-year EPS + current fiscal YTD − prior-year same-length YTD
/// the standard cumulative roll-forward (SEC reports quarters as YTD cumulatives, not standalone). When
/// there's no YTD reported past the latest 10-K (a just-filed annual, or an annual-only filer) it returns
/// that annual EPS unchanged. Pure -> unit-tested. None if not even an annual EPS is present, or if the
/// roll fails either guard (see `fresh`/`sane` below) — the caller then tries the next concept name.
fn ttm_eps_from_concept(j: &Value, today: NaiveDate) -> Option<f64> {
    // the "USD/shares" unit key contains a '/', which a JSON pointer would mis-split -> chained get.
    // (FX) the unit is NOT assumed USD: a 20-F filer reports EPS in its own currency ("EUR/shares" for
    // ASML), and hard-coding USD returned None for every one of them. The USD-first preference MIRRORS
    // `money_unit` on purpose — a dual-currency filer (TM tags both JPY and USD) must resolve to the same
    // currency here as its FundRows did, or the P/E would divide two different books by each other.
    let units = j.get("units")?.as_object()?;
    let key = if units.contains_key("USD/shares") {
        "USD/shares".to_string()
    } else {
        units.keys().filter(|k| k.ends_with("/shares")).min()?.clone()
    };
    let arr = units.get(&key)?.as_array()?;
    // (start, end) -> (latest filed, val); de-dupes a period's restatements to its NEWEST filing.
    //
    // NEWEST, not first, and a share SPLIT is why. The roll below spans three periods, and a split
    // restates every prior EPS by its factor — so the legs are only comparable when they all carry the
    // same share basis, and the newest filing is the only basis all three can share. Keeping the first
    // filing pinned whichever legs predate the split to the OLD basis and left the rest on the new one,
    // which does not read as a data error, it reads as a plausible number:
    //
    //   ORLY split 15-for-1 on 2025-06-10, mid-roll. FY2025 2.98 (post) + Q1-2026 YTD 0.72 (post)
    //   - Q1-2025 YTD 9.35 (PRE, first filed 2025-05) = -5.66, which is what the cache held.
    //
    // Neither existing guard sees it: the roll is fresh, and |-5.66| is well inside 100x the annual.
    // A negative EPS then just drops out of the `pe` cell's `> 0.0` filter, so the column reads n/a and
    // nothing anywhere says why. Any name that split since its oldest comparative filing had this.
    //
    // Accounting restatements ride along with the same rule, and that is also correct for a roll: the
    // restated figure is the one the filer now stands behind.
    let mut periods: std::collections::HashMap<(NaiveDate, NaiveDate), (NaiveDate, f64)> = std::collections::HashMap::new();
    for x in arr {
        let d = |k| x.get(k).and_then(|v| v.as_str()).and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        let (Some(sd), Some(ed), Some(filed)) = (d("start"), d("end"), d("filed")) else { continue };
        let Some(val) = x.get("val").and_then(|v| v.as_f64()) else { continue };
        periods
            .entry((sd, ed))
            .and_modify(|cur| {
                if filed > cur.0 {
                    *cur = (filed, val);
                }
            })
            .or_insert((filed, val));
    }
    // split into full years vs sub-year YTD cumulatives, each carrying (end, val[, span])
    let mut annual: Vec<(NaiveDate, f64)> = Vec::new();
    let mut ytd: Vec<(NaiveDate, f64, i64)> = Vec::new();
    for ((sd, ed), (_, val)) in &periods {
        let span = (*ed - *sd).num_days();
        if (350..=380).contains(&span) {
            annual.push((*ed, *val));
        } else if span < 350 {
            ytd.push((*ed, *val, span));
        }
    }
    annual.sort_by_key(|(e, _)| *e);
    let (fy_end, fy_eps) = *annual.last()?; // latest full year — the base + the fallback
    // TWO GUARDS, and neither subsumes the other — both were needed against real payloads:
    //   FRESHNESS. MNST stopped filing this concept in 2010, so the roll happily served a 2011 EPS
    //     against a 2026 price: P/E 34.3 where the truth is ~48.2, and a PEG wrong by the same factor.
    //     Nothing in the maths above notices, because the arithmetic is perfectly valid — just ancient.
    //   SANITY. HAL mis-tags SHARE COUNTS (120000 .. 650000) inside the EPS concept, and those entries
    //     run 2021-09 .. 2024-09 — INSIDE a 2-year freshness window. The roll came out -60001.29 on a
    //     -1.29 annual. Today it is masked only because `pe` filters `> 0.0`; `earnings_yield` wouldn't.
    let fresh = |end: NaiveDate| (today - end).num_days() <= 730;
    // Relative, never absolute: BRK-A's genuine EPS is ~$40,000, so any fixed band is a trap. K=100 is
    // set against the case this fn EXISTS to serve — LITE mid-ramp rolls 15x its annual (FY 0.37 vs TTM
    // 5.7) — and still rejects unit confusion by 2+ orders of magnitude (HAL is 46,500x). It catches
    // unit confusion, NOT implausible valuation: a merely-wrong 50x roll still passes, by design.
    // The 1.0 floor stops a near-breakeven annual (0.01) from making an honest 0.50 roll look like 50x.
    let ok = |end: NaiveDate, ttm: f64| {
        (fresh(end) && ttm.abs() <= 100.0 * fy_eps.abs().max(1.0)).then_some(ttm)
    };
    // current YTD = the longest cumulative period that ends AFTER the latest 10-K (into the new fiscal year)
    let current = ytd
        .iter()
        .filter(|(e, _, _)| *e > fy_end)
        .max_by_key(|(_, _, span)| *span);
    let Some(&(cur_end, cur_val, cur_span)) = current else {
        return ok(fy_end, fy_eps); // no newer quarter -> the annual EPS IS the trailing year
    };
    // prior-year YTD of the SAME length: end ≈ current end − 1y, span ≈ current span
    let prior = ytd.iter().find(|(e, _, span)| {
        let dy = (cur_end - *e).num_days();
        (350..=380).contains(&dy) && (*span - cur_span).abs() <= 20
    });
    match prior {
        Some(&(_, prior_val, _)) => ok(cur_end, fy_eps + cur_val - prior_val),
        None => ok(fy_end, fy_eps), // can't de-cumulate without the prior-year YTD -> honest annual fallback
    }
}

/// (TER) ETF annual expense ratio (%) from FMP `stable/etf/info` (`expenseRatio`, a FRACTION -> ×100).
/// Disk-cached + budget-capped via [`cached_fund_json`]. None unless FMP_API_KEY is set AND the symbol is
/// an FMP-covered ETF — FMP's free tier is US-centric, so EU-listed UCITS ETFs (e.g. VUAA.DE) often
/// return nothing and the column stays n/a for them.
/// ponytail: scale is the FMP convention; if a known US ETF prints 100× off, drop the ×100 here.
async fn fetch_expense(client: &Client, urls: &Urls, ticker: &str, name: &str) -> Option<f64> {
    // Börse Frankfurt TER (captured for free during the universe build) first — it covers the EU UCITS
    // ETFs FMP's US-centric free tier leaves n/a. FMP fallback for US-listed ETFs not in the BF list.
    if let Some(t) = bf_ter_exact(ticker) {
        return Some(t);
    }
    // A pinned EU listing often sits on a different venue than the map's ISIN-resolved one (SPYL.DE
    // pinned, map holds SPYL.L): issuers keep one mnemonic per fund across venues, so a same-stem hit
    // is the same fund's TER. ETF-classified quotes only — an equity ticker could stem-collide.
    if let Some(t) = ticker.split_once('.').and_then(|(stem, _)| {
        BF_TER.get()?.iter().find(|(k, _)| k.split('.').next() == Some(stem)).map(|(_, t)| *t)
    }) {
        return Some(t);
    }
    // Last BF resort: unique fund-name prefix (rescues VVSM.DE, whose venue the symbol map can't reach).
    // Retry without Yahoo's umbrella-company prefix: "Amundi Index Solutions - Amundi S&P 500 Swap UCITS
    // ETF EUR Acc" — BF lists only the part after " - " (rescues AUM5.DE, TER 0.15%).
    if let Some(t) =
        bf_ter_by_name(name).or_else(|| name.split_once(" - ").and_then(|(_, fund)| bf_ter_by_name(fund)))
    {
        return Some(t);
    }
    let v = cached_fund_json(client, &urls.fund_expense, ticker, "etf").await?;
    let ter = v.get(0).unwrap_or(&v).get("expenseRatio")?.as_f64()?;
    (ter.is_finite() && ter > 0.0).then_some(ter * 100.0)
}

/// Exact Börse-Frankfurt TER map hit. Safe for ANY quote type: presence under the exact resolved
/// symbol proves the listing is an ETP, so Yahoo mislabeling an ETC as EQUITY (physical-gold ETCs)
/// can't hide its TER.
fn bf_ter_exact(ticker: &str) -> Option<f64> {
    BF_TER.get().and_then(|m| m.get(ticker)).copied()
}

// (G) Cold-fetch budget for the historical-fundamentals lane: FMP free tier = 250 calls/day, so cap
// NEW network fetches per run and serve everything else from the disk cache. note: process-wide
// counter, no cross-run persistence — the disk cache is what actually amortizes the budget over days.
static FUND_FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const FUND_FETCH_BUDGET: usize = 200; // leave headroom under 250/day for the live P/E/ROE calls

fn fund_cache_path(ticker: &str) -> std::path::PathBuf {
    crate::config::data_path(".fmp_cache").join(format!("{}.json", ticker.replace(['/', '\\'], "_")))
}

/// Persistent ISIN -> Yahoo-symbol map (one flat JSON object, gitignored like `.fmp_cache`).
/// Only POSITIVE resolutions live here — see the cache block in `fetch_xetra_etfs`.
pub(crate) const ISIN_CACHE_PATH: &str = ".isin_cache.json";

/// Last-good venue ISIN lists (`{"euronext": [...], "six": [...]}`), the fallback when a venue
/// endpoint has an outage — see the store block in `fetch_universe`.
const VENUE_ISINS_PATH: &str = ".venue_isins.json";

/// Weekly regulatory ETF-ISIN list (`{"fetched": "YYYY-MM-DD", "isins": [...]}`) scanned out of
/// the ESMA + FCA FIRDS dumps — see `fetch_regulatory_etf_isins`. Doubles as the last-good
/// fallback when a registry or download leg fails.
const REGULATORY_ISINS_PATH: &str = ".regulatory_isins.json";

/// Definitive Yahoo "no such ISIN" answers (`{isin: "YYYY-MM-DD"}`), retried after 30 days.
/// Only regulatory-sourced (speculative) ISINs land here — venue-list misses keep retrying every
/// run (round-36 flakiness lesson) — see the resolution block in `fetch_xetra_etfs`.
const ISIN_NEG_CACHE_PATH: &str = ".isin_negative_cache.json";

/// Weekly Yahoo quoteSummary fund facts (`{sym: ["YYYY-MM-DD", ter%|null, aum|null]}`) for funds
/// whose BF facts are missing — see `yahoo_fund_facts_fill`. Both-None rows are cached too, so a
/// factless fund costs one request a week, not one per run.
const FUND_FACTS_CACHE_PATH: &str = ".fund_facts_cache.json";

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
        shares: v.get("weightedAverageShsOutDil").and_then(|x| x.as_f64()).filter(|s| *s > 0.0),
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
/// (#37) Takes the WHOLE `BuyHeuristic`, not just the factor name: `peg_yield` divides by
/// `picks::long_cagr_pct`, which reads `use_life_cagr` / `use_trend_cagr` / `fixed_cagr_years`. Passing
/// only `factor` is what let this fn hardcode `trend_cagr` and drift from the `peg` column.
pub async fn enrich_fund_factor(client: &Client, urls: &Urls, quotes: &mut [core::Quote], tuning: &crate::config::BuyHeuristic) {
    const LIVE_TTL: StdDuration = StdDuration::from_secs(7 * 24 * 3600); // refetch weekly -> catches new filings
    let factor = tuning.growth_fund_factor.as_str();
    let today = chrono::Local::now().date_naive();
    // (FX) run-local rate cache. Only foreign filers whose listing trades in a DIFFERENT currency than
    // their books ever touch it, so it stays empty on a US-only universe and costs at most one fetch per
    // distinct reporting currency per run.
    let fx = fx_cache();
    let needs_insider = factor == "insider_net_buys_90d"; // (Item 16) composite stays FMP-only (no skew)
    for q in quotes.iter_mut() {
        if crate::picks::is_currency_quoted(&q.ticker) {
            continue; // crypto/FX -> no income statement, don't spend a budget slot probing.
                      // Suffix check, NOT any dash: BRK-B/BF-B are share classes the backtest
                      // validated WITH the factor — a contains('-') here was a train-serve skew.
        }
        evict_if_stale(&fund_cache_path(&q.ticker), LIVE_TTL); // (Item 14) drop a stale newest-quarter
        let fetched = fetch_fundamentals_ranked(client, urls, &q.ticker).await;
        // (FX) the currency the statements are KEPT in travels with the rows. Newest row wins — a filer
        // can redenominate, and the live factors are as-of today anyway. None for FMP-sourced rows,
        // which is exactly the signal to leave the price join untouched.
        let fund_ccy = fetched.as_ref().and_then(|r| r.last().and_then(|x| x.currency.clone()));
        let mut ff = fetched
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
        // (#1) prefer the SEC roll-forward TTM EPS — the SAME number behind the fixed live P/E — over the
        // annual-only `eps_ttm` fund_factors derives from SEC 10-K rows (which is stale for a mid-ramp
        // grower: LITE FY 0.37 vs TTM 5.7). Only when this factor is selected, so other factors pay no
        // fetch; US filers only (non-US keeps the derived value); the sidecar cache makes it near-free.
        // DO NOT widen this to `peg_yield`, however obvious it looks. `peg_yield` is the SHIPPED factor,
        // and it deliberately reads the annual `eps_ttm` because that is the only thing the backtest can
        // reconstruct as-of — a TTM roll needs quarterly data the as-of path does not have. Adding
        // `peg_yield` here would MANUFACTURE a train-serve skew: the live tilt would score on a number
        // the validation never saw. The latent skew is confined to `earnings_yield`, which isn't shipped.
        if factor == "earnings_yield" {
            if let Some(ttm) = sec_ttm_eps(client, urls, &q.ticker).await {
                ff.eps_ttm = Some(ttm);
            }
        }
        // (FX) the price these per-share figures may legally be divided by. A filer whose listing trades
        // in its own reporting currency (every US name) takes `close_native` untouched and costs NO fetch;
        // only a genuine mismatch (ASML: EUR statements, USD ADR) pays one rate lookup, cached per run.
        let price = match (q.close_native, fund_ccy.as_deref(), q.quote_currency.as_deref()) {
            (Some(c), Some(fund), Some(nat)) => price_in(client, urls, &fx, c, nat, fund).await,
            // unknown reporting currency (FMP rows) or unknown quote currency -> unchanged legacy
            // behaviour: the native close, exactly as before this existed.
            (c, _, _) => c,
        };
        ff.earnings_yield = price.and_then(|p| core::earnings_yield(ff.eps_ttm, p));
        // (PEG) same native-close discipline one line up, so `growth_fund_factor: peg_yield` is a
        // real selection instead of a silent None (which would zero the fund term and drop the
        // validated earnings_yield tilt with nothing in its place). `peg_yield` None-outs
        // loss-makers and non-positive growth itself — no extra guard here. Mirrors report.rs's
        // info cell, so the drill-in number and the scored number are one definition.
        //
        // (#37) growth term is `long_cagr_pct` — the SCORE's CAGR — not `q.trend_cagr`. It was
        // trend_cagr until 2026-07-27, which under `use_life_cagr: true` is a different arm of the
        // same switch the `peg` COLUMN follows: the gate cut APH at 2.02 while ranking ODFL at 2.51.
        // The backtest loop and report.rs mirror this exact call; all three must move together.
        ff.peg_yield = price.and_then(|p| core::peg_yield(ff.eps_ttm, crate::picks::long_cagr_pct(q, tuning), p));
        q.fund_factor = core::select_fund_factor(&ff, factor);
        // (G+) carry the whole struct so `growth_fund_extra`'s named terms resolve here too. Set AFTER
        // the price-dependent fields above, or the extra terms would read a half-built earnings_yield.
        q.fund = Some(ff);
    }
}

/// Fill the DISPLAY-ONLY income-statement snapshot (rev_yoy / eps_yoy / net_margin_fy) on the quotes
/// named in `targets` — the screen's ranked top stocks + pinned stocks, NOT the whole universe: the
/// columns only print for displayed rows, and enriching ~500 S&P names cold would burn the FMP daily
/// budget that P/E-ROE and the fund tilt share. Same pipeline `report` prints (report fetch — FMP
/// first, SEC EDGAR fallback for US filers when FMP is capped — → annual_rollup → newest complete FY),
/// cache-first, so warm runs cost zero requests. The per-ticker source mix is fine HERE because these
/// cells are never ranked (the skew rule guards the fund tilt, not display columns). Never scored —
/// the fund-factor family measured null for ranking; this is legibility for the human.
pub async fn enrich_income_stmt(client: &Client, urls: &Urls, quotes: &mut [core::Quote], targets: &HashSet<String>) {
    const LIVE_TTL: StdDuration = StdDuration::from_secs(7 * 24 * 3600); // weekly refetch, like the fund tilt
    for q in quotes.iter_mut() {
        // crypto/FX (currency-quoted tickers) and funds carry no income statement -> don't spend a
        // budget slot (suffix check: BRK-B is a stock and gets its columns)
        if !targets.contains(&q.ticker) || crate::picks::is_currency_quoted(&q.ticker) || q.instrument_type.eq_ignore_ascii_case("ETF") {
            continue;
        }
        evict_if_stale(&fund_cache_path(&q.ticker), LIVE_TTL);
        if let Some((rows, source)) = fetch_fundamentals_report(client, urls, &q.ticker).await {
            let annual = core::annual_rollup(&rows);
            if let Some(snap) = core::income_snapshot(&annual) {
                (q.rev_yoy, q.eps_yoy, q.net_margin_fy, q.buyback_yoy) = snap;
            }
            // (B) same rollup, kept this time: the multi-year trajectory line for screen's
            // fundamentals footer. Zero extra requests — this was fetched and discarded before.
            q.annual_brief = core::annual_brief(&annual).map(|b| format!("{b}  [{source}]"));
        }
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
    crate::config::data_path(".sec_cache").join(format!("{}.json", ticker.replace(['/', '\\'], "_")))
}

// The SEC transport pair. `offline()` is checked HERE, in the two functions every SEC path funnels
// through, for the same reason `get_json`/`get_text` check it: OFFLINE has to mean no socket, and the
// eight call sites above are easy to add a ninth to without noticing. Without this an offline run
// still reached sec.gov whenever the local `.sec_cache` missed — which is precisely the case a frozen
// -data pin hits on a machine that has never run live, so the pin's hermeticity depended on the
// developer's cache being warm.
async fn sec_get_text(client: &Client, url: &str, ua: &str) -> Option<String> {
    if offline() {
        return None;
    }
    throttle().await;
    client.get(url).header("User-Agent", ua).send().await.ok()?.text().await.ok()
}
async fn sec_get_json(client: &Client, url: &str, ua: &str) -> Option<Value> {
    if offline() {
        return None;
    }
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
///
/// (EU listing) The lookup goes through the EU->US alias first, and that is the ONE seam that decides
/// whether a swapped name has any fundamentals at all. `prefer_eu_listing` renames S&P constituents onto
/// their Xetra twins BEFORE any fundamentals fetch, and every SEC read in this file funnels through here
/// — P/E, ROE/A, the TTM EPS roll, the income statement, the insider factor. Keyed by US ticker, a Xetra
/// symbol misses and the whole block goes dark at once: a live run reported `222 of 549 stocks carry the
/// factor` with 266 constituents swapped. Aliasing costs one hash lookup and restores all of it.
///
/// The alias is safe to apply unconditionally: the map's KEYS are Xetra symbols, which only ever reach
/// this fn when the swap actually ran, so a US-only run cannot hit one. No `prefer_eu_listing` plumbing.
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
    let key = EU_TO_US
        .get_or_init(|| {
            let eu: HashMap<String, String> = std::fs::read_to_string(crate::config::data_path(EU_LISTING_CACHE_PATH))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            invert_eu(&eu)
        })
        .get(&ticker.to_uppercase())
        .cloned()
        .unwrap_or_else(|| ticker.to_uppercase());
    map.get(&key).cloned()
}

/// (EU listing) `US -> EU` inverted to `EU -> US`, for [`sec_cik`]. Read once from the resolver's cache.
static EU_TO_US: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();

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

// A NAMED struct, not the positional tuple this used to be: serde only implements Serialize for tuples
// up to 16 elements and the row outgrew that at 18. Names are strictly better anyway — the tuple came
// with a hand-maintained index map in a comment, and reading `roe` at slot 7 when slot 7 was `shares`
// is a mistake the compiler could not catch. Field names cost a few hundred bytes per cached ticker.
// A shape change fails deserialization -> treated as a miss -> refetched + rewritten (SEC is uncapped,
// so a one-time rebuild is free), which is also why every field is a plain Option with no serde attrs.
#[derive(serde::Serialize, serde::Deserialize)]
struct SecCacheRow {
    filed: String,
    period_end: String,
    revenue: Option<f64>,
    gross_margin: Option<f64>,
    op_margin: Option<f64>,
    net_margin: Option<f64>,
    eps: Option<f64>,
    roe: Option<f64>,
    shares: Option<f64>,
    fcf_margin: Option<f64>,
    interest_cover: Option<f64>,
    net_cash_rev: Option<f64>,
    ebitda: Option<f64>,
    net_debt: Option<f64>,
    currency: Option<String>, // (FX) money lines are meaningless against a price without it
    roa: Option<f64>,         // the ROE fallback for negative-equity filers
    prior_eps: Option<f64>,   // the prior FY's EPS as THIS row's filing stated it (the YoY denominator)
    prior_shares: Option<f64>, // likewise for the share count
}

/// ANNUAL report forms. `10-K` = US domestic; `20-F` = foreign private issuer (ASML, ARM, BABA); `40-F`
/// = Canadian MJDS (RY, TD). All three are the filer's ONE yearly report, which is what the 350-380 day
/// span check downstream assumes. `6-K` is deliberately EXCLUDED: it is the foreign INTERIM filing, and
/// admitting it would splice sub-year slices into rows that must each cover one fiscal year.
fn is_annual_form(f: &str) -> bool {
    f.starts_with("10-K") || f.starts_with("20-F") || f.starts_with("40-F")
}

/// Concept names per XBRL taxonomy — the same income/balance lines under two different vocabularies.
/// `us-gaap` covers US filers AND the foreign issuers who file 20-F in us-gaap (ASML, ARM, BABA);
/// `ifrs-full` covers most of the rest (NVS, AZN, NVO, RY, TD). Every IFRS tag here was verified present
/// in live companyfacts payloads rather than guessed from the spec. Coverage is uneven by filer and a
/// missing tag stays None (neutral): a bank with no operating-income line loses op_margin, not the row.
struct FactTags {
    rev: &'static [&'static str],
    gp: &'static [&'static str],
    op: &'static [&'static str],
    dna: &'static [&'static str],
    ni: &'static [&'static str],
    eps: &'static [&'static str],
    shares: &'static [&'static str],
    eq: &'static [&'static str],
    assets: &'static [&'static str],
    ocf: &'static [&'static str],
    capex: &'static [&'static str],
    intexp: &'static [&'static str],
    debt_nc: &'static [&'static str],
    debt_cur: &'static [&'static str],
    cash: &'static [&'static str],
}

// ExcludingAssessedTax listed before Including: when a filer reports both for a period (values differ by
// the assessed taxes) the first-inserted wins on a filed-date tie, and Excluding is the cleaner revenue
// line. ServicesNet/GoodsNet = the pre-2018 (pre-ASC-606) era of service/goods filers (e.g. ODFL) whose
// whole history was invisible without them.
const US_GAAP_TAGS: FactTags = FactTags {
    rev: &["Revenues", "RevenueFromContractWithCustomerExcludingAssessedTax",
           "RevenueFromContractWithCustomerIncludingAssessedTax", "SalesRevenueNet",
           "SalesRevenueServicesNet", "SalesRevenueGoodsNet"],
    gp: &["GrossProfit"],
    op: &["OperatingIncomeLoss"],
    dna: &["DepreciationDepletionAndAmortization", "DepreciationAndAmortization",
           "DepreciationAmortizationAndAccretionNet"],
    // NetIncomeLoss first (parent-company net income, the standard tag); some filers stopped filing it
    // years ago (CF in 2011, MNST) and carry only the available-to-common / ProfitLoss variants — their
    // net margin AND ROE were silently None without the fallbacks.
    ni: &["NetIncomeLoss", "NetIncomeLossAvailableToCommonStockholdersBasic", "ProfitLoss"],
    // Same tag-switch story as `ni` above, and for several of the same filers: the diluted-EPS series
    // simply STOPS (MNST 2010, KIM 2018, HAL 2019, EXC/FCX 2021, VRSN 2022) when the filer moves to the
    // continuing-operations concept, and ABNB/REG never used the standard tag at all. Single-tag, this
    // list left 9 tickers with no EPS on their newest annual row -> no eps_yoy, no eps_growth, no PEG.
    // ORDER IS LOAD-BEARING: `collect` merges all tags into one map keyed by period end and keeps the
    // lowest `filed`, strictly. A filer reports diluted and basic in the SAME 10-K on the SAME day
    // (AAPL FY2025: both filed 2025-10-31, 7.46 vs 7.49), so on a tie the strict `<` keeps whichever is
    // listed FIRST — diluted. Measured across the cached universe: 487/509 tickers unchanged, 17 gained
    // an EPS they never had, and 5 (AIG/DELL/FAST/O/WTW) had a value REPLACED — not by the ordering, but
    // because a fallback tag was filed STRICTLY EARLIER. That is the earliest-filed policy working, and
    // it removes look-ahead rather than adding it: DELL's FY2024 original 10-K (filed 2024-03-25) tagged
    // only continuing-ops 4.36, while the 4.60 this list used to serve first appears a YEAR later as a
    // comparative in the FY2025 10-K. Same shape for FAST 2009 (1.24 as-reported in 2011 vs 0.62
    // restated post-split in 2012). Do not "fix" these back.
    // Two things this deliberately accepts: continuing-ops EXCLUDES discontinued operations, so for a
    // spin-off filer (EXC/Constellation) it is not the headline EPS — it is the cleaner recurring one;
    // and `EarningsPerShareBasic` is basic, ~0.4% above diluted on AAPL-shaped dilution, which is
    // exactly why it is last. It is here for LEN, whose FY2025 has basic but no diluted.
    eps: &["EarningsPerShareDiluted", "IncomeLossFromContinuingOperationsPerDilutedShare",
           "EarningsPerShareBasic"],
    shares: &["WeightedAverageNumberOfDilutedSharesOutstanding", "WeightedAverageNumberOfSharesOutstandingBasic"],
    eq: &["StockholdersEquity", "StockholdersEquityIncludingPortionAttributableToNoncontrollingInterest"],
    // total assets — the ROE fallback denominator. Unlike equity it CANNOT go negative, which is the
    // whole point: a buyback-shrunk filer (HCA, HLT) has meaningless ROE but perfectly ordinary ROA.
    // Same literal in both taxonomies (verified live: AZN/NVS USD, NVO DKK), hence no IFRS-specific name.
    assets: &["Assets"],
    ocf: &["NetCashProvidedByUsedInOperatingActivities", "NetCashProvidedByUsedInOperatingActivitiesContinuingOperations"],
    capex: &["PaymentsToAcquirePropertyPlantAndEquipment", "PaymentsToAcquireProductiveAssets"],
    intexp: &["InterestExpense", "InterestExpenseDebt", "InterestAndDebtExpense"],
    debt_nc: &["LongTermDebtNoncurrent", "LongTermDebt"],
    debt_cur: &["LongTermDebtCurrent", "DebtCurrent"],
    cash: &["CashAndCashEquivalentsAtCarryingValue", "CashCashEquivalentsRestrictedCashAndRestrictedCashEquivalents"],
};

// IFRS. `ProfitLoss`/`Equity`/`CashFlowsFromUsedInOperatingActivities`/`CashAndCashEquivalents` were
// present in 7/7 filers probed, so ROE and net_cash_rev carry broadly; `GrossProfit` and the D&A tag
// only 3/7, so gross_margin and EBITDA are frequently None here. NOTE the row ANCHOR is revenue — a
// filer with neither revenue tag (HSBC, a bank) yields NO rows at all, not partial ones.
const IFRS_TAGS: FactTags = FactTags {
    rev: &["Revenue", "RevenueFromSaleOfGoods"],
    gp: &["GrossProfit"],
    op: &["ProfitLossFromOperatingActivities"],
    dna: &["DepreciationAndAmortisationExpense"],
    ni: &["ProfitLoss", "ProfitLossAttributableToOwnersOfParent"],
    eps: &["DilutedEarningsLossPerShare", "BasicEarningsLossPerShare"],
    shares: &["AdjustedWeightedAverageShares", "WeightedAverageShares"],
    eq: &["Equity", "EquityAttributableToOwnersOfParent"],
    // same tag string as us-gaap. Starts LATER than Equity for several IFRS filers (AZN 2015 vs 2014,
    // NVS 2017, NVO 2020), so early rows carry ROE but no ROA — missing stays None, never a fake 0.
    assets: &["Assets"],
    ocf: &["CashFlowsFromUsedInOperatingActivities"],
    capex: &["PurchaseOfPropertyPlantAndEquipmentClassifiedAsInvestingActivities"],
    intexp: &["InterestExpense"],
    debt_nc: &["LongtermBorrowings", "Borrowings"],
    debt_cur: &["CurrentPortionOfLongtermBorrowings", "ShorttermBorrowings"],
    cash: &["CashAndCashEquivalents"],
};

/// (FX) The currency the filer REPORTS in, read off the XBRL unit key instead of assumed. A 20-F filer
/// uses its own books' currency (ASML in EUR), so the old hard-coded "USD" matched nothing at all and
/// silently produced zero rows. Read from the money anchors, skipping the unitless share counts and the
/// compound "CUR/shares" EPS keys. USD is PREFERRED when a filer tags both (TM and BABA publish a USD
/// convenience translation next to JPY/CNY): it is the one that already matches their ADR's trading
/// currency, so choosing it removes an FX conversion instead of adding one. Otherwise the lexicographic
/// minimum, purely so the pick is deterministic run to run.
fn money_unit(g: &Value, tags: &FactTags) -> Option<String> {
    let mut best: Option<String> = None;
    for tag in tags.rev.iter().chain(tags.ni).chain(tags.eq) {
        let Some(units) = g.get(tag).and_then(|t| t.get("units")).and_then(|u| u.as_object()) else {
            continue;
        };
        for k in units.keys().filter(|k| *k != "shares" && !k.contains('/')) {
            if k == "USD" {
                return Some(k.clone());
            }
            if best.as_ref().is_none_or(|b| k < b) {
                best = Some(k.clone());
            }
        }
    }
    best
}

/// Parse a SEC `companyfacts` payload into ANNUAL `FundRow`s (one per fiscal year). Pure -> unit-tested.
/// Revenue is merged across the concepts different eras/filers use; each annual line is joined to the
/// others by exact period-end date (an annual report's income lines all share one period end). Margins
/// derived (line / revenue), matching `parse_fund_row`. A missing line -> None (neutral), never a fake 0.
fn parse_sec_facts(j: &Value) -> Vec<core::FundRow> {
    // us-gaap first — it covers US filers AND the foreign issuers who file 20-F in us-gaap; ifrs-full is
    // the rest of the foreign world. The concept NAMES differ per taxonomy, so the tag table travels
    // with the pointer rather than being read from a single global list.
    let (g, tags) = match j.pointer("/facts/us-gaap") {
        Some(v) => (v, &US_GAAP_TAGS),
        None => match j.pointer("/facts/ifrs-full") {
            Some(v) => (v, &IFRS_TAGS),
            None => return Vec::new(),
        },
    };
    let Some(money) = money_unit(g, tags) else {
        return Vec::new(); // no money unit anywhere -> nothing joinable to parse
    };
    let per_share = format!("{money}/shares");
    // annual datapoints for a set of equivalent concept names:
    //   period-end -> (earliest filed, value, the PRIOR annual period's value FROM THAT SAME FILING).
    // The third slot is the whole point. Keeping the earliest filing per period is right for a LEVEL
    // (it is what was knowable then, no look-ahead) but it makes every RATIO divide two different
    // bases, because each 10-K restates its comparatives at the CURRENT share basis. TPL split 3-for-1
    // in 2024 and again in 2025, so its stored series runs 7.69M / 23.02M / 69.03M — three bases in
    // one column, reading as +199.9% share growth twice. The prior-from-the-same-filing is measured
    // against the identical basis by construction: no split ratio to infer, no restatement to detect,
    // and it also settles the case a ratio never can — whether a share jump was a split (comparatives
    // restated, TPL) or a real issuance (comparatives untouched, COF buying Discover).
    // Look-ahead-free: this value was printed in the same document, on the same day, as `value`.
    let collect = |names: &[&str], unit: &str| -> std::collections::BTreeMap<NaiveDate, (NaiveDate, f64, Option<f64>)> {
        // period end -> (filed, WHICH tag won, value). The tag index rides along so the prior below is
        // read from the SAME concept: mixing a diluted `value` with a basic `prior` would re-introduce
        // a (small, ~0.4%) basis mismatch, which is the exact bug class this field exists to kill.
        let mut m: std::collections::BTreeMap<NaiveDate, (NaiveDate, usize, f64)> = std::collections::BTreeMap::new();
        // (tag, filed, period end) -> value. Every datapoint, not just the winners — the prior year of
        // a filing is normally NOT a winner anywhere (its own original 10-K filed it first).
        let mut seen: std::collections::BTreeMap<(usize, NaiveDate, NaiveDate), f64> = std::collections::BTreeMap::new();
        for (tag_idx, tag) in names.iter().enumerate() {
            // chained get (NOT json pointer): the "USD/shares" unit key contains a '/', which a JSON
            // pointer would mis-split into two tokens -> EPS silently lost.
            let arr = match g.get(tag).and_then(|t| t.get("units")).and_then(|u| u.get(unit)).and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };
            for x in arr {
                if !x.get("form").and_then(|v| v.as_str()).is_some_and(is_annual_form) {
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
                // `or_insert`, not overwrite: on a repeat the first tag listed wins, matching the
                // strict `<` below so `seen` and `m` never disagree about which concept is authoritative.
                seen.entry((tag_idx, filed, ed)).or_insert(val);
                // keep the ORIGINAL report (lowest filed) for this period end, not a later restatement
                m.entry(ed).and_modify(|cur| {
                    if filed < cur.0 {
                        *cur = (filed, tag_idx, val);
                    }
                }).or_insert((filed, tag_idx, val));
            }
        }
        // attach each winner's same-filing prior: the greatest period end BEFORE this one that the same
        // filing also reported, under the same concept. `range` over the (tag, filed, end) key is an
        // exact scan of one document's comparatives — a 10-K carries two or three, so this normally
        // hits. Nothing there (first year of coverage, or a filer that prints no comparative) -> None,
        // and the callers fall back to the cross-filing read they used before.
        m.into_iter()
            .map(|(end, (filed, tag_idx, val))| {
                let prior = seen
                    .range((tag_idx, filed, NaiveDate::MIN)..(tag_idx, filed, end))
                    .next_back()
                    .map(|(_, v)| *v);
                (end, (filed, val, prior))
            })
            .collect()
    };
    // Balance-sheet items (equity) are INSTANT (as-of period_end, no 12-month duration), so the
    // 350-380 day filter in `collect` drops them — a separate point-in-time collector keyed on `end`,
    // annual forms only, earliest-filed wins (matches `collect`). Same 3-slot value shape as `collect`
    // so one `at` accessor serves both, with the prior always None: no consumer wants a year-over-year
    // ratio of a BALANCE-sheet instant, and a slot nobody reads beats a second accessor everywhere.
    let collect_instant = |names: &[&str], unit: &str| -> std::collections::BTreeMap<NaiveDate, (NaiveDate, f64, Option<f64>)> {
        let mut m: std::collections::BTreeMap<NaiveDate, (NaiveDate, f64, Option<f64>)> = std::collections::BTreeMap::new();
        for tag in names {
            let arr = match g.get(tag).and_then(|t| t.get("units")).and_then(|u| u.get(unit)).and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };
            for x in arr {
                if !x.get("form").and_then(|v| v.as_str()).is_some_and(is_annual_form) {
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
                        *cur = (filed, val, None);
                    }
                }).or_insert((filed, val, None));
            }
        }
        m
    };
    // Concept names come from the taxonomy table; the unit is the filer's OWN reporting currency, not a
    // hard-coded "USD" (that matched nothing for a EUR filer like ASML). `shares` is a unitless count and
    // stays literal — it is the one line that never carries a currency.
    let rev = collect(tags.rev, &money);
    let gp = collect(tags.gp, &money);
    let op = collect(tags.op, &money);
    // (EV/EBITDA probe) D&A from the cash-flow statement — the add-back that turns operating income into
    // EBITDA. Duration concept. Missing -> EBITDA None-outs (a partial EBITDA is garbage).
    let dna = collect(tags.dna, &money);
    let ni = collect(tags.ni, &money);
    let eps = collect(tags.eps, &per_share);
    // diluted weighted-avg shares — a 12-month DURATION concept (unit "shares") -> `collect`, not
    // `collect_instant`. Basic fallback for the rare filer that never reports diluted. Feeds the buyback column.
    let shares = collect(tags.shares, "shares");
    let eq = collect_instant(tags.eq, &money);
    let assets = collect_instant(tags.assets, &money);
    // (round 107) survival inputs. Duration lines: operating cash flow, capex, interest expense.
    let ocf = collect(tags.ocf, &money);
    let capex = collect(tags.capex, &money);
    let intexp = collect(tags.intexp, &money);
    // Instant balance items: debt (noncurrent + current, collected separately — the total-debt tag is
    // only the fallback inside the noncurrent map) and cash. A missing debt tag reads as 0 debt, which
    // can only make net_cash_rev OPTIMISTIC — a reject-the-worst gate then under-rejects, never wrongly
    // rejects, so the failure direction is safe. Cash is the parse anchor: no cash line -> None.
    let debt_nc = collect_instant(tags.debt_nc, &money);
    let debt_cur = collect_instant(tags.debt_cur, &money);
    let cash = collect_instant(tags.cash, &money);
    rev.into_iter()
        .map(|(end, (filed, revenue, _))| {
            type Collected = std::collections::BTreeMap<NaiveDate, (NaiveDate, f64, Option<f64>)>;
            let at = |m: &Collected| m.get(&end).map(|(_, v, _)| *v);
            // the same period's PRIOR-YEAR value as the winning filing stated it — see `collect`
            let at_prior = |m: &Collected| m.get(&end).and_then(|(_, _, p)| *p);
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
                shares: at(&shares),
                // the year-over-year denominators, on THIS row's basis. Every ratio built from these
                // two compares like with like even across a split or a restatement.
                prior_eps: at_prior(&eps),
                prior_shares: at_prior(&shares),
                // ROE = net income ÷ shareholders' equity (%), both as-of this period end. Free from
                // SEC — no premium ratios endpoint needed.
                roe: at(&ni).zip(at(&eq)).and_then(|(n, e)| (e != 0.0).then_some(n / e * 100.0)),
                // ROA = net income ÷ TOTAL ASSETS (%) — same numerator, a denominator that cannot go
                // negative. `> 0.0` not `!= 0.0`: total assets are positive by construction, so a
                // non-positive value is a parse artifact, not a leveraged balance sheet.
                roa: at(&ni).zip(at(&assets)).and_then(|(n, a)| (a > 0.0).then_some(n / a * 100.0)),
                // (round 107) survival levels, high = safer. fcf needs BOTH lines (a bank with no capex
                // tag stays None-neutral, not fake-frugal); interest_cover needs a positive interest
                // expense (debt-free -> None-neutral, and a negative op income reads as negative cover).
                fcf_margin: at(&ocf).zip(at(&capex)).and_then(|(o, c)| margin(Some(o - c))),
                interest_cover: at(&op).zip(at(&intexp)).and_then(|(o, i)| (i > 0.0).then_some(o / i)),
                net_cash_rev: at(&cash).and_then(|c| {
                    margin(Some(c - at(&debt_nc).unwrap_or(0.0) - at(&debt_cur).unwrap_or(0.0)))
                }),
                // (EV/EBITDA probe) raw as-of levels. EBITDA needs BOTH op income and D&A (a partial is
                // garbage). net_debt = total debt − cash, cash the anchor (None -> None), missing debt reads
                // 0 (same optimistic-safe direction as net_cash_rev). Sign is OPPOSITE net_cash_rev's (debt −
                // cash, so + = levered) because EV ADDS net debt.
                ebitda: at(&op).zip(at(&dna)).map(|(o, d)| o + d),
                net_debt: at(&cash).map(|c| at(&debt_nc).unwrap_or(0.0) + at(&debt_cur).unwrap_or(0.0) - c),
                // (FX) every money line above is in THIS currency. The margins/ROE are ratios and cancel
                // it; anything later joined to a price must convert first or land in the Item 16 trap.
                currency: Some(money.clone()),
                ..Default::default()
            }
        })
        .collect()
}

/// EPS + weighted-average shares per fiscal year out of ONE 10-K's RAW XBRL INSTANCE. Pure -> unit-tested.
///
/// WHY THIS EXISTS AT ALL. `companyfacts`/`companyconcept` expose only UNDIMENSIONED facts. A multi-class
/// filer tags every per-share figure against `StatementClassOfStockAxis`, so the API serves it NOTHING:
/// V, HSY, STZ, BKR, ERIE and KKR all 404 on `EarningsPerShareDiluted` while happily serving revenue and
/// net income. 8 of 509 cached US filers (1.6%) — see the caller for the one that is unfixable. The
/// filing's own instance document keeps the dimensions, so the numbers are there, just not through the API.
///
/// WHICH CLASS, in two steps: keep the classes whose `shares x eps` RECONCILES with the same period's
/// undimensioned net income, then take the one with the MOST SHARES. Both steps are load-bearing and each
/// is decided by a real filer:
///
/// - RECONCILIATION is what a member-name allowlist cannot do. ERIE's Class B is EPS 1,801 on 2,542 shares
///   (product $0.005B) against Class A's 10.69 on 52.3M ($0.559B = net income exactly) — and no allowlist
///   knows in advance that ERIE and V trade as `CommonClassAMember` while HSY and KKR tag
///   `CommonStockMember`. The 10% band is a SEPARATOR, not a precision claim: real classes land <=0.2% off
///   and wrong ones >=75% off, with room for a filer whose `NetIncomeLoss` includes noncontrolling
///   interests its per-share line excludes (KKR, 5.6%, and a sole candidate anyway).
/// - MOST SHARES is what "closest product" cannot do, and getting it wrong is not a rounding error.
///   Berkshire tags BOTH equivalent classes and BOTH reconcile: `EquivalentClassAMember` 46,563 on
///   1,438,223 shares = $66.968B, `EquivalentClassBMember` 31.04 on 2,157,335,139 = $66.964B, against net
///   income $66.968B. Closest-product picks Class A on floating-point noise, and BRK-B trades at ~1/1500
///   of a Class A share. The corrupted quantity is P/E, and ONLY P/E: EPS *growth* is a ratio of two
///   numbers in the same units, so it is scale-invariant and reads −24.8% either way. That is what made
///   this survivable — a declining EPS has no usable PEG, so BRK-B never surfaced. One growing year and
///   P/E 0.01 becomes PEG ~0.0004: through the `growth_max_peg` ceiling with a maxed `peg_yield` tilt.
///   The rule is not a tiebreak: a per-class EPS restates the WHOLE company in that class's units, so the
///   finest-grained unit carries the largest count — which is the widely-held, retail-traded line, the
///   one whose price these numbers get divided into.
///
/// Verified on 7/7 against the real filings: BRK-B 31.04 (66.964B vs 66.968B), V 10.20 (20.05/20.058),
/// HSY 4.34 (0.883/0.883), STZ 9.61 (1.687/1.687), BKR 2.60 (2.584/2.588), ERIE 10.69 (0.559/0.559),
/// KKR 2.34 (2.236/2.37).
///
/// Same three rules as `parse_sec_facts` so the two agree: tag lists IN ORDER (diluted before basic — BKR
/// tags both), the 350-380 day span = one fiscal year, and a missing line stays None. Namespace prefixes
/// are matched loosely (`<context>` vs `<xbrli:context>`) because filers differ; attributes may be split
/// across lines, hence regex rather than a literal find.
fn parse_sec_instance(xml: &str, tags: &FactTags) -> std::collections::BTreeMap<NaiveDate, (f64, Option<f64>)> {
    use std::collections::BTreeMap;
    let empty = BTreeMap::new();
    // `[\w-]+:`, NOT `\w+:` — the two prefixes that matter, `us-gaap:` and `ifrs-full:`, both contain a
    // HYPHEN, which `\w` does not match. With `\w` this parser silently found zero facts on every real
    // filing; the unit test below is what caught it.
    let (Ok(ctx_re), Ok(start_re), Ok(end_re), Ok(mem_re)) = (
        regex::Regex::new(r#"(?s)<(?:[\w-]+:)?context id="([^"]+)"[^>]*>(.*?)</(?:[\w-]+:)?context>"#),
        regex::Regex::new(r"<(?:[\w-]+:)?startDate>\s*([0-9-]+)"),
        regex::Regex::new(r"<(?:[\w-]+:)?endDate>\s*([0-9-]+)"),
        regex::Regex::new(r"<(?:[\w-]+:)?explicitMember[^>]*>\s*([^<]*?)\s*</"),
    ) else {
        return empty;
    };
    // context id -> (fiscal year end, its dimension members joined). Durations only: a balance-sheet
    // context is instant and an interim one fails the span check, so neither can pollute an annual row.
    let mut ctx: HashMap<&str, (NaiveDate, String)> = HashMap::new();
    for c in ctx_re.captures_iter(xml) {
        let (id, body) = (c.get(1).map_or("", |m| m.as_str()), c.get(2).map_or("", |m| m.as_str()));
        let (Some(sd), Some(ed)) = (
            start_re.captures(body).and_then(|m| NaiveDate::parse_from_str(&m[1], "%Y-%m-%d").ok()),
            end_re.captures(body).and_then(|m| NaiveDate::parse_from_str(&m[1], "%Y-%m-%d").ok()),
        ) else {
            continue;
        };
        if !(350..=380).contains(&(ed - sd).num_days()) {
            continue;
        }
        let mut dims: Vec<&str> = mem_re.captures_iter(body).filter_map(|m| m.get(1)).map(|m| m.as_str()).collect();
        dims.sort_unstable(); // instance order is the filer's; sorted so one class keys identically everywhere
        ctx.insert(id, (ed, dims.join("|")));
    }
    // (fiscal year end, dimension key) -> value, first tag in the list wins (US_GAAP_TAGS order is
    // load-bearing: diluted before basic). `ni` is keyed on the year alone and takes ONLY the
    // undimensioned fact — the consolidated bottom line every class's product is measured against.
    let per_class = |names: &[&str], undimensioned_only: bool| -> BTreeMap<(NaiveDate, String), f64> {
        let mut m: BTreeMap<(NaiveDate, String), f64> = BTreeMap::new();
        for tag in names {
            let Ok(re) = regex::Regex::new(&format!(r#"<(?:[\w-]+:)?{tag}\s[^>]*contextRef="([^"]+)"[^>]*>([^<]*)</"#)) else {
                continue;
            };
            for f in re.captures_iter(xml) {
                let (Some((end, dims)), Ok(val)) = (ctx.get(&f[1]), f[2].trim().parse::<f64>()) else {
                    continue;
                };
                if undimensioned_only && !dims.is_empty() {
                    continue;
                }
                m.entry((*end, dims.clone())).or_insert(val);
            }
        }
        m
    };
    let eps = per_class(tags.eps, false);
    let shares = per_class(tags.shares, false);
    let ni = per_class(tags.ni, true);
    // every class that tagged an EPS this fiscal year, grouped so they can be compared against each other
    let mut by_year: BTreeMap<NaiveDate, Vec<(f64, Option<f64>)>> = BTreeMap::new();
    for ((end, dims), e) in &eps {
        by_year.entry(*end).or_default().push((*e, shares.get(&(*end, dims.clone())).copied()));
    }
    by_year
        .into_iter()
        .filter_map(|(end, cands)| {
            let n = ni.get(&(end, String::new())).copied();
            // RECONCILES = this class's `shares x eps` is the whole company. Wide band on purpose: on live
            // data a real class lands <=0.2% off and a wrong one >=75% off, and 10% leaves room for a
            // filer whose `NetIncomeLoss` carries noncontrolling interests the per-share line excludes
            // (KKR, 5.6%). It is a SEPARATOR, not a precision claim.
            let reconciles = |&(e, sh): &(f64, Option<f64>)| match (n, sh) {
                (Some(n), Some(s)) => (s * e - n).abs() <= n.abs() * 0.10,
                _ => false,
            };
            let most_shares = |a: &&(f64, Option<f64>), b: &&(f64, Option<f64>)| a.1.unwrap_or(0.0).total_cmp(&b.1.unwrap_or(0.0));
            cands
                .iter()
                .filter(|c| reconciles(c))
                .max_by(most_shares)
                // nothing reconciles (no net income for the year, or a filer whose bottom line and
                // per-share line genuinely disagree) -> the closest product, else the biggest class.
                .or_else(|| {
                    cands.iter().min_by(|a, b| {
                        let dist = |c: &(f64, Option<f64>)| match (n, c.1) {
                            (Some(n), Some(s)) => (s * c.0 - n).abs(),
                            (None, Some(s)) => -s,
                            _ => f64::INFINITY, // unscoreable, so it loses to any class that isn't
                        };
                        dist(a).total_cmp(&dist(b))
                    })
                })
                .map(|&c| (end, c))
        })
        .collect()
}

/// The newest annual filing's dimensioned EPS/shares for a US ticker, by fiscal year end. DISK-CACHED
/// (`.sec_cache/{ticker}_inst2.json`) INCLUDING THE EMPTY RESULT: BRK-B's instance is 13.5MB and KKR's
/// 19.8MB, so re-downloading those to re-learn nothing is exactly the cost worth caching away.
/// Budget-capped like every other SEC call. ceiling: ONE filing, so ~3 fiscal years of coverage; older
/// as-of rows keep the None they already had. Walking more 10-Ks is the upgrade, at 2.4-19.8MB each.
///
/// The key carries a VERSION DIGIT because this cache has no TTL: a parse that was wrong when it was
/// written stays wrong forever. `inst1` -> `inst2` retires the class picker that shipped BRK-B at EPS
/// 46,563 (Class A) — see `parse_sec_instance`. Bump it again on any change to what that parser picks.
async fn fetch_sec_instance_eps(client: &Client, urls: &Urls, ticker: &str) -> std::collections::BTreeMap<NaiveDate, (f64, Option<f64>)> {
    use std::sync::atomic::Ordering;
    let cache = sec_cache_path(&format!("{ticker}_inst2"));
    if let Some(c) = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str::<Vec<(String, f64, Option<f64>)>>(&s).ok()) {
        return c
            .into_iter()
            .filter_map(|(d, e, s)| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok().map(|d| (d, (e, s))))
            .collect(); // cache hit (empty included) -> no network, no budget spend
    }
    let mut found = std::collections::BTreeMap::new();
    // a network failure below leaves `found` empty and STILL writes the negative cache — deliberate: the
    // caller is the 1.6% tail, and a transient miss costs one blank column until the cache file is removed.
    if let Some(cik) = sec_cik(client, urls, ticker).await {
        if SEC_FETCHES.fetch_add(1, Ordering::Relaxed) < SEC_FETCH_BUDGET {
            if let Some(v) = sec_get_json(client, &urls.sec_submissions.replace("{cik}", &cik), &urls.sec_user_agent).await {
                let recent = v.get("filings").and_then(|f| f.get("recent"));
                let get = |k: &str, i: usize| recent?.get(k)?.as_array()?.get(i)?.as_str().map(str::to_string);
                let newest = recent
                    .and_then(|r| r.get("form"))
                    .and_then(|f| f.as_array())
                    .and_then(|forms| forms.iter().position(|f| f.as_str().is_some_and(is_annual_form)));
                if let Some(i) = newest.filter(|_| SEC_FETCHES.fetch_add(1, Ordering::Relaxed) < SEC_FETCH_BUDGET) {
                    if let (Some(acc), Some(doc)) = (get("accessionNumber", i), get("primaryDocument", i)) {
                        // the EXTRACTED instance sitting beside the inline-XBRL 10-K: same folder, primary
                        // document name with ".htm" swapped for "_htm.xml". ponytail: hardcoded like the
                        // `yahoo_crumb` endpoints — lift into `Urls` only if a test needs to stub it.
                        let url = format!(
                            "https://www.sec.gov/Archives/edgar/data/{}/{}/{}_htm.xml",
                            cik.trim_start_matches('0'),
                            acc.replace('-', ""),
                            doc.trim_end_matches(".htm")
                        );
                        if let Some(xml) = sec_get_text(client, &url, &urls.sec_user_agent).await {
                            found = parse_sec_instance(&xml, &US_GAAP_TAGS);
                            if found.is_empty() {
                                found = parse_sec_instance(&xml, &IFRS_TAGS); // unmeasured: no 20-F filer is in today's cohort
                            }
                        }
                    }
                }
            }
        }
    }
    let serial: Vec<(String, f64, Option<f64>)> = found.iter().map(|(d, (e, s))| (d.format("%Y-%m-%d").to_string(), *e, *s)).collect();
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&cache, serde_json::to_string(&serial).unwrap_or_default());
    found
}

/// Annual `FundRow`s for a US ticker from SEC XBRL company-facts, with the per-share lines the API drops
/// filled back in from the filing itself. The fallback runs ONLY when the whole series has no EPS, so the
/// 501 healthy filers pay nothing for it; the 8 that need it are all multi-class or partnership-structured.
pub async fn fetch_fundamentals_sec(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::FundRow>> {
    let mut rows = fetch_sec_facts_rows(client, urls, ticker).await?;
    if rows.iter().all(|r| r.eps.is_none()) {
        let inst = fetch_sec_instance_eps(client, urls, ticker).await;
        for r in rows.iter_mut() {
            if let Some(&(eps, shares)) = inst.get(&r.period_end) {
                r.eps = Some(eps);
                r.shares = shares;
                // the prior year off the SAME document — the split-proof comparative `eps_yoy_split_safe`
                // prefers, free here because one 10-K carries all three years in one instance.
                if let Some((_, &(pe, ps))) = inst.range(..r.period_end).next_back() {
                    r.prior_eps = Some(pe);
                    r.prior_shares = ps;
                }
            }
        }
    }
    Some(rows)
}

/// Annual `FundRow`s for a US ticker from SEC XBRL company-facts. DISK-CACHED as compact parsed rows
/// (`.sec_cache/{ticker}_facts.json`) — NOT the multi-MB raw payload — append-only history reused
/// forever. Budget-capped (`SEC_FETCH_BUDGET`). None for a non-US/unknown ticker or no annual data.
async fn fetch_sec_facts_rows(client: &Client, urls: &Urls, ticker: &str) -> Option<Vec<core::FundRow>> {
    use std::sync::atomic::Ordering;
    // "_facts9": cache-key bump when the parse gains concepts (facts3 added diluted-shares; facts4 added
    // the round-107 survival levels; facts5 added the EV/EBITDA levels; facts6 fixes the misspelled
    // DepreciationDepletionAndAmortization concept — facts5 rows have EBITDA None for every filer on
    // that tag, e.g. AAPL 2023+; facts7 adds the 20-F/40-F forms, the ifrs-full taxonomy and the
    // reporting CURRENCY — every foreign filer cached as "no rows" under facts6 and would stay empty
    // forever; facts8 adds ROA, without which every negative-equity filer keeps a permanently blank
    // quality term; facts9 adds the EPS tag fallbacks — 9 tickers, MNST/KIM/HAL/EXC/FCX/VRSN/LEN/ABNB/REG,
    // cached under facts8 with NO eps on their newest annual row; facts10 adds the same-filing prior
    // eps/shares, without which every year-over-year ratio divides two different share bases and any
    // filer that split reads as a collapse — TPL -64.7% against +6.0% real) — old rows were parsed
    // WITHOUT them and would pin the gaps forever.
    // Old *_facts{3,4,5,6,7,8,9}.json files are orphaned (few KB each); refetch amortizes over runs under
    // SEC_FETCH_BUDGET.
    let cache = sec_cache_path(&format!("{ticker}_facts10"));
    if let Some(cached) = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str::<Vec<SecCacheRow>>(&s).ok()) {
        let rows: Vec<core::FundRow> = cached
            .into_iter()
            .filter_map(|c| {
                Some(core::FundRow {
                    filed: NaiveDate::parse_from_str(&c.filed, "%Y-%m-%d").ok()?,
                    period_end: NaiveDate::parse_from_str(&c.period_end, "%Y-%m-%d").ok()?,
                    revenue: c.revenue,
                    gross_margin: c.gross_margin,
                    op_margin: c.op_margin,
                    net_margin: c.net_margin,
                    eps: c.eps,
                    shares: c.shares,
                    prior_eps: c.prior_eps,
                    prior_shares: c.prior_shares,
                    roe: c.roe,
                    roa: c.roa,
                    fcf_margin: c.fcf_margin,
                    interest_cover: c.interest_cover,
                    net_cash_rev: c.net_cash_rev,
                    ebitda: c.ebitda,
                    net_debt: c.net_debt,
                    currency: c.currency,
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
            .map(|r| SecCacheRow {
                filed: r.filed.format("%Y-%m-%d").to_string(),
                period_end: r.period_end.format("%Y-%m-%d").to_string(),
                revenue: r.revenue,
                gross_margin: r.gross_margin,
                op_margin: r.op_margin,
                net_margin: r.net_margin,
                eps: r.eps,
                roe: r.roe,
                shares: r.shares,
                fcf_margin: r.fcf_margin,
                interest_cover: r.interest_cover,
                net_cash_rev: r.net_cash_rev,
                ebitda: r.ebitda,
                net_debt: r.net_debt,
                currency: r.currency.clone(),
                roa: r.roa,
                prior_eps: r.prior_eps,
                prior_shares: r.prior_shares,
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
/// The second tuple element names which source won — the two render materially different tables
/// (FMP = quarterly rollup, SEC = annual facts), so `report` prints it in the title line.
pub async fn fetch_fundamentals_report(client: &Client, urls: &Urls, ticker: &str) -> Option<(Vec<core::FundRow>, &'static str)> {
    if let Some(rows) = fetch_fundamentals_history(client, urls, ticker).await {
        return Some((rows, "FMP"));
    }
    fetch_fundamentals_sec(client, urls, ticker).await.map(|rows| (rows, "SEC EDGAR"))
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

/// (#45) Per-coin MVRV (market cap / realized cap) from CoinMetrics' community tier, keyed by OUR
/// ticker ("BTC-EUR" -> 1.19). The same quantity `fetch_nupl` returns for Bitcoin alone, one row per
/// coin: NUPL = 1 - 1/MVRV, verified live at BTC MVRV 1.19 -> 0.160 against a footer reading 0.166.
///
/// TWO requests, and the first one is not optional. The timeseries endpoint rejects the ENTIRE batch
/// with a 400 if a single requested asset is unsupported on this tier — measured against `cc`,
/// `pyusd`, `rlusd` and `usde`, each of which killed a whole 28-asset query. So the catalog is fetched
/// first and used as an allowlist. It must be the `catalog` path, not `catalog-all`: the latter
/// enumerates pro-tier coverage and re-admits exactly the assets that break the call.
///
/// Mapping is the bare lowercase symbol — CoinMetrics ids ARE the symbols ("btc", "eth", "ada"), so
/// unlike the rejected DefiLlama proxy there is no id table to join and keep current.
///
/// Empty map on ANY failure, which reads downstream as "no MVRV for anything" and lets every coin pass
/// the ceiling free. Coverage is thin by nature (~17 of the top 100 carry a value, and BNB, SOL, TRX,
/// AVAX, SUI and XMR are among the misses), so the gate is designed around absence, not around a
/// fetch that is expected to succeed.
pub async fn fetch_mvrv(client: &Client, urls: &Urls, tickers: &[String]) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let Some(catalog) = get_json(client, &urls.coinmetrics_catalog).await else { return out };
    // catalog -> metrics[0].frequencies[freq == "1d"].assets
    let supported: std::collections::HashSet<String> = catalog
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|m| m.get("frequencies"))
        .and_then(|f| f.as_array())
        .into_iter()
        .flatten()
        .filter(|f| f.get("frequency").and_then(Value::as_str) == Some("1d"))
        .filter_map(|f| f.get("assets").and_then(Value::as_array))
        .flatten()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    // our crypto tickers -> the CoinMetrics ids we are allowed to ask for, and back again
    let mut want: Vec<(String, String)> = tickers
        .iter()
        .filter(|t| crate::picks::is_currency_quoted(t))
        .map(|t| (crate::picks::underlying(t).to_lowercase(), t.clone()))
        .filter(|(id, _)| supported.contains(id))
        .collect();
    want.sort();
    want.dedup();
    if want.is_empty() {
        return out;
    }
    let ids: Vec<&str> = {
        let mut v: Vec<&str> = want.iter().map(|(id, _)| id.as_str()).collect();
        v.dedup();
        v
    };
    let url = urls.coinmetrics_mvrv.replace("{assets}", &ids.join(","));
    let Some(body) = get_json(client, &url).await else { return out };
    let cutoff = (chrono::Utc::now().date_naive() - chrono::Duration::days(MVRV_MAX_AGE_DAYS)).to_string();
    let by_id = mvrv_rows(&body, &cutoff);
    for (id, ticker) in &want {
        if let Some(m) = by_id.get(id.as_str()) {
            out.insert(ticker.clone(), *m);
        }
    }
    out
}

/// (#45) How stale an MVRV row may be and still count. Generous for a daily series — the point is not
/// freshness-to-the-day, it is telling "yesterday's value" apart from "coverage ended in 2019".
const MVRV_MAX_AGE_DAYS: i64 = 10;

/// (#45) One MVRV timeseries body -> newest value per CoinMetrics asset id, stale rows dropped.
///
/// The date check is load-bearing, not hygiene. `limit_per_asset=1` returns each asset's LAST row,
/// which is the current one only while CoinMetrics still covers that asset: bnb stops at 2019-04-22
/// and dot at 2022-06-03. Without this filter BNB — today's rank 1 — would print a seven-year-old
/// 1.88 and be gated on it, and a wrong number is worse than a missing one. A dropped row leaves the
/// coin absent from the map, so it passes the ceiling free exactly like a coin with no data at all.
///
/// Filtered here rather than with a `start_time=` URL param so the rule survives someone hand-editing
/// `urls.coinmetrics_mvrv` in settings.yaml. ISO-8601 sorts lexically, so the date prefixes compare
/// as plain strings — no parsing.
fn mvrv_rows<'a>(body: &'a Value, cutoff: &str) -> HashMap<&'a str, f64> {
    let mut by_id: HashMap<&str, f64> = HashMap::new();
    for row in body.get("data").and_then(Value::as_array).into_iter().flatten() {
        let (Some(asset), Some(v), Some(t)) = (
            row.get("asset").and_then(Value::as_str),
            row.get("CapMVRVCur"),
            row.get("time").and_then(Value::as_str),
        ) else {
            continue;
        };
        if t.len() < 10 || &t[..10] < cutoff {
            continue;
        }
        // the API serves numbers as STRINGS ("0.96110917"); accept a bare number too, in case that changes
        if let Some(m) = v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()) {
            if m > 0.0 {
                by_id.insert(asset, m);
            }
        }
    }
    by_id
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
///
/// UNGRADEABLE, hence the skip: a signed POST against a live endpoint, with the same `offline()`
/// pinning problem as `yahoo_crumb`.
#[mutants::skip]
async fn borse_frankfurt_post(client: &Client, url: &str, salt: &str, body: &Value) -> Option<Value> {
    // second of the three seams that bypassed `get_json`/`get_text` — this one because it signs its
    // own headers. `None` is already the fetch-failed path the caller handles.
    if offline() {
        return None;
    }
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

/// (lowercased BF fund name, TER %) for ALL BF rows — the name-keyed fallback for pinned listings whose
/// symbol/venue never appears in `BF_TER` (VVSM.DE: its ISIN resolves only to the chart-less .SG venue,
/// so the symbol map can't carry it). Consulted by unique-PREFIX match only: the live list had ~25
/// "Amundi STOXX Europe 600 …" share classes sharing a prefix, so a non-unique match means "not sure",
/// never a guess.
static BF_TER_NAMES: std::sync::OnceLock<Vec<(String, f64)>> = std::sync::OnceLock::new();

/// Resolved-Yahoo-symbol -> fund AUM (assets under management), captured from the SAME etp_search row
/// as the TER — no extra request. Feeds the ETF closure-risk gate + AUM column. BF mixes fund
/// currencies (EUR/USD); treated as EUR-approximate — ±10% FX error is immaterial against the
/// order-of-magnitude gate threshold.
static BF_AUM: std::sync::OnceLock<HashMap<String, f64>> = std::sync::OnceLock::new();

/// (lowercased BF fund name, AUM) for ALL BF rows — same name-keyed fallback role as `BF_TER_NAMES`.
static BF_AUM_NAMES: std::sync::OnceLock<Vec<(String, f64)>> = std::sync::OnceLock::new();

/// Yahoo quoteSummary TER fallback for funds with NO BF facts (see `yahoo_fund_facts_fill`). Kept
/// OUT of `BF_TER` on purpose: `Quote.expense_ratio` feeds the score's ter_damp, and filling it from
/// a second source moved live ranks — these are display/H-flag facts only (`Quote.ter_fallback`).
static YH_TER: std::sync::OnceLock<HashMap<String, f64>> = std::sync::OnceLock::new();

/// Yahoo quoteSummary AUM fallback — same display-only stance as `YH_TER` (`Quote.aum_fallback`;
/// the closure-risk AUM gate keeps reading BF-only `aum_eur`).
static YH_AUM: std::sync::OnceLock<HashMap<String, f64>> = std::sync::OnceLock::new();

/// Exact-symbol Yahoo-fallback lookups — the fill is keyed by the resolved Yahoo symbol the quote
/// fetch itself uses, so no stem/name tiers are needed.
fn yh_ter_exact(ticker: &str) -> Option<f64> {
    YH_TER.get().and_then(|m| m.get(ticker)).copied()
}
fn yh_aum_exact(ticker: &str) -> Option<f64> {
    YH_AUM.get().and_then(|m| m.get(ticker)).copied()
}

/// (round 51) Tickers whose MAX-monthly series contributed ZERO bars beyond the 10y daily window on
/// a previous run (young listings — a large slice of the ETF universe). For those the second Yahoo
/// call per name buys nothing, so it's skipped for `LONG_SKIP_TTL_DAYS` (a relisting/history
/// extension heals within a month). ticker -> date recorded; gitignored local cache, same pattern
/// as `.fund_facts_cache.json`. Zero display change: a skipped fetch takes the exact daily-only
/// fallback path a failed fetch already takes (20Y column n/a, as today).
const LONG_SKIP_FILE: &str = ".long_history_skip.json";
const LONG_SKIP_TTL_DAYS: i64 = 30;
static LONG_SKIP: std::sync::OnceLock<HashMap<String, NaiveDate>> = std::sync::OnceLock::new();
/// Tickers to record after this run's fan-out (collected concurrently, written ONCE in `quotes`).
static LONG_SKIP_NEW: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn long_skip_load() -> &'static HashMap<String, NaiveDate> {
    LONG_SKIP.get_or_init(|| {
        std::fs::read_to_string(crate::config::data_path(LONG_SKIP_FILE))
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
            .map(|m| m.into_iter().filter_map(|(t, d)| Some((t, d.parse().ok()?))).collect())
            .unwrap_or_default()
    })
}

/// The skip decision, pure for testability: recorded within the TTL -> skip the monthly fetch.
fn long_skip_fresh(recorded: Option<&NaiveDate>, today: NaiveDate) -> bool {
    recorded.is_some_and(|d| (today - *d).num_days() <= LONG_SKIP_TTL_DAYS)
}

/// Persist the skip list: still-fresh old entries keep their ORIGINAL date (so the TTL actually
/// expires), this run's new zero-contribution tickers stamp today. Stale entries drop out.
fn long_skip_save() {
    let today = chrono::Local::now().date_naive();
    let mut m: HashMap<String, String> = long_skip_load()
        .iter()
        .filter(|(_, d)| long_skip_fresh(Some(d), today))
        .map(|(t, d)| (t.clone(), d.to_string()))
        .collect();
    for t in LONG_SKIP_NEW.lock().unwrap().drain(..) {
        m.entry(t).or_insert_with(|| today.to_string());
    }
    let _ = std::fs::write(crate::config::data_path(LONG_SKIP_FILE), serde_json::to_string(&m).unwrap_or_default());
}

/// (round 53) Full MAX-monthly series cache: the raw Yahoo JSON per ticker, 7-day TTL. Monthly bars
/// only change on a month boundary, so refetching thousands of them every screen bought nothing but
/// runtime and 429 pressure. The RAW payload is cached (not the parsed series) so a cache hit takes
/// the exact same parse path as a live fetch — output identical by construction, including the
/// adjusted-close config flag. Gitignored (~tens of MB, self-prunes on TTL); loaded once per
/// process, written once per fan-out, same pattern as `.long_history_skip.json` above.
/// `pub` alongside the TTL for `backtest_edge_holds`, which decides whether to run a full wide
/// backtest or skip green by asking whether THIS file is fresh enough to serve it.
pub const LONG_CACHE_FILE: &str = ".long_history_cache.json";
pub const LONG_CACHE_TTL_DAYS: i64 = 7;
static LONG_CACHE: std::sync::OnceLock<HashMap<String, (NaiveDate, &'static RawValue)>> = std::sync::OnceLock::new();
/// This run's live fetches (collected concurrently, written ONCE in `quotes`).
static LONG_CACHE_NEW: std::sync::Mutex<Vec<(String, Box<RawValue>)>> = std::sync::Mutex::new(Vec::new());

/// `RawValue`, not `Value`: this file is 129.7 MB and EVERY command that touches long history pays this
/// function up front, serially, before any work starts — it was the entire 0.98 s fixed cost of a
/// `backtest`, 46% of a wide run and 100% of a one-ticker one. `RawValue` makes serde find each
/// payload's extent and skip it, instead of allocating the per-number/string/array nodes that built
/// 5057 trees a run to read a handful. Whoever actually wants a ticker parses it then, via
/// `parse_chart_raw` — same bytes, same `serde_json`, same `parse_chart`, so the parity rule above holds.
///
/// BORROWED, not `Box`: entries point INTO the file buffer rather than owning a copy of their span, so
/// the 129.7 MB is not memcpy'd a second time (and not freed at exit either). `fs::read` over
/// `read_to_string` drops a third pass — `String` will not exist until Rust has proven all 129.7 MB is
/// valid UTF-8, and serde validates the parts that matter as it goes anyway.
///
/// The leak is deliberate and is what makes the borrow sound: `LONG_CACHE` is a `OnceLock` static that
/// is already alive for the whole process, so the buffer it points into has to be too. Nothing here
/// grows per call — it happens once, and "freeing" it would only ever happen at exit.
fn long_cache_load() -> &'static HashMap<String, (NaiveDate, &'static RawValue)> {
    LONG_CACHE.get_or_init(|| {
        std::fs::read(crate::config::data_path(LONG_CACHE_FILE))
            .ok()
            .map(|b| &*Box::leak(b.into_boxed_slice()))
            .and_then(|b| serde_json::from_slice::<HashMap<String, (String, &'static RawValue)>>(b).ok())
            .map(|m| m.into_iter().filter_map(|(t, (d, v))| Some((t, (d.parse().ok()?, v)))).collect())
            .unwrap_or_default()
    })
}

/// The reuse decision, pure for testability: cached within the TTL -> serve from disk, no refetch.
///
/// (#56) OFFLINE makes a present entry immortal. A committed fixture cache is stamped with the date it
/// was generated, so the TTL would expire it a week later and the frozen-data pin would start trying to
/// fetch — which `offline()` already blocks, but as an empty result rather than the recorded history.
/// Only PRESENCE is overridden; a ticker absent from the file is still a miss.
fn long_cache_fresh(recorded: Option<&NaiveDate>, today: NaiveDate) -> bool {
    recorded.is_some_and(|d| offline() || (today - *d).num_days() <= LONG_CACHE_TTL_DAYS)
}

/// Persist: still-fresh old entries keep their ORIGINAL date (so the TTL actually expires), this
/// run's fetches stamp today. Stale entries drop out — the file self-prunes.
///
/// `pub` for `backtest`, the other command that pulls monthly history: without its own flush a wide
/// run reads the cache but never contributes to it, so tickers `screen` never touches (the stress
/// losers, the benchmark) would re-download on every run forever.
pub fn long_cache_save() {
    if offline() {
        return; // (#56) the committed fixture cache is READ-ONLY — a test run must never rewrite it
    }
    let today = chrono::Local::now().date_naive();
    let new = LONG_CACHE_NEW.lock().unwrap();
    if new.is_empty() && long_cache_load().iter().all(|(_, (d, _))| long_cache_fresh(Some(d), today)) {
        return; // nothing new, nothing stale — skip rewriting tens of MB
    }
    let mut m: HashMap<&str, (String, &RawValue)> = long_cache_load()
        .iter()
        .filter(|(_, (d, _))| long_cache_fresh(Some(d), today))
        // `&**v` reborrows as a plain `&RawValue` — the file format must not change, and this writes
        // exactly the same bytes as before whichever side the span came from.
        // A carried-over entry re-emits the span serde normalised when it was first written; a new one
        // was normalised by `to_raw_value` in `chart_json_long`. Either way, same bytes as the old
        // parse-and-reserialize round trip.
        .map(|(t, (d, v))| (t.as_str(), (d.to_string(), &**v)))
        .collect();
    for (t, v) in new.iter() {
        m.insert(t.as_str(), (today.to_string(), &**v));
    }
    let _ = std::fs::write(crate::config::data_path(LONG_CACHE_FILE), serde_json::to_string(&m).unwrap_or_default());
}
/// Per-row BF keyData facts: share-class + replication tokens ("Acc"/"Dist"; "Swap"/"Full"/"Opt"/
/// "Hybr"/"Samp") and the benchmark-index name (lowercased at capture so twin matching is one `==`;
/// BF normalizes it — same-index funds carry the literal same string, hedged classes differ).
#[derive(Clone, PartialEq, Default, Debug)]
pub struct BfMeta {
    pub use_of: Option<&'static str>,
    pub repl: Option<&'static str>,
    pub bench: Option<String>,
    /// (DOM) fund legal domicile = first 2 ISIN chars ("IE"/"LU"/"DE"…), set where the ISIN is in hand
    /// (BF rows AND venue/regulatory-only funds — every source is ISIN-keyed). Display + CORE-shortlist
    /// ordering only, never scored. Withholding stakes: IE gets the 15% US-dividend treaty, LU eats 30%.
    pub dom: Option<String>,
}

/// Resolved-Yahoo-symbol -> share-class/replication tokens, captured from the SAME etp_search row as
/// TER/AUM — no extra request. Display-only (USE/REPL columns), never scored: the ranking's price-only
/// CAGR already prices the Dist payout drag (payouts leave the NAV), so Acc twins win by construction.
static BF_META: std::sync::OnceLock<HashMap<String, BfMeta>> = std::sync::OnceLock::new();

/// (lowercased BF fund name, meta) for ALL BF rows — same name-keyed fallback role as `BF_TER_NAMES`.
static BF_META_NAMES: std::sync::OnceLock<Vec<(String, BfMeta)>> = std::sync::OnceLock::new();

/// (#45) EVERY BF row name, lowercased, UNFILTERED — where `BF_META_NAMES` keeps only rows whose
/// keyData parsed to something usable. The set difference between the two lists is exactly "BF has a
/// row for this fund but it told us nothing", a cause that is otherwise indistinguishable from "the
/// fund is not on BF at all": both surface as a `BfMeta::default()` and an `n/a` cell. Diagnostic-only
/// — `bf_meta` must NOT read this, or a factless row would start shadowing the name fallback.
static BF_ROW_NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Yahoo-name -> BF value via unique prefix (Yahoo drops BF's share-class suffix: "VanEck Semiconductor
/// UCITS ETF" vs BF's "… - USD Acc"). None unless EXACTLY one BF fund name starts with the quote's name.
fn bf_by_name<T: Clone>(list: &std::sync::OnceLock<Vec<(String, T)>>, name: &str) -> Option<T> {
    let n = name.trim().to_lowercase();
    if n.len() < 10 {
        return None; // too short to identify one fund
    }
    let mut hits = list.get()?.iter().filter(|(bf, _)| bf.starts_with(&n));
    match (hits.next(), hits.next()) {
        (Some((_, v)), None) => Some(v.clone()),
        _ => None,
    }
}

fn bf_ter_by_name(name: &str) -> Option<f64> {
    bf_by_name(&BF_TER_NAMES, name)
}

/// First 2 ISIN chars = the fund's legal domicile country code. Defensive slice: upstream ISINs are
/// shape-checked (`core::is_isin`), but a short string must yield None, never panic.
fn isin_domicile(isin: &str) -> Option<String> {
    isin.get(..2).map(|p| p.to_ascii_uppercase())
}

/// Yahoo cookie+crumb pair for the query2 quoteSummary API (required since 2023). Fetched once per
/// process, best-effort: a race just repeats the two-request handshake, first `set` wins.
/// ponytail: endpoints hardcoded — lift into `Urls` only if a test ever needs to stub them.
static YQ_AUTH: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();

/// A full browser UA, for Yahoo only. The bare `"Mozilla/5.0"` the rest of this file sends is now
/// enough on its own to get the crumb handshake refused, so the string's realism IS the feature —
/// do not shorten it. Scoped to the three Yahoo requests rather than the shared `client()` builder,
/// because every other provider here is happy with the short form and SEC prefers a contact UA.
const YAHOO_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
/// UNGRADEABLE, hence the skip: two live GETs against hardcoded Yahoo hosts. The one branch a test
/// could pin is the `offline()` guard, and pinning it needs `FOLIOMAN_OFFLINE` set for the whole
/// process — which a `--lib` test cannot do without `std::env::set_var`, and one of those invalidates
/// every env-based CI repro in this binary. `tests/cli.rs` pins it from a child process, which is
/// outside the mutation gate's killing suite.
#[mutants::skip]
async fn yahoo_crumb(client: &reqwest::Client) -> Option<(String, String)> {
    // Restated here because the quoteSummary pair (this handshake + `quote_summary_json`) is the one
    // outbound path in this file that does NOT ride `get_json`/`get_text` — so the guard those two
    // carry never applied to it. Checked before the `YQ_AUTH` read so an offline run never caches a
    // None that a later online call in the same process would inherit.
    //
    // The gap was not theoretical: `FOLIOMAN_OFFLINE=1 folioman report AAA` fetched live fund facts
    // off Yahoo and printed a fund profile, so a test asserting the no-data path passed on a box that
    // cannot reach Yahoo and failed on CI, which can.
    if offline() {
        return None;
    }
    if let Some(v) = YQ_AUTH.get() {
        return v.clone();
    }
    let v: Option<(String, String)> = async {
        // Two hosts, in order, first non-empty cookie wins. fc.yahoo.com answers 404 but historically SET
        // the session cookie; it has since stopped sending `set-cookie` at all, which made `cookie` empty
        // and returned None on EVERY run — and because this is the sole crumb seam, that one dead handshake
        // silently took out fund P/E, the TER/AUM fallback, sector weightings and the stock/bond split
        // together. finance.yahoo.com still sets one. Keep fc first: it is the cheaper 404 when it works.
        //
        // Either way, keep the name=value pairs and drop the attributes (Path/Expires/HttpOnly).
        let mut cookie = String::new();
        for host in ["https://fc.yahoo.com", "https://finance.yahoo.com"] {
            let Ok(resp) = client.get(host).header("User-Agent", YAHOO_UA).send().await else { continue };
            cookie = resp
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|h| h.to_str().ok()?.split(';').next().map(str::to_string))
                .collect::<Vec<_>>()
                .join("; ");
            if !cookie.is_empty() {
                break;
            }
        }
        if cookie.is_empty() {
            return None;
        }
        let resp = client
            .get("https://query2.finance.yahoo.com/v1/test/getcrumb")
            .header("Cookie", &cookie)
            .header("User-Agent", YAHOO_UA)
            .send()
            .await
            .ok()?;
        // status FIRST: a 429 body is plain prose, and prose passed the old token check (see `is_crumb`)
        if !resp.status().is_success() {
            return None;
        }
        let crumb = resp.text().await.ok()?;
        is_crumb(&crumb).then_some((cookie, crumb))
    }
    .await;
    let _ = YQ_AUTH.set(v.clone());
    v
}

/// Does this response body look like a Yahoo crumb rather than an error?
///
/// A real crumb is a short opaque token with no spaces (`0EBQ3sZ/xyz`). The previous rule was only
/// "non-empty and contains no `{`", which accepts Yahoo's rate-limit body — the bare text
/// `Too Many Requests`, no braces anywhere. That token was then cached in `YQ_AUTH` for the whole
/// process and appended to every quoteSummary URL, so ONE 429 turned into a run's worth of requests
/// that could only ever earn `401 Invalid Crumb`. Measured against the live endpoint while probing.
///
/// Split out of [`yahoo_crumb`] because that fn is `#[mutants::skip]`-ed live-only code; the rule that
/// actually decides is pure, so it can be graded and tested.
fn is_crumb(token: &str) -> bool {
    let t = token.trim();
    !t.is_empty() && !t.contains('{') && !t.chars().any(char::is_whitespace)
}

/// (TER/AUM) `(TER %, AUM)` out of one justETF ETF-profile page. `(None, None)` on any shape change.
///
/// THE THIRD SOURCE for these two facts, and currently the only working one. Börse Frankfurt answers
/// most funds; Yahoo used to fill the holes and no longer can (its crumb handshake is dead upstream).
/// justETF is keyless and covers EU UCITS funds, which is exactly the population BF misses.
///
/// ponytail: an HTML scrape, with a known ceiling — two `data-testid` anchors, no HTML parser, no new
/// dependency. `data-testid` attributes are what justETF's own test suite selects on, so they are the
/// most stable handles the page offers, but they are still not an API. If the page moves, both values
/// go `None` and the cells stay `n/a` exactly as they are today; nothing guesses.
///
/// Units are normalised to match Börse Frankfurt's (`bf_row_aum`): TER in percent (`0.15% p.a.` ->
/// `0.15`) and AUM in absolute currency (`EUR 16,988 m` -> `1.6988e10`), so the two sources are
/// interchangeable downstream. A magnitude suffix that is neither millions nor billions yields None
/// rather than a figure wrong by a factor of a million.
fn parse_justetf_facts(html: &str) -> (Option<f64>, Option<f64>) {
    // "0.15% p.a." / "16,988 m" -> the leading number, thousands separators dropped. Non-positive is
    // rejected: justETF prints a literal 0 for "unknown", the same convention BF and Yahoo use.
    fn lead_num(s: &str) -> Option<f64> {
        let t: String = s
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
            .filter(|c| *c != ',')
            .collect();
        t.parse::<f64>().ok().filter(|v: &f64| v.is_finite() && *v > 0.0)
    }
    let ter = between(html, "tl_etf-basics_value_ter\">", "<").and_then(lead_num);
    let aum = between(html, "etf-basics_row_fund-size", "</tr>").and_then(|row| {
        let cell = between(row, "<div>", "<")?.trim(); // "EUR 16,988 m "
        let rest = cell.split_once(' ')?.1; // drop the currency word
        let n = lead_num(rest)?;
        let mag = rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '.').trim();
        match mag {
            // `bn` is what justETF writes; matching on `b` alone covers that and any `b`/`billion`
            // variant in one test, where `starts_with("bn") || starts_with('b')` was the same set
            // written twice — the first arm implies the second.
            m if m.starts_with('b') => Some(n * 1e9),
            m if m.starts_with('m') => Some(n * 1e6),
            _ => None,
        }
    });
    (ter, aum)
}

/// (TER/AUM) Live justETF facts for one ISIN. `None` on transport failure or an unusable page.
async fn justetf_fund_facts(client: &Client, urls: &Urls, isin: &str) -> Option<(Option<f64>, Option<f64>)> {
    let html = get_text(client, &urls.justetf_profile.replace("{isin}", isin)).await?;
    Some(parse_justetf_facts(&html))
}

/// Pull (TER %, AUM) out of one Yahoo quoteSummary payload. TER arrives as a FRACTION (0.0014 =
/// 0.14%) -> ×100; a literal 0.0 is Yahoo's "unknown", not a free fund (probe receipt: VUAA.DE came
/// back 0.0 against its known 0.07%) -> None. AUM = totalAssets in the fund's quote currency,
/// umbrella-level — treated EUR-approximate like BF's (order-of-magnitude gate, ±FX immaterial).
fn parse_yahoo_fund_facts(v: &Value) -> (Option<f64>, Option<f64>) {
    let Some(r) = v.pointer("/quoteSummary/result/0") else {
        return (None, None);
    };
    let ter = r
        .pointer("/fundProfile/feesExpensesInvestment/annualReportExpenseRatio/raw")
        .and_then(|x| x.as_f64())
        .filter(|t| *t > 0.0)
        .map(|t| t * 100.0);
    let aum = r
        .pointer("/summaryDetail/totalAssets/raw")
        .or_else(|| r.pointer("/defaultKeyStatistics/totalAssets/raw"))
        .and_then(|x| x.as_f64())
        .filter(|a| *a > 0.0);
    (ter, aum)
}

/// (round 47) Second FACTS source: BF's etp_search is the only venue payload carrying TER/AUM, so
/// Euronext/SIX/regulatory-only funds land factless and print all-n/a cells forever. Fetch the holes
/// from Yahoo quoteSummary into SEPARATE fallback maps (`YH_TER`/`YH_AUM`) — never merged into the BF
/// maps, because `expense_ratio`/`aum_eur` feed the SCORE (ter_damp ^20 drag) and the AUM gate: a
/// first merged run moved live ranks (PEA 3->9, score 7.3->6.8), and the scoring lane is closed.
/// Fallback facts are DISPLAY + H/CORE only, read via `Quote::ter_shown`/`aum_shown`. Weekly disk
/// cache + per-run request budget so a cold universe converges over a few runs instead of stampeding
/// Yahoo. USE/REPL stay BF-only (Yahoo lacks them), so factless venue funds still can't earn the H
/// flag — the win is honest TER/AUM cells.
async fn yahoo_fund_facts_fill(
    client: &reqwest::Client,
    urls: &Urls,
    syms: &[String],
    bf_ter: &HashMap<String, f64>,
    bf_aum: &HashMap<String, f64>,
    isin_of: &HashMap<String, String>,
) -> (HashMap<String, f64>, HashMap<String, f64>) {
    // (round 54) 200 -> 400: each TER hole blocks a potential H/CORE qualifier and the wide screen
    // was converging ~90 holes/run; the round-53 monthly cache freed the runtime headroom. Fallback
    // facts are never scored, so a faster fill is rank-neutral by construction.
    const BUDGET: usize = 400;
    type Row = (String, Option<f64>, Option<f64>); // (fetched date, TER %, AUM)
    let (mut ter_map, mut aum_map) = (HashMap::new(), HashMap::new());
    let mut cache: HashMap<String, Row> = std::fs::read_to_string(crate::config::data_path(FUND_FACTS_CACHE_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let today = chrono::Utc::now().date_naive();
    let mut todo: Vec<String> = Vec::new();
    for s in syms {
        let (miss_ter, miss_aum) = (!bf_ter.contains_key(s), !bf_aum.contains_key(s));
        // DELIBERATELY UNPINNED (mutation audit, round 5). This short-circuit is an OPTIMISATION, not
        // the firewall: the per-field `miss_ter`/`miss_aum` guards below already refuse to overwrite a
        // BF answer, so deleting this branch changes no returned value — only the number of symbols
        // that reach the fetch list. Killing it means observing that a fully-answered fund is never
        // fetched, and `yahoo_crumb` hardcodes `https://fc.yahoo.com` (no `Urls`, so `stub_urls`
        // cannot redirect it), which leaves no way to count requests without a live socket.
        if !miss_ter && !miss_aum {
            continue; // BF already answered — Yahoo is only consulted for the holes
        }
        // (fund staleness) Serve and refetch decided separately — the same split as `yahoo_top_holdings`,
        // and for the same reason: the dead crumb below returns whatever was served here, so a row that
        // is merely not-from-today must still fill the cell while it waits to be refreshed.
        let age = cache.get(s).and_then(|r| cache_age_days(&r.0, today));
        let (serve, refetch) = cache_use(age);
        if let (Some(r), true) = (cache.get(s), serve) {
            if miss_ter {
                if let Some(t) = r.1 {
                    ter_map.insert(s.clone(), t);
                }
            }
            if miss_aum {
                if let Some(a) = r.2 {
                    aum_map.insert(s.clone(), a);
                }
            }
        }
        if refetch {
            todo.push(s.clone());
        }
    }
    todo.truncate(BUDGET);
    if todo.is_empty() {
        return (ter_map, aum_map);
    }
    // one handshake up front so a dead crumb skips the whole batch instead of 400 doomed calls;
    // the per-symbol seam below reuses it via the in-process YQ_AUTH cache.
    if yahoo_crumb(client).await.is_none() {
        // ...and when it IS dead, fall through to justETF rather than giving up on the holes entirely.
        // This is no longer a rare degradation: `fc.yahoo.com` stopped setting the session cookie, so
        // this branch is the normal path, and returning here is why a live run reported 1433 funds with
        // no TER at all.
        eprintln!("fetch: Yahoo crumb handshake failed — filling fund facts from justETF instead");
        let filled = justetf_fund_facts_fill(client, urls, &todo, isin_of, &mut cache, today).await;
        for (sym, ter, aum) in filled {
            if !bf_ter.contains_key(&sym) {
                if let Some(t) = ter {
                    ter_map.entry(sym.clone()).or_insert(t);
                }
            }
            if !bf_aum.contains_key(&sym) {
                if let Some(a) = aum {
                    aum_map.entry(sym.clone()).or_insert(a);
                }
            }
        }
        write_fund_facts_cache(&cache);
        return (ter_map, aum_map);
    }
    let fetched: Vec<Option<(String, Option<f64>, Option<f64>)>> = stream::iter(todo)
        .map(|sym| async move {
            let (ter, aum) = fund_facts_live(client, &sym).await.ok()?;
            Some((sym, ter, aum))
        })
        .buffer_unordered(fetch_concurrency())
        .collect()
        .await;
    // DELIBERATELY UNPINNED (mutation audit, round 5), all three mutants on the two lines below: the
    // `||`, and both replacements for the `+=`. `got` is read by one `eprintln!` and nothing else, and
    // this whole block is unreachable without a LIVE Yahoo handshake — the `yahoo_crumb` call above
    // hardcodes `https://fc.yahoo.com` and takes no `Urls`, so `stub_urls` cannot redirect it and a
    // unit test can only make it fail closed (`YQ_AUTH.set(None)`), which returns before here.
    // Killing them needs a real socket, which no test in this binary may open. The cache-served path
    // above it — the half that decides actual returned values — is pinned.
    let mut got = 0usize;
    for (sym, ter, aum) in fetched.into_iter().flatten() {
        if ter.is_some() || aum.is_some() {
            got += 1;
        }
        if let Some(t) = ter {
            ter_map.entry(sym.clone()).or_insert(t);
        }
        if let Some(a) = aum {
            aum_map.entry(sym.clone()).or_insert(a);
        }
        cache.insert(sym, (today.to_string(), ter, aum)); // both-None cached too: one retry a day, not one per run
    }
    eprintln!("fetch: Yahoo fund-facts fallback filled {got} non-BF funds (cache: {})", cache.len());
    write_fund_facts_cache(&cache);
    (ter_map, aum_map)
}

/// Best-effort write-back of the fund-facts cache. Two callers (the Yahoo pass and the justETF one),
/// so it is a fn rather than the same three lines twice. A read-only dir just costs next run a refetch.
fn write_fund_facts_cache(cache: &HashMap<String, (String, Option<f64>, Option<f64>)>) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(crate::config::data_path(FUND_FACTS_CACHE_PATH), json);
    }
}

/// (TER/AUM) The justETF half of `yahoo_fund_facts_fill`, run when Yahoo's handshake is dead. Resolves
/// each symbol to its ISIN (justETF is ISIN-keyed; a symbol with no known ISIN is simply skipped) and
/// records every answer in the shared cache, both-None included, so a fund justETF cannot serve costs
/// one request rather than one per run.
async fn justetf_fund_facts_fill(
    client: &Client,
    urls: &Urls,
    todo: &[String],
    isin_of: &HashMap<String, String>,
    cache: &mut HashMap<String, (String, Option<f64>, Option<f64>)>,
    today: NaiveDate,
) -> Vec<(String, Option<f64>, Option<f64>)> {
    let pairs: Vec<(String, String)> =
        todo.iter().filter_map(|s| isin_of.get(s).map(|i| (s.clone(), i.clone()))).collect();
    let fetched: Vec<Option<(String, Option<f64>, Option<f64>)>> = stream::iter(pairs)
        .map(|(sym, isin)| async move {
            let (ter, aum) = justetf_fund_facts(client, urls, &isin).await?;
            Some((sym, ter, aum))
        })
        .buffer_unordered(fetch_concurrency())
        .collect()
        .await;
    let out: Vec<(String, Option<f64>, Option<f64>)> = fetched.into_iter().flatten().collect();
    for (sym, ter, aum) in &out {
        cache.insert(sym.clone(), (today.to_string(), *ter, *aum));
    }
    // Just the page count. A "how many carried a usable field" refinement would need its own `||`
    // over data no caller reads — a branch that exists only to shape one log line.
    eprintln!("fetch: justETF answered for {} non-BF funds", out.len());
    out
}

/// (round 56) Top-10 holdings out of one Yahoo quoteSummary `topHoldings` payload: the underlying
/// symbol when present, else the holding name, uppercased so the same stock matches across funds.
/// (round 57) each paired with its portfolio weight as a FRACTION (`holdingPercent.raw`, 0.058 =
/// 5.8%; 0.0 when Yahoo omits it) so the screen can flag top-heavy funds.
fn parse_top_holdings(v: &Value) -> Vec<(String, f64)> {
    v.pointer("/quoteSummary/result/0/topHoldings/holdings")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .take(10)
                .filter_map(|h| {
                    let sym = h
                        .pointer("/symbol")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| h.pointer("/holdingName").and_then(Value::as_str))
                        .map(str::to_uppercase)?;
                    let pct = h.pointer("/holdingPercent/raw").and_then(Value::as_f64).unwrap_or(0.0);
                    Some((sym, pct))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// (report round 7) `topHoldings.sectorWeightings` — an array of single-key `{sector: fraction}`
/// objects (value bare or `{raw}`). The composition datum top-10 holdings HIDES: a concentrated
/// tech ETF and a broad core both list NVDA/AAPL/MSFT up top, but their sector spreads differ.
/// Drops zero-weight sectors, sorted heaviest first.
fn parse_fund_sectors(v: &Value) -> Vec<(String, f64)> {
    let Some(arr) =
        v.pointer("/quoteSummary/result/0/topHoldings/sectorWeightings").and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut out: Vec<(String, f64)> = arr
        .iter()
        .filter_map(|o| {
            let (k, val) = o.as_object()?.iter().next()?; // each row is one {sector: weight}
            let w = val.as_f64().or_else(|| val.pointer("/raw").and_then(Value::as_f64))?;
            (w > 0.0).then(|| (pretty_sector(k), w))
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// (report round 7) `topHoldings` equity/bond split (bare or `{raw}`) — ≈100/0 for an equity ETF,
/// meaningful for a mixed/bond fund. `None` unless a stock position is present (bond defaults 0).
fn parse_fund_stock_bond(v: &Value) -> Option<(f64, f64)> {
    let r = v.pointer("/quoteSummary/result/0/topHoldings")?;
    let f = |k: &str| {
        r.pointer(&format!("/{k}"))
            .and_then(|x| x.as_f64().or_else(|| x.pointer("/raw").and_then(Value::as_f64)))
    };
    Some((f("stockPosition")?, f("bondPosition").unwrap_or(0.0)))
}

/// (fund valuation) `topHoldings.equityHoldings.priceToEarnings` — Yahoo serves the fund-book
/// ratios INVERTED: IITU.L arrives as raw 0.02947, the real P/E is 1/0.02947 ≈ 33.9 (P/B
/// 0.076→13.1 and P/S 0.096→10.5 confirm the reciprocal family — same trap class as the
/// fraction holdings weights). Do NOT "fix" this back to the raw value. Non-positive raw
/// (missing earnings / weird payload) → `None`, never a fake ratio.
fn parse_fund_pe(v: &Value) -> Option<f64> {
    v.pointer("/quoteSummary/result/0/topHoldings/equityHoldings/priceToEarnings")
        .and_then(|x| x.as_f64().or_else(|| x.pointer("/raw").and_then(Value::as_f64)))
        .filter(|r| *r > 0.0)
        .map(|r| 1.0 / r)
}

/// (report round 7) Yahoo's snake_case sector key -> display ("financial_services" -> "Financial
/// Services"); "realestate" is the one key with no underscore to split.
fn pretty_sector(k: &str) -> String {
    if k == "realestate" {
        return "Real Estate".to_string();
    }
    k.split('_')
        .map(|w| {
            let mut ch = w.chars();
            match ch.next() {
                Some(f) => f.to_uppercase().chain(ch).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// (report round 7) `fundProfile.categoryName` — the fund's one-word class (Technology, Large
/// Blend). Report-only; `None` when absent/blank.
fn parse_fund_category(v: &Value) -> Option<String> {
    v.pointer("/quoteSummary/result/0/fundProfile/categoryName")
        .or_else(|| v.pointer("/quoteSummary/result/0/fundProfile/category"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// (round 56) Weekly `{sym: ["YYYY-MM-DD", [[holding, weight]...], [[sector, weight]...], stock_bond]}`
/// cache for `yahoo_top_holdings`; empty lists are cached too, so a fund Yahoo has no holdings for
/// costs one request a week.
/// (round 57) the row schema gained per-holding weights; an old symbols-only cache fails to
/// deserialize and is treated as empty — one refetch of the ~30 printed picks heals it.
/// (sector tilt) same healing path again: rows gained the sector weightings + equity/bond split
/// that ride the SAME topHoldings payload (previously parsed-and-dropped — zero new HTTP).
/// (fund valuation) third widening, same heal: rows gained the inverted equity-book P/E.
const HOLDINGS_CACHE_PATH: &str = ".holdings_cache.json";

/// (fund staleness) Refetch a fund row that is not from TODAY. Was 7 days, which is why a run in the
/// middle of Yahoo's outage found every row 8-22 days old and had nothing at all to serve.
const FUND_CACHE_REFRESH_DAYS: i64 = 1;
/// (fund staleness) And REFUSE to serve one older than this, even when the refetch is impossible.
/// A fund's look-through book moves slowly, but not so slowly that a week-old P/E should quietly trim a
/// fund off the table — past this age the column goes `n/a`, which is a visible absence rather than a
/// number nobody can date. Between the two thresholds a row is served and MARKED, never served silently.
const FUND_CACHE_MAX_AGE_DAYS: i64 = 3;

/// (fund staleness) Age in days of a `YYYY-MM-DD` cache stamp, or None if it does not parse.
///
/// One helper for both fund caches (`yahoo_fund_facts_fill` and `yahoo_top_holdings`) because they had
/// the same date predicate written twice and only one of them would ever have been fixed. `None` for an
/// unparseable stamp reads downstream as "not usable", the same as far too old — a corrupt row must not
/// become an infinitely fresh one.
///
/// Signed on purpose: a stamp dated in the FUTURE (clock skew, a hand-edited cache) yields a negative
/// age, which is `< REFRESH` and `<= MAX_AGE`, so it counts as fresh rather than being refetched forever.
fn cache_age_days(stamp: &str, today: NaiveDate) -> Option<i64> {
    NaiveDate::parse_from_str(stamp, "%Y-%m-%d").ok().map(|d| (today - d).num_days())
}

/// (fund staleness) `(serve?, refetch?)` for a fund-cache row of this age (`None` = absent or corrupt).
///
/// The two answers are INDEPENDENT, which is the whole point and was the bug: the old code asked one
/// question ("is this fresh?") and used it for both, so a row that was merely due a refresh was also
/// treated as unusable — and when the refresh then failed (Yahoo's crumb handshake is dead), the caller
/// returned nothing at all despite holding a perfectly datable value. `serve && refetch` is exactly the
/// stale-but-usable window, i.e. the rows that must print with the `°` mark.
///
/// Both loops call this rather than repeating the comparisons, because they DID repeat them before and
/// only one of the two copies would ever have been fixed.
fn cache_use(age: Option<i64>) -> (bool, bool) {
    (age.is_some_and(|a| a <= FUND_CACHE_MAX_AGE_DAYS), age.is_none_or(|a| a >= FUND_CACHE_REFRESH_DAYS))
}

/// (report round 7) Shared cacheless quoteSummary GET — the fund seams below (and their
/// report-only `_ext`/composition cousins) differ ONLY in the `modules=` list, so the crumb
/// handshake + throttle guard live here once. `Err` = environmental (skip/retry); `Ok(Value)` =
/// an HTTP-200 body that parsed as JSON (may still be an empty result for an unknown symbol).
async fn quote_summary_json(client: &Client, sym: &str, modules: &str) -> Result<Value, String> {
    let (cookie, crumb) = yahoo_crumb(client).await.ok_or("Yahoo crumb handshake failed")?;
    let url = format!(
        "https://query2.finance.yahoo.com/v10/finance/quoteSummary/{sym}?modules={modules}&crumb={crumb}"
    );
    // the one outbound call in this file that never went through the pacer. Every quoteSummary read
    // (fund facts, topHoldings, composition) rides this fn, and the screen's fund pass fires it once
    // per fund — an unpaced burst against the endpoint that already answers 429 the most readily.
    throttle().await;
    let resp = client
        .get(&url)
        .header("Cookie", &cookie)
        .header("User-Agent", YAHOO_UA)
        .send()
        .await
        .map_err(|e| format!("transport error: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        return Err(format!("throttled/unavailable: HTTP {status}"));
    }
    resp.json().await.map_err(|_| "200 but body wasn't JSON (likely a throttle/HTML page)".to_string())
}

/// (round 66) Single-symbol live topHoldings fetch — the cacheless core of `yahoo_top_holdings`,
/// public so the network smoke test can probe the payload without touching the weekly cache.
/// `Err` = environmental (transport/handshake/throttle — skip, retry later); `Ok(empty)` = HTTP 200
/// whose body yielded no holdings, which for a major equity ETF means the payload shape drifted.
pub async fn top_holdings_live(client: &Client, sym: &str) -> Result<Vec<(String, f64)>, String> {
    Ok(parse_top_holdings(&quote_summary_json(client, sym, "topHoldings").await?))
}

/// (report round 7) Report-only cousin of `top_holdings_live`: one topHoldings fetch, but also
/// pulls the composition fields the plain seam drops — `sectorWeightings` (what the top-10
/// holdings can't show: diversified core vs sector bet), the equity/bond split, and the
/// (fund valuation) inverted equity-book P/E. Returns
/// (holdings, sectors desc, `Option<(stock, bond)>`, `Option<P/E>`).
pub(crate) async fn fund_composition_live(
    client: &Client,
    sym: &str,
) -> Result<(Vec<(String, f64)>, Vec<(String, f64)>, Option<(f64, f64)>, Option<f64>), String> {
    let v = quote_summary_json(client, sym, "topHoldings").await?;
    Ok((parse_top_holdings(&v), parse_fund_sectors(&v), parse_fund_stock_bond(&v), parse_fund_pe(&v)))
}

/// (drift net) Single-symbol live fund-facts fetch — the cacheless core of the
/// `yahoo_fund_facts_fill` fallback, public so the network smoke test can probe the quoteSummary
/// fundProfile/summaryDetail payload without touching the weekly cache. Err = environmental
/// (crumb/transport/throttle — skip, retry later); Ok((None, None)) = HTTP 200 whose body yielded
/// neither TER nor AUM, which for a major equity ETF means the payload shape drifted.
pub async fn fund_facts_live(client: &Client, sym: &str) -> Result<(Option<f64>, Option<f64>), String> {
    let v =
        quote_summary_json(client, sym, "fundProfile%2CsummaryDetail%2CdefaultKeyStatistics").await?;
    Ok(parse_yahoo_fund_facts(&v))
}

/// (report round 7) Report-only cousin of `fund_facts_live`: same one fetch, but also returns the
/// fund's `categoryName` (its one-word class — Technology, Large Blend). The screen never needs the
/// category (it ranks, doesn't describe); the report drill-in does. Returns (ter, aum, category).
pub(crate) async fn fund_facts_ext_live(
    client: &Client,
    sym: &str,
) -> Result<(Option<f64>, Option<f64>, Option<String>), String> {
    let v =
        quote_summary_json(client, sym, "fundProfile%2CsummaryDetail%2CdefaultKeyStatistics").await?;
    let (ter, aum) = parse_yahoo_fund_facts(&v);
    Ok((ter, aum, parse_fund_category(&v)))
}

/// (drift net) Single-ticker live SEC companyfacts fetch+parse — the cacheless core of
/// `fetch_fundamentals_sec`, public so the network smoke test can probe the XBRL payload without
/// reading or writing the per-ticker disk cache (the cache-first batch fn would test the disk, not
/// the wire; the shared ticker→CIK map may still come from its own cache — the drift target is the
/// companyfacts payload). Skips the batch budget counter: one probe call, not a universe sweep.
/// Err = environmental (CIK map/transport unavailable); Ok(empty) = fetched but nothing parsed,
/// which for a major US filer means the payload shape drifted.
pub async fn sec_facts_live(client: &Client, urls: &Urls, ticker: &str) -> Result<Vec<core::FundRow>, String> {
    let cik = sec_cik(client, urls, ticker)
        .await
        .ok_or("ticker->CIK resolution failed (map unavailable or non-US filer)")?;
    let v = sec_get_json(client, &urls.sec_companyfacts.replace("{cik}", &cik), &urls.sec_user_agent)
        .await
        .ok_or("companyfacts fetch failed (transport/throttle)")?;
    Ok(parse_sec_facts(&v))
}

/// Per-fund composition for the sector-tilt + fund-valuation footers:
/// (sectors desc by weight, equity/bond split, P/E of the fund's equity book, as-of).
///
/// (fund staleness) The as-of is `None` when the row was fetched THIS RUN and `Some(date)` when it came
/// off disk. It rides here rather than in a parallel map because the P/E it dates travels through six
/// signatures already; a second map would be one more thing to keep in step with this one.
pub type FundMix = (Vec<(String, f64)>, Option<(f64, f64)>, Option<f64>, Option<NaiveDate>);

/// (round 56) Top-10 holdings per fund (each with its weight fraction), for the screen's
/// holdings-overlap + concentration footers — the sector-tech table is full of "different" funds
/// holding the same mega-caps, and that concentration is invisible from the fund names.
/// (sector tilt) also returns each fund's sector weightings + equity/bond split — same payload,
/// previously discarded. (fund valuation) plus the inverted equity-book P/E, same story.
/// Display-only: none of it is scored. Called for the printed fund picks only (a couple dozen
/// symbols), so no request budget needed.
pub async fn yahoo_top_holdings(
    client: &Client,
    syms: &[String],
) -> (HashMap<String, Vec<(String, f64)>>, HashMap<String, FundMix>) {
    // (fetched date, [(holding symbol/name, weight fraction)], [(sector, weight)], stock/bond, fund P/E)
    type Row = (String, Vec<(String, f64)>, Vec<(String, f64)>, Option<(f64, f64)>, Option<f64>);
    let mut cache: HashMap<String, Row> = std::fs::read_to_string(crate::config::data_path(HOLDINGS_CACHE_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let today = chrono::Utc::now().date_naive();
    let mut out = HashMap::new();
    let mut mix = HashMap::new();
    let mut todo: Vec<String> = Vec::new();
    // (fund staleness) A cached row is SERVED and REFETCHED independently, where it used to be one
    // decision. Yahoo's crumb handshake is dead upstream, and the old shape returned only cache HITS
    // from the failure branch below — so a run found 138 rows, 0 of them "fresh" under a 7-day rule,
    // and published nothing at all despite holding 98 perfectly datable P/Es. Now: refetch anything not
    // from today, but still serve what is on disk up to `FUND_CACHE_MAX_AGE_DAYS` while that refetch is
    // impossible. The live pass below overwrites whatever it manages to get.
    for s in syms {
        let age = cache.get(s).and_then(|r| cache_age_days(&r.0, today));
        let (serve, refetch) = cache_use(age);
        if let (Some(r), true) = (cache.get(s), serve) {
            out.insert(s.clone(), r.1.clone());
            // as-of stays None for a row fetched TODAY: it is not "from cache" in any sense the reader
            // cares about, and marking every same-day row would make the mark mean nothing.
            let as_of = (serve && refetch).then(|| today - chrono::Duration::days(age.unwrap_or(0)));
            mix.insert(s.clone(), (r.2.clone(), r.3, r.4, as_of));
        }
        if refetch {
            todo.push(s.clone());
        }
    }
    if !todo.is_empty() {
        // handshake once up front for the explicit skip message; fund_composition_live's own crumb
        // call is then a free OnceLock read per symbol.
        if yahoo_crumb(client).await.is_none() {
            eprintln!("fetch: Yahoo crumb handshake failed — holdings-overlap footer skipped this run");
            return (out, mix);
        }
        let fetched: Vec<Option<(String, Vec<(String, f64)>, Vec<(String, f64)>, Option<(f64, f64)>, Option<f64>)>> =
            stream::iter(todo)
                .map(|sym| async move {
                    // Err (transport/throttle) -> None: symbol stays uncached, retried next run
                    let (holdings, sectors, stock_bond, pe) = fund_composition_live(client, &sym).await.ok()?;
                    Some((sym, holdings, sectors, stock_bond, pe))
                })
                .buffer_unordered(fetch_concurrency())
                .collect()
                .await;
        for (sym, holdings, sectors, stock_bond, pe) in fetched.into_iter().flatten() {
            cache.insert(sym.clone(), (today.to_string(), holdings.clone(), sectors.clone(), stock_bond, pe));
            out.insert(sym.clone(), holdings);
            mix.insert(sym, (sectors, stock_bond, pe, None)); // fetched now -> unmarked, overwriting any stale row served above
        }
        if let Ok(json) = serde_json::to_string(&cache) {
            let _ = std::fs::write(crate::config::data_path(HOLDINGS_CACHE_PATH), json);
        }
    }
    (out, mix)
}

/// (AUM) the same 3-tier BF lookup the TER uses (exact resolved symbol -> same-stem cross-venue ->
/// unique fund-name prefix, retried without Yahoo's umbrella-company prefix) — both values ride the
/// same etp_search row. No FMP fallback: its free tier doesn't serve fund size.
fn bf_aum(ticker: &str, name: &str) -> Option<f64> {
    if let Some(v) = bf_aum_exact(ticker) {
        return Some(v);
    }
    if let Some(v) = ticker.split_once('.').and_then(|(stem, _)| {
        BF_AUM.get()?.iter().find(|(k, _)| k.split('.').next() == Some(stem)).map(|(_, v)| *v)
    }) {
        return Some(v);
    }
    bf_by_name(&BF_AUM_NAMES, name)
        .or_else(|| name.split_once(" - ").and_then(|(_, fund)| bf_by_name(&BF_AUM_NAMES, fund)))
}

/// Exact Börse-Frankfurt AUM map hit — same "presence proves ETP" semantics as `bf_ter_exact`.
fn bf_aum_exact(ticker: &str) -> Option<f64> {
    BF_AUM.get().and_then(|m| m.get(ticker)).copied()
}

/// (USE/REPL/bench) the same 3-tier BF lookup TER/AUM use — the facts ride the same etp_search row.
fn bf_meta(ticker: &str, name: &str) -> BfMeta {
    if let Some(m) = BF_META.get().and_then(|m| m.get(ticker)).cloned() {
        return m;
    }
    if let Some(m) = ticker.split_once('.').and_then(|(stem, _)| {
        BF_META.get()?.iter().find(|(k, _)| k.split('.').next() == Some(stem)).map(|(_, m)| m.clone())
    }) {
        return m;
    }
    bf_by_name(&BF_META_NAMES, name)
        .or_else(|| name.split_once(" - ").and_then(|(_, fund)| bf_by_name(&BF_META_NAMES, fund)))
        .unwrap_or_default()
}

/// (#45) Why `bf_meta` came back empty for one fund. Four causes print an identical `n/a` today, and
/// they call for four different responses — only `AmbiguousName` is a bug this codebase can fix.
#[derive(PartialEq, Clone, Copy)]
enum BfMetaMiss {
    /// BF answered, the row just carried no `replicationMethod` (or wording `bf_row_meta` doesn't
    /// know). The ~30% the field-coverage note predicts. Upstream omission — nothing to fix here.
    NoReplField,
    /// No BF row under this name at all: a venue-list / regulatory (FIRDS) extra, which `fetch_universe`
    /// documents as factless by design. A Stockholm-only fund is not on Börse Frankfurt; no code fixes that.
    NotOnBf,
    /// BF holds the row and the lookup REFUSES it — 2+ BF names share this prefix (or the probe is under
    /// `bf_by_name`'s 10-byte floor), so its uniqueness rule returns None rather than guess between share
    /// classes. Data in hand, discarded. The one bucket where a fix needs no new data source.
    AmbiguousName,
    /// Exactly one BF row matches on a long-enough name, yet it parsed to `BfMeta::default()` — row
    /// present, keyData empty. Detectable only because `BF_ROW_NAMES` keeps what `BF_META_NAMES` drops.
    EmptyKeyData,
}

/// (#45) Classify one `n/a` replication cell. Mirrors `bf_meta`'s own lookup order so the verdict
/// describes the lookup that actually ran, not a re-derivation of it.
fn bf_meta_miss(ticker: &str, name: &str) -> BfMetaMiss {
    // test the FACT-bearing fields, not `!= BfMeta::default()`: `dom` is stamped from the ISIN on every
    // BF row (`m.dom = isin_domicile(...)` where BF_META_NAMES is built), so a `!= default` check is
    // vacuously true for the whole feed and collapses all four causes into this one bucket. Caught by
    // the first live run coming back 2345/2345 here — a split that clean is a bug, not a finding.
    let m = bf_meta(ticker, name);
    if m.use_of.is_some() || m.repl.is_some() || m.bench.is_some() {
        return BfMetaMiss::NoReplField; // BF answered with facts — it just had no replication token
    }
    // count prefix hits the way `bf_by_name` matches (whole name, then the part after " - "), but
    // COUNTING instead of demanding uniqueness — the count is the whole diagnostic.
    let probe = |n: &str| -> Option<(usize, usize)> {
        let n = n.trim().to_lowercase();
        let hits = BF_ROW_NAMES.get()?.iter().filter(|bf| bf.starts_with(&n)).count();
        (hits > 0).then_some((hits, n.len())) // bytes, matching bf_by_name's own `n.len() < 10`
    };
    let Some((hits, len)) = probe(name).or_else(|| name.split_once(" - ").and_then(|(_, f)| probe(f)))
    else {
        return BfMetaMiss::NotOnBf;
    };
    if hits >= 2 || len < 10 { BfMetaMiss::AmbiguousName } else { BfMetaMiss::EmptyKeyData }
}

/// (#45) Why USE/REPL read `n/a` across the ETF table, bucketed by cause. One pure pass over the quotes
/// already in hand — no locks and no instrumentation inside the concurrent fetch path, since every
/// input is a static that `fetch_universe` finished writing long before. `None` when nothing is missing.
///
/// Blind spot, stated: counts only rows tagged ETF, so a physical ETC that Yahoo labels EQUITY is not
/// tallied — the same rows `bf_ter_exact`/`bf_aum_exact` exist to rescue.
pub fn bf_meta_miss_report(quotes: &[Quote]) -> Option<String> {
    let etfs = || quotes.iter().filter(|q| q.instrument_type.eq_ignore_ascii_case("ETF"));
    let (total, use_na) = (etfs().count(), etfs().filter(|q| q.use_of_profits.is_none()).count());
    // sample cap per bucket: the three unfixable causes only need to be recognisable, but the fixable
    // one IS the worklist — printing it in full is the difference between "50 somewhere" and 50 names.
    let mut buckets: Vec<(BfMetaMiss, &str, usize, usize, Vec<&str>)> = vec![
        (BfMetaMiss::NotOnBf, "not on BF (venue/regulatory extra — factless by design)", 3, 0, Vec::new()),
        (BfMetaMiss::NoReplField, "BF row, no replicationMethod (upstream omission)", 3, 0, Vec::new()),
        (BfMetaMiss::EmptyKeyData, "BF row, empty keyData", 3, 0, Vec::new()),
        (BfMetaMiss::AmbiguousName, "ambiguous name -> lookup miss (FIXABLE: BF holds the row)", 80, 0, Vec::new()),
    ];
    for q in etfs().filter(|q| q.replication.is_none()) {
        let cause = bf_meta_miss(&q.ticker, &q.name);
        if let Some(b) = buckets.iter_mut().find(|b| b.0 == cause) {
            b.3 += 1;
            if b.4.len() < b.2 {
                b.4.push(&q.ticker);
            }
        }
    }
    let repl_na: usize = buckets.iter().map(|b| b.3).sum();
    if repl_na == 0 {
        return None;
    }
    let mut s = format!("fetch: BF meta missing for {repl_na}/{total} ETFs — USE n/a {use_na}, REPL n/a {repl_na}");
    for (_, label, cap, n, ex) in buckets.iter().filter(|b| b.3 > 0) {
        // say when the list is truncated — a silent cut reads as "that's all of them"
        let more = if n > cap { format!(", +{} more", n - cap) } else { String::new() };
        s.push_str(&format!("\n  {label}: {n} ({}{more})", ex.join(", ")));
    }
    Some(s)
}

const BF_TER_KEYS: &[&str] = &["ter", "totalExpenseRatio", "ongoingCharges", "ongoingCharge", "totalExpenseRatioInPercent"];

/// Pull the expense ratio out of one BF `etp_search` row. Tries the known key names (their schema drifts
/// over time); BF sends the value as a FRACTION (0.002 = 0.20% — verified live 2026-07: all 3468 rows,
/// VUAA = 0.0007 = its known 0.07%), so ×100 to percent. None if absent / nonsense.
/// BF nests the fund detail ONE level down (keyData / overview / performance sub-objects), so scan the
/// row's own keys AND one level into any sub-object. Value may arrive as a number OR a string ("0,20%").
/// ponytail: if BF ever flips back to percent, ×100 blows past the <5 sanity filter -> ter_n=0 -> the
/// first-row-fields diagnostic fires; drop the ×100 then.
fn bf_row_ter(row: &Value) -> Option<f64> {
    fn num(v: &Value) -> Option<f64> {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().trim_end_matches('%').trim().replace(',', ".").parse().ok()))
    }
    fn hit(obj: &serde_json::Map<String, Value>) -> Option<f64> {
        BF_TER_KEYS
            .iter()
            .find_map(|k| obj.iter().find(|(rk, _)| rk.eq_ignore_ascii_case(k)).and_then(|(_, v)| num(v)))
    }
    let obj = row.as_object()?;
    hit(obj)
        .or_else(|| obj.values().filter_map(|v| v.as_object()).find_map(hit))
        .map(|t| t * 100.0)
        .filter(|t| t.is_finite() && *t > 0.0 && *t < 5.0)
}

/// Pull the fund size out of one BF `etp_search` row (`overview.assetsUnderManagement`, absolute
/// fund-currency units — verified live 2026-07: 3391 of 3468 rows carry it). None if absent/nonsense.
fn bf_row_aum(row: &Value) -> Option<f64> {
    row.pointer("/overview/assetsUnderManagement")?.as_f64().filter(|v| v.is_finite() && *v > 0.0)
}

/// One BF keyData string field, English translation preferred over the original value.
fn bf_row_keydata<'a>(row: &'a Value, field: &str) -> Option<&'a str> {
    row.pointer(&format!("/keyData/{field}/translations/en"))
        .or_else(|| row.pointer(&format!("/keyData/{field}/originalValue")))?
        .as_str()
}

/// (USE/REPL) share-class + replication tokens from one BF row (`keyData.useOfProfits` on 85% of
/// rows, `keyData.replicationMethod` on 70% — verified live 2026-07). Unknown wording -> None, so a
/// BF vocabulary drift blanks the cell ("n/a") instead of printing something wrong.
fn bf_row_meta(row: &Value) -> BfMeta {
    let use_of = bf_row_keydata(row, "useOfProfits").and_then(|s| match s {
        "Accumulating" => Some("Acc"),
        "Distributing" => Some("Dist"),
        _ => None,
    });
    let repl = bf_row_keydata(row, "replicationMethod").and_then(|s| match s {
        "Swap-based" => Some("Swap"),
        "Full replication" => Some("Full"),
        "Optimised" => Some("Opt"),
        "Hybrid" => Some("Hybr"),
        "Sample" => Some("Samp"),
        _ => None,
    });
    // benchmark: free-text index name, kept as-is (lowercased) — used for same-index twin hints only
    let bench = bf_row_keydata(row, "benchmark").map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    // dom is NOT on the BF row payload path — it's stamped from the ISIN wherever the ISIN is in
    // hand (name-keyed capture + the resolution closure), so all sources share one derivation.
    BfMeta { use_of, repl, bench, dom: None }
}

/// (round 49) Share-class token from the LISTING NAME, the fallback for funds with no BF row
/// (venue/regulatory sources ship name-only). Word-split kills substring false positives
/// ("Vaccine" ≠ "acc"). Unknown wording -> None (honest n/a), same stance as bf_row_meta.
fn use_from_name(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    let word = |t: &[&str]| lower.split(|c: char| !c.is_alphanumeric()).any(|w| t.contains(&w));
    if word(&["acc", "accumulating"]) {
        Some("Acc")
    } else if word(&["dist", "distributing"]) {
        Some("Dist")
    } else {
        None
    }
}

/// One ETF as it travels through `fetch_xetra_etfs`: ISIN (later the Yahoo symbol), TER, AUM, meta.
type BfRow = (String, Option<f64>, Option<f64>, BfMeta);

/// Does this row carry any BF fact worth storing, or is it an all-`None` shell?
///
/// The same test is written at both ends of `fetch_xetra_etfs` — once when capturing name-keyed meta
/// from the BF payload, once when filling the symbol-keyed map — so it is one function now.
/// Inverting it stores an empty `BfMeta` for every fund, which reads downstream as "BF answered,
/// facts genuinely absent" and permanently silences the name-keyed fallback that would have found
/// them. `dom` counts: a venue-only fund with nothing but a domicile is still worth a cell.
fn carries_fund_facts(m: &BfMeta) -> bool {
    *m != BfMeta::default()
}

/// Merge the venue-list and regulatory ISINs into BF's own top-`cap` rows, dropping any BF already
/// returned, and report how many extras each group contributed.
///
/// Pure, and split out because all three of its decisions survive inline. Dropping either `!` re-adds
/// every ISIN BF already answered, so the same fund is searched twice, occupies two universe slots
/// and blows past the cap the caller asked for. Turning the `-` into `+` corrupts `extra_n`, the
/// figure the run diagnostic prints to separate "BF returned nothing" from "the venue lists are
/// thin" — the two failures that empty the ETF tables in exactly the same way.
fn merge_isin_lists(
    rows: Vec<BfRow>,
    extra_isins: Vec<String>,
    speculative_isins: Vec<String>,
    cap: usize,
) -> (Vec<BfRow>, usize, std::collections::HashSet<String>) {
    let total = rows.len();
    let bf_isins: std::collections::HashSet<String> = rows.iter().map(|(isin, ..)| isin.clone()).collect();
    let mut top: Vec<BfRow> = rows.into_iter().take(cap).collect();
    top.extend(
        extra_isins
            .into_iter()
            .filter(|isin| !bf_isins.contains(isin))
            .map(|isin| (isin, None, None, BfMeta::default())),
    );
    let extra_n = top.len() - total.min(cap);
    // speculative (regulatory-only) ISINs last; the set doubles as the negative-cache eligibility
    // check inside the resolution closure (caller already removed venue-list overlaps).
    let spec_set: std::collections::HashSet<String> =
        speculative_isins.into_iter().filter(|isin| !bf_isins.contains(isin)).collect();
    top.extend(spec_set.iter().cloned().map(|isin| (isin, None, None, BfMeta::default())));
    (top, extra_n, spec_set)
}

/// Is this a regulatory-only ISIN whose 30-day "definitively not on Yahoo" entry is still fresh?
///
/// Both halves are load-bearing in opposite directions, which is why this is a named function.
/// Dropping the `speculative` conjunct extends the negative cache to venue-listed funds, which are
/// never written into it — harmless today, a silent 30-day blackout the moment anything else is.
/// Widening the comparison (`<` -> `<=` or `>`, or `-` -> `+`) either brands a good ISIN dead
/// roughly forever, or expires every entry immediately and restores the ~1k dead searches per run
/// this cache exists to stop. A malformed or missing date is NOT blocking: re-searching once is
/// cheaper than a permanent false negative.
fn neg_cache_blocks(speculative: bool, cached: Option<&String>, today: NaiveDate) -> bool {
    speculative
        && cached.is_some_and(|d| {
            NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok_and(|d| (today - d).num_days() < 30)
        })
}

/// Yahoo's fallback symbol for an ISIN whose only indexed listing is Stuttgart is `<ISIN>.SG` — a
/// chart-less venue, so every such resolution is a guaranteed dead fetch (716 of the 783 old "no
/// Yahoo data" gate-outs). A real liquid listing (`.DE`/`.MI`/`.L`/`.AS`…) would have ranked first,
/// so there is nothing to rescue.
///
/// Inverting this keeps ONLY the dead venue and discards every tradable listing, emptying the ETF
/// tables while every diagnostic still reports a healthy resolution count.
/// note: only `.SG` shows up in practice; add the suffix here if another appears.
fn usable_symbol(sym: &str) -> bool {
    !sym.ends_with(".SG")
}

/// Yahoo returned no symbol — is that a definitive miss worth negative-caching for 30 days?
///
/// ONLY when the payload is search-shaped (carries a `quotes` array). A rate-limit or error JSON is
/// also symbol-less, and letting it through here brands a perfectly good ISIN dead for a month on
/// nothing but a throttled request — the cache is keyed by ISIN, so the fund vanishes from the
/// universe until the TTL lapses. Forcing it true does exactly that; forcing it false leaks the dead
/// searches back. Non-speculative ISINs are never negative-cached at all.
fn definitive_miss(speculative: bool, v: &Value) -> bool {
    speculative && v.get("quotes").is_some_and(|q| q.is_array())
}

/// Did the BF rows arrive but parse to no TER at all? Then a field name moved and the operator needs
/// the first row's key list to repair `bf_row_ter`.
///
/// Extracted rather than tested through its `eprintln!`: capturing stderr in-process needs an
/// unstable API, and asserting on the message text breaks the next time anyone rewords it. Flipping
/// the `==` or dropping the `!` fires the "fix your parser" advice on every healthy run, which
/// trains the operator to ignore the one run where it is true.
fn no_ter_parsed(ter_n: usize, first_keys: &str) -> bool {
    ter_n == 0 && !first_keys.is_empty()
}

/// The EU-buyable UCITS ETF universe: ask Börse Frankfurt for the top-`cap` ETFs by turnover (real
/// EU-listed, PRIIPs-compliant funds — unlike the US-domiciled NASDAQ-Trader ETFs an EU broker can't
/// sell), then resolve each ISIN to a Yahoo symbol via Yahoo search (first hit = the liquid EU
/// listing, e.g. `.MI`/`.L`/`.DE`). Concurrency-bounded. Empty (with a warning) if the signed API
/// rejects us — salt rotated / endpoint moved; refresh `bf_salt`/`bf_etf_search` in settings.yaml.
/// `extra_isins` = ISINs from other venue lists (Euronext/SIX) merged in AFTER the BF top-`cap`
/// cut — they ride the same ISIN->Yahoo bridge but carry no BF facts (TER/AUM/USE/REPL/benchmark
/// all None -> honest n/a cells; the AUM gate never gates None by design). `speculative_isins` =
/// regulatory-sourced (FIRDS) ISINs on no venue list: same posture, except their definitive
/// Yahoo misses are negative-cached for 30 days — roughly half never resolve, and retrying ~1k
/// dead searches every run would cost minutes forever.
pub async fn fetch_xetra_etfs(
    client: &Client,
    urls: &Urls,
    cap: usize,
    extra_isins: Vec<String>,
    speculative_isins: Vec<String>,
) -> Vec<String> {
    let body = serde_json::json!({
        "indices": [], "regions": [], "countries": [], "issuer": [], "types": [],
        "benchmarks": [], "currency": [], "strategy": [], "replicationType": [], "distributionType": [],
        "page": 0, "pageSize": cap, "sorting": "TURNOVER", "sortOrder": "DESC"
    });
    // Capture (isin, TER) per row — the TER is on the SAME search response, so EU UCITS expense ratios
    // come free here (no per-name FMP call, which doesn't cover them anyway). first_keys = the first
    // row's field names, logged ONLY if zero TERs parse, so a renamed BF field self-diagnoses in one run.
    let mut first_keys = String::new();
    let rows: Vec<(String, Option<f64>, Option<f64>, BfMeta)> = match borse_frankfurt_post(client, &urls.bf_etf_search, &urls.bf_salt, &body).await {
        Some(j) => match j.get("data").and_then(|d| d.as_array()) {
            Some(arr) => {
                // name-keyed TER + AUM lists from ALL rows (not just the top-cap): a pinned name's fund
                // can sit below the turnover cutoff and still deserve its TER / fund size.
                let _ = BF_TER_NAMES.set(
                    arr.iter()
                        .filter_map(|r| {
                            Some((r.pointer("/name/originalValue")?.as_str()?.trim().to_lowercase(), bf_row_ter(r)?))
                        })
                        .collect(),
                );
                let _ = BF_AUM_NAMES.set(
                    arr.iter()
                        .filter_map(|r| {
                            Some((r.pointer("/name/originalValue")?.as_str()?.trim().to_lowercase(), bf_row_aum(r)?))
                        })
                        .collect(),
                );
                let _ = BF_META_NAMES.set(
                    arr.iter()
                        .filter_map(|r| {
                            let mut m = bf_row_meta(r);
                            m.dom = isin_domicile(r.get("isin")?.as_str()?);
                            carries_fund_facts(&m)
                                .then_some((r.pointer("/name/originalValue")?.as_str()?.trim().to_lowercase(), m))
                        })
                        .collect(),
                );
                // (#45) the same names WITHOUT the `!= default` filter above, so the diagnostic can tell
                // "no BF row" from "BF row, empty keyData". Same payload, no extra request.
                let _ = BF_ROW_NAMES.set(
                    arr.iter()
                        .filter_map(|r| Some(r.pointer("/name/originalValue")?.as_str()?.trim().to_lowercase()))
                        .collect(),
                );
                if let Some(o) = arr.first().and_then(|r| r.as_object()) {
                    // top-level keys PLUS one level into each sub-object (TER nests under keyData/overview/
                    // performance), so a renamed field anywhere self-diagnoses in one run.
                    let mut fields: Vec<String> = o.keys().cloned().collect();
                    for (k, v) in o {
                        if let Some(sub) = v.as_object() {
                            fields.extend(sub.keys().map(|sk| format!("{k}.{sk}")));
                        }
                    }
                    first_keys = fields.join(",");
                }
                arr.iter().filter_map(|r| Some((r.get("isin")?.as_str()?.to_string(), bf_row_ter(r), bf_row_aum(r), bf_row_meta(r)))).collect()
            }
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    if rows.is_empty() {
        eprintln!("fetch: Börse Frankfurt ETF search returned nothing (salt rotated? refresh bf_salt) — falling back to the Euronext ISINs alone");
    }
    // BF ignores our pageSize and dumps the whole list (~3430); it's TURNOVER-DESC, so the top `cap`
    // are the most-liquid ETFs — keep only those, both to match universe_size and to avoid firing
    // thousands of Yahoo searches (which DO rate-limit). Euronext-only ISINs append after the cut so
    // BF's turnover ranking (and the cap semantics) stay untouched.
    let total = rows.len();
    let (top, extra_n, spec_set) = merge_isin_lists(rows, extra_isins, speculative_isins, cap);
    let reg_n = spec_set.len();
    if top.is_empty() {
        return Vec::new();
    }
    // resolve ISIN -> Yahoo symbol (first quote = the liquid EU listing), bounded fan-out, carrying the
    // captured TER + AUM + meta alongside. yahoo_search is tuned for news (quotesCount=0) — flip it to quotes here.
    // A persistent ISIN->symbol cache fronts the Yahoo search: resolution is FLAKY per run (a fund
    // found once can silently vanish the next screen when its search hiccups), and ISIN->symbol is
    // stable, so positive resolutions are remembered forever. Negative results are NOT cached
    // (retried next run); a delisted cached symbol self-gates downstream via the no-chart path.
    let isin_cache: HashMap<String, String> = std::fs::read_to_string(crate::config::data_path(ISIN_CACHE_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let neg_cache: HashMap<String, String> = std::fs::read_to_string(crate::config::data_path(ISIN_NEG_CACHE_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let today = chrono::Utc::now().date_naive();
    let cache_ref = &isin_cache;
    let neg_ref = &neg_cache;
    let spec_ref = &spec_set;
    // Ok((sym, ter, aum, meta, Some(isin) when freshly resolved -> write-back));
    // Err(Some(isin)) = definitive Yahoo miss on a speculative ISIN -> negative-cache write-back;
    // Err(None) = everything else dropped (cached negative, transport error, .SG fallback).
    type Resolved = (String, Option<f64>, Option<f64>, BfMeta, Option<String>);
    let outcomes: Vec<Result<Resolved, Option<String>>> = stream::iter(top)
        .map(|(isin, ter, aum, mut meta)| async move {
            // domicile rides the ISIN every source already carries — set it here so BF, venue-list and
            // regulatory funds all get it, on both the cache-hit and fresh-resolution paths.
            meta.dom = isin_domicile(&isin);
            if let Some(sym) = cache_ref.get(&isin) {
                return Ok((sym.clone(), ter, aum, meta, None));
            }
            let speculative = spec_ref.contains(&isin);
            if neg_cache_blocks(speculative, neg_ref.get(&isin), today) {
                return Err(None); // known-dead ISIN, TTL not expired — skip the search
            }
            let url = urls
                .yahoo_search
                .replace("{ticker}", isin.as_str())
                .replace("quotesCount=0", "quotesCount=1")
                .replace("newsCount=3", "newsCount=0");
            let Some(v) = get_json(client, &url).await else {
                return Err(None); // transport error — never a negative, retried next run
            };
            match v.pointer("/quotes/0/symbol").and_then(|s| s.as_str()) {
                Some(sym) if usable_symbol(sym) => Ok((sym.to_string(), ter, aum, meta, Some(isin))),
                Some(_) => Err(None), // Stuttgart-only — see usable_symbol
                None => Err(definitive_miss(speculative, &v).then(|| isin.clone())),
            }
        })
        .buffer_unordered(fetch_concurrency())
        .collect()
        .await;
    let mut resolved: Vec<Resolved> = Vec::new();
    let mut misses: Vec<String> = Vec::new();
    for o in outcomes {
        match o {
            Ok(t) => resolved.push(t),
            Err(Some(isin)) => misses.push(isin),
            Err(None) => {}
        }
    }
    // write definitive misses back (best-effort, like the positive cache below)
    if !misses.is_empty() {
        let mut neg = neg_cache.clone();
        for isin in misses {
            neg.insert(isin, today.to_string());
        }
        if let Ok(json) = serde_json::to_string(&neg) {
            let _ = std::fs::write(crate::config::data_path(ISIN_NEG_CACHE_PATH), json);
        }
    }
    // write fresh resolutions back (best-effort — a read-only dir just costs next run's re-search)
    let fresh: Vec<(String, String)> = resolved
        .iter()
        .filter_map(|(sym, _, _, _, src)| src.as_ref().map(|isin| (isin.clone(), sym.clone())))
        .collect();
    let fresh_n = fresh.len();
    if !fresh.is_empty() {
        let mut isin_cache = isin_cache;
        isin_cache.extend(fresh);
        if let Ok(json) = serde_json::to_string(&isin_cache) {
            let _ = std::fs::write(crate::config::data_path(ISIN_CACHE_PATH), json);
        }
    }
    // split into the ticker list + the symbol->TER and symbol->AUM maps (each stored once;
    // fetch_expense / bf_aum read them).
    let mut ter_map: HashMap<String, f64> = HashMap::new();
    let mut aum_map: HashMap<String, f64> = HashMap::new();
    let mut meta_map: HashMap<String, BfMeta> = HashMap::new();
    // (TER/AUM) symbol -> ISIN, the key justETF needs. Built here because this is the one place the two
    // identifiers are side by side; every later caller has only the symbol.
    let mut isin_of: HashMap<String, String> = HashMap::new();
    let tickers: Vec<String> = resolved
        .into_iter()
        .map(|(sym, ter, aum, meta, src)| {
            if let Some(t) = ter {
                ter_map.insert(sym.clone(), t);
            }
            if let Some(a) = aum {
                aum_map.insert(sym.clone(), a);
            }
            if carries_fund_facts(&meta) {
                meta_map.insert(sym.clone(), meta);
            }
            if let Some(isin) = src {
                isin_of.insert(sym.clone(), isin);
            }
            sym
        })
        .collect();
    // (round 47) top up missing TER/AUM from Yahoo into the SEPARATE display-only fallback statics —
    // venue/regulatory-only funds (no BF row -> factless forever) get honest cells; the BF maps that
    // feed the score/gates stay untouched so momentum ranks are byte-identical with pre-fallback runs.
    let (yh_ter, yh_aum) = yahoo_fund_facts_fill(client, urls, &tickers, &ter_map, &aum_map, &isin_of).await;
    let _ = YH_TER.set(yh_ter);
    let _ = YH_AUM.set(yh_aum);
    let ter_n = ter_map.len();
    let _ = BF_TER.set(ter_map);
    let _ = BF_AUM.set(aum_map);
    let _ = BF_META.set(meta_map);
    // conclusive diagnostic: distinguishes "BF gave 0 ISINs" from "BF ok but Yahoo bridge resolved
    // none" — the two ways the ETF tables silently empty.
    eprintln!("fetch: Börse Frankfurt returned {total} ETF ISINs (kept top {} by turnover) + {extra_n} venue-list extras + {reg_n} regulatory extras; {} resolved to Yahoo tickers ({} from cache, TER for {ter_n})", total.min(cap), tickers.len(), tickers.len() - fresh_n);
    if no_ter_parsed(ter_n, &first_keys) {
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
/// thin one just returns no Yahoo data downstream and self-gates. Retries once (chart_json's
/// fallback-retry shape: 2 attempts, 400ms apart) — a transient blip/throttle emptied the whole
/// Lisbon leg on 2026-07-04 — then degrades to an empty Vec with a diagnostic that carries the last
/// HTTP status, so the rest of the universe still builds.
/// note: symbol->`.LS` direct map (no ISIN->Yahoo-search bridge); add the bridge fetch_xetra_etfs
/// already has only if coverage turns out poor.
pub async fn fetch_euronext_lisbon(client: &Client, urls: &Urls) -> Vec<String> {
    // the table's columns, in order; index 2 (symbol) is what core::euronext_lisbon_symbols reads.
    // Raw body (not reqwest `.form()`) so the `args[...]` key keeps its literal brackets, matching the
    // request the page's JS sends.
    let body = "args[display_datapoints]=name,isin,symbol,market,lastPrice,precentDayChange,lastTradeTime\
                &draw=1&start=0&length=1000&iDisplayLength=1000&iDisplayStart=0";
    let mut last_status = String::from("no response");
    for attempt in 0..2 {
        let resp = client
            .post(&urls.euronext_lisbon)
            .header("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
            .header("X-Requested-With", "XMLHttpRequest")
            .body(body)
            .send()
            .await;
        if let Ok(r) = resp {
            last_status = r.status().to_string();
            // success = non-empty tickers, so a 200 with empty/missing aaData retries too
            if let Ok(v) = r.json::<Value>().await {
                let tickers = core::euronext_lisbon_symbols(&v);
                if !tickers.is_empty() {
                    return tickers;
                }
            }
        }
        if attempt == 0 {
            tokio::time::sleep(StdDuration::from_millis(400)).await;
        }
    }
    eprintln!("fetch: Euronext Lisbon failed after 2 attempts (last status: {last_status}) — Lisbon stocks absent from the screen");
    Vec::new()
}

/// Euronext ETF list ("track") -> ISINs for the ETF universe. Second venue source beside Börse
/// Frankfurt: Paris/Amsterdam/Milan/Brussels/Dublin/Oslo carry ~660 UCITS funds BF never lists
/// (measured 2026-07-05: 2580 unique ISINs, 659 not in BF's 3468). Same DataTables POST as
/// `fetch_euronext_lisbon`, but paged: the server answers with the right row COUNT and EMPTY
/// `aaData` above ~1000 rows per request, so ask 1000 at a time until a short page. Per-page
/// retry mirrors Lisbon's 2-attempt shape (a transient blip emptied that leg once). Degrades to
/// whatever pages arrived (or empty, with a diagnostic) — the BF leg still builds the universe.
pub async fn fetch_euronext_etf_isins(client: &Client, urls: &Urls) -> Vec<String> {
    const PAGE: usize = 1000;
    let mut isins: Vec<String> = Vec::new();
    let mut last_status = String::from("no response");
    // ponytail: hard stop at 10 pages (~10k rows) — the list is ~3.3k; a runaway server can't loop us
    'pages: for page in 0..10 {
        let start = page * PAGE;
        // raw body (not `.form()`) so the `args[...]` key keeps its literal brackets; WITHOUT
        // `display_datapoints` the server returns the right count but empty cells (Lisbon lesson).
        let body = format!(
            "args[display_datapoints]=name,isin,symbol,market&draw=1&start={start}&length={PAGE}&iDisplayLength={PAGE}&iDisplayStart={start}"
        );
        for attempt in 0..2 {
            let resp = client
                .post(&urls.euronext_track)
                // a 1000-row page takes ~18s server-side — the client's 15s default timed the
                // whole leg out; per-request override, scoped here so quote fetches stay snappy
                .timeout(StdDuration::from_secs(60))
                .header("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
                .header("X-Requested-With", "XMLHttpRequest")
                .body(body.clone())
                .send()
                .await;
            if let Ok(r) = resp {
                last_status = r.status().to_string();
                if let Ok(v) = r.json::<Value>().await {
                    // page length judged on RAW aaData rows (not parsed ISINs): a malformed row must
                    // not make a full page look short and truncate the walk.
                    let rows_n = v.get("aaData").and_then(|d| d.as_array()).map_or(0, |a| a.len());
                    if rows_n > 0 {
                        isins.extend(core::euronext_track_isins(&v));
                        if rows_n < PAGE {
                            break 'pages; // short page = end of list
                        }
                        continue 'pages;
                    }
                }
            }
            if attempt == 0 {
                tokio::time::sleep(StdDuration::from_millis(400)).await;
            }
        }
        // both attempts empty: on page 0 the leg failed; on a later page it's just the end of the list
        if start == 0 {
            eprintln!("fetch: Euronext ETF list failed after 2 attempts (last status: {last_status}) — Euronext-only ETFs absent from the screen");
        }
        break;
    }
    isins.sort();
    isins.dedup(); // cross-listed funds repeat per venue row
    isins
}

/// SIX Swiss Exchange fund list -> ETF/UCITS-named ISINs for the ETF universe (third venue source;
/// SIX-only funds measured 2026-07-06: ~258 after the name funnel — the FU segment also carries
/// Swiss MUTUAL funds, which `core::six_fund_isins` drops so they can't reach the ETF table
/// mislabeled). One plain unsigned GET, whole list in one page. Degrades to empty + a diagnostic.
///
/// UNGRADEABLE, hence the skip: one live GET plus a `core::six_fund_isins` call that is pinned where
/// it lives. Same `offline()` pinning problem as `yahoo_crumb`.
#[mutants::skip]
pub async fn fetch_six_etf_isins(client: &Client, urls: &Urls) -> Vec<String> {
    // third seam that bypassed `get_json`/`get_text` — this one because it needs a 60s per-request
    // timeout. Returns before the diagnostic below: offline is not a SIX outage, and saying so would
    // train the reader to ignore the line that reports a real one.
    if offline() {
        return Vec::new();
    }
    // 60s per-request override, same lesson as the Euronext pages (client default is 15s total)
    let payload = match client.get(&urls.six_funds).timeout(StdDuration::from_secs(60)).send().await {
        Ok(r) => r.json::<Value>().await.ok(),
        Err(_) => None,
    };
    let mut isins = payload.as_ref().map(core::six_fund_isins).unwrap_or_default();
    if isins.is_empty() {
        eprintln!("fetch: SIX fund list returned nothing — SIX-only ETFs absent from the screen");
    }
    isins.sort();
    isins.dedup();
    isins
}

/// ESMA + FCA FIRDS FULINS_C dumps -> the EU/UK regulators' complete ETF ISIN list (fourth
/// universe source; measured 2026-07-06: ~1.8k candidates beyond BF+Euronext+SIX after the
/// name/domicile funnel, ~half resolve on Yahoo, heavily LSE-listed — closes the deferred UK/LSE
/// hole without scraping). The dumps are weekly, so the scan result is cached and refreshed only
/// when >6 days old (normal runs cost nothing); a refresh is two registry GETs + two ~3-7MB zip
/// downloads scanned off the async threads. The cache is only overwritten when BOTH legs
/// delivered — a one-leg outage merges the partial result over the last-good copy instead, so
/// the universe never silently shrinks (round-36 lesson).
pub async fn fetch_regulatory_etf_isins(client: &Client, urls: &Urls) -> Vec<String> {
    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct RegCache {
        fetched: String,
        isins: Vec<String>,
    }
    let cached: RegCache = std::fs::read_to_string(crate::config::data_path(REGULATORY_ISINS_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let today = chrono::Utc::now().date_naive();
    if !cached.isins.is_empty()
        && NaiveDate::parse_from_str(&cached.fetched, "%Y-%m-%d")
            .is_ok_and(|d| (today - d).num_days() < 7)
    {
        return cached.isins;
    }
    let mut all: Vec<String> = Vec::new();
    let mut complete = true;
    for registry in [&urls.esma_firds, &urls.fca_firds] {
        let link = get_json(client, registry).await.as_ref().and_then(core::firds_latest_fulins_link);
        let bytes = match &link {
            Some(l) => match client.get(l).timeout(StdDuration::from_secs(120)).send().await {
                Ok(r) => r.bytes().await.ok(),
                Err(_) => None,
            },
            None => None,
        };
        // unzip + scan off the async runtime: the FCA XML is ~165MB of regex haystack
        let isins = match bytes {
            Some(b) => tokio::task::spawn_blocking(move || {
                let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(b)) else {
                    return Vec::new();
                };
                let Ok(mut file) = zip.by_index(0) else { return Vec::new() };
                let mut xml = String::new();
                if std::io::Read::read_to_string(&mut file, &mut xml).is_err() {
                    return Vec::new();
                }
                core::firds_etf_isins(&xml)
            })
            .await
            .unwrap_or_default(),
            None => Vec::new(),
        };
        if isins.is_empty() {
            complete = false;
        }
        all.extend(isins);
    }
    all.sort();
    all.dedup();
    if complete && !all.is_empty() {
        if let Ok(json) = serde_json::to_string(&RegCache { fetched: today.to_string(), isins: all.clone() }) {
            let _ = std::fs::write(crate::config::data_path(REGULATORY_ISINS_PATH), json);
        }
        return all;
    }
    if !cached.isins.is_empty() {
        eprintln!(
            "fetch: FIRDS regulatory list incomplete — merging over last-good copy ({} ISINs)",
            cached.isins.len()
        );
    } else if all.is_empty() {
        eprintln!("fetch: FIRDS regulatory list unavailable — regulatory-only ETFs absent from the screen");
    }
    all.extend(cached.isins);
    all.sort();
    all.dedup();
    all
}

/// Build the `screen` universe LIVE (no hand-kept list): top-`cap` crypto by market cap from
/// CoinGecko + the S&P 500 constituents CSV (single companies) + the top-`cap` EU-buyable UCITS ETFs
/// by turnover from Börse Frankfurt plus the Euronext-only funds BF doesn't list
/// (`fetch_xetra_etfs` + `fetch_euronext_etf_isins`). Symbols normalised to Yahoo form (`btc` ->
/// `BTC-EUR`/`BTC-USD`, `BRK.B` -> `BRK-B`). Crypto quote currency follows `prefer_eur`. The old
/// US-listed NASDAQ-Trader ETFs are dropped: none are EU-buyable, so they only wasted fetches.
/// Sorted + deduped; empty if all sources fail. Also returns the Xetra-ETF ticker set so the caller
/// can force-classify them as ETF — Yahoo mislabels some (e.g. structured products) as `EQUITY`, which
/// would otherwise leak them into the stocks table past the sector filter — and the ticker -> GICS
/// sector map (constituent-CSV stocks only) so the stocks table can print its sector mix.
/// (Item 18/32) The equity ponds: S&P 500 plus any extra same-format constituent CSVs from config, each
/// row a (Yahoo symbol, GICS sector). Sequential — 1-3 URLs, negligible next to the universe fan-out —
/// and a failed/empty CSV just drops its pond instead of crashing. A CSV (Symbol first, sector col 3) and
/// a Wikipedia constituents page (the only maintained source for e.g. the S&P MidCap 400) parse
/// differently; the URL's host picks.
///
/// (#44) Split out of `fetch_universe` so the EXPLICIT-ARGS path (`screen CF`, `screen --explain CF`) can
/// join sectors too. That path skips the universe fetch by design, which left `Quote.sector` None and made
/// a commodity name explain UNDAMPED (22.25) while the full screen ranked it damped (17.84) — the one
/// place the `c` flag most needs to reconcile. These are small documents, so paying for them on a
/// one-name query is cheap; the heavy CoinGecko/ETF/Lisbon legs stay skipped.
pub async fn constituent_ponds(client: &Client, urls: &Urls, sectors: &[String]) -> Vec<Vec<(String, String)>> {
    let mut ponds: Vec<Vec<(String, String)>> = Vec::new();
    for url in std::iter::once(&urls.sp500_csv).chain(urls.constituents_csv.iter()) {
        let Some(text) = get_text(client, url).await else {
            eprintln!("fetch: constituents CSV {url} unavailable — its stocks absent from the screen");
            continue;
        };
        ponds.push(if url.contains("wikipedia.org") {
            core::wiki_constituents(&text, sectors)
        } else {
            text.lines().skip(1).filter_map(|l| core::sector_symbol(l, sectors)).collect()
        });
    }
    ponds
}

/// (#44) Ticker -> GICS sector over those ponds, first pond winning (a dual-member keeps its real sector).
/// The explicit-args entry point; `fetch_universe` builds the same map inline while it walks the ponds.
pub async fn sector_map(client: &Client, urls: &Urls, sectors: &[String]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pond in constituent_ponds(client, urls, sectors).await {
        for (sym, sector) in pond {
            out.entry(sym).or_insert(sector);
        }
    }
    out
}

/// (EU listing) US symbol -> its verified Xetra Yahoo symbol (`GOOGL` -> `ABEA.DE`). Only read when
/// `prefer_eu_listing` is on, and the mapping is stable, so resolutions are remembered forever.
///
/// ponytail: ONE file, not the positive/negative pair `fetch_xetra_etfs` keeps. A known-absent twin is
/// stored as the EMPTY STRING, which costs no TTL machinery and no second const. The ceiling that buys:
/// a negative never expires, so a listing that appears later stays unseen until this file is deleted.
/// Cheap failure — the name simply keeps its US row — and `rm .eu_listing_cache.json` is the upgrade
/// path. Swap in a dated negative like `ISIN_NEG_CACHE_PATH`'s if that ever actually bites.
pub(crate) const EU_LISTING_CACHE_PATH: &str = ".eu_listing_cache.json";

/// OpenFIGI's keyless job cap. MEASURED, not guessed: 10 jobs returns HTTP 200, 20 and 50 both return
/// HTTP 413 Payload Too Large. The published rate limit alongside it is 25 requests/minute, which is
/// what `FIGI_PACE_MS` spaces — the global `throttle()` paces for Yahoo (10 req/s default) and would
/// blow through this one an order of magnitude over.
const FIGI_BATCH: usize = 10;
const FIGI_PACE_MS: u64 = 2500;

/// Bloomberg exchange code for Xetra. The ONLY venue this swap accepts — see `figi_eu_symbol`.
const XETRA_EXCH_CODE: &str = "GY";

/// (EU listing) The Xetra common-stock line inside one OpenFIGI share-class response, as a Yahoo
/// symbol. Pure, so the venue rule is testable without a network: `GY` + `Common Stock` -> `<t>.DE`.
///
/// `securityType` is checked because the same share class carries certificates, depositary lines and
/// structured products on other rows, and substituting one of those would swap the security itself
/// rather than its venue — the exact failure that killed the Yahoo-search route, where `?q=AAPL`
/// offered Thai DRs and an Argentine CEDEAR as if they were Apple.
///
/// ponytail: Xetra ONLY, no venue ladder. All ten probed S&P names carried `GY`, `LN` returns
/// non-EUR depositary lines (`0RIH`, `0QYP`), and a preference order over `NA`/`FP`/`IM`/`SW` is
/// guesswork until a real run reports a Xetra miss. Add fallbacks when that count is a number.
fn figi_eu_symbol(data: &Value) -> Option<String> {
    let row = data.as_array()?.iter().find(|r| {
        r.get("exchCode").and_then(Value::as_str) == Some(XETRA_EXCH_CODE)
            && r.get("securityType").and_then(Value::as_str) == Some("Common Stock")
    })?;
    let ticker = row.get("ticker").and_then(Value::as_str)?;
    (!ticker.is_empty()).then(|| format!("{ticker}.DE"))
}

/// (EU listing) Is this long-chart payload a usable EUR series? The swap is confirmed by the fetch the
/// pipeline was going to make anyway (`chart_json_long` is disk-cached for 7 days), so a thin or
/// wrong-currency venue excludes ITSELF here instead of needing a hand-maintained blacklist.
fn eu_chart_ok(raw: &RawValue, min_bars: usize) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(raw.get()) else {
        return false;
    };
    let eur = v.pointer("/chart/result/0/meta/currency").and_then(Value::as_str) == Some("EUR");
    let bars = v.pointer("/chart/result/0/timestamp").and_then(Value::as_array).map_or(0, Vec::len);
    eur && bars >= min_bars
}

/// (EU listing) Shortest EU record worth swapping to. 60 monthly bars = 5 years, which is
/// `growth_min_leg_years`' shipped floor — below it the swapped name could not be ranked anyway, so
/// keeping its US row is strictly better than trading a scoreable series for an unscoreable one.
const EU_MIN_BARS: usize = 60;

/// One OpenFIGI POST: `jobs` in, one `data` array (or None) out per job, order preserved.
async fn figi_batch(client: &Client, url: &str, jobs: Vec<Value>) -> Vec<Option<Value>> {
    let n = jobs.len();
    let Some(v) = post_json(client, url, &Value::Array(jobs)).await else {
        return vec![None; n]; // transport error -> every job in this batch simply stays unresolved
    };
    let arr = v.as_array().cloned().unwrap_or_default();
    (0..n).map(|i| arr.get(i).and_then(|j| j.get("data")).cloned()).collect()
}

/// (EU listing) Resolve each US symbol to its Xetra twin, two OpenFIGI hops and one chart check:
/// `TICKER`+`exchCode: US` -> `shareClassFIGI` -> every listing of that share class -> the `GY` row.
/// Cached forever (see [`EU_LISTING_CACHE_PATH`]), so the ~4 minutes this costs is paid once.
///
/// Serial and paced rather than fanned out: OpenFIGI allows 25 requests/minute keyless, which is two
/// orders of magnitude below what `fetch_concurrency()` would throw at it.
async fn resolve_eu_listings(client: &Client, urls: &Urls, syms: &[String]) -> HashMap<String, String> {
    let mut cache: HashMap<String, String> = std::fs::read_to_string(crate::config::data_path(EU_LISTING_CACHE_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let todo: Vec<String> = syms.iter().filter(|s| !cache.contains_key(*s)).cloned().collect();
    if todo.is_empty() {
        return cache;
    }
    eprintln!("fetch: resolving EU listings for {} new symbols (~{}s, cached forever after)", todo.len(), todo.len().div_ceil(FIGI_BATCH) * 2 * FIGI_PACE_MS as usize / 1000);
    let mut share_figis: Vec<(String, String)> = Vec::new();
    // Pace BETWEEN requests, never before the first one of a hop: the leading sleep bought nothing and
    // cost 2.5s on every run, including the single-batch case where no pacing is needed at all.
    //
    // The delay is CARRIED IN A VARIABLE rather than branched on the index, and that is a mutation
    // gate decision, not a style one (round 77). `if i > 0` survives `==`, `<` and `>=` because all
    // three change only WHEN the sleeps happen, never what this function returns or writes — nothing
    // short of a wall-clock assertion kills them, and `ci.yml` has no allowlist for a survivor, so one
    // unkillable mutant is a permanently red gate. Hoisting the comparison into a testable helper does
    // NOT fix it either: every usize comparison against zero has an equivalent partner (`i > 0` is
    // `i != 0`, `i == 0` is `i <= 0`), so a survivor follows the comparison wherever it is moved.
    // Deleting it is the only fix. `pace` starts at zero and is FIGI_PACE_MS from the second chunk on.
    let mut pace = 0;
    for chunk in todo.chunks(FIGI_BATCH) {
        tokio::time::sleep(StdDuration::from_millis(pace)).await;
        pace = FIGI_PACE_MS;
        let jobs = chunk
            .iter()
            .map(|t| serde_json::json!({"idType": "TICKER", "idValue": t, "exchCode": "US"}))
            .collect();
        for (sym, data) in chunk.iter().zip(figi_batch(client, &urls.openfigi_mapping, jobs).await) {
            match data.as_ref().and_then(|d| d.get(0)).and_then(|r| r.get("shareClassFIGI")).and_then(Value::as_str) {
                Some(f) => share_figis.push((sym.clone(), f.to_string())),
                None => {
                    cache.insert(sym.clone(), String::new()); // no US share class -> no twin to find
                }
            }
        }
    }
    let mut candidates: Vec<(String, String)> = Vec::new();
    for chunk in share_figis.chunks(FIGI_BATCH) {
        // hop 2 always follows hop 1's last request, so this one paces from its very first batch —
        // hence a leading sleep here where hop 1 skips its first. ONE sleep per chunk, not two: a
        // second at the end of the body spaced hop-2's requests 5s apart against a 2.4s rate limit,
        // which is ~2 minutes of pure waiting on a cold ~52-chunk resolve.
        tokio::time::sleep(StdDuration::from_millis(FIGI_PACE_MS)).await;
        let jobs = chunk
            .iter()
            .map(|(_, f)| serde_json::json!({"idType": "ID_BB_GLOBAL_SHARE_CLASS_LEVEL", "idValue": f}))
            .collect();
        for ((sym, _), data) in chunk.iter().zip(figi_batch(client, &urls.openfigi_mapping, jobs).await) {
            match data.as_ref().and_then(figi_eu_symbol) {
                Some(eu) => candidates.push((sym.clone(), eu)),
                None => {
                    cache.insert(sym.clone(), String::new()); // resolves, but lists nowhere on Xetra
                }
            }
        }
    }
    // Chart check LAST, and through the same disk-cached call the quote path uses — an accepted twin
    // costs nothing extra because this warms the very entry that fetch is about to want.
    for (sym, eu) in candidates {
        let ok = matches!(chart_json_long(client, urls, &eu).await, Some(raw) if eu_chart_ok(&raw, EU_MIN_BARS));
        cache.insert(sym, if ok { eu } else { String::new() });
    }
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(crate::config::data_path(EU_LISTING_CACHE_PATH), json);
    }
    cache
}

/// (EU listing) Rewrite one constituent pond onto its EU twins, carrying each row's GICS sector across
/// the rename. Pure and separate from the fetch so the substitution rule is testable offline.
///
/// The sector MUST travel: `sector_of` is keyed by symbol, the stocks table filters on it, and a
/// swapped row that lost its sector is dropped by that filter — the single most likely way to ship
/// this looking like "the EU swap emptied my screen".
fn swap_to_eu(pond: Vec<(String, String)>, eu: &HashMap<String, String>) -> Vec<(String, String)> {
    pond.into_iter()
        .map(|(sym, sector)| match eu.get(&sym) {
            // empty value = resolved to "no usable twin"; absent key = never asked. Both keep the US row.
            Some(t) if !t.is_empty() => (t.clone(), sector),
            _ => (sym, sector),
        })
        .collect()
}

/// (EU listing) How many of `syms` actually got swapped. Extracted from the `eprintln!` that reports
/// it because a number computed inside a printer can never be graded, and this particular number is
/// the one the knob's whole rollout depends on: the plan is to read the Xetra hit rate off stderr and
/// decide from it, so a tally that silently inverted would send that decision the wrong way.
fn eu_swap_tally(syms: &[String], eu: &HashMap<String, String>) -> usize {
    syms.iter().filter(|s| eu.get(*s).is_some_and(|t| !t.is_empty())).count()
}

/// (EU listing) The pinned tickers that ALSO resolved to a Xetra twin, as `US -> EU` strings ready to
/// print. These are exactly the names where the knob looks broken but is not.
///
/// `screen` re-appends the watchlist VERBATIM after `fetch_universe` has already swapped the pond, so
/// both legs of such a name reach `ranked`. They share one Yahoo name (`NVDA` and `NVD.DE` are both
/// "NVIDIA Corporation"), which is the key the dual-class collapse dedups on — and that collapse
/// exempts pinned tickers, so the US leg survives and the EU one is deleted with no output anywhere.
/// Each rule is deliberate and documented at its own site; the interaction between them was not, and
/// it was invisible until this line.
///
/// Empty EU value = "resolved, no usable twin" — the same convention `eu_swap_tally` reads, and never
/// a shadow. Absent key = never asked (a pinned ETF or crypto), also never a shadow.
pub(crate) fn eu_shadowed_pins(pinned: &[String], eu: &HashMap<String, String>) -> Vec<String> {
    pinned
        .iter()
        .filter_map(|t| eu.get(t).filter(|e| !e.is_empty()).map(|e| format!("{t} -> {e}")))
        .collect()
}

/// (EU listing) Invert the resolver's `US -> EU` map into `EU -> US`, so [`sec_cik`] can find the filer
/// behind a swapped symbol. Both sides upper-cased because `sec_cik` looks up on an upper-cased ticker.
///
/// Empty value = "resolved, no usable twin" and is DROPPED, not inverted to a `"" -> US` entry that
/// would then match every ticker whose upper-case form is empty. Same convention `eu_swap_tally` and
/// `eu_shadowed_pins` read; pure and separate from the cache read so the rule is testable offline.
pub(crate) fn invert_eu(eu: &HashMap<String, String>) -> HashMap<String, String> {
    eu.iter().filter(|(_, e)| !e.is_empty()).map(|(u, e)| (e.to_uppercase(), u.to_uppercase())).collect()
}

pub async fn fetch_universe(
    client: &Client,
    urls: &Urls,
    cap: usize,
    prefer_eur: bool,
    prefer_eu_listing: bool,
    sectors: &[String],
) -> (Vec<String>, std::collections::HashSet<String>, std::collections::HashMap<String, String>) {
    let cg_url = urls.coingecko_markets.replace("{n}", &cap.to_string());
    // Euronext + SIX ETF ISINs first (a few cheap requests) — deduped against each other here,
    // against BF's list inside fetch_xetra_etfs, then merged into the same ISIN->Yahoo bridge.
    // Each list is backed by its last-good copy on disk: a transient venue outage (Euronext 503'd
    // a whole run) must not silently shrink the universe — membership stays stable, the list
    // refreshes whenever the venue answers again.
    let (euronext_isins, six_isins, regulatory_isins) = tokio::join!(
        fetch_euronext_etf_isins(client, urls),
        fetch_six_etf_isins(client, urls),
        fetch_regulatory_etf_isins(client, urls),
    );
    let mut store: HashMap<String, Vec<String>> = std::fs::read_to_string(crate::config::data_path(VENUE_ISINS_PATH))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut dirty = false;
    let mut lists = [("euronext", euronext_isins), ("six", six_isins)];
    for (venue, list) in &mut lists {
        if list.is_empty() {
            *list = store.get(*venue).cloned().unwrap_or_default();
            if !list.is_empty() {
                eprintln!("fetch: {venue} venue list unavailable — using last-good copy ({} ISINs)", list.len());
            }
        } else if store.get(*venue) != Some(list) {
            store.insert(venue.to_string(), list.clone());
            dirty = true;
        }
    }
    if dirty {
        if let Ok(json) = serde_json::to_string(&store) {
            let _ = std::fs::write(crate::config::data_path(VENUE_ISINS_PATH), json);
        }
    }
    let [(_, euronext_isins), (_, six_isins)] = lists;
    let mut extra_isins = euronext_isins;
    extra_isins.extend(six_isins);
    extra_isins.sort();
    extra_isins.dedup();
    // regulatory ISINs not on any venue list are SPECULATIVE: nobody vouches they trade anywhere
    // Yahoo covers, so their definitive misses get TTL-negative-cached inside fetch_xetra_etfs
    // (venue-list misses keep retrying every run).
    let extra_set: std::collections::HashSet<&String> = extra_isins.iter().collect();
    let speculative_isins: Vec<String> =
        regulatory_isins.into_iter().filter(|i| !extra_set.contains(i)).collect();
    let (cg, etfs, lisbon) = tokio::join!(
        get_json(client, &cg_url),
        fetch_xetra_etfs(client, urls, cap, extra_isins, speculative_isins),
        fetch_euronext_lisbon(client, urls),
    );
    // (Item 18) equity ponds = S&P 500 + any extra same-format constituent CSVs from config. Sequential
    // (1–3 URLs, negligible vs the universe fan-out); a failed/empty CSV just drops its pond, never crashes.
    // (Item 32) each pond is a CSV (Symbol first, sector col 3) OR a Wikipedia constituents page
    // (the only maintained source for e.g. the S&P MidCap 400) — the URL's host picks the parser.
    let mut ponds = constituent_ponds(client, urls, sectors).await;
    // (EU listing) Swap each constituent onto its Xetra twin BEFORE any chart is fetched, so every
    // downstream reader — score, gates, tables, order glue — sees a genuine EU quote rather than a US
    // series wearing a EU ticker. Capped first: the resolver then only ever prices names that actually
    // enter the universe. Off by default; `prefer_eu_listing`'s doc carries what turning it on costs.
    if prefer_eu_listing {
        for pond in &mut ponds {
            pond.truncate(cap);
        }
        let syms: Vec<String> = ponds.iter().flatten().map(|(s, _)| s.clone()).collect();
        let eu = resolve_eu_listings(client, urls, &syms).await;
        let swapped = eu_swap_tally(&syms, &eu);
        eprintln!("fetch: EU listing swap — {swapped} of {} constituents on Xetra, {} kept on their US line", syms.len(), syms.len() - swapped);
        ponds = ponds.into_iter().map(|p| swap_to_eu(p, &eu)).collect();
    }
    let crypto_cur = if prefer_eur { "EUR" } else { "USD" };
    let mut out: Vec<String> = Vec::new();
    // crypto: CoinGecko market-cap-ranked array -> SYMBOL-<EUR|USD> (Yahoo crypto form).
    // Every other universe leg warns when it degrades — this one must too, or the crypto
    // tables just vanish with no clue whether that's the market or a dead feed.
    match cg.as_ref().and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => {
            out.extend(arr.iter().take(cap).filter_map(|c| {
                c.get("symbol").and_then(|s| s.as_str()).map(|s| format!("{}-{crypto_cur}", s.to_uppercase()))
            }));
        }
        _ => eprintln!("fetch: CoinGecko returned nothing — crypto absent from the screen"),
    }
    // stocks: each constituent CSV -> Yahoo symbol, kept only if the row's GICS sector passes `sectors`
    // (empty = all). Filtering HERE means a sector-restricted screen never even fetches the other
    // sectors' companies. `.take(cap)` AFTER the filter so cap counts matching names per pond, not raw rows.
    let mut sector_of = std::collections::HashMap::new();
    for pond in ponds {
        for (sym, sector) in pond.into_iter().take(cap) {
            out.push(sym.clone());
            // first pond wins: a dual-member (e.g. AAPL in S&P 500 AND Nasdaq-100) keeps its real
            // GICS sector — the sector-less pond's "other" must not overwrite it
            sector_of.entry(sym).or_insert(sector);
        }
    }
    // Euronext Lisbon equities (Yahoo `.LS`). note: NOT sector-filtered — the set is ~33 names,
    // so a sector-restricted screen could leak a few Lisbon stocks; tighten only if that ever bites
    // (the payload doesn't carry GICS anyway).
    out.extend(lisbon);
    let etf_set: std::collections::HashSet<String> = etfs.iter().cloned().collect();
    out.extend(etfs); // EU-buyable UCITS ETFs (Yahoo symbols)
    out.sort();
    out.dedup();
    (out, etf_set, sector_of)
}

/// Network-module asserts (no live calls): the pure, breakable bit of the Börse Frankfurt signer is
/// the MD5 — pin it to known answers so a bad refactor is caught offline. (The concatenation order is
/// verified against the live server, not here.)
#[cfg(test)]
mod tests {
    use super::*;

    /// (#76) The EU-listing swap's three pure halves, on the exact payload shapes the live services
    /// return. The knob that drives them defaults off and is therefore unkillable by the mutation gate,
    /// so these test the functions directly rather than through `fetch_universe`.
    ///
    /// Hop 2 of OpenFIGI hands back EVERY listing of a share class — 234 rows for Alphabet, spanning
    /// dozens of venues and several security types. Picking the wrong row is the whole risk: a German
    /// certificate or a London depositary line is a different instrument with a different currency, and
    /// swapping onto one would quietly price a derivative as if it were the stock.
    #[test]
    fn figi_eu_symbol_takes_only_the_xetra_common_line() {
        use serde_json::json;
        // Shape and ordering as served: the US line first, the Xetra one buried mid-list.
        let alphabet = json!([
            {"ticker": "GOOGL", "exchCode": "US", "securityType": "Common Stock"},
            {"ticker": "ABEA", "exchCode": "GF", "securityType": "Common Stock"},
            {"ticker": "ABEA", "exchCode": "GY", "securityType": "Common Stock"},
            {"ticker": "0R2U", "exchCode": "LN", "securityType": "Common Stock"},
        ]);
        assert_eq!(figi_eu_symbol(&alphabet).as_deref(), Some("ABEA.DE"));

        // Right venue, wrong instrument — a Xetra certificate must never stand in for the share.
        let certificate = json!([{"ticker": "XYZ", "exchCode": "GY", "securityType": "Certificate"}]);
        assert_eq!(figi_eu_symbol(&certificate), None);

        // Listed in Europe, but not on Xetra: this swap knows one venue, so this is a miss, not a guess.
        let paris_only = json!([{"ticker": "MC", "exchCode": "FP", "securityType": "Common Stock"}]);
        assert_eq!(figi_eu_symbol(&paris_only), None);

        // Degenerate payloads: an empty ticker would build a bare ".DE", and hop 2 can answer with
        // nothing at all.
        assert_eq!(figi_eu_symbol(&json!([{"ticker": "", "exchCode": "GY", "securityType": "Common Stock"}])), None);
        assert_eq!(figi_eu_symbol(&json!([])), None);
        assert_eq!(figi_eu_symbol(&json!({"error": "No identifier found."})), None);
    }

    /// (#76) The chart check is what makes a bad venue self-excluding: OpenFIGI can name a Xetra line
    /// that Yahoo does not quote, quotes in the wrong currency, or has barely started. A USD series
    /// wearing a `.DE` ticker is the failure this exists to stop — every threshold in ci-settings was
    /// calibrated on USD, so the swap is only honest when the series really changed currency.
    #[test]
    fn eu_chart_ok_demands_eur_and_history() {
        let chart = |cur: &str, bars: usize| {
            let ts: Vec<String> = (0..bars).map(|i| (1_000_000 + i * 86_400).to_string()).collect();
            RawValue::from_string(format!(
                r#"{{"chart":{{"result":[{{"meta":{{"currency":"{cur}"}},"timestamp":[{}]}}]}}}}"#,
                ts.join(",")
            ))
            .unwrap()
        };
        assert!(eu_chart_ok(&chart("EUR", 60), 60)); // exactly at the floor counts
        assert!(!eu_chart_ok(&chart("EUR", 59), 60)); // one bar short does not
        assert!(!eu_chart_ok(&chart("USD", 300), 60)); // the trap: right ticker, wrong denomination
        assert!(!eu_chart_ok(&chart("GBp", 300), 60)); // London's pence lines are not EUR either
        // Yahoo's own "no such symbol" answer, and a body that is not JSON at all.
        assert!(!eu_chart_ok(&RawValue::from_string(r#"{"chart":{"result":null,"error":{"code":"Not Found"}}}"#.into()).unwrap(), 60));
        assert!(!eu_chart_ok(&RawValue::from_string("null".into()).unwrap(), 60));
    }

    /// (#76) Substitution keeps the pond the same size and the sector attached. Losing the sector is the
    /// single most likely way to ship this broken — `sector_map` is keyed by the CSV symbol, so a
    /// rename that drops it empties the stocks table through the sector filter rather than erroring.
    #[test]
    fn swap_to_eu_renames_without_losing_rows_or_sectors() {
        let pond = vec![
            ("GOOGL".to_string(), "Communication Services".to_string()),
            ("AAPL".to_string(), "Information Technology".to_string()),
            ("BRK-B".to_string(), "Financials".to_string()),
            ("TSLA".to_string(), "Consumer Discretionary".to_string()),
        ];
        let eu: HashMap<String, String> = [
            ("GOOGL", "ABEA.DE"),
            ("AAPL", "APC.DE"),
            ("BRK-B", ""), // resolved, but no usable Xetra twin
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        let out = swap_to_eu(pond.clone(), &eu);
        assert_eq!(out.len(), pond.len(), "the swap never shrinks the pond");
        assert_eq!(out[0], ("ABEA.DE".to_string(), "Communication Services".to_string()));
        assert_eq!(out[1], ("APC.DE".to_string(), "Information Technology".to_string()));
        assert_eq!(out[2], pond[2], "empty value = no twin -> US row untouched");
        assert_eq!(out[3], pond[3], "absent key = never asked -> US row untouched");
        // Empty map is the resolver-fully-failed case: the whole pond must survive unchanged.
        assert_eq!(swap_to_eu(pond.clone(), &HashMap::new()), pond);

        // The stderr tally must agree with what the swap actually did, counting only real twins. An
        // inverted count reads as "497 of 503 on Xetra" on a run that swapped six names, which is the
        // number the decision to turn this knob on is meant to rest on.
        let syms: Vec<String> = pond.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(eu_swap_tally(&syms, &eu), 2, "GOOGL and AAPL swapped; BRK-B and TSLA did not");
        assert_eq!(eu_swap_tally(&syms, &HashMap::new()), 0);
        assert_eq!(eu_swap_tally(&[], &eu), 0);
    }

    /// A pinned US ticker cancels the swap for its own name, silently: `screen` re-appends the
    /// watchlist after the pond was already swapped, both legs then carry the same Yahoo name, and the
    /// dual-class collapse keeps the pinned one. This is the only warning a user gets that the knob is
    /// on and doing nothing for the names they care most about, so it must name the pair to act on —
    /// reporting a bare count would leave them re-deriving which pin did it.
    #[test]
    fn eu_shadowed_pins_names_only_the_pins_that_cancel_their_own_swap() {
        let eu: HashMap<String, String> = [
            ("NVDA", "NVD.DE"),
            ("AMZN", "AMZ.DE"),
            ("BRK-B", ""), // resolved, no usable Xetra twin -> the pin shadows nothing
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        let pinned: Vec<String> =
            ["NVDA", "BRK-B", "VUAA.DE", "BTC-EUR"].iter().map(|s| (*s).to_string()).collect();

        // AMZN resolved but is NOT pinned, so it swapped cleanly and must not be reported; VUAA.DE and
        // BTC-EUR were never asked (absent key), which is also not a shadow.
        assert_eq!(eu_shadowed_pins(&pinned, &eu), ["NVDA -> NVD.DE"]);
        assert!(eu_shadowed_pins(&pinned, &HashMap::new()).is_empty(), "no resolutions, no shadows");
        assert!(eu_shadowed_pins(&[], &eu).is_empty(), "no pins, no shadows");
    }

    /// (fund staleness) The serve/refetch split, which is the entire fix for "Yahoo is down and the
    /// fund columns went blank". `serve` decides what the caller can still publish when the network
    /// gives it nothing; `refetch` decides what it asks for. They used to be one answer, and that is
    /// why a run holding 138 datable rows published none of them.
    #[test]
    fn cache_use_serves_and_refetches_independently() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 16).unwrap();
        let age = |s: &str| cache_age_days(s, today);

        assert_eq!(age("2026-08-16"), Some(0));
        assert_eq!(age("2026-08-13"), Some(3));
        assert_eq!(age("not-a-date"), None, "a corrupt stamp is not a date");

        // fetched today: publish it, don't ask again.
        assert_eq!(cache_use(age("2026-08-16")), (true, false));
        // yesterday: still good to publish, but due a refresh — this is the marked (`°`) window.
        assert_eq!(cache_use(age("2026-08-15")), (true, true));
        // exactly at the cap: still served. The boundary is the whole knob, so it is pinned on both
        // sides rather than left to whichever comparison someone edits next.
        assert_eq!(cache_use(age("2026-08-13")), (true, true));
        // one day past it: refetch, and publish NOTHING — `n/a` beats a number nobody would trust.
        assert_eq!(cache_use(age("2026-08-12")), (false, true));
        assert_eq!(cache_use(age("2026-07-01")), (false, true));

        // absent or corrupt: never served, always refetched. A row that fails to parse must not become
        // an infinitely fresh one.
        assert_eq!(cache_use(None), (false, true));
        assert_eq!(cache_use(age("not-a-date")), (false, true));

        // clock skew / hand-edited cache: a future stamp counts as fresh rather than being refetched
        // forever, which a plain `.abs()`-free `>` would get wrong in the other direction.
        assert_eq!(cache_use(age("2026-08-20")), (true, false));
    }

    /// (TER/AUM) The justETF scrape, on the EXACT markup the live page serves (captured from
    /// IE00B3WJKG14 while probing, whitespace-collapsed). This is now the only working source for these
    /// two facts, so the shapes it must survive — and the ones it must refuse — are pinned here.
    ///
    /// UNITS ARE THE POINT: Börse Frankfurt reports AUM in absolute currency, justETF in millions. A
    /// silent unit mismatch would put every justETF-sourced fund a factor of 10^6 below the AUM gate,
    /// i.e. quietly delete them from the ETF table rather than fail loudly.
    #[test]
    fn parse_justetf_facts_reads_ter_and_fund_size() {
        let page = concat!(
            r#"<tr data-testid="etf-basics_row_fund-size"> <td class="vallabel"> Fund size </td>"#,
            r#" <td> <div> EUR 16,988 m <span class="indicator-3"></span> </div> </td> </tr>"#,
            r#" <tr data-testid="etf-basics_row_ter"> <td class="vallabel"> Total expense ratio </td>"#,
            r#" <td> <div class="val" data-testid="tl_etf-basics_value_ter">0.15% p.a.</div> </td> </tr>"#,
        );
        let (ter, aum) = parse_justetf_facts(page);
        assert_eq!(ter, Some(0.15), "percent, not a fraction — same units as BF's TER");
        assert_eq!(aum, Some(1.6988e10), "millions -> absolute, same units as bf_row_aum");

        // billions suffix, and a fund with no thousands separator
        let bn = page.replace("EUR 16,988 m", "USD 3.5 bn");
        assert_eq!(parse_justetf_facts(&bn).1, Some(3.5e9));

        // an UNKNOWN magnitude must yield None, never a number wrong by 10^6
        let odd = page.replace("EUR 16,988 m", "EUR 42 zz");
        assert_eq!(parse_justetf_facts(&odd).1, None, "unknown suffix -> no guess");

        // justETF prints a literal 0 for "unknown" — the same convention BF and Yahoo use
        let zero = page.replace(">0.15% p.a.", ">0.00% p.a.");
        assert_eq!(parse_justetf_facts(&zero).0, None, "0% is unknown, not a free fund");

        // any shape change -> both None, cells stay n/a, nothing is invented
        assert_eq!(parse_justetf_facts("<html>redesigned</html>"), (None, None));
        assert_eq!(parse_justetf_facts(""), (None, None));
    }

    /// (round 78) The crumb-vs-error rule. Yahoo answers a rate limit with the bare prose
    /// `Too Many Requests` — no braces — which the previous "no `{`" test ACCEPTED and then cached in
    /// `YQ_AUTH` for the rest of the process, so one 429 became a whole run of requests that could only
    /// earn `401 Invalid Crumb`. Found against the live endpoint.
    #[test]
    fn is_crumb_rejects_yahoos_error_bodies() {
        assert!(is_crumb("0EBQ3sZ/xyz"), "a real crumb is a short opaque token");
        assert!(is_crumb("abc.def"), "punctuation is fine — only whitespace and JSON are not");

        assert!(!is_crumb("Too Many Requests"), "THE 429 BODY — the whole reason this fn exists");
        assert!(!is_crumb("{\"finance\":{\"error\":\"Invalid Cookie\"}}"), "JSON error body");
        assert!(!is_crumb(""), "empty");
        assert!(!is_crumb("   \n"), "whitespace only");

        // A one-word error body (`Unauthorized`) is shape-identical to a crumb and this fn CANNOT
        // reject it — which is why the caller checks the HTTP status first and never reaches here on a
        // non-2xx. Asserted so the split of responsibility is not quietly re-litigated.
        assert!(is_crumb("Unauthorized"), "shape alone cannot catch this one — the status check does");
    }

    /// The EU->US alias `sec_cik` looks a swapped ticker up through. Three properties, and the run's
    /// whole fundamentals block rides on each: the direction is EU->US (inverted, not copied), the
    /// "resolved, no usable twin" empty value is DROPPED rather than becoming a `"" -> US` entry that
    /// would alias every ticker, and both sides are upper-cased because the caller keys on an
    /// upper-cased ticker (a lower-case cache entry would otherwise silently never match).
    #[test]
    fn invert_eu_maps_the_xetra_symbol_back_to_the_filer() {
        let eu: HashMap<String, String> = [
            ("NVDA", "NVD.DE"),
            ("MCD", "MDO.DE"),
            ("BRK-B", ""), // resolved, no usable twin -> contributes no alias
            ("coin", "1qz.de"), // lower-case on both sides -> must still match an upper-cased lookup
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();

        let inv = invert_eu(&eu);
        assert_eq!(inv.get("NVD.DE").map(String::as_str), Some("NVDA"));
        assert_eq!(inv.get("MDO.DE").map(String::as_str), Some("MCD"));
        assert_eq!(inv.get("1QZ.DE").map(String::as_str), Some("COIN"), "both sides upper-cased");
        assert!(!inv.contains_key(""), "the empty twin must not become a catch-all alias");
        assert_eq!(inv.len(), 3, "one entry per resolved pair, none for the empty one");
        // direction: a US ticker is a VALUE here, never a key — aliasing US->US would be a no-op that
        // silently reintroduced the bug this exists to fix.
        assert!(!inv.contains_key("NVDA"));
        assert!(invert_eu(&HashMap::new()).is_empty());
    }

    /// (#45) The MVRV staleness guard, on the exact shapes the live endpoint returns. `btc` is a
    /// covered asset (row = today), `bnb` and `dot` are dropped ones whose LAST row is years old —
    /// and `limit_per_asset=1` hands those back looking just as authoritative as btc's. Dropping
    /// them is the whole job: absent from the map = passes `crypto_max_mvrv` free, same as no data,
    /// whereas keeping them would gate BNB (rank 1 of the crypto table) on a 2019 valuation.
    #[test]
    fn mvrv_rows_drops_assets_coinmetrics_stopped_covering() {
        use serde_json::json;
        let body = json!({"data": [
            {"asset": "btc", "time": "2026-08-01T00:00:00.000000000Z", "CapMVRVCur": "1.188728646019947622"},
            {"asset": "eth", "time": "2026-08-01T00:00:00.000000000Z", "CapMVRVCur": 0.898_481_8},
            {"asset": "bnb", "time": "2019-04-22T00:00:00.000000000Z", "CapMVRVCur": "1.88"},
            {"asset": "dot", "time": "2022-06-03T00:00:00.000000000Z", "CapMVRVCur": "0.52"},
            {"asset": "junk", "time": "2026-08-01T00:00:00.000000000Z", "CapMVRVCur": "0"},
            {"asset": "nodate", "CapMVRVCur": "1.10"},
        ]});
        let rows = mvrv_rows(&body, "2026-07-22");
        assert_eq!(rows.len(), 2, "only the two fresh, positive rows survive: {rows:?}");
        assert!((rows["btc"] - 1.188_728_646).abs() < 1e-6); // string payload parsed
        assert!((rows["eth"] - 0.898_481_8).abs() < 1e-9); // bare number accepted too
        for gone in ["bnb", "dot", "junk", "nodate"] {
            assert!(!rows.contains_key(gone), "{gone} must not reach the gate");
        }
        // and the guard is a real cut, not an artifact of the fixture: with a cutoff old enough to
        // admit them, the very same body yields the stale values the live API really serves.
        let loose = mvrv_rows(&body, "2000-01-01");
        assert!((loose["bnb"] - 1.88).abs() < 1e-9);
        assert!((loose["dot"] - 0.52).abs() < 1e-9);
    }

    /// Signing + concurrency + throttle asserts (no live calls). White-box via `use super::*`.
    #[test]
    fn bf_ter_parse() {
    use serde_json::json;
    // BF sends fractions: 0.0007 = VUAA's known 0.07%. bf_row_ter converts to percent.
    let near = |v: Option<f64>, want: f64| (v.unwrap() - want).abs() < 1e-9;
    assert!(near(bf_row_ter(&json!({"isin": "X", "ter": 0.0007})), 0.07)); // primary key
    assert!(near(bf_row_ter(&json!({"totalExpenseRatio": 0.002})), 0.2)); // fallback key
    assert_eq!(bf_row_ter(&json!({"name": "fund"})), None); // no fee field -> None
    assert_eq!(bf_row_ter(&json!({"ter": 0.12})), None); // 12% after ×100 -> out of sane range, rejected
    assert_eq!(bf_row_ter(&json!({"ter": 0.0})), None); // zero -> None, never a fake 0%
    // real BF shape: TER nested one level under overview, not top-level
    assert!(near(bf_row_ter(&json!({"isin": "X", "overview": {"totalExpenseRatio": 0.0007}})), 0.07));
    // string value with EU decimal comma
    assert!(near(bf_row_ter(&json!({"overview": {"ongoingCharges": "0,002"}})), 0.2));
    // string value carrying a percent suffix (BF sometimes ships "0,20%") -> stripped before parse
    assert!(near(bf_row_ter(&json!({"overview": {"ongoingCharges": "0,002%"}})), 0.2));
    // first known key wins; a non-numeric junk string on that key -> None (never a fake fee)
    assert_eq!(bf_row_ter(&json!({"ter": "n/a"})), None);
    }

    /// `bf_row_aum`: fund size from `overview.assetsUnderManagement`; absent / top-level-only /
    /// non-positive / non-finite -> None (never a fake 0-size fund).
    #[test]
    fn bf_row_aum_parse() {
        use serde_json::json;
        assert_eq!(bf_row_aum(&json!({"overview": {"assetsUnderManagement": 1.7e9}})), Some(1.7e9));
        assert_eq!(bf_row_aum(&json!({"overview": {}})), None); // field absent
        assert_eq!(bf_row_aum(&json!({"assetsUnderManagement": 1.0e8})), None); // top-level, not under overview
        assert_eq!(bf_row_aum(&json!({"overview": {"assetsUnderManagement": 0.0}})), None); // zero -> None
        assert_eq!(bf_row_aum(&json!({"overview": {"assetsUnderManagement": -5.0}})), None); // negative -> None
        assert_eq!(bf_row_aum(&json!({})), None);
    }

    /// `between`/`between_all` string scanners: an unmatched close tag stops cleanly (None / short list),
    /// never panics or slices past the end. Pins the Form-4 XML scan's only tag-matching primitive.
    #[test]
    fn between_helpers() {
        assert_eq!(between("<a>x</a>", "<a>", "</a>"), Some("x"));
        assert_eq!(between("<a>x", "<a>", "</a>"), None); // open but no close
        assert_eq!(between("x", "<a>", "</a>"), None); // no open
        assert_eq!(between_all("<a>1</a><a>2</a>", "<a>", "</a>"), vec!["1", "2"]);
        assert_eq!(between_all("<a>1</a><a>2", "<a>", "</a>"), vec!["1"]); // trailing unmatched open dropped
        assert!(between_all("<a>1", "<a>", "</a>").is_empty()); // open, no close -> break arm, empty
    }

    /// The venue and regulatory ISINs merge in behind BF's top-`cap` cut, deduped against every ISIN
    /// BF returned — including the ones the cap threw away.
    ///
    /// `IE00C` is the case that carries this test: BF returned it, the cap cut it, and it arrives
    /// again on the speculative list. Deduping against `top` instead of `rows` would re-admit it as a
    /// factless speculative fund, and the negative cache would then be free to brand a fund BF itself
    /// listed as "not on Yahoo" for 30 days.
    #[test]
    fn merging_venue_isins_dedupes_against_every_bf_row_not_just_the_kept_ones() {
        let bf = |isin: &str, ter: Option<f64>| (isin.to_string(), ter, None, BfMeta::default());
        let rows = vec![bf("IE00A", Some(0.07)), bf("IE00B", Some(0.12)), bf("IE00C", Some(0.20))];
        let (top, extra_n, spec_set) = merge_isin_lists(
            rows,
            vec!["IE00B".into(), "LU00X".into()], // IE00B is BF's own and must not come back
            vec!["IE00C".into(), "FR00Y".into()], // IE00C is BF's own too, despite being cut by the cap
            2,
        );
        let isins: Vec<&str> = top.iter().map(|(i, ..)| i.as_str()).collect();
        assert_eq!(isins, ["IE00A", "IE00B", "LU00X", "FR00Y"], "BF's top-2 first, then extras, then speculative");
        assert_eq!(extra_n, 1, "one venue-list extra survived the dedupe — the diagnostic prints this");
        assert_eq!(spec_set.len(), 1);
        assert!(spec_set.contains("FR00Y") && !spec_set.contains("IE00C"));
        assert_eq!(top[0].1, Some(0.07), "a BF row keeps its TER through the merge");
        assert_eq!(top[2].1, None, "a venue-list extra carries no BF facts");
        assert!(!carries_fund_facts(&top[2].3), "…and no meta either");
        // and the other direction, which the line above cannot reach: a lone domicile IS a fact. A
        // `carries_fund_facts` stuck at false stores nothing in the symbol-keyed meta map and drops
        // every USE/REPL/DOM cell in the ETF tables, while the run diagnostic still reports the rows
        // as parsed. `dom` alone is the weakest case, so it is the one worth pinning.
        assert!(carries_fund_facts(&BfMeta { dom: Some("IE".into()), ..BfMeta::default() }));
        assert!(carries_fund_facts(&BfMeta { use_of: Some("Acc"), ..BfMeta::default() }));
    }

    /// The 30-day negative cache, at the exact day it expires and on both sides of it.
    #[test]
    fn the_negative_cache_expires_on_the_thirtieth_day() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 9).expect("today");
        let at = |days: i64| (today - chrono::Duration::days(days)).to_string();
        assert!(neg_cache_blocks(true, Some(&at(29)), today), "29 days old — still dead, do not re-search");
        assert!(!neg_cache_blocks(true, Some(&at(30)), today), "30 days is the TTL itself: expired, search again");
        assert!(!neg_cache_blocks(true, Some(&at(31)), today));
        assert!(!neg_cache_blocks(false, Some(&at(1)), today), "venue-listed funds are never negative-cached");
        assert!(!neg_cache_blocks(true, None, today), "never seen before");
        // an unparseable entry must fail OPEN — one wasted search beats a permanent false negative
        assert!(!neg_cache_blocks(true, Some(&"not-a-date".to_string()), today));
    }

    /// Yahoo's answer is triaged three ways, and the two failure arms differ in consequence: a
    /// Stuttgart-only symbol is dropped for this run, a definitive miss is remembered for 30 days.
    #[test]
    fn only_a_search_shaped_payload_may_brand_an_isin_dead() {
        assert!(usable_symbol("XDWD.DE"));
        assert!(usable_symbol("SWDA.L"));
        assert!(!usable_symbol("IE00B4L5Y983.SG"), "Stuttgart is chart-less — a guaranteed dead fetch");
        assert!(usable_symbol("MSG"), "the suffix is a venue, not a substring");

        let quotes = serde_json::json!({"quotes": []});
        let throttled = serde_json::json!({"finance": {"error": {"code": "Too Many Requests"}}});
        assert!(definitive_miss(true, &quotes), "search answered, ISIN genuinely absent");
        assert!(!definitive_miss(true, &throttled), "a rate-limit must never cost a fund 30 days");
        assert!(!definitive_miss(true, &serde_json::json!({"quotes": "unexpected"})), "shape must be an array");
        assert!(!definitive_miss(false, &quotes), "only regulatory-sourced ISINs are cached negative");
    }

    /// The "your parser broke" advice fires only when BF actually sent rows AND none yielded a TER.
    #[test]
    fn the_parser_diagnostic_stays_quiet_on_a_healthy_run() {
        assert!(no_ter_parsed(0, "isin,name,keyData.ter"), "rows arrived, not one TER — the key moved");
        assert!(!no_ter_parsed(412, "isin,name,keyData.ter"), "TERs parsed — nothing to fix");
        assert!(!no_ter_parsed(0, ""), "BF sent nothing at all; that is the OTHER diagnostic's job");
    }

    /// Name-keyed TER fallback: unique fund-name prefix hits, ambiguous share-class prefixes and
    /// too-short names never guess.
    #[test]
    fn bf_ter_name_lookup() {
        let _ = BF_TER_NAMES.set(vec![
            ("vaneck semiconductor ucits etf - usd acc".into(), 0.35),
            ("amundi stoxx europe 600 banks ucits etf acc".into(), 0.30),
            ("amundi stoxx europe 600 banks ucits etf dist".into(), 0.30),
            ("amundi s&p 500 swap ucits etf eur acc".into(), 0.15),
            ("amundi s&p 500 swap ucits etf eur hedged acc".into(), 0.28),
        ]);
        // Yahoo umbrella prefix ("Amundi Index Solutions - …") never prefixes a BF name; fetch_expense
        // retries with the part after " - ", which must hit its share class uniquely (AUM5.DE rescue).
        let yahoo = "Amundi Index Solutions - Amundi S&P 500 Swap UCITS ETF EUR Acc";
        assert_eq!(bf_ter_by_name(yahoo), None);
        assert_eq!(bf_ter_by_name(yahoo.split_once(" - ").unwrap().1), Some(0.15));
        assert_eq!(bf_ter_by_name("VanEck Semiconductor UCITS ETF"), Some(0.35)); // unique prefix
        assert_eq!(bf_ter_by_name("Amundi STOXX Europe 600 Banks UCITS ETF"), None); // 2 share classes -> ambiguous
        assert_eq!(bf_ter_by_name("Amundi STOXX Europe 600 Banks UCITS ETF Dist"), Some(0.30)); // exact class
        assert_eq!(bf_ter_by_name("vaneck"), None); // too short
        assert_eq!(bf_ter_by_name("iShares Physical Gold ETC"), None); // not in list
    }

    /// (#45) The four causes behind one `REPL n/a`. The point of the split is that only AmbiguousName
    /// is actionable — BF already holds that row and `bf_by_name`'s uniqueness rule throws it away —
    /// so a bucket collapsing into NotOnBf would hide a fixable bug behind "upstream has no data".
    /// Owns BF_ROW_NAMES/BF_META_NAMES: no other test seeds them, and OnceLock only takes the first set.
    #[test]
    fn bf_meta_miss_buckets() {
        let _ = BF_ROW_NAMES.set(vec![
            "vaneck semiconductor ucits etf - usd acc".into(),
            "amundi stoxx europe 600 banks ucits etf acc".into(), // two share classes under
            "amundi stoxx europe 600 banks ucits etf dist".into(), // ...one prefix -> ambiguous
            "xtrackers msci world ucits etf 1c".into(),
            "gold bullion securities".into(),
        ]);
        let _ = BF_META_NAMES.set(vec![
            (
                "vaneck semiconductor ucits etf - usd acc".into(),
                BfMeta { use_of: Some("Acc"), repl: None, bench: Some("msci world".into()), dom: Some("IE".into()) },
            ),
            // dom-only: BF_META_NAMES keeps this row (its filter is `!= default`, and dom is stamped
            // from the ISIN on EVERY row) while it carries no fund fact at all. Classifying on
            // `!= default` would call this "BF answered" and hide the other three causes — the exact
            // bug the first live run exposed, so this row is the regression pin.
            ("xtrackers msci world ucits etf 1c".into(), BfMeta { dom: Some("LU".into()), ..Default::default() }),
        ]);
        let miss = |name: &str| bf_meta_miss("X.DE", name);
        // BF answered (share class parsed) but shipped no replicationMethod — upstream omission
        assert!(miss("VanEck Semiconductor UCITS ETF") == BfMetaMiss::NoReplField);
        // no BF row prefixes this name at all -> venue/regulatory extra, unfixable
        assert!(miss("XACT Bull 2 ETF") == BfMetaMiss::NotOnBf);
        // 2 BF rows share the prefix -> bf_by_name refuses to guess. THE fixable bucket
        assert!(miss("Amundi STOXX Europe 600 Banks UCITS ETF") == BfMetaMiss::AmbiguousName);
        // exactly one row matches, yet BF_META_NAMES dropped it -> the row parsed to nothing
        assert!(miss("Xtrackers MSCI World UCITS ETF 1C") == BfMetaMiss::EmptyKeyData);
        // under bf_by_name's 10-byte floor: the row IS there, the lookup declines -> same bucket as
        // ambiguity, NOT NotOnBf, which would misreport recoverable data as absent
        assert!(miss("gold") == BfMetaMiss::AmbiguousName);

        let etf = |t: &str, n: &str| {
            let mut q = crate::core::Quote::stub(t, "", "", n);
            q.instrument_type = "ETF".into();
            q
        };
        let mut done = etf("OK.DE", "Whatever UCITS ETF");
        done.replication = Some("Full");
        assert_eq!(bf_meta_miss_report(&[done.clone()]), None, "nothing missing -> silent");
        let report = bf_meta_miss_report(&[done, etf("XB.ST", "XACT Bull 2 ETF")]).unwrap();
        assert!(report.contains("1/2 ETFs"), "counts misses over ETFs, not over the miss list: {report}");
        assert!(report.contains("not on BF") && report.contains("XB.ST"), "names the cause + a sample: {report}");
        // a stock is not an ETF row and must not be tallied
        assert_eq!(bf_meta_miss_report(&[crate::core::Quote::stub("AAPL", "", "", "Apple Inc.")]), None);
    }

    /// (USE/REPL) BF keyData token parse: English translation preferred, originalValue fallback,
    /// unknown vocabulary -> None (a BF wording drift blanks the cell, never mislabels it).
    #[test]
    fn bf_row_meta_tokens() {
        let row = serde_json::json!({"keyData": {
            "useOfProfits": {"originalValue": "Thesaurierend", "translations": {"en": "Accumulating"}},
            "replicationMethod": {"originalValue": "Swap-based"},
            "benchmark": {"originalValue": "S&P 500 Index"}
        }});
        let m = bf_row_meta(&row);
        assert_eq!((m.use_of, m.repl), (Some("Acc"), Some("Swap")));
        assert_eq!(m.bench.as_deref(), Some("s&p 500 index")); // lowercased at capture for `==` twin match
        let dist = serde_json::json!({"keyData": {"useOfProfits": {"translations": {"en": "Distributing"}}}});
        assert_eq!(bf_row_meta(&dist), BfMeta { use_of: Some("Dist"), ..BfMeta::default() });
        let drift = serde_json::json!({"keyData": {"useOfProfits": {"translations": {"en": "Reinvesting"}}}});
        assert_eq!(bf_row_meta(&drift), BfMeta::default()); // unknown wording -> blank, not a guess
        assert_eq!(bf_row_meta(&serde_json::json!({})), BfMeta::default()); // no keyData at all
        // every known replication wording maps; an unknown one blanks (not a guess)
        for (word, want) in [("Full replication", "Full"), ("Optimised", "Opt"), ("Hybrid", "Hybr"), ("Sample", "Samp")] {
            let r = serde_json::json!({"keyData": {"replicationMethod": {"originalValue": word}}});
            assert_eq!(bf_row_meta(&r).repl, Some(want));
        }
        let repl_drift = serde_json::json!({"keyData": {"replicationMethod": {"originalValue": "Synthetic-ish"}}});
        assert_eq!(bf_row_meta(&repl_drift).repl, None);
    }

    /// (round 49) USE-from-name fallback: word token only ("Vaccine" must not read as "acc"),
    /// Acc checked before Dist, unknown wording -> None.
    #[test]
    fn use_from_name_tokens() {
        assert_eq!(use_from_name("Vanguard S&P 500 UCITS ETF USD (Acc)"), Some("Acc"));
        assert_eq!(use_from_name("iShares Core MSCI World UCITS ETF USD Accumulating"), Some("Acc"));
        assert_eq!(use_from_name("Vanguard FTSE All-World UCITS ETF Dist"), Some("Dist"));
        assert_eq!(use_from_name("SPDR S&P Global Dividend Aristocrats Distributing"), Some("Dist"));
        assert_eq!(use_from_name("VanEck Vaccine and Genomics UCITS ETF"), None); // substring trap
        assert_eq!(use_from_name("Xtrackers MSCI World UCITS ETF 1C"), None); // no token -> honest n/a
    }

    /// (round 51) monthly-fetch skip decision: within the 30d TTL -> skip, expired/absent -> fetch.
    #[test]
    fn long_skip_ttl() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        let fresh = NaiveDate::from_ymd_opt(2026, 6, 20).unwrap(); // 20d old
        let stale = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(); // 70d old
        assert!(long_skip_fresh(Some(&fresh), today));
        assert!(long_skip_fresh(Some(&today), today)); // recorded today -> skip
        assert!(!long_skip_fresh(Some(&stale), today)); // TTL expired -> retry (heals relistings)
        assert!(!long_skip_fresh(None, today)); // never recorded -> fetch
    }

    /// (round 53) monthly-payload cache decision: within the 7d TTL -> serve from disk, else refetch.
    #[test]
    fn long_cache_ttl() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        let fresh = NaiveDate::from_ymd_opt(2026, 7, 5).unwrap(); // 5d old
        let stale = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(); // 9d old
        assert!(long_cache_fresh(Some(&fresh), today));
        assert!(long_cache_fresh(Some(&today), today)); // cached today -> reuse
        assert!(!long_cache_fresh(Some(&stale), today)); // TTL expired -> refetch (month rolled)
        assert!(!long_cache_fresh(None, today)); // never cached -> fetch
    }

    /// (round 56/57) topHoldings parse: symbol preferred, holdingName fallback, uppercased, empty
    /// symbol treated as absent, weight fraction kept (0.0 when absent), capped at 10, missing
    /// module -> empty (not a panic).
    #[test]
    fn top_holdings_parse() {
        let v = serde_json::json!({"quoteSummary": {"result": [{"topHoldings": {"holdings": [
            {"symbol": "AAPL", "holdingName": "Apple Inc", "holdingPercent": {"raw": 0.07}},
            {"symbol": "", "holdingName": "Microsoft Corp", "holdingPercent": {"raw": 0.06}},
            {"holdingName": "nvidia corp"},
        ]}}]}});
        assert_eq!(parse_top_holdings(&v), vec![
            ("AAPL".to_string(), 0.07),
            ("MICROSOFT CORP".to_string(), 0.06),
            ("NVIDIA CORP".to_string(), 0.0),
        ]);
        assert!(parse_top_holdings(&serde_json::json!({"quoteSummary": {"result": [{}]}})).is_empty());
        assert!(parse_top_holdings(&serde_json::json!({})).is_empty());
        let eleven: Vec<Value> = (0..11).map(|i| serde_json::json!({"symbol": format!("S{i}")})).collect();
        let v = serde_json::json!({"quoteSummary": {"result": [{"topHoldings": {"holdings": eleven}}]}});
        assert_eq!(parse_top_holdings(&v).len(), 10);
    }

    /// (report round 7) composition fields the plain holdings seam drops: sector weights (bare float
    /// OR `{raw}`, zeros dropped, sorted heaviest-first, keys prettified), the equity/bond split
    /// (bond defaults 0, `None` when no stock position), and the fund category (with blank filter).
    #[test]
    fn fund_composition_parse() {
        let v = serde_json::json!({"quoteSummary": {"result": [{
            "fundProfile": {"categoryName": "Technology"},
            "topHoldings": {
                "stockPosition": {"raw": 0.994},
                "bondPosition": 0.006,
                "sectorWeightings": [
                    {"technology": {"raw": 0.55}},
                    {"financial_services": 0.20},
                    {"realestate": {"raw": 0.10}},
                    {"utilities": 0.0},            // zero -> dropped
                ],
            },
        }]}});
        assert_eq!(parse_fund_sectors(&v), vec![
            ("Technology".to_string(), 0.55),
            ("Financial Services".to_string(), 0.20),
            ("Real Estate".to_string(), 0.10),
        ]);
        assert_eq!(parse_fund_stock_bond(&v), Some((0.994, 0.006)));
        assert_eq!(parse_fund_category(&v), Some("Technology".to_string()));

        // bondPosition absent -> defaults 0; category via fallback key; blank category -> None.
        let v2 = serde_json::json!({"quoteSummary": {"result": [{
            "fundProfile": {"category": "Large Blend"},
            "topHoldings": {"stockPosition": 1.0},
        }]}});
        assert_eq!(parse_fund_stock_bond(&v2), Some((1.0, 0.0)));
        assert_eq!(parse_fund_category(&v2), Some("Large Blend".to_string()));
        let blank = serde_json::json!({"quoteSummary": {"result": [{"fundProfile": {"categoryName": "  "}}]}});
        assert_eq!(parse_fund_category(&blank), None);

        // no stock position at all -> None (not a fund with a reported split); empty payloads.
        let none = serde_json::json!({"quoteSummary": {"result": [{"topHoldings": {"bondPosition": 0.5}}]}});
        assert_eq!(parse_fund_stock_bond(&none), None);
        assert!(parse_fund_sectors(&serde_json::json!({})).is_empty());
        assert_eq!(pretty_sector("consumer_cyclical"), "Consumer Cyclical");
        assert_eq!(pretty_sector("realestate"), "Real Estate");
    }

    /// Fund-P/E inversion pin: Yahoo serves equityHoldings ratios as RECIPROCALS (live IITU.L
    /// receipt: raw 0.02947 => real P/E 33.9). A "fix" that returns the raw value trips here.
    /// Non-positive / missing raw -> None, never a fake ratio; bare-number form parses too.
    #[test]
    fn fund_pe_inversion() {
        let wrap = |pe: serde_json::Value| {
            serde_json::json!({"quoteSummary": {"result": [{
                "topHoldings": {"equityHoldings": {"priceToEarnings": pe}}
            }]}})
        };
        let pe = parse_fund_pe(&wrap(serde_json::json!({"raw": 0.02947}))).unwrap();
        assert!((pe - 33.93).abs() < 0.05, "inverted P/E expected ~33.9, got {pe}");
        let bare = parse_fund_pe(&wrap(serde_json::json!(0.05))).unwrap();
        assert!((bare - 20.0).abs() < 1e-9);
        assert_eq!(parse_fund_pe(&wrap(serde_json::json!({"raw": 0.0}))), None);
        assert_eq!(parse_fund_pe(&wrap(serde_json::json!({"raw": -0.01}))), None);
        assert_eq!(parse_fund_pe(&wrap(serde_json::json!({}))), None);
        assert_eq!(parse_fund_pe(&serde_json::json!({})), None);
    }

    /// (us 20Y) fixture: one December index row per year, 3%/yr compounding so every YoY rate
    /// that has its predecessor level in the payload exists and equals 3.0.
    fn bls_dec_levels(years: std::ops::RangeInclusive<i32>) -> serde_json::Value {
        let data: Vec<serde_json::Value> = years
            .map(|y| {
                serde_json::json!({
                    "year": y.to_string(),
                    "period": "M12",
                    "value": format!("{:.6}", 100.0 * 1.03f64.powi(y - 2006)),
                })
            })
            .collect();
        serde_json::json!({"Results": {"series": [{"data": data}]}})
    }

    /// (us 20Y) The starved column and its fix in one place: the fresh keyless window alone
    /// (2017..2026 levels -> 9 rates) can't fill a 20Y compound; merged with the old-decade
    /// window at the LEVEL layer, the cross-window year (2017) gets its predecessor and the map
    /// reaches 20 contiguous rates. Merging parsed RATES instead of levels fails exactly here.
    #[test]
    fn bls_two_window_merge() {
        let old = bls_dec_levels(2006..=2016);
        let new = bls_dec_levels(2017..=2026);
        let new_alone = core::parse_bls_cpi(&new);
        assert_eq!(new_alone.len(), 9); // 2018..2026 — 2017 has no in-window predecessor
        assert_eq!(core::inflation_compounded(&new_alone, 20), None); // the n/a being fixed
        let merged = core::parse_bls_cpi(&merge_bls_payloads(Some(&old), &new));
        assert_eq!(merged.len(), 20); // 2007..2026 — 2017 healed across the window seam
        assert!(merged.contains_key(&2017), "cross-window year must gain its rate");
        let cum = core::inflation_compounded(&merged, 20).unwrap();
        assert!((cum - (1.03f64.powi(20) - 1.0) * 100.0).abs() < 1e-4, "20 x 3% compound, got {cum}");
        // absent/shapeless old leaves the fresh payload untouched
        assert_eq!(merge_bls_payloads(None, &new), new);
        assert_eq!(merge_bls_payloads(Some(&serde_json::json!({})), &new), new);
    }

    /// (us 20Y) Year-hole guard: the permanent old-window cache is valid only while it still
    /// yields the rate for now-10, the year adjacent to the fresh window. Same cache, one
    /// calendar year later -> stale -> refetch (silent short "20Y" compounds never happen).
    #[test]
    fn bls_old_window_revalidation() {
        let old = bls_dec_levels(2006..=2016); // rates 2007..2016
        assert!(old_window_covers(&old, 2026)); // 2026-10 = 2016 -> covered
        assert!(!old_window_covers(&old, 2027)); // 2017 rate missing -> hole -> refetch
    }

    /// Pence-quote FX scale: LSE "GBp"/"GBX" divide the pound rate by 100; real ISO codes (and the
    /// already-uppercase "GBP") pass through untouched.
    #[test]
    fn pence_fx_scale() {
        assert_eq!(fx_scale("GBp"), 0.01);
        assert_eq!(fx_scale("GBX"), 0.01);
        assert_eq!(fx_scale("GBP"), 1.0); // already pounds
        assert_eq!(fx_scale("USD"), 1.0);
        assert_eq!(fx_scale(""), 1.0);
    }

    /// TTM roll-forward off REAL LITE (Lumentum) SEC data: FY 0.37 + current 9mo-YTD 2.59 − prior 9mo-YTD
    /// −2.72 = 5.68 (vs the stale annual 0.37 that produced the bogus P/E 2319). Restatement duplicates
    /// de-dupe to the newest filing; standalone quarters are ignored (YTD cumulatives drive the roll).
    #[test]
    fn ttm_eps_rollforward() {
        use serde_json::json;
        let facts = |start: &str, end: &str, val: f64, filed: &str| {
            json!({"start": start, "end": end, "val": val, "filed": filed, "form": "10-Q"})
        };
        let j = json!({"units": {"USD/shares": [
            facts("2023-06-25", "2024-06-29", -8.12, "2024-08-20"), // FY2024 (ignored, older)
            facts("2024-06-30", "2025-06-28", 0.37, "2025-08-20"),  // FY2025 = base
            facts("2024-07-01", "2025-03-29", -2.72, "2025-05-06"), // prior-year 9mo YTD
            facts("2025-06-29", "2026-03-28", 2.59, "2026-05-06"),  // current 9mo YTD
            facts("2025-06-29", "2026-03-28", 2.59, "2026-06-01"),  // restatement dup -> deduped
            facts("2025-12-28", "2026-03-28", 1.50, "2026-05-06"),  // standalone quarter -> ignored
        ]}});
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        // 15x the annual — the very case this fn exists to serve. It MUST survive the sanity bound.
        assert_eq!(ttm_eps_from_concept(&j, today), Some(0.37 + 2.59 - (-2.72)));
        // no YTD past the latest 10-K -> falls back to the annual EPS unchanged (never n/a, never a guess)
        let annual_only = json!({"units": {"USD/shares": [
            facts("2024-06-30", "2025-06-28", 0.37, "2025-08-20"),
        ]}});
        assert_eq!(ttm_eps_from_concept(&annual_only, today), Some(0.37));
        // current YTD exists but NO prior-year YTD of matching length -> can't de-cumulate -> annual fallback
        let no_prior = json!({"units": {"USD/shares": [
            facts("2024-06-30", "2025-06-28", 0.37, "2025-08-20"),  // FY base
            facts("2025-06-29", "2026-03-28", 2.59, "2026-05-06"),  // current 9mo YTD, no prior twin
        ]}});
        assert_eq!(ttm_eps_from_concept(&no_prior, today), Some(0.37));
        assert_eq!(ttm_eps_from_concept(&json!({}), today), None); // no units at all -> None
    }

    /// (split basis) The roll spans three filings, so all three must be quoted in the SAME share basis —
    /// and after a split only the newest filing of each period is. REAL O'Reilly data: ORLY split 15-for-1
    /// on 2025-06-10, and de-duping to the first filing left the prior-year leg alone on the pre-split
    /// basis: 2.98 + 0.72 − 9.35 = −5.66, which is exactly what `.sec_cache/ORLY_ttmeps2.json` held. A
    /// negative EPS is then dropped by the `pe` cell's `> 0.0` filter, so the column just read n/a.
    ///
    /// Neither shipped guard catches this, which is why it needs its own test: the roll is fresh, and
    /// |−5.66| sits far inside the `100 * annual` sanity band. Only the de-dupe rule decides it.
    ///
    /// The two filings of the prior-year period are listed NEWEST FIRST on purpose. Array position and
    /// filing order disagree, so a rule that kept whatever landed last would still print −5.66 here.
    #[test]
    fn ttm_eps_uses_the_newest_filing_so_a_split_cannot_mix_share_bases() {
        use serde_json::json;
        let facts = |start: &str, end: &str, val: f64, filed: &str| {
            json!({"start": start, "end": end, "val": val, "filed": filed, "form": "10-Q"})
        };
        let j = json!({"units": {"USD/shares": [
            facts("2025-01-01", "2025-12-31", 2.98, "2026-02-25"), // FY2025 base, post-split
            facts("2025-01-01", "2025-03-31", 0.62, "2026-05-06"), // prior Q1 RESTATED post-split (9.35/15)
            facts("2025-01-01", "2025-03-31", 9.35, "2025-05-07"), // same period as originally filed, PRE-split
            facts("2026-01-01", "2026-03-31", 0.72, "2026-05-06"), // current Q1 YTD, post-split
        ]}});
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let ttm = ttm_eps_from_concept(&j, today).expect("the roll must produce a value");
        assert!((ttm - (2.98 + 0.72 - 0.62)).abs() < 1e-9, "want the post-split 3.08, got {ttm}");
        assert!(ttm > 0.0, "a positive TTM EPS is what keeps the pe cell from reading n/a");
    }

    /// The two guards, each against the REAL payload that motivated it. They are independent: neither
    /// bad case is caught by the other's rule, which is why both ship.
    #[test]
    fn ttm_eps_rejects_stale_and_insane_rolls() {
        use serde_json::json;
        let facts = |start: &str, end: &str, val: f64, filed: &str| {
            json!({"start": start, "end": end, "val": val, "filed": filed, "form": "10-Q"})
        };
        let today = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        // MNST: EarningsPerShareDiluted STOPS in 2010. The roll is arithmetically perfect and 15 years
        // dead — 2.28 + 1.49 − 1.04 = 2.73, which against a 2026 price printed P/E 34.3 vs the real ~48.
        let stale = json!({"units": {"USD/shares": [
            facts("2010-01-01", "2010-12-31", 2.28, "2011-03-01"), // FY2010 base
            facts("2010-01-01", "2010-06-30", 1.04, "2010-08-01"), // prior-year H1
            facts("2011-01-01", "2011-06-30", 1.49, "2011-08-01"), // current H1
        ]}});
        assert_eq!(ttm_eps_from_concept(&stale, today), None);
        // ...and the SAME payload read in 2011 was perfectly good. The guard is about age, not shape.
        assert_eq!(
            ttm_eps_from_concept(&stale, NaiveDate::from_ymd_opt(2011, 9, 1).unwrap()),
            Some(2.28 + 1.49 - 1.04)
        );
        // HAL: SHARE COUNTS mis-tagged inside the EPS concept. Note the dates — 2024/2025 — well INSIDE
        // a 2-year freshness window, so only the relative bound catches this one. 46,500x the annual.
        let insane = json!({"units": {"USD/shares": [
            facts("2024-01-01", "2024-12-31", -1.29, "2025-02-01"),   // FY2024 base, a real EPS
            facts("2024-01-01", "2024-09-30", 600000.0, "2024-10-01"),// prior-year YTD, a SHARE COUNT
            facts("2025-01-01", "2025-09-30", 540000.0, "2025-10-01"),// current YTD, likewise
        ]}});
        assert_eq!(ttm_eps_from_concept(&insane, today), None);
        // BRK-A shaped: a genuine ~$40,000 EPS is NOT unit confusion. An absolute band would kill it.
        let huge = json!({"units": {"USD/shares": [
            facts("2024-01-01", "2024-12-31", 39_000.0, "2025-02-01"),
            facts("2024-01-01", "2024-09-30", 28_000.0, "2024-10-01"),
            facts("2025-01-01", "2025-09-30", 31_000.0, "2025-10-01"),
        ]}});
        assert_eq!(ttm_eps_from_concept(&huge, today), Some(39_000.0 + 31_000.0 - 28_000.0));
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
            ]}},
            // total assets — same instant collector, the ROE fallback denominator
            "Assets": {"units": {"USD": [
                {"end": "2021-09-30", "val": 3000.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            // (round 107) survival inputs, FY2021 only — FY2022 must stay None-neutral
            "OperatingIncomeLoss": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 200.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            // (EV/EBITDA) D&A add-back, FY2021 only -> EBITDA = op 200 + 80 = 280; FY2022 stays None
            "DepreciationDepletionAndAmortization": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 80.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "NetCashProvidedByUsedInOperatingActivities": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 300.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "PaymentsToAcquirePropertyPlantAndEquipment": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 100.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "InterestExpense": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 50.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "LongTermDebtNoncurrent": {"units": {"USD": [
                {"end": "2021-09-30", "val": 300.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "DebtCurrent": {"units": {"USD": [
                {"end": "2021-09-30", "val": 100.0, "form": "10-K", "filed": "2021-11-01"}
            ]}},
            "CashAndCashEquivalentsAtCarryingValue": {"units": {"USD": [
                {"end": "2021-09-30", "val": 500.0, "form": "10-K", "filed": "2021-11-01"}
            ]}}
        }}});
        let mut rows = parse_sec_facts(&j);
        rows.sort_by_key(|r| r.period_end);
        assert_eq!(rows.len(), 2); // two fiscal years
        // FY2021: original value kept (1000, not the 1234 restatement), original filing date
        assert_eq!(rows[0].period_end, NaiveDate::from_ymd_opt(2021, 9, 30).unwrap());
        assert_eq!(rows[0].filed, NaiveDate::from_ymd_opt(2021, 11, 1).unwrap());
        assert_eq!(rows[0].revenue, Some(1000.0));
        assert_eq!(rows[0].currency.as_deref(), Some("USD")); // (FX) a US filer's books, detected not assumed
        assert_eq!(rows[0].gross_margin, Some(40.0)); // 400/1000
        assert_eq!(rows[0].eps, Some(3.0));
        assert_eq!(rows[0].roe, Some(15.0)); // NetIncome 150 ÷ StockholdersEquity 1000 (instant)
        assert_eq!(rows[0].roa, Some(5.0)); // NetIncome 150 ÷ Assets 3000 — same numerator, wider denominator
        // (round 107) survival levels, FY2021: fcf (300−100)/1000, cover 200/50, net cash (500−300−100)/1000
        assert_eq!(rows[0].fcf_margin, Some(20.0));
        assert_eq!(rows[0].interest_cover, Some(4.0));
        assert_eq!(rows[0].net_cash_rev, Some(10.0));
        // (EV/EBITDA) FY2021: EBITDA = op 200 + D&A 80 = 280; net_debt = debt (300+100) − cash 500 = −100 (net cash)
        assert_eq!(rows[0].ebitda, Some(280.0));
        assert_eq!(rows[0].net_debt, Some(-100.0));
        // FY2022: no EPS / NI / equity / survival line -> None (neutral, never a fake 0)
        assert_eq!(rows[1].revenue, Some(1200.0));
        assert_eq!(rows[1].gross_margin, Some(50.0)); // 600/1200
        assert_eq!(rows[1].eps, None);
        assert_eq!(rows[1].roe, None);
        assert_eq!(rows[1].roa, None); // no NI and no Assets line for FY2022 -> None, not a fabricated 0
        assert_eq!(rows[1].fcf_margin, None);
        assert_eq!(rows[1].interest_cover, None);
        assert_eq!(rows[1].net_cash_rev, None);
        // (EV/EBITDA) FY2022: no op income / D&A / cash line -> both None (a partial EBITDA is garbage)
        assert_eq!(rows[1].ebitda, None);
        assert_eq!(rows[1].net_debt, None);
        assert!(parse_sec_facts(&json!({})).is_empty()); // no facts -> empty, never panics
        assert!(parse_sec_facts(&json!({"facts": {}})).is_empty()); // facts but no us-gaap -> empty
    }

    /// `parse_sec_facts` drops every malformed / non-annual datapoint (missing start/end, unparseable
    /// date, missing filed/val, non-10-K form, sub-annual span) without panicking — the revenue anchor
    /// map ends empty, so no row survives.
    #[test]
    fn sec_facts_skips_malformed() {
        use serde_json::json;
        let j = json!({"facts": {"us-gaap": {"Revenues": {"units": {"USD": [
            {"end": "2021-09-30", "val": 100.0, "form": "10-K", "filed": "2021-11-01"},                 // no start -> skip
            {"start": "bad", "end": "2021-09-30", "val": 100.0, "form": "10-K", "filed": "2021-11-01"}, // unparseable date -> skip
            {"start": "2020-10-01", "end": "2021-09-30", "form": "10-K", "filed": "2021-11-01"},        // no val -> skip
            {"start": "2020-10-01", "end": "2021-09-30", "val": 100.0, "form": "10-Q", "filed": "2021-11-01"}, // not 10-K -> skip
            {"start": "2021-07-01", "end": "2021-09-30", "val": 100.0, "form": "10-K", "filed": "2021-11-01"}, // ~3mo span -> skip
        ]}}}}});
        assert!(parse_sec_facts(&j).is_empty());
    }

    /// (FX/20-F) A foreign private issuer files in `us-gaap` but keeps its books in its OWN currency —
    /// ASML files 20-F in EUR. Both were hard-coded before (form `10-K`, unit `USD`), so every datapoint
    /// was rejected and the filer cached as "no fundamentals" with no error surfacing anywhere.
    ///
    /// Pins all three fixes at once: 20-F counts as annual, EUR is DETECTED off the unit key, and the
    /// currency rides back on the row — without it nothing downstream can prove a price belongs in the
    /// same books as this EPS. `6-K` is interim and must still be rejected: it is filed EARLIER than the
    /// 20-F, so the earliest-filing dedup would hand it the period if the form filter ever let it in.
    #[test]
    fn sec_facts_foreign_20f_non_usd() {
        use serde_json::json;
        let j = json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"EUR": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 27600.0, "form": "20-F", "filed": "2024-02-14"},
                {"start": "2023-01-01", "end": "2023-12-31", "val": 99999.0, "form": "6-K", "filed": "2023-07-19"}
            ]}},
            "NetIncomeLoss": {"units": {"EUR": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 7800.0, "form": "20-F", "filed": "2024-02-14"}
            ]}},
            "EarningsPerShareDiluted": {"units": {"EUR/shares": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 19.91, "form": "20-F", "filed": "2024-02-14"}
            ]}},
            "StockholdersEquity": {"units": {"EUR": [
                {"end": "2023-12-31", "val": 13000.0, "form": "20-F", "filed": "2024-02-14"}
            ]}},
            "Assets": {"units": {"EUR": [
                {"end": "2023-12-31", "val": 39000.0, "form": "20-F", "filed": "2024-02-14"}
            ]}}
        }}});
        let rows = parse_sec_facts(&j);
        assert_eq!(rows.len(), 1, "20-F must count as an annual form");
        assert_eq!(rows[0].currency.as_deref(), Some("EUR"));
        assert_eq!(rows[0].revenue, Some(27600.0), "the earlier-filed 6-K must not win the period");
        assert_eq!(rows[0].eps, Some(19.91)); // read from EUR/shares — "USD/shares" is not a universal key
        assert_eq!(rows[0].roe, Some(60.0)); // 7800/13000 — a ratio, so it never needed the currency
        assert_eq!(rows[0].roa, Some(20.0)); // 7800/39000 — same tag string as a 10-K filer's, in EUR
    }

    /// (same-filing comparatives) `prior_eps`/`prior_shares` must come from the filing that WON the
    /// period, not from the previous row — and the difference is the whole point. Two real shapes:
    ///
    /// TPL split 3-for-1 in 2024 and again in 2025, so each 10-K restates its comparatives at the
    /// current basis. Earliest-filed keeps each year from its ORIGINAL filing, so the stored series
    /// runs 7.69M / 23.02M / 69.03M shares and 52.77 / 19.72 / 6.97 EPS — three bases, and dividing
    /// across them reads -64.7% on a company that grew +6.0%.
    ///
    /// COF issued shares to buy Discover: no split, so its FY2025 10-K repeats FY2024 UNRESTATED. The
    /// share jump is identical in shape to TPL's (both blow past any |Δ|>40% rule) but here the drop
    /// is real and must survive. Nothing but the comparative can tell these two apart.
    #[test]
    fn sec_facts_same_filing_prior_split_vs_issuance() {
        use serde_json::json;
        let ann = |end: &str, val: f64, filed: &str| {
            let start = format!("{}-01-01", &end[..4]);
            json!({"start": start, "end": end, "val": val, "form": "10-K", "filed": filed})
        };
        // --- TPL: comparatives RESTATED at each filing's basis ---
        let tpl = json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"USD": [
                ann("2023-12-31", 631_595_000.0, "2024-02-21"),
                ann("2024-12-31", 705_823_000.0, "2025-02-19"),
                ann("2025-12-31", 798_190_000.0, "2026-02-18"),
            ]}},
            "EarningsPerShareDiluted": {"units": {"USD/shares": [
                ann("2022-12-31", 57.77, "2024-02-21"), ann("2023-12-31", 52.77, "2024-02-21"),
                ann("2023-12-31", 17.59, "2025-02-19"), ann("2024-12-31", 19.72, "2025-02-19"),
                ann("2024-12-31", 6.573, "2026-02-18"), ann("2025-12-31", 6.97, "2026-02-18"),
            ]}},
            "WeightedAverageNumberOfDilutedSharesOutstanding": {"units": {"shares": [
                ann("2022-12-31", 7_726_809.0, "2024-02-21"), ann("2023-12-31", 7_686_615.0, "2024-02-21"),
                ann("2023-12-31", 23_059_845.0, "2025-02-19"), ann("2024-12-31", 23_019_751.0, "2025-02-19"),
                ann("2024-12-31", 69_059_252.0, "2026-02-18"), ann("2025-12-31", 69_027_492.0, "2026-02-18"),
            ]}},
        }}});
        let rows = parse_sec_facts(&tpl);
        let newest = rows.iter().max_by_key(|r| r.period_end).expect("FY2025 row");
        // the LEVELS still come from the earliest filing — as-of semantics are untouched
        assert_eq!(newest.eps, Some(6.97));
        assert_eq!(newest.shares, Some(69_027_492.0));
        // ...and the DENOMINATORS come from that same filing, so both sit on the post-split basis.
        // The previous ROW says 19.72 / 23,019,751; using those is the -64.7% lie.
        assert_eq!(newest.prior_eps, Some(6.573));
        assert_eq!(newest.prior_shares, Some(69_059_252.0));
        assert!((core::yoy_pct(newest.eps, newest.prior_eps).unwrap() - 6.04).abs() < 0.01);
        assert!((core::yoy_pct(newest.shares, newest.prior_shares).unwrap() + 0.046).abs() < 0.01);
        // an older row reads its OWN filing's comparative, not the next filing's restatement
        let fy23 = rows.iter().find(|r| r.period_end.to_string() == "2023-12-31").expect("FY2023 row");
        assert_eq!((fy23.eps, fy23.prior_eps), (Some(52.77), Some(57.77)));

        // --- COF: comparatives NOT restated (an acquisition, not a split) ---
        let cof = json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"USD": [
                ann("2024-12-31", 39_100_000_000.0, "2025-01-21"),
                ann("2025-12-31", 55_000_000_000.0, "2026-01-20"),
            ]}},
            "EarningsPerShareDiluted": {"units": {"USD/shares": [
                ann("2024-12-31", 11.59, "2025-01-21"),
                ann("2024-12-31", 11.59, "2026-01-20"), ann("2025-12-31", 4.03, "2026-01-20"),
            ]}},
            "WeightedAverageNumberOfDilutedSharesOutstanding": {"units": {"shares": [
                ann("2024-12-31", 383_600_000.0, "2025-01-21"),
                ann("2024-12-31", 383_600_000.0, "2026-01-20"), ann("2025-12-31", 541_300_000.0, "2026-01-20"),
            ]}},
        }}});
        let rows = parse_sec_facts(&cof);
        let newest = rows.iter().max_by_key(|r| r.period_end).expect("FY2025 row");
        assert_eq!(newest.prior_eps, Some(11.59), "unrestated -> the comparative equals the prior row");
        assert_eq!(newest.prior_shares, Some(383_600_000.0));
        // both REAL and both previously suppressed by the |Δ|>40% rule
        assert!((core::yoy_pct(newest.eps, newest.prior_eps).unwrap() + 65.23).abs() < 0.01);
        assert!((core::yoy_pct(newest.shares, newest.prior_shares).unwrap() - 41.11).abs() < 0.01);

        // a filing with no comparative at all -> None, and the caller falls back
        let lone = json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"USD": [ann("2025-12-31", 100.0, "2026-01-20")]}},
            "EarningsPerShareDiluted": {"units": {"USD/shares": [ann("2025-12-31", 1.0, "2026-01-20")]}},
        }}});
        assert_eq!(parse_sec_facts(&lone)[0].prior_eps, None);
    }

    /// (V) `parse_sec_instance` — the per-class EPS `companyfacts` refuses to serve. Fixture reproduces
    /// the REAL shape of `v-20250930_htm.xml`: default-namespace contexts and facts whose attributes are
    /// split across lines (a literal `<tag contextRef=` find matches NOTHING on the actual document,
    /// which is why this parse is regex-based). Numbers are Visa's own, verified against the filing.
    #[test]
    fn sec_instance_picks_the_class_that_reconciles() {
        let ctx = |id: &str, s: &str, e: &str, member: Option<&str>| {
            let seg = member.map_or(String::new(), |m| {
                format!(r#"<segment><xbrldi:explicitMember dimension="us-gaap:StatementClassOfStockAxis">{m}</xbrldi:explicitMember></segment>"#)
            });
            format!(
                "<context id=\"{id}\"><entity><identifier>0001403161</identifier>{seg}</entity>\
                 <period><startDate>{s}</startDate><endDate>{e}</endDate></period></context>"
            )
        };
        // attributes on their own lines, exactly as SEC emits them
        let fact = |tag: &str, cref: &str, val: &str| {
            format!("<us-gaap:{tag}\n      contextRef=\"{cref}\"\n      decimals=\"2\"\n      id=\"f-1\">{val}</us-gaap:{tag}>")
        };
        let v = [
            ctx("c-1", "2024-10-01", "2025-09-30", None), // undimensioned -> carries net income
            ctx("c-2", "2024-10-01", "2025-09-30", Some("us-gaap:CommonClassAMember")),
            ctx("c-3", "2024-10-01", "2025-09-30", Some("us-gaap:CommonClassBMember")),
            ctx("c-4", "2023-10-01", "2024-09-30", None),
            ctx("c-5", "2023-10-01", "2024-09-30", Some("us-gaap:CommonClassAMember")),
            ctx("c-6", "2022-10-01", "2023-09-30", None),
            ctx("c-7", "2022-10-01", "2023-09-30", Some("us-gaap:CommonClassAMember")),
            ctx("c-q", "2025-07-01", "2025-09-30", Some("us-gaap:CommonClassAMember")), // a QUARTER
            fact("NetIncomeLoss", "c-1", "20058000000"),
            fact("NetIncomeLoss", "c-4", "19743000000"),
            fact("NetIncomeLoss", "c-6", "17273000000"),
            fact("EarningsPerShareDiluted", "c-2", "10.20"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-2", "1966000000"),
            fact("EarningsPerShareDiluted", "c-3", "16.12"), // Class B: 16.12 x 90M = 1.45B, nowhere near 20.06B
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-3", "90000000"),
            fact("EarningsPerShareDiluted", "c-5", "9.73"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-5", "2029000000"),
            fact("EarningsPerShareDiluted", "c-7", "8.28"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-7", "2085000000"),
            fact("EarningsPerShareDiluted", "c-q", "2.98"), // the 350-380 day rule must drop this
        ]
        .join("\n");
        let got = parse_sec_instance(&v, &US_GAAP_TAGS);
        let at = |d: &str| got.get(&NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap()).copied();
        assert_eq!(got.len(), 3, "one entry per FISCAL YEAR — the quarterly context must not make a fourth");
        assert_eq!(at("2025-09-30"), Some((10.20, Some(1_966_000_000.0))), "Class A: 1,966M x 10.20 = 20.05B = net income");
        assert_eq!(at("2024-09-30"), Some((9.73, Some(2_029_000_000.0))));
        assert_eq!(at("2023-09-30"), Some((8.28, Some(2_085_000_000.0))));
        assert_eq!(at("2025-12-31"), None, "the quarter ends 2025-09-30 anyway; nothing else may appear");

        // THE ASSERT THE RULE EXISTS FOR — ERIE, whose Class B is EPS 1,801 on 2,542 shares. It reads
        // like the "real" per-share number and is off by a factor of 168 in aggregate. A member-name
        // allowlist is what this forbids: ERIE and V trade as ClassA, HSY and KKR tag CommonStockMember,
        // and no list knows that in advance. Class B is placed FIRST here so document order can't rescue
        // it, and the whole fixture is `xbrli:`-prefixed to pin the namespace tolerance.
        let erie = [
            r#"<xbrli:context id="d1"><xbrli:entity><xbrli:identifier>x</xbrli:identifier></xbrli:entity><xbrli:period><xbrli:startDate>2024-01-01</xbrli:startDate><xbrli:endDate>2024-12-31</xbrli:endDate></xbrli:period></xbrli:context>"#.to_string(),
            r#"<xbrli:context id="dB"><xbrli:entity><xbrli:identifier>x</xbrli:identifier><xbrli:segment><xbrldi:explicitMember dimension="us-gaap:StatementClassOfStockAxis">us-gaap:CommonClassBMember</xbrldi:explicitMember></xbrli:segment></xbrli:entity><xbrli:period><xbrli:startDate>2024-01-01</xbrli:startDate><xbrli:endDate>2024-12-31</xbrli:endDate></xbrli:period></xbrli:context>"#.to_string(),
            r#"<xbrli:context id="dA"><xbrli:entity><xbrli:identifier>x</xbrli:identifier><xbrli:segment><xbrldi:explicitMember dimension="us-gaap:StatementClassOfStockAxis">us-gaap:CommonClassAMember</xbrldi:explicitMember></xbrli:segment></xbrli:entity><xbrli:period><xbrli:startDate>2024-01-01</xbrli:startDate><xbrli:endDate>2024-12-31</xbrli:endDate></xbrli:period></xbrli:context>"#.to_string(),
            fact("NetIncomeLoss", "d1", "559000000"),
            fact("EarningsPerShareDiluted", "dB", "1801.00"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "dB", "2542"),
            fact("EarningsPerShareDiluted", "dA", "10.69"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "dA", "52305424"),
        ]
        .join("\n");
        let got = parse_sec_instance(&erie, &US_GAAP_TAGS);
        assert_eq!(
            got.get(&NaiveDate::parse_from_str("2024-12-31", "%Y-%m-%d").unwrap()).copied(),
            Some((10.69, Some(52_305_424.0))),
            "Class B's 1,801 x 2,542 = $4.6M against a $559M bottom line — closest-product must reject it"
        );

        // THE BERKSHIRE CASE — why "closest product" is not enough, and the assert that fails if anyone
        // restores it. BRK tags BOTH equivalent classes and BOTH reconcile to the whole company: A is
        // 46,563 x 1,438,223 = $66.968B, B is 31.04 x 2,157,335,139 = $66.964B, net income $66.968B.
        // Closest-product picks A on floating-point noise. BRK-B trades at ~1/1500 of a Class A share, so
        // that pick prints P/E 0.01 on a number the filing never stated about this ticker. It corrupts P/E
        // and only P/E — EPS growth is a same-units ratio, so it is scale-invariant — which is why the
        // live screen never showed it: BRK's EPS is currently FALLING (41.27 -> 31.04), and a negative
        // grower has no PEG to tilt on. The first growing year turns that 0.01 into PEG ~0.0004.
        let brk = [
            ctx("c-1", "2025-01-01", "2025-12-31", None),
            ctx("c-a", "2025-01-01", "2025-12-31", Some("brka:EquivalentClassAMember")),
            ctx("c-b", "2025-01-01", "2025-12-31", Some("brka:EquivalentClassBMember")),
            fact("NetIncomeLoss", "c-1", "66968000000"),
            fact("EarningsPerShareDiluted", "c-a", "46563"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-a", "1438223"),
            fact("EarningsPerShareDiluted", "c-b", "31.04"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-b", "2157335139"),
        ]
        .join("\n");
        assert_eq!(
            parse_sec_instance(&brk, &US_GAAP_TAGS).values().next().copied(),
            Some((31.04, Some(2_157_335_139.0))),
            "both classes reconcile — the MOST-SHARES rule must break the tie, or BRK-B prints EPS 46,563"
        );

        // …and when NOTHING reconciles, the closest product still decides (a filer whose bottom line and
        // per-share line genuinely disagree). Neither class is within 10% of net income here.
        let neither = [
            ctx("c-1", "2025-01-01", "2025-12-31", None),
            ctx("c-a", "2025-01-01", "2025-12-31", Some("us-gaap:CommonClassAMember")),
            ctx("c-b", "2025-01-01", "2025-12-31", Some("us-gaap:CommonClassBMember")),
            fact("NetIncomeLoss", "c-1", "1000"),
            fact("EarningsPerShareDiluted", "c-a", "1.0"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-a", "500"), // 500, off by half
            fact("EarningsPerShareDiluted", "c-b", "1.0"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-b", "10000"), // 10000, off by 10x
        ]
        .join("\n");
        assert_eq!(
            parse_sec_instance(&neither, &US_GAAP_TAGS).values().next().copied(),
            Some((1.0, Some(500.0))),
            "no class reconciles -> closest product, NOT the biggest class"
        );

        // ORDER IS LOAD-BEARING, same as `parse_sec_facts`: a filer tagging both diluted and basic in one
        // instance (BKR does) must yield the DILUTED number.
        let both = [
            ctx("c-1", "2024-01-01", "2024-12-31", None),
            fact("NetIncomeLoss", "c-1", "1000"),
            fact("EarningsPerShareBasic", "c-1", "2.60"),
            fact("EarningsPerShareDiluted", "c-1", "2.50"),
            fact("WeightedAverageNumberOfDilutedSharesOutstanding", "c-1", "400"),
        ]
        .join("\n");
        assert_eq!(
            parse_sec_instance(&both, &US_GAAP_TAGS).values().next().copied(),
            Some((2.50, Some(400.0))),
            "diluted is listed first in US_GAAP_TAGS.eps and must win, even though basic reconciles better here"
        );

        assert!(parse_sec_instance("", &US_GAAP_TAGS).is_empty()); // garbage in -> empty, never a panic
        assert!(parse_sec_instance("<html>not xbrl at all</html>", &US_GAAP_TAGS).is_empty());
    }

    /// (V) The fallback's TRIGGER is the narrow one: it fires only when the whole companyfacts series has
    /// no EPS anywhere. A filer the API serves normally must never reach it — that guard is what keeps
    /// 501 of 509 names from paying for a multi-MB instance download.
    #[test]
    fn sec_instance_fallback_trigger_is_narrow() {
        use serde_json::json;
        let ann = |start: &str, end: &str, val: f64| json!({"start": start, "end": end, "val": val, "form": "10-K", "filed": "2026-02-01"});
        let healthy = parse_sec_facts(&json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"USD": [ann("2025-01-01", "2025-12-31", 100.0)]}},
            "EarningsPerShareDiluted": {"units": {"USD/shares": [ann("2025-01-01", "2025-12-31", 1.0)]}},
        }}}));
        assert!(!healthy.iter().all(|r| r.eps.is_none()), "an EPS-carrying filer must NOT trip the fallback");
        // Visa's actual shape: revenue and net income present, not one per-share fact in the payload
        let dimensioned = parse_sec_facts(&json!({"facts": {"us-gaap": {
            "Revenues": {"units": {"USD": [ann("2024-10-01", "2025-09-30", 39_000_000_000.0)]}},
            "NetIncomeLoss": {"units": {"USD": [ann("2024-10-01", "2025-09-30", 20_058_000_000.0)]}},
        }}}));
        assert!(!dimensioned.is_empty() && dimensioned.iter().all(|r| r.eps.is_none()), "…and this shape must");
    }

    /// (IFRS) The other half of the foreign world files `ifrs-full`, whose concept names share almost
    /// nothing with us-gaap (`Revenue` not `Revenues`, `ProfitLoss` not `NetIncomeLoss`). Reading only
    /// `/facts/us-gaap` returned zero rows for every one of them.
    ///
    /// Currency is INDEPENDENT of taxonomy — AZN files IFRS in USD — so this pins that axis separately
    /// from the EUR fixture above: getting the taxonomy right must not drag a currency assumption along.
    /// No capex tag here, so fcf_margin stays None rather than treating absent as zero.
    #[test]
    fn sec_facts_ifrs_taxonomy() {
        use serde_json::json;
        let j = json!({"facts": {"ifrs-full": {
            "Revenue": {"units": {"USD": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 45800.0, "form": "20-F", "filed": "2024-02-29"}
            ]}},
            "ProfitLoss": {"units": {"USD": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 5900.0, "form": "20-F", "filed": "2024-02-29"}
            ]}},
            "DilutedEarningsLossPerShare": {"units": {"USD/shares": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 3.79, "form": "20-F", "filed": "2024-02-29"}
            ]}},
            "Equity": {"units": {"USD": [
                {"end": "2023-12-31", "val": 29500.0, "form": "20-F", "filed": "2024-02-29"}
            ]}},
            // `Assets` is the SAME literal under ifrs-full as under us-gaap — the one tag that needed no
            // translation. Pinned here because "obviously the same" is exactly how a silent None ships.
            "Assets": {"units": {"USD": [
                {"end": "2023-12-31", "val": 59000.0, "form": "20-F", "filed": "2024-02-29"}
            ]}},
            "CashFlowsFromUsedInOperatingActivities": {"units": {"USD": [
                {"start": "2023-01-01", "end": "2023-12-31", "val": 11000.0, "form": "20-F", "filed": "2024-02-29"}
            ]}}
        }}});
        let rows = parse_sec_facts(&j);
        assert_eq!(rows.len(), 1, "ifrs-full must be read when us-gaap is absent");
        assert_eq!(rows[0].currency.as_deref(), Some("USD"));
        assert_eq!(rows[0].revenue, Some(45800.0));
        assert_eq!(rows[0].eps, Some(3.79));
        assert_eq!(rows[0].roe, Some(20.0)); // 5900/29500
        assert_eq!(rows[0].roa, Some(10.0)); // 5900/59000
        assert_eq!(rows[0].fcf_margin, None); // no capex tag -> None, never "ocf is the whole FCF"
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
        // (splice) closes stay under the trimmer's 2.0×/wk bar on purpose: splice_max_weekly_rate is
        // config-dependent (0.0 in CI, ci-settings' 2.0 on a dev box), and the trim logic has its own
        // core::splice_trim_start tests — this fixture must parse identically in both environments.
        let j = json!({"chart": {"result": [{
            "timestamp": [1577836800, 1577923200, 1578009600],
            "indicators": {
                "quote": [{"close": [10.0, null, 12.0], "volume": [100, 200]}],
                "adjclose": [{"adjclose": [9.0, null, 10.8]}]
            },
            "meta": {"currency": "EUR", "instrumentType": "EQUITY"},
            "events": {"dividends": {
                "a": {"amount": 2.0, "date": 1593561600}, // 2020-07-01
                "b": {"amount": 1.0, "date": 1585699200}  // 2020-04-01
            }}
        }]}});
        let c = parse_chart(&j, "TST").unwrap();
        assert_eq!(c.closes, vec![10.0, 12.0]); // null close bar dropped
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
        assert_eq!(fr.shares, None); // no share field on this row -> None (never a fake buyback anchor)
        // diluted-share count: a positive value is kept, absent/zero -> None (feeds the buyback column)
        let with_sh = parse_fund_row(&json!({"filingDate": "2022-02-01", "revenue": 5.0, "weightedAverageShsOutDil": 1000.0})).unwrap();
        assert_eq!(with_sh.shares, Some(1000.0));
        let zero_sh = parse_fund_row(&json!({"filingDate": "2022-02-01", "revenue": 5.0, "weightedAverageShsOutDil": 0.0})).unwrap();
        assert_eq!(zero_sh.shares, None); // 0 shares -> None, never a divide anchor
        // no filingDate -> None; bad date -> None
        assert!(parse_fund_row(&json!({"revenue": 100.0})).is_none());
        assert!(parse_fund_row(&json!({"filingDate": "nope"})).is_none());
        // revenue 0 -> treated as absent, so margins go None (never a divide-by-zero garbage value)
        let zero_rev = parse_fund_row(&json!({"filingDate": "2022-02-01", "revenue": 0.0, "grossProfit": 10.0})).unwrap();
        assert_eq!(zero_rev.revenue, None);
        assert_eq!(zero_rev.gross_margin, None);
    }

    /// (round 41) `isin_domicile`: first 2 ISIN chars, uppercased; a too-short string yields None
    /// instead of panicking (defensive against a malformed venue row).
    #[test]
    fn isin_domicile_prefix() {
        assert_eq!(isin_domicile("IE00B3RBWM25"), Some("IE".to_string()));
        assert_eq!(isin_domicile("lu0908500753"), Some("LU".to_string()));
        assert_eq!(isin_domicile("X"), None);
        assert_eq!(isin_domicile(""), None);
    }

    /// (round 42) `parse_yahoo_fund_facts` against canned quoteSummary shapes. Yahoo sends TER as a
    /// FRACTION -> ×100 to percent; a literal 0.0 is its "unknown" sentinel (probe receipt: VUAA.DE
    /// returned 0.0 against its known 0.07%) -> None, never a free fund. AUM prefers
    /// summaryDetail.totalAssets, falls back to defaultKeyStatistics.
    #[test]
    fn yahoo_fund_facts_parse() {
        use serde_json::json;
        // XLKS.L-shape: real TER fraction + summaryDetail assets
        let ok = json!({"quoteSummary": {"result": [{
            "fundProfile": {"feesExpensesInvestment": {"annualReportExpenseRatio": {"raw": 0.0014}}},
            "summaryDetail": {"totalAssets": {"raw": 2.42e9}}
        }]}});
        let (ter, aum) = parse_yahoo_fund_facts(&ok);
        assert!((ter.unwrap() - 0.14).abs() < 1e-9); // fraction 0.0014 -> 0.14%
        assert_eq!(aum, Some(2.42e9));
        // VUAA.DE-shape: raw 0.0 = "unknown" -> None; assets only under defaultKeyStatistics
        let zero_ter = json!({"quoteSummary": {"result": [{
            "fundProfile": {"feesExpensesInvestment": {"annualReportExpenseRatio": {"raw": 0.0}}},
            "defaultKeyStatistics": {"totalAssets": {"raw": 5.0e8}}
        }]}});
        let (ter, aum) = parse_yahoo_fund_facts(&zero_ter);
        assert_eq!(ter, None);
        assert_eq!(aum, Some(5.0e8));
        // empty result / malformed -> (None, None), never panics
        assert_eq!(parse_yahoo_fund_facts(&json!({})), (None, None));
        assert_eq!(parse_yahoo_fund_facts(&json!({"quoteSummary": {"result": []}})), (None, None));
    }

    /// Pin the pacer OFF. MUST be called by any test that can reach the transport: `throttle()`
    /// hard-calls `config::load()`, and `config/settings.yaml` is the gitignored local overlay, so
    /// it is absent on CI and on any fresh clone. That call no longer panics — it prints and
    /// `exit(1)`s, which in a test binary kills the whole PROCESS instead of failing one test, so
    /// forgetting this is worse than it used to be. A zero interval returns before the config read,
    /// and it keeps these tests independent of the operator's `fetch_requests_per_second`, which
    /// would otherwise sleep between them at whatever rate that box is configured for.
    /// `set` losing the race to another test is fine — both write the same value.
    fn pin_throttle() {
        let _ = THROTTLE.set((Mutex::new(Instant::now()), StdDuration::ZERO));
    }

    /// The SECOND `config::load()` landmine on the transport, and the one `pin_throttle` does not
    /// cover: `fetch_concurrency()` reads `fetch_concurrency_multiplier`, so it `exit(1)`s the whole
    /// test process when `settings.yaml` is absent — which is CI, and any fresh clone. Only tests
    /// reaching a `buffer_unordered` fan-out (`fetch_universe`, `fetch_xetra_etfs`) hit it, and only
    /// with no config, so it passes locally and kills the binary on CI. Pin it with both.
    fn pin_fetch_concurrency() {
        let _ = FETCH_CONCURRENCY.set(1);
    }

    /// Seed `.sec_cache/_tickers.json`. EVERY test that can reach `sec_cik` must call this, not just
    /// the one that needs a CIK resolved, because `CIK_MAP` is a process-wide `OnceLock` filled by the
    /// FIRST `sec_cik` call in the binary: whichever test gets there first decides the map for all of
    /// them, and if the file is not on disk yet that map is permanently EMPTY.
    ///
    /// That is a real, latent order dependency, not a hypothetical. Seeding it inside the test that
    /// needs AAPL worked only while the scheduler happened to run that test first — with the scratch
    /// root cold and `--test-threads=1`, `enrich_fund_factor…` sorts earlier, claims the `OnceLock`
    /// from an absent file, and `sec_facts_rows_serve_the_cache_before_the_socket` then fails on a
    /// lookup that has nothing to do with what it is testing. Idempotent, so calling it from several
    /// tests is free.
    ///
    /// AAPL and GOOGL resolve; every other ticker (`NOSUCH`, `AAA`, `BBB`) is absent on purpose, which
    /// is the non-US path a wide screen takes and must stay a miss.
    ///
    /// (EU listing) `EU_TO_US` is a SECOND `OnceLock` filled on that same first `sec_cik` call, so it
    /// carries the same hazard — and it is set here DIRECTLY rather than through
    /// `.eu_listing_cache.json`, which is what closes it. Going through the file made the winner of the
    /// race depend on the file's content at that instant, and
    /// `resolve_eu_listings_serves_its_cache_and_remembers_misses` owns that same file. Seeding it only
    /// when absent could not repair a file left with the WRONG content by anything else on the box (a
    /// `--in-place` mutants run does exactly that), so `cargo t` failed once and then self-healed —
    /// an intermittently red gate, which is worse than a plain failure. Setting the lock means every
    /// caller supplies identical content and who wins stops mattering.
    ///
    /// `set` returning Err is the normal case (some earlier test already filled it) and is ignored.
    fn seed_cik_map() {
        let dir = crate::config::data_path(".sec_cache");
        std::fs::create_dir_all(&dir).expect("scratch .sec_cache");
        std::fs::write(
            dir.join("_tickers.json"),
            r#"{"0":{"ticker":"AAPL","cik_str":320193},"1":{"ticker":"GOOGL","cik_str":1652044}}"#,
        )
        .expect("seed cik map");
        let _ = EU_TO_US.set(invert_eu(&HashMap::from([
            ("GOOGL".to_string(), "ABEA.DE".to_string()),
            ("BRK-B".to_string(), String::new()), // a remembered miss: must invert to nothing
        ])));
    }

    /// One canned HTTP/1.1 response on an ephemeral loopback port, accepted exactly once. The SEC
    /// transport pair is two lines of `reqwest` each and had no test at all, which the mutation gate
    /// found the moment the offline guard made those lines part of a diff: `sec_get_text` could
    /// return `""` forever and the whole suite stayed green. Serving a real socket is what separates
    /// "fetched it" from "returned a stub", and it needs no mock crate to do it.
    ///
    /// `no_proxy` because `reqwest` reads `HTTP_PROXY` from the environment and a proxy set on a dev
    /// box would otherwise swallow the loopback request.
    fn stub_server(body: &'static str) -> (String, Client) {
        pin_throttle();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let _ = std::io::Read::read(&mut sock, &mut [0u8; 2048]); // drain the request line+headers
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let _ = std::io::Write::write_all(&mut sock, resp.as_bytes());
        });
        let client = Client::builder().no_proxy().build().expect("test client");
        (format!("http://{addr}/"), client)
    }

    /// `sec_get_text` hands back the body verbatim — the Form-4 XML `between()` then scans. Not
    /// covered, deliberately: the `offline()` early return. `offline()` is a process-wide `OnceLock`
    /// over `FOLIOMAN_OFFLINE`, so setting it here would race every other test in this binary, and
    /// the mutant it would kill does not exist — a stubbed `-> None` is indistinguishable from a
    /// correct offline `None`.
    #[tokio::test]
    async fn sec_get_text_returns_the_body() {
        let (url, client) = stub_server("<ownershipDocument/>");
        let got = sec_get_text(&client, &url, "folioman-test").await;
        assert_eq!(got.as_deref(), Some("<ownershipDocument/>"));
    }

    /// `sec_get_json` parses it — every CIK, submissions and companyfacts read is this call.
    #[tokio::test]
    async fn sec_get_json_parses_the_body() {
        let (url, client) = stub_server(r#"{"cik":"0000320193"}"#);
        let got = sec_get_json(&client, &url, "folioman-test").await.expect("json");
        assert_eq!(got["cik"], "0000320193");
    }

    /// The general transport trio, same shape and same reason as the SEC pair above — and the funnel
    /// every other fetch in this file goes through, so they are the likeliest lines here to be edited
    /// next. `get_json` in particular READS as covered under `cargo llvm-cov` while being nothing of
    /// the sort: the frozen-data fixture runs offline, so it only ever executes the `offline()` early
    /// return. Coverage says the line ran; only these say it fetched.
    #[tokio::test]
    async fn get_json_parses_the_body() {
        let (url, client) = stub_server(r#"{"chart":{"result":[]}}"#);
        let got = get_json(&client, &url).await.expect("json");
        assert!(got["chart"]["result"].is_array(), "{got}");
    }

    #[tokio::test]
    async fn get_text_returns_the_body() {
        let (url, client) = stub_server("ISIN,name\nIE00B4L5Y983,IWDA");
        assert_eq!(get_text(&client, &url).await.as_deref(), Some("ISIN,name\nIE00B4L5Y983,IWDA"));
    }

    #[tokio::test]
    async fn post_json_parses_the_body() {
        let (url, client) = stub_server(r#"{"isin":"LU0378438732"}"#);
        let got = post_json(&client, &url, &serde_json::json!({ "q": "ping" })).await.expect("json");
        assert_eq!(got["isin"], "LU0378438732");
    }

    /// (#76) OpenFIGI answers a batch POSITIONALLY: one element per job, in job order, and an
    /// unresolvable job still occupies its slot with a `warning` instead of `data`. Compacting that
    /// array instead of mapping it would shift every later answer up one — MSFT's share class would be
    /// recorded against the symbol before it, and the swap would then quote the wrong company entirely.
    /// Nothing downstream could detect it, because a shifted FIGI resolves perfectly well.
    #[tokio::test]
    async fn figi_batch_keeps_one_answer_per_job_in_order() {
        use serde_json::json;
        let (url, client) = stub_server(
            r#"[{"data":[{"shareClassFIGI":"BBG009S39JY5"}]},{"warning":"No identifier found."},{"data":[{"shareClassFIGI":"BBG001S5N8V8"}]}]"#,
        );
        let jobs = vec![
            json!({"idType": "TICKER", "idValue": "GOOGL", "exchCode": "US"}),
            json!({"idType": "TICKER", "idValue": "NOSUCH", "exchCode": "US"}),
            json!({"idType": "TICKER", "idValue": "MSFT", "exchCode": "US"}),
        ];
        let out = figi_batch(&client, &url, jobs).await;
        assert_eq!(out.len(), 3, "one slot per job, always");
        assert_eq!(out[0].as_ref().expect("GOOGL data")[0]["shareClassFIGI"], "BBG009S39JY5");
        assert!(out[1].is_none(), "a warning row carries no data");
        assert_eq!(out[2].as_ref().expect("MSFT data")[0]["shareClassFIGI"], "BBG001S5N8V8");
    }

    /// (#76) A dead endpoint must still answer one slot per job. Returning a SHORT vec instead would
    /// misalign the `zip` in the caller and silently drop the tail of the batch.
    #[tokio::test]
    async fn figi_batch_answers_none_per_job_when_the_endpoint_is_dead() {
        pin_throttle();
        // port 1 is privileged and unbound: refused instantly, no timeout to wait out
        let client = Client::builder().no_proxy().build().expect("test client");
        let out = figi_batch(&client, "http://127.0.0.1:1/", vec![serde_json::json!({}); 3]).await;
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(Option::is_none));
    }

    /// (#76) The EU-listing cache, both branches. ONE test, two phases, because both own the same
    /// `.eu_listing_cache.json` in the shared scratch root and would race as two `#[test]`s — same
    /// reason as `regulatory_isins_serve_a_fresh_cache_and_never_shrink` above.
    ///
    /// The dead port is the assertion in phase 1, not scaffolding: a fully cached call must consult
    /// OpenFIGI zero times, and the only way to prove "zero" is to make any call fail. Phase 2 then
    /// pins the negative-caching rule the single-file design rests on — an unresolvable symbol is
    /// remembered as the empty string rather than left absent, so it is asked about exactly once ever.
    #[tokio::test]
    async fn resolve_eu_listings_serves_its_cache_and_remembers_misses() {
        pin_throttle();
        let path = crate::config::data_path(EU_LISTING_CACHE_PATH);
        let urls = stub_urls("http://127.0.0.1:1/");
        let client = Client::builder().no_proxy().build().expect("test client");

        // 1. every symbol already known -> the file is served verbatim and nothing is fetched
        std::fs::write(&path, r#"{"GOOGL":"ABEA.DE","BRK-B":""}"#).expect("seed cache");
        let got = resolve_eu_listings(&client, &urls, &["GOOGL".to_string(), "BRK-B".to_string()]).await;
        assert_eq!(got["GOOGL"], "ABEA.DE");
        assert_eq!(got["BRK-B"], "", "an empty value is a remembered miss, not a cache gap");

        // 2. a new symbol the resolver cannot answer for -> remembered as a miss, previous answers kept
        let got = resolve_eu_listings(&client, &urls, &["GOOGL".to_string(), "TSLA".to_string()]).await;
        assert_eq!(got["GOOGL"], "ABEA.DE", "an unrelated lookup must not evict a known twin");
        assert_eq!(got["TSLA"], "");
        let on_disk: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("cache written")).expect("cache json");
        assert_eq!(on_disk["TSLA"], "", "the miss is persisted, so it is never asked again");
        assert_eq!(on_disk["GOOGL"], "ABEA.DE");
        // Safe to delete again: `seed_cik_map` no longer reads this file, it sets `EU_TO_US` directly.
        let _ = std::fs::remove_file(&path);
    }

    /// Every `Urls` field pointed at ONE loopback stub. Setting all of them is the safety property, not
    /// a chore: a fetch under test can only reach the address it was handed, so a field the test author
    /// did not think about still cannot leave the machine. `Urls` has 32 required fields, no
    /// `serde(default)` and `deny_unknown_fields`, so adding a field fails here — loudly, in one place —
    /// instead of quietly leaving a live production endpoint reachable from a unit test.
    ///
    /// Built by deserializing rather than by struct literal so a renamed field is a test failure with
    /// the field name in it, and so this stays one line per field.
    ///
    /// `openfigi_mapping` is the exception that proves the point and MUST stay listed: it carries
    /// `#[serde(default)]`, so omitting it here does not fail — it silently deserializes to the live
    /// `api.openfigi.com` endpoint and every EU-listing test starts POSTing to Bloomberg. A defaulted
    /// field is exactly the field the author does not think about, so list defaulted ones too.
    fn stub_urls(base: &str) -> Urls {
        // Every URL field, so a test can NEVER reach a real endpoint. Adding a field to `Urls` without
        // adding it here is how a unit test starts silently hitting the live internet — `justetf_profile`
        // is defaulted to justETF's real host, and `yahoo_fund_facts_fill`'s test drives that path.
        const FIELDS: [&str; 33] = [
            "openfigi_mapping",
            "yahoo_chart", "yahoo_intraday", "yahoo_search", "yahoo_quote", "euribor", "us_cpi",
            "pt_cpi", "eu_hicp", "eu_hicp_old", "coingecko_markets", "sp500_csv", "nupl",
            "coinmetrics_catalog", "coinmetrics_mvrv", "ntfy", "fundamentals", "fundamentals_quality",
            "fundamentals_history", "fund_expense", "bf_etf_search", "bf_salt", "euronext_lisbon",
            "euronext_track", "six_funds", "esma_firds", "fca_firds", "sec_ticker_cik",
            "sec_submissions", "sec_companyfacts", "sec_companyconcept", "sec_user_agent",
            "justetf_profile",
        ];
        let mut yaml: String = FIELDS.iter().map(|f| format!("{f}: \"{base}\"\n")).collect();
        yaml.push_str(&format!("constituents_csv: [\"{base}\"]\n")); // the one non-String field
        serde_yaml::from_str(&yaml).expect("stub urls")
    }

    /// The scratch-root seam itself, asserted directly. Every other test here depends on it silently:
    /// delete `config::test_root_override` and they all still pass, having written their caches into
    /// the working tree — which surfaces as a dirty `git status` on someone else's branch, days later.
    #[test]
    fn unit_tests_write_caches_into_the_scratch_root() {
        let p = crate::config::data_path(REGULATORY_ISINS_PATH);
        assert!(
            p.starts_with(crate::config::test_data_root()),
            "a unit-test cache must not land in the repo: {}",
            p.display()
        );
    }

    /// The regulatory ETF list's disk cache, both branches — reachable for the first time now that
    /// `config::data_path` re-roots into a scratch dir under test.
    ///
    /// ONE test, two phases, because both phases own the same `.regulatory_isins.json` in the one
    /// shared scratch root: as two `#[test]`s they would race each other.
    ///
    /// The seeded ISINs are in REVERSE sort order, and that is the whole discriminator. The fresh
    /// branch returns `cached.isins` verbatim; every other path through this function ends in
    /// `sort()`. Assert on order and a broken freshness check is a failure — assert on the set and
    /// the test passes either way, because the dead registry below makes the fallback return the
    /// same ISINs.
    #[tokio::test]
    async fn regulatory_isins_serve_a_fresh_cache_and_never_shrink() {
        pin_throttle();
        let path = crate::config::data_path(REGULATORY_ISINS_PATH);
        // port 1 is privileged and unbound: every request refuses instantly, no timeout to wait out
        let urls = stub_urls("http://127.0.0.1:1/");
        let client = Client::builder().no_proxy().build().expect("test client");
        let today = chrono::Utc::now().date_naive();

        // 1. cache under 7 days old -> served as-is, registries never consulted
        std::fs::write(
            &path,
            format!(r#"{{"fetched":"{today}","isins":["LU0378437502","IE00B4L5Y983"]}}"#),
        )
        .expect("seed fresh cache");
        let got = fetch_regulatory_etf_isins(&client, &urls).await;
        assert_eq!(got, ["LU0378437502", "IE00B4L5Y983"], "a fresh cache must be returned unsorted");

        // 2. stale cache + dead registries -> the last-good copy survives. A venue outage must never
        //    silently shrink the universe: that reads as a working screen with names missing from it,
        //    which nobody notices, unlike a screen that errors.
        let old = today - chrono::Duration::days(30);
        std::fs::write(&path, format!(r#"{{"fetched":"{old}","isins":["IE00B4L5Y983"]}}"#))
            .expect("seed stale cache");
        let got = fetch_regulatory_etf_isins(&client, &urls).await;
        assert_eq!(got, ["IE00B4L5Y983"], "a failed refresh must keep the last-good list");
    }

    /// (EU listing) `sec_cik` itself — the one seam every SEC read in this file funnels through, and
    /// the one the EU swap broke. `invert_eu` above proves the map is built right; this proves it is
    /// actually CONSULTED, which is a separate failure: delete the alias lookup and `invert_eu`'s test
    /// stays green while 266 swapped constituents silently lose P/E, ROE/A and the whole income block.
    ///
    /// The dead port is an assertion, not scaffolding — the seeded `_tickers.json` must be served from
    /// disk, so any fetch attempt fails the test rather than reaching sec.gov. And a CIK is asserted by
    /// VALUE: an empty-but-`Some` string is exactly what a broken zero-pad or a short-circuited lookup
    /// returns, and it reads as "resolved" at all six call sites.
    #[tokio::test]
    async fn sec_cik_resolves_a_us_ticker_and_a_swapped_xetra_symbol() {
        pin_throttle();
        seed_cik_map();
        let urls = stub_urls("http://127.0.0.1:1/");
        let client = Client::builder().no_proxy().build().expect("test client");

        assert_eq!(sec_cik(&client, &urls, "AAPL").await.as_deref(), Some("0000320193"), "zero-padded to 10");
        assert_eq!(sec_cik(&client, &urls, "aapl").await.as_deref(), Some("0000320193"), "case-insensitive");
        // the fix: the Xetra twin resolves to its US filer's CIK, not to nothing
        assert_eq!(sec_cik(&client, &urls, "ABEA.DE").await.as_deref(), Some("0001652044"), "EU->US alias");
        assert_eq!(sec_cik(&client, &urls, "abea.de").await.as_deref(), Some("0001652044"));
        // a genuinely non-US name still misses — the alias must not become a catch-all
        assert_eq!(sec_cik(&client, &urls, "NOSUCH.DE").await, None);
        assert_eq!(sec_cik(&client, &urls, "").await, None);
    }

    /// `fetch_sec_facts_rows`' SHELL — the disk cache, the CIK gate and the two `then_some` guards.
    /// `parse_sec_facts` itself is already pinned by the six tests above; what was never covered is
    /// the orchestration wrapped around it, and that is where a break is invisible: a ticker whose
    /// fundamentals quietly stop loading still prints a row, it just ranks on nothing.
    ///
    /// ONE test, four phases, because it owns two pieces of process-wide state: `CIK_MAP` (an
    /// `OnceLock` filled from `.sec_cache/_tickers.json` on the FIRST `sec_cik` call in the binary —
    /// no other test here touches it) and the single `.sec_cache` dir under the shared scratch root.
    /// Split into four `#[test]`s they would race on both.
    #[tokio::test]
    async fn sec_facts_rows_serve_the_cache_before_the_socket() {
        pin_throttle();
        seed_cik_map();

        // One cached row, every field distinct so a mis-wired mapping shows up as a wrong number
        // rather than as a coincidence.
        let row = |filed: &str, period_end: &str| SecCacheRow {
            filed: filed.into(),
            period_end: period_end.into(),
            revenue: Some(1000.0),
            gross_margin: Some(40.0),
            op_margin: Some(20.0),
            net_margin: Some(15.0),
            eps: Some(3.0),
            roe: Some(15.0),
            shares: Some(50.0),
            fcf_margin: Some(20.0),
            interest_cover: Some(4.0),
            net_cash_rev: Some(0.1),
            ebitda: Some(280.0),
            net_debt: Some(-100.0),
            currency: Some("USD".into()),
            roa: Some(5.0),
            prior_eps: Some(2.0),
            prior_shares: Some(52.0),
        };
        let cache = sec_cache_path("CACHED_facts10");
        let write = |rows: &[SecCacheRow]| {
            std::fs::write(&cache, serde_json::to_string(rows).expect("serialise")).expect("seed");
        };

        // 1. cache hit -> served without a socket, and an UNPARSEABLE date is dropped rather than
        //    defaulted. A row that silently became 1970-01-01 would sort to the front and be read as
        //    this filer's oldest history.
        write(&[row("2021-11-01", "2021-09-30"), row("not-a-date", "2022-09-30")]);
        let urls = stub_urls("http://127.0.0.1:1/"); // port 1 is unbound: any socket refuses instantly
        let client = Client::builder().no_proxy().build().expect("test client");
        let got = fetch_sec_facts_rows(&client, &urls, "CACHED").await.expect("cache hit");
        assert_eq!(got.len(), 1, "the row with the bad date must be dropped, not defaulted");
        assert_eq!(got[0].filed, NaiveDate::from_ymd_opt(2021, 11, 1).unwrap());
        assert_eq!(got[0].period_end, NaiveDate::from_ymd_opt(2021, 9, 30).unwrap());
        // EVERY field, not just the interesting ones. This is a rehydration mapping, so its failure
        // mode is a field that silently comes back None — and nothing downstream can tell "the filer
        // never reported it" from "the cache read dropped it". The quality term simply goes blank for
        // that name, on every run, until someone reads the JSON by hand. The seeded values are all
        // distinct so a crossed pair of fields reads as a wrong number rather than as a coincidence.
        assert_eq!(got[0].revenue, Some(1000.0));
        assert_eq!(got[0].gross_margin, Some(40.0));
        assert_eq!(got[0].op_margin, Some(20.0));
        assert_eq!(got[0].net_margin, Some(15.0));
        assert_eq!(got[0].eps, Some(3.0));
        assert_eq!(got[0].shares, Some(50.0));
        assert_eq!(got[0].prior_eps, Some(2.0), "the YoY denominator must survive the round trip");
        assert_eq!(got[0].prior_shares, Some(52.0));
        assert_eq!(got[0].roe, Some(15.0));
        assert_eq!(got[0].roa, Some(5.0));
        assert_eq!(got[0].fcf_margin, Some(20.0));
        assert_eq!(got[0].interest_cover, Some(4.0));
        assert_eq!(got[0].net_cash_rev, Some(0.1));
        assert_eq!(got[0].ebitda, Some(280.0));
        assert_eq!(got[0].currency.as_deref(), Some("USD"), "(FX) money lines are meaningless without it");
        assert_eq!(got[0].net_debt, Some(-100.0), "a NEGATIVE net debt is net cash, not a missing value");

        // 2. every cached row unparseable -> None, NOT an empty Vec. `Some(vec![])` reads downstream
        //    as "SEC has no fundamentals for this filer" and pins the gap; None refetches.
        write(&[row("not-a-date", "2022-09-30")]);
        assert!(fetch_sec_facts_rows(&client, &urls, "CACHED").await.is_none());

        // 3. no cache + a ticker absent from the CIK map -> None before any request. This is the
        //    non-US path every ".DE"/"-USD" name takes on a wide screen, so it must not cost a socket.
        assert!(fetch_sec_facts_rows(&client, &urls, "NOSUCH").await.is_none());

        // 4. cold ticker that DOES resolve -> fetched, parsed, and written back to the cache. The
        //    write is the half with no other cover: drop it and every run refetches every ticker,
        //    which is silent until SEC starts rate-limiting.
        //
        //    The remove is load-bearing: the scratch root OUTLIVES the run, so on every rerun after
        //    the first this phase would take the phase-1 cache path, never open the socket, and pass
        //    without testing anything. A test that only works once is a test that works never.
        let _ = std::fs::remove_file(sec_cache_path("AAPL_facts10"));
        let (base, client) = stub_server(
            r#"{"facts": {"us-gaap": {"Revenues": {"units": {"USD": [
                {"start": "2020-10-01", "end": "2021-09-30", "val": 1000.0, "form": "10-K", "filed": "2021-11-01"}
            ]}}}}}"#,
        );
        let live = stub_urls(&base);
        let got = fetch_sec_facts_rows(&client, &live, "AAPL").await.expect("parsed rows");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].revenue, Some(1000.0));
        let written = std::fs::read_to_string(sec_cache_path("AAPL_facts10")).expect("cache written");
        assert!(written.contains("2021-09-30"), "the parse must be cached, not just returned: {written}");
    }

    /// `yahoo_fund_facts_fill`'s CACHE lane — the whole function bar the fetch itself, and the lane a
    /// real `screen` run spends almost all its time in once the weekly cache is warm.
    ///
    /// The two ISIN-cache writes at the end of `fetch_xetra_etfs`. Both guards return nothing and the
    /// function hands back only a ticker list, so the FILES are the only place they are observable —
    /// same stance as the SEC cache test above.
    ///
    /// Börse Frankfurt is pointed at the unbound port 1 deliberately, and that is what makes this
    /// test safe to run beside the others: a dead POST leaves `rows` empty, so the four name-keyed
    /// `OnceLock`s (`BF_TER_NAMES`, `BF_AUM_NAMES`, `BF_META_NAMES`, `BF_ROW_NAMES`) that
    /// `bf_ter_name_lookup` and `bf_meta_miss_buckets` already own are never written. Only
    /// `urls.yahoo_search` is redirected to the stub, so each phase spends exactly the ONE connection
    /// `stub_server` grants. The trailing `BF_TER`/`BF_AUM`/`BF_META`/`YH_TER`/`YH_AUM` sets do land,
    /// but only ever as empty maps here, which every reader treats identically to unset.
    ///
    /// Two phases, one test: they share both cache files under the scratch root, and that root
    /// OUTLIVES the run — so each phase deletes what it is about to assert on. Without that, the
    /// second run of the suite would grade files the first run left behind and pass on nothing.
    #[tokio::test]
    async fn fresh_resolutions_are_cached_and_definitive_misses_are_remembered() {
        pin_throttle();
        pin_fetch_concurrency(); // this call fans out — see the helper for why it is not optional
        let _ = YQ_AUTH.set(None); // yahoo_fund_facts_fill runs at the end; keep its crumb off the wire
        let pos = crate::config::data_path(ISIN_CACHE_PATH);
        let neg = crate::config::data_path(ISIN_NEG_CACHE_PATH);
        let stub_at = |body: &'static str| {
            let (base, client) = stub_server(body);
            let mut urls = stub_urls("http://127.0.0.1:1/"); // everything else refuses instantly
            urls.yahoo_search = format!("{base}?q={{ticker}}&quotesCount=0&newsCount=3");
            (urls, client)
        };

        // 1. A fresh resolution is remembered forever, and NOTHING is written to the negative cache:
        //    no ISIN was definitively missing, so that file must not even appear.
        let _ = std::fs::remove_file(&pos);
        let _ = std::fs::remove_file(&neg);
        let (urls, client) = stub_at(r#"{"quotes":[{"symbol":"POS.DE"}]}"#);
        let tickers = fetch_xetra_etfs(&client, &urls, 10, vec!["LU00POS".into()], vec![]).await;
        assert_eq!(tickers, ["POS.DE"]);
        let cached: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&pos).expect("positive cache written")).expect("json");
        assert_eq!(cached.get("LU00POS").map(String::as_str), Some("POS.DE"), "ISIN->symbol is stable, so it is kept");
        assert!(!neg.exists(), "nothing was definitively missing — the negative cache must stay absent");

        // 2. A search-shaped payload with no symbol IS definitive: remembered, dated today, for 30
        //    days. Clearing the positive cache first is what makes its guard observable in the
        //    direction phase 1 cannot reach — with no fresh resolution the file must not reappear.
        let _ = std::fs::remove_file(&pos);
        let (urls, client) = stub_at(r#"{"quotes":[]}"#);
        let tickers = fetch_xetra_etfs(&client, &urls, 10, vec![], vec!["LU00DEAD".into()]).await;
        assert!(tickers.is_empty(), "the search answered, with nothing in it: {tickers:?}");
        let dead: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&neg).expect("negative cache written")).expect("json");
        assert_eq!(dead.get("LU00DEAD"), Some(&chrono::Utc::now().date_naive().to_string()), "dated, so it can expire");
        assert!(!pos.exists(), "no fresh resolution — the positive cache must not be rewritten");
    }

    /// Seeding `YQ_AUTH` is a HERMETICITY guard, not a shortcut. `yahoo_crumb` hardcodes
    /// `https://fc.yahoo.com` and takes no `Urls`, so `stub_urls` cannot redirect it, and `offline()`
    /// is false in this binary — any symbol reaching the fetch list would open a REAL connection to
    /// Yahoo from a unit test. `YQ_AUTH` is read before that request, so a seeded `None` makes the
    /// handshake fail closed and the function returns whatever the cache already gave it. No other
    /// test here touches `YQ_AUTH`, and holding it makes the rest of this binary safer, not riskier.
    ///
    /// Owns `.fund_facts_cache.json` in the shared scratch root, hence one test rather than four.
    #[tokio::test]
    async fn yahoo_fund_facts_fill_serves_the_cache_for_bf_holes_only() {
        pin_throttle();
        let _ = YQ_AUTH.set(None); // fail the crumb handshake without a socket — see above
        let path = crate::config::data_path(FUND_FACTS_CACHE_PATH);
        let today = chrono::Utc::now().date_naive();
        let stale = today - chrono::Duration::days(30);
        let at_the_cap = today - chrono::Duration::days(FUND_CACHE_MAX_AGE_DAYS);
        let past_the_cap = today - chrono::Duration::days(FUND_CACHE_MAX_AGE_DAYS + 1);
        // Yahoo's cached numbers are deliberately DIFFERENT from the BF ones below, so a leak across
        // the firewall shows up as the wrong value rather than as a passing coincidence.
        std::fs::write(
            &path,
            serde_json::json!({
                "HOLE.DE":     [today.to_string(), 0.20, 5.0e8],
                "BFHAS.DE":    [today.to_string(), 0.30, 1.0e9],
                "HALFBF.DE":   [today.to_string(), 0.40, 7.0e8],
                "AUMONLY.DE":  [today.to_string(), 0.25, 2.5e8],
                "STALE.DE":    [stale.to_string(), 0.99, 9.9e9],
                "ATCAP.DE":    [at_the_cap.to_string(), 0.77, 7.7e8],
                "PASTCAP.DE":  [past_the_cap.to_string(), 0.88, 8.8e8],
                "CORRUPT.DE":  ["not-a-date", 0.66, 6.6e8],
                "TERONLY.DE":  [today.to_string(), 0.15, serde_json::Value::Null],
            })
            .to_string(),
        )
        .expect("seed fund-facts cache");

        let client = Client::builder().no_proxy().build().expect("test client");
        // HALFBF.DE is in bf_ter but NOT bf_aum — the only shape that separates the two guards, see
        // the assertions below.
        let bf_ter: HashMap<String, f64> =
            [("BFHAS.DE".to_string(), 0.11), ("HALFBF.DE".to_string(), 0.12)].into_iter().collect();
        // AUMONLY.DE is HALFBF.DE's mirror — BF holds its AUM but not its TER. Both halves are needed:
        // each of the two `!`s in the short-circuit is invisible against the other fund alone.
        let bf_aum: HashMap<String, f64> =
            [("BFHAS.DE".to_string(), 2.0e9), ("AUMONLY.DE".to_string(), 3.0e9)].into_iter().collect();
        let syms: Vec<String> = [
            "HOLE.DE",
            "BFHAS.DE",
            "HALFBF.DE",
            "AUMONLY.DE",
            "STALE.DE",
            "ATCAP.DE",
            "PASTCAP.DE",
            "CORRUPT.DE",
            "TERONLY.DE",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        // Unroutable stub + an EMPTY isin map: the justETF fallback this test now falls through into
        // must not reach a real host, and with no ISIN for any symbol it has nothing to ask for anyway.
        // Both belts on purpose — `justetf_profile` defaults to justETF's live URL.
        let urls = stub_urls("http://127.0.0.1:1/none");
        let isin_of: HashMap<String, String> = HashMap::new();
        let (ter, aum) = yahoo_fund_facts_fill(&client, &urls, &syms, &bf_ter, &bf_aum, &isin_of).await;

        // a BF hole with a fresh entry -> filled from cache, no request
        assert_eq!(ter.get("HOLE.DE"), Some(&0.20));
        assert_eq!(aum.get("HOLE.DE"), Some(&5.0e8));

        // BF answered BOTH -> Yahoo is never consulted for it. This is the scoring firewall the
        // function exists to hold: `expense_ratio`/`aum_eur` feed the score and the AUM gate, Yahoo
        // facts are display + H/CORE only. A first merged run moved live ranks (PEA 3->9, 7.3->6.8).
        assert!(!ter.contains_key("BFHAS.DE"), "a BF-answered fund must not pick up Yahoo's TER");
        assert!(!aum.contains_key("BFHAS.DE"), "nor its AUM");

        // BF answered ONE side only. This is the shape that makes the firewall observable at all:
        // a fund BF fully answers is skipped wholesale by the `continue` above, so the per-field
        // guards never run on it and either guard alone looks sufficient. Here both run — the TER
        // must stay BF's and the AUM must come from Yahoo, in the SAME call.
        assert!(!ter.contains_key("HALFBF.DE"), "BF holds this TER — Yahoo's 0.40 must not override it");
        assert_eq!(aum.get("HALFBF.DE"), Some(&7.0e8), "but the AUM hole must still be filled");

        // ...and the mirror: BF holds the AUM, so the TER hole is Yahoo's to fill. Without this fund
        // the first `!` of the short-circuit is free — deleting it only changes what happens to a
        // fund BF answered on the TER side, which HALFBF.DE is not.
        assert_eq!(ter.get("AUMONLY.DE"), Some(&0.25), "BF has no TER for it — the hole must be filled");
        assert!(!aum.contains_key("AUMONLY.DE"), "but BF holds this AUM — Yahoo's must not override it");

        // far past the cap -> NOT served; it goes to the fetch list, which fails closed here. A stale
        // TER served as if current is a wrong number that never expires.
        assert!(!ter.contains_key("STALE.DE"), "a cache entry past the cap must not be served");
        assert!(!aum.contains_key("STALE.DE"));

        // (fund staleness) THE BOUNDARY, pinned from both sides. Yahoo's handshake is dead upstream, so
        // this comparison is the only thing standing between "last week's TER, silently" and `n/a` —
        // and the 30-day fund above cannot pin it, because every widening of the comparison still
        // rejects that one. At the cap the row is SERVED (the point of the whole change: a run with no
        // network still fills its cells); one day past it the row is dropped.
        assert_eq!(ter.get("ATCAP.DE"), Some(&0.77), "exactly at the cap is still inside it");
        assert_eq!(aum.get("ATCAP.DE"), Some(&7.7e8));
        assert!(!ter.contains_key("PASTCAP.DE"), "one day past the cap is out");
        assert!(!aum.contains_key("PASTCAP.DE"));

        // an unparseable stamp is treated as unusable, NOT as infinitely fresh — the failure mode a
        // bare `parse().ok()` plus a `<` comparison would otherwise invite.
        assert!(!ter.contains_key("CORRUPT.DE"), "a corrupt date must not be served");
        assert!(!aum.contains_key("CORRUPT.DE"));

        // fresh, TER present, AUM absent -> the half that exists fills and the half that does not
        // stays ABSENT. A missing AUM defaulting to 0 would read as a fund below every size gate.
        assert_eq!(ter.get("TERONLY.DE"), Some(&0.15));
        assert!(!aum.contains_key("TERONLY.DE"), "a missing AUM must stay missing, never 0");
    }

    /// `enrich_fund_factor`'s per-quote lane, opening NO socket. `fetch_fundamentals_history` treats a
    /// seeded `.fmp_cache/{ticker}.json` as a hit — no key, no budget, no request — and FMP rows carry
    /// no reporting currency, so `fund_ccy` is None, the price join takes `close_native` untouched and
    /// the FX lookup never fires either. The default `growth_fund_factor` ("rev_accel") is neither
    /// `insider_net_buys_90d` nor `earnings_yield`, so those two conditional fetches stay shut as well.
    ///
    /// BOTH source caches are seeded, and that is the point rather than belt-and-braces:
    /// `fetch_fundamentals_ranked` routes on `config::fund_source()`, which reads the operator's
    /// GITIGNORED `settings.yaml` — "sec" on this box, the "fmp" default on CI, and a process-wide
    /// `OnceLock` either way. Seeding one cache writes a test that passes on whichever machine
    /// happens to agree with it. Both paths are made to yield the same eps, so the assertions below
    /// hold under either routing.
    ///
    /// Owns `.fmp_cache` and the AAA/BBB entries of `.sec_cache` in the shared scratch root.
    #[tokio::test]
    async fn enrich_fund_factor_skips_currency_quotes_and_divides_by_the_native_close() {
        pin_throttle();
        seed_cik_map(); // this test reaches `sec_cik` first under a cold scratch root — see the helper
        let fmp = crate::config::data_path(".fmp_cache");
        let sec = crate::config::data_path(".sec_cache");
        std::fs::create_dir_all(&fmp).expect("scratch .fmp_cache");
        // `fund_factors` takes `eps_ttm` from the NEWEST row's eps, so one row settles the arithmetic.
        let filed = chrono::Local::now().date_naive() - chrono::Duration::days(30);
        let rows = format!(
            r#"[{{"filingDate":"{filed}","date":"{filed}","revenue":1000.0,"grossProfit":400.0,"operatingIncome":200.0,"netIncome":150.0,"eps":4.0,"weightedAverageShsOutDil":50.0}}]"#
        );
        let sec_rows = serde_json::to_string(&[SecCacheRow {
            filed: filed.to_string(),
            period_end: filed.to_string(),
            revenue: Some(1000.0),
            gross_margin: Some(40.0),
            op_margin: Some(20.0),
            net_margin: Some(15.0),
            eps: Some(4.0), // the same eps the FMP row carries — see the note above
            roe: Some(15.0),
            shares: Some(50.0),
            fcf_margin: Some(20.0),
            interest_cover: Some(4.0),
            net_cash_rev: Some(0.1),
            ebitda: Some(280.0),
            net_debt: Some(-100.0),
            currency: None, // as for the FMP rows: unknown books currency -> no FX lookup, no socket
            roa: Some(5.0),
            prior_eps: Some(2.0),
            prior_shares: Some(52.0),
        }])
        .expect("serialise sec rows");
        for t in ["AAA", "BBB"] {
            std::fs::write(fmp.join(format!("{t}.json")), &rows).expect("seed fmp cache");
            std::fs::write(sec.join(format!("{t}_facts10.json")), &sec_rows).expect("seed sec cache");
            // The SEC roll-forward TTM sidecar, deliberately DISAGREEING with the 4.0 above. It is
            // consulted only when the selected factor is `earnings_yield`, and the default is
            // `rev_accel` — so 9.0 must never reach `eps_ttm` below. Inverting that gate would let
            // the TTM roll override the annual EPS for every factor, which is a train-serve skew:
            // the live tilt would score on a number the backtest cannot reconstruct as-of.
            std::fs::write(sec.join(format!("{t}_ttmeps3.json")), "9.0").expect("seed ttm sidecar");
        }

        let urls = stub_urls("http://127.0.0.1:1/"); // port 1 is unbound: any stray fetch refuses at once
        let client = Client::builder().no_proxy().build().expect("test client");
        let tuning = crate::config::BuyHeuristic::default();

        let mut quotes: Vec<core::Quote> = ["AAA", "BBB", "BTC-EUR", "BRK-B"]
            .iter()
            .map(|t| core::Quote::stub(t, "1.00", "", t))
            .collect();
        quotes[0].close_native = Some(100.0);
        quotes[1].close_native = Some(200.0); // identical statements, twice the price
        quotes[2].close_native = Some(100.0);
        quotes[3].close_native = Some(100.0);

        enrich_fund_factor(&client, &urls, &mut quotes, &tuning).await;

        // 1. the same statements against twice the price must HALVE the earnings yield. Asserting the
        //    ratio rather than a literal pins the wiring — that the divisor is the native close — with
        //    out restating `core::earnings_yield`'s formula here, where it would agree by construction.
        let yield_of = |q: &core::Quote| q.fund.as_ref().and_then(|f| f.earnings_yield);
        let (a, b) = (yield_of(&quotes[0]).expect("AAA yield"), yield_of(&quotes[1]).expect("BBB yield"));
        assert!((a - 2.0 * b).abs() < 1e-9, "twice the price must halve the yield: {a} vs {b}");
        assert_eq!(quotes[0].fund.as_ref().and_then(|f| f.eps_ttm), Some(4.0), "the newest row's eps");

        // 2. a currency-quoted ticker is skipped WHOLE — no budget slot spent probing an instrument
        //    that has no income statement, and `fund` stays None rather than becoming an empty struct.
        assert!(quotes[2].fund.is_none(), "crypto has no income statement to enrich");
        assert!(quotes[2].fund_factor.is_none());

        // 3. ...and a SHARE CLASS is not. `is_currency_quoted` is a suffix check; a `contains('-')`
        //    here silently dropped BRK-B/BF-B from the very factor the backtest validated them with.
        //    That is a train-serve skew which reads as "this name has no fundamentals", not as a bug.
        assert!(quotes[3].fund.is_some(), "BRK-B is a share class, not a currency pair");
    }

    /// `LIVE_TTL` is a WEEKLY refetch, and every mutant on its `7 * 24 * 3600` shortens it: `7 + 24 *
    /// 3600` is about a day, `7 * 24 + 3600` about an hour, and both `/` forms integer-divide to 0,
    /// which evicts unconditionally. So the load-bearing assertion here is the one that KEEPS a
    /// three-day-old cache — every shortened TTL throws it away, and only the real week keeps it.
    ///
    /// It matters because the eviction is silent and self-healing in the wrong direction: a TTL of
    /// one hour still produces correct rankings, it just refetches every name on every run, which on
    /// a live key is how a screen quietly burns its whole daily budget on data it already had.
    ///
    /// `evict_if_stale` reads the file's mtime, so the ages are set with `File::set_modified` rather
    /// than waiting a week. Uses its own tickers so it never races the test above over `.fmp_cache`.
    #[tokio::test]
    async fn the_live_fundamentals_cache_is_evicted_after_a_week_not_a_day() {
        pin_throttle();
        std::fs::create_dir_all(crate::config::data_path(".fmp_cache")).expect("scratch .fmp_cache");
        let filed = chrono::Local::now().date_naive() - chrono::Duration::days(30);
        let rows = format!(
            r#"[{{"filingDate":"{filed}","date":"{filed}","revenue":1000.0,"grossProfit":400.0,"operatingIncome":200.0,"netIncome":150.0,"eps":4.0,"weightedAverageShsOutDil":50.0}}]"#
        );
        let aged = |t: &str, days: u64| {
            let p = fund_cache_path(t);
            std::fs::write(&p, &rows).expect("seed live cache");
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .expect("reopen to age")
                .set_modified(SystemTime::now() - StdDuration::from_secs(days * 24 * 3600))
                .expect("backdate mtime");
            p
        };
        let fresh = aged("TTLFRESH", 3); // inside the week — must survive
        let stale = aged("TTLSTALE", 8); // past it — must be dropped

        // The insider lane has its OWN eviction, reached only when the selected factor IS
        // `insider_net_buys_90d`. The default is not, so this long-stale file must go untouched:
        // inverting that gate makes every run evict the SEC cache for every name, which is invisible
        // in the output and just refetches the world. `.sec_cache/<ticker>.json` is the insider
        // sidecar — `fetch_fundamentals_ranked` reads `<ticker>_facts10.json`, a different file.
        std::fs::create_dir_all(crate::config::data_path(".sec_cache")).expect("scratch .sec_cache");
        let insider = sec_cache_path("TTLSTALE");
        std::fs::write(&insider, "[]").expect("seed insider sidecar");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&insider)
            .expect("reopen to age")
            .set_modified(SystemTime::now() - StdDuration::from_secs(60 * 24 * 3600))
            .expect("backdate mtime");

        let urls = stub_urls("http://127.0.0.1:1/"); // port 1 is unbound: the refetch refuses at once
        let client = Client::builder().no_proxy().build().expect("test client");
        let mut quotes: Vec<core::Quote> =
            ["TTLFRESH", "TTLSTALE"].iter().map(|t| core::Quote::stub(t, "1.00", "", t)).collect();
        enrich_fund_factor(&client, &urls, &mut quotes, &crate::config::BuyHeuristic::default()).await;

        assert!(fresh.exists(), "three days is INSIDE the weekly TTL — any shorter one evicts this");
        assert!(!stale.exists(), "eight days is past it — a stale newest-quarter must be dropped");
        assert!(insider.exists(), "the insider lane is not selected — its cache must not be touched at all");
    }

    /// The round-36 rule, asserted where it is actually enforced: a venue outage must NEVER shrink
    /// the universe. Every fetcher here is pointed at an unbound port, which is the outage — and the
    /// last-good `.venue_isins.json` must come through it BYTE-IDENTICAL. A run that overwrote it
    /// with the empty result would leave the ETF tables permanently short with no error anywhere:
    /// the screen still prints, the names are simply gone, and the next run inherits the damage.
    ///
    /// Asserted on the FILE rather than on the return value on purpose. The recovered ISINs feed
    /// `fetch_xetra_etfs`, which is also down here, so nothing about the fallback reaches the
    /// returned universe — the persisted store is the only place the rule is observable without a
    /// working Börse Frankfurt.
    ///
    /// Note for whoever adds the next test in this binary: `fetch_xetra_etfs` runs as part of this
    /// call and its five trailing `OnceLock`s (`BF_TER`, `BF_AUM`, `BF_META`, `YH_TER`, `YH_AUM`) are
    /// set UNCONDITIONALLY, so after this test they hold empty maps for the rest of the process. No
    /// committed test reads them today. The name-keyed lists are safe: those `.set()`s sit inside the
    /// POST-success branch, which a dead venue never reaches, so `bf_ter_name_lookup` and
    /// `bf_meta_miss_buckets` keep ownership of theirs.
    #[tokio::test]
    async fn a_venue_outage_never_shrinks_the_stored_universe() {
        pin_throttle();
        pin_fetch_concurrency(); // this call fans out — see the helper for why it is not optional
        let store = crate::config::data_path(VENUE_ISINS_PATH);
        let seeded = r#"{"euronext":["LU0378437502","IE00B4L5Y983"],"six":["CH0044328745"]}"#;
        std::fs::write(&store, seeded).expect("seed venue store");

        // every venue down: port 1 is privileged and unbound, so each request refuses at once and
        // the whole call returns in milliseconds instead of waiting out a timeout.
        let urls = stub_urls("http://127.0.0.1:1/");
        let client = Client::builder().no_proxy().build().expect("test client");

        let (universe, etf_set, sectors) = fetch_universe(&client, &urls, 10, true, false, &[]).await;

        let after = std::fs::read_to_string(&store).expect("store must survive the outage");
        assert_eq!(after, seeded, "a dead venue must not overwrite the last-good ISIN list");
        // and the degraded run reports nothing rather than something wrong
        assert!(universe.is_empty(), "no venue answered, so no universe: {universe:?}");
        assert!(etf_set.is_empty());
        assert!(sectors.is_empty());

        // 2. ONE leg alive -> the names it found actually come back. Without this the assertions above
        //    are satisfied by a `fetch_universe` that returns empty unconditionally, which is exactly
        //    the mutant the audit found surviving: a total-outage test cannot tell a working universe
        //    builder from one that has stopped building. Only CoinGecko is pointed at the live stub —
        //    every other leg stays on the dead port on purpose, because a live Börse Frankfurt reply
        //    would reach the POST-success branch and claim the name-keyed `OnceLock`s this binary's
        //    other tests own (see the note above).
        let (base, client) = stub_server(r#"[{"symbol":"btc"},{"symbol":"eth"}]"#);
        let mut urls = stub_urls("http://127.0.0.1:1/");
        urls.coingecko_markets = base;
        let (universe, _, _) = fetch_universe(&client, &urls, 10, true, false, &[]).await;
        assert_eq!(universe, ["BTC-EUR", "ETH-EUR"], "the one answering leg must reach the caller");
    }

    /// The macro series cache trio (CPI/HICP), untested until the scratch root existed. A same-day
    /// copy is what keeps repeated `screen` runs under the keyless request caps, so a silently broken
    /// cache does not fail — it just starts getting throttled.
    ///
    /// Its own filename, because the scratch root is shared with every other test in this binary.
    #[test]
    fn macro_cache_round_trips_and_reports_freshness() {
        let v = serde_json::json!({"Results": {"series": [{"data": [{"year": "2025"}]}]}});
        macro_cache_write("test_macro_trio", &v);
        assert_eq!(macro_cache_read("test_macro_trio"), Some(v));
        assert!(macro_cache_fresh("test_macro_trio"), "just written, so inside the TTL");
        assert!(!macro_cache_fresh("test_macro_absent"), "a missing file is not fresh");
        assert_eq!(macro_cache_read("test_macro_absent"), None);
    }

    /// Both cache paths fold `/` and `\` into the FILENAME. Without that a ticker is a path: the cache
    /// key comes from a fetched constituent list, so `../../x` would write outside the cache directory
    /// on a machine that never typed it. Nothing covered this pair at all.
    ///
    /// Asserted on `file_name()` only — `config::data_path` anchors to whichever repo root it finds,
    /// which differs between a dev box and CI, and the guard under test is the replacement, not the
    /// anchor. `data_path` itself never calls `config::load()`, so this is safe with no settings.yaml.
    #[test]
    fn cache_paths_sanitise_path_separators() {
        let name = |p: std::path::PathBuf| p.file_name().expect("filename").to_string_lossy().into_owned();
        assert_eq!(name(fund_cache_path("BRK/B")), "BRK_B.json");
        assert_eq!(name(sec_cache_path("BRK/B")), "BRK_B.json");
        assert_eq!(name(fund_cache_path(r"A\B")), "A_B.json");
        assert_eq!(name(sec_cache_path(r"A\B")), "A_B.json");
        // the traversal the replacement exists for: no parent-directory hop survives it
        assert_eq!(name(sec_cache_path("../../etc/passwd")), ".._.._etc_passwd.json");
    }

    /// Three lines, and the whole crypto NUPL gate reads through them.
    #[tokio::test]
    async fn fetch_nupl_reads_the_field() {
        let (base, client) = stub_server(r#"{"nupl": 0.42}"#);
        assert_eq!(fetch_nupl(&client, &stub_urls(&base)).await, Some(0.42));
    }

    /// At most three titles, and an item carrying its title under `content` counts as one — the shape
    /// Yahoo switched to, which is why `headline_titles` looks in both places.
    #[tokio::test]
    async fn fetch_news_takes_three_titles() {
        let (base, client) = stub_server(
            r#"{"news":[{"title":"one"},{"content":{"title":"two"}},{"title":"three"},{"title":"four"}]}"#,
        );
        assert_eq!(fetch_news(&client, &stub_urls(&base), "TST").await, ["one", "two", "three"]);
    }

    /// The intraday close array out of Yahoo's nested shape, nulls dropped.
    #[tokio::test]
    async fn intraday_closes_reads_the_close_array() {
        let (base, client) =
            stub_server(r#"{"chart":{"result":[{"indicators":{"quote":[{"close":[1.5,null,2.5]}]}}]}}"#);
        assert_eq!(intraday_closes(&client, &stub_urls(&base), "TST").await, Some(vec![1.5, 2.5]));
    }

    /// The as-of FX series on the FIRST pair tried (`{CUR}EUR=X`, uninverted), so the rate lands as
    /// quoted. One stubbed connection is the assertion, not a limitation: the second pair is
    /// unreachable, which proves the first hit short-circuits the loop instead of always trying both.
    ///
    /// No `adjclose` in the payload on purpose — `parse_chart` prefers it when `use_adjusted_close` is
    /// on, and that knob is config-dependent, so including it would make this test read differently on
    /// a dev box than in CI.
    #[tokio::test]
    async fn eur_rate_series_maps_dates_to_rates() {
        let (base, client) = stub_server(
            r#"{"chart":{"result":[{"timestamp":[1577836800,1577923200],"indicators":{"quote":[{"close":[0.9,0.0]}]}}]}}"#,
        );
        let got = eur_rate_series(&client, &stub_urls(&base), "USD", false).await.expect("series");
        assert_eq!(got.len(), 1, "a 0.0 close is dropped — inverted it would be +inf: {got:?}");
        assert_eq!(got[&NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()], 0.9);
    }

    /// USD 10.00 at 0.5 EUR/USD is EUR 5.00. One stubbed response covers it because the EUR leg
    /// returns 1.0 without fetching — the short-circuit that keeps the common path off the network.
    #[tokio::test]
    async fn price_in_converts_via_eur() {
        let (base, client) = stub_server(
            r#"{"chart":{"result":[{"timestamp":[1577836800],"indicators":{"quote":[{"close":[0.5]}]}}]}}"#,
        );
        let got = price_in(&client, &stub_urls(&base), &fx_cache(), 10.0, "USD", "EUR").await;
        assert_eq!(got, Some(5.0));
    }

    /// One CSV pond, header skipped, sector filter applied. `constituents_csv` is emptied so the
    /// single-shot stub is the only endpoint in play — with it populated the second leg would fail and
    /// print an unavailability warning that says nothing about the parse being tested.
    #[tokio::test]
    async fn constituent_ponds_parses_a_csv_pond() {
        let (base, client) = stub_server("Symbol,Name,Sector\nAAPL,Apple,Technology\nXOM,Exxon,Energy");
        let mut urls = stub_urls(&base);
        urls.constituents_csv.clear();
        let ponds = constituent_ponds(&client, &urls, &["Technology".to_string()]).await;
        assert_eq!(ponds.len(), 1, "one endpoint served -> one pond");
        assert_eq!(ponds[0], [("AAPL".to_string(), "Technology".to_string())], "Energy filtered out");
    }

    /// `sector_map` flattens those ponds ticker -> sector.
    #[tokio::test]
    async fn sector_map_flattens_the_ponds() {
        let (base, client) = stub_server("Symbol,Name,Sector\nAAPL,Apple,Technology");
        let mut urls = stub_urls(&base);
        urls.constituents_csv.clear();
        let map = sector_map(&client, &urls, &["Technology".to_string()]).await;
        assert_eq!(map.get("AAPL").map(String::as_str), Some("Technology"));
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

/// The parsed `Chart` for one ticker — the 10y daily series, for the `backtest` command. Same single
/// chart call the live path already makes (no EXTRA per-ticker fetch). None on fetch/parse fail or empty
/// history.
///
/// Returns the whole `Chart` rather than the `(dates, closes, currency)` triple it used to reduce to,
/// because `name`/`instrument_type` are what let the backtest class a name at all (see `Chart`).
///
/// (FX) the currency rides along because the closes alone are ambiguous: a foreign filer's ADR trades in
/// one currency and reports in another, so joining these closes to SEC per-share lines needs proof both
/// sides match FIRST. Price-only callers ignore the field.
pub async fn fetch_history(client: &Client, urls: &Urls, ticker: &str) -> Option<Chart> {
    let j = chart_json(client, urls, ticker, "10y").await?;
    let chart = parse_chart(&j, ticker)?;
    if chart.closes.is_empty() {
        return None;
    }
    Some(chart)
}

/// The parsed `Chart` from the MAX monthly history — the long-horizon `backtest` path. Decades of
/// monthly bars (vs fetch_history's 10y daily), so forward windows of 10y+ exist for old names and a
/// genuine multi-decade hold can be measured. Same single chart call quote_one already makes for the 20Y
/// backfill (no new fetch type). None on fetch/parse fail or empty history.
pub async fn fetch_history_long(client: &Client, urls: &Urls, ticker: &str) -> Option<Chart> {
    let json = chart_json_long(client, urls, ticker).await?;
    let chart = parse_chart_raw(&json, ticker)?;
    if chart.closes.is_empty() {
        return None;
    }
    Some(chart)
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
    // (round 51) one write per run: persist the tickers whose monthly fetch proved useless above.
    long_skip_save();
    // (round 53) one write per run: persist this run's fresh monthly payloads.
    long_cache_save();
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
/// Disk cache for slow-moving macro series (monthly CPI/HICP): a same-day copy skips the network,
/// and on a live failure (throttle/outage) any older copy still beats an ERROR row in the footer.
/// Born from BLS: its keyless cap is 25 req/day per SHARED IP, so repeated screen runs redden USA.
const MACRO_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
fn macro_cache_path(name: &str) -> std::path::PathBuf {
    crate::config::data_path(".fmp_cache").join(format!("{name}.json"))
}
fn macro_cache_read(name: &str) -> Option<Value> {
    std::fs::read_to_string(macro_cache_path(name)).ok().and_then(|s| serde_json::from_str(&s).ok())
}
fn macro_cache_fresh(name: &str) -> bool {
    std::fs::metadata(macro_cache_path(name))
        .ok()
        .and_then(|m| m.modified().ok())
        .is_some_and(|t| t.elapsed().is_ok_and(|e| e < MACRO_TTL))
}
fn macro_cache_write(name: &str, v: &Value) {
    let _ = std::fs::create_dir_all(crate::config::data_path(".fmp_cache"));
    let _ = std::fs::write(macro_cache_path(name), v.to_string());
}

/// GET + parse a macro series through the day-fresh cache; "parsed non-empty" is the success test
/// (a throttled reply can be valid JSON with empty results), so a dud is never cached and falls
/// back to the stale copy.
async fn cached_macro<F: Fn(&Value) -> BTreeMap<i32, f64>>(client: &Client, url: &str, name: &str, parse: F) -> BTreeMap<i32, f64> {
    if macro_cache_fresh(name) {
        if let Some(m) = macro_cache_read(name).map(|d| parse(&d)).filter(|m| !m.is_empty()) {
            return m;
        }
    }
    if let Some(d) = get_json(client, url).await {
        let m = parse(&d);
        if !m.is_empty() {
            macro_cache_write(name, &d);
            return m;
        }
    }
    macro_cache_read(name).map(|d| parse(&d)).unwrap_or_default()
}

/// (us 20Y) Splice the permanent old-decade window's raw index rows onto the fresh window's
/// payload so ONE parse sees continuous (year, month) LEVELS. Rates need each year's
/// predecessor level, so merging parsed RATES instead would silently drop the cross-window
/// year. `old` absent/shapeless → `new` unchanged.
fn merge_bls_payloads(old: Option<&Value>, new: &Value) -> Value {
    let path = "/Results/series/0/data";
    let (Some(o), Some(n)) = (
        old.and_then(|v| v.pointer(path)).and_then(Value::as_array),
        new.pointer(path).and_then(Value::as_array),
    ) else {
        return new.clone();
    };
    let mut data = o.clone();
    data.extend(n.iter().cloned());
    serde_json::json!({"Results": {"series": [{"data": data}]}})
}

/// (us 20Y) Year-hole guard: a cached old window is valid only while it still yields the rate
/// for year now-10, the year ADJACENT to the fresh (now-9..now) window. As the calendar rolls,
/// an aging cache would leave a missing level year in between and the merged map would silently
/// compound a too-short "20Y" — an UNDERSTATED number, worse than n/a. False → refetch the old
/// window (one extra call per calendar YEAR).
fn old_window_covers(old: &Value, now: i32) -> bool {
    core::parse_bls_cpi(old).contains_key(&(now - 10))
}

pub async fn fetch_us_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    use chrono::Datelike;
    // POST-based (year window in the body), so it drives the macro cache by hand instead of cached_macro.
    let now = chrono::Utc::now().year();
    let key = std::env::var("BLS_API_KEY").ok().filter(|k| !k.is_empty());
    // (us 20Y) Second, PERMANENT old-decade window (now-19..now-10): the keyless v1 API caps at
    // 10 years/call, so the fresh window alone yields ~9 annual rates and the 20Y column starved
    // at n/a. Unadjusted CPI-U (CUUR0000SA0) history never changes, so this window is fetched
    // once and cached with NO TTL — presence + old_window_covers = valid; steady state stays ONE
    // call/day (the retired 3-call keyless design re-paid its whole budget daily and exhausted
    // the shared-IP 25/day cap). A cold cache or a new calendar year costs one extra call; a
    // failed old fetch just leaves today's behavior (20Y n/a) until a later run heals it. The
    // keyed v2 path (20y/call) never fetches it but still merges a present copy.
    let mut old = macro_cache_read("us_cpi_old").filter(|d| old_window_covers(d, now));
    if old.is_none() && key.is_none() {
        let mut b = serde_json::Map::new();
        b.insert("seriesid".into(), serde_json::json!(["CUUR0000SA0"]));
        b.insert("startyear".into(), (now - 19).to_string().into());
        b.insert("endyear".into(), (now - 10).to_string().into()); // 10 years inclusive: at the v1 cap
        if let Some(d) = post_json(client, &urls.us_cpi, &serde_json::Value::Object(b)).await {
            if !core::parse_bls_cpi(&d).is_empty() {
                macro_cache_write("us_cpi_old", &d);
                old = Some(d);
            }
        }
    }
    let finish = |new_payload: &Value| core::parse_bls_cpi(&merge_bls_payloads(old.as_ref(), new_payload));
    if macro_cache_fresh("us_cpi") {
        if let Some(m) = macro_cache_read("us_cpi").map(|d| finish(&d)).filter(|m| !m.is_empty()) {
            return m;
        }
    }
    // BLS year windows are honored only via POST (seriesid in the body, base /data/ URL). Keyless
    // v1: 25 requests/DAY (shared per-IP), 10 years/call — the fresh window covers 5Y/10Y and,
    // merged with the old-decade cache, the 20Y. Set BLS_API_KEY (free, instant signup at
    // data.bls.gov/registrationEngine) to use v2: 500 req/day and 20y/call in one request.
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
    // a throttled BLS reply is still valid JSON with empty Results, so "parsed non-empty" is the
    // success test (on the NEW payload alone — never cache a dud), and the stale fallback still
    // beats an empty map. Every return path parses the old-window MERGE, not the bare payload.
    if let Some(d) = post_json(client, &url, &serde_json::Value::Object(body)).await {
        if !core::parse_bls_cpi(&d).is_empty() {
            macro_cache_write("us_cpi", &d);
            return finish(&d);
        }
    }
    macro_cache_read("us_cpi").map(|d| finish(&d)).unwrap_or_default()
}

/// {year -> annual CPI %} for Portugal from Banco de Portugal (series 5721550), each
/// year = its last available month. JSON-stat: value list parallels the date index.
pub async fn fetch_pt_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    // index is a JSON array; parse lives in core (tested)
    cached_macro(client, &urls.pt_cpi, "pt_cpi", core::parse_pt_series).await
}

/// {year -> annual HICP %} for the EU27 from Eurostat, each year = its last month.
/// Eurostat TERMINATED prc_hicp_manr at 2025-12 (COICOP-2018 migration, Feb 2026) — the old
/// endpoint keeps serving a frozen series with a live-looking update stamp, which silently
/// pinned "latest EU inflation" to 2025. The successor prc_hicp_minr (same JSON-stat shape;
/// all-items now coicop18=TOTAL, rate unit=RCH_A) carries 2000→now, and the frozen dataset is
/// merged UNDER it for the 1997-1999 tail so the 30y average keeps its full window. Cache key
/// bumped eu_hicp -> eu_hicp2 so a pre-switch day-fresh cache can't mask the heal.
pub async fn fetch_eu_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    let (old, new) = tokio::join!(
        cached_macro(client, &urls.eu_hicp_old, "eu_hicp_old", core::parse_eurostat_hicp),
        cached_macro(client, &urls.eu_hicp, "eu_hicp2", core::parse_eurostat_hicp),
    );
    core::merge_infl_archive(old, new)
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

/// Push a notification to the configured ntfy URL (`{topic}` filled in). Returns whether ntfy
/// accepted it (POST sent AND 2xx) — a dropped push is the one thing `alert` exists to deliver,
/// so the caller must get the chance to say so. No retry: alert runs from an hourly cron, the
/// next run re-fires anything still dipping.
pub async fn push(client: &Client, urls: &Urls, topic: &str, title: &str, msg: &str) -> bool {
    client
        .post(urls.ntfy.replace("{topic}", topic))
        .header("Title", title)
        .header("Tags", "chart_with_downwards_trend")
        .body(msg.to_string())
        .send()
        .await
        .is_ok_and(|resp| resp.status().is_success())
}
