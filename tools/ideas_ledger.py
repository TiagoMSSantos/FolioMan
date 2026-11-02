#!/usr/bin/env python3
"""Write docs/IDEAS.md: one row per buy-rule knob, deleted knob and fund factor.

Run from anywhere: python3 -I tools/ideas_ledger.py

It reads the BuyHeuristic struct and the RETIRED list in src/config.rs, FUND_FACTORS in
src/commands/backtest.rs, the FundFactors field notes in src/core.rs, and the shipped values and their comments in tests/ci-settings.yaml.
Every status is mechanical, so the file is rewritten, never edited by hand. The backtest test
`ledger_covers_knobs` reds when a knob, deleted knob or factor has no row.
"""
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RECEIPT = re.compile(r"\(#(\d+)")
REOPEN = re.compile(r"[^.;]*\b(?:re-?ask|re-?open|new reason)[^.;]*", re.I)
OFF = {"false", "null", "~", "[]", "{}", "-1e9", "-1e9.0", "-1000000000", "-1000000000.0"}


def cell(text, width=110):
    text = " ".join(text.split()).replace("|", "\\|")
    return text if len(text) <= width else text[: width - 1].rstrip() + "…"


def receipts(lines):
    seen = []
    for n in (int(m) for line in lines for m in RECEIPT.findall(line)):
        if n not in seen:
            seen.append(n)
    if len(seen) > 6:
        seen = seen[:3] + seen[-3:]
    return " ".join(f"#{n}" for n in sorted(seen))


def reopen(lines):
    m = REOPEN.search(" ".join(line.strip().lstrip("#/ ") for line in lines))
    return cell(m.group(0).strip(), 90) if m else "new reason, see receipt"


def rust_items(body, item):
    """(name, doc lines) for each `item` match: the trailing // comment, else the comment block above."""
    out, above = [], []
    for line in body.splitlines():
        s = line.strip()
        if s.startswith("//"):
            above.append(s.lstrip("/ "))
            continue
        for name in item.findall(s):
            tail = s.split("//", 1)[1].strip() if "//" in s else ""
            out.append((name, [tail] if tail else above))
        if not s.startswith("#["):
            above = []
    return out


def ci_settings():
    """{knob: (value, comment lines around it)} for the buy_heuristic block of tests/ci-settings.yaml."""
    lines = (ROOT / "tests/ci-settings.yaml").read_text().splitlines()
    start = lines.index("buy_heuristic:")
    end = next(i for i in range(start + 1, len(lines)) if re.match(r"^[a-z_]+:", lines[i]))
    key = re.compile(r"^  ([a-z_0-9]+):(\s|$)")
    out = {}
    for i in range(start + 1, end):
        m = key.match(lines[i])
        if not m:
            continue
        value = lines[i][m.end() :].split("#", 1)[0].strip() or "list"
        after = []
        for line in lines[i + 1 : end]:
            if key.match(line):
                break
            after.append(line)
        before = []
        for line in reversed(lines[start + 1 : i]):
            if not line.strip().startswith("#"):
                break
            before.append(line)
        out[m.group(1)] = (value, [lines[i]] + after + before[::-1])
    return out, lines


def main():
    config = (ROOT / "src/config.rs").read_text()
    struct = config[config.index("pub struct BuyHeuristic {") :]
    struct = struct[: struct.index("\n}\n")]
    fields = rust_items(struct.split("{", 1)[1], re.compile(r"^pub (\w+):"))
    retired_src = config[config.index("const RETIRED") :]
    retired = re.findall(r'"(\w+)"', retired_src[: retired_src.index("];")])
    backtest = (ROOT / "src/commands/backtest.rs").read_text()
    factor_src = backtest[backtest.index("const FUND_FACTORS") :]
    factor_src = factor_src[factor_src.index("= [") + 3 : factor_src.index("];")]
    factors = rust_items(factor_src, re.compile(r'"(\w+)"'))
    core = (ROOT / "src/core.rs").read_text()
    ff = core[core.index("pub struct FundFactors {") :]
    ff_docs = dict(rust_items(ff[ff.index("{") + 1 : ff.index("\n}\n")], re.compile(r"^pub (\w+):")))
    knobs, ci_lines = ci_settings()

    weights = {}
    extra = knobs.get("growth_fund_extra", ("", []))[1]
    for line, nxt in zip(extra, extra[1:]):
        f, w = re.match(r"^\s*- factor:\s*(\w+)", line), re.match(r"^\s*weight:\s*(-?[\d.]+)", nxt)
        if f and w and float(w.group(1)) != 0:
            weights[f.group(1)] = w.group(1)
    tilt = knobs.get("growth_fund_factor", ("", []))[0].strip('"')
    tilt_w = knobs.get("growth_fund_weight", ("0", []))[0]
    if tilt and float(tilt_w or 0) != 0:
        weights.setdefault(tilt, tilt_w)

    rows = []
    for name, doc in fields:
        value, notes = knobs.get(name, (None, []))
        kind = "knob, backtest only" if any("[FOIL]" in d for d in doc) else "knob"
        if value is None:
            status = "DEFAULT"
        elif value in OFF:
            status = f"OFF ({value})"
        else:
            status = f"SHIPPED {cell(value, 24)}"
        again = reopen(notes) if status.startswith("OFF") else "—"
        rows.append((cell(" ".join(doc)), kind, name, status, receipts(doc + notes), again))
    for name in retired:
        word = re.compile(rf"\b{name}\b")
        notes = [line for line in ci_lines + config.splitlines() if word.search(line)]
        rows.append(("deleted after a measurement refused it", "deleted knob", name, "RETIRED", receipts(notes), reopen(notes)))
    for name, doc in factors:
        doc = ff_docs.get(name, []) + doc
        # a one-word name like `quality` is a common word, so only its backticked spelling counts
        word = re.compile(rf"\b{name}\b" if "_" in name else rf"`{name}`")
        mentions = [line for line in ci_lines if line.lstrip().startswith("#") and word.search(line)]
        status = f"SHIPPED weight {weights[name]}" if name in weights else "MEASURED"
        again = "—" if name in weights else reopen(doc + mentions)
        rows.append((cell(" ".join(doc)), "fund factor", name, status, receipts(doc + mentions), again))

    out = [
        "# Ideas ledger",
        "",
        "Every buy-rule knob, every deleted knob and every fund factor the backtest can score, with its state",
        "today. It answers \"was this tried?\" before a new round starts. Written by `tools/ideas_ledger.py` from",
        "the code and `tests/ci-settings.yaml`; re-run it after any knob, factor or receipt change, never edit it",
        "by hand.",
        "",
        "- **SHIPPED**: `tests/ci-settings.yaml` sets the knob to this value, or the factor carries this weight.",
        "- **OFF**: the knob is shipped at its off value. **DEFAULT**: the knob is not set there, so the code",
        "  default runs.",
        "- **RETIRED**: the knob was deleted after a measurement refused it. **MEASURED**: the factor is graded",
        "  in every fund sweep but carries no weight.",
        "- **receipt**: the change ids in the comments beside it. **re-open**: what would justify asking again,",
        "  quoted from the receipt where it says so.",
        "",
        "| id | idea | kind | name | status | receipt | re-open |",
        "|---|---|---|---|---|---|---|",
    ]
    out += [f"| {i} | {idea} | {kind} | `{name}` | {status} | {rec} | {again} |" for i, (idea, kind, name, status, rec, again) in enumerate(rows, 1)]
    (ROOT / "docs/IDEAS.md").write_text("\n".join(out) + "\n")
    print(f"docs/IDEAS.md: {len(rows)} rows ({len(fields)} knobs, {len(retired)} deleted, {len(factors)} factors)")


if __name__ == "__main__":
    main()
