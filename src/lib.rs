//! folioman library: every module the binary and the integration tests share.
//! `main.rs` is a thin dispatcher over this; `tests/` exercises the public API here.
//! Pure-logic asserts live in always-compiled `core::selftest` / `picks::selftest`
//! (so a plain `cargo build`/`run` still type-checks them); the `tests/` files are
//! `#[test]` wrappers `cargo test` runs.

pub mod broker;
pub mod commands;
pub mod config;
pub mod core;
pub mod fetch;
pub mod picks;
