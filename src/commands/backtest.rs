//! `backtest [YEARS] [TICKERS...]` — zero-EXTRA-fetch sanity check of the buy heuristic. For each
//! ticker it fetches the 10y history ONCE (the same single chart call the live path makes — no extra
//! per-ticker requests, no worse rate-limit pressure than `check`), scores it AS OF ~YEARS ago on the
//! truncated series, then measures the realized return from that day to today. Reports the rank
//! correlation between score and realized return: does a higher score actually predict a better hold?
//!
//! In-sample and price-only (no as-of dividends/P/E reconstructed), so a DIRECTIONAL gut-check that
//! tells you which way the heuristic leans — NOT a forecast and NOT out-of-sample proof. Defaults to
//! the settings.yaml watchlist (small) so it stays cheap; pass tickers to test others.

use crate::picks::buy_score;
use crate::{config, core, fetch};
use futures::stream::{self, StreamExt};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();

    // first purely-numeric arg = holdout years; everything else = tickers to test
    let mut years: i64 = 5;
    let mut tickers: Vec<String> = Vec::new();
    for a in &args {
        match a.parse::<i64>() {
            Ok(y) if tickers.is_empty() && y > 0 => years = y,
            _ => tickers.push(a.clone()),
        }
    }
    if tickers.is_empty() {
        tickers = settings.tickers.clone();
    }
    let t = &settings.buy_heuristic;
    eprintln!("backtest: {} tickers, scoring as of ~{years}y ago vs realized return since…", tickers.len());

    // (ticker, score-as-of, realized-%-since). One chart fetch each, concurrency-bounded like screen.
    let rows: Vec<(String, f64, f64)> = stream::iter(tickers.iter())
        .map(|tk| {
            let client = &client;
            let urls = &settings.urls;
            async move {
                let (dates, closes) = fetch::fetch_history(client, urls, tk).await?;
                // index ~years ago; needs ≥~3y of history BEFORE it (long legs) and a real holdout after
                let cutoff = *dates.last()? - chrono::Duration::days(years * 365);
                let t_idx = dates.iter().rposition(|d| *d <= cutoff)?;
                if t_idx < 750 {
                    return None; // <~3y before the as-of date -> can't form/score the long trend fairly
                }
                let q = core::backtest_quote(tk, &dates, &closes, t_idx);
                let score = buy_score(&q, t)?; // None = the gates excluded it back then -> not a pick
                let realized = (*closes.last()? / closes[t_idx] - 1.0) * 100.0;
                Some((tk.clone(), score, realized))
            }
        })
        .buffer_unordered(fetch::FETCH_CONCURRENCY)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    if rows.len() < 2 {
        println!("backtest: only {} name(s) had {years}y+ history and passed the gates — too few to correlate.", rows.len());
        return;
    }

    let scores: Vec<f64> = rows.iter().map(|(_, s, _)| *s).collect();
    let rets: Vec<f64> = rows.iter().map(|(_, _, r)| *r).collect();
    let rho = core::spearman(&scores, &rets);

    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // best score first
    println!("\nBacktest — buy score as of ~{years}y ago vs realized return since ({} names):", rows.len());
    println!("  {:<12} {:>8} {:>14}", "TICKER", "SCORE", "REALIZED %");
    for (tk, s, r) in &sorted {
        println!("  {:<12} {:>8.1} {:>13.1}%", tk, s, r);
    }

    // practical read: did the top-scored half actually beat the bottom half on realized return?
    let half = sorted.len() / 2;
    let mean = |s: &[(String, f64, f64)]| s.iter().map(|(_, _, r)| r).sum::<f64>() / s.len().max(1) as f64;
    let (top, bot) = (mean(&sorted[..half]), mean(&sorted[sorted.len() - half..]));

    match rho {
        Some(v) => println!(
            "\nSpearman rank corr (score vs realized): {v:+.2}  [+1 = score ranks winners perfectly, 0 = no signal, − = backwards]"
        ),
        None => println!("\nSpearman rank corr: n/a"),
    }
    println!("Top-half avg realized {top:+.1}%  vs  bottom-half {bot:+.1}%  ->  edge {:+.1} pts", top - bot);
    println!("In-sample, price-only (no as-of dividends/PE). Directional gut-check, NOT a forecast.");
}
