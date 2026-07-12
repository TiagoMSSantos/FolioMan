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
    // (round 68) the ranked top-N tickers as of last run (render's turnover_note slice). The
    // rank-stability note says HOW MANY names moved; this diffs WHICH — the by-name verification
    // every code change was checked with manually, now printed + journaled every run.
    #[serde(default)]
    ranked: Vec<String>,
}

/// (round 68) One-line membership diff, "+JOINED -DROPPED" (order-insensitive — market drift
/// reorders legitimately; membership is the signal). None when nothing changed or there is no
/// baseline (first run / state predating the field). Shared by the CORE and ranked-lane diffs.
fn membership_diff(label: &str, prev_date: &str, prev: &[String], now: &[String]) -> Option<String> {
    if prev.is_empty() {
        return None;
    }
    let joined: Vec<&str> = now.iter().filter(|t| !prev.contains(t)).map(String::as_str).collect();
    let dropped: Vec<&str> = prev.iter().filter(|t| !now.contains(t)).map(String::as_str).collect();
    if joined.is_empty() && dropped.is_empty() {
        return None;
    }
    let fmt = |sign: char, ts: &[&str]| ts.iter().map(|t| format!("{sign}{t}")).collect::<Vec<_>>().join(" ");
    Some(format!(
        "{label} changed since {prev_date}: {}",
        [fmt('+', &joined), fmt('-', &dropped)].join(" ").trim()
    ))
}

const SCREEN_STATE_FILE: &str = ".screen_state.json";

/// (round 56) Permanent record of every alert this command ever printed. Each alert fires exactly
/// once — the state file is rewritten right below it — so a scrolled-past terminal or a piped-away
/// stdout is a LOST fee-hike/closure-risk warning. Append-only, gitignored, never read back here;
/// `grep VUAA .screen_alerts.log` is the fund's full event history.
const ALERT_JOURNAL_FILE: &str = ".screen_alerts.log";

/// Append `lines` to the alert journal, one `YYYY-MM-DD <line>` per row. Best-effort — a journal
/// write failure must never break the screen output it mirrors — but LOUD (round 75): the
/// journal's whole point is that a scrolled-past terminal loses nothing, so losing an entry
/// silently would defeat it. One stderr warning, never abort, never journal the failure itself.
fn journal(date: &str, lines: &[String]) {
    use std::io::Write;
    if lines.is_empty() {
        return;
    }
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ALERT_JOURNAL_FILE)
        .and_then(|mut f| lines.iter().try_for_each(|l| writeln!(f, "{date} {}", l.trim())));
    if appended.is_err() {
        eprintln!("WARNING: could not append to {ALERT_JOURNAL_FILE} — the alerts above are printed but not journaled");
    }
}

/// (round 62) Parse the state file's raw contents into (state, was_corrupt). An ABSENT file
/// (None) is a normal first run — silent. A file that reads but does NOT parse (truncated write,
/// hand edit, a schema change serde can't bridge) was previously swallowed by `.ok()` into the
/// same None, silently resetting every alert baseline (exit review, TER/AUM drift, USE/REPL
/// flips, CORE diff) — a pending alert vanished without a word. The `true` flag lets the caller
/// say so; it never changes what loads.
fn parse_state(raw: Option<String>) -> (Option<ScreenState>, bool) {
    match raw {
        None => (None, false),
        Some(s) => match serde_json::from_str(&s) {
            Ok(st) => (Some(st), false),
            Err(_) => (None, true),
        },
    }
}

/// (round 56) Two printed fund picks overlap when they share at least this many of their top-10
/// holdings — half the book, past coincidence: buying both mostly doubles the same mega-caps.
const HOLDINGS_OVERLAP_MIN: usize = 5;
/// (round 57) A pick is "top-heavy" when its top-10 holdings are at least this fraction of the
/// whole fund — single-name/sector risk concentrated inside the wrapper (a "diversified" semis ETF
/// that is half NVDA+AVGO+TSM), the risk a 20yr survival screen cares about.
const TOP_HEAVY_FRACTION: f64 = 0.40;

/// (round 57) Group the printed picks that hold most of the same top-10 names, one line per group
/// of 2+ instead of round-56's O(n²) pair spam. COMPLETE linkage: a pick joins a group only if it
/// shares ≥ `HOLDINGS_OVERLAP_MIN` holdings with EVERY current member — single-linkage would chain
/// an all-world tracker to a semis ETF through the one megacap (NVDA) they both hold and call 15
/// unrelated funds "one bet" (verified live). Greedy/order-dependent, but every printed group is a
/// true clique whose members all mutually overlap. The line reports the holdings common to the
/// whole group, so its own size states how tight the group is.
fn holdings_overlap_lines(holdings: &std::collections::HashMap<String, Vec<(String, f64)>>) -> Vec<String> {
    let mut tickers: Vec<&String> =
        holdings.keys().filter(|t| holdings[*t].len() >= HOLDINGS_OVERLAP_MIN).collect();
    tickers.sort();
    let syms = |t: &str| -> std::collections::HashSet<&str> {
        holdings[t].iter().map(|(s, _)| s.as_str()).collect()
    };
    let overlap = |a: &str, b: &str| syms(a).intersection(&syms(b)).count();
    let mut groups: Vec<Vec<&str>> = Vec::new();
    for t in &tickers {
        // join the first group this pick overlaps with ALL members of; else start its own
        match groups.iter_mut().find(|g| g.iter().all(|m| overlap(t, m) >= HOLDINGS_OVERLAP_MIN)) {
            Some(g) => g.push(t),
            None => groups.push(vec![t]),
        }
    }
    let mut groups: Vec<Vec<&str>> = groups.into_iter().filter(|g| g.len() >= 2).collect();
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(b[0])));
    groups
        .into_iter()
        .map(|members| {
            let common = members[1..].iter().fold(syms(members[0]), |mut acc, m| {
                let s = syms(m);
                acc.retain(|x| s.contains(x));
                acc
            });
            // (round 58) how much of each member the common names ARE: the same 4 shared megacaps
            // can be ~18% of an S&P 500 tracker but ~45% of a tech-sector fund — the range is the
            // number that says whether buying two members is double-buying. Members whose weights
            // Yahoo omitted (all 0.0) are left out; no weights anywhere -> no suffix.
            let sums: Vec<f64> = members
                .iter()
                .map(|m| holdings[*m].iter().filter(|(s, _)| common.contains(s.as_str())).map(|(_, p)| p).sum())
                .filter(|s: &f64| *s > 0.0)
                .collect();
            let weight = match (
                sums.iter().cloned().fold(f64::INFINITY, f64::min),
                sums.iter().cloned().fold(0.0, f64::max),
            ) {
                _ if sums.is_empty() => String::new(),
                (lo, hi) if (hi * 100.0).round() == (lo * 100.0).round() => {
                    format!(" = {:.0}% of each fund", hi * 100.0)
                }
                (lo, hi) => format!(" = {:.0}-{:.0}% of each fund", lo * 100.0, hi * 100.0),
            };
            let mut common: Vec<&str> = common.into_iter().collect();
            common.sort();
            let more = if common.len() > 4 { format!(" +{}", common.len() - 4) } else { String::new() };
            let lead = if common.len() >= HOLDINGS_OVERLAP_MIN { "effectively one bet" } else { "heavily overlap" };
            format!(
                "  {} picks {lead}: {} (shared top-10: {}{more}{weight})",
                members.len(),
                members.join(" "),
                common[..common.len().min(4)].join(" ")
            )
        })
        .collect()
}

/// (round 57) Printed picks whose top-10 holdings sum to ≥ `TOP_HEAVY_FRACTION` of the fund, heaviest
/// first — concentration inside the wrapper that the fund name hides. Silent when weights are absent
/// (sum 0.0) or the fund is genuinely broad.
fn concentration_lines(holdings: &std::collections::HashMap<String, Vec<(String, f64)>>) -> Vec<String> {
    let mut rows: Vec<(f64, String)> = holdings
        .iter()
        .filter_map(|(t, hs)| {
            let sum: f64 = hs.iter().map(|(_, p)| p).sum();
            (sum >= TOP_HEAVY_FRACTION).then(|| (sum, format!("{t} {:.0}%", sum * 100.0)))
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, l)| l).collect()
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
        } else if a.starts_with('-') {
            // an unrecognized flag must not fall through to the positional tickers: any explicit
            // arg OVERRIDES the whole universe, so a typo'd flag silently turns the full screen
            // into a tiny watchlist-only run. Tickers never START with '-' (BTC-USD has it
            // inside), so this can't reject a real symbol.
            eprintln!("screen: unknown flag {a} (only --explain [TICKER] is supported)");
            std::process::exit(2);
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
    let explicit_args = !args.is_empty();
    let (mut universe, etf_tickers, sector_of) = if explicit_args {
        (args, std::collections::HashSet::new(), std::collections::HashMap::new())
    } else {
        fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, &settings.sectors).await
    };
    // watchlist tickers are ALWAYS fetched so they show in their table for comparison (sector filter or not)
    universe.extend(settings.tickers.iter().cloned());
    universe.sort();
    universe.dedup();

    // per-class counts so an EMPTY class is visible here (a leg that "succeeded" with 0 rows
    // never trips the fetch-failure warnings). Explicit-args runs skip the split — etf_tickers
    // is empty on that path, so the split would mislabel every arg ETF as a stock.
    if explicit_args {
        eprintln!("screen: {} explicit tickers + watchlist", universe.len());
    } else {
        let crypto = universe.iter().filter(|t| crate::picks::is_currency_quoted(t)).count();
        let etfs = universe.iter().filter(|t| etf_tickers.contains(*t)).count();
        eprintln!(
            "screen: {} tickers in universe ({crypto} crypto + {} stocks + {etfs} ETFs)",
            universe.len(),
            universe.len() - crypto - etfs
        );
    }

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
    let mut fund_tilt_uncovered = false; // set below; joins the DEGRADED line at the end of the run
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic.growth_fund_factor).await;
        // the tilt fails SILENT (fetch errors -> factor None -> neutral): with the feed down every
        // stock quietly reverts to price-only ranks. Say what the tilt actually covered so a
        // degraded run is distinguishable from a normal one. Display-only.
        let stocks = quotes.iter().filter(|q| q.instrument_type == "EQUITY").count();
        let covered =
            quotes.iter().filter(|q| q.instrument_type == "EQUITY" && q.fund_factor.is_some()).count();
        eprintln!(
            "screen: fund tilt ({}): {covered} of {stocks} stocks carry the factor (SEC = US filers; uncovered names rank price-only)",
            settings.buy_heuristic.growth_fund_factor
        );
        fund_tilt_uncovered = covered == 0 && stocks > 0;
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
    // rank order kept (Vec) so the fundamentals footer below prints in table order, not hash order
    let target_order: Vec<String> = {
        let is_stock = |q: &&Quote| !crate::picks::is_currency_quoted(&q.ticker) && !crate::picks::quote_is_etf(q);
        let mut ranked: Vec<(&Quote, f64)> = quotes.iter().filter(is_stock)
            .filter_map(|q| growth_score(q, &settings.buy_heuristic).map(|s| (q, s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut order: Vec<String> =
            ranked.iter().take(settings.top_picks).map(|(q, _)| q.ticker.clone()).collect();
        for q in quotes.iter().filter(is_stock).filter(|q| settings.tickers.contains(&q.ticker)) {
            if !order.contains(&q.ticker) {
                order.push(q.ticker.clone());
            }
        }
        order
    };
    let targets: std::collections::HashSet<String> = target_order.iter().cloned().collect();
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
    let (explain_text, ranked_now) = render(&quotes, settings.top_picks, &settings.buy_heuristic, &settings.widths, nupl, &settings.sectors, &sector_of, &settings.tickers, explain.as_deref());

    // (B) fundamentals-trajectory footer for the enriched stock rows: report's annual rollup
    // compacted to one line per name, so "is the growth real or one good year?" doesn't take a
    // `report` run per ticker. DISPLAY-ONLY (every multi-year fundamental measured null as a rank
    // input); names without statements (no SEC/FMP coverage) simply don't print a line.
    {
        let briefs: Vec<(&str, &str)> = target_order
            .iter()
            .filter_map(|tk| {
                let q = quotes.iter().find(|q| &q.ticker == tk)?;
                q.annual_brief.as_deref().map(|b| (tk.as_str(), b))
            })
            .collect();
        if !briefs.is_empty() {
            println!("\nfundamentals trend — displayed stocks, complete fiscal years, oldest→newest (display-only, not scored):");
            for (tk, b) in briefs {
                println!("  {tk:<8} {b}");
            }
        }
    }

    // (X) EXIT review — WATCHLIST names that cleared every growth gate on the previous screen run
    // but fail one now. The backtest's exit probe measures this exact transition: newly-failing
    // names lag kept-passing names by ~14 pts forward — a mild REVIEW signal, not an auto-sell.
    // Watchlist only (the holdings — actionable); universe names churn with fetch batches and
    // would spam. First run (no state file) prints nothing and just seeds the state.
    let watch: Vec<&Quote> = quotes.iter().filter(|q| settings.tickers.contains(&q.ticker)).collect();
    let (prior, state_corrupt) = parse_state(std::fs::read_to_string(SCREEN_STATE_FILE).ok());
    if state_corrupt {
        // (round 62) stderr so a piped stdout still shows it, and journaled so the reset is on the
        // permanent record — the one silent failure mode the alert surface had.
        let warn = format!(
            "WARNING: {SCREEN_STATE_FILE} exists but is unreadable — alert baselines (exit review / fact drift / CORE diff) reset this run, pending alerts suppressed once"
        );
        eprintln!("{warn}");
        journal(&run_date, &[warn]);
    }
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
    // Silent when the previous state predates the field (empty prev — everything would read as new).
    let core_now: Vec<String> =
        crate::picks::hold_core_list(&quotes).iter().take(settings.top_picks).map(|q| q.ticker.clone()).collect();
    if let Some(prev) = &prior {
        if let Some(line) = membership_diff("CORE shortlist", &prev.date, &prev.core, &core_now) {
            println!("\n{line}");
            journal(&run_date, &[line]);
        }
        // (round 68) same net for the ranked tables: joins/dropouts of render's top-N since the
        // last run. Market drift moves this legitimately day-to-day — the value is the by-name
        // record (journal) and the minutes-apart code-change verification the manual table diff did.
        if let Some(line) = membership_diff("Ranking membership", &prev.date, &prev.ranked, &ranked_now) {
            println!("{line}");
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
        core: core_now.clone(), // still needed below by the holdings-overlap pick set
        ranked: ranked_now,
    };
    // (round 69) persistence failure must not be silent: a stuck baseline means every drift alert
    // above re-fires (or a pending one never fires) on the next run with no hint why. Serialize
    // failure no longer writes an empty file (which r62 would then report as CORRUPT). Warn+journal,
    // never abort — one run's worth of stale baseline is annoying, a dead screen is worse.
    let persisted = serde_json::to_string(&state).map(|json| std::fs::write(SCREEN_STATE_FILE, json));
    if !matches!(persisted, Ok(Ok(()))) {
        let warn = format!(
            "WARNING: could not persist {SCREEN_STATE_FILE} — alert baselines stay at the previous run; alerts may repeat next run"
        );
        eprintln!("{warn}");
        journal(&run_date, &[warn]);
    }

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

    // (round 56) holdings-overlap footer: the buy candidates are the ranked ETF rows + the pinned
    // funds + the CORE shortlist, and "different" sector funds routinely hold the same top-10
    // mega-caps — invisible from the names. Yahoo topHoldings, weekly-cached, display-only.
    {
        let is_fund = |q: &&Quote| crate::picks::quote_is_etf(q) && !crate::picks::is_currency_quoted(&q.ticker);
        let mut ranked: Vec<(&Quote, f64)> = quotes
            .iter()
            .filter(is_fund)
            .filter_map(|q| growth_score(q, &settings.buy_heuristic).map(|s| (q, s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        // one venue per fund name (the momentum table dedups the same way), then top rows + pinned + CORE
        let mut seen = std::collections::HashSet::new();
        let mut syms: Vec<String> = ranked
            .iter()
            .filter(|(q, _)| seen.insert(q.name.to_lowercase()))
            .take(settings.top_picks)
            .map(|(q, _)| q.ticker.clone())
            .chain(quotes.iter().filter(is_fund).filter(|q| settings.tickers.contains(&q.ticker)).map(|q| q.ticker.clone()))
            .chain(core_now.iter().cloned())
            .collect();
        syms.sort();
        syms.dedup();
        let holdings = fetch::yahoo_top_holdings(&client, &syms).await;
        let clusters = holdings_overlap_lines(&holdings);
        if !clusters.is_empty() {
            println!("\nHoldings overlap — picks that are effectively the same position (shared top-10 holdings, so buying several ≈ one concentrated bet):");
            for l in &clusters {
                println!("{l}");
            }
        }
        let heavy = concentration_lines(&holdings);
        if !heavy.is_empty() {
            println!("\nTop-heavy picks — top-10 holdings as a share of the whole fund (single-name/sector risk inside the wrapper): {}", heavy.join(", "));
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
    // optional feeds fail SILENTLY into a weaker run (NUPL None = crypto euphoria damping off;
    // empty HICP map = inflation adjustment off despite being enabled) — name them so a degraded
    // run is distinguishable from a normal one. Display-only; the score paths already handle both.
    let mut degraded: Vec<&str> = Vec::new();
    if nupl.is_none() {
        degraded.push("NUPL feed down (crypto euphoria damping off)");
    }
    if eu_infl.as_ref().is_some_and(|m| m.is_empty()) {
        degraded.push("EU HICP feed down (inflation adjustment off)");
    }
    if fund_tilt_uncovered {
        degraded.push("fund tilt feed down (0 stocks carry the factor; stock ranks are price-only)");
    }
    if !degraded.is_empty() {
        eprintln!("screen: DEGRADED — {}", degraded.join("; "));
    }
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

    /// (round 57) overlap grouping is COMPLETE-linkage: A/B/C mutually share ≥5 -> ONE group line
    /// of 3 (not 3 pair lines). D shares only 4 with A and E has too few holdings -> out. G shares 5
    /// with A but only 4 with B, so it must NOT chain into the A/B/C group (the single-linkage bug
    /// that merged 15 unrelated funds live). Shared line is the intersection across the whole group.
    #[test]
    fn holdings_overlap_clusters() {
        let names = |it: &[&str]| -> Vec<(String, f64)> { it.iter().map(|s| (s.to_string(), 0.0)).collect() };
        let syms = |n: usize| -> Vec<(String, f64)> { (0..n).map(|i| (format!("S{i}"), 0.0)).collect() };
        let mut h: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        h.insert("A.L".into(), syms(10)); // S0..S9
        h.insert("B.L".into(), { let mut v = syms(9); v.push(("ONLY-B".into(), 0.0)); v }); // S0..S8 -> ∩A=9, ∩C=5
        h.insert("C.L".into(), { let mut v = syms(5); v.extend((0..5).map(|i| (format!("C{i}"), 0.0))); v }); // S0..S4
        h.insert("D.L".into(), { let mut v: Vec<(String, f64)> = (0..6).map(|i| (format!("D{i}"), 0.0)).collect(); v.extend(syms(4)); v }); // ∩A=4 -> out
        h.insert("E.L".into(), syms(3)); // too few known holdings -> never groups
        h.insert("G.L".into(), names(&["S5", "S6", "S7", "S8", "S9", "G0", "G1", "G2", "G3", "G4"])); // ∩A=5 but ∩B=4 -> must NOT chain
        let lines = holdings_overlap_lines(&h);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "  3 picks effectively one bet: A.L B.L C.L (shared top-10: S0 S1 S2 S3 +1)");
        assert!(!lines[0].contains("G.L"));
        assert!(holdings_overlap_lines(&HashMap::new()).is_empty());
    }

    /// (round 58) the cluster line quantifies the common set per member: same 5 shared names are
    /// 25% of the broad fund but 50% of the sector fund -> range suffix; identical after rounding
    /// collapses to one number; all-zero weights (Yahoo omitted) -> no suffix (previous test).
    #[test]
    fn holdings_overlap_weight_range() {
        let w = |ps: &[f64]| -> Vec<(String, f64)> {
            ps.iter().enumerate().map(|(i, p)| (format!("S{i}"), *p)).collect()
        };
        let mut h: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        h.insert("BROAD.DE".into(), w(&[0.05; 10]));  // common 5 names = 25%
        h.insert("SECTOR.DE".into(), { let mut v = w(&[0.10; 5]); v.extend((0..5).map(|i| (format!("X{i}"), 0.02))); v }); // common = 50%
        let lines = holdings_overlap_lines(&h);
        assert_eq!(lines, vec![
            "  2 picks effectively one bet: BROAD.DE SECTOR.DE (shared top-10: S0 S1 S2 S3 +1 = 25-50% of each fund)".to_string()
        ]);
        // equal after rounding -> single number
        let mut h: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        h.insert("A.DE".into(), w(&[0.06; 10]));
        h.insert("B.DE".into(), w(&[0.06; 10]));
        assert!(holdings_overlap_lines(&h)[0].ends_with("= 60% of each fund)"));
    }

    /// (round 57) concentration: a fund whose top-10 weights sum ≥40% fires (heaviest first), a
    /// broad fund and one with absent weights (sum 0) stay silent.
    #[test]
    fn concentration_semantics() {
        let mut h: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        h.insert("HEAVY.DE".into(), vec![("A".into(), 0.30), ("B".into(), 0.28)]); // 58%
        h.insert("MID.DE".into(), vec![("A".into(), 0.25), ("B".into(), 0.20)]);   // 45%
        h.insert("BROAD.DE".into(), vec![("A".into(), 0.02), ("B".into(), 0.02)]); // 4% -> silent
        h.insert("NOWEIGHT.DE".into(), vec![("A".into(), 0.0), ("B".into(), 0.0)]); // 0% -> silent
        assert_eq!(concentration_lines(&h), vec!["HEAVY.DE 58%".to_string(), "MID.DE 45%".to_string()]);
        assert!(concentration_lines(&HashMap::new()).is_empty());
    }

    /// (round 62) state-file parse fork: absent = normal first run (silent), present-but-garbage =
    /// corrupt (the case that used to silently wipe every alert baseline), valid JSON loads, and a
    /// round-50-era file missing the newer fields still deserializes (the serde(default) guard).
    #[test]
    fn parse_state_corruption_fork() {
        assert!(matches!(parse_state(None), (None, false)));
        let (st, corrupt) = parse_state(Some("{ truncated".into()));
        assert!(st.is_none() && corrupt);
        let valid = serde_json::to_string(&ScreenState {
            date: "2026-07-11".into(),
            passing: vec!["VUAA.DE".into()],
            facts: HashMap::new(),
            fund_meta: HashMap::new(),
            core: Vec::new(),
            ranked: Vec::new(),
        })
        .unwrap();
        let (st, corrupt) = parse_state(Some(valid));
        assert!(!corrupt);
        let st = st.unwrap();
        assert_eq!((st.date.as_str(), st.passing), ("2026-07-11", vec!["VUAA.DE".to_string()]));
        let (old, corrupt) = parse_state(Some(r#"{"date":"2026-01-01","passing":[]}"#.into()));
        assert!(old.is_some() && !corrupt);
    }

    /// (round 68) membership diff: no baseline (empty prev — first run or a state file predating
    /// the field) and no-change are both silent; joins/dropouts print by NAME, order-insensitive
    /// (a pure reorder of the same names is market noise, not a membership event).
    #[test]
    fn membership_diff_semantics() {
        let v = |ts: &[&str]| ts.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert!(membership_diff("X", "d", &[], &v(&["A"])).is_none()); // no baseline -> silent
        assert!(membership_diff("X", "d", &v(&["A", "B"]), &v(&["B", "A"])).is_none()); // reorder only
        assert_eq!(
            membership_diff("Ranking membership", "2026-07-10", &v(&["A", "B"]), &v(&["B", "C"])).unwrap(),
            "Ranking membership changed since 2026-07-10: +C -A"
        );
        assert_eq!(
            membership_diff("CORE shortlist", "d", &v(&["A"]), &v(&[])).unwrap(),
            "CORE shortlist changed since d: -A" // pure dropout: no dangling "+" prefix
        );
    }
}
