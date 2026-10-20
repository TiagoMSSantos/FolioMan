//! Trading212 equity trading (official API). Auth: `TRADING212_API_KEY`.

use super::env_var;
use reqwest::Client;
use serde_json::{json, Value};

/// The live API host. Only the env-reading wrappers pass it; tests hand the `_at` halves a loopback stub.
const LIVE: &str = "https://live.trading212.com";

/// UNGRADEABLE, hence the skip: the live host in front of [`get_at`], which carries the call and is
/// pinned below against a loopback stub. Every caller reads the key from the env first.
#[mutants::skip]
async fn get(client: &Client, key: &str, path: &str) -> Result<Value, String> {
    get_at(client, LIVE, key, path).await
}

/// (#781) `get` against `host`, so a test drives the status check and the parse without a key.
async fn get_at(client: &Client, host: &str, key: &str, path: &str) -> Result<Value, String> {
    let resp = client
        .get(format!("{host}/api/v0/{path}"))
        .header("Authorization", key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(response body unreadable: {e})"));
    if !status.is_success() {
        return Err(format!("trading212 {status}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Cash available to invest + current holdings (read-only).
pub async fn summary(client: &Client) -> Result<String, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let cash = get(client, &key, "equity/account/cash").await?;
    let port = get(client, &key, "equity/portfolio").await?;
    render_summary(&cash, &port)
}

/// (round 114) Held positions (ticker, quantity, P/L) for the size command's allocation-gap section and
/// the screen's owned overlay (round 110, in Trading212 form, e.g. `AAPL_US_EQ`). Same endpoint `summary`
/// reads; rows missing ticker or quantity are skipped (the gap table must never invent a position, and a
/// display overlay must never fail the screen over one malformed row), a non-array response is API drift.
/// (#388) `ppl` is the unrealised P/L in account currency, the gain a fund switch would realise; a row
/// without it keeps its place and simply gets no switch line.
pub async fn owned_positions(client: &Client) -> Result<Vec<(String, f64, Option<f64>)>, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let port = get(client, &key, "equity/portfolio").await?;
    extract_positions(&port)
}

/// Trading212 encodes order direction by the SIGN of the quantity: buy = +qty, sell = -qty. Split
/// out of `order` so the branch is testable on its own; the transport around it is [`order_at`].
fn signed_qty(side: &str, qty: f64) -> f64 {
    if side == "sell" {
        -qty
    } else {
        qty
    }
}

/// Pure extraction, offline-testable (like `render_summary`).
fn extract_positions(port: &Value) -> Result<Vec<(String, f64, Option<f64>)>, String> {
    Ok(port
        .as_array()
        .ok_or_else(|| "trading212: portfolio response is not an array (API drift?)".to_string())?
        .iter()
        .filter_map(|h| Some((h.get("ticker")?.as_str()?.to_string(), h.get("quantity")?.as_f64()?, h.get("ppl").and_then(Value::as_f64))))
        .collect())
}

/// (round 118) Free cash (deploy-available, account currency) for the screen's cash line — just the
/// `free` field of the `equity/account/cash` endpoint `summary` already reads. A missing field is
/// API drift (error, not a silent 0.00), same stance as `render_summary`.
pub async fn cash_free(client: &Client) -> Result<f64, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let cash = get(client, &key, "equity/account/cash").await?;
    parse_free(&cash)
}

/// Pure extraction, offline-testable (like `render_summary`/`extract_positions`).
fn parse_free(cash: &Value) -> Result<f64, String> {
    cash.get("free")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| "trading212: no `free` in the cash response (API drift?)".to_string())
}


/// (round 117) One tradable instrument from the metadata endpoint — the minimum the order-glue
/// symbol resolver needs: exact T212 ticker form + ISIN + listing currency.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Instrument {
    pub ticker: String,
    pub isin: String,
    pub currency: String,
}

/// Pure extraction, offline-testable (like the others): rows missing any field are skipped, a
/// non-array response is API drift.
fn extract_instruments(v: &Value) -> Result<Vec<Instrument>, String> {
    Ok(v
        .as_array()
        .ok_or_else(|| "trading212: instruments response is not an array (API drift?)".to_string())?
        .iter()
        .filter_map(|i| {
            Some(Instrument {
                ticker: i.get("ticker")?.as_str()?.to_string(),
                isin: i.get("isin")?.as_str()?.to_string(),
                currency: i.get("currencyCode")?.as_str()?.to_string(),
            })
        })
        .collect())
}

/// (round 117) Full tradable-instrument list for order-glue symbol resolution, cached in
/// `.t212_instruments.json` for 7 days — the endpoint returns ~10k rows and is rate-limited
/// (~1 req/50s), and listings don't churn daily. ANY failure (no key, throttle, API drift) →
/// empty vec: the glue degrades to `<T212_SYMBOL>` placeholders exactly like the owned overlay,
/// never fails the screen.
pub async fn instruments_cached(client: &Client) -> Vec<Instrument> {
    instruments_cached_at(client, LIVE, env_var("TRADING212_API_KEY").ok()).await
}

/// (#781) `instruments_cached` against `host` with the key passed in (`None` = no key configured).
async fn instruments_cached_at(client: &Client, host: &str, key: Option<String>) -> Vec<Instrument> {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Cache {
        date: String,
        rows: Vec<Instrument>,
    }
    const PATH: &str = ".t212_instruments.json";
    let today = chrono::Utc::now().date_naive();
    if let Some(c) = std::fs::read_to_string(crate::config::data_path(PATH)).ok().and_then(|s| serde_json::from_str::<Cache>(&s).ok()) {
        if chrono::NaiveDate::parse_from_str(&c.date, "%Y-%m-%d").is_ok_and(|d| (today - d).num_days() < 7) {
            return c.rows;
        }
    }
    let Some(key) = key else {
        return Vec::new(); // no key configured = broker off, silent like the owned overlay
    };
    let rows = match get_at(client, host, &key, "equity/metadata/instruments").await.and_then(|v| extract_instruments(&v)) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("screen: trading212 instruments fetch failed ({e}) — order symbols degrade to placeholders");
            return Vec::new();
        }
    };
    if let Ok(json) = serde_json::to_string(&Cache { date: today.to_string(), rows: rows.clone() }) {
        let _ = std::fs::write(crate::config::data_path(PATH), json);
    }
    rows
}

/// Pure response→text rendering, split from the fetch so drift handling is testable offline.
/// A missing cash field or a non-array portfolio is API drift and must surface as the broker
/// error (accounts prints it as "(skipped) …"), never render as plausible zeros/emptiness.
fn render_summary(cash: &Value, port: &Value) -> Result<String, String> {
    let cash_num = |k: &str| {
        cash.get(k)
            .and_then(|x| x.as_f64())
            .ok_or_else(|| format!("trading212: no `{k}` in the cash response (API drift?)"))
    };
    let mut out = format!(
        "  cash: free {:.2}  invested {:.2}  total {:.2}",
        cash_num("free")?,
        cash_num("invested")?,
        cash_num("total")?,
    );
    let p = port
        .as_array()
        .ok_or_else(|| "trading212: portfolio response is not an array (API drift?)".to_string())?;
    if p.is_empty() {
        out.push_str("\n  holdings: (none)");
    } else {
        out.push_str("\n  holdings:");
        for h in p {
            let ticker = h.get("ticker").and_then(|v| v.as_str()).unwrap_or("?");
            let num = |k: &str| h.get(k).and_then(|v| v.as_f64());
            // an unparsable amount must not render as 0.00 — that reads as "nothing here"
            let (Some(qty), Some(price), Some(ppl)) = (num("quantity"), num("currentPrice"), num("ppl")) else {
                eprintln!("WARNING: trading212 position row for {ticker} has an unparsable amount — row skipped");
                continue;
            };
            out.push_str(&format!("\n    {ticker:<12} qty {qty:<10} @ {price:.2}  P/L {ppl:+.2}"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// (round 117) instruments extraction: complete rows collected, partial rows skipped (a
    /// resolver must never see a half-instrument), non-array = drift error.
    #[test]
    fn extract_instruments_collects_and_skips() {
        let v = json!([
            { "ticker": "AAPL_US_EQ", "isin": "US0378331005", "currencyCode": "USD" },
            { "ticker": "NOISIN_EQ", "currencyCode": "USD" },
            { "isin": "IE00LONELY00", "currencyCode": "EUR" }
        ]);
        let rows = extract_instruments(&v).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].ticker.as_str(), rows[0].isin.as_str(), rows[0].currency.as_str()), ("AAPL_US_EQ", "US0378331005", "USD"));
        assert!(extract_instruments(&json!({ "not": "array" })).is_err());
    }

    /// (#359) A cache inside its 7 days is served as-is: no key read, no request. `screen` calls this
    /// on every run and the endpoint allows ~1 request per 50s.
    ///
    /// (#781) The stale side runs too, in this one test because every step reads or writes the same
    /// cache file. A cache 7 days old is stale. Stale with no key is empty, and so is a refused fetch,
    /// which leaves the file alone. A good fetch returns the rows, and the file it writes is then
    /// served as fresh with the host dead.
    #[tokio::test]
    async fn instruments_cached_serves_a_fresh_cache() {
        let path = crate::config::data_path(".t212_instruments.json");
        let today = chrono::Utc::now().date_naive();
        let seed = |date: chrono::NaiveDate| {
            let cache = json!({ "date": date.to_string(), "rows": [
                { "ticker": "AAPL_US_EQ", "isin": "US0378331005", "currency": "USD" }
            ] });
            std::fs::write(&path, cache.to_string()).expect("seed cache");
        };
        let tickers = |rows: Vec<Instrument>| rows.into_iter().map(|i| i.ticker).collect::<Vec<_>>();
        seed(today);
        let client = Client::builder().no_proxy().build().expect("test client");
        let rows = instruments_cached(&client).await;
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].ticker.as_str(), rows[0].isin.as_str()), ("AAPL_US_EQ", "US0378331005"));

        seed(today - chrono::Duration::days(7));
        assert!(instruments_cached_at(&client, DEAD, None).await.is_empty(), "stale with no key");
        let (url, client, _reqs) = crate::fetch::tests::routed_stub(vec![("/instruments", "HTTP/1.1 429 Too Many Requests\r\n\r\n{}")]);
        assert!(instruments_cached_at(&client, url.trim_end_matches('/'), Some("k".into())).await.is_empty());
        let row = r#"[{"ticker":"MSFT_US_EQ","isin":"US5949181045","currencyCode":"USD"}]"#;
        let (url, client, reqs) = crate::fetch::tests::routed_stub(vec![("/instruments", row)]);
        let fetched = instruments_cached_at(&client, url.trim_end_matches('/'), Some("k".into())).await;
        assert_eq!(tickers(fetched), ["MSFT_US_EQ"], "a stale cache refetches");
        assert!(reqs.recv().expect("one request").starts_with("GET /api/v0/equity/metadata/instruments "));
        assert_eq!(tickers(instruments_cached_at(&client, DEAD, Some("k".into())).await), ["MSFT_US_EQ"], "the fetch was cached");
    }

    /// A port nothing listens on: a request to it fails at connect.
    const DEAD: &str = "http://127.0.0.1:1";

    /// (#781) The read call against a loopback stub: the key goes in the Authorization header, a 2xx
    /// parses, and a refusal or a body that is not JSON is an error.
    #[tokio::test]
    async fn get_at_reads_the_answer() {
        let (url, client, reqs) = crate::fetch::tests::routed_stub(vec![("/cash", r#"{"free":1.5}"#)]);
        assert_eq!(get_at(&client, url.trim_end_matches('/'), "k", "equity/account/cash").await, Ok(json!({ "free": 1.5 })));
        let req = reqs.recv().expect("one request");
        assert!(req.starts_with("GET /api/v0/equity/account/cash "), "{req}");
        assert!(req.to_ascii_lowercase().contains("\r\nauthorization: k\r\n"), "{req}");
        let (url, client, _reqs) = crate::fetch::tests::routed_stub(vec![("/cash", "HTTP/1.1 401 Unauthorized\r\n\r\n{}")]);
        assert_eq!(get_at(&client, url.trim_end_matches('/'), "k", "equity/account/cash").await, Err("trading212 401 Unauthorized: {}".to_string()));
        let (url, client, _reqs) = crate::fetch::tests::routed_stub(vec![("/cash", "<html>")]);
        assert!(get_at(&client, url.trim_end_matches('/'), "k", "equity/account/cash").await.is_err());
    }

    /// (#781) Every answer the order path can get, against a loopback stub. A 2xx is accepted. A 4xx
    /// refusal and a refused connection are plain errors; a 5xx says the order MAY be live. The POST
    /// carries the key and the signed quantity: a sell goes out negative.
    #[tokio::test]
    async fn order_at_reads_each_answer() {
        let (url, client, reqs) = crate::fetch::tests::routed_stub(vec![("/orders/market", r#"{"id":7}"#)]);
        let placed = order_at(&client, url.trim_end_matches('/'), "k", "sell", "AAPL_US_EQ", 2.5).await;
        assert_eq!(placed, Ok(r#"trading212 accepted: {"id":7}"#.to_string()));
        let req = reqs.recv().expect("one request");
        assert!(req.starts_with("POST /api/v0/equity/orders/market "), "{req}");
        assert!(req.to_ascii_lowercase().contains("\r\nauthorization: k\r\n"), "{req}");
        let sent: Value = serde_json::from_str(req.split_once("\r\n\r\n").map_or("", |(_, b)| b)).expect("a JSON body");
        assert_eq!(sent, json!({ "quantity": -2.5, "ticker": "AAPL_US_EQ" }));
        let answer = |body: &'static str| async move {
            let (url, client, _reqs) = crate::fetch::tests::routed_stub(vec![("/orders/market", body)]);
            order_at(&client, url.trim_end_matches('/'), "k", "buy", "AAPL_US_EQ", 1.0).await
        };
        let refused = answer("HTTP/1.1 400 Bad Request\r\n\r\n{}").await.unwrap_err();
        assert_eq!(refused, "trading212 400 Bad Request: {}");
        let unknown = answer("HTTP/1.1 503 Service Unavailable\r\n\r\n{}").await.unwrap_err();
        assert!(unknown.starts_with("trading212 503 Service Unavailable: {}") && unknown.contains("MAY be live"), "{unknown}");
        assert!(unknown.contains("Trading212 app"), "{unknown}");
        let dead = order_at(&client, DEAD, "k", "buy", "AAPL_US_EQ", 1.0).await.unwrap_err();
        assert!(!dead.is_empty() && !dead.contains("MAY"), "a refused connect placed nothing: {dead}");
    }

    #[test]
    fn renders_cash_and_holdings() {
        let cash = json!({ "free": 100.5, "invested": 200.0, "total": 300.5 });
        let port = json!([{ "ticker": "AAPL_US_EQ", "quantity": 2.0, "currentPrice": 150.0, "ppl": 12.5 }]);
        let out = render_summary(&cash, &port).unwrap();
        assert!(out.contains("free 100.50"), "{out}");
        assert!(out.contains("AAPL_US_EQ"), "{out}");
    }

    /// A renamed/dropped cash field is API drift — it must error naming the field, not print 0.00.
    #[test]
    fn missing_cash_field_is_drift_error_not_zero() {
        let cash = json!({ "free": 100.0, "invested": 200.0 }); // no "total"
        let err = render_summary(&cash, &json!([])).unwrap_err();
        assert!(err.contains("`total`"), "error must name the missing field: {err}");
    }

    /// (round 118) free-cash extraction: the `free` number is returned; a dropped `free` is API drift
    /// (error naming the field), never a silent 0.
    #[test]
    fn parse_free_extracts_and_drift_errors() {
        let cash = json!({ "free": 123.45, "invested": 200.0, "total": 323.45 });
        assert_eq!(parse_free(&cash).unwrap(), 123.45);
        let err = parse_free(&json!({ "invested": 200.0 })).unwrap_err();
        assert!(err.contains("`free`"), "must name the missing field: {err}");
    }

    /// A non-array portfolio (wrapper object, error payload) must not read as an empty account.
    #[test]
    fn non_array_portfolio_is_drift_error_not_empty() {
        let cash = json!({ "free": 0.0, "invested": 0.0, "total": 0.0 });
        let err = render_summary(&cash, &json!({ "positions": [] })).unwrap_err();
        assert!(err.contains("not an array"), "{err}");
    }

    /// (round 114) position extraction: (ticker, qty, (#388) ppl) collected, rows missing ticker or qty
    /// skipped, a missing ppl kept as None, non-array = drift error.
    #[test]
    fn extract_positions_collects_and_skips() {
        let port = json!([
            { "ticker": "AAPL_US_EQ", "quantity": 2.5, "ppl": -1.25 },
            { "quantity": 1.0 },
            { "ticker": "NOQTY_US_EQ" },
            { "ticker": "VUAGl_EQ", "quantity": 3.0 }
        ]);
        assert_eq!(
            extract_positions(&port).unwrap(),
            vec![("AAPL_US_EQ".to_string(), 2.5, Some(-1.25)), ("VUAGl_EQ".to_string(), 3.0, None)]
        );
        assert!(extract_positions(&json!({ "positions": [] })).unwrap_err().contains("not an array"));
    }

    #[test]
    fn empty_portfolio_prints_none() {
        let cash = json!({ "free": 0.0, "invested": 0.0, "total": 0.0 });
        let out = render_summary(&cash, &json!([])).unwrap();
        assert!(out.contains("holdings: (none)"), "{out}");
    }

    /// T212 encodes buy/sell as the SIGN of the quantity, so this one branch is the difference
    /// between acquiring and liquidating a position — and it had no test. Pins today's behaviour
    /// exactly, including the else-arm: any side that is not the literal `sell` is a BUY. The `trade`
    /// command confirm-gates the call and validates the side before here, so the else-arm is not
    /// reachable in practice; it is pinned because a mutant that flips it is silent otherwise.
    #[test]
    fn sell_is_the_only_negative_side() {
        assert_eq!(signed_qty("sell", 3.0), -3.0);
        assert_eq!(signed_qty("buy", 3.0), 3.0);
        assert_eq!(signed_qty("SELL", 3.0), 3.0, "match is exact-case: uppercase is NOT a sell");
        assert_eq!(signed_qty("anything-else", 3.0), 3.0);
    }

    /// A row whose amounts fail to parse is skipped (with a stderr warning), never shown as 0.00.
    #[test]
    fn unparsable_position_row_is_skipped_not_zero() {
        let cash = json!({ "free": 0.0, "invested": 0.0, "total": 0.0 });
        let port = json!([
            { "ticker": "BAD_ROW", "quantity": "2", "currentPrice": 150.0, "ppl": 0.0 },
            { "ticker": "GOOD", "quantity": 1.0, "currentPrice": 10.0, "ppl": 0.5 }
        ]);
        let out = render_summary(&cash, &port).unwrap();
        assert!(!out.contains("BAD_ROW"), "{out}");
        assert!(out.contains("GOOD"), "{out}");
    }
}

/// Trading212 market order via the **live** (production = real money) API. `ticker` must be
/// in Trading212 form, e.g. `AAPL_US_EQ`. buy = +qty, sell = -qty (T212 encodes direction
/// by sign). Key is the raw `Authorization` header value.
///
/// UNGRADEABLE, hence the skip, and here the reason is sharper than for a read endpoint: killing
/// `-> Ok(String::new())` means proving this function POSTS. Against the live host that is a real
/// market order, and the obvious alternative — asserting it errors with no credential — holds only
/// while `TRADING212_API_KEY` is unset, so the same test that passes in CI would place an order on a
/// machine that has the key. What is left here is that key read; everything past it is
/// [`order_at`], pinned against a loopback stub, and the sign of the quantity is `signed_qty`.
#[mutants::skip]
pub async fn order(client: &Client, side: &str, ticker: &str, qty: f64) -> Result<String, String> {
    let key = env_var("TRADING212_API_KEY")?;
    order_at(client, LIVE, &key, side, ticker, qty).await
}

/// (#781) `order` against `host` with the key passed in. A test can now prove what the order path
/// says on each answer without a key, so no test can reach `live.trading212.com`.
async fn order_at(client: &Client, host: &str, key: &str, side: &str, ticker: &str, qty: f64) -> Result<String, String> {
    let signed = signed_qty(side, qty);
    let resp = client
        .post(format!("{host}/api/v0/equity/orders/market"))
        .header("Authorization", key)
        .json(&json!({ "quantity": signed, "ticker": ticker }))
        .send()
        .await
        // (#667) only a failed connect proves the order never left; see `order_error`
        .map_err(|e| super::order_error("Trading212", e.is_connect(), e.to_string()))?;
    let status = resp.status();
    // the order may already be live at this point — a body-read failure must say so, not print
    // an empty confirmation.
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(response body unreadable: {e} — check the order in the Trading212 app)"));
    if status.is_success() {
        Ok(format!("trading212 accepted: {body}"))
    } else {
        Err(super::order_error("Trading212", status.is_client_error(), format!("trading212 {status}: {body}")))
    }
}
