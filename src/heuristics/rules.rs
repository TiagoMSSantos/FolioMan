//! (#1218) THE TRADING-RULES CENSUS: can a timing rule beat buy-and-hold net of cost? `backtest N
//! traderules` prices the rule families with the longest records in the literature on every name, each at
//! 1x, 2x and 3x rebalanced every bar: trend (the close above its value some month-ends back, or above its
//! moving mean), a breakout to a new high close with a trailing stop, and a swing buy after a sharp fall.
//! Each runs on each class's names, the S&P 500 as it stood and as it stands, and the screen book,
//! against buy-and-hold 1x on the same names and days. Out of the market a name earns the 13-week bill
//! rate; above 1x it borrows at that rate plus [`SPREAD`]. A census, not a knob: nothing here reaches a
//! lane, BUY NOW, the page or a trade.

use super::intraday::stats;
use chrono::{Datelike, Duration, NaiveDate};

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
    let mut out = vec![0.0; closes.len()];
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
        // the unit is 10 here, so 115 holds above the 130 peak's 110 stop
        let c = [100.0, 110.0, 100.0, 110.0, 100.0, 110.0, 100.0, 110.0, 100.0, 110.0, 105.0, 120.0, 130.0, 115.0, 100.0, 90.0];
        assert_eq!(marks(&positions(Rule::Breakout { bars: Some(3), k: 2.0 }, &days(16), &c)), "FFFFFFFFFFFTTTFF");
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
        let hold = at(&[(0, (0.02, 1)), (31, (0.01, 1)), (60, (-0.01, 1)), (91, (0.03, 1)), (92, (0.01, 1))], 93);
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
}
