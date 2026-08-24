#!/usr/bin/env python3
"""Reconstruct point-in-time index membership from a Wikipedia page's revision history.

Emits `ticker,start_date,end_date` — the exact shape `core::sp500_spans` already parses, with an
empty `end_date` meaning "still a member" and HALF-OPEN spans (`start <= on < end`), matching
`core::sp500_member_at`. Not wired into the binary: run it by hand, commit the CSV it writes.

WHY A COUNT BAND IS A HARD REFUSAL AND NOT A WARNING. The first prototype of this parser matched
`{{NyseSymbol|...}}` case-sensitively and silently returned 136 of 400 names — a file that looks
completely plausible and is two-thirds wrong, which would then be measured as "the pond adds
nothing". Any epoch whose parsed roster falls outside `--band` is DROPPED as an unread observation
and printed; it is never emitted as membership.

Sampling is MONTHLY, so a span boundary is accurate to within one month. The backtest steps its
cutoffs every six months, so that is far finer than anything downstream reads.
"""
import argparse, datetime, hashlib, json, os, re, sys, time, urllib.error, urllib.parse, urllib.request

API = "https://en.wikipedia.org/w/api.php"
UA = "FolioMan-membership-reconstruction/0.1 (portfolio backtest research; non-commercial)"
PACE = 1.5  # seconds between calls; Wikipedia answers 429 to anything much brisker

TICKER_TMPL = re.compile(
    r"\{\{\s*(?:NYSE|Nasdaq|NYSEAmerican|NYSEArca|AMEX)[A-Za-z]*\s*\|\s*([A-Za-z0-9][A-Za-z0-9.\-]{0,6})\s*[}|]",
    re.I,  # case-insensitive ON PURPOSE — see the module docstring
)

_last = [0.0]


def _api(params, cache_dir, tries=9):
    key = hashlib.sha1(urllib.parse.urlencode(params).encode()).hexdigest()
    path = os.path.join(cache_dir, key + ".json")
    if os.path.exists(path):
        return json.load(open(path))
    for attempt in range(tries):
        gap = PACE - (time.time() - _last[0])
        if gap > 0:
            time.sleep(gap)
        _last[0] = time.time()
        try:
            req = urllib.request.Request(API + "?" + urllib.parse.urlencode(params), headers={"User-Agent": UA})
            body = json.load(urllib.request.urlopen(req, timeout=90))
            json.dump(body, open(path, "w"))
            return body
        except urllib.error.HTTPError as e:
            if e.code in (429, 503):
                wait = min(300, 5 * (2 ** attempt))
                print("  wikipedia %d — backing off %ds" % (e.code, wait), file=sys.stderr)
                time.sleep(wait)
                continue
            raise
        except Exception as e:  # transient DNS/TLS/timeout
            print("  wikipedia error %s — retrying" % e, file=sys.stderr)
            time.sleep(5)
    raise SystemExit("wikipedia: gave up after %d tries" % tries)


def revision_at(title, when, cache_dir):
    """The page as it stood at `when` (a date). Returns (timestamp, wikitext)."""
    body = _api(
        {
            "action": "query", "format": "json", "prop": "revisions", "titles": title,
            "rvlimit": "1", "rvprop": "content|timestamp", "rvslots": "main", "rvdir": "older",
            "rvstart": when.strftime("%Y-%m-%dT00:00:00Z"),
        },
        cache_dir,
    )
    page = list(body["query"]["pages"].values())[0]
    if "revisions" not in page:
        return None, None
    rev = page["revisions"][0]
    return rev["timestamp"], rev["slots"]["main"]["*"]


def _tables(wikitext):
    """Each top-level `{| ... |}` table, nesting-aware."""
    out, i = [], 0
    while True:
        start = wikitext.find("{|", i)
        if start == -1:
            return out
        depth, j = 0, start
        while j < len(wikitext) - 1:
            if wikitext[j:j + 2] == "{|":
                depth += 1
                j += 2
                continue
            if wikitext[j:j + 2] == "|}":
                depth -= 1
                j += 2
                if depth == 0:
                    break
                continue
            j += 1
        out.append(wikitext[start:j])
        i = j


def roster(wikitext):
    """Tickers of the constituents table, in page order.

    The page also carries an added/removed CHANGES table whose rows hold two tickers each, and on a
    long page that table can out-number the constituents table. So the page is cut at the first
    `changes` heading before the largest ticker table is chosen.
    """
    cut = re.search(r"^=+\s*[^=\n]*changes[^=\n]*=+\s*$", wikitext, re.I | re.M)
    head = wikitext[: cut.start()] if cut else wikitext
    best = []
    for table in _tables(head):
        found = [t.upper() for t in TICKER_TMPL.findall(table)]
        if len(found) > len(best):
            best = found
    seen, out = set(), []
    for t in best:  # dual share classes are distinct tickers; only exact repeats drop
        if t not in seen:
            seen.add(t)
            out.append(t)
    return out


def months(start, end):
    y, m = start.year, start.month
    while datetime.date(y, m, 1) <= end:
        yield datetime.date(y, m, 1)
        y, m = (y + 1, 1) if m == 12 else (y, m + 1)


def build_spans(observations):
    """`[(epoch, {tickers})]` -> `[(ticker, start, end_or_None)]`, half-open.

    A span opens at the epoch a name first appears and closes (END EXCLUSIVE) at the first epoch it is
    gone. A name that leaves and comes back gets a SECOND span, never a merged one — collapsing those
    would readmit it for the years it was out, the exact bias this file exists to remove.
    """
    spans, open_at = [], {}
    for epoch, names in observations:
        for t in names:
            if t not in open_at:
                open_at[t] = epoch
        for t in [t for t in open_at if t not in names]:
            spans.append((t, open_at.pop(t), epoch))
    for t, start in open_at.items():
        spans.append((t, start, None))
    spans.sort(key=lambda s: (s[0], s[1]))
    return spans


def selftest():
    """Offline check of the two things that fail SILENTLY: the roster parse and the span walk."""
    page = (
        "intro\n"
        '{| class="wikitable" id="constituents"\n'
        "! Symbol !! Security\n"
        "|-\n| {{NyseSymbol|AA}} || [[Alcoa]]\n"
        "|-\n| {{NasdaqSymbol|aapl}} || [[Apple]]\n"     # lowercase template + ticker
        "|-\n| {{NYSEAmerican|BF.B}} || [[Brown-Forman]]\n"
        "|}\n"
        "== Selected changes to the list ==\n"
        '{| class="wikitable" id="changes"\n'
        "|-\n| {{NyseSymbol|XX}} || added || {{NyseSymbol|YY}} || removed\n"
        "|-\n| {{NyseSymbol|ZZ}} || added || {{NyseSymbol|QQ}} || removed\n"
        "|-\n| {{NyseSymbol|RR}} || added || {{NyseSymbol|SS}} || removed\n"
        "|-\n| {{NyseSymbol|TT}} || added || {{NyseSymbol|UU}} || removed\n"
        "|}\n"
    )
    got = roster(page)
    assert got == ["AA", "AAPL", "BF.B"], got            # case-insensitive, and NOT the changes table
    assert "XX" not in got and "YY" not in got, got

    d = datetime.date
    obs = [
        (d(2020, 1, 1), {"A", "B"}),
        (d(2020, 2, 1), {"A"}),          # B leaves
        (d(2020, 3, 1), {"A", "B"}),     # B returns -> a SECOND span, not a merged one
    ]
    assert build_spans(obs) == [
        ("A", d(2020, 1, 1), None),
        ("B", d(2020, 1, 1), d(2020, 2, 1)),
        ("B", d(2020, 3, 1), None),
    ], build_spans(obs)

    assert months(d(2020, 11, 1), d(2021, 1, 15)) is not None
    assert list(months(d(2020, 11, 1), d(2021, 1, 15))) == [d(2020, 11, 1), d(2020, 12, 1), d(2021, 1, 1)]
    print("selftest ok")


def main():
    ap = argparse.ArgumentParser()
    if "--selftest" in sys.argv:
        return selftest()
    ap.add_argument("--selftest", action="store_true", help="run the offline parser checks and exit")
    ap.add_argument("--title", required=True, help='e.g. "List of S&P 400 companies"')
    ap.add_argument("--start", required=True, help="first epoch, YYYY-MM")
    ap.add_argument("--band", required=True, nargs=2, type=int, metavar=("LO", "HI"),
                    help="accept an epoch only if its roster size is within [LO, HI]")
    ap.add_argument("--out", required=True)
    ap.add_argument("--cache", default=os.path.expanduser("~/.cache/folioman-wikipedia"))
    args = ap.parse_args()

    os.makedirs(args.cache, exist_ok=True)
    lo, hi = args.band
    y, m = (int(x) for x in args.start.split("-"))
    today = datetime.date.today()

    observations, rejected = [], []
    for epoch in months(datetime.date(y, m, 1), today):
        stamp, wikitext = revision_at(args.title, epoch, args.cache)
        if not wikitext:
            rejected.append((epoch, "no revision"))
            continue
        names = roster(wikitext)
        if not (lo <= len(names) <= hi):
            rejected.append((epoch, "roster %d outside [%d, %d]" % (len(names), lo, hi)))
            continue
        observations.append((epoch, set(names)))
        print("  %s  rev %s  %d names" % (epoch, stamp[:10], len(names)))

    if not observations:
        raise SystemExit("no epoch passed the count band — refusing to write an empty membership file")
    spans = build_spans(observations)

    with open(args.out, "w") as fh:
        fh.write("ticker,start_date,end_date\n")
        for t, start, end in spans:
            fh.write("%s,%s,%s\n" % (t, start, end or ""))

    print("\n%s: %d spans over %d names, %d epochs read, %d rejected"
          % (args.out, len(spans), len({s[0] for s in spans}), len(observations), len(rejected)))
    print("   window: %s .. %s" % (observations[0][0], observations[-1][0]))
    for epoch, why in rejected:
        print("   REJECTED %s: %s" % (epoch, why))


if __name__ == "__main__":
    main()
