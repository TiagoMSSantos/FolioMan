//! `backtest [YEARS] [TICKERS...]` — zero-EXTRA-fetch sanity check of the buy heuristic. One chart
//! fetch per ticker (the same single call `check` makes — no worse rate-limit pressure), then it all
//! happens offline. Metrics: rho = Spearman rank correlation (selection skill); edge = top-half minus
//! bottom-half realized return, in points; OOS = Out-Of-Sample (early-vs-late split). Other acronyms
//! (CAGR, P/E, …): see the Glossary in README.md.
//!
//! - **(#3) walk-forward**: score the name at MANY cutoffs (~every 6 months back through its history),
//!   each measured against the realized return over the SAME `YEARS`-long forward window. Pools ~10×
//!   the samples a single as-of date gives, killing lucky-single-date bias — from the very same fetch.
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
//!   biased UP. Flagged in the footer — treat the edge as optimistic, never a forecast.
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
use crate::picks::{buy_score, growth_score};
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

/// Fewest resolved tickers a wide run must carry before it may overwrite [`VERDICT_FILE`]. Same floor
/// `backtest_edge_holds` applies to its own sample: below it, the pool is a throttle artefact rather
/// than the method's proof. A healthy run resolves ~4900.
const MIN_VERDICT_TICKERS: usize = 500;

/// Every string `tests/network.rs::backtest_edge_holds` locates a graded number by. The gate has no
/// parser — it string-searches this report — so each of these is load-bearing, and TEN OF THE
/// FOURTEEN fail SOFT when one goes missing:
///
/// - `tickers:`, `── GROWTH`, `windows scored:` — `return false`, i.e. SKIP, i.e. **GREEN**. These
///   three are deliberately soft: they are the throttle guard, and a live wide run that Yahoo
///   throttles must skip rather than red. The bug is not the softness, it is that a RENAME is
///   indistinguishable from a THROTTLE — both print "SKIPPED", both pass, and the gate then grades
///   nothing at all, forever, silently.
/// - `n=`, `peer-relative`, and the three `growth_*_->off` labels — the gate re-probe WARN just
///   stops firing.
/// - `top-10 `, `excess`, `tuning adds`, `early rho`, `late rho` — panic only under `forced`.
/// - `->  edge` — the only one that is hard unconditionally (`.expect`).
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
    /// The three shipped hard gates the re-probe WARN sweeps, by their GATE SWEEP row labels.
    pub const ABLATED_GATES: &[&str] =
        &["growth_max_above_ma ->off", "growth_require_lifetime_uptrend ->off", "growth_maxdd_cap ->off"];
}

pub const GATE_MARKERS: &[&str] = &[
    markers::TICKERS,
    markers::GROWTH_SECTION,
    markers::WINDOWS_SCORED,
    markers::LANE_EDGE,
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
pub(crate) fn tuning_fingerprint(t: &BuyHeuristic) -> String {
    serde_json::to_string(t).unwrap_or_default()
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

/// DELIBERATELY UNPINNED, along with [`write_verdict`] below, and the reason is the split above.
/// Both are now pure fs shells: resolve a path, hand the bytes to a tested pure function, hand the
/// result back. A mutation audit still reports `read_verdict -> None` and `write_verdict -> ()` as
/// surviving, and that is honest — nothing asserts the fs call happens. Killing them needs a test
/// that drives `config::data_path`, which resolves through the process-global `FOLIOMAN_CONFIG`; one
/// such test already exists in config.rs and is documented as the only one, because a second would
/// race it. Two untested `std::fs` one-liners is the cheaper failure mode than a racy suite.
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

fn write_verdict(v: Verdict) {
    let path = config::data_path(VERDICT_FILE);
    let years = v.years;
    let j = merge_verdict(std::fs::read_to_string(&path).ok().as_deref(), v);
    let ok = serde_json::to_string(&j).ok().and_then(|s| std::fs::write(&path, s).ok());
    match ok {
        Some(()) => eprintln!(
            "backtest: {years}y method verdict journaled ({} horizons on file) — the screen footer cites the longest",
            j.len()
        ),
        None => eprintln!("WARNING: could not write {VERDICT_FILE} — the screen's method line stays absent/stale"),
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
    let tail = if drift {
        " — ⚠ settings changed since, rerun `folioman backtest universe`"
    } else {
        " (rerun: `folioman backtest universe`)"
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
    // `Arc`, because Samples are BULK-CLONED and a Quote is 69 fields of mostly-heap Strings. The fund
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
enum Hist {
    Raw(Cow<'static, RawValue>),
    Parsed(fetch::Chart),
}

impl Hist {
    /// The deferred parse. `None` = unparseable, or parsed to no bars — which is exactly the pair of
    /// conditions `fetch_history_long` folds into its own `None`, kept here so both paths drop the
    /// same tickers.
    fn parse(self, ticker: &str) -> Option<fetch::Chart> {
        match self {
            Hist::Raw(r) => fetch::parse_chart_raw(&r, ticker).filter(|c| !c.closes.is_empty()),
            Hist::Parsed(c) => Some(c),
        }
    }
}

/// (#1) Cross-sectional peer-group key: the ~6-month bucket a cutoff falls in (2 buckets/year). Names
/// scored in the same half-year are compared against EACH OTHER, so the score is judged on selection
/// skill, not the bull/bear regime every pooled cutoff otherwise shares.
fn bucket(d: chrono::NaiveDate) -> i32 {
    d.year() * 2 + d.month0() as i32 / 6
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
/// with a Šidák-tightened band over 14 factors.
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
/// ONE definition, four call sites (`sweep_fund_factor`, `tune_growth`, `weight_curve`, `report_lane`),
/// because a purge applied at three of four boundaries is a purge nobody can reason about. Only the
/// EARLIER side loses rows: the later side is the evidence being read, and trimming it would shrink the
/// very sample whose independence this is buying.
///
/// `months <= 0` returns `cut` untouched — the default, byte-identical, no allocation, no date maths.
/// Rows must be date-ordered, which both callers' inputs already are (`samples` is built in cutoff order
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
    for s in samples.iter_mut() {
        let k = key(s);
        // A market slice too thin to BE a peer group de-means a name against one or two neighbours, or
        // against itself (relative 0, which reads as "exactly average" and is not a measurement).
        // Falling back to the pooled group keeps the FX contamination but keeps the signal; the receipt
        // records that trade rather than hiding it.
        let g = if by_market {
            fine.get(&(k, s.quote.market.clone())).filter(|(_, n)| *n >= MIN_PEER_GROUP).copied()
        } else {
            None
        };
        let (sum, n) = g.unwrap_or(sums[&k]);
        s.relative = s.realized - sum / n as f64;
    }
}

/// (#93) Smallest market slice `demean` will treat as its own peer group. Below this the de-mean is
/// against too few neighbours to mean anything — at 1 it is a name against itself, which yields a
/// `relative` of exactly 0 and looks like a measured "average" result. Judgement value, not fitted:
/// it matches the `< 4`-family guards elsewhere in this file in spirit, set higher because a peer MEAN
/// over 4 names is still mostly one name.
const MIN_PEER_GROUP: usize = 8;

/// ~6 months between walk-forward cutoffs (trading sessions, ~252/yr). Overlapping forward windows —
/// fine for a rank correlation, not an independent-sample t-test (flagged in the footer).
const STEP_SESSIONS: usize = 126;
/// Need ~3y of history BEFORE a cutoff to form/score the long trend fairly.
const MIN_HISTORY: usize = 750;
/// (Item 9) Conservative round-trip trading cost (bps) charged against the gross edge per unit of
/// turnover. ponytail: a const, not a config knob — the backtest is a dev `cargo run` (already
/// recompiles); promote to config only if a non-dev needs to tune cost without a build.
const ROUND_TRIP_BPS: f64 = 20.0;

/// (#6 survivorship stress) Once-large names that since CRATERED or went bankrupt — the kind the live
/// `universe` (today's index members) silently drops. The `stress` keyword injects these into the pool so
/// the edge is graded against a sample that includes the losers, not just the survivors. Mixed on purpose:
/// crashed-and-alive mega/large-caps with deep Yahoo history (score at many cutoffs, then bleed forward)
/// plus a few bankrupt/failed tickers whose truncated series ends near ZERO (the strongest correction —
/// each is a cutoff with a ~−100% forward window). Any that Yahoo no longer serves return None and are
/// harmlessly skipped, so the list can over-include. ponytail: a hand-picked loser set, NOT point-in-time
/// index reconstruction (a data-vendor problem) — enough to tell "edge is real" from "edge is survivorship".
const STRESS_TICKERS: &[&str] = &[
    // crashed-and-alive, long history, once-large
    "GE", "INTC", "WBA", "T", "KHC", "MMM", "BA", "PARA", "VFC", "GPS", "M", "NWL", "HBI", "FL",
    "C", "AIG", "NOK", "BIIB", "CCL", "NCLH", "F", "WBD",
    // bankrupt / failed -> series ends near 0 (the gold correction; skipped if Yahoo drops them)
    "BBBYQ", "FRCB", "SIVBQ", "SBNY", "WEWKQ",
];

/// Everything the command line says, and nothing it doesn't. Split out of [`run`] purely so it can be
/// tested: the parse used to be an inline `for` loop inside a fn that opens sockets, so NOTHING in the
/// repo exercised it. The mutation audit made that concrete — the arms recognising `universe`, `long`,
/// `fund` and `insider` could each be turned off and every golden stayed green, because the frozen-data
/// pins only ever pass `12`, `20`, `8`, `tune`, `halflife` and `stress`. Silently losing `universe` is
/// the expensive one: the nightly gate's whole job is the WIDE run, and `wide` also gates the verdict
/// journal the screen footer cites.
#[derive(Debug, Default, PartialEq)]
struct Args {
    years: i64,
    wide: bool,
    long: bool,
    fund: bool,
    tune: bool,
    insider: bool,
    halflife: bool,
    stress: bool,
    pit: bool,
    tickers: Vec<String>,
}

/// First purely-numeric arg = holdout years; the keyword `universe` = test the live screen universe
/// (#2: a much wider sample than the ~50-name watchlist -> less single-name luck); `pit` = score each
/// cutoff against the index as it stood that day; everything else = explicit tickers to test.
fn parse_args(args: &[String]) -> Args {
    let mut a_ = Args { years: 5, ..Default::default() };
    for a in args {
        match a.parse::<i64>() {
            Ok(y) if a_.tickers.is_empty() && y > 0 => a_.years = y,
            _ if a.eq_ignore_ascii_case("universe") => a_.wide = true,
            _ if a.eq_ignore_ascii_case("long") => a_.long = true,
            _ if a.eq_ignore_ascii_case("fund") => a_.fund = true,
            _ if a.eq_ignore_ascii_case("tune") => a_.tune = true,
            _ if a.eq_ignore_ascii_case("insider") => a_.insider = true, // (Item 4) also pull SEC Form-4 net buys
            _ if a.eq_ignore_ascii_case("halflife") => a_.halflife = true, // (Item 11) hold-period net-edge sweep
            _ if a.eq_ignore_ascii_case("stress") => a_.stress = true,   // (#6) inject crashed/delisted losers
            _ if a.eq_ignore_ascii_case("pit") => a_.pit = true, // (PIT) score each cutoff against the index AS IT WAS
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

/// Does this run get to overwrite `.backtest_verdict.json`, which the screen's method footer quotes?
///
/// Both halves survived the audit and both are real. `&&`->`||` lets a plain watchlist run
/// (`backtest 12 AAPL`) publish a verdict over the nightly wide one. `>=`->`<` inverts the floor, so
/// the ONLY runs that publish are the thin ones the floor exists to reject — a Yahoo-throttled wide
/// run resolving a few hundred names instead of ~4900 is indistinguishable from a healthy one in the
/// file, and the footer goes on citing it for days.
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

/// (#117) The walk's three bar-counted parameters, resolved PER SERIES rather than once per run.
///
/// `cadence`, `min_history` and `step` are counts of BARS, and `run` fixes them at 12/36/6 for the
/// whole monthly path on the assumption that a bar is a month. That assumption is false for most of
/// the universe: `fetch::chart_json_long` asks Yahoo for `interval=1mo` and Yahoo serves a thin line
/// at whatever granularity it pleases, while nothing anywhere reads `meta.dataGranularity`. A census
/// of the live long cache found 1818 `1mo` against 2059 `1wk`, 1155 `1d`, 317 `1h`, 304 `3mo` and
/// 172 `1y` — 31% monthly. The fixture carries the same mix (58 of 200), which is why no golden has
/// ever noticed.
///
/// `step` is where it bites hardest: 6 bars is 6 SESSIONS on a daily series, so that name emits a
/// cutoff every week and a half and enters the sample with roughly 40x the weight of a monthly one —
/// sample weighting decided by which granularity a feed happened to return. `cadence` is the other
/// end, annualising `volatility_pct` (which the LIVE `sharpe_cap` and `sharpe_cap_etf` read) and
/// sizing `long_ma`.
///
/// BARS/YEAR IS MEASURED AS span/count, NOT as the median gap between bars, and the difference is the
/// whole correctness of the daily arm: consecutive daily bars are one day apart, so a median-gap
/// estimator reads 365 bars a year for a series that has 252. Dividing the record's calendar span by
/// the number of steps in it counts the weekends and holidays out for free.
///
/// `calendar == false` returns `fallback` untouched — that is what keeps every golden byte-identical
/// while the knob ships off. It is also NEAR-INERT on real data when on, which is the property worth
/// checking rather than trusting: a genuine monthly record measures 12.00 bars/yr and resolves to
/// exactly today's 12/36/6, and a genuine daily one measures 252.0 and resolves to 252/756/126
/// against today's 252/750/126.
///
/// Pure, and deliberately so: `run` is a 900-line `-> ()` with no `#[mutants::skip]`, so logic left
/// inline there is graded only through a golden — and a knob shipping OFF moves no golden, which
/// would leave every mutant in this arm alive. Here the `--lib` tests below reach it directly.
fn walk_params(
    dates: &[chrono::NaiveDate],
    calendar: bool,
    fallback: (usize, usize, usize),
) -> (usize, usize, usize) {
    if !calendar || dates.len() < 2 {
        return fallback;
    }
    let span_days = (dates[dates.len() - 1] - dates[0]).num_days();
    if span_days <= 0 {
        return fallback; // a zero-span or reversed record measures nothing; keep the run's constants
    }
    let per_year = (dates.len() - 1) as f64 * 365.25 / span_days as f64;
    let bars = |years: f64| ((per_year * years).round() as usize).max(1);
    (bars(1.0), bars(3.0), bars(0.5))
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
/// 36-bar warmup and 6-bar step over the cached series — an APPROXIMATION of the walk, not the walk
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
/// bias runs, and it is not small: `docs/heuristic-v2-spec.md` builds its case on Bessembinder's
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

/// (PIT) `pit` is the third refusal, and it is not about sample size. A point-in-time run answers a
/// DIFFERENT question than the screen footer asks — the footer quotes this journal to say what the
/// live method does on the live universe, and a PIT verdict is a lower number measured on a pool the
/// screen never ranks. Journaling one would silently restate the footer's claim on a basis nothing in
/// the footer mentions, which is worse than a stale number: it is a number that means something else.
fn may_write_verdict(wide: bool, pit: bool, resolved: usize) -> bool {
    wide && !pit && resolved >= MIN_VERDICT_TICKERS
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

    let Args { years, wide, long, fund, tune, insider, halflife, stress, pit, mut tickers } = parse_args(&args);
    // DELIBERATELY UNPINNED (mutation audit, round 3): both halves of this `&&` survive. It guards an
    // eprintln and nothing else — no branch below reads the result — so the worst a wrong spelling does
    // is print, or fail to print, one advisory line. Killing it means mutating the process environment
    // from a test, which is global state shared with every other test in this binary; `config.rs` has
    // the one such test and documents itself as the only one for that reason. Not worth a racy suite.
    if fund && std::env::var("FMP_API_KEY").ok().filter(|k| !k.is_empty()).is_none() {
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
    let step = if monthly { 6 } else { STEP_SESSIONS }; // ~6 months between cutoffs
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
        let universe =
            fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, false, &[]).await;
        (tickers, etf_set, sector_of) = universe;
    } else if tickers.is_empty() {
        tickers = settings.tickers.clone();
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
        hold_period_sweep(&client, &settings.urls, &tickers, monthly, cadence, min_history, step, tuning, &etf_set, &sector_of, &anchor_windows)
            .await;
        fetch::long_cache_save(); // this path fetches too — flush before the early return
        return;
    }

    // (#3) per ticker, score at many cutoffs and pair each with its YEARS-forward realized return.
    //
    // FETCH FIRST, WALK SECOND. The two halves want opposite things: the fetch is network-bound and
    // `buffer_unordered` is exactly right for it, the walk is pure CPU — and a `buffer_unordered`
    // stream is polled by ONE task, so until this split the whole walk ran on one thread of eight.
    // ORDER IS PRESERVED, which is the part that matters: `fetched` comes out in completion order
    // exactly as `per_ticker` used to, and rayon's `collect` keeps its input order, so the flatten
    // below sees the same sequence — the `sort_by_key(date)` after it is STABLE and inherits this
    // order for same-date ties, which `bootstrap_edge_ci`'s pools then depend on. (Live, completion
    // order now turns on fetch latency alone instead of fetch+walk; both are network-dependent and
    // neither is pinned. The pinned offline path resolves every future without yielding, so
    // completion order there is input order, before and after.)
    let fetched: Vec<_> = stream::iter(tickers.iter())
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
                // ponytail: one uncached FX chart pair per foreign ticker (~2 extra fetches each). Share a
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
                let insider_txns = if insider { fetch::fetch_insider_history(client, urls, tk).await } else { None };
                Some((tk, hist, fund_rows, fx, insider_txns))
            }
        })
        .buffer_unordered(fetch::fetch_concurrency())
        .collect()
        .await;
    // every network read this command makes is done by here, so one flush covers all three remaining
    // exits below. The monthly payloads are the expensive part of a wide run and `screen` shares the
    // same file, so whichever ran first pays and the other reads free for a week.
    fetch::long_cache_save();

    // (PIT) the names the pool asked for and Yahoo could not answer for — counted HERE, while the
    // fetch tickets still remember which ticker they were, and printed in the caveats below. `fetched`
    // is about to be consumed by the walk, and a `None` in it carries no ticker at all.
    let pit_missing = {
        let served: HashSet<&str> = fetched.iter().flatten().map(|(tk, ..)| tk.as_str()).collect();
        pit_unserved(&tickers, &pit_spans, &served)
    };

    let etf_set = &etf_set;
    let sector_of = &sector_of;
    let pit_spans = &pit_spans;
    let factor = settings.buy_heuristic.growth_fund_factor.as_str(); // (G) config-selected as-of factor
    let per_ticker: Vec<Vec<Sample>> = fetched
        .into_par_iter()
        .flatten()
        .map(|(tk, hist, fund_rows, fx, insider_txns)| {
            // the deferred parse, now on all 8 threads. `None` (bad payload, or no bars) contributes an
            // empty Vec, which the `flatten` below drops — the same nothing the fetch-side `?` used to
            // contribute by dropping the ticket before it got here.
            let chart = match hist.parse(tk) {
                Some(c) => c,
                None => return Vec::new(),
            };
            // (#46) the ASSET-CLASS fields, carried out of the same response the closes came from (no
            // extra request). `backtest_quote` builds from `Quote::stub`, which leaves `name` as the
            // TICKER and `instrument_type` empty — and `quote_is_etf` reads exactly those two, so
            // before this every fund in the pool classified as a single stock. That silently merged
            // ~4300 ETFs into the ~500-name stock peer-mean `demean` splits by, and no-op'd every
            // ETF-scoped gate (the physical-gold/ETC bar among them). Yahoo's own `instrumentType`
            // tag leads because an ETF shortName often carries no "ETF"/"UCITS" marker at all.
            let (dates, closes) = (chart.dates, chart.closes);
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
            // (PIT) this name's index spans, resolved ONCE per ticker rather than per cutoff. `None`
            // means "not an index name at all" — every coin, every ETF, and any stock that was never
            // an S&P 500 member — and is never filtered: PIT is a claim about the index pond alone.
            let member_spans = pit_spans.get(tk.as_str()).map(Vec::as_slice);
            let mut out = Vec::new();
            // (#117) per-SERIES walk parameters. Off by default, in which case this is exactly the
            // run-wide triple that arrived. See `walk_params`.
            let (cadence, min_history, step) =
                walk_params(&dates, tuning.backtest_calendar_cadence, (cadence, min_history, step));
            let mut i = min_history;
            // DELIBERATELY UNPINNED (mutation audit, round 4): `<`->`<=` survives here, and killing
            // it needs a fixture whose walk lands exactly on `dates.len()` — a golden-data change,
            // not a test. The mutant is also self-limiting: `dates[i]` on the next line panics
            // immediately rather than returning a wrong number.
            while i < dates.len() {
                // (PIT) was this name IN the index on the day we are pretending to stand? If not, the
                // cutoff never existed for a real screener and contributes no sample — that is the whole
                // point-in-time correction, and it cuts BOTH ways: a future member is not scored early,
                // and a departed one stops being scored at its exit rather than at its delisting.
                if member_spans.is_some_and(|sp| !core::sp500_member_at(sp, dates[i])) {
                    i += step;
                    continue;
                }
                // forward index: first session at least `years` past the as-of date
                let target = dates[i] + chrono::Duration::days(years * 365);
                match dates[i..].iter().position(|d| *d >= target) {
                    Some(off) => {
                        let fwd = i + off;
                        // record EVERY cutoff with a forward window (not just gated ones) so the
                        // peer-mean spans the whole period universe; each lane filters by its own gates.
                        let realized = (closes[fwd] / closes[i] - 1.0) * 100.0;
                        if !realized.is_finite() {
                            // zero/garbage close -> ±inf poisons the demeaned bucket; skip the cutoff.
                            // DELIBERATELY UNPINNED: both mutants of this `+=` survive, because every
                            // fixture close is finite and positive (`trailing_returns` documents the
                            // same property) so this branch is never taken offline. Reaching it means
                            // seeding a zero close into a committed golden, which would move six
                            // reports to grade one increment. The identical `+=` on the main path
                            // below IS caught.
                            i += step;
                            continue;
                        }
                        let mut quote = core::backtest_quote(tk, &dates, &closes, &divs, i, cadence, &anchor_windows);
                        stamp_asset_class(&mut quote, &cls_name, &cls_type, etf_set, sector_of);
                        // NOT `years`. `years` is the FORWARD hold horizon; `fund_factors` spends its
                        // third argument as the BACKWARD fundamental lookback (core.rs, `long_ago =
                        // fund_as_of(rows, cutoff - yrs*365)`). Passing the horizon meant `fund 12`
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
                            // (PEG) 1/PEG = earnings_yield · as-of CAGR. This one IS shipped live now
                            // (growth_fund_factor "peg_yield"), and the live enrich converts the same way —
                            // so train and serve compute the identical ratio instead of differing by an FX rate.
                            //
                            // (#37) the CAGR is `long_cagr_pct` — the score's own, honouring use_life_cagr /
                            // use_trend_cagr / fixed_cagr_years — not the raw `quote.trend_cagr` this read
                            // until 2026-07-27. backtest_quote fills `perf`, `life_cagr` AND `trend_cagr` at
                            // this cutoff, so every arm of that switch is reconstructable as-of and
                            // train==serve still holds. Keep this in lockstep with fetch.rs's enrich.
                            f.peg_yield = px.and_then(|p| core::peg_yield(f.eps_ttm, picks::long_cagr_pct(&quote, tuning), p));
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
                i += step;
            }
            out
        })
        .collect();

    let mut samples: Vec<Sample> = per_ticker.into_iter().flatten().collect();
    if too_few_samples(samples.len()) {
        println!(
            "backtest: only {} cutoffs had a full {years}y forward window — too few to correlate.",
            samples.len()
        );
        return;
    }
    samples.sort_by_key(|s| s.date); // chronological -> the OOS split is early-vs-late in time

    // `tune`: honest out-of-sample selection. Search the growth weights on an EARLY train split and
    // report the winner on a LATE test split it never saw — the only way to a trustworthy number when
    // the shipped knobs were hand-tuned on all the data. Does its own per-split de-mean, so branch here
    // BEFORE the whole-sample de-mean below.
    if tune {
        tune_growth(&samples, tuning);
        return;
    }

    // (#1) de-mean realized return WITHIN each ~6-month cutoff bucket AND asset class. Pooling raw returns across cutoffs
    // that span different regimes makes the score race CALENDAR LUCK (a 2016 cutoff that mooned vs a
    // 2021-top cutoff that crashed), not stock-picking. Subtracting the bucket's peer mean leaves only
    // "did this name beat the others scored the same half-year" = the selection signal we actually want.
    demean(&mut samples);
    // invariant: per-bucket de-meaning makes the relatives sum to ~0 (each bucket nets out). Fails loudly
    // if the bucket map and the fill ever drift apart. Cheap, runs in release.
    let rel_sum: f64 = samples.iter().map(|s| s.relative).sum();
    assert!(rel_sum.abs() < 1e-3 * samples.len() as f64, "de-mean broken: relatives sum to {rel_sum}");

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
    let gate_loosen: Vec<Knob> = vec![
        knob("growth_min_range_pct -10", |t| t.growth_min_range_pct -= 10.0),
        knob("growth_min_cagr -4", |t| t.growth_min_cagr -= 4.0),
        knob("growth_min_1y_pct -10", |t| t.growth_min_1y_pct -= 10.0), // restored with the knob: round 5 measured this exact row at n=284 / -108.1 pts fwd and reverted the loosened floor. Re-measured here on the current sample instead of quoted — a POSITIVE flip is the only thing that reopens the question
        knob("max_1m_drop_pct -10 (deeper)", |t| t.max_1m_drop_pct -= 10.0),
        knob("min_avg_turnover_eur ->0", |t| t.min_avg_turnover_eur = 0.0),
        knob("growth_max_above_ma ->off", |t| t.growth_max_above_ma = 0.0), // (#24) fwd return of the extreme-stretch names the gate excludes — validated -125.1 (n=267) at ship time; a POSITIVE flip here says re-probe the ceiling
        knob("growth_require_lifetime_uptrend ->off", |t| t.growth_require_lifetime_uptrend = false), // (#25) fwd return of the lifetime-downtrend names the gate excludes; n=0 while the gate is off
        knob("growth_maxdd_cap ->off", |t| t.growth_maxdd_cap = 0.0), // (#26) fwd return of the deep-drawdown names the gate excludes; n=0 while the gate is off
        // READ THIS ROW BACKWARDS FROM ITS NEIGHBOURS. Every other row REMOVES a gate and prices the
        // cohort that gate had been excluding. This one ADDS a rung to `long_leg`'s ladder, so the
        // cohort it admits was never gated at all — those names had no long CAGR, so they were
        // unscorable (the `history` reason, 1949 of 4748 EU-buyable names live). Same arithmetic, but
        // "newly admitted" here means "newly MEASURABLE", not "newly forgiven".
        knob("growth_min_leg_years ->2 (admits the 2Y rung)", |t| t.growth_min_leg_years = 2.0),
        knob("growth_max_peg ->off", |t| t.growth_max_peg = 0.0), // (#37) fwd return of the names the valuation ceiling excludes — the ceiling's own keep. The ci-settings curve (1.5..4.0) came from six hand-edited configs; this prices the on/off question every run, which is the part that sweep found decisive
    ];
    report_lane("ON-SALE (buy_score)", &samples, buy_score, tuning, &buy_knobs, years);
    report_lane("GROWTH (growth_score)", &samples, growth_score, tuning, &growth_knobs, years);
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
        // 0 first because it is the SHIPPED state (#3h) — uncapped. 50 reproduces the recorded 30->50 pair
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
    // `picks::rank_picks`/`growth_picks` (the LIVE screen path), not inside `growth_score`, which is
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
    let accepted_med = gate_audit(&samples, growth_score, tuning).map(|(_, _, amed)| amed); // (#9) are the growth lane's hard gates actually selecting winners?
    gate_sweep(&samples, tuning, &gate_loosen, accepted_med); // (#10) which specific gate is too tight?
    exit_probe(&samples, growth_score, tuning); // (Item 31) is a mid-hold gate FAILURE a measured sell signal?
    if fund_lane_on(fund, insider) {
        report_fund_lane(&samples, tuning.split_purge_months);
        sweep_fund_factor(&samples, tuning, years); // (G) which factor pays THROUGH the growth lane, held-out
    }
    report_risk_lane(&samples, tuning.split_purge_months); // closes-derived risk stats, standalone — no fundamentals needed
    // (#40) the ABSOLUTE goal metric: do the top-N picks beat an S&P500 buy-and-hold? One index fetch
    // (survivorship-clean), matched to the sample cadence. realized is untouched by de-mean.
    // (Phase E) with use_adjusted_close on, the picks' realized returns include dividends, so the fair
    // benchmark is the S&P 500 TOTAL-return index (^SP500TR, Yahoo history from 1988) — TR vs TR;
    // default (raw close) keeps the price-only ^GSPC, unchanged.
    let bench_sym = if crate::config::use_adjusted_close() { "^SP500TR" } else { "^GSPC" };
    let bench = if monthly {
        fetch::fetch_history_long(&client, &settings.urls, bench_sym).await
    } else {
        fetch::fetch_history(&client, &settings.urls, bench_sym).await
    }
    // (FX) an index LEVEL, never joined to a filing — nothing to convert, so drop the currency. Nor is
    // it ever scored, so it needs no asset-class stamp either: prices only.
    .map(|c| (c.dates, c.closes))
    .unwrap_or_default();
    report_vs_benchmark(&samples, &bench, years, tuning);
    // (r40) relative strength vs the index — needs the benchmark, so it lives here, after the fetch.
    report_relative_strength(&samples, &bench, tuning.split_purge_months);
    // (round 108) the WHEN dimension: does the market's state at entry predict the held book?
    let verdict = report_entry_state(&samples, &bench, years, tuning);
    // (round 27) journal the unconditional method verdict — but ONLY from a wide (`universe`) run:
    // the watchlist's ~50-survivor sample is not the method's proof, and must never overwrite it.
    // The screen's method footer reads this file back.
    //
    // `wide` alone was not enough. A wide run that Yahoo THROTTLED resolves a few hundred names
    // instead of ~4900, and that thin sample overwrote the journal just as surely as a watchlist run
    // would have — the same hazard the comment above warns about, reached by a different route. Hold
    // it to the same ≥500 floor `backtest_edge_holds` uses to decide its own sample is trustworthy.
    if may_write_verdict(wide, pit, tickers.len()) {
        if let Some((book, excess, win, worst, oos_early, oos_late, windows)) = verdict {
            write_verdict(Verdict {
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
            });
        }
    } else if wide {
        // say so — a silent non-write reads as "the journal is broken", not "this sample was too thin"
        eprintln!("{}", no_verdict_reason(pit, tickers.len()));
    }
    // (round 112) the DIVERSIFICATION dimension: does de-correlating the held book beat plain rank order?
    report_corr_cap(&samples, &bench, years, tuning);
    if fund_lane_on(fund, insider) {
        // (#44 Phase C) grade the FREE fundamental factors on the ABSOLUTE held-book, not peer-relative.
        report_book_by_factor(&samples, &bench, years, tuning);
        // (round 106) the no-borrow structural lever: growth book + value book held side by side.
        report_two_style_book(&samples, &bench, years, tuning);
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
#[allow(clippy::too_many_arguments)]
fn sweep_cutoffs(
    tk: &str,
    dates: &[chrono::NaiveDate],
    closes: &[f64],
    divs: &[(chrono::NaiveDate, f64)],
    name: &str,
    instrument_type: &str,
    holds: &[i64],
    min_history: usize,
    step: usize,
    cadence: usize,
    calendar: bool, // (#117) resolve the three above per SERIES; false = use them as handed in
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    windows: &BTreeMap<String, i64>, // (#88) the live anchor map, or empty for the old default-only behaviour
) -> Vec<(i64, Sample)> {
    // (#117) same resolution as the validated walk, off the same helper — the sweep answers "which
    // hold length pays best", and it could not answer it honestly while a daily-granularity name
    // contributed ~40x the cutoffs of a monthly one. Off by default, and then this is a no-op.
    let (cadence, min_history, step) = walk_params(dates, calendar, (cadence, min_history, step));
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
        let mut i = min_history;
        while i < dates.len() {
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
            i += step;
        }
    }
    out
}

/// (Item 11) Hold-period / signal half-life sweep. Re-runs the price-only walk-forward over several
/// forward windows from the SAME fetched history (fetched once per ticker, then sliced per window), and
/// prints each window's gross edge, turnover, and NET edge (gross − turnover×ROUND_TRIP_BPS). The right
/// rebalance cadence is the one that maximises NET — longer holds pay less turnover but may catch less
/// signal, so there's an optimum. ponytail: duplicates ~20 lines of run's walk-forward on purpose, to keep
/// this opt-in dev path from touching the validated default dispatch; the fetch is cached so the re-walk is
/// cheap. Price-only (no fund/insider) — this measures the price signal's decay, not the fund tilt.
#[allow(clippy::too_many_arguments)]
async fn hold_period_sweep(
    client: &reqwest::Client,
    urls: &crate::config::Urls,
    tickers: &[String],
    monthly: bool,
    cadence: usize,
    min_history: usize,
    step: usize,
    tuning: &BuyHeuristic,
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
    windows: &BTreeMap<String, i64>, // (#88) resolved once in `run`, so both quote-building paths agree
) {
    const HOLDS: [i64; 6] = [1, 2, 3, 5, 8, 10]; // forward windows (years) to compare
    eprintln!("backtest: hold-period sweep over {HOLDS:?}y windows ({} tickers)…", tickers.len());
    // FETCH FIRST, WALK SECOND, for the same reason the validated path does it (see the long note
    // there): this stream is polled by ONE task, so leaving `sweep_cutoffs` inside it ran six forward
    // windows per ticker on one thread of eight. `buffer_unordered` yields completion order and rayon's
    // `collect` preserves its input order, so `all` below sees the sequence it always did — and the
    // halflife golden is what verifies that, since `demean` and the pooled stats read that order.
    let fetched: Vec<_> = stream::iter(tickers.iter())
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
                min_history, step, cadence, tuning.backtest_calendar_cadence, etf_set, sector_of, windows,
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
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
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
        .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap())
        .map(|(name, ..)| *name)
}

/// (G) Auto-select `growth_fund_factor`: for each candidate as-of factor, re-derive every sample's
/// fund_factor IN MEMORY (no refetch — Sample.fund is already cached), search growth_fund_weight on the
/// EARLY train half, and report the factor's edge on the LATE test half it never saw + its two OOS sub-
/// halves. This judges each factor THROUGH the growth lane (report_fund_lane only probes them standalone),
/// then prints the one to paste into settings.yaml. Ships nothing. Needs the `fund` path; with <8 cutoffs
/// carrying fundamentals there's nothing to sweep. Same chronological split + seeded search as `tune`.
/// (Item 10) The best-of-N haircut's two tail percentiles: a 90% band tightened to 5/N a side, so a
/// winner picked as the MAX of `n` tried factors is charged for having been selected.
///
/// Extracted from the call site because the call site is inside `sweep_fund_factor`, a `-> ()` printer
/// the mutation gate cannot run at all — the fund lane needs a live `FMP_API_KEY` and every golden runs
/// offline (tests/backtest_fixture.rs:134). Inline, the six operator mutants on this arithmetic were
/// unkillable by construction; as a pure fn they are just tested.
fn sidak_tail(n: usize) -> (f64, f64) {
    let side = 5.0 / n.max(1) as f64;
    (side, 100.0 - side)
}

const FUND_FACTORS: [&str; 24] = [
    "rev_cagr", "rev_accel", "gross_margin", "op_margin", "margin_trend", "eps_growth",
    // the printed columns (REV-YoY / EPS-YoY / NET%), swept for the first time. Widening this
    // array TIGHTENS every reported band: the Šidák haircut below divides by FUND_FACTORS.len(), so
    // going 11 -> 14 -> 24 makes the best-of-N test stricter, not the factors weaker. A later
    // reader comparing bands across runs must check this length before calling it a regression.
    "rev_yoy", "eps_yoy", "net_margin",
    "insider_net_buys_90d", // (Item 4) shows n/a unless `insider` populated it
    "earnings_yield",       // (Item 19) as-of valuation; native-currency probe (n/a unless `fund`)
    // (PEG) growth-at-price = earnings_yield · as-of CAGR (1/PEG; 100 ⇔ PEG 1, >100 ⇔ PEG <1).
    // Swept HERE, not just in the held-book list, because this is the sweep that carries the
    // Šidák best-of-N haircut — the axis was annotated "expected dead" for rounds without ever
    // being measured. NOTE the scale: peg_yield runs ~0-500 where earnings_yield runs ~0-15, so
    // the default `growth_fund_cap: 30` clamps every PEG < 3.3 name to the same value and would
    // fake a dead result. Raise the cap (~300) for any run that reads this row.
    "peg_yield",
    "buyback_yield",        // as-of 1y share-count shrink (+ = buying back); n/a unless `fund` + shares in the rows
    // (#157) the ten factors `core::select_fund_factor` has always registered and computed but
    // this array never asked about. They were reachable from every other read site — the
    // held-book-by-FACTOR table, the SURVIVAL-GATE probe, `growth_fund_weight` — and absent ONLY
    // here, which is the one place that applies the Šidák best-of-N haircut. So the four survival
    // factors had a percentile-cut reading from (#124) and no honest multiple-testing band, and
    // the other six had neither. Read the n/a rows as a result: they record which factors the
    // point-in-time pool cannot populate, which is a fact about coverage, not about the factor.
    "roe", "quality", "roic", "ebitda_yield",
    "fcf_margin", "interest_cover", "net_cash_rev", "margin_stability",
    "accrual_gap", "asset_growth",
    "composite",            // (Item 3) shows n/a until ≥2 factors are present
];

/// (#119) UNGRADEABLE BY THE MUTATION GATE — the fund lane needs a live `FMP_API_KEY`, and every
/// golden runs offline by design (tests/backtest_fixture.rs:134), so no test in `--lib --test
/// backtest_fixture` executes one line of this. `replace sweep_fund_factor with ()` is unkillable
/// there, and skipping says so instead of arming the gate against the next edit.
///
/// The arithmetic that WAS gradeable moved out rather than being skipped with it: `sidak_tail` and
/// `bootstrap_block` are pure and tested. What stays here is the printing.
#[mutants::skip]
fn sweep_fund_factor(samples: &[Sample], default: &BuyHeuristic, years: i64) {
    if samples.iter().filter(|s| s.fund.is_some()).count() < 8 {
        return; // no as-of fundamentals (no FMP key / cold cache) -> report_fund_lane already said so
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
        for _ in 0..200 {
            let mut t = default.clone();
            t.growth_fund_weight = next() * 0.5;
            let (rho, edge) = lane_metrics(train, growth_score, &t);
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
    let results: Vec<(&str, f64, Option<f64>, Option<f64>)> =
        FUND_FACTORS.iter().map(|&n| { let (e, a, b, _) = eval(Some(n)); (n, e, a, b) }).collect();

    let fmt = |r: Option<f64>| r.map_or("n/a".to_string(), |v| format!("{v:+.2}"));
    println!("\n── FUND FACTOR SWEEP (growth_fund_weight searched per factor, held-out TEST) ──");
    println!("  price-only baseline TEST edge {baseline:+.1} — a factor must beat this with both OOS halves +");
    for (name, edge, a, b) in &results {
        println!("  {name:<14} TEST edge {edge:+.1}  OOS {} | {}", fmt(*a), fmt(*b));
    }
    match pick_sweep_winner(&results, baseline) {
        Some(w) => {
            println!("  -> WINNER: {w}. Set `growth_fund_factor: {w}` + a non-zero `growth_fund_weight`, then `backtest universe tune` to confirm.");
            // (Item 10) best-of-N haircut: the winner is the MAX of N tried factors, so its edge is inflated
            // by selection. Re-bootstrap the winner's tilt but read a Šidák-tightened tail (5/N instead of
            // 5) — if THAT band straddles 0 the "win" is within best-of-N luck, ship nothing anyway.
            let weight = eval(Some(w)).3; // seeded search -> identical to the sweep's pick; re-derive the won weight
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
            let (lo_p, hi_p) = sidak_tail(FUND_FACTORS.len());
            let block = bootstrap_block(years, &tun);
            if let Some((lo, hi)) = bootstrap_edge_ci(&s, growth_score, &tun, 1000, lo_p, hi_p, block) {
                let verdict = if lo > 0.0 {
                    "survives multiple testing -> trust the WINNER"
                } else {
                    "within best-of-N luck -> SHIP NOTHING despite the raw winner"
                };
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
        ladder.sort_by(|a, b| a.partial_cmp(b).unwrap());
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

/// Cumulative % return of a benchmark series held `years` from the first session on/after `from`. `None`
/// if `from` predates the series or no full forward window remains — mirrors the ticker walk (line ~208).
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

/// (#40) ABSOLUTE goal metric — the one the peer-relative lanes never measure. The stated purpose is
/// "out-return an S&P500 buy-and-hold", but every lane above de-means the level away (SELECTION, not the
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

/// index). This asks the real question: buy the top-N growth picks (equal-weight, non-crypto), hold
/// `years`, and does that beat holding ^GSPC over the SAME window? Per pick, excess = its annualized
/// return minus what ^GSPC did from the same cutoff. Top-N per ~6mo bucket; report mean pick/SPY CAGR,
/// excess, win-rate, worst bucket, and the early-vs-late OOS split. Read the STRESS run: the picks come
/// from today's survivors (biased UP), ^GSPC is the true index, so a non-stress win is optimistic.
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

fn report_vs_benchmark(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    // name the benchmark honestly: ^SP500TR when the run meters total return (use_adjusted_close).
    let bench_sym = if crate::config::use_adjusted_close() { "^SP500TR total-return" } else { "^GSPC" };
    if bd.len() < 2 {
        println!("\n── vs S&P500 (ABSOLUTE) ──  no {bench_sym} history fetched — skipping the benchmark leg.");
        return;
    }
    // (bucket, score, pick CAGR, SPY CAGR, ticker, as-of peg_yield) for every GATED non-crypto pick that
    // has a benchmark window. The peg rides along ONLY for the (#75) value brake below; nothing else reads it.
    let mut rows: Vec<(i32, f64, f64, f64, String, Option<f64>)> = Vec::new();
    // (#140) trailing returns for the graded `growth_corr_cap` skip, keyed by (bucket, ticker) because
    // a trail is per-SAMPLE, not per-ticker — the same name at two cutoffs carries two histories.
    // A ticker appearing twice inside one ~6mo bucket keeps the later trail; both describe the same
    // window to within the step, and a near-duplicate trail cannot change a correlation verdict.
    //
    // Built UNCONDITIONALLY, and that is deliberate. An `if cap > 0.0` guard here saves a borrowed
    // slice per sample on the shipped lane and costs a mutant nobody can kill: at `cap: 0.0` the map
    // is never read, so `>` `==` `<` `>=` all produce byte-identical output and the gate reds with
    // six survivors. `report_vs_benchmark` returns `()` and only prints, so no unit test can reach
    // the guard either ((#75)). An unobservable branch is not an optimisation, it is dead weight the
    // suite has to pretend to cover — so it is not written.
    let mut trail_of: std::collections::BTreeMap<i32, std::collections::HashMap<&str, &[f64]>> = Default::default();
    // (#144) TWO PASSES, because a rank normalisation is CROSS-SECTIONAL and the pool a name is ranked
    // against must be its OWN cutoff window. Ranking a 1998 name against a 2015 one would be lookahead
    // of the plainest kind, so the grouping is not a convenience — it is the correctness condition.
    // This one loop feeds `by_bucket` and therefore EVERY number the ship rule reads: the TOP_LADDER
    // books, `rank_slice_stats` (rank-1, the h2h GUARD, the disjoint slices) and `book_deciles`.
    //
    // BYTE-IDENTICAL ON THE SHIPPED LANE, and by two separate properties rather than one. At
    // `growth_rank_normalise: 0.0` `growth_scores_ranked` returns `growth_score`'s values verbatim.
    // And the ROW ORDER cannot move either: each bucket's Vec is filled in `samples` order exactly as
    // before, so `value_floor_trim`'s per-bucket Vec is element-for-element what it was — which
    // matters because the rank sort below is STABLE and a reordered tie would silently rebuild a book.
    let mut by_cutoff: std::collections::BTreeMap<i32, Vec<&Sample>> = Default::default();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue; // crypto: a coin isn't an S&P500-comparable hold
        }
        by_cutoff.entry(bucket(s.date)).or_default().push(s);
    }
    for (b, group) in &by_cutoff {
        let quotes: Vec<&Quote> = group.iter().map(|s| s.quote.as_ref()).collect();
        for (s, scored) in group.iter().zip(picks::growth_scores_ranked(&quotes, tuning)) {
            let Some(score) = scored else { continue };
            let Some(bench_r) = benchmark_fwd(bd, bc, s.date, years) else { continue };
            trail_of.entry(*b).or_default().insert(s.quote.ticker.as_str(), s.trail.as_slice());
            // RAW cumulative % (annualize the BOOK, not per-name)
            rows.push((*b, score, s.realized, bench_r, s.quote.ticker.clone(), s.fund.as_ref().and_then(|f| f.peg_yield)));
        }
    }
    if rows.len() < 8 {
        println!("\n── vs S&P500 (ABSOLUTE) ──  only {} gated picks have a ^GSPC window — too few.", rows.len());
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
    // (#41/#43) EQUAL-WEIGHT HELD-BOOK return — the correct metric for a no-sell hold. A held book earns
    // ann(mean of terminal MULTIPLES), NOT mean of per-name CAGRs: a 20× winner in the book covers twenty
    // −100% zeros, and a name that goes to 0 contributes its full weight lost (1/N), not its scary CAGR.
    // Also count "zeros ridden" (names ≤−90% you must hold through) to show the no-sell tail you survive.
    let mut m10: Option<f64> = None; // top-10 mean terminal multiple, feeds the after-tax footer below
    for &n in TOP_LADDER {
        let (mut book, mut spy, mut excess) = (Vec::new(), Vec::new(), Vec::new());
        let (mut zeros, mut held) = (0usize, 0usize);
        let mut zero_names: Vec<String> = Vec::new(); // (#zeros) names ≤−90% at top-10 -> union across horizons = true distinct death count
        let mut multiples: Vec<f64> = Vec::new();
        for (b, v) in &by_bucket {
            let mut vv = v.clone();
            vv.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // score desc
            // (#140) the graded redundancy skip. At the shipped `growth_corr_cap: 0.0` this is the
            // identity and every number below is unchanged; armed, it rebuilds THIS rung from the
            // ranked list, dropping a name that duplicates a bet already in the book and refilling
            // from below. NOTE what it deliberately does NOT touch: `rank_slice_stats` and
            // `book_deciles` below still read the untrimmed `by_bucket`, so the rank-1 h2h GUARD is
            // measured on the same cohort either way and cannot move for this knob — a vacuous pass,
            // and the receipt must say so rather than bank it as evidence.
            let trails: Vec<&[f64]> =
                vv.iter().map(|x| trail_of.get(b).and_then(|m| m.get(x.3.as_str())).copied().unwrap_or(&[])).collect();
            let p = corr_cap_rung(&vv, &trails, n, tuning.growth_corr_cap);
            let take = p.len();
            if take == 0 {
                continue;
            }
            let book_cum = mean(&p.iter().map(|x| 1.0 + x.1 / 100.0).collect::<Vec<_>>()); // equal-weight terminal multiple
            let spy_cum = mean(&p.iter().map(|x| 1.0 + x.2 / 100.0).collect::<Vec<_>>());
            multiples.push(book_cum);
            book.push(ann((book_cum - 1.0) * 100.0, years));
            spy.push(ann((spy_cum - 1.0) * 100.0, years));
            excess.push(*book.last().unwrap() - *spy.last().unwrap());
            zeros += p.iter().filter(|x| x.1 <= -90.0).count();
            if n == 10 {
                zero_names.extend(p.iter().filter(|x| x.1 <= -90.0).map(|x| format!("{}@b{b}({:.0}%)", x.3, x.1)));
            }
            held += take;
        }
        if n == 10 && !zero_names.is_empty() {
            println!("  top-10 zero names ({years}y): {}", zero_names.join(", "));
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
/// unkillable rather than merely unkilled. [`report_vs_benchmark`] returns `()` and only prints, the
/// knob ships at `0.0`, and `fund` — which this brake needs to see a single `peg_yield` — cannot run
/// offline (`tests/backtest_fixture.rs`), so no test could reach the comparisons in place. Split out,
/// they take arguments and answer with a value, which is the whole difference.
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
        vv.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // score desc — the screen's own order
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
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
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
/// rescaling, a sign convention or an outlier cannot move it and the 24 factors become comparable to
/// one another. Full re-reading of (#159)'s table: the (#165) block in tests/ci-settings.yaml.
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

/// (#106) The per-bucket terminal multiples of an equal-weight top-`n` book and of the same bucket's
/// benchmark leg — ONE definition of "what did the book end up worth", so [`book_stats`]'s headline
/// means and [`book_deciles`]'s distribution cannot be computed two slightly different ways. Buckets
/// that yield no pick are skipped, which is why the result is a Vec rather than a map.
fn book_multiples(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>, n: usize) -> Vec<(f64, f64)> {
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let mut out = Vec::new();
    for v in by_bucket.values() {
        let mut vv = v.clone();
        vv.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // rank_key desc
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

/// (#44 Phase C) Grade each FREE as-of fundamental factor on the HELD-BOOK metric: within the
/// growth-gated universe, rank by the factor (not the score), hold the top-N, and compare its held-book
/// excess-vs-S&P500 to ranking by `growth_score`. A factor whose held-book excess BEATS the score with
/// both OOS halves + is a better selector for a 15y no-sell book — a ship candidate for the fund tilt.
/// (round 108) ENTRY-STATE — the WHEN dimension, never measured before this round: every prior lane
/// conditioned on the PICK; this conditions on what the MARKET looked like the day money went in.
/// Buckets are classed by the benchmark's drawdown from its running high at the bucket's first
/// cutoff, then the SAME top-10 growth book is graded per class. Report-only, and the guidance can
/// only ever be "deploy new money faster when a state occurs" — never "wait in cash for it" (the
/// table can't see cash drag, and waiting is the classic market-timing trap).
/// Returns the unconditional "all entries" held-book stats (book, excess, win, worst, oos_early,
/// oos_late, windows) — the same numbers it prints on the `[unconditional]` row — so run() can
/// journal them as the method verdict without recomputing. None when the sample can't form a book.
#[allow(clippy::type_complexity)]
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

/// Only the free SEC/income-statement factors are listed (roe, the round-107 survival levels and — since
/// (#43) — roic are all SEC-computed; `FundRow::roic`, the PREMIUM field, is still never populated and is
/// not what the roic row reads). Runs only under `fund` (else `s.fund` is None everywhere).
///
/// Skipped because `replace report_book_by_factor with ()` is unkillable, not merely unkilled. It
/// returns `()` and its entire effect is `println!`, so a test that called it could assert nothing a
/// stub would fail; and the only mode that gets past the `s.fund` check is `fund`, which needs a live
/// API and is therefore unpinned by design (`tests/backtest_fixture.rs`: the six goldens cover the
/// modes that run OFFLINE). Measured in (#75)'s audit, where a comment correction inside this body was
/// enough to drag it into `--in-diff` scope and red the gate on its own.
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
    ];
    let mut any = false;
    let mut skipped: Vec<String> = Vec::new();
    for (name, get) in factors {
        let mut by: std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>> = Default::default();
        for s in samples {
            if picks::asset_class(&s.quote) == 0 || growth_score(&s.quote, tuning).is_none() {
                continue;
            }
            let Some(fv) = s.fund.as_ref().and_then(get) else { continue };
            let Some(br) = benchmark_fwd(bd, bc, s.date, years) else { continue };
            by.entry(bucket(s.date)).or_default().push((fv, s.realized, br));
        }
        let rows: usize = by.values().map(|v| v.len()).sum();
        if let Some((b, _, e, w, wo, el, la)) = book_stats(&by, n, years) {
            any = true;
            println!("  {name:<14} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (n={rows})");
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
        // coverage at all the line above already says so, and 18 identical "n=0" rows say nothing.
        println!("  no book: {} — listed, NOT scored (a shipped tilt in here is ungradeable, not fine)", skipped.join(", "));
    }
    println!("  (a factor beating growth_score's held-book excess with OOS both + is a better held-book selector -> ship it.");
    println!("   every row here is SEC-computed and free — roe, the round-107 survival levels and (#43) roic,");
    println!("   which is DERIVED (EBIT ÷ equity+net debt), not the premium `FundRow::roic` that never populates.)");

    // BLEND sweep: pure-value beat pure-score standalone — but pure-value alone risks value-traps the
    // gates miss, so find the growth_fund_weight KNEE where tilting growth_score toward the baked
    // fund_factor (= growth_fund_factor, `earnings_yield` on the SEC feed) peaks the held book. Only
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
                vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
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
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
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
    by_score.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let mut picked: Vec<&(String, f64, Option<f64>, f64, f64)> = by_score.into_iter().take(g).collect();
    let mut by_ey: Vec<&(String, f64, Option<f64>, f64, f64)> = rows.iter().filter(|r| r.2.is_some()).collect();
    by_ey.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
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
        let parts: Vec<String> =
            vexcess.chunks((vexcess.len() / 4).max(1)).take(4).map(|c| format!("{:+.1}", mean(c))).collect();
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
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
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
    rels.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (lo, hi) = (percentile(&rels, 1.0), percentile(&rels, 99.0));
    // (score, clamped-relative), sorted by score desc -> same half split as edge_halves.
    let mut v: Vec<(f64, f64)> = pairs.iter().map(|x| (x.1, x.0.relative.clamp(lo, hi))).collect();
    v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
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
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
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
    if scored.len() < 4 {
        return (None, 0.0);
    }
    let sc: Vec<f64> = scored.iter().map(|(_, v)| *v).collect();
    let rels: Vec<f64> = scored.iter().map(|(s, _)| s.relative).collect();
    let (t, b) = edge_halves(&scored);
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
        // uniform draw would misread — 0.1 crushes every name, 0.0 crushes none. The shipped state IS 0, so
        // `dim_active` sees hi=60 change nothing when no sample tops 60%/yr and reports the dim inert; a
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

/// Report one lane: filter the samples to the cutoffs this lane's gates admit, score them, and print
/// the peer-relative Spearman, the top/bottom-half edge, the out-of-sample (early-vs-late) split, and
/// the per-term ablation. `samples` must already be in date order (for the OOS split). Mutating a
/// score WEIGHT never changes a GATE, so the gated row set stays fixed across the ablation -> the rho
/// is comparable term-to-term.
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

/// (Item 5) p-th percentile of an already-sorted slice (nearest-rank). NaN on empty.
/// (#107) `pub(crate)` so `picks::rank_robustness` reads its quartiles off THIS definition rather
/// than growing a second nearest-rank rule that rounds the other way on an even sample.
pub(crate) fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
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
/// (Item 10) the percentiles are caller-set so the fund sweep can pass a Šidák-tightened tail (5/N) to
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
    let mut buckets: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        buckets.entry(bucket(s.date)).or_default().push(i);
    }
    let keys: Vec<i32> = buckets.keys().copied().collect();
    if keys.len() < 4 {
        return None;
    }
    let block = block.clamp(1, keys.len());
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
    edges.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some((percentile(&edges, lo_p), percentile(&edges, hi_p)))
}

/// (Item 9) Per-rebalance turnover fraction of the lane's HELD set: 1 − mean Jaccard of the top-half
/// tickers between consecutive ~6mo buckets. 1.0 = the whole book is replaced each period (max cost),
/// 0.0 = the same names are held (free). <2 measurable buckets -> 0.0 (can't measure; charge nothing).
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
            rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            rows[..rows.len() / 2].iter().map(|(t, _)| *t).collect::<HashSet<&str>>()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if tops.len() < 2 {
        return 0.0;
    }
    let pairs = tops.len() - 1;
    let jac_sum: f64 = tops
        .windows(2)
        .map(|w| {
            let u = w[0].union(&w[1]).count();
            if u == 0 { 1.0 } else { w[0].intersection(&w[1]).count() as f64 / u as f64 }
        })
        .sum();
    1.0 - jac_sum / pairs as f64
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
    /// sits behind `print_recall_capture`, which ships false, so a predicate left there is unkillable
    /// by the mutation gate. Same rule as the `rows.is_empty()` guard below.
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
    // `if tuning.print_recall_capture`, which ships false, so no golden ever executes it and any
    // branch left there is unkillable by the mutation gate. Guards belong where a test can reach them.
    // `pairs` is allowed to be empty — "nothing failed exactly two gates" is a real and publishable
    // answer, and it refuses the arm rather than voiding the table. Only `rows` gates the None.
    // NO `w_n > 0` TERM: it was here and it is redundant, because every winner contributes a
    // non-negative multiple, so `w_mult > 0.0` cannot hold with `w_n == 0`. A guard that re-decides
    // what the next guard already decided is an unkillable mutant by construction (`>` and `>=` agree
    // on a usize that is only ever compared to zero), so it is deleted rather than tested.
    (w_mult > 0.0 && !rows.is_empty()).then_some(MissTable { rows, pairs, by_count, w_n, w_mult })
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
/// Returns (factor, n_win, n_lose, median_win, median_lose, auc) and SKIPS any factor that cannot
/// fill both sides. (#165) added the `auc`: the medians give each factor's LEVEL, which is only
/// readable in its own units, while the `auc` is what makes two factors comparable to EACH OTHER.
/// Both are kept — dropping the medians would leave a row nobody can sanity-check against the
/// printed columns elsewhere in this report. A skipped row is a fact about what the point-in-time
/// pool covers, not about the factor — the caller prints the count it dropped so the absence stays
/// visible.
fn held_loser_factors(
    scored: &[(&Sample, f64)],
    n: usize,
    factors: &[&'static str],
) -> Vec<(&'static str, usize, usize, f64, f64, f64)> {
    let mut picked: BTreeMap<i32, Vec<(&Sample, f64)>> = BTreeMap::new();
    for (s, v) in scored {
        picked.entry(bucket(s.date)).or_default().push((s, *v));
    }
    let mut held: Vec<&Sample> = Vec::new();
    for (_, mut rows) in picked {
        if rows.len() < 2 {
            continue; // the same skip `recall_capture` applies, so both instruments count one pool
        }
        rows.sort_by(|a, z| z.1.total_cmp(&a.1));
        held.extend(rows.iter().take(n).map(|(s, _)| *s));
    }
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
            (!win.is_empty() && !lose.is_empty()).then(|| {
                // BEFORE the medians: `median` takes the Vec by value, so the cohorts are gone after.
                let sep = auc(&win, &lose);
                (*name, win.len(), lose.len(), median(win), median(lose), sep)
            })
        })
        .collect()
}

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

/// (#10) WHICH growth gate is too tight? #9 gives the aggregate verdict; this breaks it down per gate.
/// For each numeric gate, loosen its threshold one notch (relative to the loaded tuning, so a settings.yaml
/// override is respected) and report the mean forward peer-relative return of the names that loosening
/// NEWLY admits. A POSITIVE mean = that gate was discarding winners -> loosen it in settings.yaml and
/// re-validate (the lane OOS + #9's aggregate must still hold); ≤0 = the gate is correctly keeping junk
/// out, leave it. Pure measurement, no ranking change; reuses the ablation `Knob` pattern + `growth_score`.
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
    // sample counts the ten gates cost ~24 ms apiece — 237 ms, which measured as the largest single
    // stage in a wide run after the cache load and the walk itself. They are also completely
    // independent: `newly_admitted_stats` is a pure filter+mean+median over a borrowed slice, with no
    // shared state and no PRNG, so nothing here is order-sensitive the way the bootstrap stages are.
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
                    let tag = match (mean > 0.0, med > 0.0) {
                        (true, true) => "  <- TOO TIGHT (loosen this gate)",
                        (false, false) => "",
                        _ => "  <- SPLIT (tail-driven, read the median)",
                    };
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
/// "does removing this term hurt?"; `tune` answers "what is the argmax?" — across 8 dims perturbed at
/// once, so each dim's own contribution is confounded. NEITHER can tell a sharp peak from a plateau,
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
        for (name, nw, nl, mw, ml, sep) in &rows {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// (#117) `walk_params`, OFF — the arm that ships, and the only thing keeping every golden still.
    /// The fallback must come back untouched no matter what the dates say, so this hands it a daily
    /// record and the monthly triple: if the guard were dropped the result would be 252/756/126 and
    /// every fixture would move.
    #[test]
    fn walk_params_off_is_the_run_wide_triple_verbatim() {
        let daily = trading_days(ymd(2016, 1, 4), 2518);
        assert_eq!(walk_params(&daily, false, (12, 36, 6)), (12, 36, 6));
        assert_eq!(walk_params(&daily, false, (252, 750, 126)), (252, 750, 126));
        // and with the knob ON, the same input must NOT be the fallback — otherwise the assertions
        // above would pass on a function that ignores its flag entirely.
        assert_ne!(walk_params(&daily, true, (12, 36, 6)), (12, 36, 6));
    }

    /// (#117) ON, against the two cadences the tool actually runs. This is the "near-inert where the
    /// data was already right" claim, checked rather than asserted in prose: a genuine monthly record
    /// resolves to EXACTLY the 12/36/6 `run` hardcodes, and a genuine daily one lands on 252/756/126
    /// against its 252/750/126 — so turning the knob on cannot move a name whose feed was honest, and
    /// every difference it does make is a series that was never monthly to begin with.
    ///
    /// The daily case is also why bars/year is span/count and not the median gap between bars.
    /// Consecutive sessions are one day apart, so a median-gap estimator would read 365 bars a year
    /// for a record that has 252 — off by 45%, in the direction that inflates `min_history`.
    #[test]
    fn walk_params_on_reproduces_the_constants_on_honest_data() {
        let monthly = month_ends(2009, 9, 205); // AVGO's real shape: 205 bars, 2009-09 -> 2026-08
        assert_eq!(walk_params(&monthly, true, (999, 999, 999)), (12, 36, 6));

        let daily = trading_days(ymd(2016, 1, 4), 2518); // ~10y of sessions, `fetch_history`'s range
        assert_eq!(walk_params(&daily, true, (999, 999, 999)), (252, 756, 126));
    }

    /// (#117) ON, against the granularities Yahoo actually returns for a thin line while answering an
    /// `interval=1mo` request. These are the 4011 non-monthly series of the 5829 in the live long
    /// cache, and each one of them was being walked at 12/36/6.
    ///
    /// The weekly row is the one that ruins a sample quietly: at 12/36/6 such a name needs only 36
    /// WEEKS (8 months) of history before its first cutoff and then emits one every 6 weeks, so it
    /// contributes ~8.7x the cutoffs a monthly name does. Daily is ~40x. Neither is a modelling
    /// choice — it is whichever granularity the feed happened to serve.
    #[test]
    fn walk_params_on_reads_the_granularity_yahoo_actually_served() {
        let weekly = every_n_days(ymd(2010, 1, 4), 7, 800);
        assert_eq!(walk_params(&weekly, true, (12, 36, 6)), (52, 157, 26));

        let quarterly = month_ends(2000, 3, 100).into_iter().step_by(3).collect::<Vec<_>>();
        assert_eq!(walk_params(&quarterly, true, (12, 36, 6)), (4, 12, 2));

        // hourly, which is what `WIC.DE` and `34U.DE` really carry: ~450 bars over two months. No 8y
        // forward window exists in a record that short, so these contribute nothing either way — but
        // they must not be walked as if 36 bars were three years.
        let hourly = every_n_days(ymd(2026, 6, 5), 1, 70);
        let (cadence, min_history, step) = walk_params(&hourly, true, (12, 36, 6));
        assert!(cadence > 300, "a sub-daily record cannot measure 12 bars a year; got {cadence}");
        assert!(min_history > 36 && step > 6, "got {min_history}/{step}");
    }

    /// (#117) The three degenerate records, each of which would otherwise divide by zero or floor a
    /// parameter at 0 — and a `step` of 0 is an INFINITE LOOP in both walks, not a wrong number.
    #[test]
    fn walk_params_refuses_to_measure_a_record_that_says_nothing() {
        let fb = (12, 36, 6);
        assert_eq!(walk_params(&[], true, fb), fb, "no dates at all");
        assert_eq!(walk_params(&[ymd(2020, 1, 1)], true, fb), fb, "one bar spans nothing");
        assert_eq!(walk_params(&[ymd(2020, 1, 1); 4], true, fb), fb, "four bars, zero span");

        // Sparser than one bar every two years: `step` would round to 0 and hang the walk. The floor
        // is what makes it 1, and every parameter must clear it.
        let sparse = every_n_days(ymd(1990, 1, 1), 365 * 3, 12);
        let (cadence, min_history, step) = walk_params(&sparse, true, fb);
        assert!(cadence >= 1 && min_history >= 1 && step >= 1, "got {cadence}/{min_history}/{step}");
    }

    /// Consecutive trading sessions from `start`. Weekends dropped, and every 29th surviving weekday
    /// dropped as a holiday — because weekdays ALONE are 261 a year and a real exchange closes about
    /// nine more, which is the difference between 261 and the ~252 the daily constants were sized on.
    /// Modelling it matters here: the whole point of the daily assertion is that the estimator lands
    /// on the number the shipped `STEP_SESSIONS`/`MIN_HISTORY` already assume.
    fn trading_days(start: NaiveDate, n: usize) -> Vec<NaiveDate> {
        let mut out = Vec::with_capacity(n);
        let (mut d, mut weekdays) = (start, 0usize);
        while out.len() < n {
            if !matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
                weekdays += 1;
                if weekdays % 29 != 0 {
                    out.push(d);
                }
            }
            d += chrono::Duration::days(1);
        }
        out
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

    /// `n` bars spaced exactly `gap` days apart.
    fn every_n_days(start: NaiveDate, gap: i64, n: usize) -> Vec<NaiveDate> {
        (0..n).map(|k| start + chrono::Duration::days(gap * k as i64)).collect()
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
    /// Split, each cohort de-means against its own currency and the FX drops out exactly. The third
    /// cohort is one lonely German line: too thin to be a peer group, so it falls back to the pooled
    /// mean rather than de-meaning against itself and reporting a `relative` of 0 as if measured.
    #[test]
    fn the_market_split_peer_group_cancels_the_currency_move() {
        let d = ymd(2015, 1, 1);
        let mut rows: Vec<Sample> = Vec::new();
        let mut push = |tk: &str, r: f64| {
            let mut s = sample(d, r);
            s.quote = Arc::new(Quote::stub(tk, "1", "", tk));
            rows.push(s);
        };
        // two cohorts, same dispersion (-5/+5 around their own centre), 20 pts apart purely by currency.
        for (i, r) in [30.0, 40.0, 35.0, 35.0, 35.0, 35.0, 35.0, 35.0].iter().enumerate() {
            push(&format!("US{i}"), *r);
        }
        for (i, r) in [10.0, 20.0, 15.0, 15.0, 15.0, 15.0, 15.0, 15.0].iter().enumerate() {
            push(&format!("LN{i}.L"), *r);
        }
        push("DE0.DE", 45.0); // the thin slice: one name, its own market

        let pooled = {
            let mut v = rows.clone();
            demean_with(&mut v, false);
            v
        };
        let split = {
            let mut v = rows.clone();
            demean_with(&mut v, true);
            v
        };
        // pooled: one mean over all three markets, so the whole 20-pt FX gap lands in `relative` as if
        // it were selection — the US cohort is lifted and the LSE cohort docked by half of it.
        let pm: f64 = rows.iter().map(|s| s.realized).sum::<f64>() / rows.len() as f64;
        assert!((pooled[0].relative - (30.0 - pm)).abs() < 1e-9, "{}", pooled[0].relative);
        assert!((pooled[8].relative - (10.0 - pm)).abs() < 1e-9, "{}", pooled[8].relative);
        // the two cohorts sit ~20 apart in `relative` on nothing but the currency.
        assert!((pooled[0].relative - pooled[8].relative - 20.0).abs() < 1e-9);
        // split: each cohort centres on its own 35 / 15, so the SAME name reads -5 in both cohorts and
        // the currency move is gone. This is the whole claim.
        assert!((split[0].relative + 5.0).abs() < 1e-9, "{}", split[0].relative);
        assert!((split[8].relative + 5.0).abs() < 1e-9, "{}", split[8].relative);
        assert!((split[1].relative - 5.0).abs() < 1e-9 && (split[9].relative - 5.0).abs() < 1e-9);
        // the lone German line: 1 < MIN_PEER_GROUP, so it keeps the pooled mean rather than becoming 0.
        assert!((split[16].relative - 0.0).abs() > 1e-9, "a singleton must not de-mean against itself");
        assert!((split[16].relative - pooled[16].relative).abs() < 1e-9, "it falls back to the pool");
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
    }

    /// (#89) The block length is real, and 0 is exactly the one-bucket resample every band in this repo
    /// was read off.
    ///
    /// 16 half-year buckets, 4 rows each, scores 0..3, with the peer-relative return sitting on the top
    /// two rows only — so a draw's edge is exactly the MEAN of `e_k` over the buckets it drew, and the
    /// bootstrap reduces to the textbook case (the sampling distribution of a mean). `e_k` is a two-regime
    /// step (eight flat buckets then eight rich ones) because SERIAL CORRELATION is the entire subject:
    /// the real data has it (consecutive cutoffs share 95.8% of their forward path at 12y) and a
    /// one-bucket block cannot see it. Note that `picks * block == keys.len()` at both settings, so both
    /// bands come from pools of the SAME SIZE — the only difference is how many INDEPENDENT selections
    /// built them, 16 at block 1 against 2 at block 8. Measured: (46.9, 103.1) at block 1 against
    /// (28.1, 121.9) at block 8 — both centred on the true 75, width 56.2 -> 93.8, a ratio of 1.67
    /// against the 1.63 the finite-population arithmetic predicts. The 1.25 bar below is that ratio with
    /// room, not a tuned number: this is a seeded PRNG on frozen rows, so it does not drift.
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
    fn sidak_tail_tightens_both_ends_by_five_over_n() {
        assert_eq!(sidak_tail(1), (5.0, 95.0), "one candidate is no selection — the plain 90% band");
        assert_eq!(sidak_tail(10), (0.5, 99.5));
        let (lo, hi) = sidak_tail(14); // a fixed arity, NOT FUND_FACTORS.len() — this tests the
        // function, so it must not move when (#157) widens the array (14 was the length then)
        assert!((lo - 0.357_142_857).abs() < 1e-9 && (hi - 99.642_857_142).abs() < 1e-9, "{lo} {hi}");
        // both ends move OUTWARD as the search widens: more factors tried -> a stricter bar, which is
        // the whole point. A `-` flipped to `+` here would tighten the band and pass a losing winner.
        assert!(sidak_tail(14).0 < sidak_tail(10).0 && sidak_tail(14).1 > sidak_tail(10).1);
        assert_eq!(sidak_tail(0), (5.0, 95.0), "no candidates -> no division by zero");
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

    /// (#75) The value brake's graded trim, on two buckets whose peg cohorts do not overlap at all —
    /// bucket 1 spans 10..50, bucket 2 spans 100..500. That gap is the whole point: a POOLED percentile
    /// would floor both at 50 and so cut nothing from bucket 2 while gutting bucket 1, which is the one
    /// way this brake could quietly stop being cross-sectional. The boundary name (peg exactly ON the
    /// floor) is kept, matching `drop_bottom_book`'s `if v < t { skip }`, and the unjudgeable name is
    /// kept because unjudgeable is not a verdict.
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

    /// (#45) rank-slice ladder + head-to-head on a synthetic ranking with KNOWN outcomes — the
    /// assert that fails if slices stop being disjoint, stop following score order, or the
    /// head-to-head silently changes its comparison sets. Window A: 25 names, score descending,
    /// realized stepped by rank block (rank1 +100, 2-5 +50, 6-10 +30, 11-20 +10, 21-25 0), bench
    /// +10 everywhere. Window B: 8 names only — too small for a head-to-head (needs ≥11), and its
    /// 21-50 slice must NOT exist rather than fake a short book.
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
    }

    /// (#40) benchmark math: `ann` inverts a 12y cumulative back to CAGR (and floors a wipeout at
    /// −100%, no NaN), and `benchmark_fwd` picks the first session on/after the cutoff + the first past
    /// +Ny, returning None when the window runs off the end.
    #[test]
    fn benchmark_math() {
        // (1.10^12 − 1)·100 cumulative -> back to +10%/yr; a −100% window annualizes to −100, not NaN.
        assert!((ann((1.10_f64.powi(12) - 1.0) * 100.0, 12) - 10.0).abs() < 1e-6);
        assert!((ann(-100.0, 12) - (-100.0)).abs() < 1e-9);
        let dates: Vec<NaiveDate> = (0..15).map(|k| ymd(2000 + k, 1, 3)).collect();
        let closes: Vec<f64> = (0..15).map(|k| 100.0 * 1.08_f64.powi(k)).collect(); // +8%/yr
        let r = benchmark_fwd(&dates, &closes, ymd(2000, 1, 1), 12).unwrap();
        assert!((ann(r, 12) - 8.0).abs() < 0.1); // 12y forward from 2000 -> ~+8%/yr
        assert!(benchmark_fwd(&dates, &closes, ymd(2010, 1, 1), 12).is_none()); // no 12y window left
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

        // the verdict journal: wide AND deep. A watchlist run must never publish over the nightly
        // one, and a throttled wide run that resolved too few names must not either.
        assert!(may_write_verdict(true, false, MIN_VERDICT_TICKERS));
        assert!(!may_write_verdict(false, false, MIN_VERDICT_TICKERS), "a watchlist run must not publish");
        assert!(!may_write_verdict(true, false, MIN_VERDICT_TICKERS - 1), "a thin wide run is a throttled one");
        // (PIT) and the third refusal, which a big enough sample must NOT override: a point-in-time run
        // is wide and deep and still must not journal, because it measures a pool the screen never ranks.
        assert!(!may_write_verdict(true, true, MIN_VERDICT_TICKERS), "a PIT run is a different claim, not a better sample");
        assert!(!may_write_verdict(true, true, MIN_VERDICT_TICKERS * 10));
        // and the refusal SAYS WHICH, or a reader debugs the ticker count that was never the problem
        assert!(no_verdict_reason(true, MIN_VERDICT_TICKERS).contains("POINT-IN-TIME"));
        assert!(no_verdict_reason(false, 3).contains("only 3 tickers resolved"));
        assert!(!no_verdict_reason(false, 3).contains("POINT-IN-TIME"));

        // the fundamental lane: either flag turns it on, not both
        assert!(fund_lane_on(true, false));
        assert!(fund_lane_on(false, true));
        assert!(fund_lane_on(true, true));
        assert!(!fund_lane_on(false, false));
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
    /// `universe` is the one that matters most. It gates the WIDE run — the nightly gate's entire
    /// subject — and it also gates the verdict journal (`wide && tickers.len() >= MIN_VERDICT_TICKERS`)
    /// that the screen footer quotes. Lose that keyword and `backtest 12 universe` quietly becomes a
    /// backtest of one ticker literally named "universe".
    #[test]
    fn args_parse_every_keyword() {
        let p = |s: &str| parse_args(&s.split_whitespace().map(String::from).collect::<Vec<_>>());

        // default: no args at all
        assert_eq!(p(""), Args { years: 5, ..Default::default() });

        // each keyword on its own, so one arm going dead cannot hide behind another
        assert!(p("universe").wide, "`universe` must set wide — the nightly gate IS the wide run");
        assert!(p("long").long);
        assert!(p("fund").fund);
        assert!(p("tune").tune);
        assert!(p("insider").insider);
        assert!(p("halflife").halflife);
        assert!(p("stress").stress);
        assert!(p("pit").pit, "`pit` must set pit — lose it and the point-in-time run is a plain one that says it isn't");
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

        // everything unrecognised is a ticker, in order, unchanged
        let mixed = p("AAPL MSFT tune 20");
        assert!(mixed.tune);
        assert_eq!(mixed.years, 5, "20 arrives after two tickers, so it is a name too");
        assert_eq!(mixed.tickers, vec!["AAPL", "MSFT", "20"], "keywords are consumed, everything else is a ticker in order");

        // all flags at once still parse independently
        let all = p("8 universe long fund tune insider halflife stress pit");
        assert_eq!(
            all,
            Args {
                years: 8,
                wide: true,
                long: true,
                fund: true,
                tune: true,
                insider: true,
                halflife: true,
                stress: true,
                pit: true,
                tickers: vec![]
            }
        );
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
        // annualising the SPREAD instead would read +11.7 pts/yr — a different, wrong answer.
        assert!(!tag.contains("+11.7"));
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

        // ON: ~6mo buckets, so 2 per year of hold. A 0.15pt charge on a 20y run becomes 4.0pt.
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
            (rows[0].0, rows[0].1, rows[0].2, rows[0].3, rows[0].4),
            ("roe", 2, 1, 25.0, 2.0),
            "held book is WIN1/WIN2/LOSE: winners median (30,20)->25, losers (2)->2; UNHELD must not vote"
        );

        // n=4 admits UNHELD, and its -500 is what proves the top-n slice above was doing the work.
        let all = held_loser_factors(&scored, 4, &["roe"]);
        assert_eq!(all[0].2, 2, "UNHELD is a loser once held");
        assert!(all[0].4 < 2.0, "its -500 must drag the loser median below LOSE's 2.0: {:?}", all[0]);

        // A bucket of one is skipped, exactly as `recall_capture` skips it, so both instruments
        // count the same pool — otherwise a single-name window would report a 100% one-sided split.
        assert!(held_loser_factors(&scored[..1], 1, &["roe"]).is_empty());

        // (#165) THE AUC IS WIRED AND IS NOT A CONSTANT. Above, both winners sit over the lone
        // loser, so the column reads a clean 1.0; here the cohorts INTERLEAVE — W(1) below L(5),
        // W2(9) above it — so one of the two pairs fails and the same column must read 0.5. A
        // hard-coded field passes the first and fails the second, which is why both are pinned.
        assert_eq!(rows[0].5, 1.0, "roe separates WIN1/WIN2 from LOSE completely: {rows:?}");
        let ov = [
            row("W", 10.0, Some(1.0), None),
            row("L", -10.0, Some(5.0), None),
            row("W2", 20.0, Some(9.0), None),
        ];
        let ovs: Vec<(&Sample, f64)> = ov.iter().enumerate().map(|(i, x)| (x, 10.0 - i as f64)).collect();
        let r = held_loser_factors(&ovs, 3, &["roe"]);
        assert_eq!(r[0].5, 0.5, "W(1) < L(5) < W2(9): one pair wins, one loses -> 0.5: {r:?}");
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
        assert!(fresh.contains("(rerun: `folioman backtest universe`)") && !fresh.contains('⚠'));
        let drifted = verdict_line(&v, true, false);
        assert!(drifted.contains("⚠ settings changed since"));
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
            let kept = ["K1", "K2", "K3", "K4"].iter().flat_map(|t| pair(t, false, 0.0));
            let failed = ["F1", "F2", "F3", "F4"]
                .iter().zip(failed_rels).flat_map(|(t, r)| pair(t, true, r));
            kept.chain(failed).collect()
        };
        // every newly-failed name underperformed the holders -> both stats agree -> real sell signal
        let (gap, gap_med) = exit_probe(&build([-10.0, -12.0, -14.0, -16.0]), scorer, &BuyHeuristic::default()).unwrap();
        assert!(gap < 0.0 && gap_med < 0.0, "unanimous underperformance -> both stats negative");
        // THE TRAP: three of the four newly-failed names recovered; one collapsed hard enough to drag
        // the MEAN negative on its own. Mean-only, this prints "SELL". The median says hold.
        let (gap, gap_med) = exit_probe(&build([5.0, 6.0, 7.0, -900.0]), scorer, &BuyHeuristic::default()).unwrap();
        assert!(gap < 0.0, "the one collapse carries the mean");
        assert!(gap_med > 0.0, "three of four recovered — the typical name says hold");
        assert!(gap_verdict(gap, gap_med, "hold", "sell").starts_with("SPLIT"), "must not call a sell on the tail");
        // fewer than 4 on a side is no claim at all
        assert!(exit_probe(&build([-1.0; 4])[..6], scorer, &BuyHeuristic::default()).is_none());
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
        // shipped callers (all pass 1/5/50/95/99), so the clamp is dead code the report pin can never
        // exercise — the mutation audit flags it as unkilled, and this is the only way to observe it.
        assert_eq!(percentile(&s, 150.0), 100.0);
    }

    /// (Item 9) `turnover_frac` = 1 − mean Jaccard of consecutive buckets' top-half tickers. Two ~6mo
    /// buckets holding {A,B} then {A,C} overlap 1/3 -> turnover 2/3; a single bucket can't be measured -> 0.
    #[test]
    fn turnover_frac_consecutive_buckets() {
        let mk = |t: &str, m: u32| Sample {
            date: ymd(2020, m, 1),
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
        assert!((turnover_frac(&scored) - 2.0 / 3.0).abs() < 1e-9, "got {}", turnover_frac(&scored));
        assert_eq!(turnover_frac(&scored[..4]), 0.0); // single bucket -> unmeasurable -> 0
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

    /// (#10) `newly_admitted_mean` must isolate exactly the names a LOOSER gate newly admits (rejected
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
        // run()'s exact cutoff walk: MIN_HISTORY warmup, STEP_SESSIONS stride, 5y forward window,
        // stop when no full window remains. (Kept in sync by the pin itself: a drift in these
        // constants changes the sample set and the pinned numbers move.)
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
    #[allow(clippy::type_complexity)]
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
        // (P1) the four survival gates. Every one reads `quote.fund`, which `backtest_quote` never
        // fills, so all four pin INERT on this price-only path for exactly the cause (a) reason
        // `growth_max_peg` does — and all four are LIVE under `backtest ... fund`, per SCOPE above.
        // Floors probe absurdly HIGH; the dilution ceiling probes just above its own 0 off-sentinel.
        ("growth_max_dilution_pct", |t, v| t.growth_max_dilution_pct = v, 1e-6),
        ("growth_min_interest_cover", |t, v| t.growth_min_interest_cover = v, 1e9),
        ("growth_min_fcf_margin", |t, v| t.growth_min_fcf_margin = v, 1e9),
        ("growth_min_net_cash_rev", |t, v| t.growth_min_net_cash_rev = v, 1e9),
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
    /// The whole question reduces to one fact nobody wrote down: `core::backtest_quote` fills 18 fields
    /// (perf, drawdown_pct, range_pct, volatility_pct, downside_dev_pct, below/above_ma_pct, trend_r2,
    /// trend_cagr, life_cagr, capped_cagr, max_drawdown_pct, roll5y/10y_pos_pct, worst_5y/10y_pct,
    /// underwater_yrs on the daily path, and a sentinel avg_turnover_eur) and leaves the rest at
    /// `Quote::stub` defaults, so any gate reading an unfilled field can never fire. This pin turns
    /// that into an assertion, in BOTH directions — a live gate going silently dead is the ETF bug's
    /// exact shape, and a false "inert" claim is how the receipts rotted.
    ///
    /// SCOPE: the PRICE-ONLY reconstruction. `backtest ... fund` additionally attaches `quote.fund`
    /// (backtest.rs:422), so the fundamentals knobs are live on THAT path — `growth_max_peg` reads
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
    ///       `growth_min_age_years` (age_years), `growth_min_range_pct_8y` (stats_8y). None of those
    ///       five is assigned anywhere in this file; `growth_max_peg` (quote.fund) joins them on the
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
                "crypto LIVE  growth_min_8y_pct growth_min_20y_pct growth_min_leg_years growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto growth_turnover_weight\n",
                // (2026-08-19) the (P4) pair enters LIVE for etf and stock on the FIRST commit that adds
                // them — the opposite of the (P1) four, which pin INERT here because `quote.fund` is
                // unfilled on this path. These read `volatility_pct` and `max_daily_1m`, both built from
                // the same close series `backtest_quote` already walks, so they are sweepable from day one
                // and their receipts must not claim otherwise. Crypto is INERT by SCOPE (cause b): both
                // carry a `!crypto` guard, and the coin lane keeps `growth_max_vol_crypto` instead.
                "crypto INERT growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_peg growth_max_vol growth_max_daily_1m growth_max_dilution_pct growth_min_interest_cover growth_min_fcf_margin growth_min_net_cash_rev growth_require_lifetime_uptrend crypto_max_mvrv sharpe_cap_etf growth_min_aum_etf growth_ter_drag growth_commodity_damp growth_fx_damp growth_min_age_years growth_min_range_pct_8y\n",
                "etf    LIVE  growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_min_8y_pct growth_min_20y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_vol growth_max_daily_1m growth_min_leg_years sharpe_cap_etf growth_commodity_damp growth_turnover_weight\n",
                "etf    INERT growth_max_peg growth_max_dilution_pct growth_min_interest_cover growth_min_fcf_margin growth_min_net_cash_rev growth_require_lifetime_uptrend growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto crypto_max_mvrv growth_min_aum_etf growth_ter_drag growth_fx_damp growth_min_age_years growth_min_range_pct_8y\n",
                "stock  LIVE  growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_min_8y_pct growth_min_20y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_vol growth_max_daily_1m growth_min_leg_years growth_commodity_damp growth_turnover_weight\n",
                "stock  INERT growth_max_peg growth_max_dilution_pct growth_min_interest_cover growth_min_fcf_margin growth_min_net_cash_rev growth_require_lifetime_uptrend growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto crypto_max_mvrv sharpe_cap_etf growth_min_aum_etf growth_ter_drag growth_fx_damp growth_min_age_years growth_min_range_pct_8y\n",
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
    /// `instrument_type` empty and `name` = the ticker, `is_commodity_etf` (picks.rs:338) could never
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
        // run()'s exact cutoff walk, twice: once with the class fields stamped on, once without —
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
        #[allow(clippy::type_complexity)]
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
            sweep_cutoffs("VWRA.L", &dates, c, &[], "VANGUARD FUNDS PLC", "", &holds, MIN_HISTORY, STEP_SESSIONS, 252, false, &etf_set, &sector_of, &BTreeMap::new())
        };
        let got = walk(&closes);

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
        let mut poisoned = closes.clone();
        poisoned[MIN_HISTORY] = 0.0;
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

    /// 40 years of monthly bars, one grid, ten shapes: five compounders that clear the shipped gates in
    /// a known order, one that earns its CAGR through a −55% pit, and four that should never score.
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
            ("CMPA", series(1.30, 0.0, false)), // five compounders, rates apart, cycles out of step
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
                // holds=[12] / min_history 36 / step 6 / cadence 12 — `run`'s monthly branch verbatim
                sweep_cutoffs(tk, &dates, closes, &[], tk, "", &[12], 36, 6, 12, false, &etf_set, &sector_of, &BTreeMap::new())
                    .into_iter()
                    .map(|(_, s)| s)
            })
            .collect();
        demean(&mut samples);
        samples
    }

    /// The SHIPPED tuning, not the code defaults. `BuyHeuristic::default()` is deliberately neutral —
    /// its field docs read "0 = off (DEFAULT); ci-settings ships 5.0" — so a test that scored with it
    /// would grade a tuning nobody runs. Same raw-parse the config pins use (no merge, no
    /// FOLIOMAN_CONFIG, no race with the other tests in this file).
    fn shipped_tuning() -> BuyHeuristic {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        let s: config::Settings = serde_yaml::from_str(&text).expect("parse tests/ci-settings.yaml");
        s.buy_heuristic
    }

    /// The OFFLINE half of the backtest gate. `backtest_edge_holds` (tests/network.rs) asserts two
    /// different things at once — "a scoring-code change or a default-tuning edit broke the edge" AND
    /// "the edge still holds on today's market" — and prices the first at the second's cost: 3000+ live
    /// tickers, nightly, so a scoring regression surfaces the next morning. This is the first half,
    /// deterministic and network-free, so it runs on every `cargo test`. Regime drift stays nightly;
    /// that is the one question a fixture genuinely cannot answer.
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
        let tuning = shipped_tuning();
        let gated = |t: &BuyHeuristic| samples.iter().filter(|s| growth_score(&s.quote, t).is_some()).count();

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
        let mut tighter = shipped_tuning();
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
}
