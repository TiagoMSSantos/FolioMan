# (#421) Fund-coverage contract: true only when the pool's equities still carry the fundamentals the shipped
# growth score reads. A tag move drops a field for hundreds of filers at once (#419 debt, #420 operating
# income; ASU 2024-03 moves more for years after 2026-12-15) and the page then ranks them on the neutral fill
# without a word. Pages refuses to deploy a universe under a floor (pages.yml "Validate the payload"); ci.yml
# feeds it good and broken pools on every push so the file itself can't rot.
# Floors = the 2026-09-30 reading minus ~10 pts (547 equities: eps_ttm 93.6, net_debt 90.7, peg_yield 75.5,
# roic 72.6). Raise one when a round lifts its reading; never lower one to get green.
# (#424) plus the split feed: SEC files EPS on the share count of its filing date, and the engine divides each
# pre-split filing by the splits after it. With `splits` gone every such name prices cheap by its split ratio
# (CVNA read PEG 395.9 at P/E 7.5 after its 5:1). Reading 93 of 547 equities = 17.0%, floor 10.
# (#425) plus the share count's scale: a filer that tags its count in thousands or millions (SEC's 2020-11-19
# statement) prices a market cap 1000x off, and every per-cap yield with it. MDO.DE, MCD's Xetra twin, read
# 716.4 shares = 146,862 of cap. Every equity's shares x close must sit in 1e9..2e13 of its own currency
# (2026-09-30: 6.8e9 PSKY to 5.0e12 NVD.DE, EUR/USD only). A pence-quoted listing would need its own band.
# (#426) plus the core-earnings strip: the PEG prices EPS with non-operating gains taken off (GOOGL 0.81), off
# the pretax line facts18 carries. A factor above 1 means the strip ADDED earnings, which it never may; under
# 10% of equities stamped means the pretax tag went dark and every mark prices as earnings again.
[.quotes[] | select(.instrument_type == "EQUITY")] as $eq
| [$eq[] | .fund // {}] as $f
| ($f | length) as $n
| $n >= 100
  and ({eps_ttm: 83, net_debt: 80, peg_yield: 65, roic: 62}
       | all(to_entries[]; .key as $k | .value * $n <= ([$f[] | select(.[$k] != null)] | length) * 100))
  and 10 * $n <= ([$eq[] | select((.splits // []) | length > 0)] | length) * 100
  and ([$eq[] | select(.fund.shares_ttm != null and .close_native != null) | .fund.shares_ttm * .close_native
        | select(. < 1e9 or . > 2e13)] | length) == 0
  and ([$f[] | .core_factor // empty | select(. > 1)] | length) == 0
  and 10 * $n <= ([$f[] | select(.core_factor != null)] | length) * 100
