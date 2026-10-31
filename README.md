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
cargo run -- check --explain KEYS      # full watchlist view + KEYS's SCORE arithmetic decomposed
cargo run -- perf NVDA          # past EUR price + % at each horizon, with source URL (1Y+ % real terms when inflation_adjust on)
cargo run -- screen             # live universe -> GROWTH ranking per class (stocks/ETFs/crypto) + NUPL
cargo run -- size               # suggested position sizes: the equal-weight BUY NOW book as shipped (sizing.equal_weight_book; off = score ÷ volatility inside a per-class budget, capped per name and per sector) (read-only; --watchlist sizes the typed list, --picks adds typed names to the ranked book)
cargo run -- report AAPL        # annual income-statement trajectory + as-of grower profile (only the valuation tilt is score-weighed)
cargo run -- backtest           # walk-forward validation: rho, top/bottom edge, OOS split, ablation
cargo run -- backtest 7 universe       # 7y-forward window, over the live screen universe (not the watchlist)
cargo run -- backtest universe fund    # add the as-of fundamentals lane (needs FMP_API_KEY, or key-free with fund_source: "sec")
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
ROE) come from SEC EDGAR for US filers, key-free; `FMP_API_KEY` in the environment adds a P/E
fallback for names SEC does not cover. The `fund` backtest lane needs the key unless
`fund_source: "sec"` is set. Where a number is missing that term stays neutral and everything else
still works.

### `trade` — live orders (real money, opt-in)

`folioman trade <broker> <buy|sell> <SYMBOL> <QTY|€AMOUNT>` places one **live market order**. It runs
only when you invoke it, prints the order, and waits for you to type `yes`. **Credentials come
from environment variables only — never `settings.yaml`:**

| Broker | Env vars | Notes |
|--------|----------|-------|
| Trading212 | `TRADING212_API_KEY` | Live endpoint = real money. Ticker form `AAPL_US_EQ`. |
| Binance | `BINANCE_API_KEY`, `BINANCE_API_SECRET` | Spot market order. Pair like `BTCEUR`, qty in base asset, or `€AMOUNT` to spend euros (`quoteOrderQty`, how `screen` prints coin orders). |
| Trade Republic | `TR_PHONE`, `TR_PIN`, `TR_ACCEPT_UNOFFICIAL=1` | No official API; login only, order placement not implemented — trade in the app. |

⚠️ Live trading is irreversible. Use withdrawal-disabled, IP-allowlisted keys; double-check
symbol/qty at the prompt; test with tiny quantities first.

### `alert` — dip notifications

Pushes to `ntfy.sh/<ntfy_topic>` when a ticker *crosses* ≥ `drop_pct` below its trailing high —
once per dip, not once per run (`.alert_dips` remembers who was pinged; a recovery clears the
name so its next dip pings again). Subscribe your phone at `https://ntfy.sh/<topic>`. The ntfy
topic is a secret (= your watchlist) — use a long random string. Cron hourly (the `cd` is only
there because the binary path is relative: dedup state and the track journal live at the repo
root, the directory holding `config/`, whatever the working directory, so cron and manual runs
share them):

```cron
0 * * * * cd /path/to/folioman && ./target/release/folioman alert >> /tmp/folioman.log 2>&1
```

The same run also pushes a **market entry-state flip ping** when the S&P 500 *crosses* a line:
near-high (<5% off its high) / pullback (5–15%) / drawdown (≥15%). Every transition pings once —
worsening says deploy new money faster (drawdown entries beat the index by +9.1 pts/yr vs +5.9
near the high in the 12y multi-regime backtest), recovery back to near-high says resume the
normal schedule. Deduped via `.alert_state` (repo root, gitignored), so a months-long drawdown
pings once per line crossed, not hourly; a failed push keeps the old state and retries next cron.
The `screen` command shows the same signal live — as a loud banner above the tables when the
market is ≥5% off its high, as a quiet footer near the high. Set `monthly_deploy_eur` (private
overlay; 0 = off) and `screen` also turns the state into a number: `DEPLOY THIS MONTH: €X` =
your monthly base × 1 near the high, × 1.5 in a pullback, × 2 in a drawdown — the "how much this
month?" arithmetic done for you. Not advice.

Below the tables, `screen` prints the top book as **paste-ready `trade` commands** (stocks/ETFs →
Trading212, crypto → Binance): the BUY NOW book, each row buying that deploy € × its weight. A
Trading212 row prints a quantity; a Binance row prints the euros to spend (`€26.37`), tagged when
it falls under Binance's €5 order minimum. Exact
Trading212 symbols are resolved from your held positions first, then from the full T212
instrument list (fetched with your API key, cached a week in `.t212_instruments.json`) by ISIN or
unique base symbol — only genuinely ambiguous or unlisted names still print a `<T212_SYMBOL>`
placeholder. Printing is all that happens here: each command still walks `trade`'s own real-money
confirm gate when you run it.

### `track` — live out-of-sample track record

Every `screen` run journals its ranked top slice (tickers, EUR prices, S&P 500 close) to
`.screen_snapshots.jsonl` — one line per day. `folioman track` grades every past BUY NOW book at
today's prices: its return at the sizes it was bought at vs the index over the same window (lines
older than the sized journal fall back to the top-10, equal-weight), plus a win-rate
summary. This is the screen's own advice graded on data that did not exist when it ranked — the
live counterpart of the backtest's held-book metric, and the honest answer to "does the ranking
actually work?" as evidence accrues. Price-only (dividends not counted, like the backtest);
delisted/unpriced names drop out and flatter the book — the `N` column keeps that visible.

`folioman track --push` additionally sends the one-line verdict to the same ntfy topic the
alerts use — cron it monthly so the record reaches your phone without a manual run (the cron
schedule is the dedup; a failed push just retries next month):

```cron
0 9 1 * * cd /path/to/folioman && ./target/release/folioman track --push >> /tmp/folioman.log 2>&1
```

### `sim` — paper-DCA of the screen's advice

`folioman sim` replays `.screen_snapshots.jsonl` as a pretend monthly saver: each month
`monthly_deploy_eur` times the entry-state multiplier buys that month's first BUY NOW book (else the
first snapshot's equal-weight top-10) at the journalled prices, less broker fees, and compounds it
against an S&P 500 DCA of the same cashflows. No state of its own; price-only; NOT advice.

### Run-to-run alerts (`screen`)

`screen` remembers its previous run in `.screen_state.json` (repo root, gitignored) and prints
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
fetch::fetch_universe   live universe: top-N crypto (CoinGecko) + S&P 500 + Euronext Lisbon + Xetra UCITS ETFs
                        + $1B+ US IPOs of the last 5 years  (screen only)
        ↓
fetch::quotes           one Quote per ticker (price, per-horizon %, volatility, SMA dist, P/E, ROE…)
        ↓
picks::eu_buyable       drop what EU retail can't buy (US-domiciled ETFs, Asia-only listings)
        ↓
picks::growth_score     GATE then SCORE each Quote (src/picks.rs) — the heuristic. A stock that fails
                        knob gates is scored with them waived and docked 0.8 per failed gate
                        (`growth_gate_soft`); the overextension brake damps names far above their 200wk SMA
        ↓
picks::ranked           dedup currency twins + dual-class shares, one fund per benchmark index, sort best-first
        ↓
picks::print_lane       split per class (stocks / ETFs / crypto), print Top-N each
        ↓
picks::buy_book         BUY NOW: a 25-name equal-weight book, the first five at 8 shares each
                        (`head_weight`), score names and 12-7 momentum names interleaved S M S M …
        ↓
NUPL + macro footer     Bitcoin sentiment + Euribor / Certificados de Aforro / inflation baselines
```

The page shows that book as the BUY% column: `b` marks a score row the book funds, `m` a momentum
row, and the Braked table lists the stocks the overextension brake alone keeps out. Uploading a
settings file re-ranks every table in the browser (`src/web.rs`, compiled to wasm).

There is a SECOND lane, `picks::buy_score` ("on-sale", buy-the-dip). **It is a backtest foil only —
neither `check` nor `screen` prints it.** It is the dip-buying control the growth lane is graded
against. Its held top-10 book beats the S&P 500 but trails the growth book at 12 and 8 years; it
leads at 20 years, on only 22 windows (receipt (#798)). See the `[FOIL]` tags in `settings.yaml` /
`src/config.rs`.

## Tuning the buy heuristic

Goal: change what the `screen` growth ranking surfaces. The shipped knobs live in
`tests/ci-settings.yaml`, which CI and the page read. A local `config/settings.yaml` (or the file
`FOLIOMAN_CONFIG` names) is merged over it, so it only needs the knobs you change. Edit
`buy_heuristic:` there, then **validate with `cargo run -- backtest`** or a `probe.yml` dispatch
with `knobs=` (watch rho = rank correlation, the top-vs-bottom-half edge in points, and that both
out-of-sample halves stay positive). Two rules:

- **Only knobs NOT tagged `[FOIL]` affect `screen`.** The `[FOIL]` knobs (`discount_*`,
  `momentum_*` but not `growth_momentum_*`, `min_score`, `cheap_*`, `sustained_*`, `min_1y_pct`,
  `min_long_pct*`, …) feed the
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
with no fundamental coverage) are detected and skipped so the search doesn't waste draws on them.

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
| Tilt toward cheap-on-earnings (SEC P/E for US filers; FMP fallback with `FMP_API_KEY`) | `ref_pe` | raise = treat more names as cheap |
| Tilt toward strong fundamentals (`FMP_API_KEY`, or key-free with `fund_source: "sec"`) | `growth_fund_weight` (cap `growth_fund_cap`) | shipped 0.07 (`peg_yield`); 0 = off |
| Admit fewer near-miss stocks | `growth_gate_soft` | lower (0 = every gate a hard wall) |
| Give the momentum side more or fewer of every 10 book slots | `growth_momentum_slots` | shipped 5 |

### Current status (2026-10): the defaults are a measured optimum

Every shipped knob was graded by CI's backtest gate (12y and 8y walk-forward, point-in-time and
stress pools), and each knob's receipt sits beside it in `tests/ci-settings.yaml`. The ship rule
for ANY knob change is the same bar: the edge must hold with BOTH out-of-sample halves positive,
or the change reverts.

Measured dead — don't re-run these:

- **Range gate below 80** — lost the same-batch triple three separate times.
- **R² steadiness floor or damp** — low-R² boom-bust names near their high BEAT the field at
  12y (the accel term is deliberately buying cyclical upswings).
- **Loosening the 1Y floor** — the names a 10-point looser floor admits average −108 pts
  forward. It ships at −0.3% (`growth_min_1y_pct`): a hard wall for ETFs, and one of the gates
  the 0.8 soft dock covers for stocks. Coins have their own floor (`min_1y_pct_crypto`).
- **Shorter horizons** — the edge is a LONG-horizon signal: negative under 3y, rising through
  10y. Selling early converts a validated signal into noise.
- **Every fundamentals factor through the fund lane except `peg_yield`** — margins, profitability,
  rev/eps growth, buyback yield and SEC-computed ROE all fail out-of-sample against the price-only
  baseline. `peg_yield` (1/PEG) at weight 0.07 is the only live fund tilt; it replaced the earlier
  `earnings_yield` tilt at 1.0. Form-4 insider net-buys fail too. Graded on SEC's bulk insider
  data sets (2006 on) by (#1247), their held book trails the index at 12y and 8y, and both
  out-of-sample halves trail the same-set score book (#1248).

The `[FOIL]` on-sale lane stays deliberately unimproved: it is the dip-buying control the growth
lane has to beat over a multi-decade hold (#798).

## Glossary

| Acronym | Meaning |
|---------|---------|
| ETF | Exchange-Traded Fund — a pooled basket traded like a stock (vs a single-company share) |
| CAGR | Compound Annual Growth Rate (%/yr; annualizes a multi-year return so 5Y/10Y/20Y compare) |
| ROE | Return On Equity — profitability/quality factor (SEC EDGAR filers only) |
| P/E | Price-to-Earnings ratio — valuation tilt `value = ref_pe / P/E` (SEC EDGAR; FMP fallback with `FMP_API_KEY`) |
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
| FMP | Financial Modeling Prep — the fallback P/E source for names SEC EDGAR does not cover (`FMP_API_KEY` env) |
| HICP | Harmonised Index of Consumer Prices — the EU inflation series for real-return adjustment |
| CdA | Certificados de Aforro — Portuguese savings certificates (a fixed-income baseline) |

## Hard rules

1. **No auto-trading** — `trade` is manual and confirm-gated; a signal never fires an order.
2. **Raw headlines, no sentiment NLP** (NLP = Natural Language Processing).
3. **Secrets in env, never in the repo** (broker keys come from env; the ntfy topic lives only in the
   gitignored `config/settings.yaml`, never in the committed `tests/ci-settings.yaml`).
