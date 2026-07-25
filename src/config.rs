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
    #[serde(default = "default_true")]
    pub universe_prefer_eur: bool, // crypto in the live universe quoted in EUR (BTC-EUR) if true, else USD
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
    pub urls: Urls,
}

/// Toggle for showing REAL (inflation-adjusted) returns on the 1Y/5Y/10Y/20Y % columns instead of
/// nominal. Off by default. When on, deflates by the ACTUAL cumulative EU HICP inflation over each
/// horizon (fetched live, same source as the `check` footer) — no rate to guess.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)] // a typo'd knob must error, not silently fall back to the default
pub struct InflationAdjust {
    pub enabled: bool, // false = raw nominal % (default); true = deflate long-horizon returns by live EU HICP
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
        Widths { name: 26, ticker: 8, market: 11, price: 13, headline: 31, score: 5, columns: Vec::new(), column_widths: BTreeMap::new() }
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
/// and the `tax_keep_eu` / `tax_keep_other` knobs below.
/// GATES exclude a candidate outright; SCORE knobs rank the survivors. Mirrors `config/settings.yaml`.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default, deny_unknown_fields)] // a typo'd knob must error, not silently fall back to the default
pub struct BuyHeuristic {
    // --- GATES: a candidate failing ANY of these is dropped before scoring ---
    pub min_1y_pct: f64,             // [FOIL] on-sale only: reject if equity 1Y % <= this (growth uses a hardcoded 0% floor, not this)
    pub min_1y_pct_crypto: f64,      // crypto/FX (-EUR/-USD): looser 1Y floor — they swing far harder
    pub max_1m_drop_pct: f64,        // equities: reject if 1M % <= this (a hard monthly crash = falling knife)
    pub max_1m_drop_pct_crypto: f64, // crypto/FX: looser knife — a -20%/month alt is normal, not broken
    pub min_long_pct: f64,           // [FOIL] on-sale only: reject if any 5Y/10Y/20Y leg <= this (growth uses a hardcoded >0% 5Y gate)
    pub min_long_pct_crypto: f64,    // [FOIL] on-sale only: reject if the >2Y leg <= this CUMULATIVE % (a corpse, e.g. -70%+)
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
    pub long_trend_cap: f64,         // cap on that long-leg CAGR (%/yr) fed into the reward (a +50%/yr coin doesn't 5× a +10%/yr one)
    pub fixed_cagr_years: u32,       // (#15) GROWTH: pin the long-CAGR window to THIS many years (e.g. 10 -> always the 10Y leg) so every name's CAGR is measured over the SAME span; 0 = off (longest available leg, today's behaviour). Short-history names fall back to their longest leg. The pin also moves `trust_factor`'s required record to the SAME window (under an 8Y view an 8Y record IS a full record, so demanding 10Y would halve every name the view exists to judge); at 0 that stays 10Y. Edge-affecting -> validate `backtest universe` before flipping
    pub use_trend_cagr: bool,        // (#14) GROWTH: rank the long trend on the least-squares log-price SLOPE (endpoint-robust) instead of the two-point CAGR; false = off (today's endpoint CAGR). Edge-affecting -> validate `backtest universe` before flipping
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
    pub tax_keep_eu: f64,            // (D/PT) fraction of a dividend KEPT after Portuguese tax when the payer is an EU company — Art. 40.º-A CIRS englobates only 50% of dividends from an EU-resident company meeting the Parent-Subsidiary Directive conditions. HAND-SET from your own IRS position (bracket, englobamento yes/no, source withholding you actually eat): no tax law is encoded in this codebase, the two knobs ARE the model. 1.0 = off (DEFAULT — the lane is byte-identical to the pre-tax version). ponytail: ONE rate blends source withholding from 12.8% (France) to 35% (Finland) — that 22-pt within-EU spread is ~3× the EU-vs-US gap this term exists to capture, so a single number is a real approximation, not a rounding. Upgrade path if it bites: a per-market map keyed on `Quote.market`
    pub tax_keep_other: f64,         // (D/PT) same keep-fraction for EVERY other market AND for funds of any domicile: an OICVM/ETF distribution is not a Parent-Subsidiary-Directive company's *lucro*, so it draws no 50% exclusion however EU-listed the wrapper is. 1.0 = off (DEFAULT)
    pub ref_pe: f64,                 // (E) "fair" trailing P/E: value tilt = ref_pe/PE, clamped — cheap (<ref) lifts, rich (>ref) dampens; no PE = neutral
    pub quality_weight: f64,         // (F) reward per % of trailing return-on-capital — the profitability/QUALITY factor (Novy-Marx: high-ROE firms out-compound). ROE, or ROA where equity is negative (`core::quality_return`). Applied to BOTH lanes. 0 = off. Was BACKTEST-BLIND until the backtest loop started filling `Quote.roe` — every number measured for it before then was taken with the term at a constant 0 (see #F)
    pub quality_cap: f64,            // (F) cap the % fed into the quality reward (one 200%-ROE outlier can't dominate). NOTE it binds on ROE (30-50% typical) far more often than on ROA (5-15%), so a negative-equity filer can never reach the ceiling

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
    pub growth_min_score_etf: f64,   // growth SCORE: ETF-only display floor. ETFs structurally cap ~5.6 (accel/quality/liq/fund terms all ~0 for a diversified basket) vs stocks ~19, so the shared growth_min_score sits at ~89% of the ETF ceiling and guillotines them. Separate, lower floor = ETF lane trimmed proportional to ITS score distribution. DISPLAY-only (print_lane, not the backtest) -> edge-blind by construction. 0 = show all
    pub growth_overext_cap: f64,     // (1) % ABOVE the 200wk SMA at which the overextension brake maxes out
    pub growth_max_above_ma: f64,    // hard gate: reject equities more than this % above the 200wk SMA — the extreme blow-off cohort the overext brake can only floor, not remove. VALIDATED at 150 (see ci-settings.yaml); 0 = off. Crypto exempt (rides far above its SMA normally; its brake cap handles it)
    pub growth_require_lifetime_uptrend: bool, // (#25) hard gate: reject equities whose WHOLE-LIFE log-trend CAGR (quote.trend_cagr, full history) is <= 0 — moon-crash-partial-recovery names whose 5Y/10Y legs look great but never reclaimed their long-run trend. false = off (default). Crypto exempt (young coins; the range gate handles bled ones)
    pub growth_maxdd_cap: f64,       // (#26) hard gate: reject equities whose worst-ever drawdown MAGNITUDE exceeds this % (e.g. 83 rejects a name that ever fell >83% peak-to-trough). 0 = off (default)
    pub growth_maxdd_cap_crypto: f64, // (#26) crypto's OWN maxdd cap. Coins crash >90% every cycle, so the equity cap would gate Bitcoin itself (-83%); set this just ABOVE BTC's mark (84) to mean "reject coins that wiped out worse than Bitcoin". 0 = off (default)
    pub growth_max_vol_crypto: f64,  // (#36) crypto-only hard gate: reject coins whose daily-return stdev (quote.volatility_pct, the VOL column) exceeds this % — "not meaningfully wilder than Bitcoin" (BTC ~2.4%/day; 3.0 gives the base headroom). Crypto is absent from the backtest pool -> gate edge-blind by construction; equities never reach multi-% daily stdev so no equity twin. 0 = off (default)
    pub growth_min_age_years: f64,   // (#33) hard gate: reject a name younger than this many years (quote.age_years, the YRS column = whole-life listing age). A "20yr+ proven CAGR" candidate must actually HAVE a multi-year record. BACKTEST-BLIND (age_years is None in the backtest pool -> gate inert there, validated edge untouched); bites only the live screen. Pins bypass it (shown with a "young" gate-review reason). All classes. 0 = off (default)
    pub growth_min_aum_etf: f64,     // (AUM) ETF-only hard gate: reject a fund smaller than this (quote.aum_eur, EUR-approx from the BF universe payload). Sub-€100M funds get liquidated/merged — a forced taxable exit is exactly what a 20y hold must avoid. BACKTEST-BLIND (aum_eur None in the backtest pool -> gate inert, validated edge untouched); None-AUM names are NOT gated (missing data != small fund). 0 = off (default); ci-settings ships 100_000_000
    pub growth_ter_drag: bool,       // (#34) ETF cost drag: dock the growth score by the ACTUAL 20-year wealth multiple the expense ratio eats, (1-TER)^20. TER is the one cost certain to compound against a decades hold, so two near-identical index ETFs (e.g. 2× Nasdaq-100) rank by NET return, not by momentum noise. ETF-ONLY (expense_ratio is None for stocks/crypto -> ×1.0). BACKTEST-BLIND (expense_ratio None in the backtest pool -> ×1.0 there, validated edge byte-identical); shapes only the live ETF lane. false = off (default, byte-identical to the pre-(T) lane)
    pub growth_overext_floor: f64,   // (1) growth-score multiplier at that cap (e.g. 0.4 = a fully-stretched name keeps 40% of its score); 1.0 = brake off
    pub growth_turnover_weight: f64, // (L) liquidity tilt: bonus per ln(turnover/€1B), added OUTSIDE the brake. Rewards deep-liquid mega-caps (easy multi-decade exit, less manipulation) so a proven compounder like NVDA isn't ranked below an illiquid €200M twin on a score tie. RANK-NEUTRAL in backtest: backtest_quote sets a uniform sentinel turnover (#20) so this bonus is a constant offset on every name -> never moves the validated edge; 0 = off
    pub growth_overext_cap_crypto: f64, // (#4) crypto's OWN overextension cap (% above the 200wk SMA at which the brake maxes). Crypto routinely rides far above its long SMA, so a separate looser cap avoids over-braking coins; equities/ETFs keep growth_overext_cap. 0 = crypto brake off
    pub growth_fund_weight: f64,     // (G) reward per pt of the as-of FUNDAMENTAL factor (see growth_score / fund_factor). The fund lane proves WHICH as-of factor predicts forward returns standalone; this folds it INTO growth_score so its through-the-lane edge is ablatable. 0 = off (DEFAULT, no behavior change). Validate via `backtest <set> fund` then set the weight only on +ablation-Δ + both-half-positive OOS
    pub growth_fund_cap: f64,        // (G) cap (in the factor's own pts) on the fund factor fed into that reward, so one data-artifact (+9000% rev) can't dominate the rank
    pub growth_fund_factor: String,  // (G) WHICH as-of FundFactors term the fund tilt weighs: rev_cagr | rev_accel | gross_margin | op_margin | margin_trend | eps_growth. Set to whichever the `backtest <set> fund` probe shows +rho + both-half-positive OOS — no recompile. Unknown name -> neutral. Default "rev_accel" preserves the prior hardcoded behavior
    pub fund_source: String,         // (Item 22) WHICH fundamentals feed BOTH the `backtest <set> fund` lane AND the live `screen`/`check` fund tilt: "fmp" (DEFAULT, unchanged — global coverage, quarterly, 250-call/day cap, needs FMP_API_KEY) | "sec" (SEC EDGAR XBRL — free, no key, no daily cap, ~19y annual history, US filers only). The SAME source feeds backtest + live (one router) so the validated and served signal can't drift (train-serve skew). Switching to "sec" is a DATA-SOURCE change: re-run `backtest <set> fund` to re-validate the factor on SEC's annual rows BEFORE raising growth_fund_weight — the FMP-validated weight does NOT carry over. Unknown value -> "fmp". With weight 0 (default) this is inert either way.
    pub growth_mom121_weight: f64,   // (M) reward per pt of 12-1 momentum (trailing 1Y return EX the last 1mo — Jegadeesh-Titman, skips the short-term-reversal month). Price-only, so unlike the BACKTEST-BLIND div/ROE/fund tilts this one IS validated end-to-end (backtest_quote reconstructs 1Y/1M). 0 = off (DEFAULT, no behavior change). Raise only on +ablation-Δ + both-half-positive OOS via `backtest <set>` / `tune`
    pub growth_mom121_cap: f64,      // (M) cap (in pct pts) on the 12-1 momentum fed into that reward, so one moonshot can't dominate the rank
    pub growth_smoothness_weight: f64, // (E) additive reward per unit of trend_r2 (R² of the log-price trend fit, 0..1; 0 also = no history) — pays names whose long climb is a straight line over equal-CAGR rollercoaster. Price-only and reconstructed in backtest_quote -> validated end-to-end: 2/5/10/20 sweep peaked at 5 (Δedge +13.2 same-batch, rho intact; 20 collapsed). 0 = off (DEFAULT); ci-settings ships 5.0
    pub growth_underwater_weight: f64, // additive PENALTY per year of the longest below-prior-peak stretch (core::longest_underwater_yrs — the drawdown-DURATION twin of the maxdd depth cap). Price-only, reconstructed in backtest_quote on the daily cadence -> validated end-to-end. Standalone probe (backtest universe fund, 2026-07-19): n=8571 rho +0.26 edge +27.9 OOS +0.40|+0.14 both +; candidate lane run at 0.3 same-batch: edge +25.5->+27.5, rho intact, both OOS halves + worst era improved. 0 = off (DEFAULT); ci-settings ships the validated 0.3
    pub growth_value_weight: f64,    // (Item 20) authority of the BACKTEST-BLIND P/E multiplier (value_factor) in the GROWTH lane only: 1.0 = full ×0.5..1.5 swing (DEFAULT, unchanged), 0.0 = neutral (off). The validated edge was measured with this term OFF (pe_ratio is None in backtest), so it's a ±50% reorder the OOS split never saw — dial it down/off once the validated additive earnings_yield term (Item 19) carries valuation instead. Defaulted so an older settings.yaml is unchanged.
    pub growth_geomean_fold: bool,   // (#8) PROBE switch: fold proximity + value INTO the geomean damp instead of multiplying them raw onto base. Today score = base × proximity × value × geomean(trust, overext); THREE soft multipliers stack unbounded (a name at 0.7 × 0.8 × 0.85 keeps only 0.48 of base — nearly halved by three "slightly-off" signals). true = base × geomean(trust, overext, proximity, value): the SOFTEST term bounds the penalty (combine_damps), so no single soft signal dominates. Edge-affecting — it reshapes the live rank AND changes the geomean SLOT COUNT (the trust/overext exponent shifts from ½ to ¼), so DEFAULT false = unchanged; flip ONLY behind a green `backtest universe` with both OOS halves positive. Golden-rule-gated.
    pub use_adjusted_close: bool,    // (Item 21) PROBE switch: when true, parse_chart prefers Yahoo's adjclose (split+DIVIDEND adjusted) over raw close, so the long CAGR / range_pct near-high gate / drawdown / overext brake measure TOTAL return instead of price-only — fixes dividend-compounders that are mis-ranked (CAGR understates total return) or mis-EXCLUDED (nominal price below old high fails growth_min_range_pct). Flows to BOTH live + backtest (one parse site) so no train-serve skew. DEFAULT false = raw close, unchanged. Crypto/FX have no adjclose -> falls back to close (no effect). Adjusted close re-calibrates EVERY price threshold (range floor, overext, min_cagr, vol/SMA), so flip ONLY for a full `backtest universe` re-validation + gate re-sweep, then keep it only if both OOS halves still hold. Golden-rule-gated.

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
            min_1y_pct_crypto: -60.0,      // crypto routinely swings -40% in a year without breaking
            max_1m_drop_pct: -15.0,
            max_1m_drop_pct_crypto: -35.0, // alts routinely shed -20..-30% in a month without breaking
            min_long_pct: 0.0,
            min_long_pct_crypto: -70.0,    // -EUR 5Y is peak-anchored: allow deep pullbacks, cut true corpses (-70%+)
            min_avg_turnover_eur: 0.0,     // off by default; settings.yaml sets a real floor to drop thin names
            endpoint_smooth_days: 1,       // (#17/Step 4) 1 = raw last close (byte-identical, validated edge intact); e.g. 5 averages the last week's closes for measurement endpoints
            // score
            normal_volatility_pct: 2.0,    // ~2%/day = a typical large-cap equity
            discount_cap: 35.0,            // a ~35%-off (for its vol) dip maxes the discount
            discount_weight: 0.35,         // (#4) demote the dip reward — walk-forward rho is NEGATIVE for on-sale across 3/5/7y and ~0 on the 354-name wide sample (deepest-dip ranking carries no selection skill); 0.35 shifts weight to the CAGR/sharpe terms that drive the working growth lane WITHOUT gutting on-sale scores (0.15 dropped normal names below min_score for only a noise-level rho gain). 1.0 = old, 0 = off
            momentum_bounce: 1.0,          // neutral: a weekly bounce is noise at a multi-decade hold horizon
            momentum_knife: 1.0,           // neutral: this-week direction shouldn't reorder a 40-year pick
            long_trend_weight: 0.5,        // per %/yr CAGR: a +30%/yr compounder adds ~15, secondary to the discount (cap 35)
            long_trend_cap: 30.0,          // cap the long-leg CAGR at 30%/yr (a +46%/yr coin doesn't dwarf a +14%/yr one)
            fixed_cagr_years: 0,           // (#15) off: rank on the longest available leg (today's behaviour). Set 10 to pin every name's CAGR to its 10Y window
            use_trend_cagr: false,         // (#14) off: two-point endpoint CAGR (today's behaviour). true = least-squares log-slope CAGR (endpoint-robust)
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
            tax_keep_eu: 1.0,              // (D/PT) 1.0 = no tax haircut -> byte-identical to the pre-tax lane; ci-settings.yaml ships the live rate
            tax_keep_other: 1.0,           // (D/PT) same: neutral out-of-the-box, the fixture carries the operator's number
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
            growth_min_score_etf: 5.0,     // code default = growth_min_score (no behavior change out-of-box); ci-settings.yaml ships the lower ETF-calibrated floor
            growth_overext_cap: 100.0,     // (1) a name 100%+ above its 200wk SMA is maximally stretched
            growth_max_above_ma: 0.0,      // code default off; ci-settings.yaml ships the validated 150
            growth_require_lifetime_uptrend: false, // (#25) off until a probe validates it
            growth_maxdd_cap: 0.0,         // (#26) off until a probe validates a value
            growth_maxdd_cap_crypto: 0.0,  // (#26) off by default; ci-settings ships 84 (just above BTC's -83)
            growth_max_vol_crypto: 0.0,    // (#36) off by default; ci-settings ships 3.0 (just above BTC's ~2.4%/day)
            growth_min_age_years: 0.0,     // (#33) off by default; ci-settings ships 3 (backtest-blind live gate)
            growth_min_aum_etf: 0.0,       // (AUM) off by default; ci-settings ships 100M (backtest-blind ETF closure-risk gate)
            growth_ter_drag: false,        // (#34) off by default; ci-settings ships true (backtest-blind ETF cost dock)
            growth_overext_floor: 0.05,    // (1) ...and keeps only 5% of its growth score at full stretch. Tightened 0.2->0.15->0.05: each step a harder blow-off-top brake (buying right after a parabolic run-up is a poor long-hold entry). The walk-forward sweep ranks 0.05 the best generalizer — wide 5y rho +0.26->+0.28 AND OOS-late rho +0.09->+0.14 (durability +55%) with the profit edge flat (+108.5->+106.8). A prior session rejected 0.1 for "docking NVDA out of the table," but that was regime-bound: at 0.05 today NVDA still scores 6.7 > growth_min_score 5 (it's -10.6% off-hi, not parabolic) and the displayed stocks order is unchanged. 1.0 = brake off
            growth_turnover_weight: 0.5,   // (L) liquidity tilt per ln(turnover/€1B), added after the brake. Lifts deep-liquid proven compounders (NVDA €32B -> +~1.0) over illiquid €200-500M names they tie/trail on the brake-docked score, without touching the validated edge (BACKTEST-BLIND)
            growth_overext_cap_crypto: 100.0, // (#4) defaults to the equity cap (NO behavior change until tuned). Raise (e.g. 200) so the brake lets crypto ride further above its SMA before docking
            growth_fund_weight: 0.0,       // (G) OFF by default — the fund term is inert until validated. Wired through the growth lane so `backtest <set> fund` can ablate it; raise only on +Δ + both-half-positive OOS
            growth_fund_cap: 30.0,         // (G) clamp the fund factor to ±/+30 pts before weighting (irrelevant at weight 0); keeps a freshly-listed +9000% rev-accel artifact from running away with the rank
            growth_fund_factor: "rev_accel".to_string(), // (G) the prior hardcoded factor — change in settings.yaml once the probe names a better one (irrelevant at weight 0)
            fund_source: "fmp".to_string(), // (Item 22) FMP feed by default (= current behavior). Set "sec" for free/uncapped US annual fundamentals, then re-validate via `backtest <set> fund` before raising growth_fund_weight (irrelevant at weight 0)
            growth_mom121_weight: 0.0,     // (M) OFF by default — the 12-1 momentum term is wired + ablatable but inert until validated; raise only on +Δ + both-half-positive OOS
            growth_mom121_cap: 50.0,       // (M) clamp 12-1 momentum to +50 pts before weighting (irrelevant at weight 0); a name up >50% over the year-ago-to-month-ago window is already maxed for this tilt
            growth_smoothness_weight: 0.0, // (E) OFF by default (older settings.yaml unchanged); ci-settings ships the swept optimum 5.0
            growth_underwater_weight: 0.0, // OFF by default (older settings.yaml unchanged); ci-settings ships the validated 0.3 (2026-07-19 probe + candidate lane run, see field doc)
            growth_value_weight: 1.0,      // (Item 20) FULL P/E-multiplier authority by default (=current behavior, no change). The validated edge never saw this term (pe_ratio None in backtest); dial toward 0 once the additive earnings_yield term (Item 19) validates and carries valuation honestly
            growth_geomean_fold: false,    // (#8) off: multiply proximity/value raw onto base (today's behaviour, validated edge intact). true folds them into the geomean (bounds the multiplicative stack) — validate via `backtest universe` both-OOS-positive before flipping
            use_adjusted_close: false,     // (Item 21) raw price-only close by default (= current behavior, validated edge intact). Flip to true ONLY for a full backtest re-validation + gate re-sweep — adjusted close shifts every price-calibrated threshold
            nupl_euphoria: 0.5,            // (4) NUPL > 0.5 = market greed -> start damping crypto
            nupl_damp_floor: 0.5,          // (4) at NUPL 1.0 (peak euphoria) crypto scores are halved
            nupl_capitulation: 0.25,       // (4) NUPL < 0.25 = fear/accumulation -> start boosting crypto (buy-the-fear)
            nupl_boost_ceiling: 1.3,       // (4) at NUPL 0 (deep capitulation) crypto scores ×1.3. BACKTEST-BLIND judgment, kept mild
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
fn default_eu_hicp_old() -> String {
    "https://ec.europa.eu/eurostat/api/dissemination/statistics/1.0/data/prc_hicp_manr?format=JSON&lang=EN&coicop=CP00&geo=EU27_2020".to_string()
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

/// Default (TER) ETF expense-ratio endpoint: FMP `stable/etf/info` (carries `expenseRatio` as a
/// FRACTION, e.g. 0.0003 = 0.03%). Free-tier reachable; returns nothing for stocks/crypto.
fn default_fund_expense_url() -> String {
    "https://financialmodelingprep.com/stable/etf/info?symbol={ticker}&apikey={key}".to_string()
}

/// Default Börse Frankfurt ETF search (POST, turnover-sorted UCITS list).
fn default_bf_etf_search_url() -> String {
    "https://api.boerse-frankfurt.de/v1/search/etp_search".to_string()
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
fn settings_path() -> PathBuf {
    // 0. explicit override: CI points this at a checked-in fixture (tests/ci-settings.yaml) because the
    //    real config/settings.yaml is gitignored and absent there. Empty = ignore.
    if let Ok(p) = std::env::var("FOLIOMAN_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
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
/// override only replaces the knobs it names); for anything else, `over` wins outright.
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

/// Read + parse the settings (base + overlay). Panics with a clear message if missing/invalid —
/// config errors are a startup problem the user must fix, not something to fail soft on.
pub fn load() -> Settings {
    let path = settings_path();
    let merged =
        merged_config().unwrap_or_else(|| panic!("cannot read/parse config {}", path.display()));
    let s = serde_yaml::from_value(merged)
        .unwrap_or_else(|e| panic!("invalid config ({} over tests/ci-settings.yaml): {e}", path.display()));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Working dot-files anchor at the repo root (the dir holding the config), never the process
    /// cwd — the scatter this prevents was seen live: cron cwd=$HOME plus runs from a sibling
    /// repo left diverged cache copies outside the repo. Both test layouts (private overlay
    /// present locally, only the committed fixture in CI) must resolve to a dir with Cargo.toml.
    #[test]
    fn data_path_anchors_to_repo_root() {
        let p = data_path(".probe.json");
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

    /// The CI network fixture must parse into Settings (else the network-smoke job panics for a non-API
    /// reason — the exact bug this guards), AND serde defaults must fill the omitted fields (the contract
    /// that lets a minimal / older settings.yaml load). Offline + deterministic.
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
        assert_eq!(h.growth_fund_weight, 0.07, "{receipts}");
        assert_eq!(h.growth_fund_cap, 300.0, "cap is half the tilt magnitude — {receipts}");
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
    }

    /// FOLIOMAN_CONFIG overrides the discovery walk (how CI points at the fixture). No other test reads
    /// this var, so the set/remove is isolated.
    #[test]
    fn env_override_wins() {
        std::env::set_var("FOLIOMAN_CONFIG", "tests/ci-settings.yaml");
        assert_eq!(settings_path(), PathBuf::from("tests/ci-settings.yaml"));
        std::env::set_var("FOLIOMAN_CONFIG", ""); // empty -> ignored, falls back to discovery
        assert_ne!(settings_path(), PathBuf::from(""));
        std::env::remove_var("FOLIOMAN_CONFIG");
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
    }
}
