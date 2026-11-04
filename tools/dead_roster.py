#!/usr/bin/env python3
"""Build tests/dead-roster.csv: every S&P 500 exit since 1996 with WHY the name left and WHICH free archive holds its bars.

(#1307) The (#1296) census could grade no round-13 avoidance gate: it reads coverage against a 2 %/yr
literature base rate and the archive's dead names all exit in 2014 or later, so every cutoff before then
read UNDER-COVERED and the gates UNGRADEABLE. This roster replaces the base rate with the facts. For each
exited member in the point-in-time spans (`.sp500_history.json`, fja05680's file, 1996 on) it records:

  exit        bankrupt | distress | acquired | alive | spinoff | unknown
  exit_date   the event that ended the span: the bankruptcy filing, the 8-K, the Form 25/15, or the
              span end when nothing better is known
  source      which rule fired, in the pre-registered precedence below
  series      wiki | tiingo | none: which free archive carries bars to the exit; `none` = no free price
              source exists, the name walks nowhere, whatever the roster says

Precedence (stated before any row was classified; the first rule that fires wins):
  a. a Florida-UCLA-LoPucki BRD case filed within 18 months either side of the span end, or a 5-letter
     last symbol ending in Q (the OTC bankruptcy marker: ENRNQ, WCOEQ, LEHMQ, AAMRQ, FLMIQ)   -> bankrupt
  b. tests/failures.csv `bankrupt` by CIK or ticker, or an EDGAR 8-K with item 1.03 filed within 18
     months after the span end (items exist from 2004-08)                                   -> bankrupt
  c. an 8-K item 3.01, a Form 25/25-NSE or a Form 15 within 12 months after the span end, with no
     acquisition filing before it                                                         -> distress
     (#1320) and with no deal filing in the 12 months before it: an 8-K item 5.01 (item 1 before 2004-08), a
     DEFM14A/C or PREM14A/C, a 425, a tender (SC 14D1, SC 14D9, SC TO-T) or a going-private SC 13E3. The
     delisting form then closed the deal: one filed from 12 months before the span end to 45 days after it
     -> acquired; a later one explains the form without classing the exit (rules e-g decide)
  d. an 8-K item 2.01 or a DEFM14A/DEFM14C within 6 months before the span end, or the Wikipedia reason
     reading acquired/merged                                                               -> acquired
     (#1320) rule c first took the 2.01 and the DEFM14 alone as the deal, so once the ticker-CIK map gave the
     1996-2013 exits a CIK, 42 names newly read distress; the deal filings move 30 of them to acquired
     (Albertsons, Quaker Oats, Mellon, King Pharmaceuticals). A deal filing with no delisting form after it
     stays out of rule d: spinoffs file a 5.01 (Vontier) and buyers file 425s (Transocean in 2017). Residue:
     six 1998-2000 mergers still read distress (Digital Equipment, US West), as their 8-Ks list items
     5 and 7 and no deal form of theirs is on EDGAR.
  e. the Wikipedia changes-table reason text otherwise: bankrupt/distress/spinoff/alive words
  f. a span still open later, or a current EDGAR ticker                                   -> alive
  g. none                                                                                 -> unknown

Sources, all free and keyless: the spans file (MIT); the BRD cases table (Excel or CSV, downloaded by hand);
EDGAR `submissions/CIK##########.json` (10 req/s, User-Agent); EDGAR `cik-lookup-data.txt` for name -> CIK;
the Wikipedia "Historical components of the S&P 500" wikitext for ticker -> name and reason; Tiingo's `supported_tickers.csv` and
tests/dead-members.csv for the `series` column. Everything fetched is cached under `--cache`; `--offline`
reads the cache and the tree only, and classifies with what it has (source `qsuffix`/`failures`/`spans`).

WHY A COUNT FLOOR IS A HARD REFUSAL. A spans file that parsed to a tenth of its rows would write a roster
that looks plausible and grades a different question: under MIN_EXITS exited spans nothing is written.

`--census` prints, per half-year cutoff, the members the walk could score (a free series exists) against
the ones it could not, split by exit class: the number that decides whether a price source for the dead
names is worth building, and the base rate the (#1296) census should read instead of 2 %/yr.

Not wired into the binary: run it by hand, commit the CSV it writes. `--selftest` runs the unit tests.
"""
import argparse, csv, datetime, hashlib, io, json, os, re, sys, time, unittest, urllib.error, urllib.parse, urllib.request, zipfile
import xml.etree.ElementTree as ET

SUBMISSIONS = "https://data.sec.gov/submissions/{}"
CIK_LOOKUP = "https://www.sec.gov/Archives/edgar/cik-lookup-data.txt"
COMPANY_TICKERS = "https://www.sec.gov/files/company_tickers.json"
WIKI_API = "https://en.wikipedia.org/w/api.php"
WIKI_PAGE = "Historical components of the S&P 500"  # the changes table left the list page in 2026
DEAL_FORMS = ("425", "SC 14D1", "SC 14D9", "SC14D9C", "SC TO-T", "SC 13E3", "SC 13E-3")  # rule c, besides DEFM14/PREM14 and the 8-K items
UA = "FolioMan research folioman@researchmail.org"  # SEC fair access wants a name and a contact
PACE = 0.13  # seconds between SEC calls; SEC refuses an IP over 10 req/s for about 10 minutes
MIN_EXITS = 300  # the fixture spans hold 756 closed spans; a tenth of that is a broken parse
HEADER = ["ticker", "start_date", "end_date", "cik", "name", "exit", "exit_date", "source", "series"]
EXITS = ("bankrupt", "distress", "acquired", "alive", "spinoff", "unknown")
QSUFFIX = re.compile(r"^[A-Z]{4}Q$")
TICKER_TMPL = re.compile(r"\{\{\s*(?:NYSE|Nasdaq|NYSEAmerican|NYSEArca|AMEX)[A-Za-z]*\s*\|\s*([A-Za-z0-9][A-Za-z0-9.\-]{0,6})\s*[}|]", re.I)
DTS = re.compile(r"\{\{\s*dts\s*\|\s*(\d{4})\s*\|\s*(\d{1,2})\s*\|\s*(\d{1,2})", re.I)
MONTHS = {m: i + 1 for i, m in enumerate("january february march april may june july august september october november december".split())}
TEXT_DATE = re.compile(r"(" + "|".join(MONTHS) + r")\s+(\d{1,2}),?\s+(\d{4})", re.I)
REASON_WORDS = [  # the first match in this order classes the Wikipedia reason text
    ("bankrupt", re.compile(r"bankrupt|chapter 11|chapter 7|insolven|receivership|liquidat", re.I)),
    ("acquired", re.compile(r"acquir|merg|bought|buyout|taken private|take-?over|took over|purchas|tender offer", re.I)),
    ("spinoff", re.compile(r"spun|spin-?off|split into|separat", re.I)),
    ("alive", re.compile(r"market cap|moved to|midcap|smallcap|small-?cap|no longer (?:representative|eligib)|eligib|liquidity|replaced", re.I)),
    ("distress", re.compile(r"delist|deregist|fail|collaps|seiz|conservatorship|went out of business", re.I)),
]
STOP = {"INC", "INCORPORATED", "CORP", "CORPORATION", "CO", "COMPANY", "LTD", "LIMITED", "PLC", "HOLDINGS", "HOLDING", "GROUP", "THE", "LP", "LLC", "NV", "SA", "AG", "TRUST", "COS", "COMPANIES", "DE", "NEW", "OLD"}

_last = [0.0]


def day(s):
    return datetime.date.fromisoformat(str(s)[:10])


def _get(url, cache_dir, binary=False, tries=8):
    """The body at `url` (JSON unless `binary`), None on a 404. Cached by URL hash. (#728)'s fetcher."""
    os.makedirs(cache_dir, exist_ok=True)
    path = os.path.join(cache_dir, hashlib.sha1(url.encode()).hexdigest() + (".bin" if binary else ".json"))
    if os.path.exists(path):
        with open(path, "rb" if binary else "r") as fh:
            return fh.read() if binary else json.load(fh)
    for attempt in range(tries):
        gap = PACE - (time.time() - _last[0])
        if gap > 0:
            time.sleep(gap)
        _last[0] = time.time()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            with urllib.request.urlopen(req, timeout=120) as r:
                body = r.read() if binary else json.load(r)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                body = None
            elif e.code in (403, 429) or e.code >= 500:
                wait = min(600, 10 * 2 ** attempt)
                print("  %s: %d, backing off %ds" % (urllib.parse.urlsplit(url).netloc, e.code, wait), file=sys.stderr)
                time.sleep(wait)
                continue
            else:
                raise
        except Exception as e:  # transient DNS/TLS/timeout
            print("  %s: %s, retrying" % (urllib.parse.urlsplit(url).netloc, e), file=sys.stderr)
            time.sleep(5)
            continue
        if body is not None:
            with open(path, "wb" if binary else "w") as fh:
                fh.write(body) if binary else json.dump(body, fh)
        return body
    raise SystemExit("gave up on %s after %d tries" % (url, tries))


# ── inputs ─────────────────────────────────────────────────────────────────────────────────────────────


def load_spans(path):
    """{ticker: [(start, end|None), ...]} from the `.sp500_history.json` cache or fja05680's CSV."""
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    out = {}
    if path.endswith(".json"):
        for tk, spans in json.loads(text).items():
            out[tk] = [(day(s), day(e) if e else None) for s, e in spans]
        return out
    for row in csv.DictReader(io.StringIO(text)):
        tk = row["ticker"].strip().upper().replace(".", "-")
        out.setdefault(tk, []).append((day(row["start_date"]), day(row["end_date"]) if row["end_date"].strip() else None))
    return out


def exits(spans):
    """Every CLOSED span as (ticker, start, end), sorted by end; a name that left and came back has one per leave."""
    return sorted(((tk, s, e) for tk, ss in spans.items() for s, e in ss if e is not None), key=lambda x: (x[2], x[0]))


def load_failures(path):
    """tests/failures.csv -> ({cik: (date, kind)}, {ticker: (date, kind)}), bankrupt rows only."""
    by_cik, by_tk = {}, {}
    if not os.path.exists(path):
        return by_cik, by_tk
    with open(path, encoding="utf-8") as fh:
        for row in csv.DictReader(l for l in fh if not l.startswith("#")):
            if row.get("kind") != "bankrupt":
                continue
            ev = (day(row["event_date"]), "bankrupt")
            by_cik[row["cik"].zfill(10)] = ev
            if row.get("ticker"):
                by_tk[row["ticker"].strip().upper()] = ev
    return by_cik, by_tk


def xlsx_rows(data):
    """The first sheet of an .xlsx as rows of strings, with the stdlib only (shared strings resolved)."""
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        ns = {"m": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"}
        shared = []
        if "xl/sharedStrings.xml" in z.namelist():
            root = ET.fromstring(z.read("xl/sharedStrings.xml"))
            shared = ["".join(t.text or "" for t in si.iter("{%s}t" % ns["m"])) for si in root.findall("m:si", ns)]
        sheet = sorted(n for n in z.namelist() if re.fullmatch(r"xl/worksheets/sheet\d+\.xml", n))[0]
        root = ET.fromstring(z.read(sheet))
        rows = []
        for r in root.iter("{%s}row" % ns["m"]):
            cells = {}
            for c in r.findall("m:c", ns):
                ref = re.match(r"([A-Z]+)", c.get("r", "")).group(1)
                col = 0
                for ch in ref:
                    col = col * 26 + ord(ch) - 64
                v = c.find("m:v", ns)
                val = "" if v is None else v.text or ""
                if c.get("t") == "s" and val:
                    val = shared[int(val)]
                elif c.get("t") == "inlineStr":
                    val = "".join(t.text or "" for t in c.iter("{%s}t" % ns["m"]))
                cells[col - 1] = val
            width = max(cells) + 1 if cells else 0
            rows.append([cells.get(i, "") for i in range(width)])
        return rows


def excel_date(v):
    """An ISO date from an Excel serial or a date-like string, None when neither."""
    s = str(v).strip()
    if re.fullmatch(r"\d{4,5}(\.0+)?", s):
        return datetime.date(1899, 12, 30) + datetime.timedelta(days=int(float(s)))
    m = re.match(r"(\d{4})-(\d{2})-(\d{2})", s) or re.match(r"(\d{1,2})/(\d{1,2})/(\d{4})", s)
    if not m:
        return None
    g = m.groups()
    return datetime.date(int(g[0]), int(g[1]), int(g[2])) if len(g[0]) == 4 else datetime.date(int(g[2]), int(g[0]), int(g[1]))


def load_brd(path):
    """The LoPucki BRD cases table (xlsx or csv) -> [{name, filed, cik, equity}] ; refuses when the
    columns it needs are not there (a renamed export is an unread observation, not a quiet empty list)."""
    if path.lower().endswith(".xlsx"):
        with open(path, "rb") as fh:
            rows = xlsx_rows(fh.read())
    else:
        with open(path, encoding="utf-8", errors="replace", newline="") as fh:
            rows = list(csv.reader(fh))
    head = [re.sub(r"[^a-z0-9]", "", h.lower()) for h in rows[0]]
    want = {"name": ("namecorp", "name", "companyname", "debtorname"), "filed": ("datefiled", "filed", "filingdate"),
            "cik": ("cikbefore", "cik"), "equity": ("distribequity", "equitydistribution", "distributiontoequity")}
    idx = {}
    for k, names in want.items():
        hits = [i for i, h in enumerate(head) if h in names]
        if not hits and k != "equity":
            raise SystemExit("BRD: no %s column among %s" % (k, rows[0]))
        idx[k] = hits[0] if hits else None
    out = []
    for r in rows[1:]:
        if len(r) <= max(i for i in idx.values() if i is not None):
            continue
        filed = excel_date(r[idx["filed"]])
        if not filed:
            continue
        cik = re.sub(r"\D", "", r[idx["cik"]]) if idx["cik"] is not None else ""
        out.append({"name": r[idx["name"]].strip(), "filed": filed, "cik": cik.zfill(10) if cik else "",
                    "equity": (r[idx["equity"]].strip().lower() if idx["equity"] is not None else "")})
    return out


def norm_name(s):
    """A company name as a comparable key: upper, letters and digits, corporate suffixes and EDGAR's
    `/DE/`-style state or `/NEW/` qualifiers dropped."""
    s = re.sub(r"/[A-Z]{2,3}/?", " ", s.upper())
    words = [w for w in re.sub(r"[^A-Z0-9 ]", " ", s.replace("&", " AND ")).split() if w not in STOP]
    return " ".join(words)


def load_cik_lookup(text):
    """EDGAR cik-lookup-data.txt (`NAME:CIK:` per line) -> {normalised name: cik}; a name used by two CIKs
    maps to None, so a caller never guesses between them."""
    out = {}
    for line in text.splitlines():
        parts = line.rsplit(":", 2)
        if len(parts) < 3 or not parts[1].strip().isdigit():
            continue
        key = norm_name(parts[0])
        if not key:
            continue
        cik = parts[1].strip().zfill(10)
        out[key] = None if key in out and out[key] != cik else cik
    return out


def wiki_cell(s):
    """A wikitext cell as plain text: refs and templates dropped, a link read as its shown text."""
    s = re.sub(r"<ref[^>]*/>|<ref.*?</ref>|\{\{.*?\}\}", "", s, flags=re.S)
    return " ".join(re.sub(r"\[\[(?:[^|\]]*\|)?([^\]]*)\]\]", r"\1", s).split())


def wiki_tickers(cell):
    """The symbols in a ticker cell: an exchange template's, or plain uppercase words (the 2026 table)."""
    found = TICKER_TMPL.findall(cell) or [w for w in re.split(r"[\s,;/]+", wiki_cell(cell)) if re.fullmatch(r"[A-Z][A-Z0-9.\-]{0,6}", w)]
    return [t.upper().replace(".", "-") for t in found]


def wiki_changes(wikitext):
    """The changes table -> [(date, added tickers, removed tickers, removed name, reason)]. The table is the
    one with `id="changes"` (2026: moved to its own page, plain-text tickers) or the first under a heading
    naming changes (the list page before that, exchange templates); its cells are read by position: date,
    added ticker, added name, removed ticker, removed name, reason."""
    cut = re.search(r'^\{\|[^\n]*id="changes"', wikitext, re.M) or re.search(r"^=+\s*[^=\n]*changes[^=\n]*=+\s*$", wikitext, re.I | re.M)
    if not cut:
        return []
    body = wikitext[cut.end():].split("\n|}")[0]  # the first table, its closing fence dropped
    out = []
    for raw in re.split(r"^\|-.*$", body, flags=re.M):
        cells = [c.strip() for c in re.split(r"\n\|\|?|\|\|", "\n" + raw.strip())[1:]]
        if len(cells) < 6:
            continue
        m, t = DTS.search(cells[0]), TEXT_DATE.search(cells[0])
        if m:
            date = datetime.date(int(m.group(1)), int(m.group(2)), int(m.group(3)))
        elif t:
            date = datetime.date(int(t.group(3)), MONTHS[t.group(1).lower()], int(t.group(2)))
        else:
            continue
        out.append((date, wiki_tickers(cells[1]), wiki_tickers(cells[3]), wiki_cell(cells[4]), wiki_cell(cells[5])))
    return out


def reason_class(text):
    """The exit class the Wikipedia reason text names, None when it names none."""
    for cls, rx in REASON_WORDS:
        if rx.search(text or ""):
            return cls
    return None


# ── EDGAR ─────────────────────────────────────────────────────────────────────────────────────────────


def filings_of(subs, fetch):
    """Every (form, filingDate, items) of a submissions JSON, the older pages included via `fetch(name)`."""
    out = []
    pages = [subs.get("filings", {}).get("recent", {})]
    for f in subs.get("filings", {}).get("files", []):
        page = fetch(f["name"])
        if page:
            pages.append(page)
    for p in pages:
        forms, dates, items = p.get("form", []), p.get("filingDate", []), p.get("items", []) or [""] * len(p.get("form", []))
        out += [(fm, day(d), it or "") for fm, d, it in zip(forms, dates, items)]
    return out


def classify_edgar(filings, end):
    """Rules b-d on one CIK's filings around the span end `end`: (class, event date, source) or None."""
    after18, after12, before6 = end + datetime.timedelta(days=548), end + datetime.timedelta(days=365), end - datetime.timedelta(days=183)
    items = lambda it: set(x.strip() for x in it.split(",") if x.strip())
    bankrupt = [(d, "8k-1.03") for f, d, it in filings if f.startswith("8-K") and "1.03" in items(it) and end - datetime.timedelta(days=548) <= d <= after18]
    if bankrupt:
        d, s = min(bankrupt)
        return "bankrupt", d, s
    acquired = [(d, "8k-2.01" if f.startswith("8-K") else f.lower()) for f, d, it in filings
                if (f.startswith("8-K") and "2.01" in items(it) or f.startswith("DEFM14")) and before6 <= d <= end + datetime.timedelta(days=45)]
    distress = [(d, "8k-3.01" if f.startswith("8-K") else "form" + f.split("-")[0]) for f, d, it in filings
                if ((f.startswith("8-K") and "3.01" in items(it)) or f.startswith("25") or f.startswith("15-")) and end - datetime.timedelta(days=45) <= d <= after12]
    if distress and not acquired:
        deals = sorted((d, "8k-" + min(items(it) & {"5.01", "1"}) if f.startswith("8-K") else f.lower()) for f, d, it in filings
                       if (f.split("/")[0] in DEAL_FORMS or f.startswith(("DEFM14", "PREM14")) or f.startswith("8-K") and items(it) & {"5.01", "1"})
                       and end - datetime.timedelta(days=365) <= d <= min(distress)[0])
        if not deals:
            d, s = min(distress)
            return "distress", d, s
        if deals[0][0] <= end + datetime.timedelta(days=45):
            return ("acquired",) + deals[0]
    if acquired:
        d, s = min(acquired)
        return "acquired", d, s
    return None


# ── classification ────────────────────────────────────────────────────────────────────────────────────


def cik_for(tk, end, ctx):
    """The CIK of the company that left under `tk` at `end`, or "". A CIK whose first filing came more than
    18 months after the exit is a later holder of the symbol (GLD, AMH and AS for 1990s exits): every member
    filed with the SEC, so its filings and its trading today say nothing about this exit.
    shortcut: an old filer that took the symbol later still passes (MD: McDonnell Douglas in 1997, Pediatrix
    today); only a name source for pre-2014 exits closes that, and the 2026 Wikipedia table has few."""
    cik = ctx.get("cik_of", {}).get(tk, "")
    filings = ctx.get("edgar", {}).get(cik) if cik else None
    if filings and min(d for _, d, _ in filings) > end + datetime.timedelta(days=548):
        return ""
    return cik


def classify(tk, start, end, ctx):
    """One exit through the precedence in the module docstring. `ctx` holds the loaded sources:
    brd_by_cik, brd_by_name, fail_cik, fail_tk, cik_of (ticker -> cik), edgar (cik -> filings),
    wiki (ticker -> (name, reason)), alive (tickers trading today), spans."""
    cik = cik_for(tk, end, ctx)
    later_holder = bool(ctx.get("cik_of", {}).get(tk)) and not cik
    name = ctx.get("wiki", {}).get(tk, ("", ""))[0]
    window = lambda d: abs((d - end).days) <= 548
    # a. BRD within 18 months, or the OTC Q marker
    hits = [c for c in ctx.get("brd_by_cik", {}).get(cik, []) if window(c["filed"])] if cik else []
    if not hits and name:
        hits = [c for c in ctx.get("brd_by_name", {}).get(norm_name(name), []) if window(c["filed"])]
    if hits:
        c = min(hits, key=lambda c: abs((c["filed"] - end).days))
        return "bankrupt", c["filed"], "brd" + ("-noequity" if c["equity"] in ("none", "no", "0") else "")
    if QSUFFIX.match(tk):
        # the class is the marker's; the date is the filing's when a source in the window has one
        ev = (ctx.get("fail_cik", {}).get(cik) if cik else None) or ctx.get("fail_tk", {}).get(tk)
        if ev and window(ev[0]):
            return "bankrupt", ev[0], "qsuffix+failures"
        edgar = classify_edgar(ctx["edgar"][cik], end) if cik and cik in ctx.get("edgar", {}) else None
        if edgar and edgar[0] == "bankrupt":
            return "bankrupt", edgar[1], "qsuffix+" + edgar[2]
        return "bankrupt", end, "qsuffix"
    # b. failures.csv, or an 8-K 1.03
    ev = ctx.get("fail_cik", {}).get(cik) if cik else None
    ev = ev or ctx.get("fail_tk", {}).get(tk)
    if ev and window(ev[0]):
        return "bankrupt", ev[0], "failures"
    edgar = classify_edgar(ctx["edgar"][cik], end) if cik and cik in ctx.get("edgar", {}) else None
    if edgar and edgar[0] == "bankrupt":
        return edgar
    # c./d. EDGAR distress or acquisition filings
    if edgar:
        return edgar
    # d./e. the Wikipedia reason
    wiki = reason_class(ctx.get("wiki", {}).get(tk, ("", ""))[1])
    if wiki:
        return wiki, end, "wiki"
    # f. still trading: a later span that starts within 3 years (a demotion and a return), or a current
    # ticker. A span that starts later than that is the symbol's next holder (AAL: Alexander & Alexander to
    # 1997, American Airlines from 2015), never evidence about this exit.
    later = any(end < s <= end + datetime.timedelta(days=3 * 365) for s, _ in ctx["spans"].get(tk, []))
    recycled = any(s > end + datetime.timedelta(days=3 * 365) for s, _ in ctx["spans"].get(tk, []))
    if later:
        return "alive", end, "spans"
    if tk in ctx.get("alive", set()) and not recycled and not later_holder:
        return "alive", end, "edgar-tickers"
    return "unknown", end, "none"


def series_of(tk, end, wiki_span, tiingo):
    """Which free archive carries bars to this exit: the WIKI file (a series that starts before the exit
    month and reaches it), Tiingo (a USD stock listing that spans the exit), or none. A series that starts
    after the exit is the ticker's next holder."""
    first, last = wiki_span.get(tk, ("9999-99", ""))
    if first < end.strftime("%Y-%m") <= last:
        return "wiki"
    for a, c, b, e in tiingo.get(tk, []):
        if a == "Stock" and c == "USD" and b and e and day(b) < end and day(e) >= end - datetime.timedelta(days=30):
            return "tiingo"
    return "none"


def load_ticker_cik(path, spans):
    """(#1319) {ticker: cik} from tools/ticker_cik_history.py's output, for the spans this run holds. The file keys
    (ticker, start_date, end_date); a symbol with several spans takes the CIK of the LAST closed span the file names,
    since `classify` keys the roster by ticker alone and the latest exit is the one most rows ask about. {} when the
    file is absent."""
    out = {}
    if not path or not os.path.exists(path):
        return out
    with open(path, encoding="utf-8") as fh:
        rows = [r for r in csv.DictReader(l for l in fh if not l.startswith("#")) if r.get("cik")]
    for r in sorted(rows, key=lambda r: r["end_date"]):
        if r["ticker"] in spans:
            out[r["ticker"]] = r["cik"].zfill(10)
    return out


def load_wiki_span(path):
    """{ticker: (first YYYY-MM, last YYYY-MM)} of tests/dead-members.csv."""
    out = {}
    if not os.path.exists(path):
        return out
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            p = line.rstrip("\n").split(",")
            if len(p) == 4 and re.fullmatch(r"\d{4}-\d\d", p[1]):
                lo, hi = out.get(p[0], (p[1], p[1]))
                out[p[0]] = (min(lo, p[1]), max(hi, p[1]))
    return out


def load_tiingo(path):
    """Tiingo supported_tickers (.csv or .zip) -> {ticker: [(assetType, currency, start, end)]}."""
    out = {}
    if not path or not os.path.exists(path):
        return out
    if path.endswith(".zip"):
        with zipfile.ZipFile(path) as z:
            name = [n for n in z.namelist() if n.endswith(".csv")][0]
            text = z.read(name).decode("utf-8", "replace")
    else:
        with open(path, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
    for row in csv.DictReader(io.StringIO(text)):
        out.setdefault(row["ticker"].upper(), []).append((row.get("assetType", ""), row.get("priceCurrency", ""), row.get("startDate", ""), row.get("endDate", "")))
    return out


# ── census ────────────────────────────────────────────────────────────────────────────────────────────


def cutoffs(first, last):
    """Half-year cutoffs (Jan 1, Jul 1) from the first boundary at or after `first` to `last`, the walk's grid to the month."""
    d = datetime.date(first.year, 1 if first.month <= 6 else 7, 1)
    if d < first:  # the first boundary ON or after the first span start (fja05680 opens 1996-01-02)
        d = datetime.date(d.year + (d.month == 7), 7 if d.month == 1 else 1, 1)
    while d <= last:
        yield d
        d = datetime.date(d.year + (d.month == 7), 7 if d.month == 1 else 1, 1)


def census(spans, roster, years, last_exit):
    """Per cutoff with a full `years` window before `last_exit`: members, those whose span closes inside
    the window, split by exit class and by whether a free series carries them. Returns rows of dicts."""
    by_key = {(r["ticker"], r["end_date"]): r for r in roster}
    rows = []
    for c in cutoffs(min(s for ss in spans.values() for s, _ in ss), last_exit - datetime.timedelta(days=365 * years)):
        horizon = c + datetime.timedelta(days=365 * years)
        members = [(tk, s, e) for tk, ss in spans.items() for s, e in ss if s <= c and (e is None or e > c)]
        n = len(members)
        leaving = [(tk, e) for tk, _, e in members if e is not None and e <= horizon]
        counts = {k: 0 for k in EXITS}
        priced = {k: 0 for k in EXITS}
        for tk, e in leaving:
            r = by_key.get((tk, e.isoformat()))
            cls = r["exit"] if r else "unknown"
            counts[cls] += 1
            if r and r["series"] != "none":
                priced[cls] += 1
        rows.append({"cutoff": c, "members": n, "leaving": len(leaving), **{k: counts[k] for k in EXITS}, **{"priced_" + k: priced[k] for k in EXITS}})
    return rows


def print_census(rows, years):
    print("DEAD-NAME CENSUS (#1307) from the roster, HOLD %dy: index members at each cutoff whose span closes inside the window," % years)
    print("by exit class; `priced` = a free archive (WIKI or Tiingo) carries bars to the exit, so the walk can hold the name to its end.")
    print("  cutoff      members leave  bankrupt(priced) distress(priced) acquired(priced)  alive  spinoff unknown(priced)  dead%  base%")
    for r in rows:
        dead = r["bankrupt"] + r["distress"]
        print("  %s  %7d %5d  %8d(%3d)   %8d(%3d)   %8d(%3d)  %6d %7d %7d(%3d)  %5.1f  %5.1f" % (
            r["cutoff"], r["members"], r["leaving"], r["bankrupt"], r["priced_bankrupt"], r["distress"], r["priced_distress"],
            r["acquired"], r["priced_acquired"], r["alive"], r["spinoff"], r["unknown"], r["priced_unknown"],
            100.0 * dead / max(1, r["members"]), 100.0 * (1 - 0.98 ** years)))


# ── main ──────────────────────────────────────────────────────────────────────────────────────────────


def build(spans, ctx, wiki_span, tiingo, since):
    rows = []
    for tk, s, e in exits(spans):
        if e < since:
            continue
        cls, date, source = classify(tk, s, e, ctx)
        rows.append({"ticker": tk, "start_date": s.isoformat(), "end_date": e.isoformat(), "cik": cik_for(tk, e, ctx),
                     "name": ctx.get("wiki", {}).get(tk, ("", ""))[0].replace(",", " "), "exit": cls, "exit_date": date.isoformat(),
                     "source": source, "series": series_of(tk, e, wiki_span, tiingo)})
    return rows


def write_roster(rows, path, note, brd=False):
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("# (#1307) every S&P 500 exit in the point-in-time spans, with why the name left and which free archive holds its bars\n")
        fh.write("# written by tools/dead_roster.py; %s\n" % note)
        if brd:  # the BRD's one condition of use: credit it on anything holding its data
            fh.write("# bankruptcy cases from the Florida-UCLA-LoPucki Bankruptcy Research Database (BRD)\n")
        fh.write(",".join(HEADER) + "\n")
        for r in rows:
            fh.write(",".join(str(r[h]) for h in HEADER) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--spans", default="tests/fixture/.sp500_history.json", help="the membership cache or fja05680's CSV")
    ap.add_argument("--ticker-cik", default="tests/ticker-cik.csv", help="(#1319) tools/ticker_cik_history.py's output; read before EDGAR's ticker file, skipped when absent")
    ap.add_argument("--failures", default="tests/failures.csv")
    ap.add_argument("--dead-members", default="tests/dead-members.csv")
    ap.add_argument("--tiingo", default="", help="Tiingo supported_tickers.csv or .zip, for the `series` column")
    ap.add_argument("--brd", default="", help="the LoPucki BRD cases table (.xlsx or .csv), downloaded by hand")
    ap.add_argument("--wiki", default="", help="wikitext of the S&P 500 list page (saved); fetched when absent and online")
    ap.add_argument("--cache", default=".dead_roster_cache")
    ap.add_argument("--offline", action="store_true", help="no network: classify from the tree and the cache alone")
    ap.add_argument("--since", default="1996-01-01", help="ignore exits before this date")
    ap.add_argument("--out", default="tests/dead-roster.csv")
    ap.add_argument("--census", type=int, default=0, metavar="YEARS", help="print the per-cutoff census at this horizon and exit")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        sys.argv = sys.argv[:1]
        unittest.main(module=__name__, verbosity=2)
        return
    spans = load_spans(a.spans)
    closed = exits(spans)
    if len(closed) < MIN_EXITS:
        raise SystemExit("%s: %d closed spans is under the %d floor; nothing written" % (a.spans, len(closed), MIN_EXITS))
    fail_cik, fail_tk = load_failures(a.failures)
    ctx = {"spans": spans, "fail_cik": fail_cik, "fail_tk": fail_tk, "edgar": {}, "wiki": {}, "cik_of": {}, "alive": set(), "brd_by_cik": {}, "brd_by_name": {}}
    # (#1319) the historical ticker -> CIK map tools/ticker_cik_history.py writes, read FIRST: it is keyed by span, so a
    # recycled symbol maps to the holder of THIS span, where EDGAR's ticker file below knows today's holder only
    ctx["cik_of"].update(load_ticker_cik(a.ticker_cik, spans))
    if ctx["cik_of"]:
        print("%d spans carry a CIK from %s" % (len(ctx["cik_of"]), a.ticker_cik), file=sys.stderr)
    if a.brd:
        for c in load_brd(a.brd):
            if c["cik"]:
                ctx["brd_by_cik"].setdefault(c["cik"], []).append(c)
            ctx["brd_by_name"].setdefault(norm_name(c["name"]), []).append(c)
    wikitext = None
    if a.wiki and os.path.exists(a.wiki):
        with open(a.wiki, encoding="utf-8") as fh:
            wikitext = fh.read()
    elif not a.offline:
        q = urllib.parse.urlencode({"action": "query", "prop": "revisions", "rvprop": "content", "rvslots": "main", "format": "json", "titles": WIKI_PAGE})
        body = _get(WIKI_API + "?" + q, a.cache)
        page = next(iter(body["query"]["pages"].values()))
        wikitext = page["revisions"][0]["slots"]["main"]["*"]
    if wikitext:
        for _, _, removed, name, reason in wiki_changes(wikitext):
            for tk in removed:
                ctx["wiki"].setdefault(tk, (name, reason))
    if not a.offline:
        lookup = load_cik_lookup(_get(CIK_LOOKUP, a.cache, binary=True).decode("latin-1"))
        current = _get(COMPANY_TICKERS, a.cache) or {}
        for v in current.values():
            ctx["alive"].add(v["ticker"].upper().replace(".", "-"))
            ctx["cik_of"].setdefault(v["ticker"].upper().replace(".", "-"), str(v["cik_str"]).zfill(10))
        for tk, (name, _) in ctx["wiki"].items():
            if tk not in ctx["cik_of"] and name:
                cik = lookup.get(norm_name(name))
                if cik:
                    ctx["cik_of"][tk] = cik
        for c in ctx["brd_by_cik"]:
            pass  # BRD rows are joined by CIK or name in classify(); nothing to resolve here
        unmatched = [tk for tk, _, _ in closed if tk not in ctx["cik_of"]]
        print("%d of %d exits map to a CIK; %d unmatched (logged, never guessed): %s%s" % (
            len(closed) - len(unmatched), len(closed), len(unmatched), " ".join(unmatched[:40]), " ..." if len(unmatched) > 40 else ""), file=sys.stderr)
        for tk, _, e in closed:
            cik = ctx["cik_of"].get(tk)
            if not cik or cik in ctx["edgar"]:
                continue
            subs = _get(SUBMISSIONS.format("CIK%s.json" % cik), a.cache)
            if subs:
                ctx["edgar"][cik] = filings_of(subs, lambda name: _get(SUBMISSIONS.format(name), a.cache))
    wiki_span = load_wiki_span(a.dead_members)
    tiingo = load_tiingo(a.tiingo)
    rows = build(spans, ctx, wiki_span, tiingo, day(a.since))
    if a.census:
        print_census(census(spans, rows, a.census, max(e for _, _, e in closed)), a.census)
        return
    note = "spans %s; %s; BRD %s; wiki %s; EDGAR %s; tiingo %s" % (
        os.path.basename(a.spans), "offline" if a.offline else "online", "yes" if a.brd else "no", "yes" if wikitext else "no",
        len(ctx["edgar"]), "yes" if tiingo else "no")
    write_roster(rows, a.out, note, bool(a.brd))
    tally = {k: sum(r["exit"] == k for r in rows) for k in EXITS}
    priced = sum(r["series"] != "none" for r in rows)
    print("%d exits since %s -> %s: %s; %d carry a free series to the exit" % (len(rows), a.since, a.out, " ".join("%s %d" % kv for kv in tally.items()), priced), file=sys.stderr)


# ── tests ─────────────────────────────────────────────────────────────────────────────────────────────


class Tests(unittest.TestCase):
    def ctx(self, **kw):
        base = {"spans": {}, "fail_cik": {}, "fail_tk": {}, "edgar": {}, "wiki": {}, "cik_of": {}, "alive": set(), "brd_by_cik": {}, "brd_by_name": {}}
        base.update(kw)
        return base

    def test_q_suffix_is_a_bankruptcy_marker_and_shorter_symbols_are_not(self):
        end = datetime.date(2001, 11, 30)
        self.assertEqual(classify("ENRNQ", datetime.date(1996, 1, 2), end, self.ctx())[0], "bankrupt")
        self.assertEqual(classify("ENRNQ", datetime.date(1996, 1, 2), end, self.ctx())[2], "qsuffix")
        self.assertEqual(classify("Q", datetime.date(1996, 1, 2), end, self.ctx())[0], "unknown", "Qwest's one-letter Q is not the marker")
        self.assertEqual(classify("COMQ", datetime.date(1996, 1, 2), end, self.ctx())[0], "unknown", "four letters ending in Q is a plain symbol")

    def test_brd_beats_everything_and_dates_the_exit(self):
        end = datetime.date(2008, 9, 17)
        case = {"name": "Lehman Brothers Holdings Inc", "filed": datetime.date(2008, 9, 15), "cik": "0000806085", "equity": "none"}
        ctx = self.ctx(cik_of={"LEHMQ": "0000806085"}, brd_by_cik={"0000806085": [case]})
        self.assertEqual(classify("LEHMQ", datetime.date(1998, 1, 12), end, ctx), ("bankrupt", datetime.date(2008, 9, 15), "brd-noequity"))
        far = dict(case, filed=datetime.date(2012, 1, 1))
        ctx = self.ctx(cik_of={"X": "1"}, brd_by_cik={"1": [far]})
        self.assertEqual(classify("X", datetime.date(1998, 1, 12), end, ctx)[0], "unknown", "a case 40 months out is not this exit")
        ctx = self.ctx(wiki={"LEH": ("Lehman Brothers Holdings, Inc.", "")}, brd_by_name={norm_name("Lehman Brothers Holdings Inc"): [case]})
        self.assertEqual(classify("LEH", datetime.date(1998, 1, 12), end, ctx)[2], "brd-noequity", "joined by normalised name when no CIK")

    def test_failures_csv_and_edgar_items(self):
        end = datetime.date(2009, 6, 8)
        ctx = self.ctx(cik_of={"GM": "0000040730"}, fail_cik={"0000040730": (datetime.date(2009, 6, 3), "bankrupt")})
        self.assertEqual(classify("GM", datetime.date(1996, 1, 2), end, ctx), ("bankrupt", datetime.date(2009, 6, 3), "failures"))
        filings = [("8-K", datetime.date(2009, 6, 3), "1.03,2.03,2.04,3.01,8.01,9.01"), ("10-K", datetime.date(2009, 3, 5), "")]
        self.assertEqual(classify_edgar(filings, end), ("bankrupt", datetime.date(2009, 6, 3), "8k-1.03"))
        acq = [("8-K", datetime.date(2016, 5, 20), "2.01,3.01,5.01,9.01"), ("15-12B", datetime.date(2016, 6, 1), "")]
        self.assertEqual(classify_edgar(acq, datetime.date(2016, 5, 23))[0], "acquired", "a 2.01 beside a 3.01 is a completed deal, not distress")
        dis = [("25-NSE", datetime.date(2012, 2, 1), ""), ("15-12G", datetime.date(2012, 3, 1), "")]
        self.assertEqual(classify_edgar(dis, datetime.date(2012, 1, 20)), ("distress", datetime.date(2012, 2, 1), "form25"))
        self.assertIsNone(classify_edgar([("10-K", datetime.date(2012, 3, 1), "")], datetime.date(2012, 1, 20)))
        # (#1320) a tender before the Form 15 is the deal closing (Giddings & Lewis, 1997); one after the exit
        # explains the later Form 25 without making the exit a purchase or a failure
        gidl = [("SC 14D9", datetime.date(1997, 5, 1), ""), ("15-12B", datetime.date(1997, 10, 1), "")]
        self.assertEqual(classify_edgar(gidl, datetime.date(1997, 7, 17)), ("acquired", datetime.date(1997, 5, 1), "sc 14d9"))
        late = [("425", datetime.date(2014, 9, 1), ""), ("25-NSE", datetime.date(2015, 4, 7), "")]
        self.assertIsNone(classify_edgar(late, datetime.date(2014, 6, 20)))
        mkg = [("8-K", datetime.date(2000, 10, 25), "1,7"), ("15-12B", datetime.date(2000, 11, 14), "")]
        self.assertEqual(classify_edgar(mkg, datetime.date(2000, 10, 18)), ("acquired", datetime.date(2000, 10, 25), "8k-1"))
        self.assertIsNone(classify_edgar(mkg[:1], datetime.date(2000, 10, 18)), "a change in control with no delisting is a spinoff's 5.01")
        ctx = self.ctx(cik_of={"X": "1"}, edgar={"1": acq})
        self.assertEqual(classify("X", datetime.date(2000, 1, 1), datetime.date(2016, 5, 23), ctx)[2], "8k-2.01")
        ctx = self.ctx(cik_of={"WCOEQ": "0000723527"}, edgar={"0000723527": [("8-K", datetime.date(2002, 7, 22), "1.03,9.01")]})
        self.assertEqual(classify("WCOEQ", datetime.date(1996, 4, 1), datetime.date(2002, 5, 15), ctx), ("bankrupt", datetime.date(2002, 7, 22), "qsuffix+8k-1.03"),
                         "the Q marker classes it, the filing dates it")

    def test_wiki_reason_and_alive(self):
        ctx = self.ctx(wiki={"ABC": ("ABC Corp", "Acquired by XYZ Inc.")})
        self.assertEqual(classify("ABC", datetime.date(2000, 1, 1), datetime.date(2010, 1, 1), ctx)[:1], ("acquired",))
        ctx = self.ctx(wiki={"ABC": ("ABC Corp", "Market capitalization change")})
        self.assertEqual(classify("ABC", datetime.date(2000, 1, 1), datetime.date(2010, 1, 1), ctx)[0], "alive")
        ctx = self.ctx(spans={"AAL": [(datetime.date(1996, 1, 2), datetime.date(2013, 12, 9)), (datetime.date(2015, 3, 23), None)]})
        self.assertEqual(classify("AAL", datetime.date(1996, 1, 2), datetime.date(2013, 12, 9), ctx), ("alive", datetime.date(2013, 12, 9), "spans"))
        ctx = self.ctx(spans={"AAL": [(datetime.date(1996, 1, 2), datetime.date(1997, 1, 15)), (datetime.date(2015, 3, 23), None)]}, alive={"AAL"})
        self.assertEqual(classify("AAL", datetime.date(1996, 1, 2), datetime.date(1997, 1, 15), ctx)[0], "unknown",
                         "a span 18 years later is the symbol's next holder, and today's AAL is not the 1997 one")
        ctx = self.ctx(cik_of={"GLD": "1"}, edgar={"1": [("10-K", datetime.date(2005, 3, 1), "")]}, alive={"GLD"})
        self.assertEqual(classify("GLD", datetime.date(1990, 1, 2), datetime.date(1997, 5, 6), ctx)[0], "unknown",
                         "today's GLD first filed in 2005: a later holder, not the 1997 member")
        self.assertEqual(cik_for("GLD", datetime.date(1997, 5, 6), ctx), "")
        ctx["edgar"]["1"].append(("10-K", datetime.date(1998, 3, 1), ""))
        self.assertEqual(classify("GLD", datetime.date(1990, 1, 2), datetime.date(1997, 5, 6), ctx)[2], "edgar-tickers",
                         "a first filing inside 18 months of the exit keeps the CIK")
        self.assertEqual(reason_class("Filed for Chapter 11 bankruptcy protection"), "bankrupt")
        self.assertEqual(reason_class("Spun off from Abbott"), "spinoff")
        self.assertIsNone(reason_class(""))

    def test_wiki_changes_table_parses_dts_and_text_dates(self):
        text = (
            "== Selected changes to the list of S&P 500 components ==\n"
            '{| class="wikitable sortable"\n! Date !! colspan=2 | Added !! colspan=2 | Removed !! Reason\n'
            "|-\n| {{dts|2008|09|17}} || {{NyseSymbol|HRS}} || Harris Corp || {{NyseSymbol|LEH}} || Lehman Brothers || Filed for bankruptcy<ref>x</ref>\n"
            "|-\n| June 8, 2009 || {{NyseSymbol|DTV}} || DirecTV || {{NyseSymbol|GM}} || General Motors || Bankruptcy\n"
            "|-\n| {{dts|2026|01|02}} || {{NasdaqSymbol|NEW}} || New Co || {{NyseSymbol|OLD}} || Old Co || Acquired by New Co\n"
            "|}\n== Next section ==\nnothing here\n"
        )
        rows = wiki_changes(text)
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0][0], datetime.date(2008, 9, 17))
        self.assertEqual(rows[0][2], ["LEH"])
        self.assertEqual(rows[0][3], "Lehman Brothers")
        self.assertEqual(rows[0][4], "Filed for bankruptcy")
        self.assertEqual(rows[1][0], datetime.date(2009, 6, 8))
        self.assertEqual(rows[1][2], ["GM"])
        self.assertEqual(rows[2][4], "Acquired by New Co")

    def test_wiki_changes_reads_the_2026_table_with_plain_tickers(self):
        text = (
            '{| class="wikitable sortable" id="changes"\n|-\n! rowspan="2" | Effective Date\n! colspan="2" | Added\n'
            "! colspan=\"2\" | Removed\n! rowspan=\"2\" | Reason\n! rowspan=\"2\" | Refs\n|-\n! Ticker || Security || Ticker || Security\n"
            "|-\n|October 6, 2026\n|TWLO\n|[[Twilio]]\n|WBD\n|[[Warner Bros. Discovery]]\n|[[Paramount Skydance]] acquired it\n|<ref name=\":1\" />\n"
            "|-\n|| December 5, 2000 \n|| INTU\n|| [[Intuit]] \n|| BS\n|| [[Bethlehem Steel|Bethlehem Steel Corp]] \n|| Market capitalization changes.\n|| <ref name=x>{{cite news | url=https://a.b/c}}</ref>\n"
            "|-\n|October 1, 2026\n|VYLR\n|[[Vylor]]\n|\n|\n|Corteva spun off Vylor\n|<ref name=\":1\" />\n"
            "|}\n"
        )
        rows = wiki_changes(text)
        self.assertEqual(len(rows), 3, "the two header rows are not changes")
        self.assertEqual(rows[0], (datetime.date(2026, 10, 6), ["TWLO"], ["WBD"], "Warner Bros. Discovery", "Paramount Skydance acquired it"))
        self.assertEqual(rows[1], (datetime.date(2000, 12, 5), ["INTU"], ["BS"], "Bethlehem Steel Corp", "Market capitalization changes."))
        self.assertEqual(rows[2][2], [], "an addition with no removal names no exit")

    def test_cik_lookup_refuses_an_ambiguous_name(self):
        text = "ENRON CORP/OR/:0001024401:\nACME INC:0000000001:\nACME CORP:0000000002:\n"
        got = load_cik_lookup(text)
        self.assertEqual(got[norm_name("Enron Corp/OR/")], "0001024401")
        self.assertEqual(norm_name("Enron Corp"), norm_name("ENRON CORP/OR/"), "EDGAR's state qualifier is not part of the name")
        self.assertEqual(norm_name("General Motors Corp /DE/"), norm_name("General Motors Corporation"))
        self.assertIsNone(got[norm_name("Acme")], "two CIKs under one normalised name maps to None, never a guess")

    def test_series_column(self):
        end = datetime.date(2015, 6, 30)
        self.assertEqual(series_of("A", end, {"A": ("2005-09", "2015-06")}, {}), "wiki")
        self.assertEqual(series_of("A", end, {"A": ("2005-09", "2015-05")}, {}), "none", "a WIKI file that stops before the exit month does not carry it")
        self.assertEqual(series_of("A", datetime.date(1997, 1, 15), {"A": ("2005-09", "2018-03")}, {}), "none", "a series that starts after the exit is the next holder")
        self.assertEqual(series_of("A", end, {}, {"A": [("Stock", "USD", "1999-01-04", "2015-06-25")]}), "tiingo")
        self.assertEqual(series_of("A", end, {}, {"A": [("Stock", "USD", "2016-01-04", "2020-01-01")]}), "none", "a listing that starts after the exit is the ticker's next holder")

    def test_brd_table_reads_xlsx_dates_and_refuses_missing_columns(self):
        self.assertEqual(excel_date("39706"), datetime.date(2008, 9, 15))
        self.assertEqual(excel_date("2008-09-15"), datetime.date(2008, 9, 15))
        self.assertEqual(excel_date("9/15/2008"), datetime.date(2008, 9, 15))
        self.assertIsNone(excel_date("n/a"))
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "cases.csv")
            with open(p, "w", newline="") as fh:
                csv.writer(fh).writerows([["NameCorp", "DateFiled", "CikBefore", "DistribEquity"], ["Lehman Brothers Holdings Inc", "9/15/2008", "806085", "None"]])
            rows = load_brd(p)
            self.assertEqual(rows[0]["cik"], "0000806085")
            self.assertEqual(rows[0]["filed"], datetime.date(2008, 9, 15))
            self.assertEqual(rows[0]["equity"], "none")
            with open(p, "w", newline="") as fh:
                csv.writer(fh).writerows([["Debtor", "When"], ["x", "y"]])
            with self.assertRaises(SystemExit):
                load_brd(p)

    def test_census_counts_members_and_leavers_per_cutoff(self):
        spans = {"A": [(datetime.date(1996, 1, 2), datetime.date(2001, 11, 30))], "B": [(datetime.date(1996, 1, 2), None)],
                 "C": [(datetime.date(1999, 1, 1), datetime.date(2010, 1, 1))]}
        roster = [{"ticker": "A", "end_date": "2001-11-30", "exit": "bankrupt", "series": "none"},
                  {"ticker": "C", "end_date": "2010-01-01", "exit": "acquired", "series": "wiki"}]
        rows = census(spans, roster, 8, datetime.date(2010, 1, 1))
        first = rows[0]
        self.assertEqual(first["cutoff"], datetime.date(1996, 7, 1), "the first boundary after the spans open")
        self.assertEqual(first["members"], 2)
        self.assertEqual((first["leaving"], first["bankrupt"], first["priced_bankrupt"]), (1, 1, 0))
        at99 = [r for r in rows if r["cutoff"] == datetime.date(1999, 1, 1)][0]
        self.assertEqual(at99["members"], 3)
        self.assertEqual(at99["leaving"], 1, "C leaves in 2010, past an 8y window from 1999")
        self.assertEqual(rows[-1]["cutoff"], datetime.date(2002, 1, 1), "the last cutoff with a full 8y window before the last exit")

    def test_ticker_cik_file_is_read_per_span(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "tc.csv")
            with open(p, "w") as fh:
                fh.write("# comment\nticker,start_date,end_date,cik,edgar_name,source,confidence\n"
                         "AAL,1996-01-02,1997-01-15,3133,ALEXANDER & ALEXANDER,index,C\nFLM,1996-01-02,2003-04-01,352949,FLEMING,phrase,A\nZZZ,2000-01-01,2001-01-01,1,Z,phrase,A\n")
            got = load_ticker_cik(p, {"AAL": [], "FLM": []})
            self.assertEqual(got, {"AAL": "0000003133", "FLM": "0000352949"}, "padded, and only the spans this run holds")
            self.assertEqual(load_ticker_cik(os.path.join(d, "none.csv"), {}), {})

    def test_spans_csv_and_json_agree(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            j, c = os.path.join(d, "s.json"), os.path.join(d, "s.csv")
            with open(j, "w") as fh:
                json.dump({"AAMRQ": [["1996-01-02", "2003-03-14"]], "BF-B": [["1996-01-02", None]]}, fh)
            with open(c, "w") as fh:
                fh.write("ticker,start_date,end_date\nAAMRQ,1996-01-02,2003-03-14\nBF.B,1996-01-02,\n")
            self.assertEqual(load_spans(j), load_spans(c))
            self.assertEqual(exits(load_spans(c)), [("AAMRQ", datetime.date(1996, 1, 2), datetime.date(2003, 3, 14))])


if __name__ == "__main__":
    main()
