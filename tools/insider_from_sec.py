#!/usr/bin/env python3
"""Build tests/insider.csv: open-market insider buys and sells per US ticker per month, for `backtest ... insider`.

(#1247) The insider factor was never graded. The walk read each ticker's Form 4s from the submissions
`recent` block, capped at 40 filings a ticker and 600 requests a run, which reaches back one to three
years. No 8y or 12y cutoff carried the factor, so the fund lane printed "only 0 cutoffs carry this
factor": that is a missing measurement, not a fail.

This tool reads SEC's quarterly Insider Transactions Data Sets instead (Form 3/4/5 since 2006 Q1, one
zip per quarter, public domain). It keeps Form 4 only, and only transaction codes P (open-market buy)
and S (open-market sale), from both the non-derivative and derivative tables, the same rows the live
Form 4 parser counts. Each transaction is one count, as in `core::insider_net_buys`.

DATED ON THE FILING MONTH, NOT THE TRADE. The live parser dates a transaction on its trade date, which
can be days or months before the market could read the filing. Here a row's date is the last day of the
month the Form 4 was FILED in, so a cutoff never sees a filing it could not have read. The cost is a
lag of up to one month.

Pond: the bare-ticker EQUITY series in the local .long_history_cache.json that SEC's ticker list maps
to a CIK, the same filter `failures_from_sec.py` uses. A ticker reused by another company reads the
current filer's history: a known ceiling, shared with every SEC row in the walk.

WHY A COUNT FLOOR IS A HARD REFUSAL. A scan that silently loses quarters writes a file that looks
plausible and grades a different question. Under `MIN_ROWS` rows or `MIN_TICKERS` tickers, or with a
quarter missing between the first and the last, nothing is written.

Not wired into the binary: run it by hand, commit the CSV it writes.

(#1301) `--holdings` writes tests/insider_own.csv instead: per US ticker, the shares its directors and
officers held, from the same zips' Form 3, 4 and 5 holdings (amendments skipped). A filer counts when
SEC marks them Director or Officer, so a founder who is also a 10 % owner counts and a fund that is only a
10 % owner does not. Each filing's holding is the shares owned after its last row per ownership line
(direct, or each indirect nature), common or ordinary shares only, summed. An insider's holding is their
latest filing's, dated on its filing month, and drops out 24 months after it. Only the months where the
total changes are written; a 0 row marks a total that fell to nothing.
shortcut: two insiders who both report one trust's shares count it twice, and a joint filing goes to its
lowest director or officer CIK. Both only inflate a level the backtest reads as a rank.
"""

import argparse, collections, csv, datetime, io, json, os, sys, time, urllib.error, urllib.request, zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from failures_from_sec import PACE, TICKERS, UA, _get  # noqa: E402  one SEC identity for every tool

BASES = ["https://www.sec.gov/files/structureddata/data/insider-transactions-data-sets/",
         "https://www.sec.gov/files/datastandardsinnovation/data/insider-transactions-data-sets/"]
FIRST = (2006, 1)  # the data sets start at 2006 Q1
HEADER = ["ticker", "month", "buys", "sells"]
MIN_ROWS = 34000  # a broken-scan guard: about half of the 2026-10-31 scan (68780 rows on 752 tickers)
MIN_TICKERS = 375
OWN_HEADER = ["ticker", "month", "shares"]
OWN_MONTHS = 24  # (#1301) an insider with no filing for this long has likely left the board
OWN_MIN_ROWS = 50000  # (#1301) a broken-scan guard, about half the first full scan
OWN_MIN_TICKERS = 380
MON = {m: i + 1 for i, m in enumerate("JAN FEB MAR APR MAY JUN JUL AUG SEP OCT NOV DEC".split())}

_last = [0.0]


def quarters(today):
    """Every (year, quarter) from 2006 Q1 through the quarter before `today`'s."""
    y, q = FIRST
    end = (today.year, (today.month - 1) // 3 + 1)
    while (y, q) < end:
        yield y, q
        y, q = (y, q + 1) if q < 4 else (y + 1, 1)


def download(y, q, cache_dir, tries=6):
    """The quarter's zip path, None when no base serves it (not published yet)."""
    path = os.path.join(cache_dir, "%dq%d_form345.zip" % (y, q))
    if os.path.exists(path):
        return path
    for base in BASES:
        url = base + "%dq%d_form345.zip" % (y, q)
        for attempt in range(tries):
            gap = PACE - (time.time() - _last[0])
            if gap > 0:
                time.sleep(gap)
            _last[0] = time.time()
            try:
                body = urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": UA}), timeout=300).read()
            except urllib.error.HTTPError as e:
                if e.code == 404:
                    break  # try the next base
                if e.code in (403, 429) or e.code >= 500:
                    wait = min(600, 10 * 2 ** attempt)
                    print("  sec %d, backing off %ds" % (e.code, wait), file=sys.stderr)
                    time.sleep(wait)
                    continue
                raise
            except Exception as e:  # transient DNS/TLS/timeout
                print("  sec error %s, retrying" % e, file=sys.stderr)
                time.sleep(5)
                continue
            with open(path + ".part", "wb") as fh:
                fh.write(body)
            os.replace(path + ".part", path)
            return path
    return None


def month_of(day):
    """'15-FEB-2015' -> '2015-02'; None when malformed."""
    try:
        d, m, y = day.strip().split("-")
        if not (len(d) == 2 and d.isdigit() and len(y) == 4 and y.isdigit()):
            return None
        return "%s-%02d" % (y, MON[m.upper()])
    except (ValueError, KeyError):
        return None


def tsv(z, name):
    with z.open(name) as fh:
        yield from csv.DictReader(io.TextIOWrapper(fh, encoding="utf-8", errors="replace"), delimiter="\t", quoting=csv.QUOTE_NONE)


def count_quarter(z, ciks, counts):
    """Add one quarter's Form 4 P/S transactions to `counts[(cik, month)] = [buys, sells]`.
    Returns the number of transactions added."""
    filed = {}
    for r in tsv(z, "SUBMISSION.tsv"):
        cik = r["ISSUERCIK"].strip().lstrip("0")
        if r["DOCUMENT_TYPE"].strip() == "4" and cik in ciks:
            m = month_of(r["FILING_DATE"])
            if m:
                filed[r["ACCESSION_NUMBER"]] = (cik, m)
    added = 0
    for table in ("NONDERIV_TRANS.tsv", "DERIV_TRANS.tsv"):
        for r in tsv(z, table):
            code = r["TRANS_CODE"].strip()
            key = filed.get(r["ACCESSION_NUMBER"])
            if key and code in ("P", "S"):
                counts[key][0 if code == "P" else 1] += 1
                added += 1
    return added


def day_of(day):
    """'15-FEB-2015' -> (2015, 2, 15); None when malformed."""
    m = month_of(day)
    return None if m is None else (int(m[:4]), int(m[5:]), int(day.strip()[:2]))


def holdings_quarter(z, ciks, latest):
    """(#1301) Fold one quarter's Form 3/4/5 holdings into `latest[(cik, owner)][month] = (day, accession,
    shares)`, keeping each month's last filing. Returns the number of filings kept."""
    filed = {}
    for r in tsv(z, "SUBMISSION.tsv"):
        cik = r["ISSUERCIK"].strip().lstrip("0")
        day = day_of(r["FILING_DATE"])
        if r["DOCUMENT_TYPE"].strip() in ("3", "4", "5") and cik in ciks and day:
            filed[r["ACCESSION_NUMBER"]] = (cik, day)
    owner = {}
    for r in tsv(z, "REPORTINGOWNER.tsv"):
        rel = r["RPTOWNER_RELATIONSHIP"]
        acc = r["ACCESSION_NUMBER"]
        if acc in filed and ("Director" in rel or "Officer" in rel):
            o = r["RPTOWNERCIK"].strip().lstrip("0")
            owner[acc] = min(owner.get(acc, o), o, key=int)
    lines = {}  # (accession, ownership line) -> (rank, shares): a transaction row beats a holding row, then the later row
    for table, sk, kind in (("NONDERIV_TRANS.tsv", "NONDERIV_TRANS_SK", 0), ("NONDERIV_HOLDING.tsv", "NONDERIV_HOLDING_SK", 1)):
        for r in tsv(z, table):
            acc, title = r["ACCESSION_NUMBER"], r["SECURITY_TITLE"].lower()
            if acc not in owner or "preferred" in title or not ("common" in title or "ordinary" in title or "capital stock" in title):
                continue
            try:
                shares, k = float(r["SHRS_OWND_FOLWNG_TRANS"]), int(r[sk])
            except ValueError:
                continue
            line = (acc, r["DIRECT_INDIRECT_OWNERSHIP"].strip(), r["NATURE_OF_OWNERSHIP"].strip().lower())
            if line not in lines or (-kind, k) > lines[line][0]:
                lines[line] = ((-kind, k), shares)
    held = collections.defaultdict(float)
    for (acc, _, _), (_, shares) in lines.items():
        held[acc] += shares
    for acc, shares in held.items():
        (cik, day), o = filed[acc], owner[acc]
        month = "%04d-%02d" % day[:2]
        slot = latest[(cik, o)]
        if month not in slot or (day, acc) > slot[month][:2]:
            slot[month] = (day, acc, shares)
    return len(held)


def own_series(latest, end):
    """(#1301) {cik: [(month, total shares)]} change points through month `end` ('YYYY-MM'): each owner
    holds their latest filing's shares for `OWN_MONTHS` months, then nothing."""
    idx = lambda m: int(m[:4]) * 12 + int(m[5:]) - 1
    per = collections.defaultdict(list)
    for (cik, _), slot in latest.items():
        per[cik].append(sorted((idx(m), v[2]) for m, v in slot.items()))
    out, stop = {}, idx(end)
    for cik, owners in per.items():
        first = min(o[0][0] for o in owners)
        pos, rows, prev = [0] * len(owners), [], 0
        for m in range(first, stop + 1):
            total = 0.0
            for j, o in enumerate(owners):
                while pos[j] + 1 < len(o) and o[pos[j] + 1][0] <= m:
                    pos[j] += 1
                at, shares = o[pos[j]]
                if at <= m < at + OWN_MONTHS:
                    total += shares
            total = round(total)
            if total != prev:
                rows.append(("%04d-%02d" % (m // 12, m % 12 + 1), total))
                prev = total
        out[cik] = rows
    return out


def own_rows(series, by_cik):
    """CSV rows for `own_series`, one per (ticker, month), sorted."""
    out = [{"ticker": t, "month": m, "shares": s} for cik, rows in series.items() for m, s in rows for t in by_cik.get(cik, [])]
    out.sort(key=lambda r: (r["ticker"], r["month"]))
    return out


def pond(history, tickers):
    """{cik: [ticker, ...]} for the bare-ticker EQUITY series in the long price cache that SEC maps.
    Streamed entry by entry: the cache is too big to hold parsed whole."""
    with open(history, "rb") as fh:
        txt = fh.read().decode("utf-8", "replace")
    dec = json.JSONDecoder()
    i, n, out = txt.index("{") + 1, len(txt), collections.defaultdict(list)
    while i < n:
        while txt[i] in ", \n\r\t":
            i += 1
        if txt[i] == "}":
            break
        key, i = dec.raw_decode(txt, i)
        while txt[i] in ": \n\r\t":
            i += 1
        val, i = dec.raw_decode(txt, i)
        cik = tickers.get(key.upper())
        if "." in key or not cik:
            continue
        try:
            if val[1]["chart"]["result"][0]["meta"].get("instrumentType") != "EQUITY":
                continue
        except (KeyError, IndexError, TypeError):
            continue
        out[cik].append(key.upper())
    return out


def rows(counts, by_cik):
    """CSV rows, one per (ticker, month), sorted. A CIK with two pond tickers writes both."""
    out = []
    for (cik, month), (b, s) in counts.items():
        for t in by_cik.get(cik, []):
            out.append({"ticker": t, "month": month, "buys": b, "sells": s})
    out.sort(key=lambda r: (r["ticker"], r["month"]))
    return out


def selftest():
    assert month_of("15-FEB-2015") == "2015-02" and month_of("01-dec-2006") == "2006-12"
    assert month_of("2015-02-15") is None and month_of("") is None and month_of("15-XYZ-2015") is None
    assert list(quarters(datetime.date(2006, 7, 1))) == [(2006, 1), (2006, 2)]
    assert list(quarters(datetime.date(2006, 3, 31))) == []
    assert list(quarters(datetime.date(2007, 2, 1)))[-1] == (2006, 4)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("SUBMISSION.tsv", "ACCESSION_NUMBER\tFILING_DATE\tDOCUMENT_TYPE\tISSUERCIK\n"
                   "a1\t03-FEB-2015\t4\t0000000042\na2\t28-FEB-2015\t4\t42\na3\t02-MAR-2015\t4/A\t42\n"
                   "a4\t02-MAR-2015\t4\t7\na5\t05-MAR-2015\t4\t42\n")
        z.writestr("NONDERIV_TRANS.tsv", "ACCESSION_NUMBER\tTRANS_CODE\n"
                   "a1\tP\na1\tS\na2\tP\na3\tP\na4\tP\na5\tS\na5\tM\na5\tA\n")
        z.writestr("DERIV_TRANS.tsv", "ACCESSION_NUMBER\tTRANS_CODE\na5\tS\na1\tM\n")
    counts = collections.defaultdict(lambda: [0, 0])
    # a3 is an amendment and a4 is outside the pond; M and A are not open-market trades
    assert count_quarter(zipfile.ZipFile(buf), {"42"}, counts) == 5
    assert dict(counts) == {("42", "2015-02"): [2, 1], ("42", "2015-03"): [0, 2]}
    got = rows(counts, {"42": ["GOOG", "GOOGL"]})
    assert [(r["ticker"], r["month"], r["buys"], r["sells"]) for r in got] == [
        ("GOOG", "2015-02", 2, 1), ("GOOG", "2015-03", 0, 2), ("GOOGL", "2015-02", 2, 1), ("GOOGL", "2015-03", 0, 2)]
    # (#1301) holdings: h1's last D row (900) plus its trust line (50); h2 replaces h1 later the same month;
    # h3 is an amendment, h4 a pure 10 % owner, h6 outside the pond; h5 is a joint filing keyed on CIK 150
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("SUBMISSION.tsv", "ACCESSION_NUMBER\tFILING_DATE\tDOCUMENT_TYPE\tISSUERCIK\n"
                   "h1\t03-FEB-2015\t4\t42\nh2\t20-FEB-2015\t4\t42\nh3\t10-MAR-2015\t4/A\t42\n"
                   "h4\t10-MAR-2015\t3\t42\nh5\t15-MAR-2015\t5\t42\nh6\t15-MAR-2015\t4\t7\n")
        z.writestr("REPORTINGOWNER.tsv", "ACCESSION_NUMBER\tRPTOWNERCIK\tRPTOWNER_RELATIONSHIP\n"
                   "h1\t100\tDirector\nh2\t100\tDirector\nh3\t100\tDirector\nh4\t300\tTenPercentOwner\n"
                   "h5\t200\tOfficer\nh5\t150\tDirector,TenPercentOwner\nh6\t100\tDirector\n")
        cols = "ACCESSION_NUMBER\t%s\tSECURITY_TITLE\tSHRS_OWND_FOLWNG_TRANS\tDIRECT_INDIRECT_OWNERSHIP\tNATURE_OF_OWNERSHIP\n"
        z.writestr("NONDERIV_TRANS.tsv", cols % "NONDERIV_TRANS_SK" +
                   "h1\t2\tCommon Stock\t900\tD\t\nh1\t1\tCommon Stock\t1000\tD\t\nh1\t3\tSeries A Preferred Common\t999\tD\t\n"
                   "h2\t4\tCommon Stock\t800\tD\t\nh3\t5\tCommon Stock\t5\tD\t\nh4\t6\tCommon Stock\t7000\tD\t\n"
                   "h5\t7\tClass A Common Stock\t500\tD\t\nh6\t8\tCommon Stock\t9\tD\t\n")
        z.writestr("NONDERIV_HOLDING.tsv", cols % "NONDERIV_HOLDING_SK" +
                   "h1\t9\tCommon Stock\t50\tI\tBy Trust\nh1\t1\tCommon Stock\t70\tD\t\nh5\t10\tCommon Stock\t\tI\tBy LLC\n")
    latest = collections.defaultdict(dict)
    assert holdings_quarter(zipfile.ZipFile(buf), {"42"}, latest) == 3
    assert {k: {m: v[1:] for m, v in s.items()} for k, s in latest.items()} == {
        ("42", "100"): {"2015-02": ("h2", 800.0)}, ("42", "150"): {"2015-03": ("h5", 500.0)}}, latest
    first = collections.defaultdict(dict)
    with zipfile.ZipFile(buf) as z:  # h1 alone: the last transaction row plus the trust line, not the holding D row
        holdings_quarter(z, {"42"}, first)
    assert first[("42", "100")]["2015-02"][1] == "h2"
    # each owner holds for 24 months: 100 drops at 2017-02, 150 at 2017-03; a 0 filing ends a holding early
    assert own_series(latest, "2017-04") == {"42": [("2015-02", 800), ("2015-03", 1300), ("2017-02", 500), ("2017-03", 0)]}
    latest[("42", "100")]["2016-01"] = ((2016, 1, 5), "h7", 0.0)
    assert own_series(latest, "2016-02")["42"] == [("2015-02", 800), ("2015-03", 1300), ("2016-01", 500)]
    assert [(r["ticker"], r["month"], r["shares"]) for r in own_rows({"42": [("2015-02", 800)]}, {"42": ["A", "B"]})] == [
        ("A", "2015-02", 800), ("B", "2015-02", 800)]
    print("selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    ap = argparse.ArgumentParser()
    ap.add_argument("--selftest", action="store_true", help="run the offline checks and exit")
    ap.add_argument("--out", default="tests/insider.csv")
    ap.add_argument("--holdings", action="store_true", help="(#1301) write the insider holdings CSV instead")
    ap.add_argument("--own-out", default="tests/insider_own.csv")
    ap.add_argument("--history", default=".long_history_cache.json", help="the backtest's long price cache")
    ap.add_argument("--cache", default=os.path.expanduser("~/.cache/folioman-insider"))
    args = ap.parse_args()

    os.makedirs(args.cache, exist_ok=True)
    tickers = {e["ticker"].upper(): str(e["cik_str"]) for e in _get(TICKERS, args.cache, keep=False).values()}
    by_cik = pond(args.history, tickers)
    ciks = set(by_cik)
    print("pond: %d tickers on %d CIKs" % (sum(map(len, by_cik.values())), len(ciks)))
    counts = collections.defaultdict(lambda: [0, 0])
    latest = collections.defaultdict(dict)
    got = []
    for y, q in quarters(datetime.date.today()):
        path = download(y, q, args.cache)
        if path is None:
            print("  %dq%d not published" % (y, q))
            continue
        with zipfile.ZipFile(path) as z:
            if args.holdings:
                print("  %dq%d  %6d holdings" % (y, q, holdings_quarter(z, ciks, latest)))
            else:
                print("  %dq%d  %6d transactions" % (y, q, count_quarter(z, ciks, counts)))
        got.append((y, q))
    want = list(quarters(datetime.date.today()))
    if not got or want[: want.index(got[-1]) + 1] != got:
        raise SystemExit("a quarter is missing between %s and %s: refusing to write" % (want[0], got[-1] if got else None))
    if args.holdings:
        out = own_rows(own_series(latest, "%04d-%02d" % (got[-1][0], got[-1][1] * 3)), by_cik)
        n_tk = len({r["ticker"] for r in out})
        if len(out) < OWN_MIN_ROWS or n_tk < OWN_MIN_TICKERS:
            raise SystemExit("only %d rows on %d tickers (floors %d, %d): refusing to write" % (len(out), n_tk, OWN_MIN_ROWS, OWN_MIN_TICKERS))
        with open(args.own_out, "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=OWN_HEADER, lineterminator="\n")
            w.writeheader()
            w.writerows(out)
        print("%s: %d rows on %d tickers, %d quarters" % (args.own_out, len(out), n_tk, len(got)))
        return
    out = rows(counts, by_cik)
    n_tk = len({r["ticker"] for r in out})
    if len(out) < MIN_ROWS or n_tk < MIN_TICKERS:
        raise SystemExit("only %d rows on %d tickers (floors %d, %d): refusing to write" % (len(out), n_tk, MIN_ROWS, MIN_TICKERS))
    assert all("," not in str(v) for r in out for v in r.values()), "a field holds a comma"
    with open(args.out, "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=HEADER, lineterminator="\n")
        w.writeheader()
        w.writerows(out)
    print("%s: %d rows on %d tickers, %d quarters (%dq%d to %dq%d)" % (args.out, len(out), n_tk, len(got), *got[0], *got[-1]))
    for y in sorted({r["month"][:4] for r in out}):
        ys = [r for r in out if r["month"][:4] == y]
        print("   %s  %4d tickers  %6d buys  %7d sells" % (y, len({r["ticker"] for r in ys}), sum(r["buys"] for r in ys), sum(r["sells"] for r in ys)))


if __name__ == "__main__":
    main()
