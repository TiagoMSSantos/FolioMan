# Ideas ledger

Every buy-rule knob, every deleted knob and every fund factor the backtest can score, with its state
today. It answers "was this tried?" before a new round starts. Written by `tools/ideas_ledger.py` from
the code and `tests/ci-settings.yaml`; re-run it after any knob, factor or receipt change, never edit it
by hand.

- **SHIPPED**: `tests/ci-settings.yaml` sets the knob to this value, or the factor carries this weight.
- **OFF**: the knob is shipped at its off value. **DEFAULT**: the knob is not set there, so the code
  default runs.
- **RETIRED**: the knob was deleted after a measurement refused it. **MEASURED**: the factor is graded
  in every fund sweep but carries no weight.
- **receipt**: the change ids in the comments beside it. **re-open**: what would justify asking again,
  quoted from the receipt where it says so.

| id | idea | kind | name | status | receipt | re-open |
|---|---|---|---|---|---|---|
| 1 | [FOIL] on-sale only: reject if equity 1Y % <= this (the growth lane has its own `growth_min_1y_pct`, not this) | knob, backtest only | `min_1y_pct` | SHIPPED -25.0 | #796 #800 | — |
| 2 | GROWTH GATE (equities): reject if 1Y % <= this. Was a hardcoded 0.0 in both gate sites; crypto keeps its own… | knob | `growth_min_1y_pct` | SHIPPED -0.3 | #457 #566 #609 #724 #796 #800 | — |
| 3 | crypto/FX (-EUR/-USD): looser 1Y floor — they swing far harder. Used by BOTH lanes (the growth knob above is… | knob | `min_1y_pct_crypto` | SHIPPED -50.0 | #47 #561 #566 #587 #609 #724 | — |
| 4 | equities: reject if 1M % <= this (a hard monthly crash = falling knife) | knob | `max_1m_drop_pct` | SHIPPED -1000000.0 | #60 #64 | — |
| 5 | crypto/FX: looser knife — a -20%/month alt is normal, not broken | knob | `max_1m_drop_pct_crypto` | SHIPPED -35.0 | #60 #64 | — |
| 6 | [FOIL] on-sale only: reject if any 5Y/10Y/20Y leg <= this (the growth lane has its own per-rung `growth_min_{… | knob, backtest only | `min_long_pct` | SHIPPED 30.0 |  | — |
| 7 | [FOIL] on-sale only: reject if the >2Y leg <= this CUMULATIVE % (a corpse, e.g. -70%+) | knob, backtest only | `min_long_pct_crypto` | SHIPPED -70.0 |  | — |
| 8 | GROWTH GATE: reject if the 5Y CUMULATIVE % <= this. Was a hardcoded 0.0 in both gate sites (same constant-to-… | knob | `growth_min_5y_pct` | SHIPPED 75.0 |  | — |
| 9 | GROWTH GATE (crypto): the 5Y CUMULATIVE floor for coins ONLY — the twin that lets the equity bar above be tun… | knob | `growth_min_5y_pct_crypto` | SHIPPED 0.0 |  | — |
| 10 | (#1230) GROWTH GATE (ETFs): the fund lane's OWN 5Y CUMULATIVE floor, so the B2 grid can price the equity-tune… | knob | `growth_min_5y_pct_etf` | SHIPPED 0.0 | #192 #1230 | — |
| 11 | GROWTH GATE: same, 8Y rung. -1e9 = off (default), but this is the rung that actually BITES. Measured live 202… | knob | `growth_min_8y_pct` | SHIPPED 42.22 | #315 #1230 | — |
| 12 | GROWTH GATE: same, 20Y rung. -1e9 = off (default). NOTE this bar reaches LONG-LISTED STOCKS ONLY: no UCITS ET… | knob | `growth_min_20y_pct` | SHIPPED 141.18 | #315 | — |
| 13 | DISPLAY ONLY, NEVER SCORED (`picks::perf_fill`): a BLANK perf cell prints the whole-life measured return mark… | knob | `perf_fill_coverage_pct` | SHIPPED 90.0 |  | — |
| 14 | reject if avg daily turnover (EUR) < this (thin/illiquid name); 0 = off | knob | `min_avg_turnover_eur` | SHIPPED 500000 | #955 | — |
| 15 | (#247) growth GATE (stocks): the equity lane's OWN turnover floor, same units as min_avg_turnover_eur. 0.0 =… | knob, backtest only | `min_avg_turnover_eur_stock` | SHIPPED 100000 | #4 #79 #192 #232 #247 #253 | — |
| 16 | (#17/#18) MEASUREMENT endpoint for the LONG-horizon inputs (>=1Y perf/CAGR legs, range position, drawdown) =… | knob | `endpoint_smooth_days` | SHIPPED 105 | #4 #17 #79 #192 #232 #253 | — |
| 17 | a "typical" daily swing (%); the dip is scaled by normal/asset vol, so a calm name's dip counts for more than… | knob | `normal_volatility_pct` | SHIPPED 2.0 |  | — |
| 18 | cap on that volatility-scaled dip (one very deep name can't run away with the ranking) | knob | `discount_cap` | SHIPPED 35.0 |  | — |
| 19 | (#4) multiplier on the direct dip reward (discount×health×momentum). The walk-forward backtest found deepest-… | knob | `discount_weight` | SHIPPED 0.35 | #4 #796 #812 | — |
| 20 | discount ×this when a pulled-back name is turning UP (green week) — reward the bounce (>1; 1.0 = ignore weekl… | knob | `momentum_bounce` | SHIPPED 1.0 | #796 #812 | — |
| 21 | discount ×this when it's still falling (red week & day) — dock the knife (<1; 1.0 = ignore weekly timing) | knob | `momentum_knife` | SHIPPED 1.0 |  | — |
| 22 | reward per %/yr of the long leg's CAGR (annualized >2Y trend) — proven-compounder bonus | knob | `long_trend_weight` | SHIPPED 0.5 | #796 #813 | — |
| 23 | cap on that long-leg CAGR (%/yr) fed into the reward, in BOTH lanes (growth `trend` and the on-sale foil's `l… | knob | `long_trend_cap` | SHIPPED 30.0 | #3 #47 #87 #270 #796 #813 | — |
| 24 | (#15) GROWTH: pin the long-CAGR window to THIS many years (e.g. 10 -> always the 10Y leg) so every name's CAG… | knob | `fixed_cagr_years` | DEFAULT | #15 | — |
| 25 | (#331) GROWTH: the window the PEG DENOMINATOR is measured over, INDEPENDENT of the ranking leg. 0 = inherit `… | knob | `peg_cagr_years` | DEFAULT | #51 #266 #331 | — |
| 26 | GROWTH: the SHORTEST rung `long_leg` may use, on a 20/8/5/2/1Y ladder. 5.0 = today's behaviour (a name with n… | knob | `growth_min_leg_years` | SHIPPED 5.0 | #47 #49 #98 #100 #130 #152 | — |
| 27 | (#14) GROWTH: rank the long trend on the least-squares log-price SLOPE (endpoint-robust) instead of the two-p… | knob | `use_trend_cagr` | OFF (false) | #14 #53 #266 #303 #304 #305 | A re-open needs a PRE-REGISTERED total-return refit of `growth_min_cagr`, graded X vs A o… |
| 28 | (#3j) GROWTH: rank on the WHOLE-LIFE CAGR (`life_cagr`, listing -> today) instead of the 20/8/5Y leg — i.e. t… | knob | `use_life_cagr` | OFF (false) | #3 #14 #37 | new reason, see receipt |
| 29 | long-leg CAGR (%/yr, negative) at which trend_health hits 0 (a decaying multi-year trend); health=1 at flat/r… | knob | `health_zero_cagr` | SHIPPED -10.0 | #3 #8 #47 #87 #144 #270 | — |
| 30 | (B) if BOTH 1Y and 5Y % are <= this, the name is bleeding for years (value trap) -> score ×penalty | knob | `sustained_decline_pct` | SHIPPED -40.0 |  | — |
| 31 | (B) multiplier applied when the sustained-decline condition holds (e.g. 0.4) | knob | `sustained_decline_penalty` | SHIPPED 0.4 |  | — |
| 32 | (B/C) a HARSHER tier: 5Y % <= this (e.g. -70%) = a 7y+ deep bleed riding a stale old chart -> deep penalty | knob | `deep_decline_pct` | SHIPPED -70.0 |  | — |
| 33 | (B/C) multiplier when the deep-decline 5Y condition holds (lower than sustained_decline_penalty) | knob | `deep_decline_penalty` | SHIPPED 0.15 |  | — |
| 34 | (A) drop ranked rows scoring <= this (tables stop padding to top_picks with near-zero at-the-high names) | knob | `min_score` | SHIPPED -100.0 |  | — |
| 35 | (C) reward per % the price sits below its ~200wk SMA (structural "cheap vs trend") | knob | `cheap_weight` | SHIPPED 0.07 | #4 | — |
| 36 | (C) cap on that below-SMA % fed into the cheap reward | knob | `cheap_cap` | SHIPPED 60.0 | #53 #58 | — |
| 37 | (D) reward per % of trailing-1Y dividend yield (reinvested divs dominate long-run total return) | knob | `dividend_weight` | SHIPPED 0.5 | #53 #58 #60 #61 #62 #599 | — |
| 38 | (#61) ON-SALE lane's OWN dividend weight — split from the growth lane for the same reason `onsale_sharpe_weig… | knob | `onsale_dividend_weight` | SHIPPED 0.0 | #53 #58 #60 #61 #62 #599 | — |
| 39 | (D) cap on the yield % fed into the dividend reward | knob | `dividend_cap` | SHIPPED 6.0 | #53 #58 | — |
| 40 | (D/PT) fraction of a dividend KEPT after Portuguese tax when the payer is an EU company — Art. 40.º-A CIRS en… | knob | `tax_keep_eu` | SHIPPED 0.76 | #58 | — |
| 41 | (D/PT) same keep-fraction for EVERY other market AND for funds of any domicile: an OICVM/ETF distribution is… | knob | `tax_keep_other` | SHIPPED 0.72 |  | — |
| 42 | (E) "fair" trailing P/E: value tilt = ref_pe/PE, clamped — cheap (<ref) lifts, rich (>ref) dampens; no PE = n… | knob | `ref_pe` | SHIPPED 20.0 |  | — |
| 43 | (F) reward per % of trailing return-on-capital — the profitability/QUALITY factor (Novy-Marx: high-ROE firms… | knob | `quality_weight` | SHIPPED 0.15 | #61 #62 #72 #796 #798 #799 | — |
| 44 | (F) cap the % fed into the quality reward (one 200%-ROE outlier can't dominate). NOTE it binds on ROE (30-50%… | knob | `quality_cap` | SHIPPED 40.0 | #39 #40 #42 #796 #798 #799 | — |
| 45 | (#87) what an ABSENT ROE is worth, in ROE points, before weight and cap — the same field `growth_fund_extra[]… | knob | `quality_neutral` | SHIPPED 0.0 | #39 #40 #42 #43 #87 #814 | — |
| 46 | growth GATE (equities): must trade at/above this % of its own ~10y range (near the high); below = it's the on… | knob | `growth_min_range_pct` | SHIPPED 80.0 | #50 #74 | — |
| 47 | (#326) THE REGIME VALVE. Both gates above are ABSOLUTE bars applied at every date, which makes the screen pro… | knob | `growth_regime_slack_pct` | DEFAULT | #326 | — |
| 48 | growth GATE (crypto): looser range floor so more coins surface — most alts sit well below their ATH yet still… | knob | `growth_min_range_pct_crypto` | SHIPPED 40.0 | #50 #74 | — |
| 49 | growth GATE (equities): the SAME percentile bar as `growth_min_range_pct`, re-measured on the LAST 8 YEARS (`… | knob | `growth_min_range_pct_8y` | SHIPPED 80.0 |  | — |
| 50 | growth GATE (crypto): same, crypto bar. 0 = off (default). Inert at 40 today — BNB and BTC both keep an S-8Y… | knob | `growth_min_range_pct_8y_crypto` | SHIPPED 40.0 |  | — |
| 51 | crypto SCORE: tilt a coin's growth score by how its 1Y return compares to BITCOIN's (the market base) — beats… | knob | `growth_btc_outperf_weight` | SHIPPED 0.3 |  | — |
| 52 | growth GATE: CAGR (%/yr) floor — below this it's not a proven compounder, just an expensive laggard. TWO legs… | knob | `growth_min_cagr` | SHIPPED 19.0 | #3 #73 #124 #139 #223 #561 | — |
| 53 | growth GATE (crypto): looser CAGR floor so ALL potential growers (not just >8%/yr) surface in the crypto tabl… | knob | `growth_min_cagr_crypto` | SHIPPED 0.0 | #3 #52 #124 #139 #223 #561 | — |
| 54 | (#192) growth GATE (ETFs): the fund lane's OWN CAGR floor, same units and same two legs as growth_min_cagr. 0… | knob | `growth_min_cagr_etf` | SHIPPED 0.0 | #124 #139 #192 #316 #317 #318 | — |
| 55 | (#328) growth GATE (stocks only): a SECOND way past `growth_min_cagr`, and the first one that is not a single… | knob | `growth_sector_leaders` | DEFAULT | #171 #296 #308 #313 #324 #328 | — |
| 56 | growth SCORE: reward per %/yr of the long-leg CAGR (capped at long_trend_cap when that cap is on; 0 there = u… | knob | `growth_trend_weight` | SHIPPED 0.15 | #3 #47 #130 #160 #270 #599 | — |
| 57 | growth SCORE: reward per pt the recent 1Y return outpaces the long CAGR (momentum building) | knob | `growth_accel_weight` | SHIPPED 0.50 | #47 #69 #70 #137 #176 #599 | — |
| 58 | growth SCORE: cap on that 1Y-minus-CAGR acceleration term | knob | `growth_accel_cap` | SHIPPED 50.0 | #3 #47 #70 #130 #572 #573 | — |
| 59 | (#98) how much of the long CAGR the acceleration leg subtracts: accel = clamp(1Y - beta*long_cagr, 0, growth_… | knob | `growth_accel_beta` | SHIPPED 1.0 | #70 #98 #130 #160 #176 #599 | — |
| 60 | growth SCORE: hide ranked growth rows scoring <= this (padding); 0 = show all | knob | `growth_min_score` | SHIPPED -100.0 | #86 | — |
| 61 | growth SCORE: ETF-only display floor. ETFs structurally cap ~5.6 (accel/quality/liq/fund terms all ~0 for a d… | knob | `growth_min_score_etf` | SHIPPED -100.0 | #40 #41 #86 #192 #193 | — |
| 62 | (#86) ONE knob for two halves of the same defect, deliberately inseparable. HALF ONE: `ranked` trims on `min_… | knob | `growth_allow_negative_scores` | OFF (false) | #3 #86 #135 #149 #186 #223 | To re-open, an arm must clear the probe at 12y AND 8y with no negative 20y edge |
| 63 | (1) % ABOVE the 200wk SMA at which the overextension brake maxes out | knob | `growth_overext_cap` | SHIPPED 100.0 | #24 #64 #69 #558 #566 #574 | — |
| 64 | hard gate: reject equities more than this % above the 200wk SMA — the extreme blow-off cohort the overext bra… | knob | `growth_max_above_ma` | SHIPPED 150.0 | #24 #63 #64 #69 #575 #833 | — |
| 65 | (#833) a stock with fewer than LONG_MA_SESSIONS closes has no 200wk average, so its stretch reading is 0 and… | knob | `growth_brake_min_sessions` | SHIPPED 0 | #26 #138 #558 #566 #573 #833 | — |
| 66 | (#25) hard gate: reject equities whose WHOLE-LIFE log-trend CAGR (quote.trend_cagr, full history) is <= 0 — m… | knob | `growth_require_lifetime_uptrend` | SHIPPED true | #24 #25 #64 #183 #184 #1285 | — |
| 67 | (#26) hard gate: reject equities whose worst-ever drawdown MAGNITUDE exceeds this % (e.g. 83 rejects a name t… | knob | `growth_maxdd_cap` | SHIPPED 0.0 | #26 #60 #74 #134 #135 #137 | — |
| 68 | (#26) crypto's OWN maxdd cap. Coins crash >90% every cycle, so the equity cap would gate Bitcoin itself (-83%… | knob | `growth_maxdd_cap_crypto` | SHIPPED 84.0 | #26 #60 #74 #134 #135 #137 | — |
| 69 | (#36) crypto-only hard gate: reject coins whose daily-return stdev (quote.volatility_pct, the VOL column) exc… | knob | `growth_max_vol_crypto` | SHIPPED 3.0 | #36 | — |
| 70 | (#33) hard gate: reject a name younger than this many years (quote.age_years, the YRS column = whole-life lis… | knob | `growth_min_age_years` | SHIPPED 0.0 | #3 #24 #33 #64 #182 #1285 | — |
| 71 | (#1300) a COIN's own listing-age floor, on top of growth_min_age_years: a coin must be at least this many yea… | knob | `growth_min_age_years_crypto` | SHIPPED 8.0 | #33 #160 #178 #511 #1291 #1300 | — |
| 72 | (AUM) ETF-only hard gate: reject a fund smaller than this (quote.aum_eur, EUR-approx from the BF universe pay… | knob | `growth_min_aum_etf` | SHIPPED 100000000 | #511 #1291 #1306 #1315 #1316 | — |
| 73 | (#37) hard gate: reject a name whose PEG exceeds this — too expensive for the growth it actually delivers. Ex… | knob | `growth_max_peg` | SHIPPED 2.0 | #37 #120 #139 #160 #186 #266 | — |
| 74 | (#37 funds) the ETF table's OWN valuation ceiling, same PEG units and same arithmetic as growth_max_peg, appl… | knob | `growth_max_peg_etf` | SHIPPED 2.0 | #37 #41 #97 #127 #566 #817 | — |
| 75 | (V) hard gate: reject a name whose FILER states no EPS in any filing, so `growth_max_peg` can never price it.… | knob | `growth_require_peg` | SHIPPED true | #538 | — |
| 76 | (P4a) hard gate: reject a non-crypto name whose LARGEST single-bar move in the trailing month exceeded this %… | knob | `growth_max_daily_1m` | SHIPPED 0.0 | #38 #41 #97 #127 #168 #817 | — |
| 77 | (P4b) hard gate: the EQUITY/ETF twin of growth_max_vol_crypto — reject a non-crypto name whose daily-return s… | knob | `growth_max_vol` | SHIPPED 0.0 |  | — |
| 78 | (#41) REDUNDANCY skip on the shown stock table: walk the ranked rows and drop one whose 36-month trailing-ret… | knob | `growth_corr_cap` | SHIPPED 0.0 | #40 #41 #120 #140 #192 #193 | — |
| 79 | (#75) VALUE BRAKE: reject the most-expensive-FOR-THEIR-GROWTH P% of a cohort by `fund.peg_yield` (high = chea… | knob | `growth_value_floor_pct` | SHIPPED 40.0 | #41 #47 #71 #72 #75 #1283 | — |
| 80 | (#44) multiply the growth score of a COMMODITY-LINKED row — GICS Energy/Materials, or a fund whose name carri… | knob | `growth_commodity_damp` | SHIPPED 0.2 | #1 #3 #44 #508 #910 | — |
| 81 | (#45) multiply the growth score of an ETF whose LIVE quote currency is not EUR (`quote_currency`, straight fr… | knob | `growth_fx_damp` | SHIPPED 0.98 | #1 #3 #44 #45 #508 | — |
| 82 | (#472) multiply the growth score of a stock Berkshire Hathaway's NEWEST 13F holds (`Quote::brk_held`, the `W`… | knob | `growth_brk_held_boost` | SHIPPED 1.20 | #436 #442 #472 #518 #544 #737 | — |
| 83 | (#472) the same multiplier for a stock Dataroma's superinvestors bought last quarter (8+ to list), or the (#4… | knob | `growth_superinvestor_boost` | SHIPPED 1.0 | #470 #472 #473 #518 #544 #614 | — |
| 84 | (#473) buyers per `growth_superinvestor_boost` step: the user's "x1.1 per 3 superinvestors", counted from zer… | knob | `growth_superinvestor_step` | SHIPPED 3 | #472 #473 #737 #738 | — |
| 85 | (#473) the multiplier for a stock the Social table names through a recent social-arbitrage video or the hand… | knob | `growth_social_boost` | SHIPPED 1.05 | #473 | — |
| 86 | (#545) the CAPEX/OCF column's bar, in %: a stock whose trailing-twelve-month capex is above this share of its… | knob | `growth_capex_ocf_max` | SHIPPED 70 | #545 #834 | — |
| 87 | (#545) the multiplier above `growth_capex_ocf_max`. Equities with SEC 10-Q cash lines only; a bank, an insure… | knob | `growth_capex_ocf_penalty` | SHIPPED 0.80 | #86 #545 #547 #834 | — |
| 88 | (#496) multiply the growth score of an exchange's own token (`picks::EXCHANGE_TOKENS`: BNB, GT, OKB, CRO…), a… | knob | `growth_exchange_token_damp` | SHIPPED 0.5 | #494 #496 #509 | — |
| 89 | (#511) refuse an exchange's own token (`picks::EXCHANGE_TOKENS`) outright, gate tag `exchange`: the coin is o… | knob | `growth_refuse_exchange_tokens` | SHIPPED true | #496 #511 | — |
| 90 | (#494) keep ONE ranked fund per benchmark index, the cheapest euro line (the INDEX cell's ` · TICKER`); the o… | knob | `growth_etf_one_per_index` | SHIPPED true | #245 #318 #494 #509 #510 #547 | — |
| 91 | (#510) ALSO keep one ranked fund per HOLDINGS clique: index groups (as `growth_etf_one_per_index` keys them,… | knob | `growth_etf_one_per_overlap` | SHIPPED 7 | #245 #313 #318 #494 #509 #510 | — |
| 92 | (#34) ETF cost drag: dock the growth score by the ACTUAL 20-year wealth multiple the expense ratio eats, (1-T… | knob | `growth_ter_drag` | SHIPPED true | #34 | — |
| 93 | (#47) replace `trust_factor`'s single 10Y cliff with a graded record-length ladder: 20Y leg -> 1.00, 8Y -> 0.… | knob | `growth_trust_ladder` | DEFAULT | #47 | — |
| 94 | (#49) the BOTTOM two rungs of that ladder, and the only tier that is a knob instead of a constant. The (#47)… | knob | `growth_trust_young` | SHIPPED 0.5 | #47 #49 | — |
| 95 | (1) growth-score multiplier at that cap (e.g. 0.4 = a fully-stretched name keeps 40% of its score); 1.0 = bra… | knob | `growth_overext_floor` | SHIPPED 0.05 | #100 #496 #1133 | — |
| 96 | (#1133) the trust dock's exponent in the growth damp, `damp = trust^this × overext_damp^growth_overext_exp`.… | knob | `growth_trust_exp` | SHIPPED 0.5 | #1133 | — |
| 97 | (#1133) the overextension brake's exponent in that damp. 0.5 = DEFAULT (a 0.05 floor delivers 0.224); 1.0 = t… | knob | `growth_overext_exp` | SHIPPED 0.5 | #1133 #1134 #1135 | — |
| 98 | (#1136) the bubble damp: the growth score keeps max(1 − this × flags/5, 0.5) of itself, the flags being a 2Y… | knob | `growth_bubble_weight` | SHIPPED 0.0 | #1134 #1135 #1136 #1137 #1138 | — |
| 99 | (L) liquidity tilt: bonus per ln(turnover/€1B), added OUTSIDE the brake. Rewards deep-liquid mega-caps (easy… | knob | `growth_turnover_weight` | SHIPPED 0.0 | #20 #1136 #1137 #1138 | — |
| 100 | (#4) crypto's OWN overextension cap (% above the 200wk SMA at which the brake maxes). Crypto routinely rides… | knob | `growth_overext_cap_crypto` | SHIPPED 100.0 | #4 | — |
| 101 | (G) reward per pt of the as-of FUNDAMENTAL factor (see growth_score / fund_factor). The fund lane proves WHIC… | knob | `growth_fund_weight` | SHIPPED 0.07 | #3 #26 #39 #40 #424 #425 | — |
| 102 | (G) cap (in the factor's own pts) on the fund factor fed into that reward, so one data-artifact (+9000% rev)… | knob | `growth_fund_cap` | SHIPPED 300.0 | #3 #26 #39 #40 #424 #425 | — |
| 103 | (#87) what an ABSENT `fund_factor` is worth, in the factor's own pts, before weight and cap. Third of the thr… | knob | `growth_fund_neutral` | SHIPPED 0.0 | #43 #87 | — |
| 104 | (#87) the OTHER half of one missing-data policy, and the half a neutral fill alone gets backwards. A neutral… | knob | `growth_fund_scope_by_class` | OFF (false) | #40 #43 #59 #87 #1129 #1130 | Re-open only with a ruler that does |
| 105 | (G) WHICH as-of FundFactors term the fund tilt weighs: rev_cagr \| rev_accel \| gross_margin \| op_margin \|… | knob | `growth_fund_factor` | SHIPPED "peg_yield" | #2 #3 #784 | — |
| 106 | (G+) ADDITIONAL fundamental tilt terms, summed on top of the single `growth_fund_*` term above. The three kno… | knob | `growth_fund_extra` | SHIPPED list | #3 #40 #43 #87 #1129 #1130 | — |
| 107 | (Item 22) WHICH fundamentals feed BOTH the `backtest <set> fund` lane AND the live `screen`/`check` fund tilt… | knob | `fund_source` | SHIPPED "sec" | #1 #3 #147 #173 #180 #181 | — |
| 108 | (M) reward per pt of 12-1 momentum (trailing 1Y return EX the last 1mo — Jegadeesh-Titman, skips the short-te… | knob | `growth_mom121_weight` | DEFAULT |  | — |
| 109 | (M) cap (in pct pts) on the 12-1 momentum fed into that reward, so one moonshot can't dominate the rank | knob | `growth_mom121_cap` | DEFAULT |  | — |
| 110 | (E) additive reward per unit of trend_r2 (R² of the log-price trend fit, 0..1; 0 also = no history) — pays na… | knob | `growth_smoothness_weight` | SHIPPED 0.0 | #1 #59 #60 #61 #62 | — |
| 111 | additive PENALTY per year of the longest below-prior-peak stretch (core::longest_underwater_yrs — the drawdow… | knob | `growth_underwater_weight` | SHIPPED 0.3 | #1 #3 #59 #60 #223 #817 | — |
| 112 | (Item 20) authority of the P/E multiplier (value_factor) in the GROWTH lane only — BACKTEST-BLIND until (#147… | knob | `growth_value_weight` | SHIPPED 0.0 | #1 #95 #134 #145 #147 #159 | — |
| 113 | (#48) authority of the PROXIMITY multiplier — the growth score's `range_pct/100` term, which docks a name in… | knob | `growth_proximity_weight` | SHIPPED 1.0 | #8 #48 #497 #599 #819 | — |
| 114 | (#73) REPOINTED — this is now the window on `growth_min_cagr`'s WHOLE-LIFE REJECT BAR (leg 2), not on the ran… | knob | `life_cagr_max_years` | SHIPPED 0.0 | #3 #14 #37 #51 #73 #130 | — |
| 115 | (splice) redenomination-splice trimmer: drop history BEFORE the last step whose implied WEEKLY growth factor… | knob | `splice_max_weekly_rate` | SHIPPED 2.0 | #117 #497 | — |
| 116 | (#263) plateau trimmer, the splice trimmer's twin: drop history BEFORE the last run of byte-identical consecu… | knob | `flat_run_max_years` | SHIPPED 2.0 | #8 #117 #263 | — |
| 117 | (#112) add one named hazard to the backtest's caveat block: the most common way a twenty-year single-stock ho… | knob | `print_acquisition_hazard` | SHIPPED true | #8 #48 #108 #112 #819 #1114 | — |
| 118 | (#112) print the book's CURRENCY exposure under the ranking, looking funds through to their holdings instead… | knob | `print_currency_mix` | SHIPPED true | #108 #112 #1114 | — |
| 119 | (#111) print, per lane, the OWN-POOL base rate behind the whole thesis: of the samples whose trailing leg had… | knob | `print_base_rates` | SHIPPED true | #111 #124 #127 #128 #192 #1114 | — |
| 120 | (#111) hold horizon, in years, for the REVERSE-DCF line printed under the ranking: the EPS growth rate today'… | knob | `implied_growth_years` | SHIPPED 10 | #111 #124 #127 #128 #192 #1114 | — |
| 121 | (#111) the %/yr TOTAL PRICE RETURN the reverse-DCF line assumes its reader demands. Not a gate, not a score i… | knob | `implied_growth_required_pct` | SHIPPED 8.0 | #111 #1114 | — |
| 122 | (#110) symmetric threshold, in percentage points, that splits the LEVEL entry state into CHEAP / NEUTRAL / RI… | knob | `entry_excess_yield_band` | SHIPPED 0.0 | #110 #306 #1114 | — |
| 123 | (#109) dock a DISTRIBUTING share class by the twenty-year cost of paying tax on every payout instead of defer… | knob | `growth_acc_drag` | OFF (false) | #109 #306 | RE-OPEN: a disagreeing corner reaching 2H windows on the backtest's own buckets |
| 124 | (#108) the headline capital-gains rate the backtest's after-tax footer prices both arms at, in percent. Was t… | knob | `capital_gains_tax_pct` | SHIPPED 28.0 | #108 | — |
| 125 | (#108) holding-period exclusions applied to the GAIN before `capital_gains_tax_pct`, longest-hold rung wins.… | knob | `cgt_hold_schedule` | OFF ([]) | #108 | new reason, see receipt |
| 126 | (#107) how many perturbed knob vectors `picks::rank_robustness` scores the universe under, to print a MEDIAN… | knob | `rank_perturb_k` | SHIPPED 50 | #107 #1153 | — |
| 127 | (#106) print the DISTRIBUTION of the held book's terminal multiple across windows — d10 / median / d90, plus… | knob | `print_book_deciles` | SHIPPED true | #106 #124 #128 #140 #1114 #1153 | — |
| 128 | (#563) ALL-GATES-SOFT: a name that fails knob gates is scored with those gates waived (`picks::waived`, the M… | knob | `growth_gate_soft` | SHIPPED 0.8 | #186 #192 #258 #560 #563 #1282 | — |
| 129 | (#1230) the ETF twin of growth_gate_soft: a fund failing knob gates scores waived x f^n at THIS f. 0.0 = fund… | knob | `growth_gate_soft_etf` | SHIPPED 0.0 | #160 #421 #552 #566 #1230 #1364 | — |
| 130 | (#1362) the growth bar's taper: a STOCK under its `cagr` or `cagr-life` floor by less than this many %/yr is… | knob | `growth_cagr_soft_band` | SHIPPED 5.0 | #57 #565 #1354 #1362 #1369 #1370 | — |
| 131 | (#1245) each of three corporate events inside this many months before a backtest cutoff (a dividend cut to un… | knob | `growth_event_months` | SHIPPED 0 | #1245 | — |
| 132 | (#104) print, beside every lane's rho and edge, how much of the pool's EXTREME upside the ranking actually he… | knob | `print_recall_capture` | SHIPPED true | #91 #104 #140 #154 #160 #172 | — |
| 133 | (#103) journal the CORE shortlist alongside the ranked slice, so `track` can one day grade the buy-and-hold h… | knob | `journal_core_list` | SHIPPED true | #103 #253 #285 #758 #784 #795 | — |
| 134 | (#102) the CORE admission TER ceiling, in percent, as a named number instead of a literal buried in `core::ho… | knob | `hold_max_ter` | SHIPPED 0.25 | #102 | — |
| 135 | (#267) the CORE admission AUM floor, in EUR, as a named number instead of the `1e9` literal buried in `core::… | knob | `hold_min_aum_eur` | SHIPPED 1000000000 | #102 #224 #267 #300 #301 #302 | — |
| 136 | (#202) TER ceiling, in percent, for a NEW eighth CORE sleeve holding world SMALL-CAP funds; 0.0 = the sleeve… | knob | `hold_size_sleeve_ter` | SHIPPED 0.35 | #95 #102 #197 #201 #202 #220 | — |
| 137 | (#102) accept an EU-domiciled fund as UCITS even when the NAME omits the token. `hold_miss_reason` requires t… | knob | `hold_ucits_or_domicile` | SHIPPED true | #95 #102 #196 #197 #1037 | — |
| 138 | (#233) admit SWAP-BASED (synthetic) ETFs to the CORE lane. `hold_miss_leg`'s replication leg refuses everythi… | knob | `hold_allow_swap` | SHIPPED true | #95 #197 #204 #224 #233 #1037 | — |
| 139 | (#240) admit a fund whose TER we never fetched to the CORE lane. `hold_miss_leg`'s TER leg refuses TWO popula… | knob | `hold_allow_unknown_ter` | SHIPPED true | #222 #224 #226 #233 #240 #241 | — |
| 140 | (#269) a SECOND, LOWER CORE admission floor in EUR that buys NEW INDEX FAMILIES ONLY. A fund whose AUM sits b… | knob | `hold_min_aum_eur_new_family` | SHIPPED 500000000 | #224 #238 #239 #251 #268 #269 | — |
| 141 | (#273) a THIRD CORE admission AUM floor, scoped to the WORLD SECTOR SLEEVE rather than to a fund's index fami… | knob | `hold_sector_sleeve_aum` | SHIPPED 90000000 | #220 #224 #237 #269 #272 #273 | — |
| 142 | (#274) read an ISSUER SHARE-CLASS CODE (`1C` = Xtrackers/DWS Capitalising) as an accumulating class when the… | knob | `hold_issuer_share_class_codes` | SHIPPED true | #20 #171 #242 #272 #274 #278 | — |
| 143 | (#233) TER credit, in percentage points, given to a SWAP fund at GEO tier 3 (US) when the CORE sleeve sorts -… | knob | `hold_us_swap_ter_credit` | SHIPPED 0.10 | #204 #220 #224 #231 #233 #234 | — |
| 144 | (#102) require a NARROW/GEO token to sit at a word start before it disqualifies or classifies a fund name. `c… | knob | `hold_name_tokens` | OFF (false) | #95 #102 #195 #231 #233 #234 | Set true to re-open, and re-run the pair above — a pond that has grown may trip what this… |
| 145 | (#217) turn on a TENTH CORE sleeve holding world FACTOR funds -- quality, value and equal weight, plus moment… | knob | `hold_factor_sleeve` | SHIPPED true | #210 #217 #222 #226 #228 #234 | — |
| 146 | (#218) turn on an ELEVENTH CORE sleeve holding world/US SECTOR funds -- technology, health care, financials,… | knob | `hold_sector_sleeve` | SHIPPED true | #102 #210 #216 #217 #218 #224 | — |
| 147 | (#214) fill each CORE sleeve FAMILY-FIRST: one pass takes the best fund of each DISTINCT index family (which… | knob | `hold_family_first` | SHIPPED true | #197 #198 #207 #214 #215 #279 | — |
| 148 | (#272) key a CORE fund's INDEX FAMILY on `Quote::benchmark` -- BF's own index name -- instead of on the `core… | knob | `hold_family_key_benchmark` | SHIPPED true | #202 #203 #210 #220 #269 #272 | — |
| 149 | (#207) rows kept in the ALL-WORLD sleeve alone, overriding `hold_per_tier` there; 0 = no override, inherit it… | knob | `hold_per_tier_all_world` | SHIPPED 7 | #120 #197 #203 #207 #210 #213 | — |
| 150 | (#251) rows kept in the DEVELOPED sleeve alone (tier 1), overriding `hold_per_tier` there, AND the switch for… | knob | `hold_per_tier_developed` | SHIPPED 8 | #120 #197 #203 #227 #239 #251 | — |
| 151 | (#231) rows kept in the US sleeve alone, overriding `hold_per_tier` there; 0 = no override, inherit it, byte-… | knob | `hold_per_tier_us` | SHIPPED 9 | #197 #231 #234 #239 #240 #282 | — |
| 152 | (#237) rows kept in the EUROPE sleeve alone, overriding `hold_per_tier` there; 0 = no override, inherit it, b… | knob | `hold_per_tier_europe` | SHIPPED 5 | #197 #207 #213 #224 #237 #282 | — |
| 153 | (#230) rows kept in the SMALL-CAP sleeve alone, overriding `hold_per_tier` there; 0 = no override, inherit it… | knob | `hold_per_tier_size` | SHIPPED 4 | #197 #214 #229 #230 #237 #272 | — |
| 154 | (#228) rows kept in the world FACTOR sleeve alone, overriding `hold_per_tier` there; 0 = no override, inherit… | knob | `hold_per_tier_factor` | SHIPPED 5 | #197 #200 #207 #215 #216 #228 | — |
| 155 | (#220) rows kept in the world SECTOR sleeve alone, overriding `hold_per_tier` there; 0 = no override, inherit… | knob | `hold_per_tier_sector` | SHIPPED 23 | #197 #218 #220 #229 #272 #273 | — |
| 156 | (#227) turn on a TWELFTH CORE sleeve holding SINGLE-MARKET funds -- DAX, Euro Stoxx 50, MSCI EMU, FTSE 100, K… | knob | `hold_per_tier_country` | SHIPPED 17 | #120 #210 #217 #227 #236 #273 | — |
| 157 | (#239) turn on a THIRTEENTH CORE sleeve holding the NASDAQ-100 -- the one member of the exchange's index suit… | knob | `hold_per_tier_nasdaq` | SHIPPED 2 | #202 #215 #216 #234 #239 #283 | — |
| 158 | (#197) rows kept per breadth sleeve in the buy-and-hold CORE table, as a named number instead of the bare `co… | knob | `hold_per_tier` | SHIPPED 6 | #120 #195 #197 #234 #242 #272 | — |
| 159 | (#584) the BUY NOW book alternates the shipped score book with the momentum order (`growth_momentum_key`, 12-… | knob | `growth_momentum_interleave` | SHIPPED true | #197 #242 #272 #584 #585 #817 | — |
| 160 | (#587) the momentum half's sort key (`picks::momentum_keys`: the live book, the backtest's `momentum_ranked`… | knob | `growth_momentum_key` | SHIPPED 1 | #584 #585 #586 #587 #609 #817 | — |
| 161 | (#597) of every 10 slots in the interleaved book (`growth_momentum_interleave`), how many the momentum order… | knob | `growth_momentum_slots` | SHIPPED 5 | #388 #560 #584 #597 #609 #630 | — |
| 162 | (#1243) the momentum slots of every 10 in a CRASH bucket: the pond's equal-weight month (the mean of every tr… | knob | `growth_momentum_crash_slots` | SHIPPED 10 | #1243 | — |
| 163 | (#1244) the share of the momentum half's keyed names, the jumpiest by the sd of their last 36 monthly returns… | knob | `growth_momentum_vol_cut` | SHIPPED 0.0 | #1244 #1311 | — |
| 164 | (#1304) the order the stock half of the growth table is read in, one id of `picks::RANKERS`. "shipped" (the D… | knob | `growth_ranker` | SHIPPED shipped | #584 #1304 #1311 | — |
| 165 | (#101) at most this many stocks per GICS sector in the growth book; 0 = off and the DEFAULT. Nothing else in… | knob | `growth_sector_cap` | SHIPPED 0 | #101 #126 #557 #606 #630 #1142 | — |
| 166 | (#97) restate `volatility_pct` (and its `downside_dev_pct` twin) in DAILY-equivalent units on any cadence, so… | knob | `vol_daily_equivalent` | OFF (false) | #97 #116 #117 #175 #176 #566 | Do not re-open by running the cap sweep — it would reproduce the VACUOUS reading this ent… |
| 167 | (#96) restate a lane's `edge` in points PER YEAR beside the cumulative figure. `edge` is a spread of returns… | knob | `print_edge_annualized` | SHIPPED true | #37 #96 #176 #564 #1114 #1132 | — |
| 168 | (#96) charge the turnover round trip once per ~6mo REBALANCE inside the hold instead of once per hold. `turno… | knob | `cost_per_rebalance` | OFF (false) | #96 #176 #177 | new reason, see receipt |
| 169 | (#95) stop stamping TODAY's GICS sector onto a historical cutoff. `stamp_asset_class` fills `quote.sector` fr… | knob | `backtest_drop_lookahead_sector` | OFF (false) | #44 #94 #95 #96 #176 #177 | new reason, see receipt |
| 170 | (#94) move the redenomination trim from PARSE time to CUTOFF time, so a splice cannot delete history that had… | knob | `splice_trim_point_in_time` | OFF (false) | #44 #94 #139 #173 #188 #189 | TO REOPEN: build the YRS check the block above asks for — does the column stop misstating… |
| 171 | (#784) serve a SEC row as its strict twin (`core::FundRow::strict`, every line filed by revenue's filed date,… | knob | `sec_line_filed_gate` | SHIPPED true | #2 #3 #758 #784 | — |
| 172 | (#93) split the peer group `demean` subtracts by LISTING MARKET as well as by cutoff bucket and asset class.… | knob | `demean_by_market` | SHIPPED true | #93 #94 #139 #173 #1231 #1232 | — |
| 173 | (#122) DROP a ticker whose Yahoo mapping no longer points at the company it names. A dead company's ticker is… | knob | `drop_dead_ticker_series` | OFF (false) | #47 #93 #121 #122 #1231 #1232 | new reason, see receipt |
| 174 | (#92) print, beside the top-N ladder table, how many rungs the eye is choosing between. WRITTEN FOR A DEBT TH… | knob | `print_selection_count` | SHIPPED true | #47 #91 #92 #120 #121 #1114 | — |
| 175 | (#91) print, beside every window count that carries a verdict, how many INDEPENDENT trials that count is wort… | knob | `print_n_eff` | SHIPPED true | #89 #90 #91 #120 #124 #1114 | — |
| 176 | (#135) print the same-window head-to-head a SECOND time against the 6-10 book, beside today's 11-20 one. WHY:… | knob | `print_h2h_mid_ladder` | SHIPPED true | #89 #90 #124 #130 #135 #1114 | — |
| 177 | (#159) print, for the GROWTH lane, the held book split into the names that MADE money and those that LOST it,… | knob | `print_held_loser_factors` | SHIPPED true | #9 #100 #128 #129 #151 #159 | — |
| 178 | (#1242) print, after the GROWTH lane, a seeded bootstrap band on each held-book loser AUC of the seven profit… | knob | `print_held_loser_bands` | OFF (false) | #1242 | new reason, see receipt |
| 179 | (#1278) print, after the GROWTH lane, the held-book loser AUC of every registered fund factor twice: raw, and… | knob | `print_sector_neutral_auc` | OFF (false) | #1278 | new reason, see receipt |
| 180 | (#90) how much history to DROP off the end of the earlier side of every train/test and early/late split, so t… | knob | `split_purge_months` | SHIPPED 0 | #89 #90 #91 #124 #151 #160 | — |
| 181 | (#1227) resample whole TICKERS in `bootstrap_edge_ci` instead of runs of ~6-month cutoff buckets. WHY: (#119)… | knob | `bootstrap_cluster` | SHIPPED true | #89 #119 #1227 | — |
| 182 | (#119) block length, in ~6-month cutoff buckets, for `bootstrap_edge_ci`, where 0 means DERIVE IT FROM THE HO… | knob | `bootstrap_block_buckets` | SHIPPED 0 | #89 #119 | — |
| 183 | (#88) the OTHER half of the real-vs-nominal skew, and the same shape: `core::backtest_quote` hardcoded an EMP… | knob | `backtest_anchor_windows` | OFF (false) | #88 #89 #119 #120 #149 #188 | TO REOPEN, AND IT IS A CODE FIX, NOT A KNOB |
| 184 | (Item 21) PROBE switch: when true, parse_chart prefers Yahoo's adjclose (split+DIVIDEND adjusted) over raw cl… | knob | `use_adjusted_close` | OFF (false) | #53 #266 #304 #804 #807 #808 | A re-open needs a PRE-REGISTERED total-return refit of `growth_min_cagr`, graded X vs A o… |
| 185 | (#804) the RULER half of `use_adjusted_close`, without its selection half: every backtest forward return comp… | knob | `backtest_tr_ruler` | OFF (false) | #120 #149 #188 #303 #304 #804 | TO REOPEN, AND IT IS A CODE FIX, NOT A KNOB |
| 186 | (4) NUPL above this starts damping crypto scores (~0.5 = "belief/denial" greed zone) | knob | `nupl_euphoria` | SHIPPED 0.5 |  | — |
| 187 | (4) crypto-score multiplier at NUPL=1.0 (full euphoria); 1.0 = damp off | knob | `nupl_damp_floor` | SHIPPED 0.5 |  | — |
| 188 | (4) NUPL below this starts BOOSTING crypto scores (~0.25 = fear/accumulation zone); 0 = boost off | knob | `nupl_capitulation` | SHIPPED 0.25 |  | — |
| 189 | (4) crypto-score multiplier at NUPL=0 (deep capitulation); 1.0 = boost off. BACKTEST-BLIND judgment lever — k… | knob | `nupl_boost_ceiling` | SHIPPED 1.3 |  | — |
| 190 | (#45) CRYPTO's valuation CEILING — reject a coin whose MVRV (market cap / realized cap) exceeds this. The cla… | knob | `crypto_max_mvrv` | SHIPPED 1.6 | #37 #45 #64 #395 #835 | — |
| 191 | (B) GROWTH lane: reward per unit of CAGR/volatility (return per unit of daily swing). 0 = off | knob | `sharpe_weight` | SHIPPED 0.05 | #60 #62 #63 #70 #178 #599 | — |
| 192 | (B) ON-SALE lane's own Sharpe weight — split from the growth lane because the shared knob conflicts: ablation… | knob | `onsale_sharpe_weight` | SHIPPED 0.0 | #60 #62 #63 #70 #178 #599 | — |
| 193 | (B) cap on that CAGR/volatility ratio fed into the reward | knob | `sharpe_cap` | SHIPPED 15.0 | #97 #100 | — |
| 194 | (#37) ETF-only Sharpe cap, LOWER than sharpe_cap: cross-listed lines of the same fund print different daily s… | knob | `sharpe_cap_etf` | SHIPPED 9.0 | #37 #97 #100 | — |
| 195 | (C) reward per unit of CAGR/max-drawdown (return per worst historical pain). 0 = off | knob | `calmar_weight` | SHIPPED 0.0 |  | — |
| 196 | (C) cap on that CAGR/max-drawdown ratio fed into the reward | knob | `calmar_cap` | SHIPPED 2.0 |  | — |
| 197 | dedup currency twins (BTC-EUR/BTC-USD): keep the EUR leg if true, else USD | knob | `prefer_eur` | SHIPPED true |  | — |
| 198 | (#1154) growth GATE (stocks only): lowers `growth_min_cagr` by this many %/yr per tenfold of market cap above… | knob | `growth_min_cagr_size_slope` | SHIPPED 0.0 | #192 #266 #316 #317 #318 #1154 | — |
| 199 | (#1154) the market cap at which `growth_min_cagr_size_slope` starts to lower the floor; at or below it the fl… | knob | `growth_min_cagr_size_pivot` | SHIPPED 10000000000 | #192 #266 #1154 #1156 | — |
| 200 | deleted after a measurement refused it | deleted knob | `growth_min_net_margin` | RETIRED | #38 #817 | new reason, see receipt |
| 201 | deleted after a measurement refused it | deleted knob | `growth_max_margin_swing` | RETIRED | #39 #817 | new reason, see receipt |
| 202 | deleted after a measurement refused it | deleted knob | `growth_max_dilution_pct` | RETIRED | #817 | new reason, see receipt |
| 203 | deleted after a measurement refused it | deleted knob | `growth_min_interest_cover` | RETIRED | #817 | new reason, see receipt |
| 204 | deleted after a measurement refused it | deleted knob | `growth_min_fcf_margin` | RETIRED | #817 | new reason, see receipt |
| 205 | deleted after a measurement refused it | deleted knob | `growth_min_net_cash_rev` | RETIRED | #817 | new reason, see receipt |
| 206 | deleted after a measurement refused it | deleted knob | `growth_soft_life_hard` | RETIRED | #568 #817 | new reason, see receipt |
| 207 | deleted after a measurement refused it | deleted knob | `growth_hard_life_floor` | RETIRED | #576 #817 | new reason, see receipt |
| 208 | deleted after a measurement refused it | deleted knob | `growth_hard_loss` | RETIRED | #576 #817 | new reason, see receipt |
| 209 | deleted after a measurement refused it | deleted knob | `growth_soft_drop_misses` | RETIRED | #582 #817 | new reason, see receipt |
| 210 | deleted after a measurement refused it | deleted knob | `growth_soft_hard_gates` | RETIRED | #566 #797 #802 #803 #817 | new reason, see receipt |
| 211 | deleted after a measurement refused it | deleted knob | `growth_overext_peg_waive` | RETIRED | #581 #817 | new reason, see receipt |
| 212 | deleted after a measurement refused it | deleted knob | `growth_overext_backed_floor` | RETIRED | #577 #817 | new reason, see receipt |
| 213 | deleted after a measurement refused it | deleted knob | `growth_overext_trend_adjust` | RETIRED | #579 #817 | new reason, see receipt |
| 214 | deleted after a measurement refused it | deleted knob | `growth_peg_taper_k` | RETIRED | #550 #817 | new reason, see receipt |
| 215 | deleted after a measurement refused it | deleted knob | `growth_red_flag_dock` | RETIRED | #729 #730 #817 | new reason, see receipt |
| 216 | deleted after a measurement refused it | deleted knob | `growth_momentum_miss_cap` | RETIRED | #585 #817 | new reason, see receipt |
| 217 | deleted after a measurement refused it | deleted knob | `growth_momentum_keep_age` | RETIRED | #586 #817 | new reason, see receipt |
| 218 | deleted after a measurement refused it | deleted knob | `growth_gate_on_tr_cagr` | RETIRED | #99 #303 #806 #817 | new reason, see receipt |
| 219 | deleted after a measurement refused it | deleted knob | `underwater_monthly` | RETIRED | #3 #223 #817 | new reason, see receipt |
| 220 | deleted after a measurement refused it | deleted knob | `hold_domicile_penalty` | RETIRED | #213 #226 #235 #267 #268 #817 | WHY THIS CLEARS (#235)'s BAR for re-opening a refused floor -- "a tier-set chosen for a r… |
| 221 | deleted after a measurement refused it | deleted knob | `backtest_calendar_cadence` | RETIRED | #117 #817 | new reason, see receipt |
| 222 | deleted after a measurement refused it | deleted knob | `growth_tr_life_weight` | RETIRED | #572 #818 | new reason, see receipt |
| 223 | deleted after a measurement refused it | deleted knob | `growth_tr_life_penalty` | RETIRED | #572 #818 | new reason, see receipt |
| 224 | deleted after a measurement refused it | deleted knob | `growth_tr_life_pivot` | RETIRED | #572 #818 | new reason, see receipt |
| 225 | deleted after a measurement refused it | deleted knob | `growth_er_weight` | RETIRED | #145 #818 | (#145) then found the same blindness had hollowed `growth_er_weight`'s composite and four… |
| 226 | deleted after a measurement refused it | deleted knob | `growth_er_cap` | RETIRED | #818 | new reason, see receipt |
| 227 | deleted after a measurement refused it | deleted knob | `growth_record_weight` | RETIRED | #133 #818 | new reason, see receipt |
| 228 | deleted after a measurement refused it | deleted knob | `growth_rank_normalise` | RETIRED | #144 #818 | new reason, see receipt |
| 229 | deleted after a measurement refused it | deleted knob | `growth_leg_dock` | RETIRED | #566 #818 | new reason, see receipt |
| 230 | deleted after a measurement refused it | deleted knob | `growth_base_offset` | RETIRED | #144 #150 #818 | new reason, see receipt |
| 231 | deleted after a measurement refused it | deleted knob | `growth_record_full_years` | RETIRED | #818 | new reason, see receipt |
| 232 | deleted after a measurement refused it | deleted knob | `growth_geomean_fold` | RETIRED | #8 #48 #819 | new reason, see receipt |
| 233 | deleted after a measurement refused it | deleted knob | `growth_max_trend_r2` | RETIRED | #1277 #1280 | new reason, see receipt |
| 234 | deleted after a measurement refused it | deleted knob | `growth_min_underwater_yrs` | RETIRED | #1277 #1280 #1286 | new reason, see receipt |
| 235 | deleted after a measurement refused it | deleted knob | `growth_untested_hard` | RETIRED | #1285 #1286 | new reason, see receipt |
| 236 | deleted after a measurement refused it | deleted knob | `growth_stretch_hard` | RETIRED | #1285 #1286 | new reason, see receipt |
| 237 | deleted after a measurement refused it | deleted knob | `growth_refuse_financials` | RETIRED | #1277 #1280 #1299 | new reason, see receipt |
| 238 | deleted after a measurement refused it | deleted knob | `growth_financial_max_asset_growth` | RETIRED | #1277 #1280 #1299 | new reason, see receipt |
| 239 | deleted after a measurement refused it | deleted knob | `growth_refuse_sector_top_dog` | RETIRED | #1277 #1280 #1299 | new reason, see receipt |
| 240 | deleted after a measurement refused it | deleted knob | `growth_consensus_min` | RETIRED | #1297 #1298 #1303 | new reason, see receipt |
| 241 | deleted after a measurement refused it | deleted knob | `growth_consensus_dock` | RETIRED | #1297 | new reason, see receipt |
| 242 | deleted after a measurement refused it | deleted knob | `growth_hard_tags` | RETIRED | #1297 | new reason, see receipt |
| 243 | deleted after a measurement refused it | deleted knob | `growth_max_sharpe` | RETIRED | #1297 #1298 #1303 | new reason, see receipt |
| 244 | deleted after a measurement refused it | deleted knob | `growth_max_rel_win` | RETIRED | #1303 | new reason, see receipt |
| 245 | deleted after a measurement refused it | deleted knob | `growth_min_cagr_rel` | RETIRED | #1298 #1306 #1308 | new reason, see receipt |
| 246 | deleted after a measurement refused it | deleted knob | `growth_min_cagr_pct` | RETIRED | #1298 | new reason, see receipt |
| 247 | deleted after a measurement refused it | deleted knob | `growth_momentum_max_marks` | RETIRED | #1234 #1311 #1315 | new reason, see receipt |
| 248 | deleted after a measurement refused it | deleted knob | `growth_max_fail_pct` | RETIRED | #1234 #1311 #1315 | new reason, see receipt |
| 249 | deleted after a measurement refused it | deleted knob | `growth_max_1a_change` | RETIRED | #1306 #1316 | new reason, see receipt |
| 250 | revenue CAGR over the lookback (proven top-line compounding) | fund factor | `rev_cagr` | MEASURED | #165 #676 #741 #742 | do not re-ask rev_cagr as a fund term at these lookback, so `fund 12` demanded 12y of sta… |
| 251 | last-1y revenue growth minus that long CAGR (top-line accelerating) | fund factor | `rev_accel` | MEASURED | #676 #1290 #1313 #1315 #1316 #1318 | new reason, see receipt |
| 252 | current gross margin level (pricing power / moat) | fund factor | `gross_margin` | MEASURED | #159 #165 #405 #407 | new reason, see receipt |
| 253 | current operating margin level (operating efficiency) | fund factor | `op_margin` | MEASURED |  | new reason, see receipt |
| 254 | op-margin now minus ~1y ago (margin expanding = strengthening) | fund factor | `margin_trend` | SHIPPED weight -0.3 | #1289 #1313 | — |
| 255 | EPS CAGR over the lookback (bottom-line compounding; both ends must be +) | fund factor | `eps_growth` | MEASURED |  | new reason, see receipt |
| 256 | last-1y revenue growth, % (the REV-YoY column) — `rev_accel`'s fast leg the printed columns (REV-YoY / EPS-Yo… | fund factor | `rev_yoy` | MEASURED | #3 | new reason, see receipt |
| 257 | last-1y EPS growth, % (the EPS-YoY column), off the row's OWN same-filing comparative -> split-proof the prin… | fund factor | `eps_yoy` | MEASURED |  | new reason, see receipt |
| 258 | current net margin level, % (the NET% column). Distinct from op_margin: below-the-line items (tax, interest,… | fund factor | `net_margin` | MEASURED | #38 #817 | new reason, see receipt |
| 259 | (Item 4) open-market buys minus sales (Form 4 P−S) in the 90d before the cutoff; populated only under `backte… | fund factor | `insider_net_buys_90d` | MEASURED |  | new reason, see receipt |
| 260 | (Item 19) EPS ÷ as-of price, % (valuation level, high = cheap). Set in the backtest loop from the native as-o… | fund factor | `earnings_yield` | MEASURED | #2 #3 #139 #185 #413 #1345 | new reason, see receipt |
| 261 | (PEG) 1/PEG = earnings_yield · as-of CAGR (high = cheap-for-its-growth). THE LIVE RANKING TILT since 2026-07-… | fund factor | `peg_yield` | SHIPPED weight 0.07 | #3 #159 #405 #407 #581 #817 | — |
| 262 | as-of net share-count change over the last year, sign-flipped (+ = shrinking share count = buying back). Full… | fund factor | `buyback_yield` | MEASURED | #3 #156 | new reason, see receipt |
| 263 | as-of return-on-equity level, % (quality of capital). SEC feed computes it per row (NetIncome ÷ StockholdersE… | fund factor | `roe` | MEASURED | #124 #157 | new reason, see receipt |
| 264 | as-of return-on-capital level, % — `quality_return(roe, roa, net_margin)`: ROE when equity is a credible deno… | fund factor | `quality` | MEASURED | #124 #157 | new reason, see receipt |
| 265 | (#43) return on INVESTED capital, pre-tax — `roic_return`, derived from levels already cached. The leverage-f… | fund factor | `roic` | SHIPPED weight 0.25 | #43 #124 #157 | — |
| 266 | (EV/EBITDA) EBITDA ÷ EV, % (high = cheap). PROBE-ONLY, None live (price skew, same as earnings_yield) (#157)… | fund factor | `ebitda_yield` | MEASURED | #124 #157 #165 | new reason, see receipt |
| 267 | % (op cash flow − capex) / revenue | fund factor | `fcf_margin` | MEASURED | #417 | new reason, see receipt |
| 268 | × operating income / interest expense | fund factor | `interest_cover` | MEASURED | #417 #550 | new reason, see receipt |
| 269 | % (cash − debt) / revenue | fund factor | `net_cash_rev` | MEASURED | #159 #550 | new reason, see receipt |
| 270 | (round 109) the cyclical detector: NEGATED sample stddev of net_margin across the as-of lookback rows (higher… | fund factor | `margin_stability` | MEASURED | #165 #407 #415 | new reason, see receipt |
| 271 | (P2) ACCRUAL GAP: how much of the as-of EPS is NOT backed by cash, NEGATED so high = safer, the same orientat… | fund factor | `accrual_gap` | MEASURED | #165 #410 | new reason, see receipt |
| 272 | (P3) ASSET GROWTH over the same `yrs` lookback `rev_cagr` uses, NEGATED so high = safer, like every other sur… | fund factor | `asset_growth` | MEASURED | #410 | new reason, see receipt |
| 273 | (#405) GROSS PROFITABILITY (Novy-Marx 2013): gross profit ÷ total assets, %, off the same as-of row. `gross_m… | fund factor | `gp_assets` | MEASURED | #165 #405 #417 #550 | new reason, see receipt |
| 274 | (#407) CASH-BASED PROFITABILITY (Ball-Gerakos-Linnainmaa-Nikolaev 2016), proxied: free cash flow ÷ total asse… | fund factor | `cf_assets` | MEASURED | #407 #417 #550 | new reason, see receipt |
| 275 | (#407) SALES-GROWTH STABILITY (Mohanram 2005, G-score signal G5): −sample std of year-over-year revenue growt… | fund factor | `rev_stability` | MEASURED | #165 #407 #412 #417 | 25 -> 27 ALSO READ, NOT RE-ASKED: held-book rev_stability AUC 12y/8y 0 |
| 276 | (#408) FCF ÷ EV, % (high = cheap). PROBE-ONLY, None live (price skew) (#408) O'Shaughnessy value family; 27 -… | fund factor | `fcf_ev_yield` | MEASURED | #408 #523 | 5 (n=64/125, win 78% at 8y), sales_ev_yield and the composite BANKED FOR THE ~2027-09 REO… |
| 277 | (#408) revenue ÷ EV, % (high = cheap). PROBE-ONLY, None live (#408) O'Shaughnessy value family; 27 -> 30 | fund factor | `sales_ev_yield` | MEASURED | #408 | new reason, see receipt |
| 278 | (#408) O'Shaughnessy value composite: mean same-date percentile (0-100) of earnings, EBITDA, FCF and sales yi… | fund factor | `value_composite` | MEASURED | #408 | new reason, see receipt |
| 279 | (#409) book ÷ market cap, % (high = cheap). PROBE-ONLY, None live (#409) Fama-French HML and size; 30 -> 32 | fund factor | `book_yield` | MEASURED | #409 | new reason, see receipt |
| 280 | (#409) same-date percentile of mcap, 0-100, high = SMALL. Backtest stamps (#409) Fama-French HML and size; 30… | fund factor | `small_pct` | MEASURED | #409 #415 #416 | new reason, see receipt |
| 281 | (#410) R&D ÷ market cap, % (high = R&D-rich). PROBE-ONLY, None live (#410) R&D/ME (Chan-Lakonishok-Sougiannis… | fund factor | `rnd_yield` | MEASURED | #49 #223 #410 #411 #413 #550 | new reason, see receipt |
| 282 | (#410) net payout ÷ market cap, %, sign kept. PROBE-ONLY, None live (#410) R&D/ME (Chan-Lakonishok-Sougiannis… | fund factor | `payout_yield` | MEASURED | #410 | new reason, see receipt |
| 283 | (#411) NET OPERATING ASSETS (Hirshleifer-Hou-Teoh-Zhang 2004): (net debt + book equity) ÷ total assets, %, NE… | fund factor | `noa` | MEASURED | #411 | new reason, see receipt |
| 284 | (#411) OPERATING LEVERAGE (Novy-Marx 2011): operating costs ÷ total assets, %, costs = revenue − operating in… | fund factor | `op_leverage` | MEASURED | #410 #411 | new reason, see receipt |
| 285 | (#412) Beneish-Lee-Nichols (FAJ 2013): a high M-score (`beneish_m`, as-of row vs a year earlier) predicts low… | fund factor | `mscore` | MEASURED | #412 | new reason, see receipt |
| 286 | (#412) Beneish M, SBC ÷ revenue, goodwill ÷ assets, all negated; 36 -> 39 | fund factor | `sbc` | MEASURED | #412 | new reason, see receipt |
| 287 | (#412) Beneish M, SBC ÷ revenue, goodwill ÷ assets, all negated; 36 -> 39 | fund factor | `goodwill` | MEASURED | #412 #417 | new reason, see receipt |
| 288 | (#413) intangible capital, SG&A (and R&D) accumulated by `capital_stock`. `org_cap` = organization capital (δ… | fund factor | `org_cap` | MEASURED | #413 | new reason, see receipt |
| 289 | (#413) Eisfeldt-Papanikolaou org capital ÷ assets, EKP intangible value; 39 -> 41 | fund factor | `int_yield` | MEASURED | #410 #412 #413 #550 | 71, BANKED FOR THE ~2027-09 REOPEN: if a value swap is weighed, int_yield vs peg_yield on… |
| 290 | (#416) earnings_yield · chained EPS CAGR (Lynch's PEG). Shadow only (#416) PEG over EPS growth, over ROE·rete… | fund factor | `lynch_yield` | MEASURED | #416 | new reason, see receipt |
| 291 | (#416) earnings_yield · `sgr_pct` (sustainable growth). Shadow only (#416) PEG over EPS growth, over ROE·rete… | fund factor | `sgr_yield` | MEASURED | #416 | new reason, see receipt |
| 292 | (#416) `ey_cagr`, high = the multiple CONTRACTED. PROBE-ONLY, None live (#416) PEG over EPS growth, over ROE·… | fund factor | `exp_neg` | MEASURED | #416 | new reason, see receipt |
| 293 | (#417) (gross profit − SG&A) ÷ assets, %; XBRL SG&A already excludes R&D (#417) Ball et al. R&D-adjusted OP ÷… | fund factor | `op_rd` | MEASURED | #417 | new reason, see receipt |
| 294 | (#417) 1y EBITDA growth − 1y asset growth, pts; high = profit outgrew assets (#417) Ball et al. R&D-adjusted… | fund factor | `discipline` | MEASURED | #412 #417 | new reason, see receipt |
| 295 | (#523) the FAIR family as factors, %: FAIR ÷ price (ranks like MOS%), the same on owner earnings (MOS-OE%), a… | fund factor | `fair_yield` | MEASURED | #522 #523 | new reason, see receipt |
| 296 | (#523) the page's FAIR family: FAIR ÷ price, FAIR-OE ÷ price, 3y FCF ÷ cap; 46 -> 49 | fund factor | `oe_fair_yield` | MEASURED | #523 | new reason, see receipt |
| 297 | (#523) the page's FAIR family: FAIR ÷ price, FAIR-OE ÷ price, 3y FCF ÷ cap; 46 -> 49 | fund factor | `fcf_cap_yield` | MEASURED | #415 #523 | new reason, see receipt |
| 298 | (#531) Ibbotson et al. 2013 popularity: minus a year's shares traded over the as-of share count (high = UNPOP… | fund factor | `low_turnover` | MEASURED | #407 #415 #531 | 49 -> 50 (#407) forbids a re-ask on this horizon pair |
| 299 | (#532) Arnott et al. 2026 "Fundamental Growth" / Eberhart-Maxwell-Siddique 2004: the 3y change in R&D over th… | fund factor | `rnd_growth` | MEASURED | #415 #532 | new reason, see receipt |
| 300 | (#533) Wang 2019 (JFE 133(2)) receivables days NEGATED: −receivables ÷ revenue × 365 (high = collects fast).… | fund factor | `low_dro` | MEASURED | #415 #533 | new reason, see receipt |
| 301 | (#549) capex ÷ operating cash flow NEGATED, % (high = capital-light: NVDA's ~5% reads −5). The ANNUAL twin of… | fund factor | `capex_light` | MEASURED | #545 #549 | new reason, see receipt |
| 302 | (#1278) three AUC-table rows, weighted nowhere. `share_iss_5y`: minus the 5y diluted share count CAGR, % (hig… | fund factor | `share_iss_5y` | MEASURED | #1278 | new reason, see receipt |
| 303 | (#1278) −net debt ÷ total assets: balance-sheet strength on the asset base; 54 -> 55 | fund factor | `netdebt_assets` | MEASURED | #1278 | new reason, see receipt |
| 304 | (#1278) fiscal years of rising EPS in a row, cap 8; 55 -> 56 | fund factor | `eps_streak` | MEASURED | #1278 | new reason, see receipt |
| 305 | (#1287) two AUC-table rows, weighted nowhere. `underdep`: Peddireddy 2021 under-depreciation, minus the 3y cu… | fund factor | `underdep` | MEASURED | #1287 | new reason, see receipt |
| 306 | (#1287) Grinblatt-Han 2005 capital-gains overhang over the turnover-weighted 5y price; 57 -> 58 | fund factor | `cgo` | MEASURED | #1287 | new reason, see receipt |
| 307 | (#1290) two more AUC-table rows, weighted nowhere. `ocf_at_chg`: operating cash flow over assets now minus a… | fund factor | `ocf_at_chg` | MEASURED | #1290 #1308 #1313 | new reason, see receipt |
| 308 | (#1290) Titman-Wei-Xie 2004 capex over revenue against its 3y mean; 59 -> 60 | fund factor | `capex_abn` | MEASURED | #1287 #1289 #1290 #1297 #1298 #1303 | new reason, see receipt |
| 309 | (#1301) directors' and officers' combined holding, % of the as-of share count. von Lilienfeld-Toal and Ruenzi… | fund factor | `insider_own_pct` | MEASURED | #1301 | new reason, see receipt |
| 310 | (Item 3) shows n/a until ≥2 factors are present | fund factor | `composite` | MEASURED |  | new reason, see receipt |
