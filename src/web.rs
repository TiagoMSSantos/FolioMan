//! (#391) The page engine: re-rank the pool `screen` published under an uploaded settings.yaml.
//!
//! Pure, with no fetch and no filesystem, so web/engine compiles it for the browser and runs the same
//! `picks::render` the terminal does. The upload overlays CI's merged config exactly the way
//! `config::load` overlays a private settings.yaml onto tests/ci-settings.yaml.
//!
//! What an upload CANNOT move is everything `screen` decided at fetch time: the universe and its urls,
//! the dip/high and anchor windows, inflation, `stale_days`, names outside the pool (bar another
//! venue's line of a pool fund, which (#397) shows as the pool's line), which ETFs carry a
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
    /// (#397) Another venue's listing -> the pool's line of the same fund (`VUAA.DE` -> `VUAA.L`), so a
    /// pin the pool does not carry still shows. Empty when `screen` ran without an OpenFIGI key.
    #[serde(default)]
    pub aliases: HashMap<String, String>,
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
    aliases: &HashMap<String, String>,
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
        aliases: aliases.clone(),
    };
    serde_json::to_string(&u).unwrap_or_default()
}

/// Rank `universe` (a [`snapshot`]) under `overlay` (the uploaded settings.yaml text, "" = CI's own
/// config) and return the page payload `render` builds, the same shape as data.json.
pub fn screen(overlay: &str, universe: &str) -> Result<String, String> {
    let (s, u) = load(overlay, universe)?;
    let bh = &s.buy_heuristic;
    // (#397) The uploaded pins as rows the pool can show: a pin the pool holds stays, one it does not
    // becomes the pool's line of the same fund (`twins`), and anything else is `missing`. Inline, not a
    // helper: a three-Vec return is ~54 mutants for the gate to run.
    let (mut pins, mut twins, mut missing) = (Vec::new(), Vec::new(), Vec::new());
    for t in &s.tickers {
        if u.quotes.iter().any(|q| &q.ticker == t) {
            pins.push(t.clone());
        } else if let Some(twin) = u.aliases.get(t) {
            pins.push(twin.clone());
            twins.push((t.clone(), twin.clone()));
        } else {
            missing.push(t.clone());
        }
    }
    let out = RefCell::new(None);
    let sink = |json: String| *out.borrow_mut() = Some(json);
    let (_, ranked) = picks::render(&u.quotes, s.top_picks, bh, &s.widths, RenderCtx {
        nupl: u.nupl,
        sectors: &s.sectors,
        sector_of: &HashMap::new(), // the terminal lane's only reader; the payload never sees it
        pinned: &pins,
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
    // (#403) The equal-weight book never reads the fund look-through, so an empty map replays `screen`'s
    // BUY NOW exactly. The vol-target book caps fund sectors by holdings the page does not carry, so an
    // upload that turns it on gets no BUY% column rather than a wrong one.
    if s.sizing.equal_weight_book {
        picks::stamp_buy(&mut top, &picks::buy_book(&ranked, &u.quotes, bh, &s.sizing, u.nupl, &HashMap::new()));
    }
    top["generated"] = u.generated.into();
    // only when there is something to say, so a pool that holds every pin stays byte-equal to `screen`
    if !(twins.is_empty() && missing.is_empty()) {
        top["pins"] = serde_json::json!({ "twins": twins, "missing": missing });
    }
    Ok(top.to_string())
}

/// (#403) The page's "why isn't X in?": `screen --explain` on the re-ranked pool. `query` is a ticker,
/// another venue's line of a pool fund, or part of a name, because the pool keeps ONE listing per
/// company and it is often a European one (NVIDIA is NVD.DE), so a bare US ticker would read "not scanned".
pub fn explain(overlay: &str, universe: &str, query: &str) -> Result<String, String> {
    let (s, u) = load(overlay, universe)?;
    let q = query.trim();
    let low = q.to_lowercase();
    // a name that STARTS with the query first: "nvidia" is NVIDIA Corporation, not a 3x short ETP on it
    let mut named: Vec<&Quote> = u.quotes.iter().filter(|x| !low.is_empty() && x.name.to_lowercase().contains(&low)).collect();
    named.sort_by_key(|x| !x.name.to_lowercase().starts_with(&low));
    let hit = u
        .quotes
        .iter()
        .find(|x| x.ticker.eq_ignore_ascii_case(q))
        .map(|x| x.ticker.clone())
        .or_else(|| u.aliases.iter().find(|(k, _)| k.eq_ignore_ascii_case(q)).map(|(_, v)| v.clone()));
    let by_name = hit.is_none();
    let Some(t) = hit.or_else(|| named.first().map(|x| x.ticker.clone())) else {
        return Ok(format!(
            "No pool row matches \"{q}\". The pool keeps one listing per company, often a European one (NVIDIA is NVD.DE): try the company name."
        ));
    };
    let (text, _) = picks::render(&u.quotes, s.top_picks, &s.buy_heuristic, &s.widths, RenderCtx {
        nupl: u.nupl,
        sectors: &s.sectors,
        sector_of: &HashMap::new(),
        pinned: &[],
        owned: &Owned::default(),
        explain: Some(&t),
        show_hold_core: false,
        fund_pe: &u.fund_pe,
        web_out: None,
        web_inflation: &[],
        web_degraded: &[],
    });
    let mut out = text.unwrap_or_default().trim().trim_start_matches("--explain: ").to_string();
    if by_name {
        out = format!("Matched by name: {} ({t})\n\n{out}", named[0].name);
    }
    let also: Vec<&str> = named.iter().map(|x| x.ticker.as_str()).filter(|x| *x != t).take(5).collect();
    if !also.is_empty() {
        out.push_str(&format!("\n\nAlso matching \"{q}\": {}", also.join(", ")));
    }
    Ok(out)
}

/// The upload merged onto CI's config, and the pool with the two settings-driven stamps `screen` applies
/// between its fetch and its render.
fn load(overlay: &str, universe: &str) -> Result<(Settings, Universe), String> {
    let mut u: Universe = serde_json::from_str(universe).map_err(|e| format!("universe.json: {e}"))?;
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
    // Replayed in `screen`'s order. 1: the fund tilt, which `screen` fetches only when its weight is on.
    for q in &mut u.quotes {
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
        Some(bc) => picks::sector_floors(&u.quotes.iter().collect::<Vec<_>>(), bh, picks::SECTOR_DOOR_K, bc),
        None => Default::default(),
    };
    picks::stamp_sector_floors(&mut u.quotes, &floors);
    Ok((s, u))
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
        vec![pin, core, Quote::stub("MSFT", "err", "", "Microsoft"), winner()]
    }

    fn winner() -> Quote {
        let mut q = Quote::stub("NVD.DE", "€100.00", "", "NVIDIA Corp");
        q.instrument_type = "EQUITY".into();
        q.avg_turnover_eur = Some(1e9);
        q.range_pct = 90.0;
        let legs = [("1M", 2.0), ("1Y", 20.0), ("5Y", 200.0), ("8Y", 400.0), ("20Y", 4000.0)];
        q.perf = core::HORIZONS.iter().map(|(h, _)| legs.iter().find(|(l, _)| l == h).map(|&(_, v)| ("x".to_string(), v))).collect();
        q.age_years = Some(30.0);
        q.volatility_pct = Some(1.0);
        q.downside_dev_pct = Some(0.7);
        q.max_daily_1m = Some(3.0);
        q.mom_pct = Some(2.0);
        q.pe_ratio = Some(20.0);
        q.roe = Some(18.0);
        q.trend_cagr = Some(15.0);
        q.life_cagr = Some(22.0);
        q.capped_cagr = Some(22.0);
        q.life_return_pct = Some(900.0);
        q.tr_cagr = Some(19.0);
        q.roll5y_pos_pct = Some(100.0);
        q.roll10y_pos_pct = Some(100.0);
        q.worst_5y_pct = Some(5.0);
        q.worst_10y_pct = Some(25.0);
        q.underwater_yrs = Some(1.0);
        q.price_eur = Some(100.0);
        q.trend_r2 = 0.95;
        q.max_drawdown_pct = 20.0;
        q.stats_8y = Some(core::Stats8 { range_pct: 92.0, trend_r2: 0.95, max_drawdown_pct: 20.0, underwater_yrs: Some(1.0) });
        q
    }

    fn universe(quotes: &[Quote]) -> String {
        let infl = vec![vec![("REGION".to_string(), "EU".to_string())]];
        let degraded = ["MVRV feed down".to_string()];
        let aliases = HashMap::from([("VWCE.L".to_string(), "VWCE.DE".to_string())]);
        snapshot(serde_yaml::from_str(CI).ok(), quotes, None, &FundPeMap::new(), Some(0.3), &infl, &degraded, &aliases)
    }

    /// What `screen` itself would have published for this pool, pinned set and cut.
    fn direct(quotes: &[Quote], pinned: &[String], n: usize) -> serde_json::Value {
        let s: Settings = serde_yaml::from_str(CI).expect("the CI config parses");
        let out = RefCell::new(String::new());
        let sink = |json: String| *out.borrow_mut() = json;
        let (_, ranked) = picks::render(quotes, n, &s.buy_heuristic, &s.widths, RenderCtx {
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
        // (#403) the BUY% `screen` stamps from its own sized book, with its real look-through map
        picks::stamp_buy(&mut v, &picks::buy_book(&ranked, quotes, &s.buy_heuristic, &s.sizing, Some(0.3), &HashMap::new()));
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

    /// (#397) A pin the pool lacks shows as the pool's line of the same fund: the engine ranks exactly
    /// what `screen` would with the twin pinned, and names which pin became which row and which pin it
    /// could not place at all.
    #[test]
    fn a_pin_on_another_venue_shows_as_its_pool_twin() {
        let q = pool();
        let pinned = ["AAPL".to_string(), "VWCE.DE".to_string()];
        let mut got = engine("tickers: [AAPL, VWCE.L, NOPE]\n", &q);
        assert_eq!(got["pins"], serde_json::json!({"twins": [["VWCE.L", "VWCE.DE"]], "missing": ["NOPE"]}));
        got.as_object_mut().expect("payload").remove("pins");
        assert_eq!(got, direct(&q, &pinned, 25));
        assert_ne!(got, direct(&q, &pinned[..1], 25), "not vacuous: pinning the twin moves the payload");
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

    /// One row's cell by header, "" when the row or the column is absent.
    fn cell(v: &serde_json::Value, lane: &str, ticker: &str, col: &str) -> String {
        let rows = v[lane].as_array().into_iter().flatten().filter_map(|r| r.as_array());
        let row = rows.into_iter().find(|r| r.iter().any(|c| c[0] == "TICKER" && c[1] == ticker));
        let hit = row.and_then(|r| r.iter().find(|c| c[0] == col).and_then(|c| c[1].as_str()));
        hit.unwrap_or_default().to_string()
    }

    /// (#403) The page's BUY% is `screen`'s BUY NOW: the one gate-clearing name takes the whole book,
    /// the pinned row that fails a gate is on the table but unfunded. An upload that turns on the
    /// vol-target book loses the column, because its fund sector cap reads holdings the page lacks.
    #[test]
    fn buy_share_is_the_sized_book_and_leaves_with_it() {
        let q = pool();
        let got = engine("tickers: [AAPL]\n", &q);
        assert_eq!(cell(&got, "stocks", "NVD.DE", "BUY%"), "100.0%");
        assert_eq!(cell(&got, "stocks", "AAPL", "RANK"), "2*#", "not vacuous: the pinned row is on the table");
        assert_eq!(cell(&got, "stocks", "AAPL", "BUY%"), "", "a gated pin is not funded");
        let vol = engine("sizing:\n  equal_weight_book: false\n", &q);
        assert_eq!(cell(&vol, "stocks", "NVD.DE", "TICKER"), "NVD.DE");
        assert!(vol["stocks"][0].as_array().is_some_and(|r| r.iter().all(|c| c[0] != "BUY%")), "no column, not a blank one");
    }

    /// (#421) The page ranks the pool as a SET: reversed, or with a row every gate refuses appended, the
    /// payload is byte-equal. A tie is where order leaks, so the pool carries one: five identical
    /// winners, which rank by ticker. Before the tie-break they came out in HashMap order, a random
    /// one of 120 per call.
    #[test]
    fn the_payload_is_a_function_of_the_pool_as_a_set() {
        let mut q = pool();
        for (t, name) in [("NVD5.DE", "Epsilon"), ("NVD3.DE", "Gamma"), ("NVD2.DE", "Beta"), ("NVD4.DE", "Delta")] {
            let mut twin = winner();
            (twin.ticker, twin.name) = (t.into(), format!("{name} Corp"));
            q.push(twin);
        }
        let tickers = |v: &serde_json::Value| -> Vec<String> {
            let rows = v["stocks"].as_array().into_iter().flatten().filter_map(|r| r.as_array());
            rows.filter_map(|r| r.iter().find(|c| c[0] == "TICKER").and_then(|c| c[1].as_str()).map(String::from)).collect()
        };
        let want = engine("", &q);
        assert_eq!(tickers(&want), ["NVD.DE", "NVD2.DE", "NVD3.DE", "NVD4.DE", "NVD5.DE"], "an exact tie ranks by ticker");
        let mut rev = q.clone();
        rev.reverse();
        assert_eq!(engine("", &rev), want, "the pool reversed");
        let mut junk = pool().remove(0);
        (junk.ticker, junk.name) = ("JUNK".into(), "Junk Corp".into());
        q.push(junk);
        assert_eq!(engine("", &q), want, "an unpinned row every gate refuses");
    }

    /// (#403) "Why isn't X in?" answers from the ticker, another venue's line, or part of the name, and
    /// says plainly when nothing matches rather than claiming the name was never scanned.
    #[test]
    fn explain_finds_the_row_by_ticker_alias_or_name() {
        let u = universe(&pool());
        let why = |x: &str| explain("", &u, x).expect("explains");
        assert!(why("aapl").starts_with("AAPL is scanned but fails 1 growth gate:\n  AAPL       history:"), "{}", why("aapl"));
        assert_eq!(why("Apple"), format!("Matched by name: Apple Inc. (AAPL)\n\n{}", why("aapl")), "by name, and says so");
        assert!(why("n").starts_with("Matched by name: NVIDIA Corp (NVD.DE)"), "a name STARTING with it first");
        assert!(why("n").ends_with("Also matching \"n\": AAPL, VWCE.DE"), "the rest in pool order: {}", why("n"));
        assert!(why("vwce.l").starts_with("VWCE.DE isn't assessable"), "by alias: {}", why("vwce.l"));
        assert!(why("msft").starts_with("MSFT isn't assessable"));
        assert!(why(" nvidia ").contains("\n\n─── how the #1 SCORE was computed — NVIDIA Corp (NVD.DE), score 6.70"));
        assert!(!why("nvidia").contains("Also matching"), "the hit itself is not an also");
        assert!(why("i").ends_with("\n\nAlso matching \"i\": VWCE.DE, MSFT, NVD.DE"), "{}", why("i"));
        assert!(why("NOPE").starts_with("No pool row matches \"NOPE\". The pool keeps one listing per company"));
        assert!(why("").starts_with("No pool row matches \"\""), "an empty box matches no name");
        assert!(explain("tickers: [", &u, "aapl").is_err_and(|e| e.starts_with("settings.yaml:")));
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
