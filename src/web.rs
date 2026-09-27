//! (#391) The page engine: re-rank the pool `screen` published under an uploaded settings.yaml.
//!
//! Pure, with no fetch and no filesystem, so web/engine compiles it for the browser and runs the same
//! `picks::render` the terminal does. The upload overlays CI's merged config exactly the way
//! `config::load` overlays a private settings.yaml onto tests/ci-settings.yaml.
//!
//! What an upload CANNOT move is everything `screen` decided at fetch time: the universe and its urls,
//! the dip/high and anchor windows, inflation, `stale_days`, names outside the pool, which ETFs carry a
//! look-through P/E, and the fund tilt's price-dependent values (`peg_yield`, `earnings_yield`).

use crate::config::{self, Settings};
use crate::core::{self, Quote};
use crate::picks::{self, FundPeMap, Owned, RenderCtx};
use std::cell::RefCell;
use std::collections::HashMap;

/// Everything `render` reads that `screen` fetched, as it stood right before `screen`'s own render.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Universe {
    /// When `screen` ranked this pool. The page's staleness banner reads it, so a re-rank carries it
    /// through rather than claiming the browser's clock.
    pub generated: String,
    /// CI's merged config as YAML text: the base an upload overlays.
    pub base: String,
    pub quotes: Vec<Quote>,
    /// ^GSPC, which the sector door reads its backstop CAGR off.
    pub spx: Option<Quote>,
    pub fund_pe: FundPeMap,
    pub nupl: Option<f64>,
    pub inflation: Vec<Vec<(String, String)>>,
    pub degraded: Vec<String>,
}

/// The `.screen_universe.json` body. `base` is the merged config `screen` ran on (None = no config,
/// and the page then refuses every upload rather than rank off code defaults).
pub fn snapshot(
    base: Option<serde_yaml::Value>,
    quotes: &[Quote],
    spx: Option<&Quote>,
    fund_pe: &FundPeMap,
    nupl: Option<f64>,
    inflation: &[Vec<(String, String)>],
    degraded: &[String],
) -> String {
    let u = Universe {
        generated: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        base: base.and_then(|b| serde_yaml::to_string(&b).ok()).unwrap_or_default(),
        quotes: quotes.to_vec(),
        spx: spx.cloned(),
        fund_pe: fund_pe.clone(),
        nupl,
        inflation: inflation.to_vec(),
        degraded: degraded.to_vec(),
    };
    serde_json::to_string(&u).unwrap_or_default()
}

/// Rank `universe` (a [`snapshot`]) under `overlay` (the uploaded settings.yaml text, "" = CI's own
/// config) and return the page payload `render` builds, the same shape as data.json.
pub fn screen(overlay: &str, universe: &str) -> Result<String, String> {
    let u: Universe = serde_json::from_str(universe).map_err(|e| format!("universe.json: {e}"))?;
    let mut merged: serde_yaml::Value = serde_yaml::from_str(&u.base).map_err(|e| format!("CI config: {e}"))?;
    let over: serde_yaml::Value = serde_yaml::from_str(overlay).map_err(|e| format!("settings.yaml: {e}"))?;
    config::merge_yaml(&mut merged, over);
    if !config::gates_configured(&merged) {
        return Err("settings.yaml: no buy_heuristic knobs left after the merge".to_string());
    }
    let s: Settings = serde_yaml::from_value(merged.clone()).map_err(|e| format!("settings.yaml: {e}"))?;
    #[cfg(target_family = "wasm")]
    config::install(merged)?;
    let bh = &s.buy_heuristic;
    let mut quotes = u.quotes;
    // The two settings-driven stamps `screen` applies between its fetch and its render, replayed in its
    // order. 1: the fund tilt, which `screen` fetches only when its weight is on.
    for q in &mut quotes {
        if bh.growth_fund_weight > 0.0 {
            if let Some(f) = &q.fund {
                q.fund_factor = core::select_fund_factor(f, &bh.growth_fund_factor);
            }
        } else {
            q.fund = None;
            q.fund_factor = None;
        }
    }
    // 2: the (#328) sector door, floored at the index's CAGR under THESE knobs.
    let floors = match u.spx.as_ref().and_then(|b| picks::long_cagr_pct(b, bh)) {
        Some(bc) => picks::sector_floors(&quotes.iter().collect::<Vec<_>>(), bh, picks::SECTOR_DOOR_K, bc),
        None => Default::default(),
    };
    picks::stamp_sector_floors(&mut quotes, &floors);
    let out = RefCell::new(None);
    let sink = |json: String| *out.borrow_mut() = Some(json);
    picks::render(&quotes, s.top_picks, bh, &s.widths, RenderCtx {
        nupl: u.nupl,
        sectors: &s.sectors,
        sector_of: &HashMap::new(), // the terminal lane's only reader; the payload never sees it
        pinned: &s.tickers,
        owned: &Owned::default(), // the page knows no holdings
        explain: None,
        show_hold_core: true,
        fund_pe: &u.fund_pe,
        web_out: Some(&sink),
        web_inflation: &u.inflation,
        web_degraded: &u.degraded,
    });
    let mut top: serde_json::Value =
        out.into_inner().and_then(|j| serde_json::from_str(&j).ok()).ok_or("render built no payload")?;
    top["generated"] = u.generated.into();
    Ok(top.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI: &str = include_str!("../tests/ci-settings.yaml");

    /// A pool that fills more than one lane: a rich pinned stock and a CORE fund.
    fn pool() -> Vec<Quote> {
        let mut pin = Quote::stub("AAPL", "€200.00", "", "Apple Inc.");
        pin.instrument_type = "EQUITY".into();
        pin.drawdown_pct = 8.5;
        pin.avg_turnover_eur = Some(3.4e9);
        pin.volatility_pct = Some(1.3);
        pin.max_drawdown_pct = 34.0;
        pin.trend_r2 = 0.95;
        pin.life_cagr = Some(23.0);
        pin.age_years = Some(11.0);
        pin.pe_ratio = Some(31.5);
        pin.perf = vec![Some(("1Y".to_string(), 19.0))];
        let mut core = Quote::stub("VWCE.DE", "€100.00", "", "Vanguard FTSE All-World UCITS ETF");
        core.instrument_type = "ETF".into();
        core.expense_ratio = Some(0.22);
        core.replication = Some("Opt");
        core.use_of_profits = Some("Acc");
        core.aum_eur = Some(20e9);
        core.domicile = Some("IE".to_string());
        core.life_cagr = Some(9.0);
        core.age_years = Some(12.0);
        vec![pin, core, Quote::stub("MSFT", "err", "", "Microsoft")]
    }

    fn universe(quotes: &[Quote]) -> String {
        let infl = vec![vec![("REGION".to_string(), "EU".to_string())]];
        let degraded = ["MVRV feed down".to_string()];
        snapshot(serde_yaml::from_str(CI).ok(), quotes, None, &FundPeMap::new(), Some(0.3), &infl, &degraded)
    }

    /// What `screen` itself would have published for this pool, pinned set and cut.
    fn direct(quotes: &[Quote], pinned: &[String], n: usize) -> serde_json::Value {
        let s: Settings = serde_yaml::from_str(CI).expect("the CI config parses");
        let out = RefCell::new(String::new());
        let sink = |json: String| *out.borrow_mut() = json;
        picks::render(quotes, n, &s.buy_heuristic, &s.widths, RenderCtx {
            nupl: Some(0.3),
            sectors: &s.sectors,
            sector_of: &HashMap::new(),
            pinned,
            owned: &Owned::default(),
            explain: None,
            show_hold_core: true,
            fund_pe: &FundPeMap::new(),
            web_out: Some(&sink),
            web_inflation: &[vec![("REGION".to_string(), "EU".to_string())]],
            web_degraded: &["MVRV feed down".to_string()],
        });
        let mut v: serde_json::Value = serde_json::from_str(&out.into_inner()).expect("render built the payload");
        v["generated"] = serde_json::Value::Null;
        v
    }

    fn engine(overlay: &str, quotes: &[Quote]) -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(&screen(overlay, &universe(quotes)).expect("ranks")).expect("JSON");
        assert!(v["generated"].as_str().is_some_and(|g| g.ends_with('Z')), "the snapshot's own stamp rides through");
        v["generated"] = serde_json::Value::Null;
        v
    }

    /// THE PARITY PIN: the browser's re-rank of a published pool is byte-for-byte the table `screen`
    /// printed from it, both with no upload and with one that moves the pinned set and the cut. Every
    /// lane, the CORE table, inflation and the DEGRADED list come through the snapshot intact.
    #[test]
    fn the_engine_reranks_the_pool_exactly_as_screen_does() {
        let q = pool();
        let aapl = ["AAPL".to_string()];
        let want = direct(&q, &aapl, 3);
        assert!(!want["stocks"].as_array().expect("stocks lane").is_empty(), "not vacuous: the pinned row is there");
        assert_eq!(engine("tickers: [AAPL]\ntop_picks: 3\n", &q), want);
        assert_eq!(engine("", &q), direct(&q, &[], 25), "an empty upload IS CI's config");
        // a bare `buy_heuristic:` names no knob, so it moves nothing (the null-safe merge arm)
        assert_eq!(engine("buy_heuristic:\n", &q), engine("", &q));
    }

    /// The fund tilt follows the UPLOAD's weight: re-selected from `fund` while it is on, and `fund`
    /// dropped when it is 0, which is what `screen` does by never fetching it. CI's `roic` extra reads
    /// `fund` straight, so a kept one would still move the score at weight 0.
    #[test]
    fn the_fund_tilt_follows_the_uploaded_weight() {
        let mut q = pool();
        q[0].fund = Some(core::FundFactors { peg_yield: Some(50.0), roic: Some(40.0), ..Default::default() });
        let mut picked = q.clone();
        picked[0].fund_factor = Some(50.0);
        let mut bare = q.clone();
        bare[0].fund = None;
        let aapl = ["AAPL".to_string()];
        let want = direct(&picked, &aapl, 25);
        assert_ne!(want, direct(&bare, &aapl, 25), "not vacuous: the tilt moves the pinned row");
        assert_eq!(engine("tickers: [AAPL]\n", &q), want, "weight on: the factor is re-selected from `fund`");
        let off = "tickers: [AAPL]\nbuy_heuristic:\n  growth_fund_weight: 0\n";
        assert_ne!(engine(off, &picked), engine("tickers: [AAPL]\n", &picked), "not vacuous: 0 moves the row");
        assert_eq!(engine(off, &q), engine(off, &bare), "weight off: `fund` is dropped with its factor");
    }

    /// An upload that is not a usable config is an error the page prints, never a quiet default rank.
    #[test]
    fn a_bad_upload_is_refused_with_the_reason() {
        let u = universe(&pool());
        assert!(screen("tickers: [", &u).is_err_and(|e| e.starts_with("settings.yaml:")));
        assert!(screen("not_a_knob: 1\n", &u).is_err_and(|e| e.contains("not_a_knob")));
        assert!(screen("buy_heuristic: 3\n", &u).is_err_and(|e| e.contains("no buy_heuristic")));
        assert!(screen("", "{}").is_err_and(|e| e.starts_with("universe.json:")));
    }

    /// The two interned `&'static str` fields survive the snapshot (a None here blanks USE/REPL).
    #[test]
    fn interned_fields_round_trip() {
        let q = &pool()[1];
        let json = serde_json::to_string(q).expect("ser");
        let back: Quote = serde_json::from_str(&json).expect("de");
        assert_eq!((back.use_of_profits, back.replication), (Some("Acc"), Some("Opt")));
    }
}
