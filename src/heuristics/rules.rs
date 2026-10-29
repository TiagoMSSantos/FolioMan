//! (#1218) THE TRADING-RULES CENSUS: can a timing rule beat buy-and-hold net of cost? `backtest N
//! traderules` prices the rule families with the longest records in the literature on every name, each at
//! 1x, 2x and 3x rebalanced every bar: trend (the close above its value some month-ends back, or above its
//! moving mean), a breakout to a new high close with a trailing stop, and a swing buy after a sharp fall.
//! Each runs on each class's names, the S&P 500 as it stood and as it stands, and the screen book,
//! against buy-and-hold 1x on the same names and days. Out of the market a name earns the 13-week bill
//! rate; above 1x it borrows at that rate plus [`SPREAD`]. A census, not a knob: nothing here reaches a
//! lane, BUY NOW, the page or a trade.

use super::intraday::stats;
use chrono::{Datelike, Duration, Months, NaiveDate};
use std::collections::BTreeMap;

/// What a rule pays a year to borrow, on top of the bill rate, for each unit above 1x.
pub const SPREAD: f64 = 0.025;
/// The leverages every rule runs at.
pub const LEVERAGES: [u32; 3] = [1, 2, 3];

/// A timing rule. The position after each bar's close reads only that close and the ones before it, and
/// the next bar's open trades it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rule {
    /// always in
    Hold,
    /// in while a month's last close is above the one `months` month-ends before; flips only there
    Trend { months: usize },
    /// in while the close is above the mean of the last `bars` closes
    Average { bars: usize },
    /// in on a close above each of the last `bars` closes (`None`: every close since the chart starts),
    /// out on a close `k` volatility units below the peak close since entry
    Breakout { bars: Option<usize>, k: f64 },
    /// in after a fall of `fall` or more over 5 bars, or a 2-bar RSI under `rsi`; out on a close above the
    /// 5-bar mean or `max_bars` bars after entry
    Swing { fall: Option<f64>, rsi: Option<f64>, max_bars: usize },
}

impl Rule {
    pub fn family(&self) -> &'static str {
        match self {
            Rule::Hold => "hold",
            Rule::Trend { .. } => "trend",
            Rule::Average { .. } => "average",
            Rule::Breakout { .. } => "breakout",
            Rule::Swing { .. } => "swing",
        }
    }

    pub fn label(&self) -> String {
        match *self {
            Rule::Hold => "buy-and-hold".to_string(),
            Rule::Trend { months } => format!("trend {months}m"),
            Rule::Average { bars } => format!("above {bars}-bar mean"),
            Rule::Breakout { bars, k } => format!("breakout {} stop {k}", bars.map_or("all-time".to_string(), |b| format!("{b}-bar"))),
            Rule::Swing { fall, rsi, max_bars } => {
                let entry = fall.map_or(String::new(), |f| format!("fall {:.0}%", f * 100.0)) + &rsi.map_or(String::new(), |r| format!("RSI2 < {r}"));
                format!("swing {entry} exit {max_bars}")
            }
        }
    }
}

/// Every setting the daily grid tries: buy-and-hold, then each family's sweep, 47 in all.
pub fn sweep() -> Vec<Rule> {
    let mut v = vec![Rule::Hold];
    v.extend([1, 3, 6, 12].map(|months| Rule::Trend { months }));
    v.extend([50, 100, 150, 200, 250].map(|bars| Rule::Average { bars }));
    for bars in [Some(20), Some(55), Some(126), Some(252), None] {
        v.extend([2.0, 3.0, 5.0, 8.0, 10.0].map(|k| Rule::Breakout { bars, k }));
    }
    v.extend(swing().into_iter().skip(1));
    v
}

/// The monthly grid: buy-and-hold and the trend rules a month-end close can read.
pub fn monthly() -> Vec<Rule> {
    std::iter::once(Rule::Hold).chain([3, 6, 9, 12].map(|months| Rule::Trend { months })).collect()
}

/// The 60m grid: buy-and-hold and every swing rule.
pub fn swing() -> Vec<Rule> {
    let mut v = vec![Rule::Hold];
    for (fall, rsi) in [(Some(0.05), None), (Some(0.10), None), (None, Some(5.0)), (None, Some(10.0))] {
        v.extend([5, 10, 20].map(|max_bars| Rule::Swing { fall, rsi, max_bars }));
    }
    v
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// The volatility unit at bar `i`: the mean move of its last 10 closes, `None` before 10 moves.
/// shortcut: close-only, where the textbook ATR reads each bar's high and low, so daily, monthly and 60m
/// bars share one path; an ATR needs highs and lows on every chart.
fn vol(closes: &[f64], i: usize) -> Option<f64> {
    (i >= 10).then(|| mean(&(i - 9..=i).map(|j| (closes[j] - closes[j - 1]).abs()).collect::<Vec<_>>()))
}

/// The 2-bar RSI at bar `i`: the share of the last 2 moves' total size that went up, 0 to 100, 50 when
/// both are flat. shortcut: plain sums, where Wilder's RSI smooths; a smoothed one needs a warm-up.
fn rsi2(closes: &[f64], i: usize) -> Option<f64> {
    let moves = [closes[i.checked_sub(1)?] - closes[i.checked_sub(2)?], closes[i] - closes[i - 1]];
    let (up, size) = (moves[0].max(0.0) + moves[1].max(0.0), moves[0].abs() + moves[1].abs());
    Some(if size > 0.0 { 100.0 * up / size } else { 50.0 })
}

/// The position `rule` holds after each bar's close, read from `closes[..=i]` and the calendar: a bar is
/// a month's last when the next one is dated in another month.
pub fn positions(rule: Rule, dates: &[NaiveDate], closes: &[f64]) -> Vec<bool> {
    let mut pos = vec![false; closes.len()];
    let (mut on, mut peak, mut entry, mut ends) = (false, 0.0, 0, Vec::new());
    for (i, &c) in closes.iter().enumerate() {
        on = match rule {
            Rule::Hold => true,
            Rule::Trend { months } => {
                if dates.get(i + 1).is_some_and(|d| (d.year(), d.month()) != (dates[i].year(), dates[i].month())) {
                    ends.push(i);
                    ends.len().checked_sub(months + 1).map_or(on, |k| c > closes[ends[k]])
                } else {
                    on
                }
            }
            Rule::Average { bars } => i + 1 >= bars && c > mean(&closes[i + 1 - bars..=i]),
            Rule::Breakout { k, .. } if on => {
                peak = f64::max(peak, c);
                !vol(closes, i).is_some_and(|v| c < peak - k * v)
            }
            Rule::Breakout { bars, .. } => {
                // shortcut: "all-time" is the highest close since the chart starts, 10y on the daily grid
                let lo = bars.map_or(Some(0), |b| i.checked_sub(b));
                let go = lo.is_some_and(|lo| closes[lo..i].iter().all(|p| c > *p)) && vol(closes, i).is_some();
                peak = c;
                go
            }
            Rule::Swing { max_bars, .. } if on => i - entry < max_bars && !(i >= 4 && c > mean(&closes[i - 4..=i])),
            Rule::Swing { fall, rsi, .. } => {
                let fell = fall.is_some_and(|f| i >= 5 && c / closes[i - 5] - 1.0 <= -f);
                let go = fell || rsi.is_some_and(|x| rsi2(closes, i).is_some_and(|v| v < x));
                entry = i;
                go
            }
        };
        pos[i] = on;
    }
    pos
}

/// Each bar's net return for `pos` at leverage `lev`, the first bar's 0. A position set at a close trades
/// at the next open: held through, a bar pays close to close; on entry, open to close; on exit, the last
/// close to the open; both entry and exit pay half of `cost`, the round trip as a fraction, on the
/// levered size. Out, a bar earns the bill rate `cash`, a fraction a year, over its calendar days; in at
/// `lev` above 1, it pays that rate plus [`SPREAD`] on the borrowed part. A bar that loses everything reads
/// -1 and every bar after it NaN: the name is gone.
pub fn returns(dates: &[NaiveDate], opens: &[f64], closes: &[f64], pos: &[bool], lev: u32, cost: f64, cash: &[f64]) -> Vec<f64> {
    let l = f64::from(lev);
    let ratio = |a: f64, b: f64| if a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0 { a / b - 1.0 } else { 0.0 };
    let mut out: Vec<f64> = vec![0.0; closes.len()];
    for i in 1..closes.len() {
        if out[i - 1] <= -1.0 || out[i - 1].is_nan() {
            out[i] = f64::NAN;
            continue;
        }
        let years = (dates[i] - dates[i - 1]).num_days() as f64 / 365.25;
        let leg = match (i >= 2 && pos[i - 2], pos[i - 1]) {
            (true, true) => Some(ratio(closes[i], closes[i - 1])),
            (false, true) => Some(ratio(closes[i], opens[i]) - cost / 2.0),
            (true, false) => Some(ratio(opens[i], closes[i - 1]) - cost / 2.0),
            (false, false) => None,
        };
        out[i] = leg.map_or(cash[i] * years, |r| l * r - (l - 1.0) * (cash[i] + SPREAD) * years).max(-1.0);
    }
    out
}

/// Whether a name's equity over `rs` ever falls 90% or more below its peak.
pub fn ruined(rs: &[f64]) -> bool {
    let (mut e, mut peak) = (1.0, 1.0);
    rs.iter().filter(|r| r.is_finite()).any(|r| {
        e *= 1.0 + r;
        peak = f64::max(peak, e);
        e <= 0.1 * peak
    })
}

/// Per calendar day from a base date: the returns of every name in a set added up, and the name count.
pub type Tally = Vec<(f64, u32)>;

/// Adds a name's returns from bar `from` on to `t`, on the days `member` keeps: bars dated the same day
/// compound into one return, at that day's index from `base`. A gone name's NaN bars add nothing, nor
/// does a day before `base`. Returns the days added.
pub fn add(t: &mut Tally, base: NaiveDate, dates: &[NaiveDate], rs: &[f64], member: impl Fn(NaiveDate) -> bool, from: usize) -> usize {
    let (mut added, mut i) = (0, from);
    while i < rs.len() {
        let (d, mut g, mut any) = (dates[i], 1.0, false);
        while i < rs.len() && dates[i] == d {
            if rs[i].is_finite() {
                g *= 1.0 + rs[i];
                any = true;
            }
            i += 1;
        }
        let Ok(k) = usize::try_from((d - base).num_days()) else { continue };
        if any && member(d) {
            if t.len() <= k {
                t.resize(k + 1, (0.0, 0));
            }
            t[k] = (t[k].0 + g - 1.0, t[k].1 + 1);
            added += 1;
        }
    }
    added
}

/// One printed rule line and what the bar reads off it.
pub struct Score {
    pub text: String,
    /// net %/yr minus buy-and-hold 1x's, `None` under 3 months
    pub excess: Option<f64>,
    pub t: Option<f64>,
    /// each half's mean monthly excess log return, pts/yr
    pub halves: (f64, f64),
}

impl Score {
    /// The bar: beats buy-and-hold, at t ≥ 3, in both halves.
    pub fn clears(&self) -> bool {
        self.excess.is_some_and(|x| x > 0.0) && self.t.is_some_and(|t| t >= 3.0) && self.halves.0 > 0.0 && self.halves.1 > 0.0
    }
}

/// The line for `rule` against `hold`, buy-and-hold 1x on the same names: each day one equal-weight
/// basket, rebalanced daily. Rates compound over the calendar months with a day in them; the t and the
/// halves, split by count, read each month's log return minus buy-and-hold's. The worst drawdown is the
/// basket's; `ruins` of `names` are the names whose own equity fell 90%. "too few" under 3 months.
pub fn score(label: &str, rule: &Tally, hold: &Tally, base: NaiveDate, ruins: usize, names: usize) -> Score {
    let mut months: Vec<((i32, u32), f64, f64)> = Vec::new();
    let (mut e, mut peak, mut dd) = (1.0, 1.0, 0.0);
    for (k, &(hs, hn)) in hold.iter().enumerate().filter(|(_, h)| h.1 > 0) {
        let (s, n) = rule.get(k).copied().unwrap_or_default();
        let r = if n > 0 { s / f64::from(n) } else { 0.0 };
        e *= 1.0 + r;
        peak = f64::max(peak, e);
        dd = f64::max(dd, 1.0 - e / peak);
        let d = base + Duration::days(k as i64);
        let (lr, lh) = ((1.0 + r).max(1e-9).ln(), (1.0 + hs / f64::from(hn)).max(1e-9).ln());
        match months.last_mut() {
            Some(m) if m.0 == (d.year(), d.month()) => {
                m.1 += lr;
                m.2 += lh;
            }
            _ => months.push(((d.year(), d.month()), lr, lh)),
        }
    }
    let mut out = Score { text: format!("  {label}  too few months"), excess: None, t: None, halves: (0.0, 0.0) };
    let ex: Vec<f64> = months.iter().map(|m| m.1 - m.2).collect();
    let Some(s) = stats(&ex).filter(|_| ex.len() >= 3) else { return out };
    let years = months.len() as f64 / 12.0;
    let rate = |ln: f64| ((ln / years).exp() - 1.0) * 100.0;
    let (net, bh) = (rate(months.iter().map(|m| m.1).sum()), rate(months.iter().map(|m| m.2).sum()));
    out.excess = Some(net - bh);
    out.t = Some(s.t).filter(|t| t.is_finite());
    out.halves = (s.h1 * 1200.0, s.h2 * 1200.0);
    out.text = format!(
        "  {label}  {} months  net {net:+.1}%/yr  worst drawdown {:.0}%  vs buy-and-hold {:+.1} pts/yr  t {}  halves {:+.1}|{:+.1}  ruined {ruins}/{names}",
        months.len(),
        dd * 100.0,
        net - bh,
        out.t.map_or("n/a".to_string(), |t| format!("{t:.1}")),
        out.halves.0,
        out.halves.1
    );
    out
}

/// A set's report: its buy-and-hold lines, each family's best setting by excess at each leverage, then
/// how many of `scores` clear the bar.
pub fn report(set: &str, scores: &[(Rule, u32, Score)]) -> String {
    let mut out: Vec<String> = scores.iter().filter(|s| s.0 == Rule::Hold).map(|s| s.2.text.clone()).collect();
    for family in ["trend", "average", "breakout", "swing"] {
        for lev in LEVERAGES {
            let of = scores.iter().filter(|s| s.0.family() == family && s.1 == lev);
            if let Some((s, _)) = of.filter_map(|s| Some((s, s.2.excess?))).max_by(|a, b| a.1.total_cmp(&b.1)) {
                out.push(s.2.text.clone());
            }
        }
    }
    let k = scores.iter().filter(|s| s.2.clears()).count();
    out.push(format!("  {set}: {k} of {} settings × leverage clear the bar (beat buy-and-hold, t ≥ 3, both halves)", scores.len()));
    out.join("\n")
}

/// The biggest `closes[j] / closes[i]` with `j` at most `months` bars after `i` and `start_ok(i)`, as
/// `(i, j, multiple)`; prices not finite and positive never count.
pub fn best_run(closes: &[f64], months: usize, start_ok: impl Fn(usize) -> bool) -> Option<(usize, usize, f64)> {
    let ok = |p: f64| p.is_finite() && p > 0.0;
    let mut best: Option<(usize, usize, f64)> = None;
    for i in (0..closes.len()).filter(|&i| ok(closes[i]) && start_ok(i)) {
        for j in (i + 1..closes.len().min(i + months + 1)).filter(|&j| ok(closes[j])) {
            if best.is_none_or(|b| closes[j] / closes[i] > b.2) {
                best = Some((i, j, closes[j] / closes[i]));
            }
        }
    }
    best
}

/// The share of buy-and-hold's log gain over bars `from..=to` that `rule` kept; a gone name's -1 bar
/// counts as a near-total loss.
pub fn kept(rule: &[f64], hold: &[f64], from: usize, to: usize) -> f64 {
    let ln = |rs: &[f64]| rs[from..=to].iter().filter(|r| r.is_finite()).map(|r| (1.0 + r).max(1e-9).ln()).sum::<f64>();
    ln(rule) / ln(hold)
}

/// The bill rate on each of `dates` as a fraction a year: the last of `irx`, ^IRX's closes in percent, on
/// or before it; 0 before the first.
pub fn cash_on(irx: &[(NaiveDate, f64)], dates: &[NaiveDate]) -> Vec<f64> {
    dates.iter().map(|d| irx.partition_point(|x| x.0 <= *d).checked_sub(1).map_or(0.0, |k| irx[k].1 / 100.0)).collect()
}

/// (#1219) THE LEVERAGE CENSUS: does a daily-reset fund at 1.25x to 3x the S&P 500's total return end
/// richer than 1x, and how often? Two borrow models: a swap ETF that pays the bill rate on the borrowed
/// part and its 0.60%/yr fee on the whole fund (Xtrackers S&P 500 2x Leveraged Daily Swap UCITS), and a
/// margin loan at the bill rate plus [`SPREAD`].
pub const COSTS: [(&str, f64, f64); 2] = [("etf", 0.0, 0.006), ("margin", SPREAD, 0.0)];
/// (#1219) The leverages the census prices.
pub const LEVELS: [f64; 4] = [1.25, 1.5, 2.0, 3.0];

/// (#1219) The value `m`, keyed year * 12 + month0, holds for `d`'s month or the latest before it; 0
/// before the first.
pub fn on_month(m: &BTreeMap<i32, f64>, d: NaiveDate) -> f64 {
    m.range(..=d.year() * 12 + d.month0() as i32).next_back().map_or(0.0, |x| *x.1)
}

/// (#1219) Each bar's total return on `px`'s dates, the first 0. Until `tr` has a close on or before the
/// bar before, a bar pays its price move plus `yld`'s dividend yield for its month over its calendar
/// days; from then on, `tr`'s move between its last closes on or before the two dates.
pub fn splice(px: &[(NaiveDate, f64)], yld: &BTreeMap<i32, f64>, tr: &[(NaiveDate, f64)]) -> Vec<f64> {
    let at = |d: NaiveDate| tr[tr.partition_point(|x| x.0 <= d) - 1].1;
    let moves = px.windows(2).map(|w| {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if tr.first().is_some_and(|t| t.0 <= d0) {
            at(d1) / at(d0) - 1.0
        } else {
            p1 / p0 - 1.0 + on_month(yld, d1) * (d1 - d0).num_days() as f64 / 365.25
        }
    });
    std::iter::once(0.0).chain(moves).collect()
}

/// (#1219) What a fund held at `lev` times 1x returns `rs`, reset every bar, pays each bar: less the
/// bill rate `cash` plus `spread` on the borrowed part and `fee` on the whole fund, each a fraction a
/// year over the bar's calendar days. The first bar's 0, and no bar loses more than everything.
pub fn levered(dates: &[NaiveDate], rs: &[f64], cash: &[f64], lev: f64, spread: f64, fee: f64) -> Vec<f64> {
    let bar = |i: usize| {
        let years = (dates[i] - dates[i - 1]).num_days() as f64 / 365.25;
        (lev * rs[i] - ((lev - 1.0) * (cash[i] + spread) + fee) * years).max(-1.0)
    };
    std::iter::once(0.0).chain((1..rs.len()).map(bar)).collect()
}

/// (#1219) The bars that open a month: the first, and each dated in another month than the one before.
pub fn firsts(dates: &[NaiveDate]) -> Vec<usize> {
    (0..dates.len()).filter(|&i| i == 0 || (dates[i].year(), dates[i].month()) != (dates[i - 1].year(), dates[i - 1].month())).collect()
}

/// (#1219) Every `years`-year window: from each month's first bar to the first bar at least that long
/// after it. A start with no such bar opens none.
pub fn windows(dates: &[NaiveDate], years: u32) -> Vec<(usize, usize)> {
    let end = |s: usize| dates.partition_point(|d| *d < dates[s] + Months::new(12 * years));
    firsts(dates).into_iter().map(|s| (s, end(s))).filter(|w| w.1 < dates.len()).collect()
}

/// (#1219) The deepest fall below a running peak along `le`, a log equity curve, as a fraction.
pub fn drawdown(le: &[f64]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    le.iter().fold(0.0, |dd, &x| {
        peak = peak.max(x);
        f64::max(dd, 1.0 - (x - peak).exp())
    })
}

/// (#1219) Why a 20y line misses the bar, `None` when it clears it: it ends richer than 1x in 80% or
/// more of the windows in each half, its worst window ends no poorer than 1x's worst, and no window
/// falls 90% or more.
pub fn misses(halves: (f64, f64), worst: f64, worst_1x: f64, wipes: usize) -> Option<&'static str> {
    if !(halves.0 >= 0.8 && halves.1 >= 0.8) {
        Some("beats 1x in under 80% of a half")
    } else if worst < worst_1x {
        Some("its worst window ends poorer")
    } else if wipes > 0 {
        Some("a window falls 90%")
    } else {
        None
    }
}

/// (#1219) The census text on 1x total returns `rs` and bill rate `cash`, for each of `horizons` in years:
/// 1x, then each borrow model and leverage, paid in at once on a window's first bar ("lump") or one
/// equal sum on each month's first bar ("monthly"). Lump reads %/yr, monthly the end value over what
/// was paid in. Each line's halves split its windows at `split`; drawdowns and wipe-outs read the fund's
/// own path. Only `horizons[0]` is graded.
pub fn lever_report(dates: &[NaiveDate], rs: &[f64], cash: &[f64], horizons: &[u32], split: NaiveDate) -> String {
    let starts = firsts(dates);
    let mut out = vec![format!(
        "\n── (#1219) LEVERAGE CENSUS: S&P 500 total return from {}, {} bars, rebalanced every bar; 1x has no fee ──",
        dates[0],
        dates.len()
    )];
    let mut clear = 0;
    for (h, &years) in horizons.iter().enumerate() {
        let ws = windows(dates, years);
        // each window's lump multiple, monthly multiple of what was paid in, drawdown, and whether it fell 90%
        let grade = |rs: &[f64]| -> Vec<(f64, f64, f64, bool)> {
            let mut sum = 0.0;
            let le: Vec<f64> = rs.iter().map(|r| {
                sum += (1.0 + r).max(1e-9).ln();
                sum
            }).collect();
            ws.iter().map(|&(s, e)| {
                let ks = &starts[starts.partition_point(|&k| k < s)..starts.partition_point(|&k| k < e)];
                let monthly = ks.iter().map(|&k| (le[e] - le[k]).exp()).sum::<f64>() / ks.len() as f64;
                ((le[e] - le[s]).exp(), monthly, drawdown(&le[s..=e]), ruined(&rs[s + 1..=e]))
            }).collect()
        };
        let base = grade(rs);
        let lines: Vec<(String, Vec<(f64, f64, f64, bool)>)> = COSTS.iter().flat_map(|&(cost, spread, fee)| {
            LEVELS.map(|lev| (format!("{lev}x {cost}"), grade(&levered(dates, rs, cash, lev, spread, fee))))
        }).collect();
        let late: Vec<bool> = ws.iter().map(|w| dates[w.0] >= split).collect();
        let deepest = |g: &[(f64, f64, f64, bool)]| g.iter().map(|x| x.2).fold(0.0, f64::max) * 100.0;
        for money in ["lump", "monthly"] {
            let pick = |g: &[(f64, f64, f64, bool)]| -> Vec<f64> { g.iter().map(|x| if money == "lump" { x.0 } else { x.1 }).collect() };
            let show = |x: f64| if money == "lump" { format!("{:+.1}%/yr", (x.powf(1.0 / years as f64) - 1.0) * 100.0) } else { format!("{x:.2}x paid in") };
            let worst = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
            let one = pick(&base);
            let (med_1x, worst_1x) = (crate::core::median(one.clone()).unwrap_or(f64::NAN), worst(&one));
            out.push(format!(
                "  {years}y 1x {money}: {} windows, median {}, worst {}, worst drawdown {:.0}%",
                ws.len(),
                show(med_1x),
                show(worst_1x),
                deepest(&base)
            ));
            for (label, g) in &lines {
                let v = pick(g);
                let share = |half: Option<bool>| {
                    let of: Vec<bool> = v.iter().zip(&one).zip(&late).filter(|x| half.is_none_or(|h| *x.1 == h)).map(|((a, b), _)| a > b).collect();
                    of.iter().filter(|b| **b).count() as f64 / of.len() as f64
                };
                let (halves, wipes) = ((share(Some(false)), share(Some(true))), g.iter().filter(|x| x.3).count());
                let mut line = format!(
                    "  {years}y {label} {money}: beats 1x in {:.0}% (before {split} {:.0}% | after {:.0}%), median {} vs {}, worst {} vs {}, worst drawdown {:.0}%, {wipes} wipe-outs",
                    share(None) * 100.0,
                    halves.0 * 100.0,
                    halves.1 * 100.0,
                    show(crate::core::median(v.clone()).unwrap_or(f64::NAN)),
                    show(med_1x),
                    show(worst(&v)),
                    show(worst_1x),
                    deepest(g)
                );
                if h == 0 {
                    let why = misses(halves, worst(&v), worst_1x, wipes);
                    clear += usize::from(why.is_none());
                    line += &format!(": {}", why.map_or("passes the bar".to_string(), |w| format!("fails, {w}")));
                }
                out.push(line);
            }
        }
    }
    out.push(format!(
        "  {clear} of {} lines clear the bar (beat 1x in 80% of {}y windows in both halves, worst window no poorer, no 90% fall)",
        2 * COSTS.len() * LEVELS.len(),
        horizons[0]
    ));
    out.join("\n")
}

/// (#1220) THE LIFECYCLE CENSUS (Ayres and Nalebuff, "Diversification Across Time", 2010): does a saver
/// from 23 to 64 who holds stock worth λ times savings plus the rest of a working life's saving, never
/// above 2x savings, end better off than one who holds a fixed share? The rest sits in 10y Treasuries.
/// A census, not a knob. The wage saver puts the same share of each month's wage aside: the Social
/// Security medium earner's scaled factors for ages 23 to 64 (Actuarial Note 2026.3, Table 6).
pub const WAGE: [f64; 42] = [
    0.436, 0.521, 0.587, 0.645, 0.699, 0.748, 0.791, 0.829, 0.863, 0.895, 0.923, 0.948, 0.970, 0.989, 1.006, 1.020, 1.033, 1.045, 1.055, 1.064, 1.071,
    1.079, 1.085, 1.089, 1.092, 1.093, 1.093, 1.093, 1.091, 1.086, 1.080, 1.071, 1.060, 1.041, 1.018, 0.993, 0.964, 0.927, 0.878, 0.847, 0.818, 0.790,
];
/// (#1220) The real rate a year future saving is discounted at, the paper's.
pub const REAL_RATE: f64 = 0.0256;
/// (#1220) The taxed lines' rate on each sale's gain, losses carried forward.
pub const GAIN_TAX: f64 = 0.28;

/// (#1220) Books a month's net sale `gain` at `rate` against the losses carried: (tax owed, losses
/// carried) after it.
fn book(gain: f64, rate: f64, (owed, carry): (f64, f64)) -> (f64, f64) {
    let net = gain - carry;
    (owed + rate * net.max(0.0), (-net).max(0.0))
}

/// (#1220) One saver from month `s` of `mo` (each month's 1x, 2x fund, margin interest and 10y bond
/// returns) and `price` (the CPI on each month's first bar). In month `j`, `pay[j]` arrives in real
/// terms, last month's tax is paid, and the account trades to `target(wealth, the rest of the saving at
/// [`REAL_RATE`], j)` in stock, between 0 and 2x wealth. Stock above wealth is the 2x fund's share, or
/// borrowed if `margin`; stock below it leaves the rest in bonds. Each sale books its gain at average
/// cost; an account at or below 0 is sold out and starts again from the next saving; the last month
/// sells everything. Returns the end wealth over what was paid in, both real, then the deepest fall of
/// the account's unit value and whether it ever fell 90%.
pub fn save(mo: &[[f64; 4]], price: &[f64], s: usize, pay: &[f64], target: &dyn Fn(f64, f64, usize) -> f64, margin: bool, rate: f64) -> (f64, f64, bool) {
    let step = (1.0 + REAL_RATE).powf(-1.0 / 12.0);
    let mut rest = 0.0;
    let mut future: Vec<f64> = pay.iter().rev().map(|c| {
        let w = rest;
        rest = (rest + c) * step;
        w
    }).collect();
    future.reverse();
    let (mut h, mut debt, mut tax, mut rets) = ([(0.0f64, 0.0f64); 3], 0.0, (0.0f64, 0.0f64), vec![]);
    let value = |h: &[(f64, f64); 3]| h.iter().map(|x| x.0).sum::<f64>();
    for (j, (c, w)) in pay.iter().zip(&future).enumerate() {
        let (m, up) = (mo[s + j], price[s + j] / price[s]);
        let have = value(&h) - debt + c * up;
        let paid = tax.0.min(have);
        let wealth = have - paid;
        tax.0 -= paid;
        let x = target(wealth, w * up, j).clamp(0.0, 2.0 * wealth);
        let over = (x - wealth).max(0.0);
        let (fund, loan) = if margin { (0.0, over) } else { (over, 0.0) };
        let mut gain = 0.0;
        for (k, t) in h.iter_mut().zip([x - 2.0 * fund, fund, (wealth - x).max(0.0)]) {
            if t < k.0 {
                gain += (k.0 - t) * (1.0 - k.1 / k.0);
                k.1 *= t / k.0;
            } else {
                k.1 += t - k.0;
            }
            k.0 = t;
        }
        tax = book(gain, rate, tax);
        for (k, r) in h.iter_mut().zip([m[0], m[1], m[3]]) {
            k.0 *= 1.0 + r;
        }
        debt = loan * (1.0 + m[2]);
        let end = value(&h) - debt;
        if end <= 0.0 {
            tax = book(h.iter().map(|x| x.0 - x.1).sum(), rate, tax);
            (h, debt) = ([(0.0, 0.0); 3], 0.0);
        }
        rets.push(end / wealth - 1.0);
    }
    let left = value(&h) - debt - book(h.iter().map(|x| x.0 - x.1).sum(), rate, tax).0;
    let mut sum = 0.0;
    let le: Vec<f64> = std::iter::once(0.0).chain(rets.iter().map(|r| {
        sum += (1.0 + r).max(1e-9).ln();
        sum
    })).collect();
    (left * price[s] / price[s + pay.len()] / pay.iter().sum::<f64>(), drawdown(&le), ruined(&rets))
}

/// (#1220) Why a 200/50 line misses the paper's two claims, `None` when it holds both: its SD of end
/// wealth is below the same-mean fixed share's, and its certainty equivalent above the 90/50 glide's.
pub fn claims(sd: (f64, f64), ce: (f64, f64)) -> Option<&'static str> {
    if sd.0 >= sd.1 {
        Some("its SD is no lower")
    } else if ce.0 <= ce.1 {
        Some("its CE is no higher")
    } else {
        None
    }
}

/// (#1220) The census text on daily 1x total returns `rs` and bill rate `cash`, with the CPI `cpi` and
/// the 10y yield `long` (% a year) by month: a saver starting on each month's first bar whose 42 years
/// fit, saving the same real sum each month ("flat") or a share of the wage, before tax ("gross") and
/// after it. 200/100 is graded against 100/100 on the (#1219) bar, its halves split at `split`; 200/50
/// against the paper's claims, beside the fixed share whose mean end wealth is nearest on a 0.01 grid
/// up to 2.
pub fn lifecycle_report(dates: &[NaiveDate], rs: &[f64], cash: &[f64], cpi: &BTreeMap<i32, f64>, long: &BTreeMap<i32, f64>, split: NaiveDate) -> String {
    let f = firsts(dates);
    let fund = levered(dates, rs, cash, 2.0, COSTS[0].1, COSTS[0].2);
    let grow = |v: &[f64], a: usize, b: usize| v[a + 1..=b].iter().map(|r| 1.0 + r).product::<f64>() - 1.0;
    let mo: Vec<[f64; 4]> = f.windows(2).map(|w| {
        let years = (dates[w[1]] - dates[w[0]]).num_days() as f64 / 365.25;
        let bond = crate::core::bond_month_return(on_month(long, dates[w[0]]), on_month(long, dates[w[1]]));
        [grow(rs, w[0], w[1]), grow(&fund, w[0], w[1]), (cash[w[0]] + COSTS[1].1) * years, bond]
    }).collect();
    let price: Vec<f64> = f.iter().map(|&i| on_month(cpi, dates[i])).collect();
    let n = 12 * WAGE.len();
    let starts: Vec<usize> = (0..(mo.len() + 1).saturating_sub(n)).collect();
    let late: Vec<bool> = starts.iter().map(|&s| dates[f[s]] >= split).collect();
    let month = |s: usize| dates[f[s]].format("%Y-%m");
    let mut out = vec![format!(
        "\n── (#1220) LIFECYCLE CENSUS (Ayres–Nalebuff): {} savers aged 23 to 64, one starting each month from {} to {} ({:.1} independent spans), in real terms; 200/λ holds stock worth λ x (savings + the saving still to come at 2.56%/yr real), at most 2x savings, λ/λ holds λ x savings, 90/50 glides from 90% of savings to 50%, the rest sits in 10y Treasuries; above 1x savings, etf holds a 2x daily fund (bills + 0.60%/yr), margin borrows at bills + 2.5 pts; taxed lines pay 28% of each sale's gain and sell out at 64 ──",
        starts.len(),
        month(0),
        month(starts.len().saturating_sub(1)),
        mo.len() as f64 / n as f64
    )];
    let pays = [("flat", vec![1.0; n]), ("wage", WAGE.iter().flat_map(|w| [*w; 12]).collect())];
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let sd = |v: &[f64]| {
        let m = mean(v);
        mean(&v.iter().map(|x| (x - m).powi(2)).collect::<Vec<f64>>()).sqrt()
    };
    let ce = |v: &[f64]| mean(&v.iter().map(|x| x.max(1e-9).powi(-3)).collect::<Vec<f64>>()).powf(-1.0 / 3.0);
    let worst = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let med = |v: &[f64]| crate::core::median(v.to_vec()).unwrap_or(f64::NAN);
    let (mut clear, mut hold) = (0, 0);
    for (path, pay) in &pays {
        for (tax, rate) in [("gross", 0.0), ("taxed", GAIN_TAX)] {
            let run = |target: &dyn Fn(f64, f64, usize) -> f64, margin: bool| -> Vec<(f64, f64, bool)> {
                starts.iter().map(|&s| save(&mo, &price, s, pay, target, margin, rate)).collect()
            };
            let ends = |g: &[(f64, f64, bool)]| g.iter().map(|x| x.0).collect::<Vec<f64>>();
            let deepest = |g: &[(f64, f64, bool)]| g.iter().map(|x| x.1).fold(0.0, f64::max) * 100.0;
            let base = run(&|w, _, _| w, false);
            let one = ends(&base);
            out.push(format!(
                "  {path} {tax} 100/100: {} savers, median {:.2}x paid in, worst {:.2}x, worst drawdown {:.0}%",
                one.len(),
                med(&one),
                worst(&one),
                deepest(&base)
            ));
            // 503: the last of the 504 months holds 50%
            let glide = ce(&ends(&run(&|w, _, j| w * (0.9 - 0.4 * j as f64 / 503.0), false)));
            for (cost, ..) in COSTS {
                let g = run(&|w, rest, _| w + rest, cost == "margin");
                let v = ends(&g);
                // a tie is not a win; total_cmp so only a strictly richer end counts
                let share = |half: Option<bool>| {
                    let of: Vec<bool> = v.iter().zip(&one).zip(&late).filter(|x| half.is_none_or(|h| *x.1 == h)).map(|((a, b), _)| a.total_cmp(b).is_gt()).collect();
                    of.iter().filter(|b| **b).count() as f64 / of.len() as f64
                };
                let (halves, wipes) = ((share(Some(false)), share(Some(true))), g.iter().filter(|x| x.2).count());
                let why = misses(halves, worst(&v), worst(&one), wipes);
                clear += usize::from(why.is_none());
                out.push(format!(
                    "  {path} {tax} 200/100 {cost}: beats 100/100 for {:.0}% of savers (before {split} {:.0}% | after {:.0}%), median {:.2}x vs {:.2}x, worst {:.2}x vs {:.2}x, worst drawdown {:.0}%, {wipes} wipe-outs: {}",
                    share(None) * 100.0,
                    halves.0 * 100.0,
                    halves.1 * 100.0,
                    med(&v),
                    med(&one),
                    worst(&v),
                    worst(&one),
                    deepest(&g),
                    why.map_or("passes the bar".to_string(), |w| format!("fails, {w}"))
                ));
            }
            for (cost, ..) in COSTS {
                let v = ends(&run(&|w, rest, _| 0.5 * (w + rest), cost == "margin"));
                let m = mean(&v);
                let twins = (1..=200).map(|k| (k, ends(&run(&|w, _, _| w * (k as f64 / 100.0), cost == "margin"))));
                let (k, twin) = twins.min_by(|a, b| (mean(&a.1) - m).abs().total_cmp(&(mean(&b.1) - m).abs())).unwrap();
                let (s, c) = (sd(&v), ce(&v));
                let why = claims((s, sd(&twin)), (c, glide));
                hold += usize::from(why.is_none());
                out.push(format!(
                    "  {path} {tax} 200/50 {cost}: mean {m:.2}x paid in, SD {s:.2} vs {:.2} for {k}/{k} (mean {:.2}x), CE at CRRA 4 {c:.2}x vs {glide:.2}x for 90/50 ({:+.1}%): {}",
                    sd(&twin),
                    mean(&twin),
                    (c / glide - 1.0) * 100.0,
                    why.map_or("holds both claims".to_string(), |w| format!("fails, {w}"))
                ));
            }
        }
    }
    out.push(format!("  {clear} of 8 200/100 lines clear the bar (beat 100/100 for 80% of savers in both halves, worst saver no poorer, no 90% fall)"));
    out.push(format!("  {hold} of 8 200/50 lines hold both claims (SD below the same-mean fixed share's, CE at CRRA 4 above 90/50's)"));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn days(n: usize) -> Vec<NaiveDate> {
        (0..n).map(|i| d(2024, 1, 1) + Duration::days(i as i64)).collect()
    }

    fn marks(pos: &[bool]) -> String {
        pos.iter().map(|p| if *p { 'T' } else { 'F' }).collect()
    }

    fn close(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
        assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-12), "{a:?} vs {b:?}");
    }

    #[test]
    fn the_grids_hold_47_5_and_13_settings_with_these_labels() {
        let s = sweep();
        let count = |f: &str| s.iter().filter(|r| r.family() == f).count();
        assert_eq!((s.len(), count("hold"), count("trend"), count("average"), count("breakout"), count("swing")), (47, 1, 4, 5, 25, 12));
        assert_eq!(monthly().iter().map(Rule::label).collect::<Vec<_>>(), ["buy-and-hold", "trend 3m", "trend 6m", "trend 9m", "trend 12m"]);
        assert_eq!(swing().len(), 13);
        assert_eq!(swing()[1].label(), "swing fall 5% exit 5");
        assert_eq!(swing()[10].label(), "swing RSI2 < 10 exit 5");
        assert_eq!(Rule::Average { bars: 200 }.label(), "above 200-bar mean");
        assert_eq!(Rule::Breakout { bars: Some(20), k: 2.0 }.label(), "breakout 20-bar stop 2");
        assert_eq!(Rule::Breakout { bars: None, k: 2.5 }.label(), "breakout all-time stop 2.5");
    }

    #[test]
    fn the_unit_is_the_mean_of_the_last_10_moves_and_rsi2_the_up_share_of_2() {
        let c = [0.0, 1.0, 3.0, 6.0, 10.0, 15.0, 21.0, 28.0, 36.0, 45.0, 55.0, 66.0];
        assert_eq!((vol(&c, 9), vol(&c, 10), vol(&c, 11)), (None, Some(5.5), Some(6.5)));
        assert_eq!((rsi2(&c, 1), rsi2(&[10.0; 3], 2)), (None, Some(50.0)));
        assert!((rsi2(&[10.0, 12.0, 11.0], 2).unwrap() - 66.66666666666667).abs() < 1e-12);
    }

    #[test]
    fn trend_flips_only_at_a_month_s_last_bar() {
        let dates = [d(2024, 1, 30), d(2024, 1, 31), d(2024, 2, 1), d(2024, 2, 28), d(2024, 2, 29), d(2024, 3, 1), d(2024, 3, 4), d(2024, 3, 28), d(2024, 3, 29), d(2024, 4, 1)];
        let closes = [10.0, 11.0, 12.0, 13.0, 12.0, 9.0, 9.5, 8.0, 8.5, 9.0];
        assert_eq!(marks(&positions(Rule::Trend { months: 1 }, &dates, &closes)), "FFFFTTTTFF");
        assert_eq!(marks(&positions(Rule::Hold, &dates, &closes)), "TTTTTTTTTT");
        // a month-end close equal to the one before stays out
        assert_eq!(marks(&positions(Rule::Trend { months: 1 }, &[d(2024, 1, 31), d(2024, 2, 1), d(2024, 2, 29), d(2024, 3, 1)], &[10.0, 9.0, 10.0, 11.0])), "FFFF");
    }

    #[test]
    fn average_is_in_above_its_mean_once_it_has_the_bars() {
        assert_eq!(marks(&positions(Rule::Average { bars: 3 }, &days(6), &[1.0, 2.0, 3.0, 2.0, 1.0, 3.0])), "FFTFFT");
        // a close equal to its mean stays out
        assert_eq!(marks(&positions(Rule::Average { bars: 3 }, &days(5), &[2.0, 2.0, 2.0, 2.0, 3.0])), "FFFFT");
        assert_eq!(marks(&positions(Rule::Average { bars: 2 }, &days(8), &[1.0, 2.0, 3.0, 4.0, 5.0, 4.0, 3.0, 6.0])), "FTTTTFFT");
    }

    #[test]
    fn breakout_buys_a_new_high_after_10_moves_and_stops_k_units_below_its_peak() {
        // the unit is 10.5 at the 115 close, so 115 holds above the 130 peak's 109 stop
        let c = [100.0, 110.0, 100.0, 110.0, 100.0, 110.0, 100.0, 110.0, 100.0, 110.0, 105.0, 120.0, 130.0, 115.0, 100.0, 90.0];
        assert_eq!(marks(&positions(Rule::Breakout { bars: Some(3), k: 2.0 }, &days(16), &c)), "FFFFFFFFFFFTTTFF");
        // the unit is 1.125 at the 10.75 close, so it sits on the 13 peak's stop, which is no exit
        let c = [10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 12.0, 13.0, 10.75];
        assert_eq!(marks(&positions(Rule::Breakout { bars: Some(3), k: 2.0 }, &days(13), &c)), "FFFFFFFFFFTTT");
        // 11.5 tops the last 3 closes but not the first
        let c = [20.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.5, 11.5];
        assert_eq!(marks(&positions(Rule::Breakout { bars: Some(3), k: 100.0 }, &days(12), &c)), "FFFFFFFFFFFT");
        assert_eq!(marks(&positions(Rule::Breakout { bars: None, k: 100.0 }, &days(12), &c)), "FFFFFFFFFFFF");
        // a close equal to an earlier one is no new high
        let c = [10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 10.0, 11.0, 11.0];
        assert_eq!(marks(&positions(Rule::Breakout { bars: None, k: 100.0 }, &days(11), &c)), "FFFFFFFFFFF");
    }

    #[test]
    fn swing_buys_a_fall_or_a_low_rsi_and_sells_the_bounce_or_after_max_bars() {
        let c = [10.0, 10.0, 10.0, 10.0, 10.0, 9.4, 9.0, 9.2, 9.1, 9.0, 8.9];
        assert_eq!(marks(&positions(Rule::Swing { fall: Some(0.05), rsi: None, max_bars: 3 }, &days(11), &c)), "FFFFFTTTFTT");
        // flat moves read 50; the close above the 5-bar mean sells
        let c = [10.0, 10.0, 10.0, 11.0, 10.0, 9.0, 9.5, 10.5];
        assert_eq!(marks(&positions(Rule::Swing { fall: None, rsi: Some(10.0), max_bars: 20 }, &days(8), &c)), "FFFFFTTF");
        // flat bars neither fall nor read under 50, and a close equal to its 5-bar mean holds
        assert_eq!(marks(&positions(Rule::Swing { fall: Some(0.05), rsi: None, max_bars: 20 }, &days(8), &[10.0; 8])), "FFFFFFFF");
        assert_eq!(marks(&positions(Rule::Swing { fall: None, rsi: Some(50.0), max_bars: 20 }, &days(8), &[10.0; 8])), "FFFFFFFF");
        assert_eq!(marks(&positions(Rule::Swing { fall: None, rsi: Some(60.0), max_bars: 3 }, &days(6), &[10.0; 6])), "FFTTTF");
        let c = [20.0, 20.0, 20.0, 20.0, 20.0, 20.0, 20.0, 20.0, 10.0, 10.0, 10.0, 10.0, 10.0, 11.0];
        assert_eq!(marks(&positions(Rule::Swing { fall: Some(0.05), rsi: None, max_bars: 20 }, &days(14), &c)), "FFFFFFFFTTTTTF");
        // the fall reads 5 bars back, and a 0.5 rise then a 9.5 drop reads RSI 5
        assert_eq!(marks(&positions(Rule::Swing { fall: Some(0.05), rsi: None, max_bars: 20 }, &days(8), &[10.0, 10.0, 20.0, 20.0, 20.0, 20.0, 20.0, 18.0])), "FFFFFFFT");
        assert_eq!(marks(&positions(Rule::Swing { fall: None, rsi: Some(10.0), max_bars: 20 }, &days(5), &[10.0, 10.0, 10.0, 10.5, 1.0])), "FFFFT");
    }

    #[test]
    fn returns_price_entry_hold_exit_and_cash_and_lever_with_the_borrow() {
        let dates = [d(2024, 1, 1), d(2024, 1, 2), d(2024, 1, 3), d(2024, 1, 4), d(2024, 1, 8)];
        let (opens, closes, pos) = ([10.0, 10.5, 11.0, 12.0, 11.0], [10.0, 11.0, 11.5, 11.8, 10.5], [true, true, false, false, true]);
        close(&returns(&dates, &opens, &closes, &pos, 1, 0.01, &[0.05; 5]), &[0.0, 0.042619047619047674, 0.045454545454545414, 0.03847826086956519, 0.0005475701574264203]);
        close(&returns(&dates, &opens, &closes, &pos, 3, 0.01, &[0.05; 5]), &[0.0, 0.12744646523907321, 0.13595295874556643, 0.11502410499062576, 0.0005475701574264203]);
        // out after the first close, in from the second bar's open
        close(&returns(&dates[..3], &[10.0, 10.0, 12.0], &[10.0, 11.0, 13.0], &[false, true, true], 1, 0.01, &[0.05; 3]), &[0.0, 0.00013689253935660506, 0.07833333333333325]);
        let wiped = returns(&dates[..4], &[10.0; 4], &[10.0, 6.0, 12.0, 13.0], &[true; 4], 3, 0.01, &[0.05; 4]);
        assert_eq!(wiped[..2], [0.0, -1.0]);
        assert!(wiped[2].is_nan() && wiped[3].is_nan());
        // a price that is zero or not finite reads as no move
        assert_eq!(returns(&dates, &[10.0; 5], &[10.0, 0.0, 5.0, f64::INFINITY, 10.0], &[true; 5], 1, 0.0, &[0.0; 5]), [0.0; 5]);
    }

    #[test]
    fn ruin_is_a_90_percent_fall_from_the_peak() {
        assert!(ruined(&[1.5, -0.5, -0.5, -0.5, -0.2])); // exactly 0.25 of a 2.5 peak
        assert!(!ruined(&[1.5, -0.5, -0.5, -0.5, -0.1]));
        assert!(ruined(&[1.0, f64::NAN, -0.9]));
    }

    #[test]
    fn add_compounds_a_day_s_bars_and_skips_gone_bars_non_members_and_days_before_base() {
        let mut t = Tally::new();
        let rs = [0.5, 0.1, 0.2, f64::NAN, 0.3];
        assert_eq!(add(&mut t, d(2024, 1, 1), &[d(2024, 1, 1), d(2024, 1, 2), d(2024, 1, 2), d(2024, 1, 3), d(2024, 1, 5)], &rs, |x| x != d(2024, 1, 5), 1), 1);
        assert_eq!(add(&mut t, d(2024, 1, 1), &[d(2023, 12, 31), d(2024, 1, 2), d(2024, 1, 4)], &[0.4, 0.1, 0.2], |_| true, 0), 2);
        assert_eq!(t.len(), 4);
        assert_eq!((t[0], t[1].1, t[2], t[3].1), ((0.0, 0), 2, (0.0, 0), 1));
        assert!((t[1].0 - 0.42000000000000015).abs() < 1e-12);
    }

    #[test]
    fn score_compounds_monthly_against_buy_and_hold_and_reads_too_few_under_3_months() {
        let at = |pairs: &[(usize, (f64, u32))], n: usize| {
            let mut t = vec![(0.0, 0); n];
            for &(k, v) in pairs {
                t[k] = v;
            }
            t
        };
        // two names a day, so a day's return is its sum over its count
        let hold = at(&[(0, (0.04, 2)), (31, (0.02, 2)), (60, (-0.02, 2)), (91, (0.06, 2)), (92, (0.02, 2))], 93);
        // the rule's names are gone on the last day, which reads 0
        let rule = at(&[(0, (0.06, 2)), (31, (0.04, 2)), (60, (-0.06, 2)), (91, (0.04, 2))], 92);
        let s = score("stock trend 12m 2x", &rule, &hold, d(2024, 1, 1), 1, 2);
        assert_eq!(s.text, "  stock trend 12m 2x  4 months  net +12.3%/yr  worst drawdown 3%  vs buy-and-hold -7.1 pts/yr  t -0.6  halves +11.8|-24.1  ruined 1/2");
        assert!((s.excess.unwrap() + 7.126509100011756).abs() < 1e-9 && (s.t.unwrap() + 0.594636825087939).abs() < 1e-9);
        assert!((s.halves.0 - 11.765082833025803).abs() < 1e-9 && (s.halves.1 + 24.069226457843946).abs() < 1e-9);
        let s = score("stock buy-and-hold 1x", &hold, &hold, d(2024, 1, 1), 0, 1);
        assert_eq!(s.text, "  stock buy-and-hold 1x  4 months  net +19.4%/yr  worst drawdown 1%  vs buy-and-hold +0.0 pts/yr  t n/a  halves +0.0|+0.0  ruined 0/1");
        assert_eq!((s.excess, s.t), (Some(0.0), None));
        let two = at(&[(0, (0.02, 1)), (31, (0.01, 1))], 32);
        let s = score("x", &two, &two, d(2024, 1, 1), 0, 1);
        assert_eq!((s.text.as_str(), s.excess), ("  x  too few months", None));
        // day 30 is Jan 31, so January holds two days and the 4 days read 3 months
        let gap = at(&[(0, (0.01, 1)), (30, (0.01, 1)), (60, (0.01, 1)), (91, (0.01, 1))], 92);
        assert!(score("x", &gap, &gap, d(2024, 1, 1), 0, 1).text.starts_with("  x  3 months  "));
    }

    fn line(text: &str, excess: Option<f64>, t: Option<f64>, halves: (f64, f64)) -> Score {
        Score { text: text.to_string(), excess, t, halves }
    }

    #[test]
    fn the_bar_needs_excess_t_3_and_both_halves() {
        assert!(line("", Some(0.1), Some(3.0), (0.1, 0.1)).clears());
        assert!(!line("", Some(0.0), Some(3.0), (0.1, 0.1)).clears());
        assert!(!line("", Some(0.1), Some(2.9), (0.1, 0.1)).clears());
        assert!(!line("", Some(0.1), None, (0.1, 0.1)).clears());
        assert!(!line("", Some(0.1), Some(3.0), (0.0, 0.1)).clears());
        assert!(!line("", Some(0.1), Some(3.0), (0.1, 0.0)).clears());
    }

    #[test]
    fn report_prints_buy_and_hold_then_each_family_s_best_per_leverage_and_the_bar_count() {
        let scores = vec![
            (Rule::Hold, 1, line("H1", Some(0.0), None, (0.0, 0.0))),
            (Rule::Hold, 2, line("H2", Some(1.0), Some(1.0), (1.0, -1.0))),
            (Rule::Trend { months: 12 }, 1, line("T12", Some(1.0), Some(3.5), (1.0, 2.0))),
            (Rule::Trend { months: 3 }, 1, line("T3", Some(2.0), Some(1.0), (1.0, 2.0))),
            (Rule::Trend { months: 3 }, 2, line("T3x2", Some(-1.0), None, (0.0, 0.0))),
            (Rule::Average { bars: 200 }, 2, line("A", None, None, (0.0, 0.0))),
            (Rule::Swing { fall: Some(0.05), rsi: None, max_bars: 5 }, 3, line("S", Some(0.5), Some(4.0), (0.1, 0.2))),
        ];
        assert_eq!(report("stock", &scores), "H1\nH2\nT3\nT3x2\nS\n  stock: 2 of 7 settings × leverage clear the bar (beat buy-and-hold, t ≥ 3, both halves)");
    }

    #[test]
    fn best_run_is_the_biggest_multiple_inside_the_window_from_an_allowed_start() {
        let c = [1.0, 2.0, 50.0, 3.0, 120.0, 1.0];
        assert_eq!(best_run(&c, 2, |_| true), Some((0, 2, 50.0)));
        assert_eq!(best_run(&c, 3, |_| true), Some((1, 4, 60.0)));
        assert_eq!(best_run(&c, 3, |i| i != 1), Some((0, 2, 50.0)));
        assert_eq!(best_run(&[0.0, 1.0, f64::NAN, 5.0], 3, |_| true), Some((1, 3, 5.0)));
        assert_eq!(best_run(&[1.0], 3, |_| true), None);
        // a tie keeps the earlier run
        assert_eq!(best_run(&[1.0, 2.0, 1.0, 2.0], 1, |_| true), Some((0, 1, 2.0)));
    }

    #[test]
    fn kept_is_the_rule_s_share_of_buy_and_hold_s_log_gain() {
        assert!((kept(&[0.0, 0.1, f64::NAN, -0.5], &[0.0, 0.21, 0.1, 0.2], 1, 3) + 1.2767417500068707).abs() < 1e-12);
        assert!((kept(&[0.0, -1.0], &[0.0, 0.5], 1, 1) + 51.109862285408155).abs() < 1e-9);
    }

    #[test]
    fn cash_carries_the_last_bill_close_forward_in_percent() {
        let irx = [(d(2024, 1, 2), 5.0), (d(2024, 1, 4), 4.0)];
        assert_eq!(cash_on(&irx, &[d(2024, 1, 1), d(2024, 1, 2), d(2024, 1, 3), d(2024, 1, 4), d(2024, 1, 9)]), [0.0, 0.05, 0.05, 0.04, 0.04]);
    }

    #[test]
    fn a_month_reads_its_own_value_or_the_latest_before_it() {
        let m = BTreeMap::from([(2000 * 12 + 2, 0.5), (2000 * 12 + 5, 0.25)]);
        assert_eq!([d(2000, 2, 28), d(2000, 3, 1), d(2000, 5, 31), d(2000, 6, 1), d(2001, 1, 1)].map(|x| on_month(&m, x)), [0.0, 0.5, 0.5, 0.25, 0.25]);
    }

    #[test]
    fn splice_pays_price_plus_dividend_yield_until_the_total_return_series_starts() {
        let px = [(d(2000, 1, 1), 100.0), (d(2000, 1, 5), 110.0), (d(2000, 1, 21), 99.0)];
        let yld = BTreeMap::from([(2000 * 12, 0.0365)]);
        let first = 110.0 / 100.0 - 1.0 + 0.0365 * 4.0 / 365.25;
        close(&splice(&px, &yld, &[(d(2000, 1, 5), 50.0), (d(2000, 1, 21), 55.0)]), &[0.0, first, 55.0 / 50.0 - 1.0]);
        close(&splice(&px, &yld, &[]), &[0.0, first, 99.0 / 110.0 - 1.0 + 0.0365 * 16.0 / 365.25]);
    }

    #[test]
    fn a_levered_bar_pays_the_borrow_and_the_fee_and_loses_at_most_everything() {
        let dates = [d(2000, 1, 1), d(2000, 1, 11), d(2000, 1, 21)];
        let (rs, cash) = ([0.0, 0.1, -0.5], [0.0, 0.04, 0.04]);
        close(&levered(&dates, &rs, &cash, 1.0, 0.0, 0.0), &rs);
        close(&levered(&dates, &rs, &cash, 2.0, 0.025, 0.006), &[0.0, 2.0 * 0.1 - (0.065 + 0.006) * (10.0 / 365.25), -1.0]);
    }

    #[test]
    fn a_month_opens_on_its_first_bar_and_a_window_ends_a_full_term_on() {
        assert_eq!(firsts(&[d(2000, 1, 30), d(2000, 1, 31), d(2000, 2, 1), d(2000, 2, 2), d(2001, 2, 1)]), [0, 2, 4]);
        let dates = [d(2000, 1, 3), d(2000, 1, 20), d(2000, 2, 1), d(2002, 1, 2), d(2002, 1, 3), d(2002, 2, 1)];
        assert_eq!(windows(&dates, 2), [(0, 4), (2, 5)]);
    }

    #[test]
    fn drawdown_is_the_deepest_fall_below_a_running_peak() {
        assert!((drawdown(&[0.0, 2f64.ln(), 0.0, 1.5f64.ln()]) - 0.5).abs() < 1e-12);
        assert!((drawdown(&[0.0, -0.5]) - (1.0 - (-0.5f64).exp())).abs() < 1e-12);
    }

    #[test]
    fn the_bar_takes_80_percent_in_each_half_a_worst_no_poorer_and_no_wipe_out() {
        let under = Some("beats 1x in under 80% of a half");
        assert_eq!(misses((0.8, 0.8), 1.0, 1.0, 0), None);
        assert_eq!((misses((0.79, 0.9), 1.0, 1.0, 0), misses((0.9, 0.79), 1.0, 1.0, 0)), (under, under));
        assert_eq!(misses((0.9, 0.9), 0.99, 1.0, 0), Some("its worst window ends poorer"));
        assert_eq!(misses((0.9, 0.9), 1.0, 1.0, 1), Some("a window falls 90%"));
    }

    #[test]
    fn the_leverage_census_grades_the_first_horizon_and_counts_the_lines_that_clear_it() {
        // a crash on the second month's first bar wipes 3x out, and chop from bar 80 drags the levered lines
        let dates: Vec<NaiveDate> = (0..62).flat_map(|m| [1, 16].map(|day| d(2000 + m / 12, (m % 12 + 1) as u32, day))).collect();
        let rs: Vec<f64> = (0..dates.len()).map(|i| match i {
            0 => 0.0,
            2 => -0.34,
            i if i >= 80 => 0.008 + if i % 2 == 1 { 0.07 } else { -0.07 },
            _ => 0.008,
        }).collect();
        let cash: Vec<f64> = (0..dates.len()).map(|i| if i < 60 { 0.03 } else { 0.01 }).collect();
        let want = r"
── (#1219) LEVERAGE CENSUS: S&P 500 total return from 2000-01-01, 124 bars, rebalanced every bar; 1x has no fee ──
  1y 1x lump: 50 windows, median +21.1%/yr, worst -20.7%/yr, worst drawdown 34%
  1y 1.25x etf lump: beats 1x in 90% (before 2002-02-01 96% | after 84%), median +25.3%/yr vs +21.1%/yr, worst -28.7%/yr vs -20.7%/yr, worst drawdown 43%, 0 wipe-outs: fails, its worst window ends poorer
  1y 1.5x etf lump: beats 1x in 90% (before 2002-02-01 96% | after 84%), median +30.4%/yr vs +21.1%/yr, worst -36.9%/yr vs -20.7%/yr, worst drawdown 51%, 0 wipe-outs: fails, its worst window ends poorer
  1y 2x etf lump: beats 1x in 88% (before 2002-02-01 96% | after 80%), median +41.3%/yr vs +21.1%/yr, worst -55.7%/yr vs -20.7%/yr, worst drawdown 68%, 0 wipe-outs: fails, its worst window ends poorer
  1y 3x etf lump: beats 1x in 66% (before 2002-02-01 96% | after 36%), median +65.6%/yr vs +21.1%/yr, worst -100.0%/yr vs -20.7%/yr, worst drawdown 100%, 1 wipe-outs: fails, beats 1x in under 80% of a half
  1y 1.25x margin lump: beats 1x in 90% (before 2002-02-01 96% | after 84%), median +25.3%/yr vs +21.1%/yr, worst -28.7%/yr vs -20.7%/yr, worst drawdown 43%, 0 wipe-outs: fails, its worst window ends poorer
  1y 1.5x margin lump: beats 1x in 88% (before 2002-02-01 96% | after 80%), median +29.6%/yr vs +21.1%/yr, worst -37.3%/yr vs -20.7%/yr, worst drawdown 51%, 0 wipe-outs: fails, its worst window ends poorer
  1y 2x margin lump: beats 1x in 66% (before 2002-02-01 96% | after 36%), median +38.7%/yr vs +21.1%/yr, worst -56.6%/yr vs -20.7%/yr, worst drawdown 68%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  1y 3x margin lump: beats 1x in 62% (before 2002-02-01 96% | after 28%), median +58.7%/yr vs +21.1%/yr, worst -100.0%/yr vs -20.7%/yr, worst drawdown 100%, 1 wipe-outs: fails, beats 1x in under 80% of a half
  1y 1x monthly: 50 windows, median 1.11x paid in, worst 1.03x paid in, worst drawdown 34%
  1y 1.25x etf monthly: beats 1x in 90% (before 2002-02-01 100% | after 80%), median 1.13x paid in vs 1.11x paid in, worst 1.04x paid in vs 1.03x paid in, worst drawdown 43%, 0 wipe-outs: passes the bar
  1y 1.5x etf monthly: beats 1x in 90% (before 2002-02-01 100% | after 80%), median 1.16x paid in vs 1.11x paid in, worst 1.04x paid in vs 1.03x paid in, worst drawdown 51%, 0 wipe-outs: passes the bar
  1y 2x etf monthly: beats 1x in 82% (before 2002-02-01 100% | after 64%), median 1.21x paid in vs 1.11x paid in, worst 1.03x paid in vs 1.03x paid in, worst drawdown 68%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  1y 3x etf monthly: beats 1x in 60% (before 2002-02-01 100% | after 20%), median 1.33x paid in vs 1.11x paid in, worst 0.97x paid in vs 1.03x paid in, worst drawdown 100%, 1 wipe-outs: fails, beats 1x in under 80% of a half
  1y 1.25x margin monthly: beats 1x in 88% (before 2002-02-01 100% | after 76%), median 1.13x paid in vs 1.11x paid in, worst 1.04x paid in vs 1.03x paid in, worst drawdown 43%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  1y 1.5x margin monthly: beats 1x in 84% (before 2002-02-01 100% | after 68%), median 1.15x paid in vs 1.11x paid in, worst 1.04x paid in vs 1.03x paid in, worst drawdown 51%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  1y 2x margin monthly: beats 1x in 60% (before 2002-02-01 100% | after 20%), median 1.20x paid in vs 1.11x paid in, worst 1.02x paid in vs 1.03x paid in, worst drawdown 68%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  1y 3x margin monthly: beats 1x in 58% (before 2002-02-01 100% | after 16%), median 1.30x paid in vs 1.11x paid in, worst 0.95x paid in vs 1.03x paid in, worst drawdown 100%, 1 wipe-outs: fails, beats 1x in under 80% of a half
  2y 1x lump: 38 windows, median +15.8%/yr, worst -2.0%/yr, worst drawdown 34%
  2y 1.25x etf lump: beats 1x in 97% (before 2002-02-01 96% | after 100%), median +18.5%/yr vs +15.8%/yr, worst -5.5%/yr vs -2.0%/yr, worst drawdown 43%, 0 wipe-outs
  2y 1.5x etf lump: beats 1x in 97% (before 2002-02-01 96% | after 100%), median +21.8%/yr vs +15.8%/yr, worst -9.3%/yr vs -2.0%/yr, worst drawdown 51%, 0 wipe-outs
  2y 2x etf lump: beats 1x in 89% (before 2002-02-01 96% | after 77%), median +28.3%/yr vs +15.8%/yr, worst -20.9%/yr vs -2.0%/yr, worst drawdown 68%, 0 wipe-outs
  2y 3x etf lump: beats 1x in 79% (before 2002-02-01 96% | after 46%), median +40.1%/yr vs +15.8%/yr, worst -100.0%/yr vs -2.0%/yr, worst drawdown 100%, 1 wipe-outs
  2y 1.25x margin lump: beats 1x in 97% (before 2002-02-01 96% | after 100%), median +18.5%/yr vs +15.8%/yr, worst -5.5%/yr vs -2.0%/yr, worst drawdown 43%, 0 wipe-outs
  2y 1.5x margin lump: beats 1x in 92% (before 2002-02-01 96% | after 85%), median +21.1%/yr vs +15.8%/yr, worst -9.9%/yr vs -2.0%/yr, worst drawdown 51%, 0 wipe-outs
  2y 2x margin lump: beats 1x in 84% (before 2002-02-01 96% | after 62%), median +25.9%/yr vs +15.8%/yr, worst -22.4%/yr vs -2.0%/yr, worst drawdown 68%, 0 wipe-outs
  2y 3x margin lump: beats 1x in 71% (before 2002-02-01 96% | after 23%), median +34.2%/yr vs +15.8%/yr, worst -100.0%/yr vs -2.0%/yr, worst drawdown 100%, 1 wipe-outs
  2y 1x monthly: 38 windows, median 1.14x paid in, worst 1.13x paid in, worst drawdown 34%
  2y 1.25x etf monthly: beats 1x in 100% (before 2002-02-01 100% | after 100%), median 1.16x paid in vs 1.14x paid in, worst 1.14x paid in vs 1.13x paid in, worst drawdown 43%, 0 wipe-outs
  2y 1.5x etf monthly: beats 1x in 100% (before 2002-02-01 100% | after 100%), median 1.19x paid in vs 1.14x paid in, worst 1.14x paid in vs 1.13x paid in, worst drawdown 51%, 0 wipe-outs
  2y 2x etf monthly: beats 1x in 84% (before 2002-02-01 100% | after 54%), median 1.24x paid in vs 1.14x paid in, worst 1.13x paid in vs 1.13x paid in, worst drawdown 68%, 0 wipe-outs
  2y 3x etf monthly: beats 1x in 66% (before 2002-02-01 100% | after 0%), median 1.33x paid in vs 1.14x paid in, worst 1.01x paid in vs 1.13x paid in, worst drawdown 100%, 1 wipe-outs
  2y 1.25x margin monthly: beats 1x in 100% (before 2002-02-01 100% | after 100%), median 1.16x paid in vs 1.14x paid in, worst 1.14x paid in vs 1.13x paid in, worst drawdown 43%, 0 wipe-outs
  2y 1.5x margin monthly: beats 1x in 100% (before 2002-02-01 100% | after 100%), median 1.18x paid in vs 1.14x paid in, worst 1.13x paid in vs 1.13x paid in, worst drawdown 51%, 0 wipe-outs
  2y 2x margin monthly: beats 1x in 71% (before 2002-02-01 100% | after 15%), median 1.21x paid in vs 1.14x paid in, worst 1.11x paid in vs 1.13x paid in, worst drawdown 68%, 0 wipe-outs
  2y 3x margin monthly: beats 1x in 61% (before 2002-02-01 92% | after 0%), median 1.26x paid in vs 1.14x paid in, worst 0.96x paid in vs 1.13x paid in, worst drawdown 100%, 1 wipe-outs
  2 of 16 lines clear the bar (beat 1x in 80% of 1y windows in both halves, worst window no poorer, no 90% fall)";
        assert_eq!(lever_report(&dates, &rs, &cash, &[1, 2], d(2002, 2, 1)), want);
        // margin at a bill rate of minus the spread borrows for free, so on flat closes it ties 1x and never beats it
        let flat = lever_report(&days(731), &[0.0; 731], &[-SPREAD; 731], &[1], d(2024, 7, 1));
        assert!(flat.contains("1y 2x margin lump: beats 1x in 0% (before 2024-07-01 0% | after 0%)"), "{flat}");
    }

    #[test]
    fn a_saver_trades_to_the_target_pays_last_month_s_tax_and_carries_losses() {
        // month 1 sells some of the 2x fund at a gain, month 2 sells at a loss, month 3's gain is partly offset
        let mo = [[0.0; 4], [0.1, 0.18, 0.004, 0.003], [-0.2, -0.42, 0.005, 0.006], [0.3, 0.58, 0.004, -0.002], [0.4, 0.78, 0.004, 0.004]];
        let price = [90.0, 100.0, 102.0, 101.0, 104.0, 105.0];
        let pay = [1.5, 2.0, 0.7, 0.4];
        let mix = |w: f64, rest: f64, j: usize| [w + rest, w + rest, 0.4 * w, 0.9 * w][j];
        let got = [save(&mo, &price, 1, &pay, &mix, false, GAIN_TAX), save(&mo, &price, 1, &pay, &mix, true, GAIN_TAX)];
        close(&[got[0].0, got[0].1, got[1].0, got[1].1], &[1.148623151007553, 0.2646016013725718, 1.1582791606925071, 0.25982012703567026]);
        assert_eq!([got[0].2, got[1].2], [false, false]);
    }

    #[test]
    fn an_account_at_or_below_0_is_sold_out_books_the_loss_and_starts_again() {
        let price = [90.0, 100.0, 102.0, 101.0, 104.0, 105.0];
        let pay = [1.5, 2.0, 0.7, 0.4];
        // 2x on margin falls 60% and owes more than it holds; the 2x fund dies and leaves exactly 0
        let margin = [[0.0; 4], [-0.6, 0.0, 0.004, 0.0], [0.1, 0.0, 0.004, 0.002], [0.2, 0.0, 0.004, 0.003], [0.1, 0.0, 0.004, 0.001]];
        let fund = [[0.0; 4], [-0.5, -1.0, 0.004, 0.0], [0.2, 0.4, 0.004, 0.002], [0.1, 0.2, 0.004, 0.003], [0.05, 0.1, 0.004, 0.001]];
        let a = save(&margin, &price, 1, &pay, &|w, _, _| 2.0 * w, true, GAIN_TAX);
        let b = save(&fund, &price, 1, &pay, &|w, _, j| [2.0 * w, w, 0.0, w][j], false, GAIN_TAX);
        close(&[a.0, a.1, b.0, b.1], &[1.1427662462939963, 0.999999999, 0.778361956521739, 0.999999999]);
        assert!(a.2 && b.2);
    }

    #[test]
    fn a_200_50_line_holds_only_below_its_twin_s_sd_and_above_the_glide_s_ce() {
        let (sd, ce) = (Some("its SD is no lower"), Some("its CE is no higher"));
        assert_eq!([claims((1.0, 1.0), (2.0, 1.0)), claims((1.1, 1.0), (2.0, 1.0))], [sd, sd]);
        assert_eq!([claims((0.9, 1.0), (1.0, 1.0)), claims((0.9, 1.0), (0.8, 1.0))], [ce, ce]);
        assert_eq!(claims((0.9, 1.0), (1.1, 1.0)), None);
    }

    #[test]
    fn the_lifecycle_census_grades_200_100_on_the_bar_and_200_50_on_the_paper_s_claims() {
        // a crash on the second month's first bar, then chop around a 0.47% monthly drift
        let dates: Vec<NaiveDate> = (0..507).map(|m| d(1990 + m / 12, (m % 12 + 1) as u32, 1)).collect();
        let rs: Vec<f64> = (0..dates.len()).map(|i| match i {
            0 => 0.0,
            1 => -0.4,
            i if i % 2 == 1 => 0.0047 + 0.03,
            _ => 0.0047 - 0.03,
        }).collect();
        let cash = vec![0.03; dates.len()];
        let cpi: BTreeMap<i32, f64> = (0..507).step_by(6).map(|k: i32| (1990 * 12 + k, 100.0 * 1.002f64.powf(k as f64))).collect();
        let long = BTreeMap::from([(1990 * 12, 6.0), (2000 * 12, 4.0), (2010 * 12 + 5, 5.5)]);
        let want = r"
── (#1220) LIFECYCLE CENSUS (Ayres–Nalebuff): 3 savers aged 23 to 64, one starting each month from 1990-01 to 1990-03 (1.0 independent spans), in real terms; 200/λ holds stock worth λ x (savings + the saving still to come at 2.56%/yr real), at most 2x savings, λ/λ holds λ x savings, 90/50 glides from 90% of savings to 50%, the rest sits in 10y Treasuries; above 1x savings, etf holds a 2x daily fund (bills + 0.60%/yr), margin borrows at bills + 2.5 pts; taxed lines pay 28% of each sale's gain and sell out at 64 ──
  flat gross 100/100: 3 savers, median 1.83x paid in, worst 1.82x, worst drawdown 42%
  flat gross 200/100 etf: beats 100/100 for 100% of savers (before 1990-02-01 100% | after 100%), median 2.03x vs 1.83x, worst 2.02x vs 1.82x, worst drawdown 81%, 0 wipe-outs: passes the bar
  flat gross 200/100 margin: beats 100/100 for 0% of savers (before 1990-02-01 0% | after 0%), median 1.68x vs 1.83x, worst 1.66x vs 1.82x, worst drawdown 82%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  flat gross 200/50 etf: mean 2.00x paid in, SD 0.02 vs 0.04 for 145/145 (mean 2.00x), CE at CRRA 4 2.00x vs 1.92x for 90/50 (+4.1%): holds both claims
  flat gross 200/50 margin: mean 1.93x paid in, SD 0.02 vs 0.01 for 44/44 (mean 1.93x), CE at CRRA 4 1.93x vs 1.92x for 90/50 (+0.5%): fails, its SD is no lower
  flat taxed 100/100: 3 savers, median 1.49x paid in, worst 1.49x, worst drawdown 42%
  flat taxed 200/100 etf: beats 100/100 for 33% of savers (before 1990-02-01 0% | after 50%), median 1.49x vs 1.49x, worst 1.48x vs 1.49x, worst drawdown 81%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  flat taxed 200/100 margin: beats 100/100 for 0% of savers (before 1990-02-01 0% | after 0%), median 1.23x vs 1.49x, worst 1.22x vs 1.49x, worst drawdown 82%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  flat taxed 200/50 etf: mean 1.49x paid in, SD 0.01 vs 0.01 for 80/80 (mean 1.49x), CE at CRRA 4 1.49x vs 1.48x for 90/50 (+0.6%): holds both claims
  flat taxed 200/50 margin: mean 1.43x paid in, SD 0.01 vs 0.02 for 110/110 (mean 1.43x), CE at CRRA 4 1.43x vs 1.48x for 90/50 (-3.2%): fails, its CE is no higher
  wage gross 100/100: 3 savers, median 1.76x paid in, worst 1.75x, worst drawdown 42%
  wage gross 200/100 etf: beats 100/100 for 100% of savers (before 1990-02-01 100% | after 100%), median 1.94x vs 1.76x, worst 1.93x vs 1.75x, worst drawdown 81%, 0 wipe-outs: passes the bar
  wage gross 200/100 margin: beats 100/100 for 0% of savers (before 1990-02-01 0% | after 0%), median 1.62x vs 1.76x, worst 1.61x vs 1.75x, worst drawdown 82%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  wage gross 200/50 etf: mean 1.93x paid in, SD 0.01 vs 0.04 for 156/156 (mean 1.93x), CE at CRRA 4 1.93x vs 1.84x for 90/50 (+4.8%): holds both claims
  wage gross 200/50 margin: mean 1.86x paid in, SD 0.01 vs 0.01 for 46/46 (mean 1.85x), CE at CRRA 4 1.86x vs 1.84x for 90/50 (+0.7%): fails, its SD is no lower
  wage taxed 100/100: 3 savers, median 1.45x paid in, worst 1.44x, worst drawdown 42%
  wage taxed 200/100 etf: beats 100/100 for 100% of savers (before 1990-02-01 100% | after 100%), median 1.46x vs 1.45x, worst 1.45x vs 1.44x, worst drawdown 81%, 0 wipe-outs: passes the bar
  wage taxed 200/100 margin: beats 100/100 for 0% of savers (before 1990-02-01 0% | after 0%), median 1.20x vs 1.45x, worst 1.20x vs 1.44x, worst drawdown 82%, 0 wipe-outs: fails, beats 1x in under 80% of a half
  wage taxed 200/50 etf: mean 1.46x paid in, SD 0.01 vs 0.02 for 98/98 (mean 1.46x), CE at CRRA 4 1.46x vs 1.44x for 90/50 (+1.3%): holds both claims
  wage taxed 200/50 margin: mean 1.40x paid in, SD 0.01 vs 0.02 for 109/109 (mean 1.40x), CE at CRRA 4 1.40x vs 1.44x for 90/50 (-3.1%): fails, its CE is no higher
  3 of 8 200/100 lines clear the bar (beat 100/100 for 80% of savers in both halves, worst saver no poorer, no 90% fall)
  4 of 8 200/50 lines hold both claims (SD below the same-mean fixed share's, CE at CRRA 4 above 90/50's)";
        assert_eq!(lifecycle_report(&dates, &rs, &cash, &cpi, &long, d(1990, 2, 1)), want);
    }
}
