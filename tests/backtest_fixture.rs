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
//! IT DOUBLES AS THE MARKER CONTRACT. `backtest_edge_holds` finds its numbers by string-searching:
//! `tickers:`, `windows scored:`, `->  edge`, `tuning adds`, `top-3 `, `early rho`/`late rho`. Every
//! one of them is in this golden (verified against it, not assumed), so a report rename reds HERE —
//! offline, in seconds — before it can silently blind the nightly gate. That matters because the
//! nightly gate's own reaction to a missing marker is to SKIP, which is green.
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
//! - runtime 1.8s release / 24s debug. The 200-ticker recipe below is kept at that debug cost
//!   deliberately: this is the ONLY end-to-end net over real data, and trimming it to hit a stopwatch
//!   target would trade the branches it covers for seconds on a suite that already runs minutes.
//!
//! Trip-verified, because a pin nothing perturbs is a test that passes forever: nudging
//! `growth_accel_weight` 0.65 -> 0.66 in tests/ci-settings.yaml (a 1.5% move on ONE knob) reds this at
//! line 31 — `Spearman +0.19` -> `+0.18`. Reverted.
//!
//! KNOWN GAP, stated rather than implied: the run takes the NARROW path (an explicit `tickers:` list),
//! which leaves `etf_set`/`sector_of` empty (backtest.rs, `(#46)`). The chart-meta class stamping is
//! covered; the index-membership braces over it are not.
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

/// Hold horizon the golden is pinned at. 12 is >= 8, so the run takes the MONTHLY path — the same
/// branch the wide nightly gate uses (`monthly = long || years >= 8`), and the one with decades of bars.
const PIN_YEARS: &str = "12";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `config::data_path` anchors to the settings file's GRANDPARENT, so a config at
/// `tests/fixture/config/settings.yaml` makes `tests/fixture/` the whole data root — the frozen cache
/// is picked up by the ordinary cache path, with no test-only plumbing inside `fetch.rs`.
fn fixture_dir() -> PathBuf {
    repo().join("tests/fixture")
}

/// Everything the golden must not carry: wall-clock stamps, timings and machine paths. Nothing else is
/// stripped — a line dropped here is a line no longer pinned, so keep this list as short as it can be.
fn normalize(raw: &str) -> String {
    raw.lines()
        .filter(|l| {
            let t = l.trim_start();
            // the verdict's display date (chrono::Local::now) and the progress/timing chatter
            !(t.starts_with("backtest: ") && t.contains("tickers,"))
                && !t.contains("elapsed")
                && !t.contains("run 20")
                && !t.contains(env!("CARGO_MANIFEST_DIR"))
        })
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// THE PIN. Runs the real binary over the frozen cache and diffs the whole report against the golden.
#[test]
fn backtest_report_is_pinned_on_frozen_data() {
    let cache = fixture_dir().join(".long_history_cache.json");
    assert!(
        cache.is_file(),
        "frozen cache missing at {} — regenerate it with \
         `cargo test --release --test backtest_fixture -- --ignored regen` from a warm real cache",
        cache.display()
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_folioman"))
        .args(["backtest", PIN_YEARS])
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

    let golden_path = fixture_dir().join("backtest-12.golden");
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
            "the backtest report changed on FROZEN data — the market cannot have moved, so this is a \
             code or knob change.\n{detail}\n\nIf it was intended: re-validate with a live \
             `folioman backtest universe` (both OOS halves positive) exactly as the ci-settings \
             receipts require, then re-bless with \
             `FOLIOMAN_BLESS=1 cargo test --release --test backtest_fixture`."
        );
    }
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
