//! `backtest [YEARS] [TICKERS...]` — zero-EXTRA-fetch sanity check of the buy heuristic. One chart
//! fetch per ticker (the same single call `check` makes — no worse rate-limit pressure), then it all
//! happens offline. Metrics: rho = Spearman rank correlation (selection skill); edge = top-half minus
//! bottom-half realized return, in points; OOS = Out-Of-Sample (early-vs-late split). Other acronyms
//! (CAGR, P/E, …): see the Glossary in README.md.
//!
//! - **(#3) walk-forward**: score the name at MANY cutoffs (~every 6 months back through its history),
//!   each measured against the realized return over the SAME `YEARS`-long forward window. Pools ~10×
//!   the samples a single as-of date gives, killing lucky-single-date bias — from the very same fetch.
//!   (#790) The cutoffs sit on a calendar grid: one per ~6-month bucket, at the name's first bar in it.
//! - **(#1) peer-relative**: each cutoff's realized return is de-meaned within its ~6-month bucket, so the
//!   correlation measures SELECTION (did the name beat its same-period peers?), not the bull/bear regime
//!   every pooled cutoff would otherwise share. Without it the score races calendar luck, not skill.
//! - **(#2) out-of-sample split**: pooled rank-correlation on the EARLY half of cutoffs vs the LATE
//!   half. Early ≈ late = a stable signal; late collapsing/flipping = in-sample overfit or a dead regime.
//! - **head-to-head lanes**: BOTH rankings are reported against the same peer-relative returns — the
//!   on-sale lane (`buy_score`, buys pullbacks) and the growth lane (`growth_score`, buys near-high
//!   compounders still climbing). The higher rho is the lane actually selecting winners on this data.
//! - **(#1/#6) ablation**: per lane, switch each score weight OFF, recompute the pooled correlation,
//!   show the change. A term whose removal barely moves the correlation carries no ranking signal here
//!   — a prune candidate. P/E reads ~0 BY CONSTRUCTION (#6): `backtest_quote` can't reconstruct an
//!   as-of P/E, so that weight is inert here and CANNOT be validated by this command.
//! - **(#53) DIVIDENDS ARE NO LONGER IN THAT LIST.** This header claimed as-of dividends were
//!   unreconstructable too, and `dividend_weight`'s receipt cited that claim to ship the term ungraded.
//!   It was never true: `Chart.divs` carries (ex-date, amount) for the whole history, arrives in the
//!   same response as the closes, and was simply dropped at the fetch site. `backtest_quote` now takes
//!   it and derives the as-of trailing yield through `core::dividend_sums`, whose window anchors on the
//!   cutoff slice — so there is no look-ahead to guard against by hand. If you are about to write
//!   "the backtest cannot see X", check whether X is already in the payload first.
//! - **(#5) survivorship**: the universe is names that SURVIVED to today, so realized returns are
//!   biased UP. Flagged in the footer — treat the edge as optimistic, never a forecast. (#789) It is
//!   also SELECTED by today's size: the pool is today's ranked lists (CoinGecko top market cap,
//!   Frankfurt top turnover, the Lisbon, Euronext and SIX lists), so a name that is big or busy NOW
//!   is in it at every past cutoff. `pit` swaps only the S&P list; every other list stays today's.
//! - **(PIT) point-in-time universe**: `pit` scores every cutoff against the S&P 500 AS IT STOOD THAT
//!   DAY — the direct cure for the line above, not the `stress` proxy. With `universe` it also swaps
//!   the pool: today's ~503 survivors out, all 1206 names ever in the index since 1996 in, dead ones
//!   included. EXPECT THE EDGE TO FALL; that is the measurement working. Names the index held but
//!   Yahoo no longer serves are COUNTED and printed, because a point-in-time pool whose dead names
//!   silently fail to fetch is the survivors-only pool again wearing a flag.
//!
//! Defaults to the settings.yaml watchlist (small, cheap). Pass tickers to test others, or the keyword
//! `universe` to test the whole live screen universe (#2 — a far wider, less single-name-lucky sample).
//! Add `stress` to inject known crashed/delisted losers into whatever pool is tested (#6) — compare the
//! rho/edge against the same run without it to see how much of the edge is survivorship bias, or `pit`
//! to remove the bias at the source instead of estimating it.

use crate::config::BuyHeuristic;
use crate::core::Quote;
use crate::picks::{buy_score, fund_fill, growth_score};
use crate::{config, core, fetch, picks};
use chrono::Datelike;
use futures::stream::{self, StreamExt};
use rayon::prelude::*;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// How many years of filed statements the as-of fundamental factors look BACK over — deliberately a
/// constant, and deliberately not the run's forward hold horizon (see the call site). 5 is not a new
/// choice: it is what BOTH live call sites already pass (`report.rs`'s verdict mirror and
/// `fetch.rs`'s live enrich, each `fund_factors(&rows, today, 5)`), so the backtest now grades the
/// same window the screen serves. Raising it re-opens the coverage hole — SEC XBRL history only
/// reaches ~2007, so every extra year of lookback deletes a year of usable cutoffs from the front.
const FUND_LOOKBACK_YRS: i64 = 5;

/// The journaled method verdict — written by WIDE (`universe`) runs only, read by the screen's
/// method footer (track-pattern twin: one writer, one reader, same struct, so the two surfaces
/// can't disagree). A watchlist run must never overwrite this: its tiny survivor sample isn't
/// the method's proof.
pub(crate) const VERDICT_FILE: &str = ".backtest_verdict.json";

/// (#309) The POINT-IN-TIME twin: wide `pit` runs journal here, each row carrying its SIZED record, and
/// `screen` prints those under BUY NOW. Kept apart so a PIT number never restates the method footer's
/// claim (see [`may_write_verdict`]), and a survivor-universe number never grades the book bought.
/// (#788)/(#1061) `fund pit dead` runs only, see [`may_write_sized`].
pub(crate) const SIZED_FILE: &str = ".backtest_sized.json";
/// (#310) The roll point the census graded: sell the whole book after this many years, pay the tax, and
/// re-buy that day's screen. Fixed a priori, never swept — picking the best of several is the (#120) argmax.
/// A run at twice this horizon walks its history a second time, at every cutoff, to grade it ([`report_roll`]).
const ROLL_YEARS: i64 = 10;
const ROLL_MONTHS: u32 = ROLL_YEARS as u32 * 12;
/// (#311) The plan [`report_roll`] grades: this many equal monthly buys (8 years), every lot sold at `2 × ROLL_YEARS`.
const DCA_MONTHS: u32 = 96;

/// Fewest resolved tickers a wide run must carry before it may overwrite [`VERDICT_FILE`]. Same floor
/// `backtest_edge_holds` applies to its own sample: below it, the pool is a throttle artefact rather
/// than the method's proof. A healthy run resolves ~4900.
const MIN_VERDICT_TICKERS: usize = 500;

/// Every string `tests/network.rs::backtest_edge_holds` locates a graded number by. The gate has no
/// parser — it string-searches this report — so each of these is load-bearing, and (#910) FOURTEEN OF
/// THE NINETEEN fail SOFT when one goes missing:
///
/// - `tickers:`, `── GROWTH`, `windows scored:` — `return false`, i.e. SKIP, i.e. **GREEN**. These
///   three are deliberately soft: they are the throttle guard, and a live wide run that Yahoo
///   throttles must skip rather than red. The bug is not the softness, it is that a RENAME is
///   indistinguishable from a THROTTLE — both print "SKIPPED", both pass, and the gate then grades
///   nothing at all, forever, silently.
/// - `n=`, `peer-relative`, and the three `growth_*_->off` labels — the gate re-probe WARN just
///   stops firing.
/// - `top-10 `, `excess`, (#565) `passer lift`, `passer OOS early`, `passer OOS late`, (#315) `vs Série E
///   best case (top-10): beat` — panic only under `forced`.
///   `tuning adds`, `early rho`, `late rho` still print over the whole crowd; CI no longer asserts on them.
/// - `->  edge` and (#565) `->  passer edge` — the only two that are hard unconditionally.
///
/// So the honest place to enforce them is OFFLINE, against the golden, where there is no network to
/// blame for their absence: `tests/backtest_fixture.rs::gate_markers_are_all_in_the_golden`. Both
/// test crates read THIS slice, so the gate's search strings and the assertion that they still exist
/// cannot drift apart into two independent copies.
/// Named rather than indexed: `tests/network.rs` reads these by name, so a slip picks a
/// non-existent symbol (compile error) instead of the wrong string (a test that still passes while
/// grading the wrong line).
pub mod markers {
    pub const TICKERS: &str = "tickers:";
    pub const GROWTH_SECTION: &str = "── GROWTH";
    pub const WINDOWS_SCORED: &str = "windows scored:";
    pub const LANE_EDGE: &str = "->  edge";
    /// (#565) The edge over gate passers only, the crowd `->  edge` graded before `growth_gate_soft`. The CI
    /// ship rule asserts on this one; `->  edge` still prints and still must exist.
    pub const PASSER_EDGE: &str = "->  passer edge";
    /// (#565) [`TUNING_ADDS`] and the OOS halves, over the same gate passers. The CI ship rule asserts on these.
    pub const PASSER_LIFT: &str = "passer lift";
    pub const PASSER_EARLY: &str = "passer OOS early";
    pub const PASSER_LATE: &str = "passer OOS late";
    /// (#120) The held-book row SHIP RULE v2 grades — the [`super::VERDICT_TOP`] rung of the top-N
    /// ladder, not a fixed 3. Was `"top-3 "`; moving the basket without moving this would have left
    /// the gate asserting on a row the verdict no longer reports, which is the quietest possible
    /// failure. `verdict_row_matches_the_basket` pins the two together.
    ///
    /// The TRAILING SPACE is load-bearing and matched with `starts_with` after trimming, so the row
    /// for `top-10` cannot be matched by `top-100` — nor `top-1` by `top-10`. Do not tidy it away.
    pub const VERDICT_ROW: &str = "top-10 ";
    pub const EXCESS: &str = "excess";
    pub const TUNING_ADDS: &str = "tuning adds";
    pub const EARLY_RHO: &str = "early rho";
    pub const LATE_RHO: &str = "late rho";
    pub const COHORT_N: &str = "n=";
    pub const PEER_RELATIVE: &str = "peer-relative";
    /// (#315) The Série E best-case row: the share of start dates whose top-10 book beat 4.5%/yr. Starts with "vs",
    /// never `VERDICT_ROW`, so the book row stays the first `top-10 ` line the gate finds.
    pub const SERIE_E: &str = "vs Série E best case (top-10): beat";
    /// (#1116) The head of the book's decile table. It prints under `print_book_deciles`, ON in
    /// `tests/ci-settings.yaml` since (#1114), so the 12y golden holds it.
    pub const BOOK_DECILES: &str = "terminal-multiple distribution across windows";
    /// (#316) The 20y DCA rows of the book BUY NOW buys. The name pads to 7, so "SIZED   roll" never matches. Not in
    /// [`super::GATE_MARKERS`]: that list is pinned against the 12y golden, and these print at 20y only.
    pub const SIZED_DCA: &str = "SIZED   never-sell";
    pub const SERIE_E_DCA: &str = "SIZED   beat Série E best case:";
    /// (#421) The fund leg's two ratchets: the share of cutoffs the as-of SEC join reached, and the cohort of the
    /// shipped tilt factor. They print under `fund` only, so they are not in [`super::GATE_MARKERS`];
    /// `fund_markers_are_in_the_fund_golden` pins them. FUND_PEG is the `{:<14} n=` probe row, so the winners
    /// table's `peg_yield` row never matches it.
    pub const FUND_SECTION: &str = "── FUNDAMENTAL (as-of";
    pub const FUND_COVERED: &str = "cutoffs with as-of fundamentals:";
    pub const FUND_PEG: &str = "peg_yield      n=";
    /// (#423) SEC requests sent / refused this run, so the gate tells an SEC throttle (skip) from a thinned join (red).
    pub const FUND_SEC: &str = "SEC requests this run:";
    /// (#424) tickers whose filings were restated onto the chart's split basis. Zero on a live run = the split feed went dark.
    pub const FUND_SPLIT: &str = "split-restated tickers:";
    /// (#426) tickers whose EPS `strip_noncore` put on core earnings. Zero on a live run = the pretax tag went dark.
    pub const FUND_CORE: &str = "core-stripped tickers:";
    /// (#606) The share of the shipped blend's book slots its window's top-5% stocks fill. Not in
    /// [`super::GATE_MARKERS`]: that list is pinned against the 12y golden, and this prints under `winners` only.
    pub const WINNERS_HELD: &str = "WINNERS HELD (#606):";
    /// The three shipped hard gates the re-probe WARN sweeps, by their GATE SWEEP row labels.
    pub const ABLATED_GATES: &[&str] =
        &["growth_max_above_ma ->off", "growth_require_lifetime_uptrend ->off", "growth_maxdd_cap ->off"];
}

pub const GATE_MARKERS: &[&str] = &[
    markers::TICKERS,
    markers::GROWTH_SECTION,
    markers::WINDOWS_SCORED,
    markers::LANE_EDGE,
    markers::PASSER_EDGE,
    markers::PASSER_LIFT,
    markers::PASSER_EARLY,
    markers::PASSER_LATE,
    markers::VERDICT_ROW,
    markers::EXCESS,
    markers::TUNING_ADDS,
    markers::EARLY_RHO,
    markers::LATE_RHO,
    markers::COHORT_N,
    markers::PEER_RELATIVE,
    markers::ABLATED_GATES[0],
    markers::ABLATED_GATES[1],
    markers::ABLATED_GATES[2],
    markers::SERIE_E,
    markers::BOOK_DECILES,
];

/// The basket sizes the ABSOLUTE top-N table grades, and therefore the set [`VERDICT_TOP`] was
/// SELECTED from. Hoisted to a const so its length has one definition: the best-of-N caveat printed
/// beside the table quotes `TOP_LADDER.len()`, and a rung added here tightens that caveat automatically
/// instead of silently making the printed count a lie.
pub(crate) const TOP_LADDER: &[usize] = &[1, 2, 3, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50];

/// Basket size the journal grades on, and the one the ship rule reads. THE SAME 10 the entry-state
/// table and the CORR-CAP probe rank by, which is the whole point of (#120): this was 3, those were
/// 10, and the tool therefore COMPARED candidate knobs on one basket and SHIPPED its verdict on
/// another. Every receipt in tests/ci-settings.yaml was written across that gap.
///
/// 3 was not a policy, it was an ARGMAX. (#92) and (#106) already said so at length and are kept
/// below in compressed form because they are the reason this moved, not a caveat about it: the value
/// was the maximum of a 13-rung ladder ([`TOP_LADDER`]), taken on the same data the report then
/// quotes, with no best-of-N haircut, on the 0.5-3.5 effectively independent windows [`n_eff`]
/// counts — and the maximum was over a MEAN, which under positive skew is maximised by the most
/// concentrated book almost by construction, wrecking the median and the left tail as it goes.
/// A selection made that way cannot be defended by the numbers it was selected from.
///
/// So it is no longer selected at all. Pinning it to the comparison basket removes the free
/// parameter instead of re-fitting it — re-deriving 3 off the median and the 10th percentile
/// (`book_deciles` prints both) would be the same move a second time, on the same one trial.
/// The cost is real and stated: 10 is a wider, lower-mean book than 3 on this data. What it buys is
/// that the number the screen footer turns into a buy instruction is no longer the winner of a
/// contest run on the reader's behalf and never disclosed to them.
///
/// `legacy_top()` stays 10 as the serde default for journal files written before the field existed —
/// that is a historical fact about those files, not this policy, so the two are not merged.
pub(crate) const VERDICT_TOP: usize = 10;

/// The unconditional held-book verdict of one wide backtest run at ONE horizon: the "all entries"
/// row of the entry-state table (full gated pool, growth_score ranking, equal-weight top-[`VERDICT_TOP`],
/// held `years` forward, vs the index) plus the run date and a fingerprint of the tuning that earned it.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Verdict {
    pub(crate) date: String,
    pub(crate) years: i64,
    /// Basket size these numbers were earned on. Defaulted for files written before the journal
    /// carried it — those were top-10 by construction, so saying 10 is a fact, not a guess.
    #[serde(default = "legacy_top")]
    pub(crate) top: usize,
    pub(crate) windows: usize,
    pub(crate) book: f64,
    pub(crate) excess: f64,
    pub(crate) win: f64,
    pub(crate) worst: f64,
    pub(crate) oos_early: f64,
    pub(crate) oos_late: f64,
    pub(crate) tuning_fp: String,
    /// (#309) The same run's (#290) SIZED row, so `screen` can print the graded record of the book its
    /// BUY NOW block tells the reader to buy. Only [`SIZED_FILE`]'s point-in-time rows carry one; `None` on
    /// every [`VERDICT_FILE`] row and in files written before it existed.
    #[serde(default)]
    pub(crate) sized: Option<SizedVerdict>,
}

/// (#309) The SIZED row as journaled. `sizing_fp` fingerprints the `sizing:` block (and since (#1060)
/// `top_picks`, see [`sizing_fingerprint`]) beside `tuning_fp`, because the caps, the spill and the width
/// move this row without touching a single `buy_heuristic` knob.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SizedVerdict {
    pub(crate) windows: usize,
    pub(crate) book: f64,
    pub(crate) excess: f64,
    pub(crate) worst: f64,
    pub(crate) deployed: f64,
    pub(crate) sizing_fp: String,
    /// (#311) [`report_roll`]'s SIZED Δ: a [`DCA_MONTHS`] monthly-buy plan that sells each lot at [`ROLL_YEARS`] and
    /// re-buys the screen, minus the same plan never selling, money-weighted %/yr after tax. Only a `2 × ROLL_YEARS`
    /// row carries one.
    #[serde(default)]
    pub(crate) roll: Option<f64>,
}

fn legacy_top() -> usize {
    10
}

/// Every horizon a wide run has journaled, keyed by hold years (ascending). One file, one writer,
/// one reader — the track pattern the single-verdict version had, kept while the file gained rows.
/// Before this, a `backtest 8` run ERASED the 20y verdict and the footer silently switched horizon.
pub(crate) type Journal = std::collections::BTreeMap<i64, Verdict>;

/// The ONE fingerprint both surfaces use (backtest stamps it, screen compares it) — a tuning
/// knob changed since the run means the cited numbers were never earned by the current settings.
pub(crate) fn tuning_fingerprint(t: &impl serde::Serialize) -> String {
    serde_json::to_string(t).unwrap_or_default()
}

/// (#1060) The SIZED row's fingerprint: the `sizing:` block AND `top_picks`, because [`sized_buckets`] cuts
/// each bucket at `top_picks` before the caps run, so a moved width changes the row with every `sizing:`
/// knob untouched. A row journaled before this fingerprinted the block alone, so it warns until rerun.
pub(crate) fn sizing_fingerprint(sz: &config::Sizing, top: usize) -> String {
    tuning_fingerprint(&(sz, top))
}

/// Malformed/corrupt JSON is an EMPTY journal — a broken file must SILENCE the screen line, never
/// fabricate a verdict. (Kept pure and separate from the fs read so the failure mode is tested.)
/// A pre-journal file holds one bare [`Verdict`]; adopt it under its own horizon rather than going
/// dark until the next wide run.
pub(crate) fn parse_journal(raw: &str) -> Journal {
    if let Ok(j) = serde_json::from_str::<Journal>(raw) {
        return j;
    }
    serde_json::from_str::<Verdict>(raw)
        .map(|v| Journal::from([(v.years, v)]))
        .unwrap_or_default()
}

/// The LONGEST journaled horizon. "Buy now, hold 8+ years" grades hardest at the long end, and the
/// long end is the one a short run must not be able to quietly replace.
///
/// Pure half, split out for the same reason [`parse_journal`] is: `data_path` resolves through
/// `FOLIOMAN_CONFIG`, which is process-global, so any test that drove the real fs would race every
/// other test in this binary. The fs shell below is then two lines with no decisions in it.
fn latest_verdict(raw: &str) -> Option<Verdict> {
    parse_journal(raw).into_values().next_back()
}

/// A pure fs shell, like [`write_verdict`] below: resolve a path, hand the bytes to a tested pure
/// function. (#340) Pinned by `read_verdict_reads_the_journal_file`: under `cargo test --lib`
/// `config::data_path` re-roots into a per-process scratch dir, so the race that once kept it
/// unpinned is gone. `write_verdict` stays skipped: past the fs call its arms only pick a message.
pub(crate) fn read_verdict() -> Option<Verdict> {
    latest_verdict(&std::fs::read_to_string(config::data_path(VERDICT_FILE)).ok()?)
}

/// Merge, never replace: a `backtest 8` run updates the 8y row and leaves 12y/20y standing. Pure
/// half of [`write_verdict`] — `None` means "no file yet", which must start a fresh journal rather
/// than being confused with a corrupt one (both end up empty here, but for stated reasons).
fn merge_verdict(existing: Option<&str>, v: Verdict) -> Journal {
    let mut j = existing.map(parse_journal).unwrap_or_default();
    j.insert(v.years, v);
    j
}

/// (#309) `file` is [`VERDICT_FILE`] or [`SIZED_FILE`], and the message names the screen line each feeds.
#[mutants::skip] // the fs shell `read_verdict`'s doc explains; taking `file` put its `-> ()` mutant in diff scope
fn write_verdict(file: &str, v: Verdict) {
    let path = config::data_path(file);
    let years = v.years;
    let j = merge_verdict(std::fs::read_to_string(&path).ok().as_deref(), v);
    let ok = serde_json::to_string(&j).ok().and_then(|s| std::fs::write(&path, s).ok());
    let (what, reader) = if file == SIZED_FILE {
        ("point-in-time record (SIZED row included)", "the BUY NOW record line cites every horizon")
    } else {
        ("method verdict", "the screen footer cites the longest")
    };
    match ok {
        Some(()) => eprintln!("backtest: {years}y {what} journaled to {file} ({} horizons on file) — {reader}", j.len()),
        None => eprintln!("WARNING: could not write {file} — the screen line it feeds stays absent/stale"),
    }
}

/// One-line rendering of [`Verdict`] for the screen footer. `drift` = the current tuning no
/// longer matches the fingerprint that earned these numbers — say so instead of citing them
/// as if they still applied.
///
/// (#120) NO [`best_of_tag`] HERE, DELIBERATELY. This footer used to carry " [best of 13, unhaircut]"
/// beside the basket, because the basket WAS the argmax of [`TOP_LADDER`]. It no longer is —
/// [`VERDICT_TOP`] is fixed a priori — so printing a best-of-N caveat next to it would caveat a
/// selection that was not made. The tag stays on the ladder TABLE header, which really is 13 rungs
/// wide and which a reader really can argmax by eye.
pub(crate) fn verdict_line(v: &Verdict, drift: bool, show_n_eff: bool) -> String {
    // (#965) the row's own horizon: a bare `backtest universe` is 5y and refreshes only the 5y row,
    // so a footer citing 20y would keep its warning after the user did exactly what it said
    let rerun = format!("`folioman backtest {} universe`", v.years);
    let tail = if drift {
        format!(" — ⚠ settings changed since, rerun {rerun}")
    } else {
        format!(" (rerun: {rerun})")
    };
    format!(
        "Method backtest (run {}, wide universe, top-{} held {}y, {} windows{}): book {:+.1}%/yr, \
         {:+.1}pp/yr vs index, win {:.0}%, worst {:+.1}, OOS {:+.1}/{:+.1}{tail}",
        v.date,
        v.top,
        v.years,
        v.windows,
        n_eff_tag(show_n_eff, v.windows, v.years),
        v.book,
        v.excess,
        v.win,
        v.worst,
        v.oos_early,
        v.oos_late
    )
}

/// (#309) The graded record of the book `screen`'s BUY NOW block prints: every journaled horizon's SIZED
/// row, longest first, each with its own run date (a `backtest 8` run updates only its row). A row earned
/// on other `buy_heuristic`, `sizing:` or `top_picks` settings warns instead of passing as current. `None` when no row
/// carries a SIZED record, which is every journal written before (#309).
pub(crate) fn sized_record_line(j: &Journal, tuning_fp: &str, sizing_fp: &str) -> Option<String> {
    let rows: Vec<(&Verdict, &SizedVerdict)> = j.values().rev().filter_map(|v| v.sized.as_ref().map(|s| (v, s))).collect();
    if rows.is_empty() {
        return None;
    }
    let drift = rows.iter().any(|(v, s)| v.tuning_fp != tuning_fp || s.sizing_fp != sizing_fp);
    let cells: Vec<String> = rows
        .iter()
        .map(|(v, s)| {
            let roll = s.roll.map_or(String::new(), |r| {
                format!("; buying monthly for {}y, selling each lot at {ROLL_YEARS}y to re-buy the screen: {r:+.1}/yr vs never-sell, after tax", DCA_MONTHS / 12)
            });
            format!(
                "{}y {:+.1}%/yr ({:+.1} vs index, worst {:+.1}, {:.0}% in names, {} windows, run {}{roll})",
                v.years, s.book, s.excess, s.worst, s.deployed, s.windows, v.date
            )
        })
        .collect();
    let tail = if drift { " — ⚠ settings changed since, rerun `folioman backtest N universe fund pit dead`" } else { "" };
    Some(format!("  Graded record of this book (point-in-time backtest, past windows, not a forecast): {}{tail}", cells.join(" · ")))
}

/// One cutoff observation: the date it was scored on and the realized forward return over the holdout.
/// The Quote is kept so EACH lane (on-sale + growth) can score it under its own gates/knobs — and
/// ablation can re-score under a mutated knob set — with ZERO re-fetch / re-math. A sample is recorded
/// for every cutoff that has a full forward window, even if it passes neither lane's gates, so the
/// peer-mean (#1) is taken over the whole investable set of that period, not just the gated subset.
#[derive(Clone)]
struct Sample {
    date: chrono::NaiveDate,
    realized: f64, // raw forward return %
    relative: f64, // (#1) realized minus its cutoff-bucket peer mean -> SELECTION, not regime beta
    // `Arc`, because Samples are BULK-CLONED and a Quote is 87 fields of mostly-heap Strings. The fund
    // lane alone does `samples.to_vec()` once per factor and once per weight rung, and
    // `hold_period_sweep` builds six Samples per cutoff that differ only in `realized`. Sharing makes
    // those a refcount bump. The three sites that write `quote.fund_factor` use `Arc::make_mut`, so
    // they still get a private copy — same cost as the deep clone they already paid, just moved.
    quote: Arc<Quote>,
    fund: Option<core::FundFactors>, // (G) as-of fundamentals at this cutoff (None unless `fund` + FMP key + cached)
    trail: Vec<f64>, // (round 112) up to 36 trailing monthly returns % at the cutoff — CORR-CAP probe input; empty = can't judge
}

/// A ticker's history between the fetch and the walk, in whichever form it arrived.
///
/// The point of the split is WHERE the parse happens. Both wide fan-outs below are `buffer_unordered`
/// streams, which one task polls on one thread; the walks that follow them are 8-way under rayon. A
/// `Raw` payload has not been parsed yet, so its `parse` lands in the walk and spreads over the cores.
/// `Parsed` is the cases that cannot defer — the daily arm (network-bound, no disk cache, nothing to
/// gain) and a `fund` run (the FX branch needs the currency before the walk starts).
// One Hist per ticker, ~5k at most; boxing `Parsed` saves ~1 MB and costs an allocation per fetch.
#[allow(clippy::large_enum_variant)]
enum Hist {
    Raw(Cow<'static, RawValue>),
    Parsed(fetch::Chart),
}

impl Hist {
    /// The deferred parse. `None` = unparseable, or parsed to no bars — which is exactly the pair of
    /// conditions `fetch_history_long` folds into its own `None`, kept here so both paths drop the
    /// same tickers. (#310) Borrowed, so one fetch can be walked at two horizons.
    fn parse(&self, ticker: &str) -> Option<fetch::Chart> {
        match self {
            Hist::Raw(r) => fetch::parse_chart_raw(r, ticker).filter(|c| !c.closes.is_empty()),
            Hist::Parsed(c) => Some(c.clone()),
        }
    }
}

/// (#1) Cross-sectional peer-group key: the ~6-month bucket a cutoff falls in (2 buckets/year). Names
/// scored in the same half-year are compared against EACH OTHER, so the score is judged on selection
/// skill, not the bull/bear regime every pooled cutoff otherwise shares.
fn bucket(d: chrono::NaiveDate) -> i32 {
    d.year() * 2 + d.month0() as i32 / 6
}

/// (#790) The walk's cutoffs: every bar from `from` on that opens its ~6-month [`bucket`]. Both walks
/// iterate this, so every name's cutoff in a bucket sits on its first bar there, and no step count can
/// stall or skip a bucket.
///
/// Before this the walk stepped a fixed bar count from each name's own first bar, which made the
/// cutoff dates follow the listing month: a name listed in March was scored in March and September,
/// then ranked by `demean` against peers scored in January and July of the same buckets, with months
/// of extra or missing price path between them. The bar count also weighted the sample by granularity
/// (6 bars is six months monthly, six weeks weekly, six sessions daily), which (#117) could only fix
/// by rescaling a knob that ships off. A calendar grid fixes both at once.
fn cutoffs(dates: &[chrono::NaiveDate], from: usize) -> impl Iterator<Item = usize> + '_ {
    (from..dates.len()).filter(move |&i| i == 0 || bucket(dates[i - 1]) != bucket(dates[i]))
}

/// (#91) How many INDEPENDENT trials a printed window count is actually worth.
///
/// A "window" here is a ~6-month entry bucket and the hold is `years`, so two consecutive windows share
/// (years−0.5)/years of one price path: they are near-copies, not repeats of an experiment. The count
/// that means something is the span of entry dates divided by the hold, which for dense buckets is
/// `windows / (2 · years)`. The 20y report's headline row reads "win 67% ... (windows 21)" — that is 21
/// draws worth HALF a trial. The 12y run's 33 windows are worth ~1.4. Neither number is wrong; the
/// report simply never said which of the two it was quoting, and 67% of 21 reads like evidence.
///
/// ONE definition, used by every site that prints a window count and by nothing that scores.
fn n_eff(windows: usize, years: i64) -> f64 {
    windows as f64 / (2.0 * years.max(1) as f64)
}

/// The printed form of [`n_eff`] — EMPTY when the knob is off, so every call site is one extra
/// interpolation and the shipped output stays byte-identical to the goldens.
fn n_eff_tag(on: bool, windows: usize, years: i64) -> String {
    if on { format!("  n_eff {:.1}", n_eff(windows, years)) } else { String::new() }
}

/// (#92) The printed best-of-N caveat — EMPTY when the knob is off, same contract as [`n_eff_tag`].
///
/// A maximum over 13 candidates is biased upward by selection whether or not anyone says so; the only
/// question is whether the reader is told. `sweep_fund_factor` already answers its own version of this
/// with a Bonferroni-tightened band over every `FUND_FACTORS` entry.
///
/// (#120) WHAT THIS TAG NOW CAVEATS, AND WHAT IT NO LONGER DOES. It was written when [`VERDICT_TOP`]
/// was the argmax over [`TOP_LADDER`], and it flagged the shipped basket as an unhaircut 13-way
/// maximum. That debt is PAID: the basket is fixed a priori and is not selected off this table at all,
/// so the tag came off the screen footer. It stays on the ladder table header, where the 13 rungs are
/// still printed side by side and the eye still argmaxes them — the caveat's subject is now the
/// READER's selection, not the repo's.
fn best_of_tag(on: bool, candidates: usize) -> String {
    if on { format!(" [best of {candidates}, unhaircut]") } else { String::new() }
}

/// (#96) The lane's `edge` and the verdict's `top-N excess` are both printed as "pts" and are NOT the
/// same unit. `edge` is a spread of CUMULATIVE returns over the run's whole `years` hold — `realized`
/// is `close[i+hold]/close[i] − 1`, one number per cutoff, never divided by anything. `excess` is per
/// YEAR. At the 20y run that is a ~20× gap between two numbers a reader compares by eye because the
/// report gives them the same name and the same suffix.
///
/// The restatement is per HALF, not on the spread: annualising a difference of cumulative returns is
/// not the difference of the annualised ones, and the second is the one that means "points per year".
/// A half at or below −100% cumulative has no real root, so the tag goes silent rather than print NaN.
fn annualized_edge_tag(on: bool, top: f64, bot: f64, years: i64) -> String {
    let ann = |x: f64| ((1.0 + x / 100.0).powf(1.0 / years.max(1) as f64) - 1.0) * 100.0;
    if !on || top <= -100.0 || bot <= -100.0 {
        return String::new();
    }
    format!("   [= {:+.2} pts/yr over the {years}y hold]", ann(top) - ann(bot))
}

/// (#96) Round trips the net-of-cost line should charge for.
///
/// [`turnover_frac`] measures churn between CONSECUTIVE ~6mo buckets — a per-rebalance number. The
/// charge was applied ONCE against an edge spanning the full `years` hold, so a book re-formed every
/// six months for twenty years paid one round trip. At 2 rebalances a year that understates the cost
/// by `2·years`, and "NET ≤ 0: too churny to trade" can never fire on a long run no matter how much
/// the book churns: 20bps against a twenty-year cumulative spread is noise by construction.
///
/// OFF returns the single charge every golden and every fitted receipt was measured under.
fn rebalances(years: i64, per_rebalance: bool) -> i64 {
    if per_rebalance { (2 * years).max(1) } else { 1 }
}

/// The charge itself, in points. ONE definition — both the lane report and the hold-period sweep
/// route through it, and the printed formula quotes [`rebalances`] rather than re-deriving it.
fn cost_pts(turn: f64, years: i64, per_rebalance: bool) -> f64 {
    turn * rebalances(years, per_rebalance) as f64 * ROUND_TRIP_BPS / 100.0
}

/// The printed multiplier — EMPTY when the knob is off, so the shipped line stays byte-identical.
fn rebalance_tag(years: i64, per_rebalance: bool) -> String {
    if per_rebalance {
        format!(" × {} rebalances", rebalances(years, per_rebalance))
    } else {
        String::new()
    }
}

/// (#90) PURGE + EMBARGO for a chronological split. Returns where the EARLIER side should stop, given
/// where the split falls: every row within `months` of the boundary date is dropped off the end of it.
///
/// Every split in this file — `tune`'s 70/30 and the early/late OOS halves — cuts by ROW INDEX with no
/// gap, and that is only sound if a row's label is settled by the boundary. It is not: a cutoff's label
/// is its return over the NEXT `years`, so the last train row and the first test row share (years−0.5)/
/// years of the same realised path. At a 12y hold that is 11 of 12 years. The "held-out" half is then
/// largely the same outcome the earlier half was fitted on, which is exactly the failure an OOS number
/// exists to detect. `tune`'s per-split `demean` does not help here — it removes the PEER MEAN, and this
/// leak is in the label itself.
///
/// ONE definition, called at every split (`sweep_fund_factor`, `tune_growth`, `weight_curve`,
/// `report_lane`, `report_fund_lane`, `emit_probe`; (#947) nine sites in those six), because a purge
/// applied at some boundaries and not others is a purge nobody can reason about. Only the
/// EARLIER side loses rows: the later side is the evidence being read, and trimming it would shrink the
/// very sample whose independence this is buying.
///
/// `months <= 0` returns `cut` untouched — the default, byte-identical, no allocation, no date maths.
/// Rows must be date-ordered, which every caller's input already is (`samples` is built in cutoff order
/// and `scored` preserves it); `partition_point` is a binary search over that order.
fn purged_cut<T>(rows: &[T], cut: usize, months: i64, date_of: fn(&T) -> chrono::NaiveDate) -> usize {
    if months <= 0 || cut == 0 || cut >= rows.len() {
        return cut;
    }
    // 30-day months, matching the horizon arithmetic elsewhere in this file. The span is a judgement
    // call measured in years; calendar-exact month subtraction would be false precision.
    let keep_before = date_of(&rows[cut]) - chrono::Duration::days(months * 30);
    rows[..cut].partition_point(|r| date_of(r) < keep_before)
}

/// (#1227) Both sides of a split at once: the earlier side ends where [`purged_cut`] ends it, and the
/// later side starts `months` after the boundary (the embargo), so no kept row on either side sits
/// within one span of the other. Returns (end of the earlier side, start of the later side). Same
/// guards as `purged_cut`: off or a degenerate cut is `(cut, cut)`.
fn purged_halves<T>(rows: &[T], cut: usize, months: i64, date_of: fn(&T) -> chrono::NaiveDate) -> (usize, usize) {
    if months <= 0 || cut == 0 || cut >= rows.len() {
        return (cut, cut);
    }
    let keep_from = date_of(&rows[cut]) + chrono::Duration::days(months * 30);
    (purged_cut(rows, cut, months, date_of), cut + rows[cut..].partition_point(|r| date_of(r) < keep_from))
}

/// (#1227) The early/late split at the honest span, printed rather than applied: one hold purged off
/// the earlier side and one hold embargoed off the later one. A side left under [`MIN_SAMPLES`] rows
/// means no held-out number in this report is free of shared forward years at this horizon.
fn purged_split_line(samples: &[Sample], years: i64) -> String {
    let (n, mid, span) = (samples.len(), samples.len() / 2, 12 * years);
    let (early, late) = purged_halves(samples, mid, span, |s| s.date);
    let verdict = if early.min(n - late) < MIN_SAMPLES { "UNRESOLVED: a side is under the floor, so every OOS half above shares forward years" } else { "both sides kept" };
    format!("\n  purged split, {span} months a side: early {early} of {mid} rows kept, late {} of {} — {verdict}", n - late, n - mid)
}

/// (#46) Fill the three fields `Quote::stub` leaves blank that decide a name's ASSET CLASS. The backtest
/// reconstructs quotes from price history alone, and the stub sets `name` to the TICKER with an empty
/// `instrument_type` — but `quote_is_etf` is exactly `instrument_type == "ETF" || is_etf(name)`, and
/// `is_etf` substring-matches fund-name markers ("etf"/"ucits"/…) that no ticker carries. So before this,
/// EVERY fund scored as a single stock: `demean`'s class split never separated funds from companies, and
/// the ETF-scoped gates that read these fields — `is_commodity_etf` (the physical-gold/ETC bar),
/// `is_commodity`'s fund-name branch, `sharpe_cap_etf` — silently no-op'd. (`growth_min_aum_etf` and
/// `is_noneur_etf` stay inert regardless: `backtest_quote` never fills `aum_eur`/`quote_currency`.)
///
/// Yahoo's `meta.instrumentType` leads because a fund's shortName often carries no marker at all
/// ("ISHARES III PLC ISHRS CORE MSCI"); the universe's own `etf_set` overrides it so a venue that omits
/// the tag cannot drop a fund back into the stock class. Both come from data already fetched — no extra
/// request. Crypto needs nothing here: `is_currency_quoted` reads the `-EUR`/`-USD` ticker suffix, which
/// the stub does preserve, so coins were the one class that always classed correctly.
fn stamp_asset_class(
    quote: &mut Quote,
    name: &str,
    instrument_type: &str,
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
) {
    quote.name = name.to_string();
    quote.instrument_type =
        if etf_set.contains(&quote.ticker) { "ETF".to_string() } else { instrument_type.to_string() };
    // (#789) name, type and the ETF set are TODAY's too, like the sector below: a fund renamed or
    // re-tagged since is classed at every past cutoff by what it is called now.
    // (#569) last writer of name and type, so stamp here; cleared first so the derive reads them live
    quote.name_class = None;
    quote.name_class = Some(picks::name_class(quote));
    // (#95) TODAY's GICS label, and there is no as-of one to substitute — no feed this tool reads
    // carries GICS history. It is not cosmetic: `picks::is_commodity` reads `sector` and drives
    // `commodity_damp`, so a name reclassified since is damped on a fact from after the decision.
    // Dropping it makes that damp inert in the walk, which is at least computable in 1995.
    quote.sector = if crate::config::backtest_drop_lookahead_sector() {
        None
    } else {
        sector_of.get(&quote.ticker).cloned()
    };
}

/// Per-asset-class sample census, indexed by `picks::asset_class`: `[crypto, ETF, stock]`, each
/// `(cutoffs, of which growth-scored)`. Read the RATIO, never the raw count: forward-window survival
/// scales with listing age, so ETF density is horizon-dependent (0.3% of cutoffs at 20y, 22% at 12y,
/// 42% at 8y — 2026-08-02) and is NOT the ~87% the universe's ticker list suggests.
///
/// THE SECOND RATIO IS THE IMPORTANT ONE, and it is why this census exists (measured 2026-08-02). The
/// PASS rate — growth-scored over cutoffs — is not remotely equal across classes at the shipped gates:
///
///   20y   ETF 0/14      12y   ETF 4/2618   (0.15%)   8y   ETF 17/8628   (0.20%)
///         stock 380/4619       stock 572/9134 (6.3%)       stock 700/11907 (5.9%)
///
/// So EVERY ETF-facing number this backtest prints rests on 0-17 rows, and the 20y lane cannot speak
/// about funds AT ALL (14 ETF cutoffs exist in total). Do not read an ETF conclusion off this harness
/// without checking this line first.
///
/// NOT an age/history artifact — that was the first hypothesis and it is wrong. At 8y there are 8628
/// ETF cutoffs, every one past the warmup with a full forward window, and the pass rate is still 0.2%.
/// The cause is `growth_min_cagr` (19.0), whose floor sits in the RIGHT TAIL of the stock return
/// distribution while a diversified fund IS that distribution's mean — structurally, a fund cannot be
/// in its own tail. Confirmed by moving the knob alone: at 15 the ETF counts go 0/14, 12/2618 (0.46%),
/// 128/8628 (1.48%). Live, the broad sleeves run +11..+15%/yr and the only ETFs over the floor are
/// narrow tech/semis. This is a property of what a growth floor MEANS, not a bug to fix.
fn class_census(samples: &[Sample], tuning: &BuyHeuristic) -> [(usize, usize); 3] {
    let mut out = [(0usize, 0usize); 3];
    for s in samples {
        let c = &mut out[picks::asset_class(&s.quote) as usize];
        c.0 += 1;
        c.1 += usize::from(picks::growth_score(&s.quote, tuning).is_some());
    }
    out
}

/// (#1) De-mean each cutoff's realized return WITHIN its ~6-month bucket AND asset class -> `relative`
/// (the selection signal). Class-split because crypto's ~1e9-scale peer-relative returns otherwise share
/// a pool with equities and swamp the de-meaned mean, pinning every growth-knob variant at the same edge
/// (band straddles 0 = the GROWTH lane can't discriminate). Per-(bucket, class) groups compare like with
/// like. Pure + testable; the runtime sum-to-~0 invariant check stays in `run`.
fn demean(samples: &mut [Sample]) {
    // (#93) FX. `realized` is a ratio of two closes in the LISTING currency and nothing on the return
    // path converts it, so a pooled group mixes a EUR UCITS line, a GBp LSE line and a USD S&P name and
    // then subtracts the average of three different currencies from each. Splitting by market makes the
    // window's FX move COMMON to the group, so the subtraction cancels it — no rate, no fetch, no series:
    // `Quote::stub` already fills `market` from the ticker suffix. It over-splits (Germany, France and
    // the Netherlands all trade EUR and become three groups), which is why a thin slice falls back below.
    // OFF is the pooled key every number in this repo was measured against.
    //
    // The knob is a process-once accessor (18 call sites, most with no `tuning` in scope), which a unit
    // test cannot flip — so the whole body is `demean_with`, and the accessor only chooses its argument.
    demean_with(samples, crate::config::demean_by_market());
}

/// (#1148) The peer mean includes the row itself. Leaving it out would give `realized - (sum - realized)
/// / (n - 1)`, which is exactly `n / (n - 1)` times the value below: the same sign and the same order
/// inside a group, scaled by under 1% on any group of 100 or more and by 8/7 on a smallest
/// `MIN_PEER_GROUP` market slice. Since (#790) every cutoff in a bucket sits on its first bar, so the
/// group is already one cutoff-day cross-section. Kept as is: the rescale moves every golden and no rank.
fn demean_with(samples: &mut [Sample], by_market: bool) {
    let key = |s: &Sample| (bucket(s.date), picks::asset_class(&s.quote));
    let mut sums: HashMap<(i32, u8), (f64, usize)> = HashMap::new();
    let mut fine: HashMap<((i32, u8), String), (f64, usize)> = HashMap::new();
    for s in samples.iter() {
        let e = sums.entry(key(s)).or_insert((0.0, 0));
        e.0 += s.realized;
        e.1 += 1;
        if by_market {
            let e = fine.entry((key(s), s.quote.market.clone())).or_insert((0.0, 0));
            e.0 += s.realized;
            e.1 += 1;
        }
    }
    // A market slice too thin to BE a peer group de-means a name against one or two neighbours, or
    // against itself (relative 0, which reads as "exactly average" and is not a measurement). (#1231)
    // So the thin slices of a (bucket, class) pool into one leftover group, and a leftover still under
    // MIN_PEER_GROUP joins the bucket's largest slice. Every row lands in exactly ONE group, so every
    // group nets to 0 and the sum-to-0 invariant in `run` holds with the knob on. The first cut sent
    // thin rows to the POOLED mean, which nets with nobody: its first probe died on that invariant.
    let mut rest: HashMap<(i32, u8), (f64, usize)> = HashMap::new();
    for s in samples.iter() {
        if fine.get(&(key(s), s.quote.market.clone())).is_some_and(|(_, n)| *n < MIN_PEER_GROUP) {
            let e = rest.entry(key(s)).or_insert((0.0, 0));
            e.0 += s.realized;
            e.1 += 1;
        }
    }
    // largest slice per (bucket, class), ties to the higher market name so the pick never rides the hash order
    let mut top: HashMap<(i32, u8), (usize, &String)> = HashMap::new();
    for ((k, m), (_, n)) in &fine {
        if *n >= MIN_PEER_GROUP {
            let e = top.entry(*k).or_insert((*n, m));
            *e = (*e).max((*n, m));
        }
    }
    let group = |k: (i32, u8), m: &String| -> (f64, usize) {
        let Some(&(_, t)) = top.get(&k) else { return sums[&k] };
        let own = fine[&(k, m.clone())];
        let (rs, rn) = rest.get(&k).copied().unwrap_or((0.0, 0));
        match (own.1 < MIN_PEER_GROUP, rn < MIN_PEER_GROUP) {
            (true, true) => {
                let t = fine[&(k, t.clone())];
                (t.0 + rs, t.1 + rn)
            }
            (true, false) => (rs, rn),
            (false, true) if t == m => (own.0 + rs, own.1 + rn),
            (false, _) => own,
        }
    };
    for s in samples.iter_mut() {
        let (sum, n) = group(key(s), &s.quote.market);
        s.relative = s.realized - sum / n as f64;
    }
}

/// (#93) Smallest market slice `demean` will treat as its own peer group. Below this the de-mean is
/// against too few neighbours to mean anything — at 1 it is a name against itself, which yields a
/// `relative` of exactly 0 and looks like a measured "average" result. Judgement value, not fitted:
/// it matches the `< 4`-family guards elsewhere in this file in spirit, set higher because a peer MEAN
/// over 4 names is still mostly one name.
const MIN_PEER_GROUP: usize = 8;

/// Need ~3y of history BEFORE a cutoff to form/score the long trend fairly.
const MIN_HISTORY: usize = 750;
/// (Item 9) Conservative round-trip trading cost (bps) charged against the gross edge per unit of
/// turnover. shortcut: a const, not a config knob — the backtest is a dev `cargo run` (already
/// recompiles); promote to config only if a non-dev needs to tune cost without a build.
const ROUND_TRIP_BPS: f64 = 20.0;

/// (#6 survivorship stress) Once-large names that since CRATERED or went bankrupt — the kind the live
/// `universe` (today's index members) silently drops. The `stress` keyword injects these into the pool so
/// the edge is graded against a sample that includes the losers, not just the survivors. Mixed on purpose:
/// crashed-and-alive mega/large-caps with deep Yahoo history (score at many cutoffs, then bleed forward)
/// plus a few bankrupt/failed tickers whose truncated series ends near ZERO (the strongest correction —
/// each is a cutoff with a ~−100% forward window). (#789) Only a few cutoffs, though: these come from
/// Yahoo, and the walk pays a terminal return only to names on the `dead` archive, so a STRESS name
/// scores a cutoff only when its full `years` window ends by its last bar. The crash shows up only in
/// the windows that end after the collapse. Any that Yahoo no longer serves return None and are
/// harmlessly skipped, so the list can over-include. shortcut: a hand-picked loser set, NOT point-in-time
/// index reconstruction (a data-vendor problem) — enough to tell "edge is real" from "edge is survivorship".
const STRESS_TICKERS: &[&str] = &[
    // crashed-and-alive, long history, once-large
    "GE", "INTC", "WBA", "T", "KHC", "MMM", "BA", "PARA", "VFC", "GPS", "M", "NWL", "HBI", "FL",
    "C", "AIG", "NOK", "BIIB", "CCL", "NCLH", "F", "WBD",
    // bankrupt / failed -> series ends near 0 (the gold correction; skipped if Yahoo drops them)
    "BBBYQ", "FRCB", "SIVBQ", "SBNY", "WEWKQ",
    // (#566) collapsed coins, so the BOOK crypto lines grade against the losers too
    "FTT-USD", "LUNC-USD", "LUNA1-USD", "CEL-USD",
];

/// Everything the command line says, and nothing it doesn't. Split out of [`run`] purely so it can be
/// tested: the parse used to be an inline `for` loop inside a fn that opens sockets, so NOTHING in the
/// repo exercised it. The mutation audit made that concrete — the arms recognising `universe`, `long`,
/// `fund` and `insider` could each be turned off and every golden stayed green, because the frozen-data
/// pins only ever pass `12`, `20`, `8`, `tune`, `halflife` and `stress`. Silently losing `universe` is
/// the expensive one: the CI gate's whole job is the WIDE run, and `wide` also gates the verdict
/// journal the screen footer cites.
#[derive(Debug, Default, PartialEq)]
struct Args {
    years: i64,
    wide: bool,
    long: bool,
    fund: bool,
    tune: bool,
    insider: bool,
    holders: bool,
    pac: bool,
    failures: bool,
    rivals: bool,
    winners: bool,
    halflife: bool,
    stress: bool,
    pit: bool,
    dead: bool,
    dead_flat: bool,
    intraday: bool,
    holdwindows: bool,
    traderules: bool,
    leverage: bool,
    lifecycle: bool,
    bestpick: bool,
    rolls: bool,
    options: bool,
    rescore: bool,
    level: bool,
    draws: usize,
    search: bool,
    configs: usize,
    nulls: usize,
    time: bool,
    tickers: Vec<String>,
}

/// First purely-numeric arg in 1..=100 = holdout years; the keyword `universe` = test the live screen universe
/// (#2: a much wider sample than the ~50-name watchlist -> less single-name luck); `pit` = score each
/// cutoff against the index as it stood that day; everything else = explicit tickers to test. (#1118) `time`
/// = wall seconds per phase on stderr.
fn parse_args(args: &[String]) -> Args {
    let mut a_ = Args { years: 5, draws: PLACEBO_DRAWS, configs: SEARCH_CONFIGS, nulls: SEARCH_NULLS, ..Default::default() };
    for a in args {
        match a.parse::<i64>() {
            // (#948) past 100 is a typo (a date, say), not a horizon: it overflowed `forward_return`'s
            // date maths into a panic after the whole fetch. It falls through to the ticker list.
            Ok(y) if a_.tickers.is_empty() && (1..=100).contains(&y) => a_.years = y,
            _ if a.eq_ignore_ascii_case("universe") => a_.wide = true,
            _ if a.eq_ignore_ascii_case("long") => a_.long = true,
            _ if a.eq_ignore_ascii_case("fund") => a_.fund = true,
            _ if a.eq_ignore_ascii_case("tune") => a_.tune = true,
            _ if a.eq_ignore_ascii_case("insider") => a_.insider = true, // (Item 4) also pull SEC Form-4 net buys
            _ if a.eq_ignore_ascii_case("holders") => a_.holders = true, // (#518) grade the 13F-holder boosts
            _ if a.eq_ignore_ascii_case("pac") => a_.pac = true, // (#559) grade FEC corporate-PAC breadth
            _ if a.eq_ignore_ascii_case("failures") => a_.failures = true, // (#728) failed filers vs survivors
            _ if a.eq_ignore_ascii_case("rivals") => a_.rivals = true, // (#560) rival books on the same pond
            _ if a.eq_ignore_ascii_case("winners") => a_.winners = true, // (#583) where the top-10 book loses each window's winners
            _ if a.eq_ignore_ascii_case("halflife") => a_.halflife = true, // (Item 11) hold-period net-edge sweep
            _ if a.eq_ignore_ascii_case("stress") => a_.stress = true,   // (#6) inject crashed/delisted losers
            _ if a.eq_ignore_ascii_case("pit") => a_.pit = true, // (PIT) score each cutoff against the index AS IT WAS
            _ if a.eq_ignore_ascii_case("dead") => a_.dead = true, // (#751) walk dead members on tests/dead-members.csv
            _ if a.eq_ignore_ascii_case("deadflat") => (a_.dead, a_.dead_flat) = (true, true), // (#752) ... with no crash cut
            _ if a.eq_ignore_ascii_case("intraday") => a_.intraday = true, // (#809) open-to-close keys on 10y daily bars
            _ if a.eq_ignore_ascii_case("holdwindows") => a_.holdwindows = true, // (#1217) every calendar hold per asset class
            _ if a.eq_ignore_ascii_case("traderules") => a_.traderules = true, // (#1218) trend, breakout and swing rules at 1x-3x
            _ if a.eq_ignore_ascii_case("leverage") => a_.leverage = true, // (#1219) daily-reset 1.25x-3x S&P total return vs 1x
            _ if a.eq_ignore_ascii_case("lifecycle") => a_.lifecycle = true, // (#1220) Ayres-Nalebuff savers at 200/100 and 200/50
            _ if a.eq_ignore_ascii_case("bestpick") => a_.bestpick = true, // (#1222) the page's BEST picked on 2y, traded the next step
            _ if a.eq_ignore_ascii_case("rolls") => a_.rolls = true, // (#1224) the 20y DCA roll at 3m-10y, sell-all and leavers-only
            _ if a.eq_ignore_ascii_case("options") => a_.options = true, // (#1226) Cboe BuyWrite and PutWrite vs ^SP500TR over 20y
            _ if a.eq_ignore_ascii_case("time") => a_.time = true, // (#1118) wall seconds per phase on stderr
            _ if a.eq_ignore_ascii_case("rescore") => a_.rescore = true, // (#1227) re-run the book reports on a samples dump
            _ if a.eq_ignore_ascii_case("level") => a_.level = true, // (#1238) the level x path 2x2 on the entry-state book
            // (#1227) rival (e)'s random books: 200 keeps the gate's cost, 5000 lets a p below 1/201 print
            _ if a.starts_with("draws=") => a_.draws = a[6..].parse().ok().filter(|n| *n > 0).expect("draws=N takes a whole number above 0"),
            _ if a.eq_ignore_ascii_case("search") => a_.search = true, // (#1239) seeded knob search on samples dumps
            _ if a.starts_with("configs=") => a_.configs = a[8..].parse().ok().filter(|n| *n > 0).expect("configs=N takes a whole number above 0"),
            _ if a.starts_with("nulls=") => a_.nulls = a[6..].parse().expect("nulls=K takes a whole number"),
            _ => a_.tickers.push(a.clone()),
        }
    }
    a_
}

/// Trailing monthly returns in percent over the 36 months ending at `i`, for the CORR-CAP probe.
/// 36 ≈ the 200wk trend window; a zero/non-finite close drops that month (rare — alignment slippage
/// is acceptable for a correlation probe, and `corr_tail()` demands 12 overlapping months anyway).
///
/// Split out of the async walk in [`run`] for the same reason [`parse_args`] was: inline, nothing
/// could reach it. The filter only has an opinion about prices no healthy series contains, and every
/// offline pin runs on fixture closes that are all positive and finite — so the mutation audit
/// reported both `&&`->`||` and `>`->`>=` here surviving, and both are real. `||` admits a zero
/// close, and `closes[j + 1] / 0.0` is `inf`: an infinite monthly "return" fed straight into the
/// correlation, which is exactly the corruption the filter exists to prevent.
///
/// The base close is checked for finiteness too, which the inline version did NOT do — it tested
/// only `> 0.0`, and `f64::INFINITY > 0.0` is `true`, so an infinite base close divided into a
/// finite one and booked a **-100% month**: a fabricated total wipeout, finite enough to slip past
/// any "no inf" assertion. That is the doc line above ("a zero/non-finite close drops that month")
/// finally being true of the base as well as the forward close, not new hardening. Real closes are
/// finite, so no golden moves.
///
/// Panics if `closes.len() <= i`, same as the indexing it replaced. The caller only reaches this
/// with a full forward window left.
fn trailing_returns(closes: &[f64], i: usize) -> Vec<f64> {
    let ok = |p: f64| p.is_finite() && p > 0.0;
    (i.saturating_sub(36)..i)
        .filter(|&j| ok(closes[j]) && ok(closes[j + 1]))
        .map(|j| (closes[j + 1] / closes[j] - 1.0) * 100.0)
        .collect()
}

/// (#529) `FOLIOMAN_BACKTEST_SAMPLES=<path>`: one JSON line per sample, written once every cross-sectional
/// stamp is on and before any report reads them. The seam of the whole-walk look-ahead test in
/// `tests/backtest_fixture.rs`: a run that cannot see past a date must write these exact lines for every
/// cutoff whose forward window closes before it. (#857) The run then stops: no report is printed.
///
/// (#1227) Also the input of `backtest rescore`. `vol_change` rides beside the quote because `Quote` does
/// not serialize it, and the horizon and benchmark series go to [`bench_path`], so a dump can be re-scored
/// with no fetch at all.
fn write_samples(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, path: &std::path::Path) {
    let lines: String = samples
        .iter()
        .map(|s| {
            let q = &*s.quote;
            let line = serde_json::json!({"ticker": q.ticker, "date": s.date, "realized": s.realized, "trail": s.trail, "quote": q, "fund": s.fund, "vol_change": q.vol_change});
            line.to_string() + "\n"
        })
        .collect();
    std::fs::write(path, lines).expect("write FOLIOMAN_BACKTEST_SAMPLES");
    std::fs::write(bench_path(path), serde_json::to_string(&(years, bench)).expect("bench as json")).expect("write the dump's bench file");
}

/// (#1227) `<dump>.bench.json`: the run's horizon and the benchmark series its books were priced against.
fn bench_path(dump: &std::path::Path) -> std::path::PathBuf {
    let mut p = dump.as_os_str().to_owned();
    p.push(".bench.json");
    p.into()
}

/// (#1227) Knobs `backtest rescore` refuses in a grid row. Each one acts while the walk builds the samples,
/// or sets a process-wide value at config load, so a dump already carries the value it was made with and a
/// row that changed it would print the old run under a new name. Grade these with a probe run per arm.
const RESCORE_FIXED: [&str; 14] = [
    "backtest_anchor_windows",
    "backtest_tr_ruler",
    "drop_dead_ticker_series",
    "use_adjusted_close",
    "splice_max_weekly_rate",
    "flat_run_max_years",
    "vol_daily_equivalent",
    "growth_brake_min_sessions",
    "backtest_drop_lookahead_sector",
    "splice_trim_point_in_time",
    "sec_line_filed_gate",
    "life_cagr_max_years",
    "endpoint_smooth_days",
    "fund_source",
];

/// (#1227) One line of a [`write_samples`] dump, read back.
#[derive(serde::Deserialize)]
struct Dumped {
    date: chrono::NaiveDate,
    realized: f64,
    trail: Vec<f64>,
    quote: Quote,
    fund: Option<core::FundFactors>,
    vol_change: Option<f64>,
}

/// (#1227) `backtest rescore <dump> <grid>`: the top-N books and the SIZED row, re-run on a samples dump
/// once per grid row, with no fetch and no walk. A grid row is one JSON object of `buy_heuristic` keys laid
/// over the loaded config's; `{}` is the config itself. Run it under the config the dump was made with.
///
/// Only the cross-sectional stamp that reads the tuning is redone (the sector door's floors). The cohort
/// ranks are NOT: that stamp subtracts a median from the dumped value, so a second pass would shift it
/// again. `relative` is left 0, as the de-mean only feeds the lane tables, which this does not print. The
/// SIZED row runs with no spill legs, so its remainder earns the benchmark itself.
///
/// Known ceiling: JSON has no NaN, so a NaN field is written as null and a `Some(NaN)` reads back as None.
fn rescore(dump: &str, grid: &str, settings: &config::Settings) {
    let (years, bench, samples) = load_dump(dump);
    let base = serde_json::to_value(&settings.buy_heuristic).expect("tuning as json");
    for line in std::fs::read_to_string(grid).expect("read the grid").lines() {
        let row: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line).expect("a grid row is one JSON object");
        if let Some(k) = RESCORE_FIXED.iter().find(|k| row.contains_key(**k)) {
            panic!("rescore: `{k}` is fixed when the walk runs; grade it with a probe arm, not a grid row");
        }
        let mut v = base.clone();
        v.as_object_mut().expect("tuning is an object").extend(row);
        let tuning: BuyHeuristic = serde_json::from_value(v).unwrap_or_else(|e| panic!("rescore: grid row {line}: {e}"));
        println!("\n══ rescore row {line} ══");
        let mut s = samples.clone();
        stamp_sector_cohorts(&mut s, &bench, &tuning);
        report_vs_benchmark(&s, &bench, years, &tuning);
        report_sized_book(&s, &bench, years, &tuning, &settings.sizing, settings.top_picks, &[]);
    }
}

/// (#1227) A [`write_samples`] dump read back: its horizon, its benchmark series and its samples.
fn load_dump(dump: &str) -> (i64, (Vec<chrono::NaiveDate>, Vec<f64>), Vec<Sample>) {
    let (years, bench): (i64, (Vec<chrono::NaiveDate>, Vec<f64>)) =
        serde_json::from_str(&std::fs::read_to_string(bench_path(dump.as_ref())).expect("read the dump's bench file")).expect("bench json");
    let samples: Vec<Sample> = std::fs::read_to_string(dump)
        .expect("read the samples dump")
        .lines()
        .map(|l| {
            let d: Dumped = serde_json::from_str(l).expect("a dumped sample");
            let mut q = d.quote;
            q.vol_change = d.vol_change;
            q.name_class = Some(picks::name_class(&q)); // not serialized; stamped as the walk does, for speed
            Sample { date: d.date, realized: d.realized, relative: 0.0, quote: Arc::new(q), fund: d.fund, trail: d.trail }
        })
        .collect();
    (years, bench, samples)
}

/// (#1239) The knobs `search` scales beside the 13 [`picks::WEIGHT_DIMS`] weights: each cap, exponent and
/// floor that shapes a score or trims the cohort without gating a name out on its own. Same (label, get,
/// set) shape as the weights. A knob that ships at 0 stays 0, as a weight does.
const CAP_DIMS: [picks::WeightDim; 7] = [
    ("sharpe_cap", |t| t.sharpe_cap, |t, v| t.sharpe_cap = v),
    ("long_trend_cap", |t| t.long_trend_cap, |t, v| t.long_trend_cap = v),
    ("growth_accel_cap", |t| t.growth_accel_cap, |t, v| t.growth_accel_cap = v),
    ("growth_trust_exp", |t| t.growth_trust_exp, |t, v| t.growth_trust_exp = v),
    ("growth_overext_exp", |t| t.growth_overext_exp, |t, v| t.growth_overext_exp = v),
    ("growth_overext_floor", |t| t.growth_overext_floor, |t, v| t.growth_overext_floor = v),
    ("growth_value_floor_pct", |t| t.growth_value_floor_pct, |t, v| t.growth_value_floor_pct = v),
];

/// (#1239) The two [`picks::gate_notches`] rows `search` never draws: both gate on a field every backtest
/// sample leaves None, so loosening them admits no one.
const SEARCH_SKIP_NOTCHES: [&str; 2] = ["range8y", "aum"];

/// (#1239) `backtest search` defaults: configs drawn, and shuffles of `realized` priced as the null.
const SEARCH_CONFIGS: usize = 5000;
const SEARCH_NULLS: usize = 200;

/// (#1239) One random config, as multipliers on the loaded one: each weight x U(0, 2.5), each [`CAP_DIMS`]
/// knob x 2^U(-1, 1), then for each of `notches` gate notches how many times to loosen it, 0 to 2 and 0
/// most often (3u³ rounded down: 0 at 69%, 1 at 18%, 2 at 13%).
fn search_draw(next: &mut impl FnMut() -> f64, notches: usize) -> Vec<f64> {
    let mut d: Vec<f64> = (0..picks::WEIGHT_DIMS.len()).map(|_| 2.5 * next()).collect();
    d.extend((0..CAP_DIMS.len()).map(|_| (2.0 * next() - 1.0).exp2()));
    d.extend((0..notches).map(|_| (3.0 * next().powi(3)).floor()));
    d
}

/// (#1239) `base` moved by one [`search_draw`]. All ones then all zeros is `base` itself.
fn search_apply(base: &BuyHeuristic, d: &[f64], notches: &[fn(&mut BuyHeuristic)]) -> BuyHeuristic {
    let mut t = base.clone();
    for (i, (_, get, set)) in picks::WEIGHT_DIMS.iter().chain(&CAP_DIMS).enumerate() {
        set(&mut t, get(base) * d[i]);
    }
    let off = picks::WEIGHT_DIMS.len() + CAP_DIMS.len();
    for (k, loosen) in notches.iter().enumerate() {
        for _ in 0..d[off + k] as usize {
            loosen(&mut t);
        }
    }
    t
}

/// (#1239) A config's grade against the shipped one on the same returns: the worst, over the horizons, of
/// its top-10 excess gain in mean and in median. Above 0 is up on both at every horizon, SHIP RULE v2's
/// PRIMARY; at or below 0 the config is no better.
fn search_objective(got: &[(f64, f64)], shipped: &[(f64, f64)]) -> f64 {
    got.iter().zip(shipped).map(|(g, s)| (g.0 - s.0).min(g.1 - s.1)).fold(f64::INFINITY, f64::min)
}

/// (#1239) `realized` permuted within each (bucket, asset class) cell: a world where a name's score says
/// nothing about its own forward return, while each window and each class keep the returns they had.
/// Seeded Fisher-Yates on [`next_rand`]; a seed of 0 would never move.
fn shuffled(samples: &[Sample], seed: u64) -> Vec<f64> {
    let mut cells: BTreeMap<(i32, u8), Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        cells.entry((bucket(s.date), picks::asset_class(&s.quote))).or_default().push(i);
    }
    let mut out: Vec<f64> = samples.iter().map(|s| s.realized).collect();
    let mut state = seed;
    for idx in cells.values() {
        for k in (1..idx.len()).rev() {
            let j = (next_rand(&mut state) % (k as u64 + 1)) as usize;
            out.swap(idx[k], idx[j]);
        }
    }
    out
}

/// (#1239) Per bucket, oldest first, the sample indices the top-10 book holds under `tuning`, by the
/// row's own path in [`report_vs_benchmark`]: crypto out, a scored name with an index leg, the (#75) value
/// brake, a stable sort on score. The sector and corr caps are left out; `search` refuses a config that
/// arms either. `bench_r` is each sample's index leg, NaN where it has none. Picking never reads
/// `realized`, so one book prices every shuffle of it.
fn top_books(samples: &[Sample], bench_r: &[f64], tuning: &BuyHeuristic, scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>) -> Vec<Vec<usize>> {
    let mut by_cutoff: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate().filter(|(_, s)| picks::asset_class(&s.quote) != 0) {
        by_cutoff.entry(bucket(s.date)).or_default().push(i);
    }
    let mut rows = Vec::new();
    for (b, group) in &by_cutoff {
        let quotes: Vec<&Quote> = group.iter().map(|&i| samples[i].quote.as_ref()).collect();
        for (&i, scored) in group.iter().zip(scorer(&quotes, tuning)) {
            if let (false, Some(score)) = (bench_r[i].is_nan(), scored) {
                rows.push((*b, score, 0.0, 0.0, i.to_string(), samples[i].fund.as_ref().and_then(|f| f.peg_yield)));
            }
        }
    }
    value_floor_trim(&rows, tuning.growth_value_floor_pct)
        .into_values()
        .map(|mut v| {
            v.sort_by(|a, b| b.0.total_cmp(&a.0));
            v.iter().take(VERDICT_TOP).map(|x| x.3.parse().expect("a sample index")).collect()
        })
        .collect()
}

/// (#1239) The top-10 row's excess, mean and median over buckets, priced on `realized`: each bucket's
/// [`book_excess`], as [`report_vs_benchmark`] sums it, so the shipped config reprints that row's numbers.
fn search_excess(books: &[Vec<usize>], realized: &[f64], bench_r: &[f64], years: i64) -> (f64, f64) {
    let excess: Vec<f64> =
        books.iter().filter_map(|p| book_excess(&p.iter().map(|&i| (realized[i], bench_r[i])).collect::<Vec<_>>(), years)).collect();
    (excess.iter().sum::<f64>() / excess.len().max(1) as f64, median(excess))
}

/// (#1239) `backtest search <dump>... [configs=N] [nulls=K]`: a seeded random search of the knob space on
/// samples dumps, one per horizon, made under the config it runs with. Each config is graded by
/// [`search_objective`] against the loaded one. The null is the same configs re-graded on K shuffles of
/// `realized` ([`shuffled`]): the winner counts only if its gain beats what the search finds where no
/// score can see a return. Prints only; nothing reaches rank, BUY NOW, size or the broker.
#[mutants::skip] // prints only: its parts are graded by unit tests and its shipped row by the fixture pin
fn search(dumps: &[String], configs: usize, nulls: usize, settings: &config::Settings) {
    let base = &settings.buy_heuristic;
    assert!(
        base.growth_sector_leaders == 0 && base.growth_sector_cap == 0 && base.growth_corr_cap <= 0.0,
        "search: the sector door, sector cap and corr cap must be off; grade those with a probe arm"
    );
    let (labels, loosen): (Vec<&str>, Vec<_>) =
        picks::gate_notches().into_iter().filter(|n| !SEARCH_SKIP_NOTCHES.contains(&n.0)).map(|n| (n.0, n.2)).unzip();
    // per dump: horizon, samples, index legs, and `realized` as run 0 then each shuffle
    let dumps: Vec<_> = dumps
        .iter()
        .map(|d| {
            let (years, bench, samples) = load_dump(d);
            let bench_r: Vec<f64> = samples.iter().map(|s| benchmark_fwd(&bench.0, &bench.1, s.date, years).unwrap_or(f64::NAN)).collect();
            let runs: Vec<Vec<f64>> = std::iter::once(samples.iter().map(|s| s.realized).collect())
                .chain((1..=nulls as u64).map(|k| shuffled(&samples, 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(k))))
                .collect();
            (years, samples, bench_r, runs)
        })
        .collect();
    let grade = |t: &BuyHeuristic| -> Vec<Vec<(f64, f64)>> {
        dumps
            .iter()
            .map(|(years, samples, bench_r, runs)| {
                let books = top_books(samples, bench_r, t, picks::growth_scores_ranked);
                runs.iter().map(|r| search_excess(&books, r, bench_r, *years)).collect()
            })
            .collect()
    };
    let shipped = grade(base);
    let g = |got: &[Vec<(f64, f64)>], run: usize| {
        search_objective(&got.iter().map(|d| d[run]).collect::<Vec<_>>(), &shipped.iter().map(|d| d[run]).collect::<Vec<_>>())
    };
    let horizons: Vec<String> = dumps.iter().map(|d| format!("{}y", d.0)).collect();
    println!(
        "\n── SEARCH (#1239): {configs} seeded configs over {} weights, {} caps and {} gate notches, graded by the worst top-10 excess gain (mean or median) over {}; the null is the same configs on {nulls} within-(bucket, class) shuffles of realized ──",
        picks::WEIGHT_DIMS.len(),
        CAP_DIMS.len(),
        labels.len(),
        horizons.join(" and ")
    );
    for (h, s) in horizons.iter().zip(&shipped) {
        println!("  shipped: {h} top-10 excess {:+.1} (med {:+.1})", s[0].0, s[0].1);
    }
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || (next_rand(&mut state) >> 11) as f64 / (1u64 << 53) as f64;
    let draws: Vec<Vec<f64>> = (0..configs).map(|_| search_draw(&mut next, loosen.len())).collect();
    let graded: Vec<Vec<f64>> = draws
        .par_iter()
        .map(|d| {
            let got = grade(&search_apply(base, d, &loosen));
            (0..=nulls).map(|r| g(&got, r)).collect()
        })
        .collect();
    let best = (0..configs).fold(0, |b, c| if graded[c][0] > graded[b][0] { c } else { b });
    let real = graded[best][0];
    let mut null: Vec<f64> = (1..=nulls).map(|r| graded.iter().map(|v| v[r]).fold(f64::NEG_INFINITY, f64::max)).collect();
    null.sort_by(f64::total_cmp);
    let p = (1 + null.iter().filter(|x| **x >= real).count()) as f64 / (nulls + 1) as f64;
    let winner = search_apply(base, &draws[best], &loosen);
    let got = grade(&winner);
    println!("  {} of {configs} configs beat the shipped one on both moments at every horizon", graded.iter().filter(|v| v[0] > 0.0).count());
    println!(
        "  BEST: gain {real:+.2} pts/yr | {}",
        horizons.iter().zip(&got).map(|(h, s)| format!("{h} {:+.1} (med {:+.1})", s[0].0, s[0].1)).collect::<Vec<_>>().join("  ")
    );
    let (bv, wv) = (serde_json::to_value(base).expect("tuning as json"), serde_json::to_value(&winner).expect("tuning as json"));
    let moved: serde_json::Map<String, serde_json::Value> =
        wv.as_object().expect("tuning is an object").iter().filter(|(k, v)| bv.get(k.as_str()) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let notched: Vec<String> = labels.iter().zip(&draws[best][picks::WEIGHT_DIMS.len() + CAP_DIMS.len()..]).filter(|(_, n)| **n > 0.0).map(|(l, n)| format!("{l} x{n}")).collect();
    println!("  notches loosened: {}", if notched.is_empty() { "none".to_string() } else { notched.join(", ") });
    if !null.is_empty() {
        println!(
            "  NULL: the best gain on each shuffle, p50 {:+.2}  p95 {:+.2}  max {:+.2}; p = {p:.3} that a search this wide finds {real:+.2} by luck",
            percentile(&null, 50.0),
            percentile(&null, 95.0),
            percentile(&null, 100.0)
        );
    }
    // PARSIMONY: each moved knob put back alone, so a knob that buys nothing shows as no loss
    for k in moved.keys() {
        let mut v = wv.clone();
        v[k.as_str()] = bv[k.as_str()].clone();
        let t: BuyHeuristic = serde_json::from_value(v).expect("tuning from json");
        println!("  PARSIMONY: {k} put back -> gain {:+.2}", g(&grade(&t), 0));
    }
    // JITTER: the winner's weight and cap multipliers each x U(0.8, 1.2), notches kept
    let nudged: Vec<Vec<f64>> = (0..200)
        .map(|_| {
            let mut d = draws[best].clone();
            for x in &mut d[..picks::WEIGHT_DIMS.len() + CAP_DIMS.len()] {
                *x *= 0.8 + 0.4 * next();
            }
            d
        })
        .collect();
    let mut jit: Vec<f64> = nudged.par_iter().map(|d| g(&grade(&search_apply(base, d, &loosen)), 0)).collect();
    jit.sort_by(f64::total_cmp);
    println!("  JITTER: 200 nudges of the winner, gain p5 {:+.2}  p50 {:+.2}", percentile(&jit, 5.0), percentile(&jit, 50.0));
    println!("  VERDICT: {}", search_verdict(real, &null, &jit));
    println!("  GRID ROW: {}", serde_json::Value::Object(moved));
}

/// (#1240) The search's verdict on its winner's gain `real`, given the sorted best gain of each shuffle
/// (`null`) and of each nudge of the winner (`jit`). Look 1 passed a winner above the null's p95, which
/// hands the luckiest 1 in 20 searches a pass by chance, and the campaign looks every month. Now the winner
/// must beat EVERY shuffle, and its JITTER p5 must stay above zero, so a gain that holds only at its exact
/// knob values runs nothing. No shuffles, no verdict.
fn search_verdict(real: f64, null: &[f64], jit: &[f64]) -> &'static str {
    let Some(max) = null.last() else {
        return "no shuffles were run (nulls=0), so the search grades nothing";
    };
    if real <= max.max(0.0) {
        "the shipped config sits at the knob space's noise floor: no config beats it by more than the same search finds on shuffled returns"
    } else if percentile(jit, 5.0) > 0.0 {
        "the winner beats every shuffle and holds under its nudges; run the GRID ROW as a probe arm under SHIP RULE v2 before anything ships"
    } else {
        "the winner beats every shuffle but not its own nudges (JITTER p5 <= 0): a knife-edge, so nothing runs"
    }
}

/// The rayon pool size to force, or `None` to leave rayon alone.
///
/// `compute_threads: 0` (the default) deliberately builds NO pool: rayon's own default is already
/// every logical core, and leaving it untouched is what keeps `RAYON_NUM_THREADS` working as a
/// one-off override. Only an explicit cap needs a global pool.
///
/// All three comparison mutants survived inline, and `>=` is the one that matters: it pins the pool
/// at `num_threads(0)` on the DEFAULT config — the setting nobody edits — which is the shape of a
/// silent whole-machine performance regression rather than a wrong number.
fn thread_cap(compute_threads: usize) -> Option<usize> {
    (compute_threads > 0).then_some(compute_threads)
}

/// Fewest samples worth correlating. Named so the floor is one thing with one test, not a bare `4`
/// sitting inside `run` where nothing could reach it — the mutation audit reported both `<`->`<=`
/// and `<`->`==` surviving there.
const MIN_SAMPLES: usize = 4;

/// Too thin to correlate. `<=` here would throw away a legitimate 4-sample run; `==` would let a
/// 1-sample run through to `corr()` and print a correlation drawn from a single point.
fn too_few_samples(n: usize) -> bool {
    n < MIN_SAMPLES
}

/// (PIT) The point-in-time pool: today's index members LEAVE, every name that was EVER a member
/// ARRIVES — the dead ones included. That swap is the whole feature. `fetch_universe`'s equity pond is
/// `sp500_csv`, a list of the ~503 companies still in the index TODAY, so scoring a 1996 cutoff against
/// it asks "how did the eventual survivors do", which is the (#5) survivorship caveat stated as a
/// method. The membership map answers the honest question instead: who was in the index THAT DAY.
///
/// Only names carried by `sector_of` are removed, which is exactly the constituent-CSV pond —
/// crypto and the ETF lane are untouched, because neither was ever in the S&P 500 and filtering them
/// by it would empty two of the three asset classes.
///
/// KNOWN NARROWING, stated rather than buried: an EXTRA `constituents_csv` pond (an S&P MidCap 400
/// or a European index) also lives in `sector_of` and is dropped here with no historical twin to put
/// back, because this source covers the S&P 500 alone. So `pit` shrinks a multi-pond universe to one
/// pond. Add a second membership source before adding a second pond, or the comparison is unfair to
/// the pond that keeps its names.
fn pit_pool(tickers: &[String], sector_of: &HashMap<String, String>, spans: &core::MemberSpans) -> Vec<String> {
    let mut out: Vec<String> = tickers.iter().filter(|t| !sector_of.contains_key(*t)).cloned().collect();
    out.extend(spans.keys().cloned());
    out.sort();
    out.dedup();
    out
}

/// (PIT) Does this run swap its pool? Two refusals, and they refuse different disasters, which is why
/// neither half can be dropped:
///
/// - **no spans** — the source was unreachable or parsed to nothing. Swapping on an empty map DELETES
///   the index pond outright and scores the ETF and crypto lanes alone, which prints as a legitimately
///   small universe rather than as the failure it is.
/// - **no `sector_of`** — `fetch_universe` never ran, so this is an explicit ticker list. The caller
///   asked for those names; PIT filters their cutoffs but must not replace them with 1206 others.
///
/// A predicate rather than an `if` in `run` because `run` is one enormous `-> ()`: an operator inside
/// it is only ever graded through a golden, and the golden that would move here is the one path — a
/// live `universe` fetch — no offline pin can reach.
fn pit_swaps_pool(spans: &core::MemberSpans, sector_of: &HashMap<String, String>) -> bool {
    !spans.is_empty() && !sector_of.is_empty()
}

/// (PIT) How many pool names were index members that Yahoo NO LONGER SERVES. A COUNT, because the
/// alternative is what this command did for its whole life: drop the ticket in `buffer_unordered` and
/// print a total that silently omits them. A point-in-time universe whose dead names quietly fail to
/// fetch is just the survivors-only universe again, wearing a flag — so the number has to be visible
/// even when it is large, and ESPECIALLY when it is large.
///
/// Counted over the POOL and not over the whole membership map, so it means the same thing on both
/// paths: on a `universe` run the pool IS every member, on an explicit ticker list it is the members
/// among those tickers.
fn pit_unserved(pool: &[String], spans: &core::MemberSpans, served: &HashSet<&str>) -> usize {
    pool.iter().filter(|t| spans.contains_key(t.as_str()) && !served.contains(t.as_str())).count()
}

/// (#122) Does this ticker still map to the company it names, or has Yahoo repointed it at a fund id?
///
/// THE DEFECT, DEMONSTRATED RATHER THAN SUSPECTED. A ticker whose company died is not retired — it is
/// REMAPPED, and the series keeps running. `CFC` (Countrywide, out of the index 2008-07) carries 212
/// bars AFTER its exit, through 2026-08; `BOL` (taken private 2007) carries 220; `CCU` 212; `CBE` 52;
/// `MOLX` 49. Those post-exit bars are not the company's prices, because the company was gone. A hold
/// opened before the exit and closed `years` later therefore reads its ENTRY from the real company and
/// its EXIT from whatever now owns the ticker, and reports the ratio as a return. Replaying the walk's
/// 36-bar warmup and (before #790) 6-bar step over the cached series — an APPROXIMATION of the walk, not the walk
/// itself — 15-24% of PIT-eligible cutoffs have a forward window landing past the name's index exit
/// (20y 522/2196, 12y 956/5055, 8y 1039/7023). Most of those are healthy names that merely LEFT the
/// index and kept trading, which is fine; the dead ones are in there too and are NOT separable from
/// them by date, which is why the fix keys on identity below rather than on the exit date.
///
/// THE SIGNATURE, AND WHY IT IS THIS AND NOT A DATE TEST. Yahoo hands back the tell itself: a repointed
/// ticker types as `MUTUALFUND` and its only name is a numeric registrant id — `CFC` is "3847602",
/// `BSC` is "1315901", `WYE` is "4595480". Across all 6149 cached series that pattern matches 134
/// tickers and EVERY ONE of them is an S&P 500 member; there is not a single false positive in the
/// non-member remainder, which includes real mutual funds like `WLDHC.PA` that carry real names. 34 of
/// the 134 still carry bars, and those 34 are the ones actively contaminating a forward return.
///
/// WHAT IT CANNOT CATCH, STATED SO NOBODY READS IT AS A SURVIVORSHIP FIX. A ticker reused by another
/// LIVE equity keeps a real name and an `EQUITY` type, so this is blind to it: `CCU` now resolves to
/// Compania Cervecerias Unidas, `BEAM` to Beam Therapeutics (whose bars all start 2020, six years after
/// Beam Inc. was bought), `GENZ` to a VanEck ETF. Three failure modes, and this closes one. It also
/// does nothing for the 387 members served with zero bars — see [`pit_coverage`] — because a name that
/// never produces a cutoff cannot be bought and so cannot lose money.
fn ticker_mapping_is_dead(instrument_type: &str, name: &str) -> bool {
    instrument_type.eq_ignore_ascii_case("MUTUALFUND")
        && !name.is_empty()
        && name.chars().all(|c| c.is_ascii_digit())
}

/// (#121) (members in the pool, members that produced at least one scoreable cutoff).
///
/// WHY THIS IS NOT [`pit_unserved`], AND WHY THE DIFFERENCE IS THE WHOLE POINT. `pit_unserved` counts
/// names Yahoo answered NOTHING for — an outright fetch miss. Measured against `.sp500_history.json`
/// and the long history cache on 2026-08-22, that is **ZERO names**: every one of the 1206 members has
/// a cache entry. The hole is entirely invisible to it. Of the **734** members whose span carries an
/// exit date, **387** come back as a payload Yahoo is happy to serve and that carries ZERO BARS —
/// `BSC`, `TWX`, `SVU`, `ROH` and `ENRNQ` all return a `MUTUALFUND` stub with a numeric shortName —
/// and of the 347 that do carry bars, **193** hold fewer than 36 inside their own membership span.
/// All 580 are `served`, so `pit_unserved` scores them as fine; only **154** are scoreable.
/// That 387 is not an approximation: it is exactly the miss count this caveat reported on 2026-08-19,
/// before the cache warmed and turned every one of them into a served empty answer.
///
/// A name that dies therefore contributes NOTHING to this walk, not a loss. That is the direction the
/// bias runs, and it is not small: the heuristic v2 spec builds its case on Bessembinder's
/// median delisted return of −91.95%, and not one such return is in this dataset. Stronger still, the
/// count of members whose series ends more than a year before today is **ZERO out of 1206** — `CFC`
/// (Countrywide, out of the index 2008-07) has bars through 2026-08 and `MOLX` (acquired 2013) through
/// 2026-07. Yahoo is not serving a truncated history for these names; it is serving something else
/// entirely, and no name in this dataset ever stops trading.
///
/// So this returns the RATIO a reader can act on rather than a miss count they cannot: of the index
/// members this pool asked about, how many the walk could actually score. A free function and not an
/// `if` inside `run` for the reason [`pit_swaps_pool`] states — `run` is one enormous `-> ()` and the
/// only grader that reaches inside it is a golden.
fn pit_coverage(pool: &[String], spans: &core::MemberSpans, scored: &HashSet<&str>) -> (usize, usize) {
    let members = pool.iter().filter(|t| spans.contains_key(t.as_str()));
    (members.clone().count(), members.filter(|t| scored.contains(t.as_str())).count())
}

/// (#751) The dead members' monthly bars, read at run time rather than `include_str!`'d so the `dead`
/// arm ships before the data does: a missing file is an empty census, and the run says so.
const DEAD_MEMBERS_CSV: &str = "tests/dead-members.csv";
/// (#751) A dead series' terminal return is cut by [`DEAD_PENALTY`] when its last close is under this
/// share of the close 12 months earlier.
/// shortcut: one Shumway-style haircut for every crash, because the archive records neither a delist
/// reason (takeover vs failure) nor a delisting return. A sourced reason per name would replace both.
const DEAD_CRASH_RATIO: f64 = 0.5;
const DEAD_PENALTY: f64 = 0.7;
/// (#752) Tiingo's monthly bars for exited members, same rows as [`DEAD_MEMBERS_CSV`], written by
/// `tools/dead_members_from_tiingo.py` into a probe-only cache. Never committed: the data is licensed
/// for personal use. Missing (every run but a filled probe) = WIKI alone.
const TIINGO_DEAD_CSV: &str = ".tiingo_dead_members.csv";

/// (#751) The dead members' series from [`DEAD_MEMBERS_CSV`]: `ticker,month,close,div` rows, month as
/// `YYYY-MM`, split-adjusted like Yahoo's price-only close, each bar dated at its month's first day as
/// the long cache dates a monthly bar. A header or `#` line fails the parse and drops.
///
/// Returns (ticker -> (chart, dead), refused). DEAD = the series ends before the file's newest bar; the
/// rest are censored at the archive's end and walk like any live series, so no year is hard-coded. A
/// dead series must trade to within 30 days of its last S&P exit, or it is refused and counted: a
/// member cannot stop trading before it leaves, so an earlier end is a reused ticker or a hole.
fn dead_member_charts(csv: &str, spans: &core::MemberSpans) -> (HashMap<String, (fetch::Chart, bool)>, usize) {
    let mut rows: BTreeMap<&str, Vec<(chrono::NaiveDate, f64, f64)>> = BTreeMap::new();
    for l in csv.lines() {
        let [tk, month, close, div] = l.split(',').collect::<Vec<_>>()[..] else { continue };
        let date = chrono::NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d");
        if let (Ok(d), Ok(c), Ok(v)) = (date, close.parse::<f64>(), div.parse::<f64>()) {
            rows.entry(tk).or_default().push((d, c, v));
        }
    }
    let Some(end) = rows.values().flatten().map(|b| b.0).max() else { return (HashMap::new(), 0) };
    let (mut charts, mut refused) = (HashMap::new(), 0);
    for (tk, mut bars) in rows {
        bars.sort_by_key(|b| b.0);
        let last = bars[bars.len() - 1].0;
        let dead = last < end;
        // the bar stands for its whole month, so the series traded up to that month's last day
        let traded_to = last.checked_add_months(chrono::Months::new(1)).map(|d| d - chrono::Duration::days(1));
        let exit = spans.get(tk).and_then(|sp| sp.iter().map(|s| s.1).collect::<Option<Vec<_>>>()?.into_iter().max());
        if dead && !exit.zip(traded_to).is_some_and(|(e, t)| t >= e - chrono::Duration::days(30)) {
            refused += 1;
            continue;
        }
        let chart = fetch::Chart {
            dates: bars.iter().map(|b| b.0).collect(),
            closes: bars.iter().map(|b| b.1).collect(),
            volumes: Vec::new(),
            opens: Vec::new(),
            highs: Vec::new(),
            lows: Vec::new(),
            currency: "USD".to_string(),
            name: tk.to_string(),
            instrument_type: "EQUITY".to_string(),
            divs: bars.iter().filter(|b| b.2 != 0.0).map(|b| (b.0, b.2)).collect(),
            splits: Vec::new(),
        };
        charts.insert(tk.to_string(), (chart, dead));
    }
    (charts, refused)
}

/// (#751) Does this pool member walk on its dead-members series? Only when Yahoo's own bars give it no
/// point-in-time cutoff at all (none past the warm-up falls inside a membership span). Yahoo stays the
/// primary source; the archive only adds the names it cannot score. (#755) Or when the archive's series is
/// DEAD (`died`, its last bar) and Yahoo's runs on more than a quarter past it: those bars are the ticker's
/// next holder (Yahoo's CPWR is Ocean Thermal; Compuware's series ends in 2014), so the archive's is the member.
fn wants_dead_fill(yahoo: Option<&[chrono::NaiveDate]>, min_history: usize, spans: &[(chrono::NaiveDate, Option<chrono::NaiveDate>)], died: Option<chrono::NaiveDate>) -> bool {
    let reused = |d: &[chrono::NaiveDate]| died.zip(d.last()).is_some_and(|(e, l)| *l > e + chrono::Duration::days(92));
    !yahoo.is_some_and(|d| !reused(d) && d.iter().skip(min_history).any(|x| core::sp500_member_at(spans, *x)))
}

/// (#751) The forward return from cutoff `i`, and whether it is TERMINAL. A full `years` window is the
/// walk's old arithmetic, untouched, so every golden stays byte-identical. Without one, a dead series
/// (`dead_bench` = the S&P level) still pays out: its last close, rolled into the index from its last bar
/// to the window's end (a takeover's cash, reinvested), cut by the bench's penalty ([`DEAD_PENALTY`], or
/// (#752) 1.0 under `deadflat`) when it crashed first.
/// None = no window and no terminal, and the walk stops there as before: a live or censored series, or
/// an index that does not reach the window's end yet.
fn forward_return(
    dates: &[chrono::NaiveDate],
    closes: &[f64],
    i: usize,
    years: i64,
    dead_bench: Option<(&[chrono::NaiveDate], &[f64], f64)>,
) -> Option<(f64, bool)> {
    let target = dates[i] + chrono::Duration::days(years * 365);
    if let Some(off) = dates[i..].iter().position(|d| *d >= target) {
        return Some(((closes[i + off] / closes[i] - 1.0) * 100.0, false));
    }
    Some(((dead_multiple(dates, closes, closes[i], target, dead_bench?)? - 1.0) * 100.0, true))
}

/// (#804) The payouts a holder banked from cutoff `i` to `to`, as a multiple: every dividend with an ex-date
/// in `(dates[i], to]`, reinvested at the last close before that ex-date. 1.0 with none. Yahoo's dividend
/// amounts sit on the same split basis as its closes, so the ratio needs no restating.
fn payout_multiple(
    dates: &[chrono::NaiveDate],
    closes: &[f64],
    divs: &[(chrono::NaiveDate, f64)],
    i: usize,
    to: chrono::NaiveDate,
) -> f64 {
    divs.iter()
        .filter(|(d, _)| *d > dates[i] && *d <= to)
        .map(|(d, amount)| 1.0 + amount / closes[dates.partition_point(|x| x < d) - 1])
        .product()
}

/// (#751) What a dead series bought at `entry` is worth on `to`, past its last bar: its last close, rolled into
/// the index (`dead`'s dates and closes) from that bar to `to`, times the penalty when that close is under
/// [`DEAD_CRASH_RATIO`] of the one a year earlier. (#757) One helper for [`forward_return`]'s walk and
/// [`book_multiple`]'s DCA ruler, so the two price a death the same way. None when the index does not reach `to`.
fn dead_multiple(
    dates: &[chrono::NaiveDate],
    closes: &[f64],
    entry: f64,
    to: chrono::NaiveDate,
    dead: (&[chrono::NaiveDate], &[f64], f64),
) -> Option<f64> {
    let (bench_dates, bench_closes, crash_penalty) = dead;
    let at = |d: chrono::NaiveDate| bench_dates.iter().position(|x| *x >= d).map(|k| bench_closes[k]);
    let last = dates.len() - 1;
    let roll = at(to)? / at(dates[last])?;
    let year_ago = dates.iter().rposition(|d| *d <= dates[last] - chrono::Duration::days(365)).unwrap_or(0);
    let penalty = if closes[last] / closes[year_ago] < DEAD_CRASH_RATIO { crash_penalty } else { 1.0 };
    Some(closes[last] / entry * penalty * roll)
}

/// Does this run get to overwrite `.backtest_verdict.json`, which the screen's method footer quotes?
///
/// Both halves survived the audit and both are real. `&&`->`||` lets a plain watchlist run
/// (`backtest 12 AAPL`) publish a verdict over the gate's wide one. `>=`->`<` inverts the floor, so
/// the ONLY runs that publish are the thin ones the floor exists to reject — a Yahoo-throttled wide
/// run resolving a few hundred names instead of ~4900 is indistinguishable from a healthy one in the
/// file, and the footer goes on citing it for days.
///
/// (PIT) `pit` is the third refusal, and it is not about sample size. A point-in-time run answers a
/// DIFFERENT question than the screen footer asks — the footer quotes this journal to say what the
/// live method does on the live universe, and a PIT verdict is a lower number measured on a pool the
/// screen never ranks. Journaling one would silently restate the footer's claim on a basis nothing in
/// the footer mentions, which is worse than a stale number: it is a number that means something else.
fn may_write_verdict(wide: bool, pit: bool, resolved: usize) -> bool {
    wide && !pit && resolved >= MIN_VERDICT_TICKERS
}

/// (#788) The SIZED twin's rule: the verdict's floor, on a `pit` run that also walks the `dead` names. A
/// `pit` run without them gets no sample from a member that delisted inside its window, so its SIZED
/// record read the survivors only, the one pool `SIZED_FILE` exists to keep out of BUY NOW. (#1061) And
/// only with `fund`: without it no cutoff gets an as-of P/E, so the shipped PEG gate and the value factor
/// read missing and pass every name, and the record grades a book `size` never funds. (#1062) And never
/// from the `deadflat` arm: it walks the dead names with no crash cut, a sensitivity probe of
/// [`DEAD_PENALTY`], so its record is a kinder book than the shipped ruler grades.
fn may_write_sized(wide: bool, pit: bool, dead: bool, dead_flat: bool, fund: bool, resolved: usize) -> bool {
    pit && dead && !dead_flat && fund && may_write_verdict(wide, false, resolved)
}

/// Why a wide run declined to journal. A `String` and not an `if` at the call site because the call
/// site is inside `run`'s `-> ()`, where a branch is only ever graded through a golden — and no golden
/// reaches it, since journaling needs 500 resolved names and the fixture pool is 200.
fn no_verdict_reason(pit: bool, resolved: usize) -> String {
    if pit {
        return "backtest: verdict NOT journaled — a POINT-IN-TIME run measures a different pool than the \
                screen ranks, so its number must not overwrite the footer's; re-run without `pit` to journal"
            .to_string();
    }
    format!(
        "backtest: verdict NOT journaled — only {resolved} tickers resolved (need {MIN_VERDICT_TICKERS}); \
         the screen footer keeps citing the previous run"
    )
}

/// The two flags that turn on the fundamental/insider reports. Named because it gates two separate
/// blocks in [`run`] and `||`->`&&` survived at both: with `&&`, `backtest 12 fund` silently prints
/// none of the fundamental lane, and a run that looks like it worked has simply skipped the output
/// it was asked for.
fn fund_lane_on(fund: bool, insider: bool) -> bool {
    fund || insider
}

/// (#364) Whether `fund` must warn that the FMP key is missing. Only the FMP source needs it: the
/// shipped `sec` source reads SEC EDGAR keyless, and warning "the fundamental lane will be empty"
/// there was false (the fund golden prints 19 filled factors with no key).
fn fmp_key_missing(fund: bool, source: &str, key: Option<String>) -> bool {
    fund && source != "sec" && key.filter(|k| !k.is_empty()).is_none()
}

/// Which currency pair, if any, this ticker's closes must be converted through. `None` = same books,
/// or one side unknown; leave the close alone.
///
/// Split out because the guard is unreachable offline: every frozen-data golden is same-currency, so
/// forcing the guard to `true` OR to `false` left all six green. `true` would convert a USD close
/// into USD through a fetched rate; `false` would report a EUR close against USD fundamentals.
fn fx_pair<'a>(quote_ccy: Option<&'a str>, filer_ccy: Option<&'a str>) -> Option<(&'a str, &'a str)> {
    match (quote_ccy, filer_ccy) {
        (Some(q), Some(f)) if core::needs_fx(q, f) => Some((q, f)),
        _ => None,
    }
}

/// The close moved into the filer's books, or `None` when no honest as-of rate exists.
///
/// One multiply, extracted only because it is a money path with no offline cover: `*`->`+` and
/// `*`->`/` both survived, and either turns every cross-currency earnings yield into a number with
/// no meaning while the run still prints and still exits 0.
fn px_in_filer_ccy(close: f64, rate: Option<f64>) -> Option<f64> {
    rate.map(|r| close * r)
}

/// (#119) UNGRADEABLE BY THE MUTATION GATE, skipped for the same reason and on the same evidence as
/// `screen::run` (#68): `run` is reachable from `main.rs` and nowhere else, so the only test that
/// exercises it lives in the cli suite, which `ci.yml`'s mutants job does not run — it grades
/// `--lib --test backtest_fixture`. `replace run with ()` cannot be killed there.
///
/// Listed on this commit's own diff, which is what forced the issue: threading `years` into
/// `sweep_fund_factor` is a ONE-LINE change at the call site, and `--in-diff` grades whole functions,
/// so that line drags all 900 of them in. Every future edit here was armed the same way.
#[mutants::skip]
pub async fn run(args: Vec<String>) {
    let settings = config::load();
    // Here rather than in `main` because `config::load()` exits 1 on an unreadable config and `main`
    // currently reaches `help` without touching it; here rather than in `screen`/`sim` because this is
    // the only command with rayon in it. `build_global` is once-per-process and errors on a second
    // call — ignored, since the second caller is the test binary running two backtests in one
    // process, where the first pool is fine.
    if let Some(n) = thread_cap(settings.compute_threads) {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
    }
    let client = fetch::client();
    let tuning = &settings.buy_heuristic;
    // (#88) resolved ONCE, here, and handed down as a map rather than as a bool plus a source. An
    // empty map is exactly what `core::backtest_quote` used to hardcode, so `false` is byte-identical;
    // `true` measures the anchor the live tool actually uses. Resolved at the top because two paths
    // build quotes (the validated walk below, and `hold_period_sweep`) and they must not be able to
    // read the knob differently.
    let anchor_windows =
        if tuning.backtest_anchor_windows { settings.anchor_windows.clone() } else { BTreeMap::new() };

    let Args {
        years, wide, long, fund, tune, insider, holders, pac, failures, rivals, winners, halflife, stress, pit, dead, dead_flat, intraday, holdwindows, traderules,
        leverage, lifecycle, bestpick, rolls, options, rescore: rescore_dump, level, draws, search: search_dumps, configs, nulls, time, mut tickers,
    } = parse_args(&args);
    // (#1118) `time`: wall seconds per phase on stderr, so a slow section shows without a profiler.
    // Off, nothing prints. The fetch line covers the universe list, the histories and the benchmark.
    let mut mark = std::time::Instant::now();
    let mut lap = |what: &str| {
        if time {
            let now = std::time::Instant::now();
            eprintln!("backtest: time {:7.1}s {what}", (now - mark).as_secs_f64());
            mark = now;
        }
    };
    if intraday {
        intraday_ruler(&client, &settings).await;
        return;
    }
    if leverage {
        leverage_census(&client, &settings).await;
        return;
    }
    if lifecycle {
        lifecycle_census(&client, &settings).await;
        return;
    }
    if bestpick {
        best_census(&client, &settings).await;
        return;
    }
    if options {
        option_census(&client, &settings).await;
        return;
    }
    if rescore_dump {
        rescore(&tickers[0], &tickers[1], &settings);
        return;
    }
    if search_dumps {
        search(&tickers, configs, nulls, &settings);
        return;
    }
    // (#364) the condition is `fmp_key_missing`, unit-tested with the key passed in, so no test has to
    // mutate the process environment.
    if fmp_key_missing(fund, &crate::config::fund_source(), std::env::var("FMP_API_KEY").ok()) {
        eprintln!("backtest: `fund` set but FMP_API_KEY is empty — the fundamental lane will be empty (price lanes still run).");
    }
    // Daily 10y history caps the forward window at ~5y (the 3y warmup eats the rest of the 10y), so a
    // hold of ~8y+ — or an explicit `long` — switches to the MAX MONTHLY series: decades of bars for
    // old names, at the cost of monthly cadence (vol/MA are bar-approximations, see backtest_quote).
    let monthly = long || years >= 8;
    let cadence = if monthly { 12 } else { 252 }; // bars per year
    // (#18) so measure_endpoint's trading-days smoothing span covers the same calendar time on
    // monthly bars as it does on the live daily closes (train == serve).
    core::set_measure_cadence(cadence);
    let min_history = if monthly { 36 } else { MIN_HISTORY }; // ~3y of bars before a cutoff to form the long trend
    // (#46) the universe's own ETF ticker set and ticker->GICS sector map. Both were fetched and thrown
    // away (`.0`) — they are the belt to the chart-meta braces below: a venue whose meta omits
    // `instrumentType` must not silently drop a fund back into the stock class, and `is_commodity`
    // reads `quote.sector`. Empty on the narrow path, which then behaves exactly as before.
    let mut etf_set: HashSet<String> = HashSet::new();
    let mut sector_of: HashMap<String, String> = HashMap::new();
    if wide && tickers.is_empty() {
        // (#2) widen to the live screen universe (crypto + S&P 500 + Xetra UCITS ETFs) for a far bigger
        // sample. Slower (one history fetch per name) but the only cure for 53-survivor-ticker noise.
        eprintln!("backtest: fetching the live screen universe (this is the slow, wide-sample path)…");
        // no sector filter (&[]): the backtest measures edge across the FULL sample, never a slice
        //
        // (#116) THE VENUE ARGUMENT IS HARDCODED FALSE, and it is the one line in this file that must
        // never read a setting. This walk grades the DEEPEST record a name has; which venue a live row
        // is bought on is a `screen` question and reaches no score here. Passing the live knob through
        // made it one, and the collapse was not subtle: 266 of 519 S&P constituents resolve to a Xetra
        // twin, and the twin is a MEDIAN 18.6 YEARS YOUNGER than the US primary (218 of 261 by more
        // than five). CMI 1984-11 -> CUM.DE 2007-12. ORCL 1986-03 -> ORC.DE 2007-12.
        //
        // A cutoff needs `min_history` bars behind it PLUS the forward window, so a twin starting
        // 2007-12 contributes NO 20y cutoffs, SOME 12y and MOST 8y — the swap re-weights the sample by
        // horizon, in the one direction that looks like a scoring regression. Measured on CI run
        // 32456448769: 20y edge +775.8 (clean, no twin can reach it), 12y +58.2 (about half its ~117
        // baseline), 8y -22.8 — a RED gate on a universe change nothing in the diff mentioned.
        //
        // Every threshold in ci-settings.yaml was fitted on the US lines, and `.backtest_verdict.json`
        // of 2026-08-18 — the last wide run before the swap — journals 8y at top-3 excess +6.4 pts/yr
        // over 53 windows with both OOS halves positive. That is the sample those receipts describe.
        // `the_backtest_never_reads_the_venue_knob` is what keeps this argument a literal.
        //
        // (#347) FOLIOMAN_UNIVERSE_SNAPSHOT names a file: missing -> fetch live and write it there;
        // present -> read it and skip the fetch. `backtest_edge_holds` sets it so its three horizons
        // grade ONE universe. The list is a live fetch (~80s warm, ~167s cold) and the walk behind it is
        // ~5s, so three fetches were most of the gate. Only the returned triple rides along: the BF/YH
        // statics this fetch also fills are read by `quote_one` and the SEC fund/insider paths, never by
        // this walk. Measured on ci-settings, fetch arm vs snapshot arm of the same list: 12y and 8y
        // stdout byte-identical (29504 and 29855 bytes, 5338 names), 83s -> 3s and 78s -> 4s. An empty
        // or unreadable snapshot falls back to the live fetch.
        let snapshot = std::env::var_os("FOLIOMAN_UNIVERSE_SNAPSHOT");
        let saved = snapshot
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<(Vec<String>, HashSet<String>, HashMap<String, String>)>(&b).ok())
            .filter(|u| !u.0.is_empty());
        let universe = match saved {
            Some(u) => {
                eprintln!("backtest: universe read from FOLIOMAN_UNIVERSE_SNAPSHOT ({} names), not fetched", u.0.len());
                u
            }
            None => {
                let u = fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, false, &[])
                    .await;
                if let (Some(p), Ok(b)) = (&snapshot, serde_json::to_vec(&u)) {
                    let _ = std::fs::write(p, b);
                }
                u
            }
        };
        (tickers, etf_set, sector_of) = universe;
    } else if tickers.is_empty() {
        tickers = settings.tickers.clone();
    }
    // (#1217) the census reads the names and their classes, then none of the walk below
    if holdwindows {
        hold_windows(&client, &settings, &tickers, &etf_set, &sector_of, &anchor_windows).await;
        fetch::long_cache_save();
        lap("holdwindows");
        return;
    }
    // (#1218) likewise
    if traderules {
        trade_rules(&client, &settings, &tickers, &etf_set, &sector_of, &anchor_windows).await;
        fetch::long_cache_save();
        lap("traderules");
        return;
    }
    // (PIT) point-in-time index membership, fetched once (27 KB, cached forever). AN EMPTY MAP IS THE
    // OFF SWITCH: `pit` unset never fetches, an unreachable source returns empty and says so, and every
    // read below is a lookup that simply finds nothing — so the default path is bit-identical either way.
    let pit_spans =
        if pit { fetch::fetch_sp500_history(&client, &settings.urls).await } else { core::MemberSpans::new() };
    // The POOL swap is WIDE-ONLY, and `sector_of` is the test: it is non-empty exactly when
    // `fetch_universe` ran, and what it holds is that universe's index pond. An explicit ticker list
    // keeps every name the caller asked for and only has its CUTOFFS filtered below — which is also the
    // only shape of this feature a frozen-data golden can reach, since `universe` needs a live fetch.
    if pit_swaps_pool(&pit_spans, &sector_of) {
        tickers = pit_pool(&tickers, &sector_of, &pit_spans);
        eprintln!(
            "backtest: PIT — the index pond is now {} names that were EVER in the S&P 500, not the ~503 that survived to today",
            pit_spans.len()
        );
    }
    // (#6) survivorship stress: fold the crashed/delisted losers into the pool so the edge is graded
    // against a sample that INCLUDES the names the live universe drops. Dedup so a loser already in the
    // universe isn't double-counted. Compare rho/edge vs the same run WITHOUT `stress`: if the edge
    // survives the loser-inclusive pool (both OOS halves still +), it's real; if it collapses, the
    // engine was largely survivorship — stop tuning terms and shrink the claim.
    if stress {
        let have: HashSet<&str> = tickers.iter().map(String::as_str).collect();
        let added: Vec<String> =
            STRESS_TICKERS.iter().filter(|t| !have.contains(**t)).map(|t| (*t).to_string()).collect();
        eprintln!("backtest: STRESS — injecting {} crashed/delisted losers (any Yahoo no longer serves are skipped)", added.len());
        tickers.extend(added);
    }
    eprintln!(
        "backtest: {} tickers, WALK-FORWARD scoring every ~6mo with a {years}y forward holdout each ({} history)…",
        tickers.len(),
        if monthly { "MAX monthly" } else { "10y daily" }
    );

    // (Item 11) hold-period / signal half-life sweep: which forward window gives the best NET edge?
    // Self-contained (own price-only fetch, cached -> cheap) so the validated dispatch below is untouched.
    if halflife {
        hold_period_sweep(&client, &settings.urls, &tickers, monthly, cadence, min_history, tuning, &etf_set, &sector_of, &anchor_windows)
            .await;
        fetch::long_cache_save(); // this path fetches too — flush before the early return
        lap("halflife");
        return;
    }

    // (#3) per ticker, score at many cutoffs and pair each with its YEARS-forward realized return.
    //
    // FETCH FIRST, WALK SECOND. The two halves want opposite things: the fetch is network-bound and
    // `buffer_unordered` is exactly right for it, the walk is pure CPU — and a `buffer_unordered`
    // stream is polled by ONE task, so until this split the whole walk ran on one thread of eight.
    // ORDER IS PINNED, which is the part that matters: `fetched` comes out in completion order, so it
    // is sorted by ticker below; rayon's `collect` keeps its input order, so the flatten sees that
    // sequence, and the STABLE `sort_by_key(date)` after it inherits it for same-date ties, which
    // `bootstrap_edge_ci`'s pools and every probe `edge` then depend on.
    let mut fetched: Vec<_> = stream::iter(tickers.iter())
        .map(|tk| {
            let client = &client;
            let urls = &settings.urls;
            async move {
                // Deferred where it can be: a monthly payload rides into the rayon walk still in its
                // bytes, because parsing 5057 of them is most of a wide run's CPU and THIS loop is one
                // task on one thread. `fund` opts out — the FX branch below needs the quote currency,
                // which means parsing here anyway, and parsing twice would be worse than not deferring.
                // The daily arm has no disk cache, so it is network-bound and there is nothing to save.
                let hist = match (monthly, fund) {
                    (true, false) => fetch::chart_json_long(client, urls, tk).await.map(Hist::Raw),
                    (true, true) => fetch::fetch_history_long(client, urls, tk).await.map(Hist::Parsed),
                    (false, _) => fetch::fetch_history(client, urls, tk).await.map(Hist::Parsed),
                }?;
                // (G) one cached fundamentals fetch per ticker (only when `fund`); as-of factors are then
                // derived per cutoff from these rows with no further network. None -> the fund lane skips it.
                let fund_rows = if fund { fetch::fetch_fundamentals_ranked(client, urls, tk).await } else { None };
                // (FX) a foreign filer keeps its books in ITS currency while its US listing trades another
                // (ASML: EUR statements, USD ADR), so the price-joined factors below have to move the close
                // into the filer's books first — dividing a EUR EPS by a USD close is off by the whole FX
                // rate and looks entirely plausible. Same currency (every US filer) takes the None arm: no
                // fetch, no multiply, sample bit-identical. That's what makes this change additive.
                // shortcut: one uncached FX chart pair per foreign ticker (~2 extra fetches each). Share a
                // series cache across tickers if the foreign slice ever shows up in the run time.
                let filer_ccy = fund_rows.as_ref().and_then(|r| r.last().and_then(|x| x.currency.clone()));
                // `Hist::Raw` reaches here only when `fund` is off, and `filer_ccy` is `Some` only when
                // it is on, so the None arm below is unreachable for a deferred payload — the two are
                // gated by the same flag in the match above, which is why nothing has to parse to decide.
                let quote_ccy = match &hist {
                    Hist::Parsed(c) => Some(c.currency.clone()),
                    Hist::Raw(_) => None,
                };
                let fx = match fx_pair(quote_ccy.as_deref(), filer_ccy.as_deref()) {
                    // same `monthly` the closes came from: rates and prices must span the same era
                    Some((q, f)) => Some(fetch::fx_factor_series(client, urls, q, f, monthly).await),
                    None => None, // same books, or a side unknown -> leave the close alone (legacy path)
                };
                // (Item 4) one cached SEC Form-4 fetch per ticker (only when `insider`); net buys are then
                // derived per cutoff from these transactions with no further network. None -> factor skips.
                let insider_txns = if insider { fetch::fetch_insider_history(client, urls, tk, None).await } else { None };
                Some((tk, hist, fund_rows, fx, insider_txns))
            }
        })
        .buffer_unordered(fetch::fetch_concurrency())
        .collect()
        .await;
    // (#342) Live, completion order is fetch latency, and one config printed two edges (-231.2, then
    // -237.2). Offline it is input order. Either way the walk now sees A-Z; `tests/backtest_fixture.rs`
    // runs a Z-A pool against the same goldens.
    fetched.sort_by_key(|f| f.as_ref().map(|x| x.0));
    // every network read this command makes is done by here, so one flush covers all three remaining
    // exits below. The monthly payloads are the expensive part of a wide run and `screen` shares the
    // same file, so whichever ran first pays and the other reads free for a week.
    fetch::long_cache_save();
    // (#518) after the walk's fetches, so the history's SEC calls never starve the fund lane's budget
    let holder_txs = if holders { fetch::fetch_holder_history(&client, &settings.urls).await } else { HashMap::new() };

    // (PIT) the names the pool asked for and Yahoo could not answer for — counted HERE, while the
    // fetch tickets still remember which ticker they were, and printed in the caveats below. `fetched`
    // is about to be consumed by the walk, and a `None` in it carries no ticker at all.
    let pit_missing = {
        let served: HashSet<&str> = fetched.iter().flatten().map(|(tk, ..)| tk.as_str()).collect();
        pit_unserved(&tickers, &pit_spans, &served)
    };
    // (#751) the `dead` arm: a pool member Yahoo gives no point-in-time cutoff walks on its archive series
    // instead, and only a DEAD one (not censored at the archive's end) pays a terminal return in the walk.
    // Off, this is an empty set and nothing below reads the archive.
    let dead_penalty = if dead_flat { 1.0 } else { DEAD_PENALTY }; // (#752) `deadflat`: no crash cut
    let dead_end: HashSet<&str> = if dead {
        let csv = std::fs::read_to_string(DEAD_MEMBERS_CSV).unwrap_or_default();
        let (mut charts, mut refused) = dead_member_charts(&csv, &pit_spans);
        // (#752) Tiingo's series, graded on THEIR file's end (the fetch month, not the archive's 2018-03),
        // so neither file calls the other's censored names dead. Tiingo wins per ticker: one source per
        // name, no seam. The tool only fetches names the archive lacks or censors.
        let tiingo =
            std::fs::read_to_string(crate::config::data_path(TIINGO_DEAD_CSV)).unwrap_or_default();
        let (t_charts, t_refused) = dead_member_charts(&tiingo, &pit_spans);
        let t_dead = t_charts.values().filter(|c| c.1).count();
        let t_over = t_charts.keys().filter(|k| charts.contains_key(*k)).count();
        let t_n = t_charts.len();
        refused += t_refused;
        charts.extend(t_charts);
        let (mut filled, mut ends) = (0, HashSet::new());
        for tk in &tickers {
            let (Some((chart, is_dead)), Some(spans)) = (charts.get(tk), pit_spans.get(tk)) else { continue };
            let slot = fetched.iter().position(|f| f.as_ref().is_some_and(|x| x.0 == tk));
            let yahoo = slot.and_then(|k| fetched[k].as_ref()?.1.parse(tk));
            if !wants_dead_fill(yahoo.as_ref().map(|c| c.dates.as_slice()), min_history, spans, chart.dates.last().copied().filter(|_| *is_dead)) {
                continue;
            }
            let entry = Some((tk, Hist::Parsed(chart.clone()), None, None, None));
            match slot {
                Some(k) => fetched[k] = entry,
                None => fetched.push(entry),
            }
            filled += 1;
            if *is_dead {
                ends.insert(tk.as_str());
            }
        }
        fetched.sort_by_key(|f| f.as_ref().map(|x| x.0)); // (#342)'s A-Z order, with the fills in it
        let censored = charts.values().filter(|c| !c.1).count();
        eprintln!(
            "backtest: DEAD (#751) — {DEAD_MEMBERS_CSV} + (#752) {TIINGO_DEAD_CSV} ({t_n} series, {t_dead} dead, {t_over} replacing the archive's): {} series ({censored} censored at their file's end), {refused} refused (last bar >30d before the S&P exit); {filled} pool member(s) Yahoo could not score walk on them, {} of them dead, crash cut x{dead_penalty}{}",
            charts.len(),
            ends.len(),
            if charts.is_empty() { " — FILES MISSING OR EMPTY, so this arm is the control" } else { "" }
        );
        ends
    } else {
        HashSet::new()
    };
    let dead_end = &dead_end;

    // (#327) The pairs this WALK will use: the configured/journalled map, plus — when the knob is on —
    // the ones discovered point-in-time from the graded pool itself, which are the only ones whose
    // forward windows can close. Curated entries win, exactly as they do live.
    let proxy_map: BTreeMap<String, String> = if config::history_proxy_auto() {
        let months: Vec<(&str, i32, Vec<i32>, Vec<f64>)> = fetched
            .par_iter()
            .flatten()
            .filter_map(|(tk, hist, ..)| {
                let c = hist.parse(tk)?;
                let first = *c.dates.first()?;
                let (keys, rets) = month_series(&c.dates, &c.closes);
                Some((tk.as_str(), first.year() * 12 + first.month() as i32, keys, rets))
            })
            .collect();
        let found = discover_backtest_proxies(&months);
        eprintln!("backtest: history_proxy discovered {} point-in-time twin(s) across {} series", found.len(), months.len());
        let mut map = config::history_proxy().clone();
        for (young, donor) in found {
            map.entry(young).or_insert(donor);
        }
        map
    } else {
        config::history_proxy().clone()
    };
    let proxy_map = &proxy_map;
    // (#327) The donor series, parsed ONCE and only for tickers some young listing actually names. The walk
    // defers chart parsing on purpose (it is most of a wide run's CPU), so an all-pairs scan here would double
    // the hottest cost to answer a question live `screen` already answers for free off `trail_monthly`. A donor
    // the pool did not fetch is simply absent and its pairs refuse — the map is a candidate list, never a promise.
    let donors: HashMap<&str, (Vec<chrono::NaiveDate>, Vec<f64>)> = {
        let wanted: HashSet<&str> = proxy_map.values().map(|s| s.as_str()).collect();
        fetched
            .par_iter()
            .flatten()
            .filter(|(tk, ..)| wanted.contains(tk.as_str()))
            .filter_map(|(tk, hist, ..)| hist.parse(tk).map(|c| (tk.as_str(), (c.dates, c.closes))))
            .collect()
    };
    let donors = &donors;
    let etf_set = &etf_set;
    let sector_of = &sector_of;
    let pit_spans = &pit_spans;
    // (#1245) the membership source opens every incumbent's span on its first date: that is no addition
    let span_origin = pit_spans.values().flatten().map(|s| s.0).min().unwrap_or(chrono::NaiveDate::MIN);
    let factor = settings.buy_heuristic.growth_fund_factor.as_str(); // (G) config-selected as-of factor
    // (#751) fetched before the walk, which rolls a dead series' last close into it
    // (#804) `backtest_tr_ruler` grades on the total-return index too, with price-only selection
    let bench_sym = if bench_is_tr(crate::config::use_adjusted_close(), tuning) { "^SP500TR" } else { "^GSPC" };
    let bench = if monthly {
        fetch::fetch_history_long(&client, &settings.urls, bench_sym).await
    } else {
        fetch::fetch_history(&client, &settings.urls, bench_sym).await
    }
    // (FX) an index LEVEL, never joined to a filing — nothing to convert, so drop the currency. Nor is
    // it ever scored, so it needs no asset-class stamp either: prices only.
    .map(|c| (c.dates, c.closes))
    .unwrap_or_default();
    let dead_terminals = std::sync::atomic::AtomicUsize::new(0);
    lap("fetch");
    // (#310) the walk takes its horizon and BORROWS the fetch: (#311) a `2 × ROLL_YEARS` run walks the same history
    // again at 0y, every cutoff, for `report_roll`'s DCA books. Every other run walks once, exactly as before.
    let walk = |years: i64| -> Vec<Vec<Sample>> { fetched
        .par_iter()
        .flatten()
        .map(|(tk, hist, fund_rows, fx, insider_txns)| {
            // the deferred parse, now on all 8 threads. `None` (bad payload, or no bars) contributes an
            // empty Vec, which the `flatten` below drops — the same nothing the fetch-side `?` used to
            // contribute by dropping the ticket before it got here.
            let chart = match hist.parse(tk) {
                Some(c) => c,
                None => return Vec::new(),
            };
            // (#424) as-filed EPS and share counts onto the chart's split basis, BEFORE any cutoff divides
            // them by a close. Without it a pre-split filing over an adjusted close read the factor of every
            // later split too cheap (NFLX 70x), and splits follow run-ups, so the bias favoured future winners.
            let mut fund_rows = fund_rows.clone();
            if fund_rows.as_mut().is_some_and(|r| core::restate_for_splits(r, &chart.splits)) {
                SPLIT_RESTATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let (dates, closes) = (chart.dates, chart.closes);
            // (#531) the listing's own volume, paired before `apply_proxy` can glue donor years under it
            let own_volume: Vec<_> = dates.iter().copied().zip(chart.volumes).collect();
            // (#327) the listing's OWN first bar, read before anything older can be glued under it. Everything
            // before it on the spliced series is the donor's, and `own_first` is what keeps the walk from
            // standing in 1999 and buying a fund that listed in 2019.
            let own_first = dates.first().copied();
            let (dates, closes, history_proxied) = apply_proxy(tk, dates, closes, proxy_map, donors);
            let own_first = proxy_entry(own_first, history_proxied); // (#787) and the verify window after it
            // (#46) the ASSET-CLASS fields, carried out of the same response the closes came from (no
            // extra request). `backtest_quote` builds from `Quote::stub`, which leaves `name` as the
            // TICKER and `instrument_type` empty — and `quote_is_etf` reads exactly those two, so
            // before this every fund in the pool classified as a single stock. That silently merged
            // ~4300 ETFs into the ~500-name stock peer-mean `demean` splits by, and no-op'd every
            // ETF-scoped gate (the physical-gold/ETC bar among them). Yahoo's own `instrumentType`
            // tag leads because an ETF shortName often carries no "ETF"/"UCITS" marker at all.
            let (cls_name, cls_type) = (chart.name, chart.instrument_type);
            // (#122) the ticker no longer resolves to the company it names — its bars past the remap are
            // a different instrument's. Drop the whole series rather than its tail: the remap date is not
            // in the payload, so there is no honest place to cut. OFF by default; the goldens are the proof.
            if tuning.drop_dead_ticker_series && ticker_mapping_is_dead(&cls_type, &cls_name) {
                return Vec::new();
            }
            // (D) the dividend event list rode in on this SAME response and used to be dropped right
            // here — which is the whole reason the module header below claimed as-of dividends were
            // unreconstructable and `dividend_weight` shipped ungraded. `backtest_quote` slices it
            // to the cutoff, so no look-ahead arrives with it.
            let divs = chart.divs;
            let splits = chart.splits; // (#1245) read by `event_flags`, after the restate above
            // (PIT) this name's index spans, resolved ONCE per ticker rather than per cutoff. `None`
            // means "not an index name at all" — every coin, every ETF, and any stock that was never
            // an S&P 500 member — and is never filtered: PIT is a claim about the index pond alone.
            let member_spans = pit_spans.get(tk.as_str()).map(Vec::as_slice);
            // (#751) only a dead archive series may pay a terminal return; None keeps the old walk exactly
            let dead_bench = dead_end.contains(tk.as_str()).then_some((bench.0.as_slice(), bench.1.as_slice(), dead_penalty));
            let mut out = Vec::new();
            // (#790) The cutoffs sit on the half-year grid: see `cutoffs`.
            for i in cutoffs(&dates, min_history) {
                // (PIT) was this name IN the index on the day we are pretending to stand? If not, the
                // cutoff never existed for a real screener and contributes no sample — that is the whole
                // point-in-time correction, and it cuts BOTH ways: a future member is not scored early,
                // and a departed one stops being scored at its exit rather than at its delisting.
                if member_spans.is_some_and(|sp| !core::sp500_member_at(sp, dates[i])) {
                    continue;
                }
                // (#327) THE PROXY'S POINT-IN-TIME GUARD. A spliced series carries bars from years before this
                // ticker was listed, and `core::backtest_quote` slices `[splice..=as_of]` without asking whether
                // the as-of day was one anybody could have bought on. Without this line a 2019 fund becomes
                // buyable in 1999 and the borrowed years get scored as its own forward returns. Borrowed bars
                // are history, never an entry. (#787) Nor is the verify window: see `proxy_entry`.
                if own_first.is_some_and(|f| dates[i] < f) {
                    continue;
                }
                // forward return: first session at least `years` past the as-of date, or (#751) a dead
                // series' terminal payout. Record EVERY cutoff with one (not just gated ones) so the
                // peer-mean spans the whole period universe; each lane filters by its own gates.
                match forward_return(&dates, &closes, i, years, dead_bench) {
                    Some((realized, terminal)) => {
                        // (#804) the total-return ruler: the window's payouts compound into the forward
                        // return, and a cutoff ^SP500TR does not reach (pre-1988) has no fair benchmark, so
                        // it is dropped. shortcut: payouts are counted to the target date, not to the bar
                        // that closes the window, so up to one bar's payout is missed on every arm alike.
                        let realized = if tuning.backtest_tr_ruler {
                            if bench.0.first().is_none_or(|f| *f > dates[i] + chrono::Duration::days(31)) {
                                continue;
                            }
                            let to = dates[i] + chrono::Duration::days(years * 365);
                            ((1.0 + realized / 100.0) * payout_multiple(&dates, &closes, &divs, i, to) - 1.0) * 100.0
                        } else {
                            realized
                        };
                        if !realized.is_finite() {
                            // zero/garbage close -> ±inf poisons the demeaned bucket; skip the cutoff.
                            continue;
                        }
                        dead_terminals.fetch_add(usize::from(terminal), std::sync::atomic::Ordering::Relaxed);
                        let mut quote = core::backtest_quote(tk, &dates, &closes, &divs, i, cadence, &anchor_windows);
                        quote.history_proxied = history_proxied; // (#327) provenance, for the PROXY BOOK split
                        stamp_asset_class(&mut quote, &cls_name, &cls_type, etf_set, sector_of);
                        if tuning.growth_event_months > 0 {
                            let m = tuning.growth_event_months as u32;
                            quote.event_flags = event_flags(&divs, &splits, member_spans, span_origin, dates[i], m);
                        }
                        // NOT `years`. `years` is the FORWARD hold horizon; `fund_factors` spends its
                        // third argument as the BACKWARD fundamental lookback (core.rs, `long_ago =
                        // back(yrs)`, the row `yrs` fiscal years back). Passing the horizon meant `fund 12`
                        // demanded twelve years of filed statements before every cutoff — SEC XBRL
                        // starts ~2007 and a 12y run's cutoffs end 2014-07, so rev_cagr / rev_accel /
                        // eps_growth were None on EVERY sample and printed "n/a (only 0 cutoffs)".
                        // Their sweep lines then read exactly the price-only baseline, which looks
                        // like "measured, no edge" and was actually "never measured at all".
                        let mut fund =
                            fund_rows.as_ref().map(|r| core::fund_factors(r, dates[i], FUND_LOOKBACK_YRS));
                        // (Item 4) attach the as-of net insider buys (90d before the cutoff, transaction-
                        // date guarded) onto the SAME FundFactors; build one if the FMP lane is off so
                        // `insider` works standalone (no FMP key needed).
                        if let Some(txns) = insider_txns.as_ref() {
                            fund.get_or_insert_with(Default::default).insider_net_buys_90d =
                                core::insider_net_buys(txns, dates[i], 90);
                        }
                        // (Item 19) as-of earnings yield from the as-of close, in the SAME currency the
                        // EPS beside it is reported in. (FX) `fx` is None for every same-currency name,
                        // so `px` is the raw close on that path — no rate, no multiply, unchanged. A
                        // cutoff older than the FX series has no honest rate: None the three factors
                        // rather than borrow a later one, which would be look-ahead in a walk-forward lane.
                        if let Some(f) = fund.as_mut() {
                            let px = match fx.as_ref() {
                                None => Some(closes[i]),
                                Some(s) => px_in_filer_ccy(closes[i], core::rate_as_of(s, dates[i])),
                            };
                            f.earnings_yield = px.and_then(|p| core::earnings_yield(f.eps_ttm, p));
                            // (#147) THE P/E FILL. Until now `quote.pe_ratio` was never written on this
                            // path — `pe_ratio` appeared zero times in this file — so `picks::value_factor`
                            // returned 1.0 on every row and the two terms reading it were graded as
                            // different functions than the ones that ship. The yield directly above is
                            // eps_ttm over the as-of close in the filer's currency; a P/E is that inverted,
                            // and a ratio carries no currency, so no rate is applied. Available ONLY on the
                            // `fund` lane, because an as-of P/E needs as-of EPS: on a price-only run this
                            // stays None and every golden is byte-identical, which is the inertness proof.
                            quote.pe_ratio = core::pe_from_earnings_yield(f.earnings_yield);
                            // (EV/EBITDA) same close, same currency discipline: EV = shares·px + net_debt,
                            // all as-of. Still PROBE-ONLY — never the live score's weighed factor.
                            f.ebitda_yield = px.and_then(|p| core::ev_ebitda_yield(f.ebitda_ttm, f.shares_ttm, f.net_debt, p));
                            // (#408) the same EV, over FCF and over revenue
                            f.fcf_ev_yield = px.and_then(|p| core::ev_ebitda_yield(f.fcf_ttm, f.shares_ttm, f.net_debt, p));
                            f.sales_ev_yield = px.and_then(|p| core::ev_ebitda_yield(f.revenue_ttm, f.shares_ttm, f.net_debt, p));
                            // (#409) book over market cap (no net debt -> EV is the cap), and the cap itself.
                            // shortcut: the cap is in TRADING currency (USD for the US-listed pool) so names
                            // rank on one unit; an ADR whose ratio isn't 1 mis-ranks by that ratio. A
                            // filer-currency cap is worse: a JPY filer would read ~150x bigger.
                            f.book_yield = px.and_then(|p| core::ev_ebitda_yield(f.book_ttm, f.shares_ttm, None, p));
                            f.mcap = f.shares_ttm.map(|sh| sh * closes[i]).filter(|m| *m > 0.0);
                            // (#410) R&D over the cap, positive-only like book (CLS's R&D-firm universe),
                            // and net payout over the cap with its sign, so a net issuer ranks last.
                            f.rnd_yield = px.and_then(|p| core::ev_ebitda_yield(f.rnd_ttm, f.shares_ttm, None, p));
                            f.payout_yield = px.and_then(|p| core::cap_yield(f.payout_ttm, f.shares_ttm, p));
                            // (#413) intangible-adjusted book over the cap, positive-only like book
                            f.int_yield = px.and_then(|p| core::ev_ebitda_yield(f.int_book_ttm, f.shares_ttm, None, p));
                            // (PEG) 1/PEG = earnings_yield · as-of CAGR. This one IS shipped live now
                            // (growth_fund_factor "peg_yield"), and the live enrich converts the same way —
                            // so train and serve compute the identical ratio instead of differing by an FX rate.
                            //
                            // (#37) the CAGR is `long_cagr_pct` — the score's own, honouring use_life_cagr /
                            // use_trend_cagr / fixed_cagr_years — not the raw `quote.trend_cagr` this read
                            // until 2026-07-27. backtest_quote fills `perf`, `life_cagr` AND `trend_cagr` at
                            // this cutoff, so every arm of that switch is reconstructable as-of and
                            // train==serve still holds. Keep this in lockstep with fetch.rs's enrich.
                            f.peg_yield = px.and_then(|p| core::peg_yield(f.eps_ttm, picks::peg_cagr_pct(&quote, tuning), p));
                            // (#416) the same yield over three other growth terms: the chained EPS CAGR
                            // (Lynch), ROE · retention (sustainable growth), and the earnings-yield CAGR,
                            // EPS against price over the SAME 5y (FUND_LOOKBACK_YRS), high = de-rated.
                            // shortcut: the 5Y leg is in trading currency and the EPS in the filer's, so an
                            // ADR's FX drift leaks into exp_neg; the pool is US filers bar a handful.
                            f.lynch_yield = px.and_then(|p| core::peg_yield(f.eps_ttm, f.eps_growth, p));
                            f.sgr_yield = px.and_then(|p| core::peg_yield(f.eps_ttm, core::sgr_pct(f), p));
                            f.exp_neg = core::ey_cagr(f.eps_growth, picks::perf_pct(&quote, "5Y").map(|c| core::cagr(c, 5.0)));
                            if let Some(p) = px { picks::stamp_fair_yields(f, p) } // (#523) the FAIR family, as fetch.rs stamps it
                            f.low_turnover = low_turnover(&own_volume, dates[i], f.shares_ttm); // (#531)
                        }
                        // (G) fold the as-of factor INTO the growth lane so growth_fund_weight is ablatable.
                        // WHICH factor is config-driven (`growth_fund_factor`, default "rev_accel") — set it
                        // in settings.yaml to whichever report_fund_lane (below) shows +rho + both-half OOS,
                        // no recompile. Price-only backtest (no `fund`/key) leaves this None -> growth_score
                        // neutral -> validated edge untouched.
                        quote.fund_factor = fund.as_ref().and_then(|f| core::select_fund_factor(f, factor));
                        // (G+) the whole struct, for `growth_fund_extra`'s named terms. Cloned here
                        // rather than moved: `fund` is kept on the Sample so the factor sweep can
                        // re-select from it without rebuilding.
                        quote.fund = fund.clone();
                        // UN-BLIND the quality term. `backtest_quote` builds from `Quote::stub` (roe
                        // None) and fills only closes-derived fields, so before this line
                        // `quality_reward` was 0.15 × 0 for EVERY sample in EVERY lane — the term was
                        // shipped live but structurally invisible to the walk-forward that is supposed
                        // to price it. Every prior measurement of `quality_weight` was taken with the
                        // term switched off. Price-free level (no FX, unlike the three yields above),
                        // through the same fund_as_of look-ahead guard.
                        quote.roe = fund.as_ref().and_then(|f| f.quality);
                        // (round 112) this is the only place with the raw series in scope.
                        let trail = trailing_returns(&closes, i);
                        out.push(Sample { date: dates[i], realized, relative: 0.0, quote: Arc::new(quote), fund, trail });
                    }
                    None => break, // no full forward window left -> stop walking this ticker
                }
            }
            out
        })
        .collect() };
    let per_ticker = walk(years);
    lap("walk");

    let mut samples: Vec<Sample> = per_ticker.into_iter().flatten().collect();
    if too_few_samples(samples.len()) {
        println!(
            "backtest: only {} cutoffs had a full {years}y forward window — too few to correlate.",
            samples.len()
        );
        return;
    }
    samples.sort_by_key(|s| s.date); // chronological -> the OOS split is early-vs-late in time
    stamp_cohort_ranks(&mut samples); // (#408)/(#409) need the whole date's cohort, so after the sort

    // (#326) the market's state at each cutoff, stamped onto the quotes BEFORE any report scores them
    // (`report_lane`, `gate_audit`, the GATE SWEEP and the NOTCH books all gate through `growth_score`).
    // With the knob off this writes two fields nothing reads, and every number below is unchanged.
    stamp_regime(&mut samples, &bench);
    // (#328) the sector door's point-in-time cohort, stamped here for the same reason and at the same
    // point as the regime above: every report below gates through `growth_score`. With the knob off the
    // stamp is written and never read, so each number below is unchanged — and the SECTOR BOOK row can
    // still price the door for free by re-scoring with the knob flipped.
    stamp_sector_cohorts(&mut samples, &bench, tuning);
    lap("stamps");
    if let Some(p) = std::env::var_os("FOLIOMAN_BACKTEST_SAMPLES") {
        write_samples(&samples, &bench, years, p.as_ref()); // (#529) the look-ahead test's seam; unset = no-op
        return; // (#857) the test reads only the samples file, so the reports below are wasted wall time
    }

    // `tune`: honest out-of-sample selection. Search the growth weights on an EARLY train split and
    // report the winner on a LATE test split it never saw — the only way to a trustworthy number when
    // the shipped knobs were hand-tuned on all the data. Does its own per-split de-mean, so branch here
    // BEFORE the whole-sample de-mean below.
    if tune {
        tune_growth(&samples, tuning);
        lap("tune");
        return;
    }
    // (#311) the DCA ruler's books: every point-in-time cutoff (a 0y walk needs no forward window), date-sorted for
    // `month_pool`, and each name's closes to price a buy in any month. Empty at every other horizon.
    let (mut books, series): (Vec<Sample>, HashMap<&str, (Vec<chrono::NaiveDate>, Vec<f64>)>) = if years == 2 * ROLL_YEARS {
        let series = fetched.par_iter().flatten().filter_map(|(tk, hist, ..)| hist.parse(tk).map(|c| (tk.as_str(), (c.dates, c.closes)))).collect();
        (walk(0).into_iter().flatten().collect(), series)
    } else {
        Default::default()
    };
    books.sort_by_key(|s| s.date);
    // (#328) the DCA ruler's books are a SECOND sample set, built after the stamp above, so they need
    // their own — otherwise the knob would move the lump-sum lane and leave the DCA lane (the ruler
    // (#311)/(#312)/(#313) actually ship against) quietly unarmed.
    stamp_sector_cohorts(&mut books, &bench, tuning);

    // (#1) de-mean realized return WITHIN each ~6-month cutoff bucket AND asset class. Pooling raw returns across cutoffs
    // that span different regimes makes the score race CALENDAR LUCK (a 2016 cutoff that mooned vs a
    // 2021-top cutoff that crashed), not stock-picking. Subtracting the bucket's peer mean leaves only
    // "did this name beat the others scored the same half-year" = the selection signal we actually want.
    demean(&mut samples);
    // invariant: per-bucket de-meaning makes the relatives sum to ~0 (each bucket nets out). Fails loudly
    // if the bucket map and the fill ever drift apart. Cheap, runs in release.
    let rel_sum: f64 = samples.iter().map(|s| s.relative).sum();
    assert!(rel_sum.abs() < 1e-3 * samples.len() as f64, "de-mean broken: relatives sum to {rel_sum}");
    lap("books");

    // HEAD-TO-HEAD: report both lanes against the SAME peer-relative returns. The on-sale lane buys
    // pullbacks; the growth lane buys near-high compounders still climbing. Whichever has the higher
    // (more positive) rho is the one actually selecting winners on this data.
    println!("\nBacktest — WALK-FORWARD score vs {years}y-forward PEER-RELATIVE return (de-meaned per ~6mo cutoff):");
    println!("  cutoffs with a forward window: {}   tickers: {}", samples.len(), tickers.len());

    // PER-CLASS CENSUS. The backtest reconstructs quotes from price history alone, so `name`/
    // `instrument_type`/`sector` have to be stamped back on (stamp_asset_class) — without them every fund
    // classes as a single stock and `demean` grades stocks against a peer-mean padded with index funds.
    // A zero ETF count on the wide path means the stamping broke. Read the ratio, not the raw count:
    // forward-window survival scales with listing age, so ETF density here is horizon-dependent (0.3% of
    // cutoffs at 20y, 22% at 12y, 42% at 8y — 2026-08-02) and is NOT the ~87% the ticker list suggests.
    // The crypto count is the honest measure of how much coin history survives a long forward window
    // (~11y of history, so it thins to nothing: 0 scored cutoffs at every horizon measured).
    let census = class_census(&samples, tuning);
    println!(
        "  by class (growth-scored / cutoffs): crypto {}/{}   ETF {}/{}   stock {}/{}",
        census[0].1, census[0].0, census[1].1, census[1].0, census[2].1, census[2].0
    );
    // (#46) STAMPING GUARD — same standing as the de-mean invariant above: cheap, runs in release.
    // Reads the MECHANISM, not the count. A raw "ETF count > 0" check would false-fire at long horizons,
    // where a fund legitimately cannot reach a forward window at all (20y-s yields 14 ETF cutoffs; a 25y
    // run would yield none, UCITS funds dating from ~2000). This fires only when a name the universe
    // KNOWS is a fund fails to class as one — which is precisely how ~4300 ETFs scored as single stocks
    // for as long as this command has existed, with nothing anywhere to say so.
    //
    // DELIBERATELY UNPINNED (mutation audit, round 4): `!=`->`==` survives, i.e. the guard inverted
    // to panic on a CORRECTLY classed fund. It survives because `etf_set` is empty in every frozen
    // golden — the set comes from the live universe fetch — so the predicate short-circuits before
    // the comparison and no offline run evaluates it at all. Killing it means building `Sample`s by
    // hand with a populated `etf_set`, which grades this line by reimplementing the pipeline that
    // produces it. `stamp_asset_class` is pinned directly instead, one call up.
    if let Some(bad) = samples
        .iter()
        .find(|s| etf_set.contains(&s.quote.ticker) && picks::asset_class(&s.quote) != 1)
    {
        panic!(
            "class stamping broke: {} is in the universe's ETF set but classed {} — every fund is \
             scoring as a single stock and the peer-relative numbers below are contaminated",
            bad.quote.ticker,
            picks::asset_class(&bad.quote)
        );
    }

    let buy_knobs: Vec<Knob> = vec![
        knob("long_trend_weight", |tuning| tuning.long_trend_weight = 0.0),
        knob("discount_weight", |tuning| tuning.discount_weight = 0.0), // (#4) zero the dip reward — Δ>0 confirms dip-depth ranks backwards
        knob("cheap_weight", |tuning| tuning.cheap_weight = 0.0),
        // (#61) THIS ROW USED TO ZERO `dividend_weight` and was the loudest row in the on-sale table
        // (Δ+150.7). It now zeroes the lane's own split knob, because after the split zeroing the
        // shared one would move this lane by exactly nothing and print a confident Δ+0.0 saying so.
        // The growth list keeps its `growth_dividend` row on `dividend_weight`, which is still that
        // lane's live weight. At the shipped `onsale_dividend_weight: 0.0` this row is the NULL of an
        // already-off term and reads Δ+0.0 by construction — that zero is the split holding, not the
        // term being inert, and it is why the knob was kept rather than the term deleted.
        knob("onsale_dividend_weight", |tuning| tuning.onsale_dividend_weight = 0.0),
        knob("onsale_sharpe_weight", |tuning| tuning.onsale_sharpe_weight = 0.0),
        knob("calmar_weight", |tuning| tuning.calmar_weight = 0.0),
        knob("quality_weight", |tuning| tuning.quality_weight = 0.0), // shared with the growth lane — one knob, so it must be ablatable in both
        // (#58) The englobamento SPLIT off — both arms set equal, which is this knob's real "off" (0.0
        // would mean an EU dividend is worth nothing, a different question). Prices the SPLIT; the row
        // above prices the dividend WEIGHT. The two are not the same measurement and the weight is the
        // one that moves: dropping it costs Δ+142.8 edge, while equalising the arms costs Δ+0.0 — the
        // shipped 0.76-vs-0.72 gap is 4 points on a term whose whole weight is 0.5.
        //
        // IT LIVES IN THIS LANE, not the growth one, for sample: the growth gates admit 4 EU payers out
        // of 80 scored rows on the fixture, so a growth-lane sweep of this knob is flat for want of rows
        // whatever the true effect. This lane scores 526 windows. A `weight_curve` here was tried and
        // deleted — 17 lines of golden output to report the same Δ+0.0 this one line reports.
        knob("tax_split ->off", |tuning| tuning.tax_keep_eu = tuning.tax_keep_other),
    ];
    let growth_knobs: Vec<Knob> = vec![
        knob("growth_trend_weight", |tuning| tuning.growth_trend_weight = 0.0),
        knob("growth_accel_weight", |tuning| tuning.growth_accel_weight = 0.0),
        knob("sharpe_weight", |tuning| tuning.sharpe_weight = 0.0),
        knob("calmar_weight", |tuning| tuning.calmar_weight = 0.0),
        knob("overext_brake", |tuning| tuning.growth_overext_cap = 0.0),
        knob("growth_fund_weight", |tuning| tuning.growth_fund_weight = 0.0), // (G) Δ shows the as-of fund factor's through-the-lane edge; ~0 when weight is already 0 (default) or no fund coverage
        knob("growth_mom121_weight", |tuning| tuning.growth_mom121_weight = 0.0), // (M) Δ shows the 12-1 momentum term's through-the-lane edge; ~0 when weight is 0 (default)
        knob("growth_smoothness_weight", |tuning| tuning.growth_smoothness_weight = 0.0), // (E) Δ shows the trend-smoothness reward's through-the-lane edge; ~0 when weight is 0 (default)
        knob("growth_underwater_weight", |tuning| tuning.growth_underwater_weight = 0.0), // Δ shows the drawdown-duration penalty's through-the-lane edge; ~0 when weight is 0 (default)
        knob("quality_weight", |tuning| tuning.quality_weight = 0.0), // Δ prices the ROE/ROA quality reward. Absent until the term was un-blinded above — it read exactly 0.0 by construction, not by measurement. Needs `fund` (price-only lanes have no quality level -> ~0)
        // (#47) ablates TOWARD the ladder, not toward zero — the other rows here zero a weight, but this
        // knob's "off" IS the shipped state, so zeroing it would print a guaranteed 0.0 row. Δ is what
        // the graded 20/8/5 record ladder is worth against the single 10Y cliff.
        knob("growth_trust_ladder→on", |tuning| tuning.growth_trust_ladder = true),
        // (#48) the last two backtest-reachable growth terms that had no ablation row. `proximity` was
        // invisible because it had no knob to turn (see growth_proximity_weight); the growth lane's
        // dividend term shares `dividend_weight` with the on-sale lane, which had a row there and none
        // here. With these the growth ablation table is COMPLETE — every term that can move the number
        // now has a line pricing it.
        knob("proximity", |tuning| tuning.growth_proximity_weight = 0.0),
        // (#53) the `*` used to mean "reads 0.0 by construction, not by measurement" — dividends were
        // never plumbed into `backtest_quote`, so this row was decorative. They are now, so this Δ is a
        // real number and the dividend_weight CURVE below prices the whole slope.
        knob("growth_dividend", |tuning| tuning.dividend_weight = 0.0),
        // (#49) ablates TOWARD no-extra-dock, like the ladder row above and unlike the zeroing rows:
        // 0.7 is the 5Y rung, i.e. a young name treated exactly like a 5-year one. Δ is what docking
        // the 2Y/1Y rungs BELOW the 5Y one is worth. Reads 0.0 unless the loaded tuning has the ladder
        // on AND a floor below 5 — with neither, no young-rung name is scored at all.
        knob("growth_trust_young→0.7 (no extra dock)", |tuning| tuning.growth_trust_young = 0.7),
    ];
    // (G+) one row per CONFIGURED extra fundamental term, so each is priced separately rather than as
    // one all-extras-at-once Δ. That distinction is the whole point: receipt (#3d) found quality and
    // peg_yield each worth ~+85 alone and -100 together, and a combined ablation row could not have
    // told those apart. Empty by default -> the table is unchanged from the single-factor era.
    let mut growth_knobs = growth_knobs;
    for (i, t) in tuning.growth_fund_extra.iter().enumerate() {
        growth_knobs.push(knob(format!("fund_extra:{}", t.factor), move |tun| {
            tun.growth_fund_extra[i].weight = 0.0;
        }));
    }
    // (#59) NULL CALIBRATION — the SCALE for every row above, and the only row here that is guaranteed
    // to carry no information. It strips a flat 4.325 pts off `base` for every scored row: the factor
    // name matches no arm of `core::select_fund_factor`, so the lookup is None in EVERY mode (fund runs
    // included) and the term is always its `neutral` fill, never data.
    //
    // WHY A CONSTANT IS NOT A NO-OP, which is the whole point of the row. `base` is a SUM that is then
    // MULTIPLIED by trust × overext × proximity × value, so a constant c added inside it reaches the
    // score as c × multiplier and ranks by the multiplier stack. The identical argument holds the other
    // way for `liq_bonus`, which is added OUTSIDE the brake and IS rank-neutral — `picks.rs` states it
    // there and that statement is correct; it just does not transfer across the multiplication.
    //
    // HOW TO READ IT: any ablation above whose Δ is not clearly larger than this row's is a term whose
    // SIZE, not whose SIGNAL, the fixture is measuring. Placed last so it sits next to `fund_extra:roic`,
    // its exact twin: 4.325 is that term's shipped constant (0.25 × neutral 17.3), and with `quote.fund`
    // None on every offline row the two knobs remove the SAME quantity. So on any fund-less run these two
    // rows MUST print the same rho and the same Δedge — an equality the goldens pin. If they ever differ
    // offline, this row's premise is broken. Under a real `fund` run they SHOULD differ, and the gap is
    // roic finally speaking.
    growth_knobs.push(knob("null: base −4.3 (calibration)", |tun| {
        tun.growth_fund_extra.push(crate::config::FundTerm {
            factor: "__null__".into(),
            weight: -1.0,
            cap: 100.0,
            neutral: 4.325,
        });
    }));
    // (#60) THE WIDE FLOORS, measured 2026-08-09 on `backtest {20,12,8} universe` plus one `12 universe fund`,
    // pinned to tests/ci-settings.yaml, ~4997 tickers. The row above is only a ruler once you know how long it
    // is, and it is a DIFFERENT length at every horizon:
    //
    //   horizon    growth n   null Δedge   lane edge   90% band
    //   20y        481        -93.3        +459.5      [+207.4 … +700.1]  clears 0
    //   12y        668        -15.4        +132.0      [ +90.1 … +198.5]  clears 0
    //   8y         852        -16.0         +59.3      [ +24.6 …  +84.7]  clears 0
    //   12y fund   655        -37.3        +161.9      [+103.6 … +234.8]  clears 0
    //
    // TWO RESULTS THAT VALIDATE THE INSTRUMENT ITSELF. (a) `null` and `fund_extra:roic` print BIT-IDENTICAL Δs
    // on all three fund-less horizons at n=481/668/852 — the equality the comment above predicts, now confirmed
    // far off the fixture. (b) Under `fund` they finally SEPARATE: roic Δ-36.4 against null Δ-37.3. That 0.9 pts
    // is the entire informational content of roic on real data, exactly where the prediction above put it.
    //
    // WHAT THE FLOORS SAY. Read every wide ablation against its OWN horizon's null and one term survives
    // everywhere: `overext_brake`, at 3.4-5.5x the floor (Δ-419.4/-85.1/-54.9) and the only row that moves rho
    // (Δ-0.18/-0.13/-0.19 — strip the brake and rho collapses to ~0, so the brake IS the ranking). Everything
    // else sits at or under an information-free constant at every horizon: trend -103.6/+0.8/-14.3, accel
    // +46.3/-28.1/-0.1, smoothness -41.4/-6.4/-14.1, trust ladder -65.8/+3.9/-0.3, proximity -30.7/+22.4/+6.0,
    // dividend -8.1/+3.2/+4.3, sharpe +3.0/-4.3/-0.0. Six rows are bit-zero BY CONSTRUCTION, not by failure:
    // calmar and mom121 ship at 0.0, underwater is daily-cadence-gated in core.rs, and quality/fund only wake
    // under `fund` (Δ-7.3/+3.7 there, both far under that run's 37.3 floor).
    //
    // WHAT IT DOES NOT LICENCE, which matters more than what it does. A floor is a LANE-EDGE test, and this
    // repo's own receipts establish twice over that lane edge cannot SHIP a growth knob — growth_trend_weight
    // and growth_proximity_weight both demand a rank-1/head-to-head move and both refused a lane-edge case.
    // The symmetry is binding: evidence too weak to ship a term is too weak to KILL one, so "under the floor"
    // is NOT a deletion warrant. It says how much of the score's MAGNITUDE a term owns and nothing more;
    // everything under it is unresolved here, not disproved.
    //
    // AND THE FIXTURE CANNOT STAND IN FOR THIS. At n=80 the 12y fixture band is [-40.6 … +175.1], straddling 0,
    // with OOS decaying +0.35 -> +0.06; wide at n=668 the same lane reads [+90.1 … +198.5] and OOS IMPROVING
    // +0.09 -> +0.17. The cheap harness calls the growth lane noise because it cannot resolve it. Grade growth
    // changes on `universe`, and read the goldens for what they are: a pin on the arithmetic, not a verdict.
    // (#10) loosen each numeric growth GATE one notch, relative to the loaded tuning (respects settings.yaml
    // overrides). The sweep reports the mean forward return of the names each loosening newly admits.
    let gate_loosen: Vec<Knob> = picks::gate_notches().into_iter().map(|(_, label, f)| knob(label, f)).collect();
    report_lane("ON-SALE (buy_score)", &samples, buy_score, tuning, &buy_knobs, years);
    lap("lane ON-SALE");
    report_onsale_book(&samples, &bench, years, tuning);
    lap("on-sale book");
    report_lane("GROWTH (growth_score)", &samples, growth_score, tuning, &growth_knobs, years);
    if tuning.print_held_loser_bands {
        report_held_loser_bands(&samples, tuning); // (#1242)
    }
    if tuning.growth_event_months > 0 {
        report_event_flags(&samples, tuning.growth_event_months); // (#1245)
    }
    lap("lane GROWTH");
    // (#3g) the two levers that decide how hard a HIGH-CAGR name is rewarded: the slope and the ceiling.
    // Swept as curves because the ablation only prices removal and `tune` only reports a confounded
    // argmax — neither distinguishes a plateau (room to push) from a peak (already at the ceiling).
    weight_curve(
        "growth_trend_weight",
        &samples,
        tuning,
        |t, v| t.growth_trend_weight = v,
        tuning.growth_trend_weight,
        // 0 = the ablation's tilt-off control. 0.55 re-tests the recorded "0.35->0.55 cut edge +1.9->-0.6"
        // on THIS sample: that verdict comes from a ±2-edge scale, i.e. a far smaller/older run than the
        // one that now reads ±180. 1.0 is the ceiling `tune` already searches to.
        &[0.0, 0.15, 0.25, 0.35, 0.45, 0.55, 0.7, 1.0],
        "SLOPE: points of growth score per %/yr of capped long-leg CAGR.",
    );
    weight_curve(
        "long_trend_cap",
        &samples,
        tuning,
        |t, v| t.long_trend_cap = v,
        tuning.long_trend_cap,
        // 0 first as the OFF (uncapped) control; 30 is the SHIPPED state (#3k). 50 reproduces the recorded 30->50 pair
        // (edge +103.2 -> +87.7) so the old point-test is visible as part of a curve, not an isolated claim.
        &[0.0, 15.0, 20.0, 25.0, 30.0, 40.0, 50.0, 60.0],
        "CEILING (%/yr) on that CAGR — the binding constraint on the FASTEST compounders. 0 = OFF (uncapped).\n  \
         NOTE: this knob is SHARED with the on-sale lane (picks.rs, `long_trend_weight × capped_trend(..)`),\n  \
         so these rows price only its GROWTH-lane effect. That lane is the one `screen` ranks on; the on-sale\n  \
         side feeds `buy_score`, a backtest foil that is never printed — so a growth-lane read is the one\n  \
         that decides, and the foil only needs to not break.",
    );
    // (#48) the proximity slope. Swept as a curve rather than as arms because the ablation prices only
    // removal (w=0) and the question here is DIRECTION: whether the lane should keep paying up for names
    // near their high, pay more, or pay the opposite way. Negative rungs are in the ladder deliberately —
    // "further below the high should rank higher" is a real hypothesis with real evidence on BOTH sides
    // (market-level, entering on a benchmark drawdown beat entering at highs: +14.0%/yr vs +11.6%/yr,
    // same book; name-level, the range-gate ladder is monotone the other way: 90 +428.7 | 80 +410.8 |
    // 70 +381.8 | 60 +380.9), and the only way to settle it is to price the slope inside the gate the
    // ladder never moves. SOUNDNESS: weight_curve re-scores a set fixed at the shipped tuning, which is
    // valid only if the swept knob cannot change WHO is admitted — it cannot, because the range gate
    // compares `quote.range_pct` against `growth_min_range_pct` directly, before and independently of
    // this multiplier. TIE-BACK: the 0.0 row must reproduce the ablation's `proximity` row above, and
    // the 1.0 row the lane headline; if either disagrees the curve is measuring something else.
    // READ ONLY AS A SCREEN: this prints rho/edge/OOS and NOT rank-1 or head-to-head, so it can rank
    // candidate values but cannot settle a rank-1 ship rule. Confirm a winner with arms before shipping.
    weight_curve(
        "growth_proximity_weight",
        &samples,
        tuning,
        |t, v| t.growth_proximity_weight = v,
        tuning.growth_proximity_weight,
        &[-1.0, -0.5, 0.0, 0.5, 1.0, 1.5, 2.0, 3.0],
        "SLOPE of the distance-below-own-high dock: score ×(1 + w·(range_pct/100 − 1)).\n  \
         1.0 = SHIPPED (raw multiply: a name at 80% of range keeps 0.80 of its score, one at the high keeps 1.00).\n  \
         0.0 = term OFF (everyone ×1.00). NEGATIVE = INVERTED — the name furthest below its high scores HIGHEST\n  \
         (at −1.0 the 80%-of-range name is multiplied by 1.20). Clamped at 0, so no rung can produce a negative factor.",
    );
    // (#49) how hard to dock a name for a SHORT record. The (#47) ladder graded 20/8/5 and then stopped,
    // handing every shorter record the same flat 0.5 the old cliff gave a 9.9-year one — so "reward the
    // longer record" was never actually tested at the young end. This sweeps that bottom.
    // SOUNDNESS: weight_curve re-scores a set fixed at the loaded tuning, valid only if the swept knob
    // cannot change WHO is admitted. It cannot: `trust` is a pure score multiplier, and every gate in
    // `score_parts` — including the `history` bail at `long_leg_fixed(..)?` — returns before trust is
    // computed. The knob that DOES move admission is `growth_min_leg_years`, which is why the floor
    // moves in ARMS and only the dock is swept here.
    // FLAT LINE EXPECTED at the shipped tuning: with the ladder off and the floor at 5 there is no
    // young-rung name in the pool, so every row reads identically. That is the null, not a failure —
    // this curve is informative only inside a floor-2 or floor-1 arm.
    // TIE-BACK: the 0.7 row must reproduce the ablation's `growth_trust_young→0.7` row above, and the
    // row at the loaded value must reproduce the lane headline.
    weight_curve(
        "growth_trust_young",
        &samples,
        tuning,
        |t, v| t.growth_trust_young = v,
        tuning.growth_trust_young,
        &[0.7, 0.5, 0.4, 0.3, 0.2, 0.1],
        "TRUST multiplier for the ladder's bottom rungs: 2Y record ×w, 1Y record ×w/2. Lower = harsher dock.\n  \
         0.7 = the 5Y rung, i.e. NO extra dock for being young (the control).\n  \
         0.5 = DEFAULT, and the flat half-score the floor-2 arm already ran with AND LOST (rank-1 +6.0 → +4.0,\n  \
         h2h 67% → 39%) — it sits in this ladder as the reproduction of the known-bad point.\n  \
         READ ONLY AS A SCREEN: prints rho/edge/OOS, never rank-1 or h2h, so it cannot settle the ship rule.",
    );
    // (#53) the term that shipped BLIND. `dividend_weight` was sized by argument alone because the
    // receipt held that a walk-forward could never grade it — `backtest_quote` was said to be unable to
    // reconstruct as-of dividends. It never had to reconstruct them: `Chart.divs` arrives in the same
    // response and was being dropped at the fetch site. With it plumbed, this curve is the first
    // measurement the term has ever had.
    // SOUNDNESS: weight_curve re-scores a FIXED admitted set, which is only valid if the swept knob
    // cannot change WHO is admitted. It cannot — `dividend_reward` is a purely additive term applied
    // after every gate in `score_parts`, and the `min_score`/`growth_min_score` trims live in
    // `picks::ranked` (the LIVE screen path), not inside `growth_score`, which is
    // what the backtest calls.
    // WHAT A FLAT LINE MEANS HERE: that yield carries no SELECTION signal — NOT that the plumbing
    // failed. Check the `growth_dividend*` ablation row first: if that also reads 0.0 the term is still
    // inert and the plumbing is broken; if it reads non-zero and this curve is flat, that is a result.
    // TAX IS NOT GRADED BY THIS CURVE — it prices the WEIGHT, with the keep-rate held at whatever the
    // config ships. The split itself is graded by the on-sale lane's `tax_split ->off` ablation, which
    // is where enough EU payers survive the gates to resolve it. (This line used to blame a missing
    // `domicile`; nothing on this path ever read one — see `picks::is_eu_payer`.)
    weight_curve(
        "dividend_weight",
        &samples,
        tuning,
        |t, v| t.dividend_weight = v,
        tuning.dividend_weight,
        &[0.0, 0.25, 0.5, 1.0, 1.5, 3.0],
        "REWARD per % of trailing-1Y yield, after the PT keep-rate: score += w · min(yield, cap) · keep.\n  \
         0.5 = SHIPPED (revived 2026-07-25 on argument, never measured until now). 0.0 = term OFF, the\n  \
         2026-07-15 state. 1.5 = the ORIGINAL weight, cut for being oversized on a blind term.\n  \
         READ ONLY AS A SCREEN: prints rho/edge/OOS, never rank-1 or h2h, so it cannot settle the ship rule.",
    );
    lap("weight curves");
    let accepted_med = gate_audit(&samples, growth_score, tuning).map(|(_, _, amed)| amed); // (#9) are the growth lane's hard gates actually selecting winners?
    gate_sweep(&samples, tuning, &gate_loosen, accepted_med); // (#10) which specific gate is too tight?
    lap("gate audit + sweep");
    weight_sweep(&samples, tuning); // (#334) which shipped ranking weight is mis-set?
    lap("weight sweep");
    exit_probe(&samples, growth_score, tuning); // (Item 31) is a mid-hold gate FAILURE a measured sell signal?
    entry_probe(&samples, growth_score, tuning); // (#820) does a fresh pass lag a confirmed one?
    book_entry_probe(&samples, picks::growth_scores_ranked, tuning, &picks::gates_off(tuning)); // (#821) does a name new to the book lag one it already held?
    lap("exit + entry probes");
    if fund_lane_on(fund, insider) {
        report_fund_lane(&samples, tuning.split_purge_months);
        lap("fund lane");
        sweep_fund_factor(&samples, tuning, years); // (G) which factor pays THROUGH the growth lane, held-out
        lap("fund factor sweep");
    }
    if holders {
        report_holder_cohorts(&samples, &holder_txs, years, tuning);
        lap("holders");
    }
    if pac {
        report_pac(&client, &settings.urls, &samples, years, tuning).await;
        lap("pac");
    }
    if failures {
        report_failures(&client, &settings.urls, &samples).await;
        lap("failures");
    }
    report_risk_lane(&samples, tuning.split_purge_months); // closes-derived risk stats, standalone — no fundamentals needed
    lap("risk lane");
    // (#40) the ABSOLUTE goal metric: do the top-N picks beat an S&P500 buy-and-hold? One index fetch
    // (survivorship-clean), matched to the sample cadence. realized is untouched by de-mean.
    // (Phase E) with use_adjusted_close on, the picks' realized returns include dividends, so the fair
    // benchmark is the S&P 500 TOTAL-return index (^SP500TR, Yahoo history from 1988) — TR vs TR;
    // default (raw close) keeps the price-only ^GSPC, unchanged.
    // (#326) the fetch itself moved UP, to just after the walk: the regime valve needs the index series
    // before anything scores a sample. `bench` is still the same series, used by every report below.
    report_vs_benchmark(&samples, &bench, years, tuning);
    lap("vs benchmark");
    if rivals {
        report_rivals(&samples, &bench, years, tuning, draws);
        lap("rivals");
    }
    if winners {
        report_winners(&samples, &bench, years, tuning);
        lap("winners");
    }
    // (r40) relative strength vs the index — needs the benchmark, so it lives here, after the fetch.
    report_relative_strength(&samples, &bench, tuning.split_purge_months);
    lap("relative strength");
    println!("{}", purged_split_line(&samples, years));
    // (round 108) the WHEN dimension: does the market's state at entry predict the held book?
    let verdict = report_entry_state(&samples, &bench, years, tuning);
    lap("entry state");
    // (#1238) the level x path 2x2: record only, and fetched only when asked
    if level {
        let u = &settings.urls;
        let ep = fetch::fetch_shiller(&client, &u.shiller, crate::core::parse_shiller_ep).await;
        // TB3MS is a month's average, so a reader first has it the month after
        let bills: BTreeMap<i32, f64> = fetch::fetch_fred(&client, &u.fred, &u.sec_user_agent, "TB3MS").await.into_iter().map(|(k, v)| (k + 1, v)).collect();
        let out = level_path_report(&samples, &bench, years, tuning, &ep, &bills, -5.0);
        println!("\n{}", if out.is_empty() { "(#1238) level x path: no Shiller earnings or TB3MS, or no entry in a CHEAP or RICH class".into() } else { out });
        lap("level x path");
    }
    // (#324) the in-sample guard for the notch shadow `track` grades forward
    report_notch_books(&samples, &bench, years, tuning);
    report_proxy_books(&samples, &bench, years, tuning);
    report_sector_books(&samples, &bench, years, tuning);
    lap("notch + proxy + sector books");
    // (#325) the forward half, replayed on the entries no graded horizon closes (short runs only)
    report_notch_tail(&samples, years, tuning);
    lap("notch tail");
    // (#308) the indexes the spill's trackers follow, so the SIZED remainder earns what `size` buys with
    // it: all-world / developed / emerging per tier, the US tier on `bench` itself, weighted by
    // `size::spill_split`. Free Yahoo series; MSCI EM has no free history, so VEIEX (from 1994). Price-only,
    // so the total-return lane keeps the plain benchmark leg, as do the daily lane, every tier past
    // emerging, and any leg that fails to fetch. No legs at all prints the (#290) row unchanged.
    const SPILL_LEGS: [(&str, &str); 3] = [("^892400-USD-STRD", "MSCI ACWI"), ("^990100-USD-STRD", "MSCI World"), ("VEIEX", "VEIEX")];
    let mut spill_series = Vec::new();
    if monthly && !crate::config::use_adjusted_close() {
        for (sym, _) in SPILL_LEGS {
            spill_series.push(fetch::fetch_history_long(&client, &settings.urls, sym).await.map(|c| (c.dates, c.closes)));
        }
    }
    let spill_leg = |t: u8| spill_series.get(usize::from(t)).and_then(Option::as_ref);
    let tiers: Vec<u8> = (0..settings.sizing.spill_cut()).map(|i| if settings.sizing.spill_per_tier { i as u8 } else { 0 }).collect();
    let spill: Vec<(f64, &(Vec<chrono::NaiveDate>, Vec<f64>))> = if tiers.iter().any(|t| spill_leg(*t).is_some()) {
        let split = crate::commands::size::spill_split(1.0, &tiers.iter().map(|t| Some(*t)).collect::<Vec<_>>());
        split.into_iter().zip(&tiers).map(|(w, t)| (w, spill_leg(*t).unwrap_or(&bench))).collect()
    } else {
        Vec::new()
    };
    let sized = report_sized_book(&samples, &bench, years, tuning, &settings.sizing, settings.top_picks, &spill);
    lap("sized book");
    if !spill.is_empty() {
        let name = |t: u8| spill_leg(t).and(SPILL_LEGS.get(usize::from(t))).map_or(bench_sym, |l| l.1);
        let legs: Vec<String> = tiers.iter().zip(&spill).map(|(t, (w, _))| format!("{} {:.0}%", name(*t), w * 100.0)).collect();
        println!("  {:<28} (#308) the remainder above earns the spill's index legs, price-only: {}", "", legs.join(" · "));
    }
    let roll = report_roll(&samples, &books, &series, &bench, years, tuning, &settings.sizing, settings.top_picks, &spill, rivals, (dead_end, dead_penalty));
    lap("roll");
    if rolls {
        report_roll_ladder(&samples, &books, &series, &bench, years, tuning, &settings.sizing, settings.top_picks, &spill, (dead_end, dead_penalty));
        lap("roll ladder");
    }
    // (round 27) journal the unconditional method verdict — but ONLY from a wide (`universe`) run:
    // the watchlist's ~50-survivor sample is not the method's proof, and must never overwrite it.
    // The screen's method footer reads this file back.
    //
    // `wide` alone was not enough. A wide run that Yahoo THROTTLED resolves a few hundred names
    // instead of ~4900, and that thin sample overwrote the journal just as surely as a watchlist run
    // would have — the same hazard the comment above warns about, reached by a different route. Hold
    // it to the same ≥500 floor `backtest_edge_holds` uses to decide its own sample is trustworthy.
    // (#309) A wide PIT run, held to the same floor, journals to its own file instead, SIZED record and
    // all: that is the one `screen` cites under BUY NOW. The method footer's file never sees a PIT row.
    // (#788) And only with `dead`: without the delisted names that record is the survivor pool's.
    let journal = if may_write_verdict(wide, pit, tickers.len()) {
        Some(VERDICT_FILE)
    } else {
        if wide {
            // say so — a silent non-write reads as "the journal is broken", not "this sample was too thin"
            eprintln!("{}", no_verdict_reason(pit, tickers.len()));
        }
        if pit && !dead && may_write_verdict(wide, false, tickers.len()) {
            eprintln!("backtest: SIZED record NOT journaled — a `pit` run without `dead` scores survivors only; add `dead` to journal it");
        }
        if pit && dead && !fund && may_write_verdict(wide, false, tickers.len()) {
            eprintln!("backtest: SIZED record NOT journaled — a `pit dead` run without `fund` has no as-of P/E, so no PEG gate; add `fund` to journal it");
        }
        if pit && dead_flat && may_write_verdict(wide, false, tickers.len()) {
            eprintln!("backtest: SIZED record NOT journaled — `deadflat` is the no-crash-cut probe arm; run `dead` to journal it");
        }
        may_write_sized(wide, pit, dead, dead_flat, fund, tickers.len()).then_some(SIZED_FILE)
    };
    if let (Some(file), Some((book, excess, win, worst, oos_early, oos_late, windows))) = (journal, verdict) {
        write_verdict(
            file,
            Verdict {
                date: chrono::Local::now().date_naive().to_string(),
                years,
                top: VERDICT_TOP,
                windows,
                book,
                excess,
                win,
                worst,
                oos_early,
                oos_late,
                tuning_fp: tuning_fingerprint(tuning),
                sized: sized.filter(|_| pit).map(|s| SizedVerdict { roll, ..s }),
            },
        );
    }
    // (round 112) the DIVERSIFICATION dimension: does de-correlating the held book beat plain rank order?
    report_corr_cap(&samples, &bench, years, tuning);
    lap("corr cap");
    if fund_lane_on(fund, insider) {
        // (#44 Phase C) grade the FREE fundamental factors on the ABSOLUTE held-book, not peer-relative.
        report_book_by_factor(&samples, &bench, years, tuning);
        lap("book by factor");
        // (round 106) the no-borrow structural lever: growth book + value book held side by side.
        report_two_style_book(&samples, &bench, years, tuning);
        lap("two-style book");
    }

    println!("\nCaveats:");
    println!("  • Peer-relative (#1): returns are de-meaned per ~6mo cutoff, so rho is SELECTION vs same-period");
    println!("    peers (regime beta removed). A near-empty bucket has a weak peer set -> its rows count for less.");
    println!("  • In-sample: knobs were hand-tuned on today's data; even the OOS split shares the regime.");
    if stress {
        println!("  • Survivorship (#6 STRESS ON): crashed/delisted losers were INJECTED into the pool, so this");
        println!("    run partly corrects the upward bias. Compare its rho/edge to a plain run: a big drop = the");
        println!("    edge leaned on survivors; holding up (both OOS halves still +) = the edge is real.");
    } else {
        println!("  • Survivorship (#5): the universe is names that SURVIVED to today — dead tickers never enter,");
        println!("    so realized returns are biased UP. Treat the edge as optimistic. Re-run with `stress` to inject losers.");
    }
    // (#112 / round 3 §9) the other end of the survivorship story, and the one nobody names. `#5`/`#6`
    // are about holds that ENDED BADLY and never entered; this is about holds that ended WELL, early,
    // and not by choice. It sits here rather than beside the after-tax footer because it is the same
    // class of statement as the two lines above it — a reason the printed edge is optimistic by an
    // unmeasured amount — and because the after-tax footer is arithmetic, which this deliberately is not.
    if tuning.print_acquisition_hazard {
        println!("  • Acquisition truncation (#112, UNMEASURED): the most common way a 20-year single-stock hold");
        println!("    actually ENDS is neither bankruptcy nor a sell decision — it is being ACQUIRED. Compounding");
        println!("    stops at a one-time premium, and the deal CRYSTALLISES the gain: it pays, on a date the holder");
        println!("    never chose, the tax the deferral edge above assumes stays deferred. Nothing in this walk models");
        println!("    a takeout, so every multi-year hold in the tables is one that was ALLOWED to run — optimistic in");
        println!("    the same direction as the line above, by an amount this data cannot size. It argues mildly toward");
        println!("    LARGER names (harder to swallow), the reasoning `growth_min_aum_etf` already accepts for funds.");
    }
    if pit {
        println!("  • Point-in-time (PIT ON): every cutoff was scored against the S&P 500 AS IT STOOD THAT DAY, so a");
        println!("    name contributes no sample before it joined or after it left. This is the direct correction to");
        println!("    the line above, and the edge is EXPECTED to fall — a lower number here is the honest one.");
        println!("    {pit_missing} pool name(s) were index members Yahoo no longer serves: fetched nothing, scored nothing,");
        println!("    counted here rather than silently dropped. A big count means the correction is still incomplete.");
        // (#121) the miss count above is the SMALL half of the hole. Print the coverage ratio beside
        // it, because a name Yahoo answers with an empty stub is `served` and invisible to that count.
        let scored: HashSet<&str> = samples.iter().map(|s| s.quote.ticker.as_str()).collect();
        let (members, scoreable) = pit_coverage(&tickers, pit_spans, &scored);
        let pct = 100.0 * scoreable as f64 / members.max(1) as f64;
        println!("    COVERAGE (#121): {scoreable} of {members} index members in this pool produced a scoreable cutoff ({pct:.0}%).");
        println!("    READ THE TWO NUMBERS TOGETHER: the miss count above is what Yahoo refused to answer, this is what the");
        println!("    walk could actually score, and they are far apart because a dead name usually comes back as a payload");
        println!("    with ZERO BARS — which counts as SERVED. A 0 above with a low ratio here does not mean the PIT");
        println!("    correction is complete; it means the hole is not a fetch failure.");
        println!("    TWO DRIVERS, AND THIS LINE CANNOT SPLIT THEM. (1) MECHANICAL: a {years}y hold needs a full {years}y");
        println!("    forward window, so members that joined late are unscoreable at this horizon and NOT evidence of bias —");
        println!("    which is why the ratio rises as the horizon shortens. (2) SURVIVORSHIP: measured 2026-08-22 over the");
        println!("    734 members whose span carries an exit date — 387 served with ZERO BARS, 193 more with <36 in-span");
        println!("    bars, only 154 scoreable — and ZERO of all 1206 members with a series ending over a year ago. Only");
        println!("    (2) is bias, and it is the part that never shrinks with a shorter hold. So a name that DIED");
        println!("    contributes nothing here rather than a loss, and Bessembinder's -91.95% median delisted return is");
        println!("    absent from every number above. Closing it needs delisting DATES plus a terminal return, not a price");
        println!("    backfill: these prices provably do not exist to fetch.");
        println!("    COVERAGE FLOOR 1996-01-02: the source opens every pre-existing member's span on that date, so");
        println!("    cutoffs before it are DROPPED rather than corrected — it bites the 20y run's front, not the 8y one.");
        if dead {
            let n = dead_terminals.load(std::sync::atomic::Ordering::Relaxed);
            println!("    DEAD (#751): {n} of {} cutoffs paid a TERMINAL return, the one the line above asks for: a dead", samples.len());
            println!("    member's last close rolled into the S&P to the window's end, x{dead_penalty} if it lost over half its");
            println!("    price in its last year (one constant: the archive records no delist reason). Archive names carry no");
            println!("    fund rows (no SEC CIK map), so their growth fund term reads neutral; ^GSPC bars are quarterly.");
        }
    }
    // (#61) WAS "no as-of dividends or P/E reconstructed; the * term above is inert here", and the
    // dividend half of that was false — `#53` plumbed as-of divs (`picks.rs` says so at the growth
    // lane's own dividend term) and the row it called inert was the largest in the table at Δ+150.7.
    // A footnote claiming a term cannot be graded is the exact thing that stops anyone grading it.
    // (#147) The P/E half of that footnote has now gone the same way as the dividend half: a `fund`
    // run reconstructs an as-of P/E from the as-of earnings yield, so "no as-of P/E reconstructed" is
    // true only on the price-only lane. Same lesson as (#61) — name the lane the reader is on instead
    // of declaring a term ungradeable everywhere.
    if fund_lane_on(fund, insider) {
        println!("  • As-of P/E (#147): reconstructed here as 100 / the as-of earnings yield, so `value_factor` reads a REAL P/E on this lane (×1.0 only where EPS is missing). It reaches the on-sale `buy_score` at full authority; it reaches `growth_score` only through `growth_value_weight`, which ships at 0. as-of DIVIDENDS are live since #53 and every dividend row below is real.");
    } else {
        println!("  • Price-only (#6): no as-of P/E reconstructed, so the `value` multiplier is ×1.0 here; as-of DIVIDENDS are live since #53 and every dividend row below is real.");
    }
    println!("  • Overlapping 6-mo windows share price paths -> samples aren't independent; rho is directional.");
    if monthly {
        println!("  • Long-horizon (MAX monthly): only names alive for the FULL {years}y window enter, so");
        println!("    survivorship bias is WORSE than the daily path, and vol/MA are monthly-bar approximations.");
    }
}

/// One ticker's cutoff walk for the hold-period sweep: for each forward window in `holds`, step through
/// history and pair every cutoff with its realized forward return. Pure and separate from the async
/// fetch above it purely so it can be TESTED — `hold_period_sweep` is network + println only, so this
/// walk (the second of the two scoring loops in this file) had no coverage at all.
///
/// Note it de-means through the same `demean` as `run`, so it must class names the same way or the two
/// disagree on what a peer is: hence the shared `stamp_asset_class` rather than a local copy.
fn sweep_cutoffs(
    tk: &str,
    dates: &[chrono::NaiveDate],
    closes: &[f64],
    divs: &[(chrono::NaiveDate, f64)],
    name: &str,
    instrument_type: &str,
    holds: &[i64],
    min_history: usize,
    cadence: usize,
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    windows: &BTreeMap<String, i64>, // (#88) the live anchor map, or empty for the old default-only behaviour
) -> Vec<(i64, Sample)> {
    let mut out = Vec::new();
    // The cutoff's Quote does not depend on the hold window — only `realized` does — yet this used to
    // rebuild it from the raw series once per window, so every cutoff paid `backtest_quote` six times
    // and kept six copies of an identical 69-field struct. Memoised per cutoff index and shared by
    // `Arc`, it is computed once and pointed at six times. Indexed by `i` rather than keyed, because
    // the windows walk the same cutoffs in the same order and a flat `Vec` is the cheapest possible
    // lookup. Order is untouched: within a window this still emits ascending `i`, which is what the
    // stable `sort_by_key(date)` and `demean` downstream inherit.
    let mut quotes: Vec<Option<Arc<Quote>>> = vec![None; dates.len()];
    for &h in holds {
        for i in cutoffs(dates, min_history) {
            let target = dates[i] + chrono::Duration::days(h * 365);
            match dates[i..].iter().position(|d| *d >= target) {
                Some(off) => {
                    let realized = (closes[i + off] / closes[i] - 1.0) * 100.0;
                    // a zero/garbage close makes realized ±inf; one poisoned cutoff drags the
                    // whole demeaned bucket to -inf (short holds reach data the 12y path never walks)
                    if realized.is_finite() {
                        let quote = quotes[i]
                            .get_or_insert_with(|| {
                                let mut q = core::backtest_quote(tk, dates, closes, divs, i, cadence, windows);
                                stamp_asset_class(&mut q, name, instrument_type, etf_set, sector_of);
                                Arc::new(q)
                            })
                            .clone();
                        out.push((h, Sample { date: dates[i], realized, relative: 0.0, quote, fund: None, trail: Vec::new() }));
                    }
                }
                None => break,
            }
        }
    }
    out
}

/// (Item 11) Hold-period / signal half-life sweep. Re-runs the price-only walk-forward over several
/// forward windows from the SAME fetched history (fetched once per ticker, then sliced per window), and
/// prints each window's gross edge, turnover, and NET edge (gross − turnover×ROUND_TRIP_BPS). The right
/// rebalance cadence is the one that maximises NET — longer holds pay less turnover but may catch less
/// signal, so there's an optimum. shortcut: duplicates ~20 lines of run's walk-forward on purpose, to keep
/// this opt-in dev path from touching the validated default dispatch; the fetch is cached so the re-walk is
/// cheap. Price-only (no fund/insider) — this measures the price signal's decay, not the fund tilt.
async fn hold_period_sweep(
    client: &reqwest::Client,
    urls: &crate::config::Urls,
    tickers: &[String],
    monthly: bool,
    cadence: usize,
    min_history: usize,
    tuning: &BuyHeuristic,
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    windows: &BTreeMap<String, i64>, // (#88) resolved once in `run`, so both quote-building paths agree
) {
    const HOLDS: [i64; 6] = [1, 2, 3, 5, 8, 10]; // forward windows (years) to compare
    eprintln!("backtest: hold-period sweep over {HOLDS:?}y windows ({} tickers)…", tickers.len());
    // FETCH FIRST, WALK SECOND, for the same reason the validated path does it (see the long note
    // there): this stream is polled by ONE task, so leaving `sweep_cutoffs` inside it ran six forward
    // windows per ticker on one thread of eight. rayon's `collect` preserves its input order, so `demean`
    // and the pooled stats read the order `fetched` holds.
    let mut fetched: Vec<_> = stream::iter(tickers.iter())
        .map(|tk| async move {
            // (FX) price-only sweep — no filing joined, no currency needed, so a monthly payload can
            // stay in its bytes until the walk. (#46) the class fields ARE needed: this walk de-means
            // through the same `demean`, so it has to split classes the same way the validated path
            // does or the two disagree on what a peer is.
            let hist = if monthly {
                fetch::chart_json_long(client, urls, tk).await.map(Hist::Raw)
            } else {
                fetch::fetch_history(client, urls, tk).await.map(Hist::Parsed)
            };
            hist.map(|h| (tk, h))
        })
        .buffer_unordered(fetch::fetch_concurrency())
        .collect()
        .await;
    // (#1015) `buffer_unordered` yields completion order, which live is fetch latency: the (#342) sort the
    // validated path got never reached this one. `tests/backtest_fixture.rs` runs a Z-A pool against the
    // halflife golden.
    fetched.sort_by_key(|f| f.as_ref().map(|x| x.0));
    let per_ticker: Vec<Vec<(i64, Sample)>> = fetched
        .into_par_iter()
        .flatten()
        .map(|(tk, hist)| {
            let chart = match hist.parse(tk) {
                Some(c) => c,
                None => return Vec::new(),
            };
            sweep_cutoffs(
                tk, &chart.dates, &chart.closes, &chart.divs, &chart.name, &chart.instrument_type, &HOLDS,
                min_history, cadence, etf_set, sector_of, windows,
            )
        })
        .collect();
    // Split ONCE, by move. This used to build one combined `Vec` of every (window, sample) pair and
    // then, per window, scan the whole thing again and CLONE its share back out — six full passes and a
    // second copy of the data, on top of the combined vec the original was still holding. Pushing into
    // per-window buckets as the pairs arrive costs one pass and no clone; a window's samples keep the
    // order they had in the combined vec, which is what the stable sort below inherits.
    let mut by_hold: Vec<Vec<Sample>> = vec![Vec::new(); HOLDS.len()];
    for (w, smp) in per_ticker.into_iter().flatten() {
        if let Some(slot) = HOLDS.iter().position(|&h| h == w) {
            by_hold[slot].push(smp);
        }
    }

    // (#65) NULL CALIBRATION, the same construction and the same 4.325 as the ablation table's row —
    // see the long (#59) note there for why a CONSTANT inside `base` is not a no-op. Without it these
    // rows cannot be read: (#60) fixed the rule that a Δ no larger than this carries size, not signal,
    // and this sweep is the one report that shipped with no ruler at all. It matters most across
    // CONFIGS, because a gate change moves the scored POOL, so each arm has to be read against its own
    // floor rather than against the other arm.
    let null_tuning = {
        let mut t = tuning.clone();
        t.growth_fund_extra.push(crate::config::FundTerm {
            factor: "__null__".into(),
            weight: -1.0,
            cap: 100.0,
            neutral: 4.325,
        });
        t
    };

    println!("\n── HOLD-PERIOD SWEEP (growth lane, net of cost) ──");
    println!("  pick the hold with the highest NET edge; if they're flat, the longest (cheapest) wins.");
    println!("  `null` is the information-free floor for THAT hold — an edge no bigger is not a signal.");
    for (slot, &h) in HOLDS.iter().enumerate() {
        // own bucket (de-mean is per-window: a 1y and a 5y forward over the same cutoff aren't comparable)
        let mut s: Vec<Sample> = std::mem::take(&mut by_hold[slot]);
        if s.len() < 4 {
            println!("  {h}y hold  — only {} cutoffs, too few to read", s.len());
            continue;
        }
        s.sort_by_key(|x| x.date);
        demean(&mut s);
        let scored: Vec<(&Sample, f64)> =
            s.iter().filter_map(|x| growth_score(&x.quote, tuning).map(|v| (x, v))).collect();
        if scored.len() < 4 {
            println!("  {h}y hold  — only {} gated, too few to read", scored.len());
            continue;
        }
        let (t, b) = edge_halves(&scored);
        let edge = t - b;
        let turn = turnover_frac(&scored);
        // (#96) `h`, not the run's horizon: this sweep's whole point is that each row holds for a
        // DIFFERENT span, so the number of rebalances inside the hold differs per row too.
        let net = edge - cost_pts(turn, h, tuning.cost_per_rebalance);
        // same samples, same halves, only the score changes — so this reads as edge, in the same units
        let null_scored: Vec<(&Sample, f64)> =
            s.iter().filter_map(|x| growth_score(&x.quote, &null_tuning).map(|v| (x, v))).collect();
        let (nt, nb) = edge_halves(&null_scored);
        println!(
            "  {h}y hold  edge {edge:+.1}  turnover {:.0}%  net {net:+.1} pts   null {:+.1}  n={}",
            turn * 100.0,
            nt - nb,
            scored.len()
        );
    }
}

/// (#518) One side of a `holders` cohort row: n, then mean and median excess (`Sample::relative`, the
/// forward return minus its cutoff's peer mean) and hit rate (excess > 0, in percent). NaN when empty.
#[derive(Debug, Default, PartialEq)]
struct CohortSide {
    n: usize,
    mean: f64,
    median: f64,
    hit: f64,
}

fn cohort_side(xs: Vec<f64>) -> CohortSide {
    let n = xs.len();
    let mean = xs.iter().sum::<f64>() / n as f64;
    let hit = xs.iter().filter(|x| **x > 0.0).count() as f64 * 100.0 / n as f64;
    CohortSide { n, mean, median: median(xs), hit }
}

/// (#518) The cohorts `backtest … holders` grades, each a test on [`core::holder_flags`]. The shipped
/// superinvestor boost starts at the 8 buyers a stock needs to list (`fetch::SOC_MIN_BUYERS`); 3 is the
/// boost's step, graded as the lower bar. (#614) The `W` boost reads Berkshire HOLDING a name in its
/// newest 13F: the last row replays it from the [`core::HOLDER_BRK_HELD`] records.
const HOLDER_COHORTS: [(&str, fn(&core::HolderFlags) -> bool); 5] = [
    ("Berkshire added (13F or Form 4)", |f| f.brk_add),
    ("3+ superinvestors added", |f| f.adders >= 3),
    ("8+ added (the shipped listing rule)", |f| f.adders >= 8),
    ("net 3+ (adders minus trimmers)", |f| f.net >= 3),
    ("Berkshire holds (newest 13F, the W)", |f| f.brk_held),
];

/// (#518) Every [`HOLDER_COHORTS`] row as (name, the stock samples flagged at their cutoff, the stock
/// samples not). Stocks only: no ETF or coin is ever a 13F move here.
fn holder_cohorts(samples: &[Sample], txs: &HashMap<String, Vec<core::HolderTx>>) -> Vec<(&'static str, CohortSide, CohortSide)> {
    let stocks: Vec<(core::HolderFlags, f64)> = samples
        .iter()
        .filter(|s| picks::asset_class(&s.quote) == 2)
        .map(|s| (txs.get(&s.quote.ticker).map_or_else(Default::default, |t| core::holder_flags(t, s.date)), s.relative))
        .collect();
    HOLDER_COHORTS
        .iter()
        .map(|(name, on)| {
            let (a, b): (Vec<_>, Vec<_>) = stocks.iter().partition(|(f, _)| on(f));
            (*name, cohort_side(a.iter().map(|x| x.1).collect()), cohort_side(b.iter().map(|x| x.1).collect()))
        })
        .collect()
}

/// (#1219) The leverage census: the S&P 500's daily closes since 1928, ^SP500TR's from 1988 and the
/// Shiller dividend yield before it, at 1.25x to 3x against 1x over every 20y and 30y window, borrowing
/// at the NBER short-term yield until 1934 and the 3-month bill rate (TB3MS) after; then (#1237) the trend
/// census on the same series. Every source is fetched before any is read, so an offline run names each
/// one it lacks.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::rules
async fn leverage_census(client: &reqwest::Client, settings: &config::Settings) {
    use crate::heuristics::{holds, rules};
    use chrono::NaiveDate;
    let d = |y, m| NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let u = &settings.urls;
    let px = fetch::fetch_history_since(client, u, "^GSPC", d(1927, 12)).await;
    let tr = fetch::fetch_history_since(client, u, "^SP500TR", d(1988, 1)).await;
    let yld = fetch::fetch_shiller(client, &u.shiller, crate::core::parse_shiller_yield).await;
    let mut bills = fetch::fetch_fred(client, &u.fred, &u.sec_user_agent, "M1329AUSM193NNBR").await;
    let tb = fetch::fetch_fred(client, &u.fred, &u.sec_user_agent, "TB3MS").await;
    let gone = [("^GSPC", px.is_none()), ("^SP500TR", tr.is_none()), ("Shiller dividends", yld.is_empty()), ("NBER yields", bills.is_empty()), ("TB3MS", tb.is_empty())];
    let missing: Vec<&str> = gone.iter().filter(|g| g.1).map(|g| g.0).collect();
    let (Some(px), Some(tr), true) = (px, tr, missing.is_empty()) else {
        println!("(#1219) leverage census: {} unavailable", missing.join(", "));
        return;
    };
    bills.extend(tb); // TB3MS wins the 1934 overlap
    let pair = |c: &fetch::Chart| c.dates.iter().copied().zip(c.closes.iter().copied()).collect::<Vec<_>>();
    let (px, tr) = (pair(&px), pair(&tr));
    let dates: Vec<NaiveDate> = px.iter().map(|x| x.0).collect();
    let rs = rules::splice(&px, &yld, &tr);
    let cash: Vec<f64> = dates.iter().map(|&x| rules::on_month(&bills, x) / 100.0).collect();
    // the splice check: price plus the Shiller yield against ^SP500TR over the years both cover
    let from = dates.partition_point(|x| *x <= tr[0].0);
    let years = (dates[dates.len() - 1] - dates[from - 1]).num_days() as f64 / 365.25;
    let pa = |rs: &[f64]| ((rs[from..].iter().map(|r| r.ln_1p()).sum::<f64>() / years).exp() - 1.0) * 100.0;
    let (got, want) = (pa(&rules::splice(&px, &yld, &[])), pa(&rs));
    println!("(#1219) splice check from {}: price plus Shiller dividends {got:+.2}%/yr, ^SP500TR {want:+.2}%/yr, gap {:+.2} pts", dates[from - 1], got - want);
    println!("{}", rules::lever_report(&dates, &rs, &cash, &[20, 30], d(1967, 1)));
    // (#1237) an S&P line on a euro broker: one round trip's cost
    println!("{}", rules::trend_report(&dates, &rs, &cash, holds::cost_bps("etf", "EUR") / 1e4, &[20, 30], d(1967, 1)));
}

/// (#1226) The option-selling census: the Cboe S&P 500 BuyWrite (^BXM, covered calls) and PutWrite (^PUT)
/// indexes against ^SP500TR on each index's own dates over every 20y window. BXM comes from Cboe's
/// daily file (from 2002-03), Yahoo's ^BXM being monthly with gaps. Every source is fetched before any
/// is read, so an offline run names each one it lacks.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::rules
async fn option_census(client: &reqwest::Client, settings: &config::Settings) {
    use crate::heuristics::rules;
    use chrono::NaiveDate;
    let u = &settings.urls;
    let from = NaiveDate::from_ymd_opt(1988, 1, 1).unwrap();
    let tr = fetch::fetch_history_since(client, u, "^SP500TR", from).await;
    let bxm = fetch::fetch_cboe(client, &u.cboe, "BXM").await;
    let put = fetch::fetch_history_since(client, u, "^PUT", from).await;
    let gone = [("^SP500TR", tr.is_none()), ("Cboe BXM", bxm.is_empty()), ("^PUT", put.is_none())];
    let missing: Vec<&str> = gone.iter().filter(|g| g.1).map(|g| g.0).collect();
    let (Some(tr), Some(put), false) = (tr, put, bxm.is_empty()) else {
        println!("(#1226) option census: {} unavailable", missing.join(", "));
        return;
    };
    let pair = |c: &fetch::Chart| c.dates.iter().copied().zip(c.closes.iter().copied()).collect::<Vec<_>>();
    let tr = pair(&tr);
    let bxm: Vec<(NaiveDate, f64)> = bxm.into_iter().filter_map(|(d, v)| Some((NaiveDate::from_num_days_from_ce_opt(d)?, v))).collect();
    for (name, c) in [("^BXM", bxm), ("^PUT", pair(&put))] {
        let px: Vec<(NaiveDate, f64)> = c.into_iter().filter(|x| x.0 >= tr[0].0).collect();
        let dates: Vec<NaiveDate> = px.iter().map(|x| x.0).collect();
        let (sp, rs) = (rules::splice(&px, &BTreeMap::new(), &tr), rules::splice(&px, &BTreeMap::new(), &[]));
        // the whole-span check: Cboe published 1988-06 to 2007-05 as BXM +11.86, PUT +12.65, SPTR +12.30%/yr
        let years = (dates[dates.len() - 1] - dates[0]).num_days() as f64 / 365.25;
        let pa = |rs: &[f64]| ((rs.iter().map(|r| r.ln_1p()).sum::<f64>() / years).exp() - 1.0) * 100.0;
        println!("(#1226) {name} from {} to {}: {:+.2}%/yr, ^SP500TR {:+.2}%/yr on its dates", dates[0], dates[dates.len() - 1], pa(&rs), pa(&sp));
        println!("{}", rules::option_report(name, &dates, &sp, &rs, &[20]));
    }
}

/// (#1220) The lifecycle census: the (#1219) S&P 500 total return and bill rate from 1928, the CPI
/// (CPIAUCNS) and the 10y yield (Shiller's until GS10 starts in 1953), for savers aged 23 to 64 at 200/100
/// and 200/50 against fixed shares and the 90/50 glide. Every source is fetched before any is read, so
/// an offline run names each one it lacks.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::rules
async fn lifecycle_census(client: &reqwest::Client, settings: &config::Settings) {
    use crate::heuristics::rules;
    use chrono::NaiveDate;
    let d = |y, m| NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let u = &settings.urls;
    let px = fetch::fetch_history_since(client, u, "^GSPC", d(1927, 12)).await;
    let tr = fetch::fetch_history_since(client, u, "^SP500TR", d(1988, 1)).await;
    let yld = fetch::fetch_shiller(client, &u.shiller, crate::core::parse_shiller_yield).await;
    let mut long = fetch::fetch_shiller(client, &u.shiller, crate::core::parse_shiller_long).await;
    let mut bills = fetch::fetch_fred(client, &u.fred, &u.sec_user_agent, "M1329AUSM193NNBR").await;
    let tb = fetch::fetch_fred(client, &u.fred, &u.sec_user_agent, "TB3MS").await;
    let cpi = fetch::fetch_fred(client, &u.fred, &u.sec_user_agent, "CPIAUCNS").await;
    let gs10 = fetch::fetch_fred(client, &u.fred, &u.sec_user_agent, "GS10").await;
    let gone = [
        ("^GSPC", px.is_none()),
        ("^SP500TR", tr.is_none()),
        ("Shiller dividends", yld.is_empty()),
        ("Shiller 10y yields", long.is_empty()),
        ("NBER yields", bills.is_empty()),
        ("TB3MS", tb.is_empty()),
        ("CPIAUCNS", cpi.is_empty()),
        ("GS10", gs10.is_empty()),
    ];
    let missing: Vec<&str> = gone.iter().filter(|g| g.1).map(|g| g.0).collect();
    let (Some(px), Some(tr), true) = (px, tr, missing.is_empty()) else {
        println!("(#1220) lifecycle census: {} unavailable", missing.join(", "));
        return;
    };
    bills.extend(tb); // TB3MS wins the 1934 overlap
    long.extend(gs10); // GS10 wins from 1953-04
    let pair = |c: &fetch::Chart| c.dates.iter().copied().zip(c.closes.iter().copied()).collect::<Vec<_>>();
    let (px, tr) = (pair(&px), pair(&tr));
    let dates: Vec<NaiveDate> = px.iter().map(|x| x.0).collect();
    let rs = rules::splice(&px, &yld, &tr);
    let cash: Vec<f64> = dates.iter().map(|&x| rules::on_month(&bills, x) / 100.0).collect();
    let k = dates.partition_point(|x| *x < d(1928, 1));
    println!("{}", rules::lifecycle_report(&dates[k..], &rs[k..], &cash[k..], &cpi, &long, d(1956, 5)));
}

/// (#1222) `backtest N bestpick`: the Trading table's BEST, picked on each name's last 2y and traded over the
/// next 6 months or month, against holding the same name, in euros at each day's EUR/USD rate. The (#809)
/// ponds on 10y of daily bars, at the page's round trip per market.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::intraday
async fn best_census(client: &reqwest::Client, settings: &config::Settings) {
    use crate::heuristics::intraday;
    let today = chrono::Utc::now().date_naive();
    let usd = fetch::fx_factor_series(client, &settings.urls, "USD", "EUR", false).await;
    if usd.is_empty() {
        println!("(#1222) best census: EUR/USD history unavailable");
        return;
    }
    let spans = fetch::fetch_sp500_history(client, &settings.urls).await;
    let us = intraday::us_pond(&spans, today - chrono::Duration::days(3653));
    let eu: Vec<String> = intraday::EU_POND.iter().map(|t| t.to_string()).collect();
    println!("\n── (#1222) BEST, PICKED ON 2y AND TRADED THE NEXT STEP, AGAINST HOLDING THE SAME NAME ──");
    println!("  Each name picks the most euros of SAME DAY, HOLD 21, TREND and BUY&HOLD on its last 2y, €1,000 a trade");
    println!("  after the page's round trip, then trades the pick over the step. CAVEATS: a fresh €1,000 a trade, so");
    println!("  SAME DAY puts €1,000 on every session where BUY&HOLD puts it on once, as the page does; the EU pond is");
    println!("  today's members and Yahoo keeps few dead US names, so both lean to survivors; halves split by step count.");
    let start = today - chrono::Months::new(84);
    for (market, names, cost) in [("US", &us, intraday::COST_BPS_US), ("EU", &eu, intraday::COST_BPS_EU)] {
        let mut charts: Vec<(&str, fetch::Chart)> = stream::iter(names)
            .map(|tk| async move { Some((tk.as_str(), fetch::fetch_history(client, &settings.urls, tk).await?)) })
            .buffer_unordered(fetch::fetch_concurrency())
            .filter_map(std::future::ready)
            .collect()
            .await;
        // the means add up in pond order, so fetch completion order cannot move a digit
        charts.sort_by(|a, b| a.0.cmp(b.0));
        let fx: Vec<Option<Vec<f64>>> = charts.iter().map(|(_, c)| intraday::rates_on(&c.currency, &c.dates, &usd)).collect();
        let pond: Vec<intraday::Priced> = charts
            .iter()
            .zip(&fx)
            .filter_map(|((tk, c), fx)| Some((*tk, c, fx.as_deref()?, (market == "US").then(|| spans[*tk].as_slice()))))
            .collect();
        println!("  {market} pond: {} of {} names fetched in EUR or USD; cost {cost} bp round trip", pond.len(), names.len());
        for months in [6, 1] {
            println!("{}", intraday::best_report(market, months, &intraday::best_steps(&pond, cost, months, start, today)));
        }
    }
}

/// (#809) `backtest N intraday`: every intraday key graded on 10y of daily bars, per market, against the
/// bar pre-registered in tests/ci-settings.yaml. The US pond is every S&P 500 member since the start,
/// ranked only on its member days; the EU pond is the static `EU_POND`. A name in neither EUR nor USD is
/// skipped, and one USD rate (today's) converts every US session's turnover.
///
/// (#830) A third market, `US*`, re-grades the same US charts with only today's members, each ranked
/// as a member every day: the way `EU_POND` is built. A key's US* minus US hit diff is what a static
/// pond adds. Pre-registered: an EU hit pass stands only if its diff, less that key's addition, still
/// clears t 3 at the EU line's own noise (diff x 3 / t).
///
/// (#1212) The hours census last: today's members and `EU_POND` on 2y of 60m bars. (#1214) Each window's mean
/// move, from a buy at the open or a bar end to a sell at a later bar end, the census the Intraday table's
/// hour knobs are picked from. (#1215) Then the holds census: the same bars, and the 10y daily ones, held
/// from the buy session to up to 21 sessions on, every name and the `intraday_key` book. (#1216) Last, the
/// calendar census: the same-session hour pairs split by weekday, month turn, pre-holiday and expiry, each
/// slice checked against the average day on the 10y daily bars.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::intraday
async fn intraday_ruler(client: &reqwest::Client, settings: &config::Settings) {
    use crate::heuristics::intraday;
    let today = chrono::Utc::now().date_naive();
    let spans = fetch::fetch_sp500_history(client, &settings.urls).await;
    let us = intraday::us_pond(&spans, today - chrono::Duration::days(3653));
    let Some(usd) = fetch::eur_rate(client, &settings.urls, "USD", &fetch::fx_cache()).await else {
        eprintln!("backtest intraday: no USD->EUR rate, nothing graded");
        return;
    };
    let eu: Vec<String> = intraday::EU_POND.iter().map(|t| t.to_string()).collect();
    println!(
        "\n── (#809) INTRADAY: buy at the open, sell at the close, top {} equal weight, net of cost ──",
        intraday::BOOK
    );
    // (#1215) per market, the book the holds census grades on 60m bars and its daily lines; (#1216) and the
    // calendar census's daily lines with each slice's check
    let mut held = Vec::new();
    // the walk sorts each session by key then ticker, so fetch completion order cannot move a number
    for (market, names, cost) in [("US", &us, intraday::COST_BPS_US), ("EU", &eu, intraday::COST_BPS_EU)] {
        let charts: Vec<(&str, fetch::Chart)> = stream::iter(names)
            .map(|tk| async move { Some((tk.as_str(), fetch::fetch_history(client, &settings.urls, tk).await?)) })
            .buffer_unordered(fetch::fetch_concurrency())
            .filter_map(std::future::ready)
            .collect()
            .await;
        let pond: Vec<intraday::Member> = charts
            .iter()
            .filter_map(|(tk, c)| {
                let fx = intraday::rate_for(&c.currency, Some(usd))?;
                Some((*tk, c, fx, (market == "US").then(|| spans[*tk].as_slice())))
            })
            .collect();
        println!("  {market} pond: {} of {} names fetched in EUR or USD; cost {cost} bp round trip", pond.len(), names.len());
        for k in 1..=intraday::KEYS.len() as u8 {
            println!("{}", intraday::report(market, k, &intraday::walk(k, &pond, today), cost));
        }
        // today's members, each ranked every day: US*, and (#1215) the pond the holds census's book is ranked on
        let now: Vec<intraday::Member> = pond
            .iter()
            .filter(|m| m.3.is_none_or(|s| core::sp500_member_at(s, today)))
            .map(|&(tk, c, fx, _)| (tk, c, fx, None))
            .collect();
        if market == "US" {
            println!("  US* pond: {} of today's members, each ranked every day (survivor-biased, as EU)", now.len());
            for k in 1..=intraday::KEYS.len() as u8 {
                println!("{}", intraday::report("US*", k, &intraday::walk(k, &now, today), cost));
            }
        }
        let k = settings.intraday_key;
        held.push((
            intraday::books(k, &now, today),
            intraday::daily_holds(market, k, &pond, today, cost, &intraday::HOLDS),
            intraday::daily_slices(market, k, &pond, today, cost),
        ));
    }
    // (#1214) the census the Intraday table's hour knobs are picked from: today's members, as on the page
    println!("\n── (#1214) BUY AND SELL HOURS: buy at the open or a 60m bar's end, sell at a later one, every name equal weight, 2y ──");
    let (mut holds, mut cals) = (Vec::new(), Vec::new());
    for ((market, names, cost, exit), (books, daily, (by_day, beat))) in
        [("US", intraday::us_pond(&spans, today), intraday::COST_BPS_US, settings.intraday_exit_us), ("EU", eu, intraday::COST_BPS_EU, settings.intraday_exit_eu)]
            .into_iter()
            .zip(&held)
    {
        let knob = (config::session(market).0, exit);
        let mut got: Vec<(&str, _, _)> = stream::iter(&names)
            .map(|tk| async move {
                let bars = fetch::fetch_hourly(client, &settings.urls, tk, "2y").await?;
                Some((tk.as_str(), intraday::day_moves(market, &bars), intraday::day_prices(market, &bars)))
            })
            .buffer_unordered(fetch::fetch_concurrency())
            .filter_map(std::future::ready)
            .collect()
            .await;
        // the means add up in ticker order, so fetch completion order cannot move a digit
        got.sort_by(|a, b| a.0.cmp(b.0));
        let (days, prices): (Vec<_>, Vec<_>) = got.into_iter().map(|(tk, m, p)| (m, (tk, p))).unzip();
        println!("{}", intraday::hours_report(market, &days, today, cost, knob));
        holds.push(format!("{}\n{daily}", intraday::holds_report(market, &prices, books, today, cost, &intraday::HOLDS)));
        cals.push(format!("{by_day}\n{}", intraday::calendar_report(market, &prices, books, today, cost, beat)));
    }
    println!("\n── (#1215) HOLDS: buy on one session, sell on it or H sessions on, every name equal weight and the intraday_key book, one round trip ──");
    println!("  CAVEATS: a long hold mostly earns the market's drift, which the whole-span line prices; the best of up");
    println!("  to 64 (US) or 100 (EU) hour pairs wins partly by luck, so read it beside open-close; the 60m side is");
    println!("  today's members, so survivor-biased; holds start every H sessions, one phase, and halves split by count;");
    println!("  the daily lines run open to close on 10y of bars, the US side on the index as it stood each day.");
    for h in holds {
        println!("{h}");
    }
    println!("\n── (#1216) CALENDAR: buy and sell in the same session, split by weekday, month turn, pre-holiday and expiry, every name equal weight and the intraday_key book ──");
    println!("  CAVEATS: 28 (US) or 45 (EU) hour pairs in each of 9 slices and 2 name sets, about 1,300 in all, so some");
    println!("  clear t ≥ 3 by luck, which the halves and the daily check are there to catch; the 60m side is today's");
    println!("  members, so survivor-biased; a half day drops out of the 60m sessions, so the one before it reads as");
    println!("  pre-holiday; the daily lines run open to close on 10y of bars, the US side on the index as it stood each day.");
    for c in cals {
        println!("{c}");
    }
}

/// (#1217) A set's index spans: Some keeps only a name's member days.
type HoldSpans<'a> = Option<&'a [(chrono::NaiveDate, Option<chrono::NaiveDate>)]>;

/// (#1217) A chart's class (None for a leveraged fund), its round trip as a fraction, and the sets it
/// trades in: its class if the universe holds it, the S&P 500 point-in-time on its member days, and the
/// S&P 500 today if it is a member now. (#1218) The trading-rules census places names the same way.
#[mutants::skip] // shell piece of hold_windows and trade_rules
fn hold_place<'a>(
    tk: &str,
    c: &fetch::Chart,
    universe: &HashSet<&str>,
    spans: &'a core::MemberSpans,
    today: chrono::NaiveDate,
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
) -> (Option<&'static str>, f64, Vec<(String, HoldSpans<'a>)>) {
    use crate::heuristics::holds;
    let mut q = Quote::stub(tk, "", "", "");
    stamp_asset_class(&mut q, &c.name, &c.instrument_type, etf_set, sector_of);
    q.sector = sector_of.get(tk).cloned();
    let class = holds::class_of(&q);
    let mut sets: Vec<(String, HoldSpans<'_>)> = class.filter(|_| universe.contains(tk)).map(|c| (c.to_string(), None)).into_iter().collect();
    if let Some(sp) = spans.get(tk) {
        sets.push(("S&P 500 point-in-time".to_string(), Some(sp.as_slice())));
        if core::sp500_member_at(sp, today) {
            sets.push(("S&P 500 today".to_string(), None));
        }
    }
    (class, holds::cost_bps(class.unwrap_or("stock"), &c.currency) / 1e4, sets)
}

/// (#1217) Drops the bars from `cut` on: today's daily bar and this month's monthly one are still trading.
#[mutants::skip] // shell piece of hold_windows and trade_rules
fn hold_trim(c: &mut fetch::Chart, cut: chrono::NaiveDate) {
    let n = c.dates.partition_point(|d| *d < cut);
    c.dates.truncate(n);
    c.opens.truncate(n);
    c.closes.truncate(n);
}

/// (#1217) The screen's growth score of a class name at each half-year cutoff, as the walk scores it:
/// (bucket, score, bar).
#[mutants::skip] // shell piece of hold_windows and trade_rules
fn hold_scores(
    tk: &str,
    c: &fetch::Chart,
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    tuning: &BuyHeuristic,
    windows: &BTreeMap<String, i64>,
) -> Vec<(i32, f64, usize)> {
    cutoffs(&c.dates, 36)
        .filter_map(|i| {
            let mut q = core::backtest_quote(tk, &c.dates, &c.closes, &c.divs, i, 12, windows);
            stamp_asset_class(&mut q, &c.name, &c.instrument_type, etf_set, sector_of);
            Some((bucket(c.dates[i]), growth_score(&q, tuning)?, i))
        })
        .collect()
}

/// (#1217) The book: per class and half-year, the top [`crate::heuristics::holds::BOOK`] of each name's
/// [`hold_scores`], ties in name order, as (class, name index, bar), by class then half-year then score.
#[mutants::skip] // shell piece of hold_windows and trade_rules
fn hold_book<'a>(named: impl Iterator<Item = (Option<&'static str>, &'a [(i32, f64, usize)])>) -> Vec<(&'static str, usize, usize)> {
    let mut scored: BTreeMap<(&str, i32), Vec<(f64, usize, usize)>> = BTreeMap::new();
    for (n, (class, scores)) in named.enumerate() {
        for &(b, v, i) in scores {
            if let Some(c) = class {
                scored.entry((c, b)).or_default().push((v, n, i));
            }
        }
    }
    let mut out = Vec::new();
    for ((class, _), mut v) in scored {
        v.sort_by(|a, b| b.0.total_cmp(&a.0));
        out.extend(v.into_iter().take(crate::heuristics::holds::BOOK).map(|(_, n, i)| (class, n, i)));
    }
    out
}

/// (#1217) `backtest N holdwindows`: buy, hold a calendar span, sell. Every hold in [`crate::heuristics::holds::HOLDS`] per
/// asset class, net of one round trip: each class's names equal weight on 10y of daily bars (1d to 6m,
/// bought and sold at the open or the close) and on the max monthly bars (1m to 20y, close to close); the
/// S&P 500 as it stood on each buy date against today's members, whose gap is the survivor add; the
/// screen book, the top [`crate::heuristics::holds::BOOK`] names per class by growth score at each half-year cutoff, on the
/// monthly bars; then the (#1215) 60m holds census with stocks and REIT stocks apart. A census: no knob,
/// lane or trade reads it.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::holds
async fn hold_windows(
    client: &reqwest::Client,
    settings: &config::Settings,
    tickers: &[String],
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    windows: &BTreeMap<String, i64>,
) {
    use crate::heuristics::{holds, intraday};
    // per (set, grid, buy-sell, hold index), every buy date's trades
    type Accs = BTreeMap<(String, &'static str, &'static str, usize), holds::Acc>;
    const COMBOS: [(&str, bool, bool); 4] = [("O-O", true, true), ("O-C", true, false), ("C-O", false, true), ("C-C", false, false)];
    fn fold(acc: &mut Accs, sets: &[(String, HoldSpans<'_>)], s: &holds::Series, grid: &'static str, combos: &[(&'static str, bool, bool)], hs: std::ops::Range<usize>, cost: f64) {
        for &(combo, buy_open, sell_open) in combos {
            for (h, &hold) in holds::HOLDS.iter().enumerate().take(hs.end).skip(hs.start) {
                let all = holds::trades(s, buy_open, sell_open, hold);
                for (set, spans) in sets {
                    let kept: Vec<holds::Trade> = all.iter().copied().filter(|t| spans.is_none_or(|sp| core::sp500_member_at(sp, t.0))).collect();
                    holds::add(acc.entry((set.clone(), grid, combo, h)).or_default(), &kept, cost);
                }
            }
        }
    }
    core::set_measure_cadence(12);
    let (tuning, hurdle, adjusted) = (&settings.buy_heuristic, core::serie_e_best_pct(), config::use_adjusted_close());
    let today = chrono::Utc::now().date_naive();
    let spans = fetch::fetch_sp500_history(client, &settings.urls).await;
    let universe: HashSet<&str> = tickers.iter().map(String::as_str).collect();
    let mut pool: Vec<&str> = universe.iter().copied().chain(spans.keys().map(String::as_str)).collect();
    pool.sort_unstable();
    pool.dedup();
    let place = |tk: &str, c: &fetch::Chart| hold_place(tk, c, &universe, &spans, today, etf_set, sector_of);
    let charts: Vec<(&str, fetch::Chart)> = stream::iter(&pool)
        .map(|&tk| async move { Some((tk, fetch::fetch_history_long(client, &settings.urls, tk).await?)) })
        .buffered(fetch::fetch_concurrency())
        .filter_map(std::future::ready)
        .collect()
        .await;
    let named: Vec<_> = charts
        .into_par_iter()
        .map(|(tk, mut c)| {
            hold_trim(&mut c, today.with_day(1).unwrap_or(today));
            let (class, cost, sets) = place(tk, &c);
            let divs: &[(chrono::NaiveDate, f64)] = if adjusted { &[] } else { &c.divs };
            let s = holds::series(&c.dates, &c.opens, &c.closes, divs, true);
            // the screen scores a class name at each half-year cutoff, as the walk does
            let scores = if class.is_some() && universe.contains(tk) {
                hold_scores(tk, &c, etf_set, sector_of, tuning, windows)
            } else {
                Vec::new()
            };
            (class, cost, sets, s, scores)
        })
        .collect();
    // sums add up in ticker order, so neither fetch order nor thread count can move a digit
    let mut acc = Accs::new();
    for (_, cost, sets, s, _) in &named {
        fold(&mut acc, sets, s, "monthly", &COMBOS[3..], 3..holds::HOLDS.len(), *cost);
    }
    // the book, each name bought at its cutoff's close
    for (class, n, i) in hold_book(named.iter().map(|x| (x.0, x.4.as_slice()))) {
        let (_, cost, _, s, _) = &named[n];
        for (h, &hold) in holds::HOLDS.iter().enumerate().skip(3) {
            holds::add(acc.entry((format!("{class} book"), "monthly", "C-C", h)).or_default(), holds::trade(s, false, false, i, hold).as_slice(), *cost);
        }
    }
    // the daily pass folds each 10y chart as it lands, in ticker order, so none is held
    let mut daily = std::pin::pin!(stream::iter(&pool)
        .map(|&tk| async move { (tk, fetch::fetch_history(client, &settings.urls, tk).await) })
        .buffered(fetch::fetch_concurrency()));
    let mut fetched = 0;
    while let Some((tk, c)) = daily.next().await {
        let Some(mut c) = c else { continue };
        fetched += 1;
        hold_trim(&mut c, today);
        let (_, cost, sets) = place(tk, &c);
        let divs: &[(chrono::NaiveDate, f64)] = if adjusted { &[] } else { &c.divs };
        fold(&mut acc, &sets, &holds::series(&c.dates, &c.opens, &c.closes, divs, false), "daily", &COMBOS, 0..6, cost);
    }
    let lines = |set: &str, with_daily: bool| {
        let mut out = Vec::new();
        for (grid, combos, hs) in [("daily", &COMBOS[..], 0..6), ("monthly", &COMBOS[3..], 3..holds::HOLDS.len())] {
            if grid == "daily" && !with_daily {
                continue;
            }
            for &(combo, ..) in combos {
                for (h, &hold) in holds::HOLDS.iter().enumerate().take(hs.end).skip(hs.start) {
                    let a = acc.get(&(set.to_string(), grid, combo, h));
                    out.push(holds::line(&format!("{set} {grid} {combo}"), a.unwrap_or(&holds::Acc::new()), hold));
                }
            }
        }
        out
    };
    println!("\n── (#1217) HOLD WINDOWS: buy, hold a calendar span, sell; each asset class's names equal weight, net of one round trip ──");
    println!(
        "  {fetched} of {} names on 10y daily bars (1d to 6m, bought and sold at the open O or the close C), {} on max monthly bars (1m to 20y, close to close)",
        pool.len(),
        named.len()
    );
    println!("  CAVEATS: every class is today's universe under today's labels, so its dead names are absent and it reads");
    println!("  high; the S&P 500 point-in-time set holds every member on its member days, and the survivor add is what");
    println!("  today's members gain over it. Total return, each dividend reinvested. Cost a round trip: crypto 30 bp, a");
    println!("  euro line 20 bp, any other 50 bp. A rate compounds the mean log return of each buy date's basket over its");
    println!("  mean days held; t and halves read starts one hold apart. 1m, 3m and 6m print on both grids, over different");
    println!("  spans. Série E pays {hurdle:.1}%/yr.");
    if adjusted {
        println!("  use_adjusted_close is on: the closes already carry dividends, so none is added; opens stay raw, so O lines mix the two.");
    }
    for set in holds::CLASSES.into_iter().chain(["S&P 500 point-in-time", "S&P 500 today"]) {
        let ls = lines(set, true);
        for l in &ls {
            println!("{}", l.text);
        }
        println!("{}", holds::verdict(set, &ls, hurdle));
    }
    let (pit, now) = (lines("S&P 500 point-in-time", true), lines("S&P 500 today", true));
    for grid in ["daily", "monthly"] {
        let gaps: Vec<String> = pit
            .iter()
            .zip(&now)
            .filter(|(p, _)| p.label.ends_with(&format!("{grid} C-C")))
            .map(|(p, n)| match (p.net, n.net) {
                (Some(a), Some(b)) => format!("{} {:+.1}", p.hold.0, b - a),
                _ => format!("{} n/a", p.hold.0),
            })
            .collect();
        println!("  S&P 500 survivor add, {grid} C-C, today's members minus point-in-time: {} pts/yr", gaps.join(", "));
    }
    println!(
        "\n── (#1217) THE SCREEN BOOK: the top {} names per class by growth score at each half-year cutoff, bought at that close, monthly bars ──",
        holds::BOOK
    );
    println!("  CAVEATS: today's universe, so survivor-biased as above; each class's book ranks only that class's names.");
    for class in holds::CLASSES {
        let set = format!("{class} book");
        if !acc.keys().any(|k| k.0 == set) {
            println!("  {set}: screen never picks this class");
            continue;
        }
        let ls = lines(&set, false);
        for l in &ls {
            println!("{}", l.text);
        }
        println!("{}", holds::verdict(&set, &ls, hurdle));
    }
    println!("\n── (#1217) HOLD WINDOWS ON 60m BARS: the (#1215) holds census, stocks and REIT stocks apart, every name equal weight, 2y, raw prices ──");
    for (market, names, cost) in [
        ("US", intraday::us_pond(&spans, today), intraday::COST_BPS_US),
        ("EU", intraday::EU_POND.iter().map(|t| t.to_string()).collect(), intraday::COST_BPS_EU),
    ] {
        let mut got: Vec<(&str, intraday::Prices)> = stream::iter(&names)
            .map(|tk| async move { Some((tk.as_str(), intraday::day_prices(market, &fetch::fetch_hourly(client, &settings.urls, tk, "2y").await?))) })
            .buffer_unordered(fetch::fetch_concurrency())
            .filter_map(std::future::ready)
            .collect()
            .await;
        // the means add up in ticker order, so fetch completion order cannot move a digit
        got.sort_by(|a, b| a.0.cmp(b.0));
        let (reits, stocks): (Vec<_>, Vec<_>) = got.into_iter().partition(|(tk, _)| sector_of.get(*tk).is_some_and(|s| s.contains("Real Estate")));
        for (group, names) in [("stock", stocks), ("REIT stock", reits)] {
            println!("  60m {market} {group}:");
            // no book here, so its lines all read "no two holds"
            for l in intraday::holds_report(market, &names, &BTreeMap::new(), today, cost, &intraday::HOLDS).lines().filter(|l| !l.contains(" book sell day")) {
                println!("{l}");
            }
        }
    }
}

/// (#1218) `backtest N traderules`: the trading-rules census. Every rule in
/// [`crate::heuristics::rules::sweep`] at 1x, 2x and 3x on 10y of daily bars, the trend rules on the max
/// monthly bars and the swing rules on 2y of 60m bars, each against buy-and-hold 1x on the same names and
/// days, net of cost, for each class, the S&P 500 as it stood and as it stands, the (#1217) screen book and
/// the 60m ponds; then how many names ran 10x and 100x inside 5 years, and how much of each run the rules
/// kept. A census: no knob, lane or trade reads it.
#[mutants::skip] // network shell; the arithmetic and its tests live in heuristics::rules
async fn trade_rules(
    client: &reqwest::Client,
    settings: &config::Settings,
    tickers: &[String],
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    windows: &BTreeMap<String, i64>,
) {
    use crate::heuristics::{holds, intraday, rules};
    use chrono::{Duration, NaiveDate};
    // per (grid, set, setting × leverage), the set's tally, its ruined names and its names
    type Tallies = BTreeMap<(&'static str, String, usize), (rules::Tally, usize, usize)>;
    // each setting at each leverage, in order: setting 0 at 1x is buy-and-hold, the bar every line reads
    fn run(grid: &[rules::Rule], levs: &[u32], dates: &[NaiveDate], opens: &[f64], closes: &[f64], cost: f64, cash: &[f64]) -> Vec<Vec<f64>> {
        grid.par_iter()
            .flat_map_iter(|&r| {
                let pos = rules::positions(r, dates, closes);
                levs.iter().map(move |&l| rules::returns(dates, opens, closes, &pos, l, cost, cash)).collect::<Vec<_>>()
            })
            .collect()
    }
    fn fold(acc: &mut Tallies, grid: &'static str, base: NaiveDate, sets: &[(String, HoldSpans<'_>)], dates: &[NaiveDate], rss: &[Vec<f64>], from: usize) {
        for (set, spans) in sets {
            let member = |d: NaiveDate| spans.is_none_or(|sp| core::sp500_member_at(sp, d));
            for (c, rs) in rss.iter().enumerate() {
                let e = acc.entry((grid, set.clone(), c)).or_default();
                if rules::add(&mut e.0, base, dates, rs, member, from) > 0 {
                    e.2 += 1;
                    let mine: Vec<f64> = (from..rs.len()).filter(|&i| member(dates[i])).map(|i| rs[i]).collect();
                    e.1 += usize::from(rules::ruined(&mine));
                }
            }
        }
    }
    // each bar's cash is the bill rate known at the bar before it
    fn prev(ds: &[NaiveDate]) -> Vec<NaiveDate> {
        ds.first().into_iter().chain(ds).take(ds.len()).copied().collect()
    }
    core::set_measure_cadence(12);
    let (tuning, adjusted) = (&settings.buy_heuristic, config::use_adjusted_close());
    let today = chrono::Utc::now().date_naive();
    let spans = fetch::fetch_sp500_history(client, &settings.urls).await;
    let universe: HashSet<&str> = tickers.iter().map(String::as_str).collect();
    let mut pool: Vec<&str> = universe.iter().copied().chain(spans.keys().map(String::as_str)).collect();
    pool.sort_unstable();
    pool.dedup();
    let place = |tk: &str, c: &fetch::Chart| hold_place(tk, c, &universe, &spans, today, etf_set, sector_of);
    let finite = |c: &fetch::Chart| -> Vec<(NaiveDate, f64)> { c.dates.iter().copied().zip(c.closes.iter().copied()).filter(|x| x.1.is_finite()).collect() };
    let irx_d = fetch::fetch_history(client, &settings.urls, "^IRX").await.map(|c| finite(&c)).unwrap_or_default();
    let irx_m: Vec<(NaiveDate, f64)> = fetch::fetch_history_long(client, &settings.urls, "^IRX")
        .await
        .map(|c| finite(&c).into_iter().map(|(d, x)| (holds::after(d.with_day(1).unwrap_or(d), ("", 0, 1)) - Duration::days(1), x)).collect())
        .unwrap_or_default();
    let (base_m, base_d, base_h) = (NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default(), today - Duration::days(11 * 366), today - Duration::days(1096));
    let (monthly, sweep, swing) = (rules::monthly(), rules::sweep(), rules::swing());
    let mut acc = Tallies::new();
    // the monthly pass: the trend rules, the screen book and the 10x census, on the max monthly bars
    let charts: Vec<(&str, fetch::Chart)> = stream::iter(&pool)
        .map(|&tk| async move { Some((tk, fetch::fetch_history_long(client, &settings.urls, tk).await?)) })
        .buffered(fetch::fetch_concurrency())
        .filter_map(std::future::ready)
        .collect()
        .await;
    let named: Vec<_> = charts
        .into_par_iter()
        .map(|(tk, mut c)| {
            hold_trim(&mut c, today.with_day(1).unwrap_or(today));
            let (class, cost, sets) = place(tk, &c);
            let divs: &[(NaiveDate, f64)] = if adjusted { &[] } else { &c.divs };
            let s = holds::series(&c.dates, &c.opens, &c.closes, divs, true);
            let scores = if class.is_some() && universe.contains(tk) { hold_scores(tk, &c, etf_set, sector_of, tuning, windows) } else { Vec::new() };
            // a monthly bar is dated the last day its close covers
            let ends: Vec<NaiveDate> = s.close_at.iter().map(|d| *d - Duration::days(1)).collect();
            let rss = run(&monthly, &[1], &ends, &s.opens, &s.closes, cost, &rules::cash_on(&irx_m, &prev(&ends)));
            (tk, class, sets, s, scores, ends, rss)
        })
        .collect();
    // the book: each pick held 6 months from its cutoff's close
    let mut book_of: BTreeMap<usize, (String, Vec<(NaiveDate, Option<NaiveDate>)>)> = BTreeMap::new();
    for (class, n, i) in hold_book(named.iter().map(|x| (x.1, x.4.as_slice()))) {
        let at = named[n].3.close_at[i];
        book_of.entry(n).or_insert_with(|| (format!("{class} book"), Vec::new())).1.push((at, Some(holds::after(at, ("", 0, 6)))));
    }
    // sums add up in ticker order, so neither fetch order nor thread count can move a digit; per set, its
    // names and their best 5-year runs of 10x or more
    let mut census: BTreeMap<String, (usize, Vec<(&str, NaiveDate, NaiveDate, f64)>)> = BTreeMap::new();
    let mut runs_of: HashMap<(&str, String), (NaiveDate, NaiveDate)> = HashMap::new();
    for (n, (tk, _, place_sets, s, _, ends, rss)) in named.iter().enumerate() {
        let mut sets = place_sets.clone();
        if let Some((b, sp)) = book_of.get(&n) {
            sets.push((b.clone(), Some(sp.as_slice())));
        }
        fold(&mut acc, "monthly", base_m, &sets, ends, rss, 12);
        for (set, sp) in &sets {
            let e = census.entry(set.clone()).or_default();
            e.0 += 1;
            let best = rules::best_run(&s.closes, 60, |i| sp.is_none_or(|x| core::sp500_member_at(x, s.close_at[i])));
            if let Some((i, j, m)) = best.filter(|r| r.2 >= 10.0) {
                e.1.push((*tk, s.close_at[i], s.close_at[j], m));
                runs_of.insert((*tk, set.clone()), (s.close_at[i], s.close_at[j]));
            }
        }
    }
    // the daily pass folds each 10y chart as it lands, in ticker order, so none is held; per set, each
    // run's kept share at every setting × leverage
    let at_of: HashMap<&str, usize> = named.iter().enumerate().map(|(n, x)| (x.0, n)).collect();
    let mut kepts: BTreeMap<String, Vec<Vec<f64>>> = BTreeMap::new();
    let mut daily = std::pin::pin!(stream::iter(&pool)
        .map(|&tk| async move { (tk, fetch::fetch_history(client, &settings.urls, tk).await) })
        .buffered(fetch::fetch_concurrency()));
    let mut fetched = 0;
    while let Some((tk, c)) = daily.next().await {
        let Some(mut c) = c else { continue };
        fetched += 1;
        hold_trim(&mut c, today);
        let (_, cost, mut sets) = place(tk, &c);
        if let Some((b, sp)) = at_of.get(tk).and_then(|n| book_of.get(n)) {
            sets.push((b.clone(), Some(sp.as_slice())));
        }
        let divs: &[(NaiveDate, f64)] = if adjusted { &[] } else { &c.divs };
        let s = holds::series(&c.dates, &c.opens, &c.closes, divs, false);
        let ds = &s.close_at;
        let rss = run(&sweep, &rules::LEVERAGES, ds, &s.opens, &s.closes, cost, &rules::cash_on(&irx_d, &prev(ds)));
        fold(&mut acc, "daily", base_d, &sets, ds, &rss, 252);
        for (set, _) in &sets {
            let Some(&(start, end)) = runs_of.get(&(tk, set.clone())) else { continue };
            // bought at the close before `start`, sold at the last close before `end`
            let (from, to) = (ds.partition_point(|d| *d < start), ds.partition_point(|d| *d < end));
            if from >= 252 && from < to && (to < ds.len() || ds[to - 1] + Duration::days(7) >= end) {
                kepts.entry(set.clone()).or_default().push(rss.iter().map(|rs| rules::kept(rs, &rss[0], from, to - 1)).collect());
            }
        }
    }
    // the 60m pass: the swing rules inside the session, every pond name equal weight, raw prices
    let mut ponds = Vec::new();
    for (set, names, cost) in [
        ("US pond", intraday::us_pond(&spans, today), intraday::COST_BPS_US),
        ("EU pond", intraday::EU_POND.iter().map(|t| t.to_string()).collect(), intraday::COST_BPS_EU),
    ] {
        let market = &set[..2];
        let mut got: Vec<(&str, intraday::Prices)> = stream::iter(&names)
            .map(|tk| async move { Some((tk.as_str(), intraday::day_prices(market, &fetch::fetch_hourly(client, &settings.urls, tk, "2y").await?))) })
            .buffer_unordered(fetch::fetch_concurrency())
            .filter_map(std::future::ready)
            .collect()
            .await;
        // sums add up in ticker order, so fetch completion order cannot move a digit
        got.sort_by(|a, b| a.0.cmp(b.0));
        let sets: [(String, HoldSpans<'_>); 1] = [(set.to_string(), None)];
        for (_, p) in &got {
            // every session's open, then each bar's close, one price an instant
            let (dates, prices): (Vec<NaiveDate>, Vec<f64>) = p.iter().flat_map(|(d, b)| b.values().map(move |x| (*d, *x))).unzip();
            let from: usize = p.values().take(5).map(BTreeMap::len).sum();
            let rss = run(&swing, &rules::LEVERAGES, &dates, &prices, &prices, cost / 1e4, &rules::cash_on(&irx_d, &prev(&dates)));
            fold(&mut acc, "60m", base_h, &sets, &dates, &rss, from);
        }
        ponds.push(format!("{set} {} of {}", got.len(), names.len()));
    }
    let grids: [(&str, &[rules::Rule], &[u32], NaiveDate); 3] =
        [("daily", &sweep, &rules::LEVERAGES, base_d), ("monthly", &monthly, &[1], base_m), ("60m", &swing, &rules::LEVERAGES, base_h)];
    // a set's lines on a grid, in setting × leverage order, `None` when no name reached it
    let scores = |grid: &str, set: &str| -> Option<Vec<(rules::Rule, u32, rules::Score)>> {
        let &(g, rs, levs, base) = grids.iter().find(|x| x.0 == grid)?;
        let hold = &acc.get(&(g, set.to_string(), 0)).filter(|e| e.2 > 0)?.0;
        let empty = (rules::Tally::new(), 0, 0);
        Some(
            (0..rs.len() * levs.len())
                .map(|c| {
                    let (r, l) = (rs[c / levs.len()], levs[c % levs.len()]);
                    let (t, ruins, names) = acc.get(&(g, set.to_string(), c)).unwrap_or(&empty);
                    (r, l, rules::score(&format!("{set} {grid} {} {l}x", r.label()), t, hold, base, *ruins, *names))
                })
                .collect(),
        )
    };
    let mut sets: Vec<String> = holds::CLASSES.iter().map(|c| c.to_string()).collect();
    sets.extend(["S&P 500 point-in-time".to_string(), "S&P 500 today".to_string()]);
    sets.extend(holds::CLASSES.iter().map(|c| format!("{c} book")));
    let (mut tried, mut cleared, mut out, mut picks) = (0, 0, Vec::new(), BTreeMap::new());
    for (set, grid) in sets.iter().flat_map(|s| [(s.as_str(), "daily"), (s.as_str(), "monthly")]).chain([("US pond", "60m"), ("EU pond", "60m")]) {
        let Some(sc) = scores(grid, set) else {
            out.push(format!("  {set} {grid}: no names"));
            continue;
        };
        tried += sc.len();
        cleared += sc.iter().filter(|s| s.2.clears()).count();
        out.push(rules::report(&format!("{set} {grid}"), &sc));
        if grid == "daily" {
            picks.insert(set.to_string(), sc);
        }
    }
    println!("\n── (#1218) TRADING RULES: trend, breakout and swing rules at 1x, 2x and 3x against buy-and-hold on the same names and days, net of cost ──");
    println!(
        "  {fetched} of {} names on 10y daily bars ({} settings × {} leverages), {} on max monthly bars ({} settings at 1x), on 2y 60m bars {} ({} settings × {} leverages)",
        pool.len(),
        sweep.len(),
        rules::LEVERAGES.len(),
        named.len(),
        monthly.len(),
        ponds.join(", "),
        swing.len(),
        rules::LEVERAGES.len()
    );
    println!("  CAVEATS: every class is today's universe, so its dead names are absent and it reads high; the S&P 500");
    println!("  point-in-time set holds each member on its member days only, and the book each pick for 6 months from");
    println!("  its cutoff's close. {tried} lines are tried, so a few clear t ≥ 3 by luck alone. A rule reads each close and");
    println!("  trades at the next open (on 60m bars, at the next bar's price); each switch pays half a round trip on");
    println!("  the levered size: crypto 30 bp, a euro line 20 bp, any other 50 bp; on 60m bars US {:.0} bp, EU {:.0} bp.", intraday::COST_BPS_US, intraday::COST_BPS_EU);
    println!("  Out of the market a name earns the 13-week bill rate (^IRX, 0 before its history); above 1x it borrows");
    println!("  at that rate plus {:.1} pts. The stop's volatility unit is the mean close move over 10 bars, not an ATR;", rules::SPREAD * 100.0);
    println!("  \"all-time\" is the highest close since the chart starts. Each day is one equal-weight basket; t and halves");
    println!("  read its monthly log excess over buy-and-hold 1x; ruined counts the names whose own equity fell 90%.");
    println!("  Total return, each dividend reinvested, except on 60m bars. Every line starts after a warm-up: 252 daily");
    println!("  bars, 12 monthly, 5 sessions on 60m.");
    if adjusted {
        println!("  use_adjusted_close is on: the closes already carry dividends, so none is added; opens stay raw, so entry and exit bars mix the two.");
    }
    for (bars, irx) in [("daily and 60m", &irx_d), ("monthly", &irx_m)] {
        if irx.is_empty() {
            println!("  no ^IRX history on the {bars} bars: out of the market a name earns 0 there");
        }
    }
    for l in &out {
        println!("{l}");
    }
    println!("  in all: {cleared} of {tried} lines clear the bar");
    println!("\n── (#1218) 10x AND 100x RUNS: names whose total return ran 10x or more inside 5 years, and how much of each run a rule kept ──");
    println!("  CAVEATS: a run is a name's best 5-year multiple on the max monthly bars, found with hindsight, from a");
    println!("  month's close the name was in the set (a member, or a book pick). Kept is the share of buy-and-hold 1x's");
    println!("  log gain over the run a rule kept, read on the daily bars, so only runs that start a year or more into");
    println!("  the 10y chart count; buy-and-hold 1x keeps 100%, and a levered rule can keep more. Each family reads the");
    println!("  setting its daily line above printed; the median is over the set's runs.");
    for set in &sets {
        let Some((names, runs)) = census.get(set) else {
            println!("  {set}: no names");
            continue;
        };
        let big: Vec<_> = runs.iter().filter(|r| r.3 >= 100.0).collect();
        println!("  {set}: {} of {names} names ran 10x or more inside 5 years, {} ran 100x or more", runs.len(), big.len());
        if !big.is_empty() {
            println!("    100x: {}", big.iter().map(|r| format!("{} {:.0}x {}-{}", r.0, r.3, r.1.year(), r.2.year())).collect::<Vec<_>>().join(", "));
        }
        let (Some(ks), Some(sc)) = (kepts.get(set), picks.get(set)) else {
            if !runs.is_empty() {
                println!("    no run inside the daily bars");
            }
            continue;
        };
        let med = |c: usize| core::median(ks.iter().map(|k| k[c]).filter(|x| x.is_finite()).collect()).map_or("n/a".to_string(), |m| format!("{:.0}%", m * 100.0));
        let mut parts = vec![format!("buy-and-hold 2x {} 3x {}", med(1), med(2))];
        for family in ["trend", "average", "breakout", "swing"] {
            let at: Vec<String> = rules::LEVERAGES
                .iter()
                .filter_map(|&l| {
                    let of = sc.iter().enumerate().filter(|(_, s)| s.0.family() == family && s.1 == l);
                    let c = of.filter_map(|(c, s)| Some((c, s.2.excess?))).max_by(|a, b| a.1.total_cmp(&b.1))?.0;
                    Some(format!("{l}x {}", med(c)))
                })
                .collect();
            parts.push(format!("{family} {}", at.join(" ")));
        }
        println!("    median kept over {} runs on the daily bars: {}", ks.len(), parts.join(", "));
    }
}

/// (#518) The `holders` report: [`holder_cohorts`] printed with its caveats. Report only, no knob reads it.
#[mutants::skip] // printing only; `holder_cohorts` carries the test
fn report_holder_cohorts(samples: &[Sample], txs: &HashMap<String, Vec<core::HolderTx>>, years: i64, tuning: &BuyHeuristic) {
    println!("\n── (#518) HOLDERS: do Berkshire's and the superinvestors' adds pick winners? ({years}y forward, stocks) ──");
    println!("  excess = forward return minus the same-cutoff stock peer mean; hit = excess > 0. Each cohort is");
    println!("  read against the stock samples outside it. A sample is flagged by the moves FILED in the 135 days");
    println!("  before its cutoff, each manager's newest one counting: no look-ahead. {} tickers carry moves.", txs.len());
    println!("  CAVEATS: (#737) 13F before 2013-Q2 is read from its text table, kept only when its rows add up to");
    println!("  the cover's value total (none printed: no row-shaped line left unread), else the quarter folds into");
    println!("  the next; the 83 managers were picked in 2026, so survivor bias flatters their rows; cutoffs overlap,");
    println!("  so n counts samples, not independent bets. (#794) CUSIPs are named by today's OpenFIGI map, and one");
    println!("  it cannot name (mostly a dead issuer) is dropped, so its moves flag no stock: the `fetch: holders:`");
    println!("  line prints asked against named. Read mean AND hit: a hit rate up with the mean down dilutes.");
    print_cohorts(&holder_cohorts(samples, txs));
    // (#544) The boosts only multiply names the gates already pass, so the row that grades a boost is
    // flagged passers against the OTHER passers, not against every stock. (#614) Since `growth_gate_soft`
    // a docked failer scores too and the boosts multiply it as well, so both crowds print.
    let passed: Vec<Sample> = samples.iter().filter(|s| growth_score(&s.quote, tuning).is_some()).cloned().collect();
    println!("  (#544) among the stocks that score, soft admits too (rest = the other scored): what each boost multiplies.");
    print_cohorts(&holder_cohorts(&passed, txs));
    let hard = BuyHeuristic { growth_gate_soft: 0.0, ..tuning.clone() };
    // (#738) the samples behind the shipped listing rule's row, scored set, one line each: which names and
    // cutoffs its verdict rests on. `hard` = it also clears every gate.
    println!("  (#738) `8+ added` samples, scored set: ticker, cutoff, excess, hard");
    for s in passed.iter().filter(|s| picks::asset_class(&s.quote) == 2) {
        if txs.get(&s.quote.ticker).is_some_and(|t| core::holder_flags(t, s.date).adders >= 8) {
            println!("    {:<10} {} {:>+9.2} {}", s.quote.ticker, s.date, s.relative, growth_score(&s.quote, &hard).is_some());
        }
    }
    let passed: Vec<Sample> = samples.iter().filter(|s| growth_score(&s.quote, &hard).is_some()).cloned().collect();
    println!("  (#614) among the stocks that clear every gate (rest = the other hard passers):");
    print_cohorts(&holder_cohorts(&passed, txs));
}

/// (#518) One cohort table: each row's flagged side against the rest.
#[mutants::skip] // printing only
fn print_cohorts(rows: &[(&str, CohortSide, CohortSide)]) {
    println!(
        "  {:<36} {:>6} {:>8} {:>8} {:>6} | {:>6} {:>8} {:>8} {:>6}",
        "cohort", "n", "mean", "median", "hit%", "rest n", "mean", "median", "hit%"
    );
    for (name, a, b) in rows {
        println!(
            "  {name:<36} {:>6} {:>+8.2} {:>+8.2} {:>6.1} | {:>6} {:>+8.2} {:>+8.2} {:>6.1}",
            a.n, a.mean, a.median, a.hit, b.n, b.mean, b.median, b.hit
        );
    }
}

/// (#559) The `pac` cohorts, on stock samples whose cutoff's [`core::pac_cycle`] loaded. Row 1, the
/// pre-registered bar: among stocks with a matched corporate PAC that cycle, the top breadth tercile
/// against the other PAC firms. The tercile is cut per cutoff date: breadth above zero and at least the
/// value two-thirds up that date's sorted list (fewer than 3 PAC firms on a date: none on top). Row 2,
/// info only: any matched corporate PAC against none.
fn pac_cohorts(samples: &[Sample], pac: &fetch::PacData) -> [(&'static str, CohortSide, CohortSide); 2] {
    let mut by_date: BTreeMap<chrono::NaiveDate, Vec<(Option<u32>, f64)>> = BTreeMap::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) == 2) {
        let cycle = core::pac_cycle(s.date);
        if pac.cycles.contains(&cycle) {
            let b = pac.breadth.get(&s.quote.ticker).and_then(|m| m.get(&cycle)).copied();
            by_date.entry(s.date).or_default().push((b, s.relative));
        }
    }
    let (mut top, mut rest, mut any, mut none) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for rows in by_date.values() {
        let mut bs: Vec<u32> = rows.iter().filter_map(|r| r.0).collect();
        bs.sort_unstable();
        let line = bs.get(bs.len() * 2 / 3).copied().filter(|_| bs.len() >= 3);
        for &(b, x) in rows {
            let Some(b) = b else {
                none.push(x);
                continue;
            };
            any.push(x);
            if line.is_some_and(|l| b > 0 && b >= l) {
                top.push(x);
            } else {
                rest.push(x);
            }
        }
    }
    [
        ("PAC breadth top tercile (the bar)", cohort_side(top), cohort_side(rest)),
        ("any corporate PAC (info)", cohort_side(any), cohort_side(none)),
    ]
}

/// (#559) Samples each side of the bar needs before its verdict counts.
const PAC_MIN_N: usize = 30;

/// (#559) The pre-registered bar for one cell: n >= [`PAC_MIN_N`] on each side, and the top tercile
/// higher on median excess AND on hit rate.
fn pac_pass(top: &CohortSide, rest: &CohortSide) -> bool {
    top.n >= PAC_MIN_N && rest.n >= PAC_MIN_N && top.median > rest.median && top.hit > rest.hit
}

/// (#559) The `pac` report: the FEC fetch, the crosswalk audit, [`pac_cohorts`] with its verdict, and
/// the gate passers as info. Report only, no knob reads it.
#[mutants::skip] // network + printing; `pac_cohorts` and `pac_pass` carry the tests
async fn report_pac(client: &reqwest::Client, urls: &config::Urls, samples: &[Sample], years: i64, tuning: &BuyHeuristic) {
    let stocks: Vec<String> = samples
        .iter()
        .filter(|s| picks::asset_class(&s.quote) == 2)
        .map(|s| s.quote.ticker.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let (Some(lo), Some(hi)) = (samples.iter().map(|s| s.date).min(), samples.iter().map(|s| s.date).max()) else { return };
    let pac = fetch::fetch_pac_history(client, urls, &stocks, core::pac_cycle(lo), core::pac_cycle(hi)).await;
    println!("\n── (#559) PAC BREADTH: do firms whose PACs fund the most candidates beat the other PAC firms? ({years}y forward, stocks) ──");
    println!("  breadth = candidates a firm's corporate PACs gave a net positive sum (FEC 24K gifts) in the newest");
    println!("  two-year cycle that ended {} days before the cutoff. Cycles loaded: {:?}.", core::PAC_LAG_DAYS, pac.cycles);
    println!("  CAVEATS: FEC files are as-amended, not as-filed; a firm matches only by its SEC current or former");
    println!("  name, so delisted names (no CIK: most of the stress lane's losers) and renamed PACs are missed; PAC");
    println!("  firms skew large and mature; cutoffs overlap, so n counts samples, not independent bets.");
    println!(
        "  crosswalk: {} stocks matched over {} audit lines; 50 evenly spaced (cycle | FEC name | ticker | SEC name):",
        pac.breadth.len(),
        pac.audit.len()
    );
    for line in pac.audit.iter().step_by((pac.audit.len() / 50).max(1)).take(50) {
        println!("    {line}");
    }
    let rows = pac_cohorts(samples, &pac);
    print_cohorts(&rows);
    let (_, top, rest) = &rows[0];
    println!(
        "  VERDICT ({years}y): {} (bar: n >= {PAC_MIN_N} each side, top tercile higher on median AND hit%)",
        if pac_pass(top, rest) { "PASS" } else { "FAIL" }
    );
    let passed: Vec<Sample> = samples.iter().filter(|s| growth_score(&s.quote, tuning).is_some()).cloned().collect();
    println!("  among stocks the buy gates pass (info, no verdict):");
    print_cohorts(&pac_cohorts(&passed, &pac));
}

/// (#728) One row of `tests/failures.csv`, which `tools/failures_from_sec.py` writes.
#[derive(Clone, Debug, PartialEq)]
struct Failure {
    cik: String,
    ticker: String,
    event: chrono::NaiveDate,
    bankrupt: bool, // an 8-K Item 1.03; else a price wipeout
    assets_musd: Option<f64>,
}

/// (#728) The CSV's rows; the header and any malformed line drop out.
fn parse_failures(csv: &str) -> Vec<Failure> {
    csv.lines()
        .filter_map(|l| {
            let [cik, ticker, _, event, kind, assets] = l.split(',').collect::<Vec<_>>()[..] else { return None };
            Some(Failure {
                cik: cik.into(),
                ticker: ticker.into(),
                event: chrono::NaiveDate::parse_from_str(event, "%Y-%m-%d").ok()?,
                bankrupt: kind == "bankrupt",
                assets_musd: assets.parse().ok(),
            })
        })
        .collect()
}

/// (#728) Census horizons, in years before the event.
const FAIL_HORIZONS: [i64; 4] = [1, 2, 3, 5];
/// (#728) Row labels, in [`fail_groups`]' order.
const FAIL_GROUPS: [&str; 5] = ["all", "bankrupt", "bankrupt $1B+", "wipeout", "$1B+ or wipe"];
/// (#728) Failure instances a cell needs before its AUC counts toward the verdict.
const FAIL_MIN_N: usize = 30;

/// (#728) Which census rows a failure lands in: all, bankrupt, bankrupt with $1B+ of assets, wipeout,
/// and bar (b)'s size guard, bankrupt $1B+ or wipeout.
fn fail_groups(f: &Failure) -> [bool; 5] {
    let big = f.bankrupt && f.assets_musd.is_some_and(|a| a >= 1000.0);
    [true, f.bankrupt, big, !f.bankrupt, big || !f.bankrupt]
}

/// (#728) `FundFactors::red_flags` at `cutoff`: None when the newest row filed by it is over 550 days
/// old, since a filer that stopped filing has nothing current to read.
fn fail_flags(rows: &[core::FundRow], cutoff: chrono::NaiveDate) -> Option<[bool; 6]> {
    core::fund_factors(rows, cutoff, FUND_LOOKBACK_YRS).red_flags
}

/// (#728) The controls' flags at one cutoff: `hist[k]` names reading k flags, `flag[i]` reading flag i.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MonthHist {
    hist: [usize; 7],
    flag: [usize; 6],
}

fn month_hist(ctrls: &[(String, Vec<core::FundRow>)], cutoff: chrono::NaiveDate) -> MonthHist {
    let mut m = MonthHist::default();
    for f in ctrls.iter().filter_map(|(_, rows)| fail_flags(rows, cutoff)) {
        m.hist[f.iter().filter(|b| **b).count()] += 1;
        for (n, on) in m.flag.iter_mut().zip(f) {
            *n += usize::from(on);
        }
    }
    m
}

/// (#728) One census cell, summed over its failure instances. Each instance is graded against the
/// controls of its own cutoff month: `auc` adds P(the failure reads more flags than a control) + half
/// the ties, `ctrl_ge[k]` the share of controls reading k+ flags, `ctrl_flag[i]` the share with flag i.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FailTally {
    n: usize,
    auc: f64,
    fail_ge: [usize; 7],
    ctrl_ge: [f64; 7],
    fail_flag: [usize; 6],
    ctrl_flag: [f64; 6],
}

impl FailTally {
    /// A month with no control is skipped: there is nothing to rank the failure against.
    fn add(&mut self, f: [bool; 6], m: &MonthHist) {
        let n: usize = m.hist.iter().sum();
        if n == 0 {
            return;
        }
        let n = n as f64;
        let c = f.iter().filter(|b| **b).count();
        self.n += 1;
        self.auc += (m.hist[..c].iter().sum::<usize>() as f64 + 0.5 * m.hist[c] as f64) / n;
        for (k, (fail, ctrl)) in self.fail_ge.iter_mut().zip(&mut self.ctrl_ge).enumerate() {
            *fail += usize::from(c >= k);
            *ctrl += m.hist[k..].iter().sum::<usize>() as f64 / n;
        }
        for (i, (fail, ctrl)) in self.fail_flag.iter_mut().zip(&mut self.ctrl_flag).enumerate() {
            *fail += usize::from(f[i]);
            *ctrl += m.flag[i] as f64 / n;
        }
    }

    /// n >= [`FAIL_MIN_N`] and a mean AUC of at least `bar`.
    fn auc_at_least(&self, bar: f64) -> bool {
        self.n >= FAIL_MIN_N && self.auc >= bar * self.n as f64
    }
}

/// (#728) The pre-registered bar, fixed before the census ran. `t[group][horizon][era]`: groups as in
/// [`fail_groups`], horizons as in [`FAIL_HORIZONS`], era 0 = events through 2017, 1 = from 2018,
/// 2 = pooled. PASS needs all of:
///   (a) all failures, each era: AUC >= 0.75 at T-1y and >= 0.65 at T-3y, n >= [`FAIL_MIN_N`] a cell;
///   (b) bankrupt $1B+ or wipeout, pooled: AUC >= 0.70 at T-1y, the same n floor;
///   (c) k = the smallest flag count that at most 10% of controls reach at T-1y, era 0;
///   (d) at that k, at least 40% of era-1 failures reach it at T-1y.
/// Returns the verdict and (c)'s k.
fn fail_verdict(t: &[[[FailTally; 3]; 4]; 5]) -> (bool, Option<usize>) {
    let a = (0..2).all(|e| t[0][0][e].auc_at_least(0.75) && t[0][2][e].auc_at_least(0.65));
    let b = t[4][0][2].auc_at_least(0.70);
    let (old, new) = (&t[0][0][0], &t[0][0][1]);
    let k = (1..7).find(|&k| old.ctrl_ge[k] <= 0.10 * old.n as f64);
    let d = k.is_some_and(|k| new.fail_ge[k] as f64 >= 0.40 * new.n as f64);
    (a && b && d, k)
}

/// (#728) The `failures` census as printed lines. `fails` pairs each library row with its SEC rows,
/// `ctrls` each control's ticker with its rows; `today` dates the lookalike list.
#[mutants::skip] // printing; `fail_flags`, `month_hist`, `FailTally::add` and `fail_verdict` carry the tests
fn failure_census_lines(fails: &[(Failure, Vec<core::FundRow>)], ctrls: &[(String, Vec<core::FundRow>)], today: chrono::NaiveDate) -> Vec<String> {
    let mut t = [[[FailTally::default(); 3]; 4]; 5];
    let mut months = HashMap::new();
    let mut missing = [0usize; 4];
    let mut quiet = Vec::new();
    for (f, rows) in fails {
        let era = usize::from(f.event.year() >= 2018);
        for (h, yrs) in FAIL_HORIZONS.iter().enumerate() {
            let at = f.event - chrono::Duration::days(365 * yrs);
            let cutoff = at.with_day(1).unwrap_or(at);
            let Some(flags) = fail_flags(rows, cutoff) else {
                missing[h] += 1;
                continue;
            };
            if h == 0 && !flags.contains(&true) {
                quiet.push(f.ticker.as_str());
            }
            let m = *months.entry(cutoff).or_insert_with(|| month_hist(ctrls, cutoff));
            for (g, _) in fail_groups(f).iter().enumerate().filter(|(_, on)| **on) {
                t[g][h][era].add(flags, &m);
                t[g][h][2].add(flags, &m);
            }
        }
    }
    let pct = |x: f64, n: usize| if n == 0 { "—".to_string() } else { format!("{:.0}", 100.0 * x / n as f64) };
    let auc = |c: &FailTally| if c.n == 0 { "—".to_string() } else { format!("{:.2}", c.auc / c.n as f64) };
    let bk = fails.iter().filter(|(f, _)| f.bankrupt).count();
    let mut out = vec![
        "\n── (#728) FAILURES: did filers that went bankrupt or were wiped out read more red flags than survivors that month? ──".to_string(),
        format!(
            "  library: {} rows of tests/failures.csv, {bk} bankrupt (8-K Item 1.03, assets $100M+), {} wipeout (-80%, no recovery); SEC rows for {}",
            fails.len(),
            fails.len() - bk,
            fails.iter().filter(|(_, r)| !r.is_empty()).count()
        ),
        format!("  controls: {} pond stocks with SEC rows, library CIKs removed. They survived to today, which flatters every AUC.", ctrls.len()),
        "  flags: L loss, 2 a second loss year, B operating cash burn, I interest cover < 1.5x, R revenue -10%, D shares +10%".to_string(),
        "  AUC = P(a failure reads more flags than a control the same month) + half the ties; 0.5 = no signal".to_string(),
        format!("  {:<21}{:>5}{:>7}{:>7}{:>7}   fail% / ctrl% per flag, pooled", "", "n", "AUC", "<=2017", ">=2018"),
    ];
    for (h, yrs) in FAIL_HORIZONS.iter().enumerate() {
        for (g, name) in FAIL_GROUPS.iter().enumerate() {
            let [old, new, all] = &t[g][h];
            let flags: Vec<String> =
                core::FAIL_FLAGS.iter().enumerate().map(|(i, l)| format!("{l} {}/{}", pct(all.fail_flag[i] as f64, all.n), pct(all.ctrl_flag[i], all.n))).collect();
            out.push(format!("  T-{yrs}y {name:<15}{:>5}{:>7}{:>7}{:>7}   {}", all.n, auc(all), auc(old), auc(new), flags.join("  ")));
        }
    }
    let (pass, k) = fail_verdict(&t);
    let [old, new, _] = &t[0][0];
    if let Some(k) = k {
        out.push(format!(
            "  (c) k = {k}: {}% of controls read {k}+ flags at T-1y (<=2017); (d) {}% of >=2018 failures do, {}% of their controls",
            pct(old.ctrl_ge[k], old.n),
            pct(new.fail_ge[k] as f64, new.n),
            pct(new.ctrl_ge[k], new.n)
        ));
    }
    let gaps: Vec<String> = FAIL_HORIZONS.iter().zip(missing).map(|(y, n)| format!("T-{y}y {n}")).collect();
    out.push(format!("  no row filed in the 550 days to the cutoff (no XBRL yet, or a refused GET): {}", gaps.join(", ")));
    out.push(format!(
        "  0 flags at T-1y: {} failures (a sudden death, a fraud or a mislabel), first 40: {}",
        quiet.len(),
        quiet.iter().take(40).copied().collect::<Vec<_>>().join(" ")
    ));
    out.push(format!(
        "  VERDICT: {} (bar: (a) all, each era, n >= {FAIL_MIN_N}: AUC >= 0.75 at T-1y and >= 0.65 at T-3y; (b) $1B+ or wipe, pooled: AUC >= 0.70 at T-1y; (d) >= 40% of >=2018 failures at k)",
        if pass { "PASS" } else { "FAIL" }
    ));
    let k = k.unwrap_or(6);
    let at = today.with_day(1).unwrap_or(today);
    let mut look: Vec<(usize, &str, String)> = ctrls
        .iter()
        .filter_map(|(tk, rows)| {
            let f = fail_flags(rows, at)?;
            let n = f.iter().filter(|b| **b).count();
            (n >= k).then(|| (n, tk.as_str(), core::FAIL_FLAGS.iter().zip(f).filter(|(_, on)| *on).map(|(l, _)| *l).collect()))
        })
        .collect();
    look.sort_by_key(|(n, tk, _)| (std::cmp::Reverse(*n), *tk));
    let names: Vec<String> = look.iter().map(|(_, tk, l)| format!("{tk}({l})")).collect();
    out.push(format!("  TODAY ({at}): {} pond stocks read {k}+ flags: {}", look.len(), names.join(" ")));
    out
}

/// (#728) The `failures` report: the library's SEC rows by CIK, the pond's stocks as controls, then
/// [`failure_census_lines`]. Report only, no knob reads it.
#[mutants::skip] // network + printing; `failure_census_lines` carries the tests
async fn report_failures(client: &reqwest::Client, urls: &config::Urls, samples: &[Sample]) {
    let lib = parse_failures(include_str!("../../tests/failures.csv"));
    let ciks: HashSet<&str> = lib.iter().map(|f| f.cik.as_str()).collect();
    let stocks: std::collections::BTreeSet<&str> =
        samples.iter().filter(|s| picks::asset_class(&s.quote) == 2).map(|s| s.quote.ticker.as_str()).collect();
    let mut ctrls = Vec::new();
    for tk in stocks {
        let Some(rows) = fetch::fetch_fundamentals_ranked(client, urls, tk).await else { continue };
        if !fetch::cached_cik(tk).is_some_and(|c| ciks.contains(c.as_str())) {
            ctrls.push((tk.to_string(), rows));
        }
    }
    let fails: Vec<(Failure, Vec<core::FundRow>)> = stream::iter(lib.iter().cloned())
        .map(|f| async move {
            let rows = fetch::sec_facts_by_cik(client, urls, &f.cik).await;
            (f, rows)
        })
        .buffered(8)
        .collect()
        .await;
    for line in failure_census_lines(&fails, &ctrls, chrono::Local::now().date_naive()) {
        println!("{line}");
    }
    let (sent, refused) = fetch::sec_stats();
    println!("  SEC requests this run: {sent} sent, {refused} refused");
}

/// (#424) tickers whose SEC rows `restate_for_splits` moved this run. Per process, like `sec_stats`: one
/// walk per `backtest` invocation, each ticker counted once, so the fixture golden pins an exact number.
static SPLIT_RESTATED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// (G) Probe each as-of fundamental factor STANDALONE against the same peer-relative forward return:
/// rho (selection), top/bottom-half edge (profit spread), and the early-vs-late OOS split. No ablation
/// — each factor IS its own column, so there's nothing to switch off. This is the validation gate: a
/// factor earns a place in `growth_score` only if it shows real edge with both-positive OOS, the same
/// bar the price knobs cleared. `samples` is date-ordered (the OOS split is early-vs-late in time).
fn report_fund_lane(samples: &[Sample], purge_months: i64) {
    let factors: &[(&str, fn(&core::FundFactors) -> Option<f64>)] = &[
        ("revenue_cagr", |f| f.rev_cagr),
        ("revenue_accel", |f| f.rev_accel),
        ("gross_margin", |f| f.gross_margin),
        ("op_margin", |f| f.op_margin),
        ("margin_trend", |f| f.margin_trend),
        ("eps_growth", |f| f.eps_growth),
        // the three columns the report/screen tables print. Probed for the first time here: rev_yoy was
        // buried inside rev_accel, eps_yoy did not exist, and net_margin was never carried — op_margin
        // was the only margin LEVEL ever measured, and it reads strongly NEGATIVE (rho -0.23), which is
        // exactly why the below-the-line twin is worth its own row rather than being assumed to match.
        ("rev_yoy", |f| f.rev_yoy),
        ("eps_yoy", |f| f.eps_yoy),
        ("net_margin", |f| f.net_margin),
        ("quality(roe/roa)", |f| f.quality),            // quality of capital, the SCORED resolution: ROE, or ROA where equity is negative (SEC feed only; FMP free tier = None). The raw `roe` was probed here before and measured negative-equity fakes the live path rejected — this is the number the score now reads
        ("roe_raw", |f| f.roe),                         // unfiltered ROE, kept alongside so the fallback's effect is visible: the two rows differ only on negative-equity filers
        ("insider_net90d", |f| f.insider_net_buys_90d), // (Item 4) only populated under `insider`
        ("earnings_yield", |f| f.earnings_yield),       // (Item 19) as-of valuation (high = cheap); native-currency probe
        ("ebitda_yield", |f| f.ebitda_yield),           // (EV/EBITDA) capital-structure-neutral valuation cousin
        ("peg_yield", |f| f.peg_yield),                 // (PEG) growth-at-price = earnings_yield · as-of CAGR (1/PEG)
        ("buyback_yield", |f| f.buyback_yield),         // as-of 1y share-count shrink (+ = buying back)
        ("composite", |f| core::select_fund_factor(f, "composite")), // (Item 3) blend of the present factors
    ];
    let covered = samples.iter().filter(|s| s.fund.is_some()).count();
    println!("\n── FUNDAMENTAL (as-of, standalone factor probes) ──");
    println!("  cutoffs with as-of fundamentals: {} / {}", covered, samples.len());
    let (sent, refused) = crate::fetch::sec_stats();
    println!("  SEC requests this run: {sent} sent, {refused} refused (403/429/5xx/transport)");
    let restated = SPLIT_RESTATED.load(std::sync::atomic::Ordering::Relaxed);
    println!("  split-restated tickers: {restated} (filings put on the chart's split basis)");
    println!("  core-stripped tickers: {} (EPS with non-operating gains taken off)", crate::fetch::core_stripped());
    if covered < 4 {
        println!("  too few fundamental cutoffs (needs FMP_API_KEY + cached `stable/income-statement` history) — skipping.");
        return;
    }
    let mean = |s: &[&(&Sample, f64)]| s.iter().map(|x| x.0.relative).sum::<f64>() / s.len().max(1) as f64;
    let split_rho = |s: &[(&Sample, f64)]| {
        core::spearman(
            &s.iter().map(|x| x.1).collect::<Vec<_>>(),
            &s.iter().map(|x| x.0.relative).collect::<Vec<_>>(),
        )
        .map_or("n/a".to_string(), |v| format!("{v:+.2}"))
    };
    for (name, get) in factors {
        let pairs: Vec<(&Sample, f64)> =
            samples.iter().filter_map(|s| s.fund.as_ref().and_then(get).map(|v| (s, v))).collect();
        if pairs.len() < 4 {
            println!("  {:<14} n/a (only {} cutoffs carry this factor)", name, pairs.len());
            continue;
        }
        let sc: Vec<f64> = pairs.iter().map(|(_, v)| *v).collect();
        let rels: Vec<f64> = pairs.iter().map(|(s, _)| s.relative).collect();
        let rho = core::spearman(&sc, &rels).map_or("n/a".to_string(), |v| format!("{v:+.2}"));
        let mut v: Vec<&(&Sample, f64)> = pairs.iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let half = v.len() / 2;
        let edge = mean(&v[..half]) - mean(&v[v.len() - half..]);
        let mid = pairs.len() / 2; // pairs preserve the date order of `samples` -> early-vs-late OOS
        let early = purged_cut(&pairs, mid, purge_months, |p: &(&Sample, f64)| p.0.date); // (#90)
        println!(
            "  {:<14} n={:<5} rho {}  edge {:+.1}  OOS {} | {}",
            name, pairs.len(), rho, edge, split_rho(&pairs[..early]), split_rho(&pairs[mid..])
        );
    }
}

/// Closes-derived risk stats probed STANDALONE against the same peer-relative forward return —
/// the price-side twin of `report_fund_lane`, same validation gate: a stat earns a `growth_score`
/// term only on real edge with both-positive OOS. Extracted from `Sample.quote` (the consistency/
/// worst windows scale with the run's cadence, so they fill on daily AND monthly; `underwater_neg`
/// is daily-only -> it skips on a monthly run). `underwater_neg` negates years-underwater so
/// "higher = better" matches the rho/edge convention of every probe.
const RISK_FACTORS: &[(&str, fn(&Quote) -> Option<f64>)] = &[
    ("consistency_5y", |q| q.roll5y_pos_pct),               // % of rolling 5y windows positive
    ("consistency_10y", |q| q.roll10y_pos_pct),             // same hit-rate at the DECADE horizon (fills on monthly runs)
    ("worst_5y", |q| q.worst_5y_pct),                       // single worst rolling 5y outcome
    ("underwater_neg", |q| q.underwater_yrs.map(|y| -y)),   // longest below-peak stretch, negated
    // (r39) the Sortino question, asked as a matched pair so the answer is readable. `sharpe_ref`
    // is the INCUMBENT denominator (`volatility_pct`, the stat `risk_bonus` already ranks on) and
    // `sortino` is the identical ratio with only-down-moves underneath it — same numerator, same
    // window, same cadence. A verdict is only meaningful as the DIFFERENCE between these two rows:
    // `sortino` beating `sharpe_ref` is the claim "vol punishes a compounder for its up-moves".
    // Both skip a name with no trend CAGR or a zero denominator (an all-positive stretch measures
    // no downside — that is missing data, not an infinite ratio).
    ("downside_dev_neg", |q| q.downside_dev_pct.map(|d| -d)), // the raw stat alone, negated (less downside = better)
    ("sortino", |q| ratio(q.trend_cagr, q.downside_dev_pct)),
    ("sharpe_ref", |q| ratio(q.trend_cagr, q.volatility_pct)),
];

/// Return-per-unit-of-risk for the paired risk probes: `None` unless BOTH legs exist and the
/// denominator is strictly positive, so a zero-risk name drops out of the sample instead of
/// landing at infinity and hijacking the rank correlation.
fn ratio(num: Option<f64>, den: Option<f64>) -> Option<f64> {
    match (num, den) {
        (Some(n), Some(d)) if d > 0.0 => Some(n / d),
        _ => None,
    }
}

/// One standalone-probe row: Spearman rho of the signal vs the same peer-relative forward return, the
/// top-minus-bottom-half EDGE on that return, and the early-vs-late OOS split. Shared by every
/// standalone probe (`report_risk_lane`, `report_relative_strength`) so each measures a signal the
/// SAME way — a factor earns a score term only on real edge with both OOS halves positive. `pairs`
/// preserve `samples`' date order, so the midpoint IS the early/late OOS cut. Fewer than 4 pairs is
/// no claim, never a fabricated number.
fn emit_probe(name: &str, pairs: &[(&Sample, f64)], purge_months: i64) {
    if pairs.len() < 4 {
        println!("  {:<14} n/a (only {} cutoffs carry this stat)", name, pairs.len());
        return;
    }
    let mean = |s: &[&(&Sample, f64)]| s.iter().map(|x| x.0.relative).sum::<f64>() / s.len().max(1) as f64;
    let split_rho = |s: &[(&Sample, f64)]| {
        core::spearman(
            &s.iter().map(|x| x.1).collect::<Vec<_>>(),
            &s.iter().map(|x| x.0.relative).collect::<Vec<_>>(),
        )
        .map_or("n/a".to_string(), |v| format!("{v:+.2}"))
    };
    let sc: Vec<f64> = pairs.iter().map(|(_, v)| *v).collect();
    let rels: Vec<f64> = pairs.iter().map(|(s, _)| s.relative).collect();
    let rho = core::spearman(&sc, &rels).map_or("n/a".to_string(), |v| format!("{v:+.2}"));
    let mut v: Vec<&(&Sample, f64)> = pairs.iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    let half = v.len() / 2;
    let edge = mean(&v[..half]) - mean(&v[v.len() - half..]);
    let mid = pairs.len() / 2; // pairs preserve the date order of `samples` -> early-vs-late OOS
    let early = purged_cut(pairs, mid, purge_months, |p: &(&Sample, f64)| p.0.date); // (#90)
    println!(
        "  {:<14} n={:<5} rho {}  edge {:+.1}  OOS {} | {}",
        name, pairs.len(), rho, edge, split_rho(&pairs[..early]), split_rho(&pairs[mid..])
    );
}

fn report_risk_lane(samples: &[Sample], purge_months: i64) {
    println!("\n── PRICE-RISK (closes-derived, standalone probes) ──");
    for (name, get) in RISK_FACTORS {
        let pairs: Vec<(&Sample, f64)> =
            samples.iter().filter_map(|s| get(&s.quote).map(|v| (s, v))).collect();
        emit_probe(name, &pairs, purge_months);
    }
}

/// (r40) RELATIVE STRENGTH vs the index, as a matched pair so the verdict is a DIFFERENCE, not a
/// floating number. `abs_mom_5y` is the name's trailing 5y return (absolute momentum); `rel_str_5y`
/// subtracts what the benchmark did over the identical 5y window ending at the cutoff. They differ
/// ONLY by that subtraction, so `rel_str_5y` beating `abs_mom_5y` is exactly the claim "being ahead
/// of the INDEX predicts forward excess return better than being up in absolute terms"; level rows =
/// the subtraction is inert and there is nothing to ship. Window is a FIXED 5y (well-populated on the
/// monthly series, the score's most reliable long leg) so the SAME signal is graded at both the 8y
/// and 20y forward horizons. Probe-only: no score term, no live fill — this measures whether such a
/// term would be worth the (large) work of plumbing the benchmark into the per-`Quote` score.
fn report_relative_strength(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), purge_months: i64) {
    let (bd, bc) = bench;
    println!("\n── RELATIVE STRENGTH (name vs benchmark, trailing 5y; abs momentum vs index-relative) ──");
    let abs_pairs: Vec<(&Sample, f64)> =
        samples.iter().filter_map(|s| picks::perf_pct(&s.quote, "5Y").map(|v| (s, v))).collect();
    let rel_pairs: Vec<(&Sample, f64)> = samples
        .iter()
        .filter_map(|s| {
            let name = picks::perf_pct(&s.quote, "5Y")?;
            let bench_ret = bench_trailing(bd, bc, s.date, 5)?;
            Some((s, name - bench_ret))
        })
        .collect();
    emit_probe("abs_mom_5y", &abs_pairs, purge_months);
    emit_probe("rel_str_5y", &rel_pairs, purge_months);
}

/// (G) Pick the fund factor whose HELD-OUT TEST edge wins AND whose two OOS halves are both positive AND
/// that beats the price-only baseline. None -> no factor earns the tilt (keep growth_fund_weight 0). Pure
/// (tuples in, name out) so the sweep's verdict is unit-testable without building gate-clearing quotes.
fn pick_sweep_winner<'a>(results: &[(&'a str, f64, Option<f64>, Option<f64>)], baseline: f64) -> Option<&'a str> {
    results
        .iter()
        .filter(|(_, edge, a, b)| *edge > baseline && a.is_some_and(|v| v > 0.0) && b.is_some_and(|v| v > 0.0))
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .map(|(name, ..)| *name)
}

/// (Item 10) The best-of-N haircut's two tail percentiles: a 90% band tightened to 5/N a side, so a
/// winner picked as the MAX of `n` tried factors is charged for having been selected.
///
/// Extracted from the call site because the call site is inside `sweep_fund_factor`, a `-> ()` printer
/// the mutation gate cannot run at all — only the fund goldens run the fund lane (frozen SEC rows since
/// (#364)) and the gate skips them ((#590), ci.yml's `--skip backtest_fund_report --skip
/// backtest_8y_fund_samples`; (#675) this used to say the lane needs a live `FMP_API_KEY` and every
/// golden runs offline). Inline, the six operator mutants on this arithmetic were unkillable by
/// construction; as a pure fn they are just tested.
///
/// (#1227) Renamed from `sidak_tail`: 5/N a side is the Bonferroni split. Šidák would be
/// 1-(0.95)^(1/N), a hair looser; the arithmetic never changed, only the name was wrong.
fn bonferroni_tail(n: usize) -> (f64, f64) {
    let side = 5.0 / n.max(1) as f64;
    (side, 100.0 - side)
}

/// (#1045) The verdict on the haircut band, out of `sweep_fund_factor` for the same reason as
/// `bonferroni_tail`: inline, a `>` turned `>=` read a zero-floor band as a WINNER and no test could run it.
/// Strict: a band whose floor touches 0 is still best-of-N luck.
fn band_verdict(lo: f64) -> &'static str {
    if lo > 0.0 {
        "survives multiple testing -> trust the WINNER"
    } else {
        "within best-of-N luck -> SHIP NOTHING despite the raw winner"
    }
}

const FUND_FACTORS: [&str; 53] = [
    "rev_cagr", "rev_accel", "gross_margin", "op_margin", "margin_trend", "eps_growth",
    // the printed columns (REV-YoY / EPS-YoY / NET%), swept for the first time. Widening this
    // array TIGHTENS every reported band: the Bonferroni haircut below divides by FUND_FACTORS.len(), so
    // going 11 -> 14 -> 24 makes the best-of-N test stricter, not the factors weaker. A later
    // reader comparing bands across runs must check this length before calling it a regression.
    "rev_yoy", "eps_yoy", "net_margin",
    "insider_net_buys_90d", // (Item 4) shows n/a unless `insider` populated it
    "earnings_yield",       // (Item 19) as-of valuation; native-currency probe (n/a unless `fund`)
    // (PEG) growth-at-price = earnings_yield · as-of CAGR (1/PEG; 100 ⇔ PEG 1, >100 ⇔ PEG <1).
    // Swept HERE, not just in the held-book list, because this is the sweep that carries the
    // Bonferroni best-of-N haircut — the axis was annotated "expected dead" for rounds without ever
    // being measured. NOTE the scale: peg_yield runs ~0-500 where earnings_yield runs ~0-15, so
    // the default `growth_fund_cap: 30` clamps every PEG < 3.3 name to the same value and would
    // fake a dead result. Raise the cap (~300) for any run that reads this row.
    "peg_yield",
    "buyback_yield",        // as-of 1y share-count shrink (+ = buying back); n/a unless `fund` + shares in the rows
    // (#157) the ten factors `core::select_fund_factor` has always registered and computed but
    // this array never asked about. They were reachable from every other read site — the
    // held-book-by-FACTOR table, the SURVIVAL-GATE probe, `growth_fund_weight` — and absent ONLY
    // here, which is the one place that applies the Bonferroni best-of-N haircut. So the four survival
    // factors had a percentile-cut reading from (#124) and no honest multiple-testing band, and
    // the other six had neither. Read the n/a rows as a result: they record which factors the
    // point-in-time pool cannot populate, which is a fact about coverage, not about the factor.
    "roe", "quality", "roic", "ebitda_yield",
    "fcf_margin", "interest_cover", "net_cash_rev", "margin_stability",
    "accrual_gap", "asset_growth",
    "gp_assets",            // (#405) Novy-Marx gross profitability; 24 -> 25 tightens every band again
    "cf_assets", "rev_stability", // (#407) Ball et al. cash profitability, Mohanram G5; 25 -> 27
    "fcf_ev_yield", "sales_ev_yield", "value_composite", // (#408) O'Shaughnessy value family; 27 -> 30
    "book_yield", "small_pct", // (#409) Fama-French HML and size; 30 -> 32
    "rnd_yield", "payout_yield", // (#410) R&D/ME (Chan-Lakonishok-Sougiannis), net payout (BMRR); 32 -> 34
    "noa", "op_leverage", // (#411) net operating assets (HHTZ, negated), operating leverage (Novy-Marx); 34 -> 36
    "mscore", "sbc", "goodwill", // (#412) Beneish M, SBC ÷ revenue, goodwill ÷ assets, all negated; 36 -> 39
    "org_cap", "int_yield", // (#413) Eisfeldt-Papanikolaou org capital ÷ assets, EKP intangible value; 39 -> 41
    "lynch_yield", "sgr_yield", "exp_neg", // (#416) PEG over EPS growth, over ROE·retention, and de-rating; 41 -> 44
    "op_rd", "discipline", // (#417) Ball et al. R&D-adjusted OP ÷ assets, EBITDA growth minus asset growth; 44 -> 46
    "fair_yield", "oe_fair_yield", "fcf_cap_yield", // (#523) the page's FAIR family: FAIR ÷ price, FAIR-OE ÷ price, 3y FCF ÷ cap; 46 -> 49
    "low_turnover",         // (#531) Ibbotson-Chen-Kim-Hu 2013 popularity, share turnover negated; 49 -> 50
    "rnd_growth",           // (#532) Arnott et al. 2026 fundamental growth, the R&D leg: 3y ΔR&D ÷ old revenue; 50 -> 51
    "low_dro",              // (#533) Wang 2019 CCC spread, the receivables leg: −receivables days, sector-demeaned; 51 -> 52
    "capex_light",          // (#549) −capex ÷ OCF, annual, raw: the (#545) column's question asked point-in-time; 52 -> 53
    "composite",            // (Item 3) shows n/a until ≥2 factors are present
];

/// (G) Auto-select `growth_fund_factor`: for each candidate as-of factor, re-derive every sample's
/// fund_factor IN MEMORY (no refetch — Sample.fund is already cached), search growth_fund_weight on the
/// EARLY train half, and report the factor's edge on the LATE test half it never saw + its two OOS sub-
/// halves. This judges each factor THROUGH the growth lane (report_fund_lane only probes them standalone),
/// then prints the one to paste into settings.yaml. Ships nothing. Needs the `fund` path; with <8 cutoffs
/// carrying fundamentals there's nothing to sweep. Same chronological split + seeded search as `tune`.
///
/// (#119) UNGRADEABLE BY THE MUTATION GATE — only the fund goldens run the fund lane (frozen SEC rows
/// since (#364)), and the gate skips them ((#590), ci.yml's `--skip backtest_fund_report --skip
/// backtest_8y_fund_samples`; (#672) this used to say the lane needs a live key), so no test the gate
/// runs executes one line of this. `replace sweep_fund_factor with ()` is unkillable there, and
/// skipping says so instead of arming the gate against the next edit.
///
/// The arithmetic that WAS gradeable moved out rather than being skipped with it: `bonferroni_tail` and
/// `bootstrap_block` are pure and tested. What stays here is the printing.
#[mutants::skip]
fn sweep_fund_factor(samples: &[Sample], default: &BuyHeuristic, years: i64) {
    if samples.iter().filter(|s| s.fund.is_some()).count() < 8 {
        return; // no as-of fundamentals (cold SEC cache, or the FMP source with no key) -> report_fund_lane already said so
    }
    let cut = samples.len() * 7 / 10;
    // best held-out TEST (edge, OOS-early rho, OOS-late rho) when the growth fund tilt routes `factor`
    // (None = price-only baseline). Re-derives relatives per split from raw realized, like tune_growth.
    // returns (TEST edge, OOS-early, OOS-late, won growth_fund_weight) — the weight feeds Item 10's
    // winner-bootstrap so the multiple-testing band re-scores the SAME tilt the sweep picked.
    let eval = |factor: Option<&str>| -> (f64, Option<f64>, Option<f64>, f64) {
        let mut s = samples.to_vec();
        for smp in &mut s {
            Arc::make_mut(&mut smp.quote).fund_factor =
                factor.and_then(|n| smp.fund.as_ref().and_then(|f| core::select_fund_factor(f, n)));
        }
        demean(&mut s[..cut]);
        demean(&mut s[cut..]);
        // (#90) same purge as `tune_growth`, for the same reason and off the same knob.
        let (train, test) = (&s[..purged_cut(&s, cut, default.split_purge_months, |s| s.date)], &s[cut..]);
        // 1-D search: growth_fund_weight in [0,0.5] on TRAIN; keep the best train edge with rho>0.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut won = default.clone();
        let mut best = f64::NEG_INFINITY;
        // (#595) Only growth_fund_weight moves between the draws, and the score reads it once, as
        // `weight × fund_fill`. A row whose fill is 0 (a factor at or below 0, or none at the shipped
        // neutral of 0) scores the same at every drawn weight: w × ±0.0 is the same ±0.0 for any w in
        // [0, 0.5). So those rows are scored once, at weight 0, and only the rest per draw. Same rows,
        // same order, same floats, so every number below is unchanged. The draws were ~90% of a fund
        // run's CPU (profile.yml run 38757728933), and most rows carry no positive factor: no
        // fundamentals, an ETF, a coin.
        let zero = BuyHeuristic { growth_fund_weight: 0.0, ..default.clone() };
        let fixed: Vec<Option<Option<f64>>> =
            train.iter().map(|s| (fund_fill(&s.quote, &zero) == 0.0).then(|| growth_score(&s.quote, &zero))).collect();
        for _ in 0..200 {
            let mut t = default.clone();
            t.growth_fund_weight = next() * 0.5;
            let scored: Vec<(&Sample, f64)> = train
                .iter()
                .zip(fixed.iter())
                .filter_map(|(s, &f)| f.unwrap_or_else(|| growth_score(&s.quote, &t)).map(|v| (s, v)))
                .collect();
            let (rho, edge) = scored_metrics(&scored);
            if rho.unwrap_or(0.0) > 0.0 && edge > best {
                best = edge;
                won = t;
            }
        }
        let mid = test.len() / 2; // test keeps date order -> early-vs-late OOS sub-halves
        // (#90) the sub-halves are a chronological split too, and the "both halves positive" reading
        // treats them as two pieces of evidence — so the early one is purged back off the same knob.
        let early = purged_cut(test, mid, default.split_purge_months, |s| s.date);
        (
            lane_metrics(test, growth_score, &won).1,
            lane_metrics(&test[..early], growth_score, &won).0,
            lane_metrics(&test[mid..], growth_score, &won).0,
            won.growth_fund_weight,
        )
    };

    let (baseline, ..) = eval(None); // price-only TEST edge: the bar every factor must clear
    // (#168) KEEP the searched weight. `eval`'s 4th element is the `growth_fund_weight` the 200-draw
    // search actually won with, and it was dropped here for every row but the winner — which the block
    // below then re-derived with a whole second search. A row that TIES the baseline is a RESULT when
    // the search chose w=0 ("do not use this factor") and a NON-MEASUREMENT when it chose a real weight
    // and the book still did not move; the edge alone cannot tell those apart, and six rows tie the
    // baseline at 12y. Kept parallel rather than widened into the tuple: `pick_sweep_winner` and its
    // test build that shape by hand, and (#167) is the receipt for what widening a tuple costs.
    // (#364) In parallel: `eval` re-seeds its own RNG per call, and an indexed collect keeps FUND_FACTORS
    // order, so the report is the same at any thread count (backtest-12-fund.golden pins it). Serial it
    // was 25 x 200 draws in a row, 90s of the fixture under CI's `dev` profile.
    let (results, weights): (Vec<(&str, f64, Option<f64>, Option<f64>)>, Vec<f64>) = FUND_FACTORS
        .par_iter()
        .map(|&n| {
            let (e, a, b, w) = eval(Some(n));
            ((n, e, a, b), w)
        })
        .unzip();

    let fmt = |r: Option<f64>| r.map_or("n/a".to_string(), |v| format!("{v:+.2}"));
    println!("\n── FUND FACTOR SWEEP (growth_fund_weight searched per factor, held-out TEST) ──");
    println!("  price-only baseline TEST edge {baseline:+.1} — a factor must beat this with both OOS halves +");
    for ((name, edge, a, b), w) in results.iter().zip(&weights) {
        println!("  {name:<14} TEST edge {edge:+.1} (w {w:.3})  OOS {} | {}", fmt(*a), fmt(*b));
    }
    match pick_sweep_winner(&results, baseline) {
        Some(w) => {
            // (#961) `fund` in the confirm command: without it every fund_factor is None and `tune` drops
            // growth_fund_weight as inert, so the confirm run would never search the weight it names.
            println!("  -> WINNER: {w}. Set `growth_fund_factor: {w}` + a non-zero `growth_fund_weight`, then `backtest universe fund tune` to confirm.");
            // (Item 10) best-of-N haircut: the winner is the MAX of N tried factors, so its edge is inflated
            // by selection. Re-bootstrap the winner's tilt but read a Bonferroni-tightened tail (5/N instead of
            // 5) — if THAT band straddles 0 the "win" is within best-of-N luck, ship nothing anyway.
            let weight = weights[results.iter().position(|&(n, ..)| n == w).expect("the winner is a swept row")];
            // The winner's edge was measured AT THIS WEIGHT, searched in [0,0.5] — which need not be the
            // shipped `growth_fund_weight`. Print it: shipping the factor at a different weight ships an
            // untested tilt, and the miss is silent (the factor name matches, the magnitude does not).
            // Read it together with `growth_fund_cap` — what the score sees is weight × clamp(value, 0, cap).
            println!("  -> validated growth_fund_weight for {w}: {weight:.3} (at growth_fund_cap {:.0})", default.growth_fund_cap);
            let mut s = samples.to_vec();
            for smp in &mut s {
                Arc::make_mut(&mut smp.quote).fund_factor =
                    smp.fund.as_ref().and_then(|f| core::select_fund_factor(f, w));
            }
            demean(&mut s); // peer-relative is the bootstrap's metric; `relative` depends only on date+realized
            let mut tun = default.clone();
            tun.growth_fund_weight = weight;
            let (lo_p, hi_p) = bonferroni_tail(FUND_FACTORS.len());
            let block = bootstrap_block(years, &tun);
            if let Some((lo, hi)) = bootstrap_edge_ci(&s, growth_score, &tun, 1000, lo_p, hi_p, block) {
                let verdict = band_verdict(lo);
                println!("  multiple-testing band (best of {} factors): [{lo:+.1} … {hi:+.1}] pts  ({verdict})", FUND_FACTORS.len());
            }
        }
        None => println!("  -> no factor beats price-only with both OOS halves + — keep growth_fund_weight 0. SHIP NOTHING."),
    }

    // (#3) WEIGHT CURVE for the CONFIGURED factor. Everything above reports only the search's ARGMAX,
    // which cannot tell a sharp peak from a plateau — and that is the entire question when asking
    // whether the tilt can carry more authority. Plateau => the shipped weight is leaving edge on the
    // table (for peg_yield: PEG < 1 could be worth several points instead of 1.8); sharp peak => the
    // shipped value IS the ceiling and "rank cheap names harder" has no honest room left. Same
    // samples / cut / demean / metric / early-late split as `eval`, but the weight is FIXED per row,
    // so every row is directly comparable to that factor's sweep line above. No re-fetch.
    let configured = default.growth_fund_factor.as_str();
    let covered = samples
        .iter()
        .any(|s| s.fund.as_ref().and_then(|f| core::select_fund_factor(f, configured)).is_some());
    if covered {
        let curve = |w: f64| -> (f64, Option<f64>, Option<f64>) {
            let mut s = samples.to_vec();
            for smp in &mut s {
                Arc::make_mut(&mut smp.quote).fund_factor =
                    smp.fund.as_ref().and_then(|f| core::select_fund_factor(f, configured));
            }
            demean(&mut s[..cut]);
            demean(&mut s[cut..]);
            let test = &s[cut..];
            let mut t = default.clone();
            t.growth_fund_weight = w;
            let mid = test.len() / 2;
            let early = purged_cut(test, mid, default.split_purge_months, |s| s.date); // (#90)
            (
                lane_metrics(test, growth_score, &t).1,
                lane_metrics(&test[..early], growth_score, &t).0,
                lane_metrics(&test[mid..], growth_score, &t).0,
            )
        };
        // bracket the shipped value and always include 0 as the tilt-off control; sourcing `shipped`
        // from config (not a literal) keeps the row present and correctly labelled after any move.
        let shipped = default.growth_fund_weight;
        // dense through 0.05-0.10: for peg_yield the edge PEAKS at 0.05 and has fallen below the
        // tilt-off baseline by 0.10, so the usable ceiling lives inside that decade. A coarse ladder
        // shows the cliff exists but not where it starts, which is the number that decides safety.
        let mut ladder =
            vec![0.0, 0.005, shipped, 0.05, 0.06, 0.07, 0.08, 0.09, 0.1, 0.25, 0.5];
        ladder.sort_by(|a, b| a.total_cmp(b));
        ladder.dedup();
        println!(
            "\n── growth_fund_weight CURVE for `{configured}` (held-out TEST, growth_fund_cap {:.0}) ──",
            default.growth_fund_cap
        );
        for w in ladder {
            let (edge, a, b) = curve(w);
            let tag = if w == 0.0 {
                "  [tilt off]"
            } else if w == shipped {
                "  [SHIPPED]"
            } else {
                ""
            };
            println!("  weight {w:<5.3}  TEST edge {edge:+.1}  OOS {} | {}{tag}", fmt(a), fmt(b));
        }
        println!("  (flat across a range -> the tilt can carry more weight; a clear peak -> the shipped value is the ceiling.");
        println!("   SELF-CHECK: the weight-0 row must equal the price-only baseline above, and the SHIPPED row must");
        println!("   reproduce that factor's sweep line — if either disagrees this curve is measuring something else.)");
    }
}

/// Annualize a cumulative % return over `years` -> CAGR %. Clamps the wealth base at 0 so a ≤−100%
/// forward window (a stress bankruptcy) reads as −100%/yr, not a NaN from a negative root.
fn ann(cum_pct: f64, years: i64) -> f64 {
    ((1.0 + cum_pct / 100.0).max(0.0).powf(1.0 / years as f64) - 1.0) * 100.0
}

/// (#566) The (#564) BOOK lines once per asset class (`picks::asset_class`: 2 stock, 1 ETF, 0 crypto),
/// each the class's own top-`n` equal-weight hold, so every class gets its own verdict. A class that
/// booked nobody in any bucket prints nothing; a bucket where it booked nobody prints `empty`, the index
/// you'd hold instead. `class_of(bucket, ticker)`.
// shortcut: skips the sector and corr caps the main book runs; both are the identity at their shipped 0.
fn class_book_lines(
    by_bucket: &BTreeMap<i32, Vec<(f64, f64, f64, String)>>,
    calendar: &std::collections::BTreeSet<i32>,
    class_of: impl Fn(i32, &str) -> Option<u8>,
    n: usize,
    years: i64,
) -> Vec<String> {
    let mut out = Vec::new();
    for (c, label) in [(2, "stock"), (1, "etf"), (0, "crypto")] {
        let book = |b: i32| {
            let mut p: Vec<_> = by_bucket.get(&b)?.iter().filter(|x| class_of(b, &x.3) == Some(c)).collect();
            if p.is_empty() {
                return None;
            }
            p.sort_by(|x, y| y.0.total_cmp(&x.0)); // score desc, like the main book
            p.truncate(n);
            let cum = |f: fn(&(f64, f64, f64, String)) -> f64| p.iter().map(|&x| 1.0 + f(x) / 100.0).sum::<f64>() / p.len() as f64;
            let excess = ann((cum(|x| x.1) - 1.0) * 100.0, years) - ann((cum(|x| x.2) - 1.0) * 100.0, years);
            let names: Vec<String> = p.iter().map(|x| format!("{} {:+.0}%", x.3, x.1)).collect();
            Some(format!("  BOOK {label} b{b} {excess:+.1} pts/yr | {}", names.join(" ")))
        };
        let lines: Vec<(bool, String)> =
            calendar.iter().map(|&b| book(b).map_or_else(|| (false, format!("  BOOK {label} b{b} empty")), |l| (true, l))).collect();
        if lines.iter().any(|l| l.0) {
            out.extend(lines.into_iter().map(|l| l.1));
        }
    }
    out
}

/// (#874) The coin cohort the main book never ranks, scored on its own samples and benchmarked by `bench`,
/// joins `all` so `class_book_lines` can rank it as its own class. Returns the coin tickers, which the
/// caller's class closure needs: `value_floor_trim`'s rows carry no asset class.
fn merge_coin_cohort<'a>(
    all: &mut BTreeMap<i32, Vec<(f64, f64, f64, String)>>,
    coins: &BTreeMap<i32, Vec<&'a Sample>>,
    bench: impl Fn(chrono::NaiveDate) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> HashSet<&'a str> {
    let mut names = HashSet::new();
    for (b, group) in coins {
        let quotes: Vec<&Quote> = group.iter().map(|s| s.quote.as_ref()).collect();
        for (s, scored) in group.iter().copied().zip(picks::growth_scores_ranked(&quotes, tuning)) {
            let (Some(score), Some(bench_r)) = (scored, bench(s.date)) else { continue };
            all.entry(*b).or_default().push((score, s.realized, bench_r, s.quote.ticker.clone()));
            names.insert(s.quote.ticker.as_str());
        }
    }
    names
}

/// Cumulative % return of a benchmark series held `years` from the first session on/after `from`. `None`
/// if `from` predates the series or no full forward window remains — mirrors the ticker walk in `sweep_cutoffs`.
fn benchmark_fwd(dates: &[chrono::NaiveDate], closes: &[f64], from: chrono::NaiveDate, years: i64) -> Option<f64> {
    let i = dates.iter().position(|d| *d >= from)?;
    let target = dates[i] + chrono::Duration::days(years * 365);
    let off = dates[i..].iter().position(|d| *d >= target)?;
    let r = (closes[i + off] / closes[i] - 1.0) * 100.0;
    r.is_finite().then_some(r)
}

/// (r40) Cumulative % return of a benchmark series over the `years` ENDING at `date` — the trailing
/// mirror of `benchmark_fwd`, for the relative-strength probe. Anchors on the last session `≤ date`
/// and the last session `≤ date − years`; `None` if either is missing (the series doesn't reach back
/// a full window before the cutoff). Trailing, not forward, so it can't peek at the holdout it grades.
fn bench_trailing(dates: &[chrono::NaiveDate], closes: &[f64], date: chrono::NaiveDate, years: i64) -> Option<f64> {
    let end = dates.iter().rposition(|d| *d <= date)?;
    let start_date = dates[end] - chrono::Duration::days(years * 365);
    let start = dates[..=end].iter().rposition(|d| *d <= start_date)?;
    let r = (closes[end] / closes[start] - 1.0) * 100.0;
    r.is_finite().then_some(r)
}

/// (#326) The trailing window the index's percentile is measured over, matching the "~10y history"
/// `Quote::range_pct` is documented as.
const REGIME_RANGE_YEARS: i64 = 10;

/// (#326) The benchmark's OWN `range_pct` at `date`: `core::price_pct_rank` — the very function that
/// fills `Quote::range_pct` — over the index closes in the trailing [`REGIME_RANGE_YEARS`] window
/// ending there. The window matters more than it looks: `range_pct` is documented as a percentile in a
/// name's "~10y history", and over MAX history an exponentially-growing index ranks high even at the
/// bottom of a crash (most of its closes are ancient and far lower), so a MAX-window percentile would
/// leave the valve shut exactly when it should open. Trailing-only, like `bench_trailing`.
fn bench_range_pct(dates: &[chrono::NaiveDate], closes: &[f64], date: chrono::NaiveDate) -> Option<f64> {
    let end = dates.iter().rposition(|d| *d <= date)?;
    let first = dates[end] - chrono::Duration::days(REGIME_RANGE_YEARS * 365);
    let start = dates[..=end].iter().position(|d| *d >= first)?;
    (end > start).then(|| crate::core::price_pct_rank(&closes[start..=end]))
}

/// (#326) Stamp each sample's quote with the market's state at ITS OWN cutoff, so the regime valve can
/// read it from a pure `(&Quote, &BuyHeuristic)` gate. Computed once per distinct cutoff date (the walk
/// emits the same ~6-month grid for thousands of tickers) and `Arc::make_mut` does not clone here,
/// since the walk just built each quote and nothing else holds a reference yet.
fn stamp_regime(samples: &mut [Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>)) {
    let (bd, bc) = bench;
    let mut seen: BTreeMap<chrono::NaiveDate, (Option<f64>, Option<f64>)> = BTreeMap::new();
    for s in samples.iter_mut() {
        let (r1y, rng) = *seen
            .entry(s.date)
            .or_insert_with(|| (bench_trailing(bd, bc, s.date, 1), bench_range_pct(bd, bc, s.date)));
        let q = std::sync::Arc::make_mut(&mut s.quote);
        q.bench_1y_pct = r1y;
        q.bench_range_pct = rng;
    }
}

/// (round 108) Benchmark's % below its running high at the last session on/before `date` (≤ 0; 0 = at
/// a fresh high). None when `date` predates the series. The ENTRY-STATE classifier: what the market
/// looked like the day money went in.
fn bench_drawdown_at(dates: &[chrono::NaiveDate], closes: &[f64], date: chrono::NaiveDate) -> Option<f64> {
    let mut hi = f64::MIN;
    let mut last = None;
    for (d, c) in dates.iter().zip(closes) {
        if *d > date {
            break;
        }
        hi = hi.max(*c);
        last = Some(*c);
    }
    last.map(|c| (c / hi - 1.0) * 100.0)
}

/// (#140) The GRADED redundancy skip — the twin of the `picks::lane_split` `growth_corr_cap` trim,
/// sharing `core::decorrelate_keep` so the served rule, the CORR-CAP probe and this one cannot
/// disagree about what "correlated" means ((#3j)/(#41)). Keep the best-scoring rows whose trailing
/// correlation with every already-kept row stays under `cap`, refilling from below, and stop at `n`.
///
/// PER RUNG, NOT PER COHORT, and that is the whole design: a correlation cap is a book-CONSTRUCTION
/// rule whose answer depends on how many names you are buying, so `n` is always the rung being built
/// — the same `n` the live site passes (the table size) and the probe passes (`VERDICT_TOP`). The
/// (#75) value brake trims the cohort instead, because a percentile is a statement about the pool.
///
/// `cap <= 0.0` is the IDENTITY (plain top-`n`), which is what ships, and the untouched
/// `tests/fixture/*.golden` are the proof. Unjudgeable pairs never block — `decorrelate_keep` uses
/// `is_some_and`, so an empty trail is kept, the house rule every gate follows.
///
/// Pure, and split out for exactly the reason `value_floor_trim` is: [`report_vs_benchmark`] returns
/// `()` and only prints, so a comparison left inline there is unreachable by any test ((#75)).
fn corr_cap_rung<T: Clone>(rows: &[T], trails: &[&[f64]], n: usize, cap: f64) -> Vec<T> {
    if cap <= 0.0 {
        return rows[..n.min(rows.len())].to_vec();
    }
    core::decorrelate_keep(trails, n, cap).into_iter().map(|i| rows[i].clone()).collect()
}

/// (#558) The RANKED ruler's terminal multiple: the book's `k` multiples in score order, rank `r`
/// (1-based) weighted `(k + 1 - r) / (k(k+1)/2)`, so a full top-10 weighs 10..1 over 55 and a thinner
/// pool renormalises over the names it has. Unlike the equal-weight book it is NOT set-invariant
/// ((#149)): a reorder inside the basket moves it, which is the point. Pure for the reason above.
fn ranked_multiple(m: &[f64]) -> f64 {
    let k = m.len() as f64;
    m.iter().enumerate().map(|(r, x)| (k - r as f64) * x).sum::<f64>() / (k * (k + 1.0) / 2.0)
}

/// (#149) THE RULER'S OWN BLIND SPOT, reported so no receipt can bank a free pass off it. The
/// ABSOLUTE top-N table grades an EQUAL-WEIGHT basket, which is SET-invariant: reordering names
/// INSIDE the basket cannot move one number it prints. A ranking knob is therefore only measurable
/// through the names that CROSS the basket boundary — and a bucket whose gated pool is no larger
/// than the basket has nobody outside to cross it. Such a bucket prints an identical row at every
/// setting of every ranking knob, and its "held" is arithmetic rather than evidence. That is the
/// (#126)/(#145) vacuity standard applied to the verdict basket itself.
///
/// THIS IS NOT (#120)'S QUESTION AND MUST NOT BE ANSWERED ITS WAY. (#120) asked which basket
/// MAXIMISES the mean, found 3 was an unhaircut 13-way argmax over [`TOP_LADDER`], and pinned
/// [`VERDICT_TOP`] to the comparison basket precisely to delete a fitted parameter. Re-selecting a
/// rung here because it happens to peak would be that same forbidden move, on the same ladder and
/// the same data. So this reports and changes nothing: no basket moves, no pick moves. Whether a
/// column CAN respond is a VALIDITY question, not a fitting one — the precedent is (#139), which
/// already declares 20y unable to adjudicate a MEAN on `pit` while a MAX (worst window) and a COUNT
/// (rank-1 h2h) still answer there. The same split applies to a VACUOUS basket.
///
/// Returns (saturated buckets, substitutable names, median pool, vacuous). VACUOUS is DEFINED, not
/// tuned: the MEDIAN bucket is saturated — a strict majority of graded buckets hold no name outside
/// the basket at all. `pool <= basket` is a definition and "the median bucket" is a one-line rule,
/// so there is no threshold here for a later round to fit.
fn basket_vacuity(pools: &[usize], basket: usize) -> (usize, usize, usize, bool) {
    let saturated = pools.iter().filter(|&&p| p <= basket).count();
    let substitutable = pools.iter().map(|&p| p.saturating_sub(basket)).sum();
    let mut sorted = pools.to_vec();
    sorted.sort_unstable();
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    (saturated, substitutable, median, saturated * 2 > pools.len())
}

/// (#968) The benchmark is the S&P 500 total-return index whenever the run meters total return, through
/// the selection (`use_adjusted_close`) or the ruler alone (#804). `run` fetched ^SP500TR on either while
/// `report_vs_benchmark` named it ^GSPC under the ruler alone; both now read this one test.
fn bench_is_tr(adjusted_close: bool, tuning: &BuyHeuristic) -> bool {
    adjusted_close || tuning.backtest_tr_ruler
}

/// (#1082) The skip line when fewer than 8 gated picks have a benchmark window. It names the series
/// `run` fetched, as the rest of `report_vs_benchmark` does; under a total-return run it said ^GSPC.
fn bench_too_few(rows: usize, bench_sym: &str) -> Option<String> {
    if rows < 8 {
        Some(format!("\n── vs S&P500 (ABSOLUTE) ──  only {rows} gated picks have a {bench_sym} window — too few."))
    } else {
        None
    }
}

/// (#40) ABSOLUTE goal metric — the one the peer-relative lanes never measure. The stated purpose is
/// "out-return an S&P500 buy-and-hold", but every lane above de-means the level away (SELECTION, not the
/// index). This asks the real question: buy the top-N growth picks (equal-weight, non-crypto), hold
/// `years`, and does that beat holding ^GSPC over the SAME window? Per pick, excess = its annualized
/// return minus what ^GSPC did from the same cutoff. Top-N per ~6mo bucket; report mean pick/SPY CAGR,
/// excess, win-rate, worst bucket, and the early-vs-late OOS split. Read the STRESS run: the picks come
/// from today's survivors (biased UP), ^GSPC is the true index, so a non-stress win is optimistic.
fn report_vs_benchmark(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    // name the benchmark honestly: the series `run` fetched, by the same test it fetched it with
    let bench_sym = if bench_is_tr(crate::config::use_adjusted_close(), tuning) { "^SP500TR total-return" } else { "^GSPC" };
    if bd.len() < 2 {
        println!("\n── vs S&P500 (ABSOLUTE) ──  no {bench_sym} history fetched — skipping the benchmark leg.");
        return;
    }
    // (bucket, score, pick CAGR, SPY CAGR, ticker, as-of peg_yield) for every GATED non-crypto pick that
    // has a benchmark window. The peg rides along ONLY for the (#75) value brake below; nothing else reads it.
    let mut rows: Vec<(i32, f64, f64, f64, String, Option<f64>)> = Vec::new();
    // (#140) trailing returns for the graded `growth_corr_cap` skip, keyed by (bucket, ticker) because
    // a trail is per-SAMPLE, not per-ticker — the same name at two cutoffs carries two histories.
    // (#790) The walk cuts each series once per bucket, so a (bucket, ticker) pair repeats only when
    // two series share a ticker, and then the later trail wins.
    //
    // Built UNCONDITIONALLY, and that is deliberate. An `if cap > 0.0` guard here saves a borrowed
    // slice per sample on the shipped lane and costs a mutant nobody can kill: at `cap: 0.0` the map
    // is never read, so `>` `==` `<` `>=` all produce byte-identical output and the gate reds with
    // six survivors. `report_vs_benchmark` returns `()` and only prints, so no unit test can reach
    // the guard either ((#75)). An unobservable branch is not an optimisation, it is dead weight the
    // suite has to pretend to cover — so it is not written.
    // (#557) keyed to the whole Sample, not just its trail: the graded sector cap reads `quote.sector` too.
    let mut sample_of: std::collections::BTreeMap<i32, std::collections::HashMap<&str, &Sample>> = Default::default();
    // (#144) TWO PASSES, because a rank normalisation is CROSS-SECTIONAL and the pool a name is ranked
    // against must be its OWN cutoff window. Ranking a 1998 name against a 2015 one would be lookahead
    // of the plainest kind, so the grouping is not a convenience — it is the correctness condition.
    // This one loop feeds `by_bucket` and therefore EVERY number the ship rule reads: the TOP_LADDER
    // books, `rank_slice_stats` (rank-1, the h2h GUARD, the disjoint slices) and `book_deciles`.
    //
    // BYTE-IDENTICAL ON THE SHIPPED LANE, and by two separate properties rather than one.
    // `growth_scores_ranked` is `growth_score` per name.
    // And the ROW ORDER cannot move either: each bucket's Vec is filled in `samples` order exactly as
    // before, so `value_floor_trim`'s per-bucket Vec is element-for-element what it was — which
    // matters because the rank sort below is STABLE and a reordered tie would silently rebuild a book.
    let mut by_cutoff: std::collections::BTreeMap<i32, Vec<&Sample>> = Default::default();
    let mut coins: std::collections::BTreeMap<i32, Vec<&Sample>> = Default::default(); // (#566) the BOOK crypto lines only
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            coins.entry(bucket(s.date)).or_default().push(s);
            continue; // crypto: a coin isn't an S&P500-comparable hold
        }
        by_cutoff.entry(bucket(s.date)).or_default().push(s);
    }
    // (#564) every bucket with a benchmark window, scored or not: the BOOK lines below print the ones no
    // name reached as `empty`, so two arms can be graded over the same calendar (an empty one = the index).
    let mut calendar = std::collections::BTreeSet::new();
    for (b, group) in &by_cutoff {
        let quotes: Vec<&Quote> = group.iter().map(|s| s.quote.as_ref()).collect();
        for (s, scored) in group.iter().zip(picks::growth_scores_ranked(&quotes, tuning)) {
            let Some(bench_r) = benchmark_fwd(bd, bc, s.date, years) else { continue };
            calendar.insert(*b);
            let Some(score) = scored else { continue };
            sample_of.entry(*b).or_default().insert(s.quote.ticker.as_str(), *s);
            // RAW cumulative % (annualize the BOOK, not per-name)
            rows.push((*b, score, s.realized, bench_r, s.quote.ticker.clone(), s.fund.as_ref().and_then(|f| f.peg_yield)));
        }
    }
    if let Some(line) = bench_too_few(rows.len(), bench_sym) {
        println!("{line}");
        return;
    }
    // (#75) VALUE BRAKE — the GRADED twin of the `picks::lane_split` trim, sharing `core::pct_floor` so
    // the served rule and the measured one cannot drift ((#3j)). It has to run HERE, on the cohort and
    // before any ranking, for the same reason it runs where it does live: it changes WHO IS IN the pool,
    // so the top-N table AND the rank-slice ladder below must both see the trimmed cohort — grading one
    // on a pool the other never saw is how a knob ships on a number nothing served. BTreeMap -> buckets
    // iterate in chronological order, so the OOS split below is early-vs-late in time.
    let by_bucket = value_floor_trim(&rows, tuning.growth_value_floor_pct);
    println!(
        "\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD {years}y no-sell, vs {bench_sym}){} ──",
        best_of_tag(tuning.print_selection_count, TOP_LADDER.len())
    );
    let pools: Vec<usize> = by_bucket.values().map(|v| v.len()).collect();
    let (sat, subs, med_pool, vacuous) = basket_vacuity(&pools, VERDICT_TOP);
    // (#151) print the h2h denominator HERE, beside saturation, because it IS the same number:
    // `h2h_beats(vv, 10, 20)` returns None exactly when `vv.len() <= 10`, and VERDICT_TOP is 10, so
    // the rank-1 GUARD tallies precisely the buckets this census calls non-saturated. Measured 6/6
    // across the (#151) grid. A reader who sees "31 saturated of 35" now knows, without subtracting,
    // that the GUARD two lines down is answering off n=4.
    let h2h_n = pools.len() - sat;
    println!(
        "  POOL CENSUS (#149): {} bucket(s), median gated pool {med_pool}, graded basket {VERDICT_TOP} -> {sat} saturated (pool <= basket), {subs} substitutable name(s); the rank-1 h2h GUARD below reads off the other {h2h_n}",
        pools.len()
    );
    if vacuous {
        println!(
            "  POOL CENSUS (#149): VACUOUS — the median bucket has nobody outside the basket, so a RANKING knob CANNOT be adjudicated on this column; record VACUOUS, never \"held\". Gates (pool-changing) and MAX/COUNT statistics still answer, but (#151) AMENDS the rank-1 h2h out of that exemption: its denominator is the {h2h_n} non-saturated bucket(s) above, so on a saturated column it is not unaffected — it is a COUNT over the residue. Read that n before leaning on the GUARD."
        );
    }
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    // (#1155) each bucket's top-1% winners, cut from the whole non-crypto pool the gates chose from,
    // and per rung, per booked bucket, whether the book below holds one of them
    let winners: BTreeMap<i32, HashSet<&str>> = by_cutoff
        .iter()
        .map(|(b, g)| (*b, top_winners(g, 0.01)))
        .collect();
    let mut in_book: Vec<(usize, Vec<bool>)> = Vec::new();
    // (#41/#43) EQUAL-WEIGHT HELD-BOOK return — the correct metric for a no-sell hold. A held book earns
    // ann(mean of terminal MULTIPLES), NOT mean of per-name CAGRs: a 20× winner in the book covers twenty
    // −100% zeros, and a name that goes to 0 contributes its full weight lost (1/N), not its scary CAGR.
    // Also count "zeros ridden" (names ≤−90% you must hold through) to show the no-sell tail you survive.
    let mut m10: Option<f64> = None; // top-10 mean terminal multiple, feeds the after-tax footer below
    let mut census: Vec<(usize, usize)> = Vec::new(); // (#557) per bucket: top-10 (stock slots, of them tagged)
    let mut book_lines = std::collections::BTreeMap::new(); // (#564) per bucket: the verdict book, names + cum %
    for &n in TOP_LADDER {
        let (mut book, mut spy, mut excess) = (Vec::new(), Vec::new(), Vec::new());
        let mut hits = Vec::new();
        let (mut zeros, mut held) = (0usize, 0usize);
        let mut zero_names: Vec<String> = Vec::new(); // (#zeros) names ≤−90% at top-10 -> union across horizons = true distinct death count
        let mut multiples: Vec<f64> = Vec::new();
        let mut ranked: Vec<f64> = Vec::new(); // (#558) per bucket: rank-weighted book minus bench, pts/yr
        for (b, v) in &by_bucket {
            let mut vv = v.clone();
            vv.sort_by(|a, b| b.0.total_cmp(&a.0)); // score desc
            let sample = |t: &str| sample_of.get(b).and_then(|m| m.get(t)).copied();
            // (#557) the graded twin of the live `picks::cap_book`, through the same `sector_cap`, so a
            // capped name refills from below. Identity at the shipped 0. Like the corr skip below, it
            // leaves `rank_slice_stats` and `book_deciles` untrimmed, so their h2h GUARD is vacuous here.
            let vv = picks::sector_cap(vv, tuning.growth_sector_cap, |x| sample(x.3.as_str()).and_then(|s| picks::stock_sector(&s.quote)));
            // (#140) the graded redundancy skip. At the shipped `growth_corr_cap: 0.0` this is the
            // identity and every number below is unchanged; armed, it rebuilds THIS rung from the
            // ranked list, dropping a name that duplicates a bet already in the book and refilling
            // from below. NOTE what it deliberately does NOT touch: `rank_slice_stats` and
            // `book_deciles` below still read the untrimmed `by_bucket`, so the rank-1 h2h GUARD is
            // measured on the same cohort either way and cannot move for this knob — a vacuous pass,
            // and the receipt must say so rather than bank it as evidence.
            let trails: Vec<&[f64]> = vv.iter().map(|x| sample(x.3.as_str()).map_or(&[][..], |s| s.trail.as_slice())).collect();
            let p = corr_cap_rung(&vv, &trails, n, tuning.growth_corr_cap);
            if n == VERDICT_TOP {
                let stocks: Vec<&Sample> = p.iter().filter_map(|x| sample(x.3.as_str())).filter(|s| picks::asset_class(&s.quote) == 2).collect();
                census.push((stocks.len(), stocks.iter().filter_map(|s| picks::stock_sector(&s.quote)).count()));
            }
            let take = p.len();
            if take == 0 {
                continue;
            }
            hits.push(
                winners
                    .get(b)
                    .is_some_and(|w| p.iter().any(|x| w.contains(x.3.as_str()))),
            );
            let book_cum = mean(&p.iter().map(|x| 1.0 + x.1 / 100.0).collect::<Vec<_>>()); // equal-weight terminal multiple
            let spy_cum = mean(&p.iter().map(|x| 1.0 + x.2 / 100.0).collect::<Vec<_>>());
            multiples.push(book_cum);
            book.push(ann((book_cum - 1.0) * 100.0, years));
            spy.push(ann((spy_cum - 1.0) * 100.0, years));
            excess.push(*book.last().unwrap() - *spy.last().unwrap());
            if n == VERDICT_TOP {
                // the bench takes the same rank weights, so a name's own-window index leg stays paired with it
                let w = |r: Vec<f64>| ann((ranked_multiple(&r) - 1.0) * 100.0, years);
                ranked.push(w(p.iter().map(|x| 1.0 + x.1 / 100.0).collect()) - w(p.iter().map(|x| 1.0 + x.2 / 100.0).collect()));
                // (#564) never spell `markers::EXCESS`, `COHORT_N` or `TICKERS` here: the gate parses those words.
                let names: Vec<String> = p.iter().map(|x| format!("{} {:+.0}%", x.3, x.1)).collect();
                book_lines.insert(*b, format!("  BOOK b{b} {:+.1} pts/yr | {}", excess.last().unwrap(), names.join(" ")));
            }
            zeros += p.iter().filter(|x| x.1 <= -90.0).count();
            if n == 10 {
                zero_names.extend(p.iter().filter(|x| x.1 <= -90.0).map(|x| format!("{}@b{b}({:.0}%)", x.3, x.1)));
            }
            held += take;
        }
        in_book.push((n, hits));
        if n == 10 && !zero_names.is_empty() {
            // (#466) Never lead with `markers::VERDICT_ROW`: this prints BEFORE the book row, so a
            // `top-10 zero names` spelling was the line the gate parsed, found no `excess`, and red.
            println!("  zero names in top-10 ({years}y): {}", zero_names.join(", "));
        }
        if n == VERDICT_TOP {
            // (#557) the cap judges only these tagged slots. `sector_of` is TODAY's GICS label, and `pit`'s
            // delisted ever-members carry none, so on that lane the cap can see less of the book.
            println!(
                "  SECTOR CENSUS (#557): {} of {} top-{n} stock slots carry a GICS sector, the only ones growth_sector_cap {} can judge",
                census.iter().map(|c| c.1).sum::<usize>(),
                census.iter().map(|c| c.0).sum::<usize>(),
                tuning.growth_sector_cap
            );
        }
        let m = excess.len();
        if m == 0 {
            continue;
        }
        if n == 10 {
            m10 = Some(mean(&multiples));
        }
        let win = excess.iter().filter(|e| **e > 0.0).count() as f64 / m as f64 * 100.0;
        let worst = excess.iter().cloned().fold(f64::INFINITY, f64::min);
        let cut = m / 2;
        let (early, late) = (mean(&excess[..cut]), mean(&excess[cut..]));
        println!(
            "  top-{n:<2} book {:+.1}%/yr  vs S&P500 {:+.1}%/yr  ->  excess {:+.1} (med {:+.1}) pts/yr   win {win:.0}% of {m}   worst {worst:+.1}   OOS {early:+.1}/{late:+.1}   rode {zeros} zeros/{held} holds",
            mean(&book), mean(&spy), mean(&excess), median(excess.clone())
        );
        // (#315) the buy lane's promise graded forward: the share of start dates whose top-10 book beat Série E's best
        // case over the same years. CI ratchets this share (tests/network.rs); here it is a row, not a gate.
        if n == VERDICT_TOP {
            let k = crate::core::beats_serie_e(&multiples, years as f64);
            println!(
                "  {} {:.0}% of {m} windows ({k}) at {:.1}%/yr   median book {:.2}× vs {:.2}×",
                markers::SERIE_E,
                100.0 * k as f64 / m as f64,
                crate::core::serie_e_best_pct(),
                median(multiples.clone()),
                crate::core::serie_e_multiple(years as f64)
            );
            // (#558) the same buckets, weighted 10..1 by rank: a ruler that sees a reorder inside the book.
            // Never lead with `markers::VERDICT_ROW` (#466); the gate keeps reading the equal-weight row.
            println!(
                "  RANKED top-{n} (#558, weights {n}..1)  excess {:+.1} (med {:+.1}) pts/yr   worst {:+.1}",
                mean(&ranked),
                median(ranked.clone()),
                ranked.iter().cloned().fold(f64::INFINITY, f64::min)
            );
            // (#564) one line per calendar bucket, `empty` where no name was booked (the index you'd buy
            // instead), so an admission arm and its control are graded over the same buckets offline.
            for b in &calendar {
                println!("{}", book_lines.get(b).cloned().unwrap_or_else(|| format!("  BOOK b{b} empty")));
            }
            // (#566) coins never join the book above, so their line ranks them as their own cohort, untrimmed
            // (no P/E for the value floor), over the same calendar
            let mut all = by_bucket.clone();
            let coin_names = merge_coin_cohort(&mut all, &coins, |d| benchmark_fwd(bd, bc, d, years), tuning);
            let class_of = |b: i32, t: &str| {
                if coin_names.contains(t) { Some(0) } else { sample_of.get(&b).and_then(|m| m.get(t)).map(|s| picks::asset_class(&s.quote)) }
            };
            class_book_lines(&all, &calendar, class_of, n, years).iter().for_each(|l| println!("{l}"));
        }
    }
    if let Some(line) = winner_in_book_line(&in_book) {
        println!("{line}");
    }
    // (#106) (round 3 §4) the DISTRIBUTION behind the ladder above. Every row printed so far quotes a
    // MEAN across buckets, and `VERDICT_TOP` is the argmax of one of those means over these 13 rungs.
    // Under positive skew small N maximises the mean while wrecking the median and the left tail, so
    // that argmax picks the most concentrated book almost by construction. One row per rung, so the
    // basket size can be re-derived off the median and the 10th percentile instead.
    if tuning.print_book_deciles {
        println!("  terminal-multiple distribution across windows (the MEAN above is one number out of these):");
        // the ladder's rows carry a 4th element (the ticker, for the zero-names line); `book_deciles`
        // shares its arithmetic with `book_stats`, which is keyed on the 3-element row — drop the name.
        let ranked: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> =
            by_bucket.iter().map(|(b, v)| (*b, v.iter().map(|x| (x.0, x.1, x.2)).collect())).collect();
        for &n in TOP_LADDER {
            if let Some((d, below_index, below_one, m)) = book_deciles(&ranked, n) {
                println!(
                    "    top-{n:<2} d10 {:.2}×  median {:.2}×  d90 {:.2}×   P(book < index) {:.0}%   P(book < 1.0×) {:.0}%   of {m}",
                    d[0],
                    d[4],
                    d[8],
                    below_index * 100.0,
                    below_one * 100.0
                );
            }
        }
    }
    // (#45) RANK-SLICE ladder + same-window head-to-head. The cumulative top-N table above cannot
    // answer "is #1 really better than #20": ann(mean of multiples) mechanically favors bigger books
    // on fat-tailed outcomes (a 10-draw mean usually catches one lottery ticket, a 1-draw book
    // can't), so top-1 trailing top-10 is diversification math, not a ranking verdict. DISJOINT
    // slices and a direct same-window compare are the honest order test: if ranks 1 and 2-5 don't
    // beat 11-20 HERE, the order at the top of the screen carries no signal.
    let (slices, h2h_mid, h2h_low) = rank_slice_stats(&by_bucket);
    let (h1, h25, hn) = (h2h_mid.h1, h2h_mid.h25, h2h_mid.n);
    println!("  rank-slice (DISJOINT books, excess vs {bench_sym}; mean|median across windows — median is lottery-ticket-immune):");
    for (label, excess) in &slices {
        if excess.is_empty() {
            continue;
        }
        let ex_ann: Vec<f64> = excess.iter().map(|(bk, sp)| ann(*bk, years) - ann(*sp, years)).collect();
        let win = ex_ann.iter().filter(|e| **e > 0.0).count() as f64 / ex_ann.len() as f64 * 100.0;
        println!(
            "    rank {label:<6} excess {:+.1}|{:+.1} pts/yr   win {win:.0}% of {}{}",
            mean(&ex_ann),
            median(ex_ann.clone()),
            ex_ann.len(),
            n_eff_tag(tuning.print_n_eff, ex_ann.len(), years)
        );
    }
    if hn > 0 {
        println!(
            "    head-to-head same-window (no averaging artifact): #1 beat the 11-20 book in {h1}/{hn} ({:.0}%), the 2-5 book did in {h25}/{hn} ({:.0}%) — >50% = the top of the list is genuinely better than its middle",
            h1 as f64 / hn as f64 * 100.0,
            h25 as f64 / hn as f64 * 100.0
        );
    }
    // (#135) the SAME head-to-head against the 6-10 book. The line above needs 11 names in a window
    // and (#124) measured that the gates admit no more than ~10, so on the PIT lane its denominator
    // collapses to a handful (#130) and Ship Rule v2's rank-1 guard cannot be computed there at all —
    // which is why P0b has never run. This one needs 6. Printed BESIDE the shipped number and never
    // instead of it: the two DENOMINATORS side by side are the actual reading of the admission ceiling.
    if let Some(line) = h2h_low_line(tuning.print_h2h_mid_ladder, h2h_low, hn) {
        println!("{line}");
    }
    // (Phase B) the never-sell tax edge, made visible: a hold pays capital-gains ONCE at the final
    // sale, a yearly-rotation strategy on the SAME pre-tax path pays tax on each year's gain.
    if let Some(m) = m10.filter(|m| *m > 1.0) {
        // (#108) the two arms hold for different lengths of time, so under a holding-period schedule
        // they pay different rates: the never-sell arm is held for the whole window, the rotation arm
        // for a year by definition and so never past the first rung. On the shipped EMPTY schedule
        // both resolve to the headline rate and this line is byte-identical to the flat-rate one.
        let base = tuning.capital_gains_tax_pct / 100.0;
        let sched = &tuning.cgt_hold_schedule;
        let (t_hold, t_rot) = (cgt_rate(years, base, sched), cgt_rate(1, base, sched));
        let (never, rot) = after_tax_pair(m, years, t_hold, t_rot);
        let sched_tag = if sched.is_empty() {
            String::new()
        } else {
            format!(
                " [held {years}y taxed at {:.1}%, rotation at {:.1}%]",
                t_hold * 100.0,
                t_rot * 100.0
            )
        };
        println!(
            "  after-tax ({:.0}% PT, top-10 book): never-sell {never:+.1}%/yr vs yearly-rotation {rot:+.1}%/yr -> deferral edge {:+.1} pts/yr{sched_tag}",
            tuning.capital_gains_tax_pct,
            never - rot
        );
    }
    println!("  (BOOK = equal-weight terminal wealth annualized (winners carry it, a zero costs its 1/N weight); >0 beats S&P500.");
    println!("   NON-stress: picks are today's survivors (biased UP) vs the true index — run `stress` for the honest excess.)");
}

/// (#75) The value brake's cohort trim, pure for testability for a reason worth recording: its first
/// mutation audit reported SIX survivors on the two comparisons below, and every one of them was
/// unkillable rather than merely unkilled. [`report_vs_benchmark`] returns `()` and only prints, and
/// only the fund goldens run the `fund` lane this brake needs to see a single `peg_yield` (frozen SEC
/// rows since (#364)), which the gate skips ((#590)), so no test it runs could reach the comparisons
/// in place. Split out, they take arguments and answer with a value, which is the whole difference.
/// ((#997) this used to say the knob ships at `0.0`, true until (#126) shipped 40.0, and that `fund`
/// cannot run offline.)
///
/// ONE FLOOR PER BUCKET, never pooled: the floor is a cross-sectional statement about a single
/// rebalance window, and a pooled percentile would let a cheap 2009 cohort exile an expensive 2021 one
/// wholesale. Computed once per bucket rather than per row — `pct_floor` sorts, and the 20y run carries
/// ~20k rows. There is deliberately NO `pct > 0.0` guard: `pct_floor` already answers `None` at or
/// below zero, so the guard was a second copy of that decision, and at the shipped default it was
/// invisible to any test — three of those six survivors were nothing but its redundancy.
///
/// Names with no `peg_yield` are KEPT — unjudgeable is not a verdict, matching the live site and the
/// `drop_bottom_book` probe this knob came from. `< floor` rejects, so the boundary matches that
/// probe's `if v < t { skip }` and at reject-P this book reproduces the PEG-VALUE-GATE probe's own row.
fn value_floor_trim(rows: &[(i32, f64, f64, f64, String, Option<f64>)], pct: f64) -> BTreeMap<i32, Vec<(f64, f64, f64, String)>> {
    let mut by_peg: BTreeMap<i32, Vec<f64>> = BTreeMap::new();
    for (b, _, _, _, _, peg) in rows {
        by_peg.entry(*b).or_default().extend(peg);
    }
    let floors: BTreeMap<i32, Option<f64>> = by_peg.into_iter().map(|(b, vals)| (b, core::pct_floor(vals, pct))).collect();
    let mut by_bucket: BTreeMap<i32, Vec<(f64, f64, f64, String)>> = BTreeMap::new();
    for (b, sc, pc, spc, tk, peg) in rows {
        if let (Some(Some(f)), Some(v)) = (floors.get(b), peg) {
            if v < f {
                continue; // dearest for its growth in THIS window -> the brake rejects it
            }
        }
        by_bucket.entry(*b).or_default().push((*sc, *pc, *spc, tk.clone()));
    }
    by_bucket
}

type SliceStats = (Vec<(&'static str, Vec<(f64, f64)>)>, H2h, H2h);

/// (#135) head-to-head tallies against ONE comparison book: windows where the #1 name beat it, windows
/// where the 2-5 book beat it, and `n` — the windows big enough to hold that book AT ALL. `n` is the
/// guard's denominator and it is the half that starves: the 11-20 book needs 11 names in a window, and
/// (#124) measured that the gates admit no more than ~10, which is why the PIT lane's rank-1 h2h rests
/// on 2/4/6 windows (#130) and P0b has never been runnable there.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct H2h {
    h1: usize,
    h25: usize,
    n: usize,
}

/// One window's verdict against the comparison book at 0-based ranks `lo..hi`, on raw realized % (mean
/// of multiples is monotone in mean of %, so ordering within one window needs no annualization).
/// None = the window does not reach `lo`, and that must count as neither a win nor a loss — booking it
/// either way is exactly how a short book fakes a number. Both call sites pass `lo >= 5`, which is what
/// makes the `vv[1..5]` slice below safe.
fn h2h_beats(vv: &[(f64, f64, f64, String)], lo: usize, hi: usize) -> Option<(bool, bool)> {
    if vv.len() <= lo {
        return None;
    }
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let mid = mean(&vv[lo..hi.min(vv.len())].iter().map(|x| x.1).collect::<Vec<_>>());
    Some((vv[0].1 > mid, mean(&vv[1..5].iter().map(|x| x.1).collect::<Vec<_>>()) > mid))
}
/// (#135) the second head-to-head line, built as a VALUE rather than printed inline so the condition
/// that decides whether it appears is testable — the module's standing idiom (`rank_slice_stats` and
/// `h2h_beats` are pure for the same reason). `None` = stay silent, and both reasons are real: the
/// knob is off, or NO window reaches the 6-10 book, in which case the percentage would be 0/0 = NaN.
fn h2h_low_line(on: bool, low: H2h, hn: usize) -> Option<String> {
    if !on || low.n == 0 {
        return None;
    }
    Some(format!(
        "    head-to-head vs the 6-10 book (#135): #1 beat it in {}/{} ({:.0}%), the 2-5 book did in {}/{} ({:.0}%) — denominator {} here vs {} for the 11-20 line above",
        low.h1,
        low.n,
        low.h1 as f64 / low.n as f64 * 100.0,
        low.h25,
        low.n,
        low.h25 as f64 / low.n as f64 * 100.0,
        low.n,
        hn
    ))
}
/// (#45) DISJOINT rank-slice books + same-window head-to-head, pure for testability. Per window
/// (bucket): sort by score desc; slice `lo..hi` of that order becomes its own equal-weight book;
/// each slice collects per-window (book cum %, bench cum %) pairs — the caller annualizes, so this
/// fn stays years-free. Returns TWO head-to-head tallies over the same windows: `mid` is the shipped
/// guard (#1 / the 2-5 book vs the 11-20 book, windows of ≥11 names) and `low` is (#135)'s same
/// question against the 6-10 book, which needs only 6. Both come from this one pass so the pair can
/// never differ by grid drift. (#160) the ship rule reads `low` (the 6-10 book) at 12y and 8y, and
/// `mid` (11-20) at 20y only — both are printed at every horizon regardless.
fn rank_slice_stats(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64, String)>>) -> SliceStats {
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    const SLICES: [(usize, usize, &str); 5] =
        [(0, 1, "1"), (1, 5, "2-5"), (5, 10, "6-10"), (10, 20, "11-20"), (20, 50, "21-50")];
    let mut out: Vec<(&'static str, Vec<(f64, f64)>)> = SLICES.iter().map(|(_, _, l)| (*l, Vec::new())).collect();
    let (mut mid, mut low) = (H2h::default(), H2h::default());
    for v in by_bucket.values() {
        let mut vv = v.clone();
        vv.sort_by(|a, b| b.0.total_cmp(&a.0)); // score desc — the screen's own order
        for ((lo, hi, _), (_, series)) in SLICES.iter().zip(out.iter_mut()) {
            if vv.len() <= *lo {
                continue; // slice starts past this window's pool — no fake short book
            }
            let p = &vv[*lo..(*hi).min(vv.len())];
            let bk = (mean(&p.iter().map(|x| 1.0 + x.1 / 100.0).collect::<Vec<_>>()) - 1.0) * 100.0;
            let sp = (mean(&p.iter().map(|x| 1.0 + x.2 / 100.0).collect::<Vec<_>>()) - 1.0) * 100.0;
            series.push((bk, sp));
        }
        // (#45) the shipped guard: #1 vs the 11-20 book. (#135) the SAME question against the 6-10
        // book, which a ~10-name window can still answer — tallied beside it, never instead of it, and
        // from this one run so the pair cannot differ by grid drift.
        for (tally, (lo, hi)) in [(&mut mid, (10usize, 20usize)), (&mut low, (5usize, 10usize))] {
            if let Some((won_1, won_25)) = h2h_beats(&vv, lo, hi) {
                tally.n += 1;
                tally.h1 += usize::from(won_1);
                tally.h25 += usize::from(won_25);
            }
        }
    }
    (out, mid, low)
}

/// Median of an owned sample. Callers skip empty slices, so the 0-length arm never reaches a print.
/// (#815) Every backtest sort keys on `total_cmp`, as the live screen does: `partial_cmp().unwrap()`
/// panicked the whole run on one NaN score (`discount_cap: 0` makes `discount / 0`).
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let k = v.len();
    if k == 0 {
        return f64::NAN;
    }
    if k % 2 == 1 { v[k / 2] } else { (v[k / 2 - 1] + v[k / 2]) / 2.0 }
}

/// (#165) Probability that a randomly drawn `win` value exceeds a randomly drawn `lose` one, ties
/// counting a half — the separation column of the held-loser table.
///
/// WHY THIS AND NOT THE RAW MEDIAN GAP that table printed alone until now. The gap is expressed in
/// the FACTOR'S OWN UNITS, and `FUND_FACTORS` does not share units: `peg_yield` is
/// `earnings_yield · CAGR` and runs in the hundreds, `accrual_gap` is a ratio living near 0,
/// `margin_stability` is `-std(net_margin)` in single digits. Ranking factors BY THAT GAP therefore
/// ranked them by scale, which is how `accrual_gap` (+0.00 | -0.08) came to be recorded DEAD beside
/// `peg_yield` (+468.92) in (#159). This statistic is invariant under any MONOTONE transform, so a
/// rescaling, a sign convention or an outlier cannot move it and every factor becomes comparable to
/// every other. Full re-reading of (#159)'s table: the (#165) block in tests/ci-settings.yaml.
///
/// A GATE IS A THRESHOLD, and this is exactly how well the best single threshold on the factor could
/// separate the two cohorts — the question the table is asked, not a proxy for it. 0.5 = no
/// separation; > 0.5 = winners score higher; < 0.5 = losers score higher, so DIRECTION survives and
/// (#159)'s "separates backwards" family stays legible as a value below a half.
///
/// It does NOT authorise a gate. It says a threshold is possible; Ship Rule v2 still decides.
///
/// O(n·m) deliberately: the cohorts run ~150 x ~30, a sort buys nothing there, and the pair loop is
/// the version whose tie handling is obvious on inspection. NaN compares as "not greater" (the `_`
/// arm) so the result stays finite and deterministic on a poisoned input rather than propagating.
/// Either side empty = 0.5, nothing to separate — `held_loser_factors` drops one-sided factors before
/// calling, so that is a guard rather than a path.
fn auc(win: &[f64], lose: &[f64]) -> f64 {
    if win.is_empty() || lose.is_empty() {
        return 0.5;
    }
    let mut acc = 0.0;
    for w in win {
        for l in lose {
            acc += match w.partial_cmp(l) {
                Some(std::cmp::Ordering::Greater) => 1.0,
                Some(std::cmp::Ordering::Equal) => 0.5,
                _ => 0.0,
            };
        }
    }
    acc / (win.len() * lose.len()) as f64
}

/// (#108) The capital-gains rate a position held `years` pays, given the headline rate `base` (as a
/// fraction) and a holding-period exclusion `schedule`. The WIDEST exclusion among the rungs the hold
/// satisfies wins, so yaml order is irrelevant and a schedule listing only its top rung still works.
///
/// An empty schedule returns `base` unchanged, which is what makes this knob a no-op by default and
/// keeps every golden's after-tax line byte-identical. The exclusion applies to the GAIN, not to the
/// rate — `base × (1 − excl)` is the same arithmetic either way only because the taxable base is the
/// whole gain, which is the case this models and the reason the two forms are written as one here.
fn cgt_rate(years: i64, base: f64, schedule: &[crate::config::CgtRung]) -> f64 {
    let excl = schedule
        .iter()
        .filter(|r| years as f64 >= r.min_years)
        .map(|r| r.excluded_pct.clamp(0.0, 100.0))
        .fold(0.0, f64::max);
    base * (1.0 - excl / 100.0)
}

/// After-tax %/yr of (never-sell, yearly-rotation) for the SAME pre-tax terminal multiple `m` over
/// `years` at gains-tax rates `t_hold` and `t_rot`. Never-sell defers to one final sale: net multiple
/// = 1 + (m−1)(1−t_hold). Rotation realizes each year's gain: after-tax rate = gross annual rate ×
/// (1−t_rot) — a simplification that ignores loss-offset asymmetry, fine for a positive-multiple book.
///
/// (#108) The two rates were ONE argument until a holding-period exclusion made them differ. They are
/// separate because the arms hold for different lengths of time: rotation holds for a year by
/// definition and can never reach a long-hold rung, so passing one rate to both was not a
/// simplification but a statement that the schedule does not exist. Callers pass the same value twice
/// on a flat schedule, which is the shipped state.
fn after_tax_pair(m: f64, years: i64, t_hold: f64, t_rot: f64) -> (f64, f64) {
    let y = years.max(1) as f64;
    let never = ((1.0 + (m - 1.0) * (1.0 - t_hold)).powf(1.0 / y) - 1.0) * 100.0;
    let rot = (m.powf(1.0 / y) - 1.0) * (1.0 - t_rot) * 100.0;
    (never, rot)
}

/// (#560) The `rivals` books, in print order. Row 0 is the shipped score picked by the same plain
/// top-10 as every rival, so the comparison is like for like; the real shipped row (value trim, caps)
/// prints above it in `report_vs_benchmark` and is what the 6-cell tally reads. (e), the random pond,
/// prints last from [`random_books`]. (#571) (h)/(i) rank on `tr_cagr`, the TR-CAGR column's as-of number;
/// (i) keeps only names with a 10Y leg, since `age_years` is None on a backtest quote. (#584) (j) is
/// [`blend_excess`] over row 0 and the momentum side (`picks::momentum_keys`), split by
/// `growth_momentum_slots`; (#598) (k) is the same blend with (h) in place of row 0. (#600) (l) ranks the pond
/// on one key, [`rank_sum`] of row 1 and the momentum key; (m) and (n) are (j) with one half [`double_sort`]ed:
/// (m) keeps the momentum-best half of row 0's top names, (n) the row-1-best half of the momentum side's.
const RIVALS: [&str; 14] = [
    "shipped (plain top-10)",
    "(d) gates-off shipped",
    "(a) momentum 12-1",
    "(b) quality (ROE fallback)",
    "(c) value E/P",
    "(f) revenue CAGR",
    "(g) momentum x quality",
    "(h) TR life CAGR",
    "(i) TR life CAGR, 10Y+",
    "(j) 5 shipped + 5 momentum",
    "(k) 5 TR life + 5 momentum",
    "(l) integrated score + mom",
    "(m) (j), score half by mom",
    "(n) (j), mom half by score",
];

/// (#560) A gap under this many pts/yr is a tie, neither a win nor a loss. Pre-registered.
const RIVAL_TIE: f64 = 0.2;

/// (#560) One cell's win: the rival's mean AND median excess both clear shipped by [`RIVAL_TIE`].
fn rival_wins(d_mean: f64, d_median: f64) -> bool {
    d_mean >= RIVAL_TIE && d_median >= RIVAL_TIE
}

/// (#560) The momentum 12-1 rival's order over one month's pool: the names the gates-off score keys (`open`,
/// the `rivals` pond), highest momentum first. The pool is ticker-ordered and the sort stable, so a tie ranks
/// by ticker. (#587) The key is
/// `picks::momentum_keys` over the pond, so (#607) it ranks stocks only, as the live `picks::momentum_order`.
/// (#609) Keys 3 and 4 read each sample's `trail`.
fn momentum_ranked<'a>(pool: &[&'a Sample], open: &[Option<f64>], tuning: &BuyHeuristic) -> Vec<&'a Sample> {
    let pond: Vec<&Sample> = pool.iter().zip(open).filter(|x| x.1.is_some()).map(|x| *x.0).collect();
    let qs: Vec<&Quote> = pond.iter().map(|s| s.quote.as_ref()).collect();
    let trails: Vec<&[f64]> = pond.iter().map(|s| s.trail.as_slice()).collect();
    let mut v: Vec<(f64, &Sample)> = picks::momentum_keys(&qs, &trails, tuning).into_iter().zip(pond).filter_map(|(k, s)| Some((k?, s))).collect();
    v.sort_by(|a, b| b.0.total_cmp(&a.0));
    v.into_iter().map(|x| x.1).collect()
}

/// (#560) The first `n` names as an equal-weight book, the weights [`book_multiple`] reads.
fn equal_top<'a>(ranked: impl Iterator<Item = &'a Sample>, n: usize) -> Vec<(&'a str, f64)> {
    let v: Vec<&str> = ranked.take(n).map(|s| s.quote.ticker.as_str()).collect();
    v.iter().map(|t| (*t, 1.0 / v.len() as f64)).collect()
}

/// (#630) The blend's book as the page builds it: [`picks::splice_sides`] over the whole `shipped` order and
/// `fill`'s, `slots` of every 10 places momentum's (`growth_momentum_slots`), up to `n`; true = momentum's. A name
/// both sides hold is placed once, by the side that reaches it first, and the other side moves on to its next.
/// `shipped` needs only its first `n` names: no side reads more than `n`, so a longer list changes nothing.
fn blend_sides<'a>(shipped: &[&'a Sample], fill: impl Iterator<Item = &'a Sample>, slots: usize, n: usize) -> Vec<(&'a str, bool)> {
    let s: Vec<&str> = shipped.iter().map(|&s| s.quote.ticker.as_str()).collect();
    let m: Vec<&str> = fill.map(|s| s.quote.ticker.as_str()).collect();
    picks::splice_sides(&s, &m, false, slots, n)
}

/// (#584) The blend's DCA book: [`blend_sides`]' names at equal weight, as [`equal_top`]. (#630) Was `shipped`'s
/// top `n - slots` names, then `fill`'s not already held.
fn blend_top<'a>(shipped: Vec<&'a Sample>, fill: impl Iterator<Item = &'a Sample>, slots: usize, n: usize) -> Vec<(&'a str, f64)> {
    let book = blend_sides(&shipped, fill, slots, n);
    book.iter().map(|b| (b.0, 1.0 / book.len() as f64)).collect()
}

/// (#560) Each value's percentile in its column: the share of present values strictly below it, so
/// ties share a rank. `None` stays `None`.
fn rank_pct(xs: &[Option<f64>]) -> Vec<Option<f64>> {
    let present: Vec<f64> = xs.iter().flatten().copied().collect();
    let n = present.len() as f64;
    xs.iter().map(|x| x.map(|v| present.iter().filter(|p| **p < v).count() as f64 / n)).collect()
}

/// (#560) An equal-weight book of (realized %, bench %) picks as its excess over the index, pts/yr,
/// annualised the way the shipped top-N row does it. `None` for an empty book.
fn book_excess(picks: &[(f64, f64)], years: i64) -> Option<f64> {
    if picks.is_empty() {
        return None;
    }
    let cum = |f: fn(&(f64, f64)) -> f64| picks.iter().map(|p| 1.0 + f(p) / 100.0).sum::<f64>() / picks.len() as f64;
    Some(ann((cum(|p| p.0) - 1.0) * 100.0, years) - ann((cum(|p| p.1) - 1.0) * 100.0, years))
}

/// (#560) One bucket's top-`n` book by key, as [`book_excess`]. Rows are (key, realized %, bench %,
/// ticker); highest key first, ties by ticker so the book is deterministic, and one slot per ticker (a
/// name scored at two cutoffs in one bucket is one holding). Rows without the key sit out.
fn rival_excess(rows: &[(Option<f64>, f64, f64, &str)], n: usize, years: i64) -> Option<f64> {
    let mut seen = HashSet::new();
    let book: Vec<(f64, f64)> = key_order(rows).into_iter().filter(|r| seen.insert(r.3)).take(n).map(|r| (r.1, r.2)).collect();
    book_excess(&book, years)
}

/// (#560) The rows that carry a key, highest key first, ties by ticker.
fn key_order<'a>(rows: &[(Option<f64>, f64, f64, &'a str)]) -> Vec<(f64, f64, f64, &'a str)> {
    let mut keyed: Vec<(f64, f64, f64, &str)> = rows.iter().filter_map(|r| r.0.map(|k| (k, r.1, r.2, r.3))).collect();
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.3.cmp(b.3)));
    keyed
}

/// (#584) Rival (j): the `first` key's names and the `second` key's, spliced as the page's BUY NOW book is
/// ([`blend_sides`]), up to `n` in all, as one equal-weight book. Both sides order and dedupe as
/// [`rival_excess`]; `n - half` of every 10 places are the `second` key's, a name both hold goes to the side
/// that reaches it first, and a thin side leaves its slots to the other. (#630) Was `first`'s top `half`
/// names, then the `second` key's not already held.
fn blend_excess(first: &[(Option<f64>, f64, f64, &str)], second: &[(Option<f64>, f64, f64, &str)], half: usize, n: usize, years: i64) -> Option<f64> {
    let (a, b) = (key_order(first), key_order(second));
    let (s, m): (Vec<&str>, Vec<&str>) = (a.iter().map(|r| r.3).collect(), b.iter().map(|r| r.3).collect());
    let book: Vec<(f64, f64)> = picks::splice_sides(&s, &m, false, n.saturating_sub(half), n)
        .into_iter()
        .filter_map(|(t, by_m)| (if by_m { &b } else { &a }).iter().find(|r| r.3 == t).map(|r| (r.1, r.2)))
        .collect();
    book_excess(&book, years)
}

/// (#600) The best `k` tickers by key, ordered and deduped as [`rival_excess`], none of them from `skip`.
fn top_names<'a>(rows: &[(Option<f64>, f64, f64, &'a str)], k: usize, skip: &HashSet<&'a str>) -> HashSet<&'a str> {
    let mut seen = skip.clone();
    key_order(rows).into_iter().map(|r| r.3).filter(|&t| seen.insert(t)).take(k).collect()
}

/// (#600) A double sort's second key: `then`, the same rows' other key, kept only on the best `2 * k` tickers
/// by `by` outside `skip` ([`top_names`]), so a top-`k` take holds `then`'s best half of `by`'s top `2 * k`.
fn double_sort(by: &[(Option<f64>, f64, f64, &str)], then: &[Option<f64>], k: usize, skip: &HashSet<&str>) -> Vec<Option<f64>> {
    let top = top_names(by, 2 * k, skip);
    by.iter().zip(then).map(|(r, t)| t.filter(|_| top.contains(r.3))).collect()
}

/// (#600) Rival (l)'s key: a row's [`rank_pct`] in `a` plus its rank in `b`, so the order is the mean
/// percentile's. A row missing either has none.
fn rank_sum(a: &[Option<f64>], b: &[Option<f64>]) -> Vec<Option<f64>> {
    rank_pct(a).into_iter().zip(rank_pct(b)).map(|(a, b)| a.zip(b).map(|(a, b)| a + b)).collect()
}

/// (#560) Rival (e): `draws` seeded draws of `n` names per bucket from the pond, with
/// replacement like the (#329) placebo. Per draw: (mean, median) excess across buckets.
fn random_books(pond: &BTreeMap<i32, Vec<(f64, f64)>>, n: usize, years: i64, draws: usize) -> Vec<(f64, f64)> {
    let mut state = PLACEBO_SEED;
    (0..draws)
        .map(|_| {
            let xs: Vec<f64> = pond
                .values()
                .filter(|p| !p.is_empty())
                .filter_map(|p| {
                    let book: Vec<(f64, f64)> = (0..n).map(|_| p[(next_rand(&mut state) % p.len() as u64) as usize]).collect();
                    book_excess(&book, years)
                })
                .collect();
            (xs.iter().sum::<f64>() / xs.len().max(1) as f64, median(xs))
        })
        .collect()
}

/// (#1243) The CRASH buckets `growth_momentum_crash_slots` caps. The pond's equal-weight month is the mean of
/// every non-crypto trail of 24 months or more, month by month. A bucket is a crash when that month compounds
/// below 1 over the 24, AND the spread of its last 6 months sits in the top third of every bucket's reading so
/// far, this one included: at least 2/3 of them strictly below it, so the first bucket never reads one. The
/// spread is the sum of squared deviations, ordered like the sd. ^GSPC bars on file are quarterly, so the pond
/// stands in for the index.
fn crash_buckets<'a>(samples: impl IntoIterator<Item = &'a Sample>) -> std::collections::BTreeSet<i32> {
    let mut by: BTreeMap<i32, Vec<&[f64]>> = BTreeMap::new();
    for s in samples {
        if picks::asset_class(&s.quote) != 0 && s.trail.len() >= 24 {
            by.entry(bucket(s.date)).or_default().push(&s.trail[s.trail.len() - 24..]);
        }
    }
    let (mut seen, mut out) = (Vec::new(), std::collections::BTreeSet::new());
    for (b, trails) in by {
        let month: Vec<f64> = (0..24).map(|k| trails.iter().map(|t| t[k]).sum::<f64>() / trails.len() as f64).collect();
        let last = &month[18..];
        let m = last.iter().sum::<f64>() / 6.0;
        let ss: f64 = last.iter().map(|x| (x - m).powi(2)).sum();
        seen.push(ss);
        let below = seen.iter().filter(|x| **x < ss).count();
        if month.iter().map(|r| 1.0 + r / 100.0).product::<f64>() < 1.0 && below * 3 >= seen.len() * 2 {
            out.insert(b);
        }
    }
    out
}

/// (#1243) The momentum slots of every 10 in bucket `b`: `growth_momentum_slots`, capped at
/// `growth_momentum_crash_slots` in a [`crash_buckets`] bucket.
fn slots_at(tuning: &BuyHeuristic, crash: &std::collections::BTreeSet<i32>, b: i32) -> usize {
    let n = tuning.growth_momentum_slots;
    if crash.contains(&b) { n.min(tuning.growth_momentum_crash_slots) } else { n }
}

/// (#560) Every [`RIVALS`] book's per-bucket excess, and the pond (realized %, bench %) per bucket that
/// rival (e) draws from. `scorer` is `picks::growth_scores_ranked` in the report, a stand-in in tests;
/// `shipped` keys row 0, `open` (the gates-off tuning) keys row 1 and decides who is in the pond.
fn rival_books(
    samples: &[Sample],
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>,
    shipped: &BuyHeuristic,
    open: &BuyHeuristic,
) -> (Vec<Vec<f64>>, BTreeMap<i32, Vec<(f64, f64)>>) {
    let (bd, bc) = bench;
    let mut by_cutoff: BTreeMap<i32, Vec<&Sample>> = BTreeMap::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        by_cutoff.entry(bucket(s.date)).or_default().push(s);
    }
    let mut books: Vec<Vec<f64>> = vec![Vec::new(); RIVALS.len()];
    let mut pond: BTreeMap<i32, Vec<(f64, f64)>> = BTreeMap::new();
    let crash = crash_buckets(samples); // (#1243)
    for (b, group) in &by_cutoff {
        let quotes: Vec<&Quote> = group.iter().map(|s| s.quote.as_ref()).collect();
        let (sh, op) = (scorer(&quotes, shipped), scorer(&quotes, open));
        let mut rows: Vec<([Option<f64>; 9], f64, f64, &str)> = Vec::new();
        let mut picked: Vec<&Quote> = Vec::new(); // (#585)/(#587) rival (j)'s momentum side, `picks::momentum_keys`
        let mut trails: Vec<&[f64]> = Vec::new(); // (#609) and its trailing months, for keys 3 and 4
        for ((s, sh), op) in group.iter().zip(sh).zip(op) {
            let (Some(op), Some(bench_r)) = (op, benchmark_fwd(bd, bc, s.date, years)) else { continue };
            let f = s.fund.as_ref();
            let keys = [
                sh,
                Some(op),
                picks::momentum_12_1(&s.quote),
                f.and_then(|f| f.quality.or(f.roe)),
                f.and_then(|f| f.earnings_yield),
                f.and_then(|f| f.rev_cagr),
                None,
                s.quote.tr_cagr,
                s.quote.tr_cagr.filter(|_| picks::perf_pct(&s.quote, "10Y").is_some()),
            ];
            rows.push((keys, s.realized, bench_r, s.quote.ticker.as_str()));
            picked.push(s.quote.as_ref());
            trails.push(s.trail.as_slice());
        }
        if !rows.iter().any(|r| r.0[0].is_some()) {
            continue; // the shipped lane holds nobody here, so its row has no window to compare
        }
        let col = |i: usize| rank_pct(&rows.iter().map(|r| r.0[i]).collect::<Vec<_>>());
        let (mom, qual) = (col(2), col(3));
        for (r, (m, q)) in rows.iter_mut().zip(mom.into_iter().zip(qual)) {
            r.0[6] = m.zip(q).map(|(m, q)| (m + q) / 2.0);
        }
        let keyed = |i: usize| rows.iter().map(|r| (r.0[i], r.1, r.2, r.3)).collect::<Vec<_>>();
        for (i, book) in books.iter_mut().take(9).enumerate() {
            book.extend(rival_excess(&keyed(i), VERDICT_TOP, years)); // the nine keyed rows; (j) blends two of them
        }
        let capped: Vec<_> = rows.iter().zip(picks::momentum_keys(&picked, &trails, shipped)).map(|(r, k)| (k, r.1, r.2, r.3)).collect();
        let half = VERDICT_TOP.saturating_sub(slots_at(shipped, &crash, *b));
        books[9].extend(blend_excess(&keyed(0), &capped, half, VERDICT_TOP, years));
        books[10].extend(blend_excess(&keyed(7), &capped, half, VERDICT_TOP, years)); // (#598) (k): (h) in place of row 0
        // (#600) (l): the pond on one blended key; (m)/(n): (j) with its row-0 or its momentum half double-sorted
        let open_keys: Vec<Option<f64>> = rows.iter().map(|r| r.0[1]).collect();
        let mom: Vec<Option<f64>> = capped.iter().map(|c| c.0).collect();
        let rekey = |keys: Vec<Option<f64>>| rows.iter().zip(keys).map(|(r, k)| (k, r.1, r.2, r.3)).collect::<Vec<_>>();
        books[11].extend(rival_excess(&rekey(rank_sum(&open_keys, &mom)), VERDICT_TOP, years));
        books[12].extend(blend_excess(&rekey(double_sort(&keyed(0), &mom, half, &HashSet::new())), &capped, half, VERDICT_TOP, years));
        let held = top_names(&keyed(0), half, &HashSet::new());
        let fill = rekey(double_sort(&capped, &open_keys, VERDICT_TOP.saturating_sub(held.len()), &held));
        books[13].extend(blend_excess(&keyed(0), &fill, half, VERDICT_TOP, years));
        pond.insert(*b, rows.iter().map(|r| (r.1, r.2)).collect());
    }
    (books, pond)
}

/// (#560) The `rivals` report: fourteen top-10 books and a random one on the SAME pond, the non-crypto
/// samples the gates-off scorer can score, in the buckets where the shipped lane holds a name. Report
/// only; the pre-registered bar and the 6-cell tally live in the (#560) receipt.
#[mutants::skip] // printing; `rival_books` and its helpers carry the tests
fn report_rivals(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic, n_draws: usize) {
    if bench.0.len() < 2 {
        return;
    }
    let (books, pond) = rival_books(samples, bench, years, picks::growth_scores_ranked, tuning, &picks::gates_off(tuning));
    let pond_sizes: Vec<f64> = pond.values().map(|p| p.len() as f64).collect();
    println!(
        "\n── RIVALS (#560): equal-weight top-{VERDICT_TOP} books on one pond, HOLD {years}y no-sell, excess vs the index ──\n  pond: {} bucket(s) where the shipped lane holds a name, median {:.0} scoreable names each",
        pond.len(),
        median(pond_sizes)
    );
    if tuning.growth_momentum_crash_slots < tuning.growth_momentum_slots {
        let crash = crash_buckets(samples);
        println!(
            "  crash buckets (#1243): {} of {} pond bucket(s), {} momentum slot(s) of every 10 there",
            pond.keys().filter(|b| crash.contains(*b)).count(),
            pond.len(),
            tuning.growth_momentum_crash_slots
        );
    }
    let stats = |xs: &[f64]| (xs.iter().sum::<f64>() / xs.len().max(1) as f64, median(xs.to_vec()));
    let (ship_mean, ship_med) = stats(&books[0]);
    // (#1149) the random draws come first so every row can print where it ranks among them
    let draws = random_books(&pond, VERDICT_TOP, years, n_draws);
    let mut means: Vec<f64> = draws.iter().map(|d| d.0).collect();
    let mut meds: Vec<f64> = draws.iter().map(|d| d.1).collect();
    let (p95_mean, p95_med) = (percentile95(&mut means).unwrap_or(f64::NAN), percentile95(&mut meds).unwrap_or(f64::NAN));
    for (name, xs) in RIVALS.iter().zip(&books) {
        let (mean, med) = stats(xs);
        let win = xs.iter().filter(|x| **x > 0.0).count() as f64 / xs.len().max(1) as f64 * 100.0;
        let worst = xs.iter().copied().fold(f64::INFINITY, f64::min);
        let tag = if rival_wins(mean - ship_mean, med - ship_med) { "AHEAD" } else { "-" };
        let p = if xs.is_empty() { "n/a  ".to_string() } else { format!("{:.3}", p_at_least(&means, mean)) };
        println!(
            "  {name:<27} n {:>3}  excess {mean:+.1} (med {med:+.1}) pts/yr  win {win:.0}%  worst {worst:+.1}  p {p}  vs plain shipped {:+.1}/{:+.1} {tag}",
            xs.len(),
            mean - ship_mean,
            med - ship_med
        );
    }
    println!(
        "  (e) random pond x{n_draws}         excess {:+.1} (med {:+.1}) pts/yr  p5..p95 {:+.1}..{p95_mean:+.1} (med {:+.1}..{p95_med:+.1})  plain shipped {} the p95",
        median(means.clone()),
        median(meds.clone()),
        percentile(&means, 5.0),
        percentile(&meds, 5.0),
        if ship_mean > p95_mean && ship_med > p95_med { "ABOVE" } else { "NOT above" }
    );
    println!(
        "  p = the share of the {n_draws} random books whose mean excess is at or above the row's, counting the row as one more draw. {} rows are tested against one pond, so the 5% bar split across them is p < {:.4}; {n_draws} draws cannot print a p below {:.4}.",
        RIVALS.len(),
        0.05 / RIVALS.len() as f64,
        1.0 / (n_draws + 1) as f64
    );
}

/// (#1149) One-sided permutation p-value of `x` against `draws`: the draws at or above it, plus `x` itself
/// as one more draw, over all of them. A row above every draw reads 1/(n+1), never 0.
fn p_at_least(draws: &[f64], x: f64) -> f64 {
    (draws.iter().filter(|d| **d >= x).count() + 1) as f64 / (draws.len() + 1) as f64
}

/// (#583) The `winners` report's rows, in print order. [`winner_leaks`] files every stock under exactly one.
const LEAK_KINDS: [&str; 7] = [
    "held (in the top-10 book)",
    "refused (no score)",
    "value floor (PEG trim)",
    "brake alone",
    "gate dock alone",
    "brake + dock",
    "score blind",
];

/// (#583) A window's winners: its top this-many % of stocks by real forward return. Pre-registered.
const WINNER_TOP_PCT: f64 = 5.0;

/// (#583) One [`LEAK_KINDS`] row: every filed stock's excess over its window's book (pts/yr), the
/// winners' alone, and how many winners carried each gate tag.
#[derive(Debug, Default, PartialEq)]
struct Leak {
    all: Vec<f64>,
    winners: Vec<f64>,
    tags: BTreeMap<&'static str, usize>,
}

/// (#583) Files each window's stocks by what keeps them out of the shipped top-10 book. The book is the
/// shipped lane's: the scored rows, the (#75) value trim, the top [`VERDICT_TOP`] by score, and the cut
/// is its 10th score. A stock outside it is refused (no score), trimmed, or a what-if on its OWN score
/// against that cut with every other name held still: the brake off (`growth_overext_cap` 0), the soft
/// dock off (`growth_gate_soft` 1), or both. Still under the cut with both off, or already at it and
/// out on the stable sort's order, it is score blind. Winners are the top `top_pct`% of the window's
/// stocks by realized %, ties at the bar included. Groups are (quote, realized %, PEG yield) per bucket.
/// Report only.
// shortcut: skips the sector and corr caps the main book runs; both are the identity at their shipped 0.
fn winner_leaks(groups: &BTreeMap<i32, Vec<(&Quote, f64, Option<f64>)>>, years: i64, top_pct: f64, tuning: &BuyHeuristic) -> [Leak; 7] {
    let mut out: [Leak; 7] = Default::default();
    let brake = BuyHeuristic { growth_overext_cap: 0.0, ..tuning.clone() };
    let dock = BuyHeuristic { growth_gate_soft: 1.0, ..tuning.clone() };
    let both = BuyHeuristic { growth_gate_soft: 1.0, ..brake.clone() };
    for (b, group) in groups {
        let quotes: Vec<&Quote> = group.iter().map(|g| g.0).collect();
        let [sh, br, dk, bo] = [tuning, &brake, &dock, &both].map(|t| picks::growth_scores_ranked(&quotes, t));
        let rows: Vec<(i32, f64, f64, f64, String, Option<f64>)> =
            group.iter().zip(&sh).filter_map(|(g, s)| Some((*b, (*s)?, g.1, 0.0, g.0.ticker.clone(), g.2))).collect();
        let mut kept = value_floor_trim(&rows, tuning.growth_value_floor_pct).remove(b).unwrap_or_default();
        let survived: HashSet<String> = kept.iter().map(|r| r.3.clone()).collect();
        kept.sort_by(|x, y| y.0.total_cmp(&x.0)); // score desc, stable like the shipped book
        kept.truncate(VERDICT_TOP);
        if kept.is_empty() {
            continue;
        }
        let cut = kept.get(VERDICT_TOP - 1).map_or(f64::NEG_INFINITY, |r| r.0);
        let book = ann((kept.iter().map(|r| 1.0 + r.1 / 100.0).sum::<f64>() / kept.len() as f64 - 1.0) * 100.0, years);
        let held: HashSet<&str> = kept.iter().map(|r| r.3.as_str()).collect();
        let stocks: Vec<usize> = (0..group.len()).filter(|&i| picks::asset_class(group[i].0) == 2).collect();
        let Some(bar) = winner_bar(stocks.iter().map(|&i| group[i].1).collect(), top_pct) else { continue };
        let reach = |s: Option<f64>| s.is_some_and(|s| s >= cut);
        for i in stocks {
            let (q, realized, _) = group[i];
            let kind = if held.contains(q.ticker.as_str()) {
                0
            } else if sh[i].is_none() {
                1
            } else if !survived.contains(&q.ticker) {
                2
            } else if reach(sh[i]) {
                6 // at the cut, out on the sort's order
            } else if reach(br[i]) {
                3
            } else if reach(dk[i]) {
                4
            } else if reach(bo[i]) {
                5
            } else {
                6
            };
            let leak = &mut out[kind];
            let excess = ann(realized, years) - book;
            leak.all.push(excess);
            if realized >= bar {
                leak.winners.push(excess);
                match picks::gate_failures(q, tuning) {
                    None => *leak.tags.entry(picks::refusal_reason(q).unwrap_or("unassessable")).or_default() += 1,
                    Some(f) => {
                        for g in f {
                            *leak.tags.entry(g.0).or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    out
}

/// (#583) A window's winner bar: the forward return of its `top_pct`-th percentile, best first, so ties at
/// the bar count as winners. `None` on an empty window.
fn winner_bar(mut fwd: Vec<f64>, top_pct: f64) -> Option<f64> {
    fwd.sort_by(|x, y| y.total_cmp(x));
    let k = ((fwd.len() as f64 * top_pct / 100.0).ceil() as usize).max(1);
    fwd.get(k - 1).copied()
}

/// (#606) The winners the shipped BLEND holds. Per bucket, each ticker's latest cutoff. The score half is the
/// score order after the (#75) value trim; the momentum half fills from [`momentum_ranked`] over the `open`
/// pond. (#630) [`blend_sides`] splices them as the page and the 20y DCA `blend` lane do, `growth_momentum_slots`
/// of every 10 places the momentum half's, where it was the top `VERDICT_TOP - growth_momentum_slots` score names
/// then momentum's. Winners are the top `top_pct`% stocks by realized %, as in [`winner_leaks`]. Returns the
/// filled book slots, and per held winner its [score half, momentum half] slot counts, a slot counting for the
/// side that placed it, and (#740) the momentum half's slots, one per slot. A config that loses winners from either
/// half reads here.
// shortcut: skips the sector and corr caps the DCA lane runs; both are the identity at their shipped 0.
fn blend_winners<'a>(
    groups: &BTreeMap<i32, Vec<&'a Sample>>,
    top_pct: f64,
    scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>,
    tuning: &BuyHeuristic,
    open: &BuyHeuristic,
) -> (usize, BTreeMap<&'a str, [usize; 2]>, Vec<&'a Sample>) {
    let (mut slots, mut held, mut mom) = (0, BTreeMap::<&'a str, [usize; 2]>::new(), Vec::new());
    let crash = crash_buckets(groups.values().flatten().copied()); // (#1243)
    for (b, group) in groups {
        let by_tk: BTreeMap<&'a str, &'a Sample> = group.iter().map(|&s| (s.quote.ticker.as_str(), s)).collect();
        let pool: Vec<&'a Sample> = by_tk.values().copied().collect();
        let Some(bar) = winner_bar(pool.iter().filter(|s| picks::asset_class(&s.quote) == 2).map(|s| s.realized).collect(), top_pct) else { continue };
        let mut at = tuning.clone();
        at.growth_momentum_slots = slots_at(tuning, &crash, *b);
        let book = blend_book(&by_tk, scorer, &at, open);
        slots += book.len();
        for (t, by_m) in book {
            let s = by_tk[t];
            if by_m {
                mom.push(s);
            }
            if picks::asset_class(&s.quote) == 2 && s.realized >= bar {
                held.entry(t).or_default()[usize::from(by_m)] += 1;
            }
        }
    }
    (slots, held, mom)
}

/// (#821) One bucket's published blend book, as [`blend_winners`] reads it. `by_tk` holds each ticker's latest
/// sample. The score half is the score order after the (#75) value trim; the momentum half fills from
/// [`momentum_ranked`] over the `open` pond; [`blend_sides`] splices them, true = momentum's.
// shortcut: skips the sector and corr caps the DCA lane runs; both are the identity at their shipped 0.
fn blend_book<'a>(
    by_tk: &BTreeMap<&'a str, &'a Sample>,
    scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>,
    tuning: &BuyHeuristic,
    open: &BuyHeuristic,
) -> Vec<(&'a str, bool)> {
    let pool: Vec<&'a Sample> = by_tk.values().copied().collect();
    let quotes: Vec<&Quote> = pool.iter().map(|s| s.quote.as_ref()).collect();
    let rows: Vec<(i32, f64, f64, f64, String, Option<f64>)> =
        pool.iter().zip(scorer(&quotes, tuning)).filter_map(|(s, sc)| Some((0, sc?, 0.0, 0.0, s.quote.ticker.clone(), s.fund.as_ref().and_then(|f| f.peg_yield)))).collect();
    let mut kept = value_floor_trim(&rows, tuning.growth_value_floor_pct).remove(&0).unwrap_or_default();
    kept.sort_by(|x, y| y.0.total_cmp(&x.0)); // score desc, stable like the shipped book
    let shipped: Vec<&'a Sample> = kept.iter().map(|r| by_tk[r.3.as_str()]).collect();
    blend_sides(&shipped, momentum_ranked(&pool, &scorer(&quotes, open), tuning).into_iter(), tuning.growth_momentum_slots, VERDICT_TOP)
}

/// (#583) The `winners` report: where each window's top-5% stocks went, on the shipped book's pond (the
/// non-crypto samples with an index window). Report only; the reading lives in the (#583) receipt.
#[mutants::skip] // printing; `winner_leaks` carries the test
fn report_winners(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    if bd.len() < 2 {
        return;
    }
    let mut groups: BTreeMap<i32, Vec<(&Quote, f64, Option<f64>)>> = BTreeMap::new();
    let mut pools: BTreeMap<i32, Vec<&Sample>> = BTreeMap::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0 && benchmark_fwd(bd, bc, s.date, years).is_some()) {
        groups.entry(bucket(s.date)).or_default().push((s.quote.as_ref(), s.realized, s.fund.as_ref().and_then(|f| f.peg_yield)));
        pools.entry(bucket(s.date)).or_default().push(s);
    }
    let leaks = winner_leaks(&groups, years, WINNER_TOP_PCT, tuning);
    let total = leaks.iter().map(|l| l.winners.len()).sum::<usize>().max(1);
    println!(
        "\n── WINNERS (#583): each window's top {WINNER_TOP_PCT:.0}% stocks by real forward return, filed by what keeps them out of the shipped top-{VERDICT_TOP} book; gap = own CAGR minus the window's book, pts/yr, HOLD {years}y ──"
    );
    let stats = |xs: &[f64]| (xs.iter().sum::<f64>() / xs.len().max(1) as f64, median(xs.to_vec()));
    for (name, l) in LEAK_KINDS.iter().zip(&leaks) {
        let ((wm, wd), (am, ad)) = (stats(&l.winners), stats(&l.all));
        let beat = l.all.iter().filter(|x| **x > 0.0).count() as f64 / l.all.len().max(1) as f64 * 100.0;
        let mut tags: Vec<(&str, usize)> = l.tags.iter().map(|(t, n)| (*t, *n)).collect();
        tags.sort_by_key(|a| std::cmp::Reverse(a.1));
        let tags: Vec<String> = tags.iter().take(6).map(|(t, n)| format!("{t} {n}")).collect();
        println!(
            "  {name:<26} winners {:>4} ({:>3.0}%) gap {wm:+.1} (med {wd:+.1}) | all {:>5} gap {am:+.1} (med {ad:+.1}) beat book {beat:.0}% | {}",
            l.winners.len(),
            l.winners.len() as f64 / total as f64 * 100.0,
            l.all.len(),
            tags.join(", ")
        );
    }
    let (slots, held, mom) = blend_winners(&pools, WINNER_TOP_PCT, picks::growth_scores_ranked, tuning, &picks::gates_off(tuning));
    let [sh, mh] = held.values().fold([0, 0], |a, h| [a[0] + h[0], a[1] + h[1]]);
    let mut names: Vec<(&str, usize)> = held.iter().map(|(t, h)| (*t, h[0] + h[1])).collect();
    names.sort_by_key(|a| std::cmp::Reverse(a.1)); // stable: ticker order on ties
    let names: Vec<String> = names.iter().map(|(t, n)| format!("{t} {n}")).collect();
    println!(
        "{} {:.1}% of {slots} blend book slots are winners (base {WINNER_TOP_PCT:.0}%); score half {sh}, momentum half {mh} | {}",
        markers::WINNERS_HELD,
        (sh + mh) as f64 / slots.max(1) as f64 * 100.0,
        names.join(", ")
    );
    // (#740) the momentum half's held book split by factor, as (#159) splits the score lane's: what that half
    // BOUGHT AND LOST. Every slot is held, so the score is a dummy and n is unbounded.
    let mom: Vec<(&Sample, f64)> = mom.into_iter().map(|s| (s, 0.0)).collect();
    let lost = mom.iter().filter(|(s, _)| s.realized < 0.0).count();
    println!("  (#740) momentum half's held LOSERS by factor: {} slots, {lost} lost money", mom.len());
    for HeldLoserRow { factor, n_win, n_lose, med_win, med_lose, auc } in held_loser_factors(&mom, usize::MAX, &FUND_FACTORS) {
        println!("    {factor:<21} winners n={n_win:<4} med {med_win:>9.2}   losers n={n_lose:<4} med {med_lose:>9.2}   AUC {auc:>5.2}");
    }
}

/// (#106) The per-bucket terminal multiples of an equal-weight top-`n` book and of the same bucket's
/// benchmark leg — ONE definition of "what did the book end up worth", so [`book_stats`]'s headline
/// means and [`book_deciles`]'s distribution cannot be computed two slightly different ways. Buckets
/// that yield no pick are skipped, which is why the result is a Vec rather than a map.
fn book_multiples(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>, n: usize) -> Vec<(f64, f64)> {
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let mut out = Vec::new();
    for v in by_bucket.values() {
        let mut vv = v.clone();
        vv.sort_by(|a, b| b.0.total_cmp(&a.0)); // rank_key desc
        let take = n.min(vv.len());
        if take == 0 {
            continue;
        }
        let p = &vv[..take];
        out.push((
            mean(&p.iter().map(|x| 1.0 + x.1 / 100.0).collect::<Vec<_>>()),
            mean(&p.iter().map(|x| 1.0 + x.2 / 100.0).collect::<Vec<_>>()),
        ));
    }
    out
}

/// (#106) (round 3 §4) The DISTRIBUTION behind the headline: deciles of the held book's terminal
/// multiple across buckets, plus P(book < index) and P(book < 1.0). Returns
/// (deciles 10..90, p_below_index, p_below_one, buckets).
///
/// WHY THE MEAN IS THE WRONG SELECTOR FOR `VERDICT_TOP`. `book_stats` is right to average the
/// terminal MULTIPLES inside a bucket — that is what an equal-weight book is worth. But those
/// per-bucket numbers are then arithmetically averaged ACROSS buckets into the headline, and
/// `VERDICT_TOP` USED TO BE the argmax of that headline over a 13-rung ladder. Under positive skew,
/// small N maximises the mean while wrecking the median and the left tail — that is the arithmetic of
/// skew, not an empirical claim — so selecting N by mean excess selects the most concentrated book
/// almost by construction.
///
/// (#120) THAT SELECTION IS GONE — the basket is now fixed a priori at 10 — and this instrument's job
/// changed with it. It is no longer the thing that would "re-derive N honestly": re-deriving N off the
/// median would be the same selection wearing a different statistic. It is now the evidence that the
/// fixed basket is defensible, and the wide run backs it — at 20y the top-10 book's excess MEDIAN is
/// HIGHER than top-3's (+6.1 vs +5.6) while its MEAN is lower (+6.0 vs +6.3). That crossing is the
/// skew signature above, printed rather than argued.
///
/// P(book < 1.0) is the number a 20-year holder actually feels: not "did I trail the index", but "did
/// I end with less than I started". Nothing in this report has ever printed it.
fn book_deciles(
    by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>,
    n: usize,
) -> Option<(Vec<f64>, f64, f64, usize)> {
    let pairs = book_multiples(by_bucket, n);
    if pairs.is_empty() {
        return None;
    }
    let below_index = pairs.iter().filter(|(b, s)| b < s).count() as f64 / pairs.len() as f64;
    let below_one = pairs.iter().filter(|(b, _)| *b < 1.0).count() as f64 / pairs.len() as f64;
    let mut mults: Vec<f64> = pairs.iter().map(|(b, _)| *b).collect();
    mults.sort_by(f64::total_cmp);
    let deciles = (1..=9).map(|p| percentile(&mults, f64::from(p) * 10.0)).collect();
    Some((deciles, below_index, below_one, pairs.len()))
}

/// (#43) Equal-weight held-book stats for a given ranking key. `by_bucket`: 6mo-bucket ->
/// Vec<(rank_key, realized%, bench%)>. Per bucket: top-N by rank_key desc, held equal-weight -> book =
/// ann(mean terminal multiple), SPY = same on the bench leg, excess = book − SPY. Returns
/// (book, spy, excess_mean, win%, worst, oos_early, oos_late). `None` if no bucket yields a pick.
fn book_stats(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>, n: usize, years: i64) -> Option<(f64, f64, f64, f64, f64, f64, f64)> {
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let (mut book, mut spy, mut excess) = (Vec::new(), Vec::new(), Vec::new());
    for (bcum, scum) in book_multiples(by_bucket, n) {
        book.push(ann((bcum - 1.0) * 100.0, years));
        spy.push(ann((scum - 1.0) * 100.0, years));
        excess.push(*book.last().unwrap() - *spy.last().unwrap());
    }
    let m = excess.len();
    if m == 0 {
        return None;
    }
    let win = excess.iter().filter(|e| **e > 0.0).count() as f64 / m as f64 * 100.0;
    let worst = excess.iter().cloned().fold(f64::INFINITY, f64::min);
    let cut = m / 2;
    Some((mean(&book), mean(&spy), mean(&excess), win, worst, mean(&excess[..cut]), mean(&excess[cut..])))
}

/// (#796) The ON-SALE lane read on the GROWTH lane's ruler: [`book_stats`]'s top-`n` equal-weight held book plus
/// the median excess, in `report_vs_benchmark`'s pts/yr, so the foil's "negative edge" can be checked against the
/// book GROWTH ships on. The row never starts with `top-10 `, since [`markers::VERDICT_ROW`] reads the first one.
fn onsale_book_row(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>, n: usize, years: i64) -> Option<String> {
    let (book, spy, excess, win, worst, early, late) = book_stats(by_bucket, n, years)?;
    let pairs = book_multiples(by_bucket, n);
    let med = median(pairs.iter().map(|(b, s)| ann((b - 1.0) * 100.0, years) - ann((s - 1.0) * 100.0, years)).collect());
    Some(format!(
        "  ON-SALE top-{n:<2} book {book:+.1}%/yr  vs S&P500 {spy:+.1}%/yr  ->  excess {excess:+.1} (med {med:+.1}) pts/yr   win {win:.0}% of {}   worst {worst:+.1}   OOS {early:+.1}/{late:+.1}",
        pairs.len()
    ))
}

/// (#796) Prints [`onsale_book_row`] for `buy_score` over `report_book_by_factor`'s pool (non-crypto, with a
/// benchmark window), under the ON-SALE lane block. A print only: no gate reads it, and the lane stays a foil.
fn report_onsale_book(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    let mut by: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue;
        }
        let Some(score) = buy_score(&s.quote, tuning) else { continue };
        let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        by.entry(bucket(s.date)).or_default().push((score, s.realized, br));
    }
    if let Some(row) = onsale_book_row(&by, VERDICT_TOP, years) {
        println!("{row}");
    }
}

/// (#324) Each `picks::gate_notches` row applied to `tuning`: (tag, loosened tuning), in list order.
fn notch_tunings(tuning: &BuyHeuristic) -> Vec<(&'static str, BuyHeuristic)> {
    picks::gate_notches()
        .into_iter()
        .map(|(tag, _, loosen)| {
            let mut t = tuning.clone();
            loosen(&mut t);
            (tag, t)
        })
        .collect()
}

/// (#324) The in-sample GUARD for the notch shadow `track` grades forward: the book each reopen would actually buy.
/// Row one holds every name the screen CLEARED. Then comes one row per sweep notch (`picks::gate_notches`) that newly
/// admits anything, holding the cleared names PLUS that notch's cohort (refused at `tuning`, cleared once loosened:
/// the GATE SWEEP's rule). Each is equal-weight per bucket (`usize::MAX`: the whole set, not a top-N), on
/// `report_entry_state`'s pool (non-crypto, benchmarkable). Row: (label, samples it adds, or holds for `cleared`,
/// `book_stats`). A union is never thinner than the cleared book, so its worst window is not deep by construction,
/// the way a thin cohort's alone was under (#323)'s guard.
///
/// (#330) EACH ROW IS NOW READ AT TWO BASKETS, and the second one decides. Uncapped, every admit is bought
/// at full weight, so the union is a plain weighted average and ANY cohort below the book's mean must drag
/// it — that verdict is arithmetic, not evidence. At `VERDICT_TOP` the cohort has to COMPETE for the ten
/// slots the tool actually publishes, and the POOL CENSUS says those slots have room (median gated pool 5/6/7
/// against a basket of 10). The uncapped column is kept so every banked receipt stays readable.
fn notch_books(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) -> Vec<(String, usize, BookRow, BookRow)> {
    let notches = notch_tunings(tuning);
    let mut cleared: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    // (#329) every row the shipped gates refused, bucketed — the pool `placebo_bars` draws its null from.
    // A notch's own cohort is a subset of this by construction and is deliberately left in it: the null
    // asks what an ARBITRARY refused cohort of that size does, not what the complement does.
    // (#330) These rows keep rank 0.0: a refused name HAS no shipped score, and every draw overwrites the
    // key with the rank of the cohort row it stands in for. Nothing reads the 0.0.
    let mut refused: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    let mut added: Vec<Vec<(i32, (f64, f64, f64))>> = vec![Vec::new(); notches.len()];
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        let Some(br) = benchmark_fwd(&bench.0, &bench.1, s.date, years) else { continue };
        // (#330) the rank key is the score the row would be SORTED by in the book it joins — the shipped
        // score for a cleared name, the loosened score for a cohort name. Uncapped this changes nothing
        // (the whole bucket is taken and a mean does not care about order), which is what makes the old
        // column provably byte-identical; capped it is the whole question.
        if let Some(score) = growth_score(&s.quote, tuning) {
            cleared.entry(bucket(s.date)).or_default().push((score, s.realized, br));
            continue;
        }
        refused.entry(bucket(s.date)).or_default().push((0.0, s.realized, br));
        for ((_, t), cohort) in notches.iter().zip(added.iter_mut()) {
            if let Some(score) = growth_score(&s.quote, t) {
                cohort.push((bucket(s.date), (score, s.realized, br)));
            }
        }
    }
    let Some(base) = both_baskets(&cleared, None, years) else { return Vec::new() };
    let mut rows = vec![("cleared".to_string(), cleared.values().map(Vec::len).sum(), base.0, base.1)];
    for ((tag, _), cohort) in notches.iter().zip(added).filter(|(_, c)| !c.is_empty()) {
        let mut union = cleared.clone();
        let mut shape: std::collections::BTreeMap<i32, Vec<f64>> = Default::default();
        for (b, row) in &cohort {
            union.entry(*b).or_default().push(*row);
            shape.entry(*b).or_default().push(row.0);
        }
        let bars = placebo_bars(&cleared, &refused, &shape, years);
        rows.extend(both_baskets(&union, bars, years).map(|(a, t)| (tag.to_string(), cohort.len(), a, t)));
    }
    rows
}

/// (#330) One graded row at one basket: the stats and the band they were judged against.
/// A struct rather than two more tuple slots because `--in-diff` grades a touched function's whole
/// return-replacement set, and wide tuple returns are what explode it.
#[derive(Clone, Copy, PartialEq, Debug)]
struct BookRow {
    st: (f64, f64, f64, f64, f64, f64, f64),
    bar: Option<f64>,
}

/// (#330) One book read twice: uncapped (the (#324)..(#329) column) and at the basket the tool publishes.
fn both_baskets(
    book: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>,
    bars: Option<(f64, f64)>,
    years: i64,
) -> Option<(BookRow, BookRow)> {
    Some((
        BookRow { st: book_stats(book, usize::MAX, years)?, bar: bars.map(|b| b.0) },
        BookRow { st: book_stats(book, VERDICT_TOP, years)?, bar: bars.map(|b| b.1) },
    ))
}

/// (#330) THE THREE-WAY VERDICT. A cohort that never reaches the published basket leaves the row
/// bit-identical to `cleared`: the notch would change nothing the tool prints, which is neither a pass nor
/// a cost. Reporting that as FAIL — what (#324)..(#329) did — charges a notch for dilution the published
/// book never pays. It lives outside the report bodies because those carry `#[mutants::skip]`.
fn ship_verdict(cleared: &BookRow, union: &BookRow) -> &'static str {
    if union.st == cleared.st {
        return "inert";
    }
    if guard_holds(&cleared.st, &union.st, union.bar) { "pass" } else { "FAIL" }
}

// (#389) both moved to `core` so the page engine (wasm, no `commands`) links them; callers unchanged.
pub(crate) use crate::core::{next_rand, percentile};

/// (#329) Draws behind the placebo band, and its seed. BOTH FIXED BEFORE THE FIRST RUN and not to be
/// retuned afterwards: a bar chosen once its numbers are visible decides nothing.
const PLACEBO_DRAWS: usize = 200;
const PLACEBO_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// (#329) THE MATCHED-n BAR, and the reason it had to exist. (#324)'s guard reads `union.2 >= cleared.2
/// - 0.1` off the UNION book, but a cohort of `n` names only carries weight `w = n/(cleared+n)` in that
/// union, so the union moves `w * (cohort - cleared)` and the bar on the COHORT is really
/// `cohort >= cleared - 0.1/w`. That scales with `1/n`: measured on the 8y fund pit (cleared n=347,
/// excess +4.6) it demanded -12.8 of a 2-name cohort and +4.4 of a 305-name one — a 30x spread in
/// strictness decided by cohort SIZE, not by quality. `range` could not fail it arithmetically, which is
/// the same vacuity (#327) caught at n=1 and did not fix, and (#323)(d) named a matched-n guard as "a
/// round of its own".
///
/// The fix is a null with the SAME n: draw `shape` many rows from the pool the shipped gates REFUSED and
/// see what an arbitrary cohort of that exact size does to the union. `shape` is per ~6mo bucket, so the
/// draw matches the real cohort's cutoff mix name for name and the comparison is quality-only — a cohort
/// concentrated in one regime is never scored against a null spread over twenty years.
///
/// Returns the 95th percentile (nearest-rank) of `PLACEBO_DRAWS` union excesses: the notch must beat 19
/// of 20 arbitrary same-size cohorts. `None` when a bucket the cohort drew from has no refused rows at
/// all, which is a shape the null cannot match; the caller then falls back to the (#324) bar and says so.
///
/// Draws are WITH REPLACEMENT — the standard bootstrap, and it keeps the inner loop
/// allocation-free. Per bucket the cohort takes a handful of names from hundreds, so the collision rate
/// is low enough to leave the percentile alone.
///
/// Its ceiling, stated because it flatters the notches: a random refused name may have failed five gates,
/// while a notch admits names just past ONE boundary, so this null is GENEROUS. Drawing from near-misses
/// only is the upgrade path and is deliberately not taken here.
///
/// (#330) BOTH BANDS COME OUT OF THE SAME DRAWS, and that is not a micro-optimisation: it is what keeps
/// the uncapped p95 bit-identical to (#329)'s, since `next_rand` is stepped once per drawn row and the
/// sequence therefore does not move. `shape` now carries the cohort's RANKS per bucket rather than a
/// count — the count is its length. A drawn row inherits the rank of the cohort row it stands in for, so
/// the null holds POSITION fixed and varies only QUALITY: at the ship basket it asks "if arbitrary
/// refused names entered the book at exactly these ranks, how much would the published top-N move?".
/// Without that the drawn rows would carry the refused pool's own unscored key and could never enter a
/// capped basket, which would make every top-N band vacuously equal to the cleared book.
fn placebo_bars(
    cleared: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>,
    refused: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>,
    shape: &std::collections::BTreeMap<i32, Vec<f64>>,
    years: i64,
) -> Option<(f64, f64)> {
    let mut state = PLACEBO_SEED;
    let (mut all, mut top) = (Vec::with_capacity(PLACEBO_DRAWS), Vec::with_capacity(PLACEBO_DRAWS));
    for _ in 0..PLACEBO_DRAWS {
        let mut union = cleared.clone();
        for (b, ranks) in shape {
            let pool = refused.get(b).filter(|p| !p.is_empty())?;
            let dst = union.entry(*b).or_default();
            for rank in ranks {
                let mut row = pool[(next_rand(&mut state) % pool.len() as u64) as usize];
                row.0 = *rank;
                dst.push(row);
            }
        }
        all.push(book_stats(&union, usize::MAX, years)?.2);
        top.push(book_stats(&union, VERDICT_TOP, years)?.2);
    }
    Some((percentile95(&mut all)?, percentile95(&mut top)?))
}

/// (#329) Nearest-rank p95 of a band, sorted in place. Split out by (#330) so both baskets read the
/// same percentile from one definition instead of two copies that could drift apart. (#651) Ranks over
/// the band's own length; it read PLACEBO_DRAWS, which only every caller's band happened to match.
fn percentile95(band: &mut [f64]) -> Option<f64> {
    band.sort_by(f64::total_cmp);
    band.get(((band.len() as f64 * 0.95).ceil() as usize).max(1) - 1).copied()
}

/// (#324) The pre-registered no-worse bar for one notch's union book against the cleared book at ONE horizon: excess
/// within 0.1 AND worst window within 1.0 (the repo's standing no-worse bar, (#322) leg 2). A notch ships only when
/// this holds at 20y, 12y and 8y fund pit and `track`'s forward half reads REOPEN SIGNAL.
///
/// (#329) AMENDED: when `bar` carries a `placebo_bar` reading, the excess leg is judged against THAT
/// instead of the flat tenth, so the strictness no longer moves with cohort size. `None` reproduces the
/// (#324) arithmetic verbatim — that is what makes the fallback path provable rather than asserted, and
/// it is the leg `proxy_books` keeps, since its cohort is a SPLIT of the cleared book rather than an
/// admission out of the refused pool, so a refused-pool null would not describe it.
///
/// The worst-window leg is untouched. It is diluted the same way, but a deeper worst window is a real
/// risk signal at any n, and one bar per round.
fn guard_holds(cleared: &(f64, f64, f64, f64, f64, f64, f64), union: &(f64, f64, f64, f64, f64, f64, f64), bar: Option<f64>) -> bool {
    let excess_ok = match bar {
        Some(p95) => union.2 >= p95,
        None => union.2 >= cleared.2 - 0.1,
    };
    excess_ok && union.4 >= cleared.4 - 1.0
}

/// (#329) The `bar` cell for one row of the three guarded tables. Lives here, not inline in the report
/// bodies, because those carry `#[mutants::skip]` and this is a decision — which bar a reader is told
/// the verdict was taken against — so it has to stay gradeable. The base row prints nothing: it is the
/// thing every other row is measured against, so it has no bar of its own.
fn bar_column(is_base: bool, bar: Option<f64>) -> String {
    match bar {
        _ if is_base => String::new(),
        Some(p95) => format!("  bar {p95:+.1}"),
        None => "  bar n/a".to_string(),
    }
}

/// (#1049) The `guard` cell for one row of `report_proxy_books`, out of that skip-tagged body for
/// `bar_column`'s reason. It stays on (#324)'s flat bar: the borrowed cohort is a SPLIT of the cleared
/// book, not an admission out of the refused pool, so (#329)'s refused-pool null would not describe it.
/// The own-history row is the base and prints nothing.
fn proxy_guard(is_base: bool, base: &(f64, f64, f64, f64, f64, f64, f64), st: &(f64, f64, f64, f64, f64, f64, f64)) -> &'static str {
    if is_base {
        ""
    } else if guard_holds(base, st, None) {
        "  guard pass"
    } else {
        "  guard FAIL"
    }
}

/// (#324) Prints `notch_books` with each row's `guard_holds`. Print-only, so `replace with ()` is unkillable; the
/// arithmetic and the bar are tested in those two.
#[mutants::skip]
fn report_notch_books(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let rows = notch_books(samples, bench, years, tuning);
    let Some((_, _, _, base_top)) = rows.first().cloned() else { return };
    println!("\n── NOTCH BOOK (#324): the book each one-notch reopen would buy (cleared + that notch's cohort), every name equal-weight, held {years}y ──");
    for (l, n, all, top) in rows {
        let (b, _, e, w, wo, el, la) = all.st;
        let guard = if l == "cleared" { String::new() } else { format!("  ship {}", ship_verdict(&base_top, &top)) };
        let bar_col = bar_column(l == "cleared", all.bar);
        let top_col = format!("  top{VERDICT_TOP} {:+.1}/{:+.1}{}", top.st.2, top.st.4, bar_column(l == "cleared", top.bar));
        println!("  {l:<10} n={n:<6} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{bar_col}{top_col}{guard}");
    }
    println!("  (n = samples: the cleared book's own, or what the notch adds. `excess`/`bar` are the UNCAPPED book — every admit bought at full weight — kept for continuity with (#324)..(#329). (#330): THE `ship` VERDICT DECIDES AND IT IS READ AT top{VERDICT_TOP} (excess/worst, and the cleared row is the baseline it is measured against), the basket the journaled verdict publishes, where a cohort name must OUTRANK an incumbent to be bought: excess >= that column's `bar`, the p95 of {PLACEBO_DRAWS} same-n cohorts drawn at random from the refused pool, cutoff-matched and entered at the cohort's own ranks, AND worst >= cleared -1.0, at 20y, 12y and 8y. `inert` = the cohort never reaches the published basket, so the notch changes nothing the tool prints — not a pass, and not a cost it was charged for through (#329). `bar n/a` = the null could not match the cohort's shape, so the row falls back to (#324)'s flat -0.1. `track` grades the same cohorts forward)");
}

/// (#328) The index's own CAGR over the `years` ending at `date` — the door's backstop, expressed as the
/// same unit the names are judged in. Built on `bench_trailing`, so it inherits that helper's guarantee
/// of never reading past the cutoff. `None` when the series doesn't reach back a full window, which
/// shuts the door for that cutoff rather than guessing.
///
/// A total wipeout makes `1 + cum/100` non-positive and `powf` returns NaN; `is_finite` is what stops
/// that becoming a silently negative floor.
fn bench_leg_cagr(bench: &(Vec<chrono::NaiveDate>, Vec<f64>), date: chrono::NaiveDate, years: i64) -> Option<f64> {
    let cum = bench_trailing(&bench.0, &bench.1, date, years)?;
    let cagr = ((1.0 + cum / 100.0).powf(1.0 / years as f64) - 1.0) * 100.0;
    cagr.is_finite().then_some(cagr)
}

/// (#408) O'Shaughnessy's value composite: for each cutoff, rank every name on each value yield
/// (earnings, EBITDA, FCF and sales over price/EV) against that cutoff's cohort, then average the
/// present legs' percentiles. A percentile is the count of strictly lower values over n−1, times 100,
/// so ties share the lower rank and the result never depends on input order (bit-identical at any
/// thread count). A leg with fewer than 2 names at a date ranks nothing; a name needs ≥2 ranked legs.
/// (#409) Market cap ranks the same way as a fifth column, alone, into `small_pct` = 100 − its
/// percentile, so the smallest name at a cutoff reads 100. It never enters the composite.
/// (#533) `low_dro` becomes its distance from the median of the same date's same-sector names, as Wang
/// adjusts by industry; a name with no sector reads None. shortcut: today's GICS (11 sectors, #95) for
/// Wang's FF48 history; a finer key needs an as-of industry feed this tool does not read.
/// `samples` must be date-sorted: `chunk_by_mut` groups runs, not dates.
fn stamp_cohort_ranks(samples: &mut [Sample]) {
    let legs: [fn(&core::FundFactors) -> Option<f64>; 5] =
        [|f| f.earnings_yield, |f| f.ebitda_yield, |f| f.fcf_ev_yield, |f| f.sales_ev_yield, |f| f.mcap];
    let rank = |col: &[f64], v: f64| col.partition_point(|x| x.total_cmp(&v).is_lt()) as f64 / (col.len() - 1) as f64 * 100.0;
    for day in samples.chunk_by_mut(|a, b| a.date == b.date) {
        let cols: Vec<Vec<f64>> = legs
            .iter()
            .map(|leg| {
                let mut v: Vec<f64> = day.iter().filter_map(|s| s.fund.as_ref().and_then(*leg)).collect();
                v.sort_by(f64::total_cmp);
                v
            })
            .collect();
        for s in day.iter_mut() {
            let Some(f) = s.fund.as_mut() else { continue };
            let ranked: Vec<Option<f64>> = legs
                .iter()
                .zip(&cols)
                .map(|(leg, col)| if col.len() >= 2 { leg(f).map(|v| rank(col, v)) } else { None })
                .collect();
            let pcts: Vec<f64> = ranked[..4].iter().flatten().copied().collect();
            f.value_composite = (pcts.len() >= 2).then(|| pcts.iter().sum::<f64>() / pcts.len() as f64);
            f.small_pct = ranked[4].map(|p| 100.0 - p);
        }
        let mut by_sector: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
        for s in day.iter() {
            if let (Some(sec), Some(v)) = (&s.quote.sector, s.fund.as_ref().and_then(|f| f.low_dro)) {
                by_sector.entry(sec.clone()).or_default().push(v);
            }
        }
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            (v[(v.len() - 1) / 2] + v[v.len() / 2]) / 2.0
        };
        let medians: std::collections::BTreeMap<String, f64> = by_sector.into_iter().map(|(k, v)| (k, median(v))).collect();
        for s in day.iter_mut() {
            let m = s.quote.sector.as_ref().and_then(|k| medians.get(k)).copied();
            if let Some(f) = s.fund.as_mut() {
                f.low_dro = f.low_dro.zip(m).map(|(v, m)| v - m);
            }
        }
    }
}

/// (#328) Stamp every sample's point-in-time sector floor. THE ONLY PLACE the PIT rule is decided, and
/// the reason the door needs no threading: once each `Quote` carries its own floor, all ~20
/// `growth_score` call sites in this file honour the door without being touched — the wide edit that
/// would otherwise red the mutation gate.
///
/// The cohort for a cutoff is the cutoff BEFORE it. Never its own, which would let a name's own CAGR
/// help set the bar it is then judged against; never a later one, which is plain look-ahead. The first
/// cutoff has no predecessor, so its map is empty and the door is shut there — being one step stale is
/// conservative by construction, and the walk therefore starts in exactly the control's behaviour.
///
/// The benchmark leg is `growth_min_leg_years` rather than each name's own rung. shortcut: one window
/// for the whole cohort, because it is the only leg EVERY ranked name is guaranteed to own — a per-rung
/// benchmark would compare a 20y name against a 20y index and a 5y name against a 5y index and call
/// both "beat the market". Upgrade path if the row ever ships: key the cohort by (sector, rung).
fn stamp_sector_cohorts(samples: &mut [Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), tuning: &BuyHeuristic) {
    let leg = tuning.growth_min_leg_years.max(1.0) as i64;
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by_key(|&i| samples[i].date);
    let mut floors: std::collections::BTreeMap<String, f64> = Default::default();
    let mut cohort: Vec<Quote> = Vec::new();
    let mut open: Option<chrono::NaiveDate> = None;
    for i in order {
        let date = samples[i].date;
        if open != Some(date) {
            // a new cutoff begins: freeze what the PREVIOUS one supplies, then start collecting this one
            floors = match bench_leg_cagr(bench, date, leg) {
                Some(bench_cagr) => picks::sector_floors(&cohort.iter().collect::<Vec<_>>(), tuning, picks::SECTOR_DOOR_K, bench_cagr),
                None => Default::default(), // no full index window at this cutoff -> no door, no guess
            };
            cohort.clear();
            open = Some(date);
        }
        cohort.push((*samples[i].quote).clone());
        let stamp = samples[i].quote.sector.as_deref().and_then(|s| floors.get(s)).copied();
        std::sync::Arc::make_mut(&mut samples[i].quote).growth_sector_floor = stamp;
    }
}

/// (#328) What the sector door would buy, priced exactly the way (#324)'s notch rows and (#327)'s proxy
/// rows are: the cleared book, then the cleared book PLUS the names the door admits, both equal-weight
/// and held `years`, so `guard_holds` reads straight off the pair.
///
/// FREE AT EVERY RUN. The arm is just `tuning` with the knob flipped to the pre-registered
/// `picks::SECTOR_DOOR_K`, because `stamp_sector_cohorts` has already written each quote's point-in-time
/// sector floor whether or not the shipped config arms the knob. So a knob-off run is byte-identical
/// and STILL prints what the door would have bought — which is what makes a refusal here cost nothing
/// and keeps the row honest for future rounds.
///
/// The point-in-time rule is not enforced here; it lives in `stamp_sector_cohorts`, which is the only
/// place that decides which cutoff's cohort a sample is judged against.
fn sector_books(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) -> Vec<(String, usize, BookRow, BookRow)> {
    let armed = BuyHeuristic { growth_sector_leaders: picks::SECTOR_DOOR_K, ..tuning.clone() };
    let mut cleared: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    let mut refused: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    let mut added: Vec<(i32, (f64, f64, f64))> = Vec::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        let Some(br) = benchmark_fwd(&bench.0, &bench.1, s.date, years) else { continue };
        // (#330) rank key as in `notch_books`: shipped score when cleared, door score when the door admits.
        if let Some(score) = growth_score(&s.quote, tuning) {
            cleared.entry(bucket(s.date)).or_default().push((score, s.realized, br));
            continue;
        }
        // (#329) the door admits out of the refused pool exactly as a notch does, so the same null applies.
        refused.entry(bucket(s.date)).or_default().push((0.0, s.realized, br));
        if let Some(score) = growth_score(&s.quote, &armed) {
            added.push((bucket(s.date), (score, s.realized, br)));
        }
    }
    let Some(base) = both_baskets(&cleared, None, years) else { return Vec::new() };
    let mut out = vec![("cleared".to_string(), cleared.values().map(Vec::len).sum(), base.0, base.1)];
    if !added.is_empty() {
        let mut union = cleared.clone();
        let mut shape: std::collections::BTreeMap<i32, Vec<f64>> = Default::default();
        for (b, row) in &added {
            union.entry(*b).or_default().push(*row);
            shape.entry(*b).or_default().push(row.0);
        }
        let bars = placebo_bars(&cleared, &refused, &shape, years);
        out.extend(both_baskets(&union, bars, years).map(|(a, t)| ("sector-door".to_string(), added.len(), a, t)));
    }
    out
}

/// (#328) Prints `sector_books` with `guard_holds`. Print-only, so `replace with ()` is unkillable; the
/// arithmetic is tested in `sector_books` and the bar in `guard_holds`.
#[mutants::skip]
fn report_sector_books(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let rows = sector_books(samples, bench, years, tuning);
    let Some((_, _, _, base_top)) = rows.first().cloned() else { return };
    println!("\n── SECTOR BOOK (#328): what a top-{}-per-GICS-sector door would add to the cleared book, every name equal-weight, held {years}y ──", picks::SECTOR_DOOR_K);
    for (l, n, all, top) in rows {
        let (b, _, e, w, wo, el, la) = all.st;
        let guard = if l == "cleared" { String::new() } else { format!("  ship {}", ship_verdict(&base_top, &top)) };
        let bar_col = bar_column(l == "cleared", all.bar);
        let top_col = format!("  top{VERDICT_TOP} {:+.1}/{:+.1}{}", top.st.2, top.st.4, bar_column(l == "cleared", top.bar));
        println!("  {l:<12} n={n:<6} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{bar_col}{top_col}{guard}");
    }
    println!("  (n = samples: the cleared book's own, or what the door adds. Cohort is the PREVIOUS cutoff's stocks, backstopped at the index's own {}y CAGR. `excess`/`bar` are the UNCAPPED book; (#330) the `ship` verdict decides and is read at top{VERDICT_TOP} (excess/worst), the published basket — excess >= that column's `bar`, the p95 of {PLACEBO_DRAWS} same-n refused cohorts entered at the cohort's own ranks, AND worst >= cleared -1.0, at 20y, 12y and 8y. `inert` = the door's names never reach the published basket)", tuning.growth_min_leg_years.max(1.0) as i64);
}

/// (#327) How much of a young listing's life the pair is judged on. ~26 months, so `PROXY_MIN_MONTHS` (24) of
/// month-end returns fit inside it with a bar to spare. Deliberately a FIXED window at the listing's start and
/// not "however much it takes to pass": a verdict allowed to grow its own evidence window until it agrees is
/// not a test. (#787) It is knowable only once the window has closed, so a spliced series has no entry
/// before then (`proxy_entry`): the verdict is built from the returns of those first ~26 months.
const PROXY_VERIFY_DAYS: i64 = 800;

/// (#787) The first bar the walk may stand on: the listing's own first, and on a spliced series the day its
/// verify window closes. The walk used to enter a spliced series at its own first bar, so a cutoff in the
/// first ~26 months scored a splice that `apply_proxy` approved on the returns that came after it.
fn proxy_entry(own_first: Option<chrono::NaiveDate>, proxied: bool) -> Option<chrono::NaiveDate> {
    own_first.map(|f| if proxied { f + chrono::Duration::days(PROXY_VERIFY_DAYS) } else { f })
}

/// (#327) A ticker's month-end returns with the month each one ENDS in, as `year*12 + month`. The key
/// is what lets two series of different ages be compared over the same calendar window by index
/// arithmetic instead of by re-slicing dates per pair — the difference between a discovery pass that
/// takes seconds and one that takes an hour on a 6000-name pool.
fn month_series(dates: &[chrono::NaiveDate], closes: &[f64]) -> (Vec<i32>, Vec<f64>) {
    let (mut keys, mut ends) = (Vec::new(), Vec::new());
    for (i, d) in dates.iter().enumerate() {
        let c = match closes.get(i) {
            Some(&c) if c > 0.0 => c,
            _ => continue,
        };
        let k = d.year() * 12 + d.month() as i32;
        if keys.last() == Some(&k) {
            *ends.last_mut().expect("keys and ends are pushed together") = c;
        } else {
            keys.push(k);
            ends.push(c);
        }
    }
    let rets: Vec<f64> = ends.windows(2).map(|w| (w[1] / w[0] - 1.0) * 100.0).collect();
    (keys[1..].to_vec(), rets) // a return is dated by the month it ENDS in, so drop the first key
}

/// (#327) Point-in-time twin discovery INSIDE the backtest, and the reason it has to exist.
///
/// Live `screen` discovers pairs among the funds that are young TODAY — every one of them listed after
/// 2021, because an older fund has its own leg and is never a rescue subject. Not one of those names can
/// contribute a graded sample: a 2021 entry's 8-year window does not close until 2029. Applied to the
/// backtest, the live map is therefore provably inert, and an arm that equals its control has measured
/// nothing at all. So the walk pairs the names that WERE young at the cutoffs it grades, by the same
/// three-legged rule, each pair frozen at that listing's own first eligibility and judged only on bars
/// from inside it. No config, no hindsight, no live file.
///
/// `series` is (ticker, month keys, month returns) per name. Deterministic: best correlation, then the
/// donor with the longest record, then ticker ascending — the same total order live discovery uses.
/// (#650) Was a tuple compare that gave a full tie to the ticker sorting LAST.
fn discover_backtest_proxies(series: &[(&str, i32, Vec<i32>, Vec<f64>)]) -> BTreeMap<String, String> {
    let gap = (picks::PROXY_MIN_AGE_GAP_YEARS * 12.0) as i32;
    // returns over the young listing's verify window, plus the last month key in it
    let window = |s: &(&str, i32, Vec<i32>, Vec<f64>)| -> Option<(i32, Vec<f64>)> {
        let end = s.1 + (PROXY_VERIFY_DAYS / 30) as i32;
        let n = s.2.partition_point(|k| *k <= end);
        (n >= core::PROXY_MIN_MONTHS).then(|| (end, s.3[..n].to_vec()))
    };
    series
        .par_iter()
        .filter_map(|young| {
            let (end, own) = window(young)?;
            let mut best: Option<(f64, i32, &str)> = None;
            for d in series.iter().filter(|d| d.0 != young.0 && d.1 <= young.1 - gap) {
                let (lo, hi) = (d.2.partition_point(|k| *k < young.1), d.2.partition_point(|k| *k <= end));
                let Some(rho) = core::proxy_corr(&own, &d.3[lo..hi]) else { continue };
                if best.is_none_or(|(r, f, t)| {
                    rho.total_cmp(&r).then(f.cmp(&d.1)).then(t.cmp(d.0)).is_gt()
                }) {
                    best = Some((rho, d.1, d.0));
                }
            }
            best.map(|(_, _, donor)| (young.0.to_string(), donor.to_string()))
        })
        .collect()
}

/// (#531) Share turnover, NEGATED so high = unpopular (Ibbotson-Chen-Kim-Hu 2013): minus the shares
/// traded in the 365 days ending at `at`, over the as-of share count. Summed over calendar days, not
/// bars, so a monthly, weekly or daily series reads the same year (#117). `own` is the listing's own
/// (date, volume) record, so a (#327) donor's years never lend volume. None unless that record covers
/// the whole year with volume on every bar, since one unreported bar would read as quiet trading.
/// shortcut: an ADR's volume counts ADSs and `shares_ttm` the ordinary shares, so a ratio other than 1
/// (#425) mis-ranks it by that ratio, as `mcap` does.
fn low_turnover(own: &[(chrono::NaiveDate, f64)], at: chrono::NaiveDate, shares: Option<f64>) -> Option<f64> {
    let sh = shares.filter(|s| *s > 0.0)?;
    let from = at - chrono::Duration::days(365);
    if own.first()?.0 > from {
        return None;
    }
    let year = &own[own.partition_point(|(d, _)| *d <= from)..own.partition_point(|(d, _)| *d <= at)];
    if year.is_empty() || !year.iter().all(|(_, v)| *v > 0.0) {
        return None;
    }
    Some(-year.iter().map(|(_, v)| v).sum::<f64>() / sh)
}

/// (#327) Apply a configured proxy pair to ONE backtest series, and re-prove it first.
///
/// The map is only a candidate list — live `screen` built it from today's trails, and taking that on trust at a
/// 2005 cutoff would be look-ahead in the pairing itself. So the pair is re-judged here on the young listing's
/// FIRST `PROXY_VERIFY_DAYS`, the earliest evidence that ever existed for it, with the donor cut to the same
/// window so `proxy_corr`'s tail alignment is date alignment. Refused -> the plain series, unchanged, and the
/// ticker simply keeps failing the history gate as it does today.
///
/// Returns the (possibly spliced) series and whether it was. The CALLER still owes the point-in-time guard:
/// the spliced series starts years before this ticker existed, and nothing in here stops a walk from standing
/// in 1999 and buying a 2019 fund.
fn apply_proxy(
    tk: &str,
    dates: Vec<chrono::NaiveDate>,
    closes: Vec<f64>,
    map: &BTreeMap<String, String>,
    donors: &HashMap<&str, (Vec<chrono::NaiveDate>, Vec<f64>)>,
) -> (Vec<chrono::NaiveDate>, Vec<f64>, bool) {
    let (Some(donor), Some(&first)) = (map.get(tk).and_then(|p| donors.get(p.as_str())), dates.first()) else {
        return (dates, closes, false);
    };
    let end = first + chrono::Duration::days(PROXY_VERIFY_DAYS);
    let cut = |d: &[chrono::NaiveDate], c: &[f64]| {
        let n = d.iter().take_while(|x| **x <= end).count();
        core::monthly_returns_tail(&d[..n], &c[..n], usize::MAX)
    };
    if core::proxy_corr(&cut(&dates, &closes), &cut(&donor.0, &donor.1)).is_none() {
        return (dates, closes, false);
    }
    match core::splice_history(&dates, &closes, &donor.0, &donor.1) {
        Some((d, c)) => (d, c, true),
        None => (dates, closes, false), // donor adds nothing older, or no overlap at the seam
    }
}

/// (#327) Bar (ii) for the proxy round, and the reason a `gate_notches` row cannot serve it: a notch re-runs the
/// gates on the SAME quotes, while the proxy changes the SERIES each quote was built from — no amount of re-scoring
/// reconstructs that. So the split is by PROVENANCE instead: the book the screen cleared on its names' own bars,
/// against that book plus the names only the borrowed history made assessable. Same pool, same equal-weight-per-bucket
/// arithmetic and the same `guard_holds` bar as the notch shadow, so the two tables are read identically. Empty
/// second row = the splice admitted nobody at this horizon, which is a finding, not a failure.
fn proxy_books(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) -> Vec<(String, usize, (f64, f64, f64, f64, f64, f64, f64))> {
    let mut own: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    let mut borrowed: Vec<(i32, (f64, f64, f64))> = Vec::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        let Some(br) = benchmark_fwd(&bench.0, &bench.1, s.date, years) else { continue };
        if growth_score(&s.quote, tuning).is_none() {
            continue;
        }
        let row = (0.0, s.realized, br);
        if s.quote.history_proxied {
            borrowed.push((bucket(s.date), row));
        } else {
            own.entry(bucket(s.date)).or_default().push(row);
        }
    }
    let Some(base) = book_stats(&own, usize::MAX, years) else { return Vec::new() };
    let mut rows = vec![("own history".to_string(), own.values().map(Vec::len).sum(), base)];
    if !borrowed.is_empty() {
        let mut union = own.clone();
        for (b, row) in &borrowed {
            union.entry(*b).or_default().push(*row);
        }
        rows.extend(book_stats(&union, usize::MAX, years).map(|st| ("+ proxied".to_string(), borrowed.len(), st)));
    }
    rows
}

/// (#327) Prints `proxy_books` against the same bar. Print-only, like `report_notch_books`.
#[mutants::skip]
fn report_proxy_books(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let rows = proxy_books(samples, bench, years, tuning);
    let Some((_, _, base)) = rows.first().cloned() else { return };
    println!("\n── PROXY BOOK (#327): the cleared book split by where its history came from, every name equal-weight, held {years}y ──");
    for (l, n, st) in rows {
        let (b, _, e, w, wo, el, la) = st;
        let guard = proxy_guard(l == "own history", &base, &st);
        println!("  {l:<12} n={n:<6} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{guard}");
    }
    println!("  (n = samples. Guard, pre-registered: a borrowed-history name must leave the book no worse — excess >= -0.1 and worst >= -1.0 against the own-history row, at 20y, 12y and 8y. Keeps (#324)'s flat bar: this row SPLITS the cleared book rather than admitting out of the refused pool, so (#329)'s matched-n null does not describe it)");
}

/// (#325) The forward half of the reopen rule, replayed where no ship rule ever looked. Every graded horizon
/// (20/12/8y) needs its forward window to close, so none grades an entry after `since` (the last one the 8y run
/// closes), and `track` only starts in 2026. A short run (`backtest 1 long ...`) holds those entries. Per ~6-month
/// bucket after `since`: the notch cohort's mean `realized` minus the cleared book's (the gap `track` reads, cohort
/// minus the book), a line only where both sides hold a name. Pool and cohort rule are `notch_books`'. Row: (tag,
/// lines, mean gap, median gap), `track::monthly_gaps`' shape and (#1035) its averaged median, for each notch with
/// at least one line.
fn notch_tail(samples: &[Sample], since: chrono::NaiveDate, tuning: &BuyHeuristic) -> Vec<(&'static str, usize, f64, f64)> {
    let notches = notch_tunings(tuning);
    let mut cleared: std::collections::BTreeMap<i32, Vec<f64>> = Default::default();
    let mut added: Vec<std::collections::BTreeMap<i32, Vec<f64>>> = vec![Default::default(); notches.len()];
    for s in samples.iter().filter(|s| s.date > since && picks::asset_class(&s.quote) != 0) {
        if growth_score(&s.quote, tuning).is_some() {
            cleared.entry(bucket(s.date)).or_default().push(s.realized);
            continue;
        }
        for ((_, t), cohort) in notches.iter().zip(added.iter_mut()) {
            if growth_score(&s.quote, t).is_some() {
                cohort.entry(bucket(s.date)).or_default().push(s.realized);
            }
        }
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    notches
        .iter()
        .zip(added)
        .filter_map(|((tag, _), cohort)| {
            let mut gaps: Vec<f64> = cohort.iter().filter_map(|(b, v)| cleared.get(b).map(|k| mean(v) - mean(k))).collect();
            gaps.sort_by(f64::total_cmp);
            let (n, m) = (gaps.len(), mean(&gaps));
            crate::core::median(gaps).map(|med| (*tag, n, m, med))
        })
        .collect()
}

/// (#325) Prints `notch_tail` with `track`'s own verdict, so both halves of the rule read one function. `since` is
/// the last entry the shortest graded horizon (8y) closes: the latest cutoff plus `years`, less 8. At 8y and longer
/// nothing is after it and the section is absent (every golden). Print-only, so `replace with ()` is unkillable.
#[mutants::skip]
fn report_notch_tail(samples: &[Sample], years: i64, tuning: &BuyHeuristic) {
    let Some(since) = samples
        .iter()
        .map(|s| s.date)
        .max()
        .and_then(|d| d.checked_add_months(chrono::Months::new(12 * years as u32)))
        .and_then(|d| d.checked_sub_months(chrono::Months::new(96)))
    else {
        return;
    };
    let rows = notch_tail(samples, since, tuning);
    if rows.is_empty() {
        return;
    }
    println!("\n── NOTCH TAIL (#325): entries after {since}, which no graded horizon (20/12/8y) closes; each notch's cohort minus the cleared book, one line per ~6-month bucket, held {years}y ──");
    for (tag, n, mean, median) in rows {
        println!("  {tag:<10} lines {n:<3} mean {mean:+.1}  median {median:+.1} pts  {}", crate::commands::track::reopen_verdict(n, mean, median));
    }
    println!("  (Receipt (#325): this stands in for `track`'s forward half. A notch ships on REOPEN SIGNAL here AND its NOTCH BOOK guard at 20y, 12y and 8y AND a green backtest gate; one notch a round)");
}

/// (round 108) ENTRY-STATE — the WHEN dimension, never measured before this round: every prior lane
/// conditioned on the PICK; this conditions on what the MARKET looked like the day money went in.
/// Buckets are classed by the benchmark's drawdown from its running high at the bucket's first
/// cutoff, then the SAME top-10 growth book is graded per class. Report-only, and the guidance can
/// only ever be "deploy new money faster when a state occurs" — never "wait in cash for it" (the
/// table can't see cash drag, and waiting is the classic market-timing trap).
/// Returns the unconditional "all entries" held-book stats (book, excess, win, worst, oos_early,
/// oos_late, windows) — the same numbers it prints on the `[unconditional]` row — so run() can
/// journal them as the method verdict without recomputing. None when the sample can't form a book.
fn report_entry_state(
    samples: &[Sample],
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    tuning: &BuyHeuristic,
) -> Option<(f64, f64, f64, f64, f64, f64, usize)> {
    let (bd, bc) = bench;
    if bd.len() < 2 {
        return None;
    }
    // full price pool (gated, non-crypto, benchmarkable — no fund filter): the "all entries" row
    // must reproduce the absolute held-book above, so the class rows split THAT number, not a subset.
    let mut base: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    let mut first: std::collections::BTreeMap<i32, chrono::NaiveDate> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue;
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        let bk = bucket(s.date);
        base.entry(bk).or_default().push((score, s.realized, br));
        first.entry(bk).and_modify(|d| *d = (*d).min(s.date)).or_insert(s.date);
    }
    if base.is_empty() {
        return None;
    }
    let n = 10;
    println!("\n── ENTRY-STATE (class each ~6mo entry window by the benchmark's drawdown at entry, same top-{n} growth book held {years}y) ──");
    let classes: &[(&str, fn(f64) -> bool)] = &[
        ("near-high (dd > -5%)", |d| d > -5.0),
        ("pullback  (-15 < dd <= -5%)", |d| d > -15.0 && d <= -5.0),
        ("drawdown  (dd <= -15%)", |d| d <= -15.0),
    ];
    for (label, is) in classes {
        let mut m: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        let mut dds = Vec::new();
        for (bk, rows) in &base {
            let Some(dd) = first.get(bk).and_then(|d| bench_drawdown_at(bd, bc, *d)) else { continue };
            if is(dd) {
                m.insert(*bk, rows.clone());
                dds.push(dd);
            }
        }
        if let Some((b, _, e, w, wo, el, la)) = book_stats(&m, n, years) {
            let mdd = dds.iter().sum::<f64>() / dds.len() as f64;
            println!(
                "  {label:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}{}, mean entry dd {mdd:+.1}%)",
                dds.len(),
                n_eff_tag(tuning.print_n_eff, dds.len(), years)
            );
        }
    }
    if let Some((b, _, e, w, wo, el, la)) = book_stats(&base, n, years) {
        println!(
            "  {:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}{}) [unconditional]",
            "all entries",
            base.len(),
            n_eff_tag(tuning.print_n_eff, base.len(), years)
        );
    }
    // The JOURNALED row, and the only one the screen footer quotes: the top-VERDICT_TOP basket a
    // reader can actually buy, not the top-10 the table above ranks by. Printed so the footer's
    // numbers are auditable inside the run that earned them.
    let verdict = book_stats(&base, VERDICT_TOP, years).map(|(b, _, e, w, wo, el, la)| {
        println!(
            "  {:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}{}) [unconditional, JOURNALED]",
            format!("all entries (top-{VERDICT_TOP})"),
            base.len(),
            n_eff_tag(tuning.print_n_eff, base.len(), years)
        );
        (b, e, w, wo, el, la, base.len())
    });
    println!("  (a class with a handful of windows is a regime story, not a statistic. If a state over-delivers, the");
    println!("   guidance is DEPLOY NEW MONEY FASTER when it occurs — never hold cash waiting; the table can't see cash drag.)");
    verdict
}

/// (#1238) The level x path 2x2 that (#110) asked for and (#306) priced unmeasurable: each entry bucket of
/// [`report_entry_state`]'s pool is classed CHEAP or RICH by the S&P's trailing earnings yield minus the 3M
/// bill at its first cutoff (inside the `entry_excess_yield_band` dead zone it sits out), and HIGH or FALL by
/// the benchmark's drawdown there (FALL at or below `cut`), then the same top-10 growth book is graded per
/// corner. Record only. Empty when either series is missing or no bucket lands in a corner.
fn level_path_report(
    samples: &[Sample],
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    tuning: &BuyHeuristic,
    ep: &BTreeMap<i32, f64>,
    bills: &BTreeMap<i32, f64>,
    cut: f64,
) -> String {
    use crate::heuristics::rules::on_month;
    if ep.is_empty() || bills.is_empty() {
        return String::new();
    }
    let ((bd, bc), band, n) = (bench, tuning.entry_excess_yield_band, 10);
    let mut pool: BTreeMap<i32, (chrono::NaiveDate, Vec<(f64, f64, f64)>)> = BTreeMap::new();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue;
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        let e = pool.entry(bucket(s.date)).or_insert((s.date, Vec::new()));
        e.0 = e.0.min(s.date);
        e.1.push((score, s.realized, br));
    }
    let mut cells: [BTreeMap<i32, Vec<(f64, f64, f64)>>; 4] = Default::default();
    for (bk, (d, rows)) in pool {
        let Some(dd) = bench_drawdown_at(bd, bc, d) else { continue };
        let level = match on_month(ep, d) - on_month(bills, d) {
            x if x >= band => 0,
            x if x <= -band => 2,
            _ => continue,
        };
        cells[level + usize::from(dd <= cut)].insert(bk, rows);
    }
    if cells.iter().all(BTreeMap::is_empty) {
        return String::new();
    }
    let mut out = format!(
        "── LEVEL x PATH (#1238): each ~6mo entry classed by the S&P's trailing earnings yield minus the 3M T-bill (CHEAP at or above +{band}, RICH at or below -{band} pts) and its drawdown at entry (FALL at or below {cut}%), same top-{n} growth book held {years}y; one independent trial = {} windows ──",
        2 * years
    );
    for (label, m) in ["cheap @ high", "cheap & falling", "rich @ high", "rich & falling"].iter().zip(&cells) {
        let stats = book_stats(m, n, years).map_or(String::new(), |(b, _, e, w, wo, _, _)| format!("  book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}"));
        out += &format!("\n  {label:<16} windows {:>3}{stats}", m.len());
    }
    out + "\n  (record only. The read is the two corners where level and path disagree, cheap @ high and rich & falling; a corner under one trial is a regime story, not a statistic)"
}

/// (#290) One ~6-month bucket of the book `size` would actually have bought: `book` and `spy` are
/// terminal multiples (1.0 = flat), `deployed` is the percent of gross the weights placed in names.
struct SizedBucket {
    book: f64,
    spy: f64,
    deployed: f64,
}

/// (#290) The JOURNALED pool (the same filter `report_entry_state` builds `base` with) under the
/// weighting the user executes instead of an equal-weight top-N: per bucket, rank by growth score,
/// cut at `top_picks` (the ranked book `size --picks` reads), then hand those quotes to
/// `size::sized_book`, the ONE spelling of issuer dedupe + score/vol weights + name and sector caps.
/// Whatever the weights leave unallocated earns `spill`: (#308) the indexes the live spill's trackers
/// follow, at `size::spill_split`'s weights. A leg that starts after the entry, or has no full window
/// left, earns the row's benchmark leg instead, and with no legs at all (offline, the daily lane, the
/// total-return lane) the remainder earns the benchmark leg bit-for-bit as (#290) shipped it.
fn sized_buckets(
    samples: &[Sample],
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    tuning: &BuyHeuristic,
    sz: &config::Sizing,
    top: usize,
    spill: &[(f64, &(Vec<chrono::NaiveDate>, Vec<f64>))],
) -> BTreeMap<i32, SizedBucket> {
    let (bd, bc) = bench;
    let mut pool: BTreeMap<i32, Vec<(f64, &Sample, f64)>> = BTreeMap::new();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue;
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        pool.entry(bucket(s.date)).or_default().push((score, s, br));
    }
    pool.into_iter()
        .map(|(bk, mut rows)| {
            rows.sort_by(|a, b| b.0.total_cmp(&a.0));
            rows.truncate(top);
            let quotes: Vec<&Quote> = rows.iter().map(|r| &*r.1.quote).collect();
            // (weight fraction, name multiple, bench multiple, remainder multiple) per funded row, joined back by identity.
            let legs: Vec<(f64, f64, f64, f64)> = crate::commands::size::sized_book(&quotes, tuning, sz, None, picks::btc_1y(quotes.iter().copied()), &Default::default())
                .iter()
                .filter_map(|(q, _, w, _)| rows.iter().find(|r| std::ptr::eq(*q, &*r.1.quote)).map(|r| {
                    let bench = 1.0 + r.2 / 100.0;
                    let hold = if spill.is_empty() {
                        bench
                    } else {
                        spill
                            .iter()
                            .map(|(sw, (d, c))| {
                                let fwd = d.first().is_some_and(|f| *f <= r.1.date).then(|| benchmark_fwd(d, c, r.1.date, years)).flatten();
                                sw * (1.0 + fwd.unwrap_or(r.2) / 100.0)
                            })
                            .sum()
                    };
                    (w / 100.0, 1.0 + r.1.realized / 100.0, bench, hold)
                }))
                .collect();
            let spy = legs.iter().map(|l| l.2).sum::<f64>() / legs.len() as f64;
            let hold = legs.iter().map(|l| l.3).sum::<f64>() / legs.len() as f64;
            let deployed = legs.iter().map(|l| l.0).sum::<f64>();
            let book = legs.iter().map(|l| l.0 * l.1).sum::<f64>() + (1.0 - deployed) * hold;
            (bk, SizedBucket { book, spy, deployed: deployed * 100.0 })
        })
        .collect()
}

/// (#290) Prints the sized book on the JOURNALED row's columns. The stats are `book_stats`' own:
/// each bucket becomes one row, and top-1 of a one-row bucket is that row. Diagnostic only; SHIP RULE
/// and the screen's method footer stay on the equal-weight top-`VERDICT_TOP`. (#309) A point-in-time run
/// journals the row it returns to [`SIZED_FILE`], and `screen` cites it under BUY NOW.
fn report_sized_book(
    samples: &[Sample],
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    tuning: &BuyHeuristic,
    sz: &config::Sizing,
    top: usize,
    spill: &[(f64, &(Vec<chrono::NaiveDate>, Vec<f64>))],
) -> Option<SizedVerdict> {
    let sized = sized_buckets(samples, bench, years, tuning, sz, top, spill);
    let m: BTreeMap<i32, Vec<(f64, f64, f64)>> =
        sized.iter().map(|(bk, s)| (*bk, vec![(0.0, (s.book - 1.0) * 100.0, (s.spy - 1.0) * 100.0)])).collect();
    if let Some((b, _, e, w, wo, el, la)) = book_stats(&m, 1, years) {
        let deployed = sized.values().map(|s| s.deployed).sum::<f64>() / sized.len() as f64;
        println!(
            "  {:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}, deployed {deployed:.0}%) [diagnostic: the book `size` funds, top-{top}; remainder earns the index]",
            "all entries (SIZED)",
            sized.len(),
        );
        // (#309) a point-in-time run journals this to SIZED_FILE, so `screen` can cite it under BUY NOW.
        return Some(SizedVerdict { windows: sized.len(), book: b, excess: e, worst: wo, deployed, sizing_fp: sizing_fingerprint(sz, top), roll: None });
    }
    None
}

/// (#311) A close on the first bar at or after `d` (the walk's own forward rule). None when the series ends
/// before `d`, or when that close is no price: a zero would value a lot at infinity.
fn px_at(s: &(Vec<chrono::NaiveDate>, Vec<f64>), d: chrono::NaiveDate) -> Option<f64> {
    s.1.get(s.0.partition_point(|x| *x < d)).copied().filter(|p| *p > 0.0)
}

/// (#340) Priced on BOTH ends of a roll window, the liveness rule `report_roll`'s book is built on. A helper
/// so the `&&` is graded by a unit test; inline, it was reachable only through the sized journal. (#757) A `dead`
/// series needs only its entry price: [`book_multiple`] pays its terminal past its last bar, so a name that dies
/// inside the program stays in the book instead of handing its slot to a survivor.
fn priced_at_both(s: &(Vec<chrono::NaiveDate>, Vec<f64>), d: chrono::NaiveDate, end: chrono::NaiveDate, dead: bool) -> bool {
    px_at(s, d).is_some() && (dead || px_at(s, end).is_some())
}

/// (#311) The names a DCA buy on `d` picks from: each ticker's LATEST point-in-time cutoff in the six months up
/// to `d`, coins out. Nothing dated after `d` enters, so there is no look-ahead. (#790) The walk cuts each name
/// once per half-year bucket, on its first bar there, so the window is closed at BOTH ends: a daily name cut on
/// 1 July is still the latest cutoff for the 1 January buy, and every month sees the whole pond. Ticker-ordered,
/// so a tie ranks the same way every run. `books` is date-sorted.
fn month_pool(books: &[Sample], d: chrono::NaiveDate) -> Vec<&Sample> {
    let from = d - chrono::Months::new(6);
    let window = &books[books.partition_point(|s| s.date < from)..books.partition_point(|s| s.date <= d)];
    let mut latest: BTreeMap<&str, &Sample> = BTreeMap::new();
    for s in window.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        latest.insert(s.quote.ticker.as_str(), s);
    }
    latest.into_values().collect()
}

/// (#311) What €1 put into `book` (ticker, weight fraction) on `from` is worth on `to`. The unfunded remainder
/// earns `legs` as [`sized_buckets`] prices it: a leg that does not reach back to `from` borrows the benchmark.
/// None when any leg is unpriced. (#757) A name in `dead`'s set whose series ends before `to` pays the walk's
/// terminal ([`dead_multiple`], `dead`'s crash penalty); any other name that ends early prices nothing.
fn book_multiple(
    book: &[(&str, f64)],
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    series: &HashMap<&str, (Vec<chrono::NaiveDate>, Vec<f64>)>,
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    legs: &[(f64, &(Vec<chrono::NaiveDate>, Vec<f64>))],
    dead: (&HashSet<&str>, f64),
) -> Option<f64> {
    let grow = |s: &(Vec<chrono::NaiveDate>, Vec<f64>)| Some(px_at(s, to)? / px_at(s, from)?);
    let held = |t: &str, s: &(Vec<chrono::NaiveDate>, Vec<f64>)| match s.0.last() {
        Some(l) if *l < to && dead.0.contains(t) => dead_multiple(&s.0, &s.1, px_at(s, from)?, to, (bench.0.as_slice(), bench.1.as_slice(), dead.1)),
        _ => grow(s),
    };
    let names = book.iter().map(|(t, w)| Some(w * held(t, series.get(t)?)?)).sum::<Option<f64>>()?;
    let rest = legs.iter().map(|(w, s)| Some(w * grow(if s.0.first().is_some_and(|f| *f <= from) { s } else { bench })?)).sum::<Option<f64>>()?;
    Some(names + (1.0 - book.iter().map(|(_, w)| w).sum::<f64>()) * rest)
}

/// (#311) What [`DCA_MONTHS`] €1 buys, a month apart, grow to by month `2 × ROLL_MONTHS` at `r` a year.
fn dca_fv(r: f64) -> f64 {
    (0..DCA_MONTHS).map(|m| (1.0 + r).powf(f64::from(2 * ROLL_MONTHS - m) / 12.0)).sum()
}

/// (#316) How many programs' never-sell wealth beat the same buys compounded at Série E's best case. The wealth is
/// after tax and Série E is not, so the count is strict on the book.
fn dca_beats_serie_e(holds: &[f64]) -> usize {
    let hurdle = dca_fv(crate::core::serie_e_best_pct() / 100.0);
    holds.iter().filter(|w| **w > hurdle).count()
}

/// (#311) The money-weighted %/yr of a DCA program: the one rate at which [`DCA_MONTHS`] €1 buys, a month apart,
/// grow to `wealth` by month `2 × ROLL_MONTHS`. Bisection on (−100%, +100%), since the future value rises with
/// the rate. A program ending with exactly what it put in reads at or below zero, never a phantom gain.
fn dca_irr(wealth: f64) -> f64 {
    let (mut lo, mut hi) = (-1.0, 1.0);
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        if dca_fv(mid) < wealth {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0 * 100.0
}

/// (#311) One DCA program's after-tax € per arm.
struct Dca {
    roll: f64,
    hold: f64,
    index: f64,
}

/// (#311) One program starting `start`: [`DCA_MONTHS`] €1 buys a month apart, each into `book(buy date, end)`,
/// every lot sold at `end = start + 2 × ROLL_MONTHS`. Never-sell holds each lot to `end`. The roll sells it
/// [`ROLL_MONTHS`] after its buy, pays the gains tax for that hold (no loss credited, (#310)'s rule), and puts the
/// net into that month's book to `end`. The index takes the same € into the benchmark. Any missing price
/// drops the whole program, because a partial one would grade fewer € than it spent.
fn dca_program<B>(
    start: chrono::NaiveDate,
    book: impl Fn(chrono::NaiveDate, chrono::NaiveDate) -> Option<B>,
    value: impl Fn(&B, chrono::NaiveDate, chrono::NaiveDate) -> Option<f64>,
    index: impl Fn(chrono::NaiveDate, chrono::NaiveDate) -> Option<f64>,
    base: f64,
    schedule: &[crate::config::CgtRung],
) -> Option<Dca> {
    let at = |m: u32| start.checked_add_months(chrono::Months::new(m));
    let end = at(2 * ROLL_MONTHS)?;
    let net = |x: f64, held: u32| x - cgt_rate(i64::from(held / 12), base, schedule) * (x - 1.0).max(0.0);
    let mut out = Dca { roll: 0.0, hold: 0.0, index: 0.0 };
    for m in 0..DCA_MONTHS {
        let (buy, sale) = (at(m)?, at(m + ROLL_MONTHS)?);
        let (first, second) = (book(buy, end)?, book(sale, end)?);
        out.hold += net(value(&first, buy, end)?, 2 * ROLL_MONTHS - m);
        out.roll += net(value(&first, buy, sale)?, ROLL_MONTHS) * net(value(&second, sale, end)?, ROLL_MONTHS - m);
        out.index += net(index(buy, end)?, 2 * ROLL_MONTHS - m);
    }
    Some(out)
}

/// (#311) DCA ROLL-AFTER-[`ROLL_YEARS`]y. (#310) graded one lump buy; this grades the plan the money follows:
/// [`DCA_MONTHS`] equal monthly buys into that month's screen, all sold at `years`, against selling each lot at
/// [`ROLL_YEARS`] and re-buying that month's screen. Both are after tax, as money-weighted %/yr, one program per
/// half-year start ([`dca_program`]). Each month is ranked once over [`month_pool`]; a program's book is the
/// ranked names still priced at its end, the same filter in both arms. (#757) Under `dead` a name that dies inside
/// the program stays, on the walk's terminal ([`book_multiple`]), so the ruler is no longer survivors only. A month
/// with no such name puts the whole € on the legs, as BUY NOW does: its picks and trackers always sum to gross.
/// Top-10 is a reading. SIZED is the pre-registered bar, and its Δ is returned for the journal. Empty `books` (any
/// other horizon) prints nothing.
/// (#560) `rivals` adds the momentum rival's lane on the same months (`mom12-1`, named for its (#560) key; since
/// (#587) it follows `growth_momentum_key`): the DCA re-grade a rival that beats
/// the top-10 ruler owes before anything ships. (#584) and the blend's: the top-10 lane's order spliced with
/// momentum's as the page does ([`blend_top`]; (#630) it was the lane's first 10 minus `growth_momentum_slots`
/// names, then momentum's); then the SIZED book with momentum spliced in
/// ([`picks::sized_splice`]), after its head and interleaved, the two ways a shipped blend could order BUY NOW.
fn report_roll(
    samples: &[Sample],
    books: &[Sample],
    series: &HashMap<&str, (Vec<chrono::NaiveDate>, Vec<f64>)>,
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    tuning: &BuyHeuristic,
    sz: &config::Sizing,
    top: usize,
    spill: &[(f64, &(Vec<chrono::NaiveDate>, Vec<f64>))],
    rivals: bool,
    dead: (&HashSet<&str>, f64),
) -> Option<f64> {
    if books.is_empty() {
        return None;
    }
    let open = picks::gates_off(tuning);
    let base = tuning.capital_gains_tax_pct / 100.0;
    let legs = if spill.is_empty() { vec![(1.0, bench)] } else { spill.to_vec() };
    // one program per entry bucket the `years` SIZED row grades: the era its pond is on file, so a start whose
    // screen was empty grades the legs' roll, not the screen's
    let starts: Vec<chrono::NaiveDate> = sized_buckets(samples, bench, years, tuning, sz, top, spill)
        .into_keys()
        .filter_map(|b| chrono::NaiveDate::from_ymd_opt(b / 2, (b % 2 * 6 + 1) as u32, 1))
        .collect();
    let months: std::collections::BTreeSet<chrono::NaiveDate> = starts
        .iter()
        .flat_map(|s| (0..DCA_MONTHS).flat_map(|m| [m, m + ROLL_MONTHS]).filter_map(move |m| s.checked_add_months(chrono::Months::new(m))))
        .collect();
    // per month: the ladder's pool (growth-gated, value brake) in rank order, the SIZED pool by growth score, and
    // under `rivals` the momentum 12-1 order
    let ranked: BTreeMap<chrono::NaiveDate, (Vec<&Sample>, Vec<&Sample>, Vec<&Sample>)> = months
        .into_par_iter()
        .map(|d| {
            let pool = month_pool(books, d);
            let quotes: Vec<&Quote> = pool.iter().map(|s| s.quote.as_ref()).collect();
            let rows: Vec<_> = pool
                .iter()
                .zip(picks::growth_scores_ranked(&quotes, tuning))
                .filter_map(|(s, sc)| Some((0, sc?, 0.0, 0.0, s.quote.ticker.clone(), s.fund.as_ref().and_then(|f| f.peg_yield))))
                .collect();
            let by_tk: HashMap<&str, &Sample> = pool.iter().map(|s| (s.quote.ticker.as_str(), *s)).collect();
            let mut kept = value_floor_trim(&rows, tuning.growth_value_floor_pct).remove(&0).unwrap_or_default();
            kept.sort_by(|x, y| y.0.total_cmp(&x.0));
            let mut sized: Vec<(f64, &Sample)> = pool.iter().filter_map(|s| Some((growth_score(&s.quote, tuning)?, *s))).collect();
            sized.sort_by(|a, b| b.0.total_cmp(&a.0));
            let mom = if rivals { momentum_ranked(&pool, &picks::growth_scores_ranked(&quotes, &open), tuning) } else { Vec::new() };
            (d, (kept.iter().map(|x| by_tk[x.3.as_str()]).collect(), sized.into_iter().map(|x| x.1).collect(), mom))
        })
        .collect();
    let alive = |s: &Sample, d, end| {
        let tk = s.quote.ticker.as_str();
        series.get(tk).is_some_and(|x| priced_at_both(x, d, end, dead.0.contains(tk)))
    };
    let capped = |d: chrono::NaiveDate, end: chrono::NaiveDate, n: usize| -> Option<Vec<&Sample>> {
        let v: Vec<&Sample> = ranked.get(&d)?.0.iter().copied().filter(|s| alive(s, d, end)).collect();
        let v = picks::sector_cap(v, tuning.growth_sector_cap, |s| picks::stock_sector(&s.quote)); // (#557)
        let trails: Vec<&[f64]> = v.iter().map(|s| s.trail.as_slice()).collect();
        Some(corr_cap_rung(&v, &trails, n, tuning.growth_corr_cap))
    };
    let top10 = |d, end| -> Option<Vec<(&str, f64)>> { Some(equal_top(capped(d, end, VERDICT_TOP)?.into_iter(), VERDICT_TOP)) };
    // (#850) a sized row's weight is a percent and the ruler reads a fraction: one conversion for both sized books
    let pct = |w: f64| w / 100.0;
    let sized = |d, end| -> Option<Vec<(&str, f64)>> {
        let q: Vec<&Quote> = ranked.get(&d)?.1.iter().filter(|s| alive(s, d, end)).take(top).map(|s| s.quote.as_ref()).collect();
        Some(crate::commands::size::sized_book(&q, tuning, sz, None, picks::btc_1y(q.iter().copied()), &Default::default()).into_iter().map(|(q, _, w, _)| (q.ticker.as_str(), pct(w))).collect())
    };
    let mom = |d, end| -> Option<Vec<(&str, f64)>> { Some(equal_top(ranked.get(&d)?.2.iter().copied().filter(|s| alive(s, d, end)), VERDICT_TOP)) };
    let blend = |d, end| -> Option<Vec<(&str, f64)>> {
        Some(blend_top(capped(d, end, VERDICT_TOP)?, ranked.get(&d)?.2.iter().copied().filter(|s| alive(s, d, end)), tuning.growth_momentum_slots, VERDICT_TOP))
    };
    // (#584) the SIZED book with momentum names spliced in, `after` its head or interleaved
    let sized_blend = |d: chrono::NaiveDate, end: chrono::NaiveDate, after: bool| -> Option<Vec<(&str, f64)>> {
        let q: Vec<&Quote> = ranked.get(&d)?.1.iter().filter(|s| alive(s, d, end)).take(top).map(|s| s.quote.as_ref()).collect();
        let book: Vec<(&Quote, f64)> = crate::commands::size::sized_book(&q, tuning, sz, None, picks::btc_1y(q.iter().copied()), &Default::default()).into_iter().map(|r| (r.0, r.2)).collect();
        let mom: Vec<&Quote> = ranked.get(&d)?.2.iter().filter(|s| alive(s, d, end)).map(|s| s.quote.as_ref()).collect();
        Some(picks::sized_splice(&book, &mom, after, tuning.growth_momentum_slots, sz).into_iter().map(|(t, w)| (t, pct(w))).collect())
    };
    let value = |b: &Vec<(&str, f64)>, from, to| book_multiple(b, from, to, series, bench, &legs, dead);
    let index = |from, to| Some(px_at(bench, to)? / px_at(bench, from)?);
    let top_programs: BTreeMap<i32, Dca> =
        starts.par_iter().filter_map(|s| Some((bucket(*s), dca_program(*s, top10, value, index, base, &tuning.cgt_hold_schedule)?))).collect();
    let sized_programs: BTreeMap<i32, Dca> =
        starts.par_iter().filter_map(|s| Some((bucket(*s), dca_program(*s, sized, value, index, base, &tuning.cgt_hold_schedule)?))).collect();
    println!(
        "\n── DCA ROLL-AFTER-{ROLL_YEARS}y (#311): {DCA_MONTHS} equal monthly buys into that month's screen, all sold at {years}y; the roll sells each lot at {ROLL_YEARS}y, pays {:.0}% on the gain, re-buys that month's screen; vs never-sell, after tax, money-weighted ──",
        tuning.capital_gains_tax_pct
    );
    let lane = |name: &str, programs: BTreeMap<i32, Dca>| {
        let cum = |w: f64| ((1.0 + dca_irr(w) / 100.0).powf(years as f64) - 1.0) * 100.0;
        let row = |pick: fn(&Dca) -> (f64, f64)| {
            let rows: BTreeMap<i32, Vec<(f64, f64, f64)>> = programs.iter().map(|(b, p)| (*b, vec![(0.0, cum(pick(p).0), cum(pick(p).1))])).collect();
            let med = median(book_multiples(&rows, 1).iter().map(|(r, h)| ann((r - 1.0) * 100.0, years) - ann((h - 1.0) * 100.0, years)).collect());
            Some((book_stats(&rows, 1, years)?, med))
        };
        let ((roll, never, d, win, worst, early, late), med) = row(|p| (p.roll, p.hold))?;
        println!(
            "  {name:<7} roll {roll:+.1}%/yr  vs never-sell {never:+.1}%/yr  ->  {d:+.1} (med {med:+.1}) pts/yr   win {win:.0}% of {}   worst {worst:+.1}   OOS {early:+.1}/{late:+.1}",
            programs.len()
        );
        // (#312) the plan itself against the same € into the index
        let ((hold, idx, x, win, worst, early, late), med) = row(|p| (p.hold, p.index))?;
        println!(
            "  {name:<7} never-sell {hold:+.1}%/yr  vs index DCA {idx:+.1}%/yr  ->  {x:+.1} (med {med:+.1}) pts/yr   win {win:.0}% of {}   worst {worst:+.1}   OOS {early:+.1}/{late:+.1}",
            programs.len()
        );
        // (#316) the same monthly buys against Série E's best case. CI ratchets the SIZED share (tests/network.rs).
        let k = dca_beats_serie_e(&programs.values().map(|p| p.hold).collect::<Vec<_>>());
        println!(
            "  {name:<7} beat Série E best case: {:.0}% of {} programs ({k}), after tax vs {:.1}%/yr",
            100.0 * k as f64 / programs.len() as f64,
            programs.len(),
            crate::core::serie_e_best_pct()
        );
        Some(d)
    };
    lane("top-10", top_programs);
    if rivals {
        let programs = starts.par_iter().filter_map(|s| Some((bucket(*s), dca_program(*s, mom, value, index, base, &tuning.cgt_hold_schedule)?))).collect();
        lane("mom12-1", programs);
        let programs = starts.par_iter().filter_map(|s| Some((bucket(*s), dca_program(*s, blend, value, index, base, &tuning.cgt_hold_schedule)?))).collect();
        lane("blend", programs);
        let programs = starts.par_iter().filter_map(|s| Some((bucket(*s), dca_program(*s, |d, e| sized_blend(d, e, true), value, index, base, &tuning.cgt_hold_schedule)?))).collect();
        lane("S+after", programs);
        let programs = starts.par_iter().filter_map(|s| Some((bucket(*s), dca_program(*s, |d, e| sized_blend(d, e, false), value, index, base, &tuning.cgt_hold_schedule)?))).collect();
        lane("S+inter", programs);
    }
    lane("SIZED", sized_programs)
}

/// (#1224) The share of a book that a roll from `old` to `new` sells: 1 − the weight both books hold, a name's
/// overlap being the smaller of its two weights. The unfunded remainders earn the same legs ([`book_multiple`]),
/// so the smaller of the two counts as held too.
fn turnover(old: &[(&str, f64)], new: &[(&str, f64)]) -> f64 {
    let rest = |b: &[(&str, f64)]| 1.0 - b.iter().map(|(_, w)| w).sum::<f64>();
    let kept: f64 = old.iter().filter_map(|(t, w)| Some(w.min(new.iter().find(|(u, _)| u == t)?.1))).sum();
    (1.0 - kept - rest(old).min(rest(new))).clamp(0.0, 1.0)
}

/// (#1224) [`dca_program`]'s lots on a `roll`-month cycle: each lot is re-bought into `book(roll date, end)` every
/// `roll` months until `end = start + 2 × ROLL_MONTHS`. `turn(old, new)` is the share of the lot a roll sells: 1 is
/// (#311)'s sell-all, [`turnover`] sells only the leavers. The sold share pays the gains tax on its own gain (no
/// loss credited, (#310)'s rule) and `fee` on its value, and the cost basis of what stays carries over. Never-sell
/// and the index are [`dca_program`]'s. Returns the program and the mean share a roll sold.
/// shortcut: `turn` reads the books' target weights, not the drifted ones, the stayers re-weight untaxed, and the
/// rate counts the hold since the last roll, not each share's own. Sell-all at a flat rate is exact.
fn dca_chain<B>(
    start: chrono::NaiveDate,
    roll: u32,
    fee: f64,
    book: impl Fn(chrono::NaiveDate, chrono::NaiveDate) -> Option<B>,
    value: impl Fn(&B, chrono::NaiveDate, chrono::NaiveDate) -> Option<f64>,
    turn: impl Fn(&B, &B) -> f64,
    index: impl Fn(chrono::NaiveDate, chrono::NaiveDate) -> Option<f64>,
    base: f64,
    schedule: &[crate::config::CgtRung],
) -> Option<(Dca, f64)> {
    let span = 2 * ROLL_MONTHS;
    let at = |m: u32| start.checked_add_months(chrono::Months::new(m));
    let end = at(span)?;
    let rate = |held: u32| cgt_rate(i64::from(held / 12), base, schedule);
    let mut out = Dca { roll: 0.0, hold: 0.0, index: 0.0 };
    let (mut traded, mut rolls) = (0.0, 0u32);
    for m in 0..DCA_MONTHS {
        let buy = at(m)?;
        let mut b = book(buy, end)?;
        let (g, x) = (value(&b, buy, end)?, index(buy, end)?);
        out.hold += g - rate(span - m) * (g - 1.0).max(0.0);
        out.index += x - rate(span - m) * (x - 1.0).max(0.0);
        let (mut last, mut v, mut cost) = (m, 1.0, 1.0);
        for k in (m + roll..span).step_by(roll as usize) {
            v *= value(&b, at(last)?, at(k)?)?;
            let nb = book(at(k)?, end)?;
            let s = turn(&b, &nb);
            let paid = rate(k - last) * (s * (v - cost)).max(0.0) + s * v * fee;
            cost = (1.0 - s) * cost + s * v - paid;
            v -= paid;
            (traded, rolls) = (traded + s, rolls + 1);
            (b, last) = (nb, k);
        }
        v *= value(&b, at(last)?, end)?;
        out.roll += v - rate(span - last) * (v - cost).max(0.0);
    }
    Some((out, traded / f64::from(rolls.max(1))))
}

/// (#1224) The bar, set before the run. `rows` holds each leavers-only rung's SIZED Δ against the shipped roll as
/// (early-half mean, late-half mean, mean, median, programs), None where it graded nothing. The pick is the best
/// early mean above zero (the first on a tie). It ships only if its late half, mean and median are all above zero
/// over at least 12 programs. None when no rung beats the shipped roll on the early half.
fn pick_rung(rows: &[Option<(f64, f64, f64, f64, usize)>]) -> Option<(usize, bool)> {
    let (i, (_, late, mean, med, n)) =
        rows.iter().enumerate().filter_map(|(i, r)| r.filter(|r| r.0 > 0.0).map(|r| (i, r))).min_by(|a, b| b.1.0.total_cmp(&a.1.0))?;
    Some((i, late > 0.0 && mean > 0.0 && med > 0.0 && n >= 12))
}

/// (#1224) `backtest 20 rolls`: how often to re-buy the screen. [`report_roll`]'s programs at every rung from 3
/// months to [`ROLL_YEARS`], two ways: sell-all ((#311)'s rule, each roll sells the whole book) and leavers-only
/// (each roll sells only the [`turnover`]). Every roll pays one round trip's `holds::cost_bps` on what it sells, so
/// the 10y sell-all rung is the shipped roll with its fee. CONTROL is that roll without the fee and must read as
/// (#311)'s lines above. The bar is [`pick_rung`] over the leavers-only rungs' SIZED Δ against the shipped roll. A
/// print only: nothing reads it, and nothing it prints moves a book.
#[mutants::skip] // printing; turnover, dca_chain and pick_rung carry the tests
fn report_roll_ladder(
    samples: &[Sample],
    books: &[Sample],
    series: &HashMap<&str, (Vec<chrono::NaiveDate>, Vec<f64>)>,
    bench: &(Vec<chrono::NaiveDate>, Vec<f64>),
    years: i64,
    tuning: &BuyHeuristic,
    sz: &config::Sizing,
    top: usize,
    spill: &[(f64, &(Vec<chrono::NaiveDate>, Vec<f64>))],
    dead: (&HashSet<&str>, f64),
) {
    use crate::heuristics::holds;
    use chrono::{Months, NaiveDate};
    const RUNGS: [u32; 8] = [3, 6, 12, 24, 36, 60, 84, ROLL_MONTHS];
    const NAMES: [&str; 2] = ["top-10", "SIZED"];
    if books.is_empty() {
        return;
    }
    let span = 2 * ROLL_MONTHS;
    let base = tuning.capital_gains_tax_pct / 100.0;
    let fee = holds::cost_bps("stock", "USD") / 1e4;
    let legs = if spill.is_empty() { vec![(1.0, bench)] } else { spill.to_vec() };
    let starts: Vec<NaiveDate> = sized_buckets(samples, bench, years, tuning, sz, top, spill)
        .into_keys()
        .filter_map(|b| NaiveDate::from_ymd_opt(b / 2, (b % 2 * 6 + 1) as u32, 1))
        .collect();
    let at = |s: NaiveDate, m: u32| s.checked_add_months(Months::new(m));
    // every month a lot can be bought or re-bought in, with its program's end
    let keys: Vec<(NaiveDate, u32)> = starts.iter().flat_map(|s| (0..span).map(move |m| (*s, m))).collect();
    // shortcut: duplicates report_roll's per-month ranking (top-10 and SIZED only) so that function stays
    // untouched; fold the two into one helper when a pass rewrites report_roll
    let months: std::collections::BTreeSet<NaiveDate> = keys.iter().filter_map(|(s, m)| at(*s, *m)).collect();
    let ranked: BTreeMap<NaiveDate, (Vec<&Sample>, Vec<&Sample>)> = months
        .into_par_iter()
        .map(|d| {
            let pool = month_pool(books, d);
            let quotes: Vec<&Quote> = pool.iter().map(|s| s.quote.as_ref()).collect();
            let rows: Vec<_> = pool
                .iter()
                .zip(picks::growth_scores_ranked(&quotes, tuning))
                .filter_map(|(s, sc)| Some((0, sc?, 0.0, 0.0, s.quote.ticker.clone(), s.fund.as_ref().and_then(|f| f.peg_yield))))
                .collect();
            let by_tk: HashMap<&str, &Sample> = pool.iter().map(|s| (s.quote.ticker.as_str(), *s)).collect();
            let mut kept = value_floor_trim(&rows, tuning.growth_value_floor_pct).remove(&0).unwrap_or_default();
            kept.sort_by(|x, y| y.0.total_cmp(&x.0));
            let mut sized: Vec<(f64, &Sample)> = pool.iter().filter_map(|s| Some((growth_score(&s.quote, tuning)?, *s))).collect();
            sized.sort_by(|a, b| b.0.total_cmp(&a.0));
            (d, (kept.iter().map(|x| by_tk[x.3.as_str()]).collect(), sized.into_iter().map(|x| x.1).collect()))
        })
        .collect();
    let alive = |s: &Sample, d, end| {
        let tk = s.quote.ticker.as_str();
        series.get(tk).is_some_and(|x| priced_at_both(x, d, end, dead.0.contains(tk)))
    };
    let top10 = |d: NaiveDate, end: NaiveDate| -> Option<Vec<(&str, f64)>> {
        let v: Vec<&Sample> = ranked.get(&d)?.0.iter().copied().filter(|s| alive(s, d, end)).collect();
        let v = picks::sector_cap(v, tuning.growth_sector_cap, |s| picks::stock_sector(&s.quote));
        let trails: Vec<&[f64]> = v.iter().map(|s| s.trail.as_slice()).collect();
        Some(equal_top(corr_cap_rung(&v, &trails, VERDICT_TOP, tuning.growth_corr_cap).into_iter(), VERDICT_TOP))
    };
    let sized = |d: NaiveDate, end: NaiveDate| -> Option<Vec<(&str, f64)>> {
        let q: Vec<&Quote> = ranked.get(&d)?.1.iter().filter(|s| alive(s, d, end)).take(top).map(|s| s.quote.as_ref()).collect();
        Some(crate::commands::size::sized_book(&q, tuning, sz, None, picks::btc_1y(q.iter().copied()), &Default::default()).into_iter().map(|(q, _, w, _)| (q.ticker.as_str(), w / 100.0)).collect())
    };
    // both books once per (month, end), and each one's worth once per sale month: every rung and ruler reads them
    let held: HashMap<(NaiveDate, NaiveDate), [Option<Vec<(&str, f64)>>; 2]> = keys
        .par_iter()
        .filter_map(|(s, m)| Some((at(*s, *m)?, at(*s, span)?)))
        .map(|(d, e)| ((d, e), [top10(d, e), sized(d, e)]))
        .collect();
    let worth: HashMap<((NaiveDate, NaiveDate), NaiveDate), [Option<f64>; 2]> = keys
        .par_iter()
        .flat_map_iter(|(s, m)| {
            let sales: std::collections::BTreeSet<u32> = RUNGS.iter().map(|r| (m + r).min(span)).chain([span]).collect();
            sales.into_iter().filter_map(move |t| Some((at(*s, *m)?, at(*s, span)?, at(*s, t)?)))
        })
        .map(|(d, e, to)| {
            let b = held.get(&(d, e));
            (((d, e), to), [0, 1].map(|i| book_multiple(b?[i].as_ref()?, d, to, series, bench, &legs, dead)))
        })
        .collect();
    let index = |from, to| Some(px_at(bench, to)? / px_at(bench, from)?);
    let book_of = |i: usize, k: &(NaiveDate, NaiveDate)| held.get(k).and_then(|b| b[i].as_deref());
    let run = |i: usize, roll: u32, fee: f64, leavers: bool| -> BTreeMap<i32, (Dca, f64)> {
        starts
            .par_iter()
            .filter_map(|s| {
                let program = dca_chain(
                    *s,
                    roll,
                    fee,
                    |d, e| book_of(i, &(d, e)).map(|_| (d, e)),
                    |k, _, to| worth.get(&(*k, to))?[i],
                    |a, b| if leavers { turnover(book_of(i, a).unwrap_or_default(), book_of(i, b).unwrap_or_default()) } else { 1.0 },
                    index,
                    base,
                    &tuning.cgt_hold_schedule,
                )?;
                Some((bucket(*s), program))
            })
            .collect()
    };
    let cum = |w: f64| ((1.0 + dca_irr(w) / 100.0).powf(years as f64) - 1.0) * 100.0;
    // `a`'s roll against `pick` of its own program and `b`'s, over the starts both graded: book_stats, median Δ, programs
    let grade = |a: &BTreeMap<i32, (Dca, f64)>, b: &BTreeMap<i32, (Dca, f64)>, pick: fn(&Dca, &Dca) -> f64| {
        let rows: BTreeMap<i32, Vec<(f64, f64, f64)>> =
            a.iter().filter_map(|(k, x)| Some((*k, vec![(0.0, cum(x.0.roll), cum(pick(&x.0, &b.get(k)?.0)))]))).collect();
        let med = median(book_multiples(&rows, 1).iter().map(|(r, h)| ann((r - 1.0) * 100.0, years) - ann((h - 1.0) * 100.0, years)).collect());
        Some((book_stats(&rows, 1, years)?, med, rows.len()))
    };
    let never: fn(&Dca, &Dca) -> f64 = |x, _| x.hold;
    let line = |label: &str, vs: &str, a: &BTreeMap<i32, (Dca, f64)>, b: &BTreeMap<i32, (Dca, f64)>, pick: fn(&Dca, &Dca) -> f64| {
        let g = grade(a, b, pick);
        let sold = a.values().map(|p| p.1).sum::<f64>() / a.len().max(1) as f64 * 100.0;
        match g {
            Some(((r, h, d, win, worst, early, late), med, n)) => println!(
                "  {label:<22} roll {r:+.1}%/yr  vs {vs} {h:+.1}%/yr  ->  {d:+.1} (med {med:+.1}) pts/yr   win {win:.0}% of {n}   worst {worst:+.1}   OOS {early:+.1}/{late:+.1}   sells {sold:.0}%/roll"
            ),
            None => println!("  {label:<22} no program priced"),
        }
        g
    };
    let rung = |r: u32| if r < 12 { format!("{r}m") } else { format!("{}y", r / 12) };
    println!(
        "\n── ROLL LADDER (#1224): (#311)'s {DCA_MONTHS} monthly buys sold at {years}y, each lot re-bought into that month's screen every 3m to {ROLL_YEARS}y, selling the whole book (sell-all) or only the names the new book drops (leavers); a roll pays {:.0} bp on what it sells and {:.0}% on that gain, no loss credited ──",
        fee * 1e4,
        tuning.capital_gains_tax_pct
    );
    for (i, name) in NAMES.iter().enumerate() {
        let control = run(i, ROLL_MONTHS, 0.0, false);
        line(&format!("CONTROL {name:<7} {ROLL_YEARS}y"), "never-sell", &control, &control, never);
    }
    let ladder: Vec<[Vec<BTreeMap<i32, (Dca, f64)>>; 2]> =
        (0..NAMES.len()).map(|i| [false, true].map(|leavers| RUNGS.iter().map(|r| run(i, *r, fee, leavers)).collect())).collect();
    for (i, name) in NAMES.iter().enumerate() {
        for (j, ruler) in ["sell-all", "leavers"].iter().enumerate() {
            for (r, p) in RUNGS.iter().zip(&ladder[i][j]) {
                line(&format!("{:<4}{name:<7} {ruler}", rung(*r)), "never-sell", p, p, never);
            }
        }
    }
    let lanes = &ladder[1];
    let shipped = &lanes[0][RUNGS.len() - 1];
    println!("  SIZED leavers against the shipped roll (sell-all {ROLL_YEARS}y, with the fee):");
    let bar: Vec<Option<(f64, f64, f64, f64, usize)>> = RUNGS
        .iter()
        .zip(&lanes[1])
        .map(|(r, p)| line(&format!("{:<4}SIZED   leavers", rung(*r)), "shipped", p, shipped, |_, y| y.roll).map(|((_, _, d, _, _, e, l), med, n)| (e, l, d, med, n)))
        .collect();
    match pick_rung(&bar).and_then(|(i, ship)| Some((i, ship, bar[i]?))) {
        Some((i, ship, (e, l, d, med, n))) => println!(
            "  PICK {} leavers: early {e:+.1}  late {l:+.1}  mean {d:+.1}  med {med:+.1}  over {n} programs  ->  {}",
            rung(RUNGS[i]),
            if ship { "SHIP" } else { "10y STAYS" }
        ),
        None => println!("  PICK none: no leavers rung beats the shipped roll on the early half  ->  10y STAYS"),
    }
}

/// (#44 Phase C) Grade each FREE as-of fundamental factor on the HELD-BOOK metric: within the
/// growth-gated universe, rank by the factor (not the score), hold the top-N, and compare its held-book
/// excess-vs-S&P500 to ranking by `growth_score`. A factor whose held-book excess BEATS the score with
/// both OOS halves + is a better selector for a 15y no-sell book — a ship candidate for the fund tilt.
///
/// Only the free SEC/income-statement factors are listed (roe, the round-107 survival levels and — since
/// (#43) — roic are all SEC-computed; `FundRow::roic`, the PREMIUM field, is still never populated and is
/// not what the roic row reads). Runs only under `fund` (else `s.fund` is None everywhere).
///
/// Skipped because `replace report_book_by_factor with ()` is unkillable, not merely unkilled. It
/// returns `()` and its entire effect is `println!`, so a test that called it could assert nothing a
/// stub would fail; and the only mode that gets past the `s.fund` check is `fund`, whose goldens the
/// gate skips ((#672): see `sweep_fund_factor`). Measured in (#75)'s audit, where a comment correction
/// inside this body was enough to drag it into `--in-diff` scope and red the gate on its own.
#[mutants::skip]
fn report_book_by_factor(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    if bd.len() < 2 {
        return;
    }
    // (#120) THE SAME basket the verdict ships on. These rows compare candidates against each other,
    // so they want the wider, better-estimated book — and that argument, which was already written
    // here, is exactly the one that moved `VERDICT_TOP` from 3 to 10. Reading the const rather than
    // repeating the literal is what stops the two drifting apart again.
    let n = VERDICT_TOP;
    // baseline: rank the FUND-COVERED gated picks by growth_score. Restricting to fund-covered rows
    // (same universe the factors see) makes the excess head-to-head fair — otherwise the score baseline
    // spans ETF/foreign buckets the SEC factors can't reach and the SPY leg differs.
    let mut base: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 || s.fund.is_none() {
            continue;
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        base.entry(bucket(s.date)).or_default().push((score, s.realized, br));
    }
    println!("\n── held-book by FACTOR (rank FUND-COVERED gated picks by each FREE factor, top-{n} held {years}y, vs growth_score) ──");
    if let Some((b, _, e, w, wo, el, la)) = book_stats(&base, n, years) {
        let rows: usize = base.values().map(Vec::len).sum();
        println!("  growth_score   book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   [baseline, n={rows}]");
    }
    let factors: &[(&str, fn(&core::FundFactors) -> Option<f64>)] = &[
        ("gross_margin", |f| f.gross_margin), // moat / pricing power
        ("op_margin", |f| f.op_margin),       // operating quality
        ("margin_trend", |f| f.margin_trend), // strengthening
        ("rev_cagr", |f| f.rev_cagr),         // top-line compounding
        ("rev_accel", |f| f.rev_accel),
        ("eps_growth", |f| f.eps_growth),          // bottom-line compounding
        ("earnings_yield", |f| f.earnings_yield),  // VALUE (anti-overpay — the near-high gate lacks one)
        ("ebitda_yield", |f| f.ebitda_yield),      // VALUE, capital-structure-neutral (EV folds in leverage — the axis EPS/price misses)
        ("peg_yield", |f| f.peg_yield),            // GROWTH-AT-PRICE: earnings_yield · as-of CAGR (1/PEG). THE SHIPPED tilt since 2026-07-25 — the old "redundant with earnings_yield×CAGR already in the score" note was an assumption, never measured, and it was wrong on every view but this one
        ("buyback_yield", |f| f.buyback_yield),    // capital return
        ("quality(roe/roa)", |f| f.quality),       // quality of capital as SCORED: NetIncome ÷ StockholdersEquity, or ÷ Assets where equity is negative
        ("roic", |f| f.roic),                      // (#43) the same question WITHOUT the leverage: EBIT ÷ (equity + net debt). Head-to-head against the row above is the whole point — if the two grade the same, the leverage adjustment bought nothing
        ("roe_raw", |f| f.roe),                    // the unfiltered ratio — a book built on this one holds the negative-equity fakes
        ("insider_net_buys_90d", |f| f.insider_net_buys_90d), // (Item 4) insider conviction — rows appear only under `backtest … insider`
        // (round 107) SURVIVAL levels (SEC-computed, high = safer) — swept as rank factors here,
        // graded as reject-the-worst gates in the SURVIVAL-GATE probe below.
        ("fcf_margin", |f| f.fcf_margin),          // cash generation: (op cash flow − capex) / revenue
        ("interest_cover", |f| f.interest_cover),  // debt-service headroom: op income / interest expense
        ("net_cash_rev", |f| f.net_cash_rev),      // balance-sheet cushion: (cash − debt) / revenue
        // (round 109) cyclical detector: −std(net_margin) over the lookback — margin LEVEL and 1y
        // TREND are swept above; the dispersion is what a peak-cycle name hides behind a good level.
        ("margin_stability", |f| f.margin_stability),
        // (P2) cash-backing of the reported profit: −(eps − derived fcf/share) / |eps|. Distinct from
        // fcf_margin above, which prices the LEVEL of cash generation — this prices the GAP between the
        // cash and the earnings the market is being shown, which is the axis a managed income statement
        // moves along without moving the level.
        ("accrual_gap", |f| f.accrual_gap),
        // (P3) −CAGR of total assets. The counterweight to rev_cagr/rev_accel above, which reward
        // expansion without ever asking what was spent to buy it. Correlated with rev_accel by
        // construction — a head-to-head against it is the point, not an independent reading.
        ("asset_growth", |f| f.asset_growth),
        // (#405) gross profit ÷ total assets — gross_margin's numerator on the asset base (Novy-Marx).
        ("gp_assets", |f| f.gp_assets),
        // (#407) FCF ÷ total assets (Ball et al.) and −std of YoY revenue growth (Mohanram G5).
        ("cf_assets", |f| f.cf_assets),
        ("rev_stability", |f| f.rev_stability),
        // (#408) FCF ÷ EV, revenue ÷ EV, and the same-date percentile mean of the four value yields.
        ("fcf_ev_yield", |f| f.fcf_ev_yield),
        ("sales_ev_yield", |f| f.sales_ev_yield),
        ("value_composite", |f| f.value_composite),
        // (#409) book ÷ market cap, and the same-date size percentile (high = small).
        ("book_yield", |f| f.book_yield),
        ("small_pct", |f| f.small_pct),
        // (#410) R&D ÷ market cap, and net payout ÷ market cap (sign kept: a net issuer is negative).
        ("rnd_yield", |f| f.rnd_yield),
        ("payout_yield", |f| f.payout_yield),
        // (#411) −(net debt + book) ÷ assets (lean = high), and operating costs ÷ assets.
        ("noa", |f| f.noa),
        ("op_leverage", |f| f.op_leverage),
        ("mscore", |f| f.mscore),
        ("sbc", |f| f.sbc),
        ("goodwill", |f| f.goodwill),
        ("org_cap", |f| f.org_cap),
        ("int_yield", |f| f.int_yield),
        // (#416) the PEG family: peg_yield's scale (~0-500) for the first two, %/yr for the third.
        ("lynch_yield", |f| f.lynch_yield),
        ("sgr_yield", |f| f.sgr_yield),
        ("exp_neg", |f| f.exp_neg),
        ("op_rd", |f| f.op_rd), // (#417)
        ("discipline", |f| f.discipline),
        ("fair_yield", |f| f.fair_yield), // (#523)
        ("oe_fair_yield", |f| f.oe_fair_yield),
        ("fcf_cap_yield", |f| f.fcf_cap_yield),
        ("low_turnover", |f| f.low_turnover), // (#531)
        ("rnd_growth", |f| f.rnd_growth), // (#532)
        ("low_dro", |f| f.low_dro), // (#533)
        ("capex_light", |f| f.capex_light), // (#549)
    ];
    let mut any = false;
    let mut skipped: Vec<String> = Vec::new();
    for (name, get) in factors {
        // (#741) `paired` ranks the factor's OWN rows by growth_score. The baseline row ranks every
        // fund-covered pick, so a factor beating it may only have picked a better subset to rank.
        let mut by: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        let mut paired: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        for s in samples {
            if picks::asset_class(&s.quote) == 0 {
                continue;
            }
            let Some(score) = growth_score(&s.quote, tuning) else { continue };
            let Some(fv) = s.fund.as_ref().and_then(get) else { continue };
            let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
            by.entry(bucket(s.date)).or_default().push((fv, s.realized, br));
            paired.entry(bucket(s.date)).or_default().push((score, s.realized, br));
        }
        let rows: usize = by.values().map(|v| v.len()).sum();
        if let (Some((b, _, e, w, wo, el, la)), Some((_, _, pe, _, _, pel, pla))) = (book_stats(&by, n, years), book_stats(&paired, n, years)) {
            any = true;
            let per = rows as f64 / by.len() as f64;
            println!("  {name:<14} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (n={rows})  same-set score excess {pe:+.1} OOS {pel:+.1}/{pla:+.1}, {per:.0} rows/window");
        } else {
            // (N) SAY SO when a listed factor builds no book. This loop used to print only on Some, so a
            // factor with zero as-of coverage was indistinguishable from one nobody configured — and
            // `roic`, the ONE shipped `growth_fund_extra` term (0.25 × 40 = ten live points), fell into
            // that hole: no row here, no FUNDAMENTAL row, no ablation row, while receipt (#43) cites a
            // three-run ablation this data path does not reproduce. A term that can't be graded must at
            // least be visible. Printed tail only — it cannot move a rank.
            skipped.push(format!("{name} n={rows}"));
        }
    }
    if !any {
        println!("  no fundamental coverage — needs `fund` + fund_source sec (free EDGAR) or an FMP key.");
    } else if !skipped.is_empty() {
        // only when SOME factor did build a book — that is when a missing row is ambiguous. With no
        // coverage at all the line above already says so, and a column of identical "n=0" rows says nothing.
        println!("  no book: {} — listed, NOT scored (a shipped tilt in here is ungradeable, not fine)", skipped.join(", "));
    }
    println!("  (a factor beating its OWN row's same-set score excess, OOS both halves above, ranks better than growth_score;");
    println!("   beating only the baseline row is the factor's subset talking, not its ranking (#741).");
    println!("   every row here is SEC-computed and free — roe, the round-107 survival levels and (#43) roic,");
    println!("   which is DERIVED (EBIT ÷ equity+net debt), not the premium `FundRow::roic` that never populates.)");
    // (#291) RESCUE LANE. Every admission knob that ADDS names is refused, the last one on the sized book
    // too. This asks what no row above can: among the stocks the growth gates REJECT, does a quality
    // ranking find a held book worth buying? Same pool filter and same `book_stats` as the factor rows,
    // gate test inverted, stocks only (the SEC factors are company-level). Three factors named before the
    // run, so the verdict is not an argmax over the factor table. `names/window` is the (#136) coverage-mirage
    // guard: a "top-10" holding two names is not a book.
    if any {
        println!("\n── RESCUE-LANE probe (rank gate-REJECTED fund-covered stocks by a quality factor, top-{n} held {years}y, vs the growth_score baseline above) ──");
        for (name, get) in factors.iter().filter(|(nm, _)| matches!(*nm, "op_margin" | "roic" | "fcf_margin")) {
            let mut by: BTreeMap<i32, Vec<(f64, f64, f64)>> = BTreeMap::new();
            for s in samples {
                if picks::asset_class(&s.quote) != 2 || growth_score(&s.quote, tuning).is_some() {
                    continue;
                }
                let Some(fv) = s.fund.as_ref().and_then(get) else { continue };
                let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
                by.entry(bucket(s.date)).or_default().push((fv, s.realized, br));
            }
            if let Some((b, _, e, w, wo, el, la)) = book_stats(&by, n, years) {
                let held = by.values().map(|v| v.len().min(n)).sum::<usize>() as f64 / by.len() as f64;
                println!("  {name:<14} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}, names/window {held:.1})", by.len());
            }
        }
        println!("  (pre-registered (#291): a row beating the baseline's excess at BOTH 8y and 12y, OOS both +, earns a rescue-lane");
        println!("   round. SEC coverage does not reach a 20y hold, so this can never be graded at the 20y horizon.)");
    }

    // BLEND sweep: pure-value beat pure-score standalone — but pure-value alone risks value-traps the
    // gates miss, so find the growth_fund_weight KNEE where tilting growth_score toward the baked
    // fund_factor (= growth_fund_factor, `peg_yield` in ci-settings) peaks the held book. Only
    // growth_fund_weight varies; quote.fund_factor is fixed at sample-build to the configured factor.
    let mut have_ff = false;
    for s in samples {
        if s.quote.fund_factor.is_some() && s.fund.is_some() {
            have_ff = true;
            break;
        }
    }
    if have_ff {
        println!("\n── held-book vs growth_fund_weight (tilt growth_score toward `{}`, top-{n} held {years}y) ──", tuning.growth_fund_factor);
        for w in [0.0_f64, 0.1, 0.25, 0.5, 1.0, 2.0] {
            let mut t = tuning.clone();
            t.growth_fund_weight = w;
            let mut by: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
            for s in samples {
                if picks::asset_class(&s.quote) == 0 || s.fund.is_none() {
                    continue;
                }
                let Some(score) = growth_score(&s.quote, &t) else { continue };
                let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
                by.entry(bucket(s.date)).or_default().push((score, s.realized, br));
            }
            if let Some((b, _, e, wr, wo, el, la)) = book_stats(&by, n, years) {
                let tag = if w == 0.0 { "  [pure score]" } else { "" };
                println!("  weight {w:<4} book {b:+.1}%/yr  excess {e:+.1}  win {wr:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{tag}");
            }
        }
        println!("  (knee = highest book with OOS both + and worst not deeper than pure-score -> the value weight to ship.)");
    }

    // VALUE-GATE probe: the blend was flat, so the value edge is a BRAKE not a weight — reject the
    // most-expensive (lowest earnings_yield) gated STOCKS per bucket, THEN rank the survivors by the
    // unchanged growth_score. This is the near-high gate's missing valuation ceiling (the Cisco-2000
    // brake). Names with no earnings_yield (ETF/crypto/foreign/no-SEC) can't be judged -> kept. Ship
    // the reject-% ONLY if it lifts the STRESS held book with OOS both + and worst no deeper than off.
    let has_ey = samples.iter().any(|s| s.fund.as_ref().and_then(|f| f.earnings_yield).is_some());
    if has_ey {
        println!("\n── VALUE-GATE probe: drop the most-expensive P% (low earnings_yield) gated STOCKS, rank rest by growth_score, top-{n} held {years}y ──");
        for p in [0.0_f64, 10.0, 25.0, 40.0] {
            if let Some((b, _, e, w, wo, el, la)) = drop_bottom_book(samples, bd, bc, years, tuning, n, p, |f| f.earnings_yield) {
                let tag = if p == 0.0 { "  [gate off]" } else { "" };
                println!("  reject-bottom {p:>4.0}%  book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{tag}");
            }
        }
        println!("  (a reject-% that lifts book with OOS both + and worst no deeper than off -> ship as a real growth gate.)");
    }

    // (EV/EBITDA probe) the SAME brake on the capital-structure-neutral multiple — reject the most-expensive
    // (lowest ebitda_yield) gated STOCKS, then rank survivors by growth_score. Distinct from the earnings_yield
    // gate above because EV folds in leverage (a debt-heavy name can look cheap on P/E yet dear on EV/EBITDA).
    // Ship on the same golden bar: STRESS book/worst lifted, OOS both +. Prior: dead like the other 14 factors.
    let has_eby = samples.iter().any(|s| s.fund.as_ref().and_then(|f| f.ebitda_yield).is_some());
    if has_eby {
        println!("\n── EV-VALUE-GATE probe: drop the most-expensive P% (low ebitda_yield) gated STOCKS, rank rest by growth_score, top-{n} held {years}y ──");
        for p in [0.0_f64, 10.0, 25.0, 40.0] {
            if let Some((b, _, e, w, wo, el, la)) = drop_bottom_book(samples, bd, bc, years, tuning, n, p, |f| f.ebitda_yield) {
                let tag = if p == 0.0 { "  [gate off]" } else { "" };
                println!("  reject-bottom {p:>4.0}%  book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{tag}");
            }
        }
        println!("  (a reject-% that lifts book with OOS both + and worst no deeper than off -> ship as a real growth gate.)");
    }

    // (PEG probe) the brake on growth-at-price — reject the most-expensive-FOR-ITS-GROWTH P% (lowest
    // peg_yield = earnings_yield·CAGR) gated STOCKS, rank survivors by growth_score. Distinct from the two
    // gates above in intent (a fast grower can be dear on P/E yet cheap on PEG, and vice-versa).
    // ONCE "MEASURED DEAD 2026-07-25" — flat +6.8 / +6.9 / +6.9 / +6.9 at reject 0/10/25/40% on a
    // same-batch 12y (4913 tickers, 8393 fund-covered cutoffs), book +14.8%/yr and worst -7.8 unmoved,
    // read at the time as "the growth-at-price axis is closed AS A BRAKE".
    // (#75) THAT NOTE EXPIRED. Re-read 2026-08-12 the sweep is NOT flat and it MEETS the bar printed
    // below: 12y +6.1 / +6.2 / +6.2 / +6.4 (book +13.9->+14.2, OOS late +7.0->+7.5), 8y +6.3 / +6.2 /
    // +6.3 / +6.5, worst -7.8 / -9.2 unmoved at every rung. A death certificate perishes exactly like
    // the TOO TIGHT flags in the (#74) receipt do — same lesson, opposite direction — so re-read one
    // before citing it, and never delete the probe that would have told you.
    // AND IT STILL DOES NOT SHIP, which is the part worth carrying: the axis was armed as a real knob
    // (`growth_value_floor_pct`, a cross-sectional percentile floor at this same boundary) and graded
    // over 12 runs. The lift here is a TOP-10 held-book effect and decayed to +0.1 at 12y top-3 and to
    // 0.0 at 8y top-3, so it failed Ship Rule v2's ADDITION bar. THE BAR PRINTED BELOW IS STILL NOT A
    // SHIP RULE — it grades a HELD-N-YEARS NO-SELL book, while the verdict grades a REBALANCED one, and
    // those are different questions. Treat it as "worth a grid", never as "ship this". Full grid in the
    // (#75) receipt at `growth_value_floor_pct` in tests/ci-settings.yaml.
    //
    // (#120) READ THAT REFUSAL AGAIN BEFORE REUSING IT. Half of why (#75) was refused was that its lift
    // lived at top-10 while the verdict graded top-3 — and the verdict now grades top-10. The
    // no-sell-vs-rebalanced gap survives the basket move, so the bar below is still not the rule, but
    // the DECAY ARGUMENT does not survive: it was measured against a basket this repo no longer ships.
    // `growth_value_floor_pct` is therefore RE-GRADEABLE, not re-graded — nobody has run that grid
    // against a top-10 verdict. It is not re-opened here, and it stays at its shipped 0.0 until it is.
    //
    // (#126) THAT GRID WAS RUN 2026-08-23 AND THE KNOB SHIPS AT 40.0. Twelve runs on the PIT pool,
    // `backtest {12,8} universe fund pit` x {0,25,40,55,70} plus a 20y {0,70} pair: top-10 excess rises
    // on BOTH moments at BOTH graded horizons (12y +2.4|+2.5 -> +2.8|+2.8, 8y +3.7|+3.4 -> +4.5|+4.5),
    // worst window IDENTICAL at every arm, h2h >=50% throughout. The two paragraphs above are kept as
    // written — they record why the re-grade was owed — but the "stays at its shipped 0.0" clause is
    // SUPERSEDED. The last sentence below is superseded too: cutting the dear names DOES now pay at the
    // top of the book, which is exactly what (#120)'s basket move made visible. Full receipt at
    // `growth_value_floor_pct` in tests/ci-settings.yaml, caveats included — the evidence is thin
    // (n_eff 1.4 at 12y, 2.6 at 8y) and the receipt says so.
    // Note the same factor is ALSO alive as a RANK TILT in the same run (peg_yield is the shipped
    // `growth_fund_factor`) — cutting the dear names and ranking by cheapness are different questions,
    // and as of (#126) both of them pay.
    let has_peg = samples.iter().any(|s| s.fund.as_ref().and_then(|f| f.peg_yield).is_some());
    if has_peg {
        println!("\n── PEG-VALUE-GATE probe: drop the most-expensive-for-growth P% (low peg_yield) gated STOCKS, rank rest by growth_score, top-{n} held {years}y ──");
        for p in [0.0_f64, 10.0, 25.0, 40.0] {
            if let Some((b, _, e, w, wo, el, la)) = drop_bottom_book(samples, bd, bc, years, tuning, n, p, |f| f.peg_yield) {
                let tag = if p == 0.0 { "  [gate off]" } else { "" };
                println!("  reject-bottom {p:>4.0}%  book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{tag}");
            }
        }
        println!("  (a reject-% that lifts book with OOS both + and worst no deeper than off -> ship as a real growth gate.)");
    }

    // (round 107) SURVIVAL-GATE probe: the same brake pointed at DEATH instead of price. A never-sell
    // book can't exit a bankruptcy — one −100% in an equal-weight top-10 costs ~10 pts of terminal
    // wealth — so cutting a future zero is worth more than another point of selection edge. Reject the
    // per-bucket weakest P% by each survival level; names the factor can't judge are kept (None =
    // neutral). Ship exactly like the value gate: STRESS book/worst lifted, OOS both +.
    let survival: &[(&str, fn(&core::FundFactors) -> Option<f64>)] = &[
        ("fcf_margin", |f| f.fcf_margin),
        ("interest_cover", |f| f.interest_cover),
        ("net_cash_rev", |f| f.net_cash_rev),
        ("margin_stability", |f| f.margin_stability), // (round 109) reject the most-cyclical margins
        ("capex_light", |f| f.capex_light), // (#549) reject the most capex-heavy: (#545)'s penalty, point-in-time
    ];
    let mut shown = false;
    for (name, get) in survival {
        if !samples.iter().any(|s| s.fund.as_ref().and_then(get).is_some()) {
            continue; // factor never populated on this feed -> no fabricated rows
        }
        if !shown {
            println!("\n── SURVIVAL-GATE probe: drop the weakest P% per survival factor, rank rest by growth_score, top-{n} held {years}y ──");
            shown = true;
        }
        for p in [0.0_f64, 10.0, 25.0] {
            if let Some((b, _, e, w, wo, el, la)) = drop_bottom_book(samples, bd, bc, years, tuning, n, p, *get) {
                let tag = if p == 0.0 { "  [gate off]" } else { "" };
                println!("  {name:<14} reject {p:>3.0}%  book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{tag}");
            }
        }
    }
    if shown {
        println!("  (gate-off rows repeat the baseline; a reject-% lifting book or worst with OOS both + -> ship as a survival gate.)");
    }
}

/// Shared gate-probe engine: per ~6mo bucket, drop the bottom-P% of the gated, non-crypto,
/// benchmarkable stocks by `get` (names the factor can't judge are KEPT — a gate can only act on
/// evidence), rank the survivors by the unchanged growth_score, and grade the top-`n` held book.
/// `p == 0` is the gate-off baseline. Extracted from the value-gate probe so the round-107 survival
/// gates grade on byte-identical math.
fn drop_bottom_book(
    samples: &[Sample],
    bd: &[chrono::NaiveDate],
    bc: &[f64],
    years: i64,
    tuning: &BuyHeuristic,
    n: usize,
    p: f64,
    get: fn(&core::FundFactors) -> Option<f64>,
) -> Option<(f64, f64, f64, f64, f64, f64, f64)> {
    let mut buckets: std::collections::BTreeMap<i32, Vec<&Sample>> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 || growth_score(&s.quote, tuning).is_none() || benchmark_fwd(bd, bc, s.date, years).is_none() {
            continue;
        }
        buckets.entry(bucket(s.date)).or_default().push(s);
    }
    let mut by: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    for (bk, ss) in &buckets {
        let thr = if p > 0.0 {
            let mut vals: Vec<f64> = ss.iter().filter_map(|s| s.fund.as_ref().and_then(get)).collect();
            if vals.is_empty() {
                None
            } else {
                vals.sort_by(|a, b| a.total_cmp(b));
                Some(vals[(((p / 100.0) * vals.len() as f64) as usize).min(vals.len() - 1)]) // P-th percentile floor
            }
        } else {
            None
        };
        for s in ss {
            if let (Some(t), Some(v)) = (thr, s.fund.as_ref().and_then(get)) {
                if v < t {
                    continue; // below the floor -> rejected by the gate
                }
            }
            let score = growth_score(&s.quote, tuning).unwrap();
            let br = benchmark_fwd(bd, bc, s.date, years).unwrap();
            by.entry(*bk).or_default().push((score, s.realized, br));
        }
    }
    book_stats(&by, n, years)
}

// (#41) `corr_tail` lives in `core` and is now called by BOTH this probe (via core::decorrelate_keep)
// and the live growth_corr_cap skip. It was defined here alone while the live table had no correlation
// concept at all; shipping the skip without sharing the definition is how the two drift — the (#3j)
// lesson applied before the drift rather than after.

/// (round 112) CORR-CAP book — the DIVERSIFICATION axis: per bucket, walk the gated non-crypto
/// names in growth_score order and keep one only if its trailing-return correlation with every
/// already-kept name stays under `cap`; the first `n` kept are the book. Unjudgeable pairs (empty
/// trail, <12mo overlap) are KEPT — a brake can only act on evidence, like every gate. `cap` > 1.0
/// keeps everything -> reproduces the plain top-`n` book (at exactly 1.0 a PERFECT twin still drops).
fn corr_cap_book(
    samples: &[Sample],
    bd: &[chrono::NaiveDate],
    bc: &[f64],
    years: i64,
    tuning: &BuyHeuristic,
    n: usize,
    cap: f64,
) -> Option<(f64, f64, f64, f64, f64, f64, f64)> {
    let mut buckets: std::collections::BTreeMap<i32, Vec<(f64, &Sample)>> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 || benchmark_fwd(bd, bc, s.date, years).is_none() {
            continue;
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        buckets.entry(bucket(s.date)).or_default().push((score, s));
    }
    let mut by: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
    for (bk, ranked) in &mut buckets {
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (score, s) in greedy_decorrelate(ranked, n, cap) {
            let br = benchmark_fwd(bd, bc, s.date, years).unwrap();
            by.entry(*bk).or_default().push((score, s.realized, br));
        }
    }
    book_stats(&by, n, years)
}

/// (round 112) The greedy walk itself, pure for testing: keep a ranked name only if no already-kept
/// name correlates >= `cap` with it; unjudgeable pairs (None) never block. cap = INFINITY keeps the
/// plain top-`n` — the probe's identity row.
fn greedy_decorrelate<'a>(ranked: &[(f64, &'a Sample)], n: usize, cap: f64) -> Vec<(f64, &'a Sample)> {
    // (#41) the walk itself now lives in `core::decorrelate_keep`, shared with the live growth_corr_cap
    // skip so the probe and the table can never disagree about what "correlated" means. This wrapper is
    // just the Sample <-> index adaptor.
    let trails: Vec<&[f64]> = ranked.iter().map(|(_, s)| s.trail.as_slice()).collect();
    core::decorrelate_keep(&trails, n, cap).into_iter().map(|i| ranked[i]).collect()
}

/// (round 112) CORR-CAP probe: is the top-10 ten copies of one bet? The same brake family as the
/// gates, pointed at REDUNDANCY: cap the pairwise trailing-return correlation inside the book and
/// refill from the ranked list. Ship exactly like a gate: book/worst lifted, OOS both +, STRESS agrees.
fn report_corr_cap(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    if bd.len() < 2 || !samples.iter().any(|s| s.trail.len() >= 12) {
        return; // no trails (stub/short-history run) -> no fabricated rows
    }
    // (#120) THE SAME basket the verdict ships on. These rows compare candidates against each other,
    // so they want the wider, better-estimated book — and that argument, which was already written
    // here, is exactly the one that moved `VERDICT_TOP` from 3 to 10. Reading the const rather than
    // repeating the literal is what stops the two drifting apart again.
    let n = VERDICT_TOP;
    println!("\n── CORR-CAP probe: greedy top-{n} by growth_score, skip names correlating >= cap with the kept book (36mo trailing), held {years}y ──");
    for cap in [f64::INFINITY, 0.8, 0.6, 0.4] {
        if let Some((b, _, e, w, wo, el, la)) = corr_cap_book(samples, bd, bc, years, tuning, n, cap) {
            let label = if cap.is_finite() { format!("{cap:.1}") } else { "off".to_string() };
            let tag = if cap.is_finite() { "" } else { "  [cap off = plain top-10 book]" };
            println!("  cap {label:>4}  book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}{tag}");
        }
    }
    println!("  (a cap lifting book or worst with OOS both + -> ship as a book-construction rule; expectation low — every structure tweak since round 14 diluted.)");
}

/// (round 106) One bucket's TWO-STYLE union book: top-`g` by growth score plus top-`v` by
/// earnings_yield (rows without ey can't be value-picked), deduped by ticker — a name both styles
/// pick takes ONE equal-weight slot, so the book shrinks by the overlap instead of double-weighting.
/// Rows: (ticker, score, earnings_yield, realized %, bench %). Returns
/// (mean pick terminal multiple, mean bench terminal multiple, book size, overlap count).
fn union_book(rows: &[(String, f64, Option<f64>, f64, f64)], g: usize, v: usize) -> Option<(f64, f64, usize, usize)> {
    let mut by_score: Vec<&(String, f64, Option<f64>, f64, f64)> = rows.iter().collect();
    by_score.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut picked: Vec<&(String, f64, Option<f64>, f64, f64)> = by_score.into_iter().take(g).collect();
    let mut by_ey: Vec<&(String, f64, Option<f64>, f64, f64)> = rows.iter().filter(|r| r.2.is_some()).collect();
    by_ey.sort_by(|a, b| b.2.unwrap().total_cmp(&a.2.unwrap()));
    let mut overlap = 0;
    for r in by_ey.into_iter().take(v) {
        if picked.iter().any(|p| p.0 == r.0) {
            overlap += 1;
        } else {
            picked.push(r);
        }
    }
    if picked.is_empty() {
        return None;
    }
    let n = picked.len() as f64;
    let book = picked.iter().map(|r| 1.0 + r.3 / 100.0).sum::<f64>() / n;
    let spy = picked.iter().map(|r| 1.0 + r.4 / 100.0).sum::<f64>() / n;
    Some((book, spy, picked.len(), overlap))
}

/// (#1067) Means of four chronological quarters of `x` that together cover every element: quarter q is
/// `len*q/4 .. len*(q+1)/4`, the split `report_lane`'s era table uses. The `chunks(len / 4).take(4)`
/// this replaces dropped the newest `len % 4` values, so nine windows printed a last quarter of two.
fn quarter_means(x: &[f64]) -> Vec<f64> {
    (0..4)
        .map(|q| &x[x.len() * q / 4..x.len() * (q + 1) / 4])
        .map(|s| s.iter().sum::<f64>() / s.len().max(1) as f64)
        .collect()
}

/// (round 106) TWO-STYLE HELD BOOK — the no-borrow structural lever. The growth_score book and the
/// pure earnings_yield book pick mostly DIFFERENT names (the round-45 receipt: grafting value INTO
/// the score was flat precisely because the value edge lives in a different book, not in reordering
/// this one). So measure the portfolio-level structure instead: hold BOTH books side by side,
/// equal-weight union, never-sell. Report-only: the outcome can ship as portfolio GUIDANCE (like
/// "hold 10-15 equal-weight"), NEVER as a score/gate/knob — the ranking stays untouched in every branch.
fn report_two_style_book(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    if bd.len() < 2 {
        return;
    }
    // the same fair pool report_book_by_factor grades on (gated, non-crypto, fund-covered,
    // benchmarkable) so the rows here are head-to-head comparable with the factor table above.
    let mut buckets: std::collections::BTreeMap<i32, Vec<(String, f64, Option<f64>, f64, f64)>> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 || s.fund.is_none() {
            continue;
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        let ey = s.fund.as_ref().and_then(|f| f.earnings_yield);
        buckets.entry(bucket(s.date)).or_default().push((s.quote.ticker.clone(), score, ey, s.realized, br));
    }
    if buckets.is_empty() || !buckets.values().flatten().any(|r| r.2.is_some()) {
        return; // no earnings_yield coverage -> nothing to combine
    }
    println!("\n── TWO-STYLE HELD BOOK (growth_score book + pure-earnings_yield book, equal-weight UNION, held {years}y no-sell) ──");
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let variants: &[(&str, usize, usize)] = &[
        ("growth top-10  [baseline]", 10, 0),
        ("value  top-10", 0, 10),
        ("combo  5g+5v", 5, 5),
        ("combo  10g+10v", 10, 10),
    ];
    // per-variant per-bucket (book CAGR, excess) — growth/value legs feed the corr + era receipts
    let mut by_variant: Vec<std::collections::BTreeMap<i32, (f64, f64)>> = vec![Default::default(); variants.len()];
    for (vi, (label, g, v)) in variants.iter().enumerate() {
        let (mut books, mut excess, mut sizes, mut overlaps) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
        for (bk, rows) in &buckets {
            let Some((bcum, scum, size, ov)) = union_book(rows, *g, *v) else { continue };
            let (b, s) = (ann((bcum - 1.0) * 100.0, years), ann((scum - 1.0) * 100.0, years));
            books.push(b);
            excess.push(b - s);
            sizes.push(size as f64);
            overlaps += ov;
            by_variant[vi].insert(*bk, (b, b - s));
        }
        if excess.is_empty() {
            continue;
        }
        let m = excess.len();
        let win = excess.iter().filter(|e| **e > 0.0).count() as f64 / m as f64 * 100.0;
        let worst = excess.iter().cloned().fold(f64::INFINITY, f64::min);
        let cut = m / 2;
        // union size < g+v = the styles overlapped; print how much so a "combo win" that is really
        // "the same book again" is visible at a glance.
        let ov_note = if *g > 0 && *v > 0 {
            format!("  (mean size {:.1}, overlap {:.1}/window)", mean(&sizes), overlaps as f64 / m as f64)
        } else {
            String::new()
        };
        println!(
            "  {label:<26} book {:+.1}%/yr  excess {:+.1}  win {win:.0}%  worst {worst:+.1}  OOS {:+.1}/{:+.1}{ov_note}",
            mean(&books),
            mean(&excess),
            mean(&excess[..cut]),
            mean(&excess[cut..])
        );
    }
    // WHY a combo can beat both parents: window-level correlation of the two pure books over the
    // buckets both cover — low = real diversification; ~+1.0 = the same bet twice, combo can't help.
    let shared: Vec<(f64, f64)> = by_variant[0]
        .iter()
        .filter_map(|(bk, gv)| by_variant[1].get(bk).map(|vv| (gv.0, vv.0)))
        .collect();
    if shared.len() >= 4 {
        let (gs, vs): (Vec<f64>, Vec<f64>) = shared.iter().cloned().unzip();
        if let Some(rho) = core::spearman(&gs, &vs) {
            println!("  window corr (growth vs value book, spearman) {rho:+.2} over {} shared windows", shared.len());
        }
    }
    // value-leg hardening (the round-44 caveats made measurable): chronological era slices of the pure
    // value book's excess, plus how thin its per-bucket pick pool runs (a 3-name "book" is not a book).
    let vexcess: Vec<f64> = by_variant[1].values().map(|(_, e)| *e).collect();
    if vexcess.len() >= 8 {
        let parts: Vec<String> = quarter_means(&vexcess)
            .iter()
            .map(|m| format!("{m:+.1}"))
            .collect();
        let ns: Vec<f64> = buckets.values().map(|rows| rows.iter().filter(|r| r.2.is_some()).count() as f64).collect();
        let nmin = ns.iter().cloned().fold(f64::INFINITY, f64::min);
        println!(
            "  value-book era slices (chronological quarters, excess %/yr) {}   ey rows/window min {nmin:.0} mean {:.1}",
            parts.join(" / "),
            mean(&ns)
        );
    }
    println!("  (ship rule: the combo becomes PORTFOLIO GUIDANCE — buy both books, never-sell — ONLY if the STRESS");
    println!("   combo book ≥ the growth book with OOS both + and worst no deeper. Never a score/gate/knob change.)");
}

/// Top/bottom scored-half mean peer-relative return. `pairs` = (sample, score); sorted by score desc,
/// then (top-half mean relative, bottom-half mean relative). The edge is the difference. Shared by
/// `report_lane`, the ablation, and `tune`.
fn edge_halves(pairs: &[(&Sample, f64)]) -> (f64, f64) {
    let mut v: Vec<&(&Sample, f64)> = pairs.iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    let half = v.len() / 2;
    let mean = |s: &[&(&Sample, f64)]| s.iter().map(|x| x.0.relative).sum::<f64>() / s.len().max(1) as f64;
    (mean(&v[..half]), mean(&v[v.len() - half..]))
}

/// (Item 12) top-minus-bottom edge AFTER winsorizing peer-relative returns at the 1st/99th percentile
/// (clamp the tails, keep n). `edge_halves` is a MEAN, so one 10× crypto or a fraud blow-up in a half can
/// BE the whole edge. A big gap vs the raw edge = the lane leans on a few extreme rows (fragile, likely a
/// survivorship/fat-tail artifact). Pure; reuses `percentile`. <4 rows -> NaN (no spread to read).
fn winsor_edge(pairs: &[(&Sample, f64)]) -> f64 {
    if pairs.len() < 4 {
        return f64::NAN;
    }
    let mut rels: Vec<f64> = pairs.iter().map(|x| x.0.relative).collect();
    rels.sort_by(|a, b| a.total_cmp(b));
    let (lo, hi) = (percentile(&rels, 1.0), percentile(&rels, 99.0));
    // (score, clamped-relative), sorted by score desc -> same half split as edge_halves.
    let mut v: Vec<(f64, f64)> = pairs.iter().map(|x| (x.1, x.0.relative.clamp(lo, hi))).collect();
    v.sort_by(|a, b| b.0.total_cmp(&a.0));
    let half = v.len() / 2;
    let mean = |s: &[(f64, f64)]| s.iter().map(|x| x.1).sum::<f64>() / s.len().max(1) as f64;
    mean(&v[..half]) - mean(&v[v.len() - half..])
}

/// Tercile means of peer-relative return, sorted by score desc: (top, mid, bottom). A config can post a
/// strong top-vs-bottom `edge` while SCRAMBLING the middle (mid below bottom = the score only separates
/// the extremes, fragile/regime-bound). Monotone terciles (top > mid > bottom) is the cheap robustness
/// check the 2-bucket edge is blind to. note: terciles, not deciles — the gated growth sample is too
/// small for 10 stable buckets. <3 rows -> all-NaN (no spread to read).
fn edge_terciles(pairs: &[(&Sample, f64)]) -> (f64, f64, f64) {
    let mut v: Vec<&(&Sample, f64)> = pairs.iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    let n = v.len();
    if n < 3 {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let k = n / 3; // outer terciles size k; the middle keeps the remainder
    let mean = |s: &[&(&Sample, f64)]| s.iter().map(|x| x.0.relative).sum::<f64>() / s.len().max(1) as f64;
    (mean(&v[..k]), mean(&v[k..n - k]), mean(&v[n - k..]))
}

/// Tercile string for the tune report: `+top/+mid/+bot monotone|SCRAMBLED`. Scores `samples` through the
/// growth lane under `t` (same gate set as lane_metrics) and reads its tercile gradient — a SCRAMBLED tag
/// warns the human that a winning 2-bucket edge rests on extreme rows only, not a real ranking.
fn mono_str(samples: &[Sample], t: &BuyHeuristic) -> String {
    let scored: Vec<(&Sample, f64)> =
        samples.iter().filter_map(|s| growth_score(&s.quote, t).map(|v| (s, v))).collect();
    let (top, mid, bot) = edge_terciles(&scored);
    let tag = if top > mid && mid > bot { "monotone" } else { "SCRAMBLED" };
    format!("{top:+.1}/{mid:+.1}/{bot:+.1} {tag}")
}

/// Score `samples` through `scorer`/`tuning` (gates applied) -> (rho, edge) on the gated rows: rho =
/// Spearman(score, peer-relative); edge = top-half minus bottom-half peer-relative. rho is None when
/// fewer than 4 rows pass the gates. Pure -> the `tune` search calls it thousands of times cheaply.
fn lane_metrics(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> (Option<f64>, f64) {
    let scored: Vec<(&Sample, f64)> =
        samples.iter().filter_map(|s| scorer(&s.quote, tuning).map(|v| (s, v))).collect();
    scored_metrics(&scored)
}

/// (#595) [`lane_metrics`] past the scoring: (rho, edge) over the rows that scored, in sample order.
/// The fund sweep scores most of its rows once per factor, and ranks them here exactly as
/// `lane_metrics` would.
fn scored_metrics(scored: &[(&Sample, f64)]) -> (Option<f64>, f64) {
    if scored.len() < 4 {
        return (None, 0.0);
    }
    let sc: Vec<f64> = scored.iter().map(|(_, v)| *v).collect();
    let rels: Vec<f64> = scored.iter().map(|(s, _)| s.relative).collect();
    let (t, b) = edge_halves(scored);
    (core::spearman(&sc, &rels), t - b)
}

/// Does perturbing ONLY this weight (to `probe`) change any gated score? Drives `tune`'s inert-dim skip:
/// a weight the sample can't move (e.g. growth_fund_weight when every fund_factor is None -> the term is
/// always 0) is dropped from the search so it can't get a meaningless searched value. Generic over the
/// scorer so it's unit-testable without building quotes that clear growth_score's gates.
fn dim_active(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    default: &BuyHeuristic,
    set: fn(&mut BuyHeuristic, f64),
    probe: f64,
) -> bool {
    let score = |t: &BuyHeuristic| -> Vec<f64> {
        samples.iter().filter_map(|s| scorer(&s.quote, t)).collect()
    };
    let mut t = default.clone();
    set(&mut t, probe);
    score(&t) != score(default)
}

/// (honest OOS) The shipped growth knobs were hand-tuned on ALL the data, so their backtest edge is
/// optimistic (the footer caveat says as much). This splits the cutoffs chronologically, SEARCHES the
/// growth weights on the EARLY train half only, then scores the winner on the LATE test half it never
/// saw — turning hand-tuning into out-of-sample selection. Seeded xorshift64 (no `rand` dep) so a re-run
/// reproduces. Writes NOTHING: a winning config is printed for the human to paste into settings.yaml.
fn tune_growth(samples: &[Sample], default: &BuyHeuristic) {
    // chronological split (samples are date-sorted in `run`); de-mean WITHIN each split so neither leaks
    // the other's bucket means (the peer-relative invariant must hold per split, not just globally).
    let cut = samples.len() * 7 / 10;
    // (#90) TRAIN stops short of the boundary by the purge span; TEST keeps every row from `cut` on,
    // because the held-out side is the evidence and trimming it would only shrink the sample. The two
    // `demean` calls still run over the full halves: the purged rows sit in the PAST of every test row,
    // so their peer means leak nothing, and leaving them in keeps the default byte-identical.
    let train_end = purged_cut(samples, cut, default.split_purge_months, |s| s.date);
    let mut s = samples.to_vec();
    demean(&mut s[..cut]);
    demean(&mut s[cut..]);
    let (train, test) = (&s[..train_end], &s[cut..]);
    if train.len() < 8 || test.len() < 8 {
        println!("\ntune: too few cutoffs to split ({} train / {} test) — run `universe` for a bigger sample.", train.len(), test.len());
        return;
    }

    // the searched growth weights: (label, getter, setter, lo, hi). Bands bracket each shipped default.
    // growth_fund_weight is included so A1's factor is searched too (0 in its band == off, so the search
    // is free to reject it). Setters mirror the ablation knobs but assign rather than zero.
    type Get = fn(&BuyHeuristic) -> f64;
    type Set = fn(&mut BuyHeuristic, f64);
    let dims: &[(&str, Get, Set, f64, f64)] = &[
        ("growth_trend_weight", |t| t.growth_trend_weight, |t, v| t.growth_trend_weight = v, 0.0, 1.0),
        // band starts at 15, NOT 0: on this knob 0 means OFF (uncapped = +inf), so 0 is a discontinuity a
        // uniform draw would misread — 0.1 crushes every name, 0.0 crushes none. The shipped state is 30 (#3k),
        // so `dim_active` sees hi=60 change nothing when no sample tops 30%/yr and reports the dim inert; a
        // search winner that does carry a cap prints under "weights (searched dims only)" and still has to
        // beat TEST. The off-vs-on comparison belongs to `weight_curve`, which ladders 0 explicitly.
        ("long_trend_cap", |t| t.long_trend_cap, |t, v| t.long_trend_cap = v, 15.0, 60.0),
        // band widened 0.6 -> 1.0 (#47): the SHIPPED value is 0.65, so the old band could not draw it and
        // every proposal the search made was structurally weaker than what ships. Bands must bracket the
        // default, as every other dim here does.
        ("growth_accel_weight", |t| t.growth_accel_weight, |t, v| t.growth_accel_weight = v, 0.0, 1.0),
        // (#47) the accel term's CEILING, never searched before. It is half of that term's authority —
        // a name 60 pts above its long run scores identically to one 50 pts above — so leaving it fixed
        // while searching the weight only ever explored one of the two knobs that set the same thing.
        ("growth_accel_cap", |t| t.growth_accel_cap, |t, v| t.growth_accel_cap = v, 20.0, 80.0),
        ("sharpe_weight", |t| t.sharpe_weight, |t, v| t.sharpe_weight = v, 0.0, 4.0),
        ("calmar_weight", |t| t.calmar_weight, |t, v| t.calmar_weight = v, 0.0, 3.0),
        ("growth_overext_floor", |t| t.growth_overext_floor, |t, v| t.growth_overext_floor = v, 0.01, 0.5),
        ("growth_fund_weight", |t| t.growth_fund_weight, |t, v| t.growth_fund_weight = v, 0.0, 0.5),
        ("growth_mom121_weight", |t| t.growth_mom121_weight, |t, v| t.growth_mom121_weight = v, 0.0, 0.5),
    ];

    // drop INERT dims (see dim_active): a weight the sample can't move (e.g. growth_fund_weight on the
    // universe, where every fund_factor is None) is skipped so it can't get a meaningless searched value.
    let (active, inert): (Vec<_>, Vec<_>) =
        dims.iter().partition(|&&(_, _, set, _, hi)| dim_active(train, growth_score, default, set, hi));

    // seeded xorshift64 -> uniform f64 in [0,1). Deterministic: same seed, same search, every run.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };

    // search on TRAIN: keep the draw with the best train edge among those that also rank right (rho>0) —
    // the same "positive selection" bar the project keeps a knob by, but chosen WITHOUT seeing the test.
    let draws = 500;
    // Drawn serially, scored in parallel, folded serially. The split matters: `next()` is a single
    // xorshift stream and `best` keeps the FIRST config to strictly beat the running max, so both the
    // draw sequence and every tie-break have to stay in draw order. Only `lane_metrics` moves — it is
    // a pure function of `(train, t)` with no shared state, and it is where the time goes (~3.9s of a
    // 6.75s `backtest 12 tune`). 500 `BuyHeuristic` clones held at once is a few hundred KB.
    let proposals: Vec<BuyHeuristic> = (0..draws)
        .map(|_| {
            let mut t = default.clone();
            for &(_, _, set, lo, hi) in &active {
                set(&mut t, lo + next() * (hi - lo));
            }
            t
        })
        .collect();
    let scored: Vec<(Option<f64>, f64)> =
        proposals.par_iter().map(|t| lane_metrics(train, growth_score, t)).collect();
    let mut best: Option<(f64, BuyHeuristic)> = None; // (train edge, config)
    for (t, (rho, edge)) in proposals.into_iter().zip(scored) {
        if rho.unwrap_or(0.0) > 0.0 && best.as_ref().is_none_or(|(e, _)| edge > *e) {
            best = Some((edge, t));
        }
    }

    let (def_rho, def_edge) = lane_metrics(test, growth_score, default);
    let fmt = |r: Option<f64>| r.map_or("n/a".to_string(), |v| format!("{v:+.2}"));
    println!(
        "\n══ TUNE — growth lane, honest out-of-sample ({} train / {} test cutoffs, {draws} draws) ══",
        train.len(),
        test.len()
    );
    println!("  shipped default      TEST: rho {}  edge {:+.1}  terciles {}", fmt(def_rho), def_edge, mono_str(test, default));
    if !inert.is_empty() {
        let names: Vec<&str> = inert.iter().map(|(n, ..)| *n).collect();
        println!("  inert on this sample (skipped, kept at default): {}", names.join(", "));
    }
    if active.is_empty() {
        println!("  every searched weight is inert on this sample — nothing to tune. SHIP NOTHING.");
        return;
    }

    let Some((train_edge, won)) = best else {
        println!("  search found NO train config with positive rho — keep the shipped default.");
        return;
    };
    let (won_train_rho, _) = lane_metrics(train, growth_score, &won);
    let (won_test_rho, won_test_edge) = lane_metrics(test, growth_score, &won);
    println!("  search winner       TRAIN: rho {}  edge {:+.1}", fmt(won_train_rho), train_edge);
    println!("  search winner        TEST: rho {}  edge {:+.1}  terciles {}   (train ≫ test ⇒ overfit; SCRAMBLED terciles ⇒ edge rests on extremes only)", fmt(won_test_rho), won_test_edge, mono_str(test, &won));
    println!("  weights (searched dims only):");
    for &(name, get, _, _, _) in &active {
        println!("    {name:<22} {:.3}", get(&won));
    }
    // NaN guard: a degenerate sample (e.g. the monthly path admits too few gated growth names per
    // bucket to form a top/bottom-half spread) yields a NaN edge, and `NaN <= NaN` is false -> the
    // old check fell through and wrongly printed "BEATS ... paste into settings.yaml". No finite edge
    // to compare = no edge proven = keep the default.
    if !won_test_edge.is_finite() || !def_edge.is_finite() {
        println!("  -> TEST edge is undefined ({won_test_edge:+.1} vs {def_edge:+.1}) — too few gated names to form a spread on this sample. SHIP NOTHING.");
        return;
    }
    if won_test_edge <= def_edge {
        println!("  -> winner does NOT beat the default on the held-out TEST edge ({won_test_edge:+.1} vs {def_edge:+.1}); the shipped knobs already generalise. SHIP NOTHING.");
        return;
    }
    println!("  -> winner BEATS the default on TEST ({won_test_edge:+.1} vs {def_edge:+.1}). Paste the weights into settings.yaml, then re-run plain `backtest universe` to confirm.");

    // parsimony: force each weight to 0 on the winner, re-score TEST. A weight whose removal doesn't drop
    // (or even lifts) the TEST edge isn't earning its place out-of-sample -> a deletion candidate.
    println!("  parsimony (winner, each weight -> 0, TEST edge Δ vs {won_test_edge:+.1}):");
    for &(name, _, set, _, _) in &active {
        let mut t = won.clone();
        set(&mut t, 0.0);
        let (_, e) = lane_metrics(test, growth_score, &t);
        println!("    {name:<22} edge {e:+.1} Δ{:+.1}", e - won_test_edge);
    }
}

/// One ablation knob: a name + a fn that zeroes its weight in a `BuyHeuristic` copy.
/// BOXED, not a plain `fn` pointer: `growth_fund_extra` is a config-driven LIST, so its ablation rows
/// have to be generated per configured term and each needs to capture its own index — which a fn
/// pointer cannot do. Owned `String` for the same reason (the names carry the factor).
/// `+ Sync` because `report_lane` ablates the arms in parallel. Every knob below is a closure over
/// nothing, or over a `String`, so this costs nothing to satisfy — but a future knob that captured a
/// `Cell` or an `Rc` would stop compiling here rather than race.
type Knob = (String, Box<dyn Fn(&mut BuyHeuristic) + Sync>);

/// Terse constructor so the knob tables below stay one line per knob.
fn knob(name: impl Into<String>, f: impl Fn(&mut BuyHeuristic) + Sync + 'static) -> Knob {
    (name.into(), Box::new(f))
}

/// (#119) How many whole blocks the record must hold before a block bootstrap's width means anything.
/// A tenth of the record per block — see the measured collapse table in `bootstrap_edge_ci`.
const MIN_BLOCKS: usize = 10;

/// (#119) The resample block for [`bootstrap_edge_ci`], in ~6-month buckets, DERIVED FROM THE HOLD.
///
/// (#89) added the block draw and worked the honest length out in its own receipt — the dependence
/// length is `2·years` buckets, because two cutoffs less than one hold apart share most of one forward
/// path — and then shipped one bucket anyway. The reason it shipped unwired is visible in the two call
/// sites: both are handed `samples` and `tuning`, and neither carries the horizon, so the knob could
/// only ever be a FIXED count. A fixed count cannot be right for a knob read at 8y, 12y and 20y, which
/// is why the receipt could name the value and still not ship it. This is that missing wire.
///
/// 0 = derive. Any positive value is an explicit override in buckets, so `1` restores the one-bucket
/// draw every band in this repo was printed off — the revert rule the receipt names.
fn bootstrap_block(years: i64, tuning: &BuyHeuristic) -> usize {
    match tuning.bootstrap_block_buckets {
        0 => 2 * years.max(1) as usize,
        n => n,
    }
}

/// (Item 5) bootstrap band on a lane's top-minus-bottom edge between the `lo_p`/`hi_p` percentiles. BLOCK
/// bootstrap: resamples whole ~6mo cutoff buckets with replacement (overlapping windows inside a bucket
/// aren't independent, so the bucket is the resample unit), rescoring the edge each draw. A band that
/// straddles 0 = the point edge is within noise — ship nothing regardless of its sign. Seeded xorshift64
/// (no `rand` dep) -> reproducible. None when there are too few buckets (<4) to resample meaningfully.
/// (Item 10) the percentiles are caller-set so the fund sweep can pass a Bonferroni-tightened tail (5/N) to
/// charge the winner for being the best of N tried factors.
fn bootstrap_edge_ci(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
    iters: usize,
    lo_p: f64,
    hi_p: f64,
    // Resolved by `bootstrap_block`: passed in rather than read off `tuning`, because the horizon
    // that decides it lives at the call site and not in the config.
    block: usize,
) -> Option<(f64, f64)> {
    // BTreeMap, not HashMap: the draw below indexes `keys` with the seeded PRNG, so key ORDER is half
    // the seed. Rust randomizes HashMap iteration per process, which made this band — and the
    // "STRADDLES 0 -> noise" verdict read off it — differ run to run on identical data, in flat
    // contradiction of the "reproducible" claim above. Caught by the frozen-data pin
    // (tests/backtest_fixture.rs), which is the only thing that runs the same data twice.
    // Same idiom `turnover_frac` below already uses for its own date buckets.
    // (#1227) `bootstrap_cluster` resamples whole TICKERS instead: every cutoff of one name rides
    // together, so the overlap between its forward paths stays inside the unit at any hold, with no
    // block to collapse. The bucket-wide market move is what `demean` already took out of `relative`.
    let ids: BTreeMap<&str, i32> = if tuning.bootstrap_cluster {
        let names: std::collections::BTreeSet<&str> = samples.iter().map(|s| s.quote.ticker.as_str()).collect();
        names.into_iter().zip(0..).collect()
    } else {
        BTreeMap::new()
    };
    let mut buckets: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        let k = if tuning.bootstrap_cluster { ids[s.quote.ticker.as_str()] } else { bucket(s.date) };
        buckets.entry(k).or_default().push(i);
    }
    let keys: Vec<i32> = buckets.keys().copied().collect();
    if keys.len() < 4 {
        return None;
    }
    let block = if tuning.bootstrap_cluster { 1 } else { block.clamp(1, keys.len()) };
    // (#119) THE BAND COLLAPSES AT LONG BLOCKS, and that is the second way it lies. Measured on the
    // frozen 8y fixture (64-71 buckets), sweeping the block and reading the WIDTH the receipt asks for:
    //
    //     block    1      2      4      8     16     24
    //     width  62.1   69.3   79.0   70.1   47.0   (refused)
    //
    // It rises, peaks at 4, then FALLS — at block 16 the band is NARROWER than the one-bucket band it
    // was added to widen. That is not a bug in the draw: as the block approaches the record, every draw
    // resembles the whole record, so the resample stops varying and the interval shrinks toward zero.
    // Which means neither end is honest here. Below the dependence length (16 buckets at 8y) the band
    // is the i.i.d. width; at the dependence length the record holds too few blocks to vary. The peak at
    // 4 is the maximum of a bias curve, not an estimate, and picking it would be fitting the artefact.
    //
    // So the guard is on the RATIO: a block bootstrap is trustworthy while the block is a small
    // fraction of the record, and `MIN_BLOCKS` fixes that fraction at a tenth. On this data no horizon
    // clears it — 8y would need 160 buckets (80 years of cutoffs), 12y 240, 20y 400 — so the band
    // refuses everywhere, which is the finding, the same one `split_embargo_months` and (#89) reached:
    // the statistically correct value is larger than the data. Only `block == 1` is exempt, because
    // there is no blocking to collapse; that path keeps the original `< 4` guard and stays byte-identical.
    if block > 1 && keys.len() < MIN_BLOCKS * block {
        return None;
    }
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    // SCORE ONCE PER SAMPLE, not once per sample per draw. `scorer` is a plain `fn` of
    // `samples[i].quote` and `tuning`, both fixed for this whole call, so the value it returns for a
    // given `i` is identical on every one of `iters` draws — and the draw loop below used to call it
    // afresh each time. Measured on a 5057-ticker offline run: 11,779 cutoffs × 1000 iters ≈ 11.8M
    // calls, of which 11,779 were distinct. That redundancy alone was 91% of the whole command's
    // runtime (27.0s of 29.6s).
    //
    // The pool each draw sees is unchanged BY CONSTRUCTION: same bucket order, same `filter_map` order
    // inside a bucket, same values, and `next()` is still called exactly once per bucket per draw in
    // the same sequence — so the PRNG stream, and therefore every band this returns, is untouched.
    let scored: BTreeMap<i32, Vec<(&Sample, f64)>> = buckets
        .iter()
        .map(|(k, idxs)| {
            let rows =
                idxs.iter().filter_map(|&i| Some((&samples[i], scorer(&samples[i].quote, tuning)?))).collect();
            (*k, rows)
        })
        .collect();

    // DRAW SERIALLY, SCORE IN PARALLEL. `next()` is one xorshift stream consumed exactly once per
    // bucket per draw, so which buckets a draw gets has to be decided in draw order — materialise that
    // decision first, and the expensive half (`edge_halves`, a sort plus two means over ~all samples,
    // 1000 times) becomes a pure function of one draw with nothing shared.
    // (#89) BLOCK LENGTH. "the bucket is the resample unit" above is only sound if one bucket spans the
    // dependence, and it does not come close: consecutive cutoffs of the SAME ticker share
    // (2·years−1)/(2·years) of their forward path — 95.8% at 12y, 97.5% at 20y. A block one bucket long
    // against an autocorrelation length of 2·years therefore reproduces the i.i.d. width almost exactly,
    // and this band is the load-bearing "straddles 0 -> noise" test. `block` draws CONTIGUOUS RUNS of
    // that many buckets instead. At 1 it is one bucket per draw — the pre-(#119) behaviour, byte-for-byte:
    // `picks` is then `keys.len()`, each run is one key, and `next()` is consumed once per pick in the
    // same order, so the PRNG stream and every band off it are untouched.
    // (#119) `bootstrap_block` now derives it from the hold, so the shipped band is no longer the
    // one-bucket one. That is a deliberate output change, not a regression — see the receipt.
    let picks = keys.len().div_ceil(block);
    let draws: Vec<Vec<i32>> = (0..iters)
        .map(|_| {
            (0..picks)
                .flat_map(|_| {
                    let start = (next() % keys.len() as u64) as usize;
                    // wraps: the buckets are a cycle for resampling purposes, so a run starting near the
                    // end is not silently short (which would quietly shrink the late-period weight).
                    (0..block).map(|o| keys[(start + o) % keys.len()]).collect::<Vec<_>>()
                })
                .collect()
        })
        .collect();
    // `map_init` keeps the pool hoisted the way the serial loop did — one buffer per worker, cleared
    // per draw, instead of a fresh ~280 KB allocation 1000 times. `collect` preserves draw order (and
    // the sort below would make that moot anyway; it costs nothing to keep).
    let mut edges: Vec<f64> = draws
        .par_iter()
        .map_init(Vec::new, |pool: &mut Vec<(&Sample, f64)>, ks| {
            pool.clear();
            for k in ks {
                pool.extend_from_slice(&scored[k]);
            }
            (pool.len() >= 4).then(|| {
                let (t, b) = edge_halves(pool);
                t - b
            })
        })
        .flatten()
        .collect();
    // `<` vs `<=` here is deliberately LEFT UNPINNED by the mutation audit, and the reason is worth
    // stating: in practice `edges.len()` is either ~`iters` (the lane scores, every draw yields >=4 rows)
    // or 0 (the gate rejects everything), never exactly `iters / 2`. Both spellings therefore decide
    // identically for every caller. Landing on the boundary takes a hand-built sample set where precisely
    // half the seeded draws gate out, which would pin an accident of this PRNG's stream rather than any
    // contract — and would then have to be re-tuned every time the seed or the draw order moved.
    if edges.len() < iters / 2 {
        return None; // too many draws gated out to too few rows -> no trustworthy band
    }
    edges.sort_by(|a, b| a.total_cmp(b));
    Some((percentile(&edges, lo_p), percentile(&edges, hi_p)))
}

/// (Item 9) Per-rebalance turnover fraction of the lane's HELD set: 1 − mean share kept of the top-half
/// tickers between consecutive ~6mo buckets, kept = |a ∩ b| / max(|a|, |b|). (#1075) That is the
/// one-way turnover of an equal-weight book; the Jaccard |a ∩ b| / |a ∪ b| it replaced read above the
/// traded share (one swap in a top-10 read 18%, not 10%). 1.0 = the whole book is replaced each period
/// (max cost), 0.0 = the same names are held (free). <2 measurable buckets -> 0.0 (can't measure;
/// charge nothing).
/// Pure; feeds the net-of-cost line so a churny edge can't read positive once the spread is paid.
fn turnover_frac(scored: &[(&Sample, f64)]) -> f64 {
    let mut by_bucket: BTreeMap<i32, Vec<(&str, f64)>> = BTreeMap::new();
    for (s, v) in scored {
        by_bucket.entry(bucket(s.date)).or_default().push((s.quote.ticker.as_str(), *v));
    }
    // the top-half ticker set per bucket = the names you'd actually hold that period, score-sorted.
    let tops: Vec<HashSet<&str>> = by_bucket
        .into_values()
        .map(|mut rows| {
            rows.sort_by(|a, b| b.1.total_cmp(&a.1));
            rows[..rows.len() / 2].iter().map(|(t, _)| *t).collect::<HashSet<&str>>()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if tops.len() < 2 {
        return 0.0;
    }
    let pairs = tops.len() - 1;
    // every set is non-empty (filtered above), so the larger size is never 0.
    let kept_sum: f64 = tops
        .windows(2)
        .map(|w| w[0].intersection(&w[1]).count() as f64 / w[0].len().max(w[1].len()) as f64)
        .sum();
    1.0 - kept_sum / pairs as f64
}

/// (#104) RECALL and CAPTURE of the pool's extreme winners — the one question this report has never
/// asked. Every other number here is a PRECISION metric: rho, edge, top-N excess and rank-1 h2h all
/// answer "of what I picked, how much was good?". Under the skew a 20-year hold actually lives in
/// (Bessembinder: 4.3% of firms produced all net wealth creation), terminal wealth is decided by the
/// opposite question — "of what was good, how much did I pick?" — and a lane can post an excellent rho
/// while missing every 20-bagger. That lane's book badly trails a sloppier one that caught two.
///
/// Per ~6-month bucket, against the FULL pool including every gate-rejected row (that is the whole
/// point — `gate_audit` prices a gate by the MEAN of what it rejected, which is blind to a rejected
/// 30-bagger):
///   W = the bucket's winners      R = the lane's top-`n` by score
///   recall  = |W ∩ R| / |W|                       — how many of the winners did I hold?
///   capture = Σ multiple(W ∩ R) / Σ multiple(W)   — what fraction of the available extreme wealth?
/// `capture` is the one that matters; recall counts a 30-bagger and a 3-bagger the same.
///
/// A winner is a row in the bucket's top `top_frac` fraction by realized return AND at or above
/// `min_multiple`× — one definition covering both of the doc's cuts (top-1%: 0.01 / 0.0; ten-baggers:
/// 1.0 / 10.0). Multiples floor at 0.0, so a total loss contributes nothing rather than a negative
/// weight. Buckets with under two rows are skipped: recall over a single name is 0 or 1 and means
/// neither. `None` = no winner anywhere in the pool, which is the honest answer for a short horizon,
/// not a zero.
///
/// MEASUREMENT ONLY. Nothing here reaches a score, a gate or the journal.
fn recall_capture(
    samples: &[Sample],
    scored: &[(&Sample, f64)],
    n: usize,
    top_frac: f64,
    min_multiple: f64,
) -> Option<(f64, f64, usize)> {
    let mut pool: BTreeMap<i32, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        pool.entry(bucket(s.date)).or_default().push(s);
    }
    let mut picks: BTreeMap<i32, Vec<(&str, f64)>> = BTreeMap::new();
    for (s, v) in scored {
        picks.entry(bucket(s.date)).or_default().push((s.quote.ticker.as_str(), *v));
    }
    let (mut w_n, mut hit_n, mut w_mult, mut hit_mult) = (0usize, 0usize, 0.0f64, 0.0f64);
    for (b, mut rows) in pool {
        if rows.len() < 2 {
            continue;
        }
        rows.sort_by(|a, z| z.realized.total_cmp(&a.realized));
        let k = ((rows.len() as f64 * top_frac).round() as usize).clamp(1, rows.len());
        let held: HashSet<&str> = picks
            .get(&b)
            .map(|p| {
                let mut p = p.clone();
                p.sort_by(|a, z| z.1.total_cmp(&a.1));
                p.iter().take(n).map(|(t, _)| *t).collect()
            })
            .unwrap_or_default();
        for s in rows.iter().take(k) {
            let mult = (1.0 + s.realized / 100.0).max(0.0);
            if mult < min_multiple {
                continue; // the ladder is sorted, but keep the guard local rather than breaking
            }
            w_n += 1;
            w_mult += mult;
            if held.contains(s.quote.ticker.as_str()) {
                hit_n += 1;
                hit_mult += mult;
            }
        }
    }
    (w_n > 0 && w_mult > 0.0).then(|| (hit_n as f64 / w_n as f64, hit_mult / w_mult, w_n))
}

/// (#1155) The tickers of a bucket's best `frac` share by realized return: the winner cut [`recall_capture`]
/// makes, rounded and never under one name. A bucket under two rows has no winner, as there.
fn top_winners<'a>(group: &[&'a Sample], frac: f64) -> HashSet<&'a str> {
    if group.len() < 2 {
        return HashSet::new();
    }
    let mut rows = group.to_vec();
    rows.sort_by(|a, z| z.realized.total_cmp(&a.realized));
    let k = ((rows.len() as f64 * frac).round() as usize).clamp(1, rows.len());
    rows.into_iter()
        .take(k)
        .map(|s| s.quote.ticker.as_str())
        .collect()
}

/// (#1155) The skew reading of the top-N ladder: per rung, the share of booked windows whose book holds at
/// least one of its window's top-1% winners. Under the skew a long hold lives in, a book that never holds
/// one cannot carry the wealth the few winners make, whatever its mean says. `None` when no rung booked
/// a window. Display only.
fn winner_in_book_line(per_rung: &[(usize, Vec<bool>)]) -> Option<String> {
    let booked: Vec<&(usize, Vec<bool>)> = per_rung
        .iter()
        .filter(|(_, hits)| !hits.is_empty())
        .collect();
    let windows = booked.first()?.1.len();
    let cells: Vec<String> = booked
        .iter()
        .map(|(n, hits)| {
            let share = hits.iter().filter(|h| **h).count() as f64 / hits.len() as f64;
            format!("top-{n} {:.0}%", share * 100.0)
        })
        .collect();
    Some(format!(
        "  WINNER IN BOOK (#1155): share of the {windows} booked windows whose book holds a top-1% winner | {}",
        cells.join(" · ")
    ))
}

/// One attribution row. A STRUCT and not the 5-tuple this started as, for a mutation-gate reason
/// worth writing down: `--in-diff` grades a changed function's WHOLE return-replacement set, and
/// cargo-mutants enumerates a tuple field by field — every combination of `""`, `0`/`1` and
/// `0.0`/`1.0`/`-1.0`. The tuple spelling made this one function 223 return mutants against 13 here,
/// and CI's budget is 6, so the gate would have sampled ~1 in 79 of them and called that covered.
#[derive(Debug, Default)]
struct MissRow {
    reason: &'static str,
    winners: usize,
    /// Σ multiple of the winners filed under `reason` — the missed WEALTH, which is the number that
    /// matters: a 30-bagger and a 3-bagger are not the same miss.
    mult: f64,
    /// Winners failing EXACTLY this one gate. The only cohort a single-knob loosening can buy back,
    /// so Σ `sole_mult` over the table is the hard ceiling on what moving any one knob can recover.
    /// `unassessable` and `out-ranked` have no gate list, so their sole columns are structurally 0.
    sole: usize,
    sole_mult: f64,
}

/// (#128) One gate PAIR among the winners that failed EXACTLY two gates. `MissRow` above answers
/// "which gate gets the blame"; this answers the question that follows from its own result, which is
/// that the blame column is not actionable — `range` is named first for half the missed wealth and is
/// the SOLE blocker for none of it. A two-knob arm needs to know WHICH two.
#[derive(Debug, Default)]
struct PairRow {
    /// In gate order, because `gate_failures` pushes in gate order — so the key is stable and
    /// `(range, 1Y+)` can never also appear as `(1Y+, range)`.
    pair: (&'static str, &'static str),
    winners: usize,
    mult: f64,
    /// Winners where BOTH members are inside their own gate's near-miss slack — `gate_failures`'
    /// third field, already computed at every push site and until now discarded. This is the only
    /// part of the pair a loosening anyone would actually ship can admit; the rest misses by miles.
    near: usize,
    near_mult: f64,
}

/// Everything `missed_winner_reasons` returns, as ONE struct rather than the tuple it would otherwise
/// have grown into. Same mutation-gate reason as `MissRow` and it compounds: five tuple fields is a
/// return-replacement set in the hundreds against one `Default` here, and CI's budget is 6.
#[derive(Debug, Default)]
struct MissTable {
    rows: Vec<MissRow>,
    pairs: Vec<PairRow>,
    /// `(gates failed, winners, missed wealth)` ascending, empty buckets omitted. `0` is `out-ranked`;
    /// `unassessable` is absent by construction because it has no gate list to count. This is the
    /// honesty check on `pairs`: if the wealth sits at 3+, no two-knob arm reaches it and the pair
    /// table is a rounding error. Filtered HERE and not at the print site on purpose — the print site
    /// sits behind `print_recall_capture`, which defaults false and only ci-settings turns on (#154), so a
    /// predicate left there is caught by a golden diff at best, never by a unit test. Same rule as the
    /// `rows.is_empty()` guard below.
    by_count: Vec<(usize, usize, f64)>,
    w_n: usize,
    w_mult: f64,
}

/// (#127) WHY the misses were missed — the attribution `recall_capture` above cannot make.
///
/// `recall_capture` counts what the lane failed to hold; it never says whether a missed 20-bagger was
/// GATED OUT, was never scorable to begin with, or was admitted and simply out-ranked. Those three
/// have nothing in common as fixes: the first is a threshold, the second is DATA COVERAGE, and only
/// the third is something a scoring weight can touch. Sweeping a knob without that distinction is how
/// `(#60)` and `(#74)` each spent a round on a gate that was never the binding constraint.
///
/// The measurement this exists to serve: the graded basket does not bind. top-20 and top-50 return a
/// byte-identical book at all three horizons because the gates admit ~6 names per window, so the
/// ranking is choosing ~6 candidates for 10 slots while the lane captures 2-4% of the pool's extreme
/// wealth. `out-ranked`'s share is the direct test of that claim: small = selection genuinely cannot
/// move this book, and every future weight proposal has one number to answer.
///
/// Same winner definition, same buckets and the same `n` as `recall_capture` — deliberately, so the
/// two reconcile: the returned counts sum to `w_n − hit_n` and the returned multiples to
/// `w_mult − hit_mult`. The caller prints that tie-back. If it does not hold, the two instruments
/// disagree about the winner set and neither number means anything.
///
/// GROWTH LANE ONLY, enforced by the caller. `picks::gate_failures` reports the GROWTH gates and
/// `gate_failures_agrees_with_the_scorer` pins it to the scorer itself, so "blocked by X" here is the
/// same X the lane actually applied — but running it beside `buy_score` would blame one lane's misses
/// on another lane's gates.
///
/// THE FIRST FAILURE IS NOT NECESSARILY THE ONLY ONE, and reading this table without that in mind
/// inverts its meaning. A winner that fails `range`, `cagr` and `1Y+` is filed under `range` because
/// that is first in gate order — so a large `range` share does NOT imply that loosening `range` would
/// admit those names. The sweep proves the gap: `growth_min_range_pct -10` admits n=1 at every
/// horizon while `range` takes the blame for half the missed wealth. Hence the SOLE-BLOCKER columns:
/// only a winner failing EXACTLY ONE gate can be bought back by moving that one knob, and the sole
/// count is therefore the honest ceiling on what any single-knob loosening can recover. `unassessable`
/// and `out-ranked` have no gate list at all, so their sole columns are structurally zero.
///
/// (#128) …and the pair table is the answer to what that ceiling leaves. Since the sole column came
/// back at ~0 for `range` at every horizon, the reachable wealth is whatever fails a SMALL NUMBER of
/// gates together — so this also tallies, per winner, the exact pair it failed (only for winners
/// failing exactly two: a five-failure name has ten pairs and a two-knob arm buys none of them) and
/// the count histogram that says whether "small number" is even two.
///
/// Returns rows and pairs sorted by missed wealth desc, plus `by_count`, `w_n` and `w_mult`.
/// MEASUREMENT ONLY — nothing here reaches a score, a gate or the journal.
fn missed_winner_reasons(
    samples: &[Sample],
    scored: &[(&Sample, f64)],
    tuning: &BuyHeuristic,
    n: usize,
    top_frac: f64,
    min_multiple: f64,
) -> Option<MissTable> {
    let mut pool: BTreeMap<i32, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        pool.entry(bucket(s.date)).or_default().push(s);
    }
    let mut picked: BTreeMap<i32, Vec<(&str, f64)>> = BTreeMap::new();
    for (s, v) in scored {
        picked.entry(bucket(s.date)).or_default().push((s.quote.ticker.as_str(), *v));
    }
    let (mut w_n, mut w_mult) = (0usize, 0.0f64);
    let mut tally: HashMap<&'static str, (usize, f64, usize, f64)> = HashMap::new();
    let mut pair_tally: HashMap<(&'static str, &'static str), (usize, f64, usize, f64)> = HashMap::new();
    let mut count_tally: BTreeMap<usize, (usize, f64)> = BTreeMap::new();
    for (b, mut rows) in pool {
        if rows.len() < 2 {
            continue; // same skip as `recall_capture`, or the two stop counting the same winners
        }
        rows.sort_by(|a, z| z.realized.total_cmp(&a.realized));
        let k = ((rows.len() as f64 * top_frac).round() as usize).clamp(1, rows.len());
        let held: HashSet<&str> = picked
            .get(&b)
            .map(|p| {
                let mut p = p.clone();
                p.sort_by(|a, z| z.1.total_cmp(&a.1));
                p.iter().take(n).map(|(t, _)| *t).collect()
            })
            .unwrap_or_default();
        for s in rows.iter().take(k) {
            let mult = (1.0 + s.realized / 100.0).max(0.0);
            if mult < min_multiple {
                continue;
            }
            w_n += 1;
            w_mult += mult;
            if held.contains(s.quote.ticker.as_str()) {
                continue; // held, so not a miss: there is nothing to attribute
            }
            // `None` = not assessable as a growth candidate at all (leveraged / stablecoin / unknown
            // turnover / no 1Y). Empty = cleared every gate and the ranking still left it out.
            // Otherwise the FIRST failure in gate order is the one that blocked it.
            let fails = picks::gate_failures(&s.quote, tuning);
            let reason = match &fails {
                None => "unassessable",
                Some(v) => v.first().map_or("out-ranked", |f| f.0),
            };
            // SOLE blocker = this winner fails exactly one gate, so moving that one knob is the only
            // case where a single-knob loosening could actually buy it back.
            let sole = fails.as_ref().is_some_and(|v| v.len() == 1);
            let e = tally.entry(reason).or_insert((0, 0.0, 0, 0.0));
            e.0 += 1;
            e.1 += mult;
            if sole {
                e.2 += 1;
                e.3 += mult;
            }
            // (#128) the same misses again, filed by HOW MANY gates blocked them rather than by which
            // one is first. `unassessable` is deliberately absent: `None` is not zero gates failed, it
            // is no gate list at all, and folding it into `[0]` would read as "cleared everything".
            let Some(v) = &fails else { continue };
            let c = count_tally.entry(v.len()).or_insert((0, 0.0));
            c.0 += 1;
            c.1 += mult;
            // EXACTLY two, not every pair drawn from a k-failure name: five failures make ten pairs and
            // a two-knob loosening admits none of them, so enumerating those would credit each row with
            // wealth no arm can reach. Exactly-2 is the cohort a two-knob arm CAN admit, and it files
            // each winner under one key, so the column still sums.
            let [a, z] = v.as_slice() else { continue };
            let e = pair_tally.entry((a.0, z.0)).or_insert((0, 0.0, 0, 0.0));
            e.0 += 1;
            e.1 += mult;
            // BOTH inside their own gate's near-miss slack. One member near and the other missing by a
            // mile is not a pair any shippable loosening reaches, and counting it would overstate the
            // arm before the arm is even graded.
            if a.2 && z.2 {
                e.2 += 1;
                e.3 += mult;
            }
        }
    }
    let mut rows: Vec<MissRow> = tally
        .into_iter()
        .map(|(reason, (winners, mult, sole, sole_mult))| MissRow { reason, winners, mult, sole, sole_mult })
        .collect();
    // By missed WEALTH, not by count — the whole point of capture is that a 30-bagger and a 3-bagger
    // are not the same miss. Name tiebreak because `HashMap` order is unspecified and this table has
    // to read identically at any thread count.
    rows.sort_by(|a, z| z.mult.total_cmp(&a.mult).then(a.reason.cmp(z.reason)));
    let mut pairs: Vec<PairRow> = pair_tally
        .into_iter()
        .map(|(pair, (winners, mult, near, near_mult))| PairRow { pair, winners, mult, near, near_mult })
        .collect();
    pairs.sort_by(|a, z| z.mult.total_cmp(&a.mult).then(a.pair.cmp(&z.pair)));
    let by_count: Vec<(usize, usize, f64)> =
        count_tally.into_iter().map(|(gates, (winners, mult))| (gates, winners, mult)).collect();
    // `rows.is_empty()` is the "every winner was held" case, and it returns None for the same reason
    // the other two do: the caller divides by `w_mult` and by Σ`mult`, so an empty table would print
    // a NaN. Deciding it HERE and not at the print site is deliberate — the print site sits inside
    // `if tuning.print_recall_capture`, which defaults false and only ci-settings turns on (#154), so a
    // branch left there is caught by a golden diff at best, never by a unit test. Guards belong where a
    // test can reach them.
    // `pairs` is allowed to be empty — "nothing failed exactly two gates" is a real and publishable
    // answer, and it refuses the arm rather than voiding the table. Only `rows` gates the None.
    // NO `w_n > 0` TERM: it was here and it is redundant, because every winner contributes a
    // non-negative multiple, so `w_mult > 0.0` cannot hold with `w_n == 0`. A guard that re-decides
    // what the next guard already decided is an unkillable mutant by construction (`>` and `>=` agree
    // on a usize that is only ever compared to zero), so it is deleted rather than tested.
    (w_mult > 0.0 && !rows.is_empty()).then_some(MissTable { rows, pairs, by_count, w_n, w_mult })
}

/// Mean AND median of a cohort's forward peer-relative returns. Both, always. `relative` over a
/// multi-decade window is heavily right-skewed — one 40-bagger moves a 400-name mean by more than the
/// other 399 combined — so the mean can sit on the far side of zero from the typical name. Every other
/// slice in this report already prints both for exactly that reason (the rank-slice ladder says so in
/// its own header: "median is lottery-ticket-immune"); these partition audits were the holdouts.
/// Callers guard the empty case, so the `max(1)` only keeps the divide total.
fn cohort_stats(v: &[f64]) -> (f64, f64) {
    (v.iter().sum::<f64>() / v.len().max(1) as f64, median(v.to_vec()))
}

/// Directional verdict for a two-cohort gap, fired ONLY when mean and median agree on the sign. When
/// they disagree the gap is tail-driven and NEITHER number is the answer on its own — so say that
/// rather than silently picking one. The disagreement IS the finding: it means a handful of extreme
/// names, not the cohort, is producing the headline.
fn gap_verdict(mean_gap: f64, med_gap: f64, positive: &str, negative: &str) -> String {
    match (mean_gap > 0.0, med_gap > 0.0) {
        (true, true) => positive.to_string(),
        (false, false) => negative.to_string(),
        _ => format!(
            "SPLIT — mean ({mean_gap:+.1}) and median ({med_gap:+.1}) disagree: the gap is driven by \
             the tail, not the typical name. Read the median."
        ),
    }
}

/// (#111) (round 3 §8) THE HONEST DENOMINATOR. The lane's whole thesis is that a long compounding
/// record predicts the next one — `growth_min_cagr` deletes every name that has not delivered its bar,
/// so the bar is not a tilt, it is the universe. Nothing has ever asked what fraction of the names that
/// CLEARED it went on to clear it again, and that number is one pass over a pool already in memory.
///
/// Returns `(qualifying, sustained, base_rate_of_everyone)`: how many samples cleared `bar` on their
/// trailing leg at the cutoff, how many of those also cleared it forward over `years`, and — the part
/// that makes it a finding rather than a statistic — the fraction of the WHOLE pool that cleared it
/// forward. A conditional rate is meaningless without it: 30% sounds bleak until the unconditional rate
/// is 12%, at which point the record is carrying real information, and damning if the unconditional
/// rate is 34%.
///
/// The bar is `growth_min_cagr`, not a new knob: the question is about the gate the lane actually
/// ships, and a separate threshold here would answer a question nobody is asking. Trailing CAGR routes
/// through `picks::long_cagr_pct` (non-negotiable 4), forward CAGR through `core::cagr` on the same
/// `realized` every other metric in this report reads. `None` when nothing clears the bar.
fn persistence_base_rate(
    samples: &[Sample],
    tuning: &BuyHeuristic,
    years: i64,
    bar: f64,
) -> Option<(usize, usize, f64)> {
    if samples.is_empty() || bar <= 0.0 {
        return None;
    }
    let made_it = |s: &Sample| core::cagr(s.realized, years as f64) >= bar;
    let qualifying: Vec<&Sample> = samples
        .iter()
        .filter(|s| picks::long_cagr_pct(&s.quote, tuning).is_some_and(|c| c >= bar))
        .collect();
    if qualifying.is_empty() {
        return None;
    }
    let sustained = qualifying.iter().filter(|s| made_it(s)).count();
    let everyone = samples.iter().filter(|s| made_it(s)).count() as f64 / samples.len() as f64;
    Some((qualifying.len(), sustained, everyone * 100.0))
}

/// (#167) One row of the held-loser table: a factor, the size of each cohort, each cohort's median
/// and the [`auc`] separating them. See [`held_loser_factors`] for why this is a named struct and
/// not the six-field tuple it replaced.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct HeldLoserRow {
    factor: &'static str,
    n_win: usize,
    n_lose: usize,
    med_win: f64,
    med_lose: f64,
    auc: f64,
}

/// (#159) The held book's LOSERS, split by factor — the one question every other instrument in this
/// file declines to ask. `recall_capture` and `missed_winner_reasons` price what the ranking MISSED;
/// `gate_audit` and `gate_sweep` price what the gates REJECTED. Nothing priced what the lane BOUGHT
/// AND LOST, and that is the only cohort a NEW gate can act on at all — a gate cannot reach a name
/// the book never held, and five refused rounds were spent proposing gates from the literature
/// instead of from this cohort.
///
/// THE GATE VOCABULARY IS USELESS HERE BY CONSTRUCTION, which is the whole reason this reads
/// factors: a held name cleared every gate, so `picks::gate_failures` is empty for all of them and
/// attributing by gate would print one empty column. So the split is by FACTOR, over the same
/// `FUND_FACTORS` registry `sweep_fund_factor` searches, and each row gives the median among held
/// names that MADE money against those that LOST it. A factor that separates the two is a gate worth
/// proposing; one that does not is a gate worth never proposing again.
///
/// A LOSER IS `realized < 0` — money lost outright over the hold, not merely trailing its peers. The
/// peer-relative reading already exists as `relative`, and half a book sits below its own median by
/// definition, which would make every factor look like it separated something. Losing money is what
/// a survival gate is actually asked to prevent.
///
/// (#167) A NAMED ROW, NOT A TUPLE, AND THE REASON IS THE MUTATION GATE. cargo-mutants enumerates a
/// tuple return element-by-element and multiplies the candidates — `""` for `&str`, `0`/`1` for
/// `usize`, `0.0`/`1.0`/`-1.0` for `f64` — so the six-field tuple this used to return listed 220
/// replacement mutants on its own. For a NAMED type it emits one `vec![Default::default()]` instead.
/// That is not a cosmetic difference: `BUDGET` is 6 in ci.yml, so a 233-mutant diff shards 1/39 and
/// CI grades a handful — a green that reads exactly like a covered gate. Keep this a struct.
///
/// Returns one [`HeldLoserRow`] per factor and SKIPS any that cannot fill both sides. (#165) added
/// the `auc`: the medians give each factor's LEVEL, which is only readable in its own units, while
/// the `auc` is what makes two factors comparable to EACH OTHER. Both are kept — dropping the
/// medians would leave a row nobody can sanity-check against the printed columns elsewhere in this
/// report. A skipped row is a fact about what the point-in-time pool covers, not about the factor —
/// the caller prints the count it dropped so the absence stays visible.
fn held_loser_factors(
    scored: &[(&Sample, f64)],
    n: usize,
    factors: &[&'static str],
) -> Vec<HeldLoserRow> {
    let held = held_names(scored, n);
    factors
        .iter()
        .filter_map(|name| {
            let (mut win, mut lose) = (Vec::new(), Vec::new());
            for s in &held {
                let Some(v) = s.fund.as_ref().and_then(|f| core::select_fund_factor(f, name)) else {
                    continue; // no as-of fundamentals for this name -> it votes on no factor at all
                };
                if s.realized < 0.0 {
                    lose.push(v);
                } else {
                    win.push(v);
                }
            }
            (!win.is_empty() && !lose.is_empty()).then(|| HeldLoserRow {
                factor: name,
                n_win: win.len(),
                n_lose: lose.len(),
                // BEFORE the medians: `median` takes the Vec by value, so the cohorts are gone after.
                auc: auc(&win, &lose),
                med_win: median(win),
                med_lose: median(lose),
            })
        })
        .collect()
}

/// (#159) The held book: the top `n` of each entry bucket by score. A bucket of one is skipped, the
/// same skip `recall_capture` applies, so every held-book instrument counts one pool. (#1242) split
/// out of [`held_loser_factors`] so [`held_loser_bands`] reads the very same names.
fn held_names<'a>(scored: &[(&'a Sample, f64)], n: usize) -> Vec<&'a Sample> {
    let mut picked: BTreeMap<i32, Vec<(&Sample, f64)>> = BTreeMap::new();
    for (s, v) in scored {
        picked.entry(bucket(s.date)).or_default().push((s, *v));
    }
    let mut held: Vec<&Sample> = Vec::new();
    for (_, mut rows) in picked {
        if rows.len() < 2 {
            continue;
        }
        rows.sort_by(|a, z| z.1.total_cmp(&a.1));
        held.extend(rows.iter().take(n).map(|(s, _)| *s));
    }
    held
}

/// (#1242) The [`auc`] of one factor between the names in `pool` that made money and those that lost
/// it. None when either side is empty, or no name carries the factor.
fn loser_auc(pool: &[&Sample], name: &str) -> Option<f64> {
    let (mut win, mut lose) = (Vec::new(), Vec::new());
    for s in pool {
        if let Some(v) = s.fund.as_ref().and_then(|f| core::select_fund_factor(f, name)) {
            if s.realized < 0.0 {
                lose.push(v);
            } else {
                win.push(v);
            }
        }
    }
    (!win.is_empty() && !lose.is_empty()).then(|| auc(&win, &lose))
}

/// (#1242) One factor's held-book AUC with its cluster-bootstrap band, and the same AUC read inside
/// each earnings-yield tercile of the held book (lowest yield, the richest third, first). A named row for the reason
/// [`HeldLoserRow`] is one: the mutation gate.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct AucBand {
    factor: &'static str,
    auc: f64,
    lo: f64,
    hi: f64,
    terciles: [Option<f64>; 3],
}

/// (#1242) The profitability ratios the held-loser table has read backwards, as one family.
const PROFIT_FAMILY: [&str; 7] = ["gross_margin", "gp_assets", "roe", "quality", "op_margin", "net_margin", "cf_assets"];

/// (#1242) Prints [`held_loser_bands`] for the GROWTH lane's held book. Display only.
#[mutants::skip]
fn report_held_loser_bands(samples: &[Sample], tuning: &BuyHeuristic) {
    let scored: Vec<(&Sample, f64)> =
        samples.iter().filter_map(|s| growth_score(&s.quote, tuning).map(|v| (s, v))).collect();
    println!("  (#1242) held book's loser AUC, 95% band from 2000 draws of whole (entry date, sector) clusters:");
    for AucBand { factor, auc, lo, hi, terciles } in held_loser_bands(&scored, VERDICT_TOP, &PROFIT_FAMILY, 2000) {
        let t = terciles.map(|x| x.map_or("  n/a".to_string(), |v| format!("{v:>5.2}")));
        println!(
            "    {factor:<13} AUC {auc:>5.2}  band {lo:>5.2} to {hi:>5.2}   inside E/P thirds (rich, mid, cheap) {} {} {}",
            t[0], t[1], t[2]
        );
    }
    println!("    read the family row as the one test; a band holding 0.5 = no reading. Survivors only: dead names carry no fund rows.");
}

/// (#1245) The walk's event flags at one cutoff, one bit each: 1 a dividend cut (the last 12 months paid
/// under 3/4 of the 12 before, which held at least two payments), 2 a reverse split (a ratio under 1) and
/// 4 a fresh S&P 500 add (a span opened after `origin`, the source's first date, which every incumbent's
/// span shares). The last two look `months` back. Each window is (from, as_of]: an event on the cutoff
/// counts, one exactly a window back does not.
/// shortcut: the cut always reads one year against the year before, whatever `months` is, and a special
/// dividend in the prior year reads as a cut. Both payout sources (Yahoo, the dead CSV) are split-adjusted.
fn event_flags(
    divs: &[(chrono::NaiveDate, f64)],
    splits: &[(chrono::NaiveDate, f64)],
    spans: Option<&[(chrono::NaiveDate, Option<chrono::NaiveDate>)]>,
    origin: chrono::NaiveDate,
    as_of: chrono::NaiveDate,
    months: u32,
) -> u32 {
    let back = |m: u32| as_of.checked_sub_months(chrono::Months::new(m)).unwrap_or(chrono::NaiveDate::MIN);
    let (year, two) = (back(12), back(24));
    let paid = |from: chrono::NaiveDate, to: chrono::NaiveDate| {
        divs.iter().filter(|(d, _)| from < *d && *d <= to).fold((0usize, 0.0f64), |(n, t), (_, v)| (n + 1, t + v))
    };
    let ((prior_n, prior), (_, last)) = (paid(two, year), paid(year, as_of));
    let cut = prior_n >= 2 && last < 0.75 * prior;
    let from = back(months);
    let reverse = splits.iter().any(|(d, r)| from < *d && *d <= as_of && *r < 1.0);
    let added = spans.is_some_and(|sp| sp.iter().any(|(s, _)| from < *s && *s <= as_of && origin < *s));
    u32::from(cut) + (u32::from(reverse) << 1) + (u32::from(added) << 2)
}

/// (#1245) How often each [`event_flags`] bit fired on a stock cutoff of this walk. Display only.
#[mutants::skip]
fn report_event_flags(samples: &[Sample], months: usize) {
    let stocks: Vec<u32> =
        samples.iter().filter(|s| picks::asset_class(&s.quote) == 2).map(|s| s.quote.event_flags).collect();
    let fired = |bit: u32| stocks.iter().filter(|f| *f & bit != 0).count();
    println!(
        "  (#1245) event flags over {months} months, of {} stock cutoffs: dividend cut {}, reverse split {}, fresh S&P add {}, any {}",
        stocks.len(),
        fired(1),
        fired(2),
        fired(4),
        fired(7)
    );
}

/// (#1242) How sure is "quality reads backwards on this book"? The held-loser AUC (#159, #165) is the
/// measured reason behind several refusals, and it stands on 7 to 35 losers. Two things can fake it.
///
/// CLUSTERS. Losers bunch by entry date and sector: one bad quarter in one sector puts five held names
/// on the losing side at once, and a per-name error bar counts them as five independent votes. So the
/// band resamples whole (entry bucket, sector) clusters with replacement, `draws` times, seeded, and
/// reports the 2.5th and 97.5th percentile of the factor's AUC across draws. A draw with one side
/// empty casts no vote for that factor.
///
/// ONE FAMILY. The profitability ratios are one signal read seven ways, so the last row, `family`,
/// is the mean AUC across the factor rows, with its band taken from the per-draw mean. Read that row
/// as THE test; the per-factor rows only show which ratio drives it.
///
/// THE VALUE CONFOUND. Profitable names tend to be the expensive ones, and the value signal reads
/// strongly on this book. `terciles` re-reads each AUC inside each third of the held book by
/// earnings yield, so a factor that only separates because it tracks price shows up near 0.5 there.
///
/// Survivors only in practice: dead names carry no fund rows, so they vote on nothing here. Measure
/// only. It feeds no score, gate, journal or book.
fn held_loser_bands(scored: &[(&Sample, f64)], n: usize, factors: &[&'static str], draws: usize) -> Vec<AucBand> {
    let held = held_names(scored, n);
    let mut by_ep: Vec<(f64, &Sample)> = held
        .iter()
        .filter_map(|s| s.fund.as_ref().and_then(|f| core::select_fund_factor(f, "earnings_yield")).map(|v| (v, *s)))
        .collect();
    by_ep.sort_by(|a, z| a.0.total_cmp(&z.0));
    let mut thirds: [Vec<&Sample>; 3] = Default::default();
    for (i, (_, s)) in by_ep.iter().enumerate() {
        thirds[i * 3 / by_ep.len()].push(s);
    }
    let mut by: BTreeMap<(i32, Option<&str>), Vec<&Sample>> = BTreeMap::new();
    for s in &held {
        by.entry((bucket(s.date), s.quote.sector.as_deref())).or_default().push(s);
    }
    let clusters: Vec<Vec<&Sample>> = by.into_values().collect();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    // one vote list per factor, plus the family's at the end
    let mut boot: Vec<Vec<f64>> = vec![Vec::new(); factors.len() + 1];
    for _ in 0..draws {
        let mut pool: Vec<&Sample> = Vec::new();
        for _ in 0..clusters.len() {
            pool.extend(&clusters[(core::next_rand(&mut state) % clusters.len() as u64) as usize]);
        }
        let got: Vec<f64> =
            factors.iter().zip(&mut boot).filter_map(|(f, b)| loser_auc(&pool, f).inspect(|a| b.push(*a))).collect();
        if !got.is_empty() {
            boot[factors.len()].push(got.iter().sum::<f64>() / got.len() as f64);
        }
    }
    let band = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        (core::percentile(v, 2.5), core::percentile(v, 97.5))
    };
    let mean = |v: Vec<f64>| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let mut rows: Vec<AucBand> = Vec::new();
    for (name, b) in factors.iter().zip(boot.iter_mut()) {
        if let Some(auc) = loser_auc(&held, name) {
            let (lo, hi) = band(b);
            rows.push(AucBand { factor: name, auc, lo, hi, terciles: [0, 1, 2].map(|t| loser_auc(&thirds[t], name)) });
        }
    }
    if let Some(auc) = mean(rows.iter().map(|r| r.auc).collect()) {
        let (lo, hi) = band(&mut boot[factors.len()]);
        let terciles = [0, 1, 2].map(|t| mean(rows.iter().filter_map(|r| r.terciles[t]).collect()));
        rows.push(AucBand { factor: "family", auc, lo, hi, terciles });
    }
    rows
}

/// (#9) Do the hard growth GATES actually select winners? Every rho/edge the lanes print is computed
/// over GATED-IN names only (`report_lane` drops the `None`s), so a gate that quietly discards future
/// winners is invisible. This partitions the FULL de-meaned sample by whether `scorer` admits it
/// (Some = passed the gates, None = rejected) and compares the two groups' mean forward peer-relative
/// return. ACCEPTED mean ≫ REJECTED mean = the gates pick winners; gap ≈ 0 or negative = the gates are
/// shrinking the pool for no edge (e.g. the 80% range gate dropping a great compounder that's merely in a
/// 20% correction). Pure measurement — no ranking change. Returns the gap (accepted − rejected) for the
/// test; None when either side has too few rows to mean. Generic over the scorer so it's unit-testable
/// without building quotes that clear growth_score's gate maze (same pattern as `lane_metrics`).
fn gate_audit(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> Option<(f64, f64, f64)> {
    let (accepted, rejected): (Vec<&Sample>, Vec<&Sample>) =
        samples.iter().partition(|s| scorer(&s.quote, tuning).is_some());
    println!("\n── GATE AUDIT (growth gates: do the names they EXCLUDE actually underperform?) ──");
    if accepted.len() < 4 || rejected.len() < 4 {
        println!("  {} accepted / {} rejected — too few on one side to compare.", accepted.len(), rejected.len());
        return None;
    }
    let rels = |g: &[&Sample]| g.iter().map(|s| s.relative).collect::<Vec<f64>>();
    let (am, amed) = cohort_stats(&rels(&accepted));
    let (rm, rmed) = cohort_stats(&rels(&rejected));
    let (gap, gap_med) = (am - rm, amed - rmed);
    println!("  accepted (passed gates): n={:<5} fwd peer-relative  mean {am:+.1} | med {amed:+.1} pts", accepted.len());
    println!("  rejected (failed gates): n={:<5} fwd peer-relative  mean {rm:+.1} | med {rmed:+.1} pts", rejected.len());
    let verdict = gap_verdict(
        gap,
        gap_med,
        "gates SELECT winners (accepted beat the rejected pool)",
        "gates ADD NOTHING — the rejected names did as well or better; consider loosening them",
    );
    println!("  gap  mean {gap:+.1} | med {gap_med:+.1} pts  ->  {verdict}");
    // (#151) `amed` rides along because the GATE SWEEP below needs it and there must be ONE
    // definition of "the median the shipped gates actually accept" (non-negotiable #4). Recomputing
    // it inside `gate_sweep` off the same partition would be a second opinion about the same number.
    Some((gap, gap_med, amed))
}

/// (#10 helper) Forward peer-relative return of the names REJECTED under `base` tuning but ACCEPTED
/// once loosened to `relaxed` (the set a looser gate NEWLY admits). `(count, mean, median)`, or None when
/// loosening admits nobody. Both statistics because these sets run tiny (n=4..50 in practice) and a
/// single survivor can carry the mean on its own — see `cohort_stats`. Pure + scorer-generic so the
/// per-gate sweep is unit-testable without building quotes that clear growth_score's gate maze (same
/// trick as `gate_audit`/`lane_metrics`).
fn newly_admitted_stats(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    base: &BuyHeuristic,
    relaxed: &BuyHeuristic,
) -> Option<(usize, f64, f64)> {
    let newly: Vec<f64> = samples
        .iter()
        .filter(|s| scorer(&s.quote, base).is_none() && scorer(&s.quote, relaxed).is_some())
        .map(|s| s.relative)
        .collect();
    if newly.is_empty() {
        return None;
    }
    let (mean, med) = cohort_stats(&newly);
    Some((newly.len(), mean, med))
}

/// (#334) The WEIGHT SWEEP's arithmetic, scorer-generic for the same reason `newly_admitted_stats` is. Per
/// bucket, rank at `base` and at `notched` the way the book loop does (crypto skipped, samples order, stable
/// descending sort) and cut both at `top`. An equal-weight top-`top` book is set-invariant ((#149)), so a
/// weight reaches it ONLY through the names it swaps across the edge: the gap is the mean `realized` of the
/// ENTRANTS minus that of the names they DISPLACED. `realized`, not `relative`: both sides share one bucket,
/// so the peer mean cancels. Only a PURE swap grades — a notch that grew or shrank the pool changes the
/// book's size too, and a missing gap is not a zero one. Returns (buckets ranked, one gap per swap).
fn weight_swap_gaps(
    samples: &[Sample],
    scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>,
    base: &BuyHeuristic,
    notched: &BuyHeuristic,
    top: usize,
) -> (usize, Vec<f64>) {
    let mut by_bucket: BTreeMap<i32, Vec<&Sample>> = BTreeMap::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        by_bucket.entry(bucket(s.date)).or_default().push(s);
    }
    let cut = |group: &[&Sample], t: &BuyHeuristic| -> Vec<usize> {
        let quotes: Vec<&Quote> = group.iter().map(|s| s.quote.as_ref()).collect();
        let mut ranked: Vec<(usize, f64)> = scorer(&quotes, t).into_iter().enumerate().filter_map(|(i, v)| v.map(|v| (i, v))).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.into_iter().take(top).map(|(i, _)| i).collect()
    };
    let mean = |group: &[&Sample], ix: &[usize]| ix.iter().map(|i| group[*i].realized).sum::<f64>() / ix.len() as f64;
    let gaps = by_bucket
        .values()
        .filter_map(|group| {
            let (was, now) = (cut(group, base), cut(group, notched));
            let entrants: Vec<usize> = now.iter().copied().filter(|i| !was.contains(i)).collect();
            let displaced: Vec<usize> = was.iter().copied().filter(|i| !now.contains(i)).collect();
            (!entrants.is_empty() && entrants.len() == displaced.len()).then(|| mean(group, &entrants) - mean(group, &displaced))
        })
        .collect();
    (by_bucket.len(), gaps)
}

/// (#334) WHICH shipped ranking weight is mis-set? The GATE SWEEP's twin for the weights, which had no free
/// instrument at all — only one-off A/B rounds. Each nonzero weight at ×0 and ×2 (`picks::weight_notches`,
/// pre-registered), priced by what it swaps across the top-`VERDICT_TOP` edge. INFORM ONLY: RE-PROBE means
/// a full A/B round, never a weight move. `k/N` is the swapped-bucket count, because on a saturated lane
/// (POOL CENSUS above: `pit`'s median pool 5-7 against a basket of 10) most buckets hold nobody to swap in,
/// and a row there is a count over the residue. `stress` is the non-vacuous lane: a refusal there is
/// permanent, a pass proves nothing. The forward half is `screen`'s `Snapshot::swap`, graded by `track`.
/// The `base` cut is recomputed per notch, as `gate_sweep` does; hoist it if this stage ever shows in a profile.
fn weight_sweep(samples: &[Sample], tuning: &BuyHeuristic) {
    println!("\n── WEIGHT SWEEP (#334) (each shipped weight x0 / x2 -> top-{VERDICT_TOP} ENTRANTS minus the names they DISPLACED, realized fwd %) ──");
    println!("  a weight never gates, so an equal-weight book feels it ONLY through the names it swaps across the edge (#149).");
    println!("  RE-PROBE needs mean AND median positive and means a full A/B round, never a weight move. k/N = buckets that swapped.");
    let rows: Vec<String> = picks::weight_notches(tuning)
        .par_iter()
        .map(|(label, t)| {
            let (n, gaps) = weight_swap_gaps(samples, picks::growth_scores_ranked, tuning, t, VERDICT_TOP);
            if gaps.is_empty() {
                return format!("  {label:<28} no swaps (0/{n} buckets)");
            }
            let (mean, med) = cohort_stats(&gaps);
            let verdict = gap_verdict(mean, med, "RE-PROBE (entrants beat the displaced)", "holds");
            format!("  {label:<28} swapped {}/{n} buckets  mean {mean:+.1} | med {med:+.1} pts  -> {verdict}", gaps.len())
        })
        .collect();
    for row in rows {
        println!("{row}");
    }
}

/// (#890) The flag a gate-sweep row carries: TOO TIGHT only when the newly admitted cohort's mean AND median
/// are both positive, SPLIT when they disagree (tail-driven), nothing when both are non-positive.
fn sweep_tag(mean: f64, med: f64) -> &'static str {
    match (mean > 0.0, med > 0.0) {
        (true, true) => "  <- TOO TIGHT (loosen this gate)",
        (false, false) => "",
        _ => "  <- SPLIT (tail-driven, read the median)",
    }
}

/// (#10) WHICH growth gate is too tight? #9 gives the aggregate verdict; this breaks it down per gate.
/// For each numeric gate, loosen its threshold one notch (relative to the loaded tuning, so a settings.yaml
/// override is respected) and report the mean forward peer-relative return of the names that loosening
/// NEWLY admits. A POSITIVE mean = that gate was discarding winners -> loosen it in settings.yaml and
/// re-validate (the lane OOS + #9's aggregate must still hold); ≤0 = the gate is correctly keeping junk
/// out, leave it. Pure measurement, no ranking change; reuses the ablation `Knob` pattern + `growth_score`.
#[mutants::skip] // printing only; `sweep_tag` carries the test
fn gate_sweep(samples: &[Sample], tuning: &BuyHeuristic, gates: &[Knob], accepted_med: Option<f64>) {
    println!("\n── GATE SWEEP (loosen each gate one notch -> fwd return of the names it NEWLY admits) ──");
    println!("  positive = the gate was too tight (newly-admitted beat the field); ≤0 = it's keeping junk out.");
    println!("  the TOO TIGHT flag needs mean AND median positive — one survivor can carry a mean on its own.");
    // (#151) THE ABSOLUTE BAR IS THE WRONG BAR ON ITS OWN, and reading it alone is what refused four
    // pool rounds for free. `fwd peer-relative` runs a strongly negative MEDIAN for every cohort
    // measured, the SHIPPED one included: on `12y universe fund pit` the GATE AUDIT above prints the
    // accepted names at mean +46.3 | med -73.2 (n=249). Mean far above median is a right-skewed
    // forward distribution, which is the likely reason the peer subtraction lands where it does; the
    // NUMBERS are what matter here, not the mechanism. So "median > 0" asks a newly-admitted cohort to
    // clear a bar the shipped gates do not clear either. The question a POOL round actually needs is
    // comparative: is what this gate newly admits WORSE THAN WHAT IS ALREADY IN THE BOOK? That is
    // `vs shipped` below, and it is the only column of the two that can justify widening the pool.
    if let Some(a) = accepted_med {
        println!("  vs shipped = newly-admitted median MINUS the accepted cohort's median ({a:+.1}); >0 = no worse than what the gates already keep.");
    }
    // Each row re-scores EVERY sample twice (once at the shipped tuning, once loosened), so at wide-run
    // sample counts a gate costs ~24 ms: 237 ms for the ten `gate_notches` held when this was
    // measured (fourteen now), the largest single stage in a wide run after the cache load and the
    // walk itself. They are also completely independent: `newly_admitted_stats` is a pure
    // filter+mean+median over a borrowed slice, with no shared state and no PRNG, so nothing here is
    // order-sensitive the way the bootstrap stages are.
    // Format into rows and print after: `par_iter().collect()` preserves input order, so the rows land
    // in the gate order they always did — printing from inside the workers is what would scramble them.
    // (The base scoring is still repeated per gate; hoisting it would save about half again, and is the
    // next thing to try if this stage ever matters.)
    let rows: Vec<String> = gates
        .par_iter()
        .map(|(name, loosen)| {
            let mut t = tuning.clone();
            loosen(&mut t);
            match newly_admitted_stats(samples, growth_score, tuning, &t) {
                Some((n, mean, med)) => {
                    let tag = sweep_tag(mean, med);
                    // (#151) the comparative column. Absent when the audit could not run, never
                    // faked with a 0.0 default — "no baseline" and "a baseline of zero" are the two
                    // different answers this whole receipt is about.
                    let vs = match accepted_med {
                        Some(a) => format!("  vs shipped {:+.1}{}", med - a, if med > a { "  <- ADMITS NO WORSE THAN THE BOOK" } else { "" }),
                        None => String::new(),
                    };
                    format!("  {name:<26} n={n:<4} fwd peer-relative  mean {mean:+.1} | med {med:+.1} pts{vs}{tag}")
                }
                None => format!("  {name:<26} admits 0 new names (gate not binding on this sample)"),
            }
        })
        .collect();
    for row in rows {
        println!("{row}");
    }
}

/// (#3g) The growth lane's edge/rho as ONE knob is swept across a ladder. The ablation above answers
/// "does removing this term hurt?"; `tune` answers "what is the argmax?" — across every searched dim perturbed
/// at once, so each dim's own contribution is confounded. NEITHER can tell a sharp peak from a plateau,
/// and that is the entire question when asking whether a tilt can carry more authority: a plateau says
/// the shipped value is leaving edge on the table, a peak says it IS the ceiling.
///
/// Same rows, same rule as the ablation: `scored` is fixed at the SHIPPED tuning and each ladder point
/// re-scores THOSE rows (`.unwrap_or(*v)` for anything the mutated config would gate out), so rho stays
/// comparable point-to-point. Sound here because neither swept knob touches a gate — `growth_trend_weight`
/// is a pure multiplier, and the `growth_min_cagr` floor reads the UNCAPPED `long_cagr`, so moving
/// `long_trend_cap` cannot change who is admitted either.
///
/// Two exact tie-backs, printed as a self-check: the SHIPPED row must reproduce the lane's own headline
/// rho/edge, and (weight curve only) the 0.0 row must reproduce the ablation's `growth_trend_weight`
/// line. If either disagrees this curve is measuring something else. Ships nothing — pure measurement.
fn weight_curve(
    knob_name: &str,
    samples: &[Sample],
    tuning: &BuyHeuristic,
    set: fn(&mut BuyHeuristic, f64),
    shipped: f64,
    ladder: &[f64],
    note: &str,
) {
    let scored: Vec<(&Sample, f64)> =
        samples.iter().filter_map(|s| growth_score(&s.quote, tuning).map(|v| (s, v))).collect();
    if scored.len() < 8 {
        println!("\n── {knob_name} CURVE — only {} scored windows, too few to sweep. ──", scored.len());
        return;
    }
    let rels: Vec<f64> = scored.iter().map(|(s, _)| s.relative).collect();
    let mid = scored.len() / 2;
    // (#90) same purge as `report_lane`'s OOS line. Every rung re-scores the SAME rows in the SAME
    // order (`re` is a map over `scored`), so one boundary computed once is valid for the whole ladder.
    let early = purged_cut(&scored, mid, tuning.split_purge_months, |p| p.0.date);
    let split_rho = |s: &[(&Sample, f64)]| {
        core::spearman(&s.iter().map(|x| x.1).collect::<Vec<_>>(), &s.iter().map(|x| x.0.relative).collect::<Vec<_>>())
            .map_or("n/a".to_string(), |v| format!("{v:+.2}"))
    };
    println!("\n── {knob_name} CURVE (growth lane, n={}) ──", scored.len());
    println!("  {note}");
    // One rung per core, same argument as the ablation arms in `report_lane`: every rung re-scores the
    // same fixed `scored` set against its own clone, sharing nothing. Rows are formatted in parallel
    // and printed in ladder order, so the curve reads identically at any thread count.
    let rows: Vec<String> = ladder
        .par_iter()
        .map(|&x| {
            let mut t = tuning.clone();
            set(&mut t, x);
            let re: Vec<(&Sample, f64)> =
                scored.iter().map(|(s, v)| (*s, growth_score(&s.quote, &t).unwrap_or(*v))).collect();
            let (top, bot) = edge_halves(&re);
            let rho = core::spearman(&re.iter().map(|(_, v)| *v).collect::<Vec<_>>(), &rels)
                .map_or("n/a".to_string(), |v| format!("{v:+.2}"));
            // "off", not "term off": 0 zeroes a WEIGHT's term, but on a CAP knob it removes the ceiling and
            // leaves the term running at full size. One label that is true for both kinds of knob.
            let tag = if x == shipped { "  [SHIPPED]" } else if x == 0.0 { "  [off]" } else { "" };
            format!(
                "  {x:<6.2} rho {rho}  edge {:+.1}  winsor {:+.1}  OOS {} | {}{tag}",
                top - bot,
                winsor_edge(&re),
                split_rho(&re[..early]),
                split_rho(&re[mid..])
            )
        })
        .collect();
    for r in rows {
        println!("{r}");
    }
    println!("  (flat across a range -> the tilt can carry more weight; a clear peak -> the shipped value IS the");
    println!("   ceiling. Read `winsor` beside `edge`: a rise that only shows raw is leaning on extreme rows.");
    println!("   SELF-CHECK: the SHIPPED row must reproduce the lane's headline rho/edge above.)");
}

/// Report one lane: filter the samples to the cutoffs this lane's gates admit, score them, and print
/// the peer-relative Spearman, the top/bottom-half edge, the out-of-sample (early-vs-late) split, and
/// the per-term ablation. `samples` must already be in date order (for the OOS split). Mutating a
/// score WEIGHT never changes a GATE, so the gated row set stays fixed across the ablation -> the rho
/// is comparable term-to-term.
fn report_lane(
    label: &str,
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
    knobs: &[Knob],
    years: i64,
) {
    let scored: Vec<(&Sample, f64)> =
        samples.iter().filter_map(|s| scorer(&s.quote, tuning).map(|v| (s, v))).collect();
    println!("\n── {label} ──");
    if scored.len() < 4 {
        println!("  only {} windows passed this lane's gates — too few to correlate.", scored.len());
        return;
    }
    let sc: Vec<f64> = scored.iter().map(|(_, v)| *v).collect();
    let rels: Vec<f64> = scored.iter().map(|(s, _)| s.relative).collect();
    let rho = core::spearman(&sc, &rels);
    println!("  windows scored: {}", scored.len());
    match rho {
        Some(v) => println!("  Spearman(score, peer-relative): {v:+.2}   [+1 winners-first, 0 none, − backwards]"),
        None => println!("  Spearman: n/a"),
    }

    // top vs bottom scored half, by peer-relative realized. `edge_halves` is reused by the ablation
    // below (and by `tune`) so it reports the Δ of the PROFIT metric, not just rho — rho and edge can
    // disagree (a term can read mildly rho-harmful yet be load-bearing for the actual top/bottom spread).
    let (top, bot) = edge_halves(&scored);
    let base_edge = top - bot;
    println!(
        "  top-half peer-relative {top:+.1} pts  vs  bottom-half {bot:+.1} pts  ->  edge {base_edge:+.1} pts{}",
        annualized_edge_tag(tuning.print_edge_annualized, top, bot, years)
    );
    // (Item 5) bootstrap band: is that point edge distinguishable from 0 given overlapping-sample noise?
    let block = bootstrap_block(years, tuning);
    if let Some((lo, hi)) = bootstrap_edge_ci(samples, scorer, tuning, 1000, 5.0, 95.0, block) {
        let verdict = if lo > 0.0 {
            "clears 0 -> real"
        } else if hi < 0.0 {
            "below 0 -> backwards"
        } else {
            "STRADDLES 0 -> noise"
        };
        println!("  90% bootstrap band on edge: [{lo:+.1} … {hi:+.1}] pts  ({verdict})");
    } else {
        // (#119) REFUSING IS THE ANSWER. The alternative is a band drawn at a block far below the
        // dependence length, which is the i.i.d. width wearing a block bootstrap's name — narrow,
        // confident and wrong. Printing why, rather than printing nothing, keeps the missing band from
        // reading as a missing feature.
        println!(
            "  90% bootstrap band on edge: NONE — the honest block at this {years}y hold is {block} ~6mo \
             buckets and this record holds under {MIN_BLOCKS} of them; a block that big stops varying, \
             so there is no band to read"
        );
    }
    // (Item 9) net of cost: a high-turnover edge can be NET-negative once you pay the spread to chase it
    // each rebalance. cost(pts) = turnover_frac × ROUND_TRIP_BPS / 100 (1 pt = 100 bps).
    let turn = turnover_frac(&scored);
    let net = base_edge - cost_pts(turn, years, tuning.cost_per_rebalance);
    let tag = if net <= 0.0 { "  <- NET ≤ 0: too churny to trade" } else { "" };
    println!(
        "  net of cost: edge {base_edge:+.1} − turnover {:.0}% × {ROUND_TRIP_BPS:.0}bps{} = net {net:+.1} pts{tag}",
        turn * 100.0,
        rebalance_tag(years, tuning.cost_per_rebalance)
    );
    // (Item 12) is that edge a broad spread or one lucky name? Winsorize the tails and re-read it.
    let wedge = winsor_edge(&scored);
    let wtag = if wedge <= 0.0 && base_edge > 0.0 { "  <- raw edge is an OUTLIER ARTIFACT (leans on extreme rows)" } else { "" };
    println!("  winsorized edge (1/99 clamp): {wedge:+.1} pts{wtag}");
    // (#104) the recall half of the picture — see `recall_capture`. Every line above this one is a
    // precision metric; these two ask what the ranking MISSED, against the full pool the gates cut
    // from. Printed only behind the knob, so the goldens keep the report they were blessed on.
    if tuning.print_recall_capture {
        println!("  recall of the pool's extreme winners (what the ranking MISSED):");
        // (#120) ONE row, not two: this printed the verdict basket beside the comparison book, and
        // those are now the same number, so the pair was the same line twice.
        for (n, label) in [(VERDICT_TOP, "held book")] {
            for (frac, floor, cut) in [(0.01, 0.0, "top 1%"), (1.0, 10.0, "≥10×")] {
                match recall_capture(samples, &scored, n, frac, floor) {
                    Some((recall, capture, w)) => println!(
                        "    {label:<13} vs {cut:<7} winners (n={w:<5}): recall {:.0}%   capture {:.0}%",
                        recall * 100.0,
                        capture * 100.0
                    ),
                    None => println!("    {label:<13} vs {cut:<7} winners: none in the pool at this horizon"),
                }
            }
        }
        // (#127) …and WHY each miss was missed. OUTSIDE the loop above on purpose: that loop shadows
        // `label` with "held book", so the lane guard has to read the lane's own label from here.
        // GROWTH only — `gate_failures` reports the GROWTH gates, so running this beside `buy_score`
        // would blame one lane's misses on the other lane's gates.
        if label.starts_with("GROWTH") {
            for (frac, floor, cut) in [(0.01, 0.0, "top 1%"), (1.0, 10.0, "≥10×")] {
                let Some(t) = missed_winner_reasons(samples, &scored, tuning, VERDICT_TOP, frac, floor)
                else {
                    continue;
                };
                let (rows, w_n, w_mult) = (&t.rows, t.w_n, t.w_mult);
                let missed_n: usize = rows.iter().map(|r| r.winners).sum();
                let missed_mult: f64 = rows.iter().map(|r| r.mult).sum();
                // TIE-BACK, and it is not decoration: this % must equal 100 − the capture printed
                // above for the same cut (±1 from rounding). If it does not, the two instruments
                // disagree about the winner set and this whole table is void.
                println!(
                    "    why the {cut} misses were missed ({missed_n} of {w_n} winners = {:.0}% of their wealth; capture above is the rest):",
                    missed_mult / w_mult * 100.0
                );
                for r in rows {
                    println!(
                        "      {:<24} {:>4} winners   {:>5.1}% of the missed wealth   sole blocker: {:>4} ({:>4.1}%)",
                        r.reason,
                        r.winners,
                        r.mult / missed_mult * 100.0,
                        r.sole,
                        r.sole_mult / missed_mult * 100.0
                    );
                }
                // The ceiling on every single-knob loosening, in one number. `range` can own half the
                // missed wealth and still be worth almost nothing to move if it is hardly ever the
                // ONLY thing in the way — which is exactly what `growth_min_range_pct -10` admitting
                // n=1 on the gate sweep has been saying all along.
                let sole_mult: f64 = rows.iter().map(|r| r.sole_mult).sum();
                println!(
                    "      -> at most {:.1}% of the missed wealth is reachable by loosening ONE gate; the rest fails 2+ gates at once",
                    sole_mult / missed_mult * 100.0
                );
                // (#128) …so WHICH two. Only winners failing exactly two gates appear here, and the
                // near-miss column is the part of each pair that sits inside the gates' own slack —
                // the only part a loosening anyone would ship actually admits.
                for pr in t.pairs.iter().take(6) {
                    println!(
                        "      {:<11} + {:<11} {:>4} winners   {:>5.1}% of the missed wealth   both near-miss: {:>4} ({:>4.1}%)",
                        pr.pair.0,
                        pr.pair.1,
                        pr.winners,
                        pr.mult / missed_mult * 100.0,
                        pr.near,
                        pr.near_mult / missed_mult * 100.0
                    );
                }
                // The histogram is what says whether the pair table is worth reading at all: if the
                // wealth sits at 3+ gates, no two-knob arm reaches it. Gate-assessable winners only —
                // `unassessable` has no gate list, so these shares do not sum to 100.
                let hist: String =
                    t.by_count.iter().map(|(g, _, m)| format!("{g}:{:.0}% ", m / missed_mult * 100.0)).collect();
                let pair_mult: f64 = t.pairs.iter().map(|pr| pr.mult).sum();
                println!(
                    "      -> exactly-2-gate winners hold {:.1}% of the missed wealth; by gate count (0 = out-ranked, unassessable excluded): {}",
                    pair_mult / missed_mult * 100.0,
                    hist.trim_end()
                );
            }
        }
    }
    // (#159) the precision half nothing else prints: not what the ranking missed, but what it BOUGHT
    // AND LOST. GROWTH only, for the same reason `missed_winner_reasons` is — these factors are the
    // growth lane's, and running the split beside `buy_score` would attribute one lane's losers to
    // the other lane's inputs. Knob-gated, so the goldens keep the report they were blessed on.
    if tuning.print_held_loser_factors && label.starts_with("GROWTH") {
        let rows = held_loser_factors(&scored, VERDICT_TOP, &FUND_FACTORS);
        println!("  held book's LOSERS by factor (what we BOUGHT and lost, median per side):");
        if rows.is_empty() {
            println!("    no factor fills both sides — no as-of fundamentals, or the book never lost money");
        }
        for &HeldLoserRow { factor: name, n_win: nw, n_lose: nl, med_win: mw, med_lose: ml, auc: sep } in &rows {
            // The gap is computed here rather than left to the reader, but it is expressed in THIS
            // factor's units and so is only comparable to ITSELF across horizons. (#165) the AUC
            // beside it is the cross-factor reading: unit-free, 0.5 = no separation, below a half =
            // the LOSERS score higher. Rank the factors on |AUC - 0.5|, never on the gap column.
            println!(
                "    {name:<21} winners n={nw:<4} med {mw:>9.2}   losers n={nl:<4} med {ml:>9.2}   gap {:>9.2}   AUC {sep:>5.2}",
                mw - ml
            );
        }
        println!(
            "    ({} of {} factors filled both sides; a factor absent here is uncovered in this pool, not disproven.",
            rows.len(),
            FUND_FACTORS.len()
        );
        println!("     READ THE GAP, NOT THE LEVEL: a gate can only act on a factor whose two cohorts differ.");
        println!(
            "     (#165) COMPARE FACTORS ON AUC, NOT ON GAP — the gap carries each factor's own units, so ranking"
        );
        println!("     by it ranks by scale. 0.5 = no separation; < 0.5 = the losers score higher.)");
    }
    // (#111) (round 3 §8) the base rate behind the whole lane: of the names that HAD compounded at the
    // bar, what fraction did it again over the next window — against the fraction of everyone who did.
    // The conditional number alone is a rhetorical device; the pair is a finding. Knob-gated, so the
    // goldens keep the report they were blessed on.
    if tuning.print_base_rates {
        let bar = tuning.growth_min_cagr;
        match persistence_base_rate(samples, tuning, years, bar) {
            Some((q, kept, everyone)) => {
                let rate = kept as f64 / q as f64 * 100.0;
                let read = if rate > everyone {
                    "the record CARRIES information"
                } else {
                    "the record carries NOTHING — a trailing bar is not a forecast here"
                };
                println!(
                    "  base rate at the {bar:.0}%/yr bar: {kept}/{q} ({rate:.0}%) of names that had cleared it \
                     cleared it again over {years}y, vs {everyone:.0}% of the whole pool -> {read}"
                );
            }
            None => println!("  base rate at the {bar:.0}%/yr bar: no sample clears it — nothing to condition on"),
        }
    }
    // (#57) NULL MODEL: the same code with the tuning switched off — `BuyHeuristic::default()`, the
    // deliberately-neutral code defaults (growth_min_cagr 8.0, every PEG/maxdd/vol cap 0.0 = off).
    // The raw edge above cannot be asserted against a fixed threshold because it moves with the market;
    // this DELTA can, because both arms score the SAME samples over the SAME window, so regime drift
    // cancels out of the difference. A shipped tuning that ranks no better than no tuning is a
    // regression the nightly gate's `edge > 0` collapse check reads as perfectly green.
    // Note this is a fair fight, not the ablation: the null arm re-applies its OWN gates (looser, so
    // more rows), because the gates ARE part of the tuning being graded.
    let (null_rho, null_edge) = lane_metrics(samples, scorer, &BuyHeuristic::default());
    let lift = base_edge - null_edge;
    let ltag = if lift <= 0.0 { "  <- the tuning is NOT EARNING ITS KEEP" } else { "" };
    println!(
        "  vs null model (tuning off): rho {}  edge {null_edge:+.1} pts  ->  tuning adds {lift:+.1} pts{ltag}",
        null_rho.map_or("n/a".to_string(), |v| format!("{v:+.2}"))
    );

    // out-of-sample early vs late (scored is date-ordered)
    let mid = scored.len() / 2;
    // (#90) purge the early half back off the boundary — these two rho's are read as independent
    // evidence ("both halves positive"), and without a gap the rows either side of `mid` share almost
    // all of one forward path. 0 (the default) leaves `early == mid` and the line byte-identical.
    let early = purged_cut(&scored, mid, tuning.split_purge_months, |p| p.0.date);
    let split_rho = |s: &[(&Sample, f64)]| {
        core::spearman(
            &s.iter().map(|x| x.1).collect::<Vec<_>>(),
            &s.iter().map(|x| x.0.relative).collect::<Vec<_>>(),
        )
        .map_or("n/a".to_string(), |v| format!("{v:+.2}"))
    };
    println!(
        "  out-of-sample (split {}): early rho {}  |  late rho {}",
        scored[mid].0.date,
        split_rho(&scored[..early]),
        split_rho(&scored[mid..])
    );
    // (#565) edge, null-model lift and OOS halves again over the windows that clear every gate: the crowd the
    // lines above graded before `growth_gate_soft`. Armed, docked failers join `scored` (stress 20y: 370 -> 4922
    // windows), so those lines grade a different crowd; a passer's own score is bit-identical either way. CI
    // grades this line.
    let hard = BuyHeuristic { growth_gate_soft: 0.0, ..tuning.clone() };
    let passers: Vec<(&Sample, f64)> = samples.iter().filter_map(|s| scorer(&s.quote, &hard).map(|v| (s, v))).collect();
    let (p_top, p_bot) = edge_halves(&passers);
    let p_mid = passers.len() / 2;
    let p_early = purged_cut(&passers, p_mid, tuning.split_purge_months, |p| p.0.date);
    println!(
        "  gate passers only  ->  passer edge {:+.1} pts  ->  passer lift {:+.1} pts  |  passer OOS early {}  |  passer OOS late {}",
        p_top - p_bot,
        p_top - p_bot - null_edge,
        split_rho(&passers[..p_early]),
        split_rho(&passers[p_mid..])
    );

    // (Item 30) regime slices: the edge within each chronological quarter of the scored sample.
    // Guards against "the whole edge is one bull regime" — a real selection signal should show up
    // (not necessarily equally) across eras. Returns are already peer-relative per ~6mo bucket, so
    // each era's edge reads selection within that era, not the era's beta.
    println!("  edge by era (chronological quarters):");
    for q in 0..4 {
        let (lo, hi) = (scored.len() * q / 4, scored.len() * (q + 1) / 4);
        let era = &scored[lo..hi];
        if era.len() < 4 {
            continue;
        }
        let (t, b) = edge_halves(era);
        println!("    {} .. {}  n={:<6} edge {:+.1} pts", era[0].0.date, era[era.len() - 1].0.date, era.len(), t - b);
    }

    // ablation: zero each knob, re-score the SAME gated rows, recompute BOTH metrics. Δrho = rank
    // selection change; Δedge = profit-spread change (the one that matters). +Δ ⇒ the knob HURT,
    // −Δ ⇒ it HELPED. Watch for sign disagreement: a knob that's −Δedge (load-bearing for profit)
    // but ~0/+Δrho is a trap — don't delete it on the rho reading alone.
    let base_rho = rho.unwrap_or(0.0);
    println!("  ablation (Δ vs full: rho {base_rho:+.2}, edge {base_edge:+.1}):");
    // One arm per core. Each arm re-scores the same fixed `scored` rows against its own clone of the
    // tuning and touches nothing shared — the comment above already says so ("re-score the SAME gated
    // rows"), which is exactly the property that makes this safe. Formatted into strings and printed
    // afterwards in `knobs` order, so the table reads identically at any thread count.
    let rows: Vec<String> = knobs
        .par_iter()
        .map(|(name, mutate)| {
            let mut t2 = tuning.clone();
            mutate(&mut t2);
            let abl: Vec<(&Sample, f64)> =
                scored.iter().map(|(s, v)| (*s, scorer(&s.quote, &t2).unwrap_or(*v))).collect();
            let (et, eb) = edge_halves(&abl);
            let dedge = (et - eb) - base_edge;
            match core::spearman(&abl.iter().map(|(_, v)| *v).collect::<Vec<_>>(), &rels) {
                Some(v) => format!("    {:<20} rho {v:+.2} Δ{:+.2}   edge {:+.1} Δ{dedge:+.1}", name, v - base_rho, et - eb),
                None => format!("    {:<20} rho n/a   edge {:+.1} Δ{dedge:+.1}", name, et - eb),
            }
        })
        .collect();
    for r in rows {
        println!("{r}");
    }
}

/// (Item 31) Split each ticker's consecutive-cutoff pairs where the EARLIER cutoff passed the lane's
/// gates: did the later cutoff keep passing, or newly fail? Returns the two cohorts' forward
/// peer-relative returns (taken AT the later cutoff = the moment a holder would see the flip).
/// Pure so the split is unit-testable; consecutive cutoffs are ~6mo apart by construction.
fn exit_cohorts(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> (Vec<f64>, Vec<f64>) {
    // BTreeMap, not HashMap: the two Vecs below are filled by ITERATING this map, so its per-process
    // random order would be baked into their element order. Same family as the `bootstrap_edge_ci` bug
    // (see the comment there) — found by auditing for it after that one, not by a failing test, because
    // the consumers here take a mean/median and float addition is only ALMOST associative: the wobble
    // sits below the printed decimal today. That makes it invisible, not absent, and it would surface
    // the moment a consumer starts caring about order. Cheap to just not have.
    let mut by_ticker: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        by_ticker.entry(s.quote.ticker.as_str()).or_default().push(s);
    }
    let (mut kept, mut failed) = (Vec::new(), Vec::new());
    for series in by_ticker.values_mut() {
        series.sort_by_key(|s| s.date);
        for w in series.windows(2) {
            if scorer(&w[0].quote, tuning).is_some() {
                if scorer(&w[1].quote, tuning).is_some() {
                    kept.push(w[1].relative);
                } else {
                    failed.push(w[1].relative);
                }
            }
        }
    }
    (kept, failed)
}

/// (Item 31) The buy-and-hold plan's missing half: after a name is BOUGHT (passed the growth gates),
/// is a later gate failure a sell signal or a wobble to hold through? Compares the forward
/// peer-relative return of names that newly FAILED a gate vs names that kept passing, both measured
/// from the flip cutoff. A strongly negative gap = the gate review in `check` is an evidence-backed
/// exit trigger; a flat gap = hold through it. Reports mean AND median for the same reason `gate_audit`
/// does, and matters more here: this verdict decides whether to SELL a held position, so a gap that
/// exists only in the tail must not read as a sell signal. Returns `(mean gap, median gap)` for the test.
fn exit_probe(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> Option<(f64, f64)> {
    let (kept, failed) = exit_cohorts(samples, scorer, tuning);
    println!("\n── EXIT PROBE (growth lane: passed gates ~6mo ago -> what next?) ──");
    if kept.len() < 4 || failed.len() < 4 {
        println!("  too few flips to read (kept {} / newly-failed {}).", kept.len(), failed.len());
        return None;
    }
    let ((mk, medk), (mf, medf)) = (cohort_stats(&kept), cohort_stats(&failed));
    let (gap, gap_med) = (mf - mk, medf - medk);
    println!("  kept passing   n={:<6} fwd peer-relative  mean {mk:+.1} | med {medk:+.1} pts", kept.len());
    println!("  newly FAILED   n={:<6} fwd peer-relative  mean {mf:+.1} | med {medf:+.1} pts", failed.len());
    // Sign convention is inverted vs `gate_audit`: here a NEGATIVE gap is the actionable one (the
    // names that failed went on to do worse), so the two verdict strings swap places.
    let verdict = gap_verdict(
        gap,
        gap_med,
        "gate failure is NOT a sell signal — the newly-failed did as well or better; hold through it",
        "gate failure is a SELL signal — the newly-failed went on to underperform the names that held",
    );
    println!("  gap  mean {gap:+.1} | med {gap_med:+.1} pts  ->  {verdict}");
    Some((gap, gap_med))
}

/// (#820) The entry-side mirror of `exit_cohorts`: consecutive-cutoff pairs where the EARLIER cutoff
/// FAILED the lane's gates and the later one passes. Returns the forward peer-relative returns taken at
/// the later cutoff, the moment the name first shows as a buy. The pass->pass half is `exit_cohorts`'s
/// `kept`, so it is not built twice.
fn fresh_entries(samples: &[Sample], scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>, tuning: &BuyHeuristic) -> Vec<f64> {
    // BTreeMap for the same reason as `exit_cohorts`: the output order follows the map's.
    let mut by_ticker: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        by_ticker.entry(s.quote.ticker.as_str()).or_default().push(s);
    }
    let mut fresh = Vec::new();
    for series in by_ticker.values_mut() {
        series.sort_by_key(|s| s.date);
        for w in series.windows(2) {
            if scorer(&w[0].quote, tuning).is_none() && scorer(&w[1].quote, tuning).is_some() {
                fresh.push(w[1].relative);
            }
        }
    }
    fresh
}

/// (#820) Sweep 40's falsifier: should a buy wait for a second passing cutoff? Compares the forward
/// peer-relative return of names that pass NOW after failing ~6mo ago (newly passing) against names
/// that passed both times (confirmed). A negative gap on both stats = fresh passers lag, so an
/// entry-confirmation rule earns a graded knob; otherwise buy on the first pass. Report only, no knob.
/// Returns `(mean gap, median gap)`, fresh minus confirmed, for the test.
fn entry_probe(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> Option<(f64, f64)> {
    let (confirmed, _) = exit_cohorts(samples, scorer, tuning);
    let fresh = fresh_entries(samples, scorer, tuning);
    println!("\n── ENTRY PROBE (growth lane: passes now -> did it pass ~6mo ago too?) ──");
    if confirmed.len() < 4 || fresh.len() < 4 {
        println!("  too few entries to read (confirmed {} / newly-passing {}).", confirmed.len(), fresh.len());
        return None;
    }
    let ((mc, medc), (mf, medf)) = (cohort_stats(&confirmed), cohort_stats(&fresh));
    let (gap, gap_med) = (mf - mc, medf - medc);
    println!("  confirmed      n={:<6} fwd peer-relative  mean {mc:+.1} | med {medc:+.1} pts", confirmed.len());
    println!("  newly PASSING  n={:<6} fwd peer-relative  mean {mf:+.1} | med {medf:+.1} pts", fresh.len());
    let verdict = gap_verdict(
        gap,
        gap_med,
        "entry confirmation would NOT pay — fresh passers did as well or better; buy on the first pass",
        "fresh passers LAG the confirmed ones — an entry-confirmation rule is worth a graded knob",
    );
    println!("  gap  mean {gap:+.1} | med {gap_med:+.1} pts  ->  {verdict}");
    Some((gap, gap_med))
}

/// (#821) The rank form of (#820)'s split. Per bucket, [`blend_book`] over the non-crypto samples, each ticker's
/// latest; each held name is filed by whether the previous bucket's book held it too (confirmed) or not (new to
/// the book). Returns the two sides' forward peer-relative returns, taken at the later cutoff. A bucket with no
/// previous bucket in the walk is skipped: there was no earlier book to wait for.
fn book_entries(
    samples: &[Sample],
    scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>,
    tuning: &BuyHeuristic,
    open: &BuyHeuristic,
) -> (Vec<f64>, Vec<f64>) {
    let mut groups: BTreeMap<i32, BTreeMap<&str, &Sample>> = BTreeMap::new();
    for s in samples.iter().filter(|s| picks::asset_class(&s.quote) != 0) {
        groups.entry(bucket(s.date)).or_default().insert(s.quote.ticker.as_str(), s);
    }
    let (mut confirmed, mut fresh, mut held) = (Vec::new(), Vec::new(), BTreeMap::<i32, HashSet<&str>>::new());
    for (&b, by_tk) in &groups {
        let book = blend_book(by_tk, scorer, tuning, open);
        if let Some(before) = held.get(&(b - 1)) {
            for (t, _) in &book {
                let side = if before.contains(t) { &mut confirmed } else { &mut fresh };
                side.push(by_tk[t].relative);
            }
        }
        held.insert(b, book.into_iter().map(|x| x.0).collect());
    }
    (confirmed, fresh)
}

/// (#821) Sweep 40's idea as it was phrased: should a buy wait until the book held the name at the previous
/// cutoff too? Compares the forward peer-relative return of names new to the published blend book against
/// names it held both times ([`book_entries`]). A negative gap on both stats = new entrants lag, so a
/// wait-one-cutoff rule earns a graded knob; otherwise buy on entry. Report only, no knob. Returns
/// `(mean gap, median gap)`, new minus held, for the test.
fn book_entry_probe(
    samples: &[Sample],
    scorer: fn(&[&Quote], &BuyHeuristic) -> Vec<Option<f64>>,
    tuning: &BuyHeuristic,
    open: &BuyHeuristic,
) -> Option<(f64, f64)> {
    let (confirmed, fresh) = book_entries(samples, scorer, tuning, open);
    println!("\n── BOOK ENTRY PROBE (#821) (blend book: held now -> was it in the book ~6mo ago too?) ──");
    if confirmed.len() < 4 || fresh.len() < 4 {
        println!("  too few entries to read (held both times {} / new to the book {}).", confirmed.len(), fresh.len());
        return None;
    }
    let ((mc, medc), (mf, medf)) = (cohort_stats(&confirmed), cohort_stats(&fresh));
    let (gap, gap_med) = (mf - mc, medf - medc);
    println!("  held both times  n={:<6} fwd peer-relative  mean {mc:+.1} | med {medc:+.1} pts", confirmed.len());
    println!("  new to the book  n={:<6} fwd peer-relative  mean {mf:+.1} | med {medf:+.1} pts", fresh.len());
    let verdict = gap_verdict(
        gap,
        gap_med,
        "waiting a cutoff would NOT pay — new entrants did as well or better; buy on entry",
        "new entrants LAG the names held twice — a wait-one-cutoff rule is worth a graded knob",
    );
    println!("  gap  mean {gap:+.1} | med {gap_med:+.1} pts  ->  {verdict}");
    Some((gap, gap_med))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// The pre-(#790) daily stride, ~6 months of sessions. Only the replicas below still walk it: their
    /// pinned numbers were measured on that walk.
    const STEP_SESSIONS: usize = 126;

    /// (#558) rank r of k weighs (k+1-r)/(k(k+1)/2): of two names the first counts twice the second, a
    /// reorder moves the multiple, and a full ten weighs 10..1 over 55 (sum of i*(11-i) is 220).
    #[test]
    fn ranked_multiple_weights_rank_linearly() {
        assert!((ranked_multiple(&[3.0, 1.0]) - 7.0 / 3.0).abs() < 1e-12);
        assert!((ranked_multiple(&[1.0, 3.0]) - 5.0 / 3.0).abs() < 1e-12);
        assert_eq!(ranked_multiple(&[2.0]), 2.0);
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert!((ranked_multiple(&ten) - 4.0).abs() < 1e-12);
    }

    /// (#531) twelve monthly bars of 10 shares over 120 shares outstanding is one full turn a year. The
    /// bar dated exactly a year back sits outside the window; a record that starts inside the year, a
    /// bar with no volume, or no share count reads None.
    #[test]
    fn low_turnover_sums_the_trailing_year_of_own_volume() {
        let m = |y, mo| NaiveDate::from_ymd_opt(y, mo, 1).unwrap();
        let own: Vec<_> = (0..13).map(|k| (m(2023 + k / 12, (k % 12) as u32 + 1), 10.0)).collect();
        let at = m(2024, 1);
        assert_eq!(low_turnover(&own, at, Some(120.0)), Some(-1.0));
        assert_eq!(low_turnover(&own, at, Some(60.0)), Some(-2.0));
        assert_eq!(low_turnover(&own[1..], at, Some(120.0)), None); // starts inside the year
        assert_eq!(low_turnover(&own, at, None), None);
        assert_eq!(low_turnover(&own, at, Some(0.0)), None);
        let mut gap = own.clone();
        gap[6].1 = 0.0;
        assert_eq!(low_turnover(&gap, at, Some(120.0)), None);
        gap[6].1 = 10.0;
        gap[0].1 = 0.0; // the excluded bar a year back does not count
        assert_eq!(low_turnover(&gap, at, Some(120.0)), Some(-1.0));
    }

    /// (#790) Each cutoff lands on the first bar of its half-year bucket. The monthly record runs
    /// 2000-03 to 2002-02, so bar 0 opens the first bucket and bars 4 (2000-07), 10 (2001-01), 16 and
    /// 22 open the rest. A start inside a bucket waits for the next opening, and a start in the last
    /// bucket or past the end yields nothing. A daily record opens a bucket on its first session of
    /// January, and a bucket with no bars at all is skipped.
    #[test]
    fn cutoffs_land_on_the_first_bar_of_each_half_year() {
        fn at(d: &[NaiveDate], from: usize) -> Vec<usize> {
            cutoffs(d, from).collect()
        }
        let monthly = month_ends(2000, 3, 24);
        assert_eq!(at(&monthly, 0), [0, 4, 10, 16, 22]);
        assert_eq!(at(&monthly, 4), [4, 10, 16, 22]);
        assert_eq!(at(&monthly, 5), [10, 16, 22]);
        assert!(at(&monthly, 23).is_empty() && at(&monthly, 99).is_empty() && at(&[], 3).is_empty());
        let daily = [ymd(2003, 12, 30), ymd(2003, 12, 31), ymd(2004, 1, 2), ymd(2004, 1, 5)];
        assert_eq!(at(&daily, 1), [2]);
        let gap = [ymd(2005, 3, 1), ymd(2005, 5, 1), ymd(2006, 2, 1), ymd(2006, 3, 1)];
        assert_eq!(at(&gap, 1), [2]);
    }

    /// `n` bars, one per month, on the 1st — the shape a real `interval=1mo` payload carries.
    fn month_ends(year: i32, month: u32, n: usize) -> Vec<NaiveDate> {
        (0..n)
            .map(|k| {
                let m = month as usize - 1 + k;
                ymd(year + (m / 12) as i32, (m % 12) as u32 + 1, 1)
            })
            .collect()
    }

    /// (#116) STRUCTURAL PIN, and the regression it exists for was a RED CI GATE, not a hypothetical.
    /// `run` used to hand the venue setting (`prefer_eu` + `_listing`) to `fetch_universe`, so the fixture
    /// this walk is graded on decided which VENUE each constituent was priced from. Flipping that knob
    /// to `true` swapped 266 of 519 S&P names onto Xetra twins that are a MEDIAN 18.6 YEARS YOUNGER
    /// than their US primaries — which, because a cutoff needs history behind it PLUS the forward
    /// window, drains the 20y lane of them entirely and fills the 8y lane with them. The gate read that
    /// as a scoring collapse (8y edge -22.8 against a healthy +6.4 top-3 excess three days earlier) and
    /// no line of the diff that caused it went near a score.
    ///
    /// So the venue argument is a LITERAL and this counts it. The knob still exists and the live lanes
    /// still read it; what must never come back is this file reading it.
    ///
    /// The needle is assembled with `concat!` ON PURPOSE: written as one literal it would occur in this
    /// test's own source and count itself. For the same reason no comment or message anywhere in this
    /// file may write the setting's name contiguously — spell it with the underscore split, as the
    /// assert message below does.
    #[test]
    fn the_backtest_never_reads_the_venue_knob() {
        let src = include_str!("backtest.rs");
        let reads = src.matches(concat!("prefer_eu", "_listing")).count();
        assert_eq!(
            reads, 0,
            "the walk grades the DEEPEST series a name has, never the venue a live row is bought on; \
             found {reads} mentions of the venue knob in this file. Pass the literal `false` to \
             `fetch_universe` — a Xetra twin is a median 18.6y younger, so reading the setting here \
             re-weights the graded sample by horizon and reds this gate at 8y."
        );
    }

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }
    fn sample(date: NaiveDate, realized: f64) -> Sample {
        Sample { date, realized, relative: 0.0, quote: Arc::new(Quote::stub("X", "1", "", "X")), fund: None, trail: Vec::new() }
    }

    /// (#334) A name whose test score is `a + quality_weight × b` (`swap_scorer`), so a `quality_weight`
    /// notch reorders exactly the names carrying `b > 0`.
    fn swap_sample(date: NaiveDate, ticker: &str, a: f64, b: f64, realized: f64) -> Sample {
        let mut q = Quote::stub(ticker, "€1.00", "", ticker);
        q.range_pct = a;
        q.avg_turnover_eur = Some(b);
        Sample { date, realized, relative: 0.0, quote: Arc::new(q), fund: None, trail: Vec::new() }
    }
    fn swap_scorer(pool: &[&Quote], t: &BuyHeuristic) -> Vec<Option<f64>> {
        pool.iter().map(|q| Some(q.range_pct + t.quality_weight * q.avg_turnover_eur.unwrap_or(0.0)).filter(|s| *s > 0.0)).collect()
    }

    /// (#334) Entrants minus displaced, per bucket, pure swaps only. Two swapping buckets with different
    /// gaps (one 1-for-1, one 2-for-2 so the per-side MEAN is exercised), a coin that outscores everyone and
    /// must not count, a tie the notch leaves alone, and a notch that grows a one-name pool, which is not a
    /// swap and must not grade.
    #[test]
    fn weight_swap_gaps_price_entrants_against_the_displaced() {
        let (a, b, c, d) = (ymd(2010, 1, 5), ymd(2011, 1, 5), ymd(2012, 1, 5), ymd(2013, 1, 5));
        let samples = vec![
            swap_sample(a, "A1", 10.0, 0.0, 5.0),
            swap_sample(a, "A2", 9.0, 0.0, 1.0),
            swap_sample(a, "A3", 8.0, 5.0, 21.0), // 8 -> 13: in for A2, 21 - 1 = +20
            swap_sample(a, "BTC-EUR", 100.0, 100.0, 1000.0),
            swap_sample(b, "B1", 10.0, 0.0, 0.0),
            swap_sample(b, "B2", 9.0, 0.0, 10.0),
            swap_sample(b, "B3", 8.0, 4.0, 4.0), // 8 -> 12
            swap_sample(b, "B4", 7.0, 6.0, 2.0), // 7 -> 13: both in, mean(4, 2) - mean(0, 10) = -2
            swap_sample(c, "C1", 5.0, 0.0, 0.0),
            swap_sample(c, "C2", 5.0, 0.0, 0.0),
            swap_sample(c, "C3", 5.0, 0.0, 9.0),
            swap_sample(d, "D1", 5.0, 0.0, 0.0),
            swap_sample(d, "D2", -1.0, 2.0, 50.0), // unscored -> +1: admitted, nobody displaced
        ];
        let base = BuyHeuristic { quality_weight: 0.0, ..BuyHeuristic::default() };
        let notched = BuyHeuristic { quality_weight: 1.0, ..BuyHeuristic::default() };
        assert_eq!(weight_swap_gaps(&samples, swap_scorer, &base, &notched, 2), (4, vec![20.0, -2.0]));
        assert_eq!(weight_swap_gaps(&samples, swap_scorer, &base, &base, 2), (4, vec![]), "an unmoved weight swaps nobody");
        assert_eq!(weight_swap_gaps(&samples, swap_scorer, &base, &notched, 10), (4, vec![]), "a basket holding the whole pool swaps nobody");
    }

    /// (#1239) Per bucket, oldest first: the ten best scores, the earlier name kept on a tie at the cut,
    /// with the coin, the name lacking an index leg and the unscored name all left out.
    #[test]
    fn top_books_hold_the_top_ten_per_bucket() {
        let (a, b) = (ymd(2010, 1, 5), ymd(2010, 7, 5));
        let mut samples = vec![swap_sample(b, "B1", 2.0, 0.0, 0.0), swap_sample(b, "B2", 5.0, 0.0, 0.0)];
        for (k, score) in [12.0, 11.0, 10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 3.0, 1.0].into_iter().enumerate() {
            samples.push(swap_sample(a, &format!("A{k}"), score, 0.0, 0.0)); // indices 2..=13
        }
        samples.push(swap_sample(a, "BTC-EUR", 100.0, 0.0, 0.0));
        samples.push(swap_sample(a, "NOLEG", 50.0, 0.0, 0.0));
        samples.push(swap_sample(a, "NEG", -1.0, 0.0, 0.0));
        let mut bench_r = vec![0.0; samples.len()];
        bench_r[15] = f64::NAN;
        assert_eq!(top_books(&samples, &bench_r, &BuyHeuristic::default(), swap_scorer), vec![(2..12).collect::<Vec<usize>>(), vec![1, 0]]);
    }

    /// (#1239) Bucket excesses 50 (the `book_excess` pin's book), 0 and 20 at 1y: mean 70/3, median 20. An
    /// empty book prices nothing, as the top row skips it.
    #[test]
    fn search_excess_is_the_top_rows_mean_and_median() {
        let books = vec![vec![0, 1], vec![2], vec![], vec![3]];
        let (mean, med) = search_excess(&books, &[300.0, -100.0, 10.0, 20.0], &[100.0, 0.0, 10.0, 0.0], 1);
        assert!((mean - 70.0 / 3.0).abs() < 1e-9 && (med - 20.0).abs() < 1e-9, "{mean} {med}");
    }

    /// (#1239) Each (bucket, class) cell keeps its own returns: three stocks and two ETFs in one bucket, two
    /// stocks in the next, a lone coin. Over 64 seeds the two-ETF cell must both swap and stay put, which a
    /// shuffle that always moves every name (j drawn below k) cannot do.
    #[test]
    fn shuffled_permutes_realized_within_each_bucket_and_class() {
        let (a, b) = (ymd(2010, 1, 5), ymd(2010, 7, 5));
        let etf = |realized| {
            let mut q = Quote::stub("E", "1", "", "E");
            q.instrument_type = "ETF".to_string();
            Sample { date: a, realized, relative: 0.0, quote: Arc::new(q), fund: None, trail: Vec::new() }
        };
        let samples = vec![
            swap_sample(a, "S1", 1.0, 0.0, 1.0),
            swap_sample(a, "S2", 1.0, 0.0, 2.0),
            swap_sample(a, "S3", 1.0, 0.0, 3.0),
            etf(10.0),
            etf(20.0),
            swap_sample(b, "S4", 1.0, 0.0, 100.0),
            swap_sample(b, "S5", 1.0, 0.0, 200.0),
            swap_sample(a, "BTC-EUR", 1.0, 0.0, 1000.0),
        ];
        let mut swapped = 0;
        for seed in 1..=64 {
            let out = shuffled(&samples, seed);
            let cells: [&[usize]; 4] = [&[0, 1, 2], &[3, 4], &[5, 6], &[7]];
            for cell in cells {
                let mut got: Vec<f64> = cell.iter().map(|&i| out[i]).collect();
                got.sort_by(f64::total_cmp);
                assert_eq!(got, cell.iter().map(|&i| samples[i].realized).collect::<Vec<_>>(), "seed {seed}");
            }
            swapped += usize::from(out[3] == 20.0);
        }
        assert!(swapped > 0 && swapped < 64, "the ETF pair swapped on {swapped} of 64 seeds");
    }

    /// (#1239) A draw at u = 0, 0.5 and 0.9: weights 2.5u, caps 2^(2u - 1), notches floor(3u³) = 0, 0, 2.
    #[test]
    fn search_draw_maps_each_uniform_onto_its_range() {
        let (w, c) = (picks::WEIGHT_DIMS.len(), CAP_DIMS.len());
        for (u, weight, cap, notch) in [(0.0, 0.0, 0.5, 0.0), (0.5, 1.25, 1.0, 0.0), (0.9, 2.25, 0.8f64.exp2(), 2.0)] {
            assert_eq!(search_draw(&mut || u, 3), [vec![weight; w], vec![cap; c], vec![notch; 3]].concat(), "u = {u}");
        }
    }

    /// (#1239) Every scaled knob set to its own index + 1, then doubled; a notch drawn twice runs twice, one
    /// drawn 0 times never runs; all ones and zeros hand back the base unchanged.
    #[test]
    fn search_apply_scales_each_knob_and_runs_each_notch_its_count() {
        let n = picks::WEIGHT_DIMS.len() + CAP_DIMS.len();
        let mut base = BuyHeuristic::default();
        for (i, (_, _, set)) in picks::WEIGHT_DIMS.iter().chain(&CAP_DIMS).enumerate() {
            set(&mut base, i as f64 + 1.0);
        }
        let notches: [fn(&mut BuyHeuristic); 2] = [|t| t.growth_min_cagr -= 1.0, |t| t.growth_min_range_pct -= 1.0];
        let t = search_apply(&base, &[vec![2.0; n], vec![2.0, 0.0]].concat(), &notches);
        for (i, (label, get, _)) in picks::WEIGHT_DIMS.iter().chain(&CAP_DIMS).enumerate() {
            assert_eq!(get(&t), 2.0 * (i as f64 + 1.0), "{label}");
        }
        assert_eq!((t.growth_min_cagr, t.growth_min_range_pct), (base.growth_min_cagr - 2.0, base.growth_min_range_pct));
        let same = search_apply(&base, &[vec![1.0; n], vec![0.0, 0.0]].concat(), &notches);
        assert_eq!(serde_json::to_value(&same).unwrap(), serde_json::to_value(&base).unwrap());
    }

    /// (#1239) Gains (2, 0.5) at one horizon and (0.2, 4) at the other: the grade is the worst, 0.2, a mean
    /// gain. Gains (4, 0.2) and (2.2, 2): the worst, 0.2 again, is a median gain.
    #[test]
    fn search_objective_takes_the_worst_moment_at_the_worst_horizon() {
        let shipped = [(1.0, 0.5), (1.8, 1.0)];
        assert!((search_objective(&[(3.0, 1.0), (2.0, 5.0)], &shipped) - 0.2).abs() < 1e-9);
        assert!((search_objective(&[(5.0, 0.7), (4.0, 3.0)], &shipped) - 0.2).abs() < 1e-9, "the median gain binds");
        assert_eq!(search_objective(&shipped, &shipped), 0.0, "the shipped config gains nothing on itself");
    }

    /// (#1240) Shuffles' best gains 0.2..1.5 and nudges' p5 0.3: a gain of 1.6 beats every shuffle and its
    /// nudges, so the GRID ROW runs. 1.5 ties the null's max and 1.2 beats its p95 only: both read the
    /// floor. Nudges whose p5 is 0 or below make 1.6 a knife-edge. A null all below zero still floors a
    /// gain of 0. No shuffles grade nothing.
    #[test]
    fn search_verdict_needs_every_shuffle_beaten_and_the_nudges_held() {
        let null: Vec<f64> = (0..20).map(|k| if k == 19 { 1.5 } else { 0.2 + 0.05 * k as f64 }).collect();
        let jit: Vec<f64> = (0..20).map(|k| 0.3 + 0.1 * k as f64).collect();
        assert!(search_verdict(1.6, &null, &jit).contains("GRID ROW"));
        assert!(search_verdict(1.5, &null, &jit).contains("noise floor"), "a tie with the null's max is luck");
        assert!(search_verdict(1.2, &null, &jit).contains("noise floor"), "look 1's p95 bar no longer passes");
        let flat: Vec<f64> = (0..20).map(|k| 0.1 * (k as f64 - 1.0)).collect(); // p5 is the 2nd of 20: 0
        assert!(search_verdict(1.6, &null, &flat).contains("knife-edge"), "a JITTER p5 of exactly 0 runs nothing");
        assert!(search_verdict(1.6, &null, &[-0.4, 2.0]).contains("knife-edge"));
        assert!(search_verdict(0.0, &[-1.0, -0.5], &jit).contains("noise floor"), "no gain is never a winner");
        assert!(search_verdict(1.6, &[], &jit).contains("nulls=0"));
    }

    /// (#408) X ranks top of 3 on earnings (2/2 -> 100) and bottom of 2 on EBITDA (0/1 -> 0); FCF and
    /// sales hold one name each, so they rank nothing -> X is 50. Y: 50 and 100 -> 75. Z has one leg and
    /// W is alone at its date: both None. A reordered date stamps the same numbers. (#409) Caps X 300,
    /// Y 100, Z 200 rank 100/0/50, so `small_pct` is 0/100/50; W's lone cap ranks nothing -> None.
    #[test]
    fn stamp_cohort_ranks_averages_same_date_percentiles() {
        use crate::core::FundFactors;
        let row = |d: u32, t: &str, ey: Option<f64>, eb: Option<f64>, fcf: Option<f64>, sales: Option<f64>, mcap: f64| Sample {
            date: ymd(2010, 1, d),
            realized: 0.0,
            relative: 0.0,
            quote: Arc::new(Quote::stub(t, "1", "", t)),
            fund: Some(FundFactors {
                earnings_yield: ey,
                ebitda_yield: eb,
                fcf_ev_yield: fcf,
                sales_ev_yield: sales,
                mcap: Some(mcap),
                ..Default::default()
            }),
            trail: Vec::new(),
        };
        let x = || row(1, "X", Some(10.0), Some(20.0), Some(5.0), Some(100.0), 300.0);
        let y = || row(1, "Y", Some(5.0), Some(30.0), None, None, 100.0);
        let z = || row(1, "Z", Some(1.0), None, None, None, 200.0);
        let w = || row(2, "W", Some(9.0), Some(9.0), None, None, 50.0);
        let stamped = |mut v: Vec<Sample>| {
            stamp_cohort_ranks(&mut v);
            let mut out: Vec<(String, Option<f64>, Option<f64>)> = v
                .iter()
                .map(|s| {
                    let f = s.fund.as_ref().unwrap();
                    (s.quote.ticker.clone(), f.value_composite, f.small_pct)
                })
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        };
        let want = vec![
            ("W".into(), None, None),
            ("X".into(), Some(50.0), Some(0.0)),
            ("Y".into(), Some(75.0), Some(100.0)),
            ("Z".into(), None, Some(50.0)),
        ];
        assert_eq!(stamped(vec![x(), y(), z(), w()]), want);
        assert_eq!(stamped(vec![z(), x(), y(), w()]), want);
    }

    /// (#533) `low_dro` is demeaned within (date, sector): Tech −10/−30/−60 has median −30, so it reads
    /// +20/0/−30; Energy −40 alone reads 0; a Tech name at another date is its own cohort; no sector is None.
    #[test]
    fn stamp_cohort_ranks_demeans_low_dro_within_date_and_sector() {
        use crate::core::FundFactors;
        let row = |d: u32, t: &str, sector: Option<&str>, dro: f64| {
            let mut q = Quote::stub(t, "1", "", t);
            q.sector = sector.map(str::to_string);
            Sample {
                date: ymd(2010, 1, d),
                realized: 0.0,
                relative: 0.0,
                quote: Arc::new(q),
                fund: Some(FundFactors { low_dro: Some(dro), ..Default::default() }),
                trail: Vec::new(),
            }
        };
        let mut v = vec![
            row(1, "A", Some("Tech"), -10.0),
            row(1, "B", Some("Tech"), -30.0),
            row(1, "C", Some("Tech"), -60.0),
            row(1, "D", Some("Energy"), -40.0),
            row(1, "E", None, -5.0),
            row(2, "F", Some("Tech"), -99.0),
            row(2, "G", Some("Tech"), -1.0),
        ];
        stamp_cohort_ranks(&mut v);
        let got: Vec<Option<f64>> = v.iter().map(|s| s.fund.as_ref().unwrap().low_dro).collect();
        assert_eq!(got, vec![Some(20.0), Some(0.0), Some(-30.0), Some(0.0), None, Some(-49.0), Some(49.0)]);
    }

    /// (#328) A stock at one cutoff with a chosen 5Y leg and GICS label — the only inputs the sector
    /// door reads. `realized` is irrelevant here; the door is decided entirely before the forward
    /// window opens.
    fn sector_sample(date: NaiveDate, ticker: &str, sector: &str, five_y_cum: f64) -> Sample {
        let mut q = Quote::stub(ticker, "€100.00", "", ticker);
        q.instrument_type = "EQUITY".into();
        q.avg_turnover_eur = Some(1e9);
        q.range_pct = 90.0;
        q.sector = Some(sector.into());
        q.perf = crate::core::HORIZONS
            .iter()
            .map(|(l, _)| match *l {
                "1M" => Some(("x".to_string(), 2.0)),
                "1Y" => Some(("x".to_string(), 20.0)),
                "5Y" => Some(("x".to_string(), five_y_cum)),
                _ => None,
            })
            .collect();
        Sample { date, realized: 0.0, relative: 0.0, quote: Arc::new(q), fund: None, trail: Vec::new() }
    }

    /// (#328) THE POINT-IN-TIME FREEZE, and the only place it can be checked. The door's whole claim to
    /// not being look-ahead is that a cutoff is gated on the cohort of the cutoff BEFORE it — so:
    ///
    /// - the FIRST cutoff has no predecessor and must come out unstamped (door shut, control behaviour);
    /// - the SECOND must carry a floor drawn from the FIRST cutoff's names, not from its own — a name
    ///   must never help set the bar it is then judged against;
    /// - a sector that only appears LATER must never reach back and stamp an earlier cutoff.
    ///
    /// The bench series rises steadily, so `bench_leg_cagr` resolves at every cutoff and the door is
    /// open on the merits rather than shut by a missing index window.
    #[test]
    fn sector_cohort_is_frozen_at_the_previous_cutoff() {
        let bench = (
            vec![ymd(1995, 1, 3), ymd(2000, 1, 3), ymd(2005, 1, 3), ymd(2010, 1, 3)],
            vec![100.0, 120.0, 140.0, 160.0],
        );
        let tuning = BuyHeuristic::default();
        let mut samples = vec![
            sector_sample(ymd(2000, 1, 3), "OLD", "Energy", 300.0),
            sector_sample(ymd(2005, 1, 3), "MID", "Energy", 100.0),
            sector_sample(ymd(2010, 1, 3), "NEW", "Utilities", 100.0),
        ];
        stamp_sector_cohorts(&mut samples, &bench, &tuning);

        assert_eq!(samples[0].quote.growth_sector_floor, None, "the first cutoff has no predecessor -> door shut");

        let from_old = picks::long_cagr_pct(&samples[0].quote, &tuning).expect("OLD has a 5Y leg");
        let own = picks::long_cagr_pct(&samples[1].quote, &tuning).expect("MID has a 5Y leg");
        let stamped = samples[1].quote.growth_sector_floor.expect("the second cutoff inherits the first's cohort");
        assert!((stamped - from_old).abs() < 1e-9, "the bar must come from the PREVIOUS cutoff's name, not its own ({stamped} vs own {own})");
        assert!(stamped > own, "…and here that bar is HARDER than the name's own CAGR, so a self-built cohort would have been the giveaway");

        assert_eq!(samples[2].quote.growth_sector_floor, None, "Utilities never appeared in an earlier cutoff -> nothing to inherit");
    }

    /// The too-few-rows guards of `winsor_edge` (<4), `edge_terciles` (<3), `lane_metrics` (<4 scored)
    /// and `bootstrap_edge_ci` (<4 buckets), checked ONE ROW EITHER SIDE of each exact boundary.
    ///
    /// The frozen-data report pin (tests/backtest_fixture.rs) structurally cannot reach any of them:
    /// every lane it scores carries hundreds of rows, so `< 4` and `<= 4` produce byte-identical
    /// reports there. The mutation audit is what surfaced them — flipping each of these four guards
    /// left all six goldens green. What the guard buys is that a spread read off two or three rows
    /// never reaches the human as a number: NaN/None renders as `n/a`, a computed value renders as if
    /// it meant something, and the whole point of the winsorized edge and the tercile monotonicity
    /// check is to warn that an edge rests on too few rows.
    #[test]
    fn spread_guards_hold_at_their_boundary() {
        // One row per YEAR, so the four rows are also four distinct ~6mo buckets: `bootstrap_edge_ci`
        // guards on bucket count where the other three guard on row count.
        let rows: Vec<Sample> = [(2020, -10.0), (2021, 0.0), (2022, 5.0), (2023, 20.0)]
            .iter()
            .map(|&(y, rel)| Sample { relative: rel, ..sample(ymd(y, 1, 1), rel) })
            .collect();
        // score = index, so each function's internal sort is well-defined rather than tie-broken.
        let pairs = |n: usize| -> Vec<(&Sample, f64)> {
            rows[..n].iter().enumerate().map(|(i, s)| (s, i as f64)).collect()
        };
        assert!(winsor_edge(&pairs(3)).is_nan(), "3 rows is too few to winsorize");
        assert!(winsor_edge(&pairs(4)).is_finite(), "4 rows is exactly enough to winsorize");
        assert!(edge_terciles(&pairs(2)).0.is_nan(), "2 rows is too few for terciles");
        assert!(edge_terciles(&pairs(3)).0.is_finite(), "3 rows is exactly enough for terciles");

        // These two gate on rows that SCORED, so they need a scorer; a constant one keeps every row and
        // leaves the guard as the only thing under test.
        fn keeps_every_row(_: &Quote, _: &BuyHeuristic) -> Option<f64> {
            Some(0.0)
        }
        let t = BuyHeuristic::default();
        assert_eq!(lane_metrics(&rows[..3], keeps_every_row, &t).1, 0.0, "3 scored rows -> no edge");
        assert_ne!(lane_metrics(&rows[..4], keeps_every_row, &t).1, 0.0, "4 scored rows -> an edge");
        let ci = |n: usize| bootstrap_edge_ci(&rows[..n], keeps_every_row, &t, 8, 5.0, 95.0, 1);
        assert!(ci(3).is_none(), "3 buckets is too few to resample");
        assert!(ci(4).is_some(), "4 buckets is exactly enough to resample");

        // Enough buckets, but the gate rejects every row, so every draw is empty. That is the second
        // way the bootstrap declines to publish a band, and it must stay None rather than hand back a
        // band computed from an empty edge distribution (which percentile() renders as NaN, not `n/a`).
        fn scores_nothing(_: &Quote, _: &BuyHeuristic) -> Option<f64> {
            None
        }
        assert!(
            bootstrap_edge_ci(&rows, scores_nothing, &t, 8, 5.0, 95.0, 1).is_none(),
            "every draw gated out -> no band, not a NaN band"
        );
    }

    /// (#93) The market-split peer group, on the case it exists for: one bucket holding a US cohort and
    /// a London cohort whose whole return difference is the FX move over the window, not skill.
    ///
    /// Pooled, every US name reads as a winner and every LSE name as a loser by half the currency move.
    /// Split, each cohort de-means against its own currency and the FX drops out exactly. (#1231) A
    /// lonely German line is too thin to be a peer group: it joins the bucket's LARGEST slice rather
    /// than de-meaning against itself and reporting a `relative` of 0 as if measured. Eight thin lines
    /// together are a group of their own. Either way every group nets to 0, as `run` asserts.
    #[test]
    fn the_market_split_peer_group_cancels_the_currency_move() {
        let d = ymd(2015, 1, 1);
        let row = |tk: &str, r: f64| {
            let mut s = sample(d, r);
            s.quote = Arc::new(Quote::stub(tk, "1", "", tk));
            s
        };
        // two cohorts, same dispersion (-5/+5 around their own centre), 20 pts apart purely by currency.
        // Nine US names to eight LSE ones, so the largest slice is not a tie.
        let us = [30.0, 40.0, 35.0, 35.0, 35.0, 35.0, 35.0, 35.0, 35.0].into_iter().enumerate();
        let ln = [10.0, 20.0, 15.0, 15.0, 15.0, 15.0, 15.0, 15.0].into_iter().enumerate();
        let ln: Vec<Sample> = ln.map(|(i, r)| row(&format!("LN{i}.L"), r)).collect();
        let rows: Vec<Sample> = us.map(|(i, r)| row(&format!("US{i}"), r)).chain(ln.clone()).collect();
        let run = |v: &[Sample], by_market: bool| {
            let mut v = v.to_vec();
            demean_with(&mut v, by_market);
            v
        };
        let nets = |v: &[Sample]| v.iter().map(|s| s.relative).sum::<f64>().abs() < 1e-9;
        let (pooled, split) = (run(&rows, false), run(&rows, true));
        // pooled: one mean over both markets, so the whole 20-pt FX gap lands in `relative` as if it
        // were selection — the US cohort is lifted and the LSE cohort docked.
        let pm: f64 = rows.iter().map(|s| s.realized).sum::<f64>() / rows.len() as f64;
        assert!((pooled[0].relative - (30.0 - pm)).abs() < 1e-9, "{}", pooled[0].relative);
        assert!((pooled[9].relative - (10.0 - pm)).abs() < 1e-9, "{}", pooled[9].relative);
        // the two cohorts sit ~20 apart in `relative` on nothing but the currency.
        assert!((pooled[0].relative - pooled[9].relative - 20.0).abs() < 1e-9);
        // split: each cohort centres on its own 35 / 15, so the SAME name reads -5 in both cohorts and
        // the currency move is gone. This is the whole claim.
        assert!((split[0].relative + 5.0).abs() < 1e-9, "{}", split[0].relative);
        assert!((split[9].relative + 5.0).abs() < 1e-9, "{}", split[9].relative);
        assert!((split[1].relative - 5.0).abs() < 1e-9 && (split[10].relative - 5.0).abs() < 1e-9);
        assert!(nets(&split));
        // the lonely German line joins the US slice: (315 + 45) / 10 = 36, so it reads +9 and each US
        // name moves by one point. LSE is not the largest slice and keeps its own 15.
        let lone = run(&[rows.clone(), vec![row("DE0.DE", 45.0)]].concat(), true);
        assert!((lone[17].relative - 9.0).abs() < 1e-9, "{}", lone[17].relative);
        assert!((lone[0].relative + 6.0).abs() < 1e-9, "{}", lone[0].relative);
        assert!((lone[9].relative + 5.0).abs() < 1e-9, "{}", lone[9].relative);
        assert!(nets(&lone), "the first cut sent this row to the pooled mean and broke the invariant");
        // eight one-line markets ARE a peer group together: they de-mean against their own 50, and
        // the LSE slice beside them (exactly MIN_PEER_GROUP names, the largest) keeps its own 15.
        let thin = [46.0, 54.0, 50.0, 50.0, 50.0, 50.0, 50.0, 50.0].into_iter().enumerate();
        let thin = run(&[ln, thin.map(|(i, r)| row(&format!("S{i}.X{i}"), r)).collect()].concat(), true);
        assert!((thin[8].relative + 4.0).abs() < 1e-9, "{}", thin[8].relative);
        assert!((thin[0].relative + 5.0).abs() < 1e-9, "{}", thin[0].relative);
        assert!(nets(&thin));
        // and OFF must be the identity — every edge, rho and OOS number in this repo rests on it.
        assert!(pooled.iter().zip(&rows).all(|(p, r)| p.realized == r.realized));
    }

    /// (#90) The purge, at the boundaries that decide whether it is safe: 0 is the identity — the
    /// DEFAULT, and the whole reason every golden stays byte-identical — and a real span drops exactly
    /// the tail whose forward window is still open at the split date, no more.
    ///
    /// The last assertion is the finding rather than a corner case: at the honest 12y span (288 months,
    /// a `years` purge plus a `years` embargo) this sample has NO train rows left. That is why the knob
    /// ships at 0 instead of at the statistically correct value.
    #[test]
    fn the_split_purge_drops_only_the_overlapping_tail() {
        // one row per quarter from 2010-01, so index 30 is 2017-07-01 and the spans below are readable.
        let rows: Vec<Sample> =
            (0..40).map(|i| sample(ymd(2010 + i / 4, 1 + 3 * (i % 4) as u32, 1), 0.0)).collect();
        let cut = |months| purged_cut(&rows, 30, months, |s: &Sample| s.date);
        assert_eq!(rows[30].date, ymd(2017, 7, 1));
        assert_eq!(cut(0), 30, "0 is OFF, and off must be the identity — every golden rests on this");
        assert_eq!(cut(-12), 30, "a negative span is off too, never a reversal");
        // 12 30-day months back from 2017-07-01 is 2016-07-06, so 2016-07-01 (index 26) is the last row
        // kept and 27 is the new end: rows 27..30 are the ones still sharing a forward path with TEST.
        assert_eq!(cut(12), 27);
        assert_eq!(rows[26].date, ymd(2016, 7, 1));
        assert_eq!(cut(288), 0, "the honest 12y span (purge + embargo) outruns the whole sample");
        // degenerate cuts pass through rather than indexing `rows[cut]` off the end.
        assert_eq!(purged_cut(&rows, 0, 12, |s: &Sample| s.date), 0);
        assert_eq!(purged_cut(&rows, 40, 12, |s: &Sample| s.date), 40);
        // (#340) a row exactly `months*30` days back sits ON the edge and is purged with the overlap.
        let edge: Vec<Sample> = (0..4).map(|i| sample(ymd(2010, 1, 1) + chrono::Duration::days(30 * i), 0.0)).collect();
        assert_eq!(purged_cut(&edge, 3, 1, |s: &Sample| s.date), 2);
        // (#1227) the embargo is the mirror: 12 30-day months on from 2017-07-01 is 2018-06-26, so
        // 2018-07-01 (index 34) is the first later row kept; a row ON that date would be kept too.
        let halves = |months| purged_halves(&rows, 30, months, |s: &Sample| s.date);
        assert_eq!(halves(0), (30, 30), "off is the identity on both sides");
        assert_eq!(halves(12), (27, 34));
        assert_eq!(rows[34].date, ymd(2018, 7, 1));
        assert_eq!(halves(288), (0, 40), "the honest 12y span empties both sides");
        assert_eq!(purged_halves(&rows, 40, 12, |s: &Sample| s.date), (40, 40));
        assert_eq!(purged_halves(&rows, 0, 12, |s: &Sample| s.date), (0, 0));
        assert_eq!(purged_halves(&edge, 1, 1, |s: &Sample| s.date), (0, 2), "a row exactly one span on is kept");
        // the footer at a 1y hold: one span drops the three rows either side of mid, keeping mid-3 early
        // and n-mid-4 late. 16 rows keep 5 and 4, ON the floor; 14 keep 4 and 3, under it.
        let line = |n: usize| purged_split_line(&rows[..n], 1);
        assert_eq!(line(16), "\n  purged split, 12 months a side: early 5 of 8 rows kept, late 4 of 8 — both sides kept");
        assert!(line(14).contains("early 4 of 7 rows kept, late 3 of 7 — UNRESOLVED"), "{}", line(14));
        assert!(purged_split_line(&rows[..16], 2).starts_with("\n  purged split, 24 months a side"), "the span is a year per year held");
    }

    /// (#89) The block length is real, and 0 is exactly the one-bucket resample every band in this repo
    /// was read off.
    ///
    /// 100 half-year buckets, 4 rows each, scores 0..3, with the peer-relative return sitting on the top
    /// two rows only — so a draw's edge is exactly the MEAN of `e_k` over the buckets it drew, and the
    /// bootstrap reduces to the textbook case (the sampling distribution of a mean). `e_k` is a two-regime
    /// step (fifty flat buckets then fifty rich ones) because SERIAL CORRELATION is the entire subject:
    /// the real data has it (consecutive cutoffs share 95.8% of their forward path at 12y) and a
    /// one-bucket block cannot see it. Both bands come from pools of about the same size (100 buckets
    /// at block 1, 13 × 8 = 104 at block 8) — the real difference is how many INDEPENDENT selections
    /// built them, 100 at block 1 against 13 at block 8. Measured, and pinned to the bit below:
    /// (64.5, 88.5) at block 1 against (41.8, 108.2) at block 8, width 24.0 -> 66.3, a ratio of about
    /// 2.8. The 1.25 bar is far under that ratio, not a tuned number: this is a seeded PRNG on frozen
    /// rows, so it does not drift.
    #[test]
    fn a_longer_bootstrap_block_widens_the_band() {
        fn score_is_the_price(q: &Quote, _: &BuyHeuristic) -> Option<f64> {
            q.price.parse().ok()
        }
        let rows: Vec<Sample> = (0..100)
            .flat_map(|k| {
                let e = if k < 50 { 0.0 } else { 150.0 };
                let date = ymd(2020 + k / 2, 1 + 6 * (k % 2) as u32, 1);
                // scores 0..3 are the same in every bucket, so the top/bottom split is the same two rows
                // per bucket in every draw and nothing but the drawn bucket set moves the edge.
                (0..4).map(move |r| Sample {
                    relative: if r >= 2 { e } else { 0.0 },
                    quote: Arc::new(Quote::stub("X", &r.to_string(), "", "X")),
                    ..sample(date, 0.0)
                })
            })
            .collect();
        let t = BuyHeuristic::default();
        let band = |block: usize| bootstrap_edge_ci(&rows, score_is_the_price, &t, 400, 5.0, 95.0, block).unwrap();
        assert_eq!(band(0), band(1), "0 and 1 both clamp to a one-bucket draw inside the fn");
        let (one, eight) = (band(1), band(8));
        assert!(
            eight.1 - eight.0 > 1.25 * (one.1 - one.0),
            "8-bucket blocks make 13 independent picks where 1-bucket blocks make 100, so the band must \
             grow: {one:?} vs {eight:?}"
        );
        // (#119) 100 buckets carry twelve 8-bucket blocks, which is why the assertion above measures a
        // WIDENING at all: the same comparison on a 32-bucket record reads the other way, because a
        // 4-block draw barely varies. That is the collapse the ratio guard exists to refuse.
        //
        // 100 and not 96 so the guard's EXACT boundary is reachable: ten 10-bucket blocks fit a
        // 100-bucket record and eleven do not, which is the pair that separates `<` from `<=`. On a
        // record no multiple of MIN_BLOCKS the two spellings decide identically and the mutant lives.
        let band_at = |b: usize| bootstrap_edge_ci(&rows, score_is_the_price, &t, 400, 5.0, 95.0, b);
        assert!(band_at(10).is_some(), "100/10 -> exactly ten whole blocks, which is enough");
        assert!(band_at(11).is_none(), "100/11 is under ten whole blocks -> no band, however honest the length");
        // (#340) The bands to the bit. The width asserts above survive a broken xorshift or a wrong
        // block index as long as the band still widens; a seeded stream on frozen rows has one answer.
        assert_eq!((one, eight), ((64.5, 88.5), (41.82692307692308, 108.17307692307692)));
        // the edge is a SPREAD: moving every row's level moves no band. The bottom half above is all
        // zeros, which is why `t + b` read the same as `t - b` there.
        let shifted: Vec<Sample> = rows.iter().map(|s| Sample { relative: s.relative - 50.0, ..s.clone() }).collect();
        let lvl = bootstrap_edge_ci(&shifted, score_is_the_price, &t, 400, 5.0, 95.0, 1).unwrap();
        assert!((lvl.0 - one.0).abs() < 1e-9 && (lvl.1 - one.1).abs() < 1e-9, "{lvl:?} vs {one:?}");
    }

    /// (#1227) `bootstrap_cluster` draws whole tickers. Eight names in twelve buckets, each bucket holding
    /// every name once: a bucket draw sees the same mix in every draw, so its band is one point, while a
    /// ticker draw changes the mix and the band opens. The block is ignored, so a block the bucket draw
    /// refuses (twelve buckets hold two of six, not ten) still prints, and the seed makes it repeat.
    #[test]
    fn a_ticker_cluster_bootstrap_resamples_names_not_dates() {
        fn score_is_the_price(q: &Quote, _: &BuyHeuristic) -> Option<f64> {
            q.price.parse().ok()
        }
        let rows: Vec<Sample> = (0..12)
            .flat_map(|k| {
                let date = ymd(2010 + k / 2, 1 + 6 * (k % 2) as u32, 1);
                (0..8).map(move |t| Sample {
                    relative: 10.0 * t as f64,
                    quote: Arc::new(Quote::stub(&format!("T{t}"), &t.to_string(), "", "X")),
                    ..sample(date, 0.0)
                })
            })
            .collect();
        let off = BuyHeuristic::default();
        let on = BuyHeuristic { bootstrap_cluster: true, ..BuyHeuristic::default() };
        let band = |t: &BuyHeuristic, block: usize| bootstrap_edge_ci(&rows, score_is_the_price, t, 400, 5.0, 95.0, block);
        assert_eq!(band(&off, 1), Some((40.0, 40.0)), "every bucket is the same eight names, so every draw is the same edge");
        assert!(band(&off, 6).is_none(), "twelve buckets are two blocks of six, under ten");
        let c = band(&on, 1).unwrap();
        assert!(c.1 - c.0 > 1.0, "a ticker draw changes the names, so the band opens: {c:?}");
        assert_eq!(band(&on, 6), Some(c), "the cluster draw has no block, so a refused block still prints");
        assert_eq!(band(&on, 1), Some(c), "seeded");
    }

    /// (#120) The gate's search string and the basket it grades are ONE number. `markers::VERDICT_ROW`
    /// has to be a `const &str` (both test crates read it, and `tests/network.rs` matches on it), so it
    /// cannot be built from `VERDICT_TOP` at compile time without a formatting dep. This is that
    /// dependency, as an assertion instead: move `VERDICT_TOP` and forget the marker, and the gate goes
    /// on asserting on a row the verdict no longer reports — green, forever, grading nothing. Exactly
    /// the failure `gate_markers_are_all_in_the_golden` was written for, one level up.
    #[test]
    fn verdict_row_matches_the_basket() {
        assert_eq!(markers::VERDICT_ROW, format!("top-{VERDICT_TOP} "), "the marker must name the graded basket");
        assert!(markers::VERDICT_ROW.ends_with(' '), "the trailing space is what stops top-100 matching top-10");
        // and the row it names must be a rung the ladder actually prints, or the marker matches nothing
        assert!(TOP_LADDER.contains(&VERDICT_TOP), "the verdict basket must be one of the printed rungs");
    }

    /// (Item 10) The best-of-N haircut. Tested here and not through its caller because the caller is
    /// `sweep_fund_factor`, which the offline suite cannot run at all.
    #[test]
    fn bonferroni_tail_tightens_both_ends_by_five_over_n() {
        assert_eq!(bonferroni_tail(1), (5.0, 95.0), "one candidate is no selection — the plain 90% band");
        assert_eq!(bonferroni_tail(10), (0.5, 99.5));
        let (lo, hi) = bonferroni_tail(14); // a fixed arity, NOT FUND_FACTORS.len() — this tests the
        // function, so it must not move when (#157) widens the array (14 was the length then)
        assert!((lo - 0.357_142_857).abs() < 1e-9 && (hi - 99.642_857_142).abs() < 1e-9, "{lo} {hi}");
        // both ends move OUTWARD as the search widens: more factors tried -> a stricter bar, which is
        // the whole point. A `-` flipped to `+` here would tighten the band and pass a losing winner.
        assert!(bonferroni_tail(14).0 < bonferroni_tail(10).0 && bonferroni_tail(14).1 > bonferroni_tail(10).1);
        assert_eq!(bonferroni_tail(0), (5.0, 95.0), "no candidates -> no division by zero");
    }

    /// (#1045) Only a band floor strictly above 0 trusts the winner; a floor AT 0 straddles it.
    #[test]
    fn band_verdict_trusts_only_a_floor_above_zero() {
        assert!(band_verdict(0.1).starts_with("survives"), "{}", band_verdict(0.1));
        for lo in [0.0, -0.1] {
            assert!(band_verdict(lo).starts_with("within best-of-N luck"), "{lo}: {}", band_verdict(lo));
        }
    }

    /// (#119) The wire itself. `bootstrap_block` is what turns a knob that could only ever hold ONE
    /// number into a length that tracks the horizon it is read at — so the thing under test is that
    /// the same config yields a different block at 8y than at 20y, which is exactly what a fixed count
    /// could not do and why (#89) shipped off.
    ///
    /// Graded directly and not through a golden: the goldens are 12y/20y/8y runs of the whole report,
    /// so they pin the CONSEQUENCE of this number and never the number, and a mutant returning a
    /// constant would still move every band by some amount and look like a re-bless.
    #[test]
    fn bootstrap_block_tracks_the_hold_unless_overridden() {
        let derive = BuyHeuristic::default();
        assert_eq!(derive.bootstrap_block_buckets, 0, "0 is the shipped sentinel for `derive`");
        // buckets are ~6mo, the dependence length is one hold -> 2 buckets per year of hold
        assert_eq!(bootstrap_block(8, &derive), 16);
        assert_eq!(bootstrap_block(12, &derive), 24);
        assert_eq!(bootstrap_block(20, &derive), 40);
        // a hold of 0 or less is not a hold; clamp to one year rather than returning a 0 block, which
        // `bootstrap_edge_ci` would then clamp back to 1 and silently call a one-bucket draw honest.
        assert_eq!(bootstrap_block(0, &derive), 2);
        assert_eq!(bootstrap_block(-5, &derive), 2);

        // any positive value is taken as written, at every horizon — that is the documented revert.
        let pinned = BuyHeuristic { bootstrap_block_buckets: 1, ..BuyHeuristic::default() };
        assert_eq!(bootstrap_block(8, &pinned), 1);
        assert_eq!(bootstrap_block(20, &pinned), 1, "an override does NOT track the hold");
    }

    /// (#140) The graded redundancy skip's contract: IDENTITY at the shipped `growth_corr_cap: 0.0`
    /// (the goldens depend on it), drops the second copy of a bet once armed and refills from below,
    /// still honours `n`, and never blocks on an unjudgeable pair. `report_vs_benchmark` returns `()`
    /// and only prints, so none of this is reachable unless the walk is split out ((#75)).
    #[test]
    fn corr_cap_rung_drops_the_duplicate_bet_and_is_identity_when_off() {
        let up: Vec<f64> = (0..12).map(|i| i as f64).collect();
        let down: Vec<f64> = (0..12).map(|i| -(i as f64)).collect();
        let rows = vec!["A", "TWIN", "C"];
        let trails: Vec<&[f64]> = vec![&up, &up, &down];

        // OFF: plain top-n, the twin still in the book, and n past the end keeps everything.
        assert_eq!(corr_cap_rung(&rows, &trails, 2, 0.0), vec!["A", "TWIN"]);
        assert_eq!(corr_cap_rung(&rows, &trails, 9, 0.0), vec!["A", "TWIN", "C"]);
        // ARMED: TWIN correlates +1.0 with A, so it drops and C refills from below.
        assert_eq!(corr_cap_rung(&rows, &trails, 2, 0.4), vec!["A", "C"]);
        // n still bounds the book once armed.
        assert_eq!(corr_cap_rung(&rows, &trails, 1, 0.4), vec!["A"]);
        // An unjudgeable pair (empty trail) never blocks — unjudgeable is not a verdict.
        let blind: Vec<&[f64]> = vec![&up, &[], &down];
        assert_eq!(corr_cap_rung(&rows, &blind, 2, 0.4), vec!["A", "TWIN"]);
    }

    /// (#566) Each class books its own top-n: a higher-scored name of another class never takes its slot,
    /// a bucket it missed reads `empty`, and a class absent from every bucket prints no line at all.
    #[test]
    fn class_book_lines_grades_each_class_alone() {
        let row = |s: f64, r: f64, bench: f64, t: &str| (s, r, bench, t.to_string());
        let by_bucket = BTreeMap::from([(1, vec![row(5.0, -50.0, 0.0, "B"), row(9.0, 100.0, 0.0, "A"), row(7.0, 20.0, 10.0, "C")])]);
        let calendar = std::collections::BTreeSet::from([1, 2]);
        let class_of = |_: i32, t: &str| Some(if t == "C" { 0 } else { 2 });
        assert_eq!(
            class_book_lines(&by_bucket, &calendar, class_of, 1, 1),
            ["  BOOK stock b1 +100.0 pts/yr | A +100%", "  BOOK stock b2 empty", "  BOOK crypto b1 +10.0 pts/yr | C +20%", "  BOOK crypto b2 empty"]
        );
        // n = 2: the equal-weight multiple (2.0 + 0.5) / 2 over 2 years
        assert_eq!(class_book_lines(&by_bucket, &calendar, class_of, 2, 2)[0], "  BOOK stock b1 +11.8 pts/yr | A +100% B -50%");
    }

    /// (#874) A cohort sample joins `all` scored on its own quote and benchmarked by `bench`, its ticker comes
    /// back for the class closure, and a sample the benchmark has no window for joins nothing. The helper is
    /// class-blind, so a stock sample stands in for a coin.
    #[test]
    fn merge_coin_cohort_adds_scored_samples_and_names_them() {
        let tuning = shipped_hard();
        let s = synthetic_samples().into_iter().find(|s| growth_score(&s.quote, &tuning).is_some()).expect("a scoring sample");
        let score = growth_score(&s.quote, &tuning).unwrap();
        let coins = BTreeMap::from([(1, vec![&s])]);
        let mut all = BTreeMap::new();
        let names = merge_coin_cohort(&mut all, &coins, |_| Some(10.0), &tuning);
        assert_eq!(names, HashSet::from([s.quote.ticker.as_str()]));
        assert_eq!(all[&1], [(score, s.realized, 10.0, s.quote.ticker.clone())]);
        let mut none = BTreeMap::new();
        assert!(merge_coin_cohort(&mut none, &coins, |_| None, &tuning).is_empty() && none.is_empty());
    }

    /// (#75) The value brake's graded trim, on two buckets whose peg cohorts do not overlap at all —
    /// bucket 1 spans 10..50, bucket 2 spans 100..500. That gap is the whole point: a POOLED percentile
    /// would floor both at 50 and so cut nothing from bucket 2 while gutting bucket 1, which is the one
    /// way this brake could quietly stop being cross-sectional. The boundary name (peg exactly ON the
    /// floor) is kept, matching `drop_bottom_book`'s `if v < t { skip }`, and the unjudgeable name is
    /// kept because unjudgeable is not a verdict.
    #[test]
    fn value_floor_trims_each_bucket_against_its_own_cohort() {
        let row = |b: i32, tk: &str, peg: Option<f64>| (b, 1.0, 2.0, 3.0, tk.to_string(), peg);
        let rows = vec![
            row(1, "P10", Some(10.0)),
            row(1, "P20", Some(20.0)),
            row(1, "P30", Some(30.0)),
            row(1, "P40", Some(40.0)),
            row(1, "P50", Some(50.0)),
            row(1, "NOPEG", None),
            row(2, "Q100", Some(100.0)),
            row(2, "Q200", Some(200.0)),
            row(2, "Q300", Some(300.0)),
            row(2, "Q400", Some(400.0)),
            row(2, "Q500", Some(500.0)),
        ];
        let names = |m: &BTreeMap<i32, Vec<(f64, f64, f64, String)>>, b: i32| -> Vec<String> {
            m.get(&b).map(|v| v.iter().map(|(_, _, _, t)| t.clone()).collect()).unwrap_or_default()
        };
        let off = value_floor_trim(&rows, 0.0);
        assert_eq!(names(&off, 1), ["P10", "P20", "P30", "P40", "P50", "NOPEG"], "0 = off, the cohort is untouched");
        assert_eq!(names(&off, 2), ["Q100", "Q200", "Q300", "Q400", "Q500"], "0 = off in every bucket, not just the first");
        let cut = value_floor_trim(&rows, 40.0);
        assert_eq!(
            names(&cut, 1),
            ["P30", "P40", "P50", "NOPEG"],
            "floor = the 40th pct of THIS bucket (30): dearest-for-their-growth go, the name sitting exactly ON the floor stays, unjudgeable stays"
        );
        assert_eq!(
            names(&cut, 2),
            ["Q300", "Q400", "Q500"],
            "bucket 2 is floored at 300 by its OWN cohort — pooled, it would floor at 50 and cut nobody"
        );
    }

    #[test]
    fn h2h_low_line_stays_silent_unless_it_has_something_true_to_say() {
        // (#135) THE PRINT CONDITION IS THE TESTABLE PART. This line exists to be read BESIDE the
        // shipped 11-20 head-to-head, so the two ways it must stay quiet are not cosmetic: printing
        // when the knob is off would move every golden (non-negotiable #1), and printing when no
        // window reaches the 6-10 book would render 0/0 as "NaN%" and put a fake denominator next to
        // a real one — the exact "a short book fakes a number" failure `h2h_beats` returns None for.
        let some = H2h { h1: 3, h25: 2, n: 4 };
        assert_eq!(h2h_low_line(false, some, 6), None, "knob off: silent even with windows to report");
        assert_eq!(h2h_low_line(true, H2h::default(), 6), None, "no window reaches the 6-10 book: silent");

        let line = h2h_low_line(true, some, 6).expect("knob on with 4 windows must print");
        assert!(line.contains("3/4 (75%)"), "rank-1 tally and its own denominator: {line}");
        assert!(line.contains("2/4 (50%)"), "the 2-5 book shares that denominator: {line}");
        // The whole point of the line: its denominator NEXT TO the shipped one, never in place of it.
        assert!(line.contains("denominator 4 here vs 6"), "both denominators, side by side: {line}");
    }

    /// (#45) rank-slice ladder + head-to-head on a synthetic ranking with KNOWN outcomes — the
    /// assert that fails if slices stop being disjoint, stop following score order, or the
    /// head-to-head silently changes its comparison sets. Window A: 25 names, score descending,
    /// realized stepped by rank block (rank1 +100, 2-5 +50, 6-10 +30, 11-20 +10, 21-25 0), bench
    /// +10 everywhere. Window B: 8 names only — too small for a head-to-head (needs ≥11), and its
    /// 21-50 slice must NOT exist rather than fake a short book.
    #[test]
    fn rank_slice_ladder_and_head_to_head() {
        let mut by_bucket: std::collections::BTreeMap<i32, Vec<(f64, f64, f64, String)>> =
            std::collections::BTreeMap::new();
        let step = |rank: usize| match rank {
            0 => 100.0,
            1..=4 => 50.0,
            5..=9 => 30.0,
            10..=19 => 10.0,
            _ => 0.0,
        };
        by_bucket.insert(1, (0..25).map(|r| (100.0 - r as f64, step(r), 10.0, format!("A{r}"))).collect());
        by_bucket.insert(2, (0..8).map(|r| (100.0 - r as f64, step(r), 10.0, format!("B{r}"))).collect());
        let (slices, h2h_mid, h2h_low) = rank_slice_stats(&by_bucket);
        let (h1, h25, hn) = (h2h_mid.h1, h2h_mid.h25, h2h_mid.n);
        let get = |label: &str| slices.iter().find(|(l, _)| *l == label).unwrap().1.clone();
        let close = |a: &(f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9;
        // window A's slice books equal their block means; bench cum is +10 in every slice
        assert_eq!(get("1").len(), 2, "both windows have a #1");
        assert!(get("1").iter().all(|p| close(p, (100.0, 10.0))));
        assert!(close(&get("2-5")[0], (50.0, 10.0)));
        assert!(close(&get("6-10")[0], (30.0, 10.0)));
        assert_eq!(get("11-20").len(), 1, "window B (8 names) has no rank-11");
        assert!(close(&get("11-20")[0], (10.0, 10.0)));
        assert_eq!(get("21-50").len(), 1, "window B must not fake a 21-50 book");
        assert!(close(&get("21-50")[0], (0.0, 10.0)));
        // window B's short tail: ranks 6-8 all sit in the 6-10 slice, clamped at its pool
        assert!(close(&get("6-10")[1], (30.0, 10.0)));
        // head-to-head: only window A qualifies (>10 names); #1 (+100) and 2-5 (+50) both beat 11-20 (+10)
        assert_eq!((h1, h25, hn), (1, 1, 1));
        // (#135) THE SAME QUESTION ON A WINDOW THE SHIPPED GUARD CANNOT SEE. Window B (8 names) is
        // too short to hold ranks 11-20 and contributes NOTHING to the line above, but it does hold a
        // 6-10 book (ranks 6-8, clamped at its pool), so it answers here: #1 (+100) and the 2-5 book
        // (+50) both beat it (+30). The denominators — 1 above, 2 here — are the finding, not the
        // verdicts, which agree.
        assert_eq!(
            (h2h_low.h1, h2h_low.h25, h2h_low.n),
            (2, 2, 2),
            "both windows reach the 6-10 book; only window A reaches the 11-20 book"
        );

        // EXACTLY ON THE BOUNDARY. Windows A and B (25 and 8 names) straddle every slice start but
        // land on none of them, so `vv.len() <= lo` and `vv.len() < lo` decide identically above —
        // the mutation audit flagged that guard as unkilled for precisely that reason. A window with
        // exactly 10 names sits ON the 11-20 slice's `lo`, and there the two spellings diverge:
        // `<` lets the slice through, `&vv[10..10]` is EMPTY, and `mean` of nothing is 0.0, which
        // this function's `(mean - 1.0) * 100.0` renders as a **-100% book** — a fabricated total
        // wipeout row in the ladder, not a missing one. That is the "no fake short book" the guard's
        // own comment promises, and it now has a test standing on it.
        by_bucket.insert(3, (0..10).map(|r| (100.0 - r as f64, step(r), 10.0, format!("C{r}"))).collect());
        let (slices, h2h_mid, h2h_low) = rank_slice_stats(&by_bucket);
        let get = |label: &str| slices.iter().find(|(l, _)| *l == label).unwrap().1.clone();
        assert_eq!(get("11-20").len(), 1, "a 10-name window sits ON the 11-20 slice start and must contribute NOTHING");
        assert_eq!(get("6-10").len(), 3, "all three windows still reach the 6-10 slice");
        assert!(get("11-20").iter().all(|p| p.0 > -100.0), "an empty slice must be skipped, never booked as -100%");
        // (#135) A 10-NAME WINDOW IS EXACTLY THE ADMISSION CEILING (#124) MEASURED, and this is what it
        // does to the two guards: it is NOT > 10, so the shipped h2h still counts one window, while the
        // 6-10 book now counts three. That 1-vs-3 gap on this fixture is the same shape as 2/4/6 vs a
        // full denominator on the PIT lane (#130) — the guard is starved by the gates it grades.
        assert_eq!(h2h_mid.n, 1, "a 10-name window still cannot answer the 11-20 head-to-head");
        assert_eq!((h2h_low.h1, h2h_low.h25, h2h_low.n), (3, 3, 3), "it can answer the 6-10 one");
        // (#135) THE COMPARISON BOOK MUST CLAMP TO A SHORT POOL, NOT INDEX PAST IT. A 14-name window
        // reaches the 11-20 slice but holds only four of its ten ranks, so `hi.min(vv.len())` is load-
        // bearing: dropping the clamp slices vv[10..20] on a 14-long vec and panics. Both books agree
        // here too (#1 +100 and the 2-5 book +50 beat 11-14's +10 and 6-10's +30).
        by_bucket.insert(4, (0..14).map(|r| (100.0 - r as f64, step(r), 10.0, format!("D{r}"))).collect());
        let (_, h2h_mid, h2h_low) = rank_slice_stats(&by_bucket);
        assert_eq!((h2h_mid.h1, h2h_mid.h25, h2h_mid.n), (2, 2, 2), "the 14-name window reaches the 11-20 book");
        assert_eq!((h2h_low.h1, h2h_low.h25, h2h_low.n), (4, 4, 4), "and every window reaches the 6-10 book");

        // (#135) A TIE IS NOT A WIN, AND ONLY AN EXACT TIE CAN PROVE IT. Every name in window E has
        // the SAME return, so #1, the 2-5 book and both comparison books are all exactly 50.0 — the
        // one shape where `>` and `>=` disagree. Both denominators must count the window (12 names
        // reaches 11-20 and 6-10 alike) while NEITHER numerator moves. Relaxing either comparison in
        // `h2h_beats` to `>=` would book a dead heat as a win for the argmax, which is precisely the
        // winner's-curse the h2h GUARD exists to catch, so this case is load-bearing for the guard.
        by_bucket.insert(5, (0..12).map(|r| (100.0 - r as f64, 50.0, 10.0, format!("E{r}"))).collect());
        let (_, h2h_mid, h2h_low) = rank_slice_stats(&by_bucket);
        assert_eq!((h2h_mid.h1, h2h_mid.h25, h2h_mid.n), (2, 2, 3), "the tied window counts, and wins nothing");
        assert_eq!((h2h_low.h1, h2h_low.h25, h2h_low.n), (4, 4, 5), "same on the 6-10 book");
        // median: odd + even sample sizes, and the top-N table's clone-then-sort usage pattern
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), 2.5);
        // (#815) a NaN sorts last under total_cmp instead of panicking the sort
        assert_eq!(median(vec![3.0, f64::NAN, 1.0]), 3.0);
    }

    /// (#40) benchmark math: `ann` inverts a 12y cumulative back to CAGR (and floors a wipeout at
    /// −100%, no NaN), and `benchmark_fwd` picks the first session on/after the cutoff + the first past
    /// +Ny, returning None when the window runs off the end.
    #[test]
    fn benchmark_math() {
        // (1.10^12 − 1)·100 cumulative -> back to +10%/yr; a −100% window annualizes to −100, not NaN.
        assert!((ann((1.10_f64.powi(12) - 1.0) * 100.0, 12) - 10.0).abs() < 1e-6);
        // (#867) below −100% cumulative the wealth base clamps at 0: no NaN root
        assert_eq!(ann(-150.0, 3), -100.0);
        assert!((ann(-100.0, 12) - (-100.0)).abs() < 1e-9);
        let dates: Vec<NaiveDate> = (0..15).map(|k| ymd(2000 + k, 1, 3)).collect();
        let closes: Vec<f64> = (0..15).map(|k| 100.0 * 1.08_f64.powi(k)).collect(); // +8%/yr
        let r = benchmark_fwd(&dates, &closes, ymd(2000, 1, 1), 12).unwrap();
        assert!((ann(r, 12) - 8.0).abs() < 0.1); // 12y forward from 2000 -> ~+8%/yr
        assert!(benchmark_fwd(&dates, &closes, ymd(2010, 1, 1), 12).is_none()); // no 12y window left
    }

    /// (#804) `payout_multiple` counts a payout strictly after the cutoff and up to `to` inclusive, each at
    /// the close of the last bar BEFORE its ex-date, so an ex-date that falls on a bar reinvests at the
    /// prior close.
    #[test]
    fn payout_multiple_reinvests_each_window_payout_at_the_prior_close() {
        let dates: Vec<NaiveDate> = (0..4).map(|k| ymd(2000 + k, 1, 3)).collect();
        let closes = [100.0, 110.0, 120.0, 130.0];
        let divs = [
            (ymd(2000, 1, 3), 50.0),  // ON the cutoff: banked before it, not counted
            (ymd(2000, 6, 1), 5.0),   // at 100
            (ymd(2001, 1, 3), 11.0),  // on a bar: at the PRIOR close, 100, not 110
            (ymd(2002, 6, 1), 12.0),  // on `to`: counted, at 120
            (ymd(2002, 6, 2), 99.0),  // after `to`: not counted
        ];
        let m = payout_multiple(&dates, &closes, &divs, 0, ymd(2002, 6, 1));
        assert!((m - 1.05 * 1.11 * 1.1).abs() < 1e-12, "{m}");
        assert_eq!(payout_multiple(&dates, &closes, &divs, 3, ymd(2010, 1, 1)), 1.0, "no payout after the last cutoff");
    }

    /// (r40) `bench_trailing` is the BACKWARD mirror of `benchmark_fwd`: the return over the N years
    /// ENDING at the cutoff, the quantity subtracted from a name's trailing return to make relative
    /// strength. It must be a real, non-zero, per-cutoff-VARYING number — if it ever collapsed to 0
    /// (or the subtraction were dropped) `rel_str_5y` would equal `abs_mom_5y` and the whole probe
    /// would be measuring absolute momentum twice. None before the series reaches back a full window.
    #[test]
    fn bench_trailing_math() {
        let dates: Vec<NaiveDate> = (0..15).map(|k| ymd(2000 + k, 1, 3)).collect();
        let closes: Vec<f64> = (0..15).map(|k| 100.0 * 1.08_f64.powi(k)).collect(); // +8%/yr
        let r = bench_trailing(&dates, &closes, ymd(2012, 1, 3), 5).unwrap();
        assert!((ann(r, 5) - 8.0).abs() < 0.2); // trailing 5y of +8%/yr -> ~+8%/yr, and materially non-zero
        assert!(r > 40.0); // (1.08^5 − 1)·100 ≈ +47%: the subtracted leg is real, not a rounding ghost
        assert!(bench_trailing(&dates, &closes, ymd(2001, 1, 3), 5).is_none()); // <5y of trailing history
    }

    /// (#326) `bench_range_pct` is the index's OWN `range_pct`: `core::price_pct_rank` over the trailing
    /// 10 years ending at the cutoff, the same statistic `Quote::range_pct` carries for a name. Two
    /// properties decide whether the regime valve can ever open, and both are pinned here:
    /// (a) at a fresh high the percentile is ~100 (the valve stays shut, so an up market is a no-op),
    ///     and after a crash it collapses (the valve opens);
    /// (b) it is TRAILING — a crash that happens LATER must not change an earlier cutoff's number, or
    ///     the valve would be reading the holdout it is supposed to be graded against.
    #[test]
    fn bench_range_pct_is_the_index_percentile_at_the_cutoff() {
        // 10 years of monthly bars climbing, then a 50% crash in the last year.
        let dates: Vec<NaiveDate> = (0..132u32).map(|k| ymd(2000 + k as i32 / 12, k % 12 + 1, 1)).collect();
        let mut closes: Vec<f64> = (0..120).map(|k| 100.0 * 1.005_f64.powi(k)).collect();
        let peak = *closes.last().unwrap();
        closes.extend((1..=12).map(|k| peak * (1.0 - 0.5 * k as f64 / 12.0)));
        let at_peak = bench_range_pct(&dates, &closes, ymd(2009, 12, 1)).unwrap();
        let at_bottom = bench_range_pct(&dates, &closes, ymd(2010, 12, 1)).unwrap();
        assert!(at_peak > 95.0, "at a fresh high the index ranks at the top of its own range: {at_peak}");
        assert!(at_bottom < 15.0, "after halving it ranks near the bottom: {at_bottom}");
        // (b) the no-peek property: the same cutoff, graded on a series that later crashes, reads the
        // same as one that stops at the cutoff.
        let cut = dates.iter().position(|d| *d > ymd(2009, 12, 1)).unwrap();
        let truncated = bench_range_pct(&dates[..cut], &closes[..cut], ymd(2009, 12, 1)).unwrap();
        assert!((at_peak - truncated).abs() < 1e-9, "a later crash moved an earlier cutoff: {at_peak} vs {truncated}");
        assert!(bench_range_pct(&dates, &closes, ymd(1990, 1, 1)).is_none(), "before the series -> no claim");
        // (#340) one bar in the window is no range: a percentile of a single close would read as a claim.
        assert!(bench_range_pct(&dates[..1], &closes[..1], ymd(2000, 1, 1)).is_none());
    }

    /// (#340) The benchmark leg's CAGR is the compounded rate, not the cumulative return split evenly:
    /// +21% over two years is 10%/yr. Every operator in the formula moves this one number.
    #[test]
    fn bench_leg_cagr_compounds_the_trailing_return() {
        let bench = (vec![ymd(2000, 1, 1), ymd(2002, 1, 1)], vec![100.0, 121.0]);
        let got = bench_leg_cagr(&bench, ymd(2002, 1, 1), 2).unwrap();
        assert!((got - 10.0).abs() < 1e-9, "{got}");
        assert!(bench_leg_cagr(&bench, ymd(2001, 1, 1), 2).is_none(), "no full window behind the date -> no leg");
    }

    /// (#326) The stamp gives every sample the market state of ITS OWN cutoff, not one number for the
    /// whole run — the property that makes a per-name pure gate regime-aware. Samples of the same date
    /// share a value, samples of different dates do not, and a sample the benchmark cannot reach keeps
    /// None (valve inert) rather than borrowing a neighbour's.
    #[test]
    fn stamp_regime_gives_each_sample_its_own_date() {
        let mut samples = synthetic_samples();
        samples.sort_by_key(|s| s.date);
        let dates: Vec<NaiveDate> = (0..40u32).map(|k| ymd(2000 + k as i32 / 4, k % 4 * 3 + 1, 1)).collect();
        // +2%/quarter, with a 40% break in the middle: a CONSTANT-growth series would hand every cutoff
        // the same trailing 1Y and the varying-state assert below could not fail.
        let closes: Vec<f64> = (0..40).map(|k| 100.0 * 1.02_f64.powi(k) * if (16..24).contains(&k) { 0.6 } else { 1.0 }).collect();
        stamp_regime(&mut samples, &(dates.clone(), closes));
        let first = samples.first().expect("samples").date;
        let by_date: BTreeMap<NaiveDate, Vec<Option<f64>>> =
            samples.iter().fold(BTreeMap::new(), |mut m, s| {
                m.entry(s.date).or_default().push(s.quote.bench_1y_pct);
                m
            });
        for (d, v) in &by_date {
            assert!(v.windows(2).all(|w| w[0] == w[1]), "one value per cutoff at {d}: {v:?}");
        }
        let stamped: Vec<Option<f64>> = by_date.values().map(|v| v[0]).collect();
        assert!(stamped.iter().filter(|v| v.is_some()).count() >= 2, "the benchmark must reach most cutoffs: {stamped:?}");
        assert!(
            stamped.iter().flatten().any(|v| (v - stamped.iter().flatten().next().expect("a stamped cutoff")).abs() > 1e-9),
            "different cutoffs must read different market states, not one run-wide number: {stamped:?}"
        );
        // (#1058) The first cutoff (1988-01) predates the series (2000-01), so neither stamp can be
        // measured there. The old `range none || 1y some` held on any 1Y value once the range read None.
        assert!(
            samples.iter().filter(|s| s.date == first).all(|s| s.quote.bench_1y_pct.is_none() && s.quote.bench_range_pct.is_none()),
            "a cutoff the series cannot reach back a full year from keeps None, not a neighbour's value"
        );
    }

    /// PRICE-RISK probe extractors: pass-through for consistency/worst-5y, and the underwater
    /// NEGATION pinned (years-underwater is bad-when-high, so the probe must see it sign-flipped
    /// to share the higher-is-better rho/edge convention — dropping the `-` silently reads the
    /// probe backwards). (r39) the Sortino pair is pinned to DIFFERENT denominators off one shared
    /// numerator — if both rows ever read the same field the comparison they exist to make is void
    /// and the run would print two identical rows as if they were evidence.
    #[test]
    fn risk_factor_extractors() {
        let mut q = Quote::stub("X", "1", "", "X");
        q.roll5y_pos_pct = Some(90.0);
        q.worst_5y_pct = Some(-12.0);
        q.underwater_yrs = Some(2.5);
        q.trend_cagr = Some(24.0);
        q.downside_dev_pct = Some(0.8);
        q.volatility_pct = Some(1.6);
        let get = |q: &Quote, name: &str| RISK_FACTORS.iter().find(|(n, _)| *n == name).unwrap().1(q);
        assert_eq!(get(&q, "consistency_5y"), Some(90.0));
        assert_eq!(get(&q, "worst_5y"), Some(-12.0));
        assert_eq!(get(&q, "underwater_neg"), Some(-2.5));
        assert_eq!(get(&q, "downside_dev_neg"), Some(-0.8));
        assert_eq!(get(&q, "sortino"), Some(30.0)); // 24.0 / 0.8 — down-moves only
        assert_eq!(get(&q, "sharpe_ref"), Some(15.0)); // 24.0 / 1.6 — every move, the incumbent
        // an all-positive stretch has NO measured downside: the name must leave the sample, not
        // arrive at +inf and dominate the rank correlation with what is really missing data.
        let mut flat = q.clone();
        flat.downside_dev_pct = Some(0.0);
        assert_eq!(get(&flat, "sortino"), None);
        assert_eq!(get(&flat, "downside_dev_neg"), Some(0.0)); // the raw stat is still a real 0
        assert!(RISK_FACTORS.iter().all(|(_, f)| f(&Quote::stub("Y", "1", "", "Y")).is_none()));
    }

    fn stub_verdict(years: i64, top: usize) -> Verdict {
        Verdict {
            date: "2026-07-19".into(),
            years,
            top,
            windows: 84,
            book: 14.3,
            excess: 6.9,
            win: 71.0,
            worst: -8.2,
            oos_early: 5.1,
            oos_late: 7.4,
            tuning_fp: "{\"a\":1}".into(),
            sized: None,
        }
    }

    /// The CORR-CAP trailing window, and the four-term price filter the goldens structurally cannot
    /// reach: every fixture close is positive and finite, so the filter never has to refuse anything,
    /// and the audit duly reported its `&&`, `>` and `is_finite` mutants all surviving. Each poke
    /// below is a price no healthy series contains, and each one must silently cost TWO rows — the
    /// month that divides BY it, and the month that divides INTO it.
    #[test]
    fn trailing_returns_clamps_its_window_and_refuses_bad_prices() {
        // Exactly 1% per month, so every clean return is 1.0 and any admitted junk stands out.
        let clean: Vec<f64> = (0..50).map(|k| 1.01_f64.powi(k)).collect();

        assert_eq!(trailing_returns(&clean, 40).len(), 36, "36 months back");
        assert_eq!(trailing_returns(&clean, 10).len(), 10, "clamp at the series start, never underflow");
        assert!(trailing_returns(&clean, 0).is_empty(), "no history at index 0");
        assert_eq!(trailing_returns(&clean, 40), trailing_returns(&clean[..=40], 40), "(#529) nothing past the cutoff is read");
        for r in trailing_returns(&clean, 40) {
            assert!((r - 1.0).abs() < 1e-9, "a 1%/month series must yield 1.0, got {r}");
        }

        // Each of these is admitted by SOME single-term mutation of the filter, and each produces a
        // number that would go straight into a correlation:
        //   0.0 base    -> `>`->`>=` or `&&`->`||` admits it, and x / 0.0 is +inf
        //   0.0 forward -> `>`->`>=` on the forward term books a FINITE -100%, so "no inf" misses it
        //   inf         -> `is_finite`->`true` admits it; inf as a base books -100%, as a forward +inf
        //   NaN         -> caught by `is_finite`; every comparison against it is false
        for bad in [0.0, -1.0, f64::INFINITY, f64::NAN] {
            let mut c = clean.clone();
            c[20] = bad;
            let got = trailing_returns(&c, 40);
            assert_eq!(got.len(), 34, "close {bad} must drop both months touching it, not just one");
            assert!(got.iter().all(|r| r.is_finite()), "close {bad} leaked a non-finite return: {got:?}");
            assert!(got.iter().all(|r| *r > -100.0), "close {bad} booked a fabricated wipeout: {got:?}");
        }
    }

    /// The four `run` predicates the round-4 mutation audit found unreachable. Each was an operator
    /// sitting inline in a 600-line async body that opens sockets, so no offline pin could touch it —
    /// 10 of the 19 survivors were these. One test, because they are one finding.
    #[test]
    fn run_predicates_hold_their_thresholds() {
        // the rayon cap: 0 means "leave rayon alone", never "a pool of zero threads"
        assert_eq!(thread_cap(0), None, "the default must not pin a pool");
        assert_eq!(thread_cap(1), Some(1));
        assert_eq!(thread_cap(8), Some(8));

        // the correlation floor: 4 is enough, 3 is not
        assert!(too_few_samples(3));
        assert!(!too_few_samples(MIN_SAMPLES), "the floor is inclusive — 4 samples correlate");
        assert!(!too_few_samples(500));

        // the verdict journal: wide AND deep. A watchlist run must never publish over the gate's
        // wide one, and a throttled wide run that resolved too few names must not either.
        assert!(may_write_verdict(true, false, MIN_VERDICT_TICKERS));
        assert!(!may_write_verdict(false, false, MIN_VERDICT_TICKERS), "a watchlist run must not publish");
        assert!(!may_write_verdict(true, false, MIN_VERDICT_TICKERS - 1), "a thin wide run is a throttled one");
        // (PIT) and the third refusal, which a big enough sample must NOT override: a point-in-time run
        // is wide and deep and still must not journal, because it measures a pool the screen never ranks.
        assert!(!may_write_verdict(true, true, MIN_VERDICT_TICKERS), "a PIT run is a different claim, not a better sample");
        assert!(!may_write_verdict(true, true, MIN_VERDICT_TICKERS * 10));
        // (#788) the SIZED twin is that PIT run, deep enough, AND with the dead names walked
        assert!(may_write_sized(true, true, true, false, true, MIN_VERDICT_TICKERS));
        assert!(!may_write_sized(true, true, false, false, true, MIN_VERDICT_TICKERS), "a survivor-only PIT run");
        assert!(!may_write_sized(true, false, true, false, true, MIN_VERDICT_TICKERS), "not a PIT run");
        assert!(!may_write_sized(true, true, true, false, true, MIN_VERDICT_TICKERS - 1), "a thin one");
        // (#1061) and with `fund`, or the PEG gate never fires and the record grades another book
        assert!(!may_write_sized(true, true, true, false, false, MIN_VERDICT_TICKERS), "a price-only PIT run");
        // (#1062) and with the crash cut the shipped ruler charges, never the `deadflat` probe's none
        assert!(!may_write_sized(true, true, true, true, true, MIN_VERDICT_TICKERS), "the no-crash-cut arm");
        // and the refusal SAYS WHICH, or a reader debugs the ticker count that was never the problem
        assert!(no_verdict_reason(true, MIN_VERDICT_TICKERS).contains("POINT-IN-TIME"));
        assert!(no_verdict_reason(false, 3).contains("only 3 tickers resolved"));
        assert!(!no_verdict_reason(false, 3).contains("POINT-IN-TIME"));

        // the fundamental lane: either flag turns it on, not both
        assert!(fund_lane_on(true, false));
        assert!(fund_lane_on(false, true));
        assert!(fund_lane_on(true, true));
        assert!(!fund_lane_on(false, false));

        // (#364) the FMP-key warning: only `fund` on the FMP source with no usable key
        assert!(fmp_key_missing(true, "fmp", None));
        assert!(fmp_key_missing(true, "fmp", Some(String::new())), "an empty key is no key");
        assert!(!fmp_key_missing(true, "fmp", Some("k".into())));
        assert!(!fmp_key_missing(true, "sec", None), "SEC is keyless");
        assert!(!fmp_key_missing(false, "fmp", None));
    }

    /// The cross-currency path, which no frozen-data golden reaches: every fixture ticker quotes and
    /// files in one currency, so the FX guard and the conversion itself were both unpinned.
    #[test]
    fn fx_converts_only_across_books_and_only_by_multiplying() {
        // same books, or a side unknown -> no conversion at all
        assert_eq!(fx_pair(Some("USD"), Some("USD")), None);
        assert_eq!(fx_pair(Some("USD"), None), None);
        assert_eq!(fx_pair(None, Some("EUR")), None);
        assert_eq!(fx_pair(None, None), None);
        // genuinely different books -> the pair, in (quote, filer) order
        assert_eq!(fx_pair(Some("EUR"), Some("USD")), Some(("EUR", "USD")));

        // the multiply. `+` and `/` both survived here: at a rate near 1.0 the wrong operator still
        // prints a plausible number, so the rate is deliberately far from 1.
        assert_eq!(px_in_filer_ccy(50.0, Some(4.0)), Some(200.0));
        assert_eq!(px_in_filer_ccy(50.0, None), None, "no as-of rate must not fall back to the raw close");
    }

    /// EVERY arm of the command line, because until the mutation audit none of them had a caller.
    /// The parse lived inline in `run()`, which opens sockets, so nothing in the repo could reach it:
    /// turning off the arms that recognise `universe`, `long`, `fund` or `insider` left all six
    /// frozen-data goldens green, since those only ever pass `12`/`20`/`8`/`tune`/`halflife`/`stress`.
    ///
    /// `universe` is the one that matters most. It gates the WIDE run — the CI gate's entire
    /// subject — and it also gates the verdict journal (`wide && tickers.len() >= MIN_VERDICT_TICKERS`)
    /// that the screen footer quotes. Lose that keyword and `backtest 12 universe` quietly becomes a
    /// backtest of one ticker literally named "universe".
    #[test]
    fn args_parse_every_keyword() {
        let p = |s: &str| parse_args(&s.split_whitespace().map(String::from).collect::<Vec<_>>());

        // default: no args at all
        assert_eq!(p(""), Args { years: 5, draws: PLACEBO_DRAWS, configs: SEARCH_CONFIGS, nulls: SEARCH_NULLS, ..Default::default() });

        // each keyword on its own, so one arm going dead cannot hide behind another
        assert!(p("universe").wide, "`universe` must set wide — the CI gate IS the wide run");
        assert!(p("long").long);
        assert!(p("fund").fund);
        assert!(p("tune").tune);
        assert!(p("insider").insider);
        assert!(p("holders").holders);
        assert!(p("pac").pac);
        assert!(p("failures").failures);
        assert!(p("rivals").rivals);
        assert!(p("winners").winners);
        assert!(p("halflife").halflife);
        assert!(p("stress").stress);
        assert!(p("pit").pit, "`pit` must set pit — lose it and the point-in-time run is a plain one that says it isn't");
        assert!(p("dead").dead && !p("dead").pit && !p("pit").dead);
        assert!(
            p("deadflat").dead && p("deadflat").dead_flat && !p("dead").dead_flat,
            "(#752) deadflat is the dead arm, uncut"
        );
        assert!(p("intraday").intraday && !p("intraday").wide && !p("12").intraday);
        assert!(p("holdwindows").holdwindows && !p("holdwindows").intraday && !p("12").holdwindows);
        assert!(p("traderules").traderules && !p("traderules").holdwindows && !p("12").traderules);
        assert!(p("leverage").leverage && !p("traderules").leverage && !p("leverage").traderules);
        assert!(p("lifecycle").lifecycle && !p("leverage").lifecycle && !p("lifecycle").leverage);
        assert!(p("bestpick").bestpick && !p("lifecycle").bestpick && !p("bestpick").lifecycle);
        assert!(p("rolls").rolls && !p("bestpick").rolls && !p("rolls").bestpick);
        assert!(p("options").options && !p("rolls").options && !p("options").rolls);
        assert!(p("rescore a b").rescore && p("rescore a b").tickers == ["a", "b"] && !p("options").rescore && !p("rescore").options);
        assert!(p("draws=5000").draws == 5000 && p("draws=5000").tickers.is_empty(), "(#1227) draws=N is a count, not a ticker");
        assert!(std::panic::catch_unwind(|| p("draws=0")).is_err() && std::panic::catch_unwind(|| p("draws=x")).is_err(), "a draw count must be a whole number above 0");
        assert!(p("level").level && p("LEVEL").level && !p("options").level && p("level").tickers.is_empty(), "(#1238) `level` is a keyword, not a ticker");
        let s = p("search a b configs=7 nulls=0");
        assert!(s.search && !p("level").search && s.tickers == ["a", "b"] && (s.configs, s.nulls) == (7, 0), "(#1239) search takes dumps, a config count and a null count");
        assert!(std::panic::catch_unwind(|| p("configs=0")).is_err() && std::panic::catch_unwind(|| p("nulls=x")).is_err(), "configs must be above 0, nulls a whole number");
        assert!(p("time").time && p("TIME").time && !p("12").time && p("time").tickers.is_empty(), "(#1118) `time` is a keyword, not a ticker");
        // case-insensitive, as every arm claims via eq_ignore_ascii_case
        assert!(p("UNIVERSE").wide && p("Stress").stress && p("HalfLife").halflife && p("PIT").pit);
        // and each keyword sets ONLY its own flag: `pit` and `stress` are opposite treatments of the
        // same bias (remove it vs estimate it) and an arm that set both would silently conflate them.
        assert!(!p("pit").stress && !p("stress").pit);

        // the numeric arm: first positive integer wins, and it is NOT a ticker
        assert_eq!(p("12").years, 12);
        assert!(p("12").tickers.is_empty());
        assert_eq!(p("12 universe").years, 12);
        assert!(p("12 universe").wide);

        // `tickers.is_empty()` guards the numeric arm: once a ticker has been seen, a later number is
        // a TICKER, not a re-read of the horizon. Both halves of that `&&` are load-bearing, and the
        // audit flagged the whole condition as replaceable by `true` and by `||` without any golden
        // noticing, so each half gets its own assertion.
        let after = p("AAPL 12");
        assert_eq!(after.years, 5, "a number AFTER a ticker must not silently re-set the horizon");
        assert_eq!(after.tickers, vec!["AAPL", "12"]);
        // `y > 0`: zero and negative horizons are not horizons. `12 -3` keeps 12 and treats -3 as a name.
        assert_eq!(p("0").years, 5, "a zero horizon must be rejected, not accepted as 0 years");
        assert_eq!(p("0").tickers, vec!["0"]);
        assert_eq!(p("-3").years, 5);
        // (#948) and past 100 years is a typo, not a walk: 100 is a horizon, 101 and a date are names.
        assert_eq!(p("100").years, 100);
        assert_eq!(p("101").years, 5);
        assert_eq!(p("20260101").tickers, vec!["20260101"]);

        // everything unrecognised is a ticker, in order, unchanged
        let mixed = p("AAPL MSFT tune 20");
        assert!(mixed.tune);
        assert_eq!(mixed.years, 5, "20 arrives after two tickers, so it is a name too");
        assert_eq!(mixed.tickers, vec!["AAPL", "MSFT", "20"], "keywords are consumed, everything else is a ticker in order");

        // all flags at once still parse independently
        let all = p("8 universe long fund tune insider holders pac rivals winners halflife stress pit dead deadflat failures intraday holdwindows traderules leverage lifecycle bestpick rolls options rescore level draws=9 search configs=3 nulls=4 time");
        assert_eq!(
            all,
            Args {
                years: 8,
                wide: true,
                long: true,
                fund: true,
                tune: true,
                insider: true,
                holders: true,
                pac: true,
                rivals: true,
                winners: true,
                halflife: true,
                stress: true,
                pit: true,
                dead: true,
                dead_flat: true,
                failures: true,
                intraday: true,
                holdwindows: true,
                traderules: true,
                leverage: true,
                lifecycle: true,
                bestpick: true,
                rolls: true,
                options: true,
                rescore: true,
                level: true,
                draws: 9,
                search: true,
                configs: 3,
                nulls: 4,
                time: true,
                tickers: vec![]
            }
        );
    }

    /// (#583) Every stock lands in exactly one leak row, and only winners carry tags. Bucket 1 holds a
    /// clean top-10 (C0..C9, C1/C2 dear-ish but over the PEG floor), a clean C10 at the cut that loses on
    /// sort order, and one leaker per row; bucket 2 books nobody, so it files nothing.
    #[test]
    fn winner_leaks_file_each_stock_by_what_keeps_it_out() {
        let legs = |pairs: &[(&str, f64)]| -> Vec<Option<(String, f64)>> {
            crate::core::HORIZONS
                .iter()
                .map(|(l, _)| pairs.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
                .collect()
        };
        let t = BuyHeuristic { growth_gate_soft: 0.8, growth_min_age_years: 10.0, growth_value_floor_pct: 40.0, ..Default::default() };
        let q = |tk: &str| {
            let mut q = Quote::stub(tk, "€100.00", "", "Test Corp");
            q.instrument_type = "EQUITY".into();
            q.avg_turnover_eur = Some(1e9);
            q.range_pct = 90.0;
            q.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("5Y", 200.0)]);
            q
        };
        let clean: Vec<Quote> = (0..11).map(|i| q(&format!("C{i}"))).collect();
        let mut refused = q("REF");
        refused.avg_turnover_eur = None;
        let mut brk = q("BRK");
        brk.above_ma_pct = 96.0;
        let mut dock = q("DOCK");
        dock.age_years = Some(5.0);
        let mut both = q("BOTH");
        (both.age_years, both.above_ma_pct) = (Some(5.0), 96.0);
        let mut blind = q("BLIND");
        (blind.range_pct, blind.above_ma_pct) = (50.0, 96.0);
        let val = q("VAL");
        let mut fund = q("FUND");
        (fund.instrument_type, fund.range_pct) = ("ETF".into(), 50.0);
        let mut ref2 = q("REF2");
        ref2.avg_turnover_eur = None;

        // preconditions: each leaker reaches the cut (a clean name's score) only with its own relief
        let brake = BuyHeuristic { growth_overext_cap: 0.0, ..t.clone() };
        let relief = BuyHeuristic { growth_gate_soft: 1.0, ..t.clone() };
        let free = BuyHeuristic { growth_gate_soft: 1.0, ..brake.clone() };
        let cut = growth_score(&clean[0], &t).expect("a clean name scores");
        let at = |x: &Quote, t: &BuyHeuristic| growth_score(x, t).is_some_and(|s| s >= cut);
        assert_eq!(picks::refusal_reason(&refused), Some("no-turnover"));
        assert!(growth_score(&refused, &free).is_none() && growth_score(&fund, &t).is_none());
        assert_eq!(picks::gate_failures(&brk, &t).map(|f| f.len()), Some(0), "the brake is a score factor, not a gate");
        assert!(growth_score(&brk, &t).is_some_and(|s| s < cut));
        assert_eq!(growth_score(&brk, &brake), Some(cut), "brake off, BRK scores exactly the cut");
        for (x, dock_alone) in [(&dock, true), (&both, false)] {
            assert!(growth_score(x, &t).is_some_and(|s| s < cut) && !at(x, &brake));
            assert_eq!(at(x, &relief), dock_alone);
            assert_eq!(growth_score(x, &free), Some(cut));
        }
        assert!(growth_score(&blind, &t).is_some() && !at(&blind, &free), "still under the cut with both off");

        let mut b1: Vec<(&Quote, f64, Option<f64>)> = clean.iter().zip([1000.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -50.0]).map(|(c, r)| (c, r, None)).collect();
        (b1[1].2, b1[2].2) = (Some(5.0), Some(5.0));
        b1.extend([
            (&refused, 900.0, None),
            (&brk, 800.0, None),
            (&dock, 700.0, None),
            (&both, 600.0, None),
            (&blind, 500.0, None),
            (&val, 400.0, Some(1.0)), // under the 40th-pct PEG floor of [1, 5, 5]
            (&fund, 5000.0, None),    // not a stock, so never filed and never a winner
        ]);
        let groups = BTreeMap::from([(1, b1), (2, vec![(&ref2, 999.0, None)])]);
        // 17 stocks, top 40% -> 7 winners down to VAL's 400; the book's multiple is (11 + 9) / 10 = 2, +100%
        let got = winner_leaks(&groups, 1, 40.0, &t);
        let tags = |xs: &[&'static str]| xs.iter().map(|x| (*x, 1)).collect::<BTreeMap<_, _>>();
        let leak = |all: Vec<f64>, winners: Vec<f64>, tags: BTreeMap<&'static str, usize>| Leak { all, winners, tags };
        let mut held = vec![900.0];
        held.extend([-100.0; 9]);
        let blind_tags: Vec<&'static str> = picks::gate_failures(&blind, &t).expect("assessable").iter().map(|g| g.0).collect();
        assert!(!blind_tags.is_empty());
        assert_eq!(
            got,
            [
                leak(held, vec![900.0], tags(&[])),
                leak(vec![800.0], vec![800.0], tags(&["no-turnover"])),
                leak(vec![300.0], vec![300.0], tags(&[])),
                leak(vec![700.0], vec![700.0], tags(&[])),
                leak(vec![600.0], vec![600.0], tags(&["young"])),
                leak(vec![500.0], vec![500.0], tags(&["young"])),
                leak(vec![-150.0, 400.0], vec![400.0], tags(&blind_tags)),
            ]
        );
    }

    /// (#606) The blend's held winners, with 8 momentum slots so the score half is 2. Bucket 1: a stale S1 row
    /// gives way to the latest; S1 (score half) and M1 (first momentum slot, exactly at the bar) are held winners;
    /// N1 is a winner neither half holds; (#607) the ETF E1 out-ranks M2 on momentum and still takes no slot, as
    /// live. Bucket 2 has no stock, so it books nothing. Bucket 3's score half is empty, so momentum fills every slot.
    /// (#630) Bucket 4, 5 slots: momentum's #2 is the score half's #4, so S4 is held as momentum's, and S6 enters the
    /// book in place of M6, momentum's #6, which the head-then-fill book held.
    #[test]
    fn blend_winners_split_held_winners_by_half() {
        assert_eq!(winner_bar(vec![10.0, 40.0, 30.0, 20.0], 50.0), Some(30.0));
        // (#867) 1.2 places rounds UP to the 2nd best; at least the best one wins; at 100% the bar is the worst
        assert_eq!(winner_bar(vec![10.0, 40.0, 30.0, 20.0], 30.0), Some(30.0));
        assert_eq!(winner_bar(vec![10.0, 40.0, 30.0, 20.0], 0.0), Some(40.0));
        assert_eq!(winner_bar(vec![10.0, 40.0, 30.0, 20.0], 100.0), Some(10.0));
        assert_eq!(winner_bar(Vec::new(), 5.0), None);
        let (a, b, c) = (ymd(2010, 1, 5), ymd(2011, 1, 6), ymd(2012, 1, 7));
        // shipped score = `score`, open score = `score + add` (`swap_scorer`); the momentum key is the 1Y leg
        let s = |date, tk: &str, score: f64, add: f64, one_year: Option<f64>, realized: f64| {
            let mut x = swap_sample(date, tk, score, add, realized);
            Arc::make_mut(&mut x.quote).perf =
                crate::core::HORIZONS.iter().map(|(l, _)| one_year.filter(|_| *l == "1Y").map(|v| ("x".to_string(), v))).collect();
            x
        };
        let etf = |mut x: Sample| {
            Arc::make_mut(&mut x.quote).instrument_type = "ETF".into();
            x
        };
        let b1 = [
            s(ymd(2010, 1, 4), "S1", 5.0, 0.0, None, -50.0),
            s(a, "S1", 5.0, 0.0, None, 100.0),
            s(a, "S2", 4.0, 0.0, None, 1.0),
            s(a, "S3", 3.0, 0.0, Some(10.0), 20.0),
            s(a, "M1", -1.0, 3.0, Some(50.0), 90.0),
            s(a, "M2", -1.0, 3.0, Some(40.0), 10.0),
            etf(s(a, "E1", -1.0, 3.0, Some(45.0), 500.0)),
            s(a, "N1", -1.0, 0.0, None, 95.0),
        ];
        let e2 = etf(s(b, "E2", 5.0, 0.0, Some(10.0), 999.0));
        let b3 = [s(c, "T1", -1.0, 3.0, Some(20.0), 30.0), s(c, "T2", -1.0, 3.0, Some(10.0), 5.0)];
        let groups = BTreeMap::from([(bucket(a), b1.iter().collect()), (bucket(b), vec![&e2]), (bucket(c), b3.iter().collect())]);
        let shipped = BuyHeuristic { quality_weight: 0.0, growth_momentum_slots: 8, ..BuyHeuristic::default() };
        let open = BuyHeuristic { quality_weight: 1.0, ..shipped.clone() };
        // bucket 1: 6 stocks, top 50% down to M1's 90; the book is S1 S2 | M1 M2 S3. Bucket 3: bar T1's 30, book T1 T2
        let (slots, held, mom) = blend_winners(&groups, 50.0, swap_scorer, &shipped, &open);
        assert_eq!(slots, 7);
        assert_eq!(held, BTreeMap::from([("M1", [0, 1]), ("S1", [1, 0]), ("T1", [0, 1])]));
        let tickers = |v: Vec<&Sample>| {
            let mut t: Vec<String> = v.iter().map(|s| s.quote.ticker.clone()).collect();
            t.sort();
            t
        };
        assert_eq!(tickers(mom), ["M1", "M2", "S3", "T1", "T2"], "(#740) the momentum half's slots");
        // (#630) 12 stocks, bar S4's 800 (a tie at the bar holds): the book is S1 M1 S2 S4 S3 M3 S5 M4 S6 M5, M6 (900) out
        let d = ymd(2013, 1, 8);
        let b4 = [
            s(d, "S1", 7.0, 0.0, None, 1.0),
            s(d, "S2", 6.0, 0.0, None, 2.0),
            s(d, "S3", 5.0, 0.0, None, 3.0),
            s(d, "S4", 4.0, 0.0, Some(40.0), 800.0),
            s(d, "S5", 3.0, 0.0, None, 5.0),
            s(d, "S6", 2.0, 0.0, None, 1000.0),
            s(d, "S7", 1.0, 0.0, None, 7.0),
            s(d, "M1", -1.0, 3.0, Some(50.0), 8.0),
            s(d, "M3", -1.0, 3.0, Some(30.0), 9.0),
            s(d, "M4", -1.0, 3.0, Some(20.0), 10.0),
            s(d, "M5", -1.0, 3.0, Some(15.0), 11.0),
            s(d, "M6", -1.0, 3.0, Some(10.0), 900.0),
        ];
        let five = BuyHeuristic { growth_momentum_slots: 5, ..shipped };
        let open = BuyHeuristic { quality_weight: 1.0, ..five.clone() };
        let (slots, held, mom) = blend_winners(&BTreeMap::from([(bucket(d), b4.iter().collect::<Vec<_>>())]), 25.0, swap_scorer, &five, &open);
        assert_eq!(slots, 10);
        assert_eq!(held, BTreeMap::from([("S4", [0, 1]), ("S6", [1, 0])]));
        assert_eq!(tickers(mom), ["M1", "M3", "M4", "M5", "S4"]);
    }

    /// (#560) The rival books' arithmetic: the tie band, the 12-1 formula, rank ties, the book's dedupe and
    /// ticker tie-break, and a seeded random pond.
    #[test]
    fn rival_books_pick_rank_and_draw_as_documented() {
        assert!(rival_wins(0.2, 0.2), "a gap AT the tie band is a win");
        assert!(!rival_wins(0.19, 5.0) && !rival_wins(5.0, 0.19), "mean AND median must both clear");

        let quote = |pairs: &[(&str, f64)]| {
            let mut q = Quote::stub("M", "1", "", "M");
            q.perf = crate::core::HORIZONS
                .iter()
                .map(|(l, _)| pairs.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
                .collect();
            q
        };
        let mom = |pairs: &[(&str, f64)]| picks::momentum_12_1(&quote(pairs));
        assert!((mom(&[("1Y", 20.0), ("1M", 10.0)]).unwrap() - (1.2 / 1.1 - 1.0) * 100.0).abs() < 1e-9);
        assert!((mom(&[("1Y", 20.0)]).unwrap() - 20.0).abs() < 1e-9, "a missing 1M nets nothing out");
        assert_eq!(mom(&[("1M", 5.0)]), None, "no 1Y, no momentum");
        assert!((mom(&[("1Y", 20.0), ("1M", -100.0)]).unwrap() - 11_900.0).abs() < 1e-6, "a wiped month is floored at 1%");

        assert_eq!(rank_pct(&[Some(3.0), None, Some(1.0), Some(3.0)]), vec![Some(1.0 / 3.0), None, Some(0.0), Some(1.0 / 3.0)]);

        assert_eq!(book_excess(&[], 1), None);
        // years 1: the book doubles (+100%), the index gains 50% -> +50 pts
        assert!((book_excess(&[(300.0, 100.0), (-100.0, 0.0)], 1).unwrap() - 50.0).abs() < 1e-9);
        let rows = [
            (Some(9.0), 300.0, 100.0, "C"),
            (Some(8.0), -50.0, 0.0, "C"), // the same name again: one holding, so B/A refill
            (Some(5.0), 100.0, 0.0, "B"),
            (Some(5.0), 0.0, 0.0, "A"), // ties B on key, wins on ticker
            (None, 1000.0, 0.0, "Z"),   // no key: never in this book
        ];
        // C + A: (4 + 1)/2 = 2.5x vs (2 + 1)/2 = 1.5x -> +150 - +50
        assert!((rival_excess(&rows, 2, 1).unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(rival_excess(&[(None, 1.0, 1.0, "Z")], 2, 1), None);

        // (#584) blend: `first` keys A, B, C (Z has no key); `second` keys A, then D before E on ticker, then C
        let first = [(Some(3.0), 100.0, 0.0, "A"), (Some(2.0), 0.0, 0.0, "B"), (Some(1.0), 900.0, 0.0, "C"), (None, 5000.0, 0.0, "Z")];
        let second = [(Some(9.0), 100.0, 0.0, "A"), (Some(1.0), -100.0, 0.0, "E"), (Some(1.0), 300.0, 0.0, "D"), (Some(0.5), 900.0, 0.0, "C")];
        // (#630) 5 of 10 places are `second`'s, as the page splices: A, D (`second`'s A is held), B, E, C -> (2 + 4 + 1 + 0 + 10)/5 -> +240
        assert!((blend_excess(&first, &second, 5, 10, 1).unwrap() - 240.0).abs() < 1e-9);
        // a thin first side: A alone, its other places go to `second`: D, E, C -> (2 + 4 + 0 + 10)/4 -> +300
        assert!((blend_excess(&first[..1], &second, 5, 10, 1).unwrap() - 300.0).abs() < 1e-9);
        // `rise`'s #2 is `ship`'s #4, S4: it books on `rise`'s own row (+90) and `ship` runs on, so S6 enters and M6 does not.
        // The book S1 M1 S2 S4 S3 M3 S5 M4 S6 M5 -> 2.12x against an index at 1.1x -> +102 (head-then-fill had +47)
        let ship = [
            (Some(7.0), 0.0, 100.0, "S1"),
            (Some(6.0), 0.0, 0.0, "S2"),
            (Some(5.0), 0.0, 0.0, "S3"),
            (Some(4.0), 40.0, 0.0, "S4"),
            (Some(3.0), 0.0, 0.0, "S5"),
            (Some(2.0), 1000.0, 0.0, "S6"),
            (Some(1.0), 700.0, 0.0, "S7"),
        ];
        let rise = [
            (Some(9.0), 0.0, 0.0, "M1"),
            (Some(8.0), 90.0, 0.0, "S4"),
            (Some(7.0), 0.0, 0.0, "M3"),
            (Some(6.0), 0.0, 0.0, "M4"),
            (Some(5.0), 30.0, 0.0, "M5"),
            (Some(4.0), 500.0, 0.0, "M6"),
        ];
        assert!((blend_excess(&ship, &rise, 5, 10, 1).unwrap() - 102.0).abs() < 1e-9);
        // a thin `rise`, M1 alone: its other places go to `ship`, which runs on to S7 -> 3.175x against 1.125x -> +205 (head-then-fill had -10)
        assert!((blend_excess(&ship, &rise[..1], 5, 10, 1).unwrap() - 205.0).abs() < 1e-9);
        assert_eq!(blend_excess(&first[3..], &[], 5, 10, 1), None, "no keyed name on either side, no book");
        // (#600) `first`'s top 2 x 1 keyed names are A and B (Z has no key), so C loses its `then` key; skipping
        // A, the top 2 x 2 run out at B and C. (l)'s key: 3 is above 1 of 2, 2 above 1 of 3.
        let none = HashSet::new();
        assert_eq!(top_names(&first, 2, &none), HashSet::from(["A", "B"]));
        assert_eq!(top_names(&first, 2, &HashSet::from(["A"])), HashSet::from(["B", "C"]), "a skipped name is no pick");
        let then = [Some(1.0), Some(2.0), Some(3.0), Some(4.0)];
        assert_eq!(double_sort(&first, &then, 1, &none), [Some(1.0), Some(2.0), None, None]);
        assert_eq!(double_sort(&first, &then, 2, &HashSet::from(["A"])), [None, Some(2.0), Some(3.0), None]);
        assert_eq!(rank_sum(&[Some(3.0), Some(1.0), None], &[Some(1.0), Some(2.0), Some(5.0)]), [Some(0.5), Some(1.0 / 3.0), None]);

        let flat: BTreeMap<i32, Vec<(f64, f64)>> = [(1, vec![(100.0, 0.0); 3]), (2, vec![(100.0, 0.0)]), (3, vec![])].into();
        let draws = random_books(&flat, 10, 1, PLACEBO_DRAWS);
        assert_eq!(draws.len(), PLACEBO_DRAWS);
        assert_eq!(random_books(&flat, 10, 1, 7).len(), 7, "(#1227) the count is the caller's");
        assert!(draws.iter().all(|d| (d.0 - 100.0).abs() < 1e-9 && (d.1 - 100.0).abs() < 1e-9), "every draw of a flat pond is that pond");
        let two: BTreeMap<i32, Vec<(f64, f64)>> = [(1, vec![(100.0, 0.0), (0.0, 0.0)])].into();
        let draws = random_books(&two, 1, 1, PLACEBO_DRAWS);
        assert_eq!(draws, random_books(&two, 1, 1, PLACEBO_DRAWS), "seeded: the same pond draws the same books");
        assert!(draws.iter().any(|d| d.0 > 50.0) && draws.iter().any(|d| d.0 < 50.0), "both names get drawn");
        // (#1149) two of three draws reach 2.0 (a tie counts), so (2 + 1) / 4; above every draw is the 1/(n+1) floor
        assert_eq!(p_at_least(&[1.0, 2.0, 3.0], 2.0), 0.75);
        assert_eq!(p_at_least(&[1.0, 2.0, 3.0], 9.0), 0.25);
    }

    /// (#560) `rival_books` on a stand-in scorer (shipped = `range_pct`, open adds `avg_turnover_eur`).
    /// 2010: S1 is in both lanes, S2 only gates-off, N1 in neither, the coin never. 2011: only the open
    /// lane scores, so the bucket sits out. 2012: no index window, so nobody is in it. Index +10%.
    #[test]
    fn rival_books_key_each_book_on_one_pond() {
        use crate::core::FundFactors;
        let (a, b, c) = (ymd(2010, 1, 5), ymd(2011, 1, 6), ymd(2012, 1, 7));
        let bench = (vec![a, b, c], vec![100.0, 110.0, 110.0]);
        let with = |mut s: Sample, f: FundFactors, one_year: Option<f64>| {
            let mut q = (*s.quote).clone();
            q.tr_cagr = (s.quote.ticker == "S2").then_some(50.0); // (#571) S2 alone keys (h); no 10Y leg, so (i) is empty
            q.perf = crate::core::HORIZONS.iter().map(|(l, _)| one_year.filter(|_| *l == "1Y").map(|v| ("x".to_string(), v))).collect();
            s.quote = Arc::new(q);
            s.fund = Some(f);
            s
        };
        let samples = vec![
            with(swap_sample(a, "S1", 5.0, 0.0, 30.0), FundFactors { quality: Some(1.0), earnings_yield: Some(4.0), ..Default::default() }, Some(15.0)),
            with(swap_sample(a, "S2", -1.0, 3.0, 10.0), FundFactors { roe: Some(9.0), rev_cagr: Some(12.0), ..Default::default() }, None),
            swap_sample(a, "N1", -1.0, 0.0, 999.0),
            swap_sample(a, "BTC-EUR", 100.0, 100.0, 1000.0),
            swap_sample(b, "O1", -1.0, 3.0, 50.0),
            swap_sample(c, "S3", 5.0, 0.0, 70.0),
        ];
        let shipped = BuyHeuristic { quality_weight: 0.0, ..BuyHeuristic::default() };
        let open = BuyHeuristic { quality_weight: 1.0, ..BuyHeuristic::default() };
        let (books, pond) = rival_books(&samples, &bench, 1, swap_scorer, &shipped, &open);
        // S1 alone: +30 vs +10 -> 20. S1 + S2: +20 -> 10. S2 alone: 0. (#598) (k): S2 by TR life, then S1 by momentum.
        // (#600) (l)/(m)/(n): S2 has no 1Y, so no momentum key, and S1 is alone in all three.
        let want: [&[f64]; 14] = [&[20.0], &[10.0], &[20.0], &[10.0], &[20.0], &[0.0], &[20.0], &[0.0], &[], &[20.0], &[10.0], &[20.0], &[20.0], &[20.0]];
        assert_eq!(books.len(), RIVALS.len());
        for (i, (got, want)) in books.iter().zip(want).enumerate() {
            assert_eq!(got.len(), want.len(), "book {i}: {got:?}");
            assert!(got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-9), "book {i}: {got:?} vs {want:?}");
        }
        assert_eq!(pond.keys().copied().collect::<Vec<_>>(), vec![bucket(a)], "2011 has no shipped name, 2012 no window");
        let round = |p: &(f64, f64)| (p.0.round(), p.1.round());
        assert_eq!(pond[&bucket(a)].iter().map(round).collect::<Vec<_>>(), vec![(30.0, 10.0), (10.0, 10.0)], "N1 and the coin stay out");
    }

    /// (#560) The DCA lane's momentum order keys only names the gates-off score keys and that carry a 1Y,
    /// highest 12-1 first, a tie in pool (ticker) order; the top `n` split one euro equally.
    #[test]
    fn momentum_dca_book_ranks_the_open_pond() {
        let s = |tk: &str, one_year: Option<f64>| {
            let mut q = Quote::stub(tk, "1", "", tk);
            q.perf = crate::core::HORIZONS.iter().map(|(l, _)| one_year.filter(|_| *l == "1Y").map(|v| ("x".to_string(), v))).collect();
            Sample { quote: Arc::new(q), ..sample(ymd(2020, 6, 1), 0.0) }
        };
        let pool = [s("A", Some(10.0)), s("B", Some(30.0)), s("C", Some(10.0)), s("D", None), s("E", Some(99.0))];
        let refs: Vec<&Sample> = pool.iter().collect();
        let order = momentum_ranked(&refs, &[Some(1.0), Some(1.0), Some(1.0), Some(1.0), None], &BuyHeuristic::default());
        let tickers: Vec<&str> = order.iter().map(|x| x.quote.ticker.as_str()).collect();
        assert_eq!(tickers, ["B", "A", "C"], "E is outside the pond, D has no 1Y, A ties C and goes first");
        assert_eq!(equal_top(order.iter().copied(), 2), vec![("B", 0.5), ("A", 0.5)]);
        assert_eq!(equal_top(order.iter().copied(), 9).len(), 3);
        assert!(equal_top(std::iter::empty(), 10).is_empty());
        // (#584) the blend: B first, then A as `fill`'s; shipped's A is held, so C, `fill`'s next, takes the third place;
        // an empty shipped side leaves every place to `fill`
        let third = 1.0 / 3.0;
        assert_eq!(blend_top(vec![&pool[1], &pool[0]], [&pool[0], &pool[2], &pool[3]].into_iter(), 5, 3), vec![("B", third), ("A", third), ("C", third)]);
        assert_eq!(blend_top(Vec::new(), [&pool[2]].into_iter(), 5, 3), vec![("C", 1.0)]);
        // (#630) the collision: `fill`'s #2 is shipped's #4, so S4 books as `fill`'s, shipped's S6 enters the book and `fill`'s M6 does not
        let p: Vec<Sample> = ["S1", "S2", "S3", "S4", "S5", "S6", "S7", "M1", "M3", "M4", "M5", "M6"].iter().map(|&t| s(t, None)).collect();
        let ship: Vec<&Sample> = p[..7].iter().collect();
        let rise = [7, 3, 8, 9, 10, 11].map(|i| &p[i]);
        let want = [("S1", false), ("M1", true), ("S2", false), ("S4", true), ("S3", false), ("M3", true), ("S5", false), ("M4", true), ("S6", false), ("M5", true)];
        assert_eq!(blend_sides(&ship, rise.into_iter(), 5, 10), want);
        assert_eq!(blend_top(ship, rise.into_iter(), 5, 10), want.map(|w| (w.0, 0.1)));
        // (#584) SIZED splice: after the head, S6 (held by momentum too) moves up to a momentum slot; interleaved,
        // a held name is skipped and a dry side hands its slot over
        let sh = ["S1", "S2", "S3", "S4", "S5", "S6"];
        assert_eq!(picks::splice_order(&sh, &["M1", "S6", "M2"], true, 5, 9), ["S1", "S2", "S3", "S4", "S5", "M1", "S6", "M2"]);
        assert_eq!(picks::splice_order(&sh, &["M1"], true, 5, 7), ["S1", "S2", "S3", "S4", "S5", "M1", "S6"]);
        let m6 = ["M1", "M2", "M3", "M4", "M5", "M6"];
        assert_eq!(picks::splice_order(&sh, &m6, true, 5, 11), ["S1", "S2", "S3", "S4", "S5", "M1", "M2", "M3", "M4", "M5", "S6"], "`slots` momentum slots after the head, then shipped");
        assert_eq!(picks::splice_order(&["A", "B", "C"], &["B", "X"], false, 5, 9), ["A", "B", "C", "X"]);
        assert_eq!(picks::splice_order(&["A", "B", "C"], &["X", "Y"], false, 5, 3), ["A", "X", "B"]);
        // (#585) the side that placed each name: B is both sides', momentum reaches it first; a dry side's slot
        // goes to the other side's name, flagged as that side's
        assert_eq!(picks::splice_sides(&["A", "B", "C"], &["B", "X"], false, 5, 9), [("A", false), ("B", true), ("C", false), ("X", true)]);
        assert_eq!(picks::splice_sides(&["A"], &["X", "Y"], false, 5, 3), [("A", false), ("X", true), ("Y", true)]);
        assert_eq!(picks::splice_sides(&["A", "B"], &[], false, 5, 2), [("A", false), ("B", false)]);
        // (#597) `slots` of every 10 go to momentum, spread evenly; 5 is the (#584) alternation
        let (s9, m9) = (["S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8", "S9"], ["M1", "M2", "M3", "M4", "M5", "M6", "M7", "M8", "M9"]);
        assert_eq!(picks::splice_order(&s9, &m9, false, 7, 10), ["S1", "M1", "M2", "S2", "M3", "M4", "S3", "M5", "M6", "M7"]);
        assert_eq!(picks::splice_order(&s9, &m9, false, 3, 10), ["S1", "S2", "S3", "M1", "S4", "S5", "M2", "S6", "S7", "M3"]);
        assert_eq!(picks::splice_order(&s9, &m9, false, 10, 3), ["M1", "M2", "M3"]);
        assert_eq!(picks::splice_order(&s9, &m9, false, 0, 3), ["S1", "S2", "S3"]);
        // (#944) a huge `slots` reads as every slot, in both modes, and never overflows
        assert!(picks::momentum_slot(1, usize::MAX));
        assert_eq!(picks::splice_order(&s9, &m9, true, usize::MAX, 7), ["S1", "S2", "S3", "S4", "S5", "M1", "M2"]);
        assert_eq!(picks::splice_order(&s9, &m9, false, usize::MAX, 3), ["M1", "M2", "M3"]);
        assert!((0..1000).all(|i| picks::momentum_slot(i, 5) == (i % 2 == 1)), "5 = the (#584) alternation");
        // the weights: momentum twin Z (A's issuer) drops so X takes its slot, the cut drops B; then the first HEAD
        // rows take the head share
        let (a, b, x) = (Quote::stub("A", "1", "", "Acme"), Quote::stub("B", "1", "", "Beta"), Quote::stub("X", "1", "", "Xeno"));
        let z = Quote::stub("Z", "1", "", "Acme");
        let sz = config::Sizing { book_names: 2, head_weight: 3.0, ..Default::default() };
        assert_eq!(picks::sized_splice(&[(&a, 50.0), (&b, 50.0)], &[&z, &x], false, 5, &sz), vec![("A", 50.0), ("X", 50.0)]);
        // (#744) Y is B's twin, but the cut places only A from the score side, so Y keeps its slot
        let y = Quote::stub("Y", "1", "", "Beta");
        assert_eq!(picks::sized_splice(&[(&a, 50.0), (&b, 50.0)], &[&y, &x], false, 5, &sz), vec![("A", 50.0), ("Y", 50.0)]);
        // (#753) book row X is booked from the momentum side, and it still blocks its twin X2: one Xco, not both
        let (xo, x2, m) = (Quote::stub("X", "1", "", "Xco"), Quote::stub("X2", "1", "", "Xco"), Quote::stub("M", "1", "", "Mco"));
        let got = picks::sized_splice(&[(&a, 50.0), (&xo, 50.0)], &[&x2, &xo, &m], false, 5, &config::Sizing { book_names: 3, ..sz.clone() });
        assert_eq!(got.iter().map(|r| r.0).collect::<Vec<_>>(), ["A", "X", "M"]);
        let sz = config::Sizing { book_names: 6, ..sz };
        let six: Vec<Quote> = ["P", "Q", "R", "S", "T", "U"].iter().map(|t| Quote::stub(t, "1", "", t)).collect();
        let book: Vec<(&Quote, f64)> = six[..5].iter().map(|q| (q, 0.0)).collect();
        let got = picks::sized_splice(&book, &[&six[5]], true, 5, &sz);
        assert_eq!(got.iter().map(|r| r.0).collect::<Vec<_>>(), ["P", "Q", "R", "S", "T", "U"]);
        assert_eq!(got.iter().map(|r| (r.1 * 0.16).round()).collect::<Vec<_>>(), [3.0, 3.0, 3.0, 3.0, 3.0, 1.0], "5 heads x 3 + 1 tail = 16 shares");
        // (#1143) interleaved P U Q R S T: `head_park_first` seats score rank 1 (P) just after the head, at the tail share
        let got = picks::sized_splice(&book, &[&six[5]], false, 5, &config::Sizing { head_park_first: true, ..sz.clone() });
        assert_eq!(got.iter().map(|r| r.0).collect::<Vec<_>>(), ["U", "Q", "R", "S", "T", "P"]);
        assert_eq!(got.iter().map(|r| (r.1 * 0.16).round()).collect::<Vec<_>>(), [3.0, 3.0, 3.0, 3.0, 3.0, 1.0]);
        // a coin keeps its weight and takes no head slot, a momentum coin is dropped; a commodity head name is parked behind the head
        let mut oil = Quote::stub("P", "1", "", "P");
        oil.sector = Some("Energy".into());
        let btc = Quote::stub("BTC-EUR", "1", "", "Bitcoin");
        let book: Vec<(&Quote, f64)> = std::iter::once((&btc, 5.0)).chain(std::iter::once((&oil, 0.0))).chain(six[1..5].iter().map(|q| (q, 0.0))).collect();
        let sz = config::Sizing { head_skip_commodity: true, ..sz };
        let eth = Quote::stub("ETH-EUR", "1", "", "Ethereum");
        let got = picks::sized_splice(&book, &[&eth, &six[5]], true, 5, &sz);
        assert_eq!(got.iter().map(|r| r.0).collect::<Vec<_>>(), ["BTC-EUR", "Q", "R", "S", "T", "U", "P"]);
        assert_eq!(got.iter().map(|r| (r.1 * 16.0).round()).collect::<Vec<_>>(), [80.0, 285.0, 285.0, 285.0, 285.0, 285.0, 95.0], "95% left: 3/16 each head, 1/16 P");
    }

    /// (#518) Each cohort splits the STOCK samples on the flags at their own cutoff; a coin is never
    /// counted, a move filed after the cutoff never flags it. (#614) A Berkshire holding flags the last
    /// row alone; a holding gone the quarter after flags nothing.
    #[test]
    fn holder_cohorts_split_stocks_on_flags_at_the_cutoff() {
        let at = |tk: &str, rel: f64| Sample { relative: rel, quote: Arc::new(Quote::stub(tk, "1", "", tk)), ..sample(ymd(2020, 6, 1), 0.0) };
        let tx = |m: u32, add: bool| core::HolderTx { filed: ymd(2020, 5, 1), manager: m, add };
        let txs: HashMap<String, Vec<core::HolderTx>> = [
            ("B8", (0..8).map(|m| tx(m, true)).collect()),
            ("A3", (1..4).map(|m| tx(m, true)).chain([tx(core::HOLDER_BRK_HELD, true)]).collect()),
            ("MIX", vec![tx(1, true), tx(2, true), tx(3, true), tx(4, false), tx(core::HOLDER_BRK_HELD, false)]),
            ("LATE", (0..8).map(|m| tx(m, true)).collect()),
            ("BTC-USD", (0..8).map(|m| tx(m, true)).collect()),
        ]
        .into_iter()
        .map(|(t, v)| (t.to_string(), v))
        .collect();
        let late = Sample { date: ymd(2020, 5, 1), ..at("LATE", -2.0) }; // its moves are filed ON the cutoff
        let pool = [at("B8", 4.0), at("A3", 2.0), at("MIX", -1.0), late, at("NONE", 0.0), at("BTC-USD", 9.0)];
        let rows = holder_cohorts(&pool, &txs);
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), HOLDER_COHORTS.map(|c| c.0));
        assert_eq!(rows.iter().map(|r| (r.1.n, r.2.n)).collect::<Vec<_>>(), [(1, 4), (3, 2), (1, 4), (2, 3), (1, 4)]);
        assert_eq!(rows[1].1, CohortSide { n: 3, mean: 5.0 / 3.0, median: 2.0, hit: 200.0 / 3.0 });
        assert_eq!(rows[1].2, CohortSide { n: 2, mean: -1.0, median: -1.0, hit: 0.0 }, "a zero excess is no hit");
        assert!(cohort_side(Vec::new()).mean.is_nan());
    }

    /// (#559) The tercile is cut per cutoff date among the PAC firms: the line is the value two-thirds up
    /// the sorted list, ties at it count as top, a zero breadth never does, and a date with fewer than 3
    /// PAC firms puts none on top. Coins and cutoffs whose cycle did not load are out.
    #[test]
    fn pac_cohorts_cut_terciles_per_cutoff_date() {
        let at = |tk: &str, (y, m): (i32, u32), rel: f64| Sample {
            relative: rel,
            quote: Arc::new(Quote::stub(tk, "1", "", tk)),
            ..sample(ymd(y, m, 1), 0.0)
        };
        let pac = fetch::PacData {
            cycles: [2004].into(),
            breadth: [("A", (2004, 10)), ("B", (2004, 5)), ("C", (2004, 1)), ("Z", (2004, 0)), ("Y", (2004, 0)), ("X", (2004, 0)), ("OLD", (2002, 9))]
                .into_iter()
                .map(|(t, (c, b))| (t.to_string(), BTreeMap::from([(c, b)])))
                .collect(),
            audit: Vec::new(),
        };
        let (d1, d2, d4) = ((2005, 6), (2005, 7), (2005, 8)); // all read the 2004 cycle
        let pool = [
            // d1: breadths 0,1,5,10 -> the line is 5: A and B on top
            at("A", d1, 3.0), at("B", d1, 1.0), at("C", d1, -1.0), at("Z", d1, -2.0),
            at("N", d1, 0.5), at("OLD", d1, 4.0), at("BTC-USD", d1, 9.0), // no PAC, a 2002-only PAC, a coin
            // d2: exactly 3 PAC firms, 0,5,10 -> the line is 10: A alone on top
            at("A", d2, 2.0), at("B", d2, -3.0), at("Z", d2, 5.0),
            // d4: all zero -> the line is 0, and zero is never top
            at("Z", d4, 1.0), at("Y", d4, 1.5), at("X", d4, -0.5),
            at("A", (2003, 6), 7.0), // reads the 2002 cycle, which did not load: out
        ];
        let rows = pac_cohorts(&pool, &pac);
        assert_eq!(rows.iter().map(|r| (r.1.n, r.2.n)).collect::<Vec<_>>(), [(3, 7), (10, 2)]);
        assert_eq!((rows[0].1.median, rows[0].1.hit), (2.0, 100.0));
        assert_eq!((rows[0].2.median, rows[0].2.hit), (-0.5, 300.0 / 7.0));
    }

    /// (#559) The bar: n >= 30 on BOTH sides, and strictly higher median AND hit rate; a tie fails.
    #[test]
    fn pac_pass_needs_both_stats_higher_and_thirty_a_side() {
        let side = |n, median, hit| CohortSide { n, mean: 0.0, median, hit };
        let rest = side(30, 0.0, 50.0);
        assert!(pac_pass(&side(30, 1.0, 55.0), &rest));
        assert!(!pac_pass(&side(29, 1.0, 55.0), &rest));
        assert!(!pac_pass(&side(30, 1.0, 55.0), &side(29, 0.0, 50.0)));
        assert!(!pac_pass(&side(30, 1.0, 50.0), &rest), "hit tie");
        assert!(!pac_pass(&side(30, 0.0, 55.0), &rest), "median tie");
        assert!(!pac_pass(&side(30, -1.0, 99.0), &rest), "hit up, median down");
    }

    /// (#728) The newest row filed by the cutoff, at most 550 days old, and the fiscal year before it,
    /// even when a first XBRL 10-K files both years the same day.
    #[test]
    fn fail_flags_read_a_row_at_most_550_days_old() {
        let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let row = |filed, pe, nm| core::FundRow { filed: d(filed), period_end: d(pe), net_margin: Some(nm), ..Default::default() };
        let rows = [row("2019-03-01", "2018-12-31", -1.0), row("2020-03-01", "2019-12-31", -2.0)];
        let two = |f: Option<[bool; 6]>| f.map(|f| (f[0], f[1]));
        assert_eq!(two(fail_flags(&rows, d("2020-03-01") + chrono::Duration::days(550))), Some((true, true)));
        assert_eq!(fail_flags(&rows, d("2020-03-01") + chrono::Duration::days(551)), None, "stopped filing");
        assert_eq!(two(fail_flags(&rows, d("2019-03-01"))), Some((true, false)), "no year before the first");
        assert_eq!(fail_flags(&rows, d("2019-02-28")), None, "nothing filed yet");
        let first = [row("2020-03-01", "2018-12-31", -1.0), row("2020-03-01", "2019-12-31", -2.0)];
        assert_eq!(two(fail_flags(&first, d("2020-03-01"))), Some((true, true)));
    }

    /// (#728) Controls bin by flag count; a stale control is left out.
    #[test]
    fn month_hist_bins_fresh_controls() {
        let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let row = |filed, nm| core::FundRow { filed: d(filed), period_end: d(filed) - chrono::Duration::days(60), net_margin: Some(nm), ..Default::default() };
        let ctrls = [("LOSS", "2020-03-01", -1.0), ("OK", "2020-03-01", 5.0), ("STALE", "2017-03-01", -1.0)]
            .map(|(t, filed, nm)| (t.to_string(), vec![row(filed, nm)]));
        assert_eq!(month_hist(&ctrls, d("2020-06-01")), MonthHist { hist: [1, 1, 0, 0, 0, 0, 0], flag: [1, 0, 0, 0, 0, 0] });
    }

    /// (#728) One failure reading 1 flag against 5 controls (2 read 0, 2 read 1, 1 reads 2): it beats
    /// 2, ties 2, so AUC 0.6. A month with no control adds nothing.
    #[test]
    fn fail_tally_ranks_each_failure_against_its_month() {
        let m = MonthHist { hist: [2, 2, 1, 0, 0, 0, 0], flag: [3, 0, 2, 0, 0, 1] };
        let f = [true, false, false, false, false, false];
        let mut t = FailTally::default();
        t.add(f, &MonthHist::default());
        assert_eq!(t, FailTally::default());
        t.add(f, &m);
        let one = FailTally {
            n: 1,
            auc: 0.6,
            fail_ge: [1, 1, 0, 0, 0, 0, 0],
            ctrl_ge: [1.0, 0.6, 0.2, 0.0, 0.0, 0.0, 0.0],
            fail_flag: [1, 0, 0, 0, 0, 0],
            ctrl_flag: [0.6, 0.0, 0.4, 0.0, 0.0, 0.2],
        };
        assert_eq!(t, one);
        t.add(f, &m);
        assert_eq!((t.n, t.auc, t.fail_ge[1], t.ctrl_ge[1], t.fail_flag[0], t.ctrl_flag[5]), (2, 1.2, 2, 1.2, 2, 0.4));
    }

    /// (#728) Size splits on $1B of last-10-K assets; a wipeout joins the size-guard row unsized.
    #[test]
    fn fail_groups_split_on_kind_and_a_billion() {
        let f = |bankrupt, assets_musd| Failure { cik: "1".into(), ticker: "X".into(), event: chrono::NaiveDate::MIN, bankrupt, assets_musd };
        assert_eq!(fail_groups(&f(true, Some(1000.0))), [true, true, true, false, true]);
        assert_eq!(fail_groups(&f(true, Some(999.9))), [true, true, false, false, false]);
        assert_eq!(fail_groups(&f(true, None)), [true, true, false, false, false]);
        assert_eq!(fail_groups(&f(false, Some(5000.0))), [true, false, false, true, true]);
    }

    /// (#728) Six fields and a date, else the line drops; every shipped row parses.
    #[test]
    fn parse_failures_keeps_whole_rows() {
        let csv = "cik,ticker,name,event_date,kind,assets_musd\n0000000001,AAA,Alpha,2020-05-01,bankrupt,1234.5\n\
                   0000000002,BBB,Beta,2019-01-02,wipeout,\n0000000003,CCC,Gamma,2019-01-02,wipeout\n0000000004,DDD,Delta,2019-13-02,wipeout,\n";
        let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(
            parse_failures(csv),
            [
                Failure { cik: "0000000001".into(), ticker: "AAA".into(), event: d("2020-05-01"), bankrupt: true, assets_musd: Some(1234.5) },
                Failure { cik: "0000000002".into(), ticker: "BBB".into(), event: d("2019-01-02"), bankrupt: false, assets_musd: None },
            ]
        );
        let shipped = include_str!("../../tests/failures.csv");
        assert_eq!(parse_failures(shipped).len(), shipped.lines().count() - 1);
    }

    /// (#728) The bar at each of its edges: every cell at n 30 sits exactly on its AUC bound, and k is
    /// the first count at most 10% of the old era's controls reach.
    #[test]
    fn fail_verdict_holds_every_bar_at_its_edge() {
        let cell = FailTally { n: 30, auc: 22.5, fail_ge: [30, 25, 20, 12, 5, 1, 0], ctrl_ge: [30.0, 20.0, 9.0, 3.0, 1.0, 0.0, 0.0], ..Default::default() };
        let mut pass = [[[cell; 3]; 4]; 5];
        (pass[0][2][0].auc, pass[0][2][1].auc, pass[4][0][2].auc) = (19.5, 19.5, 21.0);
        assert_eq!(fail_verdict(&pass), (true, Some(3)));
        let fails = |edit: &dyn Fn(&mut [[[FailTally; 3]; 4]; 5])| {
            let mut t = pass;
            edit(&mut t);
            fail_verdict(&t)
        };
        assert_eq!(fails(&|t| t[0][0][0].auc = 22.4), (false, Some(3)), "(a) T-1y, old era");
        assert_eq!(fails(&|t| t[0][0][1].auc = 22.4), (false, Some(3)), "(a) T-1y, new era");
        assert_eq!(fails(&|t| t[0][2][0].auc = 19.4), (false, Some(3)), "(a) T-3y");
        assert_eq!(fails(&|t| (t[0][0][1].n, t[0][0][1].auc) = (29, 29.0)), (false, Some(3)), "(a) n under 30");
        assert_eq!(fails(&|t| t[4][0][2].auc = 20.9), (false, Some(3)), "(b)");
        assert_eq!(fails(&|t| t[0][0][1].fail_ge[3] = 11), (false, Some(3)), "(d)");
        assert_eq!(fails(&|t| t[0][0][0].ctrl_ge[3] = 3.1), (false, Some(4)), "(c) moves k");
    }

    /// (#728) The printout end to end on a toy library: one bankrupt reading a loss against a sick and a
    /// healthy control, one wipeout with no control that month.
    #[test]
    fn failure_census_prints_the_verdict_and_todays_lookalikes() {
        let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let row = |filed, nm| core::FundRow { filed: d(filed), period_end: d(filed) - chrono::Duration::days(60), net_margin: Some(nm), ..Default::default() };
        let fail = |ticker: &str, event, bankrupt| Failure { cik: ticker.into(), ticker: ticker.into(), event: d(event), bankrupt, assets_musd: Some(2000.0) };
        let fails = [(fail("BUST", "2021-06-15", true), vec![row("2020-03-01", -5.0)]), (fail("WIPE", "2016-06-15", false), vec![row("2015-03-01", 5.0)])];
        let ctrls = [("SICK", -1.0), ("OK", 5.0)].map(|(t, nm)| (t.to_string(), vec![row("2020-03-01", nm)]));
        let out = failure_census_lines(&fails, &ctrls, d("2020-07-10"));
        assert!(out.iter().any(|l| l.starts_with("  T-1y all ") && l.contains("0.75")), "{out:#?}");
        assert!(out.iter().any(|l| l.starts_with("  0 flags at T-1y: 1 failures") && l.ends_with("WIPE")), "{out:#?}");
        assert!(out.iter().any(|l| l.starts_with("  VERDICT: FAIL")), "{out:#?}");
        assert!(out.contains(&"  TODAY (2020-07-01): 1 pond stocks read 1+ flags: SICK(L)".to_string()), "{out:#?}");
    }

    /// The verdict journal's OTHER half — the merge and the horizon pick, which had no test caller
    /// at all until the mutation audit said so out loud. `parse_journal` right next door was already
    /// covered (its mutants die); `tuning_fingerprint`, `read_verdict` and `write_verdict` all
    /// survived every mutation, meaning the suite could not tell them from stubs returning nothing.
    ///
    /// What that costs is not abstract. `screen.rs` reads the journal for its method footer and
    /// compares the stored `tuning_fp` against a live `tuning_fingerprint(&settings.buy_heuristic)`
    /// to decide whether to print the numbers or a ⚠ stale-settings warning. Make the fingerprint
    /// constant and the comparison is dead in BOTH directions — drift never fires, or always fires —
    /// and the footer quotes numbers the current knobs never earned. Make the merge replace instead
    /// of merge and a `backtest 8` run erases the 20y row, which is the exact regression the journal
    /// was introduced to end.
    #[test]
    fn verdict_journal_merges_and_fingerprints() {
        // MERGE, NEVER REPLACE. An 8y write must leave 12y and 20y standing.
        let on_file = serde_json::to_string(&Journal::from([
            (20, stub_verdict(20, VERDICT_TOP)),
            (12, stub_verdict(12, VERDICT_TOP)),
        ]))
        .unwrap();
        let merged = merge_verdict(Some(&on_file), stub_verdict(8, VERDICT_TOP));
        assert_eq!(merged.keys().copied().collect::<Vec<_>>(), vec![8, 12, 20], "an 8y write erased a longer horizon");

        // Same horizon twice = the newer run wins. Merge must not mean "keep the stale one".
        let mut newer = stub_verdict(12, VERDICT_TOP);
        newer.book = 99.0;
        let replaced = merge_verdict(Some(&on_file), newer);
        assert_eq!(replaced.len(), 2);
        assert!((replaced[&12].book - 99.0).abs() < 1e-9, "re-running a horizon must overwrite its row");

        // No file yet -> a fresh one-row journal, not a panic and not an empty write.
        assert_eq!(merge_verdict(None, stub_verdict(12, VERDICT_TOP)).len(), 1);
        // Corrupt file -> the new row still lands. A broken journal must not also swallow this run.
        assert_eq!(merge_verdict(Some("not json"), stub_verdict(12, VERDICT_TOP)).len(), 1);

        // THE LONGEST HORIZON, not the first. BTreeMap orders ascending, so this is `next_back`, and
        // a `next` here would quietly make the footer cite the SHORTEST run on file.
        let three = serde_json::to_string(&Journal::from([
            (8, stub_verdict(8, VERDICT_TOP)),
            (12, stub_verdict(12, VERDICT_TOP)),
            (20, stub_verdict(20, VERDICT_TOP)),
        ]))
        .unwrap();
        assert_eq!(latest_verdict(&three).expect("parses").years, 20);
        assert!(latest_verdict("not json").is_none(), "a corrupt journal must silence the footer");

        // THE FINGERPRINT, which is the entire contract screen.rs's drift check rests on: same
        // tuning -> same string (or the footer warns on every run and the warning stops meaning
        // anything), one knob moved -> different string (or it never warns and cites stale numbers).
        let base = BuyHeuristic::default();
        assert_eq!(tuning_fingerprint(&base), tuning_fingerprint(&BuyHeuristic::default()), "same tuning must fingerprint the same");
        let mut moved = BuyHeuristic::default();
        moved.growth_trend_weight += 0.01;
        assert_ne!(tuning_fingerprint(&base), tuning_fingerprint(&moved), "a moved knob must change the fingerprint");
        assert!(!tuning_fingerprint(&base).is_empty(), "an empty fingerprint compares unequal to everything -> drift warns forever");
    }

    /// (#311) The first bar ON or after the date prices it; a zero close prices nothing; past the end is None.
    #[test]
    fn px_at_reads_the_first_bar_on_or_after_and_refuses_a_zero_close() {
        let ymd = |y, m| NaiveDate::from_ymd_opt(y, m, 1).unwrap();
        let s = (vec![ymd(2000, 1), ymd(2000, 2), ymd(2000, 3)], vec![10.0, 0.0, 30.0]);
        assert_eq!(px_at(&s, ymd(2000, 1)), Some(10.0), "a bar ON the date is that date's price");
        assert_eq!(px_at(&s, ymd(1999, 12)), Some(10.0));
        assert_eq!(px_at(&s, ymd(2000, 2)), None, "a zero close prices nothing");
        assert_eq!(px_at(&s, ymd(2000, 4)), None, "the series ended");
        // (#340) a roll window needs a price at BOTH ends
        assert!(priced_at_both(&s, ymd(2000, 1), ymd(2000, 3), false));
        assert!(!priced_at_both(&s, ymd(2000, 1), ymd(2000, 4), false), "priced at entry, but the series ended");
        assert!(!priced_at_both(&s, ymd(2000, 2), ymd(2000, 3), false), "a zero close at entry");
        // (#757) a dead series needs only its entry: the DCA ruler pays its terminal
        assert!(priced_at_both(&s, ymd(2000, 1), ymd(2000, 4), true), "a dead series ended inside the window");
        assert!(!priced_at_both(&s, ymd(2000, 4), ymd(2000, 5), true), "nothing buys a dead series after its last bar");
    }

    /// (#311) A buy on `d` sees each name's LATEST cutoff in [d − 6 months, d]: (#790) a cutoff exactly six months
    /// back is in, an older one is out, one ON `d` is in, one after `d` is look-ahead and out, and a coin never
    /// enters.
    #[test]
    fn month_pool_takes_each_names_latest_cutoff_in_the_six_months_to_the_buy() {
        let ymd = |y, m| NaiveDate::from_ymd_opt(y, m, 1).unwrap();
        let at = |tk: &str, date| Sample { date, realized: 0.0, relative: 0.0, quote: Arc::new(Quote::stub(tk, "1", "", tk)), fund: None, trail: Vec::new() };
        let books = [at("GONE", ymd(1999, 12)), at("OLD", ymd(2000, 1)), at("AAA", ymd(2000, 2)), at("BTC-USD", ymd(2000, 3)), at("AAA", ymd(2000, 5)), at("BBB", ymd(2000, 7)), at("LATE", ymd(2000, 8))];
        let got: Vec<(&str, NaiveDate)> = month_pool(&books, ymd(2000, 7)).iter().map(|s| (s.quote.ticker.as_str(), s.date)).collect();
        assert_eq!(got, [("AAA", ymd(2000, 5)), ("BBB", ymd(2000, 7)), ("OLD", ymd(2000, 1))]);
    }

    /// (#311) Hand arithmetic: A ×3 at 30%, B ×2 at 20%, and the 50% remainder on two legs. L starts ON the buy
    /// date, so it prices itself (×3); M starts later, so it borrows the benchmark (×1.5). 0.9 + 0.4 + 0.5 × 2.25 =
    /// 2.425. A name with no series prices nothing.
    #[test]
    fn book_multiple_prices_names_and_the_remainder_on_its_legs() {
        let ymd = |y| NaiveDate::from_ymd_opt(y, 1, 1).unwrap();
        let two = |a, b| (vec![ymd(2000), ymd(2010)], vec![a, b]);
        let series = HashMap::from([("A", two(10.0, 30.0)), ("B", two(5.0, 10.0))]);
        let bench = (vec![ymd(1999), ymd(2000), ymd(2010)], vec![50.0, 100.0, 150.0]);
        let (l, m) = (two(20.0, 60.0), (vec![ymd(2005), ymd(2010)], vec![1.0, 9.0]));
        let legs = [(0.5, &l), (0.5, &m)];
        let none = HashSet::new();
        let got = book_multiple(&[("A", 0.3), ("B", 0.2)], ymd(2000), ymd(2010), &series, &bench, &legs, (&none, DEAD_PENALTY)).unwrap();
        assert!((got - 2.425).abs() < 1e-12, "{got}");
        assert_eq!(book_multiple(&[("ZZZ", 0.5)], ymd(2000), ymd(2010), &series, &bench, &legs, (&none, DEAD_PENALTY)), None);
    }

    /// (#757) X dies in 2005 at 12, under half its year-ago 30: the walk's terminal rolls 12 into the index (120 to
    /// 150) and cuts it, 12/10 × 0.7 × 1.25 = 1.05. Y has the same bars but is not dead, so it prices nothing. W is
    /// dead and crashed too, but its last bar is ON `to`, so it is a price: 9/10, no cut.
    #[test]
    fn book_multiple_pays_a_name_that_dies_mid_hold_the_walks_terminal() {
        let ymd = |y| NaiveDate::from_ymd_opt(y, 1, 1).unwrap();
        let died = (vec![ymd(2000), ymd(2004), ymd(2005)], vec![10.0, 30.0, 12.0]);
        let series = HashMap::from([("X", died.clone()), ("Y", died), ("W", (vec![ymd(2000), ymd(2009), ymd(2010)], vec![10.0, 30.0, 9.0]))]);
        let bench = (vec![ymd(2000), ymd(2005), ymd(2010)], vec![100.0, 120.0, 150.0]);
        let legs = [(1.0, &bench)];
        let dead = HashSet::from(["X", "W"]);
        let got = |tk| book_multiple(&[(tk, 1.0)], ymd(2000), ymd(2010), &series, &bench, &legs, (&dead, DEAD_PENALTY));
        let x = got("X").unwrap();
        assert!((x - 1.05).abs() < 1e-12, "{x}");
        assert_eq!(got("Y"), None, "a live or censored name that ends early prices nothing");
        assert_eq!(got("W"), Some(0.9));
    }

    /// (#316) Strict at the hurdle, and the hurdle is 4.5%/yr on the program's own lots.
    #[test]
    fn dca_beats_serie_e_is_strict_at_the_hurdle() {
        let hurdle = dca_fv(0.045);
        assert_eq!(dca_beats_serie_e(&[hurdle, hurdle + 1e-9, 1.0, hurdle * 2.0]), 2);
        assert!((dca_irr(hurdle) - 4.5).abs() < 1e-9, "{}", dca_irr(hurdle));
    }

    /// (#311) Every lot compounding at 10%/yr reads 10.0. A program ending with exactly what it put in reads at or
    /// below zero: the bisection's first midpoint IS 0%, so that tie is what picks the side.
    #[test]
    fn dca_irr_recovers_the_rate_every_lot_compounded_at() {
        let wealth = (0..96).map(|m| 1.1_f64.powf(f64::from(240 - m) / 12.0)).sum::<f64>();
        assert!((dca_irr(wealth) - 10.0).abs() < 1e-9, "{}", dca_irr(wealth));
        let flat = dca_irr(96.0);
        assert!(flat <= 0.0 && flat > -1e-9, "{flat}");
    }

    /// (#311) Hand-summed program. Every leg grows 1 + months/120 (the index 1 + months/240). A sale held under 15y
    /// pays 50% of the gain; the schedule excludes all of it from 15y. Never-sell: lots 0..=60 hold ≥ 180 months and
    /// keep 3 − m/120 (sum 167.75); lots 61..=95 net 2 − m/240 (58.625); total 226.375. Roll: 1.5 at 10y times
    /// 1.5 − m/240 on the re-bought leg is 2.25 − m/160 per lot, 187.5 in all. Index: 114.375 + 46.8125 = 161.1875.
    /// Each book is priced from its own buy date, and every book is asked for the survivors at the program's end.
    #[test]
    fn dca_program_taxes_each_sale_on_its_own_hold() {
        let (start, end) = (NaiveDate::from_ymd_opt(2000, 7, 1).unwrap(), NaiveDate::from_ymd_opt(2020, 7, 1).unwrap());
        let months = |a: NaiveDate, b: NaiveDate| f64::from((b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32);
        let rung = [crate::config::CgtRung { min_years: 15.0, excluded_pct: 100.0 }];
        let p = dca_program(
            start,
            |d, e| {
                assert_eq!(e, end);
                Some(d)
            },
            |b: &NaiveDate, from, to| {
                assert_eq!(*b, from);
                Some(1.0 + months(from, to) / 120.0)
            },
            |from, to| Some(1.0 + months(from, to) / 240.0),
            0.5,
            &rung,
        )
        .unwrap();
        let close = |got: f64, want: f64| (got - want).abs() < 1e-9;
        assert!(close(p.hold, 226.375) && close(p.roll, 187.5) && close(p.index, 161.1875), "{} {} {}", p.hold, p.roll, p.index);
    }

    /// (#858) Two identities, so no hand sum has to be right. Untaxed, on one series that compounds at a single
    /// rate, the roll's sale and re-buy at the same price leave it with never-sell's €, and the index on that series
    /// reads the same. Every leg halving at a 50% rate pins (#310)'s no-loss-credit rule: each lot keeps 0.5, so
    /// never-sell and the index read 48 and the roll 24, where a credit would read 72 and 54.
    #[test]
    fn dca_program_untaxed_roll_is_hold_and_a_loss_earns_no_credit() {
        let start = NaiveDate::from_ymd_opt(2000, 7, 1).unwrap();
        let months = |a: NaiveDate, b: NaiveDate| f64::from((b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32);
        let grow = |from, to| Some(2.0_f64.powf(months(from, to) / 120.0));
        let p = dca_program(start, |d, _| Some(d), |_: &NaiveDate, from, to| grow(from, to), grow, 0.0, &[]).unwrap();
        let want = (0..96).map(|m| 2.0_f64.powf(f64::from(240 - m) / 120.0)).sum::<f64>();
        let close = |got: f64| (got - want).abs() < 1e-9;
        assert!(close(p.hold) && close(p.roll) && close(p.index), "{want}: {} {} {}", p.hold, p.roll, p.index);
        let half = dca_program(start, |d, _| Some(d), |_: &NaiveDate, _, _| Some(0.5), |_, _| Some(0.5), 0.5, &[]).unwrap();
        assert_eq!((half.hold, half.roll, half.index), (48.0, 24.0, 48.0));
    }

    /// (#1224) Sell-all at [`ROLL_MONTHS`] with no fee is [`dca_program`]: its two tests' closures read the same €,
    /// every roll selling the whole lot. A roll as long as the program never sells, so it is never-sell to the cent.
    #[test]
    fn dca_chain_sell_all_at_ten_years_is_dca_program() {
        let (start, end) = (NaiveDate::from_ymd_opt(2000, 7, 1).unwrap(), NaiveDate::from_ymd_opt(2020, 7, 1).unwrap());
        let months = |a: NaiveDate, b: NaiveDate| f64::from((b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32);
        let rung = [crate::config::CgtRung { min_years: 15.0, excluded_pct: 100.0 }];
        let all = |_: &NaiveDate, _: &NaiveDate| 1.0;
        let (p, sold) = dca_chain(
            start,
            ROLL_MONTHS,
            0.0,
            |d, e| {
                assert_eq!(e, end);
                Some(d)
            },
            |b: &NaiveDate, from, to| {
                assert_eq!(*b, from);
                Some(1.0 + months(from, to) / 120.0)
            },
            all,
            |from, to| Some(1.0 + months(from, to) / 240.0),
            0.5,
            &rung,
        )
        .unwrap();
        let close = |got: f64, want: f64| (got - want).abs() < 1e-9;
        assert!(close(p.hold, 226.375) && close(p.roll, 187.5) && close(p.index, 161.1875) && sold == 1.0, "{} {} {} {sold}", p.hold, p.roll, p.index);
        let half = |_: &NaiveDate, _, _| Some(0.5);
        let (h, _) = dca_chain(start, ROLL_MONTHS, 0.0, |d, _| Some(d), half, all, |_, _| Some(0.5), 0.5, &[]).unwrap();
        assert_eq!((h.hold, h.roll, h.index), (48.0, 24.0, 48.0));
        let grow = |_: &NaiveDate, from, to| Some(1.0 + months(from, to) / 120.0);
        let (n, none) = dca_chain(start, 2 * ROLL_MONTHS, 0.0, |d, _| Some(d), grow, all, |_, _| Some(1.0), 0.5, &rung).unwrap();
        assert!(close(n.roll, 226.375) && n.roll == n.hold && none == 0.0, "{} {} {none}", n.roll, n.hold);
    }

    /// (#1224) Leavers-only. Every leg doubles, each roll sells half, the rate is 50%. A lot rolled at 60, 120 and
    /// 180 months: sell half of 2 (tax 0.25, basis 1.25, keeps 1.75), of 3.5 (tax 0.5625, basis 1.8125), of 5.875
    /// (tax 1.015625, basis 2.828125, keeps 4.859375), then 9.71875 sold whole nets 6.2734375. The 60 lots bought
    /// before month 60 roll three times, the other 36 twice (3.84375 each): 514.78125. Never-sell nets 1.5 a lot.
    /// A book that never changes sells nothing, so on one compounding series the roll is never-sell.
    #[test]
    fn dca_chain_taxes_only_the_share_a_roll_sells() {
        let start = NaiveDate::from_ymd_opt(2000, 7, 1).unwrap();
        let twice = |_: &NaiveDate, _, _| Some(2.0);
        let (p, sold) = dca_chain(start, 60, 0.0, |d, _| Some(d), twice, |_, _| 0.5, |_, _| Some(1.0), 0.5, &[]).unwrap();
        assert_eq!((p.roll, p.hold, p.index, sold), (514.78125, 144.0, 96.0, 0.5));
        let months = |a: NaiveDate, b: NaiveDate| f64::from((b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32);
        let grow = |_: &NaiveDate, from, to| Some(2.0_f64.powf(months(from, to) / 120.0));
        let (q, none) = dca_chain(start, 3, 0.0, |d, _| Some(d), grow, |_, _| 0.0, |_, _| Some(1.0), 0.5, &[]).unwrap();
        assert!((q.roll - q.hold).abs() < 1e-9 && none == 0.0, "{} {} {none}", q.roll, q.hold);
    }

    /// (#1224) Untaxed, sell-all, every leg doubling: a lot pays 0.5% of its value at each roll, so the 60 lots that
    /// roll three times end 16 × 0.995³ and the other 36 8 × 0.995². A missing price drops the program.
    #[test]
    fn dca_chain_charges_the_fee_on_each_rolls_sale() {
        let start = NaiveDate::from_ymd_opt(2000, 7, 1).unwrap();
        let twice = |_: &NaiveDate, _, _| Some(2.0);
        let (p, _) = dca_chain(start, 60, 0.005, |d, _| Some(d), twice, |_, _| 1.0, |_, _| Some(1.0), 0.0, &[]).unwrap();
        let want = 60.0 * 16.0 * 0.995_f64.powi(3) + 36.0 * 8.0 * 0.995_f64.powi(2);
        assert!((p.roll - want).abs() < 1e-9 && p.hold == 192.0, "{} {want} {}", p.roll, p.hold);
        let gap = |_: &NaiveDate, from: NaiveDate, _| (from.year() < 2010).then_some(2.0);
        assert!(dca_chain(start, 60, 0.0, |d, _| Some(d), gap, |_, _| 1.0, |_, _| Some(1.0), 0.0, &[]).is_none());
    }

    /// (#1224) What a roll sells: nothing of the same book, all of a disjoint one, half where half the weight stays,
    /// and the unfunded remainders count as held, since both earn the same legs.
    #[test]
    fn turnover_is_the_weight_the_new_book_drops() {
        let book = [("A", 0.6), ("B", 0.4)];
        assert_eq!(turnover(&book, &book), 0.0);
        assert_eq!(turnover(&book, &[("C", 0.5), ("D", 0.5)]), 1.0);
        assert_eq!(turnover(&[("A", 0.5), ("B", 0.5)], &[("A", 0.5), ("C", 0.5)]), 0.5);
        assert_eq!(turnover(&[("A", 0.5)], &[("B", 0.5)]), 0.5);
        assert_eq!(turnover(&[], &[]), 0.0);
    }

    /// (#1224) The bar: the best early half above zero is picked, and it ships only with its late half, mean and
    /// median above zero over 12 or more programs.
    #[test]
    fn pick_rung_takes_the_best_early_half_and_ships_on_all_four() {
        let row = |early, late, mean, med, n| Some((early, late, mean, med, n));
        assert_eq!(pick_rung(&[row(0.1, 1.0, 1.0, 1.0, 12), None, row(0.3, 1.0, 1.0, 1.0, 12), row(0.2, 1.0, 1.0, 1.0, 12)]), Some((2, true)));
        assert_eq!(pick_rung(&[row(0.3, 1.0, 1.0, 1.0, 12), row(0.3, -1.0, 1.0, 1.0, 12)]), Some((0, true)), "a tie keeps the first");
        assert_eq!(pick_rung(&[row(0.0, 1.0, 1.0, 1.0, 12), row(-0.1, 1.0, 1.0, 1.0, 12), None]), None);
        assert_eq!(pick_rung(&[row(0.1, 0.0, 1.0, 1.0, 12)]), Some((0, false)));
        assert_eq!(pick_rung(&[row(0.1, 1.0, 0.0, 1.0, 12)]), Some((0, false)));
        assert_eq!(pick_rung(&[row(0.1, 1.0, 1.0, 0.0, 12)]), Some((0, false)));
        assert_eq!(pick_rung(&[row(0.1, 1.0, 1.0, 1.0, 11)]), Some((0, false)));
    }

    /// (#309) The BUY NOW record line: longest horizon first, rows without a SIZED record skipped, and
    /// drift on EITHER fingerprint warns. The `sizing:` half is the new one: a moved cap changes the
    /// SIZED row with every `buy_heuristic` knob untouched.
    #[test]
    fn sized_record_line_cites_sized_rows_and_both_fingerprints() {
        let sized = |years, date: &str| {
            let mut v = stub_verdict(years, VERDICT_TOP);
            v.date = date.into();
            v.sized = Some(SizedVerdict { windows: 21, book: 6.5, excess: -0.1, worst: -2.8, deployed: 44.0, sizing_fp: "S".into(), roll: (years == 20).then_some(2.1) });
            v
        };
        let j = Journal::from([(8, sized(8, "2026-09-12")), (12, stub_verdict(12, VERDICT_TOP)), (20, sized(20, "2026-09-13"))]);
        let tf = "{\"a\":1}";
        assert_eq!(
            sized_record_line(&j, tf, "S").as_deref(),
            Some(
                "  Graded record of this book (point-in-time backtest, past windows, not a forecast): \
                 20y +6.5%/yr (-0.1 vs index, worst -2.8, 44% in names, 21 windows, run 2026-09-13; \
                 buying monthly for 8y, selling each lot at 10y to re-buy the screen: +2.1/yr vs never-sell, after tax) · \
                 8y +6.5%/yr (-0.1 vs index, worst -2.8, 44% in names, 21 windows, run 2026-09-12)"
            )
        );
        let warn = " — ⚠ settings changed since, rerun `folioman backtest N universe fund pit dead`";
        assert!(sized_record_line(&j, "moved", "S").unwrap().ends_with(warn), "a moved buy_heuristic knob must warn");
        assert!(sized_record_line(&j, tf, "moved").unwrap().ends_with(warn), "a moved sizing knob must warn");
        // A stale row WITHOUT a SIZED record is not this line's claim, so it must not warn here.
        let mut old = stub_verdict(12, VERDICT_TOP);
        old.tuning_fp = "older".into();
        let mixed = Journal::from([(12, old), (20, sized(20, "2026-09-13"))]);
        assert!(!sized_record_line(&mixed, tf, "S").unwrap().contains('⚠'));
        assert_eq!(sized_record_line(&Journal::from([(12, stub_verdict(12, VERDICT_TOP))]), tf, "S"), None, "a pre-(#309) journal stays silent");
        let mut moved = config::Sizing::default();
        moved.max_name_pct += 1.0;
        let base = sizing_fingerprint(&config::Sizing::default(), 25);
        assert_ne!(base, sizing_fingerprint(&moved, 25), "a moved cap must change the sizing fingerprint");
        assert_ne!(base, sizing_fingerprint(&config::Sizing::default(), 20), "(#1060) a moved top_picks cuts the book, so it must too");
    }

    /// (#91) The effective trial count, on the two window counts the finding names, plus the two things
    /// that keep it from being a footgun: OFF renders the byte-identical footer every golden pins, and
    /// the arithmetic is a pure read of `windows` and `years` — no journal field, so an old verdict file
    /// gets a correct `n_eff` too.
    #[test]
    fn n_eff_says_how_many_trials_a_window_count_is_worth() {
        // the 20y report's headline: "win 67% ... (windows 21)" is 21 draws worth half a trial.
        assert!((n_eff(21, 20) - 0.525).abs() < 1e-9);
        // the 12y run's 33 windows are ~1.4 — one and a bit.
        assert!((n_eff(33, 12) - 1.375).abs() < 1e-9);
        // a hold of 0 would divide by zero and print inf as if it meant something.
        assert!(n_eff(10, 0).is_finite());

        assert_eq!(n_eff_tag(false, 21, 20), "", "OFF must add NOTHING — the goldens are the proof");
        assert_eq!(n_eff_tag(true, 21, 20), "  n_eff 0.5");
        let v = stub_verdict(12, VERDICT_TOP);
        let off = verdict_line(&v, false, false);
        assert!(off.contains("top-10 held 12y, 84 windows): book"), "the footer names the graded basket: {off}");
        assert!(!off.contains("n_eff") && !off.contains("best of"));
        assert!(verdict_line(&v, false, true).contains("84 windows  n_eff 3.5): book"));

        // (#92) the ladder table's half of the caveat, which the footer no longer shares.
        assert_eq!(best_of_tag(false, 13), "", "OFF must add NOTHING — the goldens are the proof");
        assert_eq!(best_of_tag(true, TOP_LADDER.len()), " [best of 13, unhaircut]");
        assert_eq!(TOP_LADDER.len(), 13, "the printed count and the graded ladder are the same list");
        assert!(TOP_LADDER.contains(&VERDICT_TOP), "the shipped basket must be a rung the table grades");
        // (#120) THE FOOTER MUST NEVER CARRY THE BEST-OF CAVEAT AGAIN. `VERDICT_TOP` is fixed a priori,
        // so a best-of-13 tag beside it would caveat a selection nobody made. There is no knob that can
        // put it back — the parameter is gone — and this asserts the strongest form of that: whatever
        // the display knobs say, the rendered footer never claims the basket was a maximum.
        assert!(!verdict_line(&v, false, true).contains("best of"), "the fixed basket is not a best-of-N");
        assert!(!verdict_line(&v, true, false).contains("best of"), "not on the drift arm either");
    }

    /// (#96) The two numbers a reader compares by eye and should not: a lane's `edge` is a spread of
    /// CUMULATIVE returns over the hold, `top-N excess` is per year. The restatement must be the
    /// difference of the annualised HALVES, not the annualisation of the difference — those disagree,
    /// and only the first one is "points per year". OFF renders nothing, which is what the goldens pin.
    #[test]
    fn a_cumulative_edge_and_a_per_year_excess_are_not_the_same_number() {
        // 10× and 2× over a 20y hold: +900 vs +100 cumulative, an +800 pt "edge".
        // Annualised the two halves are +12.20%/yr and +3.53%/yr — an +8.68 pt/yr spread, ~92× smaller.
        let tag = annualized_edge_tag(true, 900.0, 100.0, 20);
        assert_eq!(tag, "   [= +8.68 pts/yr over the 20y hold]", "{tag}");
        // annualising the SPREAD instead would read +11.61 pts/yr — a different, wrong answer.
        assert!(!tag.contains("+11.61"));
        // at a 1y hold the two units coincide, so the restatement must be the edge itself.
        assert_eq!(annualized_edge_tag(true, 900.0, 100.0, 1), "   [= +800.00 pts/yr over the 1y hold]");

        assert_eq!(annualized_edge_tag(false, 900.0, 100.0, 20), "", "OFF adds NOTHING — goldens are the proof");
        // a half at −100% has no real root; the tag goes silent rather than printing NaN.
        assert_eq!(annualized_edge_tag(true, 900.0, -100.0, 20), "");
        assert!(!annualized_edge_tag(true, 900.0, -99.9, 20).contains("NaN"));
    }

    /// (#96) `turnover_frac` is a PER-REBALANCE number but the charge landed once against a hold-long
    /// edge. At a 20y hold that is 40 six-month re-formations paying for one, and the printed formula
    /// must quote the multiplier it actually used rather than leave the arithmetic unreconcilable.
    #[test]
    fn the_turnover_charge_must_count_the_rebalances_inside_the_hold() {
        // OFF: one round trip, whatever the hold — the arithmetic every fitted receipt was measured under.
        assert_eq!(rebalances(20, false), 1);
        assert!((cost_pts(0.5, 20, false) - 0.1).abs() < 1e-12);
        assert_eq!(rebalance_tag(20, false), "", "OFF adds NOTHING — goldens are the proof");

        // ON: ~6mo buckets, so 2 per year of hold. A 0.1pt charge on a 20y run becomes 4.0pt.
        assert_eq!(rebalances(20, true), 40);
        assert!((cost_pts(0.5, 20, true) - 4.0).abs() < 1e-12);
        assert_eq!(rebalance_tag(20, true), " × 40 rebalances");
        // the hold-period sweep passes its own row's `h`, so a 1y row pays 2 and a 10y row pays 20.
        assert_eq!((rebalances(1, true), rebalances(10, true)), (2, 20));
        // a 0y hold must still charge something rather than zeroing the cost line.
        assert_eq!(rebalances(0, true), 1);
    }

    /// (#104) The two properties that make recall/capture worth printing, stated as arithmetic.
    ///
    /// ONE: capture is not recall. A lane that holds one of the bucket's two winners scores 50% recall
    /// either way, but 67% capture if it caught the 10× and 33% if it caught the 5×. Under skew those
    /// are not the same book, and recall alone cannot tell them apart — which is the entire reason the
    /// doc calls capture "the one that matters".
    ///
    /// TWO: the denominator includes what the GATES threw away. A winner the lane never scored is
    /// still a winner of the pool, so it drags recall down. That is the measurement `gate_audit`
    /// structurally cannot make — it reads the MEAN of the rejected cohort, and one 30-bagger among
    /// 300 losers does not move a mean far enough to notice.
    #[test]
    fn capture_weights_the_winners_that_recall_merely_counts() {
        let row = |t: &'static str, realized: f64| Sample {
            date: ymd(2010, 1, 1),
            realized,
            relative: 0.0,
            quote: Arc::new(Quote::stub(t, "1", "", t)),
            fund: None,
            trail: Vec::new(),
        };
        // one bucket, five names: BIG is a 10×, MID a 5×, the rest are not winners at any cut.
        let pool = vec![row("BIG", 900.0), row("MID", 400.0), row("C", 100.0), row("D", 0.0), row("E", -50.0)];
        let hold = |names: &[&str]| -> Vec<(&Sample, f64)> {
            // score descending in the order given, so `take(n)` holds exactly these names
            names.iter().enumerate().map(|(i, n)| (pool.iter().find(|s| s.quote.ticker == *n).unwrap(), 100.0 - i as f64)).collect()
        };
        // top 40% of 5 rows = 2 winners, {BIG, MID}: Σ multiple = 10 + 5 = 15.
        let (recall, capture, w) = recall_capture(&pool, &hold(&["BIG"]), 1, 0.4, 0.0).unwrap();
        assert_eq!(w, 2);
        assert!((recall - 0.5).abs() < 1e-12, "one of two winners held");
        assert!((capture - 10.0 / 15.0).abs() < 1e-12, "caught the 10×: {capture}");
        let (recall, capture, _) = recall_capture(&pool, &hold(&["MID"]), 1, 0.4, 0.0).unwrap();
        assert!((recall - 0.5).abs() < 1e-12, "SAME recall — this is the point");
        assert!((capture - 5.0 / 15.0).abs() < 1e-12, "caught the 5×: {capture}");

        // a gate-rejected winner still counts against the lane: BIG is in the pool, not in `scored`.
        let (recall, capture, w) = recall_capture(&pool, &hold(&["C", "D", "E"]), 3, 0.2, 0.0).unwrap();
        assert_eq!((w, recall, capture), (1, 0.0, 0.0), "the gates cut the only winner and the metric says so");

        // the ≥10× cut reads the whole pool by multiple, not by rank: only BIG qualifies.
        let (recall, _, w) = recall_capture(&pool, &hold(&["BIG", "MID"]), 2, 1.0, 10.0).unwrap();
        assert_eq!((w, recall), (1, 1.0));
        // …and a pool with no ten-bagger answers None rather than a flattering 100%.
        assert!(recall_capture(&pool[2..], &hold(&["C"]), 1, 1.0, 10.0).is_none());
        // a single-row bucket is skipped: recall over one name is 0 or 1 and means neither.
        assert!(recall_capture(&pool[..1], &hold(&["BIG"]), 1, 1.0, 0.0).is_none());
    }

    /// (#1155) The winner cut is `recall_capture`'s: the best `frac` share by realized return, rounded,
    /// never under one name, none in a bucket under two rows. The ladder line is, per rung, the share of
    /// booked windows whose book held one; a rung that booked nothing prints no cell.
    #[test]
    fn winner_in_book_semantics() {
        let row = |t: &'static str, realized: f64| Sample {
            date: ymd(2010, 1, 1),
            realized,
            relative: 0.0,
            quote: Arc::new(Quote::stub(t, "1", "", t)),
            fund: None,
            trail: Vec::new(),
        };
        let pool = [row("C", 100.0), row("BIG", 900.0), row("MID", 400.0)];
        let refs: Vec<&Sample> = pool.iter().collect();
        let set = |names: &[&'static str]| names.iter().copied().collect::<HashSet<&str>>();
        let best = set(&["BIG"]);
        assert_eq!(top_winners(&refs, 0.34), best); // a third of three rows is one
        assert_eq!(top_winners(&refs, 0.5), set(&["BIG", "MID"])); // 1.5 rounds to 2
        assert_eq!(top_winners(&refs[..2], 0.01), best); // a tiny share keeps one
        assert!(top_winners(&refs[..1], 1.0).is_empty()); // one row is no winner
        assert!(top_winners(&[], 1.0).is_empty());

        assert_eq!(winner_in_book_line(&[]), None);
        assert_eq!(winner_in_book_line(&[(1, vec![])]), None, "no rung booked");
        let rungs = [
            (1, vec![true, false, false, false]),
            (2, vec![]),
            (3, vec![true, true, false, true]),
        ];
        assert_eq!(
            winner_in_book_line(&rungs).as_deref(),
            Some(
                "  WINNER IN BOOK (#1155): share of the 4 booked windows whose book holds a top-1% winner | top-1 25% · top-3 75%"
            )
        );
    }

    /// (#127) The attribution that turns "the lane missed 96% of the extreme wealth" into something
    /// actionable. Three buckets, three DIFFERENT fixes — a threshold, data coverage, a scoring
    /// weight — so mislabelling one sends a whole round at the wrong lever.
    ///
    /// Two properties beyond the mapping itself:
    ///
    /// ONE: rows rank by missed WEALTH, not by count. The fixture makes those two orders disagree —
    /// `unassessable` has the most winners and the least wealth — because ranking a skewed miss list
    /// by headcount is exactly the mistake `capture` exists to prevent.
    ///
    /// TWO: the rows reconcile with `recall_capture` EXACTLY, in both count and wealth. The two share
    /// a winner definition, a bucket skip and an `n`; if either drifts, this assert is what catches it,
    /// and it is also what kills a `Default::default()` return on the function under test.
    #[test]
    fn missed_winners_are_attributed_to_the_constraint_that_actually_blocked_them() {
        let legs = |pairs: &[(&str, f64)]| -> Vec<Option<(String, f64)>> {
            crate::core::HORIZONS
                .iter()
                .map(|(l, _)| pairs.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
                .collect()
        };
        let t = BuyHeuristic::default();
        // Each fixture asserts its own `gate_failures` verdict FIRST, so it documents its precondition
        // instead of silently drifting into a different row when a gate default moves.
        let unassessable = |tick: &'static str| Quote::stub(tick, "1", "", tick); // no turnover
        assert!(
            picks::gate_failures(&unassessable("BARE"), &t).is_none(),
            "unknown turnover is NOT ASSESSABLE, which is a different answer from being gated out"
        );
        let no_history = |tick: &'static str| {
            let mut q = Quote::stub(tick, "1", "", tick);
            q.instrument_type = "EQUITY".into();
            q.avg_turnover_eur = Some(1e9);
            q
        };
        assert_eq!(
            picks::gate_failures(&no_history("NOHIST"), &t).expect("assessable").first().map(|f| f.0),
            Some("history"),
            "a known turnover but no 5Y leg is the `history` cohort"
        );
        // `gate_fixture`'s own recipe (picks.rs): near its high, a 24.6%/yr 5Y leg, climbing on the year.
        let clears = |tick: &'static str| {
            let mut q = no_history(tick);
            q.range_pct = 90.0;
            q.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("5Y", 200.0)]);
            q
        };
        let cf = picks::gate_failures(&clears("CLEAN"), &t);
        assert!(cf.as_ref().is_some_and(|v| v.is_empty()), "CLEAN must clear every gate, got {cf:?}");
        // Fails EXACTLY TWO gates: outside its own range AND down on the year. Filed under whichever is
        // first in gate order, but NOT a sole blocker — no single knob buys this name back. Both misses
        // are inside the gates' own near-miss slack (75 vs an 80 floor with 10pp of give; -5% on the
        // year against a 0 floor with 10pp), so this is the pair cohort an arm could actually admit.
        let multi = {
            let mut q = clears("MULTI");
            q.range_pct = 75.0;
            q.perf = legs(&[("1M", 2.0), ("1Y", -5.0), ("5Y", 200.0)]);
            q
        };
        let mf = picks::gate_failures(&multi, &t).expect("assessable");
        assert_eq!(mf.len(), 2, "MULTI must fail EXACTLY 2 gates or it lands outside the pair table: {mf:?}");
        assert!(mf[0].2 && mf[1].2, "both MULTI failures must be NEAR misses: {mf:?}");
        let multi_reason = mf[0].0;
        // The same pair, but one member missing by a mile (10% of range against an 80 floor). Same row
        // as MULTI, and the near column is what separates them — without it the pair table would price
        // an unreachable name as if a modest loosening could buy it.
        let far = {
            let mut q = multi.clone();
            q.ticker = "FAR".into();
            q.range_pct = 10.0;
            q
        };
        let ff = picks::gate_failures(&far, &t).expect("assessable");
        assert_eq!(
            (ff.len(), ff[0].2, ff[1].2),
            (2, false, true),
            "FAR must fail the SAME two gates with the first NOT near: {ff:?}"
        );

        // One bucket. Realized % -> multiple: 950->10.5, 900->10.0, 400->5.0, 300->4.0, 100->2.0, …
        let row = |q: Quote, realized: f64| Sample {
            date: ymd(2010, 1, 1),
            realized,
            relative: 0.0,
            quote: Arc::new(q),
            fund: None,
            trail: Vec::new(),
        };
        let pool = vec![
            row(clears("TOP"), 950.0),      // held -> attributed to nothing
            row(clears("CLEAN"), 900.0),    // cleared the gates, ranked below n -> out-ranked
            row(no_history("NOHIST"), 400.0), // fails exactly one gate -> SOLE blocker
            row(multi, 300.0),                // fails exactly 2, both near -> the reachable pair cohort
            row(far, 200.0),                  // fails the same 2, one by a mile -> in the pair, not near
            row(unassessable("BARE"), 100.0),
            row(unassessable("BARE2"), 10.0),
            row(unassessable("BARE3"), 5.0),
        ];
        // scored = the two that clear the gates, TOP ranked first; n=1 holds TOP alone.
        let scored: Vec<(&Sample, f64)> = ["TOP", "CLEAN"]
            .iter()
            .enumerate()
            .map(|(i, n)| (pool.iter().find(|s| s.quote.ticker == *n).unwrap(), 100.0 - i as f64))
            .collect();
        let table =
            missed_winner_reasons(&pool, &scored, &t, 1, 1.0, 0.0).expect("eight winners in the pool");
        let (rows, w_n, w_mult) = (&table.rows, table.w_n, table.w_mult);
        assert_eq!(w_n, 8);
        assert!((w_mult - 36.65).abs() < 1e-9, "Σ multiple over every winner: {w_mult}");

        // THE MAPPING, and the order is by wealth: out-ranked 10.0 > range 7.0 (MULTI+FAR) > history
        // 5.0 > unassessable 4.15 — even though `unassessable` has three winners to every other row's
        // one or two.
        let got: Vec<(&str, usize)> = rows.iter().map(|r| (r.reason, r.winners)).collect();
        assert_eq!(
            got,
            vec![("out-ranked", 1), (multi_reason, 2), ("history", 1), ("unassessable", 3)],
            "{rows:?}"
        );
        assert!((rows[0].mult - 10.0).abs() < 1e-9 && (rows[3].mult - 4.15).abs() < 1e-9, "{rows:?}");

        // THE SOLE-BLOCKER COLUMN, which is the whole reason this table can be acted on: only NOHIST
        // fails exactly one gate. MULTI is counted under the same kind of row but is NOT recoverable by
        // moving one knob, and `out-ranked`/`unassessable` are structurally zero (no gate failed / not
        // assessable). Read the first-failure column alone and MULTI's wealth looks reachable; it isn't.
        let sole: Vec<(usize, f64)> = rows.iter().map(|r| (r.sole, r.sole_mult)).collect();
        assert_eq!(sole.iter().map(|s| s.0).collect::<Vec<_>>(), vec![0, 0, 1, 0], "{rows:?}");
        assert!((sole[2].1 - 5.0).abs() < 1e-9 && sole.iter().map(|s| s.1).sum::<f64>() == 5.0, "{rows:?}");

        // (#128) THE PAIR TABLE. MULTI and FAR fail the same two gates, so they are ONE row carrying
        // both — and the near column splits it, because only MULTI is inside the slack a shippable
        // loosening reaches. Read `mult` alone and the arm looks twice as valuable as it is.
        let pairs: Vec<((&str, &str), usize, f64, usize, f64)> =
            table.pairs.iter().map(|pr| (pr.pair, pr.winners, pr.mult, pr.near, pr.near_mult)).collect();
        assert_eq!(pairs.len(), 1, "one distinct pair in this pool: {pairs:?}");
        assert_eq!((pairs[0].0, pairs[0].1, pairs[0].3), ((mf[0].0, mf[1].0), 2, 1), "{pairs:?}");
        assert!((pairs[0].2 - 7.0).abs() < 1e-9 && (pairs[0].4 - 4.0).abs() < 1e-9, "{pairs:?}");

        // THE COUNT HISTOGRAM, and its own tie-back: it files every GATE-ASSESSABLE miss exactly once,
        // so it must sum to the rows table minus `unassessable` — which has no gate list and is absent
        // by construction. If it summed to the whole table, `None` would be being read as "zero gates
        // failed", i.e. as out-ranked, which is the opposite of what it means.
        let hist = &table.by_count;
        assert_eq!(
            hist.iter().map(|h| (h.0, h.1)).collect::<Vec<_>>(),
            vec![(0, 1), (1, 1), (2, 2)],
            "out-ranked once, one single-gate miss, two two-gate misses: {hist:?}"
        );
        let assessable: f64 = rows.iter().filter(|r| r.reason != "unassessable").map(|r| r.mult).sum();
        assert!(
            (hist.iter().map(|h| h.2).sum::<f64>() - assessable).abs() < 1e-9,
            "histogram {hist:?} must sum to the assessable rows {assessable}"
        );

        // THE TIE-BACK to `recall_capture` on the same arguments — both halves, count and wealth.
        let (recall, capture, rc_w) = recall_capture(&pool, &scored, 1, 1.0, 0.0).unwrap();
        let (missed_n, missed_mult): (usize, f64) =
            (rows.iter().map(|r| r.winners).sum(), rows.iter().map(|r| r.mult).sum());
        assert_eq!((rc_w, missed_n), (w_n, w_n - 1), "one winner was held, seven were missed");
        assert!((recall - 1.0 / 8.0).abs() < 1e-9);
        assert!(
            (missed_mult / w_mult - (1.0 - capture)).abs() < 1e-9,
            "missed wealth {missed_mult} of {w_mult} must be exactly the complement of capture {capture}"
        );

        // Every winner HELD -> None, not an empty table the caller would divide by. top_frac 0.01
        // rounds k down to 1, so TOP is the only winner, and n=1 holds it.
        assert!(missed_winner_reasons(&pool, &scored, &t, 1, 0.01, 0.0).is_none());

        // no winner anywhere -> None, not an empty table that reads like "nothing was missed".
        assert!(missed_winner_reasons(&pool, &scored, &t, 1, 1.0, 100.0).is_none());

        // WINNERS WORTH NOTHING -> None. Two names that realized -100% are still counted as winners
        // (`min_multiple` 0.0 admits them) and still attributed, so the table is NOT empty — but their
        // multiples sum to zero, and every share the caller prints divides by that sum. `rows.is_empty()`
        // cannot see this case; only the `w_mult > 0.0` guard stands between the report and a NaN.
        let wiped = vec![row(unassessable("Z1"), -100.0), row(unassessable("Z2"), -100.0)];
        assert!(missed_winner_reasons(&wiped, &[], &t, 1, 1.0, 0.0).is_none());
        // (#340) `recall_capture` carries the same guard: counted winners worth nothing -> None.
        assert!(recall_capture(&wiped, &[], 1, 1.0, 0.0).is_none());
        // (#340) a winner exactly ON `min_multiple` counts: CLEAN's 10.0x keeps the table alive at a 10x bar.
        assert_eq!(missed_winner_reasons(&pool, &scored, &t, 1, 1.0, 10.0).map(|x| x.w_n), Some(2));
    }

    /// (#159) `held_loser_factors` has three claims and each one rots differently if it breaks.
    /// (a) It reads the HELD book — the top-n of each bucket — not the pool: a name the ranking left
    /// out has no bearing on what the book bought, and letting one vote is how a "loser trait" gets
    /// manufactured out of names nobody owned. (b) It splits on `realized < 0`, money actually lost.
    /// (c) It DROPS a factor that cannot fill both sides, which is the claim that would fail
    /// silently — a factor present only on the winners would otherwise print its median against an
    /// empty cohort and read exactly like the separation this instrument exists to find.
    #[test]
    fn held_loser_factors_reads_only_the_held_book_and_drops_one_sided_factors() {
        use crate::core::FundFactors;
        let row = |t: &str, realized: f64, roe: Option<f64>, roic: Option<f64>| Sample {
            date: ymd(2010, 1, 1),
            realized,
            relative: 0.0,
            quote: Arc::new(Quote::stub(t, "1", "", t)),
            fund: Some(FundFactors { roe, roic, ..Default::default() }),
            trail: Vec::new(),
        };
        // One bucket, four names. `roe` fills both sides; `roic` is winners-only and must vanish.
        // UNHELD ranks last and carries absurd values, so if it ever votes the medians move visibly.
        let pool = [
            row("WIN1", 100.0, Some(30.0), Some(9.0)),
            row("WIN2", 50.0, Some(20.0), Some(7.0)),
            row("LOSE", -40.0, Some(2.0), None),
            row("UNHELD", -99.0, Some(-500.0), Some(-500.0)),
        ];
        let scored: Vec<(&Sample, f64)> = ["WIN1", "WIN2", "LOSE", "UNHELD"]
            .iter()
            .enumerate()
            .map(|(i, n)| (pool.iter().find(|s| s.quote.ticker == *n).unwrap(), 100.0 - i as f64))
            .collect();

        let rows = held_loser_factors(&scored, 3, &["roe", "roic"]);
        assert_eq!(rows.len(), 1, "roic is winners-only and must be dropped, not printed: {rows:?}");
        assert_eq!(
            (rows[0].factor, rows[0].n_win, rows[0].n_lose, rows[0].med_win, rows[0].med_lose),
            ("roe", 2, 1, 25.0, 2.0),
            "held book is WIN1/WIN2/LOSE: winners median (30,20)->25, losers (2)->2; UNHELD must not vote"
        );

        // n=4 admits UNHELD, and its -500 is what proves the top-n slice above was doing the work.
        let all = held_loser_factors(&scored, 4, &["roe"]);
        assert_eq!(all[0].n_lose, 2, "UNHELD is a loser once held");
        assert!(all[0].med_lose < 2.0, "its -500 must drag the loser median below LOSE's 2.0: {:?}", all[0]);

        // A bucket of one is skipped, exactly as `recall_capture` skips it, so both instruments
        // count the same pool — otherwise a single-name window would report a 100% one-sided split.
        assert!(held_loser_factors(&scored[..1], 1, &["roe"]).is_empty());

        // (#165) THE AUC IS WIRED AND IS NOT A CONSTANT. Above, both winners sit over the lone
        // loser, so the column reads a clean 1.0; here the cohorts INTERLEAVE — W(1) below L(5),
        // W2(9) above it — so one of the two pairs fails and the same column must read 0.5. A
        // hard-coded field passes the first and fails the second, which is why both are pinned.
        assert_eq!(rows[0].auc, 1.0, "roe separates WIN1/WIN2 from LOSE completely: {rows:?}");
        let ov = [
            row("W", 10.0, Some(1.0), None),
            row("L", -10.0, Some(5.0), None),
            row("W2", 20.0, Some(9.0), None),
        ];
        let ovs: Vec<(&Sample, f64)> = ov.iter().enumerate().map(|(i, x)| (x, 10.0 - i as f64)).collect();
        let r = held_loser_factors(&ovs, 3, &["roe"]);
        assert_eq!(r[0].auc, 0.5, "W(1) < L(5) < W2(9): one pair wins, one loses -> 0.5: {r:?}");
        // (#340) a bucket of exactly two is read, and a name that ends flat is a winner, not a loser.
        let edge = [row("W", 10.0, Some(9.0), None), row("FLAT", 0.0, Some(5.0), None), row("L", -10.0, Some(1.0), None)];
        let es: Vec<(&Sample, f64)> = edge.iter().enumerate().map(|(i, x)| (x, 10.0 - i as f64)).collect();
        assert_eq!(held_loser_factors(&es, 3, &["roe"])[0].n_win, 2, "FLAT (0.0) votes with the winners");
        assert_eq!(held_loser_factors(&[es[0], es[2]], 2, &["roe"]).len(), 1, "two names are enough to split");
    }

    /// (#1245) `event_flags` at a 2020-06-30 cutoff, 12 months back: each window is (from, as_of], so an
    /// event on the cutoff counts and one exactly a window back does not. The cut needs two prior payments
    /// and a fall to under 3/4 of them; a split needs a ratio under 1; an add needs a span opened after the
    /// source's first date.
    #[test]
    fn event_flags_read_each_event_inside_its_window() {
        let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let (as_of, origin) = (d("2020-06-30"), d("1996-01-02"));
        // an empty `spans` is a name that was never a member: `None`, as the walk passes it
        let flags = |divs: &[(&str, f64)], splits: &[(&str, f64)], spans: &[(&str, Option<&str>)], m: u32| {
            let divs: Vec<_> = divs.iter().map(|&(t, v)| (d(t), v)).collect();
            let splits: Vec<_> = splits.iter().map(|&(t, r)| (d(t), r)).collect();
            let spans: Vec<_> = spans.iter().map(|&(a, b)| (d(a), b.map(d))).collect();
            event_flags(&divs, &splits, (!spans.is_empty()).then_some(spans.as_slice()), origin, as_of, m)
        };
        // the dividend cut: 2018-06-30 is two years back (out), 2019-06-30 closes the prior year (in it)
        let paid = |last: f64| vec![("2018-06-30", 9.0), ("2018-12-31", 1.0), ("2019-06-30", 1.0), ("2020-06-30", last)];
        assert_eq!(flags(&paid(1.5), &[], &[], 12), 0, "exactly 3/4 of the prior year is no cut");
        assert_eq!(flags(&paid(1.49), &[], &[], 12), 1, "under 3/4 is a cut, the cutoff's own payment counted");
        assert_eq!(flags(&[("2018-12-31", 1.0), ("2019-06-30", 1.0)], &[], &[], 12), 1, "a stop is a cut");
        assert_eq!(flags(&[("2019-06-30", 1.0)], &[], &[], 12), 0, "one prior payment is no record to cut");
        let later = [("2018-12-31", 1.0), ("2019-06-30", 1.0), ("2020-07-01", 1.0)];
        assert_eq!(flags(&later, &[], &[], 12), 1, "a later payment is unseen");
        // the reverse split
        assert_eq!(flags(&[], &[("2020-06-30", 0.1)], &[], 12), 2, "a reverse split on the cutoff");
        assert_eq!(flags(&[], &[("2019-06-30", 0.1)], &[], 12), 0, "exactly a window back is out");
        assert_eq!(flags(&[], &[("2020-07-01", 0.1)], &[], 12), 0, "a later split is unseen");
        assert_eq!(flags(&[], &[("2020-01-01", 4.0), ("2020-02-01", 1.0)], &[], 12), 0, "a forward split or a ratio of 1 is no flag");
        assert_eq!(flags(&[], &[("2019-12-01", 0.1)], &[], 6), 0, "the window follows `months`");
        assert_eq!(flags(&[], &[("2019-12-01", 0.1)], &[], 12), 2);
        // the fresh add
        assert_eq!(flags(&[], &[], &[("2020-06-30", None)], 12), 4, "added on the cutoff");
        assert_eq!(flags(&[], &[], &[("2019-06-30", None)], 12), 0, "added exactly a window back is out");
        assert_eq!(flags(&[], &[], &[("2020-07-01", None)], 12), 0, "a later add is unseen");
        assert_eq!(flags(&[], &[], &[("2001-01-01", Some("2005-01-01")), ("2020-01-01", None)], 12), 4, "a re-add counts");
        let early = |start: &str| {
            let spans = [(d(start), None)];
            event_flags(&[], &[], Some(spans.as_slice()), origin, d("1996-06-30"), 12)
        };
        assert_eq!(early("1996-01-02"), 0, "the source's first date is no addition");
        assert_eq!(early("1996-03-01"), 4, "a span opened after it is one");
        assert_eq!(flags(&paid(0.0), &[("2020-03-01", 0.5)], &[("2020-01-01", None)], 12), 7, "the three bits stack");
    }

    /// (#1242) `held_loser_bands` makes four claims. (a) The point AUC is the held book's, and a flat
    /// name is a winner. (b) A one-sided factor is dropped and the family row is the mean of the rest.
    /// (c) The terciles split the held book by earnings yield, a third each. (d) The band resamples
    /// whole (bucket, sector) clusters: one cluster gives a band of width zero at the point, two
    /// clusters that disagree give the full spread.
    #[test]
    fn held_loser_bands_resample_clusters_and_split_by_earnings_yield() {
        use crate::core::FundFactors;
        let row = |t: &str, sector: &str, realized: f64, ep: f64, gm: f64, roe: f64, roic: Option<f64>| Sample {
            date: ymd(2010, 1, 1),
            realized,
            relative: 0.0,
            quote: Arc::new(Quote { sector: Some(sector.into()), ..Quote::stub(t, "1", "", t) }),
            fund: Some(FundFactors {
                earnings_yield: Some(ep),
                gross_margin: Some(gm),
                roe: Some(roe),
                roic,
                ..Default::default()
            }),
            trail: Vec::new(),
        };
        // One cluster. E/P order A C B E D F puts one winner and one loser in each third.
        let one = [
            row("A", "Tech", 10.0, 1.0, 9.0, 1.0, Some(1.0)),
            row("B", "Tech", 0.0, 3.0, 1.0, 2.0, Some(1.0)),
            row("C", "Tech", -10.0, 2.0, 5.0, 3.0, None),
            row("D", "Tech", 10.0, 5.0, 8.0, 4.0, Some(1.0)),
            row("E", "Tech", -10.0, 4.0, 2.0, 5.0, None),
            row("F", "Tech", -10.0, 6.0, 7.0, 6.0, None),
        ];
        let scored: Vec<(&Sample, f64)> = one.iter().enumerate().map(|(i, x)| (x, 10.0 - i as f64)).collect();
        let rows = held_loser_bands(&scored, 6, &["gross_margin", "roe", "roic"], 50);
        let names: Vec<&str> = rows.iter().map(|r| r.factor).collect();
        assert_eq!(names, ["gross_margin", "roe", "family"], "roic is winners-only and must drop: {rows:?}");
        // winners A9 B1 D8 vs losers C5 E2 F7: 6 of 9 pairs; roe winners 1 2 4 vs losers 3 5 6: 1 of 9
        assert_eq!(rows[0].auc, 6.0 / 9.0, "B ends flat and votes with the winners: {rows:?}");
        assert_eq!(rows[1].auc, 1.0 / 9.0);
        assert_eq!(rows[2].auc, (6.0 / 9.0 + 1.0 / 9.0) / 2.0, "family = mean of the factor rows");
        for r in &rows {
            assert_eq!((r.lo, r.hi), (r.auc, r.auc), "one cluster: every draw is the whole book: {r:?}");
        }
        assert_eq!(rows[0].terciles, [Some(1.0), Some(0.0), Some(1.0)], "A9>C5, B1<E2, D8>F7");
        assert_eq!(rows[1].terciles, [Some(0.0); 3], "A1<C3, B2<E5, D4<F6");
        assert_eq!(rows[2].terciles, [Some(0.5), Some(0.0), Some(0.5)]);

        // Two sectors that disagree: Tech separates fully, Energy fully backwards. The book reads 0.5,
        // and the draws must reach both ends.
        let two = [
            row("A", "Tech", 10.0, 1.0, 9.0, 0.0, None),
            row("C", "Tech", -10.0, 2.0, 5.0, 0.0, None),
            row("B", "Energy", 10.0, 3.0, 1.0, 0.0, None),
            row("E", "Energy", -10.0, 4.0, 2.0, 0.0, None),
        ];
        let scored: Vec<(&Sample, f64)> = two.iter().enumerate().map(|(i, x)| (x, 10.0 - i as f64)).collect();
        let gm = held_loser_bands(&scored, 4, &["gross_margin"], 200)[0];
        assert_eq!((gm.auc, gm.lo, gm.hi), (0.5, 0.0, 1.0), "{gm:?}");
        assert!(held_loser_bands(&[], 10, &["gross_margin"], 10).is_empty());
    }

    /// (#165) `auc` is the ruler the held-loser table ranks factors on, so the values that DEFINE it
    /// are pinned directly rather than inferred from a report: perfect separation, perfect inversion,
    /// no separation (twice, for two different reasons), an asymmetric denominator, and the monotone
    /// invariance the whole receipt leans on. The median gap this replaces can make none of these
    /// distinctions — that is why it exists — so a mutant collapsing any arm has to fail here.
    #[test]
    fn auc_is_a_unit_free_separation_statistic() {
        assert_eq!(auc(&[3.0, 4.0], &[1.0, 2.0]), 1.0, "every winner above every loser");
        assert_eq!(auc(&[1.0, 2.0], &[3.0, 4.0]), 0.0, "the same data with the roles swapped");
        // 0.5 twice: interleaved cohorts that tie on aggregate, and an all-ties cohort. The tie arm
        // is scored at a half, so a factor constant across the book reads "no separation", never 0.
        assert_eq!(auc(&[1.0, 2.0], &[1.0, 2.0]), 0.5, "interleaved");
        assert_eq!(auc(&[5.0, 5.0], &[5.0, 5.0]), 0.5, "all ties");
        // The denominator is n*m, not n+m: 2x1 with one strict win and one tie.
        assert_eq!(auc(&[3.0, 1.0], &[1.0]), 0.75);
        // MONOTONE INVARIANCE — the property (#165) rests on. Rescaling by 100 is exactly the
        // peg_yield-vs-accrual_gap confound that made the raw gap column rank factors by units.
        let (w, l) = ([3.0, 4.0, 1.5], [1.0, 2.0]);
        let (bw, bl): (Vec<f64>, Vec<f64>) =
            (w.iter().map(|x| x * 100.0).collect(), l.iter().map(|x| x * 100.0).collect());
        assert_eq!(auc(&w, &l), auc(&bw, &bl), "a rescaled factor must read identically");
        // Empty either side is a guard, not a path: nothing to separate reads as no separation.
        assert_eq!(auc(&[], &[1.0]), 0.5);
        assert_eq!(auc(&[1.0], &[]), 0.5);
    }

    /// (#340) The fs shell the screen footer reads through; the only test touching VERDICT_FILE in the
    /// lib-test scratch root.
    #[test]
    fn read_verdict_reads_the_journal_file() {
        let path = config::data_path(VERDICT_FILE);
        let j = Journal::from([(20, stub_verdict(20, VERDICT_TOP)), (8, stub_verdict(8, VERDICT_TOP))]);
        std::fs::write(&path, serde_json::to_string(&j).unwrap()).unwrap();
        let got = read_verdict().map(|v| v.years);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(got, Some(20), "the longest horizon is the row the footer cites");
        assert!(read_verdict().is_none(), "no file, no verdict");
    }

    /// (round 27) the journaled method verdict: serde roundtrip is identity (the screen reads back
    /// exactly what backtest wrote), corrupt/empty JSON is an empty journal (a broken file silences
    /// the footer, never fabricates a verdict), and verdict_line's drift arm swaps the rerun-pointer
    /// for the ⚠ stale-settings warning (citing stale numbers as current would mislead the buy
    /// decision). The line must name the BASKET too — a top-3 claim and a top-10 claim are different
    /// claims, which is why (#120) had to move the printed row and not only the const.
    #[test]
    fn verdict_journal_semantics() {
        let v = stub_verdict(12, VERDICT_TOP);
        let json = serde_json::to_string(&Journal::from([(v.years, stub_verdict(12, VERDICT_TOP))])).unwrap();
        let back = parse_journal(&json).into_values().next_back().expect("roundtrip parses");
        assert_eq!((back.date.as_str(), back.years, back.top, back.windows), ("2026-07-19", 12, VERDICT_TOP, 84));
        assert!((back.book - 14.3).abs() < 1e-9 && (back.excess - 6.9).abs() < 1e-9);
        assert_eq!(back.tuning_fp, "{\"a\":1}");

        assert!(parse_journal("not json").is_empty());
        assert!(parse_journal("").is_empty());
        assert!(parse_journal("{\"date\":\"x\"}").is_empty()); // missing fields -> empty, not a default
        assert!(parse_journal("{\"20\":{\"date\":\"x\"}}").is_empty()); // half-written row, same rule

        let fresh = verdict_line(&v, false, false);
        assert!(fresh.contains("run 2026-07-19, wide universe, top-10 held 12y, 84 windows"), "{fresh}");
        assert!(fresh.contains("book +14.3%/yr, +6.9pp/yr vs index, win 71%, worst -8.2, OOS +5.1/+7.4"));
        assert!(fresh.contains("(rerun: `folioman backtest 12 universe`)") && !fresh.contains('⚠'));
        let drifted = verdict_line(&v, true, false);
        assert!(drifted.ends_with("⚠ settings changed since, rerun `folioman backtest 12 universe`"), "{drifted}");
        assert!(!drifted.contains("(rerun:"));
    }

    /// The journal is keyed by horizon so a short run can't erase the long one, and the footer quotes
    /// the LONGEST row on file — "buy now, hold 8+ years" is graded at the hard end. A pre-journal
    /// file (one bare Verdict, no `top`) is adopted under its own horizon as the top-10 it was.
    #[test]
    fn verdict_journal_is_keyed_by_horizon() {
        let j = Journal::from([(8, stub_verdict(8, 3)), (20, stub_verdict(20, 3)), (12, stub_verdict(12, 3))]);
        let round = parse_journal(&serde_json::to_string(&j).unwrap());
        assert_eq!(round.len(), 3);
        assert_eq!(round.into_values().next_back().unwrap().years, 20, "footer must cite the longest hold");

        // legacy single-verdict file: adopted, not discarded, and honestly labelled top-10.
        let legacy = r#"{"date":"2026-08-05","years":20,"windows":39,"book":12.9,"excess":5.8,
            "win":82.0,"worst":-0.5,"oos_early":5.0,"oos_late":7.9,"tuning_fp":"{}"}"#;
        let adopted = parse_journal(legacy);
        assert_eq!(adopted.len(), 1);
        let only = adopted.into_values().next_back().unwrap();
        assert_eq!((only.years, only.top), (20, 10));
        assert!(verdict_line(&only, false, false).contains("top-10 held 20y"));
    }

    /// (#106) The skew claim, as arithmetic on ten windows: eight books that HALVED and two that went
    /// 20×. The mean says 4.4× and every headline in this report is built on that mean; the median
    /// says the typical window lost half its money, and P(book < 1.0×) says eight windows in ten did.
    /// Those are the same ten numbers summarised two ways, and only one of them is what a holder
    /// experiences — which is the whole argument for re-deriving `VERDICT_TOP` off the median and the
    /// 10th percentile rather than off the mean.
    #[test]
    fn the_mean_and_the_median_book_are_not_the_same_claim() {
        let mut m: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        for b in 0..8 {
            m.insert(b, vec![(1.0, -50.0, 0.0)]); // book 0.5×, index flat -> lost, and lost to the index
        }
        m.insert(8, vec![(1.0, 1900.0, 0.0)]); // two 20× windows carry the whole mean
        m.insert(9, vec![(1.0, 1900.0, 0.0)]);
        let (d, below_index, below_one, n) = book_deciles(&m, 1).unwrap();
        assert_eq!(n, 10);
        assert!((d[4] - 0.5).abs() < 1e-9, "the median window HALVED: {}", d[4]);
        assert!((d[8] - 20.0).abs() < 1e-9, "d90 is what made the mean: {}", d[8]);
        assert!((below_one - 0.8).abs() < 1e-9, "eight of ten ended below their starting value");
        assert!((below_index - 0.8).abs() < 1e-9);
        // the mean the ladder prints, off the SAME ten numbers — 4.4× against a 0.5× median.
        let mean_mult = book_multiples(&m, 1).iter().map(|(b, _)| b).sum::<f64>() / 10.0;
        assert!((mean_mult - 4.4).abs() < 1e-9, "{mean_mult}");
        assert!(book_deciles(&std::collections::BTreeMap::new(), 1).is_none()); // empty -> None, not 0%
        // (#340) a window that ties the index, ending exactly where it began, is below neither.
        let flat = std::collections::BTreeMap::from([(0, vec![(1.0, 0.0, 0.0)])]);
        let (_, below_index, below_one, _) = book_deciles(&flat, 1).unwrap();
        assert_eq!((below_index, below_one), (0.0, 0.0));
    }

    /// (#149) The vacuity census is a GUARD on every future receipt, so its boundaries are pinned
    /// rather than assumed. Each assert below kills one way the rule could silently loosen.
    #[test]
    fn basket_vacuity_marks_the_column_that_cannot_move() {
        // A bucket whose pool EQUALS the basket is saturated: the basket already holds everyone, so
        // nobody can cross the boundary. `<` instead of `<=` here would let the blindest possible
        // bucket read as measurable.
        assert_eq!(basket_vacuity(&[10], 10), (1, 0, 10, true));
        // One name outside the basket is one name that CAN cross — not saturated, and it is the
        // count of outsiders (not of names) that measures the column.
        assert_eq!(basket_vacuity(&[11], 10), (0, 1, 11, false));
        // A pool SMALLER than the basket contributes zero substitutable names, never a wrapped
        // count: `saturating_sub` is load-bearing, a bare `-` would panic or wrap to usize::MAX.
        assert_eq!(basket_vacuity(&[3], 10), (1, 0, 3, true));
        // VACUOUS needs a STRICT majority — an exact half-and-half split still adjudicates, so the
        // median bucket really is the line and `>=` would be a different, looser rule.
        assert_eq!(basket_vacuity(&[5, 5, 20, 20], 10), (2, 20, 20, false));
        assert_eq!(basket_vacuity(&[5, 5, 5, 20], 10), (3, 10, 5, true));
        // The census reports a MEDIAN, not a mean: one deep bucket must not hide a shelf of thin
        // ones, which is exactly the shape (#147) misread as a weak knob.
        assert_eq!(basket_vacuity(&[2, 3, 400], 10), (2, 390, 3, true));
        // Empty run: no buckets, so nothing is vacuous and nothing is claimed.
        assert_eq!(basket_vacuity(&[], 10), (0, 0, 0, false));
    }

    /// (#151) The census's non-saturated count IS the rank-1 h2h GUARD's denominator. `h2h_beats`
    /// stays silent when the pool cannot reach rank 11, and `basket_vacuity` calls exactly those
    /// buckets saturated — so `buckets - saturated` is not an estimate of the GUARD's sample size,
    /// it is that sample size. This test is why the census may print it as a fact. If VERDICT_TOP
    /// ever moves off the 10 hardcoded in `rank_slice_stats`'s SLICES, this fails, and the census
    /// line must stop claiming the identity rather than be re-blessed around it.
    #[test]
    fn the_h2h_denominator_is_the_unsaturated_bucket_count() {
        let row = |k: f64| (k, 0.0, 0.0, String::new());
        let mut m: std::collections::BTreeMap<i32, Vec<(f64, f64, f64, String)>> = Default::default();
        // Pools of 10 (saturated: no rank 11), 11 (one outsider) and 4 (saturated).
        for (bucket, n) in [(0, 10), (1, 11), (2, 4)] {
            m.insert(bucket, (0..n).map(|i| row(f64::from(i))).collect());
        }
        let pools: Vec<usize> = m.values().map(|v| v.len()).collect();
        let (sat, _, _, _) = basket_vacuity(&pools, VERDICT_TOP);
        let (_, h2h_mid, _) = rank_slice_stats(&m);
        assert_eq!(sat, 2, "10 <= basket and 4 <= basket are saturated; 11 is not");
        assert_eq!(h2h_mid.n, pools.len() - sat, "the GUARD tallies the non-saturated buckets");
        assert_eq!(h2h_mid.n, 1, "and here that is the single 11-name bucket");
    }

    #[test]
    fn sized_buckets_price_the_funded_book() {
        // (#290) Hand arithmetic on `Sizing::default()`: a lone stock renormalises to 100% of gross, clamps
        // at max_name_pct 8, and the other 92% earns the benchmark leg. Bench +50 over 1y on every date.
        let q = crate::commands::size::tests::scoring_quote;
        let d = NaiveDate::from_ymd_opt(2000, 1, 3).unwrap();
        let bench = (
            ["2000-01-03", "2000-07-03", "2001-01-03", "2001-07-03"].iter().map(|s| s.parse().unwrap()).collect(),
            vec![100.0, 100.0, 150.0, 150.0],
        );
        let at = |date, realized, quote| Sample { date, realized, relative: 0.0, quote: Arc::new(quote), fund: None, trail: Vec::new() };
        let (tuning, sz) = (BuyHeuristic::default(), config::Sizing::default());
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;

        // lone name: 0.08 * 2.0 + 0.92 * 1.5 = 1.54
        let one = [at(d, 100.0, q("AAA.DE", "Alpha", 900.0, 2.0))];
        let m = sized_buckets(&one, &bench, 1, &tuning, &sz, 10, &[]);
        let s = &m[&bucket(d)];
        assert!(close(s.book, 1.54) && close(s.spy, 1.5) && close(s.deployed, 8.0), "{} {} {}", s.book, s.spy, s.deployed);

        // (#308) the remainder earns the spill legs instead. A doubles; B starts AFTER the entry, so it earns
        // the bench leg rather than reading its later window (+200): 0.08 * 2.0 + 0.92 * (0.5 * 2.0 + 0.5 *
        // 1.5) = 1.77, and spy stays the benchmark leg.
        let a = (vec![d, NaiveDate::from_ymd_opt(2001, 1, 3).unwrap()], vec![100.0, 200.0]);
        let b = (vec![NaiveDate::from_ymd_opt(2000, 7, 3).unwrap(), NaiveDate::from_ymd_opt(2001, 7, 3).unwrap()], vec![100.0, 300.0]);
        let s = &sized_buckets(&one, &bench, 1, &tuning, &sz, 10, &[(0.5, &a), (0.5, &b)])[&bucket(d)];
        assert!(close(s.book, 1.77) && close(s.spy, 1.5) && close(s.deployed, 8.0), "{} {} {}", s.book, s.spy, s.deployed);

        // two issuers, both clamped at 8: 0.08 * 2.0 + 0.08 * 0.5 + 0.84 * 1.5 = 1.46, deployed 16. The worse
        // name comes FIRST in the input, so the cut at top-1 below proves ranking, not input order.
        // A gate-refused stub and an out-of-window date never enter the pool, so no bucket, no NaN.
        let late = NaiveDate::from_ymd_opt(2001, 1, 3).unwrap();
        let two = [
            at(d, -50.0, q("BBB.DE", "Beta", 500.0, 4.0)),
            at(d, 100.0, q("AAA.DE", "Alpha", 900.0, 2.0)),
            at(NaiveDate::from_ymd_opt(2000, 7, 3).unwrap(), 0.0, core::Quote::stub("DEAD.DE", "€1.00", "", "Dead")),
            at(late, 0.0, q("CCC.DE", "Gamma", 900.0, 2.0)),
        ];
        let m = sized_buckets(&two, &bench, 1, &tuning, &sz, 10, &[]);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec![bucket(d)]);
        let s = &m[&bucket(d)];
        assert!(close(s.book, 1.46) && close(s.spy, 1.5) && close(s.deployed, 16.0), "{} {} {}", s.book, s.spy, s.deployed);
        let s = &sized_buckets(&two, &bench, 1, &tuning, &sz, 1, &[])[&bucket(d)];
        assert!(close(s.book, 1.54) && close(s.deployed, 8.0), "top-1 keeps the better score: {}", s.book);

        // twin listings of one issuer fund ONE weight, the better-ranked one's; a 0.5 bench leg on the loser
        // would drag spy if it were counted.
        let twins = [at(d, -50.0, q("AAA2.DE", "Alpha", 700.0, 2.0)), at(d, 100.0, q("AAA.DE", "Alpha", 900.0, 2.0))];
        let s = &sized_buckets(&twins, &bench, 1, &tuning, &sz, 10, &[])[&bucket(d)];
        assert!(close(s.book, 1.54) && close(s.deployed, 8.0), "twin collapses onto the winner: {}", s.book);
    }

    #[test]
    fn book_stats_topn_held_book() {
        // top-1 by rank_key desc, held 1y. Bucket A: key 5 wins (real +100 -> 2x, bench 0). Bucket B:
        // one row (real 0 -> flat, bench +50). Book = mean(ann(multiple)) = (100+0)/2 = 50; SPY = (0+50)/2
        // = 25; excess = (100 + -50)/2 = 25; win 50%; worst -50; OOS early +100 / late -50.
        let mut m: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        m.insert(0, vec![(5.0, 100.0, 0.0), (1.0, -50.0, 0.0)]);
        m.insert(1, vec![(3.0, 0.0, 50.0)]);
        let (book, spy, excess, win, worst, early, late) = book_stats(&m, 1, 1).unwrap();
        assert!((book - 50.0).abs() < 1e-6, "book {book}");
        assert!((spy - 25.0).abs() < 1e-6, "spy {spy}");
        assert!((excess - 25.0).abs() < 1e-6, "excess {excess}");
        assert!((win - 50.0).abs() < 1e-6 && (worst + 50.0).abs() < 1e-6);
        assert!((early - 100.0).abs() < 1e-6 && (late + 50.0).abs() < 1e-6);
        assert!(book_stats(&std::collections::BTreeMap::new(), 1, 1).is_none()); // empty -> None

        // A bucket whose book leg lands EXACTLY on its bench leg is a tie, and a tie is not a win.
        // This pins `> 0.0` against `>= 0.0` in the win-rate filter, which the frozen-data report pin
        // structurally cannot see: on real prices an excess is a continuous float and never lands on
        // exactly zero, so both spellings print the same win%. Only a hand-built tie separates them.
        // Found by the mutation audit, not by a failing test — see tests/backtest_fixture.rs.
        let mut tie: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        tie.insert(0, vec![(1.0, 20.0, 20.0)]);
        let (.., win, _, _, _) = book_stats(&tie, 1, 1).unwrap();
        assert!(win.abs() < 1e-9, "an exact tie must not count as a win: win {win}");
    }

    #[test]
    fn onsale_book_row_prints_the_held_book_and_its_median() {
        // (#796) book_stats_topn_held_book's two buckets plus a third (real +20, bench +10), held 1y, top-1:
        // excesses +100/-50/+10, so the mean (+20) and the median (+10) differ; OOS splits 1 | 2.
        let mut m: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        m.insert(0, vec![(5.0, 100.0, 0.0), (1.0, -50.0, 0.0)]);
        m.insert(1, vec![(3.0, 0.0, 50.0)]);
        m.insert(2, vec![(1.0, 20.0, 10.0)]);
        assert_eq!(
            onsale_book_row(&m, 1, 1).unwrap(),
            "  ON-SALE top-1  book +40.0%/yr  vs S&P500 +20.0%/yr  ->  excess +20.0 (med +10.0) pts/yr   win 67% of 3   worst -50.0   OOS +100.0/-20.0"
        );
        assert!(onsale_book_row(&Default::default(), 1, 1).is_none());
    }

    /// (#67) The two BOOK PROBES that no test has ever touched: `drop_bottom_book` (the fund-factor
    /// floor) and `corr_cap_book` (the diversification cap). Both are fund-lane code, and every golden
    /// runs fund-less, so the frozen-data pins execute neither — they were invisible until a diff
    /// touched them and the mutation gate graded five surviving mutants in `drop_bottom_book` alone.
    ///
    /// WHAT THIS PINS, and why it is two asserts rather than a table of numbers. Both functions return
    /// `Option<(f64 x 7)>`, so cargo-mutants enumerates 3^7 constant tuples per function — 2188 mutants
    /// each, 4376 for the pair, which is 65% of everything a diff touching this file can generate. An
    /// empty pool returning None kills every `Some(<const>)` mutant at once, because a constant Some
    /// cannot report a book for a pool that has no rows; a populated pool returning Some kills the
    /// `None` mutant. The pair is complete against that whole class, and a hand-computed 7-tuple would
    /// add nothing to it — the arithmetic already belongs to `book_stats`, which owns its own pin.
    #[test]
    fn book_probes_reject_an_empty_pool_and_report_a_populated_one() {
        // Same construction as `walk_forward_edge_pin`: a closed-form series on a synthetic ~252/yr
        // calendar, which is the only shape known to clear growth_score's gates without a network.
        let n_bars = 13 * 252;
        let d0 = ymd(2010, 1, 4);
        let dates: Vec<NaiveDate> =
            (0..n_bars).map(|k| d0 + chrono::Duration::days(k as i64 * 365 / 252)).collect();
        let series = |g: f64, amp: f64, ph: f64| -> Vec<f64> {
            (0..n_bars)
                .map(|k| {
                    let t = k as f64 / 252.0;
                    100.0 * (1.0 + g).powf(t) * (1.0 + amp * (t * 2.7 + ph).sin())
                })
                .collect()
        };
        let universe: [(&str, Vec<f64>); 4] = [
            ("WIN1", series(0.22, 0.04, 0.0)),
            ("WIN2", series(0.17, 0.06, 1.0)),
            ("MID1", series(0.10, 0.08, 2.0)),
            ("LOSE", series(-0.08, 0.10, 5.0)),
        ];
        let years = 5;
        let mut samples: Vec<Sample> = Vec::new();
        for (rank, (tk, closes)) in universe.iter().enumerate() {
            let mut i = MIN_HISTORY;
            while i < dates.len() {
                let target = dates[i] + chrono::Duration::days(years * 365);
                let Some(off) = dates[i..].iter().position(|d| *d >= target) else { break };
                let realized = (closes[i + off] / closes[i] - 1.0) * 100.0;
                let quote = core::backtest_quote(tk, &dates, closes, &[], i, 252, &BTreeMap::new());
                samples.push(Sample {
                    date: dates[i],
                    realized,
                    relative: 0.0,
                    // a distinct factor level per name so the percentile floor has something to cut on,
                    // and a distinct trail so the correlation walk has something to judge
                    fund: Some(core::FundFactors { rev_cagr: Some(rank as f64 * 10.0), ..Default::default() }),
                    trail: (0..24).map(|m| (m as f64 * (rank as f64 + 1.0)).sin()).collect(),
                    quote: Arc::new(quote),
                });
                i += STEP_SESSIONS;
            }
        }
        demean(&mut samples);
        let tuning = BuyHeuristic::default();
        let (bd, bc) = (dates.clone(), universe[2].1.clone()); // MID1 as the benchmark leg

        // populated -> Some. Kills the `-> None` mutant in both.
        assert!(
            drop_bottom_book(&samples, &bd, &bc, years, &tuning, 2, 50.0, |f| f.rev_cagr).is_some(),
            "a scored pool with fund factors must produce a book"
        );
        assert!(
            corr_cap_book(&samples, &bd, &bc, years, &tuning, 2, f64::INFINITY).is_some(),
            "an uncapped correlation walk must reproduce the plain top-n book"
        );
        // empty -> None. Kills all 2187 `-> Some(<const tuple>)` mutants in each.
        assert!(
            drop_bottom_book(&[], &bd, &bc, years, &tuning, 2, 50.0, |f| f.rev_cagr).is_none(),
            "no rows cannot yield a book"
        );
        assert!(
            corr_cap_book(&[], &bd, &bc, years, &tuning, 2, f64::INFINITY).is_none(),
            "no rows cannot yield a book"
        );

        // (#340) The floor's arithmetic, which the Some/None pair above cannot see: p=0 is off, p=50
        // and p=100 cut different rows, p=100 indexes the top value rather than one past it, and a
        // floor every row sits on rejects nobody.
        let rev: fn(&core::FundFactors) -> Option<f64> = |f| f.rev_cagr;
        let d = |p: f64, g: fn(&core::FundFactors) -> Option<f64>| drop_bottom_book(&samples, &bd, &bc, years, &tuning, 2, p, g);
        let (off, half, top) = (d(0.0, rev), d(50.0, rev), d(100.0, rev));
        assert!(top.is_some(), "p=100 keeps the top name per bucket");
        assert_ne!(half, off, "p=50 must cut something");
        assert_ne!(half, top, "p=50 and p=100 are different floors");
        assert_ne!(top, off, "p=100 must cut something");
        assert_eq!(d(50.0, |_| Some(1.0)), off, "a floor every row sits on rejects nobody");

        // (#340) The verdict journal's source needs only two bench points: entry and exit. Anchored on
        // the last cutoff, every sample prices off the first point, so a two-point leg still forms a book.
        let d0 = samples.iter().map(|s| s.date).max().unwrap();
        let two = (vec![d0, d0 + chrono::Duration::days(20 * 365)], vec![100.0, 150.0]);
        assert!(report_entry_state(&samples, &two, years, &tuning).is_some(), "a two-point bench is a bench");
    }

    /// (round 106) `union_book`: dedupe by ticker (an overlapping pick takes ONE slot), value leg
    /// skips ey-None rows (all-None degrades to growth-only), equal-weight terminal-multiple math,
    /// empty pick set -> None.
    #[test]
    fn two_style_union_book() {
        let r = |tk: &str, score: f64, ey: Option<f64>, real: f64| (tk.to_string(), score, ey, real, 0.0);
        let rows = vec![
            r("A", 9.0, Some(2.0), 100.0), // growth #1, value #3
            r("B", 5.0, Some(8.0), 50.0),  // growth #2, value #2
            r("C", 1.0, Some(9.0), -50.0), // value #1
        ];
        let (b, s, n, ov) = union_book(&rows, 1, 0).unwrap(); // pure growth top-1 = A
        assert!((b - 2.0).abs() < 1e-9 && (s - 1.0).abs() < 1e-9 && n == 1 && ov == 0, "{b} {s} {n} {ov}");
        let (b, _, n, ov) = union_book(&rows, 0, 2).unwrap(); // pure value top-2 = C,B -> mean(0.5, 1.5)
        assert!((b - 1.0).abs() < 1e-9 && n == 2 && ov == 0, "{b} {n}");
        let (b, _, n, ov) = union_book(&rows, 2, 2).unwrap(); // A,B union C,B -> B overlaps once
        assert!((b - (2.0 + 1.5 + 0.5) / 3.0).abs() < 1e-9 && n == 3 && ov == 1, "{b} {n} {ov}");
        let noey = vec![r("A", 9.0, None, 100.0), r("B", 5.0, None, 50.0)];
        let (b, _, n, ov) = union_book(&noey, 1, 2).unwrap(); // no ey anywhere -> growth-only book
        assert!((b - 2.0).abs() < 1e-9 && n == 1 && ov == 0, "{b} {n}");
        assert!(union_book(&rows, 0, 0).is_none()); // nothing picked -> None
        // (#340) the SPY leg is the same equal-weight mean of terminal multiples: +20% and +40% -> 1.3x.
        let spy = vec![("A".to_string(), 9.0, None, 0.0, 20.0), ("B".to_string(), 5.0, None, 0.0, 40.0)];
        let (_, s, n, _) = union_book(&spy, 2, 0).unwrap();
        assert!((s - 1.3).abs() < 1e-9 && n == 2, "{s} {n}");
        // (#1067) nine windows: the last quarter holds three, so the newest window is in it.
        let nine = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
        assert_eq!(quarter_means(&nine), [1.5, 3.5, 5.5, 8.0]);
    }

    /// (round 108) `bench_drawdown_at`: at the high -> 0, halved -> −50 at the trough, recovered ->
    /// 0 again, and a date before the series -> None (no fabricated entry state).
    #[test]
    fn entry_state_drawdown() {
        let dates: Vec<NaiveDate> = (1..=4).map(|m| ymd(2020, m, 1)).collect();
        let closes = vec![100.0, 100.0, 50.0, 100.0];
        assert_eq!(bench_drawdown_at(&dates, &closes, ymd(2020, 2, 15)), Some(0.0)); // at the high
        assert_eq!(bench_drawdown_at(&dates, &closes, ymd(2020, 3, 15)), Some(-50.0)); // halved
        assert_eq!(bench_drawdown_at(&dates, &closes, ymd(2020, 5, 1)), Some(0.0)); // recovered to the high
        assert_eq!(bench_drawdown_at(&dates, &closes, ymd(2019, 12, 31)), None); // predates the series
    }

    /// (round 112) `corr_tail`: perfect co-movement -> +1, mirror -> −1, tails align when lengths
    /// differ, and the evidence bar holds — <12 overlapping months or a flat series -> None.
    #[test]
    fn pearson_correlation() {
        use crate::core::corr_tail;
        let t: Vec<f64> = (0..12).map(|i| if i % 2 == 0 { 1.0 } else { 2.0 }).collect();
        let anti: Vec<f64> = t.iter().map(|v| 3.0 - v).collect();
        assert!((corr_tail(&t, &t).unwrap() - 1.0).abs() < 1e-9);
        assert!((corr_tail(&t, &anti).unwrap() + 1.0).abs() < 1e-9);
        let long: Vec<f64> = [vec![9.0; 12], t.clone()].concat(); // 24mo whose last 12 == t
        assert!((corr_tail(&long, &t).unwrap() - 1.0).abs() < 1e-9); // aligned on the tail
        assert!(corr_tail(&t[..11], &anti[..11]).is_none()); // <12 overlap -> no verdict
        assert!(corr_tail(&[5.0; 12], &t).is_none()); // flat series -> no verdict
    }

    /// (round 112) `greedy_decorrelate`: a clone of a kept name is skipped and the next diversifier
    /// takes its slot; an empty trail can't be judged so it is KEPT; cap = INFINITY reproduces the
    /// plain top-n book (the probe's identity row).
    #[test]
    fn greedy_decorrelate_membership() {
        let t: Vec<f64> = (0..12).map(|i| if i % 2 == 0 { 1.0 } else { 2.0 }).collect();
        let anti: Vec<f64> = t.iter().map(|v| 3.0 - v).collect();
        let with = |trail: Vec<f64>| {
            let mut s = sample(ymd(2020, 1, 1), 0.0);
            s.trail = trail;
            s
        };
        let (a, b, c) = (with(t.clone()), with(t), with(anti));
        let ranked = vec![(9.0, &a), (8.0, &b), (7.0, &c)];
        let scores = |v: Vec<(f64, &Sample)>| v.into_iter().map(|(sc, _)| sc).collect::<Vec<_>>();
        // cap 0.8: b clones a (corr +1) -> skipped; c (corr −1) fills the slot
        assert_eq!(scores(greedy_decorrelate(&ranked, 2, 0.8)), vec![9.0, 7.0]);
        // cap off: plain rank order
        assert_eq!(scores(greedy_decorrelate(&ranked, 2, f64::INFINITY)), vec![9.0, 8.0]);
        // empty trail = unjudgeable -> kept even at a tight cap
        let blind = with(Vec::new());
        assert_eq!(scores(greedy_decorrelate(&[(9.0, &a), (8.0, &blind)], 2, 0.4)), vec![9.0, 8.0]);
    }

    /// (#1243) A crash bucket needs both legs: the pond's month down over 24 AND its last-6 spread in the top
    /// third so far. b4 is jumpy but up, and the fall in the first 12 of its 36 months is never read. b5
    /// compounds to exactly 1, not below. b6 clears the bar only because b4's reading sits under it. b7 is down
    /// with 4 of 7 below it, short of 2/3: b5's swing tops it only when the spread is read about the mean. b2's
    /// two names average, they do not add.
    #[test]
    fn crash_buckets_need_a_fall_and_a_top_third_spread() {
        let months = |lead: f64, a: f64, b: f64| -> Vec<f64> { (0..24).map(|i| if i < 18 { lead } else if i % 2 == 0 { a } else { b }).collect() };
        let at = |y: i32, m: u32, trail: Vec<f64>| {
            let mut s = sample(ymd(y, m, 1), 0.0);
            s.trail = trail;
            s
        };
        let pond = [
            at(2001, 1, months(1.0, 1.0, 1.0)),
            at(2001, 7, months(-1.0, -10.0, -6.0)),
            at(2001, 7, months(-1.0, -10.0, -6.0)),
            at(2002, 1, months(-1.0, 6.0, -4.0)),
            at(2002, 7, [vec![-50.0; 12], months(3.0, 11.0, -9.0)].concat()),
            at(2003, 1, months(0.0, 100.0, -50.0)),
            at(2003, 7, months(-1.0, 21.0, -19.0)),
            at(2004, 1, months(-1.0, 16.0, -14.0)),
        ];
        let want = std::collections::BTreeSet::from([bucket(ymd(2002, 1, 1)), bucket(ymd(2003, 7, 1))]);
        assert_eq!(crash_buckets(&pond), want);
        let t = BuyHeuristic { growth_momentum_slots: 5, growth_momentum_crash_slots: 1, ..BuyHeuristic::default() };
        let crash = std::collections::BTreeSet::from([7]);
        assert_eq!((slots_at(&t, &crash, 7), slots_at(&t, &crash, 8)), (1, 5));
        assert_eq!(slots_at(&BuyHeuristic { growth_momentum_crash_slots: 10, ..t.clone() }, &crash, 7), 5, "the default 10 caps nothing");
    }

    /// (Item 31) `exit_cohorts`: pairs where the earlier cutoff passes split on the later one —
    /// pass->pass lands in `kept`, pass->fail in `failed`; fail->anything and other tickers are
    /// ignored. Scorer keyed on drop_pct so the split logic is tested without building gated quotes.
    #[test]
    fn exit_cohorts_split() {
        let scorer: fn(&Quote, &BuyHeuristic) -> Option<f64> =
            |q, _| if q.drop_pct < 50.0 { Some(1.0) } else { None };
        let mk = |tk: &str, m: u32, drop: f64, rel: f64| {
            let mut s = sample(ymd(2020, m, 1), 0.0);
            let mut q = Quote::stub(tk, "1", "", tk);
            q.drop_pct = drop;
            s.quote = Arc::new(q);
            s.relative = rel;
            s
        };
        let samples = vec![
            mk("A", 1, 0.0, 1.0),  // passes
            mk("A", 7, 0.0, 2.0),  // pass -> pass: kept (rel 2.0)
            mk("A", 12, 99.0, 3.0), // pass -> FAIL: failed (rel 3.0)
            mk("B", 1, 99.0, 4.0), // never passes -> no pair counted
            mk("B", 7, 0.0, 5.0),
        ];
        let (kept, failed) = exit_cohorts(&samples, scorer, &BuyHeuristic::default());
        assert_eq!(kept, vec![2.0]);
        assert_eq!(failed, vec![3.0]);
    }

    /// (Item 31) `exit_probe` turns those two cohorts into a SELL-or-HOLD verdict, so it carries the
    /// same skew risk as `gate_audit` with a costlier wrong answer: acting on a tail-driven gap means
    /// selling a held position on evidence that describes one name. Both stats, and the verdict only
    /// on agreement. Sign convention is inverted here — a NEGATIVE gap is the actionable one.
    #[test]
    fn exit_probe_needs_both_stats_before_calling_a_sell() {
        let scorer: fn(&Quote, &BuyHeuristic) -> Option<f64> =
            |q, _| if q.drop_pct < 50.0 { Some(1.0) } else { None };
        // Each ticker gets two cutoffs: the first always passes, the second passes (kept) or fails.
        let pair = |tk: &str, fails: bool, rel: f64| {
            let mut a = sample(ymd(2020, 1, 1), 0.0);
            a.quote = Arc::new(Quote::stub(tk, "1", "", tk));
            let mut b = sample(ymd(2020, 7, 1), 0.0);
            let mut bq = Quote::stub(tk, "1", "", tk);
            bq.drop_pct = if fails { 99.0 } else { 0.0 };
            b.quote = Arc::new(bq);
            b.relative = rel;
            vec![a, b]
        };
        let build = |failed_rels: [f64; 4]| -> Vec<Sample> {
            let kept = ["K1", "K2", "K3", "K4"].iter().flat_map(|t| pair(t, false, 2.0));
            let failed = ["F1", "F2", "F3", "F4"]
                .iter().zip(failed_rels).flat_map(|(t, r)| pair(t, true, r));
            kept.chain(failed).collect()
        };
        // every newly-failed name underperformed the holders -> both stats agree -> real sell signal
        let (gap, gap_med) = exit_probe(&build([-10.0, -12.0, -14.0, -16.0]), scorer, &BuyHeuristic::default()).unwrap();
        assert!(gap < 0.0 && gap_med < 0.0, "unanimous underperformance -> both stats negative");
        // both gaps are failed minus kept: mean and median -13 against the holders' 2
        assert_eq!((gap, gap_med), (-15.0, -15.0));
        // THE TRAP: three of the four newly-failed names recovered; one collapsed hard enough to drag
        // the MEAN negative on its own. Mean-only, this prints "SELL". The median says hold.
        let (gap, gap_med) = exit_probe(&build([5.0, 6.0, 7.0, -900.0]), scorer, &BuyHeuristic::default()).unwrap();
        assert!(gap < 0.0, "the one collapse carries the mean");
        assert!(gap_med > 0.0, "three of four recovered — the typical name says hold");
        assert!(gap_verdict(gap, gap_med, "hold", "sell").starts_with("SPLIT"), "must not call a sell on the tail");
        // fewer than 4 on a side is no claim at all
        assert!(exit_probe(&build([-1.0; 4])[..6], scorer, &BuyHeuristic::default()).is_none());
        // (#340) BOTH sides need four: four kept against three newly-failed is still no claim.
        assert!(exit_probe(&build([-1.0; 4])[..14], scorer, &BuyHeuristic::default()).is_none());
        // and three kept against four newly-failed
        assert!(exit_probe(&build([-1.0; 4])[2..], scorer, &BuyHeuristic::default()).is_none());
    }

    /// (#820) `fresh_entries` keeps only fail->pass pairs: pass->pass is `exit_cohorts`'s kept,
    /// pass->fail its failed, and fail->fail never shows as a buy.
    #[test]
    fn fresh_entries_keep_only_fail_then_pass() {
        let scorer: fn(&Quote, &BuyHeuristic) -> Option<f64> =
            |q, _| if q.drop_pct < 50.0 { Some(1.0) } else { None };
        let mk = |tk: &str, m: u32, drop: f64, rel: f64| {
            let mut s = sample(ymd(2020, m, 1), 0.0);
            let mut q = Quote::stub(tk, "1", "", tk);
            q.drop_pct = drop;
            s.quote = Arc::new(q);
            s.relative = rel;
            s
        };
        let samples = vec![
            mk("A", 12, 0.0, 3.0), // pass -> pass: not fresh (out of date order on purpose)
            mk("A", 1, 99.0, 1.0), // fails
            mk("A", 7, 0.0, 2.0),  // fail -> PASS: fresh (rel 2.0)
            mk("B", 1, 99.0, 4.0), // fail -> fail: never a buy
            mk("B", 7, 99.0, 5.0),
            mk("C", 1, 0.0, 6.0), // pass -> fail: an exit, not an entry
            mk("C", 7, 99.0, 7.0),
        ];
        assert_eq!(fresh_entries(&samples, scorer, &BuyHeuristic::default()), vec![2.0]);
    }

    /// (#820) `entry_probe` reads fresh minus confirmed on both stats, and calls a verdict only when
    /// they agree. Confirmed names sit at +2.0 so a flipped subtraction cannot pass.
    #[test]
    fn entry_probe_gap_is_fresh_minus_confirmed() {
        let scorer: fn(&Quote, &BuyHeuristic) -> Option<f64> =
            |q, _| if q.drop_pct < 50.0 { Some(1.0) } else { None };
        // Each ticker gets two cutoffs: the second always passes; the first passed (confirmed) or failed (fresh).
        let pair = |tk: &str, fresh: bool, rel: f64| {
            let mut a = sample(ymd(2020, 1, 1), 0.0);
            let mut aq = Quote::stub(tk, "1", "", tk);
            aq.drop_pct = if fresh { 99.0 } else { 0.0 };
            a.quote = Arc::new(aq);
            let mut b = sample(ymd(2020, 7, 1), 0.0);
            b.quote = Arc::new(Quote::stub(tk, "1", "", tk));
            b.relative = rel;
            vec![a, b]
        };
        let build = |fresh_rels: [f64; 4]| -> Vec<Sample> {
            let confirmed = ["K1", "K2", "K3", "K4"].iter().flat_map(|t| pair(t, false, 2.0));
            let fresh = ["F1", "F2", "F3", "F4"].iter().zip(fresh_rels).flat_map(|(t, r)| pair(t, true, r));
            confirmed.chain(fresh).collect()
        };
        // fresh mean -13, median -13; confirmed +2 on both
        let got = entry_probe(&build([-10.0, -12.0, -14.0, -16.0]), scorer, &BuyHeuristic::default());
        assert_eq!(got, Some((-15.0, -15.0)));
        // one collapse drags the mean negative; the typical fresh passer beat the confirmed ones
        let (gap, gap_med) = entry_probe(&build([5.0, 6.0, 7.0, -900.0]), scorer, &BuyHeuristic::default()).unwrap();
        assert!(gap < 0.0 && gap_med > 0.0);
        assert!(gap_verdict(gap, gap_med, "buy", "wait").starts_with("SPLIT"));
        // both sides need four: 4 confirmed + 3 fresh, then 3 confirmed + 4 fresh
        assert!(entry_probe(&build([-1.0; 4])[..14], scorer, &BuyHeuristic::default()).is_none());
        assert!(entry_probe(&build([-1.0; 4])[2..], scorer, &BuyHeuristic::default()).is_none());
    }

    /// (#821) `book_entries` files each book name by the PREVIOUS bucket's book: a name it held is
    /// confirmed, one it did not is new. A ticker's last sample in a bucket wins, an unscored name never
    /// joins the book, a coin never counts, and a bucket whose previous bucket is missing is skipped.
    #[test]
    fn book_entries_split_on_the_previous_book() {
        let rel = |mut s: Sample, r: f64| {
            s.relative = r;
            s
        };
        let (a, b, c) = (ymd(2010, 1, 5), ymd(2010, 7, 5), ymd(2012, 1, 5));
        let samples = vec![
            swap_sample(a, "P1", 3.0, 0.0, 0.0),
            swap_sample(a, "P2", 2.0, 0.0, 0.0),
            swap_sample(a, "P3", 1.0, 0.0, 0.0),
            rel(swap_sample(b, "P1", 3.0, 0.0, 0.0), 99.0), // stale: the later P1 row replaces it
            rel(swap_sample(ymd(2010, 8, 5), "P1", 3.0, 0.0, 0.0), 1.0),
            rel(swap_sample(b, "P2", 2.0, 0.0, 0.0), 2.0),
            rel(swap_sample(b, "N1", 5.0, 0.0, 0.0), 7.0),
            rel(swap_sample(b, "P3", -1.0, 0.0, 0.0), 3.0), // unscored now: out of the book
            rel(swap_sample(b, "BTC-EUR", 100.0, 0.0, 0.0), 500.0), // a coin: never counts
            rel(swap_sample(c, "Q1", 1.0, 0.0, 0.0), 50.0), // no 2011 H2 bucket: nothing to compare
        ];
        let t = BuyHeuristic::default();
        assert_eq!(book_entries(&samples, swap_scorer, &t, &t), (vec![1.0, 2.0], vec![7.0]));
    }

    /// (#821) `book_entry_probe` reads new minus held on both stats. Held names sit at +2.0 so a flipped
    /// subtraction cannot pass.
    #[test]
    fn book_entry_probe_gap_is_new_minus_held() {
        // `held` of K1..K4 sit in the first book; all four K and every F sit in the second.
        let build = |held: usize, fresh_rels: &[f64]| -> Vec<Sample> {
            let mut v: Vec<Sample> = ["K1", "K2", "K3", "K4"][..held].iter().map(|t| swap_sample(ymd(2020, 1, 1), t, 1.0, 0.0, 0.0)).collect();
            for t in ["K1", "K2", "K3", "K4"] {
                let mut s = swap_sample(ymd(2020, 7, 1), t, 1.0, 0.0, 0.0);
                s.relative = 2.0;
                v.push(s);
            }
            for (t, r) in ["F1", "F2", "F3", "F4"].iter().zip(fresh_rels) {
                let mut s = swap_sample(ymd(2020, 7, 1), t, 1.0, 0.0, 0.0);
                s.relative = *r;
                v.push(s);
            }
            v
        };
        let t = BuyHeuristic::default();
        // new mean -13, median -13; held +2 on both
        assert_eq!(book_entry_probe(&build(4, &[-10.0, -12.0, -14.0, -16.0]), swap_scorer, &t, &t), Some((-15.0, -15.0)));
        // one collapse drags the mean negative; the typical new entrant beat the held names
        let (gap, gap_med) = book_entry_probe(&build(4, &[5.0, 6.0, 7.0, -900.0]), swap_scorer, &t, &t).unwrap();
        assert!(gap < 0.0 && gap_med > 0.0);
        // both sides need four: 3 held + 4 new (K4 joins late), then 4 held + 3 new
        assert!(book_entry_probe(&build(3, &[-1.0; 3]), swap_scorer, &t, &t).is_none());
        assert!(book_entry_probe(&build(4, &[-1.0; 3]), swap_scorer, &t, &t).is_none());
    }

    /// Peer-bucket key + the de-meaning that turns realized returns into the SELECTION signal rho
    /// measures. If `bucket` or `demean` drift, rho would race calendar luck instead of skill.
    #[test]
    fn peer_relative_math() {
        // bucket: 2 per year (H1 = Jan-Jun, H2 = Jul-Dec), monotone across the year boundary
        assert_eq!(bucket(ymd(2020, 1, 1)), bucket(ymd(2020, 6, 30))); // same half-year -> same bucket
        assert_ne!(bucket(ymd(2020, 6, 30)), bucket(ymd(2020, 7, 1))); // H1 vs H2 split
        assert!(bucket(ymd(2020, 12, 31)) < bucket(ymd(2021, 1, 1))); // strictly increases over years

        // demean: two cutoffs in 2020-H1 (realized 10/30 -> mean 20 -> relatives -10/+10); singletons
        // in other buckets net to exactly 0. Each bucket's relatives must sum to ~0 (the run invariant).
        let mut s = [
            sample(ymd(2020, 2, 1), 10.0),
            sample(ymd(2020, 3, 1), 30.0),
            sample(ymd(2020, 9, 1), 5.0),   // alone in 2020-H2
            sample(ymd(2021, 2, 1), 100.0), // alone in 2021-H1
        ];
        demean(&mut s);
        assert!((s[0].relative - -10.0).abs() < 1e-9);
        assert!((s[1].relative - 10.0).abs() < 1e-9);
        assert!(s[2].relative.abs() < 1e-9); // singleton bucket -> de-means to 0
        assert!(s[3].relative.abs() < 1e-9);
        // per-bucket sums net to ~0
        let mut sums: HashMap<i32, f64> = HashMap::new();
        for x in &s {
            *sums.entry(bucket(x.date)).or_insert(0.0) += x.relative;
        }
        assert!(sums.values().all(|v| v.abs() < 1e-9));
    }

    /// (#1 class split) De-mean groups by (bucket, asset class): a +1e9 crypto in the SAME bucket as two
    /// stocks must NOT move the stocks' peer-mean — else crypto's scale swamps the equity edge to noise.
    #[test]
    fn demean_splits_by_asset_class() {
        let stock = |r: f64| Sample { date: ymd(2020, 2, 1), realized: r, relative: 0.0, quote: Arc::new(Quote::stub("X", "1", "", "X")), fund: None, trail: Vec::new() };
        let crypto = |r: f64| Sample { date: ymd(2020, 2, 1), realized: r, relative: 0.0, quote: Arc::new(Quote::stub("BTC-USD", "1", "", "Bitcoin")), fund: None, trail: Vec::new() };
        let mut s = [stock(10.0), stock(30.0), crypto(1e9)];
        demean(&mut s);
        assert!((s[0].relative - -10.0).abs() < 1e-9); // stock peer-mean = 20, unmoved by the crypto
        assert!((s[1].relative - 10.0).abs() < 1e-9);
        assert!(s[2].relative.abs() < 1e-9); // crypto alone in its class -> de-means to 0
    }

    /// A synthetic scorer reading one quote field — lets us test `lane_metrics`/`edge_halves` (the
    /// honest-OOS machinery the search drives) WITHOUT building quotes that pass growth_score's gate
    /// maze (those gates are exercised in `picks`). Score = drawdown_pct, set per sample below.
    fn dd_score(q: &Quote, _: &BuyHeuristic) -> Option<f64> {
        Some(q.drawdown_pct)
    }
    fn s_rel(relative: f64, dd: f64) -> Sample {
        let mut q = Quote::stub("X", "1", "", "X");
        q.drawdown_pct = dd;
        Sample { date: ymd(2020, 1, 1), realized: relative, relative, quote: Arc::new(q), fund: None, trail: Vec::new() }
    }

    /// `lane_metrics` is what the tune search ranks configs by, so its rho/edge must have the right SIGN:
    /// a score that tracks the peer-relative return reads +rho / +edge; the reverse reads negative.
    #[test]
    fn lane_metrics_sign() {
        // score (dd) rises with the peer-relative return -> perfect rank agreement
        let up: Vec<Sample> = [(-3.0, 1.0), (-2.0, 2.0), (-1.0, 3.0), (1.0, 4.0), (2.0, 5.0), (3.0, 6.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (rho, edge) = lane_metrics(&up, dd_score, &BuyHeuristic::default());
        assert!(rho.unwrap() > 0.9, "monotone agreement -> rho≈+1, got {rho:?}");
        assert!(edge > 0.0, "winners-first -> positive top-minus-bottom edge, got {edge}");
        // flip the score order against the returns -> selection goes backwards
        let down: Vec<Sample> = [(-3.0, 6.0), (-2.0, 5.0), (-1.0, 4.0), (1.0, 3.0), (2.0, 2.0), (3.0, 1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (rho2, edge2) = lane_metrics(&down, dd_score, &BuyHeuristic::default());
        assert!(rho2.unwrap() < -0.9 && edge2 < 0.0, "reversed -> negative rho/edge, got {rho2:?}/{edge2}");
        // too few gated rows -> rho None (the <4 guard the search must tolerate)
        assert!(lane_metrics(&up[..3], dd_score, &BuyHeuristic::default()).0.is_none());
    }

    /// (Item 5) `percentile` is nearest-rank on a sorted slice: p5/p95 land near the ends, p50 mid, and
    /// an empty slice is NaN (never panics). This is what turns the bootstrap edge distribution into a band.
    #[test]
    fn percentile_nearest_rank() {
        let s: Vec<f64> = (0..=100).map(|i| i as f64).collect(); // 0..100 sorted
        assert_eq!(percentile(&s, 0.0), 0.0);
        assert_eq!(percentile(&s, 100.0), 100.0);
        assert_eq!(percentile(&s, 50.0), 50.0);
        assert_eq!(percentile(&s, 5.0), 5.0);
        assert_eq!(percentile(&s, 95.0), 95.0);
        assert!(percentile(&[], 50.0).is_nan());
        // p above 100 clamps to the last element instead of indexing past the end. Unreachable from the
        // shipped callers (deciles, medians, quartiles, 1/99 trims and the bootstrap band's tails, all
        // inside 0..=100), so the clamp is dead code the report pin can never exercise — the mutation
        // audit flags it as unkilled, and this is the only way to observe it.
        assert_eq!(percentile(&s, 150.0), 100.0);
    }

    /// (Item 9) `turnover_frac` = 1 − mean share kept of consecutive buckets' top-half tickers. Two ~6mo
    /// buckets holding {A,B} then {A,C} keep 1 of 2 -> turnover 1/2 (#1075; the old Jaccard read 2/3); a
    /// single bucket can't be measured -> 0.
    #[test]
    fn turnover_frac_consecutive_buckets() {
        // month 13 on is 2021, so a case can span three ~6mo buckets.
        let mk = |t: &str, m: u32| Sample {
            date: ymd(2020 + (m as i32 - 1) / 12, (m - 1) % 12 + 1, 1),
            realized: 0.0,
            relative: 0.0,
            quote: Arc::new(Quote::stub(t, "1", "", t)),
            fund: None,
            trail: Vec::new(),
        };
        // bucket H1: A,B,C,D (top-half by score -> A,B). bucket H2: A,C,E,F (top-half -> A,C).
        let s = [mk("A", 1), mk("B", 2), mk("C", 3), mk("D", 4), mk("A", 7), mk("C", 8), mk("E", 9), mk("F", 10)];
        let scored: Vec<(&Sample, f64)> =
            s.iter().enumerate().map(|(i, smp)| (smp, if i % 4 < 2 { 9.0 } else { 1.0 })).collect();
        assert!((turnover_frac(&scored) - 0.5).abs() < 1e-9, "got {}", turnover_frac(&scored));
        assert_eq!(turnover_frac(&scored[..4]), 0.0); // single bucket -> unmeasurable -> 0
        // (#1075) Unequal halves over three buckets: {A,B} -> {A,C,E,G} keeps 1 of the larger book's 4,
        // then {A,C,E,G} -> {A,C} keeps 2 of 4, so turnover = 1 − (1/4 + 2/4) / 2 = 5/8. Jaccard read
        // 13/20 and a smaller-side denominator would read 1/4.
        let u = [
            ("A", 1), ("B", 2), ("C", 3), ("D", 4), ("A", 7), ("C", 7), ("E", 8), ("G", 8), ("H", 9), ("I", 9),
            ("J", 10), ("K", 10), ("A", 13), ("C", 13), ("X", 14), ("Y", 14),
        ]
        .map(|(t, m)| mk(t, m));
        let scored: Vec<(&Sample, f64)> =
            u.iter().enumerate().map(|(i, smp)| (smp, if matches!(i, 0..2 | 4..8 | 12..14) { 9.0 } else { 1.0 })).collect();
        assert!((turnover_frac(&scored) - 0.625).abs() < 1e-9, "got {}", turnover_frac(&scored));
    }

    /// (Item 12) `winsor_edge` clamps the 1/99 tails so one extreme row can't BE the edge. 100 modest rows
    /// read a small spread; plant a 500-pt outlier in the top half and the RAW edge explodes while the
    /// winsorized edge barely moves — exactly the fragility flag we want.
    #[test]
    fn winsor_edge_clamps_outlier() {
        // relatives ~-2..+2, score == relative so the score-halves coincide with the relative-halves.
        let mut samples: Vec<Sample> = (0..100)
            .map(|i| {
                let r = (i as f64 - 50.0) / 25.0;
                Sample { date: ymd(2020, 1, 1), realized: r, relative: r, quote: Arc::new(Quote::stub("X", "1", "", "X")), fund: None, trail: Vec::new() }
            })
            .collect();
        samples[99].relative = 500.0; // a 500-pt blow-up in the highest-scored row
        let scored: Vec<(&Sample, f64)> = samples.iter().map(|s| (s, s.relative)).collect();
        let (t, b) = edge_halves(&scored);
        let raw = t - b;
        let wins = winsor_edge(&scored);
        assert!(raw > 5.0, "the planted outlier blows up the raw edge, got {raw}");
        assert!(wins < raw - 4.0, "winsorizing the 1/99 tails kills most of the artifact, got {wins} vs raw {raw}");
    }

    /// `pick_sweep_winner` is the sweep's verdict: it must take the BEST held-out edge among factors that
    /// beat the price-only baseline AND keep both OOS halves positive, and return None when none qualify
    /// (so the sweep ships nothing). One reject per disqualifier, plus the empty case.
    #[test]
    fn pick_sweep_winner_gates_on_oos_and_baseline() {
        let baseline = 9.0;
        let r = [
            ("rev_accel", 9.4, Some(0.03), Some(0.01)),    // beats baseline, both OOS + -> candidate
            ("margin_trend", 11.0, Some(0.07), Some(0.05)), // highest edge, both + -> WINNER
            ("eps_growth", 12.0, Some(0.04), Some(-0.02)),  // higher edge but one OOS half negative -> reject
            ("op_margin", 8.0, Some(0.02), Some(0.02)),     // below baseline -> reject
        ];
        assert_eq!(pick_sweep_winner(&r, baseline), Some("margin_trend"));
        // nothing clears the bar (one below baseline, one missing an OOS half) -> None (ship nothing)
        let none = [("x", 8.0, Some(0.1), Some(0.1)), ("y", 10.0, Some(0.1), None)];
        assert_eq!(pick_sweep_winner(&none, 9.0), None);
        // (#340) every bar is strict: an edge only EQUAL to the baseline, or a flat OOS half, earns nothing.
        assert_eq!(pick_sweep_winner(&[("tie", 9.0, Some(0.1), Some(0.1))], 9.0), None);
        assert_eq!(pick_sweep_winner(&[("a0", 10.0, Some(0.0), Some(0.1))], 9.0), None);
        assert_eq!(pick_sweep_winner(&[("b0", 10.0, Some(0.1), Some(0.0))], 9.0), None);
    }

    /// `edge_terciles` must read the score-sorted gradient: a score that tracks the return reads a
    /// monotone top>mid>bot; a score that only separates the EXTREMES (mid sags below bot) reads
    /// SCRAMBLED — the fragility the 2-bucket edge hides.
    #[test]
    fn edge_terciles_catches_scramble() {
        // score == relative -> perfect ranking -> top > mid > bot
        let mono: Vec<Sample> = (1..=9).map(|i| s_rel(i as f64, 0.0)).collect();
        let pairs: Vec<(&Sample, f64)> = mono.iter().map(|s| (s, s.relative)).collect();
        let (t, m, b) = edge_terciles(&pairs);
        assert!(t > m && m > b, "monotone score -> ordered terciles, got {t}/{m}/{b}");

        // build a score that puts the LOWEST relatives in the MIDDLE tercile: top rows + a sagging middle.
        // relatives by descending score: [9,8,7, 1,2,3, 4,5,6] -> top mean 8, mid mean 2, bot mean 5 -> mid<bot
        let rels = [9.0, 8.0, 7.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let scr: Vec<Sample> = rels.iter().map(|&r| s_rel(r, 0.0)).collect();
        let pairs: Vec<(&Sample, f64)> =
            scr.iter().enumerate().map(|(i, s)| (s, (rels.len() - i) as f64)).collect(); // score desc
        let (t, m, b) = edge_terciles(&pairs);
        assert!(!(t > m && m > b) && m < b, "sagging middle -> SCRAMBLED (mid<bot), got {t}/{m}/{b}");

        // <3 rows -> all NaN (no spread to read)
        assert!(edge_terciles(&pairs[..2]).0.is_nan());
    }

    /// A scorer that READS one tuning field — so perturbing THAT field changes scores (the dim is live),
    /// while perturbing a field it ignores leaves scores identical (inert). Pins `dim_active`, the probe
    /// `tune` uses to drop no-effect weights like growth_fund_weight on a fund-less sample.
    fn trend_dd(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
        Some(t.growth_trend_weight * q.drawdown_pct)
    }
    #[test]
    fn dim_active_detects_inert() {
        let s: Vec<Sample> = [1.0, 2.0, 3.0, 4.0].iter().map(|&d| s_rel(0.0, d)).collect();
        let def = BuyHeuristic::default();
        // trend_dd reads growth_trend_weight -> perturbing it moves scores -> ACTIVE
        assert!(dim_active(&s, trend_dd, &def, |t, v| t.growth_trend_weight = v, 1.0));
        // trend_dd never reads growth_fund_weight -> perturbing it is a no-op -> INERT (the case that
        // skips growth_fund_weight when no cutoff carries an as-of fundamental)
        assert!(!dim_active(&s, trend_dd, &def, |t, v| t.growth_fund_weight = v, 0.5));
    }

    /// (#6) the `stress` injection must ADD every loser the pool lacks and DUPLICATE none already in it —
    /// else a loser that's also a current index member gets scored twice, skewing the peer buckets. Pins
    /// the dedup-filter in `run` (kept identical here: filter STRESS_TICKERS by a set of what's present).
    #[test]
    fn stress_injection_dedups() {
        assert_eq!(STRESS_TICKERS.iter().collect::<HashSet<_>>().len(), STRESS_TICKERS.len(), "STRESS list has a dup");
        // a universe that already holds GE + INTC (two of the losers) plus an unrelated name
        let mut tickers: Vec<String> = ["AAPL", "GE", "INTC"].iter().map(|s| s.to_string()).collect();
        let have: HashSet<&str> = tickers.iter().map(String::as_str).collect();
        let added: Vec<String> =
            STRESS_TICKERS.iter().filter(|t| !have.contains(**t)).map(|t| (*t).to_string()).collect();
        tickers.extend(added);
        // every loser is present exactly once; the pre-existing GE/INTC weren't re-added
        let uniq: HashSet<&str> = tickers.iter().map(String::as_str).collect();
        assert_eq!(uniq.len(), tickers.len(), "injection duplicated a ticker");
        assert!(STRESS_TICKERS.iter().all(|t| uniq.contains(*t)), "a loser is missing from the pool");
        assert!(uniq.contains("AAPL")); // the unrelated universe name survives
    }

    /// (PIT) The pool swap, and specifically the two things it must NOT do. It must not touch a name
    /// outside the index pond — that is where the crypto and ETF lanes live, and filtering those by S&P
    /// 500 membership would zero two of the three asset classes the report prints by class. And it must
    /// not keep today's survivors merely because they are also historical members: they come back
    /// through the membership map, once, on the same footing as the dead ones.
    #[test]
    fn pit_pool_swaps_the_index_pond_and_leaves_the_other_lanes_alone() {
        // a wide-path pool: two index stocks, one ETF, one coin. `sector_of` marks the index pond.
        let tickers: Vec<String> = ["AAPL", "IWDA.L", "GE", "BTC-EUR"].iter().map(|s| s.to_string()).collect();
        let sector_of: HashMap<String, String> = [("AAPL", "Information Technology"), ("GE", "Industrials")]
            .iter()
            .map(|(t, s)| ((*t).to_string(), (*s).to_string()))
            .collect();
        let spans: core::MemberSpans = [
            ("AAPL", vec![("1996-01-02".parse().expect("date"), None)]),
            ("GE", vec![("1996-01-02".parse().expect("date"), None)]),
            // the whole reason this exists: a name that is NOT in today's pond at all
            ("SBNY", vec![("2021-12-20".parse().expect("date"), Some("2023-03-15".parse().expect("date")))]),
        ]
        .into_iter()
        .map(|(t, v)| (t.to_string(), v))
        .collect();

        let pool = pit_pool(&tickers, &sector_of, &spans);
        assert!(pool.contains(&"SBNY".to_string()), "a DEAD member is the point — it must arrive");
        assert!(pool.contains(&"IWDA.L".to_string()) && pool.contains(&"BTC-EUR".to_string()),
                "the ETF and crypto lanes are not index names and must survive the swap untouched");
        assert_eq!(pool.iter().filter(|t| *t == "AAPL").count(), 1, "a survivor is not added twice");
        assert_eq!(pool, vec!["AAPL", "BTC-EUR", "GE", "IWDA.L", "SBNY"], "sorted and deduped, nothing else");

        // An EMPTY membership map would otherwise DELETE the index pond and score ETFs and coins alone,
        // which is why the caller refuses to swap on one. Pinned here so that guard cannot drift.
        assert_eq!(pit_pool(&tickers, &sector_of, &core::MemberSpans::new()), vec!["BTC-EUR", "IWDA.L"]);
    }

    /// (PIT) The swap guard's four cases. Each `false` here is a distinct disaster the guard exists to
    /// refuse, and the `run` call site is inside a `-> ()` that only a live `universe` fetch reaches —
    /// so this is the only place either half can be held to account.
    #[test]
    fn pit_swaps_pool_needs_both_a_source_and_an_index_pond() {
        let spans: core::MemberSpans =
            [("AAPL".to_string(), vec![("1996-01-02".parse().expect("date"), None)])].into_iter().collect();
        let pond: HashMap<String, String> =
            [("AAPL".to_string(), "Information Technology".to_string())].into_iter().collect();

        assert!(pit_swaps_pool(&spans, &pond), "a live source and a wide-path pond: swap");
        assert!(!pit_swaps_pool(&core::MemberSpans::new(), &pond), "no source -> swapping would DELETE the pond");
        assert!(!pit_swaps_pool(&spans, &HashMap::new()), "no pond -> an explicit ticker list, keep what was asked for");
        assert!(!pit_swaps_pool(&core::MemberSpans::new(), &HashMap::new()), "neither, plainly nothing to do");
    }

    /// (PIT) The unserved count. It exists because the failure it names is INVISIBLE by default: a dead
    /// ticker fetches nothing, its ticket is dropped, and the run prints a pool size that never mentions
    /// it. Counted over the POOL, so a name in the membership map that this run never asked for is not
    /// blamed on Yahoo.
    #[test]
    fn pit_unserved_counts_only_pool_members_yahoo_dropped() {
        let spans: core::MemberSpans = ["AAPL", "SBNY", "AAMRQ", "ABI"]
            .iter()
            .map(|t| ((*t).to_string(), vec![("1996-01-02".parse().expect("date"), None)]))
            .collect();
        let pool: Vec<String> = ["AAPL", "SBNY", "AAMRQ", "BTC-EUR"].iter().map(|s| s.to_string()).collect();
        let served: HashSet<&str> = ["AAPL", "BTC-EUR"].into_iter().collect();

        assert_eq!(pit_unserved(&pool, &spans, &served), 2, "SBNY and AAMRQ were asked for and came back empty");
        // ABI is in the map but NOT in this pool: not asked for, so not a miss. Counting it would make
        // every narrow run report hundreds of phantom misses.
        // BTC-EUR served fine and is not a member anyway — neither half of the filter alone is enough.
        assert_eq!(pit_unserved(&pool, &spans, &["AAPL", "SBNY", "AAMRQ", "BTC-EUR"].into_iter().collect()), 0);
        assert_eq!(pit_unserved(&[], &spans, &served), 0, "no pool, no misses");
        assert_eq!(pit_unserved(&pool, &core::MemberSpans::new(), &served), 0, "pit off -> nothing to miss");
    }

    /// (#122) The dead-mapping signature, pinned on the real payloads it was derived from.
    ///
    /// Both halves are load-bearing and the test says so by breaking each one alone: a real mutual fund
    /// carries a real name, and a live equity that merely left the index carries an `EQUITY` type. The
    /// measured basis is 6149 cached series, where this matches 134 tickers, all of them S&P members,
    /// with no false positive anywhere in the non-member remainder.
    #[test]
    fn dead_ticker_mapping_needs_both_a_fund_type_and_a_numeric_name() {
        // the exact shortNames Yahoo returns for these, 2026-08-22.
        assert!(ticker_mapping_is_dead("MUTUALFUND", "3847602"), "CFC, Countrywide");
        assert!(ticker_mapping_is_dead("MUTUALFUND", "1315901"), "BSC, Bear Stearns");
        assert!(ticker_mapping_is_dead("MUTUALFUND", "4595480"), "WYE, Wyeth");
        assert!(ticker_mapping_is_dead("mutualfund", "655556"), "Yahoo's casing is not a contract");

        // a REAL fund keeps a real name — WLDHC.PA is in the live cache and must survive.
        assert!(!ticker_mapping_is_dead("MUTUALFUND", "Amundi MSCI World Swap II UCITS"));
        // a live equity that left the index is not dead: AMD left in 2013 and came back.
        assert!(!ticker_mapping_is_dead("EQUITY", "Advanced Micro Devices, Inc."));
        // the reuse cases this is deliberately blind to — asserted so the blindness is a decision.
        assert!(!ticker_mapping_is_dead("EQUITY", "Beam Therapeutics Inc."), "reused by a LIVE equity");
        assert!(!ticker_mapping_is_dead("ETF", "VanEck Digital Native"), "GENZ, reused by an ETF");
        // an absent name must not read as "all digits" vacuously — `chars().all` is true on empty.
        assert!(!ticker_mapping_is_dead("MUTUALFUND", ""), "no name is not a numeric name");
        // a name that merely CONTAINS digits is a real name.
        assert!(!ticker_mapping_is_dead("MUTUALFUND", "3M Company"));
    }

    /// (#121) The coverage ratio the miss count cannot see. `pit_unserved` asks "did Yahoo answer?";
    /// this asks "did the walk score it?", and the gap between those two questions is where 574 of the
    /// 703 removed S&P members live — served, non-empty as a response, and carrying no usable bars.
    ///
    /// The two numbers must move INDEPENDENTLY, which is what this pins: a name can be served and
    /// unscoreable (the empty-stub case, the whole point), and the denominator must count members the
    /// pool asked about rather than the whole membership map.
    #[test]
    fn pit_coverage_separates_served_from_scoreable() {
        let spans: core::MemberSpans = ["AAPL", "BSC", "TWX", "ABI"]
            .iter()
            .map(|t| ((*t).to_string(), vec![("1996-01-02".parse().expect("date"), None)]))
            .collect();
        let pool: Vec<String> = ["AAPL", "BSC", "TWX", "BTC-EUR"].iter().map(|s| s.to_string()).collect();

        // BSC and TWX are exactly the measured case: Yahoo SERVED them, the walk scored neither.
        let served: HashSet<&str> = ["AAPL", "BSC", "TWX", "BTC-EUR"].into_iter().collect();
        assert_eq!(pit_unserved(&pool, &spans, &served), 0, "nothing missing — and that is the trap");
        let scored: HashSet<&str> = ["AAPL", "BTC-EUR"].into_iter().collect();
        assert_eq!(
            pit_coverage(&pool, &spans, &scored),
            (3, 1),
            "3 members asked about, 1 scored — the stub-served pair is the hole a miss count reports as zero"
        );

        // ABI is in the map but not in this pool: it must not inflate the denominator.
        assert_eq!(pit_coverage(&pool, &spans, &["AAPL", "BSC", "TWX", "ABI"].into_iter().collect()), (3, 3));
        // BTC-EUR is scored but is not a member: it must not inflate the numerator either.
        assert_eq!(pit_coverage(&pool, &spans, &["BTC-EUR"].into_iter().collect()), (3, 0));
        assert_eq!(pit_coverage(&[], &spans, &scored), (0, 0), "no pool, nothing to cover");
        assert_eq!(pit_coverage(&pool, &core::MemberSpans::new(), &scored), (0, 0), "pit off -> no members");
    }

    /// (#751) The archive loader. A series ending before the file's newest bar is DEAD and must have
    /// traded to within 30 days of its last S&P exit (GONE sits exactly on that edge, EARLY a day past it,
    /// OPEN never left); one reaching the end is censored and kept even though it left the index later.
    #[test]
    fn dead_member_charts_split_dead_censored_and_refused() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date");
        let spans: core::MemberSpans = [
            ("GONE", vec![(d("1996-01-02"), Some(d("2001-02-01"))), (d("2003-01-01"), Some(d("2010-07-30")))]),
            ("EARLY", vec![(d("1996-01-02"), Some(d("2010-07-31")))]),
            ("OPEN", vec![(d("1996-01-02"), None)]),
            ("LIVE", vec![(d("1996-01-02"), Some(d("2020-01-01")))]),
        ]
        .into_iter()
        .map(|(t, s)| (t.to_string(), s))
        .collect();
        let csv = "ticker,month,close,div\n# public domain\nGONE,2010-06,12,0.5\nGONE,2010-05,10,0\nEARLY,2010-06,4,0\n\
                   OPEN,2010-06,5,0\nLIVE,2018-03,7,0\nLIVE,2018-02,6,0\nODD,2018-01\n";
        let (charts, refused) = dead_member_charts(csv, &spans);
        assert_eq!(refused, 2, "EARLY stopped trading before it left, OPEN never left");
        let mut kept: Vec<(&str, bool)> = charts.iter().map(|(t, c)| (t.as_str(), c.1)).collect();
        kept.sort();
        assert_eq!(kept, [("GONE", true), ("LIVE", false)]);
        let gone = &charts["GONE"].0;
        assert_eq!(gone.dates, [d("2010-05-01"), d("2010-06-01")], "sorted, dated at the month's first day");
        assert_eq!(gone.closes, [10.0, 12.0]);
        assert_eq!(gone.divs, [(d("2010-06-01"), 0.5)], "only paid dividends become events");
        assert_eq!(dead_member_charts("", &spans).1, 0, "no file, no census");
    }

    /// (#751) Yahoo stays primary: the archive fills a member only when no Yahoo bar past the warm-up is
    /// a point-in-time cutoff, or (#755) when Yahoo's series outlives the archive's dead one by over 92 days.
    #[test]
    fn wants_dead_fill_only_where_yahoo_has_no_member_cutoff() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date");
        let spans = [(d("2000-01-01"), Some(d("2005-01-01")))];
        let bars = [d("1999-01-01"), d("2001-01-01"), d("2006-01-01")];
        assert!(wants_dead_fill(None, 1, &spans, None), "Yahoo served no bars");
        assert!(!wants_dead_fill(Some(&bars[..]), 1, &spans, None), "2001 is a member cutoff past the warm-up");
        assert!(wants_dead_fill(Some(&bars[..]), 2, &spans, None), "inside the warm-up it is not");
        // a reused ticker: Yahoo's last bar (2006-01-01) is the next holder's once the archive's series died
        assert!(wants_dead_fill(Some(&bars[..]), 1, &spans, Some(d("2005-06-01"))), "214 days past the death");
        assert!(!wants_dead_fill(Some(&bars[..]), 1, &spans, Some(d("2005-10-01"))), "92 days: still the member");
    }

    /// (#751) A full window is the walk's old arithmetic, terminal or not; without one, only a dead series
    /// (a bench passed) pays its last close rolled into the index, cut when it lost half in its last year.
    #[test]
    fn forward_return_pays_a_dead_series_terminal() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date");
        let dates = [d("2000-01-01"), d("2001-01-01"), d("2002-01-01"), d("2003-01-01")];
        let bench = ([d("2002-01-01"), d("2003-01-01"), d("2006-01-01")], [100.0, 110.0, 132.0]);
        let dead = Some((&bench.0[..], &bench.1[..], DEAD_PENALTY));
        let flat = [10.0, 20.0, 18.0, 9.0]; // last close is exactly half the year-ago one: no cut
        assert_eq!(forward_return(&dates, &flat, 0, 1, dead), Some(((20.0f64 / 10.0 - 1.0) * 100.0, false)));
        assert_eq!(forward_return(&dates, &flat, 1, 1, None), Some(((18.0f64 / 20.0 - 1.0) * 100.0, false)));
        assert_eq!(forward_return(&dates, &flat, 1, 5, None), None, "a live or censored series stops");
        assert_eq!(forward_return(&dates, &flat, 1, 5, dead), Some(((9.0f64 / 20.0 * (132.0 / 110.0) - 1.0) * 100.0, true)));
        let crashed = [10.0, 20.0, 30.0, 9.0];
        let cut = (9.0f64 / 20.0 * DEAD_PENALTY * (132.0 / 110.0) - 1.0) * 100.0;
        assert_eq!(forward_return(&dates, &crashed, 1, 5, dead), Some((cut, true)));
        let uncut = Some((&bench.0[..], &bench.1[..], 1.0)); // (#752) `deadflat`
        let whole = (9.0f64 / 20.0 * (132.0 / 110.0) - 1.0) * 100.0;
        assert_eq!(forward_return(&dates, &crashed, 1, 5, uncut), Some((whole, true)));
        let short = Some((&bench.0[..2], &bench.1[..2], DEAD_PENALTY));
        assert_eq!(forward_return(&dates, &crashed, 1, 5, short), None, "the index has not reached the window's end");
    }

    /// (#9) gate_audit reports a POSITIVE accepted−rejected gap when the gate KEEPS the high-return names
    /// and drops the low ones (gates select winners), and a NEGATIVE gap when it admits the losers (the
    /// "loosen me" signal). Synthetic gate admits dd>0; `s_rel` sets relative == the value passed.
    fn dd_gate(q: &Quote, _: &BuyHeuristic) -> Option<f64> {
        (q.drawdown_pct > 0.0).then_some(1.0)
    }
    #[test]
    fn gate_audit_flags_good_and_bad_gates() {
        let def = BuyHeuristic::default();
        // winners (relative +) pass the gate; losers (relative −) fail -> accepted mean ≫ rejected -> +gap
        let good: Vec<Sample> = [(5.0, 1.0), (6.0, 1.0), (7.0, 1.0), (8.0, 1.0), (-5.0, -1.0), (-6.0, -1.0), (-7.0, -1.0), (-8.0, -1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (gap, gap_med, _) = gate_audit(&good, dd_gate, &def).unwrap();
        assert!(gap > 0.0 && gap_med > 0.0, "gate keeps winners -> both stats positive");
        // flip: the dd>0 (accepted) names now carry the LOW returns -> gate admits losers -> negative gap
        let bad: Vec<Sample> = [(-5.0, 1.0), (-6.0, 1.0), (-7.0, 1.0), (-8.0, 1.0), (5.0, -1.0), (6.0, -1.0), (7.0, -1.0), (8.0, -1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (gap, gap_med, _) = gate_audit(&bad, dd_gate, &def).unwrap();
        assert!(gap < 0.0 && gap_med < 0.0, "gate admits losers -> both stats negative");
        // <4 on one side (4 accepted / 1 rejected) -> None (the too-few guard)
        assert!(gate_audit(&good[..5], dd_gate, &def).is_none());

        // THE CASE THE BARE MEAN GOT WRONG, and the reason this audit grew a median. Accepted holds
        // four typical LOSERS and one 20-bagger; rejected is four flat names. Every accepted name but
        // one underperformed, yet the outlier drags the accepted MEAN above the rejected mean, so a
        // mean-only audit prints "gates SELECT winners". The median sees four losers and disagrees.
        let skewed: Vec<Sample> = [(-5.0, 1.0), (-6.0, 1.0), (-7.0, 1.0), (-8.0, 1.0), (2000.0, 1.0),
                                   (0.0, -1.0), (1.0, -1.0), (-1.0, -1.0), (0.0, -1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (gap, gap_med, amed) = gate_audit(&skewed, dd_gate, &def).unwrap();
        assert!(gap > 0.0, "one 20-bagger carries the accepted MEAN above the rejected pool");
        assert!(gap_med < 0.0, "the typical accepted name still lost — the median must not follow the tail");
        // and the verdict must refuse to pick a side rather than quietly reporting the mean's answer
        assert!(gap_verdict(gap, gap_med, "yes", "no").starts_with("SPLIT"), "disagreement must print SPLIT");
        // (#151) THE THIRD RETURN IS THE ACCEPTED COHORT'S OWN MEDIAN, and this fixture is exactly why
        // the GATE SWEEP needs it: the shipped gates here accept a cohort whose median is -6.0, so a
        // looser gate admitting names at median -5.0 would be admitting names BETTER than the book —
        // while an absolute "median > 0" bar calls both of them junk and refuses the widening. The
        // sweep's `vs shipped` column is `newly-admitted median - this number`.
        assert_eq!(amed, -6.0, "the accepted cohort's median must be carried out, not the gap");
        assert!(amed < 0.0 && gap > 0.0, "the shipped cohort can be below the absolute bar while still out-selecting the rejects — the case the bar cannot see");
    }

    /// `gap_verdict` fires a directional verdict ONLY on agreement; either mismatch is a SPLIT. Exact
    /// strings pass through untouched so the two call sites keep their own (opposite-sign) wording.
    #[test]
    fn gap_verdict_needs_both_stats_to_agree() {
        assert_eq!(gap_verdict(1.0, 2.0, "yes", "no"), "yes");
        assert_eq!(gap_verdict(-1.0, -2.0, "yes", "no"), "no");
        assert!(gap_verdict(5.0, -1.0, "yes", "no").starts_with("SPLIT"), "mean + / median − -> SPLIT");
        assert!(gap_verdict(-5.0, 1.0, "yes", "no").starts_with("SPLIT"), "mean − / median + -> SPLIT");
        // exactly 0 counts as non-positive on both sides, so a dead-flat gap reads as the negative verdict
        assert_eq!(gap_verdict(0.0, 0.0, "yes", "no"), "no");
    }

    /// (#890) The gate-sweep flag by quadrant of (mean, median): both positive = TOO TIGHT, both non-positive =
    /// no flag, a sign split = SPLIT, and exactly 0 counts as non-positive on both sides.
    #[test]
    fn sweep_tag_quadrants() {
        assert_eq!(sweep_tag(1.0, 1.0), "  <- TOO TIGHT (loosen this gate)");
        assert_eq!(sweep_tag(-1.0, -1.0), "");
        assert_eq!(sweep_tag(1.0, -1.0), "  <- SPLIT (tail-driven, read the median)");
        assert_eq!(sweep_tag(-1.0, 1.0), "  <- SPLIT (tail-driven, read the median)");
        assert_eq!(sweep_tag(0.0, 0.0), "");
    }

    /// `cohort_stats` returns (mean, median) and the two part ways on skew — the whole premise of the
    /// change. Median of an even count averages the middle pair.
    #[test]
    fn cohort_stats_splits_mean_from_median() {
        let (mean, med) = cohort_stats(&[-1.0, -1.0, -1.0, 400.0]);
        assert!((mean - 99.25).abs() < 1e-9, "mean follows the outlier, got {mean}");
        assert!((med + 1.0).abs() < 1e-9, "median ignores it, got {med}");
        let (m2, med2) = cohort_stats(&[1.0, 3.0]);
        assert!((m2 - 2.0).abs() < 1e-9 && (med2 - 2.0).abs() < 1e-9, "even count -> midpoint of the pair");
    }

    /// (#10) `newly_admitted_stats` must isolate exactly the names a LOOSER gate newly admits (rejected
    /// under `base`, accepted under `relaxed`) and average their forward return — the signal that a gate
    /// is too tight. Synthetic gate: admit dd > growth_min_cagr, so lowering that threshold admits more.
    fn cagr_gate(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
        (q.drawdown_pct > t.growth_min_cagr).then_some(1.0)
    }
    #[test]
    fn newly_admitted_mean_isolates_the_loosened_set() {
        let base = BuyHeuristic { growth_min_cagr: 5.0, ..Default::default() }; // base admits dd>5
        let relaxed = BuyHeuristic { growth_min_cagr: 0.0, ..base.clone() }; // loosened: admits dd>0
        // dd 1/2/3 are NEWLY admitted (rejected at >5, accepted at >0); dd6 was already in under base
        // (not "newly"); dd-1 stays rejected. relative == the first tuple field (s_rel).
        let s: Vec<Sample> = [(10.0, 1.0), (20.0, 2.0), (30.0, 3.0), (99.0, 6.0), (-5.0, -1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (n, mean, med) = newly_admitted_stats(&s, cagr_gate, &base, &relaxed).unwrap();
        assert_eq!(n, 3, "only dd 1/2/3 are newly admitted (dd6 was already in)");
        assert!((mean - 20.0).abs() < 1e-9, "their relatives 10/20/30 -> mean 20, got {mean}");
        assert!((med - 20.0).abs() < 1e-9, "odd count -> the middle value, got {med}");
        // loosening that admits nobody (relaxed == base) -> None
        assert!(newly_admitted_stats(&s, cagr_gate, &base, &base).is_none());

        // Why the sweep's TOO TIGHT flag now needs BOTH stats: these newly-admitted sets run tiny
        // (n=4..50 on real data), so one survivor is enough to push a mean positive while three of the
        // four names it admits actually lost. Mean-only, that gate reads "loosen me".
        let tail: Vec<Sample> = [(-9.0, 1.0), (-8.0, 2.0), (-7.0, 3.0), (900.0, 4.0), (99.0, 6.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (n, mean, med) = newly_admitted_stats(&tail, cagr_gate, &base, &relaxed).unwrap();
        assert_eq!(n, 4);
        assert!(mean > 0.0 && med < 0.0, "mean {mean} / median {med} must disagree here");
    }

    /// `tune` de-means each chronological split INDEPENDENTLY (`demean(&mut s[..cut])` / `s[cut..]`).
    /// The peer-relative invariant must then hold WITHIN each half, not just globally — else the train
    /// and test rho would race the other half's regime mean.
    #[test]
    fn split_demean_keeps_invariant_per_half() {
        // 4 early cutoffs (2019) + 4 late (2022), each half spanning its own buckets with non-zero means
        let mut s = [
            sample(ymd(2019, 2, 1), 10.0),
            sample(ymd(2019, 3, 1), 30.0),
            sample(ymd(2019, 9, 1), 50.0),
            sample(ymd(2019, 10, 1), 70.0),
            sample(ymd(2022, 2, 1), -5.0),
            sample(ymd(2022, 3, 1), 15.0),
            sample(ymd(2022, 9, 1), 200.0),
            sample(ymd(2022, 10, 1), 100.0),
        ];
        let cut = s.len() * 7 / 10; // 5
        demean(&mut s[..cut]);
        demean(&mut s[cut..]);
        let (train, test) = s.split_at(cut);
        // each half's relatives net to ~0 (every bucket within it de-meaned to its own peers)
        assert!(train.iter().map(|x| x.relative).sum::<f64>().abs() < 1e-9);
        assert!(test.iter().map(|x| x.relative).sum::<f64>().abs() < 1e-9);
    }

    /// (Phase B) `after_tax_pair` against hand-computed constants: M=4.0 over 12y at t=0.28.
    /// Never-sell: net multiple 1+3·0.72=3.16 -> 3.16^(1/12)−1 ≈ +10.06%/yr. Yearly rotation:
    /// gross 4^(1/12)−1 ≈ 12.25%/yr, ×0.72 ≈ +8.82%/yr. Deferral edge ≈ +1.25 pts/yr — the
    /// never-sell lever the footer prints. years=0 must clamp to 1 (no div-by-zero / powf(inf)).
    #[test]
    fn after_tax_pair_hand_computed() {
        let (never, rot) = after_tax_pair(4.0, 12, 0.28, 0.28);
        assert!((never - 10.063).abs() < 0.01, "never-sell got {never}");
        assert!((rot - 8.817).abs() < 0.01, "rotation got {rot}");
        assert!(never > rot); // deferral always wins on a gain
        let (n2, r2) = after_tax_pair(4.0, 0, 0.28, 0.28); // years clamps to 1
        assert!((n2 - 216.0).abs() < 1e-9); // 1+3·0.72=3.16 -> +216%/yr over 1y
        assert!((r2 - 216.0).abs() < 1e-9); // 4^1−1=300% gross ×0.72 = +216% — same over 1y
    }

    /// (#111) The own-pool base rate. This is a counting exercise, so the test counts by hand — and the
    /// thing worth pinning is not the arithmetic but the SHAPE of the answer:
    ///
    ///   * the conditional rate is over the qualifying cohort only, and the unconditional rate is over
    ///     everyone. Printing the first without the second is the failure this exists to avoid, so the
    ///     fn returns both or nothing.
    ///   * the two are genuinely independent — a pool can be built where clearing the bar historically
    ///     predicts clearing it forward WORSE than chance, and the fn must be able to say so.
    ///   * nothing qualifying = None, not a 0% that reads like a finding about a cohort that is empty.
    #[test]
    fn a_conditional_rate_without_its_denominator_is_a_rhetorical_device() {
        let legs = |pairs: &[(&str, f64)]| -> Vec<Option<(String, f64)>> {
            crate::core::HORIZONS
                .iter()
                .map(|(l, _)| pairs.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
                .collect()
        };
        // a 5Y leg of +200% is 24.6%/yr trailing; +50% is 8.4%/yr
        let mk = |trailing_5y: f64, realized: f64| {
            let mut q = Quote::stub("X", "1", "", "X");
            q.perf = legs(&[("5Y", trailing_5y)]);
            Sample { date: ymd(2020, 1, 1), realized, relative: 0.0, quote: Arc::new(q), fund: None, trail: Vec::new() }
        };
        let t = BuyHeuristic { growth_min_cagr: 20.0, ..BuyHeuristic::default() };
        // forward over 10y: +900% is 25.9%/yr (clears 20), +100% is 7.2%/yr (does not)
        let (win, lose) = (900.0, 100.0);

        // 4 qualifiers, 1 of them sustains; 4 laggards, 3 of them sustain — the record ANTI-predicts,
        // and the fn has to be able to report that rather than only ever flattering the gate.
        let pool: Vec<Sample> = [
            mk(200.0, win), mk(200.0, lose), mk(200.0, lose), mk(200.0, lose),
            mk(50.0, win), mk(50.0, win), mk(50.0, win), mk(50.0, lose),
        ]
        .into_iter()
        .collect();
        let (q, kept, everyone) = persistence_base_rate(&pool, &t, 10, t.growth_min_cagr).unwrap();
        assert_eq!((q, kept), (4, 1), "counted over the qualifying cohort only");
        assert!((everyone - 50.0).abs() < 1e-9, "4 of 8 in the WHOLE pool sustained: {everyone}");
        assert!(
            (kept as f64 / q as f64 * 100.0) < everyone,
            "this pool is built so the record anti-predicts, and the pair must be able to say so"
        );

        // nothing clears the bar -> nothing to condition on, and that is not a 0% about a cohort
        let none = BuyHeuristic { growth_min_cagr: 99.0, ..BuyHeuristic::default() };
        assert_eq!(persistence_base_rate(&pool, &none, 10, none.growth_min_cagr), None);
        assert_eq!(persistence_base_rate(&[], &t, 10, t.growth_min_cagr), None, "an empty pool says nothing");
        assert_eq!(persistence_base_rate(&pool, &t, 10, 0.0), None, "(#340) a bar of 0 is off, whatever the pool holds");
    }

    /// (#108) The holding-period schedule. Three things have to hold, and the first is the golden rule:
    ///
    ///   * an EMPTY schedule is the identity — every horizon pays the headline rate, so the after-tax
    ///     footer is byte-identical to the flat-rate one it replaced. This is the whole default.
    ///   * the WIDEST satisfied rung wins regardless of yaml order, and a hold short of every rung
    ///     pays full freight. A schedule is a ladder, not a sequence of overrides.
    ///   * the schedule can only ever WIDEN the deferral edge, never narrow it: the rotation arm holds
    ///     for a year by definition, so it is pinned to the bottom rung while the never-sell arm
    ///     climbs. That asymmetry is the entire finding — a flat rate understates the one number in
    ///     the report that most supports the tool's own buy-and-hold thesis.
    #[test]
    fn a_holding_period_schedule_can_only_widen_the_deferral_edge() {
        use crate::config::CgtRung;
        let rung = |min_years: f64, excluded_pct: f64| CgtRung { min_years, excluded_pct };
        // deliberately NOT in ascending order, and it must not matter
        let sched = [rung(8.0, 30.0), rung(2.0, 10.0), rung(5.0, 20.0)];

        assert_eq!(cgt_rate(20, 0.28, &[]), 0.28, "empty schedule = today's flat rate, at any horizon");
        assert_eq!(cgt_rate(1, 0.28, &[]), 0.28);

        assert_eq!(cgt_rate(1, 0.28, &sched), 0.28, "below every rung -> full freight");
        assert!((cgt_rate(2, 0.28, &sched) - 0.252).abs() < 1e-12, "the rung bites AT its threshold");
        assert!((cgt_rate(4, 0.28, &sched) - 0.252).abs() < 1e-12);
        assert!((cgt_rate(5, 0.28, &sched) - 0.224).abs() < 1e-12);
        assert!((cgt_rate(20, 0.28, &sched) - 0.196).abs() < 1e-12, "past the top rung it stays there");

        // and the asymmetry: same pre-tax path, same window, the schedule only moves the held arm
        let (m, years) = (4.0, 12);
        let flat = after_tax_pair(m, years, 0.28, 0.28);
        let laddered =
            after_tax_pair(m, years, cgt_rate(years, 0.28, &sched), cgt_rate(1, 0.28, &sched));
        assert!((laddered.1 - flat.1).abs() < 1e-12, "rotation never reaches a long-hold rung");
        assert!(laddered.0 > flat.0, "the held arm does");
        assert!(laddered.0 - laddered.1 > flat.0 - flat.1, "so the deferral edge can only widen");
    }

    /// (round 67) END-TO-END walk-forward pin. Every helper above is tested piecewise, but this is
    /// the only test that runs the actual pipeline composition — synthetic price series →
    /// `core::backtest_quote` at run()'s cutoff cadence → `demean` → `growth_score` gates →
    /// `lane_metrics`/`winsor_edge`/`edge_terciles` — to EXACT numbers. Every tuning decision trusts
    /// this pipeline's edge; if this test reds you changed edge composition (a quote-reconstruction
    /// fn, the de-mean, a gate, the metric math) — revert unless that was the explicit, validated
    /// goal, and re-validate the shipped tuning if it was. Deterministic: fixed dates, closed-form
    /// series, BuyHeuristic::default(), no network, no RNG.
    #[test]
    fn walk_forward_edge_pin() {
        // ~13y of "daily" bars on a synthetic trading calendar (calendar-days spread like ~252/yr;
        // strictly increasing since the step 365/252 > 1 day).
        let n_bars = 13 * 252;
        let d0 = ymd(2010, 1, 4);
        let dates: Vec<NaiveDate> =
            (0..n_bars).map(|k| d0 + chrono::Duration::days(k as i64 * 365 / 252)).collect();
        // closed-form series: CAGR `g` plus a deterministic sine wobble (amplitude `amp`, phase `ph`)
        // so ranks aren't degenerate and vol/MA/R² differ per name.
        let series = |g: f64, amp: f64, ph: f64| -> Vec<f64> {
            (0..n_bars)
                .map(|k| {
                    let t = k as f64 / 252.0;
                    100.0 * (1.0 + g).powf(t) * (1.0 + amp * (t * 2.7 + ph).sin())
                })
                .collect()
        };
        // a spread of compounders, laggards and choppy names: winners near-high with high R²
        // (pass the growth gates), losers/flats shape the peer-means and get gated out.
        let universe: [(&str, Vec<f64>); 8] = [
            ("WIN1", series(0.22, 0.04, 0.0)),
            ("WIN2", series(0.17, 0.06, 1.0)),
            ("MID1", series(0.10, 0.08, 2.0)),
            ("MID2", series(0.07, 0.10, 3.0)),
            ("FLAT", series(0.01, 0.12, 4.0)),
            ("LOSE", series(-0.08, 0.10, 5.0)),
            ("WOB1", series(0.13, 0.20, 0.5)),
            ("WOB2", series(0.04, 0.25, 1.5)),
        ];
        // run()'s cutoff walk before (#790): MIN_HISTORY warmup, STEP_SESSIONS stride, 5y forward
        // window, stop when no full window remains. The pinned numbers were measured on that walk,
        // so the replica keeps it; the pin grades the scorer, not the cutoff grid.
        let years = 5;
        let mut samples: Vec<Sample> = Vec::new();
        for (tk, closes) in &universe {
            let mut i = MIN_HISTORY;
            while i < dates.len() {
                let target = dates[i] + chrono::Duration::days(years * 365);
                let Some(off) = dates[i..].iter().position(|d| *d >= target) else { break };
                let realized = (closes[i + off] / closes[i] - 1.0) * 100.0;
                let quote = core::backtest_quote(tk, &dates, closes, &[], i, 252, &BTreeMap::new());
                samples.push(Sample { date: dates[i], realized, relative: 0.0, quote: Arc::new(quote), fund: None, trail: Vec::new() });
                i += STEP_SESSIONS;
            }
        }
        demean(&mut samples);
        let tuning = BuyHeuristic::default();
        let (rho, edge) = lane_metrics(&samples, growth_score, &tuning);
        let scored: Vec<(&Sample, f64)> =
            samples.iter().filter_map(|s| growth_score(&s.quote, &tuning).map(|v| (s, v))).collect();
        let (top, mid, bot) = edge_terciles(&scored);
        let got = format!(
            "n={} scored={} rho={:.3} edge={:+.1} winsor={:+.1} terciles={:+.1}/{:+.1}/{:+.1}",
            samples.len(), scored.len(), rho.unwrap_or(f64::NAN), edge, winsor_edge(&scored),
            top, mid, bot,
        );
        // golden values captured from the run that introduced this test. The SIGNS are meaningless
        // here (synthetic series, tiny universe — this is not a quality claim about the heuristic);
        // only their exact stability matters. Trip-verified: widening winsor_edge's clamp
        // percentiles to 5/95 reddened this pin.
        // rho moved -0.052 -> -0.057 on 2026-07-25 when `long_leg`'s middle rung went 10Y -> 8Y. This
        // is the one pin that genuinely had to move: it scores `backtest_quote`s built from contiguous
        // closes, so unlike the picks.rs fixtures it really does carry an 8Y leg and really is measured
        // on it now. Every other field is unchanged.
        // (H-cov) moved AGAIN on 2026-07-26, same reason and the same direction: scored 30 -> 26.
        // `horizon_changes` now blanks a leg the series does not reach, and `backtest_quote` slices to
        // [..=as_of], so the EARLY cutoffs hold only a year or two of bars — they were being scored on
        // legs `asof_avg` fabricated from the slice's own first days. Four such samples stop scoring.
        // The guard can only ever turn a Some into a None, so `scored` can only fall; a RISE here would
        // mean something other than this change moved. Signs remain meaningless (synthetic, tiny
        // universe); the drop is the fix removing fabricated inputs, not a quality claim.
        // (#3j) moved a THIRD time on 2026-07-26: scored 26 -> 24. `backtest_quote` now fills `life_cagr`,
        // so the `(#3i)` whole-life half of `growth_min_cagr` finally fires here — (#3i) had shipped
        // claiming it never could, on the false premise that a `[..=as_of]` slice has no whole-life
        // history (it starts at the series' FIRST bar; the field was simply never filled). Two samples
        // clear their 20/8/5Y rung and fail the same floor since listing, exactly the shape the bar
        // exists to catch. `scored` falling by 2 is the check that this and only this moved: the bar can
        // only turn a Some into a None, so a RISE would mean something else did. edge -28.4 -> -26.2
        // follows from dropping those two rows, not from admitting anything new — and on 88 synthetic
        // series it carries no more meaning than the sign does. `use_life_cagr` is OFF here (default), so
        // this pin still measures the LEG-ranked lane; the knob is priced in the (#3j) universe runs.
        assert_eq!(got, "n=88 scored=24 rho=-0.029 edge=-26.2 winsor=-26.2 terciles=+31.2/+68.8/+54.2");
    }

    // ---------------------------------------------------------------------------------------------
    // ASSET-CLASS COVERAGE. `walk_forward_edge_pin` above is deliberately left byte-for-byte alone —
    // its fixtures are stock-like, so its unchanged numbers are the check that the stock path did not
    // move. The tests below cover the two classes it cannot see.
    // ---------------------------------------------------------------------------------------------

    /// Same synthetic trading calendar the pin builds inline (calendar days spread like ~252/yr,
    /// strictly increasing since 365/252 > 1 day). Shared here rather than reaching into the pin, so
    /// the pin stays untouched.
    fn synth_calendar(n_bars: usize) -> Vec<NaiveDate> {
        let d0 = ymd(2010, 1, 4);
        (0..n_bars).map(|k| d0 + chrono::Duration::days(k as i64 * 365 / 252)).collect()
    }
    /// Closed-form price series: CAGR `g` plus a deterministic sine wobble, so ranks aren't degenerate
    /// and vol/MA/R² differ per name. No RNG — these tests pin exact numbers.
    fn synth_series(n_bars: usize, g: f64, amp: f64, ph: f64) -> Vec<f64> {
        (0..n_bars)
            .map(|k| {
                let t = k as f64 / 252.0;
                100.0 * (1.0 + g).powf(t) * (1.0 + amp * (t * 2.7 + ph).sin())
            })
            .collect()
    }

    /// Every growth knob, paired with a probe value cranked past anything a fixture can reach: floors go
    /// absurdly HIGH, caps absurdly LOW (but never to the knob's own "0 = off" sentinel), multipliers to
    /// a value that is neither of their two off-states. Cranking to an impossible value is what makes an
    /// INERT verdict mean "unreachable" rather than "my fixture happened to sit on the safe side".
    const GATE_PROBES: &[(&str, fn(&mut BuyHeuristic, f64), f64)] = &[
        // equity/shared hard gates
        ("growth_min_cagr", |t, v| t.growth_min_cagr = v, 1e9),
        ("growth_min_range_pct", |t, v| t.growth_min_range_pct = v, 1e9),
        ("growth_min_1y_pct", |t, v| t.growth_min_1y_pct = v, 1e9),
        ("growth_min_5y_pct", |t, v| t.growth_min_5y_pct = v, 1e9),
        ("growth_min_8y_pct", |t, v| t.growth_min_8y_pct = v, 1e9),
        ("growth_min_20y_pct", |t, v| t.growth_min_20y_pct = v, 1e9),
        ("growth_maxdd_cap", |t, v| t.growth_maxdd_cap = v, 1e-6),
        ("growth_max_above_ma", |t, v| t.growth_max_above_ma = v, 1e-6),
        ("max_1m_drop_pct", |t, v| t.max_1m_drop_pct = v, 1e9),
        ("growth_max_peg", |t, v| t.growth_max_peg = v, 1e-6),
        // (P4) both are ceilings, so both probe LOW — a cap of ~0 rejects any name carrying the field at
        // all. Unlike the (P1) four below, `backtest_quote` DOES fill what these read (volatility_pct and
        // max_daily_1m, both off the same close series), so they pin LIVE on this price-only path for
        // every non-crypto class. Crypto is INERT by scope, not by missing data — cause (b).
        ("growth_max_vol", |t, v| t.growth_max_vol = v, 1e-6),
        ("growth_max_daily_1m", |t, v| t.growth_max_daily_1m = v, 1e-6),
        ("growth_require_lifetime_uptrend", |t, v| t.growth_require_lifetime_uptrend = v != 0.0, 1.0),
        // a floor, so it probes HIGH: no listing carries a 1e9-year leg, so every rung is skipped,
        // `long_leg` returns None and nothing is scorable. LIVE everywhere is the correct verdict.
        ("growth_min_leg_years", |t, v| t.growth_min_leg_years = v, 1e9),
        // crypto twins
        ("growth_min_cagr_crypto", |t, v| t.growth_min_cagr_crypto = v, 1e9),
        ("growth_min_5y_pct_crypto", |t, v| t.growth_min_5y_pct_crypto = v, 1e9),
        ("growth_min_range_pct_crypto", |t, v| t.growth_min_range_pct_crypto = v, 1e9),
        ("min_1y_pct_crypto", |t, v| t.min_1y_pct_crypto = v, 1e9),
        ("max_1m_drop_pct_crypto", |t, v| t.max_1m_drop_pct_crypto = v, 1e9),
        ("growth_maxdd_cap_crypto", |t, v| t.growth_maxdd_cap_crypto = v, 1e-6),
        ("growth_max_vol_crypto", |t, v| t.growth_max_vol_crypto = v, 1e-6),
        // (#45) probe = a ceiling of ~0, which rejects any coin carrying ANY MVRV. It still reads INERT,
        // and that is cause (a): `backtest_quote` never assigns `quote.mvrv`. Pinned so the "unsweepable"
        // claim in the crypto_max_mvrv receipt is a test result rather than an assertion in a comment.
        ("crypto_max_mvrv", |t, v| t.crypto_max_mvrv = v, 1e-6),
        // ETF-scoped
        // (#192) a FLOOR, so it probes HIGH — and 1e9 is safely off its own `0.0 = INHERIT` sentinel,
        // which is the one value that would probe as a RELAXATION rather than a tightening. Expected
        // LIVE for etf and INERT for the other two BY SCOPE (cause b), which is the whole claim the
        // knob makes; pinned here so "ETF-only" is a test result and not a comment.
        ("growth_min_cagr_etf", |t, v| t.growth_min_cagr_etf = v, 1e9),
        // (#1230) the fund 5Y twin: a floor with the same `0.0 = INHERIT` sentinel, so the same probe and the
        // same claim, LIVE for etf and INERT by scope for the other two.
        ("growth_min_5y_pct_etf", |t, v| t.growth_min_5y_pct_etf = v, 1e9),
        ("sharpe_cap_etf", |t, v| t.sharpe_cap_etf = v, 1e-6),
        ("growth_min_aum_etf", |t, v| t.growth_min_aum_etf = v, 1e9),
        ("growth_ter_drag", |t, v| t.growth_ter_drag = v != 0.0, 1.0),
        // score multipliers and tilts
        ("growth_commodity_damp", |t, v| t.growth_commodity_damp = v, 0.5),
        ("growth_fx_damp", |t, v| t.growth_fx_damp = v, 0.5),
        ("growth_turnover_weight", |t, v| t.growth_turnover_weight = v, 1.0),
        // known-unreachable by construction — pinned so the claim stops being folklore
        ("growth_min_age_years", |t, v| t.growth_min_age_years = v, 1e9),
        ("growth_min_range_pct_8y", |t, v| t.growth_min_range_pct_8y = v, 1e9),
    ];

    /// WHICH GROWTH KNOBS CAN MOVE A BACKTEST NUMBER AT ALL, per asset class.
    ///
    /// `config.rs` carries ~10 hand-written claims of the form "BACKTEST-BLIND by construction" /
    /// "edge-blind" / "LIVE-ONLY BY CONSTRUCTION". Nothing has ever verified one, and they are wrong
    /// often enough to matter: `sharpe_cap_etf` said "the backtest pool holds stock constituents only"
    /// (the pool is ~4311 ETFs of 4954), and `growth_commodity_damp` said the pool "carries no sector
    /// and no real fund names" — true until the class stamping landed, false the moment it did. Both
    /// were load-bearing: knobs get set by judgement precisely BECAUSE they are believed unsweepable.
    ///
    /// The whole question reduces to one fact nobody wrote down: `core::backtest_quote` fills 23 fields
    /// (perf, drawdown_pct, range_pct, volatility_pct, downside_dev_pct, below/above_ma_pct, trend_r2,
    /// trend_cagr, life_cagr, capped_cagr, tr_cagr, max_drawdown_pct, max_daily_1m, roll5y/10y_pos_pct,
    /// worst_5y/10y_pct, underwater_yrs on the daily path, age_years, price_eur, div_eur, and a sentinel
    /// avg_turnover_eur) and leaves the rest at
    /// `Quote::stub` defaults, so any gate reading an unfilled field can never fire. This pin turns
    /// that into an assertion, in BOTH directions — a live gate going silently dead is the ETF bug's
    /// exact shape, and a false "inert" claim is how the receipts rotted.
    ///
    /// SCOPE: the PRICE-ONLY reconstruction. `backtest ... fund` additionally attaches `quote.fund`
    /// (the `quote.fund = fund.clone()` in the walk), so the fundamentals knobs are live on THAT path — `growth_max_peg` reads
    /// INERT below and config.rs's "MEASURABLE (peg_yield is filled in the backtest)" is still right;
    /// the two statements are about different runs, not in conflict.
    ///
    /// Reads as "the score MOVED", not "the ranking moved". `growth_turnover_weight` is the case where
    /// those differ: `backtest_quote` sets a uniform sentinel turnover, so the knob shifts every name by
    /// the same constant — reachable, but rank-neutral and so still unsweepable for edge. LIVE here
    /// means "can change a number", never "is safe to tune on".
    ///
    /// EVERY INERT VERDICT, WITH ITS CAUSE — hand-checked against the fill list above, because an INERT
    /// entry is about to be cited as evidence that a knob cannot be swept, and three distinct causes
    /// hide behind the one word:
    ///   (a) FIELD NEVER FILLED — dead on this path, no fixture can rescue it. `growth_min_aum_etf`
    ///       (aum_eur), `growth_ter_drag` (expense_ratio), `growth_fx_damp` (quote_currency),
    ///       `growth_min_range_pct_8y` (stats_8y). None of those four is assigned by
    ///       `core::backtest_quote` (`growth_min_age_years` left this list when (#182) filled age_years); `growth_max_peg` (quote.fund) joins them on the
    ///       price-only path only, per SCOPE above.
    ///   (b) SCOPED TO ANOTHER CLASS — correct by construction, and the thing round 1's bug broke: the
    ///       `if crypto {…} else {…}` selectors (maxdd cap, 1M knife, 1Y/range/CAGR floors, vol cap),
    ///       the `!crypto` guards (above-MA ceiling, lifetime uptrend, the (P4) vol and spike ceilings),
    ///       `sharpe_cap_etf` off a fund,
    ///       and the commodity damp on a coin (no GICS sector, not a fund name).
    ///   (c) REACHABLE BUT DOMINATED — `growth_require_lifetime_uptrend`. Its fields ARE filled, so it
    ///       is not dead; it cannot bite because `growth_min_cagr` rejects the same names first. A
    ///       fixture built to trip it (crash −95%, recover to just under the start price, life_cagr
    ///       −0.2%/yr with a +1349% 20Y leg) fails the floor at `cagr-life -0.2%/yr (need ≥8.0%)` and
    ///       never reaches the gate. Lower `growth_min_cagr` below 0 and this knob wakes up.
    #[test]
    fn growth_gate_reachability_pin() {
        // 25y, not the ~13y the other tests use: `growth_min_20y_pct` reads a 20Y leg, and `perf_pct`
        // returns None for a leg the history is too short to carry. On a 13y fixture that knob reads
        // INERT — a fixture artifact indistinguishable, in the golden string, from a structurally dead
        // gate. The whole point of this pin is that the two never get confused, so the fixture has to
        // be old enough to carry every leg the gates ask for.
        let n = 25 * 252;
        let dates = synth_calendar(n);
        // amp 0.25 / phase 1.0 is chosen, not arbitrary. The wobble has to be deep enough that the
        // series actually DIPS — at the amp the other tests use, the 22%/yr drift dominates the sine
        // everywhere, `max_drawdown_pct` is 0.00, and every drawdown-reading cap reads INERT because
        // nothing can be below a cap of zero. The phase then places the endpoint on a rising leg, so
        // the fixture still clears the 1Y floor and scores. Measured here: maxdd 26.0, above-MA 26.5,
        // vol 0.19, 20Y leg +4296% — each of those is an input some gate below compares against.
        let closes = synth_series(n, 0.22, 0.25, 1.0);
        let etf_set: HashSet<String> = HashSet::new();
        // an Energy sector on the stock: `is_commodity` reads `quote.sector`, which only exists in the
        // backtest since the class stamping. Without it the commodity damp reads inert for want of a
        // sector rather than for want of reachability — the exact confusion this pin exists to end.
        let sector_of: HashMap<String, String> =
            [("XOM".to_string(), "Energy".to_string())].into_iter().collect();
        let d = BuyHeuristic::default();

        let fixture = |tk: &str, name: &str, ity: &str| -> Sample {
            let mut quote = core::backtest_quote(tk, &dates, &closes, &[], n - 1, 252, &BTreeMap::new());
            stamp_asset_class(&mut quote, name, ity, &etf_set, &sector_of);
            Sample { date: dates[n - 1], realized: 0.0, relative: 0.0, quote: Arc::new(quote), fund: None, trail: Vec::new() }
        };
        // A PANEL per class, not one fixture: a knob reads LIVE if ANY member moves. `is_commodity`'s
        // fund leg is name-driven, so a single broad-market ETF would report the commodity damp INERT
        // for the whole ETF class — true of that fund, false of the class. The clean-energy name trips
        // the `energy` token WITHOUT tripping `is_commodity_etf` (no "physical"/"commodit"/`etc`/metal
        // token), so it stays rankable and gets damped rather than structurally rejected.
        let panel: [(&str, Vec<Sample>); 3] = [
            ("crypto", vec![fixture("BTC-EUR", "BTC-EUR", "CRYPTOCURRENCY")]),
            (
                "etf   ",
                vec![
                    fixture("XDWD.L", "Xtrackers MSCI World UCITS ETF", "ETF"),
                    fixture("INRG.L", "iShares Global Clean Energy UCITS ETF", "ETF"),
                ],
            ),
            ("stock ", vec![fixture("XOM", "Exxon Mobil Corp", "EQUITY")]),
        ];

        let mut out = String::new();
        for (label, members) in &panel {
            for s in members {
                // PRECONDITION. A fixture that is already gated out scores None under every probe, which
                // reads as "every knob is inert" — a table of confident lies. Assert it scores FIRST.
                assert!(
                    growth_score(&s.quote, &d).is_some(),
                    "{label} fixture {} must score under default tuning, else every INERT verdict below is meaningless",
                    s.quote.ticker
                );
                assert_eq!(picks::asset_class(&s.quote), match label.trim() { "crypto" => 0, "etf" => 1, _ => 2 });
            }
            let (mut live, mut inert) = (Vec::new(), Vec::new());
            for (name, set, probe) in GATE_PROBES {
                let moved = members
                    .iter()
                    .any(|s| dim_active(std::slice::from_ref(s), growth_score, &d, *set, *probe));
                if moved { live.push(*name) } else { inert.push(*name) }
            }
            out += &format!("{label} LIVE  {}\n{label} INERT {}\n", live.join(" "), inert.join(" "));
        }
        // golden inventory. A knob moving between LIVE and INERT is a REAL event — either a gate stopped
        // firing (the ETF bug), or a receipt somewhere now states the opposite of the truth. Re-read the
        // knob's comment in config.rs before re-pinning; the point of this test is to force that read.
        assert_eq!(
            out,
            concat!(
                // (2026-08-03) `growth_min_5y_pct` moved crypto LIVE -> crypto INERT and the new
                // `growth_min_5y_pct_crypto` took its place, for every class. That is the twin doing
                // exactly its job: the equity 5Y floor no longer reaches a coin, so it can sit at its
                // measured optimum (+75) without emptying the crypto table. Its ETF/stock standing is
                // unchanged, which is the other half of the claim.
                "crypto LIVE  growth_min_8y_pct growth_min_20y_pct growth_min_leg_years growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto growth_turnover_weight growth_min_age_years\n",
                // (2026-08-19) the (P4) pair enters LIVE for etf and stock on the FIRST commit that adds
                // them — the opposite of the (P1) four, which pin INERT here because `quote.fund` is
                // unfilled on this path. These read `volatility_pct` and `max_daily_1m`, both built from
                // the same close series `backtest_quote` already walks, so they are sweepable from day one
                // and their receipts must not claim otherwise. Crypto is INERT by SCOPE (cause b): both
                // carry a `!crypto` guard, and the coin lane keeps `growth_max_vol_crypto` instead.
                // (#182) (2026-08-31) `growth_min_age_years` moved INERT -> LIVE in ALL THREE lanes.
                // It was INERT for cause (a) — an unfilled field, not scope: `core::backtest_quote`
                // left `quote.age_years` None, so `(#33)`'s receipt correctly read "BACKTEST-BLIND
                // either way ... it only shapes the LIVE screen". That was never a property of the
                // walk. The `[..=as_of]` slice starts at the first bar of the record, so the age was
                // always there to read, and `backtest_quote` now fills it the way `fetch.rs` does.
                // The gate still SHIPS 0.0, so nothing moves until an arm turns it on — what changed
                // is that an arm can now be graded at all. This lane has no `!crypto` guard, which is
                // why crypto goes LIVE with the other two rather than staying INERT by scope.
                "crypto INERT growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_peg growth_max_vol growth_max_daily_1m growth_require_lifetime_uptrend crypto_max_mvrv growth_min_cagr_etf growth_min_5y_pct_etf sharpe_cap_etf growth_min_aum_etf growth_ter_drag growth_commodity_damp growth_fx_damp growth_min_range_pct_8y\n",
                // (#1230) `growth_min_5y_pct_etf` enters etf LIVE and crypto/stock INERT on its first
                // commit, the same standing `growth_min_cagr_etf` has, which is its whole claim.
                "etf    LIVE  growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_min_8y_pct growth_min_20y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_vol growth_max_daily_1m growth_min_leg_years growth_min_cagr_etf growth_min_5y_pct_etf sharpe_cap_etf growth_commodity_damp growth_turnover_weight growth_min_age_years\n",
                "etf    INERT growth_max_peg growth_require_lifetime_uptrend growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto crypto_max_mvrv growth_min_aum_etf growth_ter_drag growth_fx_damp growth_min_range_pct_8y\n",
                "stock  LIVE  growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_min_8y_pct growth_min_20y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_vol growth_max_daily_1m growth_min_leg_years growth_commodity_damp growth_turnover_weight growth_min_age_years\n",
                "stock  INERT growth_max_peg growth_require_lifetime_uptrend growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto crypto_max_mvrv growth_min_cagr_etf growth_min_5y_pct_etf sharpe_cap_etf growth_min_aum_etf growth_ter_drag growth_fx_damp growth_min_range_pct_8y\n",
            )
        );
    }

    /// The backtest rebuilds quotes from PRICE HISTORY ALONE: `core::backtest_quote` calls
    /// `Quote::stub(ticker, "", "", ticker)`, leaving `name` = the ticker and `instrument_type` empty.
    /// `picks::quote_is_etf` reads exactly those two fields, and no real fund ticker (`VWRA.L`,
    /// `XDWD.L`, `SEMI.AS`) contains an `ETF_MARKERS` substring — so before `stamp_asset_class`, EVERY
    /// fund in the pool classified as a single stock (~4300 of ~4950 live names). `demean` splits on
    /// `picks::asset_class`, so the stock peer-mean every printed edge was measured against was ~87%
    /// ETFs, and every ETF-scoped gate silently no-op'd. This test is the regression: drop the
    /// stamping and the ETF/sector cases go red instead of quietly reverting to the old behaviour.
    #[test]
    fn stamp_asset_class_recovers_all_three_classes() {
        let etf_set: HashSet<String> = ["VWRA.L".to_string()].into_iter().collect();
        let sector_of: HashMap<String, String> =
            [("NESN.SW".to_string(), "Consumer Staples".to_string())].into_iter().collect();
        let stamped = |tk: &str, name: &str, ity: &str| {
            let mut q = Quote::stub(tk, "1", "", tk);
            stamp_asset_class(&mut q, name, ity, &etf_set, &sector_of);
            q
        };

        // THE BUG ITSELF: an unstamped stub is a single stock whatever it really is.
        let raw = Quote::stub("XDWD.L", "1", "", "XDWD.L");
        assert_eq!(picks::asset_class(&raw), 2, "unstamped stub must class stock — the bug this fixes");

        // Yahoo's own meta.instrumentType is the primary route: fund shortNames frequently carry no
        // marker at all ("ISHARES III PLC ISHRS CORE MSCI"), which is why the name fallback missed them.
        assert_eq!(picks::asset_class(&stamped("XDWD.L", "ISHARES III PLC ISHRS CORE MSCI", "ETF")), 1);
        // belt-and-braces: fetch_universe's etf_set catches a fund whose meta is blank on this venue.
        assert_eq!(picks::asset_class(&stamped("VWRA.L", "VANGUARD FUNDS PLC", "")), 1);
        // the old name-substring fallback still fires when both of the above are missing.
        assert_eq!(picks::asset_class(&stamped("X", "iShares Core MSCI World UCITS", "")), 1);
        // crypto never needed the stamp (`is_currency_quoted` reads the TICKER) — it is the one class
        // the stub always got right. Pinned so a future stamping change cannot break it.
        assert_eq!(picks::asset_class(&stamped("BTC-EUR", "BTC-EUR", "CRYPTOCURRENCY")), 0);
        // a real single stock stays a stock AND picks up the GICS sector `is_commodity` needs.
        let nestle = stamped("NESN.SW", "Nestle S.A.", "EQUITY");
        assert_eq!(picks::asset_class(&nestle), 2);
        assert_eq!(nestle.sector.as_deref(), Some("Consumer Staples"));
    }

    /// The ETF-scoped gates were DEAD CODE in the backtest, not merely mis-grouped: with
    /// `instrument_type` empty and `name` = the ticker, `is_commodity_etf` could never
    /// fire, so a physical-gold ETC — a metal peg with no earnings — scored as a proven compounder in
    /// every number the backtest ever printed. Same prices, same reconstruction, stamped vs not.
    #[test]
    fn stamping_wires_up_the_etf_only_gates() {
        let n = 13 * 252;
        let dates = synth_calendar(n);
        let closes = synth_series(n, 0.22, 0.04, 0.0);
        let (etf_set, sector_of) = (HashSet::new(), HashMap::new());
        let tuning = BuyHeuristic::default();

        // the fixture must score BEFORE anything gates it, else a pass below proves nothing.
        let mut etc = core::backtest_quote("SGLN.L", &dates, &closes, &[], n - 1, 252, &BTreeMap::new());
        assert!(growth_score(&etc, &tuning).is_some(), "fixture must score unstamped — the old behaviour");
        stamp_asset_class(&mut etc, "iShares Physical Gold ETC", "ETF", &etf_set, &sector_of);
        assert_eq!(picks::asset_class(&etc), 1);
        assert!(growth_score(&etc, &tuning).is_none(), "physical-gold ETC must be gated once it classes as a fund");

        // and the gate is SELECTIVE, not "every ETF drops out" — which would fake the pass above.
        let mut broad = core::backtest_quote("XDWD.L", &dates, &closes, &[], n - 1, 252, &BTreeMap::new());
        stamp_asset_class(&mut broad, "Xtrackers MSCI World UCITS ETF", "ETF", &etf_set, &sector_of);
        assert_eq!(picks::asset_class(&broad), 1);
        assert!(growth_score(&broad, &tuning).is_some(), "a plain index ETF must keep scoring");
    }

    /// END-TO-END walk-forward over a universe carrying ALL THREE classes — the composition
    /// `walk_forward_edge_pin` cannot reach. Every name runs the same closed-form generator, so any
    /// per-class difference below comes from the CLASS SPLIT and not from the prices.
    #[test]
    fn mixed_class_walk_forward_pin() {
        let n_bars = 13 * 252;
        let dates = synth_calendar(n_bars);
        let etf_set: HashSet<String> = HashSet::new();
        let sector_of: HashMap<String, String> = HashMap::new();
        // ticker, name, Yahoo instrumentType, CAGR, wobble amplitude, phase
        let universe: [(&str, &str, &str, f64, f64, f64); 9] = [
            ("WIN1", "Winner Industries AG", "EQUITY", 0.22, 0.04, 0.0),
            ("MID1", "Middling Corp", "EQUITY", 0.10, 0.08, 2.0),
            ("LOSE", "Sinking Corp", "EQUITY", -0.08, 0.10, 5.0),
            ("XDWD.L", "Xtrackers MSCI World UCITS ETF", "ETF", 0.12, 0.05, 1.0),
            ("VWRA.L", "Vanguard FTSE All-World UCITS ETF", "ETF", 0.09, 0.06, 3.0),
            ("SEMI.AS", "VanEck Semiconductor UCITS ETF", "ETF", 0.19, 0.14, 0.5),
            ("BTC-EUR", "BTC-EUR", "CRYPTOCURRENCY", 0.55, 0.30, 1.5),
            ("ETH-EUR", "ETH-EUR", "CRYPTOCURRENCY", 0.35, 0.35, 4.0),
            ("LTC-EUR", "LTC-EUR", "CRYPTOCURRENCY", -0.05, 0.25, 2.5),
        ];
        // run()'s pre-(#790) cutoff walk, twice: once with the class fields stamped on, once without —
        // the second is precisely the pre-fix behaviour, where every fund and coin is a "stock".
        let years = 5i64;
        let build = |stamp: bool| -> Vec<Sample> {
            let mut out: Vec<Sample> = Vec::new();
            for (tk, name, ity, g, amp, ph) in &universe {
                let closes = synth_series(n_bars, *g, *amp, *ph);
                let mut i = MIN_HISTORY;
                while i < dates.len() {
                    let target = dates[i] + chrono::Duration::days(years * 365);
                    let Some(off) = dates[i..].iter().position(|d| *d >= target) else { break };
                    let realized = (closes[i + off] / closes[i] - 1.0) * 100.0;
                    let mut quote = core::backtest_quote(tk, &dates, &closes, &[], i, 252, &BTreeMap::new());
                    if stamp {
                        stamp_asset_class(&mut quote, name, ity, &etf_set, &sector_of);
                    }
                    out.push(Sample { date: dates[i], realized, relative: 0.0, quote: Arc::new(quote), fund: None, trail: Vec::new() });
                    i += STEP_SESSIONS;
                }
            }
            out
        };

        let mut samples = build(true);
        demean(&mut samples);
        // every (bucket, class) group nets to ~0 — demean's promise, now checked PER CLASS.
        let mut sums: HashMap<(i32, u8), f64> = HashMap::new();
        for s in &samples {
            *sums.entry((bucket(s.date), picks::asset_class(&s.quote))).or_insert(0.0) += s.relative;
        }
        assert!(sums.values().all(|v| v.abs() < 1e-6), "per-(bucket,class) relatives must net to 0");
        let classes: HashSet<u8> = sums.keys().map(|k| k.1).collect();
        assert_eq!(classes.len(), 3, "all three classes must survive the pipeline, got {classes:?}");

        // THE REGRESSION. Unstamped, the funds and coins de-mean against the STOCK peer-mean and land
        // on different relatives. If these ever stop differing, the class split is dead again.
        let mut pooled = build(false);
        demean(&mut pooled);
        assert!(pooled.iter().all(|s| picks::asset_class(&s.quote) != 1), "unstamped: no ETF can exist");
        let moved = samples
            .iter()
            .zip(&pooled)
            .filter(|(a, b)| (a.relative - b.relative).abs() > 1e-6)
            .count();
        assert!(moved > 0, "stamping must change the peer-relative returns — it groups demean");

        let tuning = BuyHeuristic::default();
        let (rho, edge) = lane_metrics(&samples, growth_score, &tuning);
        let scored: Vec<(&Sample, f64)> =
            samples.iter().filter_map(|s| growth_score(&s.quote, &tuning).map(|v| (s, v))).collect();
        let n_of = |c: u8| samples.iter().filter(|s| picks::asset_class(&s.quote) == c).count();
        let got = format!(
            "n={} crypto={} etf={} stock={} moved={} scored={} rho={:.3} edge={:+.1}",
            samples.len(), n_of(0), n_of(1), n_of(2), moved, scored.len(), rho.unwrap_or(f64::NAN), edge,
        );
        // golden values from the run that introduced this test. As with the pin above, the SIGNS carry
        // no quality claim (synthetic series, 9 names) — only their exact stability matters.
        // `moved=66` is the shape of the bug, not a round number: 33 ETF rows (which changed class) plus
        // 33 stock rows (whose peer-mean LOST those ETFs). The 33 crypto rows are byte-identical in both
        // runs — crypto is the one class the bare stub always classed correctly, so it had nothing to fix.
        assert_eq!(got, "n=99 crypto=33 etf=33 stock=33 moved=66 scored=42 rho=0.211 edge=+5.6");
    }

    /// Does a coin reconstructed by `backtest_quote` actually take the CRYPTO branch of every forked
    /// gate — or does it merely happen to pass the equity one?
    ///
    /// `picks.rs` already unit-tests these gates, but on hand-built quotes with the fields assigned
    /// directly. That is a different question from the one that decides whether a backtest number means
    /// anything: whether the RECONSTRUCTION fills the fields the crypto branch reads. Round 1 is the
    /// standing proof that "obviously it works" beliefs about this exact reconstruction survive for
    /// years — `growth_min_cagr`'s own receipt shipped calling its life-CAGR leg unreachable in the
    /// backtest, on the false premise that `backtest_quote` could not supply `life_cagr`.
    ///
    /// Each pair asserts BOTH directions, which is what makes it unfakeable: the crypto knob gates the
    /// coin AND the equity twin leaves it alone; the equity knob gates the stock AND the crypto twin
    /// leaves it alone. A quote taking the wrong branch fails one side or the other.
    #[test]
    fn each_class_reads_its_own_gate_leg() {
        // same non-degenerate series the reachability pin uses, and for the same reason: the maxdd
        // fork below compares against `max_drawdown_pct`, which is exactly 0.00 on a series whose
        // drift never lets it dip. A cap probe cannot bite a zero, so the fork would assert nothing.
        let n = 25 * 252;
        let dates = synth_calendar(n);
        let closes = synth_series(n, 0.22, 0.25, 1.0);
        let (etf_set, sector_of) = (HashSet::new(), HashMap::new());
        let d = BuyHeuristic::default();
        let build = |tk: &str, name: &str, ity: &str| {
            let mut q = core::backtest_quote(tk, &dates, &closes, &[], n - 1, 252, &BTreeMap::new());
            stamp_asset_class(&mut q, name, ity, &etf_set, &sector_of);
            q
        };
        let coin = build("BTC-EUR", "BTC-EUR", "CRYPTOCURRENCY");
        let stock = build("XOM", "Exxon Mobil Corp", "EQUITY");
        assert_eq!((picks::asset_class(&coin), picks::asset_class(&stock)), (0, 2));
        assert!(growth_score(&coin, &d).is_some() && growth_score(&stock, &d).is_some());

        // (crypto setter, equity setter, probe) — floors crank up, caps crank down, both past anything
        // the fixture can reach, so a gate that CAN see this quote must bite.
        let forks: &[(&str, fn(&mut BuyHeuristic, f64), fn(&mut BuyHeuristic, f64), f64)] = &[
            ("1Y floor", |t, v| t.min_1y_pct_crypto = v, |t, v| t.growth_min_1y_pct = v, 1e9),
            ("1M knife", |t, v| t.max_1m_drop_pct_crypto = v, |t, v| t.max_1m_drop_pct = v, 1e9),
            ("range floor", |t, v| t.growth_min_range_pct_crypto = v, |t, v| t.growth_min_range_pct = v, 1e9),
            ("CAGR floor", |t, v| t.growth_min_cagr_crypto = v, |t, v| t.growth_min_cagr = v, 1e9),
            ("maxdd cap", |t, v| t.growth_maxdd_cap_crypto = v, |t, v| t.growth_maxdd_cap = v, 1e-6),
        ];
        for (label, set_crypto, set_equity, probe) in forks {
            let tuned = |set: &fn(&mut BuyHeuristic, f64)| {
                let mut t = d.clone();
                set(&mut t, *probe);
                t
            };
            let (ct, et) = (tuned(set_crypto), tuned(set_equity));
            assert!(growth_score(&coin, &ct).is_none(), "{label}: crypto knob must gate the coin");
            assert!(growth_score(&coin, &et).is_some(), "{label}: equity knob must NOT reach the coin");
            assert!(growth_score(&stock, &et).is_none(), "{label}: equity knob must gate the stock");
            assert!(growth_score(&stock, &ct).is_some(), "{label}: crypto knob must NOT reach the stock");
        }

        // sharpe_cap_etf has no twin — it is a cap that applies to funds only. Same shape: it must move
        // the ETF's score and leave the other two classes byte-identical.
        let etf = build("XDWD.L", "Xtrackers MSCI World UCITS ETF", "ETF");
        assert_eq!(picks::asset_class(&etf), 1);
        let capped = BuyHeuristic { sharpe_cap_etf: 1e-6, ..d.clone() };
        assert_ne!(growth_score(&etf, &capped), growth_score(&etf, &d), "sharpe_cap_etf must reach a fund");
        assert_eq!(growth_score(&stock, &capped), growth_score(&stock, &d), "…and no stock");
        assert_eq!(growth_score(&coin, &capped), growth_score(&coin, &d), "…and no coin");
    }

    /// `hold_period_sweep` is network + println only, so its per-ticker walk — the SECOND of the two
    /// scoring loops in this file — had no coverage at all. `sweep_cutoffs` is that walk, extracted
    /// unchanged. `run()` was deliberately left alone, so `walk_forward_edge_pin` proves the extraction
    /// touched nothing else.
    #[test]
    fn sweep_cutoffs_walks_every_hold_and_stamps_classes() {
        // ~13y so the longest hold below still has a full forward window after MIN_HISTORY (750 bars,
        // ~3y) of warm-up — at 6y the 5y hold yields nothing and the test would assert on an empty set.
        let n = 13 * 252;
        let dates = synth_calendar(n);
        let closes = synth_series(n, 0.12, 0.05, 1.0);
        let etf_set: HashSet<String> = ["VWRA.L".to_string()].into_iter().collect();
        let sector_of = HashMap::new();
        let holds = [1i64, 2, 5];
        let walk = |c: &[f64]| {
            sweep_cutoffs("VWRA.L", &dates, c, &[], "VANGUARD FUNDS PLC", "", &holds, MIN_HISTORY, 252, &etf_set, &sector_of, &BTreeMap::new())
        };
        let got = walk(&closes);

        // (#790) each hold cuts once per half-year bucket, in consecutive buckets, on the bucket's first bar.
        for &h in &holds {
            let cut: Vec<NaiveDate> = got.iter().filter(|(w, _)| *w == h).map(|(_, s)| s.date).collect();
            assert!(cut.windows(2).all(|p| bucket(p[1]) == bucket(p[0]) + 1), "{h}y hold skipped or repeated a bucket");
            for d in &cut {
                let k = dates.binary_search(d).unwrap();
                assert_ne!(bucket(dates[k - 1]), bucket(*d), "{h}y cutoff {d} is not its bucket's first bar");
            }
        }

        // every hold produces cutoffs, and a longer hold can only yield FEWER (it needs more forward
        // history) — the shape that makes the per-hold edge rows comparable at all.
        for &h in &holds {
            assert!(got.iter().any(|(w, _)| *w == h), "{h}y hold produced no cutoffs");
        }
        let n_of = |h: i64| got.iter().filter(|(w, _)| *w == h).count();
        assert!(n_of(1) >= n_of(2) && n_of(2) >= n_of(5), "longer holds must not yield more cutoffs");
        // the stamping reached it — via etf_set here, since instrument_type is deliberately left blank.
        assert!(got.iter().all(|(_, s)| picks::asset_class(&s.quote) == 1), "sweep must class funds as ETFs");

        // a zero close poisons `realized` to ±inf, and one such row drags a whole de-meaned bucket to
        // -inf. Short holds reach early data the 12y path never walks, which is where this bites.
        // (#790) The first cutoff is the first bucket opening at or after MIN_HISTORY, not bar 750.
        let mut poisoned = closes.clone();
        poisoned[cutoffs(&dates, MIN_HISTORY).next().unwrap()] = 0.0;
        let after = walk(&poisoned);
        assert!(after.iter().all(|(_, s)| s.realized.is_finite()), "non-finite realized must be dropped");
        assert!(after.len() < got.len(), "the poisoned cutoffs must actually have been dropped");
    }

    /// The census is the diagnostic every conclusion in the ETF-classification receipt rests on, and the
    /// original bug survived years precisely because no class count was ever printed or checked.
    #[test]
    fn class_census_counts_scored_and_total_per_class() {
        let n = 13 * 252;
        let dates = synth_calendar(n);
        let (etf_set, sector_of) = (HashSet::new(), HashMap::new());
        let d = BuyHeuristic::default();
        let mk = |tk: &str, name: &str, ity: &str, g: f64| {
            let closes = synth_series(n, g, 0.04, 0.0);
            let mut quote = core::backtest_quote(tk, &dates, &closes, &[], n - 1, 252, &BTreeMap::new());
            stamp_asset_class(&mut quote, name, ity, &etf_set, &sector_of);
            Sample { date: dates[n - 1], realized: 0.0, relative: 0.0, quote: Arc::new(quote), fund: None, trail: Vec::new() }
        };
        // one compounder and one sinking laggard per class: the scored counts must come out BELOW the
        // totals, else this would pass just as happily against a fn that returned the totals twice.
        let samples = [
            mk("BTC-EUR", "BTC-EUR", "CRYPTOCURRENCY", 0.30),
            mk("DEAD-EUR", "DEAD-EUR", "CRYPTOCURRENCY", -0.30),
            mk("XDWD.L", "Xtrackers MSCI World UCITS ETF", "ETF", 0.14),
            mk("BAD.L", "Sinking UCITS ETF", "ETF", -0.30),
            mk("WIN", "Winner Industries AG", "EQUITY", 0.22),
            mk("LOSE", "Sinking Corp", "EQUITY", -0.30),
        ];
        let census = class_census(&samples, &d);
        assert_eq!([census[0].0, census[1].0, census[2].0], [2, 2, 2], "totals per class");
        assert_eq!([census[0].1, census[1].1, census[2].1], [1, 1, 1], "only the compounder scores");
        // and it is indexed BY picks::asset_class, not by insertion order
        assert_eq!(class_census(&samples[..2], &d)[0].0, 2, "crypto lands at index 0");
        assert_eq!(class_census(&samples[2..4], &d)[1].0, 2, "ETF lands at index 1");
        assert_eq!(class_census(&samples[4..], &d)[2].0, 2, "stock lands at index 2");
    }

    /// 40 years of monthly bars, one grid, ten shapes: four compounders that clear the shipped gates in
    /// a known order, one that earns its CAGR through a −55% pit, and five that should never score.
    /// Deterministic — closes are `base * rate^(i/12)`, so R² is ~1 everywhere and the score ordering
    /// reduces to the CAGR terms, which is exactly the ordering the forward returns have too.
    fn synthetic_universe() -> (Vec<NaiveDate>, Vec<(&'static str, Vec<f64>)>) {
        const BARS: usize = 480;
        let dates: Vec<NaiveDate> = (0..BARS)
            .map(|i| {
                let (y, m) = (1985 + (i / 12) as i32, (i % 12) as u32 + 1);
                NaiveDate::from_ymd_opt(y, m, 1).unwrap()
            })
            .collect();
        // `rate` = annual growth factor; `phase` decorrelates the CYCLE from the rate; `pit` = the
        // drawdown-and-recovery name.
        //
        // The ripple is not decoration. A pure exponential is PERMANENTLY extended above its 200wk MA
        // by an amount that is a strict function of its rate, so extension and CAGR are perfectly
        // collinear — and the shipped over-extension brake then docks precisely the fastest compounder.
        // Measured on the first draft of this fixture: score ran backwards (30%/yr scored 9.16, 20%/yr
        // scored 9.91) and the lane edge came out −614.9 at rho −0.58. That is a pathology of constant-
        // rate curves, not of the scorer; real compounders oscillate around their trend and are extended
        // or not depending on WHERE IN THE CYCLE they are. Giving each name its own phase restores that.
        let series = |rate: f64, phase: f64, pit: bool| -> Vec<f64> {
            (0..BARS)
                .map(|i| {
                    let t = i as f64;
                    // ±18% cycle, 40-month period — big enough to move the near-high gate and the MA
                    // distance, far too small to overturn 12 years of compounding in the forward window.
                    let cycle = 1.0 + 0.18 * ((t + phase) * std::f64::consts::TAU / 40.0).sin();
                    let base = 100.0 * rate.powf(t / 12.0) * cycle;
                    // triangular −55% dip over bars 180..240, fully recovered after: a real maxdd and a
                    // real underwater stretch on a name whose long CAGR still clears the bar.
                    if pit && (180..=240).contains(&i) {
                        base * (1.0 - 0.55 * (1.0 - ((t - 210.0) / 30.0).abs()))
                    } else {
                        base
                    }
                })
                .collect()
        };
        let names = vec![
            ("CMPA", series(1.30, 0.0, false)), // four compounders, rates apart, cycles out of step
            ("CMPB", series(1.26, 13.0, false)),
            ("CMPC", series(1.22, 27.0, false)),
            ("CMPD", series(1.20, 7.0, false)),
            ("DRAW", series(1.24, 33.0, true)), // passes on the flat stretches, gated inside the pit
            ("MODE", series(1.12, 20.0, false)), // < growth_min_cagr 19 -> never scores
            ("SLOW", series(1.06, 3.0, false)),
            ("FLAT", series(1.01, 17.0, false)),
            ("FADE", series(0.97, 30.0, false)), // fails the lifetime uptrend too
            ("SINK", series(0.92, 11.0, false)),
        ];
        (dates, names)
    }

    /// Build the walk-forward sample set the wide `backtest 12 universe` builds, from synthetic prices
    /// instead of live ones — same `sweep_cutoffs`, same `demean`, same monthly params `run` uses.
    fn synthetic_samples() -> Vec<Sample> {
        let (dates, names) = synthetic_universe();
        let (etf_set, sector_of) = (HashSet::new(), HashMap::new());
        let mut samples: Vec<Sample> = names
            .iter()
            .flat_map(|(tk, closes)| {
                // holds=[12] / min_history 36 / cadence 12 — `run`'s monthly branch verbatim. Bar 36 is
                // 1988-01, a bucket opening, so the (#790) grid cuts the same Jan/Jul dates the old
                // 6-bar stride did.
                sweep_cutoffs(tk, &dates, closes, &[], tk, "", &[12], 36, 12, &etf_set, &sector_of, &BTreeMap::new())
                    .into_iter()
                    .map(|(_, s)| s)
            })
            .collect();
        demean(&mut samples);
        samples
    }

    /// The SHIPPED tuning, not the code defaults. `BuyHeuristic::default()` is deliberately neutral —
    /// `growth_value_floor_pct` reads "0 = off (default)" there while ci-settings ships 40.0 — so a test
    /// that scored with it would grade a tuning nobody runs. Same raw-parse the config pins use (no merge,
    /// no FOLIOMAN_CONFIG, no race with the other tests in this file).
    fn shipped_tuning() -> BuyHeuristic {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        let s: config::Settings = serde_yaml::from_str(&text).expect("parse tests/ci-settings.yaml");
        s.buy_heuristic
    }

    /// (#565) [`shipped_tuning`] with every gate hard. At `growth_gate_soft` 0.6 all 500 fixture samples
    /// score, so a test of what the GATES admit (notch cohorts, the 158 pin) would go inert on it. (#566)
    /// arms stocks only, at 0.8; every fixture name is a stock, so this still matters.
    /// A passer scores bit-identically at any f, so this is the shipped tuning's own passer crowd.
    fn shipped_hard() -> BuyHeuristic {
        BuyHeuristic { growth_gate_soft: 0.0, ..shipped_tuning() }
    }

    /// (#324) The shipped tuning on the synthetic world: `cleared` holds only cleared names, each union row adds
    /// exactly what its notch newly admits (the GATE SWEEP's rule, counted independently here), notches that admit
    /// nothing print no row, and with no benchmark there is no book.
    #[test]
    fn notch_books_add_each_notch_cohort_to_the_cleared_book() {
        let (samples, tuning) = (synthetic_samples(), shipped_hard());
        let (dates, _) = synthetic_universe();
        let bench = (dates.clone(), (0..dates.len()).map(|i| 1.08_f64.powf(i as f64 / 12.0)).collect());
        let rows = notch_books(&samples, &bench, 12, &tuning);
        let got: Vec<(&str, usize)> = rows.iter().map(|(l, n, _, _)| (l.as_str(), *n)).collect();
        let fwd = |s: &&Sample| picks::asset_class(&s.quote) != 0 && benchmark_fwd(&bench.0, &bench.1, s.date, 12).is_some();
        let mut want = vec![("cleared", samples.iter().filter(fwd).filter(|s| growth_score(&s.quote, &tuning).is_some()).count())];
        for (tag, _, loosen) in picks::gate_notches() {
            let mut t = tuning.clone();
            loosen(&mut t);
            let n = samples.iter().filter(fwd).filter(|s| growth_score(&s.quote, &tuning).is_none() && growth_score(&s.quote, &t).is_some()).count();
            if n > 0 {
                want.push((tag, n));
            }
        }
        assert_eq!(got, want);
        assert_eq!(got, [("cleared", 158), ("cagr", 16), ("1Y+", 31), ("history", 11)]);
        // (#330) THE UNCAPPED COLUMN IS PINNED TO THE DIGIT ACROSS THE BASKET CHANGE. Giving the rows a real
        // rank key cannot move it — uncapped, `book_multiples` takes the whole bucket and a mean is
        // order-independent — and these four figures are (#324)'s, unchanged since.
        let book: Vec<f64> = rows.iter().map(|r| r.2.st.0).collect();
        for (b, pin) in book.iter().zip([24.591905072812, 24.473134675704, 25.537195954510, 24.478451418977]) {
            assert!((b - pin).abs() < 1e-9, "{book:?}");
        }
        // (#324)'s flat bar is unchanged by (#329) — passing None must reproduce it digit for digit.
        let flat: Vec<bool> = rows[1..].iter().map(|r| guard_holds(&rows[0].2.st, &r.2.st, None)).collect();
        assert_eq!(flat, [false, true, false], "{rows:?}");
        // (#329) every cohort row carries a matched-n band; only the base row has none.
        assert!(rows[0].2.bar.is_none() && rows[0].3.bar.is_none(), "the cleared row is what the others are measured against");
        assert!(rows[1..].iter().all(|r| r.2.bar.is_some() && r.3.bar.is_some()), "{rows:?}");
        // ...and the band must actually CHANGE the board, or it is a column nobody reads. Cleared
        // excess is 16.5919, so the flat bar asks 16.4919 of all three alike. `cagr` (n=16, union
        // 16.4731) misses that by 0.019 and clears its own 16.2151 band; `history` (n=11, union
        // 16.4785) is refused by both — a smaller cohort drawing a STRICTER bar (16.4811) than the
        // larger one, which is the n-bias running the other way and exactly what (#329) removes.
        let banded: Vec<bool> = rows[1..].iter().map(|r| guard_holds(&rows[0].2.st, &r.2.st, r.2.bar)).collect();
        assert_eq!(banded, [true, true, false], "{rows:?}");
        assert_ne!(banded, flat, "a band that never overrules the flat bar is not worth printing");
        assert!(notch_books(&samples, &(Vec::new(), Vec::new()), 12, &tuning).is_empty());
    }

    /// (#327) `n` monthly bars from `y`/`m`, compounding a return pattern keyed on the ABSOLUTE month
    /// index — so a young series and an old one built from the same pattern really do move together
    /// over their overlap, the way two wrappers on one index do. A flat pattern would have no variance
    /// and `pearson` would refuse it, which is correct and useless as a fixture.
    fn proxy_series(y: i32, m: u32, off: usize, n: usize, pat: impl Fn(usize) -> f64) -> (Vec<NaiveDate>, Vec<f64>) {
        let mut closes = vec![100.0];
        for k in 1..n {
            closes.push(closes[k - 1] * (1.0 + pat(off + k) / 100.0));
        }
        (month_ends(y, m, n), closes)
    }

    /// (#327) The splice a backtest series is allowed, and the three ways it is refused. The `pat` here
    /// is the "index" both wrappers track; the young listing starts 168 months into it.
    #[test]
    fn apply_proxy_splices_only_a_pair_that_proves_itself() {
        let pat = |k: usize| if k.is_multiple_of(3) { 4.0 } else { -1.0 };
        let (yd, yc) = proxy_series(2019, 1, 168, 60, pat); // 2019-01 .. 2023-12
        let (dd, dc) = proxy_series(2005, 1, 0, 228, pat); // 2005-01 .. 2023-12, the same index
        let map: BTreeMap<String, String> = [("YOUNG".to_string(), "OLD".to_string())].into();
        let mut donors: HashMap<&str, (Vec<NaiveDate>, Vec<f64>)> = HashMap::new();
        donors.insert("OLD", (dd.clone(), dc.clone()));

        // the pair proves itself on the young listing's first ~26 months -> spliced, 168 bars prepended
        let (d, c, on) = apply_proxy("YOUNG", yd.clone(), yc.clone(), &map, &donors);
        assert!(on);
        assert_eq!((d.len(), c.len()), (228, 228));
        assert_eq!(d[0], ymd(2005, 1, 1));
        // THE POINT-IN-TIME HAZARD, spelled out: the series now starts 14 years before the listing did.
        // `run`'s `own_first` guard is the only thing standing between that and a 2005 purchase of a
        // 2019 fund, and the goldens are its proof (the walk lives inside a `#[mutants::skip]` `run`).
        assert!(d[0] < yd[0]);
        assert_eq!(d[168..], yd[..], "the listing's own bars must survive the splice verbatim");
        // (#787) and no entry until the verify window that approved the splice has closed
        assert_eq!(proxy_entry(Some(yd[0]), on), Some(ymd(2021, 3, 11)));
        assert_eq!(proxy_entry(Some(yd[0]), false), Some(yd[0]));
        assert_eq!(proxy_entry(None, on), None);

        // a 2x tracker of the SAME index: correlates ~1.00, compounds elsewhere -> refused, series intact
        let (td, tc) = proxy_series(2005, 1, 0, 228, |k| pat(k) * 2.0);
        donors.insert("OLD", (td, tc));
        assert_eq!(apply_proxy("YOUNG", yd.clone(), yc.clone(), &map, &donors), (yd.clone(), yc.clone(), false));

        // a ticker nobody paired, and a pair whose donor the pool never fetched -> untouched, no panic
        donors.insert("OLD", (dd.clone(), dc.clone()));
        assert!(!apply_proxy("OTHER", yd.clone(), yc.clone(), &map, &donors).2);
        assert!(!apply_proxy("YOUNG", yd.clone(), yc.clone(), &map, &HashMap::new()).2);

        // a donor with nothing older than the listing -> `splice_history` declines and so does this
        donors.insert("OLD", (yd.clone(), yc.clone()));
        assert!(!apply_proxy("YOUNG", yd.clone(), yc.clone(), &map, &donors).2);
    }

    /// (#327) The walk's OWN discovery, which is the only kind whose pairs can ever be graded — the live
    /// map names funds listed after 2021, whose forward windows close in 2029. Same three legs as live
    /// discovery plus the age gap, checked here through `month_series` so the calendar-key arithmetic
    /// the pass is built on is exercised rather than assumed.
    #[test]
    fn discover_backtest_proxies_pairs_point_in_time() {
        let pat = |k: usize| if k.is_multiple_of(3) { 4.0 } else { -1.0 };
        let entry = |tk: &'static str, y: i32, off: usize, n: usize, f: &dyn Fn(usize) -> f64| {
            let (d, c) = proxy_series(y, 1, off, n, f);
            let (keys, rets) = month_series(&d, &c);
            (tk, d[0].year() * 12 + d[0].month() as i32, keys, rets)
        };
        let series = vec![
            entry("YOUNG", 2019, 168, 60, &pat),                      // 2019-01, needs a rescue
            entry("OLD", 2005, 0, 228, &pat),                         // 14y older, same index
            entry("LEVER", 2005, 0, 228, &|k| pat(k) * 2.0),          // same shape, twice the compounding
            entry("BARELY", 2018, 156, 72, &pat),                     // perfect twin, only 1y older
        ];
        let got = discover_backtest_proxies(&series);
        assert_eq!(got.get("YOUNG").map(String::as_str), Some("OLD"));
        // OLD and LEVER are old enough to be subjects too, but nothing in this pool predates them by
        // two years, so they pair with nobody — the age gap is a filter on DONORS, not just subjects.
        assert_eq!(got.len(), 2, "{got:?}"); // YOUNG and BARELY, each onto the one legal donor
        assert_eq!(got.get("BARELY").map(String::as_str), Some("OLD"));

        // drop OLD and the 2x twin is still refused rather than accepted as the last one standing
        let thin: Vec<_> = series.into_iter().filter(|s| s.0 != "OLD").collect();
        assert_eq!(discover_backtest_proxies(&thin).get("YOUNG"), None);
    }

    /// (#340) `month_series` exactly: a non-positive close is no bar, the last close in a month is its
    /// end, and each return is keyed by the month it ENDS in (`year*12 + month`).
    #[test]
    fn month_series_keys_each_return_by_its_end_month() {
        let d = [ymd(2020, 1, 15), ymd(2020, 1, 31), ymd(2020, 2, 10), ymd(2020, 3, 5), ymd(2020, 3, 20)];
        let (keys, rets) = month_series(&d, &[100.0, 0.0, 110.0, -1.0, 99.0]);
        assert_eq!(keys, vec![2020 * 12 + 2, 2020 * 12 + 3]);
        assert_eq!(rets.len(), 2);
        assert!((rets[0] - 10.0).abs() < 1e-9 && (rets[1] + 10.0).abs() < 1e-9, "{rets:?}");
    }

    /// (#340) What the census found unpinned in the discovery pass. Among LEGAL donors the higher rho
    /// wins, a perfect tie goes to the longer record, (#650) a tie on both to the ticker that sorts first,
    /// and none of it depends on the order the pool lists them in. The age gap is exactly two years: a
    /// perfect twin 18 months older is refused. Only the verify window is judged, so a listing that
    /// drifts after it keeps its pair. And overlap must be real months inside that window: a donor
    /// delisted 20 months in has no 24 to lend, whatever it traded before the young listing existed.
    #[test]
    fn discover_backtest_proxies_ranks_donors_and_judges_only_the_window() {
        let pat = |k: usize| if k.is_multiple_of(3) { 4.0 } else { -1.0 };
        let alt = |k: usize| if k.is_multiple_of(2) { 3.0 } else { -2.0 };
        let quad = |k: usize| if k.is_multiple_of(4) { 5.0 } else { -1.5 };
        let hex = |k: usize| if k.is_multiple_of(6) { 6.0 } else { -1.0 };
        // `off` is months since 2005-01, so every series reads the pattern at the same absolute month.
        let entry = |tk: &'static str, y: i32, m: u32, n: usize, f: &dyn Fn(usize) -> f64| {
            let (d, c) = proxy_series(y, m, ((y - 2005) * 12) as usize + m as usize - 1, n, f);
            let (keys, rets) = month_series(&d, &c);
            (tk, d[0].year() * 12 + d[0].month() as i32, keys, rets)
        };
        let oldest = entry("OLDEST", 2005, 1, 228, &pat);
        // the same record under a 2010 listing claim: an exact rho tie, so only age can break it
        let twin = ("TWIN", 2010 * 12 + 1, oldest.2.clone(), oldest.3.clone());
        // (#650) the same record AND the same listing month: a full tie, so only the ticker can break it
        let zed = ("ZED", oldest.1, oldest.2.clone(), oldest.3.clone());
        // NOISY (2016-01) clears every leg on a lower rho, and most of its record postdates YOUNG's listing.
        let noisy = entry("NOISY", 2016, 1, 96, &|k| pat(k) + if k.is_multiple_of(7) { 0.3 } else { 0.0 });
        for donors in [[noisy.clone(), oldest.clone(), twin.clone(), zed.clone()], [zed, twin, oldest, noisy]] {
            let mut pool = vec![
                entry("YOUNG", 2019, 1, 60, &pat),
                entry("Y2", 2019, 1, 60, &alt),
                entry("N2", 2017, 7, 78, &alt), // a perfect twin only 18 months older
                entry("Y3", 2019, 1, 60, &|k| if k < 168 + 27 { quad(k) } else { -quad(k) }),
                entry("D3", 2005, 1, 228, &quad),
                entry("Y4", 2019, 1, 60, &hex),
                entry("G4", 2005, 1, 189, &hex), // delisted 2020-09, 20 months into Y4's window
            ];
            pool.extend(donors);
            let got = discover_backtest_proxies(&pool);
            assert_eq!(got.get("YOUNG").map(String::as_str), Some("OLDEST"), "{got:?}");
            assert_eq!(got.get("Y2"), None, "18 months is under the two-year gap");
            assert_eq!(got.get("Y3").map(String::as_str), Some("D3"), "judged on the window, not on what came after");
            assert_eq!(got.get("Y4"), None, "20 months of real overlap is under PROXY_MIN_MONTHS");
        }
    }

    /// (#327) The verdict is FROZEN at first eligibility, and that cuts both ways. A donor that drifts
    /// during the young listing's first two years is refused even though it converges later; one that
    /// matched then is kept even though it drifts after. Both directions matter: the first stops a pair
    /// being talked into existence by evidence that did not exist yet, the second is what makes the
    /// backtest's pairing reproducible at a 2005 cutoff instead of a verdict about today.
    #[test]
    fn proxy_pairing_is_frozen_at_first_eligibility() {
        let pat = |k: usize| if k.is_multiple_of(3) { 4.0 } else { -1.0 };
        let (yd, yc) = proxy_series(2019, 1, 168, 60, pat);
        let map: BTreeMap<String, String> = [("YOUNG".to_string(), "OLD".to_string())].into();
        let mut donors: HashMap<&str, (Vec<NaiveDate>, Vec<f64>)> = HashMap::new();

        // drifts only over the first 30 overlapping months (< 2021-07), agrees ever after -> REFUSED
        let late = proxy_series(2005, 1, 0, 228, |k| if (168..198).contains(&k) { pat(k) * 3.0 } else { pat(k) });
        donors.insert("OLD", late);
        assert!(!apply_proxy("YOUNG", yd.clone(), yc.clone(), &map, &donors).2);

        // agrees over those months, drifts ever after -> ADMITTED on the evidence that existed
        let early = proxy_series(2005, 1, 0, 228, |k| if k >= 200 { pat(k) * 3.0 } else { pat(k) });
        donors.insert("OLD", early);
        assert!(apply_proxy("YOUNG", yd.clone(), yc, &map, &donors).2);
    }

    /// (#327) Bar (ii): the cleared book split by where its history came from. With nothing borrowed
    /// there is exactly ONE row and it is the book as it stands today — that is the no-op proof. Mark a
    /// cleared name's samples as borrowed and a second row appears carrying exactly those samples, so
    /// the guard is asked about the names the splice added and nothing else.
    #[test]
    fn proxy_books_split_the_cleared_book_by_provenance() {
        let (mut samples, tuning) = (synthetic_samples(), shipped_hard());
        let (dates, _) = synthetic_universe();
        let bench = (dates.clone(), (0..dates.len()).map(|i| 1.08_f64.powf(i as f64 / 12.0)).collect());

        let rows = proxy_books(&samples, &bench, 12, &tuning);
        assert_eq!(rows.len(), 1, "no borrowed history -> no second row");
        assert_eq!((rows[0].0.as_str(), rows[0].1), ("own history", 158)); // the same 158 `notch_books` clears

        // borrow one cleared name's history and nothing else
        let tk = samples
            .iter()
            .find(|s| growth_score(&s.quote, &tuning).is_some() && benchmark_fwd(&bench.0, &bench.1, s.date, 12).is_some())
            .map(|s| s.quote.ticker.clone())
            .expect("the synthetic world clears at least one name");
        let borrowed = samples.iter().filter(|s| s.quote.ticker == tk).count();
        for s in samples.iter_mut().filter(|s| s.quote.ticker == tk) {
            Arc::make_mut(&mut s.quote).history_proxied = true;
        }
        let rows = proxy_books(&samples, &bench, 12, &tuning);
        let got: Vec<(&str, usize)> = rows.iter().map(|(l, n, _)| (l.as_str(), *n)).collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "own history");
        assert_eq!(got[1].0, "+ proxied");
        assert_eq!(got[0].1 + got[1].1, 158, "the split moves samples, it never invents or drops them");
        assert!(got[1].1 > 0 && got[1].1 <= borrowed);
        // and the guard reads off the pair the same way the notch shadow does
        assert_eq!(guard_holds(&rows[0].2, &rows[1].2, None), rows[1].2 .2 >= rows[0].2 .2 - 0.1 && rows[1].2 .4 >= rows[0].2 .4 - 1.0);
        assert!(proxy_books(&samples, &(Vec::new(), Vec::new()), 12, &tuning).is_empty());
    }

    /// (#325) The tail replay on the synthetic world: only entries after `since` count, a line needs a cohort name AND
    /// a cleared name in its bucket, and its gap is cohort mean minus cleared mean (recounted independently here).
    #[test]
    fn notch_tail_grades_only_entries_after_since() {
        let (samples, tuning) = (synthetic_samples(), shipped_tuning());
        let mut dates: Vec<chrono::NaiveDate> = samples.iter().map(|s| s.date).collect();
        dates.sort();
        let (first, mid, last) = (dates[0], dates[dates.len() / 2], dates[dates.len() - 1]);
        let gaps = |since: chrono::NaiveDate, t: &BuyHeuristic| {
            let (mut cleared, mut cohort) = (std::collections::BTreeMap::<i32, Vec<f64>>::new(), std::collections::BTreeMap::<i32, Vec<f64>>::new());
            for s in samples.iter().filter(|s| s.date > since && picks::asset_class(&s.quote) != 0) {
                let side = if growth_score(&s.quote, &tuning).is_some() {
                    &mut cleared
                } else if growth_score(&s.quote, t).is_some() {
                    &mut cohort
                } else {
                    continue;
                };
                side.entry(bucket(s.date)).or_default().push(s.realized);
            }
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            cohort.iter().filter_map(|(b, v)| cleared.get(b).map(|k| mean(v) - mean(k))).collect::<Vec<f64>>()
        };
        // (#1035) The median is the recount's, not a pin: 26 and 6 lines are even counts, read as the two middle gaps' mean.
        for (since, pins) in [
            (first.pred_opt().unwrap(), [("cagr", 15, -256.895200120187), ("1Y+", 26, 826.404569217314)]),
            (mid, [("cagr", 6, -138.124976295149), ("1Y+", 15, 704.172422548712)]),
        ] {
            let rows = notch_tail(&samples, since, &tuning);
            let want: Vec<(&str, Vec<f64>)> = notch_tunings(&tuning).iter().map(|(tag, t)| (*tag, gaps(since, t))).filter(|(_, g)| !g.is_empty()).collect();
            assert_eq!(rows.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), want.iter().map(|(tag, g)| (*tag, g.len())).collect::<Vec<_>>(), "since {since}");
            for (((tag, n, mean, median), (ptag, pn, pmean)), (_, g)) in rows.iter().zip(pins).zip(&want) {
                let recount = crate::core::median(g.clone()).unwrap_or(f64::NAN);
                assert!((*tag, *n) == (ptag, pn) && (mean - pmean).abs() < 1e-9 && (median - recount).abs() < 1e-9, "since {since}: {rows:?}");
            }
        }
        assert!(notch_tail(&samples, last, &tuning).is_empty(), "nothing is after the last entry");
    }

    /// (#324) The guard's two bars, each at its exact boundary and just past it.
    #[test]
    fn guard_holds_is_excess_within_a_tenth_and_worst_within_one() {
        let at = |e: f64, w: f64| guard_holds(&(0.0, 0.0, 1.0, 0.0, -5.0, 0.0, 0.0), &(0.0, 0.0, e, 0.0, w, 0.0, 0.0), None);
        assert!(at(0.9, -6.0));
        assert!(!at(0.89, -6.0));
        assert!(!at(0.9, -6.01));
    }

    /// (#329) The amended excess leg: a supplied band REPLACES `cleared - 0.1` and ignores the cleared
    /// row entirely, while `None` falls back to (#324)'s arithmetic. The worst-window leg answers to
    /// neither — it is the one bar this round left alone.
    #[test]
    fn guard_holds_uses_the_band_when_one_is_supplied() {
        let at = |e: f64, w: f64, bar: Option<f64>| guard_holds(&(0.0, 0.0, 1.0, 0.0, -5.0, 0.0, 0.0), &(0.0, 0.0, e, 0.0, w, 0.0, 0.0), bar);
        // a band STRICTER than the flat bar refuses what the flat bar waved through
        assert!(at(0.9, -6.0, None) && !at(0.9, -6.0, Some(2.0)));
        // and a band LOOSER than it admits what the flat bar refused — the reopen direction
        assert!(!at(0.5, -6.0, None) && at(0.5, -6.0, Some(0.4)));
        // exactly at the band passes, a hair under does not
        assert!(at(2.0, -6.0, Some(2.0)) && !at(1.99, -6.0, Some(2.0)));
        // the worst-window leg still refuses, whichever bar the excess leg is read against
        assert!(!at(9.9, -6.01, Some(0.0)) && !at(9.9, -6.01, None));
    }

    /// (#1049) The proxy table's guard cell: blank on the base row, then (#324)'s flat bar on both legs.
    #[test]
    fn proxy_guard_is_blank_on_the_base_row_and_reads_the_flat_bar() {
        let base = (0.0, 0.0, 1.0, 0.0, -5.0, 0.0, 0.0);
        let at = |e: f64, w: f64| (0.0, 0.0, e, 0.0, w, 0.0, 0.0);
        assert_eq!(proxy_guard(true, &base, &at(0.0, -9.0)), "");
        assert_eq!(proxy_guard(false, &base, &at(0.9, -6.0)), "  guard pass");
        assert_eq!(proxy_guard(false, &base, &at(0.89, -6.0)), "  guard FAIL");
        assert_eq!(proxy_guard(false, &base, &at(0.9, -6.01)), "  guard FAIL");
    }

    /// (#329) `n` identical rows in bucket `b`: realized `r`%, benchmark `bench`%. At `years = 1` `ann`
    /// is the identity, so `book_stats` reads excess straight off `r - bench` and every number in the
    /// band tests below is hand-checkable rather than blessed.
    fn band_book(b: i32, n: usize, r: f64, bench: f64) -> std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> {
        [(b, vec![(0.0, r, bench); n])].into_iter().collect()
    }

    /// (#330) A cohort shape of `n` rows in bucket `b`, every one entering at `rank`. (#329) carried a
    /// bare count here; the rank is what lets a drawn row compete for a capped basket.
    fn band_shape(b: i32, n: usize, rank: f64) -> std::collections::BTreeMap<i32, Vec<f64>> {
        [(b, vec![rank; n])].into_iter().collect()
    }

    /// (#329) THE CLAIM THE ROUND RESTS ON. (#324)'s flat `cleared - 0.1` is read off the UNION, so the
    /// bar it puts on a COHORT is `cleared - 0.1/w` and scales with `1/n`. The matched-n band does not:
    /// it tracks the book closely when the cohort is tiny, and falls away as the cohort grows, which is
    /// exactly the bias being removed. Shaped like the 8y fund pit (cleared excess +4.6).
    #[test]
    fn placebo_bar_is_stricter_at_small_n_and_looser_at_large_n() {
        let cleared = band_book(0, 200, 14.6, 10.0); // excess +4.6
        let refused = band_book(0, 200, 10.0, 10.0); // arbitrary refused names: excess 0.0
        let flat = 4.6 - 0.1; // what (#324) would have demanded of EVERY cohort, whatever its size
        let bar = |k: usize| placebo_bars(&cleared, &refused, &band_shape(0, k, 0.0), 1).unwrap().0;
        let (small, large) = (bar(2), bar(100));
        // 2 names can barely move a 200-name book, so an arbitrary pair already lands near +4.6 —
        // the bar TIGHTENS, and `range`'s free pass at n=2 dies.
        assert!(small > flat, "a 2-name cohort must answer to a bar near the book, got {small}");
        // 100 names dilute it heavily, so demanding +4.5 there was charging for SIZE, not quality.
        assert!(large < flat, "a 100-name cohort must not be refused for being large, got {large}");
        assert!(small > large, "the band must fall away with n: {small} vs {large}");
    }

    /// (#329) The seed is reset per call, so the band is reproducible — without that the goldens move
    /// under their own feet. And with a refused pool that VARIES, the draw is what decides the answer:
    /// a p95 must sit above the all-names mean and strictly inside the best/worst cohorts, which is
    /// what shows the sampler ran at all rather than returning a constant.
    #[test]
    fn placebo_bar_is_deterministic_and_actually_samples() {
        let cleared = band_book(0, 200, 14.6, 10.0);
        let refused: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> =
            [(0, (0..200).map(|i| (0.0, i as f64, 10.0)).collect())].into_iter().collect();
        let shape = band_shape(0, 20, 0.0);
        let (a, b) = (placebo_bars(&cleared, &refused, &shape, 1).unwrap().0, placebo_bars(&cleared, &refused, &shape, 1).unwrap().0);
        assert_eq!(a.to_bits(), b.to_bits(), "two calls must agree bit for bit, got {a} then {b}");
        // 20 rows drawn from realized 0..199 onto a 200-row book at 14.6: mean case 12.32, extremes 3.27 / 21.36
        assert!(a > 12.32, "a p95 sits above the mean cohort, got {a}");
        assert!(a < 21.36, "and below the richest cohort the pool can supply, got {a}");
    }

    /// (#329) The null has to match the cohort's CUTOFF SHAPE, not just its size — otherwise a cohort
    /// concentrated in one regime is scored against a null spread over twenty years. A bucket the cohort
    /// drew from with no refused rows is a shape the pool cannot fill, so the band refuses to guess and
    /// the caller falls back to (#324)'s bar. An empty cohort dilutes nothing and reads the book itself.
    #[test]
    fn placebo_bar_refuses_a_shape_the_refused_pool_cannot_fill() {
        let cleared: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> =
            [(0, vec![(0.0, 14.6, 10.0); 100]), (1, vec![(0.0, 14.6, 10.0); 100])].into_iter().collect();
        let refused = band_book(0, 200, 10.0, 10.0); // bucket 1 is empty
        let spans_both: std::collections::BTreeMap<i32, Vec<f64>> = [(0, vec![0.0; 5]), (1, vec![0.0; 5])].into_iter().collect();
        assert!(placebo_bars(&cleared, &refused, &spans_both, 1).is_none(), "bucket 1 has no refused rows to draw");
        assert!(placebo_bars(&cleared, &refused, &band_shape(0, 5, 0.0), 1).is_some(), "bucket 0 alone can be matched");
        let empty = placebo_bars(&cleared, &refused, &Default::default(), 1).unwrap().0;
        assert!((empty - 4.6).abs() < 1e-9, "no cohort, no dilution — the band is the book, got {empty}");
    }

    /// (#330) THE BAND MUST BE DRAWN AT THE BASKET IT GRADES. A null read uncapped and then applied to a
    /// top-N row is (#329)'s mistake in a new place: uncapped, 10 arbitrary refused rows are averaged into
    /// a 200-name book and barely move it; at the published basket those same rows, entering at the
    /// cohort's ranks, TAKE the slots. The two bands must therefore differ, and the top one must be the
    /// harsher read, or the new column is being judged against the wrong null.
    #[test]
    fn placebo_bar_draws_at_the_basket_it_grades() {
        let cleared = band_book(0, 200, 14.6, 10.0); // excess +4.6, every row at rank 0.0
        let refused = band_book(0, 200, 10.0, 10.0); // arbitrary refused names: excess 0.0
        // ranks above every cleared row, so the drawn rows own all ten published slots
        let (all, top) = placebo_bars(&cleared, &refused, &band_shape(0, 10, 1.0), 1).unwrap();
        assert!((all - 4.6).abs() < 0.3, "uncapped, 10 rows in 210 hardly move the book, got {all}");
        assert!((top - 0.0).abs() < 1e-9, "at the basket the cohort IS the book, so the band is its own excess, got {top}");
        assert!(top < all, "a basket the cohort can capture must read harsher than the uncapped average");
        // and the same draws with the cohort ranked BELOW the book never reach the basket at all
        let below = placebo_bars(&cleared, &refused, &band_shape(0, 10, -1.0), 1).unwrap();
        assert!((below.1 - 4.6).abs() < 1e-9, "unreachable rows leave the published book untouched, got {}", below.1);
        // The uncapped band is rank-INVARIANT, but only to within floating-point summation order: sorting
        // the drawn rows to the front of the bucket instead of the back re-associates the mean and moves
        // it by tens of ULPs (~5e-14 here). That is why (#330)'s byte-identity claim for the old column is
        // made on the PRINTED goldens at one decimal, not on the bits.
        assert!((below.0 - all).abs() < 1e-9, "rank moves the basket reading, not the uncapped one: {} vs {all}", below.0);
        assert_ne!(below.1.to_bits(), top.to_bits(), "...while the basket reading is entirely rank's doing");
    }

    /// (#651) Nearest rank over the band it is handed, unsorted on the way in: 200 draws read the 190th
    /// smallest, 20 the 19th (under PLACEBO_DRAWS' rank that was past the end, so None), one draw
    /// itself, and an empty band nothing.
    #[test]
    fn percentile95_ranks_over_the_band_it_is_given() {
        let mut full: Vec<f64> = (1..=PLACEBO_DRAWS).rev().map(|v| v as f64).collect();
        assert_eq!(percentile95(&mut full), Some(190.0));
        let mut short: Vec<f64> = (1..=20).rev().map(f64::from).collect();
        assert_eq!(percentile95(&mut short), Some(19.0));
        assert_eq!(percentile95(&mut [7.0]), Some(7.0));
        assert_eq!(percentile95(&mut []), None);
    }

    /// (#330) THE CENTRAL CLAIM OF THE ROUND. A cohort that never outranks an incumbent changes nothing
    /// the tool publishes, so the union row is bit-identical to cleared — and that is neither a pass nor a
    /// cost. Through (#329) it was printed as `guard FAIL`, charging a notch for dilution the published
    /// book never pays.
    #[test]
    fn ship_basket_ignores_a_cohort_that_never_reaches_it() {
        let cleared = band_book(0, 200, 14.6, 10.0);
        let mut union = cleared.clone();
        union.get_mut(&0).unwrap().extend(vec![(-1.0, -50.0, 10.0); 40]); // ruinous, and ranked last
        let (c_all, c_top) = both_baskets(&cleared, None, 1).unwrap();
        let (u_all, u_top) = both_baskets(&union, None, 1).unwrap();
        assert!(u_all.st.2 < c_all.st.2 - 5.0, "uncapped, 40 disasters must drag the book hard");
        assert_eq!(u_top.st, c_top.st, "at the published basket they are never bought, so nothing moves");
        assert_eq!(ship_verdict(&c_top, &u_top), "inert");
        assert_eq!(ship_verdict(&c_all, &u_all), "FAIL", "the same cohort uncapped is a real cost");
    }

    /// (#330) The other half: a cohort name that DOES outrank an incumbent is bought, displaces it, and
    /// moves the published row. Without this the basket could be reading "inert" because it is inert,
    /// rather than because the cohort lost the competition.
    #[test]
    fn ship_basket_admits_a_name_that_outranks_an_incumbent() {
        let cleared = band_book(0, 200, 14.6, 10.0);
        let mut union = cleared.clone();
        union.get_mut(&0).unwrap().push((1.0, 114.6, 10.0)); // top-ranked, and a 100-point winner
        let (c_all, c_top) = both_baskets(&cleared, None, 1).unwrap();
        let (u_all, u_top) = both_baskets(&union, None, 1).unwrap();
        // one slot of ten swaps 14.6 for 114.6: the published excess moves by exactly a tenth of 100
        assert!((u_top.st.2 - c_top.st.2 - 10.0).abs() < 1e-9, "{} vs {}", u_top.st.2, c_top.st.2);
        assert!(u_all.st.2 - c_all.st.2 < 0.6, "uncapped, one name in 201 is a rounding error");
        assert_eq!(ship_verdict(&c_top, &u_top), "pass");
    }

    /// (#330) The three-way verdict pinned on its own, because a report that folds `inert` into either
    /// side is exactly the defect this round found: the notch board through (#329) had no way to say
    /// "this admission would change nothing we print".
    #[test]
    fn inert_is_not_reported_as_a_pass_or_a_fail() {
        let row = |e: f64, w: f64, bar: Option<f64>| BookRow { st: (0.0, 0.0, e, 0.0, w, 0.0, 0.0), bar };
        let cleared = row(1.0, -5.0, None);
        assert_eq!(ship_verdict(&cleared, &row(1.0, -5.0, None)), "inert");
        assert_eq!(ship_verdict(&cleared, &row(1.0, -5.0, Some(9.0))), "inert", "a bar cannot fail a row that moved nothing");
        assert_eq!(ship_verdict(&cleared, &row(0.95, -5.0, None)), "pass", "moved, and inside (#324)'s tenth");
        assert_eq!(ship_verdict(&cleared, &row(0.5, -5.0, None)), "FAIL");
        assert_eq!(ship_verdict(&cleared, &row(9.0, -6.01, None)), "FAIL", "the worst-window leg still bites");
        assert_eq!(ship_verdict(&cleared, &row(2.0, -5.0, Some(3.0))), "FAIL", "and the band is what a moved row answers to");
    }

    /// The OFFLINE half of the backtest gate. `backtest_edge_holds` (tests/network.rs) asserts two
    /// different things at once — "a scoring-code change or a default-tuning edit broke the edge" AND
    /// "the edge still holds on today's market" — and prices the first at the second's cost: 3000+ live
    /// tickers in a release build, so a scoring regression waits on the slowest CI job. This is the
    /// first half, deterministic and network-free, so it runs on every `cargo test`. Regime drift stays
    /// with the live gate; that is the one question a fixture genuinely cannot answer.
    ///
    /// WHY THE EDGE IS PINNED AND NOT ASSERTED POSITIVE. The obvious assert here is `edge > 0`, and it
    /// is not available honestly. This fixture is a PERSISTENCE world: each name's forward 12y return
    /// follows the same rate its history was built from. The shipped score docks over-extension, and a
    /// steady compounder's distance above its 200wk MA is a function of its rate — the MA lags ~2 years,
    /// so price/MA settles near rate² (1.69 at 30%/yr vs 1.44 at 20%/yr), permanently and regardless of
    /// where in any cycle the name sits. So the score ranks the SLOWER compounders higher here (measured:
    /// 13.1 at 30%/yr rising to 18.0 at 20%/yr) and the lane edge is negative by construction. That is
    /// not a defect in the scorer — the brake is a shipped, validated knob and the live +117 edge comes
    /// from a market where extension really does precede mean reversion. It is a property of persistence
    /// worlds, and no arrangement of steady compounders escapes it.
    ///
    /// Which means the SIGN of a synthetic edge is chosen by whoever picks the price shapes. Asserting
    /// `edge > 0` would only prove the fixture was bent until it went green. Pinning the VALUE is the
    /// non-circular assert, and it is strictly the stronger one: it fails on any scoring change, not
    /// just on one big enough to flip a sign.
    #[test]
    fn shipped_tuning_scores_fixture_unchanged() {
        let samples = synthetic_samples();
        let tuning = shipped_hard();
        let gated = |t: &BuyHeuristic| samples.iter().filter(|s| growth_score(&s.quote, t).is_some()).count();
        // (#565) SOFT PIN: armed, every fixture sample scores (docked by f^n). Every fixture name is a stock,
        // the one class (#566) arms. The pins below grade the passers.
        assert_eq!(gated(&shipped_tuning()), 500, "the shipped growth_gate_soft no longer scores every fixture sample");

        // GATE PIN. Moves when a knob changes what passes: re-validate with a live `backtest universe`
        // (both OOS halves positive) BEFORE moving it, exactly as the ci-settings receipts require.
        assert_eq!(
            gated(&tuning),
            158,
            "the shipped gates admit a different slice of the fixture than they did — if you moved a \
             knob, re-validate with a live `backtest universe` (both OOS halves positive) and then \
             move this pin; if you didn't, a gate's CODE changed and that is the regression"
        );

        // SCORE PIN. Same receipt rule. Tolerance is wide enough that reassociating a float sum is not a
        // red build and narrow enough that a real term change is: the value is ~-520 in units of pts.
        let (rho, edge) = lane_metrics(&samples, growth_score, &tuning);
        assert!(rho.is_some(), "too few scored samples to correlate — the fixture stopped reaching the lane");
        assert!(
            (edge - -520.2).abs() < 0.5,
            "GROWTH edge on the fixture moved to {edge:+.1} pts (pinned -520.2) — the score arithmetic \
             changed. Re-validate with a live `backtest universe` before moving this pin; see the note \
             above for why the sign is not the thing being asserted"
        );

        // The pin above is only worth having if it CAN move — a count nothing perturbs is a test that
        // passes forever. Raising the CAGR floor past two of the five compounders must drop it.
        let mut tighter = shipped_hard();
        tighter.growth_min_cagr = 25.0;
        assert!(
            gated(&tighter) < gated(&tuning),
            "tightening growth_min_cagr 19 -> 25 changed nothing: the pin is inert and would not catch \
             a knob regression either"
        );
    }

    /// (#47) WHICH GATES ARE LOAD-BEARING. Most of the ~20 growth gates sole-block nobody — every name
    /// they reject also fails something else, so removing them would return zero rows. Those are free,
    /// and the receipts say so. The review found nothing that detects the day one of them stops being
    /// free: a knob edit elsewhere can promote a decorative bar into the thing costing you rows, and the
    /// only place that shows up today is a live `screen` funnel nobody diffs.
    ///
    /// So pin the SET, not the counts. Counts are fixture trivia; the set is the claim the receipts make.
    /// Reuses `synthetic_samples()` deliberately — a second pool would need its own receipts and would
    /// drift from the one the score pin above already grades.
    #[test]
    fn sole_blocking_gates_are_pinned() {
        let samples = synthetic_samples();
        let sole = |t: &BuyHeuristic| {
            let mut s: std::collections::BTreeSet<&'static str> = Default::default();
            for x in &samples {
                if let Some([(gate, ..)]) = picks::gate_failures(&x.quote, t).as_deref() {
                    s.insert(gate);
                }
            }
            s.into_iter().collect::<Vec<_>>()
        };
        // NOT the live set. The live funnel sole-blocks `cagr peg maxdd 1Y+`; this pool carries no
        // fundamentals (so `peg` can never fire alone) and it does carry short-history names (so
        // `history`/`cagr-life` do). The pin grades THIS pool against itself — the point is that the
        // set is stable, not that a fixture reproduces the market.
        assert_eq!(
            sole(&shipped_tuning()),
            ["1Y+", "cagr", "cagr-life", "history"],
            "a different set of gates is now sole-blocking this fixture. If you moved a knob, a bar just \
             became load-bearing (or stopped being) — check the live `screen` funnel's sole-blocked column \
             before moving this pin. If you moved no knob, a gate's CODE changed."
        );

        // Same reason the sibling pin carries one: a set nothing perturbs is a test that passes forever.
        // A gate that sole-blocks nobody here is exactly the kind the review says nothing watches, so
        // tighten one and require it to ENTER the set.
        let mut tighter = shipped_tuning();
        tighter.growth_maxdd_cap = 10.0;
        assert!(sole(&tighter).contains(&"maxdd"), "tightening growth_maxdd_cap to 10% did not make it sole-block anyone: the pin is inert");
    }

    /// The index the synthetic world is graded against: 8%/yr on the universe's own month calendar.
    fn synthetic_bench() -> (Vec<NaiveDate>, Vec<f64>) {
        let (dates, _) = synthetic_universe();
        let closes: Vec<f64> = (0..dates.len()).map(|i| 1.08_f64.powf(i as f64 / 12.0)).collect();
        (dates, closes)
    }

    /// (#1238) Every entry bucket lands in the one corner its level and path name, the band and the cut are
    /// inclusive, a level inside the band sits out, and with no earnings series there is no table.
    #[test]
    fn level_path_report_puts_every_bucket_in_its_corner() {
        let (samples, t) = (synthetic_samples(), BuyHeuristic { entry_excess_yield_band: 1.0, ..shipped_hard() });
        let p = report_entry_state(&samples, &synthetic_bench(), 12, &t).expect("the fixture forms a book").6;
        let (dates, _) = synthetic_universe();
        let at = |v: f64| BTreeMap::from([(0, v)]);
        // the index tops at `hi` on the first bar and sits at 3 after: drawdown 0 at hi 3, -25 at 4, -62.5 at 8
        let grade = |hi: f64, ep: &BTreeMap<i32, f64>| {
            let closes: Vec<f64> = (0..dates.len()).map(|i| if i == 0 { hi } else { 3.0 }).collect();
            level_path_report(&samples, &(dates.clone(), closes), 12, &t, ep, &at(4.0), -25.0)
        };
        let counts = |out: &str| -> Vec<usize> { out.lines().filter_map(|l| l.split(" windows ").nth(1)?.split_whitespace().next()?.parse().ok()).collect() };
        let cheap_high = grade(3.0, &at(5.0));
        assert_eq!(counts(&cheap_high), [p, 0, 0, 0], "{cheap_high}");
        assert!(cheap_high.contains("one independent trial = 24 windows") && cheap_high.contains("  book "), "{cheap_high}");
        assert_eq!(counts(&grade(4.0, &at(3.0))), [0, 0, 0, p], "rich at -1 and a fall at -25 are both inclusive");
        assert_eq!(counts(&grade(8.0, &at(5.0))), [0, p, 0, 0]);
        assert_eq!(counts(&grade(3.0, &at(3.0))), [0, 0, p, 0]);
        assert_eq!(grade(3.0, &at(4.5)), "", "a level inside the band sits out");
        assert_eq!(grade(3.0, &BTreeMap::new()), "", "no earnings series, no table");
        assert!(p > 0);
    }

    /// The journal shell merges a second horizon into the first (so a short run never replaces a long one) and
    /// a path whose directory does not exist is reported, not created. Own file name: no other test touches it.
    #[test]
    fn write_verdict_merges_horizons_and_survives_an_unwritable_path() {
        let file = "write_verdict_test_journal.json";
        let path = config::data_path(file);
        let _ = std::fs::remove_file(&path);
        write_verdict(file, stub_verdict(8, VERDICT_TOP));
        write_verdict(file, stub_verdict(12, VERDICT_TOP));
        let raw = std::fs::read_to_string(&path).expect("write_verdict created the journal");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(parse_journal(&raw).keys().copied().collect::<Vec<i64>>(), [8, 12], "both horizons stay on file");

        let missing = "write_verdict_no_such_dir/journal.json";
        write_verdict(missing, stub_verdict(8, VERDICT_TOP));
        assert!(!config::data_path(missing).exists(), "a write into a missing directory leaves no file behind");
    }

    /// A month is ended by its LAST bar: the second February bar overwrites the first, so the February return
    /// is 120 over January's 105, not 110 over 100.
    #[test]
    fn month_series_ends_a_month_on_its_last_bar() {
        let d = [ymd(2020, 1, 15), ymd(2020, 1, 31), ymd(2020, 2, 10), ymd(2020, 2, 20), ymd(2020, 3, 5)];
        let (keys, rets) = month_series(&d, &[100.0, 105.0, 110.0, 120.0, 126.0]);
        assert_eq!(keys, vec![2020 * 12 + 2, 2020 * 12 + 3]);
        assert!((rets[0] - (120.0 / 105.0 - 1.0) * 100.0).abs() < 1e-9 && (rets[1] - 5.0).abs() < 1e-9, "{rets:?}");
    }

    #[test]
    fn bar_column_is_blank_on_the_base_row_and_names_a_missing_bar() {
        assert_eq!(bar_column(true, Some(1.0)), "");
        assert_eq!(bar_column(true, None), "");
        assert_eq!(bar_column(false, Some(2.34)), "  bar +2.3");
        assert_eq!(bar_column(false, None), "  bar n/a");
    }

    /// An empty bucket, or a book of zero names, yields no pair instead of a 0/0 multiple.
    #[test]
    fn book_multiples_skips_buckets_that_yield_no_pick() {
        let m: BTreeMap<i32, Vec<(f64, f64, f64)>> = BTreeMap::from([(0, Vec::new()), (1, vec![(2.0, 10.0, 0.0), (1.0, -10.0, 5.0)])]);
        let one = book_multiples(&m, 1);
        assert_eq!(one.len(), 1, "the empty bucket contributes no pair");
        assert!((one[0].0 - 1.1).abs() < 1e-9 && (one[0].1 - 1.0).abs() < 1e-9, "top-1 is the best-ranked row: {one:?}");
        assert!(book_multiples(&m, 0).is_empty(), "a zero-name book has no pair either");
    }

    /// (#968) Either total-return knob names the total-return index; only both off is price-only.
    #[test]
    fn either_total_return_knob_names_the_tr_benchmark() {
        let mut t = shipped_hard();
        t.backtest_tr_ruler = false;
        assert!(!bench_is_tr(false, &t), "both off: the price-only ^GSPC");
        assert!(bench_is_tr(true, &t), "adjusted close alone");
        t.backtest_tr_ruler = true;
        assert!(bench_is_tr(false, &t), "ruler alone: the missed case");
        assert!(bench_is_tr(true, &t));
    }

    /// (#1082) The too-few line names the fetched series and fires below 8 rows only.
    #[test]
    fn too_few_bench_rows_names_the_fetched_series() {
        let line = bench_too_few(7, "^SP500TR total-return").expect("7 rows are too few");
        assert!(line.contains("only 7 gated picks have a ^SP500TR total-return window"), "{line}");
        assert!(!line.contains("^GSPC"), "{line}");
        assert!(bench_too_few(8, "^GSPC").is_none(), "8 rows are enough");
        assert!(bench_too_few(9, "^GSPC").is_none());
    }

    /// The report fns that return or print only past a guard: no benchmark, no scorable pool, no trail. Each
    /// must decline quietly rather than index into an empty series or fabricate a row.
    #[test]
    fn report_helpers_decline_without_a_benchmark_or_a_scorable_pool() {
        let (samples, tuning, bench) = (synthetic_samples(), shipped_hard(), synthetic_bench());
        let none: (Vec<NaiveDate>, Vec<f64>) = (Vec::new(), Vec::new());
        assert!(report_entry_state(&samples, &none, 12, &tuning).is_none(), "no benchmark, no entry-state rows");
        assert!(report_entry_state(&[], &bench, 12, &tuning).is_none(), "no scorable sample, nothing to class");
        assert!(report_sized_book(&[], &bench, 12, &tuning, &config::Sizing::default(), 10, &[]).is_none(), "no bucket, no sized verdict");

        report_rivals(&samples, &none, 12, &tuning, PLACEBO_DRAWS);
        report_winners(&samples, &none, 12, &tuning);
        report_two_style_book(&samples, &none, 12, &tuning);
        report_vs_benchmark(&samples, &none, 12, &tuning);
        // with a benchmark but no fund coverage the two-style book has no earnings yield to combine
        report_two_style_book(&samples, &bench, 12, &tuning);
        // and with a benchmark but no gated picks the absolute table says "too few" instead of grading nothing
        report_vs_benchmark(&[], &bench, 12, &tuning);

        // CORR-CAP: no sample carries a trail -> no rows; a trail but nothing that scores -> header and footer only
        report_corr_cap(&[sample(ymd(2000, 1, 3), 0.0)], &bench, 12, &tuning);
        report_corr_cap(&[Sample { trail: vec![0.5; 12], ..sample(ymd(2000, 1, 3), 0.0) }], &bench, 12, &tuning);
    }

    /// (#738) A name carrying 8 adders is flagged at its cutoff and the report lists it. Eight managers add
    /// every synthetic name 10 days before each cutoff, so the `8+ added` listing has rows to print; the empty
    /// inputs print empty tables instead of dividing by zero.
    #[test]
    fn report_holder_cohorts_lists_the_scored_names_eight_managers_added() {
        let (samples, tuning) = (synthetic_samples(), shipped_hard());
        let mut txs: HashMap<String, Vec<core::HolderTx>> = HashMap::new();
        for s in &samples {
            let filed = s.date - chrono::Duration::days(10);
            txs.entry(s.quote.ticker.clone()).or_default().extend((1..=8).map(|m| core::HolderTx { filed, manager: m, add: true }));
        }
        let listed = samples
            .iter()
            .filter(|s| picks::asset_class(&s.quote) == 2 && growth_score(&s.quote, &tuning).is_some())
            .filter(|s| core::holder_flags(&txs[&s.quote.ticker], s.date).adders >= 8)
            .count();
        assert!(listed > 0, "the synthetic world scores stocks, and every one of them is flagged");
        assert!(holder_cohorts(&samples, &txs).iter().any(|r| r.1.n > 0), "some cohort holds a flagged stock");
        report_holder_cohorts(&samples, &txs, 12, &tuning);
        report_holder_cohorts(&[], &HashMap::new(), 12, &tuning);
    }

    /// (#325) Under HARD gates the replay has refused names to file. Each notch's line count is the number of
    /// buckets holding a name only that notch admits AND a cleared name, recounted here without `notch_tail`'s
    /// zip. The sibling pin runs at the soft shipped tuning, where nothing is refused and this arm never runs.
    #[test]
    fn notch_tail_under_hard_gates_files_each_refused_name_under_the_notch_that_admits_it() {
        let (samples, tuning) = (synthetic_samples(), shipped_hard());
        let since = samples.iter().map(|s| s.date).min().unwrap().pred_opt().unwrap();
        let rows = notch_tail(&samples, since, &tuning);
        assert!(!rows.is_empty(), "the hard world refuses names that a notch then admits next to cleared ones");
        let after: Vec<&Sample> = samples.iter().filter(|s| s.date > since && picks::asset_class(&s.quote) != 0).collect();
        let cleared: std::collections::BTreeSet<i32> = after.iter().filter(|s| growth_score(&s.quote, &tuning).is_some()).map(|s| bucket(s.date)).collect();
        for (tag, n, _, _) in &rows {
            let (_, loosened) = notch_tunings(&tuning).into_iter().find(|(g, _)| g == tag).expect("every row names a notch");
            let cohort: std::collections::BTreeSet<i32> = after
                .iter()
                .filter(|s| growth_score(&s.quote, &tuning).is_none() && growth_score(&s.quote, &loosened).is_some())
                .map(|s| bucket(s.date))
                .collect();
            assert_eq!(*n, cohort.intersection(&cleared).count(), "{tag}");
        }
    }

    /// (#328) The sector door adds a row only when a stamped sector floor admits a name the shipped floor
    /// refused. Unstamped, the world is the cleared book alone; stamp a floor of 0 on every name and the CAGR
    /// bar opens for the slow compounders, so a `sector-door` row appears carrying the admitted samples.
    #[test]
    fn sector_books_adds_a_door_row_only_when_a_sector_floor_admits_a_refused_name() {
        let (mut samples, tuning, bench) = (synthetic_samples(), shipped_hard(), synthetic_bench());
        let rows = sector_books(&samples, &bench, 12, &tuning);
        assert_eq!(rows.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), [("cleared", 158)], "no floor stamped, no door row");

        for s in &mut samples {
            Arc::make_mut(&mut s.quote).growth_sector_floor = Some(0.0);
        }
        let rows = sector_books(&samples, &bench, 12, &tuning);
        assert_eq!(rows.len(), 2, "{:?}", rows.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>());
        assert_eq!((rows[0].0.as_str(), rows[0].1), ("cleared", 158), "the door can only ADD: the cleared book is untouched");
        assert!(rows[1].0 == "sector-door" && rows[1].1 > 0, "{:?}", (rows[1].0.as_str(), rows[1].1));
    }

    /// The sweep printers on inputs too thin to grade: a curve with fewer than 8 scored windows says so, a
    /// split with fewer than 8 cutoffs a side says so, and the gate sweep prints a row per gate with or
    /// without the accepted-cohort median. None of them may panic.
    #[test]
    fn sweep_printers_handle_thin_and_degenerate_samples() {
        let (samples, tuning) = (synthetic_samples(), shipped_hard());
        weight_curve("test_knob", &[], &tuning, |t, v| t.growth_trend_weight = v, tuning.growth_trend_weight, &[0.0, 1.0], "note");
        tune_growth(&samples[..5], &tuning);
        let gates: Vec<Knob> = picks::gate_notches().into_iter().map(|(_, label, f)| knob(label, f)).collect();
        gate_sweep(&samples, &tuning, &gates, None);
        gate_sweep(&samples, &tuning, &gates, Some(0.0));
    }
}
