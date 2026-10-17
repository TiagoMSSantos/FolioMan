//! Live broker order clients — one file per broker. **REAL MONEY, IRREVERSIBLE.** All
//! credentials come from environment variables only, never `settings.yaml` (secrets must not
//! live in a committed/shared config file). Each broker exposes a single `order(...)` placing
//! one MARKET order; the `trade` command confirm-gates every call.
//!
//! note: market orders only — no limit/stop. Add an order-type arg when actually needed.

pub mod binance;
pub mod tr;
pub mod trading212;

use std::env;

/// Read a required broker credential from the environment (never config).
pub(crate) fn env_var(k: &str) -> Result<String, String> {
    env::var(k).map_err(|_| format!("missing env var {k} (broker credential)"))
}

/// (#667) An order error the broker may not have refused. `trade` prints every `Err` as ORDER FAILED
/// and exits 1, which reads as "nothing happened, retry". That holds only when `definite`: the
/// connection never opened, or the broker answered a 4xx refusal. A request that died after it was
/// written, or a 5xx (Binance documents its 5xx as "execution status UNKNOWN"), may have placed the
/// order, and a reflex retry buys twice. Those errors say so and name the app to check.
pub(crate) fn order_error(app: &str, definite: bool, msg: String) -> String {
    if definite {
        msg
    } else {
        format!("{msg} — the order MAY be live: check the {app} app before retrying")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate in front of every order path: no credential = a NAMED error, never an empty-string
    /// key that would reach a broker as an unauthenticated request. Probed with a variable that
    /// cannot be set rather than by setting one — `std::env::set_var` is process-global and would
    /// race every other test in this binary.
    #[test]
    fn missing_credential_errors_naming_the_variable() {
        let err = env_var("FOLIOMAN_NO_SUCH_BROKER_CREDENTIAL").unwrap_err();
        assert!(err.contains("FOLIOMAN_NO_SUCH_BROKER_CREDENTIAL"), "must name the variable: {err}");
        // PATH is set in every environment this can run in, so the success arm is real too.
        assert!(env_var("PATH").is_ok());
    }

    /// (#667) a definite refusal passes through unchanged; anything else keeps the broker's words and
    /// adds the doubt and the app, so a retry after ORDER FAILED is a check, not a reflex.
    #[test]
    fn an_order_error_past_connect_says_it_may_be_live() {
        assert_eq!(order_error("Binance", true, "binance 400 Bad Request: {}".into()), "binance 400 Bad Request: {}");
        let maybe = order_error("Binance", false, "binance 503 Service Unavailable: {}".into());
        assert!(maybe.starts_with("binance 503 Service Unavailable: {}"), "{maybe}");
        assert!(maybe.contains("MAY be live") && maybe.contains("Binance app"), "{maybe}");
    }
}
