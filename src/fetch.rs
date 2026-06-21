//! All network I/O for folioman. One shared `reqwest::Client` (keep-alive pool,
//! HTTP/2, gzip) drives every request; fan-out is async via `join_all`. Every fetch
//! fails soft — a bad ticker or a down API yields a fallback/err row, never a crash.
//! Universe-scale fan-out is concurrency-bounded (`FETCH_CONCURRENCY`) so it can't 429-storm.
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
use std::time::Duration as StdDuration;
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
    client.get(url).send().await.ok()?.json::<Value>().await.ok()
}

async fn get_text(client: &Client, url: &str) -> Option<String> {
    client.get(url).send().await.ok()?.text().await.ok()
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
    // One retry: under the wide screen fan-out Yahoo 429s/times-out random names — either no response
    // (None) or a rate-limit body that parses to JSON but carries no chart.result. Both drop the name to
    // an err stub that then vanishes from the universe (NVDA disappeared this way). A single backoff+retry
    // recovers them. ponytail: fixed 400ms, one extra try — bump if drops persist under heavier load.
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
    let closes_arr = quote0.get("close")?.as_array()?;
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
pub async fn quote_one(client: &Client, urls: &Urls, fx: &FxCache, ticker: &str, dip_days: i64, high_days: i64, intraday: bool, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Quote {
    let (chart_j, chart_long_j, titles, intra, pe, roe) = tokio::join!(
        // 10y, NOT max: Yahoo coarsens interval=1d to monthly bars once the span passes ~10y, which
        // makes 1D/1W/1M meaningless (only month-boundary points exist). 10y keeps TRUE daily bars
        // (~3652) for 1D..10Y, plus turnover/SMA/range/R². The pre-10y span (the 20Y column) is
        // back-filled from the separate monthly series below.
        chart_json(client, urls, ticker, "10y"),
        // monthly full history, ONLY to back-fill the >10y horizons (20Y) without breaking the daily ones.
        chart_json_long(client, urls, ticker),
        fetch_news(client, urls, ticker),
        async { if intraday { intraday_closes(client, urls, ticker).await } else { None } },
        // (E) trailing P/E for the valuation tilt — equities only (crypto/FX have no earnings); a
        // no-op (instant None) unless FMP_API_KEY is set, so the default path stays network-free here.
        async { if ticker.contains('-') { None } else { fetch_pe(client, urls, ticker).await } },
        // (F) trailing ROE for the quality tilt — equities only; instant None unless FMP_API_KEY is set.
        async { if ticker.contains('-') { None } else { fetch_roe(client, urls, ticker).await } },
    );

    let chart = match chart_j.as_ref().and_then(|j| parse_chart(j, ticker)) {
        Some(c) => c,
        None => return Quote::stub(ticker, "err", "", ticker),
    };
    if chart.closes.is_empty() {
        return Quote::stub(ticker, "no data", "", &chart.name);
    }

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
    let rate = eur_rate(client, urls, &chart.currency, fx).await;
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
        // (A) percentile rank of today's price in its OWN ~10y history; picks discount = 100-this.
        // Self-normalizes amplitude so BTC-near-its-range-top and a deep alt don't both peg the cap.
        range_pct: core::price_pct_rank(&chart.closes),
        // (A/C) zero-extra-fetch quality scalars from the SAME closes: log-trend R² (steadiness) and
        // worst historical drawdown (pain). Feed the consistency multiplier + Calmar reward.
        trend_r2: core::trend_r2(&chart.closes),
        max_drawdown_pct: core::max_drawdown_pct(&chart.closes),
    }
}

/// (E) Trailing P/E from the configured fundamentals source (FMP `quote` by default). None unless
/// `FMP_API_KEY` is set in the environment (kept out of config) AND the source returns a positive
/// PE. ponytail: free fundamentals tiers are rate-limited, so expect this to populate at `check`
/// scale and stay None across the ~750-ticker `screen` (where the value tilt then just stays 1.0).
async fn fetch_pe(client: &Client, urls: &Urls, ticker: &str) -> Option<f64> {
    let key = std::env::var("FMP_API_KEY").ok().filter(|k| !k.is_empty())?;
    let url = urls.fundamentals.replace("{ticker}", ticker).replace("{key}", &key);
    let v = get_json(client, &url).await?;
    // FMP /quote returns a single-element array: [{ "pe": 28.4, ... }]; fall back to a bare object.
    let pe = v.get(0).unwrap_or(&v).get("pe")?.as_f64()?;
    (pe > 0.0).then_some(pe)
}

/// (F) Trailing return-on-equity (%) from the configured quality source (FMP `ratios-ttm` by
/// default). None unless `FMP_API_KEY` is set AND the source returns it. Same opt-in / rate-limit
/// profile as `fetch_pe`: populates at `check` scale, stays None across `screen` (quality tilt = 1.0).
/// FMP returns ROE as a FRACTION (0.42 = 42%), so ×100. ponytail: BACKTEST-BLIND — point-in-time, no
/// as-of reconstruction, so the picks term is theory-weighted and kept small.
async fn fetch_roe(client: &Client, urls: &Urls, ticker: &str) -> Option<f64> {
    let key = std::env::var("FMP_API_KEY").ok().filter(|k| !k.is_empty())?;
    let url = urls.fundamentals_quality.replace("{ticker}", ticker).replace("{key}", &key);
    let v = get_json(client, &url).await?;
    let roe = v.get(0).unwrap_or(&v).get("returnOnEquityTTM")?.as_f64()?;
    roe.is_finite().then_some(roe * 100.0)
}

/// Latest Bitcoin NUPL (net unrealized profit/loss) from bitcoin-data.com. None on failure.
pub async fn fetch_nupl(client: &Client, urls: &Urls) -> Option<f64> {
    get_json(client, &urls.nupl).await?.get("nupl")?.as_f64()
}

/// Tech-sector S&P-500 symbols (Yahoo form) from the constituents CSV — for screen's tech table.
/// Empty if the CSV fetch fails (tech table is then skipped). ponytail: re-fetches the CSV that
/// fetch_universe already pulled; one cheap raw.githubusercontent GET, not worth threading a tuple.
pub async fn fetch_tech_symbols(client: &Client, urls: &Urls) -> std::collections::HashSet<String> {
    match get_text(client, &urls.sp500_csv).await {
        Some(text) => text.lines().skip(1).filter_map(|l| core::tech_symbol(l)).collect(),
        None => std::collections::HashSet::new(),
    }
}

/// Sign one Börse Frankfurt API request the same way their web client does (reverse-engineered from
/// the bundle): `Client-Date` = an ISO-8601 UTC instant we also hash; `X-Client-TraceId` =
/// md5(ClientDate + full-url + salt); `X-Security` = md5(UTC `yyyyMMddHHmm`). The server re-hashes
/// the Client-Date we send, so its timezone is free; only X-Security must match the server's minute
/// (UTC, with skew tolerance). NB: send NO `Origin` header — their gateway 403s a browser origin.
fn bf_sign(url: &str, salt: &str) -> [(&'static str, String); 3] {
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
async fn bf_post(client: &Client, url: &str, salt: &str, body: &Value) -> Option<Value> {
    let mut req = client.post(url).header("Accept", "application/json, text/plain, */*").json(body);
    for (k, v) in bf_sign(url, salt) {
        req = req.header(k, v);
    }
    req.send().await.ok()?.json::<Value>().await.ok()
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
    let isins: Vec<String> = match bf_post(client, &urls.bf_etf_search, &urls.bf_salt, &body).await {
        Some(j) => j
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| arr.iter().filter_map(|r| r.get("isin")?.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    if isins.is_empty() {
        eprintln!("fetch: Börse Frankfurt ETF search returned nothing (salt rotated? refresh bf_salt) — ETF tables will be empty");
        return Vec::new();
    }
    // BF ignores our pageSize and dumps the whole list (~3430); it's TURNOVER-DESC, so the top `cap`
    // are the most-liquid ETFs — keep only those, both to match universe_size and to avoid firing
    // thousands of Yahoo searches (which DO rate-limit).
    let total = isins.len();
    let isins: Vec<&String> = isins.iter().take(cap).collect();
    // resolve ISIN -> Yahoo symbol (first quote = the liquid EU listing), bounded fan-out.
    // yahoo_search is tuned for news (quotesCount=0) — flip it to quotes here, or every row is None.
    let tickers: Vec<String> = stream::iter(isins.iter())
        .map(|isin| async move {
            let url = urls
                .yahoo_search
                .replace("{ticker}", isin.as_str())
                .replace("quotesCount=0", "quotesCount=1")
                .replace("newsCount=3", "newsCount=0");
            get_json(client, &url).await?.pointer("/quotes/0/symbol")?.as_str().map(String::from)
        })
        .buffer_unordered(fetch_concurrency())
        .filter_map(|x| async move { x })
        .collect()
        .await;
    // conclusive diagnostic: distinguishes "BF gave 0 ISINs" from "BF ok but Yahoo bridge resolved
    // none" — the two ways the ETF tables silently empty.
    eprintln!("fetch: Börse Frankfurt returned {total} ETF ISINs (kept top {} by turnover); {} resolved to Yahoo tickers", isins.len(), tickers.len());
    if tickers.is_empty() {
        eprintln!("fetch: ISIN->Yahoo resolution returned nothing (Yahoo search rate-limited?) — ETF tables will be empty");
    }
    tickers
}

/// Build the `screen` universe LIVE (no hand-kept list): top-`cap` crypto by market cap from
/// CoinGecko + the S&P 500 constituents CSV (single companies) + the top-`cap` EU-buyable UCITS ETFs
/// by turnover from Börse Frankfurt (`fetch_xetra_etfs`). Symbols normalised to Yahoo form (`btc` ->
/// `BTC-EUR`/`BTC-USD`, `BRK.B` -> `BRK-B`). Crypto quote currency follows `prefer_eur`. The old
/// US-listed NASDAQ-Trader ETFs are dropped: none are EU-buyable, so they only wasted fetches.
/// Sorted + deduped; empty if all sources fail.
pub async fn fetch_universe(client: &Client, urls: &Urls, cap: usize, prefer_eur: bool) -> Vec<String> {
    let cg_url = urls.coingecko_markets.replace("{n}", &cap.to_string());
    let (cg, csv, etfs) = tokio::join!(
        get_json(client, &cg_url),
        get_text(client, &urls.sp500_csv),
        fetch_xetra_etfs(client, urls, cap),
    );
    let crypto_cur = if prefer_eur { "EUR" } else { "USD" };
    let mut out: Vec<String> = Vec::new();
    // crypto: CoinGecko market-cap-ranked array -> SYMBOL-<EUR|USD> (Yahoo crypto form)
    if let Some(arr) = cg.as_ref().and_then(|v| v.as_array()) {
        out.extend(arr.iter().take(cap).filter_map(|c| {
            c.get("symbol").and_then(|s| s.as_str()).map(|s| format!("{}-{crypto_cur}", s.to_uppercase()))
        }));
    }
    // stocks: S&P 500 CSV, first column = Symbol; '.'->'-' for Yahoo (BRK.B -> BRK-B).
    // ponytail: naive comma split — constituent symbols/CSV carry no embedded commas.
    if let Some(text) = csv {
        out.extend(
            text.lines().skip(1).take(cap)
                .filter_map(|l| l.split(',').next())
                .map(|s| s.trim().replace('.', "-"))
                .filter(|s| !s.is_empty()),
        );
    }
    out.extend(etfs); // EU-buyable UCITS ETFs (Yahoo symbols)
    out.sort();
    out.dedup();
    out
}

/// Network-module asserts (no live calls): the pure, breakable bit of the Börse Frankfurt signer is
/// the MD5 — pin it to known answers so a bad refactor is caught offline. (The concatenation order is
/// verified against the live server, not here.)
pub fn selftest() {
    use md5::{Digest, Md5};
    let md5_hex = |s: &str| hex::encode(Md5::digest(s.as_bytes()));
    assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    // signer emits exactly the three headers the gateway needs, TraceId folds in the url + salt
    let h = bf_sign("https://x/y", "saltz");
    assert_eq!(h.len(), 3);
    assert_eq!([h[0].0, h[1].0, h[2].0], ["Client-Date", "X-Client-TraceId", "X-Security"]);
    assert_eq!(h[1].1, md5_hex(&format!("{}https://x/ysaltz", h[0].1))); // trace = md5(date+url+salt)

    // concurrency = cores × multiplier, both floored at 1 (a 0 anywhere can't stall the fan-out)
    assert_eq!(concurrency_for(8, 8), 64);
    assert_eq!(concurrency_for(4, 0), 4); // multiplier 0 -> treated as 1
    assert_eq!(concurrency_for(0, 8), 8); // cores 0 -> treated as 1
}

/// At most this many quote fetches in flight at once. Unbounded `join_all` over the ~750-ticker
/// screen universe stampeded Yahoo into 429s/timeouts (dropping random coins like BTC to err
/// stubs); a bounded window keeps each request uncontended.
///
/// Value = CPU cores × `fetch_concurrency_multiplier` (settings.yaml, default 8). Read once from
/// config and cached — the closure runs on the first fetch and never again, so all call sites stay
/// signature-free. ponytail: lazy global, no threading; bump the multiplier to scan faster or drop it
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

/// One Quote per ticker, concurrent (≤`FETCH_CONCURRENCY` in flight), input order preserved.
pub async fn quotes(client: &Client, urls: &Urls, fx: &FxCache, tickers: &[String], dip_days: i64, high_days: i64, intraday: bool, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Vec<Quote> {
    // Warm the USD rate once up front. Otherwise every US stock races its own USDEUR=X call in the
    // fan-out; one gets rate-limited -> None cached -> all USD names print "USD?" instead of €.
    // ponytail: USD only (dominant case); rare currencies (GBP/CHF) still race, fine at this scale.
    let _ = eur_rate(client, urls, "USD", fx).await;
    // progress to stderr (stdout stays a clean table): a big `screen` is minutes of silent network,
    // so log every PROGRESS_EVERY completions + the final total. ponytail: atomic counter, no bar lib.
    let total = tickers.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let concurrency = fetch_concurrency();
    eprintln!("fetch: {total} quotes (≤{concurrency} concurrent)…");
    const PROGRESS_EVERY: usize = 50;
    // `buffered` (ordered) caps concurrency yet preserves input order (`check` prints in that order).
    let mut out: Vec<Quote> = stream::iter(tickers.iter())
        .map(|tk| {
            let done = &done;
            async move {
                let q = quote_one(client, urls, fx, tk, dip_days, high_days, intraday, windows, infl).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n % PROGRESS_EVERY == 0 || n == total {
                    eprintln!("fetch: {n}/{total} quotes fetched");
                }
                q
            }
        })
        .buffered(concurrency)
        .collect()
        .await;
    // Second pass: under the wide fan-out + the extra long-history call per name, Yahoo 429s a handful of
    // names into "err"/"no data" stubs (empty perf -> gated out -> silently vanish from every table; NVDA
    // hit this). Re-fetch ONLY those, at low concurrency so they don't contend, and splice the recoveries
    // back. ponytail: one extra pass, no exponential schedule — the contention is gone once the stampede is.
    let retry_idx: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, q)| q.price == "err" || q.price == "no data")
        .map(|(i, _)| i)
        .collect();
    if !retry_idx.is_empty() {
        eprintln!("fetch: re-fetching {} dropped name(s) at low concurrency…", retry_idx.len());
        let recovered: Vec<(usize, Quote)> = stream::iter(retry_idx.into_iter())
            .map(|i| async move {
                (i, quote_one(client, urls, fx, &tickers[i], dip_days, high_days, intraday, windows, infl).await)
            })
            .buffered(4)
            .collect()
            .await;
        for (i, q) in recovered {
            if q.price != "err" && q.price != "no data" {
                out[i] = q;
            }
        }
    }
    out
}

/// Best-effort live 3-month Euribor (%). Returns (rate, is_live); falls back on failure.
/// ponytail: scrapes euribor-rates.eu HTML — fragile, hence the config fallback.
pub async fn fetch_euribor_3m(client: &Client, urls: &Urls, fallback: f64) -> (f64, bool) {
    if let Some(html) = get_text(client, &urls.euribor).await {
        let re = regex::Regex::new(r"(-?\d+\.\d+)\s*%").unwrap();
        if let Some(c) = re.captures(&html) {
            if let Ok(v) = c[1].parse::<f64>() {
                return (v, true);
            }
        }
    }
    (fallback, false)
}

/// {year -> annual CPI %} for the USA from the BLS public API (CPI-U index, converted to a
/// YoY rate in `core::parse_bls_cpi`); empty on failure. Monthly source, so it reaches the
/// current year — unlike the World Bank's ~1.5y-lagged annual series it replaced.
pub async fn fetch_us_inflation(client: &Client, urls: &Urls) -> BTreeMap<i32, f64> {
    match get_json(client, &urls.us_cpi).await {
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
