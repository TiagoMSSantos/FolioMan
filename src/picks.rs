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
/// up — a bottoming setup, not a knife). Finally the score is **halved if the asset has no 10Y
/// history**: the thesis is "intact long-term uptrend", untrustworthy without a decade of data.
/// Higher = more interesting. **NOT advice** — a ranking of the table, not a forecast.
pub fn buy_score(q: &Quote, t: &BuyHeuristic) -> Option<f64> {
    if is_leveraged(&q.name) {
        return None; // leveraged/inverse decay product -> never a long-term hold
    }
    let long = long_term_pct(q)?; // track-record gate: no ≥5Y leg -> not a "5Y+ uptrend"
    let y1 = perf_pct(q, "1Y")?;
    // per-class 1Y floor: crypto/FX are far more volatile -> a looser floor (else every dip is a knife)
    let floor = if is_currency_quoted(&q.ticker) { t.min_1y_pct_crypto } else { t.min_1y_pct };
    if y1 <= floor {
        return None; // 1Y floor: a deep 1-year downtrend is not a pullback
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
    let on_sale = q.drawdown_pct.min(t.on_sale_cap); // % off the ~1Y high — the real discount
    let score = t.on_sale_weight * on_sale
        + t.y1_weight * y1.min(t.y1_cap)
        + t.long_weight * long.min(t.long_cap)
        + recovery_bonus(q, t.recovery_weight);
    // no 10Y track record -> can't trust the "intact long-term uptrend" thesis -> halve.
    let penalty = if perf_pct(q, "10Y").is_none() { 0.5 } else { 1.0 };
    Some(score * penalty)
}

/// Horizons whose Δ% is shown in the picks table (chronological).
const DIFF_HORIZONS: &[&str] = &["1D", "1W", "1M", "1Y", "5Y", "10Y", "20Y"];

/// Print the Top-N buy candidates derived from the already-fetched quotes (no network).
pub fn render(qs: &[Quote], n: usize, t: &BuyHeuristic, w: &Widths) {
    let (nw, tw) = (w.name, w.ticker);
    let scored: Vec<(&Quote, f64)> =
        qs.iter().filter_map(|q| buy_score(q, t).map(|s| (q, s))).collect();
    let mut picks = dedup_currency_twins(scored, t.prefer_eur); // one row per asset (e.g. BTC, not BTC-EUR+BTC-USD)
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
    // still falling on the week (no green) -> no recovery bonus (don't catch the knife)
    let falling = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1W", -2.0)]), &t).unwrap();
    assert!((falling - base).abs() < 1e-9);
    // only today green off a monthly dip -> half credit
    let fresh = buy_score(&q(5.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0), ("1M", -5.0), ("1D", 1.0)]), &t).unwrap();
    assert!((fresh - base - 0.5).abs() < 1e-9);
    // track-record gate (A): no >2Y leg at all -> excluded (can't be a "5Y+ uptrend")
    assert!(buy_score(&q(5.0, &[("1Y", 20.0)]), &t).is_none());
    // leveraged/inverse gate (F): name match excludes a 2x/short product outright
    assert!(is_leveraged("GraniteShares 2x Short NVD") && !is_leveraged("Apple Inc."));
    let mut lev = q(40.0, &[("1Y", 10.0), ("5Y", 40.0), ("10Y", 40.0)]);
    lev.name = "GraniteShares 2x Short NVD".into();
    assert!(buy_score(&lev, &t).is_none());
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
