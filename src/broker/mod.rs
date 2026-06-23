//! Live broker order clients — one file per broker. **REAL MONEY, IRREVERSIBLE.** All
//! credentials come from environment variables only, never `settings.yaml` (secrets must not
//! live in a committed/shared config file). Each broker exposes a single `order(...)` placing
//! one MARKET order; the `trade` command confirm-gates every call.
//!
//! ponytail: market orders only — no limit/stop. Add an order-type arg when actually needed.

pub mod binance;
pub mod tr;
pub mod trading212;

use std::env;

/// Read a required broker credential from the environment (never config).
pub(crate) fn env_var(k: &str) -> Result<String, String> {
    env::var(k).map_err(|_| format!("missing env var {k} (broker credential)"))
}
