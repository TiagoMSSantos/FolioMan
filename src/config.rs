//! User-editable folioman config, loaded from `config/settings.yaml`.
//! Language-agnostic YAML so any tool can read the same source of truth.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub tickers: Vec<String>,
    pub dip_days: i64,
    pub high_days: i64,
    pub drawdown_pct: f64,
    pub drop_pct: f64,
    #[serde(default = "default_universe_size")]
    pub universe_size: usize, // top-N per class (crypto + stocks/ETFs) `screen` pulls from the live sources
    #[serde(default = "default_true")]
    pub universe_prefer_eur: bool, // crypto in the live universe quoted in EUR (BTC-EUR) if true, else USD
    pub euribor_3m: f64,
    pub euribor_3m_date: String,
    pub ntfy_topic: String,
    #[serde(default = "default_top_picks")]
    pub top_picks: usize, // how many buy candidates `check` lists after the table
    #[serde(default)]
    pub widths: Widths, // column truncate/pad widths for the tables
    #[serde(default)]
    pub buy_heuristic: BuyHeuristic, // tunable gates/weights/caps for the picks score
    pub urls: Urls,
}

fn default_top_picks() -> usize {
    5
}

fn default_universe_size() -> usize {
    100
}

fn default_true() -> bool {
    true
}

/// Table column widths (chars): each value is truncated AND padded to this. Optional in
/// YAML — omit the `widths:` block or any field for these defaults.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Widths {
    pub name: usize,     // NAME column (check/screen/picks)
    pub ticker: usize,   // TICKER column
    pub market: usize,   // MARKET column (check)
    pub headline: usize, // HEADLINE column (check)
}

impl Default for Widths {
    fn default() -> Self {
        Widths { name: 26, ticker: 8, market: 11, headline: 31 }
    }
}

/// Tunable knobs for the buy-candidate heuristic (`src/picks.rs`). Every field is optional
/// in YAML — omit the whole `buy_heuristic:` block or any field to use these defaults.
/// Caps stop one signal dominating; gates exclude candidates outright.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BuyHeuristic {
    pub min_1y_pct: f64,        // 1Y floor (equities): reject below this (mildly negative = allow a pullback, reject a downtrend)
    pub min_1y_pct_crypto: f64, // 1Y floor for crypto/FX (-USD/-EUR tickers): looser, they're far more volatile
    pub max_1m_drop_pct: f64,   // reject if 1M % <= this (falling-knife gate)
    pub min_long_pct: f64,      // reject if long-term (>2Y) % <= this (structural-decline gate)
    pub on_sale_weight: f64,    // weight on the pullback off the ~1Y high (the core "on sale" reward)
    pub on_sale_cap: f64,       // cap on the % below the recent high fed into the score
    pub y1_weight: f64,         // weight on 1Y momentum (kept small — momentum is a gate, not the prize)
    pub y1_cap: f64,            // cap on the 1Y % fed into the score
    pub long_weight: f64,       // weight on the long-term (>2Y) trend
    pub long_cap: f64,          // cap on the long-term % fed into the score
    pub recovery_weight: f64,   // bonus when pulled back on the month but turning back up (bounce, not knife)
    pub fresh_dip_weight: f64,  // bonus when falling THIS WEEK (recent dip ranks above a stale month-old one)
    pub fresh_dip_cap: f64,     // cap on the 1W drop % fed into the fresh-dip bonus
    pub prefer_eur: bool,       // dedup currency twins (BTC-EUR/BTC-USD): keep the EUR leg if true, else USD
}

impl Default for BuyHeuristic {
    fn default() -> Self {
        BuyHeuristic {
            min_1y_pct: 0.0,
            min_1y_pct_crypto: -60.0, // crypto routinely swings -40% in a year without breaking
            max_1m_drop_pct: -15.0,
            min_long_pct: 0.0,
            on_sale_weight: 1.0, // a pullback is the dominant signal
            on_sale_cap: 35.0,   // a ~35%-off dip maxes it; a 60%+ collapse (likely broken) can't dominate
            y1_weight: 0.05,     // small: a +400% rocket no longer drowns out an on-sale quality name
            y1_cap: 50.0,
            long_weight: 0.05,
            long_cap: 300.0,
            recovery_weight: 1.0,
            fresh_dip_weight: 0.3, // up to fresh_dip_cap×this nudge; lifts a this-week faller over a stale dip
            fresh_dip_cap: 15.0,
            prefer_eur: true,
        }
    }
}

/// Every data-source URL the tool hits. Templates use `{placeholder}` tokens replaced
/// at fetch time (`{ticker}`, `{range}`, `{topic}`). Edit in settings.yaml.
#[derive(Debug, Deserialize, Clone)]
pub struct Urls {
    pub yahoo_chart: String,   // {ticker} {range}
    pub yahoo_intraday: String, // {ticker} — hourly bars (~2d) for the screen 1h/6h/12h columns
    pub yahoo_search: String,  // {ticker}
    pub yahoo_quote: String,   // {ticker} (human quote page, for `perf`)
    pub euribor: String,
    pub us_cpi: String, // BLS CPI-U (no placeholder; seriesID is in the URL path)
    pub pt_cpi: String,
    pub eu_hicp: String,
    pub coingecko_markets: String, // {n} = top-N crypto by market cap -> screen universe
    pub sp500_csv: String,         // S&P 500 constituents CSV -> screen stock/ETF universe
    pub ntfy: String,          // {topic}
}

/// Locate `config/settings.yaml` next to the exe or up the tree, mirroring the old
/// Python "run from any cwd" behaviour. Falls back to `config/settings.yaml` relative
/// to the current directory.
fn settings_path() -> PathBuf {
    // 1. next to / above the executable (installed binary layout)
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            let cand = d.join("config/settings.yaml");
            if cand.is_file() {
                return cand;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    // 2. up from the current working directory
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = Some(cwd);
        while let Some(d) = dir {
            let cand = d.join("config/settings.yaml");
            if cand.is_file() {
                return cand;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    PathBuf::from("config/settings.yaml")
}

/// Read + parse the settings file. Panics with a clear message if missing/invalid —
/// config errors are a startup problem the user must fix, not something to fail soft on.
pub fn load() -> Settings {
    let path = settings_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_yaml::from_str(&text)
        .unwrap_or_else(|e| panic!("invalid YAML in {}: {e}", path.display()))
}
