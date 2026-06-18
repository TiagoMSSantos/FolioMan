# folioman — investment CLI toolkit (Rust)

Personal CLI to review ETF/stock/crypto holdings, catch dips, and check yields, rates and
inflation. **Read-only by default**; the opt-in `trade` command places **manual, confirm-gated
live orders** (no auto-trading).

## Usage

```sh
# needs a Rust toolchain: https://rustup.rs
cargo build --release
cargo test                      # pure-logic asserts (one file per module)
cargo run -- selftest           # same asserts, no network

cargo run -- check              # default watchlist: price(EUR), % per horizon, trend, headline,
                                #   then a "buy candidates" ranking, Euribor, CdA, inflation
cargo run -- check AAPL BTC-USD VWCE.DE
cargo run -- perf NVDA          # past EUR price + % at each horizon, with source URL
cargo run -- screen             # all-time highs/lows, fallers (1M/3M/6M/1Y), dividend payers
cargo run -- alert              # ntfy push for dips (cron it)
cargo run -- accounts           # cash + holdings per broker (read-only)
cargo run -- trade binance buy BTCEUR 0.001   # LIVE order, real money, confirm-gated

# installed binary finds config/settings.yaml next to or above it
./target/release/folioman check
```

Edit `config/settings.yaml` for the watchlist, the broader `screen` universe, thresholds, table
widths, the buy-heuristic knobs, and all data-source URLs. `n/a` in a column = history doesn't
reach that far. Outputs are a transparent ranking of public data — **not investment advice**.

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

## Hard rules

1. **No auto-trading** — `trade` is manual and confirm-gated; a signal never fires an order.
2. **Raw headlines, no sentiment NLP.**
3. **Secrets in env, never in the repo** (broker keys + ntfy topic stay out of `settings.yaml`).
