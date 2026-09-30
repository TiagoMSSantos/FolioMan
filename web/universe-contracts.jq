# (#421) Fund-coverage contract: true only when the pool's equities still carry the fundamentals the shipped
# growth score reads. A tag move drops a field for hundreds of filers at once (#419 debt, #420 operating
# income; ASU 2024-03 moves more for years after 2026-12-15) and the page then ranks them on the neutral fill
# without a word. Pages refuses to deploy a universe under a floor (pages.yml "Validate the payload"); ci.yml
# feeds it good and broken pools on every push so the file itself can't rot.
# Floors = the 2026-09-30 reading minus ~10 pts (547 equities: eps_ttm 93.6, net_debt 90.7, peg_yield 75.5,
# roic 72.6). Raise one when a round lifts its reading; never lower one to get green.
[.quotes[] | select(.instrument_type == "EQUITY") | .fund // {}] as $f
| ($f | length) as $n
| $n >= 100
  and ({eps_ttm: 83, net_debt: 80, peg_yield: 65, roic: 62}
       | all(to_entries[]; .key as $k | .value * $n <= ([$f[] | select(.[$k] != null)] | length) * 100))
