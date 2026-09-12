//! `size [TICKERS]` — suggested position sizes for the growth picks: weight ∝ score ÷ volatility
//! (vol-target) inside a per-class risk budget, then capped per name and per sector (`config::Sizing`).
//! READ-ONLY, never trades — you still type the qty into `trade` yourself. No TICKERS -> the last
//! `screen` run's RANKED candidates `(#260)`; `--watchlist` sizes the hand-typed list instead, and
//! `--picks` still names the default. The two had drifted far enough that the default was the worse
//! list: on 2026-09-07 the watchlist sized 4 names, 16.0% of gross, on a day the screen ranked 15.
//! Names that fail the growth gate are dropped (nothing to size). The rows can sum to under 100 when a
//! cap binds; that remainder is deliberately unallocated, not a rounding error — `(#246)` names the
//! broad-market default for it (CORE #1, off the last `screen` run) instead of leaving it in cash.
//! NOT advice.

use crate::picks::{crypto_adjust, growth_score, nupl_factor, perf_pct, size_weights};
use crate::{config, fetch};

/// (#260) Which ticker list `size` sizes, and the line to print about it. `None` = a hard stop the
/// caller reports and exits on.
///
/// THE DEFAULT IS THE RANKED BOOK, and that is a deliberate reversal of `(#248)`, which built
/// `--picks` as opt-in under the heading "WHY OPT-IN AND NOT A NEW DEFAULT". Its argument was against
/// a silent UNION of the hand-typed watchlist with the machine-ranked list, which "would make the
/// printed TOTAL unattributable". That objection does not reach a source SWITCH: exactly one list is
/// sized and the run says which. Its other reason — bare `size` staying byte-identical — is given up
/// on purpose, because the two lists had drifted to where the default was the worse one: on
/// 2026-09-07 the watchlist sized 4 names (16.0% of gross deployed) on a day the screen ranked 15.
///
/// PRECEDENCE, and every rung of it is load-bearing:
/// 1. An explicit ticker list ALWAYS wins, so `size AAPL` still sizes exactly AAPL. Typing names and
///    getting the ranked book back would be the same failure `(#248)` removed, mirrored.
/// 2. …unless `--picks` is also given, which keeps `(#248)`'s "ranked plus extras, minus duplicates"
///    (a repeated row would draw its class budget twice).
/// 3. `--watchlist` is the way back to the old default, and beats `--picks` when both are typed —
///    it is the explicit escape hatch, so it wins the contradiction.
/// 4. Otherwise the ranked book.
///
/// THE TWO NO-STATE PATHS DIFFER, and must. A typed `--picks` with nothing on file is a HARD STOP —
/// `(#248)`'s "NO SILENT FALLBACK ... the flag was typed on purpose and sizing a different list than
/// the one asked for is the exact failure this round removes", still exactly right. A bare `size`
/// with nothing on file falls back to the watchlist and SAYS SO, because there the user asked for no
/// list in particular and silence would be the drift all over again.
pub(crate) fn size_source(
    picks: bool,
    watchlist: bool,
    named: Vec<String>,
    ranked: Option<(String, Vec<String>)>,
    wl: &[String],
) -> Option<(Vec<String>, Option<String>)> {
    if watchlist {
        return Some((if named.is_empty() { wl.to_vec() } else { named }, None));
    }
    if !named.is_empty() && !picks {
        return Some((named, None));
    }
    match ranked {
        Some((date, mut list)) => {
            let note = format!("Sizing the {} ranked pick(s) from the {date} screen run.", list.len());
            let extra: Vec<String> = named.into_iter().filter(|t| !list.contains(t)).collect();
            list.extend(extra);
            Some((list, Some(note)))
        }
        None if picks => None,
        None => Some((
            wl.to_vec(),
            Some("No ranked picks on file — sizing the watchlist instead. Run `screen` for candidates.".into()),
        )),
    }
}

/// (#262) Indices of the first row per ISSUER, in rank order — every later row carrying a name already
/// seen is dropped. The caller's list MUST already be sorted best-first, because that is what decides
/// which listing of a twin pair survives.
///
/// WHY THIS EXISTS. `max_name_pct` reads "max 4.0%/name" and was applied per TICKER, so a company with
/// two European listings drew the cap twice: on 2026-09-07 ABEC.DE and ABEA.DE both sized 4.0% and the
/// printed book ran 8% Alphabet under a 4% cap, with nothing on the page saying so. The hold lane has
/// collapsed twins since `picks::hold_core_list` (`cores.retain(|q| seen.insert(q.name.as_str()))`);
/// the sizer never got the same treatment. Applied BEFORE `size_weights`, so a dropped row never draws
/// a share of its class budget — it is not a row that got capped to zero, it is not a row.
///
/// The key is `name.to_lowercase()`, matching `screen`'s fund dedup rather than `hold_core_list`'s raw
/// string. The two already disagree; this takes the safer of the pair rather than silently unifying
/// them, which would move the hold lane on a sizing round.
///
/// AN EMPTY NAME IS ALWAYS KEPT. Twins cannot be proven without a name, and merging every unnamed quote
/// into one row would be a data-quality bug wearing a risk control's clothes — non-negotiable #5,
/// missing data passes.
pub(crate) fn first_per_issuer(names: &[&str]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    (0..names.len())
        .filter(|&i| names[i].is_empty() || seen.insert(names[i].to_lowercase()))
        .collect()
}

/// (#286) THE EXECUTED BOOK: which candidates `size` actually funds, and at what weight. Every line
/// of it was lifted VERBATIM out of `run`, and the lift is the point. `run` is `#[mutants::skip]` —
/// it is wiring, and the attribute below says so in its own words — so the scoring, the crypto
/// adjust, the issuer dedup and the weighting have all sat where the mutation gate cannot reach
/// them. Here the gate grades them.
///
/// The second reason is `screen`, and it is the reason this round exists. `track` grades the ranked
/// top-10 EQUAL-WEIGHT, which is not the book anyone is told to buy: gate failures are dropped, one
/// row survives per issuer, and what is left is weighted by score / volatility inside a class budget
/// and then capped. Journalling that needs ONE spelling of it (non-negotiable #4), not a copy in
/// `screen` drifting against the original here. `size --picks` sizes the ranked list off
/// `.screen_state.json`, so a screen run calling this on its own `ranked_now` quotes computes the
/// same rows the user sees minutes later.
///
/// `nupl` comes IN and `cfactor`/`btc_1y` are derived HERE, rather than each caller deriving them:
/// two callers re-deriving a scoring input is exactly how the `cagr` column drifted off the score it
/// was printing (see `long_leg_fixed`'s doc for that case, which cost a round to find).
///
/// Returns `(quote, score, weight %, cap reason)` in sized order — best score first, one row per
/// issuer. EMPTY means nothing passed the growth gate; the caller says so in its own words, because
/// `size` and `screen` owe the user different sentences about it. No fetch, no I/O, no state read.
pub(crate) fn sized_book<'a>(
    quotes: &[&'a crate::core::Quote],
    tuning: &config::BuyHeuristic,
    sz: &config::Sizing,
    nupl: Option<f64>,
) -> Vec<(&'a crate::core::Quote, f64, f64, Option<&'static str>)> {
    // (Item 17) the SAME crypto NUPL + BTC-relative adjustments `screen`/`check` apply at render
    // time, so crypto sizes rank the way the picks tables showed them, not on the raw price-only
    // score. Equities pass through `crypto_adjust` unchanged.
    let cfactor = nupl_factor(nupl, tuning);
    let btc_1y = quotes.iter().find(|q| q.ticker.starts_with("BTC-")).and_then(|q| perf_pct(q, "1Y"));
    // score with the SAME growth lane `screen` uses; None = the name failed the growth gate -> not sized.
    let mut scored: Vec<_> = quotes
        .iter()
        .filter_map(|q| growth_score(q, tuning).map(|s| (*q, crypto_adjust(q, s, tuning, cfactor, btc_1y))))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1)); // best score first; total_cmp: a NaN score must not panic the sort
    // (#262) ... then one row per ISSUER, best-scoring listing wins. See `first_per_issuer` for why.
    let keep = first_per_issuer(&scored.iter().map(|(q, _)| q.name.as_str()).collect::<Vec<_>>());
    let scored: Vec<_> = keep.into_iter().map(|i| scored[i]).collect();
    // (Item 6) pass the asset class as the cluster key so a correlated block (all crypto) is one risk
    // bucket, not N independent bets.
    // (P5) `asset_class` rather than the raw `instrument_type` string: that field is Yahoo's free text,
    // so "EQUITY" and "" split the stock class into two buckets that each drew a full share. The sector
    // rides along for the stock-class sector cap.
    //
    // An empty `scored` needs no guard: `size_weights` finds no class carrying weight and returns an
    // empty vec by its own divide-by-zero rule, so the zip below yields nothing. A branch here would
    // be a second statement of that rule, and the one in `size_weights` is the one with the test.
    let weights = size_weights(
        &scored
            .iter()
            .map(|(q, s)| (*s, q.volatility_pct, crate::picks::asset_class(q), q.sector.as_deref()))
            .collect::<Vec<_>>(),
        sz,
    );
    scored.into_iter().zip(weights).map(|((q, s), (w, cap))| (q, s, w, cap)).collect()
}


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
    // (#260) the DEFAULT source is the ranked book now, not the watchlist. The whole decision — and
    // (#248)'s reversed argument — lives on `size_source`, where the mutation gate can reach it; `run`
    // is `#[mutants::skip]`. The flags are stripped from the ticker list or they would be fetched as
    // quotes. The state read is unconditional and costs one small local file; nothing is FETCHED on
    // any no-state path, which was (#248)'s actual concern.
    let picks = args.iter().any(|a| a == "--picks");
    let watchlist = args.iter().any(|a| a == "--watchlist");
    let named: Vec<String> = args.into_iter().filter(|a| a != "--picks" && a != "--watchlist").collect();
    let ranked = crate::commands::screen::last_ranked(
        std::fs::read_to_string(config::data_path(crate::commands::screen::SCREEN_STATE_FILE)).ok(),
    );
    let Some((tickers, note)) = size_source(picks, watchlist, named, ranked, &settings.tickers) else {
        println!("No ranked picks on file — run `screen` first, then `size --picks`.");
        return;
    };
    if let Some(note) = note {
        println!("{note}");
    }

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

    // (Item 17) whole-market NUPL, fetched once. The adjustment it drives lives in `sized_book`.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;

    // (#286) the whole pipeline — score, crypto adjust, issuer dedup, weight, cap — now lives in
    // `sized_book`, where the mutation gate can reach it and where `screen` reads the same rows to
    // journal them. Nothing below this line changed: the printing is what `run` was always for.
    let sz = &settings.sizing;
    let book = sized_book(&quotes.iter().collect::<Vec<_>>(), &settings.buy_heuristic, sz, nupl);

    if book.is_empty() {
        println!("No names pass the growth gate — nothing to size. (try `screen` for candidates)");
        return;
    }

    println!("Suggested sizes — weight ∝ score ÷ volatility WITHIN a class budget, then capped (READ-ONLY, NOT advice):");
    println!(
        "  budget {:.0}/{:.0}/{:.0} stock/ETF/crypto (renormalised over the classes present) · max {:.1}%/issuer · max {:.1}%/sector\n",
        sz.budget_stock, sz.budget_etf, sz.budget_crypto, sz.max_name_pct, sz.max_sector_pct,
    );
    println!("  {:<10} {:>7} {:>7} {:>7}  CAP", "TICKER", "SCORE", "VOL", "SIZE%");
    for &(q, s, w, cap) in &book {
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
    let total: f64 = book.iter().map(|(_, _, w, _)| w).sum();
    println!("  {:<10} {:>7} {:>7} {:>6.1}%", "TOTAL", "", "", total);
    if total < 99.5 {
        // (#246) ... and say WHERE it goes. Cash is the one asset guaranteed to lose over 20 years,
        // so a remainder with no destination is the most expensive thing this command can print —
        // `(#245)` measured it at 67% of gross, and 62 of those 67 points are stock-class budget
        // that no eligible single name can absorb. A broad all-world tracker IS that same equity
        // exposure in one row, and the tool already ranks those: the CORE shortlist, breadth-major,
        // so its first row is the widest one. Read off the last `screen` run (no fetch, no price),
        // dated so a stale shortlist shows as stale. No state file / no CORE yet -> the old line,
        // unchanged.
        //
        // (#253) ... and then SIZE it, instead of stopping at the sentence. `(#246)` named the home
        // and said in its own receipt that it "allocates nothing and buys nothing"; `(#248)`
        // re-measured the hole at 67.0% of gross and pre-registered it as still open. Two thirds of
        // a twenty-year equity budget parked in cash is not a neutral default — it is the one
        // position guaranteed to lose over that horizon, and it loses more than any ranking error
        // this command could make. The row deliberately sits OUTSIDE the `max_name_pct` regime,
        // which is `(#246)`'s argument taken at its word: that cap governs single-name risk, and a
        // whole-world tracker is the market rather than a name, so the 4%/name bar would leave 63
        // points still in cash and answer nothing. TOTAL above is untouched and still prints the
        // capped-basket sum — `(#245)`'s point, that a total of 33 is exactly how the caps announce
        // themselves, is still right and this must not paper over it. So: TOTAL, then the row, then
        // TOTAL+. SCORE/VOL print an em dash because this instrument never went through the growth
        // gate and must not look as if it had. Still READ-ONLY: no fetch, no price, no weight in
        // `size_weights` moved. No state file / no CORE yet / every CORE row already sized -> the
        // old line, verbatim, because a remainder with no known home is precisely what it is for.
        let rest = 100.0 - total;
        let sized: Vec<String> = book.iter().map(|(q, ..)| q.ticker.clone()).collect();
        match crate::commands::screen::last_core(
            std::fs::read_to_string(config::data_path(crate::commands::screen::SCREEN_STATE_FILE)).ok(),
            &sized,
            // (#285) `spill_cut()`, not the raw knob: the count of CORE rows that receive money is
            // read in ONE place now, because `track` grades exactly this many and the two must be the
            // same number by construction rather than by two matching `.max(1)`s.
            sz.spill_cut(),
        ) {
            Some((date, rows)) => {
                // (#259) ... and say HOW it replicates, when that is worth saying. This one row can
                // be two thirds of gross and sits outside `max_name_pct` by design, so a synthetic
                // wrapper entering it silently is the one disclosure the row was still missing.
                // Empty for physical AND for unknown — see `screen::spill_repl_note`, which owns the
                // rule; this end only prints what it is handed.
                //
                // (#261) ... and split it over the first `spill_names` of them rather than one. The
                // share is computed ONCE (non-negotiable #4) and every row prints its own CORE index,
                // so a walk-down that skipped an already-sized name still says which row it settled
                // on — the old line hardcoded "#1" and could name the third-broadest tracker as the
                // broadest. TOTAL+ is unchanged and still exact: it sums `total + rest`, never the
                // rounded per-row figures, so a remainder that does not divide evenly cannot drift it
                // off 100.0. At `spill_names: 1` every byte below is the `(#253)` line verbatim,
                // trailing sentence included — that is what makes the knob a real revert.
                let n = rows.len();
                let each = rest / n as f64;
                for (i, core, repl) in rows {
                    let note = repl.map(|r| format!(" · {r}")).unwrap_or_default();
                    println!(
                        "  {core:<10} {dash:>7} {dash:>7} {each:>6.1}%  broad-market · CORE #{rank}, {date} screen{note}",
                        dash = "—",
                        rank = i + 1,
                    );
                }
                println!("  {:<10} {:>7} {:>7} {:>6.1}%", "TOTAL+", "", "", total + rest);
                let home = if n == 1 {
                    "in one all-world tracker".to_string()
                } else {
                    format!("split equally over {n} all-world trackers")
                };
                println!(
                    "  (the {rest:.1}% the caps could not deploy, {home} — outside the per-name cap by design: it is the market, not a name. Or add candidates / raise a cap. NOT advice)"
                );
            }
            None => println!(
                "  ({rest:.1}% unallocated — the caps bind and no name in those classes can take more; add candidates or raise a cap)"
            ),
        }
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
        let sized: Vec<(String, String, Option<f64>, f64)> = book
            .iter()
            .map(|&(q, _, w, _)| {
                let key = if crate::picks::is_currency_quoted(&q.ticker) {
                    format!("c:{}", crate::picks::underlying(&q.ticker).to_lowercase())
                } else {
                    format!("s:{}", crate::picks::yahoo_base(&q.ticker))
                };
                (q.ticker.clone(), key, q.price_eur, w)
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

    /// A Quote that clears `picks::growth_score`'s whole gate stack, built from `core::Quote::stub`
    /// plus only the fields those gates actually read. It is NOT a copy of picks.rs's `buy_heuristic`
    /// fixture — that one is a 60-line struct literal local to a test fn and unreachable from here,
    /// and duplicating it would be a second spelling of "a scoring quote" (non-negotiable #4). The
    /// stub carries every other field at its own default, so a gate this fixture does not name is a
    /// gate that reads missing data — which passes, per non-negotiable #5.
    fn scoring_quote(ticker: &str, name: &str, cum_20y: f64, vol: f64) -> crate::core::Quote {
        let mut q = crate::core::Quote::stub(ticker, "€1.00", "", name);
        // Contiguous history: a real name carrying a 20Y leg carries every shorter one. The rungs
        // below are the same cumulative return annualized down, so `long_leg_fixed` picks 20Y and
        // every shorter gate reads a consistent number rather than a fixture artifact.
        let g = 1.0 + cum_20y / 100.0;
        q.perf = crate::core::HORIZONS
            .iter()
            .map(|(_l, d)| Some(("x".to_string(), (g.powf(*d as f64 / 7300.0) - 1.0) * 100.0)))
            .collect();
        q.avg_turnover_eur = Some(1e9); // (#20) unknown turnover is a hard refusal, so it must be known
        q.range_pct = 100.0; // at its high — the growth lane's on-sale mirror
        q.volatility_pct = Some(vol);
        q
    }

    /// (#286) `sized_book` — the whole `size` pipeline, lifted out of a `#[mutants::skip]` `run` so
    /// the gate can reach it and so `screen` can journal the same book without a second spelling.
    /// What this pins: it scores, it drops what the gate refuses, it keeps ONE row per issuer, and
    /// the weights it returns are the ones `run` prints.
    #[test]
    fn sized_book_is_the_executed_book() {
        let tuning = config::BuyHeuristic::default();
        let sz = config::Sizing::default();
        let quotes = vec![
            scoring_quote("AAA.DE", "Alpha Corp", 900.0, 2.0),
            scoring_quote("AAA2.DE", "Alpha Corp", 700.0, 2.0), // same issuer -> the loser is dropped
            scoring_quote("BBB.DE", "Beta Corp", 500.0, 4.0),
            crate::core::Quote::stub("DEAD.DE", "€1.00", "", "Gamma Corp"), // no history, no turnover
        ];
        let book = sized_book(&quotes.iter().collect::<Vec<_>>(), &tuning, &sz, None);

        let tickers: Vec<&str> = book.iter().map(|(q, ..)| q.ticker.as_str()).collect();
        assert_eq!(tickers, vec!["AAA.DE", "BBB.DE"], "gate refusals and the issuer dedup both bite: {tickers:?}");
        assert!(book[0].1 > book[1].1, "sorted by score, best first: {:?}", book.iter().map(|(_, s, ..)| *s).collect::<Vec<_>>());
        assert!(book.iter().all(|(_, _, w, _)| *w > 0.0), "a sized row carries a real weight");
        let total: f64 = book.iter().map(|(_, _, w, _)| w).sum();
        assert!(total > 0.0 && total <= 100.0, "weights are percentages of gross: {total}");

        // EMPTY is a real answer, not a bug: nothing cleared the gate, so there is nothing to fund.
        // This is the arm `run`'s early return and `screen`'s journal both depend on.
        let none = vec![crate::core::Quote::stub("DEAD.DE", "€1.00", "", "Gamma Corp")];
        assert!(sized_book(&none.iter().collect::<Vec<_>>(), &tuning, &sz, None).is_empty());
    }

    /// (#260) which list `size` sizes. The DEFAULT is the ranked book — the reversal of (#248)'s
    /// opt-in — so every rung of the precedence is pinned here, including the two that must NOT have
    /// changed: an explicit ticker list still wins outright, and a typed `--picks` with no state file
    /// is still a hard stop rather than a quiet fallback.
    #[test]
    fn size_source_prefers_the_ranked_book() {
        let v = |ts: &[&str]| ts.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        let wl = v(&["ABEA.DE", "IITU.L"]);
        let ranked = || Some(("2026-09-07".to_string(), v(&["ABEC.DE", "KLA.DE"])));

        // bare `size` -> the ranked book, and it says so. THE POINT OF THE ROUND.
        let (t, note) = size_source(false, false, Vec::new(), ranked(), &wl).unwrap();
        assert_eq!(t, v(&["ABEC.DE", "KLA.DE"]));
        assert_eq!(note.as_deref(), Some("Sizing the 2 ranked pick(s) from the 2026-09-07 screen run."));

        // an explicit list still wins outright — typing names and getting the ranked book back would
        // be (#248)'s own failure mirrored.
        let (t, note) = size_source(false, false, v(&["NVD.DE"]), ranked(), &wl).unwrap();
        assert_eq!((t, note), (v(&["NVD.DE"]), None));

        // `--picks` is now a synonym for the default, and still appends extras minus duplicates: a
        // repeated row would draw its class budget twice.
        let (t, _) = size_source(true, false, v(&["NVD.DE", "KLA.DE"]), ranked(), &wl).unwrap();
        assert_eq!(t, v(&["ABEC.DE", "KLA.DE", "NVD.DE"]), "KLA.DE is already ranked and must not repeat");

        // `--watchlist` is the way back, and reproduces the old default exactly.
        let (t, note) = size_source(false, true, Vec::new(), ranked(), &wl).unwrap();
        assert_eq!((t, note), (wl.clone(), None));
        // ... it also beats `--picks` when both are typed: the explicit escape hatch wins.
        let (t, _) = size_source(true, true, Vec::new(), ranked(), &wl).unwrap();
        assert_eq!(t, wl);
        // ... and still yields to an explicit list, like every other path.
        let (t, _) = size_source(false, true, v(&["NVD.DE"]), ranked(), &wl).unwrap();
        assert_eq!(t, v(&["NVD.DE"]));

        // THE TWO NO-STATE PATHS DIFFER, and that is (#248)'s rule kept. A typed `--picks` stops
        // dead; a bare `size` falls back to the watchlist and SAYS so.
        assert!(size_source(true, false, Vec::new(), None, &wl).is_none());
        let (t, note) = size_source(false, false, Vec::new(), None, &wl).unwrap();
        assert_eq!(t, wl);
        assert!(note.is_some_and(|n| n.contains("watchlist")), "the fallback must not be silent");
    }

    /// (#262) One row per ISSUER, and the FIRST one — rank order decides which listing of a twin pair
    /// keeps the slot, because `run` dedupes after the sort. The 2026-09-07 pair is the live case:
    /// ABEC.DE (score 19.3) and ABEA.DE (18.9) are both "Alphabet", and both used to size 4.0%.
    #[test]
    fn first_per_issuer_keeps_the_best_ranked_listing() {
        // the live case: twins collapse to the FIRST, everything else survives in order.
        assert_eq!(
            first_per_issuer(&["Alphabet", "Alphabet", "KLA", "Binance Coin"]),
            vec![0, 2, 3],
            "the twin must collapse onto the better-ranked venue",
        );

        // non-adjacent twins still collapse — the pair need not be neighbours in the ranking.
        assert_eq!(first_per_issuer(&["Alphabet", "KLA", "Alphabet"]), vec![0, 1]);

        // case differences are the same issuer (`screen`'s fund dedup keys the same way).
        assert_eq!(first_per_issuer(&["Alphabet", "ALPHABET", "alphabet"]), vec![0]);

        // distinct names ALL survive: this must not be able to thin a book that has no twins.
        assert_eq!(first_per_issuer(&["A", "B", "C"]), vec![0, 1, 2]);

        // AN EMPTY NAME IS ALWAYS KEPT — non-negotiable #5, missing data passes. Merging unnamed
        // quotes would be a data-quality bug wearing a risk control's clothes.
        assert_eq!(first_per_issuer(&["", "", "A", ""]), vec![0, 1, 2, 3]);

        assert!(first_per_issuer(&[]).is_empty(), "empty in -> empty out");
    }

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
