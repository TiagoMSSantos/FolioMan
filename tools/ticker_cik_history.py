#!/usr/bin/env python3
"""Build tests/ticker-cik.csv: the SEC CIK behind every exited S&P 500 (or MidCap 400) member, by the symbol it traded under.

(#1319) Three receipts name the same gap: (#1241) refused EX-27 fundamentals because dead names had no CIK
("reopen only if a free historical ticker-to-CIK or ticker-to-name map turns up"), (#1310) mapped 368 of 756
roster spans and left 361 of the 468 pre-2014 exits without one, (#1314) could price 74 of 360 dead names for the
same reason. EDGAR's own ticker file knows today's symbols only. This tool rebuilds the historical map from
EDGAR alone, keyless, in this precedence (the first rule that fires wins; a tie writes nothing):

  phrase   EDGAR full-text search (2001 on), the filer's OWN 10-K / 10-K405 / 10-Q / 8-K saying
           "under the symbol <TK>", "ticker symbol <TK>" or "symbol <TK>" inside [span start - 1y, span end + 1y].
           An acquirer's S-4 quotes the target's symbol too, so only those four forms count. The CIK must have
           filed a 10-K inside the span (its submissions JSON), and its first filing must not come more than 18
           months after the exit ((#1310): a later holder of the symbol). (#1320) It must also FILL the span: first
           filing within a year of the span's start (or of May 1996, EDGAR's phase-in end) and a 10-K within 18
           months of its end; a filer quoting a partner's symbol rarely does. "NYSE: <TK>" / "Nasdaq: <TK>" were
           dropped: 33 of 33 spans they took on the first live run were another filer's.
  name     the same search on the member's name (the roster's `name` column, from Wikipedia), forms
           10-K / 10-K405 only; the EDGAR filer name's normalised tokens must contain the member name's.
  index    1996-2000 exits (before full-text search): the EDGAR quarterly master index (master.idx), 10-K
           filers whose normalised name contains the member name's, filed inside the span; one CIK or nothing.
  current  EDGAR's company_tickers.json for a symbol a renamed survivor still carries (ATGE: DeVry 2009-2012 is
           Adtalem today, one CIK since 1994), under the same in-span check.
  figi     (--figi) OpenFIGI's mapping API with includeUnlistedEquities, the one free source that names a DELISTED
           symbol (Bloomberg's symbology: "BDK US" -> BLACK & DECKER CORP); the name then goes through the name or
           index rule. Only 44 of the 361 unmapped pre-2014 exits carry a name in the roster, so this is what the
           1996-2000 era needs. OpenFIGI names the symbol's holder TODAY, so a CIK found through it must fill the
           span too (Bank One's ONE -> POWER ONE). Not reachable from every network.
  WRONG    (#1320) pairs checked wrong by hand that pass every guard; never written, whatever rule finds them.
  roster   a CIK the roster already carries ((#1310): EDGAR's ticker file, Q-symbols matched to their BRD case)
           is copied, never re-derived.

Validated by hand on 2026-11-03 before this was written (phrase rule): FLM -> FLEMING COMPANIES INC /OK/
0000352949 (10-K 2002-03-06), AWE -> AT&T WIRELESS SERVICES 0001138234, HPC -> HERCULES 0000046989, IMNX -> IMMUNEX
0000719529, PMCS -> PMC SIERRA 0000767920, CEN -> CERIDIAN 0001124887 (10-K405); TOS misses the phrase and the name
rule finds TOSCO CORP 0000074091, while "symbol TOS" alone returns PHILLIPS PETROLEUM's S-4 (the acquirer) - the
case the own-forms rule exists for.

WHY A COUNT FLOOR IS A HARD REFUSAL. A search answering a tenth of its queries writes a map that looks plausible
and prices a different question: under MIN_MAPPED new rows nothing is written.

Output `tests/ticker-cik.csv`: ticker,start_date,end_date,cik,edgar_name,source,confidence (A = phrase with an own
10-K in the span, B = name, C = master index, R = roster). Unmatched and ambiguous spans are printed, never guessed.
Readers: tools/dead_roster.py (before company_tickers.json), tools/item5_prices.py (in place of its hand Q map).

    python3 -I tools/ticker_cik_history.py --cache /tmp/edgar --out tests/ticker-cik.csv
    python3 -I tools/ticker_cik_history.py --roster tests/sp400-roster.csv --out tests/sp400-ticker-cik.csv
    python3 -I tools/ticker_cik_history.py --selftest

Stdlib only. Fetches go through tools/dead_roster.py's polite, cached getter (SEC: 10 req/s, a User-Agent).
"""
import argparse, csv, datetime, io, json, os, re, sys, unittest, urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dead_roster as dr  # noqa: E402

EFTS = "https://efts.sec.gov/LATEST/search-index"
SUBMISSIONS = "https://data.sec.gov/submissions/CIK{}.json"
MASTER = "https://www.sec.gov/Archives/edgar/full-index/{}/QTR{}/master.idx"  # pipe-delimited: CIK|Company Name|Form Type|Date Filed|Filename (company.idx is fixed-width)
# the filer's own PERIODIC reports only: an 8-K is often another filer's (a merger partner's 8-K quotes the target's
# symbol: "under the symbol SOTR" returned R&G FINANCIAL and COMMUNITY BANKSHARES 8-Ks beside SouthTrust's), and a
# fund's N-PX prints every holding's ticker. 20-F/40-F for the foreign private issuers the index held (UN, RD).
OWN_FORMS = ("10-K", "10-K405", "10-KT", "10-K/A", "10-K405/A", "10-Q", "10-Q/A", "20-F", "20-F/A", "40-F")
COMPANY_TICKERS = "https://www.sec.gov/files/company_tickers.json"
OPENFIGI = "https://api.openfigi.com/v3/mapping"  # free, keyless at 25 requests a minute, 10 jobs a request
ANNUAL = ("10-K", "10-K405", "10-KT", "10-K/A", "10-K405/A")
# (#1320) "NYSE: <TK>" / "Nasdaq: <TK>" are dropped: the first live run took 33 spans through them and every one was
# another filer quoting a partner's symbol (MERISEL's 10-K "Nasdaq: ACAS", NRG's "NYSE: CEG"); 0 were right.
PHRASES = ('"under the symbol {t}"', '"ticker symbol {t}"', '"symbol {t}"')
EDGAR_FULL = datetime.date(1996, 5, 31)  # every domestic filer was on EDGAR by May 1996 (the phase-in's end)
# (#1320) wrong pairs the span check cannot see, checked by hand: AT&T CORP's 10-K quotes its AT&T Wireless tracking
# stock (AWE is AT&T WIRELESS SERVICES); CCB is Capital Cities/ABC, not COASTAL FINANCIAL; TA is Transamerica, not
# the TRAVELCENTERS OF AMERICA OpenFIGI names (today's holder of the symbol).
WRONG = {("AWE", "0000005907"), ("CCB", "0000935930"), ("TA", "0001038523")}
FTS_FROM = datetime.date(2001, 1, 1)  # full-text search indexes 2001 on
MIN_MAPPED = 20  # a run that resolves fewer new spans writes nothing (a broken search, not a thin answer)
HEADER = ["ticker", "start_date", "end_date", "cik", "edgar_name", "source", "confidence"]
DISPLAY = re.compile(r"^(.*?)\s+(?:\(([^()]*)\)\s+)?\(CIK (\d{10})\)$")
LATER_HOLDER_DAYS = 548  # (#1310) a CIK whose first filing is more than 18 months after the exit held the symbol later
WINDOW_BEFORE = datetime.timedelta(days=365)
WINDOW_AFTER = datetime.timedelta(days=365)


def day(s):
    return datetime.date.fromisoformat(str(s)[:10])


def parse_display(s):
    """'FLEMING COMPANIES INC /OK/  (CIK 0000352949)' -> (cik, ticker or '', name); None when it is not that shape."""
    m = DISPLAY.match(s.strip())
    if not m:
        return None
    name, ticks, cik = m.groups()
    return cik, (ticks or "").split(",")[0].strip(), " ".join(name.split())


def fts_url(q, forms, lo, hi, start=0):
    params = {"q": q, "dateRange": "custom", "startdt": lo.isoformat(), "enddt": hi.isoformat()}
    if forms:
        params["forms"] = ",".join(forms)
    if start:
        params["from"] = start
    return EFTS + "?" + urllib.parse.urlencode(params)


def fts_hits(q, forms, lo, hi, fetch):
    """Every `_source` of a full-text query, paged by 100; `fetch(url)` returns the parsed JSON or None."""
    out = []
    while True:
        body = fetch(fts_url(q, forms, lo, hi, len(out)))
        hits = ((body or {}).get("hits") or {}).get("hits") or []
        out += [h.get("_source", {}) for h in hits]
        total = (((body or {}).get("hits") or {}).get("total") or {}).get("value", 0)
        if not hits or len(out) >= total:
            return out


def own_filers(hits):
    """{cik: (name, n hits)} over the hits whose form is the filer's own and that carry exactly one CIK."""
    out = {}
    for h in hits:
        form = (h.get("form") or "").strip()
        if form not in OWN_FORMS:
            continue
        names = [p for p in map(parse_display, h.get("display_names") or []) if p]
        ciks = h.get("ciks") or [p[0] for p in names]
        if len(set(ciks)) != 1:
            continue
        cik = str(ciks[0]).zfill(10)
        name = next((p[2] for p in names if p[0] == cik), names[0][2] if names else "")
        n, _ = out.get(cik, (0, name))
        out[cik] = (n + 1, name or _)
    return out


def filings_of(cik, fetch):
    """[(form, filing date)] of a CIK from its submissions JSON and older pages; [] when EDGAR has none."""
    subs = fetch(SUBMISSIONS.format(cik))
    if not subs:
        return []
    pages = [subs.get("filings", {}).get("recent", {})]
    for f in subs.get("filings", {}).get("files", []):
        page = fetch("https://data.sec.gov/submissions/" + f["name"])
        if page:
            pages.append(page)
    out = []
    for p in pages:
        out += [(fm, day(d)) for fm, d in zip(p.get("form", []), p.get("filingDate", []))]
    return out


def in_span(filings, start, end):
    """Did this CIK file an annual report inside [start - 400 days, end + 548 days], and not first appear on EDGAR
    more than 18 months after the exit? (#1310)'s later-holder rule, read on the filings list."""
    if not filings:
        return False
    first = min(d for _, d in filings)
    if (first - end).days > LATER_HOLDER_DAYS:
        return False
    lo, hi = start - datetime.timedelta(days=400), end + datetime.timedelta(days=LATER_HOLDER_DAYS)
    return any(f.split("/")[0] in ("10-K", "10-K405", "10-KT") and lo <= d <= hi for f, d in filings)


def fills_span(filings, start, end):
    """(#1320) Did this CIK file from the span's opening (EDGAR from mid-1996) to within 18 months of its close? The
    phrase rule checks no name and the OpenFIGI rules borrow today's holder's, so those two also need this: EMC's
    symbol hit SCANDIUM INTERNATIONAL (first filing 2007), Bank One's ONE hit POWER ONE (1997), Harrah's HET hit Park
    Place's CAESARS ENTERTAINMENT INC (1998). A partial CIK (DIRECTV's 2009 holding company for a 2006 span) is lost too."""
    return alive_at_exit(filings, end) and (min(d for _, d in filings) - max(start, EDGAR_FULL)).days <= 365


def alive_at_exit(filings, end):
    """(#1320) Did this CIK file an annual report within 18 months before the exit? Even the member's own name can
    land on a predecessor that stopped years earlier: Express Scripts' 1992 CIK filed its last 10-K in 2011, while the
    2018 exit belongs to the 2012 holding company; the Starwood REIT trust stopped in 2006, the 2016 exit is the
    hotel corporation's. A series priced off the old CIK would end years before the member left the index."""
    annual = [d for f, d in filings if f.split("/")[0] in ANNUAL]
    return bool(annual) and (end - max(annual)).days <= LATER_HOLDER_DAYS


def contains_tokens(edgar_name, member_name):
    """Do the member name's normalised tokens all appear in the EDGAR filer name's (corporate suffixes and state
    qualifiers dropped on both sides)? 'Tosco' in 'TOSCO CORP'; 'Fleming Companies' in 'FLEMING COMPANIES INC /OK/'."""
    a, b = set(dr.norm_name(edgar_name).split()), set(dr.norm_name(member_name).split())
    return bool(b) and b <= a


def by_phrase(tk, start, end, fetch):
    """Rule `phrase`: (cik, name) when exactly one own-form filer says the symbol inside the window and its
    filings put it in the span; ('ambiguous', names) on two; None on none."""
    lo, hi = max(start - WINDOW_BEFORE, FTS_FROM), end + WINDOW_AFTER
    if hi < FTS_FROM:
        return None
    seen = {}
    for ph in PHRASES:
        for cik, (n, name) in own_filers(fts_hits(ph.format(t=tk), OWN_FORMS, lo, hi, fetch)).items():
            k, nm = seen.get(cik, (0, name))
            seen[cik] = (k + n, nm or name)
        if seen:
            break  # the first phrase with any own-form hit decides; the others only add noise words
    fit = {c: v for c, v in seen.items() if (tk, c) not in WRONG and in_span(f := filings_of(c, fetch), start, end) and fills_span(f, start, end)}
    if len(fit) == 1:
        (cik, (_, name)), = fit.items()
        return cik, name
    if len(fit) > 1:
        return "ambiguous", sorted(v[1] for v in fit.values())
    return None


def by_name(member_name, start, end, fetch):
    """Rule `name`: the annual-report filers whose EDGAR name contains the member name's tokens, inside the window
    and the span; one CIK or nothing."""
    if not member_name:
        return None
    lo, hi = max(start - WINDOW_BEFORE, FTS_FROM), end + WINDOW_AFTER
    if hi < FTS_FROM:
        return None
    hits = fts_hits('"%s"' % member_name.replace('"', ""), ANNUAL, lo, hi, fetch)
    fit = {}
    for cik, (n, name) in own_filers(hits).items():
        if contains_tokens(name, member_name) and in_span(filings_of(cik, fetch), start, end):
            fit[cik] = name
    if len(fit) == 1:
        (cik, name), = fit.items()
        return cik, name
    if len(fit) > 1:
        return "ambiguous", sorted(fit.values())
    return None


def parse_master(text):
    """EDGAR master.idx (`CIK|Company Name|Form Type|Date Filed|Filename`) -> [(name, form, cik, filed)]; the header
    ends at the dashed line."""
    out = []
    body = False
    for line in text.splitlines():
        if line.startswith("----"):
            body = True
            continue
        if not body:
            continue
        parts = [p.strip() for p in line.split("|")]
        if len(parts) >= 4 and parts[0].isdigit():
            try:
                out.append((parts[1], parts[2], parts[0].zfill(10), day(parts[3])))
            except ValueError:
                continue
    return out


def quarters(lo, hi):
    y, q = lo.year, (lo.month - 1) // 3 + 1
    while datetime.date(y, 3 * q - 2, 1) <= hi:
        yield y, q
        q += 1
        if q == 5:
            y, q = y + 1, 1


def by_index(member_name, start, end, fetch_text):
    """Rule `index`: 10-K filers in the master index inside the span whose name contains the member name's tokens;
    one CIK or nothing. `fetch_text(url)` returns the index text."""
    if not member_name:
        return None
    fit = {}
    lo, hi = max(start, datetime.date(1994, 1, 1)), end + datetime.timedelta(days=LATER_HOLDER_DAYS)
    for y, q in quarters(lo, hi):
        text = fetch_text(MASTER.format(y, q))
        if not text:
            continue
        for name, form, cik, filed in parse_master(text):
            if form.split("/")[0] in ("10-K", "10-K405", "10-KT") and lo <= filed <= hi and contains_tokens(name, member_name):
                fit[cik] = name
    if len(fit) == 1:
        (cik, name), = fit.items()
        return cik, name
    if len(fit) > 1:
        return "ambiguous", sorted(fit.values())
    return None


def by_current(tk, start, end, current, fetch):
    """Rule `current`: EDGAR's ticker file knows the symbol today and that CIK filed inside the span (a renamed
    survivor), or None. `current` = {ticker: (cik, title)}."""
    hit = current.get(tk)
    if not hit:
        return None
    cik, title = hit
    return (cik, title) if in_span(filings_of(cik, fetch), start, end) else None


def figi_names(tickers, post):
    """OpenFIGI names for symbols, delisted included: {ticker: [name, ...]}. `post(url, payload)` returns the parsed
    JSON list, one entry per job. Ten jobs a request, as the keyless limit allows."""
    out = {}
    tickers = list(tickers)
    for i in range(0, len(tickers), 10):
        batch = tickers[i:i + 10]
        jobs = [{"idType": "TICKER", "idValue": t, "exchCode": "US", "includeUnlistedEquities": True} for t in batch]
        res = post(OPENFIGI, jobs) or []
        for t, r in zip(batch, res):
            names = sorted({d["name"] for d in (r or {}).get("data", []) if d.get("name") and d.get("marketSector") == "Equity"})
            if names:
                out[t] = names
    return out


def resolve(row, fetch, fetch_text, hand=None, current=None, figi=None):
    """One roster span -> (cik, edgar_name, source, confidence) or ('', reason, 'none', '').
    `current` = EDGAR's ticker file as {ticker: (cik, title)}; `figi` = {ticker: [names]} from OpenFIGI."""
    tk, start, end = row["ticker"], day(row["start_date"]), day(row["end_date"])
    if row.get("cik"):
        return row["cik"].zfill(10), row.get("name", ""), "roster", "R"
    if hand and tk in hand:
        return hand[tk], row.get("name", ""), "qsymbol", "R"
    got = by_phrase(tk, start, end, fetch)
    if got and got[0] != "ambiguous":
        return got[0], got[1], "phrase", "A"
    ambiguous = got[1] if got else None
    names = [row.get("name", "")] if row.get("name") else []
    names += [n for n in (figi or {}).get(tk, []) if n not in names]
    # a name the roster carries is the member's own; an OpenFIGI one may be a later holder's, so its CIK must fill the
    # span; either way the CIK must still be filing at the exit
    def ok(got, nm):
        if not got or got[0] == "ambiguous" or (tk, got[0]) in WRONG:
            return False
        f = filings_of(got[0], fetch)
        return alive_at_exit(f, end) if nm == row.get("name") else fills_span(f, start, end)
    for nm in names:
        got = by_name(nm, start, end, fetch)
        if ok(got, nm):
            return got[0], got[1], "name" if nm == row.get("name") else "figi-name", "B"
        ambiguous = ambiguous or (got[1] if got and got[0] == "ambiguous" else None)
    got = by_current(tk, start, end, current or {}, fetch)
    if got:
        return got[0], got[1], "current", "B"
    if start < FTS_FROM + datetime.timedelta(days=365):
        for nm in names:
            got = by_index(nm, start, end, fetch_text)
            if ok(got, nm):
                return got[0], got[1], "index" if nm == row.get("name") else "figi-index", "C"
            ambiguous = ambiguous or (got[1] if got and got[0] == "ambiguous" else None)
    return "", ("ambiguous: " + " | ".join(ambiguous)) if ambiguous else ("no filer found" if names else "no filer found, and no name to search by"), "none", ""


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--roster", default="tests/dead-roster.csv", help="tools/dead_roster.py's output; one row per closed span")
    ap.add_argument("--cache", default=".ticker_cik_cache")
    ap.add_argument("--out", default="tests/ticker-cik.csv")
    ap.add_argument("--since", default="1996-01-01")
    ap.add_argument("--max", type=int, default=0, help="resolve at most this many unmapped spans this run (0 = all)")
    ap.add_argument("--figi", action="store_true", help="ask OpenFIGI (keyless, 25 requests a minute) for the names of unmapped symbols first")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        sys.argv = sys.argv[:1]
        unittest.main(module=__name__, verbosity=2)
        return
    with open(a.roster, encoding="utf-8") as fh:
        rows = [r for r in csv.DictReader(l for l in fh if not l.startswith("#")) if r["end_date"] >= a.since]
    try:
        import item5_prices  # the 26 Q-symbols matched to their BRD case by hand ((#1314)), until this file replaces them
        hand = dict(item5_prices.Q)
    except Exception:
        hand = {}
    fetch = lambda url: dr._get(url, a.cache)
    fetch_text = lambda url: (dr._get(url, a.cache, binary=True) or b"").decode("latin-1")
    current = {}
    for v in (fetch(COMPANY_TICKERS) or {}).values():
        current.setdefault(v["ticker"].upper().replace(".", "-"), (str(v["cik_str"]).zfill(10), v.get("title", "")))
    figi = {}
    if a.figi:
        unmapped = [r["ticker"] for r in rows if not r.get("cik") and r["ticker"] not in hand]
        figi = figi_names(unmapped, lambda url, jobs: _post_json(url, jobs, a.cache))
        print("OpenFIGI named %d of %d unmapped symbols" % (len(figi), len(unmapped)), file=sys.stderr)
    out, unmatched, done = [], [], 0
    for r in rows:
        if a.max and not r.get("cik") and r["ticker"] not in hand and done >= a.max:
            continue
        cik, name, source, conf = resolve(r, fetch, fetch_text, hand, current, figi)
        if source not in ("roster", "qsymbol"):
            done += 1
        if cik:
            out.append({"ticker": r["ticker"], "start_date": r["start_date"], "end_date": r["end_date"], "cik": cik,
                        "edgar_name": name.replace(",", " "), "source": source, "confidence": conf})
        else:
            unmatched.append((r["ticker"], r["end_date"], name))
    new = sum(o["source"] in ("phrase", "name", "index") for o in out)
    print("%d spans: %d mapped (%d new by phrase/name/index, %d from the roster), %d unmatched" % (
        len(rows), len(out), new, len(out) - new, len(unmatched)), file=sys.stderr)
    for tk, end, why in unmatched[:60]:
        print("  %s %s: %s" % (tk, end, why), file=sys.stderr)
    if new < MIN_MAPPED and not a.max:
        raise SystemExit("only %d new spans mapped, under the %d floor: nothing written" % (new, MIN_MAPPED))
    with open(a.out, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("# (#1319) the SEC CIK behind each exited index member's symbol: phrase = its own filings name the symbol (EDGAR\n")
        fh.write("# full-text search, 2001 on), name = the member name on its annual reports, index = the 1994-2001 master index,\n")
        fh.write("# roster/qsymbol = copied from tests/dead-roster.csv or the (#1314) BRD matches; written by tools/ticker_cik_history.py\n")
        fh.write(",".join(HEADER) + "\n")
        for o in sorted(out, key=lambda o: (o["ticker"], o["start_date"])):
            fh.write(",".join(str(o[h]) for h in HEADER) + "\n")


def _post_json(url, payload, cache_dir):
    """POST `payload` as JSON, cached by (url, payload) hash; paced to OpenFIGI's keyless 25 requests a minute."""
    import hashlib, time, urllib.request
    os.makedirs(cache_dir, exist_ok=True)
    body = json.dumps(payload, sort_keys=True).encode()
    path = os.path.join(cache_dir, hashlib.sha1(url.encode() + body).hexdigest() + ".post.json")
    if os.path.exists(path):
        with open(path) as fh:
            return json.load(fh)
    time.sleep(2.5)
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json", "User-Agent": dr.UA})
    with urllib.request.urlopen(req, timeout=60) as r:
        res = json.load(r)
    with open(path, "w") as fh:
        json.dump(res, fh)
    return res


# ── tests: the cases verified by hand on 2026-11-03, as a fake EDGAR ───────────────────────────────────────────


def hit(name, cik, form, date):
    return {"_source": {"ciks": [cik], "display_names": ["%s  (CIK %s)" % (name, cik)], "form": form, "file_date": date}}


def body(hits):
    return {"hits": {"total": {"value": len(hits)}, "hits": hits}}


def subs(forms_dates):
    return {"filings": {"recent": {"form": [f for f, _ in forms_dates], "filingDate": [d for _, d in forms_dates]}, "files": []}}


class FakeEdgar:
    """url -> JSON, keyed on the query's phrase or name and the CIK's submissions."""

    def __init__(self):
        self.fts, self.subs, self.idx, self.calls = {}, {}, {}, []

    def __call__(self, url):
        self.calls.append(url)
        if url.startswith(SUBMISSIONS[: SUBMISSIONS.index("{")]):
            return self.subs.get(re.search(r"CIK(\d{10})", url).group(1))
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
        key = q["q"][0]
        forms = tuple(q.get("forms", [""])[0].split(",")) if q.get("forms") else ()
        return self.fts.get((key, forms), body([]))

    def text(self, url):
        return self.idx.get(url, "")


class Tests(unittest.TestCase):
    def edgar(self):
        e = FakeEdgar()
        e.fts[('"under the symbol FLM"', OWN_FORMS)] = body([hit("FLEMING COMPANIES INC /OK/", "0000352949", "10-K", "2002-03-06")])
        e.subs["0000352949"] = subs([("10-K", "2002-03-06"), ("10-K", "2001-03-28"), ("10-K", "1997-03-20"), ("8-K", "2003-04-01")])
        # Tosco: the phrase hits the acquirer's S-4 only; the name query finds Tosco's own 10-K405
        e.fts[('"symbol TOS"', OWN_FORMS)] = body([hit("PHILLIPS PETROLEUM CO", "0000078214", "S-4", "2001-02-20")])
        e.fts[('"Tosco Corp"', ANNUAL)] = body([hit("TOSCO CORP", "0000074091", "10-K405", "2001-03-30"), hit("PHILLIPS PETROLEUM CO", "0000078214", "10-K", "2001-03-21")])
        e.subs["0000074091"] = subs([("10-K405", "2001-03-30"), ("10-K", "2000-03-29")])
        e.subs["0000078214"] = subs([("10-K", "2001-03-21"), ("10-K", "2000-03-20")])
        # a recycled symbol: the only phrase hit is a filer whose first filing is years after the exit
        e.fts[('"under the symbol GLD"', OWN_FORMS)] = body([hit("SPDR GOLD TRUST", "0001222333", "10-K", "2012-11-29")])
        e.subs["0001222333"] = subs([("10-K", "2012-11-29"), ("8-K", "2004-11-18")])
        # two own-form filers inside the span: ambiguous, nothing written
        e.fts[('"under the symbol ABC"', OWN_FORMS)] = body([hit("ABC CORP", "0000000001", "10-K", "2005-03-01"), hit("ABC HOLDINGS INC", "0000000002", "10-K", "2005-03-02")])
        e.subs["0000000001"] = subs([("10-K", "2005-03-01"), ("10-K", "2000-03-01")])
        e.subs["0000000002"] = subs([("10-K", "2005-03-02"), ("10-K", "2000-03-02")])
        # a symbol phrase in a filer that only reached EDGAR 11 years into the span (EMC -> SCANDIUM INTERNATIONAL)
        e.fts[('"symbol EMC"', OWN_FORMS)] = body([hit("SCANDIUM INTERNATIONAL MINING CORP", "0000000003", "10-K", "2007-03-30")])
        e.subs["0000000003"] = subs([("10-K", "2007-03-30"), ("10-K", "2016-03-30")])
        # a hand-checked wrong pair: AT&T CORP quotes its tracking stock AWE
        e.fts[('"symbol AWE"', OWN_FORMS)] = body([hit("AT&T CORP", "0000005907", "10-K", "2002-03-01")])
        e.subs["0000005907"] = subs([("10-K", "2002-03-01"), ("10-K", "2004-03-01"), ("10-K", "1997-03-01")])
        return e

    def test_phrase_rule_finds_fleming_through_its_own_10k(self):
        e = self.edgar()
        got = resolve({"ticker": "FLM", "start_date": "1996-01-02", "end_date": "2003-04-01", "cik": "", "name": "Fleming Companies"}, e, e.text)
        self.assertEqual(got, ("0000352949", "FLEMING COMPANIES INC /OK/", "phrase", "A"))

    def test_an_acquirers_s4_never_counts_and_the_name_rule_finds_the_target(self):
        e = self.edgar()
        got = resolve({"ticker": "TOS", "start_date": "1996-01-02", "end_date": "2001-09-17", "cik": "", "name": "Tosco Corp"}, e, e.text)
        self.assertEqual(got, ("0000074091", "TOSCO CORP", "name", "B"), "Phillips' S-4 and Phillips' own 10-K (no Tosco token) both fall away")

    def test_a_later_holder_of_the_symbol_is_refused(self):
        e = self.edgar()
        got = resolve({"ticker": "GLD", "start_date": "1996-01-02", "end_date": "1998-06-30", "cik": "", "name": ""}, e, e.text)
        self.assertEqual(got[2], "none", "the first EDGAR filing is 6 years after the exit: a later holder, %r" % (got,))

    def test_two_filers_are_ambiguous_and_nothing_is_written(self):
        e = self.edgar()
        got = resolve({"ticker": "ABC", "start_date": "2000-01-01", "end_date": "2005-06-01", "cik": "", "name": ""}, e, e.text)
        self.assertEqual(got[2], "none")
        self.assertTrue(got[1].startswith("ambiguous: ABC CORP | ABC HOLDINGS INC"))

    def test_a_filer_that_starts_filing_years_into_the_span_is_refused(self):
        e = self.edgar()
        got = resolve({"ticker": "EMC", "start_date": "1996-01-02", "end_date": "2016-09-07", "cik": "", "name": ""}, e, e.text)
        self.assertEqual(got[2], "none", "its 10-Ks sit inside the span but it never filed for 1996-2006: %r" % (got,))

    def test_a_hand_checked_wrong_pair_is_never_written(self):
        e = self.edgar()
        got = resolve({"ticker": "AWE", "start_date": "2001-07-09", "end_date": "2004-10-27", "cik": "", "name": ""}, e, e.text)
        self.assertEqual(got[2], "none", "AT&T CORP fills the span, yet AWE is AT&T Wireless: %r" % (got,))

    def test_a_predecessor_cik_that_stopped_years_before_the_exit_is_refused(self):
        e = self.edgar()
        e.fts[('"Express Scripts"', ANNUAL)] = body([hit("EXPRESS SCRIPTS INC", "0000885721", "10-K", "2011-02-22")])
        e.subs["0000885721"] = subs([("10-K", "2011-02-22"), ("10-K", "2004-03-01"), ("10-K", "1997-03-01")])
        got = resolve({"ticker": "ESRX", "start_date": "2003-09-26", "end_date": "2018-12-21", "cik": "", "name": "Express Scripts"}, e, e.text)
        self.assertEqual(got[2], "none", "the member's own name, but its last 10-K is 7 years before the exit: %r" % (got,))
        e.subs["0000885721"] = subs([("10-K", "2018-02-27"), ("10-K", "2011-02-22")])
        got = resolve({"ticker": "ESRX", "start_date": "2003-09-26", "end_date": "2018-12-21", "cik": "", "name": "Express Scripts"}, e, e.text)
        self.assertEqual(got[2], "name", "a 10-K ten months before the exit is still filing")

    def test_no_phrase_names_an_exchange(self):
        self.assertFalse([p for p in PHRASES if "NYSE" in p or "Nasdaq" in p], "a partner's 10-K quotes 'Nasdaq: ACAS'")

    def test_roster_and_hand_ciks_are_copied_not_searched(self):
        e = self.edgar()
        got = resolve({"ticker": "X", "start_date": "2000-01-01", "end_date": "2005-06-01", "cik": "0000000009", "name": "X"}, e, e.text)
        self.assertEqual(got, ("0000000009", "X", "roster", "R"))
        got = resolve({"ticker": "ENRNQ", "start_date": "1996-01-02", "end_date": "2001-11-30", "cik": "", "name": "Enron"}, e, e.text, hand={"ENRNQ": "0001024401"})
        self.assertEqual(got[:1] + got[2:], ("0001024401", "qsymbol", "R"))
        self.assertEqual(e.calls, [], "no EDGAR call for a known CIK")

    def test_master_index_rule_for_the_pre_2001_exits(self):
        e = self.edgar()
        idx = ("Description: Master Index of EDGAR Dissemination Feed\n\nCIK|Company Name|Form Type|Date Filed|Filename\n" + "-" * 60 + "\n"
               "3133|ALEXANDER & ALEXANDER SERVICES INC|10-K|1996-03-28|edgar/data/3133/0000003133-96-000005.txt\n"
               "315293|AON CORP|10-K|1996-03-27|edgar/data/315293/x.txt\n")
        e.idx[MASTER.format(1996, 1)] = idx
        e.subs["0000003133"] = subs([("10-K", "1996-03-28"), ("10-Q", "1996-11-12")])
        got = resolve({"ticker": "AAL", "start_date": "1996-01-02", "end_date": "1997-01-15", "cik": "", "name": "Alexander & Alexander Services"}, e, e.text)
        self.assertEqual(got, ("0000003133", "ALEXANDER & ALEXANDER SERVICES INC", "index", "C"))
        rows = parse_master(idx)
        self.assertEqual(rows[0][2], "0000003133")
        self.assertEqual(rows[1][3], datetime.date(1996, 3, 27))

    def test_other_filers_8ks_do_not_count(self):
        hits = [hit("SOUTHTRUST CORP", "0000092081", "10-K", "2002-03-07"), hit("R&G FINANCIAL CORP", "0001016933", "8-K", "2004-10-12"),
                hit("COMMUNITY BANKSHARES INC /VA/", "0000742279", "8-K", "2001-09-27"), hit("MP63 FUND INC", "0001071873", "N-PX", "2008-08-22")]
        self.assertEqual(list(own_filers([h["_source"] for h in hits])), ["0000092081"], "a merger partner's 8-K and a fund's N-PX quote the symbol too")

    def test_current_ticker_file_maps_a_renamed_survivor(self):
        e = self.edgar()
        e.subs["0000730464"] = subs([("10-K", "2012-08-23"), ("10-K", "1996-09-25")])
        current = {"ATGE": ("0000730464", "Adtalem Global Education Inc.")}
        got = resolve({"ticker": "ATGE", "start_date": "2009-06-09", "end_date": "2012-10-01", "cik": "", "name": ""}, e, e.text, current=current)
        self.assertEqual(got, ("0000730464", "Adtalem Global Education Inc.", "current", "B"))
        # the same file must not hand a 1990s exit to today's holder of a recycled symbol
        current = {"GLD": ("0001222333", "SPDR Gold Trust")}
        got = resolve({"ticker": "GLD", "start_date": "1996-01-02", "end_date": "1998-06-30", "cik": "", "name": ""}, e, e.text, current=current)
        self.assertEqual(got[2], "none")

    def test_figi_name_feeds_the_name_and_index_rules(self):
        e = self.edgar()
        e.fts[('"BLACK & DECKER CORP"', ANNUAL)] = body([hit("BLACK & DECKER CORP", "0000012355", "10-K", "2009-02-20")])
        e.subs["0000012355"] = subs([("10-K", "2009-02-20"), ("10-K", "1997-03-20")])
        got = resolve({"ticker": "BDK", "start_date": "1996-01-02", "end_date": "2010-03-15", "cik": "", "name": ""}, e, e.text, figi={"BDK": ["BLACK & DECKER CORP"]})
        self.assertEqual(got, ("0000012355", "BLACK & DECKER CORP", "figi-name", "B"))
        res = [{"data": [{"name": "BLACK & DECKER CORP", "marketSector": "Equity"}, {"name": "BDK OPT", "marketSector": "Equity Option"}]}, {"warning": "No identifier found."}]
        self.assertEqual(figi_names(["BDK", "ZZZ"], lambda url, jobs: res), {"BDK": ["BLACK & DECKER CORP"]})

    def test_name_tokens_and_window_helpers(self):
        self.assertTrue(contains_tokens("FLEMING COMPANIES INC /OK/", "Fleming Companies"))
        self.assertTrue(contains_tokens("TOSCO CORP", "Tosco Corp"))
        self.assertFalse(contains_tokens("PHILLIPS PETROLEUM CO", "Tosco Corp"))
        self.assertFalse(contains_tokens("ABC CORP", ""), "an empty member name matches nothing")
        f = [("10-K", datetime.date(2002, 3, 6)), ("8-K", datetime.date(2003, 4, 1))]
        self.assertTrue(in_span(f, datetime.date(1996, 1, 2), datetime.date(2003, 4, 1)))
        self.assertFalse(in_span([("10-K", datetime.date(2012, 11, 29))], datetime.date(1996, 1, 2), datetime.date(1998, 6, 30)), "first filing 14 years late")
        self.assertFalse(in_span([("8-K", datetime.date(1997, 1, 1))], datetime.date(1996, 1, 2), datetime.date(1998, 6, 30)), "no annual report in the span")
        self.assertEqual(list(quarters(datetime.date(2000, 11, 1), datetime.date(2001, 4, 1))), [(2000, 4), (2001, 1), (2001, 2)])
        self.assertIn("forms=10-K%2C10-K405", fts_url("x", ANNUAL[:2], datetime.date(2001, 1, 1), datetime.date(2001, 12, 31)))
        self.assertEqual(parse_display("SVB FINANCIAL GROUP  (SIVBQ)  (CIK 0000719739)"), ("0000719739", "SIVBQ", "SVB FINANCIAL GROUP"))

    def test_fts_paging_reads_every_page(self):
        pages = {0: body([hit("A", "0000000001", "10-K", "2005-01-01")] * 100), 100: body([hit("A", "0000000001", "10-K", "2005-01-01")] * 20)}
        for p in pages.values():
            p["hits"]["total"]["value"] = 120

        def fetch(url):
            q = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
            return pages[int(q.get("from", ["0"])[0])]

        self.assertEqual(len(fts_hits('"x"', ANNUAL, datetime.date(2005, 1, 1), datetime.date(2005, 12, 31), fetch)), 120)


if __name__ == "__main__":
    main()
