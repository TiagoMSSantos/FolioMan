//! Buy-candidate algorithm: rank the `check` table for "quality on sale".
//! Pure scoring + the table printer, kept together so the whole heuristic lives in one
//! place. **NOT advice** — a transparent ranking of the table, never an auto-buy.

use crate::commands::truncate;
use crate::config::{BuyHeuristic, Widths};
use crate::core::{self, Quote, HORIZONS};
use std::collections::{HashMap, HashSet};

/// The longest >2Y leg as (cumulative %, span years): 20Y, else 10Y, else 5Y. None if the asset
/// has no >2Y history. The cumulative % feeds the corpse GATE; annualized (CAGR) it feeds the SCORE,
/// so a 10Y and a 20Y leg are compared on the same %/yr footing.
fn long_leg(q: &Quote) -> Option<(f64, f64)> {
    for (label, years) in [("20Y", 20.0), ("10Y", 10.0), ("5Y", 5.0)] {
        if let Some(p) = perf_pct(q, label) {
            return Some((p, years));
        }
    }
    None
}

/// How intact the long-term trend is, 0..1 — used to scale the on-sale discount so a decaying name's
/// deep "discount" can't outrank a healthy compounder's modest pullback. `zero` (a negative %/yr
/// CAGR) is where health hits 0; health reaches 1 at a flat/rising long trend.
fn trend_health(long_cagr: f64, zero: f64) -> f64 {
    ((long_cagr - zero) / -zero).clamp(0.0, 1.0)
}

/// (D) Trailing ~1Y dividend yield (%) for the dividend reward; 0 if it doesn't pay / no price /
/// short history. Same per-horizon yield `screen` lists.
fn dividend_yield_1y(q: &Quote) -> f64 {
    core::dividend_yields(&q.div_eur, q.price_eur).first().and_then(|o| *o).unwrap_or(0.0)
}

/// (E) Valuation tilt from trailing P/E: cheap (PE < ref) lifts the score, rich dampens it, clamped
/// to [VALUE_TILT_MIN, VALUE_TILT_MAX]. Unknown PE or non-earning (crypto/ETF/PE<=0) -> 1.0 (neutral
/// — never punished for missing data).
fn value_factor(q: &Quote, ref_pe: f64) -> f64 {
    match q.pe_ratio {
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
fn sustained_decline_factor(q: &Quote, t: &BuyHeuristic) -> f64 {
    match (perf_pct(q, "1Y"), perf_pct(q, "5Y")) {
        (Some(y1), Some(y5)) if y1 <= t.sustained_decline_pct && y5 <= t.sustained_decline_pct => {
            // harsher tier: a 5Y this deep (e.g. LTC -73%) is a 7y+ bleed coasting on a stale old
            // chart — dock it much harder than a "merely" -40% multi-year drift.
            if y5 <= t.deep_decline_pct {
                t.deep_decline_penalty
            } else {
                t.sustained_decline_penalty
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
fn momentum_factor(q: &Quote, bounce: f64, knife: f64) -> f64 {
    if perf_pct(q, "1M").unwrap_or(0.0) >= 0.0 {
        return 1.0; // not pulled back -> nothing to time
    }
    if perf_pct(q, "1W").unwrap_or(0.0) > 0.0 {
        bounce // up on the week off a monthly dip -> turn confirmed
    } else if perf_pct(q, "1D").unwrap_or(0.0) > 0.0 {
        1.0 + (bounce - 1.0) * 0.5 // only today green -> half the bounce premium
    } else {
        knife // still falling -> dock it (don't catch the knife)
    }
}

/// Substrings (lowercased) that mark a leveraged/inverse product — daily-reset decay vehicles
/// that are never a long-term hold, so they can't be "quality on sale". `direxion` catches the
/// Direxion Daily 3× family when Yahoo hands a SHORT name ("Direxion Daily Technology" with the
/// "Bull 3X" dropped) that the `3x` marker would miss (e.g. TECL leaked into the stocks table).
/// ponytail: cheap name match; tighten the list if a legit name ever trips it.
const LEVERAGED_MARKERS: &[&str] =
    &["2x", "3x", " short", "inverse", "leverag", "bear ", "ultra", "direxion"];

fn is_leveraged(name: &str) -> bool {
    let n = name.to_lowercase();
    LEVERAGED_MARKERS.iter().any(|m| n.contains(m))
}

/// Substrings (lowercased) that mark a pooled fund (ETF / UCITS index fund) vs a single-company
/// stock — plain index-fund longNames all carry one ("...S&P 500 UCITS ETF", "...ETF Trust"),
/// company names ("Apple Inc.") don't. Used only to SPLIT the equity table, never to gate.
/// ponytail: name match, no asset-type field exists; tighten the list if a stock ever trips it.
const ETF_MARKERS: &[&str] = &["etf", "ucits", " index fund", " fund "];

fn is_etf(name: &str) -> bool {
    let n = name.to_lowercase();
    ETF_MARKERS.iter().any(|m| n.contains(m))
}

/// Is this quote a pooled fund? Prefer Yahoo's own `instrumentType` ("ETF"), which is present even
/// when the name string isn't a giveaway (ETF shortNames like "ISHARES III PLC ISHRS CORE MSCI"
/// carry no marker). Falls back to the name-substring guess for rows with no meta (backtest stubs).
fn quote_is_etf(q: &Quote) -> bool {
    q.instrument_type.eq_ignore_ascii_case("ETF") || is_etf(&q.name)
}

/// Underlying of a currency-quoted ticker: strips a trailing `-EUR`/`-USD` (crypto twins like
/// `BTC-EUR`/`BTC-USD`); anything else is its own underlying.
fn underlying(ticker: &str) -> &str {
    ticker.strip_suffix("-EUR").or_else(|| ticker.strip_suffix("-USD")).unwrap_or(ticker)
}

/// Currency-quoted (crypto/FX) ticker — carries a `-USD`/`-EUR` suffix, unlike an equity/ETF
/// symbol. Such assets are far more volatile, so a −40% year is normal noise, not a death
/// signal: they get the looser `min_1y_pct_crypto` floor instead of the equity `min_1y_pct`.
fn is_currency_quoted(ticker: &str) -> bool {
    underlying(ticker) != ticker
}

/// Dollar-pegged stablecoin underlyings — pegged to $1, so zero growth potential (never a "buy and
/// hold for decades" candidate). On the EUR leg their price drifts with EUR/USD, faking a drawdown
/// that slips past the `drawdown < 3%` peg gate — so exclude them by symbol instead.
const STABLECOINS: &[&str] =
    &["USDT", "USDC", "DAI", "TUSD", "FDUSD", "PYUSD", "USDE", "BUSD", "USDP", "GUSD", "USDD", "FRAX"];

fn is_stablecoin(ticker: &str) -> bool {
    STABLECOINS.contains(&underlying(ticker))
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
    for (q, s) in picks {
        let base = underlying(&q.ticker);
        let take = match best.get(base) {
            None => true,
            // replace only if the newcomer is the preferred currency and the kept one isn't
            Some((kept, _)) => q.ticker.ends_with(pref) && !kept.ticker.ends_with(pref),
        };
        if take {
            best.insert(base, (q, s));
        }
    }
    best.into_values().collect()
}

/// % change at a given horizon label (e.g. "1Y") from a Quote's perf, by label not index
/// (robust to HORIZONS reordering). None if that horizon has no data.
pub fn perf_pct(q: &Quote, label: &str) -> Option<f64> {
    let i = HORIZONS.iter().position(|(l, _)| *l == label)?;
    q.perf.get(i).and_then(|o| o.as_ref()).map(|(_, p)| *p)
}

/// Confidence multiplier — halve a name without a long PROVEN record. Equities should carry a 10Y
/// leg; crypto can't (Yahoo's EUR crypto pairs are too young to ever show 10Y), so for them a 5Y leg
/// is "proven enough". Without this, BTC is halved for a history gap that's purely an artifact of the
/// EUR quote, and vanishes from the growth lane despite a 15-year track record.
fn trust_factor(q: &Quote, crypto: bool) -> f64 {
    let needed = if crypto { "5Y" } else { "10Y" };
    if perf_pct(q, needed).is_none() {
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

/// (#3) 12-1 momentum (%): the return from ~12 months ago to ~1 month ago, SKIPPING the last month
/// (the canonical academic momentum factor — the most recent month is short-term reversal noise, so
/// it's excluded). Built from the already-fetched 1Y and 1M horizon returns, ZERO extra fetch: both
/// share today's price, so price_1mo/price_12mo − 1 = (1 + r_1Y)/(1 + r_1M) − 1. None if either
/// horizon is missing, or if the price a month ago was ~0 (ratio undefined).
fn mom_12_1_pct(q: &Quote) -> Option<f64> {
    let y1 = perf_pct(q, "1Y")?;
    let m1 = perf_pct(q, "1M")?;
    let denom = 1.0 + m1 / 100.0;
    if denom <= 0.0 {
        return None;
    }
    Some(((1.0 + y1 / 100.0) / denom - 1.0) * 100.0)
}

/// (#3) Reward for POSITIVE 12-1 momentum, capped. Negative momentum → 0 (no reward, no extra
/// penalty — the gates already cut deep downtrends). Missing horizons → 0. Shared by both lanes: in
/// the on-sale lane it combats buying laggards (a pullback still in a rising trailing trend beats one
/// in a dying one); in the growth lane it's the trend-persistence core.
fn mom_12_1_reward(q: &Quote, t: &BuyHeuristic) -> f64 {
    t.mom_12_1_weight * mom_12_1_pct(q).unwrap_or(0.0).clamp(0.0, t.mom_12_1_cap)
}

/// (F) Profitability/QUALITY reward: trailing ROE, the canonical quality factor (high-ROE firms
/// out-compound long-run). None (crypto/ETF/no FMP key) → 0 = neutral; negative ROE clamps to 0 (no
/// bonus, the gates handle bleeders). Shared by both lanes. BACKTEST-BLIND: ROE is point-in-time so
/// the price-only walk-forward can't score it — deliberately weighted small (`quality_weight`).
fn quality_reward(q: &Quote, t: &BuyHeuristic) -> f64 {
    t.quality_weight * q.roe.unwrap_or(0.0).clamp(0.0, t.quality_cap)
}

/// (A/B/C) Quality tilts shared by BOTH lanes, all derived from already-fetched closes (zero extra
/// fetch). Returns `(consistency_mult, risk_reward)`:
/// - **consistency_mult** (A) — `consistency_floor`..1 scaled by the log-price trend R²: a smooth
///   compounder (R²→1) keeps full score, a lumpy/lucky path (R²→0) is tapered toward the floor. This
///   is the rigorous fix for CAGR endpoint-luck — you hold the path, not the endpoints.
/// - **risk_reward** (B+C) — additive bonus for return PER unit of risk: Sharpe-ish (CAGR/volatility,
///   path noise) + Calmar (CAGR/max-drawdown, tail pain). Both reward the same thing from two angles —
///   a name that compounds hard while staying calm and shallow-drawdown. Missing/zero risk inputs → 0
///   (never punished for absent data).
fn quality_factors(q: &Quote, long_cagr: f64, t: &BuyHeuristic) -> (f64, f64) {
    let consistency = t.consistency_floor + (1.0 - t.consistency_floor) * q.trend_r2.clamp(0.0, 1.0);
    let sharpe = match q.volatility_pct {
        Some(v) if v > 0.0 => (long_cagr / v).clamp(0.0, t.sharpe_cap),
        _ => 0.0,
    };
    let calmar = if q.max_drawdown_pct > 0.0 {
        (long_cagr / q.max_drawdown_pct).clamp(0.0, t.calmar_cap)
    } else {
        0.0
    };
    (consistency, t.sharpe_weight * sharpe + t.calmar_weight * calmar)
}

/// Score a quote as a "quality on sale" buy candidate for a multi-DECADE hold, or `None` if it
/// fails a gate. The formula:
///
/// ```text
///   base  = discount_weight×discount × trend_health × momentum + long_reward×discount_frac + cheap_reward + dividend_reward + risk_reward + mom_12_1_reward + quality_reward
///   score = base × value × geomean(decline, trust, consistency)   // (#4) geomean caps stacked penalties
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
///   `long_trend_weight`, `long_trend_cap`), scaled by **discount_frac** = discount/`discount_cap`
///   so a proven compounder only earns it when actually pulled back — at its high the reward → 0.
/// - **cheap_reward** — (C) reward for sitting below the ~200wk SMA (`cheap_weight`, `cheap_cap`).
/// - **dividend_reward** — (D) reward for trailing yield (`dividend_weight`, `dividend_cap`). BACKTEST-BLIND:
///   the price-only backtest can't reconstruct as-of dividends, so this term is unvalidated — keep its weight small.
/// - **value** — (E) P/E tilt: cheap lifts, rich dampens, unknown neutral (`ref_pe`). BACKTEST-BLIND:
///   no as-of P/E in the backtest, so this term is unvalidated there too — keep the tilt gentle.
/// - **quality_reward** — (F) trailing-ROE profitability tilt (`quality_weight`/`quality_cap`); the
///   canonical quality factor (high-ROE firms out-compound). BACKTEST-BLIND (point-in-time ROE), so small.
/// - **decline** — (B) value-trap dock when 1Y & 5Y both deeply negative.
/// - **risk_reward** — (B/C) Sharpe-ish (CAGR/vol) + Calmar (CAGR/max-drawdown) bonus; return per unit of risk.
/// - **mom_12_1_reward** — (#3) reward for positive 12-1 momentum (12mo→1mo trailing trend); favours a pullback still in an uptrend over one in a dying trend.
/// - **consistency** — (A) multiplier from the log-price trend R²; a lumpy/lucky path is tapered toward `consistency_floor`.
/// - **trust** — halves anything without a long record (10Y for equities, 5Y for young-EUR-pair crypto).
///
/// Every knob lives in `BuyHeuristic` (settings.yaml `buy_heuristic:`). Higher = more interesting.
/// GATES below exclude a candidate before scoring. **NOT advice** — a ranking, never a forecast.
pub fn buy_score(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
    let crypto = is_currency_quoted(&q.ticker); // crypto/FX (-EUR/-USD): looser, peak-anchor-aware rules

    // ---- GATES: drop anything that isn't a quality name on a real pullback ----
    if is_leveraged(&q.name) {
        return None; // leveraged/inverse product -> decays, never a long-term hold
    }
    if q.avg_turnover_eur.map_or(false, |v| v < t.min_avg_turnover_eur) {
        return None; // too thin/illiquid (unknown turnover passes — don't punish missing data)
    }
    if crypto && is_stablecoin(&q.ticker) {
        return None; // dollar-pegged stablecoin -> no growth; its EUR-leg FX drift fakes a drawdown
    }
    if crypto && q.drawdown_pct < 3.0 {
        return None; // crypto at its high -> nothing on sale
    }
    // longest >2Y leg (crypto is younger, so fall back to its 1Y leg): cumulative for the gate,
    // annualized (CAGR) for the score.
    let (long_cum, long_years) =
        long_leg(q).or_else(|| if crypto { perf_pct(q, "1Y").map(|p| (p, 1.0)) } else { None })?;
    if crypto && long_cum <= t.min_long_pct_crypto {
        return None; // crypto corpse: a >2Y leg this deep (e.g. -95%) is a dead coin, not a dip
    }
    let y1 = perf_pct(q, "1Y")?;
    let floor = if crypto { t.min_1y_pct_crypto } else { t.min_1y_pct };
    if y1 <= floor {
        return None; // deep 1-year downtrend -> not a pullback
    }
    let knife = if crypto { t.max_1m_drop_pct_crypto } else { t.max_1m_drop_pct };
    if perf_pct(q, "1M").unwrap_or(0.0) <= knife {
        return None; // crashing this month -> falling knife
    }
    if !crypto {
        // equities must be structurally up: EVERY multi-year leg must hold. (Crypto -EUR 5Y is
        // peak-anchored and routinely negative even when healthy, so this gate is meaningless there.)
        for label in ["5Y", "10Y", "20Y"] {
            if perf_pct(q, label).map_or(false, |p| p <= t.min_long_pct) {
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
    let cheapness = 100.0 - q.range_pct;
    let discount =
        normalized_dip(cheapness, q.volatility_pct, t.normal_volatility_pct).min(t.discount_cap);
    let health = trend_health(long_cagr, t.health_zero_cagr);
    let momentum = momentum_factor(q, t.momentum_bounce, t.momentum_knife);
    // (2a) scale the long-trend reward by how on-sale the name is (discount as a fraction of the cap):
    // a proven compounder is only a BUY when it's actually pulled back. At its all-time high the
    // discount is ~0, so the reward fades to ~0 and an at-the-high rocket stops ranking as "on sale".
    let discount_frac = (discount / t.discount_cap).clamp(0.0, 1.0); // 0 = at its high, 1 = deeply discounted
    let long_reward = t.long_trend_weight * long_cagr.min(t.long_trend_cap) * discount_frac; // (A)
    let cheap_reward = t.cheap_weight * q.below_ma_pct.min(t.cheap_cap); // (C)
    let dividend_reward = t.dividend_weight * dividend_yield_1y(q).min(t.dividend_cap); // (D)

    let (consistency, risk_reward) = quality_factors(q, long_cagr, t); // (A/B/C) zero-fetch quality tilts
    let base = t.discount_weight * discount * health * momentum // (#4) demoted: dip-depth ranks backwards on peer-relative backtest
        + long_reward
        + cheap_reward
        + dividend_reward
        + risk_reward
        + mom_12_1_reward(q, t) // (#3) trailing-trend confirmation: prefer pullbacks still in an uptrend
        + quality_reward(q, t); // (F) ROE profitability tilt (BACKTEST-BLIND, small)
    let value = value_factor(q, t.ref_pe); // (E) cheap lifts, rich dampens, unknown neutral
    let decline = sustained_decline_factor(q, t); // (B) multi-year-bleed dock
    let trust = trust_factor(q, crypto); // (A) equities need a 10Y leg; crypto's young EUR pairs need only 5Y
    // (#4) geomean the pure penalties so several mild damps can't compound to ~0; value (a tilt that
    // can exceed 1.0) stays a direct multiplier.
    Some(base * value * combine_damps(&[decline, trust, consistency]))
}

/// Score a quote as a MOMENTUM/GROWTH candidate — the MIRROR of `buy_score`. The on-sale lane fades
/// a name's score to ~0 as it nears its high (a proven compounder at a new high has no "discount"),
/// so it never surfaces quality that's expensive *because* it keeps winning. This lane is exactly
/// that set: a name AT/NEAR its own range high, with a strong proven long-term CAGR, still climbing.
///
/// ```text
///   base  = growth_trend_weight × min(long_cagr, long_trend_cap)
///         + growth_accel_weight × clamp(1Y − long_cagr, 0, growth_accel_cap)   // recent outpaces long => accelerating
///         + mom_12_1_reward                                                    // (#3) 12-1 trailing-trend persistence
///         + quality_reward                                                     // (F) ROE profitability tilt (BACKTEST-BLIND, small)
///   score = base × proximity × value(E) × geomean(trust, overext, consistency)   // (#4) geomean of the penalties
/// ```
///
/// Gated HARD so it can't degrade into top-chasing: must sit in the top `growth_min_range_pct` of its
/// own ~10y range, compound at least `growth_min_cagr` %/yr, have a POSITIVE 1Y (actually climbing),
/// and not be crashing this month. The P/E value tilt (E) still damps a nosebleed valuation, so a
/// blow-off top is penalised, not rewarded. `None` if it fails a gate. **NOT advice** — a ranking.
pub fn growth_score(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
    let crypto = is_currency_quoted(&q.ticker);

    // ---- GATES (reuse the cheap exclusions; the rest are the on-sale lane's mirror) ----
    if is_leveraged(&q.name) {
        return None; // leveraged/inverse decays -> never a long-term hold
    }
    if crypto && is_stablecoin(&q.ticker) {
        return None; // pegged $1 -> no growth
    }
    if q.avg_turnover_eur.map_or(false, |v| v < t.min_avg_turnover_eur) {
        return None; // too thin (unknown turnover passes)
    }
    if q.range_pct < t.growth_min_range_pct {
        return None; // NOT near its high -> that's the on-sale lane's job, not this one
    }
    let (long_cum, long_years) =
        long_leg(q).or_else(|| if crypto { perf_pct(q, "1Y").map(|p| (p, 1.0)) } else { None })?;
    let long_cagr = core::cagr(long_cum, long_years);
    if long_cagr < t.growth_min_cagr {
        return None; // weak long-run trend -> an expensive laggard, not a proven compounder
    }
    let y1 = perf_pct(q, "1Y")?;
    if y1 <= 0.0 {
        return None; // not actually climbing this year -> no momentum to ride
    }
    let knife = if crypto { t.max_1m_drop_pct_crypto } else { t.max_1m_drop_pct };
    if perf_pct(q, "1M").unwrap_or(0.0) <= knife {
        return None; // rolling over hard this month -> momentum broke
    }
    if !crypto && perf_pct(q, "5Y").map_or(false, |y5| y5 <= 0.0) {
        // (3) consistency: a near-high name negative over 5Y mooned-then-bled — its great 10Y CAGR is a
        // stale endpoint, not a durable trend. Require the mid leg to hold too. (Crypto 5Y is
        // peak-anchored noise; the range gate already excludes bled coins there, so skip it.)
        return None;
    }

    // ---- SCORE ----
    let trend = long_cagr.min(t.long_trend_cap); // proven compounding, capped like the on-sale lane
    let accel = (y1 - long_cagr).clamp(0.0, t.growth_accel_cap); // last year outpacing the long run = building
    let proximity = q.range_pct / 100.0; // 0.7..1.0 — closer to the high = stronger confirmation
    let (consistency, risk_reward) = quality_factors(q, long_cagr, t); // (A/B/C) zero-fetch quality tilts
    let base = t.growth_trend_weight * trend
        + t.growth_accel_weight * accel
        + risk_reward
        + mom_12_1_reward(q, t) // (#3) 12-1 trend persistence — the growth lane's core signal
        + quality_reward(q, t); // (F) ROE profitability tilt (BACKTEST-BLIND, small)
    let value = value_factor(q, t.ref_pe); // (E) a nosebleed P/E still damps the score (anti top-chase)
    let trust = trust_factor(q, crypto); // (A) equities need a 10Y leg; crypto's young EUR pairs need only 5Y
    // (1) overextension brake: how far the price has run ABOVE its own 200wk SMA. Far above trend =
    // stretched/blow-off, so taper the score toward `growth_overext_floor` at the cap. This is the
    // generic brake the P/E tilt can't provide for crypto/ETFs (no earnings) — works on price alone.
    let overext = q.above_ma_pct.min(t.growth_overext_cap);
    let overext_damp = if t.growth_overext_cap > 0.0 {
        1.0 - (overext / t.growth_overext_cap) * (1.0 - t.growth_overext_floor) // 1.0 at trend .. floor at the cap
    } else {
        1.0 // cap 0 = brake disabled
    };
    // (#4) geomean the pure penalties (trust/overext/consistency); proximity + value stay direct
    Some(base * proximity * value * combine_damps(&[trust, overext_damp, consistency]))
}

/// (4) Whole-market crypto sentiment damp from Bitcoin NUPL (net unrealized profit/loss — already
/// fetched for the screen footer). NUPL above `nupl_euphoria` is market greed/top territory, so scale
/// crypto scores toward `nupl_damp_floor` (reached at NUPL 1.0, peak euphoria). 1.0 (no damp) when
/// NUPL is unknown or below the euphoria line. Market-wide, so it scales the whole crypto lane
/// uniformly — thinning the crypto buy/growth tables in a frothy top, fattening them after a flush.
fn nupl_damp(nupl: Option<f64>, t: &BuyHeuristic) -> f64 {
    match nupl {
        Some(v) if v > t.nupl_euphoria && t.nupl_euphoria < 1.0 => {
            let over = ((v - t.nupl_euphoria) / (1.0 - t.nupl_euphoria)).clamp(0.0, 1.0);
            1.0 - over * (1.0 - t.nupl_damp_floor)
        }
        _ => 1.0,
    }
}

/// Horizons whose Δ% is shown in the picks table (chronological).
const DIFF_HORIZONS: &[&str] = &["1D", "1W", "1M", "1Y", "5Y", "10Y", "20Y"];

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
///   over-filter. ponytail: ceiling — a delisted alt could slip through; tighten if it ever bites.
/// - **ETF**: only funds LISTED on a European exchange. A US-domiciled ETF (SPY/QQQ/VOO) trades on a
///   US venue and has no PRIIPs KID, so EU brokers can't sell it to retail; a UCITS fund lists on
///   Xetra/LSE/Borsa Italiana (market != USA/Canada). Venue is the robust UCITS proxy — the name
///   string isn't (Yahoo gives ETF shortNames with no "UCITS" marker), so don't gate on it.
/// - **stock**: only on a venue EU retail brokers serve (`EU_BUYABLE_MARKETS`); other listings drop.
///
/// `pub` so `screen` can filter its WHOLE universe once (every table — ATH/ATL/fallers/dividends/buys),
/// not just the picks lanes.
pub fn eu_buyable(q: &Quote) -> bool {
    if is_currency_quoted(&q.ticker) {
        return true; // crypto major
    }
    if quote_is_etf(q) {
        // European-listed only: US/Canada listing = US-domiciled (no KID), barred for EU retail.
        return q.market != "USA" && q.market != "Canada" && EU_BUYABLE_MARKETS.contains(&q.market.as_str());
    }
    EU_BUYABLE_MARKETS.contains(&q.market.as_str())
}

/// Score every quote with `score`, dedup currency twins, drop rows at/below `min_score`, sort
/// best-first. Shared by both lanes (on-sale `buy_score`, growth `growth_score`) and all per-class
/// tables — the lane is just which scorer + threshold the caller passes. Non-EU-buyable names
/// (US-domiciled ETFs, Asian-only listings) are filtered out up front.
fn ranked<'a>(
    qs: &'a [Quote],
    t: &BuyHeuristic,
    score: impl Fn(&Quote, &BuyHeuristic) -> Option<f64>,
    min_score: f64,
) -> Vec<(&'a Quote, f64)> {
    let scored: Vec<(&Quote, f64)> =
        qs.iter().filter(|q| eu_buyable(q)).filter_map(|q| score(q, t).map(|s| (q, s))).collect();
    let mut picks = dedup_currency_twins(scored, t.prefer_eur); // one row per asset (BTC, not BTC-EUR+BTC-USD)
    // drop padding rows below the lane's floor, so the tables stop filling to top_picks with near-zero
    // names. (min_score 0 -> show everything > 0.)
    picks.retain(|(_, s)| *s > min_score.max(0.0));
    picks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first
    // (B) collapse dual-class share twins (GOOG/GOOGL, BRK.A/BRK.B): same company = identical Yahoo
    // name; after the best-first sort, keep the first (higher-scoring/more-liquid) leg, drop the rest.
    let mut seen: HashSet<&str> = HashSet::new();
    picks.retain(|(q, _)| seen.insert(q.name.as_str()));
    picks
}

/// Upside to reclaim the OFF-HI high, from the OFF-HI drawdown: a name 46% off its high needs +85%
/// to get back there. NOT a forecast — just the room back to that high (anchor = `high_days`).
/// Clamps the asymptote near a total wipeout (-99%+ off is a corpse anyway).
fn upside_to_high(dd: f64) -> f64 {
    if dd >= 99.0 {
        return 9900.0;
    }
    dd * 100.0 / (100.0 - dd)
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

/// Print one Top-`n` buy-candidate table (a single asset-class subset of the ranked picks).
fn print_picks(title: &str, picks: &[(&Quote, f64)], n: usize, w: &Widths) {
    let (nw, tw, mw, pw, sw) = (w.name, w.ticker, w.market, w.price, w.score);
    println!("\n{title}");
    if picks.is_empty() {
        println!("  (none pass the gates)");
        return;
    }
    let diff_hdr = DIFF_HORIZONS.iter().map(|l| format!("{:>8}", l)).collect::<Vec<_>>().join(" ");
    let cell = |o: Option<f64>| o.map_or("n/a".to_string(), |v| format!("{:+.1}%", v));
    println!(
        "  {:<4} {:<nw$} {:<tw$} {:<mw$} {:>pw$} {:>7} {:>7} {:>7} {diff_hdr} {:>7} {:>8} {:>10} {:>sw$}",
        "RANK", truncate("NAME", nw), truncate("TICKER", tw), truncate("MARKET", mw), "PRICE(EUR)",
        "1H", "6H", "12H", "OFF-HI", "UPSIDE", "TURNOVER", truncate("SCORE", sw)
    );
    for (i, (q, score)) in picks.iter().take(n).enumerate() {
        let diffs = DIFF_HORIZONS
            .iter()
            .map(|l| format!("{:>8}", perf_pct(q, l).map_or("n/a".to_string(), |v| format!("{:+.1}%", v))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  {:<4} {:<nw$} {:<tw$} {:<mw$} {:>pw$} {:>7} {:>7} {:>7} {diffs} {:>7} {:>8} {:>10} {:>sw$.1}",
            i + 1,
            truncate(&q.name, nw),
            truncate(&q.ticker, tw),
            truncate(&q.market, mw),
            q.price,
            cell(q.intraday[0]),
            cell(q.intraday[1]),
            cell(q.intraday[2]),
            format!("-{:.1}%", q.drawdown_pct), // % below the OFF-HI high (high_days anchor, default all-time)
            format!("+{:.1}%", upside_to_high(q.drawdown_pct)), // room back to that high (NOT a forecast)
            turnover_cell(q.avg_turnover_eur),
            score,
        );
    }
}

/// Print ONE lane's ranked picks SPLIT per asset class (stocks / [tech stocks] / ETFs / crypto) so a
/// +9400% crypto can't crowd out equities and a basket fund isn't ranked head-to-head with a single
/// company — the best in EACH class surfaces. Class: currency-quoted ticker (`-USD`/`-EUR`) → crypto,
/// else fund name (ETF/UCITS) → ETF, else stock. Currency twins already deduped in `ranked`.
/// `kind` names the lane in each title ("buy candidates" / "growth candidates").
fn print_lane(
    picks: Vec<(&Quote, f64)>,
    n: usize,
    w: &Widths,
    tech: &HashSet<String>,
    kind: &str,
    desc: &str,
) {
    let (crypto, equity): (Vec<_>, Vec<_>) =
        picks.into_iter().partition(|(q, _)| is_currency_quoted(&q.ticker));
    let (etf, stock): (Vec<_>, Vec<_>) = equity.into_iter().partition(|(q, _)| quote_is_etf(q));
    print_picks(&format!("Top {n} stocks {kind} — {desc}"), &stock, n, w);
    // tech-only subset (S&P 500 GICS Information Technology + Communication Services); skipped when
    // no sector data (e.g. `screen TICKER...` or `check`, which pass an empty set). ETFs aren't in the
    // S&P constituent set, so this stays single-stock even drawing from the pre-split equity list.
    if !tech.is_empty() {
        let tech_picks: Vec<_> = stock.iter().filter(|(q, _)| tech.contains(&q.ticker)).cloned().collect();
        print_picks(&format!("Top {n} tech stocks {kind} — {desc}"), &tech_picks, n, w);
    }
    print_picks(&format!("Top {n} ETFs {kind} — {desc}"), &etf, n, w);
    print_picks(&format!("Top {n} crypto {kind} — {desc}"), &crypto, n, w);
}

/// Print the Top-N picks in TWO lanes: the on-sale "quality on sale" heuristic, then its mirror, the
/// growth/momentum lane (proven compounders at/near their high still climbing — names the on-sale
/// score fades to ~0 and would otherwise never surface). Each lane is split per asset class.
/// `nupl` (Bitcoin net-unrealized-P/L, the screen footer's market-greed gauge; `None` on `check` or
/// fetch fail) damps the crypto rows of BOTH lanes when the market is euphoric.
pub fn render(qs: &[Quote], n: usize, t: &BuyHeuristic, w: &Widths, tech: &HashSet<String>, nupl: Option<f64>) {
    // (4) market-greed damp, applied to crypto rows only (it's a whole-crypto-market gauge)
    let cdamp = nupl_damp(nupl, t);
    let crypto_damp = |q: &Quote, s: f64| if is_currency_quoted(&q.ticker) { s * cdamp } else { s };
    let onsale_scorer = |q: &Quote, t: &BuyHeuristic| buy_score(q, t).map(|s| crypto_damp(q, s));
    let growth_scorer = |q: &Quote, t: &BuyHeuristic| growth_score(q, t).map(|s| crypto_damp(q, s));

    let onsale = "quality-on-sale heuristic: most below its peak (OFF-HI; anchor = high_days, default \
                  all-time over the fetched ~10y) with a still-intact longer-term trend (5Y+ where the \
                  history exists — Yahoo's EUR crypto pairs are younger, so 1Y stands in). NOT advice, \
                  just a ranking:";
    print_lane(ranked(qs, t, onsale_scorer, t.min_score), n, w, tech, "buy candidates", onsale);

    // mirror lane: at/near the high but still expected to compound — the on-sale score can't surface
    // these (no "discount"), so they get their own ranking. Gated to proven, still-climbing names.
    let growth = "growth/momentum lane (mirror of on-sale): at/near its own ~10y high (OFF-HI ≈ 0) with \
                  a strong proven long-term CAGR and an accelerating recent year, braked by how far it's \
                  run above its 200wk trend — quality pricey *because* it keeps winning. NOT advice:";
    print_lane(ranked(qs, t, growth_scorer, t.growth_min_score), n, w, tech, "growth candidates", growth);
}

/// Buy-heuristic asserts (no network). Run by the `selftest` subcommand and the unit test.
pub fn selftest() {
    // build a Quote with chosen horizon %s set (others n/a), robust to HORIZONS order. First
    // arg = drawdown_pct (% below the OFF-HI high) — the on-sale signal the score is built on.
    let q = |drawdown_pct: f64, labels: &[(&str, f64)]| -> Quote {
        let perf = HORIZONS
            .iter()
            .map(|(l, _)| labels.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
            .collect();
        Quote {
            ticker: "T".into(), price: "€1.00".into(), dip: "-5.0%".into(), drop_pct: drawdown_pct,
            market: "USA".into(), instrument_type: String::new(), head: String::new(), news_block: String::new(), perf,
            name: "n".into(), trend: String::new(), at_ath: false, at_atl: false, mom_pct: None,
            div_eur: Vec::new(), price_eur: None, drawdown_pct, intraday: [None; 3],
            avg_turnover_eur: None, volatility_pct: None, below_ma_pct: 0.0, above_ma_pct: 0.0,
            pe_ratio: None,
            roe: None,
            // for tests, mirror the on-sale magnitude: a deeper drawdown = deeper in its range.
            // (real fetch computes range_pct independently; tying them keeps the score asserts honest.)
            range_pct: 100.0 - drawdown_pct,
            trend_r2: 0.0, // default lumpy -> consistency floor, UNIFORM across test quotes so relational asserts hold
            max_drawdown_pct: 0.0, // default -> no calmar reward (additive 0)
        }
    };
    let t = BuyHeuristic::default(); // momentum neutral 1.0/1.0, CAGR-based long reward, A-E terms on

    // --- pure helpers ---
    assert_eq!(perf_pct(&q(5.0, &[("1Y", 20.0)]), "1Y"), Some(20.0));
    assert_eq!(perf_pct(&q(5.0, &[]), "1Y"), None);
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

    // --- GATES (exclusion behaviour, unchanged) ---
    assert!(buy_score(&q(5.0, &[("1Y", 20.0)]), &t).is_none()); // equity: no >2Y leg -> excluded
    let mut crypto = q(5.0, &[("1Y", 20.0)]); // ...but crypto falls back to its 1Y leg -> admitted
    crypto.ticker = "BTC-EUR".into();
    assert!(buy_score(&crypto, &t).is_some());
    assert!(buy_score(&q(5.0, &[("1Y", 20.0), ("5Y", 40.0), ("1M", -25.0)]), &t).is_none()); // equity knife
    let mut knife_crypto = q(5.0, &[("1Y", 20.0), ("1M", -25.0)]); // crypto looser knife -> admitted
    knife_crypto.ticker = "ETH-EUR".into();
    assert!(buy_score(&knife_crypto, &t).is_some());
    assert!(buy_score(&q(5.0, &[("1Y", 20.0), ("5Y", -50.0)]), &t).is_none()); // equity: neg 5Y leg
    let mut corpse = q(40.0, &[("1Y", -30.0), ("5Y", -95.0)]); // crypto corpse (>2Y leg -95%) excluded
    corpse.ticker = "FIL-EUR".into();
    assert!(buy_score(&corpse, &t).is_none());
    let mut peg = q(0.3, &[("1Y", 3.0), ("5Y", 3.0)]); // crypto at its high: drawdown<3% -> nothing on sale
    peg.ticker = "PEPE-EUR".into();
    assert!(buy_score(&peg, &t).is_none());
    // stablecoin gate (3): excluded even with a fat EUR-leg "drawdown" that clears the 3% peg gate
    assert!(is_stablecoin("USDC-EUR") && is_stablecoin("USDT-USD") && !is_stablecoin("BTC-EUR"));
    let mut stable = q(16.0, &[("1Y", -20.0)]);
    stable.ticker = "USDC-EUR".into();
    assert!(buy_score(&stable, &t).is_none()); // pegged $1 -> no growth, FX drift faked the dip
    assert!(is_leveraged("GraniteShares 2x Short NVD") && !is_leveraged("Apple Inc."));
    // (1) Direxion Daily 3x leaks a SHORT name without "3x" -> issuer marker still catches it (TECL)
    assert!(is_leveraged("Direxion Daily Technology") && !is_leveraged("Technology Select Sector"));
    // ETF classifier (splits the equity table only): funds match, single companies don't
    assert!(is_etf("iShares Core S&P 500 UCITS ETF") && is_etf("SPDR S&P 500 ETF Trust"));
    assert!(!is_etf("Apple Inc.") && !is_etf("NVIDIA Corporation"));
    let mut lev = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    lev.name = "GraniteShares 2x Short NVD".into();
    assert!(buy_score(&lev, &t).is_none()); // leveraged/inverse product excluded
    let liq_t = BuyHeuristic { min_avg_turnover_eur: 1_000_000.0, ..BuyHeuristic::default() };
    let mut thin = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    thin.avg_turnover_eur = Some(1_000.0);
    assert!(buy_score(&thin, &liq_t).is_none()); // below liquidity floor
    thin.avg_turnover_eur = Some(5_000_000.0);
    assert!(buy_score(&thin, &liq_t).is_some());
    thin.avg_turnover_eur = None; // unknown turnover not punished
    assert!(buy_score(&thin, &liq_t).is_some());
    assert!(buy_score(&q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("1M", -30.0)]), &t).is_none()); // equity knife
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", -3.0)]), &t).is_none()); // neg >2Y -> excluded
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", -5.0)]), &t).is_none()); // every leg must hold
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 80.0), ("20Y", 200.0)]), &t).is_some());
    assert!(buy_score(&q(5.0, &[("1Y", -5.0), ("5Y", 40.0)]), &t).is_none()); // declining year
    assert!(buy_score(&q(30.0, &[("1Y", -40.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).is_none()); // equity 1Y floor
    let mut cr = q(30.0, &[("1Y", -40.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cr.ticker = "BTC-USD".into();
    assert!(buy_score(&cr, &t).is_some()); // crypto looser 1Y floor
    assert!(buy_score(&q(5.0, &[("5Y", 40.0)]), &t).is_none()); // no 1Y data
    assert!(buy_score(&Quote::stub("X", "err", "", "X"), &t).is_none()); // err row

    // --- SCORE (relational, robust to knob tuning) ---
    // trust: same inputs, the one missing a 10Y record scores lower (uptrend less proven)
    let with10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    let no10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0)]), &t).unwrap();
    assert!(with10 > no10);
    // discount caps: an 80% drawdown doesn't score below a 5% one, all else equal
    let deep = buy_score(&q(80.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    let shallow = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    assert!(deep >= shallow);
    // (A) discount keys off range position: same drawdown, the one deeper in its own range
    // (lower range_pct) outranks the one near its range high — the fix raw ATH-distance couldn't make
    let mut deep_in_range = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    deep_in_range.range_pct = 20.0; // trades near its 10y low
    let mut near_high = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    near_high.range_pct = 80.0; // trades near its 10y high
    assert!(buy_score(&deep_in_range, &t).unwrap() > buy_score(&near_high, &t).unwrap());
    // a deep pullback on a healthy long trend beats a rocket at new highs (discount 0)
    let pullback = buy_score(&q(40.0, &[("1Y", 30.0), ("5Y", 50.0), ("10Y", 50.0)]), &t).unwrap();
    let rocket = buy_score(&q(0.0, &[("1Y", 400.0), ("5Y", 500.0), ("10Y", 500.0)]), &t).unwrap();
    assert!(pullback > rocket, "on-sale name must beat the rocket: {pullback} vs {rocket}");
    // #1 end-to-end: same 30% drawdown, the calm (low-vol) name outranks the wild one
    let mut calm = q(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    calm.volatility_pct = Some(1.0);
    let mut wild = q(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    wild.volatility_pct = Some(4.0);
    assert!(buy_score(&calm, &t).unwrap() > buy_score(&wild, &t).unwrap());
    // (2a) at its all-time high (discount ~0) a huge-CAGR name must NOT outrank an equal pulled-back
    // one — the long-trend reward fades without an actual discount (kills the at-the-high "rocket")
    let at_high = q(0.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 500.0)]); // range_pct 100 -> discount 0
    let pulled = q(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 500.0)]); // same CAGR, real discount
    assert!(buy_score(&pulled, &t).unwrap() > buy_score(&at_high, &t).unwrap());
    // (A) a stronger long-term CAGR outranks a weaker one, all else equal
    let strong = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 400.0)]), &t).unwrap();
    let weak = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    assert!(strong > weak);
    // (A) trend_health: 0 at the decay (zero) threshold, 1 at a flat/rising trend
    assert_eq!(trend_health(t.health_zero_cagr, t.health_zero_cagr), 0.0);
    assert_eq!(trend_health(0.0, t.health_zero_cagr), 1.0);
    // (B) sustained-decline dock: 1Y & 5Y both deep-red is docked below an equal coin that's recovering
    let mut bleeder = q(40.0, &[("1Y", -50.0), ("5Y", -60.0), ("10Y", 200.0)]);
    bleeder.ticker = "LTC-EUR".into();
    let mut recover = q(40.0, &[("1Y", 20.0), ("5Y", -60.0), ("10Y", 200.0)]);
    recover.ticker = "XYZ-EUR".into();
    assert!(buy_score(&bleeder, &t).unwrap() < buy_score(&recover, &t).unwrap());
    assert!((sustained_decline_factor(&bleeder, &t) - t.sustained_decline_penalty).abs() < 1e-9);
    assert_eq!(sustained_decline_factor(&recover, &t), 1.0); // positive 1Y -> not a value trap
    // (C) harsher tier: a 5Y past deep_decline_pct (e.g. LTC -73%) docks below the -40% tier
    let deep_bleeder = q(40.0, &[("1Y", -58.0), ("5Y", -73.0), ("10Y", 282.0)]); // LTC-shaped
    assert!((sustained_decline_factor(&deep_bleeder, &t) - t.deep_decline_penalty).abs() < 1e-9);
    assert!(t.deep_decline_penalty < t.sustained_decline_penalty); // tier 2 is harsher
    // (C) sitting below the ~200wk SMA lifts the score
    let mut cheap = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cheap.below_ma_pct = 50.0;
    let dear = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert!(buy_score(&cheap, &t).unwrap() > buy_score(&dear, &t).unwrap());
    // (D) a dividend payer outranks an otherwise-equal non-payer
    let mut payer = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    payer.price_eur = Some(100.0);
    payer.div_eur = vec![Some(5.0)]; // ~5% trailing-1Y yield (DIV_HORIZONS[0] = 1Y)
    let nonpayer = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert!(dividend_yield_1y(&payer) > 0.0);
    assert!(buy_score(&payer, &t).unwrap() > buy_score(&nonpayer, &t).unwrap());
    // (E) value tilt: a cheap P/E lifts, a rich one dampens, unknown is neutral (1.0)
    let mut cheap_pe = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cheap_pe.pe_ratio = Some(8.0);
    let mut rich_pe = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    rich_pe.pe_ratio = Some(60.0);
    let neutral_pe = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    assert!(value_factor(&cheap_pe, t.ref_pe) > 1.0 && value_factor(&rich_pe, t.ref_pe) < 1.0);
    assert_eq!(value_factor(&neutral_pe, t.ref_pe), 1.0);
    assert!(buy_score(&cheap_pe, &t).unwrap() > buy_score(&neutral_pe, &t).unwrap());
    assert!(buy_score(&rich_pe, &t).unwrap() < buy_score(&neutral_pe, &t).unwrap());
    // upside to high: 50% off -> +100% to recover; at the high -> 0; near-total wipeout clamps
    assert!((upside_to_high(50.0) - 100.0).abs() < 1e-9);
    assert_eq!(upside_to_high(0.0), 0.0);
    assert_eq!(upside_to_high(99.5), 9900.0);

    // currency-twin dedup (E): keep the preferred leg, pass other tickers through
    let mut btc_e = q(10.0, &[("1Y", 5.0), ("5Y", 40.0), ("10Y", 40.0)]);
    btc_e.ticker = "BTC-EUR".into();
    let mut btc_u = q(10.0, &[("1Y", 5.0), ("5Y", 40.0), ("10Y", 40.0)]);
    btc_u.ticker = "BTC-USD".into();
    let mut aapl = q(5.0, &[("1Y", 5.0), ("5Y", 40.0), ("10Y", 40.0)]);
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

    // (B) ranked dedups dual-class share twins by identical company name (GOOG/GOOGL -> one row)
    let mut goog = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // both name "n"
    goog.ticker = "GOOG".into();
    let mut googl = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    googl.ticker = "GOOGL".into();
    assert_eq!(ranked(&[goog, googl], &t, buy_score, t.min_score).len(), 1);
    // (A) ranked hides rows scoring at/below min_score (near-the-high padding), keeps real candidates
    let shallow = q(2.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // tiny discount -> low score
    assert!(buy_score(&shallow, &t).unwrap() < t.min_score);
    assert!(ranked(std::slice::from_ref(&shallow), &t, buy_score, t.min_score).is_empty());
    let strong_pick = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]); // real discount -> kept
    assert_eq!(ranked(std::slice::from_ref(&strong_pick), &t, buy_score, t.min_score).len(), 1);

    // --- GROWTH LANE (mirror of buy_score): near-high proven compounders the on-sale score drops ---
    // an at-the-high rocket buy_score fades to ~0 (or trims) IS a growth candidate here
    let rocket = q(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // range_pct 100, strong CAGR, climbing
    assert!(growth_score(&rocket, &t).is_some());
    // ...and ranked picks it up where the on-sale lane (min_score) would have trimmed an at-high name
    assert_eq!(ranked(std::slice::from_ref(&rocket), &t, growth_score, t.growth_min_score).len(), 1);
    // a deeply pulled-back name is NOT a growth candidate (that's the on-sale lane's job)
    let dipped = q(40.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]); // range_pct 60 < growth_min_range_pct
    assert!(growth_score(&dipped, &t).is_none());
    // weak long trend -> an expensive laggard, not a proven compounder -> excluded
    assert!(growth_score(&q(0.0, &[("1Y", 3.0), ("5Y", 6.0), ("10Y", 10.0)]), &t).is_none());
    // not climbing this year (negative 1Y) -> no momentum -> excluded
    assert!(growth_score(&q(0.0, &[("1Y", -5.0), ("5Y", 200.0), ("10Y", 500.0)]), &t).is_none());
    // crashing this month -> momentum broke -> excluded
    assert!(growth_score(&q(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0), ("1M", -30.0)]), &t).is_none());
    // leveraged/stablecoin still excluded in this lane too
    let mut lev_g = q(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    lev_g.name = "Direxion Daily Technology".into();
    assert!(growth_score(&lev_g, &t).is_none());
    // acceleration: same long CAGR, the name whose recent year OUTPACES it scores higher (momentum)
    let accel = growth_score(&q(0.0, &[("1Y", 80.0), ("5Y", 100.0), ("10Y", 150.0)]), &t).unwrap();
    let steady = growth_score(&q(0.0, &[("1Y", 15.0), ("5Y", 100.0), ("10Y", 150.0)]), &t).unwrap();
    assert!(accel > steady);
    // (E) a nosebleed P/E damps the growth score (anti top-chase), an unknown PE stays neutral
    let mut rich_g = q(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    rich_g.pe_ratio = Some(80.0);
    assert!(growth_score(&rich_g, &t).unwrap() < growth_score(&rocket, &t).unwrap());
    // (1) overextension brake: a name run far ABOVE its 200wk SMA scores below an at-trend twin
    let mut stretched = q(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    stretched.above_ma_pct = 100.0; // maximally stretched
    assert!(growth_score(&stretched, &t).unwrap() < growth_score(&rocket, &t).unwrap());
    assert!((core::above_long_ma_pct(&[50.0, 50.0, 100.0], 3) - 50.0).abs() < 1e-9); // 100 vs mean 66.67
    assert_eq!(core::above_long_ma_pct(&[100.0, 100.0, 50.0], 3), 0.0); // below the mean -> 0
    // (3) consistency: a near-high equity negative over 5Y (mooned-then-bled) is rejected despite a fat 10Y
    assert!(growth_score(&q(0.0, &[("1Y", 60.0), ("5Y", -20.0), ("10Y", 500.0)]), &t).is_none());
    let mut bled_crypto = q(0.0, &[("1Y", 60.0), ("5Y", -20.0), ("10Y", 500.0)]); // ...but crypto 5Y is noise
    bled_crypto.ticker = "ETH-EUR".into();
    assert!(growth_score(&bled_crypto, &t).is_some());
    // (4) NUPL damp: euphoric market (high NUPL) shrinks the multiplier; below the line / unknown = 1.0
    assert_eq!(nupl_damp(None, &t), 1.0);
    assert_eq!(nupl_damp(Some(0.0), &t), 1.0); // below euphoria line
    assert!(nupl_damp(Some(0.75), &t) < 1.0 && nupl_damp(Some(0.75), &t) > t.nupl_damp_floor);
    assert!((nupl_damp(Some(1.0), &t) - t.nupl_damp_floor).abs() < 1e-9); // peak euphoria -> floor

    // --- (A) trend consistency: R² of the log-price line, damps CAGR endpoint-luck ---
    assert!(core::trend_r2(&[1.0, 2.0, 4.0, 8.0, 16.0]) > 0.999); // perfect exponential -> R²≈1
    assert!(core::trend_r2(&[1.0, 100.0, 2.0, 200.0, 3.0]) < 0.5); // zigzag -> lumpy
    assert_eq!(core::trend_r2(&[5.0]), 0.0); // too short
    // (C) max drawdown: worst peak-to-trough
    assert!((core::max_drawdown_pct(&[100.0, 50.0, 75.0]) - 50.0).abs() < 1e-9);
    assert_eq!(core::max_drawdown_pct(&[1.0, 2.0, 3.0]), 0.0); // monotone up -> never down
    // (A) quality_factors: a smooth path (R²=1) keeps a higher consistency multiplier than a lumpy one.
    // The damp is DISABLED by default (consistency_floor 1.0), so pin an explicit floor to test the mechanism.
    let t_consist = BuyHeuristic { consistency_floor: 0.5, ..t.clone() };
    assert!(quality_factors(&{ let mut x = q(5.0, &[("10Y", 40.0)]); x.trend_r2 = 1.0; x }, 20.0, &t_consist).0
        > quality_factors(&{ let mut x = q(5.0, &[("10Y", 40.0)]); x.trend_r2 = 0.0; x }, 20.0, &t_consist).0);
    // (B) risk_reward: same CAGR, the lower-volatility name earns a bigger Sharpe-ish bonus
    assert!(quality_factors(&{ let mut x = q(5.0, &[]); x.volatility_pct = Some(1.0); x }, 20.0, &t).1
        > quality_factors(&{ let mut x = q(5.0, &[]); x.volatility_pct = Some(4.0); x }, 20.0, &t).1);
    // (C) risk_reward: same CAGR, the SHALLOWER max-drawdown name earns a bigger Calmar bonus
    assert!(quality_factors(&{ let mut x = q(5.0, &[]); x.max_drawdown_pct = 20.0; x }, 20.0, &t).1
        > quality_factors(&{ let mut x = q(5.0, &[]); x.max_drawdown_pct = 90.0; x }, 20.0, &t).1);
    // (A) end-to-end: a steady compounder outranks an otherwise-identical lumpy one in the on-sale lane
    let mut steady_q = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    steady_q.trend_r2 = 0.95;
    let mut lumpy_q = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    lumpy_q.trend_r2 = 0.10;
    assert!(buy_score(&steady_q, &t_consist).unwrap() > buy_score(&lumpy_q, &t_consist).unwrap());

    // (A) crypto trust: a young EUR pair (5Y but no 10Y, like BTC-EUR) is NOT halved — 5Y is proven
    // enough for crypto; an equity still needs a 10Y leg, a barely-listed coin (1Y only) is still cut.
    assert!((trust_factor(&q(20.0, &[("1Y", 30.0), ("5Y", 200.0)]), true) - 1.0).abs() < 1e-9);
    assert_eq!(trust_factor(&q(20.0, &[("1Y", 30.0)]), true), 0.5); // crypto, only 1Y -> unproven
    assert_eq!(trust_factor(&q(5.0, &[("5Y", 40.0)]), false), 0.5); // equity, no 10Y -> halved
    assert!((trust_factor(&q(5.0, &[("10Y", 40.0)]), false) - 1.0).abs() < 1e-9);
    // end-to-end: a 5Y-only crypto (BTC-EUR shape) is admitted to the growth lane and NOT trust-halved
    let mut btc_young = q(20.0, &[("1Y", 30.0), ("5Y", 200.0)]); // no 10Y leg, like the young EUR pair
    btc_young.ticker = "BTC-EUR".into();
    assert!((trust_factor(&btc_young, true) - 1.0).abs() < 1e-9);
    assert!(growth_score(&btc_young, &t).is_some());

    // (#4) combine_damps: empty/all-1.0 -> 1.0; a lone 0.5 softens to 0.5^(1/n) (bounded, NOT the raw
    // product); the geomean of several mild damps stays well above their product (no silent nuke).
    assert_eq!(combine_damps(&[]), 1.0);
    assert_eq!(combine_damps(&[1.0, 1.0, 1.0]), 1.0);
    assert!((combine_damps(&[0.5, 1.0, 1.0]) - 0.5_f64.powf(1.0 / 3.0)).abs() < 1e-9);
    assert!(combine_damps(&[0.5, 0.4, 0.5]) > 0.5 * 0.4 * 0.5); // geomean bounded above the product
    assert!(combine_damps(&[0.9, 0.5]) < combine_damps(&[0.9, 0.9])); // still monotone in each term

    // (#3) 12-1 momentum: +50% over 1Y, +20% over 1M -> 1.5/1.2 - 1 = +25%; reward = weight × min(25, cap)
    let mom = q(20.0, &[("1Y", 50.0), ("1M", 20.0)]);
    assert!((mom_12_1_pct(&mom).unwrap() - 25.0).abs() < 1e-9);
    assert!((mom_12_1_reward(&mom, &t) - t.mom_12_1_weight * 25.0).abs() < 1e-9);
    // negative 12-1 momentum -> clamped to 0 reward (no reward, no extra penalty)
    let down = q(20.0, &[("1Y", -30.0), ("1M", 10.0)]);
    assert!(mom_12_1_pct(&down).unwrap() < 0.0);
    assert_eq!(mom_12_1_reward(&down, &t), 0.0);
    // missing the 1M leg -> None -> 0 reward (never punished for absent data)
    let bare = q(20.0, &[("1Y", 50.0)]);
    assert!(mom_12_1_pct(&bare).is_none() && mom_12_1_reward(&bare, &t) == 0.0);

    // (F) ROE quality reward: positive ROE -> weight×roe (capped); None/negative -> 0 (neutral)
    let mut hi_roe = q(20.0, &[("1Y", 10.0)]);
    hi_roe.roe = Some(30.0);
    assert!((quality_reward(&hi_roe, &t) - t.quality_weight * 30.0).abs() < 1e-9);
    hi_roe.roe = Some(t.quality_cap + 500.0); // a buyback-levered outlier is clamped at the cap
    assert!((quality_reward(&hi_roe, &t) - t.quality_weight * t.quality_cap).abs() < 1e-9);
    hi_roe.roe = Some(-50.0); // loss-making -> no quality bonus
    assert_eq!(quality_reward(&hi_roe, &t), 0.0);
    assert_eq!(quality_reward(&bare, &t), 0.0); // roe None -> 0

    // EU-buyability gate: crypto majors + UCITS ETFs + US/Canada/EU-listed stocks pass; a US-domiciled
    // ETF (no PRIIPs KID) and an Asian-only listing are dropped — EU retail can't buy them.
    let mut us_etf = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    us_etf.name = "SPDR S&P 500 ETF Trust".into();
    us_etf.ticker = "SPY".into();
    us_etf.market = "USA".into();
    assert!(!eu_buyable(&us_etf)); // US-domiciled ETF -> not EU-buyable
    let mut ucits = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    ucits.name = "iShares Core S&P 500 UCITS ETF".into();
    ucits.market = "UK".into();
    assert!(eu_buyable(&ucits)); // UCITS wrapper -> buyable
    // the bug this fixes: a UCITS ETF whose Yahoo shortName carries NO "ETF"/"UCITS" marker still
    // classifies as an ETF (via instrumentType) and stays buyable on its European listing.
    let mut bare = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    bare.name = "ISHARES III PLC ISHRS CORE MSCI".into(); // real marker-less ETF shortName
    bare.instrument_type = "ETF".into();
    bare.market = "Ireland".into();
    assert!(quote_is_etf(&bare) && !is_etf(&bare.name)); // typed as ETF, not name-matched
    assert!(eu_buyable(&bare)); // EU venue -> buyable despite the marker-less name
    let mut hk = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    hk.name = "Tencent Holdings".into();
    hk.market = "Hong Kong".into();
    assert!(!eu_buyable(&hk)); // HK-only listing off most EU retail brokers
    let mut us_stk = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    us_stk.name = "Apple Inc.".into(); // market defaults to "USA"
    assert!(eu_buyable(&us_stk));
    let mut btc_b = q(20.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    btc_b.ticker = "BTC-EUR".into();
    assert!(eu_buyable(&btc_b)); // crypto major
    // end-to-end: `ranked` drops the US ETF even though it scores above min_score
    assert!(buy_score(&us_etf, &t).unwrap() > t.min_score);
    assert!(ranked(std::slice::from_ref(&us_etf), &t, buy_score, t.min_score).is_empty());
    assert_eq!(ranked(std::slice::from_ref(&ucits), &t, buy_score, t.min_score).len(), 1);
}
