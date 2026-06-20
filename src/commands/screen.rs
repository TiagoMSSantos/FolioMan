//! `screen [TICKERS]` — scan a LIVE universe (top-N crypto from CoinGecko + S&P 500
//! constituents, see `fetch::fetch_universe`; `screen TICKER...` overrides): all-time
//! highs/lows, instruments falling over ~1M/3M/6M/1Y, top dividend payers, and a
//! buy-candidates ranking.

use crate::commands::truncate;
use crate::core::{Quote, DIV_HORIZONS};
use crate::picks::{perf_pct, render};
use crate::{config, fetch};

/// 1Y dividend yield (%) used to rank payers; 0 if no payout / no price / short history.
/// Ranking by yield (not absolute cash) so high-% low-price names surface over big-cash
/// low-yield ones.
fn yield_1y(q: &Quote) -> f64 {
    crate::core::dividend_yields(&q.div_eur, q.price_eur)
        .first()
        .and_then(|o| *o)
        .unwrap_or(0.0)
}

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx = fetch::fx_cache();
    // live universe (CoinGecko + S&P 500), not a hand-kept list; explicit args override it
    let (universe, tech) = if args.is_empty() {
        tokio::join!(
            fetch::fetch_universe(&client, &settings.urls, settings.universe_size, settings.universe_prefer_eur),
            fetch::fetch_tech_symbols(&client, &settings.urls), // GICS sectors for the tech-only buy table
        )
    } else {
        (args, std::collections::HashSet::new()) // explicit tickers: no sector data -> no tech table
    };

    eprintln!("screen: {} tickers in universe (crypto + S&P 500 + all US-listed ETFs)", universe.len());

    // live EU HICP series to inflation-adjust long-horizon returns, only when enabled
    let eu_infl = if settings.inflation_adjust.enabled {
        eprintln!("screen: fetching EU HICP inflation series…");
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    let qs = fetch::quotes(&client, &settings.urls, &fx, &universe, settings.dip_days, settings.high_days, true, &settings.anchor_windows, eu_infl.as_ref()).await; // intraday on: picks table shows 1h/6h/12h

    let w = &settings.widths;
    let (nw, tw, pw) = (w.name, w.ticker, w.price);

    // header row naming every column (NAME = instrument, % col label varies)
    let hdr = |pct_col: &str| {
        println!(
            "  {:<nw$} {:<tw$} {:>pw$} {:>8}  {}",
            truncate("NAME", nw), truncate("TICKER", tw), "PRICE(EUR)", pct_col, "TREND"
        );
    };
    let row = |q: &Quote, pct: String| {
        println!(
            "  {:<nw$} {:<tw$} {:>pw$} {:>8}  {}",
            truncate(&q.name, nw), truncate(&q.ticker, tw), q.price, pct, q.trend
        );
    };

    // ATH/ATL: % column = 1-month change (mom_pct)
    let show = |title: &str, group: &[&Quote]| {
        println!("\n{} ({}):", title, group.len());
        hdr("1M %");
        for q in group {
            row(q, q.mom_pct.map_or("n/a".to_string(), |m| format!("{:+.1}%", m)));
        }
        if group.is_empty() {
            println!("  (none)");
        }
    };

    println!("Scanned {} instruments.", qs.len());
    show("All-time highs", &qs.iter().filter(|q| q.at_ath).collect::<Vec<_>>());
    show("All-time lows", &qs.iter().filter(|q| q.at_atl).collect::<Vec<_>>());

    // single fallers table (was 4 per-horizon tables): in if down over ANY of 1M/3M/6M/1Y
    // (union, so nothing the old tables showed is lost); columns 1D..1Y, biggest 1M drop first.
    let fall_cols = ["1D", "1W", "1M", "3M", "6M", "1Y"];
    let mut fallers: Vec<&Quote> = qs
        .iter()
        .filter(|q| ["1M", "3M", "6M", "1Y"].iter().any(|l| perf_pct(q, l).map_or(false, |p| p < 0.0)))
        .collect();
    fallers.sort_by(|a, b| {
        perf_pct(a, "1M").unwrap_or(0.0).partial_cmp(&perf_pct(b, "1M").unwrap_or(0.0)).unwrap()
    });
    let fall_hdr = fall_cols.iter().map(|l| format!("{l:>8}")).collect::<Vec<_>>().join(" ");
    println!("\nFalling (down over 1M/3M/6M/1Y), biggest 1-month drop first ({}):", fallers.len());
    println!(
        "  {:<nw$} {:<tw$} {:>pw$} {fall_hdr}  {}",
        truncate("NAME", nw), truncate("TICKER", tw), "PRICE(EUR)", "TREND"
    );
    if fallers.is_empty() {
        println!("  (none)");
    }
    for q in &fallers {
        let cells = fall_cols
            .iter()
            .map(|l| format!("{:>8}", perf_pct(q, l).map_or("n/a".to_string(), |v| format!("{:+.1}%", v))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  {:<nw$} {:<tw$} {:>pw$} {cells}  {}",
            truncate(&q.name, nw), truncate(&q.ticker, tw), q.price, q.trend
        );
    }

    // top dividend payers: total per share (EUR) + yield per window, highest 1Y yield first
    let mut payers: Vec<&Quote> = qs.iter().filter(|q| yield_1y(q) > 0.0).collect();
    payers.sort_by(|a, b| yield_1y(b).partial_cmp(&yield_1y(a)).unwrap());
    let div_hdr = DIV_HORIZONS
        .iter()
        .map(|(l, _)| format!("{:>17}", l))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "\nTop dividend payers — total per share EUR + avg annual yield % over last \
         1Y/5Y/10Y/20Y, highest 1Y yield first ({}):",
        payers.len()
    );
    println!("  {:<nw$} {:<tw$} {div_hdr}", truncate("NAME", nw), truncate("TICKER", tw));
    if payers.is_empty() {
        println!("  (none paid dividends)");
    }
    for q in payers.iter().take(25) {
        let yields = crate::core::dividend_yields(&q.div_eur, q.price_eur);
        let cells = DIV_HORIZONS
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let cell = match q.div_eur.get(i).and_then(|o| *o) {
                    Some(v) => {
                        let y = yields.get(i).and_then(|o| *o)
                            .map_or("n/a".to_string(), |p| format!("{:.1}%", p));
                        format!("€{} ({})", crate::core::fmt_money2(v), y)
                    }
                    None => "n/a".to_string(),
                };
                format!("{cell:>17}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        println!("  {:<nw$} {:<tw$} {cells}", truncate(&q.name, nw), truncate(&q.ticker, tw));
    }

    // Bitcoin NUPL: whole-market crypto sentiment gauge. Fetched BEFORE render so it can damp the
    // crypto rows of both lanes (high NUPL = euphoric top), then also printed as the footer line.
    let nupl = fetch::fetch_nupl(&client, &settings.urls).await;

    // buy candidates among the SCANNED universe (not settings.tickers) — same heuristic as `check`
    render(&qs, 20, &settings.buy_heuristic, w, &tech, nupl);

    if let Some(n) = nupl {
        println!(
            "\nBitcoin NUPL: {n:.3} ({}) — net unrealized profit/loss, whole-market sentiment (damps the crypto tables above). NOT advice.",
            crate::core::nupl_zone(n)
        );
    }
}
