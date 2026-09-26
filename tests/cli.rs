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
//! - **`run_stubbed`** — (#359) `run_isolated` ONLINE against a loopback stub, for the two things
//!   offline cannot reach: a PRICED quote and a DELIVERED push.
//!
//! SCOPE NOTE, so nobody reads more safety into this file than it carries: the mutation gate grades
//! `--lib --test backtest_fixture`. `tests/cli.rs` is NOT in that killing suite, so a case here raises
//! coverage and pins observable behaviour, but earns no mutation protection ON PUSH. (#357) A census
//! dispatched with `tests: --lib --test backtest_fixture --test cli` does grade against it, and the
//! (#359) slice (brokers + small commands) is receipted that way — by hand, not per push. Logic worth
//! grading on every push still belongs in a `--lib` test.

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
    let mut cmd = isolated_cmd(name, overlay, files);
    cmd.env("FOLIOMAN_OFFLINE", "1");
    cmd.args(args);
    finish(&mut cmd, stdin_none())
}

/// (#359) `run_isolated`, but ONLINE against a loopback stub instead of offline. One local server
/// answers `body` to every request, and all three proxy variables point at it with `NO_PROXY`
/// stripped, so a request to ANY host lands there: plain http is served, https dies at the TLS
/// handshake after its CONNECT. Nothing leaves the machine. The overlay moves `yahoo_chart` and
/// `ntfy` to plain http (the host is never resolved; the proxy takes it), so charts and pushes get
/// `body` and every other call fails the way a dead network does.
fn run_stubbed(name: &str, body: &'static str, files: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let proxy = format!("http://{}", stub_server(body));
    let overlay = "urls:\n  yahoo_chart: \"http://stub.invalid/{ticker}?range={range}\"\n  ntfy: \"http://stub.invalid/{topic}\"\n";
    let mut cmd = isolated_cmd(name, overlay, files);
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        cmd.env(k, &proxy);
    }
    for k in ["NO_PROXY", "no_proxy", "FOLIOMAN_OFFLINE"] {
        cmd.env_remove(k);
    }
    cmd.args(args);
    finish(&mut cmd, stdin_none())
}

/// Serves `body` to every connection on an ephemeral loopback port for the rest of the test process.
/// The lib suite's `fetch::tests::stub_server` in miniature, `Connection: close` included and for the
/// same reason (see there).
fn stub_server(body: &'static str) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let _ = std::io::Read::read(&mut sock, &mut [0u8; 4096]); // drain the request line+headers
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    addr
}

/// The data root, config overlay and credential scrub both runners share; the caller picks online or
/// offline.
fn isolated_cmd(name: &str, overlay: &str, files: &[(&str, &str)]) -> Command {
    let root = isolated_root(name);
    let _ = std::fs::remove_dir_all(&root); // a stale root from a previous run must not grade this one
    std::fs::create_dir_all(root.join("config")).expect("mkdir data root");
    std::fs::write(root.join("config/settings.yaml"), overlay).expect("write overlay");
    for (file, body) in files {
        std::fs::write(root.join(file), body).expect("seed data root");
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_folioman"));
    cmd.env("FOLIOMAN_CONFIG", root.join("config/settings.yaml"));
    strip_broker_creds(&mut cmd);
    // FMP_API_KEY changes report's empty-data wording, so a machine that exports one would read a
    // different string than CI. Removed here, which also makes the keyless branch the asserted one.
    cmd.env_remove("FMP_API_KEY");
    cmd
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

/// (#359) Trade Republic's unofficial login stays opt-in. Confirmed with "yes" but without
/// `TR_ACCEPT_UNOFFICIAL=1` (stripped, like every credential here), the order fails naming the switch,
/// before the phone and PIN are read or a login request is made.
#[test]
fn trade_tr_without_opt_in_fails_naming_the_switch() {
    let (code, _, stderr) = run(&["trade", "tr", "buy", "IE00B4L5Y983", "1"], Some("yes\n"));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("ORDER FAILED") && stderr.contains("TR_ACCEPT_UNOFFICIAL"), "{stderr}");
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
/// The quantities are the assertion that matters. AAA's 4.7727 is two months of arithmetic: January
/// splits €500 over two names (€250 ÷ €100 = 2.5), February does the same at a higher price
/// (€250 ÷ €110 = 2.2727). (#302) Offline no line carries a quote currency and none is a coin, so every
/// lot pays €0 in broker fees; the fee arithmetic itself is pinned in `sim`'s unit tests. A budget split
/// wrongly, or a price read off the wrong month, moves that number — which is exactly what a silent
/// regression here looks like. Nothing wall-clock-dependent is asserted: the accrued "pending cash" months grow every month
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
    assert!(stdout.contains("2024-01-05  ×1  invested €500 (fees €0)"), "january buy missing: {stdout}");
    assert!(stdout.contains("2024-02-02  ×1  invested €500 (fees €0)"), "february buy missing: {stdout}");
    assert!(stdout.contains("4.7727"), "AAA qty (2.5 + 2.2727 over two months) missing: {stdout}");
    assert!(stdout.contains("invested €1000 since 2024-01-05"), "summary missing: {stdout}");
    // offline every holding is unpriceable: it must degrade to a dash, never to a fabricated zero
    assert!(stdout.contains("→ now n/a"), "unpriced basket should read n/a: {stdout}");
    assert!(stdout.contains("0 of 3 positions priced today"), "priced count missing: {stdout}");
    assert!(stdout.contains("flat ×1 every month instead: n/a"), "(#311) flat-cash line missing: {stdout}");
    assert!(stdout.contains("equal-weight top-10 on that flat cash: n/a"), "(#312) top-10 line missing: {stdout}");
    assert!(stdout.contains("equal_weight_book on that flat cash: n/a"), "(#314) knob twin line missing: {stdout}");
    assert!(stdout.contains("same money at Série E best case 4.5%/yr: €"), "(#315) Série E line missing: {stdout}");
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
    // (#359) The fixture has `inflation_adjust` on, but offline there is no HICP series, so every %
    // stayed nominal: the "real EUR terms" header may only claim what the numbers under it are.
    assert!(!stdout.contains("inflation-adjusted"), "empty HICP must leave the label off: {stdout}");
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
    // (#359) No Trading212 key (stripped), so no cash figure: the line stays silent rather than print a
    // balance nobody fetched.
    assert!(!stdout.contains("Broker cash"), "cash line without a key: {stdout}");
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
///
/// Doubles as the ONLY test of `FOLIOMAN_OFFLINE` against the Yahoo quoteSummary path. That path
/// signs its own requests and so bypassed the `get_json`/`get_text` guard: this test went green on a
/// box that cannot reach Yahoo and red on CI, which can — CI's `report AAA` fetched a live fund
/// profile and exited 0. `fetch::yahoo_crumb` carries the guard now. Keep the ticker a REAL fund
/// (AAA is one): a made-up symbol would pass whether or not the guard exists.
#[test]
fn report_equity_without_statements_exit_1() {
    let (code, stdout, _) = run_isolated("report-offline", "{}\n", &[], &["report", "AAA"]);
    assert_eq!(code, 1, "zero tables for an equity must exit 1: {stdout}");
    assert!(stdout.contains("no statements"), "empty-data line missing: {stdout}");
}

/// (#359) A chart that fell 100 -> 51.2: 48.8% under its high, so over the fixture's `drop_pct` 5 and
/// past the 15% DRAWDOWN line. Every bar-to-bar step is 0.8x, inside the splice trim's (0.5, 2.0).
const DIP_CHART: &str = r#"{"chart":{"result":[{"timestamp":[1577836800,1577923200,1578009600,1578096000],"indicators":{"quote":[{"close":[100,80,64,51.2],"volume":[1000,1000,1000,1000]}]},"meta":{"currency":"EUR"}}]}}"#;

/// (#359) Both pings DELIVERED. ZZDIP enters the dedup ledger NEXT TO BBB, a name this run never read,
/// which stays: an args-scoped `alert` must not wipe the cron's state for everything else. The entry
/// state is persisted because its push was delivered, and no "NOT delivered" warning prints for a push
/// that was.
#[test]
fn alert_records_a_delivered_dip_and_entry_state() {
    let (code, _, stderr) = run_stubbed("alert-dip", DIP_CHART, &[(".alert_dips", "BBB\n")], &["alert", "ZZDIP"]);
    assert_eq!(code, 0, "{stderr}");
    let root = isolated_root("alert-dip");
    assert_eq!(std::fs::read_to_string(root.join(".alert_dips")).expect("dip ledger"), "BBB\nZZDIP\n");
    assert_eq!(std::fs::read_to_string(root.join(".alert_state")).expect("entry state"), "DRAWDOWN");
    assert!(!stderr.contains("NOT delivered"), "{stderr}");
}

/// (#359) The same run when no chart parses: every quote is an err stub reading a 0.0 drop and 0.0 off
/// the high, and neither may count as a recovery. ZZDIP stays in the ledger and the stored DRAWDOWN
/// stays put; a real 0.0 would have cleared the one, and flipped the other to NEAR-HIGH and pushed it.
#[test]
fn alert_does_not_read_a_stub_quote_as_a_recovery() {
    let seeded = [(".alert_dips", "ZZDIP\n"), (".alert_state", "DRAWDOWN")];
    let (code, _, stderr) = run_stubbed("alert-stub", "{}", &seeded, &["alert", "ZZDIP"]);
    assert_eq!(code, 0, "{stderr}");
    let root = isolated_root("alert-stub");
    assert_eq!(std::fs::read_to_string(root.join(".alert_dips")).expect("dip ledger"), "ZZDIP\n");
    assert_eq!(std::fs::read_to_string(root.join(".alert_state")).expect("entry state"), "DRAWDOWN");
}
