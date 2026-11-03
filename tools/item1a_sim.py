#!/usr/bin/env python3
"""(#1316) S2 Lazy Prices: how much each pool filer rewrote its 10-K Item 1A (Risk Factors) from the year before.

Cohen, Malloy and Nguyen (2020, "Lazy Prices") find that firms whose 10-K text changes a lot from one year
to the next under-perform the firms that leave it alone, and that the Risk Factors section carries most of
it. This tool reads Item 1A from EDGAR-CORPUS (Loukas et al. 2021: one JSON line per 10-K, one shard per
filing year), keeps the pool's filers and writes `tests/item1a-sim.csv`: `ticker,year,sim`, where sim is
the cosine of the term counts of the Item 1A filed in `year` and the one filed in `year - 1` (1 = the same
words). A section under 200 words (omitted, "not applicable") writes no row. Item 1A is required from
fiscal years ending December 2005, so the first shard is 2006; the walk's last cutoffs (8 years back)
need 2018 at most.

Pool: `.long_history_cache.json` equities mapped to a CIK through `.eu_listing_cache.json` and
`.sec_cache/_tickers.json`, plus every `tests/dead-roster.csv` row with a CIK. A CIK writes one row per
walk ticker that maps to it; a ticker that maps to two filers writes none.

    python3 -I tools/item1a_sim.py --work DIR --out tests/item1a-sim.csv   # streams ~24 GB, stores none
    python3 -I tools/item1a_sim.py --selftest

Stdlib only. DIR keeps one term-count file per filing year, so a rerun resumes at the first missing year."""
import argparse
import collections
import csv
import json
import math
import os
import re
import sys
import urllib.request

URL = "https://huggingface.co/datasets/eloukas/edgar-corpus/resolve/main/%d/%s.jsonl"
SPLITS = ("train", "test", "validate")
HEAD = re.compile(rb'\{"filename":"[^"]*","cik":"(\d+)"')
WORD = re.compile(r"[a-z]{2,}")
MIN_WORDS = 200


def terms(text):
    w = WORD.findall(text.lower())
    return collections.Counter(w) if len(w) >= MIN_WORDS else None


def cosine(a, b):
    if len(a) > len(b):
        a, b = b, a
    dot = sum(c * b.get(w, 0) for w, c in a.items())
    return dot / math.sqrt(sum(c * c for c in a.values()) * sum(c * c for c in b.values()))


def keep_longer(out, cik, t):
    if t and sum(t.values()) > sum(out.get(cik, {}).values()):
        out[cik] = t


def extract(lines, ciks, out):
    """Fold the lines' Item 1A term counts into `out` for the pool's CIKs; a filer's longest section wins.
    The CIK is read off the line head, so the other filers' lines are never parsed."""
    for line in lines:
        m = HEAD.match(line)
        cik = m and m.group(1).decode().zfill(10)
        if cik in ciks:
            keep_longer(out, cik, terms(json.loads(line).get("section_1A") or ""))
    return out


def year_terms(year, ciks, work):
    path = os.path.join(work, "tf_%d.json" % year)
    if os.path.exists(path):
        with open(path) as f:
            return json.load(f)
    out = {}
    for split in SPLITS:
        req = urllib.request.Request(URL % (year, split), headers={"User-Agent": "folioman-research/1.0"})
        with urllib.request.urlopen(req, timeout=120) as r:
            extract(r, ciks, out)
    with open(path + ".tmp", "w") as f:
        json.dump(out, f)
    os.replace(path + ".tmp", path)
    return out


def pool(root):
    """{cik: sorted walk tickers}."""
    p = lambda *a: os.path.join(root, *a)
    with open(p(".long_history_cache.json")) as f:
        lh = json.load(f)
    with open(p(".eu_listing_cache.json")) as f:
        us_of = {eu.upper(): us.upper() for us, eu in json.load(f).items() if eu}
    with open(p(".sec_cache", "_tickers.json")) as f:
        cik_of = {r["ticker"].upper(): "%010d" % r["cik_str"] for r in json.load(f).values()}
    out = collections.defaultdict(set)
    for k, v in lh.items():
        meta = ((((v[1] or {}).get("chart") or {}).get("result") or [{}])[0] or {}).get("meta") or {}
        if meta.get("instrumentType") != "EQUITY":
            continue
        us = us_of.get(k.upper(), k.upper())
        cik = cik_of.get(us) or cik_of.get(us.replace(".", "-"))
        if cik:
            out[cik].add(k)
    with open(p("tests", "dead-roster.csv")) as f:
        for r in csv.DictReader(l for l in f if not l.startswith("#")):
            if r["cik"].strip():
                out[r["cik"].strip().zfill(10)].add(r["ticker"])
    return {c: sorted(t) for c, t in out.items()}


def sims(ciks, years, load):
    """{(ticker, year): sim} for each pool filer with an Item 1A in `year` and `year - 1`."""
    rows, clash, prev = {}, set(), None
    for y in years:
        cur = load(y)
        for cik in sorted(cur.keys() & (prev or {}).keys()):
            for tk in ciks[cik]:
                if (tk, y) in rows:
                    clash.add((tk, y))
                rows[(tk, y)] = round(cosine(cur[cik], prev[cik]), 4)
        print("%d: %d filers with an Item 1A" % (y, len(cur)), file=sys.stderr)
        prev = cur
    return {k: v for k, v in rows.items() if k not in clash}


def write(rows, out):
    with open(out, "w") as f:
        f.write("# (#1316) cosine of each filer's 10-K Item 1A term counts against the year before, "
                "written by tools/item1a_sim.py from EDGAR-CORPUS (Loukas et al. 2021)\n")
        f.write("ticker,year,sim\n")
        for (tk, y), s in sorted(rows.items()):
            f.write("%s,%d,%s\n" % (tk, y, s))


def selftest():
    words = lambda n, w="risk": " ".join([w] * n)
    assert terms(words(199)) is None and terms(words(200)) == {"risk": 200}
    assert terms("Item 1A. " + words(200, "Risk")) == {"item": 1, "risk": 200}, "case folds, 1-letter tokens drop"
    a, b = {"x": 3, "y": 4}, {"x": 6, "y": 8}
    assert abs(cosine(a, b) - 1) < 1e-12 and cosine(a, {"z": 1}) == 0 and abs(cosine(a, {"x": 1}) - 0.6) < 1e-12
    line = lambda cik, n: json.dumps({"filename": "%s_2007.txt" % cik, "cik": cik, "year": "2007",
                                      "section_1": "x", "section_1A": words(n)}, separators=(",", ":")).encode()
    out = extract([line("320193", 300), line("999", 500), line("320193", 250), b"{\"broken", line("320193", 400)],
                  {"0000320193": ["AAPL"]}, {})
    assert out == {"0000320193": {"risk": 400}}, "pool only, the longest section wins, a broken line is skipped"
    tf = {2006: {"1": {"a": 1}, "2": {"a": 1}, "3": {"a": 1}}, 2007: {"1": {"a": 2}, "2": {"b": 1}, "3": {"a": 1}}}
    got = sims({"1": ["A", "AA.DE"], "2": ["B"], "3": ["A"]}, [2006, 2007], tf.get)
    assert got == {("AA.DE", 2007): 1.0, ("B", 2007): 0.0}, got
    print("selftest OK")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=".")
    ap.add_argument("--work", help="term-count dir (fresh, outside the repo)")
    ap.add_argument("--first", type=int, default=2006)
    ap.add_argument("--last", type=int, default=2018)
    ap.add_argument("--out", default="tests/item1a-sim.csv")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        selftest()
        sys.exit(0)
    ciks = pool(a.root)
    print("%d pool filers" % len(ciks), file=sys.stderr)
    os.makedirs(a.work, exist_ok=True)
    write(sims(ciks, range(a.first, a.last + 1), lambda y: year_terms(y, ciks, a.work)), a.out)
