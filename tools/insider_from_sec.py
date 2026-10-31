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


def pond(history, tickers):
    """{cik: [ticker, ...]} for the bare-ticker EQUITY series in the long price cache that SEC maps.
    Streamed entry by entry: the cache is too big to hold parsed whole."""
    txt = open(history, "rb").read().decode("utf-8", "replace")
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
    print("selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    ap = argparse.ArgumentParser()
    ap.add_argument("--selftest", action="store_true", help="run the offline checks and exit")
    ap.add_argument("--out", default="tests/insider.csv")
    ap.add_argument("--history", default=".long_history_cache.json", help="the backtest's long price cache")
    ap.add_argument("--cache", default=os.path.expanduser("~/.cache/folioman-insider"))
    args = ap.parse_args()

    os.makedirs(args.cache, exist_ok=True)
    tickers = {e["ticker"].upper(): str(e["cik_str"]) for e in _get(TICKERS, args.cache, keep=False).values()}
    by_cik = pond(args.history, tickers)
    ciks = set(by_cik)
    print("pond: %d tickers on %d CIKs" % (sum(map(len, by_cik.values())), len(ciks)))
    counts = collections.defaultdict(lambda: [0, 0])
    got = []
    for y, q in quarters(datetime.date.today()):
        path = download(y, q, args.cache)
        if path is None:
            print("  %dq%d not published" % (y, q))
            continue
        with zipfile.ZipFile(path) as z:
            print("  %dq%d  %6d transactions" % (y, q, count_quarter(z, ciks, counts)))
        got.append((y, q))
    want = list(quarters(datetime.date.today()))
    if not got or want[: want.index(got[-1]) + 1] != got:
        raise SystemExit("a quarter is missing between %s and %s: refusing to write" % (want[0], got[-1] if got else None))
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
