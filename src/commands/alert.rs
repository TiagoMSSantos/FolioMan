//! `alert [TICKERS]` — ntfy.sh push for tickers >= drop_pct below their trailing high, plus a
//! market-level entry-state flip ping when the S&P 500 crosses a near-high/pullback/drawdown line.

use crate::commands::screen::entry_state_class;
use crate::{config, fetch};

/// (round 113) Last pushed market entry state, one word in the working dir — same local-file
/// pattern as `.screen_state.json`, plain text because one word needs no serde. Gitignored.
const ALERT_STATE_FILE: &str = ".alert_state";

/// (round 113) Whether an entry-state reading warrants a push: any change from the stored state
/// (worsening = deploy faster, recovery = resume normal schedule), except the very first run when
/// the market is NEAR-HIGH — nothing actionable to say, so the baseline is set silently.
fn entry_flip_due(prev: Option<&str>, state: &str) -> bool {
    match prev {
        Some(p) => p != state,
        None => state != "NEAR-HIGH",
    }
}

/// Tickers whose dip alert already went out, one per line — same plain-text working-dir pattern as
/// `.alert_state`. A name in this set stays silent while it remains in dip territory; recovery
/// drops it so the NEXT dip pings again. Without this, a daily cron re-pinged every name for every
/// day it sat below the line — fatigue that buries the one alert that matters.
const ALERT_DIPS_FILE: &str = ".alert_dips";

/// Per-ticker dip dedup: push only when a name ENTERS dip territory (mirrors the entry-state flip
/// discipline: one ping per line crossed, not one per run).
fn dip_ping_due(already_alerted: bool, in_dip: bool) -> bool {
    in_dip && !already_alerted
}

/// Apply one ticker's reading to the alerted set. Insert only on a DELIVERED push (a dropped push
/// retries next run, like the flip ping); recovery removes unconditionally (no push to deliver).
/// Tickers not read this run are never touched — an args-scoped `alert TSLA` must not wipe the
/// cron's state for everything else.
fn apply_dip_state(set: &mut std::collections::BTreeSet<String>, ticker: &str, in_dip: bool, delivered: bool) {
    if in_dip {
        if delivered {
            set.insert(ticker.to_string());
        }
    } else {
        set.remove(ticker);
    }
}

/// Persist the last pushed state; a failed write only risks a repeated ping next run — warn, don't die.
fn persist_entry_state(state: &str) {
    if std::fs::write(ALERT_STATE_FILE, state).is_err() {
        eprintln!("WARNING: could not persist {ALERT_STATE_FILE} — the entry-state ping may repeat next run");
    }
}

pub async fn run(args: Vec<String>) {
    let settings = config::load();
    let client = fetch::client();
    let fx_cache = fetch::fx_cache();
    let tickers = if args.is_empty() { settings.tickers.clone() } else { args };

    let mut alerted: std::collections::BTreeSet<String> = std::fs::read_to_string(ALERT_DIPS_FILE)
        .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    let before = alerted.clone();
    for quote in fetch::quotes(&client, &settings.urls, &fx_cache, &tickers, settings.dip_days, settings.high_days, false, true, &settings.anchor_windows, None).await { // news on: alert body shows headlines; keys on price drop, not returns
        if quote.price == "err" || quote.price == "no data" {
            continue; // a stub quote reads drop_pct 0.0 — treating that as a recovery would fake-clear the dedup
        }
        let in_dip = quote.drop_pct >= settings.drop_pct;
        let mut delivered = false;
        if dip_ping_due(alerted.contains(&quote.ticker), in_dip) {
            delivered = fetch::push(
                &client,
                &settings.urls,
                &settings.ntfy_topic,
                &format!("{} {} (buy-dip?)", quote.ticker, quote.dip),
                &format!(
                    "{} is {:.1}% below its {}d high.\n{}",
                    quote.ticker, quote.drop_pct, settings.dip_days, quote.news_block
                ),
            )
            .await;
            // cron pipes stderr to the log (see README) — a dropped push must leave a trace there.
            // Keep going: one failed push must not cost the remaining tickers their alerts.
            if !delivered {
                eprintln!("WARNING: ntfy push failed for {} — dip alert NOT delivered", quote.ticker);
            }
        }
        apply_dip_state(&mut alerted, &quote.ticker, in_dip, delivered);
    }
    if alerted != before {
        let text = alerted.iter().map(|t| format!("{t}\n")).collect::<String>();
        if std::fs::write(ALERT_DIPS_FILE, text).is_err() {
            eprintln!("WARNING: could not persist {ALERT_DIPS_FILE} — delivered dip alerts may repeat next run");
        }
    }

    // (round 113) market-level entry-state flip ping: the deploy-faster-in-drawdowns edge
    // (+9.1 pts/yr over SPY vs +5.9 near the high) is only actionable if it reaches the phone the
    // day the market crosses a line — the screen banner needs a screen run, and a drawdown is
    // exactly when nobody runs one. Push on EVERY state change, deduped via the stored last state.
    // A failed/stub ^GSPC fetch skips silently (state untouched — a 0.0 stub would fake a recovery).
    let spx = fetch::quotes(
        &client, &settings.urls, &fx_cache, &["^GSPC".to_string()], settings.dip_days, settings.high_days,
        false, false, &settings.anchor_windows, None,
    )
    .await;
    if let Some(q) = spx.first().filter(|q| q.price != "err" && q.price != "no data") {
        let off_hi = q.drawdown_pct;
        let (state, read) = entry_state_class(off_hi);
        let prev = std::fs::read_to_string(ALERT_STATE_FILE).ok();
        if entry_flip_due(prev.as_deref().map(str::trim), state) {
            let delivered = fetch::push(
                &client,
                &settings.urls,
                &settings.ntfy_topic,
                &format!("Market {state}: S&P 500 {off_hi:.1}% off high"),
                &format!("{read}. NOT advice."),
            )
            .await;
            if delivered {
                // persist ONLY on delivery — a dropped push keeps the old state so the next cron
                // run retries the flip instead of losing it.
                persist_entry_state(state);
            } else {
                eprintln!("WARNING: ntfy push failed for market entry state — flip alert NOT delivered");
            }
        } else if prev.is_none() {
            // first run with the market near-high: set the baseline silently so a later flip fires.
            persist_entry_state(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_dip_state, dip_ping_due, entry_flip_due};

    /// (round 113) flip semantics: first run is silent at near-high but pushes when actionable;
    /// any later change pushes (worsening AND recovery); an unchanged state stays silent.
    #[test]
    fn entry_flip_semantics() {
        assert!(!entry_flip_due(None, "NEAR-HIGH"));
        assert!(entry_flip_due(None, "PULLBACK"));
        assert!(entry_flip_due(None, "DRAWDOWN"));
        assert!(entry_flip_due(Some("NEAR-HIGH"), "PULLBACK"));
        assert!(entry_flip_due(Some("PULLBACK"), "DRAWDOWN"));
        assert!(entry_flip_due(Some("DRAWDOWN"), "NEAR-HIGH"));
        assert!(!entry_flip_due(Some("PULLBACK"), "PULLBACK"));
        assert!(!entry_flip_due(Some("NEAR-HIGH"), "NEAR-HIGH"));
    }

    /// Dip-dedup lifecycle: entering dip pings once; staying in dip is silent; a dropped push is
    /// NOT recorded (retries next run); recovery clears the ticker so the next dip pings again;
    /// tickers not read this run keep their state (args-scoped runs must not wipe cron state).
    #[test]
    fn dip_dedup_semantics() {
        let mut set = std::collections::BTreeSet::from(["HELD".to_string()]);
        // enter dip -> ping due; delivered -> recorded
        assert!(dip_ping_due(set.contains("AAPL"), true));
        apply_dip_state(&mut set, "AAPL", true, true);
        assert!(set.contains("AAPL"));
        // still in dip -> silent, stays recorded
        assert!(!dip_ping_due(set.contains("AAPL"), true));
        apply_dip_state(&mut set, "AAPL", true, false);
        assert!(set.contains("AAPL"));
        // dropped push -> not recorded -> next run retries
        assert!(dip_ping_due(set.contains("TSLA"), true));
        apply_dip_state(&mut set, "TSLA", true, false);
        assert!(dip_ping_due(set.contains("TSLA"), true));
        // recovery clears -> re-dip pings again
        apply_dip_state(&mut set, "AAPL", false, false);
        assert!(!set.contains("AAPL"));
        assert!(dip_ping_due(set.contains("AAPL"), true));
        // not in dip + never alerted -> nothing due, nothing recorded
        assert!(!dip_ping_due(set.contains("MSFT"), false));
        apply_dip_state(&mut set, "MSFT", false, false);
        assert!(!set.contains("MSFT"));
        // unread ticker untouched across the whole dance
        assert!(set.contains("HELD"));
    }
}
