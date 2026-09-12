//! `track` — live out-of-sample track record: how did each past `screen` top-10 actually do?
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
        for (ticker, px) in snap.rows.iter_mut().chain(snap.core.iter_mut()) {
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
///   `(#285)` and what `verdict_stats` and the screen's trust line still grade;
/// * the CORE hold shortlist — `snap.core` cut at `Sizing::spill_cut()`, the number of trackers
///   `size` actually spills the undeployed remainder into;
/// * (#286) the EXECUTED book — [`sized_rows`], uncut, the only one of the three that is not
///   equal-weight.
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
/// which is a claim about the momentum book; mixing a second book into it would silently move a
/// number two surfaces currently agree on, which is the exact drift its own doc exists to prevent.
/// (#286) One lane, graded across every journalled run: `(runs carrying this lane, runs, printed
/// rows)`. The arithmetic of "grade them all and format each" is shared; the WORDS are not, because
/// each lane owes the reader a different sentence about what it is and how it is weighted. Two
/// closures to push those sentences in here would be more code than the four lines they replace.
fn graded_rows<'a>(
    snaps: &'a [Snapshot],
    rows_of: &dyn Fn(&'a Snapshot) -> Vec<(&'a str, Option<f64>, f64)>,
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

fn core_section(
    snaps: &[Snapshot],
    cut: usize,
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    let (journalled, total, rows) = graded_rows(snaps, &|s| equal(&s.core), cut, today, px_now, spx_now);
    if rows.is_empty() {
        return format!(
            "\n  CORE hold shortlist: nothing gradeable yet. A line needs a day of age and at least one\n  \
             priced row before it grades, and only {journalled} of {total} journalled run(s) carry a CORE\n  \
             list at all. `journal_core_list` is what writes it; the record starts the run AFTER that is\n  \
             switched on, and cannot be backdated."
        );
    }
    let body = rows.join("\n");
    format!(
        "\n  CORE hold shortlist — the buy-and-hold half of the report, graded the same way. Each row is\n  \
         the first {cut} name(s) of that run's CORE list: what `size` spills the remainder its caps\n  \
         could not deploy into, which is routinely two thirds of gross. Equal-weight, EUR seat,\n  \
         price-only, same windows as above. NOT advice.\n  \
         Journalled on {journalled} of {total} run(s).\n\n{TABLE_HEADER}\n{body}"
    )
}

/// (#286) The third table, and the only one that grades what the user was actually told to buy.
///
/// The two above grade LISTS. This grades the BOOK: `size` drops the names that fail the growth
/// gate, keeps one listing per issuer, and weights what survives by score / volatility inside a
/// class budget before capping it — so its membership and its weights both differ from the ranked
/// top-10 the first table prints. Until this round nothing recorded that, and it cannot be
/// reconstructed after the fact, because the weights depend on each name's volatility as of the run.
///
/// It grades the GROWTH half only. Those weights sum to whatever the caps could deploy — 70.0% on
/// the 2026-09-12 run — and the undeployed remainder goes to the CORE trackers the table above
/// grades. The two sections together cover the book; neither is the whole of it, and the blurbs say
/// so rather than leaving the reader to add them up.
fn sized_section(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> String {
    // usize::MAX, not a cut: the sizing IS the cut. A row that reached this list already survived
    // the gate, the issuer dedup and the caps, and dropping its tail would grade a book nobody holds.
    let (journalled, total, rows) = graded_rows(snaps, &sized_rows, usize::MAX, today, px_now, spx_now);
    if rows.is_empty() {
        return format!(
            "\n  Executed book: nothing gradeable yet. A line needs a day of age and at least one priced\n  \
             row before it grades, and only {journalled} of {total} journalled run(s) carry a sized book at\n  \
             all. The record starts the run AFTER this ships and cannot be backdated — the weights depend\n  \
             on each name's volatility as of that run, so no later run can recover them."
        );
    }
    let body = rows.join("\n");
    format!(
        "\n  Executed book — that run's ranked list put through `size`'s weighting, WEIGHTED as it funds\n  \
         it. Not the same book as the first table: names failing the growth gate are gone, only one\n  \
         listing per issuer survives, and what is left is weighted by score / volatility inside a class\n  \
         budget, then capped. Covers the deployed half only — the remainder the caps could not place is\n  \
         the CORE block above. Scored on the SCREEN's numbers, which use live fundamentals; a later\n  \
         `size --picks` re-derives them price-only, so its uncapped rows can differ by a few tenths of a\n  \
         point (class sums are identical). EUR seat, price-only returns, same windows. NOT advice.\n  \
         Journalled on {journalled} of {total} run(s).\n\n{TABLE_HEADER}\n{body}"
    )
}

/// Fold every gradeable snapshot with a benchmark leg into the verdict numbers:
/// (wins, graded_n, excess_sum). The ONE source for the summary — track's table and the screen's
/// live-track-record line both consume this, so the two surfaces can't disagree.
pub(crate) fn verdict_stats(
    snaps: &[Snapshot],
    today: chrono::NaiveDate,
    px_now: &dyn Fn(&str) -> Option<f64>,
    spx_now: Option<f64>,
) -> (usize, usize, f64) {
    snaps
        .iter()
        .filter_map(|s| grade(s, &equal(&s.rows), BOOK, today, px_now, spx_now))
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
/// ponytail: the upgrade path is grading each snapshot at its own maturity (`d_i + H`) instead of at
/// today, which makes the windows sequential and lets this grow past 1. It needs per-ticker history
/// rather than the one current-price fetch `run` makes, and on a record this young it would grade
/// zero rows — so it is named, not built.
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

    // one paced fetch for the union of every snapshot's book tickers + the benchmark
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    // (#285) the CORE names are fetched too, cut at the count `size` funds — without them every CORE
    // row would price as n/a and the new block would read empty for the wrong reason. Only the graded
    // prefix is fetched: the journal carries the whole shortlist, and pricing rows nothing grades
    // would buy requests this command has no use for.
    let core_cut = settings.sizing.spill_cut();
    let mut tickers: Vec<String> = snaps
        .iter()
        .flat_map(|s| s.rows.iter().take(BOOK).chain(s.core.iter().take(core_cut)).map(|(t, _)| t.clone()))
        .chain(std::iter::once("^GSPC".to_string()))
        .collect();
    tickers.sort();
    tickers.dedup();
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
        "Track record — the screen's own past top-10s graded on prices that did not exist when they\n\
         ranked (equal-weight, EUR seat, price-only like the backtest). Excess = book − S&P 500 over\n\
         the same window. Delisted/unpriced names drop out and FLATTER the book — the N column keeps\n\
         that honest. NOT advice.\n"
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
        if let Some(g) = grade(snap, &equal(&snap.rows), BOOK, today, &px_now, spx_now) {
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
    // (#285) and the other two thirds. Printed LAST, below the summary, because that summary is the
    // momentum book's verdict and belongs next to the momentum table — a second table wedged between
    // them would invite the reader to attribute one to the other.
    println!("{}", core_section(&snaps, core_cut, today, &px_now, spx_now));
    // (#286) and the book those two lists actually become once `size` has had them. Last, because it
    // is the only weighted table and the reader should meet the two equal-weight ones first.
    println!("{}", sized_section(&snaps, today, &px_now, spx_now));
    if push {
        let delivered = fetch::push(
            &client,
            &settings.urls,
            &settings.ntfy_topic,
            &format!("Track record: {summary}"),
            "Screen's own past top-10s graded at today's prices vs the S&P 500 — live out-of-sample. NOT advice.",
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

        // no journal at all, and a journal with no CORE list: both are "nothing yet", not a zero
        for snaps in [vec![], vec![snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))])]] {
            let out = core_section(&snaps, 3, today, &px, Some(105.0));
            assert!(out.contains("nothing gradeable yet"), "{out}");
            assert!(out.contains("cannot be backdated"), "the perishability is the point: {out}");
            assert!(!out.contains("BEAT?"), "an empty table reads as a measured result: {out}");
            assert!(!out.contains('%'), "and so does a zero: {out}");
        }
        // a CORE list exists but the run is TODAY -> still nothing gradeable, and the count says
        // the recording is working, which is the difference between the two failure modes.
        let young = vec![with_core(snap("2026-07-16", Some(100.0), &[]), &[("UP", Some(100.0))])];
        let out = core_section(&young, 3, today, &px, Some(105.0));
        assert!(out.contains("only 1 of 1 journalled run(s)"), "{out}");
        assert!(out.contains("nothing gradeable yet"), "a zero-day window grades nothing: {out}");

        // graded: -10% book against a +5% index, so the CORE row loses by 15pp and says so
        let snaps = vec![
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), // momentum only -> no CORE row
            with_core(snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), &[("DOWN", Some(100.0))]),
        ];
        let out = core_section(&snaps, 3, today, &px, Some(105.0));
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

    /// (#286) The executed-book block. The empty arm is the shipped state — 13 of 13 journal lines
    /// carry no sized book — and it must not lie: no table, no zero, and it says how many runs carry
    /// one so "nothing recorded yet" and "recorded this morning" stay distinguishable.
    ///
    /// The graded arm is where this table earns its place next to the other two: the SAME two names
    /// that grade to 0.0% equal-weight grade to +5.0% at the weights `size` actually funds, so a
    /// section that silently fell back to `equal` would print the first table's number here.
    #[test]
    fn sized_section_grades_the_weighted_book_not_the_list() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 16).unwrap();
        let px = |t: &str| match t {
            "UP" => Some(110.0),
            "DOWN" => Some(90.0),
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

        let snaps = vec![
            snap("2026-06-16", Some(100.0), &[("UP", Some(100.0))]), // no sized book -> no row
            with_sized(
                snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0))]),
                &[("UP", 30.0), ("DOWN", 10.0)],
            ),
        ];
        // +2.0% index, deliberately NOT the +5.0% that would tie the book: `graded_row` compares the
        // raw floats and this book is 5.000000000000004, so a tie fixture here would assert a win
        // and read as a bug in the row formatter. The tie is already pinned, on exact inputs, by
        // `graded_row_calls_only_a_strict_win_a_win`.
        let out = sized_section(&snaps, today, &px, Some(102.0));
        assert!(out.contains(TABLE_HEADER), "the executed block must print THE header: {out}");
        assert!(out.contains("Journalled on 1 of 2 run(s)."), "{out}");
        assert_eq!(out.matches("2026-06-16").count(), 1, "only the line carrying a sized book grades: {out}");
        assert!(out.contains("+5.0%"), "graded at the FUNDED weights, not equal-weight (which is 0.0%): {out}");
        assert!(out.contains("+3.0pp"), "+5.0 book against a +2.0 index: {out}");
        assert!(out.lines().last().unwrap().ends_with("  yes"), "and it beat the index: {out}");
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
