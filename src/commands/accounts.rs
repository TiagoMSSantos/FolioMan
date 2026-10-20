//! `accounts` — read-only: cash available to invest + current holdings per broker.
//! Credentials from env (see `src/broker/`). A broker with no creds (or no API, like Trade
//! Republic) prints its skip/error reason instead of failing the whole command.

use crate::{broker, fetch};

/// (#669) `#[mutants::skip]` for the reason spelled out on `track::run`: a command entry point is
/// reachable only through `--test cli`, which the gate does not run, so one edited line in here would
/// red the gate on its own.
#[mutants::skip]
pub async fn run(_args: Vec<String>) {
    let client = fetch::client_long();
    let (t212, bnc, tr) = tokio::join!(
        broker::trading212::summary(&client),
        broker::binance::summary(&client),
        broker::tr::summary(&client),
    );
    for (name, keyed, res) in [("Trading212", true, t212), ("Binance", true, bnc), ("Trade Republic", false, tr)] {
        println!("\n{name}:");
        match res {
            Ok(s) => println!("{s}"),
            Err(e) => println!("  {} {e}", err_tag(keyed, &e)),
        }
    }
}

/// (#772) The tag before a broker's `Err`. A key not set is the broker switched off, and Trade Republic
/// has no read API at all, so both are a skip. Anything else failed with its key set, and the cash and
/// holdings it would have listed are missing from this report.
fn err_tag(keyed: bool, err: &str) -> &'static str {
    if keyed && !broker::is_keyless(err) {
        "ERROR:"
    } else {
        "(skipped)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_tag_skips_only_a_broker_that_is_off() {
        assert_eq!(err_tag(true, "trading212 401 Unauthorized: {}"), "ERROR:");
        assert_eq!(err_tag(true, &broker::env_var("FOLIOMAN_NO_SUCH_BROKER_CREDENTIAL").unwrap_err()), "(skipped)");
        assert_eq!(err_tag(false, "Trade Republic has no official API"), "(skipped)");
    }
}
