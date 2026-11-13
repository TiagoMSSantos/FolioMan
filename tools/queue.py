#!/usr/bin/env python3
"""(#1490) The queue: every pre-registered receipt in tests/ci-settings.yaml that no verdict has read yet.

Proposals outrunning verdicts is the failure the receipts keep naming, and a plan that misses a verdict recorded in
place re-proposes a closed question. This lists what is waiting, so a round starts from the queue. A receipt block is
a `# (#N)` line and the comment lines under it, the (#1294) lint's own definition. A block is PRE-REGISTERED when it
carries `PRE-REGISTERED <date>` (a `retired:` note that quotes one is not), and it names its verdict rows as
`Row: (#M)`, `row (#M)` or `Rows (#M)`. It is READ when any of these holds:
  - a line of its own block opens with VERDICT or RESULT, or says `graded <date>` (a verdict recorded in place);
  - a later block carries the same id and is not itself a pre-registration;
  - a block whose first line says VERDICT, RESULT(S), graded <date> or SHIPPED names (#N) on that line, alone or
    inside a range such as `(#1242)-(#1245)`;
  - a block it names as a row exists and its first line says one of those words.
Everything else is OPEN and printed with its date and the start of its title. Receipts before (#1291), where the
(#1294) lint starts, kept no single convention, so `--since N` (default 1291) sets where the list starts.

Run: python3 -I tools/queue.py [path] [--since N]   (default: tests/ci-settings.yaml beside this tool)
     python3 -I tools/queue.py --selftest
"""
import os, re, sys

HEAD = re.compile(r"^\s*# \(#(\d+)\)\s*(.*)$")
PRE = re.compile(r"PRE-REGISTERED (\d{4}-\d{2}-\d{2})")
ROWS = re.compile(r"\b[Rr]ows?:? \(#(\d+)\)")
DONE_HEAD = re.compile(r"\b(?:VERDICT|RESULTS?|graded \d{4}-\d{2}-\d{2}|SHIPPED)\b")
DONE_LINE = re.compile(r"^\s*#\s+(?:VERDICT|RESULTS?)\b|\bgraded \d{4}-\d{2}-\d{2}")
CITE = re.compile(r"\(#(\d+)\)(?:\s*-\s*\(#(\d+)\))?")


def blocks(text):
    """[(id, header text, body lines)] in file order; a block runs over the comment lines under its header."""
    out, cur = [], None
    for line in text.splitlines():
        m = HEAD.match(line)
        if m:
            cur = (int(m.group(1)), m.group(2), [])
            out.append(cur)
        elif cur is not None and line.lstrip().startswith("#"):
            cur[2].append(line)
        else:
            cur = None
    return out


def queue(text, since=0):
    """[(id, date, title, rows)] of the pre-registered receipts from `since` on that no verdict reads, in id order."""
    bs = blocks(text)
    done_heads = {n for n, h, _ in bs if DONE_HEAD.search(h)}
    cited = set()
    for n, h, _ in bs:
        if DONE_HEAD.search(h):
            for a, b in CITE.findall(h):
                cited.update(range(int(a), int(b or a) + 1))
    out = []
    for i, (n, head, body) in enumerate(bs):
        whole = "\n".join([head] + body)
        pre = PRE.search(whole)
        if n < since or not pre or head.startswith("retired:") or n in [o[0] for o in out]:
            continue
        rows = [int(r) for r in ROWS.findall(whole)]
        later = any(m == n and not PRE.search("\n".join([h] + b)) for m, h, b in bs[i + 1:])
        if any(DONE_LINE.search(l) for l in body) or later or n in cited or any(r in done_heads for r in rows):
            continue
        out.append((n, pre.group(1), head, rows))
    return sorted(out)


def selftest():
    text = "\n".join([
        "# (#10) A, PRE-REGISTERED 2026-11-01 before any run. Row: (#11).",
        "#   more",
        "# (#11) VERDICT, 2026-11-02, on (#10): KEEP.",
        "key: 1",
        "# (#12) B, PRE-REGISTERED 2026-11-03. Row: (#13).",
        "# (#14) C, PRE-REGISTERED 2026-11-03, record only.",
        "# RESULT, 2026-11-04: read in place.",
        "x: 2",
        "# (#15) D, PRE-REGISTERED 2026-11-05. Rows (#16).",
        "y: 3",
        "# (#17) VERDICT, 2026-11-06, on (#15): read by citation.",
        "# (#18) E, PRE-REGISTERED 2026-11-07. Row: (#19).",
        "z: 4",
        "# (#19) SHIPPED 2026-11-08 on the owner's word under (#18).",
        "# (#20) a note that mentions PRE-REGISTERED nothing",
        "# (#21) F, PRE-REGISTERED 2026-11-09, no row named.",
        "# (#22) G, PRE-REGISTERED 2026-11-09. The VERDICT words in a rule do not read it.",
        "k: 5",
        "# (#22) its result, under the same id.",
        "# (#23) H, PRE-REGISTERED 2026-11-09.",
        "# (#24) I, PRE-REGISTERED 2026-11-09.",
        "# (#25) RESULTS OF (#23)-(#24), 2026-11-10.",
        "  # (#26) retired: old_knob: 0  # PRE-REGISTERED 2026-10-01 and refused",
        "# (#27) J, PRE-REGISTERED 2026-11-11, with its verdict on a line of its own block:",
        "# VERDICT: closed.",
    ])
    got = queue(text)
    assert [n for n, *_ in got] == [12, 21], got
    assert [n for n, *_ in queue(text, since=13)] == [21], "since skips the older ones"
    assert got[0][1] == "2026-11-03" and got[0][3] == [13], got[0]
    assert blocks("# (#1) a\n#  b\nk: v\n#  orphan\n")[0][2] == ["#  b"], "a block ends at the first non-comment line"
    assert queue("") == []
    print("queue selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    args = sys.argv[1:]
    since = 1291
    if "--since" in args:
        k = args.index("--since")
        since = int(args[k + 1])
        del args[k : k + 2]
    path = args[0] if args else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "ci-settings.yaml")
    with open(path, encoding="utf-8") as f:
        text = f.read()
    q = queue(text, since)
    print(f"{len(q)} pre-registered receipt(s) from (#{since}) on with no verdict read, by id:")
    for n, date, head, rows in q:
        title = head if len(head) <= 96 else head[:95] + "…"
        print(f"  (#{n}) {date}  {title}" + (f"  [rows {' '.join(f'(#{r})' for r in rows)}]" if rows else ""))


if __name__ == "__main__":
    main()
