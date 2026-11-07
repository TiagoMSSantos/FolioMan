#!/usr/bin/env python3
"""Re-grade finished `backtest rescore` logs under the RU1-RU3 ruler (#1306), with no new run.

(#1306) Three of round 13's decisions turned on ONE window of 46 in the same 8y rank-1 head-to-head cell
((#1289) shipped on 17/46 -> 18/46, (#1290) `capex_abn` refused on 18/46 -> 17/46, (#1287) `underdep`
flipped on a 0.1 step), and the worst-window guard reads n = 1-2 overlapping windows that all hold 2008.
This tool reads the per-bucket `BOOK b<bucket> <excess> pts/yr | T1 +x% T2 +y% ...` lines every report
already prints (the top-10 book in SCORE ORDER, each name's cumulative return) and grades each grid row
against row 0 on:

  RU1  top-5 vs 6-10, pooled: the top-5 book's equal-weight terminal multiple beats the 6-10 book's in
       >= 50 % of the windows that hold 6+ names, AND not below row 0 by more than one window in 40.
  RU2  tail: the 20th-percentile bucket excess, and the mean of the five worst buckets, each not below
       row 0 by more than 0.5 pts/yr.
  RU3  PRIMARY: the median AND the 10 %-trimmed mean of the bucket excess, each not down more than 0.1
       pts/yr on row 0 (the (#1291) thresholds, two robust moments in place of mean + median).

Beside them it re-reads the (#1291) guards as the log printed them (mean/median, worst window, rank-1
head-to-head, pool, null band) so a row that flips between the two rulers is visible in one table. The
rank-1 and 2-5 tallies the log prints are the cross-check for RU1's own count.

Input: one or more logs, raw from `gh run view --log` or bare. A `rescore` log holds several
`══ rescore row {json} ══` blocks per horizon (a stress log runs 20, 12 and 8 in one job); a plain
`backtest` log is one row, named after the file. The first row of each horizon is its control. Pass a
grid's 12y, 8y, 20y and stress logs together and the summary reads each rule on the lane and horizons the
(#1291) cell it replaces read; `--primary dead` moves RU3 and the band to the `dead` lane.

Not wired into the binary: run it by hand on downloaded probe logs (`gh run view <id> --log`, or the
`Probe dump` artifact), and quote the table in the receipt. `--selftest` runs the unit tests.
"""
import argparse, contextlib, io, json, math, os, re, sys, unittest

ROW = re.compile(r"^══ rescore row (.*) ══\s*$")
HOLD = re.compile(r"HOLD (\d+)y no-sell")
BOOK = re.compile(r"^\s*BOOK b(-?\d+) (?:([+-]\d+\.\d) pts/yr \| (.*)|empty)\s*$")
NAME = re.compile(r"^(\S+) ([+-]\d+)%$")
H2H_LOW = re.compile(r"head-to-head vs the 6-10 book \(#135\): #1 beat it in (\d+)/(\d+) \(\d+%\), the 2-5 book did in (\d+)/(\d+)")
H2H_MID = re.compile(r"#1 beat the 11-20 book in (\d+)/(\d+)")
TOP10 = re.compile(r"^\s*top-10 book .*excess ([+-]\d+\.\d) \(med ([+-]\d+\.\d)\) pts/yr.*worst ([+-]\d+\.\d)")
POOL = re.compile(r"POOL CENSUS \(#149\): (\d+) bucket\(s\), median gated pool (\d+)")
BAND = re.compile(r"^\s*row (.*): gain mean ([+-]\d+\.\d\d) \| med ([+-]\d+\.\d\d) ; null band .* -> (OUTSIDE|INSIDE)\s*$")
TAIL = re.compile(r"^\s*row (.*): tail gain p20 ([+-]\d+\.\d\d) \| worst5 ([+-]\d+\.\d\d) ; tail band .* -> TAIL (OUTSIDE|INSIDE)\s*$")  # (#1317) RU-T
RIVAL_BOOK = re.compile(r"^\s*BOOK rival (\S+) b(-?\d+) ([+-]\d+\.\d) pts/yr\s*$")  # (round 16 §4.1) one line per rivals row per bucket, excess only
PREFIX = re.compile(r"^[^\t]*\t[^\t]*\t\S+Z ")  # `gh run view --log`: job, step, timestamp
NULL_HDR = "── NULL BAND (#1292)"
STRESS = "backtest: STRESS"
SLACK_DENOM = 40  # RU1: one window of slack per 40 graded windows
TAIL_SLACK = 0.5  # RU2: pts/yr a tail cell may sit under row 0
PRIMARY_SLACK = 0.1  # RU3: pts/yr a robust moment may sit under row 0 (the (#1291) number)
ERA_MIN = 4  # RU-E: an era is read when both rows hold at least this many buckets in it
ERA_SLACK = 0.3  # RU-E: pts/yr the median gain over row 0 may sit below zero in the row's worst era


def percentile(xs, p):
    """Nearest-rank percentile of `xs` (not sorted in place), None when empty. (#1292)'s form."""
    if not xs:
        return None
    s = sorted(xs)
    k = max(1, math.ceil(p / 100.0 * len(s)))
    return s[min(k, len(s)) - 1]


def trimmed_mean(xs, share=0.10):
    """Mean after dropping floor(share * n) values from each end; the plain mean under 10 values."""
    if not xs:
        return None
    s = sorted(xs)
    k = int(len(s) * share)
    core = s[k:len(s) - k] if k else s
    return sum(core) / len(core)


def median(xs):
    if not xs:
        return None
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2.0


def worst_mean(xs, k=5):
    """Mean of the k lowest values, or of all of them under k."""
    if not xs:
        return None
    s = sorted(xs)[:k]
    return sum(s) / len(s)


ERAS = ((None, 2001, "<=2001"), (2002, 2007, "2002-07"), (2008, 2013, "2008-13"), (2014, None, ">=2014"))


def era_of(bucket):
    """The cutoff era of a `BOOK b<bucket>` id: the bucket is year * 2 + half (`backtest::bucket`)."""
    year = bucket // 2
    for lo, hi, name in ERAS:
        if (lo is None or year >= lo) and (hi is None or year <= hi):
            return name
    return "?"


def era_stats(books):
    """Per era: (n, mean, median) of the bucket excess of one row's books; eras with no bucket are left out."""
    out = {}
    for b, (e, _) in books.items():
        if e is not None:
            out.setdefault(era_of(b), []).append(e)
    return {k: (len(v), sum(v) / len(v), median(v)) for k, v in out.items()}


def split_halves(names):
    """(top-5 multiple, 6-10 multiple) of one BOOK line's names in rank order, None under 6 names.
    Equal-weight terminal multiples, as the book itself is priced (mean of 1 + r/100)."""
    if len(names) < 6:
        return None
    mult = lambda rs: sum(1.0 + r / 100.0 for r in rs) / len(rs)
    return mult([r for _, r in names[:5]]), mult([r for _, r in names[5:10]])


def parse_book(line):
    """`(bucket, excess, [(name, cum%), ...])` for a BOOK line, excess None for `empty`; None otherwise."""
    m = BOOK.match(line)
    if not m:
        return None
    bucket = int(m.group(1))
    if m.group(2) is None:
        return bucket, None, []
    names = []
    for tok in re.findall(r"\S+ [+-]\d+%", m.group(3)):
        n = NAME.match(tok)
        names.append((n.group(1), float(n.group(2))))
    return bucket, float(m.group(2)), names


def lane_of(text):
    """The lane a log graded: `stress` once the loser injection ran, `dead` with the (#751) members, else `pit`."""
    return "stress" if STRESS in text else ("dead" if "DEAD (#751)" in text else "pit")


def new_row(label, group):
    return {"label": label, "group": group, "years": None, "books": {}, "h2h": None, "h2h_mid": None, "top10": None, "pool": None, "band": None, "tail": None}


def parse_rows(text, label):
    """The rows of one log in print order: [{label, group, years, books, h2h, top10, pool, band}].
    A multi-horizon rescore log prints each horizon as a plain run (the preamble) then its rows then its band:
    a row label seen twice opens a new group with its own control, and nothing is filed from the band header
    or a `backtest: STRESS` line or a change of HOLD years until the next row header. A log with no row header
    is one row named `label`. The gh `probe<TAB>backtest<TAB><time>Z ` prefix is stripped first."""
    lines = [PREFIX.sub("", l, count=1) for l in text.splitlines()]
    rescore = any(ROW.match(l) for l in lines)
    rows, seen, group = [], {}, 0
    rivals, held = {}, None  # (#1318) rival rows by (id, HOLD years); `held` = the last HOLD years printed, filed or not
    cur = None if rescore else new_row(label, 0)
    if cur:
        rows.append(cur)
    for line in lines:
        h = HOLD.search(line)
        if h:
            held = int(h.group(1))
        m = RIVAL_BOOK.match(line)
        if m:  # (§4.1) a rivals row's per-bucket excess, filed as its own row `rival:<id>` on its horizon's group below.
            # (#1318) Matched before the `cur` skip: a rescore log prints the RIVALS block in the walk preamble, before
            # any row header, and the 8y block before the 8y control opens its group.
            r = rivals.setdefault((m.group(1), held), new_row("rival:" + m.group(1), None))
            r["years"] = held
            r["books"].setdefault(int(m.group(2)), (float(m.group(3)), []))
            continue
        m = ROW.match(line)
        if m:
            if m.group(1) in seen:
                group, seen = group + 1, {}
            cur = seen[m.group(1)] = new_row(m.group(1), group)
            rows.append(cur)
            continue
        m = BAND.match(line)
        if m:  # printed after the group's last row, so file it on the row it names in this group
            if m.group(1) in seen:
                seen[m.group(1)]["band"] = (float(m.group(2)), float(m.group(3)), m.group(4))
            continue
        m = TAIL.match(line)
        if m:  # (#1317) the tail band, printed beside the PRIMARY band once the Rust side prints it
            if m.group(1) in seen:
                seen[m.group(1)]["tail"] = (float(m.group(2)), float(m.group(3)), m.group(4))
            continue
        if NULL_HDR in line or STRESS in line:
            cur = None
            continue
        if cur is None:
            continue
        h = HOLD.search(line)
        if h:
            if cur["years"] is None:
                cur["years"] = int(h.group(1))
            elif int(h.group(1)) != cur["years"]:  # the next horizon's preamble, no band between
                cur = None
                continue
        b = parse_book(line)
        if b and b[0] not in cur["books"]:  # the first block only: `BOOK stock`/`BOOK coin` do not match
            cur["books"][b[0]] = (b[1], b[2])
            continue
        m = H2H_LOW.search(line)
        if m and cur["h2h"] is None:
            cur["h2h"] = tuple(int(g) for g in m.groups())  # (h1, n, h25, n)
            continue
        m = H2H_MID.search(line)
        if m and cur["h2h_mid"] is None:
            cur["h2h_mid"] = (int(m.group(1)), int(m.group(2)))
            continue
        m = TOP10.match(line)
        if m and cur["top10"] is None:
            cur["top10"] = tuple(float(g) for g in m.groups())  # (mean, median, worst)
            continue
        m = POOL.search(line)
        if m and cur["pool"] is None:
            cur["pool"] = (int(m.group(1)), int(m.group(2)))
    groups = {}
    for r in rows:
        groups.setdefault(r["years"], r["group"])
    for r in rivals.values():  # a horizon with no graded row has no control to read against: left out
        if r["years"] in groups:
            r["group"] = groups[r["years"]]
            rows.append(r)
    return rows


def grade_row(row):
    """RU1-RU3 statistics of one row, plus the old guards as the log printed them."""
    excess = [e for e, _ in row["books"].values() if e is not None]
    wins = n = 0
    for e, names in row["books"].values():
        h = split_halves(names)
        if h is None:
            continue
        n += 1
        wins += h[0] > h[1]
    return {
        "buckets": len(excess),
        "mean": sum(excess) / len(excess) if excess else None,
        "median": median(excess),
        "trimmed": trimmed_mean(excess),
        "worst": min(excess) if excess else None,
        "p20": percentile(excess, 20),
        "worst5": worst_mean(excess),
        "eras": era_stats(row["books"]),
        "ru1_wins": wins,
        "ru1_n": n,
        "ru1_pct": 100.0 * wins / n if n else None,
        "rank1": row["h2h"],
        "top10": row["top10"],
        "pool": row["pool"],
        "band": row["band"],
        "tail": row["tail"],
    }


def verdicts(g, c):
    """Per-cell pass/fail of a graded row `g` against its control `c`; None where a cell cannot read."""
    out = {}
    if g["ru1_n"] and c["ru1_n"]:
        slack = 100.0 * max(1, g["ru1_n"] // SLACK_DENOM) / g["ru1_n"]
        # (#1383) 1e-9: 100*29/46 sits one ulp under 100*30/46 - 100/46, so one window down read as two.
        out["RU1"] = g["ru1_pct"] >= 50.0 and g["ru1_pct"] >= c["ru1_pct"] - slack - 1e-9
    else:
        out["RU1"] = None
    out["RU2"] = (g["p20"] >= c["p20"] - TAIL_SLACK and g["worst5"] >= c["worst5"] - TAIL_SLACK) if g["buckets"] and c["buckets"] else None
    out["RU3"] = (g["median"] >= c["median"] - PRIMARY_SLACK and g["trimmed"] >= c["trimmed"] - PRIMARY_SLACK) if g["buckets"] and c["buckets"] else None
    # the (#1291) guards as printed, for the flip column
    out["old_primary"] = (g["mean"] >= c["mean"] - PRIMARY_SLACK and g["median"] >= c["median"] - PRIMARY_SLACK) if g["buckets"] and c["buckets"] else None
    out["old_worst"] = (g["worst"] >= c["worst"] - TAIL_SLACK) if g["buckets"] and c["buckets"] else None
    if g["rank1"] and c["rank1"]:
        gp, cp = g["rank1"][0] / max(1, g["rank1"][1]), c["rank1"][0] / max(1, c["rank1"][1])
        out["old_rank1"] = gp >= cp
    else:
        out["old_rank1"] = None
    out["old_pool"] = (g["pool"][1] >= 0.8 * c["pool"][1]) if g["pool"] and c["pool"] else None
    out["band"] = (g["band"][2] == "OUTSIDE") if g["band"] else None
    # (#1317) RU-T: TAIL when the row's p20 and worst-five gains sit outside their own null band and RU3 holds
    # (#1318) both gains must be up: a tail outside its band on the losing side is RU2's failure, not TAIL
    out["RU_T"] = (g["tail"][2] == "OUTSIDE" and g["tail"][0] > 0 and g["tail"][1] > 0 and bool(out["RU3"])) if g.get("tail") else None
    # (#1306 L1) RU-E: no era pays for another. The row's median gain over control in its worst era, over the
    # eras where both rows hold >= ERA_MIN buckets; None when no era qualifies.
    ge, ce = g.get("eras", {}), c.get("eras", {})
    gaps = [ge[k][2] - ce[k][2] for k in ge if k in ce and ge[k][0] >= ERA_MIN and ce[k][0] >= ERA_MIN]
    out["RU_E"] = (min(gaps) >= -ERA_SLACK) if gaps else None
    out["era_worst_gap"] = min(gaps) if gaps else None
    return out


def fmt(x, w="+.1f"):
    return "   -" if x is None else format(x, w)


def pf(v):
    return "  -  " if v is None else ("pass " if v else "FAIL ")


def report(logs, primary="pit"):
    """Print one table per (log, horizon) group and the cross-horizon summary. `logs` = [(label, text)].
    The summary reads each rule on the cells (#1291) read its old twin on: RU3 and the band on the `primary`
    lane at 12y and 8y, RU1 there and at 20y stress, RU2 on the primary lane at 20y, 12y and 8y, the pool
    everywhere."""
    per_row = {}  # row label -> {(log, lane, years): verdicts}
    for label, text in logs:
        lane, rows = lane_of(text), parse_rows(text, label)
        if not any(r["books"] for r in rows):
            print(f"{label}: no BOOK lines, nothing to grade")
            continue
        for gi in sorted({r["group"] for r in rows}):
            grp = [r for r in rows if r["group"] == gi]
            years = grp[0]["years"]
            print(f"\n── {label} {lane}: {len(grp)} row(s), HOLD {years}y; row 0 = control ──")
            print("  row                                      n  mean   med  trim  | worst   p20 worst5 | RU1 wins/n (rank-1, 2-5 printed) | RU1  RU2  RU3  | old: prim worst rank1 pool | band")
            grades = [grade_row(r) for r in grp]
            for r, g in zip(grp, grades):
                v = verdicts(g, grades[0])
                r1 = f"{g['ru1_wins']:>3}/{g['ru1_n']:<3} {fmt(g['ru1_pct'], '5.1f')}%" if g["ru1_n"] else "   -       "
                printed = f"({g['rank1'][0]}/{g['rank1'][1]}, {g['rank1'][2]}/{g['rank1'][3]})" if g["rank1"] else "(-)"
                print(
                    f"  {r['label'][:40]:<40} {g['buckets']:>3} {fmt(g['mean'])} {fmt(g['median'])} {fmt(g['trimmed'])}  | "
                    f"{fmt(g['worst'])} {fmt(g['p20'])} {fmt(g['worst5'])}  | {r1} {printed:<14} | "
                    f"{pf(v['RU1'])}{pf(v['RU2'])}{pf(v['RU3'])} | {pf(v['old_primary'])}{pf(v['old_worst'])}{pf(v['old_rank1'])}{pf(v['old_pool'])} | "
                    f"{'OUTSIDE' if v['band'] else ('INSIDE' if v['band'] is not None else '-')}"
                )
                per_row.setdefault(r["label"], {})[(label, lane, years)] = v
            # (#1306 L1) the era table: each row's median excess per cutoff era, and its gain over row 0
            eras = [k for _, _, k in ERAS if any(k in g["eras"] for g in grades)]
            print("  ERA (median excess | gain over row 0; n buckets):  " + "   ".join(f"{k:<22}" for k in eras) + "   RU-E")
            for r, g in zip(grp, grades):
                cells = []
                for k in eras:
                    if k in g["eras"]:
                        n_, _, med = g["eras"][k]
                        gain = med - grades[0]["eras"][k][2] if k in grades[0]["eras"] else None
                        cells.append(f"{med:+5.1f} | {fmt(gain, '+5.1f') if gain is not None else '  -  '} ({n_:>2})".ljust(22))
                    else:
                        cells.append("-".ljust(22))
                v = verdicts(g, grades[0])
                print(f"  {r['label'][:40]:<40}   " + "   ".join(cells) + f"   {pf(v['RU_E'])}")
    prim = lambda lane, y: lane == primary and y in (12, 8)
    rank = lambda lane, y: prim(lane, y) or (lane == "stress" and y == 20)
    tail = lambda lane, y: lane == primary and y in (20, 12, 8)
    print(f"\n── SUMMARY (#1306), primary lane {primary}: RU = RU3 + band at 12y/8y, RU1 at 12y/8y + 20y stress, RU2 at 20y/12y/8y, pool everywhere; old = the (#1291) cells on the same lines ──")
    for label, cells in per_row.items():
        got = lambda key, where: [v[key] for (_, lane, y), v in cells.items() if where(lane, y) and v[key] is not None]
        pool = all(got("old_pool", lambda lane, y: True))
        p, op = got("RU3", prim), got("old_primary", prim)
        ru_pass = bool(p) and all(p) and all(got("RU1", rank)) and all(got("RU2", tail)) and pool
        era_ok = all(got("RU_E", prim))  # (#1306 L1) RU-E is reported, not yet in the pass: it is pre-registered in round 16, not adopted
        old_pass = bool(op) and all(op) and all(got("old_rank1", rank)) and all(got("old_worst", tail)) and pool
        bands = got("band", prim)
        band = "OUTSIDE" if bands and all(bands) else ("INSIDE" if bands else "n/a")
        tails = got("RU_T", prim)
        tail_s = "TAIL" if tails and all(tails) else ("-" if tails else "n/a")  # not `tail`: that name is the RU2 horizon filter above
        flip = "  <- FLIPS" if ru_pass != old_pass else ""
        print(f"  RU {'pass' if ru_pass else 'FAIL'}   old {'pass' if old_pass else 'FAIL'}   band {band:<7}   tail {tail_s:<4}   RU-E {'pass' if era_ok else 'FAIL'}   cells {len(cells)}   {label}{flip}")


class Tests(unittest.TestCase):
    def test_book_line_parses_rank_order_and_empty(self):
        b = parse_book("  BOOK b4003 +2.6 pts/yr | VFC +518% M +150% COF +7% ASML +201% MMM +143% JBL -29% NOK -82% AEE -9% F -10% BA +251%")
        self.assertEqual(b[0], 4003)
        self.assertEqual(b[1], 2.6)
        self.assertEqual(b[2][0], ("VFC", 518.0))
        self.assertEqual(b[2][9], ("BA", 251.0))
        self.assertEqual(parse_book("  BOOK b3988 empty"), (3988, None, []))
        self.assertIsNone(parse_book("  BOOK stock b3988 +0.0 pts/yr | ^GSPC +178%"), "the stock-only twin is not the graded book")
        self.assertEqual(parse_book("  BOOK b3992 -1.1 pts/yr | INTC +146% ^GSPC +109% T +54%")[2][1], ("^GSPC", 109.0), "an index filler is a name")

    def test_split_halves_needs_six_names(self):
        names = [("A", 100.0), ("B", 0.0), ("C", -50.0), ("D", 20.0), ("E", 30.0), ("F", 10.0), ("G", -10.0)]
        top, low = split_halves(names)
        self.assertAlmostEqual(top, (2.0 + 1.0 + 0.5 + 1.2 + 1.3) / 5)
        self.assertAlmostEqual(low, (1.1 + 0.9) / 2)
        self.assertIsNone(split_halves(names[:5]))

    def test_statistics(self):
        xs = [-7.0, -4.0, -1.0, 0.0, 0.5, 1.0, 2.0, 3.0, 3.5, 9.0]
        self.assertEqual(percentile(xs, 20), -4.0)
        self.assertEqual(median(xs), 0.75)
        self.assertAlmostEqual(trimmed_mean(xs), sum(xs[1:-1]) / 8)
        self.assertAlmostEqual(worst_mean(xs), (-7.0 - 4.0 - 1.0 + 0.0 + 0.5) / 5)
        self.assertAlmostEqual(trimmed_mean([1.0, 2.0]), 1.5, msg="under 10 values nothing is trimmed")

    def test_rescore_rows_split_and_band_files_on_its_row(self):
        text = (
            "══ rescore row {} ══\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD 12y no-sell, vs ^GSPC) ──\n"
            "  POOL CENSUS (#149): 2 bucket(s), median gated pool 100, graded basket 10 -> 0 saturated (pool <= basket), 5 substitutable name(s); x\n"
            "  top-10 book +6.0%/yr  vs S&P500 +6.6%/yr  ->  excess -0.7 (med -0.3) pts/yr   win 37% of 41   worst -5.9   OOS -0.7/-0.7\n"
            "  BOOK b1 +1.0 pts/yr | A +10% B +20% C +30% D +40% E +50% F +1% G +2% H +3% I +4% J +5%\n"
            "  BOOK b2 -3.0 pts/yr | A +10% B +20% C +30% D +40% E +50% F +200% G +2% H +3% I +4% J +5%\n"
            "  BOOK stock b1 +1.0 pts/yr | A +10%\n"
            "    head-to-head vs the 6-10 book (#135): #1 beat it in 1/2 (50%), the 2-5 book did in 2/2 (100%) — denominator 2 here vs 1\n"
            "══ rescore row {\"k\":1} ══\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD 12y no-sell, vs ^GSPC) ──\n"
            "  BOOK b1 +1.5 pts/yr | A +10% B +20% C +30% D +40% E +50% F +1% G +2% H +3% I +4% J +5%\n"
            "  BOOK b2 empty\n"
            "\n── NULL BAND (#1292): ... ──\n"
            "  row {\"k\":1}: gain mean +0.50 | med +0.50 ; null band mean [-0.10,+0.10] med [-0.10,+0.10] (200) -> OUTSIDE\n"
        )
        rows = parse_rows(text, "x")
        self.assertEqual([r["label"] for r in rows], ["{}", '{"k":1}'])
        self.assertEqual(rows[0]["years"], 12)
        self.assertEqual(rows[0]["h2h"], (1, 2, 2, 2))
        self.assertEqual(rows[0]["pool"], (2, 100))
        self.assertEqual(rows[0]["top10"], (-0.7, -0.3, -5.9))
        self.assertEqual(rows[1]["band"], (0.5, 0.5, "OUTSIDE"))
        g0, g1 = grade_row(rows[0]), grade_row(rows[1])
        self.assertEqual((g0["ru1_wins"], g0["ru1_n"]), (1, 2), "b1's top-5 beats its 6-10, b2's does not")
        self.assertEqual(g0["buckets"], 2)
        self.assertEqual(g1["buckets"], 1, "an empty bucket is not a graded window")
        v = verdicts(g1, g0)
        self.assertTrue(v["RU3"])
        self.assertTrue(v["band"])
        self.assertIsNone(v["old_rank1"], "row 1 printed no head-to-head line")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            report([("x", text)], "pit")  # (#1318) the summary loop once rebound `tail` and crashed on row 2
        self.assertEqual(sum(1 for l in out.getvalue().splitlines() if l.startswith("  RU ")), 2)

    def test_prefixed_multi_horizon_log_groups_per_horizon(self):
        P = "probe\tbacktest\t2026-11-02T10:24:53.0000000Z "
        hdr = lambda y: P + f"── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD {y}y no-sell, vs ^GSPC) ──\n"
        book = lambda b, e: P + f"  BOOK b{b} {e} pts/yr | A +90% B +80% C +70% D +60% E +50% F +1% G +2% H +3% I +4% J +5%\n"
        text = (
            P + "backtest: STRESS — injecting 17 crashed/delisted losers\n" + hdr(20) + book(99, "+9.0")
            + P + "══ rescore row {} ══\n" + hdr(20) + book(1, "+1.0") + book(2, "+2.0")
            + P + "══ rescore row {\"k\":1} ══\n" + hdr(20) + book(1, "+1.5") + book(2, "+2.5")
            + P + "── NULL BAND (#1292): x ──\n"
            + P + "  row {\"k\":1}: gain mean +0.50 | med +0.50 ; null band mean [-0.10,+0.10] med [-0.10,+0.10] (200) -> OUTSIDE\n"
            + P + "backtest: STRESS — injecting 17 crashed/delisted losers\n" + hdr(12) + book(98, "-9.0")
            + P + "══ rescore row {} ══\n" + hdr(12) + book(1, "+3.0")
            + P + "══ rescore row {\"k\":1} ══\n" + hdr(12) + book(1, "+3.5")
            + hdr(8) + book(97, "+7.0")  # a next horizon's preamble with no band before it
        )
        rows = parse_rows(text, "s")
        self.assertEqual(lane_of(text), "stress")
        self.assertEqual([(r["label"], r["group"], r["years"]) for r in rows], [("{}", 0, 20), ('{"k":1}', 0, 20), ("{}", 1, 12), ('{"k":1}', 1, 12)])
        self.assertEqual(sorted(rows[0]["books"]), [1, 2], "the preamble's bucket 99 is not row 0's")
        self.assertEqual(sorted(rows[3]["books"]), [1], "the 8y preamble does not bleed into the last 12y row")
        self.assertEqual(rows[1]["band"], (0.5, 0.5, "OUTSIDE"))
        self.assertIsNone(rows[3]["band"], "a band files on its own group only")
        self.assertEqual(lane_of("x\n  DEAD (#751): y"), "dead")
        self.assertEqual(lane_of("x"), "pit")

    def test_tail_band_line_files_on_its_row(self):
        text = (
            "══ rescore row {} ══\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD 8y no-sell, vs ^GSPC) ──\n"
            "  BOOK b4010 +1.0 pts/yr | A +10% B +20% C +30% D +40% E +50% F +1% G +2% H +3% I +4% J +5%\n"
            "══ rescore row {\"k\":1} ══\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD 8y no-sell, vs ^GSPC) ──\n"
            "  BOOK b4010 +1.0 pts/yr | A +10% B +20% C +30% D +40% E +50% F +1% G +2% H +3% I +4% J +5%\n"
            "\n── NULL BAND (#1292): ... ──\n"
            "  row {\"k\":1}: gain mean +0.10 | med +0.10 ; null band mean [-0.30,+0.30] med [-0.40,+0.40] (200) -> INSIDE\n"
            "  row {\"k\":1}: tail gain p20 +1.20 | worst5 +2.10 ; tail band p20 [-0.60,+0.70] worst5 [-0.90,+1.00] (200) -> TAIL OUTSIDE\n"
        )
        rows = parse_rows(text, "x")
        self.assertEqual(rows[1]["tail"], (1.2, 2.1, "OUTSIDE"))
        self.assertIsNone(rows[0]["tail"])
        v = verdicts(grade_row(rows[1]), grade_row(rows[0]))
        self.assertEqual(v["band"], False)
        self.assertEqual(v["RU_T"], True, "TAIL: outside its own band with RU3 held, while PRIMARY sits inside")
        worse = rows[1] | {"tail": (0.44, -0.71, "OUTSIDE")}  # (#1318) the plus 10 row at 8y: worst five fell
        self.assertEqual(verdicts(grade_row(worse), grade_row(rows[0]))["RU_T"], False)

    def test_rival_book_lines_become_their_own_rows(self):
        text = (
            "══ rescore row {} ══\n── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD 12y no-sell, vs ^GSPC) ──\n"
            "  BOOK b4000 +1.0 pts/yr | A +10% B +20% C +30% D +40% E +50% F +1% G +2% H +3% I +4% J +5%\n"
            "  BOOK rival lt_rev5 b4000 -2.0 pts/yr\n  BOOK rival lt_rev5 b4001 +3.0 pts/yr\n  BOOK rival div_yield b4000 +0.5 pts/yr\n"
        )
        rows = parse_rows(text, "x")
        self.assertEqual([r["label"] for r in rows], ["{}", "rival:lt_rev5", "rival:div_yield"])
        g = grade_row(rows[1])
        self.assertEqual((g["buckets"], g["median"]), (2, 0.5))
        self.assertEqual(rows[1]["years"], 12, "a rival row inherits the group's horizon")
        self.assertEqual(g["eras"]["<=2001"], (2, 0.5, 0.5))

    def test_rival_lines_in_a_rescore_preamble_file_on_their_horizon(self):
        P = "probe\tbacktest\t2026-11-04T08:10:28.0000000Z "
        riv = lambda y, b, e: P + f"── RIVALS (#560): equal-weight top-10 books on one pond, HOLD {y}y no-sell, excess vs the index ──\n" + P + f"  BOOK rival v b{b} {e} pts/yr\n"
        hdr = lambda y: P + f"── vs S&P500 (ABSOLUTE: buy top-N equal-weight, HOLD {y}y no-sell, vs ^GSPC) ──\n"
        book = lambda b: P + f"  BOOK b{b} +1.0 pts/yr | A +10% B +20% C +30% D +40% E +50% F +1% G +2% H +3% I +4% J +5%\n"
        text = (
            hdr(12) + riv(12, 3992, "-2.0") + P + "══ rescore row {} ══\n" + hdr(12) + book(3992)
            + hdr(8) + riv(8, 4000, "+3.0") + P + "══ rescore row {} ══\n" + hdr(8) + book(4000)
        )
        rows = parse_rows(text, "x")  # (#1318) both RIVALS blocks print before their horizon's first row header
        self.assertEqual([(r["label"], r["group"], r["years"], sorted(r["books"])) for r in rows],
                         [("{}", 0, 12, [3992]), ("{}", 1, 8, [4000]), ("rival:v", 0, 12, [3992]), ("rival:v", 1, 8, [4000])])

    def test_era_of_bucket_and_era_stats(self):
        self.assertEqual(era_of(2000 * 2 + 1), "<=2001")
        self.assertEqual(era_of(2002 * 2), "2002-07")
        self.assertEqual(era_of(2013 * 2 + 1), "2008-13")
        self.assertEqual(era_of(2014 * 2), ">=2014")
        books = {2000 * 2: (1.0, []), 2000 * 2 + 1: (3.0, []), 2005 * 2: (-2.0, []), 2005 * 2 + 1: (None, [])}
        st = era_stats(books)
        self.assertEqual(st["<=2001"], (2, 2.0, 2.0))
        self.assertEqual(st["2002-07"], (1, -2.0, -2.0), "an empty bucket is not a graded window")
        c = {"eras": {"<=2001": (5, 1.0, 1.0), "2002-07": (5, 2.0, 2.0)}, "ru1_n": 0, "buckets": 0, "rank1": None, "pool": None, "band": None}
        g = dict(c, eras={"<=2001": (5, 1.0, 0.6), "2002-07": (5, 2.0, 2.5)})
        v = verdicts(g, c)
        self.assertEqual(v["RU_E"], False, "the worst era is 0.4 under control, past the 0.3 slack")
        self.assertAlmostEqual(v["era_worst_gap"], -0.4)
        g2 = dict(c, eras={"<=2001": (3, 1.0, -5.0), "2002-07": (5, 2.0, 2.1)})
        self.assertTrue(verdicts(g2, c)["RU_E"], "an era with under ERA_MIN buckets is not read")

    def test_ru1_slack_is_one_window_per_forty(self):
        c = {"ru1_n": 46, "ru1_pct": 100.0 * 18 / 46, "buckets": 0, "rank1": None, "pool": None, "band": None}
        g = dict(c, ru1_pct=100.0 * 17 / 46)
        self.assertEqual(verdicts(g, c)["RU1"], False, "one window down but under 50 % fails the floor")
        c2 = dict(c, ru1_pct=100.0 * 24 / 46)
        self.assertTrue(verdicts(dict(c2, ru1_pct=100.0 * 23 / 46), c2)["RU1"], "one window down, above 50 %: passes")
        self.assertFalse(verdicts(dict(c2, ru1_pct=100.0 * 22 / 46), c2)["RU1"], "two windows down fails the slack")
        c3 = dict(c, ru1_pct=100.0 * 30 / 46)
        self.assertTrue(verdicts(dict(c3, ru1_pct=100.0 * 29 / 46), c3)["RU1"], "29 of 46 against 30: one window")

    def test_goldens_parse_as_one_row_each(self):
        here = os.path.dirname(os.path.abspath(__file__))
        path = os.path.join(here, "..", "tests", "fixture", "backtest-12-pit.golden")
        if not os.path.exists(path):
            self.skipTest("no fixture here")
        with open(path) as fh:
            rows = parse_rows(fh.read(), "pit")
        self.assertEqual(len(rows), 1)
        g = grade_row(rows[0])
        self.assertEqual(g["buckets"], 41)
        self.assertEqual(g["worst"], rows[0]["top10"][2], "the BOOK minimum is the printed worst window")
        self.assertAlmostEqual(g["mean"], rows[0]["top10"][0], delta=0.051, msg="the BOOK mean is the printed excess")
        self.assertEqual(g["rank1"][:2], (17, 36))  # (#1378) the free-zone 60 rebless moved rank 1's wins 16 -> 17


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("logs", nargs="*", help="rescore or backtest logs; row 0 of each is its control")
    ap.add_argument("--primary", default="pit", choices=["pit", "dead", "stress"], help="lane RU3 and the band read")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        sys.argv = sys.argv[:1]
        unittest.main(module=__name__, verbosity=2)
        return
    if not a.logs:
        ap.error("name at least one log, or --selftest")
    logs = []
    for p in a.logs:
        with open(p, errors="replace") as fh:
            logs.append((os.path.basename(p), fh.read()))
    report(logs, a.primary)


if __name__ == "__main__":
    main()
