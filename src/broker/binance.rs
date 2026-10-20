//! Binance spot trading (official API). Auth: `BINANCE_API_KEY` + `BINANCE_API_SECRET`.

use super::env_var;
use reqwest::Client;
use serde_json::Value;

/// Quote/stable assets treated as "cash available to invest".
const CASH_ASSETS: &[&str] = &["EUR", "USD", "USDT", "USDC", "BUSD", "FDUSD"];

/// (#776) Both credentials, or why not. Only a pair missing BOTH is keyless (the broker is off). A key
/// without its secret, or the reverse, used to read as keyless too, so `size` and `accounts` skipped
/// Binance in silence; prefixed, it is a failed call they report.
fn creds(key: Result<String, String>, secret: Result<String, String>) -> Result<(String, String), String> {
    match (key, secret) {
        (Ok(k), Ok(s)) => Ok((k, s)),
        (Err(e), Err(_)) => Err(e),
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(format!("half-configured: {e}")),
    }
}

/// Signed `/api/v3/account` call → the raw balances array. Shared by `summary` (rendering) and
/// `owned_assets` (round 111 screen overlay). A missing balances array is API drift, not an empty
/// account — say so instead of rendering "(none)".
///
/// UNGRADEABLE, hence the skip: the env credential read in front of [`account_balances_at`], which
/// carries the call and is pinned below against a loopback stub. The credential pairing is [`creds`].
#[mutants::skip]
async fn account_balances(client: &Client) -> Result<Vec<Value>, String> {
    let (key, secret) = creds(env_var("BINANCE_API_KEY"), env_var("BINANCE_API_SECRET"))?;
    account_balances_at(client, LIVE, &key, &secret).await
}

/// The live API host. Only the env-reading wrappers pass it; tests hand the `_at` halves a loopback stub.
const LIVE: &str = "https://api.binance.com";

/// (#780) `account_balances` against `host`, with the credentials passed in rather than read, so a test
/// drives the signed GET, the status check and the drift error without a key or a real account.
async fn account_balances_at(client: &Client, host: &str, key: &str, secret: &str) -> Result<Vec<Value>, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let query = format!("recvWindow=5000&timestamp={ts}");
    let sig = sign(secret, &query);
    let url = format!("{host}/api/v3/account?{query}&signature={sig}");
    let resp = client
        .get(&url)
        .header("X-MBX-APIKEY", key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("binance {status}: {body}"));
    }
    let acct: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    acct.get("balances")
        .and_then(|v| v.as_array())
        .cloned()
        .ok_or_else(|| "binance: no balances array in the account response (API drift?)".to_string())
}

/// (round 111) Non-cash assets with a real balance (e.g. `BTC`, `ETH`) for the screen's
/// held-position overlay.
pub async fn owned_assets(client: &Client) -> Result<Vec<String>, String> {
    Ok(extract_amounts(&account_balances(client).await?).into_iter().map(|(a, _)| a).collect())
}

/// (round 114) Non-cash assets WITH the held amount (free+locked) for the size command's
/// allocation-gap section.
pub async fn owned_amounts(client: &Client) -> Result<Vec<(String, f64)>, String> {
    Ok(extract_amounts(&account_balances(client).await?))
}

/// Pure extraction, offline-testable: cash/stable and zero-balance rows drop, unparsable rows are
/// skipped — a display overlay must never invent a holding.
fn extract_amounts(balances: &[Value]) -> Vec<(String, f64)> {
    balances
        .iter()
        .filter_map(|b| {
            let asset = b.get("asset").and_then(|v| v.as_str())?;
            let num = |k: &str| b.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
            let held = num("free")? + num("locked")?;
            (held > 0.0 && !CASH_ASSETS.contains(&asset)).then(|| (asset.to_string(), held))
        })
        .collect()
}

/// Free + invested balances (read-only). Splits stable/fiat (cash) from the rest (holdings).
///
/// UNGRADEABLE, hence the skip: what is left here after `render_balances` was split out is the
/// env credential read in `account_balances`, against the live host. Killing `-> Ok(String::new())`
/// needs either a live socket, or a test that asserts this errors — which only holds while
/// `BINANCE_API_KEY` is unset, so it would pass in CI and fetch for real on a machine that has keys.
/// Neither belongs in the offline suite. The signed GET is [`account_balances_at`] and the text is
/// `render_balances`, both pinned below.
#[mutants::skip]
pub async fn summary(client: &Client) -> Result<String, String> {
    Ok(render_balances(&account_balances(client).await?))
}

/// Pure balances→text rendering, split from the fetch so the dust/drift handling is testable offline
/// (like `extract_amounts`). The fetch half is [`account_balances_at`], pinned against a stub.
fn render_balances(balances: &[Value]) -> String {
    let mut cash = Vec::new();
    let mut holdings = Vec::new();
    for b in balances {
        let asset = b.get("asset").and_then(|v| v.as_str()).unwrap_or("?");
        let num = |k: &str| b.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
        // an unparsable amount must not render as 0.0 — that reads as "no money here"
        let (Some(free), Some(locked)) = (num("free"), num("locked")) else {
            eprintln!("WARNING: binance balance row for {asset} has an unparsable amount — row skipped");
            continue;
        };
        if free + locked <= 0.0 {
            continue; // skip dust/empty
        }
        let line = format!("    {asset:<8} free {free}  locked {locked}");
        if CASH_ASSETS.contains(&asset) {
            cash.push(line);
        } else {
            holdings.push(line);
        }
    }
    let block = |label: &str, v: &[String]| {
        if v.is_empty() {
            format!("\n  {label}: (none)")
        } else {
            format!("\n  {label}:\n{}", v.join("\n"))
        }
    };
    format!("{}{}", block("cash", &cash), block("holdings", &holdings))
}

/// HMAC-SHA256(secret, msg) as lowercase hex — Binance's request signature scheme.
pub fn sign(secret: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC any key len");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// (#298) The smallest market order Binance fills on the pairs `screen` prints: `exchangeInfo`'s
/// NOTIONAL `minNotional` is 5.00 on BTCEUR and BNBEUR, with `applyMinToMarket` (read 2026-09-13).
/// `screen::order_glue` tags a coin row under it; Binance's own rejection stays the backstop.
pub const MIN_ORDER_EUR: f64 = 5.0;

/// (#298) The signed order query. `eur` sends `quoteOrderQty` (spend this much of the quote asset, EUR
/// on the pairs `screen` prints, and Binance sizes the fill to its lot step) instead of `quantity`
/// (base asset, which must land on the pair's LOT_SIZE step: BNBEUR's is 0.001). Pure, so the one
/// branch a real-money order turns on is pinned offline.
fn order_query(symbol: &str, side: &str, amount: f64, eur: bool, ts: u128) -> String {
    let field = if eur { "quoteOrderQty" } else { "quantity" };
    format!("symbol={symbol}&side={}&type=MARKET&{field}={amount}&recvWindow=5000&timestamp={ts}", side.to_uppercase())
}

/// (#762) What a 2xx order answer with an `orderId` says. A MARKET order Binance cannot fully fill
/// (a thin book, or self-trade prevention) still answers 2xx, with `status` EXPIRED or
/// EXPIRED_IN_MATCH and a partial or zero `executedQty`. It used to print "filled" all the same, so
/// only FILLED says filled now. A body with no readable status keeps the old text.
fn fill_label(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    match v.get("status").and_then(Value::as_str) {
        Some("FILLED") | None => format!("binance filled: {body}"),
        Some(status) => {
            let qty = v.get("executedQty").and_then(Value::as_str).unwrap_or("?");
            format!("binance order {status}, executedQty {qty} — NOT fully filled, check the Binance app: {body}")
        }
    }
}

/// Binance spot MARKET order via the live API. `symbol` = pair, e.g. `BTCEUR`. `qty` = base
/// asset amount, or euros to spend when `eur` ([`order_query`]). HMAC-signed query, key in the
/// `X-MBX-APIKEY` header.
///
/// UNGRADEABLE, hence the skip, for `summary`'s reason: env credentials and the live host. Everything
/// past the credential read is [`order_at`], pinned below against a loopback stub; the branch that picks
/// the amount field is [`order_query`].
#[mutants::skip]
pub async fn order(client: &Client, side: &str, symbol: &str, qty: f64, eur: bool) -> Result<String, String> {
    let (key, secret) = creds(env_var("BINANCE_API_KEY"), env_var("BINANCE_API_SECRET"))?;
    order_at(client, LIVE, &key, &secret, side, symbol, qty, eur).await
}

/// (#780) `order` against `host` with the credentials passed in. A test can now prove what the order
/// path says on each answer (filled, no orderId, a 4xx refusal, a 5xx that MAY be live, a refused
/// connection) without a key, so no test can reach `api.binance.com`.
async fn order_at(client: &Client, host: &str, key: &str, secret: &str, side: &str, symbol: &str, qty: f64, eur: bool) -> Result<String, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let query = order_query(symbol, side, qty, eur, ts);
    let sig = sign(secret, &query);
    let url = format!("{host}/api/v3/order?{query}&signature={sig}");
    let resp = client
        .post(&url)
        .header("X-MBX-APIKEY", key)
        .send()
        .await
        // (#667) only a failed connect proves the order never left; see `order_error`
        .map_err(|e| super::order_error("Binance", e.is_connect(), e.to_string()))?;
    let status = resp.status();
    // the order may already be live at this point — a body-read failure must say so, not print
    // an empty confirmation.
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(response body unreadable: {e} — check the order in the Binance app)"));
    if status.is_success() {
        // 2xx without an orderId in the body = accepted transport-wise but the fill is
        // unconfirmed; the caller printed real money, make the doubt explicit.
        if body.contains("orderId") {
            Ok(fill_label(&body))
        } else {
            Ok(format!("binance answered {status} but no orderId in the response — VERIFY the order in the app: {body}"))
        }
    } else {
        Err(super::order_error("Binance", status.is_client_error(), format!("binance {status}: {body}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// (#776) a pair is keyless only when both halves are missing; one half alone names the missing
    /// variable and is not keyless, so callers report it instead of skipping the broker.
    #[test]
    fn creds_names_a_half_configured_pair() {
        let set = |v: &str| Ok::<String, String>(v.to_string());
        let unset = || env_var("FOLIOMAN_NO_SUCH_BINANCE_SECRET");
        assert_eq!(creds(set("k"), set("s")), Ok(("k".to_string(), "s".to_string())));
        let off = creds(env_var("FOLIOMAN_NO_SUCH_BINANCE_KEY"), unset()).unwrap_err();
        assert!(crate::broker::is_keyless(&off) && off.contains("FOLIOMAN_NO_SUCH_BINANCE_KEY"), "{off}");
        for half in [creds(set("k"), unset()), creds(unset(), set("s"))] {
            let e = half.unwrap_err();
            assert!(e.starts_with("half-configured: ") && e.contains("FOLIOMAN_NO_SUCH_BINANCE_SECRET"), "{e}");
            assert!(!crate::broker::is_keyless(&e), "{e}");
        }
    }

    /// (round 111/114) owned-amounts extraction: cash/stable and zero rows drop, unparsable rows
    /// skip, and the held amount = free + locked.
    #[test]
    fn extract_amounts_drops_cash_zero_and_unparsable() {
        let rows = vec![
            json!({ "asset": "BTC", "free": "0.5", "locked": "0.25" }),
            json!({ "asset": "EUR", "free": "100", "locked": "0" }),
            json!({ "asset": "ETH", "free": "0", "locked": "0" }),
            json!({ "asset": "SOL", "free": "oops", "locked": "0" }),
        ];
        assert_eq!(extract_amounts(&rows), vec![("BTC".to_string(), 0.75)]);
    }

    /// (#298) The amount field is the whole difference between spending €23.63 and buying 23.63 BTC.
    #[test]
    fn order_query_picks_the_amount_field() {
        assert_eq!(
            order_query("BTCEUR", "buy", 23.63, true, 1),
            "symbol=BTCEUR&side=BUY&type=MARKET&quoteOrderQty=23.63&recvWindow=5000&timestamp=1"
        );
        assert_eq!(
            order_query("BTCEUR", "sell", 0.001, false, 1),
            "symbol=BTCEUR&side=SELL&type=MARKET&quantity=0.001&recvWindow=5000&timestamp=1"
        );
    }

    /// (#762) Only FILLED reads as filled. EXPIRED with a partial fill and EXPIRED_IN_MATCH with none
    /// say how much went through; a body that is not JSON keeps the old text.
    #[test]
    fn fill_label_says_filled_only_for_filled() {
        let filled = r#"{"orderId":1,"status":"FILLED","executedQty":"0.001"}"#;
        assert_eq!(fill_label(filled), format!("binance filled: {filled}"));
        let partial = r#"{"orderId":2,"status":"EXPIRED","executedQty":"0.0004"}"#;
        assert_eq!(
            fill_label(partial),
            format!("binance order EXPIRED, executedQty 0.0004 — NOT fully filled, check the Binance app: {partial}")
        );
        let stp = r#"{"orderId":3,"status":"EXPIRED_IN_MATCH","executedQty":"0.00000000"}"#;
        assert!(fill_label(stp).starts_with("binance order EXPIRED_IN_MATCH, executedQty 0.00000000 — NOT fully filled"));
        assert_eq!(fill_label("orderId garbled"), "binance filled: orderId garbled");
    }

    /// Rendering: stable/fiat lands under `cash` and everything else under `holdings`, dust and
    /// zero rows drop, and an unparsable amount SKIPS the row rather than printing it as 0 — "free 0"
    /// against a real balance is the reading this must never produce.
    ///
    /// (#359) ADA is held entirely in open orders: `free` 0, `locked` 3. It is still held, so the
    /// dust test must read the SUM — `free - locked` would drop it as -3.
    #[test]
    fn render_balances_splits_cash_from_holdings() {
        let out = render_balances(&[
            json!({ "asset": "BTC", "free": "0.5", "locked": "0.25" }),
            json!({ "asset": "ADA", "free": "0", "locked": "3" }),
            json!({ "asset": "USDT", "free": "1000", "locked": "0" }),
            json!({ "asset": "ETH", "free": "0", "locked": "0" }),
            json!({ "asset": "SOL", "free": "oops", "locked": "0" }),
        ]);
        let (cash, holdings) = out.split_once("\n  holdings").expect("both blocks present");
        assert!(cash.contains("USDT"), "{out}");
        assert!(!cash.contains("BTC"), "{out}");
        assert!(holdings.contains("BTC"), "{out}");
        assert!(holdings.contains("ADA"), "a balance locked in open orders is still held: {out}");
        assert!(!out.contains("ETH"), "zero balance is dust: {out}");
        assert!(!out.contains("SOL"), "unparsable row must skip, not render 0: {out}");
    }

    /// Both empty arms — an account with nothing in it prints "(none)" twice, never an empty block
    /// that reads as a truncated response.
    #[test]
    fn render_balances_empty_prints_none_for_both() {
        let out = render_balances(&[]);
        assert!(out.contains("cash: (none)"), "{out}");
        assert!(out.contains("holdings: (none)"), "{out}");
    }

    /// Signing self-check against a known HMAC-SHA256 test vector.
    #[test]
    fn signing() {
        assert_eq!(
            sign("key", "The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    /// The query a stubbed request signed and the signature it sent, both read off its request line.
    fn signed(req: &str) -> (&str, &str) {
        let path = req.split(' ').nth(1).unwrap_or("");
        path.split_once('?').and_then(|(_, q)| q.rsplit_once("&signature=")).expect("a signed query")
    }

    /// (#780) The signed account GET against a loopback stub. The rows come back, and the request
    /// carries the key header and a signature over its own query. A refusal, or a 200 without a
    /// balances array, is an error and never an empty account.
    #[tokio::test]
    async fn account_balances_at_signs_and_reads_the_rows() {
        let (url, client, reqs) =
            crate::fetch::tests::routed_stub(vec![("/api/v3/account", r#"{"balances":[{"asset":"BTC","free":"0.5","locked":"0"}]}"#)]);
        let rows = account_balances_at(&client, url.trim_end_matches('/'), "k", "s").await;
        assert_eq!(rows, Ok(vec![json!({ "asset": "BTC", "free": "0.5", "locked": "0" })]));
        let req = reqs.recv().expect("one request");
        assert!(req.starts_with("GET /api/v3/account?recvWindow=5000&timestamp="), "{req}");
        let (query, sig) = signed(&req);
        assert_eq!(sig, sign("s", query));
        assert!(req.to_ascii_lowercase().contains("\r\nx-mbx-apikey: k\r\n"), "{req}");
        for (answer, want) in [
            ("HTTP/1.1 401 Unauthorized\r\n\r\n{\"code\":-2015}", "binance 401 Unauthorized: {\"code\":-2015}"),
            ("{\"accountType\":\"SPOT\"}", "binance: no balances array"),
        ] {
            let (url, client, _reqs) = crate::fetch::tests::routed_stub(vec![("/api/v3/account", answer)]);
            let err = account_balances_at(&client, url.trim_end_matches('/'), "k", "s").await.unwrap_err();
            assert!(err.starts_with(want), "{err}");
        }
    }

    /// (#780) Every answer the order path can get, against a loopback stub. FILLED prints the fill, and
    /// a 2xx without an orderId says VERIFY. A 4xx refusal and a refused connection are plain errors;
    /// a 5xx says the order MAY be live. The POST carries `order_query`'s fields, signed, with the key.
    #[tokio::test]
    async fn order_at_reads_each_answer() {
        let filled = r#"{"orderId":1,"status":"FILLED","executedQty":"0.001"}"#;
        let (url, client, reqs) = crate::fetch::tests::routed_stub(vec![("/api/v3/order", filled)]);
        let placed = order_at(&client, url.trim_end_matches('/'), "k", "s", "buy", "BTCEUR", 23.63, true).await;
        assert_eq!(placed, Ok(fill_label(filled)));
        let req = reqs.recv().expect("one request");
        let want = "POST /api/v3/order?symbol=BTCEUR&side=BUY&type=MARKET&quoteOrderQty=23.63&recvWindow=5000&timestamp=";
        assert!(req.starts_with(want), "{req}");
        let (query, sig) = signed(&req);
        assert_eq!(sig, sign("s", query));
        assert!(req.to_ascii_lowercase().contains("\r\nx-mbx-apikey: k\r\n"), "{req}");
        let answer = |body: &'static str| async move {
            let (url, client, _reqs) = crate::fetch::tests::routed_stub(vec![("/api/v3/order", body)]);
            order_at(&client, url.trim_end_matches('/'), "k", "s", "sell", "BTCEUR", 0.001, false).await
        };
        let unconfirmed = answer("{\"code\":0}").await.expect("a 2xx is Ok");
        assert!(unconfirmed.contains("VERIFY"), "{unconfirmed}");
        let refused = answer("HTTP/1.1 400 Bad Request\r\n\r\n{\"code\":-1013}").await.unwrap_err();
        assert!(refused.starts_with("binance 400 Bad Request: ") && !refused.contains("MAY"), "{refused}");
        let unknown = answer("HTTP/1.1 503 Service Unavailable\r\n\r\n{}").await.unwrap_err();
        assert!(unknown.starts_with("binance 503 Service Unavailable: ") && unknown.contains("MAY be live"), "{unknown}");
        let dead = order_at(&client, "http://127.0.0.1:1", "k", "s", "buy", "BTCEUR", 1.0, true).await.unwrap_err();
        assert!(!dead.is_empty() && !dead.contains("MAY"), "a refused connect placed nothing: {dead}");
    }
}
