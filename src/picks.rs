//! Buy-candidate algorithm: rank the `check` table for "quality on sale".
//! Pure scoring + the table printer, kept together so the whole heuristic lives in one
//! place. **NOT advice** — a transparent ranking of the table, never an auto-buy.

use crate::commands::truncate;
use crate::config::{BuyHeuristic, Widths};
use crate::core::{Quote, HORIZONS};
use std::collections::HashMap;

/// Longest-available trend over a window longer than 2 years (5Y → 10Y → 20Y), i.e. the
/// structural multi-year direction. None if the asset has no >2Y history.
fn long_term_pct(q: &Quote) -> Option<f64> {
    perf_pct(q, "20Y").or_else(|| perf_pct(q, "10Y")).or_else(|| perf_pct(q, "5Y"))
}

/// Recovery signal: rewards a name that has **pulled back this month but is turning back up**
/// — a bottoming setup, not a falling knife. Full `weight` if the week is green while the
/// month is red (bounce confirmed), half if only today is green, 0 otherwise (still falling,
/// or never pulled back). Replaces the old decline-duration bonus, which perversely rewarded
/// a name the *longer* it kept falling — the opposite of "about to grow".
fn recovery_bonus(q: &Quote, weight: f64) -> f64 {
    if perf_pct(q, "1M").unwrap_or(0.0) >= 0.0 {
        return 0.0; // not pulled back on the month -> no recovery setup to reward
    }
    if perf_pct(q, "1W").unwrap_or(0.0) > 0.0 {
        weight // up on the week off a monthly dip -> bounce confirmed
    } else if perf_pct(q, "1D").unwrap_or(0.0) > 0.0 {
        weight * 0.5 // only today green -> a fresh, unconfirmed turn
    } else {
        0.0 // still falling -> no reward (don't catch the knife)
    }
}

/// Fresh-dip signal: rewards a name **falling right now (this week)**, not one whose dip is a
/// stale month-old move that already bottomed. Magnitude of the 1W drop (capped `cap`) ×
/// `weight`, doubled when today is also red (drop still in progress = freshest entry) vs halved
/// when today turned green (handled by `recovery_bonus` instead). 0 if up on the week. This
/// surfaces names with a recent 1D/1W decline alongside the slower OFF-HI / 1M dips.
/// ponytail: linear in the 1W drop; swap for a curve only if the ranking needs it.
fn fresh_dip_bonus(q: &Quote, weight: f64, cap: f64) -> f64 {
    let wk = perf_pct(q, "1W").unwrap_or(0.0);
    if wk >= 0.0 {
        return 0.0; // not falling this week -> no fresh dip (a bounce is recovery_bonus's job)
    }
    let mag = (-wk).min(cap); // steeper recent drop = fresher entry, capped so a crash can't run away
    let accel = if perf_pct(q, "1D").unwrap_or(0.0) < 0.0 { 1.0 } else { 0.5 }; // still red today = freshest
    weight * mag * accel
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

/// Heuristic "quality on sale" buy score. `None` = excluded as a candidate. All gates and
/// caps come from `BuyHeuristic` (settings.yaml `buy_heuristic:`) so they're tunable.
///
/// Gates (all must pass): **not a leveraged/inverse product** (name match — those decay, never
/// a long-term hold); has a **multi-year (≥5Y) track record** (no 5Y/10Y/20Y leg at all → it
/// can't be a "5Y+ uptrend", so it's out — this kills brand-new tickers and 2x/short ETFs);
/// has a 1-year history with 1Y % above its class floor (a **floor**: equities use `min_1y_pct`,
/// crypto/FX the looser `min_1y_pct_crypto` since they're far more volatile — keep mildly
/// negative to allow a real *pullback* but reject a name deep in a 1-year *downtrend*); **not crashing now**
/// (1M % above `max_1m_drop_pct` — a fresh crater is a knife); and **structurally healthy** —
/// *every* multi-year horizon it has (5Y, 10Y, 20Y) above `min_long_pct` (one weak long leg
/// rejects it).
///
/// Score rewards a **recent low inside an intact long-term uptrend** — "quality on sale".
/// The dominant term is the pullback off the recent high (`drawdown_pct`, the % below the
/// high over `settings.high_days` ≈ 1Y, weighted `on_sale_weight`, capped `on_sale_cap` — keep
/// the cap modest so a moderate pullback maxes it and a 60%+ collapse, likely broken, doesn't
/// dominate). Then a *small* 1Y-momentum term (`y1_weight × 1Y%`, capped low — momentum is a
/// gate, not the prize) + proven multi-year trend (`long_weight × >2Y%`, capped `long_cap`) +
/// a **recovery bonus** (`recovery_weight`, paid only when it's pulling back yet turning back
/// up — a bottoming setup, not a knife) + a **fresh-dip bonus** (`fresh_dip_weight × 1W drop`,
/// capped `fresh_dip_cap`, paid when it's falling *this week* so a recent dip ranks above a
/// stale month-old one). Finally the score is **halved if the asset has no 10Y
/// history**: the thesis is "intact long-term uptrend", untrustworthy without a decade of data.
/// Higher = more interesting. **NOT advice** — a ranking of the table, not a forecast.
pub fn buy_score(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
    if is_leveraged(&q.name) {
        return None; // leveraged/inverse decay product -> never a long-term hold
    }
    // liquidity gate: a thin/obscure name (tiny daily turnover) is a risky "opportunity".
    // Only gates when turnover is known; unknown (None) passes (don't punish missing volume data).
    if q.avg_turnover_eur.map_or(false, |v| v < t.min_avg_turnover_eur) {
        return None;
    }
    // track-record gate: need a ≥5Y leg. Crypto is mostly <5yr old -> fall back to its 1Y leg
    // (else nearly every coin is rejected); equities still require the real 5Y+ record.
    let long = long_term_pct(q)
        .or_else(|| if is_currency_quoted(&q.ticker) { perf_pct(q, "1Y") } else { None })?;
    let y1 = perf_pct(q, "1Y")?;
    // per-class 1Y floor: crypto/FX are far more volatile -> a looser floor (else every dip is a knife)
    let floor = if is_currency_quoted(&q.ticker) { t.min_1y_pct_crypto } else { t.min_1y_pct };
    if y1 <= floor {
        return None; // 1Y floor: a deep 1-year downtrend is not a pullback
    }
    // per-class knife: crypto swings harder, so a looser monthly-drop floor (else every alt is a knife)
    let knife = if is_currency_quoted(&q.ticker) { t.max_1m_drop_pct_crypto } else { t.max_1m_drop_pct };
    if perf_pct(q, "1M").unwrap_or(0.0) <= knife {
        return None; // crashing this month -> falling knife, not "on sale"
    }
    // every long horizon it has must be up: a dip is only a buy if 5Y AND 10Y AND 20Y hold.
    // Equities only: Yahoo -EUR crypto pairs mostly start near the 2021 peak, so their "5Y" leg
    // is peak-anchored and routinely negative even for healthy coins -> meaningless gate. Crypto
    // relies on the 1Y floor + knife instead.
    if !is_currency_quoted(&q.ticker) {
        for label in ["5Y", "10Y", "20Y"] {
            if perf_pct(q, label).map_or(false, |p| p <= t.min_long_pct) {
                return None; // a negative multi-year leg -> structural loser, not a dip
            }
        }
    }
    let on_sale = q.drawdown_pct.min(t.on_sale_cap); // % off the ~1Y high — the real discount
    let score = t.on_sale_weight * on_sale
        + t.y1_weight * y1.min(t.y1_cap)
        + t.long_weight * long.min(t.long_cap)
        + recovery_bonus(q, t.recovery_weight)
        + fresh_dip_bonus(q, t.fresh_dip_weight, t.fresh_dip_cap);
    // no 10Y track record -> can't trust the "intact long-term uptrend" thesis -> halve.
    let penalty = if perf_pct(q, "10Y").is_none() { 0.5 } else { 1.0 };
    Some(score * penalty)
}

/// Horizons whose Δ% is shown in the picks table (chronological).
const DIFF_HORIZONS: &[&str] = &["1D", "1W", "1M", "1Y", "5Y", "10Y", "20Y"];

/// Score every quote, dedup currency twins, sort best-first. Shared by the per-class tables.
fn ranked<'a>(qs: &'a [Quote], t: &BuyHeuristic) -> Vec<(&'a Quote, f64)> {
    let scored: Vec<(&Quote, f64)> =
        qs.iter().filter_map(|q| buy_score(q, t).map(|s| (q, s))).collect();
    let mut picks = dedup_currency_twins(scored, t.prefer_eur); // one row per asset (BTC, not BTC-EUR+BTC-USD)
    picks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first
    picks
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
    let (nw, tw, mw, pw) = (w.name, w.ticker, w.market, w.price);
    println!("\n{title}");
    if picks.is_empty() {
        println!("  (none pass the gates)");
        return;
    }
    let diff_hdr = DIFF_HORIZONS.iter().map(|l| format!("{:>8}", l)).collect::<Vec<_>>().join(" ");
    let cell = |o: Option<f64>| o.map_or("n/a".to_string(), |v| format!("{:+.1}%", v));
    println!(
        "  {:<4} {:<nw$} {:<tw$} {:<mw$} {:>pw$} {:>7} {:>7} {:>7} {diff_hdr} {:>7} {:>10} {:>8}",
        "RANK", truncate("NAME", nw), truncate("TICKER", tw), truncate("MARKET", mw), "PRICE(EUR)",
        "1H", "6H", "12H", "OFF-HI", "TURNOVER", "SCORE"
    );
    for (i, (q, score)) in picks.iter().take(n).enumerate() {
        let diffs = DIFF_HORIZONS
            .iter()
            .map(|l| format!("{:>8}", perf_pct(q, l).map_or("n/a".to_string(), |v| format!("{:+.1}%", v))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  {:<4} {:<nw$} {:<tw$} {:<mw$} {:>pw$} {:>7} {:>7} {:>7} {diffs} {:>7} {:>10} {:>8.2}",
            i + 1,
            truncate(&q.name, nw),
            truncate(&q.ticker, tw),
            truncate(&q.market, mw),
            q.price,
            cell(q.intraday[0]),
            cell(q.intraday[1]),
            cell(q.intraday[2]),
            format!("-{:.1}%", q.drawdown_pct), // % below the ~1Y high (real pullback, not 30d)
            turnover_cell(q.avg_turnover_eur),
            score,
        );
    }
}

/// Print the Top-N buy candidates, SPLIT per asset class (stocks/ETFs vs crypto) so a +9400%
/// crypto can't crowd out equities — the best in EACH class surfaces. Class = currency-quoted
/// ticker (`-USD`/`-EUR`) → crypto, else stocks/ETFs. Currency twins already deduped in `ranked`.
pub fn render(qs: &[Quote], n: usize, t: &BuyHeuristic, w: &Widths) {
    let (crypto, equity): (Vec<_>, Vec<_>) =
        ranked(qs, t).into_iter().partition(|(q, _)| is_currency_quoted(&q.ticker));
    let desc = "quality-on-sale heuristic: a recent low (most below its ~1Y high, OFF-HI) inside \
                an intact long-term (5Y+) uptrend. NOT advice, just a ranking:";
    print_picks(&format!("Top {n} stocks/ETFs buy candidates — {desc}"), &equity, n, w);
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
            avg_turnover_eur: None,
        }
    };
    let t = BuyHeuristic::default(); // on_sale_w 1.0/cap 35, y1_w .05/cap 50, long_w .05/cap 300, recovery_w 1.0
    assert_eq!(perf_pct(&q(5.0, &[("1Y", 20.0)]), "1Y"), Some(20.0));
    assert_eq!(perf_pct(&q(5.0, &[]), "1Y"), None);
    // score asserts carry a 10Y leg so the no-10Y halving penalty stays 1.0 (clean numbers).
    // score = 1.0*off-hi(5) + 0.05*20(1Y) + 0.05*20(long=10Y) = 5 + 1 + 1 = 7
    assert!((buy_score(&q(5.0, &[("1Y", 20.0), ("10Y", 20.0)]), &t).unwrap() - 7.0).abs() < 1e-9);
    // off-hi caps at 35: drawdown 80 -> 35; + 0.05*10 + 0.05*10 = 35 + 0.5 + 0.5 = 36
    assert!((buy_score(&q(80.0, &[("1Y", 10.0), ("10Y", 10.0)]), &t).unwrap() - 36.0).abs() < 1e-9);
    // missing 10Y history halves the score (long-term uptrend unverifiable)
    let with10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    let no10 = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0)]), &t).unwrap();
    assert!((no10 - with10 / 2.0).abs() < 1e-9);
    // a deep pullback (off-hi 40->cap 35) outranks a rocket at new highs (off-hi 0) despite weaker 1Y
    let pullback = buy_score(&q(40.0, &[("1Y", 30.0), ("5Y", 50.0)]), &t).unwrap();
    let rocket = buy_score(&q(0.0, &[("1Y", 400.0), ("5Y", 500.0)]), &t).unwrap();
    assert!(pullback > rocket, "on-sale name must beat the rocket: {pullback} vs {rocket}");
    // long-term term uses >2Y when present: off-hi(5) + 0.05*10 + 0.05*40(10Y) = 5+0.5+2 = 7.5
    let base = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]), &t).unwrap();
    assert!((base - 7.5).abs() < 1e-9);
    // recovery bonus (weight 1.0): pulled back on the month + green week -> full +1.0
    let bounce = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1W", 2.0)]), &t).unwrap();
    assert!((bounce - base - 1.0).abs() < 1e-9);
    // still falling on the week (no green) -> no recovery bonus, but a fresh-dip bonus instead:
    // 1W -2.0 -> mag 2.0, today not red (1D n/a) -> accel 0.5 -> 0.3*2.0*0.5 = 0.3 over base
    let falling = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1W", -2.0)]), &t).unwrap();
    assert!((falling - base - 0.3).abs() < 1e-9);
    // fresh dip, still red today -> full accel: 1W -3 (mag 3) + 1D -1 -> 0.3*3*1.0 = 0.9 over base
    let fresh_dip = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1W", -3.0), ("1D", -1.0)]), &t).unwrap();
    assert!((fresh_dip - base - 0.9).abs() < 1e-9);
    // green week -> no fresh-dip bonus (a bounce is recovery_bonus's job, not fresh-dip's)
    assert_eq!(fresh_dip_bonus(&q(5.0, &[("1W", 2.0)]), 0.3, 15.0), 0.0);
    // fresh-dip cap: a -50% week is capped at 15 -> 0.3*15*1.0 = 4.5, not 0.3*50
    assert!((fresh_dip_bonus(&q(5.0, &[("1W", -50.0), ("1D", -1.0)]), 0.3, 15.0) - 4.5).abs() < 1e-9);
    // only today green off a monthly dip -> half credit
    let fresh = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1D", 1.0)]), &t).unwrap();
    assert!((fresh - base - 0.5).abs() < 1e-9);
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
