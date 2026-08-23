//! User-editable folioman config, loaded from `config/settings.yaml`.
//! Language-agnostic YAML so any tool can read the same source of truth.
//! Acronyms (CAGR, ROE, P/E, NUPL, SMA, …): see the Glossary in README.md.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)] // a typo'd key must error, not silently fall back to the default
pub struct Settings {
    pub tickers: Vec<String>, // single watchlist: `check`/`perf`/`alert` fetch it as their default list AND `screen` always fetches+pins it (marked PIN, exempt from the sector/score cut) so you can compare holdings against the top growth candidates. note: one list, two roles

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
    #[serde(default)]
    pub compute_threads: usize, // worker threads for the CPU-bound halves of `backtest` — the per-ticker walk, the knob sweeps and the `tune` search. 0 (DEFAULT) = every logical core. This is NOT `fetch_concurrency_multiplier`: that one sizes in-flight NETWORK requests and wants to be much larger than the core count, this one sizes actual compute and wants to be at most it. Set it to cap FolioMan while you use the machine for something else; `RAYON_NUM_THREADS=n` still works as a one-off override whenever this is left at 0. Thread count must never change a printed number — the walk collects in ticker order and every seeded PRNG stream stays serial — and `tests/backtest_fixture.rs` pins that: the frozen-data goldens are generated single-threaded and must reproduce byte-for-byte at any setting
    #[serde(default = "default_true")]
    pub universe_prefer_eur: bool, // crypto in the live universe quoted in EUR (BTC-EUR) if true, else USD
    #[serde(default)]
    pub prefer_eu_listing: bool, // EQUITY VENUE SWAP: replace a constituent-CSV stock with its Xetra twin (GOOGL -> ABEA.DE) when one resolves and its chart proves out (EUR, enough bars), so the row a EUR investor sees is the line they can actually buy. Resolution is OpenFIGI (`openfigi_mapping`), TICKER -> shareClassFIGI -> every listing, `GY` venue only; the US line stays whenever the twin does not resolve or its chart fails. false = off (DEFAULT), and off is byte-identical to no swap at all. EDGE-AFFECTING and UNGRADEABLE — the swapped series is EUR-denominated, so its CAGR carries EUR/USD drift while every threshold in ci-settings was calibrated on USD series; Xetra history also starts 2007-12-31 for nearly every name (18.6y, under the 20Y rung, harmless only while `fixed_cagr_years` <= 8). Turnover drops to the Xetra line's, which moves `liq_bonus` and the rank tie-break. Read one real screen before trusting any number it produces
    #[serde(default)]
    pub sectors: Vec<String>, // `screen` sector filter (GICS keyword, case-insensitive substring): which company/ETF types to fetch. Empty = ALL sectors. e.g. [Technology, Communication, Semiconductor] = tech only. Stocks filtered before fetch (by GICS sector); ETFs filtered by fund name (no GICS for funds)
    pub ntfy_topic: String,
    #[serde(default)]
    pub monthly_deploy_eur: f64, // deploy-math base: the € amount you put in per month. `screen` prints "DEPLOY THIS MONTH: €X" = this × the entry-state multiplier (near-high 1×, pullback 1.5×, drawdown 2×). 0 (default) = line off. Personal number — set it in the private config/settings.yaml overlay, never in the shared base
    #[serde(default = "default_top_picks")]
    pub top_picks: usize, // how many buy candidates `check` lists after the table
    #[serde(default = "default_stale_days")]
    pub stale_days: i64, // (D) `screen` drops a name whose newest close bar is older than this many CALENDAR days (halted/dead listing frozen at a stale price -> a fake near-high). Default 7 tolerates a long weekend/holiday. 0 = off (keep everything)
    #[serde(default)]
    pub widths: Widths, // column truncate/pad widths for the tables
    #[serde(default)]
    pub buy_heuristic: BuyHeuristic, // tunable gates/weights/caps for the picks score
    #[serde(default)]
    pub anchor_windows: BTreeMap<String, i64>, // per-horizon ±days averaged around the anchor date; missing label = built-in default (see core::default_anchor_half)
    #[serde(default)]
    pub history_proxy: BTreeMap<String, String>, // young listing -> older SAME-strategy, SAME-currency twin (e.g. VUAA.DE: SXR8.DE); the twin's closes are rebased+prepended so the young wrapper is scored on the strategy's proven history (marked ~ in the table). User-curated only — a wrong twin silently corrupts CAGR
    #[serde(default)]
    pub inflation_adjust: InflationAdjust, // show real (inflation-adjusted) returns on the 1Y+ columns
    #[serde(default)]
    pub sizing: Sizing, // (P5) risk budget for the `size` table — class split and the two concentration caps
    pub urls: Urls,
}

/// (P5) THE RISK BUDGET behind the `size` table. Separate from `buy_heuristic` on purpose: nothing
/// here touches a score, a gate or a ranking, and none of it is reachable from `backtest` — `size` is
/// a live, read-only command, so no receipt in tests/ci-settings.yaml can ever be graded against these
/// and none should pretend to be. They are POLICY, set by judgement, and that is the honest standing.
///
/// WHAT THEY REPLACE: `size_weights` used to split gross EQUALLY across the asset classes present, so
/// a single crypto name that scored positive drew 33% of the book — the same share as the entire
/// stock class — purely because it was the only coin. There was no per-name cap and no sector cap at
/// all, so within a class one high-score low-vol name could take most of what was left.
///
/// The caps redistribute WITHIN their class and never across one, so the budget split above cannot be
/// quietly undone by a cap. CONSEQUENCE, and it is intended: a class too small to absorb its budget
/// under the name cap leaves the remainder UNALLOCATED — 3 stocks under a 4% cap deploy 12%, not 70%
/// — and the table's total prints below 100 to say so. That is the honest reading of "you do not hold
/// enough names to put this much in this class", not a bug to normalise away.
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(default, deny_unknown_fields)] // a typo'd knob must error, not silently fall back to the default
pub struct Sizing {
    pub budget_stock: f64,   // (P5) share of gross for the single-stock class, RENORMALISED over the classes actually present — with no coin in the table the stock/ETF pair rescales to 73.7/26.3 rather than leaving 5% idle. Relative weights, so 70/25/5 and 14/5/1 mean the same thing
    pub budget_etf: f64,     // (P5) share of gross for funds. Above the crypto share and below the stock one: an index ETF is already an internally diversified block, so it needs less of the name-level diversification the stock budget is buying, but it is not the tail bet crypto is
    pub budget_crypto: f64,  // (P5) share of gross for coins. 5 is a JUDGEMENT VALUE and the one number here with a real consequence: it is the standing answer to "how much of the book may sit in the asset class that has drawn down 80%+ in every cycle it has had"
    pub max_name_pct: f64,   // (P5) no single row may exceed this % of gross. Excess is redistributed to the UNCLAMPED members of the same class in proportion to their vol-target weight, iterated to a fixpoint (capped at 5 passes — the loop is monotone, so a pass that moves nothing ends it). 0 = off
    pub max_sector_pct: f64, // (P5) no GICS sector may exceed this % of gross, STOCK CLASS ONLY: `quote.sector` is filled for equities live, ETFs carry a fund-level sector that means something different, and a coin has none. Applied after the name cap and re-checked with it, since capping a sector hands weight to names that may then breach their own ceiling. 0 = off
}

/// Defaults are the SHIPPED policy, not a neutral off-state — the one place in this file where a
/// knob's default deliberately does NOT reproduce the previous behaviour. `size` never runs in the
/// backtest and no golden covers it, so there is nothing for an off-by-default to protect; leaving
/// the old equal-class split as the default would just mean the fix ships disabled.
impl Default for Sizing {
    fn default() -> Self {
        Self { budget_stock: 70.0, budget_etf: 25.0, budget_crypto: 5.0, max_name_pct: 4.0, max_sector_pct: 25.0 }
    }
}

/// Toggle for showing REAL (inflation-adjusted) returns on the 1Y/5Y/10Y/20Y % columns instead of
/// nominal. Off by default. When on, deflates by the ACTUAL cumulative EU HICP inflation over each
/// horizon (fetched live, same source as the `check` footer) — no rate to guess.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)] // a typo'd knob must error, not silently fall back to the default
pub struct InflationAdjust {
    pub enabled: bool, // false = raw nominal % (default); true = deflate long-horizon returns by live EU HICP
    // (#88) `enabled` is documented above and in ci-settings.yaml as a knob about COLUMNS. It is not:
    // `horizon_changes` writes into `Quote.perf`, and `Quote.perf` is what `picks::perf_pct` hands to
    // every gate and score term, so turning it on silently re-denominates the whole growth lane while
    // the backtest that fitted its thresholds stays nominal (`backtest_quote` passes `infl: None`).
    // true = the score reads a nominal copy (`Quote.perf_nominal`) and the % columns keep the real one,
    // i.e. `enabled` finally means only what it says. false = today's behaviour and the DEFAULT, because
    // the shipped lane must not move until this is deliberately turned on.
    pub score_on_nominal: bool,
}

fn default_top_picks() -> usize {
    5
}

fn default_stale_days() -> i64 {
    7
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
#[serde(default, deny_unknown_fields)] // a typo'd knob must error, not silently fall back to the default
pub struct Widths {
    pub name: usize,     // NAME column (check/screen/picks)
    // (#43) NAME width for the ETF TABLE ONLY (screen/picks). 0 = off -> the ETF table uses `name` like
    // everything else. ETF names run ~51 chars at the median against a stock table's ~15 ("Apple",
    // "NVIDIA"), so one shared width cannot serve both: at `name: 28` only 3% of ETF names fit whole.
    // Does NOT apply to `check` (one MIXED table — a per-row name width prints ragged columns) nor to
    // the crypto lane (coin names are short). Display-only, never scored.
    pub name_etf: usize,
    pub ticker: usize,   // TICKER column
    pub market: usize,   // MARKET column (check/picks)
    pub price: usize,    // PRICE(EUR) column (check/screen/picks)
    pub headline: usize, // HEADLINE column (check)
    pub score: usize,    // SCORE column (picks) — shown to 1 decimal
    // (screen) WHICH columns the screen/picks tables show + their order. Empty (default) = the canonical
    // layout (`picks::DEFAULT_COLUMNS`). List keys to pick your own set / order — e.g.
    // `columns: [rank, name, ticker, price, cagr, vol, maxdd, 1y, 5y, off-hi, score]`. All keys live in
    // `picks::COLUMNS`: rank name ticker market price cagr 1h 6h 12h 1d 1w 1m 1y 5y 10y 20y vol maxdd r2
    // abv-ma pe peg roe div ter off-hi upside turnover score. Unknown keys are ignored. Display-only — no scoring.
    pub columns: Vec<String>,
    // (screen/picks) Per-column width override, keyed by the SAME column key as `columns`. Wins over both
    // the fixed `picks::COLUMNS` width and the data-sized name/ticker/market/price/score values. Omitted /
    // missing key = the built-in width. e.g. `column_widths: { name: 34, ticker: 10 }`. Unknown keys ignored.
    pub column_widths: BTreeMap<String, usize>,
}

impl Default for Widths {
    fn default() -> Self {
        Widths { name: 26, name_etf: 0, ticker: 8, market: 11, price: 13, headline: 31, score: 5, columns: Vec::new(), column_widths: BTreeMap::new() }
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
///
/// The `dividend` term in BOTH lanes is scored NET of Portuguese tax — see `picks::dividend_reward`
/// and the `tax_keep_eu` / `tax_keep_other` knobs below. (#61) The two lanes now carry SEPARATE
/// weights, and ci-settings ships this one at `onsale_dividend_weight: 0.0`, so in practice the tax
/// model shapes the GROWTH lane alone — which is the lane `screen` ranks on and the lane it was
/// built for. The mechanism stays wired in both so the split remains a knob, not a deletion.
/// GATES exclude a candidate outright; SCORE knobs rank the survivors. Mirrors `config/settings.yaml`.
/// (G+) One additional fundamental tilt term: which `FundFactors` field, how much per point, and the
/// clamp applied before weighting. What the score adds is `weight × clamp(value, 0, cap)` — the same
/// arithmetic as the primary `growth_fund_*` term, just repeatable. An unknown `factor` name reads
/// None and contributes 0, exactly like the primary (`core::select_fund_factor`).
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)] // no `default` — a term with a missing weight or cap is a config bug, not a 0
pub struct FundTerm {
    pub factor: String,
    pub weight: f64,
    pub cap: f64,
    /// (N) what a MISSING factor scores. In a RANKING an absent datum is not neutral — at 0 it is a
    /// demotion of up to `weight × cap`, which for the shipped roic term (0.25 × 40) is TEN POINTS
    /// charged to every filer that simply doesn't report the input. Census over the 509 cached SEC
    /// filers: `op_margin` is the only missing leg (77.6%), every other leg roic needs reads 99.6-100%,
    /// so 114 names (22.4%) — `AFL AIG ALL AXP BAC BRK-B C CB COF` and also `CVX COP DE ADM BMY BIIB
    /// ARE BXP` — scored 0 against a covered-peer median of 17.3. Set this to the population's typical
    /// value and "unknown" ranks as typical instead of as worst. A KNOWN-BAD value still clamps to 0
    /// and ranks BELOW an unknown one, which is the correct ordering.
    /// Field-level default only: a term may omit `neutral` (-> 0.0 = the pre-(N) behaviour, so every
    /// recorded receipt stays numerically exact), but omitting `weight` or `cap` is still a config bug.
    /// (#59) A NON-ZERO `neutral` IS A SCORING TERM WHEREVER THE FACTOR IS UNCOVERED, not a no-op: with
    /// the factor missing for every row (any run without `fund`), this term collapses to the constant
    /// `weight × neutral` on the whole sample. That constant is NOT rank-neutral, because it lands
    /// inside `growth_score`'s additive `base`, which is then MULTIPLIED by trust × overext × proximity
    /// × value — so it reaches the score as `constant × multiplier` and ranks by the multiplier stack.
    /// (Contrast `liq_bonus`, added OUTSIDE that multiplication, where the same constant genuinely is
    /// rank-neutral and `picks.rs` correctly says so.) Measured: the shipped roic fill moves the fixture
    /// growth lane by Δ-72.2/-261.8/-56.1 edge at 12y/20y/8y while carrying no information at all — see
    /// the `null: base −4.3 (calibration)` ablation row, which reproduces those numbers exactly.
    #[serde(default)]
    pub neutral: f64,
}

/// (#108) One rung of a capital-gains HOLDING-PERIOD schedule: hold the position at least `min_years`
/// and `excluded_pct` of the gain is excluded from the taxable base BEFORE the headline rate applies.
/// Rungs are matched by "widest exclusion among those satisfied", so the order they are written in the
/// yaml does not matter and a partial ladder degrades to the rungs it does list.
///
/// NO TAX LAW IS ENCODED IN THIS CODEBASE — that is the file's standing rule, and it is why this is a
/// list of numbers a human writes down rather than a `match` on a jurisdiction. An empty schedule (the
/// DEFAULT) means one flat rate at every horizon, which is exactly what the report printed before this
/// type existed. The schedule the round-3 note proposes for a Portuguese resident, under Art. 43 CIRS
/// as amended by Lei n.º 31/2024, is `[{2, 10}, {5, 20}, {8, 30}]` — 28% becoming an effective 25.2 /
/// 22.4 / 19.6%. That reading came from secondary sources and one of the three consulted described
/// only the older SME-specific regime, so it is written HERE, in a doc comment, and NOT in any shipped
/// value: confirm it with an accountant before setting a number you will act on.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)] // a rung with a missing threshold or exclusion is a config bug, not a 0
pub struct CgtRung {
    /// Minimum holding period, in years, at which this rung's exclusion starts applying.
    pub min_years: f64,
    /// Percentage of the GAIN excluded from the taxable base at that holding period (0-100).
    pub excluded_pct: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default, deny_unknown_fields)] // a typo'd knob must error, not silently fall back to the default
pub struct BuyHeuristic {
    // --- GATES: a candidate failing ANY of these is dropped before scoring ---
    pub min_1y_pct: f64,             // [FOIL] on-sale only: reject if equity 1Y % <= this (the growth lane has its own `growth_min_1y_pct`, not this)
    pub growth_min_1y_pct: f64,      // GROWTH GATE (equities): reject if 1Y % <= this. Was a hardcoded 0.0 in both gate sites; crypto keeps its own `min_1y_pct_crypto`. MEASURED and REVERTED once already (round 5, 2026-07-03): a -10 floor took the lane edge +106.6 -> +101.1 and -20 -> +94.4, and the n=284 names -10 NEWLY admits went on to average -108.1 pts forward peer-relative — down-1Y-near-high names are falling out of favour, not resting, which is the on-sale foil's losing profile. The knob is back so the cohort can be RE-MEASURED on current data (`growth_min_1y_pct -10` in the GATE SWEEP) rather than quoted from a receipt, not because the answer changed. Edge-affecting -> read that sweep row AND re-validate `backtest universe` (both OOS halves +) before moving it. 0.0 = today's behaviour (default)
    pub min_1y_pct_crypto: f64,      // crypto/FX (-EUR/-USD): looser 1Y floor — they swing far harder. Used by BOTH lanes (the growth knob above is the equity leg only)
    pub max_1m_drop_pct: f64,        // equities: reject if 1M % <= this (a hard monthly crash = falling knife)
    pub max_1m_drop_pct_crypto: f64, // crypto/FX: looser knife — a -20%/month alt is normal, not broken
    pub min_long_pct: f64,           // [FOIL] on-sale only: reject if any 5Y/10Y/20Y leg <= this (the growth lane has its own per-rung `growth_min_{5,8,20}y_pct`, not this)
    pub min_long_pct_crypto: f64,    // [FOIL] on-sale only: reject if the >2Y leg <= this CUMULATIVE % (a corpse, e.g. -70%+)
    pub growth_min_5y_pct: f64,      // GROWTH GATE: reject if the 5Y CUMULATIVE % <= this. Was a hardcoded 0.0 in both gate sites (same constant-to-knob move as `growth_min_1y_pct` above). A leg the quote does not have (`n/a`) is SKIPPED, never read as a failed bar — missing history is not a weak return. ALL LANES including crypto: the `!crypto` guard this gate used to carry came off deliberately, so a coin now answers to the same bar. That is NOT the old behaviour and the backtest cannot warn about it (every edge metric filters crypto out) — check the live crypto lane by eye after moving it. 0.0 = the shipped default and the equity behaviour that predates the knob
    pub growth_min_5y_pct_crypto: f64, // GROWTH GATE (crypto): the 5Y CUMULATIVE floor for coins ONLY — the twin that lets the equity bar above be tuned without pricing crypto against it. MEASURED 2026-08-03: the equity ladder peaks at +75 (20y lane edge +410.8 -> +459.3, rho +0.14 -> +0.17, h2h 67% -> 76%, rank-1 med +6.0 -> +6.4, confirmed by its +50 neighbour and falling away at +100/+150), but BTC's 5Y was +51.5% and BNB's +63.1% that same day, so a shared +75 printed "Top 0 of 20 max crypto — (none pass the gates)". 0.0 = the value the shared knob shipped, i.e. crypto behaviour is UNCHANGED by that equity move. UNGRADEABLE BY THE BACKTEST — every edge metric filters crypto out (`backtest.rs:1224, 1456, 1519, 1560, 1610, 1728, 1781, 1872`), so no run can tell you what moving THIS number costs; check the live crypto table by eye instead
    pub growth_min_8y_pct: f64,      // GROWTH GATE: same, 8Y rung. -1e9 = off (default), but this is the rung that actually BITES. Measured live 2026-07-27 at `growth_max_peg: 2.0`, +50% bar: rejects AMGN (8Y +44.0%), GILD (+40.7%), SCHW (+48.2%), ULTA (+49.2%) — 4.4-5.1%/yr over 8 years from names that all clear a 16%/yr LIFE CAGR, which is exactly the case no other gate reads. HOW MUCH IT BITES DEPENDS ON `growth_max_peg`: the same bar rejected nobody at a 1.0 ceiling, where only 5 stocks reached the ranking. Re-measure after any PEG move instead of quoting this comment
    pub growth_min_20y_pct: f64,     // GROWTH GATE: same, 20Y rung. -1e9 = off (default). NOTE this bar reaches LONG-LISTED STOCKS ONLY: no UCITS ETF or coin in the universe has 20y of history, so their 20Y leg is `n/a` and skipped by construction. At +200 it also rejects nobody (2026-07-27); the case it exists for is a name whose life CAGR clears the bar on early decades while its recent 20y is ordinary — PGR compounds +8.6%/yr over its last 20y behind a 16%/yr life CAGR
    pub perf_fill_coverage_pct: f64, // DISPLAY ONLY, NEVER SCORED (`picks::perf_fill`): a BLANK perf cell prints the whole-life measured return marked `≈` when the record covers at least this % of that rung — and strictly LESS than 100% of it. 100.0 = off, and off BY CONSTRUCTION rather than by a branch: a real leg already needs ~99% coverage to exist (`core.rs` (H-cov)), so `cov >= 1.0 && cov < 1.0` can never admit a cell. The `< 1.0` half is load-bearing, not tidiness: a leg can also be blank on an OLD name (a zero past price at the anchor -> `Some(0.0) => None`), and filling THAT is exactly the fabricated-leg bug (H-cov) was added to remove. Never a projection — the value is what the record did, wearing a longer label, same convention as the `†` S-8Y mark
    pub min_avg_turnover_eur: f64,   // reject if avg daily turnover (EUR) < this (thin/illiquid name); 0 = off
    pub endpoint_smooth_days: usize, // (#17/#18) MEASUREMENT endpoint for the LONG-horizon inputs (>=1Y perf/CAGR legs, range position, drawdown) = mean of the closes in the last N TRADING DAYS (1 = raw last close). Converted to bars by the run's cadence (core::measure_endpoint) so the span means the same calendar time live (daily) and in the monthly backtest -> train == serve. Short legs (1D/1W/1M knife), the displayed price and the overext brake stay RAW by design (brake smoothing measured worse). Edge-affecting -> validate `backtest 12 universe` (both OOS halves +) before changing

    // --- SCORE — ON-SALE LANE (`buy_score`): EVERYTHING from here to "GROWTH LANE" below is [FOIL]
    //     (backtest-only; `screen` ignores it), EXCEPT the shared tilts noted inline. ---
    pub normal_volatility_pct: f64,  // a "typical" daily swing (%); the dip is scaled by normal/asset vol, so a calm name's dip counts for more than a wild one's
    pub discount_cap: f64,           // cap on that volatility-scaled dip (one very deep name can't run away with the ranking)
    pub discount_weight: f64,        // (#4) multiplier on the direct dip reward (discount×health×momentum). The walk-forward backtest found deepest-dip ranking is BACKWARDS on peer-relative selection, so default <1.0 demotes it toward the quality/trend terms; 1.0 = old behaviour, 0 = off. Does NOT touch discount_frac (the long_reward "must be pulled back" scaling stays on raw discount)
    pub momentum_bounce: f64,        // discount ×this when a pulled-back name is turning UP (green week) — reward the bounce (>1; 1.0 = ignore weekly timing)
    pub momentum_knife: f64,         // discount ×this when it's still falling (red week & day) — dock the knife (<1; 1.0 = ignore weekly timing)
    pub long_trend_weight: f64,      // reward per %/yr of the long leg's CAGR (annualized >2Y trend) — proven-compounder bonus
    pub long_trend_cap: f64,         // cap on that long-leg CAGR (%/yr) fed into the reward, in BOTH lanes (growth `trend` and the on-sale foil's `long_reward`) — a +50%/yr coin doesn't 5× a +10%/yr one. 0 = OFF (uncapped, as shipped); the readers go through `capped_trend`, which needs that guard because `min(cagr, 0.0)` would otherwise zero every positive CAGR
    pub fixed_cagr_years: u32,       // (#15) GROWTH: pin the long-CAGR window to THIS many years (e.g. 10 -> always the 10Y leg) so every name's CAGR is measured over the SAME span; 0 = off (longest available leg, today's behaviour). Short-history names fall back to their longest leg. The pin also moves `trust_factor`'s required record to the SAME window (under an 8Y view an 8Y record IS a full record, so demanding 10Y would halve every name the view exists to judge); at 0 that stays 10Y. Edge-affecting -> validate `backtest universe` before flipping
    pub growth_min_leg_years: f64,   // GROWTH: the SHORTEST rung `long_leg` may use, on a 20/8/5/2/1Y ladder. 5.0 = today's behaviour (a name with no 5y+ leg has no long CAGR, so it can't be ranked and dies on the `history` gate — 1949 of 4748 EU-buyable names, 2026-08-03). 2.0 admits the 2Y rung, giving those young names a real (if short) CAGR to be judged on. There is deliberately NO 3.0 setting: `core::HORIZONS` carries no 3Y leg and `Quote.perf` is a positional Vec aligned to it, so adding one is a re-fetch, not a config change. `trust_factor` still demands a 10Y record for equities, so a 2Y-rung name arrives at half score — a damp, not a substitute for measuring this. NOTE THE INVERTED CONVENTION: this is a FLOOR, not a switch, so 0.0 is NOT "off" the way it is for `growth_maxdd_cap` and friends — it is the LOOSEST setting (every rung allowed, same effect as 1.0). "Off" is 5.0. (#49) 1.0 admits the 1Y rung too, and pairs with `growth_trust_young`: admitting young names is only half the idea — the other half is docking them BY RECORD LENGTH, which the (#47) ladder could not do until that knob gave it a bottom. Measured separately, both halves LOST (this knob at 2.0: 20y rank-1 median +6.0 -> +4.0, h2h 67% -> 39%); the (#49) round is the first time they run TOGETHER, which is the only arrangement that answers the question. Edge-affecting -> validate `backtest universe` before flipping
    pub use_trend_cagr: bool,        // (#14) GROWTH: rank the long trend on the least-squares log-price SLOPE (endpoint-robust) instead of the two-point CAGR; false = off (today's endpoint CAGR). Edge-affecting -> validate `backtest universe` before flipping
    pub use_life_cagr: bool,         // (#3j) GROWTH: rank on the WHOLE-LIFE CAGR (`life_cagr`, listing -> today) instead of the 20/8/5Y leg — i.e. the number the `cagr` column has always printed. Fixes the age cliff by removing the window: age >=20 draws the 20Y rung, 8-19 the 8Y, so a 22y-old name is judged on 20 years and a 19y-old on 8, against the same `growth_min_cagr` floor (GOOGL: 20Y leg 16.2%/yr, whole life 23%/yr — rejected at a floor of 17 for being old enough to earn the long rung). Costs the OTHER kind of comparability: "life" is 5y for one name and 46y for another, so a young bull-only record is ranked head-to-head with a full-cycle one. `fixed_cagr_years` is the rival fix (same window for everyone). Routed through `long_cagr_from`, so it moves ALL SEVEN readers together — gate, trend, accel, sharpe, calmar, trend_health, PEG — not just the gate. No `life_cagr` (short history) falls back to the leg. false = off (today's leg). Edge-affecting -> validate `backtest universe fund 12` before flipping; the daily path truncates "life" to the fetch window
    pub health_zero_cagr: f64,       // long-leg CAGR (%/yr, negative) at which trend_health hits 0 (a decaying multi-year trend); health=1 at flat/rising
    pub sustained_decline_pct: f64,  // (B) if BOTH 1Y and 5Y % are <= this, the name is bleeding for years (value trap) -> score ×penalty
    pub sustained_decline_penalty: f64, // (B) multiplier applied when the sustained-decline condition holds (e.g. 0.4)
    pub deep_decline_pct: f64,       // (B/C) a HARSHER tier: 5Y % <= this (e.g. -70%) = a 7y+ deep bleed riding a stale old chart -> deep penalty
    pub deep_decline_penalty: f64,   // (B/C) multiplier when the deep-decline 5Y condition holds (lower than sustained_decline_penalty)
    pub min_score: f64,              // (A) drop ranked rows scoring <= this (tables stop padding to top_picks with near-zero at-the-high names)
    pub cheap_weight: f64,           // (C) reward per % the price sits below its ~200wk SMA (structural "cheap vs trend")
    pub cheap_cap: f64,              // (C) cap on that below-SMA % fed into the cheap reward
    pub dividend_weight: f64,        // (D) reward per % of trailing-1Y dividend yield (reinvested divs dominate long-run total return)
    pub onsale_dividend_weight: f64, // (#61) ON-SALE lane's OWN dividend weight — split from the growth lane for the same reason `onsale_sharpe_weight` was, and with the same shape: one shared knob was serving two lanes that want opposite values. The wide 12y ablation says the on-sale lane wants ZERO and says it loudly — zeroing the shared knob moved that lane from rho -0.14 / edge -65.0, a 90% band of [-109.8 … -17.9] entirely BELOW zero and all four eras negative, to rho +0.03 / edge +85.7, a band of [+50.6 … +126.7] entirely above it and all four eras positive. A lane that ranked BACKWARDS ranks forwards without it. The growth lane keeps `dividend_weight` and therefore keeps the whole Art. 40.º-A tax model, which is the lane that model was built for and the lane `screen` ranks on. DEFAULT 1.5 = `dividend_weight`'s default, so the default build is byte-identical to the pre-split lane and only a configured value changes behaviour; ci-settings ships 0.0. Kept as a knob rather than deleting the on-sale dividend term outright so the null stays reproducible via the `onsale_dividend_weight` ablation row
    pub dividend_cap: f64,           // (D) cap on the yield % fed into the dividend reward
    pub tax_keep_eu: f64,            // (D/PT) fraction of a dividend KEPT after Portuguese tax when the payer is an EU company — Art. 40.º-A CIRS englobates only 50% of dividends from an EU-resident company meeting the Parent-Subsidiary Directive conditions. HAND-SET from your own IRS position (bracket, englobamento yes/no, source withholding you actually eat): no tax law is encoded in this codebase, the two knobs ARE the model. 1.0 = off (DEFAULT — the lane is byte-identical to the pre-tax version). ponytail: ONE rate blends source withholding from 12.8% (France) to 35% (Finland) — that 22-pt within-EU spread is ~3× the EU-vs-US gap this term exists to capture, so a single number is a real approximation, not a rounding. Upgrade path if it bites: a per-market map keyed on `Quote.market` — PRICED (#58) AND IT DOES NOT PAY: the coarse EU-vs-other split such a map would refine ablates at Δ+0.0 edge in the walk-forward, so a finer cut of the same term cannot buy more than nothing. Build it only to make the after-tax yield a human reads more truthful, never for edge
    pub tax_keep_other: f64,         // (D/PT) same keep-fraction for EVERY other market AND for funds of any domicile: an OICVM/ETF distribution is not a Parent-Subsidiary-Directive company's *lucro*, so it draws no 50% exclusion however EU-listed the wrapper is. 1.0 = off (DEFAULT)
    pub ref_pe: f64,                 // (E) "fair" trailing P/E: value tilt = ref_pe/PE, clamped — cheap (<ref) lifts, rich (>ref) dampens; no PE = neutral
    pub quality_weight: f64,         // (F) reward per % of trailing return-on-capital — the profitability/QUALITY factor (Novy-Marx: high-ROE firms out-compound). ROE, or ROA where equity is negative or collapsed below 1/20th of assets (`core::quality_return`). Applied to BOTH lanes. 0 = off. Was BACKTEST-BLIND until the backtest loop started filling `Quote.roe` — every number measured for it before then was taken with the term at a constant 0 (see #F)
    pub quality_cap: f64,            // (F) cap the % fed into the quality reward (one 200%-ROE outlier can't dominate). NOTE it binds on ROE (30-50% typical) far more often than on ROA (5-15%), so a negative-equity filer can never reach the ceiling
    pub quality_neutral: f64,        // (#87) what an ABSENT ROE is worth, in ROE points, before weight and cap — the same field `growth_fund_extra[].neutral` already carries, for the term that predates it. `quality_reward` reads `roe.unwrap_or(0.0)` and its own doc calls that "0 = neutral", which the (N) note beside growth_fund_extra refutes in as many words: in a RANKING absent is not neutral, it is a demotion of up to weight x cap. `quote.roe` needs a CIK or an FMP key, so every non-US filer, every name past the 600-call SEC budget and every ETF and coin takes the full 0.15 x 40 = -6.0 against US peers in the same table. 0.0 = today's behaviour and the DEFAULT; the covered-peer median is the value that makes the term mean what its doc says

    // --- GROWTH LANE: a SECOND ranking (the mirror of the on-sale lane) for quality names AT/NEAR
    //     their high that are still climbing — proven compounders the on-sale score fades to ~0.
    pub growth_min_range_pct: f64,   // growth GATE (equities): must trade at/above this % of its own ~10y range (near the high); below = it's the on-sale lane's job
    pub growth_min_range_pct_crypto: f64, // growth GATE (crypto): looser range floor so more coins surface — most alts sit well below their ATH yet still out-compound; equities use the strict gate above
    pub growth_min_range_pct_8y: f64,     // growth GATE (equities): the SAME percentile bar as `growth_min_range_pct`, re-measured on the LAST 8 YEARS (`stats_8y.range_pct`). 0 = off (default). The bar above reads the ~10y fetched chart, so a name whose old much-lower closes prop up its percentile clears it while its recent 8 years read as a name in decline — PGR ranked #1 at 2Y -14.0% on exactly that gap. This bar IS what blanks the `S-8Y` column: under the 8Y pin both CAGR floors are neutralized to -inf and `as_8y_window` swaps only range/R2/maxdd/underwater, of which no gate reads R2 or underwater and an 8y maxdd can only be <= the 10y one — so range is the only swapped stat that can newly reject, and an armed gate can never disagree with a blank cell. A quote with NO `stats_8y` (under 8y of record) PASSES: its whole record already is the window and the bar above judged it. LIVE-ONLY BY CONSTRUCTION -> UNMEASURABLE: `stats_8y` is set only in fetch.rs, so it is None in every backtest run and this gate cannot fire there (same standing as the age/AUM gates)
    pub growth_min_range_pct_8y_crypto: f64, // growth GATE (crypto): same, crypto bar. 0 = off (default). Inert at 40 today — BNB and BTC both keep an S-8Y despite sitting 43-53% off their highs
    pub growth_btc_outperf_weight: f64, // crypto SCORE: tilt a coin's growth score by how its 1Y return compares to BITCOIN's (the market base) — beats BTC -> boost, lags -> mild dock. 0 = off. SCREEN/CHECK-only (backtest scores names independently, so it's backtest-blind -> validated edge untouched)
    pub growth_min_cagr: f64,        // growth GATE: CAGR (%/yr) floor — below this it's not a proven compounder, just an expensive laggard. TWO legs, same number: (1) the long-leg CAGR (20/8/5Y rung) — validated, runs everywhere; (2) (#3i) the WHOLE-LIFE CAGR (`life_cagr`) — a name with a bad first decade clears leg (1) on its good recent rung. Leg (2) only ever rejects, never admits, so the live pool is a strict SUBSET of the leg-(1) pool. CORRECTION, (#3j): (#3i) shipped calling leg (2) "SCREEN-ONLY by construction" because backtest_quote left `life_cagr` None — but that slice runs from the first bar of the full series, so the history was always there and merely unread. `backtest_quote` now fills it, leg (2) fires in the backtest too, and the 12->14 receipt's leg-(1)-only measurement is superseded by the (#3j) runs. (#73): leg (2) now reads `picks::life_leg_cagr`, i.e. `capped_cagr.or(life_cagr)`, so `life_cagr_max_years` can WINDOW it to the last min(age, N) years instead of the whole lifetime. That knob ships 0.0 (graded on a seven-arm three-horizon v2 grid and held), where the expression is the uncapped `life_cagr` this doc describes. "Only ever rejects, never admits" survives the change — leg (2) is still a filter ON TOP of leg (1) and can add nothing leg (1) refused — but WHICH subset it carves moves in both directions: a windowed leg (2) passes a name its dead first decade blocked, and rejects a has-been whose early run still flatters a long rung
    pub growth_min_cagr_crypto: f64, // growth GATE (crypto): looser CAGR floor so ALL potential growers (not just >8%/yr) surface in the crypto table, ranked vs Bitcoin; equities keep the strict floor above
    pub growth_trend_weight: f64,    // growth SCORE: reward per %/yr of the long-leg CAGR (capped at long_trend_cap when that cap is on; 0 there = uncapped, as shipped)
    pub growth_accel_weight: f64,    // growth SCORE: reward per pt the recent 1Y return outpaces the long CAGR (momentum building)
    pub growth_accel_cap: f64,       // growth SCORE: cap on that 1Y-minus-CAGR acceleration term
    pub growth_accel_beta: f64,      // (#98) how much of the long CAGR the acceleration leg subtracts: accel = clamp(1Y - beta*long_cagr, 0, growth_accel_cap). 1.0 = the original 1Y-minus-CAGR expression, bit for bit (x*1.0 is exact), and the DEFAULT. It exists because inside the unclamped band the lane's derivative in the very quantity it hunts is growth_trend_weight - beta*growth_accel_weight = 0.15 - 0.50 = -0.35 per %/yr: a 30%/yr compounder scores BELOW an identical 20%/yr one at the same 1Y return. The (#70) pin records that raising growth_trend_weight to fix it makes the graded book worse; that grid moved the WEIGHT and never the coupling. Slope turns positive below beta = growth_trend_weight/growth_accel_weight = 0.30; beta 0 is plain 1Y momentum. Lowering beta also lifts names off the clamp floor, which is part of the effect
    pub growth_min_score: f64,       // growth SCORE: hide ranked growth rows scoring <= this (padding); 0 = show all
    pub growth_min_score_etf: f64,   // growth SCORE: ETF-only display floor. ETFs structurally cap ~5.6 (accel/quality/liq/fund terms all ~0 for a diversified basket) vs stocks ~19, so the shared growth_min_score sits at ~89% of the ETF ceiling and guillotines them. Separate, lower floor = ETF lane trimmed proportional to ITS score distribution. DISPLAY-only (print_lane, not the backtest) -> edge-blind by construction. 0 = show all
    pub growth_allow_negative_scores: bool, // (#86) ONE knob for two halves of the same defect, deliberately inseparable. HALF ONE: `ranked` trims on `min_score.max(0.0)`, so every negative floor is the same floor — both `growth_min_score` and `growth_min_score_etf` ship at -100.0 in ci-settings.yaml and neither can fire, which is what the ~40-line receipt arguing 4.0 vs 5.0 for the ETF floor is describing. HALF TWO: the score is `base × damps + liq_bonus` with every damp in 0..1, so when `base` is negative a damp RAISES it toward zero — base -2 × damp 0.22 = -0.44, and the most overextended, least-trusted, longest-underwater name ranks HIGHEST among the negatives. `underwater` is the only term that can drive base below zero and it is unbounded below. Half two is currently almost invisible BECAUSE of half one, which is exactly why they cannot be separate knobs: honouring a negative floor without fixing the inversion publishes a table ordered backwards at the bottom. true = the floor is used as written AND a negative base takes no damp; false (default) = today's behaviour, byte-identical
    pub growth_overext_cap: f64,     // (1) % ABOVE the 200wk SMA at which the overextension brake maxes out
    pub growth_max_above_ma: f64,    // hard gate: reject equities more than this % above the 200wk SMA — the extreme blow-off cohort the overext brake can only floor, not remove. VALIDATED at 150 (see ci-settings.yaml); 0 = off. Crypto exempt (rides far above its SMA normally; its brake cap handles it)
    pub growth_require_lifetime_uptrend: bool, // (#25) hard gate: reject equities whose WHOLE-LIFE log-trend CAGR (quote.trend_cagr, full history) is <= 0 — moon-crash-partial-recovery names whose 5Y/10Y legs look great but never reclaimed their long-run trend. false = off (default). Crypto exempt (young coins; the range gate handles bled ones). REACHABLE BUT DOMINATED: its fields (trend_cagr, life_cagr) ARE filled in the backtest, yet `growth_gate_reachability_pin` reads it INERT for every class, because `growth_min_cagr` (8.0) rejects the same names first — a fixture built to trip exactly this gate (crash −95%, recover to just under the start price: life_cagr −0.2%/yr with a +1349% 20Y leg) dies at `cagr-life -0.2%/yr (need ≥8.0%)` and never reaches it. So it is not backtest-blind, it is redundant at any `growth_min_cagr >= 0` — which is every config shipped. Lower that floor below 0 and this knob starts biting
    pub growth_maxdd_cap: f64,       // (#26) hard gate: reject equities whose worst-ever drawdown MAGNITUDE exceeds this % (e.g. 83 rejects a name that ever fell >83% peak-to-trough). 0 = off (default)
    pub growth_maxdd_cap_crypto: f64, // (#26) crypto's OWN maxdd cap. Coins crash >90% every cycle, so the equity cap would gate Bitcoin itself (-83%); set this just ABOVE BTC's mark (84) to mean "reject coins that wiped out worse than Bitcoin". 0 = off (default)
    pub growth_max_vol_crypto: f64,  // (#36) crypto-only hard gate: reject coins whose daily-return stdev (quote.volatility_pct, the VOL column) exceeds this % — "not meaningfully wilder than Bitcoin" (BTC ~2.4%/day; 3.0 gives the base headroom). Edge-blind, but NOT for the reason this comment used to give: "crypto is absent from the backtest pool" was FALSE (fetch_universe pushes CoinGecko SYMBOL-EUR tickers and nothing filters them; coins are in fact the one class the bare `Quote::stub` always classed correctly, since `is_currency_quoted` reads the ticker suffix). The gate IS reachable on a reconstructed coin — `backtest_quote` fills volatility_pct, and `each_class_reads_its_own_gate_leg` pins that the crypto leg is the one taken. What is missing is SAMPLES: ~11y of coin history leaves 0 growth-scored crypto cutoffs at every horizon measured (20y-s 0/4633, 12y-s 0/11758, 8y-s 18/20575 with 0 scored), so no sweep can vote. Equities never reach multi-% daily stdev, so no equity twin. 0 = off (default)
    pub growth_min_age_years: f64,   // (#33) hard gate: reject a name younger than this many years (quote.age_years, the YRS column = whole-life listing age). A "20yr+ proven CAGR" candidate must actually HAVE a multi-year record. BACKTEST-BLIND (age_years is None in the backtest pool -> gate inert there, validated edge untouched); bites only the live screen. Pins bypass it (shown with a "young" gate-review reason). All classes. 0 = off (default)
    pub growth_min_aum_etf: f64,     // (AUM) ETF-only hard gate: reject a fund smaller than this (quote.aum_eur, EUR-approx from the BF universe payload). Sub-€100M funds get liquidated/merged — a forced taxable exit is exactly what a 20y hold must avoid. BACKTEST-BLIND (aum_eur None in the backtest pool -> gate inert, validated edge untouched); None-AUM names are NOT gated (missing data != small fund). 0 = off (default); ci-settings ships 100_000_000
    pub growth_max_peg: f64,         // (#37) hard gate: reject a name whose PEG exceeds this — too expensive for the growth it actually delivers. Expressed as PEG (2.0 = "reject PEG > 2") so the config reads like the column; applied internally as its reciprocal, since the measurable field is `fund.peg_yield` = 1/PEG x 100 (peg_yield 100 <=> PEG 1, so the bar is 100/growth_max_peg). This IS the printed `peg` cell — one PEG since 2026-07-27. Before that the cell computed `pe_ratio / long_cagr_from` while this gate read a `peg_yield` whose growth term was a hardcoded `trend_cagr`, i.e. two arms of the SAME three-way CAGR switch; under `use_life_cagr: true` they diverge and the tool cut APH at PEG 2.02 in the run it ranked ODFL printing 2.51. `peg_yield` now divides by `picks::long_cagr_pct` on every path (fetch.rs enrich / backtest.rs loop / report.rs mirror -> core::peg_yield) -> train==serve, and it follows use_life_cagr / use_trend_cagr / fixed_cagr_years with the rest of the lane. Its NUMERATOR is the annual 10-K EPS (the only basis the as-of path can rebuild), while the `P/E` column prefers the fresher TTM roll — so P/E ÷ CAGR is close to this but not equal, widest for mid-ramp growers. LOSS-MAKERS (eps_ttm <= 0) are also rejected: peg_yield is None there and "no earnings" is not a passing grade for a valuation ceiling. Genuinely absent fundamentals still pass, like every other data gate. STILL EQUITY-ONLY, and now deliberately so: crypto/ETFs carry no `fund`, and the other two classes each got their OWN number rather than sharing this one — funds in `growth_max_peg_etf` (see that knob for why one shared ceiling was tried, measured and abandoned), coins in `crypto_max_mvrv`, which is not even a PEG (MVRV has no earnings term). Three classes, three ceilings, three units — deliberately, because the one thing that has repeatedly gone wrong here is two different quantities sharing a name. Nothing here changes; the equity value and every receipt below price the equity lane alone. 0 = off (default). RE-MEASURE before trusting any threshold: the shipped 1.5/2.0/2.5/3.0/4.0 sweep priced the OLD definition — life CAGR runs BELOW trend CAGR, so every PEG got bigger and every ceiling got stricter
    pub growth_max_peg_etf: f64,     // (#37 funds) the ETF table's OWN valuation ceiling, same PEG units and same arithmetic as growth_max_peg, applied to a FUND's look-through equity-book P/E (`parse_fund_pe` -> `picks::fund_peg_yield`, dividing by the same `long_cagr_pct`). NOT a gate in `growth_score`: it is a post-rank trim in `picks::lane_split`, because that P/E exists only for the funds `yahoo_top_holdings` fetches (the ranked picks + a refill bench, ~50 of a ~4300-fund universe) — a universe-wide fund gate would mean thousands of crumbed quoteSummary calls. Sitting before the cut to `n`, it refills from below instead of shortening the table. Funds with no P/E are never trimmed, and pinned tickers bypass it. SWAP-BASED (synthetic) funds are the exception, since for them "no P/E" is PERMANENT rather than missing — they hold a total-return swap and Yahoo reports a literal 0.0 equity book, so they used to ride the free pass forever (XLKS.L held rank #2 untested in the run this trim cut seven funds at PEG 2.57-3.80). They now borrow the look-through P/E of a PHYSICAL fund on the same index via `screen::borrow_index_twins`, matched by NAME — never by composition, because a synthetic fund's reported basket is collateral, not exposure, and a sector fingerprint rejects the real pairs. Borrowed values act like fetched ones and print with a `~` plus their source. SEPARATE FROM THE EQUITY NUMBER BY MEASUREMENT, not by taste: the shared 1.6 was tried live 2026-08-02 and cut EVERY fund that reports the datum — all-world 3.29, S&P 500 2.57, US tech 1.78, semis 1.69 — leaving a one-row table whose sole survivor was the fund with NO P/E, i.e. selection by absent data, the exact inversion the trim exists to prevent. A diversified basket compounds ~7%/yr against a ~23 book P/E, so it simply cannot clear an equity-grade PEG bar. NOT BACKTEST-GRADEABLE at any horizon (the look-through P/E is a live snapshot with no history, so there is no as-of value to reconstruct) — judgement plus a live-run diff, like growth_min_age_years and growth_min_aum_etf. 0 = off (default)
    pub growth_require_peg: bool,    // (V) hard gate: reject a name whose FILER states no EPS in any filing, so `growth_max_peg` can never price it. Reads `fund.eps_never_reported` — NOT `peg_yield.is_none()`, which is a completely different and much larger set: it also catches loss-makers (rejected above, with their own message), negative-growth names (a real number the ceiling should not judge), every ETF and coin, and every filer with no SEC coverage at all. The cohort is 8 of 509 cached US filers, all multi-class or partnership-structured, and seven of them are FIXED upstream by the XBRL-instance fallback (fetch.rs) — what is left is the residue that tags no per-share element anywhere, ARES alone today. Their exclusion is real, not cosmetic: `growth_max_peg` cut ODFL 2.49 / ROST 2.27 / WMT 2.38 / TDY 2.00 in the same run, and a name that walks past a ceiling because its data is absent is ranked on a strictly easier test than its peers. Deliberate departure from the house "None passes" rule, and the same one `growth_max_peg` already makes for loss-makers. FILER-LEVEL, never as-of, so it reads identically in the backtest and live (see the field's doc in core.rs). false = off (default)
    pub growth_min_net_margin: f64,  // (#38) hard gate: reject a name whose as-of net margin (%) is under this — the NET% column's scored twin, `fund.net_margin`, joined point-in-time from the filings (no look-ahead) and present in BOTH the live enrich and the backtest loop, unlike the display-only `net_margin_fy`. WARNING BEFORE RAISING THIS: single-digit net margin is a BUSINESS-MODEL fact for retail, distribution and industrials, not a quality signal — a floor here cuts whole industries (EMCOR 7.5%, Casey's 4.1%) rather than bad businesses, which is why it ships only if a run says so. None (no fundamentals) passes. 0 = off (default)
    pub growth_max_margin_swing: f64, // (#39) hard gate: reject a name whose net margin SWINGS more than this many percentage points, std-dev across its as-of annual filings. Expressed as the POSITIVE std (5.0 = "reject a margin wobbling more than 5pp") because the measurable field, `fund.margin_stability`, is -std and so is always <= 0 — a knob taking negative numbers reads backwards, and the same reasoning gave growth_max_peg its PEG units. Bar is `margin_stability < -growth_max_margin_swing`. This is a CYCLE detector, not a quality one: a peak-cycle fertilizer or refiner hides the cycle behind a good current LEVEL, which is why growth_min_net_margin cannot see it and this can. NOTE the field needs >= 3 annual rows, and FMP's quarterly rows inflate the std through seasonality (the validated `fund_source sec` lane files one annual row per year) — so on an FMP-fed name this gate reads high and cuts too much. None (under 3 filings, or no fundamentals) passes, like every data gate. Equity-only for free (crypto/ETFs carry no `fund`). 0 = off (default)
    // (P1) SURVIVAL gates — four rejections read off `fund`, every one OFF by default. These factors
    // have only ever been measured as additive TILTS; as gates they answer a different question — "is
    // this filer still going to be here in twenty years" — which no scoring term in the lane expresses.
    // Equity-only for free: coins and ETFs carry no `quote.fund`. None (no fundamentals) PASSES, per the
    // house rule. All four are UNFITTED JUDGEMENT VALUES and ship off until a sweep says otherwise.
    pub growth_max_dilution_pct: f64,   // (P1a) hard gate: reject a filer whose as-of 1y SHARE COUNT grew by more than this %. Reads `-fund.buyback_yield` — that field is sign-flipped (+ = shrinking = buying back), so the dilution a holder actually suffers is its negation, and a 3% raise reads +3.0 here. Serial issuance is how a compounder's per-share result is quietly taken from the holder, and it is invisible to every margin and growth term in this lane. 0 = off (default)
    pub growth_min_interest_cover: f64, // (P1b) hard gate: reject a filer whose as-of operating income ÷ interest expense is under this ×. `fund.interest_cover` is None when NO interest expense was filed at all, so a debt-free balance sheet reads NEUTRAL and passes — deliberately: scoring "carries no debt" as "cannot service its debt" would invert the gate. 0 = off (default)
    pub growth_min_fcf_margin: f64,     // (P1c) hard gate: reject a filer whose as-of (op cash flow − capex) ÷ revenue is under this %. The off sentinel CANNOT be the house 0 here, because a bar of exactly 0 ("reject anyone burning cash") is the single setting most worth measuring — a knob whose off-state collides with its most interesting value cannot be swept. -1e9 = off (default)
    pub growth_min_net_cash_rev: f64,   // (P1d) hard gate: reject a filer whose as-of (cash − debt) ÷ revenue is under this %. Same -1e9 off sentinel for the same reason as P1c: negative values are meaningful here (a levered balance sheet), so 0 is a legitimate bar. Revenue-scaled rather than EBITDA-scaled, so loss-makers stay defined instead of None-ing out of the gate entirely. -1e9 = off (default)
    // (P4) TWO PRICE-PATH gates, both NON-CRYPTO. Coins already carry three tail knobs of their own
    // (`growth_maxdd_cap_crypto`, `max_1m_drop_pct_crypto`, `growth_max_vol_crypto`), and a coin's
    // ordinary week clears every rung either of these would ever be set to — scoping them to the
    // equity/ETF lane keeps them gates rather than a class filter wearing a threshold. Both read a
    // field `backtest_quote` fills, so unlike the (P1) fundamentals gates they are LIVE in the pool
    // for every class and CAN be swept. Missing series passes, per the house rule.
    pub growth_max_daily_1m: f64,    // (P4a) hard gate: reject a non-crypto name whose LARGEST single-bar move in the trailing month exceeded this %. Reads `quote.max_daily_1m` (core::max_daily_pct, shared by the live fetch and the backtest so train and serve cannot drift). Not a dispersion and not a drawdown: a +30% day is a squeeze, a bid or a meme, and all three are the opposite of the durable compounding this lane ranks for — the existing vol and maxdd terms average that spike away instead of seeing it. CEILING: on a MONTHLY-cadence backtest the source window collapses to one bar and the field becomes that month's own return, so a monthly sweep of this knob measures a different signal and must not be pooled with a daily one. 0 = off (default)
    pub growth_max_vol: f64,         // (P4b) hard gate: the EQUITY/ETF twin of growth_max_vol_crypto — reject a non-crypto name whose daily-return stdev (`quote.volatility_pct`) exceeds this %. The crypto knob's comment says "equities never reach multi-% daily stdev, so no equity twin"; that is true of ITS shipped 3.0 and false of the concept — high-beta single names run 3-4%/day and small-cap biotech well past that, so the equity ladder sits BELOW the crypto one, not at it. Unlike its crypto sibling this one is NOT edge-blind: the pool is overwhelmingly equities, so a sweep here has samples to vote with, which is the whole reason it is worth adding. 0 = off (default)
    pub growth_corr_cap: f64,        // (#41) REDUNDANCY skip on the shown stock table: walk the ranked rows and drop one whose 36-month trailing-return correlation with an already-kept row reaches this, refilling from below WHERE THERE IS ANYTHING TO REFILL FROM — measured live 2026-07-28 there is not: the table keeps 38/38 rows off, 37 at 0.8, 35 at 0.6 and 29 at 0.4, because `decorrelate_keep` stops at `n` and the live pool of ~38 intra-correlated US large-caps runs out of uncorrelated candidates first. The probe fills its book from a much deeper list, which is why its number does not transfer. See the (#41) receipt in ci-settings.yaml; it ships 0 = off for that reason. Not a gate and not a score term — the rows it removes passed everything; it removes the SECOND copy of a bet, which is why it runs after the score trim rather than inside score_parts. Measured in the CORR-CAP probe, which is a WITHIN-RUN sweep and so immune to the (#40) cross-run universe noise, and which shares `core::decorrelate_keep` with the live skip so probe and served rule cannot drift. TWO runs, agreeing on the plateau and disagreeing on which end of it leads: 0.8 is identical to off on both; the earlier run gave 0.6 book +13.9->+14.3, excess +5.8->+6.2, win 86->88%, OOS +4.9/+6.8 -> +5.1/+7.2 with 0.4 "the same plateau"; the 2026-07-28 run gives 0.6 book +14.3 OOS +5.8/+6.6 (late half BELOW off's +6.7) and 0.4 book +14.4, excess +6.3, worst flat, OOS +5.9/+6.8 — only 0.4 clears the probe's own ship rule there. 0.4 is therefore the value this knob WOULD take if it took one — ci-settings ships 0.0, for the live-pool reason stated above, and the two halves of this sentence used to contradict each other. Needs >=12 overlapping months to judge a pair; an unjudgeable pair never blocks. 0 = off (default)
    pub growth_value_floor_pct: f64, // (#75) VALUE BRAKE: reject the most-expensive-FOR-THEIR-GROWTH P% of a cohort by `fund.peg_yield` (high = cheap for the growth delivered), as a CROSS-SECTIONAL floor rather than an absolute number — 40.0 means "cut the dearest 40% of the names this window actually admitted". Not a `growth_score` gate and it cannot be one: a percentile needs the whole cohort and `score_parts` sees one quote at a time. It is a post-rank trim at TWO sites, `picks::lane_split` (served) and `report_vs_benchmark` (graded), both after the gates and before the cut to N, so what it drops refills from below instead of shortening the table — the (#41) `growth_corr_cap` placement exactly. Both call `core::pct_floor` so the served and measured brakes cannot drift ((#3j)). WHY A PERCENTILE AND NOT A TIGHTER `growth_max_peg`: that knob already owns the absolute ceiling, and this asks a question no absolute number can — "dearest OF THE SURVIVORS". That is the TIGHTENING direction, which `gate_sweep` structurally cannot see (it only ever loosens each gate one notch), so no sweep in this repo has ever proposed it; every PEG ladder ever run went 1.6/2.0/2.5/off. Names with NO peg_yield are KEPT (unjudgeable is not a verdict — the house rule, and the same one `drop_bottom_book` follows), which also makes it equity-only for free: ETFs and coins carry no `fund`. Pinned tickers bypass it live. MEASURABLE, and only under `backtest <h> universe fund` — `fund_lane_on` gates the as-of fill, so on a price-only or `stress` run every peg_yield is None and this knob is inert BY CONSTRUCTION rather than by measurement. 0 = off (default). MEASURED AND REFUSED 2026-08-12 (12 runs, `backtest {12,8} universe fund` x 0/25/40/55/70 plus a 20y 0/70 pair): 40 is the best arm at both graded horizons and it RISES at 12y (top-3 +6.8|+6.9 -> +6.9|+7.1) but merely HOLDS at 8y (+7.3|+7.8 on both sides, to the decimal), which fails Ship Rule v2's ADDITION bar. The PEG-VALUE-GATE probe's own printed ship line IS met — that bar grades a TOP-10 held-N-years no-sell book, and the +0.3/+0.2 it reports decays to +0.1 and 0.0 at the rebalanced TOP-3 the verdict graded AT THE TIME. (#120) MOVED THAT BASKET TO TOP-10, so this decay argument is measured against a basket the repo no longer ships and CANNOT be re-cited as-is; what survives the move is the held-no-sell vs rebalanced gap, not the basket gap. RE-GRADEABLE, not re-graded — see the (#120) receipt. 20y is PROVEN inert: control and reject-70 print identical books and h2h, since no 20y cutoff carries a peg_yield. Guards never bound (worst window identical at every arm; h2h >=50% everywhere) — but note the h2h DENOMINATOR shrinks as the brake tightens (12y 27->23 windows, 8y 35->29), because that test needs >10 names per window. (#126) THE RE-GRADE RAN 2026-08-23 ON THE PIT POOL AND IT SHIPS AT 40.0 — twelve runs, `backtest {12,8} universe fund pit` x {0,25,40,55,70} plus a 20y {0,70} pair; top-10 excess rises on BOTH moments at BOTH graded horizons (12y +2.4|+2.5 -> +2.8|+2.8, 8y +3.7|+3.4 -> +4.5|+4.5), worst window identical at every arm, h2h >=50% throughout, and 40 is an INTERIOR plateau peak with decay on both sides rather than a grid-edge argmax. The 2026-08-12 refusal above is kept as written because it records what was true under the top-3 basket. 20y stayed provably inert on the new pool too (control and reject-70 byte-identical), so that half of PRIMARY was discharged VACUOUSLY and the round substituted 12y for it — stricter, and stated as a deviation in the receipt. Full receipt, caveats included (n_eff 1.4 at 12y / 2.6 at 8y; h2h denominators of 2-9 windows; the 8y median still climbing at the 70 rung), at `growth_value_floor_pct` in tests/ci-settings.yaml
    pub growth_commodity_damp: f64,  // (#44) multiply the growth score of a COMMODITY-LINKED row — GICS Energy/Materials, or a fund whose name carries a commodity token (`picks::is_commodity`). Its CAGR tracks a mean-reverting input price, so the long record is a spot-price snapshot, not compounding: CF Industries ranked FIRST at +20%/yr on a −62% maxdd and R² 0.76, MPC eighth on −81% and 0.68. Rendered as `c` in the rank cell whether or not this knob is set. 1.0 = OFF (default, byte-identical to the pre-(#44) lane); ci-settings ships 0.8. NO LONGER BACKTEST-BLIND — this comment used to read "*by construction*: the pool carries no sector and no real fund names, so the damp is ×1.0 there … this knob can never be swept". Both premises died with the ETF-classification fix: `stamp_asset_class` now stamps sector AND the real fund name onto every backtest quote, so `picks::is_commodity` sees both legs and the damp fires on every Energy/Materials row and every commodity-named fund. `growth_gate_reachability_pin` pins it LIVE for stocks and ETFs (INERT only for crypto, which has neither sector nor fund name). CONSEQUENCE: the knob woke up in the SAME commit that re-grouped the de-mean, so that commit's receipt attributed to the peer-mean split a move this damp partly caused. A control run at 1.0 (all else at ci-settings) separates them — the damp is worth edge +7.3 / +6.9 / +0.9 at 20/12/8y-s, positive at every horizon, which is the first measurement this knob has ever had. Still set by judgement (0.8 is untuned — the sweep is now POSSIBLE but has not been run); note the ci-settings receipt records that the nearest measurable cousin (an R²-steadiness damp) measured edge-NEGATIVE
    pub growth_fx_damp: f64,         // (#45) multiply the growth score of an ETF whose LIVE quote currency is not EUR (`quote_currency`, straight from Yahoo — "GBp"/"GBP"/"USD"/"SEK"/"CHF" lines all count). Prices what the ranking is otherwise blind to for a EUR investor: broker FX conversion (~0.25% each way) plus the off-home-venue spread — a cost the EUR line of the SAME multi-listed fund does not carry, so on a near-tie this prefers the EUR twin. Currency, not country: a EUR-quoted line on a non-eurozone venue costs no FX and is NOT docked. ETF-only (`picks::asset_class` 1) — the stock lane is all-USD by nature so a uniform dock would reorder nothing, and crypto's -EUR legs already quote EUR. Rendered as `x` in the rank cell whether or not this knob is set. 1.0 = OFF (default, byte-identical); a configured 0.0 is ALSO off, same guard as growth_commodity_damp. ci-settings ships 0.98 — tie-break strength, ~0.5–0.8% round trip amortized over a long hold, deliberately too small to bury a genuinely stronger foreign listing. BACKTEST-BLIND *by construction*: `quote_currency` is filled only at the live fetch (fetch.rs), None on every backtest quote -> ×1.0 there — validated edge untouched, knob can never be swept. Set by judgement, (#44) is the precedent
    pub growth_ter_drag: bool,       // (#34) ETF cost drag: dock the growth score by the ACTUAL 20-year wealth multiple the expense ratio eats, (1-TER)^20. TER is the one cost certain to compound against a decades hold, so two near-identical index ETFs (e.g. 2× Nasdaq-100) rank by NET return, not by momentum noise. ETF-ONLY (expense_ratio is None for stocks/crypto -> ×1.0). BACKTEST-BLIND (expense_ratio None in the backtest pool -> ×1.0 there, validated edge byte-identical); shapes only the live ETF lane. false = off (default, byte-identical to the pre-(T) lane)
    pub growth_trust_ladder: bool,   // (#47) replace `trust_factor`'s single 10Y cliff with a graded record-length ladder: 20Y leg -> 1.00, 8Y -> 0.85, 5Y -> 0.70, none -> 0.50. THE CLIFF'S DEFECT: it cannot tell a 46-year record from a 10-year one (both 1.0) while a 9.9-year one takes the full 0.5 — all of its resolution is spent on one point of a continuum. Keyed on WHICH PERF LEG EXISTS, never on `quote.age_years`: the YRS column is None in the backtest pool (live-only, same standing as the commodity/FX damps) so an age-keyed ladder could never be graded, while the legs are on both paths and carry the same fact. ONE SHARED LADDER, crypto included, by deliberate choice — NOT a uniform crypto dock: BTC (2010 listing, 8Y leg but no 20Y) lands at 0.85 and a 5-year alt at 0.70, so the crypto lane REORDERS toward older coins. Intended, and unmeasurable by construction (crypto is filtered out of every edge metric) -> eyeball the live crypto table, no run can vouch for it. Tiers hardcoded: a knob per rung is four more numbers needing four more receipts. false = off (DEFAULT, byte-identical to the pre-(#47) cliff); edge-affecting -> validate `backtest universe` before flipping. MEASURED AND NOT SHIPPED (2026-08-03, arm D of the 27-run `{20,12,8} universe stress` grid): the cliff's defect is real but grading it LOSES rank-1 at every horizon — 20y king rank-1 median +4.6 vs +6.4 baseline, 12y -0.3, 8y -1.7, lane edge +407.3 vs +452.8. The extra resolution is spent on a distinction the forward returns do not reward: the pool's 20Y-leg names are the survivors of the oldest cohort, so promoting them to 1.00 and docking the 8Y cohort to 0.85 tilts the argmax toward maturity rather than toward compounding. Kept off, not deleted, so the null stays reproducible via the `growth_trust_ladder->on` ablation row; full grid in the (#47) receipt at growth_accel_weight in ci-settings.yaml
    pub growth_trust_young: f64,     // (#49) the BOTTOM two rungs of that ladder, and the only tier that is a knob instead of a constant. The (#47) ladder stopped at `none -> 0.50`, so it did NOT dock a 2-year record any harder than a 7-year one — both took the old cliff's flat half-score, which is exactly the distinction the young rungs need to make. Here the 2Y rung takes this value and the 1Y rung takes HALF of it (half the record, half the trust): one knob, two rungs, so the "no knob per rung" bargain above still holds and only the number that decides the outcome gets swept. 0.5 = DEFAULT and byte-identical to (#47)'s bottom rung, which keeps arm D's measurement valid. DEAD WEIGHT unless BOTH `growth_trust_ladder` is on AND `growth_min_leg_years` admits a rung below 5 — at the shipped floor `long_leg_fixed` returns None for those names and `score_parts` bails on the `history` gate BEFORE trust is computed, so no 2Y/1Y-rung name reaches this. Its curve printing a flat line at the shipped tuning is the expected reading, not a bug. NOT the only thing docking a 1Y name: that name's `long_cagr == return_1y`, so `accel` (weight 0.65, the heaviest term) is structurally ZERO for it — see `long_leg`. Edge-affecting when live -> validate `backtest universe` before flipping
    pub growth_overext_floor: f64,   // (1) growth-score multiplier at that cap (e.g. 0.4 = a fully-stretched name keeps 40% of its score); 1.0 = brake off
    pub growth_turnover_weight: f64, // (L) liquidity tilt: bonus per ln(turnover/€1B), added OUTSIDE the brake. Rewards deep-liquid mega-caps (easy multi-decade exit, less manipulation) so a proven compounder like NVDA isn't ranked below an illiquid €200M twin on a score tie. RANK-NEUTRAL in backtest: backtest_quote sets a uniform sentinel turnover (#20) so this bonus is a constant offset on every name -> never moves the validated edge; 0 = off
    pub growth_overext_cap_crypto: f64, // (#4) crypto's OWN overextension cap (% above the 200wk SMA at which the brake maxes). Crypto routinely rides far above its long SMA, so a separate looser cap avoids over-braking coins; equities/ETFs keep growth_overext_cap. 0 = crypto brake off
    pub growth_fund_weight: f64,     // (G) reward per pt of the as-of FUNDAMENTAL factor (see growth_score / fund_factor). The fund lane proves WHICH as-of factor predicts forward returns standalone; this folds it INTO growth_score so its through-the-lane edge is ablatable. 0 = off (DEFAULT, no behavior change). Validate via `backtest <set> fund` then set the weight only on +ablation-Δ + both-half-positive OOS
    pub growth_fund_cap: f64,        // (G) cap (in the factor's own pts) on the fund factor fed into that reward, so one data-artifact (+9000% rev) can't dominate the rank
    pub growth_fund_neutral: f64,    // (#87) what an ABSENT `fund_factor` is worth, in the factor's own pts, before weight and cap. Third of the three fundamental terms to get the field, so all three finally answer "no data" the same way instead of three ways. 0.0 = today's behaviour and the DEFAULT
    pub growth_fund_scope_by_class: bool, // (#87) the OTHER half of one missing-data policy, and the half a neutral fill alone gets backwards. A neutral is a COVERED-PEER MEDIAN, which is a sensible stand-in for a filer whose data we merely failed to fetch and is meaningless for an asset that has no income statement at all. ci-settings ships `growth_fund_extra: roic, weight 0.25, neutral 17.3`, so every ETF, every coin and every non-SEC stock collects 0.25 x 17.3 = 4.325 points for having no fundamentals — against a `trend_term` whose entire discriminating span above the `growth_min_cagr` floor is 1.65 points, and against a MEASURED filer with a real 5% ROIC who scores 1.25. Coverage outranks merit by ~3 points in the term that is supposed to measure merit. true = crypto and ETFs take 0 from all three fundamental terms (they have nothing to be missing), while a stock that could have been covered still takes the neutral; false (default) = today's behaviour, byte-identical
    pub growth_fund_factor: String,  // (G) WHICH as-of FundFactors term the fund tilt weighs: rev_cagr | rev_accel | gross_margin | op_margin | margin_trend | eps_growth | rev_yoy | eps_yoy | net_margin. Set to whichever the `backtest <set> fund` probe shows +rho + both-half-positive OOS — no recompile. Unknown name -> neutral. Default "rev_accel" preserves the prior hardcoded behavior
    // (G+) ADDITIONAL fundamental tilt terms, summed on top of the single `growth_fund_*` term above.
    // The three knobs above stay the PRIMARY term because the weight sweep, the weight curve and every
    // recorded receipt in tests/ci-settings.yaml address them by name; this list is purely additive.
    // EMPTY BY DEFAULT, which makes the whole mechanism inert and every prior receipt still exact.
    // Each term carries its OWN cap on purpose: the factors are on wildly different scales (peg_yield
    // ~0-500, net_margin ~0-60, eps_yoy unbounded), and receipt (#3) records what one shared cap did
    // to a mismatched factor — +151.9 vs +195.1, where "the +43.2 IS the clamp releasing".
    // Same validation bar as the primary: a term earns a non-zero weight from a probe row showing
    // +rho with both OOS halves positive, never from looking reasonable.
    pub growth_fund_extra: Vec<FundTerm>,
    pub fund_source: String,         // (Item 22) WHICH fundamentals feed BOTH the `backtest <set> fund` lane AND the live `screen`/`check` fund tilt: "fmp" (DEFAULT, unchanged — global coverage, quarterly, 250-call/day cap, needs FMP_API_KEY) | "sec" (SEC EDGAR XBRL — free, no key, no daily cap, ~19y annual history, US filers only). The SAME source feeds backtest + live (one router) so the validated and served signal can't drift (train-serve skew). Switching to "sec" is a DATA-SOURCE change: re-run `backtest <set> fund` to re-validate the factor on SEC's annual rows BEFORE raising growth_fund_weight — the FMP-validated weight does NOT carry over. Unknown value -> "fmp". With weight 0 (default) this is inert either way.
    pub growth_mom121_weight: f64,   // (M) reward per pt of 12-1 momentum (trailing 1Y return EX the last 1mo — Jegadeesh-Titman, skips the short-term-reversal month). Price-only, so unlike the BACKTEST-BLIND div/ROE/fund tilts this one IS validated end-to-end (backtest_quote reconstructs 1Y/1M). 0 = off (DEFAULT, no behavior change). Raise only on +ablation-Δ + both-half-positive OOS via `backtest <set>` / `tune`
    pub growth_mom121_cap: f64,      // (M) cap (in pct pts) on the 12-1 momentum fed into that reward, so one moonshot can't dominate the rank
    pub growth_smoothness_weight: f64, // (E) additive reward per unit of trend_r2 (R² of the log-price trend fit, 0..1; 0 also = no history) — pays names whose long climb is a straight line over equal-CAGR rollercoaster. Price-only and reconstructed in backtest_quote -> validated end-to-end: 2/5/10/20 sweep peaked at 5 (Δedge +13.2 same-batch, rho intact; 20 collapsed). 0 = off (DEFAULT); ci-settings ALSO ships 0.0 — (#62) zeroed this term on 2026-08-10 (the sweep above is 2/5/10/20 and never tested zero), so default and shipped now agree and this knob is off everywhere
    pub growth_underwater_weight: f64, // additive PENALTY per year of the longest below-prior-peak stretch (core::longest_underwater_yrs — the drawdown-DURATION twin of the maxdd depth cap). Price-only, reconstructed in backtest_quote on the daily cadence -> validated end-to-end. Standalone probe (backtest universe fund, 2026-07-19): n=8571 rho +0.26 edge +27.9 OOS +0.40|+0.14 both +; candidate lane run at 0.3 same-batch: edge +25.5->+27.5, rho intact, both OOS halves + worst era improved. 0 = off (DEFAULT); ci-settings ships the validated 0.3
    pub growth_value_weight: f64,    // (Item 20) authority of the BACKTEST-BLIND P/E multiplier (value_factor) in the GROWTH lane only: 1.0 = full ×0.5..1.5 swing (DEFAULT, unchanged), 0.0 = neutral (off). The validated edge was measured with this term OFF (pe_ratio is None in backtest), so it's a ±50% reorder the OOS split never saw — dial it down/off once the validated additive earnings_yield term (Item 19) carries valuation instead. Defaulted so an older settings.yaml is unchanged.
    pub growth_proximity_weight: f64, // (#48) authority of the PROXIMITY multiplier — the growth score's `range_pct/100` term, which docks a name in proportion to how far it sits below its own 10y high. Blend toward neutral: `1 + w*(range_pct/100 - 1)`, so 1.0 = today's raw multiply (DEFAULT, byte-identical), 0.0 = term off (×1.0 for everyone), 2.0 = twice the slope, and a NEGATIVE w INVERTS it — the name furthest below its high scores highest. Same shape and same reason as `growth_value_weight`: an authority dial on a term that already ships ON, which is why neutral here is 1.0 and not the house 0. WHY IT EXISTS: this was the last backtest-reachable term in the lane with no knob, so it appeared in no ablation table and no curve ever run — invisible, yet worth up to 20% of the whole score (the range gate admits 80-100%). Not a small omission: `growth_geomean_fold`'s receipt records that merely SOFTENING this term (4th-rooting it inside the geomean) cost edge +103.2 -> +79.2, "the raw proximity multiply carries real selection weight". Nobody had asked what sharpening or inverting it does. The formula is clamped at 0.0 because `combine_damps` is `product().powf(1/n)` and a negative factor under a fractional exponent is NaN — unreachable inside the shipped gate, but the knob and the gate are both config input, so the guard sits at the boundary rather than in a comment. Computed at proximity's single source, so it covers the geomean-fold branch too. MEASURED 2026-08-05 (8-rung curve at 20/12/8y-stress + arms at −1.0 and 0.0), 1.0 STANDS, and the finding is that this knob has ZERO RANK-1 AUTHORITY: at the 20y king the served top-10 AND top-50 books are IDENTICAL across w = −1.0 / 0.0 / 1.0 (+12.6%/yr, excess +5.4, win 97% of 39, worst −0.5, OOS +5.1/+5.7, 4 zeros/481 holds), as is rank-1 itself (+6.4 median, win 87%, h2h 76% = 16/21 in all three arms). It is NOT inert — ranks 2-50 visibly reorder — it simply never reaches the argmax, the same trap `growth_overext_floor` and `growth_max_peg` record. On lane edge it is horizon-CONFLICTED and inside the noise either way: turning it off costs −48.2 at 20y, PAYS +23.8 at 12y, −1.2 at 8y, while the whole −1.0..3.0 ladder spans ~50 edge against a 20y bootstrap band 472 wide. Inverting it (w<0, "a name further below its high should rank HIGHER") is uniformly slightly WORSE than simply neutralizing it at every horizon (−39.1 / +22.5 / −2.1), so even conditional on wanting to stop paying for proximity, paying backwards is not the way. Edge-affecting -> validate `backtest universe` before moving it; full grid in the (#48) receipt at growth_proximity_weight in ci-settings.yaml
    pub growth_geomean_fold: bool,   // (#8) PROBE switch: fold proximity + value INTO the geomean damp instead of multiplying them raw onto base. Today score = base × proximity × value × geomean(trust, overext); THREE soft multipliers stack unbounded (a name at 0.7 × 0.8 × 0.85 keeps only 0.48 of base — nearly halved by three "slightly-off" signals). true = base × geomean(trust, overext, proximity, value): the SOFTEST term bounds the penalty (combine_damps), so no single soft signal dominates. Edge-affecting — it reshapes the live rank AND changes the geomean SLOT COUNT (the trust/overext exponent shifts from ½ to ¼), so DEFAULT false = unchanged; flip ONLY behind a green `backtest universe` with both OOS halves positive. Golden-rule-gated.
    pub life_cagr_max_years: f64,    // (#73) REPOINTED — this is now the window on `growth_min_cagr`'s WHOLE-LIFE REJECT BAR (leg 2), not on the rank. >0 makes that bar read the endpoint CAGR over the last min(age, this) years instead of the uncapped lifetime, via `picks::life_leg_cagr`, the one remaining reader of `Quote.capped_cagr`. Leg 1 (the 20/8/5Y rung) and the whole ranking are UNTOUCHED at every value. Two-sided: it admits a compounder blocked by a decade it no longer resembles, and rejects a has-been whose early run still flatters a 20Y rung. Young names are unaffected — `core::capped_life_cagr` clamps to min(age, N) and returns None under 5y of history, where the bar falls back to the uncapped lifetime it always used. 0 = off (today's behaviour, byte-identical). Edge-affecting -> graded on the stress grid before shipping any value here.
                                     // (#3l) SUPERSEDED, kept because the receipt must stay readable: this knob used to swap false-mode's RANK window (the rung ladder -> a continuous min(age, N) CAGR through `long_cagr_from`), and the old doc here promised "this knob moves the rank window, not the proven-compounder bar". Both arms LOST (-66 edge), so it shipped 0.0 and that branch was never taken; (#73) deleted the branch and gave the knob the job the (#3i) bar actually needed. Full history in the (#3l) and (#73) blocks of tests/ci-settings.yaml
    pub splice_max_weekly_rate: f64, // (splice) redenomination-splice trimmer: drop history BEFORE the last step whose implied WEEKLY growth factor exceeds this (or its 1/x mirror) — a vendor series gluing an MXN head onto a GBP tail (×19.6 in one step) otherwise feeds life_cagr/MAXDD/R² a fiction spanning the whole record (0A08.L printed +62%/yr life CAGR vs its true +6.7). Weekly RATE, not raw ratio, so a wide real gap (NVR ×26/92d = 1.28×/wk) survives; sub-week gaps clamp to 7d so one −10% day can't extrapolate to a trip. Applied at parse_chart (one site -> live + backtest see the same data, no train-serve skew) + the merged-series seam; crypto exempt (SHIB did a real 13×/wk). 0.0 = off (DEFAULT, no behavior change); ci-settings ships the measured 2.0. BACKTEST-BLIND in the receipt sense: it changes the INPUT universe, so before/after edges are two different measurements, not an A/B.
    pub print_acquisition_hazard: bool, // (#112) add one named hazard to the backtest's caveat block: the most common way a twenty-year single-stock hold actually ENDS is neither bankruptcy nor a sell decision, it is being ACQUIRED. Compounding stops at a one-time premium, and the deal crystallises the gain — paying, on a date the holder did not choose, the tax that the whole after-tax footer above it assumes stays deferred. Nothing in the walk models a takeout: every multi-year hold in the tables is a hold that was ALLOWED to run, so the reported edge is optimistic in the same direction survivorship already is, by an amount nobody here has measured. It argues mildly toward LARGER names (harder to swallow), which is the same reasoning `growth_min_aum_etf` already accepts on the fund side. Prose only — no gate, no score term, no journal field; a hazard that cannot be quantified from this data should be NAMED, not silently priced at zero. false = the caveat block every golden holds, byte-identical, and the DEFAULT
    pub print_currency_mix: bool, // (#112) print the book's CURRENCY exposure under the ranking, looking funds through to their holdings instead of reading their listing. WHY: a EUR investor holding a US-heavy book for twenty years carries a large unhedged FX position, and no line in the tool displays it. The closest thing is `print_lane`'s `market mix`, which counts LISTING COUNTRY over the stocks lane only — and for a fund that is not merely incomplete, it is backwards: a Xetra-listed S&P 500 tracker reads "Germany" and is 100% dollar exposure. Look-through uses the top-10 holdings already fetched for `fund_pe`, so the line costs zero extra requests; it also covers only that top-10, which is why the line prints its own coverage and calls itself directional rather than a hedge ratio. Display only — not scored, not gated, deliberately: whether currency concentration should tilt the rank is a measured question, and a book nobody can see is one nobody can measure. false = today's footer set, byte-identical, and the DEFAULT
    pub print_base_rates: bool, // (#111) print, per lane, the OWN-POOL base rate behind the whole thesis: of the samples whose trailing leg had already cleared `growth_min_cagr`, what fraction cleared it again over the forward window — beside the fraction of the ENTIRE pool that cleared it. The conditional number alone is a rhetorical device; the pair is a finding, because 30% is bleak against an unconditional 34% and strong against an unconditional 12%. The lane's whole thesis is that a long record predicts the next one — `growth_min_cagr` does not tilt on it, it DELETES everything below it, so the bar IS the universe — and nothing has ever asked what that bar's hit rate actually is. One pass over samples already in memory. The bar is the shipped gate rather than a new threshold: a private one here would answer a question nobody is asking. Display only — no score, no gate, no journal field. false = today's output, byte-identical, and the DEFAULT
    pub implied_growth_years: u32, // (#111) hold horizon, in years, for the REVERSE-DCF line printed under the ranking: the EPS growth rate today's multiple already pays for, assuming the P/E reverts to `ref_pe` over that horizon and the buyer requires `implied_growth_required_pct` %/yr. WHY: the most reliable human defence against buying hype is not a gate, it is being shown the claim you are implicitly making — and the tell is never the growth a company has posted, it is the growth the PRICE requires. Printing "delivered 61%/yr, price implies 28%/yr for a decade" turns a ranking into a falsifiable prediction, next to the base rate for actually sustaining it (`print_base_rates`). Dividends are ignored, which makes the printed number CONSERVATIVE for a payer and keeps the arithmetic checkable in a reader's head. No P/E (funds, coins, loss-makers) prints no line — there is no implied growth rate for a thing with no earnings. 0 = off, no line, and the DEFAULT
    pub implied_growth_required_pct: f64, // (#111) the %/yr TOTAL PRICE RETURN the reverse-DCF line assumes its reader demands. Not a gate, not a score input, and not fitted to anything: it is the reader's own hurdle rate, and the printed implied growth moves one-for-one with it, which is why the line prints the assumption alongside the answer. 8.0 is a plain long-run equity hurdle and an UNFITTED judgement value; it is inert while `implied_growth_years` is 0
    pub entry_excess_yield_band: f64, // (#110) symmetric threshold, in percentage points, that splits the LEVEL entry state into CHEAP / NEUTRAL / RICH — where the level is `index earnings yield − Euribor 3M`, i.e. Shiller's excess CAPE yield in one subtraction, computed on the CORE anchor's look-through P/E and the rate the macro footer already prints. WHY: `entry_state_class` keys the deploy multiplier on distance from the S&P high, and that receipt (+9.1 / +6.0 / +5.9 pts/yr) is real — but distance-from-high is a PATH signal, and it is right for the wrong reason at both ends. After a decade-long grind up a market can be at a nosebleed valuation AND at its high, where the rule says deploy slowly; after a fast crash from a cheap base it says deploy fast. Starting valuation is the LEVEL signal with the strongest long-horizon evidence, and both legs are already on disk. This knob PRINTS the second axis and names the 2×2 corner; it does NOT touch `deploy_multiplier`, because which axis prices the deploy schedule better is a measured question and printing it is how the data to answer it gets collected. Trailing look-through P/E, not cyclically adjusted — a real difference, which is why the line names its inputs and not just its verdict. 0 = off, no line, and the DEFAULT
    pub growth_acc_drag: bool, // (#109) dock a DISTRIBUTING share class by the twenty-year cost of paying tax on every payout instead of deferring to one sale: `(1 − yield × payout-tax)^20`, the same shape and the same exponent as `growth_ter_drag`, and plausibly a LARGER drag than any TER difference that term models. The payout tax comes from `picks::tax_keep` — the same keep-fraction the dividend reward nets at, not a second rate that could drift; note that means the code default `tax_keep_other: 1.0` (no haircut) leaves this INERT even when switched on, which is the correct coupling: a term docking a payout the dividend reward simultaneously scores as untaxed would be two answers to one question. WHY IT SHIPS OFF, and this is the whole subtlety: the Acc twin ALREADY wins today, but by accident — `use_adjusted_close: false` makes the ranking's CAGR price-only, so a Dist fund's payouts leave the NAV and reach the score as a depressed price series. Turning this on while that is still true DOUBLE-COUNTS the drag. It becomes the correct mechanism exactly when the accident is removed (`use_adjusted_close: true` plus a total-return benchmark, round 2 C2), which is why the two must move in one change. `use_of_profits` is None for stocks, crypto and every backtest quote, so this is backtest-blind like the TER and commodity docks and cannot be swept. false = today's lane, byte-identical, and the DEFAULT
    pub capital_gains_tax_pct: f64, // (#108) the headline capital-gains rate the backtest's after-tax footer prices both arms at, in percent. Was the hardcoded `CAPITAL_GAINS_TAX = 0.28` with a comment saying "add a knob only if a second rate is ever needed" — `cgt_hold_schedule` below is that second rate, so this is now the knob it asked for. NOT a scoring input and NOT a gate: it reaches exactly one printed line in `report_lane` and nothing reads it back. 28.0 = today's constant, byte-identical, and the DEFAULT
    pub cgt_hold_schedule: Vec<CgtRung>, // (#108) holding-period exclusions applied to the GAIN before `capital_gains_tax_pct`, longest-hold rung wins. WHY IT MATTERS HERE: the after-tax footer compares never-sell against yearly rotation, and it currently taxes BOTH arms at the same flat rate. If a jurisdiction's rate falls with the holding period, that comparison understates the deferral edge — and the deferral edge is the single number in this whole report that most supports the tool's own buy-and-hold thesis, so understating it is the one direction of error that flatters nothing. The rotation arm always pays the <2y rate (it holds for a year by definition), so the schedule only ever moves the never-sell leg. See `CgtRung` for the schedule this was written for and for why no jurisdiction's numbers ship here. Empty = one flat rate at every horizon, today's output, and the DEFAULT
    pub rank_perturb_k: usize, // (#107) how many perturbed knob vectors `picks::rank_robustness` scores the universe under, to print a MEDIAN RANK and an IQR beside today's point-estimate ranking. Every weight in `picks::WEIGHT_DIMS` is scaled by an independent U(0.8, 1.2) per draw; gates and caps are untouched, so the ranked population is identical in every draw and a wide IQR is always about ORDER, never about a name leaving the pool. WHY: McLean & Pontiff put a published predictor's out-of-sample decay at 26%, and 58% post-publication. Walk-forward, OOS halves and ship rule v2 are the defence, and they still only ever produce ONE knob vector — under which every name's rank is a point estimate carrying no error bar. A name at #2 under the shipped knobs and #40 under a 10% nudge is a knob artefact; a name in the top ten under three quarters of plausible configs is not. Display only — this does NOT reorder the printed book, which would be a ship-rule change and needs a measured run, not a print knob. Cost is k full re-scores of the universe, so it is off by default rather than cheap-by-default. 0 = off, today's output byte-identical, and the DEFAULT
    pub print_book_deciles: bool, // (#106) print the DISTRIBUTION of the held book's terminal multiple across windows — d10 / median / d90, plus P(book < index) and P(book < 1.0) — beside the top-N ladder that currently quotes only means. `VERDICT_TOP: 3` is the argmax of one of those means over 13 rungs, and under positive skew small N maximises the MEAN while wrecking the median and the left tail: that is the arithmetic of skew, not an empirical claim, so selecting a basket size by mean excess selects the most concentrated book almost by construction. P(book < 1.0) is the number a 20-year holder actually feels — not "did I trail the index" but "did I end with less than I started" — and nothing in this report has ever printed it. Display only — no score, no gate, no journal field. false = today's output, byte-identical, and the DEFAULT
    pub growth_er_weight: f64, // (#105) authority of the EXPECTED-RETURN tilt — the Grinold-Kroner composition `dividend_yield + clamp(eps_growth) + buyback_yield + re-rating drag`, in %/yr over the stated 20-year hold. Every other input in this lane is TRAILING; the only number in the whole score that references the horizon it claims to pick for is the exponent in `ter_damp`. This is the one candidate term whose measurement horizon matches the hold. Additive, inside the multiplier block, NOT floored at 0 — a negative expected return is information and docking for it is the one thing no other term here can do. A name with no as-of fundamental row contributes 0 (absence is not punished), which carries the same (#59) coverage caveat as every other fundamental tilt. 0.0 = off, the term contributes exactly 0, and the DEFAULT
    pub growth_er_cap: f64, // (#105) per-leg clamp on the expected-return composition, in %/yr: income and growth clamp to [0, cap], dilution to [-cap, cap]. The re-rating leg is NOT clamped here — it routes through `value_factor`, whose [0.5, 1.5] band already bounds it to about ±3.4%/yr over a 20-year hold. Exists so one absurd filed number (a 400% EPS "growth" off a near-zero base, a share count restated through a merger) cannot dominate a sum whose whole claim is that its legs are economically commensurate. UNFITTED: 20.0 is a judgement value that never fires while `growth_er_weight` is 0
    pub print_recall_capture: bool, // (#104) print, beside every lane's rho and edge, how much of the pool's EXTREME upside the ranking actually held. Every metric this report prints is a precision metric — rho, edge, top-N excess, rank-1 h2h all answer "of what I picked, how much was good?" — and under the skew a 20-year hold lives in, terminal wealth is decided by the opposite question. `recall_capture` measures it against the FULL pool, gate-rejected rows included, which is the half `gate_audit` structurally cannot see: it prices a gate by the MEAN of what it rejected, and a mean is blind to the one 30-bagger among 300 losers. Display only — no score, no gate, no journal field reads it. false = today's output, byte-identical, and the DEFAULT
    pub journal_core_list: bool, // (#103) journal the CORE shortlist alongside the ranked slice, so `track` can one day grade the buy-and-hold half of the report on out-of-sample prices. `screen` already computes `core_now` (breadth -> domicile -> TER -> AUM, capped at top_picks) and PRINTS it, but only `ranked_now` reaches `.screen_snapshots.jsonl` — so the momentum book accrues a live record with every run while the one-fund-forever recommendation accrues nothing, forever. Nothing can grade what was never written down, which makes the RECORDING the urgent half and the grading the patient one: this knob only writes the field. `Snapshot.core` is `#[serde(default, skip_serializing_if)]`, so OFF emits a byte-identical journal line and ON stays readable by every older build. false = today's journal, and the DEFAULT
    pub hold_max_ter: f64, // (#102) the CORE admission TER ceiling, in percent, as a named number instead of a literal buried in `core::hold_miss_reason`. 0.25 was chosen so FTSE All-World (VWCE/VWRL, 0.22%) clears it; the printed reject reason formats the cap from this field, so the message cannot drift from the test. Note the SECOND half of this finding is NOT fixed here: this leg reads `ter_shown()` (BF ∨ Yahoo fallback) while the score's TER damp reads `expense_ratio` (BF only), so the H flag and the score can disagree about a fund's cost on the same row. Unifying them moves a scoring input and is a measured change, not a rename. 0.25 = today's literal, byte-identical, and the DEFAULT
    pub hold_ucits_or_domicile: bool, // (#102) accept an EU-domiciled fund as UCITS even when the NAME omits the token. `hold_miss_reason` requires the literal "ucits" in the fund name, and real cores do not always carry it — "ISHARES III PLC ISHRS CORE MSCI" is an Irish UCITS fund whose feed name has no such token, so it is rejected for a naming accident rather than a fact. true = the leg passes on the name token OR a `domicile` prefix outside `core::NON_EU`, the same blocklist `firds_etf_isins` already screens the FIRDS dumps with (one list, two readers). Names WITH the token are unaffected, and `domicile: None` (watchlist rows) still falls back to the name — missing data cannot newly pass a gate it used to fail. false = name token only, today's behaviour, and the DEFAULT
    pub hold_name_tokens: bool, // (#102) require a NARROW/GEO token to sit at a word start before it disqualifies or classifies a fund name. `core::geo_tier` scans with bare `contains`, and the list holds short generic words ("value", "select", "small", "esg", "health") that also occur INSIDE longer words — the classic "N-etf-lix" substring bug, here able to silently delete a legitimate core from the shortlist. Deliberately a word-START rule and not whole-word: half the list is intentionally a PREFIX ("technolog" -> Technology/Technologies, "financ" -> Financial/Finance, "communicat", "sustainab", "semiconduct"), which whole-word matching would break. Tokens already carrying their own separator (" pab", "sri ") are hand-rolled guards for exactly this and are passed through untouched. Strictly a TIGHTENING: every match it drops was a mid-word accident. false = bare substring, today's behaviour, and the DEFAULT
    pub growth_sector_cap: usize, // (#101) at most this many stocks per GICS sector in the printed growth table; 0 = off and the DEFAULT. Nothing else in the tool limits concentration BY SECTOR: growth_corr_cap ships 0.0, and while (#126) turned growth_value_floor_pct on at 40.0 that brake cuts by price-for-growth rather than by sector, so the stock table can still legitimately be twenty semiconductor names -- the printed "sector mix" line is an observation, not a constraint. Post-rank DISPLAY trim at the same seam as the redundancy skip and the value brake: after the score/sector cut, before the table is cut to n, so a dropped row refills from below. Keeps the highest-scoring rows of each sector (the list arrives rank-ordered), pinned tickers bypass, and a name with no sector is kept (unjudgeable is not a verdict)
    pub growth_gate_on_tr_cagr: bool, // (#99) add the dividend leg to the CAGR the whole-life reject bar judges, so growth_min_cagr stops deleting payers. Closes are price-only (use_adjusted_close: false), so long_cagr is roughly total-return CAGR minus dividend yield: a name compounding at 19%/yr total while paying 3% measures ~15.5%/yr and is REMOVED from the universe by growth_min_cagr: 19.0 -- and no dividend_weight can recover it, because the reward is applied after the gate. true = life_leg_cagr adds `tr_cagr - life_cagr`, the dividend contribution in CAGR points over the same endpoints, to whatever leg it already returned. That is deliberately an UPLIFT and not a switch to tr_cagr itself: switching would also silently discard the life_cagr_max_years window, making the knob two changes. LOWER BOUND, since tr_cagr adds payouts without reinvesting them. Missing tr_cagr or life_cagr falls back to today's leg (missing data passes). false = price-only, today's behaviour, and the DEFAULT
    pub backtest_calendar_cadence: bool, // (#117) resolve the walk's three bar-counted parameters PER SERIES instead of once per run. `cadence`, `min_history` and `step` are counted in BARS and fixed at 12/36/6 for the whole monthly path — 12 bars a year, 3 years of history before a cutoff, 6 months between cutoffs — but the series they are applied to are not monthly. `fetch::chart_json_long` asks Yahoo for `interval=1mo` and Yahoo answers with whatever it likes for a thin line; nothing in this repo reads `meta.dataGranularity`. Census of the live long cache: 1818 `1mo`, 2059 `1wk`, 1155 `1d`, 317 `1h`, 304 `3mo`, 172 `1y` — 31% monthly. `step` is the sharp end: 6 bars on a daily series is 6 SESSIONS, so that name contributes a cutoff every week and a half and lands in the sample with ~40x the weight of a monthly one, purely from which granularity Yahoo happened to serve. `cadence` is the other end: it annualises `volatility_pct` (read by the LIVE `sharpe_cap` and `sharpe_cap_etf`) and sizes `long_ma`. true = derive bars/year from the record itself (span / count) and re-express the three as 1 year, 3 years and 6 months of THAT series. NEAR-INERT WHERE THE DATA WAS ALREADY RIGHT, which is the property that makes it safe to reason about: a real monthly series measures 12.00 bars/yr and resolves to exactly 12/36/6, and a real daily one measures 252.0 and resolves to 252/756/126 against today's 252/750/126. MEASURED 2026-08-22 AND REFUSED — six runs, `backtest {20,12,8} universe`, one boolean apart: GROWTH edge +780.8 -> +37.6 at 20y, +193.5 -> +69.3 at 12y, +48.8 -> -4.0 at 8y, rank-1 h2h 48% at 12y (the guard is 50%) and a worse top-3 worst window at all three. It ROSE the cutoff count rather than cutting it (20800 -> 31010 at 8y): `min_history: 36` bars is thirty-six years of a `1y` series, so the fixed floor was doing a second, undocumented job as a granularity filter, and removing it injects the thin end of the universe. The defect above is real; this is not the fix for it. Full grid and the revert rule: the (#117) receipt in tests/ci-settings.yaml. false = the fixed constants, today's behaviour, byte-identical, and the DEFAULT
    pub vol_daily_equivalent: bool, // (#97) restate `volatility_pct` (and its `downside_dev_pct` twin) in DAILY-equivalent units on any cadence, so the absolute thresholds reading them mean the same thing live and in a backtest. A per-bar stdev scales with sqrt(bar length), so the same asset prints ~sqrt(21) = 4.6x more vol on the monthly bars a long run walks than on the daily bars the screen walks -- and EVERY consumer is an absolute threshold: sharpe_cap/sharpe_cap_etf clamp long_cagr/vol (4.6x bigger denominator = the cap stops binding in the run that fitted it, while live it binds for nearly every name past the CAGR gate), plus the growth_max_vol* ceilings and the normal_volatility_pct divisor. All three ship-rule horizons are monthly (`long || years >= 8`). `span_to_bars` already holds every WINDOW to the same calendar length across cadences; this is the amplitude half of that same train==serve rule. LIVE IS BIT-IDENTICAL EITHER WAY: at cadence 252 the factor is exactly 1.0. false = today's raw per-bar figure, and the DEFAULT
    pub print_edge_annualized: bool, // (#96) restate a lane's `edge` in points PER YEAR beside the cumulative figure. `edge` is a spread of returns over the run's whole `years` hold and `top-N excess` is per year, yet both print as "pts" — a ~20x gap between two numbers the report invites the reader to compare. Print-only: no score, no gate, no journal field reads it. OFF = today's single unqualified figure, which is what every golden holds.
    pub cost_per_rebalance: bool, // (#96) charge the turnover round trip once per ~6mo REBALANCE inside the hold instead of once per hold. `turnover_frac` is a per-rebalance number (mean Jaccard between consecutive ~6mo buckets) but the charge was applied once against a `years`-long cumulative edge, so a book re-formed forty times over a 20y hold paid one round trip. Moves the printed `net` and can newly fire "NET <= 0: too churny to trade". OFF = the single charge every golden and every fitted receipt was measured under.
    pub backtest_drop_lookahead_sector: bool, // (#95) stop stamping TODAY's GICS sector onto a historical cutoff. `stamp_asset_class` fills `quote.sector` from the current universe fetch, so a 1995 cutoff is labelled with the classification the company carries in 2026 — and that label REACHES THE SCORE: `picks::is_commodity` reads it and drives `commodity_damp`. A name reclassified since (Industrials -> Information Technology is the common one) is damped, or not damped, on a fact from thirty years after the decision. Unlike the point-in-time universe there is no as-of source to substitute: GICS history is not in any feed this tool reads. So the only honest options are today's label or NO label, and this knob picks between them. true = `quote.sector` stays None in the backtest, which makes `is_commodity` false and `commodity_damp` inert there (missing data passes, the house rule) — the walk then measures the lane WITHOUT the damp, which is at least a thing that could have been computed in 1995. false = today's label, today's behaviour, byte-identical, and the DEFAULT. Does not touch the LIVE path, where today's sector IS the point-in-time sector
    pub splice_trim_point_in_time: bool, // (#94) move the redenomination trim from PARSE time to CUTOFF time, so a splice cannot delete history that had not happened yet. `splice_trim_start` keeps the LAST qualifying step in the whole series and `parse_chart` drains everything before it — so a 2015 redenomination deletes all pre-2015 bars for a 2005 cutoff, and `age_years`, `life_cagr`, MAXDD and trend R² are then computed on a record the walk could not have seen. Its own receipt measures the reach: 138/3346 non-crypto series (4.1%), median trimmed series losing 72% of its record. true = `parse_chart` leaves the series whole and `core::backtest_quote` trims over `[..=as_of]` instead, which is the same answer at the last bar (so LIVE is unchanged in the merged-series path, which already re-trims) and a knowable answer at every earlier one. false = trim at parse, today's behaviour, byte-identical, and the DEFAULT
    pub demean_by_market: bool, // (#93) split the peer group `demean` subtracts by LISTING MARKET as well as by cutoff bucket and asset class. `realized` is `closes[fwd]/closes[i] - 1` — a ratio of two closes in the LISTING currency, with no FX conversion anywhere on the return path — so a EUR UCITS line, a GBp LSE line and a USD S&P name are pooled into one peer mean with their FX returns stripped. The de-mean is then a mixture of currencies and every `relative` in the group carries the FX move over that window as contamination. Splitting the group by market makes the FX return COMMON to the group, so subtracting the mean cancels it, at no fetch: `Quote::stub` already fills `market` from the ticker suffix. It over-splits — Germany, France and the Netherlands all trade in EUR and become three groups — so a market slice thinner than MIN_PEER_GROUP falls back to the pooled group rather than de-meaning against nobody. Does NOT fix the ABSOLUTE leg, which still compares native-currency books to ^GSPC; see the receipt. false = one pooled group, today's behaviour, byte-identical, and the DEFAULT
    pub drop_dead_ticker_series: bool, // (#122) DROP a ticker whose Yahoo mapping no longer points at the company it names. A dead company's ticker is not retired, it is REMAPPED: it comes back typed `MUTUALFUND` with a numeric registrant id for a name (`CFC` = "3847602", `BSC` = "1315901", `WYE` = "4595480") and its bars KEEP RUNNING to today. `CFC` (Countrywide, index exit 2008-07) carries 212 bars after that exit, `BOL` 220, `CCU` 212, `CBE` 52, `MOLX` 49. A hold opened before the exit and closed `years` later reads its ENTRY from the real company and its EXIT from whatever now owns the ticker, then reports the ratio as a return — this is a WRONG NUMBER in the sample, not merely a missing one, which is what separates it from the survivorship hole (#121) measures. THE SIGNATURE IS EXACT ON THIS DATA: across all 6149 cached series, `MUTUALFUND` + an all-digit name matches 134 tickers and every single one is an S&P 500 member — zero false positives among non-members, which include real mutual funds carrying real names. 34 of the 134 still carry bars and are the ones actively contaminating a forward return; the other 100 contribute nothing either way. DROPS THE WHOLE SERIES, not its tail, because the remap date is not in the payload and there is no honest place to cut. WHAT IT DOES NOT FIX: a ticker reused by another LIVE equity keeps a real name and an `EQUITY` type and is invisible here (`CCU` now resolves to Compania Cervecerias Unidas, `BEAM` to Beam Therapeutics, `GENZ` to a VanEck ETF), and it does nothing for the 387 members served with zero bars, which never produce a cutoff at all. PIT-ONLY BY CONSTRUCTION, WHICH IS THE UNCOMFORTABLE PART: the wide `universe` pool is the LIVE screen universe (5049 names) and contains none of these tickers, so on a non-PIT run this knob drops nothing — measured, six runs at {20,12,8} one boolean apart, cutoff counts identical to the unit (4541 / 11835 / 20806 both arms). The contaminated series enter ONLY through the PIT membership map, which is to say the lane that exists to REMOVE look-ahead is the only lane that admits them. Full receipt and the A/B: the (#122) entry in tests/ci-settings.yaml. false = today's behaviour, byte-identical, and the DEFAULT
    pub print_selection_count: bool, // (#92) print, beside the top-N ladder table, how many rungs the eye is choosing between. WRITTEN FOR A DEBT THAT IS NOW PAID: `VERDICT_TOP: 3` used to be the argmax over `TOP_LADDER`'s 13 rungs, measured on the same data the report then quotes, with no best-of-N haircut and on the ~0.5-3.5 effectively independent windows (#91) counts. (#120) fixed the basket a priori at 10, so the repo no longer makes that selection and the caveat came OFF the screen footer — a best-of-13 tag beside a basket nobody argmaxed would caveat a selection that was never made. It stays on the TABLE header, whose 13 rungs are still printed side by side for a reader to argmax by eye; that reader's selection is what it now warns about. This file still carries exactly one real multiple-testing correction (the 14 fund factors, Šidák-tightened). Display only — no gate, no score, no journal field. false = today's output, byte-identical, and the DEFAULT
    pub print_n_eff: bool, // (#91) print, beside every window count that carries a verdict, how many INDEPENDENT trials that count is worth. Windows are ~6-month buckets and the hold is `years`, so consecutive windows are near-copies of one price path and the honest count is windows/(2·years): the 20y golden's headline "win 67% ... (windows 21)" is 21 draws of 0.5 of a trial, and the 12y run's 33 windows are ~1.4. A win rate over fewer than two independent trials is a story, and nothing in the printed report currently says so. Display only — no gate, no score, no journal field reads or writes it. false = today's output, byte-identical, and the DEFAULT
    pub split_purge_months: i64, // (#90) how much history to DROP off the end of the earlier side of every train/test and early/late split, so the two sides do not share a forward path. Both splits cut by ROW INDEX with no gap today, and at a 12y hold a boundary cutoff shares 11 of its 12 forward years with the "held-out" half — the labels ARE the other side's outcomes, so an OOS rho or a `tune` winner reads as evidence while being close to in-sample. `tune`'s per-split `demean` fixes PEER-MEAN leakage, not this. The honest span is the purge (the forward window, `years`) PLUS an embargo of the same, so 24·years months — 288 at 12y — which is more history than the sample has. 0 = no purge, today's behaviour, byte-identical, and the DEFAULT
    pub bootstrap_block_buckets: usize, // (#119) block length, in ~6-month cutoff buckets, for `bootstrap_edge_ci`, where 0 means DERIVE IT FROM THE HOLD (`bootstrap_block`: 2·years buckets, the dependence length, because two cutoffs less than one hold apart share most of a forward path). (#89) worked that value out and still shipped one bucket, because both call sites are handed `samples` and `tuning` and neither carried the horizon — a knob read at 8y, 12y and 20y in the same run cannot be a fixed count. Any positive value is an explicit override, so 1 restores the one-bucket draw every band printed before (#119). Deriving it does NOT produce a wider band on this data, it produces NO band: at the honest length the record holds under ten blocks at every horizon, and a block that large stops varying (measured collapse table in `bootstrap_edge_ci`)
    pub backtest_anchor_windows: bool, // (#88) the OTHER half of the real-vs-nominal skew, and the same shape: `core::backtest_quote` hardcoded an EMPTY `anchor_windows` map, so every backtest leg used `core::default_anchor_half` while the live path uses the settings map. ci-settings ships 1Y: 182 against that default's 90, 1M: 15 against 30, 1W: 3 against 7 — so `return_1y`, the input to `growth_min_1y_pct`, to `accel` (the lane's heaviest term) and to `mom121`, is a DIFFERENT ESTIMATOR in train and in serve, and the 1M knife gate is measured over half the window it is swept on. true = the backtest reads the same `anchor_windows` the live tool does, which is the direction A1 asks for and the one that is actually implementable (unlike the inflation half — no as-of HICP series exists offline). false = today's behaviour and the DEFAULT: every golden and every fitted receipt stands until this is deliberately turned on, at which point ALL of them need re-measuring, because the quantity under every perf-fed bar has changed
    pub use_adjusted_close: bool,    // (Item 21) PROBE switch: when true, parse_chart prefers Yahoo's adjclose (split+DIVIDEND adjusted) over raw close, so the long CAGR / range_pct near-high gate / drawdown / overext brake measure TOTAL return instead of price-only — fixes dividend-compounders that are mis-ranked (CAGR understates total return) or mis-EXCLUDED (nominal price below old high fails growth_min_range_pct). Flows to BOTH live + backtest (one parse site) so no train-serve skew. DEFAULT false = raw close, unchanged. Crypto/FX have no adjclose -> falls back to close (no effect). Adjusted close re-calibrates EVERY price threshold (range floor, overext, min_cagr, vol/SMA), so flip ONLY for a full `backtest universe` re-validation + gate re-sweep, then keep it only if both OOS halves still hold. Golden-rule-gated.

    // --- CRYPTO market-sentiment damp (Bitcoin NUPL): a whole-market greed gauge already fetched for
    //     the screen footer; high NUPL = euphoria/top -> shrink crypto scores in BOTH lanes. ---
    pub nupl_euphoria: f64,          // (4) NUPL above this starts damping crypto scores (~0.5 = "belief/denial" greed zone)
    pub nupl_damp_floor: f64,        // (4) crypto-score multiplier at NUPL=1.0 (full euphoria); 1.0 = damp off
    pub nupl_capitulation: f64,      // (4) NUPL below this starts BOOSTING crypto scores (~0.25 = fear/accumulation zone); 0 = boost off
    pub nupl_boost_ceiling: f64,     // (4) crypto-score multiplier at NUPL=0 (deep capitulation); 1.0 = boost off. BACKTEST-BLIND judgment lever — keep mild
    pub crypto_max_mvrv: f64,        // (#45) CRYPTO's valuation CEILING — reject a coin whose MVRV (market cap / realized cap) exceeds this. The class-native answer to `growth_max_peg`, and the FIRST one that survived measurement: the (#37) DefiLlama revenue proxy was rejected because chain "P/E" runs in the thousands, while MVRV runs 0.26-1.37 across the live top 100. MVRV is NOT a PEG — realized cap values each coin at the price it last moved, so there is no earnings term and it reads as a P/B. A real GATE in `growth_score` (unlike the ETF ceiling, which is a display trim: MVRV covers the whole universe in ONE request, so there is no fetch-budget reason to keep it off the ranked edge). Missing MVRV PASSES — CoinMetrics serves ~17 of the top 100, so the house missing-data rule does most of the work here. SET BY JUDGEMENT AND UNSWEEPABLE: not because the data lacks history (it goes back to 2011-12-31, daily) but because the backtest pool growth-scores ZERO crypto rows at every horizon — 20y-s 0/4633, 12y-s 0/11758, 8y-s 18 cutoffs with 0 scored. No sweep can ever vote on this number. 0 = off (default); ci-settings ships 1.6 — NOT the 2.0 this block argues for; see the (#64) receipt in ci-settings.yaml for why the stricter value stands and why no run can settle it

    // --- QUALITY tilts (B/C) — all from already-fetched closes, ZERO extra fetch ---
    pub sharpe_weight: f64,          // (B) GROWTH lane: reward per unit of CAGR/volatility (return per unit of daily swing). 0 = off
    pub onsale_sharpe_weight: f64,   // (B) ON-SALE lane's own Sharpe weight — split from the growth lane because the shared knob conflicts: ablation shows growth wants 0.15 but on-sale wants 0 (removing it lifts on-sale 12y edge +23.7). 0 = off
    pub sharpe_cap: f64,             // (B) cap on that CAGR/volatility ratio fed into the reward
    pub sharpe_cap_etf: f64,         // (#37) ETF-only Sharpe cap, LOWER than sharpe_cap: cross-listed lines of the same fund print different daily stdev (thin-line prints + FX conversion), so ETF CAGR/vol above ~9 is listing-line noise, not fund risk — uncapped it reorders identical index wrappers against their real TER cost. Backtest pool holds stock constituents only -> edge byte-identical. 0 = off (ETFs use sharpe_cap)
    pub calmar_weight: f64,          // (C) reward per unit of CAGR/max-drawdown (return per worst historical pain). 0 = off
    pub calmar_cap: f64,             // (C) cap on that CAGR/max-drawdown ratio fed into the reward

    pub prefer_eur: bool,            // dedup currency twins (BTC-EUR/BTC-USD): keep the EUR leg if true, else USD
}

/// (E) Hard clamp bounds on the P/E value tilt, so one absurdly cheap/expensive P/E can't swamp the
/// score. note: fixed — these are guardrails, not a thing anyone tunes; widen here if ever needed.
pub const VALUE_TILT_MIN: f64 = 0.5;
pub const VALUE_TILT_MAX: f64 = 1.5;

/// (C) Sessions in the long moving-average window (~200 weeks of trading days). note: a const,
/// not a knob — 200wk is the conventional long-trend line; change here if you disagree.
pub const LONG_MA_SESSIONS: usize = 1000;

impl Default for BuyHeuristic {
    fn default() -> Self {
        BuyHeuristic {
            // gates
            min_1y_pct: 0.0,
            growth_min_1y_pct: 0.0,        // the value the growth lane hardcoded before this knob existed -> neutral default AND live behaviour at once. Loosening it is measured-negative (see the field doc); ci-settings ships 0.0 with the receipt
            min_1y_pct_crypto: -60.0,      // crypto routinely swings -40% in a year without breaking
            max_1m_drop_pct: -15.0,
            max_1m_drop_pct_crypto: -35.0, // alts routinely shed -20..-30% in a month without breaking
            min_long_pct: 0.0,
            min_long_pct_crypto: -70.0,    // -EUR 5Y is peak-anchored: allow deep pullbacks, cut true corpses (-70%+)
            growth_min_5y_pct: 0.0,        // the value the growth lane hardcoded before this knob existed -> the equity default stays behaviour-neutral. (Crypto is NOT neutral: this bar reaches coins now, where the old gate skipped them.)
            growth_min_5y_pct_crypto: 0.0, // the value the SHARED knob shipped before the twin existed -> adding the twin is behaviour-neutral for coins by construction, and stays neutral when the equity bar moves
            growth_min_8y_pct: -1e9,       // off: at the +50 asked for it rejects nobody, so shipping it as a default would only risk the validated edge for no live effect. settings.yaml sets a real bar
            growth_min_20y_pct: -1e9,      // off, same reasoning (+200 rejects nobody today)
            perf_fill_coverage_pct: 100.0, // off: no blank cell is ever filled. ci-settings.yaml ships the real 90.0 — the code default stays at "print only what was measured under its own label"
            min_avg_turnover_eur: 0.0,     // off by default; settings.yaml sets a real floor to drop thin names
            endpoint_smooth_days: 1,       // (#17/Step 4) 1 = raw last close (byte-identical, validated edge intact); e.g. 5 averages the last week's closes for measurement endpoints
            // score
            normal_volatility_pct: 2.0,    // ~2%/day = a typical large-cap equity
            discount_cap: 35.0,            // a ~35%-off (for its vol) dip maxes the discount
            discount_weight: 0.35,         // (#4) demote the dip reward — walk-forward rho is NEGATIVE for on-sale across 3/5/7y and ~0 on the 354-name wide sample (deepest-dip ranking carries no selection skill); 0.35 shifts weight to the CAGR/sharpe terms that drive the working growth lane WITHOUT gutting on-sale scores (0.15 dropped normal names below min_score for only a noise-level rho gain). 1.0 = old, 0 = off
            momentum_bounce: 1.0,          // neutral: a weekly bounce is noise at a multi-decade hold horizon
            momentum_knife: 1.0,           // neutral: this-week direction shouldn't reorder a 40-year pick
            long_trend_weight: 0.5,        // per %/yr CAGR: a +30%/yr compounder adds ~15, secondary to the discount (cap 35)
            long_trend_cap: 30.0,          // cap the long-leg CAGR at 30%/yr (a +46%/yr coin doesn't dwarf a +14%/yr one). 0 = OFF (uncapped) — the live fixture ships 0 (#3h, by direction, against the curve); this default stays 30 per the house rule that defaults are neutral and ci-settings carries live, and because it keeps the "cap still clamps when enabled" test honest. CURVED at 12y (#3g): the ceiling wanted to come DOWN, not up — 15/20/25/30 read edge +175.6/+185.1/+191.1/+181.3 and everything above falls monotonically (40 +177.3, 50 +168.1, 60 +168.1), confirming the older on-horizon 30->50 point-test. OFF was then MEASURED on the same n=1338: edge +169.8, i.e. -11.5 vs 30 and -21.4 vs the 25 peak. Knob is SHARED with the on-sale foil (`long_trend_weight × capped_trend`), so a growth-lane curve prices only half its effect
            fixed_cagr_years: 0,           // (#15) off: rank on the longest available leg (today's behaviour). Set 10 to pin every name's CAGR to its 10Y window
            growth_min_leg_years: 5.0,     // the 20/8/5Y ladder as it has always been — the 2Y rung stays out until a grid says otherwise
            use_trend_cagr: false,         // (#14) off: two-point endpoint CAGR (today's behaviour). true = least-squares log-slope CAGR (endpoint-robust)
            use_life_cagr: false,          // (#3j) off: the 20/8/5Y leg (today's behaviour). true = whole-life CAGR since listing — the `cagr` column's number, no age cliff, but no common window either
            health_zero_cagr: -10.0,       // a -10%/yr multi-year trend = dead -> trend_health 0
            sustained_decline_pct: -40.0,  // (B) 1Y AND 5Y both <= -40% = multi-year bleed, not a dip
            sustained_decline_penalty: 0.4, // (B) score ×0.4 when that holds (value-trap dock)
            deep_decline_pct: -70.0,       // (B/C) 5Y <= -70% = a 7y+ deep bleed (e.g. LTC -73%) riding an ancient 10Y pump
            deep_decline_penalty: 0.15,    // (B/C) score ×0.15 then — harsher than the -40% tier
            min_score: 5.0,                // (A) hide ranked rows scoring <= 5 (near-the-high padding); 0 = show all top_picks
            cheap_weight: 0.07,            // (#4) ~+4 at the cap (halved from 0.15) — "structural cheap" is another dip term the backtest doesn't reward; demoted toward the trend/quality factors
            cheap_cap: 60.0,               // (C) cap the below-SMA % fed into the cheap reward
            dividend_weight: 1.5,          // (D) ~+9 at the cap for a 6% yielder
            onsale_dividend_weight: 1.5,   // (#61) ON-SALE lane. Deliberately EQUAL to `dividend_weight` above, not 0.0: the split's measurement was made against ci-settings, so ci-settings is where the 0.0 ships and the default build stays byte-identical to the pre-split lane. If `dividend_weight`'s default ever moves, move this with it or the "byte-identical" claim silently stops being true
            dividend_cap: 6.0,             // (D) cap the trailing yield % fed into the dividend reward
            tax_keep_eu: 1.0,              // (D/PT) 1.0 = no tax haircut -> byte-identical to the pre-tax lane; ci-settings.yaml ships the live rate
            tax_keep_other: 1.0,           // (D/PT) same: neutral out-of-the-box, the fixture carries the operator's number
            ref_pe: 20.0,                  // (E) "fair" P/E; PE 10 -> ×1.5 (capped cheap), PE 40 -> ×0.5 (capped rich)
            quality_weight: 0.15,          // (F) per % return-on-capital: 40% adds ~+6 (capped). Sized small while the term was blind; the first sighted run (ci-settings (F)) says it earns more than that in the buy lane — zeroing it costs -48.7 edge and flips rho to -0.06
            quality_cap: 40.0,             // (F) cap the ROE % at 40 (a buyback-levered 200% ROE doesn't dwarf a healthy 25%)
            quality_neutral: 0.0,          // (#87) code default = the pre-(#87) `unwrap_or(0.0)`, so an uncovered name keeps taking the full -6.0 until this is deliberately raised. See the field comment and the ci-settings receipt
            // growth lane (near-high compounders still climbing)
            growth_min_range_pct: 80.0,    // must sit in the top 20% of its own ~10y range. Tightened 70->80: the walk-forward shows the acceleration signal only works for genuine near-high names — at 80 the growth lane's rho rises (5y +0.24->+0.35 narrow, +0.21->+0.24 wide) AND the top/bottom-half edge flips POSITIVE (+31.6 pts wide, OOS +0.12/+0.12), i.e. top picks actually outperform. Loosening to 55 collapsed it (rho +0.10, OOS-early negative)
            growth_min_range_pct_8y: 0.0,  // off: the 8y-window bar is live-only and ungradeable by the backtest, so the code default stays at "judge the ~10y window alone". ci-settings.yaml arms it at the same 80
            growth_min_range_pct_8y_crypto: 0.0, // off, same reasoning (40 is inert on today's ranked coins anyway)
            growth_min_range_pct_crypto: 40.0, // crypto: looser range floor (top 60% of its own range) so more coins surface — alts spend most of their life far below ATH yet still out-compound. The BTC-relative tilt + nupl damp keep the wider crypto table honest. The strict equity gate (80) stays for stocks/ETFs
            growth_btc_outperf_weight: 0.3, // crypto: ±30% score swing at a full year of BTC-relative out/under-performance (bounded 0.5x..2x). BTC itself nets 1.0x (the neutral base). 0 = rank crypto on absolute growth only
            growth_min_cagr: 8.0,          // long-leg must compound >=8%/yr (beat a broad index) to be a "proven" grower
            growth_min_cagr_crypto: 0.0,   // crypto: any positive long trend qualifies (show ALL potential growers up to BTC); raise toward 8 to tighten the crypto table to proven compounders
            growth_trend_weight: 0.35,     // reward per %/yr of the CAPPED long-leg CAGR — the `LEG` column prints exactly this input. CURVED at 12y (#3g), replacing a stale "raw long-CAGR is mildly HARMFUL (Δ+0.03/+0.10)" note the same run refutes: zeroing the term costs edge +181.3 -> +119.9 (rho +0.18 -> +0.15), so it is load-bearing, not harmful. It is ALSO already at its ceiling — 0.15/0.25/0.35/0.45 read edge +184.6/+185.9/+181.3/+183.3, a flat plateau whose whole spread sits far inside the [+133.9 … +236.5] bootstrap band, and above it the edge CLIFFS (0.55 +153.8, 0.70 +134.7, 1.00 +125.1; winsorized tracks it, so the cliff is not an outlier effect). "Reward a faster compounder harder" has no honest room left on this knob
            growth_accel_weight: 0.2,      // trimmed 0.35->0.2: rho RANKS accel as the dominant helper (wide Δ-0.13) but the EDGE ablation flips it — accel HURTS the profit spread (zeroing it lifts wide edge +43.7->+94.5). Recent 1Y-vs-CAGR pop is the noisiest, most mean-reverting growth signal (hot-streak chasing). 0.2 is the durable middle: wide edge +43.7->+50.5 (5y)/+24.3->+28.5 (3y), rho flat/up, OOS both halves positive. 0.0 maxes edge but flips OOS-late NEGATIVE (regime artifact). per pt the last year outpaced the long CAGR -> momentum building
            growth_accel_cap: 50.0,        // cap that acceleration term (a +200% year doesn't run away with it)
            growth_accel_beta: 1.0,        // (#98) 1.0 = subtract the whole long CAGR, the shipped expression, byte-identical. Any other value is an UNSWEPT scoring change; see the receipt in ci-settings before moving it
            growth_min_score: 5.0,         // hide growth rows scoring <= 5 (padding); 0 = show all top_picks
            growth_min_score_etf: 5.0,     // code default = growth_min_score (no behavior change out-of-box); ci-settings.yaml ships the lower ETF-calibrated floor
            growth_allow_negative_scores: false, // (#86) OFF = the shipped lane, byte-identical: the floor stays clamped at 0 and a negative base keeps taking the damp. Turning it on changes both halves at once and is edge-affecting on the live tables (the backtest pool has no negative-base rows to reorder, so no golden covers it) — measure before shipping it, and never turn on one half alone
            growth_overext_cap: 100.0,     // (1) a name 100%+ above its 200wk SMA is maximally stretched
            growth_max_above_ma: 0.0,      // code default off; ci-settings.yaml ships the validated 150
            growth_require_lifetime_uptrend: false, // (#25) off until a probe validates it
            growth_maxdd_cap: 0.0,         // (#26) off until a probe validates a value
            growth_maxdd_cap_crypto: 0.0,  // (#26) off by default; ci-settings ships 84 (just above BTC's -83)
            growth_max_vol_crypto: 0.0,    // (#36) off by default; ci-settings ships 3.0 (just above BTC's ~2.4%/day)
            growth_min_age_years: 0.0,     // (#33) off by default; ci-settings ships 0.0 TOO — deliberately, to keep early winners visible and let trust_factor dock them instead (backtest-blind either way, so no grid can settle it)
            growth_min_aum_etf: 0.0,       // (AUM) off by default; ci-settings ships 100M (backtest-blind ETF closure-risk gate)
            growth_max_peg: 0.0,           // (#37) off by default. Set 2.0 to reject PEG > 2. MEASURABLE (peg_yield is filled in the backtest) -> validate on `backtest universe fund 12` before shipping a value
            growth_max_peg_etf: 0.0,       // (#37 funds) off by default. ci-settings ships 2.0 — the value that keeps the concentrated growth funds and cuts the broad expensive ones; see the knob doc for why it is not the equity 1.6
            growth_require_peg: false,     // (V) off by default. Only meaningful alongside growth_max_peg — it closes the hole where a filer with NO per-share data walks past that ceiling untested
            growth_min_net_margin: 0.0,    // (#38) off by default. Set 10.0 to reject NET% < 10. Measurable too — and likely to cut low-margin INDUSTRIES rather than bad names, so measure before believing it
            growth_max_daily_1m: 0.0,       // (P4a) off by default. 20.0 rejects a name that moved more than +20% in a single session last month
            growth_max_vol: 0.0,            // (P4b) off by default. Ladder sits BELOW the crypto twin's 3.0 — see the field doc
            growth_corr_cap: 0.0,          // (#41) off — and ci-settings ships 0 too, which is rare: the probe's measured cap TRUNCATES the live table instead of deduping it (38 rows -> 29 at 0.4). Receipt in ci-settings.yaml
            growth_value_floor_pct: 0.0,   // (#75) off by default — at 0 both call sites skip the trim entirely, so the served table and every prior receipt's book are byte-identical. MEASURABLE on `backtest <h> universe fund` only; grade it on the VERDICT_TOP book before shipping any value — that basket was 3 when (#75) was refused and is 10 since (#120), which is exactly the half of the refusal that no longer holds. (#126) THAT GRADE WAS RUN 2026-08-23 and tests/ci-settings.yaml now SHIPS 40.0. This CODE default stays 0.0 on purpose, matching every other graded gate in this block (growth_maxdd_cap 0.0 vs shipped 84.0, growth_max_peg 0.0 vs shipped 1.6, growth_fund_weight 0.0 vs shipped 0.07): the code default is the neutral fallback for a config-less run, the yaml carries the shipped policy. Do not "sync" the two
            growth_max_margin_swing: 0.0,  // (#39) off by default. The cycle detector growth_min_net_margin can't be: it reads margin DISPERSION, not level
            growth_max_dilution_pct: 0.0,   // (P1a) off by default. 5.0 rejects a filer that grew its share count more than 5% in a year
            growth_min_interest_cover: 0.0, // (P1b) off by default. 3.0 rejects operating income under 3x interest expense
            growth_min_fcf_margin: -1e9,    // (P1c) off by default — NOT 0, which is a real bar here. See the field doc
            growth_min_net_cash_rev: -1e9,  // (P1d) off by default — NOT 0, for the same reason as P1c
            growth_commodity_damp: 1.0,    // (#44) 1.0 = OFF (this knob is a MULTIPLIER, so neutral is 1.0, not the house 0 — same inversion as tax_keep_eu); ci-settings ships 0.8
            growth_fx_damp: 1.0,           // (#45) 1.0 = OFF (multiplier, same inversion as #44); ci-settings ships 0.98 (non-EUR-listed ETF FX/venue tie-break)
            growth_ter_drag: false,        // (#34) off by default; ci-settings ships true (backtest-blind ETF cost dock)
            growth_trust_ladder: false,    // (#47) off by default = the single 10Y cliff, byte-identical to the pre-(#47) lane. ci-settings ships nothing, i.e. false TOO — the ladder was built, measured (arm D) and REFUSED: rank-1 median down at all three horizons. Do not flip without a run that clears the rule it failed (rank-1 median up at the 20y king, h2h >=50% at all three)
            growth_trust_young: 0.5,       // (#49) the ladder's 2Y rung; the 1Y rung is half of it. 0.5 reproduces (#47)'s `none -> 0.50` bottom exactly, so this default is byte-identical to the pre-(#49) ladder and arm D's grid stays reproducible. Inert at the shipped `growth_min_leg_years: 5.0` — no 2Y/1Y-rung name survives the history gate to be docked
            growth_overext_floor: 0.05,    // (1) ...and keeps only 5% of its growth score at full stretch. Tightened 0.2->0.15->0.05: each step a harder blow-off-top brake (buying right after a parabolic run-up is a poor long-hold entry). The walk-forward sweep ranks 0.05 the best generalizer — wide 5y rho +0.26->+0.28 AND OOS-late rho +0.09->+0.14 (durability +55%) with the profit edge flat (+108.5->+106.8). A prior session rejected 0.1 for "docking NVDA out of the table," but that was regime-bound: at 0.05 today NVDA still scores 6.7 > growth_min_score 5 (it's -10.6% off-hi, not parabolic) and the displayed stocks order is unchanged. 1.0 = brake off
            growth_turnover_weight: 0.5,   // (L) liquidity tilt per ln(turnover/€1B), added after the brake. Lifts deep-liquid proven compounders (NVDA €32B -> +~1.0) over illiquid €200-500M names they tie/trail on the brake-docked score, without touching the validated edge (BACKTEST-BLIND)
            growth_overext_cap_crypto: 100.0, // (#4) defaults to the equity cap (NO behavior change until tuned). Raise (e.g. 200) so the brake lets crypto ride further above its SMA before docking
            growth_fund_weight: 0.0,       // (G) OFF by default — the fund term is inert until validated. Wired through the growth lane so `backtest <set> fund` can ablate it; raise only on +Δ + both-half-positive OOS
            growth_fund_cap: 30.0,         // (G) clamp the fund factor to ±/+30 pts before weighting (irrelevant at weight 0); keeps a freshly-listed +9000% rev-accel artifact from running away with the rank
            growth_fund_neutral: 0.0,      // (#87) code default = the pre-(#87) `unwrap_or(0.0)`; inert anyway at growth_fund_weight 0
            growth_fund_scope_by_class: false, // (#87) OFF = today's behaviour: an ETF or a coin still collects the stock-peer neutral from every fundamental term it has no data for
            growth_fund_factor: "rev_accel".to_string(), // (G) the prior hardcoded factor — change in settings.yaml once the probe names a better one (irrelevant at weight 0)
            growth_fund_extra: Vec::new(), // (G+) OFF by default — an empty list makes the multi-term sum contribute exactly 0.0, so scores stay byte-identical to the single-factor tilt and every recorded receipt still holds
            fund_source: "fmp".to_string(), // (Item 22) FMP feed by default (= current behavior). Set "sec" for free/uncapped US annual fundamentals, then re-validate via `backtest <set> fund` before raising growth_fund_weight (irrelevant at weight 0)
            growth_mom121_weight: 0.0,     // (M) OFF by default — the 12-1 momentum term is wired + ablatable but inert until validated; raise only on +Δ + both-half-positive OOS
            growth_mom121_cap: 50.0,       // (M) clamp 12-1 momentum to +50 pts before weighting (irrelevant at weight 0); a name up >50% over the year-ago-to-month-ago window is already maxed for this tilt
            growth_smoothness_weight: 0.0, // (E) OFF by default (older settings.yaml unchanged); ci-settings ships 0.0 too since (#62) zeroed it 2026-08-10 — the 5.0 "swept optimum" was a knee among NONZERO weights only
            growth_underwater_weight: 0.0, // OFF by default (older settings.yaml unchanged); ci-settings ships the validated 0.3 (2026-07-19 probe + candidate lane run, see field doc)
            growth_value_weight: 1.0,      // (Item 20) FULL P/E-multiplier authority by default (=current behavior, no change). The validated edge never saw this term (pe_ratio None in backtest); dial toward 0 once the additive earnings_yield term (Item 19) validates and carries valuation honestly
            growth_proximity_weight: 1.0,  // (#48) FULL proximity authority = the raw `range_pct/100` multiply, byte-identical to the pre-(#48) lane. Neutral is 1.0, not 0 (multiplier authority, same inversion as growth_value_weight). 0.0 turns the term off; negative inverts it
            growth_geomean_fold: false,    // (#8) off: multiply proximity/value raw onto base (today's behaviour, validated edge intact). true folds them into the geomean (bounds the multiplicative stack) — validate via `backtest universe` both-OOS-positive before flipping
            life_cagr_max_years: 0.0,      // (#73) OFF by default = `growth_min_cagr`'s leg 2 reads the uncapped lifetime, today's behaviour; >0 windows that bar to the last min(age, N) years. Graded on a 6/8/10/12/16/20 ladder
            splice_max_weekly_rate: 0.0,   // (splice) OFF by default (older settings.yaml unchanged); ci-settings ships the measured 2.0 — the valley between the fastest real mover (3USL.L 1.87×/wk, a 3× leveraged ETP) and the slowest splice (COFF.L 2.06×/wk)
            print_acquisition_hazard: false, // (#112) off = the caveat block every golden holds.
            print_currency_mix: false, // (#112) off = today's footer set; the FX position stays invisible, which is the defect, not the default's fault.
            print_base_rates: false, // (#111) off = the report every golden holds.
            implied_growth_years: 0, // (#111) 0 = no reverse-DCF line, today's ranking footer.
            implied_growth_required_pct: 8.0, // (#111) unfitted judgement value; inert while the horizon is 0.
            entry_excess_yield_band: 0.0, // (#110) 0 = no level axis printed; the path axis alone, which is today's footer.
            growth_acc_drag: false, // (#109) off = no share-class dock; the Acc preference keeps coming from the price-only CAGR, which is today's lane.
            capital_gains_tax_pct: 28.0, // (#108) the exact constant the hardcoded `CAPITAL_GAINS_TAX` carried.
            cgt_hold_schedule: Vec::new(), // (#108) empty = one flat rate at every horizon, the footer every golden holds.
            rank_perturb_k: 0, // (#107) 0 = no perturbed re-scores, no footer — the point-estimate ranking every golden holds.
            print_book_deciles: false, // (#106) OFF = the mean-only ladder every golden holds.
            growth_er_weight: 0.0, // (#105) 0 = the term contributes exactly 0.0 — the lane every golden holds.
            growth_er_cap: 20.0, // (#105) unfitted judgement value; inert while the weight is 0.
            print_recall_capture: false, // (#104) OFF = today's report, which is what every golden holds.
            journal_core_list: false, // (#103) OFF = only the ranked slice is journalled, which is what every existing line in a user's `.screen_snapshots.jsonl` holds.
            hold_max_ter: 0.25, // (#102) the literal that shipped, extracted verbatim — moving it changes who wears the H flag.
            hold_ucits_or_domicile: false, // (#102) OFF = the name token is the only way to prove UCITS-ness, today's behaviour.
            hold_name_tokens: false, // (#102) OFF = bare `contains`, today's behaviour, mid-word accidents included.
            growth_sector_cap: 0, // (#101) 0 = off, today's unconstrained table, byte-identical. This is a RISK constraint, not a ranking improvement -- see the receipt on why the backtest cannot grade it
            growth_gate_on_tr_cagr: false, // (#99) OFF = the price-only bar every golden and every gate receipt was measured under. ON raises the measured CAGR of every payer, so it LOOSENS the gate and enlarges the pool -- before/after are two different universes, not an A/B on the same one
            backtest_calendar_cadence: false, // (#117) OFF = the fixed 12/36/6 (and 252/750/126) every golden and every fitted receipt in ci-settings was measured under, and MEASURED-AND-REFUSED as of 2026-08-22 — the ON arm breaks both ship-rule guards at 12y. Turning it on re-weights the SAMPLE — a daily-granularity name stops contributing ~40x the cutoffs of a monthly one — which moves every edge, rho and OOS number at once rather than moving one gate
            vol_daily_equivalent: false, // (#97) OFF = raw per-bar stdev, the scale every vol-reading receipt in ci-settings was fitted at. Turning it on does not move the live screen by a single bit; it moves the BACKTEST onto live's units, which is what invalidates those receipts and is exactly why it cannot ship without the re-sweep its receipt names.
            print_edge_annualized: false, // (#96) OFF = the cumulative-only line every golden holds. Print-only knob; turning it on adds a bracketed pts/yr restatement and changes nothing that is scored.
            cost_per_rebalance: false, // (#96) OFF = one round trip per hold, which is the arithmetic every fitted receipt in ci-settings was measured under. ON is the honest charge and is EXPECTED to turn long-horizon `net` numbers negative — that is the finding, not a regression.
            backtest_drop_lookahead_sector: false, // (#95) OFF = today's sector stamped at every cutoff, which is what every golden and every fitted receipt was measured under. Turning it on makes commodity_damp INERT in the backtest — a deliberate reachability change, and the reason `growth_gate_reachability_pin` describes the default and not this arm
            splice_trim_point_in_time: false, // (#94) OFF = the parse-time trim every golden and every fitted receipt was measured under. Turning it on lengthens the record of ~4% of names at every early cutoff, which moves age, life_cagr, MAXDD and R² for those names
            demean_by_market: false,       // (#93) OFF = the pooled (bucket, class) peer group every edge, rho and OOS number in this repo was measured against. Turning it on re-cuts the SELECTION metric itself, so it re-measures everything at once
            drop_dead_ticker_series: false, // (#122) OFF = today's behaviour byte-for-byte, which every golden in tests/fixture pins. Turning it on removes 34 contaminated series from the walk; grade it under Ship Rule v2 before shipping any other value
            print_selection_count: false,  // (#92) OFF = today's output byte-for-byte, which is what every golden in tests/fixture pins. Display-only: turning it on adds a best-of-N caveat to the top-N table header — (#120) took it off the screen footer, where it would now caveat a basket that is fixed a priori rather than selected — and changes no number
            print_n_eff: false,            // (#91) OFF = today's output byte-for-byte, which is what every golden in tests/fixture pins. Display-only knob: turning it on adds a tag to five lines and changes no number
            split_purge_months: 0,         // (#90) OFF = both splits stay a bare row-index cut, today's behaviour and what every OOS rho and `tune` winner in this repo was read off. Any value >0 drops rows and so moves every split-derived number at once
            bootstrap_block_buckets: 0,    // (#119) derive from the hold. The band this suppresses at every horizon was the one every 'clears 0' and every 'below 0 -> backwards' in this repo was read off — see the field comment and the ci-settings receipt
            backtest_anchor_windows: false, // (#88) OFF = the backtest keeps the empty map it always hardcoded, so every golden stays byte-identical. Turning it on re-measures every perf-fed gate at once — see the field comment and the ci-settings receipt
            use_adjusted_close: false,     // (Item 21) raw price-only close by default (= current behavior, validated edge intact). Flip to true ONLY for a full backtest re-validation + gate re-sweep — adjusted close shifts every price-calibrated threshold
            nupl_euphoria: 0.5,            // (4) NUPL > 0.5 = market greed -> start damping crypto
            nupl_damp_floor: 0.5,          // (4) at NUPL 1.0 (peak euphoria) crypto scores are halved
            nupl_capitulation: 0.25,       // (4) NUPL < 0.25 = fear/accumulation -> start boosting crypto (buy-the-fear)
            nupl_boost_ceiling: 1.3,       // (4) at NUPL 0 (deep capitulation) crypto scores ×1.3. BACKTEST-BLIND judgment, kept mild
            crypto_max_mvrv: 0.0,          // (#45) off by default. ci-settings ships 1.6, which is STRICTER than the 2.0 this note argues for and therefore no longer shares one definition of "expensive" with `nupl_euphoria: 0.5` (NUPL 0.5 <=> MVRV 2.0) — the disagreement is deliberate and receipted at (#64);see the knob doc
            // quality tilts (zero extra fetch)
            sharpe_weight: 0.15,           // (B) GROWTH lane. Halved 0.3->0.15: the edge ablation showed sharpe dragging the profit spread; 0.15 is the peak (5y wide edge +95.6->+107.3, beats both 0.3 and 0.0; rho +0.24, OOS positive). CAGR/vol ~10 for a calm +20%/yr name -> ~+1.5
            onsale_sharpe_weight: 0.0,     // (B) ON-SALE lane. ZEROED — split from growth because the shared knob conflicted: growth wants 0.15, on-sale wants 0. Validated: zeroing lifts on-sale 12y edge +39.2->+62.5 (Δ+23.3) while growth keeps 0.15. 0 = off
            sharpe_cap: 15.0,              // (B) cap the CAGR/volatility ratio (a low-vol freak can't run away with it)
            sharpe_cap_etf: 0.0,           // (#37) off by default; ci-settings ships 9.0 (line-noise ceiling for cross-listed wrappers)
            calmar_weight: 1.0,            // (C) cut further — the Calmar (CAGR/maxDD) tilt is mildly harmful in BOTH lanes on the wide sample too (Δ+0.02/+0.03); kept at 1.0 for a little long-hold drawdown-awareness. CAGR/maxDD ~0.4 for +20%/yr at -50% worst -> ~+0.4
            calmar_cap: 2.0,               // (C) cap the CAGR/max-drawdown ratio
            prefer_eur: true,
        }
    }
}

/// Every data-source URL the tool hits. Templates use `{placeholder}` tokens replaced
/// at fetch time (`{ticker}`, `{range}`, `{topic}`). Edit in settings.yaml.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)] // a typo'd key must error, not silently fall back to the default
pub struct Urls {
    pub yahoo_chart: String,   // {ticker} {range}
    pub yahoo_intraday: String, // {ticker} — hourly bars (~2d) for the screen 1h/6h/12h columns
    pub yahoo_search: String,  // {ticker}
    pub yahoo_quote: String,   // {ticker} (human quote page, for `perf`)
    // (EU listing) OpenFIGI mapping endpoint — POST, keyless, 10 jobs per request (20 returns HTTP
    // 413). Read ONLY by `prefer_eu_listing`; untouched when that knob is off. Defaulted so an older
    // settings.yaml still loads. Yahoo's own search cannot do this job: `?q=GOOGL` returns a 2x ETF
    // and options but no European venue, and `?q=AAPL` offers Thai DRs and an Argentine CEDEAR that
    // are not the same security at all.
    #[serde(default = "default_openfigi_mapping")]
    pub openfigi_mapping: String,
    pub euribor: String,
    pub us_cpi: String, // BLS CPI-U base /data/ URL (v1); seriesID + year window POSTed by fetch_us_inflation — keyless it POSTs the fresh 10y window daily plus a PERMANENT old-decade window once (merged, fills 20Y); swaps to /v2/ when BLS_API_KEY env is set (20y/call vs v1's 10y, 500 vs 25 req/day)
    pub pt_cpi: String,
    pub eu_hicp: String, // Eurostat HICP annual-rate series (COICOP-2018 successor prc_hicp_minr since Feb 2026)
    // The TERMINATED pre-2026 dataset (prc_hicp_manr, frozen at 2025-12, still served): merged
    // under the live series for its 1997-1999 tail so the 30y average keeps its full window.
    // Defaulted so an older settings.yaml without it still loads.
    #[serde(default = "default_eu_hicp_old")]
    pub eu_hicp_old: String,
    pub coingecko_markets: String, // {n} = top-N crypto by market cap -> screen universe
    pub sp500_csv: String,         // S&P 500 constituents CSV -> screen stock/ETF universe (the base equity pond)
    // (Item 18) EXTRA equity constituent CSVs in the SAME column layout (Symbol, _, GICS Sector, …) —
    // e.g. an S&P MidCap 400 or European-index CSV — APPENDED to the S&P 500 pond so the proven growth
    // ranking draws from more candidates (selection ⊆ universe; the heuristic is unchanged, so no
    // re-validation). Defaulted empty so an older settings.yaml is unchanged. Each added URL must be
    // VERIFIED-reachable and same-format (parsed by `core::sector_symbol`), else its pond silently stays
    // empty. note: no GICS-sector column -> the sector filter drops every row, so the layout matters.
    #[serde(default)]
    pub constituents_csv: Vec<String>,
    // (PIT) POINT-IN-TIME S&P 500 membership: `ticker,start_date,end_date`, one row per span, an empty
    // end date meaning "still in the index". Read ONLY by `backtest … universe pit`, never by `screen` —
    // the live screen wants today's members and nothing else. This is the survivorship cure the (#5)
    // caveat has been apologising for: `sp500_csv` above lists the ~503 names that SURVIVED to today, so
    // every backtest cutoff since 1996 has been scored on a pool chosen with hindsight.
    //
    // WHY THIS FILE AND NOT THE PUBLISHER'S OTHER ONE. The same repository ships a per-date snapshot
    // (`date,"TICKER,…"`, 2718 dates, 5.3 MB). The spans file is 27 KB and rebuilds all 2718 snapshots
    // exactly (see `core::sp500_spans`), so it is a lossless 200x-smaller substitute. It also carries the
    // dead names — AAMRQ, ABI, AHM, SBNY — which is the entire point; a survivors-only list cannot.
    // Defaulted so an older settings.yaml still loads, and swappable so the source is not welded in.
    #[serde(default = "default_sp500_history")]
    pub sp500_history: String,
    pub nupl: String,          // latest Bitcoin NUPL (net unrealized profit/loss) -> screen sentiment line
    // (#45) PER-COIN MVRV (market cap / realized cap), the generalization of `nupl` above — same
    // quantity, one row per coin instead of Bitcoin's alone (NUPL = 1 - 1/MVRV; cross-checked live at
    // BTC MVRV 1.19 -> 0.160 against the footer's 0.166). TWO endpoints because one is not enough: the
    // timeseries call 400s the ENTIRE batch if a single requested asset is unsupported, so the catalog
    // has to say which assets are allowed first. It must be `/v4/catalog/` (community tier, 125 assets)
    // and NOT `/v4/catalog-all/` — the latter lists pro-tier assets (cc, pyusd, rlusd, usde) that then
    // kill the batch. Both defaulted, so an older settings.yaml still loads and simply gets no MVRV.
    #[serde(default = "default_coinmetrics_catalog_url")]
    pub coinmetrics_catalog: String,
    #[serde(default = "default_coinmetrics_mvrv_url")]
    pub coinmetrics_mvrv: String, // {assets} = comma-separated CoinMetrics asset ids (lowercase symbols)
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
    // (TER) ETF expense-ratio source for the `ter` column. {ticker} + {key}; same opt-in/rate-limit
    // profile as `fundamentals` (FMP key only, populates at `check` scale). Stocks/crypto return no
    // expenseRatio -> column stays n/a. Defaulted so an older settings.yaml without it still loads.
    #[serde(default = "default_fund_expense_url")]
    pub fund_expense: String,
    // Börse Frankfurt / Xetra ETF search (POST) -> the EU-buyable UCITS ETF universe (US-listed
    // ETFs aren't EU-buyable). Signed with `bf_salt` lifted from their web bundle; if the API
    // moves or the salt rotates, refresh these two here — no recompile. Defaulted so older settings
    // still load.
    #[serde(default = "default_bf_etf_search_url")]
    pub bf_etf_search: String,
    #[serde(default = "default_bf_salt")]
    pub bf_salt: String,
    // justETF ETF profile page (GET, keyless HTML, `{isin}` substituted) -> TER + fund size for the
    // funds Börse Frankfurt does not list. The THIRD source for these two facts, and now the only
    // working one: BF answers most, Yahoo used to fill the holes, and Yahoo's crumb handshake is dead
    // (`fc.yahoo.com` stopped setting the session cookie). HTML, so it is scraped off two stable
    // `data-testid` anchors — see `parse_justetf_facts`, which returns None on any shape change rather
    // than guessing. Display + H/CORE only, exactly like the Yahoo fallback it stands in for: these
    // never reach the score. Defaulted so an older settings.yaml loads.
    #[serde(default = "default_justetf_profile_url")]
    pub justetf_profile: String,
    // Euronext Lisbon equities list (POST, DataTables JSON, `mics=XLIS` scopes it to Lisbon) -> the
    // Portugal `.LS` stock leg of the screen universe. The column datapoints the renderer needs are
    // sent in the request body by `fetch_euronext_lisbon`. Defaulted so an older settings.yaml loads.
    #[serde(default = "default_euronext_lisbon_url")]
    pub euronext_lisbon: String,
    // Euronext ETF list (POST, same DataTables shape as euronext_lisbon, all Euronext venues:
    // Paris/Amsterdam/Milan/Brussels/Dublin/Oslo) -> extra UCITS ISINs the Börse Frankfurt search
    // doesn't list (~660 funds). Defaulted so an older settings.yaml loads.
    #[serde(default = "default_euronext_track_url")]
    pub euronext_track: String,
    // SIX Swiss Exchange fund list (GET, plain unsigned JSON, PortalSegment=FU) -> ISINs of
    // SIX-listed funds. The segment mixes ETFs with Swiss mutual funds, so the parser keeps only
    // ETF/UCITS-named rows. Defaulted so an older settings.yaml loads.
    #[serde(default = "default_six_funds_url")]
    pub six_funds: String,
    // ESMA + FCA FIRDS registries (GET, plain JSON, no auth): the EU/UK regulators' full instrument
    // reference dumps. Each query returns the download links of the weekly FULINS_C (fund-class)
    // zip files; fetch_regulatory_etf_isins downloads the newest, scans the XML for CFI CE* rows
    // with an ETF/UCITS name, and feeds the ISINs into the ETF universe. Defaulted so an older
    // settings.yaml loads.
    #[serde(default = "default_esma_firds_url")]
    pub esma_firds: String,
    #[serde(default = "default_fca_firds_url")]
    pub fca_firds: String,
    // (Item 4) SEC EDGAR insider (Form 4) source for the `insider_net_buys_90d` factor — free, no key,
    // but SEC requires a DESCRIPTIVE User-Agent or it 403s. Only hit (cached) under `backtest … insider`.
    // {cik} = 10-digit zero-padded. Defaulted so an older settings.yaml still loads.
    #[serde(default = "default_sec_ticker_cik_url")]
    pub sec_ticker_cik: String,
    #[serde(default = "default_sec_submissions_url")]
    pub sec_submissions: String,
    // (report) SEC XBRL company-facts: every us-gaap concept (revenue/grossProfit/operatingIncome/
    // netIncome/EPS) with filingDate — the FREE, no-key, no-daily-cap fallback when FMP is throttled.
    #[serde(default = "default_sec_companyfacts_url")]
    pub sec_companyfacts: String,
    // (ratios) SEC XBRL single-concept endpoint — one us-gaap concept's full history, TINY vs companyfacts.
    // Used to roll a TRAILING-TWELVE-MONTH diluted EPS for the live P/E (the last 10-K annual EPS goes
    // stale the moment a fast grower reports a quarter). {cik} 10-digit zero-padded, {concept} = us-gaap tag.
    #[serde(default = "default_sec_companyconcept_url")]
    pub sec_companyconcept: String,
    // SET THIS to a real "app contact@email" — SEC blocks generic/empty agents. Placeholder works in dev
    // but be a good citizen before any wide run.
    #[serde(default = "default_sec_user_agent")]
    pub sec_user_agent: String,
}

/// (Item 4) Default SEC ticker→CIK map (one fetch, cached): a JSON object of {cik_str, ticker, title}.
fn default_sec_ticker_cik_url() -> String {
    "https://www.sec.gov/files/company_tickers.json".to_string()
}

/// (Item 4) Default SEC submissions endpoint: `filings.recent` carries parallel form/accessionNumber/
/// primaryDocument/filingDate arrays; we filter form == "4". {cik} = 10-digit zero-padded.
fn default_sec_submissions_url() -> String {
    "https://data.sec.gov/submissions/CIK{cik}.json".to_string()
}

/// (report) Default SEC XBRL company-facts endpoint: one JSON with every reported us-gaap concept's
/// full history (val + period end + filingDate). {cik} = 10-digit zero-padded.
fn default_sec_companyfacts_url() -> String {
    "https://data.sec.gov/api/xbrl/companyfacts/CIK{cik}.json".to_string()
}

/// (ratios) Default SEC XBRL single-concept endpoint: one us-gaap concept's full period history — used
/// to compute a trailing-twelve-month EPS for the live P/E. {cik} = 10-digit zero-padded, {concept} tag.
fn default_sec_companyconcept_url() -> String {
    "https://data.sec.gov/api/xbrl/companyconcept/CIK{cik}/us-gaap/{concept}.json".to_string()
}

/// (Item 4) Default SEC User-Agent. SEC's fair-access policy needs a descriptive contact; replace the
/// email with your own before running wide.
fn default_sec_user_agent() -> String {
    "folioman/0.1 (contact: set-your-email@example.com)".to_string()
}

/// Default Euronext Lisbon equities endpoint: the live DataTables JSON scoped to the Lisbon MIC
/// (XLIS). POSTed (with the column datapoints) by `fetch_euronext_lisbon` -> Yahoo `.LS` tickers.
fn default_euronext_lisbon_url() -> String {
    "https://live.euronext.com/en/pd_es/data/stocks?mics=XLIS".to_string()
}

/// Default Euronext ETF-list endpoint ("track" = their name for ETFs/ETPs), all Euronext MICs.
/// POSTed page-by-page by `fetch_euronext_etf_isins` -> extra ISINs for the ETF universe.
fn default_euronext_track_url() -> String {
    "https://live.euronext.com/en/pd_es/data/track".to_string()
}

/// Default SIX fund-list endpoint: the delayed-quotes JSON scoped to the fund segment, one page
/// covers the whole list (3120 rows < 5000). Fetched by `fetch_six_etf_isins`.
fn default_six_funds_url() -> String {
    "https://www.six-group.com/fqs/snap.json?select=ISIN,ShortName&where=PortalSegment=FU&pagesize=5000".to_string()
}

/// Default ESMA FIRDS registry query: newest FULINS_C (fund-class) full-instrument files, sorted
/// by publication date. Solr JSON; docs[].download_link points at the ~3MB weekly zip.
fn default_esma_firds_url() -> String {
    "https://registers.esma.europa.eu/solr/esma_registers_firds_files/select?q=file_name:FULINS_C_*&rows=5&sort=publication_date%20desc&wt=json".to_string()
}

/// Default FCA (UK) FIRDS registry query: same FULINS_C files for UK venues post-Brexit —
/// this is what covers LSE-only funds. Elasticsearch JSON; hits.hits[]._source.download_link.
fn default_fca_firds_url() -> String {
    "https://api.data.fca.org.uk/fca_data_firds_files?q=FULINS_C&from=0&size=100".to_string()
}

/// Default archive URL for the terminated pre-2026 Eurostat HICP dataset (see `Urls.eu_hicp_old`).
/// (PIT) Default point-in-time S&P 500 membership source. Ordinary raw-file GET, no key, no rate limit,
/// cached to `.sp500_history.json` after the first read.
fn default_sp500_history() -> String {
    "https://raw.githubusercontent.com/fja05680/sp500/master/sp500_ticker_start_end.csv".to_string()
}

fn default_eu_hicp_old() -> String {
    "https://ec.europa.eu/eurostat/api/dissemination/statistics/1.0/data/prc_hicp_manr?format=JSON&lang=EN&coicop=CP00&geo=EU27_2020".to_string()
}

/// Default (E) fundamentals endpoint: FMP's free `stable/quote` (carries `pe`). The old v3
/// `/api/v3/quote/` legacy endpoint died 2025-08-31 for new keys; `stable` is the replacement.
fn default_fundamentals_url() -> String {
    "https://financialmodelingprep.com/stable/quote?symbol={ticker}&apikey={key}".to_string()
}

/// (#45) Which assets the FREE tier serves `CapMVRVCur` for (125 of them). Queried before the values
/// because the timeseries endpoint rejects a whole batch over one unsupported asset. `catalog-all`
/// would list pro-tier assets too and defeat the purpose — the path is deliberately `catalog`.
fn default_coinmetrics_catalog_url() -> String {
    "https://community-api.coinmetrics.io/v4/catalog/metrics?metrics=CapMVRVCur".to_string()
}

/// (#45) Daily MVRV for `{assets}`. No key — CoinMetrics' community tier is open, and this is one
/// request for the entire crypto lane.
///
/// `limit_per_asset=1` is NOT an optimization, it is the only reason this returns anything useful.
/// The series is served oldest-first from 2011-12-31 and the endpoint has no `reverse` parameter
/// (asking for one 400s: "Unsupported parameter 'reverse'"), so without the limit `page_size` is
/// spent on 2012 rows and every current value falls off the far end of the page — measured, and it is
/// exactly how BTC came back `n/a` on the first live run. With it, each asset yields one row: its
/// LAST, which for a still-covered asset is today's. For a dropped one it is the day coverage ended,
/// which is why `fetch_mvrv` re-checks each row's date before believing it.
fn default_coinmetrics_mvrv_url() -> String {
    "https://community-api.coinmetrics.io/v4/timeseries/asset-metrics?assets={assets}&metrics=CapMVRVCur&frequency=1d&limit_per_asset=1&page_size=1000".to_string()
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

/// Default (TER) ETF expense-ratio endpoint: FMP `stable/etf/info` (carries `expenseRatio` as a
/// FRACTION, e.g. 0.0003 = 0.03%). Free-tier reachable; returns nothing for stocks/crypto.
fn default_fund_expense_url() -> String {
    "https://financialmodelingprep.com/stable/etf/info?symbol={ticker}&apikey={key}".to_string()
}

/// Default Börse Frankfurt ETF search (POST, turnover-sorted UCITS list).
fn default_bf_etf_search_url() -> String {
    "https://api.boerse-frankfurt.de/v1/search/etp_search".to_string()
}

/// Default justETF profile page, `{isin}` substituted. The English locale on purpose: the anchors
/// `parse_justetf_facts` reads are locale-independent `data-testid`s, but the AUM magnitude suffix
/// ("m"/"bn") is not.
fn default_justetf_profile_url() -> String {
    "https://www.justetf.com/en/etf-profile.html?isin={isin}".to_string()
}

/// Default OpenFIGI mapping endpoint — Bloomberg's keyless open identifier service, the only source
/// probed that answers "which European line is this US ticker" correctly and consistently.
fn default_openfigi_mapping() -> String {
    "https://api.openfigi.com/v3/mapping".to_string()
}

/// Default request-signing salt, lifted from the Börse Frankfurt web bundle (`tracing.salt`). Public
/// (it ships in their client JS), rotates occasionally — refresh from the bundle if `screen`'s ETF
/// table empties. note: a value that lives in config so a rotation needs an edit, not a rebuild.
fn default_bf_salt() -> String {
    "af5a8d16eb5dc49f8a72b26fd9185475c7a".to_string()
}

/// Locate `config/settings.yaml` next to the exe or up the tree, mirroring the old
/// Python "run from any cwd" behaviour. Falls back to `config/settings.yaml` relative
/// to the current directory.
/// Step 0 of `settings_path`, split out so it is testable WITHOUT mutating process-global env. The
/// test that used to cover this called `std::env::set_var`, which every concurrently-running test
/// that reaches `load()` races — and once one did, pointing `FOLIOMAN_CONFIG` at a bogus path to
/// simulate CI stopped working, because this test overwrote it mid-run. That is not hypothetical: it
/// is why a config-less breakage reached CI twice. Empty = ignored, so `FOLIOMAN_CONFIG=` forces the
/// discovery walk rather than resolving to `""`.
fn override_path(var: Option<&str>) -> Option<PathBuf> {
    var.filter(|p| !p.is_empty()).map(PathBuf::from)
}

fn settings_path() -> PathBuf {
    // 0. explicit override: CI points this at a checked-in fixture (tests/ci-settings.yaml) because the
    //    real config/settings.yaml is gitignored and absent there. Empty = ignore.
    if let Some(p) = override_path(std::env::var("FOLIOMAN_CONFIG").ok().as_deref()) {
        return p;
    }
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

/// Locate the committed canonical base `tests/ci-settings.yaml` — the shared tuning that BOTH the CI gate
/// and the private `config/settings.yaml` inherit, so neither has to duplicate `buy_heuristic`. Walk the
/// exe dir then the cwd, like `settings_path`. Empty PathBuf if absent (installed layout has no tests/ dir)
/// -> `load` then uses the overlay alone (identical to the pre-merge behaviour).
fn ci_settings_path() -> PathBuf {
    let starts = [
        std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)),
        std::env::current_dir().ok(),
    ];
    for start in starts.into_iter().flatten() {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let cand = d.join("tests/ci-settings.yaml");
            if cand.is_file() {
                return cand;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    PathBuf::new()
}

/// (tests round 4) Last live drift-net run, unix seconds, written by tests/network.rs whenever
/// the opted-in probe family executes. `screen` reads it to nag when the nets go stale — pub
/// (not pub(crate)) because the integration-test crate is the writer.
pub const NET_STAMP_FILE: &str = ".folioman_net_stamp";

/// Anchor for the working dot-files (caches, dedup state, the track journal): the repo root —
/// the directory holding the config — NOT the process cwd. Cron starts in $HOME, and any run
/// from another directory used to scatter diverged copies of every cache there (seen live:
/// ~287K across 5 cache files inside a sibling repo, with keyed FMP data refetched because the
/// split copy missed it). Tries the private overlay first, then the committed CI fixture (so
/// config-less checkouts like CI anchor too); a bare binary with no repo keeps the old
/// cwd-relative behaviour.
pub fn data_path(name: &str) -> PathBuf {
    // Under `cargo test --lib` this ALWAYS re-roots into a scratch dir (see `test_data_root`), so a
    // unit test that reaches a caching fetcher cannot write a dot-file into the working tree. In
    // every other build it is a `None` that inlines away. The real probe is `anchored_path`, which
    // the anchor test calls directly — testing `data_path` there would grade the override instead.
    if let Some(p) = test_root_override(name) {
        return p;
    }
    anchored_path(name)
}

/// The anchor probe itself, split from [`data_path`] so it stays reachable under test.
fn anchored_path(name: &str) -> PathBuf {
    for cfg in [settings_path(), ci_settings_path()] {
        if cfg.is_file() {
            if let Some(root) = cfg.parent().and_then(Path::parent) {
                if !root.as_os_str().is_empty() {
                    return root.join(name);
                }
            }
        }
    }
    PathBuf::from(name)
}

/// Deep-merge `over` INTO `base`: for two mappings, recurse key-by-key (so a partial `buy_heuristic:`
/// override only replaces the knobs it names); a null overlay value overrides NOTHING; for anything
/// else, `over` wins outright.
fn merge_yaml(base: &mut serde_yaml::Value, over: serde_yaml::Value) {
    match (base, over) {
        (serde_yaml::Value::Mapping(b), serde_yaml::Value::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge_yaml(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        // A bare `key:` with nothing under it — every knob commented out — names no knobs, so by the
        // contract above it replaces no knobs. Without this arm null is "anything else" and wins
        // outright, deleting the whole base subtree: on 2026-08-03 a commented-out `buy_heuristic:`
        // in the overlay swapped all ~200 tuned gates for the code defaults (min_cagr 19->8, PEG/maxdd/
        // vol caps OFF) and ranked a plausible-looking table off them, with no warning anywhere.
        (_, serde_yaml::Value::Null) => {}
        (b, o) => *b = o,
    }
}

/// The canonical base mapping to overlay onto. Empty when the overlay IS the fixture (CI sets
/// `FOLIOMAN_CONFIG=tests/ci-settings.yaml` -> pure fixture, no self-merge) or when no base file exists.
fn ci_base_yaml(overlay: &Path) -> serde_yaml::Value {
    let empty = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    if overlay.ends_with("ci-settings.yaml") {
        return empty;
    }
    let base = ci_settings_path();
    if base.as_os_str().is_empty() {
        return empty;
    }
    std::fs::read_to_string(&base)
        .ok()
        .and_then(|t| serde_yaml::from_str(&t).ok())
        .unwrap_or(empty)
}

/// Overlay the discovered config file ON TOP of the committed canonical base (`tests/ci-settings.yaml`),
/// overlay winning field-by-field. So `config/settings.yaml` only needs to carry what it CHANGES —
/// secrets (`ntfy_topic`), the watchlist, CI-specific values it must override (`universe_size`), and any
/// knob under test — instead of duplicating the whole `buy_heuristic`. `None` if the overlay is
/// absent/invalid (soft callers fall back to defaults; `load` turns `None` into a panic).
fn merged_config() -> Option<serde_yaml::Value> {
    let overlay_path = settings_path();
    let text = std::fs::read_to_string(&overlay_path).ok()?;
    let overlay: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let mut merged = ci_base_yaml(&overlay_path);
    merge_yaml(&mut merged, overlay);
    Some(merged)
}

/// The tuned gates ARE the product. `BuyHeuristic` derives `Default`, so an absent/null/empty
/// `buy_heuristic` deserializes CLEAN into the code defaults — `growth_min_cagr` 8.0, the PEG/maxdd/
/// vol caps all 0.0 = off — and ranks a plausible-looking table off an unconfigured heuristic. That
/// is not a degraded result, it is a wrong one, so `load` refuses rather than warns.
fn gates_configured(merged: &serde_yaml::Value) -> bool {
    merged.get("buy_heuristic").and_then(|v| v.as_mapping()).is_some_and(|m| !m.is_empty())
}

/// A startup config problem, reported and exited on. NOT a panic: a typo in `settings.yaml` is the
/// user's file being wrong, and `thread 'main' panicked at` plus a backtrace hint reads as a crash
/// in the tool — it sends the reader to the source instead of to line 12 of their YAML.
///
/// Diverging (`-> !`) is what lets `load` keep its infallible signature, so none of its 13 call
/// sites change. A `Result` here would be an abstraction with exactly one implementation: every one
/// of those callers is a command entry point that could only print this and exit.
fn config_fatal(path: &Path, detail: &str) -> ! {
    eprintln!("folioman: cannot use config {}", path.display());
    eprintln!("  {detail}");
    eprintln!("  fix that file, or point FOLIOMAN_CONFIG at a self-contained one");
    std::process::exit(1);
}

/// Read + parse the settings (base + overlay). Exits 1 with a clear message if missing/invalid —
/// config errors are a startup problem the user must fix, not something to fail soft on.
pub fn load() -> Settings {
    let path = settings_path();
    let Some(merged) = merged_config() else {
        config_fatal(&path, "unreadable, or not valid YAML");
    };
    if !gates_configured(&merged) {
        config_fatal(
            &path,
            "resolved to NO buy_heuristic — every gate would silently fall back to its code \
             default (growth_min_cagr 8.0, PEG/maxdd/vol caps off) and the ranking would be junk. \
             Either tests/ci-settings.yaml is unreachable from here (it is the canonical base; run \
             from the repo, or point FOLIOMAN_CONFIG at a self-contained config), or the overlay \
             carries a bare `buy_heuristic:` with every knob commented out — delete that line, or \
             give it a knob.",
        );
    }
    let s = match serde_yaml::from_value(merged) {
        Ok(s) => s,
        Err(e) => config_fatal(&path, &format!("invalid over tests/ci-settings.yaml: {e}")),
    };
    // (round 113) QA tripwire: the measured edge lives in specific validated knob values, and the
    // overlay wins the merge silently — a leftover experiment serves an UNVALIDATED ranking with no
    // trace. Name every moved knob once per process, on stderr so tables and receipts stay clean.
    use std::sync::OnceLock;
    static TRIPWIRE: OnceLock<()> = OnceLock::new();
    TRIPWIRE.get_or_init(|| {
        if let Some(w) = heuristic_drift() {
            eprintln!("{w}");
        }
    });
    s
}

/// (round 113) The off-validated warning for the loaded overlay, if any. The validated baseline =
/// code defaults overlaid by the committed `tests/ci-settings.yaml` knobs — the exact config the
/// backtest receipts graded. None when the overlay IS the fixture (CI), names no `buy_heuristic`
/// knobs, or matches the baseline value-for-value. Deliberate experiments still work — they're just
/// named. Display-only, never changes behaviour.
fn heuristic_drift() -> Option<String> {
    let path = settings_path();
    if path.ends_with("ci-settings.yaml") {
        return None; // the fixture IS the validated baseline
    }
    let over: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    let over_bh = over.get("buy_heuristic")?.as_mapping()?;
    let serde_yaml::Value::Mapping(mut base) = serde_yaml::to_value(BuyHeuristic::default()).ok()? else {
        return None;
    };
    if let Some(serde_yaml::Value::Mapping(cb)) = ci_base_yaml(&path).get("buy_heuristic").cloned() {
        for (k, v) in cb {
            base.insert(k, v);
        }
    }
    let drift = drift_lines(&base, over_bh);
    (!drift.is_empty()).then(|| {
        format!(
            "WARNING: buy_heuristic off-validated in {} (vs tests/ci-settings.yaml + defaults): {}",
            path.display(),
            drift.join(", ")
        )
    })
}

/// (round 113) Pure knob diff: every overlay key whose value differs from the validated baseline,
/// as `key base->loaded`. Integer/float spellings of the same number (5 vs 5.0) are NOT drift.
fn drift_lines(base: &serde_yaml::Mapping, over: &serde_yaml::Mapping) -> Vec<String> {
    let fmt = |v: &serde_yaml::Value| serde_yaml::to_string(v).unwrap_or_default().trim().to_string();
    let eq = |a: &serde_yaml::Value, b: &serde_yaml::Value| match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    };
    over.iter()
        .filter(|(k, v)| base.get(k).is_none_or(|b| !eq(b, v)))
        .map(|(k, v)| {
            let from = base.get(k).map(&fmt).unwrap_or_else(|| "?".to_string());
            format!("{} {from}->{}", fmt(k), fmt(v))
        })
        .collect()
}

/// (Item 21) Process-once read of the adjusted-close probe flag. SOFT — a missing/invalid config
/// yields false (the validated raw-close default), so it never panics in unit tests where the
/// gitignored settings.yaml is absent. `parse_chart` reads this to prefer Yahoo adjclose; flipping it
/// requires a full `backtest universe` re-validation + gate re-sweep (see `BuyHeuristic`).
pub fn use_adjusted_close() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.use_adjusted_close)
            .unwrap_or(false)
    })
}

/// (splice) free accessor twin of `use_adjusted_close` — `parse_chart` and the merged-series seam
/// have no `tuning` handle, and the value must be identical for live + backtest (train==serve).
pub fn splice_max_weekly_rate() -> f64 {
    use std::sync::OnceLock;
    static RATE: OnceLock<f64> = OnceLock::new();
    *RATE.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.splice_max_weekly_rate)
            .unwrap_or(0.0)
    })
}

/// (#102) free accessor twins for the CORE admission rule. `core::hold_miss_reason` and `geo_tier`
/// take a `&Quote` / a `&str` and nothing else — `is_broad_index_name` is called from the fund
/// funnels, the CORE shortlist and the H-flag column, none of which hold a `tuning`, and threading
/// one through all of them to carry three display knobs would be the larger change.
pub fn hold_name_tokens() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.hold_name_tokens)
            .unwrap_or(false)
    })
}

/// (#102) see [`hold_name_tokens`].
pub fn hold_ucits_or_domicile() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.hold_ucits_or_domicile)
            .unwrap_or(false)
    })
}

/// (#102) see [`hold_name_tokens`]. The printed reject reason formats this same number, so the cap
/// and the message it quotes cannot drift apart.
pub fn hold_max_ter() -> f64 {
    use std::sync::OnceLock;
    static CAP: OnceLock<f64> = OnceLock::new();
    *CAP.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.hold_max_ter)
            .unwrap_or(0.25)
    })
}

/// (#99) free accessor twin — `picks::life_leg_cagr` takes only a `&Quote` on purpose (its doc says a
/// second config read is how a fill site and a read site end up on different windows), and it has two
/// call sites that must stay the same expression.
pub fn gate_on_tr_cagr() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.growth_gate_on_tr_cagr)
            .unwrap_or(false)
    })
}

/// (#97) free accessor twin — `core::backtest_quote` fills `volatility_pct` and has no `tuning` handle,
/// and the LIVE path reaches the same line, so the flag must resolve identically at both.
pub fn vol_daily_equivalent() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.vol_daily_equivalent)
            .unwrap_or(false)
    })
}

/// (#95) free accessor twin of `splice_trim_point_in_time` — `stamp_asset_class` has three production
/// call sites inside the walk and no `tuning` handle at any of them.
pub fn backtest_drop_lookahead_sector() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.backtest_drop_lookahead_sector)
            .unwrap_or(false)
    })
}

/// (#94) free accessor twin of `splice_max_weekly_rate`, read at the same two sites the rate is —
/// `parse_chart` (whether to drain at all) and `core::backtest_quote` (the per-cutoff trim). Neither
/// has a `tuning` handle, and the two MUST agree or the series would be trimmed twice or not at all.
pub fn splice_trim_point_in_time() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.splice_trim_point_in_time)
            .unwrap_or(false)
    })
}

/// (#93) free accessor twin of `use_adjusted_close` — `demean` is called from 18 sites inside
/// `backtest.rs`, most of them slices of a local `Vec<Sample>` with no `tuning` in scope, and threading
/// a bool through all of them would be a wide edit for a knob that ships off.
pub fn demean_by_market() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.demean_by_market)
            .unwrap_or(false)
    })
}

/// (#3l) free accessor twin of `splice_max_weekly_rate` — `core::backtest_quote` and the fetch
/// enrich have no `tuning` handle, and the value must be identical for live + backtest (train==serve).
pub fn life_cagr_max_years() -> f64 {
    use std::sync::OnceLock;
    static YEARS: OnceLock<f64> = OnceLock::new();
    *YEARS.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.life_cagr_max_years)
            .unwrap_or(0.0)
    })
}

/// (history_proxy) Process-once read of the young-listing -> older-twin map. Read once per quote in
/// the screen fan-out, so parse the config ONCE, not thousands of times. SOFT — a missing/invalid
/// config yields an empty map (no splices), so it never panics in unit tests where the gitignored
/// settings.yaml is absent.
pub fn history_proxy() -> &'static BTreeMap<String, String> {
    use std::sync::OnceLock;
    static MAP: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.history_proxy)
            .unwrap_or_default()
    })
}

/// (Item 22) Process-once read of the fundamentals source ("fmp" | "sec") for the ranking fund lane.
/// SOFT — a missing/invalid config yields "fmp" (the validated default), so it never panics in unit
/// tests where the gitignored settings.yaml is absent. `fetch_fundamentals_ranked` reads this to route
/// BOTH the backtest and the live enrich through one source (no train-serve skew); switching to "sec"
/// is a data-source change that needs a `backtest <set> fund` re-validation (see `BuyHeuristic`).
pub fn fund_source() -> String {
    use std::sync::OnceLock;
    static SRC: OnceLock<String> = OnceLock::new();
    SRC.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.fund_source)
            .unwrap_or_else(|| "fmp".to_string())
    })
    .clone()
}

/// Process-once read of `compute_threads` for sizing the process's thread pools, WITHOUT the fatal
/// path. SOFT — a missing/invalid config yields 0, which every caller reads as "let the pool pick".
///
/// The softness is the whole point and not a convenience. `load()` calls `config_fatal` and exits 1
/// on an unreadable config, but the tokio runtime is built before `main` dispatches, and `main` must
/// still reach `help` on a broken config — the exact reason `load()` lives inside each command's `run`
/// rather than in `main` (see `commands::backtest::run`). Calling `load()` here would turn every
/// invocation, `folioman help` included, into an exit 1.
///
/// Deliberately NOT sharing `commands::backtest::thread_cap`, which encodes the same `0 = auto`
/// sentinel for the rayon pool. Its only call site sits inside `backtest::run`, a command entry with
/// no `#[mutants::skip]`, and `cargo mutants --in-diff` grades whole functions — so reusing it would
/// drag an ungradeable entry point into the mutation gate to save two lines.
///
/// Skipped because its answer is the AMBIENT config, which the `--lib` suite cannot choose: CI has no
/// `config/settings.yaml` (gitignored) and the fixture pins the knob at 0, so `replace -> 0` is
/// unkillable there rather than merely unkilled. The decision itself takes the config as an argument
/// and is graded — see [`compute_threads_of`]. Measured: without this split the gate lists exactly
/// these two mutants and MISSES the first.
#[mutants::skip]
pub fn compute_threads() -> usize {
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| compute_threads_of(merged_config()))
}

/// The knob out of an already-merged config: 0 — "let the pool size itself" — when there is no config
/// or it does not parse. Split from [`compute_threads`] purely so this half is reachable from a test.
fn compute_threads_of(cfg: Option<serde_yaml::Value>) -> usize {
    cfg.and_then(|v| serde_yaml::from_value::<Settings>(v).ok()).map_or(0, |s| s.compute_threads)
}

/// (#17/Step 4) Process-once read of the measurement-endpoint smoothing window (closes averaged for
/// the "current price" used by perf %/CAGR/range/drawdown and the hard gates). SOFT — a missing/invalid
/// config yields 1 (the raw last close, today's validated behaviour), so it never panics in unit tests
/// where the gitignored settings.yaml is absent. `core::measure_endpoint` reads this at the ONE helper
/// both the live fetch and `backtest_quote` flow through (no train-serve skew); flipping it >1 is
/// edge-affecting and needs a `backtest universe` re-validation (see `BuyHeuristic`).
pub fn endpoint_smooth_days() -> usize {
    // Unit tests assert the pure endpoint math (pct_from_high(&[100, 80, 95]) == 5 etc.) on tiny
    // synthetic arrays; the committed ci-settings value would silently average them. Tests pin the
    // inert 1; the smoothing itself is exercised by `endpoint_avg`'s own test + the backtest A/B.
    if cfg!(test) {
        return 1;
    }
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        merged_config()
            .and_then(|v| serde_yaml::from_value::<Settings>(v).ok())
            .map(|s| s.buy_heuristic.endpoint_smooth_days)
            .unwrap_or(1)
    })
}

/// Scratch anchor for `cargo test --lib`. `fetch.rs`'s caching fetchers all resolve their cache
/// through [`data_path`], which points at the repo root — so before this existed, a unit test that
/// called one wrote `.isin_cache.json` and friends into the working tree, and the only way to
/// redirect it was `std::env::set_var`. That is process-global, shared with every other test in this
/// binary, and is exactly what hid a config-less breakage twice. Hence a value, not an env var.
///
/// ONE root per process (`OnceLock`), so tests here share a directory and must use distinct
/// filenames — the ceiling, and the reason it is cheap. It lives under `target/`, which is
/// gitignored, and `CARGO_MANIFEST_DIR` is used because `CARGO_TARGET_TMPDIR` is not defined for
/// unit tests, only for integration tests (see `tests/cli.rs`).
///
/// Deliberately unconditional under test rather than opt-in: opt-in would race, because the moment
/// any test initialised the root, a test asserting the REAL anchor would start failing depending on
/// thread order. `anchored_path` is what that test targets instead.
#[cfg(test)]
static DATA_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(test)]
pub fn test_data_root() -> &'static PathBuf {
    DATA_ROOT.get_or_init(|| {
        let p = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target/lib-test-data"));
        std::fs::create_dir_all(&p).expect("create the lib-test scratch root");
        p
    })
}

#[cfg(test)]
fn test_root_override(name: &str) -> Option<PathBuf> {
    Some(test_data_root().join(name))
}

#[cfg(not(test))]
fn test_root_override(_name: &str) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Working dot-files anchor at the repo root (the dir holding the config), never the process
    /// cwd — the scatter this prevents was seen live: cron cwd=$HOME plus runs from a sibling
    /// repo left diverged cache copies outside the repo. Both test layouts (private overlay
    /// present locally, only the committed fixture in CI) must resolve to a dir with Cargo.toml.
    /// Calls `anchored_path`, NOT `data_path`: under test the latter always answers from the
    /// scratch root, so asserting on it would grade the override and never the anchor this exists
    /// to protect.
    #[test]
    fn data_path_anchors_to_repo_root() {
        let p = anchored_path(".probe.json");
        assert!(p.ends_with(".probe.json"), "{p:?}");
        let root = p.parent().expect("anchored path has a parent");
        assert!(root.join("Cargo.toml").is_file(), "expected the repo root, got {root:?}");
    }

    /// Every fs touch of a working dot-file must route through data_path — a bare relative
    /// path silently re-roots state on the process cwd (the cwd-scatter class the anchor was
    /// shipped to kill; this scan caught two live regressions on its first run: screen's
    /// .isin_cache.json read and picks' turnover-note cache). Main code only — test modules
    /// (everything after a file's first `#[cfg(test)]`) may use scratch files freely.
    #[test]
    fn working_dotfiles_anchor_through_data_path() {
        let fs_ops = ["read_to_string(", "fs::write(", "File::open(", "File::create(",
            "OpenOptions::new(", "remove_file(", "create_dir_all(", "PathBuf::from("];
        let mut stack = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
        let mut hits = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).expect("read source file");
                    let main_code = text.split("#[cfg(test)]").next().unwrap_or("");
                    for (i, line) in main_code.lines().enumerate() {
                        if line.trim_start().starts_with("//") || line.contains("data_path") {
                            continue;
                        }
                        // a `".x` string literal (dot-file) or an UPPER_CASE *_FILE/*_PATH state const
                        let dot_literal = line.as_bytes().windows(3).any(|w| {
                            w[0] == b'"' && w[1] == b'.' && (w[2].is_ascii_alphanumeric() || w[2] == b'_')
                        });
                        let state_const =
                            line.contains("_FILE") || line.contains("_PATH") || line.contains("(PATH");
                        if (dot_literal || state_const) && fs_ops.iter().any(|op| line.contains(op)) {
                            hits.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                        }
                    }
                }
            }
        }
        assert!(
            hits.is_empty(),
            "working dot-file access bypasses config::data_path:\n{}",
            hits.join("\n")
        );
    }

    /// The CI network fixture must parse into Settings (else `integration-tests` and `backtest-gate`
    /// panic for a non-API reason — the exact bug this guards), AND serde defaults must fill the omitted
    /// fields (the contract that lets a minimal / older settings.yaml load). Offline + deterministic.
    #[test]
    fn ci_fixture_parses_with_defaults() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        let s: Settings = serde_yaml::from_str(&text).expect("parse tests/ci-settings.yaml");
        // required fields present + the only URL the smoke tests exercise is a usable template
        assert!(s.urls.yahoo_chart.contains("{ticker}") && s.urls.yahoo_chart.contains("{range}"));
        assert_eq!(s.ntfy_topic, "ci-smoke-tests-unused"); // dummy, no secret
        // universe_size is set EXPLICITLY (wide pool so the backtest-gate clears its <500-ticker throttle guard)
        assert_eq!(s.universe_size, 1_234_000);
        // omitted fields still fall back to serde defaults (stale_days is not set in the fixture)
        assert_eq!(s.stale_days, default_stale_days());
        assert!(s.sectors.is_empty());
        assert!(s.urls.fundamentals.contains("financialmodelingprep")); // defaulted Urls subfield
    }

    /// (PIT) The default membership source, pinned by SHAPE rather than by host. Nothing else in the
    /// tree reads this string, so without a pin `default_sp500_history` is free to return anything at
    /// all — and the failure it would cause is silent: a URL that fetches but does not parse leaves
    /// `pit` with an empty map, which is exactly what `pit` being OFF looks like.
    #[test]
    fn default_sp500_history_points_at_the_spans_file() {
        let url = default_sp500_history();
        assert!(url.starts_with("https://"), "the membership source is fetched over TLS or not at all: {url}");
        assert!(
            url.ends_with("sp500_ticker_start_end.csv"),
            "the SPANS file — `ticker,start_date,end_date` — and NOT the same publisher's 5.3 MB per-date \
             snapshot. Both answer the same question and only this one parses with `core::sp500_spans`; \
             point this at the snapshot and `pit` silently gets an empty map, which reads as `pit` being \
             off. Got: {url}"
        );
    }

    /// (#79, rewritten at #114) What the PUBLIC PAGE publishes must stay private-clean, must carry its
    /// display shape, and must rank on the lane the receipts below it graded.
    ///
    /// (#79) asked this of `config/web-settings.yaml`, a second committed overlay that merged over this
    /// base for the Pages run. (#114) DELETED that file. It had drifted into a whole second lane —
    /// twelve `buy_heuristic` knobs, `use_life_cagr: true` among them — so the most-seen output the tool
    /// has was ranking on a config no receipt in `tests/ci-settings.yaml` had ever graded, and
    /// `web/index.html` had to disclose that in prose. Pages now runs
    /// `FOLIOMAN_CONFIG=tests/ci-settings.yaml` directly, so the page, the terminal and this suite share
    /// ONE lane and this test asks its questions of the base itself.
    ///
    /// That makes the privacy half MORE load-bearing, not less. The repo is public and so is the site,
    /// and the file under test is now the same one every receipt is written in — so a `tickers:` entry
    /// or a real `ntfy_topic` pasted in here would publish itself. These asserts are deliberately about
    /// ABSENCE.
    #[test]
    fn the_published_base_leaks_nothing_and_ranks_on_the_validated_lane() {
        // A second committed overlay is the exact regression this whole test is the tombstone for:
        // re-adding one forks the page off the graded lane again, silently, and the fork is only
        // visible to whoever reads the workflow env var.
        assert!(
            !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config/web-settings.yaml")).exists(),
            "config/web-settings.yaml is back. It was deleted at (#114) because a second committed \
             overlay is how the published page came to rank on twelve off-validated knobs. Put the \
             value in tests/ci-settings.yaml with a receipt, or leave it in the gitignored local \
             overlay — not in a second tracked config"
        );

        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml"))
            .expect("read tests/ci-settings.yaml");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&text).expect("parse the base");
        // asked of the VALUE, before it is consumed: this is the same check `load` runs, and an empty
        // `buy_heuristic` would otherwise deserialize clean into the code defaults and rank a
        // plausible-looking table off an unconfigured heuristic.
        assert!(gates_configured(&parsed), "the page must inherit the tuned gates");
        // `Settings` denies unknown fields, so this also catches a mis-nested key (`compute_threads`
        // under `buy_heuristic` is the live example) before a runner does, hours later, unwatched.
        let s: Settings = serde_yaml::from_value(parsed).expect("the published config must parse");

        // nothing that says WHOSE screen this is
        assert!(s.tickers.is_empty(), "a pin publishes a name purely because someone watches it");
        assert!(s.sectors.is_empty(), "the page shows the unfiltered ranking");
        assert_eq!(s.monthly_deploy_eur, 0.0, "how much is being invested is nobody's business");
        assert_eq!(s.ntfy_topic, "ci-smoke-tests-unused", "the base dummy — never a real topic");

        // the display shape, moved down from the deleted overlay at (#114): an explicit column list,
        // not the built-in default. Without it the page publishes the narrow layout and silently drops
        // the ETF-only columns.
        assert!(s.widths.columns.len() > 20, "the page publishes the wide terminal layout");
        assert!(s.widths.columns.iter().any(|c| c == "ter"), "the ETF lane needs its own columns");
        assert!(s.widths.name_etf > 0, "the ETF lane's own NAME width, same as the terminal's");

        // THE CAGR-BASIS PAIR, pinned by value, and pinned INVERTED from what (#79) pinned. Every knob
        // around these moves a THRESHOLD; these two pick WHICH NUMBER gets compounded, which is why
        // they are the pair a reader is least likely to notice has moved. The deleted overlay set them
        // to (true, 20) so the page would mirror a terminal that did the same. `use_life_cagr` is a
        // MEASURED LOSER — (#3j): edge +125.6 vs +167.6, rho +0.17 vs +0.20, both out-of-sample halves
        // down; (#3k) re-condemned it at all four horizons, "loses EVERY view it was graded on. Do not
        // re-propose." Turning it on here would re-propose it to the most-seen output the tool has.
        assert!(!s.buy_heuristic.use_life_cagr, "(#3j)/(#3k) condemned whole-life CAGR twice — the page ranks on the ladder leg");
        assert_eq!(s.buy_heuristic.fixed_cagr_years, 0, "…and on the 20/8/5 ladder, not a window pinned to one rung");
    }

    /// Value-pin for the RANKING-LIVE tilt knobs. The scoring pins (r59/r67) guard the CODE defaults
    /// (tilt off) and the drift tripwire treats this fixture as its BASELINE, so an edit to these
    /// values in tests/ci-settings.yaml is the one path that changes live ranks with nothing
    /// tripping. Raw-parses the fixture (no merge) so the pin covers the committed base, never the
    /// private overlay.
    #[test]
    fn validated_tilt_pin() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        let s: Settings = serde_yaml::from_str(&text).expect("parse tests/ci-settings.yaml");
        let h = &s.buy_heuristic;
        let receipts = "RANKING change: re-validate first (same-batch backtest, BOTH OOS halves \
                        positive — receipts block in tests/ci-settings.yaml), then move this pin";
        assert_eq!(h.fund_source, "sec", "{receipts}");
        assert_eq!(h.growth_fund_factor, "peg_yield", "{receipts}");
        // (#3) weight and cap are ONE dial: the score sees weight x clamp(factor, 0, cap). peg_yield
        // runs ~0-500 (100 = PEG 1) where the old earnings_yield ran ~0-15, so carrying the previous
        // 1.0/30 pair over would have shipped a tilt ~55x hotter than the sweep ever measured, under a
        // factor name that still matched its receipt. Pin BOTH so neither can drift alone.
        // (#71) 2026-08-11: graded under Ship Rule v2 for the first time — twelve `backtest {20,12,8}
        // universe fund` runs at 0 / 0.05 / 0.07 / 0.12 — and 0.07 HELD. Note the generic message above
        // does not apply to this knob: "both OOS halves positive" is the FUND LANE's criterion, and at 8y
        // that lane prints "keep growth_fund_weight 0. SHIP NOTHING." while the graded top-3 book makes
        // weight 0 the worst of the four arms (8y mean +7.1 vs +7.3, at every basket size, h2h 63%->57%).
        // Grade this one on the VERDICT_TOP book, and only on 12y+8y — 20y carries zero fundamental
        // coverage, so its ablation is D+0.00 and every arm ties there for free. (#120) moved that
        // basket 3 -> 10 and THIS RECEIPT SURVIVES THE MOVE INTACT, unlike (#75)'s: it already recorded
        // the arm ordering "at every basket size", so the finding never rested on the basket being 3.
        // Read the (#71) receipt before moving it.
        assert_eq!(h.growth_fund_weight, 0.07, "0.07 held a v2 grid — 0 fails the primary; do NOT zero this on the fund lane's own SHIP NOTHING line, which grades the lane and not the book. {receipts}");
        assert_eq!(h.growth_fund_cap, 300.0, "cap is half the tilt magnitude — {receipts}");
        // (G+) the multi-term tilt is no longer inert: (#43) ships ROIC as its first entry. The bar for a
        // non-empty list was "+rho with both OOS halves positive, same as the primary" — roic cleared it
        // on the DECISIVE instrument, the within-run ablation, paying at 0.1/0.25/0.5 (Δ-2.8/-6.2/-2.3)
        // where the rejected interest_cover paid nothing (Δ+0.0/+0.0/+5.4), plus held-book OOS +3.3/+4.3.
        // Pin factor, weight AND cap: weight × cap is ONE dial (receipt (#3)), so any of the three drifting
        // alone re-ranks the live book under a config that still matches its receipt.
        assert_eq!(h.growth_fund_extra.len(), 1, "{receipts}");
        assert_eq!(h.growth_fund_extra[0].factor, "roic", "{receipts}");
        assert_eq!(h.growth_fund_extra[0].weight, 0.25, "the measured ablation peak — {receipts}");
        assert_eq!(h.growth_fund_extra[0].cap, 40.0, "matches quality_cap — {receipts}");
        // (#72) 2026-08-12: graded under Ship Rule v2 and HELD at 0.15 — ten `backtest {8,12} universe
        // fund` runs at 0 / 0.075 / 0.15 / 0.30 plus a 20y pair. Pinned here, with the fund tilt it
        // interacts with ((#3d)), because NOTHING ELSE GUARDS IT: `backtest_fixture` runs offline and
        // never exercises `fund`, so `quote.roe` is None there and this term is inert at any weight —
        // the goldens cannot move when it does. Two things the message above gets wrong for this knob:
        // "both OOS halves positive" is a LANE criterion, and the standalone ROE probe is NEGATIVE at
        // both graded horizons (rho -0.04 / -0.03) while the graded book does not move at all; and the
        // deletion rule wants three horizons, which this term can never have (20y carries no
        // fundamentals — measured here at the extremes, not assumed). Read the (#72) receipt first: the
        // live lead is a code round on the ON-SALE lane, which currently has no graded book to judge.
        assert_eq!(h.quality_weight, 0.15, "0.15 held a v2 grid — every arm from 0 to 0.30 ties on the book, so this value is held rather than proven; do NOT zero it on the on-sale lane's ablation, which is lane edge and has been wrong five times. {receipts}");
        // (D) revived 2026-07-25. This one canNOT carry the receipt the message above demands: the
        // backtest cannot reconstruct as-of dividends, so no walk-forward run grades it at any weight
        // (see commands/backtest.rs's module header). It is pinned as a JUDGMENT lever sized by
        // argument — 0.5 peaks the term at +2.25, under quality_weight's +6 blind ceiling.
        let blind = "BACKTEST-BLIND by construction — no walk-forward receipt is possible for this \
                     term at any weight; it is sized by argument and pinned so the size can't drift \
                     silently. Re-argue it in tests/ci-settings.yaml before moving this pin";
        assert_eq!(h.dividend_weight, 0.5, "{blind}");
        // (D/PT) the two after-tax keep-fractions ARE the whole tax model and they move live ranks,
        // so they belong in the same tripwire as every other unvalidatable live tilt.
        assert_eq!(h.tax_keep_eu, 0.76, "{blind}");
        assert_eq!(h.tax_keep_other, 0.72, "{blind}");
        assert_eq!(h.growth_value_weight, 0.0, "blind tilt stays consolidated to 0 — {receipts}");
        // (#73) 2026-08-12: the knob's first pin ever, and it is here rather than with the scoring pins
        // because (#73) repointed it — it is a GATE knob now, the window on growth_min_cagr's whole-life
        // leg, so the generic message above (which talks about ranking) does not describe it. At 0.0 the
        // gate reads the uncapped lifetime and every golden is bit-identical, which is exactly why it
        // needs a pin: nothing else in the suite fails if this number moves. Graded on a 0/6/8/10/12/16/20
        // ladder, `backtest {20,12,8} universe stress`, full Ship Rule v2 — 6/8/10 fail the rank-1 h2h
        // guard, and 12/16/20 clear every guard and PRIMARY but leave rank-1 median flat at 12y and 20y,
        // which the round's pre-registered ADDITION bar refuses. Read the (#73) receipt in
        // tests/ci-settings.yaml before moving this: the lane edge DOES improve at 12 and 16, and it is
        // pre-committed non-voting for the sixth documented time.
        // Deliberately NOT carrying `{receipts}`: that message opens "RANKING change", and naming a gate
        // knob's failure after the wrong mechanism is the same defect the `cagr-life` span word avoids.
        assert_eq!(h.life_cagr_max_years, 0.0, "GATE change: HELD at 0.0 by a seven-arm v2 grid at three horizons — a window on growth_min_cagr's leg 2 never reaches the graded book. Do NOT arm this on the lane edge, and do NOT arm it on the 18 `cagr-life` sole-blocks: the grid showed the useful arms admit FEWER names, not more. Read the (#73) receipt in tests/ci-settings.yaml, then move this pin");
    }

    /// FOLIOMAN_CONFIG overrides the discovery walk (how CI points at the fixture). Asserted on the
    /// pure half, NOT by setting the variable: `set_var` is process-global, so the old version of this
    /// test raced every other test that reaches `load()` and — worse — silently overwrote the variable
    /// for anyone using it to simulate CI's config-less run. The `settings_path` -> env wiring itself
    /// stays graded end-to-end by tests/backtest_fixture.rs, which sets FOLIOMAN_CONFIG on the child it
    /// spawns and is in the mutation gate's killing suite.
    #[test]
    fn env_override_wins() {
        assert_eq!(
            override_path(Some("tests/ci-settings.yaml")),
            Some(PathBuf::from("tests/ci-settings.yaml"))
        );
        assert_eq!(override_path(Some("")), None, "empty -> ignored, falls back to discovery");
        assert_eq!(override_path(None), None, "unset -> discovery");
    }

    /// Pins the deny_unknown_fields guard: a typo'd top-level key must ERROR naming the field, never
    /// silently fall back to the default. If a refactor drops the serde attribute, this test reds.
    #[test]
    fn unknown_top_level_key_errors() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        let err = serde_yaml::from_str::<Settings>(&format!("{text}\nnot_a_real_key: 1\n"))
            .expect_err("unknown key must not parse")
            .to_string();
        assert!(err.contains("unknown field `not_a_real_key`"), "must name the field: {err}");
    }

    /// (#76) The OpenFIGI endpoint is `serde(default)`, so no committed YAML supplies it and the
    /// constant IS the shipped value — nothing else in the tree would notice it changing. Pinned
    /// through a real parse of the committed fixture rather than by calling the default fn directly,
    /// because the property that matters is what a settings file WITHOUT the key resolves to.
    ///
    /// Defaulted rather than required so an existing private settings.yaml keeps loading: `Urls` is
    /// `deny_unknown_fields` and every other field is mandatory, so a bare addition would have broken
    /// every deployed config on upgrade.
    #[test]
    fn openfigi_endpoint_defaults_to_the_live_mapping_service() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        assert!(!text.contains("openfigi_mapping"), "fixture must exercise the DEFAULT, not pin the key");
        let settings: Settings = serde_yaml::from_str(&text).expect("parse ci-settings.yaml");
        assert_eq!(settings.urls.openfigi_mapping, "https://api.openfigi.com/v3/mapping");
    }

    /// (TER/AUM) Same story as the OpenFIGI pin above, and the same reason it needs one: `serde(default)`
    /// means no committed YAML supplies this, so the constant IS the shipped value. It also carries the
    /// `{isin}` placeholder `justetf_fund_facts` substitutes — a URL that lost it would fetch the same
    /// literal page for every fund and quietly report one ETF's TER for all of them.
    #[test]
    fn justetf_endpoint_defaults_to_the_live_profile_page() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ci-settings.yaml");
        let text = std::fs::read_to_string(path).expect("read tests/ci-settings.yaml");
        assert!(!text.contains("justetf_profile"), "fixture must exercise the DEFAULT, not pin the key");
        let settings: Settings = serde_yaml::from_str(&text).expect("parse ci-settings.yaml");
        assert_eq!(settings.urls.justetf_profile, "https://www.justetf.com/en/etf-profile.html?isin={isin}");
        assert!(settings.urls.justetf_profile.contains("{isin}"), "the placeholder is load-bearing");
    }

    /// Same pin for the hand-tuned knob surface: a buy_heuristic typo must error, not become a no-op.
    #[test]
    fn typoed_buy_heuristic_knob_errors() {
        let err = serde_yaml::from_str::<BuyHeuristic>("groth_accel_weight: 0.5\n")
            .expect_err("typo'd knob must not parse")
            .to_string();
        assert!(err.contains("unknown field `groth_accel_weight`"), "must name the field: {err}");
    }

    /// (round 113) Drift tripwire semantics: a changed number or string is named `key base->loaded`,
    /// an int/float respelling of the same value is NOT drift, an unknown key shows `?` as its base,
    /// and the serialized code defaults expose real knob values for the baseline.
    #[test]
    fn drift_lines_semantics() {
        let m = |s: &str| match serde_yaml::from_str::<serde_yaml::Value>(s).unwrap() {
            serde_yaml::Value::Mapping(m) => m,
            _ => unreachable!(),
        };
        let base = m("a: 0.5\nb: 5\nc: fmp\n");
        let over = m("b: 5.0\na: 0.7\nc: sec\n");
        assert_eq!(drift_lines(&base, &over), vec!["a 0.5->0.7", "c fmp->sec"]);
        assert_eq!(drift_lines(&base, &m("b: 5\n")), Vec::<String>::new());
        assert_eq!(drift_lines(&base, &m("z: 1\n")), vec!["z ?->1"]);
        // the baseline source: code defaults serialize to a mapping with the real knob values
        let defaults = serde_yaml::to_value(BuyHeuristic::default()).unwrap();
        assert_eq!(defaults["growth_min_range_pct"].as_f64(), Some(80.0));
        // the growth 1Y floor replaced a hardcoded 0.0, so its default must BE that constant — a
        // non-zero default would silently move the live ranking the moment the knob shipped.
        assert_eq!(defaults["growth_min_1y_pct"].as_f64(), Some(0.0));
    }

    /// The overlay wins field-by-field over the base, mappings merge DEEP (a partial `buy_heuristic:` only
    /// replaces the knobs it names), new keys are added, and untouched base keys survive. This is the whole
    /// contract that lets `config/settings.yaml` carry only overrides instead of a full `buy_heuristic` copy.
    #[test]
    fn overlay_merges_deep_over_base() {
        let mut base: serde_yaml::Value =
            serde_yaml::from_str("a: 1\nb:\n  x: 10\n  y: 20\n").expect("base");
        let over: serde_yaml::Value = serde_yaml::from_str("b:\n  y: 99\nc: 3\n").expect("over");
        merge_yaml(&mut base, over);
        assert_eq!(base["a"].as_u64(), Some(1)); // untouched base key kept
        assert_eq!(base["b"]["x"].as_u64(), Some(10)); // sibling under a merged mapping kept
        assert_eq!(base["b"]["y"].as_u64(), Some(99)); // overlay scalar wins
        assert_eq!(base["c"].as_u64(), Some(3)); // new overlay key added

        // ...and the same contract read the other way: a key the overlay names but leaves EMPTY names
        // no knobs under it, so it replaces none of them. `b:` with its children commented out used to
        // delete the whole subtree — the shape that served a defaults-only ranking on 2026-08-03.
        let mut base: serde_yaml::Value =
            serde_yaml::from_str("a: 1\nb:\n  x: 10\n  y: 20\n").expect("base");
        merge_yaml(&mut base, serde_yaml::from_str("b:\nc: 3\n").expect("over"));
        assert_eq!(base["b"]["x"].as_u64(), Some(10));
        assert_eq!(base["b"]["y"].as_u64(), Some(20));
        assert_eq!(base["c"].as_u64(), Some(3)); // the rest of the overlay still applies
    }

    /// `load` refuses a config that resolves to no gates at all, in every shape that reaches it:
    /// the base file unreachable (key absent), or a bare `buy_heuristic:` in the overlay (null/empty).
    /// Serde would take all three without a word and hand back `BuyHeuristic::default()`.
    #[test]
    fn gates_configured_rejects_empty_heuristic() {
        let y = |s: &str| serde_yaml::from_str::<serde_yaml::Value>(s).expect("yaml");
        assert!(gates_configured(&y("buy_heuristic:\n  growth_min_cagr: 19.0\n")));
        assert!(!gates_configured(&y("buy_heuristic:\n"))); // bare key -> null
        assert!(!gates_configured(&y("buy_heuristic: {}\n"))); // named, but empty
        assert!(!gates_configured(&y("monthly_deploy_eur: 2200\n"))); // no base found
    }

    /// `compute_threads` is read to size the tokio runtime BEFORE `main` dispatches, so unlike every
    /// other knob it cannot go through `load()` — that exits 1 on a bad config, and `folioman help`
    /// has to keep working on one. This pins the soft half: a config that is absent or not a config
    /// at all answers 0 ("size the pool yourself") instead of exiting or panicking.
    ///
    /// The present-config arm reads the committed fixture and moves only the one key, because
    /// `Settings` is `deny_unknown_fields` with a dozen required keys — a synthetic mapping big
    /// enough to parse would pin the mapping rather than the lookup.
    #[test]
    fn compute_threads_falls_back_to_auto_instead_of_exiting() {
        let fixture = ci_settings_path();
        let text = std::fs::read_to_string(&fixture).expect("the committed fixture is readable");
        let mut cfg: serde_yaml::Value = serde_yaml::from_str(&text).expect("the fixture is YAML");
        cfg.as_mapping_mut()
            .expect("the fixture is a mapping")
            .insert("compute_threads".into(), serde_yaml::Value::from(6u64));
        assert_eq!(compute_threads_of(Some(cfg)), 6, "a configured cap must survive the read");
        assert_eq!(compute_threads_of(None), 0, "no config = let the pool pick, NOT an exit");
        assert_eq!(
            compute_threads_of(Some(serde_yaml::Value::Bool(true))),
            0,
            "a config that cannot be a Settings is the same soft 0, not a panic before main runs"
        );
    }

    /// The `#[serde(default = "…")]` fallbacks, pinned to the numbers their field comments promise.
    /// These fire only for a key the config OMITS, so the committed settings + the CI fixture between
    /// them keep most of them from ever running — which is exactly how a stubbed `default_universe_size`
    /// returning 0 would reach a user with an older settings.yaml and silently empty their universe.
    /// Called directly rather than round-tripped through YAML: `Settings`/`Urls` are
    /// `deny_unknown_fields` with a dozen required keys each, and a fixture that big would pin the
    /// fixture, not the defaults.
    #[test]
    fn serde_defaults_are_the_documented_values() {
        assert_eq!(default_top_picks(), 5);
        assert_eq!(default_stale_days(), 7);
        assert_eq!(default_universe_size(), 100);
        assert_eq!(default_fetch_concurrency_multiplier(), 8);
        assert_eq!(default_fetch_requests_per_second(), 10.0);
        assert!(default_true());
    }

    /// The URL fallbacks that exist so an older settings.yaml still loads. Pinned on host + the
    /// `{placeholder}` tokens the fetchers substitute — a default that loses `{ticker}` fetches the
    /// same page for every symbol, which reads as data rather than as an error.
    #[test]
    fn defaulted_urls_keep_their_host_and_placeholders() {
        let fundamentals = default_fundamentals_url();
        assert!(fundamentals.starts_with("https://financialmodelingprep.com/"), "{fundamentals}");
        assert!(fundamentals.contains("{ticker}") && fundamentals.contains("{key}"), "{fundamentals}");
        let quality = default_fundamentals_quality_url();
        assert!(quality.starts_with("https://financialmodelingprep.com/"), "{quality}");
        assert!(quality.contains("{ticker}") && quality.contains("{key}"), "{quality}");
        assert!(default_eu_hicp_old().contains("prc_hicp_manr"), "the TERMINATED dataset, not the live one");
        assert!(default_euronext_lisbon_url().contains("mics=XLIS"), "Lisbon MIC scope is the whole point");
    }
}
