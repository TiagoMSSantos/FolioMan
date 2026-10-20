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
# 716.4 shares = 146,862 of cap. Every equity's shares x close must sit in 1e8..2e13 of its own currency
# (2026-09-30: 6.8e9 PSKY to 5.0e12 NVD.DE, EUR/USD only). A pence-quoted listing would need its own band.
# (#501) floor 1e9 -> 1e8: the (#499) IPO pond admits fallen listings (HTZ $1.79 x 322M = $576M, 2026-10-06), a real
# cap, not a scale slip. A count in thousands still trips it on any cap under $100B, one in millions on every cap.
# (#426) plus the core-earnings strip: the PEG prices EPS with non-operating gains taken off (GOOGL 0.81), off
# the pretax line facts18 carries. A factor above 1 means the strip ADDED earnings, which it never may; under
# 10% of equities stamped means the pretax tag went dark and every mark prices as earnings again.
# (#763) plus the FX leg: a quote with a native close and no EUR price drops out of every ranked table. One
# missed USD rate did that to all 1850 USD quotes (22 of 39 Stocks rows) and every check here still passed.
# 2026-10-20: 0 of 5397 quotes with a close lack price_eur; at most 1% may.
# (#764) plus a floor per instrument type, where only EQUITY had one (100): a partial Yahoo throttle left a
# page ranked from a fraction of the pool. 2026-10-20 reading: EQUITY 625, ETF 4704, CRYPTOCURRENCY 69.
# Floors 500 / 4000 / 60: 64 of the 625 equities are under 5 years old, and 2026-09-30 read 547, so a floor
# near 0.9x would refuse a page that lost one feed's names.
[.quotes[] | select(.instrument_type == "EQUITY")] as $eq
| [$eq[] | .fund // {}] as $f
| ($f | length) as $n
| [.quotes[] | select(.close_native != null)] as $priced
| $n >= 500
  and ([.quotes[] | select(.instrument_type == "ETF")] | length) >= 4000
  and ([.quotes[] | select(.instrument_type == "CRYPTOCURRENCY")] | length) >= 60
  and ([$priced[] | select(.price_eur == null)] | length) * 100 <= ($priced | length)
  and ({eps_ttm: 83, net_debt: 80, peg_yield: 65, roic: 62}
       | all(to_entries[]; .key as $k | .value * $n <= ([$f[] | select(.[$k] != null)] | length) * 100))
  and 10 * $n <= ([$eq[] | select((.splits // []) | length > 0)] | length) * 100
  and ([$eq[] | select(.fund.shares_ttm != null and .close_native != null) | .fund.shares_ttm * .close_native
        | select(. < 1e8 or . > 2e13)] | length) == 0
  and ([$f[] | .core_factor // empty | select(. > 1)] | length) == 0
  and 10 * $n <= ([$f[] | select(.core_factor != null)] | length) * 100
