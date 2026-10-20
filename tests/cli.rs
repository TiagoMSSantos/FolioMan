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

use std::collections::BTreeSet;
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

fn repo_file(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// The top-level page-payload keys a source file stamps with `top["k"] = …`: screen's, over the payload
/// the shared builder made, or the page engine's (src/web.rs), over its own.
fn stamped_keys(path: &str) -> BTreeSet<String> {
    repo_file(path)
        .split("top[\"")
        .skip(1)
        .filter_map(|s| s.split_once('"').filter(|(_, rest)| rest.starts_with("] = ")).map(|(k, _)| k.to_string()))
        .collect()
}

/// Whether `src` reads `data.<key>` as a whole name: `data.social` must not pass on `data.social_cagr_floor`.
fn reads_data_key(src: &str, key: &str) -> bool {
    let name = |c: char| c == '_' || c.is_ascii_alphanumeric();
    let pat = format!("data.{key}");
    src.match_indices(&pat).any(|(i, _)| !src[..i].ends_with(name) && !src[i + pat.len()..].starts_with(name))
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
    // (#671) A crypto/FX ticker skips only the statement fetch: the quote (and HICP) fetches run first
    // (src/commands/report.rs), so under `run` this went ONLINE, in the "fully offline" coverage job
    // too, and wrote its caches into the repo root. Isolated and offline: report's dispatch and its
    // crypto branch, no socket.
    let (code, stdout, _) = run_isolated("report-crypto", "{}\n", &[], &["report", "BTC-USD"]);
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

/// (#627) A settings file that does not parse must stop the run, not let it rank on code defaults: `config::load`
/// exits 1 through `config_fatal`, naming the file (a panic would exit 101). Invalid YAML fails before the
/// shared base is parsed, so the case costs nothing; `sim` carries it because it loads the config first.
#[test]
fn sim_with_unparseable_settings_exits_1_naming_the_file() {
    let (code, _, stderr) = run_isolated("sim-bad-settings", "buy_heuristic: [\n", &[], &["sim"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("cannot use config") && stderr.contains("settings.yaml"), "file not named: {stderr}");
}

/// `track` grades journaled books against today's prices, so offline it has nothing to grade — and the
/// contract under test is that it says so instead of printing an empty table as if it were a result.
#[test]
fn track_grades_the_journal_offline() {
    let (code, stdout, stderr) = run_isolated(
        "track-journal",
        "{}\n",
        &[(".screen_snapshots.jsonl", JOURNAL)],
        &["track"],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("Track record"), "header missing: {stdout}");
    // (#771) offline the benchmark is unpriced, and that is the reason the summary must give
    assert!(stdout.contains("summary: benchmark ^GSPC unpriced today"), "ungradeable summary missing: {stdout}");
    assert!(stderr.contains("WARNING: ^GSPC unpriced"), "{stderr}");
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
    // (#627) The page payload: every key screen stamps past the shared builder lands in it and the page reads it.
    // The keys the engine never makes are exactly smoke.mjs's exempt list, which only Pages runs, after the
    // merge: a key missing there reds the publish, and a misspelt one loses its table from the page.
    let payload: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(isolated_root("screen-offline").join(".screen_web.json")).expect("screen writes the page payload"),
    )
    .expect("the payload parses");
    let (stamped, engine) = (stamped_keys("src/commands/screen.rs"), stamped_keys("src/web.rs"));
    let app = repo_file("web/app.js");
    for k in &stamped {
        assert!(payload.get(k).is_some(), "payload lacks `{k}`");
        assert!(reads_data_key(&app, k), "web/app.js never reads data.{k}: the page drops what screen writes");
    }
    // (#530) the keys both stamp are the CAGR floors the page greys or hides rows under; (#746) and the Braked table
    for k in stamped.intersection(&engine) {
        assert!(payload[k].is_number() || k == "braked", "`{k}` must be a number: {}", payload[k]);
    }
    let smoke = repo_file("web/engine/smoke.mjs");
    let line = smoke.lines().find(|l| l.trim_start().starts_with("const ci = [")).expect("smoke.mjs lists its CI-only keys");
    let ci: BTreeSet<String> = line.split('"').skip(1).step_by(2).map(str::to_string).collect();
    let screen_only: BTreeSet<String> = stamped.difference(&engine).cloned().collect();
    assert_eq!(ci, screen_only, "smoke.mjs's `ci` list must name exactly the keys only screen stamps");
}

/// A run that priced nothing must record nothing. The dip journal is a dedup ledger: an entry written
/// for a name that was never actually seen dipping would suppress the real alert when it later does.
/// (#770) Asked for a name, so the run really priced nothing (the default list is empty), and it says so.
#[test]
fn alert_without_prices_records_no_dips() {
    let (code, _, stderr) = run_isolated("alert-offline", "{}\n", &[], &["alert", "AAA"]);
    assert_eq!(code, 0);
    assert!(stderr.contains("WARNING: none of 1 tickers priced"), "{stderr}");
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

/// DIP_CHART plus one 2:1 split dated 2024-01-20, AFTER the JOURNAL's first snapshot (2024-01-05) and
/// BEFORE its second (2024-02-02): a price journaled in January restates, one journaled in February
/// does not. The split sits in `events.splits` next to `meta`, where `parse_chart_raw` reads it.
const SPLIT_CHART: &str = r#"{"chart":{"result":[{"timestamp":[1577836800,1577923200,1578009600,1578096000],"indicators":{"quote":[{"close":[100,80,64,51.2],"volume":[1000,1000,1000,1000]}]},"meta":{"currency":"EUR"},"events":{"splits":{"1705708800":{"date":1705708800,"numerator":2,"denominator":1}}}}]}}"#;

/// A chart that closes on its high: 0% under it, so no dip and a NEAR-HIGH entry state.
const RISING_CHART: &str = r#"{"chart":{"result":[{"timestamp":[1577836800,1577923200,1578009600,1578096000],"indicators":{"quote":[{"close":[100,101,102,103],"volume":[1000,1000,1000,1000]}]},"meta":{"currency":"EUR"}}]}}"#;

/// An index that quadruples-and-more over the window, so a book that lost money is far BEHIND it.
const RISING_SPX: &str = r#"{"chart":{"result":[{"timestamp":[1577836800,1577923200,1578009600,1578096000],"indicators":{"quote":[{"close":[4000,5000,6000,9000],"volume":[1000,1000,1000,1000]}]},"meta":{"currency":"EUR"}}]}}"#;

/// DIP_CHART tagged as a fund by Yahoo's own `instrumentType`, which is what routes a name into the ETF table.
const ETF_CHART: &str = r#"{"chart":{"result":[{"timestamp":[1577836800,1577923200,1578009600,1578096000],"indicators":{"quote":[{"close":[100,80,64,51.2],"volume":[1000,1000,1000,1000]}]},"meta":{"currency":"EUR","instrumentType":"ETF"}}]}}"#;

/// `stub_server` with routes: the first `(needle, status, body)` whose needle occurs in the request line
/// answers it, and a request that no route matches gets an empty 404. An empty needle matches every
/// request, so a catch-all goes LAST. Through the proxy, the request line is absolute-form
/// (`GET http://stub.invalid/ZZDIP?range=10y HTTP/1.1`), so a needle can name a ticker, the ntfy topic or
/// a query parameter; an https call arrives as `CONNECT host:443` and falls to the 404.
fn stub_routed(routes: Vec<(&'static str, u16, &'static str)>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let mut buf = [0u8; 4096];
            let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let line = head.lines().next().unwrap_or("");
            let (status, body) = routes.iter().find(|r| line.contains(r.0)).map_or((404, ""), |r| (r.1, r.2));
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let resp = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    addr
}

/// `run_stubbed` against `stub_routed`. `extra` is appended to the stub overlay (further 2-space `urls:`
/// entries first, then top-level keys; never a second `urls:`), `files` are seeded as in `run_isolated`
/// and `dirs` are created as EMPTY DIRECTORIES in the data root, which is how a test makes a state
/// file's write fail without needing a read-only filesystem.
fn run_routed(
    name: &str,
    routes: Vec<(&'static str, u16, &'static str)>,
    extra: &str,
    files: &[(&str, &str)],
    dirs: &[&str],
    args: &[&str],
) -> (i32, String, String) {
    let proxy = format!("http://{}", stub_routed(routes));
    let overlay = format!("urls:\n  yahoo_chart: \"http://stub.invalid/{{ticker}}?range={{range}}\"\n  ntfy: \"http://stub.invalid/{{topic}}\"\n{extra}");
    let mut cmd = isolated_cmd(name, &overlay, files);
    for d in dirs {
        std::fs::create_dir_all(isolated_root(name).join(d)).expect("seed dir");
    }
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        cmd.env(k, &proxy);
    }
    for k in ["NO_PROXY", "no_proxy", "FOLIOMAN_OFFLINE"] {
        cmd.env_remove(k);
    }
    cmd.args(args);
    finish(&mut cmd, stdin_none())
}

/// `run_isolated` plus empty directories seeded in the data root (see `run_routed`).
fn run_isolated_dirs(name: &str, overlay: &str, files: &[(&str, &str)], dirs: &[&str], args: &[&str]) -> (i32, String, String) {
    let mut cmd = isolated_cmd(name, overlay, files);
    for d in dirs {
        std::fs::create_dir_all(isolated_root(name).join(d)).expect("seed dir");
    }
    cmd.env("FOLIOMAN_OFFLINE", "1");
    cmd.args(args);
    finish(&mut cmd, stdin_none())
}

/// A dropped push is a logged warning and NOT a state change: the dip stays out of the ledger and the
/// entry state stays unwritten, so the next cron run retries both instead of believing they went out.
#[test]
fn alert_warns_and_records_nothing_when_the_push_is_refused() {
    let routes = vec![("ci-smoke-tests-unused", 404, ""), ("range=", 200, DIP_CHART)];
    let (code, _, stderr) = run_routed("alert-refused", routes, "", &[], &[], &["alert", "ZZDIP"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("ntfy push failed for ZZDIP — dip alert NOT delivered"), "{stderr}");
    assert!(stderr.contains("ntfy push failed for market entry state — flip alert NOT delivered"), "{stderr}");
    let root = isolated_root("alert-refused");
    assert!(!root.join(".alert_dips").exists(), "an undelivered dip alert must not enter the dedup ledger");
    assert!(!root.join(".alert_state").exists(), "an undelivered flip must leave the stored state alone");
}

/// Both state files are directories, so each write fails: the run says so on stderr (the cron log) and
/// still exits 0, because a lost dedup entry only risks one repeated ping.
#[test]
fn alert_warns_when_the_state_files_cannot_be_written() {
    let (code, _, stderr) =
        run_routed("alert-unwritable", vec![("", 200, DIP_CHART)], "", &[], &[".alert_dips", ".alert_state"], &["alert", "ZZDIP"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("WARNING: could not persist .alert_dips"), "{stderr}");
    assert!(stderr.contains("WARNING: could not persist .alert_state"), "{stderr}");
    assert!(!stderr.contains("NOT delivered"), "both pushes were delivered: {stderr}");
}

/// A name on its high and a market near its high: nothing to say, so no ping and no dip entry, but the
/// NEAR-HIGH baseline is written silently so that a later flip to a pullback fires.
#[test]
fn alert_sets_a_silent_near_high_baseline_on_the_first_quiet_run() {
    let (code, _, stderr) = run_stubbed("alert-quiet", RISING_CHART, &[], &["alert", "ZZUP"]);
    assert_eq!(code, 0, "{stderr}");
    let root = isolated_root("alert-quiet");
    assert_eq!(std::fs::read_to_string(root.join(".alert_state")).expect("baseline state"), "NEAR-HIGH");
    assert!(!root.join(".alert_dips").exists(), "a name on its high never enters the dip ledger");
    assert!(!stderr.contains("NOT delivered"), "no push was due, so none can have failed: {stderr}");
}

/// One line of the `sim` or `track` output containing `needle`, or a panic that prints the whole output.
fn line_with<'a>(out: &'a str, needle: &str) -> &'a str {
    out.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line containing `{needle}` in:\n{out}"))
}

/// A corrupt line is counted and skipped on stderr, and a journal whose only line carries no price has
/// nothing to buy from: the sim says so instead of printing an empty ledger.
#[test]
fn sim_reports_corrupt_lines_and_an_unpriced_journal() {
    let journal = concat!("this line is not json\n", r#"{"date":"2024-01-05","spx":null,"rows":[["AAA",null]]}"#, "\n");
    let files = [(".screen_snapshots.jsonl", journal)];
    let (code, stdout, stderr) = run_isolated("sim-unpriced", "monthly_deploy_eur: 500\n", &files, &["sim"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("WARNING: 1 corrupt line(s) in .screen_snapshots.jsonl skipped"), "{stderr}");
    assert!(stdout.contains("Nothing bought yet — .screen_snapshots.jsonl has no priced monthly snapshot to buy from."), "{stdout}");
}

/// Every name priced today, one split between the two journaled months: January's prices restate, the
/// replay says how many, and a book that went from 1000 to well over that beats both the index (whose
/// level here is a fifty-euro chart against a 4700 journal) and Serie E's best case.
#[test]
fn sim_restates_split_prices_and_beats_both_benchmarks_on_a_gain() {
    let files = [(".screen_snapshots.jsonl", JOURNAL)];
    let (code, stdout, stderr) = run_routed("sim-split", vec![("", 200, SPLIT_CHART)], "monthly_deploy_eur: 500\n", &files, &[], &["sim"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("note: 2 journaled price(s) restated for share splits since their snapshot"), "{stdout}");
    assert!(stdout.contains("BUYS — each month's advice, and what that €-basket is worth today"), "{stdout}");
    assert!(!stdout.contains("/2 priced"), "every lot is priced, so no `n/m priced` suffix: {stdout}");
    assert!(!stdout.contains("positions priced today"), "all three holdings are priced: {stdout}");
    assert!(stdout.contains("HOLDINGS at today's prices"), "{stdout}");
    assert!(stdout.contains("10.0000"), "BBB and CCC each bought €250 at €25 of today's shares: {stdout}");
    assert!(line_with(&stdout, "same money into the S&P 500 instead").contains("screen ahead by"), "{stdout}");
    assert!(line_with(&stdout, "same money at Série E best case").contains("screen ahead by"), "{stdout}");
    assert!(line_with(&stdout, "flat ×1 every month instead").contains("vs the S&P 500 on that cash"), "{stdout}");
}

/// A journal line from before `spx_off_hi` existed buys at a fallback ×1 and says so; two of the three
/// names never price, so the lots, the holdings and the totals each report the gap; and a book that lost
/// half its money while the index more than doubled is behind both benchmarks.
#[test]
fn sim_reports_unpriced_lots_and_trails_both_benchmarks_on_a_loss() {
    let journal = concat!(
        r#"{"date":"2024-01-05","spx":4700.0,"rows":[["AAA",100.0],["BBB",50.0]]}"#,
        "\n",
        r#"{"date":"2024-02-02","spx":4850.0,"spx_off_hi":-0.5,"rows":[["AAA",110.0],["CCC",25.0]]}"#,
        "\n",
    );
    let routes = vec![("GSPC", 200, RISING_SPX), ("/AAA?", 200, DIP_CHART)];
    let files = [(".screen_snapshots.jsonl", journal)];
    let (code, stdout, stderr) = run_routed("sim-loss", routes, "monthly_deploy_eur: 500\n", &files, &[], &["sim"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("×1 (no S&P state journaled)"), "{stdout}");
    assert!(stdout.contains(", 1/2 priced"), "{stdout}");
    assert!(stdout.contains("(1 of 3 positions priced today"), "{stdout}");
    assert!(!stdout.contains("restated"), "no split in this run: {stdout}");
    assert!(line_with(&stdout, "same money into the S&P 500 instead").contains("screen behind by"), "{stdout}");
    assert!(line_with(&stdout, "same money at Série E best case").contains("screen behind by"), "{stdout}");
}

/// The journal with a CORE row on January's line, so `track` has a tier to look up.
const JOURNAL_CORE: &str = concat!(
    r#"{"date":"2024-01-05","spx":4700.0,"spx_off_hi":-1.5,"rows":[["AAA",100.0],["BBB",50.0]],"core":[["CCC",25.0]]}"#,
    "\n",
    r#"{"date":"2024-02-02","spx":4850.0,"spx_off_hi":-0.5,"rows":[["AAA",110.0],["CCC",25.0]]}"#,
    "\n",
);

/// With prices, a split and a delivered ping: the restatement note counts the CORE row too, both windows
/// grade (and print a row each), the summary line is the one the ping carries, and no warning appears.
#[test]
fn track_grades_split_restated_rows_and_pushes_the_summary() {
    let files = [(".screen_snapshots.jsonl", JOURNAL_CORE)];
    let (code, stdout, stderr) = run_stubbed("track-split", SPLIT_CHART, &files, &["track", "--push"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("note: 3 journaled price(s) restated for share splits since their snapshot"), "{stdout}");
    assert!(line_with(&stdout, "2024-01-05").contains("yes"), "January's book beat the index: {stdout}");
    assert!(line_with(&stdout, "2024-02-02").contains("yes"), "February's book beat the index: {stdout}");
    assert!(stdout.contains("summary: book beat the index in 2/2 windows"), "{stdout}");
    assert!(stdout.contains("CORE hold shortlist"), "{stdout}");
    assert!(!stderr.contains("NOT delivered"), "the stub accepts the ping: {stderr}");
}

/// A refused ping is a stderr warning for the cron log and does not change the printed record.
#[test]
fn track_warns_when_the_summary_ping_is_refused() {
    let routes = vec![("ci-smoke-tests-unused", 404, ""), ("", 200, SPLIT_CHART)];
    let files = [(".screen_snapshots.jsonl", JOURNAL)];
    let (code, stdout, stderr) = run_routed("track-refused", routes, "", &files, &[], &["track", "--push"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("WARNING: ntfy push failed — track summary NOT delivered"), "{stderr}");
    assert!(stdout.contains("summary: book beat the index in 2/2 windows"), "{stdout}");
}

/// A journal that cannot be appended to (here: the file name is a directory) loses one day of record,
/// says so on stderr, and does not stop the screen from printing its ranking.
#[test]
fn screen_warns_when_it_cannot_append_the_journal() {
    let (code, stdout, stderr) = run_isolated_dirs("screen-journal-dir", "{}\n", &[], &[".screen_snapshots.jsonl"], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("could not append .screen_snapshots.jsonl"), "{stderr}");
    assert!(stdout.contains("growth candidates"), "the ranking still prints: {stdout}");
}

/// A `.screen_state.json` as the last `screen` run left it: a two-name BUY NOW book at 30% + 20%, and a
/// five-row CORE shortlist over four breadth tiers (0 all-world, 1 developed, 3 US). C1 is swap-replicated.
const SIZE_STATE: &str = r#"{"date":"2026-10-19","passing":[],"fund_meta":{"C1":[null,"Swap"]},"core_all":["C1","C2","C3","C4","C5"],"core_tier":{"C1":0,"C2":0,"C3":3,"C4":1,"C5":1},"buy_now":[["AAA",30.0],["BBB",20.0]]}"#;

/// Bare `size` prints the book `screen` journalled, at that run's weights, and sends the 50% the two
/// picks leave over to one CORE tracker per market, the US one at double weight, naming the rows it skips.
#[test]
fn size_prints_the_journalled_book_and_spills_the_rest_over_the_core_markets() {
    let (code, stdout, stderr) = run_isolated("size-spill", "{}\n", &[(".screen_state.json", SIZE_STATE)], &["size"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Printing the BUY NOW book of the 2026-10-19 screen run: 2 pick(s) at that run's weights"), "{stdout}");
    assert!(stdout.contains("Suggested sizes — (#313) equal-weight top-"), "{stdout}");
    let (aaa, bbb) = (line_with(&stdout, "  AAA "), line_with(&stdout, "  BBB "));
    assert!(aaa.contains("30.0%") && bbb.contains("20.0%"), "{stdout}");
    assert!(aaa.contains("n/a"), "no quote this run, so no volatility: {aaa}");
    assert!(line_with(&stdout, "  TOTAL ").contains("50.0%"), "{stdout}");
    let c1 = line_with(&stdout, "  C1 ");
    assert!(c1.contains("12.5%") && c1.contains("CORE #1, 2026-10-19 screen") && c1.contains("swap-replicated"), "{c1}");
    assert!(line_with(&stdout, "  C3 ").contains("25.0%"), "the US tracker takes two shares: {stdout}");
    assert!(line_with(&stdout, "  C4 ").contains("12.5%") && line_with(&stdout, "  C4 ").contains("CORE #4"), "{stdout}");
    assert!(!stdout.contains("  C2 ") && !stdout.contains("  C5 "), "one tracker per market, so the second row of a tier is not funded: {stdout}");
    assert!(line_with(&stdout, "  TOTAL+").contains("100.0%"), "{stdout}");
    let home = "the 50.0% the caps could not deploy, split over 3 trackers, across all-world + US + developed, the US one at 2x the others";
    assert!(stdout.contains(home), "{stdout}");
    let skipped = "(2 more row(s) cleared the 20-year hold screen this run and get nothing — 3 funded above: 1 all-world (C2) · 1 developed (C5).";
    assert!(stdout.contains(skipped), "{stdout}");
}

/// The vol-target book (equal weight off) prints its class budgets, and with no CORE shortlist on file the
/// unallocated part has no home. Inflation adjustment off skips the HICP fetch, a zero fund-tilt weight
/// skips the fundamentals enrichment, and a set deploy base prints the deploy line, whose entry state
/// stays unknown offline.
#[test]
fn size_vol_target_book_reports_unallocated_budget_and_the_deploy_line() {
    let overlay = "sizing:\n  equal_weight_book: false\nmonthly_deploy_eur: 1000\ninflation_adjust:\n  enabled: false\nbuy_heuristic:\n  growth_fund_weight: 0\n";
    let state = r#"{"date":"2026-10-19","passing":[],"buy_now":[["AAA",30.0],["BBB",20.0]]}"#;
    let (code, stdout, stderr) = run_isolated("size-vol-target", overlay, &[(".screen_state.json", state)], &["size"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Suggested sizes — weight ∝ score ÷ volatility WITHIN a class budget"), "{stdout}");
    assert!(stdout.contains(" stock/ETF/crypto (renormalised over the classes present)"), "{stdout}");
    assert!(stdout.contains("(50.0% unallocated — the caps bind and no name in those classes can take more"), "{stdout}");
    assert!(!stdout.contains("TOTAL+"), "no tracker takes the remainder: {stdout}");
    assert!(stdout.contains("DEPLOY THIS MONTH: €1000 — base × 1 (S&P 500 entry state unavailable this run)"), "{stdout}");
}

/// `size --picks` sizes only what `screen` ranked; with nothing on file it says so and stops, fetching nothing.
#[test]
fn size_picks_without_a_screen_run_on_file_says_to_run_screen_first() {
    let (code, stdout, stderr) = run_isolated("size-picks-empty", "{}\n", &[], &["size", "--picks"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("No ranked picks on file — run `screen` first, then `size --picks`."), "{stdout}");
    assert!(!stdout.contains("Suggested sizes"), "{stdout}");
}

/// (#774) A state file that does not parse is named on stderr, not read in silence as "no screen run".
#[test]
fn size_warns_when_the_screen_state_file_is_corrupt() {
    let (code, stdout, stderr) =
        run_isolated("size-corrupt-state", "{}\n", &[(".screen_state.json", "{ truncated")], &["size", "--picks"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("WARNING: .screen_state.json exists but is unreadable — sized as if no screen run is on file"), "{stderr}");
    assert!(stdout.contains("No ranked picks on file — run `screen` first, then `size --picks`."), "{stdout}");
}

/// A state with a ranked list but no journalled BUY NOW book re-sizes the ranked names and says which run
/// they came from; here no name has a price, so nothing passes the growth gate.
#[test]
fn size_falls_back_to_the_ranked_list_when_no_buy_book_was_journalled() {
    let state = r#"{"date":"2026-10-19","passing":[],"ranked":["AAA"]}"#;
    let (code, stdout, stderr) = run_isolated("size-ranked-only", "{}\n", &[(".screen_state.json", state)], &["size"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Sizing the 1 ranked pick(s) from the 2026-10-19 screen run."), "{stdout}");
    assert!(stdout.contains("No names pass the growth gate"), "{stdout}");
}

/// `run_isolated` with macro-feed caches seeded under `.fmp_cache` (`<feed>.json` -> body). A day-fresh
/// copy answers before any fetch is tried, so the inflation deflator and the footer read it without a socket.
fn run_macro_seeded(name: &str, overlay: &str, feeds: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let mut cmd = isolated_cmd(name, overlay, &[]);
    let dir = isolated_root(name).join(".fmp_cache");
    std::fs::create_dir_all(&dir).expect("mkdir feed cache");
    for (feed, body) in feeds {
        std::fs::write(dir.join(format!("{feed}.json")), body).expect("seed feed cache");
    }
    cmd.env("FOLIOMAN_OFFLINE", "1");
    cmd.args(args);
    finish(&mut cmd, stdin_none())
}

/// Eurostat JSON-stat for the EU HICP feed, ending in 2099: current on any clock, so the staleness test never trips on it.
const EU_HICP_CURRENT: &str = r#"{"dimension":{"time":{"category":{"index":{"2098-12":0,"2099-12":1}}}},"value":{"0":2.4,"1":2.1}}"#;

/// The Portuguese CPI feed frozen at 2019: stale from March of any later year.
const PT_CPI_FROZEN: &str = r#"{"dimension":{"reference_date":{"category":{"index":["2018-12","2019-12"]}}},"value":[1.0,2.0]}"#;

/// An empty watchlist with the HICP fetch and the fund-factor enrichment both configured off: the table
/// header still prints, nothing is flagged for review, and the run ends cleanly on the macro footer.
#[test]
fn check_with_an_empty_watchlist_flags_nothing_and_skips_the_optional_fetches() {
    let overlay = "inflation_adjust:\n  enabled: false\nbuy_heuristic:\n  growth_fund_weight: 0\n";
    let (code, stdout, stderr) = run_isolated("check-empty-watchlist", overlay, &[], &["check"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PRICE(EUR)"), "table header missing: {stdout}");
    assert!(!stdout.contains("gate review"), "nothing was checked, so nothing can be flagged: {stdout}");
    assert!(stdout.contains("Inflation — latest annual % + cumulative price rise"), "the macro footer still prints: {stdout}");
}

/// `--explain T` names a ticker that is not on the watchlist: it is fetched anyway, and its score
/// walkthrough prints after the table.
#[test]
fn check_explain_fetches_a_ticker_that_is_not_on_the_watchlist() {
    let (code, stdout, stderr) = run_isolated("check-explain", "{}\n", &[], &["check", "--explain", "ZZX"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.lines().any(|l| l.contains("ZZX") && !l.starts_with("--explain")), "ZZX was added to the table: {stdout}");
    assert!(stdout.contains("--explain: ZZX "), "the walkthrough header is missing: {stdout}");
}

/// A fund's top holdings come off the day cache (dated far ahead, so it never counts as due a refetch):
/// the checked ETF gets one line with its weights and the top-10 share, and no crumb handshake is needed.
#[test]
fn check_prints_the_top_holdings_of_a_checked_etf_from_the_day_cache() {
    let cache = r#"{"ZZETF":["2999-01-01",[["HOLDA",0.1],["HOLDB",0.05]],[],null,null]}"#;
    let files = [(".holdings_cache.json", cache)];
    let extra = "buy_heuristic:\n  growth_fund_weight: 0\ninflation_adjust:\n  enabled: false\n"; // no fundamentals or HICP fetch to wait on
    let (code, stdout, stderr) = run_routed("check-etf-holdings", vec![("", 200, ETF_CHART)], extra, &files, &[], &["check", "ZZETF"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Top-10 holdings — what's inside the wrapper:"), "{stdout}");
    assert!(line_with(&stdout, "  ZZETF: HOLDA").contains("HOLDA 10.0%, HOLDB 5.0% (top-10 = 15% of fund)"), "{stdout}");
}

/// The footer tells a healthy series from a frozen one: the EU feed carries a year no clock has reached,
/// the Portuguese one stops in 2019. From March on the old year is called out as stale; in January and
/// February the grace period leaves it as a plain "latest".
#[test]
fn check_footer_marks_a_frozen_inflation_feed_stale_and_a_current_one_healthy() {
    let feeds = [("eu_hicp2", EU_HICP_CURRENT), ("pt_cpi", PT_CPI_FROZEN)];
    let (code, stdout, stderr) = run_macro_seeded("check-macro-footer", "{}\n", &feeds, &["check"]);
    assert_eq!(code, 0, "{stderr}");
    let eu = line_with(&stdout, "(latest 2099)");
    assert!(eu.trim_start().starts_with("EU") && !eu.contains("STALE"), "{eu}");
    let pt = line_with(&stdout, "(latest 2019");
    assert!(pt.trim_start().starts_with("Portugal"), "{pt}");
    let past_grace = {
        use chrono::Datelike;
        chrono::Local::now().date_naive().month() >= 3
    };
    assert_eq!(pt.contains("⚠ STALE — feed frozen at an old year?"), past_grace, "{pt}");
}

/// With `inflation_adjust` off there is no HICP series at all, so `perf` neither fetches nor labels one.
#[test]
fn perf_skips_the_hicp_series_when_inflation_adjustment_is_off() {
    let (code, stdout, stderr) = run_isolated("perf-no-hicp", "inflation_adjust:\n  enabled: false\n", &[], &["perf", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("AAA [AAA]"), "{stdout}");
    assert!(!stdout.contains("inflation-adjusted"), "{stdout}");
}

/// A non-empty HICP series makes the >=1Y legs real, and `perf` says so before the first block.
#[test]
fn perf_labels_the_real_terms_mix_when_a_hicp_series_is_on_file() {
    let (code, stdout, stderr) = run_macro_seeded("perf-hicp-label", "{}\n", &[("eu_hicp2", EU_HICP_CURRENT)], &["perf", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    let label = "(1Y+ % inflation-adjusted — real EUR terms, EU HICP; past prices stay nominal)";
    assert!(stdout.starts_with(label), "{stdout}");
    assert!(stdout.contains("AAA [AAA]"), "{stdout}");
}

/// A priced quote prints a past price and a signed % per horizon that the chart reaches: the 1D leg of
/// the four-bar fixture (64 -> 51.2) is exactly -20%, and the legs it does not reach stay `n/a`.
#[test]
fn perf_prints_a_priced_leg_for_a_horizon_the_chart_reaches() {
    let (code, stdout, stderr) = run_stubbed("perf-priced", DIP_CHART, &[], &["perf", "ZZDIP"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("[ZZDIP]"), "{stdout}");
    let one_day = line_with(&stdout, "  1D ");
    assert!(one_day.contains("-20.0%") && !one_day.contains("n/a"), "{one_day}");
    assert!(line_with(&stdout, "  20Y ").contains("n/a"), "a four-day chart reaches no 20-year leg: {stdout}");
}

/// A worker-thread cap in the config is applied when the runtime is built, and the command still runs.
#[test]
fn a_configured_worker_thread_cap_leaves_the_command_working() {
    let (code, stdout, stderr) = run_isolated("perf-thread-cap", "compute_threads: 2\n", &[], &["perf", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("AAA [AAA]"), "{stdout}");
}

/// Seeds one ticker's parsed SEC rows (`<ticker>_facts26.json`) under `.sec_cache` in a case's data root, AFTER
/// `isolated_cmd` has wiped it. `report` reads a cached file before it asks for a CIK, so the statements need no socket.
fn seed_sec_cache(name: &str, ticker: &str, body: &str) {
    let dir = isolated_root(name).join(".sec_cache");
    std::fs::create_dir_all(&dir).expect("mkdir sec cache");
    std::fs::write(dir.join(format!("{ticker}_facts26.json")), body).expect("seed sec cache");
}

/// Three annual filings in USD for a made-up filer: EPS 2.50, 2.75 and 3.00, no share counts, no pretax line.
const SEC_ROWS_STEADY: &str = concat!(
    r#"[{"filed":"2024-02-15","period_end":"2023-12-31","currency":"USD","revenue":4000000000.0,"gross_margin":41.0,"op_margin":22.0,"net_margin":17.0,"eps":2.5},"#,
    r#"{"filed":"2025-02-15","period_end":"2024-12-31","currency":"USD","revenue":4500000000.0,"gross_margin":41.5,"op_margin":22.5,"net_margin":17.5,"eps":2.75},"#,
    r#"{"filed":"2026-02-15","period_end":"2025-12-31","currency":"USD","revenue":5000000000.0,"gross_margin":42.0,"op_margin":23.0,"net_margin":18.0,"eps":3.0}]"#,
);

/// Statements on file print a fiscal-year table, the grower profile, the valuation cells and the verdict, and the run
/// exits 0. Offline, the quote is an error stub with no close, so the valuation line names the filer's currency and
/// shows no price instead of dividing by something made up.
#[test]
fn report_prints_the_annual_table_and_the_verdict_for_cached_statements() {
    let name = "report-cached-statements";
    let mut cmd = isolated_cmd(name, "{}\n", &[]);
    seed_sec_cache(name, "ZZREP", SEC_ROWS_STEADY);
    cmd.env("FOLIOMAN_OFFLINE", "1");
    cmd.args(["report", "ZZREP"]);
    let (code, stdout, stderr) = finish(&mut cmd, stdin_none());
    assert_eq!(code, 0, "a printed table is a success: {stderr}\n{stdout}");
    assert!(stdout.contains("ZZREP — annual income statements (fiscal-year rollup, newest first · source: SEC EDGAR)"), "{stdout}");
    let newest = line_with(&stdout, "2025");
    assert_eq!(newest.split_whitespace().nth(6), Some("3.00"), "the 2025 row carries the filed EPS: {newest}");
    let valuation = line_with(&stdout, "valuation: eps_ttm");
    assert!(valuation.contains("eps_ttm 3.00  close - (USD)  earnings_yield -"), "{valuation}");
    assert!(stdout.contains("ZZREP — verdict:"), "the verdict closes the block: {stdout}");
}

/// Rows filed BEFORE a split are put on today's share basis with the chart's own split list, so the oldest year reads
/// one basis with the rest (4.00 filed in 2023, a 2:1 split in 2024, prints 2.00 beside the post-split 2.20). The
/// filings are in USD and the listing trades in EUR, so the close is converted first: the stub answers every pair
/// with the 51.2 close, which makes the USD per EUR rate 51.2 and the converted close exactly 1.00.
#[test]
fn report_restates_pre_split_eps_and_prices_the_close_in_the_filers_currency() {
    let name = "report-split-restate";
    let rows = concat!(
        r#"[{"filed":"2023-02-15","period_end":"2022-12-31","currency":"USD","revenue":4000000000.0,"gross_margin":41.0,"op_margin":22.0,"net_margin":17.0,"eps":4.0},"#,
        r#"{"filed":"2024-02-15","period_end":"2023-12-31","currency":"USD","revenue":4500000000.0,"gross_margin":41.5,"op_margin":22.5,"net_margin":17.5,"eps":2.2},"#,
        r#"{"filed":"2025-02-15","period_end":"2024-12-31","currency":"USD","revenue":5000000000.0,"gross_margin":42.0,"op_margin":23.0,"net_margin":18.0,"eps":2.42}]"#,
    );
    let proxy = format!("http://{}", stub_routed(vec![("", 200, SPLIT_CHART)]));
    let overlay = "urls:\n  yahoo_chart: \"http://stub.invalid/{ticker}?range={range}\"\n  ntfy: \"http://stub.invalid/{topic}\"\ninflation_adjust:\n  enabled: false\n";
    let mut cmd = isolated_cmd(name, overlay, &[]);
    seed_sec_cache(name, "ZZSPLIT", rows);
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        cmd.env(k, &proxy);
    }
    for k in ["NO_PROXY", "no_proxy", "FOLIOMAN_OFFLINE"] {
        cmd.env_remove(k);
    }
    cmd.args(["report", "ZZSPLIT"]);
    let (code, stdout, stderr) = finish(&mut cmd, stdin_none());
    assert_eq!(code, 0, "{stderr}\n{stdout}");
    // the market line above the table carries a stale-day count, so match the row by its leading year, not by substring
    let eps_of = |year: &str| {
        let row = stdout.lines().find(|l| l.trim_start().starts_with(year)).unwrap_or_else(|| panic!("no {year} row in:\n{stdout}"));
        row.split_whitespace().nth(6).map(str::to_string)
    };
    assert_eq!(eps_of("2022").as_deref(), Some("2.00"), "a pre-split filing is halved: {stdout}");
    assert_eq!(eps_of("2023").as_deref(), Some("2.20"), "a post-split filing is left alone: {stdout}");
    let valuation = line_with(&stdout, "valuation: eps_ttm");
    assert!(valuation.contains("eps_ttm 2.42  close 1.00 (USD)"), "{valuation}");
}

/// With an FMP key exported the empty-data line stops telling the reader to set one and names the throttle instead.
#[test]
fn report_without_statements_blames_the_throttle_when_an_fmp_key_is_set() {
    let name = "report-fmp-key-set";
    let mut cmd = isolated_cmd(name, "{}\n", &[]);
    cmd.env("FMP_API_KEY", "not-a-real-key");
    cmd.env("FOLIOMAN_OFFLINE", "1");
    cmd.args(["report", "AAA"]);
    let (code, stdout, _) = finish(&mut cmd, stdin_none());
    assert_eq!(code, 1, "zero tables for an equity must exit 1: {stdout}");
    assert!(stdout.contains("FMP is throttled"), "{stdout}");
    assert!(!stdout.contains("set FMP_API_KEY"), "a key is set, so the keyless advice is wrong: {stdout}");
}

/// No tickers on the command line and none in the config: nothing is fetched and nothing printed, and the run is a success.
#[test]
fn report_with_no_tickers_anywhere_prints_nothing_and_succeeds() {
    let (code, stdout, stderr) = run_isolated("report-no-tickers", "{}\n", &[], &["report"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("annual income statements") && !stdout.contains("no statements"), "{stdout}");
}

/// A pinned ticker whose resolved EU twin is already cached is named in a warning (the pin wins the
/// dual-class collapse), a pin the resolver found no twin for is not, and an unreadable SIGI list is
/// said so on stderr instead of silently dropping its rows.
#[test]
fn screen_names_a_pin_that_shadows_its_eu_listing_and_an_unreadable_sigi_list() {
    let files = [(".eu_listing_cache.json", r#"{"AAA":"AAA.DE","BBB":""}"#)];
    let overlay = "tickers:\n  - AAA\n  - BBB\nurls:\n  sigi_pond: \"/nonexistent/sigi-pond.csv\"\n";
    let (code, _, stderr) = run_isolated("screen-shadow-pin", overlay, &files, &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("pinned US tickers shadow their resolved EU listing"), "{stderr}");
    assert!(stderr.contains("): AAA -> AAA.DE"), "{stderr}");
    assert!(!stderr.contains("BBB -> "), "an empty cache value means no usable twin, so nothing is shadowed: {stderr}");
    assert!(stderr.contains("screen: 2 explicit tickers + watchlist"), "{stderr}");
    assert!(stderr.contains("screen: SIGI list /nonexistent/sigi-pond.csv unreadable — no SIGI rows"), "{stderr}");
}

/// Without arguments the universe comes from the fetch legs, so the run counts it by class, and a banked
/// Wikidata pond is cut to the names that are young and liquid enough: offline neither seeded name is.
#[test]
fn screen_without_arguments_counts_the_universe_and_reports_the_wikidata_pond_cut() {
    let files = [(".wikidata_listings.json", r#"[["NEWCO.DE","other"],["FRESH.PA","other"]]"#)];
    let (code, stdout, stderr) = run_isolated("screen-noargs", "{}\n", &files, &["screen"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("tickers in universe ("), "{stderr}");
    assert!(stderr.contains("Wikidata EU pond — 0 of 2 names kept"), "{stderr}");
    assert!(stdout.contains("growth candidates"), "{stdout}");
}

/// With every optional feed dead the run still finishes, and the closing DEGRADED line names each one so a
/// weakened run reads differently from a normal one. A coin with no MVRV is counted on stdout.
#[test]
fn screen_degraded_line_names_the_dead_feeds() {
    let (code, stdout, stderr) = run_isolated("screen-degraded-feeds", "{}\n", &[], &["screen", "BTC-EUR"]);
    assert_eq!(code, 0, "{stderr}");
    let degraded = line_with(&stderr, "screen: DEGRADED");
    assert!(degraded.contains("NUPL feed down (crypto euphoria damping off)"), "{degraded}");
    assert!(degraded.contains("EU HICP feed down (inflation adjustment off)"), "{degraded}");
    assert!(degraded.contains("MVRV feed down (crypto_max_mvrv ceiling off; every coin passes it)"), "{degraded}");
    assert!(stdout.contains("Crypto valuation: 0 of 1 coins carry an MVRV (the rest pass the ceiling free)"), "{stdout}");
}

/// A HICP feed that answers but stops at an old year is not "down": the adjustment still runs, on old
/// rates, and the DEGRADED line says so from March onward (the grace period before the first print).
#[test]
fn screen_marks_a_frozen_eu_hicp_feed_stale_in_the_degraded_line() {
    let frozen = r#"{"dimension":{"time":{"category":{"index":{"2018-12":0,"2019-12":1}}}},"value":{"0":2.4,"1":2.1}}"#;
    let (code, _, stderr) = run_macro_seeded("screen-hicp-stale", "{}\n", &[("eu_hicp2", frozen)], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    let past_grace = {
        use chrono::Datelike;
        chrono::Local::now().date_naive().month() >= 3
    };
    assert_eq!(stderr.contains("EU HICP feed stale (latest 2019 — inflation adjustment using old rates)"), past_grace, "{stderr}");
    assert!(!stderr.contains("EU HICP feed down"), "a non-empty series is not a dead feed: {stderr}");
}

/// The optional stages announce themselves only while their knob is on: the same run with the US-series
/// scoring, the stale-listing drop, the fund tilt and the CORE journal switched off prints neither line.
#[test]
fn screen_with_its_optional_stages_switched_off_skips_their_lines() {
    let (code, _, stderr) = run_isolated("screen-switches-on", "{}\n", &[], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("screen: (#608)") && stderr.contains("screen: fund tilt ("), "the shipped config runs both stages: {stderr}");
    let off = concat!(
        "eu_listing_us_series: false\nstale_days: 0\nbuy_heuristic:\n",
        "  growth_fund_weight: 0\n  growth_momentum_interleave: false\n  journal_core_list: false\n"
    );
    let (code, _, stderr) = run_isolated("screen-switches-off", off, &[], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("screen: (#608)"), "{stderr}");
    assert!(!stderr.contains("screen: fund tilt ("), "{stderr}");
    assert!(isolated_root("screen-switches-off").join(".screen_snapshots.jsonl").is_file(), "the ranking is journalled either way");
}

/// With twin discovery on, the run reports what it found and journals the (here empty) map beside the other
/// state files, so the next run's fetch has a file to read.
#[test]
fn screen_discovers_history_proxies_when_the_knob_is_on() {
    let (code, _, stderr) = run_isolated("screen-history-proxy", "history_proxy_auto: true\n", &[], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    let line = line_with(&stderr, "history_proxy:");
    assert!(line.contains("0 new same-series twin(s) found"), "{line}");
    assert!(line.contains("0 carried forward, journalled for the next run; 0 spliced in this one"), "{line}");
    let journal = std::fs::read_to_string(isolated_root("screen-history-proxy").join(".history_proxy_auto.json")).expect("twin journal");
    assert_eq!(journal.trim(), "{}");
}

/// A previous run's state is the baseline for three alerts: a watchlist name that passed then and cannot be
/// assessed now, a CORE name that dropped out, and a ranked name that dropped out. Each is printed and journalled.
#[test]
fn screen_diffs_the_seeded_state_and_journals_the_changes() {
    let state = r#"{"date":"2026-10-01","passing":["AAA"],"core":["OLDCORE"],"ranked":["OLDRANK"]}"#;
    let (code, stdout, stderr) = run_isolated("screen-seeded-state", "tickers:\n  - AAA\n", &[(".screen_state.json", state)], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Exit review — watchlist names that PASSED all growth gates on 2026-10-01 but fail now"), "{stdout}");
    assert!(stdout.contains("no longer assessable (unknown turnover / structural exclusion)"), "{stdout}");
    assert!(stdout.contains("CORE shortlist changed since 2026-10-01:") && stdout.contains("-OLDCORE"), "{stdout}");
    assert!(stdout.contains("Ranking membership changed since 2026-10-01:") && stdout.contains("-OLDRANK"), "{stdout}");
    let alerts = std::fs::read_to_string(isolated_root("screen-seeded-state").join(".screen_alerts.log")).expect("alert journal");
    assert!(alerts.contains("AAA") && alerts.contains("no longer assessable"), "{alerts}");
    assert!(alerts.contains("-OLDCORE") && alerts.contains("-OLDRANK"), "{alerts}");
}

/// A state file that reads but does not parse resets the baselines: the run says so on stderr, journals the
/// reset, and writes a readable file in its place so the next run starts clean.
#[test]
fn screen_resets_a_corrupt_state_file_and_says_so() {
    let (code, _, stderr) = run_isolated("screen-corrupt-state", "{}\n", &[(".screen_state.json", "{not json")], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("WARNING: .screen_state.json exists but is unreadable — alert baselines"), "{stderr}");
    let root = isolated_root("screen-corrupt-state");
    let alerts = std::fs::read_to_string(root.join(".screen_alerts.log")).expect("alert journal");
    assert!(alerts.contains("exists but is unreadable"), "{alerts}");
    let healed = std::fs::read_to_string(root.join(".screen_state.json")).expect("state file");
    assert!(serde_json::from_str::<serde_json::Value>(&healed).is_ok(), "{healed}");
}

/// The state file is a directory, so the write fails: the run warns on stderr, journals the warning and still
/// exits 0, because one stale baseline costs a repeated alert and a dead screen costs the whole ranking.
#[test]
fn screen_warns_when_it_cannot_persist_its_state() {
    let (code, _, stderr) = run_isolated_dirs("screen-persist-fail", "{}\n", &[], &[".screen_state.json"], &["screen", "AAA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("WARNING: could not persist .screen_state.json — alert baselines stay at the previous run;"), "{stderr}");
    let alerts = std::fs::read_to_string(isolated_root("screen-persist-fail").join(".screen_alerts.log")).expect("alert journal");
    assert!(alerts.contains("could not persist .screen_state.json"), "{alerts}");
}

/// A listing whose newest bar is years old is dropped as stale and named on stderr, and the S&P 500 falling
/// from its high prints the entry-state banner and the deploy line with the drawdown multiplier.
#[test]
fn screen_drops_a_stale_listing_and_prints_the_drawdown_entry_state_and_deploy_line() {
    let routes = vec![("range=", 200, DIP_CHART)];
    let (code, stdout, stderr) = run_routed("screen-dip", routes, "monthly_deploy_eur: 300\n", &[], &[], &["screen", "ZZDIP"]);
    assert_eq!(code, 0, "{stderr}");
    let stale = line_with(&stderr, "stale listing(s)");
    assert!(stale.contains("(>7d since last close)") && stale.contains("ZZDIP ("), "{stale}");
    assert!(stdout.contains("ENTRY STATE: DRAWDOWN — S&P 500"), "{stdout}");
    assert!(stdout.contains("DEPLOY THIS MONTH: €600 — base €300 × 2 (DRAWDOWN entry state)"), "{stdout}");
}

/// Twelve years of daily bars that rise with a slow wave: every rolling 5y and 10y window is positive. The
/// bars are generated, not stored, so the fixture stays a few lines.
fn long_rising_chart() -> String {
    let n = 4400_u64;
    let ts = (0..n).map(|i| (1_262_304_000 + 86_400 * i).to_string()).collect::<Vec<_>>().join(",");
    let closes = (0..n).map(|i| format!("{:.2}", 100.0 + i as f64 * 0.05 + 15.0 * (i as f64 / 150.0).sin())).collect::<Vec<_>>().join(",");
    let vols = (0..n).map(|_| "1000000").collect::<Vec<_>>().join(",");
    format!(
        r#"{{"chart":{{"result":[{{"timestamp":[{ts}],"indicators":{{"quote":[{{"close":[{closes}],"volume":[{vols}]}}]}},"meta":{{"currency":"EUR"}}}}]}}}}"#
    )
}

/// The row under the screen footer whose title contains `title`, or a panic that prints the whole output.
fn footer_row<'a>(out: &'a str, title: &str) -> &'a str {
    let at = out.find(title).unwrap_or_else(|| panic!("no footer `{title}` in:\n{out}"));
    out[at..].lines().nth(1).unwrap_or_else(|| panic!("footer `{title}` has no row in:\n{out}"))
}

/// A pinned name with twelve years of bars gets a row in every closes-derived footer: both consistency
/// windows (always positive for this series), the underwater stretch, the worst 5y and 10y holds and the
/// calendar years. The long monthly history is refused, so everything comes from the daily chart.
#[test]
fn screen_footers_report_long_history_stats_for_a_pinned_name() {
    let body: &'static str = Box::leak(long_rising_chart().into_boxed_str());
    let routes = vec![("range=max", 404, ""), ("range=", 200, body)];
    let extra = "tickers:\n  - ZZLH\nstale_days: 0\n";
    let (code, stdout, stderr) = run_routed("screen-long-history", routes, extra, &[], &[], &["screen", "ZZLH"]);
    assert_eq!(code, 0, "{stderr}");
    for title in ["5y-consistency — % of rolling 5-year windows", "10y-consistency — % of rolling 10-year windows"] {
        let row = footer_row(&stdout, title);
        assert!(row.contains("ZZLH") && row.contains("100%"), "{title}: {row}");
    }
    let underwater = footer_row(&stdout, "Longest underwater — worst stretch below the prior peak");
    assert!(underwater.contains("ZZLH") && underwater.trim_end().ends_with('y'), "{underwater}");
    for title in ["Worst 5-year hold — the single worst rolling 5y", "Worst 10-year hold — the single worst rolling 10y"] {
        let row = footer_row(&stdout, title);
        assert!(row.contains("ZZLH") && row.contains('+'), "every window gained, so the worst is a gain: {title}: {row}");
    }
    let years = footer_row(&stdout, "Calendar years — each full year's return");
    assert!(years.contains("ZZLH") && years.contains("2021 "), "{years}");
}
