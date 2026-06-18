//! Trading212 equity trading (official API). Auth: `TRADING212_API_KEY`.

use super::env_var;
use reqwest::Client;
use serde_json::{json, Value};

async fn get(client: &Client, key: &str, path: &str) -> Result<Value, String> {
    let resp = client
        .get(format!("https://live.trading212.com/api/v0/{path}"))
        .header("Authorization", key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
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
    let num = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let mut out = format!(
        "  cash: free {:.2}  invested {:.2}  total {:.2}",
        num(&cash, "free"),
        num(&cash, "invested"),
        num(&cash, "total"),
    );
    match port.as_array() {
        Some(p) if !p.is_empty() => {
            out.push_str("\n  holdings:");
            for h in p {
                out.push_str(&format!(
                    "\n    {:<12} qty {:<10} @ {:.2}  P/L {:+.2}",
                    h.get("ticker").and_then(|v| v.as_str()).unwrap_or("?"),
                    num(h, "quantity"),
                    num(h, "currentPrice"),
                    num(h, "ppl"),
                ));
            }
        }
        _ => out.push_str("\n  holdings: (none)"),
    }
    Ok(out)
}

/// Trading212 market order via the **live** (production = real money) API. `ticker` must be
/// in Trading212 form, e.g. `AAPL_US_EQ`. buy = +qty, sell = -qty (T212 encodes direction
/// by sign). Key is the raw `Authorization` header value.
pub async fn order(client: &Client, side: &str, ticker: &str, qty: f64) -> Result<String, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let signed = if side == "sell" { -qty } else { qty };
    let resp = client
        .post("https://live.trading212.com/api/v0/equity/orders/market")
        .header("Authorization", key)
        .json(&json!({ "quantity": signed, "ticker": ticker }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(format!("trading212 accepted: {body}"))
    } else {
        Err(format!("trading212 {status}: {body}"))
    }
}
