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
//!   — a prune candidate. dividend/PE read ~0 BY CONSTRUCTION (#6): `backtest_quote` can't reconstruct
//!   as-of dividends or P/E, so those weights are inert in the backtest and CANNOT be validated by it.
//! - **(#5) survivorship**: the universe is names that SURVIVED to today, so realized returns are
//!   biased UP. Flagged in the footer — treat the edge as optimistic, never a forecast.
//!
//! Defaults to the settings.yaml watchlist (small, cheap). Pass tickers to test others, or the keyword
//! `universe` to test the whole live screen universe (#2 — a far wider, less single-name-lucky sample).

use crate::config::BuyHeuristic;
use crate::core::Quote;
use crate::picks::{buy_score, growth_score};
use crate::{config, core, fetch};
use chrono::Datelike;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;

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
    quote: Quote,
    fund: Option<core::FundFactors>, // (G) as-of fundamentals at this cutoff (None unless `fund` + FMP key + cached)
}

/// (#1) Cross-sectional peer-group key: the ~6-month bucket a cutoff falls in (2 buckets/year). Names
/// scored in the same half-year are compared against EACH OTHER, so the score is judged on selection
/// skill, not the bull/bear regime every pooled cutoff otherwise shares.
fn bucket(d: chrono::NaiveDate) -> i32 {
    d.year() * 2 + d.month0() as i32 / 6
}

/// (#1) De-mean each cutoff's realized return WITHIN its ~6-month bucket -> `relative` (the selection
/// signal). Pure + testable; the runtime sum-to-~0 invariant check stays in `run`.
fn demean(samples: &mut [Sample]) {
    let mut sums: HashMap<i32, (f64, usize)> = HashMap::new();
    for s in samples.iter() {
        let e = sums.entry(bucket(s.date)).or_insert((0.0, 0));
        e.0 += s.realized;
        e.1 += 1;
    }
    for s in samples.iter_mut() {
        let (sum, n) = sums[&bucket(s.date)];
        s.relative = s.realized - sum / n as f64;
    }
}

/// ~6 months between walk-forward cutoffs (trading sessions, ~252/yr). Overlapping forward windows —
/// fine for a rank correlation, not an independent-sample t-test (flagged in the footer).
const STEP_SESSIONS: usize = 126;
/// Need ~3y of history BEFORE a cutoff to form/score the long trend fairly.
const MIN_HISTORY: usize = 750;

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let tuning = &settings.buy_heuristic;

    // first purely-numeric arg = holdout years; the keyword `universe` = test the live screen universe
    // (#2: a much wider sample than the ~50-name watchlist -> less single-name luck); everything else =
    // explicit tickers to test.
    let mut years: i64 = 5;
    let mut wide = false;
    let mut long = false;
    let mut fund = false;
    let mut tune = false;
    let mut tickers: Vec<String> = Vec::new();
    for a in &args {
        match a.parse::<i64>() {
            Ok(y) if tickers.is_empty() && y > 0 => years = y,
            _ if a.eq_ignore_ascii_case("universe") => wide = true,
            _ if a.eq_ignore_ascii_case("long") => long = true,
            _ if a.eq_ignore_ascii_case("fund") => fund = true,
            _ if a.eq_ignore_ascii_case("tune") => tune = true,
            _ => tickers.push(a.clone()),
        }
    }
    if fund && std::env::var("FMP_API_KEY").ok().filter(|k| !k.is_empty()).is_none() {
        eprintln!("backtest: `fund` set but FMP_API_KEY is empty — the fundamental lane will be empty (price lanes still run).");
    }
    // Daily 10y history caps the forward window at ~5y (the 3y warmup eats the rest of the 10y), so a
    // hold of ~8y+ — or an explicit `long` — switches to the MAX MONTHLY series: decades of bars for
    // old names, at the cost of monthly cadence (vol/MA are bar-approximations, see backtest_quote).
    let monthly = long || years >= 8;
    let cadence = if monthly { 12 } else { 252 }; // bars per year
    let min_history = if monthly { 36 } else { MIN_HISTORY }; // ~3y of bars before a cutoff to form the long trend
    let step = if monthly { 6 } else { STEP_SESSIONS }; // ~6 months between cutoffs
    if wide && tickers.is_empty() {
        // (#2) widen to the live screen universe (crypto + S&P 500 + Xetra UCITS ETFs) for a far bigger
        // sample. Slower (one history fetch per name) but the only cure for 53-survivor-ticker noise.
        eprintln!("backtest: fetching the live screen universe (this is the slow, wide-sample path)…");
        // no sector filter (&[]): the backtest measures edge across the FULL sample, never a slice
        tickers =
            fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, &[]).await.0;
    } else if tickers.is_empty() {
        tickers = settings.tickers.clone();
    }
    eprintln!(
        "backtest: {} tickers, WALK-FORWARD scoring every ~6mo with a {years}y forward holdout each ({} history)…",
        tickers.len(),
        if monthly { "MAX monthly" } else { "10y daily" }
    );

    // (#3) per ticker, score at many cutoffs and pair each with its YEARS-forward realized return.
    let per_ticker: Vec<Vec<Sample>> = stream::iter(tickers.iter())
        .map(|tk| {
            let client = &client;
            let urls = &settings.urls;
            let factor = settings.buy_heuristic.growth_fund_factor.as_str(); // (G) config-selected as-of factor
            async move {
                let fetched = if monthly {
                    fetch::fetch_history_long(client, urls, tk).await
                } else {
                    fetch::fetch_history(client, urls, tk).await
                };
                let (dates, closes) = match fetched {
                    Some(x) => x,
                    None => return Vec::new(),
                };
                // (G) one cached fundamentals fetch per ticker (only when `fund`); as-of factors are then
                // derived per cutoff from these rows with no further network. None -> the fund lane skips it.
                let fund_rows = if fund { fetch::fetch_fundamentals_history(client, urls, tk).await } else { None };
                let mut out = Vec::new();
                let mut i = min_history;
                while i < dates.len() {
                    // forward index: first session at least `years` past the as-of date
                    let target = dates[i] + chrono::Duration::days(years * 365);
                    match dates[i..].iter().position(|d| *d >= target) {
                        Some(off) => {
                            let fwd = i + off;
                            // record EVERY cutoff with a forward window (not just gated ones) so the
                            // peer-mean spans the whole period universe; each lane filters by its own gates.
                            let mut quote = core::backtest_quote(tk, &dates, &closes, i, cadence);
                            let realized = (closes[fwd] / closes[i] - 1.0) * 100.0;
                            let fund = fund_rows.as_ref().map(|r| core::fund_factors(r, dates[i], years));
                            // (G) fold the as-of factor INTO the growth lane so growth_fund_weight is ablatable.
                            // WHICH factor is config-driven (`growth_fund_factor`, default "rev_accel") — set it
                            // in settings.yaml to whichever report_fund_lane (below) shows +rho + both-half OOS,
                            // no recompile. Price-only backtest (no `fund`/key) leaves this None -> growth_score
                            // neutral -> validated edge untouched.
                            quote.fund_factor = fund.as_ref().and_then(|f| core::select_fund_factor(f, factor));
                            out.push(Sample { date: dates[i], realized, relative: 0.0, quote, fund });
                        }
                        None => break, // no full forward window left -> stop walking this ticker
                    }
                    i += step;
                }
                out
            }
        })
        .buffer_unordered(fetch::fetch_concurrency())
        .collect()
        .await;

    let mut samples: Vec<Sample> = per_ticker.into_iter().flatten().collect();
    if samples.len() < 4 {
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

    // (#1) de-mean realized return WITHIN each ~6-month cutoff bucket. Pooling raw returns across cutoffs
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

    let buy_knobs: &[Knob] = &[
        ("long_trend_weight", |tuning| tuning.long_trend_weight = 0.0),
        ("discount_weight", |tuning| tuning.discount_weight = 0.0), // (#4) zero the dip reward — Δ>0 confirms dip-depth ranks backwards
        ("cheap_weight", |tuning| tuning.cheap_weight = 0.0),
        ("dividend_weight*", |tuning| tuning.dividend_weight = 0.0),
        ("onsale_sharpe_weight", |tuning| tuning.onsale_sharpe_weight = 0.0),
        ("calmar_weight", |tuning| tuning.calmar_weight = 0.0),
    ];
    let growth_knobs: &[Knob] = &[
        ("growth_trend_weight", |tuning| tuning.growth_trend_weight = 0.0),
        ("growth_accel_weight", |tuning| tuning.growth_accel_weight = 0.0),
        ("sharpe_weight", |tuning| tuning.sharpe_weight = 0.0),
        ("calmar_weight", |tuning| tuning.calmar_weight = 0.0),
        ("overext_brake", |tuning| tuning.growth_overext_cap = 0.0),
        ("growth_fund_weight", |tuning| tuning.growth_fund_weight = 0.0), // (G) Δ shows the as-of fund factor's through-the-lane edge; ~0 when weight is already 0 (default) or no fund coverage
        ("growth_mom121_weight", |tuning| tuning.growth_mom121_weight = 0.0), // (M) Δ shows the 12-1 momentum term's through-the-lane edge; ~0 when weight is 0 (default)
    ];
    report_lane("ON-SALE (buy_score)", &samples, buy_score, tuning, buy_knobs);
    report_lane("GROWTH (growth_score)", &samples, growth_score, tuning, growth_knobs);
    if fund {
        report_fund_lane(&samples);
    }

    println!("\nCaveats:");
    println!("  • Peer-relative (#1): returns are de-meaned per ~6mo cutoff, so rho is SELECTION vs same-period");
    println!("    peers (regime beta removed). A near-empty bucket has a weak peer set -> its rows count for less.");
    println!("  • In-sample: knobs were hand-tuned on today's data; even the OOS split shares the regime.");
    println!("  • Survivorship (#5): the universe is names that SURVIVED to today — dead tickers never enter,");
    println!("    so realized returns are biased UP. Treat the edge as optimistic.");
    println!("  • Price-only (#6): no as-of dividends or P/E reconstructed; the * term above is inert here.");
    println!("  • Overlapping 6-mo windows share price paths -> samples aren't independent; rho is directional.");
    if monthly {
        println!("  • Long-horizon (MAX monthly): only names alive for the FULL {years}y window enter, so");
        println!("    survivorship bias is WORSE than the daily path, and vol/MA are monthly-bar approximations.");
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
        ("long_trend_cap", |t| t.long_trend_cap, |t, v| t.long_trend_cap = v, 15.0, 60.0),
        ("growth_accel_weight", |t| t.growth_accel_weight, |t, v| t.growth_accel_weight = v, 0.0, 0.6),
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
    let mut best: Option<(f64, BuyHeuristic)> = None; // (train edge, config)
    for _ in 0..draws {
        let mut t = default.clone();
        for &(_, _, set, lo, hi) in &active {
            set(&mut t, lo + next() * (hi - lo));
        }
        let (rho, edge) = lane_metrics(train, growth_score, &t);
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
type Knob = (&'static str, fn(&mut BuyHeuristic));

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

    // ablation: zero each knob, re-score the SAME gated rows, recompute BOTH metrics. Δrho = rank
    // selection change; Δedge = profit-spread change (the one that matters). +Δ ⇒ the knob HURT,
    // −Δ ⇒ it HELPED. Watch for sign disagreement: a knob that's −Δedge (load-bearing for profit)
    // but ~0/+Δrho is a trap — don't delete it on the rho reading alone.
    let base_rho = rho.unwrap_or(0.0);
    println!("  ablation (Δ vs full: rho {base_rho:+.2}, edge {base_edge:+.1}):");
    for (name, mutate) in knobs {
        let mut t2 = tuning.clone();
        mutate(&mut t2);
        let abl: Vec<(&Sample, f64)> =
            scored.iter().map(|(s, v)| (*s, scorer(&s.quote, &t2).unwrap_or(*v))).collect();
        let (et, eb) = edge_halves(&abl);
        let dedge = (et - eb) - base_edge;
        match core::spearman(&abl.iter().map(|(_, v)| *v).collect::<Vec<_>>(), &rels) {
            Some(v) => println!("    {:<20} rho {v:+.2} Δ{:+.2}   edge {:+.1} Δ{dedge:+.1}", name, v - base_rho, et - eb),
            None => println!("    {:<20} rho n/a   edge {:+.1} Δ{dedge:+.1}", name, et - eb),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }
    fn sample(date: NaiveDate, realized: f64) -> Sample {
        Sample { date, realized, relative: 0.0, quote: Quote::stub("X", "1", "", "X"), fund: None }
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

    /// A synthetic scorer reading one quote field — lets us test `lane_metrics`/`edge_halves` (the
    /// honest-OOS machinery the search drives) WITHOUT building quotes that pass growth_score's gate
    /// maze (those gates are exercised in `picks`). Score = drawdown_pct, set per sample below.
    fn dd_score(q: &Quote, _: &BuyHeuristic) -> Option<f64> {
        Some(q.drawdown_pct)
    }
    fn s_rel(relative: f64, dd: f64) -> Sample {
        let mut q = Quote::stub("X", "1", "", "X");
        q.drawdown_pct = dd;
        Sample { date: ymd(2020, 1, 1), realized: relative, relative, quote: q, fund: None }
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
}
