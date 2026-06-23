//! User-editable folioman config, loaded from `config/settings.yaml`.
//! Language-agnostic YAML so any tool can read the same source of truth.
//! Acronyms (CAGR, ROE, P/E, NUPL, SMA, …): see the Glossary in README.md.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub tickers: Vec<String>, // single watchlist: `check`/`perf`/`alert` fetch it as their default list AND `screen` always fetches+pins it (marked PIN, exempt from the sector/score cut) so you can compare holdings against the top growth candidates. ponytail: one list, two roles

    pub dip_days: i64,
    pub high_days: i64,
    pub drawdown_pct: f64,
    pub drop_pct: f64,
    #[serde(default = "default_universe_size")]
    pub universe_size: usize, // top-N per class (crypto + stocks/ETFs) `screen` pulls from the live sources
    #[serde(default = "default_fetch_concurrency_multiplier")]
    pub fetch_concurrency_multiplier: usize, // in-flight fetches = CPU cores × this (default 8); raise to scan faster, lower if Yahoo 429s
    #[serde(default = "default_fetch_requests_per_second")]
    pub fetch_requests_per_second: f64, // global outbound-request pacer (req/s); spaces launches so the fan-out can't burst-429 (default 10). 0 = no pacing
    #[serde(default = "default_true")]
    pub universe_prefer_eur: bool, // crypto in the live universe quoted in EUR (BTC-EUR) if true, else USD
    #[serde(default)]
    pub sectors: Vec<String>, // `screen` sector filter (GICS keyword, case-insensitive substring): which company/ETF types to fetch. Empty = ALL sectors. e.g. [Technology, Communication, Semiconductor] = tech only. Stocks filtered before fetch (by GICS sector); ETFs filtered by fund name (no GICS for funds)
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

fn default_fetch_concurrency_multiplier() -> usize {
    8
}

fn default_fetch_requests_per_second() -> f64 {
    10.0
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
/// TWO scoring lanes share this struct, and `screen` prints ONLY the GROWTH lane:
/// - **GROWTH lane** (`growth_score`) — what `screen` prints; the only lane with a validated edge.
///   `base = growth_trend_weight·CAGR + growth_accel_weight·accel + risk + quality + dividend`,
///   `score = base · proximity · value(P/E) · geomean(trust, overext) + liquidity`.
///   (CAGR = Compound Annual Growth Rate.)
/// - **ON-SALE lane** (`buy_score`) — a BACKTEST FOIL ONLY (not printed by `screen`); kept so
///   `backtest` can show dip-buying has NEGATIVE multi-decade edge. Fields marked `[FOIL]` below feed
///   ONLY this lane — changing them does nothing to `screen` output.
/// Its score: `base = discount × trend_health × momentum + long_reward(A) + cheap_reward(C) +
/// dividend_reward(D)`, then `score = base × value(E) × geomean(decline(B), trust)`.
/// GATES exclude a candidate outright; SCORE knobs rank the survivors. Mirrors `config/settings.yaml`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BuyHeuristic {
    // --- GATES: a candidate failing ANY of these is dropped before scoring ---
    pub min_1y_pct: f64,             // [FOIL] on-sale only: reject if equity 1Y % <= this (growth uses a hardcoded 0% floor, not this)
    pub min_1y_pct_crypto: f64,      // crypto/FX (-EUR/-USD): looser 1Y floor — they swing far harder
    pub max_1m_drop_pct: f64,        // equities: reject if 1M % <= this (a hard monthly crash = falling knife)
    pub max_1m_drop_pct_crypto: f64, // crypto/FX: looser knife — a -20%/month alt is normal, not broken
    pub min_long_pct: f64,           // [FOIL] on-sale only: reject if any 5Y/10Y/20Y leg <= this (growth uses a hardcoded >0% 5Y gate)
    pub min_long_pct_crypto: f64,    // [FOIL] on-sale only: reject if the >2Y leg <= this CUMULATIVE % (a corpse, e.g. -70%+)
    pub min_avg_turnover_eur: f64,   // reject if avg daily turnover (EUR) < this (thin/illiquid name); 0 = off

    // --- SCORE — ON-SALE LANE (`buy_score`): EVERYTHING from here to "GROWTH LANE" below is [FOIL]
    //     (backtest-only; `screen` ignores it), EXCEPT the shared tilts noted inline. ---
    pub normal_volatility_pct: f64,  // a "typical" daily swing (%); the dip is scaled by normal/asset vol, so a calm name's dip counts for more than a wild one's
    pub discount_cap: f64,           // cap on that volatility-scaled dip (one very deep name can't run away with the ranking)
    pub discount_weight: f64,        // (#4) multiplier on the direct dip reward (discount×health×momentum). The walk-forward backtest found deepest-dip ranking is BACKWARDS on peer-relative selection, so default <1.0 demotes it toward the quality/trend terms; 1.0 = old behaviour, 0 = off. Does NOT touch discount_frac (the long_reward "must be pulled back" scaling stays on raw discount)
    pub momentum_bounce: f64,        // discount ×this when a pulled-back name is turning UP (green week) — reward the bounce (>1; 1.0 = ignore weekly timing)
    pub momentum_knife: f64,         // discount ×this when it's still falling (red week & day) — dock the knife (<1; 1.0 = ignore weekly timing)
    pub long_trend_weight: f64,      // reward per %/yr of the long leg's CAGR (annualized >2Y trend) — proven-compounder bonus
    pub long_trend_cap: f64,         // cap on that long-leg CAGR (%/yr) fed into the reward (a +50%/yr coin doesn't 5× a +10%/yr one)
    pub health_zero_cagr: f64,       // long-leg CAGR (%/yr, negative) at which trend_health hits 0 (a decaying multi-year trend); health=1 at flat/rising
    pub sustained_decline_pct: f64,  // (B) if BOTH 1Y and 5Y % are <= this, the name is bleeding for years (value trap) -> score ×penalty
    pub sustained_decline_penalty: f64, // (B) multiplier applied when the sustained-decline condition holds (e.g. 0.4)
    pub deep_decline_pct: f64,       // (B/C) a HARSHER tier: 5Y % <= this (e.g. -70%) = a 7y+ deep bleed riding a stale old chart -> deep penalty
    pub deep_decline_penalty: f64,   // (B/C) multiplier when the deep-decline 5Y condition holds (lower than sustained_decline_penalty)
    pub min_score: f64,              // (A) drop ranked rows scoring <= this (tables stop padding to top_picks with near-zero at-the-high names)
    pub cheap_weight: f64,           // (C) reward per % the price sits below its ~200wk SMA (structural "cheap vs trend")
    pub cheap_cap: f64,              // (C) cap on that below-SMA % fed into the cheap reward
    pub dividend_weight: f64,        // (D) reward per % of trailing-1Y dividend yield (reinvested divs dominate long-run total return)
    pub dividend_cap: f64,           // (D) cap on the yield % fed into the dividend reward
    pub ref_pe: f64,                 // (E) "fair" trailing P/E: value tilt = ref_pe/PE, clamped — cheap (<ref) lifts, rich (>ref) dampens; no PE = neutral
    pub quality_weight: f64,         // (F) reward per % of trailing ROE — the profitability/QUALITY factor (Novy-Marx: high-ROE firms out-compound). Applied to BOTH lanes. BACKTEST-BLIND (point-in-time ROE, no as-of), so kept small. 0 = off
    pub quality_cap: f64,            // (F) cap the ROE % fed into the quality reward (one 200%-ROE outlier can't dominate)

    // --- GROWTH LANE: a SECOND ranking (the mirror of the on-sale lane) for quality names AT/NEAR
    //     their high that are still climbing — proven compounders the on-sale score fades to ~0.
    pub growth_min_range_pct: f64,   // growth GATE (equities): must trade at/above this % of its own ~10y range (near the high); below = it's the on-sale lane's job
    pub growth_min_range_pct_crypto: f64, // growth GATE (crypto): looser range floor so more coins surface — most alts sit well below their ATH yet still out-compound; equities use the strict gate above
    pub growth_btc_outperf_weight: f64, // crypto SCORE: tilt a coin's growth score by how its 1Y return compares to BITCOIN's (the market base) — beats BTC -> boost, lags -> mild dock. 0 = off. SCREEN/CHECK-only (backtest scores names independently, so it's backtest-blind -> validated edge untouched)
    pub growth_min_cagr: f64,        // growth GATE: long-leg CAGR (%/yr) floor — below this it's not a proven compounder, just an expensive laggard
    pub growth_min_cagr_crypto: f64, // growth GATE (crypto): looser CAGR floor so ALL potential growers (not just >8%/yr) surface in the crypto table, ranked vs Bitcoin; equities keep the strict floor above
    pub growth_trend_weight: f64,    // growth SCORE: reward per %/yr of the long-leg CAGR (capped at long_trend_cap)
    pub growth_accel_weight: f64,    // growth SCORE: reward per pt the recent 1Y return outpaces the long CAGR (momentum building)
    pub growth_accel_cap: f64,       // growth SCORE: cap on that 1Y-minus-CAGR acceleration term
    pub growth_min_score: f64,       // growth SCORE: hide ranked growth rows scoring <= this (padding); 0 = show all
    pub growth_overext_cap: f64,     // (1) % ABOVE the 200wk SMA at which the overextension brake maxes out
    pub growth_overext_floor: f64,   // (1) growth-score multiplier at that cap (e.g. 0.4 = a fully-stretched name keeps 40% of its score); 1.0 = brake off
    pub growth_turnover_weight: f64, // (L) liquidity tilt: bonus per ln(turnover/€1B), added OUTSIDE the brake. Rewards deep-liquid mega-caps (easy multi-decade exit, less manipulation) so a proven compounder like NVDA isn't ranked below an illiquid €200M twin on a score tie. BACKTEST-BLIND (backtest_quote has no turnover) so it never moves the validated edge; 0 = off
    pub growth_overext_cap_crypto: f64, // (#4) crypto's OWN overextension cap (% above the 200wk SMA at which the brake maxes). Crypto routinely rides far above its long SMA, so a separate looser cap avoids over-braking coins; equities/ETFs keep growth_overext_cap. 0 = crypto brake off

    // --- CRYPTO market-sentiment damp (Bitcoin NUPL): a whole-market greed gauge already fetched for
    //     the screen footer; high NUPL = euphoria/top -> shrink crypto scores in BOTH lanes. ---
    pub nupl_euphoria: f64,          // (4) NUPL above this starts damping crypto scores (~0.5 = "belief/denial" greed zone)
    pub nupl_damp_floor: f64,        // (4) crypto-score multiplier at NUPL=1.0 (full euphoria); 1.0 = damp off
    pub nupl_capitulation: f64,      // (4) NUPL below this starts BOOSTING crypto scores (~0.25 = fear/accumulation zone); 0 = boost off
    pub nupl_boost_ceiling: f64,     // (4) crypto-score multiplier at NUPL=0 (deep capitulation); 1.0 = boost off. BACKTEST-BLIND judgment lever — keep mild

    // --- QUALITY tilts (B/C) — all from already-fetched closes, ZERO extra fetch ---
    pub sharpe_weight: f64,          // (B) GROWTH lane: reward per unit of CAGR/volatility (return per unit of daily swing). 0 = off
    pub onsale_sharpe_weight: f64,   // (B) ON-SALE lane's own Sharpe weight — split from the growth lane because the shared knob conflicts: ablation shows growth wants 0.15 but on-sale wants 0 (removing it lifts on-sale 12y edge +23.7). 0 = off
    pub sharpe_cap: f64,             // (B) cap on that CAGR/volatility ratio fed into the reward
    pub calmar_weight: f64,          // (C) reward per unit of CAGR/max-drawdown (return per worst historical pain). 0 = off
    pub calmar_cap: f64,             // (C) cap on that CAGR/max-drawdown ratio fed into the reward

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
            discount_weight: 0.35,         // (#4) demote the dip reward — walk-forward rho is NEGATIVE for on-sale across 3/5/7y and ~0 on the 354-name wide sample (deepest-dip ranking carries no selection skill); 0.35 shifts weight to the CAGR/sharpe terms that drive the working growth lane WITHOUT gutting on-sale scores (0.15 dropped normal names below min_score for only a noise-level rho gain). 1.0 = old, 0 = off
            momentum_bounce: 1.0,          // neutral: a weekly bounce is noise at a multi-decade hold horizon
            momentum_knife: 1.0,           // neutral: this-week direction shouldn't reorder a 40-year pick
            long_trend_weight: 0.5,        // per %/yr CAGR: a +30%/yr compounder adds ~15, secondary to the discount (cap 35)
            long_trend_cap: 30.0,          // cap the long-leg CAGR at 30%/yr (a +46%/yr coin doesn't dwarf a +14%/yr one)
            health_zero_cagr: -10.0,       // a -10%/yr multi-year trend = dead -> trend_health 0
            sustained_decline_pct: -40.0,  // (B) 1Y AND 5Y both <= -40% = multi-year bleed, not a dip
            sustained_decline_penalty: 0.4, // (B) score ×0.4 when that holds (value-trap dock)
            deep_decline_pct: -70.0,       // (B/C) 5Y <= -70% = a 7y+ deep bleed (e.g. LTC -73%) riding an ancient 10Y pump
            deep_decline_penalty: 0.15,    // (B/C) score ×0.15 then — harsher than the -40% tier
            min_score: 5.0,                // (A) hide ranked rows scoring <= 5 (near-the-high padding); 0 = show all top_picks
            cheap_weight: 0.07,            // (#4) ~+4 at the cap (halved from 0.15) — "structural cheap" is another dip term the backtest doesn't reward; demoted toward the trend/quality factors
            cheap_cap: 60.0,               // (C) cap the below-SMA % fed into the cheap reward
            dividend_weight: 1.5,          // (D) ~+9 at the cap for a 6% yielder
            dividend_cap: 6.0,             // (D) cap the trailing yield % fed into the dividend reward
            ref_pe: 20.0,                  // (E) "fair" P/E; PE 10 -> ×1.5 (capped cheap), PE 40 -> ×0.5 (capped rich)
            quality_weight: 0.15,          // (F) per % ROE: a 40% ROE adds ~+6 (capped) — secondary tilt, deliberately small since BACKTEST-BLIND
            quality_cap: 40.0,             // (F) cap the ROE % at 40 (a buyback-levered 200% ROE doesn't dwarf a healthy 25%)
            // growth lane (near-high compounders still climbing)
            growth_min_range_pct: 80.0,    // must sit in the top 20% of its own ~10y range. Tightened 70->80: the walk-forward shows the acceleration signal only works for genuine near-high names — at 80 the growth lane's rho rises (5y +0.24->+0.35 narrow, +0.21->+0.24 wide) AND the top/bottom-half edge flips POSITIVE (+31.6 pts wide, OOS +0.12/+0.12), i.e. top picks actually outperform. Loosening to 55 collapsed it (rho +0.10, OOS-early negative)
            growth_min_range_pct_crypto: 40.0, // crypto: looser range floor (top 60% of its own range) so more coins surface — alts spend most of their life far below ATH yet still out-compound. The BTC-relative tilt + nupl damp keep the wider crypto table honest. The strict equity gate (80) stays for stocks/ETFs
            growth_btc_outperf_weight: 0.3, // crypto: ±30% score swing at a full year of BTC-relative out/under-performance (bounded 0.5x..2x). BTC itself nets 1.0x (the neutral base). 0 = rank crypto on absolute growth only
            growth_min_cagr: 8.0,          // long-leg must compound >=8%/yr (beat a broad index) to be a "proven" grower
            growth_min_cagr_crypto: 0.0,   // crypto: any positive long trend qualifies (show ALL potential growers up to BTC); raise toward 8 to tighten the crypto table to proven compounders
            growth_trend_weight: 0.35,     // trimmed — ablation shows raw long-CAGR is mildly HARMFUL to growth selection both windows (Δ+0.03/+0.10); weight shifted to acceleration. per %/yr CAGR
            growth_accel_weight: 0.2,      // trimmed 0.35->0.2: rho RANKS accel as the dominant helper (wide Δ-0.13) but the EDGE ablation flips it — accel HURTS the profit spread (zeroing it lifts wide edge +43.7->+94.5). Recent 1Y-vs-CAGR pop is the noisiest, most mean-reverting growth signal (hot-streak chasing). 0.2 is the durable middle: wide edge +43.7->+50.5 (5y)/+24.3->+28.5 (3y), rho flat/up, OOS both halves positive. 0.0 maxes edge but flips OOS-late NEGATIVE (regime artifact). per pt the last year outpaced the long CAGR -> momentum building
            growth_accel_cap: 50.0,        // cap that acceleration term (a +200% year doesn't run away with it)
            growth_min_score: 5.0,         // hide growth rows scoring <= 5 (padding); 0 = show all top_picks
            growth_overext_cap: 100.0,     // (1) a name 100%+ above its 200wk SMA is maximally stretched
            growth_overext_floor: 0.05,    // (1) ...and keeps only 5% of its growth score at full stretch. Tightened 0.2->0.15->0.05: each step a harder blow-off-top brake (buying right after a parabolic run-up is a poor long-hold entry). The walk-forward sweep ranks 0.05 the best generalizer — wide 5y rho +0.26->+0.28 AND OOS-late rho +0.09->+0.14 (durability +55%) with the profit edge flat (+108.5->+106.8). A prior session rejected 0.1 for "docking NVDA out of the table," but that was regime-bound: at 0.05 today NVDA still scores 6.7 > growth_min_score 5 (it's -10.6% off-hi, not parabolic) and the displayed stocks order is unchanged. 1.0 = brake off
            growth_turnover_weight: 0.5,   // (L) liquidity tilt per ln(turnover/€1B), added after the brake. Lifts deep-liquid proven compounders (NVDA €32B -> +~1.0) over illiquid €200-500M names they tie/trail on the brake-docked score, without touching the validated edge (BACKTEST-BLIND)
            growth_overext_cap_crypto: 100.0, // (#4) defaults to the equity cap (NO behavior change until tuned). Raise (e.g. 200) so the brake lets crypto ride further above its SMA before docking
            nupl_euphoria: 0.5,            // (4) NUPL > 0.5 = market greed -> start damping crypto
            nupl_damp_floor: 0.5,          // (4) at NUPL 1.0 (peak euphoria) crypto scores are halved
            nupl_capitulation: 0.25,       // (4) NUPL < 0.25 = fear/accumulation -> start boosting crypto (buy-the-fear)
            nupl_boost_ceiling: 1.3,       // (4) at NUPL 0 (deep capitulation) crypto scores ×1.3. BACKTEST-BLIND judgment, kept mild
            // quality tilts (zero extra fetch)
            sharpe_weight: 0.15,           // (B) GROWTH lane. Halved 0.3->0.15: the edge ablation showed sharpe dragging the profit spread; 0.15 is the peak (5y wide edge +95.6->+107.3, beats both 0.3 and 0.0; rho +0.24, OOS positive). CAGR/vol ~10 for a calm +20%/yr name -> ~+1.5
            onsale_sharpe_weight: 0.0,     // (B) ON-SALE lane. ZEROED — split from growth because the shared knob conflicted: growth wants 0.15, on-sale wants 0. Validated: zeroing lifts on-sale 12y edge +39.2->+62.5 (Δ+23.3) while growth keeps 0.15. 0 = off
            sharpe_cap: 15.0,              // (B) cap the CAGR/volatility ratio (a low-vol freak can't run away with it)
            calmar_weight: 1.0,            // (C) cut further — the Calmar (CAGR/maxDD) tilt is mildly harmful in BOTH lanes on the wide sample too (Δ+0.02/+0.03); kept at 1.0 for a little long-hold drawdown-awareness. CAGR/maxDD ~0.4 for +20%/yr at -50% worst -> ~+0.4
            calmar_cap: 2.0,               // (C) cap the CAGR/max-drawdown ratio
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
    pub us_cpi: String, // BLS CPI-U base /data/ URL (v1); seriesID + year window POSTed by fetch_us_inflation, which swaps to /v2/ when BLS_API_KEY env is set (20y/call vs v1's 10y, 500 vs 25 req/day)
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
    // (F) ROE/quality source for the profitability tilt. {ticker} + {key}; same opt-in/rate-limit
    // profile as `fundamentals` above. Defaulted so an older settings.yaml without it still loads.
    #[serde(default = "default_fundamentals_quality_url")]
    pub fundamentals_quality: String,
    // (G) HISTORICAL date-stamped income statements for the backtest's as-of fundamentals lane. FMP
    // `stable/income-statement` quarterly carries filingDate + revenue/grossProfit/operatingIncome/
    // netIncome/eps and is free-tier reachable (key-metrics/ratios history are premium-gated). {ticker}
    // + {key}; only hit (cached) under `backtest ... fund`. Defaulted so an older settings.yaml loads.
    #[serde(default = "default_fundamentals_history_url")]
    pub fundamentals_history: String,
    // NASDAQ Trader SymDir symbol files (pipe-delimited, ETF flag column) -> screen ETF universe.
    // No free AUM-ranked ETF source exists, so these dump ALL US-listed ETFs across both exchanges;
    // the turnover gate culls the illiquid tail. Defaulted so an older settings.yaml still loads.
    #[serde(default = "default_nasdaq_listed_url")]
    pub nasdaq_listed: String,
    #[serde(default = "default_other_listed_url")]
    pub other_listed: String,
    // Börse Frankfurt / Xetra ETF search (POST) -> the EU-buyable UCITS ETF universe (the US-listed
    // ETFs above aren't EU-buyable). Signed with `bf_salt` lifted from their web bundle; if the API
    // moves or the salt rotates, refresh these two here — no recompile. Defaulted so older settings
    // still load.
    #[serde(default = "default_bf_etf_search_url")]
    pub bf_etf_search: String,
    #[serde(default = "default_bf_salt")]
    pub bf_salt: String,
}

/// Default (E) fundamentals endpoint: FMP's free `stable/quote` (carries `pe`). The old v3
/// `/api/v3/quote/` legacy endpoint died 2025-08-31 for new keys; `stable` is the replacement.
fn default_fundamentals_url() -> String {
    "https://financialmodelingprep.com/stable/quote?symbol={ticker}&apikey={key}".to_string()
}

/// Default (F) quality endpoint: FMP's `stable/ratios-ttm` (carries `returnOnEquityTTM`). v3 legacy
/// `/api/v3/ratios-ttm/` died 2025-08-31 for new keys; `stable` replaces it.
fn default_fundamentals_quality_url() -> String {
    "https://financialmodelingprep.com/stable/ratios-ttm?symbol={ticker}&apikey={key}".to_string()
}

/// Default (G) historical statements endpoint: FMP `stable/income-statement` quarterly. limit=48 =
/// ~12y of quarters in ONE call (cheap on the 250/day free budget; cached forever after). Free-tier
/// reachable; `period=quarter` on key-metrics/ratios is premium, so only income-statement is sourced.
fn default_fundamentals_history_url() -> String {
    "https://financialmodelingprep.com/stable/income-statement?symbol={ticker}&period=quarter&limit=48&apikey={key}".to_string()
}

/// Default NASDAQ-listed symbol file (ETF flag = column 6).
fn default_nasdaq_listed_url() -> String {
    "https://www.nasdaqtrader.com/dynamic/SymDir/nasdaqlisted.txt".to_string()
}

/// Default other-listed (NYSE/Arca/BATS) symbol file (ETF flag = column 4); where SPY etc. live.
fn default_other_listed_url() -> String {
    "https://www.nasdaqtrader.com/dynamic/SymDir/otherlisted.txt".to_string()
}

/// Default Börse Frankfurt ETF search (POST, turnover-sorted UCITS list).
fn default_bf_etf_search_url() -> String {
    "https://api.boerse-frankfurt.de/v1/search/etp_search".to_string()
}

/// Default request-signing salt, lifted from the Börse Frankfurt web bundle (`tracing.salt`). Public
/// (it ships in their client JS), rotates occasionally — refresh from the bundle if `screen`'s ETF
/// table empties. ponytail: a value that lives in config so a rotation needs an edit, not a rebuild.
fn default_bf_salt() -> String {
    "af5a8d16eb5dc49f8a72b26fd9185475c7a".to_string()
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
