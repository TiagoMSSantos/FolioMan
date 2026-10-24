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
//!
//! The pond is stocks with a 30-session average turnover (close × volume, in EUR) of at least
//! `MIN_TURNOVER_EUR`: the US side is the S&P 500 as it stood each day, the EU side the static
//! `EU_POND`. The book is the top `BOOK` by key, equal weight, net of a round-trip cost per market.
//! `backtest N intraday` grades every key on 10y of daily bars; nothing here reaches a lane, BUY NOW or
//! the 20y backtest. (#811) With `intraday_key` set, `screen` prints today's top `BOOK` per market as the
//! page's Intraday table, a shadow, and journals it; `track` grades each journalled book on its session.
//! (#825) The table's RIGHT/FAIL column counts from the date in `SINCE_FILE`, which the Pages
//! workflow's `since` input sets; (#827) every session of each book name since then, on its own bars.

use crate::core;
use crate::fetch::Chart;
use chrono::{NaiveDate, NaiveTime, Timelike};
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
pub const KEYS: [&str; 11] = [
    "K1 tug of war", "K2 calm range", "K3 5d intraday", "K4 hit share", "K5 tug 1y", "K6 tug 2y", "K7 hit 1y", "K8 hit 2y", "K9 day Sharpe",
    "K10 low volume shock", "K11 high volume shock",
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
/// K11 are None when any of their 21 volume prints is missing or zero.
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
        10 | 11 if i >= 21 => {
            let shock = pos(c.volumes[i - 1])? / ((i - 21..i - 1).map(|j| pos(c.volumes[j])).sum::<Option<f64>>()? / 20.0);
            Some(if k == 10 { -shock } else { shock })
        }
        _ => None,
    }
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

/// Each session dated on or after `since` and before `until`, with every member that ranks on key `k`
/// that day as (ticker, key, open-to-close return), in rank order.
fn sessions<'a>(k: u8, pond: &[Member<'a>], since: NaiveDate, until: NaiveDate) -> BTreeMap<NaiveDate, Vec<(&'a str, f64, f64)>> {
    let mut by: BTreeMap<NaiveDate, Vec<(&str, f64, f64)>> = BTreeMap::new();
    for &(tk, c, fx, spans) in pond {
        for i in completed(c, since)..completed(c, until) {
            if spans.is_some_and(|s| !core::sp500_member_at(s, c.dates[i])) || !liquid(c, i, fx) {
                continue;
            }
            if let (Some(score), Some(r)) = (key(k, c, i), session_return(c, i)) {
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
    sessions(k, pond, NaiveDate::MIN, today)
        .into_iter()
        .filter(|(_, v)| v.len() >= BOOK)
        .map(|(date, v)| {
            let rs: Vec<f64> = v.iter().map(|x| x.2).collect();
            let up = |xs: &[f64]| xs.iter().filter(|&&r| r > 0.0).count() as f64 / xs.len() as f64;
            Day { date, n: v.len(), book: mean(&rs[..BOOK]), all: mean(&rs), hit: up(&rs[..BOOK]), all_hit: up(&rs) }
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
/// name's own bars: right = it closed above its open, before costs, fail = at or below it.
pub fn web_rows(market: &str, k: u8, book: &[(&str, f64, &Chart)], since: NaiveDate, until: NaiveDate, fx: f64, today: NaiveDate) -> Vec<Vec<serde_json::Value>> {
    use serde_json::json;
    let hours = if market == "US" { "09:30-16:00 New York" } else { "09:00-17:30 Frankfurt" };
    let share = |(r, f): (usize, usize)| (r + 1) as f64 / (r + f + 2) as f64;
    let tally = |c: &Chart| {
        (completed(c, since)..completed(c, until))
            .filter_map(|i| session_return(c, i))
            .fold((0usize, 0usize), |(r, f), x| if x > 0.0 { (r + 1, f) } else { (r, f + 1) })
    };
    let mut v: Vec<_> = book.iter().map(|b| (b, tally(b.2))).collect();
    v.sort_by(|a, b| share(b.1).total_cmp(&share(a.1)));
    v.into_iter()
        .enumerate()
        .map(|(n, ((tk, x, c), (r, f)))| {
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
                json!(["PRICE(EUR)", format!("€{}", core::fmt_money2(c.closes[c.closes.len() - 1] * fx))]),
                json!(["KEY", format!("{:+.2}%", x * 100.0), null, tip]),
                json!(["SESSION", hours]),
                json!(["DATA TO", format!("{since} – {}", c.dates[completed(c, until) - 1])]),
                json!(["RIGHT/FAIL", format!("{r}/{f}"), null, format!("{r} right, {f} failed since {since}: all {} sessions, right = closed above its open", r + f)]),
                json!(["SCORE", format!("{:.0}%", share((r, f)) * 100.0)]),
            ]
        })
        .collect()
}

/// (#823) Whether `market`'s session dated today is over at `now`, UTC: from 17:00 for EU (17:30
/// Frankfurt is 15:30 UTC in summer time, 16:30 in winter) and 21:30 for US (16:00 New York is 20:00 or
/// 21:00), past the closing auction and a delayed chart either way.
/// shortcut: fixed UTC hours, so in summer time a closed session waits an hour longer than it must;
/// each exchange's time-zone rules would cut that hour.
pub fn closed(market: &str, now: NaiveTime) -> bool {
    (now.hour(), now.minute()) >= if market == "US" { (21, 30) } else { (17, 0) }
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
        5 | 7 => 253,
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
        assert!(key(0, &c, 30).is_none() && key(12, &c, 30).is_none());
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
        // (#827) every session from bar 1 (2026-01-02) to bar 30: B's 30 up outranks ACME's 22 in 30 (its four
        // flat bars, 4, 12, 22 and 23, are fails), though ACME's key is higher
        let rows = web_rows("US", 3, &[("ACME", 0.01234, &c), ("B", -0.5, &up)], d(1), d(31), 2.0, d(31));
        assert_eq!(
            serde_json::json!(rows),
            serde_json::json!([
                [["RANK", "1"], ["MARKET", "US"], ["TICKER", "B"], ["NAME", "Up"], ["PRICE(EUR)", "€202.00"], ["KEY", "-50.00%", null, "K3 5d intraday"], ["SESSION", "09:30-16:00 New York"], ["DATA TO", "2026-01-02 – 2026-01-31"], ["RIGHT/FAIL", "30/0", null, "30 right, 0 failed since 2026-01-02: all 30 sessions, right = closed above its open"], ["SCORE", "97%"]],
                [["RANK", "2"], ["MARKET", "US"], ["TICKER", "ACME"], ["NAME", "Acme"], ["PRICE(EUR)", "€206.00"], ["KEY", "+1.23%", null, "K3 5d intraday"], ["SESSION", "09:30-16:00 New York"], ["DATA TO", "2026-01-02 – 2026-01-31"], ["RIGHT/FAIL", "22/8", null, "22 right, 8 failed since 2026-01-02: all 30 sessions, right = closed above its open"], ["SCORE", "72%"]],
            ])
        );
        // equal hit shares keep key order
        let order: Vec<_> = web_rows("EU", 3, &[("Y", 0.0, &c), ("X", 0.0, &c)], d(1), d(31), 1.0, d(31)).iter().map(|r| r[2][1].as_str().unwrap().to_string()).collect();
        assert_eq!(order, ["Y", "X"]);
        // too few sessions for K1's hover sums: it names the key; (#826) the session dated today closed, so
        // the window ends on it, one bar past the key's last; (#827) bar 0 before `since` and bar 21 dated
        // `until` both closed up, and neither counts
        let eu = web_rows("EU", 1, &[("X", 0.0, &c)], d(1), d(21), 1.0, d(20));
        assert_eq!((eu[0][1][1].as_str(), eu[0][5][3].as_str()), (Some("EU"), Some("K1 tug of war")));
        assert_eq!((eu[0][6][1].as_str(), eu[0][7][1].as_str(), eu[0][8][1].as_str(), eu[0][9][1].as_str()), (Some("09:00-17:30 Frankfurt"), Some("2026-01-02 – 2026-01-21"), Some("16/4"), Some("77%")));
        // (#824) 21 sessions behind it: the hover works the number out
        let k1 = web_rows("EU", 1, &[("X", key(1, &c, 31).unwrap(), &c)], d(1), d(31), 1.0, d(31));
        assert_eq!(k1[0][4][1], "€103.00");
        assert_eq!(k1[0][5][3], "K1 tug of war, last 21 sessions:\nday moves (open to close) added up: +80.00%\nnight moves (close to next open) added up: -82.87%\nkey = day - night = +80.00 - (-82.87) = +162.87%\nhigh = it rose while the exchange was open and fell while it was shut\n(each move is a log return, so they add exactly; a small move is about its % change)");
        // (#832) K7's hover counts the up sessions behind its share
        let k7 = web_rows("EU", 7, &[("X", 0.5, &c)], d(1), d(31), 1.0, d(31));
        assert_eq!(k7[0][5][3], "K7 hit 1y, last 252 sessions:\n126 closed above their open = 50.00%\n(a session that closed at its open counts as not up)");
        // (#452) the page's hover text keys on exactly these headers
        let heads: std::collections::BTreeSet<&str> = rows[0].iter().map(|c| c[0].as_str().unwrap()).collect();
        assert_eq!(crate::picks::web_help(false, &[])["intraday"].keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>(), heads);
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
        assert_eq!([at(5), at(7)], ["2y"; 2]);
        assert_eq!([at(6), at(8)], ["5y"; 2], "505 x 3 / 2 = 757 days");
        assert_eq!(chart_range(d(0), d(1767), 8), "10y", "the longer reach wins");
    }

    /// Under `cargo test --lib` the data dir is a scratch dir that holds no `SINCE_FILE`.
    #[test]
    fn a_missing_since_file_means_today() {
        assert_eq!(since(d(5)), d(5));
    }

    #[test]
    fn a_session_is_closed_from_a_fixed_utc_hour() {
        let at = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert!(!closed("EU", at(16, 59)) && closed("EU", at(17, 0)));
        assert!(!closed("US", at(17, 0)) && !closed("US", at(21, 29)) && closed("US", at(21, 30)));
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
}
