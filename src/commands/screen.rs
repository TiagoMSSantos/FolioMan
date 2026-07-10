//! `screen [TICKERS]` — scan a LIVE universe (top-N crypto from CoinGecko + S&P 500
//! constituents, see `fetch::fetch_universe`; `screen TICKER...` overrides) and rank the
//! 20yr+ buy-and-hold growth candidates per asset class (stocks / ETFs / crypto). The
//! growth lane is the only one with a validated forward edge (walk-forward rho +0.26,
//! top-vs-bottom-half +108 pts); the old on-sale / ATH-ATL / fallers / dividend tables
//! were dropped — their selection edge was zero-to-negative for a multi-decade hold.

use crate::core::Quote;
use crate::picks::{eu_buyable, exit_review_lines, gate_failures, growth_near_miss, growth_score, render};
use crate::{config, fetch};

/// (X) Watchlist gate-state persisted between `screen` runs so the EXIT-review footer can flag a
/// holding that PASSED every growth gate last run but fails now — the transition the backtest's
/// exit probe measures (newly-failing names lag kept-passing names by ~14 pts forward). Lives in
/// `.screen_state.json` in the working dir (same local-file pattern as `.fmp_cache`), gitignored.
#[derive(serde::Serialize, serde::Deserialize)]
struct ScreenState {
    date: String,         // YYYY-MM-DD of the run that wrote it
    passing: Vec<String>, // watchlist tickers that cleared every growth gate on that run
    // (round 50) ticker -> (TER %, AUM €) AS SHOWN last run (ter_shown/aum_shown, i.e. incl. the
    // Yahoo fallback) for watchlist + H-flagged funds — feeds the fact-drift alerts below.
    // serde(default) so state files written before this field still load.
    #[serde(default)]
    facts: std::collections::HashMap<String, (Option<f64>, Option<f64>)>,
    // (round 54) ticker -> (USE, REPL) as shown last run, same fund set as `facts`. A share-class
    // conversion (Acc -> Dist: payouts turn taxable yearly) or a replication flip (physical -> Swap:
    // counterparty risk enters) is a structural hold-review event. Parallel field, not a wider
    // `facts` tuple, so round-50..53 state files still deserialize.
    #[serde(default)]
    fund_meta: std::collections::HashMap<String, (Option<String>, Option<String>)>,
    // (round 55) the CORE shortlist tickers as printed last run. The shortlist is what a 20yr
    // holder actually buys, and its composition changes silently (round-53's replication fix put
    // VWRA/SSAC in overnight) — one diff line makes joins/dropouts visible.
    #[serde(default)]
    core: Vec<String>,
}

const SCREEN_STATE_FILE: &str = ".screen_state.json";

/// (round 56) Permanent record of every alert this command ever printed. Each alert fires exactly
/// once — the state file is rewritten right below it — so a scrolled-past terminal or a piped-away
/// stdout is a LOST fee-hike/closure-risk warning. Append-only, gitignored, never read back here;
/// `grep VUAA .screen_alerts.log` is the fund's full event history.
const ALERT_JOURNAL_FILE: &str = ".screen_alerts.log";

/// Append `lines` to the alert journal, one `YYYY-MM-DD <line>` per row. Best-effort: a journal
/// write failure must never break the screen output it mirrors.
fn journal(date: &str, lines: &[String]) {
    use std::io::Write;
    if lines.is_empty() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(ALERT_JOURNAL_FILE) {
        for l in lines {
            let _ = writeln!(f, "{date} {}", l.trim());
        }
    }
}

/// (round 50) TER hike worth flagging: ≥0.05 pp is a real fee decision (Vanguard-scale cuts/hikes
/// move in 0.05+ steps), below is basis-point noise / rounding drift in the source.
const TER_HIKE_PP: f64 = 0.05;
/// (round 50) AUM collapse worth flagging: a halving since last run. Normal market drawdown moves
/// AUM tens of %, not −50% between screens — that scale means redemptions/closure risk.
const AUM_COLLAPSE_FRACTION: f64 = 0.5;

/// (round 50) Fact-drift alerts for a never-sell holder: the two events a 20yr hold must react to
/// are a fee hike (compounds against you forever) and an AUM collapse (fund closure = forced
/// taxable exit). Pure diff of last run's facts vs this run's; None<->Some transitions are silent
/// (data-coverage churn, not fund events). Sorted for stable output.
/// Known limitation (round 55, documented not guarded): `ter_shown` mixes BF and Yahoo-fallback
/// sources, so a fund whose BF row disappears can flip source between runs and the two sources'
/// TER can differ by >0.05 pp — a false "hike" alert. Rare, and the cost is one glance at a
/// review line; a source-tracking guard isn't worth the state it would add.
fn fact_alerts(
    prev: &std::collections::HashMap<String, (Option<f64>, Option<f64>)>,
    cur: &std::collections::HashMap<String, (Option<f64>, Option<f64>)>,
) -> Vec<String> {
    let aum_fmt = |v: f64| if v >= 1e9 { format!("€{:.1}B", v / 1e9) } else { format!("€{:.0}M", v / 1e6) };
    let mut out = Vec::new();
    let mut tickers: Vec<&String> = cur.keys().filter(|t| prev.contains_key(*t)).collect();
    tickers.sort();
    for t in tickers {
        let (pt, pa) = prev[t];
        let (ct, ca) = cur[t];
        if let (Some(p), Some(c)) = (pt, ct) {
            if c - p >= TER_HIKE_PP {
                out.push(format!("ALERT {t}: TER {p:.2}% -> {c:.2}% (fee hike compounds against a hold)"));
            }
        }
        if let (Some(p), Some(c)) = (pa, ca) {
            if p > 0.0 && c <= p * AUM_COLLAPSE_FRACTION {
                out.push(format!("ALERT {t}: AUM {} -> {} (closure risk — a forced taxable exit)", aum_fmt(p), aum_fmt(c)));
            }
        }
    }
    out
}

/// (round 54) Structural drift: a share-class conversion (Acc -> Dist: payouts turn taxable every
/// year) or a replication flip (physical -> Swap: counterparty risk enters the hold) since the last
/// run. Rare, but exactly the kind of quiet fund event a never-sell holder would otherwise learn
/// about years later. Same stance as `fact_alerts`: pure diff, None<->Some silent (coverage churn).
fn meta_alerts(
    prev: &std::collections::HashMap<String, (Option<String>, Option<String>)>,
    cur: &std::collections::HashMap<String, (Option<String>, Option<String>)>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut tickers: Vec<&String> = cur.keys().filter(|t| prev.contains_key(*t)).collect();
    tickers.sort();
    for t in tickers {
        let (pu, pr) = &prev[t];
        let (cu, cr) = &cur[t];
        if let (Some(p), Some(c)) = (pu, cu) {
            if p != c {
                out.push(format!("ALERT {t}: share class {p} -> {c} (distribution policy changed — review the hold)"));
            }
        }
        if let (Some(p), Some(c)) = (pr, cr) {
            if p != c {
                out.push(format!("ALERT {t}: replication {p} -> {c} (fund structure changed — review the hold)"));
            }
        }
    }
    out
}

pub async fn run(args: Vec<String>) {
    let started = std::time::Instant::now();
    let run_date = chrono::Local::now().date_naive().to_string();
    // `--explain [TICKER]`: after the tables, print the SCORE arithmetic for TICKER (a flag with no
    // ticker, or no flag at all, still explains the #1 row — that footer is always on). The named ticker
    // is also added to the scan, so `screen --explain NVDA` ranks + explains just NVDA. Strip the flag
    // out of the positional tickers first, else it gets fetched as a junk symbol.
    let mut explain: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        if a == "--explain" {
            if it.peek().is_some_and(|t| !t.starts_with('-')) {
                let t = it.next().unwrap();
                positional.push(t.clone()); // ensure the target is fetched/scanned
                explain = Some(t);
            } // a bare `--explain` falls through: the default #1 footer covers it
        } else {
            positional.push(a);
        }
    }
    let args = positional;

    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    // live universe (CoinGecko + S&P 500), not a hand-kept list; explicit args override it.
    // etf_tickers = Xetra-ETF source set, used below to fix Yahoo mislabeling them as EQUITY.
    let (mut universe, etf_tickers, sector_of) = if args.is_empty() {
        fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, &settings.sectors).await
    } else {
        (args, std::collections::HashSet::new(), std::collections::HashMap::new())
    };
    // watchlist tickers are ALWAYS fetched so they show in their table for comparison (sector filter or not)
    universe.extend(settings.tickers.iter().cloned());
    universe.sort();
    universe.dedup();

    eprintln!("screen: {} tickers in universe (crypto + S&P 500 + Xetra UCITS ETFs)", universe.len());

    // live EU HICP series to inflation-adjust long-horizon returns, only when enabled
    let eu_infl = if settings.inflation_adjust.enabled {
        eprintln!("screen: fetching EU HICP inflation series…");
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    let mut quotes = fetch::quotes(&client, &settings.urls, &fx_cache, &universe, settings.dip_days, settings.high_days, true, false, &settings.anchor_windows, eu_infl.as_ref()).await; // intraday on (picks shows 1h/6h/12h), news off (screen never prints headlines)
    // anything from the Xetra ETF feed IS an ETF, even if Yahoo tags it EQUITY (structured products
    // like BNP Paribas Issuance) — force it so it can't leak into the stocks table past the sector filter
    for quote in &mut quotes {
        if etf_tickers.contains(&quote.ticker) {
            quote.instrument_type = "ETF".into();
        }
    }
    // (G) route the validated as-of fundamental onto the live quotes so the growth ranking weighs it —
    // only when the tilt is on (weight 0 default = no fetch, no change). Across the ~750-name universe the
    // FMP daily budget caps cold fetches; the rest serve from the disk cache, warming over runs.
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic.growth_fund_factor).await;
    }
    // keep only what an EU-retail investor can actually buy (drops any non-European-listed ETF,
    // Asian-only stock listings) so the growth ranking below is actionable.
    let before = quotes.len();
    let quotes: Vec<Quote> = quotes.into_iter().filter(eu_buyable).collect();
    eprintln!("screen: {} of {before} instruments are EU-buyable (rest filtered out)", quotes.len());

    // (D) drop STALE listings — a name whose newest close bar is older than `stale_days` CALENDAR days is a
    // halted/dead listing frozen at an old price, so its "near-high" range_pct is fake. 0 = off (keep all).
    let fresh_before = quotes.len();
    let (quotes, stale): (Vec<Quote>, Vec<Quote>) = if settings.stale_days > 0 {
        let today = chrono::Local::now().date_naive();
        quotes.into_iter().partition(|q| match q.last_close_date {
            Some(d) => (today - d).num_days() <= settings.stale_days,
            None => true, // no date (shouldn't happen live) -> keep, don't silently drop
        })
    } else {
        (quotes, Vec::new())
    };
    if !stale.is_empty() {
        let today = chrono::Local::now().date_naive();
        let names: Vec<String> = stale.iter()
            .map(|q| format!("{} ({}d)", q.ticker, q.last_close_date.map_or(-1, |d| (today - d).num_days())))
            .collect();
        eprintln!("screen: dropped {} stale listing(s) (>{}d since last close): {}", stale.len(), settings.stale_days, names.join(", "));
    }
    // (round 52) no separate "Scanned N" line — the Data-quality line below carries the same count.

    // Income-statement snapshot (REV-YoY / EPS-YoY / NET%) for the DISPLAYED stock rows only: the
    // ranked top-N plus pinned stocks — enriching all ~500 S&P names cold would burn the shared FMP
    // daily budget for columns nobody sees. Display-only fields, so pre-ranking here to learn WHICH
    // tickers will print cannot change the ranking render() computes. Cache-first: warm runs are free.
    let mut quotes = quotes;
    let targets: std::collections::HashSet<String> = {
        let is_stock = |q: &&Quote| !q.ticker.contains('-') && !crate::picks::quote_is_etf(q);
        let mut ranked: Vec<(&Quote, f64)> = quotes.iter().filter(is_stock)
            .filter_map(|q| growth_score(q, &settings.buy_heuristic).map(|s| (q, s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.iter().take(settings.top_picks).map(|(q, _)| q.ticker.clone())
            .chain(quotes.iter().filter(is_stock).filter(|q| settings.tickers.contains(&q.ticker)).map(|q| q.ticker.clone()))
            .collect()
    };
    fetch::enrich_income_stmt(&client, &settings.urls, &mut quotes, &targets).await;

    // (C) DATA-QUALITY audit: surface the n/a holes (a missing/wrong column) as one number instead of
    // finding them one row at a time. Counts by asset class so a stock with no P/E or an ETF with no TER
    // is visible at a glance.
    let stocks_no_pe = quotes.iter().filter(|q| q.instrument_type.eq_ignore_ascii_case("EQUITY") && q.pe_ratio.is_none()).count();
    let etfs_no_ter = quotes.iter().filter(|q| q.instrument_type.eq_ignore_ascii_case("ETF") && q.ter_shown().is_none()).count();
    println!(
        "Data quality: {} names | {stocks_no_pe} stocks missing P/E | {etfs_no_ter} ETFs missing TER | {} stale dropped (>{}d)",
        quotes.len(), fresh_before - quotes.len(), settings.stale_days
    );

    // Bitcoin NUPL: whole-market crypto sentiment gauge. Fetched BEFORE render so it can damp the
    // crypto rows (high NUPL = euphoric top), then also printed as the footer line.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;

    // the 20yr+ growth ranking, split per asset class (stocks / ETFs / crypto); sectors filters ETFs
    // by fund name (stocks were already sector-filtered before fetch)
    // (round 52) render returns the score-math walkthrough; printed AFTER the actionable footers
    // (gate/exit review, fact drift, near-miss) so alerts aren't buried under arithmetic.
    let explain_text = render(&quotes, settings.top_picks, &settings.buy_heuristic, &settings.widths, nupl, &settings.sectors, &sector_of, &settings.tickers, explain.as_deref());

    // (X) EXIT review — WATCHLIST names that cleared every growth gate on the previous screen run
    // but fail one now. The backtest's exit probe measures this exact transition: newly-failing
    // names lag kept-passing names by ~14 pts forward — a mild REVIEW signal, not an auto-sell.
    // Watchlist only (the holdings — actionable); universe names churn with fetch batches and
    // would spam. First run (no state file) prints nothing and just seeds the state.
    let watch: Vec<&Quote> = quotes.iter().filter(|q| settings.tickers.contains(&q.ticker)).collect();
    let prior: Option<ScreenState> =
        std::fs::read_to_string(SCREEN_STATE_FILE).ok().and_then(|s| serde_json::from_str(&s).ok());
    if let Some(prev) = &prior {
        let lines = exit_review_lines(&prev.passing, &watch, &settings.buy_heuristic, settings.widths.ticker);
        if !lines.is_empty() {
            println!(
                "\nExit review — watchlist names that PASSED all growth gates on {} but fail now\n(measured: newly-failing names lag kept-passing by ~14 pts fwd — review, not auto-sell):",
                prev.date
            );
            for l in &lines {
                println!("{l}");
            }
            journal(&run_date, &lines);
        }
    }
    // (round 50) fact-drift alerts: TER hikes / AUM collapses on watchlist + H-flagged funds since
    // the previous run — the two fund events a never-sell holder must actually react to.
    let tracked: Vec<&Quote> = quotes
        .iter()
        .filter(|q| settings.tickers.contains(&q.ticker) || crate::core::hold_suitable(q))
        .collect();
    let facts: std::collections::HashMap<String, (Option<f64>, Option<f64>)> =
        tracked.iter().map(|q| (q.ticker.clone(), (q.ter_shown(), q.aum_shown()))).collect();
    // (round 54) USE/REPL tracked alongside — structural changes alert below.
    let fund_meta: std::collections::HashMap<String, (Option<String>, Option<String>)> = tracked
        .iter()
        .map(|q| (q.ticker.clone(), (q.use_of_profits.map(String::from), q.replication.map(String::from))))
        .collect();
    if let Some(prev) = &prior {
        let mut alerts = fact_alerts(&prev.facts, &facts);
        alerts.extend(meta_alerts(&prev.fund_meta, &fund_meta));
        if !alerts.is_empty() {
            println!("\nFund-fact drift since {} (fee hikes / closure risk / structure changes — review, not auto-sell):", prev.date);
            for a in &alerts {
                println!("  {a}");
            }
            journal(&run_date, &alerts);
        }
    }

    // (round 55) CORE membership diff: joins/dropouts of the shortlist above since the last run.
    // Silent when the previous state predates the field (empty vec — everything would read as new).
    let core_now: Vec<String> =
        crate::picks::hold_core_list(&quotes).iter().take(settings.top_picks).map(|q| q.ticker.clone()).collect();
    if let Some(prev) = prior.as_ref().filter(|p| !p.core.is_empty()) {
        let joined: Vec<&str> =
            core_now.iter().filter(|t| !prev.core.contains(t)).map(String::as_str).collect();
        let dropped: Vec<&str> =
            prev.core.iter().filter(|t| !core_now.contains(t)).map(String::as_str).collect();
        if !joined.is_empty() || !dropped.is_empty() {
            let fmt = |sign: char, ts: &[&str]| {
                ts.iter().map(|t| format!("{sign}{t}")).collect::<Vec<_>>().join(" ")
            };
            let line = format!(
                "CORE shortlist changed since {}: {}",
                prev.date,
                [fmt('+', &joined), fmt('-', &dropped)].join(" ").trim()
            );
            println!("\n{line}");
            journal(&run_date, &[line]);
        }
    }

    let state = ScreenState {
        date: run_date.clone(),
        passing: watch
            .iter()
            .filter(|q| gate_failures(q, &settings.buy_heuristic).is_some_and(|f| f.is_empty()))
            .map(|q| q.ticker.clone())
            .collect(),
        facts,
        fund_meta,
        core: core_now,
    };
    let _ = std::fs::write(SCREEN_STATE_FILE, serde_json::to_string(&state).unwrap_or_default());

    // (B) NEAR-MISS tail: names the growth lane rejected on EXACTLY one gate — a compounder one notch
    // outside the fence (e.g. a great name 25% off its high failing only the range gate). Makes the silent
    // exclusions visible so a dropped winner can be eyeballed, without loosening any gate. Empty -> nothing.
    // (round 52) pinned names skipped: the gate-review footer above already explains them, and the
    // same ticker printing twice with the same reason read as a bug (VVSM stretch receipt).
    let mut near: Vec<(&Quote, &'static str, String)> = quotes.iter()
        .filter(|q| !settings.tickers.contains(&q.ticker))
        .filter_map(|q| growth_near_miss(q, &settings.buy_heuristic).map(|(g, why)| (q, g, why)))
        .collect();
    if !near.is_empty() {
        // (round 53) within the cagr group (the bulk) closest-to-the-bar first — the `why` string
        // starts with the value, higher = closer to the floor. Other gates mix floor/ceiling
        // directions, one sort rule would be wrong for half of them; they keep ticker order.
        let cagr_val = |gate: &str, why: &str| {
            if gate == "cagr" { why.split('%').next().and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0) } else { 0.0 }
        };
        near.sort_by(|a, b| {
            a.1.cmp(b.1)
                .then_with(|| cagr_val(b.1, &b.2).partial_cmp(&cagr_val(a.1, &a.2)).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| a.0.ticker.cmp(&b.0.ticker))
        });
        println!("\nNear-miss — rejected on ONE growth gate (not ranked above), loosen intentionally if wanted:");
        // (round 54) one row per FUND: the same UCITS fund lists on several venues (L&G Gold Mining
        // printed as both AUCO.L and ETLX.DE) — the momentum tables dedup by underlying, this block
        // didn't. First occurrence wins = the closest venue, thanks to the sort above.
        let mut seen_names = std::collections::HashSet::new();
        for (q, gate, why) in near.iter().filter(|(q, ..)| seen_names.insert(q.name.to_lowercase())) {
            println!("  {:<8} {:<44.44} {:<10} {why}", q.ticker, q.name, gate);
        }
    }

    // (round 52) score-math walkthrough LAST among the analysis blocks: reference material, not an alert.
    if let Some(text) = explain_text {
        println!("{}", text.trim_end());
    }

    if let Some(n) = nupl {
        println!(
            "\nBitcoin NUPL: {n:.3} ({}) — net unrealized profit/loss, whole-market sentiment (damps the crypto tables above). NOT advice.",
            crate::core::nupl_zone(n)
        );
    }

    // Euribor / Certificados de Aforro / inflation — fixed-income + macro baselines to compare the
    // asset tables against
    crate::commands::print_macro_footer(&client, &settings.urls).await;

    // (round 56) run diagnostics on stderr (stdout stays pipeable): the round-51/53 fetch caches
    // are otherwise invisible — the first sign of one silently breaking would be Yahoo 429s.
    let (calls, cache_hits, skips) = fetch::fetch_stats();
    let secs = started.elapsed().as_secs();
    eprintln!(
        "screen: {calls} paced HTTP calls | monthly series: {cache_hits} cache hits, {skips} too-young skips | {}m{:02}s",
        secs / 60,
        secs % 60
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// (round 50) fact-drift alert semantics: a real TER hike and an AUM halving fire; basis-point
    /// wobble, coverage churn (None<->Some) and unknown-both stay silent.
    #[test]
    fn fact_alerts_semantics() {
        let m = |rows: &[(&str, Option<f64>, Option<f64>)]| -> HashMap<String, (Option<f64>, Option<f64>)> {
            rows.iter().map(|(t, ter, aum)| (t.to_string(), (*ter, *aum))).collect()
        };
        let prev = m(&[
            ("VUAA.DE", Some(0.07), Some(28.8e9)),
            ("WOBBLE.DE", Some(0.20), Some(2.0e9)),
            ("SHRINK.DE", Some(0.15), Some(2.1e9)),
            ("CHURN.DE", None, None),
            ("GONE.DE", Some(0.10), Some(1e9)),
        ]);
        let cur = m(&[
            ("VUAA.DE", Some(0.15), Some(28.0e9)),  // +0.08 pp -> fires
            ("WOBBLE.DE", Some(0.22), Some(1.9e9)), // +0.02 pp + normal AUM drift -> silent
            ("SHRINK.DE", Some(0.15), Some(0.8e9)), // -62% AUM -> fires
            ("CHURN.DE", Some(0.10), Some(5e9)),    // None -> Some = coverage, silent
            ("NEW.DE", Some(0.30), Some(1e9)),      // not in prev -> silent
        ]);
        let alerts = fact_alerts(&prev, &cur);
        assert_eq!(alerts, vec![
            "ALERT SHRINK.DE: AUM €2.1B -> €800M (closure risk — a forced taxable exit)".to_string(),
            "ALERT VUAA.DE: TER 0.07% -> 0.15% (fee hike compounds against a hold)".to_string(),
        ]);
        assert!(fact_alerts(&prev, &prev).is_empty()); // no drift -> no alerts
    }

    /// (round 54) structural drift: a share-class conversion and a replication flip fire; unchanged
    /// values, coverage churn (None<->Some) and unknown tickers stay silent.
    #[test]
    fn meta_alerts_semantics() {
        let m = |rows: &[(&str, Option<&str>, Option<&str>)]| -> HashMap<String, (Option<String>, Option<String>)> {
            rows.iter().map(|(t, u, r)| (t.to_string(), (u.map(String::from), r.map(String::from)))).collect()
        };
        let prev = m(&[
            ("CONV.DE", Some("Acc"), Some("Full")),
            ("FLIP.DE", Some("Acc"), Some("Opt")),
            ("SAME.DE", Some("Acc"), Some("Full")),
            ("CHURN.DE", None, None),
        ]);
        let cur = m(&[
            ("CONV.DE", Some("Dist"), Some("Full")), // share-class conversion -> fires
            ("FLIP.DE", Some("Acc"), Some("Swap")),  // replication flip -> fires
            ("SAME.DE", Some("Acc"), Some("Full")),  // unchanged -> silent
            ("CHURN.DE", Some("Acc"), Some("Full")), // None -> Some = coverage, silent
            ("NEW.DE", Some("Dist"), Some("Swap")),  // not in prev -> silent
        ]);
        assert_eq!(meta_alerts(&prev, &cur), vec![
            "ALERT CONV.DE: share class Acc -> Dist (distribution policy changed — review the hold)".to_string(),
            "ALERT FLIP.DE: replication Opt -> Swap (fund structure changed — review the hold)".to_string(),
        ]);
        assert!(meta_alerts(&prev, &prev).is_empty());
    }
}
