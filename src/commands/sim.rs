//! `sim` — paper-DCA of the screen's own advice, executed with (pretend) money: each calendar
//! month, income arrives (`monthly_deploy_eur` × the deploy-line entry-state multiplier) and buys
//! the equal-weight top-10 of that month's FIRST screen snapshot at the journaled prices, €1 fee
//! per name bought. Pure replay of `.screen_snapshots.jsonl` — no state of its own: every run
//! recomputes the whole ledger, so the rule lives in code+config (versioned) and there is no
//! second file to drift. This is the cumulative €-weighted cousin of `track`: track grades each
//! past top-10 per window, sim compounds them into one fee-aware portfolio vs an S&P 500 DCA of
//! the same cashflows. Price-only, EUR seat, dividends not counted. NOT advice.

use crate::commands::screen::deploy_scaled_eur;
use crate::commands::track::{Snapshot, BOOK, SNAPSHOT_FILE};
use crate::{config, fetch};
use std::collections::BTreeMap;

/// Flat fee per distinct name per buy event (user's broker: ≥1€ per asset buy).
const SIM_FEE_EUR: f64 = 1.0;

/// One executed monthly buy: the month's budget split equal-weight across the snapshot's priced
/// top rows. `deployed` = the full budget (fees included); `mult_known` = false means the journal
/// line predates the `spx_off_hi` field, so the ×1 multiplier is a fallback, not a measured state.
struct Event {
    date: String,
    mult: f64,
    mult_known: bool,
    deployed: f64,
    fees: f64,
    spx: Option<f64>,
    lots: Vec<(String, f64, f64)>, // (ticker, qty, cost € incl. this lot's fee)
}

/// The whole replay: executed events + months whose income is still cash (no snapshot to buy
/// with, or a degenerate budget) — that cash deploys at the next event, or waits as `pending`.
struct Ledger {
    events: Vec<Event>,
    pending_months: u32,
}

/// "YYYY-MM-DD" -> (year, month). Malformed dates return None (the row is skipped upstream).
fn ym(date: &str) -> Option<(i32, u32)> {
    let y = date.get(0..4)?.parse().ok()?;
    let m = date.get(5..7)?.parse().ok()?;
    (1..=12).contains(&m).then_some((y, m))
}

fn next_ym((y, m): (i32, u32)) -> (i32, u32) {
    if m == 12 { (y + 1, 1) } else { (y, m + 1) }
}

/// First snapshot of each calendar month, keyed by (year, month) — the buy dates. Input order
/// does not matter; within a month the earliest date wins.
fn monthly_firsts(snaps: &[Snapshot]) -> BTreeMap<(i32, u32), &Snapshot> {
    let mut firsts: BTreeMap<(i32, u32), &Snapshot> = BTreeMap::new();
    for s in snaps {
        let Some(key) = ym(&s.date) else { continue };
        match firsts.get(&key) {
            Some(prev) if prev.date <= s.date => {}
            _ => {
                firsts.insert(key, s);
            }
        }
    }
    firsts
}

/// Execute one buy: equal-weight split of `budget` across the top [`BOOK`] rows that carried a
/// price (unpriced rows drop and the split grows — same priced-N honesty as `track`). Returns
/// None when nothing is buyable (no priced rows, or the per-name slice would not clear the fee)
/// so the caller can keep that month's income as pending cash instead of vaporising it.
fn buy_event(snap: &Snapshot, budget: f64, mult: f64, mult_known: bool) -> Option<Event> {
    let priced: Vec<(&str, f64)> = snap
        .rows
        .iter()
        .take(BOOK)
        .filter_map(|(t, p)| p.filter(|p| *p > 0.0).map(|p| (t.as_str(), p)))
        .collect();
    let k = priced.len();
    if k == 0 {
        return None;
    }
    let alloc = budget / k as f64;
    if alloc <= SIM_FEE_EUR {
        return None;
    }
    let lots = priced
        .iter()
        .map(|(t, px)| ((*t).to_string(), (alloc - SIM_FEE_EUR) / px, alloc))
        .collect();
    Some(Event {
        date: snap.date.clone(),
        mult,
        mult_known,
        deployed: budget,
        fees: k as f64 * SIM_FEE_EUR,
        spx: snap.spx,
        lots,
    })
}

/// Replay the journal month by month from the first snapshot's month through `now_ym`: a month
/// with a snapshot deploys base × entry-state multiplier plus any accrued gap cash; a month
/// without one banks its base at ×1 (income arrives regardless — it just deploys late, at the
/// next event's prices). `base` must be > 0 (gated in `run`).
fn ledger(snaps: &[Snapshot], base: f64, now_ym: (i32, u32)) -> Ledger {
    let firsts = monthly_firsts(snaps);
    let Some(start) = firsts.keys().next().copied() else {
        return Ledger { events: Vec::new(), pending_months: 0 };
    };
    let mut events = Vec::new();
    let mut pending = 0u32;
    let mut m = start;
    while m <= now_ym {
        match firsts.get(&m) {
            Some(snap) => {
                // ponytail: unwrap_or is unreachable (base > 0 gated) — kept total, no panic path
                let (mult, scaled) = deploy_scaled_eur(base, snap.spx_off_hi).unwrap_or((1.0, base));
                let budget = scaled + f64::from(pending) * base;
                match buy_event(snap, budget, mult, snap.spx_off_hi.is_some()) {
                    Some(ev) => {
                        pending = 0;
                        events.push(ev);
                    }
                    None => pending += 1, // nothing buyable: this month's income stays cash
                }
            }
            None => pending += 1,
        }
        m = next_ym(m);
    }
    Ledger { events, pending_months: pending }
}

/// The boring twin: the same gross cashflow into the S&P 500 at the same dates, one 1€ fee per
/// event, priced off the journaled ^GSPC close. Returns (cost €, index units, covered events) —
/// events whose line carried no ^GSPC close are skipped, `covered` keeps that visible.
fn benchmark(events: &[Event]) -> (f64, f64, usize) {
    events.iter().filter_map(|e| e.spx.filter(|s| *s > 0.0).map(|s| (e.deployed, s))).fold(
        (0.0, 0.0, 0),
        |(cost, units, n), (deployed, spx)| {
            (cost + deployed, units + (deployed - SIM_FEE_EUR) / spx, n + 1)
        },
    )
}

/// Aggregate every event's lots into one holding per ticker (a name bought in several months is
/// ONE position): summed qty, summed cost €. Keyed by owned ticker so the map is free of the
/// events' lifetime; run() prints from this same map, so the shown rows ARE the tested ones.
fn holdings(events: &[Event]) -> BTreeMap<String, (f64, f64)> {
    let mut held: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for e in events {
        for (t, qty, cost) in &e.lots {
            let h = held.entry(t.clone()).or_insert((0.0, 0.0));
            h.0 += qty;
            h.1 += cost;
        }
    }
    held
}

/// Portfolio totals at TODAY's prices: (value €, cost € of the priced names, priced count).
/// An unpriced-today name is excluded from BOTH sides — value AND cost — so P/L never compares
/// a book with a hole in it against full cost; the count keeps the hole visible upstream.
fn value_priced(
    held: &BTreeMap<String, (f64, f64)>,
    px_now: &dyn Fn(&str) -> Option<f64>,
) -> (f64, f64, usize) {
    held.iter()
        .filter_map(|(t, &(qty, cost))| px_now(t).map(|px| (qty * px, cost)))
        .fold((0.0, 0.0, 0), |(v, c, n), (val, cost)| (v + val, c + cost, n + 1))
}

/// One buy event's basket at TODAY's prices — the per-month "what did that advice become" line:
/// (value €, cost € of the priced lots, priced count). Same both-sides rule as [`value_priced`]
/// (an unpriced lot drops from value AND cost). None = nothing in the basket priced today.
fn event_now(e: &Event, px_now: &dyn Fn(&str) -> Option<f64>) -> Option<(f64, f64, usize)> {
    let (v, c, n) = e
        .lots
        .iter()
        .filter_map(|(t, qty, cost)| px_now(t).map(|px| (qty * px, *cost)))
        .fold((0.0, 0.0, 0), |(v, c, n), (val, cost)| (v + val, c + cost, n + 1));
    (n > 0).then_some((v, c, n))
}

pub async fn run(_args: Vec<String>) {
    let settings = config::load();
    let base = settings.monthly_deploy_eur;
    if base <= 0.0 {
        println!(
            "sim needs monthly_deploy_eur (> 0) in config/settings.yaml — the € of monthly income \
             the paper portfolio invests. The deploy line in `screen` uses the same knob."
        );
        return;
    }
    let (mut snaps, corrupt) = crate::commands::track::read_snapshots();
    if corrupt > 0 {
        eprintln!("WARNING: {corrupt} corrupt line(s) in {SNAPSHOT_FILE} skipped");
    }
    if snaps.is_empty() {
        println!("No journal yet — {SNAPSHOT_FILE} appears after the first `screen` run; the sim buys from each month's first snapshot.");
        return;
    }
    snaps.sort_by(|a, b| a.date.cmp(&b.date));
    let today = chrono::Local::now().date_naive();
    let Some(now_key) = ym(&today.format("%Y-%m-%d").to_string()) else { return };
    let led = ledger(&snaps, base, now_key);
    if led.events.is_empty() {
        println!("Nothing bought yet — {SNAPSHOT_FILE} has no priced monthly snapshot to buy from.");
        return;
    }

    println!(
        "Paper DCA — the screen's own monthly advice executed with pretend money: each month's\n\
         first snapshot, equal-weight top-{BOOK} at the journaled prices, base €{base:.0} × the\n\
         deploy-line entry-state multiplier, €{SIM_FEE_EUR:.0} fee per name bought. Pure replay of\n\
         {SNAPSHOT_FILE} (rerun = recompute, no sim state). Price-only, EUR, dividends not\n\
         counted. NOT advice.\n"
    );

    // one paced fetch for today's prices: every held ticker + the benchmark leg — fetched BEFORE
    // the BUYS section so each buy line can say what that month's basket became.
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let mut tickers: Vec<String> = led
        .events
        .iter()
        .flat_map(|e| e.lots.iter().map(|(t, _, _)| t.clone()))
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

    println!("\n  BUYS — each month's advice, and what that €-basket is worth today");
    for e in &led.events {
        let names: Vec<&str> = e.lots.iter().map(|(t, _, _)| t.as_str()).collect();
        let mult = if e.mult_known {
            format!("×{}", e.mult)
        } else {
            "×1 (no S&P state journaled)".to_string()
        };
        let now = match event_now(e, &px_now) {
            Some((v, c, n)) if c > 0.0 => {
                let part = if n < e.lots.len() {
                    format!(", {n}/{} priced", e.lots.len())
                } else {
                    String::new()
                };
                format!("→ now €{v:.0} ({:+.1}%{part})", 100.0 * (v / c - 1.0))
            }
            _ => "→ now n/a".to_string(),
        };
        println!(
            "  {}  {}  invested €{:.0} (fees €{:.0})  {}  |  {}",
            e.date,
            mult,
            e.deployed,
            e.fees,
            now,
            names.join(" ")
        );
    }

    let held = holdings(&led.events);
    // totals come from the SAME map + price closure the display loop below reads, via the pure
    // (tested) fns — membership and arithmetic can't disagree with the printed rows.
    let (value, priced_cost, priced_n) = value_priced(&held, &px_now);

    println!("\n  HOLDINGS at today's prices");
    println!("  {:<10} {:>12} {:>10} {:>10} {:>8}", "TICKER", "QTY", "COST€", "VALUE€", "P/L");
    let (mut contributed, mut fees) = (0.0, 0.0);
    for e in &led.events {
        contributed += e.deployed;
        fees += e.fees;
    }
    for (t, (qty, cost)) in &held {
        match px_now(t) {
            Some(px) => {
                let v = qty * px;
                println!(
                    "  {:<10} {:>12.4} {:>10.0} {:>10.0} {:>+7.1}%",
                    t, qty, cost, v,
                    100.0 * (v / cost - 1.0)
                );
            }
            None => println!("  {:<10} {:>12.4} {:>10.0} {:>10} {:>8}", t, qty, cost, "-", "-"),
        }
    }
    if priced_n < held.len() {
        println!(
            "  ({priced_n} of {} positions priced today — unpriced names show cost only and are \
             excluded from value and P/L)",
            held.len()
        );
    }

    println!("\n  SUMMARY");
    let pl = value - priced_cost;
    let pl_pct = if priced_cost > 0.0 { 100.0 * pl / priced_cost } else { 0.0 };
    let since = led.events.first().map_or("-", |e| e.date.as_str());
    println!("  invested €{contributed:.0} since {since}  →  worth €{value:.0} today");
    println!(
        "  change: {pl:+.0}€ ({pl_pct:+.1}%)   fees paid: €{fees:.0} ({:.1}% of invested, a one-off drag)",
        if contributed > 0.0 { 100.0 * fees / contributed } else { 0.0 }
    );
    let (b_cost, b_units, b_n) = benchmark(&led.events);
    match px_now("^GSPC") {
        Some(spx_now) if b_n > 0 => {
            let b_value = b_units * spx_now;
            let b_pct = 100.0 * (b_value / b_cost - 1.0);
            let ex = pl_pct - b_pct;
            let coverage = if b_n < led.events.len() {
                format!(" — benchmark covers {b_n}/{} buys", led.events.len())
            } else {
                String::new()
            };
            let verdict = if ex.abs() < 0.05 {
                "level with the index so far".to_string()
            } else if ex > 0.0 {
                format!("screen ahead by {ex:+.1} pp")
            } else {
                format!("screen behind by {ex:+.1} pp")
            };
            println!(
                "  same money into the S&P 500 instead: €{b_value:.0} ({b_pct:+.1}%)  →  {verdict}{coverage}"
            );
        }
        _ => println!("  same money into the S&P 500 instead: n/a (no benchmark leg priced)"),
    }
    if led.pending_months > 0 {
        println!(
            "  pending cash €{:.0} ({} month(s) without a buyable snapshot — deploys at the next `screen` run)",
            f64::from(led.pending_months) * base,
            led.pending_months
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(date: &str, spx: Option<f64>, off_hi: Option<f64>, rows: &[(&str, Option<f64>)]) -> Snapshot {
        Snapshot {
            date: date.into(),
            spx,
            spx_off_hi: off_hi,
            rows: rows.iter().map(|(t, p)| (t.to_string(), *p)).collect(),
        }
    }

    /// monthly_firsts(): first snapshot of each calendar month wins regardless of input order;
    /// malformed dates drop.
    #[test]
    fn monthly_firsts_semantics() {
        let snaps = vec![
            snap("2026-07-20", None, None, &[("A", Some(1.0))]),
            snap("2026-07-16", None, None, &[("B", Some(1.0))]),
            snap("2026-08-03", None, None, &[("C", Some(1.0))]),
            snap("garbage", None, None, &[("D", Some(1.0))]),
        ];
        let firsts = monthly_firsts(&snaps);
        assert_eq!(firsts.len(), 2);
        assert_eq!(firsts[&(2026, 7)].date, "2026-07-16");
        assert_eq!(firsts[&(2026, 8)].date, "2026-08-03");
    }

    /// buy_event(): equal-weight split of the budget across priced top rows — €1 fee inside each
    /// slice; an unpriced row drops and the split grows (k=2 → bigger slices, fewer fees); no
    /// priced rows or a slice that can't clear the fee → None (the month stays cash).
    #[test]
    fn buy_event_math() {
        let s = snap("2026-07-16", None, None, &[("A", Some(10.0)), ("B", Some(20.0)), ("C", None)]);
        let e = buy_event(&s, 300.0, 1.0, false).expect("priced rows buy");
        // 2 priced rows: alloc 150 each, invested 149, qty = 149/px
        assert_eq!(e.lots.len(), 2);
        assert_eq!(e.fees, 2.0 * SIM_FEE_EUR);
        assert!((e.lots[0].1 - 149.0 / 10.0).abs() < 1e-9);
        assert!((e.lots[1].1 - 149.0 / 20.0).abs() < 1e-9);
        assert!((e.lots.iter().map(|(_, _, c)| c).sum::<f64>() - 300.0).abs() < 1e-9);

        // top-BOOK cap: an 11th row never buys
        let rows: Vec<(String, Option<f64>)> =
            (0..12).map(|i| (format!("T{i}"), Some(10.0))).collect();
        let s = Snapshot { date: "2026-07-16".into(), spx: None, spx_off_hi: None, rows };
        assert_eq!(buy_event(&s, 3000.0, 1.0, true).unwrap().lots.len(), BOOK);

        // nothing priced, or degenerate budget (slice ≤ fee) → None
        assert!(buy_event(&snap("2026-07-16", None, None, &[("A", None)]), 300.0, 1.0, false).is_none());
        assert!(buy_event(&snap("2026-07-16", None, None, &[("A", Some(1.0))]), 1.0, 1.0, false).is_none());
    }

    /// ledger(): a month with a snapshot deploys base × the SAME deploy_scaled_eur composition the
    /// screen banner uses (never re-derived); a snapshot-less month banks base ×1 into the next
    /// event; trailing empty months stay pending.
    #[test]
    fn ledger_accrual_and_multiplier() {
        let base = 1000.0;
        // July buys; August has no snapshot; September buys with July's mult math + August's cash
        let deep = Some(-25.0); // whatever state class that is, the composition must MATCH screen's
        let snaps = vec![
            snap("2026-07-16", Some(100.0), None, &[("A", Some(10.0))]),
            snap("2026-09-02", Some(100.0), deep, &[("A", Some(10.0))]),
        ];
        let led = ledger(&snaps, base, (2026, 10));
        assert_eq!(led.events.len(), 2);
        // July: no off-hi journaled → ×1 fallback, flagged unknown
        assert!(!led.events[0].mult_known);
        assert!((led.events[0].deployed - base).abs() < 1e-9);
        // September: scaled by the shared composition + August's banked base
        let (mult, scaled) = deploy_scaled_eur(base, deep).unwrap();
        assert!(led.events[1].mult_known);
        assert!((led.events[1].mult - mult).abs() < 1e-9);
        assert!((led.events[1].deployed - (scaled + base)).abs() < 1e-9);
        // October (no snapshot yet) pending
        assert_eq!(led.pending_months, 1);

        // a month whose snapshot has NO priced rows buys nothing — its income stays pending
        // (the ledger wiring of buy_event's None, not just buy_event standalone)
        let led = ledger(&[snap("2026-07-16", None, None, &[("A", None)])], base, (2026, 7));
        assert!(led.events.is_empty());
        assert_eq!(led.pending_months, 1);

        // future-dated journal line (start after `now`) → nothing to replay, no panic, no pending
        let led = ledger(&[snap("2027-01-05", None, None, &[("A", Some(1.0))])], base, (2026, 7));
        assert!(led.events.is_empty() && led.pending_months == 0);
    }

    /// holdings(): a name bought in several months is ONE position with summed qty and cost —
    /// an overwrite instead of a sum would misreport every multi-month holding silently.
    #[test]
    fn holdings_aggregate_across_events() {
        let ev = |lots: Vec<(&str, f64, f64)>| Event {
            date: "2026-07-16".into(),
            mult: 1.0,
            mult_known: true,
            deployed: 0.0,
            fees: 0.0,
            spx: None,
            lots: lots.into_iter().map(|(t, q, c)| (t.to_string(), q, c)).collect(),
        };
        let held = holdings(&[
            ev(vec![("AAPL", 1.0, 240.0), ("MSFT", 2.0, 240.0)]),
            ev(vec![("AAPL", 0.5, 120.0)]),
        ]);
        assert_eq!(held.len(), 2);
        assert_eq!(held["AAPL"], (1.5, 360.0));
        assert_eq!(held["MSFT"], (2.0, 240.0));
    }

    /// value_priced(): an unpriced-today name drops from BOTH the value AND the cost side of
    /// P/L (a one-sided leak would skew P/L% silently); the priced count exposes the hole.
    #[test]
    fn value_priced_excludes_unpriced_both_sides() {
        let held: BTreeMap<String, (f64, f64)> =
            [("UP".to_string(), (2.0, 100.0)), ("GONE".to_string(), (1.0, 999.0))].into();
        let px = |t: &str| (t == "UP").then_some(60.0);
        let (value, priced_cost, priced_n) = value_priced(&held, &px);
        assert!((value - 120.0).abs() < 1e-9);
        assert!((priced_cost - 100.0).abs() < 1e-9); // GONE's 999 cost must NOT drag P/L
        assert_eq!(priced_n, 1);
    }

    /// event_now(): one buy basket valued today under the same both-sides rule — an unpriced lot
    /// drops from value AND cost (its growth % never compares apples to a hole), the priced count
    /// exposes the gap, and a fully-unpriced basket is None (line prints n/a, not 0%).
    #[test]
    fn event_now_both_sides() {
        let e = Event {
            date: "2026-07-16".into(),
            mult: 1.0,
            mult_known: true,
            deployed: 480.0,
            fees: 2.0 * SIM_FEE_EUR,
            spx: Some(5000.0),
            lots: vec![("UP".to_string(), 2.0, 240.0), ("GONE".to_string(), 1.0, 240.0)],
        };
        let px = |t: &str| (t == "UP").then_some(150.0);
        let (v, c, n) = event_now(&e, &px).expect("one lot priced");
        assert!((v - 300.0).abs() < 1e-9);
        assert!((c - 240.0).abs() < 1e-9); // GONE.s cost drops too — both sides
        assert_eq!(n, 1);
        assert!(event_now(&e, &|_| None).is_none()); // nothing priced → n/a, not fake 0%
    }

    /// benchmark(): same gross cashflow into the journaled ^GSPC close minus one fee per event;
    /// events without a benchmark leg are skipped and the covered count says so.
    #[test]
    fn benchmark_math() {
        let mk = |deployed: f64, spx: Option<f64>| Event {
            date: "2026-07-16".into(),
            mult: 1.0,
            mult_known: true,
            deployed,
            fees: SIM_FEE_EUR,
            spx,
            lots: vec![],
        };
        let (cost, units, n) = benchmark(&[mk(1000.0, Some(100.0)), mk(500.0, None)]);
        assert_eq!(n, 1);
        assert!((cost - 1000.0).abs() < 1e-9);
        assert!((units - 999.0 / 100.0).abs() < 1e-9);
        // index +10% since → value 1098.9 vs cost 1000 → the fee drag shows up honestly
        assert!((units * 110.0 - 1098.9).abs() < 1e-6);

        // a zero ^GSPC close is not a price — the event skips like a missing leg
        assert_eq!(benchmark(&[mk(1000.0, Some(0.0))]).2, 0);
    }

    /// ym()/next_ym(): month parsing + December rollover.
    #[test]
    fn month_helpers() {
        assert_eq!(ym("2026-07-16"), Some((2026, 7)));
        assert_eq!(ym("2026-13-01"), None);
        assert_eq!(ym("junk"), None);
        assert_eq!(next_ym((2026, 12)), (2027, 1));
        assert_eq!(next_ym((2026, 7)), (2026, 8));
    }
}
