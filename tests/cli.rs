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
    // strip broker credentials so every case is deterministic on a machine where the user's
    // real keys are exported (no test here may ever reach a live broker call anyway).
    for k in ["TRADING212_API_KEY", "BINANCE_API_KEY", "BINANCE_API_SECRET", "TR_PHONE", "TR_PIN", "TR_ACCEPT_UNOFFICIAL"] {
        cmd.env_remove(k);
    }
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
fn help_prints_usage_exit_0() {
    // asked-for help is not an error: same banner as the catch-all, but exit 0 (CLI convention).
    for flag in ["help", "--help", "-h"] {
        let (code, stdout, _) = run(&[flag], None);
        assert_eq!(code, 0, "{flag} should exit 0");
        assert!(stdout.contains("folioman — review"), "usage banner missing for {flag}: {stdout}");
    }
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
fn accounts_without_creds_skips_all_brokers_exit_0() {
    // with no credentials every broker short-circuits BEFORE any network call, so this runs
    // offline. Pins the documented contract: a broker with no creds (or no API, like Trade
    // Republic) prints its skip reason instead of failing the whole command.
    let (code, stdout, _) = run(&["accounts"], None);
    assert_eq!(code, 0);
    for broker in ["Trading212", "Binance", "Trade Republic"] {
        assert!(stdout.contains(broker), "{broker} header missing: {stdout}");
    }
    assert_eq!(stdout.matches("(skipped)").count(), 3, "expected 3 skipped brokers: {stdout}");
}

#[test]
fn screen_unknown_flag_exit_2() {
    // rejected at arg parse, BEFORE config loading or any network call — a typo'd flag must not
    // silently become a "ticker" that overrides the whole universe with a watchlist-only run.
    let (code, _, stderr) = run(&["screen", "--bogus"], None);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown flag --bogus"), "flag guard missing: {stderr}");
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
