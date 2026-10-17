#!/usr/bin/env python3
"""(#637) Fail when automerge.yml's `need` list and the checks the workflows report have drifted apart.

automerge.yml merges a bot PR once every check run is green AND each name in its hand-kept `need`
list ran and succeeded. The list rots in two directions, and they fail in opposite ways:
  - a job added to ci.yml and left out of `need` is a gate the merge never waits on: a bot PR merges
    while that job is red. That fails OPEN and nothing says so;
  - a `need` entry no job reports any more (a renamed job) can never be satisfied: every bot PR idles
    for good. That fails closed, but only shows as "why does nothing merge".
This reads ci.yml's jobs and codeql.yml's check names and compares both ways. A check run is named by
its job's `name:`, else the job key, and a matrix adds " (leg)" through the name template; ci.yml's
jobs carry neither. `cleanup-runs` is the one job `need` leaves out: it runs on main only, so a PR
never has that check.

No YAML library: the runner image does not promise PyYAML, so this reads the 2-space job keys under
`jobs:` with regexes. It exits loudly on a shape it was not taught (no jobs found, a matrix or a
`name:` expression it cannot expand) rather than pass on a parse that found nothing.

usage: automerge-need.py ci.yml automerge.yml codeql.yml
"""
import json, re, sys

EXEMPT = {"cleanup-runs"}  # main-only (`if: github.ref == 'refs/heads/main'`), skipped on every PR


def jobs(path):
    """job key -> the lines of its block, for the 2-space keys under the column-0 `jobs:`."""
    out, key, inside = {}, None, False
    for line in open(path).read().splitlines():
        if re.match(r"jobs:\s*(#.*)?$", line):
            inside = True
        elif inside and line[:1] not in ("", " ", "#"):
            break  # the next top-level key ends `jobs:`
        elif inside:
            m = re.match(r"  ([A-Za-z_][\w-]*):\s*(#.*)?$", line)
            if m:
                key = m.group(1)
                out[key] = []
            elif key:
                out[key].append(line)
    if not out:
        sys.exit(f"{path}: no jobs found under `jobs:`; this script was not taught that layout")
    return out


def checks(path):
    """The check-run names a workflow reports: one per job, one per leg of a single-axis matrix."""
    names = set()
    for key, block in jobs(path).items():
        text = "\n".join(block)
        m = re.search(r"^    name:\s*(.+?)\s*$", text, re.M)
        name = m.group(1).strip("\"'") if m else key
        if "${{" not in name and not re.search(r"^\s+matrix:", text, re.M):
            names.add(name)
            continue
        axis = re.search(r"^\s+matrix:\s*\n\s+(\w+):\s*\[(.*?)\]\s*$", text, re.M)
        tmpl = re.fullmatch(r"([^$]*)\$\{\{\s*matrix\.(\w+)\s*\}\}([^$]*)", name)
        if not (axis and tmpl and tmpl.group(2) == axis.group(1)):
            sys.exit(f"{path}: job `{key}` has a matrix or a name expression this script cannot "
                     "expand; teach it, or its check names drift unseen")
        names |= {tmpl.group(1) + leg.strip() + tmpl.group(3) for leg in axis.group(2).split(",")}
    return names


def need(path):
    """automerge.yml's `need='[...]'` line, which must be there exactly once."""
    found = re.findall(r"^\s*need='(\[.*\])'\s*$", open(path).read(), re.M)
    if len(found) != 1:
        sys.exit(f"{path}: expected one need='[...]' line, found {len(found)}")
    return set(json.loads(found[0]))


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__.splitlines()[-1])
    ci, merge, codeql = sys.argv[1:]
    gates, wanted = checks(ci) - EXEMPT, need(merge)
    known = gates | (checks(codeql) - EXEMPT)  # codeql.yml has its own main-only `cleanup-runs`
    problems = []
    if gates - wanted:
        problems.append(f"{ci}: job(s) {sorted(gates - wanted)} missing from {merge}'s `need` list: "
                        "a bot PR would merge while they are red (a job that never runs on a PR "
                        "belongs in EXEMPT in this script instead)")
    if wanted - known:
        problems.append(f"{merge}: `need` names {sorted(wanted - known)}, which no job in "
                        f"{ci} or {codeql} reports on a PR: every bot PR would wait forever")
    if problems:
        sys.exit("\n".join(problems))
    print(f"automerge `need` covers all {len(gates)} ci.yml jobs; every entry names a real check")


main()
