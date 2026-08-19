//! `size [TICKERS]` — suggested position sizes for the growth picks: weight ∝ score ÷ volatility
//! (vol-target) inside a per-class risk budget, then capped per name and per sector (`config::Sizing`).
//! READ-ONLY, never trades — you still type the qty into `trade` yourself. No TICKERS -> the watchlist.
//! Names that fail the growth gate are dropped (nothing to size). The rows can sum to under 100 when a
//! cap binds; that remainder is deliberately unallocated, not a rounding error. NOT advice.

use crate::picks::{crypto_adjust, growth_score, nupl_factor, perf_pct, size_weights};
use crate::{config, fetch};

/// (#80) UNGRADEABLE BY THE MUTATION GATE, and skipped so that stays a stated fact rather than a
/// trap — the same call already made for `screen::run` and `check::run`. `run` is reachable from
/// `main.rs` alone, so the only test that exercises it is `size_without_candidates_says_nothing_to_size`
/// in the cli suite — which `ci.yml`'s mutants job deliberately does not run (it grades `--lib --test
/// backtest_fixture`), and which walks the empty path anyway. Graded 2026-08-19 against exactly that
/// selection — 42 mutants over the whole file, of which 11 fall inside `run`, and all 11 MISSED.
///
/// Without the attribute the gate is armed against every future edit here, because `--in-diff` grades
/// whole functions and any one-line change drags all of `run` into scope. The risk-budget rewrite
/// below is precisely such an edit. The alternative was adding `--test cli` to the gate; it was
/// declined on cost, the same way it was for the other two.
#[mutants::skip]
pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let eu_infl = if settings.inflation_adjust.enabled {
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    // same fetch shape as `perf`/`screen`; intraday + news off (sizing needs neither).
    let mut quotes = fetch::quotes(
        &client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, false,
        &settings.anchor_windows, eu_infl.as_ref(), settings.inflation_adjust.score_on_nominal,
    )
    .await;

    // (Item 15) same fund-tilt enrichment `screen`/`check` do, so sizing ranks the names the way `screen`
    // shows them when the tilt is on. Inert when growth_fund_weight == 0 (default) -> no extra fetches.
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic).await;
    }

    // (Item 17) apply the SAME crypto NUPL + BTC-relative adjustments `screen`/`check` do at render time,
    // so crypto sizes rank the way the picks tables showed them, not on the raw price-only score. Whole-
    // market NUPL fetched once; equities pass through crypto_adjust unchanged.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;
    let cfactor = nupl_factor(nupl, &settings.buy_heuristic);
    let btc_1y = quotes.iter().find(|q| q.ticker.starts_with("BTC-")).and_then(|q| perf_pct(q, "1Y"));

    // score with the SAME growth lane `screen` uses; None = the name failed the growth gate -> not sized.
    let mut scored: Vec<_> = quotes
        .iter()
        .filter_map(|q| {
            growth_score(q, &settings.buy_heuristic)
                .map(|s| (q, crypto_adjust(q, s, &settings.buy_heuristic, cfactor, btc_1y)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1)); // best score first; total_cmp: a NaN score must not panic the sort

    if scored.is_empty() {
        println!("No names pass the growth gate — nothing to size. (try `screen` for candidates)");
        return;
    }

    // (Item 6) pass the asset class as the cluster key so a correlated block (all crypto) is one risk
    // bucket, not N independent bets.
    // (P5) `asset_class` rather than the raw `instrument_type` string: that field is Yahoo's free text,
    // so "EQUITY" and "" split the stock class into two buckets that each drew a full share. The sector
    // rides along for the stock-class sector cap.
    let sz = &settings.sizing;
    let weights = size_weights(
        &scored
            .iter()
            .map(|(q, s)| (*s, q.volatility_pct, crate::picks::asset_class(q), q.sector.as_deref()))
            .collect::<Vec<_>>(),
        sz,
    );

    println!("Suggested sizes — weight ∝ score ÷ volatility WITHIN a class budget, then capped (READ-ONLY, NOT advice):");
    println!(
        "  budget {:.0}/{:.0}/{:.0} stock/ETF/crypto (renormalised over the classes present) · max {:.1}%/name · max {:.1}%/sector\n",
        sz.budget_stock, sz.budget_etf, sz.budget_crypto, sz.max_name_pct, sz.max_sector_pct,
    );
    println!("  {:<10} {:>7} {:>7} {:>7}  CAP", "TICKER", "SCORE", "VOL", "SIZE%");
    for ((q, s), (w, cap)) in scored.iter().zip(&weights) {
        println!(
            "  {:<10} {:>7.1} {:>7} {:>6.1}%  {}",
            q.ticker,
            s,
            q.volatility_pct.map_or("n/a".to_string(), |v| format!("{v:.1}%")),
            w,
            cap.map_or(String::new(), |c| format!("cap: {c}")),
        );
    }
    // The total is NOT decoration. A capped basket deliberately does not deploy its whole budget — with
    // too few names in a class the remainder has nowhere to go that respects the caps — and printing 100
    // when the rows sum to 40 would hide exactly the fact these caps exist to surface.
    let total: f64 = weights.iter().map(|(w, _)| w).sum();
    println!("  {:<10} {:>7} {:>7} {:>6.1}%", "TOTAL", "", "", total);
    if total < 99.5 {
        println!(
            "  ({:.1}% unallocated — the caps bind and no name in those classes can take more; add candidates or raise a cap)",
            100.0 - total,
        );
    }

    // Entry-state deploy pace — the same validated line `screen` prints (drawdown deployments beat
    // the index +9.1 pts/yr vs +5.9 near the high in the 12y multi-regime backtest; the multiplier
    // is shared via screen::deploy_line so the two surfaces can never disagree). This REPLACES the
    // old below-200wk-SMA gross haircut, which preached the opposite of that measured edge. A stub
    // ^GSPC quote leaves the state unknown -> deploy_line's honest ×1-base fallback; an unset
    // monthly_deploy_eur (≤0) prints nothing, same as `screen`.
    let spx = fetch::quotes(
        &client, &settings.urls, &fx_cache, &["^GSPC".to_string()], settings.dip_days, settings.high_days,
        false, false, &settings.anchor_windows, eu_infl.as_ref(), settings.inflation_adjust.score_on_nominal,
    )
    .await;
    let off_hi = spx
        .first()
        .filter(|q| q.price != "err" && q.price != "no data")
        .map(|q| q.drawdown_pct);
    if let Some(line) = crate::commands::screen::deploy_line(settings.monthly_deploy_eur, off_hi) {
        println!("{line}");
    }

    // (round 114) allocation gap — what you ACTUALLY hold (Trading212 stocks + Binance crypto,
    // valued at THIS run's EUR prices, so no broker-currency conversion) vs the SIZE% split above.
    // Keyless brokers are silently skipped, same posture as the screen's owned overlay; with no
    // broker key at all the section is absent. Class-prefixed keys so a SOL coin never matches a
    // SOL-lettered stock (round 111 rule). Display-only, NOT advice.
    let mut held: Vec<(String, String, f64)> = Vec::new();
    if let Ok(v) = crate::broker::trading212::owned_positions(&client).await {
        for (t, q) in v {
            held.push((format!("s:{}", crate::picks::t212_base(&t)), t, q));
        }
    }
    if let Ok(v) = crate::broker::binance::owned_amounts(&client).await {
        for (a, q) in v {
            held.push((format!("c:{}", a.to_lowercase()), a, q));
        }
    }
    if !held.is_empty() {
        let sized: Vec<(String, String, Option<f64>, f64)> = scored
            .iter()
            .zip(&weights)
            .map(|((q, _), w)| {
                let key = if crate::picks::is_currency_quoted(&q.ticker) {
                    format!("c:{}", crate::picks::underlying(&q.ticker).to_lowercase())
                } else {
                    format!("s:{}", crate::picks::yahoo_base(&q.ticker))
                };
                (q.ticker.clone(), key, q.price_eur, w.0)
            })
            .collect();
        println!("\nAllocation gap — actual broker weights vs the SIZE% split (matched names only; NOT advice):");
        for line in allocation_gap_lines(&sized, &held) {
            println!("{line}");
        }
    }
}

/// (round 114) The gap table, pure for testing. `sized` = (display ticker, class-prefixed base key,
/// EUR price, suggested SIZE%); `held` = (key, broker label, qty). ACTUAL% is each matched holding's
/// share of the matched holdings' total EUR value — held names the sized list doesn't cover have no
/// EUR price on this run, so they're excluded from the % math and said out loud instead of silently
/// skewing the weights. A held name whose quote lost its EUR price (FX unknown) is flagged, never
/// shown as "not held".
fn allocation_gap_lines(sized: &[(String, String, Option<f64>, f64)], held: &[(String, String, f64)]) -> Vec<String> {
    let mut qty: std::collections::HashMap<&str, f64> = Default::default();
    for (k, _, q) in held {
        *qty.entry(k.as_str()).or_insert(0.0) += q;
    }
    let total: f64 = sized
        .iter()
        .filter_map(|(_, k, p, _)| Some(qty.get(k.as_str())? * (*p)?))
        .sum();
    let mut out = Vec::new();
    if total > 0.0 {
        out.push(format!("  {:<10} {:>10} {:>8} {:>8} {:>8}", "TICKER", "VALUE(EUR)", "ACTUAL%", "SUGG%", "GAP"));
        for (disp, k, p, sugg) in sized {
            let q_held = qty.get(k.as_str()).copied().unwrap_or(0.0);
            if q_held > 0.0 && p.is_none() {
                out.push(format!("  {disp:<10} (held, but no EUR price this run — excluded from the % math)"));
                continue;
            }
            let v = q_held * p.unwrap_or(0.0);
            let actual = v / total * 100.0;
            let gap = actual - sugg;
            let tag = if v == 0.0 {
                "  not held"
            } else if gap > 5.0 {
                "  overweight"
            } else if gap < -5.0 {
                "  underweight"
            } else {
                ""
            };
            out.push(format!("  {disp:<10} {v:>10.0} {actual:>7.1}% {sugg:>7.1}% {gap:>+7.1}%{tag}"));
        }
    } else {
        out.push("  (no held name matches the sized list — no weights to compare)".to_string());
    }
    let covered: std::collections::HashSet<&str> = sized.iter().map(|(_, k, _, _)| k.as_str()).collect();
    for (k, label, q) in held {
        if !covered.contains(k.as_str()) {
            out.push(format!("  (held but not sized: {label} qty {q} — no EUR price this run, excluded from the % math)"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (round 114) Gap-table semantics: matched holdings split ACTUAL% over their EUR total, an
    /// unheld sized name reads "not held" with a negative gap, ±5pt gaps get the weight tag, a held
    /// name with no EUR price is flagged (never "not held"), and held-but-not-sized names are named
    /// outside the % math. No match at all -> the no-weights line.
    #[test]
    fn allocation_gap_semantics() {
        let s = |d: &str, k: &str, p: Option<f64>, w: f64| (d.to_string(), k.to_string(), p, w);
        let h = |k: &str, l: &str, q: f64| (k.to_string(), l.to_string(), q);
        let sized = vec![
            s("AAPL", "s:aapl", Some(10.0), 50.0),
            s("IITU.L", "s:iitu", Some(20.0), 30.0),
            s("BTC-EUR", "c:btc", Some(100.0), 20.0),
            s("NVDA", "s:nvda", None, 0.0),
        ];
        let held = vec![
            h("s:aapl", "AAPL_US_EQ", 10.0),  // 100 EUR -> 50% of 200, gap 0
            h("s:iitu", "IITU_GB_EQ", 5.0),   // 100 EUR -> 50%, gap +20 -> overweight
            h("s:nvda", "NVDA_US_EQ", 3.0),   // held but price None -> flagged
            h("c:sol", "SOL", 2.0),           // held, not sized -> named outside the math
        ];
        let out = allocation_gap_lines(&sized, &held).join("\n");
        assert!(out.contains("AAPL              100    50.0%    50.0%    +0.0%\n"), "{out}");
        assert!(out.contains("IITU.L            100    50.0%    30.0%   +20.0%  overweight"), "{out}");
        assert!(out.contains("BTC-EUR             0     0.0%    20.0%   -20.0%  not held"), "{out}");
        assert!(out.contains("NVDA       (held, but no EUR price this run"), "{out}");
        assert!(out.contains("held but not sized: SOL qty 2"), "{out}");
        // nothing matches -> the honest no-weights line, not a zero-division table
        let none = allocation_gap_lines(&sized[..1], &[h("c:eth", "ETH", 1.0)]).join("\n");
        assert!(none.contains("no held name matches"), "{none}");
    }

    /// (#80) The three tag edges `allocation_gap_semantics` cannot reach, and the priceless row that
    /// nobody holds. Graded 2026-08-19: these four mutants survived that test —
    /// `173:23 > -> >=`, `182:27 > -> >=`, `184:27 < -> ==`, `184:27 < -> <=`.
    ///
    /// The ±5pt band is EXCLUSIVE, so a gap of exactly ±5.0 must carry NO tag; the underweight arm
    /// needs a row that is actually held, because `v == 0.0` claims every unheld row first; and the
    /// "no EUR price" flag keys off the HOLDING, not off the missing price — a name nobody holds
    /// reads "not held" even when its price is unknown.
    ///
    /// Every ACTUAL% here is 25.0 by construction (20 of an 80 total): quarters are exact in binary,
    /// where a tenth is not, and a gap of 5.000000000000002 would silently un-test the boundary.
    #[test]
    fn allocation_gap_tag_edges() {
        let s = |d: &str, k: &str, p: Option<f64>, w: f64| (d.to_string(), k.to_string(), p, w);
        let h = |k: &str, l: &str, q: f64| (k.to_string(), l.to_string(), q);
        let sized = vec![
            s("EDGEHI", "s:hi", Some(20.0), 20.0), // 25.0 - 20 = +5.0 exactly -> NO tag
            s("EDGELO", "s:lo", Some(20.0), 30.0), // 25.0 - 30 = -5.0 exactly -> NO tag
            s("DEEPLO", "s:deep", Some(20.0), 50.0), // 25.0 - 50 = -25.0 -> underweight
            s("BIG", "s:big", Some(20.0), 10.0),   // 25.0 - 10 = +15.0 -> overweight
            s("GHOST", "s:ghost", None, 0.0),      // no price AND unheld -> "not held", not flagged
        ];
        let held = vec![
            h("s:hi", "HI", 1.0),
            h("s:lo", "LO", 1.0),
            h("s:deep", "DEEP", 1.0),
            h("s:big", "BIG", 1.0),
        ];
        let out = allocation_gap_lines(&sized, &held);
        // the tag is whatever trails the last '%' — empty when the gap sits inside the band
        let tag = |name: &str| {
            let line = out
                .iter()
                .find(|l| l.starts_with(&format!("  {name} ")))
                .unwrap_or_else(|| panic!("no row for {name}: {out:?}"));
            line.trim_end().rsplit('%').next().unwrap().trim().to_string()
        };
        assert_eq!(tag("EDGEHI"), "", "+5.0 is the edge and the band excludes it: {out:?}");
        assert_eq!(tag("EDGELO"), "", "-5.0 likewise: {out:?}");
        assert_eq!(tag("DEEPLO"), "underweight", "{out:?}");
        assert_eq!(tag("BIG"), "overweight", "{out:?}");
        assert_eq!(tag("GHOST"), "not held", "unheld outranks unpriced: {out:?}");
    }
}
