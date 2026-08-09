//! End-to-end tests of the compiled `folioman` binary, OFFLINE. Driven via `CARGO_BIN_EXE_folioman`
//! (set by cargo for integration tests) — no `assert_cmd` dependency.
//!
//! Two families, and the difference matters when adding a case:
//!
//! - **`run`** — paths that validate/abort BEFORE any network or `yes` confirmation. The money path
//!   (`trade`) is the highest-stakes surface: these pin its arg-validation guards and the fat-finger
//!   confirm gate so a refactor can't silently let a malformed or unconfirmed order through.
//! - **`run_isolated`** — commands that COMPUTE from disk (`sim`, `track`), fan out over tickers
//!   (`screen`, `check`, `perf`, `size`, `report`) or PERSIST state (`screen`, `alert`). These reach
//!   real command bodies, under `FOLIOMAN_OFFLINE=1` so no socket is opened, and in a data root of
//!   their own so nothing they write touches the working tree.
//!
//! SCOPE NOTE, so nobody reads more safety into this file than it carries: the mutation gate grades
//! `--lib --test backtest_fixture`. `tests/cli.rs` is NOT in that killing suite, so a case here raises
//! coverage and pins observable behaviour, but earns no mutation protection. Logic worth grading
//! belongs in a `--lib` test.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Broker credentials are stripped from every spawn so cases stay deterministic on a machine where
/// the user's real keys are exported. No test here may reach a live broker call regardless.
fn strip_broker_creds(cmd: &mut Command) {
    for k in ["TRADING212_API_KEY", "BINANCE_API_KEY", "BINANCE_API_SECRET", "TR_PHONE", "TR_PIN", "TR_ACCEPT_UNOFFICIAL"] {
        cmd.env_remove(k);
    }
}

/// Spawn, feed `stdin`, collect (exit_code, stdout, stderr).
fn finish(cmd: &mut Command, stdin: Option<&str>) -> (i32, String, String) {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
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

/// Run the binary with `args` and optional stdin; return (exit_code, stdout, stderr).
fn run(args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_folioman"));
    // point config::load() at the committed fixture: the private config/settings.yaml is
    // gitignored, so in CI any subcommand that reaches config loading would panic without this.
    cmd.env("FOLIOMAN_CONFIG", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml"));
    strip_broker_creds(&mut cmd);
    cmd.args(args);
    finish(&mut cmd, stdin)
}

/// Run against an ISOLATED data root: a per-test directory under `CARGO_TARGET_TMPDIR` holding its own
/// `config/settings.yaml`, seeded with `files`.
///
/// The isolation is the whole point. `config::data_path` anchors to the config file's GRANDPARENT, so
/// a run pointed at the shared `tests/ci-settings.yaml` writes `.screen_snapshots.jsonl`,
/// `.screen_state.json`, `.alert_dips` and friends into the REPO. Pointing it at a temp root moves
/// every one of those into the temp root — which is what makes it safe to drive commands that persist
/// state, and what lets a test SEED that state and assert on what the command computes from it.
///
/// `overlay` is deep-merged over the committed `tests/ci-settings.yaml`, still located by the upward
/// walk from the binary's own directory, so these runs inherit the shipped tuning instead of
/// re-declaring it. Pass `"{}"` to change nothing.
///
/// `FOLIOMAN_OFFLINE=1` — no socket may be opened. These cases assert what a command computes from
/// disk; a fetch reaching the network would make them slow, flaky and dependent on a live market.
fn run_isolated(name: &str, overlay: &str, files: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let root = isolated_root(name);
    let _ = std::fs::remove_dir_all(&root); // a stale root from a previous run must not grade this one
    std::fs::create_dir_all(root.join("config")).expect("mkdir data root");
    std::fs::write(root.join("config/settings.yaml"), overlay).expect("write overlay");
    for (file, body) in files {
        std::fs::write(root.join(file), body).expect("seed data root");
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_folioman"));
    cmd.env("FOLIOMAN_CONFIG", root.join("config/settings.yaml"));
    cmd.env("FOLIOMAN_OFFLINE", "1");
    strip_broker_creds(&mut cmd);
    // FMP_API_KEY changes report's empty-data wording, so a machine that exports one would read a
    // different string than CI. Removed here, which also makes the keyless branch the asserted one.
    cmd.env_remove("FMP_API_KEY");
    cmd.args(args);
    finish(&mut cmd, stdin_none())
}

/// Where `run_isolated` puts a case's data root. Exposed so a test can assert on what the command
/// PERSISTED there, which is both the interesting half of some commands and the standing proof that
/// the isolation works — a dot-file appearing here is one that is not appearing in the repo.
fn isolated_root(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Spelled out so `run_isolated`'s call sites don't all carry a bare `None` that needs a turbofish.
fn stdin_none() -> Option<&'static str> {
    None
}

/// Two months of journal, priced, with the S&P state present so the deploy multiplier is known
/// rather than defaulted. Deliberately fixed past dates: the buy events are then the same on any
/// day this runs. (Do NOT assert on accrued "pending" months — that count grows with wall clock.)
const JOURNAL: &str = concat!(
    r#"{"date":"2024-01-05","spx":4700.0,"spx_off_hi":-1.5,"rows":[["AAA",100.0],["BBB",50.0]]}"#,
    "\n",
    r#"{"date":"2024-02-02","spx":4850.0,"spx_off_hi":-0.5,"rows":[["AAA",110.0],["CCC",25.0]]}"#,
    "\n",
);

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
fn sim_without_deploy_base_exit_0() {
    // the CI fixture carries monthly_deploy_eur: 0, so `sim` gates off with the knob hint BEFORE
    // reading the journal or touching the network — deterministic on any machine.
    let (code, stdout, _) = run(&["sim"], None);
    assert_eq!(code, 0);
    assert!(stdout.contains("monthly_deploy_eur"), "knob gate missing: {stdout}");
}

#[test]
fn trade_abort_at_confirm_no_order() {
    // valid args reach the fat-finger gate; typing anything but "yes" aborts BEFORE any broker call.
    let (code, stdout, _) = run(&["trade", "binance", "buy", "BTCEUR", "1"], Some("no\n"));
    assert_eq!(code, 0);
    assert!(stdout.contains("aborted."), "expected abort, got: {stdout}");
}

/// The full paper-DCA replay off a seeded journal — the one command that computes a real result with
/// no network at all, since every price it needs was journaled at rank time.
///
/// The quantities are the assertion that matters. AAA's 4.7536 is two months of arithmetic: January
/// splits €500 over two names and takes a €1 fee off each (€249 ÷ €100 = 2.49), February does the same
/// at a higher price (€249 ÷ €110 = 2.2636). A fee applied to the wrong side of the split, or a budget
/// divided before the fee, moves that number — which is exactly what a silent regression here looks
/// like. Nothing wall-clock-dependent is asserted: the accrued "pending cash" months grow every month
/// this test survives, so they are deliberately left alone.
#[test]
fn sim_replays_the_journal_offline() {
    let (code, stdout, _) = run_isolated(
        "sim-replay",
        "monthly_deploy_eur: 500\n",
        &[(".screen_snapshots.jsonl", JOURNAL)],
        &["sim"],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("Paper DCA"), "header missing: {stdout}");
    assert!(stdout.contains("2024-01-05  ×1  invested €500 (fees €2)"), "january buy missing: {stdout}");
    assert!(stdout.contains("2024-02-02  ×1  invested €500 (fees €2)"), "february buy missing: {stdout}");
    assert!(stdout.contains("4.7536"), "AAA qty (2.49 + 2.2636 over two months) missing: {stdout}");
    assert!(stdout.contains("invested €1000 since 2024-01-05"), "summary missing: {stdout}");
    // offline every holding is unpriceable: it must degrade to a dash, never to a fabricated zero
    assert!(stdout.contains("→ now n/a"), "unpriced basket should read n/a: {stdout}");
    assert!(stdout.contains("0 of 3 positions priced today"), "priced count missing: {stdout}");
}

#[test]
fn sim_without_journal_says_so() {
    let (code, stdout, _) = run_isolated("sim-empty", "monthly_deploy_eur: 500\n", &[], &["sim"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("No journal yet"), "empty-journal hint missing: {stdout}");
}

/// `track` grades journaled books against today's prices, so offline it has nothing to grade — and the
/// contract under test is that it says so instead of printing an empty table as if it were a result.
#[test]
fn track_grades_the_journal_offline() {
    let (code, stdout, _) = run_isolated(
        "track-journal",
        "{}\n",
        &[(".screen_snapshots.jsonl", JOURNAL)],
        &["track"],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("Track record"), "header missing: {stdout}");
    assert!(stdout.contains("nothing gradeable yet"), "ungradeable summary missing: {stdout}");
}

#[test]
fn track_without_journal_says_so() {
    let (code, stdout, _) = run_isolated("track-empty", "{}\n", &[], &["track"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("No track record yet"), "empty-journal hint missing: {stdout}");
}

/// A garbage line must be skipped and COUNTED, not silently dropped: the journal is append-only and a
/// truncated write is how it goes bad, so a run that quietly grades fewer months than it has is the
/// failure mode worth naming out loud.
#[test]
fn track_warns_on_corrupt_journal_lines() {
    let corrupt = format!("{JOURNAL}not json at all\n");
    let (code, _, stderr) = run_isolated(
        "track-corrupt",
        "{}\n",
        &[(".screen_snapshots.jsonl", &corrupt)],
        &["track"],
    );
    assert_eq!(code, 0);
    assert!(stderr.contains("1 corrupt line(s)"), "corrupt-line warning missing: {stderr}");
}

/// `perf`'s per-ticker block, every horizon printed. Offline the quote is an err stub, which is the
/// point: a name with no data must still render its row with `n/a` per horizon rather than vanish or
/// abort the run — the self-swallowing contract `quote_one` documents.
#[test]
fn perf_prints_a_block_per_ticker_offline() {
    let (code, stdout, _) = run_isolated("perf-offline", "{}\n", &[], &["perf", "AAA"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("AAA [AAA]"), "ticker block missing: {stdout}");
    for horizon in ["1D", "1W", "1M", "1Y", "5Y"] {
        assert!(stdout.contains(horizon), "{horizon} row missing: {stdout}");
    }
}

/// `check` renders its table and then the ranked sections. With no priced name every gate rejects, and
/// the run must still exit 0 — an empty ranking is a valid answer, not a failure.
#[test]
fn check_renders_table_and_empty_rankings_offline() {
    let (code, stdout, _) = run_isolated("check-offline", "{}\n", &[], &["check", "AAA"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("PRICE(EUR)"), "table header missing: {stdout}");
    assert!(stdout.contains("(none pass the gates)"), "empty ranking missing: {stdout}");
}

#[test]
fn size_without_candidates_says_nothing_to_size() {
    let (code, stdout, _) = run_isolated("size-offline", "{}\n", &[], &["size", "AAA"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("No names pass the growth gate"), "size hint missing: {stdout}");
}

/// The full screen pipeline offline: universe assembly, the buyability and quality filters, all three
/// ranked sections, and the journal append at the end.
///
/// The journal assertion is the one worth having twice over. It pins `append_snapshot` — the write
/// that every later `track` and `sim` run grades against — and it is the standing proof that
/// `run_isolated` isolates: this file materialises in the temp root, which is precisely why it is not
/// materialising in the working tree, where a `screen` under the shared config would have put it.
#[test]
fn screen_ranks_and_journals_offline() {
    let (code, stdout, _) = run_isolated("screen-offline", "{}\n", &[], &["screen", "AAA"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("growth candidates"), "ranking preamble missing: {stdout}");
    for section in ["max stocks", "max ETFs", "max crypto"] {
        assert!(stdout.contains(section), "{section} section missing: {stdout}");
    }
    assert!(
        isolated_root("screen-offline").join(".screen_snapshots.jsonl").is_file(),
        "screen must journal its ranking — and into the DATA ROOT, not the repo: {stdout}"
    );
}

/// A run that priced nothing must record nothing. The dip journal is a dedup ledger: an entry written
/// for a name that was never actually seen dipping would suppress the real alert when it later does.
#[test]
fn alert_without_prices_records_no_dips() {
    let (code, _, _) = run_isolated("alert-offline", "{}\n", &[], &["alert"]);
    assert_eq!(code, 0);
    assert!(
        !isolated_root("alert-offline").join(".alert_dips").exists(),
        "no priced name dipped, so no dedup entry may be written"
    );
}

/// The exit-code contract: asked about one equity, produced zero statement tables -> exit 1, so a cron
/// can tell total failure from a partial run. The market line alone does not count as success.
#[test]
fn report_equity_without_statements_exit_1() {
    let (code, stdout, _) = run_isolated("report-offline", "{}\n", &[], &["report", "AAA"]);
    assert_eq!(code, 1, "zero tables for an equity must exit 1: {stdout}");
    assert!(stdout.contains("no statements"), "empty-data line missing: {stdout}");
}
