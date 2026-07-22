//! `screen [TICKERS]` — scan a LIVE universe (top-N crypto from CoinGecko + S&P 500
//! constituents, see `fetch::fetch_universe`; `screen TICKER...` overrides) and rank the
//! 20yr+ buy-and-hold growth candidates per asset class (stocks / ETFs / crypto). The
//! growth lane is the only one with a validated forward edge (walk-forward rho +0.26,
//! top-vs-bottom-half +108 pts); the old on-sale / ATH-ATL / fallers / dividend tables
//! were dropped — their selection edge was zero-to-negative for a multi-decade hold.

use crate::core::Quote;
use crate::picks::{eu_buyable, exit_review_lines, gate_failures, growth_near_miss, growth_score, render, RenderCtx};
use crate::{config, core, fetch, picks};

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
        .open(crate::config::data_path(ALERT_JOURNAL_FILE))
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

/// (sector tilt) One line per fund that carries sector data: top-2 sectors + equity/bond split,
/// heaviest top sector first — the "99% one sector" risk the shared mega-cap holdings hide.
/// Weights arrive as fractions (0.99 = 99%); the bond leg prints only when it actually exists
/// (≥0.5% — equity ETFs report bond 0). Funds without sector data simply don't print.
fn sector_tilt_lines(mix: &std::collections::HashMap<String, fetch::FundMix>) -> Vec<String> {
    let mut rows: Vec<(&String, &Vec<(String, f64)>, Option<(f64, f64)>)> = mix
        .iter()
        .filter(|(_, (sectors, _, _))| !sectors.is_empty())
        .map(|(t, (sectors, sb, _))| (t, sectors, *sb))
        .collect();
    rows.sort_by(|a, b| b.1[0].1.total_cmp(&a.1[0].1).then_with(|| a.0.cmp(b.0)));
    rows.into_iter()
        .map(|(t, sectors, sb)| {
            let tops = sectors
                .iter()
                .take(2)
                .map(|(name, w)| format!("{name} {:.0}%", 100.0 * w))
                .collect::<Vec<_>>()
                .join(" · ");
            let split = match sb {
                Some((stock, bond)) if bond >= 0.005 => {
                    format!("  (equity {:.0}% / bond {:.0}%)", 100.0 * stock, 100.0 * bond)
                }
                _ => String::new(),
            };
            format!("  {t:<10} {tops}{split}")
        })
        .collect()
}

/// (fund valuation) One wrapped line of fund equity-book P/Es, CHEAPEST first — the number behind
/// the header's "quality pricey because it keeps winning". Values arrive already inverted from
/// `parse_fund_pe` (Yahoo serves reciprocals — see the fetch-side pin). Funds without the datum
/// stay silent; `None` when nobody has it.
fn fund_pe_line(mix: &std::collections::HashMap<String, fetch::FundMix>) -> Option<String> {
    let mut rows: Vec<(&String, f64)> =
        mix.iter().filter_map(|(t, (_, _, pe))| pe.map(|p| (t, p))).collect();
    rows.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    (!rows.is_empty())
        .then(|| rows.iter().map(|(t, p)| format!("{t} {p:.0}")).collect::<Vec<_>>().join(" · "))
}

/// (r11) CAGR premium over the index on the longest SHARED leg — "10Y" first, else "5Y". Both
/// sides must carry the SAME horizon so the comparison is like-for-like (a young listing's 5Y
/// vs the index's 10Y would flatter it). `None` = no shared leg (young listing / failed index
/// fetch). Premium = name CAGR − index CAGR, in %/yr — positive means it beat buying the index.
fn spy_premium(q: &core::Quote, spx: &core::Quote) -> Option<(f64, &'static str)> {
    [("10Y", 10.0), ("5Y", 5.0)].iter().find_map(|(leg, yrs)| {
        match (picks::perf_pct(q, leg), picks::perf_pct(spx, leg)) {
            (Some(a), Some(b)) => Some((core::cagr(a, *yrs) - core::cagr(b, *yrs), *leg)),
            _ => None,
        }
    })
}

/// (r11) One name's calendar-year strip: the LAST `max_years` complete years, ascending.
fn year_cells(years: &[(i32, f64)], max_years: usize) -> String {
    let skip = years.len().saturating_sub(max_years);
    years[skip..].iter().map(|(y, v)| format!("{y} {v:+.0}%")).collect::<Vec<_>>().join(" · ")
}

/// (r13) One row per ranked fund WITH a captured BF benchmark, RANK order; benchless
/// (venue/regulatory-only) names are skipped — no claim, same stance as the other footers.
fn bench_rows<'q>(ranked: &[String], quotes: &'q [core::Quote]) -> Vec<(&'q str, &'q str)> {
    ranked
        .iter()
        .filter_map(|t| {
            let q = quotes.iter().find(|q| &q.ticker == t)?;
            Some((q.ticker.as_str(), q.benchmark.as_deref()?))
        })
        .collect()
}

/// (r19) Same-index twins: funds sharing one BF benchmark string are interchangeable wrappers,
/// so their realized 5Y gap IS the cost/replication difference — fund-vs-fund tracking, the
/// honest method (index-series comparisons mix price vs total-return bases). Exact `==` grouping:
/// BF normalizes the string, and hedged classes carry a different one so they never collide with
/// unhedged twins. Groups of ≥2 only (a solo fund is no comparison); <5y twins drop via perf
/// None — no claim. Each group best-compounder-first.
fn twin_groups<'q>(names: &[String], quotes: &'q [Quote]) -> Vec<(&'q str, Vec<(&'q str, f64)>)> {
    let mut by_bench: std::collections::BTreeMap<&str, Vec<(&str, f64)>> =
        std::collections::BTreeMap::new();
    for t in names {
        let Some(q) = quotes.iter().find(|q| &q.ticker == t) else { continue };
        let (Some(b), Some(p)) = (q.benchmark.as_deref(), picks::perf_pct(q, "5Y")) else {
            continue;
        };
        by_bench.entry(b).or_default().push((q.ticker.as_str(), p));
    }
    by_bench
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(b, mut v)| {
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            (b, v)
        })
        .collect()
}

/// (r20) Headlines footer feed: input order preserved (rank then pinned), names with no or
/// empty headline skipped silently — a headless row would be a fake "no news" claim. Yahoo's
/// search feed falls back to generic market stories, handing several names the SAME headline,
/// so exact-duplicate titles merge into one row listing every name that returned it.
fn headline_rows(
    names: &[String],
    first_title: &std::collections::BTreeMap<String, String>,
) -> Vec<(Vec<String>, String)> {
    let mut rows: Vec<(Vec<String>, String)> = Vec::new();
    for t in names {
        let Some(title) = first_title.get(t).filter(|s| !s.is_empty()) else { continue };
        match rows.iter_mut().find(|(_, existing)| existing == title) {
            Some((group, _)) => group.push(t.clone()),
            None => rows.push((vec![t.clone()], title.clone())),
        }
    }
    rows
}

/// (r14) "hedged" as a whole word — split on non-alphanumerics so "unhedged"/"Hedge" never match
/// (same word-split stance as fetch's use_from_name: substrings are trap city in fund names).
fn has_hedged_token(s: &str) -> bool {
    s.split(|c: char| !c.is_alphanumeric()).any(|w| w.eq_ignore_ascii_case("hedged"))
}

/// (r14) Ranked funds that are currency-hedged share classes, RANK order: the listing name
/// (all runs) or the BF bench string (wide runs) carries the word "hedged". Hedge cost drags a
/// decades hold — an accidental hedged pick instead of the unhedged twin is a silent mistake.
fn hedged_names<'q>(ranked: &[String], quotes: &'q [core::Quote]) -> Vec<&'q str> {
    ranked
        .iter()
        .filter_map(|t| quotes.iter().find(|q| &q.ticker == t))
        .filter(|q| has_hedged_token(&q.name) || q.benchmark.as_deref().is_some_and(has_hedged_token))
        .map(|q| q.ticker.as_str())
        .collect()
}

/// (r15) Footer population: the ranked book PLUS pinned extras. Pinned rows score a sentinel,
/// sort last, and never survive render's take(n) — so the HELD names (the ones the user cares
/// about most) carried no per-name footer data. Ranked order first, then pinned extras in
/// watchlist order. Track journal + order glue stay ranked-only (grading/deploy must not see
/// pinned rows).
fn footer_names(ranked: &[String], pinned: &[String]) -> Vec<String> {
    ranked.iter().chain(pinned.iter().filter(|p| !ranked.contains(p))).cloned().collect()
}

/// (round 28) Which of today's top rows have DURABLY held a top-10 rank across the journal —
/// the frequency the trust line's return-grade and the single-step membership diff don't show.
/// For each `today_top` ticker (rank order preserved), the fraction of `past` snapshots whose own
/// top-[`track::BOOK`] rows carried it; kept when `>= min_frac`. A name sitting at rank 11+ in a
/// snapshot does NOT count (the book is the top-10, same cut the journal is graded on). Empty
/// `past` → empty: a persistence claim needs history.
fn persistent_leaders(
    today_top: &[String],
    past: &[crate::commands::track::Snapshot],
    min_frac: f64,
) -> Vec<String> {
    if past.is_empty() {
        return Vec::new();
    }
    today_top
        .iter()
        .filter(|t| {
            let hits = past
                .iter()
                .filter(|s| {
                    s.rows
                        .iter()
                        .take(crate::commands::track::BOOK)
                        .any(|(r, _)| r == *t)
                })
                .count();
            hits as f64 / past.len() as f64 >= min_frac
        })
        .cloned()
        .collect()
}

/// (round 29) Rank DIRECTION for today's top names — which are climbing toward #1 vs fading down
/// the book across the journal. Orthogonal to the persistent-leaders footer (top-10 MEMBERSHIP)
/// and the trust line (RETURN): a name can hold the book durably yet be deteriorating, a caution
/// for a 20yr anchor. `journal` is chronological and includes today's row, so the trend ends at
/// today's rank. For each `today_top` ticker (today-order kept), its rank in each snapshot =
/// position within the top-[`track::BOOK`] slice, +1 (a name below the book has NO rank — same cut
/// r28 and `track::grade` use). A claim needs `>= min_pts` appearances; earlier-half mean vs
/// later-half mean decides, with a `band`-rank deadband so a flat drift stays silent. Returns
/// (climbers, faders), each `(ticker, first_rank, last_rank)` as first→last evidence.
#[allow(clippy::type_complexity)]
fn rank_trend(
    today_top: &[String],
    journal: &[crate::commands::track::Snapshot],
    min_pts: usize,
    band: f64,
) -> (Vec<(String, usize, usize)>, Vec<(String, usize, usize)>) {
    let mut climbers = Vec::new();
    let mut faders = Vec::new();
    for t in today_top {
        let ranks: Vec<usize> = journal
            .iter()
            .filter_map(|s| {
                s.rows
                    .iter()
                    .take(crate::commands::track::BOOK)
                    .position(|(r, _)| r == t)
                    .map(|p| p + 1)
            })
            .collect();
        if ranks.len() < min_pts {
            continue; // a trend needs enough points to be non-trivial
        }
        let m = ranks.len() / 2;
        let mean = |xs: &[usize]| xs.iter().sum::<usize>() as f64 / xs.len() as f64;
        let (mean_e, mean_l) = (mean(&ranks[..m]), mean(&ranks[m..]));
        let entry = (t.clone(), ranks[0], *ranks.last().unwrap());
        if mean_l <= mean_e - band {
            climbers.push(entry); // later ranks are NUMERICALLY smaller = higher up the book
        } else if mean_l >= mean_e + band {
            faders.push(entry);
        }
    }
    (climbers, faders)
}

/// (round 30) Book STABILITY — how much of the top-[`track::BOOK`] set carries over between
/// consecutive screens, averaged across the journal. A meta-confidence number ABOUT the ranking
/// (does it reshuffle every screen?), orthogonal to the persistent-leaders footer (which names
/// hold), the rank-trend footer (which way a name moves), and the trust line (return). Retention
/// per pair = |A ∩ B| / min(|A|, |B|) over the two top-[`track::BOOK`] slices — scored against the
/// SMALLER book so a short snapshot (some days store < 10 rows) reads as fewer slots, not churn.
/// `None` if fewer than two comparable screens.
fn book_stability(journal: &[crate::commands::track::Snapshot]) -> Option<f64> {
    let books: Vec<std::collections::HashSet<&str>> = journal
        .iter()
        .map(|s| {
            s.rows
                .iter()
                .take(crate::commands::track::BOOK)
                .map(|(t, _)| t.as_str())
                .collect()
        })
        .collect();
    if books.len() < 2 {
        return None; // no pair to compare
    }
    let mut sum = 0.0;
    let mut pairs = 0usize;
    for w in books.windows(2) {
        let denom = w[0].len().min(w[1].len());
        if denom == 0 {
            continue; // an empty book has nothing to retain
        }
        sum += w[0].intersection(&w[1]).count() as f64 / denom as f64;
        pairs += 1;
    }
    if pairs == 0 {
        return None;
    }
    Some(sum / pairs as f64)
}

/// (round 31) Mean book RANK — a name's AVERAGE position in the top-[`track::BOOK`] slice across
/// the journal, the LEVEL the slope/churn footers don't read. r29 rank-trend reports direction and
/// r30 reports book stability, but a name can be flat (r29 silent) yet durably sit at #2 vs #8 —
/// same r28 "persistent" bucket, opposite conviction. Reorders today's top by where each name has
/// SAT on average, so a one-day spike (high today, mid on average) sorts below a durable resident.
/// Rank per snapshot = position within the top-[`track::BOOK`] slice, +1 (a below-book name adds no
/// point — same cut r28/r29/r30 and `track::grade` use). A name needs `>= min_pts` appearances for a
/// non-trivial mean. Returns (ticker, mean_rank, appearances), best-seated (lowest mean) first.
fn mean_ranks(
    today_top: &[String],
    journal: &[crate::commands::track::Snapshot],
    min_pts: usize,
) -> Vec<(String, f64, usize)> {
    let mut out: Vec<(String, f64, usize)> = today_top
        .iter()
        .filter_map(|t| {
            let ranks: Vec<usize> = journal
                .iter()
                .filter_map(|s| {
                    s.rows
                        .iter()
                        .take(crate::commands::track::BOOK)
                        .position(|(r, _)| r == t)
                        .map(|p| p + 1)
                })
                .collect();
            if ranks.len() < min_pts {
                return None; // a 1-2 screen mean is trivial
            }
            let mean = ranks.iter().sum::<usize>() as f64 / ranks.len() as f64;
            Some((t.clone(), mean, ranks.len()))
        })
        .collect();
    // best-seated first; ties → more evidence first, then name (deterministic)
    out.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.2.cmp(&a.2)).then_with(|| a.0.cmp(&b.0)));
    out
}

/// (round 34) Fund FLOW — for today's top names, net shares created/redeemed across the journal
/// with price appreciation divided OUT of AUM growth. AUM = shares × price, so between a name's
/// EARLIEST and LATEST journal points that carry BOTH a positive close (`rows`) and a positive AUM
/// (`aum`), `(aum_late/aum_early) / (close_late/close_early) − 1` is the pure net-flow fraction: > 0
/// = money arriving (smart-money validation + closure-risk comfort for a 20yr hold), < 0 = bleeding
/// assets (a fund shrinking toward liquidation before a decades hold ends). Orthogonal to every
/// other footer — those read RANK and price-RETURN, never the asset base. AUM is rank-independent,
/// so points are gathered from ALL rows (not the top-[`track::BOOK`] cut the rank footers use). A
/// name needs ≥ 2 qualifying points; funds only (stocks/crypto journal `None` AUM and drop out).
/// Returns (ticker, net_flow_pct, points), biggest inflow first; empty when nothing qualifies.
fn fund_flow_lines(
    today_top: &[String],
    journal: &[crate::commands::track::Snapshot],
) -> Vec<(String, f64, usize)> {
    let mut out: Vec<(String, f64, usize)> = today_top
        .iter()
        .filter_map(|t| {
            // journal-order points where this name has BOTH a positive close and a positive AUM
            let pts: Vec<(f64, f64)> = journal
                .iter()
                .filter_map(|s| {
                    let close =
                        s.rows.iter().find(|(r, _)| r == t).and_then(|(_, p)| *p).filter(|p| *p > 0.0)?;
                    let aum =
                        s.aum.iter().find(|(r, _)| r == t).and_then(|(_, a)| *a).filter(|a| *a > 0.0)?;
                    Some((close, aum))
                })
                .collect();
            if pts.len() < 2 {
                return None; // a flow reading needs two AUM+close observations
            }
            let (c0, a0) = pts[0];
            let (c1, a1) = *pts.last().unwrap();
            let flow = (a1 / a0) / (c1 / c0) - 1.0; // price appreciation divided out → net shares
            Some((t.clone(), flow * 100.0, pts.len()))
        })
        .collect();
    // biggest inflow first; ties → more evidence, then name (deterministic)
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.2.cmp(&a.2)).then_with(|| a.0.cmp(&b.0)));
    out
}

/// (r15) Footer row label: pinned extras carry the table's `*` glyph so a starred footer row
/// reads as "your watchlist, not the ranking"; ranked names print bare.
fn footer_label(t: &str, ranked: &[String]) -> String {
    if ranked.iter().any(|r| r == t) { t.to_string() } else { format!("{t}*") }
}

/// (r15) Names with a KNOWN ISIN that the T212 catalog does NOT carry, input order. Catalog
/// membership is by ISIN (any venue listing counts as orderable). No cached ISIN or an empty
/// catalog = no claim — same silent stance as every other data footer.
fn t212_missing<'a>(
    names: &'a [String],
    isin_of: &std::collections::HashMap<String, String>,
    catalog: &std::collections::HashSet<String>,
) -> Vec<&'a str> {
    if catalog.is_empty() {
        return Vec::new();
    }
    names
        .iter()
        .filter_map(|t| isin_of.get(t).filter(|isin| !catalog.contains(*isin)).map(|_| t.as_str()))
        .collect()
}

/// (r16) Funds in the footer population with AUM under €100M — liquidation/closure territory
/// over a decades hold (a forced exit mid-hold is a taxable event). aum_shown() None
/// (stocks/crypto, factless funds) = skipped, no claim. Threshold hardcoded: industry rule of
/// thumb; the H/CORE gate already demands ≥€1B, this only guards the ranked tail.
fn small_aum_names<'q>(names: &'q [String], quotes: &[core::Quote]) -> Vec<(&'q str, f64)> {
    names
        .iter()
        .filter_map(|t| {
            let q = quotes.iter().find(|q| &q.ticker == t)?;
            q.aum_shown().filter(|a| *a < 1e8).map(|a| (t.as_str(), a))
        })
        .collect()
}

/// (crossover) Fund picks whose top-10 holdings you ALREADY own directly as stocks — buying the
/// fund silently doubles those positions (the `o` marker only catches the same ticker, sector
/// tilt only the sector). Per fund: the shared names with their in-fund weights + the summed
/// overlap, heaviest first. `held` = normalized broker stock bases (`Owned.stocks`). Zero-weight
/// holdings rows (Yahoo omitted the weight) drop — same blindness the top-heavy footer documents.
fn crossover_lines(
    holdings: &std::collections::HashMap<String, Vec<(String, f64)>>,
    held: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut rows: Vec<(f64, String)> = holdings
        .iter()
        .filter_map(|(fund, hs)| {
            let shared: Vec<(&str, f64)> = hs
                .iter()
                .filter(|(h, w)| *w > 0.0 && held.contains(&crate::picks::yahoo_base(h)))
                .map(|(h, w)| (h.as_str(), *w))
                .collect();
            (!shared.is_empty()).then(|| {
                let sum: f64 = shared.iter().map(|(_, w)| w).sum();
                let names = shared
                    .iter()
                    .map(|(h, w)| format!("{h} {:.0}%", 100.0 * w))
                    .collect::<Vec<_>>()
                    .join(" + ");
                (sum, format!("  {fund:<10} {names} = {:.0}% of the fund", 100.0 * sum))
            })
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
    // ticker, or no flag at all, still explains the #1 row — that footer is always on). The named
    // ticker is also added to the scan, so `screen --explain NVDA` ranks + explains just NVDA.
    let (explain, mut positional) = crate::commands::parse_explain("screen", args);
    if let Some(t) = &explain {
        positional.push(t.clone()); // ensure the target is fetched/scanned
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
    // names fetch swallowed to an err/no-data stub (quote_one self-swallows a bad ticker) — otherwise
    // invisible in aggregate: each surfaces only as one "err" row buried in a class table.
    let no_data = quotes.iter().filter(|q| q.price == "err" || q.price == "no data").count();
    println!(
        "Data quality: {} names | {no_data} no data | {stocks_no_pe} stocks missing P/E | {etfs_no_ter} ETFs missing TER | {} stale dropped (>{}d)",
        quotes.len(), fresh_before - quotes.len(), settings.stale_days
    );

    // Bitcoin NUPL: whole-market crypto sentiment gauge. Fetched BEFORE render so it can damp the
    // crypto rows (high NUPL = euphoric top), then also printed as the footer line.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;

    // (round 110/111) owned-position overlay: what you already hold at the brokers, so the tables
    // can mark covered rows with `o`. Stocks/ETFs from Trading212, crypto from Binance; each broker
    // absent (no env key) or erroring is independently and silently off — a broker key is optional
    // config, not a degradation. Display-only.
    // t212_raw keeps the broker's exact ticker forms (`AAPL_US_EQ`) — the order-glue footer needs
    // them verbatim, while the overlay below collapses them to comparable bases.
    let mut t212_raw: Vec<String> = Vec::new();
    let owned = {
        let mut o = crate::picks::Owned::default();
        if let Ok(v) = crate::broker::trading212::owned_tickers(&client).await {
            t212_raw = v.clone();
            o.stocks = v.iter().map(|t| crate::picks::t212_base(t)).collect();
        }
        if let Ok(v) = crate::broker::binance::owned_assets(&client).await {
            o.crypto = v.iter().map(|a| a.to_lowercase()).collect();
        }
        o
    };
    // (round 118) live free cash, same key + degrade-to-silence stance as the holdings overlay above.
    let t212_cash = crate::broker::trading212::cash_free(&client).await.ok();

    // (round 112) entry-state fetch, hoisted ABOVE the tables: S&P 500 % off its high decides how fast
    // new money should go in. Fetched once here; the top banner (when actionable) and the near-high
    // footer both read it. A failed fetch stays silent (None). Display-only.
    let spx = fetch::quotes(
        &client, &settings.urls, &fx_cache, &["^GSPC".to_string()], settings.dip_days, settings.high_days,
        false, false, &settings.anchor_windows, eu_infl.as_ref(),
    )
    .await;
    let spx_off_hi: Option<f64> = spx.first().map(|q| q.drawdown_pct);
    // Promote the actionable states (pullback/drawdown) to a loud banner ABOVE the ranking — the
    // round-109 footer was buried under the tables where the user never scrolled. Near-high stays a
    // quiet footer (nothing to do). None (fetch failed) prints nothing.
    if let Some(banner) = spx_off_hi.and_then(entry_state_banner) {
        println!("{banner}");
    }
    // (round 115) deploy-math: turn the entry state into a number. Prints only when the user set
    // their personal monthly base (monthly_deploy_eur > 0, private overlay).
    if let Some(line) = deploy_line(settings.monthly_deploy_eur, spx_off_hi) {
        println!("{line}");
    }
    // (round 118) is the deploy actually funded? print live free cash + a covers/short verdict
    // right under it. Silent without a key (t212_cash None), like the `o` overlay.
    if let Some(line) = cash_line(
        deploy_scaled_eur(settings.monthly_deploy_eur, spx_off_hi).map(|(_, t)| t),
        t212_cash,
    ) {
        println!("{line}");
    }

    // the 20yr+ growth ranking, split per asset class (stocks / ETFs / crypto); sectors filters ETFs
    // by fund name (stocks were already sector-filtered before fetch)
    // (round 52) render returns the score-math walkthrough; printed AFTER the actionable footers
    // (gate/exit review, fact drift, near-miss) so alerts aren't buried under arithmetic.
    // show_hold_core = true: this is a hunt (wide OR `screen etfs`), so re-surface the buy-and-hold
    // cores the momentum ranking buries at 0.0. Empty cores early-return, so stock/crypto lanes stay quiet.
    // (round 12) columns-drift nag: an explicit columns: list silently hides every column added
    // after it was written (dom sat invisible this way). stderr like the stale-nets nag.
    let hidden = picks::missing_columns(&settings.widths.columns);
    if !hidden.is_empty() {
        eprintln!("(columns: config hides available: {} — add to the columns: block in settings.yaml to show)", hidden.join(", "));
    }
    let (explain_text, ranked_now) = render(&quotes, settings.top_picks, &settings.buy_heuristic, &settings.widths, RenderCtx {
        nupl,
        sectors: &settings.sectors,
        sector_of: &sector_of,
        pinned: &settings.tickers,
        owned: &owned,
        explain: explain.as_deref(),
        show_hold_core: true,
    });

    // (round 114) live track record: journal today's ranked slice + the S&P close so `track` can
    // grade every past top-10 on prices that didn't exist when it ranked. One line per day (a
    // same-day rerun adds nothing); a failed append warns inside and never fails the screen.
    crate::commands::track::append_snapshot(&crate::commands::track::Snapshot {
        date: run_date.clone(),
        spx: spx.first().and_then(|q| q.price_eur),
        spx_off_hi,
        rows: ranked_now
            .iter()
            .map(|t| (t.clone(), quotes.iter().find(|q| &q.ticker == t).and_then(|q| q.price_eur)))
            .collect(),
        // (round 34) parallel per-name AUM so the fund-flow footer can accrue an AUM history. Uses
        // `aum_shown()` (BF `aum_eur` ∨ Yahoo `aum_fallback`) — the SAME value the table's AUM column
        // prints, so the journal matches what the user sees, and BF's intermittent universe
        // enrichment doesn't starve the signal. Non-funds carry None and never produce a reading.
        // ponytail: aum_shown source is stable within a user's consistent monthly full-runs; a rare
        // cross-source switch could blip one reading — split into per-source fields only if observed.
        aum: ranked_now
            .iter()
            .map(|t| (t.clone(), quotes.iter().find(|q| &q.ticker == t).and_then(|q| q.aum_shown())))
            .collect(),
    });

    // (r15) footer population: ranked book + pinned extras — the held/watched names sit in the
    // table (sentinel score) but carried no footer data until now. Starred rows = pinned extras.
    let footer_names = footer_names(&ranked_now, &settings.tickers);

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

    // (consistency) the named buy-and-hold datum: how often did 5 patient years pay? One line for
    // the ranked book, best first, from the same closes every other stat reads. NOMINAL by design
    // (the label says so — per-window inflation deflation isn't worth the date mapping); names
    // with <5y of history have no window and stay silent. DISPLAY-ONLY, never scored.
    {
        let mut rows: Vec<(String, f64)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .and_then(|q| q.roll5y_pos_pct)
                    .map(|p| (footer_label(t, &ranked_now), p))
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if !rows.is_empty() {
            let cells = rows.iter().map(|(t, p)| format!("{t} {p:.0}%")).collect::<Vec<_>>().join(" · ");
            println!(
                "\n5y-consistency — % of rolling 5-year windows with a positive (nominal) return, weekly-stepped:\n  {cells}"
            );
        }

        // (r16) the DECADE twin — the horizon the book is actually held for; names with <10y of
        // history stay silent (no claim), so a short 10y line next to a full 5y line is itself
        // information: the missing names simply haven't lived a decade yet.
        let mut rows: Vec<(String, f64)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .and_then(|q| q.roll10y_pos_pct)
                    .map(|p| (footer_label(t, &ranked_now), p))
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if !rows.is_empty() {
            let cells = rows.iter().map(|(t, p)| format!("{t} {p:.0}%")).collect::<Vec<_>>().join(" · ");
            println!(
                "\n10y-consistency — % of rolling 10-year windows with a positive (nominal) return, weekly-stepped:\n  {cells}"
            );
        }
    }

    // (underwater) the endurance twin of MAXDD, worst first: depth said how far down, this says
    // how LONG the pain lasted. Same closes as everything above; ongoing stretches count (the
    // OFF-HI column already says who's underwater NOW). DISPLAY-ONLY, never scored.
    {
        let mut rows: Vec<(String, f64)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .and_then(|q| q.underwater_yrs)
                    .map(|v| (footer_label(t, &ranked_now), v))
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if !rows.is_empty() {
            let cells = rows.iter().map(|(t, v)| format!("{t} {v:.1}y")).collect::<Vec<_>>().join(" · ");
            println!(
                "\nLongest underwater — worst stretch below the prior peak, in years (endurance check for buy-and-hold):\n  {cells}"
            );
        }
    }

    // (worst-5y) severity closes the closes-derived risk picture: depth (MAXDD column) ·
    // duration (underwater) · frequency (5y-consistency) · severity (this). Worst outcome
    // first. DISPLAY-ONLY, never scored.
    {
        let mut rows: Vec<(String, f64)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .and_then(|q| q.worst_5y_pct)
                    .map(|v| (footer_label(t, &ranked_now), v))
            })
            .collect();
        rows.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if !rows.is_empty() {
            let cells = rows.iter().map(|(t, v)| format!("{t} {v:+.0}%")).collect::<Vec<_>>().join(" · ");
            println!(
                "\nWorst 5-year hold — the single worst rolling 5y (nominal) outcome, weekly-stepped:\n  {cells}"
            );
        }

        // (r16) decade severity: "has ANY patient decade lost money?" — the literal 20y-hold
        // confidence question the 5y window can't answer.
        let mut rows: Vec<(String, f64)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .and_then(|q| q.worst_10y_pct)
                    .map(|v| (footer_label(t, &ranked_now), v))
            })
            .collect();
        rows.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if !rows.is_empty() {
            let cells = rows.iter().map(|(t, v)| format!("{t} {v:+.0}%")).collect::<Vec<_>>().join(" · ");
            println!(
                "\nWorst 10-year hold — the single worst rolling 10y (nominal) outcome, weekly-stepped:\n  {cells}"
            );
        }
    }

    // (r11) vs S&P 500 — the capstone yardstick surfaced live (the backtest's goal metric is
    // absolute-vs-^GSPC): each pick's CAGR premium over the index on the longest shared leg.
    // Reuses the round-112 entry-state ^GSPC quote — zero new network; a failed index fetch
    // silently drops this footer exactly like it drops the banner. DISPLAY-ONLY, never scored.
    if let Some(spx_q) = spx.first() {
        let mut rows: Vec<(String, f64, &str)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .and_then(|q| spy_premium(q, spx_q))
                    .map(|(p, leg)| (footer_label(t, &ranked_now), p, leg))
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if !rows.is_empty() {
            let cells =
                rows.iter().map(|(t, p, leg)| format!("{t} {p:+.1} ({leg})")).collect::<Vec<_>>().join(" · ");
            println!(
                "\nvs S&P 500 — CAGR premium over the index, longest shared leg (%/yr; the yardstick the strategy is graded on):\n  {cells}"
            );
        }
    }

    // (r11) calendar years — the regime check cumulative horizons smear: does the name lose whole
    // YEARS? One row per pick in RANK order (per-name detail, not a re-ranking); names without a
    // complete year pair stay silent. DISPLAY-ONLY, never scored.
    {
        let rows: Vec<(String, String)> = footer_names
            .iter()
            .filter_map(|t| {
                quotes
                    .iter()
                    .find(|q| &q.ticker == t)
                    .filter(|q| !q.year_returns.is_empty())
                    .map(|q| (footer_label(t, &ranked_now), year_cells(&q.year_returns, 8)))
            })
            .collect();
        if !rows.is_empty() {
            println!("\nCalendar years — each full year's return (regime check: does it lose whole years?):");
            for (t, cells) in rows {
                println!("  {t:<9} {cells}");
            }
        }

        // (r13) benchmark display: the BF index string is already on the Quote for twin hints —
        // showing it answers "what am I actually buying" and names hedged share classes outright.
        let rows = bench_rows(&footer_names, &quotes);
        if !rows.is_empty() {
            println!("\nIndex — the benchmark each fund tracks (BF-normalized; hedged classes say so):");
            for (t, b) in rows {
                println!("  {:<9} {b}", footer_label(t, &ranked_now));
            }
        }

        // (r19) same-index twin spread: interchangeable wrappers ranked by what they actually
        // compounded — the realized gap is TER + replication drag as one number.
        let groups = twin_groups(&footer_names, &quotes);
        if !groups.is_empty() {
            println!("\nSame-index twins — realized 5Y per wrapper (the gap = real cost/replication drag; pick the best compounder):");
            for (b, v) in groups {
                let cells = v
                    .iter()
                    .map(|(t, p)| format!("{} {p:+.1}%", footer_label(t, &ranked_now)))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let gap = v.first().map_or(0.0, |f| f.1) - v.last().map_or(0.0, |l| l.1);
                println!("  {b}: {cells}  (gap {gap:.1}pp)");
            }
        }

        // (r14) hedged-class warning: silent on a clean book (the common case).
        let hedged = hedged_names(&footer_names, &quotes);
        if !hedged.is_empty() {
            let names = hedged.iter().map(|t| footer_label(t, &ranked_now)).collect::<Vec<_>>().join(", ");
            println!("\nHedged share classes — currency-hedged (hedge cost drags a decades hold; check the unhedged twin first): {names}");
        }
    }

    // (r15) T212 orderability: the screen ends in T212 buy orders (order glue), so a ranked or
    // pinned name whose ISIN the broker catalog doesn't carry is a dead pick for THIS user —
    // say so before they shortlist it. ISIN-known names only (no claim otherwise); keyless runs
    // stay silent (instruments_cached returns empty without a key, ≤1 HTTP per 7 days with one).
    {
        let catalog: std::collections::HashSet<String> =
            crate::broker::trading212::instruments_cached(&client).await.into_iter().map(|i| i.isin).collect();
        if !catalog.is_empty() {
            let isin_of: std::collections::HashMap<String, String> =
                std::fs::read_to_string(crate::config::data_path(crate::fetch::ISIN_CACHE_PATH))
                    .ok()
                    .and_then(|s| serde_json::from_str::<std::collections::HashMap<String, String>>(&s).ok())
                    .map(|m| m.into_iter().map(|(isin, sym)| (sym, isin)).collect())
                    .unwrap_or_default();
            let missing = t212_missing(&footer_names, &isin_of, &catalog);
            if !missing.is_empty() {
                let names =
                    missing.iter().map(|t| footer_label(t, &ranked_now)).collect::<Vec<_>>().join(", ");
                println!("\nNot at Trading 212 — no listing for this ISIN in the broker catalog (can't order there): {names}");
            }
        }
    }

    // (r16) fund-survival line, same "can you actually hold it for decades" family as the T212
    // marker: the drift alert only fires on AUM COLLAPSE between runs and the H/CORE gate only
    // grades core candidates — nothing said a ranked fund is small RIGHT NOW. Silent on a clean
    // book (the common case; the current ranked tail bottoms around €300M).
    {
        let small = small_aum_names(&footer_names, &quotes);
        if !small.is_empty() {
            let cells = small
                .iter()
                .map(|(t, a)| {
                    let amt = if *a >= 1e6 { format!("€{:.0}M", a / 1e6) } else { format!("€{:.0}K", a / 1e3) };
                    format!("{} {amt}", footer_label(t, &ranked_now))
                })
                .collect::<Vec<_>>()
                .join(", ");
            println!("\nFund survival — AUM under €100M (liquidation/closure risk over a decades hold; a forced exit is a taxable event): {cells}");
        }
    }

    // (r20) headlines footer — the LAST census shelf item: latest first headline per ranked or
    // pinned name, context only and labeled so: news is momentum noise against a 20y hold, the
    // case for a name lives in the numbers above. Fetched here for just the footer names — the
    // universe quotes call keeps news OFF (~500 wasted paced calls otherwise).
    {
        let mut first_title: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for t in &footer_names {
            if let Some(title) =
                fetch::fetch_news(&client, &settings.urls, t).await.into_iter().next()
            {
                first_title.insert(t.clone(), title);
            }
        }
        let rows = headline_rows(&footer_names, &first_title);
        if !rows.is_empty() {
            println!("\nHeadlines — latest news per name (context only, NOT a 20y signal):");
            for (group, title) in rows {
                let labels =
                    group.iter().map(|t| footer_label(t, &ranked_now)).collect::<Vec<_>>().join(", ");
                println!("  {labels:<9} {title}");
            }
        }
    }

    // (X) EXIT review — WATCHLIST names that cleared every growth gate on the previous screen run
    // but fail one now. The backtest's exit probe measures this exact transition: newly-failing
    // names lag kept-passing names by ~14 pts forward — a mild REVIEW signal, not an auto-sell.
    // Watchlist only (the holdings — actionable); universe names churn with fetch batches and
    // would spam. First run (no state file) prints nothing and just seeds the state.
    let watch: Vec<&Quote> = quotes.iter().filter(|q| settings.tickers.contains(&q.ticker)).collect();
    let (prior, state_corrupt) = parse_state(std::fs::read_to_string(crate::config::data_path(SCREEN_STATE_FILE)).ok());
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
        ranked: ranked_now.clone(), // still needed below by the order-glue footer
    };
    // (round 69) persistence failure must not be silent: a stuck baseline means every drift alert
    // above re-fires (or a pending one never fires) on the next run with no hint why. Serialize
    // failure no longer writes an empty file (which r62 would then report as CORRUPT). Warn+journal,
    // never abort — one run's worth of stale baseline is annoying, a dead screen is worse.
    let persisted = serde_json::to_string(&state).map(|json| std::fs::write(crate::config::data_path(SCREEN_STATE_FILE), json));
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
        let (holdings, mix) = fetch::yahoo_top_holdings(&client, &syms).await;
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
        // (sector tilt) the composition datum the two blocks above can't show: a concentrated
        // tech ETF and a broad core list the SAME mega-caps up top — the sector spread is where
        // they differ. Same payload as the holdings fetch, previously discarded.
        let tilt = sector_tilt_lines(&mix);
        if !tilt.is_empty() {
            println!("\nSector tilt — fund picks by top-sector weight (what the shared mega-cap holdings hide):");
            for l in &tilt {
                println!("{l}");
            }
        }
        // (crossover) the third concentration lens: fund picks × the stocks you already hold
        // directly. Both inputs are already in memory (holdings cache + broker positions) — the
        // cross was just never computed.
        let cross = crossover_lines(&holdings, &owned.stocks);
        if !cross.is_empty() {
            println!("\nPortfolio crossover — picks that duplicate stocks you already hold directly (buying the fund doubles them):");
            for l in &cross {
                println!("{l}");
            }
        }
        // (fund valuation) HOW pricey the pricey quality is: the fund book's P/E, same payload
        // as everything above. Display-only, never scored.
        if let Some(pe) = fund_pe_line(&mix) {
            println!("\nFund valuation — P/E of each fund's equity book (cheapest first): {pe}");
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

    // (round 112) entry-state footer: only the NEAR-HIGH case lands here — pullback/drawdown were
    // already promoted to the loud top banner (above the tables). Keeping the quiet one-liner here for
    // near-high avoids a redundant banner when there's nothing to do. `spx` was fetched above render.
    if let Some(off) = spx_off_hi.filter(|off| *off < 5.0) {
        println!("\n{}", entry_state_line(off));
    }

    // (round 116) order-glue: the ranked top book as paste-ready `trade` commands, so acting on the
    // screen stops being manual retyping. Broker per class (stocks/ETFs → Trading212, crypto →
    // Binance); exact T212 symbols only exist for names already held (t212_raw), others print a
    // placeholder; Binance pairs are derivable for every coin. QTY = this month's deploy € (base ×
    // entry-state multiplier, same math as the top line) split equally across the book — the
    // equal-weight top-10 IS the validated backtest/track book. Each command still runs trade's own
    // real-money confirm gate; nothing here sends anything.
    {
        let deploy_scaled =
            deploy_scaled_eur(settings.monthly_deploy_eur, spx_off_hi).map(|(_, total)| total);
        let book: Vec<&String> = ranked_now.iter().take(crate::commands::track::BOOK).collect();
        // (round 117) fetch the full T212 instrument list (7-day cached, silent-empty without a
        // key) only when some stock/ETF row can't already be resolved from held positions — a
        // fully-held book or a keyless run costs zero extra HTTP. The ISIN map (inverted from the
        // ETF universe's ISIN→Yahoo cache) gives the resolver its exact-match path.
        let need_instruments = book.iter().any(|t| {
            !crate::picks::is_currency_quoted(t)
                && !t212_raw.iter().any(|r| crate::picks::t212_base(r) == crate::picks::yahoo_base(t))
        });
        let instruments = if need_instruments {
            crate::broker::trading212::instruments_cached(&client).await
        } else {
            Vec::new()
        };
        let isin_of: std::collections::HashMap<String, String> = if instruments.is_empty() {
            Default::default()
        } else {
            std::fs::read_to_string(crate::config::data_path(crate::fetch::ISIN_CACHE_PATH))
                .ok()
                .and_then(|s| serde_json::from_str::<std::collections::HashMap<String, String>>(&s).ok())
                .map(|m| m.into_iter().map(|(isin, sym)| (sym, isin)).collect())
                .unwrap_or_default()
        };
        // (round 118) tally which resolution rung produced each symbol — the stderr report below
        // makes a keyed verification run self-explaining and turns a silent resolver miss (stale
        // ISIN cache, quietly-empty instruments fetch) into a visible placeholder count.
        let mut glue_rows: Vec<(String, Option<f64>, &'static str, Option<String>)> = Vec::new();
        let (mut n_owned, mut n_isin, mut n_base, mut n_binance, mut n_ph) = (0usize, 0usize, 0usize, 0usize, 0usize);
        for t in &book {
            let price = quotes.iter().find(|q| &q.ticker == *t).and_then(|q| q.price_eur);
            if crate::picks::is_currency_quoted(t) {
                n_binance += 1;
                let sym = format!("{}EUR", crate::picks::underlying(t).to_uppercase());
                glue_rows.push(((*t).clone(), price, "binance", Some(sym)));
            } else {
                let resolved = resolve_t212(t, &t212_raw, &instruments, &isin_of);
                match &resolved {
                    Some((_, "owned")) => n_owned += 1,
                    Some((_, "isin")) => n_isin += 1,
                    Some(_) => n_base += 1,
                    None => n_ph += 1,
                }
                glue_rows.push(((*t).clone(), price, "trading212", resolved.map(|(sym, _)| sym)));
            }
        }
        if let Some(glue) = order_glue(&glue_rows, deploy_scaled) {
            println!("{glue}");
            eprintln!(
                "screen: order symbols — owned {n_owned} | isin {n_isin} | base {n_base} | placeholder {n_ph} | binance {n_binance} | instruments {}",
                instruments.len()
            );
        }
    }

    // (trust line) the ranking's own live out-of-sample grade: every past journaled top-10 at
    // today's prices vs the S&P 500 — same fold as `track` (verdict_stats), so the two can't
    // disagree. Zero new fetches: past books are ex-universe names, so this run's quotes already
    // price them (a narrow watchlist run may grade fewer rows; track's table stays the honest
    // view). Today's own snapshot is 0 days old and grades nothing, so no self-grade.
    {
        let (snaps, _) = crate::commands::track::read_snapshots();
        if !snaps.is_empty() {
            let today = chrono::Local::now().date_naive();
            let px_now = |t: &str| {
                quotes.iter().find(|q| q.ticker == t).and_then(|q| q.price_eur).filter(|p| *p > 0.0)
            };
            let spx_now = spx.first().and_then(|q| q.price_eur).filter(|p| *p > 0.0);
            let (wins, n, sum) = crate::commands::track::verdict_stats(&snaps, today, &px_now, spx_now);
            if n > 0 {
                println!(
                    "\nLive track record — this ranking's past top-10s at today's prices: {} (details: `folioman track`)",
                    crate::commands::track::summary_line(wins, n, sum)
                );
            }
            // (round 25) follow-the-screen digest: the sim ledger folded to one line — the
            // compounded €-outcome of buying each month's book vs the same cashflow DCA'd into
            // the index. Same fns as `sim` (ledger/holdings/value fold), so the two surfaces
            // can't disagree. Gated on the same knob as the deploy line and `sim` itself;
            // unset knob, nothing bought yet, or nothing priced today = silent.
            if settings.monthly_deploy_eur > 0.0 {
                let now_ym = (chrono::Datelike::year(&today), chrono::Datelike::month(&today));
                if let Some((since, cost, value, bench, priced, held)) = crate::commands::sim::digest(
                    &snaps, settings.monthly_deploy_eur, now_ym, &px_now, spx_now,
                ) {
                    println!(
                        "\n{}",
                        crate::commands::sim::digest_line(
                            settings.monthly_deploy_eur, &since, cost, value, bench, priced, held,
                        )
                    );
                }
            }
        }
    }

    // (round 27) method line: the wide-backtest verdict (top-10 held-book vs the index over the
    // whole 54y sample) journaled by `backtest universe`, cited here so the buy surface carries
    // the long-horizon proof of the ranking it prints — not just this run's live grade. Drift =
    // the tuning changed since the run, so the numbers are stale; say so. Absent file (never
    // backtested) = silent. Zero fetch.
    if let Some(v) = crate::commands::backtest::read_verdict() {
        let drift = v.tuning_fp != crate::commands::backtest::tuning_fingerprint(&settings.buy_heuristic);
        println!("\n{}", crate::commands::backtest::verdict_line(&v, drift));
    }

    // (round 28) persistent leaders: which of today's top-10 have DURABLY held a top-10 rank
    // across the journal — the multi-screen frequency the trust line (return) and the membership
    // diff (churn since last screen) don't show. A 20yr holder wants the durable leaders, not a
    // name that flashed in once. Reads the same journal as the trust line; today's own row (just
    // written) is excluded so the frequency is out-of-sample. K and the since-date are stated so a
    // thin journal can't read as a long record. Zero fetch; silent under 2 past screens or an
    // empty durable set.
    {
        let (snaps, _) = crate::commands::track::read_snapshots();
        let past: Vec<_> = snaps.into_iter().filter(|s| s.date < run_date).collect();
        if past.len() >= 2 {
            let today_top: Vec<String> =
                ranked_now.iter().take(crate::commands::track::BOOK).cloned().collect();
            let leaders = persistent_leaders(&today_top, &past, 0.8);
            if !leaders.is_empty() {
                let k = past.len();
                let since = past.iter().map(|s| s.date.as_str()).min().unwrap_or("");
                println!(
                    "\nPersistent leaders — held a top-10 rank in ≥80% of the last {k} screens (since {since}): {}",
                    leaders.join(", ")
                );
            }
        }
    }

    // (round 29) rank trend: for today's top-10, which are CLIMBING toward #1 vs FADING down the
    // book across the journal — the direction the persistent-leaders footer (membership) and the
    // trust line (return) don't show. A durable name that's deteriorating is a caution for a 20yr
    // anchor. The chronological journal already includes today's row, so the trend ends at today's
    // rank. Zero fetch; needs ≥3 screens and ≥3 appearances per name; a ±1-rank deadband keeps a
    // flat drift silent; silent when nothing clears the band.
    {
        let (snaps, _) = crate::commands::track::read_snapshots();
        if snaps.len() >= 3 {
            let today_top: Vec<String> =
                ranked_now.iter().take(crate::commands::track::BOOK).cloned().collect();
            let (up, down) = rank_trend(&today_top, &snaps, 3, 1.0);
            if !up.is_empty() || !down.is_empty() {
                let k = snaps.len();
                let fmt = |v: &[(String, usize, usize)]| {
                    v.iter()
                        .map(|(n, a, b)| format!("{n} (#{a}→#{b})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut parts = Vec::new();
                if !up.is_empty() {
                    parts.push(format!("climbing: {}", fmt(&up)));
                }
                if !down.is_empty() {
                    parts.push(format!("fading: {}", fmt(&down)));
                }
                println!("\nRank trend over the last {k} screens — {}", parts.join(" · "));
            }
        }
    }

    // (round 30) book stability: how much of the top-10 set has carried over screen-to-screen
    // across the journal — a meta-confidence number ABOUT the ranking, not any one name. A book
    // that reshuffles every screen is a weak 20yr anchor even when today's #1 looks great. Distinct
    // from the single-step membership diff above: this averages retention over ALL consecutive
    // pairs, scored against the smaller book so a short snapshot reads as fewer slots, not churn.
    // Zero fetch; needs ≥3 screens (≥2 pairs); silent otherwise.
    {
        let (snaps, _) = crate::commands::track::read_snapshots();
        if snaps.len() >= 3 {
            if let Some(ratio) = book_stability(&snaps) {
                let book = crate::commands::track::BOOK;
                let k = snaps.len();
                let pct = ratio * 100.0;
                let tag = if ratio >= 0.8 {
                    "stable"
                } else if ratio >= 0.6 {
                    "moderate churn"
                } else {
                    "churny — ranks are noisy"
                };
                println!(
                    "\nTop-{book} stability — holds {pct:.0}% of its names screen-to-screen \
                     across the last {k} screens ({tag})"
                );
            }
        }
    }

    // (round 31) average book rank: for today's top-10, the mean position each name has held in the
    // journal — the LEVEL the rank-trend (slope) and book-stability (churn) footers don't read. A
    // flat name r29 stays silent on can still durably sit at #2 vs #8; this reorders today's top by
    // where each has SAT on average, so a one-day spike sorts below a durable resident. Zero fetch;
    // needs ≥3 screens and ≥3 appearances per name; silent when none qualify.
    {
        let (snaps, _) = crate::commands::track::read_snapshots();
        if snaps.len() >= 3 {
            let today_top: Vec<String> =
                ranked_now.iter().take(crate::commands::track::BOOK).cloned().collect();
            let depth = mean_ranks(&today_top, &snaps, 3);
            if !depth.is_empty() {
                let k = snaps.len();
                let parts = depth
                    .iter()
                    .map(|(n, m, c)| format!("{n} #{m:.1} ({c}×)"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                println!("\nAverage book rank over the last {k} screens — {parts}");
            }
        }
    }

    // (round 34) fund flow: for today's top-10, net shares created/redeemed across the journal with
    // price appreciation divided OUT of AUM growth — is each fund GAINING or BLEEDING assets? A 20yr
    // durability axis orthogonal to every rank/return footer above: a fund bleeding AUM risks
    // liquidation before a decades hold ends, and net inflows are smart-money confirmation. Funds
    // only (stocks/crypto carry no AUM). Zero fetch. COARSE by design — BF refreshes AUM ~monthly,
    // so a reading accrues over weeks, not per-day; silent until ≥2 journal points carry AUM+close.
    {
        let (snaps, _) = crate::commands::track::read_snapshots();
        let today_top: Vec<String> =
            ranked_now.iter().take(crate::commands::track::BOOK).cloned().collect();
        let flows = fund_flow_lines(&today_top, &snaps);
        if !flows.is_empty() {
            let parts = flows
                .iter()
                .map(|(n, f, c)| {
                    let tag = if *f < 0.0 { " (bleeding)" } else { "" };
                    format!("{n} {f:+.1}% ({c}×){tag}")
                })
                .collect::<Vec<_>>()
                .join(" · ");
            println!(
                "\nFund flows across the journal (net shares created/redeemed, price-adjusted) — {parts}"
            );
        }
    }

    // Conviction bridge: the per-name depth lives in other subcommands, but nothing on this
    // surface said so — the ranking is where a pick decision starts, so the pointer belongs here.
    println!(
        "\nBefore buying a row, deep-dive it: `folioman report TICKER` (income trajectory, fund \
         sectors/assets, why-ranked verdict) · `folioman screen --explain TICKER` (the score \
         arithmetic) · `folioman size` (how much of each)."
    );

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
    let mut degraded: Vec<String> = Vec::new();
    if nupl.is_none() {
        degraded.push("NUPL feed down (crypto euphoria damping off)".to_string());
    }
    if eu_infl.as_ref().is_some_and(|m| m.is_empty()) {
        degraded.push("EU HICP feed down (inflation adjustment off)".to_string());
    } else if let Some(y) = eu_infl
        .as_ref()
        .and_then(|m| core::infl_series_stale(m, chrono::Local::now().date_naive()))
    {
        // frozen-not-empty feed (e.g. a terminated Eurostat dataset): adjustment still runs
        // but deflates with rates that stop at an old year
        degraded.push(format!("EU HICP feed stale (latest {y} — inflation adjustment using old rates)"));
    }
    if fund_tilt_uncovered {
        degraded
            .push("fund tilt feed down (0 stocks carry the factor; stock ranks are price-only)".to_string());
    }
    if !degraded.is_empty() {
        eprintln!("screen: DEGRADED — {}", degraded.join("; "));
    }
    let stamp = std::fs::read_to_string(crate::config::data_path(crate::config::NET_STAMP_FILE)).ok();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(nag) = net_nag(stamp.as_deref(), now_secs) {
        eprintln!("{nag}");
    }
}

/// (tests round 4) Stale drift-net nag. The live network probes are the only thing that catches a
/// feed changing shape under the parsers (no HTTP mocks by design), but they only run when someone
/// types the command — which had happened three times ever, each at ship time, while a real
/// fund-facts outage sat invisible. The screen is the moment the numbers get trusted, so staleness
/// announces itself here. Missing/corrupt stamp degrades to the never-ran wording; clock skew is
/// cosmetic (saturating).
const NET_STAMP_STALE_DAYS: u64 = 30;

fn net_nag(stamp: Option<&str>, now_secs: u64) -> Option<String> {
    const CMD: &str = "FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored";
    match stamp.and_then(|s| s.trim().parse::<u64>().ok()) {
        None => Some(format!(
            "screen: drift nets have never run on this machine — feed drift would be invisible; run {CMD}"
        )),
        Some(then) => {
            let age_days = now_secs.saturating_sub(then) / 86_400;
            (age_days > NET_STAMP_STALE_DAYS)
                .then(|| format!("screen: drift nets last ran {age_days}d ago — run {CMD}"))
        }
    }
}

/// (round 112) Entry-state classifier — the single source of state name + read wording, so the top
/// banner, the footer and the `alert` flip ping never drift. `off_hi` = % the S&P 500 sits below its high (positive, 0 = at
/// high). Classes: <5 near-high, 5–15 pullback, ≥15 drawdown. Wording is now the EXCESS-vs-SPY framing
/// from this session's 12y multi-regime + survivorship-stress run (entries bucketed by how far the
/// index was off its high): drawdown +9.1 pts/yr over SPY, pullback +6.0, near-high +5.9 — the whole
/// book beats the index at every entry point, so the lever is deployment SPEED, never waiting in cash.
pub(crate) fn entry_state_class(off_hi: f64) -> (&'static str, &'static str) {
    if off_hi >= 15.0 {
        ("DRAWDOWN", "Deploy new money FASTER — drawdown entries beat the index by +9.1 pts/yr vs +5.9 near the high (12y multi-regime backtest, 100% win vs SPY)")
    } else if off_hi >= 5.0 {
        ("PULLBACK", "Lean in as it deepens — pullback entries beat the index +6.0 pts/yr vs +5.9 near the high; the edge steepens past −15%")
    } else {
        ("NEAR-HIGH", "Normal schedule — near-high entries still beat the index +5.9 pts/yr; waiting in cash for a dip was the only losing move")
    }
}

/// (round 109→112) One-line entry-state read for the near-high footer. Delegates to
/// [`entry_state_class`]; format is unchanged so the alert scans the same as the other footers.
fn entry_state_line(off_hi: f64) -> String {
    let (state, read) = entry_state_class(off_hi);
    format!("Entry state: S&P 500 {off_hi:.1}% off its high — {state}. {read}. NOT advice.")
}

/// (round 112) Loud top banner for the ACTIONABLE entry states (pullback/drawdown). `None` at near-high
/// (<5% off) — nothing to do, so the quiet footer covers it. `Some` boxed multi-line printed ABOVE the
/// ranking tables so the deploy-faster signal isn't buried below them like the round-109 footer was.
fn entry_state_banner(off_hi: f64) -> Option<String> {
    if off_hi < 5.0 {
        return None;
    }
    let (state, read) = entry_state_class(off_hi);
    let rule = "=".repeat(72);
    Some(format!(
        "\n{rule}\n  ENTRY STATE: {state} — S&P 500 {off_hi:.1}% off its high\n  {read}. NOT advice.\n{rule}"
    ))
}

/// (round 115) deploy-math: monthly base € × entry-state multiplier, so "how much this month?"
/// stops being mental arithmetic. Multipliers 1×/1.5×/2× follow the backtest excess shape
/// (+5.9/+6.0/+9.1 pts/yr near-high/pullback/drawdown) — never 0× anywhere, waiting in cash was
/// the only losing move. State thresholds stay single-sourced in [`entry_state_class`]. `None`
/// when the knob is unset (≤0). A failed ^GSPC fetch prints the 1× base and says why instead of
/// guessing a state.
pub(crate) fn deploy_line(base_eur: f64, off_hi: Option<f64>) -> Option<String> {
    let (mult, total) = deploy_scaled_eur(base_eur, off_hi)?;
    Some(match off_hi {
        Some(off) => {
            let (state, _) = entry_state_class(off);
            format!(
                "\n  DEPLOY THIS MONTH: €{total:.0} — base €{base_eur:.0} × {mult} ({state} entry state). NOT advice."
            )
        }
        None => format!(
            "\n  DEPLOY THIS MONTH: €{total:.0} — base × {mult} (S&P 500 entry state unavailable this run). NOT advice."
        ),
    })
}

/// (round 118) Broker free-cash line, printed under the deploy figure so the month's deploy
/// fundability is visible without a separate `accounts` run. `t212_free` is the live Trading212
/// free-cash balance (`None` = no key / fetch failed → the line stays silent, exactly like the
/// `o` holdings overlay on a keyless run). With a monthly deploy set (`deploy_scaled` = the
/// entry-scaled €) the line says whether the cash covers it or by how much it falls short; with
/// no budget it just states the balance. Display-only, no ranking effect. Trading212 only — the
/// ETF deploy broker; Binance (crypto) cash is a different budget, deferred.
fn cash_line(deploy_scaled: Option<f64>, t212_free: Option<f64>) -> Option<String> {
    let free = t212_free?;
    Some(match deploy_scaled {
        Some(d) if d > 0.0 => {
            let verdict = if free >= d {
                format!("covers this month's €{d:.0} deploy")
            } else {
                format!("€{:.0} short of the €{d:.0} deploy", d - free)
            };
            format!("\n  Broker cash — Trading212 free €{free:.0}, {verdict}.")
        }
        _ => format!("\n  Broker cash — Trading212 free €{free:.0}."),
    })
}

/// (tests round 5) The deploy composition — knob gate (≤0 = unset), unknown-state ×1 fallback,
/// base × ladder — in ONE place. The banner/`size` line (via [`deploy_line`]), the order-glue
/// QTY sizing, and `sim`'s monthly paper budget all consume this, so the € the banner announces,
/// the € the orders split, and the € the sim deploys can never disagree; until now only the
/// multiplier was shared and the composition lived twice.
pub(crate) fn deploy_scaled_eur(base_eur: f64, off_hi: Option<f64>) -> Option<(f64, f64)> {
    if base_eur <= 0.0 {
        return None;
    }
    let mult = off_hi.map_or(1.0, deploy_multiplier);
    Some((mult, base_eur * mult))
}

/// (round 116) The 1×/1.5×/2× ladder as a number — shared by the deploy line and the order-glue
/// QTY sizing so the two can never disagree. Thresholds stay in [`entry_state_class`].
fn deploy_multiplier(off_hi: f64) -> f64 {
    match entry_state_class(off_hi).0 {
        "DRAWDOWN" => 2.0,
        "PULLBACK" => 1.5,
        _ => 1.0,
    }
}

/// (round 117) Resolve a Yahoo ticker to its exact Trading212 symbol, safest source first:
/// (1) held positions are ground truth; (2) ISIN-exact via the ETF universe's ISIN cache —
/// several listings of one ISIN are the same fund, prefer the EUR one; (3) base-symbol match
/// ONLY when unique — two venues sharing a base is ambiguity, and a guessed venue in a
/// paste-ready real-money command is worse than a placeholder. Symbols return VERBATIM: T212
/// venue letters are meaningfully lowercase (`VUAGl_EQ`, l = LSE) and `trade` now sends them
/// untransformed — the instrument list is the single source of truth end-to-end.
fn resolve_t212(
    yahoo: &str,
    owned_raw: &[String],
    instruments: &[crate::broker::trading212::Instrument],
    isin_of: &std::collections::HashMap<String, String>,
) -> Option<(String, &'static str)> {
    let base = crate::picks::yahoo_base(yahoo);
    if let Some(o) = owned_raw.iter().find(|r| crate::picks::t212_base(r) == base) {
        return Some((o.clone(), "owned"));
    }
    if let Some(isin) = isin_of.get(yahoo) {
        let hits: Vec<&crate::broker::trading212::Instrument> =
            instruments.iter().filter(|i| &i.isin == isin).collect();
        if !hits.is_empty() {
            let pick = hits.iter().find(|i| i.currency == "EUR").unwrap_or(&hits[0]);
            return Some((pick.ticker.clone(), "isin"));
        }
    }
    let base_hits: Vec<&crate::broker::trading212::Instrument> =
        instruments.iter().filter(|i| crate::picks::t212_base(&i.ticker) == base).collect();
    match base_hits.as_slice() {
        [one] => Some((one.ticker.clone(), "base")),
        _ => None,
    }
}

/// (round 116) order-glue: the top book as paste-ready `trade` commands. Rows =
/// (yahoo ticker, price €, broker, broker symbol if known); `deploy_eur` = this month's scaled
/// deploy total, split equally across the rows (the equal-weight book). Unknown broker symbol →
/// `<T212_SYMBOL>` placeholder (T212 forms are only knowable from held positions); no deploy set
/// or no price → `<QTY>`. Never prints an empty section; commands only PRINT here — sending one
/// still walks trade's real-money confirm gate.
fn order_glue(rows: &[(String, Option<f64>, &'static str, Option<String>)], deploy_eur: Option<f64>) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let n = rows.len();
    let slice = deploy_eur.map(|d| d / n as f64);
    let mut out = String::from("\nPaste-ready orders — ");
    match slice {
        Some(s) => out.push_str(&format!(
            "€{:.0} this month ÷ {n} names ≈ €{s:.0} each. Review each; every command asks its own real-money 'yes'. NOT advice.\n",
            s * n as f64
        )),
        None => out.push_str(
            "set monthly_deploy_eur for sized QTY. Review each; every command asks its own real-money 'yes'. NOT advice.\n",
        ),
    }
    let mut unheld = false;
    for (ticker, price, broker, sym) in rows {
        let sym_cell = sym.clone().unwrap_or_else(|| {
            unheld = true;
            "<T212_SYMBOL>".to_string()
        });
        let qty_cell = match (slice, price) {
            (Some(s), Some(p)) if *p > 0.0 => format!("{:.4}", s / p),
            _ => "<QTY>".to_string(),
        };
        let price_note = price.map_or(String::new(), |p| format!(" @ €{p:.2}"));
        out.push_str(&format!("  folioman trade {broker} buy {sym_cell} {qty_cell}   # {ticker}{price_note}\n"));
    }
    if unheld {
        out.push_str("  # <T212_SYMBOL> = not currently held, so the Trading212 ticker form is unknown — look it up in the app once.\n");
    }
    out.pop(); // drop the trailing newline; the caller println!'s
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// (tests round 4) Stale-nets nag: never-ran/corrupt and stale stamps must nag with the exact
    /// run command; a fresh or exactly-30d stamp must stay silent (monthly cadence, `>` not `>=`).
    #[test]
    fn net_nag_fires_on_stale_missing_and_garbage_stamps() {
        let now = 1_800_000_000u64;
        for bad in [None, Some(""), Some("not-a-number")] {
            let msg = net_nag(bad, now).expect("missing/corrupt stamp must nag");
            assert!(msg.contains("never run"), "wrong never-ran wording: {msg}");
            assert!(msg.contains("FOLIOMAN_NET_TESTS=1"), "nag must carry the run command: {msg}");
        }
        let stale = (now - 45 * 86_400).to_string();
        let msg = net_nag(Some(&stale), now).expect("45d-old stamp must nag at a 30d threshold");
        assert!(msg.contains("45d ago"), "age missing from stale nag: {msg}");
        let fresh = (now - 86_400).to_string();
        assert!(net_nag(Some(&fresh), now).is_none(), "fresh stamp must not nag");
        let edge = (now - 30 * 86_400).to_string();
        assert!(net_nag(Some(&edge), now).is_none(), "exactly 30d is the cadence, not stale");
    }

    /// (round 109) entry-state classes + exact boundaries: <5 near-high, 5–15 pullback, ≥15 drawdown.
    #[test]
    fn entry_state_classes_and_boundaries() {
        assert!(entry_state_line(0.0).contains("NEAR-HIGH"));
        assert!(entry_state_line(4.9).contains("NEAR-HIGH"));
        assert!(entry_state_line(5.0).contains("PULLBACK"));
        assert!(entry_state_line(14.9).contains("PULLBACK"));
        assert!(entry_state_line(15.0).contains("DRAWDOWN"));
        assert!(entry_state_line(7.0).contains("7.0% off"));
    }

    /// (round 112) the top banner fires ONLY for actionable states (≥5% off) and carries the state name
    /// + the deploy verb; near-high returns None so the footer (not a banner) covers it.
    #[test]
    fn entry_state_banner_actionable_only() {
        assert!(entry_state_banner(0.0).is_none());
        assert!(entry_state_banner(4.9).is_none());
        let pull = entry_state_banner(5.0).expect("pullback is actionable");
        assert!(pull.contains("PULLBACK") && pull.contains("Lean in") && pull.contains("5.0% off"));
        let dd = entry_state_banner(15.0).expect("drawdown is actionable");
        assert!(dd.contains("DRAWDOWN") && dd.contains("Deploy") && dd.contains("FASTER"));
        // the banner and the footer never disagree on the state name for the same input
        assert!(entry_state_line(20.0).contains("DRAWDOWN") && entry_state_banner(20.0).unwrap().contains("DRAWDOWN"));
    }

    /// (round 115) deploy-math line: off at base ≤0; multiplier follows the entry state
    /// (1×/1.5×/2× at the same <5/5–15/≥15 boundaries as the classifier); unknown state = 1× base
    /// with the reason, never a guessed multiplier.
    #[test]
    fn deploy_line_semantics() {
        assert!(deploy_line(0.0, Some(20.0)).is_none());
        assert!(deploy_line(-5.0, Some(20.0)).is_none());
        let near = deploy_line(1000.0, Some(0.0)).unwrap();
        assert!(near.contains("€1000") && near.contains("× 1 ") && near.contains("NEAR-HIGH"));
        let pull = deploy_line(1000.0, Some(5.0)).unwrap();
        assert!(pull.contains("€1500") && pull.contains("× 1.5") && pull.contains("PULLBACK"));
        let dd = deploy_line(1000.0, Some(15.0)).unwrap();
        assert!(dd.contains("€2000") && dd.contains("× 2 ") && dd.contains("DRAWDOWN"));
        let unknown = deploy_line(1000.0, None).unwrap();
        assert!(unknown.contains("€1000") && unknown.contains("unavailable"));
    }

    /// (round 118) cash-line semantics: no cash (no key) prints nothing; with a deploy budget the
    /// line says covers vs by-how-much short; with no budget it just states the balance.
    #[test]
    fn cash_line_semantics() {
        assert!(cash_line(Some(500.0), None).is_none());
        let covers = cash_line(Some(500.0), Some(600.0)).unwrap();
        assert!(covers.contains("free €600") && covers.contains("covers this month's €500 deploy"));
        let short = cash_line(Some(500.0), Some(300.0)).unwrap();
        assert!(short.contains("free €300") && short.contains("€200 short of the €500 deploy"));
        let no_budget = cash_line(None, Some(400.0)).unwrap();
        assert!(no_budget.contains("free €400") && !no_budget.contains("deploy"));
        // zero budget is "unset", same as None → balance only, no verdict
        let zero_budget = cash_line(Some(0.0), Some(400.0)).unwrap();
        assert!(zero_budget.contains("free €400") && !zero_budget.contains("deploy"));
    }

    /// (round 116) order-glue semantics: empty book prints nothing; sized rows split the deploy €
    /// equally (qty = slice ÷ price) across both brokers; missing deploy or missing symbol degrade
    /// to placeholders (never a guessed number), and the unheld footnote only prints when earned.
    #[test]
    fn order_glue_semantics() {
        assert!(order_glue(&[], Some(1000.0)).is_none());
        let rows = vec![
            ("AAPL".to_string(), Some(150.0), "trading212", Some("AAPL_US_EQ".to_string())),
            ("BTC-EUR".to_string(), Some(75000.0), "binance", Some("BTCEUR".to_string())),
        ];
        let sized = order_glue(&rows, Some(300.0)).unwrap();
        assert!(sized.contains("€300 this month ÷ 2 names ≈ €150 each"));
        assert!(sized.contains("folioman trade trading212 buy AAPL_US_EQ 1.0000"));
        assert!(sized.contains("folioman trade binance buy BTCEUR 0.0020"));
        assert!(!sized.contains("<T212_SYMBOL>") && !sized.contains("look it up"));
        let no_deploy = order_glue(&rows, None).unwrap();
        assert!(no_deploy.contains("set monthly_deploy_eur") && no_deploy.contains("<QTY>"));
        let unheld = order_glue(
            &[("SXLK.L".to_string(), Some(156.62), "trading212", None)],
            Some(300.0),
        )
        .unwrap();
        assert!(unheld.contains("<T212_SYMBOL>") && unheld.contains("look it up"));
        assert!(unheld.contains("# SXLK.L @ €156.62"));
        // priceless row can't size a qty even with a deploy set
        let no_price = order_glue(&[("X".to_string(), None, "trading212", None)], Some(100.0)).unwrap();
        assert!(no_price.contains("<QTY>"));
    }

    /// (round 117) symbol resolution priority: owned beats instruments; ISIN-exact prefers the
    /// EUR listing; base match resolves only when unique — two venues on one base stay a
    /// placeholder (never guess a listing for a real-money command).
    #[test]
    fn resolve_t212_priority_and_ambiguity() {
        use crate::broker::trading212::Instrument;
        let inst = |t: &str, isin: &str, ccy: &str| Instrument { ticker: t.to_string(), isin: isin.to_string(), currency: ccy.to_string() };
        let owned = vec!["IITU_GB_EQ".to_string()];
        let instruments = vec![
            inst("IITUx_EQ", "IE00B3WJKG14", "USD"), // distractor — owned entry must win before instruments are consulted
            inst("SXLK_US_EQ", "IE00BWBXM948", "USD"),
            inst("SXLKe_EQ", "IE00BWBXM948", "EUR"), // same ISIN, EUR listing preferred
            inst("AAPL_US_EQ", "US0378331005", "USD"),
            inst("DUAL_US_EQ", "US1111111111", "USD"),
            inst("DUAL_GB_EQ", "GB2222222222", "GBP"), // same base, different ISINs = ambiguous
        ];
        let isin_of: HashMap<String, String> = [("SXLK.L".to_string(), "IE00BWBXM948".to_string())].into();
        // (round 118) each resolution also names its source rung — the stderr report counts them
        assert_eq!(resolve_t212("IITU.L", &owned, &instruments, &isin_of), Some(("IITU_GB_EQ".to_string(), "owned")));
        // venue letter stays lowercase — the paste line must show T212's true form (real-money path)
        assert_eq!(resolve_t212("SXLK.L", &owned, &instruments, &isin_of), Some(("SXLKe_EQ".to_string(), "isin")));
        assert_eq!(resolve_t212("AAPL", &owned, &instruments, &isin_of), Some(("AAPL_US_EQ".to_string(), "base")));
        assert_eq!(resolve_t212("DUAL", &owned, &instruments, &isin_of), None);
        assert_eq!(resolve_t212("UNKNOWN", &owned, &[], &isin_of), None);
    }

    /// (tests round 5) The banner € and the order-glue € ride ONE composition fn — pin its
    /// semantics (knob gate, unknown-state ×1, ladder product) so the two real-money surfaces
    /// can never silently disagree.
    #[test]
    fn deploy_composition_single_source() {
        assert!(deploy_scaled_eur(0.0, Some(20.0)).is_none());
        assert!(deploy_scaled_eur(-1.0, None).is_none());
        assert_eq!(deploy_scaled_eur(1000.0, None), Some((1.0, 1000.0)));
        assert_eq!(deploy_scaled_eur(1000.0, Some(0.0)), Some((1.0, 1000.0)));
        assert_eq!(deploy_scaled_eur(1000.0, Some(5.0)), Some((1.5, 1500.0)));
        assert_eq!(deploy_scaled_eur(1000.0, Some(15.0)), Some((2.0, 2000.0)));
        // the glue total IS the number the banner prints — same fn, and same value end to end
        let (_, total) = deploy_scaled_eur(800.0, Some(7.0)).expect("set knob must size");
        assert!(
            deploy_line(800.0, Some(7.0)).expect("same inputs must print").contains(&format!("€{total:.0}")),
            "banner and glue disagree on the deploy total"
        );
    }

    /// (round 116) the multiplier ladder tracks the classifier's exact boundaries.
    #[test]
    fn deploy_multiplier_boundaries() {
        assert_eq!(deploy_multiplier(0.0), 1.0);
        assert_eq!(deploy_multiplier(4.9), 1.0);
        assert_eq!(deploy_multiplier(5.0), 1.5);
        assert_eq!(deploy_multiplier(14.9), 1.5);
        assert_eq!(deploy_multiplier(15.0), 2.0);
    }

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

    /// (crossover) a fund sharing held stocks prints them with summed weight (heaviest overlap
    /// first); Yahoo symbols normalize to broker bases (NVDA matches nvda); zero-weight rows and
    /// funds sharing nothing stay silent; empty held set -> no output at all.
    #[test]
    fn crossover_semantics() {
        let mut h: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        h.insert("TECH.L".into(), vec![("NVDA".into(), 0.20), ("AAPL".into(), 0.17), ("MSFT".into(), 0.11)]);
        h.insert("SMALL.DE".into(), vec![("NVDA".into(), 0.05), ("ZZZ".into(), 0.30)]);
        h.insert("CLEAN.DE".into(), vec![("ZZZ".into(), 0.40)]); // shares nothing -> silent
        h.insert("NOW8.DE".into(), vec![("NVDA".into(), 0.0)]); // weight omitted -> silent
        let held: HashSet<String> = ["nvda".to_string(), "aapl".to_string()].into();
        let lines = crossover_lines(&h, &held);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  TECH.L") && lines[0].contains("NVDA 20% + AAPL 17% = 37% of the fund"));
        assert!(lines[1].starts_with("  SMALL.DE") && lines[1].contains("NVDA 5% = 5% of the fund"));
        assert!(!lines.iter().any(|l| l.contains("MSFT")), "unheld holding must not print: {lines:?}");
        assert!(crossover_lines(&h, &HashSet::new()).is_empty());
    }

    /// (sector tilt) heaviest top sector prints first, top-2 sectors only, the bond leg shows only
    /// when it exists (equity funds report bond 0), and a fund without sector data stays silent.
    #[test]
    fn sector_tilt_semantics() {
        let mut m: HashMap<String, fetch::FundMix> = HashMap::new();
        m.insert(
            "TECH.L".into(),
            (vec![("Technology".into(), 0.99), ("Communication Services".into(), 0.01)], Some((1.0, 0.0)), None),
        );
        m.insert(
            "MIXED.DE".into(),
            (
                vec![("Financial Services".into(), 0.40), ("Industrials".into(), 0.30), ("Energy".into(), 0.20)],
                Some((0.60, 0.40)),
                None,
            ),
        );
        m.insert("NOSECTORS.L".into(), (Vec::new(), Some((1.0, 0.0)), None)); // silent
        let lines = sector_tilt_lines(&m);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  TECH.L") && lines[0].contains("Technology 99% · Communication Services 1%"));
        assert!(!lines[0].contains("bond"), "pure-equity fund must not print a bond leg: {}", lines[0]);
        assert!(lines[1].contains("Financial Services 40% · Industrials 30%"), "top-2 cap: {}", lines[1]);
        assert!(!lines[1].contains("Energy"), "third sector must drop: {}", lines[1]);
        assert!(lines[1].contains("(equity 60% / bond 40%)"), "real bond leg prints: {}", lines[1]);
        assert!(sector_tilt_lines(&HashMap::new()).is_empty());
    }

    /// (r11) vs-SPY premium: longest SHARED leg wins (10Y before 5Y), premium = name CAGR minus
    /// index CAGR (index-beater must print POSITIVE — the sign the footer ranks on), young name
    /// falls back to the shared 5Y leg, and no shared long leg = None (no claim).
    #[test]
    fn spy_premium_semantics() {
        let q = |legs: &[(&str, f64)]| {
            let mut quote = Quote::stub("T", "1", "", "T");
            quote.perf = core::HORIZONS
                .iter()
                .map(|(l, _)| legs.iter().find(|(pl, _)| pl == l).map(|(_, v)| (l.to_string(), *v)))
                .collect();
            quote
        };
        let spx = q(&[("10Y", 150.0), ("5Y", 40.0)]);
        let (p, leg) = spy_premium(&q(&[("10Y", 300.0), ("5Y", 80.0)]), &spx).unwrap();
        assert_eq!(leg, "10Y"); // longest shared leg preferred even when 5Y is also shared
        let want = core::cagr(300.0, 10.0) - core::cagr(150.0, 10.0);
        assert!((p - want).abs() < 1e-9, "premium {p} != cagr diff {want}");
        assert!(p > 0.0, "index-beating name must print a POSITIVE premium, got {p}");
        let (p5, leg5) = spy_premium(&q(&[("5Y", 80.0)]), &spx).unwrap();
        assert_eq!(leg5, "5Y"); // young name: no 10Y leg -> falls back to shared 5Y
        assert!((p5 - (core::cagr(80.0, 5.0) - core::cagr(40.0, 5.0))).abs() < 1e-9);
        assert_eq!(spy_premium(&q(&[("1Y", 20.0)]), &spx), None); // no shared long leg
    }

    /// (r11) calendar strip cells: `{year} {ret:+.0}%` dot-joined, and max_years keeps the LAST
    /// (most recent) years — dropping the newest instead is the truncation bug this pins.
    #[test]
    fn year_cells_semantics() {
        let yrs = [(2022, -30.0), (2023, 45.2), (2024, 9.8)];
        assert_eq!(year_cells(&yrs, 8), "2022 -30% · 2023 +45% · 2024 +10%");
        assert_eq!(year_cells(&yrs, 2), "2023 +45% · 2024 +10%");
        assert_eq!(year_cells(&[], 8), "");
    }

    /// (r13) benchmark footer feed: RANK order preserved, benchless quotes and unknown ranked
    /// tickers skipped silently (venue/regulatory-only funds carry no BF bench — no claim).
    #[test]
    fn bench_rows_semantics() {
        let q = |t: &str, b: Option<&str>| {
            let mut quote = Quote::stub(t, "1", "", t);
            quote.benchmark = b.map(str::to_string);
            quote
        };
        let quotes = vec![
            q("A.L", Some("s&p 500 information technology")),
            q("B.L", None),
            q("C.L", Some("nasdaq-100")),
        ];
        let ranked = ["C.L", "B.L", "A.L", "GHOST.L"].map(String::from);
        // C before A = rank order, not quotes order; B (benchless) + GHOST (unknown) skipped
        assert_eq!(
            bench_rows(&ranked, &quotes),
            vec![("C.L", "nasdaq-100"), ("A.L", "s&p 500 information technology")]
        );
        assert!(bench_rows(&[], &quotes).is_empty());
    }

    /// (r19) twin grouping: same-bench funds grouped best-compounder-first, singletons dropped
    /// (a solo fund is no comparison), benchless and no-5Y quotes skipped — no claim either way.
    #[test]
    fn twin_groups_semantics() {
        let q = |t: &str, bench: Option<&str>, five_y: Option<f64>| {
            let mut quote = Quote::stub(t, "1", "", t);
            quote.benchmark = bench.map(str::to_string);
            quote.perf = vec![None; core::HORIZONS.len()];
            let i5 = core::HORIZONS.iter().position(|(l, _)| *l == "5Y").unwrap();
            quote.perf[i5] = five_y.map(|p| (String::new(), p));
            quote
        };
        let quotes = vec![
            q("NASD.L", Some("nasdaq-100"), Some(61.6)),
            q("CSNDX.SW", Some("nasdaq-100"), Some(59.9)),
            q("ANX.PA", Some("nasdaq-100"), Some(60.8)),
            q("SXLK.L", Some("s&p u.s. technology select"), Some(77.0)), // singleton bench
            q("NOB.L", None, Some(50.0)),                                // benchless
            q("YOUNG.DE", Some("nasdaq-100"), None),                     // <5y history
        ];
        let names: Vec<String> =
            ["NASD.L", "CSNDX.SW", "ANX.PA", "SXLK.L", "NOB.L", "YOUNG.DE"].map(String::from).into();
        let groups = twin_groups(&names, &quotes);
        // one group survives: the singleton s&p bench, the benchless and the no-5Y all drop
        assert_eq!(groups.len(), 1);
        let (bench, twins) = &groups[0];
        assert_eq!(*bench, "nasdaq-100");
        // best compounder first — the order the header promises
        assert_eq!(twins, &[("NASD.L", 61.6), ("ANX.PA", 60.8), ("CSNDX.SW", 59.9)]);
        assert!(twin_groups(&[], &quotes).is_empty());
    }

    /// (r20) headline rows: input order preserved, empty-title and missing names skipped (a
    /// headless name must never print a blank "no news" row), and exact-duplicate titles merge
    /// into one row — Yahoo's generic-story fallback hands several names the same headline.
    #[test]
    fn headline_rows_semantics() {
        let generic = "The Average 401(k) Is Misleading You".to_string();
        let titles: std::collections::BTreeMap<String, String> = [
            ("NVDA".to_string(), "Nvidia ships new chip".to_string()),
            ("VUAA.DE".to_string(), generic.clone()),
            ("SPYL.DE".to_string(), generic.clone()),
            ("HEADLESS.L".to_string(), String::new()), // fetched but headless
        ]
        .into();
        let names = ["VUAA.DE", "MISSING.L", "HEADLESS.L", "NVDA", "SPYL.DE"].map(String::from);
        // VUAA row first = input order; SPYL merges into it; MISSING + HEADLESS skipped
        assert_eq!(
            headline_rows(&names, &titles),
            vec![
                (vec!["VUAA.DE".to_string(), "SPYL.DE".to_string()], generic),
                (vec!["NVDA".to_string()], "Nvidia ships new chip".to_string())
            ]
        );
        assert!(headline_rows(&[], &titles).is_empty());
    }

    /// (r14) hedged detection: whole-word only — "UnHedged"/"Hedge" must never flag (the
    /// substring bug this pins), name OR bench source both count, rank order preserved.
    #[test]
    fn hedged_names_semantics() {
        let q = |t: &str, name: &str, bench: Option<&str>| {
            let mut quote = Quote::stub(t, "1", "", name);
            quote.benchmark = bench.map(str::to_string);
            quote
        };
        let quotes = vec![
            q("N.L", "iShares MSCI World EUR Hedged UCITS ETF", None), // name carries it
            q("B.L", "Amundi S&P 500 Acc", Some("s&p 500 eur hedged")), // bench carries it
            q("U.L", "WisdomTree UnHedged Global", Some("msci world unhedged")), // never flags
            q("H.L", "Man Hedge Fund Strategies", None), // "Hedge" word ≠ "hedged"
            q("C.L", "Vanguard S&P 500", Some("s&p 500")), // clean
        ];
        let ranked = ["B.L", "U.L", "H.L", "N.L", "C.L"].map(String::from);
        assert_eq!(hedged_names(&ranked, &quotes), vec!["B.L", "N.L"]); // rank order, not quotes order
        assert!(hedged_names(&[], &quotes).is_empty());
        assert!(has_hedged_token("EUR Hedged (Acc)"));
        assert!(!has_hedged_token("unhedged"));
    }

    /// (r15) footer population = ranked ∪ pinned: ranked order first, pinned extras appended in
    /// watchlist order, a pinned name that RANKED appears once (and un-starred). The star glyph
    /// marks only the extras — the rows the ranking would otherwise hide from every footer.
    #[test]
    fn footer_names_semantics() {
        let ranked = ["A.L", "B.L"].map(String::from);
        let pinned = ["P.DE", "B.L", "Q.DE"].map(String::from); // B.L ranked AND pinned
        assert_eq!(footer_names(&ranked, &pinned), ["A.L", "B.L", "P.DE", "Q.DE"].map(String::from));
        assert_eq!(footer_names(&ranked, &[]), ranked.to_vec()); // no pins = old behavior exactly
        assert_eq!(footer_names(&[], &pinned), pinned.to_vec()); // pins survive an empty ranking
        assert_eq!(footer_label("A.L", &ranked), "A.L"); // ranked prints bare
        assert_eq!(footer_label("B.L", &ranked), "B.L"); // pinned-but-ranked too
        assert_eq!(footer_label("P.DE", &ranked), "P.DE*"); // extras carry the table's glyph
    }

    /// (round 28) persistent leaders: a name in ≥80% of past top-10s is kept (0.8 boundary IN,
    /// 0.6 OUT); a name that only ever sat at rank 11+ is NOT counted (the book is the top-10);
    /// a brand-new name (0 past appearances) is out; input/rank order is preserved; empty past
    /// (no history) → empty.
    #[test]
    fn persistent_leaders_semantics() {
        use crate::commands::track::Snapshot;
        // Each snapshot: `lead` names first, padded with fillers to 10, then DEEP at rank 11 —
        // so DEEP is present-but-below-the-book in EVERY snapshot (capped 0.0, uncapped 1.0).
        let snap = |date: &str, lead: &[&str]| {
            let mut rows: Vec<(String, Option<f64>)> =
                lead.iter().map(|t| (t.to_string(), Some(1.0))).collect();
            let mut f = 0;
            while rows.len() < crate::commands::track::BOOK {
                rows.push((format!("PAD{f}"), Some(1.0)));
                f += 1;
            }
            rows.push(("DEEP".to_string(), Some(1.0))); // rank 11 — past the book cut
            Snapshot { date: date.into(), spx: None, spx_off_hi: None, aum: Vec::new(), rows }
        };
        // ALL: 5/5 (=1.0) · MOST: 4/5 (=0.8 boundary) · HALF: 3/5 (=0.6) · DEEP: rank-11 in all 5
        let past = vec![
            snap("2026-06-01", &["ALL", "MOST", "HALF"]),
            snap("2026-06-02", &["ALL", "MOST", "HALF"]),
            snap("2026-06-03", &["ALL", "MOST", "HALF"]),
            snap("2026-06-04", &["ALL", "MOST"]),
            snap("2026-06-05", &["ALL"]),
        ];
        // today's top, deliberately NOT in the past-frequency order, to prove input order is kept;
        // DEEP is IN today's list, so the book cap is what keeps it out of the result.
        let today = ["HALF", "ALL", "NEW", "DEEP", "MOST"].map(String::from);
        // 0.8: ALL(1.0) + MOST(0.8 boundary IN); HALF(0.6) out, NEW(0) out, DEEP(capped 0) out
        assert_eq!(persistent_leaders(&today, &past, 0.8), vec!["ALL".to_string(), "MOST".to_string()]);
        // 0.6: HALF clears the lower bar; DEEP still never does (rank 11 is past the book cut)
        assert_eq!(
            persistent_leaders(&today, &past, 0.6),
            vec!["HALF".to_string(), "ALL".to_string(), "MOST".to_string()]
        );
        assert!(persistent_leaders(&today, &[], 0.8).is_empty()); // no history → no claim
    }

    /// (round 29) rank trend: a name whose later-half mean rank beats its earlier-half by ≥ band is
    /// climbing (first→last evidence); the reverse fades; a flat name (within the deadband) is
    /// neither; < min_pts appearances gets no claim; a name below BOOK contributes no point; an
    /// absent name is skipped; today-order is preserved within each group.
    #[test]
    fn rank_trend_semantics() {
        use crate::commands::track::Snapshot;
        // place each (name, rank) at index rank-1; every other slot is a unique pad, so a name
        // really sits at the given rank and unlisted names are absent. Book = the top-BOOK slice.
        let snap = |date: &str, at: &[(&str, usize)]| {
            let len = at
                .iter()
                .map(|(_, r)| *r)
                .max()
                .unwrap_or(0)
                .max(crate::commands::track::BOOK);
            let mut rows: Vec<(String, Option<f64>)> =
                (1..=len).map(|i| (format!("{date}_PAD{i}"), Some(1.0))).collect();
            for (n, r) in at {
                rows[*r - 1] = (n.to_string(), Some(1.0));
            }
            Snapshot { date: date.into(), spx: None, spx_off_hi: None, aum: Vec::new(), rows }
        };
        // UP [8,7,5,3] climbs · UP2 [9,6,4,2] climbs · DOWN [2,3,6,7] fades · FLAT [10×4] flat ·
        // THIN present only twice (<3) · BELOW always at rank 12 (past the top-10 cut → no point)
        let journal = vec![
            snap("2026-06-01", &[("UP", 8), ("UP2", 9), ("DOWN", 2), ("FLAT", 10), ("BELOW", 12)]),
            snap("2026-06-02", &[("UP", 7), ("UP2", 6), ("DOWN", 3), ("FLAT", 10), ("THIN", 8), ("BELOW", 12)]),
            snap("2026-06-03", &[("UP", 5), ("UP2", 4), ("DOWN", 6), ("FLAT", 10), ("THIN", 2), ("BELOW", 12)]),
            snap("2026-06-04", &[("UP", 3), ("UP2", 2), ("DOWN", 7), ("FLAT", 10), ("BELOW", 12)]),
        ];
        // scrambled today order proves grouping keeps today-order, not journal or strength order
        let today = ["UP", "FLAT", "UP2", "DOWN", "THIN", "BELOW", "NEW"].map(String::from);
        let (up, down) = rank_trend(&today, &journal, 3, 1.0);
        // climbers in today-order; first→last observed rank as evidence
        assert_eq!(up, vec![("UP".to_string(), 8, 3), ("UP2".to_string(), 9, 2)]);
        // fader; FLAT (deadband) / THIN (<3 appearances) / BELOW (below book) / NEW (absent) excluded
        assert_eq!(down, vec![("DOWN".to_string(), 2, 7)]);
    }

    /// (round 30) book stability: averaged top-BOOK name-retention between consecutive screens.
    /// A fully-held book scores 1.0; a two-of-three churn scores 2/3; a smaller book is scored
    /// against its own size (min denominator), so all of a short book carrying over is still 1.0;
    /// a name past the top-BOOK cut can't affect the score; < 2 screens yields no number.
    #[test]
    fn book_stability_semantics() {
        use crate::commands::track::Snapshot;
        let snap = |date: &str, names: &[&str]| Snapshot {
            date: date.into(),
            spx: None,
            spx_off_hi: None,
            rows: names.iter().map(|t| (t.to_string(), Some(1.0))).collect(),
            aum: Vec::new(),
        };
        // fully stable: same top set across 3 screens → every pair retains all → 1.0
        let stable = vec![
            snap("d1", &["A", "B", "C"]),
            snap("d2", &["A", "B", "C"]),
            snap("d3", &["A", "B", "C"]),
        ];
        assert_eq!(book_stability(&stable), Some(1.0));
        // partial churn: each pair keeps {A,B} of 3 → 2/3 on both pairs → 2/3
        let churn = vec![
            snap("d1", &["A", "B", "C"]),
            snap("d2", &["A", "B", "D"]),
            snap("d3", &["A", "B", "E"]),
        ];
        assert!((book_stability(&churn).unwrap() - 2.0 / 3.0).abs() < 1e-9);
        // smaller book fully carried: {A,B} ⊂ {A,B,C,D} → 2/min(2,4)=2/2 → 1.0, not 2/4
        let grow = vec![snap("d1", &["A", "B"]), snap("d2", &["A", "B", "C", "D"])];
        assert_eq!(book_stability(&grow), Some(1.0));
        // BOOK cap: two 11-name books identical in the top 10, differing only at rank 11 → the
        // 11th name is past the cut, so retention is a full 10/10 → 1.0 (not 10/11)
        let ten: Vec<String> =
            (0..crate::commands::track::BOOK).map(|i| format!("N{i}")).collect();
        let mut a: Vec<&str> = ten.iter().map(String::as_str).collect();
        let mut b = a.clone();
        a.push("X11");
        b.push("Y11");
        let capped = vec![snap("d1", &a), snap("d2", &b)];
        assert_eq!(book_stability(&capped), Some(1.0));
        // < 2 screens → no pair to compare → None
        assert_eq!(book_stability(&[snap("d1", &["A"])]), None);
    }

    /// (round 31) mean book rank: a name's average top-BOOK position across the journal, best-seated
    /// first. A durably-high name (mean #2) sorts before a volatile one (mean #5); < min_pts
    /// appearances is excluded; an absent name is excluded; a name past the top-BOOK cut contributes
    /// no point; the output is mean-sorted, not today-sorted.
    #[test]
    fn mean_ranks_semantics() {
        use crate::commands::track::Snapshot;
        // place each (name, rank) at index rank-1, every other slot a unique pad, top-BOOK slice
        let snap = |date: &str, at: &[(&str, usize)]| {
            let len = at
                .iter()
                .map(|(_, r)| *r)
                .max()
                .unwrap_or(0)
                .max(crate::commands::track::BOOK);
            let mut rows: Vec<(String, Option<f64>)> =
                (1..=len).map(|i| (format!("{date}_PAD{i}"), Some(1.0))).collect();
            for (n, r) in at {
                rows[*r - 1] = (n.to_string(), Some(1.0));
            }
            Snapshot { date: date.into(), spx: None, spx_off_hi: None, aum: Vec::new(), rows }
        };
        // A durably #2 (mean 2.0) · B bounces 1/5/9 (mean 5.0) · C only twice (< 3 appearances) ·
        // E always rank 12 (past the top-10 cut → no point) · D never appears
        let journal = vec![
            snap("2026-06-01", &[("A", 2), ("B", 1), ("C", 3), ("E", 12)]),
            snap("2026-06-02", &[("A", 2), ("B", 5), ("C", 3), ("E", 12)]),
            snap("2026-06-03", &[("A", 2), ("B", 9), ("E", 12)]),
        ];
        // scrambled today order proves the output is mean-sorted, not today-sorted
        let today = ["B", "E", "C", "A", "D"].map(String::from);
        let got = mean_ranks(&today, &journal, 3);
        // A (mean 2.0) before B (mean 5.0); C (< 3), D (absent), E (below book) all excluded
        assert_eq!(got, vec![("A".to_string(), 2.0, 3), ("B".to_string(), 5.0, 3)]);
    }

    /// (round 34) fund flow: net shares created/redeemed with price appreciation divided OUT of AUM
    /// growth. AUM 2× while price +25% → (2.0/1.25)−1 = +60% inflow; AUM −20% while price flat →
    /// −20% outflow (bleeding); a name with < 2 AUM+close points is excluded; a non-fund (None AUM)
    /// is excluded; an absent name is excluded; output is flow-sorted (biggest inflow first); a
    /// single-snapshot (or empty) journal yields nothing.
    #[test]
    fn fund_flow_semantics() {
        use crate::commands::track::Snapshot;
        // (ticker, close, aum) per snapshot — None aum = a non-fund row (stock/crypto)
        let snap = |date: &str, at: &[(&str, Option<f64>, Option<f64>)]| Snapshot {
            date: date.into(),
            spx: None,
            spx_off_hi: None,
            rows: at.iter().map(|(t, c, _)| (t.to_string(), *c)).collect(),
            aum: at.iter().map(|(t, _, a)| (t.to_string(), *a)).collect(),
        };
        let journal = vec![
            snap(
                "2026-06-01",
                &[
                    ("INFLOW", Some(10.0), Some(100.0)),
                    ("OUTFLOW", Some(20.0), Some(100.0)),
                    ("STOCK", Some(5.0), None),
                ],
            ),
            snap(
                "2026-06-02",
                &[
                    // aum 100→200 (2×) vs close 10→12.5 (+25%) → (2.0/1.25)−1 = +0.60
                    ("INFLOW", Some(12.5), Some(200.0)),
                    // aum 100→80 (−20%) vs close flat → 0.8−1 = −0.20
                    ("OUTFLOW", Some(20.0), Some(80.0)),
                    ("STOCK", Some(6.0), None), // no AUM either end → excluded
                    ("ONESHOT", Some(1.0), Some(50.0)), // only one AUM point → excluded
                ],
            ),
        ];
        // scrambled today order + an absent name prove filtering and the flow sort
        let today = ["OUTFLOW", "INFLOW", "STOCK", "ONESHOT", "ABSENT"].map(String::from);
        let got = fund_flow_lines(&today, &journal);
        assert_eq!(got.len(), 2); // STOCK (no AUM), ONESHOT (1 pt), ABSENT (never seen) all out
        assert_eq!(got[0].0, "INFLOW"); // +60% sorts above −20%
        assert!((got[0].1 - 60.0).abs() < 1e-9);
        assert_eq!(got[0].2, 2);
        assert_eq!(got[1].0, "OUTFLOW");
        assert!((got[1].1 - (-20.0)).abs() < 1e-9);
        // a flow needs two points: empty and single-snapshot journals yield nothing
        assert!(fund_flow_lines(&today, &[]).is_empty());
        assert!(fund_flow_lines(&today, &journal[..1]).is_empty());
    }

    /// (r15) T212 orderability: flagged = known-ISIN AND absent from the catalog, input order.
    /// No cached ISIN = no claim (skipped); empty catalog (keyless run) = always silent.
    #[test]
    fn t212_missing_semantics() {
        let isin_of: HashMap<String, String> = [("A.L", "IE00A"), ("B.L", "IE00B"), ("C.L", "IE00C")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into();
        let catalog: HashSet<String> = ["IE00B"].map(String::from).into();
        let names = ["A.L", "B.L", "C.L", "NOISIN.L"].map(String::from);
        assert_eq!(t212_missing(&names, &isin_of, &catalog), vec!["A.L", "C.L"]); // B carried, NOISIN no claim
        assert!(t212_missing(&names, &isin_of, &HashSet::new()).is_empty()); // keyless = silent
        assert!(t212_missing(&["B.L".into()], &isin_of, &catalog).is_empty()); // all orderable = silent
    }

    /// (r16) fund-survival flag: under €100M flagged (BF aum_eur or Yahoo aum_fallback via
    /// aum_shown), at/over silent, AUM-less names (stocks/crypto) skipped — no claim.
    #[test]
    fn small_aum_semantics() {
        let mut tiny = Quote::stub("TINY.L", "1", "", "Micro Fund");
        tiny.aum_eur = Some(36e6);
        let mut big = Quote::stub("BIG.L", "1", "", "Mega Fund");
        big.aum_eur = Some(16e9);
        let mut fb = Quote::stub("FB.L", "1", "", "Fallback Fund");
        fb.aum_fallback = Some(9e7); // Yahoo fallback counts too (aum_shown)
        let none = Quote::stub("NVDA", "1", "", "Stock No Aum");
        let quotes = vec![tiny, big, fb, none];
        let names = ["BIG.L", "TINY.L", "FB.L", "NVDA"].map(String::from);
        let got = small_aum_names(&names, &quotes);
        assert_eq!(got.len(), 2); // input order preserved
        assert_eq!(got[0].0, "TINY.L");
        assert_eq!(got[1].0, "FB.L");
        assert!((got[0].1 - 36e6).abs() < 1.0);
        assert!(small_aum_names(&["BIG.L".into(), "NVDA".into()], &quotes).is_empty()); // clean book = silent
    }

    /// (fund valuation) cheapest-first ordering, pe-less funds silent, empty map -> None. The
    /// values are post-inversion real ratios (fetch-side pin owns the reciprocal trap).
    #[test]
    fn fund_pe_line_semantics() {
        let mut m: HashMap<String, fetch::FundMix> = HashMap::new();
        m.insert("IITU.L".into(), (Vec::new(), None, Some(33.93)));
        m.insert("SPYL.DE".into(), (Vec::new(), None, Some(24.2)));
        m.insert("NOPE.L".into(), (Vec::new(), None, None)); // silent
        assert_eq!(fund_pe_line(&m).unwrap(), "SPYL.DE 24 · IITU.L 34");
        assert_eq!(fund_pe_line(&HashMap::new()), None);
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
