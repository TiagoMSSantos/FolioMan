//! `track` — live out-of-sample track record: how did each past `screen` book actually do?
//!
//! Every `screen` run appends its ranked top slice (tickers + EUR prices + the S&P 500 close) to
//! `.screen_snapshots.jsonl` (working dir, gitignored; one JSON line per day — a same-day rerun
//! adds nothing). `track` replays the journal against TODAY's prices: the equal-weight book
//! return of each snapshot's top rows vs the index over the same window. This grades the screen's
//! own advice on data that did not exist when it ranked — the live counterpart of the backtest's
//! held-book metric, accruing evidence with every month that passes. Price-only (dividends not
//! counted), EUR seat, same conventions as the backtest receipts. NOT advice.

use crate::{config, fetch};
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_FILE: &str = ".screen_snapshots.jsonl";
/// Graded book = the top-10 slice, matching the backtest's held-book receipts.
pub(crate) const BOOK: usize = 10;

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub date: String, // YYYY-MM-DD of the screen run
    pub spx: Option<f64>, // ^GSPC close (EUR) that day — the benchmark leg
    /// ^GSPC % off its high that day — lets `sim` replay the deploy-line entry-state multiplier
    /// at the journaled date. Absent on pre-sim lines (serde default) -> sim falls back to ×1.
    #[serde(default)]
    pub spx_off_hi: Option<f64>,
    pub rows: Vec<(String, Option<f64>)>, // (ticker, close EUR) in rank order, top slice
    /// (round 34) per-name fund AUM (EUR) for the same top slice, PARALLEL to `rows` — lets the
    /// fund-flow footer divide price appreciation out of AUM growth to read net shares
    /// created/redeemed. Absent on pre-r34 lines (serde default -> empty) -> the flow footer stays
    /// silent for them. Non-fund rows (stocks/crypto) carry `None` here.
    #[serde(default)]
    pub aum: Vec<(String, Option<f64>)>,
    /// (#103) the CORE shortlist of the same run — the buy-and-hold half of the report, which the
    /// screen prints and then forgot. Same `(ticker, close EUR)` shape as `rows` so a future grader
    /// can reuse `grade` unchanged, and in the same CORE order (breadth -> domicile -> TER -> AUM).
    /// Written only when `journal_core_list` is on; `skip_serializing_if` keeps the OFF line
    /// byte-identical to every line already on disk, and `default` keeps old lines readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core: Vec<(String, Option<f64>)>,
    /// (#286) THE EXECUTED BOOK: `(ticker, SIZE%)` exactly as `size` would fund that run — gate
    /// failures already dropped, one row per issuer, weighted by score / volatility inside a class
    /// budget and then capped. `rows` is the RANKED list and `track` graded its top slice
    /// equal-weight, which is not the book anyone is told to buy: on the 2026-09-12 run the ranked
    /// 12 size to weights from 8.0% down to 1.8%, and two of them fall outside the graded ten
    /// entirely. `screen::run` fills this from `size::sized_book`, the one spelling of that
    /// pipeline.
    ///
    /// NO PRICE HERE, and that is not an omission. Every sized ticker is already in `rows` with that
    /// day's close; a second copy of the same price is the drift non-negotiable #4 exists to stop,
    /// and `sized_rows` does the join. NO KNOB either — this is journal data, like `aum` (round 34)
    /// and `spx_off_hi`, both of which ship unconditionally. `core` is gated only because `(#103)`
    /// had already built `journal_core_list` before there was a grader for it.
    ///
    /// `skip_serializing_if` keeps a run that sizes nothing byte-identical to every line on disk;
    /// `default` keeps every older line readable.
    /// MEASURED 2026-09-12: these weights are scored on the SCREEN's own numbers. The rows its buy
    /// table marks `#` scored with live fundamentals, which `size --picks` never fetches — it re-derives
    /// the score price-only. On that run IITU.L read 6.3 here against 6.5 there, SMH.L 4.7 against 5.0,
    /// SEMI.AS 4.6 against 5.0, and the three uncapped ETF weights moved by ≤0.2pp as a result. The
    /// CLASS sums are identical (ETF 25.0% both ways) and no name enters or leaves, because the
    /// divergence redistributes inside one budget rather than changing what clears a gate or a cap.
    /// Journalling the screen's version is deliberate: it is the better-informed one and it is the one
    /// the printed buy table ranked on, so the record matches what the user was shown that day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sized: Vec<(String, f64)>,
    /// (#324) THE NOTCH COHORTS of the same run: `(ticker, close EUR, notch tag)` for every name one
    /// sweep notch would newly admit (`screen::notch_cohorts` over `picks::gate_notches`), not pinned,
    /// not crypto, one row per fund. (#323 journalled the narrow near-miss tail, a different set from
    /// the notch a reopen would ship, so it graded names no reopen would buy.) Every admission refusal on file was
    /// graded on the same backtest windows each round, and every "reopen if" waited on evidence the
    /// journal could not collect, because it only ever held the names that PASSED. This is that
    /// evidence: [`near_section`] grades these names against the book the line bought, per gate, on
    /// prices that did not exist when they were refused. The gate travels with the name because the
    /// reopen rule reads one gate at a time and a journal line cannot be backdated.
    /// Journal data, no knob, same serde contract as `sized`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub near: Vec<(String, Option<f64>, String)>,
    /// (#332) THE PEG DENOMINATOR SHADOW: `(ticker, served peg_yield, the same PEG re-priced on the
    /// pinned window)` for every ranked or notch name this run could price a PEG for.
    ///
    /// (#331) gave the PEG denominator its own window (`peg_cagr_years`) and REFUSED all three rungs
    /// on the backtest, shipping the knob inert at 0. That refusal has no forward half. `near` above
    /// shadows would-be ADMITS, and the split window admits nobody at any rung — it is structurally
    /// EVICT-ONLY, because `picks::long_leg_fixed` falls back to the age ladder for exactly the young
    /// names a pin would rescue. So the one question left open — *is a pinned PEG a better valuation
    /// number than an age-ladder one?* — was un-gradeable by every instrument on file.
    ///
    /// This is that instrument, and it does NOT grade admission. It grades RANKING POWER: split these
    /// names into a cheap half and a rich half under each denominator, hold both, and see which
    /// denominator's cheap half does better ([`peg_section`]). A name both windows agree on lands in
    /// the same half twice and cancels; only the names the pin RE-RANKS can move the verdict.
    ///
    /// NO PRICE HERE, for the reason `sized` states: `rows` and `near` already carry that day's close
    /// for every ticker this can name, and a second copy is the drift non-negotiable #4 exists to
    /// stop. [`journal_px`] does the join. NO KNOB either — journal data, like `aum` and `spx_off_hi`.
    /// Same serde contract as `sized`: a run that prices no PEG stays byte-identical to the lines
    /// already on disk, and every older line reads back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peg: Vec<(String, f64, f64)>,
    /// (#334) THE WEIGHT SWAP SHADOW: `(ticker, close EUR, weight notch, entrant?)` for every name one of
    /// `picks::weight_notches` would move across the top-[`BOOK`] edge that day — entrants in the notched
    /// order, then the names they displace. An equal-weight book feels a ranking weight only through those
    /// names, so `track` grades entrants against the displaced of the SAME line, a within-line contrast
    /// config churn cannot corrupt. The close rides on EVERY row, displaced included, though a displaced
    /// name usually sits in `rows` too: entrants can rank past `rows`, and `rows` carries coins the live
    /// rank excludes, so no join through `rows` holds. [`adjust_for_splits`] restates both copies in one
    /// pass, so they cannot drift. Same serde contract as `sized` and `peg`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub swap: Vec<(String, Option<f64>, String, bool)>,
    /// (#335) THE EXIT SHADOW: `(ticker, close EUR, still passes the gates?)` for every non-coin name the
    /// PREVIOUS month's graded book bought ([`prior_month`]) that this run's book no longer holds. The
    /// forward half of backtest's EXIT PROBE (Item 31): does a name the rules now refuse go on to lag the
    /// names the book kept? It has to be journalled on the day it happens — a name that left the book has
    /// no close in `rows` from then on, and grading it from its last in-book close would count the fall
    /// that pushed it out, which is look-ahead. Same serde contract as `sized`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit: Vec<(String, Option<f64>, bool)>,
    /// (#337) THE CARRY: `(ticker, close EUR)` for every name the PREVIOUS month's graded line
    /// ([`prior_month`]) grades that this line's `rows` and `exit` do not already price — [`carry`]
    /// builds it. Eight of the nine forward tables grade every line to TODAY, so their windows nest and
    /// their 12-line rules rest on about one effective trial. (#336) broke the nesting for the executed
    /// book by pricing month m's book at month m+1's line, which works because the book's names are in
    /// `rows` or `exit` a month later. The cohorts are not: the near-miss tail, the PEG and weight-swap
    /// shadows, the exits and the CORE list sit mostly outside `rows`, and a close nobody journalled on
    /// the day can never be backfilled. So this journals them now; the chained readers come later, from
    /// data that exists. Journal data, no knob, one copy per price (non-negotiable #4), same serde
    /// contract as `sized`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carry: Vec<(String, Option<f64>)>,
}

/// Append today's ranked slice — unless the journal already ends with this date (same-day rerun).
/// A write failure only costs one day of track record: warn, never fail the screen run.
pub fn append_snapshot(snap: &Snapshot) {
    let last_date = std::fs::read_to_string(crate::config::data_path(SNAPSHOT_FILE)).ok().and_then(|s| {
        s.lines().last().and_then(|l| serde_json::from_str::<Snapshot>(l).ok()).map(|s| s.date)
    });
    if last_date.as_deref() == Some(snap.date.as_str()) {
        return;
    }
    let appended = serde_json::to_string(snap).map(|json| {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::config::data_path(SNAPSHOT_FILE))
            .and_then(|mut f| writeln!(f, "{json}"))
    });
    if !matches!(appended, Ok(Ok(()))) {
        eprintln!("WARNING: could not append {SNAPSHOT_FILE} — today's ranking missing from the track record");
    }
}

/// Read + parse the snapshot journal: (snapshots in file order, corrupt line count). Shared by
/// `track`, `sim` and the screen's trust line so the three parse the record identically.
pub(crate) fn read_snapshots() -> (Vec<Snapshot>, usize) {
    let raw = std::fs::read_to_string(crate::config::data_path(SNAPSHOT_FILE)).unwrap_or_default();
    let mut corrupt = 0usize;
    let snaps = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).map_err(|_| corrupt += 1).ok())
        .collect();
    (snaps, corrupt)
}

/// (#82) Restate every journaled price into TODAY's share definition, in place, and return how many
/// rows moved. Call this once, right after the prices are fetched and BEFORE anything reads the
/// journal — then `grade`, `verdict_stats` and `sim`'s ledger all keep working on numbers that mean
/// what they say, with no signature of their own to change.
///
/// THE BUG THIS EXISTS FOR. `rows` holds the EUR price as it was quoted on the day. `px_now` comes
/// from a chart Yahoo has retro-adjusted for every split since. Comparing the two books a 10:1 split
/// as a permanent -90% — not a rounding error, a wrong sign, forever, in the one artefact whose whole
/// point is to be the honest live out-of-sample record. It flatters or wrecks the summary line the
/// `--push` ping sends, and `sim` bought at the same uncorrected price.
///
/// It corrects the journal in memory only. The file keeps the raw quoted price, deliberately: that is
/// what was true on the day, the correction depends on splits that had not happened yet, and rewriting
/// history would mean re-deriving it on every future split anyway. Restating on read is idempotent;
/// restating on disk is not.
///
/// `spx` is left alone — an index level is not a share and does not split.
///
/// (#285) It walks the CORE list too, now that there is one to grade. A tracker consolidating 10:1
/// is rarer than a stock splitting, but not rare enough to grade without: the failure is the same
/// wrong sign, forever, in the same artefact, and the correction is the same loop.
pub(crate) fn adjust_for_splits(snaps: &mut [Snapshot], factor_since: &dyn Fn(&str, chrono::NaiveDate) -> f64) -> usize {
    let mut restated = 0usize;
    for snap in snaps.iter_mut() {
        let Ok(then) = chrono::NaiveDate::parse_from_str(&snap.date, "%Y-%m-%d") else { continue };
        // (#323) and the near-miss tail, which `near_section` grades the same way
        let lanes = snap.rows.iter_mut().chain(snap.core.iter_mut()).map(|(t, p)| (&*t, p));
        // (#334) and the weight-swap shadow, both sides
        let lanes = lanes.chain(snap.swap.iter_mut().map(|(t, p, ..)| (&*t, p)));
        // (#335) and the exit shadow, which carries its own close for the same reason
        let lanes = lanes.chain(snap.exit.iter_mut().map(|(t, p, _)| (&*t, p)));
        // (#337) and the carry, which is nothing but closes
        let lanes = lanes.chain(snap.carry.iter_mut().map(|(t, p)| (&*t, p)));
        for (ticker, px) in lanes.chain(snap.near.iter_mut().map(|(t, p, _)| (&*t, p))) {
            let factor = factor_since(ticker, then);
            // `!= 1.0` and not an epsilon: a factor is a ratio of two small integers or it is the
            // empty product, so the no-split case is exactly 1.0 and never near it.
            if factor > 0.0 && factor != 1.0 {
                if let Some(p) = px {
                    *p /= factor;
                    restated += 1;
                }
            }
        }
    }
    restated
}

/// The `factor_since` closure [`adjust_for_splits`] wants, read off quotes the command already
/// fetched. One definition so `track`, `sim` and the screen's trust line cannot restate the same
/// journal three slightly different ways — an unknown ticker answers 1.0, the same as a known one
/// with no splits, because "we could not price it" and "it never split" both mean leave it alone.
pub(crate) fn split_factor_from(quotes: &[crate::core::Quote]) -> impl Fn(&str, chrono::NaiveDate) -> f64 + '_ {
    move |ticker, since| {
        quotes
            .iter()
            .find(|q| q.ticker == ticker)
            .map_or(1.0, |q| crate::core::split_factor_since(&q.splits, since))
    }
}

/// One graded journal row: equal-weight book return vs the index over the same window.
/// `priced` says how many of the book's names had a price on BOTH ends — delisted/err names drop
/// out, which FLATTERS the book (survivorship); the count keeps that visible.
struct Graded {
    date: String,
    days: i64,
    priced: usize,
    book_pct: f64,
    spy_pct: Option<f64>,
}

/// Grade one of a snapshot's lists against today's prices. `None` = nothing gradeable (too young,
/// empty list, no priced rows).
///
/// (#285) THE LIST AND ITS CUT ARE PARAMETERS, because there are three books to grade and only one
/// piece of arithmetic may exist for them (non-negotiable #4):
///
/// * the momentum book — `snap.rows` cut at [`BOOK`], which is what every surface graded before
///   `(#285)`, and (#322) what [`book_rows`] still grades for a line journalled before `sized` was;
/// * the CORE hold shortlist — `snap.core` cut at `Sizing::spill_cut()`, the number of trackers
///   `size` actually spills the undeployed remainder into;
/// * (#286) the EXECUTED book — [`sized_rows`], uncut, weighted as `size` weights it. (#296) The CORE
///   rows carry `size::spill_split`'s weights too, all exactly 1.0 unless a US row is funded.
///
/// (#286) EACH ROW CARRIES ITS WEIGHT and the fold is `Σ w·r / Σ w`. The two equal-weight lanes hand
/// `1.0` to every row through [`equal`], which does not merely approximate what they computed
/// before: `1.0 * x` is exactly `x` in IEEE754 and a sum of n ones is exactly n, so those two tables
/// print the identical float they printed when `grade` could only average. A second weighted fold
/// living beside this one is what #4 forbids, and it is also how two tables of the same book start
/// disagreeing.
///
/// `date` and `spx` still come off the snapshot, deliberately: the window and the benchmark leg are
/// properties of the RUN, not of which of that run's lists is being graded. That is also what makes
/// the tables comparable — same endpoints, same index, same convention, one function.
fn grade(
    snap: &Snapshot,
    rows: &[(&str, Option<f64>, f64)],
    cut: usize,
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> Option<Graded> {
    let then = chrono::NaiveDate::parse_from_str(&snap.date, "%Y-%m-%d").ok()?;
    let days = (today - then).num_days();
    if days < 1 {
        return None; // today's snapshot: zero-day window grades nothing
    }
    let rets: Vec<(f64, f64)> = rows
        .iter()
        .take(cut)
        .filter_map(|(t, px_then, w)| {
            let (then_px, now_px) = (px_then.filter(|p| *p > 0.0)?, px_now(t)?);
            Some((*w, now_px / then_px - 1.0))
        })
        .collect();
    if rets.is_empty() {
        return None;
    }
    // A book whose priced rows all weigh nothing is not a 0% book, it is an ungraded one — and the
    // division below would hand back a NaN that prints as a real number.
    let wsum: f64 = rets.iter().map(|(w, _)| w).sum();
    if wsum <= 0.0 {
        return None;
    }
    let book_pct = 100.0 * rets.iter().map(|(w, r)| w * r).sum::<f64>() / wsum;
    let spy_pct = snap.spx.filter(|p| *p > 0.0).zip(spx_now).map(|(then_px, now_px)| 100.0 * (now_px / then_px - 1.0));
    Some(Graded { date: snap.date.clone(), days, priced: rets.len(), book_pct, spy_pct })
}

/// (#286) An EQUAL-WEIGHT lane as [`grade`] wants it. The 1.0 is not a convention to be read as
/// "unweighted" — see `grade`'s doc for why it is exactly the pre-weight arithmetic.
fn equal(rows: &[(String, Option<f64>)]) -> Vec<(&str, Option<f64>, f64)> {
    rows.iter().map(|(t, p)| (t.as_str(), *p, 1.0)).collect()
}

/// (#286) The EXECUTED book as [`grade`] wants it: each sized ticker, the close `rows` journalled
/// for it on that run, and the weight `size` gave it.
///
/// THE JOIN IS THE POINT. `sized` deliberately carries no price, because `rows` already holds one
/// for every ticker it can name and two copies of one number drift (non-negotiable #4). A sized
/// ticker absent from `rows` — which nothing writes today, since both come off the same run's
/// `ranked_now` — lands here as `None` and `grade` drops it from the book and from N, the same way
/// it drops a delisted momentum row.
fn sized_rows(snap: &Snapshot) -> Vec<(&str, Option<f64>, f64)> {
    snap.sized
        .iter()
        .map(|(t, w)| (t.as_str(), snap.rows.iter().find(|(r, _)| r == t).and_then(|(_, p)| *p), *w))
        .collect()
}

/// (#322) THE book a run told the user to buy, and the one lane the verdict grades: the executed book
/// when the line journalled one, else — every line before (#286) — the top-[`BOOK`] equal-weight slice
/// the verdict always graded. It used to grade that slice even on lines carrying the executed book,
/// which since (#321) is up to 25 names at weights no top-10 shares, so the trust line under BUY NOW
/// was a claim about a different book. Uncut: [`grade`] takes `usize::MAX` for it.
fn book_rows(snap: &Snapshot) -> Vec<(&str, Option<f64>, f64)> {
    if snap.sized.is_empty() {
        equal(&snap.rows).into_iter().take(BOOK).collect()
    } else {
        sized_rows(snap)
    }
}

/// (#322) The same names as [`sized_rows`], re-weighted flat the way `sizing.equal_weight_book` pays
/// them: coins keep their journalled weight, every other name splits the rest equally. Graded beside
/// the executed book so a weighting rule can be judged against its flat twin on prices that did not
/// exist when it ranked — the forward half of the backtest's SIZED comparison.
fn flat_rows(snap: &Snapshot) -> Vec<(&str, Option<f64>, f64)> {
    let coins: Vec<(bool, f64)> = snap.sized.iter().map(|(t, w)| (crate::picks::is_currency_quoted(t), *w)).collect();
    let flat = crate::commands::size::equal_weights(&coins, usize::MAX, 1.0);
    sized_rows(snap).into_iter().zip(flat).filter_map(|((t, p, _), w)| w.map(|w| (t, p, w))).collect()
}

/// (#323) A line's notch cohorts as [`grade`] wants them, equal-weight: every notch, or just `gate`.
fn near_rows<'a>(snap: &'a Snapshot, gate: Option<&str>) -> Vec<(&'a str, Option<f64>, f64)> {
    snap.near.iter().filter(|(.., g)| gate.is_none_or(|w| w == g)).map(|(t, p, _)| (t.as_str(), *p, 1.0)).collect()
}

/// (#322) Every ticker `run` prices: each line's graded lanes and the benchmark. `sized` was graded
/// but never FETCHED — only `rows.take(BOOK)` and `core` were — so a bought name ranked past ten had
/// no price today and dropped out of the executed book silently (2 of 12 on the 2026-09-13 line;
/// up to 15 of 25 since (#321)). Sorted and deduped across the whole journal.
fn fetch_set(snaps: &[Snapshot]) -> Vec<String> {
    let mut tickers: Vec<String> = snaps
        .iter()
        // (#289) the WHOLE CORE list, not its first `core_cut` rows: which rows `size` funds is picked
        // by breadth tier, and the tier is read off the fund NAME, which arrives with the quote.
        .flat_map(|s| {
            s.rows.iter().take(BOOK).chain(&s.core).map(|(t, _)| t).chain(s.sized.iter().map(|(t, _)| t))
                .chain(s.near.iter().map(|(t, ..)| t)) // (#323)
                // (#332) the PEG cohort, which reaches past `rows.take(BOOK)` into the ranked tail.
                // Chained rather than lifting the take: this field IS the declaration of which names
                // that shadow grades, so the fetch set follows the journal instead of guessing wider.
                .chain(s.peg.iter().map(|(t, ..)| t))
                // (#334) the weight-swap shadow: an entrant can rank past `rows.take(BOOK)`
                .chain(s.swap.iter().map(|(t, ..)| t))
                // (#335) a name that left the book is in no other list that line wrote
                .chain(s.exit.iter().map(|(t, ..)| t))
        })
        .cloned()
        .chain(std::iter::once("^GSPC".to_string()))
        .collect();
    tickers.sort();
    tickers.dedup();
    tickers
}

/// The column header every table prints. (#285) One spelling, so the CORE block underneath cannot
/// drift out of alignment with the momentum block the first time a column width moves.
///
/// A CONST AND NOT A FUNCTION, and the mutation gate is why. As `fn table_header() -> String` it
/// built this same fixed string through a `format!` of six literals, which bought nothing at runtime
/// and cost a mutant: `replace table_header -> String with String::new()` SURVIVED the first push of
/// this round, because the test asserted `out.contains(&table_header())` and `contains("")` is true
/// of everything. The test was vacuous, but so was the function — the honest fix is to delete it
/// rather than to test around it, since a const has no return to replace. The literal is also the
/// alignment contract itself, which is what `graded_row`'s widths are chosen against.
const TABLE_HEADER: &str = "  DATE            AGE    N       BOOK    S&P 500    EXCESS  BEAT?";

/// One printed table row. (#285) Pulled out of `run`'s print loop so the CORE block renders through
/// the SAME formatter as the momentum block: a second `println!` carrying the same widths would be a
/// second definition of the table (non-negotiable #4), and the two would disagree the first time one
/// of them was edited. It also moves the row's only decision — whether the window BEAT the index —
/// out of the `#[mutants::skip]` entry point and into a function the gate can reach.
fn graded_row(g: &Graded) -> String {
    match g.spy_pct {
        Some(spy) => {
            let excess = g.book_pct - spy;
            format!(
                "  {:<12} {:>5}d {:>4} {:>+9.1}% {:>+9.1}% {:>+8.1}pp  {}",
                g.date, g.days, g.priced, g.book_pct, spy, excess,
                if excess > 0.0 { "yes" } else { "no" }
            )
        }
        None => format!(
            "  {:<12} {:>5}d {:>4} {:>+9.1}% {:>10} {:>9}  (no benchmark that day)",
            g.date, g.days, g.priced, g.book_pct, "n/a", "n/a"
        ),
    }
}

/// (#285) The CORE half of the report as a printable block: the buy-and-hold shortlist graded on the
/// same windows, against the same index, with the same arithmetic as the momentum table above it.
///
/// WHY IT EXISTS. `size` spills every point its caps could not deploy — routinely two thirds of
/// gross — over the first `spill_cut()` rows of the CORE list, and the screen calls that list the
/// twenty-year instrument. Nothing had ever recorded it: `journal_core_list` shipped OFF, so on the
/// author's own journal 13 of 13 lines carried the momentum book and 0 of 13 carried the CORE one.
/// The lane is also backtest-ungradeable BY CONSTRUCTION — `backtest::stamp_asset_class` fills name,
/// instrument type and sector only, so a reconstructed quote carries no TER and dies on the CORE
/// admission leg — which leaves this live journal as the only record of it that will ever exist.
///
/// THE EMPTY CASE PRINTS A SENTENCE AND NEVER A TABLE. An empty table with a 0.0% in it reads as a
/// measured result, and "nothing recorded" is the opposite of a measurement. It also says how many
/// lines carry a CORE list, because "the knob is off" and "the knob is on but the record is a day
/// old" are different problems with different fixes and only the user can tell them apart.
///
/// DELIBERATELY NOT FOLDED INTO `verdict_stats`. That fold is shared with the screen's trust line,
/// which is a claim about the book BUY NOW bought (#322); mixing the CORE list into it would silently
/// move a number two surfaces currently agree on, which is the exact drift its own doc exists to prevent.
/// (#286) One lane, graded across every journalled run: `(runs carrying this lane, runs, printed
/// rows)`. The arithmetic of "grade them all and format each" is shared; the WORDS are not, because
/// each lane owes the reader a different sentence about what it is and how it is weighted. Two
/// closures to push those sentences in here would be more code than the four lines they replace.
fn graded_rows(
    snaps: &[Snapshot],
    // (#289) higher-ranked, because one caller is now a CLOSURE over the tier lookup rather than a
    // plain fn item, and a closure cannot name the snapshot lifetime its own rows borrow from.
    rows_of: &dyn for<'a> Fn(&'a Snapshot) -> Vec<(&'a str, Option<f64>, f64)>,
    cut: usize,
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> (usize, usize, Vec<String>) {
    // no emptiness filter before `grade`: it already answers None for a list with no priced rows,
    // and a second guard saying the same thing is a second place to get it wrong.
    let rows = snaps
        .iter()
        .filter_map(|s| grade(s, &rows_of(s), cut, today, px_now, spx_now))
        .map(|g| graded_row(&g))
        .collect();
    (snaps.iter().filter(|s| !rows_of(s).is_empty()).count(), snaps.len(), rows)
}

/// (#289) …and it grades the rows `size` FUNDS, which is not the same thing as the first `cut` of
/// them. `size` selects through `screen::last_core`, which honours `sizing.spill_per_tier`; this
/// grader had its own `.take(cut)`, so with the knob on it measured three all-world trackers while
/// the book bought all-world + developed + emerging. A record that does not describe the book is the
/// defect `(#286)` exists to prevent, so both sides now call `screen::spill_picks` and there is one
/// definition of the selection (non-negotiable #4).
///
/// `tier_of` is passed the way `px_now` is, and for the same reason: the tier is a pure function of
/// the fund NAME, the names arrive with the quotes `run` already fetched, and re-deriving them here
/// would be a second source for one fact. A ticker with no quote answers `None` and is simply not
/// groupable — with every tier `None` the selection degrades to the old prefix, which is what a
/// journal line whose funds no longer quote must do rather than grade a market it guessed.
fn core_section(
    snaps: &[Snapshot],
    cut: usize,
    per_tier: bool,
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    tier_of: &dyn Fn(&str) -> Option<u8>,
    spx_now: Option<f64>,
) -> String {
    // the annotation is load-bearing: without the expected `for<'a>` type in front of it, inference
    // gives the closure two unrelated lifetimes and it cannot return rows borrowed from `s`.
    let funded: &dyn for<'a> Fn(&'a Snapshot) -> Vec<(&'a str, Option<f64>, f64)> = &|s| {
        let all = equal(&s.core);
        let tiers: Vec<Option<u8>> = all.iter().map(|(t, ..)| tier_of(t)).collect();
        let picked = crate::commands::screen::spill_picks(&tiers, cut, per_tier);
        // (#296) at the weights `size` pays them. A remainder of n keeps every non-US weight exactly 1.0.
        let w = crate::commands::size::spill_split(picked.len() as f64, &picked.iter().map(|&i| tiers[i]).collect::<Vec<_>>());
        picked.into_iter().zip(w).map(|(i, w)| (all[i].0, all[i].1, w)).collect()
    };
    let (journalled, total, rows) = graded_rows(snaps, funded, cut, today, px_now, spx_now);
    if rows.is_empty() {
        return format!(
            "\n  CORE hold shortlist: nothing gradeable yet. A line needs a day of age and at least one\n  \
             priced row before it grades, and only {journalled} of {total} journalled run(s) carry a CORE\n  \
             list at all. `journal_core_list` is what writes it; the record starts the run AFTER that is\n  \
             switched on, and cannot be backdated."
        );
    }
    let body = rows.join("\n");
    // (#289) "the first N" was true only while the selection was a prefix. It is not one with the
    // tilt on, and a blurb describing a different rule than the table used is the drift this file
    // already carries scars for.
    let how = if per_tier { ", one per market" } else { ", the broadest first" };
    format!(
        "\n  CORE hold shortlist — the buy-and-hold half of the report, graded the same way. Each row is\n  \
         the {cut} name(s) of that run's CORE list that `size` funds{how}: what it spills the remainder\n  \
         its caps could not deploy into, which is routinely two thirds of gross. Weighted as `size`\n  \
         pays them (a US row 2x, the rest equal), EUR seat, price-only, same windows as above. NOT advice.\n  \
         Journalled on {journalled} of {total} run(s).\n\n{TABLE_HEADER}\n{body}"
    )
}

/// (#322) The third table: the executed book's FLAT TWIN. The verdict above now grades the book `size`
/// funded, weighted as it funded it ([`book_rows`]); this grades the same names on the same windows at
/// the weights `sizing.equal_weight_book` would have paid them. The gap between the two is what the
/// weighting rule earned or cost out of sample — identical rows mean the book was already flat.
///
/// (#286) it was the executed book itself, printed apart from a verdict that graded the ranked top-10.
/// With the verdict on the executed book that table would print the same numbers twice.
fn sized_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    // usize::MAX, not a cut: the sizing IS the cut. A row that reached this list already survived
    // the gate, the issuer dedup and the book width, and dropping its tail would grade a book nobody holds.
    let (journalled, total, rows) = graded_rows(snaps, &flat_rows, usize::MAX, today, px_now, spx_now);
    if rows.is_empty() {
        return format!(
            "\n  Flat twin of the executed book: nothing gradeable yet. A line needs a day of age and at least\n  \
             one priced row before it grades, and only {journalled} of {total} journalled run(s) carry an\n  \
             executed book at all. The record starts the run AFTER one is journalled and cannot be backdated."
        );
    }
    let body = rows.join("\n");
    format!(
        "\n  Flat twin — the SAME names each run's executed book bought, re-weighted flat: coins keep their\n  \
         weight, every other name splits the rest equally. Set it against the verdict table's dated rows:\n  \
         the gap is what the book's weighting earned or cost on prices that did not exist when it ranked.\n  \
         Scored on the SCREEN's numbers, which use live fundamentals. EUR seat, price-only returns, same\n  \
         windows. NOT advice. Journalled on {journalled} of {total} run(s).\n\n{TABLE_HEADER}\n{body}"
    )
}

/// (#323) Monthly lines a gate's shadow needs before its verdict can say anything but "wait": a year,
/// written before the first line was journalled. The receipt holds the rule; this is its clock.
const REOPEN_LINES: usize = 12;

/// (#323) The pre-registered reading of one gate's forward record. `lines` monthly gaps, shadow minus
/// the bought book: a REOPEN SIGNAL needs the full year AND both the mean and the median strictly
/// above zero — the mean alone is one survivor's, the median alone ignores how much a winner won.
pub(crate) fn reopen_verdict(lines: usize, mean: f64, median: f64) -> &'static str {
    match clears(lines, mean, median) {
        None => "needs 12 lines",
        Some(true) => "REOPEN SIGNAL",
        Some(false) => "holds",
    }
}

/// (#336) The rule under [`reopen_verdict`], spelled once so [`chain_section`] reads the same year and
/// the same two statistics in its own words: `None` until [`REOPEN_LINES`], then mean AND median > 0.
fn clears(lines: usize, mean: f64, median: f64) -> Option<bool> {
    (lines >= REOPEN_LINES).then_some(mean > 0.0 && median > 0.0)
}

/// (#323) Per gate: `(gate, monthly lines, mean gap, median gap)`, where a gap is that gate's
/// near-miss shadow minus the book the SAME line bought ([`book_rows`]), both graded to today. One
/// line per month (`sim::monthly_firsts`, which prefers a line carrying an executed book), because
/// `track` grades every line against one endpoint and a same-week rerun is not a second trial. A line
/// whose book or shadow cannot grade contributes nothing — a missing gap is not a zero one. Median is
/// nearest-rank (`backtest::percentile`), the repo's one rule.
fn gate_verdicts(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> Vec<(String, usize, f64, f64)> {
    let mut gaps: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
    for s in crate::commands::sim::monthly_firsts(snaps).into_values() {
        let Some(book) = grade(s, &book_rows(s), usize::MAX, today, px_now, spx_now) else { continue };
        let gates: std::collections::BTreeSet<&str> = s.near.iter().map(|(.., g)| g.as_str()).collect();
        for g in gates {
            if let Some(n) = grade(s, &near_rows(s, Some(g)), usize::MAX, today, px_now, spx_now) {
                gaps.entry(g).or_default().push(n.book_pct - book.book_pct);
            }
        }
    }
    gaps.into_iter()
        .map(|(g, mut v)| {
            v.sort_by(f64::total_cmp);
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            (g.to_string(), v.len(), mean, crate::commands::backtest::percentile(&v, 50.0))
        })
        .collect()
}

/// (#323) The fourth table: the NOTCH SHADOW (#324: what each one-notch loosening would have added). Held
/// equal-weight on the same windows as the book it bought, then read gate by gate against that book.
/// This is the forward half of the backtest's NOTCH BOOK rows, and the only out-of-sample
/// evidence an admission refusal can ever be re-opened on: every refusal on file was graded on the
/// same backtest windows each round, which cannot disagree with themselves.
fn near_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let pooled: &dyn for<'a> Fn(&'a Snapshot) -> Vec<(&'a str, Option<f64>, f64)> = &|s| near_rows(s, None);
    let (journalled, total, rows) = graded_rows(snaps, pooled, usize::MAX, today, px_now, spx_now);
    if rows.is_empty() {
        return format!(
            "\n  Notch shadow: nothing gradeable yet. A line needs a day of age and at least one priced\n  \
             row before it grades, and only {journalled} of {total} journalled run(s) carry notch cohorts.\n  \
             The record starts the run AFTER one is journalled and cannot be backdated."
        );
    }
    let gates: String = gate_verdicts(snaps, today, px_now, spx_now)
        .iter()
        .map(|(g, n, mean, med)| {
            format!("\n    {g:<10} {n:>3} line(s)  mean {mean:>+7.1}pp  median {med:>+7.1}pp  {}", reopen_verdict(*n, *mean, *med))
        })
        .collect();
    let body = rows.join("\n");
    // (#330) the deciding basket, read from the one constant the backtest publishes it at, so the two
    // surfaces cannot state different rules.
    let top = crate::commands::backtest::VERDICT_TOP;
    format!(
        "\n  Notch shadow — the names each run would have added had ONE gate loosened one sweep notch (the\n  \
         screen's notch lines), equal-weight, same windows. A cohort that keeps beating the bought book is\n  \
         a gate refusing winners. EUR seat, price-only. NOT advice. Journalled on {journalled} of {total} run(s).\n\n\
         {TABLE_HEADER}\n{body}\n\n  \
         Per notch, one line a month, cohort minus the book that line bought. Receipt (#324), bar amended\n  \
         by (#329) and basket by (#330): that notch ships when {REOPEN_LINES}+ lines read mean AND median\n  \
         above 0 AND its backtest NOTCH BOOK row reads `ship pass` at the top-{top} basket — the\n  \
         one the journaled verdict publishes, where a cohort name must OUTRANK an incumbent to be bought —\n  \
         holding excess at or above that column's `bar`, the p95 of same-size cohorts drawn at random from\n  \
         the refused pool, and worst within 1.0 of cleared, at 20y, 12y and 8y. The old flat 0.1 was read\n  \
         off the union, so it asked -12.8 of a 2-name cohort and +4.4 of a 305-name one; the band asks the\n  \
         same of both. The uncapped column it was read on charged every admit at full weight, so any\n  \
         below-mean cohort had to drag it — arithmetic, not evidence; `ship inert` is a cohort the\n  \
         published book never buys at all.{gates}"
    )
}

/// (#332) The pinned denominator window this journal shadows: `peg_cagr_years: 10`.
///
/// PRE-REGISTERED, and fixed before the first line accrued. (#331) graded rungs 5, 8 and 10 and
/// refused all three; rung 10 is the one its receipt names as the reopen — the only arm genuinely UP
/// at 8y on book CAGR, excess, OOS and rank-1, dead on a single 12y median over 36 windows. Journalling
/// all three and reopening on whichever won afterwards is the tune-after-the-fact trap (#277)/(#278)
/// exist to stop, so the record carries ONE rung and the choice cannot be revisited once evidence is in.
pub const PEG_PIN_YEARS: u32 = 10;

/// (#332) That day's close for a journalled ticker, from whichever list already carries one. `peg`
/// holds no price of its own: its names are drawn from `rows` ∪ `near`, and both of those do.
fn journal_px(snap: &Snapshot, ticker: &str) -> Option<f64> {
    snap.rows
        .iter()
        .find(|(t, _)| t == ticker)
        .and_then(|(_, p)| *p)
        .or_else(|| snap.near.iter().find(|(t, ..)| t == ticker).and_then(|(_, p, _)| *p))
}

/// (#332) One half of one denominator's split, as [`grade`] wants it: equal-weight, priced from the
/// close the same line journalled.
///
/// HIGH `peg_yield` IS CHEAP — the field is `100/PEG`, the same inversion `growth_max_peg` compares
/// against — so the cheap half is the TOP of a descending sort. Ties break on ticker, so the split is
/// deterministic rather than dependent on journal order.
///
/// AN ODD COHORT DROPS ITS MEDIAN NAME FROM BOTH HALVES rather than lengthening one of them. The two
/// halves must be the same size or the comparison starts pricing cohort SIZE, which is precisely the
/// defect (#329) spent a round removing from the notch bar.
fn peg_rows(snap: &Snapshot, pinned: bool, cheap: bool) -> Vec<(&str, Option<f64>, f64)> {
    let mut ranked: Vec<(&str, f64)> =
        snap.peg.iter().map(|(t, ladder, pin)| (t.as_str(), if pinned { *pin } else { *ladder })).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let half = ranked.len() / 2;
    let slice = if cheap { &ranked[..half] } else { &ranked[ranked.len() - half..] };
    slice.iter().map(|(t, _)| (*t, journal_px(snap, t), 1.0)).collect()
}

/// (#332) One line's four half-returns: `(graded leg, ladder cheap, ladder rich, pin cheap, pin rich)`.
/// None unless ALL FOUR halves grade — a line that can price three of them describes no comparison.
fn peg_halves(
    snap: &Snapshot,
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> Option<(Graded, f64, f64, f64, f64)> {
    let g = |pinned, cheap| grade(snap, &peg_rows(snap, pinned, cheap), usize::MAX, today, px_now, spx_now);
    let (lc, lr, pc, pr) = (g(false, true)?, g(false, false)?, g(true, true)?, g(true, false)?);
    let pcts = (lc.book_pct, lr.book_pct, pc.book_pct, pr.book_pct);
    Some((lc, pcts.0, pcts.1, pcts.2, pcts.3))
}

/// (#332) One printed row of the PEG shadow. `N` is the WHOLE cohort, not one half: the halves are
/// each `N/2` by construction and printing that instead would understate what the line graded.
fn peg_row(snap: &Snapshot, g: &Graded, lc: f64, lr: f64, pc: f64, pr: f64) -> String {
    format!(
        "  {:<12} {:>5}d {:>4} {:>+12.1}% {:>+11.1}% {:>+11.1}% {:>+10.1}% {:>+12.1}pp",
        g.date,
        g.days,
        snap.peg.len(),
        lc,
        lr,
        pc,
        pr,
        pc - lc
    )
}

/// (#332) The deciding statistic's record: `(monthly lines, mean, median)` of the pinned cheap half
/// minus the ladder cheap half. One line a month (`sim::monthly_firsts`, the same clock
/// [`gate_verdicts`] runs on, because a same-week rerun is not a second trial).
///
/// THE CHEAP HALF IS THE ONLY HALF THE BOOK EVER BUYS. A spread (cheap minus rich) would pay the pin
/// for sorting names the tool is never long, so a denominator that merely dumps losers harder would
/// read as an improvement the book never collects. Both halves grade over the SAME window to the same
/// endpoint, so the market leg cancels with no spread needed to control for it — the benchmark does
/// not enter this arithmetic at all. A line that cannot grade contributes nothing; a missing gap is
/// not a zero one. Median is nearest-rank (`backtest::percentile`), the repo's one rule.
fn peg_verdict(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> (usize, f64, f64) {
    let mut gaps: Vec<f64> = crate::commands::sim::monthly_firsts(snaps)
        .into_values()
        .filter_map(|s| peg_halves(s, today, px_now, spx_now).map(|(_, lc, _, pc, _)| pc - lc))
        .collect();
    gaps.sort_by(f64::total_cmp);
    if gaps.is_empty() {
        return (0, 0.0, 0.0);
    }
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    (gaps.len(), mean, crate::commands::backtest::percentile(&gaps, 50.0))
}

/// (#332) The fifth table: THE PEG DENOMINATOR SHADOW — which denominator is the better valuation
/// number, graded on prices that did not exist when either ranked.
///
/// WHY IT EXISTS. (#331) split the PEG denominator from the ranking leg (`peg_cagr_years`) and refused
/// rungs 5, 8 and 10, shipping the knob inert at 0. Nothing could grade that refusal forward. The
/// notch shadow above is the forward half of an ADMISSION refusal and journals would-be admits, and
/// this knob admits nobody at any rung: it is structurally EVICT-ONLY, because `picks::long_leg_fixed`
/// falls back to the age ladder for exactly the young names a pin would rescue. So the question the
/// round actually asked — is a PEG divided by a PINNED window a better valuation number than one
/// divided by whatever rung the AGE ladder happened to pick — had no out-of-sample instrument at all.
///
/// IT GRADES RANKING, NOT ADMISSION, and the distinction is the point. Every name here is ranked by
/// each denominator, split at the median into a cheap half and a rich half, and both halves are held
/// equal-weight over the same window. A name the two windows agree on lands in the same half twice and
/// cancels out; only the names the pin RE-RANKS can move the verdict. The cohort spans the ranked book
/// AND the notch names, so the PEG range is the full one rather than the narrow band left under the
/// ceiling — and because both denominators score the IDENTICAL name set, any bias in that set is
/// common-mode and cancels in the difference.
fn peg_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let rows: Vec<String> = snaps
        .iter()
        .filter_map(|s| peg_halves(s, today, px_now, spx_now).map(|(g, lc, lr, pc, pr)| peg_row(s, &g, lc, lr, pc, pr)))
        .collect();
    let journalled = snaps.iter().filter(|s| !s.peg.is_empty()).count();
    let total = snaps.len();
    if rows.is_empty() {
        return format!(
            "\n  PEG denominator shadow: nothing gradeable yet. A line needs a day of age and at least one\n  \
             priced name in each half before it grades, and only {journalled} of {total} journalled run(s)\n  \
             carry a PEG cohort. The record starts the run AFTER one is journalled and cannot be backdated."
        );
    }
    let (n, mean, med) = peg_verdict(snaps, today, px_now, spx_now);
    let body = rows.join("\n");
    format!(
        "\n  PEG denominator shadow — the SAME names ranked by two PEGs: the shipped one, whose growth term\n  \
         is whatever rung the AGE ladder picked, and the same PEG re-priced on a pinned {PEG_PIN_YEARS}Y window\n  \
         (`peg_cagr_years`, graded and refused by (#331), shipping inert at 0). Each denominator splits the\n  \
         cohort at its median into a cheap half and a rich half, held equal-weight over the same window. A\n  \
         name both windows agree on cancels; only re-ranked names move this. Ranked book + notch names, EUR\n  \
         seat, price-only. NOT advice. Journalled on {journalled} of {total} run(s).\n\n\
         {PEG_HEADER}\n{body}\n\n  \
         Verdict, one line a month: PIN CHEAP minus LADDER CHEAP, {n} line(s), mean {mean:+.1}pp, median\n  \
         {med:+.1}pp — {}. The cheap half is the ONLY half this tool is ever long, so the spread against the\n  \
         rich half is reported but does NOT decide: a denominator that merely dumps losers harder earns the\n  \
         book nothing. Pre-registered by (#332) before the first line accrued: `peg_cagr_years: {PEG_PIN_YEARS}`\n  \
         re-opens when {REOPEN_LINES}+ monthly lines read mean AND median above 0 AND a re-run backtest passes\n  \
         Ship Rule v2 PRIMARY at 12y AND 8y on the pit lane — the exact leg rung {PEG_PIN_YEARS} failed on, where a 12y\n  \
         median of 36 windows fell while its mean rose. Both halves grade to one shared endpoint, so this\n  \
         record's n_eff is <= 1 however many lines accrue, the same ceiling `effective_trials` states below.",
        reopen_verdict(n, mean, med)
    )
}

/// (#332) The PEG shadow's column header. A const for the reason [`TABLE_HEADER`] is one: it IS the
/// alignment contract [`peg_row`]'s widths are chosen against, and a `format!` of literals would buy
/// nothing at runtime while handing the mutation gate a free survivor.
const PEG_HEADER: &str =
    "  DATE            AGE    N  LADDER CHEAP  LADDER RICH    PIN CHEAP     PIN RICH   PIN-LADDER";

/// (#334) One side of one weight notch's swap, as [`grade`] wants it: equal-weight, priced from the
/// close the swap row journalled itself.
fn swap_rows<'a>(snap: &'a Snapshot, notch: &str, entrant: bool) -> Vec<(&'a str, Option<f64>, f64)> {
    snap.swap.iter().filter(|(_, _, n, e)| n == notch && *e == entrant).map(|(t, p, ..)| (t.as_str(), *p, 1.0)).collect()
}

/// (#334) Per weight notch: `(notch, monthly lines, mean gap, median gap)`, a gap being the names that
/// notch would move INTO the top-[`BOOK`] minus the names it would push OUT, both graded to today on
/// the SAME line. [`gate_verdicts`]' twin, read the same way: one line a month, a side that cannot
/// grade contributes nothing (a missing gap is not a zero one), nearest-rank median.
fn swap_verdicts(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> Vec<(String, usize, f64, f64)> {
    let mut gaps: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
    for s in crate::commands::sim::monthly_firsts(snaps).into_values() {
        let notches: std::collections::BTreeSet<&str> = s.swap.iter().map(|(_, _, n, _)| n.as_str()).collect();
        for n in notches {
            let side = |entrant| grade(s, &swap_rows(s, n, entrant), usize::MAX, today, px_now, spx_now);
            if let (Some(i), Some(o)) = (side(true), side(false)) {
                gaps.entry(n).or_default().push(i.book_pct - o.book_pct);
            }
        }
    }
    gaps.into_iter()
        .map(|(n, mut v)| {
            v.sort_by(f64::total_cmp);
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            (n.to_string(), v.len(), mean, crate::commands::backtest::percentile(&v, 50.0))
        })
        .collect()
}

/// (#334) The sixth table: THE WEIGHT-SWAP SHADOW, the forward half of backtest's WEIGHT SWEEP. Every
/// gate had a forward instrument ([`near_section`]); the RANKING weights had none, so a mis-set weight
/// could only ever be graded on the backtest windows it was tuned on. An equal-weight top-N book is
/// set-invariant, so a weight is read at the book's edge: the names its x0 / x2 notch would swap in
/// against the names they would swap out. INFORM ONLY — a flag earns a full A/B round, never a move.
fn swap_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let journalled = snaps.iter().filter(|s| !s.swap.is_empty()).count();
    let total = snaps.len();
    let verdicts = swap_verdicts(snaps, today, px_now, spx_now);
    if verdicts.is_empty() {
        return format!(
            "\n  Weight-swap shadow: nothing gradeable yet. A line needs a day of age and a priced name on\n  \
             both sides of a swap, and only {journalled} of {total} journalled run(s) carry swaps. The record\n  \
             starts the run AFTER one is journalled and cannot be backdated."
        );
    }
    let body: String = verdicts
        .iter()
        .map(|(n, k, mean, med)| {
            format!("\n    {n:<28} {k:>3} line(s)  mean {mean:>+7.1}pp  median {med:>+7.1}pp  {}", reopen_verdict(*k, *mean, *med))
        })
        .collect();
    format!(
        "\n  Weight-swap shadow (#334) — each shipped ranking weight x0 / x2: the names that notch would move\n  \
         INTO the top-{BOOK} against the names it would push OUT, equal-weight, same windows. One line a\n  \
         month, entrants minus displaced. Pre-registered: a notch flags REOPEN SIGNAL at {REOPEN_LINES}+ lines\n  \
         with mean AND median above 0. INFORM ONLY: a flag earns a full A/B round, never a weight move, and\n  \
         16 notches read at once will throw a false flag by chance. EUR seat, price-only. NOT advice.\n  \
         Journalled on {journalled} of {total} run(s).{body}"
    )
}

/// (#335) One side of a within-line comparison, as [`grade`] wants it.
type Rows<'a> = Vec<(&'a str, Option<f64>, f64)>;

/// (#335) Per label: `(label, monthly lines, mean gap, median gap)`, a gap being one line's second list
/// minus its first, both graded to today. [`swap_verdicts`]' fold for any pair of lists a line can
/// name: one line a month, a side that cannot grade contributes nothing (a missing gap is not a zero
/// one), nearest-rank median.
fn monthly_gaps<'a>(
    snaps: &'a [Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
    pairs: &dyn Fn(&'a Snapshot) -> Vec<(&'static str, Rows<'a>, Rows<'a>)>,
) -> Vec<(String, usize, f64, f64)> {
    let mut gaps: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
    for s in crate::commands::sim::monthly_firsts(snaps).into_values() {
        for (label, hi, lo) in pairs(s) {
            let side = |rows: &[(&str, Option<f64>, f64)]| grade(s, rows, usize::MAX, today, px_now, spx_now);
            if let (Some(h), Some(l)) = (side(&hi), side(&lo)) {
                gaps.entry(label).or_default().push(l.book_pct - h.book_pct);
            }
        }
    }
    gaps.into_iter()
        .map(|(g, mut v)| {
            v.sort_by(f64::total_cmp);
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            (g.to_string(), v.len(), mean, crate::commands::backtest::percentile(&v, 50.0))
        })
        .collect()
}

/// (#335) The executed book in RANK order, equal-weight: `rows` (every ranked name, 25 on the
/// 2026-09-23 line) kept to the names `sized` bought, so an unbought pin drops, minus coins, which a
/// class budget sizes rather than a rank. A line from before `sized` keeps all of `rows`.
fn ladder(snap: &Snapshot) -> Rows<'_> {
    equal(&snap.rows)
        .into_iter()
        .filter(|(t, ..)| !crate::picks::is_currency_quoted(t))
        .filter(|(t, ..)| snap.sized.is_empty() || snap.sized.iter().any(|(s, _)| s == t))
        .collect()
}

/// (#335) The seventh table: THE RANK LADDER, the forward half of backtest's top-N ladder and rank-1
/// head-to-head. The book since (#321)/(#322) is 25 names with the first `size::HEAD` paid double, and
/// both of those rest on in-sample rank slices only. Every line already journals the whole ranked book
/// in order, so this needs no new field and grades every line on disk.
fn ladder_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let head = crate::commands::size::HEAD;
    let gaps = monthly_gaps(snaps, today, px_now, spx_now, &|s| {
        let l = ladder(s);
        let at = |i: usize| i.min(l.len());
        vec![
            ("6-10 minus 1-5", l[..at(head)].to_vec(), l[at(head)..at(BOOK)].to_vec()),
            ("11-25 minus 1-10", l[..at(BOOK)].to_vec(), l[at(BOOK)..].to_vec()),
        ]
    });
    if gaps.is_empty() {
        return format!(
            "\n  Rank ladder: nothing gradeable yet. A line needs a day of age and a priced name on both sides\n  \
             of a rank cut, and none of the {} journalled run(s) has one yet.",
            snaps.len()
        );
    }
    let body: String = gaps
        .iter()
        .map(|(g, n, mean, med)| {
            format!("\n    {g:<22} {n:>3} line(s)  mean {mean:>+7.1}pp  median {med:>+7.1}pp  {}", reopen_verdict(*n, *mean, *med))
        })
        .collect();
    format!(
        "\n  Rank ladder (#335) — each run's executed book cut by rank: 1-{head} (the names `head_weight` pays\n  \
         double), {}-{BOOK}, and the tail past {BOOK}. Coins and unbought pins out, each slice equal-weight on\n  \
         the SAME line, same windows, one line a month, lower slice minus higher. Pre-registered: a gap\n  \
         flags REOPEN SIGNAL at {REOPEN_LINES}+ lines with mean AND median above 0. `6-10 minus 1-5` re-opens\n  \
         (#322) `head_weight`, the tilt paying the wrong five (in-sample twin: backtest's rank-1 vs 6-10\n  \
         head-to-head); `11-25 minus 1-10` re-opens (#321)'s width, the rank carrying nothing past ten (twin:\n  \
         the top-N ladder). A flag earns an A/B round, never a move. EUR seat, price-only. NOT advice.{body}",
        head + 1
    )
}

/// (#335) The line `track` grades for the latest calendar month BEFORE `date`'s — `sim::monthly_firsts`'
/// pick, so the book `screen` diffs against when it journals `exit` and the book [`exit_section`] reads
/// the KEPT names from are one line by construction. None with no earlier month or an unparseable date.
fn prior_month<'a>(snaps: &'a [Snapshot], date: &str) -> Option<&'a Snapshot> {
    use chrono::Datelike;
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    crate::commands::sim::monthly_firsts(snaps).range(..(d.year(), d.month())).next_back().map(|(_, s)| *s)
}

/// (#335) The names `prev`'s book bought that `book_now` no longer holds, in `prev`'s order, each with
/// what `look` reads today: `(close EUR, still passes the gates?)`. Coins are out: the live rank never
/// passes one, so every coin would read as FAILED.
pub(crate) fn exits(prev: Option<&Snapshot>, book_now: &[String], look: &dyn Fn(&str) -> (Option<f64>, bool)) -> Vec<(String, Option<f64>, bool)> {
    prev.map(book_rows)
        .unwrap_or_default()
        .into_iter()
        .filter(|(t, ..)| !crate::picks::is_currency_quoted(t) && !book_now.iter().any(|b| b == t))
        .map(|(t, ..)| {
            let (px, passes) = look(t);
            (t.to_string(), px, passes)
        })
        .collect()
}

/// (#337) Today's close for every ticker `prev`'s line grades ([`fetch_set`], so the carry and what
/// `track` prices cannot drift) that `today`'s `rows` and `exit` do not already price. The benchmark
/// is out: every line carries `spx`. Sorted, one row per ticker; no `prev`, nothing to carry.
pub(crate) fn carry(prev: Option<&Snapshot>, today: &Snapshot, look: &dyn Fn(&str) -> Option<f64>) -> Vec<(String, Option<f64>)> {
    let priced = |t: &str| today.rows.iter().any(|(x, _)| x == t) || today.exit.iter().any(|(x, ..)| x == t);
    fetch_set(prev.map(std::slice::from_ref).unwrap_or_default())
        .into_iter()
        .filter(|t| t != "^GSPC" && !priced(t))
        .map(|t| {
            let px = look(&t);
            (t, px)
        })
        .collect()
}

/// (#335) The journal's [`prior_month`] line for `date`, read off disk. `screen` calls it BEFORE
/// appending today's line, which is why it cannot reuse the read `screen` makes after the append. That
/// read is restated for splits and pinned to one call (screen's `the_journal_is_read_once_and_restated_once`)
/// because its footers read PRICES; this line only feeds [`exits`] and (#337) [`carry`], which take its
/// TICKERS and price them with today's close, so nothing un-restated leaves it. `#[mutants::skip]`: a
/// file read wired to a tested fn, reachable only from `screen::run`.
#[mutants::skip]
pub(crate) fn journal_prior(date: &str) -> Option<Snapshot> {
    let (snaps, _) = read_snapshots();
    prior_month(&snaps, date).cloned()
}

/// (#335) The gap that decides the exit shadow; the other one prints for information.
const EXIT_DECIDES: &str = "kept minus failed";

/// (#335) The eighth table: THE EXIT SHADOW, the forward half of backtest's EXIT PROBE (Item 31). Each
/// month, the names the previous month's book held and this month's book KEPT, against the ones it
/// dropped: FAILED (no longer pass a gate) and OUT-RANKED (still pass, lost the seat), all graded from
/// the flip line. Kept minus failed above 0 = the refused names went on to lag the held ones, so
/// holding through a gate failure costs money and a sell rule re-opens.
fn exit_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let gaps = monthly_gaps(snaps, today, px_now, spx_now, &|s| {
        let before: Vec<&str> = prior_month(snaps, &s.date).map(book_rows).unwrap_or_default().into_iter().map(|(t, ..)| t).collect();
        let kept: Rows = book_rows(s)
            .into_iter()
            .filter(|(t, ..)| !crate::picks::is_currency_quoted(t) && before.contains(t))
            .map(|(t, p, _)| (t, p, 1.0))
            .collect();
        let left = |passes: bool| s.exit.iter().filter(|e| e.2 == passes).map(|(t, p, _)| (t.as_str(), *p, 1.0)).collect::<Vec<_>>();
        vec![(EXIT_DECIDES, left(false), kept.clone()), ("kept minus out-ranked", left(true), kept)]
    });
    let journalled = snaps.iter().filter(|s| !s.exit.is_empty()).count();
    if gaps.is_empty() {
        return format!(
            "\n  Exit shadow: nothing gradeable yet. A line needs a day of age, a book the month before, and a\n  \
             priced name on both sides, and only {journalled} of {} journalled run(s) carry exits. The record\n  \
             starts the run AFTER one is journalled and cannot be backdated.",
            snaps.len()
        );
    }
    let body: String = gaps
        .iter()
        .map(|(g, n, mean, med)| {
            let verdict = if g == EXIT_DECIDES { reopen_verdict(*n, *mean, *med) } else { "inform only" };
            format!("\n    {g:<22} {n:>3} line(s)  mean {mean:>+7.1}pp  median {med:>+7.1}pp  {verdict}")
        })
        .collect();
    format!(
        "\n  Exit shadow (#335) — each month, the names last month's book held that this month's KEPT, against\n  \
         the ones it dropped: FAILED no longer pass a gate, OUT-RANKED still pass but lost the seat. Graded\n  \
         from the day they left, equal-weight, one line a month. Pre-registered: `{EXIT_DECIDES}` flags\n  \
         REOPEN SIGNAL at {REOPEN_LINES}+ lines with mean AND median above 0 — the refused names went on to\n  \
         lag the held ones, so a sell-on-fail rule re-opens (Item 31; in-sample twin: backtest's EXIT PROBE).\n  \
         A flag earns an A/B round, never a move. EUR seat, price-only. NOT advice. Journalled on\n  \
         {journalled} of {} run(s).{body}",
        snaps.len()
    )
}

/// (#336) Each month's executed book held to the NEXT month's graded line: `(that line's date, names
/// held, graded)`
/// per consecutive pair of `sim::monthly_firsts`. The end prices come off the later line itself —
/// `rows` for the names it still ranks, (#335)'s `exit` for the ones its book dropped, which is the
/// whole of the earlier book bar a coin that left `rows` — so the windows follow one another instead of
/// all ending today, and nothing is fetched. [`grade`] does the arithmetic, unpriced names and all.
fn chain_links(snaps: &[Snapshot]) -> Vec<(String, usize, Graded)> {
    let firsts: Vec<&Snapshot> = crate::commands::sim::monthly_firsts(snaps).into_values().collect();
    firsts
        .windows(2)
        .filter_map(|w| {
            let (m, n) = (w[0], w[1]);
            let end = chrono::NaiveDate::parse_from_str(&n.date, "%Y-%m-%d").ok()?;
            let px = |t: &str| {
                n.rows.iter().map(|(x, p)| (x, *p)).chain(n.exit.iter().map(|(x, p, _)| (x, *p))).find(|(x, _)| *x == t)?.1.filter(|p| *p > 0.0)
            };
            let book = book_rows(m);
            grade(m, &book, usize::MAX, end, &px, n.spx.filter(|p| *p > 0.0)).map(|g| (n.date.clone(), book.len(), g))
        })
        .collect()
}

/// (#336) The ninth table: THE CHAINED RECORD. The verdict table grades every line to today, so its
/// windows nest on one endpoint and a year of them is ~1 trial (`trials_note`); these links do not
/// overlap, so twelve are twelve. Inform only: the trust line and the push still read the table.
fn chain_section(snaps: &[Snapshot]) -> String {
    let links = chain_links(snaps);
    if links.is_empty() {
        return format!(
            "\n  Chained record: nothing gradeable yet. A link needs two months' graded lines with a priced\n  \
             book name, and the {} journalled run(s) do not span two months yet.",
            snaps.len()
        );
    }
    let body: String = links
        .iter()
        .map(|(to, held, g)| {
            let pct = |v: Option<f64>| v.map_or("     —".to_string(), |v| format!("{v:>+6.1}"));
            let excess = g.spy_pct.map(|s| g.book_pct - s);
            format!(
                "\n    {}  {to}  {:>4}  {}  {}  {}  {:>2}/{}",
                g.date, g.days, pct(Some(g.book_pct)), pct(g.spy_pct), pct(excess), g.priced, held
            )
        })
        .collect();
    let both: Vec<(f64, f64)> = links.iter().filter_map(|(.., g)| g.spy_pct.map(|s| (g.book_pct, s))).collect();
    let compound = |leg: fn(&(f64, f64)) -> f64| 100.0 * (both.iter().map(|r| 1.0 + leg(r) / 100.0).product::<f64>() - 1.0);
    let mut ex: Vec<f64> = both.iter().map(|(b, s)| b - s).collect();
    ex.sort_by(f64::total_cmp);
    let footer = match ex.len() {
        0 => "\n  no link carries an S&P leg at both ends yet — nothing to compare.".to_string(),
        k => {
            let (mean, med) = (ex.iter().sum::<f64>() / k as f64, crate::commands::backtest::percentile(&ex, 50.0));
            let verdict = match clears(k, mean, med) {
                None => "needs 12 links",
                Some(true) => "BEATS S&P",
                Some(false) => "TRAILS",
            };
            format!(
                "\n  links {k} | compounded book {:+.1}% vs S&P {:+.1}% | monthly excess mean {mean:+.1}pp  median {med:+.1}pp  {verdict}",
                compound(|r| r.0),
                compound(|r| r.1)
            )
        }
    };
    format!(
        "\n  Chained record (#336) — each month's executed book held to the NEXT month's graded line and priced\n  \
         from that line's journalled closes (`rows`, and `exit` for the names it dropped), S&P leg the same\n  \
         two days. The windows follow one another, so N links are N draws, not the ~1 the verdict table is\n  \
         worth. Pre-registered: BEATS S&P at {REOPEN_LINES}+ links with mean AND median monthly excess above 0,\n  \
         else TRAILS. Inform only: no gate, trust line or push reads it. Unpriced names drop, N shows how\n  \
         many. EUR seat, price-only. NOT advice.\n\n    \
         held from   to          days  book %   S&P %  excess   N{body}{footer}"
    )
}

/// Fold every gradeable snapshot with a benchmark leg into the verdict numbers:
/// (wins, graded_n, excess_sum). The ONE source for the summary — track's table and the screen's
/// live-track-record line both consume this, so the two surfaces can't disagree. (#322) Each line
/// grades [`book_rows`]: the book it told the user to buy.
pub(crate) fn verdict_stats(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> (usize, usize, f64) {
    snaps
        .iter()
        .filter_map(|s| grade(s, &book_rows(s), usize::MAX, today, px_now, spx_now))
        .filter_map(|g| g.spy_pct.map(|spy| g.book_pct - spy))
        .fold((0, 0, 0.0), |(wins, n, sum), ex| (wins + (ex > 0.0) as usize, n + 1, sum + ex))
}

/// The one-line verdict — printed at the bottom of every run, and the title of the `--push` ping.
pub(crate) fn summary_line(wins: usize, graded_n: usize, excess_sum: f64) -> String {
    match graded_n {
        0 => "nothing gradeable yet — snapshots need at least one day of age (and priced rows).".to_string(),
        n => format!(
            "book beat the index in {wins}/{n} windows ({:.0}%), mean excess {:+.1}pp per window.",
            100.0 * wins as f64 / n as f64,
            excess_sum / n as f64
        ),
    }
}

/// (#146) How many INDEPENDENT trials this record is actually worth.
///
/// `track` grades EVERY snapshot against TODAY, so its windows are NESTED on one shared endpoint —
/// not sequential like the backtest's ~6-month entry buckets. The span of entry dates is
/// `oldest − newest`; the longest hold graded is `oldest` itself. The second is never smaller than
/// the first, so this ratio is <= 1 and STAYS <= 1 however many snapshots accrue: a 40-row table and
/// a 10,000-row one are both one observation of one endpoint. That is a property of grading to today,
/// not a shortage of data, and no amount of screening fixes it.
///
/// (#124)'s bucket formula `windows / (2 · years)` MUST NOT be reused here. It counts SEQUENTIAL
/// buckets, and that receipt says so itself: applied to a differently-shaped count it "would be a
/// wrong number wearing the right label".
///
/// (#336) The sequential version is built as [`chain_section`]: each month's book graded to the next
/// month's line, off closes that line journals (`rows` + `exit`), so it needs no per-ticker history.
/// This count stays as it is, because it describes the verdict table, which still grades to today.
fn effective_trials(oldest_days: i64, newest_days: i64) -> f64 {
    if oldest_days <= 0 {
        return 0.0;
    }
    (oldest_days - newest_days) as f64 / oldest_days as f64
}

/// The printed caveat that goes wherever the win rate goes. Same contract as the backtest's
/// `n_eff_tag`: ONE definition, appended by every surface that prints the number, so `track`'s table
/// and the screen's trust line cannot drift into quoting the same fold with different confidence.
/// EMPTY when nothing is gradeable — `summary_line` already says so in that case.
///
/// Folds over `grade`, the same fn `verdict_stats` folds, so the set behind this note and the set
/// behind the number it qualifies are the same set by construction rather than by a duplicated filter.
pub(crate) fn trials_note(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let ages: Vec<i64> = snaps
        .iter()
        .filter_map(|s| grade(s, &equal(&s.rows), BOOK, today, px_now, spx_now))
        .filter(|g| g.spy_pct.is_some())
        .map(|g| g.days)
        .collect();
    let (Some(oldest), Some(newest)) = (ages.iter().copied().max(), ages.iter().copied().min())
    else {
        return String::new();
    };
    format!(
        "\n  ~{:.1} effective trials — every window above ends TODAY, so those {} rows are NESTED on \
         one shared endpoint, not {} independent draws. The longest spans {oldest}d = {:.1}% of the \
         {}y hold this ranking is built for. Descriptive only: never a ship-rule input.",
        effective_trials(oldest, newest),
        ages.len(),
        ages.len(),
        100.0 * oldest as f64 / (365.25 * f64::from(crate::picks::HOLD_YEARS)),
        crate::picks::HOLD_YEARS,
    )
}

/// `#[mutants::skip]` because a command entry point is structurally ungradeable by the gate, not
/// merely ungraded: `run` is reachable from `main.rs` and nowhere else, so the only test that could
/// exercise it lives in the `cli` suite, and the mutants job kills with `--lib --test
/// backtest_fixture`. `replace run with ()` therefore survives whatever anyone writes, and since
/// `--in-diff` grades whole functions, one line changed in here would red the gate on its own. The
/// gradeable parts were pulled out instead — `adjust_for_splits`, `split_factor_from`, `grade`,
/// `verdict_stats`, `summary_line`, `effective_trials`, `trials_note` — and this is left as the
/// wiring between them.
#[mutants::skip]
pub async fn run(args: Vec<String>) {
    // --push: also send the summary to ntfy — for a monthly cron, so the track record reaches the
    // phone without a manual run. The cron schedule IS the dedup: no state file, one ping per fire.
    let push = args.iter().any(|a| a == "--push");
    let (mut snaps, corrupt) = read_snapshots();
    if corrupt > 0 {
        eprintln!("WARNING: {corrupt} corrupt line(s) in {SNAPSHOT_FILE} skipped");
    }
    if snaps.is_empty() {
        println!("No track record yet — {SNAPSHOT_FILE} appears after the first `screen` run; grading starts the day after.");
        return;
    }

    // one paced fetch for the union of every snapshot's graded tickers + the benchmark
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let core_cut = settings.sizing.spill_cut();
    let tickers = fetch_set(&snaps);
    let quotes = fetch::quotes(
        &client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days,
        false, false, &settings.anchor_windows, None, settings.inflation_adjust.score_on_nominal,
    )
    .await;
    let px_now = |t: &str| quotes.iter().find(|q| q.ticker == t).and_then(|q| q.price_eur).filter(|p| *p > 0.0);
    let spx_now = px_now("^GSPC");
    // (#82) BEFORE anything grades: journaled prices are quoted-on-the-day, `px_now` is retro-adjusted,
    // and a split between them books a fake collapse. Restating here means every reader below — the
    // table, the fold, the push ping — sees one consistent share definition.
    let restated = adjust_for_splits(&mut snaps, &split_factor_from(&quotes));

    println!(
        "Track record — the book each past `screen` run said to buy, graded on prices that did not exist\n\
         when it ranked: the executed book at the weights `size` funded (a run journalled before that\n\
         book was grades its top-10 equal-weight). EUR seat, price-only like the backtest. Excess =\n\
         book − S&P 500 over the same window. Delisted/unpriced names drop out and FLATTER the book —\n\
         the N column keeps that honest. NOT advice.\n"
    );
    if restated > 0 {
        println!(
            "  note: {restated} journaled price(s) restated for share splits since their snapshot — the\n  \
             journal keeps the price as quoted that day, and this run divides it by the splits that have\n  \
             happened since so both ends of every window mean the same share.\n"
        );
    }
    println!("{TABLE_HEADER}");
    let today = chrono::Local::now().date_naive();
    for snap in &snaps {
        if let Some(g) = grade(snap, &book_rows(snap), usize::MAX, today, &px_now, spx_now) {
            println!("{}", graded_row(&g));
        }
    }
    // summary comes from the SAME fold the screen's trust line reads — not from accumulators in
    // the print loop above, so the two surfaces can't drift apart.
    let (wins, graded_n, excess_sum) = verdict_stats(&snaps, today, &px_now, spx_now);
    // (#146) the win rate and the caveat that says what it is worth travel together, here and in the
    // screen's trust line. The `--push` ping interpolates `summary` too, so the phone — the most
    // decontextualised surface there is — carries it for free.
    let summary =
        format!("{}{}", summary_line(wins, graded_n, excess_sum), trials_note(&snaps, today, &px_now, spx_now));
    println!("\n  summary: {summary}");
    // (#285) and the CORE list. Printed LAST, below the summary, because that summary is the bought
    // book's verdict and belongs next to the bought book's table — a second table wedged between
    // them would invite the reader to attribute one to the other.
    // (#289) the tier lookup, built off the quotes already fetched above — one source for the fund
    // name, and none for the tier beyond `core::hold_breadth_tier` itself.
    let tier_of = |t: &str| {
        quotes.iter().find(|q| q.ticker == t).map(|q| crate::core::hold_breadth_tier(&q.name))
    };
    println!(
        "{}",
        core_section(&snaps, core_cut, settings.sizing.spill_per_tier, today, &px_now, &tier_of, spx_now)
    );
    // (#322) and the executed book's flat twin, to read against the verdict table's weighted rows.
    println!("{}", sized_section(&snaps, today, &px_now, spx_now));
    // (#324) and what each one-notch loosening would have added, read against that same bought book
    println!("{}", near_section(&snaps, today, &px_now, spx_now));
    println!("{}", peg_section(&snaps, today, &px_now, spx_now));
    println!("{}", swap_section(&snaps, today, &px_now, spx_now)); // (#334)
    println!("{}", ladder_section(&snaps, today, &px_now, spx_now)); // (#335)
    println!("{}", exit_section(&snaps, today, &px_now, spx_now));
    println!("{}", chain_section(&snaps)); // (#336)
    if push {
        let delivered = fetch::push(
            &client,
            &settings.urls,
            &settings.ntfy_topic,
            &format!("Track record: {summary}"),
            "Each past screen's bought book graded at today's prices vs the S&P 500 — live out-of-sample. NOT advice.",
        )
        .await;
        if !delivered {
            eprintln!("WARNING: ntfy push failed — track summary NOT delivered (next monthly cron retries)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(date: &str, spx: Option<f64>, rows: &[(&str, Option<f64>)]) -> Snapshot {
        Snapshot {
            date: date.into(),
            spx,
            spx_off_hi: None,
            rows: rows.iter().map(|(t, p)| (t.to_string(), *p)).collect(),
            aum: Vec::new(),
            core: Vec::new(),
            sized: Vec::new(),
            near: Vec::new(), peg: Vec::new(), swap: Vec::new(), exit: Vec::new(), carry: Vec::new(),
        }
    }

    /// The same snapshot with a journalled CORE shortlist — the half `journal_core_list` writes.
    fn with_core(mut s: Snapshot, core: &[(&str, Option<f64>)]) -> Snapshot {
        s.core = core.iter().map(|(t, p)| (t.to_string(), *p)).collect();
        s
    }

    /// The same snapshot with a journalled SIZED book — the half `screen` writes from `sized_book`.
    fn with_sized(mut s: Snapshot, sized: &[(&str, f64)]) -> Snapshot {
        s.sized = sized.iter().map(|(t, w)| (t.to_string(), *w)).collect();
        s
    }

    /// (#323) The same snapshot with a journalled near-miss tail — the half `screen` writes from
    /// `near_miss_tail`.
    fn with_near(mut s: Snapshot, near: &[(&str, Option<f64>, &str)]) -> Snapshot {
        s.near = near.iter().map(|(t, p, g)| (t.to_string(), *p, g.to_string())).collect();
        s
    }

    /// (#332) The same snapshot with a journalled PEG cohort: `(ticker, served peg_yield, the same
    /// PEG re-priced on the pinned window)`.
    fn with_peg(mut s: Snapshot, peg: &[(&str, f64, f64)]) -> Snapshot {
        s.peg = peg.iter().map(|(t, l, p)| (t.to_string(), *l, *p)).collect();
        s
    }

    /// (#334) The same snapshot with a journalled weight-swap shadow: `(ticker, close, notch, entrant?)`.
    fn with_swap(mut s: Snapshot, swap: &[(&str, Option<f64>, &str, bool)]) -> Snapshot {
        s.swap = swap.iter().map(|(t, p, n, e)| (t.to_string(), *p, n.to_string(), *e)).collect();
        s
    }

    /// (#335) The same snapshot with a journalled exit shadow: `(ticker, close, still passes?)`.
    fn with_exit(mut s: Snapshot, exit: &[(&str, Option<f64>, bool)]) -> Snapshot {
        s.exit = exit.iter().map(|(t, p, k)| (t.to_string(), *p, *k)).collect();
        s
    }

    /// (#335) Slices by rank on one line, lower minus higher: head +10, 6-10 +30, tail -10 read +20 and
    /// -30. The coin at #3 and the unbought pin would each move a slice if they were counted. A later
    /// same-month line is not a second trial; a July line from before `sized` grades on `rows` alone
    /// and adds +30 to the first gap only (no tail), so it reads mean 25.0 and nearest-rank median 30.0.
    #[test]
    fn ladder_section_grades_rank_slices_within_a_line() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 16).unwrap();
        let px = |t: &str| match t.as_bytes()[0] {
            b'H' => Some(110.0),
            b'M' => Some(130.0),
            b'T' => Some(90.0),
            _ => Some(1000.0), // the coin and the pin
        };
        let lone = snap("2026-06-16", Some(100.0), &[("H1", Some(100.0))]);
        let empty = ladder_section(std::slice::from_ref(&lone), today, &px, Some(100.0));
        assert!(empty.contains("nothing gradeable yet") && empty.contains("of the 1 journalled"), "{empty}");
        assert!(!empty.contains("pp"), "{empty}");

        let ranked = ["H1", "H2", "BTC-EUR", "H3", "H4", "H5", "M1", "M2", "M3", "M4", "M5", "PIN", "T1", "T2"];
        let at = |then: f64| ranked.iter().map(|t| (*t, Some(then))).collect::<Vec<_>>();
        let bought: Vec<(&str, f64)> = ranked.iter().filter(|t| **t != "PIN").map(|t| (*t, 4.0)).collect();
        let june = with_sized(snap("2026-06-16", Some(100.0), &at(100.0)), &bought);
        let rerun = with_sized(snap("2026-06-20", Some(100.0), &at(50.0)), &bought);
        let mut july_rows = at(100.0);
        july_rows.retain(|(t, _)| t.starts_with('H') || t.starts_with('M'));
        july_rows[..5].iter_mut().for_each(|r| r.1 = Some(110.0));
        let july = snap("2026-07-01", Some(100.0), &july_rows);
        let out = ladder_section(&[june, rerun, july], today, &px, Some(100.0));
        let line = |label: &str| out.lines().find(|l| l.trim_start().starts_with(label)).unwrap_or_default().to_string();
        let mid = line("6-10 minus 1-5");
        assert!(mid.contains("  2 line(s)") && mid.contains("mean   +25.0pp") && mid.contains("median   +30.0pp"), "{out}");
        assert!(mid.contains("needs 12 lines"), "{mid}");
        let tail = line("11-25 minus 1-10");
        assert!(tail.contains("  1 line(s)") && tail.contains("mean   -30.0pp") && tail.contains("median   -30.0pp"), "{out}");
        assert!(out.contains("1-5 (the names") && out.contains("6-10, and the tail past 10"), "{out}");
        assert!(out.contains("(#322) `head_weight`") && out.contains("(#321)'s width"), "{out}");
    }

    /// (#335) The previous calendar month's graded line — `monthly_firsts`' pick, so a sized line beats
    /// an earlier bare one — skipping same-month lines and a month with no line at all.
    #[test]
    fn prior_month_is_the_previous_months_graded_line() {
        let snaps = vec![
            snap("2026-07-02", None, &[("X", Some(1.0))]),
            with_sized(snap("2026-07-05", None, &[("Y", Some(1.0))]), &[("Y", 1.0)]),
            with_sized(snap("2026-08-01", None, &[("Z", Some(1.0))]), &[("Z", 1.0)]),
            snap("2026-09-03", None, &[("W", Some(1.0))]),
        ];
        let prior = |d: &str| prior_month(&snaps, d).map(|s| s.date.as_str());
        assert_eq!(prior("2026-08-15"), Some("2026-07-05"));
        assert_eq!(prior("2026-09-01"), Some("2026-08-01"));
        assert_eq!(prior("2026-11-01"), Some("2026-09-03"), "no October line: September is the last book");
        assert_eq!(prior("2026-07-20"), None, "nothing before July");
        assert_eq!(prior("garbage"), None);
    }

    /// (#335) What left: the prior book's names minus today's, in the prior book's order, with what
    /// `look` reads today. An unbought pin was never in the book, a coin never leaves, a line from
    /// before `sized` offers its top rows, and no prior month means nothing left.
    #[test]
    fn exits_are_the_prior_books_names_that_left() {
        let prev = with_sized(
            snap("2026-09-01", None, &[("A", Some(1.0)), ("PIN", Some(1.0)), ("BTC-EUR", Some(1.0)), ("D", Some(1.0)), ("C", Some(1.0))]),
            &[("A", 5.0), ("BTC-EUR", 5.0), ("D", 5.0), ("C", 5.0)],
        );
        let now = ["A".to_string(), "E".to_string()];
        let look = |t: &str| if t == "C" { (Some(5.0), true) } else { (None, false) };
        let want = vec![("D".to_string(), None, false), ("C".to_string(), Some(5.0), true)];
        assert_eq!(exits(Some(&prev), &now, &look), want);
        let bare = snap("2026-09-01", None, &[("P", Some(1.0)), ("A", Some(1.0))]);
        assert_eq!(exits(Some(&bare), &now, &look), vec![("P".to_string(), None, false)]);
        assert!(exits(None, &now, &look).is_empty());
    }

    /// (#337) Every name last month's line grades, minus what today's `rows` (R1) and `exit` (R3)
    /// already price and minus the benchmark, sorted, each with today's close — `None` when the quote
    /// has none. The book's rank-11 name past `rows.take(BOOK)` would be here too: the set is
    /// `fetch_set`'s, whose test covers each lane. No prior month, nothing to carry.
    #[test]
    fn carry_prices_last_months_graded_names_the_line_does_not() {
        let prev = with_core(
            with_near(snap("2026-09-01", Some(100.0), &[("R1", Some(1.0)), ("R2", Some(1.0)), ("R3", Some(1.0))]), &[("NM", Some(1.0), "cagr")]),
            &[("CORE", Some(1.0))],
        );
        let prev = with_exit(with_swap(with_peg(prev, &[("PG", 0.1, 0.2)]), &[("SW", Some(1.0), "q x2", true)]), &[("EX", Some(1.0), false)]);
        let today = with_exit(snap("2026-10-01", Some(110.0), &[("R1", Some(2.0)), ("NEW", Some(2.0))]), &[("R3", Some(2.0), true)]);
        let look = |t: &str| (t != "NM").then_some(2.0);
        let got = carry(Some(&prev), &today, &look);
        let want: Vec<(String, Option<f64>)> = ["CORE", "EX", "NM", "PG", "R2", "SW"]
            .map(|t| (t.to_string(), (t != "NM").then_some(2.0)))
            .into();
        assert_eq!(got, want);
        assert!(carry(None, &today, &look).is_empty());
    }

    /// (#335) From the flip line: A and B were held both months (+10), C left failing (-20), D left
    /// still passing (+30). Kept minus failed +30.0 decides; kept minus out-ranked -20.0 informs. F is
    /// new, E left unjournalled and the coin is held both months: none of them is KEPT or graded.
    #[test]
    fn exit_section_grades_failed_against_kept_from_the_flip() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        let px = |t: &str| match t {
            "A" | "B" => Some(110.0),
            "C" => Some(80.0),
            "D" => Some(130.0),
            _ => Some(1000.0),
        };
        let at100 = |ts: &[&'static str]| ts.iter().map(|t| (*t, Some(100.0))).collect::<Vec<_>>();
        let book = |ts: &[&'static str]| ts.iter().map(|t| (*t, 4.0)).collect::<Vec<_>>();
        let sept_names = ["A", "B", "BTC-EUR", "C", "D", "E"];
        let sept = with_sized(snap("2026-09-01", Some(100.0), &at100(&sept_names)), &book(&sept_names));
        let empty = exit_section(std::slice::from_ref(&sept), today, &px, Some(100.0));
        assert!(empty.contains("nothing gradeable yet") && empty.contains("0 of 1"), "{empty}");
        assert!(!empty.contains("pp"), "{empty}");

        let oct_names = ["A", "B", "BTC-EUR", "F"];
        let oct = with_exit(
            with_sized(snap("2026-10-01", Some(100.0), &at100(&oct_names)), &book(&oct_names)),
            &[("C", Some(100.0), false), ("D", Some(100.0), true)],
        );
        let out = exit_section(&[sept, oct], today, &px, Some(100.0));
        let line = |label: &str| out.lines().find(|l| l.trim_start().starts_with(label)).unwrap_or_default().to_string();
        let failed = line("kept minus failed");
        assert!(failed.contains("  1 line(s)") && failed.contains("mean   +30.0pp") && failed.contains("median   +30.0pp"), "{out}");
        assert!(failed.contains("needs 12 lines"), "{failed}");
        let ranked = line("kept minus out-ranked");
        assert!(ranked.contains("mean   -20.0pp") && ranked.contains("inform only"), "{out}");
        assert!(out.contains("Item 31") && out.contains("EXIT PROBE"), "{out}");
        assert!(out.contains("1 of 2 run(s)"), "{out}");
    }

    /// (#334) Entrants minus displaced, one line a month: +10 then +20 reads mean 15.0 and nearest-rank
    /// median 20.0. A month without swaps counts toward the total, not the journalled; a notch whose
    /// displaced side cannot price grades NOTHING rather than a zero.
    #[test]
    fn swap_section_grades_entrants_against_the_displaced() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "E1" => Some(120.0),
            "E2" => Some(130.0),
            "D1" | "D2" | "EP" => Some(110.0),
            _ => None,
        };
        let bare = snap("2026-05-16", Some(100.0), &[("A", Some(100.0))]);
        let empty = swap_section(std::slice::from_ref(&bare), today, &px, Some(100.0));
        assert!(empty.contains("nothing gradeable yet") && empty.contains("0 of 1"), "{empty}");
        assert!(empty.contains("cannot be backdated") && !empty.contains("pp"), "{empty}");

        let snaps = vec![
            with_swap(snap("2026-04-16", Some(100.0), &[]), &[("E1", Some(100.0), "q x0", true), ("D1", Some(100.0), "q x0", false)]),
            bare,
            with_swap(
                snap("2026-06-16", Some(100.0), &[]),
                &[("E2", Some(100.0), "q x0", true), ("D2", Some(100.0), "q x0", false), ("EP", Some(100.0), "p x2", true), ("DP", Some(100.0), "p x2", false)],
            ),
        ];
        let out = swap_section(&snaps, today, &px, Some(100.0));
        assert!(out.contains("Journalled on 2 of 3 run(s)."), "{out}");
        let q = out.lines().find(|l| l.trim_start().starts_with("q x0")).unwrap_or_default();
        assert!(q.contains("  2 line(s)") && q.contains("mean   +15.0pp") && q.contains("median   +20.0pp"), "{q}");
        assert!(q.contains("needs 12 lines"), "{q}");
        assert!(!out.contains("p x2"), "an unpriced side is no gap, not a zero one: {out}");
    }

    /// (#332) A PEG cohort's price join reaches into BOTH source lists, because the cohort spans the
    /// ranked book AND the notch names and `peg` deliberately carries no price of its own.
    #[test]
    fn journal_px_joins_from_rows_and_from_near() {
        let s = with_near(snap("2026-01-01", Some(100.0), &[("IN_ROWS", Some(10.0))]), &[("IN_NEAR", Some(20.0), "cagr")]);
        assert_eq!(journal_px(&s, "IN_ROWS"), Some(10.0));
        assert_eq!(journal_px(&s, "IN_NEAR"), Some(20.0), "a notch name must price, or half the cohort silently drops");
        assert_eq!(journal_px(&s, "NOWHERE"), None);
    }

    /// (#332) HIGH `peg_yield` IS CHEAP (the field is `100/PEG`), the two halves are the same size,
    /// and an ODD cohort drops its median name from both rather than lengthening one.
    #[test]
    fn peg_rows_splits_cheap_from_rich_on_the_chosen_denominator() {
        let px = [("A", 1.0), ("B", 1.0), ("C", 1.0), ("D", 1.0), ("E", 1.0)];
        // ladder ranks A cheapest (90) down to E (10); the PIN exactly reverses that order.
        let s = with_peg(
            snap("2026-01-01", Some(100.0), &px.map(|(t, p)| (t, Some(p)))),
            &[("A", 90.0, 10.0), ("B", 70.0, 30.0), ("C", 50.0, 50.0), ("D", 30.0, 70.0), ("E", 10.0, 90.0)],
        );
        let names = |pinned, cheap| peg_rows(&s, pinned, cheap).iter().map(|(t, ..)| *t).collect::<Vec<_>>();
        assert_eq!(names(false, true), vec!["A", "B"], "cheap = the TOP of a descending peg_yield sort");
        assert_eq!(names(false, false), vec!["D", "E"]);
        // the pin reverses the ranking, so the two denominators disagree about every name but the median
        assert_eq!(names(true, true), vec!["E", "D"]);
        assert_eq!(names(true, false), vec!["B", "A"]);
        // 5 names -> 2 + 2, and C (the median) is in neither half under either denominator
        for (pinned, cheap) in [(false, true), (false, false), (true, true), (true, false)] {
            assert_eq!(peg_rows(&s, pinned, cheap).len(), 2, "halves must match in size or the split prices cohort size");
            assert!(!names(pinned, cheap).contains(&"C"), "the median name belongs to neither half");
        }

        // TIES BREAK ON TICKER, so which name lands in which half cannot depend on journal order.
        // Journalled Z-before-Y at one identical peg_yield, the cheap half must still read Y.
        let tied = with_peg(
            snap("2026-01-01", Some(100.0), &[("Y", Some(1.0)), ("Z", Some(1.0))]),
            &[("Z", 50.0, 50.0), ("Y", 50.0, 50.0)],
        );
        assert_eq!(peg_rows(&tied, false, true).iter().map(|(t, ..)| *t).collect::<Vec<_>>(), vec!["Y"]);
        assert_eq!(peg_rows(&tied, false, false).iter().map(|(t, ..)| *t).collect::<Vec<_>>(), vec!["Z"]);
    }

    /// (#332) THE DECIDING STATISTIC IS THE CHEAP HALF, and this is the test that pins it: moving the
    /// RICH half's realized return — by any amount, in either direction — must not move the verdict.
    /// A spread-based ruler would fail this, and would pay a denominator for sorting names the tool is
    /// never long.
    #[test]
    fn peg_verdict_reads_the_cheap_half_only() {
        // Six names, halves of three. The two denominators agree that E and F are RICH and that A and
        // B are CHEAP; they disagree about exactly one slot — the ladder calls C cheap, the pin calls D
        // cheap. So the verdict is (D - C)/3 and E, F can never reach it.
        let names = [
            ("A", 90.0, 95.0),
            ("B", 80.0, 85.0),
            ("C", 70.0, 65.0), // ladder: cheap   pin: rich
            ("D", 60.0, 75.0), // ladder: rich    pin: cheap
            ("E", 50.0, 55.0), // rich under both
            ("F", 40.0, 45.0), // rich under both
        ];
        let rows: Vec<(&str, Option<f64>)> = names.iter().map(|(t, ..)| (*t, Some(100.0))).collect();
        let s = with_peg(snap("2026-01-01", Some(100.0), &rows), &names);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        assert_eq!(peg_rows(&s, false, true).iter().map(|(t, ..)| *t).collect::<Vec<_>>(), vec!["A", "B", "C"]);
        assert_eq!(peg_rows(&s, true, true).iter().map(|(t, ..)| *t).collect::<Vec<_>>(), vec!["A", "B", "D"]);

        // C doubles; everything else sits still. Ladder cheap holds it, the pin does not.
        let base = |t: &str| Some(if t == "C" { 200.0 } else { 100.0 });
        let (n, mean, med) = peg_verdict(std::slice::from_ref(&s), today, &base, Some(100.0));
        assert_eq!(n, 1);
        assert!((mean - -100.0 / 3.0).abs() < 1e-9 && (med - -100.0 / 3.0).abs() < 1e-9, "mean {mean} med {med}");
        assert_eq!(reopen_verdict(n, mean, med), "needs 12 lines", "one line can never be a reopen");

        // Now send E and F — rich under BOTH denominators — to the moon. The verdict must not budge.
        let rich_moved = |t: &str| Some(match t {
            "C" => 200.0,
            "E" | "F" => 500.0,
            _ => 100.0,
        });
        let (n2, mean2, med2) = peg_verdict(&[s], today, &rich_moved, Some(100.0));
        assert_eq!(n2, n);
        assert!((mean2 - mean).abs() < 1e-9 && (med2 - med).abs() < 1e-9, "the rich half must not reach the verdict: {mean2} vs {mean}");

        // A line with no PEG cohort folds to a stated zero, not to a NaN wearing a percent sign.
        let bare = snap("2026-01-01", Some(100.0), &[("A", Some(100.0))]);
        assert_eq!(peg_verdict(&[bare], today, &base, Some(100.0)), (0, 0.0, 0.0));
    }

    /// (#332) The section prints both denominators, the pre-registered bar, and says so plainly when
    /// there is nothing to grade — an empty table with a 0.0 in it reads as a measurement.
    #[test]
    fn peg_section_grades_two_denominators_on_one_name_set() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let px = |t: &str| Some(if t == "A" { 200.0 } else { 100.0 });
        let bare = snap("2026-01-01", Some(100.0), &[("A", Some(100.0))]);
        let empty = peg_section(std::slice::from_ref(&bare), today, &px, Some(100.0));
        assert!(empty.contains("nothing gradeable yet") && empty.contains("0 of 1"), "{empty}");
        assert!(!empty.contains('%'), "the empty case must not print a table: {empty}");

        let names = [("A", 90.0, 10.0), ("B", 70.0, 30.0), ("C", 30.0, 70.0), ("D", 10.0, 90.0)];
        let rows: Vec<(&str, Option<f64>)> = names.iter().map(|(t, ..)| (*t, Some(100.0))).collect();
        let out = peg_section(&[with_peg(snap("2026-01-01", Some(100.0), &rows), &names)], today, &px, Some(100.0));
        assert!(out.contains(PEG_HEADER), "{out}");
        assert!(out.contains("90d    4 "), "N is the whole cohort, not one half: {out}");
        assert!(out.contains("+50.0%") && out.contains("-50.0pp"), "both halves and the delta must print: {out}");
        assert!(out.contains("needs 12 lines") && out.contains("Ship Rule v2 PRIMARY at 12y AND 8y"), "{out}");
        assert!(out.contains("Journalled on 1 of 1 run(s)"), "a line carrying a cohort must be counted as carrying one: {out}");
        assert!(out.contains("10Y window"), "the pinned rung must be named in the prose: {out}");
    }

    /// (#285) The MOMENTUM-lane grade: `snap.rows` cut at [`BOOK`], which is what every assertion
    /// written before this round grades. A shim rather than an inline call because `grade` now takes
    /// the list alongside the snapshot, and a temporary `&snap(..)` cannot lend both at once.
    fn grade_book(
        s: &Snapshot,
        today: chrono::NaiveDate,
        px: &dyn Fn(&str) -> Option<f64>,
        spx: Option<f64>,
    ) -> Option<Graded> {
        grade(s, &equal(&s.rows), BOOK, today, px, spx)
    }

    /// (round 34) backward-compat: a PRE-r34 journal line (no `aum` key, and no `spx_off_hi`) still
    /// deserializes — both are `#[serde(default)]` — so an existing `.screen_snapshots.jsonl` keeps
    /// parsing and the fund-flow footer simply stays silent on those lines (empty `aum`).
    #[test]
    fn snapshot_pre_r34_line_parses() {
        let line = r#"{"date":"2026-06-01","spx":100.0,"rows":[["A",10.0],["B",null]]}"#;
        let s: Snapshot = serde_json::from_str(line).expect("pre-r34 line must still parse");
        assert_eq!(s.date, "2026-06-01");
        assert_eq!(s.spx_off_hi, None); // serde default
        assert!(s.aum.is_empty()); // serde default → flow footer silent for this line
        assert_eq!(s.rows.len(), 2);
        assert_eq!(s.rows[1], ("B".to_string(), None));
        assert!(s.core.is_empty()); // (#103) serde default → every line already on disk still reads
    }

    /// (#103) The knob-off guarantee for a file the goldens cannot cover: `.screen_snapshots.jsonl`
    /// is a user's own gitignored record, appended to forever, so a new field that widened the line
    /// unconditionally would be a silent format change nobody could diff. With `journal_core_list`
    /// off the CORE list is empty and `skip_serializing_if` drops the key entirely — the emitted line
    /// is byte-identical to the one this build's predecessor wrote. On, it round-trips.
    #[test]
    fn an_empty_core_list_leaves_the_journal_line_byte_identical() {
        let mut s = snap("2026-06-01", Some(100.0), &[("A", Some(10.0))]);
        let off = serde_json::to_string(&s).unwrap();
        assert!(!off.contains("core"), "OFF must not widen the line: {off}");
        s.core = vec![("VWCE.DE".into(), Some(140.0)), ("SWRD.L".into(), None)];
        let on = serde_json::to_string(&s).unwrap();
        assert!(on.contains(r#""core":[["VWCE.DE",140.0],["SWRD.L",null]]"#), "{on}");
        assert_eq!(serde_json::from_str::<Snapshot>(&on).unwrap().core, s.core);
    }

    /// (#82) The correction itself, stated as the bug it removes: a name journaled at €100 that has
    /// since done a 10:1 split trades at €12 today, and comparing those two numbers books -88% when
    /// the position is up 20%. Restating the OLD price (100 ÷ 10 = 10) is what makes both ends of the
    /// window mean the same share.
    ///
    /// Everything else here is a thing that must NOT move: a `None` price stays `None` rather than
    /// becoming a number, `spx` is an index level and never splits, a ticker no name in the quote set
    /// answers 1.0, and a snapshot dated after the split is already in the right definition. The
    /// returned count is what `track` and `sim` print, so it counts PRICES restated, not snapshots.
    #[test]
    fn adjust_for_splits_restates_only_prices_quoted_before_one() {
        let d = |y, m| chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap();
        // AAA split 10:1 on 2025-01-01; BBB never split; CCC is not in the quote set at all.
        let factor = |t: &str, since: chrono::NaiveDate| match t {
            "AAA" => crate::core::split_factor_since(&[(d(2025, 1), 10.0)], since),
            _ => crate::core::split_factor_since(&[], since),
        };
        let mut snaps = vec![
            snap("2024-06-01", Some(5000.0), &[("AAA", Some(100.0)), ("BBB", Some(50.0)), ("CCC", None)]),
            snap("2026-03-01", Some(6000.0), &[("AAA", Some(12.0))]),
            snap("not-a-date", Some(1.0), &[("AAA", Some(999.0))]),
        ];
        assert_eq!(adjust_for_splits(&mut snaps, &factor), 1, "one price moved, not one snapshot");
        assert_eq!(snaps[0].rows[0].1, Some(10.0), "€100 pre-split is €10 of today's share");
        assert_eq!(snaps[0].rows[1].1, Some(50.0), "no split -> byte-identical, not merely close");
        assert_eq!(snaps[0].rows[2].1, None, "an unpriced row stays unpriced; 1.0 is not a price");
        assert_eq!(snaps[0].spx, Some(5000.0), "an index level is not a share");
        assert_eq!(snaps[1].rows[0].1, Some(12.0), "already after the split -> untouched");
        assert_eq!(snaps[2].rows[0].1, Some(999.0), "an unparseable date is skipped, not guessed at");
        // and it is idempotent in the only sense that matters: a second pass over the SAME quotes
        // finds nothing left to do, because the correction is keyed off the snapshot date, not the
        // price. `track` re-reads the journal from disk every run, so this is the real second pass.
        assert_eq!(adjust_for_splits(&mut snaps, &factor), 1, "keyed off the date: the same row again");

        // (#285) the CORE list is restated by the same pass, for the same reason: it is priced the
        // same way, graded by the same `grade`, and a tracker that consolidates 10:1 would book the
        // identical fake collapse. It counts into the same total, because that number says PRICES
        // restated and a CORE price is one — a separate tally would be a second number for one fact.
        let mut cored = vec![with_core(
            snap("2024-06-01", Some(5000.0), &[("BBB", Some(50.0))]),
            &[("AAA", Some(100.0)), ("BBB", Some(50.0)), ("CCC", None)],
        )];
        assert_eq!(adjust_for_splits(&mut cored, &factor), 1, "the CORE price moved, and is counted");
        assert_eq!(cored[0].core[0].1, Some(10.0), "a CORE price splits like any other");
        assert_eq!(cored[0].core[1].1, Some(50.0), "no split -> untouched, here too");
        assert_eq!(cored[0].core[2].1, None, "an unpriced CORE row stays unpriced");
        assert_eq!(cored[0].rows[0].1, Some(50.0), "and the momentum rows are still walked");

        // (#334) the weight-swap shadow carries its own close, so the same pass restates it
        let mut swapped = vec![with_swap(snap("2024-06-01", Some(5000.0), &[]), &[("AAA", Some(100.0), "q x0", false)])];
        assert_eq!(adjust_for_splits(&mut swapped, &factor), 1, "{:?}", swapped[0].swap);
        assert_eq!(swapped[0].swap[0].1, Some(10.0), "a swap close splits like any other");

        // (#335) and the exit shadow's own close
        let mut exited = vec![with_exit(snap("2024-06-01", Some(5000.0), &[]), &[("AAA", Some(100.0), false)])];
        assert_eq!(adjust_for_splits(&mut exited, &factor), 1, "{:?}", exited[0].exit);
        assert_eq!(exited[0].exit[0].1, Some(10.0), "an exit close splits like any other");

        // (#337) and the carry, which is nothing but closes
        let mut carried = vec![snap("2024-06-01", Some(5000.0), &[])];
        carried[0].carry = vec![("AAA".into(), Some(100.0)), ("BBB".into(), Some(50.0))];
        assert_eq!(adjust_for_splits(&mut carried, &factor), 1, "{:?}", carried[0].carry);
        assert_eq!(carried[0].carry, vec![("AAA".into(), Some(10.0)), ("BBB".into(), Some(50.0))]);

        // (#323) and the near-miss tail, which `near_section` grades the same way
        let mut neared = vec![with_near(
            snap("2024-06-01", Some(5000.0), &[]),
            &[("AAA", Some(100.0), "cagr"), ("BBB", Some(50.0), "peg")],
        )];
        assert_eq!(adjust_for_splits(&mut neared, &factor), 1, "the near-miss price moved, and is counted");
        assert_eq!(neared[0].near[0].1, Some(10.0), "a near-miss price splits like any other");
        assert_eq!(neared[0].near[1].1, Some(50.0), "no split -> untouched");
        assert_eq!(neared[0].near[0].2, "cagr", "the gate is not a price");

        // the closure the commands actually pass, over quotes they already fetched
        let mut q = crate::core::Quote::stub("AAA", "€12.00", "", "A");
        q.splits = vec![(d(2025, 1), 10.0)];
        let from_quotes = split_factor_from(std::slice::from_ref(&q));
        assert_eq!(from_quotes("AAA", d(2024, 6)), 10.0);
        assert_eq!(from_quotes("ZZZ", d(2024, 6)), 1.0, "unknown ticker leaves the price alone");
    }

    /// grade(): zero-day windows and unpriced books grade nothing; a priced book computes the
    /// equal-weight return vs the benchmark leg; a missing then-price drops the row (N shrinks).
    #[test]
    fn grade_semantics() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            _ => None,
        };

        // same-day snapshot: nothing to grade
        assert!(grade_book(&snap("2026-07-16", Some(100.0), &[("UP", Some(100.0))]), today, &px, Some(105.0)).is_none());
        // no priced rows: nothing to grade
        assert!(grade_book(&snap("2026-06-16", Some(100.0), &[("GONE", Some(100.0))]), today, &px, Some(105.0)).is_none());

        // +10% and -10% legs -> book 0.0%; spy +5% -> excess negative; missing then-price drops a row
        let g = grade_book(
            &snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0)), ("UP", None)]),
            today, &px, Some(105.0),
        )
        .expect("priced book grades");
        assert_eq!((g.days, g.priced), (30, 2));
        assert!(g.book_pct.abs() < 1e-9);
        assert!((g.spy_pct.unwrap() - 5.0).abs() < 1e-9);

        // benchmark missing on either end -> book still grades, spy is None
        let g = grade_book(&snap("2026-06-16", None, &[("UP", Some(100.0))]), today, &px, Some(105.0)).expect("grades");
        assert!(g.spy_pct.is_none() && (g.book_pct - 10.0).abs() < 1e-9);
    }

    /// (#285) `grade` reads the list it is HANDED, cut where it is TOLD — the whole of what makes one
    /// piece of arithmetic serve two books.
    ///
    /// The snapshot below is built so the two lanes cannot be confused for one another: its momentum
    /// rows are all winners and its CORE rows all losers, so a grader that silently kept reading
    /// `snap.rows` would report +10 where the CORE book is -10. The CUT is pinned the same way — the
    /// third CORE name is a winner, so grading 3 instead of 2 moves the answer off -10, and a cut
    /// that quietly reverted to [`BOOK`] would take all three.
    ///
    /// The empty-list arm is the one the shipped journal is in TODAY: 13 of 13 lines carry no CORE
    /// list at all, and that must answer None — nothing gradeable — rather than a zero return.
    #[test]
    fn grade_reads_the_list_it_is_given() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            _ => None,
        };
        let s = with_core(
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("UP", Some(100.0))]),
            &[("DOWN", Some(100.0)), ("DOWN", Some(100.0)), ("UP", Some(100.0))],
        );

        let book = grade_book(&s, today, &px, Some(105.0)).expect("momentum grades");
        assert!((book.book_pct - 10.0).abs() < 1e-9, "the momentum lane is untouched: {}", book.book_pct);

        let core = grade(&s, &equal(&s.core), 2, today, &px, Some(105.0)).expect("CORE grades");
        assert_eq!(core.priced, 2, "the cut is the cut: the third CORE name is not in this book");
        assert!((core.book_pct + 10.0).abs() < 1e-9, "graded the CORE list, not the rows: {}", core.book_pct);
        // same window, same benchmark leg — that is what makes the two tables comparable at all.
        assert_eq!((core.date.as_str(), core.days), (book.date.as_str(), book.days));
        assert!((core.spy_pct.unwrap() - book.spy_pct.unwrap()).abs() < 1e-9);

        // a wider cut than the list holds takes what exists, and the third name pulls the book up
        assert!(grade(&s, &equal(&s.core), 99, today, &px, Some(105.0)).unwrap().priced == 3);
        // ...and today's journal: no CORE list -> nothing gradeable, never a zero
        assert!(grade(&snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), &[], 3, today, &px, Some(105.0)).is_none());
    }

    /// (#285) The CORE block: a sentence while the record is empty, a table once it is not.
    ///
    /// The empty arm is the shipped state and the one that must not lie — no table, no 0.0%, and it
    /// says how many lines carry a list so "the knob is off" and "the knob is on and the record is
    /// one day old" stay distinguishable. The populated arm must render through the SAME formatter
    /// as the momentum table: asserting on the header text is what catches a second, drifting copy.
    #[test]
    fn core_section_says_nothing_until_there_is_something_to_say() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            _ => None,
        };
        // (#289) a journal line whose funds no longer quote knows no tiers, and that arm must still
        // grade — it degrades to the (#261) walk-down, which is what every assertion below pins.
        let no_tier = |_: &str| None;

        // no journal at all, and a journal with no CORE list: both are "nothing yet", not a zero
        for snaps in [vec![], vec![snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))])]] {
            let out = core_section(&snaps, 3, false, today, &px, &no_tier, Some(105.0));
            assert!(out.contains("nothing gradeable yet"), "{out}");
            assert!(out.contains("cannot be backdated"), "the perishability is the point: {out}");
            assert!(!out.contains("BEAT?"), "an empty table reads as a measured result: {out}");
            assert!(!out.contains('%'), "and so does a zero: {out}");
        }
        // a CORE list exists but the run is TODAY -> still nothing gradeable, and the count says
        // the recording is working, which is the difference between the two failure modes.
        let young = vec![with_core(snap("2026-07-16", Some(100.0), &[]), &[("UP", Some(100.0))])];
        let out = core_section(&young, 3, false, today, &px, &no_tier, Some(105.0));
        assert!(out.contains("only 1 of 1 journalled run(s)"), "{out}");
        assert!(out.contains("nothing gradeable yet"), "a zero-day window grades nothing: {out}");

        // graded: -10% book against a +5% index, so the CORE row loses by 15pp and says so
        let snaps = vec![
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), // momentum only -> no CORE row
            with_core(snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), &[("DOWN", Some(100.0))]),
        ];
        let out = core_section(&snaps, 3, false, today, &px, &no_tier, Some(105.0));
        // Spelled out, not `contains(&TABLE_HEADER)` against itself: the first push of this round
        // asserted the output against the header-builder's own result, and `contains("")` is true of
        // everything, so the gate rightly called that assertion vacuous. This one pins the layout.
        assert_eq!(TABLE_HEADER, "  DATE            AGE    N       BOOK    S&P 500    EXCESS  BEAT?");
        assert!(out.contains(TABLE_HEADER), "the CORE block must print THE header: {out}");
        assert!(out.contains("Journalled on 1 of 2 run(s)."), "{out}");
        assert_eq!(out.matches("2026-06-16").count(), 1, "only the line carrying a CORE list grades: {out}");
        assert!(out.contains("-15.0pp"), "the CORE book lost by 15pp: {out}");
        assert!(out.lines().last().unwrap().ends_with("  no"), "...and the BEAT? column must say so: {out}");
    }

    /// `(#289)` The grader grades the rows `size` FUNDS, and the knob moves which rows those are.
    ///
    /// Four CORE names at a cut of two, priced so the two candidate books cannot be confused: the
    /// two all-world rows are both +10%, the developed and emerging rows are both -10%. The
    /// walk-down funds the first two — both all-world — and reads +10.0%. The tilt funds the best
    /// all-world and the best developed and reads +0.0%. A grader still carrying its own `.take`
    /// would print +10.0% in BOTH arms, which is the defect this round exists to remove.
    ///
    /// The blurb is asserted too. It said "the first N" for four rounds, and with the tilt on that
    /// sentence describes a selection the table below it did not make.
    #[test]
    fn core_section_grades_the_rows_size_funds() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "AW1" | "AW2" => Some(110.0),
            "DEV" | "EM" => Some(90.0),
            _ => None,
        };
        let tier_of = |t: &str| match t {
            "AW1" | "AW2" => Some(crate::core::ALL_WORLD_TIER),
            "DEV" => Some(1),
            "EM" => Some(2),
            _ => None,
        };
        let core = [("AW1", Some(100.0)), ("AW2", Some(100.0)), ("DEV", Some(100.0)), ("EM", Some(100.0))];
        let snaps = vec![with_core(snap("2026-06-16", Some(100.0), &[]), &core)];

        let off = core_section(&snaps, 2, false, today, &px, &tier_of, Some(105.0));
        assert!(off.contains("+10.0%"), "the walk-down funds AW1 + AW2: {off}");
        assert!(off.contains("the broadest first"), "...and must say which rule it used: {off}");

        let on = core_section(&snaps, 2, true, today, &px, &tier_of, Some(105.0));
        assert!(on.contains("+0.0%"), "the tilt funds AW1 + DEV: {on}");
        assert!(on.contains("one per market"), "...and must say which rule it used: {on}");

        // the tilt cannot invent a market: with no tier knowable it degrades to the walk-down, so
        // the TABLE is the `off` table. Only the sentence above it differs, because the rule asked
        // for did differ — the table is what grades, and it must not guess a market (rule #5).
        let blind = core_section(&snaps, 2, true, today, &px, &|_| None, Some(105.0));
        let table = |s: &str| s.lines().skip_while(|l| !l.contains("BEAT?")).collect::<Vec<_>>().join("\n");
        assert_eq!(table(&blind), table(&off), "a tierless journal line grades the walk-down");
    }

    /// (#296) the CORE table grades the spill at the weights `size` pays it, through the same
    /// `size::spill_split`. AW +10% and US -10% read -3.3% with the US row at 2x; equal would read +0.0%.
    #[test]
    fn core_section_weights_the_us_tier_as_size_does() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "AW" => Some(110.0),
            "US" => Some(90.0),
            _ => None,
        };
        let tier_of = |t: &str| match t {
            "AW" => Some(crate::core::ALL_WORLD_TIER),
            "US" => Some(crate::core::US_TIER),
            _ => None,
        };
        let snaps = vec![with_core(snap("2026-06-16", Some(100.0), &[]), &[("AW", Some(100.0)), ("US", Some(100.0))])];
        let on = core_section(&snaps, 2, true, today, &px, &tier_of, Some(105.0));
        assert!(on.contains("-3.3%"), "AW 1 share + US 2 shares = (10 - 20) / 3: {on}");
    }

    /// (#286) `grade` now folds a WEIGHTED book, and the weight is the whole reason this round
    /// exists: `size` does not fund the ranked list equally, so grading it equally grades a book
    /// nobody holds.
    ///
    /// The three arms are the three ways this can be got wrong. First, the equal-weight lanes must
    /// be BYTE-identical to what round 68 shipped — `1.0 * x` is exactly `x` and `sum of 1.0` is
    /// exactly `n` in IEEE754, so this is not an approximation and is asserted as an equality.
    /// Second, an unequal book must land somewhere only the weights can put it. Third, a book whose
    /// priced rows all weigh nothing is an UNGRADED book, not a 0% one — the fold would divide by
    /// zero and hand back a NaN that prints as a real number.
    #[test]
    fn grade_folds_the_weights_it_is_handed() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            _ => None,
        };
        let s = snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0))]);

        // Equal weights are the IDENTITY, and this is asserted as a bit-for-bit equality against the
        // pre-round-68 expression spelled out — `100 * sum(r) / n`. Not against 0.0: these two legs
        // sum to 1.11e-16 rather than zero (0.1 and -0.1 are neither exact in binary), so the old
        // code never printed a zero here either and `grade_semantics` has always used a tolerance.
        // A tolerance would pass for a fold that is merely CLOSE; this pins that nothing moved.
        let flat = grade(&s, &equal(&s.rows), BOOK, today, &px, Some(105.0)).expect("grades");
        let unweighted = 100.0 * ((110.0 / 100.0 - 1.0) + (90.0 / 100.0 - 1.0)) / 2.0;
        assert_eq!(flat.book_pct, unweighted, "equal weights are the identity, not an approximation");

        // 3:1 in favour of the winner -> +5.0, which no unweighted fold of these two rows can reach
        let tilted: Vec<(&str, Option<f64>, f64)> = vec![("UP", Some(100.0), 30.0), ("DOWN", Some(100.0), 10.0)];
        let g = grade(&s, &tilted, BOOK, today, &px, Some(105.0)).expect("grades");
        assert!((g.book_pct - 5.0).abs() < 1e-9, "the weights moved the book: {}", g.book_pct);
        assert_eq!(g.priced, 2, "weighting is not a filter — both rows are still in the book");

        // every priced row weighs nothing -> ungradeable, never 0.0% and never NaN
        let zeroed: Vec<(&str, Option<f64>, f64)> = vec![("UP", Some(100.0), 0.0), ("DOWN", Some(100.0), 0.0)];
        assert!(grade(&s, &zeroed, BOOK, today, &px, Some(105.0)).is_none(), "a weightless book is not a flat one");
    }

    /// (#286) `sized_rows` — the join, and the one place it can be got wrong. The weight lives in
    /// `sized` and the price lives in `rows`, because journalling the price twice is exactly the
    /// drift non-negotiable #4 exists to stop. A sized ticker with no row is therefore unpriced
    /// rather than an error: `grade` drops it the same way it drops any other unpriced row.
    #[test]
    fn sized_rows_joins_the_weight_to_the_price_in_rows() {
        let s = with_sized(
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", None)]),
            &[("UP", 8.0), ("DOWN", 3.0), ("GHOST", 1.0)],
        );
        let rows = sized_rows(&s);
        assert_eq!(rows, vec![("UP", Some(100.0), 8.0), ("DOWN", None, 3.0), ("GHOST", None, 1.0)]);

        // and the join is by TICKER, not by position: the two lists are ordered independently —
        // `sized` comes out of `size_weights` sorted by score, `rows` out of the screen's own order.
        let shuffled = with_sized(
            snap("2026-06-16", Some(100.0), &[("B", Some(20.0)), ("A", Some(10.0))]),
            &[("A", 5.0), ("B", 6.0)],
        );
        assert_eq!(sized_rows(&shuffled), vec![("A", Some(10.0), 5.0), ("B", Some(20.0), 6.0)]);
    }

    /// (#322) The flat-twin block. The empty arm must not lie: no table, no zero, and it says how many
    /// runs carry an executed book so "nothing recorded yet" and "recorded this morning" stay apart.
    ///
    /// The graded arm pins the three ways the twin could be wrong. The executed book (30/10/10) grades
    /// +8.0%; the twin must print +2.0%, which is the coin at its journalled 10 and the other two
    /// splitting 90 — not +8.0% (the funded weights again) and not +6.7% (the coin flattened too).
    #[test]
    fn sized_section_grades_the_flat_twin_of_the_bought_book() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            "BTC-EUR" => Some(120.0),
            _ => None,
        };

        // no journal, and a journal carrying no sized book: both are "nothing yet", not a zero
        for snaps in [vec![], vec![snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))])]] {
            let out = sized_section(&snaps, today, &px, Some(105.0));
            assert!(out.contains("nothing gradeable yet"), "{out}");
            assert!(out.contains("cannot be backdated"), "the perishability is the point: {out}");
            assert!(!out.contains("BEAT?"), "an empty table reads as a measured result: {out}");
            assert!(!out.contains('%'), "and so does a zero: {out}");
        }

        let bought = with_sized(
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0)), ("BTC-EUR", Some(100.0))]),
            &[("UP", 30.0), ("DOWN", 10.0), ("BTC-EUR", 10.0)],
        );
        let executed = grade(&bought, &book_rows(&bought), usize::MAX, today, &px, Some(101.0)).unwrap();
        assert!((executed.book_pct - 8.0).abs() < 1e-9, "the verdict grades the funded weights: {}", executed.book_pct);

        let snaps = vec![snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), bought];
        let out = sized_section(&snaps, today, &px, Some(101.0));
        assert!(out.contains(TABLE_HEADER), "the twin must print THE header: {out}");
        assert!(out.contains("Journalled on 1 of 2 run(s)."), "{out}");
        assert_eq!(out.matches("2026-06-16").count(), 1, "only the line carrying a sized book grades: {out}");
        assert!(out.contains("+2.0%"), "coin kept at 10, the rest flat at 45 each: {out}");
        assert!(out.contains("+1.0pp"), "+2.0 book against a +1.0 index: {out}");
    }

    /// (#322) The verdict grades the book each line BOUGHT. A line carrying an executed book grades it
    /// at its funded weights (+5.0%, where its top-10 equal-weight reads 0.0%); a line from before one
    /// was journalled grades its top-10 equal-weight, and its rank-11 row stays out of it.
    #[test]
    fn verdict_grades_the_book_each_line_bought() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            _ => None,
        };
        let bought = with_sized(
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0))]),
            &[("UP", 30.0), ("DOWN", 10.0)],
        );
        let (wins, n, sum) = verdict_stats(&[bought], today, &px, Some(102.0));
        assert_eq!((wins, n), (1, 1));
        assert!((sum - 3.0).abs() < 1e-9, "+5.0 funded book against +2.0: {sum}");

        let mut rows = vec![("DOWN", Some(100.0)); BOOK];
        rows.push(("UP", Some(100.0))); // rank 11: outside a top-10 book
        let (wins, n, sum) = verdict_stats(&[snap("2026-06-16", Some(100.0), &rows)], today, &px, Some(102.0));
        assert_eq!((wins, n), (0, 1));
        assert!((sum - (-10.0 - 2.0)).abs() < 1e-9, "the top-10 at -10.0 against +2.0: {sum}");
    }

    /// (#322) `run` prices exactly this set. The rank-11 row is fetched only because it was BOUGHT —
    /// before (#322) it was graded as part of the executed book and never priced, so it dropped out.
    #[test]
    fn fetch_set_prices_every_graded_lane() {
        let names: Vec<String> = (0..=BOOK).map(|i| format!("R{i:02}")).collect();
        let rows: Vec<(&str, Option<f64>)> = names.iter().map(|t| (t.as_str(), Some(1.0))).collect();
        let s = with_near(
            with_sized(
                with_core(snap("2026-06-16", Some(100.0), &rows), &[("CORE", Some(1.0)), ("R00", Some(1.0))]),
                &[("R10", 4.0), ("BTC-EUR", 5.0)],
            ),
            &[("NM", Some(1.0), "cagr")], // (#323) refused, yet graded — so priced
        );
        let s = with_swap(s, &[("SW", Some(1.0), "quality_weight x2", true)]); // (#334) an entrant past the book
        let s = with_exit(s, &[("EX", Some(1.0), false)]); // (#335) a name that left the book
        let mut want: Vec<String> = names[..BOOK].to_vec();
        want.extend(["BTC-EUR", "CORE", "EX", "NM", "R10", "SW", "^GSPC"].map(String::from));
        want.sort();
        assert_eq!(fetch_set(&[s.clone(), s]), want, "sorted, deduped across lanes and lines");
    }

    /// (#286) The knob-off guarantee for the journal, the same one `(#103)` wrote for the CORE list:
    /// `.screen_snapshots.jsonl` is the user's own gitignored record, appended to forever, so a new
    /// field that widened every line unconditionally would be a format change nobody could diff.
    /// Empty -> `skip_serializing_if` drops the key and the line is byte-identical to the one this
    /// build's predecessor wrote. Populated -> it round-trips. Absent -> `serde(default)` reads it.
    #[test]
    fn an_empty_sized_book_leaves_the_journal_line_byte_identical() {
        let mut s = snap("2026-06-01", Some(100.0), &[("A", Some(10.0))]);
        let off = serde_json::to_string(&s).unwrap();
        assert!(!off.contains("sized"), "OFF must not widen the line: {off}");
        s.sized = vec![("ABEC.DE".into(), 8.0), ("LYBK.DE".into(), 1.8)];
        let on = serde_json::to_string(&s).unwrap();
        assert!(on.contains(r#""sized":[["ABEC.DE",8.0],["LYBK.DE",1.8]]"#), "{on}");
        assert_eq!(serde_json::from_str::<Snapshot>(&on).unwrap().sized, s.sized);
        // and every line already on disk — none of which carries the key — still reads
        let line = r#"{"date":"2026-06-01","spx":100.0,"rows":[["A",10.0]]}"#;
        assert!(serde_json::from_str::<Snapshot>(line).unwrap().sized.is_empty());

        // (#323) the near-miss tail keeps the same contract: absent key, round-trip, old lines read
        assert!(!off.contains("near"), "an empty tail must not widen the line: {off}");
        s.near = vec![("EME".into(), Some(612.5), "cagr".into())];
        let on = serde_json::to_string(&s).unwrap();
        assert!(on.contains(r#""near":[["EME",612.5,"cagr"]]"#), "{on}");
        assert_eq!(serde_json::from_str::<Snapshot>(&on).unwrap().near, s.near);
        assert!(serde_json::from_str::<Snapshot>(line).unwrap().near.is_empty());

        // (#334) and the weight-swap shadow
        assert!(!off.contains("swap"), "an empty shadow must not widen the line: {off}");
        s.swap = vec![("EME".into(), Some(612.5), "quality_weight x0".into(), true)];
        let on = serde_json::to_string(&s).unwrap();
        assert!(on.contains(r#""swap":[["EME",612.5,"quality_weight x0",true]]"#), "{on}");
        assert_eq!(serde_json::from_str::<Snapshot>(&on).unwrap().swap, s.swap);
        assert!(serde_json::from_str::<Snapshot>(line).unwrap().swap.is_empty());

        // (#335) and the exit shadow
        assert!(!off.contains("exit"), "an empty shadow must not widen the line: {off}");
        s.exit = vec![("EME".into(), Some(612.5), false)];
        let on = serde_json::to_string(&s).unwrap();
        assert!(on.contains(r#""exit":[["EME",612.5,false]]"#), "{on}");
        assert_eq!(serde_json::from_str::<Snapshot>(&on).unwrap().exit, s.exit);
        assert!(serde_json::from_str::<Snapshot>(line).unwrap().exit.is_empty());

        // (#337) and the carry
        assert!(!off.contains("carry"), "an empty carry must not widen the line: {off}");
        s.carry = vec![("EME".into(), Some(612.5)), ("NOQ".into(), None)];
        let on = serde_json::to_string(&s).unwrap();
        assert!(on.contains(r#""carry":[["EME",612.5],["NOQ",null]]"#), "{on}");
        assert_eq!(serde_json::from_str::<Snapshot>(&on).unwrap().carry, s.carry);
        assert!(serde_json::from_str::<Snapshot>(line).unwrap().carry.is_empty());
    }

    /// (#323) The shadow table grades the near-miss tail equal-weight on the line's own window — +10.0%
    /// here, the cagr name +20 and the peg name 0 — and only on lines carrying one. The per-gate rows
    /// read each gate against the book that SAME line bought (+5.0% at its funded 30/10): cagr +15.0,
    /// peg -5.0 — not against the index, and not against the pooled shadow.
    #[test]
    fn near_section_grades_the_refused_names_against_the_bought_book() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            "NC" => Some(120.0),
            "NP" => Some(100.0),
            _ => None,
        };
        for snaps in [vec![], vec![snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))])]] {
            let out = near_section(&snaps, today, &px, Some(105.0));
            assert!(out.contains("nothing gradeable yet"), "{out}");
            assert!(out.contains("cannot be backdated"), "the perishability is the point: {out}");
            assert!(!out.contains("BEAT?"), "an empty table reads as a measured result: {out}");
            assert!(!out.contains('%'), "and so does a zero: {out}");
        }

        let line = with_near(
            with_sized(
                snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0))]),
                &[("UP", 30.0), ("DOWN", 10.0)],
            ),
            &[("NC", Some(100.0), "cagr"), ("NP", Some(100.0), "peg")],
        );
        let snaps = vec![snap("2026-05-16", Some(100.0), &[("UP", Some(100.0))]), line];
        let out = near_section(&snaps, today, &px, Some(101.0));
        assert!(out.contains(TABLE_HEADER), "the shadow must print THE header: {out}");
        assert!(out.contains("Journalled on 1 of 2 run(s)."), "{out}");
        assert_eq!(out.matches("2026-05-16").count(), 0, "a line with no tail grades nothing: {out}");
        assert!(out.contains("+10.0%") && out.contains("+9.0pp"), "pooled +10.0 against a +1.0 index: {out}");
        let row = |gate: &str| out.lines().find(|l| l.trim_start().starts_with(gate)).unwrap_or_default().to_string();
        let cagr = row("cagr ");
        assert!(cagr.contains("1 line(s)") && cagr.matches("+15.0pp").count() == 2, "mean and median: {cagr}");
        assert!(cagr.contains("needs 12 lines"), "{cagr}");
        assert_eq!(row("peg ").matches("-5.0pp").count(), 2, "{out}");
    }

    /// (#329) THE TWO SURFACES MUST STATE THE SAME BAR. `backtest`'s NOTCH BOOK now prints a per-row
    /// `bar` — the p95 of same-size cohorts drawn from the refused pool — and grades against it; this
    /// sentence is the only place a reader of `track` is told what that grading was. It is pinned here
    /// because it already drifted once: (#324)'s flat tenth outlived the arithmetic that justified it,
    /// and nothing failed when the two halves disagreed.
    #[test]
    fn notch_reopen_text_cites_the_band_not_the_flat_tenth() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| (t == "UP" || t == "NC").then_some(110.0);
        let line = with_near(
            with_sized(snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), &[("UP", 30.0)]),
            &[("NC", Some(100.0), "cagr")],
        );
        let out = near_section(&[line], today, &px, Some(105.0));
        assert!(out.contains("bar"), "the reopen rule must name the band it is read against: {out}");
        assert!(out.contains("p95") && out.contains("refused pool"), "and say what the band IS: {out}");
        assert!(out.contains("(#329)"), "carrying the receipt that amended it: {out}");
        assert!(!out.contains("within 0.1"), "the superseded flat bar must not still be advertised: {out}");
        assert!(out.contains("worst within 1.0"), "the worst-window leg is untouched and still stated: {out}");
    }

    /// (#330) AND THE SAME BASKET. The bar was only half the instrument: (#324)..(#329) read it off the
    /// UNCAPPED union, where every admit is bought at full weight and any below-mean cohort must drag the
    /// book by arithmetic. The verdict now comes from the top-`VERDICT_TOP` basket the tool actually
    /// publishes, and a reader of `track` has to be told which book was graded — otherwise the forward
    /// half keeps quoting a rule the backtest half stopped applying, which is exactly the drift (#329)
    /// caught a round too late to be comfortable about.
    #[test]
    fn notch_reopen_text_cites_the_ship_basket() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| (t == "UP" || t == "NC").then_some(110.0);
        let line = with_near(
            with_sized(snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), &[("UP", 30.0)]),
            &[("NC", Some(100.0), "cagr")],
        );
        let out = near_section(&[line], today, &px, Some(105.0));
        let basket = format!("top-{}", crate::commands::backtest::VERDICT_TOP);
        assert!(out.contains(&basket), "the rule must name the basket it is decided at ({basket}): {out}");
        assert!(out.contains("ship pass"), "and the verdict cell a reader would go and look at: {out}");
        assert!(out.contains("ship inert"), "including the third verdict, which is the point of (#330): {out}");
        assert!(out.contains("(#330)"), "carrying the receipt that amended it: {out}");
        assert!(out.contains("OUTRANK"), "and why a capped basket is a different question: {out}");
    }

    /// (#323) One gap per MONTH, from the line `monthly_firsts` keeps (a later same-month line whose
    /// shadow doubled must not enter); a line whose book cannot grade adds nothing, and nor does a gate
    /// whose shadow cannot; mean and nearest-rank median over what is left.
    #[test]
    fn gate_verdicts_read_one_line_a_month() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "B" => Some(100.0),
            "N1" => Some(115.0),
            "N2" => Some(95.0),
            "N3" => Some(140.0),
            "X" => Some(200.0),
            _ => None,
        };
        let line = |date: &str, book: &str, near: &[(&str, Option<f64>, &str)]| {
            with_near(snap(date, Some(100.0), &[(book, Some(100.0))]), near)
        };
        let snaps = vec![
            line("2026-03-02", "B", &[("N1", Some(100.0), "cagr")]),
            line("2026-03-20", "B", &[("X", Some(100.0), "cagr")]),
            line("2026-04-02", "B", &[("N2", Some(100.0), "cagr")]),
            line("2026-05-02", "B", &[("N3", Some(100.0), "cagr"), ("NP", Some(100.0), "peg")]),
            line("2026-06-02", "GONE", &[("N1", Some(100.0), "cagr")]),
        ];
        let v = gate_verdicts(&snaps, today, &px, Some(101.0));
        assert_eq!(v.len(), 1, "peg's only name is unpriced -> no gap, not a zero one: {v:?}");
        let (gate, n, mean, med) = &v[0];
        assert_eq!((gate.as_str(), *n), ("cagr", 3));
        assert!((mean - 50.0 / 3.0).abs() < 1e-9, "+15, -5, +40: {mean}");
        assert!((med - 15.0).abs() < 1e-9, "{med}");
    }

    /// (#323) The pre-registered reading: a year of monthly lines, and BOTH statistics strictly above 0.
    #[test]
    fn reopen_verdict_needs_a_year_and_both_statistics() {
        assert_eq!(reopen_verdict(11, 5.0, 5.0), "needs 12 lines");
        assert_eq!(reopen_verdict(12, 5.0, 5.0), "REOPEN SIGNAL");
        assert_eq!(reopen_verdict(12, 5.0, 0.0), "holds", "a zero median is not a win");
        assert_eq!(reopen_verdict(12, 0.0, 5.0), "holds", "nor a zero mean");
        assert_eq!(reopen_verdict(40, -1.0, 5.0), "holds", "one survivor's mean cannot carry it the other way either");
    }

    #[test]
    fn chain_grades_each_book_at_the_next_months_line() {
        // Oct still ranks A, and journals B — which its book dropped — only in `exit`.
        let mut oct = snap("2026-10-01", Some(110.0), &[("A", Some(120.0)), ("C", Some(50.0)), ("D", Some(40.0))]);
        oct.exit = vec![("B".into(), Some(90.0), false)];
        let snaps = vec![
            snap("2026-09-21", Some(100.0), &[("A", Some(100.0)), ("B", Some(100.0))]),
            snap("2026-09-22", Some(1.0), &[("A", Some(1.0)), ("B", Some(1.0))]), // same month: not a link
            oct,
            // Nov: C closes at 0 and D is gone, so Oct's book grades on A alone.
            snap("2026-11-02", Some(99.0), &[("A", Some(60.0)), ("C", Some(0.0))]),
        ];
        let out = chain_section(&snaps);
        // Sep: A +20, B -10 -> +5 against +10. Oct: A -50 against 110 -> 99.
        assert!(out.contains("\n    2026-09-21  2026-10-01    10    +5.0   +10.0    -5.0   2/2"), "{out}");
        assert!(out.contains("\n    2026-10-01  2026-11-02    32   -50.0   -10.0   -40.0   1/3"), "{out}");
        // 1.05 x 0.5 and 1.1 x 0.9; nearest-rank median of two is the upper one.
        assert!(
            out.contains("links 2 | compounded book -47.5% vs S&P -1.0% | monthly excess mean -22.5pp  median -5.0pp  needs 12 links"),
            "{out}"
        );
        assert!(chain_section(&snaps[..2]).contains("nothing gradeable yet"), "one month is no link");
    }

    #[test]
    fn chain_verdict_needs_twelve_links_and_both_statistics() {
        // `n` month-start lines, A and the S&P compounding at their own monthly rates.
        let months = |n: usize, a: f64, spx: f64| -> Vec<Snapshot> {
            (0..n)
                .map(|i| {
                    let date = format!("{}-{:02}-01", 2025 + i / 12, i % 12 + 1);
                    snap(&date, Some(100.0 * spx.powi(i as i32)), &[("A", Some(100.0 * a.powi(i as i32)))])
                })
                .collect()
        };
        let beats = chain_section(&months(13, 1.02, 1.01));
        assert!(beats.contains("links 12 |") && beats.ends_with("BEATS S&P"), "{beats}");
        let young = chain_section(&months(12, 1.02, 1.01));
        assert!(young.contains("links 11 |") && young.ends_with("needs 12 links"), "{young}");
        let trails = chain_section(&months(13, 1.005, 1.01));
        assert!(trails.contains("links 12 |") && trails.ends_with("TRAILS"), "{trails}");
        // A zero benchmark close voids the S&P leg of both links it touches: they print, uncounted.
        let mut holed = months(13, 1.02, 1.01);
        holed[6].spx = Some(0.0);
        let holed = chain_section(&holed);
        assert!(holed.contains("links 10 |") && holed.contains("     —"), "{holed}");
        let mut blind = months(3, 1.02, 1.01);
        blind.iter_mut().for_each(|s| s.spx = None);
        assert!(chain_section(&blind).ends_with("nothing to compare."), "no S&P leg anywhere");
    }

    /// (#285) `graded_row` is the ONE row formatter. The BEAT? decision is its only judgement and
    /// lives here rather than in the `#[mutants::skip]` entry point, so it is gradeable: a book that
    /// ties the index has NOT beaten it, and a window with no benchmark leg makes no claim either way.
    #[test]
    fn graded_row_calls_only_a_strict_win_a_win() {
        let g = |book: f64, spy: Option<f64>| Graded {
            date: "2026-06-16".into(),
            days: 30,
            priced: 4,
            book_pct: book,
            spy_pct: spy,
        };
        assert!(graded_row(&g(10.0, Some(5.0))).ends_with("  yes"));
        assert!(graded_row(&g(0.0, Some(5.0))).ends_with("  no"));
        assert!(graded_row(&g(5.0, Some(5.0))).ends_with("  no"), "a tie is not a win");
        let none = graded_row(&g(10.0, None));
        assert!(none.contains("(no benchmark that day)") && none.contains("n/a"), "{none}");
        assert!(!none.contains("yes") && !none.contains("  no"), "no benchmark -> no verdict: {none}");
    }

    /// summary_line(): the verdict that reaches the phone via `--push` — 0 windows reads as
    /// nothing-gradeable; graded windows carry the win rate and mean excess unmangled.
    #[test]
    fn summary_semantics() {
        assert!(summary_line(0, 0, 0.0).starts_with("nothing gradeable yet"));
        assert_eq!(
            summary_line(2, 3, 4.5),
            "book beat the index in 2/3 windows (67%), mean excess +1.5pp per window."
        );
    }

    /// verdict_stats(): the shared fold behind track's summary AND the screen's trust line —
    /// one win + one loss counted with their excess sum; ungradeable rows (same-day) and rows
    /// without a benchmark leg stay out of n.
    #[test]
    fn verdict_stats_fold() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
            _ => None,
        };
        let snaps = vec![
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), // book +10, spy +5 -> win, ex +5
            snap("2026-06-16", Some(100.0), &[("DOWN", Some(100.0))]), // book -10, spy +5 -> loss, ex -15
            snap("2026-06-16", None, &[("UP", Some(100.0))]),        // no benchmark leg -> not counted
            snap("2026-07-16", Some(100.0), &[("UP", Some(100.0))]), // same-day -> not counted
        ];
        let (wins, n, sum) = verdict_stats(&snaps, today, &px, Some(105.0));
        assert_eq!((wins, n), (1, 2));
        assert!((sum - (5.0 - 15.0)).abs() < 1e-9);
    }

    /// effective_trials(): the ratio that says a nested-window record is worth about one trial.
    /// The 0.5 case is the load-bearing one — it is none of the values a return-replacement would
    /// substitute, so it pins the arithmetic and not just the shape.
    #[test]
    fn effective_trials_prices_nested_windows() {
        assert!((effective_trials(40, 20) - 0.5).abs() < 1e-9);
        // <= 1 BY CONSTRUCTION: the newest window can only be younger than the oldest, never older,
        // so the numerator can never exceed the denominator. This is the whole point of the number.
        assert!((effective_trials(42, 0) - 1.0).abs() < 1e-9, "a record graded from day zero is ONE trial");
        assert!(effective_trials(9_999, 1) <= 1.0, "no snapshot count can buy a second trial");
        // Degenerate spans answer 0, not NaN or a divide-by-zero: `run` prints this unconditionally.
        assert!((effective_trials(0, 0) - 0.0).abs() < 1e-9);
        assert!((effective_trials(-5, 0) - 0.0).abs() < 1e-9);
    }

    /// trials_note(): the caveat that travels with every printed win rate. Grades the same set
    /// `verdict_stats` does — the un-benchmarked row is the probe for that, since counting it would
    /// make the record look OLDER and the trial count higher than it is.
    #[test]
    fn trials_note_reports_the_effective_count() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| if t == "UP" { Some(110.0) } else { None };
        let snaps = vec![
            snap("2026-05-16", None, &[("UP", Some(100.0))]), // 61d but NO benchmark -> not graded
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), // 30d -> oldest graded
            snap("2026-07-06", Some(100.0), &[("UP", Some(100.0))]), // 10d -> newest graded
        ];
        let note = trials_note(&snaps, today, &px, Some(105.0));
        // (30 - 10) / 30 = 0.67; the un-benchmarked 61d row would read 0.8 and "spans 61d".
        assert!(note.contains("~0.7 effective trials"), "{note}");
        assert!(note.contains("those 2 rows"), "graded count must exclude the un-benchmarked row: {note}");
        assert!(note.contains("spans 30d"), "longest window must be the oldest GRADED one: {note}");
        assert!(note.contains("0.4% of the 20y hold"), "horizon share missing: {note}");

        // Nothing gradeable -> EMPTY, because `summary_line` already says "nothing gradeable yet"
        // and two ways of saying it is how the two surfaces start disagreeing.
        assert_eq!(trials_note(&[], today, &px, Some(105.0)), "");
        let unpriced = vec![snap("2026-06-16", Some(100.0), &[("MISSING", Some(100.0))])];
        assert_eq!(trials_note(&unpriced, today, &px, Some(105.0)), "");
    }

    /// Snapshot JSONL round-trips (the journal format `screen` writes and `track` reads back),
    /// and pre-sim journal lines WITHOUT spx_off_hi still deserialize (serde default -> None).
    #[test]
    fn snapshot_roundtrip() {
        let s = snap("2026-07-16", Some(5000.0), &[("SXLK.L", Some(156.62)), ("ERR", None)]);
        let line = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&line).unwrap();
        assert_eq!(back.date, "2026-07-16");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].1, Some(156.62));

        let old: Snapshot =
            serde_json::from_str(r#"{"date":"2026-07-01","spx":5000.0,"rows":[["A",1.0]]}"#).unwrap();
        assert!(old.spx_off_hi.is_none());
    }
}
