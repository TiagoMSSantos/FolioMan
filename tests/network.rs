//! LIVE smoke tests — the only thing that catches a real API changing shape under the parsers
//! (Yahoo chart schema, FX, the full Quote build). READ-ONLY endpoints only — never a broker/order call.
//!
//! They run on EVERY `cargo test`, unguarded, because they tell a transient hiccup apart from real
//! drift — so a red here MEANS something:
//!   - transport error / HTTP 429 / 5xx / 401 / 403 / Yahoo's own error envelope  -> SKIP (environmental)
//!   - HTTP 200 but the fields the parser needs are gone                          -> FAIL (API drift)
//! Offline is just the transport-error arm, so a plane, a VPN or a dead endpoint skips GREEN. That
//! classifier is what makes an always-on live test safe; without it these would need a gate.
//! Skips print to stderr, which `cargo test` swallows on a pass — use `-- --nocapture` to read them
//! (CI does), or a net that has been silently skipping for months looks exactly like a passing one.
//!
//! Every raw probe goes through `fetch::throttle`, the same pacer production uses, so the file cannot
//! open an unpaced side channel. Whole run is ~10 Yahoo calls against the 4757 a single `screen` does.
//!
//! NOTHING in this file is `#[ignore]`. The last holdout, `backtest_edge_holds`, shells
//! `backtest 12 universe` over ~4900 live tickers and measures REGIME — whether the shipped edge still
//! holds on today's market. It now runs under the same skip-when-you-can't contract as everything else:
//! release profile + a warm `.long_history_cache.json` and it runs (~127s, almost no network); debug or
//! cold and it prints why and skips green. `FOLIOMAN_BACKTEST_GATE=1` FORCES it past both guards — that
//! is how the nightly job runs it on a runner, which is always debug-cold-cache territory:
//!
//!     FOLIOMAN_BACKTEST_GATE=1 cargo test --release --test network backtest_edge_holds -- --nocapture
//!
//! The deterministic half of its old job (did a code or knob edit change the scoring) split off into
//! `shipped_tuning_scores_fixture_unchanged` in src/commands/backtest.rs, offline, 0.02s.

// note: separate test crate, so the lib's crate-root allow doesn't reach here. Same call: docs render fine.
#![allow(clippy::doc_lazy_continuation)]

use folioman::{config, fetch};
use serde_json::Value;

/// (tests round 4) Stamp the pull so `screen` can nag when the nets go stale — a net nobody runs
/// catches nothing. Once per process; a run that ends in SKIPPED lines still counts (the family
/// executed and a human saw the skips). Never fails a probe.
///
/// Kept after the env gate was dropped: the nag still means something for someone who runs `screen`
/// daily and `cargo test` never.
fn stamp_run() {
    static STAMP: std::sync::Once = std::sync::Once::new();
    STAMP.call_once(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::write(config::data_path(config::NET_STAMP_FILE), now.to_string());
    });
}

enum Probe {
    Healthy(Value), // 200 + the chart contract is present -> the body to hand the real parser
    Skip(String),   // environmental (throttle/transport/bad-symbol envelope) -> no-op, don't fail
    Drift(String),  // 200 OK but the shape parse_chart depends on is gone -> a real regression
}

/// Status-aware probe of the live Yahoo chart endpoint. Separates "the network/API is having a moment"
/// (skip) from "the contract the parser relies on changed" (fail) — the distinction the CI shell can't make.
async fn probe_yahoo(ticker: &str) -> Probe {
    stamp_run();
    let urls = config::load().urls;
    let url = urls.yahoo_chart.replace("{ticker}", ticker).replace("{range}", "1y");
    fetch::throttle().await; // same pacer production uses — no unpaced side channel from the tests
    let resp = match fetch::client().get(&url).send().await {
        Ok(r) => r,
        Err(e) => return Probe::Skip(format!("transport error: {e}")),
    };
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        return Probe::Skip(format!("throttled/unavailable: HTTP {status}"));
    }
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Probe::Skip("200 but body wasn't JSON (likely a throttle/HTML page)".into()),
    };
    // the exact fields parse_chart reads: chart.result[0].timestamp (non-empty) + .indicators.quote[0].close
    let has_ts = body
        .pointer("/chart/result/0/timestamp")
        .and_then(|t| t.as_array())
        .is_some_and(|a| !a.is_empty());
    let has_close = body.pointer("/chart/result/0/indicators/quote/0/close").is_some();
    if has_ts && has_close {
        Probe::Healthy(body)
    } else if !body.pointer("/chart/error").is_none_or(|e| e.is_null()) {
        Probe::Skip(format!("Yahoo error envelope: {}", body.pointer("/chart/error").unwrap()))
    } else {
        Probe::Drift("HTTP 200 but chart.result[0].timestamp/close missing and no error envelope".into())
    }
}

/// Resolve a probe: `None` (caller should `return`/`continue`) when skipped, the healthy body when
/// healthy, panics on drift. Returning the BODY is what lets a caller assert against the very response
/// it just classified, instead of issuing a second request that may land on a different verdict.
fn healthy_or_skip(p: Probe, ctx: &str) -> Option<Value> {
    match p {
        Probe::Healthy(body) => Some(body),
        Probe::Skip(why) => {
            eprintln!("network smoke [{ctx}] SKIPPED — {why}");
            None
        }
        Probe::Drift(why) => panic!("API DRIFT [{ctx}]: {why}"),
    }
}

#[tokio::test]
async fn yahoo_history_parses() {
    for ticker in ["AAPL", "BTC-USD"] {
        let Some(body) = healthy_or_skip(probe_yahoo(ticker).await, ticker) else {
            continue; // endpoint throttled for this symbol — don't fail the suite
        };
        // Parse the body the probe ALREADY fetched, not a fresh `fetch_history` GET. One request per
        // ticker instead of two (three, counting `chart_json`'s retry), and — the part that matters —
        // the response asserted on is the same one classified healthy, so a throttle landing between
        // probe and fetch can no longer masquerade as parser drift and red the build.
        // The probe's range is 1y rather than fetch_history's 10y: same schema, smaller payload, and
        // every field below lives in the meta, so the shorter window tests the identical contract.
        let chart = fetch::parse_chart(&body, ticker)
            .unwrap_or_else(|| panic!("{ticker}: 200 OK but parse_chart returned None — drift"));
        let (dates, closes) = (&chart.dates, &chart.closes);
        assert!(!closes.is_empty(), "{ticker}: no closes");
        // (FX) the backtest decides whether to convert by comparing this to the filer's reporting
        // currency. An empty string compares unequal to everything, so a silent drop here would start
        // converting US names against themselves — assert the meta field actually arrives.
        assert!(!chart.currency.is_empty(), "{ticker}: chart meta carried no currency");
        // the backtest classes every name from these two fields alone (`stamp_asset_class`), because
        // `backtest_quote` rebuilds quotes from prices only. If Yahoo drops instrumentType, every fund
        // silently falls back to the name guess and most of them re-class as single stocks — exactly
        // the regression the class stamping exists to end. Fail here rather than in a quiet peer-mean.
        assert!(!chart.instrument_type.is_empty(), "{ticker}: chart meta carried no instrumentType");
        assert!(!chart.name.is_empty(), "{ticker}: chart meta carried no name");
        assert_eq!(dates.len(), closes.len(), "{ticker}: dates/closes length mismatch");
        assert!(dates.windows(2).all(|w| w[0] <= w[1]), "{ticker}: dates not ascending");
        assert!(closes.iter().all(|c| c.is_finite() && *c > 0.0), "{ticker}: bad close value");
    }
}

#[tokio::test]
async fn fx_rate_resolves() {
    // FX resolves via the same Yahoo chart endpoint (USDEUR=X) -> gate on its health first. The body
    // is discarded here on purpose: `eur_rate` owns cache lookup + rate selection, not just the parse,
    // so this one genuinely needs the real call rather than the probe's payload.
    if healthy_or_skip(probe_yahoo("USDEUR=X").await, "FX USDEUR=X").is_none() {
        return;
    }
    let settings = config::load();
    let rate = fetch::eur_rate(&fetch::client(), &settings.urls, "USD", &fetch::fx_cache()).await;
    let r = rate.expect("Yahoo healthy but USD->EUR rate did not resolve — drift");
    assert!(r.is_finite() && r > 0.0, "implausible FX rate: {r}");
}

#[tokio::test]
async fn full_quote_build() {
    if healthy_or_skip(probe_yahoo("AAPL").await, "full_quote_build").is_none() {
        return;
    }
    let settings = config::load();
    // ONE ticker, not two. A full `fetch::quotes` build is the most expensive call in this file (10y
    // chart + monthly-max series + fundamentals + FX, per name) and this test is about the assembly
    // path, which one name exercises exactly as well. BTC-USD keeps its coverage in the chart net above.
    let tickers = vec!["AAPL".to_string()];
    let quotes = fetch::quotes(
        &fetch::client(), &settings.urls, &fetch::fx_cache(), &tickers,
        settings.dip_days, settings.high_days, false, false, &settings.anchor_windows, None,
    )
    .await;
    assert_eq!(quotes.len(), tickers.len());
    for q in &quotes {
        assert!(q.price != "err" && q.price != "no data", "{}: failed to fetch ({})", q.ticker, q.price);
        assert!((0.0..=100.0).contains(&q.range_pct), "{}: range_pct out of range: {}", q.ticker, q.range_pct);
        assert!(q.perf.iter().any(|p| p.is_some()), "{}: no horizon % parsed", q.ticker);
    }
}

/// (round 66) topHoldings drift net. The holdings pipeline (screen's overlap/concentration footers,
/// check's holdings block) degrades SILENTLY by design: any parse miss becomes an empty list that the
/// weekly cache then serves for a week — if Yahoo reshapes the quoteSummary topHoldings payload, the
/// footers just vanish and nothing ever reds. This is the one place that failure mode turns into a
/// red build. Uses the cacheless `top_holdings_live` core, so the run never touches (or is fooled by)
/// `.holdings_cache.json`. Same contract as the probes above: environmental -> skip, 200-but-empty
/// on a major equity ETF -> FAIL (drift).
#[tokio::test]
async fn yahoo_top_holdings_parses() {
    stamp_run();
    // a symbol the screen actually prints (sector-tech table) — the exact payload class the footers eat
    const SYM: &str = "IITU.L";
    match fetch::top_holdings_live(&fetch::client(), SYM).await {
        Err(why) => eprintln!("network smoke [topHoldings {SYM}] SKIPPED — {why}"),
        Ok(holdings) => {
            assert!(
                !holdings.is_empty(),
                "{SYM}: 200 OK but no holdings parsed — quoteSummary topHoldings payload drifted \
                 (the screen's overlap/concentration footers are silently empty right now)"
            );
            assert!(holdings.len() <= 10, "{SYM}: parser returned {} rows, contract is top-10", holdings.len());
            assert!(
                holdings.iter().any(|(_, w)| *w > 0.0),
                "{SYM}: holdings parsed but every holdingPercent weight is 0 — weight field drifted \
                 (the top-heavy concentration footer is silently blind)"
            );
        }
    }
}

/// (tests round 3) SEC drift net. `fetch_fundamentals_sec` feeds the ONE validated ranking tilt
/// (earnings_yield, fund_source "sec"): a live XBRL payload reshape makes `fund_factor` None and the
/// tilt goes silently OFF while every offline parse pin stays green — this is the only place that
/// failure mode reds. Uses the cacheless `sec_facts_live` core, so the run never tests the disk
/// cache instead of the wire. Environmental (CIK map down/throttle) -> skip; fetched-but-unparsable
/// on a major US filer -> FAIL (drift).
#[tokio::test]
async fn sec_facts_parse() {
    stamp_run();
    match fetch::sec_facts_live(&fetch::client(), &config::load().urls, "AAPL").await {
        Err(why) => eprintln!("network smoke [SEC facts AAPL] SKIPPED — {why}"),
        Ok(rows) => {
            assert!(
                rows.len() >= 5,
                "AAPL: only {} annual facts rows parsed (a major US filer carries ~19y) — \
                 companyfacts payload drifted (the earnings_yield tilt is starving)",
                rows.len()
            );
            assert!(
                rows.iter().any(|r| r.eps.is_some()),
                "AAPL: facts parsed but no EPS in any row — the earnings_yield numerator is gone \
                 (the ranking tilt is silently OFF right now)"
            );
        }
    }
}

/// (tests round 3) Yahoo fund-facts drift net — the crumb-gated fallback that fills ETF TER/AUM
/// holes (the TER drag, AUM gate and bridge hints ride it). Proven silent-degrade: it failed live
/// on 2026-07-17 ("Yahoo crumb handshake failed — fund-facts fallback skipped this run") and
/// nothing red. Same contract as the topHoldings net: crumb/transport/throttle -> skip;
/// 200-but-factless on a major equity ETF -> FAIL (drift).
#[tokio::test]
async fn yahoo_fund_facts_parse() {
    stamp_run();
    match fetch::fund_facts_live(&fetch::client(), "IITU.L").await {
        Err(why) => eprintln!("network smoke [fund facts IITU.L] SKIPPED — {why}"),
        Ok((ter, aum)) => {
            let t = ter.expect(
                "IITU.L: 200 OK but no TER parsed — fundProfile expense-ratio drifted \
                 (ETF TER cells are silently n/a right now)",
            );
            assert!(t > 0.0 && t < 5.0, "IITU.L: implausible TER {t}% (parse should yield ~0.15)");
            let a = aum.expect(
                "IITU.L: 200 OK but no AUM parsed — summaryDetail totalAssets drifted \
                 (the AUM gate and CORE sizing are silently blind)",
            );
            assert!(a > 1e8, "IITU.L: implausible AUM {a} for a multi-billion ETF");
        }
    }
}

/// (round 79) Generic status probe for the non-Yahoo endpoints: transport/throttle/auth failures
/// SKIP; anything else is left to the real parser — a healthy endpoint plus a None parse is the
/// drift verdict. (probe_yahoo stays separate: it also checks the chart contract + error envelope.)
async fn probe_url(url: &str, ctx: &str) -> bool {
    stamp_run();
    fetch::throttle().await; // paced like every other outbound call in the project
    let resp = match fetch::client().get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("network smoke [{ctx}] SKIPPED — transport error: {e}");
            return false;
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        eprintln!("network smoke [{ctx}] SKIPPED — throttled/unavailable: HTTP {status}");
        return false;
    }
    true
}

/// (round 79) NUPL drift net. `fetch_nupl` feeds the crypto damp/boost in screen and size, and its
/// None path is silent BY DESIGN (the factor just stays neutral) — so a bitcoin-data.com payload
/// reshape would quietly disable the euphoria brake forever. Healthy endpoint + None parse = red.
#[tokio::test]
async fn nupl_parses() {
    let settings = config::load();
    if !probe_url(&settings.urls.nupl, "NUPL").await {
        return;
    }
    let nupl = fetch::fetch_nupl(&fetch::client(), &settings.urls)
        .await
        .expect("NUPL endpoint healthy but fetch_nupl returned None — payload shape drifted (the crypto damp is silently neutral right now)");
    assert!((-1.0..2.0).contains(&nupl), "implausible NUPL value: {nupl}");
}

/// (round 79) Euribor drift net. `fetch_euribor_3m` scrapes euribor-rates.eu HTML (fragile, says
/// its own doc) for the check footer's Certificados de Aforro baseline; a page redesign = silent
/// permanent None. Healthy endpoint + no parsable rate = red.
#[tokio::test]
async fn euribor_parses() {
    let settings = config::load();
    if !probe_url(&settings.urls.euribor, "Euribor 3M").await {
        return;
    }
    let rate = fetch::fetch_euribor_3m(&fetch::client(), &settings.urls)
        .await
        .expect("euribor-rates.eu healthy but fetch_euribor_3m parsed no rate — page layout drifted");
    assert!((-2.0..10.0).contains(&rate), "implausible 3M Euribor: {rate}%");
}

/// Walk-forward regime gate. Shells `backtest {20,12,8} universe` over the LIVE universe and asserts
/// the committed default tuning still yields a POSITIVE validated edge AND a positive top-3 held book
/// at each horizon. Same skip-vs-fail contract as the probes above: a throttle (spawn error / nonzero
/// exit / too-few-tickers) SKIPS green, per horizon; only a genuine COLLAPSE (GROWTH edge <= 0, top-3
/// excess <= 0, or BOTH out-of-sample halves negative) FAILS. It scores with the real tuning mirrored
/// into ci-settings.yaml's `buy_heuristic`.
///
/// COLLAPSE DETECTOR, NOT A SHIP GATE. Thresholds are `> 0`, never "not worse than last time": market
/// drift must never redden the build (see the job comment in ci.yml). Deciding whether a knob may move
/// is SHIP RULE v2's job, and it is graded by a human on a same-day A/B, not here.
///
/// REGIME ONLY. The other half of what this used to cover — "did an edit to the scoring code or a knob
/// change what the shipped tuning does" — is deterministic and runs offline on every `cargo test` as
/// `shipped_tuning_scores_fixture_unchanged` (src/commands/backtest.rs). What is left here is the
/// question no fixture can answer: does the edge still hold on TODAY's market.
///
/// NO LONGER `#[ignore]`. It runs whenever it can afford to and skips green when it cannot, which is
/// the contract every other net in this file uses. Two guards decide, and BOTH are bypassed by
/// `FOLIOMAN_BACKTEST_GATE` — that variable used to mean "opt in", it now means **force**:
///
/// - **Release only.** Measured on the same warm cache: 127s release, 355s debug (2.8x), and
///   `CARGO_BIN_EXE_folioman` is whichever profile the test was built in. A plain `cargo test` would
///   otherwise pay six minutes for a walk-forward the release job does in two.
/// - **Warm history cache only.** With `.long_history_cache.json` fresh the run reads its ~3900
///   monthly payloads off disk; cold, it would fetch every one of them. A fresh clone must not
///   discover that by doing it.
///
/// CI has no persisted cache, so the second guard is why the force flag exists: without it the nightly
/// `backtest-gate` job would skip too and the gate would quietly stop gating.
#[test]
fn backtest_edge_holds() {
    let forced = std::env::var("FOLIOMAN_BACKTEST_GATE").is_ok();
    if !forced && cfg!(debug_assertions) {
        eprintln!(
            "backtest-gate SKIPPED — debug build; the wide walk-forward is 127s release / 355s debug. \
             Run `cargo test --release`, or set FOLIOMAN_BACKTEST_GATE=1 to force it"
        );
        return;
    }
    // ponytail: size+mtime, not a parse. Deciding whether to skip by deserializing 88 MB of JSON costs
    // more than either wrong guess — a wrong "warm" just fetches the misses, a wrong "cold" just skips.
    if !forced {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fetch::LONG_CACHE_FILE);
        let fresh = std::fs::metadata(path).ok().and_then(|m| {
            let age = m.modified().ok()?.elapsed().ok()?;
            // 20 MB ~ 600 tickers; a populated cache is ~125 MB for ~3900. Below that the run would fetch.
            Some(age.as_secs() < fetch::LONG_CACHE_TTL_DAYS as u64 * 86_400 && m.len() > 20_000_000)
        });
        if fresh != Some(true) {
            eprintln!(
                "backtest-gate SKIPPED — {} is missing, stale (>{}d) or too small to serve ~4900 \
                 tickers. Run `folioman screen` to warm it, or set FOLIOMAN_BACKTEST_GATE=1 to fetch live",
                fetch::LONG_CACHE_FILE,
                fetch::LONG_CACHE_TTL_DAYS
            );
            return;
        }
    }
    // pull the first signed number that follows `marker` in `hay` (e.g. "edge +117.1" -> 117.1).
    fn num_after(hay: &str, marker: &str) -> Option<f64> {
        let rest = &hay[hay.find(marker)? + marker.len()..];
        let start = rest.find(|c: char| c == '+' || c == '-' || c.is_ascii_digit())?;
        let tok: String = rest[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '+' || *c == '-')
            .collect();
        tok.trim_start_matches('+').parse().ok()
    }

    /// One horizon, end to end. `true` = it was actually graded; `false` = it skipped (environmental).
    /// Every skip path returns green, per this file's skip-on-throttle contract.
    ///
    /// `forced` is the `FOLIOMAN_BACKTEST_GATE` read above, threaded in because this is a nested `fn`
    /// and cannot capture it. It separates the two skip families: an ENVIRONMENTAL skip (throttle,
    /// cold cache, spawn failure) stays green even in CI, but a STRUCTURAL skip — the run completed
    /// and a block the gate greps for simply isn't there — is a report change that silently disarms
    /// half the gate, so under the force flag (i.e. CI) it panics instead of printing a note nobody
    /// reads. A laptop still skips green on all of them.
    fn grade_horizon(years: i64, forced: bool) -> bool {
        let out = match std::process::Command::new(env!("CARGO_BIN_EXE_folioman"))
            .args(["backtest", &years.to_string(), "universe"])
            // pin the child to the committed fixture. Without it a local run scores with the gitignored
            // config/settings.yaml overlay and the gate grades a PER-MACHINE tuning — green on your knobs
            // says nothing about what ships. CI already exports this; setting it here makes the two agree.
            .env("FOLIOMAN_CONFIG", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml"))
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("backtest-gate {years}y SKIPPED — could not spawn binary: {e}");
                return false;
            }
        };
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            eprintln!("backtest-gate {years}y SKIPPED — nonzero exit {}; stderr tail: {}", out.status, err.lines().last().unwrap_or(""));
            return false; // a mid-fetch crash is environmental here; lint/unit/build jobs catch real code breakage offline
        }
        // search stdout AND stderr — robust to whichever stream the report/diagnostics land on.
        let stdout = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        // throttle guard: the wide universe fetch resolves ~3000+ tickers healthy; a tiny count means
        // Yahoo/Börse-Frankfurt throttled most requests -> the de-meaned sample is unreliable -> SKIP.
        match num_after(&stdout, "tickers:") {
            Some(n) if n >= 500.0 => {}
            Some(n) => {
                eprintln!("backtest-gate {years}y SKIPPED — only {n} tickers resolved (throttled/unavailable)");
                return false;
            }
            None => {
                eprintln!("backtest-gate {years}y SKIPPED — no ticker count in output (run didn't complete)");
                return false;
            }
        }
        // isolate the GROWTH lane's report (there's also an ON-SALE block earlier with its own edge).
        // Everything after this marker, so the `vs S&P500` held-book block below is inside it too.
        let growth = match stdout.split("── GROWTH").nth(1) {
            Some(g) => g,
            None => {
                eprintln!("backtest-gate {years}y SKIPPED — no GROWTH section (run didn't complete)");
                return false;
            }
        };
        // (#57) HEALTH GATE, before any assert. `tickers:` above counts the universe LIST, not the
        // quotes that actually RESOLVED, so a throttled run clears it with 500+ names and still gates
        // in almost nothing. Measured on a deliberately network-starved wide run: it passed the
        // `tickers:` guard, printed "only 1 windows passed this lane's gates", and `report_lane` then
        // printed no headline at all — so the `edge` grep below silently latched onto a number from a
        // LATER section and the gate graded garbage. The lane's own scored count is the honest signal.
        //
        // This skips GREEN even under `forced`, unlike the structural panics below: an outage really is
        // the environmental case this file's skip-on-throttle contract exists for. What the panics below
        // catch is the opposite — a HEALTHY run whose report shape changed.
        const MIN_SCORED: f64 = 20.0; // report_lane itself refuses under 4; a healthy wide run scores hundreds
        match num_after(growth, "windows scored:") {
            Some(n) if n >= MIN_SCORED => {}
            other => {
                eprintln!(
                    "backtest-gate {years}y SKIPPED — GROWTH lane scored {} windows (<{MIN_SCORED:.0}). The \
                     universe list resolved but hardly anything gated in: thin data, not a verdict",
                    other.map_or_else(|| "no".to_string(), |n| format!("{n:.0}"))
                );
                return false;
            }
        }
        // the top-vs-bottom-half validated edge. Anchored on the headline's own `->  edge`, not a bare
        // "edge": the word recurs in the ablation, the era table and the entry-state block, so a bare
        // marker reads whichever one happens to come first if the headline is ever missing.
        let edge = num_after(growth, "->  edge").expect("a completed GROWTH run prints its headline edge");
        assert!(
            edge > 0.0,
            "{years}y: GROWTH validated edge COLLAPSED to {edge:+.1} pts (healthy baseline ~+117 at 12y) — a \
             scoring-code change or a default-tuning edit broke the walk-forward edge; fix it before merging"
        );
        // (SHIP RULE v2) the metric the screen footer actually quotes. The lane edge above grades the
        // top-HALF against the bottom-HALF, which can stay healthy while the small book a reader buys
        // goes backwards — that gap is exactly why the rule moved. Row format:
        //   top-3  book +13.7%/yr  vs S&P500 +7.2%/yr  ->  excess +6.5 (med +6.6) pts/yr  win 97% of 39 …
        // `starts_with("top-3 ")` after trimming, so "top-30"/"top-35" can never match it.
        let t3 = growth.lines().find(|l| l.trim_start().starts_with("top-3 ")).and_then(|l| num_after(l, "excess"));
        match t3 {
            Some(x) => {
                assert!(
                    x > 0.0,
                    "{years}y: the TOP-3 held book went NEGATIVE vs the index ({x:+.1} pts/yr) — SHIP RULE v2 \
                     grades this basket and the screen footer quotes it, so this is a real collapse, not a lane \
                     statistic. Lane edge was still {edge:+.1}, which is why edge alone is not enough."
                );
                eprintln!("backtest-gate {years}y OK — GROWTH edge {edge:+.1} pts, top-3 excess {x:+.1} pts/yr");
            }
            // A run CAN complete the GROWTH lane and print no held-book block — too few gated picks
            // with a ^GSPC window. That is exactly what the health gate above now filters out, and it
            // is why this panic is safe to add and would NOT have been before it: past that gate the
            // lane scored >= MIN_SCORED windows, so the absence of the row is not thinness, it is a
            // renamed or moved row that just switched off the half of the gate SHIP RULE v2 votes on.
            // `tests/backtest_fixture.rs` pins this string offline so a rename reds there first; this
            // is the backstop for the case that pin is bypassed.
            None if forced => panic!(
                "{years}y: GROWTH completed (edge {edge:+.1}) but NO `top-3 ` held-book row parsed. Under \
                 FOLIOMAN_BACKTEST_GATE this is a report-format regression, not a thin sample: the top-3 \
                 excess assert — the metric SHIP RULE v2 grades and the screen footer quotes — was skipped"
            ),
            None => eprintln!("backtest-gate {years}y OK — GROWTH edge {edge:+.1} pts (no top-3 held-book row parsed)"),
        }
        // (#57) null model: the shipped tuning vs the SAME code with the tuning off. Assertable where
        // the raw edge is not — both arms score the same samples over the same window, so market drift
        // cancels out of the delta. A knob edit that ranks no better than no tuning at all leaves the
        // `edge > 0` check above perfectly green.
        match num_after(growth, "tuning adds") {
            Some(lift) => {
                assert!(
                    lift > 0.0,
                    "{years}y: the shipped tuning adds only {lift:+.1} pts over BuyHeuristic::default() — no \
                     tuning at all ranks as well or better. Lane edge {edge:+.1} still passes the collapse \
                     check, which is exactly why this A/B exists. Re-tune, or revert the knob change."
                );
                eprintln!("backtest-gate {years}y null-model lift {lift:+.1} pts");
            }
            None if forced => panic!(
                "{years}y: GROWTH completed but printed no `tuning adds` line — the null-model A/B was skipped"
            ),
            None => eprintln!("backtest-gate {years}y — no null-model line parsed"),
        }
        // whole out-of-sample backwards (BOTH halves negative) = the edge doesn't generalize -> collapse.
        match (num_after(growth, "early rho"), num_after(growth, "late rho")) {
            (Some(early), Some(late)) => {
                assert!(
                    !(early < 0.0 && late < 0.0),
                    "{years}y: both out-of-sample halves negative (early {early:+.2}, late {late:+.2}) — edge is in-sample only"
                );
                eprintln!("backtest-gate {years}y OOS early {early:+.2} / late {late:+.2}");
            }
            // same reasoning as the top-3 arm: the split is unconditional once >=4 windows scored, so
            // in CI a missing rho is a renamed line disarming the generalization check, not thin data.
            _ if forced => panic!(
                "{years}y: GROWTH completed but printed no `early rho`/`late rho` — the out-of-sample check was skipped"
            ),
            _ => {}
        }
        // (#29) re-probe WARNING for the shipped hard gates: each GATE SWEEP row prints the mean forward
        // peer-relative return of the cohort that gate excludes. All three shipped NEGATIVE-or-noise
        // (stretch −125.1 n=267, maxdd −16.5 n=171, lifetime +30.2 on a noise-level n=10) — a strongly
        // POSITIVE flip with a real sample means the gate is discarding winners in the current regime and
        // its threshold should be re-probed (same-batch pair). WARN only, never assert: loosening a gate
        // is a measured human decision, not a red build.
        for gate in ["growth_max_above_ma ->off", "growth_require_lifetime_uptrend ->off", "growth_maxdd_cap ->off"] {
            if let Some(line) = growth.lines().find(|l| l.contains(gate)) {
                if let (Some(n), Some(mean)) = (num_after(line, "n="), num_after(line, "peer-relative")) {
                    if n >= 30.0 && mean > 20.0 {
                        eprintln!(
                            "backtest-gate WARNING {years}y — `{gate}` excluded cohort now averages {mean:+.1} pts fwd (n={n:.0}); \
                             the gate may be discarding winners in this regime — re-probe its threshold before trusting it"
                        );
                    }
                }
            }
        }
        true
    }

    // (SHIP RULE v2, 2026-08-06) ONE horizon is no longer enough. The rule grades top-3 at 20y AND 8y
    // with 12y as the consistency read, so a gate that only ever ran 12y was guarding a lane the rule
    // does not vote on. 20 leads: hardest lane, and the horizon the screen footer quotes.
    //
    // This is NEARLY FREE, and the reason must not be lost — `monthly = long || years >= 8`
    // (commands/backtest.rs), so all three take the MONTHLY path and share ONE .long_history_cache.json.
    // The first run pays the wide fetch, the other two read it off disk (~127s each). Three horizons
    // cost about +4 min, not 3x. Do NOT "optimize" this back to a single run, and do NOT add a 5y rung:
    // below 8 the run switches to daily cadence and pays a SECOND full fetch.
    let graded = [20, 12, 8].into_iter().filter(|y| grade_horizon(*y, forced)).count();
    if graded == 0 {
        // (#57) CI reaches here only if all three horizons skipped, and under the force flag every
        // environmental skip has already been ruled out except a genuine outage. Printing a note and
        // exiting 0 is the failure this file's own header blames for network-smoke silently rotting
        // twice: a month of throttling and the nightly gate stops gating with a green check mark.
        assert!(
            !forced,
            "backtest-gate: all 3 horizons skipped under FOLIOMAN_BACKTEST_GATE — NOTHING was gated. The \
             force flag exists so CI cannot skip; reaching here means the run itself is broken or the \
             data is unusable (spawn failure, nonzero exit, <500 tickers, or a GROWTH lane too thin to \
             score). Read the per-horizon SKIPPED lines above — they say which."
        );
        eprintln!("backtest-gate SKIPPED — every horizon skipped (throttle/cold cache); NOTHING was gated this run");
    } else {
        eprintln!("backtest-gate DONE — {graded}/3 horizons graded on SHIP RULE v2 (lane edge + top-3 held book + OOS)");
    }
}
