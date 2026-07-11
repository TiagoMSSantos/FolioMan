//! End-to-end tests of the compiled `folioman` binary, OFFLINE. Every case here is a path that
//! validates/aborts BEFORE any network or `yes` confirmation, so no live API is touched. Driven via
//! `CARGO_BIN_EXE_folioman` (set by cargo for integration tests) — no `assert_cmd` dependency.
//!
//! The money path (`trade`) is the highest-stakes surface: these pin its arg-validation guards and the
//! fat-finger confirm gate so a refactor can't silently let a malformed or unconfirmed order through.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run the binary with `args` and optional stdin; return (exit_code, stdout, stderr).
fn run(args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_folioman"));
    // point config::load() at the committed fixture: the private config/settings.yaml is
    // gitignored, so in CI any subcommand that reaches config loading would panic without this.
    cmd.env("FOLIOMAN_CONFIG", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml"));
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn folioman");
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    } // dropping stdin (None case) closes it -> a read returns EOF, never blocks
    let out = child.wait_with_output().expect("wait folioman");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn unknown_subcommand_prints_usage_exit_2() {
    let (code, stdout, _) = run(&["wat"], None);
    assert_eq!(code, 2);
    assert!(stdout.contains("folioman — review"), "usage banner missing: {stdout}");
}

#[test]
fn trade_wrong_arg_count_exit_2() {
    let (code, _, stderr) = run(&["trade", "binance", "buy"], None); // 3 args, needs 4
    assert_eq!(code, 2);
    assert!(stderr.contains("usage: folioman trade"), "trade usage missing: {stderr}");
}

#[test]
fn trade_non_positive_qty_exit_2() {
    let (code, _, stderr) = run(&["trade", "binance", "buy", "BTCEUR", "-1"], None);
    assert_eq!(code, 2);
    assert!(stderr.contains("QTY must be"), "qty guard missing: {stderr}");
}

#[test]
fn trade_bad_side_exit_2() {
    let (code, _, stderr) = run(&["trade", "binance", "hodl", "BTCEUR", "1"], None);
    assert_eq!(code, 2);
    assert!(stderr.contains("side must be"), "side guard missing: {stderr}");
}

#[test]
fn report_crypto_prints_no_statement_offline() {
    // `-` in the ticker (crypto/FX) short-circuits BEFORE any fetch (src/commands/report.rs), so
    // this exercises report's binary dispatch + its only offline branch without touching the network.
    let (code, stdout, _) = run(&["report", "BTC-USD"], None);
    assert_eq!(code, 0);
    assert!(stdout.contains("no income statement (crypto/FX)"), "crypto guard missing: {stdout}");
}

#[test]
fn trade_abort_at_confirm_no_order() {
    // valid args reach the fat-finger gate; typing anything but "yes" aborts BEFORE any broker call.
    let (code, stdout, _) = run(&["trade", "binance", "buy", "BTCEUR", "1"], Some("no\n"));
    assert_eq!(code, 0);
    assert!(stdout.contains("aborted."), "expected abort, got: {stdout}");
}
