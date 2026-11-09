#!/usr/bin/env python3
"""(#1427) tests/event-dates.csv: the calendar the EVENTS census of `backtest N intraday` prices.

Rows are `ticker,date,kind`. Two kinds:
  earnings  one row per (ticker, filing date) of every 8-K whose items include 2.02 (Results of Operations and
            Financial Condition), read from EDGAR's submissions JSON for the CIK behind each S&P 500 member since
            2012 (the point-in-time membership CSV the backtest uses; CIKs from sec.gov company_tickers.json, then
            tests/ticker-cik.csv for exited symbols). A company that reports before the open moves in session D's
            day leg, one that reports after the close moves in the night leg into D+1 (a filing accepted after
            17:30 ET is dated the next business day, which the same window still holds): the census prices the
            close D-1 -> close D+1 window, with the session split printed beside it.
  restatement, unregistered_sale, departure  (#1432) one row per (ticker, filing date) of every 8-K whose items
            include 4.02 (Non-Reliance on Previously Issued Financial Statements: a restatement), 3.02 (Unregistered
            Sales of Equity Securities: dilution outside a registered offering) or 5.02 (Departure of Directors or
            Certain Officers), from the same submissions JSON; the census prices them at the one-year horizon too,
            as a harm cohort read on the pond, not a same-session trade.
  fomc      ticker `*` (the whole pond, one observation a date): the FOMC statement release dates 2013-2026,
            checked line by line against federalreserve.gov/monetarypolicy/fomccalendars.htm and
            fomchistorical2013..2020.htm on 2026-11-08 (statement day of each meeting; the unscheduled 2019-10-11,
            2020-03-03 and 2020-03-15 statements included; notation votes and no-statement calls left out).
            Hardcoded, so `--fomc-only` writes them with no network.

Not wired into the binary: run it by hand and commit the CSV; `backtest N intraday` reads it when present.
    python3 -I tools/event_dates.py --out tests/event-dates.csv --cache ~/.cache/folioman-events
    python3 -I tools/event_dates.py --fomc-only --out tests/event-dates.csv
    python3 -I tools/event_dates.py --tickers AAPL,MSFT --out /tmp/two.csv     # a quick look
    python3 -I tools/event_dates.py --selftest
Under `MIN_EARNINGS` earnings rows the tool refuses to write, as the other SEC tools do on a broken scan.
"""
import argparse, csv, datetime, io, os, sys, urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from failures_from_sec import TICKERS, UA, _get  # noqa: E402  one SEC identity for every tool

SUBMISSIONS = "https://data.sec.gov/submissions/CIK{cik}.json"
PAGE = "https://data.sec.gov/submissions/{name}"
MEMBERS = "https://raw.githubusercontent.com/fja05680/sp500/master/sp500_ticker_start_end.csv"
TICKER_CIK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "ticker-cik.csv")
START = datetime.date(2012, 1, 1)  # 8-K items are tagged in the submissions JSON from about 2005; the 10y bars start ~2016
HEADER = ["ticker", "date", "kind"]
# (#1427)/(#1432) the 8-K items each kind is read off; an 8-K/A and a form other than 8-K never count
ITEMS = {"2.02": "earnings", "4.02": "restatement", "3.02": "unregistered_sale", "5.02": "departure"}
MIN_EARNINGS = 8000  # a broken-scan guard: ~600 members x 4 a year x 10+ years is 24,000+; a third of that

# (date, how) — statement release dates; `scheduled-future` for a meeting not yet held on 2026-11-08
FOMC = [
    ("2013-01-30", "scheduled"), ("2013-03-20", "scheduled"), ("2013-05-01", "scheduled"), ("2013-06-19", "scheduled"),
    ("2013-07-31", "scheduled"), ("2013-09-18", "scheduled"), ("2013-10-30", "scheduled"), ("2013-12-18", "scheduled"),
    ("2014-01-29", "scheduled"), ("2014-03-19", "scheduled"), ("2014-04-30", "scheduled"), ("2014-06-18", "scheduled"),
    ("2014-07-30", "scheduled"), ("2014-09-17", "scheduled"), ("2014-10-29", "scheduled"), ("2014-12-17", "scheduled"),
    ("2015-01-28", "scheduled"), ("2015-03-18", "scheduled"), ("2015-04-29", "scheduled"), ("2015-06-17", "scheduled"),
    ("2015-07-29", "scheduled"), ("2015-09-17", "scheduled"), ("2015-10-28", "scheduled"), ("2015-12-16", "scheduled"),
    ("2016-01-27", "scheduled"), ("2016-03-16", "scheduled"), ("2016-04-27", "scheduled"), ("2016-06-15", "scheduled"),
    ("2016-07-27", "scheduled"), ("2016-09-21", "scheduled"), ("2016-11-02", "scheduled"), ("2016-12-14", "scheduled"),
    ("2017-02-01", "scheduled"), ("2017-03-15", "scheduled"), ("2017-05-03", "scheduled"), ("2017-06-14", "scheduled"),
    ("2017-07-26", "scheduled"), ("2017-09-20", "scheduled"), ("2017-11-01", "scheduled"), ("2017-12-13", "scheduled"),
    ("2018-01-31", "scheduled"), ("2018-03-21", "scheduled"), ("2018-05-02", "scheduled"), ("2018-06-13", "scheduled"),
    ("2018-08-01", "scheduled"), ("2018-09-26", "scheduled"), ("2018-11-08", "scheduled"), ("2018-12-19", "scheduled"),
    ("2019-01-30", "scheduled"), ("2019-03-20", "scheduled"), ("2019-05-01", "scheduled"), ("2019-06-19", "scheduled"),
    ("2019-07-31", "scheduled"), ("2019-09-18", "scheduled"), ("2019-10-11", "unscheduled"), ("2019-10-30", "scheduled"),
    ("2019-12-11", "scheduled"), ("2020-01-29", "scheduled"), ("2020-03-03", "unscheduled"), ("2020-03-15", "unscheduled"),
    ("2020-04-29", "scheduled"), ("2020-06-10", "scheduled"), ("2020-07-29", "scheduled"), ("2020-09-16", "scheduled"),
    ("2020-11-05", "scheduled"), ("2020-12-16", "scheduled"), ("2021-01-27", "scheduled"), ("2021-03-17", "scheduled"),
    ("2021-04-28", "scheduled"), ("2021-06-16", "scheduled"), ("2021-07-28", "scheduled"), ("2021-09-22", "scheduled"),
    ("2021-11-03", "scheduled"), ("2021-12-15", "scheduled"), ("2022-01-26", "scheduled"), ("2022-03-16", "scheduled"),
    ("2022-05-04", "scheduled"), ("2022-06-15", "scheduled"), ("2022-07-27", "scheduled"), ("2022-09-21", "scheduled"),
    ("2022-11-02", "scheduled"), ("2022-12-14", "scheduled"), ("2023-02-01", "scheduled"), ("2023-03-22", "scheduled"),
    ("2023-05-03", "scheduled"), ("2023-06-14", "scheduled"), ("2023-07-26", "scheduled"), ("2023-09-20", "scheduled"),
    ("2023-11-01", "scheduled"), ("2023-12-13", "scheduled"), ("2024-01-31", "scheduled"), ("2024-03-20", "scheduled"),
    ("2024-05-01", "scheduled"), ("2024-06-12", "scheduled"), ("2024-07-31", "scheduled"), ("2024-09-18", "scheduled"),
    ("2024-11-07", "scheduled"), ("2024-12-18", "scheduled"), ("2025-01-29", "scheduled"), ("2025-03-19", "scheduled"),
    ("2025-05-07", "scheduled"), ("2025-06-18", "scheduled"), ("2025-07-30", "scheduled"), ("2025-09-17", "scheduled"),
    ("2025-10-29", "scheduled"), ("2025-12-10", "scheduled"), ("2026-01-28", "scheduled"), ("2026-03-18", "scheduled"),
    ("2026-04-29", "scheduled"), ("2026-06-17", "scheduled"), ("2026-07-29", "scheduled"), ("2026-09-16", "scheduled"),
    ("2026-10-28", "scheduled"), ("2026-12-09", "scheduled-future"),
]


def result_dates(page, start, item="2.02"):
    """The filing dates of the 8-Ks on one submissions page whose items include `item`, on or after `start`.
    The first page keeps its arrays under filings.recent; the older pages carry them at the top level."""
    arrays = page.get("filings", {}).get("recent", page)
    out = set()
    for form, date, items in zip(arrays.get("form", []), arrays.get("filingDate", []), arrays.get("items", [])):
        if form == "8-K" and item in [x.strip() for x in (items or "").split(",")] and date >= start.isoformat():
            out.add(date)
    return out


def earnings(cik, cache_dir, start, item="2.02"):
    """Every `item` filing date for one CIK, across the first page and each older page that reaches `start`."""
    first = _get(SUBMISSIONS.format(cik=str(cik).zfill(10)), cache_dir)
    if not first:
        return set()
    dates = result_dates(first, start, item)
    for page in first.get("filings", {}).get("files", []):
        if page.get("filingTo", "9999-12-31") < start.isoformat():
            continue
        more = _get(PAGE.format(name=page["name"]), cache_dir)
        if more:
            dates |= result_dates(more, start, item)
    return dates


def all_kinds(cik, cache_dir, start):
    """{kind: dates} for every item in `ITEMS`, from the pages the cache already holds after the first item."""
    return {kind: sorted(earnings(cik, cache_dir, start, item)) for item, kind in ITEMS.items()}


def members(text, start):
    """The tickers in the point-in-time membership CSV (`ticker,start_date,end_date`) whose spell ends on or after
    `start` (an empty end is today). A dotted class ticker (BRK.B) is written as SEC prints it (BRK-B)."""
    out = set()
    for row in csv.DictReader(io.StringIO(text)):
        end = (row.get("end_date") or "").strip()
        if not end or end >= start.isoformat():
            out.add(row["ticker"].strip().upper().replace(".", "-"))
    return out


def ciks(tickers, sec_map, ticker_cik_text):
    """{ticker: cik} from SEC's live list first, then tests/ticker-cik.csv for an exited symbol (its latest row)."""
    out = {t: sec_map[t] for t in tickers if t in sec_map}
    rows = [r for r in csv.DictReader(l for l in io.StringIO(ticker_cik_text) if not l.startswith("#"))]
    for r in sorted(rows, key=lambda r: r.get("end_date") or "", reverse=True):  # the latest spell first
        t = r["ticker"].strip().upper().replace(".", "-")
        if t in tickers and t not in out and r.get("cik"):
            out[t] = r["cik"].strip()
    return out


def rows(by_ticker, today):
    """CSV rows sorted by kind, ticker, date: each ticker's dates per kind (a plain list is the earnings kind),
    then the pond's FOMC dates."""
    out = []
    for t, kinds in by_ticker.items():
        kinds = kinds if isinstance(kinds, dict) else {"earnings": kinds}
        out += [(t, d, kind) for kind, ds in kinds.items() for d in ds]
    out += [("*", d, "fomc") for d, _ in FOMC if d <= today.isoformat()]
    return sorted(set(out))


def csv_text(rows_, note):
    buf = io.StringIO()
    buf.write("# (#1427) the EVENTS calendar: %s; written by tools/event_dates.py\n" % note)
    w = csv.writer(buf, lineterminator="\n")
    w.writerow(HEADER)
    w.writerows(rows_)
    return buf.getvalue()


def selftest():
    start = datetime.date(2012, 1, 1)
    first = {"filings": {"recent": {"form": ["8-K", "8-K/A", "8-K", "8-K", "10-Q"], "filingDate": ["2024-01-25", "2024-01-26", "2011-10-18", "2024-04-25", "2024-05-01"],
                                    "items": ["2.02,9.01", "2.02", "2.02", "5.02,9.01", ""]}, "files": [{"name": "CIK0000000042-submissions-001.json", "filingTo": "2015-01-01"}, {"name": "old.json", "filingTo": "2011-12-31"}]}}
    assert result_dates(first, start) == {"2024-01-25"}, "only an original 8-K with item 2.02 on or after the start"
    assert result_dates(first, start, "5.02") == {"2024-04-25"} and result_dates(first, start, "4.02") == set(), "(#1432) other items"
    assert set(ITEMS.values()) == {"earnings", "restatement", "unregistered_sale", "departure"}
    page = {"form": ["8-K"], "filingDate": ["2014-07-22"], "items": ["2.02"]}
    assert result_dates(page, start) == {"2014-07-22"}, "older pages carry the arrays at the top level"
    assert members("ticker,start_date,end_date\nAAPL,1982-11-30,\nEK,1957-03-04,2010-01-01\nBRK.B,2010-02-16,\nX,1957-03-04,2014-07-02\n", start) == {"AAPL", "BRK-B", "X"}
    sec_map = {"AAPL": "320193", "BRK-B": "1067983"}
    tc = "# comment\nticker,start_date,end_date,cik,edgar_name,source,confidence\nX,1996-01-02,2014-07-02,0001163302,United States Steel,roster,R\nX,1957-01-01,1990-01-01,0000000001,old,roster,R\n"
    assert ciks({"AAPL", "BRK-B", "X", "NOPE"}, sec_map, tc) == {"AAPL": "320193", "BRK-B": "1067983", "X": "0001163302"}, "the latest exited row wins"
    today = datetime.date(2026, 11, 8)
    r = rows({"MSFT": {"2024-01-30"}, "AAPL": {"earnings": ["2024-02-01", "2024-01-25"], "departure": ["2024-03-01"]}}, today)
    assert ("AAPL", "2024-03-01", "departure") in r and ("MSFT", "2024-01-30", "earnings") in r, "(#1432) a kinds dict and a plain list both read"
    assert r[:3] == [("*", "2013-01-30", "fomc"), ("*", "2013-03-20", "fomc"), ("*", "2013-05-01", "fomc")]
    assert ("*", "2026-12-09", "fomc") not in r and ("*", "2026-10-28", "fomc") in r, "a future meeting waits"
    earn = [x for x in r if x[2] == "earnings"]
    assert earn == [("AAPL", "2024-01-25", "earnings"), ("AAPL", "2024-02-01", "earnings"), ("MSFT", "2024-01-30", "earnings")]
    assert r[0] == ("*", "2013-01-30", "fomc") and [x for x in r if x[2] == "departure"] == [("AAPL", "2024-03-01", "departure")]
    dates = [d for d, _ in FOMC]
    assert dates == sorted(dates) and len(set(dates)) == len(dates) == 114, len(dates)
    assert sum(how == "unscheduled" for _, how in FOMC) == 3 and all(d[:4] in map(str, range(2013, 2027)) for d in dates)
    for d, _ in FOMC:
        assert datetime.date.fromisoformat(d).weekday() < 5 or d == "2020-03-15", d  # the one Sunday statement
    text = csv_text(r, "selftest")
    assert text.splitlines()[1] == "ticker,date,kind" and text.splitlines()[2] == "*,2013-01-30,fomc"
    print("event_dates selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    ap = argparse.ArgumentParser()
    ap.add_argument("--selftest", action="store_true", help="run the offline checks and exit")
    ap.add_argument("--out", default="tests/event-dates.csv")
    ap.add_argument("--cache", default=os.path.expanduser("~/.cache/folioman-events"))
    ap.add_argument("--members", default=MEMBERS, help="the point-in-time membership CSV (a URL or a local path)")
    ap.add_argument("--ticker-cik", default=TICKER_CIK)
    ap.add_argument("--tickers", help="comma-separated symbols instead of the membership file (a quick look; no row floor)")
    ap.add_argument("--start", default=START.isoformat())
    ap.add_argument("--fomc-only", action="store_true", help="write the `*` rows alone, no network")
    a = ap.parse_args()
    start, today = datetime.date.fromisoformat(a.start), datetime.date.today()
    by_ticker = {}
    if not a.fomc_only:
        os.makedirs(a.cache, exist_ok=True)
        sec_map = {e["ticker"].upper(): str(e["cik_str"]) for e in _get(TICKERS, a.cache, keep=False).values()}
        if a.tickers:
            tickers = {t.strip().upper() for t in a.tickers.split(",")}
        elif a.members.startswith("http"):
            req = urllib.request.Request(a.members, headers={"User-Agent": UA})
            tickers = members(urllib.request.urlopen(req, timeout=90).read().decode(), start)
        else:
            with open(a.members) as fh:
                tickers = members(fh.read(), start)
        with open(a.ticker_cik) as fh:
            mapping = ciks(tickers, sec_map, fh.read())
        print("pond: %d tickers, %d with a CIK, %d without (%s)" % (len(tickers), len(mapping), len(tickers) - len(mapping), ",".join(sorted(tickers - set(mapping))[:12])))
        for n, (t, cik) in enumerate(sorted(mapping.items()), 1):
            by_ticker[t] = all_kinds(cik, a.cache, start)  # (#1432) every kind off the same cached pages
            if n % 50 == 0 or n == len(mapping):
                print("  %d/%d  %s %d earnings dates" % (n, len(mapping), t, len(by_ticker[t]["earnings"])), file=sys.stderr)
        n_rows = sum(len(k["earnings"]) for k in by_ticker.values())
        if not a.tickers and n_rows < MIN_EARNINGS:
            raise SystemExit("only %d earnings rows (floor %d): refusing to write" % (n_rows, MIN_EARNINGS))
    out = rows(by_ticker, today)
    counts = ", ".join("%d %s" % (sum(r[2] == k for r in out), k) for k in sorted({r[2] for r in out}))
    note = "%s rows on %d names from %s, as of %s" % (counts, len(by_ticker), start, today)
    with open(a.out, "w") as fh:
        fh.write(csv_text(out, note))
    print("wrote %s: %d rows (%s)" % (a.out, len(out), note))


if __name__ == "__main__":
    main()
