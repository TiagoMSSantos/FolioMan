#!/usr/bin/env python3
"""(#1450) What one knob moves on the published page: a three-way diff of `screen` payloads.

screen-ab.yml runs `screen` three times on one runner: control, the knob arm, control again. The two controls ran on
the same settings, so whatever differs between them is noise (a price moved while the job ran, a feed answered
differently). A row or a cell counts for the KNOB only when both controls agree and the arm does not.

Reads `.screen_web.json` (every top-level array of rows; a row is a list of [column, value, link, tip] cells, keyed
by its TICKER cell) and `.screen_universe.json` (`quotes`, keyed by ticker; an object or a list is one column a
leaf, so `perf[10][1]`, the 20Y leg's percent, is read apart from `perf[10][0]`, its then-price label, which is
written at the live exchange rate and moves between two screens of one day).
The universe has thousands of rows, so its list prints five changes a column; the counts are always whole.

    python3 -I tools/screen_ab.py c1.json arm.json c2.json
    python3 -I tools/screen_ab.py --selftest
"""
import json, sys
from collections import Counter


def keyed(rows):
    """{row key: {column: value}} for a table of cell rows. A tip is its own column, so a moved tip is seen."""
    out = {}
    for r in rows:
        cells = {}
        for c in r:
            cells[c[0]] = c[1]
            if len(c) > 3 and c[3] is not None:
                cells[c[0] + " (tip)"] = c[3]
        key = cells.get("TICKER") or " | ".join(str(c[1]) for c in r if c[0] != "RANK")[:80]
        n = sum(1 for k in out if k == key or k.startswith(key + " #"))
        out[f"{key} #{n + 1}" if n else str(key)] = cells
    return out


def flat(row, k, x):
    """One column a leaf of a quote's member: `fund.roic`, `perf[10][1]`."""
    if isinstance(x, (dict, list)) and x:
        for k2, x2 in x.items() if isinstance(x, dict) else enumerate(x):
            flat(row, f"{k}.{k2}" if isinstance(x, dict) else f"{k}[{k2}]", x2)
    else:
        row[k] = x


def tables(doc):
    """{table: {row key: {column: value}}}. Top-level values that are not row tables form the one row of `(top)`."""
    out = {"(top)": {"": {}}}
    for name, v in doc.items():
        if name == "quotes":
            out[name] = {}
            for q in v:
                row = out[name][q["ticker"]] = {}
                for k, x in q.items():
                    flat(row, k, x)
        elif isinstance(v, list) and v and all(isinstance(r, list) for r in v):
            out[name] = keyed(v)
        else:
            out["(top)"][""][name] = v
    return out


def show(v):
    s = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)
    return s if len(s) <= 60 else s[:57] + "..."


def diff(c1, arm, c2, cap=None, out=print):
    """Print the listing; return ({table: Counter of row and cell counts}, [knob change lines])."""
    t1, ta, t2 = tables(c1), tables(arm), tables(c2)
    counts, cols, lines, shown = {}, Counter(), [], Counter()
    for t in dict.fromkeys([*t1, *ta, *t2]):
        r1, ra, r2 = t1.get(t, {}), ta.get(t, {}), t2.get(t, {})
        n = counts[t] = Counter()
        for k in dict.fromkeys([*r1, *ra, *r2]):
            if (k in r1) != (k in r2):
                n["noise rows"] += 1
            elif (k in r1) != (k in ra):
                word, row = ("LEFT", r1[k]) if k in r1 else ("ENTERED", ra[k])
                n[word.lower()] += 1
                at = " ".join(f"{c}={show(row[c])}" for c in ("RANK", "BUY%", "SCORE") if c in row)
                lines.append(f"{t}  {k}  {word}  {at}".rstrip())
            elif k in r1:
                for col in dict.fromkeys([*r1[k], *ra[k], *r2[k]]):
                    a, b, c = r1[k].get(col), ra[k].get(col), r2[k].get(col)
                    kind = "noise" if a != c else "knob" if a != b else None
                    if kind:
                        n[kind + " cells"] += 1
                        cols[t, col, kind] += 1
                    if kind == "knob" and (cap is None or shown[t, col] < cap):
                        shown[t, col] += 1
                        lines.append(f"{t}  {k}  {col}  {show(a)} -> {show(b)}")
    out("== tables: rows control 1 / arm / control 2, then what the knob moved, then noise")
    for t, n in counts.items():
        out(f"{t}  rows {len(t1.get(t, {}))}/{len(ta.get(t, {}))}/{len(t2.get(t, {}))}  entered {n['entered']}"
            f"  left {n['left']}  knob cells {n['knob cells']}  |  noise rows {n['noise rows']}"
            f"  noise cells {n['noise cells']}")
    out("== columns with a change: knob cells, noise cells (a noisy column carries no knob claim on its noisy rows)")
    for t, col in dict.fromkeys((t, col) for t, col, _ in cols):
        out(f"{t}  {col}  knob {cols[t, col, 'knob']}  noise {cols[t, col, 'noise']}")
    out("== knob changes: table, row, column, controls -> arm" + (f" (first {cap} a column)" if cap else ""))
    for line in lines:
        out(line)
    return counts, lines


def selftest():
    def row(rank, ticker, price, y20, tip=None):
        return [["RANK", rank], ["TICKER", ticker], ["PRICE", price], ["20Y", y20, None, tip]]

    c1 = {"generated": "t1", "degraded": [], "stocks": [row(1, "AAA", 10, "5%"), row(2, "BBB", 20, "7%", "old"),
                                                        row(3, "CCC", 30, "1%")]}
    arm = {"generated": "t2", "degraded": [], "stocks": [row(1, "AAA", 10, "5%"), row(2, "BBB", 20, "9%", "new"),
                                                         row(3, "DDD", 40, "2%")]}
    c2 = {"generated": "t3", "degraded": [], "stocks": [row(1, "AAA", 11, "5%"), row(2, "BBB", 20, "7%", "old"),
                                                        row(3, "CCC", 30, "1%"), row(4, "EEE", 50, "3%")]}
    printed = []
    counts, lines = diff(c1, arm, c2, out=printed.append)
    assert counts["stocks"] == {"noise cells": 1, "knob cells": 2, "left": 1, "entered": 1, "noise rows": 1}, counts
    assert counts["(top)"] == {"noise cells": 1}, counts  # `generated` differs between the controls
    assert lines == ["stocks  BBB  20Y  7% -> 9%", "stocks  BBB  20Y (tip)  old -> new", "stocks  CCC  LEFT  RANK=3",
                     "stocks  DDD  ENTERED  RANK=3"], lines
    assert "stocks  rows 3/3/4  entered 1  left 1  knob cells 2  |  noise rows 1  noise cells 1" in printed, printed
    assert "stocks  20Y  knob 1  noise 0" in printed and "stocks  PRICE  knob 0  noise 1" in printed, printed
    # an arm equal to its controls lists nothing, and a forged control (the arm run twice) reads as noise, not knob
    assert diff(c1, c1, c1, out=lambda _: None)[1] == []
    assert diff(c1, arm, arm, out=lambda _: None)[1] == []
    # rows without a ticker key on their first cells; a repeated key is numbered
    assert list(keyed([[["DAY", "Mon"], ["HEADLINE", "x"]], [["DAY", "Mon"], ["HEADLINE", "x"]]])) == \
        ["Mon | x", "Mon | x #2"]
    # the universe: keyed by ticker, objects and lists one column a leaf, five changes a column at most
    def uni(x, n=8):
        return {"quotes": [{"ticker": f"T{i}", "fund": {"roic": x}, "perf": [["p", 5], ["q", x]]} for i in range(n)]}
    counts, lines = diff(uni(1), uni(2), uni(1), cap=5, out=lambda _: None)
    assert counts["quotes"] == {"knob cells": 16} and len(lines) == 10, (counts, lines)
    assert lines[:2] == ["quotes  T0  fund.roic  1 -> 2", "quotes  T0  perf[1][1]  1 -> 2"], lines
    # a leg's label that moved between the controls is noise on the label alone: its percent keeps the knob read
    drift = uni(1)
    drift["quotes"][0]["perf"][1][0] = "r"
    counts, _ = diff(uni(1), uni(2), drift, cap=5, out=lambda _: None)
    assert counts["quotes"] == {"knob cells": 16, "noise cells": 1}, counts
    print("screen_ab selftest ok")


def main():
    if "--selftest" in sys.argv:
        return selftest()
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    docs = [json.load(open(p, encoding="utf-8")) for p in sys.argv[1:]]
    diff(*docs, cap=5 if "quotes" in docs[0] else None)


if __name__ == "__main__":
    main()
