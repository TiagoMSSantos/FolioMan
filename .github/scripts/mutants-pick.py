#!/usr/bin/env python3
"""(#855) Choose what one leg of ci.yml's mutation gate grades.

The gate grades at most BUDGET mutants per push, split across two legs (#854). Within the budget it
grades every mutant. Over it, a plain round-robin shard spent the budget mostly on fn-value mutants
(`replace f -> T with <value>`): in census (#847) 0.11% of those survived, against 2.37% of every
other kind. So the other kinds take the budget first, spread across the diff's files, and fn-value
mutants only fill what is left.

Struct-field mutants (`delete field x from struct S expression`) ignore `-F` in cargo-mutants 27.1.0
but obey `--shard`, so they ride along with any pick: they hold the first slots, and when they alone
fill the budget the legs fall back to the old shard.

stdin: `cargo mutants --list --json --in-diff push.diff`. stdout, one item per line: how many mutants
this leg grades (the step checks the run against it), a note for the step summary, then the
cargo-mutants arguments that select the leg.

usage: mutants-pick.py BUDGET LEG
"""
import json, re, sys
from itertools import zip_longest


def spread(ms):
    """Round-robin across files, list order within a file."""
    by = {}
    for m in ms:
        by.setdefault(m["file"], []).append(m)
    return [m for row in zip_longest(*by.values()) for m in row if m is not None]


def esc(s):
    """Escape every character the regex crate treats as a metacharacter."""
    return re.sub(r"([\\.+*?()|\[\]{}^$#&~-])", r"\\\1", s)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.splitlines()[-1])
    budget, leg = int(sys.argv[1]), int(sys.argv[2])
    # A diff with no Rust in it lists to an empty stdout, not `[]` (27.1.0, exit 0). A failed list
    # exits non-zero, and the step reds on that before it gets here.
    ms = json.loads(sys.stdin.read().strip() or "[]")
    n, sf = len(ms), sum(m["genre"] == "StructField" for m in ms)
    sel, k, shards, args = ms, leg, 2, []
    if n == 0:
        note = "no mutants in the diff — nothing to grade"
    elif n <= budget:
        note = f"{n} mutants in the changed lines, all graded"
    elif sf >= budget:
        old = -(-n // budget)
        k, shards = 1 + leg * old, 2 * old  # both legs together grade the old shard 1/old
        note = f"{n} mutants exceeds the {budget} budget and {sf} struct-field mutants fill it — grading shard 1/{old}"
    else:
        rest = [m for m in ms if m["genre"] != "StructField"]
        picks = (spread([m for m in rest if m["genre"] != "FnValue"])
                 + spread([m for m in rest if m["genre"] == "FnValue"]))[: budget - sf]
        names = {m["name"] for m in picks}
        sel = [m for m in ms if m["genre"] == "StructField" or m["name"] in names]
        for name in sorted(names):
            args += ["-F", f"^{esc(name)}$"]
        fv = sum(m["genre"] == "FnValue" for m in picks)
        note = (f"{n} mutants exceeds the {budget} budget — grading {len(sel)}: {sf} struct-field, "
                f"{len(picks) - fv} other non-fn-value, {fv} fn-value")
    mine = len(sel[k::shards])
    print(mine)
    print(f"{note}; leg {leg} grades {mine}")
    print("\n".join(args + ["--shard", f"{k}/{shards}", "--sharding", "round-robin"]))


main()
