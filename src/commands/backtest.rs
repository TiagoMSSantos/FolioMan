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
//! Add `stress` to inject known crashed/delisted losers into whatever pool is tested (#6) — compare the
//! rho/edge against the same run without it to see how much of the edge is survivorship bias.

use crate::config::BuyHeuristic;
use crate::core::Quote;
use crate::picks::{buy_score, growth_score};
use crate::{config, core, fetch, picks};
use chrono::Datelike;
use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, HashMap, HashSet};

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

/// The unconditional held-book verdict of one wide backtest run: the "all entries" row of the
/// entry-state table (full gated pool, growth_score ranking, equal-weight top-10, held `years`
/// forward, vs the index) plus the run date and a fingerprint of the tuning that earned it.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Verdict {
    pub(crate) date: String,
    pub(crate) years: i64,
    pub(crate) windows: usize,
    pub(crate) book: f64,
    pub(crate) excess: f64,
    pub(crate) win: f64,
    pub(crate) worst: f64,
    pub(crate) oos_early: f64,
    pub(crate) oos_late: f64,
    pub(crate) tuning_fp: String,
}

/// The ONE fingerprint both surfaces use (backtest stamps it, screen compares it) — a tuning
/// knob changed since the run means the cited numbers were never earned by the current settings.
pub(crate) fn tuning_fingerprint(t: &BuyHeuristic) -> String {
    serde_json::to_string(t).unwrap_or_default()
}

/// Malformed/corrupt JSON is None — a broken file must SILENCE the screen line, never
/// fabricate a verdict. (Kept pure and separate from the fs read so the failure mode is tested.)
pub(crate) fn parse_verdict(raw: &str) -> Option<Verdict> {
    serde_json::from_str(raw).ok()
}

pub(crate) fn read_verdict() -> Option<Verdict> {
    parse_verdict(&std::fs::read_to_string(config::data_path(VERDICT_FILE)).ok()?)
}

fn write_verdict(v: &Verdict) {
    let ok = serde_json::to_string(v)
        .ok()
        .and_then(|s| std::fs::write(config::data_path(VERDICT_FILE), s).ok());
    match ok {
        Some(()) => eprintln!("backtest: method verdict journaled — the screen footer will cite it"),
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
        "Method backtest (run {}, wide universe, top-10 held {}y, {} windows): book {:+.1}%/yr, \
         {:+.1}pp/yr vs index, win {:.0}%, worst {:+.1}, OOS {:+.1}/{:+.1}{tail}",
        v.date, v.years, v.windows, v.book, v.excess, v.win, v.worst, v.oos_early, v.oos_late
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
    quote: Quote,
    fund: Option<core::FundFactors>, // (G) as-of fundamentals at this cutoff (None unless `fund` + FMP key + cached)
    trail: Vec<f64>, // (round 112) up to 36 trailing monthly returns % at the cutoff — CORR-CAP probe input; empty = can't judge
}

/// (#1) Cross-sectional peer-group key: the ~6-month bucket a cutoff falls in (2 buckets/year). Names
/// scored in the same half-year are compared against EACH OTHER, so the score is judged on selection
/// skill, not the bull/bear regime every pooled cutoff otherwise shares.
fn bucket(d: chrono::NaiveDate) -> i32 {
    d.year() * 2 + d.month0() as i32 / 6
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
    let mut insider = false;
    let mut halflife = false;
    let mut stress = false;
    let mut tickers: Vec<String> = Vec::new();
    for a in &args {
        match a.parse::<i64>() {
            Ok(y) if tickers.is_empty() && y > 0 => years = y,
            _ if a.eq_ignore_ascii_case("universe") => wide = true,
            _ if a.eq_ignore_ascii_case("long") => long = true,
            _ if a.eq_ignore_ascii_case("fund") => fund = true,
            _ if a.eq_ignore_ascii_case("tune") => tune = true,
            _ if a.eq_ignore_ascii_case("insider") => insider = true, // (Item 4) also pull SEC Form-4 net buys
            _ if a.eq_ignore_ascii_case("halflife") => halflife = true, // (Item 11) hold-period net-edge sweep
            _ if a.eq_ignore_ascii_case("stress") => stress = true,   // (#6) inject crashed/delisted losers
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
    // (#18) so measure_endpoint's trading-days smoothing span covers the same calendar time on
    // monthly bars as it does on the live daily closes (train == serve).
    core::set_measure_cadence(cadence);
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
        hold_period_sweep(&client, &settings.urls, &tickers, monthly, cadence, min_history, step, tuning).await;
        return;
    }

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
                let (dates, closes, native_ccy) = match fetched {
                    Some(x) => x,
                    None => return Vec::new(),
                };
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
                let fx = match filer_ccy.as_deref() {
                    Some(f) if core::needs_fx(&native_ccy, f) => {
                        // same `monthly` the closes came from: rates and prices must span the same era
                        Some(fetch::fx_factor_series(client, urls, &native_ccy, f, monthly).await)
                    }
                    _ => None, // same books, or a side unknown -> leave the close alone (legacy path)
                };
                // (Item 4) one cached SEC Form-4 fetch per ticker (only when `insider`); net buys are then
                // derived per cutoff from these transactions with no further network. None -> factor skips.
                let insider_txns = if insider { fetch::fetch_insider_history(client, urls, tk).await } else { None };
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
                            let realized = (closes[fwd] / closes[i] - 1.0) * 100.0;
                            if !realized.is_finite() {
                                // zero/garbage close -> ±inf poisons the demeaned bucket; skip the cutoff
                                i += step;
                                continue;
                            }
                            let mut quote = core::backtest_quote(tk, &dates, &closes, i, cadence);
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
                                    Some(s) => core::rate_as_of(s, dates[i]).map(|r| closes[i] * r),
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
                            // (round 112) trailing monthly returns for the CORR-CAP probe — this is the only
                            // place with the raw series in scope. 36 months ≈ the 200wk trend window. A
                            // zero/non-finite close drops that month (rare; alignment slippage is acceptable
                            // for a correlation probe, and corr_tail() demands 12 overlapping months anyway).
                            let lo = i.saturating_sub(36);
                            let trail: Vec<f64> = (lo..i)
                                .filter(|&j| closes[j] > 0.0 && closes[j + 1].is_finite() && closes[j + 1] > 0.0)
                                .map(|j| (closes[j + 1] / closes[j] - 1.0) * 100.0)
                                .collect();
                            out.push(Sample { date: dates[i], realized, relative: 0.0, quote, fund, trail });
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

    let buy_knobs: Vec<Knob> = vec![
        knob("long_trend_weight", |tuning| tuning.long_trend_weight = 0.0),
        knob("discount_weight", |tuning| tuning.discount_weight = 0.0), // (#4) zero the dip reward — Δ>0 confirms dip-depth ranks backwards
        knob("cheap_weight", |tuning| tuning.cheap_weight = 0.0),
        knob("dividend_weight*", |tuning| tuning.dividend_weight = 0.0),
        knob("onsale_sharpe_weight", |tuning| tuning.onsale_sharpe_weight = 0.0),
        knob("calmar_weight", |tuning| tuning.calmar_weight = 0.0),
        knob("quality_weight", |tuning| tuning.quality_weight = 0.0), // shared with the growth lane — one knob, so it must be ablatable in both
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
    gate_audit(&samples, growth_score, tuning); // (#9) are the growth lane's hard gates actually selecting winners?
    gate_sweep(&samples, tuning, &gate_loosen); // (#10) which specific gate is too tight?
    exit_probe(&samples, growth_score, tuning); // (Item 31) is a mid-hold gate FAILURE a measured sell signal?
    if fund || insider {
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
    // (FX) an index LEVEL, never joined to a filing — nothing to convert, so drop the currency
    .map(|(dates, closes, _)| (dates, closes))
    .unwrap_or_default();
    report_vs_benchmark(&samples, &bench, years, tuning);
    // (r40) relative strength vs the index — needs the benchmark, so it lives here, after the fetch.
    report_relative_strength(&samples, &bench);
    // (round 108) the WHEN dimension: does the market's state at entry predict the held book?
    let verdict = report_entry_state(&samples, &bench, years, tuning);
    // (round 27) journal the unconditional method verdict — but ONLY from a wide (`universe`) run:
    // the watchlist's ~50-survivor sample is not the method's proof, and must never overwrite it.
    // The screen's method footer reads this file back.
    if wide {
        if let Some((book, excess, win, worst, oos_early, oos_late, windows)) = verdict {
            write_verdict(&Verdict {
                date: chrono::Local::now().date_naive().to_string(),
                years,
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
    }
    // (round 112) the DIVERSIFICATION dimension: does de-correlating the held book beat plain rank order?
    report_corr_cap(&samples, &bench, years, tuning);
    if fund || insider {
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
    println!("  • Price-only (#6): no as-of dividends or P/E reconstructed; the * term above is inert here.");
    println!("  • Overlapping 6-mo windows share price paths -> samples aren't independent; rho is directional.");
    if monthly {
        println!("  • Long-horizon (MAX monthly): only names alive for the FULL {years}y window enter, so");
        println!("    survivorship bias is WORSE than the daily path, and vol/MA are monthly-bar approximations.");
    }
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
) {
    const HOLDS: [i64; 6] = [1, 2, 3, 5, 8, 10]; // forward windows (years) to compare
    eprintln!("backtest: hold-period sweep over {HOLDS:?}y windows ({} tickers)…", tickers.len());
    let per_ticker: Vec<Vec<(i64, Sample)>> = stream::iter(tickers.iter())
        .map(|tk| async move {
            let fetched = if monthly {
                fetch::fetch_history_long(client, urls, tk).await
            } else {
                fetch::fetch_history(client, urls, tk).await
            };
            let (dates, closes, _) = match fetched { // (FX) price-only sweep — no filing joined, no currency needed
                Some(x) => x,
                None => return Vec::new(),
            };
            let mut out = Vec::new();
            for &h in &HOLDS {
                let mut i = min_history;
                while i < dates.len() {
                    let target = dates[i] + chrono::Duration::days(h * 365);
                    match dates[i..].iter().position(|d| *d >= target) {
                        Some(off) => {
                            let realized = (closes[i + off] / closes[i] - 1.0) * 100.0;
                            // a zero/garbage close makes realized ±inf; one poisoned cutoff drags the
                            // whole demeaned bucket to -inf (short holds reach data the 12y path never walks)
                            if realized.is_finite() {
                                let quote = core::backtest_quote(tk, &dates, &closes, i, cadence);
                                out.push((h, Sample { date: dates[i], realized, relative: 0.0, quote, fund: None, trail: Vec::new() }));
                            }
                        }
                        None => break,
                    }
                    i += step;
                }
            }
            out
        })
        .buffer_unordered(fetch::fetch_concurrency())
        .collect()
        .await;
    let all: Vec<(i64, Sample)> = per_ticker.into_iter().flatten().collect();

    println!("\n── HOLD-PERIOD SWEEP (growth lane, net of cost) ──");
    println!("  pick the hold with the highest NET edge; if they're flat, the longest (cheapest) wins.");
    for &h in &HOLDS {
        // own bucket (de-mean is per-window: a 1y and a 5y forward over the same cutoff aren't comparable)
        let mut s: Vec<Sample> = all.iter().filter(|(w, _)| *w == h).map(|(_, smp)| smp.clone()).collect();
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
        println!("  {h}y hold  edge {edge:+.1}  turnover {:.0}%  net {net:+.1} pts", turn * 100.0);
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
            smp.quote.fund_factor = factor.and_then(|n| smp.fund.as_ref().and_then(|f| core::select_fund_factor(f, n)));
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
                smp.quote.fund_factor = smp.fund.as_ref().and_then(|f| core::select_fund_factor(f, w));
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
                smp.quote.fund_factor =
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
    // (bucket, score, pick CAGR, SPY CAGR, ticker) for every GATED non-crypto pick that has a benchmark window.
    let mut rows: Vec<(i32, f64, f64, f64, String)> = Vec::new();
    for s in samples {
        if picks::asset_class(&s.quote) == 0 {
            continue; // crypto: a coin isn't an S&P500-comparable hold
        }
        let Some(score) = growth_score(&s.quote, tuning) else { continue };
        let Some(bench_r) = benchmark_fwd(bd, bc, s.date, years) else { continue };
        rows.push((bucket(s.date), score, s.realized, bench_r, s.quote.ticker.clone())); // RAW cumulative % (annualize the BOOK, not per-name)
    }
    if rows.len() < 8 {
        println!("\n── vs S&P500 (ABSOLUTE) ──  only {} gated picks have a ^GSPC window — too few.", rows.len());
        return;
    }
    // BTreeMap -> buckets iterate in chronological order, so the OOS split is early-vs-late in time.
    let mut by_bucket: std::collections::BTreeMap<i32, Vec<(f64, f64, f64, String)>> = std::collections::BTreeMap::new();
    for (b, sc, pc, spc, tk) in &rows {
        by_bucket.entry(*b).or_default().push((*sc, *pc, *spc, tk.clone()));
    }
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
            "  top-{n:<2} book {:+.1}%/yr  vs S&P500 {:+.1}%/yr  ->  excess {:+.1} pts/yr   win {win:.0}% of {m}   worst {worst:+.1}   OOS {early:+.1}/{late:+.1}   rode {zeros} zeros/{held} holds",
            mean(&book), mean(&spy), mean(&excess)
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
    let verdict = book_stats(&base, n, years).map(|(b, _, e, w, wo, el, la)| {
        println!(
            "  {:<28} book {b:+.1}%/yr  excess {e:+.1}  win {w:.0}%  worst {wo:+.1}  OOS {el:+.1}/{la:+.1}   (windows {}) [unconditional]",
            "all entries", base.len()
        );
        (b, e, w, wo, el, la, base.len())
    });
    println!("  (a class with a handful of windows is a regime story, not a statistic. If a state over-delivers, the");
    println!("   guidance is DEPLOY NEW MONEY FASTER when it occurs — never hold cash waiting; the table can't see cash drag.)");
    verdict
}

/// Only the free SEC/income-statement factors are listed (roe + the round-107 survival levels are
/// SEC-computed; roic stays premium-gated). Runs only under `fund` (else `s.fund` is None everywhere).
fn report_book_by_factor(samples: &[Sample], bench: &(Vec<chrono::NaiveDate>, Vec<f64>), years: i64, tuning: &BuyHeuristic) {
    let (bd, bc) = bench;
    if bd.len() < 2 {
        return;
    }
    let n = 10; // the measured held-book optimum
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
        }
    }
    if !any {
        println!("  no fundamental coverage — needs `fund` + fund_source sec (free EDGAR) or an FMP key.");
    }
    println!("  (a factor beating growth_score's held-book excess with OOS both + is a better held-book selector -> ship it.");
    println!("   roe + the round-107 survival levels (fcf_margin/interest_cover/net_cash_rev) are SEC-computed; roic stays premium-gated.)");

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
    // MEASURED DEAD 2026-07-25, wide same-batch 12y (4913 tickers, 8393 fund-covered cutoffs): flat across
    // the whole sweep — excess +6.8 / +6.9 / +6.9 / +6.9 at reject 0/10/25/40%, book +14.8%/yr and worst
    // -7.8 unmoved throughout. The growth-at-price axis is closed AS A BRAKE, on measurement rather than
    // on the old "expected dead" assumption. Note the same factor is very much alive as a RANK TILT in the
    // same run (peg_yield is the shipped `growth_fund_factor`) — cutting the dear names and ranking by
    // cheapness are different questions, and only the second one pays here.
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
    let n = 10; // the measured held-book optimum
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
/// BOXED, not a plain `fn` pointer: `growth_fund_extra` is a config-driven LIST, so its ablation rows
/// have to be generated per configured term and each needs to capture its own index — which a fn
/// pointer cannot do. Owned `String` for the same reason (the names carry the factor).
type Knob = (String, Box<dyn Fn(&mut BuyHeuristic)>);

/// Terse constructor so the knob tables below stay one line per knob.
fn knob(name: impl Into<String>, f: impl Fn(&mut BuyHeuristic) + 'static) -> Knob {
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
    let mut buckets: HashMap<i32, Vec<usize>> = HashMap::new();
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
    let mut edges: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let mut pool: Vec<(&Sample, f64)> = Vec::new();
        for _ in 0..keys.len() {
            let k = keys[(next() % keys.len() as u64) as usize];
            for &i in &buckets[&k] {
                if let Some(v) = scorer(&samples[i].quote, tuning) {
                    pool.push((&samples[i], v));
                }
            }
        }
        if pool.len() >= 4 {
            let (t, b) = edge_halves(&pool);
            edges.push(t - b);
        }
    }
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
fn gate_audit(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> Option<f64> {
    let (accepted, rejected): (Vec<&Sample>, Vec<&Sample>) =
        samples.iter().partition(|s| scorer(&s.quote, tuning).is_some());
    println!("\n── GATE AUDIT (growth gates: do the names they EXCLUDE actually underperform?) ──");
    if accepted.len() < 4 || rejected.len() < 4 {
        println!("  {} accepted / {} rejected — too few on one side to compare.", accepted.len(), rejected.len());
        return None;
    }
    let mean = |g: &[&Sample]| g.iter().map(|s| s.relative).sum::<f64>() / g.len() as f64;
    let (a, r) = (mean(&accepted), mean(&rejected));
    let gap = a - r;
    println!("  accepted (passed gates): n={:<5} mean fwd peer-relative {a:+.1} pts", accepted.len());
    println!("  rejected (failed gates): n={:<5} mean fwd peer-relative {r:+.1} pts", rejected.len());
    let verdict = if gap > 0.0 {
        "gates SELECT winners (accepted beat the rejected pool)"
    } else {
        "gates ADD NOTHING — the rejected names did as well or better; consider loosening them"
    };
    println!("  gap {gap:+.1} pts  ->  {verdict}");
    Some(gap)
}

/// (#10 helper) Mean forward peer-relative return of the names REJECTED under `base` tuning but ACCEPTED
/// once loosened to `relaxed` (the set a looser gate NEWLY admits). `(count, mean)`, or None when
/// loosening admits nobody. Pure + scorer-generic so the per-gate sweep is unit-testable without building
/// quotes that clear growth_score's gate maze (same trick as `gate_audit`/`lane_metrics`).
fn newly_admitted_mean(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    base: &BuyHeuristic,
    relaxed: &BuyHeuristic,
) -> Option<(usize, f64)> {
    let newly: Vec<&Sample> = samples
        .iter()
        .filter(|s| scorer(&s.quote, base).is_none() && scorer(&s.quote, relaxed).is_some())
        .collect();
    if newly.is_empty() {
        return None;
    }
    let mean = newly.iter().map(|s| s.relative).sum::<f64>() / newly.len() as f64;
    Some((newly.len(), mean))
}

/// (#10) WHICH growth gate is too tight? #9 gives the aggregate verdict; this breaks it down per gate.
/// For each numeric gate, loosen its threshold one notch (relative to the loaded tuning, so a settings.yaml
/// override is respected) and report the mean forward peer-relative return of the names that loosening
/// NEWLY admits. A POSITIVE mean = that gate was discarding winners -> loosen it in settings.yaml and
/// re-validate (the lane OOS + #9's aggregate must still hold); ≤0 = the gate is correctly keeping junk
/// out, leave it. Pure measurement, no ranking change; reuses the ablation `Knob` pattern + `growth_score`.
fn gate_sweep(samples: &[Sample], tuning: &BuyHeuristic, gates: &[Knob]) {
    println!("\n── GATE SWEEP (loosen each gate one notch -> mean fwd return of the names it NEWLY admits) ──");
    println!("  positive = the gate was too tight (newly-admitted beat the field); ≤0 = it's keeping junk out.");
    for (name, loosen) in gates {
        let mut t = tuning.clone();
        loosen(&mut t);
        match newly_admitted_mean(samples, growth_score, tuning, &t) {
            Some((n, mean)) => {
                let tag = if mean > 0.0 { "  <- TOO TIGHT (loosen this gate)" } else { "" };
                println!("  {name:<26} n={n:<4} mean fwd peer-relative {mean:+.1} pts{tag}");
            }
            None => println!("  {name:<26} admits 0 new names (gate not binding on this sample)"),
        }
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
    for &x in ladder {
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
        println!(
            "  {x:<6.2} rho {rho}  edge {:+.1}  winsor {:+.1}  OOS {} | {}{tag}",
            top - bot,
            winsor_edge(&re),
            split_rho(&re[..mid]),
            split_rho(&re[mid..])
        );
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

/// (Item 31) Split each ticker's consecutive-cutoff pairs where the EARLIER cutoff passed the lane's
/// gates: did the later cutoff keep passing, or newly fail? Returns the two cohorts' forward
/// peer-relative returns (taken AT the later cutoff = the moment a holder would see the flip).
/// Pure so the split is unit-testable; consecutive cutoffs are ~6mo apart by construction.
fn exit_cohorts(
    samples: &[Sample],
    scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>,
    tuning: &BuyHeuristic,
) -> (Vec<f64>, Vec<f64>) {
    let mut by_ticker: HashMap<&str, Vec<&Sample>> = HashMap::new();
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
/// exit trigger; a flat gap = hold through it.
fn exit_probe(samples: &[Sample], scorer: fn(&Quote, &BuyHeuristic) -> Option<f64>, tuning: &BuyHeuristic) {
    let (kept, failed) = exit_cohorts(samples, scorer, tuning);
    println!("\n── EXIT PROBE (growth lane: passed gates ~6mo ago -> what next?) ──");
    if kept.len() < 4 || failed.len() < 4 {
        println!("  too few flips to read (kept {} / newly-failed {}).", kept.len(), failed.len());
        return;
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let (mk, mf) = (mean(&kept), mean(&failed));
    println!("  kept passing   n={:<6} mean fwd peer-relative {mk:+.1} pts", kept.len());
    println!("  newly FAILED   n={:<6} mean fwd peer-relative {mf:+.1} pts", failed.len());
    println!("  gap {:+.1} pts  (strongly negative = gate failure is a SELL signal; ~0 = hold through)", mf - mk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }
    fn sample(date: NaiveDate, realized: f64) -> Sample {
        Sample { date, realized, relative: 0.0, quote: Quote::stub("X", "1", "", "X"), fund: None, trail: Vec::new() }
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

    /// (round 27) the journaled method verdict: serde roundtrip is identity (the screen reads back
    /// exactly what backtest wrote), corrupt/empty JSON is None (a broken file silences the footer,
    /// never fabricates a verdict), and verdict_line's drift arm swaps the rerun-pointer for the ⚠
    /// stale-settings warning (citing stale numbers as current would mislead the buy decision).
    #[test]
    fn verdict_journal_semantics() {
        let v = Verdict {
            date: "2026-07-19".into(),
            years: 12,
            windows: 84,
            book: 14.3,
            excess: 6.9,
            win: 71.0,
            worst: -8.2,
            oos_early: 5.1,
            oos_late: 7.4,
            tuning_fp: "{\"a\":1}".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back = parse_verdict(&json).expect("roundtrip parses");
        assert_eq!((back.date.as_str(), back.years, back.windows), ("2026-07-19", 12, 84));
        assert!((back.book - 14.3).abs() < 1e-9 && (back.excess - 6.9).abs() < 1e-9);
        assert_eq!(back.tuning_fp, "{\"a\":1}");

        assert!(parse_verdict("not json").is_none());
        assert!(parse_verdict("").is_none());
        assert!(parse_verdict("{\"date\":\"x\"}").is_none()); // missing fields -> None, not a default

        let fresh = verdict_line(&v, false);
        assert!(fresh.contains("run 2026-07-19, wide universe, top-10 held 12y, 84 windows"));
        assert!(fresh.contains("book +14.3%/yr, +6.9pp/yr vs index, win 71%, worst -8.2, OOS +5.1/+7.4"));
        assert!(fresh.contains("(rerun: `folioman backtest universe`)") && !fresh.contains('⚠'));
        let drifted = verdict_line(&v, true);
        assert!(drifted.contains("⚠ settings changed since"));
        assert!(!drifted.contains("(rerun:"));
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
            s.quote = Quote::stub(tk, "1", "", tk);
            s.quote.drop_pct = drop;
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
        let stock = |r: f64| Sample { date: ymd(2020, 2, 1), realized: r, relative: 0.0, quote: Quote::stub("X", "1", "", "X"), fund: None, trail: Vec::new() };
        let crypto = |r: f64| Sample { date: ymd(2020, 2, 1), realized: r, relative: 0.0, quote: Quote::stub("BTC-USD", "1", "", "Bitcoin"), fund: None, trail: Vec::new() };
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
        Sample { date: ymd(2020, 1, 1), realized: relative, relative, quote: q, fund: None, trail: Vec::new() }
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
    }

    /// (Item 9) `turnover_frac` = 1 − mean Jaccard of consecutive buckets' top-half tickers. Two ~6mo
    /// buckets holding {A,B} then {A,C} overlap 1/3 -> turnover 2/3; a single bucket can't be measured -> 0.
    #[test]
    fn turnover_frac_consecutive_buckets() {
        let mk = |t: &str, m: u32| Sample {
            date: ymd(2020, m, 1),
            realized: 0.0,
            relative: 0.0,
            quote: Quote::stub(t, "1", "", t),
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
                Sample { date: ymd(2020, 1, 1), realized: r, relative: r, quote: Quote::stub("X", "1", "", "X"), fund: None, trail: Vec::new() }
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
        assert!(gate_audit(&good, dd_gate, &def).unwrap() > 0.0, "gate keeps winners -> positive gap");
        // flip: the dd>0 (accepted) names now carry the LOW returns -> gate admits losers -> negative gap
        let bad: Vec<Sample> = [(-5.0, 1.0), (-6.0, 1.0), (-7.0, 1.0), (-8.0, 1.0), (5.0, -1.0), (6.0, -1.0), (7.0, -1.0), (8.0, -1.0)]
            .iter().map(|&(r, d)| s_rel(r, d)).collect();
        assert!(gate_audit(&bad, dd_gate, &def).unwrap() < 0.0, "gate admits losers -> negative gap");
        // <4 on one side (4 accepted / 1 rejected) -> None (the too-few guard)
        assert!(gate_audit(&good[..5], dd_gate, &def).is_none());
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
        let (n, mean) = newly_admitted_mean(&s, cagr_gate, &base, &relaxed).unwrap();
        assert_eq!(n, 3, "only dd 1/2/3 are newly admitted (dd6 was already in)");
        assert!((mean - 20.0).abs() < 1e-9, "their relatives 10/20/30 -> mean 20, got {mean}");
        // loosening that admits nobody (relaxed == base) -> None
        assert!(newly_admitted_mean(&s, cagr_gate, &base, &base).is_none());
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
                let quote = core::backtest_quote(tk, &dates, closes, i, 252);
                samples.push(Sample { date: dates[i], realized, relative: 0.0, quote, fund: None, trail: Vec::new() });
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
}
