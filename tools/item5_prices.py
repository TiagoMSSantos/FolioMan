#!/usr/bin/env python3
"""(#1314) DN2: monthly prices for dead S&P 500 members that no free price archive holds.

Every 10-K prints, in Item 5 or the annual report it incorporates (EX-13), the high and low sale price
of each quarter of its last two fiscal years. This tool reads those tables from every 10-K of the
target names, chains the filings into one series (consecutive filings overlap by a year, so a split
shows as one steady ratio), and writes `tests/dead-prices.csv` in the dead-members format:
`ticker,month,close,div`, where close = the quarter's (high+low)/2 at the quarter-end month, carried
forward monthly. The walk reads it after `tests/dead-members.csv`.

Targets: tests/dead-roster.csv rows that left the index before 2014 with no WIKI/Tiingo series, a
known CIK, and no Yahoo bar that the walk could already score (a Yahoo series that scores keeps the
name, and the dead fill must never displace it).

Bankrupt names (Florida-UCLA-LoPucki Bankruptcy Research Database, `--brd`) whose filing date falls
within a year of their index exit, and whose plan left no positive equity distribution, get a final
bar at 1% of the last price in the filing month.

    python3 -I tools/item5_prices.py --cache DIR --brd cases.csv --out tests/dead-prices.csv
    python3 -I tools/item5_prices.py --selftest

Stdlib only. EDGAR fetches reuse tools/dead_roster.py's polite getter (<= 8 req/s, cached)."""
import argparse
import functools
import csv
import datetime as dt
import html
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dead_roster as dr  # noqa: E402

# Q-suffixed tickers matched to their BRD case by hand: the roster has no CIK for them
Q = {"FLMIQ": "0000352949", "FLTWQ": "0000314132", "GAPTQ": "0000043300", "OWENQ": "0000075234", "BHMSQ": "0000011860",
     "ENRNQ": "0001024401", "UAWGQ": "0000701345", "WCOEQ": "0000723527", "NRTLQ": "0000072911", "AAMRQ": "0000006201",
     "WNDXQ": "0000107681", "DALRQ": "0000027904", "DPHIQ": "0001072342", "CPNLQ": "0000916457", "VSTNQ": "0001111335",
     "DCNAQ": "0000026780", "CCTYQ": "0000104599", "ABKFQ": "0000874501", "LEHMQ": "0000806085", "WAMUQ": "0000933136",
     "MTLQQ": "0000040730", "CITGQ": "0001171825", "EKDKQ": "0000031235", "RSHCQ": "0000096289", "SUNEQ": "0000945436",
     "ANRZQ": "0001301063"}
# Tribune's 2007 buyout paid holders $34 cash a year before its 2008 bankruptcy: no 1% bar
NO_TERMINAL = {"TRB"}
FORMS = ("10-K", "10-K405", "10-KT", "10-KSB", "10-K/A", "10-K405/A")
SPLIT_TOL = 0.02
# filings restate each other's overlap with a typo or an intraday-vs-closing basis now and then
AGREE_TOL = 0.05

MONTHS = {m: i + 1 for i, m in enumerate(["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"])}
QWORD = {"first": 1, "second": 2, "third": 3, "fourth": 4, "1st": 1, "2nd": 2, "3rd": 3, "4th": 4,
         "q1": 1, "q2": 2, "q3": 3, "q4": 4, "one": 1, "two": 2, "three": 3, "four": 4}
MONTH_RE = r"(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?"
FRAC = {"\xbc": 0.25, "\xbd": 0.5, "\xbe": 0.75}
NUM_RE = re.compile(r"(?<![\w/.])(\d+\.\d+|\.\d+|\d+/\d+|\d+(?:[ -]+\d+/\d+|[\xbc\xbd\xbe])?)(?![\w/])")


def num(tok):
    """'70 1/4' / '70-1/4' / '7/8' / '70\xbd' / '.10' -> float; None for a fraction with an odd denominator."""
    tok = tok.strip()
    if tok[-1] in FRAC:
        return int(tok[:-1]) + FRAC[tok[-1]]
    m = re.fullmatch(r"(?:(\d+)[ -]+)?(\d+)/(\d+)", tok)
    if m:
        whole, a, b = int(m.group(1) or 0), int(m.group(2)), int(m.group(3))
        return whole + a / b if b in (2, 4, 8, 16, 32, 64) and a < b else None
    return float(tok)


def _blank(m):
    return " " * len(m.group(0))


@functools.lru_cache(maxsize=None)
def cleaned(text):
    """`text` with dates, years, ordinals, footnote marks and $ blanked (positions kept), cents as dollars."""
    t = re.sub(r"(?i)\b" + MONTH_RE + r"\s+\d{1,2}\b(,?\s*(19|20)\d\d)?", _blank, text)
    t = re.sub(r"(?i)\$\s*\d[\d\s/.-]*par\s+value", _blank, t)  # GM's "common stock $1 2/3 par value"
    t = re.sub(r"\b(19|20)\d\d\b", _blank, t)
    t = re.sub(r"(\d+)\s+/\s+(\d+)", lambda m: (m.group(1) + "/" + m.group(2)).ljust(len(m.group(0))), t)  # "11 / 16"
    t = re.sub(r"(?i)\b\d+\s?(st|nd|rd|th)\b", _blank, t)
    t = re.sub(r"\(\s*\w{1,2}\s*\)", _blank, t)
    t = re.sub(r"(?i)(\d*\.?\d+)\s*(cents?\b|\xa2)", lambda m: "%g" % (float(m.group(1)) / 100), t)
    return t.replace("$", " ")


@functools.lru_cache(maxsize=None)
def numbers(text):
    """The numbers of a table row once dates, years, ordinals and footnote marks are gone."""
    out = []
    for tok in NUM_RE.findall(cleaned(text).replace(",", "")):
        v = num(tok)
        if v is None:
            return []
        out.append(v)
    return out


def label_of(text):
    """The row label: the text before the first number."""
    m = NUM_RE.search(cleaned(text).replace(",", " "))
    return text[:m.start()] if m else text


def documents(raw):
    """(type, body) of each document of a full EDGAR submission; the 10-K itself and its EX-13 only."""
    for m in re.finditer(r"(?is)<DOCUMENT>(.*?)</DOCUMENT>", raw):
        ty = re.search(r"(?i)<TYPE>\s*([^\s<]+)", m.group(1))
        ty = ty.group(1).upper() if ty else ""
        if ty.startswith("10-K") or ty.startswith("EX-13"):
            yield ty, m.group(1)


def lines(body):
    """Text lines; an HTML table row becomes one line of its cells."""
    if not re.search(r"(?i)<(td|tr)[\s>]", body):
        body = re.sub(r"(?i)</?(s|c|caption|table|page|fn)>", " ", body)
        return [re.sub(r"[ \t\xa0]+", " ", x).strip() for x in html.unescape(body).split("\n")]
    body = re.sub(r"(?is)<(script|style)\b.*?</\1>", " ", body)

    def row(m):
        return "\n" + re.sub(r"\s+", " ", re.sub(r"<[^>]+>", " ", m.group(0))) + "\n"
    body = re.sub(r"(?is)<tr\b.*?(?:</tr>|(?=<tr\b)|(?=</table>))", row, body)
    body = re.sub(r"(?i)<br\s*/?>|</(p|div|h\d|table)>", "\n", body)
    body = html.unescape(re.sub(r"<[^>]+>", " ", body))
    return [re.sub(r"[ \t\xa0]+", " ", x).strip() for x in body.split("\n")]


@functools.lru_cache(maxsize=None)
def layout(text):
    """H/L/D/C column roles of a header row, in order ('High Low Dividends' -> 'HLD')."""
    roles = {"high": "H", "low": "L", "close": "C", "last": "C", "closing": "C"}
    out = ""
    for w in re.findall(r"(?i)[a-z]+", text):
        w = w.lower()
        if w in roles:
            out += roles[w]
        elif w.startswith("dividend") or w == "div":
            out += "D"
    return re.sub(r"D+", "D", out)


ORD = r"\b(first|second|third|fourth|q[1-4]|1\s?st|2\s?nd|3\s?rd|4\s?th)\b"


def quarter_of(label):
    """1-4 for a quarter word, ('m', month) for a month label, ('p', 1-4) for part of a quarter
    ("Third Quarter, through February 22, 2005"), None otherwise."""
    lab = label.lower()
    if re.search(r"\b(to|through)\s+" + MONTH_RE, lab):
        m = re.search(ORD, lab)
        return ("p", QWORD[m.group(1).replace(" ", "")]) if m else None
    m = re.search(ORD, lab)
    if m:
        w = m.group(1).replace(" ", "")
        if "quarter" in lab or "qtr" in lab or w[0] in "q1234" or re.fullmatch(r"\W*" + ORD + r"\W*", lab):
            return QWORD[w]
    m = re.findall(r"\b" + MONTH_RE, lab)
    if m and not re.search(r"\b(to|through)\b", lab) and re.match(r"\s*(quarter\s+end(ed|ing)\s+|three\s+months\s+ended\s+)?" + MONTH_RE, lab):
        return ("m", MONTHS[m[-1][:3]])
    return None


def years_in(text):
    return [int(y) for y in re.findall(r"\b((?:19|20)\d\d)\b", text)]


def parse_lines(L, depth=14):
    """[(year label, quarter or ('m', month), high, low, div)] from every price table in `L`.

    A quarter row reads its numbers against the nearest header above holding High and Low (one H/L
    group per year column, years from the row's own label or the nearest label above holding years);
    dividends may sit in each group, after all price columns, or be left blank. A transposed table has
    High and Low rows under a header of quarter names."""
    out, L = [], [x for x in L if x]
    for i, line in enumerate(L):
        if len(line) > 300 or not re.search(r"\d", line):
            continue
        label = label_of(line)
        q, lab_lay = quarter_of(label), layout(label)
        if q is None and lab_lay != "H":
            continue
        nums = numbers(line)
        if len(nums) < 2:
            continue
        above = L[max(0, i - depth):i]
        part = isinstance(q, tuple) and q[0] == "p"
        own = [] if part else years_in(label)  # a part's own date is calendar, its section label the fiscal year
        ys = own[:1] or years_in(next((label_of(x) for x in reversed(above) if years_in(label_of(x))), ""))
        if q is not None and not lab_lay:
            head = next((x for x in reversed(above) if "H" in layout(x) and "L" in layout(x) and not numbers(x)), None)
            if head is None:
                continue
            lay = layout(head)
            groups = lay.count("H")
            if not ys and groups > 1:
                # a quarterly-results note: one year header far above, as many years as price groups
                ys = next((years_in(x) for x in reversed(L[max(0, i - 60):i])
                           if len(years_in(x)) == groups and re.fullmatch(r"\W*(quarter\W+)?((19|20)\d\d\W*)+", x, re.I)), [])
            if not ys:
                continue
            if groups == 1 and len(ys) > 1 and len(nums) == len(lay) * len(ys):
                lay, groups = lay * len(ys), len(ys)
            if groups > 1:
                if len(ys) < groups:
                    continue
                ys = ys[:groups]
            core = lay.replace("D", "")
            if len(nums) == len(lay) and len(lay) % groups == 0:
                per = len(lay) // groups
                cols = [dict(zip(lay[g * per:(g + 1) * per], nums[g * per:(g + 1) * per])) for g in range(groups)]
            elif len(nums) in (len(core), len(core) + groups) and len(core) % groups == 0:
                # dividends left blank ('--'), or printed after all the price columns
                per = len(core) // groups
                cols = [dict(zip(core[g * per:(g + 1) * per], nums[g * per:(g + 1) * per])) for g in range(groups)]
                for g in range(groups if len(nums) > len(core) else 0):
                    cols[g]["D"] = nums[len(core) + g]
            else:
                continue
            for g, pa in enumerate(cols):
                y = ys[g] if groups > 1 else ys[-1] if own else ys[0]
                if "H" in pa and "L" in pa:
                    out.append((y, q, pa["H"], pa["L"], pa.get("D", 0.0)))
        elif lab_lay == "H" and len(nums) in (4, 5, 8):
            # transposed: a High row under quarter-name columns, its Low row next to it
            # (quarters printed in either order; the header may sit 80 lines up, the year label up to 3 lines above it;
            # a fifth number is the fiscal-year column, kept only if it is the quarters' high)
            low = next((x for x in L[i + 1:i + 4] if layout(label_of(x)) == "L" and len(numbers(x)) == len(nums)), None)
            if len(nums) == 5:
                if nums[4] != max(nums[:4]):
                    continue
                nums = nums[:4]
            word = r"(?i)\b(first|second|third|fourth|1st|2nd|3rd|4th)\b"
            hi = next((j for j in range(i - 1, max(-1, i - 81), -1) if len(re.findall(word, L[j])) >= 4), None)
            if low is None or hi is None:
                continue
            order = [QWORD[w.lower()] for w in re.findall(word, L[hi])][:4]
            ys = years_in(next((label_of(x) for x in reversed(L[max(0, hi - 3):i]) if years_in(label_of(x))), ""))
            if len(set(order)) < 4 or len(ys) < len(nums) // 4:
                continue
            for k, (h, lo) in enumerate(zip(nums, numbers(low))):
                out.append((ys[k // 4], order[k % 4], h, lo, 0.0))
    out = [r for r in out if r[2] >= r[3] > 0]
    full = [r for r in out if not (isinstance(r[1], tuple) and r[1][0] == "p")]
    newest = max((r[0] for r in full), default=0)
    # part quarters: the later part wins (nearest the quarter end), a full row wins over both, and a part
    # past the newest full year is a stub after the fiscal year end (Symbol's 2005 10-K)
    parts = {(r[0], r[1][1]): r for r in out if r not in full and r[0] <= newest}
    return full + [(y, q, h, lo, d) for (y, q), (_, _, h, lo, d) in parts.items()]


def add_months(y, m, k):
    t = y * 12 + m - 1 + k
    return t // 12, t % 12 + 1


def to_months(rows, report):
    """Map parsed rows onto calendar quarter-end months. The table's latest year label is the fiscal
    year that ends at `report` (YYYY-MM-DD); a quarter number counts back from that year's end. A
    table whose newest year is not the report's fiscal year read only an older column: dropped."""
    if not rows:
        return {}
    ry, rm = int(report[:4]), int(report[5:7])
    if int(report[8:10]) <= 7:  # a 52/53-week year ending in the first days of a month closes the month before
        ry, rm = add_months(ry, rm, -1)
    off = ry - max(r[0] for r in rows)
    if off not in ((0, 1) if rm <= 6 else (0,)):
        return {}  # only a year ending in its first half may carry its start year; else the newest year was missed
    out = {}
    for y, q, h, lo, d in rows:
        fy = y + off
        if isinstance(q, tuple):
            m = q[1]
            key = (fy if m <= rm else fy - 1, m)
        else:
            key = add_months(fy, rm, -3 * (4 - q))
        out.setdefault(key, (h, lo, d))
    return out


def chain(filings):
    """One quarter-end series from per-filing dicts sorted newest first, on the newest basis. A later
    filing restates its predecessor's overlap: one steady ratio (3 in 4 values within 5% of the median)
    is a split and rescales the older values (a median within 2% of 1 is no split). A filing whose
    overlap holds no steady ratio read some other table and is skipped; no overlap cuts the series
    there. Returns (series, note on cuts and skips)."""
    s, why, skipped = {}, "", 0
    for k, f in enumerate(filings):
        if not f:
            continue
        if not s:
            s = dict(f)
            continue
        both = sorted(set(s) & set(f))
        if not both:
            why = "no overlap before %d-%02d" % min(s)
            break
        r = [s[x][j] / f[x][j] for x in both for j in (0, 1)]
        ratio = sorted(r)[len(r) // 2]
        if sum(abs(x / ratio - 1) <= AGREE_TOL for x in r) < 0.75 * len(r):
            skipped += 1
            continue
        if abs(ratio - 1) <= SPLIT_TOL:
            ratio = 1.0
        for x, (h, lo, d) in f.items():
            if x not in s:
                s[x] = (h * ratio, lo * ratio, d * ratio)
    return s, "; ".join(x for x in (why, "%d disagreeing filings skipped" % skipped if skipped else "") if x)


def monthly(series, pad_to=None):
    """(YYYY-MM, mid, div) per month from the first quarter end to the last (or `pad_to`), carried forward."""
    if not series:
        return []
    keys = sorted(series)
    out, (y, m) = [], keys[0]
    last = max(keys[-1], pad_to or keys[-1])
    cur = None
    while (y, m) <= last:
        div = 0.0
        if (y, m) in series:
            h, lo, div = series[(y, m)]
            cur = (h + lo) / 2
        out.append(("%d-%02d" % (y, m), cur, div))
        y, m = add_months(y, m, 1)
    return out


def targets(root):
    """{ticker: cik} of roster names that need this source."""
    rows = list(csv.DictReader(x for x in open(os.path.join(root, "tests/dead-roster.csv")) if not x.startswith("#")))
    out = {}
    for r in rows:
        if r["exit"] != "alive" and r["exit_date"] < "2014" and r["series"] == "none":
            cik = r["cik"] or Q.get(r["ticker"])
            if cik:
                out[r["ticker"]] = cik
    return out


def yahoo_scores(chart, spans, min_history=252):
    """Would the walk score this Yahoo series on its own? (mirrors wants_dead_fill: a bar past the
    warm-up inside a membership span)"""
    try:
        ts = chart["chart"]["result"][0]["timestamp"] or []
    except (KeyError, IndexError, TypeError):
        return False
    days = [dt.datetime.fromtimestamp(t, dt.timezone.utc).date().isoformat() for t in ts[min_history:]]
    return any(a <= d and (b is None or d < b) for d in days for a, b in spans)


def filings_of(cik, last_exit, cache):
    s = dr._get(dr.SUBMISSIONS.format("CIK%s.json" % cik), cache)
    pages = [s["filings"]["recent"]] + [dr._get(dr.SUBMISSIONS.format(f["name"]), cache) for f in s["filings"].get("files", [])]
    limit = str(dt.date.fromisoformat(last_exit) + dt.timedelta(days=480))
    out = []
    for p in pages:
        for i, form in enumerate(p["form"]):
            if form in FORMS and p["filingDate"][i] <= limit:
                out.append((p["filingDate"][i], p["reportDate"][i], p["accessionNumber"][i], form))
    return sorted(out, reverse=True)


def parse_filing(raw, report):
    rows = []
    for _, body in documents(raw):
        rows += parse_lines(lines(body))
    return to_months(rows, report)


def brd_cases(path):
    """{cik: (filed YYYY-MM-DD, distrib_equity)} from the BRD Cases table."""
    out = {}
    with open(path, encoding="latin-1") as f:
        for r in csv.DictReader(f):
            try:
                filed = dt.datetime.strptime(r["DateFiled"].strip(), "%m/%d/%Y").date().isoformat()
            except ValueError:
                continue
            out.setdefault(r["CikBefore"].strip().lstrip("0"), []).append((filed, r.get("DistribEquity", "").strip()))
    return out


def build(args):
    root = args.root
    spans = json.load(open(os.path.join(root, ".sp500_history.json")))
    yahoo = {}
    if os.path.exists(args.yahoo):
        yahoo = json.load(open(args.yahoo))
    else:
        print("warning: no %s, Yahoo exclusion skipped" % args.yahoo, file=sys.stderr)
    brd = brd_cases(args.brd) if args.brd else {}
    # the walk reads this file as one with the archive, so a reused ticker there (NSM: Nationstar's
    # 2012-2018 bars, not National Semiconductor's) would splice two companies into one series
    with open(os.path.join(root, "tests/dead-members.csv")) as f:
        archive = {l.split(",", 1)[0] for l in f if not l.startswith("#")}
    out, report = [], []
    for tk, cik in sorted(targets(root).items()):
        sp = [(a, b) for a, b in spans.get(tk, [])]
        if not sp or any(b is None or b >= "2014" for a, b in sp):
            report.append((tk, "a span runs past 2013"))
            continue
        if tk in archive:
            report.append((tk, "the archive holds another company under this ticker"))
            continue
        if tk in yahoo and yahoo_scores(yahoo[tk][1], sp):
            report.append((tk, "Yahoo already scores it"))
            continue
        exit_ = max(b for a, b in sp)
        per = []
        for filed, rep, acc, form in filings_of(cik, exit_, args.cache):
            raw = dr._get("https://www.sec.gov/Archives/edgar/data/%d/%s.txt" % (int(cik), acc), args.cache, binary=True)
            if raw is None or not rep:
                continue
            per.append(parse_filing(raw.decode("latin-1"), rep))
        s, why = chain(per)
        if not s:
            report.append((tk, "no price table read"))
            continue
        ex = dt.date.fromisoformat(exit_)
        last = max(s)
        gap = (ex.year - last[0]) * 12 + ex.month - last[1]
        if gap > 15:  # the walk refuses a dead series that stops before the exit
            report.append((tk, "ends %d months before the exit" % gap))
            continue
        pad = (ex.year, ex.month) if 0 < gap <= 15 else None
        rows = monthly(s, pad)
        term = ""
        cases = brd.get(cik.lstrip("0"), []) if tk not in NO_TERMINAL else []
        for filed, eq in sorted(cases):
            if filed <= str(ex + dt.timedelta(days=365)) and not (eq and float(eq) > 0):
                y, m = int(filed[:4]), int(filed[5:7])
                ly, lm = int(rows[-1][0][:4]), int(rows[-1][0][5:7])
                y, m = max((y, m), add_months(ly, lm, 1))
                mid = rows[-1][1]
                while (ly, lm) < add_months(y, m, -1):
                    ly, lm = add_months(ly, lm, 1)
                    rows.append(("%d-%02d" % (ly, lm), mid, 0.0))
                rows.append(("%d-%02d" % (y, m), mid * 0.01, 0.0))
                term = " +1%% bar %d-%02d" % (y, m)
                break
        out += [(tk, mo, c, d) for mo, c, d in rows]
        report.append((tk, "%s..%s %d quarters%s%s%s" % (rows[0][0], rows[-1][0], len(s), " padded" if pad else "", term, "; " + why if why else "")))
    with open(args.out, "w") as f:
        f.write("# (#1314) quarterly (high+low)/2 sale prices of exited S&P 500 members, read from the Item 5 / annual-report\n")
        f.write("# tables of their 10-K filings on SEC EDGAR (tools/item5_prices.py), at the quarter-end month, carried forward\n")
        f.write("# monthly; split-adjusted onto each name's last filing; 1%-of-last bars for bankruptcies from the\n")
        f.write("# Florida-UCLA-LoPucki Bankruptcy Research Database\n")
        f.write("ticker,month,close,div\n")
        for tk, mo, c, d in out:
            f.write("%s,%s,%s,%s\n" % (tk, mo, ("%.4f" % c).rstrip("0").rstrip("."), ("%.4f" % d).rstrip("0").rstrip(".")))
    for tk, msg in report:
        print("%-6s %s" % (tk, msg))


def selftest():
    assert num("70 1/4") == 70.25 and num("70-1/4") == 70.25 and num("7/8") == 0.875 and num("70\xbd") == 70.5
    assert num(".10") == 0.1 and num("3/7") is None
    assert numbers("March 31 $ 70 1/4 $ 55 1/2 $ 79 1/4 $ 69 3/8") == [70.25, 55.5, 79.25, 69.375]
    assert numbers("First $ 55 11 / 16 $ 27 7 / 8 $ 34 7 / 8 $ 30 5 / 32") == [55.6875, 27.875, 34.875, 30.15625]
    # year columns (AMR's 1993 10-K)
    t = ["1993 1992", "High Low High Low", "QUARTER ENDED", "March 31 $ 70 1/4 $ 55 1/2 $ 79 1/4 $ 69 3/8",
         "June 30 72 7/8 60 73 61 5/8", "September 30 68 3/8 59 1/2 66 7/8 55 1/4", "December 31 71 3/4 63 1/4 67 1/2 55"]
    r = parse_lines(t)
    assert len(r) == 8 and (1993, ("m", 3), 70.25, 55.5, 0.0) in r and (1992, ("m", 12), 67.5, 55.0, 0.0) in r, r
    assert parse_lines(t + ["January 1, 1994 to February 19, 1994 $ 80 $ 70"]) == r
    assert to_months(r, "1994-01-01") == to_months(r, "1993-12-31")
    assert to_months([(1992, 1, 2.0, 1.0, 0.0)], "1993-12-31") == {}
    m = to_months(r, "1993-12-31")
    assert m[(1993, 3)] == (70.25, 55.5, 0.0) and m[(1992, 12)] == (67.5, 55.0, 0.0)
    # year blocks with a dividend column, fiscal year ending in February, labelled by its end year
    t = ["Price Range High Low Dividends", "Fiscal 1999", "First Quarter $40 $30 $.05", "Second Quarter 42 31 .05",
         "Third Quarter 44 32 .05", "Fourth Quarter 50 40 .05", "Fiscal 1998", "First Quarter 20 10 .05",
         "Second Quarter 22 11 .05", "Third Quarter 24 12 .05", "Fourth Quarter 26 13 .05"]
    m = to_months(parse_lines(t), "1999-02-28")
    assert m[(1999, 2)] == (50, 40, 0.05) and m[(1998, 5)] == (40, 30, 0.05) and m[(1998, 2)] == (26, 13, 0.05), m
    # transposed quarterly data
    t = ["2004", "First Second Third Fourth", "Net sales 100 200 300 400", "Stock price:", "High 10 11 12 13",
         "Low 5 6 7 8"]
    m = to_months(parse_lines(t), "2004-12-31")
    assert m == {(2004, 3): (10, 5, 0.0), (2004, 6): (11, 6, 0.0), (2004, 9): (12, 7, 0.0), (2004, 12): (13, 8, 0.0)}, m
    # dividends in cents; '1st Quarter' rows
    t = ["1999 1998", "Dividend High Low Dividend High Low", "1st Quarter 42 cents $30-7/8 $28 41 cents $29 $25"]
    assert parse_lines(t) == [(1999, 1, 30.875, 28, 0.42), (1998, 1, 29, 25, 0.41)], parse_lines(t)
    # dividends after all price columns; four year labels for two groups
    t = ["Fiscal 2001 Fiscal 2000 Fiscal 2001 Fiscal 2000", "Quarter High Low High Low Cash Dividends Declared",
         "First $35.00 $22.31 $48.00 $36.56 $0.10 $0.08", "1st...... 7.42 3.91 $24.59 $16.99 $.0175 $.0175"]
    assert parse_lines(t) == [(2001, 1, 35, 22.31, 0.1), (2000, 1, 48, 36.56, 0.08), (2001, 1, 7.42, 3.91, 0.0175),
                              (2000, 1, 24.59, 16.99, 0.0175)], parse_lines(t)
    # 'Low High' order, an ordinal split by a space
    t = ["Fiscal 2001 Fiscal 2000", "Low \u2014 High Low \u2014 High", "Common Stock 1 st $ 27.55 $ 40.00 $ 20.10 $ 30.00"]
    assert parse_lines(t) == [(2001, 1, 40, 27.55, 0.0), (2000, 1, 30, 20.1, 0.0)], parse_lines(t)
    # the year in the first row's label, blank dividends below
    t = ["High Low Dividends", "December 31, 2001* First Quarter $ 37.33 $ 19.50 $ 0.01", "Second Quarter 32.10 20.11 --"]
    assert parse_lines(t) == [(2001, 1, 37.33, 19.5, 0.01), (2001, 2, 32.1, 20.11, 0.0)], parse_lines(t)
    # transposed, labels with words before High / Low
    t = ["First Second Third Fourth", "Quarter Quarter Quarter Quarter", "2000",
         "Common stock price - high $ 31.13 $ 42.69 $ 74.00 $ 85.94", "Common stock price - low 20.00 30.00 40.00 50.00"]
    assert parse_lines(t) == [(2000, 1, 31.13, 20, 0.0), (2000, 2, 42.69, 30, 0.0), (2000, 3, 74, 40, 0.0),
                              (2000, 4, 85.94, 50, 0.0)], parse_lines(t)
    # transposed, quarters printed newest first, the year label far above the price rows (WaMu's 2006 10-K)
    t = ["Year Ended December 31, 2006", "Fourth Quarter Third Quarter Second Quarter First Quarter"] + [
        "Net income %d 1 2 3" % k for k in range(20)] + ["Common stock price per share:", "High 46.29 46.42 46.48 45.51",
                                                         "Low 42.19 41.47 42.53 41.89"]
    assert sorted(parse_lines(t))[0] == (2006, 1, 45.51, 41.89, 0.0), parse_lines(t)
    # transposed, High inside a label naming the par value, a block per year (GM's 2006 10-K)
    t = ["2006 Quarters", "1st 2nd 3rd 4th", "Cash dividends per share of common stock $1 2 / 3 par value $ 0.25 $ 0.25 $ 0.25 $ 0.25",
         "Price range of common stock $1 2 / 3 par value(1): High $ 24.60 $ 30.56 $ 33.64 $ 36.56",
         "Low $ 18.47 $ 19.00 $ 27.12 $ 28.49", "2005 Quarters", "1st 2nd 3rd 4th",
         "Price range of common stock $1 2 / 3 par value(1): High $ 40.80 $ 36.65 $ 37.70 $ 31.50",
         "Low $ 27.98 $ 24.67 $ 30.21 $ 18.33"]
    r = parse_lines(t)
    assert r[0] == (2006, 1, 24.6, 18.47, 0.0) and r[7] == (2005, 4, 31.5, 18.33, 0.0) and len(r) == 8, r
    assert numbers("$1-2/3 par value (1): High $41.12 $39.50 $43.23 $54.39") == [41.12, 39.5, 43.23, 54.39]
    # transposed with a fiscal-year column last (Novell's 2001 10-K); a fifth number that is no year high is refused
    t = ["FIRST SECOND THIRD FOURTH FISCAL", "QUARTER QUARTER QUARTER QUARTER YEAR", "FISCAL 2001",
         "High........ $ 9.344 $ 8.719 $ 5.690 $ 5.050 $ 9.344", "Low......... $ 4.906 $ 3.550 $ 4.500 $ 3.100 $ 3.100"]
    assert parse_lines(t) == [(2001, 1, 9.344, 4.906, 0.0), (2001, 2, 8.719, 3.55, 0.0), (2001, 3, 5.69, 4.5, 0.0),
                              (2001, 4, 5.05, 3.1, 0.0)], parse_lines(t)
    t[3] = "High........ $ 9.344 $ 8.719 $ 5.690 $ 5.050 $ 7.000"
    assert parse_lines(t) == [], parse_lines(t)
    # quarterly-results note: Low-High pairs per year, the year header 20+ lines above (Molex's 2004 10-K)
    t = ["Quarter 2004 2003 2002"] + ["Net revenue %dst 1 2 3" % k for k in range(20)] + [
        "Stock Prices Low - High Low - High Low - High", "Common Stock 1st $26.14 $31.10 $22.85 $33.75 $25.76 $36.59"]
    assert parse_lines(t) == [(2004, 1, 31.1, 26.14, 0.0), (2003, 1, 33.75, 22.85, 0.0), (2002, 1, 36.59, 25.76, 0.0)], parse_lines(t)
    # a part quarter after the year end is no quarter of the table (Symbol's 2005 10-K)
    t = ["High Low", "Year ended December 31, 2005:", "Fourth Quarter 12.95 8.01", "Year ended December 31, 2006:",
         "First Quarter (through February 28, 2006) 13.70 11.20"]
    assert parse_lines(t) == [(2005, 4, 12.95, 8.01, 0.0)], parse_lines(t)
    # a quarter split at a delisting date: the later part stands for the quarter (Winn-Dixie's 2005 10-K)
    t = ["High Low Dividends Per Share", "2005:", "Second Quarter $ 5.05 2.97 —",
         "Third Quarter, through February 22, 2005 $ 4.00 1.41 —", "Third Quarter, subsequent to February 22, 2005 $ 1.55 0.55 —"]
    assert parse_lines(t) == [(2005, 2, 5.05, 2.97, 0.0), (2005, 3, 1.55, 0.55, 0.0)], parse_lines(t)
    # HTML row: one line per <tr>
    assert [x for x in lines("<table><tr><td>First Quarter</td><td>$</td><td>12.50</td><td>10</td></tr></table>") if x] == [
        "First Quarter $ 12.50 10"]
    # chain: a 2:1 split shows as a steady 0.5 ratio; a broken overlap cuts
    new = {(2001, 3): (10, 8, 0), (2000, 12): (9, 7, 0)}
    old = {(2000, 12): (18, 14, 0), (2000, 9): (16, 12, 0)}
    s, why = chain([new, old])
    assert s[(2000, 9)] == (8, 6, 0) and not why
    s, why = chain([new, {(2000, 12): (18.4, 14, 0), (2000, 9): (16, 12, 0)}])
    assert s[(2000, 9)] == (8, 6, 0) and not why, s
    s, why = chain([new, {(2000, 12): (18, 7, 0), (2000, 9): (1, 1, 0)}, old])
    assert s[(2000, 9)] == (8, 6, 0) and why == "1 disagreeing filings skipped", why
    s, why = chain([new, {(1999, 3): (1, 1, 0)}])
    assert (1999, 3) not in s and why.startswith("no overlap")
    assert monthly({(2000, 3): (10, 8, 0.1), (2000, 6): (12, 10, 0)}, (2000, 8)) == [
        ("2000-03", 9, 0.1), ("2000-04", 9, 0), ("2000-05", 9, 0), ("2000-06", 11, 0), ("2000-07", 11, 0), ("2000-08", 11, 0)]
    print("selftest OK")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=".")
    ap.add_argument("--cache", help="EDGAR cache dir (fresh, outside the repo)")
    ap.add_argument("--brd", help="BRD Cases CSV")
    ap.add_argument("--yahoo", default=".long_history_cache.json")
    ap.add_argument("--out", default="tests/dead-prices.csv")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    selftest() if a.selftest else build(a)
