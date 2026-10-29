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
//! (#825) The table's RIGHT/FAIL column counts from the date in `SINCE_FILE`, which the Pages
//! workflow's `since` input sets; (#827) every session of each book name since then, on its own bars.
//! (#1196) Its GAIN/LOSS column adds up the same sessions' % moves, up days against down days, and the
//! rows sort on it, since a name can close up on most days and still lose.
//! (#1212) Two more columns sell the same sessions at a set hour, `intraday_exit_us` New York time and
//! `intraday_exit_eu` Frankfurt time, on each book name's 60m bars; `backtest N intraday` ends on the
//! hours census the two knobs are picked from, (#1214) every buy hour and later sell hour. Display only,
//! like the rest of the table. (#1215) The census then holds past the session: a buy on one session and a
//! sell up to `HOLDS` sessions on, at every hour pair on the 60m bars and open to close on the daily ones.
//! (#1216) Last, the same-session hour pairs split by calendar slice: weekday, month turn, pre-holiday, expiry.
//! (#1221) The page calls the table Trading: each row also prices the same sessions held 21 at a time, in on a
//! 200-close trend and bought once, names the best of the four, and the rows sort on it.

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
/// (#825) The one-line file holding the date the RIGHT/FAIL column counts from, `YYYY-MM-DD`.
pub const SINCE_FILE: &str = ".intraday_since";

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

/// (#1221) The page's TREND trades on bars `s..e`: in on each bar whose previous close sits above the mean of
/// the 200 closes up to it, bought at the first such bar's open and sold at the open of the first bar out,
/// or at bar `e - 1`'s close when still in. One return a spell; a bar with fewer than 200 closes behind it is out.
/// shortcut: on a chart that starts under 200 sessions before `s` the first bars read out; `chart_range`
/// reaches far enough for K7's 2y chart, a wider reach would fix any other key.
fn trend(c: &Chart, s: usize, e: usize) -> Vec<f64> {
    let up = |j: usize| j.checked_sub(200).is_some_and(|a| c.closes[j - 1] > mean(&c.closes[a..j]));
    let ins: Vec<usize> = (s..e).filter(|&j| up(j)).collect();
    ins.chunk_by(|a, b| b - a == 1)
        .filter_map(|run| {
            let out = run[run.len() - 1] + 1;
            Some(pos(if out < e { c.opens[out] } else { c.closes[e - 1] })? / pos(c.opens[run[0]])? - 1.0)
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

/// (#811) The page rows for one market's [`rank`] book: position, market, ticker, name, key, the
/// session's hours on the exchanges' own clock, and the last completed bar the key read. (#824) Each
/// row's latest price in euros at `fx`, the market's euro rate (one currency per market), and K1's
/// [`tug`] sums as the KEY hover; (#832) K7's count of up sessions. (#825) The rows in order of hit share, (right + 1) / (right + fail + 2),
/// ties in key order. (#826) That share as the SCORE cell, and DATA TO as the window the record counts,
/// `since` to the last bar before `until`. (#827) The record is every session in that window on the
/// name's own bars: right = it closed above its open, before costs, fail = at or below it. (#1196) The
/// rows now sort on GAIN/LOSS instead: the window's up moves and down moves, each session's % change
/// added up, ordered by up / (up + down), ties in key order; a name with no move at all sorts last. Its
/// hover adds the averages and both sums net of the market's round trip, `COST_BPS_US` or `COST_BPS_EU`.
/// (#1206) P/L(EUR) is the same sessions in euros: €1,000 bought at each open and sold at the close,
/// each % change times €1,000 added up after that round trip; its hover adds the figure before costs.
/// (#1207) The hover says the stake is a fresh €1,000 each session and that every session of the name
/// counts, not only the days the table listed it. (#1208) It also gives the average a session, before
/// and after costs. (#1221) Three more ways to trade the same sessions, each €1,000 a round trip after the
/// same cost: HOLD 21 back to back from the first open ([`held_return`] over 21 sessions, the last one cut
/// at the window's end), TREND ([`trend`]) and BUY&HOLD (the first open to the last close). BEST names the
/// highest of those three and P/L(EUR), and the rows now sort on its euros, ties in key order.
pub fn web_rows(market: &str, k: u8, book: &[(&str, f64, &Chart)], since: NaiveDate, until: NaiveDate, fx: f64, today: NaiveDate) -> Vec<Vec<serde_json::Value>> {
    use serde_json::json;
    let hours = if market == "US" { "09:30-16:00 New York" } else { "09:00-17:30 Frankfurt" };
    let cost = if market == "US" { COST_BPS_US } else { COST_BPS_EU };
    let share = |(r, f): (usize, usize)| (r + 1) as f64 / (r + f + 2) as f64;
    let net = |rs: &[f64]| rs.iter().map(|r| (r - cost / 1e4) * 1000.0).sum::<f64>();
    let mut v: Vec<_> = book
        .iter()
        .map(|b| {
            let (s, e) = (completed(b.2, since), completed(b.2, until));
            let xs: Vec<f64> = (s..e).filter_map(|i| session_return(b.2, i)).collect();
            let ud = gains(&xs, 0.0);
            let holds: Vec<f64> = (s..e).step_by(21).filter_map(|j| held_return(b.2, j, (e - 1 - j).min(20))).collect();
            let bh = e.checked_sub(s + 1).and_then(|h| held_return(b.2, s, h));
            let (nu, nd) = gains(&xs, cost / 1e4);
            let spells = trend(b.2, s, e);
            let mut best = vec![("P/L(EUR)", (nu - nd) * 1000.0), ("HOLD 21", net(&holds)), ("TREND", net(&spells))];
            best.extend(bh.map(|r| ("BUY&HOLD", net(&[r]))));
            best.sort_by(|a, b| b.1.total_cmp(&a.1)); // ties keep this order
            (b, xs, ud, (s, e, holds, spells, bh), best)
        })
        .collect();
    v.sort_by(|a, b| b.4[0].1.total_cmp(&a.4[0].1));
    let ratio = |a: f64, b: f64| if b > 0.0 { format!("{:.2}", a / b) } else { "—".into() };
    let avg = |s: f64, n: usize, sign: &str| if n > 0 { format!("{sign}{:.2}%", s * 100.0 / n as f64) } else { "—".into() };
    let eur = |x: f64| format!("{}€{}", if x < 0.0 { "-" } else { "+" }, core::fmt_money2(x.abs()));
    let trips = |rs: &[f64]| {
        let (n, fee) = (rs.len(), core::fmt_money2(rs.len() as f64 * cost / 10.0));
        format!("{} before costs; the backtest's {cost:.0} bp a round trip costs {n} × €{} = €{fee}; {} after", eur(rs.iter().sum::<f64>() * 1000.0), core::fmt_money2(cost / 10.0), eur(net(rs)))
    };
    v.into_iter()
        .enumerate()
        .map(|(n, ((tk, x, c), xs, (u, d), (s, e, holds, spells, bh), best))| {
            let r = xs.iter().filter(|&&s| s > 0.0).count();
            let f = xs.len() - r;
            let (nu, nd) = gains(&xs, cost / 1e4);
            let per = |x: f64| eur(x * 1000.0 / xs.len().max(1) as f64);
            let gain = format!(
                "{r} up days added up {:+.1}% (avg {}); {f} down or flat days -{:.1}% (avg {}); since {since}, open to close, before costs\ngain factor {} = up ÷ down = ({r}/{f} days) × ({}/{} avg move)\nafter the backtest's {cost:.0} bp a round trip: {:+.1}%/-{:.1}%, gain factor {}\n(each session's % change added up, not compounded)",
                u * 100.0,
                avg(u, r, "+"),
                d * 100.0,
                avg(d, f, "-"),
                ratio(u, d),
                avg(u, r, ""),
                avg(d, f, ""),
                nu * 100.0,
                nd * 100.0,
                ratio(nu, nd)
            );
            // shortcut: the euro stake earns the session's % change in the listing's currency, so a US row
            // leaves out the euro's move inside the session; counting it would need EUR/USD bars per session
            let pl = format!(
                "€1,000 bought at the open and sold at the close, each of the {} sessions since {since}: {} before costs; the backtest's {cost:.0} bp a round trip costs {} × €{} = €{}; {} after, about {} a session ({} before costs)\n(a fresh €1,000 each session, not compounded: a gain is not reinvested, a loss is topped back up; every session of this name, not only the days the table listed it; a US row leaves out the euro's move inside the session)",
                r + f,
                eur((u - d) * 1000.0),
                r + f,
                core::fmt_money2(cost / 10.0),
                core::fmt_money2((r + f) as f64 * cost / 10.0),
                eur((nu - nd) * 1000.0),
                per(nu - nd),
                per(u - d)
            );
            let tip = match tug(c, completed(c, today)).map(|(d, n)| (d * 100.0, n * 100.0)) {
                Some((d, n)) if k == 1 => format!(
                    "K1 tug of war, last 21 sessions:\nday moves (open to close) added up: {d:+.2}%\nnight moves (close to next open) added up: {n:+.2}%\nkey = day - night = {d:+.2} - ({n:+.2}) = {:+.2}%\nhigh = it rose while the exchange was open and fell while it was shut\n(each move is a log return, so they add exactly; a small move is about its % change)",
                    x * 100.0
                ),
                _ if k == 7 => format!("K7 hit 1y, last 252 sessions:\n{:.0} closed above their open = {:.2}%\n(a session that closed at its open counts as not up)", x * 252.0, x * 100.0),
                _ => KEYS[usize::from(k) - 1].to_string(),
            };
            vec![
                json!(["RANK", (n + 1).to_string()]),
                json!(["MARKET", market]),
                json!(["TICKER", tk]),
                json!(["NAME", c.name]),
                json!(["SECTOR", "n/a"]), // (#1204) `picks::stamp_site` names it off the row's quote
                json!(["PRICE(EUR)", format!("€{}", core::fmt_money2(c.closes[c.closes.len() - 1] * fx))]),
                json!(["KEY", format!("{:+.2}%", x * 100.0), null, tip]),
                json!(["SESSION", hours]),
                json!(["DATA TO", format!("{since} – {}", c.dates[completed(c, until) - 1])]),
                json!(["RIGHT/FAIL", format!("{r}/{f}"), null, format!("{r} right, {f} failed since {since}: all {} sessions, right = closed above its open", r + f)]),
                json!(["GAIN/LOSS", format!("{:+.1}%/-{:.1}%", u * 100.0, d * 100.0), null, gain]),
                json!(["P/L(EUR)", eur((nu - nd) * 1000.0), null, pl]),
                json!(["HOLD 21", eur(net(&holds)), null, format!("€1,000 bought at an open and sold at the close of the 21st session, then bought again at the next open, back to back over the same {} sessions; the last hold ends on the last one: {} holds, {}\n(a fresh €1,000 each hold, not compounded; hindsight, as P/L(EUR) is)", r + f, holds.len(), trips(&holds))]),
                json!(["TREND", eur(net(&spells)), null, format!("In while the last close sits above the average of the last 200 closes: bought at the next open, sold at the next open after a close under it, or at the last close if still in, over the same {} sessions: {} round trips, {}\n(a fresh €1,000 each round trip, not compounded; 0 round trips = it stayed out and made nothing)", r + f, spells.len(), trips(&spells))]),
                match bh {
                    Some(h) => json!(["BUY&HOLD", eur(net(&[h])), null, format!("€1,000 bought at the open on {} and sold at the close on {}, one round trip: {}", c.dates[s], c.dates[e - 1], trips(&[h]))]),
                    None => json!(["BUY&HOLD", "—"]),
                },
                json!(["BEST", format!("{} {}", best[0].0, eur(best[0].1)), null, format!("The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\n{}\n(hindsight: every column counts sessions that are over; the rows sort on this column)", best.iter().map(|(h, x)| format!("{h} {}", eur(*x))).collect::<Vec<_>>().join("\n"))]),
                json!(["SCORE", format!("{:.0}%", share((r, f)) * 100.0)]),
            ]
        })
        .collect()
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

/// (#825) The date in `SINCE_FILE`, or `today` when the file is missing or holds no date.
pub fn since(today: NaiveDate) -> NaiveDate {
    std::fs::read_to_string(crate::config::data_path(SINCE_FILE)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(today)
}

/// (#825) The shortest chart range that reaches 60 days before `since`: the floor's 30 sessions and
/// K1's 22 before the first session counted. (#832) And far enough back for key `k`'s own window before
/// today, at 3 calendar days per 2 sessions: on a 1y chart a US name has about 250 completed sessions,
/// short of K7's 252, so every US key was None and the US rows dropped.
pub fn chart_range(since: NaiveDate, today: NaiveDate, k: u8) -> &'static str {
    let window: i64 = match k {
        4 | 9 => 63,
        5 | 7 | 12 => 253,
        6 | 8 => 505,
        _ => 22,
    };
    let need = ((today - since).num_days() + 60).max(window * 3 / 2);
    [(91, "3mo"), (182, "6mo"), (365, "1y"), (730, "2y"), (1826, "5y")].into_iter().find(|(d, _)| need <= *d).map_or("10y", |(_, r)| r)
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

/// (#1212) The header hover for `market`'s exit column.
pub fn exit_help(market: &str, exit: u32) -> String {
    let (clock, other) = if market == "US" { ("New York", "EU") } else { ("Frankfurt", "US") };
    format!(
        "The same sessions as P/L(EUR), sold at {:02}:{:02} {clock} time instead of at the close: €1,000 bought at each open, every gain and loss added up after the round trip, from Yahoo's 60m bars, so a half day drops out; {market} rows only, — on {other} rows; PT = the same moments on Portugal's clock this week. Hindsight, as P/L(EUR) is; hover a cell for the same sessions sold at the close. The hour is the intraday_exit_{} knob: buying at the open and selling then was the best pair in a 2-year census of every buy hour and later sell hour (receipt #1214), yet every pair lost money after costs, and none beat open to close by a significant margin",
        exit / 60,
        exit % 60,
        market.to_lowercase()
    )
}

/// (#1212) Two cells on each row, after P/L(EUR): each `exits` market's P/L sold at its exit hour, in euros
/// after the round trip, over the sessions in `since..until` that the row's 60m bars in `hourly` carry in
/// full. A row of the other market reads —, and so does a name with no such session. The hover puts the
/// same sessions sold at the close beside it.
pub fn stamp_exits(rows: &mut [Vec<serde_json::Value>], hourly: &HashMap<String, Vec<(i64, f64, f64)>>, exits: &[(&str, u32)], since: NaiveDate, until: NaiveDate, today: NaiveDate) {
    use serde_json::json;
    let eur = |x: f64| format!("{}€{}", if x < 0.0 { "-" } else { "+" }, core::fmt_money2(x.abs()));
    for row in rows.iter_mut() {
        let (market, tk) = (row[1][1].as_str().unwrap_or("").to_string(), row[2][1].as_str().unwrap_or(""));
        let days = hourly.get(tk).map(|b| day_moves(&market, b)).unwrap_or_default();
        let cells: Vec<serde_json::Value> = exits
            .iter()
            .map(|&(m, exit)| {
                let head = exit_head(m, exit, today);
                let close = session(m).1;
                let xs: Vec<(f64, f64)> = days.iter().filter(|(d, _)| (since..until).contains(*d)).filter_map(|(_, e)| Some((*e.get(&exit)?, *e.get(&close)?))).collect();
                if m != market || xs.is_empty() {
                    return json!([head, "—"]);
                }
                let (n, cost) = (xs.len(), if m == "US" { COST_BPS_US } else { COST_BPS_EU });
                let fee = n as f64 * cost / 10.0;
                let (gross, at_close) = (xs.iter().map(|x| x.0).sum::<f64>() * 1000.0, xs.iter().map(|x| x.1).sum::<f64>() * 1000.0);
                let tip = format!(
                    "€1,000 bought at the open and sold at {:02}:{:02} {} time, each of the {n} sessions since {since} on this name's 60m bars: {} before costs; the backtest's {cost:.0} bp a round trip costs {n} × €{} = €{}; {} after, about {} a session\nsold at the close instead, the same {n} sessions: {} after costs, so this hour made {} against the close\n(hindsight, as P/L(EUR) is: the key picked these names from these sessions; a half day, or a session missing its first or last bar, drops out, so the count can sit under P/L(EUR)'s)",
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
                json!([head, eur(gross - fee), null, tip])
            })
            .collect();
        let at = row.iter().position(|c| c[0] == "P/L(EUR)").map_or(row.len(), |i| i + 1);
        row.splice(at..at, cells);
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
        // (#827) every session from bar 1 (2026-01-02) to bar 30: ACME's 22 up in 30 (its four flat bars, 4, 12,
        // 22 and 23, are fails); B never fell, and its hover prints — for every ratio over its zero losses;
        // (#1221) ACME's BEST, P/L(EUR) +€1,046.85, tops B's +€150.00, so it ranks first though (#1196)
        // GAIN/LOSS would put B first; HOLD 21 is bars 1-21 then 22-30, and 31 bars leave TREND no 200 closes
        let rows = web_rows("US", 3, &[("ACME", 0.01234, &c), ("B", -0.5, &up)], d(1), d(31), 2.0, d(31));
        assert_eq!(
            serde_json::json!(rows),
            serde_json::json!([
                [["RANK", "1"], ["MARKET", "US"], ["TICKER", "ACME"], ["NAME", "Acme"], ["SECTOR", "n/a"], ["PRICE(EUR)", "€206.00"], ["KEY", "+1.23%", null, "K3 5d intraday"], ["SESSION", "09:30-16:00 New York"], ["DATA TO", "2026-01-02 – 2026-01-31"], ["RIGHT/FAIL", "22/8", null, "22 right, 8 failed since 2026-01-02: all 30 sessions, right = closed above its open"], ["GAIN/LOSS", "+124.5%/-4.8%", null, "22 up days added up +124.5% (avg +5.66%); 8 down or flat days -4.8% (avg -0.60%); since 2026-01-02, open to close, before costs\ngain factor 25.80 = up ÷ down = (22/8 days) × (5.66%/0.60% avg move)\nafter the backtest's 50 bp a round trip: +113.5%/-8.8%, gain factor 12.86\n(each session's % change added up, not compounded)"], ["P/L(EUR)", "+€1,046.85", null, "€1,000 bought at the open and sold at the close, each of the 30 sessions since 2026-01-02: +€1,196.85 before costs; the backtest's 50 bp a round trip costs 30 × €5.00 = €150.00; +€1,046.85 after, about +€34.89 a session (+€39.89 before costs)\n(a fresh €1,000 each session, not compounded: a gain is not reinvested, a loss is topped back up; every session of this name, not only the days the table listed it; a US row leaves out the euro's move inside the session)"], ["HOLD 21", "+€89.01", null, "€1,000 bought at an open and sold at the close of the 21st session, then bought again at the next open, back to back over the same 30 sessions; the last hold ends on the last one: 2 holds, +€99.01 before costs; the backtest's 50 bp a round trip costs 2 × €5.00 = €10.00; +€89.01 after\n(a fresh €1,000 each hold, not compounded; hindsight, as P/L(EUR) is)"], ["TREND", "+€0.00", null, "In while the last close sits above the average of the last 200 closes: bought at the next open, sold at the next open after a close under it, or at the last close if still in, over the same 30 sessions: 0 round trips, +€0.00 before costs; the backtest's 50 bp a round trip costs 0 × €5.00 = €0.00; +€0.00 after\n(a fresh €1,000 each round trip, not compounded; 0 round trips = it stayed out and made nothing)"], ["BUY&HOLD", "+€14.80", null, "€1,000 bought at the open on 2026-01-02 and sold at the close on 2026-01-31, one round trip: +€19.80 before costs; the backtest's 50 bp a round trip costs 1 × €5.00 = €5.00; +€14.80 after"], ["BEST", "P/L(EUR) +€1,046.85", null, "The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\nP/L(EUR) +€1,046.85\nHOLD 21 +€89.01\nBUY&HOLD +€14.80\nTREND +€0.00\n(hindsight: every column counts sessions that are over; the rows sort on this column)"], ["SCORE", "72%"]],
                [["RANK", "2"], ["MARKET", "US"], ["TICKER", "B"], ["NAME", "Up"], ["SECTOR", "n/a"], ["PRICE(EUR)", "€202.00"], ["KEY", "-50.00%", null, "K3 5d intraday"], ["SESSION", "09:30-16:00 New York"], ["DATA TO", "2026-01-02 – 2026-01-31"], ["RIGHT/FAIL", "30/0", null, "30 right, 0 failed since 2026-01-02: all 30 sessions, right = closed above its open"], ["GAIN/LOSS", "+30.0%/-0.0%", null, "30 up days added up +30.0% (avg +1.00%); 0 down or flat days -0.0% (avg —); since 2026-01-02, open to close, before costs\ngain factor — = up ÷ down = (30/0 days) × (1.00%/— avg move)\nafter the backtest's 50 bp a round trip: +15.0%/-0.0%, gain factor —\n(each session's % change added up, not compounded)"], ["P/L(EUR)", "+€150.00", null, "€1,000 bought at the open and sold at the close, each of the 30 sessions since 2026-01-02: +€300.00 before costs; the backtest's 50 bp a round trip costs 30 × €5.00 = €150.00; +€150.00 after, about +€5.00 a session (+€10.00 before costs)\n(a fresh €1,000 each session, not compounded: a gain is not reinvested, a loss is topped back up; every session of this name, not only the days the table listed it; a US row leaves out the euro's move inside the session)"], ["HOLD 21", "+€10.00", null, "€1,000 bought at an open and sold at the close of the 21st session, then bought again at the next open, back to back over the same 30 sessions; the last hold ends on the last one: 2 holds, +€20.00 before costs; the backtest's 50 bp a round trip costs 2 × €5.00 = €10.00; +€10.00 after\n(a fresh €1,000 each hold, not compounded; hindsight, as P/L(EUR) is)"], ["TREND", "+€0.00", null, "In while the last close sits above the average of the last 200 closes: bought at the next open, sold at the next open after a close under it, or at the last close if still in, over the same 30 sessions: 0 round trips, +€0.00 before costs; the backtest's 50 bp a round trip costs 0 × €5.00 = €0.00; +€0.00 after\n(a fresh €1,000 each round trip, not compounded; 0 round trips = it stayed out and made nothing)"], ["BUY&HOLD", "+€5.00", null, "€1,000 bought at the open on 2026-01-02 and sold at the close on 2026-01-31, one round trip: +€10.00 before costs; the backtest's 50 bp a round trip costs 1 × €5.00 = €5.00; +€5.00 after"], ["BEST", "P/L(EUR) +€150.00", null, "The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\nP/L(EUR) +€150.00\nHOLD 21 +€10.00\nBUY&HOLD +€5.00\nTREND +€0.00\n(hindsight: every column counts sessions that are over; the rows sort on this column)"], ["SCORE", "97%"]],
            ])
        );
        // equal BEST keeps key order
        let order: Vec<_> = web_rows("EU", 3, &[("Y", 0.0, &c), ("X", 0.0, &c)], d(1), d(31), 1.0, d(31)).iter().map(|r| r[2][1].as_str().unwrap().to_string()).collect();
        assert_eq!(order, ["Y", "X"]);
        // too few sessions for K1's hover sums: it names the key; (#826) the session dated today closed, so
        // the window ends on it, one bar past the key's last; (#827) bar 0 before `since` and bar 21 dated
        // `until` both closed up, and neither counts
        let eu = web_rows("EU", 1, &[("X", 0.0, &c)], d(1), d(21), 1.0, d(20));
        assert_eq!((eu[0][1][1].as_str(), eu[0][6][3].as_str()), (Some("EU"), Some("K1 tug of war")));
        assert_eq!((eu[0][7][1].as_str(), eu[0][8][1].as_str(), eu[0][9][1].as_str(), eu[0][16][1].as_str()), (Some("09:00-17:30 Frankfurt"), Some("2026-01-02 – 2026-01-21"), Some("16/4"), Some("77%")));
        // (#824) 21 sessions behind it: the hover works the number out
        let k1 = web_rows("EU", 1, &[("X", key(1, &c, 31).unwrap(), &c)], d(1), d(31), 1.0, d(31));
        assert_eq!(k1[0][5][1], "€103.00");
        assert_eq!(k1[0][6][3], "K1 tug of war, last 21 sessions:\nday moves (open to close) added up: +80.00%\nnight moves (close to next open) added up: -82.87%\nkey = day - night = +80.00 - (-82.87) = +162.87%\nhigh = it rose while the exchange was open and fell while it was shut\n(each move is a log return, so they add exactly; a small move is about its % change)");
        // (#832) K7's hover counts the up sessions behind its share
        let k7 = web_rows("EU", 7, &[("X", 0.5, &c)], d(1), d(31), 1.0, d(31));
        assert_eq!(k7[0][6][3], "K7 hit 1y, last 252 sessions:\n126 closed above their open = 50.00%\n(a session that closed at its open counts as not up)");
        // (#452) the page's hover text keys on exactly these headers
        let heads: std::collections::BTreeSet<&str> = rows[0].iter().map(|c| c[0].as_str().unwrap()).collect();
        assert_eq!(crate::picks::web_help(false, &[])["intraday"].keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>(), heads);
    }

    /// (#1196) GAIN/LOSS adds up the moves, not the day count: a name up on two days in three by a little
    /// reads worse than one up on one day in three by a lot. (#1221) The rows sort on BEST: GAP falls every
    /// session but gaps up every night, so it ranks first on BUY&HOLD with the lowest key and no up day; the
    /// three losers tie on TREND's +€0.00, out on 31 bars, and keep key order.
    #[test]
    fn rows_sort_on_the_best_way_to_trade_not_on_gains_or_the_day_count() {
        let o = [100.0; 31];
        // bar 0 sits before `since`; bars 1-30 count
        let small = chart(&o, &[[100.1; 21].as_slice(), &[99.0; 10]].concat(), 1e5);
        let big = chart(&o, &[[103.0; 11].as_slice(), &[99.5; 20]].concat(), 1e5);
        let flat = chart(&o, &o, 1e5);
        let down = chart(&o, &[99.0; 31], 1e5);
        let up: Vec<f64> = (0..31).map(|j| 100.0 + j as f64).collect();
        let gap = chart(&up, &up.iter().map(|x| x - 0.5).collect::<Vec<_>>(), 1e5);
        let book = [("SMALL", 4.0, &small), ("FLAT", 3.0, &flat), ("DOWN", 2.0, &down), ("BIG", 1.0, &big), ("GAP", 0.5, &gap)];
        let rows = web_rows("EU", 3, &book, d(1), d(31), 1.0, d(31));
        let cells: Vec<_> = rows.iter().map(|r| [2, 9, 10, 11, 12, 13, 14, 15].map(|i| r[i][1].as_str().unwrap())).collect();
        assert_eq!(
            cells,
            [
                ["GAP", "0/30", "+0.0%/-13.1%", "-€190.61", "+€250.54", "+€0.00", "+€280.18", "BUY&HOLD +€280.18"],
                ["BIG", "10/20", "+30.0%/-10.0%", "+€140.00", "-€14.00", "+€0.00", "-€7.00", "P/L(EUR) +€140.00"],
                ["SMALL", "20/10", "+2.0%/-10.0%", "-€140.00", "-€24.00", "+€0.00", "-€12.00", "TREND +€0.00"],
                ["FLAT", "0/30", "+0.0%/-0.0%", "-€60.00", "-€4.00", "+€0.00", "-€2.00", "TREND +€0.00"],
                ["DOWN", "0/30", "+0.0%/-30.0%", "-€360.00", "-€24.00", "+€0.00", "-€12.00", "TREND +€0.00"],
            ]
        );
        // the hover's net sums pay the EU's 20 bp a session: BIG's 10 up days keep 2.8% each
        assert!(rows[1][10][3].as_str().unwrap().contains("after the backtest's 20 bp a round trip: +28.0%/-14.0%, gain factor 2.00"));
        // (#1206) €1,000 a session: FLAT never moved, so it reads +€0.00 before costs and pays 30 × €2.00
        assert!(rows[3][11][3].as_str().unwrap().contains(": +€0.00 before costs; the backtest's 20 bp a round trip costs 30 × €2.00 = €60.00; -€60.00 after, about -€2.00 a session (+€0.00 before costs)"));
        // (#1208) BIG's 30 sessions average its +€140.00 and +€200.00
        assert!(rows[1][11][3].as_str().unwrap().contains("+€140.00 after, about +€4.67 a session (+€6.67 before costs)"));
        // (#1221) GAP's one round trip runs from its first open to its last close, and BEST lists all four
        assert_eq!(rows[0][14][3], "€1,000 bought at the open on 2026-01-02 and sold at the close on 2026-01-31, one round trip: +€282.18 before costs; the backtest's 20 bp a round trip costs 1 × €2.00 = €2.00; +€280.18 after");
        assert_eq!(rows[0][15][3], "The best of these ways to trade the name over the same sessions, in euros after costs, highest first:\nBUY&HOLD +€280.18\nHOLD 21 +€250.54\nTREND +€0.00\nP/L(EUR) -€190.61\n(hindsight: every column counts sessions that are over; the rows sort on this column)");
        // a window of one session holds from its open to its close; none leaves BUY&HOLD blank
        assert_eq!(web_rows("EU", 3, &[("GAP", 0.0, &gap)], d(1), d(2), 1.0, d(31))[0][14][1], "-€6.95");
        assert_eq!(web_rows("EU", 3, &[("GAP", 0.0, &gap)], d(1), d(1), 1.0, d(31))[0][14], serde_json::json!(["BUY&HOLD", "—"]));
    }

    /// (#1221) TREND reads the close before each bar against the 200 closes up to it: out on bar 199 (199
    /// closes behind it), in on bars 200 and 201, out on bar 202 after bar 201's close fell under, back in from
    /// bar 203 and still in at the end, so the last spell sells at the last close.
    #[test]
    fn trend_spells_trade_the_next_open_after_the_close_crosses_its_200_mean() {
        let o: Vec<f64> = (0..206).map(|j| 100.0 + j as f64).collect();
        let c = [[100.0; 199].as_slice(), &[120.0, 120.0, 50.0, 120.0, 120.0, 120.0, 120.0]].concat();
        let ch = chart(&o, &c, 1e5);
        assert_eq!(trend(&ch, 199, 206), [o[202] / o[200] - 1.0, c[205] / o[203] - 1.0]);
        assert_eq!(trend(&ch, 199, 202), [c[201] / o[200] - 1.0], "a window that ends in a spell sells at its last close");
        assert!(trend(&chart(&o, &[100.0; 206], 1e5), 199, 206).is_empty(), "a close level with its mean is out");
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

    #[test]
    fn the_chart_range_reaches_60_days_before_since() {
        assert_eq!(chart_range(d(0), d(31), 1), "3mo", "91 days");
        assert_eq!(chart_range(d(0), d(32), 1), "6mo");
        assert_eq!(chart_range(d(1), NaiveDate::from_ymd_opt(2026, 10, 22).unwrap(), 1), "1y", "293 + 60 = 353 days");
        assert_eq!((chart_range(d(0), d(305), 1), chart_range(d(0), d(306), 1)), ("1y", "2y"));
        assert_eq!(chart_range(d(0), d(1766), 1), "5y");
        assert_eq!(chart_range(d(0), d(1767), 1), "10y");
    }

    #[test]
    fn the_chart_range_holds_the_keys_own_window() {
        // (#832) the live case: since 2026-01-02 reads 1y on K1, but K7's 252 sessions need 2y
        let today = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();
        assert_eq!(chart_range(d(1), today, 7), "2y", "253 x 3 / 2 = 379 days");
        let at = |k| chart_range(today, today, k);
        assert_eq!([at(1), at(2), at(3)], ["3mo"; 3], "60 days still rule the short keys");
        assert_eq!([at(4), at(9)], ["6mo"; 2], "63 x 3 / 2 = 94 days");
        assert_eq!([at(5), at(7), at(12)], ["2y"; 3]);
        assert_eq!([at(6), at(8)], ["5y"; 2], "505 x 3 / 2 = 757 days");
        assert_eq!(chart_range(d(0), d(1767), 8), "10y", "the longer reach wins");
    }

    /// Under `cargo test --lib` the data dir is a scratch dir that holds no `SINCE_FILE`.
    #[test]
    fn a_missing_since_file_means_today() {
        assert_eq!(since(d(5)), d(5));
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
        for (market, exit, clock, knob, other) in [("US", 870, "14:30 New York", "us", "EU"), ("EU", 960, "16:00 Frankfurt", "eu", "US")] {
            let h = exit_help(market, exit);
            assert!(h.contains(&format!("sold at {clock} time")) && h.contains(&format!("{market} rows only, — on {other} rows")) && h.contains(&format!("intraday_exit_{knob} knob")), "{h}");
        }
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
    fn stamp_exits_puts_each_market_sold_at_its_hour_after_pl() {
        use serde_json::json;
        let row = |market: &str, tk: &str| vec![json!(["RANK", "1"]), json!(["MARKET", market]), json!(["TICKER", tk]), json!(["P/L(EUR)", "+€1.00"]), json!(["SCORE", "50%"])];
        let mut rows = vec![row("US", "AAA"), row("EU", "EEE"), row("US", "BBB")];
        let us = |m, d, h| utc(2026, m, d, h, 30);
        // 09:30 New York is 13:30 UTC before 2026-11-01 and 14:30 after; the first and last sessions sit outside since..until
        let hourly = HashMap::from([
            (
                "AAA".to_string(),
                [
                    session_bars(us(10, 29, 13), 100.0, &[(0, 100.0), (4, 150.0), (6, 150.0)]),
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
        stamp_exits(&mut rows, &hourly, &[("US", 870), ("EU", 960)], on(10, 30), on(11, 3), on(11, 2));
        let (us_head, eu_head) = ("P/L 09:30-14:30 NY (14:30-19:30 PT)", "P/L 09:00-16:00 FRA (08:00-15:00 PT)");
        let hindsight = "(hindsight, as P/L(EUR) is: the key picked these names from these sessions; a half day, or a session missing its first or last bar, drops out, so the count can sit under P/L(EUR)'s)";
        let tip_us = format!("€1,000 bought at the open and sold at 14:30 New York time, each of the 2 sessions since 2026-10-30 on this name's 60m bars: +€30.00 before costs; the backtest's 50 bp a round trip costs 2 × €5.00 = €10.00; +€20.00 after, about +€10.00 a session\nsold at the close instead, the same 2 sessions: +€30.00 after costs, so this hour made -€10.00 against the close\n{hindsight}");
        let tip_eu = format!("€1,000 bought at the open and sold at 16:00 Frankfurt time, each of the 1 sessions since 2026-10-30 on this name's 60m bars: +€20.00 before costs; the backtest's 20 bp a round trip costs 1 × €2.00 = €2.00; +€18.00 after, about +€18.00 a session\nsold at the close instead, the same 1 sessions: -€22.00 after costs, so this hour made +€40.00 against the close\n{hindsight}");
        let score = json!(["SCORE", "50%"]);
        assert_eq!(rows[0][4..], [json!([us_head, "+€20.00", null, tip_us]), json!([eu_head, "—"]), score.clone()]);
        assert_eq!(rows[1][4..], [json!([us_head, "—"]), json!([eu_head, "+€18.00", null, tip_eu]), score.clone()]);
        // a name with no 60m bars
        assert_eq!(rows[2][4..], [json!([us_head, "—"]), json!([eu_head, "—"]), score]);
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
