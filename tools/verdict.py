#!/usr/bin/env python3
"""(#1440) Read a pre-registered receipt's verdict off its probe logs, with the bar applied as the receipt wrote it.

Each pending receipt of rounds 18-20 fixed its bar before any run. This tool is that bar as code, so the person
who runs the probe does not have to re-derive it from a paragraph: it prints the cells it read, each cell's pass
or fail, and the verdict word the receipt asks for. It decides nothing by itself: the owner's word ships.

Logs are `gh run view <id> --log > file` (or bare text). Rows, BOOK lines, bands and eras are read by
`tools/regrade_ruler.py`'s own parser, so both tools read a log the same way.

  gates    (#1435) the retired gates on honest bars. One control log (the shipped arm, no knobs) and one arm log
           (the grid). Per horizon, every arm row against the CONTROL's top-10 book: mean within 0.5, median within
           0.8, worst window within 0.8, RU1 (top 5 against names 6-10) at 50 % or more and not under the control's
           by more than one window in 40; then the era cells: the 2002-07 gap (control minus arm row 0) the row
           closes, and what it costs in >=2014 against arm row 0. (A) every cell at every horizon and >=2014 within
           1.0; (B) not (A) and the 2002-07 gap closed by half or more at every horizon that reads it; else (C).
               python3 -I tools/verdict.py gates --control control.log --arm arm.log
  rivals   (#1431) Occam's rows on the rivals table: (y) and (z) against "shipped (plain top-10)" per horizon. LOSS
           at a horizon = the mean or the median gap under -0.5, or both at or under -0.2, or the worst window more
           than 1.0 under shipped's; WIN = both gaps at +0.2 or more; else TIE. A row TIES when no horizon reads
           LOSS and both 12y and 8y were read.
               python3 -I tools/verdict.py rivals rivals.log
  head     (#1441) the head weight on the live-equivalent arm: 20y logs per lane and rung. A tilted rung (2, 4, 8)
           qualifies when its `S+inter never-sell` excess over index DCA beats the flat book's (rung 1) by +0.2 or
           more on mean AND median in EVERY lane, worst program no more than 0.5 under flat's. None qualifies ->
           1. Else the smallest qualifying rung whose mean gain is within 0.2 of the best one's in every lane.
               python3 -I tools/verdict.py head --lane pit 1=p1.log 2=p2.log 4=p4.log 8=p8.log --lane dead 1=d1.log 2=d2.log 4=d4.log 8=d8.log
  cap      (#1443) the sector cap on the live-equivalent arm, the (#557) bar: plain logs, one per cap and one
           control, each holding the 20y, 12y and 8y walks. A cap qualifies when, at every horizon, its top-10
           median excess is no more than 0.1 under the control's AND its worst window is not under the control's.
           Among qualifiers the largest smallest worst-window gain wins, a tie going to the larger cap; a winner
           that lifts no worst window is indistinguishable from off.
               python3 -I tools/verdict.py cap --control c.log --cap 3=k3.log 4=k4.log 5=k5.log
  payouts  (#1477) the Yahoo payout census: one control log (no knobs) and one arm log (knobs payout_guard=true), each
           with `payouts` in its args and the 20y, 12y and 8y walks. Prints the census lines of every walk, then each
           horizon's top-10 book in both. The (#1247) rule: QUESTION when the arm moves the top-10 mean or median by
           0.1 or more at any horizon, else RECORD (the Screen A/B's page rows are the rule's other half).
               python3 -I tools/verdict.py payouts --control c.log --arm a.log
  plant    (#1486) the ruler's eyesight at any horizon: the `top 10 plant X/yr` lines of a `rescore="plant"` log.
           Per horizon, each X's passing draws and the smallest X passing 16 of 20, read against (#1332)'s bands:
           at or under 1 -> CANDIDATE BAR; over 1 and at or under 4 -> FLOOR; nothing at 4 -> BLIND. Then, per X,
           the seeds every horizon in the log passes.
               python3 -I tools/verdict.py plant plant.log
  guard    (#1436) and any harm-guard grid: per horizon and row, the GUARD line's word and RU1-RU3 against row 0.
           `--row TEXT` names the row to decide (a substring of its JSON): SHIP when the guard reads PASS or INERT
           and no RU cell fails, at 12y and at 8y.
               python3 -I tools/verdict.py guard --row '"dividend_weight":0.17' grid.log
           (the row label is the grid line as `jq -c` printed it, so no space after the colon)

    python3 -I tools/verdict.py --selftest
"""
import argparse, math, os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from regrade_ruler import ERA_MIN, PREFIX, grade_row, parse_rows, verdicts  # noqa: E402  one parser for both tools

GROUP = re.compile(r"(?:##\[group\]|::group::)(backtest|rescore) (\d+)\b")
RIVALS_HDR = re.compile(r"── RIVALS \(#560\).*HOLD (\d+)y no-sell")
RIVAL_ROW = re.compile(
    r"^\s{2}(.{28}) n\s+(\d+)\s+excess (\S+) \(med (\S+)\) pts/yr\s+win\s+\d+%\s+worst (\S+)\s+p \S+\s+vs plain shipped (\S+)/(\S+) (?:AHEAD|-)"
)
DCA = re.compile(
    r"^\s*(\S+)\s+never-sell ([+-]\d+\.\d)%/yr\s+vs index DCA ([+-]\d+\.\d)%/yr\s+->\s+([+-]\d+\.\d) \(med ([+-]\d+\.\d)\) pts/yr\s+win (\d+)% of (\d+)\s+worst ([+-]\d+\.\d)"
)
GUARD_HDR = "── GUARD (#1339)/(#1341)"
GUARD_ROW = re.compile(
    r"^\s*row (.*): top-10 gain ([+-]\d+\.\d\d) edge ([+-]\d+\.\d\d) ; pond gain ([+-]\d+\.\d\d) edge ([+-]\d+\.\d\d) ; pool (\d+\.\d) % ; "
    r"\[(.*?)\] read on (\d+\.\d) % of scored stocks ; (\d+\.\d\d) top-10 name\(s\) moved a bucket -> (\w+)\s*$"
)
HOLD = re.compile(r"HOLD (\d+)y no-sell")
MEAN_BAND, MEDIAN_BAND, WORST_BAND = 0.5, 0.8, 0.8  # (#1333) as (#1390) and (#1426) read it on the control
RECENT_SLACK, GAP_SHARE = 1.0, 0.5  # (#1435) >=2014 against arm row 0; the share of the 2002-07 gap a row must close
TIE, LOSS_BOTH, WORST_SLACK = 0.5, 0.2, 1.0  # (#1431)
CAP_MEDIAN = 0.1  # (#557) how far a capped book's median may sit under the control's
STEP, HEAD_WORST = 0.2, 0.5  # (#1441) the (#584) C step, and how far a rung's worst program may sit under flat's
PLANT = re.compile(r"^\s*top (\d+) plant ([+-]?\d+\.\d)/yr: .* ; pass (\d+)/(\d+) seeds \[([\d, ]*)\]\s*$")
PLANT_PASS = 16  # (#1332) an edge passes on 16 of 20 draws
PAYOUT_MOVE = 0.1  # (#1477) the (#1247) rule: a top-10 mean or median moved this much at any horizon asks the owner
PAYOUT_LINE = re.compile(r"backtest: \(#1477\) (.*)$")
EPS = 1e-9


def clean(text):
    return [PREFIX.sub("", l, count=1) for l in text.splitlines()]


def chunks(text):
    """A log cut at the workflow's `backtest N` / `rescore N` group lines: [(kind, years, text)], or one
    ("whole", None, text) when the log carries no group line (a bare or trimmed log)."""
    out, cur = [], None
    for line in clean(text):
        m = GROUP.search(line)
        if m:
            cur = (m.group(1), int(m.group(2)), [])
            out.append(cur)
        elif cur:
            cur[2].append(line)
    if not out:
        return [("whole", None, text)]
    return [(k, y, "\n".join(ls)) for k, y, ls in out]


def graded(text, label):
    """{years: [(row label, grade)]} for one log: its rescore rows where it has any (row 0 first), else the plain
    walk of each horizon as one row."""
    parts = chunks(text)
    resc = [p for p in parts if p[0] == "rescore"]
    use = resc or [p for p in parts if p[0] in ("backtest", "whole")]
    out = {}
    for _, _, body in use:
        for r in parse_rows(body, label):
            if r["label"].startswith("rival:") or not r["books"] or r["years"] is None:
                continue
            out.setdefault(r["years"], []).append((r["label"], grade_row(r)))
    return out


def ru1_ok(g, c):
    """RU1 as `regrade_ruler.verdicts` reads it; True where neither row holds a window to read."""
    v = verdicts(g, c)["RU1"]
    return True if v is None else v


def gate_cells(row, control, row0):
    """One arm row at one horizon against the control and against arm row 0: (cells, closed share or None,
    >=2014 cost or None). A cell is (name, value, limit, ok)."""
    cells = [
        ("mean", row["mean"], control["mean"] - MEAN_BAND, row["mean"] >= control["mean"] - MEAN_BAND - EPS),
        ("median", row["median"], control["median"] - MEDIAN_BAND, row["median"] >= control["median"] - MEDIAN_BAND - EPS),
        ("worst", row["worst"], control["worst"] - WORST_BAND, row["worst"] >= control["worst"] - WORST_BAND - EPS),
        ("RU1", row["ru1_pct"], 50.0, ru1_ok(row, control)),
    ]
    era = lambda g, k: g["eras"][k][2] if k in g["eras"] and g["eras"][k][0] >= ERA_MIN else None
    c07, r07, z07 = era(control, "2002-07"), era(row, "2002-07"), era(row0, "2002-07")
    closed = (r07 - z07) / (c07 - z07) if None not in (c07, r07, z07) and c07 - z07 > EPS else None
    r14, z14 = era(row, ">=2014"), era(row0, ">=2014")
    cost = z14 - r14 if None not in (r14, z14) else None
    return cells, closed, cost


def gates(control_text, arm_text, out=print):
    """(#1435): the read of every arm row and the block's verdict, (A), (B) or (C), with the row that earns it."""
    control, arm = graded(control_text, "control"), graded(arm_text, "arm")
    years = sorted(set(control) & set(arm), reverse=True)
    if not years:
        out("no horizon is in both logs: nothing to read")
        return "UNREAD", None
    labels = [l for l, _ in arm[years[0]]]
    reads = {}
    for y in years:
        c, rows = control[y][0][1], dict(arm[y])
        row0 = arm[y][0][1]
        out(f"── {y}y: control mean {c['mean']:+.1f} median {c['median']:+.1f} worst {c['worst']:+.1f} RU1 {c['ru1_wins']}/{c['ru1_n']} ──")
        for label in labels:
            if label not in rows:
                continue
            cells, closed, cost = gate_cells(rows[label], c, row0)
            reads.setdefault(label, []).append((all(k[3] for k in cells), closed, cost))
            txt = "  ".join(f"{n} {'-' if v is None else format(v, '+.1f')} (needs {lim:+.1f}) {'ok' if ok else 'FAIL'}" for n, v, lim, ok in cells)
            era = f"2002-07 gap closed {'n/a' if closed is None else format(closed * 100, '.0f') + ' %'}; >=2014 cost {'n/a' if cost is None else format(cost, '+.1f')}"
            out(f"  row {label[:48]:<48} {txt}  | {era}")
    best, who = "C", None
    for label in labels[1:]:  # row 0 is the arm with no gate: it is the baseline, not a candidate
        r = reads.get(label, [])
        if len(r) != len(years):
            continue
        in_band = all(ok for ok, _, _ in r) and all(cost is None or cost <= RECENT_SLACK + EPS for _, _, cost in r)
        closes = [c for _, c, _ in r if c is not None]
        half = bool(closes) and all(c >= GAP_SHARE - EPS for c in closes)
        grade = "A" if in_band else ("B" if half else "C")
        if grade < best:
            best, who = grade, label
    words = {"A": "RECOVERY: one owner question, honest bars WITH this gate", "B": "PARTIAL: recorded, the remainder is the next question", "C": "NOTHING: recorded, both knobs back to RETIRED"}
    out(f"VERDICT ({'/'.join(str(y) + 'y' for y in years)}): ({best}) {words[best]}" + (f" — row {who}" if who else ""))
    return best, who


def num(s):
    """A printed number, None for NaN, inf or anything that is not one (an empty rivals row prints those)."""
    try:
        x = float(s)
    except ValueError:
        return None
    return x if math.isfinite(x) else None


def rival_table(text):
    """{years: {id: (mean, median, worst, gap mean, gap median)}} from the first RIVALS table of each horizon. The
    id is the letter in the row's `(x)` tag, or `shipped`."""
    out, years, done = {}, None, set()
    for line in clean(text):
        h = RIVALS_HDR.search(line)
        if h:
            if years is not None:
                done.add(years)
            years = int(h.group(1))
            continue
        m = RIVAL_ROW.match(line)
        if not m or years is None or years in done:
            continue
        name = m.group(1).strip()
        rid = "shipped" if name.startswith("shipped") else name[1:name.index(")")] if name.startswith("(") and ")" in name else name
        out.setdefault(years, {}).setdefault(rid, tuple(num(m.group(i)) for i in (3, 4, 5, 6, 7)))
    return out


def rival_word(row, shipped):
    """LOSS, WIN or TIE for one rivals row at one horizon, per (#1431)."""
    _, _, worst, dm, dmed = row
    if None in (worst, dm, dmed) or shipped[2] is None:
        return "UNREAD"
    if dm < -TIE - EPS or dmed < -TIE - EPS or (dm <= -LOSS_BOTH + EPS and dmed <= -LOSS_BOTH + EPS) or worst < shipped[2] - WORST_SLACK - EPS:
        return "LOSS"
    if dm >= LOSS_BOTH - EPS and dmed >= LOSS_BOTH - EPS:
        return "WIN"
    return "TIE"


def rivals(text, out=print, ids=("y", "z")):
    """(#1431): each Occam row's word per horizon and overall; other rows print as information."""
    table = rival_table(text)
    verdict = {}
    for y in sorted(table, reverse=True):
        ship = table[y].get("shipped")
        if not ship:
            continue
        out(f"── {y}y rivals: shipped excess {ship[0]:+.1f} (med {ship[1]:+.1f}) worst {ship[2]:+.1f} ──")
        for rid, row in table[y].items():
            if rid == "shipped" or None in row:  # an empty row (n 0) prints NaN and inf: nothing to read
                continue
            w = rival_word(row, ship)
            mark = " <- (#1431)" if rid in ids else " (information: the same bar, no decision)"
            out(f"  ({rid}) excess {row[0]:+.1f} (med {row[1]:+.1f}) worst {row[2]:+.1f}  vs shipped {row[3]:+.1f}/{row[4]:+.1f}  -> {w}{mark}")
            if rid in ids:
                verdict.setdefault(rid, {})[y] = w
    result = {}
    for rid in ids:
        ws = verdict.get(rid, {})
        if 12 not in ws or 8 not in ws or "UNREAD" in ws.values():
            result[rid] = "UNREAD"
        elif "LOSS" in ws.values():
            result[rid] = "LOSS"
        elif all(w == "WIN" for w in ws.values()):
            result[rid] = "WIN"
        else:
            result[rid] = "TIE"
    if result.get("z") in ("TIE", "WIN"):
        say = "owner question: growth_ranker two_terms"
    elif result.get("y") in ("TIE", "WIN"):
        say = "owner question: growth_ranker equal_terms"
    elif "UNREAD" in result.values():
        say = "needs the 12y and 8y tables in one log"
    else:
        say = "the fitted weights carry information; recorded, nothing moves"
    out("VERDICT: " + ", ".join(f"({k}) {v}" for k, v in result.items()) + f" -> {say}")
    return result


def dca(text, lane="S+inter"):
    """(excess mean, median, worst, programs) of one DCA lane's never-sell row against index DCA, or None."""
    for line in clean(text):
        m = DCA.match(line)
        if m and m.group(1) == lane:
            return float(m.group(4)), float(m.group(5)), float(m.group(8)), int(m.group(7))
    return None


def head(lanes, out=print):
    """(#1441): `lanes` = {lane: {rung: log text}}. Returns the rung the bar ships, None when a row is missing.
    A tilted rung QUALIFIES when its `S+inter never-sell` excess over index DCA beats flat's (rung 1) by +0.2 or
    more on mean AND median in every lane, with its worst program no more than 0.5 under flat's. None qualifies
    -> 1. Else the best rung is the one with the largest smallest-lane mean gain, and the bar ships the SMALLEST
    qualifying rung whose mean gain is within 0.2 of the best's in every lane: a doubling that adds under the
    ruler's own step buys concentration, not return."""
    read = {lane: {r: dca(t) for r, t in rungs.items()} for lane, rungs in lanes.items()}
    for lane, rungs in read.items():
        for r in sorted(rungs):
            v = rungs[r]
            out(f"  {lane:<6} head_weight {r:<2} S+inter never-sell vs index DCA " + ("no row" if v is None else f"{v[0]:+.1f} (med {v[1]:+.1f}) worst {v[2]:+.1f} over {v[3]} programs"))
    rungs = sorted(set.intersection(*(set(v) for v in read.values()))) if read else []
    if len(rungs) < 2 or rungs[0] != 1 or any(read[lane][r] is None for lane in read for r in rungs):
        out("VERDICT: UNREAD — every lane needs a readable log for rung 1 and at least one rung above it")
        return None
    gain = lambda lane, r: (read[lane][r][0] - read[lane][1][0], read[lane][r][1] - read[lane][1][1], read[lane][r][2] - read[lane][1][2])
    qualifies = []
    for r in rungs[1:]:
        g = {lane: gain(lane, r) for lane in read}
        ok = all(dm >= STEP - EPS and dmed >= STEP - EPS and dw >= -HEAD_WORST - EPS for dm, dmed, dw in g.values())
        out(f"  rung {r} over flat: " + "; ".join(f"{lane} mean {dm:+.1f} median {dmed:+.1f} worst {dw:+.1f}" for lane, (dm, dmed, dw) in g.items()) + (" -> qualifies" if ok else " -> does not"))
        if ok:
            qualifies.append(r)
    if not qualifies:
        out("VERDICT: no tilted rung beats the flat book by the bar -> head_weight 1 (flat)")
        return 1
    floor = lambda r: min(gain(lane, r)[0] for lane in read)
    best = max(qualifies, key=lambda r: (floor(r), -r))
    ship = next(r for r in qualifies if all(gain(lane, best)[0] - gain(lane, r)[0] <= STEP + EPS for lane in read))
    out(f"VERDICT: qualifying rungs {qualifies}, best {best}; the smallest within {STEP} of it in every lane -> head_weight {ship}")
    return ship


def top10s(text):
    """{years: (mean, median, worst)} of the growth lane's `top-10 book` line in each plain walk of a log."""
    out = {}
    for kind, _, body in chunks(text):
        if kind == "rescore":
            continue
        for r in parse_rows(body, "walk"):
            if r["top10"] and r["years"] is not None and not r["label"].startswith("rival:"):
                out.setdefault(r["years"], r["top10"])
    return out


def cap(control_text, caps, out=print):
    """(#1443): `caps` = {k: log text}. Returns (word, k): ("SHIP?", k) for a cap that lifts a worst window, ("KEEP", None)
    when no cap qualifies or the winner lifts none, ("UNREAD", None) when a horizon is missing."""
    control = top10s(control_text)
    years = sorted(control, reverse=True)
    reads = {k: top10s(t) for k, t in caps.items()}
    if not years or any(set(r) != set(control) for r in reads.values()):
        out("VERDICT: UNREAD — the control and every cap log must hold the same horizons")
        return "UNREAD", None
    out("  control " + "  ".join(f"{y}y median {control[y][1]:+.1f} worst {control[y][2]:+.1f}" for y in years))
    gains = {}
    for k in sorted(reads):
        r = reads[k]
        ok = all(r[y][1] >= control[y][1] - CAP_MEDIAN - EPS and r[y][2] >= control[y][2] - EPS for y in years)
        gain = min(r[y][2] - control[y][2] for y in years)
        out(f"  cap {k}   " + "  ".join(f"{y}y median {r[y][1]:+.1f} worst {r[y][2]:+.1f}" for y in years) + (f"  -> qualifies, smallest worst-window gain {gain:+.1f}" if ok else "  -> does not qualify"))
        if ok:
            gains[k] = gain
    if not gains:
        out("VERDICT: KEEP 0 — no cap qualifies")
        return "KEEP", None
    best = max(gains, key=lambda k: (gains[k], k))
    if gains[best] <= EPS:
        out(f"VERDICT: KEEP 0 — cap {best} wins the bar but lifts no worst window: indistinguishable from off, as (#557) read")
        return "KEEP", None
    out(f"VERDICT: SHIP? cap {best} lifts every worst window by {gains[best]:+.1f} or more with no median down more than {CAP_MEDIAN}: a RISK choice, the owner's word")
    return "SHIP?", best


def payouts(control_text, arm_text, out=print):
    """(#1477): the census lines of each walk, each horizon's top-10 book in the control and the arm, and the (#1247)
    rule: ("QUESTION", [horizons that moved]), ("RECORD", []) or ("UNREAD", [])."""
    for kind, years, body in chunks(control_text):
        for line in body.splitlines():
            m = PAYOUT_LINE.search(line)
            if m:
                out(f"  {'' if years is None else str(years) + 'y '}{m.group(1)}")
    control, arm = top10s(control_text), top10s(arm_text)
    years = sorted(control, reverse=True)
    if not years or set(control) != set(arm):
        out("VERDICT: UNREAD — the control and the arm must hold the same horizons")
        return "UNREAD", []
    moved = []
    for y in years:
        (cm, cmed, cw), (am, amed, aw) = control[y], arm[y]
        big = abs(am - cm) >= PAYOUT_MOVE - EPS or abs(amed - cmed) >= PAYOUT_MOVE - EPS
        out(f"  {y}y top-10 mean {cm:+.1f} -> {am:+.1f}, median {cmed:+.1f} -> {amed:+.1f}, worst {cw:+.1f} -> {aw:+.1f}" + ("  MOVED" if big else ""))
        if big:
            moved.append(y)
    if moved:
        out(f"VERDICT: QUESTION — the guard moves the top-10 book by {PAYOUT_MOVE} or more at {', '.join(f'{y}y' for y in moved)}: one owner question")
        return "QUESTION", moved
    out(f"VERDICT: RECORD — no horizon moves by {PAYOUT_MOVE}; a top-10 page row the Screen A/B moves is the other trigger")
    return "RECORD", []


def plant_rows(text):
    """{years: {X: (passes, draws, seeds)}} from each horizon's `top 10 plant` lines (the top-10 book only)."""
    out = {}
    for _, years, body in chunks(text):
        for line in body.splitlines():
            m = PLANT.match(line)
            if m and m.group(1) == "10" and years is not None:
                seeds = frozenset(int(x) for x in m.group(5).replace(" ", "").split(",") if x)
                out.setdefault(years, {})[float(m.group(2))] = (int(m.group(3)), int(m.group(4)), seeds)
    return out


def plant(text, out=print):
    """(#1486): {years: (word, smallest passing X or None)}, the word from (#1332)'s bands; {} when nothing reads."""
    rows = plant_rows(text)
    if not rows:
        out("VERDICT: UNREAD — no `top 10 plant` line under a `rescore N` group")
        return {}
    words = {}
    for y in sorted(rows, reverse=True):
        xs = sorted(rows[y].items())
        passing = [x for x, (p, _, _) in xs if p >= PLANT_PASS]
        best = passing[0] if passing else None
        word = "BLIND" if best is None else ("CANDIDATE BAR" if best <= 1 + EPS else "FLOOR")
        words[y] = (word, best)
        cells = "  ".join(f"X {x:+.1f} {p}/{n}" for x, (p, n, _) in xs)
        out(f"  {y}y  {cells}  -> smallest passing X {'none' if best is None else format(best, '+.1f')}: {word}")
    if len(rows) > 1:
        for x in sorted(set.intersection(*(set(r) for r in rows.values()))):
            both = sorted(frozenset.intersection(*(rows[y][x][2] for y in rows)))
            out(f"  X {x:+.1f}: seeds every horizon passes {both} ({len(both)})")
    return words


def guard_rows(text):
    """{years: {row label: guard word}} from each horizon's GUARD block."""
    out, years, on = {}, None, False
    for line in clean(text):
        h = HOLD.search(line)
        if h:
            years = int(h.group(1))
        if GUARD_HDR in line:
            on = True
            continue
        m = GUARD_ROW.match(line) if on else None
        if m and years is not None:
            out.setdefault(years, {})[m.group(1)] = m.group(10)
        elif on and line.startswith("──"):
            on = False
    return out


def guard(text, want=None, out=print):
    """Per horizon and row: the GUARD word and RU1-RU3 against row 0. With `want`, SHIP or KEEP for that row."""
    words, rows = guard_rows(text), graded(text, "grid")
    decided = {}
    for y in sorted(rows, reverse=True):
        base = rows[y][0][1]
        out(f"── {y}y: row 0 mean {base['mean']:+.1f} median {base['median']:+.1f} worst {base['worst']:+.1f} ──")
        for label, g in rows[y][1:]:
            v = verdicts(g, base)
            ru = {k: v[k] for k in ("RU1", "RU2", "RU3")}
            word = words.get(y, {}).get(label, "no GUARD line")
            ok = word in ("PASS", "INERT") and all(x is not False for x in ru.values())
            out(f"  row {label[:60]:<60} guard {word:<12} " + " ".join(f"{k} {'-' if x is None else ('pass' if x else 'FAIL')}" for k, x in ru.items()) + f"  mean {g['mean']:+.1f} median {g['median']:+.1f} worst {g['worst']:+.1f}")
            if want and want in label:
                decided[y] = ok
    if want:
        if 12 not in decided or 8 not in decided:
            out(f"VERDICT: UNREAD — no row holding {want!r} at 12y and at 8y")
            return "UNREAD"
        word = "SHIP" if all(decided.values()) else "KEEP"
        out(f"VERDICT: {word} for the row holding {want!r} ({', '.join(f'{y}y ' + ('no harm' if ok else 'harm') for y, ok in sorted(decided.items(), reverse=True))})")
        return word
    return None


def _book(bucket, excess, top, rest):
    names = " ".join(f"T{i} {top:+d}%" for i in range(5)) + " " + " ".join(f"U{i} {rest:+d}%" for i in range(5))
    return f"  BOOK b{bucket} {excess:+.1f} pts/yr | {names}"


def _plain(years, per_era, top=120, rest=80):
    """A plain walk at one horizon: per_era = {era start year: excess}, five buckets an era, top 5 ahead of 6-10."""
    lines = [f"##[group]backtest {years} universe fund pit", f"── GROWTH: top-10 book, HOLD {years}y no-sell ──"]
    for start, x in per_era.items():
        lines += [_book((start + i) * 2, x, top, rest) for i in range(5)]
    return "\n".join(lines + ["##[endgroup]"])


def _rescore(years, rows):
    """A rescore block: rows = [(label, per_era, top, rest)], row 0 first."""
    lines = [f"##[group]rescore {years}"]
    for label, per_era, top, rest in rows:
        lines += [f"══ rescore row {label} ══", f"── GROWTH: top-10 book, HOLD {years}y no-sell ──"]
        for start, x in per_era.items():
            lines += [_book((start + i) * 2, x, top, rest) for i in range(5)]
    return "\n".join(lines + ["##[endgroup]"])


def selftest():
    quiet = lambda *_: None
    eras = lambda a, b, c, d: {1996: a, 2002: b, 2008: c, 2014: d}
    control = _plain(12, eras(2.0, 4.0, 4.0, 1.5)) + "\n" + _plain(8, eras(2.0, 4.0, 5.0, 1.5))
    row0 = eras(2.4, 2.0, 4.2, 6.5)
    fin = ('{"growth_refuse_financials":true}', eras(2.4, 3.2, 4.2, 6.4), 120, 80)
    dog = ('{"growth_refuse_sector_top_dog":true}', eras(2.4, 3.8, 4.2, 2.0), 120, 80)
    arm = _rescore(12, [("{}", row0, 80, 120), fin, dog]) + "\n" + _rescore(8, [("{}", row0, 80, 120), fin, dog])
    g = graded(control, "c")
    assert sorted(g) == [8, 12] and abs(g[12][0][1]["mean"] - 2.875) < 1e-9 and g[12][0][1]["ru1_pct"] == 100.0, g[12][0][1]
    a = graded(arm, "a")
    assert [l for l, _ in a[12]] == ["{}", fin[0], dog[0]] and a[12][0][1]["ru1_pct"] == 0.0, "row 0's top five trail its 6-10"
    # financials: mean 4.05 >= 2.875 - 0.5, median, worst and RU1 hold, 2002-07 closes (3.2 - 2.0) / (4.0 - 2.0) = 60 %,
    # >=2014 costs 0.1: (A). The top-dog row closes 90 % but costs 4.5 in >=2014: (B) at best.
    assert gates(control, arm, quiet) == ("A", fin[0])
    weak = ('{"growth_refuse_financials":true}', eras(2.4, 2.4, 4.2, 6.4), 120, 80)  # closes 20 % of the gap
    lone = lambda r: _rescore(12, [("{}", row0, 80, 120), r]) + "\n" + _rescore(8, [("{}", row0, 80, 120), r])
    assert gates(control, lone(dog), quiet) == ("B", dog[0])
    assert gates(control, lone(weak), quiet)[0] == "A", "in the band on every cell is (A) whatever the gap share"
    sunk = ('{"growth_refuse_financials":true}', eras(-3.0, 2.2, -3.0, 6.4), 80, 120)  # mean and worst out, RU1 fails, gap 10 %
    assert gates(control, lone(sunk), quiet) == ("C", None)
    half = ('{"growth_refuse_financials":true}', eras(-3.0, 3.0, -3.0, 6.4), 80, 120)  # out of the band, gap closed 50 %
    assert gates(control, lone(half), quiet) == ("B", half[0])
    assert gates("nothing here", arm, quiet)[0] == "UNREAD"
    # rivals: the table's own columns
    hdr = lambda y: f"── RIVALS (#560): equal-weight top-10 books on one pond, HOLD {y}y no-sell, excess vs the index ──"
    row = lambda name, mean, med, worst, dm, dmed: f"  {name:<28} n  67  excess {mean:+.1f} (med {med:+.1f}) pts/yr  win 80%  worst {worst:+.1f}  p 0.100  vs plain shipped {dm:+.1f}/{dmed:+.1f} -  vs (j) +0.0/+0.0 -  p95 -"
    tbl = lambda y, yrow, zrow: "\n".join([hdr(y), row("shipped (plain top-10)", 2.8, 4.2, -14.4, 0.0, 0.0), "  (b) quality (ROE fallback)   n   0  excess -0.0 (med NaN) pts/yr  win 0%  worst +inf  p n/a    vs plain shipped -2.8/NaN -  vs (j) -3.0/NaN -  p95 -", row("(j) 5 shipped + 5 momentum", 3.0, 3.8, -10.8, 0.2, -0.4), yrow, zrow])
    y_loss, z_tie = row("(y) equal-weight armed terms", 1.3, 2.3, -10.8, -1.5, -1.9), row("(z) trend + accel only", 2.8, 3.5, -10.8, 0.0, -0.4)
    t = rival_table(tbl(12, y_loss, z_tie))
    assert t[12]["y"] == (1.3, 2.3, -10.8, -1.5, -1.9) and t[12]["shipped"][2] == -14.4 and t[12]["b"][1] is None, t
    both = tbl(12, y_loss, z_tie) + "\n" + tbl(8, y_loss, z_tie)
    assert rivals(both, quiet) == {"y": "LOSS", "z": "TIE"}
    assert rivals(tbl(12, y_loss, z_tie), quiet) == {"y": "UNREAD", "z": "UNREAD"}, "one horizon is not a read"
    assert rival_word((0, 0, -10.8, -0.2, -0.2), (2.8, 4.2, -14.4, 0, 0)) == "LOSS", "both gaps at -0.2 is the (#560) loss"
    assert rival_word((0, 0, -10.8, -0.5, 0.1), (2.8, 4.2, -14.4, 0, 0)) == "TIE" and rival_word((0, 0, -15.5, 0.3, 0.3), (2.8, 4.2, -14.4, 0, 0)) == "LOSS"
    assert rival_word((0, 0, -10.8, 0.2, 0.2), (2.8, 4.2, -14.4, 0, 0)) == "WIN"
    # head: (#584) C's own pit and stress numbers stop at 2 on this bar (2 -> 4 reads +0.3/+0.1 and +0.2/+0.0)
    line = lambda lane, d, m: f"  {lane:<7} never-sell +9.0%/yr  vs index DCA +5.4%/yr  ->  {d:+.1f} (med {m:+.1f}) pts/yr   win 100% of 51   worst +0.2   OOS +2.4/+3.2"
    log = lambda d, m: "\n".join([line("top-10", 2.8, 3.1), line("S+inter", d, m), line("SIZED", 3.3, 3.3)])
    assert dca(log(3.9, 3.2)) == (3.9, 3.2, 0.2, 51) and dca(log(3.9, 3.2), "SIZED") == (3.3, 3.3, 0.2, 51) and dca("x") is None
    # the shipped arm's own (#584) C rows: only 8 beats flat on both medians in both lanes (stress 2 and 4 read +0.0)
    shipped = {"pit": {1: log(3.1, 2.6), 2: log(3.3, 2.9), 4: log(3.6, 3.0), 8: log(3.9, 3.2)}, "stress": {1: log(5.8, 6.5), 2: log(6.0, 6.5), 4: log(6.2, 6.5), 8: log(6.4, 6.8)}}
    assert head(shipped, quiet) == 8, "the bar reproduces the shipped choice on the numbers that shipped it"
    flat = {"pit": {1: log(3.0, 3.0), 2: log(3.1, 3.3), 4: log(3.1, 3.0), 8: log(2.9, 3.1)}, "stress": {1: log(5.0, 5.0), 2: log(5.3, 5.0), 4: log(5.2, 5.1), 8: log(5.1, 5.0)}}
    assert head(flat, quiet) == 1, "no rung clears +0.2 on mean and median in both lanes"
    near = {"pit": {1: log(3.0, 3.0), 2: log(3.2, 3.2), 4: log(3.5, 3.4), 8: log(3.6, 3.7)}, "stress": {1: log(5.0, 5.0), 2: log(5.3, 5.2), 4: log(5.5, 5.4), 8: log(5.7, 5.6)}}
    assert head(near, quiet) == 4, "8 is best, 4 is within 0.2 of it in both lanes, 2 is not (pit +0.2 against +0.6)"
    far = {"pit": {1: log(3.0, 3.0), 2: log(3.2, 3.2), 4: log(3.3, 3.3), 8: log(3.9, 3.7)}, "stress": {1: log(5.0, 5.0), 2: log(5.2, 5.2), 4: log(5.3, 5.3), 8: log(5.8, 5.6)}}
    assert head(far, quiet) == 8, "nothing smaller is within 0.2 of 8"
    deep = lambda d, m, w: line("S+inter", d, m).replace("worst +0.2", f"worst {w:+.1f}")
    risky = {"pit": {1: deep(3.0, 3.0, 0.2), 8: deep(3.9, 3.7, -0.4)}, "stress": {1: deep(5.0, 5.0, 0.2), 8: deep(5.8, 5.6, 0.2)}}
    assert head(risky, quiet) == 1, "a worst program 0.6 under flat's disqualifies the rung"
    assert head({"pit": {1: log(3.0, 3.0), 2: "no row here"}, "stress": {1: log(3.0, 3.0), 2: log(3.0, 3.0)}}, quiet) is None
    # guard: the GUARD word and the RU cells of the named row
    grow = lambda label, word: f"  row {label}: top-10 gain +0.04 edge -0.35 ; pond gain +0.00 edge -0.15 ; pool 100.0 % ; [] read on 100.0 % of scored stocks ; 0.14 top-10 name(s) moved a bucket -> {word}"
    d17 = '{"dividend_weight":0.17}'
    grid = lambda y, per, word: _rescore(y, [("{}", eras(2, 2, 2, 2), 120, 80), (d17, per, 120, 80)]).replace("##[endgroup]", "") + f"\n{GUARD_HDR}: the paper route's harm guard.\n" + grow(d17, word)
    fine = grid(12, eras(2, 2, 2, 2), "PASS") + "\n" + grid(8, eras(2, 2, 2, 2), "INERT")
    assert guard_rows(fine) == {12: {d17: "PASS"}, 8: {d17: "INERT"}}
    assert guard(fine, '"dividend_weight":0.17', quiet) == "SHIP"
    assert guard(grid(12, eras(2, 2, 2, 2), "FAIL") + "\n" + grid(8, eras(2, 2, 2, 2), "PASS"), "dividend_weight", quiet) == "KEEP"
    assert guard(grid(12, eras(1, 1, 1, 1), "PASS") + "\n" + grid(8, eras(2, 2, 2, 2), "PASS"), "dividend_weight", quiet) == "KEEP", "a median 1.0 down fails RU3"
    assert guard(grid(12, eras(2, 2, 2, 2), "PASS"), "dividend_weight", quiet) == "UNREAD"
    # cap: the (#557) bar on the `top-10 book` line of each horizon's plain walk
    walk = lambda y, med, worst: f"##[group]backtest {y} universe fund pit\n── GROWTH: HOLD {y}y no-sell ──\n  top-10 book +11.4%/yr  vs S&P500 +8.7%/yr  ->  excess +2.4 (med {med:+.1f}) pts/yr   win 84% of 37   worst {worst:+.1f}   OOS +0.7/+4.7\n##[endgroup]"
    three = lambda cells: "\n".join(walk(y, m, w) for y, (m, w) in zip((20, 12, 8), cells))
    ctl = three([(2.0, -9.0), (3.0, -8.6), (2.9, -6.9)])
    assert top10s(ctl) == {20: (2.4, 2.0, -9.0), 12: (2.4, 3.0, -8.6), 8: (2.4, 2.9, -6.9)}
    lifts = three([(2.0, -7.0), (3.1, -6.0), (2.8, -6.0)])       # every worst window up, one median 0.1 down
    same = three([(2.0, -9.0), (3.0, -8.6), (2.9, -6.9)])        # the control again
    hurts = three([(1.5, -7.0), (3.1, -6.0), (2.8, -6.0)])       # a median 0.5 down
    assert cap(ctl, {3: lifts, 5: same}, quiet) == ("SHIP?", 3)
    assert cap(ctl, {4: same, 5: same}, quiet) == ("KEEP", None), "a winner that lifts nothing is off"
    assert cap(ctl, {3: hurts}, quiet) == ("KEEP", None) and cap(ctl, {3: walk(12, 3.0, -8.0)}, quiet)[0] == "UNREAD"
    assert cap(ctl, {3: lifts, 4: lifts}, quiet) == ("SHIP?", 4), "a tie goes to the larger cap"
    assert [k for k, _, _ in chunks(control)] == ["backtest", "backtest"] and chunks("bare")[0][0] == "whole"
    # payouts: the census lines are echoed per walk, and the (#1247) rule reads the top-10 line of each horizon
    census = "backtest: (#1477) payouts by market: USA 30211 read, D1 3, D2 0"
    seen = []
    assert payouts(ctl.replace("── GROWTH: HOLD 20y", census + "\n── GROWTH: HOLD 20y"), same, seen.append) == ("RECORD", [])
    assert seen[0] == "  20y payouts by market: USA 30211 read, D1 3, D2 0", seen[0]
    nudged = three([(2.0, -9.0), (3.1, -8.6), (2.9, -6.9)])     # the 12y median 0.1 up: the rule's own size
    assert payouts(ctl, nudged, quiet) == ("QUESTION", [12])
    assert payouts(ctl, three([(1.9, -9.0), (3.0, -8.6), (2.8, -6.9)]), quiet) == ("QUESTION", [20, 8]), "either way"
    assert payouts(ctl, walk(12, 3.0, -8.6), quiet) == ("UNREAD", [])
    # plant: the (#1332) table per horizon, top 10 only, and (#1332)'s bands
    cell = "+10 % gain +0.08|+0.19 band top +0.48|+0.79 out 0/20 ; +20 % gain +0.25|+0.24 band top +0.48|+0.79 out 1/20"
    pline = lambda top, x, seeds: f"  top {top} plant {x:+.1f}/yr: {cell} ; pass {len(seeds)}/20 seeds {list(seeds)}"
    block = lambda y, rows: "\n".join([f"##[group]rescore {y}"] + [pline(10, x, s) for x, s in rows] + [pline(20, 4.0, range(1, 21)), "##[endgroup]"])
    seen3 = [(0.5, []), (1.0, [3]), (2.0, list(range(1, 18))), (4.0, list(range(1, 21)))]
    seen1 = [(0.5, list(range(1, 17))), (1.0, list(range(1, 21))), (2.0, list(range(1, 21))), (4.0, list(range(1, 21)))]
    blind = [(0.5, []), (1.0, []), (2.0, [1]), (4.0, [1, 7])]
    log = block(3, seen3) + "\n" + block(1, seen1)
    assert plant_rows(log)[3][2.0][:2] == (17, 20) and 20 not in [k for k in plant_rows(log)], "top 20 lines are not the bar's"
    assert plant(log, quiet) == {3: ("FLOOR", 2.0), 1: ("CANDIDATE BAR", 0.5)}
    assert plant(block(3, blind), quiet) == {3: ("BLIND", None)}
    shown = []
    plant(log, shown.append)
    assert "  X +1.0: seeds every horizon passes [3] (1)" in shown, shown
    assert plant("nothing here", quiet) == {}
    print("verdict selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gates")
    g.add_argument("--control", required=True)
    g.add_argument("--arm", required=True)
    r = sub.add_parser("rivals")
    r.add_argument("log")
    h = sub.add_parser("head")
    h.add_argument("--lane", nargs="+", action="append", required=True, metavar="NAME RUNG=LOG", help="a lane's name, then its logs by rung; give it once per lane")
    c = sub.add_parser("cap")
    c.add_argument("--control", required=True)
    c.add_argument("--cap", nargs="+", required=True, metavar="K=LOG")
    pl = sub.add_parser("plant")
    pl.add_argument("log")
    pa = sub.add_parser("payouts")
    pa.add_argument("--control", required=True)
    pa.add_argument("--arm", required=True)
    q = sub.add_parser("guard")
    q.add_argument("log")
    q.add_argument("--row", help="a substring of the row's compact JSON, e.g. '\"dividend_weight\":0.17'")
    a = ap.parse_args()
    def read(p):
        with open(p, encoding="utf-8", errors="replace") as f:
            return f.read()

    if a.cmd == "gates":
        gates(read(a.control), read(a.arm))
    elif a.cmd == "rivals":
        rivals(read(a.log))
    elif a.cmd == "head":
        head({items[0]: {int(k): read(v) for k, v in (i.split("=", 1) for i in items[1:])} for items in a.lane})
    elif a.cmd == "cap":
        cap(read(a.control), {int(k): read(v) for k, v in (i.split("=", 1) for i in a.cap)})
    elif a.cmd == "payouts":
        payouts(read(a.control), read(a.arm))
    elif a.cmd == "plant":
        plant(read(a.log))
    else:
        guard(read(a.log), a.row)


if __name__ == "__main__":
    main()
