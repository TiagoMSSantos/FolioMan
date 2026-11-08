#!/usr/bin/env python3
"""(#1339) Paper-route proof: does each pre-registered idea's earning third beat its market abroad and at home?

The (#1333)-(#1338) eyesight tests found that our own backtest cannot see a buy-rule edge of realistic size. The
owner's (#1339) paper route lets a small idea ship on OTHER people's proof plus our harm guard. This tool is the
proof half. It reads the free Jensen-Kelly-Pedersen global factor data (jkpfactors.com, factor data set
2025-03-05): `pfs.csv` (three portfolios per characteristic, country and month, sorted on non-microcap
breakpoints) and `market_returns.csv` (each country's market return).

For each of the 42 ideas in `MENU`, fixed in the (#1339) pre-registration before any return was read:
- The long leg is the third the paper says earns: portfolio 3 when JKP's direction is +1, portfolio 1 when -1.
- Each month's gap is that leg's capped value-weight return minus the same country-month market return, in the
  column the unit check picks. A country-month counts only when the leg holds at least 5 names.
- US = USA. Developed ex-US = MSCI "developed" minus USA (`DEV`), one gap a month: the countries that count,
  weighted by their market's start-of-month equity (`me_lag1`).
- PASS needs, in BOTH regions: the full-sample t of the monthly gap at least 1.96, and the gap's mean above 0 over
  the months from 1 January of the year after publication. Fewer than 60 such months in a region is UNGRADEABLE,
  which fails.

UNIT CHECK. JKP's README does not say whether the portfolio returns are local or USD, raw or excess. Their code
builds them from USD excess returns, so the expected market column is `mkt_vw_cap_exc`. Two checks on be_me's
three-portfolio mean confirm it before any premium is printed: over developed ex-US country-months it must sit
closer (RMS) to the USD market than to the local one; and in the US its gap to the raw market, regressed on the
risk-free rate (raw minus excess market), must have a slope nearer -1 (excess) than 0 (raw).

(#1346) JKP's 2026 release (S3 bucket jkpfactors-data, Last-Modified 2026-08-04, months to 2025-12) ships one
zip per country (`pf/<c>.zip`) and one market file (`mkt.zip`) whose only return is USD excess. A dir holding
`mkt.zip` reads that layout: the legs and be_me come from the new zips; the market columns for checks (i) and (ii)
and every `me_lag1` weight come from the pinned old `market_returns.csv`, a 2025 month holding the country's
2024-12-31 weight. Check (iii) confirms the new return sits nearer the old USD excess market than the local one;
the gaps are then graded on the new return (`ret_2026`).

(#1356) MENU 2 holds the 14 ideas (#1339) left out under exclusion (d), "lines we do not parse", whose
Kelly-Pedersen cluster held no ship when the owner opened it. Same order rule, same bars; `--menu 2` grades it.

(#1402) `--beyond` grades the Value cluster's passers, in guard order, on what each adds beyond the shipped ni_me:
in each region, the idea's monthly gap is regressed on ni_me's over the months both carry, and the intercept must
clear the same two-part bar (t at least 1.96, and a mean above 0 after publication over 60 months or more).

(#1410) `--losers` is menu 3: the 56 ideas of menus 1 and 2, merged in the same order rule, each graded on its
OTHER third, the one the paper says trails (portfolio 1 when JKP's direction is +1, portfolio 3 when -1). The gap
is graded as the market minus that third, so the same bar reads as its mirror: the worst third trails at a t of
-1.96 or below, and its gap stays below 0 after publication, in both regions.

(#1411) `--losers --beyond` grades menu 3's passers in a cluster that already holds a ship (Momentum, Investment)
the way `--beyond` grades Value's: each trailing gap, regressed on the shipped idea's trailing gap, must clear the
same two-part bar on its intercept in both regions.

Usage: python3 -I -B tools/jkp_proof.py <dir holding pfs.csv (or mkt.zip and pf/*.zip) and market_returns.csv>
       python3 -I -B tools/jkp_proof.py <dir> --menu 2
       python3 -I -B tools/jkp_proof.py <dir> --beyond
       python3 -I -B tools/jkp_proof.py <dir> --losers
       python3 -I -B tools/jkp_proof.py <dir> --losers --beyond
       python3 -I tools/jkp_proof.py --selftest
Stdlib only. The data is downloaded and untrusted: it is parsed as numbers and text, never executed.
"""
import csv
import glob
import io
import math
import os
import sys
import tempfile
import zipfile
from collections import defaultdict

# (name, publication year, JKP direction, Kelly-Pedersen cluster), in (#1339) order: year, then name.
MENU = [
    ("beta_60m", 1973, -1, "Low Risk"), ("div12m_me", 1979, 1, "Value"), ("market_equity", 1981, -1, "Size"),
    ("ni_me", 1983, 1, "Value"), ("be_me", 1985, 1, "Value"), ("ret_60_12", 1985, -1, "Investment"),
    ("debt_me", 1988, 1, "Value"), ("at_me", 1992, 1, "Value"), ("ret_12_1", 1993, 1, "Momentum"),
    ("ret_3_1", 1993, 1, "Momentum"), ("ret_6_1", 1993, 1, "Momentum"), ("ret_9_1", 1993, 1, "Momentum"),
    ("fcf_me", 1994, 1, "Value"), ("sale_gr1", 1994, -1, "Investment"), ("sale_gr3", 1994, -1, "Investment"),
    ("oaccruals_at", 1996, -1, "Accruals"), ("sale_me", 1996, 1, "Value"), ("dsale_drec", 1998, -1, "Profit Growth"),
    ("dsale_dsga", 1998, 1, "Profit Growth"), ("capx_gr1", 2001, -1, "Investment"), ("rd_me", 2001, 1, "Size"),
    ("rd_sale", 2001, 1, "Low Leverage"), ("capex_abn", 2004, -1, "Debt Issuance"),
    ("noa_at", 2004, -1, "Debt Issuance"), ("noa_gr1a", 2004, -1, "Investment"), ("ocf_me", 2004, 1, "Value"),
    ("age", 2005, -1, "Low Leverage"), ("be_gr1a", 2005, -1, "Investment"), ("sti_gr1a", 2005, 1, "Seasonality"),
    ("capx_gr2", 2006, -1, "Investment"), ("capx_gr3", 2006, -1, "Investment"), ("bev_mev", 2007, 1, "Value"),
    ("eqnpo_me", 2007, 1, "Value"), ("netdebt_me", 2007, -1, "Low Leverage"), ("at_gr1", 2008, -1, "Investment"),
    ("chcsho_12m", 2008, -1, "Value"), ("debt_gr3", 2008, -1, "Debt Issuance"), ("ebitda_mev", 2011, 1, "Value"),
    ("oaccruals_ni", 2011, -1, "Accruals"), ("rd5_at", 2011, 1, "Low Leverage"), ("cash_at", 2012, 1, "Low Leverage"),
    ("ocf_at_chg1", 2019, 1, "Profit Growth"),
]
# (#1356) menu 2, in the same order rule
MENU2 = [
    ("dsale_dinv", 1998, 1, "Profit Growth"), ("sale_emp_gr1", 1998, 1, "Profit Growth"),
    ("kz_index", 2001, 1, "Seasonality"), ("cowc_gr1a", 2005, -1, "Accruals"), ("fnl_gr1a", 2005, -1, "Debt Issuance"),
    ("lti_gr1a", 2005, -1, "Seasonality"), ("ncol_gr1a", 2005, -1, "Debt Issuance"),
    ("nfna_gr1a", 2005, 1, "Debt Issuance"), ("taccruals_at", 2005, -1, "Accruals"),
    ("dbnetis_at", 2006, -1, "Seasonality"), ("tangibility", 2009, 1, "Low Leverage"),
    ("taccruals_ni", 2011, -1, "Accruals"), ("tax_gr1a", 2011, 1, "Profit Growth"),
    ("aliq_mat", 2014, -1, "Low Leverage"),
]
# (#1402) the second Value ship: the cluster's (#1347) passers in guard order, each graded beyond the shipped base
VALUE_QUEUE = ("be_me", "fcf_me", "sale_me", "ocf_me", "bev_mev", "chcsho_12m", "ebitda_mev")
BASE = "ni_me"
# MSCI "developed" in JKP's Country_Classification.xlsx, minus USA
DEV = ("JPN HKG GBR CAN AUS DEU FRA SWE CHE SGP ITA ESP ISR NLD NOR BEL DNK FIN NZL AUT IRL PRT").split()
MIN_N, MIN_POST, T_BAR = 5, 60, 1.96
MKT_COLS = ("me_lag1", "mkt_vw_cap", "mkt_vw_cap_lcl", "mkt_vw_cap_exc")
LONG_PF = {name: (3 if di == 1 else 1) for name, _, di, _ in MENU + MENU2}
# (#1410) menu 3: every idea of menus 1 and 2 in the order rule; `--losers` keeps the third at the other end
MENU3 = sorted(MENU + MENU2, key=lambda x: (x[1], x[0]))
LOSERS = False
# (#1411) menu 3's passers whose cluster holds a ship, in menu order, each with that cluster's shipped idea
LOSER_QUEUE = (("ret_12_1", "ret_6_1"), ("ret_9_1", "ret_6_1"), ("noa_gr1a", "capx_gr3"))
KEEP = set(DEV) | {"USA"}
HELD = "2024-12-31"  # (#1346) the old file's last month; its me_lag1 weighs each country through 2025


def num(s):
    try:
        v = float(s)
    except ValueError:
        return None
    return v if math.isfinite(v) else None


def mean(xs):
    return sum(xs) / len(xs)


def tstat(xs):
    """Mean over its standard error, sd with T-1; None under 2 months or with no spread."""
    if len(xs) < 2:
        return None
    m = mean(xs)
    sd = math.sqrt(sum((x - m) ** 2 for x in xs) / (len(xs) - 1))
    return m / (sd / math.sqrt(len(xs))) if sd > 0 else None


def slope(xs, ys):
    mx, my = mean(xs), mean(ys)
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sum((x - mx) ** 2 for x in xs)


def market_column(usd, excess):
    """The market column the unit check picks; local excess has none (None = UNGRADEABLE)."""
    return {(True, True): "mkt_vw_cap_exc", (True, False): "mkt_vw_cap", (False, False): "mkt_vw_cap_lcl"}.get(
        (usd, excess)
    )


def region_gaps(legs, mkt, countries, col, weighted):
    """eom -> the month's gap: long leg minus market, me_lag1-weighted over the countries that count."""
    acc = defaultdict(lambda: [0.0, 0.0])
    for c in countries:
        for eom, (ret, n) in legs.get(c, {}).items():
            m = mkt.get((c, eom))
            w = (m or {}).get("me_lag1") if weighted else 1.0
            if ret is None or n < MIN_N or m is None or m.get(col) is None or not w:
                continue
            a = acc[eom]
            a[0] += w * (ret - m[col])
            a[1] += w
    return {e: s / w for e, (s, w) in sorted(acc.items())}


def grade(gaps, year):
    """Full-sample t and the post-publication mean of one region's monthly gaps, and the region's verdict."""
    xs = list(gaps.values())
    post = [x for e, x in gaps.items() if e >= f"{year + 1}-01-01"]
    t = tstat(xs)
    pm = mean(post) if post else None
    if len(post) < MIN_POST:
        v = "UNGRADEABLE"
    else:
        v = "PASS" if t is not None and t >= T_BAR and pm > 0 else "FAIL"
    return {"T": len(xs), "mean": mean(xs) if xs else None, "t": t, "P": len(post), "post": pm, "v": v}


def beyond(gy, gx, year):
    """(#1402) OLS of an idea's monthly gap on the base's, over the months both carry. `mean` and `t` are the
    intercept's (the alpha); `post` is the mean, from 1 January of the year after publication, of the gap minus the
    full-sample slope times the base's gap. Same verdict rule as `grade`."""
    ms = [e for e in gy if e in gx]
    xs, ys = [gx[e] for e in ms], [gy[e] for e in ms]
    n = len(ms)
    mx = mean(xs) if xs else 0.0
    sxx = sum((x - mx) ** 2 for x in xs)
    if n < 3 or sxx == 0:
        return {"T": n, "mean": None, "t": None, "P": 0, "post": None, "v": "UNGRADEABLE", "b": None}
    b = slope(xs, ys)
    a = mean(ys) - b * mx
    se = math.sqrt(sum((y - a - b * x) ** 2 for x, y in zip(xs, ys)) / (n - 2) * (1 / n + mx * mx / sxx))
    t = a / se if se > 0 else None
    post = [y - b * x for e, x, y in zip(ms, xs, ys) if e >= f"{year + 1}-01-01"]
    pm = mean(post) if post else None
    if len(post) < MIN_POST:
        v = "UNGRADEABLE"
    else:
        v = "PASS" if t is not None and t >= T_BAR and pm > 0 else "FAIL"
    return {"T": n, "mean": a, "t": t, "P": len(post), "post": pm, "v": v, "b": b}


def old_market(d):
    mkt = {}
    with open(f"{d}/market_returns.csv", newline="") as f:
        for r in csv.DictReader(f):
            if r["excntry"] in KEEP:
                mkt[(r["excntry"], r["eom"])] = {k: num(r[k]) for k in MKT_COLS}
    return mkt


def put(legs, bm, name, c, pf, eom, ret, n):
    """One menu cell in a kept country: be_me feeds the unit check, the earning third (under `--losers`, the
    trailing one) feeds the legs."""
    if name == "be_me" and ret is not None:
        bm[(c, eom)][pf] = ret
    if pf == (4 - LONG_PF[name] if LOSERS else LONG_PF[name]):
        legs[name][c][eom] = (ret, int(num(n) or 0))


def load(d):
    mkt = old_market(d)
    legs = defaultdict(lambda: defaultdict(dict))  # name -> country -> eom -> (ret, n)
    bm = defaultdict(dict)  # (country, eom) -> pf -> be_me ret_vw_cap, for the unit check
    with open(f"{d}/pfs.csv", newline="") as f:
        rd = csv.reader(f)
        h = {k: i for i, k in enumerate(next(rd))}
        ic, ich, ipf, ie, i_n, iret = (h[k] for k in ("excntry", "characteristic", "pf", "eom", "n", "ret_vw_cap"))
        for r in rd:
            if r[ich] in LONG_PF and r[ic] in KEEP:
                put(legs, bm, r[ich], r[ic], int(r[ipf]), r[ie], num(r[iret]), r[i_n])
    return mkt, legs, bm


def zip_rows(src):
    """The rows of a zip's one csv member (a path or a file object); several members is not JKP's layout."""
    with zipfile.ZipFile(src) as z:
        (member,) = z.namelist()
        with z.open(member) as f:
            yield from csv.DictReader(io.TextIOWrapper(f, encoding="utf-8", newline=""))


def overlay(mkt, rows):
    """(#1346) Adds each (country, eom, ret) as ret_2026; a month past the old file holds its HELD weight."""
    for c, eom, ret in rows:
        m = mkt.get((c, eom))
        if m is None:
            m = mkt[(c, eom)] = dict.fromkeys(MKT_COLS)
            m["me_lag1"] = mkt.get((c, HELD), {}).get("me_lag1")
        m["ret_2026"] = ret
    return mkt


def load_2026(d):
    """(#1346) The 2026 layout: `mkt.zip` + `pf/*.zip`, with the old market file for checks and weights."""
    new = ((r["location"].upper(), r["date"], num(r["ret"])) for r in zip_rows(f"{d}/mkt.zip"))
    mkt = overlay(old_market(d), [x for x in new if x[0] in KEEP])
    legs = defaultdict(lambda: defaultdict(dict))
    bm = defaultdict(dict)
    for p in sorted(glob.glob(f"{d}/pf/*.zip")):
        for r in zip_rows(p):
            if r["name"] in LONG_PF and r["location"].upper() in KEEP:
                put(legs, bm, r["name"], r["location"].upper(), int(float(r["pf"])), r["date"], num(r["ret"]), r["n"])
    return mkt, legs, bm


def unit_check(mkt, bm):
    """(usd, excess, printed lines) from be_me's three-portfolio mean against the market columns."""
    sq_usd, sq_lcl, us_d, us_rf = [], [], [], []
    for (c, eom), p in bm.items():
        m = mkt.get((c, eom))
        if len(p) != 3 or m is None:
            continue
        tm = mean(list(p.values()))
        if c == "USA":
            if m["mkt_vw_cap"] is not None and m["mkt_vw_cap_exc"] is not None:
                us_d.append(tm - m["mkt_vw_cap"])
                us_rf.append(m["mkt_vw_cap"] - m["mkt_vw_cap_exc"])
        elif m["mkt_vw_cap"] is not None and m["mkt_vw_cap_lcl"] is not None:
            sq_usd.append((tm - m["mkt_vw_cap"]) ** 2)
            sq_lcl.append((tm - m["mkt_vw_cap_lcl"]) ** 2)
    ru, rl = math.sqrt(mean(sq_usd)), math.sqrt(mean(sq_lcl))
    s = slope(us_rf, us_d)
    usd, excess = ru < rl, abs(s + 1) < abs(s)
    lines = [
        f"unit check (i): {len(sq_usd)} dev ex-US country-months, be_me mean's RMS gap to the USD market {ru:.4f}, "
        f"to the local market {rl:.4f} -> {'USD' if usd else 'LOCAL'}",
        f"unit check (ii): {len(us_d)} US months, slope of (be_me mean - raw market) on the risk-free rate "
        f"{s:+.2f} -> {'EXCESS' if excess else 'RAW'}",
    ]
    return usd, excess, lines


def check_new_market(mkt):
    """(#1346) unit check (iii): the new return's RMS gap to the old USD excess market against the local one."""
    se, sl = [], []
    for m in mkt.values():
        r = m.get("ret_2026")
        if r is not None and m["mkt_vw_cap_exc"] is not None and m["mkt_vw_cap_lcl"] is not None:
            se.append((r - m["mkt_vw_cap_exc"]) ** 2)
            sl.append((r - m["mkt_vw_cap_lcl"]) ** 2)
    rx, rl = math.sqrt(mean(se)), math.sqrt(mean(sl))
    return rx < rl, (f"unit check (iii): {len(se)} country-months, the new market return's RMS gap to the old USD "
                     f"excess market {rx:.4f}, to the old local one {rl:.4f} -> {'USD EXCESS' if rx < rl else 'OTHER'}")


def cell(g):
    pct = lambda x: "    -" if x is None else f"{x * 1200:+5.1f}"
    t = "    -" if g["t"] is None else f"{g['t']:+5.2f}"
    return f"{g['T']:4d} {pct(g['mean'])} {t} {g['P']:4d} {pct(g['post'])} {g['v'][:5]:5s}"


def inputs(d):
    """Loads either layout and prints the unit checks: (market rows, legs, the market column or None)."""
    new = os.path.exists(f"{d}/mkt.zip")
    mkt, legs, bm = (load_2026 if new else load)(d)
    usd, excess, lines = unit_check(mkt, bm)
    col = market_column(usd, excess)
    if new:
        ok, line = check_new_market(mkt)
        lines.append(line)
        col = "ret_2026" if ok and col == "mkt_vw_cap_exc" else None  # the new file has no other market column
    print("\n".join(lines))
    if col is None:
        print("market column: none the release can grade on -> UNGRADEABLE, nothing passes")
    else:
        print(f"market column: {col}; gaps in %/yr (monthly x 12); post = from 1 Jan of the year after publication")
    return mkt, legs, col


def report_beyond(d, queue=tuple((n, BASE) for n in VALUE_QUEUE)):
    """(#1402) Each passer, in guard order, graded on what it adds beyond its base in each region. (#1411) Under
    `--losers` both gaps are the market minus the trailing third."""
    mkt, legs, col = inputs(d)
    if col is None:
        return 1
    side = trail if LOSERS else (lambda g: g)
    regions = (("USA",), False), (DEV, True)
    gaps = lambda name: [side(region_gaps(legs[name], mkt, cs, col, w)) for cs, w in regions]
    year = {name: y for name, y, *_ in MENU + MENU2}
    b = lambda g: "    -" if g["b"] is None else f"{g['b']:+5.2f}"
    print("beyond: alpha = intercept of the idea's monthly gap on its base's, months both carry; b = slope"
          + ("; gaps = market minus the trailing third" if LOSERS else ""))
    print(f"{'idea':14s} {'year':4s} {'base':9s} |   US:     b    T alpha     t    P  post v     "
          f"|  DEV:     b    T alpha     t    P  post v     | verdict")
    passers = []
    for name, base in queue:
        us, dev = (beyond(g, gb, year[name]) for g, gb in zip(gaps(name), gaps(base)))
        ok = us["v"] == "PASS" and dev["v"] == "PASS"
        if ok:
            passers.append(name)
        print(f"{name:14s} {year[name]:4d} {base:9s} | {b(us)} {cell(us)} | {b(dev)} {cell(dev)} | "
              f"{'PASS' if ok else 'fail'}")
    print(f"passers beyond their base, in guard order: {len(passers)} of {len(queue)}: " + ", ".join(passers))
    return 0


def trail(gaps):
    """(#1410) The market minus the leg, so `grade`'s bar passes a leg that TRAILS."""
    return {e: -x for e, x in gaps.items()}


def report(d, menu=MENU):
    mkt, legs, col = inputs(d)
    if col is None:
        return 1
    side = trail if LOSERS else (lambda g: g)
    if LOSERS:
        print("losers: each idea's other third; gap = market minus that third, so PASS = it trails")
    print(f"{'idea':14s} {'year':4s} {'dir':>3s} {'cluster':14s} |   US: T  mean     t    P  post v     "
          f"|  DEV: T  mean     t    P  post v     | verdict")
    passers = []
    for name, year, di, cl in menu:
        us = grade(side(region_gaps(legs[name], mkt, ["USA"], col, False)), year)
        dev = grade(side(region_gaps(legs[name], mkt, DEV, col, True)), year)
        ok = us["v"] == "PASS" and dev["v"] == "PASS"
        if ok:
            passers.append((name, cl))
        print(f"{name:14s} {year:4d} {di:+3d} {cl:14s} | {cell(us)} | {cell(dev)} | {'PASS' if ok else 'fail'}")
    print(f"passers, in order: {len(passers)} of {len(menu)}: " + ", ".join(f"{n} ({c})" for n, c in passers))
    return 0


def demo():
    order = lambda m: m == sorted(m, key=lambda x: (x[1], x[0]))
    assert order(MENU) and order(MENU2) and len(MENU) == 42 and len(MENU2) == 14 and len(LONG_PF) == 56
    assert LONG_PF["taccruals_at"] == 1 and LONG_PF["tax_gr1a"] == 3
    assert not {c for *_, c in MENU2} & {"Value", "Momentum", "Investment"}  # the clusters holding a ship
    assert num("0.5") == 0.5 and num("") is None and num("NA") is None and num("nan") is None
    assert abs(slope([0, 1, 2], [0, -1, -2]) + 1) < 1e-12
    assert market_column(True, True) == "mkt_vw_cap_exc" and market_column(False, True) is None
    # two countries weigh 3:1; a 4-name leg drops out; a month with no market row drops out
    mkt = {("A", "2000-01-31"): {"me_lag1": 3.0, "m": 0.0}, ("B", "2000-01-31"): {"me_lag1": 1.0, "m": 0.01},
           ("B", "2000-02-29"): {"me_lag1": 1.0, "m": 0.0}}
    legs = {"A": {"2000-01-31": (0.02, 9), "2000-03-31": (0.5, 9)}, "B": {"2000-01-31": (0.05, 9),
            "2000-02-29": (0.9, 4)}}
    g = region_gaps(legs, mkt, ["A", "B"], "m", True)
    assert list(g) == ["2000-01-31"] and abs(g["2000-01-31"] - (3 * 0.02 + 1 * 0.04) / 4) < 1e-12, g
    months = [f"{y}-{m:02d}-28" for y in range(1990, 2001) for m in range(1, 13)]
    up = {e: 0.01 + (0.005 if i % 2 else -0.005) for i, e in enumerate(months)}
    assert grade(up, 1994)["v"] == "PASS"
    assert grade(up, 1996)["v"] == "UNGRADEABLE"  # 1997-2000 holds 48 months, under 60
    assert grade({e: -x for e, x in up.items()}, 1994)["v"] == "FAIL"
    # (#1410) menu 3 is both menus in the order rule; a leg 1 %/month under its market passes as a trailer
    assert order(MENU3) and len(MENU3) == 56 and {n for n, *_ in MENU3} == set(LONG_PF)
    assert grade(trail({e: -x for e, x in up.items()}), 1994)["v"] == "PASS" and grade(trail(up), 1994)["v"] == "FAIL"
    # (#1402) the queue is the Value cluster in menu order, without the base; an idea that only copies the base
    # earns no alpha, and one with a steady extra gap passes on it
    menu_value = [n for n, *_, c in MENU if c == "Value"]
    assert [n for n in menu_value if n in VALUE_QUEUE] == list(VALUE_QUEUE) and BASE in menu_value
    assert BASE not in VALUE_QUEUE
    x = {e: 0.01 if i % 2 else -0.01 for i, e in enumerate(months)}
    y = {e: 0.004 + 0.5 * x[e] + 0.002 * (i % 3 - 1) for i, e in enumerate(months)}
    g = beyond(y, x, 1994)
    assert g["v"] == "PASS" and abs(g["b"] - 0.5) < 1e-9 and abs(g["mean"] - 0.004) < 1e-9 and g["P"] == 72, g
    assert abs(g["post"] - 0.004) < 1e-9, g
    assert beyond(y, x, 1996)["v"] == "UNGRADEABLE"
    assert beyond({e: 2 * v for e, v in x.items()}, x, 1994)["v"] == "FAIL"  # a pure copy: alpha 0, no spread
    assert beyond({e: v - 0.008 for e, v in y.items()}, x, 1994)["v"] == "FAIL"  # alpha -0.004
    assert beyond(y, {e: 0.01 for e in months}, 1994)["v"] == "UNGRADEABLE"  # a flat base has no slope
    assert beyond(y, dict(list(x.items())[:2]), 1994)["T"] == 2  # only the months both carry
    # (#1411) the loser queue is in menu order, each idea beside its own cluster's ship; trailing both gaps flips
    # the alpha and keeps the slope
    cl = {n: c for n, _, _, c in MENU3}
    assert [n for n, _ in LOSER_QUEUE] == [n for n, *_ in MENU3 if n in dict(LOSER_QUEUE)]
    assert all(cl[n] == cl[s] and s in ("ret_6_1", "capx_gr3") for n, s in LOSER_QUEUE)
    gt = beyond(trail(y), trail(x), 1994)
    assert abs(gt["mean"] + g["mean"]) < 1e-12 and abs(gt["b"] - g["b"]) < 1e-12 and gt["v"] == "FAIL", gt
    # (#1346) the 2026 layout end to end: a 2025 month holds the 2024-12 weight, a country with none drops out
    with tempfile.TemporaryDirectory() as d:
        with open(f"{d}/market_returns.csv", "w", newline="") as f:
            f.write("excntry,eom,me_lag1,mkt_vw_cap,mkt_vw_cap_lcl,mkt_vw_cap_exc\n"
                    "USA,2024-12-31,2.0,0.012,0.012,0.010\nGBR,2024-12-31,1.0,0.02,0.05,0.019\n")
        with zipfile.ZipFile(f"{d}/mkt.zip", "w") as z:
            z.writestr("m.csv", "location,name,date,ret\nusa,mkt,2024-12-31,0.011\nusa,mkt,2025-01-31,0.02\n"
                                "gbr,mkt,2024-12-31,0.018\nnzl,mkt,2025-01-31,0.03\nzzz,mkt,2025-01-31,9\n")
        os.mkdir(f"{d}/pf")
        with zipfile.ZipFile(f"{d}/pf/usa.zip", "w") as z:
            z.writestr("p.csv", "location,name,pf,n,date,ret\nusa,be_me,3.0,7,2025-01-31,0.05\n"
                                "usa,be_me,1.0,7,2025-01-31,0.01\nusa,not_on_menu,3.0,7,2025-01-31,1\n")
        with zipfile.ZipFile(f"{d}/pf/nzl.zip", "w") as z:
            z.writestr("p.csv", "location,name,pf,n,date,ret\nnzl,be_me,3.0,9,2025-01-31,0.9\n")
        mkt, legs, bm = load_2026(d)
        # (#1410) `--losers` keeps be_me's portfolio 1, the unit check still sees both thirds, NZL (no 1) drops out
        global LOSERS
        LOSERS = True
        try:
            _, lose, lose_bm = load_2026(d)
        finally:
            LOSERS = False
    assert lose["be_me"]["USA"] == {"2025-01-31": (0.01, 7)} and "NZL" not in lose["be_me"], lose["be_me"]
    assert lose_bm == bm
    assert mkt[("USA", "2025-01-31")]["me_lag1"] == 2.0 and mkt[("USA", "2025-01-31")]["mkt_vw_cap"] is None
    assert mkt[("USA", "2024-12-31")]["ret_2026"] == 0.011 and ("ZZZ", "2025-01-31") not in mkt
    assert mkt[("NZL", "2025-01-31")]["me_lag1"] is None
    assert legs["be_me"]["USA"] == {"2025-01-31": (0.05, 7)} and bm[("USA", "2025-01-31")] == {3: 0.05, 1: 0.01}
    assert "not_on_menu" not in legs
    g = region_gaps(legs["be_me"], mkt, ["USA", "NZL"], "ret_2026", True)
    assert list(g) == ["2025-01-31"] and abs(g["2025-01-31"] - 0.03) < 1e-12, g
    ok, line = check_new_market(mkt)
    assert ok and line.startswith("unit check (iii): 2 country-months"), line


if __name__ == "__main__":
    demo()
    if sys.argv[1:] == ["--selftest"]:
        print("jkp_proof selftest: ok")
        sys.exit(0)
    if sys.argv[2:] == ["--beyond"]:
        sys.exit(report_beyond(sys.argv[1]))
    if sys.argv[2:] == ["--losers", "--beyond"]:
        LOSERS = True
        sys.exit(report_beyond(sys.argv[1], LOSER_QUEUE))
    if sys.argv[2:] == ["--losers"]:
        LOSERS = True
        sys.exit(report(sys.argv[1], MENU3))
    if sys.argv[2:] not in ([], ["--menu", "2"]):
        sys.exit("usage: jkp_proof.py <dir> [--menu 2 | --beyond | --losers [--beyond]]")
    sys.exit(report(sys.argv[1], MENU2 if sys.argv[2:] else MENU))
