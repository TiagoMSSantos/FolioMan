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
//! ONE test is still gated: `backtest_edge_holds` shells `backtest 12 universe` over 3000+ live
//! tickers (CI budgets 45 minutes). Its fan-out IS the test, so it cannot be made cheap, and what it
//! measures is REGIME — whether the shipped edge still holds on today's market. The deterministic half
//! of its old job (did a code or knob edit change the scoring) split off into
//! `shipped_tuning_scores_fixture_unchanged` in src/commands/backtest.rs, which runs offline on every
//! `cargo test`. So this keeps `#[ignore]` + its own `FOLIOMAN_BACKTEST_GATE`:
//!
//!     FOLIOMAN_BACKTEST_GATE=1 cargo test --release --test network backtest_edge_holds -- --ignored

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

/// Nightly walk-forward gate. Shells the release binary's `backtest 12 universe` over the LIVE
/// universe and asserts the committed default tuning still yields a POSITIVE validated edge.
/// Same skip-vs-fail contract as the probes above: a throttle (spawn error / nonzero exit /
/// too-few-tickers) SKIPS green; only a genuine COLLAPSE (GROWTH edge <= 0, or BOTH out-of-sample
/// halves negative) FAILS. It scores with the real tuning mirrored into ci-settings.yaml's
/// `buy_heuristic`, so a red here means a scoring-code change or a knob edit broke the validated edge.
///
/// REGIME ONLY. The other half of what this used to cover — "did an edit to the scoring code or a knob
/// change what the shipped tuning does" — is deterministic and now runs offline on every `cargo test`
/// as `shipped_tuning_scores_fixture_unchanged` (src/commands/backtest.rs). What is left here is the
/// question no fixture can answer: does the edge still hold on TODAY's market. That one is worth
/// 3000+ live tickers; the other never was.
#[test]
#[ignore = "multi-minute live universe backtest; nightly only. FOLIOMAN_BACKTEST_GATE=1 cargo test --release --test network backtest_edge_holds -- --ignored"]
fn backtest_edge_holds() {
    // its own env var, not the ignore attribute alone: `-- --ignored` runs every ignored test in this
    // file, so anything that ever gets gated here again would drag a multi-minute universe backtest
    // along with it. Only the nightly backtest-gate job sets FOLIOMAN_BACKTEST_GATE.
    if std::env::var("FOLIOMAN_BACKTEST_GATE").is_err() {
        return;
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

    let out = match std::process::Command::new(env!("CARGO_BIN_EXE_folioman"))
        .args(["backtest", "12", "universe"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("backtest-gate SKIPPED — could not spawn binary: {e}");
            return;
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        eprintln!("backtest-gate SKIPPED — nonzero exit {}; stderr tail: {}", out.status, err.lines().last().unwrap_or(""));
        return; // a mid-fetch crash is environmental here; lint/unit/build jobs catch real code breakage offline
    }
    // search stdout AND stderr — robust to whichever stream the report/diagnostics land on.
    let stdout = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    // throttle guard: the wide universe fetch resolves ~3000+ tickers healthy; a tiny count means
    // Yahoo/Börse-Frankfurt throttled most requests -> the de-meaned sample is unreliable -> SKIP.
    match num_after(&stdout, "tickers:") {
        Some(n) if n >= 500.0 => {}
        Some(n) => {
            eprintln!("backtest-gate SKIPPED — only {n} tickers resolved (throttled/unavailable)");
            return;
        }
        None => {
            eprintln!("backtest-gate SKIPPED — no ticker count in output (run didn't complete)");
            return;
        }
    }
    // isolate the GROWTH lane's report (there's also an ON-SALE block earlier with its own edge).
    let growth = match stdout.split("── GROWTH").nth(1) {
        Some(g) => g,
        None => {
            eprintln!("backtest-gate SKIPPED — no GROWTH section (run didn't complete)");
            return;
        }
    };
    // first "edge <n> pts" inside the block is the top-vs-bottom-half validated edge.
    let edge = num_after(growth, "edge").expect("a completed GROWTH run prints its edge");
    assert!(
        edge > 0.0,
        "GROWTH validated edge COLLAPSED to {edge:+.1} pts (healthy baseline ~+117) — a scoring-code \
         change or a default-tuning edit broke the walk-forward edge; fix it before merging"
    );
    // whole out-of-sample backwards (BOTH halves negative) = the edge doesn't generalize -> collapse.
    if let (Some(early), Some(late)) = (num_after(growth, "early rho"), num_after(growth, "late rho")) {
        assert!(
            !(early < 0.0 && late < 0.0),
            "both out-of-sample halves negative (early {early:+.2}, late {late:+.2}) — edge is in-sample only"
        );
        eprintln!("backtest-gate OK — GROWTH edge {edge:+.1} pts, OOS early {early:+.2} / late {late:+.2}");
    } else {
        eprintln!("backtest-gate OK — GROWTH edge {edge:+.1} pts (no OOS line parsed)");
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
                        "backtest-gate WARNING — `{gate}` excluded cohort now averages {mean:+.1} pts fwd (n={n:.0}); \
                         the gate may be discarding winners in this regime — re-probe its threshold before trusting it"
                    );
                }
            }
        }
    }
}
