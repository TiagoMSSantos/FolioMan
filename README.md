# folioman — investment CLI toolkit (Rust)

Personal CLI to review ETF/stock/crypto holdings, catch dips, and check yields, rates and
inflation. **Read-only by default**; the opt-in `trade` command places **manual, confirm-gated
live orders** (no auto-trading).

## Usage

```sh
# needs a Rust toolchain: https://rustup.rs
cargo build --release
cargo test                      # unit tests (#[cfg(test)] mod tests per module) + tests/ integration

cargo run -- check              # default watchlist: price(EUR), % per horizon, trend, headline,
                                #   then the GROWTH ranking (buy candidates), Euribor, CdA, inflation
cargo run -- check AAPL BTC-USD VWCE.DE
cargo run -- perf NVDA          # past EUR price + % at each horizon, with source URL
cargo run -- screen             # live universe -> GROWTH ranking per class (stocks/ETFs/crypto) + NUPL
cargo run -- size               # suggested position sizes for the growth picks (score ÷ volatility, read-only)
cargo run -- report AAPL        # annual income-statement trajectory + the grower verdict screen bets on
cargo run -- backtest           # walk-forward validation: rho, top/bottom edge, OOS split, ablation
cargo run -- backtest 7 universe       # 7y-forward window, over the live screen universe (not the watchlist)
cargo run -- backtest universe fund    # add the as-of fundamentals lane (needs FMP_API_KEY)
cargo run -- backtest universe tune    # honest train/test weight search (ships nothing unless it beats defaults)
cargo run -- alert              # ntfy push for dips (cron it)
cargo run -- accounts           # cash + holdings per broker (read-only)
cargo run -- trade binance buy BTCEUR 0.001   # LIVE order, real money, confirm-gated

# installed binary finds config/settings.yaml next to or above it
./target/release/folioman check
```

Edit `config/settings.yaml` for the watchlist, thresholds, table widths, the buy-heuristic
knobs, and all data-source URLs. `screen` builds its universe **live** (top-N crypto from
CoinGecko + the S&P 500 constituents CSV + the Euronext Lisbon equities + top-N EU UCITS ETFs from
Börse Frankfurt — tune `universe_size`; `screen TICKER...` overrides),
so there's no hand-kept list to maintain. `n/a` in a column = history doesn't reach that far.
Outputs are a transparent ranking of public data — **not investment advice**.

Set `FOLIOMAN_CONFIG=/path/to/settings.yaml` to override where the config is loaded from (CI
points it at a secret-free `tests/ci-settings.yaml` fixture this way). Fundamentals columns (P/E,
ROE, and the `fund` backtest lane) need `FMP_API_KEY` in the environment; without it those terms
stay neutral and everything else still works.

### `trade` — live orders (real money, opt-in)

`folioman trade <broker> <buy|sell> <SYMBOL> <QTY>` places one **live market order**. It runs
only when you invoke it, prints the order, and waits for you to type `yes`. **Credentials come
from environment variables only — never `settings.yaml`:**

| Broker | Env vars | Notes |
|--------|----------|-------|
| Trading212 | `TRADING212_API_KEY` | Live endpoint = real money. Ticker form `AAPL_US_EQ`. |
| Binance | `BINANCE_API_KEY`, `BINANCE_API_SECRET` | Spot market order. Pair like `BTCEUR`, qty in base asset. |
| Trade Republic | `TR_PHONE`, `TR_PIN`, `TR_ACCEPT_UNOFFICIAL=1` | No official API; login only, order placement not implemented — trade in the app. |

⚠️ Live trading is irreversible. Use withdrawal-disabled, IP-allowlisted keys; double-check
symbol/qty at the prompt; test with tiny quantities first.

### `alert` — dip notifications

Pushes to `ntfy.sh/<ntfy_topic>` for each ticker ≥ `drop_pct` below its trailing high. Subscribe
your phone at `https://ntfy.sh/<topic>`. The ntfy topic is a secret (= your watchlist) — use a
long random string. Cron hourly:

```cron
0 * * * * /path/to/folioman/target/release/folioman alert >> /tmp/folioman.log 2>&1
```

The same run also pushes a **market entry-state flip ping** when the S&P 500 *crosses* a line:
near-high (<5% off its high) / pullback (5–15%) / drawdown (≥15%). Every transition pings once —
worsening says deploy new money faster (drawdown entries beat the index by +9.1 pts/yr vs +5.9
near the high in the 12y multi-regime backtest), recovery back to near-high says resume the
normal schedule. Deduped via `.alert_state` (working dir, gitignored), so a months-long drawdown
pings once per line crossed, not hourly; a failed push keeps the old state and retries next cron.
The `screen` command shows the same signal live — as a loud banner above the tables when the
market is ≥5% off its high, as a quiet footer near the high.

### Run-to-run alerts (`screen`)

`screen` remembers its previous run in `.screen_state.json` (working dir, gitignored) and prints
a footer alert when something a long-term holder must react to changed since then — each is
**review, not auto-sell**:

- **Exit review** — a watchlist name that passed every growth gate last run fails one now
  (measured: newly-failing names lag kept-passing peers by ~14 pts forward).
- **Fund-fact drift** — TER hike or AUM collapse on watchlist + hold-suitable funds (fee creep,
  closure risk).
- **Structure changes** — USE Acc→Dist (payouts turn taxable yearly) or replication
  physical→Swap (counterparty risk enters).
- **CORE shortlist diff** and **ranking membership diff** — which names joined/dropped the
  buy-and-hold shortlist and the ranked tables since the last run, by ticker.

Every alert is also appended to `.screen_alerts.log` (permanent, append-only journal — a
scrolled-past terminal loses nothing; `grep VUAA .screen_alerts.log` is that fund's event
history). Deleting `.screen_state.json` resets all baselines (next run is a quiet first run);
a corrupt or unwritable state file is detected and warned about, never silently ignored.

## How a buy candidate is computed

`check` and `screen` both print the **growth ranking** — the proven-compounder lane. The pipeline
(function names so you can jump into the source):

```
fetch::fetch_universe   live universe: top-N crypto (CoinGecko) + S&P 500 + Euronext Lisbon + Xetra UCITS ETFs  (screen only)
        ↓
fetch::quotes           one Quote per ticker (price, per-horizon %, volatility, SMA dist, P/E, ROE…)
        ↓
picks::eu_buyable       drop what EU retail can't buy (US-domiciled ETFs, Asia-only listings)
        ↓
picks::growth_score     GATE then SCORE each Quote (src/picks.rs) — the heuristic
        ↓
picks::ranked           dedup currency twins + dual-class shares, sort best-first, trim padding
        ↓
picks::print_lane       split per class (stocks / ETFs / crypto), print Top-N each
        ↓
NUPL + macro footer     Bitcoin sentiment + Euribor / Certificados de Aforro / inflation baselines
```

There is a SECOND lane, `picks::buy_score` ("on-sale", buy-the-dip). **It is a backtest foil only —
neither `check` nor `screen` prints it.** It exists so `backtest` can show that dip-buying has
*negative* edge over a multi-decade hold. See the `[FOIL]` tags in `settings.yaml` / `src/config.rs`.

## Tuning the buy heuristic

Goal: change what the `screen` growth ranking surfaces. Edit `buy_heuristic:` in
`config/settings.yaml`, then **validate with `cargo run -- backtest`** (watch rho = rank
correlation, the top-vs-bottom-half edge in points, and that both out-of-sample halves stay
positive). Two rules:

- **Only knobs NOT tagged `[FOIL]` affect `screen`.** The `[FOIL]` knobs (`discount_*`,
  `momentum_*`, `min_score`, `cheap_*`, `sustained_*`, `min_1y_pct`, `min_long_pct*`, …) feed the
  backtest-only on-sale lane. Tuning them changes nothing you see.
- **Gates move the signal; weights barely do.** A gate reshapes the scored pool; additive/multiplier
  weights only re-rank what's already in it. Tune gates first.

Hand-tuning while watching the full backtest is **in-sample** — you've peeked at all the data, so the
reported edge is optimistic. `backtest <set> tune` is the honest check: it splits the samples
chronologically (~70% early = train, 30% late = test), searches the LIVE growth weights on **train
only**, then reports rho/edge on the held-out **test** split next to the current default's test
numbers. Overfit shows up as train ≫ test. It **ships nothing** — if no searched config beats the
default out-of-sample, the defaults already generalize (a clean null is a successful run). Copy a
genuine winner into `settings.yaml` yourself; `tune` never writes. Inert knobs (e.g. the fund weight
with no `FMP_API_KEY` coverage) are detected and skipped so the search doesn't waste draws on them.

| Want… | Turn this (LIVE knob) | Direction |
|-------|-----------------------|-----------|
| Fewer, higher-conviction stock picks | `growth_min_cagr`, `growth_min_range_pct` | raise |
| Hide weak ranked rows | `growth_min_score` | raise |
| More crypto coins in the table | `growth_min_range_pct_crypto`, `growth_min_cagr_crypto` | lower |
| Punish blow-off-top / parabolic names harder | `growth_overext_floor` | lower |
| Drop thin/illiquid junk | `min_avg_turnover_eur` | raise |
| Favor deep-liquid mega-caps on ties | `growth_turnover_weight` | raise |
| Damp crypto when the market is euphoric | `nupl_euphoria` / `nupl_damp_floor` | lower the floor |
| Reward dividend payers more | `dividend_weight` (cap `dividend_cap`) | raise |
| Tilt toward cheap-on-earnings (needs `FMP_API_KEY`) | `ref_pe` | raise = treat more names as cheap |
| Tilt toward strong fundamentals (`FMP_API_KEY`, or key-free with `fund_source: "sec"`) | `growth_fund_weight` (cap `growth_fund_cap`) | shipped 1.0 (`earnings_yield`); 0 = off |

### Current status (2026-07): the defaults are a measured optimum

The shipped defaults were validated by the 12y walk-forward: growth edge ~+170 pts with both
out-of-sample halves positive; a held top-10 book compounds ~+15.0%/yr vs the S&P's ~8.0. The
ship rule for ANY knob change is the same bar: the edge must hold with BOTH out-of-sample
halves positive, or the change reverts.

Measured dead — don't re-run these:

- **Range gate below 80** — lost the same-batch triple three separate times.
- **R² steadiness floor or damp** — low-R² boom-bust names near their high BEAT the field at
  12y (the accel term is deliberately buying cyclical upswings).
- **Removing the 1Y>0 floor** — the names it would admit average −108 pts forward; it is one
  of the lane's most protective gates.
- **Shorter horizons** — the edge is a LONG-horizon signal: negative under 3y, rising through
  10y. Selling early converts a validated signal into noise.
- **Every fundamentals factor through the fund lane except `earnings_yield`** — margins, rev/eps
  growth, buyback yield, SEC-computed ROE, and Form-4 insider net-buys all fail out-of-sample
  against the price-only baseline on the full S&P pool.

The `[FOIL]` on-sale lane stays deliberately unimproved: its job is to keep proving that
dip-buying loses to the growth lane over a multi-decade hold.

One exception shipped 2026-07-12: the `earnings_yield` fund tilt (`growth_fund_weight: 1.0`,
SEC-sourced) is ON. It beat the price-only baseline with both out-of-sample halves positive in
three independent-pool wide runs, and the on-vs-off ship test on the serving universe improved
the growth edge (+170.3 vs +156.9, OOS +0.14|+0.20) and the held top-10 book (+15.0 vs
+14.7%/yr) with the worst 12y hold unchanged. Revert rule: a future wide run losing an
out-of-sample half sets the weight back to 0. The one earlier contradicting run (2026-07-02)
traced to baseline variance — the factor's own lane edge held +137..+157 in all four runs.

## Glossary

| Acronym | Meaning |
|---------|---------|
| ETF | Exchange-Traded Fund — a pooled basket traded like a stock (vs a single-company share) |
| CAGR | Compound Annual Growth Rate (%/yr; annualizes a multi-year return so 5Y/10Y/20Y compare) |
| ROE | Return On Equity — profitability/quality factor (needs `FMP_API_KEY`) |
| P/E | Price-to-Earnings ratio — valuation tilt `value = ref_pe / P/E` (needs `FMP_API_KEY`) |
| NUPL | Net Unrealized Profit/Loss — Bitcoin whole-market sentiment gauge (damps the crypto lane) |
| SMA | Simple Moving Average; "200wk SMA" = the ~1000-session long-term trend line |
| FX | Foreign exchange (currency-quoted tickers carry a `-EUR`/`-USD` suffix: crypto/FX) |
| UCITS | EU-regulated pooled-fund wrapper; EU retail can buy these (vs US-domiciled ETFs) |
| PRIIPs / KID | EU retail-disclosure document a fund needs to be sold to EU retail |
| GICS | Global Industry Classification Standard — the sector taxonomy the sector filter uses |
| Sharpe | Risk-adjusted return = CAGR ÷ volatility (reward per unit of daily swing) |
| Calmar | Risk-adjusted return = CAGR ÷ worst peak-to-trough drawdown (reward per unit of pain) |
| ATH / ATL | All-Time High / All-Time Low |
| OFF-HI | How far below its high a name trades (the `screen` drawdown column) |
| rho | Spearman rank correlation — the backtest's selection-skill metric |
| OOS | Out-Of-Sample — the late-vs-early split that tests whether an edge generalizes |
| FMP | Financial Modeling Prep — the fundamentals API behind P/E and ROE (`FMP_API_KEY` env) |
| HICP | Harmonised Index of Consumer Prices — the EU inflation series for real-return adjustment |
| CdA | Certificados de Aforro — Portuguese savings certificates (a fixed-income baseline) |

## Hard rules

1. **No auto-trading** — `trade` is manual and confirm-gated; a signal never fires an order.
2. **Raw headlines, no sentiment NLP** (NLP = Natural Language Processing).
3. **Secrets in env, never in the repo** (broker keys + ntfy topic stay out of `settings.yaml`).
