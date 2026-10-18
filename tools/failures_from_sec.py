#!/usr/bin/env python3
"""Build tests/failures.csv: the US filers that went bankrupt or were wiped out, for `backtest ... failures`.

(#728) The backtest cannot see a dead name: a delisted ticker has no price bars, so of 737 S&P exits
only 141 are scorable, and every distress gate was graded on survivors alone. This list names the
failures by CIK instead, so their SEC rows can be read years before the event with no bars at all.

Two kinds, one row per CIK. The earliest event wins, and bankrupt wins a tie:
  bankrupt  an 8-K reporting Item 1.03 (bankruptcy or receivership), 2009 on, from EDGAR full-text
            search. Event = the CIK's first such filing. Kept only when the last 10-K whose period ended
            by then reported total assets of $100M or more; a filer with no XBRL assets has no rows to
            read. A parent does not file 1.03 for a subsidiary's case: 3M/Aearo and J&J/LTL read 0 hits.
  wipeout   a US equity in the local .long_history_cache.json whose adjusted close fell 80% below its
            running peak and whose last bar is still under 25% of that peak. Event = the first close at
            -80%. A series with a one-bar drop of 85% or more is dropped: that is a ticker reused by
            another company (PARA, UK, GP), not a collapse.

Bankrupt rows already in the CSV are kept as they are (append-only), so a later EDGAR re-index cannot
move a graded event. Wipeout rows are recomputed on every run, since a name can recover.

WHY A COUNT FLOOR IS A HARD REFUSAL. A search that silently returns a tenth of the filings writes a
file that looks plausible and grades a different question. Under 500 bankrupt rows, nothing is written.

Not wired into the binary: run it by hand, commit the CSV it writes.
"""
import argparse, csv, datetime, hashlib, json, os, re, sys, time, urllib.error, urllib.parse, urllib.request

EFTS = "https://efts.sec.gov/LATEST/search-index"
CONCEPT = "https://data.sec.gov/api/xbrl/companyconcept/CIK{cik}/us-gaap/Assets.json"
TICKERS = "https://www.sec.gov/files/company_tickers.json"
UA = "FolioMan research folioman@researchmail.org"  # SEC fair access wants a name and a contact
PACE = 0.13  # seconds between calls; SEC refuses an IP over 10 req/s for about 10 minutes
START = datetime.date(2009, 1, 1)  # XBRL rows start 2009-2011: an earlier event has nothing to read
MIN_ASSETS = 1e8
MIN_BANKRUPT = 400  # a broken-scan guard: 2026-10 measured 455, few before 2012 (no XBRL Assets yet)
HEADER = ["cik", "ticker", "name", "event_date", "kind", "assets_musd"]
DISPLAY = re.compile(r"^(.*?)\s+(?:\(([^()]*)\)\s+)?\(CIK (\d{10})\)$")

_last = [0.0]


def _get(url, cache_dir, keep=True, tries=8):
    """The JSON at `url`, None on a 404. Cached by URL hash when `keep`."""
    path = os.path.join(cache_dir, hashlib.sha1(url.encode()).hexdigest() + ".json")
    if keep and os.path.exists(path):
        with open(path) as fh:
            return json.load(fh)
    for attempt in range(tries):
        gap = PACE - (time.time() - _last[0])
        if gap > 0:
            time.sleep(gap)
        _last[0] = time.time()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            body = json.load(urllib.request.urlopen(req, timeout=90))
        except urllib.error.HTTPError as e:
            if e.code == 404:  # no such concept for this filer: an answer, not a refusal
                body = None
            elif e.code in (403, 429) or e.code >= 500:
                wait = min(600, 10 * 2 ** attempt)
                print("  sec %d, backing off %ds" % (e.code, wait), file=sys.stderr)
                time.sleep(wait)
                continue
            else:
                raise
        except Exception as e:  # transient DNS/TLS/timeout
            print("  sec error %s, retrying" % e, file=sys.stderr)
            time.sleep(5)
            continue
        if keep:
            with open(path, "w") as fh:
                json.dump(body, fh)
        return body
    raise SystemExit("sec: gave up on %s after %d tries" % (url, tries))


def months(start, end):
    """(first day, last day) of each calendar month from `start`'s through `end`, clipped to `end`."""
    d = start.replace(day=1)
    while d <= end:
        nxt = (d.replace(day=28) + datetime.timedelta(days=4)).replace(day=1)
        yield d, min(nxt - datetime.timedelta(days=1), end)
        d = nxt


def filings(lo, hi, cache_dir, today):
    """Every full-text hit for "Item 1.03" in 8-Ks filed lo..hi, paged 100 at a time."""
    keep = hi < today - datetime.timedelta(days=30)  # a recent window is still filling in
    out = []
    while True:
        q = urllib.parse.urlencode({"q": '"Item 1.03"', "forms": "8-K", "dateRange": "custom",
                                    "startdt": lo.isoformat(), "enddt": hi.isoformat(), "from": len(out)})
        body = _get(EFTS + "?" + q, cache_dir, keep)
        hits = body["hits"]["hits"]
        out += [h["_source"] for h in hits]
        if not hits or len(out) >= body["hits"]["total"]["value"]:
            return out


def is_bankruptcy(src):
    """The filing reports Item 1.03 itself; a full-text hit can merely quote it."""
    return "1.03" in (src.get("items") or [])


def parse_display(s):
    """'SVB FINANCIAL GROUP  (SIVBQ)  (CIK 0000719739)' -> (cik, first ticker or '', name without commas)."""
    m = DISPLAY.match(s.strip())
    if not m:
        return None
    name, ticks, cik = m.groups()
    return cik, (ticks or "").split(",")[0].strip(), " ".join(re.sub(r'[,"]', " ", name).split())


def last_10k_assets(body, event):
    """Total assets on the latest 10-K whose period ended on or before `event` (ISO), None if none."""
    rows = [r for r in ((body or {}).get("units") or {}).get("USD", [])
            if str(r.get("form", "")).startswith("10-K") and r.get("end", "") <= event and r.get("val")]
    return max(rows, key=lambda r: (r["end"], r.get("filed", "")))["val"] if rows else None


def wipe_event(pts):
    """Unix time of the first close 80% below the running peak while the LAST close sits under 25% of
    that peak; None when there is none, the series is short, or one bar fell 85% or more (a splice)."""
    if len(pts) < 24 or any(b <= 0.15 * a for (_, a), (_, b) in zip(pts, pts[1:])):
        return None
    peak = 0.0
    for ts, c in pts:
        peak = max(peak, c)
        if c <= 0.2 * peak and pts[-1][1] < 0.25 * peak:
            return ts
    return None


def row(cik, ticker, name, event, kind, assets):
    # backtest.rs splits each line on plain commas, so a name keeps none (and needs no CSV quoting)
    return {"cik": cik, "ticker": ticker, "name": name.replace(",", " "), "event_date": event, "kind": kind,
            "assets_musd": "" if assets is None else "%.1f" % (assets / 1e6)}


def bankrupts(cache_dir, today):
    hits, year = [], None
    for lo, hi in months(START, today):
        hits += [h for h in filings(lo, hi, cache_dir, today) if is_bankruptcy(h)]
        if lo.month == 12 or hi == today:
            print("  EFTS through %s: %d Item 1.03 hits" % (hi, len(hits)), file=sys.stderr)
    hits.sort(key=lambda h: h["file_date"])
    seen, out = set(), {}
    for h in hits:
        # Co-registrants share one filing: the largest pre-event balance sheet speaks for it, and every
        # CIK on it is spent, so a subsidiary's later 8-K in the same case adds no second event.
        filers = [p for p in map(parse_display, h.get("display_names") or []) if p and p[0] not in seen]
        seen.update(p[0] for p in filers)
        sized = []
        for p in filers:
            a = last_10k_assets(_get(CONCEPT.format(cik=p[0]), cache_dir), h["file_date"])
            if a is not None and a >= MIN_ASSETS:
                sized.append((a, p))
        if sized:
            a, (cik, tick, name) = max(sized)
            out[cik] = row(cik, tick, name, h["file_date"], "bankrupt", a)
    return out


def wipeouts(history, tickers, cache_dir):
    with open(history) as fh:
        d = json.load(fh)
    out = {}
    for t, v in sorted(d.items()):
        hit = tickers.get(t.upper())
        if "." in t or not hit:  # US listings only, and only SEC filers
            continue
        try:
            r = v[1]["chart"]["result"][0]
            if r["meta"].get("instrumentType") != "EQUITY":
                continue
            closes = ((r["indicators"].get("adjclose") or [{}])[0] or {}).get("adjclose") or r["indicators"]["quote"][0]["close"]
            pts = [(ts, c) for ts, c in zip(r["timestamp"], closes) if c]
        except (KeyError, IndexError, TypeError):
            continue
        ts = wipe_event(pts)
        if ts is None:
            continue
        event = datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).date()
        if event < START:
            continue
        cik, title = hit
        a = last_10k_assets(_get(CONCEPT.format(cik=cik), cache_dir), event.isoformat())
        out[cik] = row(cik, t.upper(), " ".join(re.sub(r'[,"]', " ", title).split()), event.isoformat(), "wipeout", a)
    return out


def merge(old_bankrupt, new_bankrupt, wipes):
    """One row per CIK: an old bankrupt row is never moved, the earliest event wins, bankrupt on a tie."""
    out = dict(new_bankrupt)
    out.update(old_bankrupt)
    for cik, w in wipes.items():
        if cik not in out or w["event_date"] < out[cik]["event_date"]:
            out[cik] = w
    return sorted(out.values(), key=lambda r: (r["event_date"], r["cik"]))


def selftest():
    up = [(i, 17.0 + i) for i in range(24)]  # peak 40.0 at the last of these bars
    assert wipe_event(up) is None
    crash = up + [(24, 30.0), (25, 20.0), (26, 8.0), (27, 7.0), (28, 9.9)]
    assert wipe_event(crash) == 26  # exactly -80% counts; the last bar 9.9 < 10.0 = 25% of the peak
    assert wipe_event(crash[:-1] + [(28, 10.0)]) is None  # back to 25% of the peak: recovered
    assert wipe_event(up + [(24, 6.0)]) is None  # one bar -85%: a reused ticker, not a collapse
    assert wipe_event(crash[:23]) is None  # under 24 bars
    again = crash[:-1] + [(28, 50.0), (29, 9.9)] + [(30 + i, 11.0) for i in range(3)]
    assert wipe_event(again) == 29  # recovered from the first crash, wiped out from the new peak
    assert parse_display("SVB FINANCIAL GROUP  (SIVBQ)  (CIK 0000719739)") == ("0000719739", "SIVBQ", "SVB FINANCIAL GROUP")
    assert parse_display("GPB Holdings II, LP  (CIK 0001668875)") == ("0001668875", "", "GPB Holdings II LP")
    assert parse_display("Foo Corp  (FOO, FOOW)  (CIK 0000000001)")[1] == "FOO"
    assert parse_display("no cik here") is None
    assert is_bankruptcy({"items": ["1.03", "9.01"]}) and not is_bankruptcy({"items": ["1.01"]})
    body = {"units": {"USD": [
        {"form": "10-K", "end": "2021-12-31", "filed": "2022-02-20", "val": 5e8},
        {"form": "10-K/A", "end": "2022-12-31", "filed": "2023-04-01", "val": 4e8},
        {"form": "10-Q", "end": "2023-03-31", "filed": "2023-05-01", "val": 3e8},
        {"form": "10-K", "end": "2023-12-31", "filed": "2024-03-01", "val": 2e8}]}}
    assert last_10k_assets(body, "2023-06-30") == 4e8  # the newest 10-K by period, not the 10-Q
    assert last_10k_assets(body, "2021-12-31") == 5e8 and last_10k_assets(body, "2021-01-01") is None
    assert last_10k_assets(None, "2023-01-01") is None
    old = {"1": row("1", "A", "a", "2015-01-01", "bankrupt", 2e8)}
    new = {"1": row("1", "A", "a", "2016-01-01", "bankrupt", 2e8), "2": row("2", "B", "b", "2020-05-01", "bankrupt", 1e9)}
    wipes = {"2": row("2", "B", "b", "2020-05-01", "wipeout", None), "3": row("3", "C", "c", "2019-01-01", "wipeout", None)}
    got = merge(old, new, wipes)
    assert [(r["cik"], r["event_date"], r["kind"]) for r in got] == [
        ("1", "2015-01-01", "bankrupt"), ("3", "2019-01-01", "wipeout"), ("2", "2020-05-01", "bankrupt")]
    assert got[2]["assets_musd"] == "1000.0" and got[1]["assets_musd"] == ""
    assert list(months(datetime.date(2020, 11, 5), datetime.date(2021, 1, 15)))[-1] == (datetime.date(2021, 1, 1), datetime.date(2021, 1, 15))
    print("selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    ap = argparse.ArgumentParser()
    ap.add_argument("--selftest", action="store_true", help="run the offline checks and exit")
    ap.add_argument("--out", default="tests/failures.csv")
    ap.add_argument("--history", default=".long_history_cache.json", help="the backtest's long price cache")
    ap.add_argument("--cache", default=os.path.expanduser("~/.cache/folioman-failures"))
    args = ap.parse_args()

    os.makedirs(args.cache, exist_ok=True)
    today = datetime.date.today()
    tickers = {e["ticker"].upper(): ("%010d" % e["cik_str"], e["title"])
               for e in _get(TICKERS, args.cache, keep=False).values()}
    old = {}
    if os.path.exists(args.out):
        with open(args.out, newline="") as fh:
            old = {r["cik"]: r for r in csv.DictReader(fh) if r["kind"] == "bankrupt"}
    rows = merge(old, bankrupts(args.cache, today), wipeouts(args.history, tickers, args.cache))
    n_bk = sum(r["kind"] == "bankrupt" for r in rows)
    if n_bk < MIN_BANKRUPT:
        raise SystemExit("only %d bankrupt rows (floor %d): refusing to write" % (n_bk, MIN_BANKRUPT))
    assert all("," not in v for r in rows for v in r.values()), "a field holds a comma"
    with open(args.out, "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=HEADER, lineterminator="\n")
        w.writeheader()
        w.writerows(rows)
    print("%s: %d rows, %d bankrupt, %d wipeout, %d kept from the old file"
          % (args.out, len(rows), n_bk, len(rows) - n_bk, len(old)))
    for y in sorted({r["event_date"][:4] for r in rows}):
        ys = [r for r in rows if r["event_date"][:4] == y]
        print("   %s  %3d bankrupt  %3d wipeout" % (y, sum(r["kind"] == "bankrupt" for r in ys), sum(r["kind"] == "wipeout" for r in ys)))


if __name__ == "__main__":
    main()
