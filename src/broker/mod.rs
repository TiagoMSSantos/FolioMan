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
}
