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
    Ok(extract_assets(&account_balances(client).await?))
}

/// Pure extraction, offline-testable: cash/stable and zero-balance rows drop, unparsable rows are
/// skipped — a display overlay must never invent a holding.
fn extract_assets(balances: &[Value]) -> Vec<String> {
    balances
        .iter()
        .filter_map(|b| {
            let asset = b.get("asset").and_then(|v| v.as_str())?;
            let num = |k: &str| b.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
            let held = num("free")? + num("locked")?;
            (held > 0.0 && !CASH_ASSETS.contains(&asset)).then(|| asset.to_string())
        })
        .collect()
}

/// Free + invested balances (read-only). Splits stable/fiat (cash) from the rest (holdings).
pub async fn summary(client: &Client) -> Result<String, String> {
    let balances = account_balances(client).await?;
    let mut cash = Vec::new();
    let mut holdings = Vec::new();
    for b in &balances {
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
    Ok(format!("{}{}", block("cash", &cash), block("holdings", &holdings)))
}

/// HMAC-SHA256(secret, msg) as lowercase hex — Binance's request signature scheme.
pub fn sign(secret: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC any key len");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Binance spot MARKET order via the live API. `symbol` = pair, e.g. `BTCEUR`. `qty` = base
/// asset amount. HMAC-signed query, key in the `X-MBX-APIKEY` header.
pub async fn order(client: &Client, side: &str, symbol: &str, qty: f64) -> Result<String, String> {
    let key = env_var("BINANCE_API_KEY")?;
    let secret = env_var("BINANCE_API_SECRET")?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let side_u = side.to_uppercase(); // BUY / SELL
    let query = format!(
        "symbol={symbol}&side={side_u}&type=MARKET&quantity={qty}&recvWindow=5000&timestamp={ts}"
    );
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

    /// (round 111) owned-assets extraction: cash/stable and zero rows drop, unparsable rows skip.
    #[test]
    fn extract_assets_drops_cash_zero_and_unparsable() {
        let rows = vec![
            json!({ "asset": "BTC", "free": "0.5", "locked": "0" }),
            json!({ "asset": "EUR", "free": "100", "locked": "0" }),
            json!({ "asset": "ETH", "free": "0", "locked": "0" }),
            json!({ "asset": "SOL", "free": "oops", "locked": "0" }),
        ];
        assert_eq!(extract_assets(&rows), vec!["BTC"]);
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
