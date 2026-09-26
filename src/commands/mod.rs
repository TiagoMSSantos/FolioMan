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

    /// (#249) The page's inflation rows, asserted as EXACT STRINGS. `web/index.html` re-formats
    /// nothing — it prints these cells verbatim — so a shifted horizon, a renamed header or a
    /// swapped region lands straight in the published table with no other check between here and
    /// the site. (#366) The horizons are [`INFL_YEARS`] (plus the EU's span, #367), the region set
    /// is all three, and a series too short for a horizon prints n/a there rather than a shorter
    /// span passed off as the full one.
    #[test]
    fn inflation_web_rows_publish_all_three_regions_out_to_40y() {
        use std::collections::BTreeMap;
        let steady: BTreeMap<i32, f64> = (1987..=2026).map(|y| (y, 2.0)).collect(); // healthy: 40 rates, carries the current year
        let pt: BTreeMap<i32, f64> = (1987..=2026).map(|y| (y, 1.0)).collect();
        let frozen: BTreeMap<i32, f64> = (2000..=2025).map(|y| (y, 3.0)).collect(); // EU HICP's depth, feed terminated at 2025
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 6).expect("a real date");

        let rows = inflation_web_rows(&[("Portugal", pt), ("USA", steady.clone()), ("EU", frozen.clone())], today);
        assert_eq!(rows.len(), 3, "(#366) Portugal is published too");
        assert_eq!(
            rows[0].iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(),
            ["REGION", "LATEST", "2Y", "5Y", "10Y", "20Y", "26Y", "30Y", "40Y", "AS OF"],
            "the horizons, in order — (#367) with the EU's whole span (26 rates here) sorted in"
        );
        // USA first even though `inflation_all` returns Portugal, USA, EU — published order is this
        // function's, not the fetch's. Cells are compounded, not multiplied: 2%/yr for 40y is +120.8%.
        assert_eq!(
            rows[0].iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            ["USA", "2.0%", "4.0%", "10.4%", "21.9%", "48.6%", "67.3%", "81.1%", "120.8%", "2026"]
        );
        assert_eq!(
            rows[1].iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            ["EU", "3.0%", "6.1%", "15.9%", "34.4%", "80.6%", "115.7%", "n/a", "n/a", "2025 ⚠ STALE"],
            "a frozen feed still prints its numbers and says where they stop; 26 years is n/a at 30Y/40Y"
        );
        assert_eq!(
            rows[2].iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            ["Portugal", "1.0%", "2.0%", "5.1%", "10.5%", "22.0%", "29.5%", "34.8%", "48.9%", "2026"]
        );

        // dead feed: every cell n/a, and AS OF says so rather than leaving the row to read as measured.
        // The EU here spans 40 rates, which dedups into 40Y: no extra column.
        let dead = inflation_web_rows(&[("USA", BTreeMap::new()), ("EU", steady.clone())], today);
        assert_eq!(
            dead[0].iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            ["USA", "n/a", "n/a", "n/a", "n/a", "n/a", "n/a", "n/a", "⚠ no data"]
        );
        assert_eq!(dead[1][8].1, "2026", "the healthy row is unaffected by its neighbour's dead feed");

        // a region the fetch never returned is simply absent — never an empty row, never a panic
        assert_eq!(inflation_web_rows(&[("EU", steady)], today).len(), 1);
        assert!(inflation_web_rows(&[], today).is_empty());
    }

    /// (#367) The EU-span column's two decisions, off the `println!` sink where nothing could see them:
    /// which row sets the span (the EU's, by label — Portugal is longer, so a wrong match shows), and
    /// where it lands among the fixed horizons (sorted in; a span equal to one of them adds nothing).
    #[test]
    fn the_eu_span_joins_the_horizons_sorted_and_deduped() {
        use std::collections::BTreeMap;
        let series = |n: i32| (2027 - n..=2026).map(|y| (y, 2.0)).collect::<BTreeMap<i32, f64>>();
        assert_eq!(eu_span(&[("Portugal", series(78)), ("USA", series(39)), ("EU", series(27))]), Some(27));
        assert_eq!(eu_span(&[("Portugal", series(78)), ("EU", BTreeMap::new())]), None, "a dead EU feed adds no column");
        assert_eq!(eu_span(&[("Portugal", series(78))]), None);
        assert_eq!(infl_years(None), INFL_YEARS);
        assert_eq!(infl_years(Some(27)), [2, 5, 10, 20, 27, 30, 40]);
        assert_eq!(infl_years(Some(30)), INFL_YEARS, "2029: the span meets 30Y and is not printed twice");
        assert_eq!(infl_years(Some(1)), [1, 2, 5, 10, 20, 30, 40]);
    }

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

/// (#366) The inflation table's cumulative horizons, shared by the footer and the page so the two
/// cannot drift. 2Y/5Y/20Y line up with the ranked tables' growth columns; 10Y/30Y/40Y are the long
/// view a 20+ year hold is judged against. A region too short for a horizon prints n/a there (EU HICP
/// starts in 2000, so its 30Y/40Y are n/a by design — no older basket is spliced under it).
/// (#367) At run time [`eu_span`] joins these through [`infl_years`], so every region is also read
/// over EU27's whole history — the longest span the EU row can be compared on.
const INFL_YEARS: [usize; 6] = [2, 5, 10, 20, 30, 40];

/// (#367) EU27's full HICP history in years — its annual-rate count, 27 in 2026 (2000..=2026). None
/// when the feed returned no EU row, or an empty one: a dead feed adds no column.
fn eu_span(inflations: &[(&'static str, std::collections::BTreeMap<i32, f64>)]) -> Option<usize> {
    inflations.iter().find(|(label, _)| *label == "EU").map(|(_, s)| s.len()).filter(|&n| n > 0)
}

/// (#367) The table's horizons: [`INFL_YEARS`] plus the EU span, sorted in and deduped — the span
/// grows a year each January and meets 30Y in 2029, when it stops adding a column of its own.
fn infl_years(eu: Option<usize>) -> Vec<usize> {
    let mut ys = INFL_YEARS.to_vec();
    ys.extend(eu);
    ys.sort_unstable();
    ys.dedup();
    ys
}

/// (#249) The Pages site's copy of the inflation table `print_macro_footer` prints below, emitted as
/// rows instead of printed.
///
/// WHY IT IS ON THE PAGE AT ALL: the three ranked tables carry `2Y 5Y 8Y 20Y` columns and this table
/// is cumulative at [`INFL_YEARS`], so a row reads straight across the 2Y/5Y/20Y growth columns with no
/// arithmetic in the reader's head.
///
/// (#366) ALL THREE REGIONS, as the footer prints them. (#249) had dropped Portugal because the page's
/// growth columns are deflated by EU HICP alone — that still holds: the EU row is the deflator that
/// was applied, USA and Portugal are context, and none of them is applied to a row by the reader.
///
/// Rows are `(header, cell)` pairs — the SAME shape the three lanes use — so `web/index.html` renders
/// this with the table builder it already has and no cell is formatted in two places.
///
/// Nothing here derives a number: [`core::inflation_summary`], [`core::inflation_compounded`] and
/// [`core::infl_series_stale`] are the three helpers the footer itself calls, and [`pct`] is the
/// footer's own formatter. `AS OF` carries the footer's three states (healthy year / frozen feed /
/// dead feed) because a dead feed reaches the page as a row of `n/a` cells, and a page with no other
/// tell would publish that as if it were measured.
pub(crate) fn inflation_web_rows(
    inflations: &[(&'static str, std::collections::BTreeMap<i32, f64>)],
    today: chrono::NaiveDate,
) -> Vec<Vec<(String, String)>> {
    // Published order, not the fetch order: `inflation_all` returns Portugal first, and the page
    // wants the deflator it applied (EU) read against the dollar context (USA).
    const PUBLISHED: [&str; 3] = ["USA", "EU", "Portugal"];
    let ys = infl_years(eu_span(inflations));
    PUBLISHED
        .iter()
        .filter_map(|want| inflations.iter().find(|(label, _)| label == want))
        .map(|(label, series)| {
            let (year, latest, _, _) = crate::core::inflation_summary(series);
            let cum = |years| pct(crate::core::inflation_compounded(series, years));
            // An empty series has no latest year AND is never "stale" (`infl_series_stale` returns
            // None on empty, which is why the dead-feed arm reads off `year` rather than re-testing
            // `is_empty()` — one source for "is there anything here at all").
            let as_of = match (crate::core::infl_series_stale(series, today), year) {
                (Some(frozen), _) => format!("{frozen} ⚠ STALE"),
                (None, Some(y)) => y.to_string(),
                (None, None) => "⚠ no data".to_string(),
            };
            let mut row = vec![("REGION".to_string(), (*label).to_string()), ("LATEST".to_string(), pct(latest))];
            row.extend(ys.iter().map(|&y| (format!("{y}Y"), cum(y))));
            row.push(("AS OF".to_string(), as_of));
            row
        })
        .collect()
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

    let eu = eu_span(&inflations);
    let ys = infl_years(eu);
    let span = eu.map(|n| format!(" ({n}Y = EU27's whole HICP history)")).unwrap_or_default();
    println!("\nInflation — latest annual % + cumulative price rise (compounded) over last N years{span}:");
    let heads: String = ys.iter().map(|y| format!(" {:>9}", format!("{y}Y"))).collect();
    println!("  {:<9} {:>9}{heads}", "", "latest");
    for (label, series) in &inflations {
        let (ly, lv, _, _) = core::inflation_summary(series);
        let cums: String =
            ys.iter().map(|&y| format!(" {:>9}", pct(core::inflation_compounded(series, y)))).collect();
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
        println!("  {:<9} {:>9}{cums}{note}", label, pct(lv));
    }
    euribor
}
