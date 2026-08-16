//! Buy-candidate algorithm + table printer (the whole heuristic in one place). TWO lanes:
//! - `growth_score` — proven compounders near their high, still climbing. THIS is what `check` and
//!   `screen` print (via `render`); the only lane with a validated forward edge.
//! - `buy_score` — "on-sale"/buy-the-dip. A BACKTEST FOIL ONLY (used by `backtest` to show dip-buying
//!   loses over a multi-decade hold); never printed. Knobs feeding it are tagged `[FOIL]` in config.
//! Acronyms (CAGR, ROE, P/E, NUPL, SMA, Sharpe, Calmar, …): see the Glossary in README.md.
//! **NOT advice** — a transparent ranking of the table, never an auto-buy.

use crate::commands::truncate;
use crate::config::{BuyHeuristic, Widths};
use crate::core::{self, Quote, HORIZONS};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// The longest >2Y leg as (cumulative %, span years): 20Y, else 8Y, else 5Y. None if the asset
/// has no >2Y history. The cumulative % feeds the corpse GATE; annualized (CAGR) it feeds the SCORE,
/// so an 8Y and a 20Y leg are compared on the same %/yr footing.
///
/// The middle rung was 10Y until 2026-07-25, when it moved to 8Y to match the displayed perf columns
/// — a name aged 10-20y was being ranked on a leg no table showed. VALIDATED same-batch 12y: baseline
/// edge +156.8 (rho +0.15, OOS +0.14|+0.12, winsorized +126.7) -> 8Y rung +169.4 (rho +0.15, OOS
/// +0.14|+0.12, winsorized +137.2). Every dial up or flat, none down, and the on-sale foil moved the
/// same way independently (+62.2 -> +63.0). CAVEAT, recorded honestly: +12.6 sits INSIDE the 90%
/// bootstrap bands ([+95.9 … +205.5] vs [+105.8 … +217.3]), the sample shrinks 1542 -> 1488 windows
/// (a shorter leg compounds less, so more names fall under growth_min_cagr), and the newest era is
/// worse (+100.2 -> +80.5). Accepted on direction and unanimity, not on significance.
///
/// `min_leg_years` (`growth_min_leg_years`) sets the SHORTEST rung this may return. At the shipped
/// 5.0 the 2Y rung below is skipped and this is byte-identical to the 20/8/5 ladder above. At 2.0 a
/// name with only a 2-year record gets a real CAGR instead of `None` — i.e. it stops dying on the
/// `history` gate, which is the single largest cohort the growth lane rejects (1949 of 4748
/// EU-buyable names, 2026-08-03) and the only one no other knob can reach.
///
/// 2Y and 1Y are the only rungs available to add: `core::HORIZONS` has no 3Y leg, and `Quote.perf`
/// is a Vec positionally aligned to it, so inserting one is a re-fetch and a re-verification of
/// every `perf_pct` site — not a config change.
///
/// (#49) 1Y is the bottom rung, and it carries a SECOND penalty nobody configured. A 1Y-rung name
/// has `long_cagr == return_1y` by construction, so `accel = return_1y − long_cagr` is identically
/// ZERO — the heaviest term in the growth score (weight 0.65 shipped) is structurally dead for
/// exactly these names. That is on top of whatever `trust_factor` docks them. Read the young-dock
/// curve knowing the dock is not the only thing holding these names down.
///
/// Admitting a rung is NOT the same as trusting it. `trust_factor` still requires a 10Y leg for
/// equities, so every 2Y-rung name is scored at 0.5; and `growth_min_cagr`'s whole-life leg still
/// applies, which for a 2-year-old listing is roughly the same number. Both are damps. Neither is a
/// substitute for the grid — a 19%/yr bar over two years is a far lower bar than over twenty.
fn long_leg(quote: &Quote, min_leg_years: f64) -> Option<(f64, f64)> {
    for (label, years) in [("20Y", 20.0), ("8Y", 8.0), ("5Y", 5.0), ("2Y", 2.0), ("1Y", 1.0)] {
        if years < min_leg_years {
            continue;
        }
        if let Some(p) = perf_pct(quote, label) {
            return Some((p, years));
        }
    }
    None
}

/// (#15) Like `long_leg` but PIN the horizon to `fixed_years` (e.g. 10 -> always the 10Y leg) so every
/// name's long CAGR is measured over the SAME window — otherwise an old name gets its full-cycle 20Y
/// CAGR (dragged through every crash) while a young name gets a flattering 5Y bull-only CAGR, and the
/// two are ranked head-to-head. `fixed_years` = 0 -> off (longest available leg, today's behaviour).
/// If the pinned leg is missing (short-history name) we fall back to the longest available leg; that
/// name is a `trust_factor` 0.5 anyway, so it can't out-rank a genuinely proven compounder on this.
fn long_leg_fixed(quote: &Quote, fixed_years: u32, min_leg_years: f64) -> Option<(f64, f64)> {
    if fixed_years == 0 {
        return long_leg(quote, min_leg_years);
    }
    match perf_pct(quote, &format!("{fixed_years}Y")) {
        Some(p) => Some((p, fixed_years as f64)),
        None => long_leg(quote, min_leg_years), // pinned leg absent -> longest leg, docked by trust_factor
    }
}

/// (#14) The long-leg CAGR the growth lane RANKS on. Three sources, one knob each, both default OFF ->
/// identical to the plain two-point CAGR, so the validated edge is untouched until a flip is measured:
///   - default          endpoint CAGR of `long_leg_fixed`'s rung (20/8/5Y, or the pinned window)
///   - `use_trend_cagr` (#14) endpoint-robust least-squares log-slope, precomputed at fetch/backtest build
///   - `use_life_cagr`  (#3j) whole-life CAGR since listing, ditto — no age cliff, no common window
///
/// `life_cagr_max_years` used to be a fourth source here and is NOT one any more: (#73) repointed it at
/// the whole-life REJECT BAR (`life_leg_cagr` below), which is the only reader of `capped_cagr` now. The
/// ranking arm it drove was measured at −66 edge and shipped off, so this deletes a branch that was
/// never taken. Ranking is the 20/8/5Y rung ladder at every value of that knob.
///
/// THE chokepoint, and the reason it is worth one: `score_parts` calls this ONCE and hands the result to
/// every reader, so a knob flipped here moves all seven together — the `growth_min_cagr` gate, `trend`,
/// `accel` (1Y minus this), `sharpe` and `calmar` (this over vol / maxdd), `trend_health`, and the `LEG`
/// column. Anything re-deriving the CAGR for itself silently keeps the old definition.
///
/// Extracted rather than inlined a third time: the `leg` COLUMN has to print the score's own number,
/// and a display that re-derives the score's arithmetic is exactly how the `cagr` column drifted into
/// showing `life_cagr` (whole-life) while the rank ran on this (the 20/8/5 rung, capped). The `peg`
/// COLUMN was routed here for the same reason — but the PEG *gate* was not, and that was the real
/// last site: it read `trend_cagr` directly, so column and gate sat on two arms of this one switch.
/// `long_cagr_pct` below closed that; nothing may divide by a CAGR without coming through here.
fn long_cagr_from(quote: &Quote, tuning: &BuyHeuristic, cum: f64, years: f64) -> f64 {
    if tuning.use_life_cagr {
        // (#3j) whole-life, listing -> as_of: the `cagr` COLUMN's number, promoted from display to rank.
        // Checked FIRST because it is the coarsest override — it discards the rung `long_leg_fixed` just
        // picked, so a config setting both this and `use_trend_cagr` gets the life number, not a silent mix.
        // Fall back to the leg, never to 0: a name too young for `life_cagr` (<6mo) still has a real
        // 5Y rung, and a 0 here would read as "flat compounder" and fail the floor for the wrong reason.
        quote.life_cagr.unwrap_or_else(|| core::cagr(cum, years))
    } else if tuning.use_trend_cagr {
        quote.trend_cagr.unwrap_or_else(|| core::cagr(cum, years))
    } else {
        core::cagr(cum, years)
    }
}

/// (#73) The CAGR the WHOLE-LIFE REJECT BAR judges — `growth_min_cagr`'s second leg, and the one number
/// `capped_cagr` still feeds. `None` only when the quote has no life history at all (<6mo), which must
/// read as "unknown", never as a failed bar.
///
/// ONE definition because there are TWO call sites — `score_parts` and `gate_failures` — and this file
/// says at both of them that they must stay the same expression. They drifted once; a shared fn is what
/// stops it happening again, and it costs three lines.
///
/// `capped_cagr` is `None` whenever `life_cagr_max_years` is 0 (`core::capped_life_cagr` returns None at
/// or below zero), so the `or` IS the knob: off = the uncapped lifetime this bar always used. No config
/// read here on purpose — the field's presence is the signal, and reading the knob a second time is how
/// a fill site and a read site end up on different windows.
fn life_leg_cagr(quote: &Quote) -> Option<f64> {
    quote.capped_cagr.or(quote.life_cagr)
}

/// (#37) The ONE CAGR every PEG in this tool divides by — `long_cagr_from` above, the same number the
/// score, the `growth_min_cagr` gate and the CAGR/LEG columns already run on, with the leg lookup
/// folded in so a caller cannot get half of it.
///
/// Exists because `peg_yield`'s three fill sites (fetch enrich, backtest loop, report mirror) each
/// hardcoded `quote.trend_cagr` — one arm of the three-way switch above — while the `peg` COLUMN took
/// whichever arm the config picked. Under the live `use_life_cagr: true` that split had the tool
/// cutting APH at PEG 2.02 in the same run it ranked ODFL printing 2.51. Two numbers, one name.
///
/// None when the quote has no long leg at all. That propagates: `core::peg_yield` returns None, the
/// cell prints `n/a` and the gate declines to price the name, which is the honest outcome — the
/// alternative is inventing a growth figure to divide by.
pub fn long_cagr_pct(quote: &Quote, tuning: &BuyHeuristic) -> Option<f64> {
    long_leg_fixed(quote, tuning.fixed_cagr_years, tuning.growth_min_leg_years).map(|(cum, years)| long_cagr_from(quote, tuning, cum, years))
}

/// (#37 funds) A fund's look-through equity-book P/E together with WHERE THE NUMBER CAME FROM.
///
/// `from: None` = Yahoo served this ratio for this fund, or it was copied across venue listings of the
/// SAME fund (one UCITS fund lists on Xetra + Euronext + Lisbon; that copy is bookkeeping, not an
/// inference).
///
/// `from: Some(src)` = BORROWED from an index twin. A swap-based (synthetic) ETF holds a total-return
/// swap, not equities: Yahoo reports `stockPosition 0.0`, `otherPosition 1.0`, no holdings, and every
/// `equityHoldings` ratio as a literal `0.0`, so `parse_fund_pe` correctly refuses it. There is no
/// equity book to look through to — the number cannot be measured for this fund at any effort, only
/// inferred from a physical fund tracking the same index.
///
/// The provenance is not decoration. A borrowed value ACTS: it feeds the `growth_max_peg_etf` trim
/// exactly like a fetched one, so it can remove a fund from the printed table. Anything that can cost
/// you a candidate has to say out loud that it was inferred, hence the `~` the `peg` cell appends and
/// the source `fund_pe_line` names.
/// (fund staleness) `as_of: Some(date)` = this ratio came off disk rather than the wire, and is that
/// old. Same argument as `from` one paragraph up, applied to TIME instead of provenance: a cached P/E
/// acts exactly like a fetched one, so a value that can cut a fund from the table has to say how old it
/// is. `None` = fetched this run. The fetch side refuses to serve anything past
/// `FUND_CACHE_MAX_AGE_DAYS`, so a marked cell is stale but never unboundedly so.
#[derive(Clone, Debug, PartialEq)]
pub struct FundPe {
    pub pe: f64,
    pub from: Option<String>,
    pub as_of: Option<chrono::NaiveDate>,
}

/// Look-through P/E per fund ticker. Aliased because it rides through six signatures.
pub type FundPeMap = HashMap<String, FundPe>;

impl From<f64> for FundPe {
    /// A measured ratio fetched THIS RUN — the common case, and what every live fetch produces.
    fn from(pe: f64) -> Self {
        Self { pe, from: None, as_of: None }
    }
}

/// A FUND's `peg_yield`, from its look-through equity-book P/E (`fund_pe`, keyed by ticker — already
/// un-inverted by `parse_fund_pe`) over the same `long_cagr_pct` the equity ceiling and the `peg` column
/// divide by. ONE fn for the trim in `lane_split` and the printed cell, for the same reason the equity
/// side has one: until 2026-07-27 the cell computed its own PEG while the gate read another, and the tool
/// cut a name at PEG 2.02 in the run it ranked one printing 2.51. None = no P/E for this fund (not in the
/// fetched set, or Yahoo served none AND no index twin resolved), which passes every trim — missing data
/// is not a verdict. Reads `.pe` without caring whether it was measured or borrowed: that is the point of
/// borrowing, and `FundPe::from` carries the provenance to the places that must show it.
pub fn fund_peg_yield(quote: &Quote, tuning: &BuyHeuristic, fund_pe: &FundPeMap) -> Option<f64> {
    core::peg_yield_from_pe(fund_pe.get(&quote.ticker)?.pe, long_cagr_pct(quote, tuning))
}

/// (#3h) The long-leg CAGR as a trend reward takes it: clamped at `long_trend_cap`, unless the cap is
/// OFF. `0` = off, the same convention `growth_maxdd_cap` / `growth_max_above_ma` / `fixed_cagr_years`
/// already use.
///
/// The zero-check is REQUIRED, not cosmetic. `long_cagr.min(0.0)` is `0.0` for every positive CAGR, so
/// a config shipping 0 without this guard would silently zero BOTH trend terms rather than uncap them —
/// a lane quietly losing its main signal and still printing plausible scores.
///
/// One helper for all three readers (growth score, on-sale foil, `LEG` column) so the printed cell
/// cannot drift from the arithmetic it claims to show. That drift is what the `LEG` column was added
/// to end; re-implementing the clamp per site is how it would come back.
fn capped_trend(long_cagr: f64, tuning: &BuyHeuristic) -> f64 {
    if tuning.long_trend_cap > 0.0 {
        long_cagr.min(tuning.long_trend_cap)
    } else {
        long_cagr
    }
}

/// (S-8Y) The same quote with every price stat whose window exceeds 8 years re-measured on its last 8
/// (`core::Stats8`, precomputed at fetch — the closes are gone by the time anything here runs).
/// `long_leg_fixed` only pins the CAGR leg; without this the 8Y-pinned column mixed an 8-year CAGR with
/// 10-year range/R²/drawdown/underwater and only half meant what its header said.
///
/// `None` (no history older than 8y, stub, backtest) borrows the quote untouched — the whole record IS
/// the 8-year window there, so the full-window stats already ARE the 8-year ones. That borrow is what
/// keeps the LIVE ranking bit-identical: nothing but the S-8Y column ever calls this.
fn as_8y_window(quote: &Quote) -> Cow<'_, Quote> {
    let Some(s) = &quote.stats_8y else { return Cow::Borrowed(quote) };
    let mut q = quote.clone();
    q.range_pct = s.range_pct;
    q.trend_r2 = s.trend_r2;
    q.max_drawdown_pct = s.max_drawdown_pct;
    q.underwater_yrs = s.underwater_yrs;
    Cow::Owned(q)
}

/// (S-8Y) `"†"` when the 8Y pin did NOT apply to this name — it has no 8Y leg, so `long_leg_fixed`
/// fell back to the longest one and its S-8Y is the full-history score wearing an 8Y label. Marked
/// rather than blanked, because an unmarked cell would silently claim an 8-year judgement it never
/// made — the same ambiguity the bare `n/a` had before r37b dropped the CAGR floor.
///
/// Distinct from `n/a`, which still appears when a PINNED name fails a gate the pin doesn't touch
/// (r37b neutralized only the CAGR floor). A pinned row prints at score 0.0 even when gated, so it
/// can reach this cell with nothing to show — e.g. VVSM.DE at +152% above its 200wk SMA.
fn short_8y_mark(quote: &Quote) -> &'static str {
    if perf_pct(quote, "8Y").is_none() {
        "†"
    } else {
        ""
    }
}

/// The `≈` stand-in for a rung the record ALMOST reaches, or `None` for "leave the cell `n/a`".
///
/// `YRS` rounds to nearest while a leg needs its full span minus 31d of slack, so every rung has a
/// ~5-month band where the age column claims the years and the leg is blank: 8Y needs 7.91y but
/// prints "8" from 7.50 (WTAI.MI, ~7.7y). This fills that band with the whole-life MEASURED return —
/// never a projection: `(1+CAGR)^N` is a re-render of two columns already on screen that READS as a
/// measurement, and on WTAI it swings 7.6x on which CAGR you feed it. Marked, for the same reason
/// `short_8y_mark` marks rather than blanks: an unmarked cell would claim a judgement it never made.
///
/// Three ways to get `None`, and only one of them is "too young":
/// - the real leg exists -> nothing to stand in for;
/// - `cov` below the configured bar -> too little record to speak for the rung;
/// - `cov >= 1.0` -> the record DOES span the rung and the leg is blank for some other reason (a zero
///   past price at the anchor). Filling there is the fabricated-leg bug `core.rs` (H-cov) removed.
///
/// Display-only by construction: reads `life_return_pct`, which is not in `perf`, so `perf_pct` — and
/// therefore every gate, `long_leg`, `spy_premium`, `twin_groups` — cannot see this number.
fn perf_fill(quote: &Quote, label: &str, tuning: &BuyHeuristic) -> Option<f64> {
    if perf_pct(quote, label).is_some() {
        return None;
    }
    let days = HORIZONS.iter().find(|(l, _)| *l == label)?.1 as f64;
    let cov = quote.age_years? * 365.25 / days;
    (cov >= tuning.perf_fill_coverage_pct / 100.0 && cov < 1.0).then_some(quote.life_return_pct)?
}

/// How intact the long-term trend is, 0..1 — used to scale the on-sale discount so a decaying name's
/// deep "discount" can't outrank a healthy compounder's modest pullback. `zero` (a negative %/yr
/// CAGR) is where health hits 0; health reaches 1 at a flat/rising long trend.
fn trend_health(long_cagr: f64, zero: f64) -> f64 {
    ((long_cagr - zero) / -zero).clamp(0.0, 1.0)
}

/// (D) Trailing ~1Y dividend yield (%) for the dividend reward; 0 if it doesn't pay / no price /
/// short history. Same per-horizon yield `screen` lists.
fn dividend_yield_1y(quote: &Quote) -> f64 {
    core::dividend_yields(&quote.div_eur, quote.price_eur).first().and_then(|o| *o).unwrap_or(0.0)
}

/// (D) The dividend reward, scored NET of Portuguese tax. Shared by BOTH lanes so the on-sale foil
/// and the growth lane can never drift apart on it.
///
/// A gross euro of dividend is not worth a gross euro of dividend: under Art. 40.º-A CIRS a payout
/// from an EU-resident company is englobado at only 50% (you must opt for englobamento), while a US
/// or other non-EU payer is taxed in full. `tax_keep_*` are the after-tax KEEP fractions, hand-set
/// by the operator from their own bracket — see the knob docs; nothing here encodes tax law.
///
/// CAP FIRST, THEN SCALE — not interchangeable. Capping the scaled yield (`min(yield × keep, cap)`)
/// would saturate EU and non-EU onto the identical capped value for every above-cap yielder, erasing
/// the tax distinction exactly where yields are highest and the distinction is worth most.
///
/// The EU rate reaches STOCKS only: a fund distribution is not a Parent-Subsidiary-Directive
/// company's *lucro*, so an EU-listed UCITS draws no exclusion and takes `tax_keep_other`.
///
/// NO LONGER BACKTEST-BLIND, in either half. This doc used to say `backtest_quote` could not
/// reconstruct as-of dividends and that therefore no walk-forward could grade the term at any weight;
/// `#53` disproved the premise (`Chart.divs` was always in the payload, the backtest fetch simply
/// dropped it) and the `dividend_weight` curve now prices the WEIGHT. The tax split is graded too, by
/// the on-sale lane's `tax_split ->off` ablation — see `is_eu_payer` for why it was never blind either.
/// That row reads Δ+0.0: at the shipped 0.76-vs-0.72 the split does not move the ranking, while the
/// WEIGHT it scales moves it by Δ+142.8. Size the weight with care; the split is a rounding detail.
/// (#61) The WEIGHT is a parameter because the two lanes disagree about it — growth wants it, on-sale
/// wants zero — exactly as `risk_bonus` takes its Sharpe weight for the same reason. Everything else
/// here (the cap, the tax keep-rate, the cap-then-scale order) is shared deliberately: the lanes
/// differ on how much a dividend is worth, never on what a dividend after Portuguese tax IS.
fn dividend_reward(quote: &Quote, weight: f64, tuning: &BuyHeuristic) -> f64 {
    weight * dividend_yield_1y(quote).min(tuning.dividend_cap) * tax_keep(quote, tuning).0
}

/// (D) Is this row an EU-resident *company* payer for Art. 40.º-A purposes — the ONE predicate behind
/// the englobamento split, named so the paragraph below has somewhere to live.
///
/// NOT BLIND IN THE BACKTEST, contrary to what three comments here and in `commands/backtest.rs` used
/// to claim. Neither input is `domicile`: `market` is filled by `core::market_of` from the ticker
/// suffix inside `Quote::stub`, and `instrument_type` is filled by `backtest::stamp_asset_class` from
/// Yahoo's `meta.instrumentType` plus the universe's `etf_set`. Both are the SAME inputs the live
/// screen scores on, which is the whole bar for gradeability — see the `tax_split ->off` ablation row.
///
/// The real approximation is elsewhere and is documented at `core::EU_MARKETS`: `market` is the
/// LISTING VENUE, not the payer's tax residence, so an Irish-domiciled S&P 500 name reads "USA" and is
/// under-credited. That direction is conservative and it is a data limit, not a blind term.
fn is_eu_payer(quote: &Quote) -> bool {
    !quote_is_etf(quote) && core::is_eu_market(&quote.market)
}

/// (D) Which after-tax keep-fraction applies to this row, plus the tier label the score breakdown
/// prints. One source for the EU/other decision so `explain_growth_score` can never label a row
/// differently from how `dividend_reward` actually scored it.
fn tax_keep(quote: &Quote, tuning: &BuyHeuristic) -> (f64, &'static str) {
    if is_eu_payer(quote) {
        (tuning.tax_keep_eu, "EU payer, 50% englobado")
    } else {
        (tuning.tax_keep_other, "non-EU or fund")
    }
}

/// (E) Valuation tilt from trailing P/E: cheap (PE < ref) lifts the score, rich dampens it, clamped
/// to [VALUE_TILT_MIN, VALUE_TILT_MAX]. Unknown PE or non-earning (crypto/ETF/PE<=0) -> 1.0 (neutral
/// — never punished for missing data).
fn value_factor(quote: &Quote, ref_pe: f64) -> f64 {
    match quote.pe_ratio {
        Some(pe) if pe > 0.0 && ref_pe > 0.0 => {
            (ref_pe / pe).clamp(crate::config::VALUE_TILT_MIN, crate::config::VALUE_TILT_MAX)
        }
        _ => 1.0,
    }
}

/// (B) Value-trap dock: when a name's 1Y AND 5Y returns are BOTH <= `sustained_decline_pct` it has
/// bled for years, not merely dipped — scale its score by `sustained_decline_penalty`. 1.0 (no dock)
/// if either leg is absent or above the line (a recovering peak-anchored coin — bad 5Y, positive 1Y
/// — is NOT docked).
fn sustained_decline_factor(quote: &Quote, tuning: &BuyHeuristic) -> f64 {
    match (perf_pct(quote, "1Y"), perf_pct(quote, "5Y")) {
        (Some(return_1y), Some(return_5y)) if return_1y <= tuning.sustained_decline_pct && return_5y <= tuning.sustained_decline_pct => {
            // harsher tier: a 5Y this deep (e.g. LTC -73%) is a 7y+ bleed coasting on a stale old
            // chart — dock it much harder than a "merely" -40% multi-year drift.
            if return_5y <= tuning.deep_decline_pct {
                tuning.deep_decline_penalty
            } else {
                tuning.sustained_decline_penalty
            }
        }
        _ => 1.0,
    }
}

/// #1 — Volatility-normalized dip: how deep the pullback is RELATIVE to this asset's normal daily
/// swing. A calm name (low vol) dropping 30% is a bigger event than a wild one dropping 30%, so we
/// scale the raw drawdown by `normal / asset_vol`: calm names get their dip amplified, wild ones
/// damped. Unknown/zero vol -> no scaling (use the raw drawdown). This is the "discount" the score
/// is built on, before the cap.
fn normalized_dip(drawdown: f64, vol: Option<f64>, normal: f64) -> f64 {
    match vol {
        Some(v) if v > 0.0 => drawdown * normal / v,
        _ => drawdown,
    }
}

/// #2 — Momentum factor: a multiplier on the discount based on what price is doing NOW, so a
/// confirmed turn-up outranks a name still knifing down at the same drawdown. Neutral (1.0) if it
/// hasn't pulled back this month; `bounce` (>1) on a green week off a monthly dip; half that
/// premium if only today is green; `knife` (<1) while it's still falling.
fn momentum_factor(quote: &Quote, bounce: f64, knife: f64) -> f64 {
    if perf_pct(quote, "1M").unwrap_or(0.0) >= 0.0 {
        return 1.0; // not pulled back -> nothing to time
    }
    if perf_pct(quote, "1W").unwrap_or(0.0) > 0.0 {
        bounce // up on the week off a monthly dip -> turn confirmed
    } else if perf_pct(quote, "1D").unwrap_or(0.0) > 0.0 {
        1.0 + (bounce - 1.0) * 0.5 // only today green -> half the bounce premium
    } else {
        knife // still falling -> dock it (don't catch the knife)
    }
}

/// Substrings (lowercased) that mark a leveraged/inverse product — daily-reset decay vehicles
/// that are never a long-term hold, so they can't be "quality on sale". `direxion` catches the
/// Direxion Daily 3× family when Yahoo hands a SHORT name ("Direxion Daily Technology" with the
/// "Bull 3X" dropped) that the `3x` marker would miss (e.g. TECL leaked into the stocks table).
/// note: cheap name match; tighten the list if a legit name ever trips it.
const LEVERAGED_MARKERS: &[&str] =
    &["2x", "3x", " short", "inverse", "leverag", "bear ", "ultra", "direxion"];

fn is_leveraged(name: &str) -> bool {
    let n = name.to_lowercase();
    LEVERAGED_MARKERS.iter().any(|m| n.contains(m))
}

/// Substrings (lowercased) that mark a pooled fund (ETF / UCITS index fund) vs a single-company
/// stock — plain index-fund longNames all carry one ("...S&P 500 UCITS ETF", "...ETF Trust"),
/// company names ("Apple Inc.") don't. Used only to SPLIT the equity table, never to gate.
/// note: name match, no asset-type field exists; tighten the list if a stock ever trips it.
const ETF_MARKERS: &[&str] = &["etf", "ucits", " index fund", " fund "];

fn is_etf(name: &str) -> bool {
    let n = name.to_lowercase();
    ETF_MARKERS.iter().any(|m| n.contains(m))
}

/// Substrings marking a PHYSICAL-commodity / precious-metal ETC (checked ETF-scoped only): a
/// store-of-value with no earnings or cashflow, so it cannot COMPOUND — a momentum spike (gold +99%
/// over 5Y) ranks it #1 in a "proven 20yr+ compounder" lane, a category error like the leveraged
/// decay vehicles above. ETF-scoped so a gold-MINER equity basket ("VanEck Gold Miners", earnings-
/// bearing) is untouched — it carries neither marker. `physical` catches the metal ETCs (all Yahoo
/// names carry it: "…IE Physical Gold"), `commodit` the broad commodity funds. Tighten if a legit
/// fund ever trips it (an equity sector fund named "…Energy"/"…Materials" has no marker, stays in).
/// Third net: a standalone "ETC" NAME TOKEN. ETC = Exchange Traded Commodity — the European legal
/// wrapper that BY DEFINITION holds a commodity, so the wrapper token itself is a marker. Catches
/// issuer-legal-name rows the substring markers miss ("XTrackers ETC PLC" = physical gold, no
/// "physical" in the name). Token match (split on non-alphanumeric), not substring, so equity fund
/// names containing the letters ("...Fetch...", "ETCetera") can never trip it; miner/producer
/// equity baskets are "UCITS ETF" — no ETC token.
const COMMODITY_MARKERS: &[&str] = &["physical", "commodit"];
// Physical metal trackers that carry neither marker nor the ETC wrapper token surface as plain
// metal names ("Xetra-Gold", "Gold Bullion Securities"). A bare metal token is a commodity marker
// too — unless the name also says the fund holds miner EQUITIES (Gold Miners / Gold Producers /
// Silver Mining), earnings-bearing baskets the lane keeps. Token match, so "Goldman" can't trip.
const METAL_TOKENS: &[&str] = &["gold", "silver", "platinum", "palladium", "bullion"];
const MINER_TOKENS: &[&str] = &["miner", "miners", "mining", "producer", "producers"];

fn is_commodity_etf(quote: &Quote) -> bool {
    quote_is_etf(quote) && {
        let n = quote.name.to_lowercase();
        let token = |t: &str| n.split(|c: char| !c.is_ascii_alphanumeric()).any(|x| x == t);
        COMMODITY_MARKERS.iter().any(|m| n.contains(m))
            || token("etc")
            || (METAL_TOKENS.iter().any(|m| token(m)) && !MINER_TOKENS.iter().any(|m| token(m)))
    }
}

/// (#44) GICS sectors whose earnings are a SPREAD on a traded input price, so the long CAGR is a
/// spot-price snapshot rather than a compounding record. Energy is ~90% clean (only the pipelines
/// WMB/KMI/OKE are toll roads, not spread-takers). Materials is ~50/50 — CF, Mosaic, Freeport, Newmont,
/// Nucor, Dow, LyondellBasell ride the cycle; Sherwin-Williams and PPG (branded paint), Linde and Air
/// Products (industrial gas on 15-year take-or-pay), Ecolab, Vulcan/Martin Marietta (aggregates, local
/// monopoly pricing) do not. That noise is ACCEPTED by direction: Energy-only would miss CF, which
/// ranked FIRST on the live screen, and the branded/contracted Materials names are slow compounders this
/// momentum lane never surfaces anyway.
const COMMODITY_SECTORS: &[&str] = &["Energy", "Materials"];
/// Funds carry no GICS, so the fund path is name tokens. MINER_TOKENS is reused here as an INCLUSION
/// while `is_commodity_etf` uses it as an EXEMPTION — both are right: a miner basket holds
/// earnings-bearing equities (so it keeps RANKING, not gated out like a physical ETC) but those earnings
/// still ride the metal price (so it FLAGS). Known over-inclusion: "energy" fires on clean-energy
/// equipment funds. "battery" is deliberately absent — L&G Battery Value-Chain stays unflagged.
const COMMODITY_FUND_TOKENS: &[&str] = &["oil", "gas", "energy", "uranium", "lithium", "copper", "steel"];

/// (#44) Does this row's growth depend on a commodity price? GICS sector for stocks, name tokens for
/// funds. Unknown sector (crypto, `check`, `screen TICKER…`, the backtest pool) -> false, the same
/// missing-data stance every gate here takes.
fn is_commodity(quote: &Quote) -> bool {
    quote.sector.as_deref().is_some_and(|s| COMMODITY_SECTORS.iter().any(|c| s.eq_ignore_ascii_case(c)))
        || (quote_is_etf(quote) && {
            let n = quote.name.to_lowercase();
            // token match, not `contains` — same reason as is_commodity_etf: "Goldman" must not trip "gold"
            let token = |t: &str| n.split(|c: char| !c.is_ascii_alphanumeric()).any(|x| x == t);
            MINER_TOKENS.iter().chain(COMMODITY_FUND_TOKENS).any(|t| token(t))
        })
}

/// (#45) Non-EUR-quoted ETF line — the FX/venue dock's predicate, shared by the score damp and the
/// `x` rank flag so the printed mark can never disagree with the arithmetic. Currency, not venue:
/// Yahoo's `quote_currency` ("GBp"/"USD"/"SEK"…) is what a EUR buyer's broker must convert, and a
/// EUR-quoted line on a non-eurozone venue costs no FX. None (every backtest stub) is innocent —
/// the dock is backtest-blind by construction, like (#44).
fn is_noneur_etf(quote: &Quote) -> bool {
    quote_is_etf(quote)
        && quote.quote_currency.as_deref().is_some_and(|c| !c.eq_ignore_ascii_case("EUR"))
}

/// Is this quote a pooled fund? Prefer Yahoo's own `instrumentType` ("ETF"), which is present even
/// when the name string isn't a giveaway (ETF shortNames like "ISHARES III PLC ISHRS CORE MSCI"
/// carry no marker). Falls back to the name-substring guess for rows with no meta (backtest stubs).
pub(crate) fn quote_is_etf(quote: &Quote) -> bool {
    quote.instrument_type.eq_ignore_ascii_case("ETF") || is_etf(&quote.name)
}

/// Underlying of a currency-quoted ticker: strips a trailing `-EUR`/`-USD` (crypto twins like
/// `BTC-EUR`/`BTC-USD`); anything else is its own underlying.
pub(crate) fn underlying(ticker: &str) -> &str {
    ticker.strip_suffix("-EUR").or_else(|| ticker.strip_suffix("-USD")).unwrap_or(ticker)
}

/// Currency-quoted (crypto/FX) ticker — carries a `-USD`/`-EUR` suffix, unlike an equity/ETF
/// symbol. Such assets are far more volatile, so a −40% year is normal noise, not a death
/// signal: they get the looser `min_1y_pct_crypto` floor instead of the equity `min_1y_pct`.
/// pub(crate): `report` uses this too — a bare `contains('-')` there misfired on share-class
/// tickers (`BRK.B` is normalized to `BRK-B` for the whole S&P universe).
pub(crate) fn is_currency_quoted(ticker: &str) -> bool {
    underlying(ticker) != ticker
}

/// Coarse asset class for peer-grouping: 0 = crypto (`-USD`/`-EUR`), 1 = ETF/fund, 2 = single stock.
/// Same split `print_lane` shows. The backtest de-means WITHIN class so a +9400% crypto's huge return
/// can't swamp the equity peer-mean and flatten every growth-knob's edge to noise.
pub fn asset_class(quote: &Quote) -> u8 {
    if is_currency_quoted(&quote.ticker) {
        0
    } else if quote_is_etf(quote) {
        1
    } else {
        2
    }
}

/// (#21) PEGGED underlyings excluded from the growth lane — each tracks an external peg, so its long
/// "CAGR" is the peg drifting, NOT compounding, and it's never a "buy and hold for decades" grower:
///   - dollar stablecoins (USDT…USDF): pegged to $1. On the EUR leg the price drifts with EUR/USD,
///     faking a drawdown that slips past the `drawdown < 3%` peg gate — so exclude by symbol instead.
///   - metal tokens (XAUT Tether Gold, PAXG PAX Gold): track a gram of gold, not a growing business.
///     They ranked in the crypto GROWTH table (a +11% "CAGR" that's just the gold price) — not growth.
const PEGGED: &[&str] = &[
    "USDT", "USDC", "DAI", "TUSD", "FDUSD", "PYUSD", "USDE", "BUSD", "USDP", "GUSD", "USDD", "FRAX",
    "USDF", // FolgoryUSD — dollar token (VOL ~0), surfaced #11 crypto
    "XAUT", "PAXG", // gold-backed — a metal peg, not a compounding asset
];

fn is_stablecoin(ticker: &str) -> bool {
    PEGGED.contains(&underlying(ticker))
}

/// Collapse `<X>-EUR`/`<X>-USD` twins to ONE row (same asset, just a different quote currency),
/// keeping the `prefer_eur`-matching leg when both are present (else whichever exists). Other
/// tickers pass through untouched. Order is NOT preserved (the caller re-sorts).
fn dedup_currency_twins<'a>(
    picks: Vec<(&'a Quote, f64)>,
    prefer_eur: bool,
) -> Vec<(&'a Quote, f64)> {
    let pref = if prefer_eur { "-EUR" } else { "-USD" };
    let mut best: HashMap<&str, (&'a Quote, f64)> = HashMap::new();
    for (quote, s) in picks {
        let base = underlying(&quote.ticker);
        let take = match best.get(base) {
            None => true,
            // replace only if the newcomer is the preferred currency and the kept one isn't
            Some((kept, _)) => quote.ticker.ends_with(pref) && !kept.ticker.ends_with(pref),
        };
        if take {
            best.insert(base, (quote, s));
        }
    }
    best.into_values().collect()
}

/// % change at a given horizon label (e.g. "1Y") from a Quote's perf, by label not index
/// (robust to HORIZONS reordering). None if that horizon has no data.
pub fn perf_pct(quote: &Quote, label: &str) -> Option<f64> {
    let i = HORIZONS.iter().position(|(l, _)| *l == label)?;
    quote.perf.get(i).and_then(|o| o.as_ref()).map(|(_, p)| *p)
}

/// The growth lane's per-rung CUMULATIVE-return floors: `(perf label, near-miss tag, floor %)`.
/// ONE definition, read by BOTH `score_parts` and `gate_failures` — those two must agree or a name is
/// silently dropped from the ranking while the tail claims it passes (see picks.rs:745). Sharing the
/// table makes them agree by construction instead of by two copies being kept in sync by hand.
///
/// The 5Y rung takes a CRYPTO TWIN, the same split five sibling gates already carry
/// (`min_1y_pct_crypto`, `growth_min_cagr_crypto`, `growth_min_range_pct_crypto`,
/// `max_1m_drop_pct_crypto`, `growth_maxdd_cap_crypto`). It exists because the equity floor's measured
/// optimum lands on top of Bitcoin: the 2026-08-03 ladder puts the equity peak at +75 (20y lane edge
/// +410.8 -> +459.3, h2h 67% -> 76%), and BTC's 5Y was +51.5% that day, so one shared knob priced the
/// whole crypto table against an equity-tuned bar and emptied it. The 8Y/20Y rungs need no twin — no
/// coin in the universe carries a leg that long, so those cells are `n/a` and skipped by construction.
fn long_leg_floors(tuning: &BuyHeuristic, crypto: bool) -> [(&'static str, &'static str, f64); 3] {
    [
        ("5Y", "5Y+", if crypto { tuning.growth_min_5y_pct_crypto } else { tuning.growth_min_5y_pct }),
        ("8Y", "8Y+", tuning.growth_min_8y_pct),
        ("20Y", "20Y+", tuning.growth_min_20y_pct),
    ]
}

/// Confidence multiplier — halve a name without a long PROVEN record. Equities should carry a 10Y
/// leg; crypto can't (Yahoo's EUR crypto pairs are too young to ever show 10Y), so for them a 5Y leg
/// is "proven enough". Without this, BTC is halved for a history gap that's purely an artifact of the
/// EUR quote, and vanishes from the growth lane despite a 15-year track record.
///
/// `fixed_years` (#15 `fixed_cagr_years`) moves the required leg WITH the pinned CAGR window: under an
/// 8-year view an 8-year record IS the full record, so demanding 10Y there would halve every name the
/// view exists to judge. 0 (the live default) keeps 10Y — this is inert on the ranked path.
///
/// The 10Y here deliberately does NOT track `long_leg`'s ladder, which moved to 20/8/5 on 2026-07-25.
/// Looks inconsistent; is measured. Same-batch 12y, ladder already at 8Y: trust leg 10Y -> edge +169.4
/// (rho +0.15, OOS +0.14|+0.12, winsorized +137.2); dropping it to 8Y -> +144.4 (rho +0.14, OOS
/// +0.13|+0.12, winsorized +115.8) — WORSE than the 20/10/5 baseline it started from. The two numbers
/// answer different questions: the ladder picks the window a CAGR is measured over, this picks the bar
/// for calling a record PROVEN, and a 10-year record is demonstrably worth more than an 8-year one.
/// Do NOT "fix" the mismatch without a same-batch run that clears +169.4.
///
/// (#47) `growth_trust_ladder` replaces that ONE cliff with a graded 20/8/5 ladder. The cliff's defect
/// is that it cannot tell a 46-year record from a 10-year one — both score 1.0 — while a 9.9-year one
/// takes the full 0.5. Record length is a continuum and the cliff spends all of its resolution on a
/// single point of it.
///
/// Keyed on WHICH PERF LEG EXISTS, never on `quote.age_years`: the YRS column is None in the backtest
/// pool (live-only, same standing as the commodity/FX damps), so an age-keyed ladder could never be
/// graded. The legs are present on both paths, carry the same information, and make this measurable.
///
/// ONE SHARED LADDER, crypto included — a deliberate call, not an oversight. It is NOT a uniform
/// crypto dock: BTC (listed 2010, so an 8Y leg but no 20Y) lands at 0.85 while a 5-year alt lands at
/// 0.70, which REORDERS the crypto lane toward the older coins. That is the intended effect, and it is
/// unmeasurable by construction (crypto is filtered out of every backtest edge metric — see
/// `growth_min_5y_pct_crypto`), so it needs an eyeball check on the live table, not a run.
///
/// Tiers are hardcoded. A knob per rung is four more numbers needing four more receipts, and the
/// ladder has to earn its place before it earns a shape.
///
/// (#49) ONE exception to that, at the BOTTOM: `young`. The ladder as first built stopped at
/// `none => 0.50`, which meant it did not dock a 2-year record any harder than a 7-year one — the
/// old cliff's flat half-score for both. That is precisely the distinction the young rungs need, so
/// the bottom two tiers are driven by a single swept number: the 2Y rung takes `young`, the 1Y rung
/// `young / 2.0` (half the record, half the trust). One knob, two rungs — the "no knob per rung"
/// bargain above survives, and only the number that actually decides the outcome is swept.
///
/// `young` is dead weight unless BOTH the ladder is on AND `growth_min_leg_years` admits a rung
/// below 5 — at the shipped floor no 2Y/1Y-rung name exists to dock, so its curve prints a flat
/// line. That is the expected baseline reading, not a bug.
fn trust_factor(quote: &Quote, crypto: bool, fixed_years: u32, ladder: bool, young: f64) -> f64 {
    if ladder {
        // longest leg present wins; `fixed_cagr_years` deliberately does NOT pin this one — the ladder
        // asks how long the record IS, which is a fact about the name, not about the view chosen for it.
        return match () {
            _ if perf_pct(quote, "20Y").is_some() => 1.0,
            _ if perf_pct(quote, "8Y").is_some() => 0.85,
            _ if perf_pct(quote, "5Y").is_some() => 0.70,
            _ if perf_pct(quote, "2Y").is_some() => young,
            _ => young / 2.0, // 1Y-only (or no leg at all): half the record, half the trust
        };
    }
    let needed = if crypto {
        "5Y".to_string()
    } else if fixed_years > 0 {
        format!("{fixed_years}Y")
    } else {
        "10Y".to_string()
    };
    if perf_pct(quote, &needed).is_none() {
        0.5
    } else {
        1.0
    }
}

/// (#4) Combine the pure penalty multipliers (each ∈[0,1]) as a GEOMETRIC MEAN, not a raw product, so
/// several mild damps can't compound multiplicatively toward ~0 and silently delete an otherwise
/// strong pick — the bug that dropped BTC, where trust × overext × consistency stacked to near-zero.
/// geomean(all 1.0) = 1.0; a lone 0.5 damp costs 0.5^(1/n), not 0.5; the combined penalty is bounded
/// by the SOFTEST term instead of the product. Still monotone in every term (ranking order preserved).
/// Empty -> 1.0 (no damp). Caps the stacked penalty, as #4 asked.
fn combine_damps(damps: &[f64]) -> f64 {
    if damps.is_empty() {
        return 1.0;
    }
    damps.iter().product::<f64>().powf(1.0 / damps.len() as f64)
}

/// (F) Profitability/QUALITY reward: trailing return on capital, the canonical quality factor
/// (high-ROE firms out-compound long-run). `quote.roe` holds `core::quality_return` — ROE where equity
/// is a credible denominator, ROA where it is negative or bought down past 1/20th of assets — so a
/// buyback-shrunk filer (HCA, CL) scores on a real denominator instead of on a collapsed one. None (crypto/ETF/no fund coverage) → 0 = neutral;
/// negative clamps to 0 (no bonus, the gates handle bleeders). Shared by both lanes.
///
/// No longer backtest-blind: the backtest loop fills `quote.roe` from the as-of `FundFactors.quality`,
/// so `backtest <set> fund` can finally price this term instead of measuring a constant 0.
fn quality_reward(quote: &Quote, tuning: &BuyHeuristic) -> f64 {
    tuning.quality_weight * quote.roe.unwrap_or(0.0).clamp(0.0, tuning.quality_cap)
}

/// (B/C) Risk-adjusted-return bonus from already-fetched closes (zero extra fetch): additive reward
/// for return PER unit of risk — Sharpe-ish (CAGR/volatility, path noise) + Calmar (CAGR/max-drawdown,
/// tail pain). Both reward the same thing from two angles: a name that compounds hard while staying
/// calm and shallow-drawdown. Missing/zero risk inputs → 0 (never punished for absent data). The
/// Sharpe/Calmar weights are passed in PER LANE — the growth and on-sale lanes want different Sharpe
/// emphasis (growth 0.15, on-sale 0), so the caller supplies its own.
fn risk_bonus(quote: &Quote, long_cagr: f64, sharpe_weight: f64, calmar_weight: f64, tuning: &BuyHeuristic) -> f64 {
    // (#37) ETFs get their own (lower) Sharpe cap: cross-listed lines of the SAME fund print
    // different daily stdev (thin-line prints + FX conversion; NASD.L 1.8%/day vs CSNDX.SW 1.1%
    // for the identical Nasdaq-100 holdings), so a high ETF CAGR/vol ratio measures listing-line
    // noise, not fund risk — uncapped it outvotes a real 0.11%/yr TER difference between wrappers.
    let sharpe_cap = if tuning.sharpe_cap_etf > 0.0 && quote_is_etf(quote) {
        tuning.sharpe_cap_etf
    } else {
        tuning.sharpe_cap
    };
    let sharpe = match quote.volatility_pct {
        Some(v) if v > 0.0 => (long_cagr / v).clamp(0.0, sharpe_cap),
        _ => 0.0,
    };
    let calmar = if quote.max_drawdown_pct > 0.0 {
        (long_cagr / quote.max_drawdown_pct).clamp(0.0, tuning.calmar_cap)
    } else {
        0.0
    };
    sharpe_weight * sharpe + calmar_weight * calmar
}

/// Score a quote as a "quality on sale" buy candidate for a multi-DECADE hold, or `None` if it
/// fails a gate. The formula:
///
/// ```text
///   base  = discount_weight×discount × trend_health × momentum + long_reward×discount_frac + cheap_reward + dividend_reward + risk_reward + quality_reward
///   score = base × value × geomean(decline, trust)   // (#4) geomean caps stacked penalties
/// ```
///
/// - **discount** — how deep in its OWN ~10y range it trades (100 − percentile rank; self-normalizes
///   amplitude across BTC vs a penny alt), then volatility-normalized and capped (`normal_volatility_pct`,
///   `discount_cap`), then scaled by **discount_weight** (#4, default 0.35): the walk-forward backtest found
///   deepest-dip ranking is BACKWARDS on peer-relative selection, so the direct dip reward is demoted toward
///   the trend/quality terms (set 1.0 to restore the old weight). The OFF-HI column (`drawdown_pct`) is display only.
/// - **trend_health** ∈ [0,1] — fades the discount as the long trend's CAGR weakens (`health_zero_cagr`).
/// - **momentum** — weekly bounce/knife multiplier (`momentum_bounce`/`knife`); 1.0 = off (default:
///   weekly timing is noise at a decades horizon).
/// - **long_reward** — (A) reward for the long leg's CAGR (annualized, comparable across spans;
///   `long_trend_weight`, and `long_trend_cap` when that cap is on — 0 = off, uncapped, as shipped),
///   scaled by **discount_frac** = discount/`discount_cap`
///   so a proven compounder only earns it when actually pulled back — at its high the reward → 0.
/// - **cheap_reward** — (C) reward for sitting below the ~200wk SMA (`cheap_weight`, `cheap_cap`).
/// - **dividend_reward** — (D) reward for trailing yield (`onsale_dividend_weight` since (#61) — this
///   lane's own weight, SHIPPED AT 0, because paying for yield ranked the lane backwards; `dividend_cap`
///   and the tax keep-rate stay shared with growth). NO LONGER
///   BACKTEST-BLIND (#53): `Chart.divs` was always in the payload and is now plumbed into
///   `backtest_quote`, so the ablation row and the `dividend_weight` curve grade it for real. The TAX
///   split is graded too (#58), and the `domicile` this line used to blame was never read by anything:
///   `tax_keep` branches on `is_eu_payer`, i.e. `market` (from the ticker suffix) and `instrument_type`
///   (from `stamp_asset_class`) — both live in the backtest, both the same inputs the live screen uses.
/// - **value** — (E) P/E tilt: cheap lifts, rich dampens, unknown neutral (`ref_pe`). BACKTEST-BLIND:
///   no as-of P/E in the backtest, so this term is unvalidated there too — keep the tilt gentle.
/// - **quality_reward** — (F) return-on-capital profitability tilt (`quality_weight`/`quality_cap`);
///   the canonical quality factor (high-ROE firms out-compound). ROE, or ROA where equity is negative
///   or collapsed.
/// - **decline** — (B) value-trap dock when 1Y & 5Y both deeply negative.
/// - **risk_reward** — (B/C) Sharpe-ish (CAGR/vol) + Calmar (CAGR/max-drawdown) bonus; return per unit of risk. On-sale lane uses its own `onsale_sharpe_weight`.
/// - **trust** — halves anything without a long record (10Y for equities, 5Y for young-EUR-pair crypto).
///
/// Every knob lives in `BuyHeuristic` (settings.yaml `buy_heuristic:`). Higher = more interesting.
/// GATES below exclude a candidate before scoring. **NOT advice** — a ranking, never a forecast.
pub fn buy_score(quote: &Quote, tuning: &BuyHeuristic) -> Option<f64> {
    let crypto = is_currency_quoted(&quote.ticker); // crypto/FX (-EUR/-USD): looser, peak-anchor-aware rules

    // ---- GATES: drop anything that isn't a quality name on a real pullback ----
    if is_leveraged(&quote.name) {
        return None; // leveraged/inverse product -> decays, never a long-term hold
    }
    if quote.avg_turnover_eur.is_some_and(|v| v < tuning.min_avg_turnover_eur) {
        return None; // too thin/illiquid (unknown turnover passes — don't punish missing data)
    }
    if crypto && is_stablecoin(&quote.ticker) {
        return None; // dollar-pegged stablecoin -> no growth; its EUR-leg FX drift fakes a drawdown
    }
    if crypto && quote.drawdown_pct < 3.0 {
        return None; // crypto at its high -> nothing on sale
    }
    // longest >2Y leg (crypto is younger, so fall back to its 1Y leg): cumulative for the gate,
    // annualized (CAGR) for the score.
    // 5.0, NOT `tuning.growth_min_leg_years`: that knob is the GROWTH lane's ladder floor, and this is
    // the on-sale foil. Wiring it here too would move both lanes at once, and the backtest reports them
    // head-to-head — the comparison has to hold one side still to mean anything.
    let (long_cum, long_years) = long_leg(quote, 5.0)
        .or_else(|| if crypto { perf_pct(quote, "1Y").map(|p| (p, 1.0)) } else { None })?;
    if crypto && long_cum <= tuning.min_long_pct_crypto {
        return None; // crypto corpse: a >2Y leg this deep (e.g. -95%) is a dead coin, not a dip
    }
    let return_1y = perf_pct(quote, "1Y")?;
    let floor = if crypto { tuning.min_1y_pct_crypto } else { tuning.min_1y_pct };
    if return_1y <= floor {
        return None; // deep 1-year downtrend -> not a pullback
    }
    let knife = if crypto { tuning.max_1m_drop_pct_crypto } else { tuning.max_1m_drop_pct };
    if perf_pct(quote, "1M").unwrap_or(0.0) <= knife {
        return None; // crashing this month -> falling knife
    }
    if !crypto {
        // equities must be structurally up: EVERY multi-year leg must hold. (Crypto -EUR 5Y is
        // peak-anchored and routinely negative even when healthy, so this gate is meaningless there.)
        for label in ["5Y", "10Y", "20Y"] {
            if perf_pct(quote, label).is_some_and(|p| p <= tuning.min_long_pct) {
                return None;
            }
        }
    }

    // ---- SCORE ----
    let long_cagr = core::cagr(long_cum, long_years); // (A) annualized -> comparable across 5/10/20Y
    // (A) on-sale = how deep in its OWN ~10y range it trades (100−percentile rank), NOT raw distance
    // below the high. Self-normalizes amplitude so volatile names that all sit far below ATH no longer
    // peg the cap together — a coin at the 20th pct outranks one at the 70th. drawdown_pct stays the
    // OFF-HI display only. Still vol-scaled + capped, so a calm name's cheapness counts for more.
    let cheapness = 100.0 - quote.range_pct;
    let discount =
        normalized_dip(cheapness, quote.volatility_pct, tuning.normal_volatility_pct).min(tuning.discount_cap);
    let health = trend_health(long_cagr, tuning.health_zero_cagr);
    let momentum = momentum_factor(quote, tuning.momentum_bounce, tuning.momentum_knife);
    // (2a) scale the long-trend reward by how on-sale the name is (discount as a fraction of the cap):
    // a proven compounder is only a BUY when it's actually pulled back. At its all-time high the
    // discount is ~0, so the reward fades to ~0 and an at-the-high rocket stops ranking as "on sale".
    let discount_frac = (discount / tuning.discount_cap).clamp(0.0, 1.0); // 0 = at its high, 1 = deeply discounted
    let long_reward = tuning.long_trend_weight * capped_trend(long_cagr, tuning) * discount_frac; // (A) cap 0 = uncapped
    let cheap_reward = tuning.cheap_weight * quote.below_ma_pct.min(tuning.cheap_cap); // (C)
    let dividend_reward = dividend_reward(quote, tuning.onsale_dividend_weight, tuning); // (D/#61) net of PT tax — see the fn. This lane's OWN weight: shipped 0.0, because paying for yield here ranked the lane BACKWARDS

    let risk_reward = risk_bonus(quote, long_cagr, tuning.onsale_sharpe_weight, tuning.calmar_weight, tuning); // (B/C) on-sale lane's own Sharpe weight
    let base = tuning.discount_weight * discount * health * momentum // (#4) demoted: dip-depth ranks backwards on peer-relative backtest
        + long_reward
        + cheap_reward
        + dividend_reward
        + risk_reward
        + quality_reward(quote, tuning); // (F) return-on-capital tilt — MEASURED since the backtest fills quote.roe: zeroing it costs this lane -48.7 edge and flips rho negative (see ci-settings (F))
    let value = value_factor(quote, tuning.ref_pe); // (E) cheap lifts, rich dampens, unknown neutral
    let decline = sustained_decline_factor(quote, tuning); // (B) multi-year-bleed dock
    let trust = trust_factor(
        quote,
        crypto,
        tuning.fixed_cagr_years,
        tuning.growth_trust_ladder,
        tuning.growth_trust_young,
    ); // (A) equities need a 10Y leg (the pinned window when fixed_cagr_years is set); crypto: only 5Y. (#47) or the graded 20/8/5 ladder, (#49) with the 2Y/1Y rungs under it
    // (#4) geomean the pure penalties so several mild damps can't compound to ~0; value (a tilt that
    // can exceed 1.0) stays a direct multiplier.
    Some(base * value * combine_damps(&[decline, trust]))
}

/// Per-term breakdown of a growth SCORE, so `screen` can print the exact arithmetic that ranked the #1
/// row (transparency / "validate it yourself"). SINGLE SOURCE: `growth_score` is literally
/// `score_parts(..).map(|p| p.score)`, so the explained terms can never drift from the ranked number.
/// All fields are the post-cap/clamp values actually summed/multiplied — nothing recomputed downstream.
struct ScoreParts {
    long_cagr: f64,    // raw long-leg CAGR (%/yr) before the trend cap
    return_1y: f64,    // raw 1Y return (%) — accel input
    trend: f64,        // capped_trend(long_cagr) — clamped at long_trend_cap, or raw when it is 0 (off)
    accel: f64,        // clamp(return_1y − long_cagr, 0, growth_accel_cap)
    trend_term: f64,   // growth_trend_weight × trend
    accel_term: f64,   // growth_accel_weight × accel
    risk_reward: f64,  // (B/C) Sharpe+Calmar bonus
    quality: f64,      // (F) quality_weight × ROE
    dividend: f64,     // (D) dividend_weight × min(yield, cap) × PT tax keep-rate (EU payer keeps more)
    fund: f64,         // (G) growth_fund_weight × clamp(fund_factor, 0, cap)
    mom121: f64,       // (M) growth_mom121_weight × clamp(12-1 mom, 0, cap)
    smooth: f64,       // (E) growth_smoothness_weight × trend_r2
    underwater: f64,   // −growth_underwater_weight × underwater_yrs (drawdown-duration penalty; 0 when off/None)
    base: f64,         // sum of the nine terms above
    proximity: f64,    // (#48) 1 + growth_proximity_weight × (range_pct/100 − 1); = range_pct/100 at the shipped w=1
    value_raw: f64,    // (E) raw P/E value_factor (ref_pe/PE clamped)
    value: f64,        // 1 + growth_value_weight × (value_raw − 1)
    trust: f64,        // (A) history-completeness damp
    overext: f64,      // min(above_ma_pct, overext_cap)
    overext_cap: f64,  // the class's overextension cap
    overext_damp: f64, // 1 − (overext/cap)×(1−floor)
    damp: f64,         // geomean(trust, overext_damp)
    liq_bonus: f64,    // (L) turnover_weight × ln(max(turnover/1e9, 1))
    ter_damp: f64,     // (T) ETF cost drag (1−TER)^20; 1.0 for stocks/crypto/None or when growth_ter_drag off
    commodity_damp: f64, // (#44) growth_commodity_damp on a GICS Energy/Materials row or a commodity-named fund; 1.0 otherwise / knob off / sector unknown (backtest)
    fx_damp: f64,      // (#45) growth_fx_damp on an ETF whose live quote currency is not EUR; 1.0 otherwise / knob off / currency unknown (backtest)
    score: f64,        // base × proximity × value × damp × ter_damp × commodity_damp × fx_damp + liq_bonus  (or base × geomean(trust,overext,prox,value) × ter_damp × commodity_damp × fx_damp + liq_bonus when #8 growth_geomean_fold)
}

/// Score a quote as a MOMENTUM/GROWTH candidate — the MIRROR of `buy_score`. The on-sale lane fades
/// a name's score to ~0 as it nears its high (a proven compounder at a new high has no "discount"),
/// so it never surfaces quality that's expensive *because* it keeps winning. This lane is exactly
/// that set: a name AT/NEAR its own range high, with a strong proven long-term CAGR, still climbing.
///
/// ```text
///   base  = growth_trend_weight × capped_trend(long_cagr)   [cap 0 = uncapped, shipped]
///         + growth_accel_weight × clamp(1Y − long_cagr, 0, growth_accel_cap)   // recent outpaces long => accelerating
///         + quality_reward                                                     // (F) ROE profitability tilt (SIGHTED, ablation-graded; see below)
///   score = base × proximity × value(E) × geomean(trust, overext)   // (#4) geomean of the penalties
/// ```
///
/// Gated HARD so it can't degrade into top-chasing: must sit in the top `growth_min_range_pct` of its
/// own ~10y range, compound at least `growth_min_cagr` %/yr, have a POSITIVE 1Y (actually climbing),
/// and not be crashing this month. The P/E value tilt (E) still damps a nosebleed valuation, so a
/// blow-off top is penalised, not rewarded. `None` if it fails a gate. **NOT advice** — a ranking.
/// Returns the full per-term [`ScoreParts`]; [`growth_score`] is the scalar wrapper most callers use.
fn score_parts(quote: &Quote, tuning: &BuyHeuristic) -> Option<ScoreParts> {
    let crypto = is_currency_quoted(&quote.ticker);

    // ---- GATES (reuse the cheap exclusions; the rest are the on-sale lane's mirror) ----
    if is_leveraged(&quote.name) {
        return None; // leveraged/inverse decays -> never a long-term hold
    }
    if is_commodity_etf(quote) {
        return None; // physical commodity/metal ETC -> no cashflow, doesn't compound (not this lane's thesis)
    }
    if crypto && is_stablecoin(&quote.ticker) {
        return None; // pegged $1 -> no growth
    }
    // (#20) UNKNOWN turnover -> excluded from the growth lane, full stop (independent of any floor). The
    // lane's thesis is a deep-liquid, multi-decade-holdable compounder (it even pays a liquidity BONUS),
    // and a name whose turnover Yahoo never served can't be assessed as one. The backtest stays
    // unaffected: backtest_quote sets a SENTINEL turnover (never None there), so this is a LIVE-only gate
    // and the validated edge is untouched. A KNOWN-but-thin turnover is dropped only when a floor is
    // configured (settings.yaml `min_avg_turnover_eur`; 0 = off). NOTE: a thin listing can still report a
    // tiny NONZERO turnover (0Y72.L = €0K rounded, i.e. Some(~0), not None) and slip past this gate with a
    // 0 floor -> the identical-horizon artifact those listings ride is caught downstream by #23.
    match quote.avg_turnover_eur {
        None => return None, // untradeable / turnover unknown -> not a deep-liquid compounder
        Some(v) if tuning.min_avg_turnover_eur > 0.0 && v < tuning.min_avg_turnover_eur => return None,
        _ => {}
    }
    // (#33) minimum listing age. Checked EARLY so a too-young name rejects with an explicit reason
    // (mirrored in gate_failures) instead of silently `?`-bailing at long_leg_fixed below. age_years
    // is None in the backtest pool -> gate inert there (edge untouched); it only bites the live screen.
    if tuning.growth_min_age_years > 0.0 && quote.age_years.is_some_and(|a| a < tuning.growth_min_age_years) {
        return None; // too young -> no multi-year record to trust as a proven compounder
    }
    // (AUM) ETF minimum fund size (EUR-approximate, from the BF etp_search payload). A sub-scale
    // fund is the one instrument a 20y hold can lose WITHOUT the strategy failing: issuers liquidate/
    // merge small funds, forcing a taxable exit mid-hold. ETF-only (companies aren't funds; crypto has
    // no issuer); aum_eur is None for stocks/crypto/backtest/off-BF names -> gate inert (missing data
    // is not a small fund — same stance as the age gate above).
    if !crypto && tuning.growth_min_aum_etf > 0.0 && quote_is_etf(quote) && quote.aum_eur.is_some_and(|a| a < tuning.growth_min_aum_etf) {
        return None; // sub-scale fund -> liquidation/merge risk over a decades hold
    }
    let min_range = if crypto { tuning.growth_min_range_pct_crypto } else { tuning.growth_min_range_pct };
    if quote.range_pct < min_range {
        return None; // equities: NOT near its high -> the on-sale lane's job. crypto: looser floor (alts run below ATH)
    }
    // (S-8Y) the SAME percentile bar on the LAST 8 YEARS. `range_pct` above is measured on the ~10y
    // fetched chart, so a name whose old, much-lower closes prop up its percentile clears it while its
    // recent 8 years read as a name in decline — PGR ranked #1 (score 22.3, next 20.9) at 2Y -14.0% and
    // 29.4% off its high on exactly that gap: its +17%/yr life CAGR is powered by decades now outside
    // the window. This bar IS what blanks the `S-8Y` column (see the knob doc for why range is the only
    // swapped stat that can newly reject), so an armed gate and a blank cell can never disagree.
    // `is_some_and`: no `stats_8y` = under 8y of record = its whole span IS the window, already judged
    // by the bar above. Missing data is not a failed bar, same as every other gate here.
    // LIVE-ONLY BY CONSTRUCTION: `stats_8y` is set only in fetch.rs, so this is inert in the backtest —
    // the validated edge cannot move, and equally cannot vouch for this gate.
    let min_range_8y = if crypto { tuning.growth_min_range_pct_8y_crypto } else { tuning.growth_min_range_pct_8y };
    if min_range_8y > 0.0 && quote.stats_8y.as_ref().is_some_and(|s| s.range_pct < min_range_8y) {
        return None; // near its 10y high only because the window is long enough to hide the last 8 years
    }
    // a "20yr+ proven CAGR" candidate must HAVE a multi-year record. Crypto used to fall back to its
    // 1Y leg here — but that admitted no-history tokens (microNFT, freshly-listed scams with a
    // +100000% data-artifact year) into a lane that promises a proven long trend. Require a real >2Y
    // leg for crypto too: trust_factor already treats 5Y as "proven enough" for young EUR pairs, so
    // this just promotes that bar from a soft halving to a hard gate (BTC/ETH/XMR/… all have 5Y).
    let (long_cum, long_years) = long_leg_fixed(quote, tuning.fixed_cagr_years, tuning.growth_min_leg_years)?; // (#15) pin the CAGR window
    let long_cagr = long_cagr_from(quote, tuning, long_cum, long_years); // (#14) the `LEG` column shows this
    let min_cagr = if crypto { tuning.growth_min_cagr_crypto } else { tuning.growth_min_cagr };
    if long_cagr < min_cagr {
        return None; // equities: weak trend = expensive laggard. crypto: looser floor (show all growers vs BTC)
    }
    // (#3i) the SAME bar against the WHOLE-LIFE CAGR. The leg above is the most recent 20/8/5Y rung, so a
    // name with a bad first decade clears it on its good second one — a 46y listing was admitted on its
    // last 20 years and the first 26 never touched a gate. The gold-miner ETFs are the live case: they
    // crashed 2011-15 and ran since, so IAUP.L reads +3%/yr since listing against a +16%/yr 8Y leg.
    //
    // (#3j) CORRECTION to what (#3i) shipped here. That comment called this leg "LIVE-ONLY / unmeasurable
    // by construction", on the reasoning that `backtest_quote` never fills life_cagr because "a point-in-
    // time slice has no whole-life history". The premise was false: `backtest_quote` slices `[..=as_of]`
    // from the FIRST bar of the full series, so the whole-life history was always present and merely
    // unread. The field is now filled there (core.rs, same `core::life_cagr` as the live fetch), this leg
    // fires in the backtest too, and the gate is MEASURED rather than asserted — see the (#3j) runs.
    //
    // `is_some_and` still matters, for the honest reason: a name under 6 months old has no life CAGR, and
    // absent history must not read as a failed bar. It can only REJECT a name, never admit one.
    //
    // (#73) the number is now `life_leg_cagr`, not `quote.life_cagr` — the last min(age, N) years when
    // `life_cagr_max_years` is armed, the uncapped lifetime when it is 0 (today's behaviour, and the
    // default). What (#3i) missed is that this bar has no window AT ALL while leg 1 has three: a name
    // whose dead decade is old fails here on history it no longer resembles. Two-sided, which is why it
    // needed a book and not an argument: it also REJECTS a has-been whose early run still flatters a
    // 20Y rung. See the (#73) receipt for the ladder and the verdict.
    if life_leg_cagr(quote).is_some_and(|l| l < min_cagr) {
        return None; // strong recent leg, mediocre whole life -> not a proven compounder
    }
    let return_1y = perf_pct(quote, "1Y")?;
    // equities must be climbing this year; crypto is allowed down to its looser 1Y floor so the market
    // base (Bitcoin, often red year-on-year) and near-BTC coins still appear, ranked vs BTC.
    // The equity leg reads `growth_min_1y_pct` (default 0.0 = the constant this replaced). Must stay
    // the SAME expression as the `gate_failures` copy — the two disagreeing means a name is silently
    // dropped from the ranking while the tail claims it passes, or vice versa.
    let y1_floor = if crypto { tuning.min_1y_pct_crypto } else { tuning.growth_min_1y_pct };
    if return_1y <= y1_floor {
        return None; // not climbing (equities) / a corpse below the crypto floor -> no trend to ride
    }
    let knife = if crypto { tuning.max_1m_drop_pct_crypto } else { tuning.max_1m_drop_pct };
    if perf_pct(quote, "1M").unwrap_or(0.0) <= knife {
        return None; // rolling over hard this month -> momentum broke
    }
    // (#23) DEGENERATE-SERIES gate: a real, continuously-traded name CANNOT show identical cumulative
    // returns at 1D, 1W AND 1M — that requires exactly ONE bar to have moved in a whole month. It's the
    // signature of a thin/dead listing that repriced once (0Y72.L printed +212.9% identically at every
    // horizon and rode it to #1 via accel). The turnover gate (#20) misses it because Yahoo reports a
    // tiny NONZERO volume (Some(~0), not None). accel = 1Y − CAGR then treats that single jump as a
    // "building trend". Reject the artifact directly. Backtest-safe: backtest_quote builds perf from a
    // continuous close series (1D≠1W≠1M), and the |1D|>0.5 guard skips a genuinely flat span, so this
    // never fires on real history -> the validated edge is untouched.
    if let (Some(d1), Some(w1), Some(m1)) =
        (perf_pct(quote, "1D"), perf_pct(quote, "1W"), perf_pct(quote, "1M"))
    {
        if d1.abs() > 0.5 && (d1 - w1).abs() < 1e-6 && (d1 - m1).abs() < 1e-6 {
            return None; // single-bar repricing artifact -> not a tradeable price history
        }
    }
    // (3) consistency: a near-high name negative over 5Y mooned-then-bled — its great 10Y CAGR is a
    // stale endpoint, not a durable trend. The 8Y/20Y rungs extend the same idea to the windows NO
    // other gate reads: under `use_life_cagr` the leg check and the life check are the same number,
    // so a name whose great decades are early clears every CAGR bar on history it will not repeat.
    // A leg the quote lacks (`n/a`) is SKIPPED — missing history is not a weak return, and rejecting
    // on it would cut every ETF and coin, none of which has a 20Y leg at all.
    // Must stay the SAME table as the `gate_failures` copy — `long_leg_floors` IS that table, so the
    // two cannot drift (picks.rs:745 explains what drift costs).
    for (label, _, floor) in long_leg_floors(tuning, crypto) {
        if perf_pct(quote, label).is_some_and(|p| p <= floor) {
            return None;
        }
    }
    // (#24) EXTREME-STRETCH gate: reject names too far above their 200wk SMA — past the brake cap the
    // damp saturates, so the brake alone can't remove a 5x-above-trend blow-off. Same-batch triple:
    // ceiling 150 lifts edge +106.6 -> +115.9 (winsorized +84.1 -> +88.1, OOS +0.13|+0.08 both +) and
    // the excluded names average -125.1 pts forward vs peers (n=267) — unlike the low-R² cohort, which
    // BEAT the field (round 4). Distinct signals: an ugly past (low R²) is fine; an extreme present
    // stretch is not. 100 measured edge-flat (+106.9), so the ceiling sits at 150, ABOVE the brake cap:
    // moderately-stretched names stay (flagged `!`), only the blow-off tail is cut. 0 = off.
    if !crypto && tuning.growth_max_above_ma > 0.0 && quote.above_ma_pct > tuning.growth_max_above_ma {
        return None;
    }
    // (#25) LIFETIME-UPTREND gate: the ranked CAGR uses the longest 20Y/10Y/5Y leg, so a name that
    // mooned at IPO, crashed, and partially recovered can show a healthy 10Y CAGR while its WHOLE-LIFE
    // trend is still negative. quote.trend_cagr is the full-history log-slope fit (endpoint-robust,
    // same fn live and in backtest_quote -> train==serve); reject when it never turned positive.
    // Second leg: trend_cagr is fit on the FETCHED daily window (~10y), so a name that collapsed
    // BEFORE that window and recovered inside it slips through (MSCI Greece: -95% by 2012, positive
    // window trend, yet whole-life CAGR -8%/18y). quote.life_cagr is the true listing-to-date CAGR
    // from the merged monthly-max series. (#3j) NOTE: this used to add "None in backtest, so the leg is
    // live-only and edge-blind by construction" — `backtest_quote` fills life_cagr now, so the leg runs
    // on both paths and that exemption is gone.
    if !crypto
        && tuning.growth_require_lifetime_uptrend
        && (quote.trend_cagr.is_some_and(|t| t <= 0.0) || quote.life_cagr.is_some_and(|l| l <= 0.0))
    {
        return None;
    }
    // (#26) MAXDD gate: reject names whose worst-ever peak-to-trough loss exceeds the cap — the
    // continuous pain signals (Sharpe/Calmar) were measured near-inert here, but a hard tail cut is a
    // different lever (round 7 precedent: damp-verdicts don't transfer to gates). Per-class cap:
    // coins crash >90% every cycle, so crypto gets its own bar ("worse than Bitcoin"), not the equity one.
    let maxdd_cap = if crypto { tuning.growth_maxdd_cap_crypto } else { tuning.growth_maxdd_cap };
    if maxdd_cap > 0.0 && quote.max_drawdown_pct > maxdd_cap {
        return None;
    }
    // (#36) crypto VOL cap: reject coins whose daily swing runs wilder than the base — same
    // philosophy as the crypto maxdd bar ("no worse than Bitcoin"), applied to day-to-day stdev
    // instead of the worst-ever tail. Crypto-only (absent from the backtest pool -> edge-blind);
    // equities never reach multi-% daily stdev. None (missing series) passes, like every data gate.
    if crypto
        && tuning.growth_max_vol_crypto > 0.0
        && quote.volatility_pct.is_some_and(|v| v > tuning.growth_max_vol_crypto)
    {
        return None;
    }
    // (#45) CRYPTO VALUATION CEILING — the class-native twin of the PEG ceiling just below, and the
    // first crypto cheapness measure to survive measurement (the (#37) DefiLlama revenue proxy was
    // rejected: chain "P/E" in the thousands, ETH needing 1,629 %/yr to clear a 1.6 bar). MVRV = market
    // cap / realized cap — what the market pays over what its holders actually paid. Compared DIRECTLY,
    // not reciprocally like `peg_yield`: on this scale expensive is already high.
    //
    // Explicitly `crypto`-scoped rather than relying on a None field, unlike the fundamentals gates
    // below: `quote.mvrv` really is None for every non-coin, but the scope is a fact about the metric
    // (realized cap is on-chain), not an accident of which fetches filled which struct.
    //
    // The shipped 2.0 is NOT a fresh judgement. `nupl_euphoria` (0.5) already declares where crypto
    // greed begins, and NUPL = 1 - 1/MVRV, so NUPL 0.5 IS MVRV 2.0. The damp and this gate share one
    // threshold: a coin above it is both score-damped and rejected. That double charge is deliberate —
    // the alternative is two numbers that both mean "expensive" and drift apart, which is exactly the
    // failure the PEG cell below documents.
    //
    // DO NOT lower this toward 1.0 without re-reading the `discount_weight` receipt: the walk-forward
    // backtest found deepest-dip ranking BACKWARDS, and below ~1.0 this ceiling inverts into precisely
    // that — it would cut BTC (1.19 live) and keep the most bled alts (ICP 0.28, CRO 0.26).
    if crypto && tuning.crypto_max_mvrv > 0.0 && quote.mvrv.is_some_and(|m| m > tuning.crypto_max_mvrv) {
        return None;
    }
    // (#37)/(#38) VALUATION + MARGIN gates, both off (0) by default. No `!crypto` guard, unlike the two
    // above: coins and ETFs carry no `quote.fund` at all, so the None arms already scope these to
    // equities. Adding the guard "for consistency" would be a second thing to keep true for no gain.
    let ff = quote.fund.as_ref();
    // (#37) PEG CEILING. The knob is expressed as PEG so it reads like the column and like the request;
    // it is APPLIED to `peg_yield`, which is 1/PEG x 100 (peg_yield 100 <=> PEG 1), hence the reciprocal
    // and the flipped comparison — a HIGH peg_yield is cheap. peg_yield is filled from ONE fn on every
    // path (fetch.rs live enrich, backtest.rs loop, report.rs mirror) -> train==serve.
    //
    // This IS the printed `peg` cell now, and that is not an accident. Until 2026-07-27 the cell computed
    // its own PEG (`pe_ratio / long_cagr_from`) while this gate read `peg_yield`, whose growth term was a
    // hardcoded `trend_cagr` — one arm of the same three-way switch the cell was following via config.
    // Under `use_life_cagr: true` the arms diverge, and the tool cut APH at PEG 2.02 in the very run it
    // ranked ODFL printing 2.51. Both sides now divide by `long_cagr_pct`, so the ceiling means what the
    // column shows. Do not re-derive a PEG anywhere; route through `long_cagr_pct`.
    if tuning.growth_max_peg > 0.0 {
        let bar = 100.0 / tuning.growth_max_peg; // PEG 2.0 -> reject peg_yield < 50
        match ff.and_then(|f| f.peg_yield) {
            Some(p) if p < bar => return None, // pricier than the ceiling for the growth it delivers
            // peg_yield None-outs loss-makers AND negative growth alike, so None is ambiguous. Split it
            // on the one case we can actually identify: negative EPS is not "missing data", it is a name
            // with no earnings to be cheap against, and letting the most expensive cohort walk through a
            // valuation ceiling would make the gate mean the opposite of its name. Deliberate departure
            // from the house "None passes" rule, and the only one here.
            None if ff.and_then(|f| f.eps_ttm).is_some_and(|e| e <= 0.0) => return None,
            _ => {} // no fundamentals at all -> passes, like every other data gate
        }
    }
    // (V) …and the other half of that "None is ambiguous": a filer that states no EPS in ANY filing, so
    // the ceiling above can never be applied to it at all. Separate knob because it is a different claim
    // — not "too expensive", but "cannot be priced" — and separate from `peg_yield.is_none()`, which
    // would also swallow every ETF, coin and uncovered filer. See the config doc for the cohort.
    if tuning.growth_require_peg && ff.is_some_and(|f| f.eps_never_reported) {
        return None;
    }
    // (#38) NET-MARGIN FLOOR on `fund.net_margin` — the as-of, point-in-time twin of the NET% column
    // (`net_margin_fy` is display-only and live-only, so it is unusable for a gate that has to be
    // measured). Read the knob doc before raising this: a floor here cuts low-margin INDUSTRIES
    // (retail, distribution, industrials) rather than low-quality names.
    if tuning.growth_min_net_margin > 0.0
        && ff.and_then(|f| f.net_margin).is_some_and(|m| m < tuning.growth_min_net_margin)
    {
        return None;
    }
    // (#39) MARGIN-SWING ceiling — the CYCLE detector the (#38) level floor cannot be. `margin_stability`
    // is -std(net_margin) across the as-of annual filings, so it is always <= 0 and the knob carries the
    // POSITIVE std (see its config doc). A peak-cycle fertilizer or refiner posts a fine margin LEVEL at
    // the top of its cycle — only the dispersion gives it away.
    //
    // Deliberately NOT wired as a `growth_fund_extra` tilt, which would need no code at all: that path
    // does `v.clamp(0.0, cap)`, and since this factor is never positive EVERY value would clamp to 0 and
    // the term would read INERT — reporting "no change" and looking like a safe flip, which is exactly
    // the (#3i) failure mode. A sign-negative factor needs a gate, not a floor-at-zero reward.
    if tuning.growth_max_margin_swing > 0.0
        && ff
            .and_then(|f| f.margin_stability)
            .is_some_and(|s| s < -tuning.growth_max_margin_swing)
    {
        return None;
    }

    // ---- SCORE ----
    let trend = capped_trend(long_cagr, tuning); // proven compounding; long_trend_cap 0 = uncapped (shipped)
    let accel = (return_1y - long_cagr).clamp(0.0, tuning.growth_accel_cap); // last year outpacing the long run = building
    // (#48) distance from the name's own 10y high, blended toward neutral by its authority knob:
    // w=1 is the raw `range_pct/100` multiply (0.8..1.0 inside the shipped gate — closer to the high
    // = stronger confirmation), w=0 turns the term off, w<0 inverts it so the name furthest below its
    // high scores highest. Clamped at 0 because `combine_damps` fractional-exponents its inputs and a
    // negative factor there is NaN — unreachable at the shipped gate, guarded at the boundary anyway.
    let proximity =
        (1.0 + tuning.growth_proximity_weight * (quote.range_pct / 100.0 - 1.0)).max(0.0);
    let risk_reward = risk_bonus(quote, long_cagr, tuning.sharpe_weight, tuning.calmar_weight, tuning); // (B/C) growth lane's Sharpe weight
    // (M) 12-1 momentum: trailing-year return EXCLUDING the last month (skip the short-term-reversal
    // month — Jegadeesh-Titman). Price-only, so it's validated end-to-end (backtest_quote has 1Y/1M),
    // unlike the still-blind value/TER tilts (div and ROE are sighted now). Missing 1M -> skip-month = 0. Guard the denominator
    // against a near-total-wipeout 1M (>= -99%) so the ratio can't blow up.
    let r1m = perf_pct(quote, "1M").unwrap_or(0.0);
    let mom121 = ((1.0 + return_1y / 100.0) / (1.0 + r1m / 100.0).max(0.01) - 1.0) * 100.0;
    // each term broken into its own local so `ScoreParts`/`explain_growth_score` can show the arithmetic
    // without recomputing (single source). Summed in the SAME order as before -> byte-identical base.
    let trend_term = tuning.growth_trend_weight * trend;
    let accel_term = tuning.growth_accel_weight * accel;
    let quality = quality_reward(quote, tuning); // (F) return-on-capital tilt — MEASURED (Δ-12.6 edge here, Δ-48.7 in the buy lane), and it OVERLAPS the (G) tilt below: see ci-settings (#3d)
    let dividend = dividend_reward(quote, tuning.dividend_weight, tuning); // (D) total-return tilt, NET of Portuguese tax (EU payers keep more — Art. 40.º-A CIRS; see dividend_reward). Closes are price-only (no adjclose) so divs are missing from the CAGR. SIGHTED since #53 (as-of divs are plumbed; weight AND tax split both curve-graded), small (near-high growers are low-yield). 52w-high anchor was sweep-tested here too and REGRESSED the 12y edge at every weight -> dropped
    // (G) as-of FUNDAMENTAL tilt. Like the (D)/(F) terms above — all sighted now — this one IS validatable: the
    // backtest attaches the as-of factor to quote.fund_factor so `backtest <set> fund` can ablate it.
    // Floor at 0 (only reward the factor, don't penalise a missing/negative one) and cap the artifact.
    // weight 0 (default) -> this whole term is 0 -> growth_score is byte-identical to the pre-(G) lane.
    // (G+) plus any ADDITIONAL named terms, each with its own weight and its own cap — the factors are
    // on incompatible scales, so one shared clamp would silently flatten whichever is smaller.
    // `growth_fund_extra` is empty by default, and an empty sum is exactly 0.0, so the default lane is
    // byte-identical to the single-factor tilt. An unknown factor name reads None -> contributes 0.
    // (N) a MISSING extra factor scores `t.neutral`, not 0. The line above says "don't penalise a
    // missing/negative one" and the old `unwrap_or(0.0)` only delivered the NEGATIVE half: in a
    // ranking, absent is not neutral — it is a demotion of up to `weight × cap`. 114 of 509 cached SEC
    // filers (22.4%) report no `op_margin`, so roic is None for them and they were charged the full
    // 0.25 × 40 = 10 pts against a covered-peer median of 17.3. `neutral` defaults to 0.0, so a term
    // that doesn't set it behaves exactly as before and every recorded receipt still holds. The clamp
    // stays OUTSIDE the fill on purpose: a known-bad value floors at 0 and ranks BELOW an unknown one.
    // (#59) BUT the fill is a live scoring term wherever the factor is UNCOVERED. With the factor
    // missing for every row — any run without `fund` — this sum is the constant `weight × neutral`, and
    // a constant here is NOT rank-neutral: it is added to `base` below and `base` is then multiplied by
    // trust × overext × proximity × value, so it reaches the score as `constant × multiplier` and ranks
    // by the multipliers. The `liq_bonus` note further down makes the opposite call for the opposite
    // reason and is right: that one is added OUTSIDE the brake, where a constant really is inert. The
    // test of rank-neutrality is WHICH SIDE OF THE MULTIPLICATION the term lands on, nothing else.
    let fund = tuning.growth_fund_weight * quote.fund_factor.unwrap_or(0.0).clamp(0.0, tuning.growth_fund_cap)
        + tuning
            .growth_fund_extra
            .iter()
            .map(|t| {
                let v = quote.fund.as_ref().and_then(|f| crate::core::select_fund_factor(f, &t.factor));
                t.weight * v.unwrap_or(t.neutral).clamp(0.0, t.cap)
            })
            .sum::<f64>();
    // (M) 12-1 momentum tilt. Floor at 0: reward momentum, don't punish its absence (matches (G)/div).
    // weight 0 (default) -> this term is 0 -> growth_score is byte-identical to the pre-(M) lane.
    let mom_term = tuning.growth_mom121_weight * mom121.clamp(0.0, tuning.growth_mom121_cap);
    // (E) trend-smoothness reward: pays names whose long climb is a straight line (high R² of the
    // log-price trend fit) over equal-CAGR rollercoasters. trend_r2 is rebuilt in backtest_quote, so
    // unlike the BACKTEST-BLIND tilts this term is validated end-to-end — the weight-2/5/10/20 sweep
    // peaked at 5 (Δedge +13.2 same-batch, rho intact; 20 collapsed -38). Weight 0 = term inert.
    // A 6M-momentum twin (F) was swept in the same run and came back null — not wired.
    let smooth = tuning.growth_smoothness_weight * quote.trend_r2;
    // drawdown-DURATION penalty: dock per year of the longest below-prior-peak stretch (the depth cap
    // #26 can't see a shallow-but-endless bleed). Price-only, rebuilt in backtest_quote (daily cadence)
    // -> validated end-to-end; standalone probe 2026-07-19: rho +0.26, edge +27.9, OOS +0.40|+0.14.
    // None (no history) -> no penalty, matching the missing-data stance of every gate. Weight 0 = off.
    let underwater = -tuning.growth_underwater_weight * quote.underwater_yrs.unwrap_or(0.0);
    let base = trend_term + accel_term + risk_reward + quality + dividend + fund + mom_term + smooth + underwater;
    let value_raw = value_factor(quote, tuning.ref_pe); // (E) a nosebleed P/E still damps the score (anti top-chase)
    // (Item 20) dial the BLIND P/E multiplier's authority toward neutral 1.0. weight 1.0 = full ×0.5..1.5
    // swing (default, unchanged); 0.0 = off. The validated edge was measured with this term OFF (pe_ratio
    // is None in the backtest), so this knob lets valuation move to the validated additive earnings_yield
    // term (Item 19) once it probes +, without a recompile. On-sale `buy_score` keeps full value_factor.
    let value = 1.0 + tuning.growth_value_weight * (value_raw - 1.0);
    let trust = trust_factor(
        quote,
        crypto,
        tuning.fixed_cagr_years,
        tuning.growth_trust_ladder,
        tuning.growth_trust_young,
    ); // (A) equities need a 10Y leg (the pinned window when fixed_cagr_years is set); crypto: only 5Y. (#47) or the graded 20/8/5 ladder, (#49) with the 2Y/1Y rungs under it
    // (1) overextension brake: how far the price has run ABOVE its own 200wk SMA. Far above trend =
    // stretched/blow-off, so taper the score toward `growth_overext_floor` at the cap. This is the
    // generic brake the P/E tilt can't provide for crypto/ETFs (no earnings) — works on price alone.
    // (Tried a CAGR-conditional floor — brake elite compounders less — but it CUT wide edge
    //  +108.9->+80.5 and flipped OOS-late negative: high-CAGR stretched names revert too. Hard brake stays.)
    // (#4) per-class cap: crypto rides far above its long SMA normally, so it gets its own (looser) cap.
    let overext_cap = if crypto { tuning.growth_overext_cap_crypto } else { tuning.growth_overext_cap };
    let overext = quote.above_ma_pct.min(overext_cap);
    let overext_damp = if overext_cap > 0.0 {
        1.0 - (overext / overext_cap) * (1.0 - tuning.growth_overext_floor) // 1.0 at trend .. floor at the cap
    } else {
        1.0 // cap 0 = brake disabled
    };
    // (L) liquidity tilt, added OUTSIDE the brake so a parabolic stretch can't crush it: deep-liquid
    // mega-caps are easier to hold/exit over decades and harder to manipulate, a real quality the
    // brake-docked score ignores. Reward only turnover ABOVE €1B (ln ratio, 0 below) so it lifts proven
    // liquid compounders (NVDA €32B) over the illiquid €200-500M names they trail on the docked score,
    // not the whole field. RANK-NEUTRAL in the backtest: backtest_quote sets a uniform sentinel turnover
    // (#20), so this liq_bonus is the SAME constant offset on every backtest name -> cross-sectional order
    // (and the validated edge) is untouched.
    let liq_bonus = if tuning.growth_turnover_weight > 0.0 {
        tuning.growth_turnover_weight * quote.avg_turnover_eur.map_or(0.0, |v| (v / 1e9).max(1.0).ln())
    } else {
        0.0
    };
    // geomean the pure penalties (bounded — see combine_damps).
    // (Tried (#1) an R²-trend-steadiness damp @0.5 and (#3) a 3M/6M momentum-confirm damp @0.3 here.
    //  backtest universe ablation (467 win) said BOTH edge-negative: removing R² lifted edge +45.6->+68.0
    //  & rho +0.18->+0.20; removing mom-confirm lifted edge +5.9. R² docks exactly the parabolic
    //  compounders this lane exists to surface, and accel already encodes momentum -> dropped both.)
    let damp = combine_damps(&[trust, overext_damp]);
    // (#8) PROBE: fold proximity + value INTO the geomean so three soft multipliers can't compound to
    // ~0 (a name at prox 0.7 × value 0.8 × damp 0.85 keeps only 0.48 of base). The geomean bounds the
    // stack by the SOFTEST term instead of the raw product. Edge-affecting — it also changes the geomean
    // SLOT COUNT (trust/overext exponent ½ -> ¼), which alone can move the edge (a past constant-1.0 slot
    // deletion shifted it +98->+109) -> knob-gated, default off = the raw-multiply formula, edge intact.
    // (T) TER cost drag — ETF-only, BACKTEST-BLIND. The expense ratio is the ONE cost certain to
    // compound against a decades hold, so dock the score by the actual 20-year wealth multiple it eats:
    // (1 − TER)^20 (20 = the lane's stated hold horizon). Tiebreaks near-identical index ETFs (two
    // Nasdaq-100 -> the cheaper NET-return wins) that momentum alone ranks arbitrarily. expense_ratio is
    // None for stocks/crypto/no-source AND in the backtest pool -> ×1.0 there -> edge byte-identical.
    // Knob-gated: off (default) = byte-identical to the pre-(T) lane.
    let ter_damp = if tuning.growth_ter_drag {
        quote.expense_ratio.map_or(1.0, |t| (1.0 - t / 100.0).max(0.0).powi(20))
    } else {
        1.0
    };
    // (#44) COMMODITY dock — same shape as ter_damp (multiplicative, knob-gated, backtest-blind), and
    // for a related reason: a commodity-linked row's long CAGR is a spot-price snapshot, not compounding.
    // Docks on the sector CAUSE, not the price SYMPTOM — which is the whole difference from the (#1)
    // R²-steadiness damp recorded above as edge-NEGATIVE: R² docked the parabolic compounders this lane
    // exists to surface, while this leaves every one of them untouched and fires on Energy/Materials only.
    // Unmeasurable by construction (no sector in the pool), so it can never be swept — see the receipt.
    // 1.0 = off; a configured 0.0 is ALSO off, so reaching for the house `0 = off` convention on a knob
    // that inverts it can't silently zero every Energy row out of the table.
    let commodity_damp = match tuning.growth_commodity_damp {
        d if d > 0.0 && is_commodity(quote) => d,
        _ => 1.0,
    };
    // (#45) FX/venue dock — same shape as commodity_damp, for a cost instead of a signal defect: a
    // non-EUR-quoted ETF line (GBp/USD/SEK/CHF) costs a EUR investor broker FX conversion plus the
    // off-home spread that the EUR line of the same multi-listed fund does not. Tie-break sized, so
    // it prefers the EUR twin without burying a genuinely stronger foreign listing. ETF-only:
    // stocks are one all-USD lane (a uniform dock reorders nothing) and crypto -EUR legs quote EUR.
    // None currency = unknown = innocent — which is every backtest quote, so this is backtest-blind
    // by construction like (#44) and can never be swept.
    let fx_damp = match tuning.growth_fx_damp {
        d if d > 0.0 && is_noneur_etf(quote) => d,
        _ => 1.0,
    };
    let score = if tuning.growth_geomean_fold {
        base * combine_damps(&[trust, overext_damp, proximity, value]) * ter_damp * commodity_damp * fx_damp + liq_bonus
    } else {
        base * proximity * value * damp * ter_damp * commodity_damp * fx_damp + liq_bonus
    };
    Some(ScoreParts {
        long_cagr, return_1y, trend, accel, trend_term, accel_term, risk_reward, quality, dividend,
        fund, mom121: mom_term, smooth, underwater, base, proximity, value_raw, value, trust, overext,
        overext_cap, overext_damp, damp, liq_bonus, ter_damp, commodity_damp, fx_damp, score,
    })
}

/// Scalar growth score — the number `screen`/`size`/`backtest` rank on. Thin wrapper over
/// `score_parts` so the ranked value and the `explain_growth_score` breakdown share one computation.
pub fn growth_score(quote: &Quote, tuning: &BuyHeuristic) -> Option<f64> {
    score_parts(quote, tuning).map(|p| p.score)
}

/// (B) DIAGNOSTIC — read-only, never scored. For a name the growth lane REJECTED, return the ONE gate it
/// fails IF it fails EXACTLY one of the actionable numeric gates AND fails it by only a small margin: a
/// "near miss" — a compounder one notch outside the fence (e.g. a great name 25% off its high failing only
/// the range gate, or one whose long CAGR is a hair under the floor). A gross miss (down 34% over 1Y) is a
/// hard reject, not a near miss, so it's dropped. `None` = not a candidate (leveraged / stablecoin / no multi-year
/// history / no 1Y data — nothing to "almost pass"), OR it clears every gate (would be ranked), OR it
/// fails ≥2 (not a near miss). Returns (gate_name, human "why" string) for the printed tail in `screen`.
///
/// ponytail: MIRRORS the gates in `score_parts` instead of sharing them — this is cosmetic (a printed
/// tail), so duplicating the checks keeps the load-bearing, edge-validated scorer untouched. Drift only
/// mislabels the tail, never the rank. Keep in sync if a `score_parts` gate changes.
pub fn growth_near_miss(quote: &Quote, tuning: &BuyHeuristic) -> Option<(&'static str, String)> {
    let mut fails = gate_failures(quote, tuning)?;
    // exactly one gate failed AND it's a CLOSE miss -> a genuine near-miss worth surfacing
    match fails.len() {
        1 if fails[0].2 => {
            let (gate, why, _) = fails.pop().unwrap();
            Some((gate, why))
        }
        _ => None,
    }
}

/// The MULTI-gate sibling of `growth_near_miss`: a name one notch outside EXACTLY `n` fences, every
/// one of them close. `None` for everything else — a different failure count, or one where any fail is
/// a gross miss (a hard reject, not a near miss). Returns (the failing gates in gate order, the joined
/// "why") for `screen`'s second and third tails.
///
/// Exists because the one-gate block above is the whole reason a name like MSFT could vanish from the
/// tool entirely: no table row (it fails a gate), no near-miss line (it fails two), and so no way to
/// see it at all. One gate costs one knob to recover and two cost two, which is why they print as
/// separate blocks rather than one merged list — the ARITY is the parameter, so a caller adds a block
/// per recovery cost instead of pasting this predicate again.
pub fn growth_n_gate_miss(quote: &Quote, tuning: &BuyHeuristic, n: usize) -> Option<(Vec<&'static str>, String)> {
    let fails = gate_failures(quote, tuning)?;
    if fails.len() != n || !fails.iter().all(|(.., close)| *close) {
        return None;
    }
    let why = fails.iter().map(|(g, w, _)| format!("{g}: {w}")).collect::<Vec<_>>().join("; ");
    Some((fails.iter().map(|(g, ..)| *g).collect(), why)) // gate order is deterministic in gate_failures
}

/// Names whose ONLY failing gate is the 1Y floor — a proven long record having one down year.
/// Returns `(1Y %, long CAGR %/yr, range %)` for `screen`'s down-year tail; `None` for anything else.
///
/// Deliberately ignores `is_close`, unlike its two siblings: the near-miss tail already shows the
/// shallow half of this cohort (the `1Y+` margin is a hardcoded 10pp, so 0..-10% names land there and
/// deeper ones land nowhere), and splitting one list at -10% hides exactly the names the floor costs
/// most. The overlap with the tail above is accepted on purpose.
///
/// This list is NOT a buy list. It is the cohort round 5 measured at -108.1 pts forward
/// peer-relative (n=284, 2026-07-03) before reverting the loosened floor — see `growth_min_1y_pct`.
/// The caller prints that receipt under it.
pub fn growth_down_year_miss(quote: &Quote, tuning: &BuyHeuristic) -> Option<(f64, f64, f64)> {
    let fails = gate_failures(quote, tuning)?;
    if fails.len() != 1 || fails[0].0 != "1Y+" {
        return None;
    }
    // reachable only past every `?` in gate_failures above (it returned a 1Y+ fail, so both parsed)
    let (long_cum, long_years) = long_leg_fixed(quote, tuning.fixed_cagr_years, tuning.growth_min_leg_years)?;
    Some((perf_pct(quote, "1Y")?, long_cagr_from(quote, tuning, long_cum, long_years), quote.range_pct))
}

/// (#54) What the CAGR PIN costs, BY NAME: names failing at least one gate under the pin that the SAME
/// name would clear on its longest leg. `None` when the pin is off, when the name ranks under the pin,
/// when the pin breaks nothing new, or when it is not assessable at all.
///
/// KEYED ON "WHAT THE PIN BROKE", NOT ON "WOULD IT RANK AT 0" — and that distinction was measured, not
/// guessed. The first cut of this required the name to clear EVERY gate unpinned, so the list could
/// promise "these rank at 0". Run against the live universe with `fixed_cagr_years: 8` it printed
/// NOTHING, including for AMZN — the name the block exists for — because AMZN's PEG misses its ceiling
/// on both sides of the counterfactual (PEG is P/E over this same CAGR, ~1.63 unpinned against a 1.60
/// bar). A definition that answers "the pin cost you nothing" while the pin is visibly halving a name's
/// CAGR is answering the wrong question. So the predicate is now per-GATE, and `still` carries the
/// fences that fail at 0 as well, letting the caller print an honest "setting 0 is not enough" instead
/// of silently omitting the row.
///
/// WHY THIS IS A COUNTERFACTUAL AND NOT A GATE TAIL — the four blocks above all key on WHICH gate fired,
/// which cannot represent this one. `fixed_cagr_years` does not reject anything itself: `long_leg_fixed`
/// falls back to the longest leg when the pinned window is absent, so it never returns None on account of
/// the pin. What it does is change the CAGR's VALUE, and `long_cagr_from` is the chokepoint feeding SEVEN
/// readers — the `growth_min_cagr` gate, `trend`, `accel`, `sharpe`, `calmar`, `trend_health` and `peg`
/// (which divides by it). So the pin's casualties come out of different gates name by name, and the only
/// thing they share is the counterfactual. Asking "who fails the cagr gate" would miss every name the pin
/// pushed through `peg` or `calmar` instead, and would also indict names that fail that gate anyway.
///
/// THE CASE THAT PROMPTED IT: at `fixed_cagr_years: 8`, AMZN is scored on its 8Y leg (+15.0%/yr,
/// 2018-07 -> 2026-07) rather than its 20Y one (+30.4%/yr, $1.34 -> $271.58). A 15.4pp haircut moves it
/// past several fences at once, and because the miss is both gross and multi-gate it appears in NONE of
/// the tails above — no table row, no near-miss line, no down-year line. It simply vanished, which is
/// exactly the failure mode `growth_n_gate_miss` was written to end and this extends to the pin.
///
/// THE PIN IS A COMPARABILITY CHOICE, NOT A QUALITY JUDGMENT, and the caller must say so under the list:
/// a name here is not one the tool judged bad, it is one the tool declined to judge on its own longest
/// record. Whether that trade is worth making is `fixed_cagr_years`' own receipt, not this block's.
///
/// ponytail: evaluates the gates twice per name rather than threading a second CAGR through the scorer.
/// This is a printed footer over a few thousand quotes, so the second pass is free at this scale, and it
/// keeps the edge-validated path untouched — the same bargain `growth_near_miss` already took.
pub struct PinDrop {
    pub broke: Vec<&'static str>, // gates that fail PINNED and would pass on the longest leg
    pub why: String,              // ...and their reasons, joined — the pin's own bill
    pub still: Vec<&'static str>, // gates failing at 0 too: setting the knob back is NOT enough
    pub pinned: (f64, f64),       // (CAGR %/yr, window years) under the pin
    pub free: (f64, f64),         // (CAGR %/yr, window years) on the longest leg
}

pub fn pin_dropped(quote: &Quote, tuning: &BuyHeuristic) -> Option<PinDrop> {
    if tuning.fixed_cagr_years == 0 {
        return None; // no pin -> nothing to attribute to it
    }
    let pinned_fails = gate_failures(quote, tuning)?;
    if pinned_fails.is_empty() {
        return None; // ranks WITH the pin -> not a casualty
    }
    let free_t = BuyHeuristic { fixed_cagr_years: 0, ..tuning.clone() };
    let still: Vec<&'static str> = gate_failures(quote, &free_t)?.iter().map(|(g, ..)| *g).collect();
    let broke: Vec<&'static str> =
        pinned_fails.iter().map(|(g, ..)| *g).filter(|g| !still.contains(g)).collect();
    if broke.is_empty() {
        return None; // it fails the same fences either way -> the pin is not what broke it
    }
    let why = pinned_fails
        .iter()
        .filter(|(g, ..)| broke.contains(g))
        .map(|(g, w, _)| format!("{g}: {w}"))
        .collect::<Vec<_>>()
        .join("; ");
    let leg = |t: &BuyHeuristic| {
        long_leg_fixed(quote, t.fixed_cagr_years, t.growth_min_leg_years)
            .map(|(cum, years)| (long_cagr_from(quote, t, cum, years), years))
    };
    Some(PinDrop { broke, why, still, pinned: leg(tuning)?, free: leg(&free_t)? })
}

/// WHY this name is not in the ranking, in one line, ALWAYS — the display-layer total function over
/// `gate_failures`' three outcomes. `None` (structural refusal / no 1Y leg) becomes the refusal word
/// instead of a silent skip, which is the whole point: that bucket is the one a reader can never
/// otherwise see, and "it printed nothing" is indistinguishable from "the tool has no idea".
///
/// Deliberately a WRAPPER and not a change to `gate_failures`' contract. Folding refusals into the
/// fail vec was the obvious move and is wrong: `sole_blocking_gates_are_pinned` (backtest.rs) pins the
/// set of gates that sole-block anything, and every refused name would arrive as a new sole-blocker —
/// breaking a measured receipt to improve a footer. The funnel already attributes that bucket in
/// aggregate (screen.rs, `refusal_reason(q).unwrap_or("no-1Y")`); this names it per-quote.
pub fn unranked_reason(quote: &Quote, tuning: &BuyHeuristic) -> String {
    match gate_failures(quote, tuning) {
        // `refusal_reason` covers three of the four ways out; the fourth (missing 1Y leg) is the only
        // remaining `?`-bail in `gate_failures`, so this fallback is exact rather than a guess.
        None => refusal_reason(quote).unwrap_or("no 1Y history").to_string(),
        Some(f) if f.is_empty() => "clears every gate".to_string(),
        Some(f) => f.iter().map(|(g, w, _)| format!("{g}: {w}")).collect::<Vec<_>>().join("; "),
    }
}

/// (#55) A name with a PROVEN long record that still didn't rank: (long CAGR %/yr, that leg's years,
/// every gate it fails). `None` = it ranks, it has no long record, or its record doesn't clear the
/// lane's own CAGR floor.
///
/// Exists because AMZN can fail two gates and be named by NOTHING. Every other tail in this file keys
/// on a shape it doesn't have: the funnel names only SOLE-blockers (AMZN is counted in the `cagr` and
/// `peg` rows and named in neither), near-miss needs exactly one failing gate, the two-gate tail needs
/// BOTH close (PEG 3.37 against a 1.60 ceiling is gross, not close), the down-year tail needs a sole
/// `1Y+` blocker, and the leg-floor tails need that specific floor to fire. So the one cohort a reader
/// actually asks about — great record, gone anyway — was reachable only by `--explain TICKER`, i.e.
/// only if you already suspected the name.
///
/// The cut is the RECORD, not size or turnover. A turnover cut ranks by fame: it prints the same
/// mega-caps every run whether or not any came close, and carries no information after the first read.
/// A record cut answers the question being asked, is self-limiting, and goes quiet as names decay.
///
/// Measured UNPINNED on purpose (`fixed_cagr_years: 0`), reusing `pin_dropped`'s counterfactual: the
/// pin is exactly what makes a long-record name look ordinary, so measuring the record through the pin
/// would let the pin hide the names this list exists to surface. Compared against the SAME per-class
/// floor `gate_failures` uses, so "proven" here means the lane's own definition of proven, not a
/// second opinion invented for a footer.
pub const PROVEN_MIN_YEARS: f64 = 5.0;

pub fn proven_but_unranked(quote: &Quote, tuning: &BuyHeuristic) -> Option<(f64, f64, String)> {
    let fails = gate_failures(quote, tuning)?; // refusals stay out: a 3x ETF is excluded on purpose
    if fails.is_empty() {
        return None; // ranks -> not missing
    }
    let free_t = BuyHeuristic { fixed_cagr_years: 0, ..tuning.clone() };
    let (cum, years) = long_leg_fixed(quote, 0, tuning.growth_min_leg_years)?; // no long leg -> nothing proven
    // THIS BLOCK DEFINES ITS OWN "LONG", and must: `growth_min_leg_years` is a rank-side knob a user is
    // free to zero (the live overlay does), and at 0 `long_leg` hands back a 1Y rung — which turned the
    // first live run into a list of 900%/yr one-year coins under a header promising a proven record.
    // `.max` rather than a bare 5.0 so raising the knob still tightens this list.
    if years < tuning.growth_min_leg_years.max(PROVEN_MIN_YEARS) {
        return None;
    }
    let cagr = long_cagr_from(quote, &free_t, cum, years);
    let floor = if is_currency_quoted(&quote.ticker) { tuning.growth_min_cagr_crypto } else { tuning.growth_min_cagr };
    if cagr < floor {
        return None; // no proven record -> the gates are simply right about it, nothing to explain
    }
    Some((cagr, years, unranked_reason(quote, tuning)))
}

/// Every name a given long-leg floor rejects, whatever ELSE it also fails — the loosest tail in the
/// file, and the only view of a gate that sole-blocks nobody. `tag` is a `long_leg_floors` tag ("5Y+",
/// "8Y+"); the perf label is the tag without its `+`. Returns (long CAGR %/yr, that gate's own "why",
/// the OTHER gates the name fails). `None` = not assessable as a growth candidate, or it clears this floor.
///
/// Deliberately ignores BOTH `is_close` and the failure count, unlike all three siblings above, because
/// at `growth_min_5y_pct: 75.0` every one of those filters empties the list: the 5Y bar fails 2161 names
/// and sole-blocks ZERO (funnel 2026-08-03: `5Y+ 423/0 1709/0 29/0`), and the names worth seeing miss it
/// grossly, not narrowly — AMZN at +24.7% against a +75 bar is invisible in the near-miss tail (needs one
/// gate), the two-gate tail (needs both close) and the down-year tail (needs a sole blocker) alike.
///
/// Returns the STORED `why` rather than reformatting the comparison: that is what keeps the per-class
/// floor right for free, since crypto answers to `growth_min_5y_pct_crypto` and a caller quoting one
/// number in its header would be lying about the coins.
pub fn growth_leg_floor_miss(quote: &Quote, tuning: &BuyHeuristic, tag: &str) -> Option<(f64, String, Vec<&'static str>)> {
    let fails = gate_failures(quote, tuning)?;
    let hit = fails.iter().position(|(g, ..)| *g == tag)?; // clears this floor (or the floor is off) -> not in this list
    // reachable only past every `?` in gate_failures above (it returned a long-leg fail, so the leg parsed)
    let (long_cum, long_years) = long_leg_fixed(quote, tuning.fixed_cagr_years, tuning.growth_min_leg_years)?;
    let others = fails.iter().enumerate().filter(|(i, _)| *i != hit).map(|(_, (g, ..))| *g).collect();
    Some((long_cagr_from(quote, tuning, long_cum, long_years), fails[hit].1.clone(), others))
}

/// WHY a quote is not assessable as a growth candidate at all — the structural half of
/// `gate_failures`' `None`, named so a caller can report WHICH refusal it was instead of a silent drop.
///
/// Exists for the screen's gate funnel: a tally whose denominator doesn't reconcile
/// (`scanned = refused + failed + ranked`) can't be trusted to aim a knob at anything. `gate_failures`
/// calls this as its own early-out rather than re-testing the same four conditions, so the funnel's
/// refused bucket and the ranking's refusal cannot disagree.
///
/// NOT exhaustive of `None`: `gate_failures` also bails on a missing 1Y leg, one line further down and
/// only reachable once a long leg exists. That case is `gate_failures() == None` while this returns
/// `Some`-less — i.e. the funnel reads "refused with no structural reason" as the missing-1Y case, which
/// is exact because these are the only two ways out.
pub fn refusal_reason(quote: &Quote) -> Option<&'static str> {
    if is_leveraged(&quote.name) {
        return Some("leveraged");
    }
    if is_commodity_etf(quote) {
        return Some("commodity");
    }
    if is_currency_quoted(&quote.ticker) && is_stablecoin(&quote.ticker) {
        return Some("stablecoin");
    }
    if quote.avg_turnover_eur.is_none() {
        return Some("no-turnover");
    }
    None
}

/// Every growth gate this quote FAILS, in gate order: (gate_name, human "why", is_close_miss).
/// `None` = not assessable as a growth candidate at all (leveraged / stablecoin / unknown turnover /
/// no 1Y data — see `refusal_reason`); a name with no 5y+ leg returns a single "history" fail (so a
/// pinned young ETF explains its 0.0 rather than vanishing); empty vec = clears every gate. Shared by
/// the near-miss tail above and `check`'s held-name gate review, so a held name that would no longer
/// rank gets flagged with the same wording the screen tail uses.
pub fn gate_failures(quote: &Quote, tuning: &BuyHeuristic) -> Option<Vec<(&'static str, String, bool)>> {
    let crypto = is_currency_quoted(&quote.ticker);
    // not a near-miss CANDIDATE: structural rejects / missing data have nothing to "almost pass"
    if refusal_reason(quote).is_some() {
        return None;
    }
    let turnover = quote.avg_turnover_eur?; // unknown turnover -> not assessable as a compounder
    // (#33) minimum-age gate. Reported as an explicit "young" reason. A name too young for the fixed
    // CAGR leg would `?`-bail the next line to a SILENT None (0.0 in the table, no reason) — so when
    // the leg is missing AND the age gate fires, return the young reason alone instead of dropping it.
    let young_fail = |a: f64| ("young", format!("{a:.0}y listed (need ≥{:.0}y)", tuning.growth_min_age_years), a >= tuning.growth_min_age_years - 1.0);
    let too_young = tuning.growth_min_age_years > 0.0 && quote.age_years.is_some_and(|a| a < tuning.growth_min_age_years);
    let Some((long_cum, long_years)) = long_leg_fixed(quote, tuning.fixed_cagr_years, tuning.growth_min_leg_years) else {
        // No leg at or above `growth_min_leg_years` (5y+ at the shipped value) -> the name can't be ranked
        // at all (score_parts `?`-bails here too -> a silent 0.0). ALWAYS explain it, so a pinned young ETF
        // (VUAA 2y, SPYL 3y) prints a reason instead of a mystery 0.0 in the screen's gate-review footer.
        // Prefer the actionable "young" wording when the age gate is the floor that fired; otherwise a plain
        // "history" note (nothing to tune AT THE SHIPPED SETTING — the knob exists, it is just not graded).
        if too_young {
            return Some(vec![young_fail(quote.age_years.unwrap())]);
        }
        let why = match quote.age_years {
            Some(a) => format!("{a:.0}y listed, no 5y+ record to rank"),
            None => "no 5y+ record to rank".to_string(),
        };
        return Some(vec![("history", why, false)]);
    };
    let return_1y = perf_pct(quote, "1Y")?; // no 1Y data
    let long_cagr = long_cagr_from(quote, tuning, long_cum, long_years);
    let min_range = if crypto { tuning.growth_min_range_pct_crypto } else { tuning.growth_min_range_pct };
    let min_cagr = if crypto { tuning.growth_min_cagr_crypto } else { tuning.growth_min_cagr };
    let y1_floor = if crypto { tuning.min_1y_pct_crypto } else { tuning.growth_min_1y_pct }; // same expression as score_parts, deliberately
    let knife = if crypto { tuning.max_1m_drop_pct_crypto } else { tuning.max_1m_drop_pct };
    let r1m = perf_pct(quote, "1M").unwrap_or(0.0);

    // Collect (gate, why, is_close): a gate FAILS at any magnitude, but only a fail WITHIN a margin of the
    // threshold is a genuine "near miss" worth printing (a name 34% down over 1Y is a hard reject, not a
    // near miss). Margins are hardcoded — this is a cosmetic tail, not a tuned knob.
    let mut fails: Vec<(&'static str, String, bool)> = Vec::new();
    if too_young {
        fails.push(young_fail(quote.age_years.unwrap())); // has the CAGR leg but still under the age floor
    }
    // (AUM) ETF fund-size gate, same scoping as score_parts: None AUM never fails. Close = within
    // half the floor (a €60M fund vs a €100M line is a "watch it grow" case, €5M is a hard reject).
    if !crypto && tuning.growth_min_aum_etf > 0.0 && quote_is_etf(quote) {
        if let Some(a) = quote.aum_eur.filter(|&a| a < tuning.growth_min_aum_etf) {
            fails.push((
                "aum",
                format!("{} fund — liquidation/merge risk over a decades hold (need ≥ {})", turnover_cell(Some(a)), turnover_cell(Some(tuning.growth_min_aum_etf))),
                a >= tuning.growth_min_aum_etf * 0.5,
            ));
        }
    }
    if quote.range_pct < min_range {
        fails.push(("range", format!("{:.0}% in range (need ≥{:.0}%)", quote.range_pct, min_range), quote.range_pct >= min_range - 10.0));
    }
    // (S-8Y) mirror of the 8y-window range bar. MUST stay the same expression as `score_parts`' copy —
    // the two disagreeing means a name is silently dropped from the ranking while the tail claims it
    // passes, or vice versa (see picks.rs:745). Same `is_close` convention as the 10y entry above.
    let min_range_8y = if crypto { tuning.growth_min_range_pct_8y_crypto } else { tuning.growth_min_range_pct_8y };
    if min_range_8y > 0.0 {
        if let Some(s) = quote.stats_8y.as_ref().filter(|s| s.range_pct < min_range_8y) {
            fails.push(("range8y", format!("{:.0}% in 8y range (need ≥{:.0}%)", s.range_pct, min_range_8y), s.range_pct >= min_range_8y - 10.0));
        }
    }
    if long_cagr < min_cagr {
        // close = within 1.5 pp of the floor. 4.0 flooded the screen tail with ~55 "10%/yr vs a 14%
        // bar" rows — that's a different asset class, not a near miss; 1.5 keeps the genuine
        // one-notch-out compounders (13.9 vs 14.0) and nothing else.
        fails.push(("cagr", format!("{long_cagr:.1}%/yr (need ≥{min_cagr:.1}%)"), long_cagr >= min_cagr - 1.5));
    }
    // (#3i) the whole-life half of the same floor. REQUIRED, not decorative: without it a name rejected
    // by the life bar drops out of the table with no reason printed anywhere. Same 1.5pp near-miss margin
    // as the leg above. Label `cagr-life`, NOT `lifetime` — that string belongs to the `<= 0` value-trap
    // dock below, and a footer that can't tell "mediocre" from "negative" is the confusion this fixes.
    // (#73) reads `life_leg_cagr`, the same number `score_parts` rejects on — these two MUST stay one
    // expression, which is why it is a function now. The span word moves with it: printing "since
    // listing" for a windowed CAGR would be the exact defect this footer exists to prevent, a reason
    // string that names a measurement the tool did not take.
    if let Some(l) = life_leg_cagr(quote).filter(|&l| l < min_cagr) {
        let span = if quote.capped_cagr.is_some() { "in its capped window" } else { "since listing" };
        fails.push(("cagr-life", format!("{l:.1}%/yr {span} (need ≥{min_cagr:.1}%)"), l >= min_cagr - 1.5));
    }
    if return_1y <= y1_floor {
        fails.push(("1Y+", format!("1Y {return_1y:+.1}% (need >{y1_floor:.1}%)"), return_1y > y1_floor - 10.0));
    }
    if r1m <= knife {
        fails.push(("1M-knife", format!("1M {r1m:+.1}% (floor {knife:.1}%)"), r1m > knife - 8.0));
    }
    // (#23) the DEGENERATE-SERIES gate's reason — the one `score_parts` gate this mirror never carried,
    // which made a single-bar repricing count as "clears every gate" in any tally built on this fn. Same
    // expression as the copy in `score_parts`, deliberately.
    //
    // `is_close` is FALSE and always will be: a thin listing that repriced once is not a name one notch
    // outside a fence, so it stays out of the near-miss tail (which needs a close fail) — and a name
    // failing this PLUS one close gate correctly drops out of the two-gate tail, which needs both close.
    if let (Some(d1), Some(w1), Some(m1)) =
        (perf_pct(quote, "1D"), perf_pct(quote, "1W"), perf_pct(quote, "1M"))
    {
        if d1.abs() > 0.5 && (d1 - w1).abs() < 1e-6 && (d1 - m1).abs() < 1e-6 {
            fails.push(("artifact", format!("1D/1W/1M all {d1:+.1}% — single-bar repricing, not a price history"), false));
        }
    }
    // mirror of the `long_leg_floors` loop in `score_parts` — same table, so the two cannot disagree.
    for (label, tag, floor) in long_leg_floors(tuning, crypto) {
        if let Some(p) = perf_pct(quote, label).filter(|p| *p <= floor) {
            fails.push((tag, format!("{label} {p:+.1}% (need >{floor:.0}%)"), p > floor - 15.0));
        }
    }
    if tuning.min_avg_turnover_eur > 0.0 && turnover < tuning.min_avg_turnover_eur {
        fails.push(("liquidity", format!("€{:.0}K/day (floor €{:.0}K)", turnover / 1e3, tuning.min_avg_turnover_eur / 1e3), turnover >= tuning.min_avg_turnover_eur * 0.5));
    }
    if !crypto && tuning.growth_max_above_ma > 0.0 && quote.above_ma_pct > tuning.growth_max_above_ma {
        fails.push(("stretch", format!("+{:.0}% above 200wk SMA (ceiling +{:.0}%)", quote.above_ma_pct, tuning.growth_max_above_ma), quote.above_ma_pct <= tuning.growth_max_above_ma + 25.0));
    }
    if !crypto && tuning.growth_require_lifetime_uptrend {
        if let Some(t) = quote.trend_cagr.filter(|&t| t <= 0.0) {
            fails.push(("lifetime", format!("{t:+.1}%/yr whole-life trend (need >0)"), t > -3.0));
        } else if let Some(l) = quote.life_cagr.filter(|&l| l <= 0.0) {
            // window trend positive but true listing-to-date CAGR negative: collapsed before the
            // fetched window, recovered inside it (MSCI Greece pattern) — still a lifetime loser.
            fails.push(("lifetime", format!("{l:+.1}%/yr since listing (need >0)"), l > -3.0));
        }
    }
    if crypto && tuning.growth_max_vol_crypto > 0.0 {
        if let Some(v) = quote.volatility_pct.filter(|&v| v > tuning.growth_max_vol_crypto) {
            fails.push(("volatile", format!("{v:.1}%/day swing (cap {:.1}%)", tuning.growth_max_vol_crypto), v <= tuning.growth_max_vol_crypto + 0.5));
        }
    }
    // (#45) the MVRV ceiling's reason — the twin of the `crypto && crypto_max_mvrv` gate in
    // `score_parts`; the two must stay the SAME expression or the ranking and this footer disagree
    // about why a coin vanished. No unit conversion needed (unlike the PEG below): the knob, the gate
    // and the MVRV column are all the same number, so what the footer quotes is what was compared.
    // RELATIVE near-miss margin (50% over), matching the PEG leg — MVRV is a multiple, not a pp count.
    if crypto && tuning.crypto_max_mvrv > 0.0 {
        if let Some(m) = quote.mvrv.filter(|m| *m > tuning.crypto_max_mvrv) {
            fails.push(("mvrv", format!("MVRV {m:.2} (ceiling {:.2})", tuning.crypto_max_mvrv), m <= tuning.crypto_max_mvrv * 1.5));
        }
    }
    // (#37) the PEG ceiling's reason. Convert the gating value BACK to a PEG (100/peg_yield) so the
    // footer speaks the same units as the knob and the column. Since 2026-07-27 the `peg` cell prints
    // this same number, so the footer's PEG and the table's PEG must MATCH for a given ticker — that
    // equality is pinned in tests and is the check to run first if this gate ever looks wrong again.
    let ff = quote.fund.as_ref();
    if tuning.growth_max_peg > 0.0 {
        let bar = 100.0 / tuning.growth_max_peg;
        match ff.and_then(|f| f.peg_yield) {
            Some(p) if p < bar => {
                let peg = 100.0 / p; // p > 0 always: peg_yield is None unless BOTH factors are positive
                // RELATIVE margin (50% over), not the flat +0.5 this used to carry. PEG is a MULTIPLE and
                // the ceiling's measured range is 1.25..3.0, so a fixed 0.5 meant "40% over" at the bottom
                // of that range and "17% over" at the top — one knob, two meanings, and AAPL at 2.14 vs a
                // 1.6 ceiling was filed as a gross reject for missing by 0.04. Same shape as the two other
                // relative margins here (`aum` and `liquidity` both test `>= floor * 0.5`); the pp-quantity
                // gates keep absolute margins, which is right for them and wrong for a ratio.
                fails.push(("peg", format!("PEG {peg:.2} (ceiling {:.2})", tuning.growth_max_peg), peg <= tuning.growth_max_peg * 1.5));
            }
            None if ff.and_then(|f| f.eps_ttm).is_some_and(|e| e <= 0.0) => {
                // never a near-miss: there is no PEG at all to be close with
                fails.push(("peg", "no PEG — loss-making (EPS ≤ 0)".to_string(), false));
            }
            _ => {}
        }
    }
    // (V) the twin of the `growth_require_peg` gate in `score_parts` — the two must stay the SAME
    // expression, or the ranking and the printed tail disagree about why a name is gone. Never a
    // near-miss: an absent number cannot be close to a ceiling.
    if tuning.growth_require_peg && ff.is_some_and(|f| f.eps_never_reported) {
        fails.push(("peg", "no PEG — filer tags no EPS".to_string(), false));
    }
    // (#38) the net-margin floor's reason, same 1.5-2pp near-miss convention as the CAGR legs.
    if tuning.growth_min_net_margin > 0.0 {
        if let Some(m) = ff.and_then(|f| f.net_margin).filter(|&m| m < tuning.growth_min_net_margin) {
            fails.push(("margin", format!("{m:.1}% net margin (floor {:.1}%)", tuning.growth_min_net_margin), m >= tuning.growth_min_net_margin - 2.0));
        }
    }
    // (#39) the margin-swing ceiling's reason. Quoted as the POSITIVE std the knob speaks, not the raw
    // negative field — same rule as the PEG leg above: print the number the operator actually set.
    if tuning.growth_max_margin_swing > 0.0 {
        if let Some(s) = ff.and_then(|f| f.margin_stability).filter(|&s| s < -tuning.growth_max_margin_swing) {
            let swing = -s;
            fails.push(("swing", format!("{swing:.1}pp net-margin swing (ceiling {:.1}pp)", tuning.growth_max_margin_swing), swing <= tuning.growth_max_margin_swing + 2.0));
        }
    }
    let maxdd_cap = if crypto { tuning.growth_maxdd_cap_crypto } else { tuning.growth_maxdd_cap };
    if maxdd_cap > 0.0 && quote.max_drawdown_pct > maxdd_cap {
        fails.push(("maxdd", format!("-{:.0}% worst drawdown (cap -{:.0}%)", quote.max_drawdown_pct, maxdd_cap), quote.max_drawdown_pct <= maxdd_cap + 5.0));
    }
    Some(fails)
}

/// (history_proxy hints) For subject ETFs that failed the history/young gate, say when an older fund
/// tracking the IDENTICAL BF benchmark index exists in the scanned pool — exactly the case the
/// `history_proxy` config bridges, which the user must curate by hand and can't discover from the
/// table. Suggest-only BY DESIGN: auto-applying a twin was rejected (a wrong twin silently corrupts
/// CAGR), so the user stays the curator; the hint tells them to verify the currency too (splice
/// refuses a currency mismatch at apply time anyway). Benchmark strings are lowercased at capture
/// and BF-normalized (hedged share classes carry a DIFFERENT string), so exact `==` is the match.
pub fn bridge_hint_lines(subjects: &[&Quote], pool: &[Quote], tuning: &BuyHeuristic) -> Vec<String> {
    subjects
        .iter()
        .filter_map(|q| {
            if !quote_is_etf(q) || q.history_proxied || crate::config::history_proxy().contains_key(&q.ticker) {
                return None; // not bridgeable / already bridged / already configured
            }
            let bench = q.benchmark.as_deref()?;
            let fails = gate_failures(q, tuning)?;
            if !fails.iter().any(|(g, _, _)| *g == "history" || *g == "young") {
                return None; // only a missing long record is what a twin can repair
            }
            let twin = pool
                .iter()
                .filter(|t| {
                    t.ticker != q.ticker
                        && quote_is_etf(t)
                        && t.benchmark.as_deref() == Some(bench)
                        && long_leg_fixed(t, tuning.fixed_cagr_years, tuning.growth_min_leg_years).is_some() // twin must HAVE the record
                })
                .max_by(|a, b| a.age_years.unwrap_or(0.0).total_cmp(&b.age_years.unwrap_or(0.0)))?;
            Some(format!(
                "  hint: {} tracks the same index as {} ({}, {:.0}y) — consider settings.yaml history_proxy: {}: {} (verify same currency first)",
                q.ticker, twin.ticker, bench, twin.age_years.unwrap_or(0.0), q.ticker, twin.ticker
            ))
        })
        .collect()
}

/// Per-name "TICKER  gate: why; gate: why" lines for a gate-review footer: every name in `quotes`
/// that is NOT in the ranking — one that fails a growth gate, or one that isn't assessable at all
/// (leveraged/stablecoin/missing data, named by `unranked_reason` rather than skipped, as they were
/// through round 54). Shared by `check` (the whole watchlist) and `screen` (its pinned names, which
/// bypass the score trim and print as 0.0 — this says which gate they tripped). Empty vec -> the
/// caller prints no block, so clean tables stay clean.
pub fn gate_review_lines(quotes: &[&Quote], tuning: &BuyHeuristic, ticker_w: usize) -> Vec<String> {
    quotes
        .iter()
        .filter_map(|q| {
            // (#55) refusals used to `?`-skip out of here, so a pinned name that turned leveraged or lost
            // its turnover feed printed NOTHING — the one bucket a review footer most needs to name, since
            // there is no gate to look up and no knob to loosen. `unranked_reason` gives it the structural
            // word instead. Only a name that CLEARS every gate still stays silent, so clean tables stay clean.
            let fails = gate_failures(q, tuning);
            if fails.as_ref().is_some_and(|f| f.is_empty()) {
                return None;
            }
            // (N) the third tuple field is `is_close`, which this block used to compute and DISCARD — so a
            // pinned name one notch outside a fence and one nowhere near it read identically, and the
            // actionable half of this footer was invisible. ALL failing gates must be close: a name that is
            // narrow on one and gross on another costs more than one knob to recover, which is not "narrow".
            // Pinned names never reach the near-miss tail (screen.rs skips them so the same ticker can't
            // print twice), so this line is the only place they can carry that signal.
            //
            // (round 54) hold-core names fail the growth gates FOREVER BY DESIGN (a broad index fund
            // never clears a 14%/yr momentum bar) — without this tag the same three pinned funds read
            // as unresolved warnings every single run. It WINS the slot over `narrow`: on the one cohort
            // whose gates never apply, "loosen if wanted" is advice pointing at the wrong lane.
            let tag = match fails.as_deref() {
                None => "", // structural refusal: no gate was reached, so neither tag can be true
                Some(_) if core::hold_suitable(q) => "  (hold-core H — growth gates don't apply)",
                Some(f) if f.iter().all(|(.., close)| *close) => "  (narrow — loosen if wanted)",
                Some(_) => "",
            };
            Some(format!("  {:<ticker_w$} {}{tag}", q.ticker, unranked_reason(q, tuning)))
        })
        .collect()
}

/// Per-name lines for `screen`'s EXIT-review footer: names in `prior_passing` (cleared every growth
/// gate on the PREVIOUS screen run) that fail at least one gate NOW — the transition the backtest's
/// exit probe measures as a mild forward-underperformance signal (newly-failing names lag names that
/// keep passing). A prior-passing name that turned structurally unassessable (gate_failures None:
/// unknown turnover = dead/halted listing) is exit-worthy too, so it gets a line instead of vanishing.
/// Still-passing and not-previously-passing names are skipped. Empty vec -> caller prints no block.
pub fn exit_review_lines(prior_passing: &[String], quotes: &[&Quote], tuning: &BuyHeuristic, ticker_w: usize) -> Vec<String> {
    quotes
        .iter()
        .filter(|q| prior_passing.contains(&q.ticker))
        .filter_map(|q| {
            let why = match gate_failures(q, tuning) {
                Some(fails) if fails.is_empty() => return None, // still passing
                Some(fails) => fails.iter().map(|(gate, why, _)| format!("{gate}: {why}")).collect::<Vec<_>>().join("; "),
                None => "no longer assessable (unknown turnover / structural exclusion)".to_string(),
            };
            Some(format!("  {:<ticker_w$} {}", q.ticker, why))
        })
        .collect()
}

/// Human-readable derivation of a growth SCORE: the formula then every term filled in with this quote's
/// real numbers, ending in the score itself. Lets a `screen` reader hand-verify why the #1 row ranked
/// where it did. `displayed` is the score AS SHOWN in the table (crypto rows carry a NUPL + BTC-relative
/// adjustment on top of the base formula); when it differs from the base `score`, the extra step is noted
/// so the math still reconciles to the table. `None` if the quote fails a growth gate (nothing to explain).
pub fn explain_growth_score(quote: &Quote, tuning: &BuyHeuristic, displayed: f64) -> Option<String> {
    let p = score_parts(quote, tuning)?;
    let mut s = String::new();
    let name = if quote.name.is_empty() { quote.ticker.as_str() } else { quote.name.as_str() };
    s.push_str(&format!(
        "\n─── how the #1 SCORE was computed — {name} ({}), score {displayed:.2}. Verify it yourself ───\n",
        quote.ticker
    ));
    if tuning.growth_geomean_fold {
        s.push_str("  growth_score = base × geomean(trust, overext_damp, proximity, value) + liq_bonus   (#8 fold ON)\n\n");
    } else {
        s.push_str("  growth_score = base × proximity × value × geomean(trust, overext_damp) + liq_bonus\n\n");
    }
    s.push_str("  base = trend + accel + risk + quality + dividend + fund + mom121 + smooth\n");
    // The formula string tracks the cap knob: printing `min(CAGR, cap)` while `long_trend_cap` is 0
    // (off, shipped) would advertise a clamp that is not running — the same class of lie the `cagr`
    // column told for months. Padded to keep the `=` column aligned with the rows below.
    s.push_str(&format!("    trend    = growth_trend_weight × {:<21} = {:.2} × {:.2} = {:.2}\n",
        if tuning.long_trend_cap > 0.0 { "min(CAGR, cap)" } else { "CAGR (cap off)" },
        tuning.growth_trend_weight, p.trend, p.trend_term));
    s.push_str(&format!("    accel    = growth_accel_weight × clamp(1Y−CAGR,0,cap)  = {:.2} × {:.2} = {:.2}   (1Y {:.1} − CAGR {:.1})\n",
        tuning.growth_accel_weight, p.accel, p.accel_term, p.return_1y, p.long_cagr));
    s.push_str(&format!("    risk     = Sharpe+Calmar bonus                        = {:.2}\n", p.risk_reward));
    s.push_str(&format!("    quality  = quality_weight × ROE                       = {:.2}\n", p.quality));
    let (keep, tier) = tax_keep(quote, tuning); // (D) same call dividend_reward scored on -> label can't drift
    s.push_str(&format!("    dividend = dividend_weight × min(1Y yield, cap) × keep = {:.2}   (PT tax keep {keep:.2} — {tier})\n", p.dividend));
    // (G+) `p.fund` is the SUM of the primary tilt AND every `growth_fund_extra` term, so the one-line
    // label is the whole story only while that list is EMPTY (the default, and then this is byte-identical
    // to before). With extras configured the label would otherwise name a formula that did not produce the
    // number printed next to it — break them out instead.
    s.push_str(&format!(
        "    fund     = {:<42} = {:.2}\n",
        if tuning.growth_fund_extra.is_empty() { "growth_fund_weight × clamp(fund_factor)" } else { "primary tilt + the extras below" },
        p.fund
    ));
    for t in &tuning.growth_fund_extra {
        let v = quote.fund.as_ref().and_then(|f| crate::core::select_fund_factor(f, &t.factor));
        let shown = v.map_or_else(|| "n/a".to_string(), |v| format!("{v:.1}"));
        s.push_str(&format!(
            "      + {:<16} {:.2} × {:<21} = {:.2}\n",
            t.factor,
            t.weight,
            format!("clamp({shown}, 0, {:.0})", t.cap),
            t.weight * v.unwrap_or(0.0).clamp(0.0, t.cap)
        ));
    }
    s.push_str(&format!("    mom121   = growth_mom121_weight × clamp(12-1 mom)     = {:.2}\n", p.mom121));
    s.push_str(&format!("    smooth   = growth_smoothness_weight × trend_r2 (R²)   = {:.2}\n", p.smooth));
    s.push_str(&format!("    underwtr = −growth_underwater_weight × underwater_yrs = {:.2}\n", p.underwater));
    s.push_str(&format!("    base (sum)                                            = {:.2}\n", p.base));
    s.push_str(&format!("  proximity    = 1 + growth_proximity_weight × (range−1)  = 1 + {:.2} × ({:.3}−1) = {:.3}\n",
        tuning.growth_proximity_weight, quote.range_pct / 100.0, p.proximity));
    s.push_str(&format!("  value        = 1 + growth_value_weight × (P/E factor−1) = 1 + {:.2} × ({:.2}−1) = {:.3}\n",
        tuning.growth_value_weight, p.value_raw, p.value));
    s.push_str(&format!("  trust        = history-completeness damp                = {:.3}\n", p.trust));
    if p.overext_cap > 0.0 {
        s.push_str(&format!("  overext_damp = 1 − (min(above_MA,cap)/cap)×(1−floor)    = 1 − ({:.1}/{:.0})×(1−{:.2}) = {:.3}\n",
            p.overext, p.overext_cap, tuning.growth_overext_floor, p.overext_damp));
    } else {
        s.push_str("  overext_damp = (brake off, cap 0)                       = 1.000\n");
    }
    s.push_str(&format!("  geomean(trust, overext_damp) = √({:.3} × {:.3})         = {:.3}\n", p.trust, p.overext_damp, p.damp));
    s.push_str(&format!("  liq_bonus    = growth_turnover_weight × ln(max(turn/1e9,1)) = {:.2}\n", p.liq_bonus));
    // (T) TER cost drag only prints when it bites (an ETF with a TER, drag on) — stocks/crypto are ×1.0.
    let ter_frag = if p.ter_damp < 1.0 {
        s.push_str(&format!("  ter_damp     = (1 − TER)^20                             = (1 − {:.2}%)^20 = {:.3}\n",
            quote.expense_ratio.unwrap_or(0.0), p.ter_damp));
        format!(" × {:.3}", p.ter_damp)
    } else {
        String::new()
    };
    // (#44) same treatment: print the commodity dock only when it actually bites, so a non-commodity
    // row's SCORE line is byte-identical to the pre-(#44) one.
    let commodity_frag = if p.commodity_damp < 1.0 {
        s.push_str(&format!("  commodity    = growth_commodity_damp ({}) — CAGR tracks a mean-reverting input price = {:.3}\n",
            quote.sector.as_deref().unwrap_or("commodity-named fund"), p.commodity_damp));
        format!(" × {:.3}", p.commodity_damp)
    } else {
        String::new()
    };
    // (#45) same treatment again: printed only when it bites.
    let fx_frag = if p.fx_damp < 1.0 {
        s.push_str(&format!("  fx           = growth_fx_damp ({} listing) — FX conversion + off-home spread for a EUR buyer = {:.3}\n",
            quote.quote_currency.as_deref().unwrap_or("non-EUR"), p.fx_damp));
        format!(" × {:.3}", p.fx_damp)
    } else {
        String::new()
    };
    if tuning.growth_geomean_fold {
        s.push_str(&format!("\n  SCORE = {:.2} × geomean(trust {:.3}, overext {:.3}, prox {:.3}, value {:.3}){ter_frag}{commodity_frag}{fx_frag} + {:.2} = {:.2}\n",
            p.base, p.trust, p.overext_damp, p.proximity, p.value, p.liq_bonus, p.score));
    } else {
        s.push_str(&format!("\n  SCORE = {:.2} × {:.3} × {:.3} × {:.3}{ter_frag}{commodity_frag}{fx_frag} + {:.2} = {:.2}\n",
            p.base, p.proximity, p.value, p.damp, p.liq_bonus, p.score));
    }
    if (displayed - p.score).abs() > 1e-6 {
        s.push_str(&format!("  crypto NUPL + BTC-relative adjustment: {:.2} → {displayed:.2} (the table value)\n", p.score));
    }
    s.push_str("  (BACKTEST-BLIND terms — value/TER/fund(if FMP-only) — were never in the\n   walk-forward; quality, dividends and the PT tax split ARE graded there. NOT advice.)\n");
    Some(s)
}

/// (4) Whole-market crypto sentiment FACTOR from Bitcoin NUPL (net unrealized profit/loss — already
/// fetched for the screen footer). SYMMETRIC: above `nupl_euphoria` is greed/top territory, so scale
/// crypto scores DOWN toward `nupl_damp_floor` (reached at NUPL 1.0); below `nupl_capitulation` is
/// fear/accumulation, so scale UP toward `nupl_boost_ceiling` (reached at NUPL 0, clamped for the
/// negative deep-bear readings). 1.0 (neutral) in the band between, and when NUPL is unknown.
/// Market-wide — scales the whole crypto lane uniformly: thins the tables in a frothy top, fattens
/// them after a flush. BACKTEST-BLIND (NUPL isn't in backtest_quote): the boost is a judgment lever,
/// not edge-validated — `nupl_boost_ceiling` is kept mild.
pub fn nupl_factor(nupl: Option<f64>, tuning: &BuyHeuristic) -> f64 {
    match nupl {
        Some(v) if v > tuning.nupl_euphoria && tuning.nupl_euphoria < 1.0 => {
            let over = ((v - tuning.nupl_euphoria) / (1.0 - tuning.nupl_euphoria)).clamp(0.0, 1.0);
            1.0 - over * (1.0 - tuning.nupl_damp_floor)
        }
        Some(v) if v < tuning.nupl_capitulation && tuning.nupl_capitulation > 0.0 => {
            let under = ((tuning.nupl_capitulation - v) / tuning.nupl_capitulation).clamp(0.0, 1.0);
            1.0 + under * (tuning.nupl_boost_ceiling - 1.0)
        }
        _ => 1.0,
    }
}

/// Listing venues an EU-retail broker actually serves — US/Canada + the European exchanges
/// `suffix_country` knows. Asian/AU/BR/IN listings (Hong Kong, Japan, China, South Korea, India,
/// Australia, Brazil) are off most EU retail brokers, so names listed only there are dropped.
const EU_BUYABLE_MARKETS: &[&str] = &[
    "USA", "Canada", "Germany", "UK", "France", "Netherlands", "Italy", "Spain", "Switzerland",
    "Austria", "Portugal", "Belgium", "Finland", "Sweden", "Norway", "Denmark", "Ireland",
];

/// Can an EU-retail investor actually BUY this? Filters the tables down to reachable names:
/// - **crypto** (currency-quoted): majors trade on EU-regulated exchanges -> buyable. Stablecoins /
///   corpses are already score-gated, and no free per-token EU-availability feed exists, so don't
///   over-filter. note: ceiling — a delisted alt could slip through; tighten if it ever bites.
/// - **ETF**: only funds LISTED on a European exchange. A US-domiciled ETF (SPY/QQQ/VOO) trades on a
///   US venue and has no PRIIPs KID, so EU brokers can't sell it to retail; a UCITS fund lists on
///   Xetra/LSE/Borsa Italiana (market != USA/Canada). Venue is the robust UCITS proxy — the name
///   string isn't (Yahoo gives ETF shortNames with no "UCITS" marker), so don't gate on it.
/// - **stock**: only on a venue EU retail brokers serve (`EU_BUYABLE_MARKETS`); other listings drop.
///
/// `pub` so `screen` can filter its WHOLE universe once (every table — ATH/ATL/fallers/dividends/buys),
/// not just the picks lanes.
pub fn eu_buyable(quote: &Quote) -> bool {
    if is_currency_quoted(&quote.ticker) {
        return true; // crypto major
    }
    if quote_is_etf(quote) {
        // European-listed only: US/Canada listing = US-domiciled (no KID), barred for EU retail.
        return quote.market != "USA" && quote.market != "Canada" && EU_BUYABLE_MARKETS.contains(&quote.market.as_str());
    }
    EU_BUYABLE_MARKETS.contains(&quote.market.as_str())
}

/// Score every quote with `score`, dedup currency twins, drop rows at/below `min_score`, sort
/// best-first. Shared by both lanes (on-sale `buy_score`, growth `growth_score`) and all per-class
/// tables — the lane is just which scorer + threshold the caller passes. Non-EU-buyable names
/// (US-domiciled ETFs, Asian-only listings) are filtered out up front.
fn ranked<'a>(
    quotes: &'a [Quote],
    tuning: &BuyHeuristic,
    score: impl Fn(&Quote, &BuyHeuristic) -> Option<f64>,
    min_score: f64,
    pinned: &HashSet<&str>,
) -> Vec<(&'a Quote, f64)> {
    let scored: Vec<(&Quote, f64)> =
        quotes.iter().filter(|quote| eu_buyable(quote)).filter_map(|quote| score(quote, tuning).map(|s| (quote, s))).collect();
    let mut picks = dedup_currency_twins(scored, tuning.prefer_eur); // one row per asset (BTC, not BTC-EUR+BTC-USD)
    // drop padding rows below the lane's floor, so the tables stop filling to top_picks with near-zero
    // names. (min_score 0 -> show everything > 0.)
    picks.retain(|(_, s)| *s > min_score.max(0.0));
    // best score first; ties broken by TURNOVER (most liquid first) not the incoming alphabetical order
    // — score-equal names are otherwise ordered by ticker, which buried a deep-liquid compounder (NVDA,
    // €32B) under a tiny-turnover twin (AMETEK, €244M) at the top-50 cutoff. Tie-break is edge-neutral
    // (the backtest scores are unchanged; only the arbitrary intra-tie order moves).
    // (#66) `total_cmp`, not `partial_cmp().unwrap()`: `unwrap_or` supplies a value for None and says
    // nothing about NaN, so a single NaN score or turnover took the whole screen down with it. Ordering
    // is unchanged on every finite input — the only difference is -0.0 sorting below +0.0 instead of
    // tying — which is why the goldens stay bit-identical across this swap.
    picks.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then(b.0.avg_turnover_eur.unwrap_or(0.0).total_cmp(&a.0.avg_turnover_eur.unwrap_or(0.0)))
    });
    // (B) collapse dual-class share twins (GOOG/GOOGL, BRK.A/BRK.B): same company = identical Yahoo
    // name; after the best-first sort, keep the first (higher-scoring/more-liquid) leg, drop the rest.
    // A pinned ticker is NEVER deduped away — else a pinned ETF (VUAA.DE) vanishes behind a same-named
    // higher-scored twin (VUAA.L) in the full universe. (insert() still runs so a non-pinned twin is
    // dropped whether the pinned leg came before or after it.)
    let mut seen: HashSet<&str> = HashSet::new();
    picks.retain(|(quote, _)| {
        let fresh = seen.insert(quote.name.as_str());
        pinned.contains(quote.ticker.as_str()) || fresh
    });
    picks
}

/// Upside to reclaim the OFF-HI high, from the OFF-HI drawdown: a name 46% off its high needs +85%
/// to get back there. NOT a forecast — just the room back to that high (anchor = `high_days`).
/// Clamps the asymptote near a total wipeout (-99%+ off is a corpse anyway).
fn upside_to_high(drawdown: f64) -> f64 {
    if drawdown >= 99.0 {
        return 9900.0;
    }
    drawdown * 100.0 / (100.0 - drawdown)
}

/// Compact EUR turnover for the table: €1.2B / €340M / €5K / n/a.
fn turnover_cell(o: Option<f64>) -> String {
    match o {
        Some(v) if v >= 1e9 => format!("€{:.1}B", v / 1e9),
        Some(v) if v >= 1e6 => format!("€{:.0}M", v / 1e6),
        Some(v) => format!("€{:.0}K", v / 1e3),
        None => "n/a".to_string(),
    }
}

/// One screen/picks table column: its `settings.yaml` key, header text, min width, and right-align
/// (numbers right, text left). `width 0` -> use the data-sized value from `Widths` (name/ticker/market/
/// price/score). Toggle/reorder columns via `widths.columns` (see [`active_columns`]).
struct ColSpec {
    key: &'static str,
    hdr: &'static str,
    width: usize,
    right: bool,
}

/// Every available column, canonical order. `widths.columns` picks a subset/order by key; the analytics
/// columns past the price/perf block (vol/maxdd/r2/abv-ma/pe/roe/div) are OFF unless listed. All are
/// DISPLAY-ONLY — derived from already-fetched `Quote` fields, they never touch a score.
const COLUMNS: &[ColSpec] = &[
    ColSpec { key: "rank", hdr: "RANK", width: 7, right: false }, // (#44) 6 -> 7: a 7th rank flag ("10*#!c~Ho" is possible) needs the room
    ColSpec { key: "name", hdr: "NAME", width: 0, right: false },
    ColSpec { key: "ticker", hdr: "TICKER", width: 0, right: false },
    ColSpec { key: "market", hdr: "MARKET", width: 0, right: false },
    ColSpec { key: "price", hdr: "PRICE(EUR)", width: 0, right: true },
    ColSpec { key: "cagr", hdr: "CAGR", width: 8, right: true }, // whole-life %/yr since listing (display; ranking uses the fixed-horizon ladder — see `leg`)
    ColSpec { key: "leg", hdr: "LEG", width: 8, right: true }, // the CAPPED long-leg %/yr the growth rank actually scores on
    ColSpec { key: "trcagr", hdr: "TR-CAGR", width: 8, right: true }, // whole-life %/yr WITH the dividend sum added (lower-bound total return; ≈ CAGR for Acc/non-payers)
    ColSpec { key: "1h", hdr: "1H", width: 7, right: true },
    ColSpec { key: "6h", hdr: "6H", width: 7, right: true },
    ColSpec { key: "12h", hdr: "12H", width: 7, right: true },
    ColSpec { key: "1d", hdr: "1D", width: 8, right: true },
    ColSpec { key: "1w", hdr: "1W", width: 8, right: true },
    ColSpec { key: "1m", hdr: "1M", width: 8, right: true },
    ColSpec { key: "1y", hdr: "1Y", width: 8, right: true },
    ColSpec { key: "2y", hdr: "2Y", width: 8, right: true },
    ColSpec { key: "5y", hdr: "5Y", width: 8, right: true },
    ColSpec { key: "8y", hdr: "8Y", width: 8, right: true },
    ColSpec { key: "10y", hdr: "10Y", width: 8, right: true },
    ColSpec { key: "20y", hdr: "20Y", width: 8, right: true },
    ColSpec { key: "yrs", hdr: "YRS", width: 4, right: true },       // real listing age in years — how much record backs the CAGR headline
    ColSpec { key: "vol", hdr: "VOL", width: 7, right: true },       // daily-return stdev (risk)
    ColSpec { key: "maxdd", hdr: "MAXDD", width: 8, right: true },   // worst peak-to-trough drop ever (pain)
    ColSpec { key: "r2", hdr: "R2", width: 6, right: true },         // log-trend steadiness 0..1 (smoothness)
    ColSpec { key: "abv-ma", hdr: "ABV-MA", width: 8, right: true }, // % above the 200wk SMA (overextension)
    ColSpec { key: "pe", hdr: "P/E", width: 7, right: true },        // trailing P/E (FMP key only)
    ColSpec { key: "peg", hdr: "PEG", width: 6, right: true },       // (#37) 100/peg_yield — THE PEG: what growth_max_peg cuts on. Annual EPS ÷ the score's CAGR, so it won't exactly equal the TTM-based P/E cell ÷ CAGR
    ColSpec { key: "mvrv", hdr: "MVRV", width: 6, right: true },     // (#45) CRYPTO's valuation cell — market cap / realized cap, what `crypto_max_mvrv` cuts on. NOT a PEG and deliberately not in that column: MVRV has no earnings term (realized cap values each coin at the price it last moved), so it is a P/B analogue. <1 = the market sits below its own aggregate cost basis
    ColSpec { key: "roe", hdr: "ROE/A", width: 7, right: true },     // trailing return on equity — or on ASSETS where equity is not a credible denominator, negative or collapsed (`core::quality_return`), hence the slash: one column, two denominators, no per-row flag
    ColSpec { key: "div", hdr: "DIV", width: 7, right: true },       // trailing-1Y dividend yield
    ColSpec { key: "ter", hdr: "TER", width: 6, right: true },       // ETF annual expense ratio % — the one cost that compounds against a decades hold (FMP key, ETFs only)
    ColSpec { key: "aum", hdr: "AUM", width: 6, right: true },       // ETF fund size (BF etp_search, EUR-approximate) — sub-scale funds get liquidated/merged mid-hold
    ColSpec { key: "use", hdr: "USE", width: 4, right: false },      // ETF share class: Acc(umulating)/Dist(ributing) — Dist pays out (taxed yearly); Acc compounds tax-deferred
    ColSpec { key: "repl", hdr: "REPL", width: 4, right: false },    // ETF replication: Swap/Full/Opt(imised)/Hybr(id)/Samp(le) — counterparty structure over a decades hold
    ColSpec { key: "dom", hdr: "DOM", width: 4, right: false },      // ETF legal domicile (ISIN prefix): IE gets the 15% US-dividend withholding treaty, LU eats 30% — ≈ +0.2%/yr on a US/world fund over a decades hold
    ColSpec { key: "rev-yoy", hdr: "REV-YoY", width: 8, right: true }, // newest complete-FY revenue growth vs prior FY (stocks only; report pipeline) — "still growing?"
    ColSpec { key: "eps-yoy", hdr: "EPS-YoY", width: 8, right: true }, // newest complete-FY EPS growth vs prior FY (stocks only) — profit follow-through
    ColSpec { key: "net", hdr: "NET%", width: 6, right: true },      // newest complete-FY net margin level (stocks only) — profitability quality
    ColSpec { key: "buyback", hdr: "BUYBK", width: 8, right: true }, // newest complete-FY net share-count change, sign-flipped (stocks only): + = buying back (tax-deferred capital return), − = diluting
    ColSpec { key: "off-hi", hdr: "OFF-HI", width: 7, right: true },
    ColSpec { key: "upside", hdr: "UPSIDE", width: 8, right: true },
    ColSpec { key: "turnover", hdr: "TURNOVER", width: 10, right: true },
    ColSpec { key: "score", hdr: "SCORE", width: 0, right: true },
    ColSpec { key: "score8y", hdr: "S-8Y", width: 6, right: true }, // DIAGNOSTIC: the same score with the long-CAGR window (and the trust leg) pinned to 8Y — "how does this name look on an 8-year view?". Never ranked on, and scored WITHOUT the 8Y CAGR admission floor so every row carries a comparable number
];

/// Canonical default layout when `widths.columns` is empty: the historical table PLUS `cagr` and
/// `maxdd` (return + worst-pain — what a 20yr buy-and-hold screen was missing). Users add vol/r2/pe/roe/
/// div/abv-ma by listing them in `widths.columns`.
const DEFAULT_COLUMNS: &[&str] = &[
    "rank", "name", "ticker", "market", "price", "cagr", "leg", "yrs", "1h", "6h", "12h", "1d", "1w", "1m", "1y",
    "2y", "5y", "8y", "10y", "20y", "maxdd", "off-hi", "upside", "turnover", "score", "score8y",
];

/// Resolve `widths.columns` (config) to the ordered `ColSpec`s to print. Empty config -> `DEFAULT_COLUMNS`;
/// otherwise the listed keys in order. Unknown keys are skipped (a typo drops that column, never panics).
fn active_columns(cfg: &[String]) -> Vec<&'static ColSpec> {
    let keys: Vec<&str> = if cfg.is_empty() { DEFAULT_COLUMNS.to_vec() } else { cfg.iter().map(String::as_str).collect() };
    keys.iter().filter_map(|k| COLUMNS.iter().find(|c| c.key.eq_ignore_ascii_case(k))).collect()
}

/// Does the configured layout print an intraday column?
///
/// `screen` pays a WHOLE EXTRA Yahoo chart request per name (`range=5d&interval=60m`) to fill
/// `Quote::intraday`, and these three cells are the only thing in the codebase that reads it —
/// nothing scores or gates on it. Every outbound request is spaced `1/fetch_requests_per_second`
/// apart by the global pacer, so on a 3847-name universe that leg is ~3847 requests ≈ 65s of pure
/// sleep, ~24% of the run, for two or three display cells. Hide them and stop paying for them.
///
/// Routed through [`active_columns`] rather than scanning `cfg` directly so the empty-means-DEFAULT
/// rule and the case-insensitive key match stay defined in exactly one place: `DEFAULT_COLUMNS` DOES
/// carry 1h/6h/12h, so an unconfigured layout still fetches, and only an explicit list that omits all
/// three turns the fetch off.
pub fn wants_intraday(cfg: &[String]) -> bool {
    active_columns(cfg).iter().any(|c| matches!(c.key, "1h" | "6h" | "12h"))
}

/// (round 12) Master column keys ABSENT from an explicit `widths.columns` config, master order,
/// case-insensitive (same matching as `active_columns`). Screen nags on these once per run: a
/// hand-maintained columns list silently hides every column added after it was written (dom was
/// invisible for months this way). Empty config = built-in DEFAULT layout, curated — never nags.
pub fn missing_columns(cfg: &[String]) -> Vec<&'static str> {
    if cfg.is_empty() {
        return Vec::new();
    }
    COLUMNS
        .iter()
        .filter(|c| !cfg.iter().any(|k| c.key.eq_ignore_ascii_case(k)))
        .map(|c| c.key)
        .collect()
}

/// Pad/truncate one cell to `width`, right- or left-aligned.
fn fmt_cell(s: &str, width: usize, right: bool) -> String {
    let t = truncate(s, width);
    if right {
        format!("{t:>width$}")
    } else {
        format!("{t:<width$}")
    }
}

/// The effective width of a column: its fixed `ColSpec.width`, or the data-sized `Widths` value when 0.
fn col_width(spec: &ColSpec, w: &Widths) -> usize {
    // explicit settings.yaml override wins for any column; EVERY path is floored at the header so a
    // too-tight width setting can clip data but never the header itself (price: 9 printed "PRICE(EUR").
    let base = if let Some(&n) = w.column_widths.get(spec.key) {
        n
    } else {
        match (spec.width, spec.key) {
            (0, "name") => w.name,
            (0, "ticker") => w.ticker,
            (0, "market") => w.market,
            (0, "price") => w.price,
            (0, "score") => w.score,
            (fixed, _) => fixed,
        }
    };
    base.max(spec.hdr.chars().count()) // never narrower than the header
}

/// Strip ONE trailing corporate legal suffix for display, so a tight NAME column truncates to a whole
/// name ("Monolithic Power Systems") instead of a dangling fragment ("Monolithic Power Systems, In").
/// Longest-first so ", Inc." wins over " Inc". ETF/crypto names end in "ETF"/"Acc"/"Dist"/coin names and
/// pass through untouched. Display-only — TER name matching and every lookup keep the full name.
fn display_name(name: &str) -> String {
    const SUFFIXES: [&str; 16] = [
        ", Incorporated", " Incorporated", " Corporation", ", Inc.", ", Inc", " Inc.", " Inc",
        ", Corp.", " Corp.", " Corp", " Limited", ", Ltd.", " Ltd.", ", LLC", " PLC", " plc",
    ];
    let n = name.trim_end();
    for s in SUFFIXES {
        if let Some(stem) = n.strip_suffix(s) {
            let stem = stem.trim_end_matches([',', ' ']);
            if stem.chars().count() >= 4 {
                return stem.to_string(); // guard: never strip a name down to almost nothing
            }
        }
    }
    n.to_string()
}

/// (#43) Drop the "UCITS ETF" boilerplate token IN PLACE, keeping whatever follows. "UCITS" is in 100%
/// of the 2463 cached ETF names and "ETF" in 96% — it carries no information, and it sits MID-string, so
/// a tight NAME column spent its last 10 chars printing the noise instead of the fund. NOT a tail cut:
/// what follows the token is the SHARE CLASS ("1C USD Hedged", "(Acc)"), and dropping that makes 52% of
/// funds share one display name (Amundi Core S&P 500 Swap alone has 8 classes). Currency hedging appears
/// nowhere else in the table — USE/REPL show only Acc/Dist — so the tail stays. Display-only.
fn drop_ucits(n: &str) -> String {
    let Some(i) = n.find(" UCITS") else { return n.to_string() };
    let rest = &n[i + " UCITS".len()..];
    let rest = rest.strip_prefix(" ETF").unwrap_or(rest);
    let out = format!("{}{}", &n[..i], rest).trim().to_string();
    if out.chars().count() >= 4 { out } else { n.to_string() } // same guard display_name uses
}

/// Display name for any quote — the raw Yahoo name minus per-class noise. Crypto: drop the quote-currency
/// word ("Bitcoin EUR" -> "Bitcoin"; the ticker column already carries -EUR/-USD). ETF: drop the
/// umbrella-company prefix ("iShares VII PLC - iShares NASDAQ 100 UCITS ETF" -> the fund part) — only
/// when what follows " - " is the fund name (has ETF/UCITS), so share-class tails like "- USD Acc" never
/// match — THEN the "UCITS ETF" boilerplate token (`drop_ucits`), which that prefix strip leaves in the
/// middle of the result. Everything else: strip one corporate legal suffix. Used by the picks table AND `check`'s
/// summary line; lookups (BF TER name match) keep the full name.
pub fn clean_name(quote: &Quote) -> String {
    let n = display_name(&quote.name);
    let is_crypto = is_currency_quoted(&quote.ticker) || quote.instrument_type.eq_ignore_ascii_case("CRYPTOCURRENCY");
    if is_crypto {
        n.strip_suffix(" USD").or_else(|| n.strip_suffix(" EUR")).map(str::to_string).unwrap_or(n)
    } else if quote.instrument_type.eq_ignore_ascii_case("ETF") {
        let fund = match n.split_once(" - ") {
            Some((_, fund)) if fund.contains("ETF") || fund.contains("UCITS") => fund.to_string(),
            _ => n,
        };
        drop_ucits(&fund) // (#43) then the boilerplate token, which the prefix strip leaves behind
    } else {
        n
    }
}

/// Render ONE cell's text for column `key`. `mark` is the rank label (number + `*`/`#` flags), `alt` the
/// 8Y-pinned score for the `score8y` column (`None` = gated/unscoreable). All values come from
/// already-fetched `Quote` fields — pure formatting, no scoring. Unknown key -> "?".
/// `tuning` is read by the `leg` column alone (which rung, which CAGR flavour, which cap) so that cell
/// can print the score's own number instead of re-deriving one; nothing here scores.
fn col_cell(key: &str, quote: &Quote, score: f64, alt: Option<f64>, mark: &str, tuning: &BuyHeuristic, fund_pe: &FundPeMap) -> String {
    // ≥1000% drops the decimal so a +26522% 20Y cell still fits its 8-char column instead of overflowing.
    let pct1 = |o: Option<f64>| {
        o.map_or("n/a".to_string(), |v| if v.abs() >= 1000.0 { format!("{v:+.0}%") } else { format!("{v:+.1}%") })
    };
    // asset class -> which fundamental columns even APPLY. "—" = not applicable to this class (an equity
    // has no expense ratio; an ETF/crypto has no P/E/ROE); "n/a" stays reserved for applies-but-no-data.
    // Unknown class ("" instrument_type) falls through to the value so a real name is never wrongly blanked.
    let is_crypto = is_currency_quoted(&quote.ticker) || quote.instrument_type.eq_ignore_ascii_case("CRYPTOCURRENCY");
    let is_etf = quote.instrument_type.eq_ignore_ascii_case("ETF");
    let is_equity = quote.instrument_type.eq_ignore_ascii_case("EQUITY");
    let stock_only_na = is_etf || is_crypto; // P/E, ROE don't apply here (PEG now splits equity/fund — see below)
    let etf_only_na = is_equity || is_crypto; // TER doesn't apply here
    let crypto_only_na = is_equity || is_etf; // MVRV doesn't apply here — realized cap is an on-chain quantity
    match key {
        "rank" => mark.to_string(),
        "name" => clean_name(quote),
        "ticker" => quote.ticker.clone(),
        "market" => quote.market.clone(),
        "price" => quote.price.clone(),
        // whole-life endpoint CAGR over the FULL (monthly-backfilled) history — the honest "what did
        // this compound at since listing" headline. NOT what the growth rank scores (that's `leg`
        // below); in scoring `life_cagr` appears only as a NEGATIVE guard, the value-trap dock when it
        // is <= 0. So this cell can differ from the CAGR a gate message quotes — by design, and the
        // two now sit side by side rather than one silently standing in for the other.
        "cagr" => quote.life_cagr.map_or("n/a".to_string(), |v| format!("{v:+.0}%")),
        // proven long-term CAGR (%/yr) from the ranked leg — the annualized trend the ranking actually
        // rewards, shown so a reader sees "+14%/yr" and not just a +1344% cumulative blob. This is
        // EXACTLY what `trend_term` multiplies (`--explain`: "trend = growth_trend_weight × CAGR").
        // Goes through `capped_trend`, so it tracks `long_trend_cap` whatever it is set to — 0 (shipped)
        // prints the raw leg CAGR, a positive cap prints the clamped one. The cell must never re-derive
        // this: matching the arithmetic is the only reason the column exists.
        "leg" => long_leg_fixed(quote, tuning.fixed_cagr_years, tuning.growth_min_leg_years).map_or("n/a".to_string(), |(c, y)| {
            format!("{:+.0}%", capped_trend(long_cagr_from(quote, tuning, c, y), tuning))
        }),
        "trcagr" => quote.tr_cagr.map_or("n/a".to_string(), |v| format!("{v:+.0}%")),
        // real listing age in years. NOT the span of the ranked leg — though it determines it, since
        // the ladder is 20Y -> 8Y -> 5Y on availability: ≥20y ranks on 20Y, ≥8y on 8Y, else 5Y. A
        // "+16%/yr over 20" and a "+16%/yr over 5" are NOT the same conviction, so pairing this with
        // `leg` makes the record length behind the headline number visible per row.
        // ONE decimal, not zero: rounding to nearest printed "8" for a 7.7y record while the 8Y leg —
        // which needs 7.91y — sat blank in the same row, so the two columns read as contradicting.
        "yrs" => quote.age_years.map_or("n/a".to_string(), |y| format!("{y:.1}")),
        "1h" => pct1(quote.intraday[0]),
        "6h" => pct1(quote.intraday[1]),
        "12h" => pct1(quote.intraday[2]),
        "1d" | "1w" | "1m" | "1y" | "2y" | "5y" | "8y" | "10y" | "20y" => {
            let label = key.to_uppercase();
            match perf_pct(quote, &label) {
                Some(p) => pct1(Some(p)),
                // `≈` PREFIX, not suffix: it qualifies the number before it is read. Still fits the
                // 8-wide column at the worst case (`≈+26522%`) because pct1 drops the decimal ≥1000%.
                None => perf_fill(quote, &label, tuning).map_or("n/a".to_string(), |p| format!("≈{}", pct1(Some(p)))),
            }
        }
        "vol" => quote.volatility_pct.map_or("n/a".to_string(), |v| format!("{v:.1}%")),
        "maxdd" => {
            if quote.max_drawdown_pct > 0.0 {
                format!("-{:.0}%", quote.max_drawdown_pct)
            } else {
                "n/a".to_string()
            }
        }
        "r2" => format!("{:.2}", quote.trend_r2),
        "abv-ma" => {
            if quote.above_ma_pct > 0.0 {
                format!("+{:.0}%", quote.above_ma_pct)
            } else {
                "0%".to_string()
            }
        }
        "pe" if stock_only_na => "—".to_string(),
        "pe" => quote.pe_ratio.map_or("n/a".to_string(), |v| format!("{v:.1}")),
        // (#37) THE one PEG: the number `growth_max_peg` cuts on and `growth_fund_factor: peg_yield`
        // tilts on, printed verbatim as its reciprocal (`peg_yield` 100 <=> PEG 1). <1 = cheap for its
        // growth, >2 = pricey.
        //
        // This cell used to compute `pe_ratio / long_cagr_from(..)` — a SECOND PEG, which is how the
        // tool came to cut APH at 2.02 in the same run it ranked ODFL printing 2.51. That formula was
        // also arithmetic you can do on the two columns beside it (P/E ÷ CAGR), so it spent a column
        // hiding the only PEG the tool actually acts on.
        //
        // CAVEAT worth knowing before dividing the row by hand: this uses the ANNUAL 10-K EPS, the only
        // basis the as-of backtest can reconstruct (`fetch.rs` fences the TTM roll to `earnings_yield`
        // for exactly that reason), while the `P/E` cell prefers the fresher TTM roll. So `P/E ÷ CAGR`
        // is CLOSE to this but not equal, and is furthest off for mid-ramp growers. Two columns, two
        // jobs — the fix is not to make them agree, it is to keep P/E true.
        //
        // `n/a` here is not missing data. It is the gate saying it cannot price this name — loss-maker,
        // non-positive growth, or no long leg — and `gate_failures` names that case in words.
        "peg" if is_crypto => "—".to_string(),
        // (#37 funds) the ETF leg of the same cell. A fund has no per-share EPS, so its PEG is built
        // from the look-through equity-book P/E (`parse_fund_pe`) over the SAME `long_cagr_pct` the
        // equity arm below divides by. Routed through `fund_peg_yield` — the identical fn `lane_split`
        // trims on — so the printed number and the acted-on number are one value, not two that agree
        // today. That is the whole lesson of the APH/ODFL bug described above, applied to funds.
        // `n/a` = this fund served no P/E and no index twin resolved; it is never trimmed then.
        //
        // A trailing `~` means the P/E was BORROWED from a physical fund tracking the same index,
        // because this one is swap-based and has no equity book of its own (see `FundPe`). It is an
        // inference, and it can cut this fund from the table, so it must never read like a measurement.
        // `fund_pe_line` names the source ticker.
        //
        // (fund staleness) A trailing `°` means the P/E was SERVED FROM CACHE rather than fetched this
        // run. Same rule as `~` one paragraph up, for age instead of provenance: the value still feeds
        // the trim, so it must not read like something measured today. Both marks can appear together —
        // a borrowed value inherits its source's age.
        "peg" if is_etf => fund_peg_yield(quote, tuning, fund_pe).map_or("n/a".to_string(), |p| {
            let fp = fund_pe.get(&quote.ticker);
            let borrowed = fp.is_some_and(|f| f.from.is_some());
            let cached = fp.is_some_and(|f| f.as_of.is_some());
            format!("{:.2}{}{}", 100.0 / p, if borrowed { "~" } else { "" }, if cached { "°" } else { "" })
        }),
        "peg" => quote
            .fund
            .as_ref()
            .and_then(|f| f.peg_yield)
            .map_or("n/a".to_string(), |p| format!("{:.2}", 100.0 / p)),
        // (#45) MVRV — market cap / realized cap, the coin's price against what its holders actually
        // paid. This is what `crypto_max_mvrv` cuts on, printed verbatim. It is NOT a PEG and must not
        // be folded into that column: there is no earnings term, so it reads as a P/B, not a P/E ÷ g.
        // Same quantity as the `Bitcoin NUPL` footer, per coin instead of market-wide (NUPL = 1 - 1/MVRV).
        // `n/a` = no on-chain data for this coin (most of the top 100) — it passes the ceiling free.
        "mvrv" if crypto_only_na => "—".to_string(),
        "mvrv" => quote.mvrv.map_or("n/a".to_string(), |v| format!("{v:.2}")),
        "roe" if stock_only_na => "—".to_string(),
        // (#42) The `|ROE| > 100 -> n/m` rule that used to live here is GONE, not relaxed: it read the
        // ROE LEVEL, and level does not separate "earned a lot on its assets" from "bought its equity
        // down to nothing". It hid AAPL (+152% on a 4.9x multiplier, the same leverage as ITW's printed
        // +95%) while printing BA's +41% on 31x. `quality_return` now swaps the real artifacts to ROA
        // upstream on the MULTIPLIER, so a number reaching this cell is one that survived that check —
        // and hiding a figure the score is already acting on was the inconsistency, not the cure.
        // NEGATIVE-equity filers likewise never reach here: HCA prints its real +9%, not a fake -113%.
        "roe" => quote.roe.map_or("n/a".to_string(), |v| format!("{v:+.0}%")),
        // Read the Option `dividend_yields` already carries rather than `dividend_yield_1y`, whose
        // `unwrap_or(0.0)` collapses two different facts into one: Some(0.0) = pays NOTHING (MNST, and
        // it's a real, knowable 0.00%), None = no price or too little history to say. Printing both as
        // "n/a" made a confident zero look like missing data. 2 decimals so a token yield (NVDA ~0.02%)
        // prints instead of rounding to 0.0%. The SCORE keeps `unwrap_or(0.0)` — `dividend_reward`
        // genuinely wants unknown to score as zero — so this is display-only, no re-validation.
        "div" => core::dividend_yields(&quote.div_eur, quote.price_eur)
            .first()
            .and_then(|o| *o)
            // A yield that ROUNDS to zero is a non-payer and must print as one. The raw value comes off
            // an FX-scaled sum and can land a hair below zero, which formats as "-0.00%" — every one of
            // the 24 non-payers on the table printed that. Clamp the DISPLAY, not the number: the score
            // reads `dividend_yield_1y` on its own path and is untouched.
            .map_or("n/a".to_string(), |d| format!("{:.2}%", if d.abs() < 0.005 { 0.0 } else { d })),
        // ETF expense ratio (%/yr). Low is good — a 0.07% index ETF vs a 0.50% active fund is ~18% more
        // wealth over 40y. "—" for stocks/crypto (no expense ratio); "n/a" for an ETF FMP didn't cover.
        "ter" if etf_only_na => "—".to_string(),
        "ter" => quote.ter_shown().map_or("n/a".to_string(), |v| format!("{v:.2}%")),
        // ETF fund size (EUR-approximate). Small funds get liquidated/merged — a forced taxable exit
        // mid-hold. "—" for stocks/crypto (not funds); "n/a" for an ETF BF's payload didn't cover.
        "aum" if etf_only_na => "—".to_string(),
        "aum" => turnover_cell(quote.aum_shown()),
        // ETF share class + replication tokens (BF keyData). Display-only — the price-only CAGR already
        // prices the Dist payout drag, so these inform the BUY (which listing), never the ranking.
        "use" if etf_only_na => "—".to_string(),
        "use" => quote.use_of_profits.map_or("n/a".to_string(), str::to_string),
        "repl" if etf_only_na => "—".to_string(),
        "repl" => quote.replication.map_or("n/a".to_string(), str::to_string),
        "dom" if etf_only_na => "—".to_string(),
        "dom" => quote.domicile.clone().unwrap_or_else(|| "n/a".to_string()),
        // newest complete-FY income-statement snapshot (report pipeline; enriched only for displayed
        // stock rows). "—" for ETF/crypto (no income statement); "n/a" = not enriched / no data.
        "rev-yoy" if stock_only_na => "—".to_string(),
        "rev-yoy" => quote.rev_yoy.map_or("n/a".to_string(), |v| format!("{v:+.1}%")),
        "eps-yoy" if stock_only_na => "—".to_string(),
        "eps-yoy" => quote.eps_yoy.map_or("n/a".to_string(), |v| format!("{v:+.1}%")),
        "net" if stock_only_na => "—".to_string(),
        "net" => quote.net_margin_fy.map_or("n/a".to_string(), |v| format!("{v:.1}")),
        "buyback" if stock_only_na => "—".to_string(),
        "buyback" => quote.buyback_yoy.map_or("n/a".to_string(), |v| format!("{v:+.1}%")),
        "off-hi" => format!("-{:.1}%", quote.drawdown_pct),
        "upside" => format!("+{:.1}%", upside_to_high(quote.drawdown_pct)),
        "turnover" => turnover_cell(quote.avg_turnover_eur),
        "score" => format!("{score:.1}"),
        // the 8Y-pinned twin of SCORE (`alt`), computed by the caller — crypto included: BTC-EUR carries
        // a real 8Y leg (+657% ≈ 28%/yr), so blanking the class would hide a number that exists. "n/a"
        // survives only for a name with no rankable leg at all, which a printed row can't be.
        // `†` = the pin did NOT apply: no 8Y leg, so `long_leg_fixed` fell back to the longest one and
        // this is the full-history score wearing an 8Y label. Same test the fallback itself uses.
        "score8y" => alt.map_or("n/a".to_string(), |v| format!("{v:.1}{}", short_8y_mark(quote))),
        _ => "?".to_string(),
    }
}

/// (round 110) Ticker-base normalizers for the owned-position overlay. Trading212 encodes a listing
/// as `BASE_MARKET_EQ` (`AAPL_US_EQ`), Yahoo as `BASE.EXCHANGE` (`IITU.L`); comparing the lowercased
/// bases joins the two worlds. Honest caveat: a same-base listing on another exchange also matches —
/// acceptable for a display flag (same strategy, different venue), never fed into a score.
pub fn t212_base(ticker: &str) -> String {
    ticker.split('_').next().unwrap_or(ticker).to_lowercase()
}
pub fn yahoo_base(ticker: &str) -> String {
    ticker.split('.').next().unwrap_or(ticker).to_lowercase()
}

/// (round 111) What the user already holds, split by class so a Binance asset (`SOL`) can never
/// flag the same-lettered stock. Stocks/ETFs = lowercased Yahoo bases (Trading212); crypto =
/// lowercased asset names (Binance), matched against `underlying()` of the currency-quoted ticker.
#[derive(Default)]
pub struct Owned {
    pub stocks: HashSet<String>,
    pub crypto: HashSet<String>,
}

impl Owned {
    fn holds(&self, ticker: &str) -> bool {
        if is_currency_quoted(ticker) {
            self.crypto.contains(&underlying(ticker).to_lowercase())
        } else {
            self.stocks.contains(&yahoo_base(ticker))
        }
    }
}

/// Print one Top-`n` buy-candidate table (a single asset-class subset of the ranked picks). Columns +
/// order come from `widths.columns` via [`active_columns`] (default = [`DEFAULT_COLUMNS`]).
#[allow(clippy::too_many_arguments)]
fn print_picks(title: &str, picks: &[(&Quote, f64)], n: usize, w: &Widths, pinned: &HashSet<&str>, owned: &Owned, hide: &[&str], tuning: &BuyHeuristic, fund_pe: &FundPeMap) {
    println!("\n{title}");
    if picks.is_empty() {
        println!("  (none pass the gates)");
        return;
    }
    // `hide`: column keys to drop for THIS table — a class never has these fundamentals (P/E/PEG/ROE
    // don't exist for ETFs or crypto), so they'd just print "—" every row. Dropped, not blanked.
    let mut cols = active_columns(&w.columns);
    cols.retain(|c| !hide.contains(&c.key));
    let header = cols.iter().map(|c| fmt_cell(c.hdr, col_width(c, w), c.right)).collect::<Vec<_>>().join(" ");
    println!("  {header}");
    // S-8Y: the same heuristic with the CAGR window (and, via `trust_factor`, the required record)
    // pinned to 8 years — a second READ on each row, never a ranking input. Built once per table and
    // only when the column is on, so a table without it pays nothing.
    //
    // The CAGR floor is neutralized so EVERY printed row gets a number. Pinning changes exactly one
    // input, `long_cagr`, and `growth_min_cagr` is the only gate that reads it (the others test range,
    // age, AUM, 1Y/1M, drawdown, above-MA — none of which the pin touches, and every printed row already
    // cleared them live). So dropping this one floor is the whole of "score it anyway": without it a
    // strong 20-year name whose 8-year window compounds under 14%/yr — XDJE.DE at 10.9%/yr — printed a
    // bare "n/a" that read as missing data instead of the low score it actually earns on 8 years.
    // NOTE: the floor is a GATE, never a term (grep says `min_cagr` is only ever compared, never summed),
    // so removing it changes no arithmetic — S-8Y is the same score, judged without the 8Y admission bar.
    let tuning8 = cols.iter().any(|c| c.key == "score8y").then(|| BuyHeuristic {
        fixed_cagr_years: 8,
        growth_min_cagr: f64::NEG_INFINITY,
        growth_min_cagr_crypto: f64::NEG_INFINITY,
        ..tuning.clone()
    });
    // one printed row; `mark` is the rank label (number + "*" pinned / "#" fundamentals flags). Flags on
    // the rank cell, not the name, so name truncation can't eat them.
    let row = |mark: &str, quote: &Quote, score: f64| {
        let alt = tuning8.as_ref().and_then(|t| growth_score(&as_8y_window(quote), t));
        let line = cols
            .iter()
            .map(|c| fmt_cell(&col_cell(c.key, quote, score, alt, mark, tuning, fund_pe), col_width(c, w), c.right))
            .collect::<Vec<_>>()
            .join(" ");
        println!("  {line}");
    };
    let star = |quote: &Quote| if pinned.contains(quote.ticker.as_str()) { "*" } else { "" }; // * = a pinned (watchlist) name
    // # = the score used LIVE fundamentals (trailing P/E, ROE, and/or the as-of fund_factor when the
    // growth_fund tilt is on), not price-only — only equities with an FMP key populate these, so on the
    // wide screen it flags the few enriched rows (the pins).
    let enriched =
        |quote: &Quote| if quote.pe_ratio.is_some() || quote.roe.is_some() || quote.expense_ratio.is_some() || quote.fund_factor.is_some() { "#" } else { "" };
    // ! = LATE-CYCLE: the overextension brake is FLOORED for this row (price >= growth_overext_cap %
    // above its 200wk SMA — e.g. WDC at +486% vs cap 100). The score is already maximally docked, but
    // past the cap the column can't dock MORE, so a 5x-above-trend name prints like a 1x one without
    // this flag. Display-only: read it as "rank earned on a cycle blow-off, conviction is the SCORE".
    let braked = |quote: &Quote| {
        let cap = if is_currency_quoted(&quote.ticker) { tuning.growth_overext_cap_crypto } else { tuning.growth_overext_cap };
        if cap > 0.0 && quote.above_ma_pct >= cap { "!" } else { "" }
    };
    // ~ = the score ran on BRIDGED history (config `history_proxy`: a young listing spliced onto its
    // configured older same-strategy twin). CAGR/YRS describe the strategy's record, not this listing's.
    let bridged = |quote: &Quote| if quote.history_proxied { "~" } else { "" };
    // H = a buy-and-hold-20yr CORE (broad + cheap + physical + Acc + large + UCITS), flagged
    // INDEPENDENTLY of the momentum score — the broad index funds it marks are floored to 0.0 by the
    // late-cycle brake, so without this the table reads them as the WORST rows. Display-only.
    let holdable = |quote: &Quote| if core::hold_suitable(quote) { "H" } else { "" };
    // o = ALREADY HELD at the broker (round 110): the screen ranks candidates but can't otherwise
    // see your portfolio, so a top row you already own reads "covered", not "buy more". Display-only.
    let held = |quote: &Quote| if owned.holds(&quote.ticker) { "o" } else { "" };
    // c = COMMODITY-LINKED (GICS Energy/Materials, or a fund named for a commodity): its earnings are a
    // spread on a traded input price, so the long CAGR is a spot-price snapshot rather than compounding.
    // R² cannot isolate this — it measures the SYMPTOM, and on the live screen Amazon's 0.79 sat BETWEEN
    // the two commodity names (CF 0.76, MPC 0.68) — so the flag names the CAUSE. Printed whether or not
    // growth_commodity_damp is set: the dock is optional, knowing what the row is never is.
    let commodity = |quote: &Quote| if is_commodity(quote) { "c" } else { "" };
    // x = non-EUR-quoted ETF line (GBp/USD/SEK…): a EUR buyer pays broker FX conversion + the
    // off-home spread the EUR twin of the same fund doesn't. Printed whether or not growth_fx_damp
    // is set — same rule as `c`: the dock is optional, knowing what the row is never is.
    let fx_listed = |quote: &Quote| if is_noneur_etf(quote) { "x" } else { "" };
    let mark = |quote: &Quote, i: usize| {
        format!("{}{}{}{}{}{}{}{}{}", i + 1, star(quote), enriched(quote), braked(quote), commodity(quote), fx_listed(quote), bridged(quote), holdable(quote), held(quote))
    };
    // pinned tickers that ranked BELOW the cut still print (with their real rank + "*") so you can
    // compare a holding against the tops above even when it doesn't make the top-N.
    let below_cut = picks.iter().enumerate().skip(n).filter(|(_, (quote, _))| pinned.contains(quote.ticker.as_str()));
    let mut seen = String::new(); // rank-flag chars that actually printed, drives the legend line
    for (i, (quote, score)) in picks.iter().enumerate().take(n).chain(below_cut) {
        let m = mark(quote, i);
        for flag in ['*', '#', '!', 'c', 'x', '~', 'H', 'o'] {
            if m.contains(flag) && !seen.contains(flag) {
                seen.push(flag);
            }
        }
        // `†` rides the S-8Y cell, not the rank cell, so it can't come from `m` — collect it here.
        if tuning8.is_some() && !short_8y_mark(quote).is_empty() && !seen.contains('†') {
            seen.push('†');
        }
        // `≈` rides a perf cell for the same reason — and only over the columns THIS table prints, so
        // a layout without the long rungs never explains a mark it didn't show.
        if !seen.contains('≈') && cols.iter().any(|c| perf_fill(quote, &c.key.to_uppercase(), tuning).is_some()) {
            seen.push('≈');
        }
        // `°` rides the ETF `peg` cell, same collection story as the two above. Gated on the column
        // actually printing AND on the row having a PEG to show: a fund whose cached P/E yields no PEG
        // prints `n/a`, which carries no mark and must not pull a legend entry in behind it.
        if !seen.contains('°')
            && cols.iter().any(|c| c.key == "peg")
            && fund_pe.get(&quote.ticker).is_some_and(|f| f.as_of.is_some())
            && fund_peg_yield(quote, tuning, fund_pe).is_some()
        {
            seen.push('°');
        }
        row(&m, quote, *score);
    }
    // Legend: explain only the flags THIS table used, so clean tables stay clean.
    let mut legend: Vec<String> = [
        ("*", "pinned watchlist name"),
        ("#", "score used live fundamentals, not price-only"),
        ("!", "late-cycle: price >= cap above 200wk trend, brake floored — conviction is the SCORE, not the rank"),
        ("c", "commodity-linked (GICS Energy/Materials, or a commodity-named fund) — earnings are a spread on a traded input price, so the CAGR is a spot-price snapshot, not compounding; scaled by growth_commodity_damp when set"),
        ("x", "non-EUR-quoted ETF line — a EUR buyer pays FX conversion + off-home spread vs the EUR twin; scaled by growth_fx_damp when set"),
        ("~", "history bridged from configured older twin (history_proxy) — CAGR/YRS describe the strategy, not this listing"),
        ("H", "hold-suitable: broad + cheap + physical + accumulating + large — a buy-and-hold-20yr core, independent of the momentum rank"),
        ("o", "already held (broker portfolio)"),
        ("†", "under 8y of record — its S-8Y is the full-history score, not an 8-year one"),
    ]
    .iter()
    .filter(|(flag, _)| seen.contains(flag))
    .map(|(flag, what)| format!("{flag} = {what}"))
    .collect();
    // Carries the configured coverage, so it can't be a static entry in the table above.
    if seen.contains('°') {
        legend.push(
            "° = look-through P/E served from cache, not fetched this run (Yahoo unreachable) — the fund P/E line dates each one; anything older than 3 days is dropped rather than shown"
                .into(),
        );
    }
    if seen.contains('≈') {
        legend.push(format!(
            "≈ = record covers ≥{:.0}% of that horizon but not all of it — the cell is the whole-life MEASURED return under a longer label (not a projection), and is never scored",
            tuning.perf_fill_coverage_pct
        ));
    }
    // Column note, not a flag: one cell can hold either ratio and there is no per-row marker to hang it
    // on, so the table says so once. Only when the column is actually printed (ETF/crypto tables hide it).
    if cols.iter().any(|c| c.key == "roe") {
        legend.push("ROE/A = return on equity, or on ASSETS where equity is negative or under 1/20th of assets (buyback-shrunk filers: HCA, CL)".into());
    }
    if !legend.is_empty() {
        println!("  ({})", legend.join("; "));
    }
}

/// Print ONE lane's ranked picks SPLIT per asset class (stocks / [tech stocks] / ETFs / crypto) so a
/// +9400% crypto can't crowd out equities and a basket fund isn't ranked head-to-head with a single
/// company — the best in EACH class surfaces. Class: currency-quoted ticker (`-USD`/`-EUR`) → crypto,
/// else fund name (ETF/UCITS) → ETF, else stock. Currency twins already deduped in `ranked`.
/// `kind` names the lane in each title ("buy candidates" / "growth candidates").
/// Split a ranked lane into its three printable tables and apply every DISPLAY trim (score floors,
/// ETF sector filter, redundancy skip). Split out of `print_lane` so the trims are assertable: they
/// are the only knobs in the tool whose effect used to exist purely as printed text.
fn lane_split<'a>(picks: Vec<(&'a Quote, f64)>, n: usize, sectors: &[String], tuning: &BuyHeuristic, pinned: &HashSet<&str>, fund_pe: &FundPeMap) -> (Vec<(&'a Quote, f64)>, Vec<(&'a Quote, f64)>, Vec<(&'a Quote, f64)>) {
    let min_score = tuning.growth_min_score;
    // ETFs get their OWN, lower floor: 4 of the 7 score terms (accel/quality/liq/fund) are ~0 for a
    // diversified basket, so ETF scores structurally cap ~5.6 vs stocks ~19 — the shared growth_min_score
    // sits at ~89% of the ETF ceiling and shows only a sliver. Trim the ETF lane proportional to ITS
    // distribution instead. DISPLAY-only (never touches the ranked/backtest edge).
    let etf_min_score = tuning.growth_min_score_etf;
    let (crypto, equity): (Vec<_>, Vec<_>) =
        picks.into_iter().partition(|(quote, _)| is_currency_quoted(&quote.ticker));
    let (etf, stock): (Vec<_>, Vec<_>) = equity.into_iter().partition(|(quote, _)| quote_is_etf(quote));
    // Equities: apply the score trim HERE (the input list was ranked with no trim so the crypto lane
    // below can stay full). ETFs carry no GICS sector, so the sector filter matches the configured
    // keywords against the fund NAME; stocks were already sector-filtered before fetch. Pinned tickers
    // bypass BOTH the score trim and the sector filter (`|| pinned`) — they're always shown.
    let keep = |quote: &Quote, s: f64, floor: f64, sector_ok: bool| (s > floor && sector_ok) || pinned.contains(quote.ticker.as_str());
    let stock: Vec<_> = stock.into_iter().filter(|(quote, s)| keep(quote, *s, min_score, true)).collect();
    // (#41) REDUNDANCY skip, stocks only — the ETF lane is diversified by construction and crypto is one
    // bet already. Runs HERE, after the score/sector trim and before the table is cut to `n`, so the rows
    // it drops are replaced from below instead of leaving a short table. Every row here passed every gate;
    // what this removes is the SECOND COPY of a bet, which is why it is not a gate and not a score term.
    // Pinned tickers bypass it, exactly as they bypass the score trim — a pin means "always show me this".
    let stock: Vec<_> = if tuning.growth_corr_cap > 0.0 {
        let trails: Vec<&[f64]> = stock.iter().map(|(q, _)| q.trail_monthly.as_slice()).collect();
        let kept = core::decorrelate_keep(&trails, n, tuning.growth_corr_cap);
        stock
            .iter()
            .enumerate()
            .filter(|(i, (q, _))| kept.contains(i) || pinned.contains(q.ticker.as_str()))
            .map(|(_, row)| *row)
            .collect()
    } else {
        stock
    };
    // (#75) VALUE BRAKE, stocks only, and deliberately the NEXT thing after the redundancy skip: both are
    // cohort-aware post-rank trims, both sit after the score/sector cut and before the table is cut to
    // `n` so what they drop refills from below. Cut the dearest-for-their-growth `growth_value_floor_pct`%
    // by `fund.peg_yield` — a CROSS-SECTIONAL floor over this table's own rows, not an absolute PEG,
    // because `growth_max_peg` already owns the absolute ceiling and the question here is "dearest OF THE
    // SURVIVORS". Names with no peg_yield are KEPT: unjudgeable is not a verdict, the same house rule the
    // gate above follows, and what makes this equity-only for free. Pinned tickers bypass it like every
    // other display trim. `core::pct_floor` is shared with the graded twin in `report_vs_benchmark` so
    // the served brake and the measured brake cannot drift — the (#3j) lesson, applied at the seam.
    // NOTE the two cohorts are not identical and cannot be: this one is THIS TABLE's post-trim stock
    // rows, the backtest's is every gated non-crypto sample in a ~6mo bucket. Same rule, same floor
    // arithmetic, different populations — which is why the grid grades the backtest and this site only
    // inherits the verdict.
    let peg_of = |quote: &Quote| quote.fund.as_ref().and_then(|f| f.peg_yield);
    let stock: Vec<_> = match core::pct_floor(
        stock.iter().filter_map(|(quote, _)| peg_of(quote)).collect(),
        tuning.growth_value_floor_pct,
    ) {
        Some(floor) => stock
            .into_iter()
            .filter(|(quote, _)| {
                peg_of(quote).is_none_or(|v| v >= floor) || pinned.contains(quote.ticker.as_str())
            })
            .collect(),
        None => stock, // knob off, or nobody in this table carries a PEG -> nothing to rank against
    };
    let etf: Vec<_> = etf
        .into_iter()
        .filter(|(quote, s)| keep(quote, *s, etf_min_score, core::sector_matches(&quote.name, sectors)))
        .collect();
    // (#37 funds) the PEG CEILING finally reaches ETFs. It cannot live in `growth_score` beside the
    // equity leg, and the asymmetry is forced by data, not taste: a fund's look-through P/E arrives only
    // from `yahoo_top_holdings`, called for the ranked picks plus a refill bench (~50 symbols against a
    // ~4300-fund universe), so a universe-wide fund gate would mean thousands of crumbed quoteSummary
    // calls — the reason that fetch documents itself as display-only. Trimming HERE — after
    // the score/sector cut, before the table is cut to `n` — buys the same ceiling with the rows below
    // refilling the gap, exactly like the (#41) redundancy skip above. Its OWN number, not the equity
    // `growth_max_peg`: the shared 1.6 was tried live 2026-08-02 and cut every fund that reports the
    // datum (all-world 3.29, S&P 500 2.57, US tech 1.78, semis 1.69), leaving a one-row table whose
    // only survivor was the fund with NO P/E — selection by absent data, the inversion this prevents.
    // A diversified basket compounds ~7%/yr against a ~23 book P/E; it cannot clear an equity bar.
    // Funds with no P/E in the payload are never trimmed (missing data is not a verdict — the rule
    // `fund_pe_line` already follows), and pinned tickers bypass this like every other display trim.
    // NARROWED 2026-08-02: "no P/E" no longer means "swap-based". A synthetic ETF holds a total-return
    // swap and reports a literal 0.0 book, so it used to ride the free pass PERMANENTLY — XLKS.L sat at
    // rank #2 untested in the same run this trim cut seven funds at PEG 2.57-3.80, which is the very
    // "selection by absent data" the paragraph above says the trim exists to prevent, reappearing one
    // level down. Those funds now borrow a physical index twin's P/E (`FundPe.from`) and are trimmed on
    // it like anyone else. The free pass survives only where NOTHING can be resolved — outside the
    // bench, or a fund with no twin, e.g. JEDG.L, physical and simply absent from Yahoo's ratio feed.
    // NOTE this is a gate, NOT the `growth_fund_weight` tilt: that still reads `fund.peg_yield`, which
    // is None for every fund, so the tilt stays equity-scoped. Do not "fix" that by attaching a
    // synthetic FundFacts here — it would switch the tilt on for funds as a plumbing side effect.
    let etf: Vec<_> = if tuning.growth_max_peg_etf > 0.0 {
        let bar = 100.0 / tuning.growth_max_peg_etf; // PEG 2.0 -> reject peg_yield < 50
        etf.into_iter()
            .filter(|(q, _)| {
                pinned.contains(q.ticker.as_str())
                    || fund_peg_yield(q, tuning, fund_pe).is_none_or(|p| p >= bar)
            })
            .collect()
    } else {
        etf
    };
    (stock, etf, crypto)
}

#[allow(clippy::too_many_arguments)]
fn print_lane(picks: Vec<(&Quote, f64)>, n: usize, w: &Widths, kind: &str, desc: &str, sectors: &[String], sector_of: &HashMap<String, String>, tuning: &BuyHeuristic, pinned: &HashSet<&str>, owned: &Owned, fund_pe: &FundPeMap) {
    let (stock, etf, crypto) = lane_split(picks, n, sectors, tuning, pinned, fund_pe);
    // Title carries the selected sector filter so the table says what it's showing ("all" = no filter).
    // Count shown = how many actually qualified (capped at n); "of {n} max" explains a short table —
    // it's not a quota, that's all that passed the gates + filter.
    let secs = if sectors.is_empty() { "all".to_string() } else { sectors.join(", ") };
    let head = |len: usize| if len >= n { format!("Top {n}") } else { format!("Top {len} of {n} max") };
    // P/E, ROE, REV-YoY, EPS-YoY, NET% are equity-only; TER/AUM/USE/REPL are ETF-only; MVRV is
    // crypto-only. PEG spans equity AND funds now — one header, two constructions (per-share EPS vs
    // look-through book P/E), both over the same CAGR. Hide the always-"—" columns per class: stocks
    // drop the fund + coin columns, ETFs drop the equity fundamentals but KEEP their PEG, crypto drops
    // every equity/fund column and keeps MVRV.
    // the ranking explainer prints ONCE here — repeating the same ~340-char paragraph in all three
    // table titles (incl. the crypto sentence over the stocks table) tripled the noise.
    println!("\n{kind} — {desc}");
    print_picks(&format!("{} stocks [sectors: {secs}]:", head(stock.len())), &stock, n, w, pinned, owned, &["ter", "aum", "use", "repl", "mvrv"], tuning, fund_pe);
    // (#27) cluster concentration: a top-20 stock table is usually ~3 correlated trades, not 20
    // independent bets — count the SHOWN rows per GICS sector so "semis-heavy" is a number, not a
    // vibe. Display-only; empty map (`check`, explicit-args screen) skips the sector line. Names
    // outside the constituent CSVs (Lisbon pond, watchlist pins) count under "other".
    // (#28) same for LISTING MARKET (currency/country exposure): 20/20 USA = one FX bet on the dollar.
    let mix_line = |label: &str, counts: HashMap<&str, usize>, hint: &str| {
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mix = counts.iter().map(|(k, c)| format!("{k} {c}")).collect::<Vec<_>>().join(", ");
        println!("  ({label}: {mix} — {hint})");
    };
    let shown: Vec<&Quote> = stock.iter().take(n).map(|(quote, _)| *quote).collect();
    if !shown.is_empty() {
        if !sector_of.is_empty() {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for quote in &shown {
                *counts.entry(sector_of.get(&quote.ticker).map_or("other", String::as_str)).or_insert(0) += 1;
            }
            mix_line("sector mix", counts, "names in one sector move together; treat each sector as ONE bet when sizing");
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for quote in &shown {
            *counts.entry(quote.market.as_str()).or_insert(0) += 1;
        }
        mix_line("market mix", counts, "listing country ~ currency exposure; all-USA = one bet on the dollar too");
    }
    // (#43) ETF names run ~51 chars at the median against a stock table's ~15, so the ETF lane gets its
    // OWN NAME width. Inserted into `column_widths` rather than set on `w.name` because `col_width` reads
    // that map FIRST — writing `name` would lose to any explicit `column_widths: { name: N }` the user
    // already has. The three lane tables already print different column SETS (`hide` differs per call),
    // so a differing NAME width costs no alignment that ever existed.
    let etf_w = if w.name_etf > 0 {
        let mut w2 = w.clone();
        w2.column_widths.insert("name".into(), w.name_etf);
        Cow::Owned(w2)
    } else {
        Cow::Borrowed(w)
    };
    // `peg` is NOT hidden here any more (#37 funds): the ETF lane is trimmed on a real fund PEG, and a
    // table that acts on a number while refusing to print it is how the ETF ceiling's first cull looked
    // like an unexplained one-row table. `pe` stays hidden — a fund's book P/E is a look-through
    // aggregate, not the row's own multiple, and it already has the footer line.
    print_picks(&format!("{} ETFs [sectors: {secs}]:", head(etf.len())), &etf, n, &etf_w, pinned, owned, &["pe", "roe", "rev-yoy", "eps-yoy", "net", "buyback", "mvrv"], tuning, fund_pe);
    // Crypto: NOT min_score-trimmed — show ALL potential growers ranked vs Bitcoin (the base), so BTC
    // itself stays visible even when the overext brake docks its score. Capped at n by print_picks.
    print_picks(&format!("{} crypto (ranked vs Bitcoin, the base):", head(crypto.len())), &crypto, n, w, pinned, owned, &["pe", "peg", "roe", "rev-yoy", "eps-yoy", "net", "ter", "aum", "use", "repl", "div", "buyback", "dom"], tuning, fund_pe);
}

/// Tilt a crypto growth score by its 1Y return RELATIVE to Bitcoin (the crypto market's base). `edge`
/// = the coin's year minus BTC's, as a fraction; the score scales by (1 + w·edge), bounded 0.5x..2x so
/// one moonshot can't run away and a laggard is docked, not zeroed. BTC vs itself = edge 0 = 1.0x (the
/// neutral anchor every other coin is read against). Unknown BTC-or-coin 1Y, or w=0 -> unchanged.
fn btc_relative(coin_1y: Option<f64>, btc_1y: Option<f64>, score: f64, w: f64) -> f64 {
    match (coin_1y, btc_1y) {
        (Some(c), Some(b)) if w > 0.0 => score * (1.0 + w * (c - b) / 100.0).clamp(0.5, 2.0),
        _ => score,
    }
}

/// (Item 17) The crypto-only score adjustments `screen`/`check` apply at render time: scale by the
/// whole-market NUPL factor (`cfactor`) then tilt vs Bitcoin's year. Equities/ETFs pass through
/// unchanged. Shared so `size` ranks crypto identically to the tables it came from (not on the raw
/// `growth_score`). Caller precomputes `cfactor` (`nupl_factor`) + `btc_1y` once — both are O(universe),
/// don't recompute per call.
pub fn crypto_adjust(quote: &Quote, base: f64, tuning: &BuyHeuristic, cfactor: f64, btc_1y: Option<f64>) -> f64 {
    if !is_currency_quoted(&quote.ticker) {
        return base; // equities/ETFs: no crypto-market damp, no BTC base
    }
    btc_relative(perf_pct(quote, "1Y"), btc_1y, base * cfactor, tuning.growth_btc_outperf_weight)
}

/// CORE-shortlist domicile ordering: IE first — the 15% US-dividend withholding treaty vs LU's 30%
/// ≈ +0.2%/yr on a US/world equity fund over a decades hold, outweighing the single-digit-bp TER
/// deltas ranked after it. Unknown last: missing data is never rewarded.
fn dom_rank(q: &Quote) -> u8 {
    match q.domicile.as_deref() {
        Some("IE") => 0,
        Some(_) => 1,
        None => 2,
    }
}

/// Buy-and-hold CORE shortlist — the one-fund-forever holds the momentum SCORE buries at 0.0 (the
/// overext brake floors a broad index fund that has simply run for years). Built straight from the full
/// universe, bypassing `growth_score` entirely: keep every `hold_suitable` fund (broad + cheap +
/// physical + accumulating + large + UCITS), collapse currency/listing twins by name, and rank by what
/// a decades hold actually cares about — broadest diversification first (one fund = the whole world),
/// then cheapest TER, then largest AUM (least closure risk). Pure display; touches no score, no gate,
/// no backtest. Scoped to the wide `screen` universe (empty on the small `check` watchlist anyway).
/// (round 55) The CORE shortlist selection, shared by the printed block and the screen-state
/// membership diff: EU-buyable hold-suitable funds, one row per fund name (best venue kept),
/// breadth-major sort (all-world -> World -> S&P 500; IE domicile, cheapest TER, largest AUM
/// within a tier), capped at 3 per tier so no index family crowds out the others.
pub fn hold_core_list(quotes: &[Quote]) -> Vec<&Quote> {
    let mut cores: Vec<&Quote> = quotes.iter().filter(|q| eu_buyable(q) && core::hold_suitable(q)).collect();
    cores.sort_by(|a, b| {
        core::hold_breadth_tier(&a.name)
            .cmp(&core::hold_breadth_tier(&b.name))
            .then(dom_rank(a).cmp(&dom_rank(b)))
            .then(a.ter_shown().unwrap_or(9.9).total_cmp(&b.ter_shown().unwrap_or(9.9)))
            .then(b.aum_shown().unwrap_or(0.0).total_cmp(&a.aum_shown().unwrap_or(0.0)))
    });
    let mut seen: HashSet<&str> = HashSet::new();
    cores.retain(|q| seen.insert(q.name.as_str())); // one row per fund (VUAA.DE vs VUAA.L), best-ranked kept
    // cap each breadth tier so every index family shows — else the many MSCI World trackers crowd
    // out the S&P 500 and all-world cores. Sort is breadth-major, so a per-tier counter suffices.
    // Sized from core::HOLD_TIERS, NOT a literal: this was `[0u8; 3]` indexed by tier, so adding a
    // fourth tier panicked on index-out-of-bounds instead of merely mis-printing.
    // (#66) …and the ELEMENT type was the same bug a second time, on the same line. The counter is
    // incremented once per distinct fund NAME in a tier and was a `u8`, so the 256th name in one tier
    // wrapped it to 0 and the tier silently admitted three more rows. The two profiles disagree about
    // what that means, which is why no test could have caught it: `[profile.mutants]` inherits `dev`
    // and has overflow-checks ON, so `cargo t` would have panicked — while `[profile.release]`, the
    // binary that actually runs the daily screen, sets only opt-level and lto, leaving them OFF to wrap
    // in silence. A `usize` counter cannot reach either outcome and costs nothing.
    let mut per_tier = [0usize; core::HOLD_TIERS];
    cores.retain(|q| {
        let t = core::hold_breadth_tier(&q.name) as usize;
        per_tier[t] += 1;
        per_tier[t] <= HOLD_PER_TIER
    });
    cores
}

/// Rows kept per breadth tier in the CORE list — enough to show an alternative issuer/venue per
/// sleeve without one index family crowding out the others.
pub const HOLD_PER_TIER: usize = 3;

fn print_hold_core(quotes: &[Quote], n: usize, pinned: &HashSet<&str>, owned: &Owned) {
    let cores = hold_core_list(quotes);
    if cores.is_empty() {
        return;
    }
    let per_tier0 = cores.iter().filter(|q| core::hold_breadth_tier(&q.name) == 0).count();
    println!(
        "\nbuy-and-hold CORE — broad geographic sleeves (the momentum ranking buries these at 0.0; \
         ranked broadest-first: all-world → developed → emerging → US → ex-US → Europe → Japan/Asia-Pac, \
         then domicile (IE first, withholding) → cheapest TER → largest AUM. One all-world fund IS a \
         whole book; the sleeves below it are for building one yourself. NOT advice):"
    );
    // (round 118) supply per sleeve, BEFORE the cap. Without it the block is unreadable: three rows
    // in a sleeve means "three shown of possibly forty", and an absent sleeve is indistinguishable
    // from one the universe genuinely cannot fill (Japan/Asia-Pac in a EU-buyable pond). The old
    // hardcoded 3-tier cap silently truncated for exactly this reason and nothing said so.
    let mut supply = [0usize; core::HOLD_TIERS];
    for q in quotes.iter().filter(|q| eu_buyable(q) && core::hold_suitable(q)) {
        supply[core::hold_breadth_tier(&q.name) as usize] += 1;
    }
    const SLEEVE: [&str; core::HOLD_TIERS] =
        ["all-world", "developed", "emerging", "US", "ex-US", "Europe", "Japan/AsiaPac"];
    let counts: Vec<String> = SLEEVE
        .iter()
        .zip(supply.iter())
        .map(|(name, n)| format!("{name} {n}"))
        .collect();
    println!("  supply per sleeve (before the ≤{HOLD_PER_TIER}/sleeve cap): {}", counts.join(" · "));
    // (round 111) leading 1-char cell = the owned-position marker; this list is what a 20yr holder
    // actually buys, so "covered" matters most here. Blank when the overlay is off/empty.
    println!("  {:<1} {:<44} {:<9} {:<9} {:>5} {:>4} {:>6} {:>7} {:<4} {:<4} {:<4}", "", "NAME", "TICKER", "MARKET", "CAGR", "YRS", "TER", "AUM", "USE", "REPL", "DOM");
    let mut any_owned = false;
    for q in cores.iter().take(n) {
        let cagr = q.life_cagr.map_or("n/a".to_string(), |v| format!("{v:+.0}%"));
        let yrs = q.age_years.map_or("—".to_string(), |a| format!("{a:.1}")); // 1 decimal, as in the screen table
        let ter = q.ter_shown().map_or("n/a".to_string(), |t| format!("{t:.2}%"));
        let own = if owned.holds(&q.ticker) { "o" } else { "" };
        any_owned |= !own.is_empty();
        println!(
            "  {:<1} {:<44} {:<9} {:<9} {:>5} {:>4} {:>6} {:>7} {:<4} {:<4} {:<4}",
            own,
            truncate(&q.name, 44),
            truncate(&q.ticker, 9),
            truncate(&q.market, 9),
            cagr,
            yrs,
            ter,
            turnover_cell(q.aum_shown()),
            q.use_of_profits.unwrap_or("—"),
            q.replication.unwrap_or("—"),
            q.domicile.as_deref().unwrap_or("n/a"),
        );
    }
    if any_owned {
        println!("  (o = already held (broker portfolio))");
    }
    // (round 49) tier-0 hole made visible: the venue lists carry all-world funds, but without facts
    // (TER/AUM) none can qualify — the documented ceiling was silent until now.
    if per_tier0 == 0 {
        println!("  (no all-world/ACWI fund with facts qualified — pin one, e.g. VWCE.DE, to surface it)");
    }
    // (round 49) pinned near-H: a watchlist fund that IS a broad core candidate but misses the H flag —
    // say the first failing leg (single source of truth: core::hold_miss_reason == !hold_suitable),
    // same posture as the gate-review footer. Bounded by watchlist size.
    let near: Vec<&Quote> = quotes.iter()
        .filter(|q| pinned.contains(q.ticker.as_str()) && quote_is_etf(q) && core::is_broad_index_name(&q.name))
        .collect();
    for q in near {
        if let Some(reason) = core::hold_miss_reason(q) {
            println!("  {:<9} not hold-core: {reason}", truncate(&q.ticker, 9));
        }
    }
}

/// Print the Top-N GROWTH picks split per asset class (stocks / ETFs / crypto). The growth lane —
/// proven compounders at/near their own ~10y high still climbing — is the ONLY lane with a validated
/// forward edge for a 20yr+ buy-and-hold (walk-forward rho +0.26, top-vs-bottom-half +108 pts). The
/// old on-sale "buy the dip" lane was dropped: its walk-forward edge is NEGATIVE (-72 pts), i.e.
/// deepest-dip ranking picks future LOSERS over a multi-decade hold. `nupl` (Bitcoin
/// net-unrealized-P/L, the screen footer's market-greed gauge; `None` on `check` or fetch fail) damps
/// the crypto rows when the market is euphoric.
/// Returns (score-math walkthrough for the caller to print last, this run's ranked top-`n`
/// tickers — round 68: the screen diffs the latter against its previous run's state).
/// The per-run display context for `render`: the peripheral inputs (market sentiment, sector filter,
/// pinned/owned overlays, --explain target, core-shortlist toggle) bundled so the hot path stays
/// `render(quotes, n, tuning, w, ctx)`. Named fields also kill the call-site bool blindness the old
/// trailing `show_hold_core` flag had — `RenderCtx { show_hold_core: true, .. }` reads itself.
pub struct RenderCtx<'a> {
    pub nupl: Option<f64>,               // Bitcoin NUPL sentiment gauge; damps crypto rows (None on check/fetch-fail)
    pub sectors: &'a [String],           // ETF sector filter (stocks are pre-filtered before fetch)
    pub sector_of: &'a HashMap<String, String>,
    pub pinned: &'a [String],            // watchlist tickers shown even when gated
    pub owned: &'a Owned,                // broker-held positions -> `o` overlay
    pub explain: Option<&'a str>,        // --explain TICKER (None = explain the #1 row)
    pub show_hold_core: bool,            // print the buy-and-hold CORE shortlist (screen hunts, not check)
    pub fund_pe: &'a FundPeMap, // (#37 funds) look-through equity-book P/E per fund ticker,
    // already un-inverted by `parse_fund_pe`. Feeds the ETF PEG trim in `lane_split` and the printed
    // cell. EMPTY is the honest default (`check`, offline tests): no P/E anywhere -> nothing trimmed.
}

pub fn render(quotes: &[Quote], n: usize, tuning: &BuyHeuristic, w: &Widths, ctx: RenderCtx) -> (Option<String>, Vec<String>) {
    // Pinned tickers (config `pinned`): always shown in their class table for comparison, even if they
    // fail the growth gate or the sector/score cut. Still subject to eu_buyable (don't show unbuyable).
    let pinned_set: HashSet<&str> = ctx.pinned.iter().map(String::as_str).collect();
    // (4) market-sentiment factor, applied to crypto rows only (it's a whole-crypto-market gauge):
    // <1 in euphoria, >1 in capitulation, 1.0 in the neutral band / unknown.
    let cfactor = nupl_factor(ctx.nupl, tuning);
    // Bitcoin = the crypto market's base: tilt each alt by its 1Y return RELATIVE to BTC, so the looser
    // crypto gate surfaces more coins without flooding the table with names that merely lag the base.
    let btc_1y = quotes.iter().find(|quote| quote.ticker.starts_with("BTC-")).and_then(|quote| perf_pct(quote, "1Y"));
    let crypto_adj = |quote: &Quote, s: f64| crypto_adjust(quote, s, tuning, cfactor, btc_1y); // (Item 17) shared with `size`
    // a gated pinned name returns None from growth_score; give it a tiny sentinel score so it survives
    // ranked's `>0` trim and reaches print_lane (where pinned is exempt from the score/sector cut). Skip
    // err/no-data quotes (a bad symbol like a suffix-less ETF) — nothing to compare, don't show a blank row.
    let growth_scorer = |quote: &Quote, tuning: &BuyHeuristic| {
        growth_score(quote, tuning).map(|s| crypto_adj(quote, s)).or_else(|| {
            let usable = quote.price != "err" && quote.price != "no data";
            (usable && pinned_set.contains(quote.ticker.as_str())).then_some(f64::MIN_POSITIVE)
        })
    };

    let growth = "20yr+ growth ranking: at/near its own ~10y high (OFF-HI ≈ 0) with a strong proven \
                  long-term CAGR and an accelerating recent year, braked by how far it's run above its \
                  200wk trend — quality pricey *because* it keeps winning. Crypto ranked vs Bitcoin (the \
                  market base). For a literal 20-year buy-and-hold the anchor is a broad, cheap, accumulating \
                  index core (the `H`-marked hold-suitable names) — these ranked tilts are higher-variance, \
                  regime-dependent sector/thematic bets whose recent leadership won't necessarily repeat. \
                  NOT advice.";
    // rank with NO trim (0.0): print_lane trims equities by growth_min_score but keeps the crypto lane
    // full (all growers up to Bitcoin). Gates inside growth_score still exclude non-growers.
    let picks = ranked(quotes, tuning, growth_scorer, 0.0, &pinned_set);
    // (Item 8) churn warning: compare this run's top-N against the last. Separate cache for the wide
    // `screen` universe vs the small `check`/watch set (keyed by size) so their overlaps don't mix.
    let cache = crate::config::data_path(if quotes.len() > 200 { ".folioman_turnover_screen.txt" } else { ".folioman_turnover_watch.txt" });
    let tickers: Vec<String> = picks.iter().map(|(q, _)| q.ticker.clone()).collect();
    if let Some(note) = turnover_note(&tickers, n, &cache) {
        println!("{note}");
    }
    // worked example: derive a row's SCORE term-by-term so a reader can hand-verify the ranking. Default
    // is the #1 (highest-scoring) row; `--explain TICKER` targets that ticker instead. Captured before
    // print_lane consumes `picks`. crypto_adj is folded into the displayed score.
    let target = match ctx.explain {
        Some(t) => picks.iter().find(|(q, _)| q.ticker.eq_ignore_ascii_case(t)),
        None => picks.first(),
    };
    let explain_text = target.and_then(|&(q, s)| explain_growth_score(q, tuning, s));
    print_lane(picks, n, w, "growth candidates", growth, ctx.sectors, ctx.sector_of, tuning, &pinned_set, ctx.owned, ctx.fund_pe);
    // buy-and-hold CORE shortlist: momentum floors broad index funds at 0.0, so surface the
    // one-fund-forever holds re-sorted by hold-suitability (breadth → domicile → TER → AUM) — the
    // right order for a 20yr hold, which the momentum table inverts. Caller-gated (display-only):
    // every `screen` lane that carries cores (the wide run OR `screen etfs`), never `check`. Empty
    // cores early-return inside, so stock/crypto-only screen filters stay silent.
    if ctx.show_hold_core {
        // n is DERIVED, never a second magic number: `hold_core_list` already caps 3 per tier, so a
        // literal here silently truncated the lower tiers the moment tiers were added — which is the
        // exact failure this list exists to avoid.
        print_hold_core(quotes, core::HOLD_TIERS * HOLD_PER_TIER, &pinned_set, ctx.owned);
    }
    // gate review: pinned names are shown in their table even when a gate rejects them (score 0.0).
    // Say WHICH gate, so a 0.0 next to strong metrics isn't mistaken for a bug (VVSM stretch, VUAA/
    // SPYL young, …). Same footer `check` prints, scoped here to the pinned rows that bypassed the cut.
    let pinned_q: Vec<&Quote> = quotes.iter().filter(|q| pinned_set.contains(q.ticker.as_str())).collect();
    let review = gate_review_lines(&pinned_q, tuning, w.ticker);
    if !review.is_empty() {
        println!("\ngate review — why these pinned names scored 0.0 (review, not auto-sell):");
        for line in &review {
            println!("{line}");
        }
    }
    // history_proxy discovery: a young pinned ETF whose exact benchmark index an older scanned fund
    // already tracks is one settings.yaml line away from a real long-term CAGR — say so here, where
    // the "young/history" gate line above just explained the 0.0. Suggest-only; user curates.
    for h in &bridge_hint_lines(&pinned_q, quotes, tuning) {
        println!("{h}");
    }
    // returned, not printed: the caller places the score-math walkthrough AFTER the actionable
    // footers (gate review / exit review / fact drift / near-miss) so the alerts aren't buried
    // under 20 lines of arithmetic.
    let text = match (explain_text, ctx.explain) {
        (Some(text), _) => Some(text),
        // An explicit --explain TICKER that didn't land a ranked row. This used to print ONE string
        // naming three causes and picking none ("fails a growth gate, isn't EU-buyable, or wasn't
        // scanned"), which is the same non-answer whether the name was gated, out-scored, or never
        // fetched. `gate_failures` already knows which — it was simply never asked on this path, so a
        // name failing 2+ gates (invisible in the near-miss tail, which needs EXACTLY one) had no
        // explanation anywhere in the tool. Four distinct verdicts now, formatted by the same
        // `gate_review_lines` the pinned gate-review footer uses so the wording can't drift.
        (None, Some(t)) if !t.is_empty() => Some(match quotes.iter().find(|q| q.ticker.eq_ignore_ascii_case(t)) {
            None => format!("\n--explain: {t} wasn't scanned — not in the universe, or filtered out as not EU-buyable."),
            Some(q) => match gate_failures(q, tuning) {
                None => format!(
                    "\n--explain: {t} isn't assessable as a growth candidate (leveraged / stablecoin / physical-commodity ETC, or unknown turnover)."
                ),
                // NOT "out-scored by the top n": `target` is looked up in the untrimmed `picks`, so a
                // ranked-but-below-the-cut name still gets the score walkthrough above. Reaching here
                // means it left `picks` entirely — the lane floor or a twin dedup.
                Some(f) if f.is_empty() => format!(
                    "\n--explain: {t} clears every growth gate but isn't ranked — its score fell to/below the lane floor (0.0), or a better-scoring twin (dual-class / currency listing) took its row."
                ),
                Some(f) => format!(
                    "\n--explain: {t} is scanned but fails {} growth gate{}:\n{}",
                    f.len(),
                    if f.len() == 1 { "" } else { "s" },
                    gate_review_lines(&[q], tuning, w.ticker).join("\n")
                ),
            },
        }),
        _ => None,
    };
    // (round 68) the same top-n slice turnover_note just measured, handed to the caller so the
    // screen can DIFF membership by name against its previous run (the note only says how many moved).
    (text, tickers.into_iter().take(n).collect())
}

/// Suggested basket weights (%, summing to 100) for an already-scored list: weight ∝ score ÷
/// volatility, so two near-equal-score names don't get equal money when one swings twice as hard.
/// Vol-target, NOT Kelly — Kelly needs a forward return distribution we don't have and overbets
/// noise. A missing or near-zero vol is floored at `MIN_VOL` so a no-history name can't grab the
/// whole basket; a non-positive score contributes 0. Empty in -> empty out; an all-zero pool -> all
/// zeros (no NaN). `scored` = `(growth score, volatility_pct, cluster)`; weights are aligned to it.
///
/// (Item 6) CORRELATION-AWARE: each distinct `cluster` (the asset class — crypto/ETF/stock) is one risk
/// bucket that gets an EQUAL share of gross; vol-target only splits WITHIN a bucket. So five names that
/// move together don't draw 5× the money of one independent name — the correlated block is capped at one
/// bucket's budget. ceiling: asset class is a crude correlation proxy; swap in a pairwise price-correlation
/// matrix (the history `size` already fetched) if the class split underdelivers.
pub fn size_weights(scored: &[(f64, Option<f64>, &str)]) -> Vec<f64> {
    const MIN_VOL: f64 = 0.5; // % daily-return stdev floor: only catches near-zero/no-history vol (a calm
                              // large-cap already swings ~1%); a higher floor would flatten real equities
                              // to one vol and silently turn this back into score-only sizing.
    let raw: Vec<f64> = scored.iter().map(|(score, vol, _)| score.max(0.0) / vol.unwrap_or(MIN_VOL).max(MIN_VOL)).collect();
    // sum the vol-target weight per cluster; only clusters with positive weight count toward the split.
    let mut cluster_tot: HashMap<&str, f64> = HashMap::new();
    for ((_, _, c), r) in scored.iter().zip(&raw) {
        *cluster_tot.entry(*c).or_insert(0.0) += *r;
    }
    let k = cluster_tot.values().filter(|t| **t > 0.0).count();
    if k == 0 {
        return vec![0.0; scored.len()]; // nothing positive to size -> zeros, never a divide-by-zero NaN
    }
    let budget = 100.0 / k as f64; // each risk bucket gets an equal share of gross
    scored
        .iter()
        .zip(&raw)
        .map(|((_, _, c), r)| {
            let ct = cluster_tot[*c];
            if ct > 0.0 { r / ct * budget } else { 0.0 }
        })
        .collect()
}

/// (Item 8) Jaccard overlap of the top-`n` tickers of two ranked lists: |∩| / |∪| in [0,1]. 1.0 = the
/// same names (stable); low = churn — paid in spread+fees, and a sign a knob change reshuffled the picks
/// (overfit smell). Both empty -> 1.0 (nothing changed). Pure.
fn rank_jaccard(prev: &[String], now: &[String], n: usize) -> f64 {
    let a: HashSet<&str> = prev.iter().take(n).map(String::as_str).collect();
    let b: HashSet<&str> = now.iter().take(n).map(String::as_str).collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    a.intersection(&b).count() as f64 / a.union(&b).count() as f64
}

/// (Item 8) Compare this run's top-`n` tickers against the previous run cached at `path`; return a one-line
/// turnover note and rewrite the cache. None on the first ever run (no baseline). Plain-text cache (one
/// ticker per line) — no serde needed. ceiling: one cache file per `path`; the caller passes a different
/// path for `screen` vs `check` so their different universes don't cross-contaminate the overlap.
fn turnover_note(now: &[String], n: usize, path: &std::path::Path) -> Option<String> {
    let prev: Vec<String> =
        std::fs::read_to_string(path).ok().map(|s| s.lines().map(String::from).collect()).unwrap_or_default();
    let top: Vec<String> = now.iter().take(n).cloned().collect();
    let _ = std::fs::write(path, top.join("\n"));
    if prev.is_empty() {
        return None; // first run -> nothing to compare against yet
    }
    let j = rank_jaccard(&prev, now, n);
    let moved = top.iter().filter(|t| !prev.iter().take(n).any(|p| p == *t)).count();
    Some(format!(
        "Rank stability vs last run: {:.0}% top-{n} overlap ({moved} new name{}). Low overlap = churn (spread/fee cost) or an over-sensitive knob.",
        j * 100.0,
        if moved == 1 { "" } else { "s" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `col_cell` under the SHIPPED tuning — the cells that read config (`leg`: which rung, which CAGR
    /// flavour, `long_trend_cap`) are then asserted against the defaults the screen actually runs with,
    /// not against a hand-built config that could drift from them.
    fn cc(key: &str, quote: &Quote, score: f64, alt: Option<f64>, mark: &str) -> String {
        col_cell(key, quote, score, alt, mark, &BuyHeuristic::default(), &HashMap::new())
    }

    /// (round 110) owned-overlay base normalizers: Trading212 `BASE_MARKET_EQ` and Yahoo
    /// `BASE.EXCHANGE` meet on the lowercased base; dash share-class tickers pass through unmangled.
    #[test]
    fn owned_overlay_base_mapping() {
        assert_eq!(t212_base("AAPL_US_EQ"), "aapl");
        assert_eq!(t212_base("IITU_GB_EQ"), "iitu");
        assert_eq!(yahoo_base("AAPL"), "aapl");
        assert_eq!(yahoo_base("IITU.L"), "iitu");
        assert_eq!(yahoo_base("VVSM.DE"), "vvsm");
        assert_eq!(yahoo_base("BRK-B"), "brk-b");
        assert_eq!(t212_base(yahoo_base("AAPL").as_str()), yahoo_base(t212_base("AAPL_US_EQ").as_str()));
        // (round 111) class-split lookup: a Binance asset flags only currency-quoted rows, a broker
        // stock never flags a crypto row — SOL the coin and SOL the stock stay distinct.
        let owned = Owned { crypto: ["sol".to_string()].into(), stocks: ["sol".to_string()].into() };
        assert!(owned.holds("SOL-EUR"));
        assert!(owned.holds("SOL"));
        let coin_only = Owned { crypto: ["sol".to_string()].into(), ..Default::default() };
        assert!(coin_only.holds("SOL-USD") && !coin_only.holds("SOL"));
        let stock_only = Owned { stocks: ["sol".to_string()].into(), ..Default::default() };
        assert!(stock_only.holds("SOL") && !stock_only.holds("SOL-USD"));
    }

    /// (round 73) currency-quoted = a `-EUR`/`-USD` SUFFIX, not any dash: share-class tickers
    /// (`BRK.B` is normalized to `BRK-B` universe-wide) must classify as equities.
    #[test]
    fn currency_quoted_is_suffix_not_any_dash() {
        assert!(is_currency_quoted("BTC-USD"));
        assert!(is_currency_quoted("ETH-EUR"));
        assert!(!is_currency_quoted("BRK-B"));
        assert!(!is_currency_quoted("AAPL"));
    }

    /// Legal-suffix strip: whole names fit the tight NAME column instead of clipping to dangling
    /// fragments; ETF/crypto names and short names pass through untouched.
    #[test]
    fn display_name_strips_legal_suffixes() {
        assert_eq!(display_name("Monolithic Power Systems, Inc."), "Monolithic Power Systems");
        assert_eq!(display_name("Marathon Petroleum Corporation"), "Marathon Petroleum");
        assert_eq!(display_name("Old Dominion Freight Line, Inc."), "Old Dominion Freight Line");
        assert_eq!(display_name("Apple Inc."), "Apple");
        assert_eq!(display_name("Amazon.com, Inc."), "Amazon.com");
        assert_eq!(display_name("VanEck Semiconductor UCITS ETF - USD Acc"), "VanEck Semiconductor UCITS ETF - USD Acc");
        assert_eq!(display_name("Bitcoin"), "Bitcoin"); // no suffix -> unchanged
        assert_eq!(display_name("Inc"), "Inc"); // guard: never strip to almost nothing
    }

    /// (#43) The ETF boilerplate strip. "UCITS ETF" is in ~100%/96% of real ETF names and sits MID-string,
    /// so it must be deleted IN PLACE — the share-class tail after it is the only thing telling two rows
    /// of the same fund apart, and cutting it collapses 52% of funds onto a shared display name.
    #[test]
    fn drop_ucits_deletes_the_token_and_keeps_the_share_class() {
        // the point of the infix form: "1C USD Hedged" survives, so hedged/unhedged stay distinguishable
        assert_eq!(drop_ucits("Amundi Core S&P 500 Swap UCITS ETF 1C USD Hedged"), "Amundi Core S&P 500 Swap 1C USD Hedged");
        assert_eq!(drop_ucits("L&G Battery Value-Chain UCITS ETF"), "L&G Battery Value-Chain"); // trailing: no dangling space
        assert_eq!(drop_ucits("VanEck Semiconductor UCITS ETF - USD Acc"), "VanEck Semiconductor - USD Acc");
        assert_eq!(drop_ucits("Invesco Physical Gold UCITS"), "Invesco Physical Gold"); // "UCITS" alone (no " ETF")
        assert_eq!(drop_ucits("SPDR S&P 500 ETF Trust"), "SPDR S&P 500 ETF Trust"); // no UCITS token -> untouched
        assert_eq!(drop_ucits("SXR8 UCITS ETF"), "SXR8"); // 4 chars clears the guard
        assert_eq!(drop_ucits("SXR UCITS ETF"), "SXR UCITS ETF"); // guard: stripping leaves <4 chars -> keep whole
    }

    /// (#44) `is_commodity`: GICS sector for stocks, name tokens for funds, and NOTHING for a row whose
    /// sector is unknown — which is the backtest pool, `check` and `screen TICKER…`, all of which pass an
    /// empty sector_of. That last case is what makes the damp backtest-blind by construction.
    #[test]
    fn is_commodity_reads_gics_for_stocks_and_tokens_for_funds() {
        let mut q = Quote::stub("CF", "€111.14", "", "CF Industries Holdings");
        q.instrument_type = "EQUITY".into();
        assert!(!is_commodity(&q), "sector unknown -> never flagged (backtest / check / explicit-args)");
        q.sector = Some("Materials".into());
        assert!(is_commodity(&q)); // CF ranked FIRST live on a -62% maxdd
        q.sector = Some("energy".into());
        assert!(is_commodity(&q), "sector match is case-insensitive");
        q.sector = Some("Information Technology".into());
        assert!(!is_commodity(&q));
        // the token path is FUND-only: a stock named "Energy Transfer" is judged on its GICS, not its name
        let mut stock = Quote::stub("ET", "€1.00", "", "Energy Transfer Partners");
        stock.instrument_type = "EQUITY".into();
        assert!(!is_commodity(&stock));
        // funds carry no GICS -> tokens. MINER_TOKENS is an INCLUSION here and an EXEMPTION in
        // is_commodity_etf: a miner basket keeps RANKING (not gated like a physical ETC) but still FLAGS.
        let mut fund = Quote::stub("GDX.L", "€1.00", "", "VanEck Gold Miners UCITS ETF");
        fund.instrument_type = "ETF".into();
        assert!(is_commodity(&fund) && !is_commodity_etf(&fund));
        fund.name = "iShares S&P 500 Energy Sector UCITS ETF".into();
        assert!(is_commodity(&fund));
        fund.name = "iShares Core S&P 500 UCITS ETF".into();
        assert!(!is_commodity(&fund));
        fund.name = "Goldman Sachs Access UCITS ETF".into();
        assert!(!is_commodity(&fund), "token boundary: 'Goldman' is not 'gold'");
        fund.name = "L&G Battery Value-Chain UCITS ETF".into();
        assert!(!is_commodity(&fund), "'battery' is deliberately in neither list");
        // BACKTEST INERTNESS, the claim the ci-settings receipt rests on. `backtest_quote` builds
        // `Quote::stub(tk, "", "", tk)`: sector None AND name == the ticker. The subtle part is that
        // `quote_is_etf` falls back to `is_etf(&quote.name)`, a SUBSTRING match — so a ticker that
        // literally contains "etf" DOES open the fund path (the Polish ETFB*.WA family; 11 such
        // tickers are in the live pool). It stays inert only because is_commodity token-SPLITS: a
        // ticker has no commodity token. Swap either match rule and the backtest stops being blind.
        let etfish = Quote::stub("ETFBW20LV.WA", "", "", "ETFBW20LV.WA");
        assert!(quote_is_etf(&etfish), "substring 'etf' in the ticker opens the fund path");
        assert!(!is_commodity(&etfish), "...but no commodity TOKEN -> damp x1.0 in the backtest");
        assert!(!is_commodity(&Quote::stub("CF", "", "", "CF")), "sector-less stub, the pool's shape");
    }

    /// (#43) `clean_name`'s ETF arm: the umbrella-prefix strip and the token drop COMPOSE, and the token
    /// drop is ETF-scoped — an equity whose name happens to carry "UCITS" is left alone.
    #[test]
    fn clean_name_strips_etf_boilerplate_only_for_etfs() {
        let mut etf = Quote::stub("SXR8.DE", "€1.00", "", "iShares VII PLC - iShares NASDAQ 100 UCITS ETF");
        etf.instrument_type = "ETF".into();
        assert_eq!(clean_name(&etf), "iShares NASDAQ 100"); // prefix strip THEN token drop
        etf.name = "Amundi MSCI World UCITS ETF Acc".into();
        assert_eq!(clean_name(&etf), "Amundi MSCI World Acc");
        let mut equity = Quote::stub("UCT", "€1.00", "", "UCITS Holdings Corporation");
        equity.instrument_type = "EQUITY".into();
        assert_eq!(clean_name(&equity), "UCITS Holdings"); // legal-suffix strip only; token drop never runs
    }

    /// (#43) `name_etf` reaches the ETF table through `column_widths`, so it beats an explicit user
    /// `column_widths: { name: N }` — the precedence `print_lane` depends on. 0 = off leaves both lanes equal.
    #[test]
    fn name_etf_width_overrides_the_shared_name_width() {
        let spec = COLUMNS.iter().find(|c| c.key == "name").unwrap();
        let mut w = Widths { name: 28, name_etf: 45, ..Widths::default() };
        w.column_widths.insert("name".into(), 20); // an explicit user override of the SHARED width
        assert_eq!(col_width(spec, &w), 20); // stock lane: the user's entry stands
        let mut etf_w = w.clone(); // what print_lane builds for the ETF call
        etf_w.column_widths.insert("name".into(), w.name_etf);
        assert_eq!(col_width(spec, &etf_w), 45); // ETF lane: name_etf wins over that same entry
        let off = Widths { name: 28, name_etf: 0, ..Widths::default() };
        assert_eq!(col_width(spec, &off), 28); // 0 = off -> the shared width, byte-identical to before
    }

    /// `size_weights`: vol-target sizing — bigger slice for higher score / lower vol; sums to 100;
    /// degenerate inputs (empty, all-zero score) stay finite. Pure, no network. (Item 6) within ONE
    /// cluster it's plain vol-target (the original behaviour); across clusters each is an equal bucket.
    #[test]
    fn size_weights_vol_target() {
        // A: score 72, vol 1%. B: SAME score, DOUBLE vol. C: lower score, same vol as A. All one cluster.
        let w = size_weights(&[(72.0, Some(1.0), "x"), (72.0, Some(2.0), "x"), (40.0, Some(1.0), "x")]);
        assert!((w.iter().sum::<f64>() - 100.0).abs() < 1e-9, "weights must sum to 100"); // normalised
        assert!(w[0] > w[1], "same score, lower vol -> bigger slice");
        assert!(w[0] > w[2], "same vol, higher score -> bigger slice");
        assert!((w[0] - 2.0 * w[1]).abs() < 1e-9, "double the vol -> half the weight");
        // degenerate: empty -> empty; all-zero score -> zeros (no NaN/panic); missing vol uses the floor.
        assert!(size_weights(&[]).is_empty());
        assert_eq!(size_weights(&[(0.0, Some(1.0), "x")]), vec![0.0]);
        assert!(size_weights(&[(50.0, None, "x"), (50.0, None, "x")]).iter().all(|w| (w - 50.0).abs() < 1e-9));
    }

    /// (Item 6) correlation-aware: two identical "crypto" names + one "stock". Plain vol-target would
    /// give the crypto BLOCK ~2/3 of the basket (3 equal names); cluster-budgeting caps it at ONE bucket,
    /// so crypto-block ≈ stock ≈ 50%, and the lone stock outweighs either correlated crypto name.
    #[test]
    fn size_weights_caps_correlated_cluster() {
        let w = size_weights(&[(60.0, Some(1.0), "crypto"), (60.0, Some(1.0), "crypto"), (60.0, Some(1.0), "stock")]);
        assert!((w.iter().sum::<f64>() - 100.0).abs() < 1e-9);
        assert!((w[0] + w[1] - w[2]).abs() < 1e-9, "crypto block == stock bucket (equal risk buckets)");
        assert!(w[2] > w[0] && w[2] > w[1], "the lone stock outweighs each correlated crypto name");
    }

    /// (#14/#15) the long-CAGR pipeline: `core::trend_cagr` fits the log-price SLOPE (perfectly
    /// log-linear data -> exact CAGR, regardless of endpoint noise; <2 pts / non-positive -> None), and
    /// `long_leg_fixed` pins the ranking window (0 = longest leg; N = the NY leg; falls back when absent).
    #[test]
    fn long_cagr_pipeline() {
        // perfectly log-linear closes (×2 per bar), cadence 1 -> annual factor 2 -> CAGR 100%.
        assert!((core::trend_cagr(&[1.0, 2.0, 4.0, 8.0], 1).unwrap() - 100.0).abs() < 1e-6);
        // monthly cadence 12 on ×2-per-bar -> 2^12 - 1 ~ huge; just assert it annualizes UP from per-bar.
        assert!(core::trend_cagr(&[1.0, 2.0, 4.0, 8.0], 12).unwrap() > 100.0);
        assert_eq!(core::trend_cagr(&[5.0], 1), None); // <2 usable points
        assert_eq!(core::trend_cagr(&[0.0, -1.0], 1), None); // non-positive skipped -> <2 left
        // long_leg_fixed: build a quote carrying 20Y/10Y/5Y legs via the buy_heuristic test's builder shape.
        let perf: Vec<Option<(String, f64)>> = HORIZONS
            .iter()
            .map(|(l, _)| match *l {
                "20Y" => Some(("x".into(), 900.0)),
                "10Y" => Some(("x".into(), 200.0)),
                "5Y" => Some(("x".into(), 60.0)),
                _ => None,
            })
            .collect();
        let mut q = Quote::stub("T", "", "", "n");
        q.perf = perf;
        assert_eq!(long_leg_fixed(&q, 0, 5.0), Some((900.0, 20.0))); // off -> longest leg (20Y)
        assert_eq!(long_leg_fixed(&q, 10, 5.0), Some((200.0, 10.0))); // pinned -> the 10Y leg
        q.perf[HORIZONS.iter().position(|(l, _)| *l == "10Y").unwrap()] = None; // drop 10Y
        assert_eq!(long_leg_fixed(&q, 10, 5.0), Some((900.0, 20.0))); // pinned leg absent -> longest leg fallback
        // `growth_min_leg_years`: a name whose ONLY leg is 2Y is unrankable at the shipped 5.0 (it
        // reports the `history` gate) and gets a real, short CAGR at 2.0. This is the whole knob.
        let mut young = Quote::stub("Y", "", "", "n");
        young.perf = HORIZONS
            .iter()
            .map(|(l, _)| (*l == "2Y").then(|| ("x".to_string(), 44.0)))
            .collect();
        assert_eq!(long_leg(&young, 5.0), None, "at 5.0 the 2Y rung must not exist — today's ladder");
        assert_eq!(long_leg(&young, 2.0), Some((44.0, 2.0)), "at 2.0 the 2Y rung carries the leg");
        // and the knob must never PROMOTE a short rung over a long one it already had
        assert_eq!(long_leg(&q, 2.0), Some((900.0, 20.0)), "longest rung still wins when present");
    }

    /// `wants_intraday` decides whether `screen` spends an EXTRA Yahoo chart request per name, so a
    /// wrong answer either blanks three columns or burns ~3847 requests (~65s of pacer sleep) on a
    /// universe run printing nothing. Both directions pinned, plus the two ways a key can fail to
    /// match — because "no intraday column configured" and "I misspelled the intraday column" must
    /// both resolve to NOT fetching rather than to a silent half-state.
    #[test]
    fn intraday_is_fetched_only_when_a_column_prints_it() {
        assert!(wants_intraday(&[]), "empty config = DEFAULT_COLUMNS, which carries 1h/6h/12h");
        for k in ["1h", "6h", "12h"] {
            assert!(wants_intraday(&["rank".to_string(), k.to_string()]), "{k} alone must fetch");
        }
        // same case-insensitive match `active_columns` uses — a config saying `6H` still prints, so it must pay
        assert!(wants_intraday(&["6H".to_string()]));
        let no_intra: Vec<String> = ["rank", "name", "cagr", "1d", "1y", "score"].iter().map(|s| s.to_string()).collect();
        assert!(!wants_intraday(&no_intra), "an explicit layout without them must not pay for them");
        assert!(!wants_intraday(&["1hour".to_string()]), "an unknown key is dropped, never a false fetch");
    }

    /// (screen columns) `active_columns` resolves config -> ordered ColSpecs (empty = default layout;
    /// whitelist = those keys in order; unknown keys dropped), `fmt_cell` pads+aligns, `col_cell` formats.
    #[test]
    fn screen_columns_config() {
        // empty config -> the canonical default layout (rank..score), and cagr/maxdd are shown by default
        let def = active_columns(&[]);
        assert_eq!(def.first().unwrap().key, "rank");
        assert_eq!(def.last().unwrap().key, "score8y"); // the 8Y-pinned read sits right after SCORE
        assert_eq!(def.len(), DEFAULT_COLUMNS.len());
        assert!(def.iter().any(|c| c.key == "cagr") && def.iter().any(|c| c.key == "maxdd"));
        // every default key resolves to a real ColSpec (guards a typo in DEFAULT_COLUMNS)
        let all_default: Vec<String> = DEFAULT_COLUMNS.iter().map(|s| s.to_string()).collect();
        assert_eq!(active_columns(&all_default).len(), DEFAULT_COLUMNS.len());
        // explicit whitelist -> exactly those keys IN ORDER; an unknown key is silently dropped
        let custom: Vec<String> = ["score", "cagr", "bogus", "vol"].iter().map(|s| s.to_string()).collect();
        assert_eq!(active_columns(&custom).iter().map(|c| c.key).collect::<Vec<_>>(), ["score", "cagr", "vol"]);
        // fmt_cell: right-align pads left, left-align pads right; truncate never over-runs the width
        assert_eq!(fmt_cell("AB", 5, true), "   AB");
        assert_eq!(fmt_cell("AB", 5, false), "AB   ");
        assert_eq!(fmt_cell("ABCDEF", 3, true).chars().count(), 3);
        // col_width: a settings.yaml override wins over the built-in width, but never narrower than the header.
        let cagr = COLUMNS.iter().find(|c| c.key == "cagr").unwrap();
        let mut wd = Widths::default();
        assert_eq!(col_width(cagr, &wd), 8); // no override -> built-in fixed width
        wd.column_widths.insert("cagr".into(), 12);
        assert_eq!(col_width(cagr, &wd), 12); // override wins
        wd.column_widths.insert("cagr".into(), 1);
        assert_eq!(col_width(cagr, &wd), "CAGR".chars().count()); // floored at the header
        // col_cell: rank passes the mark through; score -> 1dp; cagr with no history -> n/a (stub has no legs)
        let q = Quote::stub("T", "€1", "", "Name");
        assert_eq!(cc("rank", &q, 9.4, None, "3*"), "3*");
        assert_eq!(cc("score", &q, 7.0, None, ""), "7.0");
        assert_eq!(cc("cagr", &q, 0.0, None, ""), "n/a");
        // peg needs BOTH a P/E and positive growth; stub has neither -> n/a (never panics on the guard)
        assert_eq!(cc("peg", &q, 0.0, None, ""), "n/a");
        // ter is None on a stub (no expense ratio fetched) -> n/a
        assert_eq!(cc("ter", &q, 0.0, None, ""), "n/a");
        // div distinguishes "pays nothing" from "don't know" — the whole point of reading the Option
        // instead of `dividend_yield_1y`'s unwrap_or(0.0). No price -> genuinely unknown -> n/a.
        assert_eq!(cc("div", &q, 0.0, None, ""), "n/a");
        let mut payer = Quote::stub("P", "€1", "", "Payer");
        payer.price_eur = Some(100.0);
        payer.div_eur = vec![Some(2.0)]; // 2.00 EUR/share on a 100 EUR price = 2.00%
        assert_eq!(cc("div", &payer, 0.0, None, ""), "2.00%");
        // a CONFIDENT zero prints as one, and must never surface the FX-sum's negative-zero as "-0.00%"
        let mut none_paid = payer.clone();
        none_paid.div_eur = vec![Some(-0.0)];
        assert_eq!(cc("div", &none_paid, 0.0, None, ""), "0.00%");
        none_paid.div_eur = vec![Some(-1e-9)];
        assert_eq!(cc("div", &none_paid, 0.0, None, ""), "0.00%");
        // ...but a token yield still prints rather than rounding away (NVDA is ~0.02%)
        none_paid.div_eur = vec![Some(0.02)];
        assert_eq!(cc("div", &none_paid, 0.0, None, ""), "0.02%");
        // per-class gating: a crypto ('-' ticker) has neither P/E nor expense ratio -> "—" (not applicable),
        // distinct from "n/a" (applies but unfetched).
        let cq = Quote::stub("X-EUR", "€1", "", "Coin");
        assert_eq!(cc("pe", &cq, 0.0, None, ""), "—");
        assert_eq!(cc("ter", &cq, 0.0, None, ""), "—");
        // (USE/REPL) ETF prints its BF tokens; None (off-BF fund) -> n/a; crypto/equity -> — (not a fund)
        let mut eq = Quote::stub("E.DE", "€1", "", "Fund");
        eq.instrument_type = "ETF".to_string();
        assert_eq!(cc("use", &eq, 0.0, None, ""), "n/a");
        assert_eq!(cc("repl", &eq, 0.0, None, ""), "n/a");
        eq.use_of_profits = Some("Acc");
        eq.replication = Some("Swap");
        assert_eq!(cc("use", &eq, 0.0, None, ""), "Acc");
        assert_eq!(cc("repl", &eq, 0.0, None, ""), "Swap");
        assert_eq!(cc("use", &cq, 0.0, None, ""), "—");
        assert_eq!(cc("repl", &cq, 0.0, None, ""), "—");
        // (DOM) same per-class gating as USE/REPL; ETF prints its ISIN-prefix country
        assert_eq!(cc("dom", &eq, 0.0, None, ""), "n/a");
        eq.domicile = Some("IE".to_string());
        assert_eq!(cc("dom", &eq, 0.0, None, ""), "IE");
        assert_eq!(cc("dom", &cq, 0.0, None, ""), "—");
        // (TR-CAGR) display-only lower-bound total return; stub -> n/a
        assert_eq!(cc("trcagr", &q, 0.0, None, ""), "n/a");
        eq.tr_cagr = Some(16.4);
        assert_eq!(cc("trcagr", &eq, 0.0, None, ""), "+16%");
        // (DOM) CORE ordering: IE beats any other known domicile, unknown sorts last —
        // withholding (~0.2%/yr) outranks TER deltas, so the sort applies dom_rank BEFORE TER
        let mut ie = Quote::stub("A", "€1", "", "f");
        ie.domicile = Some("IE".to_string());
        let mut lu = Quote::stub("B", "€1", "", "f");
        lu.domicile = Some("LU".to_string());
        let unk = Quote::stub("C", "€1", "", "f");
        assert!(dom_rank(&ie) < dom_rank(&lu) && dom_rank(&lu) < dom_rank(&unk));
        // (REV-YoY/EPS-YoY/NET%) stock-only FY snapshot: stock prints signed %s / margin level, an
        // un-enriched stock -> n/a, ETF/crypto -> — (no income statement)
        let mut st = Quote::stub("S", "€1", "", "Co");
        assert_eq!(cc("rev-yoy", &st, 0.0, None, ""), "n/a");
        st.rev_yoy = Some(65.5);
        st.eps_yoy = Some(-12.3);
        st.net_margin_fy = Some(55.6);
        assert_eq!(cc("rev-yoy", &st, 0.0, None, ""), "+65.5%");
        assert_eq!(cc("eps-yoy", &st, 0.0, None, ""), "-12.3%");
        assert_eq!(cc("net", &st, 0.0, None, ""), "55.6");
        assert_eq!(cc("rev-yoy", &eq, 0.0, None, ""), "—"); // ETF
        assert_eq!(cc("net", &cq, 0.0, None, ""), "—"); // crypto
    }

    /// (Item 8) `rank_jaccard` = |∩|/|∪| of the top-n: identical lists -> 1.0, one swap of three -> 0.5
    /// ({A,B} shared of {A,B,C,D}), disjoint -> 0, both empty -> 1.0. Pure.
    #[test]
    fn rank_jaccard_overlap() {
        let a = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert!((rank_jaccard(&a, &a.clone(), 3) - 1.0).abs() < 1e-9);
        let b = vec!["A".to_string(), "B".to_string(), "D".to_string()];
        assert!((rank_jaccard(&a, &b, 3) - 0.5).abs() < 1e-9); // {A,B}/{A,B,C,D}
        assert_eq!(rank_jaccard(&a, &["X".to_string()], 3), 0.0); // disjoint
        assert!((rank_jaccard(&[], &[], 3) - 1.0).abs() < 1e-9); // both empty -> stable
    }

    /// (round 12) columns-drift nag feed: empty config never nags (DEFAULT layout curated), a full
    /// list is silent, a subset reports exactly the absent master keys in master order, and
    /// matching is case-insensitive — "DOM" counts as present, same as `active_columns` resolves it.
    #[test]
    fn missing_columns_semantics() {
        assert!(missing_columns(&[]).is_empty());
        let full: Vec<String> = COLUMNS.iter().map(|c| c.key.to_string()).collect();
        assert!(missing_columns(&full).is_empty());
        let mut sub = full.clone();
        sub.retain(|k| k != "dom" && k != "trcagr");
        assert_eq!(missing_columns(&sub), vec!["trcagr", "dom"]); // master COLUMNS order
        let mut upper = sub.clone();
        upper.push("DOM".to_string());
        assert_eq!(missing_columns(&upper), vec!["trcagr"]); // case-insensitive presence
    }

    /// Buy-heuristic asserts (no network). White-box: reaches `picks` privates via `use super::*`.
    #[test]
    fn buy_heuristic() {
    // build a Quote with chosen horizon %s set (others n/a), robust to HORIZONS order. First
    // arg = drawdown_pct (% below the OFF-HI high) — the on-sale signal the score is built on.
    let quote = |drawdown_pct: f64, labels: &[(&str, f64)]| -> Quote {
        let mut perf: Vec<Option<(String, f64)>> = HORIZONS
            .iter()
            .map(|(l, _)| labels.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
            .collect();
        // (ladder) Real history is contiguous: anything carrying a 10Y record also carries an 8Y one.
        // These fixtures predate the 20/8/5 ladder and list 10Y alone, so `long_leg` would skip past
        // the empty 8Y rung down to 5Y and quietly rescore ~90 asserts against a leg they never meant.
        // Fill the gap at the SAME annualized rate, which leaves every fixture's long CAGR byte-identical
        // to what its assert was written against — the ladder move is measured in the backtest, not here.
        let idx = |label: &str| HORIZONS.iter().position(|(l, _)| *l == label).unwrap();
        let (i8y, i10y) = (idx("8Y"), idx("10Y"));
        if perf[i8y].is_none() {
            if let Some((_, c10)) = perf[i10y].clone() {
                let growth = 1.0 + c10 / 100.0;
                if growth > 0.0 {
                    perf[i8y] = Some(("x".to_string(), (growth.powf(0.8) - 1.0) * 100.0));
                }
            }
        }
        Quote {
            ticker: "T".into(), price: "€1.00".into(), dip: "-5.0%".into(), drop_pct: drawdown_pct,
            market: "USA".into(), instrument_type: String::new(), head: String::new(), news_block: String::new(), perf,
            name: "n".into(), trend: String::new(), at_ath: false, at_atl: false, mom_pct: None,
            div_eur: Vec::new(), price_eur: None, close_native: None, quote_currency: None, last_close_date: None, drawdown_pct, intraday: [None; 3],
            // (#20) default a KNOWN turnover so the growth lane's unknown-turnover gate admits test
            // quotes; tests exercising that gate set avg_turnover_eur = None explicitly. €1B -> liq_bonus
            // ln(1e9/1e9)=0, so it stays rank-neutral for the relational score asserts.
            stats_8y: None, // (S-8Y) display-only diagnostic; None keeps `as_8y_window` the identity here
            sector: None, // (#44) unknown sector -> is_commodity false -> damp inert, as in the backtest
            downside_dev_pct: None, // (r39) backtest-probe-only, never read by the score
            avg_turnover_eur: Some(1e9), volatility_pct: None, below_ma_pct: 0.0, above_ma_pct: 0.0,
            pe_ratio: None,
            mvrv: None, // (#45) no MVRV -> the crypto ceiling passes these fixtures free; the gate's own tests set it
            roe: None,
            expense_ratio: None, // (TER) display-only, never scored; tests don't exercise it
            // for tests, mirror the on-sale magnitude: a deeper drawdown = deeper in its range.
            // (real fetch computes range_pct independently; tying them keeps the score asserts honest.)
            range_pct: 100.0 - drawdown_pct,
            trend_r2: 0.0, // default lumpy -> consistency floor, UNIFORM across test quotes so relational asserts hold
            trend_cagr: None, // (#14) default off; ranking uses endpoint cagr unless use_trend_cagr is set
            max_drawdown_pct: 0.0, // default -> no calmar reward (additive 0)
            roll5y_pos_pct: None,  // (consistency) display-only footer stat; never scored
            underwater_yrs: None,  // (underwater) display-only footer stat; never scored
            worst_5y_pct: None,    // (worst-5y) display-only footer stat; never scored
            roll10y_pos_pct: None, // (r16) decade twins — display-only footer stats; never scored
            worst_10y_pct: None,
            year_returns: Vec::new(), // (r11) display-only footer strip; never scored
            fund_factor: None,     // (G) default off; the fund-tilt asserts set it explicitly
            fund: None,            // (G+) default off; the multi-term asserts set it explicitly
            age_years: None,       // display-only pair; never scored
            life_cagr: None,
            capped_cagr: None,     // (#3l) default off; the capped-window arm sets it via config
            life_return_pct: None,     // (perf_fill) display-only; the fill asserts set it explicitly
            trail_monthly: Vec::new(), // (#41) no trail -> unjudgeable -> the redundancy skip never blocks
            tr_cagr: None,         // (TR-CAGR) display-only; never scored
            history_proxied: false, // display-only marker; never scored
            aum_eur: None,          // (AUM) fund-size gate inert by default; its tests set it explicitly
            ter_fallback: None,     // Yahoo facts fallback pair — display/H-CORE only; their tests set them explicitly
            aum_fallback: None,
            use_of_profits: None,   // (USE) display-only token; never scored
            replication: None,      // (REPL) display-only token; never scored
            benchmark: None,        // twin-hint key only; the hint tests set it explicitly
            domicile: None,         // (DOM) display + CORE ordering only; its tests set it explicitly
            rev_yoy: None,          // display-only FY snapshot; cell tests set them explicitly
            eps_yoy: None,
            net_margin_fy: None,
            buyback_yoy: None,
            annual_brief: None,
        }
    };
    let tuning = BuyHeuristic::default(); // momentum neutral 1.0/1.0, CAGR-based long reward, A-E terms on

    // --- pure helpers ---
    assert_eq!(perf_pct(&quote(5.0, &[("1Y", 20.0)]), "1Y"), Some(20.0));
    assert_eq!(perf_pct(&quote(5.0, &[]), "1Y"), None);
    // (A) CAGR annualizes a cumulative %: 0 stays 0, +100% over 1y = 100, +300% over 10y ≈ 14.9%/yr
    assert!(core::cagr(0.0, 10.0).abs() < 1e-9);
    assert!((core::cagr(100.0, 1.0) - 100.0).abs() < 1e-9);
    assert!((core::cagr(300.0, 10.0) - 14.87).abs() < 0.1);
    assert!(core::cagr(-100.0, 5.0).is_finite()); // near-total loss must not NaN the root
    // (C) below-SMA %: last 50 vs mean 83.33 of [100,100,50] = 40%; window longer than history = 0
    assert!((core::below_long_ma_pct(&[100.0, 100.0, 50.0], 3) - 40.0).abs() < 1e-9);
    assert_eq!(core::below_long_ma_pct(&[1.0, 2.0], 5), 0.0);
    // (A) price percentile rank: at the high -> 100, at the low -> 0, robust to a single spike
    assert_eq!(core::price_pct_rank(&[10.0, 20.0, 30.0]), 100.0); // last = max
    assert_eq!(core::price_pct_rank(&[30.0, 20.0, 10.0]), 0.0); // last = min
    assert_eq!(core::price_pct_rank(&[10.0, 1000.0, 20.0]), 50.0); // mid: 1 of 2 others below, spike ignored
    assert_eq!(core::price_pct_rank(&[]), 0.0);
    assert_eq!(core::price_pct_rank(&[5.0]), 0.0); // too short
    // #1 normalized dip: a calm asset's dip is amplified, a wild one's damped, unknown vol = raw
    assert!((normalized_dip(30.0, Some(1.0), 2.0) - 60.0).abs() < 1e-9);
    assert!((normalized_dip(30.0, Some(4.0), 2.0) - 15.0).abs() < 1e-9);
    assert_eq!(normalized_dip(30.0, None, 2.0), 30.0);
    assert_eq!(normalized_dip(30.0, Some(0.0), 2.0), 30.0); // div-by-zero guard

    // (#3h) FOIL GUARD: `long_trend_cap` is shared with this lane, and 0 now means OFF. `min(cagr, 0.0)`
    // is 0.0 for every positive CAGR, so an unguarded cap-0 config would zero `long_reward` here — a foil
    // quietly losing its trend term while still returning plausible scores, which no other test would
    // catch. A >30%/yr name must score at least as well uncapped as capped, never worse.
    let fast = quote(20.0, &[("1Y", 20.0), ("5Y", 400.0)]); // 5Y +400% = 38.0%/yr, above the 30 cap
    let capped = BuyHeuristic { long_trend_cap: 30.0, ..tuning.clone() };
    let uncapped = BuyHeuristic { long_trend_cap: 0.0, ..tuning.clone() };
    let (sc, su) = (buy_score(&fast, &capped).unwrap(), buy_score(&fast, &uncapped).unwrap());
    assert!(su >= sc, "cap 0 must UNCAP the foil's long_reward, not zero it ({su} vs {sc})");
    assert!(su > 0.0, "cap 0 zeroed the foil score outright — the min(cagr, 0.0) trap");

    // --- GATES (exclusion behaviour, unchanged) ---
    assert!(buy_score(&quote(5.0, &[("1Y", 20.0)]), &tuning).is_none()); // equity: no >2Y leg -> excluded
    let mut crypto = quote(5.0, &[("1Y", 20.0)]); // ...but crypto falls back to its 1Y leg -> admitted
    crypto.ticker = "BTC-EUR".into();
    assert!(buy_score(&crypto, &tuning).is_some());
    assert!(buy_score(&quote(5.0, &[("1Y", 20.0), ("5Y", 40.0), ("1M", -25.0)]), &tuning).is_none()); // equity knife
    let mut knife_crypto = quote(5.0, &[("1Y", 20.0), ("1M", -25.0)]); // crypto looser knife -> admitted
    knife_crypto.ticker = "ETH-EUR".into();
    assert!(buy_score(&knife_crypto, &tuning).is_some());
    assert!(buy_score(&quote(5.0, &[("1Y", 20.0), ("5Y", -50.0)]), &tuning).is_none()); // equity: neg 5Y leg
    let mut corpse = quote(40.0, &[("1Y", -30.0), ("5Y", -95.0)]); // crypto corpse (>2Y leg -95%) excluded
    corpse.ticker = "FIL-EUR".into();
    assert!(buy_score(&corpse, &tuning).is_none());
    let mut peg = quote(0.3, &[("1Y", 3.0), ("5Y", 3.0)]); // crypto at its high: drawdown<3% -> nothing on sale
    peg.ticker = "PEPE-EUR".into();
    assert!(buy_score(&peg, &tuning).is_none());
    // stablecoin gate (3): excluded even with a fat EUR-leg "drawdown" that clears the 3% peg gate
    assert!(is_stablecoin("USDC-EUR") && is_stablecoin("USDT-USD") && !is_stablecoin("BTC-EUR"));
    // (#21) pegged list also covers the dollar token USDF and the gold tokens (metal peg, not growth)
    assert!(is_stablecoin("USDF-USD") && is_stablecoin("XAUT-USD") && is_stablecoin("PAXG-USD"));
    let mut stable = quote(16.0, &[("1Y", -20.0)]);
    stable.ticker = "USDC-EUR".into();
    assert!(buy_score(&stable, &tuning).is_none()); // pegged $1 -> no growth, FX drift faked the dip
    assert!(is_leveraged("GraniteShares 2x Short NVD") && !is_leveraged("Apple Inc."));
    // (1) Direxion Daily 3x leaks a SHORT name without "3x" -> issuer marker still catches it (TECL)
    assert!(is_leveraged("Direxion Daily Technology") && !is_leveraged("Technology Select Sector"));
    // ETF classifier (splits the equity table only): funds match, single companies don't
    assert!(is_etf("iShares Core S&P 500 UCITS ETF") && is_etf("SPDR S&P 500 ETF Trust"));
    assert!(!is_etf("Apple Inc.") && !is_etf("NVIDIA Corporation"));
    let mut lev = quote(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    lev.name = "GraniteShares 2x Short NVD".into();
    assert!(buy_score(&lev, &tuning).is_none()); // leveraged/inverse product excluded
    // (#35) physical-commodity ETC: no cashflow -> not a compounder -> excluded from the growth lane.
    // ETF-scoped, so a gold-MINER equity basket (earnings-bearing) survives.
    let strong = &[("1Y", 30.0), ("5Y", 99.0), ("10Y", 200.0)]; // a name that WOULD rank on momentum
    let mut gold = quote(2.0, strong);
    gold.name = "Xtrackers IE Physical Gold ETC".into();
    gold.instrument_type = "ETF".into(); // Yahoo classifies the ETC as ETF (it lands in the ETF table live)
    assert!(is_commodity_etf(&gold) && growth_score(&gold, &tuning).is_none()); // gold ETC -> gated
    let mut miners = quote(2.0, strong);
    miners.name = "VanEck Gold Miners UCITS ETF".into(); // equity basket (holds miner STOCKS) -> NOT commodity
    assert!(!is_commodity_etf(&miners) && growth_score(&miners, &tuning).is_some()); // miners survive
    let mut broad = quote(2.0, strong);
    broad.name = "iShares Core S&P 500 UCITS ETF".into();
    assert!(!is_commodity_etf(&broad)); // a plain index ETF is never commodity-gated
    // ETC legal-wrapper token: an issuer-legal-name row carries neither "physical" nor "commodit"
    // (XGDE.DE = "XTrackers ETC PLC", physical gold) — the standalone ETC token itself is the marker.
    let mut wrapper = quote(2.0, strong);
    wrapper.name = "XTrackers ETC PLC".into();
    wrapper.instrument_type = "ETF".into();
    assert!(is_commodity_etf(&wrapper) && growth_score(&wrapper, &tuning).is_none()); // ETC wrapper -> gated
    let mut fetch = quote(2.0, strong);
    fetch.name = "Fetchr Growth ETCetera Fund UCITS ETF".into(); // token match: substrings can't trip it
    assert!(!is_commodity_etf(&fetch));
    // (#36) bare-metal hole: physical trackers named by metal only (no "physical"/"commodit"/ETC)
    let mut xetra = quote(2.0, strong);
    xetra.name = "Xetra-Gold".into();
    xetra.instrument_type = "ETF".into();
    assert!(is_commodity_etf(&xetra)); // metal token -> commodity
    xetra.name = "Gold Bullion Securities Ltd".into();
    assert!(is_commodity_etf(&xetra)); // "bullion" too
    xetra.name = "Global X Silver Miners UCITS ETF".into();
    assert!(!is_commodity_etf(&xetra)); // miner-word exemption: equity basket keeps ranking
    xetra.name = "Goldman Sachs Access UCITS ETF".into();
    assert!(!is_commodity_etf(&xetra)); // token boundary: "goldman" != "gold"
    // (#36) crypto VOL cap: daily swing wilder than the base -> out; at/below cap or unknown -> in
    let vt = BuyHeuristic { growth_max_vol_crypto: 3.0, ..BuyHeuristic::default() };
    let mut wild = quote(2.0, strong);
    wild.ticker = "OKB-USD".into();
    wild.volatility_pct = Some(3.4);
    assert!(growth_score(&wild, &vt).is_none()); // wilder than Bitcoin -> gated
    wild.volatility_pct = Some(2.4);
    assert!(growth_score(&wild, &vt).is_some()); // BTC-like swing passes
    wild.volatility_pct = None; // missing series not punished
    assert!(growth_score(&wild, &vt).is_some());
    // (#37) ETF Sharpe cap: a calm-line wrapper's CAGR/vol (15/1.0 = 15) caps at 9 for ETFs, at
    // sharpe_cap (15) for stocks — listing-line vol noise can't outvote real cost between wrappers.
    let sc = BuyHeuristic { sharpe_cap_etf: 9.0, ..BuyHeuristic::default() };
    let mut line = quote(2.0, strong);
    line.volatility_pct = Some(1.0);
    let stock_risk = risk_bonus(&line, 15.0, 0.15, 0.0, &sc);
    line.name = "iShares Core S&P 500 UCITS ETF".into();
    let etf_risk = risk_bonus(&line, 15.0, 0.15, 0.0, &sc);
    assert!((stock_risk - 0.15 * 15.0).abs() < 1e-9 && (etf_risk - 0.15 * 9.0).abs() < 1e-9);
    let off = BuyHeuristic { sharpe_cap_etf: 0.0, ..BuyHeuristic::default() };
    assert!((risk_bonus(&line, 15.0, 0.15, 0.0, &off) - 0.15 * 15.0).abs() < 1e-9); // 0 = off
    // (E) trend-smoothness reward: same quote, straighter climb (higher trend_r2) -> higher score;
    // weight 0 (default) leaves the score untouched by trend_r2.
    let sm = BuyHeuristic { growth_smoothness_weight: 5.0, ..BuyHeuristic::default() };
    let mut lumpy = quote(2.0, strong);
    let mut straight = lumpy.clone();
    straight.trend_r2 = 0.9;
    assert!(growth_score(&straight, &sm).unwrap() > growth_score(&lumpy, &sm).unwrap());
    lumpy.trend_r2 = 0.9; // weight 0: r2 inert
    assert_eq!(growth_score(&lumpy, &BuyHeuristic::default()), growth_score(&quote(2.0, strong), &BuyHeuristic::default()));
    // drawdown-duration penalty: same quote, longer underwater stretch -> lower score; None = no
    // penalty (equals an explicit 0.0y); weight 0 (default) leaves the score untouched by the field.
    let uw = BuyHeuristic { growth_underwater_weight: 0.3, ..BuyHeuristic::default() };
    let quick = quote(2.0, strong);
    let mut bleeder = quick.clone();
    bleeder.underwater_yrs = Some(3.0);
    assert!(growth_score(&bleeder, &uw).unwrap() < growth_score(&quick, &uw).unwrap());
    let mut zeroed = quick.clone();
    zeroed.underwater_yrs = Some(0.0); // fresh-high name: 0y stretch == None (no claim, no dock)
    assert_eq!(growth_score(&zeroed, &uw), growth_score(&quick, &uw));
    assert_eq!(growth_score(&bleeder, &BuyHeuristic::default()), growth_score(&quick, &BuyHeuristic::default())); // weight 0: field inert

    // (X) EXIT-review diff: only PRIOR-PASSING names that fail NOW get a line — still-passing and
    // never-passing names are skipped; a structurally unassessable one (unknown turnover) is flagged.
    let mut keep = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // clears every gate
    keep.ticker = "KEEP".into();
    let mut gone = quote(40.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // fails the range gate
    gone.ticker = "GONE".into();
    let mut dead = keep.clone();
    dead.ticker = "DEAD".into();
    dead.avg_turnover_eur = None; // unknown turnover -> gate_failures None
    let prior = vec!["KEEP".to_string(), "GONE".to_string(), "DEAD".to_string()];
    let watch = [&keep, &gone, &dead];
    let lines = exit_review_lines(&prior, &watch, &tuning, 8);
    assert_eq!(lines.len(), 2, "GONE + DEAD flagged, KEEP silent");
    assert!(lines.iter().any(|l| l.contains("GONE") && l.contains("range")));
    assert!(lines.iter().any(|l| l.contains("DEAD") && l.contains("assessable")));
    assert!(exit_review_lines(&[], &watch, &tuning, 8).is_empty()); // nothing previously passing -> no block
    // (#25) lifetime-uptrend second leg: trend_cagr is fit on the fetched window, so a name that
    // collapsed BEFORE the window and recovered inside it slips through (MSCI Greece pattern) —
    // life_cagr (listing-to-date) catches it. None (backtest) stays exempt -> edge-blind.
    // growth_min_cagr is neutralized so the LIFETIME dock is the only thing that can reject: since
    // (#3i) that floor reads life_cagr too, and -8.0 would fail it as well — the assert would still be
    // green while proving nothing about the leg under test.
    let lt = BuyHeuristic {
        growth_require_lifetime_uptrend: true,
        growth_min_cagr: f64::NEG_INFINITY,
        ..BuyHeuristic::default()
    };
    let mut greece = quote(2.0, strong);
    greece.life_cagr = Some(-8.0);
    assert!(growth_score(&greece, &lt).is_none()); // lifetime loser despite strong window legs
    greece.life_cagr = Some(12.0);
    assert!(growth_score(&greece, &lt).is_some()); // whole life positive -> passes
    greece.life_cagr = None; // backtest shape: field absent -> gate leg inert
    assert!(growth_score(&greece, &lt).is_some());

    // (#3i) growth_min_cagr's SECOND leg: the same floor against the whole-life CAGR. Strong recent legs
    // are held fixed throughout, so only life_cagr moves — a name that clears the 20/8/5Y rung but
    // compounded under the floor since listing (the gold-miner ETF shape: crashed, then ran) is out.
    let life = BuyHeuristic { growth_min_cagr: 14.0, ..BuyHeuristic::default() };
    // NOT the shared `strong` fixture: its 10Y +200% backfills the 8Y rung at 11.6%/yr, under the floor,
    // so the leg gate would reject first and this would test nothing. 10Y +400% -> 17.5%/yr, clear of 14.
    let mut miner = quote(2.0, &[("1Y", 30.0), ("5Y", 99.0), ("10Y", 400.0)]);
    assert!(long_leg(&miner, 5.0).map(|(c, y)| core::cagr(c, y)).unwrap() >= 14.0, "the LEG must clear the floor, else this tests the wrong gate");
    miner.life_cagr = Some(9.0);
    assert!(growth_score(&miner, &life).is_none()); // strong leg, mediocre whole life -> rejected
    miner.life_cagr = Some(20.0);
    assert!(growth_score(&miner, &life).is_some()); // both bars cleared
    // absent history must not read as a failed bar: a name too young for a life CAGR keeps its leg.
    // (This assert was written as "backtest_quote leaves life_cagr None, so the leg is inert there" —
    // (#3j) fills that field, so the inertness claim is gone; the fallback semantics it tests remain.)
    miner.life_cagr = None;
    assert!(growth_score(&miner, &life).is_some());
    // and the rejection has to be EXPLAINED, not silent: the gate-review footer names it `cagr-life`,
    // distinct from the `lifetime` label the <= 0 dock uses.
    miner.life_cagr = Some(9.0);
    let why = gate_failures(&miner, &life).unwrap();
    assert!(why.iter().any(|(g, _, _)| *g == "cagr-life"), "life-bar rejection must print a reason: {why:?}");
    assert!(!why.iter().any(|(g, _, _)| *g == "cagr"), "the LEG cleared — only the life bar should fail");

    // (#3j) `use_life_cagr`: rank on the whole-life CAGR — the number the `cagr` COLUMN always printed —
    // instead of the 20/8/5Y rung. GOOGL's real shape is the case that prompted it and is pinned here:
    // 22 years old, so the ladder hands it the 20Y rung at +1924% = 16.2%/yr, while its life reads
    // 23%/yr. A floor of 17 therefore rejects it for being OLD ENOUGH to earn the long rung — a 19y-old
    // peer would be judged on its 8Y leg against the same number. That cliff is the whole complaint.
    let mut goog = quote(2.0, &[("1Y", 20.0), ("5Y", 142.7), ("8Y", 372.9), ("20Y", 1924.0)]);
    goog.life_cagr = Some(23.0);
    let leg17 = BuyHeuristic { growth_min_cagr: 17.0, ..BuyHeuristic::default() };
    let life17 = BuyHeuristic { use_life_cagr: true, ..leg17.clone() };
    assert!((long_leg(&goog, 5.0).map(|(c, y)| core::cagr(c, y)).unwrap() - 16.23).abs() < 0.05, "the 20Y rung is the fixture's point");
    assert!(growth_score(&goog, &leg17).is_none(), "knob off: the 16.2%/yr leg fails a floor of 17");
    assert!(growth_score(&goog, &life17).is_some(), "knob on: the 23%/yr whole life clears it");

    // and it must reach the SCORE, not only the gate. All seven readers run off the one
    // `long_cagr_from` call, so swapping 16.2 -> 23 has to move the number — an equal score would mean
    // the knob is INERT, which is exactly the failure mode `backtest_quote` leaving life_cagr None used
    // to produce (a flip that measures "no change" and looks safe).
    let off = BuyHeuristic::default(); // default floor is 8.0 -> both bases clear, so this isolates the score
    let on = BuyHeuristic { use_life_cagr: true, ..BuyHeuristic::default() };
    assert_ne!(growth_score(&goog, &on), growth_score(&goog, &off), "use_life_cagr must change the score");
    // no life CAGR -> the two are IDENTICAL. The fallback is the LEG, never a silent 0 (which would read
    // as a flat compounder and fail the floor for a reason the name never earned).
    goog.life_cagr = None;
    assert_eq!(growth_score(&goog, &on), growth_score(&goog, &off), "no life_cagr must fall back to the leg");

    // (#37) PEG CEILING. The knob is a PEG, the field is `peg_yield` = 1/PEG x 100, so the comparison is
    // REVERSED — high peg_yield is cheap. Base quote clears every other gate at default tuning.
    let peg_t = BuyHeuristic { growth_max_peg: 2.0, ..BuyHeuristic::default() };
    let mut pq = quote(2.0, &[("1Y", 30.0), ("5Y", 99.0), ("10Y", 400.0)]);
    let with_peg = |q: &mut Quote, y: Option<f64>, eps: Option<f64>| {
        q.fund = Some(core::FundFactors { peg_yield: y, eps_ttm: eps, ..Default::default() });
    };
    with_peg(&mut pq, Some(40.0), Some(5.0)); // peg_yield 40 -> PEG 2.5, over the ceiling
    assert!(growth_score(&pq, &peg_t).is_none());
    with_peg(&mut pq, Some(60.0), Some(5.0)); // PEG 1.67 -> cheap enough
    assert!(growth_score(&pq, &peg_t).is_some());
    // the bar itself is admitted: `<`, not `<=`, matching every other floor in this chain.
    with_peg(&mut pq, Some(50.0), Some(5.0)); // exactly PEG 2.0
    assert!(growth_score(&pq, &peg_t).is_some(), "PEG exactly at the ceiling must pass");
    // LOSS-MAKER arm — the one deliberate break from "None passes". peg_yield None-outs a negative-EPS
    // name, and letting the most expensive cohort walk through a valuation ceiling would inverse the
    // gate's meaning. Distinguished from absent data by eps_ttm, which is why both cases are pinned.
    with_peg(&mut pq, None, Some(-1.0));
    assert!(growth_score(&pq, &peg_t).is_none(), "a loss-maker has no PEG — it must not pass a PEG ceiling");
    with_peg(&mut pq, None, None);
    assert!(growth_score(&pq, &peg_t).is_some(), "absent fundamentals must PASS, like every data gate");
    pq.fund = None;
    assert!(growth_score(&pq, &peg_t).is_some(), "no fund at all (every ETF/coin) passes -> equity-only for free");
    // 0 = off admits the worst case above, and guards the 100.0/0.0 infinity from reaching the compare.
    with_peg(&mut pq, Some(1.0), Some(-1.0)); // PEG 100 AND loss-making
    assert!(growth_score(&pq, &BuyHeuristic::default()).is_some(), "growth_max_peg 0 must be OFF, not a bar at infinity");

    // (N) the peg NEAR-MISS margin is RELATIVE — `<= ceiling * 1.5`, not the flat `+ 0.5` it carried
    // before. The AAPL row is the case that forced it: PEG 2.14 against a 1.6 ceiling sat 0.04 outside a
    // flat bar of 2.10 and was filed a GROSS reject, so a name one notch out vanished from the near-miss
    // tail. `pq` clears every other gate at default tuning (asserted above), so peg is the lone failure
    // and `growth_near_miss` — which needs EXACTLY one, and close — isolates this margin by itself.
    let peg16 = BuyHeuristic { growth_max_peg: 1.6, ..BuyHeuristic::default() };
    let at_peg = |q: &mut Quote, peg: f64| with_peg(q, Some(100.0 / peg), Some(5.0));
    at_peg(&mut pq, 2.14);
    assert_eq!(growth_near_miss(&pq, &peg16).map(|(g, _)| g), Some("peg"), "PEG 2.14 vs a 1.6 ceiling is ONE notch out — this fails if anyone restores `+ 0.5`");
    // the bar pinned from BOTH sides, so the 1.5 multiplier cannot drift unnoticed (1.6 * 1.5 = 2.40)
    at_peg(&mut pq, 2.39);
    assert_eq!(growth_near_miss(&pq, &peg16).map(|(g, _)| g), Some("peg"), "just inside 50% over the ceiling is still close");
    at_peg(&mut pq, 2.41);
    assert!(growth_near_miss(&pq, &peg16).is_none(), "past 50% over is a gross reject, not a near-miss");
    // and the margin still has a FAR side — it widened, it did not disappear
    at_peg(&mut pq, 5.0);
    assert!(growth_near_miss(&pq, &peg16).is_none(), "PEG 5.0 vs a 1.6 ceiling is a hard reject at any margin");

    // (V) `growth_require_peg` — the OTHER half of "None is ambiguous". A multi-class filer whose 10-K
    // tags no per-share element ANYWHERE (ARES, the sole residue) has no PEG for the ceiling to judge, so it
    // walks past a gate that cut ODFL 2.49, ROST 2.27, WMT 2.38 and TDY 2.00 in the same run.
    let req = BuyHeuristic { growth_require_peg: true, ..BuyHeuristic::default() };
    pq.fund = Some(core::FundFactors { eps_never_reported: true, ..Default::default() });
    assert!(growth_score(&pq, &BuyHeuristic::default()).is_some(), "OFF by default — shipping this knob must change nothing on its own");
    assert!(growth_score(&pq, &req).is_none(), "armed: a filer that states no EPS anywhere cannot be priced -> gated");
    // and it stands ALONE: nothing here reads `growth_max_peg`, so the two knobs can be set independently
    assert!(growth_score(&pq, &BuyHeuristic { growth_max_peg: 2.0, ..req.clone() }).is_none());
    // MIRROR LOCKSTEP — `gate_failures` is diagnostic-only, so a missing arm here means the name vanishes
    // from the table with no printed reason at all. That silence is the bug this fills (the `_ => {}`).
    let why = gate_failures(&pq, &req).expect("gated -> a reason");
    assert!(
        why.iter().any(|(g, m, close)| *g == "peg" && m.contains("tags no EPS") && !*close),
        "the tail must name it, under the `peg` key, and NEVER as a near-miss — absent data can't be close to a ceiling: {why:?}"
    );
    // THE PREDICATE IS `eps_never_reported`, NOT `peg_yield.is_none()`. The latter is a far larger set,
    // and each member of it is already handled — or deliberately not — somewhere else.
    with_peg(&mut pq, None, Some(-1.0));
    assert!(growth_score(&pq, &req).is_some(), "a LOSS-MAKER states an EPS; it is growth_max_peg's business and has its own message");
    with_peg(&mut pq, None, None);
    assert!(growth_score(&pq, &req).is_some(), "absent fundamentals still pass, like every data gate");
    pq.fund = None;
    assert!(growth_score(&pq, &req).is_some(), "no fund at all — every ETF and coin — must be untouched");

    // (#37) THE UNIFICATION PIN — the defect this whole change exists to kill. Until 2026-07-27 the
    // `peg` COLUMN computed `pe_ratio / long_cagr_from` while this gate read `peg_yield`, and the live
    // run cut APH at PEG 2.02 in the same pass it ranked ODFL printing 2.51. Reproduce ODFL's shape:
    // a printed P/E whose old-formula PEG lands one side of the ceiling and a peg_yield that lands the
    // other. Column and gate must now return the SAME verdict; before, they disagreed by construction.
    with_peg(&mut pq, Some(40.0), Some(5.0)); // peg_yield 40 -> PEG 2.50, over the 2.0 ceiling
    pq.pe_ratio = Some(20.0); // old formula: 20 / 21.5%/yr 10Y leg = PEG 0.93 -> would have printed "cheap"
    assert!(growth_score(&pq, &peg_t).is_none(), "gate must cut PEG 2.50");
    assert_eq!(col_cell("peg", &pq, 0.0, None, "", &peg_t, &HashMap::new()), "2.50", "column must SHOW the 2.50 the gate cut on, not 0.93 from pe_ratio");
    let (_, why, _) = gate_failures(&pq, &peg_t).expect("gated name yields failures").into_iter().find(|(g, ..)| *g == "peg").expect("the peg gate must name itself");
    assert!(why.contains("PEG 2.50"), "footer and column must quote ONE number, got {why:?}");

    // (#37) and the gate FOLLOWS the CAGR switch, because `long_cagr_pct` resolved it upstream into
    // peg_yield. Pinned on the helper directly — a fill site that re-hardcodes `trend_cagr` (which is
    // exactly how the two PEGs were born) makes these two reads equal and fails here.
    let mut sw = quote(2.0, &[("1Y", 30.0), ("5Y", 99.0), ("10Y", 400.0)]);
    sw.life_cagr = Some(8.0);
    sw.trend_cagr = Some(25.0);
    let life = BuyHeuristic { use_life_cagr: true, ..BuyHeuristic::default() };
    let trend = BuyHeuristic { use_trend_cagr: true, ..BuyHeuristic::default() };
    assert_eq!(long_cagr_pct(&sw, &life), Some(8.0), "use_life_cagr must reach the PEG's denominator");
    assert_eq!(long_cagr_pct(&sw, &trend), Some(25.0), "use_trend_cagr must reach it too");
    // no long leg at all -> None -> core::peg_yield returns None -> cell prints n/a and the gate
    // declines to price the name, instead of inventing a growth figure to divide by.
    assert_eq!(long_cagr_pct(&Quote::stub("N", "€1", "", "No legs"), &life), None);

    // (#38) NET-MARGIN FLOOR on the as-of `fund.net_margin` (NOT the display-only net_margin_fy).
    let nm_t = BuyHeuristic { growth_min_net_margin: 10.0, ..BuyHeuristic::default() };
    let mut nq = quote(2.0, &[("1Y", 30.0), ("5Y", 99.0), ("10Y", 400.0)]);
    let with_margin = |q: &mut Quote, m: Option<f64>| {
        q.fund = Some(core::FundFactors { net_margin: m, ..Default::default() });
    };
    with_margin(&mut nq, Some(7.5)); // the EMCOR shape — a low-margin INDUSTRY, not a bad business
    assert!(growth_score(&nq, &nm_t).is_none());
    with_margin(&mut nq, Some(12.0));
    assert!(growth_score(&nq, &nm_t).is_some());
    with_margin(&mut nq, None);
    assert!(growth_score(&nq, &nm_t).is_some(), "missing margin is not a failing margin");
    with_margin(&mut nq, Some(7.5));
    assert!(growth_score(&nq, &BuyHeuristic::default()).is_some(), "growth_min_net_margin 0 = off");

    // (#39) MARGIN-SWING ceiling. The knob is the POSITIVE std; the field is -std, so every assert here
    // also pins the sign flip — get it backwards and the gate cuts the STEADIEST names instead.
    let sw_t = BuyHeuristic { growth_max_margin_swing: 5.0, ..BuyHeuristic::default() };
    let mut sq = quote(2.0, &[("1Y", 30.0), ("5Y", 99.0), ("10Y", 400.0)]);
    let with_swing = |q: &mut Quote, s: Option<f64>| {
        q.fund = Some(core::FundFactors { margin_stability: s, ..Default::default() });
    };
    with_swing(&mut sq, Some(-8.0)); // the CF/MPC shape: margin swinging 8pp across the cycle
    assert!(growth_score(&sq, &sw_t).is_none(), "an 8pp margin swing must fail a 5pp ceiling");
    with_swing(&mut sq, Some(-2.0)); // a steady compounder
    assert!(growth_score(&sq, &sw_t).is_some(), "a 2pp swing is well inside the ceiling");
    with_swing(&mut sq, Some(-5.0));
    assert!(growth_score(&sq, &sw_t).is_some(), "exactly at the ceiling passes — `<`, like every other bar here");
    with_swing(&mut sq, None);
    assert!(growth_score(&sq, &sw_t).is_some(), "under 3 filings -> None -> passes, like every data gate");
    sq.fund = None;
    assert!(growth_score(&sq, &sw_t).is_some(), "no fund at all (every ETF/coin) passes -> equity-only for free");
    with_swing(&mut sq, Some(-8.0));
    assert!(growth_score(&sq, &BuyHeuristic::default()).is_some(), "growth_max_margin_swing 0 = off");
    let why_sw = gate_failures(&sq, &sw_t).unwrap();
    assert!(
        why_sw.iter().any(|(g, m, _)| *g == "swing" && m.contains("8.0pp")),
        "a swing rejection must print the positive swing the knob speaks: {why_sw:?}"
    );

    // both rejections must PRINT a reason — and the PEG one must quote a PEG, not a raw peg_yield,
    // because the `peg` COLUMN is computed differently and a silent cut would read as a bug.
    with_peg(&mut pq, Some(40.0), Some(5.0));
    let why = gate_failures(&pq, &peg_t).unwrap();
    assert!(why.iter().any(|(g, m, _)| *g == "peg" && m.contains("PEG 2.50")), "PEG rejection must print the PEG: {why:?}");
    with_peg(&mut pq, None, Some(-1.0));
    assert!(gate_failures(&pq, &peg_t).unwrap().iter().any(|(g, m, _)| *g == "peg" && m.contains("loss-making")));
    let why_m = gate_failures(&nq, &nm_t).unwrap();
    assert!(why_m.iter().any(|(g, m, _)| *g == "margin" && m.contains("7.5%")), "margin rejection must print a reason: {why_m:?}");
    let liq_t = BuyHeuristic { min_avg_turnover_eur: 1_000_000.0, ..BuyHeuristic::default() };
    let mut thin = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    thin.avg_turnover_eur = Some(1_000.0);
    assert!(buy_score(&thin, &liq_t).is_none()); // below liquidity floor
    thin.avg_turnover_eur = Some(5_000_000.0);
    assert!(buy_score(&thin, &liq_t).is_some());
    thin.avg_turnover_eur = None; // unknown turnover not punished
    assert!(buy_score(&thin, &liq_t).is_some());
    assert!(buy_score(&quote(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("1M", -30.0)]), &tuning).is_none()); // equity knife
    assert!(buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", -3.0)]), &tuning).is_none()); // neg >2Y -> excluded
    assert!(buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", -5.0)]), &tuning).is_none()); // every leg must hold
    assert!(buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 80.0), ("20Y", 200.0)]), &tuning).is_some());
    assert!(buy_score(&quote(5.0, &[("1Y", -5.0), ("5Y", 40.0)]), &tuning).is_none()); // declining year
    assert!(buy_score(&quote(30.0, &[("1Y", -40.0), ("5Y", 40.0), ("10Y", 40.0)]), &tuning).is_none()); // equity 1Y floor
    let mut cr = quote(30.0, &[("1Y", -40.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cr.ticker = "BTC-USD".into();
    assert!(buy_score(&cr, &tuning).is_some()); // crypto looser 1Y floor
    assert!(buy_score(&quote(5.0, &[("5Y", 40.0)]), &tuning).is_none()); // no 1Y data
    assert!(buy_score(&Quote::stub("X", "err", "", "X"), &tuning).is_none()); // err row

    // (B) near-miss diagnostic: a name rejected on EXACTLY one growth gate is surfaced; 0 or ≥2 -> None.
    // cagr(200%,10y)≈11.6%/yr (>8 floor); cagr(40%,10y)≈3.4%/yr (<floor). range_pct = 100-drawdown.
    assert!(growth_near_miss(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]), &tuning).is_none()); // clears every gate
    assert_eq!(growth_near_miss(&quote(25.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]), &tuning).map(|(g, _)| g), Some("range")); // only range fails (75<80)
    assert!(growth_near_miss(&quote(25.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &tuning).is_none()); // range AND cagr -> two gates, not a near-miss
    assert!(growth_near_miss(&quote(5.0, &[("1Y", -50.0), ("5Y", 40.0), ("10Y", 200.0)]), &tuning).is_none()); // fails ONLY 1Y+ but by 50pts -> gross reject, not a near-miss

    // (C) the TWO-gate sibling — the boundary the second screen tail turns on. Exactly 2, BOTH close.
    // cagr(97%,10y)≈7.0%/yr: 1pp under the 8 floor = close; cagr(40%,10y)≈3.4%/yr = a gross miss.
    let two = |dd: f64, c10: f64| growth_n_gate_miss(&quote(dd, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", c10)]), &tuning, 2);
    assert_eq!(two(25.0, 97.0).map(|(p, _)| p), Some(vec!["range", "cagr"]), "2 close gates -> surfaced, in gate order");
    assert!(two(25.0, 97.0).unwrap().1.contains("range:") && two(25.0, 97.0).unwrap().1.contains("cagr:"), "both reasons printed");
    assert!(two(25.0, 40.0).is_none(), "range close but cagr grossly missed -> a hard reject, not a near miss");
    assert!(two(25.0, 200.0).is_none(), "ONE gate belongs to the block above, not this one");
    assert!(two(5.0, 200.0).is_none(), "clears everything -> it ranks");
    // three gates -> its OWN tail (n=3), and in neither of the two above: the arity is the parameter,
    // and a block per arity is a block per recovery cost (3 close gates = 3 knobs).
    let mut three = quote(25.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 97.0)]);
    three.age_years = Some(4.0);
    let age3 = BuyHeuristic { growth_min_age_years: 5.0, ..BuyHeuristic::default() };
    assert_eq!(gate_failures(&three, &age3).unwrap().len(), 3, "young + range + cagr");
    let hit3 = growth_n_gate_miss(&three, &age3, 3).expect("3 close gates -> the n=3 tail");
    assert_eq!(hit3.0, vec!["young", "range", "cagr"], "every failing gate named, in gate order");
    assert!(hit3.1.contains("young:") && hit3.1.contains("range:") && hit3.1.contains("cagr:"), "all three reasons printed: {}", hit3.1);
    assert!(growth_n_gate_miss(&three, &age3, 2).is_none() && growth_near_miss(&three, &age3).is_none());
    // ...but a GROSS miss among the three is a hard reject, not a near miss — the whole content of the
    // "every failure must be close" rule, and nothing else pins it.
    let mut gross3 = quote(25.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // cagr 3.4%/yr vs 8 = gross
    gross3.age_years = Some(4.0);
    assert_eq!(gate_failures(&gross3, &age3).unwrap().len(), 3, "same three gates, one missed grossly");
    assert!(growth_n_gate_miss(&gross3, &age3, 3).is_none());

    // --- (#33) minimum-age gate (backtest-blind: age_years None -> pass; live -> gate) ---
    let age_t = BuyHeuristic { growth_min_age_years: 5.0, ..BuyHeuristic::default() };
    // clears every other gate; only age varies
    let aged = |age: Option<f64>| { let mut q = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]); q.age_years = age; q };
    assert!(growth_score(&aged(Some(2.0)), &tuning).is_some()); // gate OFF (default 0) -> young name still scored
    let young = aged(Some(2.0));
    assert!(growth_score(&young, &age_t).is_none()); // gate ON -> under floor -> excluded
    assert!(gate_failures(&young, &age_t).unwrap().iter().any(|(g, _, _)| *g == "young")); // ...with an explicit reason
    assert!(growth_score(&aged(Some(9.0)), &age_t).is_some()); // old enough -> scored
    assert!(growth_score(&aged(None), &age_t).is_some()); // unknown age -> can't judge -> pass (backtest quotes)
    // young name WITHOUT a CAGR leg: the silent long_leg_fixed None becomes an explicit reason.
    let mut young_noleg = quote(5.0, &[("1Y", 10.0)]);
    young_noleg.age_years = Some(2.0);
    assert!(growth_score(&young_noleg, &age_t).is_none());
    assert!(gate_failures(&young_noleg, &age_t).unwrap().iter().any(|(g, _, _)| *g == "young")); // age gate ON -> "young"
    // age gate OFF (default): the no-leg name is still explained, now as "history" (not a silent None ->
    // a pinned young ETF like VUAA/SPYL prints a reason instead of a mystery 0.0)
    let hist = gate_failures(&young_noleg, &tuning).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].0, "history");
    assert!(hist[0].1.contains("2y listed"));
    // a no-leg name with unknown age (backtest-style, but such quotes usually HAVE legs) -> still explained
    let mut noage_noleg = quote(5.0, &[("1Y", 10.0)]);
    noage_noleg.age_years = None;
    assert_eq!(gate_failures(&noage_noleg, &tuning).unwrap()[0].0, "history");
    // gate_review_lines: a failing name yields one TICKER row, a clean name yields nothing
    let review = gate_review_lines(&[&young], &age_t, 8);
    assert_eq!(review.len(), 1);
    assert!(review[0].contains("young"));
    assert!(gate_review_lines(&[&aged(Some(9.0))], &tuning, 8).is_empty());
    // (N) the footer surfaces `is_close`, which it used to compute and discard. Pinned names never reach
    // the near-miss tail (screen.rs skips them to stop the same ticker printing twice), so this line is
    // the ONLY place a one-notch pinned name can say so. Range is the cleanest lever: floor 80, close
    // margin 10 -> 25% off the high is narrow (range 75), 45% off is not (range 55), and both fail range
    // ALONE so the marker isn't reading some other gate.
    let legs = [("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)];
    let narrow_q = gate_review_lines(&[&quote(25.0, &legs)], &tuning, 8);
    assert_eq!(narrow_q.len(), 1);
    assert!(narrow_q[0].contains("range") && narrow_q[0].contains("(narrow"), "a one-notch miss must say so: {}", narrow_q[0]);
    let gross_q = gate_review_lines(&[&quote(45.0, &legs)], &tuning, 8);
    assert_eq!(gross_q.len(), 1);
    assert!(!gross_q[0].contains("(narrow"), "55% in range is a hard reject, not one knob away: {}", gross_q[0]);
    // ALL failing gates must be close, not merely one of them — a name narrow on range and gross on the
    // CAGR leg costs two knobs, so it is not "loosen if wanted". (Two fails also keep it out of the
    // near-miss tail, which needs exactly one; the two-gate tail is where that cohort lives.)
    let mixed = quote(25.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert_eq!(gate_failures(&mixed, &tuning).unwrap().len(), 2, "fixture must fail range AND cagr");
    assert!(!gate_review_lines(&[&mixed], &tuning, 8)[0].contains("(narrow"));

    // --- (history_proxy hints) young ETF + older fund on the IDENTICAL benchmark -> one suggest-only line ---
    let mut yng = quote(5.0, &[("1Y", 10.0)]); // no 5y+ leg -> the "history" fail a twin can repair
    yng.ticker = "YNG.DE".into();
    yng.instrument_type = "ETF".to_string();
    yng.age_years = Some(2.0);
    yng.benchmark = Some("x index".to_string());
    let mut old = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]); // real long record
    old.ticker = "OLD.DE".into();
    old.instrument_type = "ETF".to_string();
    old.age_years = Some(15.0);
    old.benchmark = Some("x index".to_string());
    let hints = bridge_hint_lines(&[&yng], std::slice::from_ref(&old), &tuning);
    assert_eq!(hints.len(), 1);
    assert!(hints[0].contains("YNG.DE") && hints[0].contains("OLD.DE") && hints[0].contains("x index"));
    // already bridged -> silent (the hint's job is done)
    let mut proxied = yng.clone();
    proxied.history_proxied = true;
    assert!(bridge_hint_lines(&[&proxied], std::slice::from_ref(&old), &tuning).is_empty());
    // twin tracks a DIFFERENT index -> no true twin -> silent (hedged share classes land here)
    let mut other_idx = old.clone();
    other_idx.benchmark = Some("y index".to_string());
    assert!(bridge_hint_lines(&[&yng], &[other_idx], &tuning).is_empty());
    // twin without its own long record can't lend one -> silent
    let mut recordless = old.clone();
    recordless.perf = yng.perf.clone();
    assert!(bridge_hint_lines(&[&yng], &[recordless], &tuning).is_empty());

    // --- (AUM) ETF minimum fund-size gate (backtest-blind: aum_eur None -> pass; ETF-only) ---
    let aum_t = BuyHeuristic { growth_min_aum_etf: 100e6, ..BuyHeuristic::default() };
    // clears every other gate; only class + AUM vary
    let fund = |aum: Option<f64>, etf: bool| {
        let mut q = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]);
        q.aum_eur = aum;
        if etf { q.instrument_type = "ETF".to_string(); }
        q
    };
    assert!(growth_score(&fund(Some(45e6), true), &tuning).is_some()); // gate OFF (default 0) -> tiny fund still scored
    let tiny = fund(Some(45e6), true);
    assert!(growth_score(&tiny, &aum_t).is_none()); // gate ON -> sub-scale ETF excluded
    let aum_fail = gate_failures(&tiny, &aum_t).unwrap();
    let (_, why, close) = aum_fail.iter().find(|(g, _, _)| *g == "aum").unwrap();
    assert!(why.contains("€45M") && why.contains("€100M") && why.contains("liquidation")); // human why with both sides
    assert!(!close); // €45M vs a €100M floor is under half -> a hard reject, not a near miss
    assert!(gate_failures(&fund(Some(60e6), true), &aum_t).unwrap().iter().any(|(g, _, c)| *g == "aum" && *c)); // ≥ half the floor -> close miss
    assert!(growth_score(&fund(Some(8e9), true), &aum_t).is_some()); // big fund passes
    assert!(growth_score(&fund(None, true), &aum_t).is_some()); // unknown AUM -> can't judge -> pass (backtest + off-BF names)
    assert!(growth_score(&fund(Some(45e6), false), &aum_t).is_some()); // non-ETF never gated on AUM

    // --- (#34) TER cost drag (ETF-only via expense_ratio; backtest-blind: None -> ×1.0) ---
    let ter_t = BuyHeuristic { growth_ter_drag: true, ..BuyHeuristic::default() };
    let etf = |ter: Option<f64>| { let mut q = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]); q.expense_ratio = ter; q };
    let undamped = growth_score(&etf(None), &ter_t).unwrap(); // no TER -> ×1.0 even with drag on
    assert_eq!(growth_score(&etf(Some(0.65)), &tuning).unwrap(), undamped); // drag OFF -> TER ignored, byte-identical
    let cheap = growth_score(&etf(Some(0.10)), &ter_t).unwrap();
    let dear = growth_score(&etf(Some(0.65)), &ter_t).unwrap();
    assert!(undamped > cheap && cheap > dear, "higher TER docks the score more: {undamped} / {cheap} / {dear}");
    assert!((cheap / undamped - 0.999_f64.powi(20)).abs() < 1e-9); // 0.10% TER over 20y ≈ ×0.980

    // --- (#44) commodity dock (GICS Energy/Materials + commodity-named funds; BACKTEST-BLIND) ---
    let comm_t = BuyHeuristic { growth_commodity_damp: 0.8, ..BuyHeuristic::default() };
    let sectored = |sector: Option<&str>| {
        let mut q = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]);
        q.sector = sector.map(str::to_string);
        q.avg_turnover_eur = Some(5e8); // under €1B -> liq_bonus 0, so the ratio below is the damp EXACTLY
        q
    };
    let plain = growth_score(&sectored(Some("Information Technology")), &tuning).unwrap();
    assert_eq!(growth_score(&sectored(Some("Energy")), &tuning).unwrap(), plain); // knob off (1.0) -> byte-identical
    assert_eq!(growth_score(&sectored(Some("Information Technology")), &comm_t).unwrap(), plain); // non-commodity untouched
    // sector unknown = the backtest pool's state -> damp inert. THIS is why the knob can never be swept.
    assert_eq!(growth_score(&sectored(None), &comm_t).unwrap(), plain);
    let docked = growth_score(&sectored(Some("Energy")), &comm_t).unwrap();
    assert!((docked / plain - 0.8).abs() < 1e-9, "liq_bonus 0 -> the ratio IS the damp: {docked} / {plain}");
    assert!((growth_score(&sectored(Some("materials")), &comm_t).unwrap() / plain - 0.8).abs() < 1e-9); // case-insensitive
    // this knob INVERTS the house `0 = off` convention (it multiplies), so a user reaching for 0 must
    // get "off", never a silent zeroing of every Energy row out of the table.
    let zeroed = BuyHeuristic { growth_commodity_damp: 0.0, ..BuyHeuristic::default() };
    assert_eq!(growth_score(&sectored(Some("Energy")), &zeroed).unwrap(), plain);

    // --- (#45) FX/venue dock (ETF + non-EUR quote currency; BACKTEST-BLIND: currency None -> ×1.0) ---
    let fx_t = BuyHeuristic { growth_fx_damp: 0.98, ..BuyHeuristic::default() };
    let listed = |ccy: Option<&str>, is_fund: bool| {
        let mut q = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 200.0)]);
        if is_fund {
            q.instrument_type = "ETF".to_string();
        }
        q.quote_currency = ccy.map(str::to_string);
        q.avg_turnover_eur = Some(5e8); // under €1B -> liq_bonus 0, so the ratio below is the damp EXACTLY
        q
    };
    let plain_etf = growth_score(&listed(Some("EUR"), true), &fx_t).unwrap();
    assert_eq!(growth_score(&listed(Some("GBp"), true), &tuning).unwrap(), plain_etf); // knob off (1.0) -> byte-identical
    // currency unknown = every backtest quote's state -> damp inert. THIS is why the knob can never be swept.
    assert_eq!(growth_score(&listed(None, true), &fx_t).unwrap(), plain_etf);
    assert!((growth_score(&listed(Some("GBp"), true), &fx_t).unwrap() / plain_etf - 0.98).abs() < 1e-9); // LSE pence line docked
    assert!((growth_score(&listed(Some("SEK"), true), &fx_t).unwrap() / plain_etf - 0.98).abs() < 1e-9);
    assert_eq!(growth_score(&listed(Some("eur"), true), &fx_t).unwrap(), plain_etf); // any casing of EUR is home
    // a stock is never docked no matter its currency — the lane is all-USD, a uniform dock reorders nothing
    assert_eq!(growth_score(&listed(Some("USD"), false), &fx_t).unwrap(), growth_score(&listed(Some("USD"), false), &tuning).unwrap());
    let fx_zero = BuyHeuristic { growth_fx_damp: 0.0, ..BuyHeuristic::default() };
    assert_eq!(growth_score(&listed(Some("GBp"), true), &fx_zero).unwrap(), plain_etf); // 0 = ALSO off, same inversion guard as (#44)
    // (round 47) THE fallback invariant: Yahoo-sourced facts must never move the score — a first
    // merged implementation leaked them into ter_damp and shifted live ranks (PEA 3->9).
    let mut yh = etf(None);
    yh.ter_fallback = Some(0.65);
    yh.aum_fallback = Some(5e8);
    assert_eq!(growth_score(&yh, &ter_t).unwrap(), undamped); // fallback TER invisible to the drag

    // --- SCORE (relational, robust to knob tuning) ---
    // trust: same inputs, the one missing a 10Y record scores lower (uptrend less proven)
    let with10 = buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &tuning).unwrap();
    let no10 = buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0)]), &tuning).unwrap();
    assert!(with10 > no10);
    // discount caps: an 80% drawdown doesn't score below a 5% one, all else equal
    let deep = buy_score(&quote(80.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &tuning).unwrap();
    let shallow = buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &tuning).unwrap();
    assert!(deep >= shallow);
    // (A) discount keys off range position: same drawdown, the one deeper in its own range
    // (lower range_pct) outranks the one near its range high — the fix raw ATH-distance couldn't make
    let mut deep_in_range = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    deep_in_range.range_pct = 20.0; // trades near its 10y low
    let mut near_high = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    near_high.range_pct = 80.0; // trades near its 10y high
    assert!(buy_score(&deep_in_range, &tuning).unwrap() > buy_score(&near_high, &tuning).unwrap());
    // a deep pullback on a healthy long trend beats a rocket at new highs (discount 0)
    let pullback = buy_score(&quote(40.0, &[("1Y", 30.0), ("5Y", 50.0), ("10Y", 50.0)]), &tuning).unwrap();
    let rocket = buy_score(&quote(0.0, &[("1Y", 400.0), ("5Y", 500.0), ("10Y", 500.0)]), &tuning).unwrap();
    assert!(pullback > rocket, "on-sale name must beat the rocket: {pullback} vs {rocket}");
    // #1 end-to-end: same 30% drawdown, the calm (low-vol) name outranks the wild one
    let mut calm = quote(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    calm.volatility_pct = Some(1.0);
    let mut wild = quote(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    wild.volatility_pct = Some(4.0);
    assert!(buy_score(&calm, &tuning).unwrap() > buy_score(&wild, &tuning).unwrap());
    // (2a) at its all-time high (discount ~0) a huge-CAGR name must NOT outrank an equal pulled-back
    // one — the long-trend reward fades without an actual discount (kills the at-the-high "rocket")
    let at_high = quote(0.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 500.0)]); // range_pct 100 -> discount 0
    let pulled = quote(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 500.0)]); // same CAGR, real discount
    assert!(buy_score(&pulled, &tuning).unwrap() > buy_score(&at_high, &tuning).unwrap());
    // (A) a stronger long-term CAGR outranks a weaker one, all else equal
    let strong = buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 400.0)]), &tuning).unwrap();
    let weak = buy_score(&quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &tuning).unwrap();
    assert!(strong > weak);
    // (A) trend_health: 0 at the decay (zero) threshold, 1 at a flat/rising trend
    assert_eq!(trend_health(tuning.health_zero_cagr, tuning.health_zero_cagr), 0.0);
    assert_eq!(trend_health(0.0, tuning.health_zero_cagr), 1.0);
    // (B) sustained-decline dock: 1Y & 5Y both deep-red is docked below an equal coin that's recovering
    let mut bleeder = quote(40.0, &[("1Y", -50.0), ("5Y", -60.0), ("10Y", 200.0)]);
    bleeder.ticker = "LTC-EUR".into();
    let mut recover = quote(40.0, &[("1Y", 20.0), ("5Y", -60.0), ("10Y", 200.0)]);
    recover.ticker = "XYZ-EUR".into();
    assert!(buy_score(&bleeder, &tuning).unwrap() < buy_score(&recover, &tuning).unwrap());
    assert!((sustained_decline_factor(&bleeder, &tuning) - tuning.sustained_decline_penalty).abs() < 1e-9);
    assert_eq!(sustained_decline_factor(&recover, &tuning), 1.0); // positive 1Y -> not a value trap
    // (C) harsher tier: a 5Y past deep_decline_pct (e.g. LTC -73%) docks below the -40% tier
    let deep_bleeder = quote(40.0, &[("1Y", -58.0), ("5Y", -73.0), ("10Y", 282.0)]); // LTC-shaped
    assert!((sustained_decline_factor(&deep_bleeder, &tuning) - tuning.deep_decline_penalty).abs() < 1e-9);
    assert!(tuning.deep_decline_penalty < tuning.sustained_decline_penalty); // tier 2 is harsher
    // (C) sitting below the ~200wk SMA lifts the score
    let mut cheap = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cheap.below_ma_pct = 50.0;
    let dear = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert!(buy_score(&cheap, &tuning).unwrap() > buy_score(&dear, &tuning).unwrap());
    // (D) a dividend payer outranks an otherwise-equal non-payer
    let mut payer = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    payer.price_eur = Some(100.0);
    payer.div_eur = vec![Some(5.0)]; // ~5% trailing-1Y yield (DIV_HORIZONS[0] = 1Y)
    let nonpayer = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert!(dividend_yield_1y(&payer) > 0.0);
    assert!(buy_score(&payer, &tuning).unwrap() > buy_score(&nonpayer, &tuning).unwrap());
    // (E) value tilt: a cheap P/E lifts, a rich one dampens, unknown is neutral (1.0)
    let mut cheap_pe = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cheap_pe.pe_ratio = Some(8.0);
    let mut rich_pe = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    rich_pe.pe_ratio = Some(60.0);
    let neutral_pe = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert!(value_factor(&cheap_pe, tuning.ref_pe) > 1.0 && value_factor(&rich_pe, tuning.ref_pe) < 1.0);
    assert_eq!(value_factor(&neutral_pe, tuning.ref_pe), 1.0);
    assert!(buy_score(&cheap_pe, &tuning).unwrap() > buy_score(&neutral_pe, &tuning).unwrap());
    // (Item 20) the growth-lane P/E-authority dial: weight 1.0 keeps the raw multiplier, 0.0 neutralises it.
    let raw = value_factor(&rich_pe, tuning.ref_pe); // < 1.0
    let dial = |w: f64, v: f64| 1.0 + w * (v - 1.0);
    assert_eq!(dial(1.0, raw), raw); // full authority (default) -> unchanged
    assert_eq!(dial(0.0, raw), 1.0); // off -> neutral, the blind ±50% swing gone
    assert!(dial(0.5, raw) > raw && dial(0.5, raw) < 1.0); // half authority -> between
    assert!(buy_score(&rich_pe, &tuning).unwrap() < buy_score(&neutral_pe, &tuning).unwrap());
    // upside to high: 50% off -> +100% to recover; at the high -> 0; near-total wipeout clamps
    assert!((upside_to_high(50.0) - 100.0).abs() < 1e-9);
    assert_eq!(upside_to_high(0.0), 0.0);
    assert_eq!(upside_to_high(99.5), 9900.0);

    // currency-twin dedup (E): keep the preferred leg, pass other tickers through
    let mut btc_e = quote(10.0, &[("1Y", 5.0), ("5Y", 40.0), ("10Y", 40.0)]);
    btc_e.ticker = "BTC-EUR".into();
    let mut btc_u = quote(10.0, &[("1Y", 5.0), ("5Y", 40.0), ("10Y", 40.0)]);
    btc_u.ticker = "BTC-USD".into();
    let mut aapl = quote(5.0, &[("1Y", 5.0), ("5Y", 40.0), ("10Y", 40.0)]);
    aapl.ticker = "AAPL".into();
    // USD listed first with the higher score, but EUR preferred -> EUR kept; AAPL untouched
    let kept = dedup_currency_twins(vec![(&btc_u, 9.0), (&btc_e, 8.0), (&aapl, 3.0)], true);
    assert_eq!(kept.len(), 2);
    assert!(kept.iter().any(|(x, _)| x.ticker == "BTC-EUR"));
    assert!(!kept.iter().any(|(x, _)| x.ticker == "BTC-USD"));
    // prefer USD instead -> the USD leg wins
    let usd = dedup_currency_twins(vec![(&btc_e, 8.0), (&btc_u, 9.0)], false);
    assert_eq!(usd.len(), 1);
    assert_eq!(usd[0].0.ticker, "BTC-USD");
    // both arms above pass the flag literally; this is the knob `ranked` actually reads (picks.rs `dedup_
    // currency_twins(scored, tuning.prefer_eur)`) — a EUR-domiciled tool defaults to keeping the EUR leg.
    assert!(BuyHeuristic::default().prefer_eur);

    let no_pin: HashSet<&str> = HashSet::new();
    // (B) ranked dedups dual-class share twins by identical company name (GOOG/GOOGL -> one row).
    // googl scores lower (shallower discount) so the higher-scored goog wins the dedup deterministically.
    let mut goog = quote(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // both name "n"
    goog.ticker = "GOOG".into();
    let mut googl = quote(38.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    googl.ticker = "GOOGL".into();
    assert_eq!(ranked(&[goog.clone(), googl.clone()], &tuning, buy_score, tuning.min_score, &no_pin).len(), 1);
    // ...but a PINNED twin is never deduped away (so a pinned ETF survives a same-named higher twin)
    let pin_googl: HashSet<&str> = ["GOOGL"].into_iter().collect();
    let twins = [goog, googl];
    let kept = ranked(&twins, &tuning, buy_score, tuning.min_score, &pin_googl);
    assert!(kept.iter().any(|(x, _)| x.ticker == "GOOGL")); // pinned lower-scored twin still present
    // (A) ranked hides rows scoring at/below min_score (near-the-high padding), keeps real candidates
    let shallow = quote(2.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // tiny discount -> low score
    assert!(buy_score(&shallow, &tuning).unwrap() < tuning.min_score);
    assert!(ranked(std::slice::from_ref(&shallow), &tuning, buy_score, tuning.min_score, &no_pin).is_empty());
    let strong_pick = quote(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // real discount -> kept
    assert_eq!(ranked(std::slice::from_ref(&strong_pick), &tuning, buy_score, tuning.min_score, &no_pin).len(), 1);

    // --- GROWTH LANE (mirror of buy_score): near-high proven compounders the on-sale score drops ---
    // an at-the-high rocket buy_score fades to ~0 (or trims) IS a growth candidate here
    let rocket = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // range_pct 100, strong CAGR, climbing
    assert!(growth_score(&rocket, &tuning).is_some());
    // SINGLE-SOURCE check: the per-term ScoreParts must reconcile to the scalar growth_score exactly,
    // so the `explain_growth_score` worked example can never drift from the ranked number.
    let parts = score_parts(&rocket, &tuning).unwrap();
    let term_sum = parts.trend_term + parts.accel_term + parts.risk_reward + parts.quality
        + parts.dividend + parts.fund + parts.mom121 + parts.smooth;
    assert!((term_sum - parts.base).abs() < 1e-9, "terms must sum to base");
    // Re-run the reconcile with the smoothness term ACTIVE so a term added to base but
    // forgotten in ScoreParts/explain can never pass again (that exact drift shipped once).
    let mut smooth_rocket = rocket.clone();
    smooth_rocket.trend_r2 = 0.9;
    let st = BuyHeuristic { growth_smoothness_weight: 5.0, ..tuning.clone() };
    let sp = score_parts(&smooth_rocket, &st).unwrap();
    let ssum = sp.trend_term + sp.accel_term + sp.risk_reward + sp.quality
        + sp.dividend + sp.fund + sp.mom121 + sp.smooth;
    assert!((ssum - sp.base).abs() < 1e-9, "terms must sum to base with smoothness on");
    assert!((sp.smooth - 4.5).abs() < 1e-9, "smooth part must carry the E term");
    let recomposed = parts.base * parts.proximity * parts.value * parts.damp + parts.liq_bonus;
    assert!((recomposed - parts.score).abs() < 1e-9, "formula must reproduce score");
    // (#8) fold path: score must equal base × geomean(trust, overext, proximity, value) + liq_bonus.
    let mut folded = tuning.clone();
    folded.growth_geomean_fold = true;
    let fp = score_parts(&rocket, &folded).unwrap();
    let expect = fp.base * combine_damps(&[fp.trust, fp.overext_damp, fp.proximity, fp.value]) + fp.liq_bonus;
    assert!((fp.score - expect).abs() < 1e-9, "#8 fold formula must reproduce score");
    assert_eq!(parts.score, growth_score(&rocket, &tuning).unwrap(), "ScoreParts.score == growth_score");
    assert!(explain_growth_score(&rocket, &tuning, parts.score).is_some());
    // ...and ranked picks it up where the on-sale lane (min_score) would have trimmed an at-high name
    assert_eq!(ranked(std::slice::from_ref(&rocket), &tuning, growth_score, tuning.growth_min_score, &no_pin).len(), 1);
    // a deeply pulled-back name is NOT a growth candidate (that's the on-sale lane's job)
    let dipped = quote(40.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // range_pct 60 < growth_min_range_pct
    assert!(growth_score(&dipped, &tuning).is_none());
    // weak long trend -> an expensive laggard, not a proven compounder -> excluded
    let laggard = quote(0.0, &[("1Y", 3.0), ("5Y", 6.0), ("10Y", 10.0)]);
    assert!(growth_score(&laggard, &tuning).is_none());
    // PINNED overlay (mirrors render's scorer): a gated name still scores (sentinel) when pinned, so it
    // survives to the table; a non-pinned gated name stays excluded. (quote() sets ticker "T".)
    let pin_scored = |pinned: bool| {
        growth_score(&laggard, &tuning).or_else(|| pinned.then_some(f64::MIN_POSITIVE))
    };
    assert!(pin_scored(true).is_some()); // pinned -> shown despite the gate
    assert!(pin_scored(false).is_none()); // not pinned -> still excluded
    // no real multi-year leg (1Y only) -> NOT a "proven long-term CAGR" candidate, even for crypto
    // (kills the no-history token junk: microNFT, freshly-listed +100000% data artifacts)
    let mut nohist = quote(0.0, &[("1Y", 700.0)]); // huge 1Y, but no 5Y/10Y/20Y leg
    nohist.ticker = "MNT-USD".into();
    assert!(growth_score(&nohist, &tuning).is_none());
    // not climbing this year (negative 1Y) -> no momentum -> excluded
    assert!(growth_score(&quote(0.0, &[("1Y", -5.0), ("5Y", 200.0), ("10Y", 500.0)]), &tuning).is_none());
    // crashing this month -> momentum broke -> excluded
    assert!(growth_score(&quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0), ("1M", -30.0)]), &tuning).is_none());
    // leveraged/stablecoin still excluded in this lane too
    let mut lev_g = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    lev_g.name = "Direxion Daily Technology".into();
    assert!(growth_score(&lev_g, &tuning).is_none());
    // (#20) UNKNOWN turnover -> excluded from the growth lane even with NO floor (untradeable artifact
    // like 0Y72.L); a known turnover is admitted, and dropped only when it's below a configured floor.
    let mut noturn = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    noturn.avg_turnover_eur = None;
    assert!(growth_score(&noturn, &tuning).is_none()); // unknown turnover, no floor -> still excluded
    noturn.avg_turnover_eur = Some(5_000_000.0);
    assert!(growth_score(&noturn, &tuning).is_some()); // known turnover, no floor -> admitted
    let liq_g = BuyHeuristic { min_avg_turnover_eur: 10_000_000.0, ..BuyHeuristic::default() };
    assert!(growth_score(&noturn, &liq_g).is_none()); // known €5M but below the €10M floor -> excluded
    // (#23) degenerate single-bar series: identical 1D=1W=1M cumulative returns = a listing that
    // repriced once (0Y72.L's +212.9%), not a trend -> excluded even with good turnover + CAGR.
    let artifact = quote(0.0, &[("1D", 212.9), ("1W", 212.9), ("1M", 212.9), ("1Y", 205.9), ("5Y", 147.0), ("10Y", 300.0)]);
    assert!(growth_score(&artifact, &tuning).is_none());
    // a real continuous series with the SAME long trend but distinct near-term legs -> still admitted.
    let real = quote(0.0, &[("1D", 1.4), ("1W", 2.3), ("1M", 8.0), ("1Y", 205.9), ("5Y", 147.0), ("10Y", 300.0)]);
    assert!(growth_score(&real, &tuning).is_some());
    // acceleration: same long CAGR, the name whose recent year OUTPACES it scores higher (momentum)
    let accel = growth_score(&quote(0.0, &[("1Y", 80.0), ("5Y", 100.0), ("10Y", 150.0)]), &tuning).unwrap();
    let steady = growth_score(&quote(0.0, &[("1Y", 15.0), ("5Y", 100.0), ("10Y", 150.0)]), &tuning).unwrap();
    assert!(accel > steady);
    // BTC-relative crypto tilt: beat BTC -> boost, == BTC -> neutral 1.0x, lag -> dock (bounded 0.5x..2x)
    assert!((btc_relative(Some(50.0), Some(20.0), 10.0, 0.3) - 10.9).abs() < 1e-9); // +30pp over BTC -> ×1.09
    assert_eq!(btc_relative(Some(20.0), Some(20.0), 10.0, 0.3), 10.0); // == BTC -> base 1.0x
    assert!(btc_relative(Some(-90.0), Some(60.0), 10.0, 0.3) >= 5.0); // big lag clamped at the 0.5x floor
    assert_eq!(btc_relative(Some(50.0), None, 10.0, 0.3), 10.0); // no BTC base -> unchanged
    assert_eq!(btc_relative(Some(50.0), Some(20.0), 10.0, 0.0), 10.0); // weight 0 -> tilt off
    // (E) a nosebleed P/E damps the growth score (anti top-chase), an unknown PE stays neutral
    let mut rich_g = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    rich_g.pe_ratio = Some(80.0);
    assert!(growth_score(&rich_g, &tuning).unwrap() < growth_score(&rocket, &tuning).unwrap());
    // (1) overextension brake: a name run far ABOVE its 200wk SMA scores below an at-trend twin
    let mut stretched = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    stretched.above_ma_pct = 100.0; // maximally stretched
    assert!(growth_score(&stretched, &tuning).unwrap() < growth_score(&rocket, &tuning).unwrap());
    // (L) liquidity tilt: a deep-liquid stretched compounder (NVDA case) outscores an illiquid twin —
    // the bonus is added OUTSIDE the brake, so the parabolic stretch can't bury it under a thin name.
    let mut liquid = stretched.clone();
    liquid.avg_turnover_eur = Some(32e9); // €32B (NVDA-class)
    let mut illiquid = stretched.clone();
    illiquid.avg_turnover_eur = Some(2e8); // €200M, below the €1B floor -> no bonus
    assert!(growth_score(&liquid, &tuning).unwrap() > growth_score(&illiquid, &tuning).unwrap());
    assert!((core::above_long_ma_pct(&[50.0, 50.0, 100.0], 3) - 50.0).abs() < 1e-9); // 100 vs mean 66.67
    assert_eq!(core::above_long_ma_pct(&[100.0, 100.0, 50.0], 3), 0.0); // below the mean -> 0
    // (3) consistency: a near-high equity negative over 5Y (mooned-then-bled) is rejected despite a fat 10Y
    assert!(growth_score(&quote(0.0, &[("1Y", 60.0), ("5Y", -20.0), ("10Y", 500.0)]), &tuning).is_none());
    // ...and since 2026-07-27, so is a COIN. This bar used to carry a `!crypto` guard on the view
    // that a -EUR 5Y is peak-anchored noise (the FOIL lane still holds it, via its own looser
    // `min_long_pct_crypto`). The growth lane now runs ONE floor for every lane, by request — so
    // this fixture flipped from ranked to gated. The flip IS the change; it is not a regression.
    let mut bled_crypto = quote(0.0, &[("1Y", 60.0), ("5Y", -20.0), ("10Y", 500.0)]);
    bled_crypto.ticker = "ETH-EUR".into();
    assert!(growth_score(&bled_crypto, &tuning).is_none());
    // the escape hatch that replaces the guard. It USED to be the shared `growth_min_5y_pct`, which
    // meant re-admitting a healthy coin mid-drawdown also loosened equities; since 2026-08-03 the two
    // are separate knobs, so the hatch is the crypto twin and the equity bar is free to move on its
    // own measured optimum. Assert it works before anyone needs it at 3am.
    let loose5 = BuyHeuristic { growth_min_5y_pct_crypto: -50.0, ..BuyHeuristic::default() };
    assert!(growth_score(&bled_crypto, &loose5).is_some());
    // ...and the split is REAL in both directions: the equity knob no longer reaches a coin (this is
    // the whole point of the twin — a +75 equity floor must not empty the crypto table), and the
    // crypto knob no longer reaches an equity.
    let equity_only = BuyHeuristic { growth_min_5y_pct: -50.0, ..BuyHeuristic::default() };
    assert!(growth_score(&bled_crypto, &equity_only).is_none(), "equity 5Y floor must not move a coin");
    let bled_equity = quote(0.0, &[("1Y", 60.0), ("5Y", -20.0), ("10Y", 500.0)]);
    let crypto_only = BuyHeuristic { growth_min_5y_pct_crypto: -50.0, ..BuyHeuristic::default() };
    assert!(growth_score(&bled_equity, &crypto_only).is_none(), "crypto 5Y floor must not move an equity");
    // (4) NUPL factor: symmetric. euphoria (high NUPL) shrinks <1; capitulation (low NUPL) boosts >1;
    // neutral band / unknown = exactly 1.0.
    assert_eq!(nupl_factor(None, &tuning), 1.0);
    assert!(nupl_factor(Some(0.40), &tuning) == 1.0); // between capitulation (0.25) and euphoria (0.5) -> neutral
    assert!(nupl_factor(Some(0.75), &tuning) < 1.0 && nupl_factor(Some(0.75), &tuning) > tuning.nupl_damp_floor);
    assert!((nupl_factor(Some(1.0), &tuning) - tuning.nupl_damp_floor).abs() < 1e-9); // peak euphoria -> floor
    assert!(nupl_factor(Some(0.0), &tuning) > 1.0); // deep capitulation -> boost
    assert!((nupl_factor(Some(0.0), &tuning) - tuning.nupl_boost_ceiling).abs() < 1e-9); // NUPL 0 -> ceiling
    assert_eq!(nupl_factor(Some(tuning.nupl_euphoria), &tuning), 1.0); // AT the band edge is not past it (`>`)
    assert!(nupl_factor(Some(-0.5), &tuning) <= tuning.nupl_boost_ceiling + 1e-9); // deep-bear readings clamp
    // each half independently disable-able — euphoria >= 1.0 kills the damp, capitulation <= 0 the boost.
    assert_eq!(nupl_factor(Some(0.9), &BuyHeuristic { nupl_euphoria: 1.0, ..tuning.clone() }), 1.0);
    assert_eq!(nupl_factor(Some(0.1), &BuyHeuristic { nupl_capitulation: 0.0, ..tuning.clone() }), 1.0);
    // (Item 17) crypto_adjust: equities pass through untouched (cfactor ignored); crypto is scaled by the
    // whole-market cfactor (btc_1y None -> btc_relative no-op, so the result isolates the NUPL scale). This
    // is what `size` must apply too, or its crypto sizes diverge from the screen tables.
    let equity = quote(5.0, &[("1Y", 20.0)]); // ticker "T" -> not currency-quoted
    assert_eq!(crypto_adjust(&equity, 10.0, &tuning, 0.5, Some(20.0)), 10.0); // equity: cfactor has no effect
    let mut coin = quote(5.0, &[("1Y", 20.0)]);
    coin.ticker = "BTC-EUR".into();
    assert!((crypto_adjust(&coin, 10.0, &tuning, 0.5, None) - 5.0).abs() < 1e-9); // crypto: 10 * cfactor 0.5
    // ...and the OTHER half of the crypto tilt: growth_btc_outperf_weight, read against Bitcoin's own year.
    // BTC vs itself is the neutral anchor (edge 0) whatever the weight; the bound is what stops one
    // moonshot running away with the lane. Backtest-blind (crypto is dropped before every edge metric).
    let tilt = |w: f64, coin_1y: f64| {
        let mut alt = quote(5.0, &[("1Y", coin_1y)]);
        alt.ticker = "ETH-EUR".into();
        crypto_adjust(&alt, 10.0, &BuyHeuristic { growth_btc_outperf_weight: w, ..tuning.clone() }, 1.0, Some(20.0))
    };
    assert_eq!(tilt(0.0, 80.0), 10.0); // 0 = off: a +60pp outperformer is not tilted at all
    assert!(tilt(1.0, 80.0) > 10.0 && tilt(1.0, -20.0) < 10.0); // beats BTC -> up; lags -> docked, not zeroed
    assert_eq!(tilt(1.0, 20.0), 10.0); // exactly BTC's year = the neutral anchor
    assert!((tilt(5.0, 500.0) - 20.0).abs() < 1e-9); // clamp 2.0x: one moonshot can't run away
    assert!((tilt(5.0, -100.0) - 5.0).abs() < 1e-9); // clamp 0.5x: a laggard is docked, never zeroed

    // --- (A) trend consistency: R² of the log-price line, damps CAGR endpoint-luck ---
    assert!(core::trend_r2(&[1.0, 2.0, 4.0, 8.0, 16.0]) > 0.999); // perfect exponential -> R²≈1
    assert!(core::trend_r2(&[1.0, 100.0, 2.0, 200.0, 3.0]) < 0.5); // zigzag -> lumpy
    assert_eq!(core::trend_r2(&[5.0]), 0.0); // too short
    // (C) max drawdown: worst peak-to-trough
    assert!((core::max_drawdown_pct(&[100.0, 50.0, 75.0]) - 50.0).abs() < 1e-9);
    assert_eq!(core::max_drawdown_pct(&[1.0, 2.0, 3.0]), 0.0); // monotone up -> never down
    // (B) risk_bonus: same CAGR, the lower-volatility name earns a bigger Sharpe-ish bonus
    assert!(risk_bonus(&{ let mut x = quote(5.0, &[]); x.volatility_pct = Some(1.0); x }, 20.0, tuning.sharpe_weight, tuning.calmar_weight, &tuning)
        > risk_bonus(&{ let mut x = quote(5.0, &[]); x.volatility_pct = Some(4.0); x }, 20.0, tuning.sharpe_weight, tuning.calmar_weight, &tuning));
    // (C) risk_bonus: same CAGR, the SHALLOWER max-drawdown name earns a bigger Calmar bonus (calmar_weight default 1.0)
    assert!(risk_bonus(&{ let mut x = quote(5.0, &[]); x.max_drawdown_pct = 20.0; x }, 20.0, tuning.sharpe_weight, tuning.calmar_weight, &tuning)
        > risk_bonus(&{ let mut x = quote(5.0, &[]); x.max_drawdown_pct = 90.0; x }, 20.0, tuning.sharpe_weight, tuning.calmar_weight, &tuning));
    // (B) per-lane Sharpe split: zeroing the on-sale weight drops the on-sale risk bonus to 0 while the
    // growth weight still rewards the same name (the conflict the split exists to resolve).
    let calm = { let mut x = quote(5.0, &[]); x.volatility_pct = Some(1.0); x };
    assert_eq!(risk_bonus(&calm, 20.0, 0.0, 0.0, &tuning), 0.0);
    assert!(risk_bonus(&calm, 20.0, tuning.sharpe_weight, 0.0, &tuning) > 0.0);

    // (A) crypto trust: a young EUR pair (5Y but no 10Y, like BTC-EUR) is NOT halved — 5Y is proven
    // enough for crypto; an equity still needs a 10Y leg, a barely-listed coin (1Y only) is still cut.
    assert!((trust_factor(&quote(20.0, &[("1Y", 30.0), ("5Y", 200.0)]), true, 0, false, 0.5) - 1.0).abs() < 1e-9);
    assert_eq!(trust_factor(&quote(20.0, &[("1Y", 30.0)]), true, 0, false, 0.5), 0.5); // crypto, only 1Y -> unproven
    assert_eq!(trust_factor(&quote(5.0, &[("5Y", 40.0)]), false, 0, false, 0.5), 0.5); // equity, no 10Y -> halved
    assert!((trust_factor(&quote(5.0, &[("10Y", 40.0)]), false, 0, false, 0.5) - 1.0).abs() < 1e-9);
    // (#15) the trust leg follows the pinned window: under an 8Y view an 8Y record is a FULL record
    // (not halved), while a name that can't even show 8Y still is. fixed_years=0 keeps the 10Y rule,
    // which is what makes the live ranking (fixed_cagr_years: 0) bit-identical to before the pin existed.
    assert!((trust_factor(&quote(5.0, &[("8Y", 120.0)]), false, 8, false, 0.5) - 1.0).abs() < 1e-9);
    assert_eq!(trust_factor(&quote(5.0, &[("8Y", 120.0)]), false, 0, false, 0.5), 0.5); // same name, unpinned -> no 10Y -> halved
    assert_eq!(trust_factor(&quote(5.0, &[("5Y", 40.0)]), false, 8, false, 0.5), 0.5); // pinned, but no 8Y leg either
    // end-to-end: a 5Y-only crypto (BTC-EUR shape) is admitted to the growth lane and NOT trust-halved
    let mut btc_young = quote(20.0, &[("1Y", 30.0), ("5Y", 200.0)]); // no 10Y leg, like the young EUR pair
    btc_young.ticker = "BTC-EUR".into();
    assert!((trust_factor(&btc_young, true, 0, false, 0.5) - 1.0).abs() < 1e-9);
    assert!(growth_score(&btc_young, &tuning).is_some());

    // (#47) the graded record ladder. Longest leg present wins; `fixed_years` does NOT pin it (the
    // ladder asks how long the record IS, a fact about the name, not about the view chosen for it).
    let rungs = [
        (vec![("1Y", 10.0), ("5Y", 40.0), ("8Y", 90.0), ("20Y", 900.0)], 1.0),
        (vec![("1Y", 10.0), ("5Y", 40.0), ("8Y", 90.0)], 0.85),
        (vec![("1Y", 10.0), ("5Y", 40.0)], 0.70),
        (vec![("1Y", 10.0), ("2Y", 20.0)], 0.5),  // (#49) 2Y rung = `young`
        (vec![("1Y", 10.0)], 0.25),               // (#49) 1Y rung = `young`/2 — was a flat 0.5 pre-(#49)
    ];
    for (legs, want) in &rungs {
        let q = quote(20.0, legs);
        assert!((trust_factor(&q, false, 0, true, 0.5) - want).abs() < 1e-9, "equity rung {legs:?} -> {want}");
        // ONE SHARED LADDER: a coin answers to the same rungs, unlike the cliff arm where crypto has
        // its own 5Y rule. This is the demotion the plan accepted, pinned so it can't drift back.
        assert!((trust_factor(&q, true, 0, true, 0.5) - want).abs() < 1e-9, "crypto rung {legs:?} -> {want}");
        // the ladder ignores the pin, so an 8Y-pinned view reads the same rung as an unpinned one
        assert!((trust_factor(&q, false, 8, true, 0.5) - want).abs() < 1e-9, "pinned rung {legs:?} -> {want}");
    }
    // a 10Y leg alone is NOT a rung — it falls to the 8Y tier it clears, which is the whole point:
    // the cliff spent all its resolution on 10Y and could not tell 10y from 46y.
    assert!((trust_factor(&quote(20.0, &[("5Y", 40.0), ("8Y", 90.0), ("10Y", 200.0)]), false, 0, true, 0.5) - 0.85).abs() < 1e-9);
    // and the cliff arm disagrees with the ladder on exactly that name -> the knob is not inert
    assert!((trust_factor(&quote(20.0, &[("5Y", 40.0), ("8Y", 90.0), ("10Y", 200.0)]), false, 0, false, 0.5) - 1.0).abs() < 1e-9);

    // (#47) DEFAULT IS BYTE-IDENTICAL. The knob ships false, so every score must be unchanged from the
    // pre-ladder lane — this is the assertion that lets the shipped edge stay uncontested until an arm
    // measures otherwise. A 20Y name is the case that would move if the default ever flipped by accident.
    let veteran = quote(20.0, &[("1Y", 30.0), ("5Y", 200.0), ("8Y", 400.0), ("10Y", 600.0), ("20Y", 2000.0)]);
    let young = quote(20.0, &[("1Y", 30.0), ("5Y", 200.0)]);
    for q in [&veteran, &young] {
        assert_eq!(
            growth_score(q, &BuyHeuristic::default()),
            growth_score(q, &BuyHeuristic { growth_trust_ladder: false, ..BuyHeuristic::default() }),
            "the shipped default must be the cliff arm, exactly"
        );
    }
    // ...and the ladder DOES move a score, so `false` is a real choice rather than a dead branch
    let on = BuyHeuristic { growth_trust_ladder: true, ..BuyHeuristic::default() };
    assert_ne!(growth_score(&young, &on), growth_score(&young, &BuyHeuristic::default()));

    // (#49) THE 1Y RUNG. `long_leg`'s ladder gained a bottom step, so a name whose only record is one
    // year now has a long CAGR to be judged on instead of dying unscorable on the `history` gate.
    // `growth_min_leg_years` stays a FLOOR, so each setting admits exactly the rungs at or above it.
    let one_year = quote(20.0, &[("1Y", 30.0)]);
    let two_year = quote(20.0, &[("1Y", 30.0), ("2Y", 60.0)]);
    assert_eq!(long_leg(&one_year, 1.0), Some((30.0, 1.0))); // floor 1 -> the 1Y rung is reachable
    assert_eq!(long_leg(&one_year, 2.0), None); // floor 2 -> excluded, as before (#49)
    assert_eq!(long_leg(&one_year, 5.0), None); // shipped floor -> still unscorable
    assert_eq!(long_leg(&two_year, 2.0), Some((60.0, 2.0))); // floor 2 admits 2Y and stops there
    assert_eq!(long_leg(&two_year, 1.0), Some((60.0, 2.0))); // LONGEST available, not shortest — floor, not pin
    // THE SECOND PENALTY, unconfigured and easy to forget: a 1Y-rung name's long CAGR IS its 1Y return,
    // so `accel = clamp(return_1y − long_cagr, ..)` is identically ZERO and the heaviest term in the
    // score (growth_accel_weight 0.65) is structurally dead for exactly this cohort. Read the young-dock
    // curve knowing the dock is not the only thing holding these names down.
    let floor1 = BuyHeuristic { growth_min_leg_years: 1.0, ..BuyHeuristic::default() };
    assert!((long_cagr_pct(&one_year, &floor1).unwrap() - 30.0).abs() < 1e-9); // == the 1Y leg -> accel term is 0
    assert!(long_cagr_pct(&one_year, &BuyHeuristic::default()).is_none()); // ...and nothing at all at floor 5

    // (#49) the young dock. 2Y rung takes the knob, 1Y rung takes half of it.
    let dock = |w: f64, q: &Quote| trust_factor(q, false, 0, true, w);
    assert!((dock(0.4, &two_year) - 0.4).abs() < 1e-9);
    assert!((dock(0.4, &one_year) - 0.2).abs() < 1e-9);
    assert!(dock(0.1, &two_year) < dock(0.5, &two_year)); // lower knob = harsher dock, both rungs
    assert!(dock(0.1, &one_year) < dock(0.5, &one_year));
    // the ORDER the whole round is about: longer record, more trust, no ties across the ladder
    let five_year = quote(20.0, &[("1Y", 30.0), ("5Y", 200.0)]);
    assert!(dock(0.4, &five_year) > dock(0.4, &two_year) && dock(0.4, &two_year) > dock(0.4, &one_year));

    // DEFAULT IS BYTE-IDENTICAL, twice over. At the shipped floor the knob cannot reach anything —
    // `score_parts` bails on `long_leg_fixed(..)?` before trust is computed — so even the ladder arm is
    // unmoved by it, which is what keeps arm D's (#47) grid reproducible against this code.
    for w in [0.1, 0.5, 0.9] {
        let tweaked = BuyHeuristic { growth_trust_young: w, ..BuyHeuristic::default() };
        assert_eq!(growth_score(&veteran, &tweaked), growth_score(&veteran, &BuyHeuristic::default()));
        assert_eq!(growth_score(&young, &tweaked), growth_score(&young, &BuyHeuristic::default()));
        let ladder_arm = BuyHeuristic { growth_trust_young: w, ..on.clone() };
        assert_eq!(growth_score(&young, &ladder_arm), growth_score(&young, &on), "arm D must not move");
    }
    // ...and it is NOT inert once the floor lets a young name in — the arm this round exists to measure
    let armed = |w: f64| BuyHeuristic {
        growth_min_leg_years: 2.0, growth_trust_ladder: true, growth_trust_young: w,
        growth_min_cagr: -100.0, growth_min_range_pct: 0.0, ..BuyHeuristic::default()
    };
    let (harsh, mild) = (growth_score(&two_year, &armed(0.1)), growth_score(&two_year, &armed(0.5)));
    assert!(harsh.is_some() && harsh < mild, "floor 2 + ladder: the young dock must bite ({harsh:?} vs {mild:?})");

    // (#48) the proximity authority knob. `quote(dd, ..)` sets range_pct = 100 − dd, so this name sits
    // at 80% of its 10y range — the exact bottom of the shipped gate, where the knob has its widest say.
    let off_hi = quote(20.0, &[("1Y", 30.0), ("5Y", 200.0), ("10Y", 600.0)]);
    assert_eq!(off_hi.range_pct, 80.0);
    let prox = |w: f64| BuyHeuristic { growth_proximity_weight: w, ..BuyHeuristic::default() };
    // DEFAULT IS BYTE-IDENTICAL: w=1 must reproduce the raw `range_pct/100` multiply the lane shipped
    // with, so the validated edge stays uncontested. Checked at the gate edge AND at the high, since
    // the blend is only pinned by two points.
    let at_high = quote(0.0, &[("1Y", 30.0), ("5Y", 200.0), ("10Y", 600.0)]); // range_pct 100
    for q in [&off_hi, &at_high] {
        assert_eq!(growth_score(q, &BuyHeuristic::default()), growth_score(q, &prox(1.0)),
            "the shipped default must be the raw proximity multiply, exactly");
    }
    // and the knob is NOT inert: off (×1.00) beats shipped (×0.80), inverted (×1.20) beats off, and a
    // steeper slope (×0.60) is worse than shipped. Ordering, not magnitudes — the run prices those.
    let s = |w: f64| growth_score(&off_hi, &prox(w)).expect("off_hi clears every gate at 80% of range");
    assert!(s(-1.0) > s(0.0), "w=−1 must LIFT a name below its high: {} vs {}", s(-1.0), s(0.0));
    assert!(s(0.0) > s(1.0), "w=0 removes the dock, so it must beat the shipped dock");
    assert!(s(1.0) > s(2.0), "a steeper slope must dock the same name harder");
    // a name AT its high is untouched by any rung — the blend pivots on range_pct = 100, so every
    // ladder value agrees there. This is what makes the knob a slope and not a level shift.
    let base_hi = growth_score(&at_high, &BuyHeuristic::default());
    for w in [-1.0, 0.0, 0.5, 3.0] {
        assert_eq!(growth_score(&at_high, &prox(w)), base_hi, "w={w} moved a name at its own high");
    }
    // the NaN guard. `combine_damps` is product().powf(1/n), so a negative factor there yields NaN and
    // silently poisons the whole score. Unreachable at the shipped gate, so it is forced here: a name
    // at 50% of range under w=3 would compute 1 + 3×(−0.5) = −0.5 without the clamp.
    let bled = quote(50.0, &[("1Y", 30.0), ("5Y", 200.0), ("10Y", 600.0)]); // range_pct 50
    let open_gate = BuyHeuristic { growth_min_range_pct: 0.0, growth_proximity_weight: 3.0, ..BuyHeuristic::default() };
    let clamped = growth_score(&bled, &open_gate).expect("gate opened, so it scores");
    assert!(clamped.is_finite(), "proximity clamp let a NaN through: {clamped}");
    let neutral = growth_score(&bled, &BuyHeuristic { growth_proximity_weight: 0.0, ..open_gate.clone() }).unwrap();
    assert!(clamped < neutral, "a clamped proximity must still be the harshest rung, not a free pass");
    // same fixture through the geomean-fold branch, which is where the NaN would actually be born:
    // combine_damps is product().powf(1/n), and (−0.5)^(1/4) is NaN, not a small number.
    let folded = BuyHeuristic { growth_geomean_fold: true, ..open_gate.clone() };
    assert!(growth_score(&bled, &folded).expect("scores").is_finite(), "geomean fold ate a negative proximity");

    // (S-8Y) the pin re-runs the CAGR floor on the 8-year window, so a name whose full record clears
    // the floor but whose 8Y window doesn't gets NO pinned score — that was the bare "n/a" in the
    // column. (Floor here is the Rust default 8.0; the shipped config raises it to 14.0.)
    // Dropping the floor (what `print_picks` builds for S-8Y) scores it anyway. The live assert is the
    // guard: it proves the name is scoreable and the pin is the ONLY reason the bare version is None.
    // The strong leg is 20Y, not 10Y: since the ladder moved to 20/8/5 an 8Y leg OUTRANKS a 10Y one,
    // so a 10Y-strong/8Y-weak name is no longer scoreable live and could not make this point.
    let weak8 = quote(5.0, &[("1Y", 12.0), ("5Y", 60.0), ("8Y", 50.0), ("10Y", 400.0), ("20Y", 1500.0)]); // 8Y +50% ≈ 5.2%/yr, under the 8.0 default floor
    assert!(growth_score(&weak8, &tuning).is_some(), "scoreable live off its 20Y leg (+14.9%/yr)");
    let pin8 = BuyHeuristic { fixed_cagr_years: 8, ..tuning.clone() };
    assert!(growth_score(&weak8, &pin8).is_none(), "8Y CAGR under the floor gates the pinned score");
    let pin8_open = BuyHeuristic { growth_min_cagr: f64::NEG_INFINITY, ..pin8.clone() };
    assert!(growth_score(&weak8, &pin8_open).is_some(), "S-8Y drops the floor -> a number, not n/a");

    // (#4) combine_damps: empty/all-1.0 -> 1.0; a lone 0.5 softens to 0.5^(1/n) (bounded, NOT the raw
    // product); the geomean of several mild damps stays well above their product (no silent nuke).
    assert_eq!(combine_damps(&[]), 1.0);
    assert_eq!(combine_damps(&[1.0, 1.0, 1.0]), 1.0);
    assert!((combine_damps(&[0.5, 1.0, 1.0]) - 0.5_f64.powf(1.0 / 3.0)).abs() < 1e-9);
    assert!(combine_damps(&[0.5, 0.4, 0.5]) > 0.5 * 0.4 * 0.5); // geomean bounded above the product
    assert!(combine_damps(&[0.9, 0.5]) < combine_damps(&[0.9, 0.9])); // still monotone in each term

    // (F) ROE quality reward: positive ROE -> weight×roe (capped); None/negative -> 0 (neutral)
    let mut hi_roe = quote(20.0, &[("1Y", 10.0)]);
    hi_roe.roe = Some(30.0);
    assert!((quality_reward(&hi_roe, &tuning) - tuning.quality_weight * 30.0).abs() < 1e-9);
    hi_roe.roe = Some(tuning.quality_cap + 500.0); // a buyback-levered outlier is clamped at the cap
    assert!((quality_reward(&hi_roe, &tuning) - tuning.quality_weight * tuning.quality_cap).abs() < 1e-9);
    hi_roe.roe = Some(-50.0); // loss-making -> no quality bonus
    assert_eq!(quality_reward(&hi_roe, &tuning), 0.0);
    assert_eq!(quality_reward(&quote(20.0, &[("1Y", 10.0)]), &tuning), 0.0); // roe None -> 0

    // EU-buyability gate: crypto majors + UCITS ETFs + US/Canada/EU-listed stocks pass; a US-domiciled
    // ETF (no PRIIPs KID) and an Asian-only listing are dropped — EU retail can't buy them.
    let mut us_etf = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    us_etf.name = "SPDR S&P 500 ETF Trust".into();
    us_etf.ticker = "SPY".into();
    us_etf.market = "USA".into();
    assert!(!eu_buyable(&us_etf)); // US-domiciled ETF -> not EU-buyable
    let mut ucits = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    ucits.name = "iShares Core S&P 500 UCITS ETF".into();
    ucits.market = "UK".into();
    assert!(eu_buyable(&ucits)); // UCITS wrapper -> buyable
    // the bug this fixes: a UCITS ETF whose Yahoo shortName carries NO "ETF"/"UCITS" marker still
    // classifies as an ETF (via instrumentType) and stays buyable on its European listing.
    let mut bare = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    bare.name = "ISHARES III PLC ISHRS CORE MSCI".into(); // real marker-less ETF shortName
    bare.instrument_type = "ETF".into();
    bare.market = "Ireland".into();
    assert!(quote_is_etf(&bare) && !is_etf(&bare.name)); // typed as ETF, not name-matched
    assert!(eu_buyable(&bare)); // EU venue -> buyable despite the marker-less name
    let mut hk = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    hk.name = "Tencent Holdings".into();
    hk.market = "Hong Kong".into();
    assert!(!eu_buyable(&hk)); // HK-only listing off most EU retail brokers
    let mut us_stk = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    us_stk.name = "Apple Inc.".into(); // market defaults to "USA"
    assert!(eu_buyable(&us_stk));
    let mut btc_b = quote(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    btc_b.ticker = "BTC-EUR".into();
    assert!(eu_buyable(&btc_b)); // crypto major
    // end-to-end: `ranked` drops the US ETF even though it scores above min_score
    assert!(buy_score(&us_etf, &tuning).unwrap() > tuning.min_score);
    assert!(ranked(std::slice::from_ref(&us_etf), &tuning, buy_score, tuning.min_score, &no_pin).is_empty());
    assert_eq!(ranked(std::slice::from_ref(&ucits), &tuning, buy_score, tuning.min_score, &no_pin).len(), 1);

    // (#4) per-class crypto overextension cap: a crypto name stretched ABOVE the equity cap is braked
    // LESS under its own looser cap. Same stretched crypto quote, two tunings -> the looser-cap score
    // is higher (the brake docks it less). Guards the knob that shipped neutral-by-default.
    let mut stretched_crypto = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    stretched_crypto.ticker = "BTC-USD".into();
    stretched_crypto.above_ma_pct = 150.0; // beyond the 100 equity cap
    let loose = BuyHeuristic { growth_overext_cap_crypto: 200.0, ..BuyHeuristic::default() };
    assert!(growth_score(&stretched_crypto, &loose).unwrap() > growth_score(&stretched_crypto, &tuning).unwrap());

    // (G) fund factor — NEUTRALITY: at the default growth_fund_weight 0 a populated fund_factor must NOT
    // move the score (byte-identical to fund_factor None), so the validated price edge is untouched until
    // the weight is deliberately raised. With a positive weight the factor lifts the score; a negative
    // factor is floored at 0 (only rewarded, never penalised).
    let mut with_fund = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    with_fund.fund_factor = Some(15.0); // e.g. +15pt revenue accel
    let none_fund = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // fund_factor None
    assert_eq!(growth_score(&with_fund, &tuning).unwrap(), growth_score(&none_fund, &tuning).unwrap()); // weight 0 -> inert
    let weighted = BuyHeuristic { growth_fund_weight: 0.5, ..BuyHeuristic::default() };
    assert!(growth_score(&with_fund, &weighted).unwrap() > growth_score(&none_fund, &weighted).unwrap()); // +factor lifts
    let mut neg_fund = none_fund.clone();
    neg_fund.fund_factor = Some(-40.0); // decelerating -> floored at 0, not a penalty
    assert_eq!(growth_score(&neg_fund, &weighted).unwrap(), growth_score(&none_fund, &weighted).unwrap());
    // exact magnitude + cap clamp — isolate the (G) term (ScoreParts.fund = weight·clamp(factor,0,cap)):
    assert_eq!(score_parts(&with_fund, &weighted).unwrap().fund, 0.5 * 15.0); // 15pt factor × 0.5, under the 30 cap
    let mut over_cap = none_fund.clone();
    over_cap.fund_factor = Some(100.0); // above the default growth_fund_cap 30 -> clamped
    assert_eq!(score_parts(&over_cap, &weighted).unwrap().fund, 0.5 * 30.0); // pins the .clamp(0,cap) upper bound

    // (G+) MULTI-TERM fund tilt. The default `growth_fund_extra` is empty, and an empty sum must be
    // exactly 0.0 — that is what keeps every recorded receipt in tests/ci-settings.yaml valid, so pin
    // it on a quote that CARRIES fundamentals (an all-None quote would pass the assert vacuously).
    let mut with_facts = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    with_facts.fund = Some(core::FundFactors {
        rev_yoy: Some(20.0),
        eps_yoy: Some(200.0), // deliberately huge -> exercises a per-term cap below
        net_margin: Some(30.0),
        ..Default::default()
    });
    assert_eq!(
        growth_score(&with_facts, &tuning).unwrap(),
        growth_score(&none_fund, &tuning).unwrap(),
        "empty growth_fund_extra must be byte-identical to the single-factor lane"
    );
    // Terms SUM, each clamped by its OWN cap — the factors are on incompatible scales, so one shared
    // clamp would flatten whichever is smaller. eps_yoy 200 is over its cap 50; the other two are not.
    let multi = BuyHeuristic {
        growth_fund_extra: vec![
            crate::config::FundTerm { factor: "rev_yoy".into(), weight: 0.5, cap: 100.0, neutral: 0.0 },
            crate::config::FundTerm { factor: "eps_yoy".into(), weight: 0.1, cap: 50.0, neutral: 0.0 },
            crate::config::FundTerm { factor: "net_margin".into(), weight: 0.2, cap: 100.0, neutral: 0.0 },
        ],
        ..BuyHeuristic::default()
    };
    let want = 0.5 * 20.0 + 0.1 * 50.0 + 0.2 * 30.0; // 10 + 5 (capped, not 20) + 6
    assert!((score_parts(&with_facts, &multi).unwrap().fund - want).abs() < 1e-9);
    // an unknown name reads None and contributes 0 rather than erroring — same as the primary term
    let typo = BuyHeuristic {
        growth_fund_extra: vec![crate::config::FundTerm { factor: "rev_yyo".into(), weight: 9.0, cap: 100.0, neutral: 0.0 }],
        ..BuyHeuristic::default()
    };
    assert_eq!(score_parts(&with_facts, &typo).unwrap().fund, 0.0);
    // a quote with NO fundamentals under a configured term contributes `neutral`, which is 0 here
    assert_eq!(score_parts(&none_fund, &multi).unwrap().fund, 0.0);
    // primary and extra ADD: same quote, primary term on top of the three extras
    let both = BuyHeuristic { growth_fund_weight: 0.5, ..multi.clone() };
    let mut with_both = with_facts.clone();
    with_both.fund_factor = Some(15.0);
    assert!((score_parts(&with_both, &both).unwrap().fund - (want + 0.5 * 15.0)).abs() < 1e-9);

    // (N) a MISSING extra factor scores `neutral`, NOT 0. In a ranking an absent datum is not neutral:
    // at 0 it is a demotion of up to `weight × cap`, and the shipped roic term (0.25 × 40) charged TEN
    // POINTS to the 114 of 509 cached SEC filers (22.4%) that report no `op_margin` — banks, insurers
    // and also CVX/COP/DE/ADM. Modelled here on `roic` with the shipped shape and the census median.
    let roic_term = |neutral: f64| BuyHeuristic {
        growth_fund_extra: vec![crate::config::FundTerm { factor: "roic".into(), weight: 0.25, cap: 40.0, neutral }],
        ..BuyHeuristic::default()
    };
    let facts = |roic: Option<f64>| {
        let mut q = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
        q.fund = Some(core::FundFactors { roic, ..Default::default() });
        q
    };
    let filled = roic_term(17.3); // census median of the 395 covered filers, clamp(0,40)
    // the whole point: unknown now scores as TYPICAL, so a missing leg costs nothing against a
    // median peer. This assert fails the moment anyone restores `unwrap_or(0.0)`.
    assert_eq!(
        growth_score(&facts(None), &filled).unwrap(),
        growth_score(&facts(Some(17.3)), &filled).unwrap(),
        "a missing factor must score as `neutral`, not as the worst possible value"
    );
    // ...and it is NOT a floor: a KNOWN-BAD roic clamps to 0 and ranks BELOW an unknown one. That
    // ordering is the reason the clamp stays outside the fill.
    assert!(
        score_parts(&facts(Some(2.0)), &filled).unwrap().fund < score_parts(&facts(None), &filled).unwrap().fund,
        "known-bad must rank under unknown"
    );
    // the fill still respects the cap — a neutral above it contributes weight × cap, not weight × neutral
    assert_eq!(score_parts(&facts(None), &roic_term(1000.0)).unwrap().fund, 0.25 * 40.0);
    // and `neutral` defaults to 0.0, so a term that omits it is byte-identical to the pre-(N) lane
    let omitted: crate::config::FundTerm = serde_yaml::from_str("factor: roic\nweight: 0.25\ncap: 40.0\n").unwrap();
    assert_eq!(omitted.neutral, 0.0, "an omitted `neutral` must deserialize to 0.0 — every recorded receipt depends on it");
    assert_eq!(score_parts(&facts(None), &roic_term(0.0)).unwrap().fund, 0.0);

    // (D) dividend fold — magnitude + cap on the LIVE term (default weight 1.5, cap 6.0); isolate via ScoreParts.dividend.
    // dividend_yield_1y divides -> not bit-exact -> epsilon compare (matches the float-score convention above).
    let mut div_grow = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    div_grow.price_eur = Some(100.0);
    div_grow.div_eur = vec![Some(5.0)]; // 5% trailing-1Y yield, under the 6% cap
    assert!((score_parts(&div_grow, &tuning).unwrap().dividend - 1.5 * 5.0).abs() < 1e-9); // weight 1.5 × yield 5 = 7.5
    let mut div_cap = div_grow.clone();
    div_cap.div_eur = vec![Some(20.0)]; // 20% (bad-feed / special-div artifact) -> clamped to the 6% cap
    assert!((score_parts(&div_cap, &tuning).unwrap().dividend - 1.5 * 6.0).abs() < 1e-9); // .min(dividend_cap) upper bound = 9.0

    // (D/PT) PORTUGUESE TAX on the dividend term. Art. 40.º-A CIRS englobates only 50% of a dividend
    // from an EU-resident company, so a EUR of EU dividend is worth more after tax than a EUR of US
    // dividend — the term must score them differently. `tuning` above leaves both keep-rates at the
    // 1.0 default, which is exactly why the two asserts ABOVE stay untouched by this feature.
    let taxed = BuyHeuristic { tax_keep_eu: 0.75, tax_keep_other: 0.72, ..BuyHeuristic::default() };
    let mut us_stock = div_grow.clone(); // market "USA" from the helper, 5% yield, under the cap
    us_stock.market = "USA".into();
    let mut pt_stock = us_stock.clone();
    pt_stock.market = "Portugal".into();
    let us_div = score_parts(&us_stock, &taxed).unwrap().dividend;
    let pt_div = score_parts(&pt_stock, &taxed).unwrap().dividend;
    assert!((us_div - 1.5 * 5.0 * 0.72).abs() < 1e-9); // non-EU payer keeps tax_keep_other
    assert!((pt_div - 1.5 * 5.0 * 0.75).abs() < 1e-9); // EU payer keeps the (higher) tax_keep_eu
    assert!(pt_div > us_div, "the 50% englobamento exclusion must favour the EU payer");
    // European but NOT EU: UK left in 2020, Switzerland/Norway never joined -> no Art. 40.º-A relief.
    for outside in ["UK", "Switzerland", "Norway"] {
        let mut q = us_stock.clone();
        q.market = outside.into();
        assert!((score_parts(&q, &taxed).unwrap().dividend - us_div).abs() < 1e-9, "{outside} is not EU");
    }
    // CAP-ORDER PIN: the cap hits the GROSS yield, then the keep-rate scales it. Write it the other
    // way round (`min(yield × keep, cap)`) and both rows saturate at the cap — the tax distinction
    // silently vanishes for every high yielder, which is where it is worth most. This assert reds.
    let mut us_over = us_stock.clone();
    us_over.div_eur = vec![Some(20.0)]; // 20% yield, well above the 6% cap
    let mut pt_over = us_over.clone();
    pt_over.market = "Portugal".into();
    let (us_o, pt_o) = (score_parts(&us_over, &taxed).unwrap().dividend, score_parts(&pt_over, &taxed).unwrap().dividend);
    assert!((us_o - 1.5 * 6.0 * 0.72).abs() < 1e-9);
    assert!((pt_o - 1.5 * 6.0 * 0.75).abs() < 1e-9);
    assert!(pt_o > us_o, "above the cap the EU/non-EU distinction must SURVIVE — cap first, then scale");
    // FUNDS take tax_keep_other at any domicile: an OICVM distribution is not a Parent-Subsidiary
    // Directive company's *lucro*, so an EU-listed UCITS draws no 50% exclusion.
    let mut eu_etf = pt_stock.clone();
    eu_etf.market = "Germany".into();
    eu_etf.instrument_type = "ETF".into();
    assert!((score_parts(&eu_etf, &taxed).unwrap().dividend - us_div).abs() < 1e-9, "a fund gets no EU exclusion");

    // (M) 12-1 momentum — NEUTRALITY: two names identical but for last-month return (different 12-1)
    // must score the SAME at the default growth_mom121_weight 0 — the price-validated lane is unchanged
    // until the weight is tuned. Both 1M values clear the knife gate, so only the 12-1 term differs.
    let hi_mom = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0), ("1M", 2.0)]);  // small recent month -> MORE of the year's gain is older -> higher 12-1
    let lo_mom = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0), ("1M", 25.0)]); // big recent month -> lower 12-1 (ex the skip month)
    assert_eq!(growth_score(&hi_mom, &tuning).unwrap(), growth_score(&lo_mom, &tuning).unwrap()); // weight 0 -> inert
    // TILT: a positive weight rewards the higher 12-1 momentum.
    let wmom = BuyHeuristic { growth_mom121_weight: 0.5, ..BuyHeuristic::default() };
    assert!(growth_score(&hi_mom, &wmom).unwrap() > growth_score(&lo_mom, &wmom).unwrap());

    // gate_failures DIAGNOSTIC reasons (the footer path, separate impl from score_parts' gating): one
    // equity quote tripping every armed gate must surface each reason tag. Arms the normally-off knobs.
    let gt = BuyHeuristic {
        min_avg_turnover_eur: 1_000_000.0,
        growth_max_above_ma: 100.0,
        growth_require_lifetime_uptrend: true,
        growth_maxdd_cap: 50.0,
        max_1m_drop_pct: -20.0,
        ..BuyHeuristic::default()
    };
    let mut bad = quote(5.0, &[("1Y", 10.0), ("5Y", -5.0), ("10Y", 40.0), ("1M", -30.0)]);
    bad.above_ma_pct = 300.0;             // stretch: far above the 200wk SMA cap
    bad.avg_turnover_eur = Some(1_000.0); // liquidity: below the €1M floor (still Some -> assessable)
    bad.max_drawdown_pct = 90.0;          // maxdd: worse than the 50% cap
    bad.trend_cagr = Some(-5.0);          // lifetime: whole-life trend <= 0
    let fails = gate_failures(&bad, &gt).unwrap();
    let has = |t: &str| fails.iter().any(|(g, _, _)| *g == t);
    assert!(has("1M-knife"), "1M -30% vs a -20% floor must fire the knife reason");
    assert!(has("5Y+"), "5Y -5% must fire the 5Y+ reason");
    assert!(has("liquidity") && has("stretch") && has("lifetime") && has("maxdd"));

    // (5Y/8Y/20Y floors) the per-rung CUMULATIVE-return gate. ONE table (`long_leg_floors`) read by
    // score_parts AND gate_failures, so the pair below is the lockstep assert picks.rs:745 asks for.
    // Fixture clears every other default gate with room: 20Y +500% = 9.4%/yr vs the 8%/yr CAGR floor.
    let rungs = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("8Y", 300.0), ("20Y", 500.0)]);
    assert!(growth_score(&rungs, &tuning).is_some(), "8Y/20Y ship OFF (-1e9) -> they must reject nothing");
    for (knob, tag) in [
        (BuyHeuristic { growth_min_8y_pct: 350.0, ..BuyHeuristic::default() }, "8Y+"),
        (BuyHeuristic { growth_min_20y_pct: 600.0, ..BuyHeuristic::default() }, "20Y+"),
    ] {
        assert!(growth_score(&rungs, &knob).is_none(), "{tag}: a leg under its armed floor must be gated");
        let why = gate_failures(&rungs, &knob).unwrap();
        assert!(why.iter().any(|(g, _, _)| *g == tag), "{tag}: score_parts gated it but the tail doesn't say so");
    }
    // THE "if not N/A" RULE: the SAME name minus its 20Y leg clears the SAME armed bar. A leg the
    // quote doesn't have is skipped, never read as a failed one — otherwise the 20Y floor would cut
    // every ETF and every coin, none of which has 20 years of history to show.
    let no20 = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("8Y", 300.0)]);
    let g20 = BuyHeuristic { growth_min_20y_pct: 600.0, ..BuyHeuristic::default() };
    assert!(growth_score(&no20, &g20).is_some(), "absent 20Y must be SKIPPED, not treated as a 0% return");

    // (S-8Y range) the 8y-window percentile bar. `rungs` has range_pct 100 — it clears the LIVE bar
    // with room, so anything that rejects below can only be reading `stats_8y`. That is the PGR shape:
    // healthy on the ~10y chart, weak once the oldest years drop out.
    let mut weak8 = rungs.clone();
    weak8.stats_8y = Some(core::Stats8 { range_pct: 40.0, trend_r2: 0.9, max_drawdown_pct: 30.0, underwater_yrs: None });
    assert!(growth_score(&weak8, &tuning).is_some(), "ships OFF (0.0) -> the 8y window must reject nobody");
    let armed = BuyHeuristic { growth_min_range_pct_8y: 80.0, growth_min_range_pct_8y_crypto: 40.0, ..BuyHeuristic::default() };
    assert!(growth_score(&weak8, &armed).is_none(), "40% of its 8y range vs an 80 bar must gate, despite a 100 live range");
    let why = gate_failures(&weak8, &armed).unwrap();
    assert!(why.iter().any(|(g, _, _)| *g == "range8y"), "score_parts gated it but the tail doesn't say so");
    // THE EQUIVALENCE this gate was chosen for: it fires exactly when the S-8Y column blanks, so an
    // armed ranking and the printed diagnostic can never disagree. If a future gate starts reading one
    // of the OTHER stats `as_8y_window` swaps (trend_r2, maxdd, underwater), this assert is what catches
    // the divergence — `tuning8` is reproduced here from the render site (picks.rs:1927).
    let tuning8 = BuyHeuristic { fixed_cagr_years: 8, growth_min_cagr: f64::NEG_INFINITY, growth_min_cagr_crypto: f64::NEG_INFINITY, ..armed.clone() };
    assert!(growth_score(&as_8y_window(&weak8), &tuning8).is_none(), "gate fired -> the S-8Y cell must be the n/a it mirrors");
    // no `stats_8y` = under 8y of record: its whole span IS the window and the live bar already judged
    // it. Missing data is not a failed bar — the same rule the 20Y floor above is pinned on.
    assert!(growth_score(&rungs, &armed).is_some(), "absent stats_8y must be SKIPPED, not read as a 0% range");
    // each class reads its OWN knob, like the 10y pair: 50% gates an equity at 80 and clears a coin at 40
    let mut mid8 = rungs.clone();
    mid8.stats_8y = Some(core::Stats8 { range_pct: 50.0, trend_r2: 0.9, max_drawdown_pct: 30.0, underwater_yrs: None });
    assert!(growth_score(&mid8, &armed).is_none(), "equity: 50 < 80");
    let mut coin8 = mid8.clone();
    coin8.ticker = "ETH-EUR".into();
    assert!(growth_score(&coin8, &armed).is_some(), "crypto: 50 >= its own 40 bar — the split must hold on this pair too");
    // (crypto) the `!crypto` guard came OFF the 5Y bar — a coin answers to it now. That is a
    // deliberate behaviour change the backtest cannot see (its edge metrics all drop crypto), so
    // pin it here: this assert failing means the guard came back.
    // (the gating half of this is pinned by the ETH-EUR fixture above; this is the TAIL half — the
    // footer has to name the rung, or a coin vanishes from the ranking with no reason printed.)
    let mut coin = quote(5.0, &[("1Y", 10.0), ("5Y", -20.0), ("10Y", 40.0)]);
    coin.ticker = "OKB-USD".into();
    assert!(gate_failures(&coin, &tuning).unwrap().iter().any(|(g, _, _)| *g == "5Y+"));
    // the lifetime SECOND leg: window trend positive but listing-to-date CAGR negative (Greece pattern)
    let mut greece_gf = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    greece_gf.trend_cagr = Some(5.0);     // window trend fine
    greece_gf.life_cagr = Some(-8.0);     // ...but since-listing negative
    assert!(gate_failures(&greece_gf, &gt).unwrap().iter().any(|(g, why, _)| *g == "lifetime" && why.contains("since listing")));
    // crypto-only VOL gate: a coin swinging wider than the base fires "volatile"
    let vt = BuyHeuristic { growth_max_vol_crypto: 3.0, ..BuyHeuristic::default() };
    let mut wildc = quote(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    wildc.ticker = "OKB-USD".into();
    wildc.volatility_pct = Some(5.0);
    assert!(gate_failures(&wildc, &vt).unwrap().iter().any(|(g, _, _)| *g == "volatile"));
    // "1Y+" floor — the falling-knife gate (round 5: the cohort a loosened floor admits measured
    // −108 fwd; never loosen). Stock leg is a hardcoded 1Y > 0 in BOTH paths: gate_failures names
    // it, and score_parts bails to None. Legs are strong enough that the 1Y floor is the ONLY
    // decisive gate — the +5% control scoring Some proves the floor is what rejected the −5% twin.
    let down1y = quote(5.0, &[("1Y", -5.0), ("5Y", 200.0), ("10Y", 500.0)]);
    assert!(gate_failures(&down1y, &tuning).unwrap().iter().any(|(g, _, _)| *g == "1Y+"));
    assert!(growth_score(&down1y, &tuning).is_none(), "score path must enforce the 1Y floor");
    let up1y = quote(5.0, &[("1Y", 5.0), ("5Y", 200.0), ("10Y", 500.0)]);
    assert!(growth_score(&up1y, &tuning).is_some(), "control: same quote above the floor ranks");
    // …and the equity floor is now the `growth_min_1y_pct` knob, whose default IS the 0.0 the two
    // gate sites hardcoded — so the asserts above are simultaneously the neutral-default pin. Set it
    // and the SAME quote must flip in BOTH sites: the ranking and the tail disagreeing is the bug.
    let loosened = BuyHeuristic { growth_min_1y_pct: -10.0, ..BuyHeuristic::default() };
    assert!(growth_score(&down1y, &loosened).is_some(), "the knob must reach score_parts, not just the tail");
    assert!(!gate_failures(&down1y, &loosened).unwrap().iter().any(|(g, _, _)| *g == "1Y+"), "…and gate_failures with it");
    // the knob is the EQUITY leg only — a coin still reads min_1y_pct_crypto (default -60 here).
    let mut coin = quote(5.0, &[("1Y", -33.0), ("5Y", 200.0), ("10Y", 500.0)]);
    coin.ticker = "BTC-EUR".into();
    let equity_strict = BuyHeuristic { growth_min_1y_pct: 0.0, ..BuyHeuristic::default() };
    assert!(!gate_failures(&coin, &equity_strict).unwrap().iter().any(|(g, _, _)| *g == "1Y+"), "crypto keeps its own floor");

    // (D) the down-year tail: ONLY the 1Y gate failing, at ANY depth — the shallow half is already in
    // the near-miss tail (its `1Y+` margin is 10pp), and splitting the list there hides the deep names
    // the floor actually costs. -40% must be selected precisely because growth_near_miss drops it.
    let shallow = quote(5.0, &[("1Y", -7.0), ("5Y", 200.0), ("10Y", 500.0)]);
    let deep = quote(5.0, &[("1Y", -40.0), ("5Y", 200.0), ("10Y", 500.0)]);
    assert!(growth_down_year_miss(&shallow, &tuning).is_some_and(|(y1, cagr, _)| y1 == -7.0 && cagr > 0.0));
    assert!(growth_down_year_miss(&deep, &tuning).is_some(), "the deep half is the whole point of this tail");
    assert!(growth_near_miss(&deep, &tuning).is_none(), "…and it is exactly what the near-miss tail cannot show");
    // a second failing gate, or none at all -> not this tail's business
    assert!(growth_down_year_miss(&quote(25.0, &[("1Y", -7.0), ("5Y", 200.0), ("10Y", 500.0)]), &tuning).is_none(), "1Y+ AND range");
    assert!(growth_down_year_miss(&up1y, &tuning).is_none(), "clears every gate -> it ranks");

    // (E) the long-leg floor tail: EVERY name the 5Y bar rejects, however grossly and whatever else it
    // also fails. The AMZN case — +25% against a +75 bar (gross) PLUS a second failing gate — is what
    // every tail above drops, so a closeness or arity filter here would return the list to empty.
    let floor5 = BuyHeuristic { growth_min_5y_pct: 75.0, growth_min_age_years: 5.0, ..BuyHeuristic::default() };
    let mut amzn = quote(25.0, &[("1Y", 10.0), ("5Y", 25.0), ("10Y", 500.0)]); // 5Y gross-fails; range 75<80 too
    amzn.age_years = Some(20.0);
    let (cagr, why, others) = growth_leg_floor_miss(&amzn, &floor5, "5Y+").expect("the 5Y bar rejects it -> this tail");
    assert!(cagr > 0.0 && why.contains("5Y +25.0%") && why.contains("need >75%"), "the row quotes the leg and the bar: {why}");
    assert_eq!(others, vec!["range"], "the OTHER gates are named, so the row says what else must give");
    assert!(growth_near_miss(&amzn, &floor5).is_none() && growth_n_gate_miss(&amzn, &floor5, 2).is_none(), "…and no tail above can reach it");
    assert!(growth_leg_floor_miss(&amzn, &tuning, "5Y+").is_none(), "at the 0.0 default a +25% 5Y clears the bar -> nothing to list");
    assert!(growth_leg_floor_miss(&amzn, &floor5, "8Y+").is_none(), "growth_min_8y_pct is -1e9: the 8Y block prints nothing until it is set");
    let clears = quote(5.0, &[("1Y", 10.0), ("5Y", 200.0), ("10Y", 500.0)]);
    assert!(growth_leg_floor_miss(&clears, &floor5, "5Y+").is_none(), "clears the floor -> not in this list");
    // per-class floor stays wired: a coin answers to growth_min_5y_pct_crypto, not the equity bar
    let mut coin5 = quote(25.0, &[("1Y", 10.0), ("5Y", 25.0), ("10Y", 500.0)]);
    coin5.ticker = "BTC-EUR".into();
    assert!(growth_leg_floor_miss(&coin5, &floor5, "5Y+").is_none(), "crypto twin is 0.0 -> +25% clears it");
    let coin_strict = BuyHeuristic { growth_min_5y_pct_crypto: 75.0, ..floor5.clone() };
    assert!(
        growth_leg_floor_miss(&coin5, &coin_strict, "5Y+").is_some_and(|(_, w, _)| w.contains("need >75%")),
        "…and when the crypto knob IS set, the row quotes that number, not the equity one"
    );

    // crypto leg: min_1y_pct_crypto is a CRASH bar, not a swing bar (round 22, live −50): a −62%
    // year fires, a BTC-like −33% year does not.
    let ct = BuyHeuristic { min_1y_pct_crypto: -50.0, ..BuyHeuristic::default() };
    let mut crashc = quote(5.0, &[("1Y", -62.0), ("5Y", 200.0), ("10Y", 500.0)]);
    crashc.ticker = "GT-USD".into();
    assert!(gate_failures(&crashc, &ct).unwrap().iter().any(|(g, _, _)| *g == "1Y+"));
    let mut swingc = quote(5.0, &[("1Y", -33.0), ("5Y", 200.0), ("10Y", 500.0)]);
    swingc.ticker = "GT-USD".into();
    assert!(!gate_failures(&swingc, &ct).unwrap().iter().any(|(g, _, _)| *g == "1Y+"));

    // turnover_cell compaction across every magnitude arm (B/M/K) + the unknown fallback
    assert_eq!(turnover_cell(Some(1.2e9)), "€1.2B");
    assert_eq!(turnover_cell(Some(340e6)), "€340M");
    assert_eq!(turnover_cell(Some(5e3)), "€5K");
    assert_eq!(turnover_cell(None), "n/a");
    }

    /// (round 59) SCORING REGRESSION PIN. Scoring is CLOSED at the round-14 optimum — every score,
    /// gate and rank below is pinned to the exact current output on a fixed synthetic universe. If
    /// this test reds you changed ranking behavior (a weight, a gate, a damp, a scored field's
    /// coverage — the round-47 trap class); revert unless changing the ranking was the explicit,
    /// validated goal. The relational asserts in `buy_heuristic` above check DIRECTIONS; this one
    /// pins the NUMBERS, replacing the manual live-run table diff done every round.
    #[test]
    fn scoring_regression_pin() {
        // archetype builder: known €1B turnover (liquidity-neutral, ln(1)=0), chosen horizons set
        let fx = |name: &str, ticker: &str, range_pct: f64, labels: &[(&str, f64)]| -> Quote {
            let mut q = Quote::stub(ticker, "€1.00", "", name);
            q.perf = HORIZONS
                .iter()
                .map(|(l, _)| labels.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
                .collect();
            // (ladder) same contiguity fix as the `quote` builder above: these fixtures list a 10Y leg
            // and no 8Y one, which no real history can produce. Left alone, the 20/8/5 ladder drops to
            // the 5Y rung and the pinned scores move for a reason that is pure fixture artifact rather
            // than a scoring change. Fill 8Y at the 10Y leg's own rate.
            let idx = |label: &str| HORIZONS.iter().position(|(l, _)| *l == label).unwrap();
            let (i8y, i10y) = (idx("8Y"), idx("10Y"));
            if q.perf[i8y].is_none() {
                if let Some((_, c10)) = q.perf[i10y].clone() {
                    let growth = 1.0 + c10 / 100.0;
                    if growth > 0.0 {
                        q.perf[i8y] = Some(("x".to_string(), (growth.powf(0.8) - 1.0) * 100.0));
                    }
                }
            }
            q.avg_turnover_eur = Some(1e9);
            q.range_pct = range_pct;
            q
        };
        let tuning = BuyHeuristic::default();

        // broad all-world core ETF: near its high, steady compounder, passes every hold-core leg
        let mut broad = fx("Vanguard FTSE All-World UCITS ETF USD Acc", "CORE.L", 90.0,
            &[("1D", 0.1), ("1W", 0.5), ("1M", 2.0), ("1Y", 25.0), ("5Y", 100.0), ("10Y", 300.0)]);
        broad.instrument_type = "ETF".into();
        broad.ter_fallback = Some(0.22);
        broad.replication = Some("Opt");
        broad.use_of_profits = Some("Acc");
        broad.aum_fallback = Some(30e9);
        // sector tech ETF: higher CAGR but stretched far above its 200wk SMA (overext brake bites)
        let mut tech = fx("iShares S&P 500 Information Technology UCITS ETF", "TECH.L", 85.0,
            &[("1M", -2.0), ("1Y", 17.0), ("5Y", 98.0), ("10Y", 438.0)]);
        tech.instrument_type = "ETF".into();
        tech.above_ma_pct = 61.0;
        // single stock with fundamentals: P/E value damp + ROE quality reward + vol/maxdd risk terms
        let mut stock = fx("Apple Inc.", "APL", 88.0,
            &[("1M", 3.0), ("1Y", 30.0), ("5Y", 150.0), ("10Y", 400.0)]);
        stock.instrument_type = "EQUITY".into();
        stock.pe_ratio = Some(30.0);
        stock.roe = Some(40.0);
        stock.volatility_pct = Some(2.0);
        stock.max_drawdown_pct = 40.0;
        // crypto pair: 5Y leg is "proven enough" (trust 1.0), looser crypto range floor applies
        let btc = fx("Bitcoin EUR", "BTC-EUR", 70.0, &[("1M", 5.0), ("1Y", 50.0), ("5Y", 400.0)]);

        // 1) EXACT scores, 2dp — the numeric pin
        let got: Vec<String> = [&broad, &tech, &stock, &btc]
            .iter()
            .map(|q| growth_score(q, &tuning).map_or("gated".into(), |s| format!("{s:.2}")))
            .collect();
        assert_eq!(got, ["6.51", "3.54", "9.60", "9.03"]);

        // 2) EXACT rank order when sorted by score — the membership/ordering pin
        let mut ranked: Vec<(&str, f64)> = [&broad, &tech, &stock, &btc]
            .iter()
            .filter_map(|q| growth_score(q, &tuning).map(|s| (q.ticker.as_str(), s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let order: Vec<&str> = ranked.iter().map(|(t, _)| *t).collect();
        assert_eq!(order, ["APL", "BTC-EUR", "CORE.L", "TECH.L"]);

        // 3) gates: each structural reject must stay a reject
        let lev = fx("Direxion Daily Tech Bull 3x", "LEV", 90.0, &[("1Y", 80.0), ("10Y", 900.0)]);
        assert_eq!(growth_score(&lev, &tuning), None); // leveraged decay vehicle
        let mut unknown_turnover = broad.clone();
        unknown_turnover.avg_turnover_eur = None;
        assert_eq!(growth_score(&unknown_turnover, &tuning), None); // (#20) unassessable liquidity
        let mut off_high = broad.clone();
        off_high.range_pct = 50.0;
        assert_eq!(growth_score(&off_high, &tuning), None); // below the 80% range floor
        let laggard = fx("Vanguard FTSE All-World UCITS ETF USD Acc", "LAG.L", 90.0,
            &[("1M", 1.0), ("1Y", 10.0), ("5Y", 20.0), ("10Y", 50.0)]);
        assert_eq!(growth_score(&laggard, &tuning), None); // 4.1%/yr < the 8%/yr CAGR floor
        let mut knife = broad.clone();
        knife.perf = fx("x", "x", 0.0, &[("1M", -20.0), ("1Y", 25.0), ("5Y", 100.0), ("10Y", 300.0)]).perf;
        assert_eq!(growth_score(&knife, &tuning), None); // 1M -20% through the -15% knife floor

        // 4) hold-core legs: exact reason string per failing leg, None on the canonical core
        assert_eq!(core::hold_miss_reason(&broad), None);
        assert_eq!(core::hold_miss_reason(&tech).as_deref(),
            Some("not a broad-index name (sector/thematic/factor tilt)"));
        let mut m = broad.clone();
        m.name = "Vanguard FTSE All-World Fund USD Acc".into();
        assert_eq!(core::hold_miss_reason(&m).as_deref(), Some("no UCITS token in the name"));
        m = broad.clone();
        m.ter_fallback = None;
        assert_eq!(core::hold_miss_reason(&m).as_deref(), Some("TER unknown"));
        m = broad.clone();
        m.ter_fallback = Some(0.30);
        assert_eq!(core::hold_miss_reason(&m).as_deref(), Some("TER 0.30% > 0.25% cap"));
        m = broad.clone();
        m.replication = Some("Swap");
        assert_eq!(core::hold_miss_reason(&m).as_deref(), Some("replication Swap (needs physical)"));
        m = broad.clone();
        m.use_of_profits = Some("Dist");
        assert_eq!(core::hold_miss_reason(&m).as_deref(), Some("share class Dist (needs Acc)"));
        m = broad.clone();
        m.aum_fallback = Some(0.5e9);
        assert_eq!(core::hold_miss_reason(&m).as_deref(), Some("AUM €0.5B < €1B floor"));

        // 5) asset-class split: instrumentType is authoritative, name marker is the fallback
        assert!(quote_is_etf(&broad) && quote_is_etf(&tech));
        assert!(!quote_is_etf(&stock));
        assert!(quote_is_etf(&Quote::stub("X", "€1", "", "Foo UCITS ETF Acc"))); // no meta -> name marker
    }

    /// HORIZONS-ordered perf legs from a sparse (label, %) list — the shape `Quote.perf` expects.
    fn legs(pairs: &[(&str, f64)]) -> Vec<Option<(String, f64)>> {
        HORIZONS
            .iter()
            .map(|(l, _)| pairs.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
            .collect()
    }

    /// A hold-suitable broad-index CORE fund: broad + UCITS name, physical, Acc, cheap TER, ≥€1B, IE.
    fn core_etf(ticker: &str, name: &str, aum: f64, ter: f64) -> Quote {
        let mut q = Quote::stub(ticker, "€100.00", "", name);
        q.instrument_type = "ETF".into();
        q.expense_ratio = Some(ter);
        q.replication = Some("Opt");
        q.use_of_profits = Some("Acc");
        q.aum_eur = Some(aum);
        q.domicile = Some("IE".to_string());
        q.life_cagr = Some(9.0);
        q.age_years = Some(12.0);
        q
    }

    /// (QA) `render` — the pure screen seam (takes already-built quotes, no network). A pinned name that
    /// FAILS the growth gate still reaches the table (render sentinel-scores it), so a single call drives
    /// the whole offline pipeline: nupl_factor, btc_relative tilt, ranked, print_lane → print_picks (rows
    /// + rank-flag marks + legend) → col_cell VALUE arms, turnover_note, gate review, bridge hints.
    /// Asserts the returned contract; printed lines go to captured test stdout. Cleans the gitignored
    /// cwd turnover cache render writes.
    #[test]
    fn render_growth_lane_smoke() {
        let tuning = BuyHeuristic::default();
        let w = Widths::default();
        let sectors: Vec<String> = Vec::new();
        let sector_of: HashMap<String, String> = HashMap::new();

        // pinned equity, no >2Y leg -> growth-gated -> render's pinned sentinel keeps it in the table.
        // rich fields make its row exercise col_cell's value arms instead of every cell reading n/a.
        let mut pin = Quote::stub("AAPL", "€200.00", "", "Apple Inc.");
        pin.instrument_type = "EQUITY".into();
        pin.drawdown_pct = 8.5;
        pin.avg_turnover_eur = Some(3.4e9);
        pin.volatility_pct = Some(1.3);
        pin.max_drawdown_pct = 34.0;
        pin.trend_r2 = 0.95;
        pin.above_ma_pct = 42.0;
        pin.life_cagr = Some(23.0);
        pin.age_years = Some(11.0);
        pin.pe_ratio = Some(31.5);
        pin.roe = Some(45.0);
        pin.rev_yoy = Some(12.3);
        pin.eps_yoy = Some(-4.5);
        pin.net_margin_fy = Some(25.6);
        pin.buyback_yoy = Some(-3.0);
        pin.intraday = [Some(0.1), Some(-0.2), Some(0.4)];
        pin.perf = legs(&[("1D", 0.3), ("1W", 1.1), ("1M", -2.0), ("1Y", 19.0)]);

        // Bitcoin base (btc_1y Some) + one alt -> the crypto lane + crypto_adjust/btc_relative tilt run.
        let mut btc = Quote::stub("BTC-USD", "€60000", "", "Bitcoin");
        btc.perf = legs(&[("1Y", 40.0)]);
        let mut eth = Quote::stub("ETH-USD", "€3000", "", "Ethereum");
        eth.perf = legs(&[("1Y", 55.0)]);

        let quotes = vec![pin, btc, eth];
        let pinned = vec!["AAPL".to_string()];
        let owned = Owned { stocks: ["aapl".to_string()].into(), ..Default::default() };

        // euphoric NUPL (>euphoria band) -> nupl_factor damps the crypto rows (the >1 branch).
        let (_text, tickers) = render(&quotes, 5, &tuning, &w, RenderCtx {
            nupl: Some(0.9), sectors: &sectors, sector_of: &sector_of, pinned: &pinned,
            owned: &owned, explain: None, show_hold_core: true, fund_pe: &HashMap::new(),
        });
        assert!(tickers.iter().any(|t| t == "AAPL"), "pinned gated name must still surface in the ranking");
        assert!(tickers.len() <= 5);

        // an --explain for a ticker that isn't in `quotes` at all -> the not-scanned branch.
        let (miss, _) = render(&quotes, 5, &tuning, &w, RenderCtx {
            nupl: None, sectors: &sectors, sector_of: &sector_of, pinned: &pinned,
            owned: &owned, explain: Some("ZZZZ"), show_hold_core: false, fund_pe: &HashMap::new(),
        });
        assert!(miss.is_some_and(|m| m.contains("wasn't scanned")));

        let _ = std::fs::remove_file(crate::config::data_path(".folioman_turnover_watch.txt")); // gitignored cache render wrote
    }

    /// (QA) `--explain TICKER` for a name that did NOT rank must name WHICH of the four things
    /// happened. All four printed ONE string before ("fails a growth gate, isn't EU-buyable, or wasn't
    /// scanned") — the same non-answer whichever applied, which is how a name failing 2+ gates ended up
    /// with no explanation anywhere in the tool (the near-miss tail needs EXACTLY one). This test is
    /// what stops the four collapsing back into one.
    #[test]
    fn explain_names_the_verdict() {
        let tuning = BuyHeuristic::default();
        let w = Widths::default();
        let (sectors, sector_of) = (Vec::<String>::new(), HashMap::<String, String>::new());
        let (pinned, owned) = (Vec::<String>::new(), Owned::default());

        // clears every growth gate; `cum8` sets the long CAGR (the 8Y rung is the ladder's, not 10Y).
        let good = |t: &str, cum8: f64| {
            let mut q = Quote::stub(t, "€100.00", "", &format!("{t} Corp"));
            q.instrument_type = "EQUITY".into();
            q.range_pct = 95.0;
            q.avg_turnover_eur = Some(3.0e9);
            q.age_years = Some(12.0);
            q.perf = legs(&[("1Y", 10.0), ("5Y", 40.0), ("8Y", cum8)]);
            q
        };
        let mut one_gate = good("ONEG", 300.0);
        one_gate.range_pct = 75.0; // only range fails (75 < 80), closely
        let mut two_gate = good("TWOG", 72.0); // 8Y 72% ≈ 7.0%/yr — 1pp under the 8.0 floor, close
        two_gate.range_pct = 75.0; //           …and range too: exactly the pair that vanishes
        let mut lev = good("LEVX", 300.0);
        lev.name = "Some 3x Daily Leveraged ETP".into(); // structural reject -> gate_failures None
        // dual-class twins (one Yahoo NAME, two tickers): the weaker leg clears every gate and is still
        // dropped from `picks` by the twin dedup — the case that reads as "gated out" if the explain
        // arm can't tell the two apart. Merely ranking below the printed cut is NOT this branch:
        // `target` searches the UNTRIMMED picks, so such a name still gets the score walkthrough.
        let (mut twin_a, mut twin_b) = (good("TWINA", 400.0), good("TWINB", 200.0));
        twin_a.name = "Twin Corp".into();
        twin_b.name = "Twin Corp".into();
        let quotes = vec![good("STRONG", 400.0), good("WEAK", 250.0), twin_a, twin_b, one_gate, two_gate, lev];

        let explain = |t: &str, n: usize| {
            render(&quotes, n, &tuning, &w, RenderCtx {
                nupl: None, sectors: &sectors, sector_of: &sector_of, pinned: &pinned,
                owned: &owned, explain: Some(t), show_hold_core: false, fund_pe: &HashMap::new(),
            })
            .0
            .unwrap_or_default()
        };

        assert!(explain("ZZZZ", 5).contains("wasn't scanned"), "absent from `quotes` = never fetched, not gated");
        assert!(explain("LEVX", 5).contains("isn't assessable"), "leveraged -> no gates to almost pass");
        let g1 = explain("ONEG", 5);
        assert!(g1.contains("fails 1 growth gate:") && g1.contains("range"), "must NAME the gate: {g1}");
        let g2 = explain("TWOG", 5);
        assert!(g2.contains("fails 2 growth gates:"), "the case with no other home in the tool: {g2}");
        assert!(g2.contains("range") && g2.contains("cagr"), "both gates named, not just the first: {g2}");
        // gated-out and unranked-but-clean are DIFFERENT answers; the old catch-all gave one string.
        assert!(gate_failures(&good("TWINB", 200.0), &tuning).is_some_and(|f| f.is_empty()), "fixture must clear every gate");
        let tw = explain("TWINB", 5);
        assert!(tw.contains("clears every growth gate"), "deduped as a twin, not rejected by a gate: {tw}");
        // a name that IS ranked gets the score walkthrough instead — even below the printed cut,
        // because `target` searches the untrimmed picks.
        assert!(explain("WEAK", 1).contains("SCORE"), "ranked name -> score math, no verdict");

        let _ = std::fs::remove_file(crate::config::data_path(".folioman_turnover_watch.txt"));
    }

    /// (QA) `hold_core_list` breadth-major sort + one-row-per-name dedup + per-tier cap ≤3, and the
    /// `print_hold_core` block over it (owned marker, empty-tier-0 hint, pinned near-miss reason). Pure.
    #[test]
    fn hold_core_list_and_print() {
        let mut quotes = vec![
            core_etf("VWCE.DE", "Vanguard FTSE All-World UCITS ETF", 20e9, 0.22), // tier 0
            core_etf("IWDA.DE", "iShares Core MSCI World UCITS ETF", 60e9, 0.20), // tier 1
            core_etf("EIMI.DE", "iShares Core MSCI Emerging Markets IMI UCITS ETF", 20e9, 0.18), // tier 2
            // tier 3 (S&P 500): four DISTINCT names -> the cheapest three survive the per-tier cap.
            core_etf("SPYL.DE", "SPDR S&P 500 UCITS ETF", 8e9, 0.03),
            core_etf("CSPX.DE", "iShares Core S&P 500 UCITS ETF", 70e9, 0.07),
            core_etf("VUAA.DE", "Vanguard S&P 500 UCITS ETF", 10e9, 0.07),
            core_etf("XDWL.DE", "Xtrackers S&P 500 UCITS ETF", 2e9, 0.09), // 4th S&P -> capped out
            core_etf("VUAA.L", "Vanguard S&P 500 UCITS ETF", 5e9, 0.15),  // dup NAME -> deduped
            core_etf("MEUD.DE", "Xtrackers MSCI Europe UCITS ETF", 6e9, 0.12), // tier 5
            Quote::stub("AAPL", "€1", "", "Apple Inc."),                   // not a fund -> excluded
        ];

        let cores = hold_core_list(&quotes);
        assert_eq!(core::hold_breadth_tier(&cores[0].name), 0, "all-world sorts first (broadest)");
        assert_eq!(cores.iter().filter(|q| core::hold_breadth_tier(&q.name) == 3).count(), 3, "S&P tier capped at 3");
        // (round 118) EM ranks ABOVE the US sleeve — DM+EM spans the planet, the S&P alone does not.
        let tiers: Vec<u8> = cores.iter().map(|q| core::hold_breadth_tier(&q.name)).collect();
        assert!(tiers.windows(2).all(|w| w[0] <= w[1]), "breadth-major order: {tiers:?}");
        assert_eq!(cores.iter().filter(|q| core::hold_breadth_tier(&q.name) == 2).count(), 1, "EM sleeve present");
        assert_eq!(cores.len(), 7, "1 all-world + 1 world + 1 EM + 3 S&P + 1 Europe (4th S&P + dup dropped)");
        let uniq: HashSet<&str> = cores.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(uniq.len(), cores.len(), "one row per fund name");
        assert!(!cores.iter().any(|q| q.name == "Apple Inc."), "a single stock is never a hold core");

        // print block: owned marker on a held core + a pinned broad-index ETF that misses a leg (near).
        let mut near = core_etf("VWRL.DE", "Vanguard FTSE All-World UCITS ETF Dist", 5e9, 0.22);
        near.use_of_profits = Some("Dist"); // fails the Acc leg -> not a core, but pinned+broad -> near-miss line
        quotes.push(near);
        let owned = Owned { stocks: ["vwce".to_string()].into(), ..Default::default() };
        let pinned: HashSet<&str> = ["VWRL.DE"].into();
        print_hold_core(&quotes, 9, &pinned, &owned);

        // no all-world fund present -> the "no ACWI fund with facts qualified" hint branch.
        let no_world: Vec<Quote> =
            quotes.iter().filter(|q| core::hold_breadth_tier(&q.name) != 0).cloned().collect();
        print_hold_core(&no_world, 9, &HashSet::new(), &Owned::default());
    }

    /// (#66) The per-tier counter must survive more names in one tier than a `u8` can hold. It was
    /// `[0u8; HOLD_TIERS]`, incremented once per distinct fund NAME, so the 256th name in a tier wrapped
    /// it back to 0 and the tier silently admitted three more rows. 300 distinct S&P 500 names is the
    /// smallest input that crosses the boundary; the cap must still read exactly HOLD_PER_TIER.
    ///
    /// This pin is the ONLY cover for that arithmetic, because the two profiles disagree about it: under
    /// `cargo t` (mutants profile, inherits dev) overflow-checks are on and the pre-fix line PANICS here,
    /// while `--release` leaves them off and would have wrapped in silence with no test able to see it.
    #[test]
    fn hold_core_tier_cap_survives_more_than_255_names_in_one_tier() {
        let quotes: Vec<Quote> = (0..300)
            .map(|i| core_etf(&format!("S{i:03}.DE"), &format!("Issuer{i:03} S&P 500 UCITS ETF"), 1e9, 0.10))
            .collect();
        let cores = hold_core_list(&quotes);
        assert_eq!(cores.len(), HOLD_PER_TIER, "one tier, 300 distinct names -> the cap, not a wrapped counter");
    }

    /// (#66) A NaN sort key must order rather than take the screen down. `unwrap_or` supplies a value
    /// for None and says nothing about NaN, so `partial_cmp(..).unwrap()` on the turnover tie-break
    /// panicked on any quote carrying one. The scorer here is constant, which forces every pair past the
    /// score comparator and onto the tie-break — the exact leg that used to panic.
    #[test]
    fn nan_tie_break_key_orders_instead_of_panicking() {
        let mut quotes = vec![
            core_etf("AAA.DE", "Alpha FTSE All-World UCITS ETF", 1e9, 0.10),
            core_etf("BBB.DE", "Beta FTSE All-World UCITS ETF", 1e9, 0.10),
        ];
        for q in &mut quotes {
            q.avg_turnover_eur = Some(f64::NAN);
        }
        let out = ranked(&quotes, &BuyHeuristic::default(), |_, _| Some(1.0), 0.0, &HashSet::new());
        assert_eq!(out.len(), 2, "a NaN tie-break key must order, not panic");
    }

    /// (QA) `col_cell` VALUE arms the `screen_columns_config` test leaves at n/a (it uses a bare stub):
    /// the number-formatting columns + the ≥1000% no-decimal path + the unknown-key fallback.
    #[test]
    fn col_cell_value_arms() {
        let mut q = Quote::stub("AAA", "€12.34", "", "Alpha");
        assert_eq!(cc("price", &q, 0.0, None, ""), "€12.34");
        assert_eq!(cc("ticker", &q, 0.0, None, ""), "AAA");
        assert_eq!(cc("market", &q, 0.0, None, ""), q.market.clone());
        q.drawdown_pct = 12.5;
        assert_eq!(cc("off-hi", &q, 0.0, None, ""), "-12.5%");
        assert_eq!(cc("upside", &q, 0.0, None, ""), format!("+{:.1}%", upside_to_high(12.5)));
        q.avg_turnover_eur = Some(3.4e9);
        assert_eq!(cc("turnover", &q, 0.0, None, ""), "€3.4B");
        q.volatility_pct = Some(1.3);
        assert_eq!(cc("vol", &q, 0.0, None, ""), "1.3%");
        q.max_drawdown_pct = 42.0;
        assert_eq!(cc("maxdd", &q, 0.0, None, ""), "-42%");
        q.trend_r2 = 0.87;
        assert_eq!(cc("r2", &q, 0.0, None, ""), "0.87");
        q.above_ma_pct = 61.0;
        assert_eq!(cc("abv-ma", &q, 0.0, None, ""), "+61%");
        q.above_ma_pct = 0.0;
        assert_eq!(cc("abv-ma", &q, 0.0, None, ""), "0%");
        q.age_years = Some(11.0);
        assert_eq!(cc("yrs", &q, 0.0, None, ""), "11.0"); // 1 decimal: "8" for a 7.7y record contradicted its own blank 8Y
        q.intraday = [Some(0.12), Some(-0.34), Some(2.0)];
        assert_eq!(cc("1h", &q, 0.0, None, ""), "+0.1%");
        assert_eq!(cc("6h", &q, 0.0, None, ""), "-0.3%");
        assert_eq!(cc("12h", &q, 0.0, None, ""), "+2.0%");
        // the "1d|1w|…|20y" arm via perf_pct; a ≥1000% cell drops the decimal so it still fits its column.
        q.perf = legs(&[("1D", 0.5), ("2Y", 41.2), ("8Y", 290.1), ("20Y", 2600.0)]);
        assert_eq!(cc("1d", &q, 0.0, None, ""), "+0.5%");
        assert_eq!(cc("20y", &q, 0.0, None, ""), "+2600%");
        assert_eq!(cc("2y", &q, 0.0, None, ""), "+41.2%");
        assert_eq!(cc("8y", &q, 0.0, None, ""), "+290.1%");
        assert_eq!(cc("1w", &q, 0.0, None, ""), "n/a"); // absent leg
        assert_eq!(cc("bogus", &q, 0.0, None, ""), "?"); // unknown key fallback
        // (perf_fill) the WTAI.MI shape: ~7.7y of record, so 1Y/5Y fill and 8Y/20Y are blank.
        let mut wtai = Quote::stub("W", "€1", "", "WTAI");
        wtai.perf = legs(&[("1Y", 12.0), ("5Y", 90.0)]);
        wtai.age_years = Some(7.7);
        wtai.life_return_pct = Some(282.0);
        // DEFAULT is off, and off BY CONSTRUCTION — `cov >= 1.0 && cov < 1.0` is unsatisfiable, so no
        // age and no life return can fill anything. `cc` uses BuyHeuristic::default().
        assert_eq!(cc("8y", &wtai, 0.0, None, ""), "n/a", "100.0 = off: no blank cell is ever filled");
        let on = BuyHeuristic { perf_fill_coverage_pct: 90.0, ..BuyHeuristic::default() };
        // 7.7y is 96% of the 8Y rung (2920d) -> fills, marked. It is 39% of the 20Y rung -> stays blank:
        // the bar is what separates "almost measured it" from "made it up".
        assert_eq!(col_cell("8y", &wtai, 0.0, None, "", &on, &HashMap::new()), "≈+282.0%");
        assert_eq!(col_cell("20y", &wtai, 0.0, None, "", &on, &HashMap::new()), "n/a");
        // THE property the whole design rests on: the fill is display-only because it never entered
        // `perf`, so `perf_pct` — and therefore every gate, `long_leg`, `spy_premium`, `twin_groups` —
        // still reads the 8Y leg as absent. If this ever passes, a fabricated leg is being scored.
        assert!(perf_pct(&wtai, "8Y").is_none(), "the fill must not have leaked into `perf`");
        // a leg that EXISTS is never replaced by the fill, whatever the coverage says
        assert_eq!(col_cell("5y", &wtai, 0.0, None, "", &on, &HashMap::new()), "+90.0%");
        // (H-cov) staying dead: this record SPANS 20 years and its 20Y leg is blank anyway (a zero past
        // price at the anchor). cov >= 1.0 -> refuse. Filling here is the exact bug core.rs's guard removed.
        let mut old = wtai.clone();
        old.age_years = Some(25.0);
        assert_eq!(col_cell("20y", &old, 0.0, None, "", &on, &HashMap::new()), "n/a", "a 25y record's blank 20Y is a data gap, not youth");
        // `leg` = the CAGR the growth rank scores, NOT the whole-life `cagr` cell. Top of the 20/8/5
        // ladder wins, so +2600% over 20y -> 17.9%/yr, and `cagr` stays n/a (this stub has no life_cagr)
        // — the two columns disagreeing is the whole point of adding this one.
        assert_eq!(cc("leg", &q, 0.0, None, ""), "+18%");
        assert_eq!(cc("cagr", &q, 0.0, None, ""), "n/a");
        // the cell is CAPPED at long_trend_cap (30). 5Y is the only rung here and +400% over 5y is
        // 38.0%/yr, so an uncapped cell would read "+38%" — reading "+30%" is what proves the printed
        // number is the one `trend_term` multiplies rather than the raw leg CAGR.
        let mut hot = Quote::stub("H", "€1", "", "Hot");
        hot.perf = legs(&[("5Y", 400.0)]);
        assert_eq!(cc("leg", &hot, 0.0, None, ""), "+30%");
        assert!(core::cagr(400.0, 5.0) > 37.0, "the cap must be what clamps this, not a weak leg");
        // (#3h) and with the cap OFF (0, the SHIPPED live value) the same row prints its raw leg CAGR.
        // This is the only assert that fails if `capped_trend`'s zero-guard regresses — without it
        // `long_cagr.min(0.0)` is 0.0 for every positive CAGR, so the cell would read "+0%" and BOTH
        // trend terms would be silently gutted while the table still printed plausible scores.
        let uncapped = BuyHeuristic { long_trend_cap: 0.0, ..BuyHeuristic::default() };
        assert_eq!(col_cell("leg", &hot, 0.0, None, "", &uncapped, &HashMap::new()), "+38%");
        // (#37) `peg` prints `fund.peg_yield` and NOTHING else — the number `growth_max_peg` cuts on.
        // It used to compute `pe_ratio / long_cagr_from` here, a second PEG that disagreed with the gate
        // (APH cut at 2.02 in the run that ranked ODFL printing 2.51). `pe_ratio` is deliberately left
        // set below and deliberately ignored: if this cell ever reads it again, this assert fails.
        let mut peg = Quote::stub("P", "€1", "", "Peg");
        peg.perf = legs(&[("5Y", 400.0)]);
        peg.pe_ratio = Some(20.0); // must NOT reach the cell — the P/E column's TTM basis is not the PEG's
        peg.life_cagr = Some(10.0); // ditto: the CAGR is already baked into peg_yield upstream
        // 40 chosen so the three candidate formulas give three DIFFERENT strings and the assert can
        // only pass one way: 100/40 = 2.50 (peg_yield), 20/37.97 = 0.53 (old leg), 20/10 = 2.00 (old life).
        peg.fund = Some(core::FundFactors { peg_yield: Some(40.0), ..Default::default() });
        assert_eq!(cc("peg", &peg, 0.0, None, ""), "2.50");
        // and it is INERT to the CAGR switch, because peg_yield already resolved it via long_cagr_pct.
        // The old cell moved 0.53 -> 2.00 across this same flip; a cell that still moves is re-deriving.
        let on_life = BuyHeuristic { use_life_cagr: true, ..BuyHeuristic::default() };
        assert_eq!(col_cell("peg", &peg, 0.0, None, "", &on_life, &HashMap::new()), "2.50");
        // no peg_yield -> n/a. NOT "missing data": the gate declined to price it (loss-maker / no growth).
        peg.fund = Some(core::FundFactors { peg_yield: None, ..Default::default() });
        assert_eq!(cc("peg", &peg, 0.0, None, ""), "n/a");
        assert_eq!(cc("leg", &Quote::stub("N", "€1", "", "No legs"), 0.0, None, ""), "n/a");
        // S-8Y renders the caller's pinned score, crypto included (BTC-EUR has a real 8Y leg); "n/a"
        // only when the caller had nothing to score at all. `q` HAS an 8Y leg -> the pin applied -> bare.
        assert_eq!(cc("score8y", &q, 7.7, Some(6.4), ""), "6.4");
        assert_eq!(cc("score8y", &q, 7.7, None, ""), "n/a");
        // no 8Y leg -> the pin fell back to the longest one -> "†" says so instead of passing the
        // full-history score off as an 8-year judgement.
        let mut short = Quote::stub("S", "€1", "", "Short");
        short.perf = legs(&[("1Y", 12.0), ("5Y", 60.0)]);
        assert_eq!(cc("score8y", &short, 7.7, Some(6.4), ""), "6.4†");
        let mut coin = Quote::stub("BTC-EUR", "€1", "", "Bitcoin");
        coin.instrument_type = "CRYPTOCURRENCY".into();
        assert_eq!(cc("score8y", &coin, 7.7, Some(6.4), ""), "6.4†"); // stub: no legs at all
    }

    /// (S-8Y) `as_8y_window` swaps EXACTLY the four >8y price stats and nothing else, and is the
    /// identity when there is no 8-year window to swap in. The identity case is what keeps the LIVE
    /// ranking bit-identical — every scored fixture in this file has `stats_8y: None`.
    #[test]
    fn as_8y_window_swaps_only_the_long_window_stats() {
        let mut q = Quote::stub("T", "€1", "", "n");
        q.perf = legs(&[("1Y", 12.0), ("8Y", 200.0), ("10Y", 400.0)]);
        q.range_pct = 95.0;
        q.trend_r2 = 0.90;
        q.max_drawdown_pct = -55.0;
        q.underwater_yrs = Some(4.0);
        q.above_ma_pct = 60.0;
        q.volatility_pct = Some(1.4);
        // None -> borrowed untouched
        assert!(matches!(as_8y_window(&q), Cow::Borrowed(_)));
        assert_eq!(as_8y_window(&q).range_pct, 95.0);

        q.stats_8y = Some(core::Stats8 { range_pct: 88.0, trend_r2: 0.96, max_drawdown_pct: -32.0, underwater_yrs: Some(1.5) });
        let w = as_8y_window(&q);
        assert!(matches!(w, Cow::Owned(_)));
        assert_eq!(w.range_pct, 88.0);
        assert_eq!(w.trend_r2, 0.96);
        assert_eq!(w.max_drawdown_pct, -32.0);
        assert_eq!(w.underwater_yrs, Some(1.5));
        // untouched: both windows already sit INSIDE 8 years (200wk SMA ≈ 3.8y, vol ≈ 1y), so
        // re-slicing them would be dead code — if that ever changes this assert is the tripwire.
        assert_eq!(w.above_ma_pct, 60.0);
        assert_eq!(w.volatility_pct, Some(1.4));
    }

    /// (S-8Y) the swap actually MOVES the score: `range_pct` is the `proximity` multiplier on the whole
    /// score, so a name whose last 8 years put it lower in its own range scores strictly lower than the
    /// same name judged over its full ~10y window. This is the one that proves the pin does something.
    #[test]
    fn as_8y_window_lower_range_scores_lower() {
        let tuning = BuyHeuristic::default();
        let mut q = Quote::stub("T", "€1", "", "n");
        q.perf = legs(&[("1Y", 30.0), ("5Y", 120.0), ("8Y", 300.0), ("10Y", 400.0)]);
        q.range_pct = 98.0;
        q.avg_turnover_eur = Some(5e9);
        let full = growth_score(&q, &tuning).expect("scoreable");
        // the oldest, cheapest bars leaving the window pull the percentile rank down
        q.stats_8y = Some(core::Stats8 { range_pct: 88.0, trend_r2: q.trend_r2, max_drawdown_pct: q.max_drawdown_pct, underwater_yrs: q.underwater_yrs });
        let pinned = growth_score(&as_8y_window(&q), &tuning).expect("scoreable");
        assert!(pinned < full, "8y window ranks it lower in its own range -> lower score ({pinned} vs {full})");
    }

    /// (QA) `turnover_note` both branches against a temp cache: first run -> None (writes baseline),
    /// later runs -> the overlap note (Jaccard %, moved count, singular/plural). Pure fs, no network.
    #[test]
    fn turnover_note_roundtrip() {
        let dir = std::env::var("CLAUDE_JOB_DIR")
            .map(|d| format!("{d}/tmp"))
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        let path = std::path::PathBuf::from(format!("{dir}/fm_turnover_test_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        assert!(turnover_note(&s(&["A", "B", "C"]), 3, &path).is_none(), "first run has no baseline");
        // vs {A,B,C}: {A,B,D} -> ∩{A,B}/∪{A,B,C,D} = 50%, one new name (singular).
        let note = turnover_note(&s(&["A", "B", "D"]), 3, &path).unwrap();
        assert!(note.contains("50% top-3 overlap"), "note = {note}");
        assert!(note.contains("(1 new name)"), "singular: {note}");
        // vs {A,B,D}: {A,X,Y} -> two new names (plural).
        let note3 = turnover_note(&s(&["A", "X", "Y"]), 3, &path).unwrap();
        assert!(note3.contains("new names"), "plural: {note3}");
        let _ = std::fs::remove_file(&path);
    }

    /// (QA) crypto render-time tilt: `crypto_adjust` skips equities, folds the NUPL cfactor then tilts
    /// vs Bitcoin's year via `btc_relative` — bounded 0.5×..2× so one moonshot can't run away. Pure.
    #[test]
    fn crypto_tilt_arms() {
        let tuning = BuyHeuristic::default();
        // equity: no crypto-market damp, no BTC base -> base passes through untouched (cfactor ignored).
        let stock = Quote::stub("AAPL", "€1", "", "Apple");
        assert_eq!(crypto_adjust(&stock, 10.0, &tuning, 0.5, Some(20.0)), 10.0);
        // btc_relative: unknown coin-or-btc 1Y, or w=0 -> unchanged.
        assert_eq!(btc_relative(None, Some(10.0), 5.0, 1.0), 5.0);
        assert_eq!(btc_relative(Some(10.0), None, 5.0, 1.0), 5.0);
        assert_eq!(btc_relative(Some(10.0), Some(5.0), 5.0, 0.0), 5.0);
        // far ahead of BTC -> capped at 2×; far behind -> floored at 0.5×.
        assert!((btc_relative(Some(10_000.0), Some(0.0), 5.0, 1.0) - 10.0).abs() < 1e-9, "2x ceiling");
        assert!((btc_relative(Some(0.0), Some(10_000.0), 5.0, 1.0) - 2.5).abs() < 1e-9, "0.5x floor");
        // crypto_adjust on BTC vs itself: edge 0 -> neutral 1×, so only the cfactor scales the base.
        let mut btc = Quote::stub("BTC-USD", "€1", "", "Bitcoin");
        btc.perf = legs(&[("1Y", 40.0)]);
        assert!((crypto_adjust(&btc, 10.0, &tuning, 0.8, Some(40.0)) - 8.0).abs() < 1e-9);
    }

    /// A quote that clears every growth gate with room: near its high, a 24.6%/yr 5Y leg, climbing on
    /// the year, calm on the month. Every fence test below moves ONE knob onto this fixture's own stat.
    fn gate_fixture() -> Quote {
        let mut q = Quote::stub("TEST", "€100.00", "", "Test Corp");
        q.instrument_type = "EQUITY".into();
        q.avg_turnover_eur = Some(1e9); // (#20) a KNOWN turnover — unknown is unassessable, not gated
        q.range_pct = 90.0;
        q.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("5Y", 200.0)]);
        q
    }

    /// (#54) `pin_dropped` must name exactly the cohort the CAGR pin costs, and nobody else. The pin
    /// rejects nothing directly — `long_leg_fixed` falls back to the longest leg when the pinned window
    /// is absent — so the ONLY way to attribute a missing row to it is the counterfactual, and there are
    /// two ways to get that wrong: indict a name that fails unpinned anyway, or miss one that fails only
    /// under the pin. Both are asserted here, along with silence at the shipped `fixed_cagr_years: 0`.
    #[test]
    fn pin_dropped_names_only_the_pins_casualties() {
        let mut q = gate_fixture();
        // a long record the pin discards: strong over 20Y, ordinary over the pinned 8Y window — the
        // AMZN shape (+30.4%/yr on its 20Y leg, +15.0%/yr on its 8Y one)
        q.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("5Y", 200.0), ("8Y", 300.0), ("20Y", 20000.0)]);
        let pinned_cagr = core::cagr(300.0, 8.0); // ~18.9%/yr
        let free_cagr = core::cagr(20000.0, 20.0); // ~30.0%/yr
        assert!(free_cagr > pinned_cagr + 5.0, "fixture must actually have a record the pin throws away");

        // a floor BETWEEN the two legs: clears on the 20Y record, fails on the pinned 8Y one
        let base = BuyHeuristic { growth_min_cagr: (pinned_cagr + free_cagr) / 2.0, ..BuyHeuristic::default() };
        let pin8 = BuyHeuristic { fixed_cagr_years: 8, ..base.clone() };
        assert!(growth_score(&q, &base).is_some(), "unpinned, the 20Y record clears the floor");
        assert!(growth_score(&q, &pin8).is_none(), "pinned to 8Y it must vanish — else there is nothing to report");

        let d = pin_dropped(&q, &pin8).expect("the pin is what broke the cagr fence");
        assert!(d.broke.contains(&"cagr"), "must name the fence the pin broke, got {:?}", d.broke);
        assert!(d.still.is_empty(), "nothing else fails at 0, so no 'not enough' note is owed");
        assert_eq!((d.pinned.1, d.free.1), (8.0, 20.0), "must quote the pinned window against the longest leg");
        assert!(
            (d.pinned.0 - pinned_cagr).abs() < 1e-6 && (d.free.0 - free_cagr).abs() < 1e-6,
            "both CAGRs must be the scorer's own numbers"
        );

        // silent at the shipped default — this block must cost nothing when the pin is off
        assert!(pin_dropped(&q, &base).is_none(), "no pin -> nothing to attribute to it");
        // the SAME fence fails either way -> the pin broke nothing new, so it must not be indicted
        let strict = BuyHeuristic { growth_min_cagr: free_cagr + 1.0, ..pin8.clone() };
        assert!(pin_dropped(&q, &strict).is_none(), "cagr fails at 0 too -> the pin did not break it");
        // still ranks under the pin -> never a casualty
        let loose = BuyHeuristic { growth_min_cagr: 0.0, ..pin8.clone() };
        assert!(pin_dropped(&q, &loose).is_none(), "ranks with the pin -> not a casualty");

        // THE AMZN SHAPE, and the reason the strict "would rank at 0" predicate was abandoned: a name the
        // pin genuinely damages while ANOTHER fence blocks it on both sides. It must still be listed, and
        // it must carry the "setting 0 is not enough" note naming that other fence.
        let both = BuyHeuristic { growth_min_range_pct: q.range_pct + 1.0, ..pin8.clone() };
        let d = pin_dropped(&q, &both).expect("a pin-broken fence must still list, even when another fence also fails");
        assert!(d.broke.contains(&"cagr") && !d.broke.contains(&"range"), "only the pin's own damage is its bill, got {:?}", d.broke);
        assert!(d.still.contains(&"range"), "the fence that fails at 0 too must be reported, got {:?}", d.still);
    }

    /// (#55) `proven_but_unranked` must name the cohort NO other tail can reach: a great long record,
    /// two failing gates, one of them gross. That shape is invisible to the funnel (names sole-blockers
    /// only), to near-miss (one gate), to the two-gate tail (both must be close) and to the leg-floor
    /// tails — which is how AMZN went missing with the tool knowing exactly why. Asserted here along
    /// with the two ways to make the list useless: admitting names with no record, and letting the CAGR
    /// pin shrink it. Also pins `unranked_reason`'s total-ness, since a blank reason is the original bug.
    #[test]
    fn proven_but_unranked_names_the_records_no_other_tail_reaches() {
        let mut q = gate_fixture();
        // the AMZN shape: strong over 20Y, ordinary over the pinned 8Y window
        q.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("5Y", 200.0), ("8Y", 300.0), ("20Y", 20000.0)]);
        let free_cagr = core::cagr(20000.0, 20.0); // ~30.0%/yr — the record, measured unpinned
        let pinned_cagr = core::cagr(300.0, 8.0); // ~18.9%/yr — what the pin sees instead

        // a floor between the two legs, so the name fails `cagr` under the pin, plus a GROSS second
        // fence — the combination every existing tail declines to print.
        let t = BuyHeuristic {
            fixed_cagr_years: 8,
            growth_min_cagr: (pinned_cagr + free_cagr) / 2.0,
            growth_min_range_pct: q.range_pct + 40.0,
            ..BuyHeuristic::default()
        };
        assert!(growth_score(&q, &t).is_none(), "fixture must actually be missing from the ranking");
        let (cagr, years, why) = proven_but_unranked(&q, &t).expect("a proven record that didn't rank must be named");
        assert!((cagr - free_cagr).abs() < 1e-6 && years == 20.0, "record must be the UNPINNED longest leg, got {cagr} on {years}Y");
        assert!(why.contains("cagr") && why.contains("range"), "every failing gate must be named, got {why}");

        // the pin must not be able to shrink this list: same name, same floor, no pin -> still listed
        // (it now fails only `range`). If this ever regresses, the block goes quiet exactly when the
        // user has turned on the knob that makes long records look ordinary.
        let unpinned = BuyHeuristic { fixed_cagr_years: 0, ..t.clone() };
        assert!(proven_but_unranked(&q, &unpinned).is_some(), "the pin must not gate membership of this list");

        // no proven record -> not this list's business, however many gates it fails
        let mut weak = gate_fixture();
        weak.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("5Y", 10.0)]); // ~1.9%/yr
        assert!(proven_but_unranked(&weak, &t).is_none(), "a weak record failing gates is the gates being right");

        // SHORT record, spectacular number: the live-config shape (`growth_min_leg_years: 0` admits a 2Y
        // rung) that filled the first run with 900%/yr one-year coins. A huge CAGR over 2 years is not a
        // proven record, and this block's own floor — not the rank-side knob — has to be what says so.
        let mut young = gate_fixture();
        young.perf = legs(&[("1M", 2.0), ("1Y", 20.0), ("2Y", 900.0)]); // ~216%/yr over 2Y
        let loose_leg = BuyHeuristic { growth_min_leg_years: 0.0, ..t.clone() };
        assert!(growth_score(&young, &loose_leg).is_none(), "fixture must be unranked, else this proves nothing");
        assert!(proven_but_unranked(&young, &loose_leg).is_none(), "a 2Y rung is not a proven long record at any CAGR");
        // and the floor tracks a STRICTER knob rather than pinning itself at 5
        let strict_leg = BuyHeuristic { growth_min_leg_years: 10.0, ..unpinned.clone() };
        assert!(proven_but_unranked(&q, &strict_leg).is_some(), "a 20Y leg still clears a 10Y floor");
        // ranks -> nothing to explain
        let clean = BuyHeuristic { growth_min_cagr: 0.0, growth_min_range_pct: 0.0, ..t.clone() };
        assert!(growth_score(&q, &clean).is_some() && proven_but_unranked(&q, &clean).is_none(), "a ranked name is not missing");

        // `unranked_reason` is TOTAL: every way out of `gate_failures` gets a word, including the two
        // that return None and used to print nothing at all.
        let mut lev = gate_fixture();
        lev.name = "Direxion Daily 3X Bull".into();
        assert_eq!(unranked_reason(&lev, &t), "leveraged", "a structural refusal must say which one");
        let mut no1y = gate_fixture();
        no1y.perf = legs(&[("1M", 2.0), ("5Y", 200.0)]); // long leg present, 1Y absent -> the other None
        assert_eq!(unranked_reason(&no1y, &t), "no 1Y history");
        assert_eq!(unranked_reason(&q, &clean), "clears every gate", "a ranked name still gets a sentence");
    }

    /// (QA) GATE FENCES, equity lane. The per-knob stress grid (2026-07-30) settled that the GATES, not
    /// the weights, are what moves this lane — and not one of them had an assert at its own boundary, so
    /// a `<` decaying into `<=` (or a knob read for the wrong lane) silently changes who is in the table
    /// with the whole suite still green. One pair per gate: the fixture's own stat AS the fence (must
    /// PASS — the bar is what it says, not one epsilon stricter) and a hair past it (must GATE).
    #[test]
    fn equity_gate_boundaries() {
        let d = BuyHeuristic::default();
        let leg = core::cagr(200.0, 5.0); // 24.57%/yr — what every CAGR fence here is measured against
        let scored = |q: &Quote, t: &BuyHeuristic| growth_score(q, t).is_some();

        // growth_min_cagr, LEG leg (picks.rs `long_cagr < min_cagr`)
        assert!(scored(&gate_fixture(), &BuyHeuristic { growth_min_cagr: leg, ..d.clone() }));
        assert!(!scored(&gate_fixture(), &BuyHeuristic { growth_min_cagr: leg + 0.01, ..d.clone() }));
        // (#3i) the SAME knob's second leg: whole-life CAGR, judged independently of the rung above —
        // a strong recent leg must not rescue a mediocre life. `None` life stays exempt (the backtest
        // had no life_cagr before 2026-07-27, and absent history is not a failed bar).
        let mut aged = gate_fixture();
        aged.life_cagr = Some(leg - 5.0);
        assert!(scored(&aged, &BuyHeuristic { growth_min_cagr: leg - 5.0, ..d.clone() }));
        assert!(!scored(&aged, &BuyHeuristic { growth_min_cagr: leg - 4.99, ..d.clone() }),
            "life-CAGR leg must gate on its own while the 5Y rung still clears");

        // growth_min_range_pct (`range_pct < min_range`)
        assert!(scored(&gate_fixture(), &BuyHeuristic { growth_min_range_pct: 90.0, ..d.clone() }));
        assert!(!scored(&gate_fixture(), &BuyHeuristic { growth_min_range_pct: 90.01, ..d.clone() }));

        // growth_min_1y_pct — `<=`, NOT `<`: at exactly the floor the name is OUT. The knob's receipt
        // rests on that (a 0.0 floor means "must be climbing", so a flat year fails).
        assert!(scored(&gate_fixture(), &BuyHeuristic { growth_min_1y_pct: 19.99, ..d.clone() }));
        assert!(!scored(&gate_fixture(), &BuyHeuristic { growth_min_1y_pct: 20.0, ..d.clone() }),
            "the 1Y floor rejects AT the bar, not just below it");

        // max_1m_drop_pct (falling-knife, also `<=`)
        assert!(scored(&gate_fixture(), &BuyHeuristic { max_1m_drop_pct: 1.99, ..d.clone() }));
        assert!(!scored(&gate_fixture(), &BuyHeuristic { max_1m_drop_pct: 2.0, ..d.clone() }));

        // growth_min_5y_pct — the CUMULATIVE leg floor, reached through `long_leg_floors` (`<=`)
        assert!(scored(&gate_fixture(), &BuyHeuristic { growth_min_5y_pct: 199.99, ..d.clone() }));
        assert!(!scored(&gate_fixture(), &BuyHeuristic { growth_min_5y_pct: 200.0, ..d.clone() }));

        // (#26) growth_maxdd_cap — `>`, so the cap itself is survivable; 0 = off
        let mut deep = gate_fixture();
        deep.max_drawdown_pct = 50.0;
        assert!(scored(&deep, &BuyHeuristic { growth_maxdd_cap: 50.0, ..d.clone() }));
        assert!(!scored(&deep, &BuyHeuristic { growth_maxdd_cap: 49.99, ..d.clone() }));
        assert!(scored(&deep, &BuyHeuristic { growth_maxdd_cap: 0.0, ..d.clone() }), "0 = off");

        // (#24) growth_max_above_ma — the blow-off cut ABOVE the brake cap (also `>`, 0 = off)
        let mut stretched = gate_fixture();
        stretched.above_ma_pct = 100.0;
        assert!(scored(&stretched, &BuyHeuristic { growth_max_above_ma: 100.0, ..d.clone() }));
        assert!(!scored(&stretched, &BuyHeuristic { growth_max_above_ma: 99.99, ..d.clone() }));

        // (#25) growth_require_lifetime_uptrend — rejects at exactly 0 (`<= 0.0`), either leg
        let up = BuyHeuristic { growth_require_lifetime_uptrend: true, ..d.clone() };
        let mut flat = gate_fixture();
        flat.trend_cagr = Some(0.0);
        assert!(!scored(&flat, &up));
        flat.trend_cagr = Some(0.01);
        assert!(scored(&flat, &up));
        let mut sunk = gate_fixture();
        sunk.life_cagr = Some(-1.0); // window trend fine, whole life negative -> still out
        // growth_min_cagr neutralized on BOTH sides: since (#3i) that floor reads life_cagr too, so a
        // -1.0 life fails it as well and the pair would be green while proving nothing about the flag.
        let dock = BuyHeuristic { growth_min_cagr: f64::NEG_INFINITY, ..up.clone() };
        assert!(!scored(&sunk, &dock));
        assert!(scored(&sunk, &BuyHeuristic { growth_require_lifetime_uptrend: false, ..dock.clone() }),
            "flag false = gate off");

        // (#37) growth_max_peg — knob is a PEG, applied to `peg_yield` = 100/PEG, so the comparison
        // flips: reject BELOW the bar. 1.6 -> bar 62.5.
        let peg_t = BuyHeuristic { growth_max_peg: 1.6, ..d.clone() };
        let with_fund = |peg_yield: Option<f64>, eps_ttm: Option<f64>| {
            let mut q = gate_fixture();
            q.fund = Some(core::FundFactors { peg_yield, eps_ttm, ..Default::default() });
            q
        };
        assert!(scored(&with_fund(Some(62.5), Some(5.0)), &peg_t), "AT the ceiling is not over it");
        assert!(!scored(&with_fund(Some(62.49), Some(5.0)), &peg_t));
        // the one deliberate departure from "None passes": no peg_yield AND negative EPS = a name with
        // no earnings to be cheap against, which must not walk through a valuation ceiling untested.
        assert!(!scored(&with_fund(None, Some(-1.0)), &peg_t));
        assert!(scored(&with_fund(None, Some(5.0)), &peg_t), "genuinely absent fundamentals still pass");
        assert!(scored(&with_fund(Some(1.0), Some(5.0)), &BuyHeuristic { growth_max_peg: 0.0, ..d.clone() }),
            "0 = off, however expensive");
    }

    /// (QA) GATE FENCES, crypto lane. Same fences, different knobs — and the split is the point: the
    /// equity bar must never reach a coin and vice versa, which is exactly what a copy-paste of the
    /// wrong `if crypto {}` arm would break (the lane picks its floor in six separate expressions).
    #[test]
    fn crypto_gate_boundaries() {
        let d = BuyHeuristic::default();
        let scored = |q: &Quote, t: &BuyHeuristic| growth_score(q, t).is_some();
        let coin = || {
            let mut q = gate_fixture();
            q.ticker = "SOL-EUR".into(); // the `-EUR`/`-USD` pair suffix IS the crypto classifier
            q.name = "Solana".into();
            q.instrument_type = String::new();
            q.range_pct = 50.0; // alts live far below ATH — the whole reason for the looser floor
            q.perf = legs(&[("1M", -10.0), ("1Y", -20.0), ("5Y", 200.0)]);
            q
        };
        let leg = core::cagr(200.0, 5.0);

        // growth_min_range_pct_crypto, and the equity bar CANNOT reach the coin
        assert!(scored(&coin(), &BuyHeuristic { growth_min_range_pct_crypto: 50.0, ..d.clone() }));
        assert!(!scored(&coin(), &BuyHeuristic { growth_min_range_pct_crypto: 50.01, ..d.clone() }));
        assert!(scored(&coin(), &BuyHeuristic { growth_min_range_pct: 95.0, ..d.clone() }),
            "the strict equity range bar must not touch the crypto lane");

        // min_1y_pct_crypto (`<=`) — the looser floor that keeps a red-year Bitcoin in the table
        assert!(scored(&coin(), &BuyHeuristic { min_1y_pct_crypto: -20.01, ..d.clone() }));
        assert!(!scored(&coin(), &BuyHeuristic { min_1y_pct_crypto: -20.0, ..d.clone() }));
        assert!(scored(&coin(), &BuyHeuristic { growth_min_1y_pct: 50.0, ..d.clone() }),
            "the equity 1Y floor must not touch the crypto lane");

        // max_1m_drop_pct_crypto (`<=`) — a -20%/month alt is normal, so its knife sits lower
        assert!(scored(&coin(), &BuyHeuristic { max_1m_drop_pct_crypto: -10.01, ..d.clone() }));
        assert!(!scored(&coin(), &BuyHeuristic { max_1m_drop_pct_crypto: -10.0, ..d.clone() }));

        // growth_min_cagr_crypto — its own floor, and the equity one stays out
        assert!(scored(&coin(), &BuyHeuristic { growth_min_cagr_crypto: leg, ..d.clone() }));
        assert!(!scored(&coin(), &BuyHeuristic { growth_min_cagr_crypto: leg + 0.01, ..d.clone() }));
        assert!(scored(&coin(), &BuyHeuristic { growth_min_cagr: 99.0, ..d.clone() }),
            "the equity CAGR gate must not touch the crypto lane");

        // (#26) growth_maxdd_cap_crypto — coins crash >80% every cycle, so the bar is "no worse than
        // Bitcoin" rather than the equity cap, which must stay off them entirely.
        let mut crashed = coin();
        crashed.max_drawdown_pct = 84.0;
        assert!(scored(&crashed, &BuyHeuristic { growth_maxdd_cap_crypto: 84.0, ..d.clone() }));
        assert!(!scored(&crashed, &BuyHeuristic { growth_maxdd_cap_crypto: 83.99, ..d.clone() }));
        assert!(scored(&crashed, &BuyHeuristic { growth_maxdd_cap: 20.0, ..d.clone() }),
            "the equity drawdown cap must not touch the crypto lane");
    }

    /// (funnel) `gate_failures` MIRRORS `score_parts` rather than sharing its gates — a duplication
    /// picks.rs justifies with "drift only mislabels the tail, never the rank". That justification dies
    /// the moment the screen's gate funnel aims a knob at the mirror's counts: a mislabel then aims the
    /// knob wrong. So pin the equivalence directly.
    ///
    /// ONE claim, both directions: a quote clears every gate (`Some([])`) if and only if the scorer
    /// ranks it (`Some(score)`). Refusals satisfy it too — both sides read `None`, so `false == false`.
    ///
    /// The (#23) row is why this test exists: until 2026-08-03 the mirror had no artifact leg, so a
    /// single-bar repricing read as "clears every gate" here while the scorer rejected it.
    #[test]
    fn gate_failures_agrees_with_the_scorer() {
        let d = BuyHeuristic::default();
        let clean = gate_fixture();

        let mut leveraged = gate_fixture();
        leveraged.name = "Some Index 2x Daily Leveraged".into();
        let mut commodity = gate_fixture();
        commodity.instrument_type = "ETF".into();
        commodity.name = "WisdomTree Physical Gold UCITS ETF".into();
        let mut stable = gate_fixture();
        stable.ticker = "USDT-EUR".into();
        let mut no_turnover = gate_fixture();
        no_turnover.avg_turnover_eur = None;
        let mut no_1y = gate_fixture();
        no_1y.perf = legs(&[("1M", 2.0), ("5Y", 200.0)]); // long leg present, 1Y absent -> the other None arm
        let mut no_history = gate_fixture();
        no_history.perf = legs(&[("1M", 2.0), ("1Y", 20.0)]);
        // (#23) 1D == 1W == 1M, all past the |1D| > 0.5 guard: one bar moved in a whole month
        let mut artifact = gate_fixture();
        artifact.perf = legs(&[("1D", 212.9), ("1W", 212.9), ("1M", 212.9), ("1Y", 20.0), ("5Y", 200.0)]);
        let mut deep = gate_fixture();
        deep.max_drawdown_pct = 95.0;
        let mut stretched = gate_fixture();
        stretched.above_ma_pct = 400.0;

        let cases: &[(&str, &Quote)] = &[
            ("clean", &clean), ("leveraged", &leveraged), ("commodity", &commodity),
            ("stablecoin", &stable), ("no-turnover", &no_turnover), ("no-1y", &no_1y),
            ("no-history", &no_history), ("artifact", &artifact), ("deep drawdown", &deep),
            ("stretched", &stretched),
        ];
        // knob sets chosen to move the fence ACROSS the fixture, so both verdicts flip inside the grid
        let tunings: &[(&str, BuyHeuristic)] = &[
            ("default", d.clone()),
            ("shipped-ish", BuyHeuristic {
                growth_min_cagr: 19.0, growth_min_range_pct: 80.0, growth_maxdd_cap: 84.0,
                growth_max_above_ma: 150.0, growth_min_1y_pct: 0.0, ..d.clone()
            }),
            ("everything off", BuyHeuristic {
                growth_min_cagr: 0.0, growth_min_range_pct: 0.0, growth_maxdd_cap: 0.0,
                growth_max_above_ma: 0.0, growth_min_1y_pct: -1e9, growth_min_5y_pct: -1e9,
                growth_require_lifetime_uptrend: false, ..d.clone()
            }),
        ];

        for (tname, t) in tunings {
            for (qname, q) in cases {
                let clears = gate_failures(q, t).is_some_and(|f| f.is_empty());
                let ranks = growth_score(q, t).is_some();
                assert_eq!(clears, ranks,
                    "{qname} under {tname}: gate_failures says clears={clears}, scorer says ranks={ranks} \
                     — the mirror drifted from score_parts, so the gate funnel's counts are wrong");
            }
        }

        // the refusal bucket must name the cause, and must fire for exactly the structural four
        assert_eq!(refusal_reason(&clean), None);
        assert_eq!(refusal_reason(&leveraged), Some("leveraged"));
        assert_eq!(refusal_reason(&commodity), Some("commodity"));
        assert_eq!(refusal_reason(&stable), Some("stablecoin"));
        assert_eq!(refusal_reason(&no_turnover), Some("no-turnover"));
        // the missing-1Y bail is NOT structural: gate_failures refuses it, refusal_reason does not —
        // which is exactly how the funnel tells the two apart (see refusal_reason's doc).
        assert_eq!(refusal_reason(&no_1y), None);
        assert!(gate_failures(&no_1y, &d).is_none());
        // and a no-history name is a FAIL, not a refusal — it explains itself in the table
        assert_eq!(refusal_reason(&no_history), None);
        assert_eq!(
            gate_failures(&no_history, &d).map(|f| f.iter().map(|(g, ..)| *g).collect::<Vec<_>>()),
            Some(vec!["history"])
        );
        // (#23) the artifact leg is present, and is never a near miss
        let art = gate_failures(&artifact, &d).unwrap();
        assert!(art.iter().any(|(g, _, close)| *g == "artifact" && !*close));
    }

    /// (QA) The SHIPPED tuning — the one every backtest receipt was graded under. Every other scoring
    /// test builds from `BuyHeuristic::default()`, so the values `tests/ci-settings.yaml` actually serves
    /// were never scored by any test: the whole file could stop being read and the suite would stay
    /// green. Parsed straight from the fixture, NOT through `config::load()`, which merges the operator's
    /// gitignored local overlay and would make this pin machine-dependent.
    #[test]
    fn shipped_config_pin() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ci-settings.yaml");
        let raw: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).expect("fixture readable")).unwrap();
        let tuning: BuyHeuristic = serde_yaml::from_value(raw["buy_heuristic"].clone())
            .expect("fixture's buy_heuristic parses as BuyHeuristic (deny_unknown_fields catches typos)");

        // The gates the per-knob grid (2026-07-30) shipped. Moving one is a measurement, never an edit:
        // re-run the stress grid, write the receipt, THEN update these numbers.
        assert_eq!(tuning.growth_min_cagr, 19.0, "shipped CAGR gate moved — re-measure, then update this pin");
        assert_eq!(tuning.growth_max_peg, 1.6, "shipped PEG ceiling moved — re-measure, then update this pin");
        assert_eq!(tuning.growth_min_range_pct, 80.0, "80 is settled (70 and 50 both measured worse)");
        assert_eq!(tuning.growth_min_1y_pct, 0.0, "removing this floor crashes the 20y lane edge -31%");
        assert_eq!(tuning.growth_min_leg_years, 5.0, "the 2Y rung ships only once the grid clears it — receipt beside the knob");
        // (2026-08-03) the 9-rung ladder's single peak: 20y lane edge +410.8 -> +459.3, rank-1 median
        // +6.0 -> +6.4, h2h 67% -> 76%, with +100 and +150 falling away on the other side.
        assert_eq!(tuning.growth_min_5y_pct, 75.0, "the 5Y equity floor is a measured peak (+75), not a taste call — re-run the ladder before moving it");
        // ...and its crypto twin must stay at what the SHARED knob served, or the equity move above
        // silently re-prices every coin against an equity-tuned bar (at +75: zero coins pass).
        assert_eq!(tuning.growth_min_5y_pct_crypto, 0.0, "moving this re-prices the crypto lane — the backtest cannot grade it, check the live table by eye");
        // ...and the one SCORE weight the growth re-fits keep moving. This pin exists to force the
        // conversation, and it has now done so twice: 0.65 came from a joint fit that also set the
        // smoothness weight, and when (#62) zeroed smoothness the fit's other terms were left standing
        // on a config that no longer existed. The re-measurement landed on 0.50 (#63). It is a
        // single-point rank-1 median spike, bracketed at 0.35 / 0.65 / 0.80, so drift in either
        // direction is a measurable regression rather than a taste call — but re-read the receipt
        // before trusting the number, not just this line.
        assert_eq!(tuning.growth_accel_weight, 0.50, "shipped accel weight moved — re-measure, then update this pin");

        // The two GATES that went missing. Both were switched off by hand in d58db7a, a commit about
        // something else, which said so in its own message — and nothing failed, for seven days, because
        // this test covered eight knobs and neither of these. The repo's gate sweep could not catch it
        // either: it only ever loosens a knob and grades what the loosening admits, so a knob moved in
        // the loosening direction is the one case the instrument is structurally blind to. These two
        // asserts are that blind spot's only cover. They pin OPPOSITE outcomes of the same measurement
        // round — one gate was restored, one was refused — so neither number is a default worth trusting
        // on sight; the (#64) receipts beside both knobs carry the runs.
        assert_eq!(tuning.growth_max_above_ma, 150.0, "the stretch gate is measured, not a taste call — its excluded cohort is negative on both moments at all three horizons, and the live table clears it by 3 points");
        assert_eq!(tuning.max_1m_drop_pct, -1000000.0, "the knife ships OFF and its own block argues for -20 — that is unresolved, not settled; read the (#64) receipt before moving it either way");

        // (#69) The SOFT brake, pinned beside the hard gate it was suspected of duplicating. The gate
        // above truncates everything past 150; this decides the ORDER of what survives, and the two are
        // not interchangeable — nine wide runs graded {100,150,200} under Ship Rule v2 and both looser
        // rungs lost. Loosening is the dangerous direction and the one the gate sweep cannot see, which
        // is exactly why it needs an assert rather than a receipt alone.
        assert_eq!(tuning.growth_overext_cap, 100.0, "the brake cap is graded under v2 — 150 is a NO-SHIP and 200 REFUSED on the rank-1 h2h guard; read the (#69) receipt before moving it");

        // (#70) Pinned because the ARGUMENT for raising it is sound and the MEASUREMENT still refuses
        // it — the combination most likely to get hand-edited by a reader who reasons it through and
        // stops there. `accel` subtracts `long_cagr`, so this weight minus `growth_accel_weight` is the
        // lane's slope in CAGR: 0.15 − 0.50 = −0.35 per %/yr, i.e. the lane penalises the compounding it
        // hunts, and lifting this knob genuinely raises rho, lane edge and OOS at all three horizons.
        // The graded top-3 book gets worse anyway. Three rungs refused on the worst-window guard.
        assert_eq!(tuning.growth_trend_weight, 0.15, "0.15 survived a v2 grid — 0.35/0.55/0.70 all REFUSED on the top-3 worst window, and 0.35 also breaks the 8y h2h guard; read the (#70) receipt, and note that rising lane edge is not a reason to move this");

        // …and that the lane still SCORES under them. A gate quartet this strict is one typo away from
        // an empty table, which no value assert above would notice.
        let mut strong = gate_fixture();
        strong.perf = legs(&[("1M", 2.0), ("1Y", 30.0), ("5Y", 300.0), ("8Y", 500.0)]);
        strong.life_cagr = Some(25.0);
        strong.fund = Some(core::FundFactors { peg_yield: Some(80.0), eps_ttm: Some(5.0), ..Default::default() });
        assert!(growth_score(&strong, &tuning).is_some(), "a 25%/yr cheap compounder must still rank");
        let mut mediocre = gate_fixture();
        mediocre.perf = legs(&[("1M", 2.0), ("1Y", 12.0), ("5Y", 80.0)]); // 12.5%/yr — fine, but not 19
        assert!(growth_score(&mediocre, &tuning).is_none(), "a sub-gate compounder must be cut");
    }

    /// (QA) GROWTH SCORE TERMS: every weight/cap knob that had no test of its own. Each pair moves ONE
    /// knob on a fixture built to excite that term and asserts the DIRECTION its receipt claims, plus the
    /// house `0 = off` convention where the knob has one. A weight that stops being read still produces
    /// plausible scores — only a comparison catches it.
    #[test]
    fn growth_term_knob_flips() {
        let d = BuyHeuristic::default();
        // 1Y ABOVE the 24.6%/yr 5Y leg so the acceleration term is live (it clamps at 0 from below).
        let hot = || {
            let mut q = gate_fixture();
            q.perf = legs(&[("1M", 2.0), ("1Y", 40.0), ("5Y", 200.0)]);
            q
        };
        let s = |q: &Quote, t: &BuyHeuristic| growth_score(q, t).expect("fixture clears every gate");

        // growth_trend_weight — reward per %/yr of the (capped) leg CAGR. Code default 0.35; ci-settings
        // ships 0.15 after (#3k) cut it as the winner's-curse cure. Both ends must stay load-bearing, so
        // the assert walks the shipped value and the code default rather than pinning either.
        let tw = |w: f64| s(&hot(), &BuyHeuristic { growth_trend_weight: w, ..d.clone() });
        assert!(tw(0.35) > tw(0.15) && tw(0.15) > tw(0.0), "trend reward must be monotone in its weight");

        // growth_accel_weight / growth_accel_cap — the 1Y-minus-CAGR term (raw accel = 40 − 24.57 = 15.43
        // here, well under the 50 cap, so the cap arm below has to move the cap to see it).
        let aw = |w: f64| s(&hot(), &BuyHeuristic { growth_accel_weight: w, ..d.clone() });
        assert!(aw(0.5) > aw(0.2) && aw(0.2) > aw(0.0), "accel reward must be monotone in its weight");
        let ac = |c: f64| s(&hot(), &BuyHeuristic { growth_accel_weight: 0.5, growth_accel_cap: c, ..d.clone() });
        assert!(ac(50.0) > ac(5.0), "the cap must clamp an accel above it");
        assert!((ac(50.0) - ac(500.0)).abs() < 1e-9, "a cap above the value cannot bite");

        // (1) growth_overext_floor — how much score survives at FULL stretch above the 200wk SMA.
        // Higher floor = weaker brake. 1.0 = brake off entirely.
        let mut stretched = hot();
        stretched.above_ma_pct = d.growth_overext_cap; // pinned at the cap -> the damp IS the floor
        let of = |f: f64| s(&stretched, &BuyHeuristic { growth_overext_floor: f, ..d.clone() });
        assert!(of(1.0) > of(0.2) && of(0.2) > of(0.05), "a higher floor must brake LESS");
        // `above_ma_pct` reaches the score through the brake and nothing else, so floor 1.0 must make a
        // maximally-stretched name score EXACTLY its at-trend twin. That identity is what "1.0 = off" means.
        assert!((of(1.0) - s(&hot(), &d)).abs() < 1e-9, "floor 1.0 = brake off, not merely weaker");

        // (M) growth_mom121_cap — clamps 12-1 momentum (here (1.40/1.02−1) = +37.3) before weighting.
        // Inert at the shipped weight 0, so arm the weight to see the cap at all.
        let mc = |c: f64| s(&hot(), &BuyHeuristic { growth_mom121_weight: 0.1, growth_mom121_cap: c, ..d.clone() });
        assert!(mc(50.0) > mc(5.0), "the 12-1 cap must clamp a momentum above it");
        assert!((s(&hot(), &BuyHeuristic { growth_mom121_cap: 5.0, ..d.clone() }) - s(&hot(), &d)).abs() < 1e-9,
            "weight 0 = the whole term inert, cap included");

        // (L) growth_turnover_weight — liquidity tilt, ln(turnover/€1B), added OUTSIDE the brake.
        let mut liquid = hot();
        liquid.avg_turnover_eur = Some(32e9); // an NVDA-class line -> ln(32) ≈ 3.47
        let lw = |w: f64| s(&liquid, &BuyHeuristic { growth_turnover_weight: w, ..d.clone() });
        assert!(lw(0.5) > lw(0.0), "the liquidity tilt must lift a deep-liquid line");
        assert!((s(&hot(), &BuyHeuristic { growth_turnover_weight: 0.5, ..d.clone() }) - s(&hot(), &d)).abs() < 1e-9,
            "at exactly €1B the ln ratio is 0 — the tilt lifts ABOVE €1B, it does not shift everything");

        // (Item 20) growth_value_weight — dials the BLIND P/E multiplier toward neutral 1.0. A rich name
        // scores HIGHER as the weight falls, which is the whole point of the knob (the validated edge was
        // measured with this term off).
        let mut rich = hot();
        rich.pe_ratio = Some(40.0); // 2× ref_pe -> the damp end of value_factor
        let vw = |w: f64| s(&rich, &BuyHeuristic { growth_value_weight: w, ..d.clone() });
        assert!(vw(0.0) > vw(0.5) && vw(0.5) > vw(1.0), "less P/E authority = less damp on a rich name");

        // (C) calmar_cap — clamps the CAGR/max-drawdown ratio inside risk_bonus.
        let mut scarred = hot();
        scarred.max_drawdown_pct = 50.0; // leg 24.6%/yr / 50 -> ratio 0.49
        let cc = |c: f64| s(&scarred, &BuyHeuristic { calmar_weight: 1.0, calmar_cap: c, ..d.clone() });
        assert!(cc(2.0) > cc(0.1), "the calmar cap must clamp a ratio above it");

        // (#73) growth_min_cagr's LEG 2 — the whole-life reject bar, now windowed. This replaces (#3l)'s
        // assert that `capped_cagr` feeds the SCORE: it no longer does, and the branch that did is
        // deleted. The field's one remaining reader is `life_leg_cagr`, so these four asserts are its
        // entire contract. `capped_cagr` is None exactly when `life_cagr_max_years` is 0, which is why
        // the knob itself never appears here — presence IS the switch.
        //
        // TWO-SIDED ON PURPOSE. The receipt claims the window both ADMITS names their dead decade blocks
        // and REJECTS has-beens whose early run still flatters a 20Y rung; a one-directional test would
        // prove half of it and let the other half rot. Leg 1 is 24.6%/yr here (the 5Y 200% rung) and the
        // floor is the code default 8.0, so leg 1 passes in all four and only leg 2 decides.
        let leg2 = |life: Option<f64>, capped: Option<f64>| {
            let mut q = hot();
            q.life_cagr = life;
            q.capped_cagr = capped;
            growth_score(&q, &d).is_some()
        };
        assert!(leg2(Some(25.0), None), "0 = off: a 25%/yr lifetime clears the bar, exactly as before");
        assert!(!leg2(Some(4.0), None), "0 = off: the uncapped lifetime must still reject");
        assert!(!leg2(Some(25.0), Some(4.0)), "armed: a dead window must reject a flattering lifetime");
        assert!(leg2(Some(4.0), Some(25.0)), "armed: a live window must rescue a name its dead decade blocks");

        // The footer has to name the span it actually measured. Printing a windowed CAGR as "since
        // listing" is the same class of defect as the `cagr-life` vs `lifetime` label split this
        // block's neighbour documents: a reason string describing a measurement nobody took.
        let life_entry = |life: f64, capped: Option<f64>| {
            let mut q = hot();
            q.life_cagr = Some(life);
            q.capped_cagr = capped;
            gate_failures(&q, &d).unwrap_or_default().into_iter().find(|(k, _, _)| *k == "cagr-life")
        };
        let life_fail = |life: f64, capped: Option<f64>| {
            life_entry(life, capped).expect("a failing life leg must produce a `cagr-life` footer")
        };
        // The bar is `<`, NOT `<=`: a name sitting exactly ON the floor is a pass, and printing it as a
        // failure would dock the one name the number was chosen to admit. `score_parts` has the same
        // boundary two functions up and the suite already pinned that one — this is its missing mirror.
        assert!(life_entry(8.0, None).is_none(), "exactly at the 8.0 floor clears leg 2, no footer");
        assert!(life_fail(4.0, None).1.contains("since listing"), "unwindowed span: {}", life_fail(4.0, None).1);
        assert!(life_fail(4.0, Some(4.0)).1.contains("capped window"), "windowed span: {}", life_fail(4.0, Some(4.0)).1);

        // The NEAR-MISS FLAG (the tuple's third slot) had no assert anywhere, on either leg — a hole
        // (#73) inherited rather than made, and only found because touching this line handed it to the
        // mutation gate. It decides whether the footer prints a name as "one notch out" or as a plain
        // reject, so a silently inverted flag mislabels every borderline compounder in the table.
        // Floor is the code default 8.0, margin 1.5, so the near band is [6.5, 8.0). Both values are
        // chosen to pin the ARITHMETIC and not just the comparison: 7.0 is inside the real band, and
        // 6.0 is outside it but inside the band a `min_cagr / 1.5` slip would open (5.33).
        assert!(life_fail(7.0, None).2, "7.0%/yr is 1.0 under the 8.0 floor — a near miss");
        assert!(!life_fail(6.0, None).2, "6.0%/yr is 2.0 under the floor — past the 1.5 margin, not near");
    }

    /// (QA) ON-SALE (foil) lane knobs — the other half of `buy_heuristic`'s untested surface. The foil
    /// shares `long_trend_cap`, `calmar_*` and the tax knobs with the growth lane, so a knob edited "for
    /// the growth lane" moves this one too; these asserts are what notices.
    #[test]
    fn foil_term_knob_flips() {
        let d = BuyHeuristic::default();
        // an on-sale candidate: 40 points down its own range (that, NOT the off-high %, is `cheapness`),
        // still compounding, below its long SMA, paying a dividend.
        let foil = || {
            let mut q = Quote::stub("FOIL", "€50.00", "", "Foil Corp");
            q.instrument_type = "EQUITY".into();
            q.avg_turnover_eur = Some(1e9);
            q.range_pct = 60.0; // cheapness = 100 − 60 = 40
            q.below_ma_pct = 50.0;
            q.price_eur = Some(50.0);
            q.div_eur = vec![Some(2.0)]; // DIV_HORIZONS 1Y first -> a 4% trailing yield
            q.perf = legs(&[("1D", 0.5), ("1W", 1.0), ("1M", 2.0), ("1Y", 5.0), ("5Y", 60.0), ("8Y", 100.0), ("10Y", 200.0)]);
            q
        };
        let s = |q: &Quote, t: &BuyHeuristic| buy_score(q, t).expect("foil fixture clears every gate");

        // (#4) discount_weight — the dip reward, demoted to 0.35 because deepest-dip ranking carries no
        // selection skill. Still load-bearing, so it must still move the score.
        let dw = |w: f64| s(&foil(), &BuyHeuristic { discount_weight: w, ..d.clone() });
        assert!(dw(1.0) > dw(0.35) && dw(0.35) > dw(0.0), "dip reward monotone in its weight");

        // discount_cap — clamps the (vol-normalized) dip. It ALSO drives `discount_frac`, so a bigger cap
        // cuts the long-trend reward's fraction: assert the cap is read, not a direction it doesn't own.
        let dc = |c: f64| s(&foil(), &BuyHeuristic { discount_cap: c, ..d.clone() });
        assert!((dc(35.0) - dc(20.0)).abs() > 1e-9, "the dip cap must reach the score");
        assert!((dc(35.0) - dc(35.0000001)).abs() < 1e-6);

        // normal_volatility_pct — the divisor that makes a dip comparable across a calm stock and a wild
        // coin. A calmer-than-normal name's dip is AMPLIFIED, so raising the reference lifts it further.
        let mut calm = foil();
        calm.volatility_pct = Some(1.0);
        let nv = |n: f64| s(&calm, &BuyHeuristic { normal_volatility_pct: n, discount_cap: 100.0, ..d.clone() });
        assert!(nv(4.0) > nv(2.0), "a higher reference vol amplifies a calm name's dip");

        // (C) cheap_weight / cheap_cap — the below-SMA reward.
        let cw = |w: f64| s(&foil(), &BuyHeuristic { cheap_weight: w, ..d.clone() });
        assert!(cw(0.2) > cw(0.07) && cw(0.07) > cw(0.0));
        let cc = |c: f64| s(&foil(), &BuyHeuristic { cheap_weight: 0.2, cheap_cap: c, ..d.clone() });
        assert!(cc(60.0) > cc(10.0), "the cheap cap must clamp a below-SMA % above it");

        // (D/#61) onsale_dividend_weight — BEHAVIOUR, not just the parse test config.rs already has.
        // SPLIT from the growth lane's `dividend_weight` for the same reason, and with the same two
        // assertions, as the `onsale_sharpe_weight` pair below: wide at 12y this lane ranked BACKWARDS
        // while paying for trailing yield (rho -0.14 -> +0.03, edge -65.0 -> +85.7 at weight 0).
        let dv = |w: f64| s(&foil(), &BuyHeuristic { onsale_dividend_weight: w, ..d.clone() });
        assert!(dv(3.0) > dv(1.5) && dv(1.5) > dv(0.0), "the yield reward must be monotone in its weight");
        assert!((s(&foil(), &BuyHeuristic { dividend_weight: 9.0, ..d.clone() }) - s(&foil(), &d)).abs() < 1e-9,
            "the GROWTH dividend weight must not touch the on-sale lane");

        // (B) onsale_sharpe_weight — SPLIT from the growth lane's `sharpe_weight` (growth wants 0.15,
        // on-sale measured better at 0). The split is the thing worth pinning: moving one must not move
        // the other's lane.
        let mut volatile = foil();
        volatile.volatility_pct = Some(2.0);
        let ow = |w: f64| s(&volatile, &BuyHeuristic { onsale_sharpe_weight: w, ..d.clone() });
        assert!(ow(0.3) > ow(0.0), "the on-sale Sharpe weight must reach the on-sale score");
        assert!((s(&volatile, &BuyHeuristic { sharpe_weight: 0.9, ..d.clone() }) - s(&volatile, &d)).abs() < 1e-9,
            "the GROWTH Sharpe weight must not touch the on-sale lane");

        // long_trend_weight — the foil's own CAGR reward (scaled by how on-sale the name is).
        let lw = |w: f64| s(&foil(), &BuyHeuristic { long_trend_weight: w, ..d.clone() });
        assert!(lw(1.0) > lw(0.5) && lw(0.5) > lw(0.0));

        // (B) sustained_decline_pct — the value-trap dock fires only when 1Y AND 5Y are BOTH under the
        // bar. `min_long_pct` is neutralized so the bled fixture can reach the score at all.
        let mut bled = foil();
        bled.perf = legs(&[("1D", 0.5), ("1W", 1.0), ("1M", 2.0), ("1Y", -50.0), ("5Y", -50.0), ("8Y", 100.0), ("10Y", 200.0)]);
        let open = BuyHeuristic { min_long_pct: -1e9, min_1y_pct: -1e9, ..d.clone() };
        let docked = s(&bled, &BuyHeuristic { sustained_decline_pct: -40.0, ..open.clone() });
        let undocked = s(&bled, &BuyHeuristic { sustained_decline_pct: -60.0, ..open.clone() });
        assert!(docked < undocked, "a multi-year bleed under the bar must be docked");

        // momentum_bounce / momentum_knife — pure arms first (no dip -> no timing at all), then the knob
        // wiring through the score.
        let mut dipped = foil();
        dipped.perf = legs(&[("1D", 0.5), ("1W", 1.0), ("1M", -5.0), ("1Y", 5.0), ("5Y", 60.0), ("8Y", 100.0), ("10Y", 200.0)]);
        assert_eq!(momentum_factor(&foil(), 1.5, 0.5), 1.0, "up on the month -> nothing to time");
        assert_eq!(momentum_factor(&dipped, 1.5, 0.5), 1.5, "green week off a monthly dip -> bounce");
        let mut only_today = dipped.clone();
        only_today.perf = legs(&[("1D", 0.5), ("1W", -1.0), ("1M", -5.0), ("1Y", 5.0), ("5Y", 60.0), ("8Y", 100.0), ("10Y", 200.0)]);
        assert_eq!(momentum_factor(&only_today, 1.5, 0.5), 1.25, "only today green -> half the premium");
        let mut falling = dipped.clone();
        falling.perf = legs(&[("1D", -0.5), ("1W", -1.0), ("1M", -5.0), ("1Y", 5.0), ("5Y", 60.0), ("8Y", 100.0), ("10Y", 200.0)]);
        assert_eq!(momentum_factor(&falling, 1.5, 0.5), 0.5, "still falling -> the knife dock");
        assert!(s(&dipped, &BuyHeuristic { momentum_bounce: 1.5, ..d.clone() }) > s(&dipped, &d),
            "the bounce knob must reach the score (1.0 = neutral, as shipped)");
        assert!(s(&falling, &BuyHeuristic { momentum_knife: 0.5, ..d.clone() }) < s(&falling, &d),
            "the knife knob must reach the score");
    }

    /// (#41 / ETF floor) DISPLAY trims — the three knobs whose whole effect used to be printed text and
    /// nothing else. `lane_split` exists so they can be asserted; before it, a wrong floor or a redundancy
    /// skip that ate the table was visible only by reading a screen run by eye.
    #[test]
    fn lane_display_trims() {
        let d = BuyHeuristic::default();
        let none: HashSet<&str> = HashSet::new();
        let all_sectors: Vec<String> = Vec::new();
        let no_pe: FundPeMap = HashMap::new(); // no look-through P/E -> the (#37) fund ceiling is a no-op
        // 36 monthly returns — corr_tail needs >=12 overlapping to judge a pair at all. TWIN mirrors LEAD
        // exactly (rho +1.0); ANTI is its inverse (rho −1.0), so no cap can call ANTI a second copy.
        let up: Vec<f64> = (0..36).map(|i| if i % 2 == 0 { 3.0 } else { -1.0 }).collect();
        let down: Vec<f64> = up.iter().map(|v| -v).collect();
        let stock = |t: &str, trail: &[f64]| {
            let mut q = Quote::stub(t, "€100.00", "", &format!("{t} Corp"));
            q.instrument_type = "EQUITY".into();
            q.trail_monthly = trail.to_vec();
            q
        };
        let (lead, twin, anti) = (stock("LEAD", &up), stock("TWIN", &up), stock("ANTI", &down));
        let rows = || vec![(&lead, 12.0), (&twin, 11.0), (&anti, 10.0)];
        let names = |v: &[(&Quote, f64)]| v.iter().map(|(q, _)| q.ticker.clone()).collect::<Vec<_>>();

        // 0 = off: the skip never runs, every ranked row reaches the table.
        let (s0, _, _) = lane_split(rows(), 10, &all_sectors, &d, &none, &no_pe);
        assert_eq!(names(&s0), ["LEAD", "TWIN", "ANTI"], "cap 0 = off");
        // armed: the perfect twin is dropped, the anti-correlated name is not.
        let armed = BuyHeuristic { growth_corr_cap: 0.9, ..d.clone() };
        let (s1, _, _) = lane_split(rows(), 10, &all_sectors, &armed, &none, &no_pe);
        assert_eq!(names(&s1), ["LEAD", "ANTI"], "the SECOND copy of a bet goes, the diversifier stays");
        // a pin outranks the skip, exactly as it outranks the score floor.
        let pin_twin: HashSet<&str> = ["TWIN"].into_iter().collect();
        let (s2, _, _) = lane_split(rows(), 10, &all_sectors, &armed, &pin_twin, &no_pe);
        assert_eq!(names(&s2), ["LEAD", "TWIN", "ANTI"], "a pin means always show me this");
        // THE reason this knob ships 0 (#41 receipt): decorrelate_keep stops at `n`, so once the pool runs
        // out of uncorrelated candidates the skip TRUNCATES the table instead of refilling it.
        let (s3, _, _) = lane_split(rows(), 1, &all_sectors, &armed, &none, &no_pe);
        assert_eq!(names(&s3), ["LEAD"], "kept stops at n — no refill from below when nothing uncorrelated is left");

        // (#75) VALUE BRAKE — the cross-sectional peg_yield floor. HIGH peg_yield = cheap for the growth
        // delivered, so the brake cuts the LOW tail. Cohort of four priced 100/80/60/40 plus one name
        // carrying no PEG at all; scores descend with cheapness so a plain score trim could never produce
        // these orders on its own.
        let priced = |t: &str, peg: Option<f64>| {
            let mut q = stock(t, &up);
            q.fund = Some(core::FundFactors { peg_yield: peg, ..Default::default() });
            q
        };
        let (cheap, mid, dear, dearest, unpriced) =
            (priced("CHEAP", Some(100.0)), priced("MID", Some(80.0)), priced("DEAR", Some(60.0)),
             priced("DEAREST", Some(40.0)), priced("NOPEG", None));
        let vrows = || vec![(&cheap, 12.0), (&mid, 11.0), (&dear, 10.0), (&dearest, 9.0), (&unpriced, 8.0)];
        let all = ["CHEAP", "MID", "DEAR", "DEAREST", "NOPEG"];
        // 0 = off: byte-identical to the pre-(#75) table, which is what makes the default a no-op.
        let (v0, _, _) = lane_split(vrows(), 10, &all_sectors, &d, &none, &no_pe);
        assert_eq!(names(&v0), all, "0 = off");
        // 25% of the FOUR names carrying a PEG -> index 1 of [40,60,80,100] -> floor 60. NOPEG is not in
        // the cohort the percentile is taken over, and is kept regardless: unjudgeable is not a verdict.
        let brake25 = BuyHeuristic { growth_value_floor_pct: 25.0, ..d.clone() };
        let (v1, _, _) = lane_split(vrows(), 10, &all_sectors, &brake25, &none, &no_pe);
        assert_eq!(names(&v1), ["CHEAP", "MID", "DEAR", "NOPEG"], "the dearest quarter goes; a name with no PEG stays");
        // AT the floor is not below it — the same boundary `drop_bottom_book` uses (`v < t` rejects).
        let brake50 = BuyHeuristic { growth_value_floor_pct: 50.0, ..d.clone() };
        let (v2, _, _) = lane_split(vrows(), 10, &all_sectors, &brake50, &none, &no_pe);
        assert_eq!(names(&v2), ["CHEAP", "MID", "NOPEG"], "floor 80 keeps MID at exactly 80");
        // A pin outranks the brake, exactly as it outranks the score floor and the redundancy skip.
        let pin_dearest: HashSet<&str> = ["DEAREST"].into_iter().collect();
        let (v3, _, _) = lane_split(vrows(), 10, &all_sectors, &brake50, &pin_dearest, &no_pe);
        assert_eq!(names(&v3), ["CHEAP", "MID", "DEAREST", "NOPEG"], "a pin means always show me this");
        // Nobody priced -> no floor -> nothing cut. The failure mode this guards is the inverse reading,
        // where an absent cohort means "everything is below the floor" and the table empties itself.
        let (v4, _, _) = lane_split(vec![(&unpriced, 8.0)], 10, &all_sectors, &brake50, &none, &no_pe);
        assert_eq!(names(&v4), ["NOPEG"], "no PEG anywhere in the cohort -> no verdict, no cut");

        // ETF lane runs its OWN floor. Same score, two floors, opposite verdicts.
        let mut fund = Quote::stub("VWCE.DE", "€120.00", "", "Vanguard FTSE All-World UCITS ETF");
        fund.instrument_type = "ETF".into();
        let etf_rows = || vec![(&fund, 4.0)];
        let low = BuyHeuristic { growth_min_score: 5.0, growth_min_score_etf: 3.0, ..d.clone() };
        let (_, e1, _) = lane_split(etf_rows(), 10, &all_sectors, &low, &none, &no_pe);
        assert_eq!(e1.len(), 1, "the STOCK floor must not reach the ETF lane");
        let high = BuyHeuristic { growth_min_score_etf: 5.0, ..low.clone() };
        let (_, e2, _) = lane_split(etf_rows(), 10, &all_sectors, &high, &none, &no_pe);
        assert!(e2.is_empty(), "its own floor does");
        // ETFs carry no GICS sector, so the filter matches the fund NAME (stocks are pre-filtered at fetch).
        let health: Vec<String> = vec!["health".into()];
        assert!(lane_split(etf_rows(), 10, &health, &low, &none, &no_pe).1.is_empty(), "sector filter reads the fund name");
        assert_eq!(lane_split(rows(), 10, &health, &low, &none, &no_pe).0.len(), 3, "…and never the stock lane");

        // Crypto is trimmed by NOTHING here — the lane is ranked vs Bitcoin and shown whole.
        let mut coin = Quote::stub("SOL-EUR", "€150.00", "", "Solana");
        let (_, _, c) = lane_split(vec![(&coin, 0.1)], 10, &health, &high, &none, &no_pe);
        assert_eq!(c.len(), 1, "no score floor, no sector filter, no redundancy skip on the crypto lane");
        coin.trail_monthly = up.clone();
        assert_eq!(lane_split(vec![(&coin, 0.1)], 10, &all_sectors, &armed, &none, &no_pe).2.len(), 1);
    }

    /// (#37 funds) the PEG CEILING on the ETF lane. Four claims, each one a way this could go wrong:
    /// a dear fund is cut, a cheap one is kept, a fund with NO look-through P/E is kept (missing data
    /// is not a verdict — the whole crypto half of this feature was dropped over exactly that rule
    /// cutting the wrong cohort), and the table REFILLS from below instead of going short.
    ///
    /// Refill is not a mechanism here, it is the position of this trim: it runs before `print_picks`
    /// cuts to `n`, so a dropped row is simply replaced by the next one. That is why it does NOT use
    /// `decorrelate_keep` — see the (#41) receipt above for what a trim that stops at `n` costs.
    #[test]
    fn etf_peg_ceiling_trims_and_refills() {
        let d = BuyHeuristic::default();
        let none: HashSet<&str> = HashSet::new();
        let all_sectors: Vec<String> = Vec::new();
        // 5Y cumulative +61.051% = exactly 10.0 %/yr, so bar 50 (PEG 2.0) splits P/E 25 (peg_yield
        // 40, cut) from P/E 4 (peg_yield 250, kept) with no float slop near the boundary.
        let fund = |t: &str| {
            let mut q = Quote::stub(t, "€100.00", "", &format!("{t} UCITS ETF"));
            q.instrument_type = "ETF".into();
            q.perf = vec![None; core::HORIZONS.len()];
            q.perf[core::HORIZONS.iter().position(|(l, _)| *l == "5Y").unwrap()] = Some((String::new(), 61.051));
            q
        };
        let (dear, cheap, unknown) = (fund("DEAR.L"), fund("CHEAP.L"), fund("UNKNOWN.L"));
        let rows = || vec![(&dear, 9.0), (&unknown, 8.0), (&cheap, 7.0)];
        let names = |v: &[(&Quote, f64)]| v.iter().map(|(q, _)| q.ticker.clone()).collect::<Vec<_>>();
        let mut pe: FundPeMap = HashMap::new();
        pe.insert("DEAR.L".into(), 25.0.into());
        pe.insert("CHEAP.L".into(), 4.0.into());
        // UNKNOWN.L deliberately absent: fetched no P/E (or ranked below the fetched bench).
        let on = BuyHeuristic { growth_max_peg_etf: 2.0, ..d.clone() };

        assert_eq!(names(&lane_split(rows(), 10, &all_sectors, &d, &none, &pe).1), ["DEAR.L", "UNKNOWN.L", "CHEAP.L"],
            "ceiling 0 = off, same convention as every other gate");
        assert_eq!(names(&lane_split(rows(), 10, &all_sectors, &on, &none, &pe).1), ["UNKNOWN.L", "CHEAP.L"],
            "dear fund cut; no-P/E fund kept (missing data is not a verdict)");
        // n=2 with one row cut: the table must still show 2, filled from below — not 1.
        assert_eq!(lane_split(rows(), 2, &all_sectors, &on, &none, &pe).1.len(), 2,
            "the trim runs BEFORE the cut to n, so a dropped row refills from below");
        let pin_dear: HashSet<&str> = ["DEAR.L"].into_iter().collect();
        assert_eq!(names(&lane_split(rows(), 10, &all_sectors, &on, &pin_dear, &pe).1), ["DEAR.L", "UNKNOWN.L", "CHEAP.L"],
            "a pin means always show me this, here as everywhere else");

        // (#37 funds) TRAIN==SERVE for the printed cell: the ETF `peg` column must show the SAME number
        // the trim above cut on. This is the fund half of the APH/ODFL pin at the top of this file — the
        // cell and the gate came apart once already, and only a test comparing them catches it.
        assert_eq!(col_cell("peg", &dear, 0.0, None, "", &on, &pe), "2.50", "P/E 25 over 10 %/yr");
        assert_eq!(col_cell("peg", &cheap, 0.0, None, "", &on, &pe), "0.40", "P/E 4 over 10 %/yr");
        assert_eq!(col_cell("peg", &unknown, 0.0, None, "", &on, &pe), "n/a",
            "no P/E fetched -> n/a, never a number and never the class-N/A dash");
        // and the ceiling's own arithmetic agrees with the cell it prints: 2.50 > 2.0 is why DEAR.L went.
        assert!(100.0 / fund_peg_yield(&dear, &on, &pe).unwrap() > on.growth_max_peg_etf);
    }

    /// (#45) The CRYPTO valuation ceiling, and the three things about it that are easy to get backwards.
    ///
    /// MVRV is compared DIRECTLY, unlike `growth_max_peg` which is applied to a reciprocal — on this
    /// scale expensive is already high, so a mistaken reciprocal here would silently invert the gate
    /// into "only buy the most expensive coins". The 2.0/1.19 pair below is the live BTC reading, so a
    /// regression that flips the comparison fails on the exact number the receipts quote.
    #[test]
    fn crypto_mvrv_ceiling_gates_only_coins_with_data() {
        let coin = |t: &str, mvrv: Option<f64>| {
            let mut q = Quote::stub(t, "€100.00", "", t);
            q.instrument_type = "CRYPTOCURRENCY".into();
            q.perf = vec![None; core::HORIZONS.len()];
            for (label, cum) in [("1Y", 40.0), ("5Y", 400.0), ("8Y", 900.0)] {
                q.perf[core::HORIZONS.iter().position(|(l, _)| *l == label).unwrap()] = Some((String::new(), cum));
            }
            q.range_pct = 90.0;
            q.avg_turnover_eur = Some(1e9);
            q.mvrv = mvrv;
            q
        };
        let on = BuyHeuristic { crypto_max_mvrv: 2.0, ..BuyHeuristic::default() };
        let off = BuyHeuristic::default();
        let (dear, cheap, blank) = (coin("DEAR-EUR", Some(3.29)), coin("BTC-EUR", Some(1.19)), coin("BNB-EUR", None));

        // PRECONDITION — all three must score with the ceiling off, or every verdict below is vacuous.
        for q in [&dear, &cheap, &blank] {
            assert!(growth_score(q, &off).is_some(), "{} must score with the ceiling off", q.ticker);
        }
        assert!(growth_score(&dear, &on).is_none(), "MVRV 3.29 is over the 2.0 ceiling");
        assert!(growth_score(&cheap, &on).is_some(), "MVRV 1.19 is under it — this is BTC's live reading");
        assert!(growth_score(&blank, &on).is_some(), "no MVRV passes free: ~17 of the top 100 carry one");

        // the footer must name the gate and quote the SAME number, in the same units as the knob
        let why = gate_failures(&dear, &on).expect("a gated coin yields failures").into_iter()
            .find(|(g, ..)| *g == "mvrv").expect("the mvrv gate must name itself");
        assert!(why.1.contains("MVRV 3.29") && why.1.contains("2.00"), "got {:?}", why.1);
        assert!(!why.2, "3.29 is more than 1.5x the ceiling — not a near-miss");
        // a coin just over the line IS a near-miss, on the same relative margin the PEG leg uses
        let edge = coin("EDGE-EUR", Some(2.5));
        let (_, _, near) = gate_failures(&edge, &on).unwrap().into_iter().find(|(g, ..)| *g == "mvrv").unwrap();
        assert!(near, "2.5 is within 1.5x of a 2.0 ceiling");

        // CLASS SCOPE: an equity carrying the same field is untouched — the gate is about coins, not
        // about which structs happen to have the column filled.
        let mut stock = coin("XOM", Some(3.29));
        stock.ticker = "XOM".into();
        stock.instrument_type = "EQUITY".into();
        assert!(growth_score(&stock, &on).is_some(), "the ceiling is crypto-scoped, not field-scoped");

        // the MVRV cell: value for a coin, class-N/A dash off one, n/a for a coin with no datum
        let no_pe = HashMap::new();
        assert_eq!(col_cell("mvrv", &cheap, 0.0, None, "", &on, &no_pe), "1.19");
        assert_eq!(col_cell("mvrv", &blank, 0.0, None, "", &on, &no_pe), "n/a");
        assert_eq!(col_cell("mvrv", &stock, 0.0, None, "", &on, &no_pe), "—");
        // ...and MVRV never leaks into the PEG column. They are different quantities (P/B vs P/E ÷ g)
        // and the whole reason MVRV got its own column is that one header cannot mean both.
        assert_eq!(col_cell("peg", &cheap, 0.0, None, "", &on, &no_pe), "—");
    }
}
