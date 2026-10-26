//! folioman library: every module the binary and the integration tests share.
//! `main.rs` is a thin dispatcher over this. Unit tests are white-box: each module
//! keeps its asserts in a `#[cfg(test)] mod tests` (reaching privates via `use super::*`),
//! compiled only under `cargo test`. `tests/` holds the few public-API integration tests.

// (#1123) clippy's cosmetic allows live in Cargo.toml's `[lints.clippy]`, which covers every target.

// (#390) The page engine (web/engine) compiles this crate for wasm32: only the pure ranking modules
// go in, everything that fetches, trades or runs a command stays native.
#[cfg(not(target_family = "wasm"))]
pub mod broker;
#[cfg(not(target_family = "wasm"))]
pub mod commands;
// (#390) The wasm build's stand-in for the two `commands` paths `picks` still calls by name. Each helper
// now lives in `core`/`picks`; natively `commands` re-exports it from there, so both builds link one
// definition. (#863) `truncate` left: only the native `check` table calls it.
#[cfg(target_family = "wasm")]
pub mod commands {
    pub mod backtest {
        pub(crate) use crate::core::{next_rand, percentile};
    }
    pub mod size {
        pub(crate) use crate::picks::equal_weights;
    }
}
pub mod config;
pub mod core;
#[cfg(not(target_family = "wasm"))]
pub mod fetch;
pub mod heuristics;
pub mod picks;
pub mod web;
