//! `backtest [YEARS] [TICKERS...]` — zero-EXTRA-fetch sanity check of the buy heuristic. One chart
//! fetch per ticker (the same single call `check` makes — no worse rate-limit pressure), then it all
//! happens offline:
//!
//! - **(#3) walk-forward**: score the name at MANY cutoffs (~every 6 months back through its history),
//!   each measured against the realized return over the SAME `YEARS`-long forward window. Pools ~10×
//!   the samples a single as-of date gives, killing lucky-single-date bias — from the very same fetch.
//! - **(#1) peer-relative**: each cutoff's realized return is de-meaned within its ~6-month bucket, so the
//!   correlation measures SELECTION (did the name beat its same-period peers?), not the bull/bear regime
//!   every pooled cutoff would otherwise share. Without it the score races calendar luck, not skill.
//! - **(#2) out-of-sample split**: pooled rank-correlation on the EARLY half of cutoffs vs the LATE
//!   half. Early ≈ late = a stable signal; late collapsing/flipping = in-sample overfit or a dead regime.
//! - **(#1/#6) ablation**: switch each score weight OFF, recompute the pooled correlation, show the
//!   change. A term whose removal barely moves the correlation carries no ranking signal here — a prune
//!   candidate. dividend/PE read ~0 BY CONSTRUCTION (#6): `backtest_quote` can't reconstruct as-of
//!   dividends or P/E, so those weights are inert in the backtest and CANNOT be validated by it.
//! - **(#5) survivorship**: the universe is names that SURVIVED to today, so realized returns are
//!   biased UP. Flagged in the footer — treat the edge as optimistic, never a forecast.
//!
//! Defaults to the settings.yaml watchlist (small, cheap). Pass tickers to test others.

use crate::config::BuyHeuristic;
use crate::picks::buy_score;
use crate::{config, core, fetch};
use chrono::Datelike;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;

/// One scored observation: the cutoff date it was scored on, the score, and the realized forward
/// return over the holdout. The Quote is kept so ablation can re-score it under a mutated knob set
/// with ZERO re-fetch / re-math.
struct Sample {
    date: chrono::NaiveDate,
    score: f64,
    realized: f64, // raw forward return %
    relative: f64, // (#1) realized minus its cutoff-bucket peer mean -> SELECTION, not regime beta
    q: core::Quote,
}

/// (#1) Cross-sectional peer-group key: the ~6-month bucket a cutoff falls in (2 buckets/year). Names
/// scored in the same half-year are compared against EACH OTHER, so the score is judged on selection
/// skill, not the bull/bear regime every pooled cutoff otherwise shares.
fn bucket(d: chrono::NaiveDate) -> i32 {
    d.year() * 2 + d.month0() as i32 / 6
}

/// ~6 months between walk-forward cutoffs (trading sessions, ~252/yr). Overlapping forward windows —
/// fine for a rank correlation, not an independent-sample t-test (flagged in the footer).
const STEP_SESSIONS: usize = 126;
/// Need ~3y of history BEFORE a cutoff to form/score the long trend fairly.
const MIN_HISTORY: usize = 750;

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let t = &settings.buy_heuristic;

    // first purely-numeric arg = holdout years; everything else = tickers to test
    let mut years: i64 = 5;
    let mut tickers: Vec<String> = Vec::new();
    for a in &args {
        match a.parse::<i64>() {
            Ok(y) if tickers.is_empty() && y > 0 => years = y,
            _ => tickers.push(a.clone()),
        }
    }
    if tickers.is_empty() {
        tickers = settings.tickers.clone();
    }
    eprintln!(
        "backtest: {} tickers, WALK-FORWARD scoring every ~6mo with a {years}y forward holdout each…",
        tickers.len()
    );

    // (#3) per ticker, score at many cutoffs and pair each with its YEARS-forward realized return.
    let per_ticker: Vec<Vec<Sample>> = stream::iter(tickers.iter())
        .map(|tk| {
            let client = &client;
            let urls = &settings.urls;
            async move {
                let (dates, closes) = match fetch::fetch_history(client, urls, tk).await {
                    Some(x) => x,
                    None => return Vec::new(),
                };
                let mut out = Vec::new();
                let mut i = MIN_HISTORY;
                while i < dates.len() {
                    // forward index: first session at least `years` past the as-of date
                    let target = dates[i] + chrono::Duration::days(years * 365);
                    match dates[i..].iter().position(|d| *d >= target) {
                        Some(off) => {
                            let fwd = i + off;
                            let q = core::backtest_quote(tk, &dates, &closes, i);
                            if let Some(score) = buy_score(&q, t) {
                                let realized = (closes[fwd] / closes[i] - 1.0) * 100.0;
                                out.push(Sample { date: dates[i], score, realized, relative: 0.0, q });
                            }
                        }
                        None => break, // no full forward window left -> stop walking this ticker
                    }
                    i += STEP_SESSIONS;
                }
                out
            }
        })
        .buffer_unordered(fetch::FETCH_CONCURRENCY)
        .collect()
        .await;

    let mut samples: Vec<Sample> = per_ticker.into_iter().flatten().collect();
    if samples.len() < 4 {
        println!(
            "backtest: only {} scored windows across the watchlist passed the gates — too few to correlate.",
            samples.len()
        );
        return;
    }
    samples.sort_by_key(|s| s.date); // chronological -> the OOS split is early-vs-late in time

    // (#1) de-mean realized return WITHIN each ~6-month cutoff bucket. Pooling raw returns across cutoffs
    // that span different regimes makes the score race CALENDAR LUCK (a 2016 cutoff that mooned vs a
    // 2021-top cutoff that crashed), not stock-picking. Subtracting the bucket's peer mean leaves only
    // "did this name beat the others scored the same half-year" = the selection signal we actually want.
    let mut sums: HashMap<i32, (f64, usize)> = HashMap::new();
    for s in &samples {
        let e = sums.entry(bucket(s.date)).or_insert((0.0, 0));
        e.0 += s.realized;
        e.1 += 1;
    }
    for s in &mut samples {
        let (sum, n) = sums[&bucket(s.date)];
        s.relative = s.realized - sum / n as f64;
    }
    // invariant: per-bucket de-meaning makes the relatives sum to ~0 (each bucket nets out). Fails loudly
    // if the bucket map and the fill ever drift apart. Cheap, runs in release.
    let rel_sum: f64 = samples.iter().map(|s| s.relative).sum();
    assert!(rel_sum.abs() < 1e-3 * samples.len() as f64, "de-mean broken: relatives sum to {rel_sum}");

    let scores: Vec<f64> = samples.iter().map(|s| s.score).collect();
    let rels: Vec<f64> = samples.iter().map(|s| s.relative).collect();
    let rho = core::spearman(&scores, &rels);

    println!("\nBacktest — WALK-FORWARD buy score vs {years}y-forward PEER-RELATIVE return (de-meaned per ~6mo cutoff):");
    println!("  windows scored: {}   tickers: {}", samples.len(), tickers.len());
    match rho {
        Some(v) => println!(
            "  Spearman(score, peer-relative): {v:+.2}   [+1 ranks winners perfectly, 0 no signal, − backwards]"
        ),
        None => println!("  Spearman: n/a"),
    }

    // practical read: did the top-scored half beat its same-period peers more than the bottom half?
    let mut by_score: Vec<&Sample> = samples.iter().collect();
    by_score.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let half = by_score.len() / 2;
    let mean = |s: &[&Sample]| s.iter().map(|x| x.relative).sum::<f64>() / s.len().max(1) as f64;
    let (top, bot) = (mean(&by_score[..half]), mean(&by_score[by_score.len() - half..]));
    println!("  top-half peer-relative {top:+.1} pts  vs  bottom-half {bot:+.1} pts  ->  edge {:+.1} pts", top - bot);

    // (#2) OUT-OF-SAMPLE: rho on the early half of cutoffs vs the late half. Late ≪ early or a sign
    // flip = the edge is in-sample overfit / a regime that already passed, not a durable signal.
    let mid = samples.len() / 2;
    let (early, late) = samples.split_at(mid);
    let split_rho = |s: &[Sample]| {
        core::spearman(
            &s.iter().map(|x| x.score).collect::<Vec<_>>(),
            &s.iter().map(|x| x.relative).collect::<Vec<_>>(),
        )
        .map_or("n/a".to_string(), |v| format!("{v:+.2}"))
    };
    println!(
        "\nOut-of-sample (split at {}): early rho {}  |  late rho {}",
        samples[mid].date,
        split_rho(early),
        split_rho(late)
    );
    println!("  (early ≈ late -> stable; late ≪ early or a sign flip -> overfit / regime-bound)");

    // (#1 + #6) ABLATION: zero each SCORE weight in turn, recompute pooled rho, show the delta. Gates
    // are untouched, so the SAME rows survive -> `abl` stays aligned with `rels`. A term whose removal
    // barely moves rho carries no ranking signal on this data -> a prune candidate.
    let base_rho = rho.unwrap_or(0.0);
    println!("\nAblation — pooled rho with each score term OFF (Δ vs full {base_rho:+.2}):");
    let knobs: &[(&str, fn(&mut BuyHeuristic))] = &[
        ("long_trend_weight", |t| t.long_trend_weight = 0.0),
        ("cheap_weight", |t| t.cheap_weight = 0.0),
        ("dividend_weight*", |t| t.dividend_weight = 0.0),
        ("sharpe_weight", |t| t.sharpe_weight = 0.0),
        ("calmar_weight", |t| t.calmar_weight = 0.0),
        ("consistency", |t| t.consistency_floor = 1.0),
    ];
    for (name, mutate) in knobs {
        let mut t2 = t.clone();
        mutate(&mut t2);
        let abl: Vec<f64> = samples.iter().map(|s| buy_score(&s.q, &t2).unwrap_or(s.score)).collect();
        match core::spearman(&abl, &rels) {
            Some(v) => println!("  {:<20} rho {v:+.2}   Δ {:+.2}", name, v - base_rho),
            None => println!("  {:<20} rho n/a", name),
        }
    }

    println!("\nCaveats:");
    println!("  • Peer-relative (#1): returns are de-meaned per ~6mo cutoff, so rho is SELECTION vs same-period");
    println!("    peers (regime beta removed). A near-empty bucket has a weak peer set -> its rows count for less.");
    println!("  • In-sample: knobs were hand-tuned on today's data; even the OOS split shares the regime.");
    println!("  • Survivorship (#5): the universe is names that SURVIVED to today — dead tickers never enter,");
    println!("    so realized returns are biased UP. Treat the edge as optimistic.");
    println!("  • Price-only (#6): no as-of dividends or P/E reconstructed; the * term above is inert here.");
    println!("  • Overlapping 6-mo windows share price paths -> samples aren't independent; rho is directional.");
}
