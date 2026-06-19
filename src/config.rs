//! User-editable folioman config, loaded from `config/settings.yaml`.
//! Language-agnostic YAML so any tool can read the same source of truth.

use serde::Deserialize;
use std::collections::BTreeMap;
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
    #[serde(default)]
    pub anchor_windows: BTreeMap<String, i64>, // per-horizon ±days averaged around the anchor date; missing label = built-in default (see core::default_anchor_half)
    #[serde(default)]
    pub inflation_adjust: InflationAdjust, // show real (inflation-adjusted) returns on the 1Y+ columns
    pub urls: Urls,
}

/// Toggle for showing REAL (inflation-adjusted) returns on the 1Y/5Y/10Y/20Y % columns instead of
/// nominal. Off by default. When on, deflates by the ACTUAL cumulative EU HICP inflation over each
/// horizon (fetched live, same source as the `check` footer) — no rate to guess.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct InflationAdjust {
    pub enabled: bool, // false = raw nominal % (default); true = deflate long-horizon returns by live EU HICP
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
    pub market: usize,   // MARKET column (check/picks)
    pub price: usize,    // PRICE(EUR) column (check/screen/picks)
    pub headline: usize, // HEADLINE column (check)
    pub score: usize,    // SCORE column (picks) — shown to 1 decimal
}

impl Default for Widths {
    fn default() -> Self {
        Widths { name: 26, ticker: 8, market: 11, price: 13, headline: 31, score: 5 }
    }
}

/// Tunable knobs for the buy-candidate heuristic (`src/picks.rs`). Every field is optional in
/// YAML — omit the whole `buy_heuristic:` block or any field to use these defaults.
///
/// The score is: `discount × trend_health × momentum + long-trend reward`, then halved if the
/// asset has no 10Y history. Read `buy_score` in `src/picks.rs` alongside these — each knob below
/// maps to one named step there. GATES exclude a candidate outright; SCORE knobs rank survivors.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BuyHeuristic {
    // --- GATES: a candidate failing ANY of these is dropped before scoring ---
    pub min_1y_pct: f64,             // equities: reject if 1Y % <= this (a deep 1-year downtrend isn't a pullback)
    pub min_1y_pct_crypto: f64,      // crypto/FX (-EUR/-USD): looser 1Y floor — they swing far harder
    pub max_1m_drop_pct: f64,        // equities: reject if 1M % <= this (a hard monthly crash = falling knife)
    pub max_1m_drop_pct_crypto: f64, // crypto/FX: looser knife — a -20%/month alt is normal, not broken
    pub min_long_pct: f64,           // equities: reject if any 5Y/10Y/20Y leg <= this (structural decline)
    pub min_long_pct_crypto: f64,    // crypto/FX: reject if the >2Y leg <= this (a corpse, e.g. -70%+); also the trend_health zero-point
    pub min_avg_turnover_eur: f64,   // reject if avg daily turnover (EUR) < this (thin/illiquid name); 0 = off

    // --- SCORE: how the survivors are ranked (higher = more interesting) ---
    pub normal_volatility_pct: f64,  // a "typical" daily swing (%); the dip is scaled by normal/asset vol, so a calm name's dip counts for more than a wild one's
    pub discount_cap: f64,           // cap on that volatility-scaled dip (one very deep name can't run away with the ranking)
    pub momentum_bounce: f64,        // discount ×this when a pulled-back name is turning UP (green week) — reward the bounce (>1)
    pub momentum_knife: f64,         // discount ×this when it's still falling (red week & day) — dock the knife (<1)
    pub long_trend_weight: f64,      // small reward added for a strong multi-year (>2Y) uptrend (momentum is a gate, not the prize)
    pub long_trend_cap: f64,         // cap on the multi-year % fed into that reward

    pub prefer_eur: bool,            // dedup currency twins (BTC-EUR/BTC-USD): keep the EUR leg if true, else USD
}

impl Default for BuyHeuristic {
    fn default() -> Self {
        BuyHeuristic {
            // gates
            min_1y_pct: 0.0,
            min_1y_pct_crypto: -60.0,      // crypto routinely swings -40% in a year without breaking
            max_1m_drop_pct: -15.0,
            max_1m_drop_pct_crypto: -35.0, // alts routinely shed -20..-30% in a month without breaking
            min_long_pct: 0.0,
            min_long_pct_crypto: -70.0,    // -EUR 5Y is peak-anchored: allow deep pullbacks, cut true corpses (-70%+)
            min_avg_turnover_eur: 0.0,     // off by default; settings.yaml sets a real floor to drop thin names
            // score
            normal_volatility_pct: 2.0,    // ~2%/day = a typical large-cap equity
            discount_cap: 35.0,            // a ~35%-off (for its vol) dip maxes the discount
            momentum_bounce: 1.0,          // neutral: a weekly bounce is noise at a multi-decade hold horizon
            momentum_knife: 1.0,           // neutral: this-week direction shouldn't reorder a 40-year pick
            long_trend_weight: 0.03,       // reward a proven long compounder, but stay below the discount term
            long_trend_cap: 1000.0,        // let a +1000%+ multi-decade track record count (was 300, too flat)
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
    pub nupl: String,          // latest Bitcoin NUPL (net unrealized profit/loss) -> screen sentiment line
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
