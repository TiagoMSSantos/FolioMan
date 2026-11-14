#!/usr/bin/env python3
"""(#1522) How often the BUY NOW head changes from one journal line to the next, which side moved it, what the names
did that day, and how steep the stretch brake is that moves the score side.

The HEAD is the sized book's full-weight stock seats (`sizing.head_weight`: five of them, the score table's first
three names and the momentum half's first two). A name that leaves the head keeps one part in eight of its share. A
STEP is two consecutive journal lines that both size exactly five head seats and both carry a `shipped` list (the
score-only book, journalled since 2026-10-15). A head name among the first three of its line's `shipped` list holds a
SCORE seat, any other a MOMENTUM seat; a leaver is judged on the line it left. A name's day move is its journalled
price on the later line over the earlier one (a euro price, so a dollar line also carries the day's exchange rate),
and the TABLE's move is the median of those over the later line's stock rows. A name's RELATIVE move is its day move
less the median over the stock rows of its own MARKET (the ticker's suffix: DE, PA, ...; none is US), or less the
table's where that market has under five priced rows. A market's rows share one session and one currency move on a
line; the whole table's do not ((#1470): before 2026-11-14 a night line could hold one market a session late).

    python3 -I tools/book_churn.py .screen_snapshots.jsonl [--since 2026-11-15] [--skip 2026-11-08 ...]
    python3 -I tools/book_churn.py .screen_snapshots.jsonl --sessions   # the lines on which a market's rows repeat
    python3 -I tools/book_churn.py .screen_snapshots.jsonl --eyesight [--since ...]   # what the read can tell apart
    python3 -I tools/book_churn.py .screen_snapshots.jsonl --mean 21   # each name's mean share over the last 21 lines
    python3 -I tools/book_churn.py --brake                             # the brake's factor and slope by stretch
    python3 -I tools/book_churn.py --page data.json                    # the Stocks rows inside the brake's band today
    python3 -I tools/book_churn.py --selftest

--since splits the steps into the ones wholly before that date and the ones wholly from it; the step that straddles
it is named and counted in neither (2026-11-15 is the first line written on the walk's legs, (#1509) and (#1512)).
--skip leaves out the steps that end on the given dates (a day a rule shipped moves seats for its own reason).
--mean is the book a buyer on a random day of that window holds on average: the same expected shares as the page's,
without the draw of the day.
--sessions: a market's rows REPEAT the line before when four in five of their day moves sit within 0.05 points of the
market's median move (a euro line that did not trade reads the same price, a dollar line moves by the exchange rate
alone). A night line reads the session that closed the evening before, so a market read on time repeats on the Sunday
and the Monday line. Read a session late it repeats on the Monday and the Tuesday line instead: Tuesday's line holds
Friday's close once more and Sunday's is the first to show it. With no repeat from Wednesday to Saturday between
such a Tuesday and such a Sunday, every line of that week from Tuesday to Saturday holds the session before; a
repeat in mid-week is a line that fell back a session after one read on time. shortcut: a market quoted in more
than one currency never repeats by this rule, and a market holiday repeats like a late line; the journal's own
prices, not checked against a close by day.
--eyesight draws the names that enter and leave at random from their own market's rows on the same steps (the steps
before --since when it is given) and counts how often (#1522)'s things said before 2 and 3 hold together: with no
effect, with half the window's own shift and with all of it. A fixed seed, so the print repeats.
--brake and --page read the stretch brake as `picks::score_parts_hard` writes it: no cut up to `growth_overext_free` %
above the 200-week average, then a straight line down to `growth_overext_floor` at `growth_overext_cap`, raised to
`growth_overext_exp`. The four values below are ci-settings' on 2026-11-14; pass your own when a knob moves. The
what-if moves the brake alone: the legs, the range, the PEG and every gate also read the day's close and are left as
the page printed them.
"""
import argparse, datetime, json, random, re, statistics

CAP, FREE, FLOOR, EXP = 100.0, 60.0, 0.05, 0.5  # growth_overext_cap / _free / _floor / _exp, ci-settings 2026-11-14
SCORE_SEATS = 3  # the head's seats the score table fills; the momentum half fills the rest
PRICE_LISTS = ("rows", "shipped", "carry", "exit")  # journal lists whose rows are [ticker, price, ...]
MIN_MARKET_ROWS = 5  # under this many priced rows a market has no median of its own: the table's stands in
REPEAT_WITHIN, REPEAT_SHARE = 0.0005, 0.8  # --sessions: 0.05 points around the market's median move, four rows in five
SAID_CHANGED, SAID_SHARE, SAID_GAP, SAID_ENTRANTS = 8, 2.0 / 3.0, 0.02, 8  # (#1522) SAID BEFORE 1 to 3 and the read's floor
DAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")


def is_coin(ticker):
    return ticker.endswith("-EUR") or ticker.endswith("-USD")


def market(ticker):
    """The exchange a line trades on, by its suffix: DE for APC.DE, US for a line with none."""
    return ticker.rsplit(".", 1)[1] if "." in ticker else "US"


def market_moves(a, b):
    """market -> the day moves of the later line's stock rows that both lines price."""
    pa, pb = prices(a), prices(b)
    out = {}
    for row in b.get("rows", []):
        if not is_coin(row[0]) and row[0] in pa and row[0] in pb:
            out.setdefault(market(row[0]), []).append(pb[row[0]] / pa[row[0]] - 1.0)
    return out


def load(text):
    """The journal's lines, oldest first; a line that is not a JSON object with a date is dropped."""
    out = []
    for raw in text.splitlines():
        try:
            line = json.loads(raw)
        except ValueError:
            continue
        if isinstance(line, dict) and isinstance(line.get("date"), str):
            out.append(line)
    return sorted(out, key=lambda l: l["date"])


def head(line):
    """The full-weight stock seats of a line's sized book, in its order."""
    stocks = [(t, w) for t, w in line.get("sized", []) if not is_coin(t) and isinstance(w, (int, float))]
    top = max((w for _, w in stocks), default=0.0)
    return [t for t, w in stocks if top > 0.0 and w >= top * 0.99]


def prices(line):
    """ticker -> the first price any of the line's price lists holds for it."""
    out = {}
    for key in PRICE_LISTS:
        for row in line.get(key, []):
            if len(row) > 1 and isinstance(row[1], (int, float)) and row[1] > 0:
                out.setdefault(row[0], row[1])
    return out


def steps(lines):
    """One dict per STEP: the later line's date, the names that entered and left the head with their side, and each
    such name's day move (None where a price is missing); `rel` holds those names' relative moves and `pool` every
    stock row's, by market."""
    out = []
    for a, b in zip(lines, lines[1:]):
        ha, hb = head(a), head(b)
        sa, sb = [r[0] for r in a.get("shipped", [])], [r[0] for r in b.get("shipped", [])]
        if len(ha) != 5 or len(hb) != 5 or not sa or not sb:
            continue
        pa, pb = prices(a), prices(b)
        move = lambda t: pb[t] / pa[t] - 1.0 if t in pa and t in pb else None
        side = lambda t, shipped: "score" if t in shipped[:SCORE_SEATS] else "momentum"
        ins = [(t, side(t, sb), move(t)) for t in hb if t not in ha]
        outs = [(t, side(t, sa), move(t)) for t in ha if t not in hb]
        by_market = market_moves(a, b)
        table = [m for moves in by_market.values() for m in moves]
        mid = statistics.median(table) if table else None
        base = {k: statistics.median(v) if len(v) >= MIN_MARKET_ROWS else mid for k, v in by_market.items()}
        rel = {t: m - base.get(market(t), mid) for t, _, m in ins + outs if m is not None and mid is not None}
        pool = {k: [m - base[k] for m in v] for k, v in by_market.items()}
        out.append({"date": b["date"], "from": a["date"], "in": ins, "out": outs, "head": hb, "table": mid, "rel": rel, "pool": pool})
    return out


def tally(window):
    """Counts over a list of steps: the steps, the ones where the head changed, where a score seat did and where a
    momentum seat did; the score swaps with both prices and those where the entrant's day move was below the leaver's;
    the entrants and the leavers with a move and a table move, and those that fell more, or less, than the table; the
    same two counts and medians on the relative moves, which is what (#1522)'s things said before 2 and 3 read."""
    changed = [s for s in window if s["in"] or s["out"]]
    has = lambda s, which: any(side == which for _, side, _ in s["in"] + s["out"])
    pairs = [(i[2], o[2]) for s in window for i in s["in"] for o in s["out"] if i[1] == o[1] == "score" and i[2] is not None and o[2] is not None]
    against = lambda key: [(m, s["table"]) for s in window for _, _, m in s[key] if m is not None and s["table"] is not None]
    ins, outs = against("in"), against("out")
    relative = lambda key: [s["rel"][t] for s in window for t, _, _ in s[key] if t in s["rel"]]
    rin, rout = relative("in"), relative("out")
    return {
        "rel_entrants": len(rin), "rel_below": sum(r < 0 for r in rin), "rel_leavers": len(rout), "rel_above": sum(r > 0 for r in rout),
        "entrant_rel": statistics.median(rin) if rin else None, "leaver_rel": statistics.median(rout) if rout else None,
        "steps": len(window), "changed": len(changed), "score": sum(has(s, "score") for s in changed), "momentum": sum(has(s, "momentum") for s in changed),
        "pairs": len(pairs), "pairs_below": sum(i < o for i, o in pairs),
        "entrants": len(ins), "entrants_below": sum(m < t for m, t in ins), "leavers": len(outs), "leavers_above": sum(m > t for m, t in outs),
        "entrant_median": statistics.median(m for m, _ in ins) if ins else None, "leaver_median": statistics.median(m for m, _ in outs) if outs else None,
    }


def pct(x):
    return "    n/a" if x is None else f"{x * 100:+6.1f} %"


def points(x):
    return "n/a" if x is None else f"{x * 100:+.1f} points"


def said(t):
    """(#1522)'s three things said before on a tally: None while the read has too few entrants, else three bools.
    1. the head changed on SAID_CHANGED steps or more; 2. SAID_SHARE or more of the entrants moved below their own
    market's median; 3. the entrants' median relative move is SAID_GAP or more below the leavers'."""
    if t["rel_entrants"] < SAID_ENTRANTS or t["leaver_rel"] is None:
        return None
    return (t["changed"] >= SAID_CHANGED, t["rel_below"] >= SAID_SHARE * t["rel_entrants"] - 1e-9, t["entrant_rel"] - t["leaver_rel"] <= -SAID_GAP + 1e-12)


def describe(label, window):
    t = tally(window)
    if t["steps"] == 0:
        return [f"{label}: no step yet"]
    out = [f"{label}: {t['steps']} steps, {window[0]['date']} to {window[-1]['date']}; the head changed on {t['changed']}, a score seat on {t['score']}, a momentum seat on {t['momentum']}"]
    for s in window:
        if s["in"] or s["out"]:
            cells = [f"+{n} {side} {pct(m)}" for n, side, m in s["in"]] + [f"-{n} {side} {pct(m)}" for n, side, m in s["out"]]
            out.append(f"  {s['date']}  table {pct(s['table'])}  " + "  ".join(cells))
    out.append(f"  entrants with a day move: {t['entrants']}, median {pct(t['entrant_median'])}; fell more than the table (or rose less) on {t['entrants_below']}")
    out.append(f"  leavers with a day move: {t['leavers']}, median {pct(t['leaver_median'])}; fell less than the table (or rose more) on {t['leavers_above']}")
    out.append(f"  against its own market's median that day: {t['rel_below']} of {t['rel_entrants']} entrants below, median {points(t['entrant_rel'])};"
               f" {t['rel_above']} of {t['rel_leavers']} leavers above, median {points(t['leaver_rel'])}")
    verdict = said(t)
    word = lambda held: "holds" if held else "fails"
    out.append(f"  (#1522) said before: the read waits: {t['rel_entrants']} entrants, and it wants {SAID_ENTRANTS}" if verdict is None else
               f"  (#1522) said before: 1 {word(verdict[0])}, 2 {word(verdict[1])}, 3 {word(verdict[2])} (the gap is {points(t['entrant_rel'] - t['leaver_rel'])})")
    out.append(f"  score swaps with both prices: {t['pairs']}; the entrant's day move was below the leaver's on {t['pairs_below']}")
    held = {}
    for s in window:
        for t in s["head"]:
            held[t] = held.get(t, 0) + 1
    order = sorted(held.items(), key=lambda kv: (-kv[1], kv[0]))
    out.append(f"  names that held a head seat: {len(order)} for 5 seats: " + " ".join(f"{t} {k}" for t, k in order))
    return out


def report(lines, since=None, skip=()):
    all_steps = [s for s in steps(lines) if s["date"] not in skip]
    out = [f"journal: {len(lines)} lines, {lines[0]['date']} to {lines[-1]['date']}; steps: {len(all_steps)}" if lines else "journal: no line"]
    if since is None:
        return "\n".join(out + describe("all steps", all_steps))
    before = [s for s in all_steps if s["date"] < since]
    after = [s for s in all_steps if s["from"] >= since]
    out += describe(f"before {since}", before) + describe(f"from {since}", after)
    for s in all_steps:
        if s["from"] < since <= s["date"]:
            cells = [f"+{t}" for t, _, _ in s["in"]] + [f"-{t}" for t, _, _ in s["out"]]
            out.append(f"the step across {since} ({s['from']} to {s['date']}) is in neither: {' '.join(cells) or 'no change'}")
    return "\n".join(out)


def mean_book(lines, n):
    """Each name's mean share of gross over the last `n` lines (0 on a line that does not size it), largest first, with
    its share on the last line."""
    window = lines[-n:]
    total = {}
    for line in window:
        for t, w in line.get("sized", []):
            if isinstance(w, (int, float)):
                total[t] = total.get(t, 0.0) + w
    last = dict((t, w) for t, w in window[-1].get("sized", [])) if window else {}
    rows = sorted(((t, s / len(window), last.get(t, 0.0)) for t, s in total.items()), key=lambda r: (-r[1], r[0]))
    out = [f"mean share over the last {len(window)} lines ({window[0]['date']} to {window[-1]['date']}), then the last line's:"] if window else ["no line"]
    out += [f"  {t:<10s} {mean:6.2f} %   {now:6.2f} %" for t, mean, now in rows]
    if rows:
        top5 = sum(mean for _, mean, _ in rows[:5])
        out.append(f"  {len(rows)} names; the five largest hold {top5:.1f} % on the mean, {sum(sorted((now for _, _, now in rows), reverse=True)[:5]):.1f} % on the last line")
    return "\n".join(out)


def sessions(lines):
    """(date, {market: True when its rows repeat the line before}) for each line after the first, over the markets
    with MIN_MARKET_ROWS priced stock rows on it."""
    out = []
    for a, b in zip(lines, lines[1:]):
        flags = {}
        for k, moves in market_moves(a, b).items():
            if len(moves) >= MIN_MARKET_ROWS:
                mid = statistics.median(moves)
                flags[k] = sum(abs(m - mid) < REPEAT_WITHIN for m in moves) >= REPEAT_SHARE * len(moves)
        out.append((b["date"], flags))
    return out


def sessions_report(lines):
    read = sessions(lines)
    count = {}
    for date, flags in read:
        day = DAYS[datetime.date.fromisoformat(date).weekday()]
        for k, repeat in flags.items():
            cell = count.setdefault(k, {}).setdefault(day, [0, 0])
            cell[0] += repeat
            cell[1] += 1
    out = [f"sessions: {len(read)} lines after the first; the lines on which a market's rows repeat the line before, by the line's weekday"]
    for k in sorted(count):
        out.append(f"  {k:<3s} " + "  ".join(f"{day} {count[k][day][0]} of {count[k][day][1]}" for day in DAYS if day in count[k]))
        late = [date for date, flags in read if flags.get(k) and DAYS[datetime.date.fromisoformat(date).weekday()] not in ("Sun", "Mon")]
        out.append(f"      repeats on a line that is neither Sunday's nor Monday's: {' '.join(late) or 'none'}")
    mixed = [date for date, flags in read if len(set(flags.values())) > 1]
    out.append(f"  lines on which one market repeats and another does not: {' '.join(mixed) or 'none'}")
    return "\n".join(out)


def eyesight(window, draws=20000, seed=1521):
    """[(label, share of draws on which things said before 2 and 3 both hold)] for three worlds: the names that enter
    and leave are random rows of their own market on the same steps, moved by nothing, by half the window's own
    median relative moves, and by all of them. Empty when the window has no relative move on either side."""
    seats = []  # per step: its pool, the entrants' markets, the leavers' markets
    for s in window:
        every = [r for moves in s["pool"].values() for r in moves]
        draw = lambda key: [s["pool"].get(market(t)) or every for t, _, _ in s[key] if t in s["rel"]]
        seats.append((draw("in"), draw("out")))
    t = tally(window)
    if t["entrant_rel"] is None or t["leaver_rel"] is None:
        return []
    rng = random.Random(seed)
    out = []
    for label, scale in (("no effect", 0.0), ("half this window's shift", 0.5), ("this window's shift", 1.0)):
        held = 0
        for _ in range(draws):
            rin = [rng.choice(pool) + scale * t["entrant_rel"] for ins, _ in seats for pool in ins]
            rout = [rng.choice(pool) + scale * t["leaver_rel"] for _, outs in seats for pool in outs]
            held += sum(r < 0 for r in rin) >= SAID_SHARE * len(rin) - 1e-9 and statistics.median(rin) - statistics.median(rout) <= -SAID_GAP + 1e-12
        out.append((label, held / draws))
    return out


def eyesight_report(lines, since=None, skip=(), draws=20000):
    window = [s for s in steps(lines) if s["date"] not in skip and (since is None or s["date"] < since)]
    t = tally(window)
    rows = eyesight(window, draws)
    if not rows:
        return "eyesight: no name with a relative move entered and left"
    out = [f"eyesight: {t['rel_entrants']} entrants and {t['rel_leavers']} leavers on {t['changed']} of {t['steps']} steps; this window's shift is {points(t['entrant_rel'])} in, {points(t['leaver_rel'])} out",
           f"  drawn {draws} times from the rows of each name's own market on its step, things said before 2 and 3 both hold:"]
    out += [f"  {label:<26s} {share * 100:5.1f} % of the draws" for label, share in rows]
    return "\n".join(out)


def brake(above, cap=CAP, free=FREE, floor=FLOOR, exp=EXP):
    """The factor the stretch brake leaves of a score at `above` % over the 200-week average."""
    free = max(free, 0.0)
    if cap <= free:
        return 1.0
    stretch = min(max(above, 0.0), cap)
    return (1.0 - max(stretch - free, 0.0) / (cap - free) * (1.0 - floor)) ** exp


def slope(above, cap=CAP, free=FREE, floor=FLOOR, exp=EXP):
    """% the braked score moves for +1 % in price at `above`: 0 outside the band, negative inside it."""
    free = max(free, 0.0)
    if cap <= free or not free < above < cap:
        return 0.0
    damp = 1.0 - (above - free) / (cap - free) * (1.0 - floor)
    return -exp * (1.0 - floor) / (cap - free) * (100.0 + above) / damp


def after_move(above, move, cap=CAP, free=FREE, floor=FLOOR, exp=EXP):
    """The brake's factor after the price moves by `move` (0.05 = +5 %), over its factor before."""
    return brake((100.0 + above) * (1.0 + move) - 100.0, cap, free, floor, exp) / brake(above, cap, free, floor, exp)


def brake_table(cap=CAP, free=FREE, floor=FLOOR, exp=EXP):
    band = (1.0 - (100.0 + free) / (100.0 + cap)) * 100.0
    out = [f"stretch brake: free to {free:g} % above, floor {floor:g} at {cap:g} %, exponent {exp:g}: full brake x{brake(cap, cap, free, floor, exp):.3f}",
           f"  a fall of {band:.1f} % takes a name from the cap to the band's edge, from the full brake to none",
           "  above   factor   score move for +1 % in price   after a -5 % day   after a +5 % day"]
    for above in (50.0, 60.0, 65.0, 70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 100.0):
        out.append(f"  {above:4.0f} %  x{brake(above, cap, free, floor, exp):.3f}   {slope(above, cap, free, floor, exp):+7.1f} %"
                   f"                      x{after_move(above, -0.05, cap, free, floor, exp):.3f}             x{after_move(above, 0.05, cap, free, floor, exp):.3f}")
    return "\n".join(out)


def cell(row, header):
    """The cell under `header` in a page row, a list of [header, cell] pairs; None when absent."""
    return next((c for h, c in row if h == header), None)


def page_rows(payload, lane="stocks"):
    """(ticker, BUY% text, score, % above) for every row of `lane` that prints a score and a signed 200-week stretch.
    A row whose stretch is the 200-day stand-in (marked) or missing has no brake reading and is left out."""
    out = []
    for row in payload.get(lane, []):
        score = re.match(r"-?\d+(\.\d+)?", cell(row, "SCORE") or "")
        above = re.fullmatch(r"([+-]\d+)%", cell(row, "ABV-MA") or "")
        if score and above:
            out.append((cell(row, "TICKER") or "?", cell(row, "BUY%") or "", float(score.group(0)), float(above.group(1))))
    return out


def page_report(payload, cap=CAP, free=FREE, floor=FLOOR, exp=EXP):
    rows = page_rows(payload)
    inside = [r for r in rows if free < r[3] or r[1]]
    out = [f"Stocks rows with a score and a 200-week stretch: {len(rows)}; sized or above {free:g} %: {len(inside)}",
           "  ticker      BUY%     score  above   brake   +1 % in price   score after -5 %   after +5 %   (the brake alone)"]
    for ticker, buy, score, above in inside:
        out.append(f"  {ticker:<10s} {buy:>7s}  {score:6.1f}  {above:+4.0f} %  x{brake(above, cap, free, floor, exp):.3f}  {slope(above, cap, free, floor, exp):+7.1f} %"
                   f"        {score * after_move(above, -0.05, cap, free, floor, exp):6.1f}          {score * after_move(above, 0.05, cap, free, floor, exp):6.1f}")
    return "\n".join(out)


def selftest():
    sized = lambda heads, tails: [["BTC-EUR", 6.67]] + [[t, 16.59] for t in heads] + [[t, 2.07] for t in tails]
    line = lambda date, heads, tails, shipped, px: {"date": date, "sized": sized(heads, tails), "shipped": [[t, px.get(t, 1.0)] for t in shipped],
                                                    "rows": [[t, px.get(t, 1.0)] for t in heads + tails]}
    journal = [
        line("2026-01-01", ["A", "M1", "B", "M2", "C"], ["D", "E"], ["A", "B", "C", "D"], {"C": 100.0, "D": 100.0, "M2": 10.0}),
        line("2026-01-02", ["A", "M1", "B", "M2", "D"], ["C", "E"], ["A", "B", "D", "C"], {"C": 104.0, "D": 90.0, "M2": 10.0}),  # D fell 10 %, C rose 4 %
        line("2026-01-03", ["A", "M1", "B", "M3", "D"], ["C", "E"], ["A", "B", "D", "C"], {"C": 104.0, "D": 90.0, "M3": 5.0}),   # a momentum seat
        line("2026-01-04", ["A", "M1", "B", "M3", "D"], ["C", "E"], ["A", "B", "D", "C"], {"C": 104.0, "D": 90.0}),              # nothing moves
        {"date": "2026-01-05", "sized": sized(["A", "M1", "B", "M3"], ["C"]), "shipped": [["A", 1.0]], "rows": []},              # four heads: no step
    ]
    text = "\n".join(json.dumps(l) for l in reversed(journal)) + "\nnot json\n[1]\n"
    lines = load(text)
    assert [l["date"] for l in lines] == ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04", "2026-01-05"], "sorted, junk dropped"
    assert head(lines[0]) == ["A", "M1", "B", "M2", "C"] and head({"sized": [["BTC-EUR", 99.0]]}) == [], "coins hold no head seat"
    st = steps(lines)
    assert [s["date"] for s in st] == ["2026-01-02", "2026-01-03", "2026-01-04"], "the four-head line makes no step"
    assert st[0]["in"] == [("D", "score", 90.0 / 100.0 - 1.0)] and st[0]["out"] == [("C", "score", 104.0 / 100.0 - 1.0)]
    assert st[1]["in"] == [("M3", "momentum", None)] and st[1]["out"] == [("M2", "momentum", None)], "no price on one line: no move"
    assert st[0]["table"] == 0.0 and st[2]["table"] == 0.0, "the median of the later line's stock rows: A, M1, B, M2 flat, D -10 %, C +4 %, E flat"
    t = tally(st)
    assert (t["steps"], t["changed"], t["score"], t["momentum"], t["pairs"], t["pairs_below"]) == (3, 2, 1, 1, 1, 1), t
    assert (t["entrants"], t["entrants_below"], t["leavers"], t["leavers_above"]) == (1, 1, 1, 1), "D fell under the table, C rose over it; M2 and M3 have no move"
    assert abs(t["entrant_median"] + 0.1) < 1e-12 and abs(t["leaver_median"] - 0.04) < 1e-12
    empty = tally([])
    assert empty["steps"] == 0 and empty["entrants"] == 0 and empty["entrant_median"] is None
    rep = report(lines, since="2026-01-03")
    assert "before 2026-01-03: 1 steps, 2026-01-02 to 2026-01-02; the head changed on 1, a score seat on 1, a momentum seat on 0" in rep, rep
    assert "from 2026-01-03: 1 steps, 2026-01-04 to 2026-01-04; the head changed on 0" in rep, rep
    assert "the step across 2026-01-03 (2026-01-02 to 2026-01-03) is in neither: +M3 -M2" in rep, rep
    assert "2026-01-02  table   +0.0 %  +D score  -10.0 %  -C score   +4.0 %" in rep and "the entrant's day move was below the leaver's on 1" in rep, rep
    assert "entrants with a day move: 1, median  -10.0 %; fell more than the table (or rose less) on 1" in rep, rep
    assert "leavers with a day move: 1, median   +4.0 %; fell less than the table (or rose more) on 1" in rep, rep
    assert "against its own market's median that day: 1 of 1 entrants below, median -10.0 points; 1 of 1 leavers above, median +4.0 points" in rep, rep
    assert "(#1522) said before: the read waits: 1 entrants, and it wants 8" in rep, rep
    assert "from 2026-01-06: no step yet" in report(lines, since="2026-01-06")
    assert "steps: 2" in report(lines, skip=("2026-01-03",)) and "all steps: 2 steps" in report(lines, skip=("2026-01-03",))
    mean = mean_book(lines[:4], 2)
    assert "mean share over the last 2 lines (2026-01-03 to 2026-01-04)" in mean and "D           16.59 %    16.59 %" in mean, mean
    half = mean_book(lines[:2], 2)  # C: a head seat, then a tail seat
    assert f"C          {(16.59 + 2.07) / 2:6.2f} %     2.07 %" in half, half
    # two markets and a thin one: a name is read against its own market's rows, and the whole table mixes them
    assert market("APC.DE") == "DE" and market("WDC") == "US" and market("AIR.PA") == "PA" and not is_coin("BRK-B")
    us, de = ["U1", "U2", "U3", "U4", "U5", "U6"], ["D1.DE", "D2.DE", "D3.DE", "D4.DE", "D5.DE"]
    wide = lambda date, heads, shipped, px: {"date": date, "sized": sized(heads, []), "shipped": [[t, px[t]] for t in shipped],
                                             "rows": [[t, px[t]] for t in us + de + ["P1.PA"]]}
    flat = dict((t, 100.0) for t in us + de + ["P1.PA"])
    tue = dict(flat, U1=90.0, U2=90.0, U3=96.0, U4=90.0, U5=90.0, U6=85.0, **{"P1.PA": 101.0})  # the US falls 10 %, Frankfurt did not trade
    wed = dict(tue, **dict((t, tue[t] * 1.01) for t in us), **{"D1.DE": 101.0, "D2.DE": 102.0, "D3.DE": 103.0, "D4.DE": 99.0, "D5.DE": 98.0})
    thu = dict(wed, **{"P1.PA": wed["P1.PA"] * 0.92})
    markets = [
        wide("2026-02-02", ["U1", "U2", "U3", "D1.DE", "D2.DE"], ["U1", "U2", "U3", "U6"], flat),
        wide("2026-02-03", ["U1", "U2", "U6", "D1.DE", "D2.DE"], ["U1", "U2", "U6", "U3"], tue),     # U6 -15 % in, U3 -4 % out
        wide("2026-02-04", ["U1", "U2", "U6", "D1.DE", "D2.DE"], ["U1", "U2", "U6", "U3"], wed),     # the dollar alone moves the US rows
        wide("2026-02-05", ["U1", "U2", "U6", "D1.DE", "P1.PA"], ["U1", "U2", "U6", "U3"], thu),     # a thin market's name in, -8 %
    ]
    ms = steps(markets)
    assert abs(ms[0]["table"] + 0.02) < 1e-12, "twelve rows: the median sits between U3's -4 % and Frankfurt's 0"
    assert abs(ms[0]["rel"]["U6"] + 0.05) < 1e-12 and abs(ms[0]["rel"]["U3"] - 0.06) < 1e-12, "against the US rows' -10 %, not the table's -2 %"
    assert ms[1]["rel"] == {} and sorted(ms[0]["pool"]) == ["DE", "PA", "US"] and ms[0]["pool"]["DE"] == [0.0] * 5
    assert abs(ms[2]["rel"]["P1.PA"] + 0.08) < 1e-12 and ms[2]["rel"]["D2.DE"] == 0.0, "one Paris row is no market: the table's median (0) stands in"
    mt = tally(ms)
    assert (mt["entrants_below"], mt["leavers_above"]) == (2, 0) and (mt["rel_entrants"], mt["rel_below"], mt["rel_leavers"], mt["rel_above"]) == (2, 2, 2, 1), mt
    assert abs(mt["entrant_rel"] + 0.065) < 1e-12 and abs(mt["leaver_rel"] - 0.03) < 1e-12, mt
    assert tally(ms[:1])["leavers_above"] == 0 and tally(ms[:1])["rel_above"] == 1, "U3 fell more than the table and less than its own market"
    fri = wide("2026-02-06", ["U1", "U2", "U6", "D3.DE", "P1.PA"], ["U1", "U2", "U6", "U3"], thu)  # D3.DE for D1.DE on a day Frankfurt is flat
    level = tally(steps(markets + [fri]))
    assert (level["rel_entrants"], level["rel_below"], level["rel_leavers"], level["rel_above"]) == (3, 2, 3, 1), "a move level with its market is neither below nor above"
    # the three things said before: a floor of entrants, then 8 changes, two thirds below, a gap of 2 points
    base = {"rel_entrants": 12, "rel_below": 8, "entrant_rel": -0.014, "leaver_rel": 0.006, "changed": 8}
    assert said(base) == (True, True, True) and said(dict(base, changed=7)) == (False, True, True)
    assert said(dict(base, rel_below=7)) == (True, False, True) and said(dict(base, entrant_rel=-0.0139)) == (True, True, False)
    assert said(dict(base, rel_entrants=7)) is None and said(dict(base, leaver_rel=None)) is None and said(mt) is None
    # sessions: Frankfurt repeats on the Tuesday line, the US on the Wednesday and Thursday lines (the dollar alone, then nothing)
    assert sessions(markets) == [("2026-02-03", {"US": False, "DE": True}), ("2026-02-04", {"US": True, "DE": False}), ("2026-02-05", {"US": True, "DE": True})]
    sess = sessions_report(markets)
    assert "  DE  Tue 1 of 1  Wed 0 of 1  Thu 1 of 1" in sess and "  US  Tue 0 of 1  Wed 1 of 1  Thu 1 of 1" in sess and "PA" not in sess, sess
    assert "neither Sunday's nor Monday's: 2026-02-03 2026-02-05" in sess and "one market repeats and another does not: 2026-02-03 2026-02-04" in sess, sess
    assert "one market repeats and another does not: none" in sessions_report(lines), "one market on every line"
    weekend = sessions_report([wide(date, ["U1", "U2", "U3", "D1.DE", "D2.DE"], ["U1"], flat) for date in ("2026-02-07", "2026-02-08", "2026-02-09", "2026-02-10")])
    assert "  DE  Mon 1 of 1  Tue 1 of 1  Sun 1 of 1" in weekend and weekend.count("neither Sunday's nor Monday's: 2026-02-10") == 2, weekend
    # eyesight: the entrants are U6 or a US row, and Paris' one row; the leavers a US row and a Frankfurt row
    eye = eyesight(ms, draws=6000)
    assert [label for label, _ in eye] == ["no effect", "half this window's shift", "this window's shift"] and eye == eyesight(ms, draws=6000), "a fixed seed"
    assert abs(eye[0][1] - 1.0 / 6.0) < 0.02 and abs(eye[1][1] - 5.0 / 6.0) < 0.02 and eye[2][1] == 1.0, eye
    assert eyesight([]) == [] and eyesight(ms[1:2]) == [] and "no name with a relative move" in eyesight_report(markets[1:3])
    # one seat, a US pool of -4 and -1 points: the entrant is always below, so the gap of 2 points decides. With no
    # effect it holds on one draw pair in four (-4 in, -1 out); with the window's own -1 in and +2 out, on three.
    one = [{"date": "2026-03-03", "from": "2026-03-02", "in": [("X", "score", -0.05)], "out": [("Y", "score", 0.0)], "head": [], "table": 0.0,
            "rel": {"X": -0.01, "Y": 0.02}, "pool": {"US": [-0.04, -0.01]}}]
    gap = eyesight(one, draws=8000)
    assert abs(gap[0][1] - 0.25) < 0.02 and abs(gap[1][1] - 0.25) < 0.02 and abs(gap[2][1] - 0.75) < 0.02, gap
    assert "2 entrants and 2 leavers on 2 of 3 steps; this window's shift is -6.5 points in, +3.0 points out" in eyesight_report(markets, draws=200)
    assert "1 entrants and 1 leavers on 1 of 1 steps" in eyesight_report(markets, since="2026-02-04", draws=200)
    # the brake: free to 60, a line to 0.05 at 100, square-rooted ((#100): a full brake keeps 0.224)
    assert brake(0.0) == brake(60.0) == 1.0 and abs(brake(100.0) - 0.05 ** 0.5) < 1e-12 and brake(150.0) == brake(100.0) and brake(-20.0) == 1.0
    assert abs(brake(80.0) - 0.525 ** 0.5) < 1e-12, "half way down the band: 1 - 0.5 x 0.95"
    assert slope(60.0) == slope(100.0) == slope(30.0) == 0.0, "the slope is the band's alone"
    assert abs(slope(80.0) - (-0.5 * 0.95 / 40.0 * 180.0 / 0.525)) < 1e-12 and -4.1 < slope(80.0) < -4.0
    numeric = (brake(80.0 + 1.8e-4) / brake(80.0) - 1.0) / 1e-6  # +0.0001 % in price is +0.00018 points of stretch at 80
    assert abs(numeric - slope(80.0)) < 1e-3, (numeric, slope(80.0))
    assert abs(after_move(100.0, -0.2) - 1.0 / 0.05 ** 0.5) < 1e-9, "a 20 % fall from the cap lands on the band's edge: the brake is off"
    assert after_move(50.0, -0.05) == 1.0 and after_move(59.0, 0.05) < 1.0 and after_move(120.0, 0.05) == 1.0
    assert brake(80.0, cap=50.0, free=60.0) == 1.0 and slope(80.0, cap=50.0, free=60.0) == 0.0, "a zone at or past the cap is the brake off"
    assert abs(brake(50.0, free=0.0, exp=1.0) - (1.0 - 0.5 * 0.95)) < 1e-12, "no free zone, no root: the old line"
    assert "a fall of 20.0 % takes a name from the cap to the band's edge" in brake_table()
    payload = {"stocks": [
        [["RANK", "1"], ["TICKER", "AAA"], ["BUY%", "16.59%"], ["SCORE", "40.0"], ["ABV-MA", "+80%"]],
        [["RANK", "2"], ["TICKER", "BBB"], ["BUY%", ""], ["SCORE", "30.0ˢ"], ["ABV-MA", "+20%"]],
        [["RANK", "3"], ["TICKER", "CCC"], ["BUY%", "2.07%"], ["SCORE", "25.5"], ["ABV-MA", "-5%"]],
        [["RANK", "4"], ["TICKER", "YNG"], ["BUY%", ""], ["SCORE", "9.0"], ["ABV-MA", "+35%ᵈ"]],
        [["RANK", "5"], ["TICKER", "NOS"], ["BUY%", ""], ["SCORE", "—"], ["ABV-MA", "+90%"]],
    ]}
    assert page_rows(payload) == [("AAA", "16.59%", 40.0, 80.0), ("BBB", "", 30.0, 20.0), ("CCC", "2.07%", 25.5, -5.0)], page_rows(payload)
    assert page_rows({}) == [] and cell([["A", "1"]], "B") is None
    page = page_report(payload)
    assert "rows with a score and a 200-week stretch: 3; sized or above 60 %: 2" in page and "BBB" not in page, page
    assert f"{40.0 * after_move(80.0, -0.05):6.1f}" in page and "CCC          2.07%    25.5    -5 %  x1.000     +0.0 %" in page, page
    print("book_churn selftest ok")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("journal", nargs="?", help="the page journal, .screen_snapshots.jsonl")
    ap.add_argument("--since", help="split the steps at this journal date, YYYY-MM-DD")
    ap.add_argument("--skip", nargs="*", default=[], help="leave out the steps that end on these dates")
    ap.add_argument("--mean", type=int, help="print each name's mean share over the last N lines instead")
    ap.add_argument("--sessions", action="store_true", help="print the lines on which a market's rows repeat the line before instead")
    ap.add_argument("--eyesight", action="store_true", help="print how often things said before 2 and 3 hold on random names instead")
    ap.add_argument("--brake", action="store_true", help="print the stretch brake's factor and slope by stretch")
    ap.add_argument("--page", help="a page payload (data.json): the Stocks rows inside the brake's band")
    ap.add_argument("--cap", type=float, default=CAP)
    ap.add_argument("--free", type=float, default=FREE)
    ap.add_argument("--floor", type=float, default=FLOOR)
    ap.add_argument("--exp", type=float, default=EXP)
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        selftest()
        return
    knobs = (a.cap, a.free, a.floor, a.exp)
    if a.brake:
        print(brake_table(*knobs))
    if a.page:
        with open(a.page, encoding="utf-8") as fh:
            print(page_report(json.load(fh), *knobs))
    if a.journal:
        with open(a.journal, encoding="utf-8") as fh:
            lines = load(fh.read())
        if a.sessions:
            print(sessions_report(lines))
        elif a.eyesight:
            print(eyesight_report(lines, a.since, tuple(a.skip)))
        else:
            print(mean_book(lines, a.mean) if a.mean else report(lines, a.since, tuple(a.skip)))
    if not (a.brake or a.page or a.journal):
        ap.error("give a journal, --brake, --page or --selftest")


if __name__ == "__main__":
    main()
