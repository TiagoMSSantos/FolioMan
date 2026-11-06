#!/usr/bin/env python3
"""Re-price a finished backtest's top-10 book against a FACTOR index instead of the S&P 500 (L4 of round 16).

The S&P 500 Momentum index beat the S&P 500 by 3.0 pts/yr from 1995 and the MSCI USA Momentum index by 2.85 pts/yr
(net) from 1994; the lane's top-10 book reads +2.3 to +2.8 pts/yr on the total-return ruler ((#805)) and +2.5 to
+3.3 on the price ruler. The question nobody has graded: does the book beat the FACTOR it resembles, or only the
index? This tool reads the `BOOK b<bucket> <excess> pts/yr | ...` lines of any backtest or rescore log (the top-10
book's excess over the run's benchmark, one line per half-year cutoff), adds back the benchmark's own annualized
return over the same window, and subtracts a factor index's, so every bucket reads `book minus factor` in pts/yr.
It prints mean, median, trimmed mean, p20 and the worst five, overall and per cutoff era ((#1317) eras), for every
row the log holds.

Inputs (monthly or coarser levels, `date,level` CSV, header optional; a Yahoo chart export or the fixture cache's
^GSPC via `--bench-from-cache` work for the benchmark; the factor series comes from the index provider's free
end-of-day download — MSCI USA Momentum USD Net/Gross from MSCI's end-of-day index data search, or S&P 500 Momentum
TR from S&P DJI):

    python3 -I tools/book_vs_factor.py --log probe-12y.log --bench gspc.csv --factor msci_usa_momentum_net.csv
    python3 -I tools/book_vs_factor.py --log probe-12y.log --bench-from-cache tests/fixture/.long_history_cache.json --factor x.csv
    python3 -I tools/book_vs_factor.py --selftest

Shortcuts, stated: a bucket's cutoff is taken as the first day of its half-year (the walk cuts on the first bar
of the bucket, days later at most); the benchmark the log priced against is the one `--bench` must hold (^GSPC on
the price ruler, ^SP500TR under `backtest_tr_ruler`); a factor index in another currency or with another dividend
treatment than the benchmark shifts every bucket by the same spread, which the per-era table makes visible.
Record only: nothing here is a ship rule.
"""
import argparse, csv, datetime, json, math, os, statistics, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import regrade_ruler as rr  # noqa: E402


def month_key(d):
    return d.year * 12 + d.month - 1


def load_levels(path):
    """`date,level` rows (header optional; Yahoo's Date,Open,...,Adj Close,... also accepted: the last numeric
    column is the level) -> {month key: last level in that month}."""
    out = {}
    with open(path, encoding="utf-8", errors="replace", newline="") as fh:
        for row in csv.reader(fh):
            if len(row) < 2:
                continue
            try:
                d = datetime.date.fromisoformat(row[0].strip()[:10])
            except ValueError:
                continue
            level = None
            for cell in reversed(row[1:]):
                try:
                    level = float(cell)
                    break
                except ValueError:
                    continue
            if level is not None and level > 0:
                out[month_key(d)] = level  # rows arrive in date order, so the month's last row wins
    return out


def load_cache_levels(path, symbol="^GSPC"):
    """The fixture/long cache's adjusted closes for `symbol` -> {month key: level}."""
    with open(path) as fh:
        d = json.load(fh)
    r = d[symbol][1]["chart"]["result"][0]
    out = {}
    for t, c in zip(r["timestamp"], r["indicators"]["adjclose"][0]["adjclose"]):
        if c:
            out[month_key(datetime.date.fromtimestamp(t))] = c
    return out


def level_at(levels, m):
    """The last level on or before month `m`, None before the first."""
    ks = [k for k in levels if k <= m]
    return levels[max(ks)] if ks else None


def ann(mult, years):
    return (mult ** (1.0 / years) - 1.0) * 100.0


def cutoff_of(bucket):
    """`backtest::bucket` = year * 2 + half -> the first day of that half-year."""
    return datetime.date(bucket // 2, 1 if bucket % 2 == 0 else 7, 1)


def reprice(books, years, bench, factor):
    """{bucket: (book excess vs bench, book excess vs factor)} for the buckets both series cover."""
    out = {}
    for b, (excess, _) in books.items():
        if excess is None:
            continue
        c = cutoff_of(b)
        m0, m1 = month_key(c), month_key(c) + 12 * years
        b0, b1, f0, f1 = level_at(bench, m0), level_at(bench, m1), level_at(factor, m0), level_at(factor, m1)
        if None in (b0, b1, f0, f1) or max(bench) < m1 or max(factor) < m1:
            continue
        bench_ann, factor_ann = ann(b1 / b0, years), ann(f1 / f0, years)
        out[b] = (excess, excess + bench_ann - factor_ann)
    return out


def summarize(xs):
    if not xs:
        return None
    return {"n": len(xs), "mean": statistics.mean(xs), "median": rr.median(xs), "trimmed": rr.trimmed_mean(xs), "p20": rr.percentile(xs, 20), "worst5": rr.worst_mean(xs)}


def fmt(s):
    if not s:
        return "   -"
    return f"n {s['n']:>3}  mean {s['mean']:+5.1f}  med {s['median']:+5.1f}  trim {s['trimmed']:+5.1f}  p20 {s['p20']:+5.1f}  worst5 {s['worst5']:+5.1f}"


def report(rows, bench, factor, factor_name):
    for r in rows:
        if not r["books"]:
            continue
        years = r["years"] or 0
        if not years:
            print(f"row {r['label']}: no HOLD years line, skipped")
            continue
        rep = reprice(r["books"], years, bench, factor)
        vs_b = [v[0] for v in rep.values()]
        vs_f = [v[1] for v in rep.values()]
        print(f"\n── row {r['label'][:60]}  HOLD {years}y  ({len(rep)} of {sum(1 for e, _ in r['books'].values() if e is not None)} buckets covered by both series) ──")
        print(f"  vs benchmark (as logged)  {fmt(summarize(vs_b))}")
        print(f"  vs {factor_name:<22} {fmt(summarize(vs_f))}")
        for _, _, era in rr.ERAS:
            sel = [v[1] for b, v in rep.items() if rr.era_of(b) == era]
            if sel:
                print(f"    era {era:<8} vs {factor_name:<16} {fmt(summarize(sel))}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--log", help="a backtest or rescore log with BOOK b lines")
    ap.add_argument("--bench", help="date,level CSV of the benchmark the log priced against (^GSPC or ^SP500TR)")
    ap.add_argument("--bench-from-cache", help="read ^GSPC levels from a .long_history_cache.json instead")
    ap.add_argument("--factor", help="date,level CSV of the factor index (e.g. MSCI USA Momentum USD Net)")
    ap.add_argument("--factor-name", default="factor")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        sys.argv = sys.argv[:1]
        unittest.main(module=__name__, verbosity=2)
        return
    if not (a.log and a.factor and (a.bench or a.bench_from_cache)):
        ap.error("--log, --factor and one of --bench/--bench-from-cache are required (or --selftest)")
    bench = load_cache_levels(a.bench_from_cache) if a.bench_from_cache else load_levels(a.bench)
    factor = load_levels(a.factor)
    with open(a.log, errors="replace") as fh:
        rows = rr.parse_rows(fh.read(), os.path.basename(a.log))
    report(rows, bench, factor, a.factor_name)


class Tests(unittest.TestCase):
    def test_reprice_adds_bench_and_subtracts_factor(self):
        # bench +10 %/yr, factor +13 %/yr, both flat-compounding monthly from 2000-01 to 2015-01
        bench, factor = {}, {}
        for m in range(month_key(datetime.date(2000, 1, 1)), month_key(datetime.date(2015, 1, 1)) + 1):
            t = (m - month_key(datetime.date(2000, 1, 1))) / 12.0
            bench[m], factor[m] = 100 * 1.10 ** t, 100 * 1.13 ** t
        books = {2001 * 2: (2.0, []), 2001 * 2 + 1: (-1.0, []), 2010 * 2: (0.5, []), 2012 * 2: (None, [])}
        rep = reprice(books, 8, bench, factor)
        self.assertEqual(sorted(rep), [2001 * 2, 2001 * 2 + 1])
        self.assertAlmostEqual(rep[2001 * 2][1], 2.0 + 10.0 - 13.0, places=6, msg="a +2 book over a +10 bench is -1 against a +13 factor")
        self.assertAlmostEqual(rep[2001 * 2 + 1][1], -1.0 - 3.0, places=6)
        self.assertNotIn(2010 * 2, rep, "2010 + 8y runs past the series' end")

    def test_cutoff_and_levels(self):
        self.assertEqual(cutoff_of(2001 * 2), datetime.date(2001, 1, 1))
        self.assertEqual(cutoff_of(2001 * 2 + 1), datetime.date(2001, 7, 1))
        lv = {month_key(datetime.date(2000, 3, 1)): 10.0, month_key(datetime.date(2000, 6, 1)): 12.0}
        self.assertEqual(level_at(lv, month_key(datetime.date(2000, 4, 1))), 10.0)
        self.assertIsNone(level_at(lv, month_key(datetime.date(1999, 12, 1))))
        self.assertAlmostEqual(ann(2.0, 10), (2 ** 0.1 - 1) * 100)

    def test_csv_loader_takes_the_last_numeric_column_and_the_months_last_row(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "x.csv")
            with open(p, "w", newline="") as fh:
                fh.write("Date,Open,High,Low,Close,Adj Close,Volume\n2000-01-03,1,2,0,1.5,1.4,100\n2000-01-31,1,2,0,1.7,1.6,100\n2000-02-29,1,2,0,1.8,1.9,100\nbad,row\n")
            lv = load_levels(p)
            self.assertEqual(lv[month_key(datetime.date(2000, 1, 1))], 100.0, "Yahoo's last column is Volume: the loader takes the last numeric column, so pass a date,level file or strip columns")
            with open(p, "w", newline="") as fh:
                fh.write("date,level\n2000-01-03,1.4\n2000-01-31,1.6\n2000-02-29,1.9\n")
            lv = load_levels(p)
            self.assertEqual((lv[month_key(datetime.date(2000, 1, 1))], lv[month_key(datetime.date(2000, 2, 1))]), (1.6, 1.9))

    def test_report_runs_on_a_golden_with_synthetic_series(self):
        here = os.path.dirname(os.path.abspath(__file__))
        path = os.path.join(here, "..", "tests", "fixture", "backtest-12-pit.golden")
        if not os.path.exists(path):
            self.skipTest("no fixture here")
        with open(path) as fh:
            rows = rr.parse_rows(fh.read(), "pit")
        bench = {m: 100 * 1.07 ** ((m - 1990 * 12) / 12.0) for m in range(1990 * 12, 2027 * 12)}
        factor = {m: 100 * 1.09 ** ((m - 1990 * 12) / 12.0) for m in range(1990 * 12, 2027 * 12)}
        rep = reprice(rows[0]["books"], rows[0]["years"], bench, factor)
        self.assertTrue(rep, "the golden's buckets price")
        for b, (e, f) in rep.items():
            self.assertAlmostEqual(f, e + 7.0 - 9.0, places=6)


if __name__ == "__main__":
    main()
