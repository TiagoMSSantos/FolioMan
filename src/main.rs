//! folioman — review ETF/stock/crypto holdings. Read-only, never trades.
//!
//!   folioman check  [TICKERS...]   price(EUR) + 1D/1W/1M/3M/6M/1Y/5Y/10Y/20Y % + market + headline
//!   folioman perf   [TICKERS...]   per-ticker block: past price + % at each horizon
//!   folioman screen [TICKERS...]   scan live universe (CoinGecko + S&P500): ATH/ATL / fallers / dividends / buys
//!   folioman backtest [YEARS] [TICKERS...|universe]  walk-forward on-sale+growth lanes vs peer-relative return + OOS + ablation
//!   folioman alert  [TICKERS...]   ntfy.sh push for tickers >= drop_pct below high
//!   folioman accounts              cash + holdings per broker (read-only; env creds)
//!   folioman trade <broker> <buy|sell> <SYMBOL> <QTY>   LIVE order (real money, confirmed)
//!   folioman selftest              run internal asserts (no network)
//!
//! No TICKERS -> uses config/settings.yaml watchlist. Edit that file for defaults.
//!
//! main.rs only dispatches: every module lives in the `folioman` library crate
//! (`src/lib.rs`); the buy heuristic in `picks.rs`, pure logic in `core.rs`, HTTP in
//! `fetch.rs`, one subcommand per file in `commands/`. Tests live in `tests/`.

use folioman::{broker, commands, core, picks};

const USAGE: &str = "\
folioman — review ETF/stock/crypto holdings. Read-only, never trades.

  folioman check  [TICKERS...]   price(EUR) + 1D/1W/1M/3M/6M/1Y/5Y/10Y/20Y % + market + headline
  folioman perf   [TICKERS...]   per-ticker block: past price + % at each horizon
  folioman screen [TICKERS...]   scan live universe (CoinGecko + S&P500): ATH/ATL / fallers / dividends / buys
  folioman backtest [YEARS] [TICKERS...|universe]  walk-forward on-sale+growth lanes vs peer-relative return + OOS + ablation
  folioman alert  [TICKERS...]   ntfy.sh push for tickers >= drop_pct below high
  folioman accounts              cash available + holdings per broker (read-only; env creds)
  folioman trade <broker> <buy|sell> <SYMBOL> <QTY>   LIVE order (real money; brokers:
                                 trading212 | binance | tr — creds from env, confirmed y/N)
  folioman selftest              run internal asserts (no network)

No TICKERS -> uses config/settings.yaml watchlist.";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("check");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match cmd {
        "check" => commands::check::run(rest).await,
        "perf" => commands::perf::run(rest).await,
        "screen" => commands::screen::run(rest).await,
        "backtest" => commands::backtest::run(rest).await,
        "alert" => commands::alert::run(rest).await,
        "accounts" => commands::accounts::run(rest).await,
        "trade" => commands::trade::run(rest).await,
        "selftest" => {
            core::selftest();
            picks::selftest();
            broker::selftest();
            folioman::fetch::selftest();
            println!("ok");
        }
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    }
    // ponytail: work + output done; exit now instead of letting the tokio runtime drop block
    // ~10s on idle blocking DNS threads (reqwest resolves via getaddrinfo/spawn_blocking, whose
    // pool keep-alive is 10s). Read-only CLI, nothing buffered to flush. Drop this if we ever
    // need destructors to run on a normal command path.
    std::process::exit(0);
}
