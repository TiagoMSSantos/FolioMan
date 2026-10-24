#!/usr/bin/env python3
"""(#854) Choose what one leg of ci.yml's mutation gate grades.

The gate grades at most BUDGET mutants per push, split across two legs. Within the budget each leg
grades half the list. Over it, the legs split the old round-robin shard 1/N: shard 1 + LEG*N of 2N,
so the two together grade exactly what shard 1/N did.

stdin: `cargo mutants --list --json --in-diff push.diff`. stdout, one item per line: how many mutants
this leg grades (the step checks the run against it), a note for the step summary, then the
cargo-mutants arguments that select the leg.

usage: mutants-pick.py BUDGET LEG
"""
import json, sys


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.splitlines()[-1])
    budget, leg = int(sys.argv[1]), int(sys.argv[2])
    # A diff with no Rust in it lists to an empty stdout, not `[]` (27.1.0, exit 0). A failed list
    # exits non-zero, and the step reds on that before it gets here.
    ms = json.loads(sys.stdin.read().strip() or "[]")
    n = len(ms)
    k, shards = leg, 2
    if n == 0:
        note = "no mutants in the diff — nothing to grade"
    elif n <= budget:
        note = f"{n} mutants in the changed lines, all graded"
    else:
        old = -(-n // budget)
        k, shards = 1 + leg * old, 2 * old  # both legs together grade the old shard 1/old
        note = f"{n} mutants exceeds the {budget} budget — grading shard 1/{old}"
    mine = len(ms[k::shards])
    print(mine)
    print(f"{note}; leg {leg} grades {mine}")
    print("\n".join(["--shard", f"{k}/{shards}", "--sharding", "round-robin"]))


main()
