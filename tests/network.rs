//! LIVE smoke tests — the only thing that catches a real API changing shape under the parsers
//! (Yahoo chart schema, FX, the full Quote build). They hit the network, so they're DOUBLE-gated:
//! `#[ignore]` (skipped by `cargo test` / the blocking CI jobs) AND an env guard. Run on demand:
//!
//!     FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored
//!
//! Crucially they tell a transient hiccup apart from real drift, so a red here MEANS something:
//!   - transport error / HTTP 429 / 5xx / 401 / 403 / Yahoo's own error envelope  -> SKIP (environmental)
//!   - HTTP 200 but the fields the parser needs are gone                          -> FAIL (API drift)
//! That's why CI doesn't swallow the failure: a rate-limit just skips (green), only genuine drift goes red.
//! READ-ONLY endpoints only — never a broker/order call.

// ponytail: separate test crate, so the lib's crate-root allow doesn't reach here. Same call: docs render fine.
#![allow(clippy::doc_lazy_continuation)]

use folioman::{config, fetch};
use serde_json::Value;

fn opted_in() -> bool {
    std::env::var("FOLIOMAN_NET_TESTS").is_ok()
}

enum Probe {
    Healthy,      // 200 + the chart contract is present -> safe to run the real parser assertions
    Skip(String), // environmental (throttle/transport/bad-symbol envelope) -> no-op, don't fail
    Drift(String), // 200 OK but the shape parse_chart depends on is gone -> a real regression
}

/// Status-aware probe of the live Yahoo chart endpoint. Separates "the network/API is having a moment"
/// (skip) from "the contract the parser relies on changed" (fail) — the distinction the CI shell can't make.
async fn probe_yahoo(ticker: &str) -> Probe {
    let urls = config::load().urls;
    let url = urls.yahoo_chart.replace("{ticker}", ticker).replace("{range}", "1y");
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
        Probe::Healthy
    } else if !body.pointer("/chart/error").is_none_or(|e| e.is_null()) {
        Probe::Skip(format!("Yahoo error envelope: {}", body.pointer("/chart/error").unwrap()))
    } else {
        Probe::Drift("HTTP 200 but chart.result[0].timestamp/close missing and no error envelope".into())
    }
}

/// Resolve a probe: returns false (caller should `return`) when skipped, true when healthy, panics on drift.
fn healthy_or_skip(p: Probe, ctx: &str) -> bool {
    match p {
        Probe::Healthy => true,
        Probe::Skip(why) => {
            eprintln!("network smoke [{ctx}] SKIPPED — {why}");
            false
        }
        Probe::Drift(why) => panic!("API DRIFT [{ctx}]: {why}"),
    }
}

#[tokio::test]
#[ignore = "live network; run with FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored"]
async fn yahoo_history_parses() {
    if !opted_in() {
        return;
    }
    let settings = config::load();
    let client = fetch::client();
    for ticker in ["AAPL", "BTC-USD"] {
        if !healthy_or_skip(probe_yahoo(ticker).await, ticker) {
            continue; // endpoint throttled for this symbol — don't fail the suite
        }
        // endpoint is healthy (200 + contract present) -> a None now is a REAL parser break, not flakiness
        let (dates, closes) = fetch::fetch_history(&client, &settings.urls, ticker)
            .await
            .unwrap_or_else(|| panic!("{ticker}: 200 OK but fetch_history/parse_chart returned None — drift"));
        assert!(!closes.is_empty(), "{ticker}: no closes");
        assert_eq!(dates.len(), closes.len(), "{ticker}: dates/closes length mismatch");
        assert!(dates.windows(2).all(|w| w[0] <= w[1]), "{ticker}: dates not ascending");
        assert!(closes.iter().all(|c| c.is_finite() && *c > 0.0), "{ticker}: bad close value");
    }
}

#[tokio::test]
#[ignore = "live network; run with FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored"]
async fn fx_rate_resolves() {
    if !opted_in() {
        return;
    }
    // FX resolves via the same Yahoo chart endpoint (USDEUR=X) -> gate on its health first.
    if !healthy_or_skip(probe_yahoo("USDEUR=X").await, "FX USDEUR=X") {
        return;
    }
    let settings = config::load();
    let rate = fetch::eur_rate(&fetch::client(), &settings.urls, "USD", &fetch::fx_cache()).await;
    let r = rate.expect("Yahoo healthy but USD->EUR rate did not resolve — drift");
    assert!(r.is_finite() && r > 0.0, "implausible FX rate: {r}");
}

#[tokio::test]
#[ignore = "live network; run with FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored"]
async fn full_quote_build() {
    if !opted_in() {
        return;
    }
    if !healthy_or_skip(probe_yahoo("AAPL").await, "full_quote_build") {
        return;
    }
    let settings = config::load();
    let tickers = vec!["AAPL".to_string(), "BTC-USD".to_string()];
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
