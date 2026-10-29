//! (#1217) THE HOLD-WINDOW CENSUS: buy, hold a calendar span, sell. Which span pays the most a year per
//! asset class, net of one round trip? `backtest N holdwindows` prices every hold in [`HOLDS`] on each
//! class's whole pond at equal weight: 1d to 6m on 10y of daily bars, bought and sold at the open or the
//! close, and 1m to 20y on the max monthly bars. Beside the pond, a book of the top [`BOOK`] names per
//! class by growth score at each half-year cutoff, and the S&P 500 as it stood on each buy date against
//! today's members. Prices are total return, each dividend reinvested at the last close before its
//! ex-date (the (#804) rule). A census, not a knob: nothing here reaches a lane, BUY NOW, the page or a
//! trade.

use super::intraday::stats;
use crate::core::Quote;
use crate::picks;
use chrono::{Datelike, Duration, Months, NaiveDate};
use std::collections::BTreeMap;

/// A hold: its label, then the calendar days and months it adds to the buy date.
pub type Hold = (&'static str, u32, u32);
/// Every hold the census prices. Daily bars price `HOLDS[..6]` (1d to 6m), monthly ones `HOLDS[3..]`.
pub const HOLDS: [Hold; 13] = [
    ("1d", 1, 0), ("1w", 7, 0), ("2w", 14, 0), ("1m", 0, 1), ("3m", 0, 3), ("6m", 0, 6), ("1y", 0, 12), ("2y", 0, 24), ("3y", 0, 36),
    ("5y", 0, 60), ("10y", 0, 120), ("15y", 0, 180), ("20y", 0, 240),
];
/// Names per class in the screen book.
pub const BOOK: usize = 10;
/// The classes, in print order.
pub const CLASSES: [&str; 7] = ["stock", "REIT stock", "equity ETF", "bond ETF", "gold/commodity ETF", "REIT fund", "crypto"];
/// Name tokens, matched lower-case, that make a fund a bond fund.
const BOND_TOKENS: [&str; 13] = [
    "bond", "treasury", "govt", "gilt", "bund", "aggregate", "corporate", "credit", "high yield", "fixed income", "money market", "t-bill",
    "inflation-linked",
];

/// The date `hold` after `d`: its months, then its days. `NaiveDate::MAX` past the calendar's end.
pub fn after(d: NaiveDate, hold: Hold) -> NaiveDate {
    d.checked_add_months(Months::new(hold.2)).and_then(|x| x.checked_add_signed(Duration::days(hold.1.into()))).unwrap_or(NaiveDate::MAX)
}

/// One name's prices as the instants a trade can buy or sell at.
pub struct Series {
    pub open_at: Vec<NaiveDate>,
    pub close_at: Vec<NaiveDate>,
    pub opens: Vec<f64>,
    pub closes: Vec<f64>,
}

/// `dates`' bars as a [`Series`], total return. Each dividend is reinvested at the last close before its
/// ex-date and lifts every price after the ex-date: an open dated on or after it, and a close whose bar
/// ends on or after it. A daily bar opens and closes on its date; a monthly bar opens on the 1st and
/// closes on the next month's 1st, so a 1m buy at an open sells at that month's close. A dividend with no
/// close before its ex-date (on or before the first daily bar, inside the first monthly one) has nothing
/// to reinvest at and is dropped.
pub fn series(dates: &[NaiveDate], opens: &[f64], closes: &[f64], divs: &[(NaiveDate, f64)], monthly: bool) -> Series {
    let open_at: Vec<NaiveDate> = if monthly { dates.iter().map(|d| d.with_day(1).unwrap_or(*d)).collect() } else { dates.to_vec() };
    let close_at: Vec<NaiveDate> = if monthly { open_at.iter().map(|d| after(*d, ("", 0, 1))).collect() } else { dates.to_vec() };
    // the last day each bar's close covers
    let ends: Vec<NaiveDate> = if monthly { close_at.iter().map(|d| *d - Duration::days(1)).collect() } else { dates.to_vec() };
    let (mut lift_open, mut lift_close) = (vec![1.0; dates.len()], vec![1.0; dates.len()]);
    for (d, amount) in divs {
        let p = ends.partition_point(|x| x < d);
        let Some(m) = p.checked_sub(1).map(|q| 1.0 + amount / closes[q]).filter(|m| m.is_finite() && *m > 0.0) else { continue };
        for (at, lift) in [(&open_at, &mut lift_open), (&ends, &mut lift_close)] {
            if let Some(l) = lift.get_mut(at.partition_point(|x| x < d)) {
                *l *= m;
            }
        }
    }
    let total = |px: &[f64], lift: &[f64]| -> Vec<f64> {
        let mut f = 1.0;
        px.iter().zip(lift).map(|(p, l)| {
            f *= l;
            p * f
        }).collect()
    };
    Series { opens: total(opens, &lift_open), closes: total(closes, &lift_close), open_at, close_at }
}

/// A trade: the buy date, the gross return and the days held.
pub type Trade = (NaiveDate, f64, i64);

/// The trade `s` holds for `hold` from bar `i`: a buy at its open (`buy_open`) or close, sold at the first
/// open (`sell_open`) or close on or after the buy plus `hold`. `None` when the series ends before
/// selling, or either price is not finite and positive.
pub fn trade(s: &Series, buy_open: bool, sell_open: bool, i: usize, hold: Hold) -> Option<Trade> {
    let side = |open: bool| if open { (&s.open_at, &s.opens) } else { (&s.close_at, &s.closes) };
    let ((buy_at, buy), (sell_at, sell)) = (side(buy_open), side(sell_open));
    let ok = |p: f64| p.is_finite() && p > 0.0;
    let (t, b) = (*buy_at.get(i)?, *buy.get(i)?);
    let j = sell_at.partition_point(|x| *x < after(t, hold));
    let x = *sell.get(j)?;
    (ok(b) && ok(x)).then(|| (t, x / b - 1.0, (sell_at[j] - t).num_days()))
}

/// [`trade`] from every bar.
pub fn trades(s: &Series, buy_open: bool, sell_open: bool, hold: Hold) -> Vec<Trade> {
    (0..s.open_at.len()).filter_map(|i| trade(s, buy_open, sell_open, i, hold)).collect()
}

/// Per buy date: the net and gross returns added up over every name bought on it, their days held, and
/// the name count.
pub type Acc = BTreeMap<NaiveDate, (f64, f64, i64, usize)>;

/// Adds `trades` to `acc`, each net of `cost`, one round trip as a fraction.
pub fn add(acc: &mut Acc, trades: &[Trade], cost: f64) {
    for &(d, g, days) in trades {
        let e = acc.entry(d).or_default();
        *e = (e.0 + g - cost, e.1 + g, e.2 + days, e.3 + 1);
    }
}

/// One printed hold line and what [`verdict`] reads off it.
pub struct Line {
    pub label: String,
    pub hold: Hold,
    pub text: String,
    /// net %/yr, `None` when the line has too few starts
    pub net: Option<f64>,
    /// net %/yr of each half
    pub halves: (f64, f64),
    pub t: Option<f64>,
}

/// A buy date's equal-weight basket: date, net, gross, mean days held.
type Basket = (NaiveDate, f64, f64, f64);

/// The compound rate of `bs`, %/yr: the mean log return, net or `gross`, over their mean days held.
fn per_year(bs: &[&Basket], gross: bool) -> f64 {
    let n = bs.len() as f64;
    let ln = bs.iter().map(|b| (1.0 + if gross { b.2 } else { b.1 }).max(1e-9).ln()).sum::<f64>() / n;
    ((ln * 365.25 / (bs.iter().map(|b| b.3).sum::<f64>() / n)).exp() - 1.0) * 100.0
}

/// The hold line for `acc`, labelled `label`. Each buy date is one equal-weight basket of every name
/// bought on it. Rates compound over the mean days held; mean, median and win are per basket, net. The t
/// and the halves read only baskets one hold apart, the first and then each next one on or after the last
/// plus `hold`, halves split by count; t is "n/a" under 3 of them, and under 2 the line reads "too few".
pub fn line(label: &str, acc: &Acc, hold: Hold) -> Line {
    let all: Vec<Basket> = acc.iter().map(|(d, &(net, gross, days, n))| (*d, net / n as f64, gross / n as f64, days as f64 / n as f64)).collect();
    let mut spaced: Vec<&Basket> = Vec::new();
    for b in &all {
        if spaced.last().is_none_or(|p| b.0 >= after(p.0, hold)) {
            spaced.push(b);
        }
    }
    let head = format!("  {label} {}  {} starts", hold.0, all.len());
    let mut out = Line { label: label.to_string(), hold, text: format!("{head}  too few"), net: None, halves: (0.0, 0.0), t: None };
    let (Some(first), Some(last), true) = (all.first(), all.last(), spaced.len() >= 2) else { return out };
    let mut net: Vec<f64> = all.iter().map(|b| b.1).collect();
    let mean = net.iter().sum::<f64>() / net.len() as f64;
    let win = net.iter().filter(|r| **r > 0.0).count() as f64 / net.len() as f64 * 100.0;
    net.sort_by(f64::total_cmp);
    let k = net.len() / 2;
    let median = if net.len() % 2 == 1 { net[k] } else { (net[k - 1] + net[k]) / 2.0 };
    let sp: Vec<f64> = spaced.iter().map(|b| b.1).collect();
    out.t = stats(&sp).map(|s| s.t).filter(|t| sp.len() >= 3 && t.is_finite());
    let (h1, h2) = spaced.split_at(spaced.len() / 2);
    out.halves = (per_year(h1, false), per_year(h2, false));
    let refs: Vec<&Basket> = all.iter().collect();
    let rate = per_year(&refs, false);
    out.net = Some(rate);
    out.text = format!(
        "{head} {}..{}  net {rate:+.1}%/yr (gross {:+.1})  per trade mean {:+.2}% median {:+.2}% win {win:.0}%  spaced {} t {}  halves {:+.1}|{:+.1}",
        first.0,
        last.0,
        per_year(&refs, true),
        mean * 100.0,
        median * 100.0,
        sp.len(),
        out.t.map_or("n/a".to_string(), |t| format!("{t:.1}")),
        out.halves.0,
        out.halves.1
    );
    out
}

/// The verdict for one name set: its best line over every hold and its best at 6m or shorter, each
/// tagged against Série E's `hurdle` %/yr, its halves and its t. A miss prints as a miss.
pub fn verdict(set: &str, lines: &[Line], hurdle: f64) -> String {
    let best = |short: bool| {
        lines.iter().filter(|l| !short || l.hold.2 <= 6).filter_map(|l| Some((l, l.net?))).max_by(|a, b| a.1.total_cmp(&b.1))
    };
    let tag = |(l, net): (&Line, f64)| {
        format!(
            "{} {}  net {net:+.1}%/yr: {} Série E {hurdle:.1}, {}, {}",
            l.label,
            l.hold.0,
            if net > hurdle { "beats" } else { "misses" },
            if l.halves.0 > 0.0 && l.halves.1 > 0.0 { "both halves net > 0" } else { "a half loses" },
            match l.t {
                Some(t) if t >= 3.0 => format!("t {t:.1} ≥ 3"),
                Some(t) => format!("t {t:.1} < 3"),
                None => "t n/a".to_string(),
            }
        )
    };
    match best(false) {
        Some(b) => format!("  {set} best: {}\n  {set} best short: {}", tag(b), best(true).map_or("every hold to 6m too few".to_string(), tag)),
        None => format!("  {set}: every hold too few"),
    }
}

/// The census class of a quote the backtest stamped (`name_class`, `instrument_type`, `sector`), or
/// `None` for a leveraged or inverse fund. A hedged fund stays in its class.
pub fn class_of(q: &Quote) -> Option<&'static str> {
    let (etf, refusal) = q.name_class.unwrap_or_else(|| picks::name_class(q));
    let name = q.name.to_lowercase();
    Some(if picks::asset_class(q) == 0 {
        "crypto"
    } else if refusal == Some("leveraged") {
        return None;
    } else if let Some(kind) = picks::reit_kind(q) {
        if kind == "stock" { "REIT stock" } else { "REIT fund" }
    } else if !etf {
        "stock"
    } else if refusal == Some("commodity") {
        "gold/commodity ETF"
    } else if BOND_TOKENS.iter().any(|t| name.contains(t)) {
        "bond ETF"
    } else {
        "equity ETF"
    })
}

/// One round trip's cost for a name, in basis points: crypto 30, a euro line 20, any other currency 50.
pub fn cost_bps(class: &str, currency: &str) -> f64 {
    if class == "crypto" {
        30.0
    } else if currency == "EUR" {
        20.0
    } else {
        50.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn after_adds_months_then_days_and_clamps_the_month_end() {
        assert_eq!(after(d(2024, 1, 31), ("1m", 0, 1)), d(2024, 2, 29));
        assert_eq!(after(d(2024, 2, 29), ("1y", 0, 12)), d(2025, 2, 28));
        assert_eq!(after(d(2024, 1, 1), ("1w", 7, 0)), d(2024, 1, 8));
        assert_eq!(after(NaiveDate::MAX, ("1d", 1, 0)), NaiveDate::MAX);
        assert_eq!(HOLDS[..6].iter().map(|h| h.0).collect::<Vec<_>>(), ["1d", "1w", "2w", "1m", "3m", "6m"]);
        assert_eq!(HOLDS[3..].first().map(|h| h.0), Some("1m"));
    }

    #[test]
    fn series_reinvests_each_dividend_from_its_ex_date_on() {
        // daily: the ex-date bar's open and close both carry it, the bars before neither; one on the
        // first bar has no close before it and is dropped
        let dates = [d(2024, 1, 2), d(2024, 1, 3), d(2024, 1, 4), d(2024, 1, 5)];
        let s = series(&dates, &[10.0, 11.0, 12.0, 13.0], &[10.5, 11.5, 12.5, 13.5], &[(d(2024, 1, 2), 9.0), (d(2024, 1, 4), 0.5)], false);
        let m = 1.0 + 0.5 / 11.5;
        assert_eq!(s.opens, [10.0, 11.0, 12.0 * m, 13.0 * m]);
        assert_eq!(s.closes, [10.5, 11.5, 12.5 * m, 13.5 * m]);
        assert_eq!((s.open_at, s.close_at), (dates.to_vec(), dates.to_vec()));
        // monthly: a mid-month ex-date lifts that month's close and the next month's open; a bar dated
        // mid-month snaps to the 1st, and each close stands on the next month's 1st
        let dates = [d(2024, 1, 1), d(2024, 2, 1), d(2024, 3, 15)];
        let s = series(&dates, &[10.0, 11.0, 12.0], &[11.0, 12.0, 13.0], &[(d(2024, 2, 10), 1.0)], true);
        let m = 1.0 + 1.0 / 11.0;
        assert_eq!(s.opens, [10.0, 11.0, 12.0 * m]);
        assert_eq!(s.closes, [11.0, 12.0 * m, 13.0 * m]);
        assert_eq!(s.open_at, [d(2024, 1, 1), d(2024, 2, 1), d(2024, 3, 1)]);
        assert_eq!(s.close_at, [d(2024, 2, 1), d(2024, 3, 1), d(2024, 4, 1)]);
        // a bad close to reinvest at drops the dividend rather than poison every later price
        let s = series(&dates, &[10.0, 11.0, 12.0], &[0.0, 12.0, 13.0], &[(d(2024, 2, 10), 1.0)], true);
        assert_eq!(s.closes, [0.0, 12.0, 13.0]);
        // an ex-date on the 1st comes after the month before closed, so that day's open carries it
        let s = series(&dates, &[10.0, 11.0, 12.0], &[11.0, 12.0, 13.0], &[(d(2024, 2, 1), 1.0)], true);
        assert_eq!((s.opens, s.closes), (vec![10.0, 11.0 * m, 12.0 * m], vec![11.0, 12.0 * m, 13.0 * m]));
    }

    #[test]
    fn trades_sell_on_the_first_instant_past_the_hold() {
        // Fri, Mon, Tue; Monday's open is missing
        let dates = [d(2024, 1, 5), d(2024, 1, 8), d(2024, 1, 9)];
        let s = series(&dates, &[10.0, f64::NAN, 12.0], &[11.0, 12.0, 15.0], &[], false);
        let day = ("1d", 1, 0);
        assert_eq!(trades(&s, false, false, day), [(dates[0], 12.0 / 11.0 - 1.0, 3), (dates[1], 15.0 / 12.0 - 1.0, 1)]);
        assert_eq!(trades(&s, true, false, day), [(dates[0], 12.0 / 10.0 - 1.0, 3)]);
        assert!(trades(&s, true, true, day).is_empty());
        assert_eq!(trades(&s, false, true, day), [(dates[1], 0.0, 1)]);
        // one start: the last bar has nothing to sell at, and past the end there is no bar
        assert_eq!(trade(&s, false, false, 1, day), Some((dates[1], 15.0 / 12.0 - 1.0, 1)));
        assert_eq!((trade(&s, false, false, 2, day), trade(&s, false, false, 3, day)), (None, None));
        // monthly: a 1m buy at an open sells at the same month's close
        let dates = [d(2024, 1, 1), d(2024, 2, 1)];
        let s = series(&dates, &[10.0, 11.0], &[11.0, 12.0], &[], true);
        assert_eq!(trades(&s, true, false, ("1m", 0, 1)), [(dates[0], 11.0 / 10.0 - 1.0, 31), (dates[1], 12.0 / 11.0 - 1.0, 29)]);
        assert_eq!(trades(&s, true, true, ("1m", 0, 1)), [(dates[0], 11.0 / 10.0 - 1.0, 31)]);
    }

    #[test]
    fn line_prints_the_compound_rate_and_spaces_the_t_one_hold_apart() {
        let ds = [d(2020, 1, 1), d(2020, 1, 15), d(2020, 2, 1), d(2020, 2, 10), d(2020, 3, 1), d(2020, 4, 1)];
        let a: Vec<Trade> = ds.iter().zip([0.02, -0.01, 0.03, 0.01, 0.04, -0.02]).zip([31, 31, 29, 29, 31, 30]).map(|((&t, g), n)| (t, g, n)).collect();
        let b: Vec<Trade> = ds[..3].iter().zip([0.0, 0.01, 0.02]).zip([31, 31, 29]).map(|((&t, g), n)| (t, g, n)).collect();
        let mut acc = Acc::new();
        add(&mut acc, &a, 0.005);
        add(&mut acc, &b, 0.005);
        assert_eq!(acc[&ds[0]].2, 62);
        assert_eq!(acc[&ds[0]].3, 2);
        let l = line("stock monthly C-C", &acc, ("1m", 0, 1));
        assert_eq!(
            l.text,
            "  stock monthly C-C 1m  6 starts 2020-01-01..2020-04-01  net +7.1%/yr (gross +13.7)  per trade mean +0.58% median +0.50% win 67%  spaced 4 t 0.7  halves +16.3|+5.6"
        );
        assert_eq!((l.label.as_str(), l.hold.0, l.net.map(|x| (x * 10.0).round())), ("stock monthly C-C", "1m", Some(71.0)));
        assert!(l.t.is_some_and(|t| (t * 10.0).round() == 7.0));
        let l = line("stock monthly C-C", &acc, ("3m", 0, 3));
        assert_eq!(
            l.text,
            "  stock monthly C-C 3m  6 starts 2020-01-01..2020-04-01  net +7.1%/yr (gross +13.7)  per trade mean +0.58% median +0.50% win 67%  spaced 2 t n/a  halves +6.1|-26.5"
        );
        assert_eq!(l.t, None);
        let one: Acc = acc.iter().take(1).map(|(k, v)| (*k, *v)).collect();
        let l = line("bond ETF daily O-O", &one, ("1d", 1, 0));
        assert_eq!((l.text.as_str(), l.net), ("  bond ETF daily O-O 1d  1 starts  too few", None));
        assert_eq!(line("crypto daily C-C", &Acc::new(), ("1w", 7, 0)).text, "  crypto daily C-C 1w  0 starts  too few");
    }

    fn row(label: &str, hold: Hold, net: Option<f64>, halves: (f64, f64), t: Option<f64>) -> Line {
        Line { label: label.to_string(), hold, text: String::new(), net, halves, t }
    }

    #[test]
    fn verdict_names_the_best_and_the_best_short_with_every_miss() {
        let lines = [
            row("stock daily O-C", ("1d", 1, 0), Some(2.0), (1.0, 1.0), Some(3.5)),
            row("stock monthly C-C", ("1y", 0, 12), Some(8.0), (5.0, -1.0), None),
            row("stock monthly O-O", ("6m", 0, 6), None, (0.0, 0.0), None),
            row("stock daily C-C", ("1w", 7, 0), Some(1.0), (2.0, 3.0), Some(2.9)),
        ];
        assert_eq!(
            verdict("stock", &lines, 4.5),
            "  stock best: stock monthly C-C 1y  net +8.0%/yr: beats Série E 4.5, a half loses, t n/a\n  stock best short: stock daily O-C 1d  net +2.0%/yr: misses Série E 4.5, both halves net > 0, t 3.5 ≥ 3"
        );
        assert_eq!(
            verdict("stock", &lines[1..], 4.5),
            "  stock best: stock monthly C-C 1y  net +8.0%/yr: beats Série E 4.5, a half loses, t n/a\n  stock best short: stock daily C-C 1w  net +1.0%/yr: misses Série E 4.5, both halves net > 0, t 2.9 < 3"
        );
        assert_eq!(
            verdict("crypto", &lines[1..3], 9.0),
            "  crypto best: stock monthly C-C 1y  net +8.0%/yr: misses Série E 9.0, a half loses, t n/a\n  crypto best short: every hold to 6m too few"
        );
        assert_eq!(verdict("REIT fund", &lines[2..3], 4.5), "  REIT fund: every hold too few");
        assert_eq!(verdict("REIT fund", &[], 4.5), "  REIT fund: every hold too few");
    }

    #[test]
    fn class_of_splits_seven_classes_and_drops_leveraged_funds() {
        let q = |tk: &str, name: &str, kind: &str, sector: Option<&str>| {
            let mut q = Quote::stub(tk, "", "", name);
            q.instrument_type = kind.to_string();
            q.sector = sector.map(str::to_string);
            class_of(&q)
        };
        assert_eq!(q("BTC-EUR", "Bitcoin EUR", "CRYPTOCURRENCY", None), Some("crypto"));
        assert_eq!(q("AAPL", "Apple Inc.", "EQUITY", Some("Information Technology")), Some("stock"));
        assert_eq!(q("O", "Realty Income Corporation", "EQUITY", Some("Real Estate")), Some("REIT stock"));
        assert_eq!(q("IWDA.AS", "iShares Core MSCI World UCITS ETF", "ETF", None), Some("equity ETF"));
        assert_eq!(q("IBGL.AS", "iShares Euro Government Bond 15-30yr UCITS ETF", "ETF", None), Some("bond ETF"));
        assert_eq!(q("XAD5.DE", "Xtrackers IE Physical Gold ETC", "ETF", None), Some("gold/commodity ETF"));
        assert_eq!(q("IPRP.AS", "iShares European Property Yield UCITS ETF", "ETF", None), Some("REIT fund"));
        assert_eq!(q("NVD", "GraniteShares 2x Short NVD", "ETF", None), None);
        assert_eq!(q("IUSE.L", "iShares Core S&P 500 UCITS ETF EUR Hedged (Acc)", "ETF", None), Some("equity ETF"));
        // a stamped class wins over the name: the universe's ETF set made this one a fund
        let mut s = Quote::stub("XYZ", "", "", "Some Treasury Holdings");
        s.name_class = Some((true, None));
        assert_eq!(class_of(&s), Some("bond ETF"));
    }

    #[test]
    fn cost_is_per_class_then_per_currency() {
        assert_eq!([cost_bps("crypto", "EUR"), cost_bps("stock", "EUR"), cost_bps("stock", "USD"), cost_bps("bond ETF", "GBP")], [30.0, 20.0, 50.0, 50.0]);
    }
}
