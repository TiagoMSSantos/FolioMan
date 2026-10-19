#!/usr/bin/env python3
"""Fill .tiingo_dead_members.csv: monthly bars of the exited S&P 500 members the WIKI archive lacks or censors.

(#752) tests/dead-members.csv stops on 2018-03-27, so a member that left the index later, or was still
trading then, walks as censored or not at all. Tiingo's free end-of-day API serves many of them. Its
licence is personal use and the repo is public, so the file this writes is never committed: probe.yml
keeps it in an encrypted cache and runs this after the backtests, `--max` names per run, because a free
key allows 50 requests an hour and 500 new symbols a month.

Rows are the WIKI tool's: `ticker,month,close,div`, split-adjusted from Tiingo's raw close, divCash and
splitFactor. A candidate is an exited member (every S&P span closed) the WIKI file does not show dead,
listed by Tiingo as a USD stock that started before its last exit and traded to within 30 days of it.
A refused or missing series writes `# miss TK why` so no later run asks again: a hole (a reused ticker
resumes after a gap as another company), a first bar after the exit (the ticker's new holder), or (#756) a
body that is not a price CSV. A second such body in a row stops the run unwritten, since two bad series in a
row read more like a refusal. A rate limit or any other error stops the run and writes nothing for that name.

A new calendar month restarts the file, and so does a file with another tag. (#756) The fetch month's bar is
partial and is skipped, so a series still trading ends on the month before it, whatever day the fill ran;
the backtest calls every series that ends before its file's newest month dead. A (#752) file kept that bar,
so it restarts rather than mix the two shapes.

Prints counts only, never the key or a price.
"""
import argparse, csv, datetime, io, os, json, re, sys, urllib.error, urllib.parse, urllib.request, zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from dead_members_from_wiki import holed, monthly, num  # noqa: E402

TICKERS = "https://apimedia.tiingo.com/docs/tiingo/daily/supported_tickers.zip"
PRICES = "https://api.tiingo.com/tiingo/daily/{}/prices?startDate=1970-01-01&format=csv"
NEED = ("date", "close", "divCash", "splitFactor")
HEAD = "# (#756) Tiingo monthly bars of exited S&P 500 members, fetched in {}. Personal-use licence: never commit.\n"


def day(s):
    return datetime.date.fromisoformat(s[:10])


def candidates(spans, wiki, listing, done):
    """Exited members, minus the WIKI file's dead ones and `done`, with a Tiingo USD stock row that
    started before the last exit and ended no earlier than 30 days before it. Sorted."""
    last = {}
    for l in wiki.splitlines():
        p = l.split(",")
        if len(p) == 4 and re.fullmatch(r"\d{4}-\d\d", p[1]):
            last[p[0]] = max(last.get(p[0], ""), p[1])
    newest = max(last.values(), default="")
    out = []
    for tk, s in sorted(spans.items()):
        if not s or any(e is None for _, e in s) or last.get(tk, newest) < newest or tk in done:
            continue
        exit_ = max(day(e) for _, e in s)
        if any(a == "Stock" and c == "USD" and b and e and day(b) < exit_ and day(e) >= exit_ - datetime.timedelta(days=30)
               for a, c, b, e in listing.get(tk, [])):
            out.append(tk)
    return out


def series(text, exit_, month):
    """Tiingo's daily CSV -> ([(month, close, div)], None), or (None, why) for a refused series. Rows of
    the fetch `month` and later are skipped. A body that is not a price CSV raises, and `main` decides
    whether it is a miss."""
    rows = csv.DictReader(io.StringIO(text))
    if not rows.fieldnames or any(c not in rows.fieldnames for c in NEED):
        raise ValueError("not a price CSV")
    days, divs, splits, first = {}, [], [], None
    for r in rows:
        if any(r[c] is None for c in NEED) or r["date"][:7] >= month:
            continue
        d, c = r["date"][:10], num(r["close"])
        if c is not None and c > 0 and (first is None or d < first):
            first = d
        if c is not None and c > 0 and (d[:7] not in days or d > days[d[:7]][0]):
            days[d[:7]] = (d, c)
        x, s = num(r["divCash"]), num(r["splitFactor"])
        if x is not None and x > 0:
            divs.append((d, x))
        if s is not None and s > 0 and s != 1.0:
            splits.append((d, s))
    if not days:
        return None, "empty"
    if holed(sorted(days)):
        return None, "hole"
    if day(first) > exit_:
        return None, "starts after its exit"
    return monthly(days, divs, splits), None


def kept(text, month):
    """The out file's text and the tickers it answers (rows or miss lines), or nothing when it was
    fetched in another month."""
    m = re.match(r"# \(#756\) .*fetched in (\d{4}-\d\d)", text)
    if not m or m.group(1) != month:
        return "", set()
    done = set()
    for l in text.splitlines():
        if l.startswith("# miss "):
            done.add(l.split()[2])
        elif not l.startswith("#") and len(p := l.split(",")) == 4 and p[0] != "ticker":
            done.add(p[0])
    return text, done


def check():
    spans = {"A": [["2000-01-01", "2020-05-01"]], "B": [["2000-01-01", None]], "C": [["2000-01-01", "2016-01-01"]],
             "D": [["2000-01-01", "2021-01-01"]], "E": [["2000-01-01", "2022-06-01"]], "F": [["2000-01-01", "2021-01-01"]]}
    wiki = "# c\nticker,month,close,div\nC,2015-12,1,0\nA,2018-03,1,0\n"
    listing = {"A": [("Stock", "USD", "1990-01-02", "2020-04-10")], "C": [("Stock", "USD", "1990-01-02", "2016-01-01")],
               "D": [("ETF", "USD", "1990-01-02", "2021-01-01")], "E": [("Stock", "USD", "2023-01-02", "2026-10-16")],
               "F": [("Stock", "USD", "1990-01-02", "2019-01-01"), ("Stock", "USD", "2024-01-02", "2026-10-16")]}
    # A: censored in WIKI, listed to 21 days before its exit. B live, C WIKI-dead, D an ETF, E a later
    # listing, F stopped two years before it left
    assert candidates(spans, wiki, listing, set()) == ["A"], candidates(spans, wiki, listing, set())
    assert candidates(spans, wiki, listing, {"A"}) == []
    # the WIKI tool's 2:1 split mid-June, as daily rows: May's close and both dividends halve
    csv_ = ("date,close,high,divCash,splitFactor\n2010-05-10,41,0,1.0,1.0\n2010-05-28,40,0,0,1.0\n"
            "2010-06-10,43,0,0.5,1.0\n2010-06-15,21,0,0,2.0\n2010-06-30,22,0,0,1.0\n")
    assert series(csv_, datetime.date(2010, 7, 1), "2026-10") == ([("2010-05", 20.0, 0.5), ("2010-06", 22.0, 0.25)], None)
    # (#756) fetched in June: June's partial rows go, its split with them, so May stays as it closed
    assert series(csv_, datetime.date(2010, 7, 1), "2010-06") == ([("2010-05", 40.0, 1.0)], None)
    assert series(csv_, datetime.date(2010, 5, 1), "2026-10") == (None, "starts after its exit")
    assert series("date,close,divCash,splitFactor\n2010-05-28,1,0,1\n2010-07-30,1,0,1\n", datetime.date(2011, 1, 1), "2026-10") == (None, "hole")
    assert series("date,close,divCash,splitFactor\n", datetime.date(2011, 1, 1), "2026-10") == (None, "empty")
    try:
        series('{"detail":"Not found."}', datetime.date(2011, 1, 1), "2026-10")
        raise AssertionError("a JSON answer must raise")
    except ValueError:
        pass
    f = HEAD.format("2026-10") + "ticker,month,close,div\nA,2010-05,20,0\n# miss B hole\n"
    assert kept(f, "2026-10") == (f, {"A", "B"}) and kept(f, "2026-11") == ("", set())
    assert kept(f.replace("(#756)", "(#752)"), "2026-10") == ("", set())  # a (#752) file kept the partial bar


def get(url, key=None):
    head = {"User-Agent": "FolioMan dead-members fill"} | ({"Authorization": f"Token {key}"} if key else {})
    with urllib.request.urlopen(urllib.request.Request(url, headers=head), timeout=60) as r:
        return r.read()


def main():
    check()
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--spans", default=".sp500_history.json", help="the membership cache `backtest ... pit` writes")
    ap.add_argument("--wiki", default="tests/dead-members.csv")
    ap.add_argument("--out", default=".tiingo_dead_members.csv")
    ap.add_argument("--max", type=int, default=45, help="names fetched this run (free key: 50 requests an hour)")
    ap.add_argument("--tickers", default=TICKERS, help="supported_tickers.zip, a path or a URL (public, no key)")
    a = ap.parse_args()
    key = os.environ.get("TIINGO_API_KEY", "")
    if not key:
        sys.exit("TIINGO_API_KEY is not set")
    month = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m")
    try:
        with open(a.out, encoding="utf-8") as f:
            text, done = kept(f.read(), month)
    except FileNotFoundError:
        text, done = "", set()
    with open(a.spans, encoding="utf-8") as f:
        spans = json.load(f)
    with open(a.wiki, encoding="utf-8") as f:
        wiki = f.read()
    raw = get(a.tickers) if a.tickers.startswith("https://") else open(a.tickers, "rb").read()
    listing = {}
    with zipfile.ZipFile(io.BytesIO(raw)) as z:
        rows = csv.reader(io.TextIOWrapper(z.open(z.namelist()[0]), encoding="utf-8", newline=""))
        h = next(rows)
        ti, ai, ci, si, ei = (h.index(c) for c in ("ticker", "assetType", "priceCurrency", "startDate", "endDate"))
        for r in rows:
            if len(r) >= len(h):
                listing.setdefault(r[ti].upper(), []).append((r[ai], r[ci], r[si], r[ei]))
    todo = candidates(spans, wiki, listing, done)
    got, missed, stop, unread = 0, 0, None, False
    with open(a.out, "w" if not text else "a", encoding="utf-8", newline="\n") as f:
        if not text:
            f.write(HEAD.format(month) + "ticker,month,close,div\n")
        for tk in todo[:a.max]:
            exit_ = max(day(e) for _, e in spans[tk])
            try:
                bars, why = series(get(PRICES.format(urllib.parse.quote(tk.lower(), safe="")), key).decode("utf-8", "replace"), exit_, month)
            except urllib.error.HTTPError as e:
                if e.code != 404:
                    stop = f"HTTP {e.code}"
                    break
                bars, why = None, "not served"
            except ValueError:
                bars, why = None, "not a price CSV"
            except (urllib.error.URLError, OSError) as e:
                stop = type(e).__name__
                break
            if unread and why == "not a price CSV":
                stop = "two bodies in a row that are not a price CSV"
                break
            unread = why == "not a price CSV"
            if bars is None:
                f.write(f"# miss {tk} {why}\n")
                missed += 1
            else:
                f.writelines(f"{tk},{m},{c:.6g},{v:.6g}\n" for m, c, v in bars)
                got += 1
            f.flush()
    print(f"tiingo dead members ({month}{', file restarted' if not text else ''}): {got} series written, {missed} missed,"
          f" {len(todo) - got - missed} of {len(todo)} candidates left{f'; stopped on {stop}' if stop else ''}",
          file=sys.stderr)
    if stop and stop != "HTTP 429":
        sys.exit(1)


if __name__ == "__main__":
    main()
