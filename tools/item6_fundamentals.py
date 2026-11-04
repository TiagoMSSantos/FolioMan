#!/usr/bin/env python3
"""(#1323) L3b: as-filed annual fundamentals for exited S&P 500 members, read from 10-K Item 6.

The walk's dead lane (#751, #752, #1314) prices exited members from archive files, but their fund rows
stay empty: SEC's XBRL facts start in 2009 and its ticker map drops a name once it delists. An empty
row passes every fund gate (picks.rs: "no fundamentals at all -> passes"), so on the dead lane the
shipped fund terms never see a dead name. Every 10-K up to 2021 printed, in Item 6 "Selected Financial
Data" or the annual report it incorporates (EX-13), a five-year table of the figures those terms read.
This tool reads those tables and writes `tests/dead-fundamentals.csv`:

    ticker,cik,period_end,filed,revenue,op_income,net_income,eps,prior_eps,assets

One row per fiscal year, from the EARLIEST 10-K that prints it (filed = that filing's date), so the
walk's as-of read sees what a reader knew then, never a later restatement. EPS is diluted where the
table prints it, else basic/primary; it and prior_eps (the same filing's previous column) are moved
onto the newest filing's share basis, like the split-adjusted price files. Money is in dollars. An empty
cell is no value, never a zero. Fields: the ones the shipped fund terms read (roic, margin_trend, the PEG
ceiling and peg_yield): revenue, operating income, net income, EPS, total assets.

Targets: tickers of tests/dead-members.csv + tests/dead-prices.csv with a CIK (the roster's, else
tests/ticker-cik.csv's, else item5_prices' hand map). Filings before 2009 are read whole; later ones
(10-50 MB with their XBRL exhibits) by their 10-K and EX-13 documents only.

    python3 -I tools/item6_fundamentals.py --cache DIR [--read-cache DIR ...] --out tests/dead-fundamentals.csv
    python3 -I tools/item6_fundamentals.py --selftest

Stdlib only. EDGAR fetches reuse tools/dead_roster.py's polite getter (<= 8 req/s, cached)."""
import argparse
import csv
import datetime as dt
import hashlib
import math
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dead_roster as dr  # noqa: E402
import item5_prices as i5  # noqa: E402

FIELDS = ("revenue", "op_income", "net_income", "eps", "assets")
UNITS = {"thousand": 1e3, "million": 1e6, "billion": 1e9}
YEAR_RE = re.compile(r"(?<![\d,.$])((?:19[89]|20[0-3])\d)(?:\(\w\)|\*+)?(?![\d,.%])")
NUM_FULL = re.compile(r"^(\()?(-)?((?:\d{1,3}(?:,\d{3})+|\d+)(?:\.\d+)?|\.\d+)(\))?$")
DASH = re.compile(r"^(?:[—–-]+|n/?a|nm|\*+)$", re.I)
STEADY, SPLIT_TOL, AGREE_TOL = 0.75, 0.02, 0.05
LABELS = {
    "revenue": re.compile(r"^(?:total |net |consolidated )*(?:operating )?(?:revenues?|sales)"
                          r"(?: and (?:other )?(?:operating )?(?:revenues?|income))?$"),
    "op_income": re.compile(r"^(?:operating (?:income|profit|earnings)|(?:income|earnings|profit) from operations)$"),
    "net_income": re.compile(r"^net (?:income|earnings)(?: (?:attributable|applicable|available) (?:to|for) (?!non|minority)"
                             r"(?:common|the company|[a-z]+ (?:inc|corp|stockholders|shareholders)).*)?$"),
    "assets": re.compile(r"^total assets$"),
}
EPS_SUB = re.compile(r"^(?:basic|diluted|primary|fully[- ]diluted|assuming (?:full )?dilution|basic and diluted"
                     r"|net (?:income|earnings)|(?:income|earnings) (?:from|before) .*)$")


def num(tok):
    """A table cell: float, None for a dash/n.a., False when the token is not a cell."""
    if DASH.match(tok):
        return None
    m = NUM_FULL.match(tok)
    if not m:
        return False
    v = float(m.group(3).replace(",", ""))
    return -v if m.group(1) or m.group(2) or m.group(4) else v


def norm(label):
    """Lower-case label with footnote marks, '(loss)' and punctuation stripped."""
    s = label.lower().replace("’", "'")
    s = re.sub(r"\((?:\d{1,2}|[a-z])\)|\*+", " ", s)
    s = re.sub(r"\((?:loss|losses|deficit)\)|\bloss\b", " ", s)
    s = re.sub(r"[$:,.;]", " ", s)
    return re.sub(r"\s+", " ", s).strip()


def split_row(line, ncols):
    """(label, values) when `line` ends in exactly `ncols` cells (leading footnote ints dropped), else None."""
    s = re.sub(r"\$", " ", line)
    s = re.sub(r"\(\s+", "(", s)
    s = re.sub(r"\s+\)", ")", s)
    t = s.split()
    i = next((k for k, x in enumerate(t) if num(x) is not False), len(t))
    label, vals = " ".join(t[:i]), t[i:]
    if not vals or any(num(x) is False for x in vals):
        return None
    while len(vals) > ncols and re.match(r"^\(\d\)$", vals[0]):
        vals = vals[1:]
    return (label, [num(x) for x in vals]) if len(vals) == ncols else None


def year_header(line):
    """The years of a run of >= 3 consecutive fiscal years in `line`, else None."""
    ys = [int(m.group(1)) for m in YEAR_RE.finditer(line)]
    best = []
    for k in range(len(ys)):
        run = [ys[k]]
        for y in ys[k + 1:]:
            if abs(y - run[-1]) == 1 and (len(run) < 2 or y - run[-1] == run[-1] - run[-2]):
                run.append(y)
            else:
                break
        best = max(best, run, key=len)
    if len(best) < 3:
        return None
    rest = YEAR_RE.sub(" ", line)
    return best if not re.search(r"\d,\d{3}", rest) else None


def unit_of(text):
    t = text.lower()
    m = re.search(r"\bin (thousand|million|billion)s?\b|(thousand|million|billion)s? of dollars|dollars in (thousand|million|billion)s?", t)
    if m:
        return UNITS[next(g for g in m.groups() if g)]
    return 1e3 if re.search(r"000'?s omitted|\(000\)|\$000", t) else None


def tables(L, depth=80):
    """[(years, unit, {field: {year: value}})] of every >=3-year table in the lines `L`."""
    out = []
    for h, line in enumerate(L):
        years = year_header(line)
        if not years:
            continue
        unit = unit_of(" ".join(L[max(0, h - 8):h + 4]))  # the unit line sits above the years or just below
        got, eps_rank, ps, mode, pending = {}, {}, False, "basic", []
        for x in L[h + 1:h + 1 + depth]:
            if year_header(x):
                break
            r = split_row(x, len(years))
            if r is None:
                if x and not re.search(r"\d", x):
                    pending.append(x)
                    lx = norm(x)
                    if "per share" in lx or "per common share" in lx or "earnings per" in lx:
                        ps, mode = True, "diluted" if "dilut" in lx else "basic"
                    elif ps and re.match(r"^(?:fully[- ])?dilut|^assuming", lx):
                        mode = "diluted"
                    elif ps and re.match(r"^(?:basic|primary)", lx):
                        mode = "basic"
                continue
            label, vals = r
            own, whole = norm(label), norm(" ".join(pending[-3:] + [label]))
            pending = []
            field, rank = None, None
            per = "per share" in own or "per common share" in own
            if per and not re.search(r"dividend|book|cash|distribut|price", own):
                ps = True
                kind = "diluted" if "dilut" in own or "dilut" in whole else mode
                rank = (kind != "diluted") * 2 + bool(re.search(r"before|continuing|operations", own))
                field = "eps"
            elif ps and (not own or EPS_SUB.match(own) or not re.search(r"dividend|book|share|weighted|average|ratio", own)
                         and all(v is None or abs(v) < 100 for v in vals)):
                # inside a per-share block a small-valued row is a per-share row, however its label wraps
                kind = "diluted" if "dilut" in own else mode
                rank = (kind != "diluted") * 2 + bool(re.search(r"before|continuing|operations", own if EPS_SUB.match(own) and own.startswith(("net", "basic", "diluted", "primary", "fully")) else whole))
                field = "eps"
            else:
                ps = False
                field = next((f for f, rx in LABELS.items() if rx.match(own)), None)
            if field is None or (field != "eps" and field in got):
                continue
            if field == "eps":
                if any(v is not None and abs(v) >= 1000 for v in vals) or rank >= eps_rank.get("eps", 9):
                    continue
                eps_rank["eps"] = rank
            got[field] = dict(zip(years, vals))
        if got:
            out.append((years, unit, got))
    return out


def per_filing(docs):
    """{year: {field: value}} of one 10-K from its documents' line lists: each field from the widest
    table that prints it, money converted to dollars where the table states its unit."""
    ts = [t for L in docs for t in tables(L)]
    ts.sort(key=lambda t: -len(t[0]))
    base = next((u for _, u, g in ts if "revenue" in g and u), None) or next((u for _, u, _ in ts if u), None)
    out = {}
    for f in FIELDS:
        for _, unit, got in ts:
            for y, v in got.get(f, {}).items():
                if v is None or y in out and f in out[y]:
                    continue
                if f != "eps":
                    v *= unit or base or 1.0
                out.setdefault(y, {})[f] = v
    for y, r in list(out.items()):  # filter, never fabricate: a row whose ratios no filer prints is a misread
        rev, op, ni, a = (r.get(k) for k in ("revenue", "op_income", "net_income", "assets"))
        if rev is not None and rev <= 0 or a is not None and a <= 0 \
                or rev and op is not None and abs(op / rev) > 1.5 or rev and ni is not None and abs(ni / rev) > 2 \
                or a and ni is not None and abs(ni / a) > 1:
            del out[y]
    return out


def years_back(d, k):
    try:
        return d.replace(year=d.year - k)
    except ValueError:  # Feb 29
        return d.replace(year=d.year - k, day=28)


def median(xs):
    xs = sorted(xs)
    return xs[len(xs) // 2]


def chain(filings):
    """{year: row} from [(filed, reportDate, per_filing dict)] sorted newest first. A row comes from the
    EARLIEST filing that prints its year; money is snapped onto the newest filing's scale (the revenue
    overlap's median ratio, rounded to a power of 1000) and EPS onto its share basis (one steady ratio,
    3 in 4 within 5% of the median, is a split; within 2% of 1 is none). A filing whose EPS overlap holds
    no steady ratio keeps its money and loses its EPS; no revenue overlap cuts the series. Returns (rows,
    note)."""
    rows, rev_at, eps_at, eps_k, skipped, why = {}, {}, {}, 1.0, 0, ""
    for filed, report, f in filings:
        if not f or not report:
            continue
        y0, rep = max(f), dt.date.fromisoformat(report)
        if abs(rep.year - y0) > 1:  # the table's newest year is not this filing's fiscal year
            continue
        money = 1.0
        if rev_at:
            both = [y for y in f if y in rev_at and f[y].get("revenue")]
            if not both:
                why = "no revenue overlap before %d" % min(rev_at)
                break
            money = 1000.0 ** round(math.log(median(rev_at[y] / f[y]["revenue"] for y in both), 1000))
        r = [eps_at[y] / f[y]["eps"] for y in f if y in eps_at and f[y].get("eps") is not None
             and min(abs(eps_at[y]), abs(f[y]["eps"])) >= 0.05 and (eps_at[y] > 0) == (f[y]["eps"] > 0)]
        eps_ok = True
        if r:
            k = median(r)
            if sum(abs(x / k - 1) <= AGREE_TOL for x in r) >= STEADY * len(r):
                eps_k = 1.0 if abs(k - 1) <= SPLIT_TOL else k
            else:
                eps_ok, skipped = False, skipped + 1
        for y, row in f.items():
            v = {k: (row[k] * money if row.get(k) is not None else None) for k in FIELDS if k != "eps"}
            e, p = row.get("eps"), f.get(y - 1, {}).get("eps")
            v["eps"] = e * eps_k if eps_ok and e is not None else None
            v["prior_eps"] = p * eps_k if eps_ok and p is not None else None
            v["period_end"], v["filed"] = years_back(rep, y0 - y).isoformat(), filed
            rows[y] = v
            if v["revenue"]:
                rev_at[y] = v["revenue"]
            if v["eps"] is not None:
                eps_at[y] = v["eps"]
    return rows, "; ".join(x for x in (why, "%d filings' EPS off-basis, dropped" % skipped if skipped else "") if x)


def get(url, caches, out, binary=True):
    """dr._get through the first cache that holds `url`, else fetched into `out`."""
    for c in caches:
        if os.path.exists(os.path.join(c, hashlib.sha1(url.encode()).hexdigest() + (".bin" if binary else ".json"))):
            return dr._get(url, c, binary)
    return dr._get(url, out, binary)


def filings_of(cik, until, caches, out):
    s = get(dr.SUBMISSIONS.format("CIK%s.json" % cik), caches, out, binary=False)
    if not s:
        return []
    pages = [s["filings"]["recent"]] + [get(dr.SUBMISSIONS.format(f["name"]), caches, out, binary=False) for f in s["filings"].get("files", [])]
    return sorted(((p["filingDate"][i], p["reportDate"][i], p["accessionNumber"][i]) for p in pages if p
                   for i, form in enumerate(p["form"]) if form in i5.FORMS and p["filingDate"][i] <= until), reverse=True)


def index_docs(page):
    """Hrefs of the 10-K and EX-13 documents listed on an EDGAR filing index page."""
    out = []
    for row in re.findall(r"(?is)<tr\b.*?</tr>", page):
        cells = [re.sub(r"<[^>]+>", "", c).strip() for c in re.findall(r"(?is)<td\b[^>]*>(.*?)</td>", row)]
        href = re.search(r'(?i)href="(?:/ix\?doc=)?(/Archives/[^"]+\.(?:htm|html|txt))"', row)
        if href and len(cells) >= 4 and cells[3].upper().startswith(("10-K", "EX-13")):
            out.append(href.group(1))
    return out


def docs_of(cik, acc, filed, caches, out):
    """Line lists of a filing's 10-K and EX-13 documents."""
    full = "https://www.sec.gov/Archives/edgar/data/%d/%s.txt" % (int(cik), acc)
    held = any(os.path.exists(os.path.join(c, hashlib.sha1(full.encode()).hexdigest() + ".bin")) for c in caches)
    if held or filed < "2009":
        raw = get(full, caches, out)
        return [i5.lines(b) for _, b in i5.documents(raw.decode("latin-1"))] if raw else []
    base = "https://www.sec.gov/Archives/edgar/data/%d/%s/" % (int(cik), acc.replace("-", ""))
    page = get(base + acc + "-index.htm", caches, out)
    if not page:
        return []
    docs = [get("https://www.sec.gov" + h, caches, out) for h in index_docs(page.decode("latin-1"))]
    return [i5.lines(d.decode("latin-1")) for d in docs if d]


def targets(root):
    """{ticker: (cik, last month)} of the dead lane's series tickers that have a CIK."""
    last = {}
    for p in ("tests/dead-members.csv", "tests/dead-prices.csv"):
        with open(os.path.join(root, p)) as f:
            for l in f:
                if not l.startswith("#") and not l.startswith("ticker,"):
                    tk, month = l.split(",", 2)[:2]
                    last[tk] = max(last.get(tk, ""), month)
    rows = {r["ticker"]: r for r in csv.DictReader(x for x in open(os.path.join(root, "tests/dead-roster.csv")) if not x.startswith("#"))}
    mapped = i5.cik_map(root)
    out = {}
    for tk in sorted(last):
        cik = (rows.get(tk, {}).get("cik") or mapped.get(tk) or i5.Q.get(tk) or "").strip()
        if cik:
            out[tk] = (cik, last[tk])
    return out


def fmt(x):
    return "" if x is None else ("%.4f" % x).rstrip("0").rstrip(".")


def build(args):
    out, report = [], []
    for tk, (cik, last) in targets(args.root).items():
        # a filing after the series' last bar is never read by the walk, and the newest one read sets the
        # EPS basis, which has to be the one the price file's last bar is on
        until = min(args.until, "%d%s-31" % (int(last[:4]) + 1, last[4:7]))
        per = [(filed, rep, per_filing(docs_of(cik, acc, filed, args.read_cache, args.cache)))
               for filed, rep, acc in filings_of(cik, until, args.read_cache, args.cache)]
        rows, why = chain(per)
        rows = {y: r for y, r in rows.items() if any(r[k] is not None for k in FIELDS)}
        if not rows:
            report.append((tk, "no Item 6 table read in %d filings" % len(per)))
            continue
        out += [(tk, cik, r) for _, r in sorted(rows.items())]
        n = {k: sum(r[k] is not None for r in rows.values()) for k in FIELDS}
        report.append((tk, "%d-%d %d years from %d filings (%s)%s" % (min(rows), max(rows), len(rows), len(per),
                                                                   " ".join("%s %d" % kv for kv in n.items()), "; " + why if why else "")))
    with open(args.out, "w") as f:
        f.write("# (#1323) as-filed annual fundamentals of exited S&P 500 members, read from the Item 6 / annual-report\n")
        f.write("# Selected Financial Data tables of their 10-K filings on SEC EDGAR (tools/item6_fundamentals.py): one\n")
        f.write("# row per fiscal year from the earliest 10-K that prints it; money in dollars, EPS diluted where printed and\n")
        f.write("# moved onto the newest filing's share basis; blank = not printed\n")
        f.write("ticker,cik,period_end,filed,revenue,op_income,net_income,eps,prior_eps,assets\n")
        for tk, cik, r in out:
            f.write(",".join([tk, cik, r["period_end"], r["filed"]] + [fmt(r[k]) for k in ("revenue", "op_income", "net_income", "eps", "prior_eps", "assets")]) + "\n")
    for tk, msg in report:
        print("%-6s %s" % (tk, msg))


def selftest():
    assert num("(346,196)") == -346196 and num("-5.2") == -5.2 and num("—") is None and num("income(1)") is False
    assert year_header("Operating Results: 1994 1993 1992(1) 1991 1990") == [1994, 1993, 1992, 1991, 1990]
    assert year_header("Total assets 1,639,634 1,636,931 1,460,999") is None and year_header("2001 2000") is None
    assert year_header("Fiscal year 1995 1994 1993*") == [1995, 1994, 1993]
    assert split_row("Net income (1) 1 2 3", 3) == ("Net income", [1, 2, 3]) and split_row("Net income 1 2", 3) is None
    assert norm("Net (loss) income(1)") == "net income" and norm("Income (loss) from operations") == "income from operations"
    # Molex 2009: a negative printed "(346,196 )", a footnote stuck to its label, dashes
    molex = per_filing([["(In thousands,", "except per share data)", "2009 2008 2007 2006 2005",
                         "Net revenue $ 2,581,841 $ 3,328,347 $ 3,265,874 $ 2,861,289 $ 2,554,458",
                         "Income (loss) from operations (346,196 ) 327,268 328,236 310,536 204,572",
                         "Net (loss) income(1) (321,287 ) 215,437 240,768 236,091 150,116",
                         "Earnings (loss) per share:", "Basic $ (1.84 ) $ 1.19 $ 1.31 $ 1.27 $ 0.80",
                         "Diluted (1.84 ) 1.19 1.30 1.26 0.79", "Total assets 2,942,157 3,599,537 3,316,108 2,974,420 2,730,162",
                         "Goodwill impairments 264,140 — — — 22,876"]])
    assert molex[2009] == {"revenue": 2581841e3, "op_income": -346196e3, "net_income": -321287e3, "eps": -1.84, "assets": 2942157e3}, molex[2009]
    assert molex[2007]["eps"] == 1.30 and len(molex) == 5
    assert LABELS["net_income"].match(norm("Net (loss) income attributable to MEMC stockholders"))
    assert not LABELS["net_income"].match(norm("Net income attributable to noncontrolling interests"))
    assert unit_of("2000 1999 1998 (in millions, except per share amounts)") == 1e6
    # 1994: multi-line labels, Primary then Fully-diluted blocks; the net fully-diluted row wins
    old = per_filing([["Item 6. Selected Financial Data", "(in thousands, except per share data)", "Year Ended December 31,",
                       "Operating Results: 1994 1993 1992(1) 1991 1990", "Net Revenue $465,306 $359,980 $344,940 $319,376 $231,496",
                       "Earnings (Loss)", "Before Cumulative", "Effect of Accounting", "Change $34,984 $12,445 ($15,506) $22,768 $7,576",
                       "Net Earnings (Loss) $34,984 $12,445 ($16,250) $22,768 $7,576", "Earnings (Loss) Per Share:", "Primary",
                       "Earnings (Loss)", "Before Cumulative Effect", "of Accounting Change $1.34 $0.50 ($0.65) $0.91 $0.33",
                       "Net Earnings (Loss) $1.34 $0.50 ($0.68) $0.91 $0.33", "Fully-diluted", "Earnings (Loss)",
                       "Before Cumulative Effect", "of Accounting Change $1.33 $0.50 ($0.65) $0.90 $0.33",
                       "Net Earnings (Loss) $1.33 $0.50 ($0.68) $0.90 $0.33", "Financial Position:",
                       "Total Assets $474,213 $419,615 $378,666 $354,357 $315,131"]])
    assert old[1992] == {"revenue": 344940e3, "net_income": -16250e3, "eps": -0.68, "assets": 378666e3}, old[1992]
    assert old[1994]["eps"] == 1.33
    assert per_filing([["Item 6. Selected Financial Data 46", "Item 7. Management's Discussion 47"]]) == {}  # a TOC line only
    # a grocer in millions, a starred year, a dash for an unprinted figure; two tables (income, balance sheet)
    groc = per_filing([["(Dollars in millions, except per share)", "Fiscal year 1995 1994 1993*",
                        "Sales $ 12,345.6 $ 11,000.0 $ 10,500.2", "Operating profit 600.1 550.0 —", "Net earnings 300.0 250.5 200.0",
                        "Net earnings per share 1.50 1.25 1.00", "Dividends per share .50 .40 .30"],
                       ["(In thousands)", "December 31, 1995 1994 1993", "Total assets 5,000,000 4,800,000 4,600,000"]])
    assert groc[1993] == {"revenue": 10500.2e6, "net_income": 200e6, "eps": 1.0, "assets": 4.6e9}, groc[1993]
    assert groc[1995]["op_income"] == 600.1e6
    html_rows = i5.lines("<table><tr><td>Fiscal</td><td>2003</td><td>2002</td><td>2001</td></tr><tr><td>Net sales</td><td>$</td>"
                         "<td>1,000</td><td>$</td><td>900</td><td>$</td><td>800</td></tr><tr><td>Net income (loss)</td><td>(50</td>"
                         "<td>)</td><td>40</td><td>30</td></tr></table>")
    assert per_filing([html_rows])[2003] == {"revenue": 1000.0, "net_income": -50.0}
    assert per_filing([["2003 2002 2001", "Net sales 100 90 80", "Net income 500 1 1"]]).keys() == {2002, 2001}  # ni/rev > 2 dropped
    # chain: the newest filing in dollars; the older one unscaled and pre-2:1 split; earliest filing wins a year
    f2 = {2000: {"revenue": 2e9, "eps": 1.0}, 1999: {"revenue": 1.8e9, "eps": 0.9}, 1998: {"revenue": 1.6e9, "eps": 0.8}}
    f1 = {1999: {"revenue": 1.8e6, "eps": 1.8}, 1998: {"revenue": 1.6e6, "eps": 1.6}, 1997: {"revenue": 1.5e6, "eps": 1.5}}
    rows, why = chain([("2000-09-01", "2000-06-30", f2), ("1999-09-01", "1999-06-30", f1)])
    assert why == "" and rows[1999] == {"revenue": 1.8e9, "op_income": None, "net_income": None, "assets": None, "eps": 0.9,
                                        "prior_eps": 0.8, "period_end": "1999-06-30", "filed": "1999-09-01"}, rows[1999]
    assert rows[2000]["filed"] == "2000-09-01" and rows[1997]["eps"] == 0.75 and rows[1997]["prior_eps"] is None
    assert rows[1997]["period_end"] == "1997-06-30" and rows[1997]["revenue"] == 1.5e9
    f0 = {1998: {"revenue": 1.6e9, "eps": 3.0}, 1997: {"revenue": 1.5e9, "eps": 0.2}, 1996: {"revenue": 1.4e9, "eps": 0.7}}
    rows, why = chain([("2000-09-01", "2000-06-30", f2), ("1999-09-01", "1999-06-30", f1), ("1998-09-01", "1998-06-30", f0)])
    assert rows[1996]["eps"] is None and rows[1996]["revenue"] == 1.4e9 and "1 filings' EPS off-basis" in why  # unsteady: EPS dropped
    rows, why = chain([("2000-09-01", "2000-06-30", f2), ("1995-09-01", "1995-06-30", {1995: {"revenue": 1e9}, 1994: {"revenue": 9e8}, 1993: {"revenue": 8e8}})])
    assert set(rows) == {2000, 1999, 1998} and why.startswith("no revenue overlap")
    assert chain([("2000-09-01", "2004-06-30", f2)])[0] == {}  # header years not the filing's own
    page = ('<table><tr><td>1</td><td>Annual report</td><td><a href="/ix?doc=/Archives/edgar/data/1/000/a10k.htm">a10k.htm</a></td>'
            '<td>10-K</td><td>1</td></tr><tr><td>2</td><td>Exhibit</td><td><a href="/Archives/edgar/data/1/000/ex13.htm">ex13.htm</a></td>'
            '<td>EX-13</td><td>1</td></tr><tr><td>3</td><td>XBRL</td><td><a href="/Archives/edgar/data/1/000/x.xml">x.xml</a></td><td>EX-101.INS</td></tr></table>')
    assert index_docs(page) == ["/Archives/edgar/data/1/000/a10k.htm", "/Archives/edgar/data/1/000/ex13.htm"]
    print("selftest OK")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--root", default=".")
    p.add_argument("--cache", help="EDGAR download cache (new fetches land here)")
    p.add_argument("--read-cache", nargs="*", default=[], help="older caches read first (dead_roster, item5_prices)")
    p.add_argument("--until", default="2019-12-31", help="last filing date read (the 8y walk's last cutoff is ~2018)")
    p.add_argument("--out", default="tests/dead-fundamentals.csv")
    p.add_argument("--selftest", action="store_true")
    a = p.parse_args()
    if a.selftest:
        return selftest()
    if not a.cache:
        p.error("--cache is required")
    build(a)


if __name__ == "__main__":
    main()
