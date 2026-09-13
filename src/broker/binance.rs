//! Binance spot trading (official API). Auth: `BINANCE_API_KEY` + `BINANCE_API_SECRET`.

use super::env_var;
use reqwest::Client;
use serde_json::Value;

/// Quote/stable assets treated as "cash available to invest".
const CASH_ASSETS: &[&str] = &["EUR", "USD", "USDT", "USDC", "BUSD", "FDUSD"];

/// Signed `/api/v3/account` call → the raw balances array. Shared by `summary` (rendering) and
/// `owned_assets` (round 111 screen overlay). A missing balances array is API drift, not an empty
/// account — say so instead of rendering "(none)".
async fn account_balances(client: &Client) -> Result<Vec<Value>, String> {
    let key = env_var("BINANCE_API_KEY")?;
    let secret = env_var("BINANCE_API_SECRET")?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let query = format!("recvWindow=5000&timestamp={ts}");
    let sig = sign(&secret, &query);
    let url = format!("https://api.binance.com/api/v3/account?{query}&signature={sig}");
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
/// UNGRADEABLE, hence the skip: what is left here after `render_balances` was split out is a
/// credential read and a signed GET against a hardcoded `api.binance.com`. Killing
/// `-> Ok(String::new())` needs either a live socket, or a test that asserts this errors — which
/// only holds while `BINANCE_API_KEY` is unset, so it would pass in CI and fetch for real on a
/// machine that has keys. Neither belongs in the offline suite. The half that carries the logic is
/// pinned below.
#[mutants::skip]
pub async fn summary(client: &Client) -> Result<String, String> {
    Ok(render_balances(&account_balances(client).await?))
}

/// Pure balances→text rendering, split from the fetch so the dust/drift handling is testable offline
/// (like `extract_amounts`). The URL is hardcoded to `api.binance.com`, so the fetch half cannot be
/// reached by a test without pointing a real-money client somewhere else — this is the half worth
/// pinning anyway.
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

/// Binance spot MARKET order via the live API. `symbol` = pair, e.g. `BTCEUR`. `qty` = base
/// asset amount, or euros to spend when `eur` ([`order_query`]). HMAC-signed query, key in the
/// `X-MBX-APIKEY` header.
///
/// UNGRADEABLE, hence the skip, for `summary`'s reason: env credentials and a signed POST to a
/// hardcoded `api.binance.com`. The branch that picks the amount field is [`order_query`], pinned below.
#[mutants::skip]
pub async fn order(client: &Client, side: &str, symbol: &str, qty: f64, eur: bool) -> Result<String, String> {
    let key = env_var("BINANCE_API_KEY")?;
    let secret = env_var("BINANCE_API_SECRET")?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let query = order_query(symbol, side, qty, eur, ts);
    let sig = sign(&secret, &query);
    let url = format!("https://api.binance.com/api/v3/order?{query}&signature={sig}");
    let resp = client
        .post(&url)
        .header("X-MBX-APIKEY", key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
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
            Ok(format!("binance filled: {body}"))
        } else {
            Ok(format!("binance answered {status} but no orderId in the response — VERIFY the order in the app: {body}"))
        }
    } else {
        Err(format!("binance {status}: {body}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// Rendering: stable/fiat lands under `cash` and everything else under `holdings`, dust and
    /// zero rows drop, and an unparsable amount SKIPS the row rather than printing it as 0 — "free 0"
    /// against a real balance is the reading this must never produce.
    #[test]
    fn render_balances_splits_cash_from_holdings() {
        let out = render_balances(&[
            json!({ "asset": "BTC", "free": "0.5", "locked": "0.25" }),
            json!({ "asset": "USDT", "free": "1000", "locked": "0" }),
            json!({ "asset": "ETH", "free": "0", "locked": "0" }),
            json!({ "asset": "SOL", "free": "oops", "locked": "0" }),
        ]);
        let (cash, holdings) = out.split_once("\n  holdings").expect("both blocks present");
        assert!(cash.contains("USDT"), "{out}");
        assert!(!cash.contains("BTC"), "{out}");
        assert!(holdings.contains("BTC"), "{out}");
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
}
