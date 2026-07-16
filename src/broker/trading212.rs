//! Trading212 equity trading (official API). Auth: `TRADING212_API_KEY`.

use super::env_var;
use reqwest::Client;
use serde_json::{json, Value};

async fn get(client: &Client, key: &str, path: &str) -> Result<Value, String> {
    let resp = client
        .get(format!("https://live.trading212.com/api/v0/{path}"))
        .header("Authorization", key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(response body unreadable: {e})"));
    if !status.is_success() {
        return Err(format!("trading212 {status}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Cash available to invest + current holdings (read-only).
pub async fn summary(client: &Client) -> Result<String, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let cash = get(client, &key, "equity/account/cash").await?;
    let port = get(client, &key, "equity/portfolio").await?;
    render_summary(&cash, &port)
}

/// (round 110) Owned tickers in Trading212 form (e.g. `AAPL_US_EQ`) for the screen's held-position
/// overlay. Same endpoint `summary` reads; rows without a ticker string are skipped (a display
/// overlay must never fail the screen over one malformed row).
pub async fn owned_tickers(client: &Client) -> Result<Vec<String>, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let port = get(client, &key, "equity/portfolio").await?;
    extract_tickers(&port)
}

/// (round 114) Held positions (ticker, quantity) for the size command's allocation-gap section.
/// Same endpoint `summary` reads; rows missing either field are skipped (the gap table must never
/// invent a position), a non-array response is API drift.
pub async fn owned_positions(client: &Client) -> Result<Vec<(String, f64)>, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let port = get(client, &key, "equity/portfolio").await?;
    extract_positions(&port)
}

/// Pure extraction, offline-testable (like `render_summary`).
fn extract_positions(port: &Value) -> Result<Vec<(String, f64)>, String> {
    Ok(port
        .as_array()
        .ok_or_else(|| "trading212: portfolio response is not an array (API drift?)".to_string())?
        .iter()
        .filter_map(|h| Some((h.get("ticker")?.as_str()?.to_string(), h.get("quantity")?.as_f64()?)))
        .collect())
}

/// Pure extraction, split from the fetch so drift handling is testable offline (like `render_summary`).
fn extract_tickers(port: &Value) -> Result<Vec<String>, String> {
    Ok(port
        .as_array()
        .ok_or_else(|| "trading212: portfolio response is not an array (API drift?)".to_string())?
        .iter()
        .filter_map(|h| h.get("ticker").and_then(|v| v.as_str()).map(str::to_string))
        .collect())
}

/// (round 117) One tradable instrument from the metadata endpoint — the minimum the order-glue
/// symbol resolver needs: exact T212 ticker form + ISIN + listing currency.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Instrument {
    pub ticker: String,
    pub isin: String,
    pub currency: String,
}

/// Pure extraction, offline-testable (like the others): rows missing any field are skipped, a
/// non-array response is API drift.
fn extract_instruments(v: &Value) -> Result<Vec<Instrument>, String> {
    Ok(v
        .as_array()
        .ok_or_else(|| "trading212: instruments response is not an array (API drift?)".to_string())?
        .iter()
        .filter_map(|i| {
            Some(Instrument {
                ticker: i.get("ticker")?.as_str()?.to_string(),
                isin: i.get("isin")?.as_str()?.to_string(),
                currency: i.get("currencyCode")?.as_str()?.to_string(),
            })
        })
        .collect())
}

/// (round 117) Full tradable-instrument list for order-glue symbol resolution, cached in
/// `.t212_instruments.json` for 7 days — the endpoint returns ~10k rows and is rate-limited
/// (~1 req/50s), and listings don't churn daily. ANY failure (no key, throttle, API drift) →
/// empty vec: the glue degrades to `<T212_SYMBOL>` placeholders exactly like the owned overlay,
/// never fails the screen.
pub async fn instruments_cached(client: &Client) -> Vec<Instrument> {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Cache {
        date: String,
        rows: Vec<Instrument>,
    }
    const PATH: &str = ".t212_instruments.json";
    let today = chrono::Utc::now().date_naive();
    if let Some(c) = std::fs::read_to_string(PATH).ok().and_then(|s| serde_json::from_str::<Cache>(&s).ok()) {
        if chrono::NaiveDate::parse_from_str(&c.date, "%Y-%m-%d").is_ok_and(|d| (today - d).num_days() < 7) {
            return c.rows;
        }
    }
    let Ok(key) = env_var("TRADING212_API_KEY") else {
        return Vec::new(); // no key configured = broker off, silent like the owned overlay
    };
    let rows = match get(client, &key, "equity/metadata/instruments").await.and_then(|v| extract_instruments(&v)) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("screen: trading212 instruments fetch failed ({e}) — order symbols degrade to placeholders");
            return Vec::new();
        }
    };
    if let Ok(json) = serde_json::to_string(&Cache { date: today.to_string(), rows: rows.clone() }) {
        let _ = std::fs::write(PATH, json);
    }
    rows
}

/// Pure response→text rendering, split from the fetch so drift handling is testable offline.
/// A missing cash field or a non-array portfolio is API drift and must surface as the broker
/// error (accounts prints it as "(skipped) …"), never render as plausible zeros/emptiness.
fn render_summary(cash: &Value, port: &Value) -> Result<String, String> {
    let cash_num = |k: &str| {
        cash.get(k)
            .and_then(|x| x.as_f64())
            .ok_or_else(|| format!("trading212: no `{k}` in the cash response (API drift?)"))
    };
    let mut out = format!(
        "  cash: free {:.2}  invested {:.2}  total {:.2}",
        cash_num("free")?,
        cash_num("invested")?,
        cash_num("total")?,
    );
    let p = port
        .as_array()
        .ok_or_else(|| "trading212: portfolio response is not an array (API drift?)".to_string())?;
    if p.is_empty() {
        out.push_str("\n  holdings: (none)");
    } else {
        out.push_str("\n  holdings:");
        for h in p {
            let ticker = h.get("ticker").and_then(|v| v.as_str()).unwrap_or("?");
            let num = |k: &str| h.get(k).and_then(|v| v.as_f64());
            // an unparsable amount must not render as 0.00 — that reads as "nothing here"
            let (Some(qty), Some(price), Some(ppl)) = (num("quantity"), num("currentPrice"), num("ppl")) else {
                eprintln!("WARNING: trading212 position row for {ticker} has an unparsable amount — row skipped");
                continue;
            };
            out.push_str(&format!("\n    {ticker:<12} qty {qty:<10} @ {price:.2}  P/L {ppl:+.2}"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// (round 117) instruments extraction: complete rows collected, partial rows skipped (a
    /// resolver must never see a half-instrument), non-array = drift error.
    #[test]
    fn extract_instruments_collects_and_skips() {
        let v = json!([
            { "ticker": "AAPL_US_EQ", "isin": "US0378331005", "currencyCode": "USD" },
            { "ticker": "NOISIN_EQ", "currencyCode": "USD" },
            { "isin": "IE00LONELY00", "currencyCode": "EUR" }
        ]);
        let rows = extract_instruments(&v).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].ticker.as_str(), rows[0].isin.as_str(), rows[0].currency.as_str()), ("AAPL_US_EQ", "US0378331005", "USD"));
        assert!(extract_instruments(&json!({ "not": "array" })).is_err());
    }

    #[test]
    fn renders_cash_and_holdings() {
        let cash = json!({ "free": 100.5, "invested": 200.0, "total": 300.5 });
        let port = json!([{ "ticker": "AAPL_US_EQ", "quantity": 2.0, "currentPrice": 150.0, "ppl": 12.5 }]);
        let out = render_summary(&cash, &port).unwrap();
        assert!(out.contains("free 100.50"), "{out}");
        assert!(out.contains("AAPL_US_EQ"), "{out}");
    }

    /// A renamed/dropped cash field is API drift — it must error naming the field, not print 0.00.
    #[test]
    fn missing_cash_field_is_drift_error_not_zero() {
        let cash = json!({ "free": 100.0, "invested": 200.0 }); // no "total"
        let err = render_summary(&cash, &json!([])).unwrap_err();
        assert!(err.contains("`total`"), "error must name the missing field: {err}");
    }

    /// A non-array portfolio (wrapper object, error payload) must not read as an empty account.
    #[test]
    fn non_array_portfolio_is_drift_error_not_empty() {
        let cash = json!({ "free": 0.0, "invested": 0.0, "total": 0.0 });
        let err = render_summary(&cash, &json!({ "positions": [] })).unwrap_err();
        assert!(err.contains("not an array"), "{err}");
    }

    /// (round 114) position extraction: (ticker, qty) pairs collected, rows missing either field
    /// skipped, non-array = drift error.
    #[test]
    fn extract_positions_collects_and_skips() {
        let port = json!([
            { "ticker": "AAPL_US_EQ", "quantity": 2.5 },
            { "quantity": 1.0 },
            { "ticker": "NOQTY_US_EQ" }
        ]);
        assert_eq!(extract_positions(&port).unwrap(), vec![("AAPL_US_EQ".to_string(), 2.5)]);
        assert!(extract_positions(&json!({ "positions": [] })).unwrap_err().contains("not an array"));
    }

    /// (round 110) ticker extraction: strings collected, malformed rows skipped, non-array = drift error.
    #[test]
    fn extract_tickers_collects_and_skips() {
        let port = json!([
            { "ticker": "AAPL_US_EQ", "quantity": 2.0 },
            { "quantity": 1.0 },
            { "ticker": "IITU_GB_EQ" }
        ]);
        assert_eq!(extract_tickers(&port).unwrap(), vec!["AAPL_US_EQ", "IITU_GB_EQ"]);
        assert!(extract_tickers(&json!({ "positions": [] })).unwrap_err().contains("not an array"));
    }

    #[test]
    fn empty_portfolio_prints_none() {
        let cash = json!({ "free": 0.0, "invested": 0.0, "total": 0.0 });
        let out = render_summary(&cash, &json!([])).unwrap();
        assert!(out.contains("holdings: (none)"), "{out}");
    }

    /// A row whose amounts fail to parse is skipped (with a stderr warning), never shown as 0.00.
    #[test]
    fn unparsable_position_row_is_skipped_not_zero() {
        let cash = json!({ "free": 0.0, "invested": 0.0, "total": 0.0 });
        let port = json!([
            { "ticker": "BAD_ROW", "quantity": "2", "currentPrice": 150.0, "ppl": 0.0 },
            { "ticker": "GOOD", "quantity": 1.0, "currentPrice": 10.0, "ppl": 0.5 }
        ]);
        let out = render_summary(&cash, &port).unwrap();
        assert!(!out.contains("BAD_ROW"), "{out}");
        assert!(out.contains("GOOD"), "{out}");
    }
}

/// Trading212 market order via the **live** (production = real money) API. `ticker` must be
/// in Trading212 form, e.g. `AAPL_US_EQ`. buy = +qty, sell = -qty (T212 encodes direction
/// by sign). Key is the raw `Authorization` header value.
pub async fn order(client: &Client, side: &str, ticker: &str, qty: f64) -> Result<String, String> {
    let key = env_var("TRADING212_API_KEY")?;
    let signed = if side == "sell" { -qty } else { qty };
    let resp = client
        .post("https://live.trading212.com/api/v0/equity/orders/market")
        .header("Authorization", key)
        .json(&json!({ "quantity": signed, "ticker": ticker }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    // the order may already be live at this point — a body-read failure must say so, not print
    // an empty confirmation.
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(response body unreadable: {e} — check the order in the Trading212 app)"));
    if status.is_success() {
        Ok(format!("trading212 accepted: {body}"))
    } else {
        Err(format!("trading212 {status}: {body}"))
    }
}
