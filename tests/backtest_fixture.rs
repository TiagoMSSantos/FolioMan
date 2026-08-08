//! FROZEN-DATA BACKTEST PIN — the deterministic half of "did this change break the backtest".
//!
//! The nightly gate (`tests/network.rs::backtest_edge_holds`) grades REGIME: does the edge still hold
//! on today's market. It cannot grade CODE, because a live run has no way to tell a scoring regression
//! from a bad quarter — which is why its thresholds are `> 0` and why an edge falling +117 -> +3 passes
//! it green. This file is the other half: same code, frozen market. Every number the report prints is
//! pinned against a committed golden, so ANY change in behaviour — a moved knob, a reassociated sum, a
//! vanished row, a renamed column — reds here, offline, in seconds.
//!
//! WHY A GOLDEN BLOCK AND NOT FIVE SCALARS. `walk_forward_edge_pin` and
//! `shipped_tuning_scores_fixture_unchanged` (src/commands/backtest.rs) already pin scalars, and they
//! pin them on hand-built samples: the first runs its OWN copy of the cutoff loop, the second scores a
//! synthetic universe. Neither ever calls `run()`. So the held-book construction, benchmark alignment,
//! FX conversion, class stamping, de-meaning and the whole report — the numbers CI greps — had no
//! deterministic net at all. Five scalars catch five regressions; a golden block also catches the row
//! that changed, the row that vanished and the row that appeared.
//!
//! IT DOUBLES AS THE MARKER CONTRACT, and that is now an ASSERTION rather than a paragraph — see
//! `gate_markers_are_all_in_the_golden` below, which checks all 14 of `backtest::GATE_MARKERS`
//! against the 12y golden. This paragraph used to make the claim in prose and name 7 of the 14;
//! nothing executable enforced it, and `tests/network.rs` held its own second copy of the strings.
//! Renaming a report line therefore means re-blessing this golden, and the diff is the review.
//!
//! DETERMINISM IS STRUCTURAL, not hoped for. `FOLIOMAN_OFFLINE=1` makes every outbound helper in
//! fetch.rs return None without opening a socket, makes the committed cache immortal (no TTL) and
//! makes it read-only (no write-back). The cutoff walk is driven by the DATES IN THE SERIES, never by
//! the clock — `chrono::Local::now()` appears in the backtest only as the verdict's display date — so
//! this pin does not drift as the calendar advances.
//!
//! Measured, not assumed (2026-08-07, at the blessed golden):
//! - `strace -f -e trace=connect,socket` over the whole process tree: **0 socket(), 0 connect()**.
//!   Not "no HTTP" — no DNS, no socket of any family. The offline switch is a hard structural cut.
//! - three consecutive runs, byte-identical. The first attempt was NOT: `bootstrap_edge_ci` seeded its
//!   draw off `HashMap` iteration order, which Rust randomizes per process, so the bootstrap band and
//!   its "STRADDLES 0" verdict moved run to run on identical data. This pin is what caught it (nothing
//!   else in the repo runs the same data twice); the fix was `BTreeMap`.
//! - debug and release output byte-identical, so the golden is profile-independent — blessing in
//!   release cannot red a debug CI job.
//! - runtime 3.3s release for all six pins (11.1s if they ran sequentially — the harness overlaps
//!   them). The 200-ticker recipe below is kept at its debug cost deliberately: this is the ONLY
//!   end-to-end net over real data, and trimming it to hit a stopwatch target would trade the branches
//!   it covers for seconds on a suite that already runs minutes.
//! - the 20y and 8y goldens differ from the 12y one on 143 of 226 lines, so the horizon rungs are
//!   genuinely different code, not a re-print. The `stress` golden differs on only 8 of 226 — it is
//!   near-duplicate and is documented as such on its own test rather than oversold here.
//!
//! Trip-verified, because a pin nothing perturbs is a test that passes forever. Nudging
//! `growth_accel_weight` 0.65 -> 0.66 in tests/ci-settings.yaml — a 1.5% move on ONE knob — reds FIVE
//! of the six: 12y and stress at `Spearman +0.19 -> +0.18`, 8y at `edge +0.9 -> -7.5`, 20y in the
//! bootstrap band, halflife at `8y hold edge +0.9 -> -7.5`.
//!
//! `tune` did NOT move on that nudge, and the honest reading is a SENSITIVITY FLOOR, not an inert pin:
//! it scores a 70/30 chronological split that is re-de-meaned per half, and 1.5% on one weight lands
//! under its printed precision. Probed further to prove it can move at all — `growth_trend_weight`
//! 0.15 -> 0.30 reds it on line 3 (`rho +0.26 -> +0.32`, `edge +144.5 -> +243.4`). So the tune golden
//! catches knob-scale changes and code changes, not hairline ones. Both perturbations reverted.
//!
//! MEASURED WITH MUTATION TESTING, because "it catches regressions" is a claim, not a number. 217
//! mutants over the twelve functions the printed numbers flow through (`edge_halves`, `winsor_edge`,
//! `lane_metrics`, `turnover_frac`, `percentile`, `bootstrap_edge_ci`, `demean`, `book_stats`,
//! `exit_cohorts`, `cohort_stats`, `edge_terciles`, `core::spearman`), graded with THESE SIX GOLDENS
//! AS THE ONLY KILLING SUITE: **200 caught, 16 missed, 1 unviable — 92.6%**. So the pin does not
//! merely pass; a change to almost any arithmetic behind the report reds it.
//!
//! Every one of the 16 survivors is a TOO-FEW-ROWS GUARD (`len < 4`, `n < 3`, `tops.len() < 2`) or a
//! defensive clamp. That is the shape of what a golden over healthy data cannot see, and it is a
//! property of the approach rather than a hole to plug here: every lane this fixture scores carries
//! hundreds of rows, so `< 4` and `<= 4` produce byte-identical reports. Those boundaries belong in
//! unit tests and now have them (`spread_guards_hold_at_their_boundary`, and the additions to
//! `book_stats_topn_held_book` / `percentile_nearest_rank` in src/commands/backtest.rs). Re-graded
//! against the full lib suite the count went **4/16 -> 15/16 killed**; the one left alive is
//! documented in place at `bootstrap_edge_ci`'s `edges.len() < iters / 2`.
//!
//! Reproducing the audit (~56 min, `-j 1` — this box OOMs its linker on concurrent cargo builds):
//!     cargo mutants -f src/commands/backtest.rs -f src/core.rs \
//!       -F 'edge_halves|winsor_edge|lane_metrics|turnover_frac|percentile|bootstrap_edge_ci|demean|book_stats|exit_cohorts|cohort_stats|edge_terciles|spearman' \
//!       -E 'book_stats .* with Some' --profile mutants --copy-target=false -j 1 -- --test backtest_fixture
//! The `-E` drops 2187 mutants that only permute `book_stats`'s 7-tuple return; `--profile mutants`
//! (Cargo.toml) is opt-level 2, which turns a 50s-per-mutant run into a ~15s one.
//!
//! ROUND 3 WIDENED THE AUDIT PAST THOSE TWELVE FUNCTIONS, and the class it found was not arithmetic
//! at all. Grading a 10% slice of the whole module (944 gradeable mutants once the tuple-return
//! permutations are excluded; ~16s each, so the full sweep is ~4.5h and only ever runs as a slice):
//!     cargo mutants -f src/commands/backtest.rs -E 'replace .* -> .* with Some' \
//!       --profile mutants --copy-target=false -j 1 --shard 0/10 -- --lib --test backtest_fixture
//! The survivors were the VERDICT JOURNAL and the ARGUMENT PARSER — code the goldens run through but
//! never vary. `tuning_fingerprint` could return a constant (killing the screen footer's stale-tuning
//! warning in both directions), `read_verdict`/`write_verdict` could become stubs, and every arm of
//! `backtest`'s command line — `universe`, `long`, `fund`, `insider` — could stop being recognised,
//! all with six green goldens, because these pins only ever pass `12`/`20`/`8`/`tune`/`halflife`/
//! `stress` and none of them reads a verdict. Fixed by splitting the pure halves out (`parse_args`,
//! `merge_verdict`, `latest_verdict`) and pinning them in src/commands/backtest.rs; 14 of those
//! mutants were then re-checked one by one and all 14 die.
//!
//! What is left alive is documented where it lives, not here: the two `std::fs` one-liners in
//! `read_verdict`/`write_verdict`, the `FMP_API_KEY` advisory print, and `bootstrap_edge_ci`'s
//! `edges.len() < iters / 2`. Killing any of them needs process-global state (`FOLIOMAN_CONFIG`, the
//! environment) that would race every other test in the binary.
//!
//! A guard class ALSO stays deliberately unpinned: the ~10 `report_*`/`emit_*` too-few-rows guards
//! (`bd.len() < 2`, `scored.len() < 8`, …). Those functions return `()` and print, so the golden IS
//! their natural pin — and it structurally cannot reach their boundaries, because every lane here
//! carries hundreds of rows. Pinning them means splitting compute from render across a dozen report
//! functions, which is a far larger change than the bug it would catch.
//!
//! CI grades the DIFF, not a slice: `.github/workflows/ci.yml`'s `mutants` job runs
//! `cargo mutants --in-diff` on every push, so new code must be killed by tests that land with it.
//!
//! KNOWN GAPS, stated rather than implied:
//! - the run takes the NARROW path (an explicit `tickers:` list), which leaves `etf_set`/`sector_of`
//!   empty (backtest.rs, `(#46)`). The chart-meta class stamping is covered; the index-membership
//!   braces over it are not. Closing it needs the WIDE path, which needs a live index-member fetch.
//! - the DAILY-cadence path (`backtest 5` — `years < 8 && !long`) has no end-to-end coverage and
//!   deliberately gets none here. `fetch_history` goes through `chart_json` (fetch.rs), which has NO
//!   disk cache — only `chart_json_long` does — so every pin above is monthly by construction.
//!   Fixturing the daily path means adding a cache to the fetch layer for tests alone, which is a
//!   worse trade than the gap. `backtest 8` sits ON the boundary and pins the `MAX monthly` label, so
//!   a change to the `years >= 8` threshold still reds.
//! - these six pins cover the modes that run OFFLINE. `fund` and `insider` need live APIs and
//!   `universe` needs a live index fetch, so none of the three is pinned.
//!
//! Re-blessing after an INTENDED change:
//!     FOLIOMAN_BLESS=1 cargo test --release --test backtest_fixture -- --nocapture
//! Regenerating the frozen cache from a warm real one (rare — only to add tickers or refresh history):
//!     cargo test --release --test backtest_fixture -- --ignored regen

use std::path::PathBuf;

/// The frozen universe: 200 tickers, chosen to span every branch the backtest can take rather than
/// sampled at random. A regeneration must reproduce this list, so the recipe is recorded here:
///
/// - `^GSPC` — the benchmark; the held-book comparison is meaningless without it
/// - the `STRESS_TICKERS` that Yahoo still serves — crashed-and-alive (GE, INTC, NOK, CCL…) plus
///   bankrupt series that end near zero (FRCB, SBNY)
/// - short history (24..120 monthly closes) — the `history`/`young` gates and the no-long-leg bail
/// - the 180..240 band — an 8Y leg but no 20Y one, where `long_leg_fixed` falls back a rung
/// - crypto, including stablecoins (USDC-EUR, USDG-USD) — the crypto gate legs AND the refusal path
/// - leveraged names (3BRL.L, SQQQ.MI, XACT-BULL-2.ST) — the `leveraged` structural refusal
/// - all five instrumentTypes Yahoo returns: ETF 107, EQUITY 67, CRYPTOCURRENCY 21, MUTUALFUND 4, INDEX 1
/// - ten quote currencies (USD 97, EUR 63, GBP 9, GBp 8, CHF 5, SEK 5, DKK 4, MXN 4, PLN 4, NZD 1) —
///   GBp vs GBP is the pence/pounds trap, and every non-USD name exercises FX conversion
/// - 92 dividend payers — `events.dividends`, `dividends_in_window`, the dividend term
const FIXTURE_TICKERS: &[&str] = &[
    "020Y.L", "0A08.L", "0A09.L", "0E2B.IL", "0FLE.L", "0P0001ON2S.CO", "0P00035XN8.F", "0TPE.L", "0VPX.L",
    "10AL.DE", "30GB.L", "31ID.AS", "33ID.AS", "3BRL.L", "3DUE.DE", "3FNP.L", "3SEM.L", "3UBS.MI", "5ESGE.MI",
    "AAP3.L", "AAVE-EUR", "ACGL", "ACT60.MI", "ACWUKD.SW", "AEE", "AEMU.L", "AGSGX.XC", "AGUG.AS", "AIFS.DE",
    "AIG", "AK8G.DE", "ALAU.L", "ALTR.LS", "AMZ3.L", "APT-USD", "ARK3.L", "ASML", "AUEM.L", "AVB", "AVERE.MI",
    "AVGO", "AWDSR.PA", "B41J.DE", "B4NE.DE", "B4NN.DE", "BA", "BBEG.DE", "BCFK.DE", "BCH-EUR", "BENE.L",
    "BID3.L", "BIIB", "BNKT.AS", "BS30.L", "BTC-EUR", "BTCN.AS", "BUG.L", "C", "CBDE.DE", "CBDG.L",
    "CBSEUD.SW", "CCL", "CD9.PA", "CEMC.DE", "CEUH.DE", "CHRW", "CHSE.DE", "CI2U.L", "CLMA.MI", "CM9.PA",
    "CMOD.L", "CMU.PA", "CNEW.L", "COF", "COIY.L", "COMS.SW", "COR.LS", "CRM3.L", "CRO-EUR", "CSGP",
    "CSPXXN.MX", "CT2B.AS", "CWE.PA", "D6RA.DE", "DBMFE.PA", "DGX", "DHYD.AS", "DIA", "DOT-EUR", "DTM.AS",
    "DXCM", "EDEP.DE", "EGL.LS", "ELV", "EPAD.AS", "ETFBCASH.WA", "ETFBNDXPL.WA", "ETFBSPXPL.WA",
    "ETFBW20LV.WA", "EUGO.MI", "EUN1.DE", "EW", "F", "FDXF", "FE", "FERG", "FGPT.L", "FIL-EUR", "FOXA", "FRCB",
    "FTAD.L", "GCVG.L", "GE", "GOOG", "GRMN", "GRWY.L", "HBAR-EUR", "HMJA.L", "I50G.L", "IBS.LS", "IBTC.SW",
    "IEGMX.XC", "IGLD.DE", "IGTM.L", "INTC", "INXG.L", "IPR.LS", "IWRD.L", "JBL", "JGCV.SW", "JGPD.DE",
    "JIREF", "JRID.L", "JST-USD", "KCS-USD", "KHC", "KIM", "LQDMX.XC", "LTC-EUR", "LU3003218107-USD.LU", "M",
    "M-USD", "MAA", "MAJDCU.CO", "MAJLO.CO", "MCP.LS", "MMM", "MONTLEV.ST", "MRNA", "MZL0.DE", "NBA.LS",
    "NCLH", "NEXO-USD", "NOK", "NWL", "ODFL", "PFG", "PI-EUR", "PMIOSU.CO", "PRIJ.L", "PSRM.L", "PWR",
    "QNT-EUR", "RSG", "S5SD.DE", "SBNY", "SGSU.L", "SJHY.L", "SJM", "SKY-EUR", "SMIEX.SW", "SP5G.L", "SPOL.L",
    "SQQQ.MI", "SUOE.L", "T", "T10A.L", "TECH", "TRES.L", "TRX-EUR", "UAL", "UNIC.L", "USAS.PA", "USDC-EUR",
    "USDG-USD", "UST10D.SW", "VFC", "VRT", "WBD", "WDSD.DE", "WLD-USD", "WOEE.DE", "WYNN", "XACT-BULL-2.ST",
    "XACT-NORDEN.ST", "XACT-SVERIGE.ST", "XISP.DE", "XLM-EUR", "XMEM.L", "^GSPC",
];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `config::data_path` anchors to the settings file's GRANDPARENT, so a config at
/// `tests/fixture/config/settings.yaml` makes `tests/fixture/` the whole data root — the frozen cache
/// is picked up by the ordinary cache path, with no test-only plumbing inside `fetch.rs`.
fn fixture_dir() -> PathBuf {
    repo().join("tests/fixture")
}

/// The ONE thing the golden must not carry: an absolute machine path, which differs per checkout and
/// would make a locally-blessed golden unmatchable in CI. That is portability, not determinism.
///
/// Round 1 filtered three more things. All three are gone, because a line dropped here is a line NO
/// LONGER PINNED and an inert filter is worse than none — if such a line ever does appear it gets
/// silently swallowed instead of reddening. Measured across all six pinned runs:
/// - `elapsed` — matches nothing; `backtest` never prints a timing.
/// - `run 20` — aimed at the verdict line, which (a) actually starts `Method backtest (run …`, (b) is
///   rendered by `verdict_line` for the SCREEN footer, not by this command, and (c) is gated behind
///   `wide && >= MIN_VERDICT_TICKERS` (500), which no fixture run reaches at 200/208 tickers. The
///   clock (`chrono::Local::now`) only ever reaches the WRITTEN JSON verdict, never stdout.
/// - `backtest: N tickers, …` — fully deterministic, and dropping it discarded both the ticker count
///   and the `MAX monthly` / `10y daily` label. That label is the visible read on
///   `monthly = long || years >= 8`, which is the whole reason the 8y and 20y goldens exist. Pinned now.
fn normalize(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.contains(env!("CARGO_MANIFEST_DIR")))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// THE PIN. Runs the real binary over the frozen cache and diffs the whole report against a golden.
///
/// Each mode gets its OWN `#[test]` rather than a loop inside one, purely so the test harness runs
/// them concurrently: sequential the six runs are 11.1s release / ~150s debug, in parallel it is the
/// slowest single run (~3.5s / ~47s). Peak RSS is 38 MB per run, so six at once is ~230 MB — this
/// repo's OOM history is the linker, not test processes.
fn pin(args: &[&str], golden_name: &str) {
    let cache = fixture_dir().join(".long_history_cache.json");
    assert!(
        cache.is_file(),
        "frozen cache missing at {} — regenerate it with \
         `cargo test --release --test backtest_fixture -- --ignored regen` from a warm real cache",
        cache.display()
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_folioman"))
        .arg("backtest")
        .args(args)
        .env("FOLIOMAN_CONFIG", fixture_dir().join("config/settings.yaml"))
        .env("FOLIOMAN_OFFLINE", "1") // no socket may be opened; a fixture miss must not become a live fetch
        .output()
        .expect("spawn folioman");
    assert!(out.status.success(), "backtest exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    let got = normalize(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));

    let golden_path = fixture_dir().join(golden_name);
    if std::env::var("FOLIOMAN_BLESS").is_ok_and(|v| !v.is_empty()) {
        std::fs::write(&golden_path, &got).expect("write golden");
        eprintln!("BLESSED {} ({} lines) — review the diff before committing", golden_path.display(), got.lines().count());
        return;
    }
    let want = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|e| panic!("read {}: {e} — bless it with FOLIOMAN_BLESS=1", golden_path.display()));

    if got != want {
        // First differing line, then the counts: a whole-file dump of a 200-line report buries the
        // one line that moved, which is the only thing a reader needs.
        let (gl, wl): (Vec<_>, Vec<_>) = (got.lines().collect(), want.lines().collect());
        let at = gl.iter().zip(&wl).position(|(a, b)| a != b);
        let detail = match at {
            Some(i) => format!("first difference at line {}:\n  golden: {}\n  got   : {}", i + 1, wl[i], gl[i]),
            None => format!("identical prefix, length differs: golden {} lines, got {}", wl.len(), gl.len()),
        };
        panic!(
            "`backtest {}` changed on FROZEN data — the market cannot have moved, so this is a \
             code or knob change.\n{detail}\n\nIf it was intended: re-validate with a live \
             `folioman backtest universe` (both OOS halves positive) exactly as the ci-settings \
             receipts require, then re-bless with \
             `FOLIOMAN_BLESS=1 cargo test --release --test backtest_fixture`.",
            args.join(" ")
        );
    }
}

/// 12 is >= 8, so this takes the MONTHLY path — the same branch the wide nightly gate uses
/// (`monthly = long || years >= 8`) and the one with decades of bars.
#[test]
fn backtest_report_is_pinned_on_frozen_data() {
    pin(&["12"], "backtest-12.golden");
}

/// 20y is the horizon SHIP RULE v2 leads on and the screen footer quotes. Longest forward window ->
/// fewest cutoffs survive it, so it exercises the thin end of the rung ladder.
#[test]
fn backtest_20y_report_is_pinned() {
    pin(&["20"], "backtest-20.golden");
}

/// 8y sits exactly ON the `years >= 8` monthly boundary. If that threshold is ever edited, this run
/// drops to daily cadence and the pinned `MAX monthly history` label flips — which is precisely the
/// regression the label is now kept in the golden to catch.
#[test]
fn backtest_8y_report_is_pinned() {
    pin(&["8"], "backtest-8.golden");
}

/// The knob search. This one carries the most weight of the five: `tune_growth`'s doc promises
/// "Seeded xorshift64 (no `rand` dep) so a re-run is identical" — the SAME reproducibility claim
/// `bootstrap_edge_ci` made and broke on `HashMap` iteration order (fixed in the round that added
/// this file). Nothing verified it until now, and `tune`'s entire job is telling a human whether to
/// move a shipped knob.
#[test]
fn backtest_tune_report_is_pinned() {
    pin(&["12", "tune"], "backtest-12-tune.golden");
}

/// The hold-period sweep — which forward window to actually hold. A wholly separate code path
/// (`hold_period_sweep` -> `sweep_cutoffs`), and the only report in the tool with no other net.
#[test]
fn backtest_halflife_report_is_pinned() {
    pin(&["12", "halflife"], "backtest-12-halflife.golden");
}

/// Survivorship stress: crashed/delisted losers folded into the pool. Frankly the weakest of the
/// five — the injection+dedup logic is already unit-tested (`stress_injection_dedups`) and the
/// resulting report is ~97% the 12y one. Pinned anyway because it costs 2.5s and settles the
/// question, but do not read it as covering something the 12y golden does not.
#[test]
fn backtest_stress_report_is_pinned() {
    pin(&["12", "stress"], "backtest-12-stress.golden");
}

/// THE MARKER CONTRACT, as an assertion rather than a claim.
///
/// `tests/network.rs::backtest_edge_holds` has no parser — it string-searches this report for every
/// number it grades. Until now, "every marker is in the golden" was prose in this file's module doc,
/// verified once by hand and never again. It was also incomplete: it named 7 of the 14.
///
/// The failure this closes is specific. Ten of the fourteen markers fail SOFT in the gate, and three
/// of those (`tickers:`, `── GROWTH`, `windows scored:`) make it `return false` — SKIP — which is
/// GREEN. Those three are soft on purpose: they are the throttle guard, and a live wide run that
/// Yahoo throttles must skip rather than red. The problem is that a RENAME and a THROTTLE are then
/// indistinguishable — same "SKIPPED" line, same green — so a renamed report row silently switches
/// the gate off and nothing ever says so.
///
/// This test can tell them apart because it has no network to blame: the report is generated offline
/// from a frozen cache, so a missing marker has exactly one cause.
///
/// Deliberately checked against the 12y golden ONLY. The other five modes print subsets (`tune` and
/// `halflife` have no GROWTH lane at all), and the gate itself only ever runs the wide 20/12/8y
/// report — asserting the full set against a mode that structurally cannot carry it would be a
/// failing test dressed as coverage.
#[test]
fn gate_markers_are_all_in_the_golden() {
    let golden_path = fixture_dir().join("backtest-12.golden");
    let golden = std::fs::read_to_string(&golden_path).expect("read backtest-12.golden");
    let missing: Vec<_> = folioman::commands::backtest::GATE_MARKERS
        .iter()
        .filter(|m| !golden.contains(**m))
        .collect();
    assert!(
        missing.is_empty(),
        "tests/network.rs greps the backtest report for these strings and {missing:?} no longer \
         appear in {}. The nightly gate's reaction to a missing marker is to SKIP, which is GREEN — \
         so this rename would have disarmed the gate silently. Either restore the report wording, or \
         update backtest::markers AND re-bless the goldens in the same commit.",
        golden_path.display()
    );
}

/// Rebuild `tests/fixture/.long_history_cache.json` from a warm real one. `#[ignore]`d: it needs the
/// developer's own ~125 MB cache, which CI and a fresh clone do not have.
///
/// Trims what the parser provably never reads — `open`/`high`/`low` are absent from every read site in
/// the project (`parse_chart` takes `close` and `volume` from `quote[0]`, `adjclose` only behind
/// `use_adjusted_close`) — and rounds to 4 decimal places. That is 7.3 MB of raw envelopes down to
/// ~2.4 MB, through the REAL `parse_chart` and the REAL cache path, so the end-to-end claim survives
/// the trim. `adjclose` is KEPT: it is read when `use_adjusted_close` is flipped, and a fixture that
/// silently changes meaning under a knob is worse than a slightly larger one.
#[test]
#[ignore = "needs a warm real .long_history_cache.json; regenerates the committed fixture"]
fn regen_backtest_fixture() {
    let src = repo().join(".long_history_cache.json");
    let raw = std::fs::read_to_string(&src)
        .unwrap_or_else(|e| panic!("read {}: {e} — run `folioman screen` first to warm it", src.display()));
    let all: std::collections::HashMap<String, (String, serde_json::Value)> =
        serde_json::from_str(&raw).expect("parse real cache");

    fn slim(v: &mut serde_json::Value) {
        if let Some(r) = v.pointer_mut("/chart/result/0/indicators/quote/0").and_then(|q| q.as_object_mut()) {
            for k in ["open", "high", "low"] {
                r.remove(k);
            }
        }
        round(v);
    }
    fn round(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    if f.fract() != 0.0 {
                        if let Some(r) = serde_json::Number::from_f64((f * 1e4).round() / 1e4) {
                            *v = serde_json::Value::Number(r);
                        }
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(round),
            serde_json::Value::Object(o) => o.values_mut().for_each(round),
            _ => {}
        }
    }

    let mut out: std::collections::BTreeMap<&str, (String, serde_json::Value)> = Default::default();
    let mut missing = Vec::new();
    for t in FIXTURE_TICKERS {
        match all.get(*t) {
            Some((d, v)) => {
                let mut v = v.clone();
                slim(&mut v);
                out.insert(t, (d.clone(), v));
            }
            None => missing.push(*t),
        }
    }
    assert!(missing.is_empty(), "not in the real cache (warm it, or drop them from the recipe): {missing:?}");

    let dst = fixture_dir().join(".long_history_cache.json");
    std::fs::create_dir_all(dst.parent().unwrap()).expect("mkdir fixture");
    let body = serde_json::to_string(&out).expect("serialize fixture");
    std::fs::write(&dst, &body).expect("write fixture");
    eprintln!("wrote {} — {} tickers, {:.2} MB", dst.display(), out.len(), body.len() as f64 / 1e6);
}
