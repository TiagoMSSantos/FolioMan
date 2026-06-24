//! LIVE smoke tests — the only thing that catches a real API changing shape under the parsers
//! (Yahoo chart schema, FX, the full Quote build). They hit the network, so they're flaky and slow
//! and must NEVER gate CI or `cargo test`. DOUBLE-gated: `#[ignore]` (skipped by default) AND an
//! env guard, so even `cargo test -- --ignored` is a no-op unless you opt in:
//!
//!     FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored
//!
//! READ-ONLY endpoints only — never a broker/order call.

use folioman::{config, fetch};

/// True only when explicitly opted in. Lets the tests early-return as a no-op otherwise (belt-and-
/// suspenders with `#[ignore]`).
fn opted_in() -> bool {
    std::env::var("FOLIOMAN_NET_TESTS").is_ok()
}

#[tokio::test]
#[ignore = "live network; run with FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored"]
async fn yahoo_history_parses() {
    if !opted_in() {
        return;
    }
    let settings = config::load();
    let client = fetch::client();
    // equity + crypto: the core 10y daily chart parse path against today's Yahoo shape.
    for ticker in ["AAPL", "BTC-USD"] {
        let (dates, closes) = fetch::fetch_history(&client, &settings.urls, ticker)
            .await
            .unwrap_or_else(|| panic!("{ticker}: fetch_history returned None — Yahoo shape changed?"));
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
    let settings = config::load();
    let rate = fetch::eur_rate(&fetch::client(), &settings.urls, "USD", &fetch::fx_cache()).await;
    let r = rate.expect("USD->EUR rate did not resolve");
    assert!(r.is_finite() && r > 0.0, "implausible FX rate: {r}");
}

#[tokio::test]
#[ignore = "live network; run with FOLIOMAN_NET_TESTS=1 cargo test --test network -- --ignored"]
async fn full_quote_build() {
    if !opted_in() {
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
