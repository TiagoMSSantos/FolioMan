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
        .timeout(StdDuration::from_secs(8))
        .connect_timeout(StdDuration::from_secs(2))
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
    divs: Vec<(NaiveDate, f64)>, // (ex-date, amount/share) from events.dividends
}

async fn chart_json(client: &Client, urls: &Urls, ticker: &str, range: &str) -> Option<Value> {
    let url = urls.yahoo_chart.replace("{ticker}", ticker).replace("{range}", range);
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
    let (chart_j, titles, intra, pe) = tokio::join!(
        // 10y, NOT max: Yahoo coarsens interval=1d to monthly bars once the span passes ~10y, which
        // makes 1D/1W/1M meaningless (only month-boundary points exist). 10y keeps TRUE daily bars
        // (~3652) for 1D..10Y; the 20Y column goes n/a (the heuristic's long leg falls back to 10Y).
        chart_json(client, urls, ticker, "10y"),
        fetch_news(client, urls, ticker),
        async { if intraday { intraday_closes(client, urls, ticker).await } else { None } },
        // (E) trailing P/E for the valuation tilt — equities only (crypto/FX have no earnings); a
        // no-op (instant None) unless FMP_API_KEY is set, so the default path stays network-free here.
        async { if ticker.contains('-') { None } else { fetch_pe(client, urls, ticker).await } },
    );

    let chart = match chart_j.as_ref().and_then(|j| parse_chart(j, ticker)) {
        Some(c) => c,
        None => return Quote::stub(ticker, "err", "", ticker),
    };
    if chart.closes.is_empty() {
        return Quote::stub(ticker, "no data", "", &chart.name);
    }

    let cur_close = *chart.closes.last().unwrap();
    let rate = eur_rate(client, urls, &chart.currency, fx).await;
    let price = match rate {
        Some(r) => format!("€{}", core::fmt_money2(cur_close * r)),
        None => format!("{} {}?", core::fmt_money2(cur_close), chart.currency),
    };

    let window = slice_since(&chart.dates, &chart.closes, dip_days);
    let d = if window.is_empty() { 0.0 } else { pct_from_high(&window) };
    // drawdown off the high over a longer window (picks "on sale" signal — a real pullback,
    // not the 30d dip which is ~0 for anything making new highs)
    let hi_window = slice_since(&chart.dates, &chart.closes, high_days);
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
        head: titles.first().cloned().unwrap_or_default(),
        news_block: titles.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n"),
        perf: horizon_changes(&chart.dates, &chart.closes, rate, windows, infl),
        name: chart.name,
        trend: format!("{arrow} {dur}"),
        at_ath,
        at_atl,
        mom_pct,
        div_eur: core::dividend_sums(&chart.divs, &chart.dates, rate),
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
        // (E) trailing P/E (equities w/ FMP_API_KEY only; None -> neutral value tilt).
        pe_ratio: pe,
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

/// Build the `screen` universe LIVE (no hand-kept list): top-`cap` crypto by market cap from
/// CoinGecko + the S&P 500 constituents CSV (stocks/ETFs), symbols normalised to Yahoo form
/// (`btc` -> `BTC-EUR`/`BTC-USD`, `BRK.B` -> `BRK-B`). Crypto quote currency follows
/// `prefer_eur` (Yahoo has both legs); US stocks/ETFs have no EUR listing, so unaffected.
/// Sorted + deduped; empty if both sources fail.
pub async fn fetch_universe(client: &Client, urls: &Urls, cap: usize, prefer_eur: bool) -> Vec<String> {
    let cg_url = urls.coingecko_markets.replace("{n}", &cap.to_string());
    let (cg, csv) = tokio::join!(
        get_json(client, &cg_url),
        get_text(client, &urls.sp500_csv),
    );
    let crypto_cur = if prefer_eur { "EUR" } else { "USD" };
    let mut out: Vec<String> = Vec::new();
    // crypto: CoinGecko market-cap-ranked array -> SYMBOL-<EUR|USD> (Yahoo crypto form)
    if let Some(arr) = cg.as_ref().and_then(|v| v.as_array()) {
        out.extend(arr.iter().take(cap).filter_map(|c| {
            c.get("symbol").and_then(|s| s.as_str()).map(|s| format!("{}-{crypto_cur}", s.to_uppercase()))
        }));
    }
    // stocks/ETFs: S&P 500 CSV, first column = Symbol; '.'->'-' for Yahoo (BRK.B -> BRK-B).
    // ponytail: naive comma split — constituent symbols/CSV carry no embedded commas.
    if let Some(text) = csv {
        out.extend(
            text.lines().skip(1).take(cap)
                .filter_map(|l| l.split(',').next())
                .map(|s| s.trim().replace('.', "-"))
                .filter(|s| !s.is_empty()),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// At most this many quote fetches in flight at once. Unbounded `join_all` over the ~750-ticker
/// screen universe stampeded Yahoo into 429s/timeouts (dropping random coins like BTC to err
/// stubs); a bounded window keeps each request uncontended. ponytail: fixed cap, tune if the
/// scan feels slow or Yahoo tightens limits.
const FETCH_CONCURRENCY: usize = 16;

/// One Quote per ticker, concurrent (≤`FETCH_CONCURRENCY` in flight), input order preserved.
pub async fn quotes(client: &Client, urls: &Urls, fx: &FxCache, tickers: &[String], dip_days: i64, high_days: i64, intraday: bool, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Vec<Quote> {
    // Warm the USD rate once up front. Otherwise every US stock races its own USDEUR=X call in the
    // fan-out; one gets rate-limited -> None cached -> all USD names print "USD?" instead of €.
    // ponytail: USD only (dominant case); rare currencies (GBP/CHF) still race, fine at this scale.
    let _ = eur_rate(client, urls, "USD", fx).await;
    // `buffered` (ordered) caps concurrency yet preserves input order (`check` prints in that order).
    stream::iter(tickers.iter())
        .map(|tk| quote_one(client, urls, fx, tk, dip_days, high_days, intraday, windows, infl))
        .buffered(FETCH_CONCURRENCY)
        .collect()
        .await
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
