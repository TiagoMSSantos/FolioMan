//! One file per subcommand — each owns its arg logic and output, nothing shared but the
//! tiny formatting helpers below. `main.rs` only dispatches.

pub mod accounts;
pub mod alert;
pub mod backtest;
pub mod check;
pub mod perf;
pub mod report;
pub mod screen;
pub mod size;
pub mod trade;

/// First `n` chars (Python str slicing is by char, like Rust here).
pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Signed-less percent or "n/a" (footer cells).
pub fn pct(x: Option<f64>) -> String {
    match x {
        None => "n/a".to_string(),
        Some(v) => format!("{:.1}%", v),
    }
}

/// The macro backdrop you compare the asset tables against: live Euribor 3M, the Certificados de
/// Aforro fixed-income ladder, and inflation. Shared by `check` and `screen` (printed at the end).
/// NO config fallbacks — a failed fetch prints an explicit error line and skips that table, never a
/// silently-stale number.
pub async fn print_macro_footer(client: &reqwest::Client, urls: &crate::config::Urls) {
    use crate::{core, fetch};
    let (euribor, inflations) = tokio::join!(
        fetch::fetch_euribor_3m(client, urls),
        fetch::inflation_all(client, urls),
    );
    match euribor {
        None => eprintln!(
            "\n⚠ ERROR: Euribor 3M live fetch failed — no fallback; Certificados de Aforro table skipped"
        ),
        Some(euribor) => {
            println!("\nEuribor 3M: {euribor:.3}%  (live)");
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
                let gain =
                    |y| format!("{:+.1}%", core::ca_cumulative_gain(base, s.premium_early, s.premium_late, y));
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
        }
    }

    println!("\nInflation — latest annual % + cumulative price rise (compounded) over last N years:");
    println!("  {:<9} {:>9} {:>9} {:>9} {:>9}", "", "latest", "5Y", "10Y", "20Y");
    for (label, series) in &inflations {
        let (ly, lv, _, _) = core::inflation_summary(series);
        let cum = |y| pct(core::inflation_compounded(series, y));
        let note = if series.is_empty() {
            "  (⚠ ERROR — live fetch failed, no data)".to_string()
        } else {
            match ly {
                Some(y) => format!("  (latest {y})"),
                None => String::new(),
            }
        };
        println!("  {:<9} {:>9} {:>9} {:>9} {:>9}{note}", label, pct(lv), cum(5), cum(10), cum(20));
    }
}
