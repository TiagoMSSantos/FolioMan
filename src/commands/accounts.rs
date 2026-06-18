//! `accounts` — read-only: cash available to invest + current holdings per broker.
//! Credentials from env (see `src/broker/`). A broker with no creds (or no API, like Trade
//! Republic) prints its skip/error reason instead of failing the whole command.

use crate::{broker, fetch};

pub async fn run(_args: Vec<String>) {
    let client = fetch::client_long();
    let (t212, bnc, tr) = tokio::join!(
        broker::trading212::summary(&client),
        broker::binance::summary(&client),
        broker::tr::summary(&client),
    );
    for (name, res) in [("Trading212", t212), ("Binance", bnc), ("Trade Republic", tr)] {
        println!("\n{name}:");
        match res {
            Ok(s) => println!("{s}"),
            Err(e) => println!("  (skipped) {e}"),
        }
    }
}
