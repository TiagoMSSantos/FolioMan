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
pub(crate) fn adjust_for_splits(snaps: &mut [Snapshot], factor_since: &dyn Fn(&str, chrono::NaiveDate) -> f64) -> usize {
    let mut restated = 0usize;
    for snap in snaps.iter_mut() {
        let Ok(then) = chrono::NaiveDate::parse_from_str(&snap.date, "%Y-%m-%d") else { continue };
        for (ticker, px) in snap.rows.iter_mut() {
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

/// Grade one snapshot against today's prices. `None` = nothing gradeable (too young, no priced rows).
fn grade(snap: &Snapshot, today: chrono::NaiveDate, px_now: &dyn Fn(&str) -> Option<f64>, spx_now: Option<f64>) -> Option<Graded> {
    let then = chrono::NaiveDate::parse_from_str(&snap.date, "%Y-%m-%d").ok()?;
    let days = (today - then).num_days();
    if days < 1 {
        return None; // today's snapshot: zero-day window grades nothing
    }
    let rets: Vec<f64> = snap
        .rows
        .iter()
        .take(BOOK)
        .filter_map(|(t, px_then)| {
            let (then_px, now_px) = (px_then.filter(|p| *p > 0.0)?, px_now(t)?);
            Some(now_px / then_px - 1.0)
        })
        .collect();
    if rets.is_empty() {
        return None;
    }
    let book_pct = 100.0 * rets.iter().sum::<f64>() / rets.len() as f64;
    let spy_pct = snap.spx.filter(|p| *p > 0.0).zip(spx_now).map(|(then_px, now_px)| 100.0 * (now_px / then_px - 1.0));
    Some(Graded { date: snap.date.clone(), days, priced: rets.len(), book_pct, spy_pct })
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
        .filter_map(|s| grade(s, today, px_now, spx_now))
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
        .filter_map(|s| grade(s, today, px_now, spx_now))
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
    let mut tickers: Vec<String> = snaps
        .iter()
        .flat_map(|s| s.rows.iter().take(BOOK).map(|(t, _)| t.clone()))
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
    println!("  {:<12} {:>6} {:>4} {:>10} {:>10} {:>9}  BEAT?", "DATE", "AGE", "N", "BOOK", "S&P 500", "EXCESS");
    let today = chrono::Local::now().date_naive();
    for snap in &snaps {
        let Some(g) = grade(snap, today, &px_now, spx_now) else { continue };
        match g.spy_pct {
            Some(spy) => {
                let excess = g.book_pct - spy;
                println!(
                    "  {:<12} {:>5}d {:>4} {:>+9.1}% {:>+9.1}% {:>+8.1}pp  {}",
                    g.date, g.days, g.priced, g.book_pct, spy, excess,
                    if excess > 0.0 { "yes" } else { "no" }
                );
            }
            None => println!(
                "  {:<12} {:>5}d {:>4} {:>+9.1}% {:>10} {:>9}  (no benchmark that day)",
                g.date, g.days, g.priced, g.book_pct, "n/a", "n/a"
            ),
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
        }
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
        assert!(grade(&snap("2026-07-16", Some(100.0), &[("UP", Some(100.0))]), today, &px, Some(105.0)).is_none());
        // no priced rows: nothing to grade
        assert!(grade(&snap("2026-06-16", Some(100.0), &[("GONE", Some(100.0))]), today, &px, Some(105.0)).is_none());

        // +10% and -10% legs -> book 0.0%; spy +5% -> excess negative; missing then-price drops a row
        let g = grade(
            &snap("2026-06-16", Some(100.0), &[("UP", Some(100.0)), ("DOWN", Some(100.0)), ("UP", None)]),
            today, &px, Some(105.0),
        )
        .expect("priced book grades");
        assert_eq!((g.days, g.priced), (30, 2));
        assert!(g.book_pct.abs() < 1e-9);
        assert!((g.spy_pct.unwrap() - 5.0).abs() < 1e-9);

        // benchmark missing on either end -> book still grades, spy is None
        let g = grade(&snap("2026-06-16", None, &[("UP", Some(100.0))]), today, &px, Some(105.0)).expect("grades");
        assert!(g.spy_pct.is_none() && (g.book_pct - 10.0).abs() < 1e-9);
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
