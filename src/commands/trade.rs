//! `trade <broker> <buy|sell> <SYMBOL> <QTY>` — place a single **live MARKET order**.
//! REAL MONEY, IRREVERSIBLE. Credentials are read from environment variables only (see
//! `src/broker.rs`); nothing trade-related is ever read from `settings.yaml`.
//!
//! Brokers: `trading212` (ticker form `AAPL_US_EQ`, sent EXACTLY as typed — venue letters are
//! meaningfully lowercase, e.g. `VUAGl_EQ`), `binance` (pair like `BTCEUR`, qty in base asset),
//! `tr`/`traderepublic` (unofficial, login only — see broker docs).
//!
//! Every order prints a summary and requires typing `yes` to send — kept even in live mode
//! as a fat-finger guard, because the action can't be undone.

use crate::{broker, fetch};
use std::io::Write;

const USAGE: &str = "usage: folioman trade <trading212|binance|tr> <buy|sell> <SYMBOL> <QTY>";

/// Parse + validate the order args: (broker, side, symbol, qty). EVERYTHING is validated here —
/// including the broker name — so a bad invocation errors BEFORE the real-money confirm prompt,
/// never after the user has already typed `yes`.
fn parse_order(args: &[String]) -> Result<(String, String, String, f64), String> {
    if args.len() != 4 {
        return Err(USAGE.to_string());
    }
    let broker_name = args[0].to_lowercase();
    if !matches!(broker_name.as_str(), "trading212" | "t212" | "binance" | "tr" | "traderepublic") {
        return Err(USAGE.to_string());
    }
    let side = args[1].to_lowercase();
    if side != "buy" && side != "sell" {
        return Err("side must be 'buy' or 'sell'".to_string());
    }
    // Symbol case is broker POLICY, not cosmetics. Binance pairs are genuinely uppercase
    // (btceur -> BTCEUR stays a typing convenience) and TR takes ISINs (uppercase by spec) —
    // but Trading212 tickers carry MEANINGFUL lowercase venue letters (`VUAGl_EQ`, l = LSE),
    // so a T212 symbol passes VERBATIM: transforming a real-money symbol could reject the
    // order, or worse, name a different instrument. Paste the order-glue line as printed.
    let symbol = if matches!(broker_name.as_str(), "trading212" | "t212") {
        args[2].clone()
    } else {
        args[2].to_uppercase()
    };
    // NaN fails the > 0.0 filter, so it lands in the same rejection as 0/-1/non-numeric
    let qty: f64 = args[3]
        .parse()
        .ok()
        .filter(|q: &f64| *q > 0.0)
        .ok_or_else(|| "QTY must be a positive number".to_string())?;
    Ok((broker_name, side, symbol, qty))
}

pub async fn run(args: Vec<String>) {
    let (broker_name, side, symbol, qty) = match parse_order(&args) {
        Ok(order) => order,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

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
        // parse_order already rejected anything else; keep a defensive arm rather than a panic
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

#[cfg(test)]
mod tests {
    use super::parse_order;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// The real-money command's parse path: every malformed invocation is rejected BEFORE the
    /// confirm prompt (incl. an unknown broker), and the happy path normalizes case.
    #[test]
    fn parse_order_rejects_and_normalizes() {
        // arity
        assert!(parse_order(&args(&["binance", "buy", "BTCEUR"])).is_err());
        assert!(parse_order(&args(&["binance", "buy", "BTCEUR", "1", "extra"])).is_err());
        // unknown broker rejected at parse time (used to slip past the confirm prompt)
        assert!(parse_order(&args(&["robinhood", "buy", "AAPL", "1"])).is_err());
        // side
        assert!(parse_order(&args(&["binance", "hold", "BTCEUR", "1"])).is_err());
        // qty: zero, negative, NaN and non-numeric all rejected the same way
        for bad in ["0", "-1", "NaN", "one"] {
            assert!(parse_order(&args(&["binance", "buy", "BTCEUR", bad])).is_err(), "qty {bad} must be rejected");
        }
        // happy path: broker+side lowercased, qty parsed; symbol case is broker POLICY —
        // T212 verbatim (venue letters are meaningfully lowercase), binance uppercased
        let (b, s, sym, q) = parse_order(&args(&["T212", "Buy", "VUAGl_EQ", "1.5"])).expect("valid order");
        assert_eq!((b.as_str(), s.as_str(), sym.as_str()), ("t212", "buy", "VUAGl_EQ"));
        assert!((q - 1.5).abs() < 1e-12);
        let (_, _, sym, _) = parse_order(&args(&["binance", "buy", "btceur", "0.1"])).expect("valid order");
        assert_eq!(sym, "BTCEUR");
    }
}
