//! One file per subcommand — each owns its arg logic and output, nothing shared but the
//! tiny formatting helpers below. `main.rs` only dispatches.

pub mod accounts;
pub mod alert;
pub mod backtest;
pub mod check;
pub mod config;
pub mod explain;
pub mod perf;
pub mod report;
pub mod screen;
pub mod sim;
pub mod size;
pub mod track;
pub mod trade;

// (#389) lives in `picks` so the page engine (wasm, no `commands`) links it; this path still works.
pub use crate::picks::truncate;

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
    /// is all three, and a series too short for a horizon prints — there (#639) rather than a shorter
    /// span passed off as the full one; n/a is a dead feed's.
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
            ["EU", "3.0%", "6.1%", "15.9%", "34.4%", "80.6%", "115.7%", "—", "—", "2025 ⚠ STALE"],
            "a frozen feed still prints its numbers and says where they stop; 26 years is — at 30Y/40Y"
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

    /// (#486) The Bonds rows as EXACT STRINGS, for the same reason as the inflation ones: the page
    /// prints them verbatim. A flat 6% yield earns +6.2% a year, 41 years of it fills every column,
    /// USA in EUR lands right after USA with the dollar's 10% fall off the euro taken out, and short,
    /// stale and dead series say so rather than printing a shorter span as the full one: (#639) a
    /// short one prints — past its start, a dead one n/a everywhere. (#735) A span with a reading
    /// hovers its yearly rate and the yields it was bought at and ends at; a — or n/a cell hovers none.
    /// (#736) Then that rate after its region's inflation: USA's own, EU's for USA in EUR and Italy,
    /// at rates that differ so a swapped region shows, and none past the deflator's reach.
    #[test]
    fn bond_web_rows_publish_usa_twice_and_say_where_a_series_stops() {
        use std::collections::BTreeMap;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).expect("a real date");
        let end = 2026 * 12 + 8; // 2026-09
        let flat = |from: i32, to: i32| -> BTreeMap<i32, f64> { (from..=to).map(|m| (m, 6.0)).collect() };
        // USD per EUR: 1.0 until a year ago, then 1.1 (a euro buys more dollars, so dollars lost value)
        let fx: BTreeMap<i32, f64> = (end - 27 * 12..=end).map(|m| (m, if m > end - 12 { 1.1 } else { 1.0 })).collect();
        let yearly = |from: i32, rate: f64| -> BTreeMap<i32, f64> { (from..=2026).map(|y| (y, rate)).collect() };
        let inflations = [("USA", yearly(1986, 2.0)), ("EU", yearly(2000, 3.0))];
        let rows = bond_web_rows(
            &[("USA", flat(end - 41 * 12, end)), ("Italy", flat(end - 35 * 12, end)), ("Spain", flat(end - 41 * 12, end - 7)), ("Portugal", BTreeMap::new())],
            &fx,
            &inflations,
            today,
        );
        let cells = |r: &Vec<serde_json::Value>| r.iter().map(|c| c[1].as_str().expect("a text cell").to_string()).collect::<Vec<_>>();
        assert_eq!(
            rows[0].iter().map(|c| c[0].as_str().expect("a header")).collect::<Vec<_>>(),
            ["MARKET", "YIELD", "1Y", "2Y", "5Y", "10Y", "20Y", "27Y", "30Y", "40Y", "AS OF"]
        );
        assert_eq!(cells(&rows[0]), ["USA", "6.00%", "+6.2%", "+12.7%", "+34.9%", "+81.9%", "+231.0%", "+403.3%", "+502.3%", "+995.7%", "2026-09"]);
        assert_eq!(
            cells(&rows[1]),
            ["USA in EUR", "6.00%", "-3.5%", "+2.5%", "+22.6%", "+65.4%", "+200.9%", "+357.5%", "—", "—", "2026-09"],
            "the 1.1 a euro now costs divides every span that starts before it; EXUSEU's 27 years cap the row"
        );
        assert_eq!(cells(&rows[2])[9], "—", "Italy's 35 years stop short of 40Y");
        assert_eq!(cells(&rows[2])[8], "+502.3%");
        assert_eq!(cells(&rows[3])[10], "2026-02 ⚠ STALE", "8 months behind October");
        assert_eq!(
            cells(&rows[4]),
            ["Portugal", "n/a", "n/a", "n/a", "n/a", "n/a", "n/a", "n/a", "n/a", "n/a", "⚠ no data"]
        );
        assert_eq!(rows.len(), 5);
        assert_eq!(bond_web_rows(&[("Germany", flat(end - 5, end - 5))], &fx, &[], today)[0][10][1], "2026-04", "6 months behind is not yet stale");
        // (#735) the hover rides after a null link, as REPORTS' does
        assert_eq!(
            rows[0][3],
            serde_json::json!(["2Y", "+12.7%", null, "+6.2%/yr; started at 6.00% yield (2024-09), now 6.00%; after inflation +4.1%/yr"])
        );
        assert_eq!(
            rows[1][2][3],
            "-3.5%/yr; started at 6.00% yield (2025-09), now 6.00%; after inflation -6.3%/yr",
            "in EUR, the dollar yields it holds, less euro inflation"
        );
        assert_eq!(rows[2][7][3], "+6.2%/yr; started at 6.00% yield (1999-09), now 6.00%; after inflation +3.1%/yr", "Italy, EU's 27 years");
        assert_eq!(rows[2][8][3], "+6.2%/yr; started at 6.00% yield (1996-09), now 6.00%", "27 years of EU rates can't deflate 30");
        assert_eq!(rows[1][8].as_array().map(Vec::len), Some(2), "a — span hovers nothing");
        assert_eq!(rows[4][2].as_array().map(Vec::len), Some(2), "nor does an n/a one");
        // bought at 0.5% and sold at 3%: the price fall outruns a year of coupons
        let rise: BTreeMap<i32, f64> = (end - 12..=end).map(|m| (m, if m == end - 12 { 0.5 } else { 3.0 })).collect();
        assert_eq!(bond_web_rows(&[("Germany", rise.clone())], &fx, &[], today)[0][2], serde_json::json!(["1Y", "-18.9%", null, "-18.9%/yr; started at 0.50% yield (2025-09), now 3.00%"]));
        let own = [("Portugal", yearly(2000, 1.0)), ("EU", yearly(2000, 3.0))];
        assert_eq!(
            bond_web_rows(&[("Portugal", rise)], &fx, &own, today)[0][2][3],
            "-18.9%/yr; started at 0.50% yield (2025-09), now 3.00%; after inflation -19.7%/yr",
            "Portugal's own 1%, not EU's 3% (-21.3%)"
        );
    }

    /// (#488) A cell that rounds to zero carries no sign either way; one that rounds to 0.1 keeps it.
    #[test]
    fn signed_pct_drops_the_sign_only_off_a_rounded_zero() {
        assert_eq!([-0.04, 0.04, -0.06, 6.2].map(signed_pct), ["0.0%", "0.0%", "-0.1%", "+6.2%"]);
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

    /// (#1409) A daily chart from (y, m, d, close) bars, every other field blank.
    fn chart(bars: &[((i32, u32, u32), f64)]) -> crate::fetch::Chart {
        let n = bars.len();
        crate::fetch::Chart {
            dates: bars.iter().map(|&((y, m, d), _)| chrono::NaiveDate::from_ymd_opt(y, m, d).expect("a real date")).collect(),
            closes: bars.iter().map(|b| b.1).collect(),
            volumes: vec![0.0; n],
            opens: vec![f64::NAN; n],
            highs: vec![f64::NAN; n],
            lows: vec![f64::NAN; n],
            currency: "USD".into(),
            name: String::new(),
            instrument_type: String::new(),
            granularity: String::new(),
            divs: vec![],
            splits: vec![],
        }
    }

    /// (#1409) Each row's cell texts, so a whole table reads as one assert.
    fn texts(rows: &[Vec<serde_json::Value>]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|c| c[1].as_str().expect("a text cell").to_string()).collect()).collect()
    }

    /// (#1409) The week strip: the S&P 500 reads its last close against the last close on or before 7 days back (a
    /// Friday over a weekend gap, never the older bar), a chart under a week old and a missing one read n/a; then the
    /// buy list against the base: joiners with BUY% and which half bought them, leavers with today's reason.
    #[test]
    fn week_web_rows_read_markets_then_who_joined_and_left() {
        let spx = chart(&[((2026, 10, 29), 50.0), ((2026, 10, 30), 100.0), ((2026, 11, 2), 101.0), ((2026, 11, 6), 110.0)]);
        let short = chart(&[((2026, 11, 5), 1.0), ((2026, 11, 6), 2.0)]);
        let markets = [("S&P 500", Some(&spx), "in dollars"), ("Euro vs dollar", Some(&short), "euro"), ("Bitcoin", None, "in euros")];
        let own = |v: &[(&str, f64)]| v.iter().map(|(t, w)| (t.to_string(), *w)).collect::<Vec<_>>();
        let now = own(&[("A", 40.0), ("M", 10.0), ("B", 50.0)]);
        let shipped = ["A".to_string(), "B".to_string()];
        let before = own(&[("A", 50.0), ("L", 25.0), ("Z", 25.0)]);
        let l = crate::core::Quote::stub("L", "1", "", "L");
        let tuning = crate::config::BuyHeuristic::default();
        let why_l = left_because(Some(&crate::picks::unranked_reason(&l, &tuning)));
        let rows = week_web_rows(markets, &now, &shipped, Some(("2026-10-30", before.as_slice())), std::slice::from_ref(&l), &tuning);
        let s = |v: [&str; 3]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            texts(&rows),
            vec![
                s(["S&P 500", "+10.0%", "2026-10-30 to 2026-11-06, in dollars"]),
                s(["Euro vs dollar", "n/a", "no price a week old this run"]),
                s(["Bitcoin", "n/a", "no price a week old this run"]),
                s(["Joined: M", "BUY% 10.0%", "not on the buy list on 2026-10-30; bought for momentum"]),
                s(["Joined: B", "BUY% 50.0%", "not on the buy list on 2026-10-30; bought for its buy score"]),
                s(["Left: L", "out", why_l.as_str()]),
                s(["Left: Z", "out", "not priced this run"]),
            ]
        );
        let none = week_web_rows(markets, &now, &shipped, None, &[], &tuning);
        assert_eq!(texts(&none)[3..], [s(["Buy list", "n/a", "no saved buy list yet to compare with"])]);
        let same = week_web_rows(markets, &now, &shipped, Some(("2026-10-30", now.as_slice())), &[], &tuning);
        assert_eq!(texts(&same)[3..], [s(["Buy list", "no change", "same names as on 2026-10-30"])]);
        assert_eq!(left_because(Some("clears every gate")), "still passes the buy rules; higher-ranked names or a cap took its seat");
        assert_eq!(left_because(Some("leveraged")), "now fails: leveraged");
    }

    /// (#1409) Headlines by day at New York winter time: 03:00 UTC on Nov 9 is still Sunday Nov 8 there, and a
    /// 03:00 UTC Saturday headline is Friday's. Newest first, a repeated title shows once (the newest), a day with
    /// none reads "(no headline)", a day's S&P 500 is its close vs the session before, and a week-old headline is out.
    #[test]
    fn news_web_rows_group_headlines_by_new_york_day() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).expect("a test time");
        let h = |t: &str, when: &str, source: &'static str| crate::fetch::Headline { at: at(when), source, title: t.into(), link: format!("https://x/{}", t.to_lowercase()) };
        let headlines = [
            h("Late", "2026-11-09T02:00:00Z", "CNBC Finance"),
            h("Jobs", "2026-11-06T15:00:00Z", "CNBC Economy"),
            h("Jobs", "2026-11-06T14:00:00Z", "CNBC Finance"),
            h("Fed", "2026-11-07T03:00:00Z", "ECB"),
            h("Old", "2026-10-30T12:00:00Z", "ECB"),
        ];
        let spx = chart(&[((2026, 11, 4), 100.0), ((2026, 11, 5), 102.0), ((2026, 11, 6), 99.96)]);
        let now = at("2026-11-09T03:00:00Z").with_timezone(&chrono::Utc);
        let rows = news_web_rows(&headlines, Some(&spx), now);
        let s = |v: [&str; 4]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            texts(&rows),
            vec![
                s(["Sun 2026-11-08", "no session", "CNBC Finance", "Late"]),
                s(["Sat 2026-11-07", "no session", "—", "(no headline)"]),
                s(["Fri 2026-11-06", "-2.0%", "ECB", "Fed"]),
                s(["Fri 2026-11-06", "-2.0%", "CNBC Economy", "Jobs"]),
                s(["Thu 2026-11-05", "+2.0%", "—", "(no headline)"]),
                s(["Wed 2026-11-04", "no session", "—", "(no headline)"]),
                s(["Tue 2026-11-03", "no session", "—", "(no headline)"]),
                s(["Mon 2026-11-02", "no session", "—", "(no headline)"]),
            ]
        );
        assert_eq!(rows[0][3], serde_json::json!(["HEADLINE", "Late", "https://x/late"]));
        assert!(news_web_rows(&[], Some(&spx), now).is_empty(), "no feed answered: no rows");
    }
}

/// (#366) The inflation table's cumulative horizons, shared by the footer and the page so the two
/// cannot drift. 2Y/5Y/20Y line up with the ranked tables' growth columns; 10Y/30Y/40Y are the long
/// view a 20+ year hold is judged against. A region too short for a horizon prints — there (#639; EU
/// HICP starts in 2000, so its 30Y/40Y are — by design: no older basket is spliced under it).
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
/// growth columns are deflated by EU HICP alone — when they are deflated at all: CI's `score_on_nominal`
/// prints them nominal (#400), and the page's glossary says which. The EU row is the deflator a real
/// column carries, USA and Portugal are context, and none of them is applied to a row by the reader.
///
/// Rows are `(header, cell)` pairs — the SAME shape the three lanes use — so `web/index.html` renders
/// this with the table builder it already has and no cell is formatted in two places.
///
/// Nothing here derives a number: [`crate::core::inflation_summary`],
/// [`crate::core::inflation_compounded`] and [`crate::core::infl_series_stale`] are the three helpers
/// the footer itself calls, and [`pct`] is the
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
            let (year, latest) = crate::core::inflation_summary(series);
            // (#639) a series too short for the span is —; n/a = a dead feed, every cell
            let cum = |years| match crate::core::inflation_compounded(series, years) {
                None if !series.is_empty() => "—".to_string(),
                v => pct(v),
            };
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

/// (#488) A return cell as `+6.2%`, but one that rounds to nothing prints `0.0%`: the US 10Y's
/// -0.04% read "-0.0%" on the page and looked like a bug.
fn signed_pct(v: f64) -> String {
    let s = format!("{v:+.1}%");
    if &s[1..] == "0.0%" { s[1..].to_string() } else { s }
}

/// (#486) The page's Bonds table: per market, the newest 10Y yield and what a constant-maturity 10Y
/// government bond returned over each [`crate::core::BOND_YEARS`] span, in its own currency, compounded
/// (`core::bond_index`). "USA in EUR" follows USA: the same dollar index divided by USD per EUR, so it
/// carries the currency swing a euro holder took, and stops where EXUSEU does (1999). Display only.
/// `AS OF` is the newest month; ⚠ STALE past 6 months, as a dead series still prints its last numbers.
///
/// (#736) `inflations` is [`inflation_web_rows`]' input. A span's hover adds its yearly rate after the
/// inflation of the currency's region: US CPI for USA, Portugal's for Portugal, EU HICP for the euro
/// markets and for USA in EUR, whose euros a euro holder spends.
pub(crate) fn bond_web_rows(
    yields: &[(&str, std::collections::BTreeMap<i32, f64>)],
    usd_per_eur: &std::collections::BTreeMap<i32, f64>,
    inflations: &[(&'static str, std::collections::BTreeMap<i32, f64>)],
    today: chrono::NaiveDate,
) -> Vec<Vec<serde_json::Value>> {
    use chrono::Datelike;
    use serde_json::json;
    let now = today.year() * 12 + today.month0() as i32;
    let ym = |m: i32| format!("{}-{:02}", m.div_euclid(12), m.rem_euclid(12) + 1);
    let deflator = |region: &str| inflations.iter().find(|(label, _)| *label == region).map(|(_, s)| s);
    let row = |label: &str, yld: &std::collections::BTreeMap<i32, f64>, index: std::collections::BTreeMap<i32, f64>| {
        let infl = deflator(match label {
            "USA" | "Portugal" => label,
            _ => "EU",
        });
        let last = index.keys().next_back().copied();
        let as_of = match last {
            None => "⚠ no data".to_string(),
            Some(m) if now - m > 6 => format!("{} ⚠ STALE", ym(m)),
            Some(m) => ym(m),
        };
        let mut cells = vec![json!(["MARKET", label]), json!(["YIELD", yld.values().next_back().map_or("n/a".to_string(), |y| format!("{y:.2}%"))])];
        // (#639) a span reaching past the start of the unbroken record is —; n/a = no series at all
        let blank = if index.is_empty() { "n/a" } else { "—" };
        cells.extend(crate::core::BOND_YEARS.iter().map(|&y| {
            let Some(cum) = crate::core::bond_cum(&index, y) else { return json!([format!("{y}Y"), blank]) };
            // (#735) the hover: the span's yearly rate and the yield its par bond was bought at (the
            // index's first month is the purchase), so a span bought near 0% before 2022's rise reads
            // why it lost, and one bought at a 6% coupon why it gained
            let start = last.map(|m| m - 12 * y);
            match start.and_then(|m0| Some((m0, yld.get(&m0)?, yld.get(&last?)?))) {
                Some((m0, y0, y1)) => {
                    let per_year = |growth: f64| signed_pct((growth.powf(1.0 / f64::from(y)) - 1.0) * 100.0);
                    // shortcut: the deflator is the last `y` YEARLY rates, the Inflation table's own
                    // number for the span (one year of slack included), not the span's months; monthly CPI
                    // would align the ends
                    let real = infl.and_then(|s| crate::core::inflation_compounded(s, y as usize));
                    let real = real.map_or(String::new(), |i| format!("; after inflation {}/yr", per_year((1.0 + cum / 100.0) / (1.0 + i / 100.0))));
                    let rate = per_year(1.0 + cum / 100.0);
                    json!([format!("{y}Y"), signed_pct(cum), null, format!("{rate}/yr; started at {y0:.2}% yield ({}), now {y1:.2}%{real}", ym(m0))])
                }
                None => json!([format!("{y}Y"), signed_pct(cum)]),
            }
        }));
        cells.push(json!(["AS OF", as_of]));
        cells
    };
    yields
        .iter()
        .flat_map(|(label, yld)| {
            let index = crate::core::bond_index(yld);
            let eur = (*label == "USA").then(|| {
                let eur = index.iter().filter_map(|(m, v)| usd_per_eur.get(m).map(|fx| (*m, v / fx))).collect();
                row("USA in EUR", yld, eur)
            });
            std::iter::once(row(label, yld, index)).chain(eur)
        })
        .collect()
}

/// (#1409) A chart's week: the last close vs the last close dated on or before its last date minus 7 days, as
/// (from, to, %). None with no bar that old.
fn week_move(c: &crate::fetch::Chart) -> Option<(chrono::NaiveDate, chrono::NaiveDate, f64)> {
    let (&to, &now) = (c.dates.last()?, c.closes.last()?);
    let i = c.dates.partition_point(|d| *d <= to - chrono::Duration::days(7)).checked_sub(1)?;
    Some((c.dates[i], to, (now / c.closes[i] - 1.0) * 100.0))
}

/// (#1409) Why a name left the buy list, off `picks::unranked_reason` today (None = no quote this run).
fn left_because(reason: Option<&str>) -> String {
    match reason {
        None => "not priced this run".into(),
        Some("clears every gate") => "still passes the buy rules; higher-ranked names or a cap took its seat".into(),
        Some(r) => format!("now fails: {r}"),
    }
}

/// (#1409) The page's "This week" strip, display only: each market's [`week_move`], then the buy list against `base`
/// (the journal's (date, sized book) about a week back, `track::week_base`): joiners with their BUY% and which half
/// bought them, leavers with [`left_because`]. One row says so when there is no base or no change.
pub(crate) fn week_web_rows(
    markets: [(&str, Option<&crate::fetch::Chart>, &str); 3],
    sized_now: &[(String, f64)],
    shipped_now: &[String],
    base: Option<(&str, &[(String, f64)])>,
    quotes: &[crate::core::Quote],
    tuning: &crate::config::BuyHeuristic,
) -> Vec<Vec<serde_json::Value>> {
    use serde_json::json;
    let row = |what: String, change: String, detail: String| vec![json!(["WHAT", what]), json!(["CHANGE", change]), json!(["DETAIL", detail])];
    let mut rows: Vec<_> = markets
        .iter()
        .map(|&(label, chart, unit)| match chart.and_then(week_move) {
            Some((from, to, pct)) => row(label.into(), signed_pct(pct), format!("{from} to {to}, {unit}")),
            None => row(label.into(), "n/a".into(), "no price a week old this run".into()),
        })
        .collect();
    let Some((date, before)) = base else {
        rows.push(row("Buy list".into(), "n/a".into(), "no saved buy list yet to compare with".into()));
        return rows;
    };
    let has = |book: &[(String, f64)], t: &str| book.iter().any(|(x, _)| x == t);
    for (t, w) in sized_now.iter().filter(|(t, _)| !has(before, t)) {
        let why = if shipped_now.contains(t) { "its buy score" } else { "momentum" };
        rows.push(row(format!("Joined: {t}"), format!("BUY% {w:.1}%"), format!("not on the buy list on {date}; bought for {why}")));
    }
    for (t, _) in before.iter().filter(|(t, _)| !has(sized_now, t)) {
        let reason = quotes.iter().find(|q| &q.ticker == t).map(|q| crate::picks::unranked_reason(q, tuning));
        rows.push(row(format!("Left: {t}"), "out".into(), left_because(reason.as_deref())));
    }
    if rows.len() == markets.len() {
        rows.push(row("Buy list".into(), "no change".into(), format!("same names as on {date}")));
    }
    rows
}

/// (#1409) The fixed UTC offset the page's news days are cut at: New York's winter time.
/// shortcut: an hour off under US daylight saving (a 23:00-24:00 New York headline lands a day late); add a tz table
/// if a misdated headline shows.
const NEWS_UTC_OFFSET_SECS: i32 = -5 * 3600;

/// (#1409) The page's headlines by day, display only: the 7 days to `now`'s, newest first. Each row carries that day's
/// S&P 500 close vs the session before (or "no session"), the feed and the linked title. Only the newest copy of a
/// repeated title shows; a day with none gets one "(no headline)" row. No headline at all = no rows (feeds unavailable).
pub(crate) fn news_web_rows(headlines: &[crate::fetch::Headline], spx: Option<&crate::fetch::Chart>, now: chrono::DateTime<chrono::Utc>) -> Vec<Vec<serde_json::Value>> {
    use serde_json::json;
    if headlines.is_empty() {
        return Vec::new();
    }
    let tz = chrono::FixedOffset::east_opt(NEWS_UTC_OFFSET_SECS).expect("a valid offset");
    let today = now.with_timezone(&tz).date_naive();
    let session = |d: chrono::NaiveDate| {
        spx.and_then(|c| {
            let i = c.dates.iter().position(|x| *x == d)?.checked_sub(1)?;
            Some(signed_pct((c.closes[i + 1] / c.closes[i] - 1.0) * 100.0))
        })
        .unwrap_or_else(|| "no session".into())
    };
    let mut sorted: Vec<_> = headlines.iter().collect();
    sorted.sort_by_key(|h| std::cmp::Reverse(h.at));
    let mut seen = std::collections::HashSet::new();
    let sorted: Vec<_> = sorted.into_iter().filter(|h| seen.insert(h.title.as_str())).collect();
    (0..7)
        .flat_map(|back| {
            let day = today - chrono::Duration::days(back);
            let spx_cell = session(day);
            let cells = |source: &str, headline: serde_json::Value| {
                vec![json!(["DAY", day.format("%a %Y-%m-%d").to_string()]), json!(["S&P 500", spx_cell]), json!(["SOURCE", source]), headline]
            };
            let mut rows: Vec<_> = sorted.iter().filter(|h| h.at.with_timezone(&tz).date_naive() == day).map(|h| cells(h.source, json!(["HEADLINE", h.title, h.link]))).collect();
            if rows.is_empty() {
                rows.push(cells("—", json!(["HEADLINE", "(no headline)"])));
            }
            rows
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
        let (ly, lv) = core::inflation_summary(series);
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
