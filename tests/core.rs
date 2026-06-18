//! Tests for src/core.rs. The asserts live in the always-compiled `core::selftest()`
//! (also runnable via `folioman selftest`); this is the `cargo test` entry point.

#[test]
fn core_pure_logic() {
    folioman::core::selftest();
}
