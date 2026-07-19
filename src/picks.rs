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
use std::collections::{HashMap, HashSet};

/// The longest >2Y leg as (cumulative %, span years): 20Y, else 10Y, else 5Y. None if the asset
/// has no >2Y history. The cumulative % feeds the corpse GATE; annualized (CAGR) it feeds the SCORE,
/// so a 10Y and a 20Y leg are compared on the same %/yr footing.
fn long_leg(quote: &Quote) -> Option<(f64, f64)> {
    for (label, years) in [("20Y", 20.0), ("10Y", 10.0), ("5Y", 5.0)] {
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
fn long_leg_fixed(quote: &Quote, fixed_years: u32) -> Option<(f64, f64)> {
    if fixed_years == 0 {
        return long_leg(quote);
    }
    match perf_pct(quote, &format!("{fixed_years}Y")) {
        Some(p) => Some((p, fixed_years as f64)),
        None => long_leg(quote), // pinned leg absent -> longest leg, docked by trust_factor
    }
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

/// Confidence multiplier — halve a name without a long PROVEN record. Equities should carry a 10Y
/// leg; crypto can't (Yahoo's EUR crypto pairs are too young to ever show 10Y), so for them a 5Y leg
/// is "proven enough". Without this, BTC is halved for a history gap that's purely an artifact of the
/// EUR quote, and vanishes from the growth lane despite a 15-year track record.
fn trust_factor(quote: &Quote, crypto: bool) -> f64 {
    let needed = if crypto { "5Y" } else { "10Y" };
    if perf_pct(quote, needed).is_none() {
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

/// (F) Profitability/QUALITY reward: trailing ROE, the canonical quality factor (high-ROE firms
/// out-compound long-run). None (crypto/ETF/no FMP key) → 0 = neutral; negative ROE clamps to 0 (no
/// bonus, the gates handle bleeders). Shared by both lanes. BACKTEST-BLIND: ROE is point-in-time so
/// the price-only walk-forward can't score it — deliberately weighted small (`quality_weight`).
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
    let (long_cum, long_years) =
        long_leg(quote).or_else(|| if crypto { perf_pct(quote, "1Y").map(|p| (p, 1.0)) } else { None })?;
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
    let long_reward = tuning.long_trend_weight * long_cagr.min(tuning.long_trend_cap) * discount_frac; // (A)
    let cheap_reward = tuning.cheap_weight * quote.below_ma_pct.min(tuning.cheap_cap); // (C)
    let dividend_reward = tuning.dividend_weight * dividend_yield_1y(quote).min(tuning.dividend_cap); // (D)

    let risk_reward = risk_bonus(quote, long_cagr, tuning.onsale_sharpe_weight, tuning.calmar_weight, tuning); // (B/C) on-sale lane's own Sharpe weight
    let base = tuning.discount_weight * discount * health * momentum // (#4) demoted: dip-depth ranks backwards on peer-relative backtest
        + long_reward
        + cheap_reward
        + dividend_reward
        + risk_reward
        + quality_reward(quote, tuning); // (F) ROE profitability tilt (BACKTEST-BLIND, small)
    let value = value_factor(quote, tuning.ref_pe); // (E) cheap lifts, rich dampens, unknown neutral
    let decline = sustained_decline_factor(quote, tuning); // (B) multi-year-bleed dock
    let trust = trust_factor(quote, crypto); // (A) equities need a 10Y leg; crypto's young EUR pairs need only 5Y
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
    trend: f64,        // min(long_cagr, long_trend_cap)
    accel: f64,        // clamp(return_1y − long_cagr, 0, growth_accel_cap)
    trend_term: f64,   // growth_trend_weight × trend
    accel_term: f64,   // growth_accel_weight × accel
    risk_reward: f64,  // (B/C) Sharpe+Calmar bonus
    quality: f64,      // (F) quality_weight × ROE
    dividend: f64,     // (D) dividend_weight × min(yield, cap)
    fund: f64,         // (G) growth_fund_weight × clamp(fund_factor, 0, cap)
    mom121: f64,       // (M) growth_mom121_weight × clamp(12-1 mom, 0, cap)
    smooth: f64,       // (E) growth_smoothness_weight × trend_r2
    underwater: f64,   // −growth_underwater_weight × underwater_yrs (drawdown-duration penalty; 0 when off/None)
    base: f64,         // sum of the nine terms above
    proximity: f64,    // range_pct / 100
    value_raw: f64,    // (E) raw P/E value_factor (ref_pe/PE clamped)
    value: f64,        // 1 + growth_value_weight × (value_raw − 1)
    trust: f64,        // (A) history-completeness damp
    overext: f64,      // min(above_ma_pct, overext_cap)
    overext_cap: f64,  // the class's overextension cap
    overext_damp: f64, // 1 − (overext/cap)×(1−floor)
    damp: f64,         // geomean(trust, overext_damp)
    liq_bonus: f64,    // (L) turnover_weight × ln(max(turnover/1e9, 1))
    ter_damp: f64,     // (T) ETF cost drag (1−TER)^20; 1.0 for stocks/crypto/None or when growth_ter_drag off
    score: f64,        // base × proximity × value × damp × ter_damp + liq_bonus  (or base × geomean(trust,overext,prox,value) × ter_damp + liq_bonus when #8 growth_geomean_fold)
}

/// Score a quote as a MOMENTUM/GROWTH candidate — the MIRROR of `buy_score`. The on-sale lane fades
/// a name's score to ~0 as it nears its high (a proven compounder at a new high has no "discount"),
/// so it never surfaces quality that's expensive *because* it keeps winning. This lane is exactly
/// that set: a name AT/NEAR its own range high, with a strong proven long-term CAGR, still climbing.
///
/// ```text
///   base  = growth_trend_weight × min(long_cagr, long_trend_cap)
///         + growth_accel_weight × clamp(1Y − long_cagr, 0, growth_accel_cap)   // recent outpaces long => accelerating
///         + quality_reward                                                     // (F) ROE profitability tilt (BACKTEST-BLIND, small)
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
    // a "20yr+ proven CAGR" candidate must HAVE a multi-year record. Crypto used to fall back to its
    // 1Y leg here — but that admitted no-history tokens (microNFT, freshly-listed scams with a
    // +100000% data-artifact year) into a lane that promises a proven long trend. Require a real >2Y
    // leg for crypto too: trust_factor already treats 5Y as "proven enough" for young EUR pairs, so
    // this just promotes that bar from a soft halving to a hard gate (BTC/ETH/XMR/… all have 5Y).
    let (long_cum, long_years) = long_leg_fixed(quote, tuning.fixed_cagr_years)?; // (#15) pin the CAGR window
    // (#14) rank the long trend on the endpoint-robust log-slope CAGR (precomputed on the quote at
    // fetch/backtest build) when enabled; else the two-point endpoint CAGR. Both knobs default off ->
    // long_cagr is byte-identical to before, so the validated edge is untouched until a flip is validated.
    let long_cagr = if tuning.use_trend_cagr {
        quote.trend_cagr.unwrap_or_else(|| core::cagr(long_cum, long_years))
    } else {
        core::cagr(long_cum, long_years)
    };
    let min_cagr = if crypto { tuning.growth_min_cagr_crypto } else { tuning.growth_min_cagr };
    if long_cagr < min_cagr {
        return None; // equities: weak trend = expensive laggard. crypto: looser floor (show all growers vs BTC)
    }
    let return_1y = perf_pct(quote, "1Y")?;
    // equities must be climbing this year; crypto is allowed down to its looser 1Y floor so the market
    // base (Bitcoin, often red year-on-year) and near-BTC coins still appear, ranked vs BTC.
    let y1_floor = if crypto { tuning.min_1y_pct_crypto } else { 0.0 };
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
    if !crypto && perf_pct(quote, "5Y").is_some_and(|return_5y| return_5y <= 0.0) {
        // (3) consistency: a near-high name negative over 5Y mooned-then-bled — its great 10Y CAGR is a
        // stale endpoint, not a durable trend. Require the mid leg to hold too. (Crypto 5Y is
        // peak-anchored noise; the range gate already excludes bled coins there, so skip it.)
        return None;
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
    // from the merged monthly-max series; None in backtest (display-era field), so this leg is
    // live-only and edge-blind by construction.
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

    // ---- SCORE ----
    let trend = long_cagr.min(tuning.long_trend_cap); // proven compounding, capped like the on-sale lane
    let accel = (return_1y - long_cagr).clamp(0.0, tuning.growth_accel_cap); // last year outpacing the long run = building
    let proximity = quote.range_pct / 100.0; // 0.7..1.0 — closer to the high = stronger confirmation
    let risk_reward = risk_bonus(quote, long_cagr, tuning.sharpe_weight, tuning.calmar_weight, tuning); // (B/C) growth lane's Sharpe weight
    // (M) 12-1 momentum: trailing-year return EXCLUDING the last month (skip the short-term-reversal
    // month — Jegadeesh-Titman). Price-only, so it's validated end-to-end (backtest_quote has 1Y/1M),
    // unlike the BACKTEST-BLIND div/ROE/fund tilts. Missing 1M -> skip-month = 0. Guard the denominator
    // against a near-total-wipeout 1M (>= -99%) so the ratio can't blow up.
    let r1m = perf_pct(quote, "1M").unwrap_or(0.0);
    let mom121 = ((1.0 + return_1y / 100.0) / (1.0 + r1m / 100.0).max(0.01) - 1.0) * 100.0;
    // each term broken into its own local so `ScoreParts`/`explain_growth_score` can show the arithmetic
    // without recomputing (single source). Summed in the SAME order as before -> byte-identical base.
    let trend_term = tuning.growth_trend_weight * trend;
    let accel_term = tuning.growth_accel_weight * accel;
    let quality = quality_reward(quote, tuning); // (F) ROE profitability tilt (BACKTEST-BLIND, small)
    let dividend = tuning.dividend_weight * dividend_yield_1y(quote).min(tuning.dividend_cap); // (D) total-return tilt: closes are price-only (no adjclose) so divs are missing from the CAGR. BACKTEST-BLIND (no as-of divs), small (near-high growers are low-yield). 52w-high anchor was sweep-tested here too and REGRESSED the 12y edge at every weight -> dropped
    // (G) as-of FUNDAMENTAL tilt. Unlike the BACKTEST-BLIND terms above, this one IS validatable: the
    // backtest attaches the as-of factor to quote.fund_factor so `backtest <set> fund` can ablate it.
    // Floor at 0 (only reward the factor, don't penalise a missing/negative one) and cap the artifact.
    // weight 0 (default) -> this whole term is 0 -> growth_score is byte-identical to the pre-(G) lane.
    let fund = tuning.growth_fund_weight * quote.fund_factor.unwrap_or(0.0).clamp(0.0, tuning.growth_fund_cap);
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
    let trust = trust_factor(quote, crypto); // (A) equities need a 10Y leg; crypto's young EUR pairs need only 5Y
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
    let score = if tuning.growth_geomean_fold {
        base * combine_damps(&[trust, overext_damp, proximity, value]) * ter_damp + liq_bonus
    } else {
        base * proximity * value * damp * ter_damp + liq_bonus
    };
    Some(ScoreParts {
        long_cagr, return_1y, trend, accel, trend_term, accel_term, risk_reward, quality, dividend,
        fund, mom121: mom_term, smooth, underwater, base, proximity, value_raw, value, trust, overext,
        overext_cap, overext_damp, damp, liq_bonus, ter_damp, score,
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

/// Every growth gate this quote FAILS, in gate order: (gate_name, human "why", is_close_miss).
/// `None` = not assessable as a growth candidate at all (leveraged / stablecoin / unknown turnover /
/// no 1Y data); a name with no 5y+ leg returns a single "history" fail (so a pinned young ETF explains
/// its 0.0 rather than vanishing); empty vec = clears every gate. Shared by the near-miss tail above and
/// `check`'s held-name gate review, so a held name that would no longer rank gets flagged with the
/// same wording the screen tail uses.
pub fn gate_failures(quote: &Quote, tuning: &BuyHeuristic) -> Option<Vec<(&'static str, String, bool)>> {
    let crypto = is_currency_quoted(&quote.ticker);
    // not a near-miss CANDIDATE: structural rejects / missing data have nothing to "almost pass"
    if is_leveraged(&quote.name) || is_commodity_etf(quote) || (crypto && is_stablecoin(&quote.ticker)) {
        return None;
    }
    let turnover = quote.avg_turnover_eur?; // unknown turnover -> not assessable as a compounder
    // (#33) minimum-age gate. Reported as an explicit "young" reason. A name too young for the fixed
    // CAGR leg would `?`-bail the next line to a SILENT None (0.0 in the table, no reason) — so when
    // the leg is missing AND the age gate fires, return the young reason alone instead of dropping it.
    let young_fail = |a: f64| ("young", format!("{a:.0}y listed (need ≥{:.0}y)", tuning.growth_min_age_years), a >= tuning.growth_min_age_years - 1.0);
    let too_young = tuning.growth_min_age_years > 0.0 && quote.age_years.is_some_and(|a| a < tuning.growth_min_age_years);
    let Some((long_cum, long_years)) = long_leg_fixed(quote, tuning.fixed_cagr_years) else {
        // No 5y+ leg -> the name can't be ranked at all (score_parts `?`-bails here too -> a silent 0.0).
        // ALWAYS explain it, so a pinned young ETF (VUAA 2y, SPYL 3y) prints a reason instead of a mystery
        // 0.0 in the screen's gate-review footer. Prefer the actionable "young" wording when the age gate
        // is the floor that fired; otherwise a plain "history" note (nothing to tune, it just needs more time).
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
    let long_cagr = if tuning.use_trend_cagr {
        quote.trend_cagr.unwrap_or_else(|| core::cagr(long_cum, long_years))
    } else {
        core::cagr(long_cum, long_years)
    };
    let min_range = if crypto { tuning.growth_min_range_pct_crypto } else { tuning.growth_min_range_pct };
    let min_cagr = if crypto { tuning.growth_min_cagr_crypto } else { tuning.growth_min_cagr };
    let y1_floor = if crypto { tuning.min_1y_pct_crypto } else { 0.0 };
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
    if long_cagr < min_cagr {
        // close = within 1.5 pp of the floor. 4.0 flooded the screen tail with ~55 "10%/yr vs a 14%
        // bar" rows — that's a different asset class, not a near miss; 1.5 keeps the genuine
        // one-notch-out compounders (13.9 vs 14.0) and nothing else.
        fails.push(("cagr", format!("{long_cagr:.1}%/yr (need ≥{min_cagr:.1}%)"), long_cagr >= min_cagr - 1.5));
    }
    if return_1y <= y1_floor {
        fails.push(("1Y+", format!("1Y {return_1y:+.1}% (need >{y1_floor:.1}%)"), return_1y > y1_floor - 10.0));
    }
    if r1m <= knife {
        fails.push(("1M-knife", format!("1M {r1m:+.1}% (floor {knife:.1}%)"), r1m > knife - 8.0));
    }
    if !crypto {
        if let Some(r5) = perf_pct(quote, "5Y") {
            if r5 <= 0.0 {
                fails.push(("5Y+", format!("5Y {r5:+.1}% (need >0)"), r5 > -15.0));
            }
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
                        && long_leg_fixed(t, tuning.fixed_cagr_years).is_some() // twin must HAVE the record
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
/// that FAILS at least one growth gate (not-assessable names — leveraged/stablecoin/missing data —
/// are skipped). Shared by `check` (the whole watchlist) and `screen` (its pinned names, which
/// bypass the score trim and print as 0.0 — this says which gate they tripped). Empty vec -> the
/// caller prints no block, so clean tables stay clean.
pub fn gate_review_lines(quotes: &[&Quote], tuning: &BuyHeuristic, ticker_w: usize) -> Vec<String> {
    quotes
        .iter()
        .filter_map(|q| {
            let fails = gate_failures(q, tuning)?;
            if fails.is_empty() {
                return None;
            }
            let why = fails.iter().map(|(gate, why, _)| format!("{gate}: {why}")).collect::<Vec<_>>().join("; ");
            // (round 54) hold-core names fail the growth gates FOREVER BY DESIGN (a broad index fund
            // never clears a 14%/yr momentum bar) — without this tag the same three pinned funds read
            // as unresolved warnings every single run.
            let hold = if core::hold_suitable(q) { "  (hold-core H — growth gates don't apply)" } else { "" };
            Some(format!("  {:<ticker_w$} {}{hold}", q.ticker, why))
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
    s.push_str(&format!("    trend    = growth_trend_weight × min(CAGR, cap)        = {:.2} × {:.2} = {:.2}\n",
        tuning.growth_trend_weight, p.trend, p.trend_term));
    s.push_str(&format!("    accel    = growth_accel_weight × clamp(1Y−CAGR,0,cap)  = {:.2} × {:.2} = {:.2}   (1Y {:.1} − CAGR {:.1})\n",
        tuning.growth_accel_weight, p.accel, p.accel_term, p.return_1y, p.long_cagr));
    s.push_str(&format!("    risk     = Sharpe+Calmar bonus                        = {:.2}\n", p.risk_reward));
    s.push_str(&format!("    quality  = quality_weight × ROE                       = {:.2}\n", p.quality));
    s.push_str(&format!("    dividend = dividend_weight × min(1Y yield, cap)       = {:.2}\n", p.dividend));
    s.push_str(&format!("    fund     = growth_fund_weight × clamp(fund_factor)    = {:.2}\n", p.fund));
    s.push_str(&format!("    mom121   = growth_mom121_weight × clamp(12-1 mom)     = {:.2}\n", p.mom121));
    s.push_str(&format!("    smooth   = growth_smoothness_weight × trend_r2 (R²)   = {:.2}\n", p.smooth));
    s.push_str(&format!("    underwtr = −growth_underwater_weight × underwater_yrs = {:.2}\n", p.underwater));
    s.push_str(&format!("    base (sum)                                            = {:.2}\n", p.base));
    s.push_str(&format!("  proximity    = range_pct / 100                          = {:.1} / 100 = {:.3}\n",
        p.proximity * 100.0, p.proximity));
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
    if tuning.growth_geomean_fold {
        s.push_str(&format!("\n  SCORE = {:.2} × geomean(trust {:.3}, overext {:.3}, prox {:.3}, value {:.3}){ter_frag} + {:.2} = {:.2}\n",
            p.base, p.trust, p.overext_damp, p.proximity, p.value, p.liq_bonus, p.score));
    } else {
        s.push_str(&format!("\n  SCORE = {:.2} × {:.3} × {:.3} × {:.3}{ter_frag} + {:.2} = {:.2}\n",
            p.base, p.proximity, p.value, p.damp, p.liq_bonus, p.score));
    }
    if (displayed - p.score).abs() > 1e-6 {
        s.push_str(&format!("  crypto NUPL + BTC-relative adjustment: {:.2} → {displayed:.2} (the table value)\n", p.score));
    }
    s.push_str("  (BACKTEST-BLIND terms — quality/dividend/fund(if FMP-only)/value — were never in the\n   walk-forward; the validated edge is the price-only trend/accel/risk path. NOT advice.)\n");
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
    picks.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap()
            .then(b.0.avg_turnover_eur.unwrap_or(0.0).partial_cmp(&a.0.avg_turnover_eur.unwrap_or(0.0)).unwrap())
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
    ColSpec { key: "rank", hdr: "RANK", width: 6, right: false },
    ColSpec { key: "name", hdr: "NAME", width: 0, right: false },
    ColSpec { key: "ticker", hdr: "TICKER", width: 0, right: false },
    ColSpec { key: "market", hdr: "MARKET", width: 0, right: false },
    ColSpec { key: "price", hdr: "PRICE(EUR)", width: 0, right: true },
    ColSpec { key: "cagr", hdr: "CAGR", width: 8, right: true }, // whole-life %/yr since listing (display; ranking uses the fixed-horizon ladder)
    ColSpec { key: "trcagr", hdr: "TR-CAGR", width: 8, right: true }, // whole-life %/yr WITH the dividend sum added (lower-bound total return; ≈ CAGR for Acc/non-payers)
    ColSpec { key: "1h", hdr: "1H", width: 7, right: true },
    ColSpec { key: "6h", hdr: "6H", width: 7, right: true },
    ColSpec { key: "12h", hdr: "12H", width: 7, right: true },
    ColSpec { key: "1d", hdr: "1D", width: 8, right: true },
    ColSpec { key: "1w", hdr: "1W", width: 8, right: true },
    ColSpec { key: "1m", hdr: "1M", width: 8, right: true },
    ColSpec { key: "1y", hdr: "1Y", width: 8, right: true },
    ColSpec { key: "5y", hdr: "5Y", width: 8, right: true },
    ColSpec { key: "10y", hdr: "10Y", width: 8, right: true },
    ColSpec { key: "20y", hdr: "20Y", width: 8, right: true },
    ColSpec { key: "yrs", hdr: "YRS", width: 4, right: true },       // real listing age in years — how much record backs the CAGR headline
    ColSpec { key: "vol", hdr: "VOL", width: 7, right: true },       // daily-return stdev (risk)
    ColSpec { key: "maxdd", hdr: "MAXDD", width: 8, right: true },   // worst peak-to-trough drop ever (pain)
    ColSpec { key: "r2", hdr: "R2", width: 6, right: true },         // log-trend steadiness 0..1 (smoothness)
    ColSpec { key: "abv-ma", hdr: "ABV-MA", width: 8, right: true }, // % above the 200wk SMA (overextension)
    ColSpec { key: "pe", hdr: "P/E", width: 7, right: true },        // trailing P/E (FMP key only)
    ColSpec { key: "peg", hdr: "PEG", width: 6, right: true },       // P/E ÷ long CAGR — valuation vs growth (<1 = cheap for its growth)
    ColSpec { key: "roe", hdr: "ROE", width: 7, right: true },       // trailing return-on-equity (quality)
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
];

/// Canonical default layout when `widths.columns` is empty: the historical table PLUS `cagr` and
/// `maxdd` (return + worst-pain — what a 20yr buy-and-hold screen was missing). Users add vol/r2/pe/roe/
/// div/abv-ma by listing them in `widths.columns`.
const DEFAULT_COLUMNS: &[&str] = &[
    "rank", "name", "ticker", "market", "price", "cagr", "yrs", "1h", "6h", "12h", "1d", "1w", "1m", "1y",
    "5y", "10y", "20y", "maxdd", "off-hi", "upside", "turnover", "score",
];

/// Resolve `widths.columns` (config) to the ordered `ColSpec`s to print. Empty config -> `DEFAULT_COLUMNS`;
/// otherwise the listed keys in order. Unknown keys are skipped (a typo drops that column, never panics).
fn active_columns(cfg: &[String]) -> Vec<&'static ColSpec> {
    let keys: Vec<&str> = if cfg.is_empty() { DEFAULT_COLUMNS.to_vec() } else { cfg.iter().map(String::as_str).collect() };
    keys.iter().filter_map(|k| COLUMNS.iter().find(|c| c.key.eq_ignore_ascii_case(k))).collect()
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

/// Display name for any quote — the raw Yahoo name minus per-class noise. Crypto: drop the quote-currency
/// word ("Bitcoin EUR" -> "Bitcoin"; the ticker column already carries -EUR/-USD). ETF: drop the
/// umbrella-company prefix ("iShares VII PLC - iShares NASDAQ 100 UCITS ETF" -> the fund part) — only
/// when what follows " - " is the fund name (has ETF/UCITS), so share-class tails like "- USD Acc" never
/// match. Everything else: strip one corporate legal suffix. Used by the picks table AND `check`'s
/// summary line; lookups (BF TER name match) keep the full name.
pub fn clean_name(quote: &Quote) -> String {
    let n = display_name(&quote.name);
    let is_crypto = is_currency_quoted(&quote.ticker) || quote.instrument_type.eq_ignore_ascii_case("CRYPTOCURRENCY");
    if is_crypto {
        n.strip_suffix(" USD").or_else(|| n.strip_suffix(" EUR")).map(str::to_string).unwrap_or(n)
    } else if quote.instrument_type.eq_ignore_ascii_case("ETF") {
        match n.split_once(" - ") {
            Some((_, fund)) if fund.contains("ETF") || fund.contains("UCITS") => fund.to_string(),
            _ => n,
        }
    } else {
        n
    }
}

/// Render ONE cell's text for column `key`. `mark` is the rank label (number + `*`/`#` flags). All values
/// come from already-fetched `Quote` fields — pure formatting, no scoring. Unknown key -> "?".
fn col_cell(key: &str, quote: &Quote, score: f64, mark: &str) -> String {
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
    let stock_only_na = is_etf || is_crypto; // P/E, PEG, ROE don't apply here
    let etf_only_na = is_equity || is_crypto; // TER doesn't apply here
    match key {
        "rank" => mark.to_string(),
        "name" => clean_name(quote),
        "ticker" => quote.ticker.clone(),
        "market" => quote.market.clone(),
        "price" => quote.price.clone(),
        // proven long-term CAGR (%/yr) from the longest available leg — the annualized trend the ranking
        // rewards, shown so a reader sees "+27%/yr for 10y" not just a +900% cumulative blob.
        // whole-life endpoint CAGR over the FULL (monthly-backfilled) history — the honest "what did
        // this compound at since listing" headline. Ranking/gates still use the fixed 20/10/5Y ladder,
        // so this cell can differ from the CAGR a gate message quotes.
        "cagr" => quote.life_cagr.map_or("n/a".to_string(), |v| format!("{v:+.0}%")),
        "trcagr" => quote.tr_cagr.map_or("n/a".to_string(), |v| format!("{v:+.0}%")),
        // span of the CAGR leg (20/10/5) — a "+16% over 20" and a "+16% over 5" are NOT the same
        // conviction; this makes the record length behind the headline number visible per row.
        "yrs" => quote.age_years.map_or("n/a".to_string(), |y| format!("{y:.0}")),
        "1h" => pct1(quote.intraday[0]),
        "6h" => pct1(quote.intraday[1]),
        "12h" => pct1(quote.intraday[2]),
        "1d" | "1w" | "1m" | "1y" | "5y" | "10y" | "20y" => pct1(perf_pct(quote, &key.to_uppercase())),
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
        // PEG = trailing P/E ÷ long-term CAGR (%/yr). Needs both (P/E is FMP-key-only) AND positive growth.
        // <1 = cheap for its growth, >2 = pricey. Price-CAGR proxy for earnings growth — display-only.
        "peg" if stock_only_na => "—".to_string(),
        "peg" => match (quote.pe_ratio, long_leg(quote).map(|(c, y)| core::cagr(c, y))) {
            (Some(pe), Some(g)) if g > 0.0 => format!("{:.2}", pe / g),
            _ => "n/a".to_string(),
        },
        "roe" if stock_only_na => "—".to_string(),
        // |ROE| > 100% is almost always a buyback-shrunk or negative-equity DENOMINATOR (AAPL "+152%",
        // HCA "-113%"), not operating quality -> n/m (not meaningful). The score side is unaffected:
        // the quality term already clamps ROE to quality_cap.
        "roe" => quote.roe.map_or("n/a".to_string(), |v| if v.abs() > 100.0 { "n/m".to_string() } else { format!("{v:+.0}%") }),
        "div" => {
            let d = dividend_yield_1y(quote);
            if d > 0.0 {
                format!("{d:.2}%") // 2 decimals so a token yield (NVDA ~0.02%) prints instead of rounding to 0.0%
            } else {
                "n/a".to_string()
            }
        }
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
fn print_picks(title: &str, picks: &[(&Quote, f64)], n: usize, w: &Widths, pinned: &HashSet<&str>, owned: &Owned, hide: &[&str], tuning: &BuyHeuristic) {
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
    // one printed row; `mark` is the rank label (number + "*" pinned / "#" fundamentals flags). Flags on
    // the rank cell, not the name, so name truncation can't eat them.
    let row = |mark: &str, quote: &Quote, score: f64| {
        let line = cols
            .iter()
            .map(|c| fmt_cell(&col_cell(c.key, quote, score, mark), col_width(c, w), c.right))
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
    let mark = |quote: &Quote, i: usize| {
        format!("{}{}{}{}{}{}{}", i + 1, star(quote), enriched(quote), braked(quote), bridged(quote), holdable(quote), held(quote))
    };
    // pinned tickers that ranked BELOW the cut still print (with their real rank + "*") so you can
    // compare a holding against the tops above even when it doesn't make the top-N.
    let below_cut = picks.iter().enumerate().skip(n).filter(|(_, (quote, _))| pinned.contains(quote.ticker.as_str()));
    let mut seen = String::new(); // rank-flag chars that actually printed, drives the legend line
    for (i, (quote, score)) in picks.iter().enumerate().take(n).chain(below_cut) {
        let m = mark(quote, i);
        for flag in ['*', '#', '!', '~', 'H', 'o'] {
            if m.contains(flag) && !seen.contains(flag) {
                seen.push(flag);
            }
        }
        row(&m, quote, *score);
    }
    // Legend: explain only the flags THIS table used, so clean tables stay clean.
    let legend: Vec<String> = [
        ("*", "pinned watchlist name"),
        ("#", "score used live fundamentals, not price-only"),
        ("!", "late-cycle: price >= cap above 200wk trend, brake floored — conviction is the SCORE, not the rank"),
        ("~", "history bridged from configured older twin (history_proxy) — CAGR/YRS describe the strategy, not this listing"),
        ("H", "hold-suitable: broad + cheap + physical + accumulating + large — a buy-and-hold-20yr core, independent of the momentum rank"),
        ("o", "already held (broker portfolio)"),
    ]
    .iter()
    .filter(|(flag, _)| seen.contains(flag))
    .map(|(flag, what)| format!("{flag} = {what}"))
    .collect();
    if !legend.is_empty() {
        println!("  ({})", legend.join("; "));
    }
}

/// Print ONE lane's ranked picks SPLIT per asset class (stocks / [tech stocks] / ETFs / crypto) so a
/// +9400% crypto can't crowd out equities and a basket fund isn't ranked head-to-head with a single
/// company — the best in EACH class surfaces. Class: currency-quoted ticker (`-USD`/`-EUR`) → crypto,
/// else fund name (ETF/UCITS) → ETF, else stock. Currency twins already deduped in `ranked`.
/// `kind` names the lane in each title ("buy candidates" / "growth candidates").
fn print_lane(picks: Vec<(&Quote, f64)>, n: usize, w: &Widths, kind: &str, desc: &str, sectors: &[String], sector_of: &HashMap<String, String>, tuning: &BuyHeuristic, pinned: &HashSet<&str>, owned: &Owned) {
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
    let etf: Vec<_> = etf
        .into_iter()
        .filter(|(quote, s)| keep(quote, *s, etf_min_score, core::sector_matches(&quote.name, sectors)))
        .collect();
    // Title carries the selected sector filter so the table says what it's showing ("all" = no filter).
    // Count shown = how many actually qualified (capped at n); "of {n} max" explains a short table —
    // it's not a quota, that's all that passed the gates + filter.
    let secs = if sectors.is_empty() { "all".to_string() } else { sectors.join(", ") };
    let head = |len: usize| if len >= n { format!("Top {n}") } else { format!("Top {len} of {n} max") };
    // P/E, PEG, ROE, REV-YoY, EPS-YoY, NET% are equity-only; TER/AUM/USE/REPL are ETF-only. Hide the
    // always-"—" columns per class: stocks drop the fund columns, ETFs drop the equity fundamentals,
    // crypto drops both.
    // the ranking explainer prints ONCE here — repeating the same ~340-char paragraph in all three
    // table titles (incl. the crypto sentence over the stocks table) tripled the noise.
    println!("\n{kind} — {desc}");
    print_picks(&format!("{} stocks [sectors: {secs}]:", head(stock.len())), &stock, n, w, pinned, owned, &["ter", "aum", "use", "repl"], tuning);
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
    print_picks(&format!("{} ETFs [sectors: {secs}]:", head(etf.len())), &etf, n, w, pinned, owned, &["pe", "peg", "roe", "rev-yoy", "eps-yoy", "net", "buyback"], tuning);
    // Crypto: NOT min_score-trimmed — show ALL potential growers ranked vs Bitcoin (the base), so BTC
    // itself stays visible even when the overext brake docks its score. Capped at n by print_picks.
    print_picks(&format!("{} crypto (ranked vs Bitcoin, the base):", head(crypto.len())), &crypto, n, w, pinned, owned, &["pe", "peg", "roe", "rev-yoy", "eps-yoy", "net", "ter", "aum", "use", "repl", "div", "buyback", "dom"], tuning);
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
            .then(a.ter_shown().unwrap_or(9.9).partial_cmp(&b.ter_shown().unwrap_or(9.9)).unwrap())
            .then(b.aum_shown().unwrap_or(0.0).partial_cmp(&a.aum_shown().unwrap_or(0.0)).unwrap())
    });
    let mut seen: HashSet<&str> = HashSet::new();
    cores.retain(|q| seen.insert(q.name.as_str())); // one row per fund (VUAA.DE vs VUAA.L), best-ranked kept
    // cap each breadth tier so all three index families show — else the many MSCI World trackers crowd
    // out the S&P 500 and all-world cores. Sort is breadth-major, so a per-tier counter suffices.
    let mut per_tier = [0u8; 3];
    cores.retain(|q| {
        let t = core::hold_breadth_tier(&q.name) as usize;
        per_tier[t] += 1;
        per_tier[t] <= 3
    });
    cores
}

fn print_hold_core(quotes: &[Quote], n: usize, pinned: &HashSet<&str>, owned: &Owned) {
    let cores = hold_core_list(quotes);
    if cores.is_empty() {
        return;
    }
    let per_tier0 = cores.iter().filter(|q| core::hold_breadth_tier(&q.name) == 0).count();
    println!(
        "\nbuy-and-hold CORE — broad one-fund-forever holds (the momentum ranking buries these at 0.0; \
         ranked by breadth → domicile (IE first, withholding) → cheapest TER → largest AUM, NOT advice):"
    );
    // (round 111) leading 1-char cell = the owned-position marker; this list is what a 20yr holder
    // actually buys, so "covered" matters most here. Blank when the overlay is off/empty.
    println!("  {:<1} {:<44} {:<9} {:<9} {:>5} {:>4} {:>6} {:>7} {:<4} {:<4} {:<4}", "", "NAME", "TICKER", "MARKET", "CAGR", "YRS", "TER", "AUM", "USE", "REPL", "DOM");
    let mut any_owned = false;
    for q in cores.iter().take(n) {
        let cagr = q.life_cagr.map_or("n/a".to_string(), |v| format!("{v:+.0}%"));
        let yrs = q.age_years.map_or("—".to_string(), |a| format!("{a:.0}"));
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
    print_lane(picks, n, w, "growth candidates", growth, ctx.sectors, ctx.sector_of, tuning, &pinned_set, ctx.owned);
    // buy-and-hold CORE shortlist: momentum floors broad index funds at 0.0, so surface the
    // one-fund-forever holds re-sorted by hold-suitability (breadth → domicile → TER → AUM) — the
    // right order for a 20yr hold, which the momentum table inverts. Caller-gated (display-only):
    // every `screen` lane that carries cores (the wide run OR `screen etfs`), never `check`. Empty
    // cores early-return inside, so stock/crypto-only screen filters stay silent.
    if ctx.show_hold_core {
        print_hold_core(quotes, 9, &pinned_set, ctx.owned); // up to 3 per breadth tier (all-world / world / S&P 500)
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
        // an explicit --explain TICKER that didn't land a row: say why instead of silently printing nothing
        (None, Some(t)) if !t.is_empty() => Some(format!(
            "\n--explain: {t} is not in the growth ranking (fails a growth gate, isn't EU-buyable, or wasn't scanned)."
        )),
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
        assert_eq!(long_leg_fixed(&q, 0), Some((900.0, 20.0))); // off -> longest leg (20Y)
        assert_eq!(long_leg_fixed(&q, 10), Some((200.0, 10.0))); // pinned -> the 10Y leg
        q.perf[HORIZONS.iter().position(|(l, _)| *l == "10Y").unwrap()] = None; // drop 10Y
        assert_eq!(long_leg_fixed(&q, 10), Some((900.0, 20.0))); // pinned leg absent -> longest leg fallback
    }

    /// (screen columns) `active_columns` resolves config -> ordered ColSpecs (empty = default layout;
    /// whitelist = those keys in order; unknown keys dropped), `fmt_cell` pads+aligns, `col_cell` formats.
    #[test]
    fn screen_columns_config() {
        // empty config -> the canonical default layout (rank..score), and cagr/maxdd are shown by default
        let def = active_columns(&[]);
        assert_eq!(def.first().unwrap().key, "rank");
        assert_eq!(def.last().unwrap().key, "score");
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
        assert_eq!(col_cell("rank", &q, 9.4, "3*"), "3*");
        assert_eq!(col_cell("score", &q, 7.0, ""), "7.0");
        assert_eq!(col_cell("cagr", &q, 0.0, ""), "n/a");
        // peg needs BOTH a P/E and positive growth; stub has neither -> n/a (never panics on the guard)
        assert_eq!(col_cell("peg", &q, 0.0, ""), "n/a");
        // ter is None on a stub (no expense ratio fetched) -> n/a
        assert_eq!(col_cell("ter", &q, 0.0, ""), "n/a");
        // per-class gating: a crypto ('-' ticker) has neither P/E nor expense ratio -> "—" (not applicable),
        // distinct from "n/a" (applies but unfetched).
        let cq = Quote::stub("X-EUR", "€1", "", "Coin");
        assert_eq!(col_cell("pe", &cq, 0.0, ""), "—");
        assert_eq!(col_cell("ter", &cq, 0.0, ""), "—");
        // (USE/REPL) ETF prints its BF tokens; None (off-BF fund) -> n/a; crypto/equity -> — (not a fund)
        let mut eq = Quote::stub("E.DE", "€1", "", "Fund");
        eq.instrument_type = "ETF".to_string();
        assert_eq!(col_cell("use", &eq, 0.0, ""), "n/a");
        assert_eq!(col_cell("repl", &eq, 0.0, ""), "n/a");
        eq.use_of_profits = Some("Acc");
        eq.replication = Some("Swap");
        assert_eq!(col_cell("use", &eq, 0.0, ""), "Acc");
        assert_eq!(col_cell("repl", &eq, 0.0, ""), "Swap");
        assert_eq!(col_cell("use", &cq, 0.0, ""), "—");
        assert_eq!(col_cell("repl", &cq, 0.0, ""), "—");
        // (DOM) same per-class gating as USE/REPL; ETF prints its ISIN-prefix country
        assert_eq!(col_cell("dom", &eq, 0.0, ""), "n/a");
        eq.domicile = Some("IE".to_string());
        assert_eq!(col_cell("dom", &eq, 0.0, ""), "IE");
        assert_eq!(col_cell("dom", &cq, 0.0, ""), "—");
        // (TR-CAGR) display-only lower-bound total return; stub -> n/a
        assert_eq!(col_cell("trcagr", &q, 0.0, ""), "n/a");
        eq.tr_cagr = Some(16.4);
        assert_eq!(col_cell("trcagr", &eq, 0.0, ""), "+16%");
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
        assert_eq!(col_cell("rev-yoy", &st, 0.0, ""), "n/a");
        st.rev_yoy = Some(65.5);
        st.eps_yoy = Some(-12.3);
        st.net_margin_fy = Some(55.6);
        assert_eq!(col_cell("rev-yoy", &st, 0.0, ""), "+65.5%");
        assert_eq!(col_cell("eps-yoy", &st, 0.0, ""), "-12.3%");
        assert_eq!(col_cell("net", &st, 0.0, ""), "55.6");
        assert_eq!(col_cell("rev-yoy", &eq, 0.0, ""), "—"); // ETF
        assert_eq!(col_cell("net", &cq, 0.0, ""), "—"); // crypto
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
        let perf = HORIZONS
            .iter()
            .map(|(l, _)| labels.iter().find(|(pl, _)| pl == l).map(|(_, v)| ("x".to_string(), *v)))
            .collect();
        Quote {
            ticker: "T".into(), price: "€1.00".into(), dip: "-5.0%".into(), drop_pct: drawdown_pct,
            market: "USA".into(), instrument_type: String::new(), head: String::new(), news_block: String::new(), perf,
            name: "n".into(), trend: String::new(), at_ath: false, at_atl: false, mom_pct: None,
            div_eur: Vec::new(), price_eur: None, close_native: None, last_close_date: None, drawdown_pct, intraday: [None; 3],
            // (#20) default a KNOWN turnover so the growth lane's unknown-turnover gate admits test
            // quotes; tests exercising that gate set avg_turnover_eur = None explicitly. €1B -> liq_bonus
            // ln(1e9/1e9)=0, so it stays rank-neutral for the relational score asserts.
            avg_turnover_eur: Some(1e9), volatility_pct: None, below_ma_pct: 0.0, above_ma_pct: 0.0,
            pe_ratio: None,
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
            age_years: None,       // display-only pair; never scored
            life_cagr: None,
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
    let lt = BuyHeuristic { growth_require_lifetime_uptrend: true, ..BuyHeuristic::default() };
    let mut greece = quote(2.0, strong);
    greece.life_cagr = Some(-8.0);
    assert!(growth_score(&greece, &lt).is_none()); // lifetime loser despite strong window legs
    greece.life_cagr = Some(12.0);
    assert!(growth_score(&greece, &lt).is_some()); // whole life positive -> passes
    greece.life_cagr = None; // backtest shape: field absent -> gate leg inert
    assert!(growth_score(&greece, &lt).is_some());
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
    let mut bled_crypto = quote(0.0, &[("1Y", 60.0), ("5Y", -20.0), ("10Y", 500.0)]); // ...but crypto 5Y is noise
    bled_crypto.ticker = "ETH-EUR".into();
    assert!(growth_score(&bled_crypto, &tuning).is_some());
    // (4) NUPL factor: symmetric. euphoria (high NUPL) shrinks <1; capitulation (low NUPL) boosts >1;
    // neutral band / unknown = exactly 1.0.
    assert_eq!(nupl_factor(None, &tuning), 1.0);
    assert!(nupl_factor(Some(0.40), &tuning) == 1.0); // between capitulation (0.25) and euphoria (0.5) -> neutral
    assert!(nupl_factor(Some(0.75), &tuning) < 1.0 && nupl_factor(Some(0.75), &tuning) > tuning.nupl_damp_floor);
    assert!((nupl_factor(Some(1.0), &tuning) - tuning.nupl_damp_floor).abs() < 1e-9); // peak euphoria -> floor
    assert!(nupl_factor(Some(0.0), &tuning) > 1.0); // deep capitulation -> boost
    assert!((nupl_factor(Some(0.0), &tuning) - tuning.nupl_boost_ceiling).abs() < 1e-9); // NUPL 0 -> ceiling
    // (Item 17) crypto_adjust: equities pass through untouched (cfactor ignored); crypto is scaled by the
    // whole-market cfactor (btc_1y None -> btc_relative no-op, so the result isolates the NUPL scale). This
    // is what `size` must apply too, or its crypto sizes diverge from the screen tables.
    let equity = quote(5.0, &[("1Y", 20.0)]); // ticker "T" -> not currency-quoted
    assert_eq!(crypto_adjust(&equity, 10.0, &tuning, 0.5, Some(20.0)), 10.0); // equity: cfactor has no effect
    let mut coin = quote(5.0, &[("1Y", 20.0)]);
    coin.ticker = "BTC-EUR".into();
    assert!((crypto_adjust(&coin, 10.0, &tuning, 0.5, None) - 5.0).abs() < 1e-9); // crypto: 10 * cfactor 0.5

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
    assert!((trust_factor(&quote(20.0, &[("1Y", 30.0), ("5Y", 200.0)]), true) - 1.0).abs() < 1e-9);
    assert_eq!(trust_factor(&quote(20.0, &[("1Y", 30.0)]), true), 0.5); // crypto, only 1Y -> unproven
    assert_eq!(trust_factor(&quote(5.0, &[("5Y", 40.0)]), false), 0.5); // equity, no 10Y -> halved
    assert!((trust_factor(&quote(5.0, &[("10Y", 40.0)]), false) - 1.0).abs() < 1e-9);
    // end-to-end: a 5Y-only crypto (BTC-EUR shape) is admitted to the growth lane and NOT trust-halved
    let mut btc_young = quote(20.0, &[("1Y", 30.0), ("5Y", 200.0)]); // no 10Y leg, like the young EUR pair
    btc_young.ticker = "BTC-EUR".into();
    assert!((trust_factor(&btc_young, true) - 1.0).abs() < 1e-9);
    assert!(growth_score(&btc_young, &tuning).is_some());

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

    // (D) dividend fold — magnitude + cap on the LIVE term (default weight 1.5, cap 6.0); isolate via ScoreParts.dividend.
    // dividend_yield_1y divides -> not bit-exact -> epsilon compare (matches the float-score convention above).
    let mut div_grow = quote(0.0, &[("1Y", 60.0), ("5Y", 200.0), ("10Y", 500.0)]);
    div_grow.price_eur = Some(100.0);
    div_grow.div_eur = vec![Some(5.0)]; // 5% trailing-1Y yield, under the 6% cap
    assert!((score_parts(&div_grow, &tuning).unwrap().dividend - 1.5 * 5.0).abs() < 1e-9); // weight 1.5 × yield 5 = 7.5
    let mut div_cap = div_grow.clone();
    div_cap.div_eur = vec![Some(20.0)]; // 20% (bad-feed / special-div artifact) -> clamped to the 6% cap
    assert!((score_parts(&div_cap, &tuning).unwrap().dividend - 1.5 * 6.0).abs() < 1e-9); // .min(dividend_cap) upper bound = 9.0

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
            owned: &owned, explain: None, show_hold_core: true,
        });
        assert!(tickers.iter().any(|t| t == "AAPL"), "pinned gated name must still surface in the ranking");
        assert!(tickers.len() <= 5);

        // an --explain for a ticker that never ranked -> the "not in the growth ranking" message branch.
        let (miss, _) = render(&quotes, 5, &tuning, &w, RenderCtx {
            nupl: None, sectors: &sectors, sector_of: &sector_of, pinned: &pinned,
            owned: &owned, explain: Some("ZZZZ"), show_hold_core: false,
        });
        assert!(miss.is_some_and(|m| m.contains("not in the growth ranking")));

        let _ = std::fs::remove_file(crate::config::data_path(".folioman_turnover_watch.txt")); // gitignored cache render wrote
    }

    /// (QA) `hold_core_list` breadth-major sort + one-row-per-name dedup + per-tier cap ≤3, and the
    /// `print_hold_core` block over it (owned marker, empty-tier-0 hint, pinned near-miss reason). Pure.
    #[test]
    fn hold_core_list_and_print() {
        let mut quotes = vec![
            core_etf("VWCE.DE", "Vanguard FTSE All-World UCITS ETF", 20e9, 0.22), // tier 0
            core_etf("IWDA.DE", "iShares Core MSCI World UCITS ETF", 60e9, 0.20), // tier 1
            // tier 2 (S&P 500): four DISTINCT names -> the cheapest three survive the per-tier cap.
            core_etf("SPYL.DE", "SPDR S&P 500 UCITS ETF", 8e9, 0.03),
            core_etf("CSPX.DE", "iShares Core S&P 500 UCITS ETF", 70e9, 0.07),
            core_etf("VUAA.DE", "Vanguard S&P 500 UCITS ETF", 10e9, 0.07),
            core_etf("XDWL.DE", "Xtrackers S&P 500 UCITS ETF", 2e9, 0.09), // 4th S&P -> capped out
            core_etf("VUAA.L", "Vanguard S&P 500 UCITS ETF", 5e9, 0.15),  // dup NAME -> deduped
            Quote::stub("AAPL", "€1", "", "Apple Inc."),                   // not a fund -> excluded
        ];

        let cores = hold_core_list(&quotes);
        assert_eq!(core::hold_breadth_tier(&cores[0].name), 0, "all-world sorts first (broadest)");
        assert_eq!(cores.iter().filter(|q| core::hold_breadth_tier(&q.name) == 2).count(), 3, "S&P tier capped at 3");
        assert_eq!(cores.len(), 5, "1 all-world + 1 world + 3 S&P (4th + dup dropped)");
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

    /// (QA) `col_cell` VALUE arms the `screen_columns_config` test leaves at n/a (it uses a bare stub):
    /// the number-formatting columns + the ≥1000% no-decimal path + the unknown-key fallback.
    #[test]
    fn col_cell_value_arms() {
        let mut q = Quote::stub("AAA", "€12.34", "", "Alpha");
        assert_eq!(col_cell("price", &q, 0.0, ""), "€12.34");
        assert_eq!(col_cell("ticker", &q, 0.0, ""), "AAA");
        assert_eq!(col_cell("market", &q, 0.0, ""), q.market.clone());
        q.drawdown_pct = 12.5;
        assert_eq!(col_cell("off-hi", &q, 0.0, ""), "-12.5%");
        assert_eq!(col_cell("upside", &q, 0.0, ""), format!("+{:.1}%", upside_to_high(12.5)));
        q.avg_turnover_eur = Some(3.4e9);
        assert_eq!(col_cell("turnover", &q, 0.0, ""), "€3.4B");
        q.volatility_pct = Some(1.3);
        assert_eq!(col_cell("vol", &q, 0.0, ""), "1.3%");
        q.max_drawdown_pct = 42.0;
        assert_eq!(col_cell("maxdd", &q, 0.0, ""), "-42%");
        q.trend_r2 = 0.87;
        assert_eq!(col_cell("r2", &q, 0.0, ""), "0.87");
        q.above_ma_pct = 61.0;
        assert_eq!(col_cell("abv-ma", &q, 0.0, ""), "+61%");
        q.above_ma_pct = 0.0;
        assert_eq!(col_cell("abv-ma", &q, 0.0, ""), "0%");
        q.age_years = Some(11.0);
        assert_eq!(col_cell("yrs", &q, 0.0, ""), "11");
        q.intraday = [Some(0.12), Some(-0.34), Some(2.0)];
        assert_eq!(col_cell("1h", &q, 0.0, ""), "+0.1%");
        assert_eq!(col_cell("6h", &q, 0.0, ""), "-0.3%");
        assert_eq!(col_cell("12h", &q, 0.0, ""), "+2.0%");
        // the "1d|1w|…|20y" arm via perf_pct; a ≥1000% cell drops the decimal so it still fits its column.
        q.perf = legs(&[("1D", 0.5), ("20Y", 2600.0)]);
        assert_eq!(col_cell("1d", &q, 0.0, ""), "+0.5%");
        assert_eq!(col_cell("20y", &q, 0.0, ""), "+2600%");
        assert_eq!(col_cell("1w", &q, 0.0, ""), "n/a"); // absent leg
        assert_eq!(col_cell("bogus", &q, 0.0, ""), "?"); // unknown key fallback
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
}
