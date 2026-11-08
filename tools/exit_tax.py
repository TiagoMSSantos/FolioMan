#!/usr/bin/env python3
"""(#1424) After-tax proceeds of SELLING a lot, under the Portuguese rules the repo's tax model names and the ones it does
not: the flat 28 % autonomous rate (CIRS art. 72.º), the holding-period exclusion (art. 43.º n.º 5: 10 / 20 / 30 % of
the gain excluded at > 2 / >= 5 / >= 8 years), the englobamento option (the included gain taxed at the general rates
of art. 68.º on top of the year's other taxable income, plus the art. 68.º-A solidarity surcharge), a sale split into
equal yearly tranches, and the life-insurance / capitalisation wrapper (art. 5.º n.º 3: 4/5 of the income taxed after
5 years, 2/5 after 8, i.e. 22.4 % and 11.2 % effective, if >= 35 % of the premiums were paid in the first half of the
contract) against the wrapper's yearly fee. With --asset crypto the lot is a criptoativo that is not a valor mobiliário
(art. 10.º n.º 1 k)): its gain and loss are excluded outright once held 365 days or more (art. 10.º n.º 22, ex-n.º 19,
read 2026-11-08; n.º 24 denies the exclusion when the taxpayer or the payer sits outside the EU/EEA or a treaty state).

NO TAX LAW IS ASSUMED CURRENT. The bracket tables below are the two readings found on 2026-11-08 and are named so the
run prints which it used; pass --brackets with your own table (upper_limit:rate,...) when the law moves. This is an
arithmetic aid for a conversation with an accountant, not advice; the repo's rule that an accountant confirms before
`cgt_hold_schedule` is armed stands.

    python3 -I tools/exit_tax.py --cost 10000 --value 67275 --years 20 --other-income 0 --tranches 1 3 5
    python3 -I tools/exit_tax.py --cost 10000 --cagr 10 --years 20 --wrapper-fee 0.5
    python3 -I tools/exit_tax.py --cost 10000 --cagr 10 --years 20 --asset crypto
    python3 -I tools/exit_tax.py --selftest
"""
import argparse, sys

FLAT = 0.28  # art. 72.º autonomous rate on the positive balance of securities gains
# art. 43.º n.º 5, read 2026-11-08: (min_years, excluded share), the widest satisfied rung wins
LADDER = [(2, 0.10), (5, 0.20), (8, 0.30)]
# art. 68.º general rates: (upper limit of rendimento coletável, rate). Two readings found on 2026-11-08:
BRACKETS = {
    # AICCOPN information note of 2026-01-07, citing Lei n.º 73-A/2025 (OE 2026)
    "oe2026": [(8342, 0.125), (12587, 0.157), (17838, 0.212), (23089, 0.241), (29397, 0.311), (43090, 0.349), (46566, 0.431), (86634, 0.446), (None, 0.48)],
    # Cofidis page updated 2026-10-27, which says the 2026 rates were cut again in September
    "oe2026-sept": [(8342, 0.122), (12587, 0.152), (17838, 0.207), (23089, 0.236), (29397, 0.306), (43090, 0.346), (46566, 0.431), (86634, 0.446), (None, 0.48)],
}
# art. 68.º-A solidarity surcharge: (lower limit, rate) on the slice of taxable income above it
SOLIDARITY = [(80000, 0.025), (250000, 0.05)]
WRAPPER = [(0, 1.0), (5, 0.8), (8, 0.4)]  # art. 5.º n.º 3: share of the wrapper's income that is taxed, by years held


def excluded(years, asset="securities"):
    """The share of the gain left out: art. 43.º n.º 5 for a listed security or open-ended fund held `years` (> 2 means
    strictly more); art. 10.º n.º 22 for a crypto asset, all of it from 365 days on and nothing before."""
    if asset == "crypto":
        return 1.0 if years >= 1.0 else 0.0
    out = 0.0
    for min_years, share in LADDER:
        if (years > min_years) if min_years == 2 else (years >= min_years):
            out = share
    return out


def general_tax(income, brackets, joint=False):
    """art. 68.º progressive tax on `income` of rendimento coletável, plus art. 68.º-A, no deductions. `joint` applies
    the quociente conjugal of art. 69.º: the couple's income is halved, taxed, and the tax doubled (68.º-A n.º 3 too)."""
    if joint:
        return 2.0 * general_tax(income / 2.0, brackets)
    tax, lower = 0.0, 0.0
    for upper, rate in brackets:
        if upper is None or income <= upper:
            tax += max(0.0, income - lower) * rate
            break
        tax += (upper - lower) * rate
        lower = upper
    for floor, rate in SOLIDARITY:
        tax += max(0.0, income - floor) * rate if floor == SOLIDARITY[-1][0] else max(0.0, min(income, SOLIDARITY[-1][0]) - floor) * rate
    return tax


def sale(cost, value, years, other_income, brackets, tranches=1, joint=False, asset="securities"):
    """Tax on one lot sold in `tranches` equal yearly parts, each year with the same `other_income`:
    (flat, flat with the ladder, englobamento with the ladder), in euros. The englobamento option covers every
    category G gain of the year (art. 22.º n.º 5), so `other_income` should hold the year's other gains too."""
    gain = max(0.0, value - cost)
    part = gain / tranches
    flat = FLAT * gain
    ladder = FLAT * gain * (1.0 - excluded(years, asset))
    englob = 0.0
    for k in range(tranches):
        included = part * (1.0 - excluded(years + k, asset))  # each later tranche is a year older
        englob += general_tax(other_income + included, brackets, joint) - general_tax(other_income, brackets, joint)
    return flat, ladder, englob


def wrapper_tax(gain, years):
    share = 1.0
    for min_years, s in WRAPPER:
        if years > min_years:
            share = s
    return FLAT * share * gain


def wrapper_break_even(cost, cagr, years, fee_grid=(0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.75, 1.0)):
    """After-tax proceeds of the bare lot (28 % with the ladder) against a wrapper paying `fee` %/yr out of the same
    pre-tax growth; the first fee whose proceeds fall under the bare lot's is the break-even."""
    bare_value = cost * (1 + cagr / 100) ** years
    bare = bare_value - FLAT * (1 - excluded(years)) * (bare_value - cost)
    rows = []
    for fee in fee_grid:
        v = cost * (1 + (cagr - fee) / 100) ** years
        rows.append((fee, v - wrapper_tax(v - cost, years)))
    return bare, rows


def fmt(cost, value, years, other, brackets, tranches, name, joint=False, asset="securities"):
    gain = value - cost
    print(f"lot: cost {cost:,.0f}  value {value:,.0f}  gain {gain:,.0f}  held {years} y  other taxable income {other:,.0f}/yr  brackets {name}{' joint' if joint else ''}  asset {asset}")
    rule = "art. 10.º n.º 22, 365 days" if asset == "crypto" else "art. 43.º n.º 5"
    print(f"  exclusion on sale now: {excluded(years, asset) * 100:.0f} % of the gain ({rule})")
    print("  tranches  flat 28 %       ladder (28 % on the rest)   englobamento + ladder     best      after-tax proceeds (best)")
    for n in tranches:
        flat, ladder, englob = sale(cost, value, years, other, brackets, n, joint, asset)
        best = min(ladder, englob)
        label = "englobamento" if englob < ladder - 1e-9 else "autonomous"
        print(f"  {n:>8}  {flat:10,.0f} ({flat / gain * 100:4.1f} %)  {ladder:10,.0f} ({ladder / gain * 100:4.1f} %)      {englob:10,.0f} ({englob / gain * 100:4.1f} %)   {label:12s} {value - best:,.0f}")


def selftest():
    assert excluded(2) == 0.0 and excluded(2.5) == 0.10 and excluded(5) == 0.20 and excluded(7.99) == 0.20 and excluded(8) == 0.30
    b = BRACKETS["oe2026"]
    assert abs(general_tax(8342, b) - 8342 * 0.125) < 1e-6
    assert abs(general_tax(12587, b) - (8342 * 0.125 + (12587 - 8342) * 0.157)) < 1e-6
    # the 86,634 figure already carries solidarity on 6,634 above 80k; the slice to 100k adds 48 % and 2.5 %
    assert abs(general_tax(100000, b) - (general_tax(86634, b) + (100000 - 86634) * (0.48 + 0.025))) < 1e-6, "top bracket plus solidarity"
    assert abs(general_tax(300000, b) - (general_tax(250000, b) + 50000 * (0.48 + 0.05))) < 1e-6, "second solidarity rung"
    full = sum((u - l) * r for (u, r), l in zip(b[:7], [0] + [x[0] for x in b[:6]]))  # the seven brackets up to 46,566, whole
    assert abs(general_tax(80000, b) - (full + (80000 - 46566) * 0.446)) < 1e-6, "no solidarity at exactly 80k"
    flat, ladder, englob = sale(10000, 67275, 20, 0, b, 1)
    assert abs(flat - 0.28 * 57275) < 1e-6 and abs(ladder - 0.28 * 0.7 * 57275) < 1e-6, (flat, ladder)
    assert englob < ladder, "no other income: the progressive scale beats 28 % on 40,093 of included gain"
    assert abs(englob - general_tax(0.7 * 57275, b)) < 1e-6
    _, _, e3 = sale(10000, 67275, 20, 0, b, 3)
    assert e3 < englob, "three tranches ride the low brackets three times"
    _, _, rich = sale(10000, 67275, 20, 120000, b, 1)
    assert rich > ladder, "on top of 120k the marginal rate is 48 % + 2.5 %: englobamento loses"
    assert abs(general_tax(100000, b, joint=True) - 2 * general_tax(50000, b)) < 1e-6, "the quociente conjugal halves, taxes, doubles"
    _, _, joint = sale(10000, 67275, 20, 40000, b, 1, joint=True)
    _, _, single = sale(10000, 67275, 20, 40000, b, 1)
    assert joint < single, "a couple's 40k rides lower brackets"
    bare, rows = wrapper_break_even(10000, 10.0, 20)
    assert rows[0][1] > bare, "a free wrapper (11.2 %) beats the bare lot (19.6 %)"
    assert any(p < bare for _, p in rows), "a dear enough wrapper loses"
    assert excluded(0.9, "crypto") == 0.0 and excluded(1.0, "crypto") == 1.0 and excluded(20, "crypto") == 1.0
    assert sale(10000, 67275, 20, 0, b, 1, asset="crypto") == (0.28 * 57275, 0.0, 0.0), "a year-old crypto lot owes nothing"
    assert sale(10000, 12000, 0.5, 0, b, 1, asset="crypto")[1] == 0.28 * 2000, "under 365 days the flat rate applies"
    assert abs(wrapper_tax(100.0, 8.5) - 11.2) < 1e-9 and abs(wrapper_tax(100.0, 6) - 22.4) < 1e-9 and abs(wrapper_tax(100.0, 3) - 28.0) < 1e-9
    print("exit_tax selftest ok")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cost", type=float, default=10000.0)
    ap.add_argument("--value", type=float, help="sale value; or give --cagr and it is cost x (1 + cagr)^years")
    ap.add_argument("--cagr", type=float, help="pre-tax %/yr, used when --value is absent and for --wrapper-fee")
    ap.add_argument("--years", type=float, default=20.0)
    ap.add_argument("--other-income", type=float, default=0.0, help="the sale year's other rendimento coletável")
    ap.add_argument("--tranches", type=int, nargs="+", default=[1, 3, 5], help="sell over this many consecutive years, equal gains")
    ap.add_argument("--brackets", default="oe2026", help="a preset name (%s) or upper:rate,... with the last upper as 'inf'" % "/".join(BRACKETS))
    ap.add_argument("--wrapper-fee", type=float, help="print the wrapper comparison; the value is only a reminder, the grid is fixed")
    ap.add_argument("--joint", action="store_true", help="tributação conjunta: --other-income is the couple's, taxed on the quociente conjugal")
    ap.add_argument("--asset", choices=["securities", "crypto"], default="securities", help="crypto = a criptoativo that is not a valor mobiliário (art. 10.º n.º 22)")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        return selftest()
    if a.brackets in BRACKETS:
        brackets, name = BRACKETS[a.brackets], a.brackets
    else:
        brackets, name = [], "custom"
        for cell in a.brackets.split(","):
            upper, rate = cell.split(":")
            brackets.append((None if upper == "inf" else float(upper), float(rate)))
    value = a.value if a.value is not None else a.cost * (1 + (a.cagr or 0.0) / 100) ** a.years
    fmt(a.cost, value, a.years, a.other_income, brackets, a.tranches, name, a.joint, a.asset)
    if a.wrapper_fee is not None and a.cagr is not None:
        bare, rows = wrapper_break_even(a.cost, a.cagr, a.years)
        print(f"\nwrapper (art. 5.º n.º 3, >= 35 % of premiums in the first half): bare lot after tax {bare:,.0f}")
        for fee, proceeds in rows:
            print(f"  fee {fee:4.2f} %/yr -> {proceeds:,.0f}  {'ahead' if proceeds > bare else 'behind'}")
    print("\nnot advice: brackets and exclusions as read on 2026-11-08; deductions, the mínimo de existência and the 2024 mandatory"
          " aggregation of short holds at top incomes are not modelled; englobamento covers all category G income of the year"
          " (art. 22.º n.º 5); an accountant confirms your own case.")


if __name__ == "__main__":
    main()
