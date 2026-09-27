//! folioman library: every module the binary and the integration tests share.
//! `main.rs` is a thin dispatcher over this. Unit tests are white-box: each module
//! keeps its asserts in a `#[cfg(test)] mod tests` (reaching privates via `use super::*`),
//! compiled only under `cargo test`. `tests/` holds the few public-API integration tests.

// note: clippy's COSMETIC lints carved out crate-wide — they flag no bugs, only code shape, and
// churn a deliberately dense, working codebase (same call as dropping `cargo fmt --check`). `-D warnings`
// in CI still denies the real-defect groups (correctness/suspicious/perf). Upgrade path: if any of these
// ever masks a genuine issue, drop the allow and fix the spot.
//   doc_lazy_continuation  — rustdoc markdown nit on `//!` list lines; docs render fine.
//   too_many_arguments     — wide fns (quotes/10, print_lane/8) are the real surface; a params struct churns callers.
//   type_complexity        — dense tuple/fn-ptr types; aliased where it reads better, allowed elsewhere.
//   items_after_test_module — fetch.rs grew its fns after the mid-file test mod; org-only, compiles identically.
#![allow(
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::items_after_test_module
)]

// (#390) The page engine (web/engine) compiles this crate for wasm32: only the pure ranking modules
// go in, everything that fetches, trades or runs a command stays native.
#[cfg(not(target_family = "wasm"))]
pub mod broker;
#[cfg(not(target_family = "wasm"))]
pub mod commands;
// (#390) The wasm build's stand-in for the three `commands` paths `picks` still calls by name. Each helper
// now lives in `core`/`picks`; natively `commands` re-exports it from there, so both builds link one
// definition.
#[cfg(target_family = "wasm")]
pub mod commands {
    pub use crate::picks::truncate;
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
pub mod picks;
