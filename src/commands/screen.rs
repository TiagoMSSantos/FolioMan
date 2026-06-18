//! `screen [TICKERS]` — scan `settings.universe` (broader than tickers): all-time
//! highs/lows, instruments falling over ~1M/3M/6M/1Y, and the top dividend payers.

use crate::commands::truncate;
use crate::core::{Quote, DIV_HORIZONS};
use crate::picks::perf_pct;
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
    let universe = if args.is_empty() { settings.universe.clone() } else { args }; // not bounded by tickers

    let qs = fetch::quotes(&client, &settings.urls, &fx, &universe, settings.dip_days, settings.high_days).await;

    let w = &settings.widths;
    let (nw, tw) = (w.name, w.ticker);

    // header row naming every column (NAME = instrument, % col label varies)
    let hdr = |pct_col: &str| {
        println!(
            "  {:<nw$} {:<tw$} {:>13} {:>8}  {}",
            truncate("NAME", nw), truncate("TICKER", tw), "PRICE(EUR)", pct_col, "TREND"
        );
    };
    let row = |q: &Quote, pct: String| {
        println!(
            "  {:<nw$} {:<tw$} {:>13} {:>8}  {}",
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

    // instruments down over `label`, biggest drop first; % column = that horizon
    let falling = |title: &str, label: &str| {
        let mut g: Vec<&Quote> =
            qs.iter().filter(|q| perf_pct(q, label).map_or(false, |p| p < 0.0)).collect();
        g.sort_by(|a, b| perf_pct(a, label).partial_cmp(&perf_pct(b, label)).unwrap());
        println!("\n{} ({}):", title, g.len());
        hdr(&format!("{label} %"));
        for q in &g {
            row(q, perf_pct(q, label).map_or("n/a".to_string(), |v| format!("{:+.1}%", v)));
        }
        if g.is_empty() {
            println!("  (none)");
        }
    };

    println!("Scanned {} instruments.", qs.len());
    show("All-time highs", &qs.iter().filter(|q| q.at_ath).collect::<Vec<_>>());
    show("All-time lows", &qs.iter().filter(|q| q.at_atl).collect::<Vec<_>>());
    falling("Falling over ~1 month", "1M");
    falling("Falling over ~3 months", "3M");
    falling("Falling over ~6 months", "6M");
    falling("Falling over ~1 year", "1Y");

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
}
