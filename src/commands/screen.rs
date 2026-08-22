//! `screen [TICKERS]` — scan a LIVE universe (top-N crypto from CoinGecko + S&P 500
//! constituents, see `fetch::fetch_universe`; `screen TICKER...` overrides) and rank the
//! 20yr+ buy-and-hold growth candidates per asset class (stocks / ETFs / crypto). The
//! growth lane is the only one with a validated forward edge (walk-forward rho +0.26,
//! top-vs-bottom-half +108 pts); the old on-sale / ATH-ATL / fallers / dividend tables
//! were dropped — their selection edge was zero-to-negative for a multi-decade hold.

use crate::core::Quote;
use crate::picks::{
    eu_buyable, exit_review_lines, gate_failures, growth_down_year_miss, growth_near_miss, growth_score, render,
    RenderCtx,
};
use crate::{config, core, fetch, picks};
use futures::stream::StreamExt;

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
        .filter(|(_, (sectors, _, _, _))| !sectors.is_empty())
        .map(|(t, (sectors, sb, _, _))| (t, sectors, *sb))
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

/// (#112 / round 3 §9) The book's CURRENCY exposure — one unit per row, funds looked THROUGH to the
/// top-10 holdings already fetched for `fund_pe` instead of read off their own listing.
///
/// The listing is the exact trap this exists to spring. `print_lane`'s `market mix` counts listing
/// COUNTRY over the stocks lane, and for a fund that is not merely incomplete but backwards: a
/// Xetra-quoted S&P 500 tracker reads "Germany" and is 100% dollar exposure. So a fund we hold no
/// holdings for contributes to `?`, never to its own quote currency — an honest blank beats a
/// confident wrong number, and how big `?` is decides how much of the rest is worth believing.
///
/// EQUAL WEIGHT per row, matching `market mix`'s counts rather than inventing a weighting: nothing
/// on this surface knows position sizes (`size` decides those, separately and later), so a weighted
/// mix would be reporting weights it made up. A fund's slice is its top-10 mix RENORMALISED to one
/// unit — an estimate of the whole fund from the part we can see, which is why the caller prints the
/// coverage next to it.
///
/// Crypto is its own bucket. A coin's `-EUR`/`-USD` suffix is a quote convention, not an exposure:
/// a EUR holder of BTC carries bitcoin risk, and folding it into USD would overstate the dollar
/// slice with the one row that has no currency risk at all.
fn currency_mix(
    book: &[String],
    quotes: &[Quote],
    holdings: &std::collections::HashMap<String, Vec<(String, f64)>>,
) -> Vec<(String, f64)> {
    let mut acc: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut rows = 0.0;
    for ticker in book {
        let Some(quote) = quotes.iter().find(|q| &q.ticker == ticker) else { continue };
        rows += 1.0;
        let mut add = |ccy: String, share: f64| *acc.entry(ccy).or_insert(0.0) += share;
        if picks::is_currency_quoted(&quote.ticker) {
            add("crypto".to_string(), 1.0);
            continue;
        }
        // weights arrive as fractions and Yahoo sometimes omits them entirely (all 0.0) — a fund
        // whose top-10 sums to nothing is a fund with no look-through, not one that is 0% everything
        let seen = holdings.get(ticker).map(|hs| (hs, hs.iter().map(|(_, w)| w).sum::<f64>()));
        match seen {
            Some((hs, total)) if total > 0.0 && picks::quote_is_etf(quote) => {
                for (sym, w) in hs {
                    add(core::listing_currency(sym).unwrap_or("?").to_string(), w / total);
                }
            }
            _ if picks::quote_is_etf(quote) => add("?".to_string(), 1.0),
            // a stock's own quote currency is a real answer straight from Yahoo, so it beats the
            // suffix guess; `to_uppercase` is what collapses LSE's pence quote (GBp) onto GBP, which
            // is the same FX exposure under a different unit
            _ => {
                let ccy = quote
                    .quote_currency
                    .as_deref()
                    .filter(|c| !c.is_empty())
                    .map(str::to_uppercase)
                    .or_else(|| core::listing_currency(&quote.ticker).map(str::to_string))
                    .unwrap_or_else(|| "?".to_string());
                add(ccy, 1.0);
            }
        }
    }
    if rows == 0.0 {
        return Vec::new();
    }
    let mut mix: Vec<(String, f64)> = acc.into_iter().map(|(c, s)| (c, s / rows)).collect();
    mix.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    mix
}

/// (#112) The printed form of [`currency_mix`], plus the one number that says how much of it is
/// measured: the WORST per-fund top-10 coverage in the book, so the claim is `≥N%` and true of every
/// fund rather than an average that hides the thinnest one. No fund in the book -> no coverage
/// clause, because there is nothing extrapolated to disclaim.
fn currency_mix_line(
    book: &[String],
    quotes: &[Quote],
    holdings: &std::collections::HashMap<String, Vec<(String, f64)>>,
) -> Option<String> {
    let mix = currency_mix(book, quotes, holdings);
    if mix.is_empty() {
        return None;
    }
    let parts = mix.iter().map(|(c, s)| format!("{c} {:.0}%", 100.0 * s)).collect::<Vec<_>>().join(", ");
    let cover = book
        .iter()
        .filter_map(|t| holdings.get(t))
        .map(|hs| hs.iter().map(|(_, w)| w).sum::<f64>())
        .filter(|s| *s > 0.0)
        .fold(f64::INFINITY, f64::min);
    let note = if cover.is_finite() {
        format!(
            " — look-through covers ≥{:.0}% of each fund's weight, so read it as directional, not as a hedge ratio",
            100.0 * cover
        )
    } else {
        String::new()
    };
    Some(format!(
        "Book currency exposure (equal weight per row; funds looked THROUGH their holdings, not read off their listing): {parts}{note}"
    ))
}

/// (funnel) Where the growth lane's candidates died: per gate, how many names failed it and how many
/// it blocked ALONE, split stocks | ETFs | crypto, naming a few of the sole-blocked tickers.
///
/// The SOLE-BLOCKER column is the only one that decides anything. A gate can reject thousands of
/// names and still cost the table nothing, because every one of them also fails something else —
/// loosening it then buys zero rows. Raw fail counts cannot tell you that, and they are what an
/// eyeball on the config sees, so the tightest-LOOKING knob is routinely mistaken for the binding
/// one. This block exists to answer "which knob would actually give me more rows" with a number.
///
/// Names are NOT filtered to close misses, unlike the near-miss tail printed after it: the point is
/// to see the gross single-gate rejects too, which that tail deliberately hides.
///
/// They are ordered BY CLASS — stocks · ETFs · crypto, matching the three numeric cells left of them
/// — because a flat alphabetical sort buried the one sole-blocked stock on the `cagr` row third in a
/// list of ETFs. `NAME_CAP` is shared across the three, spent in that order, so a row with more than
/// `NAME_CAP` sole-blockers shows nothing for its LAST class: today `history` only, whose ~1900 ETFs
/// eat the budget and leave its ~19 coins unsampled. The cells still carry every count regardless.
///
/// One row reads differently by construction: `history` always shows sole == fail, because
/// `gate_failures` returns that reason ALONE and stops — nothing else was even measured. It is an
/// honest single-gate death, just not one any knob in `buy_heuristic` can move (only `history_proxy`).
///
/// `cleared` counts names clearing every GATE — an upper bound on printed rows, not a row count: the
/// score floor, sector filter, fund-PEG trim, currency dedup and `top_picks` cap all sit downstream.
///
/// Reads `gate_failures` only — pure, one pass, no fetch. Empty input prints nothing.
fn funnel_lines(quotes: &[core::Quote], tuning: &config::BuyHeuristic) -> Vec<String> {
    const NAME_CAP: usize = 26; // enough to recognise the cohort, short enough to stay one line
    let class_of = |q: &core::Quote| {
        if picks::is_currency_quoted(&q.ticker) {
            2
        } else if picks::quote_is_etf(q) {
            1
        } else {
            0
        }
    };
    let mut fails: std::collections::HashMap<&'static str, [usize; 3]> = std::collections::HashMap::new();
    let mut sole: std::collections::HashMap<&'static str, [usize; 3]> = std::collections::HashMap::new();
    // (class, ticker) so the sample sorts class-major from the same sort_unstable — see the doc above
    let mut blocked: std::collections::HashMap<&'static str, Vec<(usize, &str)>> = std::collections::HashMap::new();
    let mut refused: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    let (mut cleared, mut failed) = (0usize, 0usize);

    for q in quotes {
        let class = class_of(q);
        match gate_failures(q, tuning) {
            // structural reject, or (no structural reason) the missing-1Y bail one line further in
            None => *refused.entry(picks::refusal_reason(q).unwrap_or("no-1Y")).or_default() += 1,
            Some(f) if f.is_empty() => cleared += 1,
            Some(f) => {
                failed += 1;
                for (gate, ..) in &f {
                    fails.entry(gate).or_default()[class] += 1;
                }
                if let [(gate, ..)] = f[..] {
                    sole.entry(gate).or_default()[class] += 1;
                    blocked.entry(gate).or_default().push((class, &q.ticker));
                }
            }
        }
    }
    if quotes.is_empty() {
        return Vec::new();
    }
    let total = |m: &std::collections::HashMap<&'static str, [usize; 3]>, g: &str| m.get(g).map_or(0, |c| c.iter().sum::<usize>());
    let mut gates: Vec<&'static str> = fails.keys().copied().collect();
    // sole-blockers first (the actionable order), fails as tiebreak, name last so runs are stable
    gates.sort_by(|a, b| total(&sole, b).cmp(&total(&sole, a)).then_with(|| total(&fails, b).cmp(&total(&fails, a))).then_with(|| a.cmp(b)));

    let mut out = vec![
        "Gate funnel — where the growth candidates died (failed / blocked by THIS gate alone):".to_string(),
        format!("  {:<10}{:>12}{:>12}{:>12}   sole-blocked", "gate", "stocks", "ETFs", "crypto"),
    ];
    for gate in gates {
        let (f, s) = (fails[gate], sole.get(gate).copied().unwrap_or_default());
        let cell = |i: usize| format!("{} / {}", f[i], s[i]);
        let mut names = blocked.remove(gate).unwrap_or_default();
        names.sort_unstable();
        let more = names.len().saturating_sub(NAME_CAP);
        let mut shown = String::new();
        let mut prev: Option<usize> = None;
        for (class, ticker) in names.iter().take(NAME_CAP) {
            shown.push_str(match prev {
                None => "",
                Some(p) if p == *class => ", ",
                _ => " · ", // class boundary: stocks · ETFs · crypto, the dot the `refused` line uses
            });
            shown.push_str(ticker);
            prev = Some(*class);
        }
        let tail = if more > 0 { format!(" +{more}") } else { String::new() };
        out.push(format!("  {gate:<10}{:>12}{:>12}{:>12}   {shown}{tail}", cell(0), cell(1), cell(2)));
    }
    let refused_n: usize = refused.values().sum();
    if refused_n > 0 {
        out.push(format!("  refused (not assessable): {}", refused.iter().map(|(why, n)| format!("{why} {n}")).collect::<Vec<_>>().join(" · ")));
    }
    // the reconciliation line: a funnel whose arithmetic doesn't close can't be trusted to aim a knob
    out.push(format!("  scanned {} = refused {refused_n} + failed {failed} + cleared {cleared} (cleared ≥ rows printed: score/sector/cap trims come after)", quotes.len()));
    out
}

const MULTI_GATE_CAP: usize = 15; // hardcoded like the near-miss margins — a cosmetic tail, not a tuned knob

/// Tails (C) and (C3): names failing EXACTLY `n` growth gates, every one of them close. Blocks are kept
/// SEPARATE per arity rather than merged because a one-gate name costs one knob to recover, a two-gate
/// name two and a three-gate name three — mixing them hides which are cheap. One fn, one call per block,
/// so the blocks cannot drift apart the way three pasted copies would.
///
/// `empty_note` is emitted when nothing qualifies. `None` = vanish (what the near-miss and two-gate
/// blocks have always done); `Some` is for a block whose whole point is that these names were invisible,
/// where a silently absent block reproduces the ambiguity it exists to remove.
///
/// Returns lines (leading blank included) rather than printing, same as `funnel_lines` — the caller
/// prints, so the dedup/histogram/cap logic below is reachable from a test.
fn multi_gate_lines(quotes: &[Quote], pinned: &[String], tuning: &config::BuyHeuristic, n: usize, lead: &str, empty_note: Option<&str>) -> Vec<String> {
    let mut hits: Vec<(&Quote, Vec<&'static str>, String)> = quotes
        .iter()
        .filter(|q| !pinned.contains(&q.ticker)) // pinned-skip: the gate-review footer covers them
        .filter_map(|q| picks::growth_n_gate_miss(q, tuning, n).map(|(gates, why)| (q, gates, why)))
        .collect();
    // one row per FUND/name (the same UCITS fund lists on several venues) — and dedup BEFORE the
    // histogram so the counts match the rows they summarise.
    let mut seen_names = std::collections::HashSet::new();
    hits.retain(|(q, ..)| seen_names.insert(q.name.to_lowercase()));
    if hits.is_empty() {
        return empty_note.map(|note| vec![String::new(), note.to_string()]).unwrap_or_default();
    }
    // commonest gate COMBINATION first. The histogram is the actionable summary and it survives the cap:
    // "peg & cagr 30" names the knobs doing the work even when the list below is truncated.
    let mut freq: std::collections::HashMap<Vec<&'static str>, usize> = std::collections::HashMap::new();
    for (_, gates, _) in &hits {
        *freq.entry(gates.clone()).or_default() += 1;
    }
    hits.sort_by(|a, b| {
        freq[&b.1].cmp(&freq[&a.1]).then_with(|| a.1.cmp(&b.1)).then_with(|| a.0.ticker.cmp(&b.0.ticker))
    });
    let mut combos: Vec<_> = freq.iter().collect();
    combos.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    // " & " not "+": one gate is literally named `1Y+`, so a `+` join printed `1Y++peg`.
    let hist = combos.iter().take(3).map(|(gates, n)| format!("{} {n}", gates.join(" & "))).collect::<Vec<_>>().join(", ");
    // say when the list is cut — a silent truncation reads as "that's all of them"
    let more = if hits.len() > MULTI_GATE_CAP { format!("top {MULTI_GATE_CAP} of {}; ", hits.len()) } else { String::new() };
    let mut out = vec![String::new(), format!("{lead} ({more}commonest: {hist}):")];
    for (q, _, why) in hits.iter().take(MULTI_GATE_CAP) {
        out.push(format!("  {:<8} {:<28.28} {why}", q.ticker, q.name));
    }
    out
}

const LEG_FLOOR_CAP: usize = 15;

/// Tail (E): every name a long-leg cumulative floor rejects, whatever ELSE it also fails. The one tail
/// with no closeness and no arity filter, because at `growth_min_5y_pct: 75.0` each of those empties the
/// list — the 5Y bar fails 2161 names and sole-blocks zero, and the names worth seeing (AMZN +24.7% vs
/// +75) miss it grossly. Every block above needs a narrow miss, a small failure count, or both, so the
/// tightest live gate in the tool had no name-level table at all.
///
/// Sorted by long CAGR DESCENDING, same trick tail (D) uses: index trackers have mediocre CAGR by
/// construction, so they sink below the cap unaided and the cap trims trackers instead of the answer —
/// no lane special-case. Vanishes when the floor rejects nobody, which is also how the 8Y block
/// self-suppresses while `growth_min_8y_pct` sits at -1e9 (measured and rejected 2026-08-03).
fn leg_floor_lines(quotes: &[Quote], pinned: &[String], tuning: &config::BuyHeuristic, tag: &str, label: &str, knob: &str, floor: f64) -> Vec<String> {
    let mut hits: Vec<(&Quote, f64, String, Vec<&'static str>)> = quotes
        .iter()
        .filter(|q| !pinned.contains(&q.ticker)) // same pinned-skip as the blocks above
        .filter_map(|q| picks::growth_leg_floor_miss(q, tuning, tag).map(|(cagr, why, others)| (q, cagr, why, others)))
        .collect();
    if hits.is_empty() {
        return Vec::new();
    }
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.ticker.cmp(&b.0.ticker)));
    let mut seen_names = std::collections::HashSet::new();
    hits.retain(|(q, ..)| seen_names.insert(q.name.to_lowercase()));
    let more = if hits.len() > LEG_FLOOR_CAP { format!("top {LEG_FLOOR_CAP} of {}; ", hits.len()) } else { String::new() };
    let mut out = vec![
        String::new(),
        // "a floor", not "the floor": crypto answers to its own twin knob (0.0 vs the equity 75.0), so a
        // header claiming ONE bar would misread every coin row. Each row quotes the bar it was judged on.
        format!("{label} floor — every name a {label} cumulative floor rejects, best long record first ({more}they fail other gates too, so no tail above can reach them; equity and crypto floors differ, each row quotes its own):"),
    ];
    for (q, cagr, why, others) in hits.iter().take(LEG_FLOOR_CAP) {
        let also = if others.is_empty() { String::new() } else { format!(" · also: {}", others.join(", ")) };
        out.push(format!("  {:<8} {:<28.28} {cagr:>7.1}%/yr  {why}{also}", q.ticker, q.name));
    }
    // the knob and its LIVE value, never a hardcoded threshold — the block exists to make a gate's cost
    // visible, and a footer naming a number the run didn't use would misdirect the very edit it invites.
    out.push(format!("  (knob `{knob}` {floor:.0} — loosening it re-admits these; receipt in ci-settings)"));
    out
}

/// (#37 funds) Issuer prefixes stripped before two fund names are compared for the same index.
///
/// Stripped as a PREFIX and only as a prefix, never as free-floating tokens, because `global`, `first`,
/// `trust` and `street` are all real index words: removing them anywhere would fuse `FTSE Global All Cap`
/// with `FTSE All Cap`, i.e. an all-cap world fund with an all-cap developed one. Applied in a loop, so
/// `Amundi Index Solutions - Amundi MSCI World` sheds both leading `amundi`s and lands on `msci world`.
const FUND_ISSUERS: &[&str] = &[
    "amundi index solutions", "legal & general", "credit suisse", "goldman sachs", "state street",
    "first trust", "bnp paribas easy", "bnp paribas", "jp morgan", "global x", "21shares", "abrdn",
    "amundi", "comstage", "coinshares", "deka", "dws", "fidelity", "franklin", "han etf", "hsbc",
    "invesco", "ishares", "jpmorgan", "l&g", "lyxor", "ossiam", "pimco", "rize", "spdr", "swisscanto",
    "tabula", "ubs", "vaneck", "vanguard", "vontobel", "wisdomtree", "xtrackers",
];

/// (#37 funds) Wrapper, share-class and product-line tokens dropped anywhere in a fund name.
///
/// The product-line half (`core`, `prime`, `source`, `index`, `solutions`, `pea`, `eqqq`) is a
/// DELIBERATE widening chosen 2026-08-02 over a conservative list, so `iShares Core S&P 500` pairs with
/// `Invesco S&P 500` and `Amundi PEA Nasdaq-100` with `Invesco EQQQ Nasdaq-100` — both genuinely the same
/// index behind different wrappers. Every token added here is a new way to fuse two different books,
/// which is why `index_keys_are_pe_homogeneous` exists: it re-derives these keys over every fund that
/// reports a P/E and fails if any key covers two different ratios.
///
/// `hedged` is deliberately NOT here. Leaving it in keeps `MSCI World EUR Hedged` unpaired with
/// `MSCI World` — a missed twin, not a false one. Misses cost an `n/a`; false pairs cost a wrong number.
const FUND_NOISE: &[&str] = &[
    "ucits", "etf", "etfs", "etc", "etp", "fund", "index", "indices", "solutions", "core", "prime",
    "source", "pea", "eqqq", "acc", "accumulating", "dist", "distributing", "inc", "1c", "1d", "1acc",
    "2c", "eur", "usd", "gbp", "chf", "&",
];

/// (#37 funds) A fund name reduced to the SET of tokens that identify its index, or `None` if too little
/// survives to be evidence of anything.
///
/// Set, not sequence: `Invesco Technology S&P US Select Sector` and `SPDR S&P U.S. Technology Select
/// Sector` name the same index in a different word order, and that is exactly the pair this exists for.
///
/// The two-token floor is the same instinct as `bf_by_name`'s 10-byte floor — a one-token key like
/// `{gold}` or `{semiconductor}` matches far too much to be called an index identity.
fn index_key(name: &str) -> Option<Vec<String>> {
    let flat: String = name
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            '.' | ',' | '(' | ')' | '\'' => None,   // "u.s." -> "us"
            '-' | '/' | '+' | ':' => Some(' '),      // "nasdaq-100" -> "nasdaq 100"
            c => Some(c),                            // '&' survives, or "s&p" shatters
        })
        .collect();
    let mut rest = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    // loop: multi-brand names ("Amundi Index Solutions - Amundi MSCI World") carry the issuer twice
    while let Some(stripped) = FUND_ISSUERS
        .iter()
        .filter(|i| rest.strip_prefix(**i).is_some_and(|r| r.is_empty() || r.starts_with(' ')))
        .max_by_key(|i| i.len())
        .and_then(|i| rest.strip_prefix(i))
    {
        rest = stripped.trim_start().to_string();
    }
    let mut tokens: Vec<String> = rest
        .split_whitespace()
        .filter(|t| !FUND_NOISE.contains(t))
        .map(str::to_string)
        .collect();
    tokens.sort();
    tokens.dedup();
    (tokens.len() >= 2).then_some(tokens)
}

/// (#37 funds) Copy each fetched P/E to the fund's OTHER venue listings.
///
/// One UCITS fund lists on Xetra + Euronext + Lisbon and the ETF table shows every listing (only crypto
/// twins collapse upstream, in `dedup_currency_twins`), while `yahoo_top_holdings` is asked for one venue
/// per name. Without this the (#37) ceiling would cut one listing and keep its twin — the same "the class
/// with missing data gets the easier test" hole the trim exists to close.
///
/// Matched on the EXACT lowercased name, not on `index_key`: this is the same fund, so copying its own
/// ratio across its own listings is bookkeeping. Provenance rides along unchanged, so a borrowed value
/// stays marked borrowed on every venue it reaches.
fn fill_venue_listings(fund_pe: &mut picks::FundPeMap, quotes: &[core::Quote]) {
    let by_name: std::collections::HashMap<String, picks::FundPe> = quotes
        .iter()
        .filter_map(|q| fund_pe.get(&q.ticker).map(|p| (q.name.to_lowercase(), p.clone())))
        .collect();
    let siblings: Vec<(String, picks::FundPe)> = quotes
        .iter()
        .filter_map(|q| by_name.get(&q.name.to_lowercase()).map(|p| (q.ticker.clone(), p.clone())))
        .collect();
    fund_pe.extend(siblings);
}

/// (#37 funds) Borrow a look-through P/E for the funds that can never report one.
///
/// A swap-based (synthetic) ETF holds a total-return swap, not equities. Yahoo answers with
/// `stockPosition 0.0`, `otherPosition 1.0`, no holdings and a literal `0.0` for every `equityHoldings`
/// ratio, so `parse_fund_pe` correctly refuses it — there is nothing to invert. The exposure is still an
/// ordinary index, and a PHYSICAL fund on that index does report the book, so the number is recoverable
/// by identity even though it is unmeasurable for this fund.
///
/// MATCHED BY NAME, NEVER BY COMPOSITION, and that is a measured decision (2026-08-02). Yahoo's reported
/// basket for a synthetic fund appears to be COLLATERAL, not exposure: XLKS.L reads Technology 0.912 /
/// Financial Services 0.072 against its twin SXLK.L's Technology 0.994 / Communication Services 0.006,
/// and PANX.PA misses ANX.PA the same way. A sector-fingerprint matcher REJECTS both real pairs. For
/// exactly the funds that need a proxy, the payload describes the wrong portfolio.
///
/// Two guards, both load-bearing:
/// - **Bench only.** Borrowing is attempted solely for funds that were fetched and came back empty.
///   Filling in the ~4250 funds outside the bench would quietly promote a post-rank display trim into a
///   universe-wide gate — the thing `growth_max_peg_etf`'s doc rules out on request-budget grounds.
/// - **Disagreement refuses.** Several tickers sharing a key is normal (venue listings, rival wrappers on
///   one index) and they should all report the same ratio, since Yahoo's fund P/E is index-level: six
///   Nasdaq-100 funds all say 32.6797, four S&P 500 wrappers all say 26.8745. If candidates DISAGREE the
///   key has fused two different books, and the honest answer is the `n/a` we started with. Same
///   uniqueness-refusal rule as `bf_by_name` / `BfMetaMiss::AmbiguousName`.
///
/// Sources must be MEASURED (`from: None`), so a borrowed value can never itself be borrowed from.
async fn borrow_index_twins(
    client: &reqwest::Client,
    bench: &[String],
    fund_pe: &picks::FundPeMap,
    quotes: &[core::Quote],
) -> Vec<(String, picks::FundPe)> {
    let quote_of = |t: &str| quotes.iter().find(|q| q.ticker == t);
    // funds we ASKED about and got nothing for — never the rest of the universe
    let orphans: Vec<(&str, Vec<String>)> = bench
        .iter()
        .filter(|t| !fund_pe.contains_key(*t))
        .filter_map(|t| quote_of(t).and_then(|q| index_key(&q.name).map(|k| (t.as_str(), k))))
        .collect();
    if orphans.is_empty() {
        return Vec::new();
    }
    // every ETF listing that shares one of those keys is a candidate source
    let wanted: std::collections::HashSet<&Vec<String>> = orphans.iter().map(|(_, k)| k).collect();
    let candidates: Vec<&core::Quote> = quotes
        .iter()
        .filter(|q| crate::picks::quote_is_etf(q))
        .filter(|q| index_key(&q.name).is_some_and(|k| wanted.contains(&k)))
        .collect();
    // second pass: the twin is usually NOT in the bench, and `yahoo_top_holdings` only ever returns
    // symbols it was asked for (`for s in syms`), so a cached-but-unrequested P/E stays invisible.
    let mut todo: Vec<String> = candidates
        .iter()
        .map(|q| q.ticker.clone())
        .filter(|t| !fund_pe.contains_key(t) && !bench.contains(t))
        .collect();
    todo.sort();
    todo.dedup();
    let extra = if todo.is_empty() {
        std::collections::HashMap::new()
    } else {
        fetch::yahoo_top_holdings(client, &todo).await.1
    };
    // (fund staleness) the source's age travels with its P/E: a value borrowed from a twin whose own
    // ratio came off disk is no fresher than that twin, and it acts just as hard.
    let measured = |t: &str| -> Option<(f64, Option<chrono::NaiveDate>)> {
        extra
            .get(t)
            .and_then(|(_, _, pe, as_of)| pe.map(|p| (p, *as_of)))
            .or_else(|| fund_pe.get(t).filter(|f| f.from.is_none()).map(|f| (f.pe, f.as_of)))
    };
    orphans
        .iter()
        .filter_map(|(orphan, key)| {
            let mut found: Vec<(&str, f64, Option<chrono::NaiveDate>)> = candidates
                .iter()
                .filter(|q| index_key(&q.name).as_ref() == Some(key))
                .filter_map(|q| measured(&q.ticker).map(|(pe, as_of)| (q.ticker.as_str(), pe, as_of)))
                .collect();
            found.sort_by(|a, b| a.0.cmp(b.0)); // deterministic source across runs
            let (src, pe, as_of) = *found.first()?;
            // rival wrappers on one index must agree; if they do not, the key fused two books
            let agrees = found.iter().all(|(_, p, _)| (p - pe).abs() <= 0.01 * pe.abs());
            agrees.then(|| ((*orphan).to_string(), picks::FundPe { pe, from: Some(src.to_string()), as_of }))
        })
        .collect()
}

/// (fund valuation) One wrapped line of fund equity-book P/Es, CHEAPEST first — the number behind
/// the header's "quality pricey because it keeps winning". Values arrive already inverted from
/// `parse_fund_pe` (Yahoo serves reciprocals — see the fetch-side pin). Funds without the datum
/// stay silent; `None` when nobody has it.
///
/// (#37 funds) now also carries the PEG that `growth_max_peg_etf` trims the ETF table on, and marks the
/// funds it CUT. This is the display half of that trim, and it lives here rather than in the `peg`
/// COLUMN on purpose: the table shows survivors only, so a column could never explain a fund that
/// vanished. `picks::fund_peg_yield` is the same fn the trim calls — one PEG, printed and acted on.
///
/// (#37 funds) A `~` marks a P/E BORROWED from a physical index twin because the fund is swap-based and
/// has no equity book (see `borrow_index_twins`). Borrowed values act — they can cut a fund from the
/// table — so the sources are named in full at the end of the line. An inference that costs you a
/// candidate has to be traceable to the fund it came from.
fn fund_pe_line(fund_pe: &picks::FundPeMap, quotes: &[core::Quote], tuning: &config::BuyHeuristic) -> Option<String> {
    let bar = (tuning.growth_max_peg_etf > 0.0).then(|| 100.0 / tuning.growth_max_peg_etf);
    let mut rows: Vec<(&String, &picks::FundPe, Option<f64>)> = fund_pe
        .iter()
        .map(|(t, fp)| {
            // PEG needs the quote's long leg; a fetched fund with no quote in this run (pinned-only
            // symbol, CORE entry outside the universe) keeps its P/E and simply prints no PEG.
            let peg = quotes
                .iter()
                .find(|q| &q.ticker == t)
                .and_then(|q| picks::fund_peg_yield(q, tuning, fund_pe));
            (t, fp, peg)
        })
        .collect();
    rows.sort_by(|a, b| a.1.pe.total_cmp(&b.1.pe).then_with(|| a.0.cmp(b.0)));
    (!rows.is_empty()).then(|| {
        let line = rows
            .iter()
            .map(|(t, fp, peg)| {
                // (fund staleness) `°` here and in the table's `peg` cell mean the same thing; the DATE
                // lives only on this line, because a per-row date in a fixed-width column would cost
                // more width than the fact is worth.
                let mark = format!(
                    "{}{}",
                    if fp.from.is_some() { "~" } else { "" },
                    if fp.as_of.is_some() { "°" } else { "" }
                );
                let p = fp.pe;
                match (peg, bar) {
                    (Some(y), Some(b)) if *y < b => format!("{t} {p:.0}{mark} (PEG {:.2} — cut)", 100.0 / y),
                    (Some(y), _) => format!("{t} {p:.0}{mark} (PEG {:.2})", 100.0 / y),
                    (None, _) => format!("{t} {p:.0}{mark}"),
                }
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let sources: Vec<String> = rows
            .iter()
            .filter_map(|(t, fp, _)| fp.from.as_ref().map(|s| format!("{t}←{s}")))
            .collect();
        let stale: Vec<String> =
            rows.iter().filter_map(|(t, fp, _)| fp.as_of.map(|d| format!("{t} {d}"))).collect();
        let mut line = line;
        if !sources.is_empty() {
            line = format!(
                "{line}\n  ~ = no equity book of its own (swap-based); P/E borrowed from a physical fund on \
                 the same index — matched by name, not measured: {}",
                sources.join(", ")
            );
        }
        if !stale.is_empty() {
            line = format!(
                "{line}\n  ° = served from cache, not fetched this run (as-of date shown); refetched daily, \
                 dropped past 3 days: {}",
                stale.join(", ")
            );
        }
        line
    })
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

/// (#68) UNGRADEABLE BY THE MUTATION GATE, and skipped so that stays a stated fact rather than a
/// trap. `run` is reachable from `main.rs` and nowhere else, so the only test that exercises it is
/// `screen_ranks_and_journals_offline` in the cli suite — which `ci.yml`'s mutants job deliberately
/// does not run, grading `--lib --test backtest_fixture` alone. `replace run with ()` therefore
/// cannot be killed there: graded 2026-08-11 against exactly that selection, 1 mutant, 1 MISSED.
///
/// Without the attribute the gate is armed against every future edit in this function, because
/// `--in-diff` grades whole functions and any one-line change drags all of `run` into scope — which
/// is how a `saturating_sub` on a diagnostic line came to red a mutation job. The alternative was
/// adding `--test cli` to the gate; it was declined on cost, the gate being 98.6% compiler against a
/// measured ~24-mutant ceiling per 20-minute job.
#[mutants::skip]
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
        // (#44) this path skips the universe fetch by design, but it still JOINS sectors — 1-3 small CSVs.
        // Without them a commodity name explains undamped here (CF 22.25) while the full screen ranks it
        // damped (17.84), and `--explain` is exactly where that number has to reconcile. Unfiltered (`&[]`):
        // this is a lookup map, not a universe filter, so a sector-restricted config must not hide the
        // row's own sector.
        (args, std::collections::HashSet::new(), fetch::sector_map(&client, &settings.urls, &[]).await)
    } else {
        fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur, settings.prefer_eu_listing, &settings.sectors).await
    };
    // watchlist tickers are ALWAYS fetched so they show in their table for comparison (sector filter or not)
    universe.extend(settings.tickers.iter().cloned());
    universe.sort();
    universe.dedup();
    // (EU listing) That extend is VERBATIM and runs AFTER `fetch_universe` swapped the pond, so a
    // pinned US ticker puts its own US leg back next to the EU twin that replaced it. Both then carry
    // the same Yahoo name, and the dual-class collapse in `ranked` keeps the pinned leg — the knob is
    // on, the twin was resolved and priced, and it is deleted with nothing said. Warn instead of
    // changing either rule: the literal watchlist and the pinned-never-deduped exemption are both
    // deliberate (see the comment above and `picks.rs`), and rewriting a ticker the user typed on
    // purpose is worse than telling them it is being shadowed.
    if settings.prefer_eu_listing {
        // ponytail: the cache read is duplicated from `resolve_eu_listings` rather than shared. Four
        // lines of "read a JSON file or default" do not earn a graded function plus the test to kill
        // its mutant — and this call site must NOT resolve anything, only read what is already known.
        let eu: std::collections::HashMap<String, String> =
            std::fs::read_to_string(config::data_path(fetch::EU_LISTING_CACHE_PATH))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
        let shadowed = fetch::eu_shadowed_pins(&settings.tickers, &eu);
        if !shadowed.is_empty() {
            eprintln!(
                "screen: pinned US tickers shadow their resolved EU listing (the pin wins the dual-class collapse — drop it, or pin the EU symbol): {}",
                shadowed.join(", ")
            );
        }
    }

    // per-class counts so an EMPTY class is visible here (a leg that "succeeded" with 0 rows
    // never trips the fetch-failure warnings). Explicit-args runs skip the split — etf_tickers
    // is empty on that path, so the split would mislabel every arg ETF as a stock.
    if explicit_args {
        eprintln!("screen: {} explicit tickers + watchlist", universe.len());
    } else {
        let crypto = universe.iter().filter(|t| crate::picks::is_currency_quoted(t)).count();
        let etfs = universe.iter().filter(|t| etf_tickers.contains(*t)).count();
        // (#68) `saturating_sub`, because the two counts are not disjoint by construction: they are
        // independent filters over the same universe, and a ticker carrying a `-USD`/`-EUR` suffix
        // that is ALSO in the Xetra set is counted by both. That made the stock count wrap to a
        // nonsense figure — harmless while `[profile.release]` left overflow-checks off, a panic now
        // that it doesn't. A diagnostic line must not be able to kill the run it is describing, so
        // the count saturates: worst case it reads 0 stocks, which is visibly wrong rather than fatal.
        eprintln!(
            "screen: {} tickers in universe ({crypto} crypto + {} stocks + {etfs} ETFs)",
            universe.len(),
            universe.len().saturating_sub(crypto).saturating_sub(etfs)
        );
    }

    // live EU HICP series to inflation-adjust long-horizon returns, only when enabled
    let eu_infl = if settings.inflation_adjust.enabled {
        eprintln!("screen: fetching EU HICP inflation series…");
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    // intraday ONLY when the table actually prints 1h/6h/12h — it was hardcoded on, and it costs one
    // extra Yahoo chart request PER NAME (~65s of pacer sleep on a full universe) to fill three display
    // cells nothing scores on. news off (screen never prints headlines).
    let intraday = picks::wants_intraday(&settings.widths.columns);
    let mut quotes = fetch::quotes(&client, &settings.urls, &fx_cache, &universe, settings.dip_days, settings.high_days, intraday, false, &settings.anchor_windows, eu_infl.as_ref(), settings.inflation_adjust.score_on_nominal).await;
    // anything from the Xetra ETF feed IS an ETF, even if Yahoo tags it EQUITY (structured products
    // like BNP Paribas Issuance) — force it so it can't leak into the stocks table past the sector filter
    for quote in &mut quotes {
        if etf_tickers.contains(&quote.ticker) {
            quote.instrument_type = "ETF".into();
        }
        // (#44) join the GICS sector the universe CSV already carries onto the quote, so `is_commodity`
        // (the `c` flag + growth_commodity_damp) can read it without threading the map through
        // growth_score and every backtest caller. Empty map on the explicit-args path -> None -> inert.
        quote.sector = sector_of.get(&quote.ticker).cloned();
    }
    // (#45) why the USE/REPL columns read n/a, bucketed by cause. Runs here because bf_meta is consulted
    // per-quote inside the fetch above, long after fetch_universe (where the other BF diagnostics print)
    // has returned — and because the ETF tagging it filters on is the loop directly above. Conditional,
    // like the "no TER parsed from BF rows" line: silent when there is nothing to report.
    if let Some(report) = fetch::bf_meta_miss_report(&quotes) {
        eprintln!("{report}");
    }
    // (G) route the validated as-of fundamental onto the live quotes so the growth ranking weighs it —
    // only when the tilt is on (weight 0 default = no fetch, no change). Across the ~750-name universe the
    // FMP daily budget caps cold fetches; the rest serve from the disk cache, warming over runs.
    let mut fund_tilt_uncovered = false; // set below; joins the DEGRADED line at the end of the run
    if settings.buy_heuristic.growth_fund_weight > 0.0 {
        fetch::enrich_fund_factor(&client, &settings.urls, &mut quotes, &settings.buy_heuristic).await;
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
        quotes.len(), fresh_before.saturating_sub(quotes.len()), settings.stale_days
    );

    // Bitcoin NUPL: whole-market crypto sentiment gauge. Fetched BEFORE render so it can damp the
    // crypto rows (high NUPL = euphoric top), then also printed as the footer line.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;
    // (#45) per-coin MVRV, the same quantity as the NUPL above with one row per coin instead of
    // Bitcoin's alone. Stamped onto the quotes HERE rather than inside `fetch::quotes` because it is
    // one bulk request for the whole crypto lane — per-quote it would be an HTTP call per coin. Must
    // precede `render`: `crypto_max_mvrv` is a real gate in `growth_score`, not a display trim, so an
    // unstamped quote would rank as though the ceiling did not exist.
    let mvrv = fetch::fetch_mvrv(&client, &settings.urls, &universe).await;
    let mvrv_hits = quotes.iter().filter(|q| mvrv.contains_key(&q.ticker)).count();
    for quote in &mut quotes {
        quote.mvrv = mvrv.get(&quote.ticker).copied();
    }
    // Coverage is thin BY DESIGN of the source, so state it rather than letting a column of n/a imply
    // a broken fetch: the coins without a value pass the ceiling untested, which is the house
    // missing-data rule and also the main reason this gate is mild in practice.
    let coins = quotes.iter().filter(|q| crate::picks::is_currency_quoted(&q.ticker)).count();
    if coins > 0 {
        println!("Crypto valuation: {mvrv_hits} of {coins} coins carry an MVRV (the rest pass the ceiling free)");
    }

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
        false, false, &settings.anchor_windows, eu_infl.as_ref(), settings.inflation_adjust.score_on_nominal,
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
    // (round 55) CORE membership diff needs this list; the holdings fetch below needs it too, and that
    // now runs BEFORE render. Pure over `quotes`, so hoisting it changes nothing — the diff still prints
    // in its own footer further down.
    let core_now: Vec<String> =
        crate::picks::hold_core_list(&quotes).iter().take(settings.top_picks).map(|q| q.ticker.clone()).collect();
    // (round 56)/(#37 funds) fund holdings + composition, fetched ONCE here. This used to sit with the
    // overlap footer below, i.e. AFTER the tables printed, because everything it fed was display-only.
    // The look-through P/E it carries is no longer display-only — it drives the ETF PEG trim inside
    // `lane_split` — so the fetch has to precede `render`. Yahoo topHoldings, weekly-cached.
    let (holdings, mix, bench) = {
        let is_fund = |q: &&Quote| crate::picks::quote_is_etf(q) && !crate::picks::is_currency_quoted(&q.ticker);
        let mut ranked: Vec<(&Quote, f64)> = quotes
            .iter()
            .filter(is_fund)
            .filter_map(|q| growth_score(q, &settings.buy_heuristic).map(|s| (q, s)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        // one venue per fund name (the momentum table dedups the same way), then top rows + pinned + CORE.
        // 2x top_picks, not top_picks: the PEG trim drops rows and the table refills from BELOW the cut,
        // and a refilled row with no P/E fetched would pass the ceiling for free. The bench costs ~24
        // extra weekly-cached symbols — still nowhere near a universe-wide fund gate.
        let mut seen = std::collections::HashSet::new();
        let mut syms: Vec<String> = ranked
            .iter()
            .filter(|(q, _)| seen.insert(q.name.to_lowercase()))
            .take(settings.top_picks * 2)
            .map(|(q, _)| q.ticker.clone())
            .chain(quotes.iter().filter(is_fund).filter(|q| settings.tickers.contains(&q.ticker)).map(|q| q.ticker.clone()))
            .chain(core_now.iter().cloned())
            .collect();
        syms.sort();
        syms.dedup();
        let (holdings, mix) = fetch::yahoo_top_holdings(&client, &syms).await;
        (holdings, mix, syms)
    };
    let mut fund_pe: picks::FundPeMap = mix
        .iter()
        .filter_map(|(t, (_, _, pe, as_of))| {
            pe.map(|p| (t.clone(), picks::FundPe { as_of: *as_of, ..picks::FundPe::from(p) }))
        })
        .collect();
    // The fetch above keeps ONE venue per fund name, but the ETF table shows every listing (only
    // crypto twins are collapsed upstream, in `dedup_currency_twins`) and the universe is Xetra +
    // Euronext + Lisbon, so a UCITS fund routinely appears two or three times. Propagate each fetched
    // P/E to that fund's other listings or the (#37) ceiling would cut one venue and keep its twin —
    // the same "the class with missing data gets the easier test" hole this trim exists to close.
    fill_venue_listings(&mut fund_pe, &quotes);
    // (#37 funds) then the swap-based funds, which report a literal 0.0 book and would otherwise ride the
    // ceiling's free pass forever. Borrows a physical index twin's P/E; costs a second topHoldings pass
    // only when something is actually missing. Re-run the venue fill afterwards so a borrowed value
    // reaches the fund's other listings too — same hole, same fix, one level down.
    let borrowed = borrow_index_twins(&client, &bench, &fund_pe, &quotes).await;
    fund_pe.extend(borrowed);
    fill_venue_listings(&mut fund_pe, &quotes);
    // (#79) the web payload's home: the same gitignored data dir `.screen_state.json` uses, written
    // on every run rather than behind a flag — it is one small file, and a knob nobody sets is a knob
    // not worth having. The Pages workflow copies it next to `web/index.html` and deploys.
    let web_out = crate::config::data_path(".screen_web.json");
    let (explain_text, ranked_now) = render(&quotes, settings.top_picks, &settings.buy_heuristic, &settings.widths, RenderCtx {
        nupl,
        sectors: &settings.sectors,
        sector_of: &sector_of,
        pinned: &settings.tickers,
        owned: &owned,
        explain: explain.as_deref(),
        show_hold_core: true,
        fund_pe: &fund_pe,
        web_out: Some(&web_out),
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
        // (#103) the CORE shortlist, priced the same way as `rows`. `core_now` was computed above for
        // the membership diff; journalling it is what lets the buy-and-hold recommendation ever be
        // graded out-of-sample, the way the momentum book already is.
        core: if settings.buy_heuristic.journal_core_list {
            core_now
                .iter()
                .map(|t| (t.clone(), quotes.iter().find(|q| &q.ticker == t).and_then(|q| q.price_eur)))
                .collect()
        } else {
            Vec::new()
        },
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
        // (#115) overlapped, not serialised: this was the last multi-name fetch still awaited one
        // name at a time, ~20-40 round trips end to end. Same shape and same width as `fetch::quotes`,
        // and the global `fetch::throttle` pacer still caps the request RATE — this overlaps latency,
        // it does NOT add a call. Completion order never reaches the output: the pairs land in a
        // BTreeMap and `headline_rows` walks `footer_names` itself.
        let (cl, urls) = (&client, &settings.urls);
        let fetched: Vec<Option<(String, String)>> = futures::stream::iter(footer_names.iter())
            .map(|t| async move {
                fetch::fetch_news(cl, urls, t).await.into_iter().next().map(|title| (t.clone(), title))
            })
            .buffered(fetch::fetch_concurrency())
            .collect()
            .await;
        let first_title: std::collections::BTreeMap<String, String> =
            fetched.into_iter().flatten().collect();
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
    // `core_now` itself is built before render (the holdings fetch needs it).
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

    // (A2) GATE FUNNEL: the counts behind the tails below. Printed first because it is the only thing
    // here that answers "which gate is actually costing me rows" — the name-level tails show WHO died,
    // this shows WHERE, and the sole-blocker column is what a knob change would actually buy.
    let funnel = funnel_lines(&quotes, &settings.buy_heuristic);
    if !funnel.is_empty() {
        println!();
        for line in funnel {
            println!("{line}");
        }
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
        // Header names all THREE predicates this block applies, not just the first: it needs exactly one
        // failing gate AND a close miss AND a non-pinned name. Advertising only "ONE growth gate" made a
        // pinned name failing one gate narrowly (AAPL, peg 2.14) look like it belonged here when it can
        // never reach this code — the pinned filter above runs before closeness is ever tested.
        println!("\nNear-miss — rejected NARROWLY on ONE growth gate (not ranked above; pinned names: see gate review), loosen intentionally if wanted:");
        // (round 54) one row per FUND: the same UCITS fund lists on several venues (L&G Gold Mining
        // printed as both AUCO.L and ETLX.DE) — the momentum tables dedup by underlying, this block
        // didn't. First occurrence wins = the closest venue, thanks to the sort above.
        let mut seen_names = std::collections::HashSet::new();
        for (q, gate, why) in near.iter().filter(|(q, ..)| seen_names.insert(q.name.to_lowercase())) {
            println!("  {:<8} {:<44.44} {:<10} {why}", q.ticker, q.name, gate);
        }
    }

    // (C) TWO-GATE tail: names failing EXACTLY two gates, both close. The block above needs exactly
    // ONE failing gate, so a name one notch outside two fences vanished from the whole tool — no table
    // row, no near-miss line, and (before the --explain fix) no way to ask. That is how MSFT went
    // missing with no explanation.
    //
    // NOT "would need both knobs loosened": with `use_life_cagr` on, `cagr` and `cagr-life` read the
    // SAME number, so that pair (the bulk of the live list) is one fact counted twice and one knob to
    // recover. The header states what is always true — these are invisible above — and lets the
    // histogram say which pairs.
    for l in multi_gate_lines(
        &quotes,
        &settings.tickers,
        &settings.buy_heuristic,
        2,
        "Two gates — one notch outside TWO fences, so the near-miss tail above (which needs exactly one) can't show them",
        None, // vanish when empty, as this block always has
    ) {
        println!("{l}");
    }

    // (C3) THREE-GATE tail: same predicate one arity further out. Prints a line when empty, unlike
    // every sibling: the reason this block exists is that the cohort was invisible, and a block that
    // silently disappears leaves "none this run" and "not looked at" indistinguishable — the exact
    // ambiguity it was added to remove.
    for l in multi_gate_lines(
        &quotes,
        &settings.tickers,
        &settings.buy_heuristic,
        3,
        "Three gates — one notch outside THREE fences, so neither tail above (which need exactly one or two) can show them",
        Some("Three gates — no name is within one notch of three fences this run."),
    ) {
        println!("{l}");
    }

    // (D) DOWN-YEAR tail: a proven long record rejected by the 1Y floor ALONE. The near-miss tail
    // above only reaches -10% (the `1Y+` close margin), so a name like MSFT at -16.9% with a 22%/yr
    // record was invisible everywhere. All lanes in one list, sorted by CAGR descending — index
    // trackers have mediocre CAGR by construction, so the single stocks this is FOR float above them
    // without a lane special-case, and the cap trims the trackers instead of the answer.
    const DOWN_YEAR_CAP: usize = 15;
    let mut down: Vec<(&Quote, f64, f64, f64)> = quotes
        .iter()
        .filter(|q| !settings.tickers.contains(&q.ticker)) // same pinned-skip as the blocks above
        .filter_map(|q| growth_down_year_miss(q, &settings.buy_heuristic).map(|(y1, cagr, range)| (q, y1, cagr, range)))
        .collect();
    if !down.is_empty() {
        down.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.ticker.cmp(&b.0.ticker)));
        let mut seen_names = std::collections::HashSet::new();
        down.retain(|(q, ..)| seen_names.insert(q.name.to_lowercase()));
        let more = if down.len() > DOWN_YEAR_CAP { format!(" (top {DOWN_YEAR_CAP} of {})", down.len()) } else { String::new() };
        println!("\nDown year — clears every other growth gate, rejected only by the 1Y floor{more}:");
        for (q, y1, cagr, range) in down.iter().take(DOWN_YEAR_CAP) {
            println!("  {:<8} {:<32.32} {y1:>7.1}% {cagr:>7.1}%/yr {range:>5.0}%", q.ticker, q.name);
        }
        // NOT a shortlist. This exact cohort was measured before the floor was put back, and printing
        // it without the number invites buying the thing the gate exists to avoid.
        println!("  (round 5: loosening this floor to -10 admitted n=284 names averaging -108.1 pts forward — knob `growth_min_1y_pct`, receipt in ci-settings)");
    }

    // (E) LONG-LEG FLOOR tails: what the 5Y and 8Y cumulative bars cost, by name. Every block above
    // filters on a narrow miss, a small failure count, or both, and the 5Y bar satisfies neither — it
    // fails 2161 names, sole-blocks ZERO, and the names worth seeing miss it grossly (AMZN +24.7%
    // against a +75 bar). So the tightest live gate in the tool was the one with no table. Printed last
    // because it is the loosest and longest list. The 8Y call is silent at `growth_min_8y_pct: -1e9` and
    // starts working the day that knob is set — same helper, one line.
    for (tag, label, knob, floor) in [
        ("5Y+", "5Y", "growth_min_5y_pct", settings.buy_heuristic.growth_min_5y_pct),
        ("8Y+", "8Y", "growth_min_8y_pct", settings.buy_heuristic.growth_min_8y_pct),
    ] {
        for l in leg_floor_lines(&quotes, &settings.tickers, &settings.buy_heuristic, tag, label, knob, floor) {
            println!("{l}");
        }
    }

    // (#54) What the CAGR PIN costs, by name. Silent at `fixed_cagr_years: 0` (the shipped default), so
    // this block only exists for someone who has deliberately turned the pin on and is owed the bill.
    // Unlike every tail above it does NOT key on a gate — the pin moves the CAGR that seven readers
    // consume, so its casualties surface through different fences and only the counterfactual is common
    // to them. Sorted by what the pin took, worst first: that ordering puts the names whose long record
    // the pin is discarding hardest at the top, which is the whole question a reader has here.
    {
        const PIN_CAP: usize = 15;
        let pin_years = settings.buy_heuristic.fixed_cagr_years;
        let mut pinned: Vec<(&Quote, picks::PinDrop)> = quotes
            .iter()
            // DELIBERATELY NOT skipping `settings.tickers`, unlike every sibling tail above. Those skip
            // watchlist names because a pinned row is printed in the table anyway, so a second line about
            // it is noise. That reasoning inverts here: PIN exempts a name from the sector and score CUTS,
            // never from the GATES, so a pinned name whose `growth_score` is None still vanishes outright —
            // and a watchlist name silently missing is the exact symptom this block was written for.
            .filter_map(|q| picks::pin_dropped(q, &settings.buy_heuristic).map(|d| (q, d)))
            .collect();
        if !pinned.is_empty() {
            pinned.sort_by(|a, b| {
                let (la, lb) = (a.1.free.0 - a.1.pinned.0, b.1.free.0 - b.1.pinned.0); // CAGR the pin gave up
                lb.total_cmp(&la).then_with(|| a.0.ticker.cmp(&b.0.ticker))
            });
            let more = if pinned.len() > PIN_CAP { format!(" (top {PIN_CAP} of {})", pinned.len()) } else { String::new() };
            println!("\nDropped by the CAGR pin (`fixed_cagr_years: {pin_years}`) — gates that pass on the longest leg{more}:");
            for (q, d) in pinned.iter().take(PIN_CAP) {
                let ((pc, py), (fc, fy)) = (d.pinned, d.free);
                println!("  {:<8} {:<26.26} {pc:>6.1}%/yr on {py:.0}Y vs {fc:>6.1}%/yr on {fy:.0}Y   {}", q.ticker, q.name, d.why);
                // Honesty line: without it a reader sets the knob to 0, the name still does not appear,
                // and the block looks broken. It is not — the pin was one of several fences.
                if !d.still.is_empty() {
                    println!("           └ setting 0 is NOT enough — also fails at 0: {}", d.still.join(", "));
                }
            }
            // The pin is a COMPARABILITY choice: it judges every name over the same window so their CAGRs
            // are commensurable. A name here was not judged bad — it was declined a judgment on its own
            // longest record. Say that, or the list reads as a rejection list and gets treated as one.
            println!(
                "  (not a quality verdict: the pin scores every name over the SAME {pin_years}Y window, so a longer\n   \
                 record is discarded rather than failed. Set `fixed_cagr_years: 0` to rank on the longest leg — \n   \
                 that is the knob's own trade, receipt at `fixed_cagr_years` in ci-settings.)"
            );
        }
    }

    // (#55) PROVEN-RECORD tail: names whose long CAGR clears the lane's own floor and which still did
    // not rank. The one cohort a reader actually asks about ("great record, where did it go?") and the
    // only one no other block could name: the funnel names SOLE-blockers, near-miss needs exactly one
    // gate, the two-gate tail needs both close, the down-year tail needs a sole `1Y+`, and the leg-floor
    // tails need their own floor to fire. AMZN at 8 fails `cagr` AND `peg`, with PEG 3.37 against a 1.60
    // ceiling — gross, not narrow — so it is counted in two funnel rows and named in none of them.
    // Printed after the pin block because the pin's bill is the more specific claim about the same names.
    {
        const PROVEN_CAP: usize = 20;
        let mut proven: Vec<(&Quote, f64, f64, String)> = quotes
            .iter()
            // Same reason as the pin block above for keeping `settings.tickers` in: the gates apply to a
            // pinned name too, so a watchlist compounder scoring None vanishes exactly like any other.
            .filter_map(|q| picks::proven_but_unranked(q, &settings.buy_heuristic).map(|(c, y, w)| (q, c, y, w)))
            .collect();
        if !proven.is_empty() {
            // LONGEST record first, best CAGR within it. Sorting by CAGR alone was measured and dropped:
            // it buries the 20Y names under 5Y momentum (live 2026-08-07, the whole top of the list was
            // 5Y coins and a 42%/yr 5Y run), and the longest record is precisely the strongest version of
            // the claim this block makes. The rungs are few (20/10/8/5), so this reads as cohorts.
            proven.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| b.1.total_cmp(&a.1)).then_with(|| a.0.ticker.cmp(&b.0.ticker)));
            let more = if proven.len() > PROVEN_CAP { format!(" (top {PROVEN_CAP} of {})", proven.len()) } else { String::new() };
            println!("\nDidn't rank despite a proven long record (≥{:.0}Y leg){more}:", picks::PROVEN_MIN_YEARS);
            for (q, cagr, years, why) in proven.iter().take(PROVEN_CAP) {
                println!("  {:<8} {:<26.26} {cagr:>6.1}%/yr on {years:.0}Y   {why}", q.ticker, q.name);
            }
            // Two claims worth stating, both load-bearing. The record is measured UNPINNED, so a reader
            // running a pin can trust that the pin did not quietly shrink this list. And a name absent
            // from EVERY block above is not unexplained — `--explain` has always answered per name; this
            // tail only removes the requirement that you suspect the name first.
            println!(
                "  (record measured on the longest leg regardless of `fixed_cagr_years`, so the pin cannot hide a name\n   \
                 from this list. For any other name: `screen --explain TICKER`.)"
            );
        }
    }

    // (round 56) holdings-overlap footer: the buy candidates are the ranked ETF rows + the pinned
    // funds + the CORE shortlist, and "different" sector funds routinely hold the same top-10
    // mega-caps — invisible from the names. Payload fetched above (it now also feeds the ETF PEG trim,
    // which has to run before the tables print); everything in this block stays display-only.
    {
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
        if let Some(pe) = fund_pe_line(&fund_pe, &quotes, &settings.buy_heuristic) {
            println!("\nFund valuation — P/E of each fund's equity book, and the PEG the ceiling trims on (cheapest first): {pe}");
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

    // (#115) THE JOURNAL, READ ONCE AND RESTATED ONCE, for every footer below it. This used to be
    // six separate reads restated in only the first of them, and that asymmetry was a live bug:
    // `fund_flow_lines` divides price appreciation out of AUM growth, so it READS journaled closes —
    // and it read them raw. AUM is split-neutral (shares double, price halves) while a journaled
    // close is not, so the error does not cancel: a 2:1 split halved `c1/c0` and printed roughly
    // +100% net inflow that never happened, on the flattering side of a footer whose only labelled
    // state is "(bleeding)". One binding makes that unrepresentable, and
    // `the_journal_is_read_once_and_restated_once` keeps it that way.
    //
    // (#82) The restatement is the same one `track` and `sim` do, off the same helper and for the
    // same reason: today's prices are retro-split-adjusted and the journal is not. Silent — `track`
    // is where the note belongs — but it MUST happen, because a trust line that quietly disagreed
    // with `track` would be worse than either. It rewrites `snap.rows` PRICES only, so the four
    // rank-based footers (which read positions, not prices) are unaffected either way; sharing one
    // restated copy is safe because `adjust_for_splits` is idempotent (see `track.rs`).
    let (mut snaps, _) = crate::commands::track::read_snapshots();
    crate::commands::track::adjust_for_splits(
        &mut snaps,
        &crate::commands::track::split_factor_from(&quotes),
    );

    // (trust line) the ranking's own live out-of-sample grade: every past journaled top-10 at
    // today's prices vs the S&P 500 — same fold as `track` (verdict_stats), so the two can't
    // disagree. Zero new fetches: past books are ex-universe names, so this run's quotes already
    // price them (a narrow watchlist run may grade fewer rows; track's table stays the honest
    // view). Today's own snapshot is 0 days old and grades nothing, so no self-grade.
    {
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
        println!("\n{}", crate::commands::backtest::verdict_line(&v, drift, settings.buy_heuristic.print_n_eff));
    }

    // (round 28) persistent leaders: which of today's top-10 have DURABLY held a top-10 rank
    // across the journal — the multi-screen frequency the trust line (return) and the membership
    // diff (churn since last screen) don't show. A 20yr holder wants the durable leaders, not a
    // name that flashed in once. Reads the same journal as the trust line; today's own row (just
    // written) is excluded so the frequency is out-of-sample. K and the since-date are stated so a
    // thin journal can't read as a long record. Zero fetch; silent under 2 past screens or an
    // empty durable set.
    {
        let past: Vec<_> = snaps.iter().filter(|s| s.date < run_date).cloned().collect();
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

    // (#111 / round 3 §8) what you'd have to believe: for each of today's top-10 with a P/E, the EPS
    // growth the CURRENT price already pays for, printed next to what the name has actually delivered.
    // A ranking says "this one is best"; this turns it into a claim someone can be wrong about. Zero
    // fetch. Silent at horizon 0 (the shipped state) and silent for any row with no earnings.
    if settings.buy_heuristic.implied_growth_years > 0 {
        let (yrs, req) = (
            settings.buy_heuristic.implied_growth_years,
            settings.buy_heuristic.implied_growth_required_pct,
        );
        let rows: Vec<String> = ranked_now
            .iter()
            .take(crate::commands::track::BOOK)
            .filter_map(|t| quotes.iter().find(|q| &q.ticker == t))
            .filter_map(|q| {
                let implied = picks::implied_growth_pct(q, &settings.buy_heuristic, yrs, req)?;
                // (non-negotiable 4) the delivered leg is `long_cagr_pct`, the same number the trend
                // term scores and the LEG column prints — never a second CAGR computed at a read site.
                let delivered = picks::long_cagr_pct(q, &settings.buy_heuristic)
                    .map_or_else(|| "n/a".to_string(), |c| format!("{c:+.0}%/yr"));
                Some(format!("{} implies {implied:+.0}%/yr (delivered {delivered})", q.ticker))
            })
            .collect();
        if !rows.is_empty() {
            println!(
                "\nWhat the price already pays for — EPS growth implied over {yrs}y at a {req:.0}%/yr required return, \
                 with the multiple reverting to ref_pe {:.0} (dividends ignored, so this is conservative for a payer):",
                settings.buy_heuristic.ref_pe
            );
            println!("  {}", rows.join(" · "));
        }
    }

    // (#107 / round 3 §3) rank robustness: today's top-10 re-ranked under `rank_perturb_k` copies of
    // the shipped knobs with every tilt weight scaled by an independent U(0.8, 1.2). The printed rank
    // above is a point estimate under ONE knob vector; this is its error bar. A name whose IQR spans
    // half the table ranks where it does because of the tuning, not despite it. Zero fetch, no reorder
    // — the book printed above is unchanged. Silent at k = 0, which is the shipped state.
    if settings.buy_heuristic.rank_perturb_k > 0 {
        let spread =
            picks::rank_robustness(&quotes, &settings.buy_heuristic, settings.buy_heuristic.rank_perturb_k);
        let mut rows: Vec<(&String, f64, f64, f64)> = ranked_now
            .iter()
            .take(crate::commands::track::BOOK)
            .filter_map(|t| spread.get(t).map(|&(m, lo, hi)| (t, m, lo, hi)))
            .collect();
        // by MEDIAN rank, not by today's score — the whole point of the footer is that the two orders
        // can disagree, and printing it in score order would hide exactly the disagreement.
        rows.sort_by(|a, b| a.1.total_cmp(&b.1));
        if !rows.is_empty() {
            let k = settings.buy_heuristic.rank_perturb_k;
            let parts = rows
                .iter()
                .map(|(t, m, lo, hi)| format!("{t} #{m:.0} (IQR {lo:.0}–{hi:.0})"))
                .collect::<Vec<_>>()
                .join(" · ");
            println!("\nRank under {k} perturbed knob vectors, by median rank — {parts}");
        }
    }

    // (#112 / round 3 §9) the FX position nothing else on this surface displays. `market mix` above
    // counts listing country over the stocks lane; this counts CURRENCY over the whole book and looks
    // funds through to their holdings, which is where the two answers stop agreeing. Zero fetch — the
    // holdings are the ones already pulled for `fund_pe`. Display only, deliberately: whether currency
    // concentration should tilt the rank is a measured question, and this is how the data to answer it
    // becomes visible. Silent at false, which is the shipped state.
    if settings.buy_heuristic.print_currency_mix {
        let book: Vec<String> = ranked_now.iter().take(crate::commands::track::BOOK).cloned().collect();
        if let Some(line) = currency_mix_line(&book, &quotes, &holdings) {
            println!("\n{line}");
        }
    }

    // (round 34) fund flow: for today's top-10, net shares created/redeemed across the journal with
    // price appreciation divided OUT of AUM growth — is each fund GAINING or BLEEDING assets? A 20yr
    // durability axis orthogonal to every rank/return footer above: a fund bleeding AUM risks
    // liquidation before a decades hold ends, and net inflows are smart-money confirmation. Funds
    // only (stocks/crypto carry no AUM). Zero fetch. COARSE by design — BF refreshes AUM ~monthly,
    // so a reading accrues over weeks, not per-day; silent until ≥2 journal points carry AUM+close.
    {
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
    let euribor_3m = crate::commands::print_macro_footer(&client, &settings.urls).await;

    // (#110) (round 3 §7) the LEVEL axis, printed under the rate it subtracts. The anchor is the first
    // CORE name with a look-through P/E — this tool's own definition of "the index", so the line cannot
    // quote a valuation for a market the CORE shortlist would never have bought. Silent at band 0
    // (the shipped state), and silent when either leg is missing rather than guessing one.
    if let (Some(e), Some((anchor, pe))) = (
        euribor_3m,
        core_now.iter().find_map(|t| fund_pe.get(t).map(|fp| (t.as_str(), fp.pe))),
    ) {
        if let Some(line) =
            valuation_state_line(anchor, pe, e, spx_off_hi, settings.buy_heuristic.entry_excess_yield_band)
        {
            println!("\n{line}");
        }
    }

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

/// (#110) (round 3 §7) The LEVEL axis, beside the PATH axis [`entry_state_class`] already prints.
///
/// "How far below its high" is a path signal, and it is right for the wrong reason at both ends: after
/// a decade-long grind up a market can be at a nosebleed valuation AND at its high, where the rule says
/// deploy slowly; after a fast crash from a cheap base it says deploy fast. Both happen to be right.
/// Neither is right BECAUSE of what the rule measured.
///
/// The level signal with the strongest long-horizon evidence is starting valuation, and the cheapest
/// honest version of it is already on disk: index earnings yield minus the risk-free rate — Shiller's
/// excess CAPE yield in one subtraction, on a look-through P/E this tool already fetches and the
/// Euribor 3M the macro footer already prints. Trailing rather than cyclically adjusted, which is a
/// real difference and is why the printed line names its inputs instead of just its verdict.
///
/// `band` is the symmetric threshold in percentage points; `0` (the shipped state) = off, no claim.
/// `None` also when the P/E is absent or non-positive — an unjudgeable market is not a neutral one.
pub(crate) fn valuation_state(index_pe: f64, risk_free_pct: f64, band: f64) -> Option<(f64, &'static str)> {
    if band <= 0.0 || index_pe <= 0.0 {
        return None;
    }
    let excess = 100.0 / index_pe - risk_free_pct;
    Some(match excess {
        e if e >= band => (excess, "CHEAP"),
        e if e <= -band => (excess, "RICH"),
        _ => (excess, "NEUTRAL"),
    })
}

/// (#110) The 2x2 read: the level state above against the path state the banner prints. The corners
/// where they AGREE are where either rule alone would have done; the corners where they disagree are
/// the whole reason for adding a second axis, so those are the ones this sentence names.
///
/// Deliberately does NOT touch `deploy_multiplier`. Which axis prices the deploy schedule better — or
/// whether the 2x2 beats both — is a measured question against the same backtest that produced the
/// +9.1/+6.0/+5.9 receipt, and printing a second axis is how you get the data to answer it.
fn valuation_state_line(anchor: &str, index_pe: f64, euribor: f64, off_hi: Option<f64>, band: f64) -> Option<String> {
    let (excess, level) = valuation_state(index_pe, euribor, band)?;
    let path = off_hi.map(|o| entry_state_class(o).0);
    let read = match (level, path) {
        (_, None) => "no index level fetched, so this is the only axis in this run".to_string(),
        ("CHEAP", Some(p @ ("DRAWDOWN" | "PULLBACK"))) | ("RICH", Some(p @ "NEAR-HIGH")) => {
            format!("agrees with {p} — either axis alone would have said the same here")
        }
        (_, Some(p)) => format!(
            "DISAGREES with {p} — this is the corner a second axis exists for, and nothing measures which one is right yet"
        ),
    };
    Some(format!(
        "Valuation state: {anchor} look-through P/E {index_pe:.1} -> earnings yield {:.2}% - Euribor 3M {euribor:.2}% = {excess:+.2} pts excess -> {level} (band ±{band:.1}). {read}. Deploy still reads the drawdown axis alone. NOT advice.",
        100.0 / index_pe
    ))
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

    /// (#110) The LEVEL axis. Four claims, and the last two are the reason the axis is worth adding:
    ///
    ///   * band 0 is the shipped state and says NOTHING — not "neutral", nothing at all. A default
    ///     that printed a verdict would be an unmeasured claim shipped on by accident.
    ///   * the classification is the subtraction and nothing else: earnings yield minus the risk-free
    ///     rate, symmetric around the band, boundaries inclusive on both sides.
    ///   * an absent or non-positive P/E is UNJUDGEABLE, not neutral. Every gate in this project lets
    ///     missing data pass; a level state has nothing to pass, so it declines to speak.
    ///   * the 2x2 line distinguishes agreement from DISAGREEMENT, because the corners where the two
    ///     axes disagree are the only reason to carry a second one.
    #[test]
    fn the_level_axis_speaks_only_when_it_has_both_legs() {
        assert_eq!(valuation_state(20.0, 2.0, 0.0), None, "band 0 is the shipped state: no claim");
        assert_eq!(valuation_state(0.0, 2.0, 1.0), None, "no P/E = unjudgeable, not neutral");
        assert_eq!(valuation_state(-5.0, 2.0, 1.0), None, "a negative P/E is not a cheap market");

        // 20 P/E -> 5.00% earnings yield. Against a 2% risk-free that is +3.00 excess.
        assert_eq!(valuation_state(20.0, 2.0, 1.0).unwrap().1, "CHEAP");
        assert_eq!(valuation_state(20.0, 2.0, 3.0).unwrap().1, "CHEAP", "the band is inclusive");
        assert_eq!(valuation_state(20.0, 2.0, 3.5).unwrap().1, "NEUTRAL");
        // 40 P/E -> 2.50% yield against a 5.5% risk-free = −3.00 excess, the mirror case
        let (excess, state) = valuation_state(40.0, 5.5, 3.0).unwrap();
        assert!((excess + 3.0).abs() < 1e-12, "{excess}");
        assert_eq!(state, "RICH", "the band is symmetric and inclusive at both ends");

        // the 2x2: agreement reads as redundancy, disagreement as the reason for the axis
        let line = |pe: f64, e: f64, off: Option<f64>| {
            valuation_state_line("VWCE.DE", pe, e, off, 1.0).expect("band 1.0 is armed")
        };
        assert!(line(20.0, 2.0, Some(30.0)).contains("agrees with DRAWDOWN"), "cheap AND falling");
        assert!(line(40.0, 5.5, Some(0.0)).contains("agrees with NEAR-HIGH"), "rich AND at the high");
        assert!(line(20.0, 2.0, Some(0.0)).contains("DISAGREES with NEAR-HIGH"), "cheap AT the high");
        assert!(line(40.0, 5.5, Some(30.0)).contains("DISAGREES with DRAWDOWN"), "rich AND falling");
        assert!(line(20.0, 2.0, None).contains("only axis"), "no index level = one axis, said out loud");
        // and it never claims to move the deploy schedule, which it does not
        assert!(line(20.0, 2.0, Some(30.0)).contains("Deploy still reads the drawdown axis alone"));
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
            (
                vec![("Technology".into(), 0.99), ("Communication Services".into(), 0.01)],
                Some((1.0, 0.0)),
                None,
                None,
            ),
        );
        m.insert(
            "MIXED.DE".into(),
            (
                vec![("Financial Services".into(), 0.40), ("Industrials".into(), 0.30), ("Energy".into(), 0.20)],
                Some((0.60, 0.40)),
                None,
                None,
            ),
        );
        m.insert("NOSECTORS.L".into(), (Vec::new(), Some((1.0, 0.0)), None, None)); // silent
        let lines = sector_tilt_lines(&m);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  TECH.L") && lines[0].contains("Technology 99% · Communication Services 1%"));
        assert!(!lines[0].contains("bond"), "pure-equity fund must not print a bond leg: {}", lines[0]);
        assert!(lines[1].contains("Financial Services 40% · Industrials 30%"), "top-2 cap: {}", lines[1]);
        assert!(!lines[1].contains("Energy"), "third sector must drop: {}", lines[1]);
        assert!(lines[1].contains("(equity 60% / bond 40%)"), "real bond leg prints: {}", lines[1]);
        assert!(sector_tilt_lines(&HashMap::new()).is_empty());
    }

    /// (#112) The whole reason this footer exists is one case: a EUR-LISTED fund holding US names is
    /// dollar exposure, and every other surface in the tool reads it as European. So the assert that
    /// must not be weakened is `SPYL.DE` landing under USD — if a refactor ever routes a fund through
    /// its own `quote_currency`, this line goes green-to-EUR and the footer starts lying in the exact
    /// direction it was built to correct.
    ///
    /// Also pins the three honesty rules around it: a fund with NO holdings is `?` and not its
    /// listing, a coin is its own bucket rather than a dollar, and a stock's pence quote (GBp) is the
    /// same GBP exposure. And the coverage clause quotes the WORST fund, not an average.
    #[test]
    fn a_funds_listing_is_not_its_currency_exposure() {
        let etf = |ticker: &str, ccy: &str| {
            let mut x = Quote::stub(ticker, "€100.00", "", "Tracker UCITS ETF");
            x.instrument_type = "ETF".into();
            x.quote_currency = Some(ccy.into());
            x
        };
        let stock = |ticker: &str, ccy: &str| {
            let mut x = Quote::stub(ticker, "€100.00", "", "Some Corp");
            x.instrument_type = "EQUITY".into();
            x.quote_currency = Some(ccy.into());
            x
        };
        let mut coin = Quote::stub("BTC-EUR", "€100.00", "", "Bitcoin");
        coin.instrument_type = "CRYPTOCURRENCY".into();
        coin.quote_currency = Some("EUR".into());
        let quotes = vec![etf("SPYL.DE", "EUR"), etf("DARK.DE", "EUR"), stock("VOD.L", "GBp"), coin];
        let mut holdings: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        // a Xetra-listed S&P 500 tracker: quoted in EUR, 80% of its visible weight in US names
        holdings.insert(
            "SPYL.DE".into(),
            vec![("AAPL".into(), 0.16), ("MSFT".into(), 0.16), ("ASML.AS".into(), 0.08)],
        );
        // DARK.DE is deliberately absent from `holdings` -> unknown, never its EUR listing
        let book: Vec<String> =
            ["SPYL.DE", "DARK.DE", "VOD.L", "BTC-EUR"].iter().map(|s| (*s).to_string()).collect();
        let mix: HashMap<String, f64> = currency_mix(&book, &quotes, &holdings).into_iter().collect();
        // SPYL.DE's 0.32/0.40 US slice = 0.8 of one row out of four
        assert!((mix["USD"] - 0.20).abs() < 1e-9, "the listing is EUR and the exposure is USD: {mix:?}");
        assert!((mix["EUR"] - 0.05).abs() < 1e-9, "only the ASML leg is really EUR: {mix:?}");
        assert!((mix["GBP"] - 0.25).abs() < 1e-9, "GBp is GBP under a different unit: {mix:?}");
        assert!((mix["crypto"] - 0.25).abs() < 1e-9, "a coin carries coin risk, not dollar risk: {mix:?}");
        assert!((mix["?"] - 0.25).abs() < 1e-9, "a fund with no look-through is unknown: {mix:?}");
        assert!((mix.values().sum::<f64>() - 1.0).abs() < 1e-9, "shares must exhaust the book: {mix:?}");
        // ordering is share-desc then alphabetical, so the reader meets the biggest slice first
        let ordered = currency_mix(&book, &quotes, &holdings);
        assert_eq!(ordered[0].0, "?", "0.25 three ways -> alphabetical, and `?` must not be hidden");
        let line = currency_mix_line(&book, &quotes, &holdings).expect("a non-empty book prints");
        assert!(line.contains("USD 20%"), "{line}");
        assert!(line.contains("≥40% of each fund"), "coverage quotes the WORST fund: {line}");
        // no rows resolvable -> no line at all, rather than a footer claiming a 0% book
        assert_eq!(currency_mix_line(&book, &[], &holdings), None);
        // no fund in the book -> nothing extrapolated, so no coverage clause to disclaim
        let solo = vec!["VOD.L".to_string()];
        let line = currency_mix_line(&solo, &quotes, &HashMap::new()).unwrap();
        assert!(line.contains("GBP 100%") && !line.contains("look-through covers"), "{line}");
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

    /// The gate tails: the n-arity block (2 and 3), the long-leg floor block, and the four behaviours
    /// that are easy to break silently — dedup by FUND NAME before the histogram (so its counts match
    /// the rows), the pinned skip, the empty note that ONLY the three-gate block carries, and the floor
    /// block admitting a GROSS miss beside a second failing gate (the AMZN case every other tail drops).
    #[test]
    fn gate_tail_blocks() {
        // 8Y is the long leg here (the ladder is 20/8/5), so 5Y is free to sit under its own floor
        // without moving the CAGR — which is the whole shape of the case this block exists for.
        let q = |ticker: &str, name: &str, range: f64, age: f64, l: &[(&str, f64)]| {
            let mut quote = Quote::stub(ticker, "€1.00", "", name);
            quote.avg_turnover_eur = Some(1e9); // else "no-turnover" -> not assessable at all
            quote.range_pct = range;
            quote.age_years = Some(age);
            quote.perf = core::HORIZONS
                .iter()
                .map(|(lab, _)| l.iter().find(|(pl, _)| pl == lab).map(|(_, v)| ("x".to_string(), *v)))
                .collect();
            quote
        };
        let tuning = config::BuyHeuristic {
            growth_min_5y_pct: 75.0,
            growth_min_age_years: 5.0,
            ..config::BuyHeuristic::default()
        };
        // AMZN: 5Y +25 against a +75 bar = a GROSS miss, plus `range` — two gates, one of them gross.
        // 8Y +214% ≈ 15%/yr clears the CAGR floor, so it is a genuine compounder the 5Y bar rejects.
        let amzn = |t: &str| q(t, "Amazon.com, Inc.", 75.0, 20.0, &[("1Y", 10.0), ("5Y", 25.0), ("8Y", 214.0)]);
        // three CLOSE gates: young (4 vs 5), range (75 vs 80), cagr (7.0%/yr vs 8.0). 5Y +100 clears
        // its floor deliberately — a fourth failure would drop this out of the n=3 block.
        let three = q("THR", "Three Gate Co", 75.0, 4.0, &[("1Y", 10.0), ("5Y", 100.0), ("8Y", 71.8)]);
        let pinned = q("PIN", "Pinned Co", 75.0, 20.0, &[("1Y", 10.0), ("5Y", 25.0), ("8Y", 214.0)]);
        let quotes = vec![amzn("AMZN"), amzn("AMZN.DE"), three, pinned];
        let pins = vec!["PIN".to_string()];

        let two = multi_gate_lines(&quotes, &pins, &tuning, 2, "TWO", None);
        assert!(two.is_empty(), "AMZN fails two gates but misses 5Y grossly -> a hard reject, not a near miss: {two:?}");
        let three_block = multi_gate_lines(&quotes, &pins, &tuning, 3, "THREE", Some("none this run"));
        assert_eq!(three_block[1], "THREE (commonest: young & range & cagr 1):", "gates joined with \" & \", never \"+\" (one gate is named `1Y+`)");
        assert_eq!(three_block.len(), 3, "one row, and the pinned name is not it: {three_block:?}");
        assert!(three_block[2].contains("THR") && three_block[2].contains("young:") && three_block[2].contains("cagr:"));
        // the empty note is the point of this block: "none this run" and "not looked at" must differ
        assert_eq!(multi_gate_lines(&[], &pins, &tuning, 3, "THREE", Some("none this run")), vec!["", "none this run"]);
        assert!(multi_gate_lines(&[], &pins, &tuning, 2, "TWO", None).is_empty(), "the two-gate block still vanishes when empty");

        let floor = leg_floor_lines(&quotes, &pins, &tuning, "5Y+", "5Y", "growth_min_5y_pct", tuning.growth_min_5y_pct);
        assert_eq!(floor.len(), 4, "blank + header + ONE deduped Amazon row + footer, pinned skipped: {floor:?}");
        assert!(floor[2].contains("AMZN ") && floor[2].contains("5Y +25.0%") && floor[2].contains("· also: range"));
        assert!(!floor[2].contains("AMZN.DE"), "one row per FUND — the second venue is the same name");
        assert!(floor[3].contains("growth_min_5y_pct` 75"), "the footer names the knob at its LIVE value: {}", floor[3]);
        // floor off -> the gate never fails -> the block self-suppresses (this is the shipped 8Y case)
        assert!(leg_floor_lines(&quotes, &pins, &tuning, "8Y+", "8Y", "growth_min_8y_pct", tuning.growth_min_8y_pct).is_empty());
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
            Snapshot { date: date.into(), spx: None, spx_off_hi: None, aum: Vec::new(), core: Vec::new(), rows }
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
            Snapshot { date: date.into(), spx: None, spx_off_hi: None, aum: Vec::new(), core: Vec::new(), rows }
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
            core: Vec::new(),
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
            Snapshot { date: date.into(), spx: None, spx_off_hi: None, aum: Vec::new(), core: Vec::new(), rows }
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
            core: Vec::new(),
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

    /// (#115) STRUCTURAL PIN — and the bug it exists for was real, not hypothetical. `run` used to
    /// read the journal six separate times and split-restate only the first of them. Five footers
    /// read RANKS, so they did not care. The sixth, `fund_flow_lines`, divides price appreciation
    /// out of AUM growth and therefore reads journaled PRICES — un-restated ones. AUM is
    /// split-neutral while a journaled close is not, so a 2:1 split halved the price leg and the
    /// footer printed roughly +100% net inflow that never happened, flattering a name on a buy
    /// surface. There is now ONE read, restated once, shared by all six. A seventh call site would
    /// silently re-open the hole, so count them here rather than trust review — `run` is 1400 lines
    /// and the mutation gate skips it, so this file gets no other structural grading.
    ///
    /// Both needles are assembled with `concat!` ON PURPOSE: written as single literals they would
    /// occur in this test's own source and count themselves. For the same reason, no comment or
    /// message anywhere in this file may write either call with its parenthesis attached.
    #[test]
    fn the_journal_is_read_once_and_restated_once() {
        let src = include_str!("screen.rs");
        let reads = src.matches(concat!("read_snapshots", "()")).count();
        assert_eq!(
            reads, 1,
            "the journal must be read ONCE and the same restated copy handed to every footer; \
             found {reads} reads. Reuse the `snaps` binding — a fresh read is un-restated, and the \
             fund-flow footer reads prices, not ranks."
        );
        let restatements = src.matches(concat!("adjust_for_splits", "(")).count();
        assert_eq!(
            restatements, 1,
            "one read, one restatement; found {restatements} calls to adjust_for_splits. Two would \
             still be correct (it is idempotent), but they mean two journals again."
        );
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

    /// (funnel) The tally's four claims: the arithmetic CLOSES (`scanned = refused + failed +
    /// cleared` — a funnel that doesn't reconcile is silently losing names and can't aim a knob),
    /// a 1-fail name is the only kind that lands in sole-blocked, a 2-fail name lands in the fail
    /// counts of BOTH gates and in neither sole column, a refusal lands only in its own bucket, and
    /// a gate nobody failed does not print at all.
    #[test]
    fn funnel_semantics() {
        // ci-settings ships these; the code defaults are 0.0 (= off), which would gate nothing. The
        // crypto cap is its own knob — set it too, or the coin below clears and the lanes don't split.
        let t = config::BuyHeuristic {
            growth_maxdd_cap: 84.0,
            growth_maxdd_cap_crypto: 84.0,
            growth_max_above_ma: 150.0,
            ..config::BuyHeuristic::default()
        };
        let q = |ticker: &str, name: &str, kind: &str| {
            let mut x = Quote::stub(ticker, "€100.00", "", name);
            x.instrument_type = kind.into();
            x.avg_turnover_eur = Some(1e9);
            x.range_pct = 90.0;
            x.perf = core::HORIZONS
                .iter()
                .map(|(l, _)| match *l {
                    "1M" => Some((l.to_string(), 2.0)),
                    "1Y" => Some((l.to_string(), 20.0)),
                    "5Y" => Some((l.to_string(), 200.0)),
                    _ => None,
                })
                .collect();
            x
        };
        let clean = q("CLEAN", "Clean Corp", "EQUITY");
        // ZDEEP sorts LAST alphabetically and AAA.L first, so a flat sort would print AAA.L, SOL-EUR,
        // ZDEEP — the ordering this test exists to reject. Class order must beat the alphabet.
        let mut dd = q("ZDEEP", "Deep Corp", "EQUITY");
        dd.max_drawdown_pct = 95.0; // one gate, alone
        let mut dd2 = q("YDEEP", "Deeper Corp", "EQUITY"); // second stock -> pins the intra-class ", "
        dd2.max_drawdown_pct = 95.0;
        let mut dd_etf = q("AAA.L", "Deep Fund UCITS ETF", "ETF");
        dd_etf.max_drawdown_pct = 95.0;
        let mut both = q("BOTH.L", "Both Fund UCITS ETF", "ETF");
        both.max_drawdown_pct = 95.0;
        both.above_ma_pct = 400.0; // two gates -> blamed for neither
        let mut coin = q("SOL-EUR", "Solana", "CRYPTOCURRENCY");
        coin.max_drawdown_pct = 95.0;
        let lev = q("LEVX.L", "Some Index 2x Daily Leveraged", "ETF"); // refused, never a fail
        let quotes = vec![clean, dd, dd2, dd_etf, both, coin, lev];

        let lines = funnel_lines(&quotes, &t);
        let row = |gate: &str| lines.iter().find(|l| l.trim_start().starts_with(gate)).cloned();
        let last = lines.last().unwrap();
        assert!(last.contains("scanned 7 = refused 1 + failed 5 + cleared 1"), "arithmetic must close: {last}");
        assert!(lines.iter().any(|l| l.contains("refused (not assessable): leveraged 1")), "{lines:#?}");

        // the three `fail / sole` cells in column order: stocks, ETFs, crypto
        let cells = |row: &str| {
            row.split_whitespace()
                .collect::<Vec<_>>()
                .windows(3)
                .filter(|w| w[1] == "/")
                .map(|w| format!("{}/{}", w[0], w[2]))
                .collect::<Vec<_>>()
        };
        let maxdd = row("maxdd").expect("maxdd row prints");
        assert_eq!(cells(&maxdd), ["2/2", "2/1", "1/1"], "the 2-fail fund is COUNTED (ETFs 2) and not BLAMED (sole 1): {maxdd}");
        // class-major ordering, `, ` inside a class and ` · ` between classes — all three at once.
        // A flat alphabetical sort would read "AAA.L, SOL-EUR, YDEEP, ZDEEP" here.
        assert!(maxdd.ends_with("YDEEP, ZDEEP · AAA.L · SOL-EUR"), "names run stocks · ETFs · crypto, not alphabetically: {maxdd}");
        assert!(!maxdd.contains("BOTH.L"), "a 2-fail name must never be named as sole-blocked: {maxdd}");
        let stretch = row("stretch").expect("stretch row prints");
        assert_eq!(cells(&stretch), ["0/0", "1/0", "0/0"], "same fund, its second gate — a fail with no sole blame: {stretch}");

        assert!(row("cagr").is_none(), "a gate nobody failed must not print: {lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("LEVX")), "a refusal is not a gate failure: {lines:#?}");
        assert!(funnel_lines(&[], &t).is_empty());
    }

    /// (fund valuation) cheapest-first ordering, pe-less funds silent, empty map -> None. The
    /// values are post-inversion real ratios (fetch-side pin owns the reciprocal trap) — a test that
    /// hard-codes 33.93 rather than Yahoo's raw 0.02947 is the cheapest guard against someone
    /// "fixing" the inversion back.
    ///
    /// (#37 funds) also pins the PEG half: the `— cut` marker must name exactly the funds
    /// `lane_split` drops, and a fund with no quote in this run prints its P/E with no PEG.
    #[test]
    fn fund_pe_line_semantics() {
        let mut m: picks::FundPeMap = HashMap::new();
        m.insert("IITU.L".into(), 33.93.into());
        m.insert("SPYL.DE".into(), 24.2.into());
        // no PEG anywhere without quotes: no long leg -> long_cagr_pct None -> peg_yield None.
        let off = config::BuyHeuristic::default();
        assert_eq!(fund_pe_line(&m, &[], &off).unwrap(), "SPYL.DE 24 · IITU.L 34");
        assert_eq!(fund_pe_line(&HashMap::new(), &[], &off), None);

        // 5Y cumulative +61.051% is exactly 10.0 %/yr, so the PEGs below are exact:
        // P/E 25 -> peg_yield (100/25)*10 = 40 -> PEG 2.50, under the 50 bar (PEG 2.0) -> cut.
        // P/E  4 -> peg_yield (100/4)*10 = 250 -> PEG 0.40, clear.
        let fund = |t: &str| {
            let mut q = core::Quote::stub(t, "€100.00", "", &format!("{t} UCITS ETF"));
            q.instrument_type = "ETF".into();
            q.perf = vec![None; core::HORIZONS.len()];
            q.perf[core::HORIZONS.iter().position(|(l, _)| *l == "5Y").unwrap()] =
                Some((String::new(), 61.051));
            q
        };
        let quotes = vec![fund("DEAR.L"), fund("CHEAP.L"), fund("GHOST.L")];
        let mut pe: picks::FundPeMap = HashMap::new();
        pe.insert("DEAR.L".into(), 25.0.into());
        pe.insert("CHEAP.L".into(), 4.0.into());
        pe.insert("ABSENT.L".into(), 12.0.into()); // fetched, but not a quote in this run -> P/E only
        let on = config::BuyHeuristic { growth_max_peg_etf: 2.0, ..off.clone() };
        assert_eq!(
            fund_pe_line(&pe, &quotes, &on).unwrap(),
            "CHEAP.L 4 (PEG 0.40) · ABSENT.L 12 · DEAR.L 25 (PEG 2.50 — cut)"
        );
        // ceiling off (0) = no verdict to print, but the PEG itself still shows.
        assert_eq!(
            fund_pe_line(&pe, &quotes, &off).unwrap(),
            "CHEAP.L 4 (PEG 0.40) · ABSENT.L 12 · DEAR.L 25 (PEG 2.50)"
        );

        // (#37 funds) a BORROWED P/E must never read like a measured one: `~` on the value, and the
        // source spelled out. This is the only signal you get that a number was inferred by a name
        // match — and it can cut the fund, so it has to survive any reformatting of this line.
        pe.insert("DEAR.L".into(), picks::FundPe { pe: 25.0, from: Some("TWIN.L".into()), as_of: None });
        let line = fund_pe_line(&pe, &quotes, &on).unwrap();
        assert!(line.contains("DEAR.L 25~ (PEG 2.50 — cut)"), "borrowed value marked in place: {line}");
        assert!(line.contains("DEAR.L←TWIN.L"), "and the source named: {line}");
        assert!(!line.contains("CHEAP.L 4~"), "a measured value is never marked: {line}");

        // (fund staleness) a CACHE-SERVED P/E carries `°` plus its as-of date. It acts exactly like a
        // fetched one — it can cut a fund — so the age has to be on the line, not inferable only from
        // the fact that Yahoo happened to be down. The two marks are independent and can co-occur.
        let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        pe.insert("CHEAP.L".into(), picks::FundPe { pe: 4.0, from: None, as_of: Some(d) });
        let line = fund_pe_line(&pe, &quotes, &on).unwrap();
        assert!(line.contains("CHEAP.L 4° (PEG 0.40)"), "cache-served value marked in place: {line}");
        assert!(line.contains("CHEAP.L 2026-08-13"), "and dated: {line}");
        assert!(!line.contains("CHEAP.L 4~"), "cached is not borrowed: {line}");
        assert!(line.contains("DEAR.L 25~ "), "a borrowed-but-fresh value keeps only its own mark: {line}");
    }

    /// (#37 funds) THE GATE ON THE INDEX-TWIN MATCHER. It decides that two differently-named funds hold
    /// the same book, and a wrong call feeds a fabricated P/E into a trim that removes funds from the
    /// table — so it is pinned against REAL Yahoo `longName` strings, captured 2026-08-02, with the real
    /// ratios beside them.
    ///
    /// The load-bearing assertion is the last one: group every fund that reports a P/E by `index_key` and
    /// demand each group be P/E-HOMOGENEOUS. Funds tracking one index report one ratio (Yahoo's fund P/E
    /// is index-level), so any key covering two different ratios has FUSED TWO DIFFERENT BOOKS. That is
    /// the exact failure mode the 2026-08-02 decision to widen `FUND_NOISE` bought — every token added
    /// there is a new way to fuse — and this is the check that makes the widening safe to revisit.
    /// It found 5 multi-fund keys and 0 heterogeneous ones at authoring time.
    #[test]
    fn index_key_never_fuses_two_different_books() {
        // (ticker, Yahoo longName, look-through P/E) — real rows from `.holdings_cache.json`.
        let funds: &[(&str, &str, f64)] = &[
            ("500X.AS", "State Street SPDR S&P 500 Leaders UCITS ETF", 25.5885),
            ("ANX.PA", "Amundi Index Solutions - Amundi Nasdaq-100 Swap ETF EUR Acc", 32.6797),
            ("ANXU.L", "Amundi Index Solutions - Amundi Nasdaq-100 Swap ETF USD Acc", 32.6797),
            ("AUM5.DE", "Amundi Index Solutions - Amundi S&P 500 Swap UCITS ETF EUR Acc", 26.8745),
            ("BATE.DE", "L&G Battery Value-Chain UCITS ETF", 19.9084),
            ("CSNDX.SW", "iShares VII PLC - iShares NASDAQ 100 UCITS ETF", 32.6904),
            ("CSSPX.MI", "iShares Core S&P 500 UCITS ETF USD (Acc)", 26.8817),
            ("EQGB.L", "Invesco EQQQ NASDAQ-100 UCITS ETF (GBP Hdg)", 32.6797),
            ("EQQQ.MI", "Invesco EQQQ NASDAQ-100 UCITS ETF", 32.6797),
            ("ESIF.L", "iShares MSCI Europe Financials Sector UCITS ETF", 12.8584),
            ("ETLX.DE", "L&G Gold Mining UCITS ETF", 11.9818),
            ("EXA1.AS", "iShares EURO STOXX Banks 30-15 UCITS ETF (DE) EUR Acc", 11.8399),
            ("EXXT.DE", "iShares NASDAQ-100 UCITS ETF (DE)", 32.6904),
            ("FLXK.L", "Franklin FTSE Korea UCITS ETF", 19.7785),
            ("GDX.L", "VanEck Gold Miners UCITS ETF", 13.5446),
            ("GDXJ.L", "VanEck Junior Gold Miners UCITS ETF", 13.2679),
            ("HMWA.L", "HSBC MSCI World UCITS ETF USD (Acc)", 24.2131),
            ("IAUP.L", "iShares V PLC - iShares Gold Producers UCITS ETF USD (Acc)", 13.5263),
            ("IITU.L", "iShares S&P 500 Information Technology Sector UCITS ETF USD (Acc)", 33.9328),
            ("ITWN.MI", "iShares MSCI Taiwan UCITS ETF USD (Dist)", 31.3676),
            ("JPNCHF.SW", "UBS Core MSCI Japan UCITS ETF hCHF acc", 18.6428),
            ("LYBK.DE", "Amundi Euro Stoxx Banks UCITS ETF Acc", 11.8399),
            ("NASD.L", "Amundi Core Nasdaq-100 Swap UCITS ETF Acc", 32.6797),
            ("PUST.PA", "Amundi PEA Nasdaq-100 UCITS ETF Acc", 32.6797),
            ("SEMI.AS", "iShares MSCI Global Semiconductors UCITS ETF USD Acc", 42.3908),
            ("SMH.L", "VanEck Semiconductor UCITS ETF", 44.4050),
            ("SPPW.DE", "State Street SPDR MSCI World UCITS ETF", 24.1721),
            ("SPXE.L", "Invesco S&P 500 Scored & Screened ETF Acc", 26.0349),
            ("SPYL.DE", "State Street SPDR S&P 500 UCITS ETF USD Acc", 26.8745),
            ("SPYL.L", "State Street SPDR S&P 500 UCITS ETF USD Acc", 26.8745),
            ("SSAC.L", "iShares MSCI ACWI UCITS ETF USD Acc", 23.1965),
            ("SXLK.L", "State Street SPDR S&P U.S. Technology Select Sector UCITS ETF", 33.9328),
            ("UETW.DE", "UBS Core MSCI World UCITS ETF USD acc", 24.2131),
            ("VALW.L", "State Street SPDR MSCI World Value UCITS ETF", 14.7124),
            ("VUAA.DE", "Vanguard S&P 500 UCITS ETF USD Accumulation", 26.9179),
            ("VVSM.DE", "VanEck Semiconductor UCITS ETF", 44.4050),
            ("VWRA.L", "Vanguard FTSE All-World UCITS ETF", 22.8938),
            ("WITS.AS", "iShares MSCI World Information Technology Sector Advanced UCITS ETF USD Inc", 34.6380),
            ("WTAI.MI", "WisdomTree Artificial Intelligence UCITS ETF - USD Acc", 34.8675),
            ("XAIX.DE", "Xtrackers Artificial Intelligence & Big Data UCITS ETF 1C", 23.7473),
            ("XDJE.DE", "Xtrackers Nikkei 225 UCITS ETF 2D EUR Hedged", 23.0044),
            ("XDPU.MI", "Xtrackers S&P 500 UCITS ETF 4C - USD", 26.8745),
            ("XDWT.L", "Xtrackers MSCI World Information Technology UCITS ETF 1C", 34.6500),
            ("XMTD.L", "Xtrackers MSCI Taiwan UCITS ETF 1C", 31.3283),
            ("XMUS.L", "Xtrackers MSCI USA Swap UCITS ETF 1C", 27.0051),
            ("XUTC.L", "Xtrackers MSCI USA Information Technology UCITS ETF 1D", 33.9328),
        ];

        // the pair the whole mechanism exists for: a swap fund and the physical fund on its index,
        // named by different issuers in a different word order.
        assert_eq!(
            index_key("Invesco Technology S&P US Select Sector UCITS ETF"),
            index_key("State Street SPDR S&P U.S. Technology Select Sector UCITS ETF"),
            "XLKS.L must resolve to SXLK.L — different issuer, different word order, one index"
        );
        // "State Street SPDR" is TWO stacked issuers and "Amundi Index Solutions - Amundi" is two more;
        // the strip loop has to keep going or neither name ever reduces to its index.
        assert_eq!(index_key("State Street SPDR S&P 500 UCITS ETF USD Acc"), index_key("iShares Core S&P 500 UCITS ETF USD (Acc)"));
        assert_eq!(index_key("Amundi Index Solutions - Amundi Nasdaq-100 Swap ETF EUR Acc"), index_key("Amundi Core Nasdaq-100 Swap UCITS ETF Acc"));
        // the product-line widening, earning its keep: a PEA wrapper IS the plain index fund.
        assert_eq!(index_key("Amundi PEA Nasdaq-100 UCITS ETF Acc"), index_key("Invesco EQQQ NASDAQ-100 UCITS ETF"));

        // refusals — each one costs an `n/a`, which is the cheap direction
        assert_eq!(index_key("VanEck Semiconductor UCITS ETF"), None, "one token is not an index identity");
        assert_ne!(
            index_key("Invesco EQQQ NASDAQ-100 UCITS ETF (GBP Hdg)"),
            index_key("Invesco EQQQ NASDAQ-100 UCITS ETF"),
            "`hedged`/`hdg` stays: a missed twin beats a wrong one"
        );
        assert_ne!(
            index_key("iShares MSCI World UCITS ETF"),
            index_key("iShares MSCI World ex USA UCITS ETF"),
            "a geographic exclusion is a different book, not a share class"
        );

        // THE GATE: one key, one book.
        let mut by_key: HashMap<Vec<String>, Vec<(&str, f64)>> = HashMap::new();
        for (t, name, pe) in funds {
            if let Some(k) = index_key(name) {
                by_key.entry(k).or_default().push((t, *pe));
            }
        }
        let fused = by_key.values().filter(|v| v.len() > 1).count();
        assert!(fused >= 5, "if the matcher stops pairing anything this test proves nothing (got {fused})");
        for (k, v) in &by_key {
            let (lo, hi) = v.iter().fold((f64::MAX, f64::MIN), |(l, h), (_, p)| (l.min(*p), h.max(*p)));
            assert!(
                hi - lo <= 0.01 * lo.abs(),
                "key {k:?} fuses two different books: {v:?} — a token in FUND_NOISE/FUND_ISSUERS is too greedy"
            );
        }
    }

    /// (#37 funds) `borrow_index_twins` end to end, and specifically its UNANIMITY RULE: a borrowed P/E
    /// is only handed over when every fund sharing the orphan's `index_key` reports the same ratio to
    /// within 1%. The test above proves the KEY does not fuse two books; this one proves that when it
    /// does anyway, the disagreement is caught and the orphan is left with no number rather than an
    /// invented one — the number can cut a fund off the table, so silence has to be the failure mode.
    ///
    /// Two groups, one per verdict. S&P 500: `SPY5.MI` orphaned, two sources 0.04% apart, so it borrows
    /// — from `CSSPX.MI`, the alphabetically first, whose cache date rides along so the `°` mark still
    /// says how old the borrowed value is. Nasdaq-100: `XNAS.DE` orphaned, sources at 20 and 30, which
    /// is exactly the fused-book signature, so it borrows nothing.
    ///
    /// Runs OFFLINE by construction and not by stubbing: every candidate is already in `fund_pe` and
    /// every orphan is in `bench`, which empties `todo` and skips the `yahoo_top_holdings` call
    /// outright. The names are real Yahoo `longName` strings so the keys are the ones the live matcher
    /// derives, not ones invented to make the test pass.
    #[tokio::test]
    async fn borrow_index_twins_demands_unanimity_before_lending_a_ratio() {
        let etf = |t: &str, name: &str| {
            let mut q = core::Quote::stub(t, "€100.00", "", name);
            q.instrument_type = "ETF".into();
            q
        };
        let quotes = vec![
            etf("SPY5.MI", "SPDR S&P 500 UCITS ETF"), // orphan: no P/E of its own
            etf("CSSPX.MI", "iShares Core S&P 500 UCITS ETF USD (Acc)"),
            etf("SPXS.L", "Invesco S&P 500 UCITS ETF Acc"),
            etf("XNAS.DE", "Xtrackers Nasdaq 100 UCITS ETF 1C"), // the other orphan
            etf("ANX.PA", "Amundi Nasdaq-100 UCITS ETF Acc"),
            etf("EQQQ.MI", "Invesco EQQQ NASDAQ-100 UCITS ETF"),
        ];
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 13).expect("a real date");
        let measured = |pe: f64, as_of: Option<chrono::NaiveDate>| picks::FundPe { pe, from: None, as_of };
        let fund_pe: picks::FundPeMap = [
            ("CSSPX.MI".to_string(), measured(26.87, Some(day))), // agree to 0.04% — well inside the 1% bar
            ("SPXS.L".to_string(), measured(26.88, None)),
            ("ANX.PA".to_string(), measured(30.0, None)), // 50% apart: two different books under one key
            ("EQQQ.MI".to_string(), measured(20.0, None)),
        ]
        .into_iter()
        .collect();
        let bench: Vec<String> = ["SPY5.MI", "XNAS.DE"].iter().map(|s| (*s).to_string()).collect();
        let client = reqwest::Client::builder().no_proxy().build().expect("test client");

        let got = borrow_index_twins(&client, &bench, &fund_pe, &quotes).await;
        assert_eq!(
            got,
            vec![(
                "SPY5.MI".to_string(),
                picks::FundPe { pe: 26.87, from: Some("CSSPX.MI".to_string()), as_of: Some(day) }
            )],
            "the agreeing group lends, named and dated; the disagreeing one lends nothing at all"
        );
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
