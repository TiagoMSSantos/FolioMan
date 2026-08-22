<!-- ============================================================================================
EDITORIAL NOTE, ADDED WHEN THIS PACK WAS SAVED INTO THE REPO — READ BEFORE FOLLOWING ANY TASK.

Every task below opens with "Read docs/heuristic-v2-plan.md section N". THAT FILE DOES NOT EXIST and
never reached the repo: the pack's own Setup block asked for it to be saved out of the conversation
that produced it, and that step was never done. It has NOT been reconstructed here, because each task
cites it as the justification for the gate it adds — writing a plausible substitute would be inventing
the evidence, which is precisely what the "every knob carries its receipt" rule exists to prevent.

What DOES exist is `docs/heuristic-v2-spec.md`, saved alongside this file. It carries the underlying
research, but under DIFFERENT section numbers, so the citations below do not resolve against it:

    task cites            what the SPEC actually numbers        where the content really is
    P0 -> section 2       2. Architecture                       spec 0.5 + 1  (lines 46-52, 65-74)
    P1 -> section 3       3. Track A - Single stocks            spec 3, table at line 151
    P2 -> section 4       4. Track B - Funds / ETFs             spec 1 line 68, G8 at line 114
    P3 -> section 5       5. Track C - Crypto                   spec 1 line 65, table at line 151
    P4 -> section 6       6. What "buy now" should output       no spec coverage found

Treat the section numbers below as DANGLING. The spec lines named above are the real sources.
============================================================================================= -->

# FolioMan v2 — Claude Code handoff pack

Companion to `docs/heuristic-v2-plan.md`. Each task below is a self-contained prompt: paste ONE into
Claude Code per session. Do not paste more than one — these are sequenced, and P0 changes the numbers
every later task is judged on.

---

## Setup (do this once)

```sh
cd /path/to/FolioMan
mkdir -p docs
# save the plan file from the Claude conversation as:
#   docs/heuristic-v2-plan.md
# save this file as:
#   docs/heuristic-v2-tasks.md
git add docs && git commit -m "docs: heuristic v2 plan + task pack"
```

Then create `CLAUDE.md` at the repo root (there isn't one — Claude Code reads it automatically on every
session, so the house rules below stop being something you have to re-explain):

```md
# FolioMan — house rules for agents

Read `docs/heuristic-v2-plan.md` before proposing changes to the buy heuristic.

## Non-negotiable
1. Every NEW knob defaults to the CURRENT behaviour. The shipped lane must stay byte-identical
   until a knob is deliberately turned on. `cargo test` goldens are the proof.
2. A gate added to `picks::score_parts` MUST be mirrored in `picks::gate_failures`.
   `gate_failures_agrees_with_the_scorer` enforces this — do not weaken that test.
3. If a gate's LIVE/INERT reachability changes, update `growth_gate_reachability_pin`
   (`src/commands/backtest.rs`). That test failing is the feature.
4. ONE definition per number. Never re-derive a CAGR/PEG/score term at a read site — route through the
   existing helper (`long_cagr_pct`, `capped_trend`, `life_leg_cagr`). See picks.rs:129-141 for why.
5. Missing data PASSES a gate. The only documented exception is the PEG/negative-EPS split.
6. Never tune or "improve" a `[FOIL]` knob. That lane exists to keep losing.
7. Secrets stay in env, never in yaml.

## Ship rule v2 (from tests/ci-settings.yaml — precedence GUARD > PRIMARY > TIE)
- PRIMARY: top-10 excess (mean AND median) up or held, at 20y AND 8y
- GUARD:   rank-1 h2h >= 50% at 20y, 12y and 8y
- GUARD:   top-10 worst window not worse
- TIE:     horizons disagree (one up, one down) -> UNRESOLVED -> NO SHIP
A broken guard REFUSES outright. It is never softened into "unresolved".

## Every knob carries its receipt
A new knob's comment in `tests/ci-settings.yaml` states: what it does, what was measured, at which
horizons, and the revert rule. An unmeasured value says so ("unfitted judgement value").
Mark any receipt measured before the point-in-time universe lands as `PRE-PIT`.
```

---

## Task P0 — point-in-time universe

```
Read docs/heuristic-v2-plan.md section 2, then implement the point-in-time universe reconstruction.

Scope:
- src/fetch.rs: add fetch_sp500_history() returning historical S&P 500 membership by date, cached to
  .sp500_history.json alongside the existing caches. Add a `urls.sp500_history` config key so the
  source is swappable. Evaluate BOTH candidate sources before choosing and tell me which you picked
  and why:
    (a) fja05680/sp500 — "S&P 500 Historical Components & Changes.csv", back to ~1996
    (b) the git history of datasets/s-and-p-500-companies (the repo urls.sp500_csv already points at)
  If neither is usable as-is, STOP and report rather than inventing a source.
- src/commands/backtest.rs: new `pit` arg (e.g. `backtest 12 universe pit`). When set, build each
  cutoff's pool from membership as of that cutoff instead of today's constituents CSV.
- A name that was a member but Yahoo no longer serves must be a COUNTED, PRINTED miss — not a silent
  skip. Print that count in the report next to the existing (#5) survivorship line. It is the residual
  bias and it must be visible.
- Default OFF. Every existing golden and receipt must stay byte-identical without `pit`.

Do NOT change any scoring knob in this task.

When it builds and tests pass, run and report side by side:
  cargo run --release -- backtest 20 universe fund
  cargo run --release -- backtest 20 universe fund pit
  (then the same at 12 and 8)
Report rho, edge, both OOS halves, top-10 excess mean+median, rank-1 h2h, windows scored, and the
new "membership names Yahoo no longer serves" count.

Expect the edge to FALL, possibly a lot at 12y/8y. That is a successful run, not a regression — the
2026-08-02 peer-mean fix is the precedent (-71.7 at 12y for a correctness fix). Do not tune anything
to recover it.
```

## Task P0b — re-sweep the fitted knobs on the PIT pool

```
With `pit` landed, re-sweep the four knobs whose values were fitted on the survivor pool. One knob at
a time, 5-rung ladder, `universe fund pit` at 20y/12y/8y, judged under the ship rule in CLAUDE.md:

  growth_min_cagr        19  ->  rungs 12 / 15 / 19 / 23 / 27
  growth_min_range_pct   80  ->  rungs 60 / 70 / 80 / 90 / off
  growth_accel_weight  0.50  ->  rungs 0.15 / 0.30 / 0.50 / 0.70 / 0.90
  growth_maxdd_cap       84  ->  rungs 70 / 78 / 84 / 90 / off

Put each arm in tests/<name>-ci-settings.yaml (NOT /tmp, NOT a job dir — see the cache-resolution
warning at the top of tests/ci-settings.yaml) and bracket every arm with a repeat of the shipped
config so the run-to-run noise floor is visible.

Ship nothing that fails a guard. Write each result into the knob's receipt comment, and update the
`DEBT, DELIBERATELY NOT PAID HERE` block in tests/ci-settings.yaml to say these are now PIT-measured.
```

## Task P1 — promote the four survival factors to gates

```
Read docs/heuristic-v2-plan.md section 3.

core::FundFactors already carries buyback_yield, fcf_margin, interest_cover and net_cash_rev, as-of on
both the backtest and live-enrich paths. They have only ever been measured as additive TILTS. Add them
as REJECTIONS.

Add four knobs (config.rs + tests/ci-settings.yaml), each defaulting to OFF so the base lane is
byte-identical:
  growth_max_dilution_pct   0.0    # 0 = off; reject when as-of 1y share count GREW more than this %
                                   # (reads -buyback_yield; buyback_yield is sign-flipped, + = shrinking)
  growth_min_interest_cover 0.0    # 0 = off; reject EBIT/interest below this
  growth_min_fcf_margin    -1e9    # off; reject a filer burning cash at the as-of filing
  growth_min_net_cash_rev  -1e9    # off; reject a balance sheet more levered than this

Implement in picks::score_parts inside the existing `// (#37)/(#38) VALUATION + MARGIN gates` block,
same `let ff = quote.fund.as_ref()` scope and the same is_some_and missing-data stance. Mirror all four
in gate_failures. Update growth_gate_reachability_pin.

Then sweep, ONE AT A TIME, `universe fund pit` at 20y/12y/8y, 5 rungs each:
  growth_max_dilution_pct    off / 10 / 5 / 2 / 0     <- run this one FIRST, highest prior
  growth_min_interest_cover  off / 2 / 3 / 5 / 8
  growth_min_fcf_margin      off / -10 / 0 / 5 / 10
  growth_min_net_cash_rev    off / -100 / -50 / -25 / 0

Then run the survivors JOINTLY — these gates reject overlapping cohorts so their individual deltas do
not add.

For each rung also report the REJECTED cohort's mean forward return vs kept peers (the same evidence
shape the #24 receipt uses). A gate that rejects names which then outperform is a bad gate no matter
what the edge says.
```

## Task P2 — accrual gate

```
Read docs/heuristic-v2-plan.md section 4.

FundRow already has revenue, fcf_margin, eps and shares, so no new SEC parsing is needed:
  fcf     = fcf_margin/100 * revenue
  fcf_ps  = fcf / shares
  accrual = (eps - fcf_ps) / max(|eps|, eps_floor)

Add FundFactors.accrual_gap, sign-oriented so HIGH = SAFER (carry -accrual), matching the round-107
convention used by margin_stability. Register it in core::select_fund_factor.

STOP THERE and run the standalone factor probe first:
  cargo run --release -- backtest 12 universe fund pit
and report accrual_gap's line from the FUNDAMENTAL standalone probes.

Only if that probe is non-null, add growth_max_accrual_gap as a rejection and sweep it 5 rungs.

Note in the knob doc: FundRow.shares is diluted weighted-average, not period-end, so fcf_ps is not a
quotable per-share FCF — it is consistent across the series, which is all the cross-sectional rank needs.
```

## Task P3 — asset growth

```
Read docs/heuristic-v2-plan.md section 5.

Asset growth has zero footprint in the tree — no gate, tilt, probe or column — yet total assets are
already being fetched: FundRow.roa is SEC-computed as netIncome/totalAssets, then the denominator is
discarded.

1. src/core.rs: carry FundRow.assets: Option<f64> from the same XBRL read that feeds roa.
2. FundFactors.asset_growth: CAGR of assets across the as-of lookback rows — same shape and same `yrs`
   lookback as the existing rev_cagr. Sign it NEGATED (high = safer = slow-growing asset base).
3. Register in select_fund_factor. Run the standalone probe FIRST and report it.
4. Only then add growth_max_asset_cagr as a gate: off / 40 / 30 / 20 / 15.

Run step 4's ablation TWICE: once with growth_accel_weight at its shipped 0.50 and once at 0.25.
Asset growth and revenue acceleration are correlated by construction, so I need to see whether this
gate substitutes for the lane's main term or fights it.
```

## Task P4 — MAX / equity volatility gate

```
Read docs/heuristic-v2-plan.md section 6.

Equities currently have no volatility ceiling (growth_max_vol_crypto is crypto-scoped) and no
single-day-extreme filter. MAX (largest single-day return in the trailing month) is a different signal
from both growth_max_above_ma (a level stretch) and trend_r2 (which was measured edge-NEGATIVE and must
not be re-litigated).

- Add Quote.max_daily_1m, filled from ONE shared helper used by both fetch.rs (live) and
  core::backtest_quote — do not re-derive it at either site.
- Add gates growth_max_daily_1m (off / 25 / 20 / 15 / 12) and growth_max_vol (equity twin of the crypto
  vol cap), both default off.
- Sweep under the usual protocol and report the rejected cohort's forward return vs peers.

My prior here is MODERATE and lower than P1-P3: this lane deliberately tolerates ugly price paths, and
the #23 degenerate-series gate already catches the worst artifact. If it measures null, record the null
in the knob doc and leave it at 0 — do not go looking for a variant that passes.
```

## Task P5 — size_weights risk budget

```
src/picks.rs size_weights() computes `budget = 100.0 / k` where k is the number of asset classes with
positive weight. One crypto name scoring positive therefore takes 33% of gross. There is no per-name
cap and no sector cap.

This is a POLICY fix, not a signal change — it never runs in the backtest and needs no validation.

- Replace the equal-per-class split with explicit config budgets, renormalised over the classes
  actually present: size_budget_stock: 70, size_budget_etf: 25, size_budget_crypto: 5.
- size_max_name_pct (default 4.0): clamp, redistribute the excess within the class, iterate to a
  fixpoint. Cap the loop at ~5 passes.
- size_max_sector_pct (default 25.0): Quote.sector is populated live (stamp_asset_class proved it),
  so cap the stock class by sector.
- Print the binding constraint next to each clamped row (`cap: name` / `cap: sector`). A silently
  clamped weight is exactly the kind of thing that rots unnoticed.

Update the `size` command's header line so it no longer claims a plain score/vol split.
```

---

## If you'd rather hand this over as GitHub issues

From your machine, with `gh` authenticated:

```sh
gh issue create --repo TiagoMSSantos/FolioMan \
  --title "heuristic v2 P0: point-in-time universe reconstruction" \
  --body-file - <<'EOF'
<paste the Task P0 block above>
EOF
```

Then in Claude Code: `implement issue #N` — it will pull the body itself.
