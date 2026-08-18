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
/// - `top-3 `, `excess`, `tuning adds`, `early rho`, `late rho` — panic only under `forced`.
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
    pub const TOP3_ROW: &str = "top-3 ";
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
    markers::TOP3_ROW,
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

/// Basket size the journal grades on. NOT the top-10 the entry-state table ranks by: the footer must
/// quote the basket a reader can actually buy off the screen, and top-3 is the measured peak of the
/// top-N ladder at EVERY horizon (8y 16.1 vs 15.0/14.9/14.4 for top-1/5/10/20; 12y 15.2; 20y 13.7),
/// with roughly HALF top-1's worst window. See the SHIP RULE v2 block in `tests/ci-settings.yaml`.
pub(crate) const VERDICT_TOP: usize = 3;

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
pub(crate) fn verdict_line(v: &Verdict, drift: bool) -> String {
    let tail = if drift {
        " — ⚠ settings changed since, rerun `folioman backtest universe`"
    } else {
        " (rerun: `folioman backtest universe`)"
    };
    format!(
        "Method backtest (run {}, wide universe, top-{} held {}y, {} windows): book {:+.1}%/yr, \
         {:+.1}pp/yr vs index, win {:.0}%, worst {:+.1}, OOS {:+.1}/{:+.1}{tail}",
        v.date, v.top, v.years, v.windows, v.book, v.excess, v.win, v.worst, v.oos_early, v.oos_late
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
    quote.sector = sector_of.get(&quote.ticker).cloned();
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
    let key = |s: &Sample| (bucket(s.date), picks::asset_class(&s.quote));
    let mut sums: HashMap<(i32, u8), (f64, usize)> = HashMap::new();
    for s in samples.iter() {
        let e = sums.entry(key(s)).or_insert((0.0, 0));
        e.0 += s.realized;
        e.1 += 1;
    }
    for s in samples.iter_mut() {
        let (sum, n) = sums[&key(s)];
        s.relative = s.realized - sum / n as f64;
    }
}

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

fn may_write_verdict(wide: bool, resolved: usize) -> bool {
    wide && resolved >= MIN_VERDICT_TICKERS
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
        let universe =
            fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, settings.prefer_eu_listing, &[]).await;
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
    if !pit_spans.is_empty() && !sector_of.is_empty() {
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
        hold_period_sweep(&client, &settings.urls, &tickers, monthly, cadence, min_history, step, tuning, &etf_set, &sector_of)
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
                        let mut quote = core::backtest_quote(tk, &dates, &closes, &divs, i, cadence);
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
    report_lane("ON-SALE (buy_score)", &samples, buy_score, tuning, &buy_knobs);
    report_lane("GROWTH (growth_score)", &samples, growth_score, tuning, &growth_knobs);
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
    gate_audit(&samples, growth_score, tuning); // (#9) are the growth lane's hard gates actually selecting winners?
    gate_sweep(&samples, tuning, &gate_loosen); // (#10) which specific gate is too tight?
    exit_probe(&samples, growth_score, tuning); // (Item 31) is a mid-hold gate FAILURE a measured sell signal?
    if fund_lane_on(fund, insider) {
        report_fund_lane(&samples);
        sweep_fund_factor(&samples, tuning); // (G) which factor pays THROUGH the growth lane, held-out
    }
    report_risk_lane(&samples); // closes-derived risk stats, standalone — no fundamentals needed
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
    report_relative_strength(&samples, &bench);
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
    if may_write_verdict(wide, tickers.len()) {
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
        eprintln!(
            "backtest: verdict NOT journaled — only {} tickers resolved (need {MIN_VERDICT_TICKERS}); \
             the screen footer keeps citing the previous run",
            tickers.len()
        );
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
    if pit {
        println!("  • Point-in-time (PIT ON): every cutoff was scored against the S&P 500 AS IT STOOD THAT DAY, so a");
        println!("    name contributes no sample before it joined or after it left. This is the direct correction to");
        println!("    the line above, and the edge is EXPECTED to fall — a lower number here is the honest one.");
        println!("    {pit_missing} pool name(s) were index members Yahoo no longer serves: fetched nothing, scored nothing,");
        println!("    counted here rather than silently dropped. A big count means the correction is still incomplete.");
        println!("    COVERAGE FLOOR 1996-01-02: the source opens every pre-existing member's span on that date, so");
        println!("    cutoffs before it are DROPPED rather than corrected — it bites the 20y run's front, not the 8y one.");
    }
    // (#61) WAS "no as-of dividends or P/E reconstructed; the * term above is inert here", and the
    // dividend half of that was false — `#53` plumbed as-of divs (`picks.rs` says so at the growth
    // lane's own dividend term) and the row it called inert was the largest in the table at Δ+150.7.
    // A footnote claiming a term cannot be graded is the exact thing that stops anyone grading it.
    println!("  • Price-only (#6): no as-of P/E reconstructed, so the `value` multiplier is ×1.0 here; as-of DIVIDENDS are live since #53 and every dividend row below is real.");
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
    etf_set: &HashSet<String>,
    sector_of: &HashMap<String, String>,
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
                                let mut q = core::backtest_quote(tk, dates, closes, divs, i, cadence);
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
                min_history, step, cadence, etf_set, sector_of,
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
        let net = edge - turn * ROUND_TRIP_BPS / 100.0;
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
fn report_fund_lane(samples: &[Sample]) {
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
        println!(
            "  {:<14} n={:<5} rho {}  edge {:+.1}  OOS {} | {}",
            name, pairs.len(), rho, edge, split_rho(&pairs[..mid]), split_rho(&pairs[mid..])
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
fn emit_probe(name: &str, pairs: &[(&Sample, f64)]) {
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
    println!(
        "  {:<14} n={:<5} rho {}  edge {:+.1}  OOS {} | {}",
        name, pairs.len(), rho, edge, split_rho(&pairs[..mid]), split_rho(&pairs[mid..])
    );
}

fn report_risk_lane(samples: &[Sample]) {
    println!("\n── PRICE-RISK (closes-derived, standalone probes) ──");
    for (name, get) in RISK_FACTORS {
        let pairs: Vec<(&Sample, f64)> =
            samples.iter().filter_map(|s| get(&s.quote).map(|v| (s, v))).collect();
        emit_probe(name, &pairs);
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
fn report_relative_strength(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>)) {
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
    emit_probe("abs_mom_5y", &abs_pairs);
    emit_probe("rel_str_5y", &rel_pairs);
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
fn sweep_fund_factor(samples: &[Sample], default: &BuyHeuristic) {
    const FACTORS: [&str; 14] = [
        "rev_cagr", "rev_accel", "gross_margin", "op_margin", "margin_trend", "eps_growth",
        // the printed columns (REV-YoY / EPS-YoY / NET%), swept for the first time. Widening this
        // array TIGHTENS every reported band: the Šidák haircut below divides by FACTORS.len(), so
        // going 11 -> 14 makes the best-of-N test stricter, not the factors weaker. A later reader
        // comparing bands across runs must check this length before calling it a regression.
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
        "composite",            // (Item 3) shows n/a until ≥2 factors are present
    ];
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
        let (train, test) = s.split_at(cut);
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
        (
            lane_metrics(test, growth_score, &won).1,
            lane_metrics(&test[..mid], growth_score, &won).0,
            lane_metrics(&test[mid..], growth_score, &won).0,
            won.growth_fund_weight,
        )
    };

    let (baseline, ..) = eval(None); // price-only TEST edge: the bar every factor must clear
    let results: Vec<(&str, f64, Option<f64>, Option<f64>)> =
        FACTORS.iter().map(|&n| { let (e, a, b, _) = eval(Some(n)); (n, e, a, b) }).collect();

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
            let n = FACTORS.len() as f64;
            if let Some((lo, hi)) = bootstrap_edge_ci(&s, growth_score, &tun, 1000, 5.0 / n, 100.0 - 5.0 / n) {
                let verdict = if lo > 0.0 {
                    "survives multiple testing -> trust the WINNER"
                } else {
                    "within best-of-N luck -> SHIP NOTHING despite the raw winner"
                };
                println!("  multiple-testing band (best of {} factors): [{lo:+.1} … {hi:+.1}] pts  ({verdict})", FACTORS.len());
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
            (
                lane_metrics(test, growth_score, &t).1,
                lane_metrics(&test[..mid], growth_score, &t).0,
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
/// index). This asks the real question: buy the top-N growth picks (equal-weight, non-crypto), hold
/// `years`, and does that beat holding ^GSPC over the SAME window? Per pick, excess = its annualized
/// return minus what ^GSPC did from the same cutoff. Top-N per ~6mo bucket; report mean pick/SPY CAGR,
/// excess, win-rate, worst bucket, and the early-vs-late OOS split. Read the STRESS run: the picks come
/// from today's survivors (biased UP), ^GSPC is the true index, so a non-stress win is optimistic.
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
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue; // crypto: a coin isn't an S&P500-comparable hold
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(bench_r) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        // RAW cumulative % (annualize the BOOK, not per-name)
        rows.push((bucket(s.date), score, s.realized, bench_r, s.quote.ticker.clone(), s.fund.as_ref().and_then(|f| f.peg_yield)));
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
    println!("\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD {years}y no-sell, vs {bench_sym}) ──");
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    // (#41/#43) EQUAL-WEIGHT HELD-BOOK return — the correct metric for a no-sell hold. A held book earns
    // ann(mean of terminal MULTIPLES), NOT mean of per-name CAGRs: a 20× winner in the book covers twenty
    // −100% zeros, and a name that goes to 0 contributes its full weight lost (1/N), not its scary CAGR.
    // Also count "zeros ridden" (names ≤−90% you must hold through) to show the no-sell tail you survive.
    let mut m10: Option<f64> = None; // top-10 mean terminal multiple, feeds the after-tax footer below
    for n in [1usize, 2, 3, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50] {
        let (mut book, mut spy, mut excess) = (Vec::new(), Vec::new(), Vec::new());
        let (mut zeros, mut held) = (0usize, 0usize);
        let mut zero_names: Vec<String> = Vec::new(); // (#zeros) names ≤−90% at top-10 -> union across horizons = true distinct death count
        let mut multiples: Vec<f64> = Vec::new();
        for (b, v) in &by_bucket {
            let mut vv = v.clone();
            vv.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // score desc
            let take = n.min(vv.len());
            if take == 0 {
                continue;
            }
            let p = &vv[..take];
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
    // (#45) RANK-SLICE ladder + same-window head-to-head. The cumulative top-N table above cannot
    // answer "is #1 really better than #20": ann(mean of multiples) mechanically favors bigger books
    // on fat-tailed outcomes (a 10-draw mean usually catches one lottery ticket, a 1-draw book
    // can't), so top-1 trailing top-10 is diversification math, not a ranking verdict. DISJOINT
    // slices and a direct same-window compare are the honest order test: if ranks 1 and 2-5 don't
    // beat 11-20 HERE, the order at the top of the screen carries no signal.
    let (slices, (h1, h25, hn)) = rank_slice_stats(&by_bucket);
    println!("  rank-slice (DISJOINT books, excess vs {bench_sym}; mean|median across windows — median is lottery-ticket-immune):");
    for (label, excess) in &slices {
        if excess.is_empty() {
            continue;
        }
        let ex_ann: Vec<f64> = excess.iter().map(|(bk, sp)| ann(*bk, years) - ann(*sp, years)).collect();
        let win = ex_ann.iter().filter(|e| **e > 0.0).count() as f64 / ex_ann.len() as f64 * 100.0;
        println!(
            "    rank {label:<6} excess {:+.1}|{:+.1} pts/yr   win {win:.0}% of {}",
            mean(&ex_ann),
            median(ex_ann.clone()),
            ex_ann.len()
        );
    }
    if hn > 0 {
        println!(
            "    head-to-head same-window (no averaging artifact): #1 beat the 11-20 book in {h1}/{hn} ({:.0}%), the 2-5 book did in {h25}/{hn} ({:.0}%) — >50% = the top of the list is genuinely better than its middle",
            h1 as f64 / hn as f64 * 100.0,
            h25 as f64 / hn as f64 * 100.0
        );
    }
    // (Phase B) the never-sell tax edge, made visible: a hold pays capital-gains ONCE at the final
    // sale, a yearly-rotation strategy on the SAME pre-tax path pays tax on each year's gain.
    if let Some(m) = m10.filter(|m| *m > 1.0) {
        let (never, rot) = after_tax_pair(m, years, CAPITAL_GAINS_TAX);
        println!(
            "  after-tax ({:.0}% PT, top-10 book): never-sell {never:+.1}%/yr vs yearly-rotation {rot:+.1}%/yr -> deferral edge {:+.1} pts/yr",
            CAPITAL_GAINS_TAX * 100.0,
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

/// (#45) DISJOINT rank-slice books + same-window head-to-head, pure for testability. Per window
/// (bucket): sort by score desc; slice `lo..hi` of that order becomes its own equal-weight book;
/// each slice collects per-window (book cum %, bench cum %) pairs — the caller annualizes, so this
/// fn stays years-free. Head-to-head counts windows (≥11 names) where the #1 name / the 2-5 book
/// beat the 11-20 book on raw realized % — mean of multiples is monotone in mean of %, so no
/// annualization is needed to ORDER them within one window.
type SliceStats = (Vec<(&'static str, Vec<(f64, f64)>)>, (usize, usize, usize));
fn rank_slice_stats(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64, String)>>) -> SliceStats {
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    const SLICES: [(usize, usize, &str); 5] =
        [(0, 1, "1"), (1, 5, "2-5"), (5, 10, "6-10"), (10, 20, "11-20"), (20, 50, "21-50")];
    let mut out: Vec<(&'static str, Vec<(f64, f64)>)> = SLICES.iter().map(|(_, _, l)| (*l, Vec::new())).collect();
    let (mut h1, mut h25, mut hn) = (0usize, 0usize, 0usize);
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
        if vv.len() > 10 {
            hn += 1;
            let mid = mean(&vv[10..20.min(vv.len())].iter().map(|x| x.1).collect::<Vec<_>>());
            if vv[0].1 > mid {
                h1 += 1;
            }
            if mean(&vv[1..5].iter().map(|x| x.1).collect::<Vec<_>>()) > mid {
                h25 += 1;
            }
        }
    }
    (out, (h1, h25, hn))
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

/// (Phase B) PT capital-gains rate; hardcoded — add a knob only if a second rate is ever needed.
const CAPITAL_GAINS_TAX: f64 = 0.28;

/// After-tax %/yr of (never-sell, yearly-rotation) for the SAME pre-tax terminal multiple `m` over
/// `years` at gains-tax rate `t`. Never-sell defers to one final sale: net multiple = 1 + (m−1)(1−t).
/// Rotation realizes each year's gain: after-tax rate = gross annual rate × (1−t) — a simplification
/// that ignores loss-offset asymmetry, fine for a positive-multiple book.
fn after_tax_pair(m: f64, years: i64, t: f64) -> (f64, f64) {
    let y = years.max(1) as f64;
    let never = ((1.0 + (m - 1.0) * (1.0 - t)).powf(1.0 / y) - 1.0) * 100.0;
    let rot = (m.powf(1.0 / y) - 1.0) * (1.0 - t) * 100.0;
    (never, rot)
}

/// (#43) Equal-weight held-book stats for a given ranking key. `by_bucket`: 6mo-bucket ->
/// Vec<(rank_key, realized%, bench%)>. Per bucket: top-N by rank_key desc, held equal-weight -> book =
/// ann(mean terminal multiple), SPY = same on the bench leg, excess = book − SPY. Returns
/// (book, spy, excess_mean, win%, worst, oos_early, oos_late). `None` if no bucket yields a pick.
fn book_stats(by_bucket: &std::collections::BTreeMap<i32, Vec<(f64, f64, f64)>>, n: usize, years: i64) -> Option<(f64, f64, f64, f64, f64, f64, f64)> {
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let (mut book, mut spy, mut excess) = (Vec::new(), Vec::new(), Vec::new());
    for v in by_bucket.values() {
        let mut vv = v.clone();
        vv.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // rank_key desc
        let take = n.min(vv.len());
        if take == 0 {
            continue;
        }
        let p = &vv[..take];
        let bcum = mean(&p.iter().map(|x| 1.0 + x.1 / 100.0).collect::<Vec<_>>());
        let scum = mean(&p.iter().map(|x| 1.0 + x.2 / 100.0).collect::<Vec<_>>());
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
                "  {label:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}, mean entry dd {mdd:+.1}%)",
                dds.len()
            );
        }
    }
    if let Some((b, _, e, w, wo, el, la)) = book_stats(&base, n, years) {
        println!(
            "  {:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}) [unconditional]",
            "all entries", base.len()
        );
    }
    // The JOURNALED row, and the only one the screen footer quotes: the top-VERDICT_TOP basket a
    // reader can actually buy, not the top-10 the table above ranks by. Printed so the footer's
    // numbers are auditable inside the run that earned them.
    let verdict = book_stats(&base, VERDICT_TOP, years).map(|(b, _, e, w, wo, el, la)| {
        println!(
            "  {:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}) [unconditional, JOURNALED]",
            format!("all entries (top-{VERDICT_TOP})"), base.len()
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
    // NOT the measured optimum — the top-N ladder peaks at 3 at every horizon (see VERDICT_TOP).
    // 10 is kept HERE on purpose: these rows compare candidates against EACH OTHER, so they want the
    // wider, better-estimated book. The journaled verdict is the one that must match the buy policy.
    let n = 10;
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
    // over 12 runs. The lift here is a TOP-10 held-book effect and decays to +0.1 at 12y top-3 and to
    // 0.0 at 8y top-3, so it fails Ship Rule v2's ADDITION bar. THE BAR PRINTED BELOW IS THEREFORE NOT
    // A SHIP RULE — it grades a top-10 held-N-years no-sell book, while the verdict grades the
    // rebalanced top-3. Treat it as "worth a grid", never as "ship this". Full grid in the (#75)
    // receipt at `growth_value_floor_pct` in tests/ci-settings.yaml.
    // Note the same factor is very much alive as a RANK TILT in the same run (peg_yield is the shipped
    // `growth_fund_factor`) — cutting the dear names and ranking by cheapness are different questions,
    // and only the second one has ever paid at the top of the book.
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
    // NOT the measured optimum — the top-N ladder peaks at 3 at every horizon (see VERDICT_TOP).
    // 10 is kept HERE on purpose: these rows compare candidates against EACH OTHER, so they want the
    // wider, better-estimated book. The journaled verdict is the one that must match the buy policy.
    let n = 10;
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
    let mut s = samples.to_vec();
    demean(&mut s[..cut]);
    demean(&mut s[cut..]);
    let (train, test) = s.split_at(cut);
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
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
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
    let draws: Vec<Vec<i32>> =
        (0..iters).map(|_| (0..keys.len()).map(|_| keys[(next() % keys.len() as u64) as usize]).collect()).collect();
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

fn gate_audit(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> Option<(f64, f64)> {
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
    Some((gap, gap_med))
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
fn gate_sweep(samples: &[Sample], tuning: &BuyHeuristic, gates: &[Knob]) {
    println!("\n── GATE SWEEP (loosen each gate one notch -> fwd return of the names it NEWLY admits) ──");
    println!("  positive = the gate was too tight (newly-admitted beat the field); ≤0 = it's keeping junk out.");
    println!("  the TOO TIGHT flag needs mean AND median positive — one survivor can carry a mean on its own.");
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
                    format!("  {name:<26} n={n:<4} fwd peer-relative  mean {mean:+.1} | med {med:+.1} pts{tag}")
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
                split_rho(&re[..mid]),
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
    println!("  top-half peer-relative {top:+.1} pts  vs  bottom-half {bot:+.1} pts  ->  edge {base_edge:+.1} pts");
    // (Item 5) bootstrap band: is that point edge distinguishable from 0 given overlapping-sample noise?
    if let Some((lo, hi)) = bootstrap_edge_ci(samples, scorer, tuning, 1000, 5.0, 95.0) {
        let verdict = if lo > 0.0 {
            "clears 0 -> real"
        } else if hi < 0.0 {
            "below 0 -> backwards"
        } else {
            "STRADDLES 0 -> noise"
        };
        println!("  90% bootstrap band on edge: [{lo:+.1} … {hi:+.1}] pts  ({verdict})");
    }
    // (Item 9) net of cost: a high-turnover edge can be NET-negative once you pay the spread to chase it
    // each rebalance. cost(pts) = turnover_frac × ROUND_TRIP_BPS / 100 (1 pt = 100 bps).
    let turn = turnover_frac(&scored);
    let net = base_edge - turn * ROUND_TRIP_BPS / 100.0;
    let tag = if net <= 0.0 { "  <- NET ≤ 0: too churny to trade" } else { "" };
    println!("  net of cost: edge {base_edge:+.1} − turnover {:.0}% × {ROUND_TRIP_BPS:.0}bps = net {net:+.1} pts{tag}", turn * 100.0);
    // (Item 12) is that edge a broad spread or one lucky name? Winsorize the tails and re-read it.
    let wedge = winsor_edge(&scored);
    let wtag = if wedge <= 0.0 && base_edge > 0.0 { "  <- raw edge is an OUTLIER ARTIFACT (leans on extreme rows)" } else { "" };
    println!("  winsorized edge (1/99 clamp): {wedge:+.1} pts{wtag}");
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
        split_rho(&scored[..mid]),
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
        let ci = |n: usize| bootstrap_edge_ci(&rows[..n], keeps_every_row, &t, 8, 5.0, 95.0);
        assert!(ci(3).is_none(), "3 buckets is too few to resample");
        assert!(ci(4).is_some(), "4 buckets is exactly enough to resample");

        // Enough buckets, but the gate rejects every row, so every draw is empty. That is the second
        // way the bootstrap declines to publish a band, and it must stay None rather than hand back a
        // band computed from an empty edge distribution (which percentile() renders as NaN, not `n/a`).
        fn scores_nothing(_: &Quote, _: &BuyHeuristic) -> Option<f64> {
            None
        }
        assert!(
            bootstrap_edge_ci(&rows, scores_nothing, &t, 8, 5.0, 95.0).is_none(),
            "every draw gated out -> no band, not a NaN band"
        );
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
        let (slices, (h1, h25, hn)) = rank_slice_stats(&by_bucket);
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

        // EXACTLY ON THE BOUNDARY. Windows A and B (25 and 8 names) straddle every slice start but
        // land on none of them, so `vv.len() <= lo` and `vv.len() < lo` decide identically above —
        // the mutation audit flagged that guard as unkilled for precisely that reason. A window with
        // exactly 10 names sits ON the 11-20 slice's `lo`, and there the two spellings diverge:
        // `<` lets the slice through, `&vv[10..10]` is EMPTY, and `mean` of nothing is 0.0, which
        // this function's `(mean - 1.0) * 100.0` renders as a **-100% book** — a fabricated total
        // wipeout row in the ladder, not a missing one. That is the "no fake short book" the guard's
        // own comment promises, and it now has a test standing on it.
        by_bucket.insert(3, (0..10).map(|r| (100.0 - r as f64, step(r), 10.0, format!("C{r}"))).collect());
        let (slices, _) = rank_slice_stats(&by_bucket);
        let get = |label: &str| slices.iter().find(|(l, _)| *l == label).unwrap().1.clone();
        assert_eq!(get("11-20").len(), 1, "a 10-name window sits ON the 11-20 slice start and must contribute NOTHING");
        assert_eq!(get("6-10").len(), 3, "all three windows still reach the 6-10 slice");
        assert!(get("11-20").iter().all(|p| p.0 > -100.0), "an empty slice must be skipped, never booked as -100%");
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
        assert!(may_write_verdict(true, MIN_VERDICT_TICKERS));
        assert!(!may_write_verdict(false, MIN_VERDICT_TICKERS), "a watchlist run must not publish");
        assert!(!may_write_verdict(true, MIN_VERDICT_TICKERS - 1), "a thin wide run is a throttled one");

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

    /// (round 27) the journaled method verdict: serde roundtrip is identity (the screen reads back
    /// exactly what backtest wrote), corrupt/empty JSON is an empty journal (a broken file silences
    /// the footer, never fabricates a verdict), and verdict_line's drift arm swaps the rerun-pointer
    /// for the ⚠ stale-settings warning (citing stale numbers as current would mislead the buy
    /// decision). The line must name the BASKET too — top-3 and top-10 are different claims.
    #[test]
    fn verdict_journal_semantics() {
        let v = stub_verdict(12, VERDICT_TOP);
        let json = serde_json::to_string(&Journal::from([(v.years, stub_verdict(12, VERDICT_TOP))])).unwrap();
        let back = parse_journal(&json).into_values().next_back().expect("roundtrip parses");
        assert_eq!((back.date.as_str(), back.years, back.top, back.windows), ("2026-07-19", 12, 3, 84));
        assert!((back.book - 14.3).abs() < 1e-9 && (back.excess - 6.9).abs() < 1e-9);
        assert_eq!(back.tuning_fp, "{\"a\":1}");

        assert!(parse_journal("not json").is_empty());
        assert!(parse_journal("").is_empty());
        assert!(parse_journal("{\"date\":\"x\"}").is_empty()); // missing fields -> empty, not a default
        assert!(parse_journal("{\"20\":{\"date\":\"x\"}}").is_empty()); // half-written row, same rule

        let fresh = verdict_line(&v, false);
        assert!(fresh.contains("run 2026-07-19, wide universe, top-3 held 12y, 84 windows"), "{fresh}");
        assert!(fresh.contains("book +14.3%/yr, +6.9pp/yr vs index, win 71%, worst -8.2, OOS +5.1/+7.4"));
        assert!(fresh.contains("(rerun: `folioman backtest universe`)") && !fresh.contains('⚠'));
        let drifted = verdict_line(&v, true);
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
        assert!(verdict_line(&only, false).contains("top-10 held 20y"));
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
                let quote = core::backtest_quote(tk, &dates, closes, &[], i, 252);
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
        let (gap, gap_med) = gate_audit(&good, dd_gate, &def).unwrap();
        assert!(gap > 0.0 && gap_med > 0.0, "gate keeps winners -> both stats positive");
        // flip: the dd>0 (accepted) names now carry the LOW returns -> gate admits losers -> negative gap
        let bad: Vec<Sample> = [(-5.0, 1.0), (-6.0, 1.0), (-7.0, 1.0), (-8.0, 1.0), (5.0, -1.0), (6.0, -1.0), (7.0, -1.0), (8.0, -1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        let (gap, gap_med) = gate_audit(&bad, dd_gate, &def).unwrap();
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
        let (gap, gap_med) = gate_audit(&skewed, dd_gate, &def).unwrap();
        assert!(gap > 0.0, "one 20-bagger carries the accepted MEAN above the rejected pool");
        assert!(gap_med < 0.0, "the typical accepted name still lost — the median must not follow the tail");
        // and the verdict must refuse to pick a side rather than quietly reporting the mean's answer
        assert!(gap_verdict(gap, gap_med, "yes", "no").starts_with("SPLIT"), "disagreement must print SPLIT");
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
        let (never, rot) = after_tax_pair(4.0, 12, 0.28);
        assert!((never - 10.063).abs() < 0.01, "never-sell got {never}");
        assert!((rot - 8.817).abs() < 0.01, "rotation got {rot}");
        assert!(never > rot); // deferral always wins on a gain
        let (n2, r2) = after_tax_pair(4.0, 0, 0.28); // years clamps to 1
        assert!((n2 - 216.0).abs() < 1e-9); // 1+3·0.72=3.16 -> +216%/yr over 1y
        assert!((r2 - 216.0).abs() < 1e-9); // 4^1−1=300% gross ×0.72 = +216% — same over 1y
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
                let quote = core::backtest_quote(tk, &dates, closes, &[], i, 252);
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
    ///       the `!crypto` guards (above-MA ceiling, lifetime uptrend), `sharpe_cap_etf` off a fund,
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
            let mut quote = core::backtest_quote(tk, &dates, &closes, &[], n - 1, 252);
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
                "crypto INERT growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_max_peg growth_require_lifetime_uptrend crypto_max_mvrv sharpe_cap_etf growth_min_aum_etf growth_ter_drag growth_commodity_damp growth_fx_damp growth_min_age_years growth_min_range_pct_8y\n",
                "etf    LIVE  growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_min_8y_pct growth_min_20y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_min_leg_years sharpe_cap_etf growth_commodity_damp growth_turnover_weight\n",
                "etf    INERT growth_max_peg growth_require_lifetime_uptrend growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto crypto_max_mvrv growth_min_aum_etf growth_ter_drag growth_fx_damp growth_min_age_years growth_min_range_pct_8y\n",
                "stock  LIVE  growth_min_cagr growth_min_range_pct growth_min_1y_pct growth_min_5y_pct growth_min_8y_pct growth_min_20y_pct growth_maxdd_cap growth_max_above_ma max_1m_drop_pct growth_min_leg_years growth_commodity_damp growth_turnover_weight\n",
                "stock  INERT growth_max_peg growth_require_lifetime_uptrend growth_min_cagr_crypto growth_min_5y_pct_crypto growth_min_range_pct_crypto min_1y_pct_crypto max_1m_drop_pct_crypto growth_maxdd_cap_crypto growth_max_vol_crypto crypto_max_mvrv sharpe_cap_etf growth_min_aum_etf growth_ter_drag growth_fx_damp growth_min_age_years growth_min_range_pct_8y\n",
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
        let mut etc = core::backtest_quote("SGLN.L", &dates, &closes, &[], n - 1, 252);
        assert!(growth_score(&etc, &tuning).is_some(), "fixture must score unstamped — the old behaviour");
        stamp_asset_class(&mut etc, "iShares Physical Gold ETC", "ETF", &etf_set, &sector_of);
        assert_eq!(picks::asset_class(&etc), 1);
        assert!(growth_score(&etc, &tuning).is_none(), "physical-gold ETC must be gated once it classes as a fund");

        // and the gate is SELECTIVE, not "every ETF drops out" — which would fake the pass above.
        let mut broad = core::backtest_quote("XDWD.L", &dates, &closes, &[], n - 1, 252);
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
                    let mut quote = core::backtest_quote(tk, &dates, &closes, &[], i, 252);
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
            let mut q = core::backtest_quote(tk, &dates, &closes, &[], n - 1, 252);
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
            sweep_cutoffs("VWRA.L", &dates, c, &[], "VANGUARD FUNDS PLC", "", &holds, MIN_HISTORY, STEP_SESSIONS, 252, &etf_set, &sector_of)
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
            let mut quote = core::backtest_quote(tk, &dates, &closes, &[], n - 1, 252);
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
                sweep_cutoffs(tk, &dates, closes, &[], tk, "", &[12], 36, 6, 12, &etf_set, &sector_of)
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
