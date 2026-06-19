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
/// Tuned for a multi-DECADE buy-and-hold. Score (see `buy_score` in `src/picks.rs`, each knob = one
/// named step): `base = discount × trend_health × momentum + long_reward(A) + cheap_reward(C) +
/// dividend_reward(D)`, then `score = base × value(E) × decline(B) × trust`. GATES exclude a
/// candidate outright; SCORE knobs rank the survivors.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BuyHeuristic {
    // --- GATES: a candidate failing ANY of these is dropped before scoring ---
    pub min_1y_pct: f64,             // equities: reject if 1Y % <= this (a deep 1-year downtrend isn't a pullback)
    pub min_1y_pct_crypto: f64,      // crypto/FX (-EUR/-USD): looser 1Y floor — they swing far harder
    pub max_1m_drop_pct: f64,        // equities: reject if 1M % <= this (a hard monthly crash = falling knife)
    pub max_1m_drop_pct_crypto: f64, // crypto/FX: looser knife — a -20%/month alt is normal, not broken
    pub min_long_pct: f64,           // equities: reject if any 5Y/10Y/20Y leg <= this (structural decline)
    pub min_long_pct_crypto: f64,    // crypto/FX: reject if the >2Y leg <= this CUMULATIVE % (a corpse, e.g. -70%+)
    pub min_avg_turnover_eur: f64,   // reject if avg daily turnover (EUR) < this (thin/illiquid name); 0 = off

    // --- SCORE: how the survivors are ranked (higher = more interesting) ---
    pub normal_volatility_pct: f64,  // a "typical" daily swing (%); the dip is scaled by normal/asset vol, so a calm name's dip counts for more than a wild one's
    pub discount_cap: f64,           // cap on that volatility-scaled dip (one very deep name can't run away with the ranking)
    pub momentum_bounce: f64,        // discount ×this when a pulled-back name is turning UP (green week) — reward the bounce (>1; 1.0 = ignore weekly timing)
    pub momentum_knife: f64,         // discount ×this when it's still falling (red week & day) — dock the knife (<1; 1.0 = ignore weekly timing)
    pub long_trend_weight: f64,      // reward per %/yr of the long leg's CAGR (annualized >2Y trend) — proven-compounder bonus
    pub long_trend_cap: f64,         // cap on that long-leg CAGR (%/yr) fed into the reward (a +50%/yr coin doesn't 5× a +10%/yr one)
    pub health_zero_cagr: f64,       // long-leg CAGR (%/yr, negative) at which trend_health hits 0 (a decaying multi-year trend); health=1 at flat/rising
    pub sustained_decline_pct: f64,  // (B) if BOTH 1Y and 5Y % are <= this, the name is bleeding for years (value trap) -> score ×penalty
    pub sustained_decline_penalty: f64, // (B) multiplier applied when the sustained-decline condition holds (e.g. 0.4)
    pub cheap_weight: f64,           // (C) reward per % the price sits below its ~200wk SMA (structural "cheap vs trend")
    pub cheap_cap: f64,              // (C) cap on that below-SMA % fed into the cheap reward
    pub dividend_weight: f64,        // (D) reward per % of trailing-1Y dividend yield (reinvested divs dominate long-run total return)
    pub dividend_cap: f64,           // (D) cap on the yield % fed into the dividend reward
    pub ref_pe: f64,                 // (E) "fair" trailing P/E: value tilt = ref_pe/PE, clamped — cheap (<ref) lifts, rich (>ref) dampens; no PE = neutral

    pub prefer_eur: bool,            // dedup currency twins (BTC-EUR/BTC-USD): keep the EUR leg if true, else USD
}

/// (E) Hard clamp bounds on the P/E value tilt, so one absurdly cheap/expensive P/E can't swamp the
/// score. ponytail: fixed — these are guardrails, not a thing anyone tunes; widen here if ever needed.
pub const VALUE_TILT_MIN: f64 = 0.5;
pub const VALUE_TILT_MAX: f64 = 1.5;

/// (C) Sessions in the long moving-average window (~200 weeks of trading days). ponytail: a const,
/// not a knob — 200wk is the conventional long-trend line; change here if you disagree.
pub const LONG_MA_SESSIONS: usize = 1000;

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
            long_trend_weight: 0.5,        // per %/yr CAGR: a +30%/yr compounder adds ~15, secondary to the discount (cap 35)
            long_trend_cap: 30.0,          // cap the long-leg CAGR at 30%/yr (a +46%/yr coin doesn't dwarf a +14%/yr one)
            health_zero_cagr: -10.0,       // a -10%/yr multi-year trend = dead -> trend_health 0
            sustained_decline_pct: -40.0,  // (B) 1Y AND 5Y both <= -40% = multi-year bleed, not a dip
            sustained_decline_penalty: 0.4, // (B) score ×0.4 when that holds (value-trap dock)
            cheap_weight: 0.15,            // (C) ~+9 at the cap for a name 60% below its 200wk trend
            cheap_cap: 60.0,               // (C) cap the below-SMA % fed into the cheap reward
            dividend_weight: 1.5,          // (D) ~+9 at the cap for a 6% yielder
            dividend_cap: 6.0,             // (D) cap the trailing yield % fed into the dividend reward
            ref_pe: 20.0,                  // (E) "fair" P/E; PE 10 -> ×1.5 (capped cheap), PE 40 -> ×0.5 (capped rich)
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
    // (E) trailing P/E source for the valuation tilt. {ticker} + {key} (from FMP_API_KEY env, kept
    // out of config). Defaulted so an older settings.yaml without it still loads; only hit for
    // equities when the env key is set (free tiers are rate-limited -> `check`-scale, not `screen`).
    #[serde(default = "default_fundamentals_url")]
    pub fundamentals: String,
}

/// Default (E) fundamentals endpoint: Financial Modeling Prep's free `quote` (carries `pe`).
fn default_fundamentals_url() -> String {
    "https://financialmodelingprep.com/api/v3/quote/{ticker}?apikey={key}".to_string()
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
