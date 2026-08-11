//! folioman — review ETF/stock/crypto holdings. Read-only, never trades.
//!
//!   folioman check  [TICKERS...] [--explain [TICKER]]   price(EUR) + 1D/1W/1M/3M/6M/1Y/5Y/10Y/20Y % + market + headline; --explain prints TICKER's SCORE arithmetic without narrowing the watchlist view
//!   folioman perf   [TICKERS...]   per-ticker block: past price + % at each horizon (1Y+ % real terms when inflation_adjust on; past prices nominal)
//!   folioman screen [TICKERS...] [--explain [TICKER]]   scan live universe (CoinGecko + S&P500) growth ranking; --explain prints the SCORE arithmetic for TICKER (or the #1 row)
//!   folioman size   [TICKERS...]   suggested position sizes for the growth picks (weight ∝ score ÷ vol; read-only)
//!   folioman report [TICKERS...]   annual income-statement trajectory (revenue/margins/EPS/YoY) + grower profile (FMP key; only the valuation tilt is score-weighed)
//!   folioman backtest [YEARS] [TICKERS...|universe] [fund] [insider] [tune] [halflife] [stress]  walk-forward lanes vs peer-relative return + OOS + ablation; `tune` = honest train/test weight search; `fund` = FMP as-of fundamentals, `insider` = SEC Form-4 net buys, `halflife` = hold-period net-edge sweep, `stress` = inject crashed/delisted losers (survivorship check)
//!   folioman alert  [TICKERS...]   ntfy.sh push for tickers >= drop_pct below high
//!   folioman track [--push]        grade every past `screen` top-10 vs the S&P 500 at today's prices; --push also ntfys the summary (monthly cron)
//!   folioman sim                   paper-DCA the screen's advice: monthly_deploy_eur × entry state buys each month's first-snapshot top-10 (€1/name fee) vs an S&P 500 DCA of the same cashflows
//!   folioman accounts              cash + holdings per broker (read-only; env creds)
//!   folioman trade <broker> <buy|sell> <SYMBOL> <QTY>   LIVE order (real money, confirmed)
//!
//! No TICKERS -> uses config/settings.yaml watchlist. Edit that file for defaults.
//!
//! main.rs only dispatches: every module lives in the `folioman` library crate
//! (`src/lib.rs`); the buy heuristic in `picks.rs`, pure logic in `core.rs`, HTTP in
//! `fetch.rs`, one subcommand per file in `commands/`. Unit tests live in each module's
//! `#[cfg(test)] mod tests` (run by `cargo test`); `tests/` holds integration tests.

use folioman::{commands, config};

const USAGE: &str = "\
folioman — review ETF/stock/crypto holdings. Read-only, never trades.

  folioman check  [TICKERS...] [--explain [TICKER]]   price(EUR) + 1D/1W/1M/3M/6M/1Y/5Y/10Y/20Y % + market + headline; --explain prints TICKER's SCORE arithmetic without narrowing the watchlist view
  folioman perf   [TICKERS...]   per-ticker block: past price + % at each horizon (1Y+ % real terms when inflation_adjust on; past prices nominal)
  folioman screen [TICKERS...] [--explain [TICKER]]   scan live universe (CoinGecko + S&P500) growth ranking; --explain prints the SCORE arithmetic for TICKER (or the #1 row)
  folioman size   [TICKERS...]   suggested position sizes for the growth picks (weight ∝ score ÷ vol; read-only)
  folioman report [TICKERS...]   annual income-statement trajectory (revenue/margins/EPS/YoY) + grower profile (FMP key; only the valuation tilt is score-weighed)
  folioman backtest [YEARS] [TICKERS...|universe] [fund] [insider] [tune] [halflife] [stress]  walk-forward lanes vs peer-relative return + OOS + ablation; `tune` = honest train/test weight search; `fund` = FMP as-of fundamentals, `insider` = SEC Form-4 net buys, `halflife` = hold-period net-edge sweep, `stress` = inject crashed/delisted losers (survivorship check)
  folioman alert  [TICKERS...]   ntfy.sh push for tickers >= drop_pct below high
  folioman track [--push]        grade every past `screen` top-10 vs the S&P 500 at today's prices; --push also ntfys the summary (monthly cron)
  folioman sim                   paper-DCA the screen's advice: monthly_deploy_eur × entry state buys each month's first-snapshot top-10 (€1/name fee) vs an S&P 500 DCA of the same cashflows
  folioman accounts              cash available + holdings per broker (read-only; env creds)
  folioman trade <broker> <buy|sell> <SYMBOL> <QTY>   LIVE order (real money; brokers:
                                 trading212 | binance | tr — creds from env, confirmed y/N)

No TICKERS -> uses config/settings.yaml watchlist.";

/// Builds the runtime, then dispatches.
///
/// HAND-ROLLED rather than `#[tokio::main]` because that macro takes tokio's default of one worker
/// per logical CPU, which left `compute_threads` capping the rayon pool in `backtest` and nothing
/// else — the knob is documented as the way to cap FolioMan "while you use the machine for something
/// else", and half a cap is not one.
///
/// Only the WORKER pool is sized. The blocking pool stays at its default on purpose: it is where
/// reqwest resolves DNS (see the exit note below) and where the ~165MB FCA regex scan runs, and
/// `fetch_concurrency()` puts ~64 requests in flight against it. Capping it to a core count would
/// serialise name resolution — `compute_threads` sizes compute, `fetch_concurrency_multiplier` sizes
/// the network, and the two are kept apart deliberately (see `config::BuyHeuristic`).
///
/// UNGRADEABLE, hence the skip: every mutant cargo-mutants generates here is `delete match arm`, and
/// the arms are reachable only by running the binary — the `cli` suite, which `ci.yml`'s gate does not
/// run (it grades `--lib --test backtest_fixture`). Since `--in-diff` grades whole functions, without
/// this attribute any one-line edit in `main` drags all of them in and reds the gate. Same failure and
/// same fix as `commands::screen::run`.
#[mutants::skip]
fn main() {
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    rt.enable_all();
    // 0 (the default) = leave tokio's own sizing alone, mirroring what `thread_cap` does for rayon.
    let threads = config::compute_threads();
    if threads > 0 {
        rt.worker_threads(threads);
    }
    rt.build().expect("tokio runtime builds").block_on(dispatch());
    // note: work + output done; exit now instead of letting the tokio runtime drop block
    // ~10s on idle blocking DNS threads (reqwest resolves via getaddrinfo/spawn_blocking, whose
    // pool keep-alive is 10s). Read-only CLI, nothing buffered to flush. Drop this if we ever
    // need destructors to run on a normal command path.
    std::process::exit(0);
}

/// The subcommand table. Split out of `main` only so the runtime construction above is not itself
/// inside the `#[mutants::skip]`d body — this half is equally ungradeable by the same argument, so it
/// carries the same attribute rather than pretending otherwise.
#[mutants::skip]
async fn dispatch() {
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
        "track" => commands::track::run(rest).await,
        "sim" => commands::sim::run(rest).await,
        "trade" => commands::trade::run(rest).await,
        "help" | "--help" | "-h" => println!("{USAGE}"), // asked-for help exits 0 below; unknown commands keep exit 2
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    }
}
