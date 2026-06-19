//! `check [TICKERS]` — price(EUR) + horizon % + market + trend + headline, then the
//! buy-candidate ranking and the Euribor / Certificados / inflation footer.

use crate::commands::{pct, truncate};
use crate::core::HORIZONS;
use crate::{config, core, fetch, picks};

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let hdr = HORIZONS
        .iter()
        .map(|(l, _)| format!("{:>8}", l))
        .collect::<Vec<_>>()
        .join(" ");
    let w = &settings.widths;
    let (nw, tw, mw, pw) = (w.name, w.ticker, w.market, w.price);
    println!(
        "{:<nw$} {:<tw$} {:>pw$} {hdr}  {:<mw$} {:<8} HEADLINE",
        truncate("NAME", nw), truncate("TICKER", tw), "PRICE(EUR)", truncate("MARKET", mw), "TREND"
    );

    // live EU HICP series for inflation-adjusting the long-horizon returns (only when enabled).
    // ponytail: this refetches EU HICP that the footer also pulls below; one extra call, only when on.
    let eu_infl = if settings.inflation_adjust.enabled {
        Some(fetch::fetch_eu_inflation(&client, &settings.urls).await)
    } else {
        None
    };
    let qs = fetch::quotes(&client, &settings.urls, &fx, &tickers, settings.dip_days, settings.high_days, false, &settings.anchor_windows, eu_infl.as_ref()).await;
    for q in &qs {
        let cells = HORIZONS
            .iter()
            .enumerate()
            .map(|(i, _)| format!("{:>8}", core::pct_cell(q.perf.get(i).and_then(|o| o.as_ref()))))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:<nw$} {:<tw$} {:>pw$} {cells}  {:<mw$} {:<8} {}",
            truncate(&q.name, nw),
            truncate(&q.ticker, tw),
            q.price,
            truncate(&q.market, mw),
            q.trend,
            truncate(&q.head, w.headline),
        );
    }

    // best buy candidates (heuristic, derived from the table above — no extra fetch).
    // Empty tech set: the watchlist carries no GICS sector data, so no tech-only table here.
    picks::render(&qs, settings.top_picks, &settings.buy_heuristic, w, &std::collections::HashSet::new());

    // one concurrent batch: Euribor + the 3 inflation fetches
    let (euribor_res, inflations) = tokio::join!(
        fetch::fetch_euribor_3m(&client, &settings.urls, settings.euribor_3m),
        fetch::inflation_all(&client, &settings.urls),
    );
    let (euribor, live) = euribor_res;
    let tag = if live {
        "live".to_string()
    } else {
        format!("⚠ FALLBACK — config value from {}", settings.euribor_3m_date)
    };
    println!("\nEuribor 3M: {:.3}%  ({tag})", euribor);
    println!(
        "Certificados de Aforro — base = min(Euribor + spread, cap), floored 0; \
         premium added per holding year. Gains compound today's base (Euribor drifts):"
    );
    println!(
        "  {:<6} {:>6} {:>6} {:>14} {:>8} {:>8} {:>8} {:>8}",
        "SÉRIE", "BASE", "CAP", "PREMIUM 2-5/6+", "1Y", "5Y", "10Y", "20Y"
    );
    for s in core::CA_SERIES {
        let base = core::ca_base_rate(euribor, s.spread, s.cap);
        let gain = |y| format!("{:+.1}%", core::ca_cumulative_gain(base, s.premium_early, s.premium_late, y));
        println!(
            "  {:<6} {:>5.2}% {:>5.1}% {:>13} {:>8} {:>8} {:>8} {:>8}",
            s.name,
            base,
            s.cap,
            format!("+{:.2}/+{:.2}%", s.premium_early, s.premium_late),
            gain(1),
            gain(5),
            gain(10),
            gain(20),
        );
    }

    println!("\nInflation — latest annual % + cumulative price rise (compounded) over last N years:");
    println!("  {:<9} {:>9} {:>9} {:>9} {:>9}", "", "latest", "5Y", "10Y", "20Y");
    for (label, series) in &inflations {
        let (ly, lv, _, _) = core::inflation_summary(series);
        let cum = |y| pct(core::inflation_compounded(series, y));
        let note = if series.is_empty() {
            "  (⚠ FALLBACK — live fetch failed, no data)".to_string()
        } else {
            match ly {
                Some(y) => format!("  (latest {y})"),
                None => String::new(),
            }
        };
        println!("  {:<9} {:>9} {:>9} {:>9} {:>9}{note}", label, pct(lv), cum(5), cum(10), cum(20));
    }
}
