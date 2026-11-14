#!/usr/bin/env python3
"""(#1522) Which settings moved between two record lines: for every step of the page's journal, the keys of
tests/ci-settings.yaml whose value moved, that arrived or that left between the commit that wrote the earlier line
and the commit that wrote the later one.

The page ships most days, and a rule that ships between two lines moves seats for its own reason. A reader of the
record (tools/book_churn.py is one) cannot tell such a step from a market's unless something names it. This names
the steps on which a shipped VALUE changed. It does not name code: code changes between almost every pair of
lines, and a knob that arrives at its off value is listed like one that arrives switched on, so a listed step is
a step to look at, not a verdict.

    python3 -I tools/ship_days.py [--since 2026-10-15] [--only-moved] [--mark 2026-10-16 2026-10-17 ...]
    python3 -I tools/ship_days.py --since 2026-10-16 --mark $(python3 -I tools/book_churn.py .screen_snapshots.jsonl --changed)
    python3 -I tools/ship_days.py --selftest

It runs `git log` and `git show` in the checkout it stands in (the one tool here that does), finds each line's
commit by its subject, `chore(track): journal the <date> ranked snapshot`, and reads the settings file as that
commit holds it. --since keeps the steps whose later line is dated then or after. --only-moved leaves out the
steps where nothing moved, arrived or left. --mark stars the steps that end on the given dates (the days the
BUY NOW head changed, as `book_churn.py --changed` prints them) and counts the settings steps among them and
among the rest.

The settings file is read by a small reader of its own, not a YAML library (`python3 -I` on a runner promises
none): `key: value` lines nested by indent, a comment cut at the first ` #` outside quotes, and a list or a
nested list of mappings kept whole as its parent key's value. shortcut: a block scalar, a flow mapping over
several lines or a quoted key would be misread; the file holds none of them (checked against a YAML library on
the 54 lines' commits to 2026-11-14: the same keys on every one, the same moved, arrived and left on every step).
"""
import argparse, re, subprocess

SETTINGS = "tests/ci-settings.yaml"
JOURNAL = ".screen_snapshots.jsonl"
SUBJECT = re.compile(r"^chore\(track\): journal the (\d{4}-\d{2}-\d{2}) ranked snapshot$")
KEY = re.compile(r"^([A-Za-z0-9_][A-Za-z0-9_.-]*):(?:\s+(.*))?$")  # `head_weight`, `1W`, `VUAA.DE`


def uncomment(line):
    """The line without its comment: cut at a `#` that opens the line or follows a blank, outside quotes."""
    quote = None
    for i, ch in enumerate(line):
        if quote:
            quote = None if ch == quote else quote
        elif ch in "\"'":
            quote = ch
        elif ch == "#" and (i == 0 or line[i - 1] in " \t"):
            return line[:i].rstrip()
    return line.rstrip()


def settings_keys(text):
    """{dotted key: value} for a settings file. A scalar's value is its text; a key whose children are list items
    holds them, and everything nested under them, as one tuple of stripped lines."""
    out, stack, held = {}, [], None  # stack: (indent, key) of the open mappings; held: (indent, path) of an open list
    for raw in text.splitlines():
        line = uncomment(raw)
        if not line.strip():
            continue
        indent, body = len(line) - len(line.lstrip(" ")), line.strip()
        if held is not None and (indent > held[0] or (indent == held[0] and body.startswith("-"))):
            out[held[1]] += (body,)
            continue
        held = None
        if body.startswith("-"):
            # a list under the nearest open key at this indent or a shallower one (`key:` then `- item` may share
            # an indent); a list with no such key is dropped
            while stack and stack[-1][0] > indent:
                stack.pop()
            if stack:
                path = ".".join(k for _, k in stack)
                out[path] = (body,)
                held = (stack[-1][0], path)
            continue
        while stack and stack[-1][0] >= indent:
            stack.pop()
        m = KEY.match(body)
        if not m:
            continue
        path = ".".join([k for _, k in stack] + [m.group(1)])
        if m.group(2) is None:
            stack.append((indent, m.group(1)))
            out[path] = ()
        else:
            out[path] = m.group(2)
    return {k: v for k, v in out.items() if v != ()}


def moved(before, after):
    """(keys whose value differs, keys only in `after`, keys only in `before`), each sorted."""
    return (sorted(k for k in before if k in after and before[k] != after[k]), sorted(k for k in after if k not in before), sorted(k for k in before if k not in after))


def line_commits(log):
    """{date: commit} from `git log --format=%H%x09%s` lines, newest first: the newest commit that journals a date."""
    out = {}
    for row in log.splitlines():
        commit, _, subject = row.partition("\t")
        m = SUBJECT.match(subject)
        if m:
            out.setdefault(m.group(1), commit)
    return out


def steps(commits, settings_at, since=None):
    """[(earlier date, later date, moved, arrived, left, before, after)] for each pair of consecutive record lines
    whose later line is dated `since` or after. `settings_at(commit)` is the settings text that commit holds."""
    dates, out, cache = sorted(commits), [], {}

    def read(d):
        if d not in cache:
            cache[d] = settings_keys(settings_at(commits[d]))
        return cache[d]

    for a, b in zip(dates, dates[1:]):
        if since is None or b >= since:
            out.append((a, b) + moved(read(a), read(b)) + (read(a), read(b)))
    return out


def show(value):
    text = " | ".join(value) if isinstance(value, tuple) else str(value)
    return text if len(text) <= 48 else text[:45] + "..."


def changed_items(before, after):
    """For a list value: the items that left and the items that came, as text."""
    b = before if isinstance(before, tuple) else (str(before),)
    a = after if isinstance(after, tuple) else (str(after),)
    return [x for x in b if x not in a], [x for x in a if x not in b]


def report(rows, only_moved=False, marks=()):
    touched = lambda r: bool(r[2] or r[3] or r[4])
    out = [f"record steps read: {len(rows)}; a settings key moved, arrived or left on {sum(map(touched, rows))}, a shipped value moved on {sum(1 for r in rows if r[2] or r[4])}"]
    if marks:
        starred, rest = [r for r in rows if r[1] in marks], [r for r in rows if r[1] not in marks]
        out.append(f"  of the {len(starred)} marked steps (*) a key moved, arrived or left on {sum(map(touched, starred))}; of the other {len(rest)}, on {sum(map(touched, rest))}")
    for a, b, mv, new, gone, before, after in rows:
        if only_moved and not (mv or new or gone):
            continue
        cells = []
        for k in mv:
            if isinstance(before[k], tuple) or isinstance(after[k], tuple):
                out_items, in_items = changed_items(before[k], after[k])
                cells.append(f"{k}: -[{show(tuple(out_items))}] +[{show(tuple(in_items))}]")
            else:
                cells.append(f"{k}: {show(before[k])} -> {show(after[k])}")
        cells += [f"+{k}: {show(after[k])}" for k in new] + [f"-{k}" for k in gone[:6]] + ([f"(and {len(gone) - 6} more left)"] if len(gone) > 6 else [])
        out.append(f"  {b}{'*' if b in marks else ' '} " + ("; ".join(cells) if cells else "nothing"))
    return "\n".join(out)


def git(*args):
    return subprocess.run(("git",) + args, capture_output=True, text=True, check=True).stdout


def selftest():
    assert uncomment("a: 1   # note") == "a: 1" and uncomment("# all") == "" and uncomment("u: http://x/#frag") == "u: http://x/#frag"
    assert uncomment('t: "14:30" # New York') == 't: "14:30"' and uncomment("s: 'a # b' # c") == "s: 'a # b'" and uncomment("  k: v") == "  k: v"
    text = "\n".join([
        "# a header comment",
        "top_picks: 25      # how many",
        "tickers: []",
        "sizing:",
        "  head_weight: 8.0    # (#322)",
        "  book_names: 10",
        "buy_heuristic:",
        "  growth_overext_free: 60.0",
        "  columns:",
        "    - rank        # the rank",
        "    - name",
        "  growth_fund_extra:",
        "    - factor: ey_pct         # (#1345)",
        "      weight: 0.03",
        "    - factor: mom61_pct",
        "      weight: 0.03",
        "  demean_by_market: true",
        "empty_block:",
        "  # nothing but a comment",
        "same_indent:",
        "- a",
        "- b",
        "intraday_exit_us: \"14:30\" # (#1212) New York time",
        "urls:",
        "  yahoo: https://query1.example/v8/{ticker}?a=1#x",
        "anchor_windows:",
        "  1W: 7",
        "  10Y: 365",
        "history_proxy:",
        "  VUAA.DE: VOO   # a dotted key",
    ])
    keys = settings_keys(text)
    assert keys == {
        "top_picks": "25", "tickers": "[]", "sizing.head_weight": "8.0", "sizing.book_names": "10", "buy_heuristic.growth_overext_free": "60.0",
        "buy_heuristic.columns": ("- rank", "- name"),
        "buy_heuristic.growth_fund_extra": ("- factor: ey_pct", "weight: 0.03", "- factor: mom61_pct", "weight: 0.03"),
        "buy_heuristic.demean_by_market": "true", "same_indent": ("- a", "- b"), "intraday_exit_us": '"14:30"',
        "urls.yahoo": "https://query1.example/v8/{ticker}?a=1#x", "anchor_windows.1W": "7", "anchor_windows.10Y": "365", "history_proxy.VUAA.DE": "VOO",
    }, keys
    assert settings_keys("") == {} and settings_keys("# only\n\n") == {} and "empty_block" not in keys, "a mapping with no child holds nothing"
    later = settings_keys(text.replace("head_weight: 8.0 ", "head_weight: 4.0 ").replace("  book_names: 10\n", "").replace("      weight: 0.03\n    - factor: mom61_pct\n", "      weight: 0.05\n")
                          + "\nnew_knob: false   # arrives off\n")
    mv, new, gone = moved(keys, later)
    assert (mv, new, gone) == (["buy_heuristic.growth_fund_extra", "sizing.head_weight"], ["new_knob"], ["sizing.book_names"]), (mv, new, gone)
    assert moved(keys, keys) == ([], [], []) and moved({}, keys)[1] == sorted(keys) and moved(keys, {})[2] == sorted(keys)
    assert later["buy_heuristic.growth_fund_extra"] == ("- factor: ey_pct", "weight: 0.05", "weight: 0.03")
    assert changed_items(keys["buy_heuristic.growth_fund_extra"], later["buy_heuristic.growth_fund_extra"]) == (["- factor: mom61_pct"], ["weight: 0.05"]), "by membership"
    assert changed_items("a", ("- b",)) == (["a"], ["- b"])
    log = "c3\tchore(track): journal the 2026-01-03 ranked snapshot\nc9\tfeat(picks): something\nc2b\tchore(track): journal the 2026-01-02 ranked snapshot\n" \
          "c2a\tchore(track): journal the 2026-01-02 ranked snapshot\nc1\tchore(track): journal the 2026-01-01 ranked snapshot\nxx\tchore(track): journal the day\n"
    commits = line_commits(log)
    assert commits == {"2026-01-03": "c3", "2026-01-02": "c2b", "2026-01-01": "c1"}, "the newest commit of a date written twice; other subjects dropped"
    held = {"c1": "a: 1\nb: 2\n", "c2b": "a: 1\nb: 3\nc: off\n", "c3": "a: 1\nb: 3\nc: off\n"}
    asked = []
    at = lambda c: (asked.append(c), held[c])[1]
    rows = steps(commits, at)
    assert [(r[0], r[1], r[2], r[3], r[4]) for r in rows] == [("2026-01-01", "2026-01-02", ["b"], ["c"], []), ("2026-01-02", "2026-01-03", [], [], [])], rows
    assert asked == ["c1", "c2b", "c3"], "each commit's settings read once"
    assert [r[1] for r in steps(commits, at, since="2026-01-03")] == ["2026-01-03"] and steps({}, at) == [] and steps({"2026-01-01": "c1"}, at) == []
    rep = report(rows)
    assert rep.splitlines()[0] == "record steps read: 2; a settings key moved, arrived or left on 1, a shipped value moved on 1", rep
    assert "  2026-01-02  b: 2 -> 3; +c: off" in rep and "  2026-01-03  nothing" in rep and "2026-01-03" not in report(rows, only_moved=True), rep
    marked = report(rows, marks=("2026-01-02",))
    assert "  of the 1 marked steps (*) a key moved, arrived or left on 1; of the other 1, on 0" in marked and "  2026-01-02* b: 2 -> 3" in marked and "  2026-01-03  nothing" in marked, marked
    assert "of the 1 marked steps (*) a key moved, arrived or left on 0; of the other 1, on 1" in report(rows, marks=("2026-01-03", "2026-02-01")), "a mark on no step counts nothing"
    wide = report([("d0", "d1", ["l"], [], ["g%d" % i for i in range(8)], {"l": ("- x", "- y")}, {"l": ("- y", "- z")})])
    assert "l: -[- x] +[- z]" in wide and "-g5; (and 2 more left)" in wide and "a shipped value moved on 1" in wide, wide
    arrives = report([("d0", "d1", [], ["n"], [], {}, {"n": "0"})], only_moved=True)
    assert arrives.splitlines()[0].endswith("arrived or left on 1, a shipped value moved on 0") and "  d1  +n: 0" in arrives, "a key that arrives is listed, and is no moved value"
    assert "marked" not in arrives, "no mark, no count line"
    assert show("x" * 60) == "x" * 45 + "..." and show(("- a", "- b")) == "- a | - b"
    print("ship_days selftest ok")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--since", help="keep the steps whose later line is dated this day or after, YYYY-MM-DD")
    ap.add_argument("--only-moved", action="store_true", help="leave out the steps on which no key moved, arrived or left")
    ap.add_argument("--mark", nargs="*", default=[], metavar="DATE", help="star the steps that end on these dates and count the settings steps among them")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        selftest()
        return
    commits = line_commits(git("log", "--format=%H%x09%s", "--", JOURNAL))
    print(report(steps(commits, lambda c: git("show", f"{c}:{SETTINGS}"), a.since), a.only_moved, tuple(a.mark)))


if __name__ == "__main__":
    main()
