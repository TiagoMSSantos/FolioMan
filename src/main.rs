//! folioman — review ETF/stock/crypto holdings. Read-only, never trades.
//!
//!   folioman check  [TICKERS...]   price(EUR) + 1D/1W/1M/3M/6M/1Y/5Y/10Y/20Y % + market + headline
//!   folioman perf   [TICKERS...]   per-ticker block: past price + % at each horizon
//!   folioman screen [TICKERS...] [--explain [TICKER]]   scan live universe (CoinGecko + S&P500) growth ranking; --explain prints the SCORE arithmetic for TICKER (or the #1 row)
//!   folioman size   [TICKERS...]   suggested position sizes for the growth picks (weight ∝ score ÷ vol; read-only)
//!   folioman report [TICKERS...]   annual income-statement trajectory (revenue/margins/EPS/YoY) + grower verdict
//!   folioman backtest [YEARS] [TICKERS...|universe] [fund] [insider] [tune] [halflife] [stress]  walk-forward lanes vs peer-relative return + OOS + ablation; `tune` = honest train/test weight search; `fund` = FMP as-of fundamentals, `insider` = SEC Form-4 net buys, `halflife` = hold-period net-edge sweep, `stress` = inject crashed/delisted losers (survivorship check)
//!   folioman alert  [TICKERS...]   ntfy.sh push for tickers >= drop_pct below high
//!   folioman accounts              cash + holdings per broker (read-only; env creds)
//!   folioman trade <broker> <buy|sell> <SYMBOL> <QTY>   LIVE order (real money, confirmed)
//!
//! No TICKERS -> uses config/settings.yaml watchlist. Edit that file for defaults.
//!
//! main.rs only dispatches: every module lives in the `folioman` library crate
//! (`src/lib.rs`); the buy heuristic in `picks.rs`, pure logic in `core.rs`, HTTP in
//! `fetch.rs`, one subcommand per file in `commands/`. Unit tests live in each module's
//! `#[cfg(test)] mod tests` (run by `cargo test`); `tests/` holds integration tests.

use folioman::commands;

const USAGE: &str = "\
folioman — review ETF/stock/crypto holdings. Read-only, never trades.

  folioman check  [TICKERS...]   price(EUR) + 1D/1W/1M/3M/6M/1Y/5Y/10Y/20Y % + market + headline
  folioman perf   [TICKERS...]   per-ticker block: past price + % at each horizon
  folioman screen [TICKERS...] [--explain [TICKER]]   scan live universe (CoinGecko + S&P500) growth ranking; --explain prints the SCORE arithmetic for TICKER (or the #1 row)
  folioman size   [TICKERS...]   suggested position sizes for the growth picks (weight ∝ score ÷ vol; read-only)
  folioman report [TICKERS...]   annual income-statement trajectory (revenue/margins/EPS/YoY) + grower verdict (FMP key)
  folioman backtest [YEARS] [TICKERS...|universe] [fund] [insider] [tune] [halflife] [stress]  walk-forward lanes vs peer-relative return + OOS + ablation; `tune` = honest train/test weight search; `fund` = FMP as-of fundamentals, `insider` = SEC Form-4 net buys, `halflife` = hold-period net-edge sweep, `stress` = inject crashed/delisted losers (survivorship check)
  folioman alert  [TICKERS...]   ntfy.sh push for tickers >= drop_pct below high
  folioman accounts              cash available + holdings per broker (read-only; env creds)
  folioman trade <broker> <buy|sell> <SYMBOL> <QTY>   LIVE order (real money; brokers:
                                 trading212 | binance | tr — creds from env, confirmed y/N)

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
        "size" => commands::size::run(rest).await,
        "report" => commands::report::run(rest).await,
        "backtest" => commands::backtest::run(rest).await,
        "alert" => commands::alert::run(rest).await,
        "accounts" => commands::accounts::run(rest).await,
        "trade" => commands::trade::run(rest).await,
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    }
    // note: work + output done; exit now instead of letting the tokio runtime drop block
    // ~10s on idle blocking DNS threads (reqwest resolves via getaddrinfo/spawn_blocking, whose
    // pool keep-alive is 10s). Read-only CLI, nothing buffered to flush. Drop this if we ever
    // need destructors to run on a normal command path.
    std::process::exit(0);
}
