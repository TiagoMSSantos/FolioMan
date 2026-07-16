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

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub date: String,                     // YYYY-MM-DD of the screen run
    pub spx: Option<f64>,                 // ^GSPC close (EUR) that day — the benchmark leg
    pub rows: Vec<(String, Option<f64>)>, // (ticker, close EUR) in rank order, top slice
}

/// Append today's ranked slice — unless the journal already ends with this date (same-day rerun).
/// A write failure only costs one day of track record: warn, never fail the screen run.
pub fn append_snapshot(snap: &Snapshot) {
    let last_date = std::fs::read_to_string(SNAPSHOT_FILE).ok().and_then(|s| {
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
            .open(SNAPSHOT_FILE)
            .and_then(|mut f| writeln!(f, "{json}"))
    });
    if !matches!(appended, Ok(Ok(()))) {
        eprintln!("WARNING: could not append {SNAPSHOT_FILE} — today's ranking missing from the track record");
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

pub async fn run(_args: Vec<String>) {
    let raw = match std::fs::read_to_string(SNAPSHOT_FILE) {
        Ok(s) => s,
        Err(_) => {
            println!("No track record yet — {SNAPSHOT_FILE} appears after the first `screen` run; grading starts the day after.");
            return;
        }
    };
    let mut corrupt = 0usize;
    let snaps: Vec<Snapshot> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).map_err(|_| corrupt += 1).ok())
        .collect();
    if corrupt > 0 {
        eprintln!("WARNING: {corrupt} corrupt line(s) in {SNAPSHOT_FILE} skipped");
    }
    if snaps.is_empty() {
        println!("No track record yet — {SNAPSHOT_FILE} has no readable snapshots.");
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
        false, false, &settings.anchor_windows, None,
    )
    .await;
    let px_now = |t: &str| quotes.iter().find(|q| q.ticker == t).and_then(|q| q.price_eur).filter(|p| *p > 0.0);
    let spx_now = px_now("^GSPC");

    println!(
        "Track record — the screen's own past top-10s graded on prices that did not exist when they\n\
         ranked (equal-weight, EUR seat, price-only like the backtest). Excess = book − S&P 500 over\n\
         the same window. Delisted/unpriced names drop out and FLATTER the book — the N column keeps\n\
         that honest. NOT advice.\n"
    );
    println!("  {:<12} {:>6} {:>4} {:>10} {:>10} {:>9}  BEAT?", "DATE", "AGE", "N", "BOOK", "S&P 500", "EXCESS");
    let today = chrono::Local::now().date_naive();
    let mut wins = 0usize;
    let mut graded_n = 0usize;
    let mut excess_sum = 0.0;
    for snap in &snaps {
        let Some(g) = grade(snap, today, &px_now, spx_now) else { continue };
        match g.spy_pct {
            Some(spy) => {
                let excess = g.book_pct - spy;
                graded_n += 1;
                wins += (excess > 0.0) as usize;
                excess_sum += excess;
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
    match graded_n {
        0 => println!("\n  nothing gradeable yet — snapshots need at least one day of age (and priced rows)."),
        n => println!(
            "\n  summary: book beat the index in {wins}/{n} windows ({:.0}%), mean excess {:+.1}pp per window.",
            100.0 * wins as f64 / n as f64,
            excess_sum / n as f64
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(date: &str, spx: Option<f64>, rows: &[(&str, Option<f64>)]) -> Snapshot {
        Snapshot { date: date.into(), spx, rows: rows.iter().map(|(t, p)| (t.to_string(), *p)).collect() }
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

    /// Snapshot JSONL round-trips (the journal format `screen` writes and `track` reads back).
    #[test]
    fn snapshot_roundtrip() {
        let s = snap("2026-07-16", Some(5000.0), &[("SXLK.L", Some(156.62)), ("ERR", None)]);
        let line = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&line).unwrap();
        assert_eq!(back.date, "2026-07-16");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].1, Some(156.62));
    }
}
