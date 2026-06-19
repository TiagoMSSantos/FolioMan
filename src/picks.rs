//! Buy-candidate algorithm: rank the `check` table for "quality on sale".
//! Pure scoring + the table printer, kept together so the whole heuristic lives in one
//! place. **NOT advice** — a transparent ranking of the table, never an auto-buy.

use crate::commands::truncate;
use crate::config::{BuyHeuristic, Widths};
use crate::core::{Quote, HORIZONS};
use std::collections::{HashMap, HashSet};

/// Longest-available trend over a window longer than 2 years (5Y → 10Y → 20Y), i.e. the
/// structural multi-year direction. None if the asset has no >2Y history.
fn long_term_pct(q: &Quote) -> Option<f64> {
    perf_pct(q, "20Y").or_else(|| perf_pct(q, "10Y")).or_else(|| perf_pct(q, "5Y"))
}

/// How intact the long-term trend is, 0..1 — used to scale the on-sale discount so a corpse's deep
/// "discount" can't outrank a healthy name's modest pullback. `zero` (negative) is the long-% at
/// which health hits 0; health reaches 1 at a flat (0%) or rising long trend. Equities clear the
/// >30% structural gate, so this is ~1.0 for them (no-op) — it reshapes the crypto ranking.
fn trend_health(long: f64, zero: f64) -> f64 {
    ((long - zero) / -zero).clamp(0.0, 1.0)
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

/// Score a quote as a "quality on sale" buy candidate, or `None` if it fails a gate. The formula:
///
/// ```text
///   score = discount × trend_health × momentum + long_reward,   then × trust
/// ```
///
/// - **discount**  — how far it's pulled back off its ~1Y high, normalized by the asset's OWN
///   volatility (#1) and capped (`normal_volatility_pct`, `discount_cap`).
/// - **trend_health** ∈ [0,1] — fades the discount as the multi-year trend weakens; kills corpses,
///   ≈1.0 for equities (`min_long_pct_crypto` is the zero-point).
/// - **momentum** — rewards a confirmed bounce, docks a still-falling knife (#2)
///   (`momentum_bounce`, `momentum_knife`).
/// - **long_reward** — a small bonus for a strong >2Y uptrend (`long_trend_weight`, `long_trend_cap`).
/// - **trust** — halves anything without a 10Y record (less-proven uptrend).
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
    // need a multi-year track record; crypto is younger, so fall back to its 1Y leg.
    let long = long_term_pct(q).or_else(|| if crypto { perf_pct(q, "1Y") } else { None })?;
    if crypto && long <= t.min_long_pct_crypto {
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

    // ---- SCORE: discount × trend_health × momentum + long_reward, then × trust ----
    let discount =
        normalized_dip(q.drawdown_pct, q.volatility_pct, t.normal_volatility_pct).min(t.discount_cap);
    let health = trend_health(long, t.min_long_pct_crypto);
    let momentum = momentum_factor(q, t.momentum_bounce, t.momentum_knife);
    let long_reward = t.long_trend_weight * long.min(t.long_trend_cap);

    let score = discount * health * momentum + long_reward;
    let trust = if perf_pct(q, "10Y").is_none() { 0.5 } else { 1.0 };
    Some(score * trust)
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
            avg_turnover_eur: None, volatility_pct: None,
        }
    };
    let t = BuyHeuristic::default(); // normal_vol 2.0, discount_cap 35, momentum 1.3/0.6, long_trend .05/cap 300
    assert_eq!(perf_pct(&q(5.0, &[("1Y", 20.0)]), "1Y"), Some(20.0));
    assert_eq!(perf_pct(&q(5.0, &[]), "1Y"), None);
    // #1 normalized dip: a calm asset's dip is amplified, a wild one's damped, unknown vol = raw
    assert!((normalized_dip(30.0, Some(1.0), 2.0) - 60.0).abs() < 1e-9);
    assert!((normalized_dip(30.0, Some(4.0), 2.0) - 15.0).abs() < 1e-9);
    assert_eq!(normalized_dip(30.0, None, 2.0), 30.0);
    assert_eq!(normalized_dip(30.0, Some(0.0), 2.0), 30.0); // div-by-zero guard
    // base score (vol n/a -> discount = raw drawdown): discount 5 × health 1 × momentum 1 + 0.05*40 = 7
    let base = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    assert!((base - 7.0).abs() < 1e-9);
    // discount caps at 35: drawdown 80 -> 35; + 0.05*40 = 37
    assert!((buy_score(&q(80.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap() - 37.0).abs() < 1e-9);
    // no 10Y history halves the score (uptrend less proven)
    let no10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0)]), &t).unwrap();
    assert!((no10 - base / 2.0).abs() < 1e-9);
    // a deep pullback (discount 35) outranks a rocket at new highs (discount 0) despite a huge 1Y
    let pullback = buy_score(&q(40.0, &[("1Y", 30.0), ("5Y", 50.0), ("10Y", 50.0)]), &t).unwrap();
    let rocket = buy_score(&q(0.0, &[("1Y", 400.0), ("5Y", 500.0), ("10Y", 500.0)]), &t).unwrap();
    assert!(pullback > rocket, "on-sale name must beat the rocket: {pullback} vs {rocket}");
    // #2 momentum: green week off a monthly dip -> bounce ×1.3: discount 5 × 1.3 = 6.5 + 2 = 8.5
    let bounce = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1W", 2.0)]), &t).unwrap();
    assert!((bounce - 8.5).abs() < 1e-9);
    // still falling (red week & day) -> knife ×0.6: discount 5 × 0.6 = 3 + 2 = 5
    let knife = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1W", -2.0), ("1D", -1.0)]), &t).unwrap();
    assert!((knife - 5.0).abs() < 1e-9);
    // only today green -> half the bounce premium (×1.15): discount 5 × 1.15 = 5.75 + 2 = 7.75
    let half = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1D", 1.0)]), &t).unwrap();
    assert!((half - 7.75).abs() < 1e-9);
    // #1 end-to-end: same 30% drawdown, the calm (low-vol) name outranks the wild one
    let mut calm = q(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    calm.volatility_pct = Some(1.0);
    let mut wild = q(30.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    wild.volatility_pct = Some(4.0);
    assert!(buy_score(&calm, &t).unwrap() > buy_score(&wild, &t).unwrap());
    // track-record gate (A): no >2Y leg at all -> excluded (can't be a "5Y+ uptrend")
    assert!(buy_score(&q(5.0, &[("1Y", 20.0)]), &t).is_none());
    // ...but a crypto ticker (currency-quoted) falls back to its 1Y leg -> admitted
    let mut crypto = q(5.0, &[("1Y", 20.0)]);
    crypto.ticker = "BTC-EUR".into();
    assert!(buy_score(&crypto, &t).is_some());
    // per-class knife: a -25% month sinks an equity (knife -15) but not crypto (knife -35)
    assert!(buy_score(&q(5.0, &[("1Y", 20.0), ("5Y", 40.0), ("1M", -25.0)]), &t).is_none());
    let mut knife_crypto = q(5.0, &[("1Y", 20.0), ("1M", -25.0)]);
    knife_crypto.ticker = "ETH-EUR".into();
    assert!(buy_score(&knife_crypto, &t).is_some());
    // structural multi-year gate is equities-only: a negative 5Y leg sinks a stock but not crypto
    // (its -EUR 5Y is peak-anchored, not a real downtrend)
    assert!(buy_score(&q(5.0, &[("1Y", 20.0), ("5Y", -50.0)]), &t).is_none());
    let mut weak5y_crypto = q(5.0, &[("1Y", 20.0), ("5Y", -50.0)]);
    weak5y_crypto.ticker = "LTC-EUR".into();
    assert!(buy_score(&weak5y_crypto, &t).is_some());
    // upside to high: 50% off -> +100% to recover; at the high -> 0; near-total wipeout clamps
    assert!((upside_to_high(50.0) - 100.0).abs() < 1e-9);
    assert_eq!(upside_to_high(0.0), 0.0);
    assert_eq!(upside_to_high(99.5), 9900.0);
    // (A) trend_health: 0 at the corpse threshold, 1 at a flat/rising long trend, partial between
    assert_eq!(trend_health(-70.0, -70.0), 0.0);
    assert_eq!(trend_health(0.0, -70.0), 1.0);
    assert!((trend_health(-21.0, -70.0) - 0.7).abs() < 1e-9);
    // (A) crypto corpse gate: a -95% 5Y leg is a dead coin, excluded even though it clears the floor
    let mut corpse = q(40.0, &[("1Y", -30.0), ("5Y", -95.0)]);
    corpse.ticker = "FIL-EUR".into();
    assert!(buy_score(&corpse, &t).is_none());
    // (A) trend-conditioned discount: same 40% OFF-HI, but the intact coin outranks the bleeding one
    let mut healthy = q(40.0, &[("1Y", -20.0), ("5Y", -20.0)]);
    healthy.ticker = "ETH-EUR".into();
    let mut bleeding = q(40.0, &[("1Y", -20.0), ("5Y", -65.0)]);
    bleeding.ticker = "AAVE-EUR".into();
    assert!(buy_score(&healthy, &t).unwrap() > buy_score(&bleeding, &t).unwrap());
    // (B) stablecoin filter: a peg barely off its high (drawdown < 3%) is excluded
    let mut peg = q(0.3, &[("1Y", 3.0), ("5Y", 3.0)]);
    peg.ticker = "DAI-EUR".into();
    assert!(buy_score(&peg, &t).is_none());
    // leveraged/inverse gate (F): name match excludes a 2x/short product outright
    assert!(is_leveraged("GraniteShares 2x Short NVD") && !is_leveraged("Apple Inc."));
    let mut lev = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    lev.name = "GraniteShares 2x Short NVD".into();
    assert!(buy_score(&lev, &t).is_none());
    // liquidity gate: known turnover below floor excluded; above floor or unknown (None) passes
    let liq_t = BuyHeuristic { min_avg_turnover_eur: 1_000_000.0, ..BuyHeuristic::default() };
    let mut thin = q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    thin.avg_turnover_eur = Some(1_000.0);
    assert!(buy_score(&thin, &liq_t).is_none());
    thin.avg_turnover_eur = Some(5_000_000.0);
    assert!(buy_score(&thin, &liq_t).is_some());
    thin.avg_turnover_eur = None; // unknown turnover not punished
    assert!(buy_score(&thin, &liq_t).is_some());
    // knife gate: deep 1M crash excludes even with a positive year (5Y leg present so A passes)
    assert!(buy_score(&q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("1M", -30.0)]), &t).is_none());
    // structural-decline gate: negative >2Y trend excludes even with a positive year
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", -3.0)]), &t).is_none());
    // ALL long horizons must hold: 5Y up but 10Y down still rejects
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", -5.0)]), &t).is_none());
    // healthy 5Y+ + positive year qualifies
    assert!(buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 80.0), ("20Y", 200.0)]), &t).is_some());
    assert!(buy_score(&q(5.0, &[("1Y", -5.0), ("5Y", 40.0)]), &t).is_none()); // declining year -> excluded
    // per-class 1Y floor: a -40% year excludes an equity but NOT a crypto twin (looser floor)
    assert!(buy_score(&q(30.0, &[("1Y", -40.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).is_none());
    let mut cr = q(30.0, &[("1Y", -40.0), ("5Y", 40.0), ("10Y", 40.0)]);
    cr.ticker = "BTC-USD".into();
    assert!(buy_score(&cr, &t).is_some());
    assert!(buy_score(&q(5.0, &[("5Y", 40.0)]), &t).is_none()); // no 1Y data -> excluded
    assert!(buy_score(&Quote::stub("X", "err", "", "X"), &t).is_none()); // err row -> excluded

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
