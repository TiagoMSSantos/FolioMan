#!/usr/bin/env python3
"""Build tests/dead-members.csv: monthly bars of the exited S&P 500 members, for `backtest ... pit dead`.

(#751) Yahoo serves most exited members as empty stubs, so the point-in-time walk was graded on the
survivors. The Quandl WIKI end-of-day archive (public domain, community-maintained, frozen 2018-03-27)
still holds many of them. This keeps the tickers whose every S&P span has closed, matched by the SAME
ticker only: a renamed or reused symbol is the backtest's refusal to make, and it counts them. A series
missing a month is refused here: WIKI stopped updating some live tickers in 2017 and later wrote one
2018-03 bar, and a reused ticker resumes after a gap as another company. A forward window read across
that hole would price a different firm, and cutting the series there would fake a death.

Rows are `ticker,month,close,div`: the month's last close and the month's summed dividends, each divided
by every split dated after it, so they match Yahoo's split-adjusted, price-only close with dividends as
separate events. WIKI's adj_close also folds the dividends in, so it is not read.

Not wired into the binary: download the Kaggle mirror (marketneutral/quandl-wiki-prices-us-equites) by
hand, run this on the zip from the repo root, commit the CSV it writes. The zip is read in place; nothing
in it is extracted.
"""
import argparse, csv, hashlib, io, json, re, sys, zipfile

NEED = ("ticker", "date", "close", "ex-dividend", "split_ratio")


def num(s):
    try:
        x = float(s)
    except ValueError:
        return None
    return x if x == x else None  # NaN reads as missing


def monthly(days, divs, splits):
    """days {YYYY-MM: (last date, raw close)}, divs [(date, raw amount)], splits [(date, ratio)] ->
    [(month, close, div)], each value divided by the product of the splits dated after it."""
    def later(d):
        f = 1.0
        for s, r in splits:
            if s > d:
                f *= r
        return f

    paid = {}
    for d, a in divs:
        paid[d[:7]] = paid.get(d[:7], 0.0) + a / later(d)
    return [(m, c / later(d), paid.get(m, 0.0)) for m, (d, c) in sorted(days.items())]


def holed(months):
    """Does a sorted list of YYYY-MM months skip one?"""
    ix = [int(m[:4]) * 12 + int(m[5:7]) for m in months]
    return any(b - a > 1 for a, b in zip(ix, ix[1:]))


def check():
    # a 2:1 split mid-June: May's close and both dividends (June's paid before the split) halve
    got = monthly({"2010-06": ("2010-06-30", 22.0), "2010-05": ("2010-05-28", 40.0)},
                  [("2010-05-10", 1.0), ("2010-06-10", 0.5)], [("2010-06-15", 2.0)])
    assert got == [("2010-05", 20.0, 0.5), ("2010-06", 22.0, 0.25)], got
    assert holed(["2010-01", "2010-03"]) and not holed(["2010-12", "2011-01"])


def main():
    check()
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("zip", help="the Kaggle download, read in place")
    ap.add_argument("--spans", default=".sp500_history.json", help="the membership cache `backtest ... pit` writes")
    ap.add_argument("--out", default="tests/dead-members.csv")
    a = ap.parse_args()
    with open(a.spans, encoding="utf-8") as f:
        exited = {t for t, s in json.load(f).items() if s and all(end is not None for _, end in s)}
    days, divs, splits = {}, {}, {}
    with zipfile.ZipFile(a.zip) as z:
        csvs = [i for i in z.infolist() if i.filename.lower().endswith(".csv")]
        if not csvs:
            sys.exit(f"{a.zip}: no .csv inside")
        member = max(csvs, key=lambda i: i.file_size)
        with z.open(member) as f:
            rows = csv.reader(io.TextIOWrapper(f, encoding="utf-8", newline=""))
            head = next(rows)
            if missing := [c for c in NEED if c not in head]:
                sys.exit(f"{member.filename}: no {missing} column")
            ti, di, ci, xi, si = (head.index(c) for c in NEED)
            for r in rows:
                if len(r) < len(head):
                    continue
                tk = r[ti].replace("_", "-").replace(".", "-").upper()
                if tk not in exited:
                    continue
                d, c = r[di], num(r[ci])
                if c is not None and c > 0:
                    m = days.setdefault(tk, {})
                    if d[:7] not in m or d > m[d[:7]][0]:
                        m[d[:7]] = (d, c)
                x, s = num(r[xi]), num(r[si])
                if x is not None and x > 0:
                    divs.setdefault(tk, []).append((d, x))
                if s is not None and s > 0 and s != 1.0:
                    splits.setdefault(tk, []).append((d, s))
    holes = [tk for tk in days if holed(sorted(days[tk]))]
    for tk in holes:
        del days[tk]
    out = sorted((tk, m, c, v) for tk in days for m, c, v in monthly(days[tk], divs.get(tk, []), splits.get(tk, [])))
    if not out:
        sys.exit("no exited S&P member matched: nothing written")
    with open(a.zip, "rb") as f:
        sha = hashlib.file_digest(f, "sha256").hexdigest()
    newest = max(r[1] for r in out)
    dead = sum(max(days[tk]) < newest for tk in days)
    name = re.sub(r"[^\w./-]", "?", member.filename)
    with open(a.out, "w", encoding="utf-8", newline="\n") as f:
        f.write(f"# (#751) monthly split-adjusted closes and dividends of {len(days)} exited S&P 500 members"
                f" ({len(holes)} more refused for a missing month)\n")
        f.write(f"# from the Quandl WIKI archive (public domain; frozen 2018-03-27): {name} in sha256 {sha}\n")
        f.write("# written by tools/dead_members_from_wiki.py and read by `backtest <years> ... pit dead`\n")
        f.write("ticker,month,close,div\n")
        for tk, m, c, v in out:
            f.write(f"{tk},{m},{c:.6g},{v:.6g}\n")
    print(f"{len(days)} of {len(exited)} exited members kept ({len(holes)} refused for a missing month),"
          f" {dead} end before {newest}; {len(out)} rows -> {a.out}",
          file=sys.stderr)


if __name__ == "__main__":
    main()
