//! (#809) THE INTRADAY HEURISTIC: which stocks will gain the most, in %, from today's open to today's
//! close. The trade it ranks for: buy at the open, sell before the close, hold nothing overnight.
//!
//! A key reads only COMPLETED sessions, the ones dated before the session it ranks for, so the same fn
//! is leak-free in the walk and on a live run at any hour (a bar dated today may still be trading).
//! - K1 tug of war: 21-day sum of ln(C/O) minus 21-day sum of ln(O/prev C). Names whose gains come in
//!   the day session rather than overnight (Lou, Polk & Skouras, "A tug of war", JFE 2019).
//! - K2 calm range: minus the 20-day mean of ln(H/L). Low intraday range names.
//! - K3 5d intraday: the 5-day mean of ln(C/O). Short-run intraday momentum.
//! - K4 hit share: (#825) the share of the last 63 sessions that closed above their open.
//! - K5-K8 long memory: (#828) K1 and K4 over 252 and 504 sessions.
//! - K9 day Sharpe: (#829) mean over sd of the last 63 open-to-close moves.
//! - K10 low volume shock: (#836) minus the last session's volume over the mean of the 20 before it.
//!   Attention-driven buying lifts the open and reverses in the day (Berkman et al., JFQA 2012), so a
//!   quiet name ranks first. K11 is the same ratio with the sign flipped, the falsifier.
//! - K12 gain share 1y: (#1197) the last 252 sessions' up moves added up, over up plus down moves.
//!
//! The pond is stocks with a 30-session average turnover (close × volume, in EUR) of at least
//! `MIN_TURNOVER_EUR`: the US side is the S&P 500 as it stood each day, the EU side the static
//! `EU_POND`. The book is the top `BOOK` by key, equal weight, net of a round-trip cost per market.
//! `backtest N intraday` grades every key on 10y of daily bars; nothing here reaches a lane, BUY NOW or
//! the 20y backtest. (#811) With `intraday_key` set, `screen` prints today's top `BOOK` per market as the
//! page's Intraday table, a shadow, and journals it; `track` grades each journalled book on its session.
//! (#825) The table's RIGHT/FAIL column counted from a date the Pages workflow set; (#827) every session
//! of each book name since then, on its own bars. (#1196) Its GAIN/LOSS column added up the same sessions'
//! % moves, up days against down days, since a name can close up on most days and still lose.
//! (#1212) Two more columns sell the same sessions at a set hour, `intraday_exit_us` New York time and
//! `intraday_exit_eu` Frankfurt time, on each book name's 60m bars; `backtest N intraday` ends on the
//! hours census the two knobs are picked from, (#1214) every buy hour and later sell hour. Display only,
//! like the rest of the table. (#1215) The census then holds past the session: a buy on one session and a
//! sell up to `HOLDS` sessions on, at every hour pair on the 60m bars and open to close on the daily ones.
//! (#1216) Last, the same-session hour pairs split by calendar slice: weekday, month turn, pre-holiday, expiry.
//! (#1221) The page calls the table Trading: each row also prices the same sessions held 21 at a time, in on a
//! 200-close trend and bought once, names the best of the four, and the rows sort on it.
//! (#1223) The window is each name's last 2 years, the one (#1222) picks BEST on, priced in euros at each
//! day's rate; the key, RIGHT/FAIL and GAIN/LOSS fold into the key column's hover, one EARLY EXIT column
//! holds the row's own market's exit hour, and the BUY NOW stocks get a table of their own. The journal
//! keeps each row's BEST, and `track` grades it month by month against buying and holding the same name.

use crate::config::session;
use crate::core;
use crate::fetch::Chart;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Weekday};
use std::collections::{BTreeMap, HashMap};

/// How many names each market's book holds.
pub const BOOK: usize = 10;
/// Round-trip cost per session, in basis points: spread plus fees, buy at the open and sell at the close.
pub const COST_BPS_US: f64 = 50.0;
pub const COST_BPS_EU: f64 = 20.0;
/// The liquidity floor, average daily turnover in EUR over the `WARMUP` sessions before the one ranked.
pub const MIN_TURNOVER_EUR: f64 = 5e6;
/// Sessions of history a name needs before it ranks: the turnover window; a key with a longer window
/// waits for its own history.
const WARMUP: usize = 30;
/// Key names, `KEYS[k - 1]` for key `k`.
pub const KEYS: [&str; 12] = [
    "K1 tug of war", "K2 calm range", "K3 5d intraday", "K4 hit share", "K5 tug 1y", "K6 tug 2y", "K7 hit 1y", "K8 hit 2y", "K9 day Sharpe",
    "K10 low volume shock", "K11 high volume shock", "K12 gain share 1y",
];

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

/// (#1215) A name's full 60m sessions, each as its prices keyed by minutes on the exchange's clock.
pub type Prices = BTreeMap<NaiveDate, BTreeMap<u32, f64>>;

/// (#1215) The hold lengths the census grades: sessions between the buy session and the sell session.
pub const HOLDS: [usize; 7] = [0, 1, 2, 5, 10, 15, 21];

/// One graded session: how many names ranked, the gross mean open-to-close return of the book and of
/// every ranked name, and (#829) the share of each that closed above its open.
pub struct Day {
    pub date: NaiveDate,
    pub n: usize,
    pub book: f64,
    pub all: f64,
    pub hit: f64,
    pub all_hit: f64,
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
/// history, or a missing print in the window. (#828) K5-K8 are K1's tug and K4's hit share over 252 and
/// 504 sessions. (#829) K9 is None on a window with no spread, so a NaN never ranks first. (#836) K10 and
/// K11 are None when any of their 21 volume prints is missing or zero. (#1197) K12 is the share of the
/// last 252 sessions' open-to-close log moves, added up as distances, that the up ones make; None on a
/// window with no move.
/// shortcut: each key re-sums its whole window, so a 2y key costs 504 steps a bar in the walk; a running
/// sum would remove that.
pub fn key(k: u8, c: &Chart, i: usize) -> Option<f64> {
    let co = |j: usize| ln(c.closes[j], c.opens[j]);
    let tug = |n: usize| -> Option<f64> {
        (i > n).then_some(())?;
        (i - n..i).map(|j| Some(co(j)? - ln(c.opens[j], c.closes[j - 1])?)).sum()
    };
    let hit = |n: usize| -> Option<f64> {
        (i >= n).then_some(())?;
        Some((i - n..i).map(|j| Some(f64::from(u8::from(co(j)? > 0.0)))).sum::<Option<f64>>()? / n as f64)
    };
    match k {
        1 => tug(21),
        2 if i >= 20 => Some(-(i - 20..i).map(|j| ln(c.highs[j], c.lows[j])).sum::<Option<f64>>()? / 20.0),
        3 if i >= 5 => Some((i - 5..i).map(co).sum::<Option<f64>>()? / 5.0),
        // shortcut: 64 possible values, so ties are common and break on ticker; a second key as the
        // tie-break would remove that.
        4 => hit(63),
        5 => tug(252),
        6 => tug(504),
        7 => hit(252),
        8 => hit(504),
        9 if i >= 63 => stats(&(i - 63..i).map(co).collect::<Option<Vec<f64>>>()?).map(|s| s.t / 63f64.sqrt()).filter(|x| x.is_finite()),
        12 if i >= 252 => up_share(gains(&(i - 252..i).map(co).collect::<Option<Vec<f64>>>()?, 0.0)),
        10 | 11 if i >= 21 => {
            let shock = pos(c.volumes[i - 1])? / ((i - 21..i - 1).map(|j| pos(c.volumes[j])).sum::<Option<f64>>()? / 20.0);
            Some(if k == 10 { -shock } else { shock })
        }
        _ => None,
    }
}

/// (#1196) The moves above `cut` and the moves below it, each added up as its distance from `cut`: (up, down).
fn gains(xs: &[f64], cut: f64) -> (f64, f64) {
    xs.iter().fold((0.0, 0.0), |(u, d), x| (u + (x - cut).max(0.0), d + (cut - x).max(0.0)))
}

/// (#1196) up / (up + down), None when nothing moved.
fn up_share((u, d): (f64, f64)) -> Option<f64> {
    (u + d > 0.0).then_some(u / (u + d))
}

/// (#824) K1's two sums for the session at bar `i`, the page's KEY hover: the last 21 sessions'
/// open-to-close log moves and their overnight ones (each close to the next open). K1 = day − night;
/// [`key`] keeps its own sum, so the ranking stays bit-identical.
fn tug(c: &Chart, i: usize) -> Option<(f64, f64)> {
    (i >= 22).then_some(())?;
    (i - 21..i).try_fold((0.0, 0.0), |(day, night), j| Some((day + ln(c.closes[j], c.opens[j])?, night + ln(c.opens[j], c.closes[j - 1])?)))
}

/// The realized open-to-close return of the session at bar `i`.
pub fn session_return(c: &Chart, i: usize) -> Option<f64> {
    held_return(c, i, 0)
}

/// (#1215) Bought at bar `i`'s open and sold at the close of the bar `h` after it.
fn held_return(c: &Chart, i: usize, h: usize) -> Option<f64> {
    Some(pos(*c.closes.get(i + h)?)? / pos(c.opens[i])? - 1.0)
}

/// (#1222) [`held_return`] in euros: bought at bar `i`'s open at `fx[i]` euros a unit of the listing's
/// currency and sold at the close `h` bars on at `fx[i + h]`. A same-session trade's ratio is exactly 1.
fn held_eur(c: &Chart, fx: &[f64], i: usize, h: usize) -> Option<f64> {
    Some(pos(*c.closes.get(i + h)?)? / pos(c.opens[i])? * (fx[i + h] / fx[i]) - 1.0)
}

/// (#1221) The page's TREND trades on bars `s..e`: in on each bar whose previous close sits above the mean of
/// the 200 closes up to it, bought at the first such bar's open and sold at the open of the first bar out,
/// or at bar `e - 1`'s close when still in. One return a spell; a bar with fewer than 200 closes behind it is out.
/// shortcut: on a chart that starts under 200 sessions before `s` the first bars read out; the page's 5y
/// chart reaches far enough for its 2-year window, and a name listed under 3 years ago reads out at first.
/// (#1222) In euros at `fx`, as [`held_eur`].
fn trend(c: &Chart, fx: &[f64], s: usize, e: usize) -> Vec<f64> {
    let up = |j: usize| j.checked_sub(200).is_some_and(|a| c.closes[j - 1] > mean(&c.closes[a..j]));
    let ins: Vec<usize> = (s..e).filter(|&j| up(j)).collect();
    ins.chunk_by(|a, b| b - a == 1)
        .filter_map(|run| {
            let out = run[run.len() - 1] + 1;
            let (j, px) = if out < e { (out, c.opens[out]) } else { (e - 1, c.closes[e - 1]) };
            Some(pos(px)? / pos(c.opens[run[0]])? * (fx[j] / fx[run[0]]) - 1.0)
        })
        .collect()
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

/// Each session dated on or after `since` and before `until`, with every member that ranks on key `k`
/// that day as (ticker, key, open-to-close return), in rank order. (#1215) The return runs to the close
/// `h` sessions on, a bar that must be completed before `until` too.
fn sessions<'a>(k: u8, pond: &[Member<'a>], since: NaiveDate, until: NaiveDate, h: usize) -> BTreeMap<NaiveDate, Vec<(&'a str, f64, f64)>> {
    let mut by: BTreeMap<NaiveDate, Vec<(&str, f64, f64)>> = BTreeMap::new();
    for &(tk, c, fx, spans) in pond {
        for i in completed(c, since)..completed(c, until).saturating_sub(h) {
            if spans.is_some_and(|s| !core::sp500_member_at(s, c.dates[i])) || !liquid(c, i, fx) {
                continue;
            }
            if let (Some(score), Some(r)) = (key(k, c, i), held_return(c, i, h)) {
                by.entry(c.dates[i]).or_default().push((tk, score, r));
            }
        }
    }
    for v in by.values_mut() {
        by_key(v);
    }
    by
}

/// Every completed session where at least `BOOK` names rank on key `k`, in date order.
pub fn walk(k: u8, pond: &[Member], today: NaiveDate) -> Vec<Day> {
    walk_held(k, pond, today, 0)
}

/// (#1215) [`walk`] with each name held from the open to the close `h` sessions on.
pub fn walk_held(k: u8, pond: &[Member], today: NaiveDate, h: usize) -> Vec<Day> {
    sessions(k, pond, NaiveDate::MIN, today, h)
        .into_iter()
        .filter(|(_, v)| v.len() >= BOOK)
        .map(|(date, v)| {
            let rs: Vec<f64> = v.iter().map(|x| x.2).collect();
            let up = |xs: &[f64]| xs.iter().filter(|&&r| r > 0.0).count() as f64 / xs.len() as f64;
            Day { date, n: v.len(), book: mean(&rs[..BOOK]), all: mean(&rs), hit: up(&rs[..BOOK]), all_hit: up(&rs) }
        })
        .collect()
}

/// (#1215) The book [`walk`] holds on each session: its top `BOOK` tickers, in rank order.
pub fn books(k: u8, pond: &[Member], today: NaiveDate) -> BTreeMap<NaiveDate, Vec<String>> {
    sessions(k, pond, NaiveDate::MIN, today, 0)
        .into_iter()
        .filter(|(_, v)| v.len() >= BOOK)
        .map(|(d, v)| (d, v[..BOOK].iter().map(|x| x.0.to_string()).collect()))
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

/// Two graded lines per market and key: the book net of cost in basis points per session, then (#829)
/// its hit share, the share of the book that closed above its open minus the pond's share that session.
/// The hit bar: that difference is above zero at t ≥ 3, in both halves.
pub fn report(market: &str, k: u8, days: &[Day], cost_bps: f64) -> String {
    let name = KEYS[usize::from(k) - 1];
    let net: Vec<f64> = days.iter().map(|d| d.book - cost_bps / 1e4).collect();
    let edge: Vec<f64> = days.iter().map(|d| d.hit - d.all_hit).collect();
    let (Some(s), Some(h)) = (stats(&net), stats(&edge)) else {
        return format!("  {market} {name:<15} no session with {BOOK}+ ranked names");
    };
    let all = mean(&days.iter().map(|d| d.all).collect::<Vec<_>>());
    let mut ns: Vec<usize> = days.iter().map(|d| d.n).collect();
    ns.sort_unstable();
    format!(
        "  {market} {name:<15} {} sessions {}..{} median {} names  book net {:+.1} bp/day (gross {:+.1})  t {:+.2}  halves {:+.1}|{:+.1}  all-names gross {:+.1}  -> {}\n  {market} {name:<15} hit share book {:.1}% pond {:.1}%  diff {:+.2} pp  t {:+.2}  halves {:+.2}|{:+.2}  -> {}",
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
        if passes(&s, all) { "PASS" } else { "FAIL" },
        mean(&days.iter().map(|d| d.hit).collect::<Vec<_>>()) * 100.0,
        mean(&days.iter().map(|d| d.all_hit).collect::<Vec<_>>()) * 100.0,
        h.mean * 100.0,
        h.t,
        h.h1 * 100.0,
        h.h2 * 100.0,
        if passes(&h, 0.0) { "PASS" } else { "FAIL" }
    )
}

/// (#811) Today's book on key `k`: the members in the index today that clear the floor, ranked on the
/// sessions completed before `today`, top `BOOK` (fewer when fewer rank), each with its key.
/// shortcut: a halted name ranks on its last bars; the page's DATA TO cell prints how old they are.
pub fn rank<'a>(k: u8, pond: &[Member<'a>], today: NaiveDate) -> Vec<(&'a str, f64, &'a Chart)> {
    let mut v: Vec<(&str, f64, &Chart)> = pond
        .iter()
        .filter(|(_, c, fx, spans)| spans.is_none_or(|s| core::sp500_member_at(s, today)) && liquid(c, completed(c, today), *fx))
        .filter_map(|&(tk, c, _, _)| Some((tk, key(k, c, completed(c, today))?, c)))
        .collect();
    by_key(&mut v);
    v.truncate(BOOK);
    v
}

/// (#1223) One Trading row's inputs: its market (`US` or `EU`, which sets the session hours and the round
/// trip), the ticker shown, the ticker whose bars price it (a Xetra twin's US listing, #608, or the same
/// ticker), the daily chart, and the euros a unit of the chart's currency buys on each bar ([`rates_on`]).
pub type Row<'a> = (&'a str, &'a str, &'a str, &'a Chart, &'a [f64]);

/// (#1223) The first day of the Trading table's window for the sessions completed before `until`: 2 years
/// back, the window (#1222) picks BEST on.
pub fn window(until: NaiveDate) -> NaiveDate {
    until - chrono::Months::new(24)
}

/// The round trip on `market`, in basis points.
fn cost_of(market: &str) -> f64 {
    if market == "US" { COST_BPS_US } else { COST_BPS_EU }
}

/// (#1223) The [`WAYS`] index the page's BEST names for a row: [`pick`] over the sessions from [`window`]
/// to `until`, after `market`'s round trip. The journal keeps it for `track`.
pub fn best_way(market: &str, c: &Chart, fx: &[f64], until: NaiveDate) -> usize {
    pick(&trades(c, fx, completed(c, window(until)), completed(c, until)), cost_of(market))
}

/// (#811) The page rows for a Trading table. (#1223) Each row prices its name the four [`WAYS`] over the
/// sessions from [`window`] to the last one completed before `until`, with [`trades`] in euros at each
/// day's rate and [`euros`] after its market's round trip, `COST_BPS_US` or `COST_BPS_EU`: the rule (#1222)
/// graded. BEST names the most euros, the first of [`WAYS`] on a tie, and the rows sort on it, ties in
/// `book` order. Cells: rank, market (its session hours in the hover, and the US listing a twin is priced
/// on), ticker, name, the last close in euros, BEST, the four ways, key `k` on the sessions completed
/// before `today` (— when it does not read; its hover keeps the old KEY tip and the window's record, up
/// sessions against the rest and their moves added up), and the window's first and last bar.
pub fn web_rows(k: u8, book: &[Row], until: NaiveDate, today: NaiveDate) -> Vec<Vec<serde_json::Value>> {
    use serde_json::json;
    let since = window(until);
    let mut v: Vec<_> = book
        .iter()
        .map(|&(m, tk, series, c, fx)| {
            let (s, e) = (completed(c, since), completed(c, until));
            let ts = trades(c, fx, s, e);
            let mut best: Vec<(usize, f64)> = (0..4).map(|w| (w, euros(&ts[w], cost_of(m)))).collect();
            best.sort_by(|a, b| b.1.total_cmp(&a.1)); // a tie keeps `WAYS` order, as `pick` does
            ((m, tk, series, c, fx), (s, e), ts, best)
        })
        .collect();
    v.sort_by(|a, b| b.3[0].1.total_cmp(&a.3[0].1));
    let eur = |x: f64| format!("{}€{}", if x < 0.0 { "-" } else { "+" }, core::fmt_money2(x.abs()));
    v.into_iter()
        .enumerate()
        .map(|(n, ((m, tk, series, c, fx), (s, e), [day, holds, spells, bh], best))| {
            let (hours, cost) = if m == "US" { ("09:30-16:00 New York", COST_BPS_US) } else { ("09:00-17:30 Frankfurt", COST_BPS_EU) };
            let trips = |rs: &[f64]| {
                let (n, fee) = (rs.len(), core::fmt_money2(rs.len() as f64 * cost / 10.0));
                format!("{} before costs; the backtest's {cost:.0} bp a round trip costs {n} × €{} = €{fee}; {} after", eur(euros(rs, 0.0)), core::fmt_money2(cost / 10.0), eur(euros(rs, cost)))
            };
            let per = |x: f64| eur(x / day.len().max(1) as f64);
            let r = day.iter().filter(|&&s| s > 0.0).count();
            let (u, d) = gains(&day, 0.0);
            // shortcut: the euro stake earns the session's % change in the listing's currency, so a US row
            // leaves out the euro's move inside the session; counting it would need EUR/USD bars per session
            let pl = format!(
                "€1,000 bought at the open and sold at the close, each of the {} sessions since {since}: {}, about {} a session ({} before costs)\n(a fresh €1,000 each session, not compounded: a gain is not reinvested, a loss is topped back up; every session of this name, not only the days the table listed it; a US row leaves out the euro's move inside the session)",
                day.len(),
                trips(&day),
                per(euros(&day, cost)),
                per(euros(&day, 0.0))
            );
            let i = completed(c, today);
            let x = key(k, c, i);
            let tip = match (tug(c, i).map(|(d, n)| (d * 100.0, n * 100.0)), x) {
                (Some((d, n)), Some(x)) if k == 1 => format!(
                    "K1 tug of war, last 21 sessions:\nday moves (open to close) added up: {d:+.2}%\nnight moves (close to next open) added up: {n:+.2}%\nkey = day - night = {d:+.2} - ({n:+.2}) = {:+.2}%\nhigh = it rose while the exchange was open and fell while it was shut\n(each move is a log return, so they add exactly; a small move is about its % change)",
                    x * 100.0
                ),
                (_, Some(x)) if k == 7 => format!("K7 hit 1y, last 252 sessions:\n{:.0} closed above their open = {:.2}%\n(a session that closed at its open counts as not up)", x * 252.0, x * 100.0),
                _ => KEYS[usize::from(k) - 1].to_string(),
            };
            let record = format!(
                "{tip}\nthe same {} sessions since {since}: {r} closed above their open and {} at or below it, a score of {:.0}% = (up + 1) / (all + 2); the up days added up {:+.1}%, the down or flat days -{:.1}%, open to close, before costs",
                day.len(),
                day.len() - r,
                (r + 1) as f64 * 100.0 / (day.len() + 2) as f64,
                u * 100.0,
                d * 100.0
            );
            let twin = if series == tk { String::new() } else { format!("; priced on {series}, its US listing") };
            vec![
                json!(["RANK", (n + 1).to_string()]),
                json!(["MKT", m, null, format!("session {hours}{twin}")]),
                json!(["TICKER", tk]),
                json!(["NAME", c.name]),
                match (c.closes.last(), fx.last()) {
                    (Some(p), Some(r)) => json!(["PRICE", format!("€{}", core::fmt_money2(p * r))]),
                    _ => json!(["PRICE", "—"]),
                },
                json!(["BEST", format!("{} {}", WAYS[best[0].0], eur(best[0].1)), null, format!("The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\n{}\n(hindsight: every column counts sessions that are over; the rows sort on this column. Picked this way on each name's last 2 years, BEST then lost to buying and holding the same name over the next 6 months and over the next month, in the US and the EU, 2019-2026: receipt #1222)", best.iter().map(|&(w, x)| format!("{} {}", WAYS[w], eur(x))).collect::<Vec<_>>().join("\n"))]),
                match bh.first() {
                    Some(_) => json!(["BUY&HOLD", eur(euros(&bh, cost)), null, format!("€1,000 bought at the open on {} and sold at the close on {}, one round trip: {}", c.dates[s], c.dates[e - 1], trips(&bh))]),
                    None => json!(["BUY&HOLD", "—"]),
                },
                json!(["HOLD 21", eur(euros(&holds, cost)), null, format!("€1,000 bought at an open and sold at the close of the 21st session, then bought again at the next open, back to back over the same {} sessions; the last hold ends on the last one: {} holds, {}\n(a fresh €1,000 each hold, not compounded; hindsight, as SAME DAY is)", day.len(), holds.len(), trips(&holds))]),
                json!(["TREND", eur(euros(&spells, cost)), null, format!("In while the last close sits above the average of the last 200 closes: bought at the next open, sold at the next open after a close under it, or at the last close if still in, over the same {} sessions: {} round trips, {}\n(a fresh €1,000 each round trip, not compounded; 0 round trips = it stayed out and made nothing)", day.len(), spells.len(), trips(&spells))]),
                json!(["SAME DAY", eur(euros(&day, cost)), null, pl]),
                json!([format!("K{k}"), x.map_or("—".into(), |x| format!("{:+.2}%", x * 100.0)), null, record]),
                json!(["DATA TO", if e > s { format!("{} – {}", c.dates[s], c.dates[e - 1]) } else { "—".into() }]),
            ]
        })
        .collect()
}

/// (#1222) One pond member priced in euros: ticker, daily chart, the euros a unit of its currency buys on
/// each bar, and its index spans (`None` = a member every day).
pub type Priced<'a> = (&'a str, &'a Chart, &'a [f64], Option<&'a [(NaiveDate, Option<NaiveDate>)]>);

/// (#1222) The page's four ways to trade a name, in its order, so a tie keeps the first.
pub const WAYS: [&str; 4] = ["SAME DAY", "HOLD 21", "TREND", "BUY&HOLD"];

/// (#1222) [`rate_for`]'s twin for trades bought and sold on different days: EUR a unit of `currency` on each
/// of `dates`. 1 for EUR; for USD the latest of `usd` (EUR per USD by date) on or before the bar, or the
/// first rate for a bar before any; None for any other currency, or for USD with no rate at all.
pub fn rates_on(currency: &str, dates: &[NaiveDate], usd: &BTreeMap<NaiveDate, f64>) -> Option<Vec<f64>> {
    match currency {
        "EUR" => Some(vec![1.0; dates.len()]),
        "USD" => dates.iter().map(|d| usd.range(..=*d).next_back().or_else(|| usd.iter().next()).map(|x| *x.1)).collect(),
        _ => None,
    }
}

/// (#1222) The trades each of [`WAYS`] makes on bars `s..e`, each a return in euros at `fx`: every session
/// open to close, HOLD 21 back to back from the first open (the last one cut at bar `e - 1`), TREND's spells,
/// and BUY&HOLD's one trade from the first open to the last close (none on no bar).
fn trades(c: &Chart, fx: &[f64], s: usize, e: usize) -> [Vec<f64>; 4] {
    [
        (s..e).filter_map(|i| session_return(c, i)).collect(),
        (s..e).step_by(21).filter_map(|j| held_eur(c, fx, j, (e - 1 - j).min(20))).collect(),
        trend(c, fx, s, e),
        e.checked_sub(s + 1).and_then(|h| held_eur(c, fx, s, h)).into_iter().collect(),
    ]
}

/// (#1222) Euros after the round trip `cost` (bp) on each trade, €1,000 a trade, added up.
fn euros(rs: &[f64], cost: f64) -> f64 {
    rs.iter().map(|r| (r - cost / 1e4) * 1000.0).sum()
}

/// (#1222) The page's BEST on `ts`: the index into [`WAYS`] with the most euros after `cost`, the first on a tie.
fn pick(ts: &[Vec<f64>; 4], cost: f64) -> usize {
    let e = ts.each_ref().map(|rs| euros(rs, cost));
    (1..4).fold(0, |b, w| if e[w] > e[b] { w } else { b })
}

/// (#1222) One step of [`best_steps`]: its first day, how many names it graded, the mean over them of the
/// pick's euros minus holding's, holding's own mean, and how many names picked each of [`WAYS`].
pub struct Step {
    pub date: NaiveDate,
    pub n: usize,
    pub excess: f64,
    pub hold: f64,
    pub picks: [usize; 4],
}

/// (#1222) THE BEST WALK: whether the page's BEST, picked on a name's last 2y, beats holding the name over
/// the next step. A step starts every `months` from `start` and runs while it ends by `today`. Each member in
/// the index on its first day that clears the floor there, with 200 bars before its 2y so TREND reads every
/// one of them, picks on the 2y and trades the pick over the step's bars; it is graded against BUY&HOLD on
/// the same bars, both in euros after `cost`. A step with no graded name drops out. The means add up in
/// pond order, so the caller sorts it.
pub fn best_steps(pond: &[Priced], cost: f64, months: u32, start: NaiveDate, today: NaiveDate) -> Vec<Step> {
    let at = |k: u32| start.checked_add_months(chrono::Months::new(k * months));
    let mut out = Vec::new();
    for k in 0.. {
        let (Some(from), Some(to)) = (at(k), at(k + 1)) else { break };
        if to > today {
            break;
        }
        let back = from - chrono::Months::new(24);
        let mut step = Step { date: from, n: 0, excess: 0.0, hold: 0.0, picks: [0; 4] };
        for &(_, c, fx, spans) in pond {
            let (l, s, e) = (completed(c, back), completed(c, from), completed(c, to));
            if l < 200 || e <= s || spans.is_some_and(|sp| !core::sp500_member_at(sp, from)) || !liquid(c, s, fx[s - 1]) {
                continue;
            }
            let w = pick(&trades(c, fx, l, s), cost);
            let next = trades(c, fx, s, e);
            let hold = euros(&next[3], cost);
            step.n += 1;
            step.excess += euros(&next[w], cost) - hold;
            step.hold += hold;
            step.picks[w] += 1;
        }
        if step.n > 0 {
            step.excess /= step.n as f64;
            step.hold /= step.n as f64;
            out.push(step);
        }
    }
    out
}

/// (#1222) One [`best_steps`] line: the steps' mean of the pick minus holding, in euros a name a step, its t
/// and halves by step count, holding's own mean, and the share of picks each way won. The bar is
/// [`passes`]: above zero at t ≥ 3, in both halves.
pub fn best_report(market: &str, months: u32, steps: &[Step]) -> String {
    let xs: Vec<f64> = steps.iter().map(|s| s.excess).collect();
    let Some(s) = stats(&xs) else {
        return format!("  {market} {months}m steps: {} graded; the t needs 2", steps.len());
    };
    let mut ns: Vec<usize> = steps.iter().map(|s| s.n).collect();
    ns.sort_unstable();
    let all = ns.iter().sum::<usize>() as f64;
    let won: Vec<String> = (0..4).map(|w| format!("{} {:.0}%", WAYS[w], steps.iter().map(|s| s.picks[w]).sum::<usize>() as f64 * 100.0 / all)).collect();
    format!(
        "  {market} {months}m steps: {} from {} to {} median {} names  pick minus hold €{:+.2} a name a step  t {:+.2}  halves €{:+.2}|€{:+.2}  hold alone €{:+.2}  picked {}  -> {}",
        s.n,
        steps[0].date,
        steps[steps.len() - 1].date,
        ns[ns.len() / 2],
        s.mean,
        s.t,
        s.h1,
        s.h2,
        mean(&steps.iter().map(|s| s.hold).collect::<Vec<_>>()),
        won.join(", "),
        if passes(&s, 0.0) { "PASS" } else { "FAIL" }
    )
}

/// (#823) Whether `market`'s session dated today is over at `now`, UTC: half an hour past the close, 17:30
/// Frankfurt for EU and 16:00 New York for US, which clears the closing auction and a delayed chart.
/// (#1180) That is 17:00 UTC for EU and 21:30 UTC for US in winter time and an hour earlier in summer time.
pub fn closed(market: &str, now: NaiveDateTime) -> bool {
    let winter: (u32, u32) = if market == "US" { (21, 30) } else { (17, 0) };
    (now.hour(), now.minute()) >= (winter.0 - u32::from(summer_time(market, now)), winter.1)
}

/// (#1180) Whether `market`'s exchange runs on summer time at `now`, UTC. US: from the second Sunday of
/// March 07:00 UTC to the first Sunday of November 06:00 UTC. EU: from the last Sunday of March 01:00 UTC
/// to the last Sunday of October 01:00 UTC.
fn summer_time(market: &str, now: NaiveDateTime) -> bool {
    let sunday = |m, n| NaiveDate::from_weekday_of_month_opt(now.year(), m, Weekday::Sun, n);
    // the last Sunday of a month is the one a week before the next month's first
    let last = |m: u32| sunday(m + 1, 1).map(|d| d - Duration::days(7));
    let at = |d: Option<NaiveDate>, h| d.and_then(|d| d.and_hms_opt(h, 0, 0));
    let (from, to) = match market {
        "US" => (at(sunday(3, 2), 7), at(sunday(11, 1), 6)),
        _ => (at(last(3), 1), at(last(10), 1)),
    };
    matches!((from, to), (Some(from), Some(to)) if from <= now && now < to)
}

/// (#1046) EUR per unit of a chart's `currency`: 1 for EUR, `usd` (EUR per USD, None when the FX quote
/// failed) for USD, and None for anything else, which drops the name from the pond. Out of the skip-tagged
/// `screen` and `backtest` shells so a deleted arm is graded.
pub fn rate_for(currency: &str, usd: Option<f64>) -> Option<f64> {
    match currency {
        "EUR" => Some(1.0),
        "USD" => usd,
        _ => None,
    }
}

/// (#811) Each journalled book graded on the session it ranked for. For a line dated `D`, each name's
/// first bar dated on or after `D` and before `today` (a bar dated today may still be trading); the
/// book's mean open-to-close return, dated by the earliest of those bars. Two lines that reach the same
/// session (a weekend's) grade it once, from the first. A name with no such bar drops out. (#876) The second
/// value counts the graded sessions that did not grade the whole journalled book: a name dropped out, or
/// the names' first bars sit on different dates (a halt), so the mean is over a thinner or mixed book than
/// the line journalled.
pub fn graded(lines: &[(NaiveDate, Vec<&str>)], charts: &HashMap<String, Chart>, today: NaiveDate) -> (Vec<(NaiveDate, f64)>, usize) {
    let mut by = BTreeMap::new();
    for (day, book) in lines {
        let rs: Vec<(NaiveDate, f64)> = book
            .iter()
            .filter_map(|t| {
                let c = charts.get(*t)?;
                let i = completed(c, *day);
                let date = *c.dates.get(i).filter(|d| **d < today)?;
                Some((date, session_return(c, i)?))
            })
            .collect();
        if let Some(date) = rs.iter().map(|r| r.0).min() {
            let thin = rs.len() < book.len() || rs.iter().any(|r| r.0 != date);
            by.entry(date).or_insert_with(|| (mean(&rs.iter().map(|r| r.1).collect::<Vec<_>>()), thin));
        }
    }
    let thin = by.values().filter(|v| v.1).count();
    (by.into_iter().map(|(d, v)| (d, v.0)).collect(), thin)
}

/// (#811) One `track` line per market: the graded sessions net of the round trip, in basis points.
/// (#876) `thin` is [`graded`]'s count of sessions on a thinner or mixed book, said on the line.
pub fn track_line(market: &str, days: &[(NaiveDate, f64)], thin: usize, cost_bps: f64) -> String {
    let net: Vec<f64> = days.iter().map(|d| d.1 - cost_bps / 1e4).collect();
    let line = match stats(&net) {
        None => format!("  {market}: {} session(s) graded; the t needs 2", days.len()),
        Some(s) => format!(
            "  {market}: {} sessions {}..{}  book net {:+.1} bp/day (gross {:+.1})  t {:+.2}",
            s.n,
            days[0].0,
            days[days.len() - 1].0,
            s.mean * 1e4,
            s.mean * 1e4 + cost_bps,
            s.t
        ),
    };
    if thin > 0 {
        format!("{line}  ({thin} of {} sessions graded on fewer names than the book held, or on two dates)", days.len())
    } else {
        line
    }
}

/// (#1223) The journalled BEST graded forward, month by month. `lines` are the journal's dated `(ticker
/// priced, WAYS index)` lists for one table, in date order; `priced` holds each ticker's daily chart and
/// its euros a unit on each bar ([`rates_on`]). Each month's first line links to the next month's first:
/// every name trades its journalled way over the bars between the two dates and buys and holds over the
/// same bars, in euros after its round trip (50 bp on a USD chart, 20 bp on any other), and the link is the
/// mean over the names of the way's euros minus holding's, dated by its first line. A name with no chart or
/// no bar between drops out, a link with no name drops, and the month still open is not graded.
pub fn best_links(lines: &[(NaiveDate, Vec<(&str, u8)>)], priced: &HashMap<String, (Chart, Vec<f64>)>) -> Vec<(NaiveDate, f64)> {
    let mut firsts: BTreeMap<(i32, u32), &(NaiveDate, Vec<(&str, u8)>)> = BTreeMap::new();
    for l in lines {
        firsts.entry((l.0.year(), l.0.month())).or_insert(l);
    }
    let firsts: Vec<_> = firsts.into_values().collect();
    firsts
        .windows(2)
        .filter_map(|w| {
            let ((d0, names), d1) = (w[0], w[1].0);
            let xs: Vec<f64> = names
                .iter()
                .filter_map(|&(tk, way)| {
                    let (c, fx) = priced.get(tk)?;
                    let (s, e) = (completed(c, *d0), completed(c, d1));
                    if e <= s {
                        return None;
                    }
                    let cost = cost_of(if c.currency == "USD" { "US" } else { "EU" });
                    let ts = trades(c, fx, s, e);
                    Some(euros(ts.get(usize::from(way))?, cost) - euros(&ts[3], cost))
                })
                .collect();
            (!xs.is_empty()).then(|| (*d0, mean(&xs)))
        })
        .collect()
}

/// (#1223) One `track` line for a table's [`best_links`]: how many links and their span, the mean of BEST
/// minus buying and holding in euros a name a link, and its t, read only once 12 links are in.
pub fn best_track_line(table: &str, links: &[(NaiveDate, f64)]) -> String {
    let wait = if links.len() < 12 { "  (reads after 12 links)" } else { "" };
    let xs: Vec<f64> = links.iter().map(|l| l.1).collect();
    match stats(&xs) {
        None => format!("  BEST {table}: {} monthly link(s) graded; the t needs 2{wait}", links.len()),
        Some(s) => format!(
            "  BEST {table}: {} monthly links {}..{}  BEST minus BUY&HOLD €{:+.2} a name a link  t {:+.2}  halves €{:+.2}|€{:+.2}{wait}",
            s.n,
            links[0].0,
            links[links.len() - 1].0,
            s.mean,
            s.t,
            s.h1,
            s.h2
        ),
    }
}

/// (#1212) Minutes `market`'s exchange clock runs ahead of UTC at `utc`: New York -300 and Frankfurt +60,
/// an hour more on summer time.
fn offset(market: &str, utc: NaiveDateTime) -> i64 {
    let winter = if market == "US" { -300 } else { 60 };
    winter + 60 * i64::from(summer_time(market, utc))
}

/// (#1212) Each full session in 60m `bars` (start in UTC seconds, open, close): its move from the open to
/// the end of every bar, keyed by that end in minutes on the exchange's clock. A session needs a bar that
/// starts at the open and one that ends at the close, so a half day, or a session missing its first or
/// last bar, drops out. (#1214) The open itself is keyed too, at 0, so a buy at any key and a sell at a
/// later one is `(1 + sell) / (1 + buy) - 1`.
pub fn day_moves(market: &str, bars: &[(i64, f64, f64)]) -> BTreeMap<NaiveDate, BTreeMap<u32, f64>> {
    let open = session(market).0;
    day_prices(market, bars).into_iter().map(|(d, p)| (d, p.iter().map(|(&m, &x)| (m, x / p[&open] - 1.0)).collect())).collect()
}

/// (#1215) [`day_moves`]'s sessions as prices: the open's at the open, and each bar's close at its end.
pub fn day_prices(market: &str, bars: &[(i64, f64, f64)]) -> Prices {
    let (open, close) = session(market);
    let mut days: BTreeMap<NaiveDate, Vec<(u32, f64, f64)>> = BTreeMap::new();
    for &(t, o, c) in bars {
        let Some(utc) = chrono::DateTime::from_timestamp(t, 0).map(|d| d.naive_utc()) else { continue };
        let at = utc + Duration::minutes(offset(market, utc));
        let m = at.hour() * 60 + at.minute();
        if (open..close).contains(&m) {
            days.entry(at.date()).or_default().push((m, o, c));
        }
    }
    days.into_iter()
        .filter_map(|(d, b)| {
            let o = pos(b.iter().find(|x| x.0 == open)?.1)?;
            let ends: BTreeMap<u32, f64> = std::iter::once((open, o)).chain(b.iter().filter_map(|&(m, _, c)| Some(((m + 60).min(close), pos(c)?)))).collect();
            ends.contains_key(&close).then_some((d, ends))
        })
        .collect()
}

/// (#1212) An exit column's header, the hour on the exchange's clock and on Portugal's: `P/L 14:30 NY
/// (18:30 PT)`. Portugal keeps UTC in winter and UTC+1 in summer, on the EU's switch dates. Both clocks
/// are read at noon UTC on `today`, and no clock switches on a trading day. (#1214) It names the buy at the
/// open as well: `P/L 09:30-14:30 NY (13:30-18:30 PT)`.
pub fn exit_head(market: &str, exit: u32, today: NaiveDate) -> String {
    let noon = today.and_hms_opt(12, 0, 0).unwrap_or_default();
    let pt = |m: u32| (i64::from(m) - offset(market, noon) + 60 * i64::from(summer_time("EU", noon))).rem_euclid(1440);
    let hm = |m: i64| format!("{:02}:{:02}", m / 60, m % 60);
    let city = if market == "US" { "NY" } else { "FRA" };
    let open = session(market).0;
    format!("P/L {}-{} {city} ({}-{} PT)", hm(open.into()), hm(exit.into()), hm(pt(open)), hm(pt(exit)))
}

/// (#1212) The EARLY EXIT cell on each row, after SAME DAY. (#1223) It holds the row's own market's hour in
/// `exits`: the SAME DAY trades sold at that hour instead of at the close, in euros after the round trip,
/// over the sessions from [`window`] to `until` that the row's 60m bars in `hourly` carry in full. A name
/// with no such session, or a market missing from `exits`, reads —. The hover names the hour on both
/// clocks ([`exit_head`]) and puts the same sessions sold at the close beside it.
pub fn stamp_exits(rows: &mut [Vec<serde_json::Value>], hourly: &HashMap<String, Vec<(i64, f64, f64)>>, exits: &[(&str, u32)], until: NaiveDate, today: NaiveDate) {
    use serde_json::json;
    let eur = |x: f64| format!("{}€{}", if x < 0.0 { "-" } else { "+" }, core::fmt_money2(x.abs()));
    let since = window(until);
    for row in rows.iter_mut() {
        let (market, tk) = (row[1][1].as_str().unwrap_or("").to_string(), row[2][1].as_str().unwrap_or(""));
        let days = hourly.get(tk).map(|b| day_moves(&market, b)).unwrap_or_default();
        let cell = exits
            .iter()
            .find(|&&(m, _)| m == market)
            .and_then(|&(m, exit)| {
                let close = session(m).1;
                let xs: Vec<(f64, f64)> = days.iter().filter(|(d, _)| (since..until).contains(*d)).filter_map(|(_, e)| Some((*e.get(&exit)?, *e.get(&close)?))).collect();
                let n = xs.len();
                let cost = cost_of(m);
                let fee = n as f64 * cost / 10.0;
                let (gross, at_close) = (xs.iter().map(|x| x.0).sum::<f64>() * 1000.0, xs.iter().map(|x| x.1).sum::<f64>() * 1000.0);
                let tip = format!(
                    "{}: €1,000 bought at the open and sold at {:02}:{:02} {} time, each of the {n} sessions since {since} on this name's 60m bars: {} before costs; the backtest's {cost:.0} bp a round trip costs {n} × €{} = €{}; {} after, about {} a session\nsold at the close instead, the same {n} sessions: {} after costs, so this hour made {} against the close\n(hindsight, as SAME DAY is; a half day, or a session missing its first or last bar, drops out, so the count can sit under SAME DAY's)",
                    exit_head(m, exit, today),
                    exit / 60,
                    exit % 60,
                    if m == "US" { "New York" } else { "Frankfurt" },
                    eur(gross),
                    core::fmt_money2(cost / 10.0),
                    core::fmt_money2(fee),
                    eur(gross - fee),
                    eur((gross - fee) / n as f64),
                    eur(at_close - fee),
                    eur(gross - at_close)
                );
                (n > 0).then(|| json!(["EARLY EXIT", eur(gross - fee), null, tip]))
            })
            .unwrap_or_else(|| json!(["EARLY EXIT", "—"]));
        let at = row.iter().position(|c| c[0] == "SAME DAY").map_or(row.len(), |i| i + 1);
        row.insert(at, cell);
    }
}

/// (#1212) The `backtest N intraday` hours lines for one market. (#1214) On each session before `today` that
/// a name's 60m bars carry in full ([`day_moves`]), every window from a buy at the open or a bar end to a
/// sell at a later bar end: its mean move over every name with both bars, equal weight. Per window: gross bp
/// and its t, net € a session at €1,000 after the round trip, the halves, and the mean gain over buying the
/// same names at the open and selling them at the close that session, with its t. The best gross window
/// and the `knob` window, (buy, sell), are marked.
pub fn hours_report(market: &str, names: &[BTreeMap<NaiveDate, BTreeMap<u32, f64>>], today: NaiveDate, cost_bps: f64, knob: (u32, u32)) -> String {
    let (open, close) = session(market);
    let mut days: BTreeMap<NaiveDate, BTreeMap<(u32, u32), Vec<(f64, f64)>>> = BTreeMap::new();
    for (d, ends) in names.iter().flat_map(|n| n.range(..today)) {
        let Some(&day) = ends.get(&close) else { continue };
        for (&b, &x) in ends.range(..close) {
            for (&s, &y) in ends.range(b + 1..) {
                let r = (1.0 + y) / (1.0 + x) - 1.0;
                days.entry(*d).or_default().entry((b, s)).or_default().push((r, r - day));
            }
        }
    }
    let windows: std::collections::BTreeSet<(u32, u32)> = days.values().flat_map(|w| w.keys().copied()).collect();
    let rows: Vec<((u32, u32), Stats, Stats)> = windows
        .into_iter()
        .filter_map(|w| {
            let (s, v): (Vec<f64>, Vec<f64>) = days
                .values()
                .filter_map(|x| {
                    let (r, g): (Vec<f64>, Vec<f64>) = x.get(&w)?.iter().copied().unzip();
                    Some((mean(&r), mean(&g)))
                })
                .unzip();
            Some((w, stats(&s)?, stats(&v)?))
        })
        .collect();
    let (Some(best), Some((first, last))) = (
        rows.iter().max_by(|a, b| a.1.mean.total_cmp(&b.1.mean)).map(|r| r.0),
        days.keys().next().zip(days.keys().next_back()),
    ) else {
        return format!("  {market}: no two sessions with 60m bars");
    };
    let hm = |m: u32| format!("{:02}:{:02}", m / 60, m % 60);
    let mut out = format!("  {market}: {} names, {} sessions {first}..{last}; cost {cost_bps} bp a round trip", names.len(), days.len());
    for (w, s, v) in rows {
        let vs = if w == (open, close) { "open to close".to_string() } else { format!("vs open-close {:+.2} bp (t {:+.2})", v.mean * 1e4, v.t) };
        let mark = match (w == best, w == knob) {
            (true, true) => "  <- best, the knob",
            (true, false) => "  <- best",
            (false, true) => "  <- the knob",
            _ => "",
        };
        out += &format!(
            "\n  {market} {}-{}  {} sessions  gross {:+.2} bp (t {:+.2})  net {:+.2} €/session  halves {:+.2}|{:+.2}  {vs}{mark}",
            hm(w.0),
            hm(w.1),
            s.n,
            s.mean * 1e4,
            s.t,
            (s.mean * 1e4 - cost_bps) / 10.0,
            s.h1 * 1e4,
            s.h2 * 1e4
        );
    }
    out
}

/// (#1215) One graded hold length, `s` the gross return a hold and `h` the sessions after the buy session:
/// net € at €1,000 after one round trip, a hold and a session held, a same-session hold counting as one.
fn hold_line(label: &str, s: &Stats, cost_bps: f64, h: usize) -> String {
    let net = (s.mean * 1e4 - cost_bps) / 10.0;
    format!(
        "{label}  {} holds  gross {:+.2} bp (t {:+.2})  net {:+.2} € a hold, {:+.2} € a session  halves {:+.2}|{:+.2}",
        s.n,
        s.mean * 1e4,
        s.t,
        net,
        net / h.max(1) as f64,
        s.h1 * 1e4,
        s.h2 * 1e4
    )
}

/// (#1215) Every session before `today` that any name has prices on, in date order.
fn sessions_before(names: &[(&str, Prices)], today: NaiveDate) -> Vec<NaiveDate> {
    names.iter().flat_map(|n| n.1.range(..today).map(|x| *x.0)).collect::<std::collections::BTreeSet<_>>().into_iter().collect()
}

/// (#1215) Each pair of a buy key on session `a` and a sell key on session `b`, a later key when they are
/// the same session: the mean return over every name with both sessions, equal weight, and over the
/// `held` names, `None` when none of them has both.
fn pair_means(names: &[(&str, Prices)], held: Option<&Vec<String>>, a: NaiveDate, b: NaiveDate) -> BTreeMap<(u32, u32), (f64, Option<f64>)> {
    let mut day: BTreeMap<_, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for (tk, p) in names {
        let (Some(buy), Some(sell)) = (p.get(&a), p.get(&b)) else { continue };
        let in_book = held.is_some_and(|v| v.iter().any(|x| x == tk));
        for (&kb, &pb) in buy {
            for (&ks, &ps) in sell.range(if a == b { kb + 1 } else { 0 }..) {
                let e = day.entry((kb, ks)).or_default();
                e.0.push(ps / pb - 1.0);
                if in_book {
                    e.1.push(ps / pb - 1.0);
                }
            }
        }
    }
    day.into_iter().map(|(w, (all, bk))| (w, (mean(&all), (!bk.is_empty()).then(|| mean(&bk))))).collect()
}

/// (#1215) The `backtest N intraday` holds lines for one market, on each name's [`day_prices`] before
/// `today`. For each `h` in `holds`, a buy at any key of one session and a sell at any key `h` sessions on
/// (a later key when `h` is 0): its mean return over every name with both prices, equal weight, and over
/// the names in that session's `books` entry. A hold starts every `h` sessions (every session for 0), so
/// none overlaps the next. Per `h` and name set, the best gross pair and the open to close one. Before
/// them, the whole span: each name bought at the first session's open and sold at the last one's close.
/// shortcut: one phase per hold length; the other `h - 1` phases would use every start.
pub fn holds_report(market: &str, names: &[(&str, Prices)], books: &BTreeMap<NaiveDate, Vec<String>>, today: NaiveDate, cost_bps: f64, holds: &[usize]) -> String {
    let (open, close) = session(market);
    let cal = sessions_before(names, today);
    let &[first, .., last] = &cal[..] else {
        return format!("  {market}: no two sessions with 60m bars");
    };
    let hm = |m: u32| format!("{:02}:{:02}", m / 60, m % 60);
    let mut out = format!("  {market}: {} names, {} sessions {first}..{last}; cost {cost_bps} bp a round trip", names.len(), cal.len());
    let span: Vec<f64> = names.iter().filter_map(|(_, p)| Some(p.get(&last)?.get(&close)? / p.get(&first)?.get(&open)? - 1.0)).collect();
    out += &if span.is_empty() {
        format!("\n  {market} whole span: no name has both the first session and the last")
    } else {
        let gross = mean(&span) * 1e4;
        format!(
            "\n  {market} whole span: {} names bought at the first open and sold at the last close  gross {gross:+.1} bp  net {:+.2} € a session over {}",
            span.len(),
            (gross - cost_bps) / 10.0 / (cal.len() - 1) as f64,
            cal.len() - 1
        )
    };
    for &h in holds {
        let (mut pond, mut book): (BTreeMap<(u32, u32), Vec<f64>>, BTreeMap<(u32, u32), Vec<f64>>) = Default::default();
        for c in (0..cal.len().saturating_sub(h)).step_by(h.max(1)) {
            for (w, (all, bk)) in pair_means(names, books.get(&cal[c]), cal[c], cal[c + h]) {
                pond.entry(w).or_default().push(all);
                if let Some(b) = bk {
                    book.entry(w).or_default().push(b);
                }
            }
        }
        for (set, by) in [("pond", &pond), ("book", &book)] {
            let label = format!("  {market} {set} sell day +{h}");
            let rows: Vec<((u32, u32), Stats)> = by.iter().filter_map(|(w, xs)| Some((*w, stats(xs)?))).collect();
            out += &match (rows.iter().max_by(|a, b| a.1.mean.total_cmp(&b.1.mean)), rows.iter().find(|r| r.0 == (open, close))) {
                (Some(b), Some(o)) => format!(
                    "\n{}\n{}",
                    hold_line(&format!("{label}  best {}-{}", hm(b.0.0), hm(b.0.1)), &b.1, cost_bps, h),
                    hold_line(&format!("{label}  open-close"), &o.1, cost_bps, h)
                ),
                _ => format!("\n{label}  no two holds"),
            };
        }
    }
    out
}

/// (#1215) The same hold lengths on daily bars, the [`walk_held`] sessions every `h` sessions (every
/// session for 0), bought at the open and sold at the close `h` sessions on: the whole ranked pond equal
/// weight, then key `k`'s book.
/// shortcut: one phase per hold length, as in [`holds_report`].
pub fn daily_holds(market: &str, k: u8, pond: &[Member], today: NaiveDate, cost_bps: f64, holds: &[usize]) -> String {
    let mut out = Vec::new();
    for &h in holds {
        let days: Vec<Day> = walk_held(k, pond, today, h).into_iter().step_by(h.max(1)).collect();
        for (set, xs) in [("pond", days.iter().map(|d| d.all).collect::<Vec<f64>>()), ("book", days.iter().map(|d| d.book).collect())] {
            let label = format!("  {market} daily {set} sell day +{h} open-close");
            out.push(match stats(&xs) {
                Some(s) => hold_line(&label, &s, cost_bps, h),
                None => format!("{label}  no two holds"),
            });
        }
    }
    out.join("\n")
}

/// (#1216) The calendar slices the census splits sessions by, in report order.
pub const SLICES: [&str; 9] = ["Mon", "Tue", "Wed", "Thu", "Fri", "month turn", "rest of month", "pre-holiday", "expiry"];

/// (#1216) Each session's [`SLICES`] tags, in that order: its weekday; in a month of `n` sessions, the first
/// 4 and last 2 are the month turn and the rest the rest of the month; a session is pre-holiday when its
/// next weekday (the Monday after a Friday) is missing from `cal` but before its last session; a Friday
/// dated the 15th to the 21st is the expiry.
/// shortcut: `cal`'s first and last months may be partial, so their sessions carry no month tag, and an
/// expiry moved to the Thursday by a Friday holiday is missed. A holiday table would close both.
pub fn slices(cal: &[NaiveDate]) -> BTreeMap<NaiveDate, Vec<&'static str>> {
    let ym = |d: &NaiveDate| (d.year(), d.month());
    let edges = (cal.first().map(ym), cal.last().map(ym));
    cal.iter()
        .map(|&d| {
            let mut v = Vec::new();
            let wd = d.weekday().num_days_from_monday() as usize;
            if wd < 5 {
                v.push(SLICES[wd]);
            }
            if Some(ym(&d)) != edges.0 && Some(ym(&d)) != edges.1 {
                let month: Vec<&NaiveDate> = cal.iter().filter(|x| ym(x) == ym(&d)).collect();
                let j = month.partition_point(|x| **x < d);
                v.push(if j < 4 || j + 2 >= month.len() { "month turn" } else { "rest of month" });
            }
            let next = d + Duration::days(if d.weekday() == Weekday::Fri { 3 } else { 1 });
            if matches!(cal.binary_search(&next), Err(i) if i < cal.len()) {
                v.push("pre-holiday");
            }
            if d.weekday() == Weekday::Fri && (15..=21).contains(&d.day()) {
                v.push("expiry");
            }
            (d, v)
        })
        .collect()
}

/// (#1216) One graded slice, `s` the gross return a session: net € a session at €1,000 after the round trip.
fn session_line(label: &str, s: &Stats, cost_bps: f64) -> String {
    format!(
        "{label}  {} sessions  gross {:+.2} bp (t {:+.2})  net {:+.2} €/session  halves {:+.2}|{:+.2}",
        s.n,
        s.mean * 1e4,
        s.t,
        (s.mean * 1e4 - cost_bps) / 10.0,
        s.h1 * 1e4,
        s.h2 * 1e4
    )
}

/// (#1216) The calendar bar on a slice pair's gross `s`: above `cost_bps` at t ≥ 3, in both halves, and
/// `beat`, its slice beat the average day open to close on the daily bars.
fn clears(s: &Stats, cost_bps: f64, beat: bool) -> bool {
    let pays = |x: f64| x * 1e4 > cost_bps;
    pays(s.mean) && s.t >= 3.0 && pays(s.h1) && pays(s.h2) && beat
}

/// (#1216) Each slice on daily bars, the [`walk`] sessions bought at the open and sold at the close: the whole
/// ranked pond equal weight, then key `k`'s book, against every session's mean. With the lines, whether
/// each (slice, name set) beat that mean, the last part of [`calendar_report`]'s bar.
pub fn daily_slices(market: &str, k: u8, pond: &[Member], today: NaiveDate, cost_bps: f64) -> (String, BTreeMap<(&'static str, &'static str), bool>) {
    let days = walk(k, pond, today);
    let tags = slices(&days.iter().map(|d| d.date).collect::<Vec<_>>());
    let (mut out, mut beat) = (Vec::new(), BTreeMap::new());
    for (set, xs) in [("pond", days.iter().map(|d| d.all).collect::<Vec<f64>>()), ("book", days.iter().map(|d| d.book).collect())] {
        let every = mean(&xs);
        for sl in SLICES {
            let ys: Vec<f64> = days.iter().zip(&xs).filter(|(d, _)| tags[&d.date].contains(&sl)).map(|x| *x.1).collect();
            let label = format!("  {market} daily {set} {sl} open-close");
            let s = stats(&ys);
            beat.insert((sl, set), s.as_ref().is_some_and(|s| s.mean > every));
            out.push(match s {
                Some(s) => format!("{}  vs every session {:+.2} bp", session_line(&label, &s, cost_bps), (s.mean - every) * 1e4),
                None => format!("{label}  no two sessions"),
            });
        }
    }
    (out.join("\n"), beat)
}

/// (#1216) The `backtest N intraday` calendar lines for one market: every buy key and later sell key in the
/// same session on each name's [`day_prices`] before `today`, as in [`holds_report`], split by the
/// [`slices`] each session carries, over every name equal weight and over the session's `books` entry. A
/// pair is marked when it [`clears`] the bar, `daily` holding whether its slice beat the average day
/// ([`daily_slices`]), and each slice and name set's best gross pair is marked.
pub fn calendar_report(market: &str, names: &[(&str, Prices)], books: &BTreeMap<NaiveDate, Vec<String>>, today: NaiveDate, cost_bps: f64, daily: &BTreeMap<(&str, &str), bool>) -> String {
    let cal = sessions_before(names, today);
    let &[first, .., last] = &cal[..] else {
        return format!("  {market}: no two sessions with 60m bars");
    };
    let tags = slices(&cal);
    let mut by: BTreeMap<(&str, &str), BTreeMap<(u32, u32), Vec<f64>>> = BTreeMap::new();
    for d in &cal {
        for (w, (all, bk)) in pair_means(names, books.get(d), *d, *d) {
            for &sl in &tags[d] {
                by.entry((sl, "pond")).or_default().entry(w).or_default().push(all);
                if let Some(b) = bk {
                    by.entry((sl, "book")).or_default().entry(w).or_default().push(b);
                }
            }
        }
    }
    let hm = |m: u32| format!("{:02}:{:02}", m / 60, m % 60);
    let mut out = format!(
        "  {market}: {} names, {} sessions {first}..{last}; cost {cost_bps} bp a round trip; bar: net > 0, t ≥ 3, both halves net > 0, the slice beat the average day on 10y daily",
        names.len(),
        cal.len()
    );
    let (mut graded, mut pass) = (0, 0);
    for sl in SLICES {
        for set in ["pond", "book"] {
            let label = format!("  {market} {set} {sl}");
            let rows: Vec<((u32, u32), Stats)> = by.get(&(sl, set)).into_iter().flatten().filter_map(|(w, xs)| Some((*w, stats(xs)?))).collect();
            let Some(best) = rows.iter().max_by(|a, b| a.1.mean.total_cmp(&b.1.mean)).map(|r| r.0) else {
                out += &format!("\n{label}  no two sessions");
                continue;
            };
            for (w, s) in &rows {
                let ok = clears(s, cost_bps, daily.get(&(sl, set)) == Some(&true));
                graded += 1;
                pass += usize::from(ok);
                out += &format!(
                    "\n{}{}{}",
                    session_line(&format!("{label} {}-{}", hm(w.0), hm(w.1)), s, cost_bps),
                    if *w == best { "  <- best" } else { "" },
                    if ok { "  <- clears the bar" } else { "" }
                );
            }
        }
    }
    out + &format!("\n  {market}: {pass} of {graded} slice pairs clear the bar")
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
        assert!(key(0, &c, 30).is_none() && key(13, &c, 30).is_none());
        // (#825) K4 against a plain count; bar 22 is flat (open = close = 101), so it counts as a miss
        let c = wavy(70);
        assert_eq!((c.opens[22], c.closes[22]), (101.0, 101.0));
        for i in [63, 69] {
            let up = (i - 63..i).filter(|&j| c.closes[j] > c.opens[j]).count();
            assert_eq!(key(4, &c, i), Some(up as f64 / 63.0), "K4 at {i}");
        }
        assert!(key(4, &c, 62).is_none() && key(4, &c, 63).is_some());
        // (#829) K9 against a plain mean over sd; a window with no spread has no key
        for i in [63, 69] {
            let xs: Vec<f64> = (i - 63..i).map(|j| (c.closes[j] / c.opens[j]).ln()).collect();
            let m = xs.iter().sum::<f64>() / 63.0;
            let sd = (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / 62.0).sqrt();
            assert!((key(9, &c, i).unwrap() - m / sd).abs() < 1e-12, "K9 at {i}");
        }
        assert!(key(9, &c, 62).is_none() && key(9, &c, 63).is_some());
        assert!(key(9, &chart(&[100.0; 70], &[100.0; 70], 1e5), 69).is_none());
        // (#828) K5-K8: K1's tug and K4's count over a year and two, from their first bar on
        let c = wavy(510);
        let co = |j: usize| (c.closes[j] / c.opens[j]).ln();
        for (k, n, sums) in [(5, 252, true), (6, 504, true), (7, 252, false), (8, 504, false)] {
            let first = if sums { n + 1 } else { n };
            assert!(key(k, &c, first - 1).is_none(), "K{k} before {first}");
            for i in [first, 509] {
                let want = if sums {
                    (i - n..i).map(|j| co(j) - (c.opens[j] / c.closes[j - 1]).ln()).sum::<f64>()
                } else {
                    (i - n..i).filter(|&j| c.closes[j] > c.opens[j]).count() as f64 / n as f64
                };
                assert!((key(k, &c, i).unwrap() - want).abs() < 1e-12, "K{k} at {i}");
            }
        }
        // (#1197) K12 against plain sums: the window's up log moves over up plus down
        for i in [252, 509] {
            let up: f64 = (i - 252..i).map(co).filter(|x| *x > 0.0).sum();
            let down: f64 = (i - 252..i).map(co).filter(|x| *x < 0.0).map(f64::abs).sum();
            let k12 = key(12, &c, i).unwrap();
            assert!((k12 - up / (up + down)).abs() < 1e-12 && k12 > 0.0 && k12 < 1.0, "K12 at {i}");
        }
        assert!(key(12, &c, 251).is_none());
        assert!(key(12, &chart(&[100.0; 300], &[100.0; 300], 1e5), 299).is_none(), "nothing moved");
        let mut gap = c.clone();
        gap.closes[300] = f64::NAN;
        assert!(key(12, &gap, 300).is_some() && key(12, &gap, 301).is_none() && key(12, &gap, 509).is_none());
        // a missing print inside the window voids the key; one at the session itself does not
        let mut gap = wavy(40);
        gap.opens[25] = f64::NAN;
        gap.highs[25] = f64::NAN;
        assert!(key(1, &gap, 30).is_none() && key(2, &gap, 30).is_none() && key(3, &gap, 30).is_none());
        assert!(key(1, &gap, 25).is_some() && key(3, &gap, 25).is_some());
    }

    /// (#824) The day sum is the window's open-to-close moves, and day less night is K1 itself.
    #[test]
    fn tug_splits_k1_into_its_day_and_night_sums() {
        let c = wavy(40);
        for i in [22, 30, 39] {
            let (day, night) = tug(&c, i).unwrap();
            let want: f64 = (i - 21..i).map(|j| (c.closes[j] / c.opens[j]).ln()).sum();
            assert!((day - want).abs() < 1e-12 && (day - night - key(1, &c, i).unwrap()).abs() < 1e-12, "at {i}");
        }
        assert_eq!(tug(&c, 21), None);
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
        // (#829) N00 closes flat at its open, a miss
        assert_eq!((day.hit, day.all_hit), (1.0, 11.0 / 12.0));
        assert!(walk(3, &all_in, d(30)).is_empty(), "bar 30 dated today is not completed");
        // (#1215) the book that session, in rank order
        let top: Vec<String> = (2..12).rev().map(|n| format!("N{n:02}")).collect();
        assert_eq!(books(3, &all_in, d(31)), BTreeMap::from([(d(30), top)]));

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
        assert_eq!((day.hit, day.all_hit), (0.9, 0.9));
        // a member that day stays in
        pond[11].3 = Some(&open);
        assert_eq!(walk(3, &pond, d(31))[0].n, 11);
        // nine names is no session
        pond[9].1 = &thin;
        pond[11].3 = Some(&out);
        assert!(walk(3, &pond, d(31)).is_empty());
        assert!(books(3, &pond, d(31)).is_empty());
    }

    /// (#1215) Twelve names that open at 100 every session and close at 100 (1 + n f) on the last five.
    #[test]
    fn daily_holds_sell_h_sessions_on_and_start_a_hold_every_h_sessions() {
        let names: Vec<String> = (0..12).map(|n| format!("N{n:02}")).collect();
        let f = [0.01, 0.005, -0.0025, 0.004, -0.002];
        let charts: Vec<Chart> = (0..12)
            .map(|n| {
                let mut c = [100.0 * (1.0 + n as f64 / 1000.0); 35];
                for (j, x) in f.iter().enumerate() {
                    c[30 + j] = 100.0 * (1.0 + n as f64 * x);
                }
                chart(&[100.0; 35], &c, 1e5)
            })
            .collect();
        let pond: Vec<Member> = names.iter().zip(&charts).map(|(t, c)| (t.as_str(), c, 1.0, None)).collect();
        let held = walk_held(3, &pond, d(35), 1);
        assert_eq!(held.iter().map(|x| x.date).collect::<Vec<_>>(), [d(30), d(31), d(32), d(33)], "bar 34 has no close a session on");
        assert!((held[0].book - (2..12).sum::<usize>() as f64 * 0.005 / 10.0).abs() < 1e-12, "sold at bar 31's close");
        // a hold of two sessions starts on bars 30 and 32 and sells at 32 and 34; three sessions is one hold
        assert_eq!(
            daily_holds("US", 3, &pond, d(35), 50.0, &[0, 1, 2, 3]),
            "  US daily pond sell day +0 open-close  5 holds  gross +159.50 bp (t +1.24)  net +10.95 € a hold, +10.95 € a session  halves +412.50|-9.17\
             \n  US daily book sell day +0 open-close  5 holds  gross +188.50 bp (t +1.24)  net +13.85 € a hold, +13.85 € a session  halves +487.50|-10.83\
             \n  US daily pond sell day +1 open-close  4 holds  gross +61.88 bp (t +0.57)  net +1.19 € a hold, +1.19 € a session  halves +68.75|+55.00\
             \n  US daily book sell day +1 open-close  4 holds  gross +73.13 bp (t +0.57)  net +2.31 € a hold, +2.31 € a session  halves +81.25|+65.00\
             \n  US daily pond sell day +2 open-close  2 holds  gross -123.75 bp (t -9.00)  net -17.37 € a hold, -8.69 € a session  halves -137.50|-110.00\
             \n  US daily book sell day +2 open-close  2 holds  gross -146.25 bp (t -9.00)  net -19.62 € a hold, -9.81 € a session  halves -162.50|-130.00\
             \n  US daily pond sell day +3 open-close  no two holds\
             \n  US daily book sell day +3 open-close  no two holds"
        );
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
        let day = |i: usize, book: f64, n: usize, hit: f64| Day { date: d(i), n, book, all: 0.0, hit, all_hit: 0.5 };
        let days = [day(0, 0.0060, 30, 0.7), day(1, 0.0062, 10, 0.6), day(2, 0.0058, 20, 0.8), day(3, 0.0060, 40, 0.7)];
        let out = report("US", 1, &days, 50.0);
        let (line, hit) = out.split_once('\n').unwrap();
        assert!(line.starts_with("  US K1 tug of war   4 sessions 2026-01-01..2026-01-04 median 30 names"), "{line}");
        assert!(line.contains("book net +10.0 bp/day (gross +60.0)") && line.contains("halves +11.0|+9.0"), "{line}");
        assert!(line.contains("t +12.25") && line.contains("all-names gross +0.0") && line.ends_with("-> PASS"), "{line}");
        // (#829) the hit line: book minus pond share each session, in percentage points
        assert_eq!(hit, "  US K1 tug of war   hit share book 70.0% pond 50.0%  diff +20.00 pp  t +4.90  halves +15.00|+25.00  -> PASS");
        assert!(report("EU", 3, &days, 70.0).contains("-> FAIL\n"));
        let tied = [day(0, 0.0060, 30, 0.5), day(1, 0.0062, 10, 0.5), day(2, 0.0058, 20, 0.8), day(3, 0.0060, 40, 0.7)];
        let out = report("US", 1, &tied, 50.0);
        assert!(out.contains("-> PASS\n") && out.ends_with("-> FAIL"), "a half that only ties the pond fails: {out}");
        assert_eq!(report("EU", 2, &days[..1], 20.0), "  EU K2 calm range   no session with 10+ ranked names");
    }

    /// The walk's twelve names ranked live: K3 reads the bars before today, members today only.
    #[test]
    fn the_live_book_is_the_top_ten_members_today() {
        let names: Vec<String> = (0..12).map(|n| format!("N{n:02}")).collect();
        let charts: Vec<Chart> = (0..12)
            .map(|n| {
                let mut c = [100.0 * (1.0 + n as f64 / 1000.0); 31];
                c[30] = 100.0 * (1.0 + n as f64 / 100.0);
                chart(&[100.0; 31], &c, 1e5)
            })
            .collect();
        let (out, open, thin) = ([(d(0), Some(d(31)))], [(d(31), None)], chart(&[100.0; 31], &[100.0; 31], 1.0));
        let mut pond: Vec<Member> = names.iter().zip(&charts).map(|(t, c)| (t.as_str(), c, 1.0, None)).collect();
        let book = rank(3, &pond, d(31));
        let tickers = |b: &[(&str, f64, &Chart)]| b.iter().map(|x| x.0).collect::<Vec<_>>().join(" ");
        assert_eq!(tickers(&book), "N11 N10 N09 N08 N07 N06 N05 N04 N03 N02");
        assert!((book[0].1 - key(3, &charts[11], 31).unwrap()).abs() < 1e-15);
        assert_eq!(rank(3, &pond, d(30))[0].0, "N11", "bar 30 dated today is not read, and 30 bars still rank");
        assert!(rank(3, &pond, d(29)).is_empty(), "29 bars is short of the floor's window");
        // out of the index today, under the floor, or no key: dropped, and fewer than ten still rank
        pond[11].3 = Some(&out);
        pond[10].1 = &thin;
        assert_eq!(tickers(&rank(3, &pond, d(31))), "N09 N08 N07 N06 N05 N04 N03 N02 N01 N00");
        pond[11].3 = Some(&open);
        assert_eq!(rank(3, &pond, d(31))[0].0, "N11");
        assert_eq!(rank(1, &pond[..3], d(31)).len(), 3, "K1 reads 22 bars, so all three rank");
        assert!(rank(5, &pond, d(31)).is_empty());
    }

    #[test]
    fn a_page_row_carries_the_key_session_and_last_bar() {
        let mut c = wavy(31);
        c.name = "Acme".into();
        let mut up = chart(&[100.0; 31], &[101.0; 31], 1e5);
        up.name = "Up".into();
        let (two, one) = (vec![2.0; 31], vec![1.0; 31]);
        // (#1223) the window starts 24 months before `until`, so 2028-01-02 starts it on bar 1 (2026-01-02) and
        // bars 1 to 30 count. (#827) ACME's 22 up in 30 (its four flat bars, 4, 12, 22 and 23, are fails); B
        // never fell; (#1221) ACME's BEST, SAME DAY +€1,046.85, tops B's +€150.00; HOLD 21 is bars 1-21 then
        // 22-30, and 31 bars leave TREND no 200 closes; K3 reads the 5 sessions before `today`
        let until = NaiveDate::from_ymd_opt(2028, 1, 2).unwrap();
        let mut rows = web_rows(3, &[("US", "ACME", "ACME", &c, &two), ("US", "B", "B", &up, &two)], until, d(31));
        stamp_exits(&mut rows, &HashMap::new(), &[("US", 870)], until, d(31));
        let best = "(hindsight: every column counts sessions that are over; the rows sort on this column. Picked this way on each name's last 2 years, BEST then lost to buying and holding the same name over the next 6 months and over the next month, in the US and the EU, 2019-2026: receipt #1222)";
        let fresh = "(a fresh €1,000 each session, not compounded: a gain is not reinvested, a loss is topped back up; every session of this name, not only the days the table listed it; a US row leaves out the euro's move inside the session)";
        let trend = "In while the last close sits above the average of the last 200 closes: bought at the next open, sold at the next open after a close under it, or at the last close if still in, over the same 30 sessions: 0 round trips, +€0.00 before costs; the backtest's 50 bp a round trip costs 0 × €5.00 = €0.00; +€0.00 after\n(a fresh €1,000 each round trip, not compounded; 0 round trips = it stayed out and made nothing)";
        assert_eq!(
            serde_json::json!(rows),
            serde_json::json!([
                [["RANK", "1"], ["MKT", "US", null, "session 09:30-16:00 New York"], ["TICKER", "ACME"], ["NAME", "Acme"], ["PRICE", "€206.00"], ["BEST", "SAME DAY +€1,046.85", null, format!("The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\nSAME DAY +€1,046.85\nHOLD 21 +€89.01\nBUY&HOLD +€14.80\nTREND +€0.00\n{best}")], ["BUY&HOLD", "+€14.80", null, "€1,000 bought at the open on 2026-01-02 and sold at the close on 2026-01-31, one round trip: +€19.80 before costs; the backtest's 50 bp a round trip costs 1 × €5.00 = €5.00; +€14.80 after"], ["HOLD 21", "+€89.01", null, "€1,000 bought at an open and sold at the close of the 21st session, then bought again at the next open, back to back over the same 30 sessions; the last hold ends on the last one: 2 holds, +€99.01 before costs; the backtest's 50 bp a round trip costs 2 × €5.00 = €10.00; +€89.01 after\n(a fresh €1,000 each hold, not compounded; hindsight, as SAME DAY is)"], ["TREND", "+€0.00", null, trend], ["SAME DAY", "+€1,046.85", null, format!("€1,000 bought at the open and sold at the close, each of the 30 sessions since 2026-01-02: +€1,196.85 before costs; the backtest's 50 bp a round trip costs 30 × €5.00 = €150.00; +€1,046.85 after, about +€34.89 a session (+€39.89 before costs)\n{fresh}")], ["EARLY EXIT", "—"], ["K3", "+3.62%", null, "K3 5d intraday\nthe same 30 sessions since 2026-01-02: 22 closed above their open and 8 at or below it, a score of 72% = (up + 1) / (all + 2); the up days added up +124.5%, the down or flat days -4.8%, open to close, before costs"], ["DATA TO", "2026-01-02 – 2026-01-31"]],
                [["RANK", "2"], ["MKT", "US", null, "session 09:30-16:00 New York"], ["TICKER", "B"], ["NAME", "Up"], ["PRICE", "€202.00"], ["BEST", "SAME DAY +€150.00", null, format!("The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\nSAME DAY +€150.00\nHOLD 21 +€10.00\nBUY&HOLD +€5.00\nTREND +€0.00\n{best}")], ["BUY&HOLD", "+€5.00", null, "€1,000 bought at the open on 2026-01-02 and sold at the close on 2026-01-31, one round trip: +€10.00 before costs; the backtest's 50 bp a round trip costs 1 × €5.00 = €5.00; +€5.00 after"], ["HOLD 21", "+€10.00", null, "€1,000 bought at an open and sold at the close of the 21st session, then bought again at the next open, back to back over the same 30 sessions; the last hold ends on the last one: 2 holds, +€20.00 before costs; the backtest's 50 bp a round trip costs 2 × €5.00 = €10.00; +€10.00 after\n(a fresh €1,000 each hold, not compounded; hindsight, as SAME DAY is)"], ["TREND", "+€0.00", null, trend], ["SAME DAY", "+€150.00", null, format!("€1,000 bought at the open and sold at the close, each of the 30 sessions since 2026-01-02: +€300.00 before costs; the backtest's 50 bp a round trip costs 30 × €5.00 = €150.00; +€150.00 after, about +€5.00 a session (+€10.00 before costs)\n{fresh}")], ["EARLY EXIT", "—"], ["K3", "+1.00%", null, "K3 5d intraday\nthe same 30 sessions since 2026-01-02: 30 closed above their open and 0 at or below it, a score of 97% = (up + 1) / (all + 2); the up days added up +30.0%, the down or flat days -0.0%, open to close, before costs"], ["DATA TO", "2026-01-02 – 2026-01-31"]],
            ])
        );
        // equal BEST keeps book order
        let order: Vec<_> = web_rows(3, &[("EU", "Y", "Y", &c, &one), ("EU", "X", "X", &c, &one)], d(31), d(31)).iter().map(|r| r[2][1].as_str().unwrap().to_string()).collect();
        assert_eq!(order, ["Y", "X"]);
        // too few sessions for K1: it reads — and its hover names the key; (#826) the session dated today
        // closed, so the window ends on it, one bar past the key's last; bar 21, dated `until`, does not count
        let eu = web_rows(1, &[("EU", "X", "X", &c, &one)], d(21), d(20));
        assert_eq!(eu[0][1], serde_json::json!(["MKT", "EU", null, "session 09:00-17:30 Frankfurt"]));
        assert_eq!((eu[0][10][0].as_str(), eu[0][10][1].as_str(), eu[0][11][1].as_str()), (Some("K1"), Some("—"), Some("2026-01-01 – 2026-01-21")));
        assert!(eu[0][10][3].as_str().unwrap().starts_with("K1 tug of war\nthe same 21 sessions since 2024-01-22: 17 closed above their open and 4 at or below it, a score of 78% "));
        // (#824) 21 sessions behind it: the hover works the number out, then the window's record
        let k1 = web_rows(1, &[("EU", "X", "X", &c, &one)], d(31), d(31));
        assert_eq!((k1[0][4][1].as_str(), k1[0][10][1].as_str()), (Some("€103.00"), Some("+162.87%")));
        assert_eq!(k1[0][10][3], "K1 tug of war, last 21 sessions:\nday moves (open to close) added up: +80.00%\nnight moves (close to next open) added up: -82.87%\nkey = day - night = +80.00 - (-82.87) = +162.87%\nhigh = it rose while the exchange was open and fell while it was shut\n(each move is a log return, so they add exactly; a small move is about its % change)\nthe same 31 sessions since 2024-02-01: 23 closed above their open and 8 at or below it, a score of 73% = (up + 1) / (all + 2); the up days added up +125.5%, the down or flat days -4.8%, open to close, before costs");
        // (#832) K7's hover counts the up sessions behind its share
        let half = chart(&[100.0; 252], &(0..252).map(|j| if j % 2 == 0 { 101.0 } else { 99.0 }).collect::<Vec<_>>(), 1e5);
        let mut k7 = web_rows(7, &[("EU", "X", "X", &half, &[1.0; 252])], d(252), d(252));
        assert_eq!(k7[0][10][1], "+50.00%");
        assert!(k7[0][10][3].as_str().unwrap().starts_with("K7 hit 1y, last 252 sessions:\n126 closed above their open = 50.00%\n(a session that closed at its open counts as not up)\nthe same 252 sessions since 2024-09-10: 126 closed above "));
        // (#1223) a Xetra twin keeps its own ticker and names the US listing it is priced on
        let twin = web_rows(3, &[("US", "APC.DE", "AAPL", &c, &one)], d(31), d(31));
        assert_eq!((twin[0][1][3].as_str(), twin[0][2][1].as_str()), (Some("session 09:30-16:00 New York; priced on AAPL, its US listing"), Some("APC.DE")));
        // (#452) the page's hover text keys on exactly these headers, the live table's key K7
        stamp_exits(&mut k7, &HashMap::new(), &[], d(252), d(252));
        let heads: std::collections::BTreeSet<&str> = k7[0].iter().map(|c| c[0].as_str().unwrap()).collect();
        assert_eq!(crate::picks::web_help(false, &[])["intraday"].keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>(), heads);
    }

    /// (#1196) The window's record adds up the moves, not the day count: a name up on two days in three by a
    /// little reads worse than one up on one day in three by a lot. (#1221) The rows sort on BEST: GAP falls
    /// every session but gaps up every night, so it ranks first on BUY&HOLD with no up day; the three losers
    /// tie on TREND's +€0.00, out on 31 bars, and keep book order.
    #[test]
    fn rows_sort_on_the_best_way_to_trade_not_on_gains_or_the_day_count() {
        let o = [100.0; 31];
        // until 2028-01-02 starts the window on 2026-01-02: bar 0 sits before it, bars 1-30 count
        let small = chart(&o, &[[100.1; 21].as_slice(), &[99.0; 10]].concat(), 1e5);
        let big = chart(&o, &[[103.0; 11].as_slice(), &[99.5; 20]].concat(), 1e5);
        let flat = chart(&o, &o, 1e5);
        let down = chart(&o, &[99.0; 31], 1e5);
        let up: Vec<f64> = (0..31).map(|j| 100.0 + j as f64).collect();
        let gap = chart(&up, &up.iter().map(|x| x - 0.5).collect::<Vec<_>>(), 1e5);
        let one = [1.0; 31];
        let book = [("SMALL", &small), ("FLAT", &flat), ("DOWN", &down), ("BIG", &big), ("GAP", &gap)].map(|(tk, c)| ("EU", tk, tk, c, one.as_slice()));
        let rows = web_rows(3, &book, NaiveDate::from_ymd_opt(2028, 1, 2).unwrap(), d(31));
        let cells: Vec<_> = rows.iter().map(|r| [2, 5, 6, 7, 8, 9].map(|i| r[i][1].as_str().unwrap())).collect();
        assert_eq!(
            cells,
            [
                ["GAP", "BUY&HOLD +€280.18", "+€280.18", "+€250.54", "+€0.00", "-€190.61"],
                ["BIG", "SAME DAY +€140.00", "-€7.00", "-€14.00", "+€0.00", "+€140.00"],
                ["SMALL", "TREND +€0.00", "-€12.00", "-€24.00", "+€0.00", "-€140.00"],
                ["FLAT", "TREND +€0.00", "-€2.00", "-€4.00", "+€0.00", "-€60.00"],
                ["DOWN", "TREND +€0.00", "-€12.00", "-€24.00", "+€0.00", "-€360.00"],
            ]
        );
        // BIG's 10 up days add up to +30.0%, its 20 down days to -10.0%
        assert!(rows[1][10][3].as_str().unwrap().ends_with("10 closed above their open and 20 at or below it, a score of 34% = (up + 1) / (all + 2); the up days added up +30.0%, the down or flat days -10.0%, open to close, before costs"));
        // (#1206) €1,000 a session: FLAT never moved, so it reads +€0.00 before costs and pays 30 × €2.00
        assert!(rows[3][9][3].as_str().unwrap().contains(": +€0.00 before costs; the backtest's 20 bp a round trip costs 30 × €2.00 = €60.00; -€60.00 after, about -€2.00 a session (+€0.00 before costs)"));
        // (#1208) BIG's 30 sessions average its +€140.00 and +€200.00
        assert!(rows[1][9][3].as_str().unwrap().contains("+€140.00 after, about +€4.67 a session (+€6.67 before costs)"));
        // (#1221) GAP's one round trip runs from its first open to its last close, and BEST lists all four
        assert_eq!(rows[0][6][3], "€1,000 bought at the open on 2026-01-02 and sold at the close on 2026-01-31, one round trip: +€282.18 before costs; the backtest's 20 bp a round trip costs 1 × €2.00 = €2.00; +€280.18 after");
        assert!(rows[0][5][3].as_str().unwrap().starts_with("The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\nBUY&HOLD +€280.18\nHOLD 21 +€250.54\nTREND +€0.00\nSAME DAY -€190.61\n(hindsight"));
        // a window of one session, bar 0, holds from its open to its close; none leaves BUY&HOLD blank
        let gap1 = [("EU", "GAP", "GAP", &gap, one.as_slice())];
        assert_eq!(web_rows(3, &gap1, d(1), d(31))[0][6][1], "-€7.00");
        let none = web_rows(3, &gap1, d(0), d(31));
        assert_eq!((&none[0][6], &none[0][11]), (&serde_json::json!(["BUY&HOLD", "—"]), &serde_json::json!(["DATA TO", "—"])));
    }

    /// (#1223) The window runs 2 years back from `until`; a leap day steps back to the last day of February.
    #[test]
    fn the_window_is_the_last_two_years() {
        let on = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        assert_eq!((window(on(2026, 10, 29)), window(on(2028, 2, 29))), (on(2024, 10, 29), on(2026, 2, 28)));
    }

    /// (#1223) BEST pays its market's round trip: 30 sessions up 0.3% each keep 0.1% on the EU's 20 bp, so SAME
    /// DAY wins, and lose 0.2% on the US's 50 bp, so TREND's no trade wins.
    #[test]
    fn best_way_pays_the_markets_round_trip() {
        let c = chart(&[100.0; 31], &[100.3; 31], 1e5);
        let until = NaiveDate::from_ymd_opt(2028, 1, 2).unwrap();
        assert_eq!((best_way("EU", &c, &[1.0; 31], until), best_way("US", &c, &[1.0; 31], until)), (0, 2));
    }

    /// (#1221) TREND reads the close before each bar against the 200 closes up to it: out on bar 199 (199
    /// closes behind it), in on bars 200 and 201, out on bar 202 after bar 201's close fell under, back in from
    /// bar 203 and still in at the end, so the last spell sells at the last close.
    #[test]
    fn trend_spells_trade_the_next_open_after_the_close_crosses_its_200_mean() {
        let o: Vec<f64> = (0..206).map(|j| 100.0 + j as f64).collect();
        let c = [[100.0; 199].as_slice(), &[120.0, 120.0, 50.0, 120.0, 120.0, 120.0, 120.0]].concat();
        let ch = chart(&o, &c, 1e5);
        let one = [1.0; 206];
        assert_eq!(trend(&ch, &one, 199, 206), [o[202] / o[200] - 1.0, c[205] / o[203] - 1.0]);
        assert_eq!(trend(&ch, &one, 199, 202), [c[201] / o[200] - 1.0], "a window that ends in a spell sells at its last close");
        assert!(trend(&chart(&o, &[100.0; 206], 1e5), &one, 199, 206).is_empty(), "a close level with its mean is out");
        // (#1222) in euros: each spell at the rate on its buy and on its sell
        let fx: Vec<f64> = (0..206).map(|j| 1.0 + j as f64 / 1000.0).collect();
        assert_eq!(trend(&ch, &fx, 199, 206), [o[202] / o[200] * (fx[202] / fx[200]) - 1.0, c[205] / o[203] * (fx[205] / fx[203]) - 1.0]);
    }

    /// (#1222) EUR per unit on each date: ones for EUR, the last USD rate on or before the date (the first
    /// one before the series starts) for USD, and nothing for another currency or an empty USD series.
    #[test]
    fn rates_carry_the_last_eur_usd_forward() {
        let usd: BTreeMap<NaiveDate, f64> = [(d(1), 0.9), (d(3), 0.8)].into();
        let days = [d(0), d(1), d(2), d(3), d(4)];
        assert_eq!(rates_on("EUR", &days[..2], &usd), Some(vec![1.0, 1.0]));
        assert_eq!(rates_on("USD", &days, &usd), Some(vec![0.9, 0.9, 0.9, 0.8, 0.8]));
        assert_eq!(rates_on("GBP", &days, &usd), None);
        assert_eq!(rates_on("USD", &days, &BTreeMap::new()), None);
    }

    /// (#1222) Each way's trades in euros: every session, HOLD 21 back to back with the last one cut at the
    /// window's end, no TREND spell on a chart short of 200 bars, and one BUY&HOLD trade; none on no bar.
    #[test]
    fn the_four_ways_trade_their_window_in_euros() {
        let c = wavy(25);
        let fx: Vec<f64> = (0..25).map(|j| 1.0 + j as f64 / 100.0).collect();
        let [day, h21, tr, bh] = trades(&c, &fx, 2, 25);
        assert_eq!(day, (2..25).map(|i| c.closes[i] / c.opens[i] - 1.0).collect::<Vec<_>>());
        assert_eq!(
            h21,
            [c.closes[22] / c.opens[2] * (fx[22] / fx[2]) - 1.0, c.closes[24] / c.opens[23] * (fx[24] / fx[23]) - 1.0]
        );
        assert!(tr.is_empty());
        assert_eq!(bh, [c.closes[24] / c.opens[2] * (fx[24] / fx[2]) - 1.0]);
        assert!(trades(&c, &fx, 5, 5).iter().all(Vec::is_empty));
    }

    /// (#1222) €1,000 a trade after the round trip; BEST is the most euros, the first way on a tie.
    #[test]
    fn best_is_the_most_euros_after_cost_first_on_a_tie() {
        assert_eq!(euros(&[0.5, -0.25], 2500.0), -250.0);
        assert_eq!(pick(&[vec![0.1], vec![0.3], vec![0.2], vec![0.3]], 0.0), 1);
        assert_eq!(pick(&[vec![], vec![0.1], vec![0.3], vec![0.2]], 0.0), 2);
        assert_eq!(pick(&[vec![], vec![], vec![0.1], vec![0.2]], 0.0), 3);
        let busy = [vec![0.01; 10], vec![0.05], vec![], vec![0.04]];
        assert_eq!((pick(&busy, 0.0), pick(&busy, 100.0)), (0, 1), "ten trades pay the round trip ten times");
    }

    /// (#1222) Three 3-month steps from 2028-07-20, the last ending on `today`. A: on all three, with exactly
    /// 200 bars before its first 2y. B: a bar short of that on the first step, so on the last two. C: ended
    /// before the first step. D: in the index for the first step only. I: under the floor. F: flat in its
    /// own currency, at 0.6 EUR up to the first step and 0.4 from it, so over the floor on the bar before
    /// that step and under it after; TREND, with no trade, is the most euros on its 2y.
    #[test]
    fn the_best_walk_picks_on_two_years_and_trades_the_next_step() {
        let ymd = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        let (start, from2, from3, today) = (ymd(2028, 7, 20), ymd(2028, 10, 20), ymd(2029, 1, 20), ymd(2029, 4, 20));
        let ix = |x: NaiveDate| (x - d(0)).num_days() as usize;
        let a = wavy(1206);
        let mut b = wavy(1205);
        b.dates = (1..1206).map(d).collect();
        let c = wavy(800);
        let mut i = wavy(1206);
        i.volumes = vec![1.0; 1206];
        let f = chart(&[100.0; 1206], &[100.0; 1206], 1e5);
        let one = [1.0; 1206];
        let ffx: Vec<f64> = (0..1206).map(|j| if j < ix(start) { 0.6 } else { 0.4 }).collect();
        let dspan = [(d(0), Some(from2))];
        let pond: [Priced; 6] = [
            ("A", &a, &one, None),
            ("B", &b, &one, None),
            ("C", &c, &one, None),
            ("D", &a, &one, Some(dspan.as_slice())),
            ("I", &i, &one, None),
            ("F", &f, &ffx, None),
        ];
        let g = |c: &Chart, fx: &[f64], l: usize, s: usize, e: usize| {
            let w = pick(&trades(c, fx, l, s), 20.0);
            let next = trades(c, fx, s, e);
            let hold = euros(&next[3], 20.0);
            (w, euros(&next[w], 20.0) - hold, hold)
        };
        let want = |date, rows: &[(usize, f64, f64)]| {
            let mut picks = [0usize; 4];
            rows.iter().for_each(|r| picks[r.0] += 1);
            let n = rows.len() as f64;
            (date, rows.len(), rows.iter().fold(0.0, |t, r| t + r.1) / n, rows.iter().fold(0.0, |t, r| t + r.2) / n, picks)
        };
        let (s1, s2, s3) = (ix(start), ix(from2), ix(from3));
        let first = g(&a, &one, 200, s1, s2);
        let flat = g(&f, &ffx, 200, s1, s2);
        assert_eq!(flat.0, 2, "F picks TREND");
        let got: Vec<_> = best_steps(&pond, 20.0, 3, start, today).iter().map(|s| (s.date, s.n, s.excess, s.hold, s.picks)).collect();
        assert_eq!(
            got,
            [
                want(start, &[first, first, flat]),
                want(from2, &[g(&a, &one, ix(ymd(2026, 10, 20)), s2, s3), g(&b, &one, ix(ymd(2026, 10, 20)) - 1, s2 - 1, s3 - 1)]),
                want(from3, &[g(&a, &one, ix(ymd(2027, 1, 20)), s3, ix(today)), g(&b, &one, ix(ymd(2027, 1, 20)) - 1, s3 - 1, ix(today) - 1)]),
            ]
        );
        let gap = [(d(0), Some(from2)), (from3, None)];
        let dates: Vec<NaiveDate> = best_steps(&[("D", &a, one.as_slice(), Some(gap.as_slice()))], 20.0, 3, start, today).iter().map(|s| s.date).collect();
        assert_eq!(dates, [start, from3], "a step with no graded name drops out and the walk goes on");
    }

    /// (#1222) The walk's line: mean pick minus hold over the steps, t, halves, holding alone, the median
    /// names graded and each way's share of the picks; PASS on [`passes`] above zero.
    #[test]
    fn the_best_walk_reads_one_line_a_market_and_step() {
        let step = |date, n, excess, hold, picks| Step { date, n, excess, hold, picks };
        let mut steps = [
            step(d(0), 3, 1.0, 10.0, [1, 1, 1, 0]),
            step(d(31), 5, 3.0, 20.0, [0, 5, 0, 0]),
            step(d(59), 4, 2.0, -6.0, [0, 0, 1, 3]),
            step(d(90), 8, 4.0, 0.0, [4, 0, 0, 4]),
        ];
        assert_eq!(
            best_report("US", 6, &steps),
            "  US 6m steps: 4 from 2026-01-01 to 2026-04-01 median 5 names  pick minus hold €+2.50 a name a step  \
             t +3.87  halves €+2.00|€+3.00  hold alone €+6.00  picked SAME DAY 25%, HOLD 21 30%, TREND 10%, BUY&HOLD 35%  -> PASS"
        );
        steps[1].excess = -3.0;
        assert!(best_report("US", 6, &steps).ends_with("halves €-1.00|€+3.00  hold alone €+6.00  picked SAME DAY 25%, HOLD 21 30%, TREND 10%, BUY&HOLD 35%  -> FAIL"));
        assert_eq!(best_report("EU", 1, &steps[..1]), "  EU 1m steps: 1 graded; the t needs 2");
    }

    /// (#1223) Each month's first journal line links to the next month's first. A gains €10 a session on a
    /// €1,000 stake: over the 31 sessions of January its SAME DAY nets 31 × €8 against holding's one €8, so
    /// the link reads €240. Z has no chart; OLD's bars end before February, so that link drops; in March,
    /// U pays the US's €5 a trip (31 × €5 against €5) and A's own BUY&HOLD reads €0, a mean of €75; April's
    /// line only ends March's link.
    #[test]
    fn best_links_grade_each_months_journalled_way_against_holding() {
        let a = chart(&[100.0; 120], &[101.0; 120], 1e5);
        let mut u = a.clone();
        u.currency = "USD".into();
        let old = chart(&[100.0; 10], &[101.0; 10], 1e5);
        let priced: HashMap<String, (Chart, Vec<f64>)> = [("A", a), ("U", u), ("OLD", old)].map(|(tk, c)| {
            let fx = vec![1.0; c.dates.len()];
            (tk.to_string(), (c, fx))
        })
        .into();
        let lines = [(d(0), vec![("A", 0), ("Z", 1)]), (d(5), vec![("A", 3)]), (d(31), vec![("OLD", 0)]), (d(59), vec![("U", 0), ("A", 3)]), (d(90), vec![("A", 0)])];
        let got = best_links(&lines, &priced);
        assert_eq!(got.iter().map(|l| l.0).collect::<Vec<_>>(), [d(0), d(59)]);
        assert!((got[0].1 - 240.0).abs() < 1e-9 && (got[1].1 - 75.0).abs() < 1e-9, "{got:?}");
        assert!(best_links(&[], &priced).is_empty());
    }

    /// (#1223) The track line counts the links, then reads their mean, t and halves, waiting for 12.
    #[test]
    fn the_best_track_line_waits_for_twelve_links() {
        assert_eq!(best_track_line("K7 US", &[]), "  BEST K7 US: 0 monthly link(s) graded; the t needs 2  (reads after 12 links)");
        let links = [(d(0), 1.0), (d(31), 2.0), (d(59), 3.0)];
        assert_eq!(
            best_track_line("K7 US", &links),
            "  BEST K7 US: 3 monthly links 2026-01-01..2026-03-01  BEST minus BUY&HOLD €+2.00 a name a link  t +3.46  halves €+1.00|€+2.50  (reads after 12 links)"
        );
        let year: Vec<(NaiveDate, f64)> = (0..12).map(|i| (d(31 * i), i as f64)).collect();
        assert!(best_track_line("BUY NOW", &year).ends_with("halves €+2.50|€+8.50"));
        assert!(best_track_line("BUY NOW", &year[..11]).ends_with("(reads after 12 links)"));
    }

    /// A weekend line and the Monday line after it reach one session; the first one grades it.
    #[test]
    fn a_journalled_book_grades_on_the_session_it_ranked_for() {
        let mut a = chart(&[100.0; 4], &[101.0, 102.0, 103.0, 104.0], 1.0);
        a.dates = vec![d(0), d(3), d(4), d(5)];
        let mut b = chart(&[100.0; 2], &[150.0, 200.0], 1.0);
        b.dates = vec![d(3), d(4)];
        let mut c = chart(&[100.0], &[110.0], 1.0);
        c.dates = vec![d(5)];
        let charts: HashMap<String, Chart> = [("A", a), ("B", b), ("C", c)].map(|(t, c)| (t.to_string(), c)).into();
        let lines = [(d(1), vec!["A", "Z"]), (d(2), vec!["B"]), (d(4), vec!["B", "C"]), (d(5), vec!["A"]), (d(9), vec!["A"])];
        let near = |got: Vec<(NaiveDate, f64)>, want: &[(NaiveDate, f64)]| {
            assert_eq!(got.len(), want.len(), "{got:?}");
            for (g, w) in got.iter().zip(want) {
                assert!(g.0 == w.0 && (g.1 - w.1).abs() < 1e-12, "{got:?}");
            }
        };
        // (#876) d(3) lost Z, d(4) mixes B's d(4) bar with C's d(5) one; d(5) is A alone, the whole book
        let (days, thin) = graded(&lines, &charts, d(6));
        near(days, &[(d(3), 0.02), (d(4), 0.55), (d(5), 0.04)]);
        assert_eq!(thin, 2);
        // a bar dated today may still be trading: C's drops out, and A's d(5) session waits
        let (days, thin) = graded(&lines, &charts, d(5));
        near(days, &[(d(3), 0.02), (d(4), 1.0)]);
        assert_eq!(thin, 2, "d(4) is B alone now, short of the two journalled");
        assert_eq!(graded(&[], &charts, d(6)), (vec![], 0));
    }

    /// (#836) K10/K11 against a plain ratio: the last session's volume over the mean of the 20 before it.
    #[test]
    fn volume_keys_read_the_last_session_over_the_20_before() {
        let mut c = wavy(30);
        c.volumes = (0..30).map(|j| 1e5 + j as f64).collect();
        for i in [21, 29] {
            let want = c.volumes[i - 1] / ((i - 21..i - 1).map(|j| c.volumes[j]).sum::<f64>() / 20.0);
            assert!((key(11, &c, i).unwrap() - want).abs() < 1e-12, "K11 at {i}");
            assert_eq!(key(10, &c, i), Some(-key(11, &c, i).unwrap()), "K10 at {i}");
        }
        assert!(key(10, &c, 20).is_none() && key(11, &c, 20).is_none());
        for bad in [0.0, f64::NAN] {
            c.volumes[25] = bad;
            assert!(key(10, &c, 25).is_some(), "bar 25 is not in the window yet");
            assert!(key(10, &c, 26).is_none() && key(11, &c, 29).is_none(), "{bad} as the last print, then in the mean");
        }
    }

    /// `year`-`month`-`day` at `hour`:`min`, UTC.
    fn utc(year: i32, month: u32, day: u32, hour: u32, min: u32) -> NaiveDateTime {
        let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        date.and_hms_opt(hour, min, 0).unwrap()
    }

    /// (#1180) Half an hour past the close on the exchange's own clock: winter 17:00 and 21:30 UTC, summer
    /// 16:00 and 20:30 UTC. Each row is the first minute of 2026 that counts as closed: the day before a
    /// switch is on the old clock, the switch day on the new one, and between the two markets' switches
    /// (US 03-08 and 11-01, EU 03-29 and 10-25) only one of them is on summer time.
    #[test]
    fn a_session_is_closed_half_an_hour_after_the_exchange_closes() {
        let first_closed = [
            ("EU", 1, 15, 17, 0),
            ("US", 1, 15, 21, 30),
            ("EU", 7, 15, 16, 0),
            ("US", 7, 15, 20, 30),
            ("US", 3, 7, 21, 30),
            ("US", 3, 8, 20, 30),
            ("US", 10, 31, 20, 30),
            ("US", 11, 1, 21, 30),
            ("EU", 3, 28, 17, 0),
            ("EU", 3, 29, 16, 0),
            ("EU", 10, 24, 16, 0),
            ("EU", 10, 25, 17, 0),
            ("US", 3, 20, 20, 30),
            ("EU", 3, 20, 17, 0),
            ("US", 10, 28, 20, 30),
            ("EU", 10, 28, 17, 0),
        ];
        for (market, m, d, h, mi) in first_closed {
            let (shut, from) = (|t| closed(market, t), utc(2026, m, d, h, mi));
            let before = from - Duration::minutes(1);
            assert!(!shut(before) && shut(from), "{market} {m}-{d}");
        }
    }

    /// (#1180) The switch instants themselves, which no close time reaches, and years where the last
    /// Sunday of March is the 25th (2029) and the 31st (2024).
    #[test]
    fn summer_time_starts_and_ends_on_the_switch_instants() {
        let starts = [
            ("US", 2026, 3, 8, 7),
            ("EU", 2026, 3, 29, 1),
            ("EU", 2029, 3, 25, 1),
            ("EU", 2024, 3, 31, 1),
        ];
        for (market, y, m, d, h) in starts {
            let on = |h, mi| summer_time(market, utc(y, m, d, h, mi));
            assert!(!on(h - 1, 59) && on(h, 0), "{market} {y}-{m}-{d} starts");
        }
        let ends = [
            ("US", 2026, 11, 1, 6),
            ("EU", 2026, 10, 25, 1),
            ("EU", 2024, 10, 27, 1),
        ];
        for (market, y, m, d, h) in ends {
            let on = |h, mi| summer_time(market, utc(y, m, d, h, mi));
            assert!(on(h - 1, 59) && !on(h, 0), "{market} {y}-{m}-{d} ends");
        }
        for (market, m, d) in [("US", 1, 1), ("US", 12, 31), ("EU", 1, 1), ("EU", 12, 31)] {
            assert!(!summer_time(market, utc(2026, m, d, 12, 0)));
        }
    }

    #[test]
    fn only_eur_and_a_quoted_usd_have_a_rate() {
        assert_eq!(rate_for("EUR", None), Some(1.0));
        assert_eq!(rate_for("USD", Some(0.9)), Some(0.9));
        assert_eq!(rate_for("USD", None), None, "a failed FX quote drops the USD names");
        assert_eq!(rate_for("GBP", Some(0.9)), None);
    }

    #[test]
    fn the_track_line_is_net_of_cost_in_bp() {
        let days = [(d(0), 0.0060), (d(1), 0.0062), (d(2), 0.0058), (d(3), 0.0060)];
        assert_eq!(
            track_line("US", &days, 0, 50.0),
            "  US: 4 sessions 2026-01-01..2026-01-04  book net +10.0 bp/day (gross +60.0)  t +12.25"
        );
        assert_eq!(track_line("EU", &days[..1], 0, 20.0), "  EU: 1 session(s) graded; the t needs 2");
        // (#876) a thinned session is said, with its share of the graded ones
        assert_eq!(
            track_line("US", &days, 2, 50.0),
            "  US: 4 sessions 2026-01-01..2026-01-04  book net +10.0 bp/day (gross +60.0)  t +12.25  (2 of 4 sessions graded on fewer names than the book held, or on two dates)"
        );
        assert_eq!(
            track_line("EU", &days[..1], 1, 20.0),
            "  EU: 1 session(s) graded; the t needs 2  (1 of 1 sessions graded on fewer names than the book held, or on two dates)"
        );
    }

    /// (#1212) One session's 60m bars: each `(h, c)` starts `h` hours after `open` (UTC) and closes at `c`;
    /// every bar opens at `o`, so the bar at `h` = 0 gives the session's open.
    fn session_bars(open: NaiveDateTime, o: f64, bars: &[(i64, f64)]) -> Vec<(i64, f64, f64)> {
        bars.iter().map(|&(h, c)| ((open + Duration::hours(h)).and_utc().timestamp(), o, c)).collect()
    }

    /// US sessions as [`day_moves`] keys them, the open at 0 and each `(end, move)`.
    fn moves(days: &[(usize, Vec<(u32, f64)>)]) -> BTreeMap<NaiveDate, BTreeMap<u32, f64>> {
        days.iter().map(|(i, e)| (d(*i), std::iter::once((570, 0.0)).chain(e.iter().copied()).collect())).collect()
    }

    #[test]
    fn bar_end_is_a_whole_hour_after_the_open_or_the_close() {
        assert_eq!((session("US"), session("EU")), ((570, 960), (540, 1050)));
        let ends = |market| (0..=1440).filter(|&m| crate::config::bar_end(market, m)).collect::<Vec<u32>>();
        assert_eq!(ends("US"), [630, 690, 750, 810, 870, 930, 960]);
        assert_eq!(ends("EU"), [600, 660, 720, 780, 840, 900, 960, 1020, 1050]);
    }

    #[test]
    fn exit_head_names_both_clocks() {
        let on = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        // New York leaves summer time on 2026-11-01, a week after Portugal, and enters it three weeks before
        assert_eq!(exit_head("US", 870, on(10, 28)), "P/L 09:30-14:30 NY (13:30-18:30 PT)");
        assert_eq!(exit_head("US", 870, on(11, 2)), "P/L 09:30-14:30 NY (14:30-19:30 PT)");
        assert_eq!(exit_head("US", 870, on(7, 1)), "P/L 09:30-14:30 NY (14:30-19:30 PT)");
        assert_eq!(exit_head("US", 960, on(3, 20)), "P/L 09:30-16:00 NY (13:30-20:00 PT)");
        for day in [on(7, 1), on(12, 1)] {
            assert_eq!(exit_head("EU", 960, day), "P/L 09:00-16:00 FRA (08:00-15:00 PT)");
        }
        assert_eq!(exit_head("EU", 600, on(1, 5)), "P/L 09:00-10:00 FRA (08:00-09:00 PT)");
    }

    #[test]
    fn day_moves_keys_each_bar_end_on_the_exchange_clock() {
        let date = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        let ts = |t: NaiveDateTime| t.and_utc().timestamp();
        // 13:30 UTC is 09:30 New York on summer time, 14:30 UTC after 2026-11-01; a bar with no close is skipped
        let mut bars = session_bars(utc(2026, 10, 30, 13, 30), 100.0, &[(0, 101.0), (3, f64::NAN), (6, 104.0)]);
        // a bar before the open, and one from the close on, are not the session's
        bars.push((ts(utc(2026, 10, 30, 12, 30)), 50.0, 50.0));
        bars.push((ts(utc(2026, 10, 30, 20, 0)), 999.0, 999.0));
        bars.extend(session_bars(utc(2026, 11, 2, 14, 30), 200.0, &[(0, 202.0), (6, 190.0)]));
        // a half day has no last bar, and this session misses its first
        bars.extend(session_bars(utc(2026, 11, 3, 14, 30), 200.0, &[(0, 202.0), (1, 204.0)]));
        bars.extend(session_bars(utc(2026, 11, 4, 14, 30), 200.0, &[(1, 202.0), (6, 204.0)]));
        let want = BTreeMap::from([
            (date(10, 30), BTreeMap::from([(570, 0.0), (630, 101.0 / 100.0 - 1.0), (960, 104.0 / 100.0 - 1.0)])),
            (date(11, 2), BTreeMap::from([(570, 0.0), (630, 202.0 / 200.0 - 1.0), (960, 190.0 / 200.0 - 1.0)])),
        ]);
        assert_eq!(day_moves("US", &bars), want);
        // (#1215) the same keys on the prices themselves
        let prices = BTreeMap::from([
            (date(10, 30), BTreeMap::from([(570, 100.0), (630, 101.0), (960, 104.0)])),
            (date(11, 2), BTreeMap::from([(570, 200.0), (630, 202.0), (960, 190.0)])),
        ]);
        assert_eq!(day_prices("US", &bars), prices);
        // Frankfurt runs two hours ahead of UTC on summer time, one after 2026-10-25; its last bar starts at 17:00
        let eu = [
            session_bars(utc(2026, 10, 23, 7, 0), 50.0, &[(0, 50.5), (6, 51.0), (8, 49.0)]),
            session_bars(utc(2026, 10, 26, 8, 0), 50.0, &[(0, 50.5), (6, 52.0), (8, 53.0)]),
        ]
        .concat();
        let want = BTreeMap::from([
            (date(10, 23), BTreeMap::from([(540, 0.0), (600, 50.5 / 50.0 - 1.0), (960, 51.0 / 50.0 - 1.0), (1050, 49.0 / 50.0 - 1.0)])),
            (date(10, 26), BTreeMap::from([(540, 0.0), (600, 50.5 / 50.0 - 1.0), (960, 52.0 / 50.0 - 1.0), (1050, 53.0 / 50.0 - 1.0)])),
        ]);
        assert_eq!(day_moves("EU", &eu), want);
        let prices = BTreeMap::from([
            (date(10, 23), BTreeMap::from([(540, 50.0), (600, 50.5), (960, 51.0), (1050, 49.0)])),
            (date(10, 26), BTreeMap::from([(540, 50.0), (600, 50.5), (960, 52.0), (1050, 53.0)])),
        ]);
        assert_eq!(day_prices("EU", &eu), prices);
    }

    #[test]
    fn stamp_exits_puts_each_rows_own_market_sold_at_its_hour_after_same_day() {
        use serde_json::json;
        let row = |market: &str, tk: &str| vec![json!(["RANK", "1"]), json!(["MKT", market]), json!(["TICKER", tk]), json!(["SAME DAY", "+€1.00"]), json!(["K7", "50%"])];
        let mut rows = vec![row("US", "AAA"), row("EU", "EEE"), row("US", "BBB"), row("XX", "AAA")];
        let us = |m, d, h| utc(2026, m, d, h, 30);
        // 09:30 New York is 13:30 UTC before 2026-11-01 and 14:30 after; the first session sits before the
        // window, 2 years before `until`, and the last on `until` itself
        let hourly = HashMap::from([
            (
                "AAA".to_string(),
                [
                    session_bars(utc(2024, 11, 1, 13, 30), 100.0, &[(0, 100.0), (4, 150.0), (6, 150.0)]),
                    session_bars(us(10, 30, 13), 100.0, &[(0, 100.0), (4, 104.0), (6, 101.0)]),
                    session_bars(us(11, 2, 14), 100.0, &[(0, 100.0), (4, 99.0), (6, 103.0)]),
                    session_bars(us(11, 3, 14), 100.0, &[(0, 100.0), (4, 150.0), (6, 150.0)]),
                ]
                .concat(),
            ),
            // 09:00 Frankfurt is 08:00 UTC after 2026-10-25
            ("EEE".to_string(), session_bars(utc(2026, 10, 30, 8, 0), 50.0, &[(0, 50.0), (6, 51.0), (8, 49.0)])),
        ]);
        let on = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        stamp_exits(&mut rows, &hourly, &[("US", 870), ("EU", 960)], on(11, 3), on(11, 2));
        let hindsight = "(hindsight, as SAME DAY is; a half day, or a session missing its first or last bar, drops out, so the count can sit under SAME DAY's)";
        let tip_us = format!("P/L 09:30-14:30 NY (14:30-19:30 PT): €1,000 bought at the open and sold at 14:30 New York time, each of the 2 sessions since 2024-11-03 on this name's 60m bars: +€30.00 before costs; the backtest's 50 bp a round trip costs 2 × €5.00 = €10.00; +€20.00 after, about +€10.00 a session\nsold at the close instead, the same 2 sessions: +€30.00 after costs, so this hour made -€10.00 against the close\n{hindsight}");
        let tip_eu = format!("P/L 09:00-16:00 FRA (08:00-15:00 PT): €1,000 bought at the open and sold at 16:00 Frankfurt time, each of the 1 sessions since 2024-11-03 on this name's 60m bars: +€20.00 before costs; the backtest's 20 bp a round trip costs 1 × €2.00 = €2.00; +€18.00 after, about +€18.00 a session\nsold at the close instead, the same 1 sessions: -€22.00 after costs, so this hour made +€40.00 against the close\n{hindsight}");
        let (key, none) = (json!(["K7", "50%"]), json!(["EARLY EXIT", "—"]));
        assert_eq!(rows[0][4..], [json!(["EARLY EXIT", "+€20.00", null, tip_us]), key.clone()]);
        assert_eq!(rows[1][4..], [json!(["EARLY EXIT", "+€18.00", null, tip_eu]), key.clone()]);
        // a name with no 60m bars, and a market with no exit hour
        assert_eq!(rows[2][4..], [none.clone(), key.clone()]);
        assert_eq!(rows[3][4..], [none.clone(), key]);
        // a row with no SAME DAY takes the cell last
        let mut bare = vec![vec![json!(["RANK", "1"]), json!(["MKT", "US"]), json!(["TICKER", "BBB"])]];
        stamp_exits(&mut bare, &hourly, &[("US", 870)], on(11, 3), on(11, 2));
        assert_eq!(bare[0][3], none);
    }

    #[test]
    fn hours_report_grades_each_buy_and_sell_window_against_open_to_close() {
        let names = [
            moves(&[(1, vec![(870, 0.010), (960, 0.000)]), (2, vec![(870, 0.030), (960, 0.010)]), (3, vec![(870, 0.000), (960, 0.015)]), (4, vec![(870, 9.0), (960, 9.0)])]),
            moves(&[(1, vec![(630, 0.005), (870, 0.030), (960, 0.020)]), (2, vec![(870, 0.010), (960, 0.010)])]),
        ];
        // a session on or after `today` is left out, every window with 10:30 in it has one session, so no t,
        // and a buy at 14:30 earns the move on from there: 1.02 / 1.03 - 1 for the second name on day 1
        assert_eq!(
            hours_report("US", &names, d(4), 50.0, (570, 870)),
            "  US: 2 names, 3 sessions 2026-01-02..2026-01-04; cost 50 bp a round trip\
             \n  US 09:30-14:30  3 sessions  gross +133.33 bp (t +2.00)  net +8.33 €/session  halves +200.00|+100.00  vs open-close +16.67 bp (t +0.20)  <- best, the knob\
             \n  US 09:30-16:00  3 sessions  gross +116.67 bp (t +7.00)  net +6.67 €/session  halves +100.00|+125.00  open to close\
             \n  US 14:30-16:00  3 sessions  gross -15.05 bp (t -0.18)  net -6.50 €/session  halves -98.05|+26.46  vs open-close -131.71 bp (t -2.00)"
        );
        let report = hours_report("US", &names, d(4), 50.0, (870, 960));
        let marks: Vec<(bool, bool)> = report.lines().skip(1).map(|l| (l.ends_with("  <- best"), l.ends_with("  <- the knob"))).collect();
        assert_eq!(marks, [(true, false), (false, false), (false, true)]);
        assert_eq!(hours_report("US", &names, d(2), 50.0, (570, 870)), "  US: no two sessions with 60m bars");
        assert_eq!(hours_report("EU", &[], d(4), 20.0, (540, 960)), "  EU: no two sessions with 60m bars");
    }

    /// (#1215) Sessions as [`day_prices`] keys them.
    fn prices(days: &[(usize, Vec<(u32, f64)>)]) -> Prices {
        days.iter().map(|(i, e)| (d(*i), e.iter().copied().collect())).collect()
    }

    #[test]
    fn holds_report_grades_every_buy_key_against_every_sell_key_h_sessions_on() {
        // A falls through its first two sessions and B through its second and last, the book's sessions, so
        // every same-session book pair loses; C has no first or second session; the session on `today` is out
        let names = [
            (
                "A",
                prices(&[
                    (1, vec![(570, 102.0), (870, 101.0), (960, 100.0)]),
                    (2, vec![(570, 101.0), (870, 100.5), (960, 99.0)]),
                    (3, vec![(570, 103.5), (870, 106.0), (960, 104.0)]),
                    (4, vec![(570, 104.0), (870, 103.0), (960, 102.0)]),
                    (5, vec![(570, 102.0), (870, 105.0), (960, 107.0)]),
                    (6, vec![(570, 1e3), (870, 1e3), (960, 1e3)]),
                ]),
            ),
            (
                "B",
                prices(&[
                    (1, vec![(570, 50.0), (870, 51.0), (960, 50.5)]),
                    (2, vec![(570, 50.5), (870, 50.0), (960, 49.0)]),
                    (3, vec![(570, 49.0), (960, 50.0)]),
                    (4, vec![(570, 50.0), (870, 52.0), (960, 51.0)]),
                    (5, vec![(570, 52.0), (870, 51.0), (960, 50.5)]),
                ]),
            ),
            (
                "C",
                prices(&[
                    (3, vec![(570, 20.0), (870, 19.8), (960, 20.6)]),
                    (4, vec![(570, 20.6), (870, 21.0), (960, 20.8)]),
                    (5, vec![(570, 21.0), (870, 21.2), (960, 20.9)]),
                ]),
            ),
        ];
        let held = |v: &[&str]| v.iter().map(|t| t.to_string()).collect::<Vec<String>>();
        // the book holds no name with prices on the third session, and nothing on the fourth
        let books = BTreeMap::from([(d(1), held(&["A"])), (d(2), held(&["A", "B"])), (d(3), held(&["Z"])), (d(5), held(&["B"]))]);
        // a hold of a session or more may sell at an earlier key than it bought at: 16:00 to 14:30 the next day
        assert_eq!(
            holds_report("US", &names, &books, d(6), 50.0, &[0, 1, 2, 5]),
            "  US: 3 names, 5 sessions 2026-01-02..2026-01-06; cost 50 bp a round trip\
             \n  US whole span: 2 names bought at the first open and sold at the last close  gross +295.1 bp  net +6.13 € a session over 4\
             \n  US pond sell day +0  best 09:30-14:30  5 holds  gross +55.84 bp (t +1.46)  net +0.58 € a hold, +0.58 € a session  halves -11.64|+100.82\
             \n  US pond sell day +0  open-close  5 holds  gross -5.03 bp (t -0.07)  net -5.50 € a hold, -5.50 € a session  halves -147.78|+90.14\
             \n  US book sell day +0  best 09:30-14:30  3 holds  gross -121.53 bp (t -3.37)  net -17.15 € a hold, -17.15 € a session  halves -98.04|-133.28\
             \n  US book sell day +0  open-close  3 holds  gross -244.02 bp (t -9.13)  net -29.40 € a hold, -29.40 € a session  halves -196.08|-267.99\
             \n  US pond sell day +1  best 16:00-14:30  4 holds  gross +252.68 bp (t +1.60)  net +20.27 € a hold, +20.27 € a session  halves +341.28|+164.07\
             \n  US pond sell day +1  open-close  4 holds  gross +62.77 bp (t +0.59)  net +1.28 € a hold, +1.28 € a session  halves -74.02|+199.55\
             \n  US book sell day +1  best 16:00-14:30  2 holds  gross +378.54 bp (t +1.15)  net +32.85 € a hold, +32.85 € a session  halves +50.00|+707.07\
             \n  US book sell day +1  open-close  2 holds  gross -97.55 bp (t -0.50)  net -14.76 € a hold, -14.76 € a session  halves -294.12|+99.01\
             \n  US pond sell day +2  best 14:30-14:30  2 holds  gross +400.71 bp (t +4.25)  net +35.07 € a hold, +17.54 € a session  halves +495.05|+306.37\
             \n  US pond sell day +2  open-close  2 holds  gross +231.40 bp (t +1.74)  net +18.14 € a hold, +9.07 € a session  halves +98.04|+364.76\
             \n  US book sell day +2  no two holds\
             \n  US pond sell day +5  no two holds\
             \n  US book sell day +5  no two holds"
        );
        assert_eq!(holds_report("US", &names, &books, d(2), 50.0, &[0]), "  US: no two sessions with 60m bars");
        assert_eq!(holds_report("EU", &[], &BTreeMap::new(), d(6), 20.0, &[0]), "  EU: no two sessions with 60m bars");
        // no name has both ends of the span
        let apart = [("P", prices(&[(1, vec![(540, 10.0), (1050, 10.0)])])), ("Q", prices(&[(2, vec![(540, 10.0), (1050, 11.0)])]))];
        assert_eq!(
            holds_report("EU", &apart, &BTreeMap::new(), d(3), 20.0, &[0]),
            "  EU: 2 names, 2 sessions 2026-01-02..2026-01-03; cost 20 bp a round trip\
             \n  EU whole span: no name has both the first session and the last\
             \n  EU pond sell day +0  best 09:00-17:30  2 holds  gross +500.00 bp (t +1.00)  net +48.00 € a hold, +48.00 € a session  halves +0.00|+1000.00\
             \n  EU pond sell day +0  open-close  2 holds  gross +500.00 bp (t +1.00)  net +48.00 € a hold, +48.00 € a session  halves +0.00|+1000.00\
             \n  EU book sell day +0  no two holds"
        );
    }

    #[test]
    fn slices_tag_each_session_by_weekday_month_turn_pre_holiday_and_expiry() {
        // the weekdays from 01-26 to 03-03 less Monday 02-09 and Tuesday 02-17; January and March are the edge
        // months, so they carry no month tag
        let day = |m: u32, dd: u32| NaiveDate::from_ymd_opt(2026, m, dd).unwrap();
        let cal: Vec<NaiveDate> = (0..37)
            .map(|i| day(1, 26) + Duration::days(i))
            .filter(|x| x.weekday().num_days_from_monday() < 5 && *x != day(2, 9) && *x != day(2, 17))
            .collect();
        assert_eq!(
            slices(&cal).iter().map(|(x, v)| format!("{x} {}", v.join(", "))).collect::<Vec<_>>(),
            [
                "2026-01-26 Mon",
                "2026-01-27 Tue",
                "2026-01-28 Wed",
                "2026-01-29 Thu",
                "2026-01-30 Fri",
                "2026-02-02 Mon, month turn",
                "2026-02-03 Tue, month turn",
                "2026-02-04 Wed, month turn",
                "2026-02-05 Thu, month turn",
                "2026-02-06 Fri, rest of month, pre-holiday",
                "2026-02-10 Tue, rest of month",
                "2026-02-11 Wed, rest of month",
                "2026-02-12 Thu, rest of month",
                "2026-02-13 Fri, rest of month",
                "2026-02-16 Mon, rest of month, pre-holiday",
                "2026-02-18 Wed, rest of month",
                "2026-02-19 Thu, rest of month",
                "2026-02-20 Fri, rest of month, expiry",
                "2026-02-23 Mon, rest of month",
                "2026-02-24 Tue, rest of month",
                "2026-02-25 Wed, rest of month",
                "2026-02-26 Thu, month turn",
                "2026-02-27 Fri, month turn",
                "2026-03-02 Mon",
                "2026-03-03 Tue",
            ]
        );
        assert!(slices(&[]).is_empty());
    }

    #[test]
    fn clears_needs_net_t_both_halves_and_the_daily_slice() {
        let ok = Stats { n: 2, mean: 0.0078125, t: 3.0, h1: 0.0078125, h2: 0.0078125 };
        assert!(clears(&ok, 78.0, true));
        assert!(clears(&Stats { t: 4.0, ..ok }, 78.0, true));
        // a gross mean equal to the cost nets nothing
        assert!(!clears(&ok, 78.125, true));
        assert!(!clears(&ok, 78.0, false));
        assert!(!clears(&Stats { mean: 0.0, ..ok }, 78.0, true));
        assert!(!clears(&Stats { t: 2.99, ..ok }, 78.0, true));
        assert!(!clears(&Stats { h1: 0.0, ..ok }, 78.0, true));
        assert!(!clears(&Stats { h2: 0.0, ..ok }, 78.0, true));
    }

    #[test]
    fn calendar_report_grades_every_same_session_pair_per_slice_and_name_set() {
        // two Fridays and two Mondays of one month, so no month tag; the first Monday's Tuesday is missing, the
        // one pre-holiday session; C starts on the second Friday; the session on `today` is out
        let names = [
            (
                "A",
                prices(&[
                    (1, vec![(570, 100.0), (870, 101.0), (960, 100.5)]),
                    (4, vec![(570, 100.0), (870, 101.2), (960, 100.8)]),
                    (8, vec![(570, 100.0), (870, 101.0), (960, 101.5)]),
                    (11, vec![(570, 100.0), (870, 101.1), (960, 101.0)]),
                    (15, vec![(570, 1e3), (870, 1e3), (960, 1e3)]),
                ]),
            ),
            (
                "B",
                prices(&[
                    (1, vec![(570, 50.0), (870, 50.3), (960, 49.9)]),
                    (4, vec![(570, 50.0), (870, 50.4), (960, 50.05)]),
                    (8, vec![(570, 50.0), (870, 50.45), (960, 50.6)]),
                    (11, vec![(570, 50.0), (870, 50.55), (960, 50.0)]),
                ]),
            ),
            ("C", prices(&[(8, vec![(570, 20.0), (870, 20.16), (960, 20.1)]), (11, vec![(570, 20.0), (870, 20.3), (960, 20.2)])])),
        ];
        let held = |v: &[&str]| v.iter().map(|t| t.to_string()).collect::<Vec<String>>();
        // the book holds no name with prices on the second Friday, so its Friday has one session
        let books = BTreeMap::from([(d(1), held(&["A"])), (d(4), held(&["A", "B"])), (d(8), held(&["Z"])), (d(11), held(&["B"]))]);
        // Friday's pond 09:30-14:30 pays at t ≥ 3 in both halves but its slice lost to the average day, and
        // Monday's pond 09:30-16:00 pays at t ≥ 3 but not in its first half
        let daily = BTreeMap::from([(("Mon", "pond"), true), (("Mon", "book"), true), (("Fri", "pond"), false)]);
        assert_eq!(
            calendar_report("US", &names, &books, d(15), 50.0, &daily),
            "  US: 3 names, 4 sessions 2026-01-02..2026-01-12; cost 50 bp a round trip; bar: net > 0, t ≥ 3, both halves net > 0, the slice beat the average day on 10y daily\
             \n  US pond Mon 09:30-14:30  2 sessions  gross +111.67 bp (t +9.57)  net +6.17 €/session  halves +100.00|+123.33  <- best  <- clears the bar\
             \n  US pond Mon 09:30-16:00  2 sessions  gross +55.83 bp (t +5.15)  net +0.58 €/session  halves +45.00|+66.67\
             \n  US pond Mon 14:30-16:00  2 sessions  gross -55.24 bp (t -73.64)  net -10.52 €/session  halves -54.49|-55.99\
             \n  US book Mon 09:30-14:30  2 sessions  gross +105.00 bp (t +21.00)  net +5.50 €/session  halves +100.00|+110.00  <- best  <- clears the bar\
             \n  US book Mon 09:30-16:00  2 sessions  gross +22.50 bp (t +1.00)  net -2.75 €/session  halves +45.00|+0.00\
             \n  US book Mon 14:30-16:00  2 sessions  gross -81.64 bp (t -3.01)  net -13.16 €/session  halves -54.49|-108.80\
             \n  US pond Tue  no two sessions\
             \n  US book Tue  no two sessions\
             \n  US pond Wed  no two sessions\
             \n  US book Wed  no two sessions\
             \n  US pond Thu  no two sessions\
             \n  US book Thu  no two sessions\
             \n  US pond Fri 09:30-14:30  2 sessions  gross +85.00 bp (t +17.00)  net +3.50 €/session  halves +80.00|+90.00  <- best\
             \n  US pond Fri 09:30-16:00  2 sessions  gross +60.83 bp (t +1.33)  net +1.08 €/session  halves +15.00|+106.67\
             \n  US pond Fri 14:30-16:00  2 sessions  gross -24.01 bp (t -0.59)  net -7.40 €/session  halves -64.51|+16.49\
             \n  US book Fri  no two sessions\
             \n  US pond month turn  no two sessions\
             \n  US book month turn  no two sessions\
             \n  US pond rest of month  no two sessions\
             \n  US book rest of month  no two sessions\
             \n  US pond pre-holiday  no two sessions\
             \n  US book pre-holiday  no two sessions\
             \n  US pond expiry  no two sessions\
             \n  US book expiry  no two sessions\
             \n  US: 2 of 9 slice pairs clear the bar"
        );
        assert_eq!(calendar_report("US", &names, &books, d(4), 50.0, &daily), "  US: no two sessions with 60m bars");
        assert_eq!(calendar_report("EU", &[], &BTreeMap::new(), d(15), 20.0, &daily), "  EU: no two sessions with 60m bars");
    }

    /// (#1216) Twelve names that open at 1 every session and close at 1 + n g / 1024 on the last 21, every
    /// return exact in binary, so the Wednesday to Friday slices tie the average day to the last bit.
    #[test]
    fn daily_slices_grade_each_slice_open_to_close_against_every_session() {
        let names: Vec<String> = (0..12).map(|n| format!("N{n:02}")).collect();
        // bars 30 to 50 run from Saturday 01-31 to Friday 02-20: a weekend carries no weekday tag, both months
        // are edges, and 02-20 is the one expiry
        let g = [8.0, 8.0, 9.0, 5.0, 6.0, 4.0, 9.0, 8.0, 8.0, 10.0, 6.0, 8.0, 12.0, 7.0, 8.0, 8.0, 11.0, 7.0, 10.0, 8.0, 8.0];
        let charts: Vec<Chart> = (0..12)
            .map(|n| {
                let mut c = [1.0 + n as f64 / 1000.0; 51];
                for (j, x) in g.iter().enumerate() {
                    c[30 + j] = 1.0 + n as f64 * x / 1024.0;
                }
                chart(&[1.0; 51], &c, 1e7)
            })
            .collect();
        let pond: Vec<Member> = names.iter().zip(&charts).map(|(t, c)| (t.as_str(), c, 1.0, None)).collect();
        let (lines, beat) = daily_slices("US", 3, &pond, d(51), 50.0);
        assert_eq!(
            lines,
            "  US daily pond Mon open-close  3 sessions  gross +537.11 bp (t +17.32)  net +48.71 €/session  halves +483.40|+563.96  vs every session +107.42 bp\
             \n  US daily pond Tue open-close  3 sessions  gross +322.27 bp (t +10.39)  net +27.23 €/session  halves +268.55|+349.12  vs every session -107.42 bp\
             \n  US daily pond Wed open-close  3 sessions  gross +429.69 bp (t +6.93)  net +37.97 €/session  halves +322.27|+483.40  vs every session +0.00 bp\
             \n  US daily pond Thu open-close  3 sessions  gross +429.69 bp (t +3.46)  net +37.97 €/session  halves +214.84|+537.11  vs every session +0.00 bp\
             \n  US daily pond Fri open-close  3 sessions  gross +429.69 bp (t +13.86)  net +37.97 €/session  halves +483.40|+402.83  vs every session +0.00 bp\
             \n  US daily pond month turn open-close  no two sessions\
             \n  US daily pond rest of month open-close  no two sessions\
             \n  US daily pond pre-holiday open-close  no two sessions\
             \n  US daily pond expiry open-close  no two sessions\
             \n  US daily book Mon open-close  3 sessions  gross +634.77 bp (t +17.32)  net +58.48 €/session  halves +571.29|+666.50  vs every session +126.95 bp\
             \n  US daily book Tue open-close  3 sessions  gross +380.86 bp (t +10.39)  net +33.09 €/session  halves +317.38|+412.60  vs every session -126.95 bp\
             \n  US daily book Wed open-close  3 sessions  gross +507.81 bp (t +6.93)  net +45.78 €/session  halves +380.86|+571.29  vs every session +0.00 bp\
             \n  US daily book Thu open-close  3 sessions  gross +507.81 bp (t +3.46)  net +45.78 €/session  halves +253.91|+634.77  vs every session +0.00 bp\
             \n  US daily book Fri open-close  3 sessions  gross +507.81 bp (t +13.86)  net +45.78 €/session  halves +571.29|+476.07  vs every session +0.00 bp\
             \n  US daily book month turn open-close  no two sessions\
             \n  US daily book rest of month open-close  no two sessions\
             \n  US daily book pre-holiday open-close  no two sessions\
             \n  US daily book expiry open-close  no two sessions"
        );
        // a tie with the average day does not beat it
        assert_eq!(beat.len(), 18);
        assert_eq!(beat.iter().filter(|x| *x.1).map(|x| *x.0).collect::<Vec<_>>(), [("Mon", "book"), ("Mon", "pond")]);
    }
}
