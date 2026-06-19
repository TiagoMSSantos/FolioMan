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
            t.sustained_decline_penalty
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
/// that are never a long-term hold, so they can't be "quality on sale".
/// ponytail: cheap name match; tighten the list if a legit name ever trips it.
const LEVERAGED_MARKERS: &[&str] = &["2x", "3x", " short", "inverse", "leverag", "bear ", "ultra"];

fn is_leveraged(name: &str) -> bool {
    let n = name.to_lowercase();
    LEVERAGED_MARKERS.iter().any(|m| n.contains(m))
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

/// Score a quote as a "quality on sale" buy candidate for a multi-DECADE hold, or `None` if it
/// fails a gate. The formula:
///
/// ```text
///   base  = discount × trend_health × momentum + long_reward + cheap_reward + dividend_reward
///   score = base × value × decline × trust
/// ```
///
/// - **discount** — pullback off the ~1Y high, normalized by the asset's OWN volatility and capped
///   (`normal_volatility_pct`, `discount_cap`).
/// - **trend_health** ∈ [0,1] — fades the discount as the long trend's CAGR weakens (`health_zero_cagr`).
/// - **momentum** — weekly bounce/knife multiplier (`momentum_bounce`/`knife`); 1.0 = off (default:
///   weekly timing is noise at a decades horizon).
/// - **long_reward** — (A) reward for the long leg's CAGR (annualized, comparable across spans;
///   `long_trend_weight`, `long_trend_cap`).
/// - **cheap_reward** — (C) reward for sitting below the ~200wk SMA (`cheap_weight`, `cheap_cap`).
/// - **dividend_reward** — (D) reward for trailing yield (`dividend_weight`, `dividend_cap`).
/// - **value** — (E) P/E tilt: cheap lifts, rich dampens, unknown neutral (`ref_pe`).
/// - **decline** — (B) value-trap dock when 1Y & 5Y both deeply negative.
/// - **trust** — halves anything without a 10Y record.
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
    if crypto && q.drawdown_pct < 3.0 {
        return None; // stablecoin peg / crypto at its high -> nothing on sale
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
    let discount =
        normalized_dip(q.drawdown_pct, q.volatility_pct, t.normal_volatility_pct).min(t.discount_cap);
    let health = trend_health(long_cagr, t.health_zero_cagr);
    let momentum = momentum_factor(q, t.momentum_bounce, t.momentum_knife);
    let long_reward = t.long_trend_weight * long_cagr.min(t.long_trend_cap); // (A)
    let cheap_reward = t.cheap_weight * q.below_ma_pct.min(t.cheap_cap); // (C)
    let dividend_reward = t.dividend_weight * dividend_yield_1y(q).min(t.dividend_cap); // (D)

    let base = discount * health * momentum + long_reward + cheap_reward + dividend_reward;
    let value = value_factor(q, t.ref_pe); // (E) cheap lifts, rich dampens, unknown neutral
    let decline = sustained_decline_factor(q, t); // (B) multi-year-bleed dock
    let trust = if perf_pct(q, "10Y").is_none() { 0.5 } else { 1.0 };
    Some(base * value * decline * trust)
}

/// Horizons whose Δ% is shown in the picks table (chronological).
const DIFF_HORIZONS: &[&str] = &["1D", "1W", "1M", "1Y", "5Y", "10Y", "20Y"];

/// Score every quote, dedup currency twins, sort best-first. Shared by the per-class tables.
fn ranked<'a>(qs: &'a [Quote], t: &BuyHeuristic) -> Vec<(&'a Quote, f64)> {
    let scored: Vec<(&Quote, f64)> =
        qs.iter().filter_map(|q| buy_score(q, t).map(|s| (q, s))).collect();
    let mut picks = dedup_currency_twins(scored, t.prefer_eur); // one row per asset (BTC, not BTC-EUR+BTC-USD)
    picks.retain(|(_, s)| *s > 0.0); // drop "cheap but still bleeding": score<=0 = long-term decline beats the discount, not a buy
    picks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first
    picks
}

/// Upside to reclaim the ~1Y high, from the OFF-HI drawdown: a name 46% off its high needs +85%
/// to get back there. NOT a forecast — just the room back to the high. Clamps the asymptote near a
/// total wipeout (-99%+ off is a corpse anyway).
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
            format!("-{:.1}%", q.drawdown_pct), // % below the ~1Y high (real pullback, not 30d)
            format!("+{:.1}%", upside_to_high(q.drawdown_pct)), // room back to that high (NOT a forecast)
            turnover_cell(q.avg_turnover_eur),
            score,
        );
    }
}

/// Print the Top-N buy candidates, SPLIT per asset class (stocks/ETFs vs crypto) so a +9400%
/// crypto can't crowd out equities — the best in EACH class surfaces. Class = currency-quoted
/// ticker (`-USD`/`-EUR`) → crypto, else stocks/ETFs. Currency twins already deduped in `ranked`.
pub fn render(qs: &[Quote], n: usize, t: &BuyHeuristic, w: &Widths, tech: &HashSet<String>) {
    let (crypto, equity): (Vec<_>, Vec<_>) =
        ranked(qs, t).into_iter().partition(|(q, _)| is_currency_quoted(&q.ticker));
    let desc = "quality-on-sale heuristic: a recent low (most below its ~1Y high, OFF-HI) with a \
                still-intact longer-term trend (5Y+ where the history exists — Yahoo's EUR crypto \
                pairs are younger, so 1Y stands in). NOT advice, just a ranking:";
    print_picks(&format!("Top {n} stocks/ETFs buy candidates — {desc}"), &equity, n, w);
    // tech-only subset (S&P 500 GICS Information Technology + Communication Services); skipped when
    // no sector data (e.g. `screen TICKER...` or `check`, which pass an empty set).
    if !tech.is_empty() {
        let tech_picks: Vec<_> = equity.iter().filter(|(q, _)| tech.contains(&q.ticker)).cloned().collect();
        print_picks(&format!("Top {n} tech stocks/ETFs buy candidates — {desc}"), &tech_picks, n, w);
    }
    print_picks(&format!("Top {n} crypto buy candidates — {desc}"), &crypto, n, w);
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
            div_eur: Vec::new(), price_eur: None, drawdown_pct, intraday: [None; 3],
            avg_turnover_eur: None, volatility_pct: None, below_ma_pct: 0.0, pe_ratio: None,
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
    let mut peg = q(0.3, &[("1Y", 3.0), ("5Y", 3.0)]); // stablecoin: drawdown<3% -> nothing on sale
    peg.ticker = "DAI-EUR".into();
    assert!(buy_score(&peg, &t).is_none());
    assert!(is_leveraged("GraniteShares 2x Short NVD") && !is_leveraged("Apple Inc."));
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
}
