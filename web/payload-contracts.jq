# (#418) Payload contracts: true only when every one holds, in any market. Pages refuses to deploy a
# payload that breaks one (pages.yml "Validate the payload"); ci.yml feeds it broken payloads on every
# push so the file itself can't rot. The three contracts:
#   1. the funded BUY% column sums to 100 +/- 2 (rounding), when the book prints one at all;
#   2. no ticker is printed twice across the stocks/etfs/crypto lanes;
#   3. no cell anywhere reads NaN or inf (word-bounded, so "Information" / "Infrastructure" pass).
[.stocks[]?, .etfs[]?, .crypto[]?] as $rows
| [$rows[] | (map(select(.[0] == "BUY%"))[0][1] // "") | select(. != "") | rtrimstr("%") | tonumber] as $w
| [$rows[] | map(select(.[0] == "TICKER"))[0][1]] as $t
| (($w | length) == 0 or ((($w | add) - 100) | fabs) <= 2)
  and (($t | length) == ($t | unique | length))
  and ([.stocks, .etfs, .crypto, .core, .inflation, .bonds, .attention, .berkshire, .young, .upcoming, .exposure | .. | strings | select(test("(^|[^A-Za-z])(NaN|inf)([^A-Za-z]|$)"))] | length == 0)
