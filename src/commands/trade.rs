//! `trade <broker> <buy|sell> <SYMBOL> <QTY>` — place a single **live MARKET order**.
//! REAL MONEY, IRREVERSIBLE. Credentials are read from environment variables only (see
//! `src/broker.rs`); nothing trade-related is ever read from `settings.yaml`.
//!
//! Brokers: `trading212` (ticker form `AAPL_US_EQ`), `binance` (pair like `BTCEUR`, qty in
//! base asset), `tr`/`traderepublic` (unofficial, login only — see broker docs).
//!
//! Every order prints a summary and requires typing `yes` to send — kept even in live mode
//! as a fat-finger guard, because the action can't be undone.

use crate::{broker, fetch};
use std::io::Write;

const USAGE: &str = "usage: folioman trade <trading212|binance|tr> <buy|sell> <SYMBOL> <QTY>";

pub async fn run(args: Vec<String>) {
    if args.len() != 4 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let broker_name = args[0].to_lowercase();
    let side = args[1].to_lowercase();
    let symbol = args[2].to_uppercase();
    let qty: f64 = match args[3].parse() {
        Ok(q) if q > 0.0 => q,
        _ => {
            eprintln!("QTY must be a positive number");
            std::process::exit(2);
        }
    };
    if side != "buy" && side != "sell" {
        eprintln!("side must be 'buy' or 'sell'");
        std::process::exit(2);
    }

    // irreversible-money confirm gate (kept in live mode on purpose — fat-finger guard)
    println!("⚠ REAL LIVE ORDER — {broker_name}: {side} {qty} {symbol}");
    print!("This spends real money and cannot be undone. Type 'yes' to send: ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    if input.trim() != "yes" {
        println!("aborted.");
        return;
    }

    let client = fetch::client_long();
    let res = match broker_name.as_str() {
        "trading212" | "t212" => broker::trading212::order(&client, &side, &symbol, qty).await,
        "binance" => broker::binance::order(&client, &side, &symbol, qty).await,
        "tr" | "traderepublic" => broker::tr::order(&client, &side, &symbol, qty).await,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match res {
        Ok(msg) => println!("OK: {msg}"),
        Err(e) => {
            eprintln!("ORDER FAILED: {e}");
            std::process::exit(1);
        }
    }
}
