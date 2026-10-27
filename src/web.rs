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
    /// (#461) A printed stock's European line -> its US line's quote, which [`picks::stamp_us_twin`]
    /// fills that row's n/a cells from. Only the rows `screen` printed: an upload that prints another
    /// European line shows its n/a.
    #[serde(default)]
    pub twins: HashMap<String, Quote>,
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
    twins: &HashMap<String, Quote>,
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
        twins: twins.clone(),
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
    let soc = s.social().map_err(|e| format!("settings.yaml: {e}"))?;
    // (#746) the upload's own Braked table, off the shipped rows before (#584) momentum rows join them, as
    // `screen` builds it: without it the page kept CI's
    top["braked"] = picks::braked_rows(&top["stocks"], &u.quotes, &pins, bh);
    // (#403) The equal-weight book never reads the fund look-through, so an empty map replays `screen`'s
    // BUY NOW exactly. The vol-target book caps fund sectors by holdings the page does not carry, so an
    // upload that turns it on gets no BUY% column rather than a wrong one.
    if s.sizing.equal_weight_book {
        let book = picks::buy_book(&ranked, &u.quotes, bh, &s.sizing, u.nupl, &HashMap::new());
        let shipped = picks::shipped_book(&ranked, &u.quotes, bh, &s.sizing, u.nupl, &HashMap::new()); // (#585)
        picks::stamp_momentum(&mut top, &book, &shipped, &u.quotes, &s.widths, bh, &u.fund_pe); // (#584)
        picks::stamp_buy(&mut top, &book);
    }
    picks::stamp_index(&mut top, &u.quotes); // (#446) after BUY%, which it sits beside
    picks::stamp_shadow_cols(&mut top, &u.quotes, &u.quotes, &s.widths, bh, &soc, &u.fund_pe, &|t: &str| t.to_string()); // (#746) Braked's
    picks::stamp_us_twin(&mut top, &u.twins, &s.widths, bh, &u.fund_pe);
    // (#469) every re-ranked row's quote carries its own site, so the exact ticker finds it
    picks::stamp_site(&mut top, &u.quotes, &|t: &str| t.to_string());
    top["generated"] = u.generated.into();
    // (#530) the upload's floors, which `screen` writes beside its rows: without them the page kept CI's
    top["cagr_floor"] = serde_json::json!(bh.growth_min_cagr);
    top["reit_cagr_floor"] = serde_json::json!(s.reit_min_cagr);
    top["page_min_cagr"] = serde_json::json!(s.page_min_cagr);
    top["social_cagr_floor"] = serde_json::json!(soc.buy.growth_min_cagr); // (#538)
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
    // (#1072) a class share typed as brokers and Nasdaq spell it (`BRK.B`) is Yahoo's `BRK-B` in the pool
    let dashed = q.replace('.', "-");
    let hit = u
        .quotes
        .iter()
        .find(|x| x.ticker.eq_ignore_ascii_case(q) || x.ticker.eq_ignore_ascii_case(&dashed))
        .map(|x| x.ticker.clone())
        .or_else(|| u.aliases.iter().find(|(k, _)| k.eq_ignore_ascii_case(q)).map(|(_, v)| v.clone()));
    let by_name = hit.is_none();
    let Some(t) = hit.or_else(|| named.first().map(|x| x.ticker.clone())) else {
        return Ok(format!(
            "No pool row matches \"{q}\". The pool keeps one listing per company, often a European one (NVIDIA is NVD.DE): try the company name."
        ));
    };
    // (#894) the upload's pins as `screen` maps them, a pool row or else the pool's line of the same
    // fund, so the box explains the ranking the tables beside it print
    let pins: Vec<String> =
        s.tickers.iter().filter_map(|p| if u.quotes.iter().any(|q| &q.ticker == p) { Some(p.clone()) } else { u.aliases.get(p).cloned() }).collect();
    let (text, _) = picks::render(&u.quotes, s.top_picks, &s.buy_heuristic, &s.widths, RenderCtx {
        nupl: u.nupl,
        sectors: &s.sectors,
        sector_of: &HashMap::new(),
        pinned: &pins,
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
    // (#916) no base, as the snapshot doc promises: else a partial upload ranked off code defaults
    if u.base.is_empty() {
        return Err("CI config: the snapshot carries none".to_string());
    }
    let mut merged: serde_yaml::Value = serde_yaml::from_str(&u.base).map_err(|e| format!("CI config: {e}"))?;
    let over: serde_yaml::Value = serde_yaml::from_str(overlay).map_err(|e| format!("settings.yaml: {e}"))?;
    config::merge_yaml(&mut merged, over);
    if !config::gates_configured(&merged) {
        return Err("settings.yaml: no buy_heuristic knobs left after the merge".to_string());
    }
    let s: Settings = config::settings_of(&merged).map_err(|e| format!("settings.yaml: {e}"))?;
    #[cfg(target_family = "wasm")]
    config::install(merged)?;
    let bh = &s.buy_heuristic;
    // Replayed in `screen`'s order. 1: the fund tilt, which (#816) `screen` fetches at any weight, so a
    // weight-0 upload keeps `fund` for the gates and extras that read it.
    for q in &mut u.quotes {
        if let Some(f) = &q.fund {
            q.fund_factor = core::select_fund_factor(f, &bh.growth_fund_factor);
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
        snapshot(serde_yaml::from_str(CI).ok(), quotes, None, &FundPeMap::new(), Some(0.3), &infl, &degraded, &aliases, &HashMap::new())
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
        v["braked"] = picks::braked_rows(&v["stocks"], quotes, pinned, &s.buy_heuristic); // (#746)
        // (#403) the BUY% `screen` stamps from its own sized book, with its real look-through map
        let book = picks::buy_book(&ranked, quotes, &s.buy_heuristic, &s.sizing, Some(0.3), &HashMap::new());
        let shipped = picks::shipped_book(&ranked, quotes, &s.buy_heuristic, &s.sizing, Some(0.3), &HashMap::new());
        picks::stamp_momentum(&mut v, &book, &shipped, quotes, &s.widths, &s.buy_heuristic, &FundPeMap::new()); // (#584)
        picks::stamp_buy(&mut v, &book);
        picks::stamp_index(&mut v, quotes);
        let soc = s.social().unwrap();
        picks::stamp_shadow_cols(&mut v, quotes, quotes, &s.widths, &s.buy_heuristic, &soc, &FundPeMap::new(), &|t: &str| t.to_string());
        picks::stamp_site(&mut v, quotes, &|t: &str| t.to_string()); // (#540) the REPORTS links, as `screen` stamps them
        v["generated"] = serde_json::Value::Null;
        v["cagr_floor"] = serde_json::json!(s.buy_heuristic.growth_min_cagr);
        v["reit_cagr_floor"] = serde_json::json!(s.reit_min_cagr);
        v["page_min_cagr"] = serde_json::json!(s.page_min_cagr);
        v["social_cagr_floor"] = serde_json::json!(soc.buy.growth_min_cagr);
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

    /// (#530) The page cuts and greys on the floors the payload carries, so an upload's floors must ride
    /// in it: before, the engine left them out and an uploaded `page_min_cagr: 21` hid nothing new.
    #[test]
    fn the_engine_echoes_the_uploads_cagr_floors() {
        let q = pool();
        let ci = engine("", &q);
        let ci_s: Settings = serde_yaml::from_str(CI).expect("the CI config parses");
        assert_eq!(ci["page_min_cagr"], serde_json::json!(ci_s.page_min_cagr));
        let up = engine("page_min_cagr: 21.0\nreit_min_cagr: 3.0\nbuy_heuristic:\n  growth_min_cagr: 7.0\n", &q);
        assert_eq!((&up["page_min_cagr"], &up["reit_cagr_floor"], &up["cagr_floor"]), (&serde_json::json!(21.0), &serde_json::json!(3.0), &serde_json::json!(7.0)));
        assert_ne!(ci["page_min_cagr"], up["page_min_cagr"], "not vacuous: CI's floor is not 21");
        // (#915) and the Social table greys under its own floor: buy's, unless `social_heuristic` names one
        assert_eq!(up["social_cagr_floor"], serde_json::json!(7.0), "inherits the upload's buy floor");
        let own = engine("buy_heuristic:\n  growth_min_cagr: 7.0\nsocial_heuristic:\n  growth_min_cagr: 11.0\n", &q);
        assert_eq!((&own["cagr_floor"], &own["social_cagr_floor"]), (&serde_json::json!(7.0), &serde_json::json!(11.0)));
    }

    /// (#461) The US twins ride in universe.json, so the engine fills the n/a cells `screen` fills, with
    /// the same `$` mark.
    #[test]
    fn the_engine_fills_na_cells_from_the_published_twins() {
        let q = pool();
        let sector = |u: &str| {
            let v: serde_json::Value = serde_json::from_str(&screen("", u).expect("ranks")).expect("JSON");
            let row = v["stocks"].as_array().expect("stocks lane").iter().find(|r| r.as_array().is_some_and(|c| c.contains(&serde_json::json!(["TICKER", "NVD.DE"])))).cloned();
            row.and_then(|r| r.as_array()?.iter().find(|c| c[0] == "SECTOR").map(|c| c[1].clone()))
        };
        let nvda = Quote { sector: Some("Information Technology".into()), ..Quote::stub("NVDA", "$1", "", "NVIDIA") };
        let twins = HashMap::from([("NVD.DE".to_string(), nvda)]);
        let with = snapshot(serde_yaml::from_str(CI).ok(), &q, None, &FundPeMap::new(), Some(0.3), &[], &[], &HashMap::new(), &twins);
        assert_eq!(sector(&universe(&q)), Some("n/a".into()), "not vacuous: the European line has no sector");
        assert_eq!(sector(&with), Some("Information Technology$".into()));
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
        // either list alone is something to say
        assert_eq!(engine("tickers: [VWCE.L]\n", &q)["pins"], serde_json::json!({"twins": [["VWCE.L", "VWCE.DE"]], "missing": []}));
        assert_eq!(engine("tickers: [NOPE]\n", &q)["pins"], serde_json::json!({"twins": [], "missing": ["NOPE"]}));
        assert!(engine("tickers: [AAPL]\n", &q).get("pins").is_none(), "a pool holding every pin stays byte-equal to `screen`");
    }

    /// The fund factor is re-selected from `fund` at any uploaded weight, and (#816) `fund` is KEPT at 0,
    /// as `screen` now fetches it at any weight. CI's `roic` extra reads `fund` straight, so the kept one
    /// still moves the payload at weight 0. The tilt term itself (weight x factor) is graded in `picks`;
    /// this pool's pinned row does not show it, so the old "0 moves the row" read the dropped `fund`.
    #[test]
    fn the_fund_factor_is_reselected_and_fund_kept_at_any_weight() {
        let mut q = pool();
        q[0].fund = Some(core::FundFactors { peg_yield: Some(50.0), roic: Some(40.0), ..Default::default() });
        let mut picked = q.clone();
        picked[0].fund_factor = Some(50.0);
        let mut bare = q.clone();
        bare[0].fund = None;
        let aapl = ["AAPL".to_string()];
        let want = direct(&picked, &aapl, 25);
        assert_ne!(want, direct(&bare, &aapl, 25), "not vacuous: `fund` moves the pinned row");
        assert_eq!(engine("tickers: [AAPL]\n", &q), want, "weight on: the factor is re-selected from `fund`");
        let off = "tickers: [AAPL]\nbuy_heuristic:\n  growth_fund_weight: 0\n";
        assert_ne!(engine(off, &q), engine(off, &bare), "(#816) weight off: `fund` is kept, so the roic extra still reads it");
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
        assert_eq!(cell(&got, "stocks", "NVD.DE", "BUY%"), "100.00%");
        assert_eq!(cell(&got, "stocks", "AAPL", "RANK"), "2*#", "not vacuous: the pinned row is on the table");
        assert_eq!(cell(&got, "stocks", "AAPL", "BUY%"), "", "a gated pin is not funded");
        let vol = engine("sizing:\n  equal_weight_book: false\n", &q);
        assert_eq!(cell(&vol, "stocks", "NVD.DE", "TICKER"), "NVD.DE");
        assert!(vol["stocks"][0].as_array().is_some_and(|r| r.iter().all(|c| c[0] != "BUY%")), "no column, not a blank one");
    }

    /// (#839) A pool saturated with ties: five more identical winners (one on bridged history), a
    /// three-fund family on one index with one TER and the same seven holdings, a stock named like a
    /// coin's base (`SOL` beside `SOL-USD`), and both legs of one coin.
    fn tied() -> Vec<Quote> {
        let mut q = pool();
        for (t, name) in [("NVD5.DE", "Epsilon"), ("NVD3.DE", "Gamma"), ("NVD2.DE", "Beta"), ("NVD6.DE", "Zeta"), ("NVD4.DE", "Delta")] {
            let mut twin = winner();
            (twin.ticker, twin.name) = (t.into(), format!("{name} Corp"));
            twin.history_proxied = t == "NVD6.DE";
            q.push(twin);
        }
        let holdings: Vec<String> = ["AAPL", "MSFT", "NVDA", "AMZN", "META", "GOOGL", "AVGO"].into_iter().map(String::from).collect();
        for (t, name) in [("EQQC.DE", "Gamma"), ("EQQA.DE", "Alpha"), ("EQQB.DE", "Beta")] {
            let mut fund = winner();
            (fund.ticker, fund.name) = (t.into(), format!("{name} Nasdaq 100 UCITS ETF"));
            fund.instrument_type = "ETF".into();
            fund.benchmark = Some("nasdaq 100 index".into());
            fund.expense_ratio = Some(0.2);
            fund.top_holdings = holdings.clone();
            q.push(fund);
        }
        for (t, name, kind) in [("SOL", "Sol Corp", "EQUITY"), ("ZZ-USD", "Zeta", "CRYPTOCURRENCY"), ("SOL-USD", "Solana", "CRYPTOCURRENCY"), ("ZZ-EUR", "Zeta", "CRYPTOCURRENCY")] {
            let mut x = winner();
            (x.ticker, x.name, x.instrument_type) = (t.into(), name.into(), kind.into());
            x.market = core::market_of(t);
            q.push(x);
        }
        q
    }

    /// The pins that put every tied group on the page whatever its gates say.
    const TIED_PINS: &str = "tickers: [EQQA.DE, EQQB.DE, EQQC.DE, SOL-USD, ZZ-USD, ZZ-EUR]\n";

    /// (#421) The page ranks the pool as a SET: reversed, shuffled, or with a row every gate refuses
    /// appended, the payload is byte-equal. A tie is where order leaks, so the pool is [`tied`]. Before
    /// the (#421) tie-break five tied winners came out in HashMap order, a random one of 120 per call;
    /// before (#839) `SOL` or `SOL-USD`, whichever the pool listed first, erased the other.
    #[test]
    fn the_payload_is_a_function_of_the_pool_as_a_set() {
        let mut q = tied();
        let tickers = |v: &serde_json::Value, lane: &str| -> Vec<String> {
            let rows = v[lane].as_array().into_iter().flatten().filter_map(|r| r.as_array());
            rows.filter_map(|r| r.iter().find(|c| c[0] == "TICKER").and_then(|c| c[1].as_str()).map(String::from)).collect()
        };
        let want = engine(TIED_PINS, &q);
        let nvd: Vec<String> = tickers(&want, "stocks").into_iter().filter(|t| t.starts_with("NVD")).collect();
        assert_eq!(nvd, ["NVD.DE", "NVD2.DE", "NVD3.DE", "NVD4.DE", "NVD5.DE", "NVD6.DE"], "an exact tie ranks by ticker");
        // not vacuous: every tied group reached the page
        assert!(cell(&want, "stocks", "NVD6.DE", "RANK").contains('~'), "the bridged row: {want}");
        assert!(tickers(&want, "stocks").contains(&"SOL".to_string()), "the stock named like a coin: {want}");
        assert!(tickers(&want, "crypto").contains(&"SOL-USD".to_string()), "the coin beside it: {want}");
        let zz: Vec<String> = tickers(&want, "crypto").into_iter().filter(|t| t.starts_with("ZZ")).collect();
        assert_eq!(zz, ["ZZ-EUR"], "one row per coin, the euro leg");
        assert!(["EQQA.DE", "EQQB.DE", "EQQC.DE"].iter().all(|t| cell(&want, "etfs", t, "INDEX").contains("×3")), "the fund family: {want}");
        let differs = |got: &serde_json::Value| -> Vec<String> {
            let (a, b) = (want.as_object().expect("object"), got.as_object().expect("object"));
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).filter(|k| a.get(*k) != b.get(*k)).collect();
            keys.into_iter().cloned().collect()
        };
        let mut arms = vec![("reversed".to_string(), q.iter().rev().cloned().collect::<Vec<_>>())];
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        for n in 1..=4 {
            let mut s = q.clone();
            for i in (1..s.len()).rev() {
                s.swap(i, (core::next_rand(&mut seed) % (i as u64 + 1)) as usize);
            }
            arms.push((format!("shuffle {n}"), s));
        }
        let mut junk = pool().remove(0);
        (junk.ticker, junk.name) = ("JUNK".into(), "Junk Corp".into());
        q.push(junk);
        arms.push(("an unpinned row every gate refuses".to_string(), q));
        let bad: Vec<String> = arms.iter().map(|(arm, p)| (arm, differs(&engine(TIED_PINS, p)))).filter(|(_, k)| !k.is_empty()).map(|(arm, k)| format!("{arm}: {k:?}")).collect();
        assert!(bad.is_empty(), "the pool's order reached the payload: {bad:?}");
    }

    /// (#840) The `n`th numeric leaf of `v`, in document order, set to `to`; its dotted path, or None
    /// past the last leaf.
    fn set_nth(v: &mut serde_yaml::Value, n: &mut usize, to: i32) -> Option<String> {
        if v.is_number() {
            if *n == 0 {
                *v = if v.is_f64() { f64::from(to).into() } else { to.into() };
                return Some(String::new());
            }
            *n -= 1;
            return None;
        }
        let mut hit = |k: String, x: &mut serde_yaml::Value| set_nth(x, n, to).map(|p| if p.is_empty() { k } else { format!("{k}.{p}") });
        match v {
            serde_yaml::Value::Mapping(m) => m.iter_mut().find_map(|(k, x)| hit(k.as_str().unwrap_or("?").to_string(), x)),
            serde_yaml::Value::Sequence(s) => s.iter_mut().enumerate().find_map(|(i, x)| hit(i.to_string(), x)),
            _ => None,
        }
    }

    /// (#840) An upload can set any knob to anything. Each numeric leaf of CI's config, set to 0 and
    /// to -1 over the [`tied`] pool, must rank with no `inf`/`NaN` cell or be refused with a message:
    /// never a panic, never a hang. Each arm runs on its own thread, so a hang is named rather than
    /// stalling the suite. The knobs read once per process (`config::install`) keep the process's value
    /// off wasm, so their arms pass here without reaching them.
    #[test]
    fn every_numeric_knob_at_zero_and_minus_one_ranks_finite_or_refuses() {
        let mut root: serde_yaml::Value = serde_yaml::from_str(CI).expect("the CI config parses");
        root["tickers"] = serde_yaml::from_str::<serde_yaml::Value>(TIED_PINS).expect("pins")["tickers"].clone();
        let u = std::sync::Arc::new(universe(&tied()));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut arms = vec![("baseline".to_string(), root.clone())];
        for to in [0, -1] {
            for n in 0.. {
                let (mut v, mut k) = (root.clone(), n);
                let Some(knob) = set_nth(&mut v, &mut k, to) else { break };
                arms.push((format!("{knob}: {to}"), v));
            }
        }
        let labels: Vec<String> = arms.iter().map(|(a, _)| a.clone()).collect();
        for (arm, v) in arms {
            let (tx, u, overlay) = (tx.clone(), u.clone(), serde_yaml::to_string(&v).expect("YAML"));
            std::thread::spawn(move || tx.send((arm, std::panic::catch_unwind(|| screen(&overlay, &u)).ok())));
        }
        drop(tx);
        let (mut bad, mut seen) = (Vec::new(), std::collections::HashSet::new());
        while let Ok((arm, got)) = rx.recv_timeout(std::time::Duration::from_secs(120)) {
            match &got {
                None => bad.push(format!("{arm}: panicked")),
                Some(Err(e)) if arm == "baseline" => bad.push(format!("{arm}: refused: {e}")),
                Some(Ok(json)) if json.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| t == "inf" || t == "NaN") => bad.push(format!("{arm}: a non-finite cell")),
                Some(_) => {}
            }
            seen.insert(arm);
        }
        bad.extend(labels.iter().filter(|a| !seen.contains(*a)).map(|a| format!("{a}: hung")));
        assert!(labels.len() > 300, "the sweep walks every numeric knob: {}", labels.len());
        assert!(bad.is_empty(), "{} of {} arms failed: {bad:#?}", bad.len(), labels.len());
    }

    /// (#841) The wasm job's site: `universe.json` for the [`tied`] pool and `data.json` as this native
    /// build ranks it, for web/engine/smoke.mjs to hold the wasm build to before a merge. Ignored: it
    /// writes where `PARITY_SITE` points, in that job alone, pinned to CI's config as the engine installs it.
    #[test]
    #[ignore]
    fn writes_the_wasm_parity_site() {
        let dir = std::path::PathBuf::from(std::env::var("PARITY_SITE").expect("PARITY_SITE names the site"));
        let u = universe(&tied());
        std::fs::create_dir_all(&dir).expect("the site dir");
        std::fs::write(dir.join("universe.json"), &u).expect("universe.json");
        std::fs::write(dir.join("data.json"), screen("", &u).expect("ranks")).expect("data.json");
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
        assert!(why(" nvidia ").contains("\n\n─── how the SCORE was computed — NVIDIA Corp (NVD.DE), score 6.70"));
        assert!(!why("nvidia").contains("Also matching"), "the hit itself is not an also");
        assert!(why("i").ends_with("\n\nAlso matching \"i\": VWCE.DE, MSFT, NVD.DE"), "{}", why("i"));
        assert!(why("NOPE").starts_with("No pool row matches \"NOPE\". The pool keeps one listing per company"));
        assert!(why("").starts_with("No pool row matches \"\""), "an empty box matches no name");
        assert!(explain("tickers: [", &u, "aapl").is_err_and(|e| e.starts_with("settings.yaml:")));
    }

    /// (#1072) A class share typed with a dot finds the pool's dashed row by its ticker. Before, `brk.b` fell
    /// through to the name match, and no name holds it, so the box answered "No pool row matches".
    #[test]
    fn explain_finds_a_class_share_typed_with_a_dot() {
        let mut brk = winner();
        brk.ticker = "BRK-B".into();
        let u = universe(&[brk]);
        let hit = explain("", &u, "BRK-B").expect("explains");
        assert!(!hit.starts_with("No pool row"), "not vacuous: {hit}");
        assert_eq!(explain("", &u, "brk.b").expect("explains"), hit);
    }

    /// (#894) The why box reads the upload's pins, as the tables beside it do: a pinned twin the same-name
    /// dedup would drop is a ranked row, so it explains with its score walkthrough, whether pinned by its
    /// own ticker or by another venue's line.
    #[test]
    fn explain_reads_the_uploads_pins() {
        let mut twin = winner();
        twin.ticker = "NVD.F".into();
        let quotes = [winner(), twin];
        let aliases = HashMap::from([("NVD.X".to_string(), "NVD.F".to_string())]);
        let u = snapshot(serde_yaml::from_str(CI).ok(), &quotes, None, &FundPeMap::new(), Some(0.3), &[], &[], &aliases, &HashMap::new());
        let walk = "how the SCORE was computed — NVIDIA Corp (NVD.F)";
        let why = |overlay: &str| explain(overlay, &u, "NVD.F").expect("explains");
        assert!(!why("").contains(walk), "not vacuous: unpinned, the same-name twin is dropped: {}", why(""));
        assert!(why("tickers: [NVD.F]\n").contains(walk), "{}", why("tickers: [NVD.F]\n"));
        assert!(why("tickers: [NVD.X]\n").contains(walk), "a pin on another venue's line pins the pool's row");
    }

    /// An upload that is not a usable config is an error the page prints, never a quiet default rank.
    #[test]
    fn a_bad_upload_is_refused_with_the_reason() {
        let u = universe(&pool());
        assert!(screen("tickers: [", &u).is_err_and(|e| e.starts_with("settings.yaml:")));
        assert!(screen("not_a_knob: 1\n", &u).is_err_and(|e| e.contains("not_a_knob")));
        assert!(screen("buy_heuristic: 3\n", &u).is_err_and(|e| e.contains("no buy_heuristic")));
        // (#909) a wrong-typed knob is named, and the merged config's line number is not printed as the upload's
        let typed = screen("buy_heuristic:\n  growth_min_cagr: abc\n", &u).expect_err("a string is not a CAGR floor");
        assert!(typed.contains("buy_heuristic.growth_min_cagr: invalid type") && !typed.contains(" at line "), "{typed}");
        assert!(screen("", "{}").is_err_and(|e| e.starts_with("universe.json:")));
        // (#916) a typo'd Social value is refused like a buy one, and so is a snapshot whose base is not YAML or absent
        assert!(screen("social_heuristic:\n  loud_penalty: abc\n", &u).is_err_and(|e| e.starts_with("settings.yaml: social_heuristic.loud_penalty")));
        let rebased = |base: &str| {
            let mut v: serde_json::Value = serde_json::from_str(&u).expect("the snapshot is JSON");
            v["base"] = base.into();
            v.to_string()
        };
        assert!(screen("", &rebased("tickers: [")).is_err_and(|e| e.starts_with("CI config:")));
        let bare = screen("buy_heuristic:\n  growth_min_cagr: 5.0\n", &rebased("")).expect_err("no base: never ranked off code defaults");
        assert!(bare.starts_with("CI config:"), "{bare}");
    }

    /// The two interned `&'static str` fields survive the snapshot (a None here blanks USE/REPL).
    #[test]
    fn interned_fields_round_trip() {
        let q = &pool()[1];
        let json = serde_json::to_string(q).expect("ser");
        let back: Quote = serde_json::from_str(&json).expect("de");
        assert_eq!((back.use_of_profits, back.replication), (Some("Acc"), Some("Opt")));
    }

    /// (#629) A stock whose 20Y leg compounds at `cagr` %/yr, in `sector`, carrying a stale door stamp for
    /// `load` to overwrite. Its 8Y leg stays `winner()`'s, so every such name pinned to that leg reads one number.
    fn compounder(ticker: &str, sector: Option<&str>, cagr: f64) -> Quote {
        let mut q = winner();
        (q.ticker, q.name, q.sector) = (ticker.into(), format!("{ticker} Corp"), sector.map(String::from));
        let leg = core::HORIZONS.iter().position(|(h, _)| *h == "20Y").expect("20Y is a horizon");
        q.perf[leg] = Some(("x".to_string(), ((1.0 + cagr / 100.0).powi(20) - 1.0) * 100.0));
        q.growth_sector_floor = Some(1.0);
        q
    }

    /// (#629) What `load` stamps on each quote under `overlay`, in millionths of a point, after checking it
    /// equals the replay `screen` runs on the same input (the sector-door lines in commands/screen.rs).
    fn door_stamps(overlay: &str, universe: &str) -> Vec<Option<i64>> {
        let (s, loaded) = load(overlay, universe).expect("the upload loads");
        let mut want: Universe = serde_json::from_str(universe).expect("the snapshot parses");
        let bh = &s.buy_heuristic;
        let floors = match want.spx.as_ref().and_then(|b| picks::long_cagr_pct(b, bh)) {
            Some(bc) => picks::sector_floors(&want.quotes.iter().collect::<Vec<_>>(), bh, picks::SECTOR_DOOR_K, bc),
            None => Default::default(),
        };
        picks::stamp_sector_floors(&mut want.quotes, &floors);
        let stamps = |u: &Universe| -> Vec<Option<f64>> { u.quotes.iter().map(|q| q.growth_sector_floor).collect() };
        assert_eq!(stamps(&loaded), stamps(&want), "load stamps what screen stamps");
        stamps(&loaded).into_iter().map(|f| f.map(|x| (x * 1e6).round() as i64)).collect()
    }

    /// (#629) The MISSES cell `screen` prints for the three pinned names under `overlay`.
    fn door_misses(overlay: &str, universe: &str) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(&screen(overlay, universe).expect("ranks")).expect("JSON");
        ["IND1.DE", "IND2.DE", "UTL1.DE"].into_iter().map(|t| cell(&v, "stocks", t, "MISSES")).collect()
    }

    /// (#629) `load` replays the (#328) sector door from the UPLOAD's knobs and the index leg, as `screen` does
    /// after its fetch. The shipped door is 0 and `direct()` stamps no floors, so every parity test above stays
    /// green with the replay deleted, skipped, or run on CI's knobs instead of the upload's. Here the index
    /// compounds at 14, Industrials' leader at 18, Utilities' at 12, and a stale stamp of 1.0 rides in on
    /// every row, so a missing or misplaced replay shows in the floors and in who misses.
    #[test]
    fn the_engine_replays_the_sector_door_under_the_upload() {
        let mut stale = winner(); // no sector: a stamp on it must be cleared, not kept
        stale.growth_sector_floor = Some(1.0);
        let cast = [compounder("IND1.DE", Some("Industrials"), 18.0), compounder("IND2.DE", Some("Industrials"), 16.0), compounder("UTL1.DE", Some("Utilities"), 12.0), stale];
        let bare = universe(&cast);
        let mut indexed: Universe = serde_json::from_str(&bare).expect("the snapshot parses");
        indexed.spx = Some(compounder("^GSPC", None, 14.0));
        let uni = serde_json::to_string(&indexed).expect("the snapshot serialises");
        let pins = "tickers: [IND1.DE, IND2.DE, UTL1.DE]\n";
        let armed = format!("{pins}buy_heuristic:\n  growth_sector_leaders: 1\n");
        let shut = format!("{pins}buy_heuristic:\n  growth_sector_leaders: 0\n");
        let eight = format!("{pins}buy_heuristic:\n  growth_sector_leaders: 1\n  fixed_cagr_years: 8\n");
        // Industrials' leader sets the floor its peer shares, Utilities' 12 is lifted to the index's 14, the sectorless row is cleared
        let tuned: [Option<i64>; 4] = [Some(18_000_000), Some(18_000_000), Some(14_000_000), None];
        assert_eq!(door_stamps(&armed, &uni), tuned, "armed: each sector's leader CAGR, floored at the index; stale stamps overwritten");
        assert_eq!(door_stamps(&shut, &uni), tuned, "door at 0: stamped all the same, the knob only decides whether the stamp is read");
        let e = (core::cagr(400.0, 8.0) * 1e6).round() as i64;
        assert_eq!(door_stamps(&eight, &uni), [Some(e), Some(e), Some(e), None], "the upload's fixed_cagr_years picks the leg: the replay runs after the merge");
        assert_eq!(door_stamps(&armed, &bare), [None::<i64>; 4], "no index leg: no door, and the stale stamps are cleared");
        assert_eq!(door_misses(&armed, &uni), ["0", "1: cagr", "1: cagr"], "armed: only the leader clears the floor it set");
        assert_eq!(door_misses(&shut, &uni), ["1: cagr", "1: cagr", "1: cagr"], "door at 0: all three miss the shipped 19, so arming flips IND1.DE alone");
    }
}
