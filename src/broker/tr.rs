//! Trade Republic — **no official/public API.** Unofficial web-login flow (phone + PIN, SMS
//! 2FA on stdin) against their app endpoints — ToS-gray, breaks on app updates. Guarded by
//! `TR_ACCEPT_UNOFFICIAL=1`. Auth: `TR_PHONE` (E.164, e.g. `+3519...`) + `TR_PIN`.

use super::env_var;
use reqwest::Client;
use serde_json::{json, Value};
use std::io::Write;

/// Run the unofficial TR login, then stop.
///
/// note: login is implemented and real; **order placement is NOT**. TR sends orders over
/// an undocumented websocket protocol that shifts between app releases — guessing it would be
/// a bug farm that silently mis-trades real money, the one thing never to fake. So this
/// authenticates, proves the session, and stops with a clear error. Upgrade path: vendor a
/// maintained client (e.g. `pytr`) and port its current order message, or wait for an
/// official API. Until then place TR orders in the app.
pub async fn order(client: &Client, side: &str, isin: &str, qty: f64) -> Result<String, String> {
    if env_var("TR_ACCEPT_UNOFFICIAL").unwrap_or_default() != "1" {
        return Err("Trade Republic has no official API. Set TR_ACCEPT_UNOFFICIAL=1 to use the \
                    unofficial (ToS-gray, fragile) login flow."
            .into());
    }
    let phone = env_var("TR_PHONE")?;
    let pin = env_var("TR_PIN")?;

    // 1. start web login -> processId; TR texts an SMS code
    let login = client
        .post("https://api.traderepublic.com/api/v1/auth/web/login")
        .json(&json!({ "phoneNumber": phone, "pin": pin }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !login.status().is_success() {
        return Err(format!("TR login failed: {} {}", login.status(), login.text().await.unwrap_or_default()));
    }
    let login_json: Value = login.json().await.map_err(|e| e.to_string())?;
    let process_id = login_json
        .get("processId")
        .and_then(|v| v.as_str())
        .ok_or("TR login: no processId in response (protocol changed?)")?
        .to_string();

    // 2. SMS 2FA code from stdin
    print!("TR SMS code: ");
    std::io::stdout().flush().ok();
    let mut code = String::new();
    std::io::stdin().read_line(&mut code).map_err(|e| e.to_string())?;
    let code = code.trim();
    let verify = client
        .post(format!("https://api.traderepublic.com/api/v1/auth/web/login/{process_id}/{code}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !verify.status().is_success() {
        return Err(format!("TR 2FA failed: {} {}", verify.status(), verify.text().await.unwrap_or_default()));
    }
    // Session cookie now held by the cookie store. Order placement is intentionally not
    // implemented (see fn doc) rather than guessed against real money.
    Err(format!(
        "TR authenticated OK ({side} {qty} {isin}), but live order placement is not implemented: \
         it requires TR's undocumented websocket order protocol, which would break (and could \
         mis-trade) on any app update. Place this order in the TR app, or vendor a maintained \
         client. See src/broker/tr.rs."
    ))
}

/// Cash/portfolio (read-only). Same blocker as orders: TR's balance + positions feed runs
/// over the undocumented websocket, not a stable REST endpoint, so this isn't implemented.
/// Upgrade path: vendor `pytr` and port its current `cash`/`portfolio` subscriptions.
pub async fn summary(_client: &Client) -> Result<String, String> {
    Err("Trade Republic has no official API; cash/portfolio read runs over its undocumented \
         websocket and is not implemented. Check the TR app, or vendor a maintained client \
         (e.g. pytr)."
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `summary` is deliberately unimplemented, and that has to stay LOUD. A stub returning `Ok`
    /// would print an empty account as though TR held nothing — the failure mode this whole file's
    /// doc argues against. No network, no credentials: the error is the entire function.
    #[tokio::test]
    async fn summary_is_an_explicit_not_implemented() {
        let client = Client::builder().no_proxy().build().expect("test client");
        let err = summary(&client).await.unwrap_err();
        assert!(err.contains("not implemented"), "{err}");
        assert!(err.contains("websocket"), "must say WHY, so nobody re-attempts it blind: {err}");
    }
}
