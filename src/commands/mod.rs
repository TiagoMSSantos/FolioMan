//! One file per subcommand — each owns its arg logic and output, nothing shared but the
//! tiny formatting helpers below. `main.rs` only dispatches.

pub mod accounts;
pub mod alert;
pub mod backtest;
pub mod check;
pub mod perf;
pub mod report;
pub mod screen;
pub mod sim;
pub mod size;
pub mod track;
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

/// `--explain [TICKER]` parse, shared by `screen` and `check`: returns the explain target and the
/// remaining positional tickers. A bare `--explain` = None (the always-on #1 footer covers it).
/// What each command DOES with the target differs on purpose: `screen` adds it to the scan list
/// (narrowing a full-universe run to it is the point), `check` keeps the whole watchlist view and
/// only ensures the target gets fetched. Any other flag is fatal: positional args OVERRIDE the
/// universe/watchlist, so a typo'd flag must not silently shrink the run to a tiny ticker list.
/// Tickers never START with '-' (BTC-USD has it inside), so this can't reject a real symbol.
pub(crate) fn parse_explain(cmd: &str, args: Vec<String>) -> (Option<String>, Vec<String>) {
    let mut explain: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        if a == "--explain" {
            if it.peek().is_some_and(|t| !t.starts_with('-')) {
                explain = Some(it.next().unwrap());
            }
        } else if a.starts_with('-') {
            eprintln!("{cmd}: unknown flag {a} (only --explain [TICKER] is supported)");
            std::process::exit(2);
        } else {
            positional.push(a);
        }
    }
    (explain, positional)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (round 61) truncate counts CHARS not bytes (fund names carry €/é/ü — a byte slice would
    /// panic mid-codepoint); pct rounds to 1dp and never signs.
    #[test]
    fn shared_formatter_semantics() {
        assert_eq!(truncate("Amundi Core €STR", 13), "Amundi Core €");
        assert_eq!(truncate("ab", 5), "ab"); // shorter than n: unchanged
        assert_eq!(truncate("", 3), "");
        assert_eq!(pct(Some(2.34)), "2.3%");
        assert_eq!(pct(Some(-1.26)), "-1.3%"); // negative keeps its own sign, no forced +
        assert_eq!(pct(None), "n/a");
    }

    /// parse_explain: flag+ticker extracted from any position, bare flag = None (footer covers #1),
    /// positional tickers pass through in order. (The unknown-flag arm exits the process — not
    /// unit-testable, guarded by inspection.)
    #[test]
    fn parse_explain_semantics() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_explain("check", v(&[])), (None, vec![]));
        assert_eq!(parse_explain("check", v(&["AAPL", "MSFT"])), (None, v(&["AAPL", "MSFT"])));
        assert_eq!(parse_explain("check", v(&["--explain", "KEYS"])), (Some("KEYS".into()), vec![]));
        assert_eq!(
            parse_explain("check", v(&["AAPL", "--explain", "KEYS", "BTC-USD"])),
            (Some("KEYS".into()), v(&["AAPL", "BTC-USD"]))
        );
        assert_eq!(parse_explain("check", v(&["--explain"])), (None, vec![])); // bare flag
    }

    /// (#110) The rate `print_macro_footer` RETURNS must be the rate it fetched, and a failed fetch
    /// must be `None` rather than a number.
    ///
    /// This function is otherwise a `println!` sink, and the killing suite (`--lib --test
    /// backtest_fixture`) can read neither its stdout nor its one real consumer — the level-entry
    /// line in `screen::run`. The mutation gate found the hole the moment (#110) put the signature in
    /// a diff: all four of `None` / `Some(0.0)` / `Some(1.0)` / `Some(-1.0)` survived, because
    /// nothing anywhere checked the value. Serving a known rate off a real socket is what separates
    /// "fetched it" from "returned a constant", and one exact-value assert kills all four.
    ///
    /// THE FAILURE ARM IS THE LOAD-BEARING ONE, and it is the reason this is not just a gate
    /// formality. `screen` subtracts this rate inside `picks`-adjacent `valuation_state`
    /// (`100/pe − euribor`), so a fabricated `Some(0.0)` reads as a zero risk-free and prints CHEAP
    /// for every market. It cannot be caught downstream either: a 0.00% Euribor was REAL for most of
    /// 2015-2022, so `valuation_state` has to accept 0.0 as an honest number. "No fetch" and "zero"
    /// must therefore stay distinguishable HERE, which is exactly what the doc comment on
    /// `fetch_euribor_3m` promises in words and nothing had asserted.
    ///
    /// Offline by construction: `stub_urls` fills every URL field with the loopback base, so no
    /// endpoint in the footer can reach a live host. The inflation half gets the same body, fails to
    /// parse it as JSON and prints its own ERROR rows — noise in the captured output, and not what
    /// this asserts.
    #[tokio::test]
    async fn the_macro_footer_hands_back_the_rate_it_fetched_and_never_invents_one() {
        // the euribor scrape is a regex over HTML, so the smallest valid page is the number itself
        let (base, client) = crate::fetch::tests::stub_server("3M Euribor 2.345 %");
        let urls = crate::fetch::tests::stub_urls(&base);
        assert_eq!(
            print_macro_footer(&client, &urls).await,
            Some(2.345),
            "the returned rate must be the SERVED one — a constant cannot match a value it never saw"
        );
        // port 1 refuses instantly: a failed fetch, with no live host involved
        let dead = crate::fetch::tests::stub_urls("http://127.0.0.1:1/");
        assert_eq!(
            print_macro_footer(&client, &dead).await,
            None,
            "no fetch is None — a zero here would print CHEAP for every market, and 0.00% is a rate that really happened"
        );
    }
}

/// The macro backdrop you compare the asset tables against: live Euribor 3M, the Certificados de
/// Aforro fixed-income ladder, and inflation. Shared by `check` and `screen` (printed at the end).
/// NO config fallbacks — a failed fetch prints an explicit error line and skips that table, never a
/// silently-stale number.
///
/// (#110) Returns the Euribor 3M it fetched, so `screen`'s level entry state can subtract the SAME
/// risk-free rate this footer printed rather than fetching a second one that could differ from it.
pub async fn print_macro_footer(client: &reqwest::Client, urls: &crate::config::Urls) -> Option<f64> {
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
                "Certificados de Aforro — base = clamp(Euribor × mult + spread, 0, cap); premium \
                 added per holding year. Gains compound today's base (Euribor drifts) and run PAST \
                 prazo, so any cell beyond it is terms, not money you collect:"
            );
            println!(
                "  {:<6} {:>6} {:>6} {:>6} {:>14} {:>8} {:>8} {:>8} {:>8}",
                "SÉRIE", "BASE", "CAP", "PRAZO", "PREMIUM", "2Y", "5Y", "8Y", "20Y"
            );
            for s in core::CA_SERIES {
                let base = core::ca_base_rate(euribor, s.mult, s.spread, s.cap);
                // no published formula -> no gains. `—` here means "IGCP does not say", NOT zero.
                let gain = |y| {
                    base.map_or_else(
                        || "—".to_string(),
                        |b| format!("{:+.1}%", core::ca_cumulative_gain(b, s.premium, y)),
                    )
                };
                println!(
                    "  {:<6} {:>6} {:>6} {:>6} {:>14} {:>8} {:>8} {:>8} {:>8}  {}",
                    s.name,
                    base.map_or_else(|| "n/a".to_string(), |b| format!("{b:.2}%")),
                    s.cap.map_or_else(|| "—".to_string(), |c| format!("{c:.1}%")),
                    s.prazo_years.map_or_else(|| "—".to_string(), |p| format!("{p}y")),
                    core::ca_premium_range(s.premium),
                    gain(2),
                    gain(5),
                    gain(8),
                    gain(20),
                    s.note,
                );
            }
        }
    }

    println!("\nInflation — latest annual % + cumulative price rise (compounded) over last N years:");
    println!("  {:<9} {:>9} {:>9} {:>9} {:>9} {:>9}", "", "latest", "2Y", "5Y", "8Y", "20Y");
    for (label, series) in &inflations {
        let (ly, lv, _, _) = core::inflation_summary(series);
        let cum = |y| pct(core::inflation_compounded(series, y));
        let note = if series.is_empty() {
            "  (⚠ ERROR — live fetch failed, no data)".to_string()
        } else if let Some(y) = core::infl_series_stale(series, chrono::Local::now().date_naive()) {
            // frozen-not-empty feed (e.g. a terminated Eurostat dataset) — the year alone is
            // easy to miss, so say it outright
            format!("  (latest {y} ⚠ STALE — feed frozen at an old year?)")
        } else {
            match ly {
                Some(y) => format!("  (latest {y})"),
                None => String::new(),
            }
        };
        println!(
            "  {:<9} {:>9} {:>9} {:>9} {:>9} {:>9}{note}",
            label,
            pct(lv),
            cum(2),
            cum(5),
            cum(8),
            cum(20)
        );
    }
    euribor
}
