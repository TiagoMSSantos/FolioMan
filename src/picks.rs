//! Buy-candidate algorithm: rank the `check` table for "quality on sale".
//! Pure scoring + the table printer, kept together so the whole heuristic lives in one
//! place. **NOT advice** — a transparent ranking of the table, never an auto-buy.

use crate::commands::truncate;
use crate::config::{BuyHeuristic, Widths};
use crate::core::{Quote, HORIZONS};

/// Longest-available trend over a window longer than 2 years (5Y → 10Y → 20Y), i.e. the
/// structural multi-year direction. None if the asset has no >2Y history.
fn long_term_pct(q: &Quote) -> Option<f64> {
    perf_pct(q, "20Y").or_else(|| perf_pct(q, "10Y")).or_else(|| perf_pct(q, "5Y"))
}

/// How long it's been falling, rewarded by *duration*, not a coarse tier: take the longest
/// sub-5Y horizon still in the red and scale by its length in years, so a 1Y downtrend
/// earns ~2× a 6M one and ~12× a 1M one (not just +1). Bonus = `weight × years_down`
/// (1D≈0.003 … 1Y=1.0); capped below 5Y so it stays a "recent decline", not the structural
/// >2Y trend (which has its own gate). Not down at all → 0.
fn decline_bonus(q: &Quote, weight: f64) -> f64 {
    let days_down = HORIZONS
        .iter()
        .filter(|(_, d)| *d < 1825) // < 5Y: a recent decline, not the long-term trend
        .filter(|(l, _)| perf_pct(q, l).map_or(false, |p| p < 0.0))
        .map(|(_, d)| *d)
        .max()
        .unwrap_or(0);
    weight * (days_down as f64 / 365.0)
}

/// % change at a given horizon label (e.g. "1Y") from a Quote's perf, by label not index
/// (robust to HORIZONS reordering). None if that horizon has no data.
pub fn perf_pct(q: &Quote, label: &str) -> Option<f64> {
    let i = HORIZONS.iter().position(|(l, _)| *l == label)?;
    q.perf.get(i).and_then(|o| o.as_ref()).map(|(_, p)| *p)
}

/// Heuristic "quality on sale" buy score. `None` = excluded as a candidate. All gates and
/// caps come from `BuyHeuristic` (settings.yaml `buy_heuristic:`) so they're tunable.
///
/// Gates (all must pass): has a 1-year history with 1Y % above `min_1y_pct` (set this
/// **negative** to include names *down* on the year — a pullback is the whole point; a
/// positive value instead demands an uptrend); **not crashing now** (1M % above
/// `max_1m_drop_pct` — a fresh crater is a knife); and **structurally healthy** — *every*
/// multi-year horizon it actually has (5Y, 10Y, 20Y) must be above `min_long_pct`. One weak
/// long horizon rejects it: a recent decline is only worth rewarding when the long-term
/// trend is still strongly up.
///
/// Score rewards a **recent low inside an intact long-term uptrend** — "quality on sale".
/// The dominant term is the pullback off the recent high (`drawdown_pct`, the % below the
/// high over `settings.high_days` ≈ 1Y, weighted `on_sale_weight`, capped `on_sale_cap`):
/// the more it's pulled back, the higher. Then a *small* 1Y-momentum term (`y1_weight × 1Y%`,
/// capped low — momentum is a gate, not the prize, so a +400% rocket at new highs no longer
/// dominates) + proven multi-year trend (`long_weight × >2Y%`, falls back to 1Y, capped
/// `long_cap`) + a decline-duration bonus (the longer it's been falling, up to 1Y, the bigger).
/// Finally the score is **halved if the asset has no 10Y history**: the whole thesis is "intact
/// long-term uptrend", and you can't trust that without a decade of track record.
/// Higher = more interesting. **NOT advice** — a ranking of the table, not a forecast.
pub fn buy_score(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
    let y1 = perf_pct(q, "1Y")?;
    if y1 <= t.min_1y_pct {
        return None; // structural-uptrend gate
    }
    if perf_pct(q, "1M").unwrap_or(0.0) <= t.max_1m_drop_pct {
        return None; // crashing this month -> falling knife, not "on sale"
    }
    // every long horizon it has must be up: a dip is only a buy if 5Y AND 10Y AND 20Y hold.
    for label in ["5Y", "10Y", "20Y"] {
        if perf_pct(q, label).map_or(false, |p| p <= t.min_long_pct) {
            return None; // a negative multi-year leg -> structural loser, not a dip
        }
    }
    let long = long_term_pct(q).or(Some(y1)).unwrap_or(0.0); // >2Y trend, or fall back to 1Y
    let on_sale = q.drawdown_pct.min(t.on_sale_cap); // % off the ~1Y high — the real discount
    let score = t.on_sale_weight * on_sale
        + t.y1_weight * y1.min(t.y1_cap)
        + t.long_weight * long.min(t.long_cap)
        + decline_bonus(q, t.decline_weight);
    // no 10Y track record -> can't trust the "intact long-term uptrend" thesis -> halve.
    let penalty = if perf_pct(q, "10Y").is_none() { 0.5 } else { 1.0 };
    Some(score * penalty)
}

/// Horizons whose Δ% is shown in the picks table (chronological).
const DIFF_HORIZONS: &[&str] = &["1D", "1W", "1M", "1Y", "5Y", "10Y", "20Y"];

/// Print the Top-N buy candidates derived from the already-fetched quotes (no network).
pub fn render(qs: &[Quote], n: usize, t: &BuyHeuristic, w: &Widths) {
    let (nw, tw) = (w.name, w.ticker);
    let mut picks: Vec<(&Quote, f64)> =
        qs.iter().filter_map(|q| buy_score(q, t).map(|s| (q, s))).collect();
    picks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first
    println!(
        "\nTop {n} buy candidates — quality-on-sale heuristic: a recent low (most below its \
         ~1Y high, OFF-HI) inside an intact long-term (5Y+) uptrend. NOT advice, just a ranking:"
    );
    if picks.is_empty() {
        println!("  (none pass the gates)");
        return;
    }
    let diff_hdr = DIFF_HORIZONS
        .iter()
        .map(|l| format!("{:>8}", l))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "  {:<4} {:<nw$} {:<tw$} {:>13} {diff_hdr} {:>7} {:>8}",
        "RANK", truncate("NAME", nw), truncate("TICKER", tw), "PRICE(EUR)", "OFF-HI", "SCORE"
    );
    for (i, (q, score)) in picks.iter().take(n).enumerate() {
        let diffs = DIFF_HORIZONS
            .iter()
            .map(|l| format!("{:>8}", perf_pct(q, l).map_or("n/a".to_string(), |v| format!("{:+.1}%", v))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  {:<4} {:<nw$} {:<tw$} {:>13} {diffs} {:>7} {:>8.2}",
            i + 1,
            truncate(&q.name, nw),
            truncate(&q.ticker, tw),
            q.price,
            format!("-{:.1}%", q.drawdown_pct), // % below the ~1Y high (real pullback, not 30d)
            score,
        );
    }
}

/// Buy-heuristic asserts (no network). Run by the `selftest` subcommand and the unit test.
pub fn selftest() {
    // build a Quote with chosen horizon %s set (others n/a), robust to HORIZONS order. First
    // arg = drawdown_pct (% below the ~1Y high) — the on-sale signal the score is built on.
    let q = |drawdown_pct: f64, labels: &[(&str, f64)]| -> Quote {
        let perf = HORIZONS
            .iter()
            .map(|(l, _)| labels.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
            .collect();
        Quote {
            ticker: "T".into(), price: "€1.00".into(), dip: "-5.0%".into(), drop_pct: drawdown_pct,
            market: "USA".into(), head: String::new(), news_block: String::new(), perf,
            name: "n".into(), trend: String::new(), at_ath: false, at_atl: false, mom_pct: None,
            div_eur: Vec::new(), price_eur: None, drawdown_pct,
        }
    };
    let t = BuyHeuristic::default(); // on_sale_w 1.0/cap 60, y1_w 0.05/cap 50, long_w 0.05/cap 300
    assert_eq!(perf_pct(&q(5.0, &[("1Y", 20.0)]), "1Y"), Some(20.0));
    assert_eq!(perf_pct(&q(5.0, &[]), "1Y"), None);
    // score asserts carry a 10Y leg so the no-10Y halving penalty stays 1.0 (clean numbers).
    // score = 1.0*off-hi(5) + 0.05*20(1Y) + 0.05*20(long=10Y) = 5 + 1 + 1 = 7
    assert!((buy_score(&q(5.0, &[("1Y", 20.0), ("10Y", 20.0)]), &t).unwrap() - 7.0).abs() < 1e-9);
    // off-hi caps at 60: drawdown 80 -> 60; + 0.05*10 + 0.05*10 = 60 + 0.5 + 0.5 = 61
    assert!((buy_score(&q(80.0, &[("1Y", 10.0), ("10Y", 10.0)]), &t).unwrap() - 61.0).abs() < 1e-9);
    // missing 10Y history halves the score (long-term uptrend unverifiable)
    let with10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    let no10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0)]), &t).unwrap();
    assert!((no10 - with10 / 2.0).abs() < 1e-9);
    // a deep pullback (off-hi 40) outranks a rocket at new highs (off-hi 0) despite weaker 1Y
    let pullback = buy_score(&q(40.0, &[("1Y", 30.0), ("5Y", 50.0)]), &t).unwrap();
    let rocket = buy_score(&q(0.0, &[("1Y", 400.0), ("5Y", 500.0)]), &t).unwrap();
    assert!(pullback > rocket, "on-sale name must beat the rocket: {pullback} vs {rocket}");
    // long-term term uses >2Y when present: off-hi(5) + 0.05*10 + 0.05*40(10Y) = 5+0.5+2 = 7.5
    assert!((buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap() - 7.5).abs() < 1e-9);
    // decline-duration bonus scales with time down (weight 1.0): base 7.5, +7/365 for 1W down
    let base = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap(); // 7.5
    let short = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1W", -3.0)]), &t).unwrap();
    assert!((short - base - 7.0 / 365.0).abs() < 1e-9);
    // longer decline scores proportionally higher: 6M (182d) >> 1W (7d)
    let longer = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1W", -3.0), ("6M", -2.0)]), &t).unwrap();
    assert!(longer > short && (longer - base - 182.0 / 365.0).abs() < 1e-9);
    // knife gate: deep 1M crash excludes even with a positive year
    assert!(buy_score(&q(40.0, &[("1Y", 10.0), ("1M", -30.0)]), &t).is_none());
    // structural-decline gate: negative >2Y trend excludes even with a positive year
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", -3.0)]), &t).is_none());
    // ALL long horizons must hold: 5Y up but 10Y down still rejects
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", -5.0)]), &t).is_none());
    // all three positive + a sustained sub-5Y decline qualifies (6M down -> +182/365)
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("6M", -2.0), ("5Y", 40.0), ("10Y", 80.0), ("20Y", 200.0)]), &t).is_some());
    assert!(buy_score(&q(5.0, &[("1Y", -5.0)]), &t).is_none()); // declining year -> excluded
    assert!(buy_score(&q(5.0, &[]), &t).is_none()); // no 1Y data -> excluded
    assert!(buy_score(&Quote::stub("X", "err", "", "X"), &t).is_none()); // err row -> excluded
}
