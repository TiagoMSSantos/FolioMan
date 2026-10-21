//! (#809) THE INTRADAY HEURISTIC: which stocks will gain the most, in %, from today's open to today's
//! close. The trade it ranks for: buy at the open, sell before the close, hold nothing overnight.
//!
//! A key reads only COMPLETED sessions, the ones dated before the session it ranks for, so the same fn
//! is leak-free in the walk and on a live run at any hour (a bar dated today may still be trading).
//! - K1 tug of war: 21-day sum of ln(C/O) minus 21-day sum of ln(O/prev C). Names whose gains come in
//!   the day session rather than overnight (Lou, Polk & Skouras, "A tug of war", JFE 2019).
//! - K2 calm range: minus the 20-day mean of ln(H/L). Low intraday range names.
//! - K3 5d intraday: the 5-day mean of ln(C/O). Short-run intraday momentum.
//!
//! The pond is stocks with a 30-session average turnover (close × volume, in EUR) of at least
//! `MIN_TURNOVER_EUR`: the US side is the S&P 500 as it stood each day, the EU side the static
//! `EU_POND`. The book is the top `BOOK` by key, equal weight, net of a round-trip cost per market.
//! `backtest N intraday` grades every key on 10y of daily bars; nothing here reaches a lane, BUY NOW or
//! the 20y backtest.

use crate::core;
use crate::fetch::Chart;
use chrono::NaiveDate;
use std::collections::BTreeMap;

/// How many names each market's book holds.
pub const BOOK: usize = 10;
/// Round-trip cost per session, in basis points: spread plus fees, buy at the open and sell at the close.
pub const COST_BPS_US: f64 = 50.0;
pub const COST_BPS_EU: f64 = 20.0;
/// The liquidity floor, average daily turnover in EUR over the `WARMUP` sessions before the one ranked.
pub const MIN_TURNOVER_EUR: f64 = 5e6;
/// Sessions of history a name needs before it ranks: the turnover window, longer than every key's.
const WARMUP: usize = 30;
/// Key names, `KEYS[k - 1]` for key `k`.
pub const KEYS: [&str; 3] = ["K1 tug of war", "K2 calm range", "K3 5d intraday"];

/// The EU pond: DAX 40, CAC 40 and Euro Stoxx 50 members as of 2026, one listing per company.
/// shortcut: a static list of today's members, so the walk is survivor-biased on this side. A
/// point-in-time list per index would remove that.
pub const EU_POND: &[&str] = &[
    "ADS.DE", "ALV.DE", "BAS.DE", "BAYN.DE", "BEI.DE", "BMW.DE", "BNR.DE", "CBK.DE", "CON.DE", "1COV.DE",
    "DTG.DE", "DBK.DE", "DB1.DE", "DHL.DE", "DTE.DE", "EOAN.DE", "FRE.DE", "FME.DE", "G1A.DE", "G24.DE",
    "HNR1.DE", "HEI.DE", "HEN3.DE", "IFX.DE", "MBG.DE", "MRK.DE", "MTX.DE", "MUV2.DE", "P911.DE", "PAH3.DE",
    "QIA.DE", "RHM.DE", "RWE.DE", "SAP.DE", "SRT3.DE", "SIE.DE", "ENR.DE", "SHL.DE", "SY1.DE", "VOW3.DE",
    "VNA.DE", "ZAL.DE", "AC.PA", "AI.PA", "AIR.PA", "CS.PA", "BNP.PA", "EN.PA", "BVI.PA", "CAP.PA",
    "CA.PA", "ACA.PA", "BN.PA", "DSY.PA", "EDEN.PA", "ENGI.PA", "EL.PA", "ERF.PA", "RMS.PA", "KER.PA",
    "OR.PA", "LR.PA", "MC.PA", "ML.PA", "ORA.PA", "RI.PA", "PUB.PA", "RNO.PA", "SAF.PA", "SGO.PA",
    "SAN.PA", "SU.PA", "GLE.PA", "HO.PA", "TTE.PA", "URW.PA", "VIE.PA", "DG.PA", "VIV.PA", "TEP.PA",
    "STMPA.PA", "WLN.PA", "ASML.AS", "ADYEN.AS", "AD.AS", "INGA.AS", "PRX.AS", "WKL.AS", "PHIA.AS", "HEIA.AS",
    "MT.AS", "ABI.BR", "ARGX.BR", "IBE.MC", "SAN.MC", "BBVA.MC", "ITX.MC", "ENEL.MI", "ENI.MI", "ISP.MI",
    "UCG.MI", "RACE.MI", "STLAM.MI", "NOKIA.HE", "NDA-FI.HE",
];

/// One pond member: ticker, daily chart, EUR per unit of its currency, and its index spans (`None` =
/// a member every day).
pub type Member<'a> = (&'a str, &'a Chart, f64, Option<&'a [(NaiveDate, Option<NaiveDate>)]>);

/// One graded session: how many names ranked, and the gross mean open-to-close return of the book and
/// of every ranked name.
pub struct Day {
    pub date: NaiveDate,
    pub n: usize,
    pub book: f64,
    pub all: f64,
}

/// A price the arithmetic can use: finite and above zero.
fn pos(x: f64) -> Option<f64> {
    (x.is_finite() && x > 0.0).then_some(x)
}

/// ln(a / b), None unless both prices are usable.
fn ln(a: f64, b: f64) -> Option<f64> {
    Some((pos(a)? / pos(b)?).ln())
}

/// Key `k` for the session at bar `i`, from bars before `i` only. None for an unknown key, too little
/// history, or a missing print in the window.
pub fn key(k: u8, c: &Chart, i: usize) -> Option<f64> {
    let co = |j: usize| ln(c.closes[j], c.opens[j]);
    match k {
        1 if i >= 22 => (i - 21..i).map(|j| Some(co(j)? - ln(c.opens[j], c.closes[j - 1])?)).sum(),
        2 if i >= 20 => Some(-(i - 20..i).map(|j| ln(c.highs[j], c.lows[j])).sum::<Option<f64>>()? / 20.0),
        3 if i >= 5 => Some((i - 5..i).map(co).sum::<Option<f64>>()? / 5.0),
        _ => None,
    }
}

/// The realized open-to-close return of the session at bar `i`.
pub fn session_return(c: &Chart, i: usize) -> Option<f64> {
    Some(pos(c.closes[i])? / pos(c.opens[i])? - 1.0)
}

/// How many bars are completed sessions, dated before `today`. A bar dated today may still be trading.
pub fn completed(c: &Chart, today: NaiveDate) -> usize {
    c.dates.partition_point(|d| *d < today)
}

/// Whether the name clears the liquidity floor for the session at bar `i`.
fn liquid(c: &Chart, i: usize, fx: f64) -> bool {
    i >= WARMUP
        && core::avg_turnover(&c.closes[..i], &c.volumes[..i], WARMUP).is_some_and(|t| t * fx >= MIN_TURNOVER_EUR)
}

/// Rank order: key descending, ties by ticker ascending.
fn by_key<T>(v: &mut [(&str, f64, T)]) {
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Every completed session where at least `BOOK` names rank on key `k`, in date order.
pub fn walk(k: u8, pond: &[Member], today: NaiveDate) -> Vec<Day> {
    let mut by: BTreeMap<NaiveDate, Vec<(&str, f64, f64)>> = BTreeMap::new();
    for &(tk, c, fx, spans) in pond {
        for i in 0..completed(c, today) {
            if spans.is_some_and(|s| !core::sp500_member_at(s, c.dates[i])) || !liquid(c, i, fx) {
                continue;
            }
            if let (Some(score), Some(r)) = (key(k, c, i), session_return(c, i)) {
                by.entry(c.dates[i]).or_default().push((tk, score, r));
            }
        }
    }
    by.into_iter()
        .filter(|(_, v)| v.len() >= BOOK)
        .map(|(date, mut v)| {
            by_key(&mut v);
            let rs: Vec<f64> = v.iter().map(|x| x.2).collect();
            Day { date, n: v.len(), book: mean(&rs[..BOOK]), all: mean(&rs) }
        })
        .collect()
}

/// The US pond: every name in the index at any point since `from`, sorted.
pub fn us_pond(spans: &core::MemberSpans, from: NaiveDate) -> Vec<String> {
    let mut v: Vec<String> =
        spans.iter().filter(|(_, s)| s.iter().any(|(_, end)| end.is_none_or(|e| e > from))).map(|(t, _)| t.clone()).collect();
    v.sort();
    v
}

/// Mean, t (mean over its standard error) and the mean of each half by session count.
pub struct Stats {
    pub n: usize,
    pub mean: f64,
    pub t: f64,
    pub h1: f64,
    pub h2: f64,
}

pub fn stats(xs: &[f64]) -> Option<Stats> {
    let n = xs.len();
    if n < 2 {
        return None;
    }
    let m = mean(xs);
    let sd = (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt();
    let (a, b) = xs.split_at(n / 2);
    Some(Stats { n, mean: m, t: m / (sd / (n as f64).sqrt()), h1: mean(a), h2: mean(b) })
}

/// The pre-registered bar: net of cost the book earns, at t ≥ 3, in both halves, and more than the
/// whole pond did gross.
pub fn passes(net: &Stats, all: f64) -> bool {
    net.mean > 0.0 && net.t >= 3.0 && net.h1 > 0.0 && net.h2 > 0.0 && net.mean > all
}

/// One graded line per market and key, in basis points per session.
pub fn report(market: &str, k: u8, days: &[Day], cost_bps: f64) -> String {
    let name = KEYS[usize::from(k) - 1];
    let net: Vec<f64> = days.iter().map(|d| d.book - cost_bps / 1e4).collect();
    let Some(s) = stats(&net) else {
        return format!("  {market} {name:<15} no session with {BOOK}+ ranked names");
    };
    let all = mean(&days.iter().map(|d| d.all).collect::<Vec<_>>());
    let mut ns: Vec<usize> = days.iter().map(|d| d.n).collect();
    ns.sort_unstable();
    format!(
        "  {market} {name:<15} {} sessions {}..{} median {} names  book net {:+.1} bp/day (gross {:+.1})  t {:+.2}  halves {:+.1}|{:+.1}  all-names gross {:+.1}  -> {}",
        s.n,
        days[0].date,
        days[days.len() - 1].date,
        ns[ns.len() / 2],
        s.mean * 1e4,
        s.mean * 1e4 + cost_bps,
        s.t,
        s.h1 * 1e4,
        s.h2 * 1e4,
        all * 1e4,
        if passes(&s, all) { "PASS" } else { "FAIL" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(i: usize) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(i as i64)
    }

    /// A chart of `o.len()` daily bars from 2026-01-01; high and low bracket the open and close by 1%.
    fn chart(o: &[f64], c: &[f64], vol: f64) -> Chart {
        Chart {
            dates: (0..o.len()).map(d).collect(),
            closes: c.to_vec(),
            volumes: vec![vol; o.len()],
            opens: o.to_vec(),
            highs: o.iter().zip(c).map(|(a, b)| a.max(*b) * 1.01).collect(),
            lows: o.iter().zip(c).map(|(a, b)| a.min(*b) * 0.99).collect(),
            currency: "EUR".into(),
            name: String::new(),
            instrument_type: "EQUITY".into(),
            divs: vec![],
            splits: vec![],
        }
    }

    /// Bars that differ every session, so a window shifted by one bar reads a different sum.
    fn wavy(n: usize) -> Chart {
        let o: Vec<f64> = (0..n).map(|j| 100.0 + (j * j % 7) as f64).collect();
        let c: Vec<f64> = (0..n).map(|j| 101.0 + (j * 3 % 11) as f64).collect();
        chart(&o, &c, 1e5)
    }

    #[test]
    fn prices_must_be_finite_and_positive() {
        assert_eq!(pos(2.0), Some(2.0));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(pos(bad), None, "{bad}");
        }
        assert_eq!(ln(f64::NAN, 1.0), None);
        assert_eq!(ln(1.0, 0.0), None);
        assert!((ln(6.0, 2.0).unwrap() - 3f64.ln()).abs() < 1e-15);
    }

    /// Each key against a plain loop over its own window, at its first defined bar and further in.
    #[test]
    fn keys_read_only_the_window_before_the_session() {
        let c = wavy(40);
        let co = |j: usize| (c.closes[j] / c.opens[j]).ln();
        for i in [22, 30, 39] {
            let mut k1 = 0.0;
            for j in i - 21..i {
                k1 += co(j) - (c.opens[j] / c.closes[j - 1]).ln();
            }
            let k2 = -(i - 20..i).map(|j| (c.highs[j] / c.lows[j]).ln()).sum::<f64>() / 20.0;
            let k3 = (i - 5..i).map(co).sum::<f64>() / 5.0;
            assert!((key(1, &c, i).unwrap() - k1).abs() < 1e-12, "K1 at {i}");
            assert!((key(2, &c, i).unwrap() - k2).abs() < 1e-12, "K2 at {i}");
            assert!((key(3, &c, i).unwrap() - k3).abs() < 1e-12, "K3 at {i}");
        }
        assert!(key(2, &c, 30).unwrap() < 0.0, "a wide range ranks low");
        // the first bar each key can read, and one before it
        assert!(key(1, &c, 21).is_none() && key(2, &c, 19).is_none() && key(3, &c, 4).is_none());
        assert!(key(1, &c, 22).is_some() && key(2, &c, 20).is_some() && key(3, &c, 5).is_some());
        assert!(key(0, &c, 30).is_none() && key(4, &c, 30).is_none());
        // a missing print inside the window voids the key; one at the session itself does not
        let mut gap = wavy(40);
        gap.opens[25] = f64::NAN;
        gap.highs[25] = f64::NAN;
        assert!(key(1, &gap, 30).is_none() && key(2, &gap, 30).is_none() && key(3, &gap, 30).is_none());
        assert!(key(1, &gap, 25).is_some() && key(3, &gap, 25).is_some());
    }

    #[test]
    fn a_session_returns_close_over_open() {
        let c = chart(&[100.0, 50.0, 0.0], &[110.0, 45.0, 1.0], 1.0);
        assert!((session_return(&c, 0).unwrap() - 0.1).abs() < 1e-12);
        assert!((session_return(&c, 1).unwrap() + 0.1).abs() < 1e-12);
        assert_eq!(session_return(&c, 2), None);
    }

    #[test]
    fn only_bars_dated_before_today_are_completed() {
        let c = wavy(5);
        assert_eq!(completed(&c, d(3)), 3);
        assert_eq!(completed(&c, d(9)), 5);
        assert_eq!(completed(&c, d(0)), 0);
    }

    /// 30 sessions of history, and the floor met exactly, in EUR.
    #[test]
    fn the_floor_needs_a_full_window_and_reads_eur() {
        let c = chart(&[100.0; 31], &[100.0; 31], 25_000.0); // 2.5M a day in the listing's currency
        assert!(liquid(&c, 30, 2.0), "2.5M × 2.0 = 5M EUR meets the floor");
        assert!(!liquid(&c, 29, 2.0), "29 sessions is short of the window");
        assert!(!liquid(&c, 30, 1.9));
        assert!(!liquid(&c, 30, 1.0));
    }

    #[test]
    fn rank_order_is_key_then_ticker() {
        let mut v = [("B", 1.0, ()), ("A", 1.0, ()), ("C", 2.0, ()), ("D", -1.0, ())];
        by_key(&mut v);
        assert_eq!(v.iter().map(|x| x.0).collect::<Vec<_>>(), ["C", "A", "B", "D"]);
    }

    /// Name `n` of 12 gains `g` = 1 + n/1000 every session (its K3), and on the graded session
    /// returns its own n/100. The book is the top 10 by K3.
    #[test]
    fn the_walk_books_the_top_ten_of_the_members_that_clear_the_floor() {
        let names: Vec<String> = (0..12).map(|n| format!("N{n:02}")).collect();
        let charts: Vec<Chart> = (0..12)
            .map(|n| {
                let g = 1.0 + n as f64 / 1000.0;
                let mut c = [100.0 * g; 31];
                c[30] = 100.0 * (1.0 + n as f64 / 100.0);
                chart(&[100.0; 31], &c, 1e5)
            })
            .collect();
        let all_in: Vec<Member> = names.iter().zip(&charts).map(|(t, c)| (t.as_str(), c, 1.0, None)).collect();
        let days = walk(3, &all_in, d(31));
        assert_eq!(days.len(), 1, "bar 30 is the first with a full turnover window");
        let day = &days[0];
        assert_eq!((day.date, day.n), (d(30), 12));
        assert!((day.book - (2..12).sum::<usize>() as f64 / 1000.0).abs() < 1e-12, "N02..N11");
        assert!((day.all - (0..12).sum::<usize>() as f64 / 1200.0).abs() < 1e-12);
        assert!(walk(3, &all_in, d(30)).is_empty(), "bar 30 dated today is not completed");

        // a name out of the index that day, and a name under the floor, both drop out
        let out = [(d(0), Some(d(30)))];
        let open = [(d(30), None)];
        let thin = chart(&[100.0; 31], &[100.0; 31], 1.0);
        let mut pond = all_in.clone();
        pond[11].3 = Some(&out);
        pond[10].1 = &thin;
        let day = &walk(3, &pond, d(31))[0];
        assert_eq!(day.n, 10);
        assert!((day.book - (0..10).sum::<usize>() as f64 / 1000.0).abs() < 1e-12, "N00..N09");
        // a member that day stays in
        pond[11].3 = Some(&open);
        assert_eq!(walk(3, &pond, d(31))[0].n, 11);
        // nine names is no session
        pond[9].1 = &thin;
        pond[11].3 = Some(&out);
        assert!(walk(3, &pond, d(31)).is_empty());
    }

    #[test]
    fn the_us_pond_is_every_member_since_the_start() {
        let mut spans = core::MemberSpans::new();
        spans.insert("OLD".into(), vec![(d(0), Some(d(5)))]);
        spans.insert("LEFT".into(), vec![(d(0), Some(d(11)))]);
        spans.insert("EDGE".into(), vec![(d(0), Some(d(10)))]);
        spans.insert("NOW".into(), vec![(d(0), Some(d(3))), (d(20), None)]);
        assert_eq!(us_pond(&spans, d(10)), ["LEFT", "NOW"]);
    }

    #[test]
    fn stats_read_mean_t_and_halves() {
        let s = stats(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!((s.n, s.mean, s.h1, s.h2), (4, 2.5, 1.5, 3.5));
        assert!((s.t - 2.5 / ((5.0f64 / 3.0).sqrt() / 2.0)).abs() < 1e-12);
        let odd = stats(&[1.0, 2.0, 3.0]).unwrap();
        assert_eq!((odd.h1, odd.h2), (1.0, 2.5));
        assert!(stats(&[1.0, 2.0]).is_some());
        assert!(stats(&[1.0]).is_none());
    }

    #[test]
    fn the_bar_needs_every_clause() {
        let ok = Stats { n: 100, mean: 0.001, t: 3.0, h1: 0.001, h2: 0.001 };
        assert!(passes(&ok, 0.0));
        assert!(!passes(&ok, 0.001), "the book must beat the whole pond, not tie it");
        assert!(!passes(&Stats { mean: 0.0, ..ok }, -1.0));
        assert!(!passes(&Stats { t: 2.99, ..ok }, 0.0));
        assert!(!passes(&Stats { h1: 0.0, ..ok }, 0.0));
        assert!(!passes(&Stats { h2: 0.0, ..ok }, 0.0));
    }

    #[test]
    fn the_report_is_net_of_cost_in_bp() {
        let day = |i: usize, book: f64, n: usize| Day { date: d(i), n, book, all: 0.0 };
        let days = [day(0, 0.0060, 30), day(1, 0.0062, 10), day(2, 0.0058, 20), day(3, 0.0060, 40)];
        let line = report("US", 1, &days, 50.0);
        assert!(line.starts_with("  US K1 tug of war   4 sessions 2026-01-01..2026-01-04 median 30 names"), "{line}");
        assert!(line.contains("book net +10.0 bp/day (gross +60.0)") && line.contains("halves +11.0|+9.0"), "{line}");
        assert!(line.contains("t +12.25") && line.contains("all-names gross +0.0") && line.ends_with("-> PASS"), "{line}");
        assert!(report("EU", 3, &days, 70.0).ends_with("-> FAIL"));
        assert_eq!(report("EU", 2, &days[..1], 20.0), "  EU K2 calm range   no session with 10+ ranked names");
    }
}
