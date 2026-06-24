//! folioman library: every module the binary and the integration tests share.
//! `main.rs` is a thin dispatcher over this. Unit tests are white-box: each module
//! keeps its asserts in a `#[cfg(test)] mod tests` (reaching privates via `use super::*`),
//! compiled only under `cargo test`. `tests/` holds the few public-API integration tests.

// ponytail: doc_lazy_continuation is rustdoc markdown pedantry (wants every `//!` list-ish line
// re-indented); docs render fine, fix re-trips on edits. One allow over churning every module doc.
#![allow(clippy::doc_lazy_continuation)]

pub mod broker;
pub mod commands;
pub mod config;
pub mod core;
pub mod fetch;
pub mod picks;
