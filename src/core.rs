//! Pure logic for folioman: types, formatting, market/trend/inflation math.
//! No network here — all I/O lives in `fetch.rs`. Read-only, never trades.
//! Acronyms (CAGR, NUPL, SMA, GICS, R², CdA, …): see the Glossary in README.md.

use chrono::{Datelike, Duration, NaiveDate};
use serde_json::Value;
use std::collections::BTreeMap;

/// label -> calendar days back. `Quote.perf` is a Vec aligned to THIS order, so anything reading a
/// leg must go through `picks::perf_pct` (label lookup) — a raw `perf[i]` breaks silently when a
/// horizon is inserted.
pub const HORIZONS: &[(&str, i64)] = &[
    ("1D", 1),
    ("1W", 7),
    ("1M", 30),
    ("3M", 91),
    ("6M", 182),
    ("1Y", 365),
    ("2Y", 730),
    ("5Y", 1825),
    ("8Y", 2920),
    ("10Y", 3650),
    ("20Y", 7300),
];

/// Certificados de Aforro: base = clamp(Euribor3M × `mult` + `spread`, 0, `cap`); a permanence
/// premium (NOT capped) is added on top per holding year, read from `premium`'s bands.
///
/// Every figure below comes from IGCP's per-series ficha técnica or the Portaria that created the
/// series. To re-check any of them, the ground truth is IGCP's monthly per-series rate sheet
/// (`igcp.pt/.../Taxa_Anual_<S>+PP.pdf`) — it publishes the rate actually being paid, so it catches
/// a wrong formula here in one subtraction.
pub struct CaSeries {
    pub name: &'static str,
    /// Multiplier on Euribor 3M. 0.60 for Série B, whose rate is `0,60 × TBA` — and TBA is the SAME
    /// "média da Euribor a 3 meses dos últimos 10 dias úteis" the newer series index to, so one
    /// field spans both shapes. 1.0 for C-F. (B was `0,80 × TBA` until DL 172-B/86 art. 15.º b).)
    ///
    /// None = IGCP publishes no rate formula for the series at all (Série A, whose ficha only says
    /// its terms follow pre-1986 Série B). Base and every gain cell then print unknown. Do NOT
    /// invent one to fill the row: `fetch.rs` already warns that a hand-entered rate silently
    /// poisons this table, and a guess is worse than a fetch failure because nothing downstream
    /// can tell it apart from a real number.
    pub mult: Option<f64>,
    pub spread: f64,
    /// None = no documented cap (Série B). 0.60 × Euribor has never come near one.
    pub cap: Option<f64>,
    /// (first holding year, percentage points), ascending — the last band that has started wins.
    /// Year 1 earns base only, unless a band starts at 1.
    ///
    /// Replaced a `premium_early`/`premium_late` pair that held exactly two tiers. C's ladder has
    /// six and F's has five, so the pair could not describe either: it stopped F at +0.50 where the
    /// real ladder climbs to +1.75 by year 14, understating F's 20Y by ~21 points.
    pub premium: &'static [(i64, f64)],
    /// None = never matures, lives until redeemed (A and B). Everything else ends — and the gain
    /// columns deliberately run past it, see `ca_cumulative_gain`.
    pub prazo_years: Option<i64>,
    pub note: &'static str,
}

/// Oldest first; the one you can still subscribe today (F) is last. A-E are all closed to new
/// subscription and stay because you may still HOLD one — and because the table's job is comparing
/// what each series pays, not only what is on sale.
pub const CA_SERIES: &[CaSeries] = &[
    // DL 43.454 (30 Dec 1960), closed Jul 1986. IGCP still names A in its monthly rate notices but
    // publishes no formula for it, so the row is honest unknowns rather than a plausible number.
    CaSeries { name: "A", mult: None, spread: 0.0, cap: None, premium: &[], prazo_years: None, note: "rate not published" },
    // DL 172-B/86 (30 Jun 1986), closed by Portaria 73-A/2008. No maturity — B lives until redeemed.
    // Single band because every surviving B was subscribed by Jan 2008 and so is 18+ years old:
    // IGCP's own ficha says "all certificates present the maximum bonus, i.e. 2%". This models what
    // B pays NOW, which is what the table asks; it is not B's historical ladder.
    CaSeries { name: "B", mult: Some(0.60), spread: 0.0, cap: None, premium: &[(1, 2.00)], prazo_years: None, note: "0,60×TBA · no maturity" },
    // Portaria 230-A/2009 era, subscribable to 2015. Last one matured in 2025, which is why IGCP's
    // rate notices list A, B, D, E, F and no C. Kept as the historical row it now is.
    CaSeries { name: "C", mult: Some(1.0), spread: 1.0, cap: Some(3.5), premium: &[(2, 0.50), (3, 0.75), (4, 1.00), (8, 1.25), (9, 1.50), (10, 2.50)], prazo_years: Some(10), note: "MATURED 2025" },
    // Portaria 17-B/2015, closed by Portaria 329-A/2017. IDENTICAL terms to E by law: 329-A/2017
    // says E "mantêm as condições financeiras" of D and only moved subscription to digital-only. If
    // these two rows ever diverge, one of them has been edited without a source.
    CaSeries { name: "D", mult: Some(1.0), spread: 1.0, cap: Some(3.5), premium: &[(2, 0.50), (6, 1.00)], prazo_years: Some(10), note: "closed Oct 2017" },
    CaSeries { name: "E", mult: Some(1.0), spread: 1.0, cap: Some(3.5), premium: &[(2, 0.50), (6, 1.00)], prazo_years: Some(10), note: "closed Jun 2023" },
    // Portaria 149-A/2023. The only series still open, and the only one whose prazo is 15y.
    CaSeries { name: "F", mult: Some(1.0), spread: 0.0, cap: Some(2.5), premium: &[(2, 0.25), (6, 0.50), (10, 1.00), (12, 1.50), (14, 1.75)], prazo_years: Some(15), note: "open" },
];

/// Premium in force during holding year `y` (1-based): the last band that has started, 0 before the
/// first. Bands are half a dozen entries, so a reverse scan is the whole algorithm.
fn ca_premium(bands: &[(i64, f64)], y: i64) -> f64 {
    bands.iter().rev().find(|(from, _)| y >= *from).map_or(0.0, |(_, p)| *p)
}

/// Compact "+0.50→+2.50%" for a ladder, "+2.00%" for a flat one, "—" for none.
pub fn ca_premium_range(bands: &[(i64, f64)]) -> String {
    match bands {
        [] => "—".to_string(),
        [(_, p)] => format!("+{p:.2}%"),
        [(_, lo), .., (_, hi)] => format!("+{lo:.2}→+{hi:.2}%"),
    }
}

/// Cumulative % gain on €1 held for `years` whole years, compounding each holding year's
/// rate = base + that year's permanence premium.
/// note: annual compounding, ignores intra-year capitalisation — close enough for a
/// footer estimate, and Euribor (so base) drifts anyway. Assumes today's base holds.
///
/// DELIBERATELY keeps compounding past the series' `prazo_years`. Every series except A and B
/// matures inside 20 years (C/D/E at 10, F at 15), so their 20Y cell is NOT money anyone collects —
/// it is what the series' terms would pay if they ran that long. The PRAZO column beside it is what
/// tells the reader which cells are real. Truncating instead was considered and declined; changing
/// it is a display decision, not a bug fix.
pub fn ca_cumulative_gain(base: f64, premium: &[(i64, f64)], years: i64) -> f64 {
    let mut factor = 1.0;
    for y in 1..=years {
        factor *= 1.0 + (base + ca_premium(premium, y)) / 100.0;
    }
    (factor - 1.0) * 100.0
}

/// Yahoo ticker suffix -> market country (listing venue, not legal domicile).
fn suffix_country(suf: &str) -> Option<&'static str> {
    Some(match suf {
        "DE" => "Germany", "L" => "UK", "PA" => "France", "AS" => "Netherlands",
        "MI" => "Italy", "MC" => "Spain", "SW" => "Switzerland", "VI" => "Austria",
        "LS" => "Portugal", "BR" => "Belgium", "HE" => "Finland", "ST" => "Sweden",
        "OL" => "Norway", "CO" => "Denmark", "IR" => "Ireland", "TO" => "Canada",
        "HK" => "Hong Kong", "T" => "Japan", "AX" => "Australia", "SA" => "Brazil",
        "NS" => "India", "SS" => "China", "SZ" => "China", "KS" => "South Korea",
        _ => return None,
    })
}

/// (#112) Yahoo ticker suffix -> the currency that venue QUOTES in. Deliberately a second table over
/// the same suffixes rather than a country -> currency map on top of `suffix_country`: the two answer
/// different questions (Sweden and Denmark are EU and are not euro; four venues share EUR), and a hop
/// through country would make the euro bloc look like a coincidence instead of a fact. Kept adjacent
/// to `suffix_country` for the same reason `EU_MARKETS` below is — an edit to either literal sees the
/// other, and a venue added to one and not the other reads as a country with no currency.
///
/// GBp (LSE pence) is NOT spelled here: it is the same EXPOSURE as GBP, and this table answers what a
/// EUR holder's FX risk is, not what unit the ticker prints in.
fn suffix_currency(suf: &str) -> Option<&'static str> {
    Some(match suf {
        "DE" | "PA" | "AS" | "MI" | "MC" | "VI" | "LS" | "BR" | "HE" | "IR" => "EUR",
        "L" => "GBP", "SW" => "CHF", "ST" => "SEK", "OL" => "NOK", "CO" => "DKK",
        "TO" => "CAD", "HK" => "HKD", "T" => "JPY", "AX" => "AUD", "SA" => "BRL",
        "NS" => "INR", "SS" | "SZ" => "CNY", "KS" => "KRW",
        _ => return None,
    })
}

/// (#112) The currency a listing trades in, from its ticker alone. No suffix = a US line = USD, the
/// same assumption [`market`] makes one function down. `None` is an UNRECOGNISED venue, printed as
/// unknown rather than guessed: a wrong currency here is silently a wrong FX exposure, and the
/// currency-mix footer would rather show a `?` slice than a confident lie.
///
/// Ticker-derived on purpose. Yahoo's `quote_currency` is the better answer for a row we hold a
/// [`Quote`] for, and the caller uses it there — this exists for a fund's HOLDINGS, which arrive as
/// bare symbols with no quote attached.
pub fn listing_currency(sym: &str) -> Option<&'static str> {
    match sym.rsplit_once('.') {
        Some((_, suffix)) => suffix_currency(suffix),
        None => Some("USD"),
    }
}

/// EU member states, spelled exactly as `suffix_country` above spells them (kept adjacent so an edit
/// to either literal sees the other). This is the Art. 40.º-A CIRS set: dividends from a company
/// resident in an EU state meeting the Parent-Subsidiary Directive conditions are englobados at only
/// 50% on a Portuguese IRS return. UK (post-Brexit), Switzerland and Norway are European but NOT EU
/// members — no exclusion, so they are deliberately absent.
///
/// CAVEAT this set cannot fix: `market` is the LISTING VENUE, not the payer's tax residence. An
/// Irish-domiciled S&P 500 name (ACN, ETN, MDT…) carries no suffix and reads "USA" here, so it is
/// under-credited. That direction is conservative — it never awards the exclusion to a payer that
/// lacks it. Closing it needs a domicile source for stocks, which no current feed provides.
const EU_MARKETS: [&str; 12] = [
    "Germany", "France", "Netherlands", "Italy", "Spain", "Austria",
    "Portugal", "Belgium", "Finland", "Sweden", "Denmark", "Ireland",
];

/// Is this `Quote.market` an EU member state (see `EU_MARKETS`)?
pub fn is_eu_market(market: &str) -> bool {
    EU_MARKETS.contains(&market)
}

/// (S-8Y) the four price stats whose window is LONGER than 8 years, re-measured on just the last 8.
/// The `S-8Y` column pins the long-CAGR window to 8 years, but every other price stat on a `Quote` is
/// computed once at fetch time over the whole ~10y daily payload — so without these the column mixed
/// an 8-year CAGR with 10-year range/R²/drawdown and only half meant what its header said.
/// Deliberately NOT the whole set: `above_ma_pct` (200wk ≈ 3.8y) and `volatility_pct` (~1y) already
/// sit inside 8 years and cannot move, so re-slicing them would be dead code.
#[derive(Debug, Clone)]
pub struct Stats8 {
    pub range_pct: f64,
    pub trend_r2: f64,
    pub max_drawdown_pct: f64,
    pub underwater_yrs: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Quote {
    pub ticker: String,
    pub price: String,   // "€123.45", "123.45 USD?" (FX unknown), "err", "no data"
    pub dip: String,     // "-3.2%"
    pub drop_pct: f64,   // numeric, for alert threshold
    pub market: String,
    pub instrument_type: String, // Yahoo chart-meta instrumentType ("ETF"/"EQUITY"/"CRYPTOCURRENCY"/...); the reliable asset-class signal, vs the name-substring guess. "" if absent.
    pub head: String,        // first headline ("" if none)
    pub news_block: String,  // up to 3 headlines, "- ..\n- .." (alert body)
    pub perf: Vec<Option<(String, f64)>>, // aligned to HORIZONS: (past_eur_str, pct) or None
    // (#88) the SAME legs, never deflated — the units every backtest receipt was fitted in. EMPTY is
    // the signal, exactly as `capped_cagr`'s absence is (`picks::life_leg_cagr`): the live fetch fills
    // this only when `inflation_adjust.score_on_nominal` is on, and `backtest_quote` never fills it, so
    // `picks::perf_pct` falls through to `perf` and the shipped lane stays byte-identical. Filled, it is
    // what every gate and score term reads, while `perf` above keeps feeding the REAL % columns.
    pub perf_nominal: Vec<Option<(String, f64)>>,
    pub name: String,    // human-readable instrument name (falls back to ticker)
    pub trend: String,   // "↑ 2w" / "↓ 5d": current direction + how long it has held
    pub at_ath: bool,    // at/near all-time high (within tol of max seen)
    pub at_atl: bool,    // at/near all-time low (within tol of min seen)
    pub mom_pct: Option<f64>, // % change vs ~1 month ago (None if no data); <0 = falling
    pub div_eur: Vec<Option<f64>>, // total dividends/share (EUR) per DIV_HORIZONS; None = short history
    pub price_eur: Option<f64>, // current close in EUR (None if FX unknown); for dividend yield
    pub close_native: Option<f64>, // (Item 19) latest close in the listing's OWN currency (NOT FX-converted) — paired with native EPS for a currency-consistent earnings_yield, same number the backtest scores on
    pub quote_currency: Option<String>, // (FX) which currency `close_native` IS, straight from Yahoo ("USD", "EUR", "GBp" for pence-quoted LSE). Needed because a foreign filer's statements are in ITS currency, not the listing's — ASML reports EUR and trades USD — so pairing EPS with a price requires proving both sides match FIRST. None = unknown -> callers must not convert
    pub last_close_date: Option<NaiveDate>, // (D) date of the most recent close bar — stale (old) = a halted/dead listing frozen at an old price; LIVE-only, None in stub/backtest (staleness is a live-fetch data-quality gate)
    pub drawdown_pct: f64, // % below the high of the last ~high_days (picks "on sale" signal)
    pub intraday: [Option<f64>; 3], // % change over [1h, 6h, 12h] = 1/6/12 hourly bars back; None if too few bars
    pub avg_turnover_eur: Option<f64>, // avg daily turnover (close*volume, EUR) ~last 30 sessions; liquidity proxy
    pub volatility_pct: Option<f64>,   // daily-return stdev (%) ~last year; the asset's "normal swing" for the picks score
    // (P4) MAX: the largest single-day gain in the trailing month, %. A DIFFERENT question from the two
    // price-shape terms already here — `volatility_pct` averages the whole distribution and `above_ma_pct`
    // measures a level stretch, and a name can sit calm and near its mean on both while having printed one
    // +38% day last week. That lottery-ticket day is the documented signal, and nothing in the tree sees it.
    pub max_daily_1m: Option<f64>,
    pub below_ma_pct: f64,             // % below the ~200-week SMA (structural "cheap vs long trend"); 0 if at/above or history too short
    pub above_ma_pct: f64,             // % ABOVE the ~200-week SMA (overextension "how far it ran"); 0 if at/below or history too short. Growth-lane brake on blow-off tops
    pub pe_ratio: Option<f64>,         // trailing P/E for the valuation tilt; None for crypto/ETF/no-earnings/no source (-> neutral)
    pub mvrv: Option<f64>,             // (#45) CRYPTO ONLY — market cap / realized cap (CoinMetrics `CapMVRVCur`). The coin's price against what its holders actually paid; <1 = the market sits below its own aggregate cost basis. This is the per-coin form of the Bitcoin-only NUPL already fetched (NUPL = 1 - 1/MVRV), and what `crypto_max_mvrv` gates on. None for every non-coin, and for most coins — CoinMetrics serves ~17 of the top 100. BACKTEST-BLIND in practice: `backtest_quote` never fills it, and no crypto row is growth-scored at any cutoff anyway
    pub roe: Option<f64>,              // (F) trailing return-on-equity (%) — the core profitability/QUALITY factor; None for crypto/ETF/no-earnings/no source (-> neutral). BACKTEST-BLIND (point-in-time, can't reconstruct as-of)
    pub expense_ratio: Option<f64>,    // (TER) ETF annual expense ratio (%); None for stocks/crypto/no source. DISPLAY-ONLY (`ter` column) — the one cost that compounds against a decades hold
    pub range_pct: f64,                // percentile rank (0..100) of the last close in its own ~10y history; 100=at high. picks discount = 100-this
    pub trend_r2: f64,                 // (A) R² (0..1) of the log-price trend — how steadily it compounds; damps CAGR endpoint-luck. 0 = no/short history
    pub trend_cagr: Option<f64>,       // (#14) annualized CAGR from the log-price trend SLOPE (endpoint-robust); precomputed at build, ranked on only when `use_trend_cagr`. None = <2 points
    pub max_drawdown_pct: f64,         // (C) worst peak-to-trough decline (%) in its history; feeds the Calmar (return-per-pain) reward. 0 = never down/no history
    pub downside_dev_pct: Option<f64>, // (r39) per-bar downside deviation — `volatility_pct` counting only DOWN moves. BACKTEST-PROBE-ONLY (Sortino candidate); no score path and no live fill read it, so it is None everywhere except `backtest_quote`
    pub roll5y_pos_pct: Option<f64>,   // (consistency) % of rolling ~5y windows with a positive NOMINAL return, from the same closes. DISPLAY-ONLY footer ("how often did 5 patient years pay?"); None = <5y of history — never a fake 100%
    pub underwater_yrs: Option<f64>,   // (underwater) longest stretch below the prior peak, in years (~252 sessions/yr), ongoing stretch counts. DISPLAY-ONLY footer — MAXDD's missing twin: depth says how far down, this says how LONG the pain lasted. None = <2 usable closes
    pub worst_5y_pct: Option<f64>,     // (worst-5y) single worst rolling ~5y NOMINAL outcome (%), severity twin of roll5y_pos_pct's frequency. DISPLAY-ONLY footer; None = <5y of history — no claim
    pub roll10y_pos_pct: Option<f64>,  // (10y-consistency, r16) same walk at the DECADE horizon the book is actually held for. DISPLAY-ONLY footer; None = <10y of history — never a fake 100%
    pub worst_10y_pct: Option<f64>,    // (worst-10y, r16) single worst rolling ~10y NOMINAL outcome (%), severity twin at the decade horizon. DISPLAY-ONLY footer; None = <10y of history — no claim
    pub year_returns: Vec<(i32, f64)>, // (r11) each COMPLETE calendar year's % return from the fetched daily window, ascending. DISPLAY-ONLY footer (regime check: losing whole years); empty = <2 usable years — no claim. NOT filled in backtest_quote (display-era, edge-blind by construction — unlike `life_cagr`, which (#3j) fills there)
    pub fund_factor: Option<f64>,      // (G) the ONE as-of fundamental factor folded into growth_score (e.g. revenue accel). Set in the backtest (from fund_factors) so the term is ablatable, and live only on the small/check-scale path; None -> neutral (universe screen & price-only backtest)
    // (G+) the WHOLE as-of fundamental struct, so `growth_fund_extra` can weigh several named terms
    // without a carrier field per factor. Set at the same three sites that fill `fund_factor`.
    // `fund_factor` stays because the factor SWEEP injects a value there directly, per candidate
    // factor, without rebuilding FundFactors — that machinery keeps working untouched.
    pub fund: Option<FundFactors>,
    pub age_years: Option<f64>,        // listing age in years from the FULL (monthly-backfilled) history; DISPLAY-ONLY (`yrs` column). None = no data / stub / backtest
    pub life_cagr: Option<f64>,        // whole-life endpoint CAGR (%) over that full history, via `core::life_cagr`. NOT display-only since (#3i)/(#3j): the `cagr` column, the `growth_min_cagr` whole-life bar, and the growth RANK when `use_life_cagr` is on. Filled in the backtest too (same fn, `[..=as_of]` slice) -> train==serve. None = <6mo history / non-positive first close / stub
    pub capped_cagr: Option<f64>,      // (#3l/#73) endpoint CAGR over the last min(age, life_cagr_max_years) years, via `core::capped_life_cagr`. ONE reader: `picks::life_leg_cagr`, i.e. `growth_min_cagr`'s whole-life reject bar — (#73) repointed this field from the RANK (where (#3l) measured it at -66 edge and shipped it off) to that bar. Filled at the same two sites as `life_cagr` (fetch + backtest_quote), same knob read via the free accessor -> train==serve. None = knob off / <5y of history, and the bar then falls back to the uncapped `life_cagr` it always used -> the pool is unchanged at 0 and young names never move
    pub life_return_pct: Option<f64>,  // whole-life CUMULATIVE real return (%) over that same full history, via `core::life_return`. DISPLAY-ONLY, and deliberately NOT an entry in `perf`: `picks::perf_fill` prints it (marked `≈`) in a long rung the record ALMOST reaches, and putting it in `perf` would hand it to `perf_pct` and therefore to every gate. None = <6mo history / non-positive first close / stub / BACKTEST (never rendered there)
    pub trail_monthly: Vec<f64>,       // (#41) up to 36 trailing MONTH-over-MONTH returns (%), newest last, via `core::monthly_returns_tail`. Sole input to the growth_corr_cap redundancy skip. Built from the DAILY chart live and from the monthly slice in the backtest — the same fn, so a pair's correlation means the same thing in train and serve. Empty = no history / stub -> unjudgeable, and an unjudgeable pair never blocks
    pub tr_cagr: Option<f64>,          // (TR-CAGR) life_cagr + the whole-life dividend sum added to the endpoint — LOWER-BOUND total return (payouts added, not reinvested). ≈ life_cagr for Acc funds/non-payers. (#99) NO LONGER DISPLAY-ONLY: with `growth_gate_on_tr_cagr` on, `picks::life_leg_cagr` adds `tr_cagr − life_cagr` to the whole-life reject bar. Filled at BOTH sites since (#99) — fetch and `backtest_quote`, the same `core::tr_life_cagr` over the same `[..=as_of]` slice and the same as-of dividends `div_eur` reads -> train==serve, and the knob is measurable. Knob off = read by the `trcagr` column only, exactly as before
    pub history_proxied: bool,         // (history_proxy) closes bridged from a configured older same-strategy twin — CAGR/YRS describe the STRATEGY, not this listing; rendered as `~` so the bridge is never invisible
    pub stats_8y: Option<Stats8>,      // (S-8Y) the >8y price stats re-measured on the last 8 years, for the 8Y-pinned diagnostic column ONLY — never read by the live score. None = no history older than 8y (its whole record IS the window, so the full-window stats already are the 8y ones) / stub / backtest
    pub sector: Option<String>,        // (#44) GICS sector, joined from the constituents CSVs (`fetch::sector_map`) — the sole input to the commodity flag/damp for stocks. Set on BOTH `screen` paths (the universe fetch and, since the explain mismatch, the explicit-args one). None for ETFs/crypto (funds carry no GICS; their path is name tokens), for `check` (its growth table is explicitly "derived from the table above — no extra fetch", a contract worth more than the flag), and for `check`. CORRECTED (#95): the claim that this is None "in the BACKTEST pool -> `is_commodity` false there -> damp ×1.0 -> validated edge untouched" is FALSE and has been since `stamp_asset_class` started stamping it — the same class of stale claim the `sharpe_cap_etf` receipt corrected on 2026-08-02. The damp IS live in the walk, on TODAY's label at every cutoff; `backtest_drop_lookahead_sector` is the knob that makes the old sentence true again
    pub aum_eur: Option<f64>,          // (AUM) fund size from the Börse Frankfurt universe payload, EUR-approximate (BF mixes fund currencies; ±FX is immaterial vs the order-of-magnitude gate). ETFs/ETPs only; None = not a fund / not in BF / backtest -> gate inert
    pub ter_fallback: Option<f64>,     // Yahoo quoteSummary TER (%) for funds with NO BF facts (venue/regulatory-only rows). Read ONLY via ter_shown() for display + H/CORE — kept out of expense_ratio because ter_damp SCORES that field (a merged run moved live ranks; scoring lane closed)
    pub aum_fallback: Option<f64>,     // Yahoo quoteSummary totalAssets for the same funds, quote-currency ≈ EUR. Read ONLY via aum_shown() for display + H/CORE — the closure-risk AUM gate stays on BF aum_eur
    pub use_of_profits: Option<&'static str>, // (USE) share class from the same BF row: "Acc"/"Dist". DISPLAY-ONLY — never scored: the price-only CAGR already prices the Dist payout drag (payouts leave the NAV), so Acc twins win by construction
    pub replication: Option<&'static str>,    // (REPL) replication method, same BF row: "Swap"/"Full"/"Opt"/"Hybr"/"Samp". DISPLAY-ONLY counterparty-structure legibility (swap-based US-index funds also legally dodge dividend withholding — why they track so well)
    pub benchmark: Option<String>,     // BF benchmark-index name, lowercased at capture (BF normalizes it: same-index funds share the literal string, hedged classes differ). Used ONLY for history_proxy twin HINTS — never scored, never a match key beyond exact `==`
    pub domicile: Option<String>,      // (DOM) fund legal domicile from the ISIN prefix ("IE"/"LU"/"DE"…). DISPLAY + CORE-shortlist ordering (IE first: 15% US-dividend withholding treaty vs LU's 30% ≈ +0.2%/yr on a US/world fund) — never scored; None for stocks/crypto, watchlist-only runs and backtest
    pub rev_yoy: Option<f64>,          // newest COMPLETE-fiscal-year revenue growth (%) vs the prior FY, from the same income-statement pipeline `report` prints. DISPLAY-ONLY (stocks) — the fund-factor family measured null for ranking; enriched only for the displayed top rows, None otherwise/backtest
    pub eps_yoy: Option<f64>,          // newest complete-FY EPS growth (%) vs the prior FY. DISPLAY-ONLY, same scoping as rev_yoy
    pub net_margin_fy: Option<f64>,    // newest complete-FY net margin (%). DISPLAY-ONLY, same scoping as rev_yoy
    pub buyback_yoy: Option<f64>,      // newest complete-FY net share-count change, sign-flipped (+ = buying back, − = diluting). DISPLAY-ONLY (stocks), same scoping as rev_yoy
    pub annual_brief: Option<String>,  // (B) one-line multi-year trajectory (rev chain + margin move + EPS CAGR + source) from the SAME rollup the snapshot above uses — screen's fundamentals footer. DISPLAY-ONLY, same scoping as rev_yoy
    pub splits: Vec<(NaiveDate, f64)>, // (#82) (effective date, ratio) from the chart's events.splits; a 4:1 split is 4.0, ascending. NOT SCORED and never will be — it exists so `track` and `sim`, which replay prices journaled BEFORE a split against a series retro-adjusted AFTER one, can restate the old price into today's share definition. Empty for stubs and for `backtest_quote`, which walks one internally consistent series and has nothing to restate
}

impl Quote {
    /// A bare row for error/no-data cases (mirrors Python's "err"/"no data" Quote).
    pub fn stub(ticker: &str, price: &str, head: &str, name: &str) -> Quote {
        Quote {
            ticker: ticker.to_string(),
            price: price.to_string(),
            dip: String::new(),
            drop_pct: 0.0,
            market: market_of(ticker),
            instrument_type: String::new(),
            head: head.to_string(),
            news_block: String::new(),
            perf: Vec::new(),
            perf_nominal: Vec::new(), // (#88) empty = "score on `perf`", the default and the backtest's only state
            name: name.to_string(),
            trend: String::new(),
            at_ath: false,
            at_atl: false,
            mom_pct: None,
            div_eur: Vec::new(),
            price_eur: None,
            close_native: None,
            quote_currency: None,
            last_close_date: None,
            drawdown_pct: 0.0,
            intraday: [None; 3],
            avg_turnover_eur: None,
            volatility_pct: None,
            max_daily_1m: None,
            below_ma_pct: 0.0,
            above_ma_pct: 0.0,
            pe_ratio: None,
            mvrv: None,
            roe: None,
            expense_ratio: None,
            range_pct: 0.0,
            trend_r2: 0.0,
            trend_cagr: None,
            max_drawdown_pct: 0.0,
            downside_dev_pct: None,
            roll5y_pos_pct: None,
            underwater_yrs: None,
            worst_5y_pct: None,
            roll10y_pos_pct: None,
            worst_10y_pct: None,
            year_returns: Vec::new(),
            fund_factor: None,
            fund: None,
            age_years: None,
            life_cagr: None,
            capped_cagr: None,
            life_return_pct: None,
            trail_monthly: Vec::new(),
            tr_cagr: None,
            history_proxied: false,
            stats_8y: None,
            sector: None, // (#44) stamped in `screen` from the universe CSV; stubs/backtest stay None
            aum_eur: None,
            ter_fallback: None,
            aum_fallback: None,
            use_of_profits: None,
            replication: None,
            benchmark: None,
            domicile: None,
            rev_yoy: None,
            eps_yoy: None,
            net_margin_fy: None,
            annual_brief: None,
            buyback_yoy: None,
            splits: Vec::new(),
        }
    }

    /// TER as SHOWN: BF first, Yahoo fallback second. For display cells + the H/CORE flag only —
    /// never feed this into scoring/gates (they stay on the raw BF `expense_ratio` so momentum ranks
    /// are byte-identical with pre-fallback runs).
    pub fn ter_shown(&self) -> Option<f64> {
        self.expense_ratio.or(self.ter_fallback)
    }

    /// AUM as SHOWN: BF first, Yahoo fallback second. Same display/H-CORE-only stance as `ter_shown`.
    pub fn aum_shown(&self) -> Option<f64> {
        self.aum_eur.or(self.aum_fallback)
    }
}

/// Compound annual growth rate (%) implied by a cumulative % over `years`: +285% over 10y ≈
/// 14.4%/yr. Annualizing makes returns over different spans comparable (a 5y vs a 10y vs a 20y
/// leg). Clamps the growth factor just above 0 so a near-total loss can't NaN the fractional root.
pub fn cagr(cumulative_pct: f64, years: f64) -> f64 {
    if years <= 0.0 {
        return cumulative_pct;
    }
    let factor = (1.0 + cumulative_pct / 100.0).max(1e-9);
    (factor.powf(1.0 / years) - 1.0) * 100.0
}

/// Whole-life endpoint CAGR (%): first close -> last close, annualized over the span of `dates`.
/// `None` under 6 months of history or on a non-positive first close — a two-point measurement off an
/// IPO week annualizes to a silly number, and a zero/negative first bar has no growth factor at all.
///
/// ONE definition for BOTH callers. The live fetch fills this from the merged (history-proxied)
/// series, `backtest_quote` from its `[..=as_of]` slice, so anything ranking on it means the same
/// thing in train and serve. The formula was inlined at the fetch site alone while the backtest left
/// the field `None`; a second copy is how those two drift.
///
/// CAVEAT worth knowing before reading a backtest number off this: on the DAILY path the slice starts
/// at the fetch window (~10y), so "life" there means "over the fetched window", not since listing.
/// Only the MAX-monthly path (`backtest ... 12`, i.e. `years >= 8`) reaches the real listing date.
/// (#41) Up to `k` trailing MONTH-over-MONTH returns (%), oldest first, newest last.
///
/// ONE definition for BOTH callers, for the same reason `life_cagr` is: the live fetch hands this a
/// DAILY chart and the backtest hands it an already-MONTHLY slice, and a correlation is only meaningful
/// if both sides resampled identically. Taking the LAST close in each calendar month makes the monthly
/// case a no-op (one close per month is already its own last), so the two paths agree by construction
/// rather than by two functions happening to match.
///
/// Non-positive closes are dropped before pairing — a zero close would make the ratio explode, and the
/// resulting month gap is harmless to a correlation that already demands 12 overlapping points.
pub fn monthly_returns_tail(dates: &[NaiveDate], closes: &[f64], k: usize) -> Vec<f64> {
    let mut month_end: Vec<f64> = Vec::new();
    let mut cur: Option<(i32, u32)> = None;
    for (i, d) in dates.iter().enumerate() {
        let c = match closes.get(i) {
            Some(&c) if c > 0.0 => c,
            _ => continue,
        };
        let key = (d.year(), d.month());
        if cur == Some(key) {
            *month_end.last_mut().expect("cur is Some only after a push") = c; // later close in the same month wins
        } else {
            month_end.push(c);
            cur = Some(key);
        }
    }
    let rets: Vec<f64> = month_end.windows(2).map(|w| (w[1] / w[0] - 1.0) * 100.0).collect();
    rets[rets.len().saturating_sub(k)..].to_vec()
}

/// (#41) Correlation of two trailing monthly-return windows, ALIGNED on their most recent months and
/// requiring 12 of overlap — the evidence bar every gate here uses. Distinct from `pearson`, which
/// demands equal lengths and only 2 points: two names with different listing ages never have equal-length
/// trails, and 2 months of overlap is not a verdict. The math itself is `pearson`'s, on the aligned tails.
pub fn corr_tail(a: &[f64], b: &[f64]) -> Option<f64> {
    let k = a.len().min(b.len());
    if k < 12 {
        return None;
    }
    pearson(&a[a.len() - k..], &b[b.len() - k..])
}

/// (#41) Greedy redundancy skip: walk `trails` in RANKED order and keep an entry only if its
/// correlation with every already-kept entry stays under `cap`. Returns the kept INDICES so the caller
/// keeps its own row type. Unjudgeable pairs (empty trail, <12mo overlap) never block — a brake acts on
/// evidence, like every gate here. `n` bounds how many are kept. There is no in-band off value — a cap
/// of 1.0 still drops a PERFECT twin (`rho >= cap`); the off-switch is the caller not calling this.
pub fn decorrelate_keep(trails: &[&[f64]], n: usize, cap: f64) -> Vec<usize> {
    let mut kept: Vec<usize> = Vec::new();
    for (i, t) in trails.iter().enumerate() {
        if kept.len() >= n {
            break;
        }
        if !kept.iter().any(|&k| corr_tail(t, trails[k]).is_some_and(|c| c >= cap)) {
            kept.push(i);
        }
    }
    kept
}

/// (#75) Cross-sectional P-th percentile FLOOR over one cohort's values — the shared definition behind
/// the live `growth_value_floor_pct` trim (`picks::lane_split`) and the backtest's graded twin
/// (`report_vs_benchmark`), so the brake that ships and the brake that was measured cannot drift. Same
/// bargain as `decorrelate_keep` above: the shared fn owns the arithmetic and returns a plain number,
/// the callers keep their own row types. `None` = nothing to cut (knob off, or a cohort where nobody
/// carries the factor), and the caller then filters nothing rather than filtering everything.
/// Truncating index, clamped to the last element, so `p = 100` is "keep only the top value and its
/// ties" instead of an out-of-bounds panic. Sorted by `total_cmp`: a NaN in the cohort must not decide
/// whether the whole table gets cut, and `partial_cmp().unwrap()` would panic on one.
pub fn pct_floor(mut values: Vec<f64>, p: f64) -> Option<f64> {
    if p <= 0.0 || values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    Some(values[(((p / 100.0) * values.len() as f64) as usize).min(values.len() - 1)])
}

/// (#182) ONE definition of listing age (non-negotiable #4): the whole-life span of a date series in
/// years, at 365.25 days to the year. This arithmetic was spelled out inline in six places —
/// `life_cagr`, `tr_life_cagr`, `capped_life_cagr` and `life_return` below, the `age_years` binding
/// in `fetch.rs`, and (as of `(#182)`) `core::backtest_quote` — which is five chances for the number
/// `growth_min_age_years` gates on to drift between train and serve. Five of the six now read it
/// from here.
///
/// `capped_life_cagr` IS THE EXCEPTION, and deliberately: its copy feeds a `< 5.0` guard, not a
/// stored field, and routing it through here makes that comparison an EQUIVALENT MUTANT the gate can
/// never kill. `num_days()` is a whole number, so the quotient can only equal 5.0 exactly at 1826.25
/// days, which does not exist — `<` and `<=` are therefore the same function there, and no test can
/// separate them. Rewriting the line would plant a permanently un-killable mutant in every future
/// diff that touches that fn. Its guard also runs identically on both paths, so it carries none of
/// the train/serve drift risk that motivates this helper. Verified by the gate: with that line
/// repointed the run came back `1 missed` on exactly that operator and nothing else.
///
/// `None` only for an EMPTY series. A single bar is a real answer of 0.0, not missing data, and the
/// distinction matters: per non-negotiable #5 a `None` PASSES the age gate, so folding the one-bar
/// case into `None` would silently admit the youngest listings the gate exists to cut.
pub fn age_years(dates: &[NaiveDate]) -> Option<f64> {
    dates
        .first()
        .zip(dates.last())
        .map(|(first, last)| (*last - *first).num_days() as f64 / 365.25)
}

pub fn life_cagr(dates: &[NaiveDate], closes: &[f64]) -> Option<f64> {
    let age = age_years(dates)?;
    match (closes.first(), closes.last()) {
        (Some(&first), Some(&last)) if first > 0.0 && age >= 0.5 => {
            Some(((last / first).powf(1.0 / age) - 1.0) * 100.0)
        }
        _ => None,
    }
}

/// (#99) `life_cagr`'s TOTAL-RETURN twin: the same endpoints with the whole-window dividend sum added
/// to the ending price. LOWER BOUND — the payout is added, not reinvested, so a true reinvested total
/// return compounds higher. Same guards as [`life_cagr`], so the two are `None` in exactly the same
/// cases and their difference is always a like-for-like comparison.
///
/// ONE definition, and that is the point of it existing: this arithmetic lived inline in `fetch.rs` and
/// was therefore LIVE-ONLY, which is why the dividend leg has never been in a backtest. `backtest_quote`
/// now fills the same field from the as-of slice through this fn, so the number means the same thing in
/// train and serve.
pub fn tr_life_cagr(dates: &[NaiveDate], closes: &[f64], divs_sum: f64) -> Option<f64> {
    let age = age_years(dates)?;
    match (closes.first(), closes.last()) {
        (Some(&first), Some(&last)) if first > 0.0 && age >= 0.5 => {
            Some((((last + divs_sum) / first).powf(1.0 / age) - 1.0) * 100.0)
        }
        _ => None,
    }
}

/// (#3l/#73) `life_cagr` over the last min(age, `max_years`) years only — since (#73), the window
/// `growth_min_cagr`'s whole-life reject bar reads when `life_cagr_max_years` > 0. Two guards, both
/// deliberate, and both still right for the bar they now feed:
/// - `max_years <= 0.0` -> None (knob off; the bar falls back to the uncapped lifetime, byte-identical).
/// - age < 5y -> None. Under (#3l) this kept the WINDOW swap from widening the ranked pool; under
///   (#73) it keeps a 6-month-old on a bull tear from clearing a proven-compounder bar on a "CAGR"
///   no gate ever validated. Same guard, same reason, other side of the score.
/// The window starts at the first bar AT/AFTER `last − max_years`, so a name younger than the cap
/// keeps its whole life (min(age, cap)) and the cut lands on a real close, never an interpolation.
pub fn capped_life_cagr(dates: &[NaiveDate], closes: &[f64], max_years: f64) -> Option<f64> {
    if max_years <= 0.0 {
        return None;
    }
    let (first, last) = dates.first().zip(dates.last())?;
    if (*last - *first).num_days() as f64 / 365.25 < 5.0 {
        return None;
    }
    let cut = *last - chrono::Duration::days((max_years * 365.25) as i64);
    let start = dates.partition_point(|d| *d < cut);
    life_cagr(&dates[start..], &closes[start.min(closes.len())..])
}

/// (splice) First index AFTER the last redenomination splice — the caller keeps `[start..]` of every
/// parallel array. A splice is a currency/denomination glue joint in a vendor series (MXN head on a
/// GBP tail, a ÷100 GBp→GBP step): one step of ×19.6 that no real asset does, which then feeds
/// `life_cagr`/MAXDD/R² a fiction spanning the whole record. Detection is the implied WEEKLY growth
/// factor, not the raw step ratio — cached bars are unevenly spaced and a raw ratio cannot tell a
/// splice from a wide gap (NVR: ×26 across 92 days of real quarterly bars = 1.28×/wk, clean).
/// `max(days, 7)` refuses to extrapolate a sub-week gap: one ordinary −10% day would otherwise read
/// as 0.9^7 = 0.48/wk and trip the floor. Thresholds are symmetric in log space (`max` up, `1/max`
/// down) because both halves lie: an UP splice fakes a chart-topping CAGR, a DOWN splice fakes a −99%
/// collapse that silently gates a good name out. `max_weekly_rate <= 1.0` (the 0.0 "off" default
/// included) trims nothing. Crypto is the CALLER's exemption — SHIB genuinely did 13×/wk.
pub fn splice_trim_start(dates: &[NaiveDate], closes: &[f64], max_weekly_rate: f64) -> usize {
    if max_weekly_rate <= 1.0 {
        return 0;
    }
    let mut start = 0;
    for i in 1..dates.len().min(closes.len()) {
        let (prev, next) = (closes[i - 1], closes[i]);
        if prev <= 0.0 || next <= 0.0 {
            continue;
        }
        let days = ((dates[i] - dates[i - 1]).num_days() as f64).max(7.0);
        let rate = (next / prev).powf(7.0 / days);
        if rate > max_weekly_rate || rate < 1.0 / max_weekly_rate {
            start = i; // keep the first post-splice close; a later splice overrides an earlier one
        }
    }
    start
}

/// Whole-life CUMULATIVE return %, REAL (deflated) when `infl` is given — the same treatment
/// `horizon_changes` gives its >=1Y legs, so this number is comparable to the cells it stands in for.
/// Same reason for the smoothed endpoint: a long leg is measured against `measure_endpoint`, not the
/// raw last close (`life_cagr` above predates that and keeps the raw one).
///
/// Deflated by the RECORD's own span, not by any rung's: it is the return actually earned over those
/// years. The rung it later fills is up to ~5 months longer (the YRS/leg dead band), worth ~0.7pp of
/// inflation — noise against the ~25pp nominal-vs-real gap this exists to close.
///
/// DISPLAY-ONLY: `picks::perf_fill`'s value, and nothing else. Never enters `Quote::perf`, so no gate,
/// no `long_leg`, no `spy_premium` and no `twin_groups` can reach it.
pub fn life_return(dates: &[NaiveDate], closes: &[f64], infl: Option<&BTreeMap<i32, f64>>) -> Option<f64> {
    let age = age_years(dates)?;
    let first = *closes.first()?;
    if first <= 0.0 || age < 0.5 {
        return None; // same guards as `life_cagr`: no divide by a junk first close, no <6mo "life"
    }
    let pct = (measure_endpoint(closes) - first) / first * 100.0;
    match infl.and_then(|s| inflation_compounded(s, age.round() as usize)) {
        Some(cum) => Some(real_pct(pct, cum)),
        None => Some(pct), // nominal: no series, or no inflation data covering that many years
    }
}

/// % the latest close sits below the simple moving average of the last `n` sessions (~a long-term
/// trend line; n≈1000 ≈ 200 weeks). 0 if at/above the average or history shorter than `n`. A
/// structural "cheap vs its own long trend" entry signal, distinct from the recency-biased 1Y-high
/// drawdown — buying below the multi-year trend, not just below last year's peak.
pub fn below_long_ma_pct(closes: &[f64], n: usize) -> f64 {
    if n == 0 || closes.len() < n {
        return 0.0;
    }
    let ma = closes[closes.len() - n..].iter().sum::<f64>() / n as f64;
    if ma <= 0.0 {
        return 0.0;
    }
    // (#19) deliberately the RAW last close, NOT measure_endpoint: smoothing this endpoint was
    // measured WORSE (backtest A/B at the 5-bar window: edge +115.7 smoothed vs +120.2 raw) — a
    // smoothed endpoint under-reads a fresh spike, so the overext brake docks parabolic names less.
    f64::max(0.0, (ma - *closes.last().expect("closes non-empty: closes.len() >= n >= 1 guarded above")) / ma * 100.0)
}

/// % the latest close sits ABOVE the moving average of the last `n` sessions — the mirror of
/// `below_long_ma_pct`. How far a name has run past its own long-term trend line; an
/// overextension/blow-off gauge for the growth lane (price 100% above its 200wk SMA = stretched).
/// 0 if at/below the average or history shorter than `n`.
pub fn above_long_ma_pct(closes: &[f64], n: usize) -> f64 {
    if n == 0 || closes.len() < n {
        return 0.0;
    }
    let ma = closes[closes.len() - n..].iter().sum::<f64>() / n as f64;
    if ma <= 0.0 {
        return 0.0;
    }
    // (#19) raw last close on purpose — see below_long_ma_pct; the brake must see the spike.
    f64::max(0.0, (*closes.last().expect("closes non-empty: closes.len() >= n >= 1 guarded above") - ma) / ma * 100.0)
}

/// (A) R² (0..1) of a straight-line fit to LOG price over time — how STEADILY the asset compounds. A
/// smooth exponential compounder → ~1; a lumpy path that mooned-then-chopped to the same endpoint →
/// lower. Damps CAGR's endpoint-luck (a lucky start/end pair on a jagged path isn't a durable trend).
/// 0 for <2 usable points; non-positive closes are skipped (log undefined). Flat = 1 (zero residual).
pub fn trend_r2(closes: &[f64]) -> f64 {
    let ys: Vec<f64> = closes.iter().filter(|&&c| c > 0.0).map(|c| c.ln()).collect();
    let n = ys.len();
    if n < 2 {
        return 0.0;
    }
    let xmean = (n as f64 - 1.0) / 2.0; // x = 0..n-1
    let ymean = ys.iter().sum::<f64>() / n as f64;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for (i, &y) in ys.iter().enumerate() {
        let dx = i as f64 - xmean;
        let dy = y - ymean;
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    if syy <= 0.0 || sxx <= 0.0 {
        return 1.0; // flat log-price = zero residual variance = perfectly "consistent"
    }
    (sxy * sxy / (sxx * syy)).clamp(0.0, 1.0)
}

/// (#14) Annualized CAGR (%) from the SLOPE of the least-squares log-price line — the same fit
/// `trend_r2` makes, returning the trend itself instead of its R². Robust to endpoint luck: one freak
/// start/end close barely moves a fitted line, unlike `cagr`, which is pure endpoint-to-endpoint and
/// so hostage to the exact first/last day. `cadence` = bars/year (252 daily, 12 monthly) annualizes
/// the per-bar log slope: CAGR = exp(slope × cadence) − 1. None for <2 usable points (log undefined /
/// degenerate fit); non-positive closes are skipped. Mirrors `trend_r2`'s loop so the two stay aligned.
pub fn trend_cagr(closes: &[f64], cadence: usize) -> Option<f64> {
    let ys: Vec<f64> = closes.iter().filter(|&&c| c > 0.0).map(|c| c.ln()).collect();
    let n = ys.len();
    if n < 2 {
        return None;
    }
    let xmean = (n as f64 - 1.0) / 2.0; // x = 0..n-1
    let ymean = ys.iter().sum::<f64>() / n as f64;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (i, &y) in ys.iter().enumerate() {
        let dx = i as f64 - xmean;
        sxx += dx * dx;
        sxy += dx * (y - ymean);
    }
    if sxx <= 0.0 {
        return None; // all x identical (n<2 already handled) -> no slope
    }
    let slope = sxy / sxx; // log-price per bar
    Some(((slope * cadence as f64).exp() - 1.0) * 100.0)
}

/// (C) Worst peak-to-trough decline (%) ever seen in the series — the deepest pain a holder endured.
/// One forward pass tracking the running peak. 0 for empty / never-down. Feeds the Calmar
/// (return-per-worst-pain) reward: a name that compounds hard with a SHALLOW max drawdown is durable.
pub fn max_drawdown_pct(closes: &[f64]) -> f64 {
    let (mut peak, mut worst) = (f64::MIN, 0.0_f64);
    for &c in closes {
        if c > peak {
            peak = c;
        }
        if peak > 0.0 {
            worst = worst.max((peak - c) / peak * 100.0);
        }
    }
    worst
}

/// (consistency footers) % of rolling ~`win_years`-year windows (`bars_per_year` bars/yr — 252 daily, 12 monthly) whose
/// endpoint return is positive, stepped weekly (5 sessions) so overlapping duplicates don't
/// drown short histories. The literal buy-and-hold question: how often did N patient years
/// pay? NOMINAL, not inflation-adjusted — the footer labels say so. None = history shorter
/// than one window (no windows means no claim, never a fake 100%); a non-positive close on
/// either end skips that window (halted/bad bars).
pub fn rolling_positive_pct(closes: &[f64], win_years: usize, bars_per_year: usize) -> Option<f64> {
    let win = win_years * bars_per_year;
    const STEP: usize = 5;
    let (mut pos, mut n) = (0usize, 0usize);
    let mut i = 0;
    while i + win < closes.len() {
        let (a, b) = (closes[i], closes[i + win]);
        if a > 0.0 && b > 0.0 {
            n += 1;
            pos += (b > a) as usize;
        }
        i += STEP;
    }
    (n > 0).then(|| 100.0 * pos as f64 / n as f64)
}

/// (underwater) Longest stretch of sessions spent below the running peak, in years (~252
/// sessions/yr) — MAXDD's missing twin: depth says how far down, this says how LONG until back
/// to even. An ongoing stretch counts at its elapsed length (whether it's underwater NOW is the
/// OFF-HI column's job). Non-positive closes (data holes) are filtered out first; a monotonic
/// riser legitimately reports 0.0; fewer than 2 usable closes -> None.
pub fn longest_underwater_yrs(closes: &[f64]) -> Option<f64> {
    let px: Vec<f64> = closes.iter().copied().filter(|c| *c > 0.0).collect();
    if px.len() < 2 {
        return None;
    }
    let (mut peak_i, mut peak, mut worst) = (0usize, px[0], 0usize);
    for (i, &c) in px.iter().enumerate().skip(1) {
        if c >= peak {
            peak = c;
            peak_i = i;
        } else {
            worst = worst.max(i - peak_i);
        }
    }
    Some(worst as f64 / 252.0)
}

/// (worst-Ny) The single worst rolling ~`win_years`-year (nominal) outcome — severity twin of
/// `rolling_positive_pct`'s frequency ("97% of windows positive; the worst one did −12%").
/// Same win/STEP walk and skip rules: a window with a non-positive close on either end is
/// skipped; `None` when no full window exists — no claim, never a fake number.
pub fn worst_rolling_pct(closes: &[f64], win_years: usize, bars_per_year: usize) -> Option<f64> {
    let win = win_years * bars_per_year;
    const STEP: usize = 5;
    let mut worst: Option<f64> = None;
    let mut i = 0;
    while i + win < closes.len() {
        let (a, b) = (closes[i], closes[i + win]);
        if a > 0.0 && b > 0.0 {
            let r = 100.0 * (b / a - 1.0);
            worst = Some(worst.map_or(r, |w: f64| w.min(r)));
        }
        i += STEP;
    }
    worst
}

/// (r11) Each COMPLETE calendar year's % return from the fetched daily window: last positive
/// close of year Y vs last positive close of year Y−1, for consecutive years only (a data hole
/// spanning a whole year breaks the chain rather than faking a multi-year "annual" number). The
/// current (partial) year is skipped — 1Y/1M columns already cover recency. Ascending year order;
/// empty when no complete pair exists (no claim).
pub fn calendar_year_returns(dates: &[NaiveDate], closes: &[f64]) -> Vec<(i32, f64)> {
    let mut last_by_year: BTreeMap<i32, f64> = BTreeMap::new();
    for (d, &c) in dates.iter().zip(closes) {
        if c > 0.0 {
            last_by_year.insert(d.year(), c); // ascending input -> keeps each year's LAST close
        }
    }
    let cur = dates.last().map_or(0, |d| d.year()); // partial year = the window's own end year
    last_by_year
        .iter()
        .zip(last_by_year.iter().skip(1))
        .filter(|((py, _), (y, _))| **y != cur && **y - **py == 1)
        .map(|((_, prev), (y, c))| (*y, 100.0 * (c / prev - 1.0)))
        .collect()
}

/// Format a number with comma thousands separators and 2 decimals (Python `{:,.2f}`).
pub fn fmt_money2(x: f64) -> String {
    let neg = x < 0.0;
    let s = format!("{:.2}", x.abs());
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), "00"));
    let bytes = int_part.as_bytes();
    let len = bytes.len();
    let mut grouped = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*b as char);
    }
    format!("{}{}.{}", if neg { "-" } else { "" }, grouped, frac)
}

/// (#18) Bars-per-year of the series the measurement fns are currently fed: 252 (daily, the live
/// screen — the default) or 12 (the long-horizon backtest's monthly bars). The backtest sets it once
/// per run so `measure_endpoint` can convert the config's TRADING-DAYS span into the same calendar
/// span in bars — the validated smoothing window means the same amount of TIME on either cadence
/// (train == serve). ponytail: process-wide atomic, fine because one backtest run = one cadence.
static MEASURE_CADENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(252);

/// Called by the backtest before scoring cutoffs built on non-daily bars (12 = monthly).
pub fn set_measure_cadence(bars_per_year: usize) {
    MEASURE_CADENCE.store(bars_per_year, std::sync::atomic::Ordering::Relaxed);
}

/// (#17/#18) The MEASUREMENT endpoint: mean of the closes in the last `endpoint_smooth_days`
/// TRADING DAYS (config; 1 = the raw last close). The span converts to bars by the current cadence
/// (`span × cadence / 252`, min 1 — e.g. 105 trading days ≈ 5 months = 105 daily closes live, 5
/// monthly bars in the 12y backtest), so the smoothing covers the same calendar time on both sides
/// -> train == serve. Long-horizon measurements — ≥1Y perf/CAGR legs (`horizon_changes`), the range
/// position (`price_pct_rank`), the drawdown (`pct_from_high`) — read their "current price" here, so
/// ONE bad print (or one hot week) on screen day can't flip the range gate or swing a rank. The
/// DISPLAYED price and the short legs (1D/1W/1M, incl. the 1M knife) stay the true last close, and
/// the overext brake deliberately reads raw too (see `above_long_ma_pct`). Panics on empty input,
/// same as the `.last().unwrap()` it replaces.
pub fn measure_endpoint(closes: &[f64]) -> f64 {
    let cadence = MEASURE_CADENCE.load(std::sync::atomic::Ordering::Relaxed);
    endpoint_avg(closes, span_to_bars(crate::config::endpoint_smooth_days(), cadence))
}

/// Trading-days span -> bar count at `cadence` bars/year (252 = daily -> identity), min 1.
fn span_to_bars(span_days: usize, cadence: usize) -> usize {
    (span_days * cadence / 252).max(1)
}

/// Mean of the last `n` closes (n clamped to [1, len]). Split from `measure_endpoint` so the math is
/// unit-testable without the process-wide config read.
fn endpoint_avg(closes: &[f64], n: usize) -> f64 {
    let n = n.clamp(1, closes.len());
    closes[closes.len() - n..].iter().sum::<f64>() / n as f64
}

/// Percentile rank (0..100) of the LAST close within the asset's OWN fetched history: ~0 = at its
/// all-time low, ~100 = at its all-time high. Self-normalizing across assets of wildly different
/// amplitude (BTC vs a penny alt) and robust to a single blow-off top — it's a rank, not a linear
/// (max−last)/(max−min) range one spike would distort. The buy "discount" uses 100−this (how deep in
/// its own history it trades). 0 for empty/one-point history.
pub fn price_pct_rank(closes: &[f64]) -> f64 {
    if closes.len() < 2 {
        return 0.0;
    }
    let last = measure_endpoint(closes);
    let below = closes.iter().filter(|&&c| c < last).count();
    below as f64 / (closes.len() - 1) as f64 * 100.0
}

/// % latest price sits below the period high. 0 if at/above high.
pub fn pct_from_high(prices: &[f64]) -> f64 {
    let high = prices.iter().cloned().fold(f64::MIN, f64::max);
    let last = measure_endpoint(prices);
    f64::max(0.0, (high - last) / high * 100.0)
}

/// Country/market from the ticker suffix. Crypto = global; no suffix = USA.
pub fn market_of(ticker: &str) -> String {
    if ticker.contains('.') {
        let suf = ticker.rsplit('.').next().unwrap().to_uppercase();
        return suffix_country(&suf).unwrap_or(&suf).to_string();
    }
    if crate::picks::is_currency_quoted(ticker) {
        // just "Crypto": "(global)" added nothing and a tight MARKET column clipped it to "Crypto ("
        // Suffix check, NOT any dash — BRK-B/BF-B are US share classes, not coins.
        return "Crypto".to_string();
    }
    "USA".to_string()
}

/// NUPL (Bitcoin Net Unrealized Profit/Loss) market-sentiment zone. Standard bands: below 0 the
/// market is underwater (capitulation); above ~0.75 it's historically frothy (euphoria). A whole-
/// market gauge, not a per-coin signal.
pub fn nupl_zone(nupl: f64) -> &'static str {
    match nupl {
        x if x < 0.0 => "Capitulation",
        x if x < 0.25 => "Hope/Fear",
        x if x < 0.5 => "Optimism/Anxiety",
        x if x < 0.75 => "Belief/Denial",
        _ => "Euphoria/Greed",
    }
}

/// GICS sectors counted as "tech" for screen's tech-only buy table. Apple/MSFT/NVDA are
/// Information Technology; Google/Meta/Netflix are Communication Services. (Amazon & Tesla are
/// GICS Consumer Discretionary, so they DON'T appear — add that sector string here to include them.)
/// Does `haystack` pass the configured `sectors` filter? Empty filter = keep everything (the default,
/// "fetch all sectors"); otherwise a case-insensitive substring match against ANY keyword. Used on
/// BOTH a stock's GICS sector string and an ETF's fund name (funds carry no GICS), so a single
/// keyword like "Technology" catches the GICS "Information Technology" AND an ETF named
/// "...Technology...". To fetch only tech, set `sectors: [Technology, Communication, Semiconductor]`.
pub fn sector_matches(haystack: &str, sectors: &[String]) -> bool {
    sectors.is_empty()
        || sectors.iter().any(|s| haystack.to_lowercase().contains(&s.trim().to_lowercase()))
}

/// Parse one S&P-500 constituents CSV row -> (Yahoo symbol, GICS sector), keeping it only if the
/// sector passes the `sectors` filter (empty = all sectors). Columns: Symbol, Security,
/// "GICS Sector", ... — Symbol and Sector carry no commas in this dataset, but the Security NAME
/// can (quoted, e.g. `"Casey's General Stores, Inc."`), which shifts the sector one column right
/// under a naive split. The sector rides along so the screen can print the top table's sector mix.
pub fn sector_symbol(csv_line: &str, sectors: &[String]) -> Option<(String, String)> {
    let cols: Vec<&str> = csv_line.splitn(5, ',').collect();
    let sym = cols.first()?.trim();
    // ponytail: only the ONE quoted comma seen in this dataset is handled; a two-comma name would
    // need a real CSV parser — add one only if the sector mix ever prints another garbage label.
    let name = cols.get(1)?.trim();
    let shifted = name.starts_with('"') && !name.ends_with('"');
    // a 2-column list (Symbol,Name — e.g. the nasdaq-100 CSV) carries no sector: keep the row
    // under "other" instead of dropping it (a sector-restricted screen still excludes it).
    let sector = match cols.get(if shifted { 3 } else { 2 }).map(|s| s.trim()) {
        Some(s) if !s.is_empty() => s,
        _ => "other",
    };
    if sym.is_empty() || !sector_matches(sector, sectors) {
        return None;
    }
    Some((yahoo_equity_symbol(sym), sector.to_string()))
}

/// Yahoo symbol form for a constituent-CSV ticker. US class-share dots become dashes
/// (BRK.B -> BRK-B), but a recognized European venue suffix is ALREADY Yahoo form and must keep
/// its dot — blanket replacement turned every FTSE/DAX pond name (AAF.L, ADS.DE) into a dead
/// symbol that fetched nothing. US class letters (A/B/C) don't collide with this venue list.
fn yahoo_equity_symbol(sym: &str) -> String {
    const VENUES: [&str; 12] = ["L", "DE", "PA", "AS", "MI", "MC", "SW", "ST", "CO", "OL", "HE", "LS"];
    match sym.rsplit_once('.') {
        Some((_, suffix)) if VENUES.contains(&suffix) => sym.to_string(),
        _ => sym.replace('.', "-"),
    }
}

/// (PIT) One name's S&P 500 membership, as HALF-OPEN spans `[start, end)`; `end: None` = still a member.
/// A `Vec` per name and not one span, because names LEAVE and COME BACK — AAL, AMD, CEG, DELL and 50
/// others do — and collapsing those into `first_start..last_end` would silently readmit a name for the
/// years it was out, which is the exact bias a point-in-time universe exists to remove.
pub type MemberSpans = std::collections::HashMap<String, Vec<(NaiveDate, Option<NaiveDate>)>>;

/// Parse the point-in-time membership source: `ticker,start_date,end_date`, one row per span, an empty
/// `end_date` meaning "still in the index today". Symbols are normalised to Yahoo form on the way in
/// (`BF.B` -> `BF-B`) so a caller can look the name up with the same string it fetches history for.
/// Malformed rows are skipped, and an unrecognizable document yields an empty map — which the caller
/// then treats as "no PIT data", never as "nobody was ever a member".
///
/// HALF-OPEN IS MEASURED, NOT ASSUMED, and it was worth measuring: the same publisher also ships a
/// 5.3 MB per-date SNAPSHOT file (`date,"TICKER,…"`, 2718 change dates, 1996-01-02..2026-06-30), and
/// this 27 KB span file is only a legitimate substitute if it reproduces it. Rebuilding all 2718
/// snapshots from these spans reproduces them EXACTLY — 0 mismatched dates — when `end` is EXCLUSIVE.
/// Read as inclusive it mismatches 604 of the 2718, with 756 extra names and 0 missing: the signature
/// of an off-by-one on the last day, on the 604 dates that ARE a departure. So the small file is
/// lossless, and `sp500_member_at` below must stay half-open.
pub fn sp500_spans(csv: &str) -> MemberSpans {
    let mut out = MemberSpans::new();
    for line in csv.lines().skip(1) {
        let mut cols = line.split(',');
        let (Some(sym), Some(start)) = (cols.next(), cols.next()) else { continue };
        let sym = sym.trim();
        let Ok(start) = start.trim().parse::<NaiveDate>() else { continue };
        // A MISSING end field is "still a member"; a PRESENT but garbled one drops the row instead.
        // The difference matters: silently defaulting a bad date to None promotes a long-dead ticker
        // to a current constituent, which is a survivorship bug reintroduced by the parser itself.
        let end = match cols.next().map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(s) => match s.parse::<NaiveDate>() {
                Ok(d) => Some(d),
                Err(_) => continue,
            },
        };
        if sym.is_empty() {
            continue;
        }
        out.entry(yahoo_equity_symbol(sym)).or_default().push((start, end));
    }
    out
}

/// (#173) Union of two point-in-time membership maps — the S&P 500 spans plus whatever
/// `Urls.membership_csv` supplied. A ticker carried by BOTH keeps both sets of spans rather than one
/// overwriting the other: a name can be an S&P MidCap 400 constituent for some years and an S&P 500
/// one for others, and dropping either half would silently shorten its point-in-time life.
///
/// Sorted and deduped per ticker so the result does not depend on which source was read first —
/// `sp500_member_at` is an `any()` and would tolerate duplicates, but `pit_pool` builds the POOL from
/// `spans.keys()` and a run has to be reproducible bar-for-bar at any thread count.
///
/// Merging an EMPTY map returns the base unchanged, which is the off switch the default relies on.
pub fn merge_spans(mut base: MemberSpans, extra: MemberSpans) -> MemberSpans {
    for (ticker, spans) in extra {
        let slot = base.entry(ticker).or_default();
        slot.extend(spans);
        slot.sort();
        slot.dedup();
    }
    base
}

/// Was this name in the index on `on`? HALF-OPEN: the start date counts, the end date does not — see
/// `sp500_spans` for the 2718-snapshot check that settled which way round.
pub fn sp500_member_at(spans: &[(NaiveDate, Option<NaiveDate>)], on: NaiveDate) -> bool {
    spans.iter().any(|(start, end)| *start <= on && end.is_none_or(|e| on < e))
}

/// (Item 32) Extract (Yahoo symbol, GICS sector) rows from a Wikipedia "List of S&P N companies"
/// page (the maintained source for the MidCap 400 — no living CSV exists). Anchors on the
/// `id="constituents"` table; per row, cell 0's text = ticker, cell 2's = sector. The tag-strip is
/// a dumb <...>-skipper — plenty for wiki cells. Malformed rows are skipped; an unrecognizable
/// page yields an empty vec, so the pond just drops like a failed CSV fetch (never crashes).
pub fn wiki_constituents(html: &str, sectors: &[String]) -> Vec<(String, String)> {
    let Some((_, rest)) = html.split_once("id=\"constituents\"") else { return Vec::new() };
    let table = rest.split("</table>").next().unwrap_or("");
    let strip = |cell: &str| {
        let mut out = String::new();
        let mut in_tag = false;
        for c in cell.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.trim().to_string()
    };
    table
        .split("<tr")
        .skip(2) // fragment before the first <tr> + the header row
        .filter_map(|row| {
            let cells: Vec<String> =
                row.split("<td").skip(1).filter_map(|c| c.split_once('>')).map(|(_, body)| strip(body)).collect();
            let (sym, sector) = (cells.first()?.as_str(), cells.get(2)?.as_str());
            if sym.is_empty() || sector.is_empty() || !sector_matches(sector, sectors) {
                return None;
            }
            Some((sym.replace('.', "-"), sector.to_string()))
        })
        .collect()
}

/// Parse the Euronext Lisbon equities DataTables payload -> Yahoo `.LS` tickers. The request
/// (`fetch_euronext_lisbon`) asks for columns `name,isin,symbol,market,...`, so each `aaData` row is
/// an array with the bare symbol at index 2 (e.g. "GALP"); append `.LS` for the Yahoo form
/// ("GALP.LS"). Keeps only plain A-Z0-9 symbols (drops empty/odd cells). Empty Vec on a missing or
/// reshaped payload — the caller then degrades to an empty leg, never a crash.
pub fn euronext_lisbon_symbols(payload: &Value) -> Vec<String> {
    payload
        .get("aaData")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let sym = r.get(2)?.as_str()?.trim();
                    (!sym.is_empty() && sym.chars().all(|c| c.is_ascii_alphanumeric()))
                        .then(|| format!("{sym}.LS"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ISIN shape check (2-letter country + 9 alphanumerics + check digit) — the venue-list parsers
/// use it to drop header junk / reshaped cells without a regex dependency.
fn is_isin(s: &str) -> bool {
    s.len() == 12
        && s.chars().take(2).all(|c| c.is_ascii_uppercase())
        && s.chars().skip(2).take(9).all(|c| c.is_ascii_alphanumeric())
        && s.chars().nth(11).is_some_and(|c| c.is_ascii_digit())
}

/// Parse the SIX fund-list payload (`fqs/snap.json`, `rowData` = `[ISIN, ShortName]` arrays) ->
/// ISINs of rows that LOOK like exchange-traded funds. The FU segment mixes real ETFs with Swiss
/// mutual funds (LGT PB / Robeco share classes) and CHF-hedged clones; resolvable mutual funds
/// would be force-classified as ETFs downstream, so keep only names carrying an "etf" or "ucits"
/// token. Misses short-named clones ("X GL GOV 3D CHF") — CHF classes of strategies whose main
/// line the pond already has. Empty Vec on a missing/reshaped payload.
pub fn six_fund_isins(payload: &Value) -> Vec<String> {
    payload
        .get("rowData")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let isin = r.get(0)?.as_str()?.trim();
                    let name = r.get(1)?.as_str()?.to_lowercase();
                    (is_isin(isin) && (name.contains("etf") || name.contains("ucits")))
                        .then(|| isin.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the Euronext ETF-list ("track") DataTables payload -> ISINs. Same request shape as
/// `euronext_lisbon_symbols` (columns `name,isin,symbol,market`), but here the useful cell is the
/// ISIN at index 1 — the symbol is venue-local and useless to Yahoo, so the caller bridges
/// ISIN -> Yahoo symbol exactly like the Börse Frankfurt rows. Keeps only ISIN-shaped cells
/// (2 letters + 9 alphanumerics + check digit). Empty Vec on a missing/reshaped payload.
pub fn euronext_track_isins(payload: &Value) -> Vec<String> {
    payload
        .get("aaData")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let isin = r.get(1)?.as_str()?.trim();
                    is_isin(isin).then(|| isin.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ISIN/domicile prefixes an EU retail account cannot buy a fund from (PRIIPs — no KID). Read by the
/// FIRDS funnel, which screens thousands of US/CA/Asia funds listed on EU MTFs out of the dumps, and
/// (#102) by the CORE admission rule's UCITS leg: "not on this list" is this file's ONE definition of
/// a fund domicile a European can hold, and two readers of it must not drift into two lists.
pub const NON_EU: [&str; 17] = [
    "US", "CA", "HK", "JP", "SG", "KY", "AU", "IL", "ZA", "TW", "KR", "IN", "TH", "MY", "CN", "BM",
    "VG",
];

/// Is this ETF name a GEOGRAPHIC market index (the kind you anchor a 20-year hold on), as opposed to
/// a single-sector / thematic / factor tilt? True = carries a geography token AND no narrow token, so
/// "S&P 500 Information Technology" (has "s&p 500" but also "information"/"technolog") is correctly
/// NOT a core, while "Vanguard S&P 500 UCITS ETF" is. Name-token heuristic — lowercased, substring
/// match, same style as the venue-list funnels above. See `GEO` for the rule and what it replaced.
pub fn is_broad_index_name(name: &str) -> bool {
    geo_tier(&name.to_lowercase()).is_some()
}

/// How many breadth tiers `hold_breadth_tier` can return. Anything sizing a per-tier array MUST read
/// this — `hold_core_list` indexed a hardcoded `[0u8; 3]` by tier, so a fourth tier was an
/// index-out-of-bounds panic rather than a display bug.
pub const HOLD_TIERS: usize = 11;

/// (round 118) The CORE admission rule, ONE table, read by both `is_broad_index_name` (does it match
/// at all) and `hold_breadth_tier` (which tier) — they cannot drift apart the way two parallel lists
/// would. A name qualifies iff it names a GEOGRAPHIC partition of the market and carries no NARROW
/// token.
///
/// THE RULE IS GEOGRAPHY, NEVER INDUSTRY OR STYLE. A region is a sleeve you can hold for 20 years;
/// an industry, a factor, a screen or a currency hedge is a bet on being right about something. That
/// line is what keeps this list from growing without limit now that it admits slices at all.
///
/// THIS REPLACES "a regional exclusion is a bet, not the whole market", which is why `world ex`,
/// `acwi ex` and `ex-usa` moved OUT of NARROW: US + World-ex-US is a perfectly good partition, and
/// the old rule barred it while admitting MSCI World (developed only) and the S&P 500 (US only) —
/// slices by any honest reading. The list stopped being "one fund forever" the day it had three
/// tiers; this makes that explicit instead of pretending otherwise.
///
/// CAP-BREADTH WAS CONSIDERED AND REJECTED: admitting IMI / all-cap / small-cap as a "coverage"
/// dimension cannot, by geography alone, tell "ACWI IMI" (broad, happens to include small caps) from
/// "MSCI World Small Cap" (a size TILT). `small` and `mid cap` therefore stay in NARROW. Revisit only
/// with an explicit completes-a-partition test, not by adding tokens.
///
/// ORDER MATTERS — first match wins, and a token that is a SUPERSTRING of another must come first.
/// Two real collisions: "msci world ex usa" contains "msci world" (would rank an ex-US sleeve AS
/// developed, i.e. above MSCI World itself), and "ftse developed europe" contains "ftse developed".
/// Both are pinned by tests.
///
/// (#215) THREE SPELLINGS, ONE RECEIPT — `prime global` (1), `ftse japan` (6), ` us equity` (3).
/// Sourced by reading the WHOLE `GEO blind spot` list rather than the top 15 by AUM the run prints:
/// 47 funds clear every admission leg but this one, and they sort 29 BOND funds, 13 single-country
/// or narrow-regional names (DAX x4, Euro Stoxx 50 x3, MSCI EMU x2, FTSE 100, Korea, India, China)
/// and just 5 broad-equity names. Two of those 5 are DELIBERATELY LEFT BLIND, because a fund must
/// land in the tier its index actually tracks and neither could be verified offline:
/// L&G Global Equity (LGGL.L, EUR 1.5B) — tier 0 or 1 unresolved — and Vanguard FTSE North America
/// (VNRA.L, EUR 2.7B), whose US+Canada geography is not any sleeve here. Guessing either is a
/// misfile, so they stay in the blind spot with this note as the pointer.
///
/// THE BOND HAZARD IS THE WHOLE RISK OF THIS ROUND, and it is structural: `NARROW` carries no bond
/// token and there is NO asset-class leg, so GEO alone is what keeps fixed income out of an equity
/// table. The blind spot holds `iShares Global Aggregate Bond` — live proof that a generic "global"
/// token would admit bonds. Hence specific tokens only, and the bond/single-country negative pins in
/// `pure_logic` are the guard, not decoration.
///
/// MEASURED 2026-09-02, live `screen`, control and probe on an IDENTICAL pond (`4797 from cache`,
/// 4584 EU-buyable ETFs, both runs — the clean pair (#213) and (#214) both had to argue around).
/// 26 rows both sides, and every swap is family-for-family: each fund evicted was the SECOND of a
/// family the sleeve already showed, each admitted is a family it showed none of.
///
///     developed  OUT SPPW.DE MSCI World 0.12% EUR 17.1B  IN F50A.DE Prime Global 0.05% EUR 2.8B
///     US         OUT WEBH.DE MSCI USA   0.03% EUR  3.7B  IN LGUS.L  L&G US Eq    0.05% EUR 1.3B
///     Japan      OUT DBXJ.DE MSCI Japan 0.12% EUR  5.6B  IN VJPA.DE FTSE Japan   0.10% EUR 1.4B
///
/// Net TER across the three: -0.07pp, i.e. the table got CHEAPER as well as more diversified. Sleeve
/// families went developed 2f->3f, US 2f->3f, Japan 1f->2f; blind spot 47 -> 44; QUALIFIED 48 -> 51.
///
/// THE SAFETY PROOF IS THE +3, NOT THE 44. Leg 0 newly admitted 44 names, but QUALIFIED rose by
/// exactly 3 — the other 41 die on TER (+22), replication (+15), Acc (+2) and AUM (+2). NO bond
/// fund qualified, which is the outcome the guard exists to produce. Re-check that +3 against the
/// blind-spot delta on any future token: they must move together.
///
/// NOT GRADEABLE by backtest, same as (#203)/(#211)/(#213): `backtest::stamp_asset_class` fills only
/// name/instrument_type/sector, so every backtest quote dies on its TER leg and no walk reaches this
/// lane. Judgement-plus-live-diff.
///
/// REVERT: drop the three tokens. That returns 47 to the blind spot and puts SPPW/WEBH/DBXJ back.
/// Revert an INDIVIDUAL token if a later pond has it admit a bond, a commodity or a single-country
/// index, or file a fund in a tier its index does not track.
const GEO: [(&str, u8); 28] = [
    // 4 = ex-US sleeves. FIRST, because every one of them contains a broader token.
    ("acwi ex", 4), ("world ex", 4), ("ex-usa", 4),
    // 5 = Europe. "ftse developed europe" precedes the generic "ftse developed" below.
    ("ftse developed europe", 5), ("msci europe", 5), ("stoxx europe 600", 5),
    // 6 = Japan
    // (#215) "ftse japan" is the PUREST form of (#203)'s finding: the sleeve already exists and GEO
    // simply could not spell it. Vanguard FTSE Japan (VJPA.DE, EUR 1.4B, 0.10%) sat in the blind
    // spot beside three MSCI/TOPIX trackers. It also gives tier 6 a SECOND index family, which is
    // what lets (#214)'s family-first fill reach it in a sleeve whose supply (4) exceeds its cap.
    ("msci japan", 6), ("topix", 6), ("ftse japan", 6),
    // 7 = Asia-Pacific. (#213) SPLIT OUT OF JAPAN, which was the one sleeve holding two disjoint
    // geographies. Because the per-sleeve cap ranks on TER *inside* a sleeve, a 0.20% Pacific fund
    // lost the sort to three 0.12% Japan trackers and the CORE table carried no Australia/HK/
    // Singapore row at all — the same failure `ter_cap_for` records for small caps ("a 0.35%
    // small-cap fund placed in the developed sleeve loses the TER sort to IWDA at 0.20% and is never
    // printed"), and the same one (#207) answered for tier 0. Ordered AFTER Japan on the sibling
    // rule the ladder already uses for Europe-before-Japan: regional weight, not nesting.
    //
    // (#213) "msci pac " exists to spell iShares' ABBREVIATED "Core MSCI Pac ex-Jpn" (CSPXJ.SW,
    // EUR 3.5B), which sat in the GEO blind spot because no token could reach it. Its TRAILING
    // SPACE is DEFENSIVE, not load-bearing: today the only thing it excludes is "MSCI Pacific",
    // which the next entry files at the SAME tier anyway, so no test can observe the difference.
    // It is written the way "msci em " and NARROW's "sri " are written because the failure mode is
    // real (a future "MSCI Pac<letter>" index would be swallowed silently) even though it has not
    // fired here — unlike (#211), where dropping the space genuinely misfiled a EUR 3.9B fund.
    //
    // The sleeve is labelled "Asia-Pacific" and NOT "ex-Japan" deliberately: plain MSCI Pacific
    // INCLUDES Japan, so an ex-Japan label would over-claim on the one token that is a superset.
    //
    // MEASURED 2026-09-02, live `screen`, control and probe BACK TO BACK in one session on the same
    // cache (`4797 from cache`, 5427 tickers, both runs). The CORE table went 25 rows -> 26, and the
    // ticker-set diff is ONE addition and ZERO removals — the pre-registered stop rule was "revert if
    // the split evicts a printed fund or leaves the new sleeve empty", and neither fired:
    //
    //     CSPXJ.SW  0.20%  EUR 3.5B  iShares Core MSCI Pac ex-Jpn   <- NEW, 16.6y, Acc, Full, IE
    //
    // Sleeve supply split `Japan/AsiaPac 4` -> `Japan 4 · Asia-Pacific 1`: the 4th name tier 6 was
    // already hiding is a JAPAN fund, so the split did not merely re-file what was there — the new
    // sleeve is filled entirely by the name `msci pac ` newly spells, and QUALIFIED moved 47 -> 48.
    // The funnel's other counts drifted between the two runs (EU-buyable 4551 -> 4574) on network
    // flakiness, NOT on this change, which is why the TICKER-SET DIFF is the attribution argument
    // here and the funnel line is not.
    //
    // JUDGEMENT-PLUS-LIVE-DIFF, NOT GRADED — no backtest quote carries a TER, so `stamp_asset_class`
    // kills every one of them on that leg and no walk reaches this lane. Same permanent
    // ungradeability as `hold_per_tier` and (#207).
    //
    // REVERT: fold tier 7 back into 6 and drop "msci pac ". That restores one mixed sleeve and puts
    // CSPXJ.SW back in the GEO blind spot, where (#213) found it.
    ("msci pac ", 7), ("msci pacific", 7),
    // 0 = the whole planet, DM+EM in one fund
    ("all-world", 0), ("all-country", 0), ("all country world", 0), ("acwi", 0), ("global all cap", 0),
    ("solactive gbs global markets", 0),
    // 1 = developed markets
    // (#215) "prime global" spells the SAME index the line above already names — Amundi's Prime
    // range omits the index from the fund name, so "Amundi Prime Global UCITS ETF Acc" (F50A.DE,
    // EUR 2.8B, 0.05%) was unreachable though `solactive gbs developed markets` was already here.
    // DEVELOPED, not tier 0: Prime Global tracks Solactive GBS Developed Markets, and its Prime
    // sibling "Amundi Prime All Country World" (WEBN.DE) is the ACWI one and already files at tier 0
    // on `all country world` — the two must not collide, and they do not share a token.
    ("msci world", 1), ("ftse developed", 1), ("solactive gbs developed markets", 1),
    ("prime global", 1),
    // 2 = emerging markets — ABOVE the US deliberately: DM+EM spans the planet, US alone does not
    // (#211) "msci em " carries a TRAILING SPACE and must keep it: without it the token matches
    // "MSCI EMU" (UBS Core MSCI EMU, EUR 3.9B) and files a EUROZONE fund as emerging markets. Same
    // guard `NARROW`'s "sri " uses. The sub-regions it still reaches ("MSCI EM Asia", "MSCI EM ex
    // China") are refused in NARROW below, because GEO's first-match-wins ordering can express a
    // DIFFERENT TIER but never a refusal.
    ("emerging", 2), ("msci em ", 2),
    // 3 = the US
    // (#215) " us equity" carries a LEADING SPACE and it is LOAD-BEARING, not defensive — the
    // (#211) test, not the (#213) one. `hold_name_tokens` ships OFF, so matching is bare `contains`
    // and the bare token matches "foc-us equity"; the space is the only thing between this tier and
    // every "Focus Equity" fund in the pond. Admits L&G US Equity (LGUS.L, EUR 1.3B, 0.05%).
    // "MSCI USA Equity" is NOT a collision: that reads " usa equity", which the token does not
    // match, and `msci usa` above files it at this same tier anyway.
    ("s&p 500", 3), ("msci usa", 3), ("crsp us total market", 3), ("russell 1000", 3),
    (" us equity", 3),
];

/// sector/thematic/tilt tokens that disqualify a geographic core: single sectors (Nasdaq-100 is a
/// tech concentration), ESG/SRI screens (a filtered subset), factor tilts
/// (value/momentum/quality/min-vol/equal-weight), size tilts, and currency-hedged classes (hedge-cost
/// drag ≈ the interest-rate differential/yr — not the canonical hold).
/// "minimum vol" + " pab": live CORE receipts — funds spell the tilt out ("MSCI World Minimum
/// Volatility") or abbreviate the ESG screen ("MSCI World PAB" = Paris-Aligned Benchmark), and the
/// "min vol"/"paris" tokens miss both. " pab" keeps its leading space so a name merely containing
/// the letters (e.g. a provider string) can't false-positive.
/// (#200) "industrial" was MISSING, and that is a different defect from the mid-word accident
/// (#195) tested for — a token absent from the list, not one matching too loosely. Without it
/// `geo_tier` read "Xtrackers MSCI World Industrials" as a DEVELOPED sleeve and "SPDR MSCI Europe
/// Industrials" as a EUROPE sleeve, so two single-sector funds were admitted to a table whose own
/// header calls itself "broad geographic sleeves". Found by (#197) while lifting `hold_per_tier`:
/// both sat among the 22 rows the ≤3/sleeve cap hides, which is why the printed table never showed
/// the bug and no receipt caught it. NDUS.L clears every other leg on live facts (TER 0.18%, AUM
/// €1.24B), so this token is the only thing standing between it and the CORE list.
const NARROW: [&str; 36] = [
    "technolog", "information", "info tech", "financ", "semiconduct", "health", "energy",
    "industrial",
    "sector", "select", "nasdaq", "small", "mid cap", "communicat", "biotech",
    "esg", "sri ", "socially responsible", "screened",
    "sustainab", "paris", " pab", "climate", "islamic", "value", "momentum", "quality",
    "equal weight", "min vol", "minimum vol", "hedged",
    // (#211) EM SUB-REGIONS. `("msci em ", 2)` above is what makes these reachable at all, and a
    // sub-region is not the tier it would inherit: "EM Asia" is a region inside a region, and
    // "EM ex China" excludes the largest constituent of the index it names, which is a bet rather
    // than a partition (there is no China sleeve to complete it with — unlike ex-USA, which round
    // 118 admitted at tier 4 precisely because US + World-ex-US IS one). Revisit with a tier, not
    // by deleting these.
    "em asia", "em ex",
    // (#216) THE SAME ARGUMENT ONE LEVEL UP, found by probing why the ex-US sleeve sits at 2 of 3.
    // `("world ex", 4)` is a wildcard for "world ex <anything>", and the live pond holds three names
    // it was filing in a US-relative sleeve. "World ex Europe" and "World ex EMU" are BETS, not
    // partitions, by the test the "em ex" note above already states: there is no Europe-complement
    // sleeve to rebuild the world with, whereas US + World-ex-US IS one, which is why "ex-usa"
    // stays. "World ex Mega Cap" is not geography at all — it is a SIZE tilt, and belongs beside
    // "small" and "mid cap" for the reason those are here.
    //
    // Tightening the GEO token to "world ex us" was written first and REVERTED: it relocates the
    // misfile rather than removing it (every one of these also contains "msci world", so they fall
    // straight through to tier 1) and it costs Vanguard FTSE All-World Ex-U.S. its only token.
    // NARROW runs BEFORE `geo_hit`, so refusing here is what actually keeps them out of every tier.
    //
    // INERT TODAY, DELIBERATELY FIXED ANYWAY: all three die on the TER leg in this pond, so no row
    // moves. Two of them (CE8.PA, CM9.PA) are 0.35% against a 0.25% cap and never will. WXMEG.SW is
    // the live hazard — it dies only on "TER unknown", so a pond that serves UBS a TER files a
    // mega-cap-excluding size tilt in the ex-US GEOGRAPHIC sleeve.
    //
    // REVERT: drop the three tokens. That returns CE8.PA/CM9.PA/WXMEG.SW to tier 4. Revert an
    // INDIVIDUAL token if it ever refuses a fund whose index IS a world-minus-US partition.
    "world ex europe", "world ex emu", "ex mega cap",
];

/// (#102) Does an ALREADY-lowercased `n` carry `t` at the START OF A WORD? The tightened matcher
/// behind `hold_name_tokens`, and deliberately word-START rather than whole-word: half of `NARROW`
/// is an intentional prefix ("technolog" must catch Technology AND Technologies), so a whole-word
/// rule would silently un-disqualify the sector funds this list exists to bar. A token that already
/// begins with its own separator (" pab") is a hand-rolled guard for this exact problem and is left
/// to do its job — the boundary is in the token, not around it.
fn token_at_word_start(n: &str, t: &str) -> bool {
    if !t.starts_with(char::is_alphanumeric) {
        return n.contains(t);
    }
    n.match_indices(t).any(|(i, _)| !n[..i].chars().next_back().is_some_and(char::is_alphanumeric))
}

/// Does token `t` fire on the ALREADY-lowercased name `n`? The one place the two matching regimes
/// live: `hold_name_tokens` ON means word-start only, OFF (what ships) means plain substring. It was
/// a closure inside `geo_tier`; (#201) lifted it so the two halves below can be asked separately
/// without either re-spelling the rule (non-negotiable #4).
fn hit(n: &str, t: &str) -> bool {
    if crate::config::hold_name_tokens() { token_at_word_start(n, t) } else { n.contains(t) }
}

/// (#201) WHICH `NARROW` token disqualifies an ALREADY-lowercased name — the fact `geo_tier` computes
/// for every fund in the pond and then throws away, exactly as `hold_miss_reason` threw away the leg
/// index before (#199). Ten rounds have argued about this list without anyone being able to see what
/// it costs; `picks::narrow_census` is the reader that finally prices it.
///
/// Returns the FIRST token in `NARROW` ORDER when several fire, so "MSCI World Small Cap ESG" is
/// reported under "small" and not "esg". The census inherits that convention — its counts are
/// "refused FIRST on this token", never "contains this token".
pub fn narrow_hit(n: &str) -> Option<&'static str> {
    NARROW.iter().copied().find(|t| hit(n, t))
}

/// (#201) The GEO tier an ALREADY-lowercased name matches IGNORING the narrow disqualifier — i.e.
/// "is this name geographically broad at all". `geo_tier` is exactly this AND `narrow_hit` being
/// empty; the census needs the halves apart so it can count the funds that ARE broad by geography and
/// are refused only by a token, which is the supply behind every "should the lane admit factor X".
pub fn geo_hit(n: &str) -> Option<u8> {
    GEO.iter().find(|(t, _)| hit(n, t)).map(|(_, tier)| *tier)
}

/// (#214) WHICH `GEO` token matched — the INDEX FAMILY, not the tier. Runs the SAME
/// first-match-wins scan `geo_hit` runs, so the two can never disagree about what fired
/// (non-negotiable #4: one definition, read two ways, not two definitions).
///
/// A tier answers "which part of the world"; a family answers "which index over it". They are
/// different questions and `hold_core_list` needs both: its cap is documented as existing "so no
/// index family crowds out the others", but the cap counts FUNDS, so when one family owns the two
/// cheapest funds in a sleeve it takes two of the three slots. Live 2026-09-02: the US sleeve
/// printed `s&p 500` once and `msci usa` TWICE out of a supply of 13.
///
/// Takes the RAW name and lowercases it, the way [`hold_breadth_tier`] does — every caller has a
/// `Quote::name` in hand, and a second already-lowercased variant would just be a second way to get
/// this wrong.
pub fn geo_family_of(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    GEO.iter().find(|(t, _)| hit(&n, t)).map(|(t, _)| *t)
}

/// (#217) The family key `(#214)`'s family-first fill should spend a slot on — the FACTOR token for
/// a fund in the factor sleeve, the GEO family for everything else.
///
/// Without this the new sleeve defeats the mechanism built to protect it. Every world-factor fund
/// carries the SAME geo token (`msci world`), so [`geo_family_of`] sees ONE family across all of
/// them and family-first would happily spend all three slots on three quality wrappers — precisely
/// the failure `(#214)` exists to prevent, reintroduced by the sleeve that needs it most.
///
/// The knob is deliberately NOT read here. A factor fund only reaches a caller of this after
/// `hold_suitable` admitted it, and with the sleeve off leg 0 refuses every one of them — so the
/// branch is unreachable when off, and reading the knob would buy nothing but an ungradeable line.
/// `narrow_hit` returns the FIRST token, and [`factor_sleeve_tier`] already guaranteed no other one
/// fires, so for an admitted fund that token IS the factor family.
pub fn sleeve_family_of(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    match narrow_hit(&n) {
        Some(t) if FACTOR.contains(&t) || SECTOR.contains(&t) => Some(t),
        _ => geo_family_of(name),
    }
}

/// (#214) The order to FILL a sleeve in, given each candidate's index family in the sleeve's
/// existing `(domicile, TER, AUM)` order. One pass takes the best fund of each DISTINCT family, in
/// the order those families first appear; everything else follows in its ORIGINAL order.
///
/// `on == false` returns the identity order — today's behaviour byte-for-byte, which is what
/// non-negotiable #1 requires of the knob that feeds this.
///
/// RE-ORDERS, NEVER DROPS, and that is the whole design: the family key is `GEO`'s own token and
/// therefore COARSE. Europe prints "FTSE Developed Europe" and "FTSE Developed Europe ex UK" under
/// ONE token though they are different indices, so a hard one-per-family cap would delete a real row
/// to satisfy a key that cannot see the difference. Here every candidate still appears and the cap
/// still fills; only the contested slot changes hands.
///
/// EXACTLY ONE family-first pass, then the original order — load-bearing, and measured. A full
/// round-robin (2nd of each family, then 3rd…) reorders the TAIL by family too, and the 2026-09-02
/// probe caught it doing damage: the US sleeve swapped WEBH.DE (msci usa, 0.03%) out for SP5AU.SW
/// (s&p 500) purely because `s&p 500` appears first, when BOTH families were already on the table.
/// That is a reorder inside a solved problem. Once every family is represented there is nothing
/// left to diversify, so the sleeve's own ranking must resume.
///
/// The knob is a PARAMETER, not a read, for the reason [`ter_cap_for`] and `(#204)` both record —
/// it is a process-wide `OnceLock` no test can flip, so a branch reachable only on an off-default
/// is a branch the mutation gate cannot grade.
///
/// Takes `(tier, family)` per candidate and does its OWN grouping, so the caller stays a
/// three-liner. The alternative — splitting the tier runs at the call site inside `hold_core_list`
/// — was written first and thrown away: it put twelve mutants of index arithmetic and loop bounds
/// into a function the gate can barely reach, several of them killable only by panic or timeout.
/// A stable sort by `(tier, not-first-of-family)` says the same thing with no arithmetic at all:
/// stability IS the "preserve the sleeve's own order" rule, not an implementation detail of it.
pub fn family_first_order(keys: &[(u8, Option<&str>)], on: bool) -> Vec<usize> {
    let mut out: Vec<usize> = (0..keys.len()).collect();
    if !on {
        return out;
    }
    // The key is (tier, family): the SAME token in two sleeves is two families, because the
    // question is "does this sleeve already show that index", and a sleeve is a tier.
    let mut seen: Vec<(u8, &str)> = Vec::new();
    let first: Vec<bool> = keys
        .iter()
        .map(|(t, f)| {
            // a name with no GEO token shares one key: two unspellable names are not two families
            let k = (*t, f.unwrap_or(""));
            let fresh = !seen.contains(&k);
            if fresh {
                seen.push(k);
            }
            fresh
        })
        .collect();
    out.sort_by_key(|&i| (keys[i].0, !first[i]));
    out
}

/// (#202) The `NARROW` tokens the size sleeve rehabilitates — market-cap SEGMENT, nothing else.
/// "value", "quality", "momentum", "equal weight" and the volatility tilts are deliberately absent:
/// they re-weight stocks the lane already owns through MSCI World, which is a tilt, not an exposure.
/// A small-cap fund holds companies no all-world large-cap tracker holds at all.
const SIZE: [&str; 1] = ["small"];

/// (#202) The eighth sleeve, ranked last (least diversified). Its own tier because a 0.35% small-cap
/// fund dropped into the developed sleeve loses the cheapest-TER sort to a 0.20% MSCI World tracker
/// and would never be printed — the sleeve is what makes it visible, not the token alone.
/// (#202) …no longer the LAST tier, since (#217) put the factor sleeve after it. Spelled RELATIVE
/// to `FACTOR_TIER` rather than as a literal 8: the two optional sleeves must stay adjacent at the
/// end of the ladder, and a literal here is how a third one would silently overwrite this one.
pub const SIZE_TIER: u8 = FACTOR_TIER - 1;

/// (#217) The `NARROW` tokens the FACTOR sleeve rehabilitates. Deliberately three, and deliberately
/// not five: "momentum" is a high-turnover bet that contradicts a 20-year hold, and "min vol"/
/// "minimum vol" target VOLATILITY rather than return, so neither belongs in a table whose header
/// says buy-and-hold. Chosen by the user; the omission is a scope decision, not a measurement.
///
/// THIS OVERRIDES A SHIPPED PRINCIPLE, BY DECISION AND NOT BY EVIDENCE. `(#210)`'s receipt states
/// that small caps are "a different slice of the market, not a re-weighting of names MSCI World
/// already holds, which is why value/quality/momentum are deliberately NOT in `core::SIZE`". That
/// was never priced. It has now been overridden on purpose — see the `(#217)` receipt in
/// tests/ci-settings.yaml, which records both halves so a future round can revert on the principle
/// alone without re-running anything.
const FACTOR: [&str; 3] = ["quality", "value", "equal weight"];

/// (#217) The tenth sleeve, ranked last. Its own tier for the reason `SIZE_TIER` has one: a factor
/// fund dropped into the developed sleeve loses the cheapest-TER sort to a 0.20% MSCI World tracker
/// and would never print. UNLIKE the size sleeve it carries NO TER ceiling of its own — [`ter_cap_for`]
/// is deliberately untouched — because the live pond has three funds clearing the BASE 0.25% cap,
/// one per factor family. There is no cap here to walk upward, which is the hazard `(#202)`/`(#210)`
/// each had to pre-register against.
/// (#218) …no longer the LAST tier either, since the sector sleeve went after it. Spelled RELATIVE
/// to `SECTOR_TIER` for the reason `SIZE_TIER` is spelled relative to this one: the optional sleeves
/// must stay adjacent at the end of the ladder, and a literal is how the next one silently
/// overwrites its neighbour.
pub const FACTOR_TIER: u8 = SECTOR_TIER - 1;

/// (#217) Which geographies earn the factor sleeve — tier 0 and 1 ONLY, for `SIZE_GEO`'s reason
/// verbatim: the sleeve is labelled `world factor`, and a EUROPE quality fund in it is two bets
/// (region × factor) wearing the name of one, when the lane already carries a Europe sleeve for the
/// region half. This is not hypothetical here either: the live pond holds 53 regional factor funds
/// against 13 world ones, so an unrestricted sleeve would fill on the wrong axis.
const FACTOR_GEO: [u8; 2] = [0, 1];

/// (#218) The `NARROW` tokens the SECTOR sleeve rehabilitates — a GICS-style slice of the market.
/// "nasdaq" is deliberately absent: the Nasdaq-100 is a broad growth index, not a sector, and
/// admitting it here would file it under a label that misdescribes it.
///
/// THIS OVERRIDES THE SAME SHIPPED PRINCIPLE `(#217)` DID, AND HARDER — BY DECISION, NOT EVIDENCE.
/// `(#210)` reserves the CORE for broad market exposure; `(#217)` overrode that for factor tilts,
/// which at least re-weight a universe the lane already owns. A sector fund does not: it is a
/// CONCENTRATED BET inside a table whose header says diversified. Chosen by the user with that
/// stated. See the `(#218)` receipt in tests/ci-settings.yaml, which records both halves so a future
/// round can revert on the principle alone without re-running anything.
const SECTOR: [&str; 10] = [
    "technolog", "information", "info tech", "financ", "semiconduct",
    "health", "energy", "industrial", "communicat", "biotech",
];

/// (#218) Tokens that may CO-OCCUR with a sector one without disqualifying the fund, but that never
/// NAME a family. Load-bearing, and measured: the live probe found `sector` firing on FOUR of the
/// nine admissible funds — "iShares S&P 500 Information Technology **Sector**" reads
/// `[technolog+information+sector]` — so `(#217)`'s third conjunct copied verbatim would have
/// refused the sleeve's single largest fund (IITU.L, EUR 15.6B). It is NOT in [`SECTOR`] because a
/// fund whose FIRST token is the bare word "sector" has not named a sector, and must still be
/// refused; keeping the two lists apart is what makes that fall out for free.
///
/// "select" is deliberately NOT here. The S&P *Select Sector* suite exists, but every one of its EU
/// listings (XLKS.L and friends) carries no `GEO` token at all and is refused a step earlier — so
/// admitting the word would buy nothing today, and the 34 geo-broad funds refused FIRST on "select"
/// are by construction not sector funds, or a sector token would have matched ahead of it.
const SECTOR_OK: [&str; 1] = ["sector"];

/// (#218) The eleventh sleeve, ranked LAST — least diversified of all, which is exactly where a
/// concentrated bet belongs in a ladder ordered broadest-first. Its own tier for the reason
/// `SIZE_TIER` and `FACTOR_TIER` each have one: a 0.25% sector fund dropped into the developed
/// sleeve loses the cheapest-TER sort to a 0.06% MSCI World tracker and would never print.
///
/// NO TER ceiling of its own — [`ter_cap_for`] is deliberately untouched, as it was for `(#217)`.
/// The live probe found nine funds clearing the BASE 0.25% cap across five families, so there is
/// nothing here to walk upward. The `(#218)` receipt pre-registers against adding one.
pub const SECTOR_TIER: u8 = HOLD_TIERS as u8 - 1;

/// (#218) Which geographies earn the sector sleeve. WIDER than [`FACTOR_GEO`] by one tier, and the
/// widening is the whole reason this constant is not simply reused: `geo_hit` answers `None` for
/// most sector funds in the pond (VanEck Semiconductor, iShares MSCI Global Semiconductors and
/// Invesco Technology S&P US Select Sector match no `GEO` token at all), so `[0, 1]` would have
/// admitted almost nothing. Tier 3 is the S&P 500 sector suite, which is the canonical instrument
/// set for this exposure and supplies four of the nine admissible funds — including the largest.
///
/// What it still REFUSES is the point: a fund must name a broad index to enter, so the invariant
/// every CORE row has held since the lane existed survives — each row is a geographically
/// identified fund, never a bare theme.
const SECTOR_GEO: [u8; 3] = [0, 1, 3];

/// (#218) The same four conjuncts as [`factor_sleeve_tier`], on the sector tokens, with the third
/// one widened by [`SECTOR_OK`] — see that constant for why a verbatim copy refuses IITU.L. The
/// conjunct still does the work it does in the other two sleeves: it is what keeps the ESG-crossed
/// funds out (the live pond holds "Fineco AM MSCI World Financials Sustainable Select" and
/// "…Information Technology Sustainable", both refused on `sustainab`) and the currency-hedged ones
/// (IUHE.AS, refused on `hedged`).
fn sector_sleeve_tier(n: &str, first: &str, on: bool) -> Option<u8> {
    if !on || !SECTOR.contains(&first) {
        return None;
    }
    if NARROW.iter().any(|t| !SECTOR.contains(t) && !SECTOR_OK.contains(t) && hit(n, t)) {
        return None;
    }
    SECTOR_GEO.contains(&geo_hit(n)?).then_some(SECTOR_TIER)
}

/// (#210) Which geographies earn the size sleeve. Tier 0 (all-world) and tier 1 (developed) ONLY,
/// because the sleeve is labelled `world small-cap`: a EUROPE small-cap fund sitting in it is two
/// bets stacked (region × size) wearing the name of one, and the lane already carries a Europe
/// sleeve for the region half.
///
/// This is not a hypothetical. `(#202)` shipped the sleeve geography-agnostic, and its live probe
/// admitted exactly ONE fund — XXSC.DE, Xtrackers MSCI *Europe* Small Cap — into a sleeve whose own
/// header says world. The two real world small-cap funds were absent for an unrelated reason: the
/// rounding bug `(#208)` fixed made `0.0035 * 100.0` read as `> 0.35` and evicted them from a cap
/// that IS 0.35. Restricting the sleeve was refused at the time only because it would have left the
/// sleeve provably empty; with `(#208)` in, that is no longer true and the restriction is free.
const SIZE_GEO: [u8; 2] = [0, 1];

/// (#202) Does an ALREADY-lowercased name earn the size sleeve, given the FIRST narrow token that
/// fired on it? All four conjuncts are load-bearing: the knob must be on (0.0 = off, and no fund
/// clears a 0.0% cap anyway), the blocking token must be a market-cap one, NO OTHER narrow token may
/// fire ("MSCI World Small Cap ESG" is still an ESG fund), and — `(#210)` — the geography must be a
/// WORLD one, not merely present (a bare "Global X Small Cap" is not a world index either, and still
/// fails for want of any geography at all).
fn size_sleeve_tier(n: &str, first: &str, cap: f64) -> Option<u8> {
    if cap <= 0.0 || !SIZE.contains(&first) {
        return None;
    }
    if NARROW.iter().any(|t| !SIZE.contains(t) && hit(n, t)) {
        return None;
    }
    SIZE_GEO.contains(&geo_hit(n)?).then_some(SIZE_TIER)
}

/// (#217) The same four conjuncts as [`size_sleeve_tier`], on the factor tokens: the knob must be
/// on, the blocking token must be a FACTOR one, NO OTHER narrow token may fire, and the geography
/// must be a world one. The third conjunct is the load-bearing one HERE in a way it is not for
/// size — the live pond holds 55 factor funds that are ALSO ESG/screened/sustainable against 13 that
/// are clean, so without it the sleeve would fill four-to-one with funds carrying a values overlay
/// nobody asked for. `on` is a plain bool, not a TER cap, because this sleeve has no ceiling of its
/// own; see [`FACTOR_TIER`].
fn factor_sleeve_tier(n: &str, first: &str, on: bool) -> Option<u8> {
    if !on || !FACTOR.contains(&first) {
        return None;
    }
    if NARROW.iter().any(|t| !FACTOR.contains(t) && hit(n, t)) {
        return None;
    }
    FACTOR_GEO.contains(&geo_hit(n)?).then_some(FACTOR_TIER)
}

/// (#202) Does the census line print this sleeve? An optional sleeve is HIDDEN while its knob is
/// off, so a lane with both off prints byte-identically to its eight-sleeve self — non-negotiable #1.
/// Every GEOGRAPHIC sleeve always prints, empty or not: round 118's rule, because an absent sleeve
/// and one the pond cannot fill are different facts and the reader needs to tell them apart.
///
/// (#217) Was `sleeves_shown`, a COUNT consumed by `.take(n)`. A count cannot express TWO
/// INDEPENDENTLY optional sleeves — size OFF with factor ON is a live combination and would have
/// printed the factor cell under the size cell's LABEL, silently. The predicate is the same rule
/// stated per-tier, and it is what makes the two knobs orthogonal instead of ordered.
pub fn sleeve_visible(tier: usize, size_cap: f64, factor_on: bool, sector_on: bool) -> bool {
    match tier as u8 {
        SIZE_TIER => size_cap > 0.0,
        FACTOR_TIER => factor_on,
        SECTOR_TIER => sector_on,
        _ => true,
    }
}

/// (#202) Which TER ceiling a sleeve is judged against. Takes both caps rather than reading them,
/// so the choice itself is a pure function the mutation gate can reach — the knob is a process-wide
/// `OnceLock` read from config, so a test cannot flip it and an unreachable branch here would be an
/// ungraded one.
/// (#207) How many rows a sleeve may print. Tier 0 (all-world) can carry its own ceiling because it
/// is the only sleeve where an extra row is a DIFFERENT product rather than another wrapper of what
/// is already shown: ACWI, ACWI IMI and FTSE All-World track different universes, and (#207) admitted
/// two funds into a sleeve that was already saturated at 3. `all_world` 0 = no override, inherit
/// `base` — which is today's behaviour exactly, on every tier.
///
/// Both caps are parameters rather than reads, for the reason [`ter_cap_for`] records: the knobs are
/// process-wide `OnceLock`s no test can flip, so a branch that only fires on an off-default would be
/// a branch the mutation gate cannot reach.
/// (#220) …and the SECTOR sleeve can carry its own for the opposite reason: it is the one sleeve
/// where the shared cap hides a whole FAMILY rather than a wrapper. (#197) set that cap at 3 on the
/// finding that every sleeve prints at least as many rows as it has index families; (#218) built the
/// first sleeve that holds five. `sector` 0 = no override, inherit `base`, today's behaviour exactly.
pub fn tier_cap(tier: usize, base: usize, all_world: usize, sector: usize) -> usize {
    if tier == 0 && all_world > 0 {
        all_world
    } else if tier == SECTOR_TIER as usize && sector > 0 {
        sector
    } else {
        base
    }
}

fn ter_cap_for(tier: u8, size_cap: f64, base_cap: f64) -> f64 {
    if tier == SIZE_TIER {
        size_cap
    } else {
        base_cap
    }
}

/// The geographic tier of an ALREADY-lowercased name, or None when no geography token matches or a
/// NARROW token disqualifies it. The single place the two public fns above agree.
fn geo_tier(n: &str) -> Option<u8> {
    geo_tier_at(
        n,
        crate::config::hold_size_sleeve_ter(),
        crate::config::hold_factor_sleeve(),
        crate::config::hold_sector_sleeve(),
    )
}

/// (#204) [`geo_tier`] with the size sleeve's cap PASSED rather than read, for the reason
/// [`ter_cap_for`] already records: the knob is a process-wide `OnceLock` no test can flip, so every
/// branch that only fires with the sleeve ON is a branch the mutation gate cannot reach. Threading
/// the cap is what makes the whole `(#202)` admission path gradeable, not just its leaf helpers.
fn geo_tier_at(n: &str, size_cap: f64, factor_on: bool, sector_on: bool) -> Option<u8> {
    if let Some(first) = narrow_hit(n) {
        // (#202) …unless the only narrow thing about it is market cap, and the sleeve is switched on.
        // (#217) …or the only narrow thing about it is a factor tilt, and THAT sleeve is on. The two
        // are mutually exclusive by construction (`SIZE` and `FACTOR` share no token, and each
        // demands no OTHER narrow token fires), so the `or_else` cannot mask a decision — it reads
        // as "whichever sleeve claims it", and neither claiming it is still the shipped refusal.
        // (#218) …or the only narrow thing about it is a sector, and the THIRD sleeve is on. All
        // three stay mutually exclusive: `SIZE`, `FACTOR` and `SECTOR` share no token, and each
        // demands no OTHER narrow token fires (bar `SECTOR_OK`, which no other sleeve names).
        return size_sleeve_tier(n, first, size_cap)
            .or_else(|| factor_sleeve_tier(n, first, factor_on))
            .or_else(|| sector_sleeve_tier(n, first, sector_on));
    }
    geo_hit(n)
}

/// Diversification tier of a broad-index fund, for ordering the buy-and-hold CORE broadest-first.
/// 0 = all-world (whole planet), 1 = developed, 2 = emerging, 3 = US, 4 = ex-US, 5 = Europe,
/// 6 = Japan, 7 = Asia-Pacific. Assumes `is_broad_index_name` already held; a name that does not
/// match lands in the last tier rather than panicking, and the CORE never prints it because the
/// filter ran first.
pub fn hold_breadth_tier(name: &str) -> u8 {
    // (#218) PINNED to `FACTOR_TIER` rather than spelled `HOLD_TIERS - 1`. That expression silently
    // followed the ladder every time a sleeve was added — it meant `SIZE_TIER` before (#217) and
    // would mean `SECTOR_TIER` now — so a geography-less name kept changing sleeves, and would have
    // landed in one that is INVISIBLE while its knob is off. The value here is unchanged (it is what
    // the expression already evaluated to), and it no longer moves on its own.
    // (#217) that fallback is `FACTOR_TIER`, not `SIZE_TIER`. It is STRICTLY safer than it was:
    // the trap `hold_miss_leg` documents is a geography-less name inheriting a sleeve's own TER
    // ceiling, and the factor sleeve deliberately has none — [`ter_cap_for`] hands `FACTOR_TIER` the
    // base cap. A name that reaches here still never prints, because leg 0 refused it first.
    geo_tier(&name.to_lowercase()).unwrap_or(FACTOR_TIER)
}

/// Is this quote a genuine buy-and-hold-20yr CORE holding — independent of the momentum SCORE, which
/// ranks recent runners and buries broad index funds at 0.0? Display-only `H` flag driver: broad
/// index + cheap + physical + accumulating + large + EU-domiciled, all read from facts already on the
/// row (no fetch, no scoring). The numeric legs (Acc / Full / AUM Some) only hold on BF-fund rows, so
/// stocks/crypto and factless venue funds return false naturally.
/// The "ucits" name token gates UCITS-ness (the wrapper), while the real country now rides
/// `Quote.domicile` (ISIN prefix) and orders the CORE shortlist. Deliberately NO domicile hard gate
/// here: watchlist-only runs have `domicile: None` and missing data must not kill the flag — the
/// same stance as the AUM gate.
pub fn hold_suitable(q: &Quote) -> bool {
    hold_miss_reason(q).is_none()
}

/// (round 49) The FIRST hold-core leg this quote fails, as a printable reason — None = passes all
/// (i.e. `hold_suitable`). Single source of truth: hold_suitable IS this function's is_none(), so
/// the H flag and the printed reason can never disagree. Leg order = cheapest check first, and the
/// (#199) The CORE admission legs, in the order [`hold_miss_leg`] walks them, indexed BY the number
/// it returns. Display labels for the funnel line — short on purpose, because all six print on one
/// row. A seventh tally slot past the end is "qualified"; see [`picks::hold_funnel`].
pub const HOLD_LEGS: [&str; 6] =
    ["not broad-index", "no UCITS", "TER", "replication", "not Acc", "AUM"];

/// TER cap note lives here: `hold_max_ter` ships 0.25 so FTSE All-World (VWCE/VWRL, 0.22%) — the
/// canonical one-fund hold — qualifies; below that is S&P/World territory (0.03–0.20%). The reason
/// string formats the cap from the knob, so it cannot quote a number the check did not use.
/// ter_shown/aum_shown: Yahoo fallback counts here (display-side flag), the score does NOT see it —
/// which also means this leg and the score's TER damp read two different fields; see (#102).
pub fn hold_miss_reason(q: &Quote) -> Option<String> {
    hold_miss_leg(q).map(|(_, msg)| msg)
}

/// (#199) The same six legs, plus the INDEX of the one that answered — the whole of
/// [`hold_miss_reason`], which is now a one-line wrapper over this so there is still exactly ONE
/// definition of the admission decision (non-negotiable #4) and not one caller had to change.
///
/// The index exists because nothing could bucket the verdict before: the message is formatted with
/// live values ("TER 0.30% > 0.25% cap"), so a tally keyed on it would key on the fund's numbers.
/// [`picks::hold_funnel`] reads the index and nothing else. Ten rounds of admission work guessed
/// which leg was binding because this number was thrown away on every one of the ~4300 funds that
/// computes it.
pub fn hold_miss_leg(q: &Quote) -> Option<(usize, String)> {
    // (#202) leg 0 KEEPS the tier it computed instead of throwing it away and re-deriving it at the
    // TER leg — `hold_breadth_tier` would answer `SIZE_TIER` for a name with no geography at all
    // (its `unwrap_or` fallback is the last tier), so re-asking there would hand the size cap to
    // anything that reached it by another route. One lookup, one definition (non-negotiable #4).
    hold_miss_leg_with(
        q,
        crate::config::hold_size_sleeve_ter(),
        crate::config::hold_factor_sleeve(),
        crate::config::hold_sector_sleeve(),
    )
}

/// (#204) The six legs with the size sleeve's TER cap PASSED rather than read — see [`geo_tier_at`].
/// `size_cap` 0.0 is the shipped lane; any positive value opens the sleeve, and only then can a name
/// carrying a `NARROW` token be refused by a leg OTHER than breadth, which is the case
/// `picks::near_miss_reason` had to be fixed for and no test could construct before.
pub fn hold_miss_leg_with(
    q: &Quote,
    size_cap: f64,
    factor_on: bool,
    sector_on: bool,
) -> Option<(usize, String)> {
    let lower = q.name.to_lowercase();
    let Some(tier) = geo_tier_at(&lower, size_cap, factor_on, sector_on) else {
        return Some((0, "not a broad-index name (sector/thematic/factor tilt)".into()));
    };
    hold_miss_leg_at(q, tier, size_cap)
}

/// (#203) Legs 1..5 for a fund ALREADY placed in `tier` — every leg of [`hold_miss_leg`] except the
/// breadth one, split out so [`hold_miss_but_breadth`] can ask the other five without re-spelling a
/// single one of them (non-negotiable #4). `tier` is read at exactly one site, [`ter_cap_for`].
fn hold_miss_leg_at(q: &Quote, tier: u8, size_cap: f64) -> Option<(usize, String)> {
    // (#102) the name token, OR — behind the knob — an EU domicile, which is the FACT the token is a
    // proxy for. `domicile: None` still falls back to the name, so missing data cannot newly pass.
    let eu_domiciled = crate::config::hold_ucits_or_domicile()
        && q.domicile.as_deref().is_some_and(|d| d.len() >= 2 && !NON_EU.contains(&&d[..2]));
    if !q.name.to_lowercase().contains("ucits") && !eu_domiciled {
        return Some((1, "no UCITS token in the name".into()));
    }
    // (#202) the size sleeve carries its own ceiling: world small-cap funds run ~0.35% against a
    // 0.25% cap fitted to all-world trackers, so one shared number cannot admit both.
    let cap = ter_cap_for(tier, size_cap, crate::config::hold_max_ter());
    match q.ter_shown() {
        None => return Some((2, "TER unknown".into())),
        Some(t) if t > cap => return Some((2, format!("TER {t:.2}% > {cap:.2}% cap"))),
        _ => {}
    }
    // (round 53) physical FAMILY, not literal "Full": this leg exists to exclude swap counterparty
    // risk over a decades hold, but requiring Full replication structurally excluded every large
    // all-world fund — VWRA (€43B), iShares ACWI (€29B), SPDR ACWI (€14B) all sample ("Optimised",
    // the norm for a 3000+ name index; BF verified live 2026-07) — keeping the CORE tier-0 slot
    // permanently empty. Optimised/Sample/Hybrid hold the stocks; Swap and unknown still fail.
    if !matches!(q.replication, Some("Full" | "Opt" | "Samp" | "Hybr")) {
        return Some((3, format!("replication {} (needs physical)", q.replication.unwrap_or("unknown"))));
    }
    if q.use_of_profits != Some("Acc") {
        return Some((4, format!("share class {} (needs Acc)", q.use_of_profits.unwrap_or("unknown"))));
    }
    if !q.aum_shown().is_some_and(|a| a >= 1e9) {
        return Some((5, match q.aum_shown() {
            Some(a) => format!("AUM €{:.1}B < €1B floor", a / 1e9),
            None => "AUM unknown".into(),
        }));
    }
    None
}

/// (#203) Would this fund clear every leg BUT breadth? The question `picks::geo_miss_census` asks of
/// the ~4100 names leg 0 refuses, to tell the two populations inside it apart: a stock or a bond fund
/// with no geography at all, versus a genuinely broad tracker whose index `GEO` simply cannot spell.
/// Only the second is supply, and only the second is a bug.
///
/// Tier 0 (all-world) is passed deliberately: it makes [`ter_cap_for`] apply the BASE cap, never the
/// size sleeve's, so a hypothetical admission is priced against the lane's own ceiling and the answer
/// does not move when `hold_size_sleeve_ter` does.
pub fn hold_miss_but_breadth(q: &Quote) -> Option<(usize, String)> {
    hold_miss_leg_at(q, 0, 0.0)
}

/// Pick the newest FULINS_C download link out of a FIRDS registry payload. Handles both registry
/// shapes: ESMA's Solr (`response.docs[].{file_name,download_link}`) and the FCA's Elasticsearch
/// (`hits.hits[]._source.{file_name,download_link}`). Newest = max file_name — the date is
/// embedded (`FULINS_C_YYYYMMDD_…`) so lexicographic order IS date order, no date parsing needed.
/// None on a missing/reshaped/empty payload — the caller then degrades to the cached list.
pub fn firds_latest_fulins_link(payload: &Value) -> Option<String> {
    let docs = payload
        .pointer("/response/docs")
        .or_else(|| payload.pointer("/hits/hits"))?
        .as_array()?;
    docs.iter()
        .filter_map(|d| {
            let d = d.get("_source").unwrap_or(d);
            let name = d.get("file_name")?.as_str()?;
            let link = d.get("download_link")?.as_str()?;
            name.starts_with("FULINS_C_")
                .then(|| (name.to_string(), link.to_string()))
        })
        .max()
        .map(|(_, link)| link)
}

/// Scan a FIRDS FULINS_C reference-data XML (ESMA or FCA weekly full dump) for exchange-traded
/// fund ISINs. Each `FinInstrmGnlAttrbts` record carries Id (ISIN) / FullNm / optional ShrtNm /
/// ClssfctnTp (CFI). Kept rows need ALL of: CFI class `CE*` (exchange-traded collective
/// investment vehicles — the class also covers Danish/Swiss listed mutual funds, same pollution
/// as the SIX FU segment, hence the same "etf"/"ucits" name funnel), an ETF/UCITS name token,
/// and a domicile prefix outside the non-EU blocklist (the dumps carry thousands of US/CA/Asia
/// funds traded on EU MTFs that an EU retail account can't buy — PRIIPs). Read as plain text
/// (ESMA is single-line, the FCA file pretty-printed — hence `\s*`); a real XML parser buys
/// nothing here. Sorted + deduped (an ISIN appears once per trading venue).
pub fn firds_etf_isins(xml: &str) -> Vec<String> {
    let re = regex::Regex::new(
        r"<FinInstrmGnlAttrbts>\s*<Id>([A-Z]{2}[0-9A-Z]{9}[0-9])</Id>\s*<FullNm>([^<]*)</FullNm>\s*(?:<ShrtNm>[^<]*</ShrtNm>\s*)?<ClssfctnTp>CE",
    )
    .unwrap();
    let mut out: Vec<String> = re
        .captures_iter(xml)
        .filter_map(|c| {
            let isin = c.get(1)?.as_str();
            let name = c.get(2)?.as_str().to_lowercase();
            ((name.contains("etf") || name.contains("ucits")) && !NON_EU.contains(&&isin[..2]))
                .then(|| isin.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Titles across yfinance/Yahoo schemas (flat `title`, nested `content.title`).
pub fn headline_titles(news_items: &[Value]) -> Vec<String> {
    let nonempty = |v: &Value, key: &str| -> Option<String> {
        v.get(key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let mut out = Vec::new();
    for n in news_items {
        let title = nonempty(n, "title")
            .or_else(|| n.get("content").and_then(|c| nonempty(c, "title")));
        if let Some(t) = title {
            out.push(t);
        }
    }
    out
}

/// Where the price comes from: the configured quote-page template with `{ticker}` filled.
pub fn source_url(template: &str, ticker: &str) -> String {
    template.replace("{ticker}", ticker)
}

/// Human-readable name from a Yahoo info/meta value; ticker if absent.
pub fn name_of(info: &Value, ticker: &str) -> String {
    let pick = |key: &str| info.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    // longName first: it carries the real fund name ("iShares Core MSCI World UCITS ETF") where the
    // shortName is a truncated registrant blob ("ISHARES III PLC ISHRS CORE MSCI"). shortName is the
    // fallback when meta omits longName (common for crypto/FX). Equities read the same either way.
    pick("longName")
        .or_else(|| pick("shortName"))
        .unwrap_or(ticker)
        .trim()
        .to_string()
}

/// CA base rate: 3-month Euribor × series multiplier + spread, capped, floored at 0.
/// The permanence premium is added on top per holding year, and is NOT capped.
///
/// None when the series has no published formula (Série A) — the caller prints unknown rather than
/// substituting a number, see `CaSeries::mult`.
pub fn ca_base_rate(euribor_3m: f64, mult: Option<f64>, spread: f64, cap: Option<f64>) -> Option<f64> {
    let raw = euribor_3m * mult? + spread;
    Some(f64::max(0.0, cap.map_or(raw, |c| f64::min(raw, c))))
}

/// (latest_year, latest_rate, avg_last_10y, avg_last_30y) from {year->rate}.
/// Averages use however many of the last N years are present. Nones if empty.
pub fn inflation_summary(
    series: &BTreeMap<i32, f64>,
) -> (Option<i32>, Option<f64>, Option<f64>, Option<f64>) {
    if series.is_empty() {
        return (None, None, None, None);
    }
    let years: Vec<i32> = series.keys().cloned().collect(); // BTreeMap keys are sorted
    let avg = |n: usize| -> f64 {
        let tail = &years[years.len().saturating_sub(n)..];
        tail.iter().map(|y| series[y]).sum::<f64>() / tail.len() as f64
    };
    let last = *years.last().expect("years non-empty: series.is_empty() guarded above");
    (Some(last), Some(series[&last]), Some(avg(10)), Some(avg(30)))
}

/// Cumulative price rise over the last `years` years, compounding each year's annual CPI
/// rate (the "true" erosion: +3%/yr for 10y ≈ +34%, not +30%). `None` when the series can't
/// reasonably cover the horizon — so we don't pass a much shorter span off as the full one (that's
/// what made the keyless 10y US window report an identical 10Y and 20Y). One year of slack is
/// allowed: a level→YoY series ALWAYS loses its earliest in-window year (no prior-year base to
/// divide by), so a true N-year horizon yields N−1 rates at best; n/a only kicks in at ≥2 short.
pub fn inflation_compounded(series: &BTreeMap<i32, f64>, years: usize) -> Option<f64> {
    if series.len() + 1 < years {
        return None;
    }
    let vals: Vec<f64> = series.values().cloned().collect(); // BTreeMap -> year-ascending
    let tail = &vals[vals.len().saturating_sub(years)..]; // saturating: the 1yr-slack case has years == len+1
    let factor = tail.iter().fold(1.0, |f, r| f * (1.0 + r / 100.0));
    Some((factor - 1.0) * 100.0)
}

/// Parse a Eurostat JSON-stat monthly annual-rate payload into {year -> annual %}: the sparse
/// `value` map is keyed by the `time` index POSITION, positions sorted so the last month of a
/// year wins (a partial current year resolves to its newest month YoY — same stance as the
/// BLS/PT parses). Junk shapes parse to empty. Works for both the COICOP-2018 successor
/// (prc_hicp_minr) and the terminated pre-2026 dataset it archives (prc_hicp_manr).
pub fn parse_eurostat_hicp(d: &Value) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let idx = d.pointer("/dimension/time/category/index").and_then(|v| v.as_object());
    let val = d.get("value");
    if let (Some(idx), Some(val)) = (idx, val) {
        let mut pairs: Vec<(&String, i64)> =
            idx.iter().map(|(k, v)| (k, v.as_i64().unwrap_or(0))).collect();
        pairs.sort_by_key(|(_, p)| *p);
        for (tm, pos) in pairs {
            if let (Some(Ok(year)), Some(rate)) = (
                tm.get(..4).map(|y| y.parse::<i32>()),
                val.get(pos.to_string()).and_then(|v| v.as_f64()),
            ) {
                out.insert(year, rate); // last month of a year wins
            }
        }
    }
    out
}

/// Merge the TERMINATED pre-2026 HICP archive under the live successor series: the successor
/// wins every overlapping year (recomputed under COICOP-2018), the archive contributes only
/// its earlier tail (1997-1999). An EMPTY live series returns empty — the archive extends a
/// LIVE feed, it must never mask a dead one (screen's degraded-feeds line keys off empty).
pub fn merge_infl_archive(
    old: BTreeMap<i32, f64>,
    new: BTreeMap<i32, f64>,
) -> BTreeMap<i32, f64> {
    if new.is_empty() {
        return new;
    }
    let mut merged = old;
    merged.extend(new);
    merged
}

/// Detect a frozen-but-non-empty inflation feed: every parser inserts the current year once
/// its first YoY print lands (by mid-Feb), so from March onward a healthy feed always carries
/// the current year. Returns the stale latest year, or None when healthy / empty (empty has
/// its own ERROR path) / before March (grace: last year's max is still legitimate).
pub fn infl_series_stale(
    series: &BTreeMap<i32, f64>,
    today: chrono::NaiveDate,
) -> Option<i32> {
    use chrono::Datelike;
    let max_year = *series.keys().next_back()?;
    (today.month() >= 3 && max_year < today.year()).then_some(max_year)
}

/// Parse the BLS public API (v1) CPI-U response into {year -> annual %}. The series is the
/// index LEVEL (e.g. CUUR0000SA0) by month, so convert to a rate: for each year, the rate is
/// (its latest month with a prior-year same-month) / (that prior-year value) − 1. A complete
/// year resolves to Dec-over-Dec; the current partial year to its newest month YoY — matching
/// how the EU/PT series use "last month of the year". Empty on a malformed payload.
/// Called once per POST year-window by `fetch_us_inflation`, results merged — so it only needs
/// each year's predecessor present within the window it's handed (windows overlap by 1 year).
pub fn parse_bls_cpi(d: &Value) -> BTreeMap<i32, f64> {
    let mut idx: BTreeMap<(i32, u32), f64> = BTreeMap::new(); // (year, month) -> index level
    let rows = d.pointer("/Results/series/0/data").and_then(|v| v.as_array());
    for r in rows.into_iter().flatten() {
        let year = r.get("year").and_then(|v| v.as_str()).and_then(|s| s.parse::<i32>().ok());
        let value = r.get("value").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
        let month = r
            .get("period")
            .and_then(|v| v.as_str())
            .and_then(|p| p.strip_prefix('M'))
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|m| (1..=12).contains(m)); // skip M13 (annual average)
        if let (Some(y), Some(m), Some(v)) = (year, month, value) {
            idx.insert((y, m), v);
        }
    }
    let mut out = BTreeMap::new();
    for (&(y, m), &v) in &idx {
        // BTreeMap is key-sorted -> larger m for a year is seen later -> latest month wins
        if let Some(&prev) = idx.get(&(y - 1, m)).filter(|&&p| p > 0.0) {
            out.insert(y, (v / prev - 1.0) * 100.0);
        }
    }
    out
}

/// Parse a BPstat-style JSON-stat payload into {year -> rate}, last month of each year
/// winning. The date index may be a JSON **array** of date strings (BPstat's actual
/// shape — already chronological and parallel to `value`) OR a `{date: position}` object;
/// `value` is an array. Handling only the object form was the Portugal-inflation bug.
pub fn parse_pt_series(d: &Value) -> BTreeMap<i32, f64> {
    let mut out = BTreeMap::new();
    let Some(idx) = d.pointer("/dimension/reference_date/category/index") else {
        return out;
    };
    let Some(values) = d.get("value").and_then(|v| v.as_array()) else {
        return out;
    };
    let dates: Vec<&str> = if let Some(arr) = idx.as_array() {
        arr.iter().filter_map(|v| v.as_str()).collect()
    } else if let Some(obj) = idx.as_object() {
        let mut pairs: Vec<(&str, i64)> =
            obj.iter().filter_map(|(k, v)| Some((k.as_str(), v.as_i64()?))).collect();
        pairs.sort_by_key(|(_, p)| *p); // chronological
        pairs.into_iter().map(|(k, _)| k).collect()
    } else {
        return out;
    };
    for (date, v) in dates.iter().zip(values) {
        if date.len() >= 4 {
            if let (Ok(year), Some(rate)) = (date[..4].parse::<i32>(), v.as_f64()) {
                out.insert(year, rate); // ascending -> later month of a year wins
            }
        }
    }
    out
}

/// Closes whose date >= last_date - days (ascending input). [] if empty.
pub fn slice_since(dates: &[NaiveDate], closes: &[f64], days: i64) -> Vec<f64> {
    if dates.is_empty() {
        return Vec::new();
    }
    let cutoff = *dates.last().expect("dates non-empty: dates.is_empty() guarded above") - Duration::days(days);
    dates.iter()
        .zip(closes)
        .filter(|(d, _)| **d >= cutoff)
        .map(|(_, c)| *c)
        .collect()
}

/// Last close with date <= target (ascending input). None if before history.
pub fn asof(dates: &[NaiveDate], closes: &[f64], target: NaiveDate) -> Option<f64> {
    let mut res = None;
    for (d, c) in dates.iter().zip(closes) {
        if *d <= target {
            res = Some(*c);
        } else {
            break;
        }
    }
    res
}

/// Average close within ±`half` days of `target` — a smoothed anchor so one outlier day (a spike
/// or gap landing exactly on the horizon date) doesn't skew a long-horizon % change. None if no
/// close falls in the window.
pub fn asof_avg(dates: &[NaiveDate], closes: &[f64], target: NaiveDate, half: i64) -> Option<f64> {
    let (lo, hi) = (target - Duration::days(half), target + Duration::days(half));
    let vals: Vec<f64> =
        dates.iter().zip(closes).filter(|(d, _)| **d >= lo && **d <= hi).map(|(_, c)| *c).collect();
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Extend a young listing's series with a configured older twin's history (`history_proxy`):
/// rebase the proxy so its close as-of the listing's first bar equals the listing's first close,
/// prepend only proxy bars strictly BEFORE that first bar, then the listing's own series
/// unchanged. None when the proxy doesn't overlap the listing's start or a rebase anchor is
/// non-positive — a splice with no common bar would fabricate a level jump.
pub fn splice_history(
    own_dates: &[NaiveDate],
    own_closes: &[f64],
    proxy_dates: &[NaiveDate],
    proxy_closes: &[f64],
) -> Option<(Vec<NaiveDate>, Vec<f64>)> {
    let (&own_first_date, &own_first_close) = (own_dates.first()?, own_closes.first()?);
    let proxy_at_start = asof(proxy_dates, proxy_closes, own_first_date)?;
    if own_first_close <= 0.0 || proxy_at_start <= 0.0 {
        return None;
    }
    let factor = own_first_close / proxy_at_start;
    let keep = proxy_dates.iter().take_while(|d| **d < own_first_date).count();
    if keep == 0 {
        return None; // proxy adds nothing older -> caller keeps the plain series
    }
    let mut dates = proxy_dates[..keep].to_vec();
    let mut closes: Vec<f64> = proxy_closes[..keep].iter().map(|c| c * factor).collect();
    dates.extend_from_slice(own_dates);
    closes.extend_from_slice(own_closes);
    Some((dates, closes))
}

/// Built-in ±days averaging window for a horizon, by its calendar length. Smoothing the anchor
/// hides a single outlier day; the further back the horizon, the wider the window. 1D = exact (a
/// 1-day move is a single point). Overridable per-label in settings.yaml `anchor_windows`.
pub fn default_anchor_half(days: i64) -> i64 {
    match days {
        d if d >= 1825 => 365, // 5Y/10Y/20Y: ±12 months
        d if d >= 182 => 90,   // 6M/1Y: ±3 months
        d if d >= 30 => 30,    // 1M: ±30 days
        d if d >= 7 => 7,      // 1W: ±7 days
        _ => 0,                // 1D: exact day
    }
}

/// A nominal % gain converted to a REAL (inflation-adjusted) one: +50% over a span that saw +10%
/// cumulative inflation is only ~+36% in purchasing power. `cum_infl_pct` = cumulative inflation %
/// over the same span. real = (1+nominal) / (1+infl) − 1.
pub fn real_pct(nominal_pct: f64, cum_infl_pct: f64) -> f64 {
    ((1.0 + nominal_pct / 100.0) / (1.0 + cum_infl_pct / 100.0) - 1.0) * 100.0
}

/// (past_price_eur_str, pct_change) or None for each HORIZON, in HORIZONS order. `windows` maps a
/// horizon label to a ±days averaging window, overriding `default_anchor_half`; missing = default.
/// `infl` = Some(year->YoY% series, e.g. EU HICP) to show inflation-adjusted returns on horizons
/// >=1Y (deflated by the real cumulative inflation over each horizon), or None for raw nominal %.
pub fn horizon_changes(dates: &[NaiveDate], closes: &[f64], rate: Option<f64>, windows: &BTreeMap<String, i64>, infl: Option<&BTreeMap<i32, f64>>) -> Vec<Option<(String, f64)>> {
    // (#18) two endpoints: the LONG legs (>=1Y — the CAGR/rank inputs) use the smoothed measurement
    // endpoint; the SHORT legs (1D/1W/1M, incl. the 1M-knife gate) keep the true last close — a
    // months-wide average would make "this month's move" meaningless as both a gate and a display.
    let cur_smooth = measure_endpoint(closes);
    let cur_raw = *closes.last().expect("closes non-empty: callers pass a fetched chart (quote_one guards !closes.is_empty(), fetch.rs)");
    let last = *dates.last().expect("dates non-empty: parallel to closes (same fetched chart)");
    let first = *dates.first().expect("dates non-empty: parallel to closes (same fetched chart)");
    HORIZONS
        .iter()
        .map(|(label, days)| {
            let cur = if *days >= 365 { cur_smooth } else { cur_raw };
            let target = last - Duration::days(*days);
            // (H-cov) the series must actually REACH this horizon. `asof` alone would return None here,
            // but `asof_avg` below averages [target ± half] and SUCCEEDS when the series merely STARTS
            // inside that window — handing back the name's earliest bars under a label they never
            // earned. That made every leg fabricable over a band `half` wide: a 4y fund printed a "5Y"
            // (±365 on 1825), a 7-month listing printed a "1Y" (±182 on 365). Not cosmetic — these legs
            // feed `long_leg`'s CAGR, the 5Y+ gate, `spy_premium` and `twin_groups`.
            // 31d of slack, not 0: history older than the ~10y daily window is MONTHLY (fetch.rs), so an
            // exact test would blank an old name's 20Y leg over bar PLACEMENT rather than missing data.
            if first > target + Duration::days(31) {
                return None;
            }
            let half = windows.get(*label).copied().unwrap_or_else(|| default_anchor_half(*days));
            let past = if half > 0 {
                asof_avg(dates, closes, target, half).or_else(|| asof(dates, closes, target))
            } else {
                asof(dates, closes, target)
            };
            match past {
                None => None,
                Some(0.0) => None,
                Some(p) => {
                    let eur = match rate {
                        Some(r) => format!("€{}", fmt_money2(p * r)),
                        None => format!("{}?", fmt_money2(p)),
                    };
                    let mut pct = (cur - p) / p * 100.0;
                    // inflation-adjust the longer horizons only (>=1Y); short ones are noise. Deflate
                    // by the ACTUAL cumulative inflation over that many years (compounded YoY series).
                    if let Some(series) = infl {
                        if *days >= 365 {
                            if let Some(cum) = inflation_compounded(series, (*days / 365) as usize) {
                                pct = real_pct(pct, cum);
                            }
                        }
                    }
                    Some((eur, pct))
                }
            }
        })
        .collect()
}

/// % change vs `bars` hourly bars ago (index-based, NOT wall-clock). Counting trading bars
/// ignores overnight/weekend gaps, so it always resolves when enough bars exist — for a stock
/// "N bars ago" is the last N *trading* hours (may span a close); for 24/7 crypto it's real
/// wall-clock hours. None if fewer than `bars`+1 closes. A ratio, so FX-agnostic.
pub fn intraday_pct(closes: &[f64], bars: usize) -> Option<f64> {
    let len = closes.len();
    if len <= bars {
        return None;
    }
    let cur = *closes.last()?;
    let past = closes[len - 1 - bars];
    if past == 0.0 {
        return None;
    }
    Some((cur - past) / past * 100.0)
}

/// Pearson correlation of two equal-length series. None if <2 points, length mismatch, or either
/// series has zero variance (a flat series has no correlation to anything).
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return None;
    }
    let nf = n as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / nf, ys.iter().sum::<f64>() / nf);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        let (dx, dy) = (x - mx, y - my);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return None;
    }
    Some(sxy / (sxx * syy).sqrt())
}

/// Spearman rank correlation = Pearson on the fractional ranks. Robust to the wild magnitude outliers
/// a few crypto names inject (it measures monotone agreement, not size). None if <2 points.
pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    pearson(&ranks(xs), &ranks(ys))
}

/// Fractional ranks (1-based; tied values share the average of their ranks), in original order.
/// note: O(n log n) sort + linear tie-merge; fine for the backtest's handful-to-hundreds of names.
///
/// (#144) `pub(crate)` so `picks::growth_scores_ranked` ranks a term the same way `spearman` ranks one.
/// Tie-averaging is the half that matters there: a term where half the pool shares one value (every
/// `growth_fund_extra` fill, every zeroed weight) must map that whole group to ONE position, not
/// spread it across the range in input order.
pub(crate) fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut r = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1; // group of ties [i..=j]
        }
        let avg = (i + j) as f64 / 2.0 + 1.0; // mean of the 1-based ranks (i+1)..=(j+1)
        for k in i..=j {
            r[idx[k]] = avg;
        }
        i = j + 1;
    }
    r
}

/// One filed fundamentals statement, point-in-time. `filed` = FMP `filingDate` (when the numbers
/// became PUBLIC), deliberately NOT the period-end `date` — joining on period-end would let the
/// backtest read a quarter before it was reported (look-ahead bias). Every ratio is Option: a factor
/// the free tier can't source (roic/debt = premium-gated) stays None and scores NEUTRAL, never zero.
/// revenue/margins/eps come from the free `stable/income-statement`; the rest await a paid tier.
#[derive(Clone, Debug, Default)]
pub struct FundRow {
    pub filed: NaiveDate,
    pub period_end: NaiveDate,         // FMP period-end `date` — DISPLAY-ONLY (groups quarters by fiscal year in `report`). The as-of join (`fund_as_of`) keys on `filed`, never this, so it can't leak look-ahead into the backtest
    pub revenue: Option<f64>,
    pub gross_margin: Option<f64>,    // % = grossProfit/revenue
    pub op_margin: Option<f64>,       // % = operatingIncome/revenue
    pub net_margin: Option<f64>,      // % = netIncome/revenue
    pub eps: Option<f64>,
    pub shares: Option<f64>,          // diluted weighted-avg shares outstanding — DISPLAY-ONLY (buyback column); None on the free tier / when the source omits it
    // The PRIOR fiscal year's eps/shares AS THE SAME FILING STATED THEM — the year-over-year
    // denominators. NOT the previous row's `eps`/`shares`: those come from their own original filing,
    // on whatever share basis was current THEN, so dividing across them straddles every split and
    // restatement (TPL's 3-for-1s turned +6.0% real EPS growth into -64.7%). A 10-K prints its
    // comparatives on today's basis, so these two compare like with like by construction, and the gap
    // between `shares` and `prior_shares` is real issuance rather than a unit change. None when the
    // filing carried no comparative (first year of coverage) or on the FMP path -> callers fall back.
    pub prior_eps: Option<f64>,
    pub prior_shares: Option<f64>,
    pub roe: Option<f64>,             // % — PREMIUM (key-metrics/ratios), None on free tier
    pub roa: Option<f64>,             // % = netIncome/totalAssets — SEC-computed. The ROE FALLBACK: assets can't go negative, so a buyback-shrunk filer (HCA, HLT) still has a meaningful quality level. None on the FMP path. Resolved with roe by `quality_return`
    // (P3) TOTAL ASSETS, the denominator `roa` above already divides by and then throws away. Carried so
    // the asset BASE can be tracked over time, which is a different question from the return on it: a
    // filer can hold a flat ROA while doubling its balance sheet, and that expansion is the one thing
    // the growth lane rewards without ever pricing. Positive by construction -> a non-positive parse
    // artifact is dropped at the source rather than becoming a CAGR denominator.
    pub assets: Option<f64>,
    pub roic: Option<f64>,            // % — PREMIUM
    pub net_debt_ebitda: Option<f64>, // ratio, lower=safer — PREMIUM
    pub fcf_ps: Option<f64>,          // free cash flow / share — PREMIUM
    // (round 107) SURVIVAL levels, SEC-computed per 10-K (None on FMP free tier). All oriented
    // high = safer so factor ranks and reject-bottom gates read one direction.
    pub fcf_margin: Option<f64>,     // % = (op cash flow − capex) / revenue; negative = burning cash
    pub interest_cover: Option<f64>, // × = operating income / interest expense; low = one bad year from distress. None when no interest expense filed (debt-free reads NEUTRAL, not great)
    pub net_cash_rev: Option<f64>,   // % = (cash − debt) / revenue; negative = levered. Revenue-scaled (not EBITDA) so loss-makers stay defined instead of None-ing out of the gate
    // (EV/EBITDA probe) raw as-of LEVELS for the enterprise-value valuation factor. SEC-computed, None on
    // the FMP free tier. ebitda = operating income + D&A (BOTH required — a partial is garbage, so None if
    // either is missing). net_debt = total debt − cash (cash the anchor, missing debt reads 0 like
    // net_cash_rev). Combined with the as-of PRICE into ebitda_yield in the backtest loop (price-dependent,
    // exactly like earnings_yield — so no live currency skew).
    pub ebitda: Option<f64>,
    pub net_debt: Option<f64>,
    // (FX) the currency these MONEY lines are REPORTED in, straight off the XBRL unit key ("EUR" for a
    // 20-F filer like ASML, "USD" for a 10-K filer). None = unknown/FMP-sourced -> callers must assume
    // nothing. The margins/ROE above are ratios and cancel it, but anything joined to a PRICE
    // (earnings_yield / peg_yield / ebitda_yield / P/E) is only meaningful once BOTH sides sit in this
    // currency — a EUR EPS over a USD ADR close is the Item 16 trap, off by the whole FX rate.
    pub currency: Option<String>,
}

/// As-of (point-in-time) join: the latest statement that was already FILED on or before `cutoff`.
/// THE look-ahead guard for the fundamentals backtest — at a given cutoff a strategy could only have
/// seen filings public by then. `None` if nothing was filed yet. O(n), order-independent (FMP returns
/// newest-first; don't assume it). Compose it twice (cutoff and cutoff−Ny) to get as-of growth/trend.
pub fn fund_as_of(rows: &[FundRow], cutoff: NaiveDate) -> Option<&FundRow> {
    rows.iter().filter(|r| r.filed <= cutoff).max_by_key(|r| r.filed)
}

/// As-of fundamental factors derived from filed statements at a cutoff — the backtest's fundamental
/// lane scores each STANDALONE against the forward return. All Option: None when the as-of history is
/// too short to span the lookback, or the source field is premium-gated (roic/debt never populate on
/// the free tier). Growth in %/yr, margins in %, trend/accel in points.
#[derive(Clone, Debug, Default)]
pub struct FundFactors {
    pub rev_cagr: Option<f64>,     // revenue CAGR over the lookback (proven top-line compounding)
    pub rev_accel: Option<f64>,    // last-1y revenue growth minus that long CAGR (top-line accelerating)
    pub gross_margin: Option<f64>, // current gross margin level (pricing power / moat)
    pub op_margin: Option<f64>,    // current operating margin level (operating efficiency)
    pub margin_trend: Option<f64>, // op-margin now minus ~1y ago (margin expanding = strengthening)
    pub eps_growth: Option<f64>,   // EPS CAGR over the lookback (bottom-line compounding; both ends must be +)
    // The three columns `report`/`screen` actually PRINT (REV-YoY, EPS-YoY, NET%), exposed as factors
    // so the sweep can price them. Until now none of the three was probeable: `rev_yoy` was computed
    // and thrown away inside `rev_accel`, `eps_yoy` did not exist (only the multi-year `eps_growth`),
    // and `net_margin` was never carried at all — `op_margin` was the only margin LEVEL ever measured.
    // All three are 1-row or 2-row reads, so they populate far earlier in the sample than the
    // `yrs`-lookback factors above, which need a filing `yrs` years before the cutoff.
    pub rev_yoy: Option<f64>,      // last-1y revenue growth, % (the REV-YoY column) — `rev_accel`'s fast leg
    pub eps_yoy: Option<f64>,      // last-1y EPS growth, % (the EPS-YoY column), off the row's OWN same-filing comparative -> split-proof
    pub net_margin: Option<f64>,   // current net margin level, % (the NET% column). Distinct from op_margin: below-the-line items (tax, interest, one-offs) live only here
    pub roe: Option<f64>,          // as-of return-on-equity level, % (quality of capital). SEC feed computes it per row (NetIncome ÷ StockholdersEquity); FMP free tier leaves it None. RAW — sweep-only; the SCORE reads `quality` below
    pub quality: Option<f64>,      // as-of return-on-capital level, % — `quality_return(roe, roa, net_margin)`: ROE when equity is a credible denominator (positive, and over 1/20th of assets), else ROA. THE field the live screen and the backtest both score, so neither can drift from the other's definition
    // (#43) return on INVESTED capital, pre-tax — `roic_return`, derived from levels already cached. The
    // leverage-free cousin of `quality` above: ROE divides by equity alone, this divides by equity + net
    // debt, which is the return a 20-year hold actually compounds at. UNSCORED so far — exposed only so
    // `growth_fund_extra` can price it; `quality` keeps its measured weight untouched until it earns one.
    // NOT `FundRow::roic` (premium, never populated) — see the fn's doc for why they must not be mixed.
    pub roic: Option<f64>,
    pub insider_net_buys_90d: Option<f64>, // (Item 4) open-market buys minus sales (Form 4 P−S) in the 90d before the cutoff; populated only under `backtest … insider`, derived in the backtest loop (not here — needs SEC, not FMP)
    pub eps_ttm: Option<f64>,      // (Item 19) the as-of EPS level (not a growth) — the numerator for earnings_yield
    pub earnings_yield: Option<f64>, // (Item 19) EPS ÷ as-of price, % (valuation level, high = cheap). Set in the backtest loop from the native as-of close; the live path fills it only when `growth_fund_factor: earnings_yield` selects it (fetch.rs gates the fill to dodge the currency skew — see `earnings_yield` fn)
    // (EV/EBITDA probe) capital-structure-neutral value cousin of earnings_yield. The three as-of LEVELS
    // are price-free (set here from the latest filed row); ebitda_yield itself is EBITDA ÷ enterprise value
    // (EV = shares·price + net_debt), so it needs the as-of price -> filled in the backtest loop like
    // earnings_yield, left None by the live path. Distinct from earnings_yield because EV folds in leverage
    // (the one axis EPS/price misses).
    pub ebitda_ttm: Option<f64>,     // (EV/EBITDA) as-of EBITDA level = operating income + D&A
    pub shares_ttm: Option<f64>,     // (EV/EBITDA) as-of diluted share count — the market-cap leg of EV
    pub net_debt: Option<f64>,       // (EV/EBITDA) as-of net debt (total debt − cash) — the leverage leg of EV
    pub ebitda_yield: Option<f64>,   // (EV/EBITDA) EBITDA ÷ EV, % (high = cheap). PROBE-ONLY, None live (price skew, same as earnings_yield)
    pub peg_yield: Option<f64>,      // (PEG) 1/PEG = earnings_yield · as-of CAGR (high = cheap-for-its-growth). THE LIVE RANKING TILT since 2026-07-25 (`growth_fund_factor`); filled from the NATIVE close in both the backtest loop and the live enrich, so train and serve share one definition
    pub buyback_yield: Option<f64>, // as-of net share-count change over the last year, sign-flipped (+ = shrinking share count = buying back). Fully as-of from the FundRows (no price needed), unlike earnings_yield — so it populates in both the backtest AND the live enrich
    // (round 107) as-of SURVIVAL levels straight off the latest filed row (like op_margin/roe) —
    // price-free, so they populate in both the backtest and the live enrich. High = safer.
    pub fcf_margin: Option<f64>,     // % (op cash flow − capex) / revenue
    pub interest_cover: Option<f64>, // × operating income / interest expense
    pub net_cash_rev: Option<f64>,   // % (cash − debt) / revenue
    // (round 109) the cyclical detector: NEGATED sample stddev of net_margin across the as-of
    // lookback rows (higher = stabler). Margin LEVEL and 1y TREND are swept elsewhere; the
    // dispersion is what a peak-cycle name (fertilizer, refiner) hides behind a good level.
    pub margin_stability: Option<f64>,
    // (P2) ACCRUAL GAP: how much of the as-of EPS is NOT backed by cash, NEGATED so high = safer,
    // the same orientation the round-107 levels above carry. Accounting earnings and cash earnings
    // diverge for ordinary reasons (working capital, a build year) and for one bad one — an income
    // statement being managed — and the lane has no other term that can tell a cash profit from a
    // booked one. DERIVED from levels already on the row, so it costs no fetch. None whenever any leg
    // is missing, which on the FMP free tier is always (fcf_margin never populates there).
    pub accrual_gap: Option<f64>,
    // (P3) ASSET GROWTH over the same `yrs` lookback `rev_cagr` uses, NEGATED so high = safer, like
    // every other survival-shaped factor here. The asset-growth anomaly is that the fastest balance-sheet
    // expanders underperform — acquisitions, capex booms and equity raises all land here — and this lane
    // currently has no term that can see it: `rev_cagr` and `rev_accel` reward the growth, and nothing
    // asks what it cost to buy. Correlated with `rev_accel` by construction, so the two must be read
    // together rather than summed.
    pub asset_growth: Option<f64>,
    // (V) this FILER never states an EPS anywhere in its series — not "not yet", not "loss-making",
    // not "no coverage at this cutoff". Read from the WHOLE `rows` slice, deliberately NOT through
    // `fund_as_of`: both callers that matter hand `fund_factors` the same full series (the backtest
    // loop and the live enrich), so a filer-level fact is identical on both sides at every cutoff. An
    // as-of version would gate a name historically and pass it live — the exact train-serve skew the
    // one-source rule exists to prevent. After the XBRL-instance fallback this is ARES alone: it tags
    // no per-share and no weighted-average element in the filing itself, so no source has it.
    pub eps_never_reported: bool,
}

/// (Item 4) One open-market insider transaction parsed from an SEC Form 4: the transaction date (the
/// look-ahead guard) and its direction. Only `P` (purchase) and `S` (sale) are kept — option grants,
/// gifts, tax-withholding (codes A/G/F/M…) are noise for a "conviction buying" signal.
#[derive(Clone, Copy, Debug)]
pub struct InsiderTx {
    pub date: NaiveDate,
    pub buy: bool, // true = open-market purchase (P), false = open-market sale (S)
}

/// (Item 4) Net open-market insider conviction in the `window_days` BEFORE `cutoff`: (#buys − #sales),
/// counting each `InsiderTx` ±1. The transaction date is the look-ahead guard — a filing dated on/after
/// the cutoff can't leak in. None when no transaction falls in the window (no coverage -> the factor stays
/// neutral, never a fabricated 0). Pure -> unit-tested without touching SEC.
pub fn insider_net_buys(txns: &[InsiderTx], cutoff: NaiveDate, window_days: i64) -> Option<f64> {
    let start = cutoff - Duration::days(window_days);
    let net: i64 = txns
        .iter()
        .filter(|t| t.date >= start && t.date < cutoff)
        .map(|t| if t.buy { 1 } else { -1 })
        .sum();
    let any = txns.iter().any(|t| t.date >= start && t.date < cutoff);
    any.then_some(net as f64)
}

/// (Item 3) A per-name blend of the available as-of factors for the `"composite"` `growth_fund_factor`.
/// ponytail: a plain mean of the factors present — they're all growth-%/points of similar magnitude, so
/// averaging is a defensible first cut. CEILING: a true cross-sectional rank-normalisation (0..1 across
/// the cutoff's universe) would be scale-clean, but `select_fund_factor` sees ONE name with no peer
/// context; lift it to a universe rank in the backtest layer IF the sweep shows the composite earns its
/// place. None until ≥2 factors are present (a 1-factor "composite" IS that factor — route it directly).
fn composite_factor(f: &FundFactors) -> Option<f64> {
    let vals: Vec<f64> =
        [f.rev_cagr, f.rev_accel, f.gross_margin, f.op_margin, f.margin_trend, f.eps_growth].into_iter().flatten().collect();
    (vals.len() >= 2).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Derive the as-of fundamental factors at `cutoff` from filed statements, looking back ~`yrs`. Every
/// read goes through `fund_as_of` so NOTHING after the cutoff's filing leaks in (look-ahead guard). A
/// growth needs a positive base to be meaningful, so a non-positive denominator -> None, never a
/// garbage ratio. note: EPS CAGR only when both endpoints are positive (a sign flip isn't a CAGR).
pub fn fund_factors(rows: &[FundRow], cutoff: NaiveDate, yrs: i64) -> FundFactors {
    let now = fund_as_of(rows, cutoff);
    let long_ago = fund_as_of(rows, cutoff - Duration::days(yrs * 365));
    let yr_ago = fund_as_of(rows, cutoff - Duration::days(365));
    let grow = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) if b > 0.0 => Some((a / b - 1.0) * 100.0),
        _ => None,
    };
    let rev_cagr = grow(now.and_then(|r| r.revenue), long_ago.and_then(|r| r.revenue)).map(|c| cagr(c, yrs as f64));
    let rev_1y = grow(now.and_then(|r| r.revenue), yr_ago.and_then(|r| r.revenue));
    // (P3) same two endpoints, same `grow` positivity guard and the same `cagr` annualiser as rev_cagr
    // above — one definition of a growth rate, per the house rule — then NEGATED so a slow-growing
    // asset base ranks high alongside the other safety factors.
    let asset_growth =
        grow(now.and_then(|r| r.assets), long_ago.and_then(|r| r.assets)).map(|c| -cagr(c, yrs as f64));
    let rev_accel = match (rev_1y, rev_cagr) {
        (Some(a), Some(c)) => Some(a - c),
        _ => None,
    };
    let margin_trend = match (now.and_then(|r| r.op_margin), yr_ago.and_then(|r| r.op_margin)) {
        (Some(a), Some(b)) => Some(a - b),
        _ => None,
    };
    // `yrs`-year EPS CAGR, CHAINED rather than taken end-to-end. The two endpoints come from two
    // filings written on two different share bases, so dividing them straddles every split in between:
    // TPL reads -21.0%/yr that way against +22.6%/yr real. Compounding each year's own same-filing
    // ratio never crosses a basis at all — each step is two numbers off one income statement.
    // Look-ahead-free: a step is fully determined by its row's own filing, and rows are already
    // filtered to `filed <= cutoff`, so nothing here is knowable earlier than it was filed.
    let eps_growth = {
        let mut yearly: Vec<&FundRow> = rows
            .iter()
            .filter(|r| r.filed <= cutoff && r.period_end > cutoff - Duration::days((yrs + 1) * 365))
            .collect();
        yearly.sort_by_key(|r| r.period_end);
        let steps: Vec<f64> = yearly
            .iter()
            .rev()
            .take(yrs as usize)
            .filter_map(|r| match (r.eps, r.prior_eps) {
                // both legs positive or the ratio is not a growth rate: a swing through a loss makes
                // -0.5 -> +1.0 read as "-300% growth", which would poison the whole product.
                (Some(e), Some(p)) if e > 0.0 && p > 0.0 => Some(e / p),
                _ => None,
            })
            .collect();
        if steps.len() == yrs as usize {
            Some(cagr((steps.iter().product::<f64>() - 1.0) * 100.0, yrs as f64))
        } else {
            // Chain broken (a loss year, a missing comparative, thin history) -> the old endpoint read,
            // still behind the per-year share guard. Endpoint-and-guard is the fallback, never the
            // primary: it can only ever blank a splitter, never measure one.
            let split_in_window = {
                let mut sh: Vec<(NaiveDate, f64)> = yearly
                    .iter()
                    .filter(|r| r.period_end >= cutoff - Duration::days(yrs * 365))
                    .filter_map(|r| r.shares.map(|s| (r.period_end, s)))
                    .collect();
                sh.sort_by_key(|(e, _)| *e);
                sh.windows(2).any(|w| w[0].1 > 0.0 && ((w[1].1 / w[0].1 - 1.0) * 100.0).abs() > 40.0)
            };
            match (now.and_then(|r| r.eps), long_ago.and_then(|r| r.eps)) {
                (Some(a), Some(b)) if a > 0.0 && b > 0.0 && !split_in_window => {
                    Some(cagr((a / b - 1.0) * 100.0, yrs as f64))
                }
                _ => None,
            }
        }
    };
    // as-of buyback yield: 1y share-count change, sign-flipped (shares shrank -> positive = buying back).
    // Reads the as-of row's OWN same-filing comparative, so a split no longer reads as a 200% issuance
    // and a genuine issuance is no longer thrown away with it. Needs only the FundRows -> fully as-of
    // (no price), and look-ahead-free: both numbers were printed in the row's own filing.
    let buyback_yield = match now.and_then(|r| r.prior_shares) {
        Some(p) => yoy_pct(now.and_then(|r| r.shares), Some(p)).map(|d| -d),
        // no comparative in that filing -> the old cross-filing read, still guarded (see eps_yoy_split_safe)
        None => match (now.and_then(|r| r.shares), yr_ago.and_then(|r| r.shares)) {
            (Some(a), Some(b)) if b > 0.0 => {
                let d = (a / b - 1.0) * 100.0;
                (d.abs() <= 40.0).then_some(-d)
            }
            _ => None,
        },
    };
    // (round 109) margin stability: negated sample stddev of net_margin over the last `yrs`+1 as-of
    // rows, oldest-first so the take() grabs the most RECENT filings. ≥3 values required (2 points =
    // a line, not a dispersion). CAVEAT: FMP rows are QUARTERLY — seasonality inflates the std; the
    // validated lane (fund_source sec) files one annual row per year, which is what the sweep grades.
    let margin_stability = {
        let mut ms: Vec<(NaiveDate, f64)> =
            rows.iter().filter(|r| r.filed <= cutoff).filter_map(|r| r.net_margin.map(|m| (r.period_end, m))).collect();
        ms.sort_by_key(|(e, _)| *e);
        let vals: Vec<f64> = ms.iter().rev().take(yrs as usize + 1).map(|(_, m)| *m).collect();
        (vals.len() >= 3).then(|| {
            let n = vals.len() as f64;
            let mean = vals.iter().sum::<f64>() / n;
            -(vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
        })
    };
    // (P2) accrual gap = −(eps − fcf_ps) / max(|eps|, floor). A filer earning 3.00 a share on 3.00 of
    // per-share free cash flow scores 0; one earning 3.00 on 0.50 of cash scores −0.83.
    //
    // fcf_ps is DERIVED here (fcf_margin · revenue ÷ shares) and is deliberately NOT `FundRow::fcf_ps`
    // — that field is premium-gated and never populates on either feed we run, so reading it would
    // leave this None on every name in the universe. Same trap `roic` documents against `FundRow::roic`.
    //
    // `shares` is the DILUTED WEIGHTED AVERAGE, not the period-end count, so the fcf_ps in here is not
    // a quotable per-share FCF and should never be printed as one. It is consistent across the series
    // and across filers, which is all a cross-sectional rank needs — and crucially `eps` is struck on
    // that same weighted average, so both legs of the subtraction sit on one share basis.
    let accrual_gap = now.and_then(|r| {
        // the floor stops a near-zero EPS from turning one cent of accrual into a ratio in the
        // hundreds. It is denominated in the filer's REPORTING currency, so it only bites where the
        // per-share unit is small — which is the intended shape: the guard is against eps ~= 0.
        const EPS_FLOOR: f64 = 0.5;
        let (rev, margin, eps, shares) = (r.revenue?, r.fcf_margin?, r.eps?, r.shares?);
        (shares > 0.0).then(|| -((eps - margin / 100.0 * rev / shares) / eps.abs().max(EPS_FLOOR)))
    });
    FundFactors {
        rev_cagr,
        rev_accel,
        gross_margin: now.and_then(|r| r.gross_margin),
        op_margin: now.and_then(|r| r.op_margin),
        margin_trend,
        eps_growth,
        // (REV-YoY) the fast leg `rev_accel` already subtracts — surfaced instead of discarded, so the
        // 1y rate can be priced apart from the accel it feeds. Fills wherever a year-ago row exists,
        // which is far more of the sample than `rev_cagr`'s `yrs`-year reach.
        rev_yoy: rev_1y,
        // (EPS-YoY) the row's OWN prior-year comparative, so the ratio never straddles a split — the
        // same join `income_snapshot` and the report table use. Falls back to the cross-filing read
        // (guarded) only for a filing that carried no comparative.
        eps_yoy: match now.and_then(|r| r.prior_eps) {
            Some(p) => yoy_pct(now.and_then(|r| r.eps), Some(p)),
            None => eps_yoy_split_safe(
                now.and_then(|r| r.eps),
                yr_ago.and_then(|r| r.eps),
                now.and_then(|r| r.shares),
                yr_ago.and_then(|r| r.shares),
            ),
        },
        // (NET%) as-of level through the same one-line join as op_margin above
        net_margin: now.and_then(|r| r.net_margin),
        roe: now.and_then(|r| r.roe), // as-of level through fund_as_of, same look-ahead guard as the margins
        // the SCORED quality level, resolved from the same as-of row. `roe` above stays raw so the
        // factor sweep can still price it standalone; everything that feeds the ranking reads this.
        quality: now.and_then(|r| quality_return(r.roe, r.roa, r.net_margin)),
        // (#43) same as-of row, same look-ahead guard — every input is a LEVEL already on it, so this
        // costs no fetch. Any missing leg -> None (neutral), never a fabricated 0.
        roic: now.and_then(|r| roic_return(r.revenue, r.op_margin, r.net_margin, r.roe, r.roa, r.net_debt)),
        insider_net_buys_90d: None, // (Item 4) SEC-sourced, set in the backtest loop, not from FMP rows
        eps_ttm: now.and_then(|r| r.eps), // (Item 19) as-of EPS level; earnings_yield needs price, set by caller
        earnings_yield: None,             // (Item 19) needs the as-of price -> filled in the backtest loop, not here
        // (EV/EBITDA) as-of levels through the same fund_as_of guard; ebitda_yield needs price -> caller fills
        ebitda_ttm: now.and_then(|r| r.ebitda),
        shares_ttm: now.and_then(|r| r.shares),
        net_debt: now.and_then(|r| r.net_debt),
        ebitda_yield: None,
        // (PEG) needs the as-of price AND CAGR, which this fn has no access to -> filled by the CALLER.
        // THREE call sites must each remember, and a miss is SILENT (select_fund_factor just reads None
        // and the tilt vanishes with no error): the backtest loop (commands/backtest.rs), the live enrich
        // (fetch.rs::enrich_fund_factor), and report.rs's verdict mirror. The last two were each missing
        // it once. Adding a price-dependent factor here means auditing all three.
        peg_yield: None,
        buyback_yield,
        // (round 107) survival levels: same as-of join as the margins, no derivation needed
        fcf_margin: now.and_then(|r| r.fcf_margin),
        interest_cover: now.and_then(|r| r.interest_cover),
        net_cash_rev: now.and_then(|r| r.net_cash_rev),
        margin_stability,
        accrual_gap,
        asset_growth,
        // (V) `rows`, not `now` — see the field's doc. An EMPTY series is not "never reports", it is no
        // coverage at all (every ETF, every coin, every filer with no `fund`), so `!is_empty()` guards it.
        eps_never_reported: !rows.is_empty() && rows.iter().all(|r| r.eps.is_none()),
    }
}

/// (FX) The price to divide a filer's PER-SHARE figures by, expressed in the filer's REPORTING currency.
///
/// Returns `close_native` completely UNCHANGED when the listing already trades in that currency — the
/// overwhelming case (every US 10-K filer) — so the validated earnings_yield / peg_yield path is
/// bit-for-bit identical and no FX rate is fetched or applied at all. That exactness is the point: any
/// drift here would move `scoring_regression_pin` and silently re-rank names the backtest already graded.
///
/// Only a genuine mismatch converts, and it routes through EUR because that is the one rate a Quote
/// already carries: `price_eur / eur_per_reporting`. Going via `price_eur` also absorbs pence-quoted LSE
/// listings for free — `price_eur` already has the GBp÷100 scale applied while `close_native` does not.
///
/// None when the mismatch can't be resolved, so a price-joined factor None-outs (neutral) instead of
/// dividing EUR earnings by a USD price — a ~1.17x error that would look entirely plausible in the
/// output. That is the Item 16 trap, and silence is the only safe failure here.
///
/// Rates are "EUR per 1 unit", the convention `fetch::eur_rate` already returns (and which already folds
/// the GBp÷100 pence scale in), so EUR is only ever an intermediate hop — never a currency either side
/// has to be in. Pure so the identity below is unit-tested rather than assumed.
/// (FX) Convert only when BOTH currencies are known AND they differ. The empty half is the sharp edge:
/// Yahoo occasionally omits `meta.currency`, and `"" != "USD"` would send a plain US name through a
/// USD→EUR rate — an ~1.16x error on exactly the names this whole change promises not to touch. Unknown
/// therefore means "leave the price alone", never "assume it needs fixing".
///
/// One predicate so live, backtest and the P/E path can't drift into three subtly different rules.
pub fn needs_fx(from: &str, to: &str) -> bool {
    !from.is_empty() && !to.is_empty() && !from.eq_ignore_ascii_case(to)
}

/// (#82) How many of today's shares one share bought on `since` has become: the product of every
/// split ratio effective AFTER that date. 1.0 when there were none, which is the overwhelming case.
///
/// This exists because a journaled price and a freshly fetched chart disagree about what "one share"
/// means. Yahoo retro-adjusts `close` for splits, so a series read today prices the CURRENT share;
/// `.screen_snapshots.jsonl` holds the price as it was quoted on the day, for the share of that day.
/// Dividing the old price by this factor moves it into today's definition — and skipping that step
/// books a 10:1 split as a permanent -90%, in the one artefact whose whole purpose is to be an honest
/// out-of-sample record. `track` posted that number for real.
///
/// STRICTLY AFTER, not on-or-after: a split effective on the snapshot date is already in the price
/// that day's screen quoted, so counting it would correct a price that needs no correcting.
///
/// Returns a factor, never a corrected price, so the one direction this can be applied in is fixed at
/// the call site rather than argued about at each of them.
pub fn split_factor_since(splits: &[(NaiveDate, f64)], since: NaiveDate) -> f64 {
    splits.iter().filter(|(d, r)| *d > since && *r > 0.0).map(|(_, r)| r).product()
}

pub fn convert_price(close_native: f64, from: &str, to: &str, eur_from: Option<f64>, eur_to: Option<f64>) -> Option<f64> {
    if !needs_fx(from, to) {
        return Some(close_native); // same books -> EXACT: no rate, no multiply, no rounding
    }
    match (eur_from, eur_to) {
        (Some(a), Some(b)) if b > 0.0 && a > 0.0 => Some(close_native * a / b),
        _ => None, // unknown rate -> caller must drop the ratio, never guess it
    }
}

/// The quality-of-capital level to SCORE and DISPLAY: return on equity when equity is positive,
/// return on assets when it is not, None when neither is usable.
///
/// ROE is only meaningful when stockholders' equity is positive. Buyback-heavy filers (HCA, HLT) run
/// NEGATIVE equity, turning a profitable year into a fake "-113%" (NI +$1.5B ÷ equity -$4B) that read
/// as a plausible printed number. Equity's sign isn't carried on the row, but net income's is
/// (net margin), and NI÷equity flips sign exactly when equity < 0 — so a meaningful ROE must share net
/// income's sign. That also catches the double-negative fake (loss year + negative equity => ROE > 0).
///
/// Suppressing it was the old behaviour and left HCA with a blank quality column despite 15 years of
/// statements; the ROA fallback replaces the meaningless ratio with a meaningful one instead of a hole.
/// ROA runs lower than ROE by construction (assets > equity for any levered filer), so a name landing
/// here earns systematically less quality credit — deliberate, and the reason it isn't a free upgrade.
///
/// ONE fn because both lanes must agree: the live screen (fetch.rs) and the backtest (fund_factors).
/// A live-only filter is exactly the train-serve skew that let the factor sweep measure negative-equity
/// fakes the live path rejects.
///
/// (#42) The sign test alone misses the other half: equity that is POSITIVE but has been bought back
/// down to a rounding error. Colgate reports ROE +3948% on ROA 13.1% — its equity is 0.33% of assets —
/// and every sign test passes it. The discriminator is the EQUITY MULTIPLIER, ROE/ROA = assets/equity,
/// not the ROE LEVEL: measured across the 447 cached filers with a positive ROE, AAPL's +152% is a
/// 4.9x multiplier (p78, ordinary) on a genuinely elite 31.2% ROA, the SAME multiplier as ITW's
/// unremarkable +95%. Level conflates "earned a lot on its assets" with "has no equity left", so a
/// level rule flags AAPL and APP (3.4x) while passing BA (+41% on 31x) and AMP (+54% on 29x).
///
/// KNOWN MISFIRE, accepted deliberately: balance-sheet businesses live in the same multiplier band and
/// are not artifacts — IBKR 37.9x, AMP 29.2x, PFG 28.7x, MET 26.2x, PRU 23.9x. For a broker or insurer
/// the leverage IS the business, ROE is the right measure, and ROA is ~0.5% by construction, so they
/// land on a denominator that understates them. Nothing separates them from Colgate without sector
/// data, which this path does not carry (`sector_matches` reads the NAME string, not a sector field).
/// ROA understates them; it does not lie about them, which is the trade taken here.
pub fn quality_return(roe: Option<f64>, roa: Option<f64>, net_margin: Option<f64>) -> Option<f64> {
    // Both tests answer "is equity a credible denominator". Unjudgeable never blocks: no ROA means the
    // multiplier can't be computed (the FMP path fills no ROA at all), so the sign test decides alone.
    let credible = |r: f64| {
        net_margin.is_none_or(|nm| (r >= 0.0) == (nm >= 0.0))
            && !roa.is_some_and(|a| a > 0.0 && r / a >= MAX_EQUITY_MULT)
    };
    match roe {
        Some(r) if credible(r) => Some(r),
        _ => roa, // no ROE, a sign-inconsistent one, or a collapsed denominator -> return on ASSETS
    }
}

/// (#42) assets/equity above which equity stops being a credible denominator for ROE. 20x = equity
/// under 5% of assets; p97 of the positive-ROE filers cached here, and the gap between the artifacts
/// (CL 302x, LYV 84x, COR 51x, GDDY 37x, BA 31x, IT 25x, VRSK 20x) and the ordinary names below it.
/// A constant, not a knob: `fund_factors` takes no tuning, so a knob costs its signature and every
/// caller, and this is an accounting fact rather than a preference. Promote it if a sweep wants it.
pub const MAX_EQUITY_MULT: f64 = 20.0;

/// (#43) Return on INVESTED capital, pre-tax: EBIT ÷ (equity + net debt), %.
///
/// The one thing `quality` structurally cannot be. ROE divides by equity ALONE, so leverage inflates it:
/// a 3x-levered 35% ROE is a ~12% ROIC business. Over a 20-year hold a company's compounding rate
/// converges on the return it earns on ALL the capital it employs, borrowed included — and the
/// discriminating band is exactly where `quality_cap: 40` still resolves detail, since above that
/// ceiling every name already scores identically.
///
/// Reconstructed from levels already cached, so no refetch and no cache-key bump:
/// ```text
///   ebit   = op_margin/100 x revenue
///   ni     = net_margin/100 x revenue
///   equity = ni / (roe/100)     <- signed right by construction: a loss-maker has ni<0 AND roe<0, so
///                                  equity comes out POSITIVE. Genuine negative equity is the case where
///                                  those two signs DISAGREE, which is what makes this reconstruction
///                                  worth more than a sign test.
///   ic     = equity + net_debt
/// ```
///
/// PRE-TAX deliberately. A flat tax rate is a constant multiplier, and a constant multiplier cannot
/// reorder a ranking — only the term's weight/cap would absorb it. The assumption buys nothing measurable,
/// so it is not made.
///
/// UNKNOWN OR NON-POSITIVE INVESTED CAPITAL -> EBIT/ASSETS, the same shape `quality_return` uses for
/// ROE -> ROA. 18 of the 395 EBIT-computable filers cached here have bought back or leased their way to
/// invested capital <= 0 (BKNG, PM, MAR, YUM, DPZ, HLT, LVS, VRSN, CRWD, FTNT, ZS, TEAM, ALNY, CAH, DVA,
/// LYV, NRG, RF) — precisely the capital-light compounders this factor exists to find, so None-ing them
/// out would be backwards. Assets is a denominator that cannot go non-positive.
///
/// Reads NOTHING from `FundRow::roic`: that field is FMP-premium and has no assignment anywhere in the
/// tree, so it is None for every row the SEC path builds. Folding a filer-reported AFTER-tax ROIC into a
/// derived PRE-tax series would put two scales in one factor and re-rank on which feed happened to cover
/// a name. Derive for everyone or nobody.
///
/// COVERAGE: 395 of 509 cached filers. The 114 misses are banks, insurers and REITs, which file no
/// operating margin -> no EBIT under any denominator. ROIC is not meaningful for them anyway (their
/// capital structure IS the business); they keep scoring on the ROE-based `quality`, which still runs for
/// all 509. No credibility multiplier like `MAX_EQUITY_MULT` here: only 6 names clear 150%, and a bar
/// that cut MCK's artifact (1,639% on invested capital = 0.5% of assets) would take DECK (213% on 16%)
/// with it, which is genuinely capital-light. Every `growth_fund_extra` term carries its own `cap`, which
/// already clamps both to the same value as any excellent business.
pub fn roic_return(
    revenue: Option<f64>,
    op_margin: Option<f64>,
    net_margin: Option<f64>,
    roe: Option<f64>,
    roa: Option<f64>,
    net_debt: Option<f64>,
) -> Option<f64> {
    let rev = revenue?;
    let ebit = op_margin? / 100.0 * rev;
    let ni = net_margin? / 100.0 * rev;
    // A 0 in either return is a division by ZERO, not a zero return — filter, never fabricate.
    let ic = match (roe.filter(|r| *r != 0.0), net_debt) {
        (Some(r), Some(nd)) => Some(ni / (r / 100.0) + nd),
        _ => None, // a missing leg means invested capital is UNKNOWN, which lands on the same branch as dead
    };
    let denom = match ic {
        Some(ic) if ic > 0.0 => ic,
        _ => {
            let assets = ni / (roa.filter(|a| *a != 0.0)? / 100.0);
            (assets > 0.0).then_some(assets)? // signs disagreeing = bad data, not a business
        }
    };
    let roic = ebit / denom * 100.0;
    roic.is_finite().then_some(roic)
}

/// (FX) The rate in force ON `date`: the latest quote at or BEFORE it, never after. None before the
/// series starts, so an early cutoff drops its price-joined factors instead of borrowing a later rate.
///
/// The direction IS the whole function. `.range(date..).next()` would hand a 2016 cutoff a 2016-or-later
/// rate — look-ahead, in the one lane that exists to have none — and the two spellings differ by four
/// characters while producing plausible numbers either way. Hence pure, named, and pinned by a test.
pub fn rate_as_of(series: &BTreeMap<NaiveDate, f64>, date: NaiveDate) -> Option<f64> {
    series.range(..=date).next_back().map(|(_, r)| *r)
}

/// (Item 19) As-of earnings yield = EPS ÷ price, in % — a VALIDATABLE valuation level (high = cheap), the
/// honest counterpart to the live-only `pe_ratio` damp (which is backtest-blind). PROBE-ONLY: computed in
/// the backtest from the native as-of close + native EPS (same currency, so the ratio is clean). NOT wired
/// into the live screen — live `price_eur` is EUR while FMP EPS is USD, so a live computation would be a
/// currency train-serve skew (the Item 16 trap); wire live only once a native live price exists AND the
/// backtest probe shows both OOS halves +. None on a missing EPS or a non-positive price (no div-by-zero,
/// no garbage ratio).
pub fn earnings_yield(eps: Option<f64>, price: f64) -> Option<f64> {
    match eps {
        Some(e) if price > 0.0 => Some(e / price * 100.0),
        _ => None,
    }
}

/// (#147) The inverse of [`earnings_yield`] — an as-of P/E from a yield in %.
///
/// EXISTS BECAUSE THE BACKTEST WAS P/E-BLIND. `quote.pe_ratio` appeared ZERO times in
/// `commands/backtest.rs`, so `picks::value_factor` returned 1.0 on every walk-forward row and both
/// terms that read it were graded as different functions than the ones that ship live: the re-rating
/// leg of `picks::expected_return_pct` was identically 0 (found by (#145)), and (#1) zeroed
/// `growth_value_weight` for exactly that reason. The loop already computes the yield at
/// `backtest.rs`'s cutoff; this is the one conversion that was missing.
///
/// A RATIO IS CURRENCY-NEUTRAL, which is what makes this honest where `ev_ebitda_yield` had to stay
/// probe-only. `backtest.rs` computes the yield from an as-of close restated into the FILER's
/// currency so it matches the EPS beside it — and since both legs are then in that one currency, the
/// quotient is the same number the quote currency would have given. No rate is applied here and none
/// is needed.
///
/// Non-positive yields answer `None`, not a negative P/E: a loss-maker has no meaningful multiple,
/// and inventing one would put a "cheap" name at the top of the very lane this feeds. Mirrors
/// [`peg_yield_from_pe`], which does the opposite conversion with the same `100.0 /` constant.
pub fn pe_from_earnings_yield(ey: Option<f64>) -> Option<f64> {
    ey.filter(|&y| y > 0.0).map(|y| 100.0 / y)
}

/// (EV/EBITDA probe) As-of EBITDA yield = EBITDA ÷ enterprise value, in % — the capital-structure-neutral
/// cousin of `earnings_yield`. EV = market cap + net debt = shares·price + net_debt. PROBE-ONLY, same
/// currency discipline as earnings_yield: computed in the backtest from the native as-of close + native
/// SEC levels (clean ratio), left None by the live path (EUR price vs USD levels would skew). None unless
/// EBITDA is POSITIVE (EV/EBITDA is meaningless for a loss-maker — a negative multiple isn't "cheap", so
/// it None-outs rather than fabricating a signal), shares are positive, and EV ends up positive.
/// ponytail: net_debt None (rare — cash is the SEC anchor) degrades EV to market-cap only; the leverage
/// leg simply drops for that name. Tighten to require net_debt only if the probe shows an edge worth it.
pub fn ev_ebitda_yield(ebitda: Option<f64>, shares: Option<f64>, net_debt: Option<f64>, price: f64) -> Option<f64> {
    match (ebitda, shares) {
        (Some(e), Some(sh)) if e > 0.0 && sh > 0.0 && price > 0.0 => {
            let ev = sh * price + net_debt.unwrap_or(0.0);
            (ev > 0.0).then_some(e / ev * 100.0)
        }
        _ => None,
    }
}

/// (PEG probe) 1/PEG as a higher-is-better "yield" so it slots into the same sweep as earnings_yield:
/// PEG = (P/E) ÷ CAGR, so 1/PEG = (eps/price)·CAGR = `earnings_yield` · CAGR (unit-consistent with the
/// textbook PEG, where growth is the % NUMBER). peg_yield == 100 ⇔ PEG == 1; > 100 ⇔ PEG < 1, i.e. higher =
/// cheaper for its growth. THE SHIPPED growth-lane fund tilt since 2026-07-25 (receipt (#3) in
/// tests/ci-settings.yaml): standalone wide 12y n=1489 rho +0.14 edge +291.7 OOS +0.16|+0.13, beating the
/// previous earnings_yield tilt on all four dials. Same native-close discipline as earnings_yield, now
/// applied on BOTH sides (backtest loop + live enrich) so train and serve compute one number.
/// None unless earnings_yield is POSITIVE (a loss-maker isn't "cheap for growth" — no fabricated
/// signal) AND CAGR is positive (negative growth makes PEG sign-nonsense). Deliberately mirrors the
/// EV/EBITDA loss-maker None-out. Those two None-outs are why "rank PEG < 0 higher" is unimplementable
/// here by design, not by omission: a negative PEG needs negative earnings, and this returns None there.
/// NOTE the SCALE before touching `growth_fund_cap`: ~0-500, vs ~0-15 for earnings_yield.
pub fn peg_yield(eps: Option<f64>, cagr: Option<f64>, price: f64) -> Option<f64> {
    let ey = earnings_yield(eps, price).filter(|&y| y > 0.0)?; // eps>0 (earnings_yield itself allows eps<0)
    let g = cagr.filter(|&g| g > 0.0)?;                        // %/yr; negative growth -> PEG meaningless
    Some(ey * g)
}

/// Same peg_yield, entered from a P/E instead of eps+price — the form a FUND's look-through valuation
/// arrives in (`parse_fund_pe` reads `topHoldings.equityHoldings.priceToEarnings`; there is no per-share
/// EPS for a basket). eps = 100/pe at a notional price of 100 reproduces `earnings_yield` exactly, so
/// both None-outs above (non-positive earnings, non-positive growth) stay shared rather than re-derived
/// — which is the whole point of routing through `peg_yield` instead of writing `100.0 / pe * cagr` here.
pub fn peg_yield_from_pe(pe: f64, cagr: Option<f64>) -> Option<f64> {
    peg_yield((pe > 0.0).then(|| 100.0 / pe), cagr, 100.0)
}

/// One fiscal year of an income statement, rolled up from the quarterly `FundRow`s — the `report`
/// command's display row. Margins are %, revenue/eps in native units. `quarters` < 4 = an incomplete
/// fiscal year (most-recent partial, or a non-December fiscal-year-end straddling the calendar split);
/// the print layer flags it so a half-year isn't misread as a revenue cliff.
#[derive(Clone, Debug, PartialEq)]
pub struct AnnualReport {
    pub year: i32,
    pub revenue: f64,
    pub gross_margin: Option<f64>,
    pub op_margin: Option<f64>,
    pub net_margin: Option<f64>,
    pub eps: Option<f64>,
    pub shares: Option<f64>,          // diluted weighted-avg shares outstanding for the FY (mean of the year's rows) — feeds the buyback column
    // prior-FY eps/shares as the SAME filing stated them (see `FundRow::prior_eps`), carried up from
    // the year's LAST row. The YoY denominators, immune to splits and restatements. SEC-only: the FMP
    // path leaves them None, and its rows are quarterly anyway, so a prior there would be a prior
    // QUARTER — the wrong comparison entirely. None -> callers fall back to the previous AnnualReport.
    pub prior_eps: Option<f64>,
    pub prior_shares: Option<f64>,
    pub quarters: usize,
}

/// Roll the quarterly `FundRow`s up to one row per fiscal year for the `report` view. Newest year
/// first. Annual revenue = Σ quarter revenue; annual EPS = Σ quarter eps (None if no quarter reports
/// it); each annual margin = the REVENUE-WEIGHTED mean of the quarter margins, which equals
/// Σ(profit)/Σ(revenue) exactly (a quarter's margin% is profit/revenue), so no absolute profit line is
/// needed. A quarter missing a margin (or revenue) just drops out of that margin's weighting, never
/// fabricating a 0. ponytail: groups by `period_end.year()` — a non-Dec fiscal year can straddle the
/// calendar split; the `quarters` count exposes it, true fiscal-period grouping deferred until it bites.
pub fn annual_rollup(rows: &[FundRow]) -> Vec<AnnualReport> {
    let mut by_year: BTreeMap<i32, Vec<&FundRow>> = BTreeMap::new();
    for r in rows {
        by_year.entry(r.period_end.year()).or_default().push(r);
    }
    by_year
        .into_iter()
        .rev() // newest year first
        .map(|(year, qs)| {
            let revenue: f64 = qs.iter().filter_map(|r| r.revenue).sum();
            // revenue-weighted margin: Σ(margin·rev)/Σ(rev) over quarters that carry BOTH
            let wmargin = |pick: fn(&FundRow) -> Option<f64>| {
                let (num, den) = qs.iter().copied().fold((0.0, 0.0), |(n, d), r| match (pick(r), r.revenue) {
                    (Some(m), Some(rev)) => (n + m * rev, d + rev),
                    _ => (n, d),
                });
                (den > 0.0).then(|| num / den)
            };
            let eps_vals: Vec<f64> = qs.iter().filter_map(|r| r.eps).collect();
            // shares is a LEVEL, not a flow: MEAN the year's rows (don't sum). SEC gives 1 annual row/yr
            // -> mean = that value; FMP gives ~4 quarters of per-quarter weighted-avg diluted -> their mean
            // approximates the annual weighted-avg diluted share count. Good enough for a display column.
            let share_vals: Vec<f64> = qs.iter().filter_map(|r| r.shares).collect();
            let newest = qs.iter().copied().max_by_key(|r| r.period_end);
            AnnualReport {
                year,
                revenue,
                gross_margin: wmargin(|r| r.gross_margin),
                op_margin: wmargin(|r| r.op_margin),
                net_margin: wmargin(|r| r.net_margin),
                eps: (!eps_vals.is_empty()).then(|| eps_vals.iter().sum::<f64>()),
                shares: (!share_vals.is_empty()).then(|| share_vals.iter().sum::<f64>() / share_vals.len() as f64),
                // from the year's LAST period end — for SEC that is the single annual row, so these are
                // its own filing's comparatives verbatim.
                prior_eps: newest.and_then(|r| r.prior_eps),
                prior_shares: newest.and_then(|r| r.prior_shares),
                quarters: qs.len(),
            }
        })
        .collect()
}

/// The screen table's income-statement snapshot: (rev_yoy %, eps_yoy %, net_margin %, buyback %) of the
/// newest COMPLETE fiscal year, each vs the next-older year — the same math the `report` rows print, so
/// the two views can't disagree. "Complete" mirrors report's `*` mark: 1 quarter = an annual filing (SEC
/// rolls a fiscal year into one row), 4+ = a full quarterly year; 2-3 = genuinely partial, skipped so
/// a half-year isn't misread as a revenue cliff. YoY needs the older row too: last year in the data
/// has nothing to compare against -> that component is None, never 0. `buyback` is the net share-count
/// change sign-flipped (shares SHRANK -> positive = buying back = tax-deferred capital return).
pub fn income_snapshot(annual: &[AnnualReport]) -> Option<(Option<f64>, Option<f64>, Option<f64>, Option<f64>)> {
    let idx = annual.iter().position(|a| a.quarters == 1 || a.quarters >= 4)?;
    let a = &annual[idx];
    let older = annual.get(idx + 1);
    let rev_yoy = older.filter(|o| o.revenue > 0.0).map(|o| (a.revenue / o.revenue - 1.0) * 100.0);
    // Both per-share columns read the PRIOR YEAR OFF THE SAME FILING, so a split (comparatives
    // restated) and an issuance (comparatives untouched) come out as what they are instead of both
    // reading as a +200% share explosion. TPL: 69,059,252 -> 69,027,492 is a 0.05% buyback, not a
    // 3-for-1; COF's 383.6M -> 541.3M stays the real 41% dilution it is, and its EPS drop stops
    // being suppressed. Only when the filing carried no comparative do we fall back to the previous
    // AnnualReport plus the old |Δ|>40% guard.
    let eps_yoy = match a.prior_eps {
        Some(p) => yoy_pct(a.eps, Some(p)),
        None => eps_yoy_split_safe(a.eps, older.and_then(|o| o.eps), a.shares, older.and_then(|o| o.shares)),
    };
    let buyback = match a.prior_shares {
        Some(p) => yoy_pct(a.shares, Some(p)).map(|d| -d),
        None => match (a.shares, older.and_then(|o| o.shares)) {
            (Some(c), Some(p)) if p > 0.0 => Some((c / p - 1.0) * 100.0).filter(|d| d.abs() <= 40.0).map(|d| -d),
            _ => None,
        },
    };
    Some((rev_yoy, eps_yoy, a.net_margin, buyback))
}

/// Percent change between two values, None unless both are present and the base is non-zero.
pub fn yoy_pct(now: Option<f64>, prior: Option<f64>) -> Option<f64> {
    match (now, prior) {
        (Some(c), Some(p)) if p != 0.0 => Some((c / p - 1.0) * 100.0),
        _ => None,
    }
}

/// FALLBACK ONLY, for a row whose filing carried no comparative. EPS growth (%) between two
/// consecutive fiscal years taken from DIFFERENT filings, nulled when the share count jumped more than
/// 40% in the same step. Those two values sit on whatever share basis each filing was written on, so
/// the 40% share move is the only available hint that the bases differ — a crude proxy that cannot
/// tell a split from an acquisition and blanks both. Prefer `FundRow::prior_eps`, which removes the
/// question by reading both numbers off one income statement; this remains for the first year of
/// coverage and the FMP path. No share data -> nothing to judge -> keep the value (absence of a
/// disqualifying signal is not itself disqualifying — same stance as `quality_return`).
/// Shared by `income_snapshot` (the screen column) and report's annual table so the two views, which
/// read the same un-adjusted rows, cannot print contradictory verdicts side by side.
pub fn eps_yoy_split_safe(
    eps: Option<f64>,
    prior_eps: Option<f64>,
    shares: Option<f64>,
    prior_shares: Option<f64>,
) -> Option<f64> {
    let split = match (shares, prior_shares) {
        (Some(c), Some(p)) if p > 0.0 => ((c / p - 1.0) * 100.0).abs() > 40.0,
        _ => false,
    };
    match (eps, prior_eps) {
        (Some(c), Some(p)) if p != 0.0 && !split => Some((c / p - 1.0) * 100.0),
        _ => None,
    }
}

/// Compact number for money-scale displays: 2.34T / 391.0B / 25.6M / 1.5K, plain below. Tier on |v|,
/// sign kept. Shared by `report`'s annual table and the screen fundamentals footer.
pub(crate) fn humanize(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// (B) One-line fundamentals trajectory for screen's footer: the newest ≤5 COMPLETE fiscal years
/// (income_snapshot's completeness rule) as an oldest→newest revenue chain, plus the net-margin move
/// and EPS CAGR over the same window. The human "is the growth real, or one good year?" view —
/// DISPLAY-ONLY: every multi-year fundamental measured null as a rank input (fund-lane audit).
/// None with <2 complete years (no trajectory to show). EPS CAGR only when both endpoints are
/// profitable (a loss endpoint makes the ratio meaningless).
pub fn annual_brief(annual: &[AnnualReport]) -> Option<String> {
    let mut years: Vec<&AnnualReport> =
        annual.iter().filter(|a| a.quarters == 1 || a.quarters >= 4).take(5).collect();
    if years.len() < 2 {
        return None;
    }
    years.reverse(); // rollup is newest-first; a trajectory reads oldest→newest
    let (first, last, n) = (years[0], years[years.len() - 1], years.len());
    let chain = years.iter().map(|a| humanize(a.revenue)).collect::<Vec<_>>().join("→");
    let mut out = format!("rev {n}y {chain}");
    if first.revenue > 0.0 && last.revenue > 0.0 {
        let cagr = ((last.revenue / first.revenue).powf(1.0 / (n - 1) as f64) - 1.0) * 100.0;
        out.push_str(&format!(" ({cagr:+.0}%/yr)"));
    }
    if let (Some(a), Some(b)) = (first.net_margin, last.net_margin) {
        out.push_str(&format!(" · net {a:.0}%→{b:.0}%"));
    }
    // EPS CAGR, CHAINED over each year's own same-filing ratio — same reasoning as `fund_factors`.
    // End-to-end this leg spanned splits un-adjusted (NVDA's 2024 10:1 read as flat EPS growth), which
    // is why it used to demand a "verifiable" share history: every adjacent year present and no step
    // over 40%. That test blanked the leg for every splitter AND for Alphabet, whose pre-2022 counts
    // are per share class so the undimensioned tag is None. The chain needs neither — a restated
    // comparative IS the split adjustment.
    let steps: Vec<f64> = years
        .iter()
        .skip(1) // the oldest year's own prior sits outside the window
        .filter_map(|a| match (a.eps, a.prior_eps) {
            (Some(e), Some(p)) if e > 0.0 && p > 0.0 => Some(e / p),
            _ => None,
        })
        .collect();
    if steps.len() == n - 1 {
        let cagr = (steps.iter().product::<f64>().powf(1.0 / (n - 1) as f64) - 1.0) * 100.0;
        out.push_str(&format!(" · eps {cagr:+.0}%/yr"));
    } else if let (Some(a), Some(b)) = (first.eps, last.eps) {
        // no comparatives (FMP rows, or a filer that prints none) -> endpoint read behind the old
        // verifiable test. A missing count is not "no split", so an absent leg still blanks it.
        let verifiable = years.windows(2).all(|w| match (w[0].shares, w[1].shares) {
            (Some(p), Some(c)) if p > 0.0 => (c / p - 1.0).abs() <= 0.4,
            _ => false,
        });
        if a > 0.0 && b > 0.0 && verifiable {
            let cagr = ((b / a).powf(1.0 / (n - 1) as f64) - 1.0) * 100.0;
            out.push_str(&format!(" · eps {cagr:+.0}%/yr"));
        }
    }
    Some(out)
}

/// Pick ONE named as-of factor out of `FundFactors` for the growth lane's fund tilt. The name comes
/// from config (`growth_fund_factor`), so the user can route whichever factor the `backtest … fund`
/// probe shows predicts best WITHOUT a recompile. An unknown name -> None (neutral) so a typo degrades
/// to the price-only score instead of panicking. Keep this match in sync with `FundFactors` and the
/// `report_fund_lane` probe list so the backtest and the live screen always weigh the SAME factor.
pub fn select_fund_factor(f: &FundFactors, name: &str) -> Option<f64> {
    match name {
        "rev_cagr" => f.rev_cagr,
        "rev_accel" => f.rev_accel,
        "gross_margin" => f.gross_margin,
        "op_margin" => f.op_margin,
        "margin_trend" => f.margin_trend,
        "eps_growth" => f.eps_growth,
        "rev_yoy" => f.rev_yoy,     // (REV-YoY) 1y top-line growth — the printed column, not the multi-year CAGR
        "eps_yoy" => f.eps_yoy,     // (EPS-YoY) 1y bottom-line growth off the same-filing comparative
        "net_margin" => f.net_margin, // (NET%) margin LEVEL below the line; op_margin is the above-the-line twin
        "roe" => f.roe,                                   // RAW ROE (SEC feed; FMP free tier = None) — includes the negative-equity fakes; kept selectable only so the sweep's two rows stay comparable
        "quality" => f.quality,                           // quality of capital as the score reads it: ROE, or ROA where equity is negative
        "roic" => f.roic,                                 // (#43) pre-tax EBIT ÷ (equity + net debt) — `quality` without the leverage inflation. Exposed for `growth_fund_extra` to price; unweighted until measured
        "insider_net_buys_90d" => f.insider_net_buys_90d, // (Item 4) SEC Form-4 conviction, `backtest … insider`
        "earnings_yield" => f.earnings_yield,             // (Item 19) as-of valuation; live fill only when selected as the fund factor
        "ebitda_yield" => f.ebitda_yield,                 // (EV/EBITDA) capital-structure-neutral valuation; PROBE-ONLY (None live)
        "peg_yield" => f.peg_yield,                        // (PEG) 1/PEG = earnings_yield · CAGR, cheap-for-growth; THE SHIPPED live tilt (2026-07-25)
        "buyback_yield" => f.buyback_yield,               // as-of 1y share-count shrink (+ = buying back); backtest-testable candidate
        "fcf_margin" => f.fcf_margin,                     // (round 107) survival: cash generation
        "interest_cover" => f.interest_cover,             // (round 107) survival: debt-service headroom
        "net_cash_rev" => f.net_cash_rev,                 // (round 107) survival: balance-sheet cushion
        "margin_stability" => f.margin_stability,         // (round 109) cyclical detector: −std(net_margin)
        "accrual_gap" => f.accrual_gap,                   // (P2) −(earnings − cash earnings)/|earnings|: how cash-backed the profit is
        "asset_growth" => f.asset_growth,                 // (P3) −CAGR of total assets: how fast the balance sheet is being expanded
        "composite" => composite_factor(f),               // (Item 3) blend of the present factors
        _ => None,
    }
}

/// Build a Quote AS OF index `as_of` (inclusive) from the full history, filling ONLY the price-derived
/// fields the buy score reads — reusing the exact same horizon/SMA/vol/R²/drawdown fns on the `[..=as_of]`
/// slices, so the backtest scores a name exactly as the live tool would have on that day. note:
/// dividends / turnover / P/E / ROE are NOT reconstructed (no clean as-of source), so those score
/// terms go neutral here; the backtest validates the PRICE-based heuristic, which is the bulk of it.
///
/// (#88) `windows` is the SECOND half of the same train/serve skew `perf_nominal` closes. "so the
/// backtest scores a name exactly as the live tool would have on that day" — the sentence above — was
/// not true of the anchor: this used to hardcode an empty map here, so every leg fell back to
/// `default_anchor_half`, while the live path reads `anchor_windows` from settings and ci-settings
/// ships 1Y: 182 against that default's 90, 1M: 15 against 30 and 1W: 3 against 7. `return_1y` — the
/// input to `growth_min_1y_pct`, to `accel` (the heaviest term in the lane) and to `mom121` — was a
/// DIFFERENT ESTIMATOR in train and in serve. Pass an empty map for the old behaviour, which is what
/// the shipped lane still does until `backtest_anchor_windows` is turned on.
pub fn backtest_quote(
    ticker: &str,
    dates: &[NaiveDate],
    closes: &[f64],
    divs: &[(NaiveDate, f64)],
    as_of: usize,
    cadence: usize,
    windows: &BTreeMap<String, i64>,
) -> Quote {
    // (#94) THE AS-OF SPLICE TRIM. `parse_chart` used to drain the pre-splice head for the whole
    // record, which applies a redenomination retroactively: a 2015 glue joint deleted every pre-2015
    // bar for a 2005 cutoff, and `age_years` / `life_cagr` / MAXDD / trend R² were then measured on a
    // record this walk could not have seen. Trimming HERE, over `[..=as_of]` only, asks the question
    // the cutoff could actually answer. At the last bar it returns exactly what parse-time trimming
    // returned, so the live path is unchanged. 0 when the knob is off (the series arrived pre-trimmed,
    // so this finds nothing anyway) and for crypto, whose real 13x/wk weeks are not splices.
    let splice = if crate::config::splice_trim_point_in_time() && !crate::picks::is_currency_quoted(ticker) {
        splice_trim_start(&dates[..=as_of], &closes[..=as_of], crate::config::splice_max_weekly_rate())
    } else {
        0
    };
    let (d, c) = (&dates[splice..=as_of], &closes[splice..=as_of]);
    let mut quote = Quote::stub(ticker, "", "", ticker);
    // (D) as-of dividends. The module header used to say a walk-forward could not reconstruct these,
    // and the `dividend_weight` receipt called grading the term IMPOSSIBLE on that basis — both were
    // wrong. `Chart.divs` carries (ex-date, amount) for the whole history and the backtest fetch simply
    // dropped it; nothing had to be computed that `dividend_sums` doesn't already compute.
    //
    // LOOK-AHEAD SAFETY IS STRUCTURAL, not a filter written here: `dividends_in_window` anchors on
    // `dates.last()` and keeps only `ex_date <= last`, and `d` is the `[..=as_of]` slice — so a payout
    // after the cutoff cannot reach the score. Pass the full `dates` here and it silently would.
    //
    // Native currency on BOTH sides (`rate: None`, close as the price), and the yield is a ratio, so
    // the units cancel and no FX is needed. `price_eur` is read by exactly two things in picks.rs —
    // the dividend score term and the `div` display column — so filling it moves nothing else.
    quote.div_eur = dividend_sums(divs, d, None);
    quote.price_eur = c.last().copied();
    quote.perf = horizon_changes(d, c, None, windows, None); // calendar-based -> cadence-agnostic
    quote.drawdown_pct = pct_from_high(c); // all-time anchor as of the `as_of` index
    quote.range_pct = price_pct_rank(c);
    // cadence = bars/year (252 daily, 12 monthly): vol over ~1y of bars; the long MA window scaled
    // from its daily session count so the SAME ~4y/200wk span is used on either cadence (cadence=252
    // reproduces the daily path exactly). note: monthly bars APPROXIMATE the daily vol/MA, not
    // equal them — fine, a backtest run is single-cadence so the cross-sectional ranks stay consistent.
    //
    // (#97) "cross-sectional ranks stay consistent" is true and is not the whole story: every knob that
    // reads this field is an ABSOLUTE threshold in per-bar % units (`sharpe_cap`, `sharpe_cap_etf`,
    // `growth_max_vol`, `growth_max_vol_crypto`, `normal_volatility_pct`), and a threshold does not care
    // that the ranks held. `daily_equivalent` puts both cadences in the same units; see its doc.
    let rescale = crate::config::vol_daily_equivalent();
    quote.volatility_pct = volatility_pct(c, cadence).map(|v| daily_equivalent(v, cadence, rescale));
    // (r39) same window/cadence as the vol above so `sortino` vs `sharpe_ref` differ ONLY in which
    // returns reach the denominator — the whole question the probe asks. Rescaled with it for the same
    // reason: leaving one of the pair in bar units would make the probe's ratio a cadence artefact.
    quote.downside_dev_pct =
        downside_deviation_pct(c, cadence).map(|v| daily_equivalent(v, cadence, rescale));
    // (P4) ONE month of the run's own bars, scaled from `cadence` exactly as the long MA below is scaled
    // from its session count, so a daily run reproduces the live 21-session window bar for bar.
    //
    // CEILING, and it is a real one: on a MONTHLY-cadence run this collapses to a single bar, and "the
    // largest single-bar move in the last month" becomes "that month's own return" — a different signal
    // wearing the same field name. The gate reading it is therefore only meaningful on the daily lane;
    // a monthly sweep of it is measuring something else and must not be pooled with a daily one.
    quote.max_daily_1m = max_daily_pct(c, (cadence / 12).max(1));
    let long_ma = crate::config::LONG_MA_SESSIONS * cadence / 252;
    quote.below_ma_pct = below_long_ma_pct(c, long_ma);
    quote.above_ma_pct = above_long_ma_pct(c, long_ma);
    quote.trend_r2 = trend_r2(c);
    quote.trend_cagr = trend_cagr(c, cadence); // (#14) same fit, annualized by the run's cadence -> train==serve
    // (#3j) whole-life endpoint CAGR over the SAME `[..=as_of]` slice, from the SAME `core::life_cagr`
    // the live fetch calls -> train==serve, exactly like `trend_cagr` above.
    //
    // This field used to be left `None` here, and `(#3i)` mistook that for a fact of nature — it
    // recorded the whole-life bar as "unmeasurable by construction, a point-in-time slice has no
    // whole-life history". WRONG: the slice starts at the FIRST bar of the full series, so the history
    // is right there and was simply never read. Filling it is what makes both the `(#3i)` gate and
    // `use_life_cagr` measurable at all — without it `long_cagr_from` falls back to the leg and the
    // knob reads INERT, reporting "no change" and looking like a safe flip.
    //
    // Read `core::life_cagr`'s caveat before trusting a number off this on the DAILY path: "life"
    // there is the ~10y fetch window. Horizon questions need `backtest ... 12` (MAX-monthly).
    quote.life_cagr = life_cagr(d, c);
    // (#182) LISTING AGE, off the SAME `[..=as_of]` slice, through the SAME `age_years` helper the
    // live fetch reads -> train==serve, exactly like `life_cagr` above. This is the `(#3i)`
    // mistake one field over: `(#33)`'s receipt recorded age as BACKTEST-BLIND ("age_years is None in
    // the backtest pool, so the gate never touches the validated edge; it only shapes the LIVE
    // screen") and treated that as a property of the walk. It is not — `d` starts at the first bar of
    // the (splice-trimmed) series, so the age is right there and was simply never read. Filling it is
    // what makes `growth_min_age_years` gradeable AT ALL; unfilled, the gate reads INERT and any arm
    // on it reports "no change" and looks like a safe flip. The `(#94)` splice trim above is what
    // keeps this honest: age is measured over the record THIS cutoff could have seen, not the whole
    // post-redenomination history. Still display-only in the shipped lane — the gate ships 0.0.
    quote.age_years = age_years(d);
    // (#3l) same slice, same free-accessor knob as the live fetch -> train==serve, like the two above.
    quote.capped_cagr = capped_life_cagr(d, c, crate::config::life_cagr_max_years());
    // (#99) the total-return twin, over the SAME `[..=as_of]` slice and the SAME dividends `div_eur`
    // already reads — so it is as-of by construction, not by a filter written here. Filling it is what
    // makes the dividend leg SIGHTED: the arithmetic was inline in `fetch.rs` and therefore live-only,
    // so `growth_min_cagr`'s price-only bar has never been A/B-able against a total-return one.
    // `d.last()` bounds the sum at the cutoff; a later payout cannot reach this number.
    let divs_to_date: f64 =
        d.last().map_or(0.0, |cut| divs.iter().filter(|(dt, _)| dt <= cut).map(|(_, v)| v).sum());
    quote.tr_cagr = tr_life_cagr(d, c, divs_to_date);
    quote.max_drawdown_pct = max_drawdown_pct(c);
    // closes-derived risk stats for the standalone PRICE-RISK probes (backtest report only — no
    // score path reads roll*/worst*). The hit-rate/worst windows scale with the run's cadence
    // (bars/year) so a MONTHLY run measures the same 5y/10y calendar span the daily run does — a
    // rolling multi-year window needs decades of history, which only the monthly series carries.
    quote.roll5y_pos_pct = rolling_positive_pct(c, 5, cadence);
    quote.worst_5y_pct = worst_rolling_pct(c, 5, cadence);
    quote.roll10y_pos_pct = rolling_positive_pct(c, 10, cadence);
    quote.worst_10y_pct = worst_rolling_pct(c, 10, cadence);
    // underwater_yrs is /252-based AND score-read (growth_underwater_weight) -> keep it daily-only
    // so a monthly run can't feed a miscalibrated duration into the validated held-book verdict.
    if cadence == 252 {
        quote.underwater_yrs = longest_underwater_yrs(c);
    }
    // (#20) the LIVE growth lane excludes a name whose turnover is UNKNOWN (an untradeable/dead listing
    // like 0Y72.L). backtest_quote can't reconstruct turnover, but that absence is NOT a liquidity signal
    // here — mark it "liquid enough" so the exclusion stays a LIVE-ONLY gate (never fires in the backtest).
    // Uniform across every backtest name -> the additive liq_bonus is a constant offset -> cross-sectional
    // rank unchanged, validated edge untouched.
    quote.avg_turnover_eur = Some(1e15);
    quote
}

/// [1h, 6h, 12h] % changes = 1/6/12 hourly bars back. With ~hourly bars over several days this
/// fills for stocks too (was always n/a past a close when matched by wall-clock time).
pub fn intraday_changes(closes: &[f64]) -> [Option<f64>; 3] {
    [1, 6, 12].map(|b| intraday_pct(closes, b))
}

/// Average daily turnover (close × volume) over the last `n` sessions — a liquidity proxy.
/// Skips zero-turnover days (no volume reported); None if none usable. A thin name (tiny
/// turnover) is a riskier "opportunity" than a deep-liquid one, so picks can gate on it.
pub fn avg_turnover(closes: &[f64], volumes: &[f64], n: usize) -> Option<f64> {
    let len = closes.len().min(volumes.len());
    let start = len.saturating_sub(n);
    let vals: Vec<f64> = (start..len).map(|i| closes[i] * volumes[i]).filter(|x| *x > 0.0).collect();
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Average daily volume over the last `n` sessions, zero days skipped. For crypto the Yahoo
/// "volume" is ALREADY a notional currency amount (not a coin count), so this is turnover as-is
/// — no ×close (that would double-count). None if no usable day.
pub fn avg_volume(volumes: &[f64], n: usize) -> Option<f64> {
    let start = volumes.len().saturating_sub(n);
    let vals: Vec<f64> = volumes[start..].iter().copied().filter(|x| *x > 0.0).collect();
    if vals.is_empty() {
        return None;
    }
    Some(vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Daily volatility: standard deviation of daily % returns over the last `n` sessions — the
/// asset's "normal swing". Lets the picks score judge whether a drawdown is unusually deep for
/// THIS asset (a real sale) or just its everyday noise. None if too few sessions. FX-agnostic.
pub fn volatility_pct(closes: &[f64], n: usize) -> Option<f64> {
    let len = closes.len();
    let start = len.saturating_sub(n + 1); // n returns need n+1 closes
    let rets: Vec<f64> = (start + 1..len)
        .map(|i| (closes[i] - closes[i - 1]) / closes[i - 1] * 100.0)
        .filter(|r| r.is_finite())
        .collect();
    if rets.len() < 2 {
        return None;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rets.len() as f64;
    Some(var.sqrt())
}

/// (#97) Per-bar dispersion, restated in DAILY-equivalent units.
///
/// A stdev of per-bar returns scales with the square root of the bar length, so the same asset prints
/// ~√21 ≈ 4.6× more volatility on the monthly bars a long backtest walks than on the daily bars the
/// live screen walks. `span_to_bars` already keeps every WINDOW the same calendar length across
/// cadences — this is the amplitude half of the same train==serve rule, and nothing did it before.
///
/// It matters because every consumer of `volatility_pct` is an ABSOLUTE threshold: `sharpe_cap` clamps
/// `long_cagr / vol`, so a 4.6× larger denominator makes the ratio 4.6× smaller and the cap stops
/// binding in the very run that fitted it, while live it binds for nearly every name that clears the
/// CAGR gate. Same for the `growth_max_vol*` ceilings and the `normal_volatility_pct` divisor. All
/// three horizons the ship rule cites (20y, 12y, 8y) are monthly — `long || years >= 8` — so this is
/// the scale every one of those receipts was measured at.
///
/// `off` returns `v` untouched. At cadence 252 the factor is exactly 1.0 and `v * 1.0` is exactly `v`,
/// so the LIVE path is bit-identical at either setting; only a non-daily run can move.
pub fn daily_equivalent(v: f64, cadence: usize, on: bool) -> f64 {
    if on { v * (cadence as f64 / 252.0).sqrt() } else { v }
}

/// (P4) MAX: the LARGEST single-bar % gain over the last `n` bars — the trailing-month extreme, not a
/// dispersion. Same walk, same `.is_finite()` filter and the same "returns need n+1 closes" arithmetic as
/// `volatility_pct` above, so the two never disagree about which bars are in the window.
///
/// One return IS a max (unlike a stdev, which needs two), so the guard is `is_empty`, not `len() < 2`.
/// Only GAINS are asked for: the crash day is already priced by `max_drawdown_pct` and by the downside
/// deviation, while the up-spike is the one this whole field exists for. A window with no up day at all
/// legitimately returns the least-negative return, which is the correct answer to "the biggest single-bar
/// move" and keeps the factor defined on a name that only fell.
pub fn max_daily_pct(closes: &[f64], n: usize) -> Option<f64> {
    let len = closes.len();
    let start = len.saturating_sub(n + 1); // n returns need n+1 closes — the same window as its twin
    (start + 1..len)
        .map(|i| (closes[i] - closes[i - 1]) / closes[i - 1] * 100.0)
        .filter(|r| r.is_finite())
        .max_by(|a, b| a.total_cmp(b))
}

/// (r39 probe) Downside twin of `volatility_pct` — same window, same per-bar % units, but only the
/// NEGATIVE returns contribute: RMS of `min(r, 0)` over ALL n periods (the standard target-0
/// downside deviation, so a name with few down bars isn't flattered by a thin denominator).
/// The whole point: Sharpe's denominator punishes a compounder for its UP-moves too, which is
/// exactly backwards for a lane that exists to surface high-CAGR winners. This is the denominator
/// that doesn't. Same `<2 returns -> None` guard as its twin; an all-positive stretch legitimately
/// returns 0.0, so a caller building a ratio MUST guard against dividing by it.
pub fn downside_deviation_pct(closes: &[f64], n: usize) -> Option<f64> {
    let len = closes.len();
    let start = len.saturating_sub(n + 1); // n returns need n+1 closes
    let rets: Vec<f64> = (start + 1..len)
        .map(|i| (closes[i] - closes[i - 1]) / closes[i - 1] * 100.0)
        .filter(|r| r.is_finite())
        .collect();
    if rets.len() < 2 {
        return None;
    }
    let sq = rets.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / rets.len() as f64;
    Some(sq.sqrt())
}

/// Horizons over which `screen` totals dividends (label -> calendar days back).
pub const DIV_HORIZONS: &[(&str, i64)] =
    &[("1Y", 365), ("5Y", 1825), ("10Y", 3650), ("20Y", 7300)];

/// Sum of dividend amounts paid in `(last - days, last]`. `None` if the history doesn't
/// reach back `days` (window not fully covered → "n/a", like the perf horizons), so a
/// partial window never understates a payer. `divs` = (ex-date, amount/share).
pub fn dividends_in_window(divs: &[(NaiveDate, f64)], dates: &[NaiveDate], days: i64) -> Option<f64> {
    let last = *dates.last()?;
    let first = *dates.first()?;
    let start = last - Duration::days(days);
    if first > start {
        return None; // history too short to cover the whole window
    }
    Some(divs.iter().filter(|(d, _)| *d > start && *d <= last).map(|(_, a)| a).sum())
}

/// Total dividends/share in EUR for each DIV_HORIZON (native sum × `rate`; left native if
/// FX unknown, mirroring the price column). `None` per horizon = history too short.
pub fn dividend_sums(divs: &[(NaiveDate, f64)], dates: &[NaiveDate], rate: Option<f64>) -> Vec<Option<f64>> {
    DIV_HORIZONS
        .iter()
        .map(|(_, days)| dividends_in_window(divs, dates, *days).map(|s| s * rate.unwrap_or(1.0)))
        .collect()
}

/// Average annual dividend yield (%) per DIV_HORIZON: total EUR paid in the window /
/// years in the window / current EUR price × 100. `None` per horizon = short history or
/// no/zero EUR price. `div_eur` must be the `dividend_sums` output (aligned to DIV_HORIZONS).
pub fn dividend_yields(div_eur: &[Option<f64>], price_eur: Option<f64>) -> Vec<Option<f64>> {
    let px = price_eur.filter(|p| *p > 0.0);
    DIV_HORIZONS
        .iter()
        .enumerate()
        .map(|(i, (_, days))| {
            let total = div_eur.get(i).copied().flatten()?;
            let years = *days as f64 / 365.0;
            Some(total / years / px? * 100.0)
        })
        .collect()
}

/// Signed % from a horizon entry; "n/a" if missing. ≥1000% drops the decimal so a +26522% 20Y cell
/// still fits the 8-char horizon column instead of overflowing it.
pub fn pct_cell(entry: Option<&(String, f64)>) -> String {
    match entry {
        Some((_, pct)) if pct.abs() >= 1000.0 => format!("{:+.0}%", pct),
        Some((_, pct)) => format!("{:+.1}%", pct),
        None => "n/a".to_string(),
    }
}

/// Largest single unit of a duration: s/m/h/d/w/M/Y (M=30d, Y=365d approx).
pub fn fmt_duration(td: Duration) -> String {
    let s = td.num_seconds();
    for (unit, sec) in [
        ("Y", 31_536_000i64), ("M", 2_592_000), ("w", 604_800),
        ("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1),
    ] {
        if s >= sec {
            return format!("{}{}", s / sec, unit);
        }
    }
    "0s".to_string()
}

/// ('↑'/'↓'/'→', duration_str, days) for the current consecutive price run.
/// Span = first close of the run to the latest (calendar time).
pub fn trend_streak(dates: &[NaiveDate], closes: &[f64]) -> (&'static str, String, i64) {
    let sign = |a: f64, b: f64| -> i32 { (a > b) as i32 - (a < b) as i32 };
    if closes.len() < 2 {
        return ("→", "0s".to_string(), 0);
    }
    let n = closes.len();
    let direction = sign(closes[n - 1], closes[n - 2]);
    if direction == 0 {
        return ("→", "0s".to_string(), 0);
    }
    let mut i = n - 1;
    while i >= 1 && sign(closes[i], closes[i - 1]) == direction {
        i -= 1;
    }
    let arrow = if direction > 0 { "↑" } else { "↓" };
    let span = *dates.last().expect("dates non-empty: lockstep with closes, len >= 2 guarded above") - dates[i];
    (arrow, fmt_duration(span), span.num_days())
}

/// (at_all_time_high, at_all_time_low): latest close within tol of the max/min seen.
/// 'All-time' = the fetched history window (Yahoo range=max).
pub fn extreme_flags(closes: &[f64], tol: f64) -> (bool, bool) {
    if closes.is_empty() {
        return (false, false);
    }
    let last = *closes.last().expect("closes non-empty: closes.is_empty() guarded above");
    let hi = closes.iter().cloned().fold(f64::MIN, f64::max);
    let lo = closes.iter().cloned().fold(f64::MAX, f64::min);
    (last >= hi * (1.0 - tol), last <= lo * (1.0 + tol))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#112) The two suffix tables answer different questions and must not be allowed to drift into
    /// each other. This pins the three ways `listing_currency` can be wrong in a way nobody would see
    /// on the printed line: a euro venue silently dropping out of the bloc, a bare symbol being read
    /// as anything other than a US listing, and — the one that matters — an UNRECOGNISED venue being
    /// guessed instead of refused. A guessed currency is a guessed FX exposure, which is the whole
    /// thing the footer that reads this exists to stop.
    #[test]
    fn an_unknown_venue_has_no_currency_rather_than_a_guessed_one() {
        // four venues share EUR, and that is a fact about the euro, not a coincidence worth deriving
        for suf in ["DE", "PA", "AS", "MI", "MC", "VI", "LS", "BR", "HE", "IR"] {
            assert_eq!(listing_currency(&format!("X.{suf}")), Some("EUR"), "{suf} is in the euro bloc");
        }
        assert_eq!(listing_currency("VOD.L"), Some("GBP"), "the pence quote is the same GBP exposure");
        assert_eq!(listing_currency("NESN.SW"), Some("CHF"));
        assert_eq!(listing_currency("ATCO-B.ST"), Some("SEK"), "EU membership is not euro membership");
        assert_eq!(listing_currency("NOVO-B.CO"), Some("DKK"), "same, the other Nordic half");
        assert_eq!(listing_currency("AAPL"), Some("USD"), "no suffix = a US line, as `market` also reads it");
        assert_eq!(listing_currency("BRK-B"), Some("USD"), "a share-class dash is not a venue");
        assert_eq!(listing_currency("X.ZZ"), None, "an unrecognised venue is unknown, never guessed");
        // every suffix `suffix_country` knows must carry a currency too — the drift this pins
        for suf in ["DE", "L", "PA", "AS", "MI", "MC", "SW", "VI", "LS", "BR", "HE", "ST", "OL", "CO",
                    "IR", "TO", "HK", "T", "AX", "SA", "NS", "SS", "SZ", "KS"] {
            assert!(suffix_currency(suf).is_some(), "{suf} has a country but no currency");
            assert!(suffix_country(suf).is_some(), "{suf} has a currency but no country");
        }
    }

    /// (splice) the trimmer must cut a redenomination joint, not real market history: the 0A08.L
    /// shape (×19.6 in 28 days) trims; the NVR shape (×26 across 92 days of real quarterly bars)
    /// survives — that pair is the assert that fails if anyone reverts to a raw step-ratio test.
    #[test]
    fn splice_trim_start_cuts_redenominations() {
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // the real case: monthly bars, MXN head glued to a GBP tail at 2019-08-11 -> 2019-09-08
        let dates = vec![ymd(2019, 5, 5), ymd(2019, 8, 11), ymd(2019, 9, 8), ymd(2026, 7, 24)];
        let closes = vec![30.0, 30.53, 597.97, 650.0];
        assert_eq!(splice_trim_start(&dates, &closes, 2.0), 2, "keep the first post-splice close");
        // trimmed life CAGR is the fund's real single-digit number, not the +61%/yr fiction
        let fiction = life_cagr(&dates, &closes).unwrap();
        let real = life_cagr(&dates[2..], &closes[2..]).unwrap();
        assert!(fiction > 50.0, "untrimmed fiction: {fiction}");
        assert!(real < 10.0, "post-splice truth: {real}");
        // a wide REAL gap is not a splice: ×26 over 92 days = 1.28×/wk, under the 2.0 bar
        let gap_d = vec![ymd(2020, 1, 1), ymd(2020, 4, 2), ymd(2021, 1, 1)];
        assert_eq!(splice_trim_start(&gap_d, &[10.0, 260.0, 300.0], 2.0), 0, "raw-ratio regression");
        // sub-week clamp: one ordinary −10% DAY reads as 0.9/wk, not 0.9^7 = 0.48 tripping the floor
        let day_d = vec![ymd(2024, 6, 3), ymd(2024, 6, 4), ymd(2024, 6, 5)];
        assert_eq!(splice_trim_start(&day_d, &[100.0, 90.0, 91.0], 2.0), 0);
        // the DOWN half: a ÷100 GBp->GBP step trims exactly like the ×19.6 one
        let dn_d = vec![ymd(2019, 1, 6), ymd(2019, 2, 3), ymd(2019, 3, 3), ymd(2026, 1, 4)];
        assert_eq!(splice_trim_start(&dn_d, &[500.0, 495.0, 4.95, 6.0], 2.0), 2);
        // off (0.0) is inert — every recorded receipt still holds
        assert_eq!(splice_trim_start(&dates, &closes, 0.0), 0);
        // two splices -> the LAST one wins
        let two_d = vec![ymd(2015, 1, 4), ymd(2015, 2, 1), ymd(2019, 8, 11), ymd(2019, 9, 8), ymd(2026, 7, 24)];
        assert_eq!(splice_trim_start(&two_d, &[1.0, 100.0, 102.0, 2000.0, 2100.0], 2.0), 3);
        // degenerate inputs: empty and single-point series trim nothing
        assert_eq!(splice_trim_start(&[], &[], 2.0), 0);
        assert_eq!(splice_trim_start(&dates[..1], &closes[..1], 2.0), 0);
    }

    /// (#94) The as-of trim: a splice must not delete history that had not happened yet.
    ///
    /// Trimming at PARSE time keeps the last qualifying step in the WHOLE record, so a redenomination
    /// deletes the pre-splice head for every earlier cutoff too — the walk then scores a name whose
    /// `age_years`, `life_cagr`, MAXDD and trend R² are all measured on bars a 2005 observer would have
    /// had and a 2015 splice retroactively removed. The trimmer is pure and takes slices, so the fix is
    /// the slice: ask it what was knowable at `as_of` rather than what is known now. The last assertion
    /// is why the live path does not move — at the final bar the two questions have the same answer.
    #[test]
    fn a_splice_cannot_delete_history_that_had_not_happened_yet() {
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // 10 monthly bars; a ×30 redenomination lands between index 5 and 6 (30^(7/30) = 2.21×/wk).
        let dates: Vec<NaiveDate> =
            (0..10).map(|i| ymd(2000, 1, 1) + chrono::Duration::days(i * 30)).collect();
        let closes: Vec<f64> = (0..10).map(|i| if i < 6 { 10.0 } else { 300.0 }).collect();

        // parse time, the whole record: the head goes, and every cutoff inherits that.
        assert_eq!(splice_trim_start(&dates, &closes, 2.0), 6);
        // as-of bar 4 the splice is still in the future — nothing is trimmed, because nothing happened.
        assert_eq!(splice_trim_start(&dates[..=4], &closes[..=4], 2.0), 0);
        assert_eq!(splice_trim_start(&dates[..=5], &closes[..=5], 2.0), 0, "the bar BEFORE the step");
        // as-of bar 6 it has happened, and the trim is the same one parse time applies.
        assert_eq!(splice_trim_start(&dates[..=6], &closes[..=6], 2.0), 6);
        // at the LAST bar the as-of question and the whole-record question coincide. That identity is
        // the whole reason moving the trim leaves the live path byte-identical.
        assert_eq!(
            splice_trim_start(&dates[..=9], &closes[..=9], 2.0),
            splice_trim_start(&dates, &closes, 2.0)
        );
    }

    /// (#99) tr_life_cagr: life_cagr's total-return twin. Same endpoints, dividends added to the ending
    /// price, so the two are None in exactly the same cases and their DIFFERENCE is the dividend
    /// contribution in CAGR points — which is the quantity `picks::life_leg_cagr` adds when
    /// `growth_gate_on_tr_cagr` is on. Zero dividends must reproduce `life_cagr` exactly, or the knob's
    /// off-arm and its on-arm would differ for non-payers.
    #[test]
    fn tr_life_cagr_adds_the_payouts_the_price_series_dropped() {
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // 10 years, price doubles: 2^(1/10) − 1 ≈ 7.18%/yr price-only.
        let dates = [ymd(2015, 1, 2), ymd(2025, 1, 2)];
        let closes = [100.0, 200.0];
        let price = life_cagr(&dates, &closes).unwrap();
        assert!((price - 7.177).abs() < 1e-3, "{price}");
        // no payouts -> the twin IS life_cagr, bit for bit. A non-payer must not move under the knob.
        assert_eq!(tr_life_cagr(&dates, &closes, 0.0), life_cagr(&dates, &closes));
        // 30 of dividends over the decade: (230/100)^(1/age) − 1 ≈ 8.68%/yr, a +1.5pt uplift the
        // price-only bar never sees — which is the whole of finding C2.
        let tr = tr_life_cagr(&dates, &closes, 30.0).unwrap();
        assert!((tr - 8.685).abs() < 1e-3, "{tr}");
        assert!(tr > price, "adding cash to the endpoint cannot lower the CAGR");
        // LOWER BOUND by construction: reinvesting those payouts would compound higher still.
        assert!(tr < (((200.0 + 30.0 * 1.5) / 100.0f64).powf(0.1) - 1.0) * 100.0);
        // the guards must match life_cagr's exactly, or the difference stops being like-for-like.
        assert_eq!(tr_life_cagr(&[ymd(2025, 1, 2)], &[100.0], 5.0), None); // under 6mo
        assert_eq!(tr_life_cagr(&dates, &[0.0, 200.0], 5.0), None); // non-positive first close
        assert_eq!(tr_life_cagr(&[], &[], 5.0), None);
    }

    /// (#3l) capped_life_cagr: the window clamps to the LAST `max_years`, so a 40y series with a
    /// hot first decade reads LOWER capped than whole-life; a name younger than the cap keeps its
    /// whole life (min(age, cap)); 0.0 is off (None -> callers fall back to the rung, byte-inert);
    /// under 5y of history is None (pool guard — the rung ladder never ranked a name without a 5Y
    /// leg, and the window swap must not admit 6-month names); the cut lands at the first bar
    /// AT/AFTER `last − max_years`, never before it.
    #[test]
    fn age_years_is_days_over_365_25() {
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // (#182) the gate `growth_min_age_years` reads this number and nothing else re-derives it,
        // so the DIVISOR is the whole contract. 731 days across a leap year is 2.0013y — pinned
        // tightly enough that `%` (-> 365.75) or `*` (-> 267_047) cannot survive.
        let two_years = vec![ymd(2020, 1, 1), ymd(2022, 1, 1)];
        assert!((age_years(&two_years).unwrap() - 2.0013).abs() < 1e-3, "{:?}", age_years(&two_years));
        // a single bar is 0.0, NOT None: per non-negotiable #5 a None PASSES the age gate, so the
        // youngest possible listing must answer 0.0 and be CUT, never wave through as missing data.
        assert_eq!(age_years(&[ymd(2026, 1, 5)]), Some(0.0));
        assert_eq!(age_years(&[]), None);
    }

    #[test]
    fn capped_life_cagr_windows() {
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // 40y series, ×100 in the first decade then flat: whole life ~12%/yr, last 30y ~0%/yr
        // (bar 1 sits at 1996-02-02, safely AFTER the 30y cut of 1996-01-06 — the cut date shifts
        // by leap days, so the fixture leaves a month of slack rather than betting on the calendar)
        let dates =
            vec![ymd(1986, 1, 5), ymd(1996, 2, 2), ymd(2006, 1, 5), ymd(2016, 1, 5), ymd(2026, 1, 5)];
        let closes = vec![1.0, 100.0, 100.0, 100.0, 100.0];
        let whole = life_cagr(&dates, &closes).unwrap();
        let capped = capped_life_cagr(&dates, &closes, 30.0).unwrap();
        assert!(whole > 10.0, "whole-life carries the hot decade: {whole}");
        assert!(capped.abs() < 0.1, "last 30y are flat: {capped}");
        // window boundary: first bar at/after the cut starts the window -> the 1996 bar, not 2006
        assert_eq!(capped, life_cagr(&dates[1..], &closes[1..]).unwrap());
        // cap longer than the record = whole life (min(age, cap))
        assert_eq!(capped_life_cagr(&dates, &closes, 60.0), life_cagr(&dates, &closes));
        // 0.0 = off -> None, callers keep the rung CAGR (byte-inert default)
        assert!(capped_life_cagr(&dates, &closes, 0.0).is_none());
        // pool guard: a 4y-old name is declined even though life_cagr itself would price it
        let young_d = vec![ymd(2022, 1, 5), ymd(2026, 1, 5)];
        let young_c = vec![10.0, 30.0];
        assert!(life_cagr(&young_d, &young_c).is_some());
        assert!(capped_life_cagr(&young_d, &young_c, 30.0).is_none());
        // degenerate: empty series
        assert!(capped_life_cagr(&[], &[], 30.0).is_none());
    }

    /// (consistency) rolling_positive_pct: an always-rising series scores 100%, always-falling
    /// 0%; exactly one window of history (or less) is None — no windows means NO CLAIM, never a
    /// fake 100%; a non-positive close at a window start skips that window instead of counting it.
    #[test]
    fn rolling_consistency_semantics() {
        const WIN: usize = 5 * 252;
        let rising: Vec<f64> = (1..=WIN + 50).map(|i| i as f64).collect();
        assert_eq!(rolling_positive_pct(&rising, 5, 252), Some(100.0));
        let falling: Vec<f64> = (1..=WIN + 50).rev().map(|i| i as f64).collect();
        assert_eq!(rolling_positive_pct(&falling, 5, 252), Some(0.0));
        assert!(rolling_positive_pct(&rising[..WIN], 5, 252).is_none()); // no full window -> no claim
        assert!(rolling_positive_pct(&[], 5, 252).is_none());
        // bad bar: zero close at the ONLY window's start -> that window skips -> nothing counted
        let mut bad = rising[..WIN + 1].to_vec();
        bad[0] = 0.0;
        assert!(rolling_positive_pct(&bad, 5, 252).is_none());
        // (r16) decade fork: 6y of history HAS 5y windows but NO 10y window — the 10y stat must
        // say no-claim, never silently reuse the 5y answer (the win_years param must bite).
        let six_yrs: Vec<f64> = (1..=6 * 252).map(|i| i as f64).collect();
        assert!(rolling_positive_pct(&six_yrs, 5, 252).is_some());
        assert!(rolling_positive_pct(&six_yrs, 10, 252).is_none());
        // (r35) cadence: the SAME 5y window on MONTHLY bars needs only 5*12 points — so a 6-year
        // monthly series HAS a 5y window, where the daily bars_per_year (5*252) would find none.
        let monthly_6y: Vec<f64> = (1..=6 * 12).map(|i| i as f64).collect();
        assert!(rolling_positive_pct(&monthly_6y, 5, 12).is_some());
        assert!(rolling_positive_pct(&monthly_6y, 5, 252).is_none());
    }

    /// (underwater) longest_underwater_yrs: rising series never dips (0.0 — a legit value, not
    /// None); a falling series is underwater its whole length; a recovery resets the peak so only
    /// the longest run wins; data-hole closes are filtered before indexing; <2 usable -> None.
    #[test]
    fn underwater_semantics() {
        let rising: Vec<f64> = (1..=300).map(|i| i as f64).collect();
        assert_eq!(longest_underwater_yrs(&rising), Some(0.0));
        let falling: Vec<f64> = (1..=5).rev().map(|i| i as f64).collect();
        assert_eq!(longest_underwater_yrs(&falling), Some(4.0 / 252.0)); // 4 sessions since peak
        // dip (1) -> recovery at 101 resets the peak -> second, longer run (3) wins
        assert_eq!(longest_underwater_yrs(&[100.0, 90.0, 101.0, 95.0, 96.0, 97.0]), Some(3.0 / 252.0));
        // leading data hole filtered out, not treated as a 0.0 peak
        assert_eq!(longest_underwater_yrs(&[0.0, 100.0, 90.0, 101.0]), Some(1.0 / 252.0));
        assert_eq!(longest_underwater_yrs(&[100.0]), None);
        assert_eq!(longest_underwater_yrs(&[]), None);
    }

    /// (worst-Ny) worst_rolling_pct: the MINIMUM window return wins (window A +10% vs window
    /// B −10% -> −10); a non-positive window endpoint skips that window, not the whole series;
    /// no full window -> None (no claim).
    #[test]
    fn worst_rolling_semantics() {
        const WIN: usize = 5 * 252;
        let mut closes = vec![1.0; WIN + 6];
        closes[0] = 100.0;
        closes[WIN] = 110.0; // window A (i=0): +10%
        closes[5] = 100.0;
        closes[5 + WIN] = 90.0; // window B (i=5): -10%
        assert!((worst_rolling_pct(&closes, 5, 252).unwrap() + 10.0).abs() < 1e-9);
        closes[0] = 0.0; // bad endpoint skips window A only
        assert!((worst_rolling_pct(&closes, 5, 252).unwrap() + 10.0).abs() < 1e-9);
        assert!(worst_rolling_pct(&vec![1.0; WIN], 5, 252).is_none()); // no full window -> no claim
        assert!(worst_rolling_pct(&[], 5, 252).is_none());
        // (r16) decade fork: a 5y-window-deep series carries NO 10y claim
        assert!(worst_rolling_pct(&closes, 10, 252).is_none());
    }

    /// (r11) `calendar_year_returns`: complete-year pairs only — the current (partial) year and
    /// any year separated by a full-year data hole are skipped; zero closes never count as a
    /// year's last print; <2 usable years = empty (no claim).
    #[test]
    fn calendar_year_semantics() {
        let ymd = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // 2022..2024 full years + a partial 2025: last closes 100, 110, 99, (partial 120)
        let dates = vec![
            ymd(2022, 3, 1), ymd(2022, 12, 30),
            ymd(2023, 6, 1), ymd(2023, 12, 29),
            ymd(2024, 12, 31),
            ymd(2025, 2, 3),
        ];
        let closes = vec![90.0, 100.0, 105.0, 110.0, 99.0, 120.0];
        let r = calendar_year_returns(&dates, &closes);
        assert_eq!(r.len(), 2); // 2023 and 2024; partial 2025 skipped
        assert_eq!(r[0].0, 2023);
        assert!((r[0].1 - 10.0).abs() < 1e-9); // 100 -> 110
        assert_eq!(r[1].0, 2024);
        assert!((r[1].1 + 10.0).abs() < 1e-9); // 110 -> 99
        // a zero 2024-year-end print must not become the year's last close
        let z = calendar_year_returns(&dates, &[90.0, 100.0, 105.0, 110.0, 0.0, 120.0]);
        assert_eq!(z.len(), 1); // only 2023 survives (2024 has no positive close at all)
        // year hole: 2022 then 2024 -> non-consecutive, no fake "annual" pair
        let hole = calendar_year_returns(
            &[ymd(2022, 12, 30), ymd(2024, 12, 31), ymd(2025, 2, 3)],
            &[100.0, 99.0, 120.0],
        );
        assert!(hole.is_empty());
        assert!(calendar_year_returns(&[ymd(2024, 1, 2)], &[100.0]).is_empty()); // <2 years
        assert!(calendar_year_returns(&[], &[]).is_empty());
    }

    /// (#17/Step 4) `endpoint_avg`: n=1 = the raw last close (the inert default must be byte-identical);
    /// n>1 averages the last n; n beyond the history clamps to the whole series. Pure math, no config.
    #[test]
    fn endpoint_avg_smooths_last_n() {
        let closes = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(endpoint_avg(&closes, 1), 40.0); // default: raw last close
        assert_eq!(endpoint_avg(&closes, 2), 35.0); // mean of last 2
        assert_eq!(endpoint_avg(&closes, 0), 40.0); // 0 clamps up to 1
        assert_eq!(endpoint_avg(&closes, 99), 25.0); // clamps down to the full series
    }

    /// (#18) `span_to_bars`: a trading-days span means the same calendar time on any cadence —
    /// identity on daily bars, ÷21 on monthly bars, never 0.
    #[test]
    fn span_to_bars_converts_by_cadence() {
        assert_eq!(span_to_bars(105, 252), 105); // live daily: identity
        assert_eq!(span_to_bars(105, 12), 5); // 12y backtest monthly: ~5 months = 5 bars
        assert_eq!(span_to_bars(1, 252), 1); // inert default stays raw…
        assert_eq!(span_to_bars(1, 12), 1); // …on both cadences (min 1)
        // (#17) `measure_endpoint` feeds this the process-global `config::endpoint_smooth_days()`, so the
        // WIRING can't be asserted without mutating global config mid-run. Pin the shipped-inert default
        // instead: smoothing is edge-affecting (its own config receipt demands a 12y OOS re-validation
        // before changing), and switching it on silently is exactly what this catches.
        assert_eq!(crate::config::BuyHeuristic::default().endpoint_smooth_days, 1);
    }

    /// `select_fund_factor`: each config name maps to its FundFactors field; an unknown name -> None
    /// (neutral) so a typo'd config can never panic the score. Pure, no network.
    #[test]
    fn select_fund_factor_maps_names() {
        let f = FundFactors {
            rev_cagr: Some(1.0),
            rev_accel: Some(2.0),
            gross_margin: Some(3.0),
            op_margin: Some(4.0),
            margin_trend: Some(5.0),
            eps_growth: Some(6.0),
            rev_yoy: Some(19.0),
            eps_yoy: Some(20.0),
            net_margin: Some(21.0),
            roe: Some(11.0),
            quality: Some(18.0),
            roic: Some(22.0),
            insider_net_buys_90d: Some(7.0),
            eps_ttm: Some(8.0),
            earnings_yield: Some(9.0),
            ebitda_ttm: Some(50.0),
            shares_ttm: Some(2.0),
            net_debt: Some(-10.0),
            ebitda_yield: Some(16.0),
            peg_yield: Some(17.0),
            buyback_yield: Some(10.0),
            fcf_margin: Some(12.0),
            interest_cover: Some(13.0),
            net_cash_rev: Some(14.0),
            margin_stability: Some(15.0),
            accrual_gap: Some(23.0),
            asset_growth: Some(24.0),
            eps_never_reported: false,
        };
        assert_eq!(select_fund_factor(&f, "rev_accel"), Some(2.0));
        assert_eq!(select_fund_factor(&f, "margin_trend"), Some(5.0));
        assert_eq!(select_fund_factor(&f, "eps_growth"), Some(6.0));
        assert_eq!(select_fund_factor(&f, "rev_cagr"), Some(1.0));
        assert_eq!(select_fund_factor(&f, "insider_net_buys_90d"), Some(7.0)); // (Item 4)
        assert_eq!(select_fund_factor(&f, "earnings_yield"), Some(9.0)); // (Item 19)
        assert_eq!(select_fund_factor(&f, "ebitda_yield"), Some(16.0)); // (EV/EBITDA)
        assert_eq!(select_fund_factor(&f, "peg_yield"), Some(17.0)); // (PEG probe)
        assert_eq!(select_fund_factor(&f, "buyback_yield"), Some(10.0));
        assert_eq!(select_fund_factor(&f, "roe"), Some(11.0)); // RAW ROE; NOT in composite (a level, and the blend already failed the lane)
        assert_eq!(select_fund_factor(&f, "quality"), Some(18.0)); // the ROE/ROA resolution — a DIFFERENT field, so the two names can't collapse into one
        assert_eq!(select_fund_factor(&f, "roic"), Some(22.0)); // (#43) the name `growth_fund_extra` selects on — a THIRD distinct field, not an alias of the two above
        assert_eq!(select_fund_factor(&f, "fcf_margin"), Some(12.0)); // (round 107) survival levels; NOT in composite either
        assert_eq!(select_fund_factor(&f, "interest_cover"), Some(13.0));
        assert_eq!(select_fund_factor(&f, "net_cash_rev"), Some(14.0));
        assert_eq!(select_fund_factor(&f, "margin_stability"), Some(15.0)); // (round 109)
        assert_eq!(select_fund_factor(&f, "accrual_gap"), Some(23.0)); // (P2) a FOURTH distinct field — not an alias of fcf_margin, which measures the level rather than the gap to earnings
        assert_eq!(select_fund_factor(&f, "asset_growth"), Some(24.0)); // (P3) the balance-sheet twin of rev_cagr, and NOT rev_cagr — a filer can grow assets while revenue stalls
        assert_eq!(select_fund_factor(&f, "composite"), Some(3.5)); // (Item 3) mean(1..6) = 21/6, valuation excluded (buyback/valuation not blended)
        assert_eq!(select_fund_factor(&f, "nope"), None); // unknown -> neutral, never panics
        // (Item 19) earnings_yield helper: EPS/price in %, guarded against div-by-zero / missing EPS
        assert_eq!(earnings_yield(Some(5.0), 100.0), Some(5.0)); // 5/100 = 5%
        assert_eq!(earnings_yield(Some(-2.0), 50.0), Some(-4.0)); // loss-maker -> negative yield (floored later in score)
        assert_eq!(earnings_yield(Some(5.0), 0.0), None); // non-positive price -> None, no div-by-zero
        assert_eq!(earnings_yield(None, 100.0), None); // no EPS -> None

        // (#147) the inverse, the conversion that un-blinds the backtest's P/E. Round-trips
        // `earnings_yield` exactly, which is the property the fill in backtest.rs depends on.
        assert_eq!(pe_from_earnings_yield(Some(5.0)), Some(20.0)); // 5% yield <=> P/E 20
        assert_eq!(pe_from_earnings_yield(Some(2.5)), Some(40.0)); // and it is not a constant
        assert_eq!(pe_from_earnings_yield(Some(-4.0)), None); // loss-maker: NO negative multiple
        assert_eq!(pe_from_earnings_yield(Some(0.0)), None); // zero yield -> None, no div-by-zero
        assert_eq!(pe_from_earnings_yield(None), None); // nothing in -> nothing out
        // the round trip both directions, so neither side can drift to its own constant
        assert_eq!(pe_from_earnings_yield(earnings_yield(Some(5.0), 100.0)), Some(20.0));
        // (EV/EBITDA) ebitda_yield = EBITDA / (shares·price + net_debt), %, high = cheap
        assert_eq!(ev_ebitda_yield(Some(50.0), Some(2.0), Some(50.0), 25.0), Some(50.0)); // EV = 2*25 + 50 = 100 -> 50/100 = 50%
        assert_eq!(ev_ebitda_yield(Some(20.0), Some(2.0), Some(-10.0), 15.0), Some(100.0)); // net CASH: EV = 30 − 10 = 20 -> 20/20 = 100%
        assert_eq!(ev_ebitda_yield(Some(30.0), Some(3.0), None, 10.0), Some(100.0)); // no net_debt -> EV = mkt cap only (30) -> 30/30
        assert_eq!(ev_ebitda_yield(Some(-5.0), Some(2.0), Some(0.0), 10.0), None); // negative EBITDA -> None (multiple meaningless)
        assert_eq!(ev_ebitda_yield(Some(50.0), Some(2.0), Some(0.0), 0.0), None); // non-positive price -> None
        assert_eq!(ev_ebitda_yield(Some(50.0), None, Some(0.0), 10.0), None); // no shares -> no market cap -> None
        assert_eq!(ev_ebitda_yield(Some(10.0), Some(1.0), Some(-100.0), 10.0), None); // net cash swamps mkt cap -> EV<=0 -> None
        // (PEG probe) peg_yield = earnings_yield(%) · CAGR(%-number) = 1/PEG · 100. peg_yield == 100 ⇔ PEG == 1; > 100 ⇔ PEG < 1 (cheap for growth)
        assert_eq!(peg_yield(Some(5.0), Some(20.0), 100.0), Some(100.0)); // ey 5% · g 20 = PEG (20/20)=1 marker
        assert_eq!(peg_yield(Some(5.0), Some(40.0), 100.0), Some(200.0)); // faster growth same price -> PEG 0.5 -> yield 200 (>100)
        assert_eq!(peg_yield(Some(-2.0), Some(20.0), 50.0), None); // loss-maker -> earnings_yield<0 filtered -> None (no fabricated "cheap")
        assert_eq!(peg_yield(Some(5.0), Some(-10.0), 100.0), None); // negative growth -> PEG sign-nonsense -> None
        assert_eq!(peg_yield(Some(5.0), Some(0.0), 100.0), None); // zero growth -> PEG infinite -> None
        assert_eq!(peg_yield(Some(5.0), None, 100.0), None); // no CAGR -> None
        assert_eq!(peg_yield(Some(5.0), Some(20.0), 0.0), None); // non-positive price -> earnings_yield None -> None
    }

    /// (FX) The three rules the price-joined factors rest on, pinned because each fails silently.
    ///
    /// 1. Same currency is the IDENTITY — bit-for-bit, before any rate is consulted. Every US filer
    ///    takes this path, so a regression here would move the validated edge without touching a factor.
    /// 2. Unknown currency ("") means LEAVE IT ALONE, not "convert". `"" != "USD"` is the trap: it would
    ///    push a plain US name through a USD→EUR rate and produce a ~1.16x wrong yield that reads fine.
    /// 3. `rate_as_of` looks BACKWARD only. Forward would be look-ahead in the walk-forward lane.
    #[test]
    fn fx_conversion_rules() {
        // 1 — identity, and note NO rates are supplied: the short-circuit must fire before it needs them
        assert_eq!(convert_price(100.0, "USD", "USD", None, None), Some(100.0));
        assert_eq!(convert_price(100.0, "usd", "USD", None, None), Some(100.0)); // case is not a currency difference
        // 2 — either side unknown -> untouched, never converted
        assert!(!needs_fx("", "USD"));
        assert!(!needs_fx("USD", ""));
        assert!(needs_fx("USD", "EUR"));
        assert_eq!(convert_price(100.0, "", "USD", Some(1.0), Some(0.86)), Some(100.0));
        // real conversion: EUR per USD 0.86, target EUR (rate 1.0) -> a $100 ADR is €86 in the filer's books
        assert_eq!(convert_price(100.0, "USD", "EUR", Some(0.86), Some(1.0)), Some(86.0));
        assert_eq!(convert_price(100.0, "USD", "EUR", None, Some(1.0)), None); // missing rate -> drop, don't guess
        assert_eq!(convert_price(100.0, "USD", "EUR", Some(0.86), Some(0.0)), None); // zero rate -> no div-by-zero
        // 3 — direction. Series has Jan and Mar; a Feb cutoff must see JAN's rate, never March's.
        let d = |m| NaiveDate::from_ymd_opt(2024, m, 1).unwrap();
        let series: BTreeMap<NaiveDate, f64> = [(d(1), 0.90), (d(3), 0.80)].into_iter().collect();
        assert_eq!(rate_as_of(&series, d(2)), Some(0.90), "look-ahead: a Feb cutoff cannot see March's rate");
        assert_eq!(rate_as_of(&series, d(3)), Some(0.80)); // exact hit is in-range (..=)
        assert_eq!(rate_as_of(&series, d(4)), Some(0.80)); // after the end -> last known, the honest carry-forward
        assert_eq!(rate_as_of(&series, d(1).pred_opt().unwrap()), None); // before the start -> no rate exists yet
        assert_eq!(rate_as_of(&BTreeMap::new(), d(2)), None); // no series -> caller drops the factor
    }

    /// (#82) `split_factor_since` is the whole of the split correction, so every branch is pinned here
    /// and the callers only have to decide WHEN to divide, never by what.
    ///
    /// The load-bearing case is #2. A journal line and a freshly fetched chart disagree about what one
    /// share is: Yahoo retro-adjusts `close`, the journal keeps the price as quoted on the day. Miss
    /// this factor and a 10:1 split reads as a permanent -90% in the live out-of-sample record.
    #[test]
    fn split_factor_compounds_only_what_came_after() {
        let d = |y, m| NaiveDate::from_ymd_opt(y, m, 1).unwrap();
        // 1 — the overwhelming case: no splits at all is the EMPTY PRODUCT, which is 1.0 and not 0.0.
        //     A factor of 0.0 would divide every price in the journal by zero.
        assert_eq!(split_factor_since(&[], d(2020, 1)), 1.0);
        let splits = [(d(2021, 1), 2.0), (d(2023, 1), 5.0)];
        // 2 — a price quoted before both is in a share definition 10x coarser than today's
        assert_eq!(split_factor_since(&splits, d(2020, 1)), 10.0, "2:1 then 5:1 compound, they do not add");
        assert_eq!(split_factor_since(&splits, d(2022, 1)), 5.0, "only the split that came after counts");
        assert_eq!(split_factor_since(&splits, d(2024, 1)), 1.0, "both already in the quoted price");
        // 3 — STRICTLY after. A split effective ON the snapshot date is already in that day's quote,
        //     so counting it would "correct" a price that needs no correcting — a 5x error, wrong way.
        assert_eq!(split_factor_since(&splits, d(2023, 1)), 1.0, "on-the-day is already priced in");
        assert_eq!(split_factor_since(&splits, d(2023, 1).pred_opt().unwrap()), 5.0, "one day earlier is not");
        // 4 — a non-positive ratio is dropped, never multiplied in: the field is only ever a divisor,
        //     so a 0.0 from a malformed payload would wipe the whole product out.
        assert_eq!(split_factor_since(&[(d(2021, 1), 0.0), (d(2021, 6), 3.0)], d(2020, 1)), 3.0);
        assert_eq!(split_factor_since(&[(d(2021, 1), -2.0)], d(2020, 1)), 1.0);
        // 5 — a reverse split is a ratio BELOW one, and it is carried, not filtered: 1:10 leaves one
        //     share where ten were, so the old price divides by 0.1 and gets ten times larger.
        assert_eq!(split_factor_since(&[(d(2021, 1), 0.1)], d(2020, 1)), 0.1);
    }

    /// `quality_return`: ROE when equity is positive, ROA when it isn't. The sign test is indirect (the
    /// row carries no equity, only net margin, and NI÷equity flips sign exactly when equity < 0), which
    /// is why every branch is pinned — a wrong one shows up as a plausible number, not an error.
    #[test]
    fn quality_return_falls_back_on_negative_equity() {
        // normal filers: ROE kept verbatim, ROA ignored even when present
        assert_eq!(quality_return(Some(30.1), Some(8.0), Some(15.0)), Some(30.1));
        assert_eq!(quality_return(Some(-5.0), Some(-2.0), Some(-3.0)), Some(-5.0)); // genuine loss-maker: real negative ROE
        assert_eq!(quality_return(Some(-27.0), Some(4.0), None), Some(-27.0)); // no margin -> can't judge -> keep ROE
        // negative equity: profit ÷ negative equity (HCA -112.6 on a +9% margin) -> ROA instead
        assert_eq!(quality_return(Some(-112.6), Some(9.0), Some(9.0)), Some(9.0));
        // double negative: loss ÷ negative equity reads POSITIVE — the fake the sign test exists to catch
        assert_eq!(quality_return(Some(40.0), Some(-3.5), Some(-3.0)), Some(-3.5));
        // no ROE at all -> ROA carries the row rather than blanking it
        assert_eq!(quality_return(None, Some(7.0), Some(12.0)), Some(7.0));
        // neither -> None (NEUTRAL in the score), never a fabricated 0
        assert_eq!(quality_return(None, None, Some(12.0)), None);
        assert_eq!(quality_return(Some(-112.6), None, Some(9.0)), None); // meaningless ROE, no fallback -> drop it

        // (#42) COLLAPSED (but positive) equity — every sign test above passes these, which is the whole
        // reason the multiplier test exists. Real cached SEC rows, so the numbers are the live ones.
        assert_eq!(quality_return(Some(3948.1), Some(13.1), Some(11.0)), Some(13.1)); // CL, 302x
        assert_eq!(quality_return(Some(406.8), Some(10.9), Some(12.0)), Some(10.9)); // GDDY, 37x
        assert_eq!(quality_return(Some(41.0), Some(1.3), Some(2.0)), Some(1.3)); // BA, 31x — a MODEST ROE, caught on leverage
        // ...and the names a LEVEL rule would wrongly hit. AAPL's 4.9x is the same leverage as ITW's
        // 5.0x; both keep their ROE, which is the asymmetry the old |ROE|>100 cell could not see.
        assert_eq!(quality_return(Some(151.9), Some(31.2), Some(26.9)), Some(151.9)); // AAPL, 4.9x
        assert_eq!(quality_return(Some(95.0), Some(19.0), Some(14.0)), Some(95.0)); // ITW, 5.0x
        assert_eq!(quality_return(Some(156.2), Some(45.9), Some(30.0)), Some(156.2)); // APP, 3.4x
        // exactly AT the bar swaps (`>=`), a hair under keeps — the boundary, since 20x is a chosen line
        assert_eq!(quality_return(Some(200.0), Some(10.0), Some(10.0)), Some(10.0));
        assert_eq!(quality_return(Some(199.0), Some(10.0), Some(10.0)), Some(199.0));
        // no ROA -> multiplier unjudgeable -> the sign test decides alone, exactly as before (#42)
        assert_eq!(quality_return(Some(3948.1), None, Some(11.0)), Some(3948.1));
        // a NON-POSITIVE ROA can't form a multiplier (assets are positive, so roa <= 0 means a LOSS, and
        // the sign test already owns that case). Guard against a divide that would flip the comparison.
        assert_eq!(quality_return(Some(-50.0), Some(-2.0), Some(-4.0)), Some(-50.0));
    }

    /// (#43) `roic_return`: EBIT ÷ (equity + net debt), with equity RECONSTRUCTED as NI ÷ ROE — every
    /// number derived, none of them filed, so each branch is pinned against a hand-computed value. A
    /// wrong branch here prints a plausible percentage, never an error.
    #[test]
    fn roic_return_derives_and_falls_back() {
        let approx = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|g| (g - want).abs() < 1e-9), "got {got:?}, want {want}");
        };
        // THE THESIS, arithmetically. rev 1000, op 12% -> EBIT 120; net 7% -> NI 70; ROE 35% -> equity
        // 200; +400 net debt -> invested capital 600 -> ROIC 20.0%. The same business reads 35% on ROE
        // and 20% on ROIC, and the 15-point gap IS the borrowed capital `quality` cannot see.
        approx(roic_return(Some(1000.0), Some(12.0), Some(7.0), Some(35.0), Some(5.0), Some(400.0)), 20.0);
        // …and with NO leverage the two must agree, or the reconstruction is wrong: equity 200, net debt
        // 0 -> IC 200 -> EBIT 120 / 200 = 60%, on the same 35% ROE. (EBIT > NI, so ROIC > ROE unlevered
        // — the tax-and-interest wedge, not an error.)
        approx(roic_return(Some(1000.0), Some(12.0), Some(7.0), Some(35.0), Some(5.0), Some(0.0)), 60.0);

        // NEGATIVE INVESTED CAPITAL (BKNG shape): bought back past zero equity. NI 250 ÷ ROE −50% ->
        // equity −500; +200 net debt -> IC −300 -> the EBIT/ASSETS branch. ROA 10% -> assets 2500 ->
        // 300/2500 = 12.0%. Note the ROE here is a FAKE (profit ÷ negative equity), which is exactly
        // the name this factor must not blank out.
        approx(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(-50.0), Some(10.0), Some(200.0)), 12.0);
        // net CASH deeper than equity reaches the same branch from the other side
        approx(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(50.0), Some(10.0), Some(-800.0)), 12.0);

        // BANK / INSURER / REIT: no operating margin filed -> no EBIT under ANY denominator -> None,
        // NOT a fabricated 0. 114 of the 509 cached filers land here and keep scoring on `quality`.
        assert_eq!(roic_return(Some(1000.0), None, Some(7.0), Some(35.0), Some(5.0), Some(400.0)), None);
        assert_eq!(roic_return(None, Some(12.0), Some(7.0), Some(35.0), Some(5.0), Some(400.0)), None);
        assert_eq!(roic_return(Some(1000.0), Some(12.0), None, Some(35.0), Some(5.0), Some(400.0)), None);

        // ROE 0 is a division by ZERO, not a zero return: invested capital becomes UNKNOWN, which takes
        // the same assets branch as dead capital rather than emitting an infinity.
        approx(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(0.0), Some(10.0), Some(200.0)), 12.0);
        // …and a missing net_debt leg likewise leaves IC unknown -> assets, not a fabricated unlevered 0
        approx(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(50.0), Some(10.0), None), 12.0);
        // no assets to fall back on either -> None
        assert_eq!(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(0.0), None, Some(200.0)), None);
        assert_eq!(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(0.0), Some(0.0), Some(200.0)), None);
        // NI and ROA disagreeing in sign implies negative assets — impossible, so bad data -> None
        assert_eq!(roic_return(Some(1000.0), Some(30.0), Some(25.0), Some(0.0), Some(-10.0), Some(200.0)), None);

        // a genuine loss-maker keeps a real NEGATIVE ROIC (both signs agree -> equity stays POSITIVE):
        // NI −50 ÷ ROE −25% -> equity +200, +300 net debt -> IC 500, EBIT −80 -> −16.0%
        approx(roic_return(Some(1000.0), Some(-8.0), Some(-5.0), Some(-25.0), Some(4.0), Some(300.0)), -16.0);
    }

    /// (#43) `roic` rides the same `fund_as_of` join as every other level, so a filing made AFTER the
    /// cutoff can never reach it — the look-ahead guard the whole backtest rests on.
    #[test]
    fn roic_is_as_of_and_selectable() {
        let r = |y: i32, op: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(y, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y - 1, 12, 31).unwrap(),
            revenue: Some(1000.0),
            op_margin: Some(op),
            net_margin: Some(7.0),
            roe: Some(35.0),
            roa: Some(5.0),
            net_debt: Some(400.0),
            ..Default::default()
        };
        let rows = vec![r(2023, 12.0), r(2024, 60.0)]; // the 2024 filing would print 100%, not 20%
        let after = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap();
        let before = NaiveDate::from_ymd_opt(2023, 6, 1).unwrap();
        assert_eq!(fund_factors(&rows, after, 5).roic.map(|v| v.round()), Some(100.0));
        assert_eq!(fund_factors(&rows, before, 5).roic.map(|v| v.round()), Some(20.0), "the 2024 filing must not leak");
        assert_eq!(fund_factors(&[], after, 5).roic, None); // no coverage -> neutral, never 0
        // and the name `growth_fund_extra` routes on reaches the field
        assert_eq!(select_fund_factor(&fund_factors(&rows, before, 5), "roic").map(|v| v.round()), Some(20.0));
    }

    /// (Item 3) `composite_factor` = mean of the factors that are `Some`; <2 present -> None (a 1-factor
    /// composite would just be that factor, so route it directly instead). insider_net_buys is NOT in the
    /// blend (different units / source), only the six FMP-derived growth factors.
    #[test]
    fn composite_factor_means_present() {
        let two = FundFactors { rev_cagr: Some(10.0), op_margin: Some(20.0), ..Default::default() };
        assert_eq!(composite_factor(&two), Some(15.0)); // mean(10,20)
        let one = FundFactors { eps_growth: Some(9.0), ..Default::default() };
        assert_eq!(composite_factor(&one), None); // only 1 factor -> None
        assert_eq!(composite_factor(&FundFactors::default()), None); // nothing -> None
    }

    /// `annual_rollup`: quarters group by period_end YEAR (newest first), revenue + eps SUM, margins are
    /// revenue-weighted, and an incomplete year reports its real `quarters` count so the print layer flags it.
    #[test]
    fn annual_rollup_groups_and_weights() {
        let q = |y: i32, m: u32, rev: f64, gm: f64, eps: f64| FundRow {
            period_end: NaiveDate::from_ymd_opt(y, m, 28).unwrap(),
            revenue: Some(rev),
            gross_margin: Some(gm),
            eps: Some(eps),
            ..Default::default()
        };
        // 2022: 4 quarters; 2023: 3 quarters (partial). Out of order on purpose.
        let rows = vec![
            q(2022, 3, 100.0, 40.0, 1.0),
            q(2023, 9, 200.0, 60.0, 4.0),
            q(2022, 6, 100.0, 50.0, 2.0),
            q(2023, 3, 200.0, 50.0, 3.0),
            q(2022, 9, 200.0, 50.0, 1.5),
            q(2022, 12, 100.0, 50.0, 1.5),
            q(2023, 6, 200.0, 55.0, 3.5),
        ];
        let out = annual_rollup(&rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].year, 2023); // newest first
        assert_eq!(out[1].year, 2022);
        // 2022 revenue = 100+100+200+100 = 500; eps summed = 6.0; 4 quarters
        assert_eq!(out[1].revenue, 500.0);
        assert_eq!(out[1].eps, Some(6.0));
        assert_eq!(out[1].quarters, 4);
        // 2022 gross margin = Σ(gm·rev)/Σrev = (40·100+50·100+50·200+50·100)/500 = 24000/500 = 48.0
        assert!((out[1].gross_margin.unwrap() - 48.0).abs() < 1e-9);
        // 2023 is the partial year: 3 quarters, revenue 600, eps 10.5
        assert_eq!(out[0].quarters, 3);
        assert_eq!(out[0].revenue, 600.0);
        assert_eq!(out[0].eps, Some(10.5));
        // a missing margin/eps drops out, never fabricates 0
        let sparse = vec![FundRow { period_end: NaiveDate::from_ymd_opt(2024, 3, 28).unwrap(), revenue: Some(10.0), ..Default::default() }];
        let s = annual_rollup(&sparse);
        assert_eq!(s[0].gross_margin, None);
        assert_eq!(s[0].eps, None);
        assert_eq!(s[0].revenue, 10.0);
    }

    /// `eps_growth` spans `yrs` years, so its split guard walks CONSECUTIVE rows instead of testing the
    /// endpoints — an endpoint test gets both directions wrong (a 2:1 spread over 5y annualizes to
    /// +14.9% and slips through; a real 10%/yr buyback stacks to -41% and gets wrongly flagged).
    #[test]
    fn eps_growth_survives_buybacks_and_dies_on_splits() {
        let r = |y: i32, eps: f64, shares: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(y + 1, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y, 12, 31).unwrap(),
            eps: Some(eps),
            shares: Some(shares),
            ..Default::default()
        };
        let cutoff = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        // MNST's real as-reported EPS chain, 2:1 split in early 2024 (shares 100 -> 200 at FY2023).
        // Endpoints 2.64 -> 1.94 read as -6.0%/yr; the truth is roughly +8.0%/yr.
        let mnst = vec![
            r(2020, 2.64, 100.0), r(2021, 2.57, 100.0), r(2022, 2.23, 100.0),
            r(2023, 1.54, 200.0), r(2024, 1.49, 200.0), r(2025, 1.94, 200.0),
        ];
        assert_eq!(fund_factors(&mnst, cutoff, 5).eps_growth, None);
        // Same EPS chain, no split anywhere -> the decline is REAL and must still be reported.
        let real: Vec<FundRow> = mnst.iter().map(|x| FundRow { shares: Some(100.0), ..x.clone() }).collect();
        assert!(fund_factors(&real, cutoff, 5).eps_growth.unwrap() < 0.0);
        // The false positive an endpoint test would create: 10%/yr buyback = -41% cumulative over 5y,
        // every single step a legitimate -10%. EPS growth must survive.
        let buyback = vec![
            r(2020, 2.00, 100.0), r(2021, 2.30, 90.0), r(2022, 2.70, 81.0),
            r(2023, 3.10, 72.9), r(2024, 3.50, 65.61), r(2025, 4.00, 59.05),
        ];
        assert!(fund_factors(&buyback, cutoff, 5).eps_growth.unwrap() > 0.0);
        // No share counts at all -> nothing to judge -> keep the value.
        let noshares: Vec<FundRow> = mnst.iter().map(|x| FundRow { shares: None, ..x.clone() }).collect();
        assert!(fund_factors(&noshares, cutoff, 5).eps_growth.is_some());
    }

    /// (same-filing comparatives) The PRIMARY `eps_growth` path: compound each year's own same-filing
    /// ratio instead of dividing two endpoints that sit on different share bases. TPL's real chain,
    /// straight off `companyconcept` — it split 3-for-1 in BOTH 2024 and 2025, so the FY2024 and FY2025
    /// filings restate their comparatives (17.59 = 52.77/3, 6.573 = 19.72/3) while the stored rows keep
    /// each year at its original basis. Endpoint-wise that is 6.97 over 22.70 = -21.0%/yr; the truth is
    /// +22.6%/yr, and the old guard could only ever blank it, never measure it.
    #[test]
    fn eps_growth_chains_same_filing_steps_across_splits() {
        let r = |y: i32, eps: f64, prior: Option<f64>| FundRow {
            filed: NaiveDate::from_ymd_opt(y + 1, 2, 20).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y, 12, 31).unwrap(),
            eps: Some(eps),
            prior_eps: prior,
            ..Default::default()
        };
        let cutoff = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let tpl = vec![
            r(2020, 22.70, None),          // oldest in window: its own prior is outside it
            r(2021, 34.83, Some(22.70)),   // +53.4%
            r(2022, 57.77, Some(34.83)),   // +65.9%
            r(2023, 52.77, Some(57.77)),   //  -8.7%
            r(2024, 19.72, Some(17.59)),   // +12.1%  <- comparative restated /3
            r(2025, 6.97, Some(6.573)),    //  +6.1%  <- restated /3 again
        ];
        let chained = fund_factors(&tpl, cutoff, 5).eps_growth.expect("chain complete");
        assert!((chained - 22.6).abs() < 0.1, "{chained}");
        // Same rows without comparatives -> the endpoint fallback, which is the -21.0%/yr lie. Pinned
        // so the two paths can never be confused for each other.
        let no_prior: Vec<FundRow> = tpl.iter().map(|x| FundRow { prior_eps: None, ..x.clone() }).collect();
        let endpoint = fund_factors(&no_prior, cutoff, 5).eps_growth.expect("no shares -> nothing to guard");
        assert!((endpoint + 21.0).abs() < 0.1, "{endpoint}");
        // One broken link (a loss year) drops the whole chain to that fallback rather than compounding
        // a partial window — 4 steps annualized over 5 years would understate growth by construction.
        let mut gap = tpl.clone();
        gap[3] = FundRow { eps: Some(-1.0), ..gap[3].clone() };
        assert!(fund_factors(&gap, cutoff, 5).eps_growth.unwrap() < 0.0);
        // buyback_yield reads the same comparative: shares 100 -> 300 across a 3:1 whose prior was
        // restated to 297 is a 1% BUYBACK, not a 200% issuance.
        let split_row = vec![FundRow {
            filed: NaiveDate::from_ymd_opt(2026, 2, 20).unwrap(),
            period_end: NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
            shares: Some(300.0),
            prior_shares: Some(303.0),
            ..Default::default()
        }];
        let bb = fund_factors(&split_row, cutoff, 5).buyback_yield.expect("comparative present");
        assert!((bb - 0.99).abs() < 0.01, "{bb}");
    }

    /// The three PRINTED columns as probeable factors: `rev_yoy` (the fast leg `rev_accel` used to
    /// swallow), `eps_yoy` (off the row's OWN comparative, so a split can't fake it — the whole point
    /// of the same-filing work) and `net_margin` (a LEVEL that had never been carried at all; only its
    /// above-the-line twin `op_margin` was ever measured). All three are 1-row or 2-row reads, so they
    /// populate where the `yrs`-lookback factors beside them are still None.
    #[test]
    fn printed_columns_are_probeable_factors() {
        let row = |y: i32, revenue: f64, eps: f64, prior_eps: Option<f64>, nm: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(y + 1, 2, 20).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y, 12, 31).unwrap(),
            revenue: Some(revenue),
            eps: Some(eps),
            prior_eps,
            net_margin: Some(nm),
            ..Default::default()
        };
        let cutoff = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        // TPL's real last two years: revenue 705.8M -> 798.2M, EPS 6.97 against a comparative of 6.573
        // that the SAME filing restated after the second 3-for-1. The row BELOW it still says 19.72.
        let rows = vec![row(2024, 705.8, 19.72, Some(17.59), 64.3), row(2025, 798.2, 6.97, Some(6.573), 60.3)];
        let f = fund_factors(&rows, cutoff, 5);
        assert!((f.rev_yoy.unwrap() - 13.09).abs() < 0.01, "{:?}", f.rev_yoy);
        assert!((f.eps_yoy.unwrap() - 6.04).abs() < 0.01, "cross-filing would read -64.7%: {:?}", f.eps_yoy);
        assert_eq!(f.net_margin, Some(60.3));
        // the 5y factors beside them are still None on this 2-row history — that asymmetry is why the
        // printed columns get their own probe rows instead of being read off rev_cagr/eps_growth
        assert_eq!(f.rev_cagr, None);
        assert_eq!(f.eps_growth, None);
        // every one of them is reachable by NAME, which is what `growth_fund_extra` selects on
        assert_eq!(select_fund_factor(&f, "rev_yoy"), f.rev_yoy);
        assert_eq!(select_fund_factor(&f, "eps_yoy"), f.eps_yoy);
        assert_eq!(select_fund_factor(&f, "net_margin"), Some(60.3));
        // net_margin is NOT op_margin: below-the-line items live only in the former, and op_margin is
        // the one that measured rho -0.23 — reading them as interchangeable is the mistake to prevent
        assert_eq!(f.op_margin, None, "these rows carry no op_margin");
        // no comparative -> the guarded cross-filing fallback, which blanks a split-sized share jump
        let no_prior: Vec<FundRow> = rows
            .iter()
            .map(|r| FundRow { prior_eps: None, shares: Some(if r.period_end.year() == 2025 { 69.0 } else { 23.0 }), ..r.clone() })
            .collect();
        assert_eq!(fund_factors(&no_prior, cutoff, 5).eps_yoy, None, "+200% shares -> split guard blanks it");
    }

    /// (V) `eps_never_reported` is a FILER-level fact, read off the whole `rows` slice — deliberately NOT
    /// through `fund_as_of` like every other field here. THE ANTI-SKEW ASSERT: an as-of version would say
    /// "true" at an early cutoff and "false" at a late one for the same name, which gates it in the
    /// backtest and passes it live. Both callers hand `fund_factors` the identical full series, so the
    /// only way the two lanes can agree is for this to ignore the cutoff entirely.
    #[test]
    fn eps_never_reported_is_filer_level_not_as_of() {
        let r = |y: i32, eps: Option<f64>| FundRow {
            filed: NaiveDate::from_ymd_opt(y + 1, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y, 12, 31).unwrap(),
            revenue: Some(100.0),
            eps,
            ..Default::default()
        };
        let early = NaiveDate::from_ymd_opt(2021, 6, 1).unwrap();
        let late = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        // Visa's shape after the instance fallback: EPS only on the newest three years, because ONE 10-K
        // is what the fallback reads. At `early` not a single EPS row is filed yet.
        let partial = vec![r(2019, None), r(2020, None), r(2023, Some(8.28)), r(2024, Some(9.73)), r(2025, Some(10.20))];
        assert!(!fund_factors(&partial, early, 5).eps_never_reported, "an as-of read would say TRUE here — that is the skew");
        assert!(!fund_factors(&partial, late, 5).eps_never_reported);
        // ARES: no per-share element in any filing, at any cutoff. THIS is what the gate cuts.
        let none_ever = vec![r(2023, None), r(2024, None), r(2025, None)];
        assert!(fund_factors(&none_ever, early, 5).eps_never_reported);
        assert!(fund_factors(&none_ever, late, 5).eps_never_reported);
        // no rows at all is NO COVERAGE, not "never reports" — every ETF, every coin, every uncovered
        // filer lands here, and gating them would turn a data gap into a verdict.
        assert!(!fund_factors(&[], late, 5).eps_never_reported, "an empty series must never read as a gate-able fact");
    }

    /// (round 109) `margin_stability` = negated sample stddev of net_margin over the as-of rows:
    /// 10/20/30 -> mean 20, sample variance 100, std 10 -> factor −10. Fewer than 3 values -> None
    /// (2 points define a line, not a dispersion), and rows filed after the cutoff never leak in.
    #[test]
    fn margin_stability_stddev() {
        let r = |y: i32, nm: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(y, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y - 1, 12, 31).unwrap(),
            net_margin: Some(nm),
            ..Default::default()
        };
        let rows = vec![r(2022, 10.0), r(2023, 20.0), r(2024, 30.0)];
        let cutoff = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap();
        let f = fund_factors(&rows, cutoff, 5);
        assert!((f.margin_stability.unwrap() + 10.0).abs() < 1e-9);
        // only 2 net_margin values -> None
        assert_eq!(fund_factors(&rows[..2], cutoff, 5).margin_stability, None);
        // as-of guard: cutoff before the 2024 filing leaves 2 rows -> None, no look-ahead
        assert_eq!(fund_factors(&rows, NaiveDate::from_ymd_opt(2023, 6, 1).unwrap(), 5).margin_stability, None);
    }

    /// (P2) `accrual_gap` = −(eps − fcf/share) / max(|eps|, 0.5), with fcf/share derived from the row's
    /// own fcf_margin, revenue and diluted share count. Zero when the profit is exactly cash-backed,
    /// negative when earnings run ahead of cash, POSITIVE when cash runs ahead of earnings — the sign
    /// is what makes it usable as a high-is-safer factor next to the round-107 levels.
    ///
    /// The floor is pinned too, because without it a filer earning a rounding error a share reports an
    /// enormous gap off an economically meaningless denominator.
    #[test]
    fn accrual_gap_is_negated_and_floored() {
        let r = |revenue: f64, fcf_margin: f64, eps: f64, shares: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(2023, 12, 31).unwrap(),
            revenue: Some(revenue),
            fcf_margin: Some(fcf_margin),
            eps: Some(eps),
            shares: Some(shares),
            ..Default::default()
        };
        let cutoff = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap();
        let gap = |row: FundRow| fund_factors(&[row], cutoff, 5).accrual_gap;

        // 1000 revenue at a 30% FCF margin = 300 of cash over 100 shares = 3.00/share, against 3.00 of
        // EPS: every cent of the reported profit arrived as cash.
        assert!(gap(r(1000.0, 30.0, 3.0, 100.0)).unwrap().abs() < 1e-9);
        // same earnings, a tenth of the cash -> (3.00 − 0.50)/3.00 = 0.833 of the profit is an accrual
        assert!((gap(r(1000.0, 5.0, 3.0, 100.0)).unwrap() + 0.8333333333).abs() < 1e-9);
        // cash AHEAD of earnings reads positive — a depreciating-asset filer is not penalised for it
        assert!((gap(r(1000.0, 30.0, 1.0, 100.0)).unwrap() - 2.0).abs() < 1e-9);
        // the floor: 0.10 of EPS and no cash is −0.2 (denominator 0.5), NOT the −1.0 the raw ratio gives
        assert!((gap(r(1000.0, 0.0, 0.1, 100.0)).unwrap() + 0.2).abs() < 1e-9);

        // any missing leg is None, never a fabricated 0 — the FMP free tier leaves fcf_margin empty,
        // and a 0 there would read as "perfectly cash-backed" on every name it covers
        assert_eq!(gap(FundRow { fcf_margin: None, ..r(1000.0, 30.0, 3.0, 100.0) }), None);
        assert_eq!(gap(FundRow { eps: None, ..r(1000.0, 30.0, 3.0, 100.0) }), None);
        assert_eq!(gap(FundRow { shares: None, ..r(1000.0, 30.0, 3.0, 100.0) }), None);
        assert_eq!(gap(FundRow { revenue: None, ..r(1000.0, 30.0, 3.0, 100.0) }), None);
        assert_eq!(gap(FundRow { shares: Some(0.0), ..r(1000.0, 30.0, 3.0, 100.0) }), None);

        // as-of guard: a cutoff before the filing date must not see the row at all
        assert_eq!(
            fund_factors(&[r(1000.0, 5.0, 3.0, 100.0)], NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), 5).accrual_gap,
            None
        );
    }

    /// (P3) `asset_growth` is `rev_cagr`'s annualiser and positivity guard pointed at total assets, then
    /// NEGATED. A balance sheet that doubled over 5 years must rank BELOW a flat one, which is the whole
    /// reason the sign is flipped — every other factor in the struct reads high = safer and a raw growth
    /// rate would silently invert against them in `composite` and in any reject-the-bottom probe.
    #[test]
    fn asset_growth_is_negated_and_shares_rev_cagr_s_guards() {
        let r = |y: i32, assets: Option<f64>, revenue: f64| FundRow {
            filed: NaiveDate::from_ymd_opt(y, 2, 1).unwrap(),
            period_end: NaiveDate::from_ymd_opt(y - 1, 12, 31).unwrap(),
            assets,
            revenue: Some(revenue),
            ..Default::default()
        };
        let cutoff = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap();
        // assets 1000 -> 2000 over the 5y lookback, revenue flat: the expansion is invisible to rev_cagr
        let rows = vec![r(2019, Some(1000.0), 500.0), r(2024, Some(2000.0), 500.0)];
        let f = fund_factors(&rows, cutoff, 5);
        let doubled = f.asset_growth.unwrap();
        assert!((doubled + cagr(100.0, 5.0)).abs() < 1e-9, "must be the NEGATED 5y CAGR, got {doubled}");
        assert!(doubled < 0.0, "a doubling balance sheet must rank low, not high");
        assert_eq!(f.rev_cagr, Some(0.0), "flat revenue — the term that already exists cannot see this");

        // a SHRINKING asset base ranks high: the sign flip has to work in both directions
        let shrunk = vec![r(2019, Some(2000.0), 500.0), r(2024, Some(1000.0), 500.0)];
        assert!(fund_factors(&shrunk, cutoff, 5).asset_growth.unwrap() > 0.0);

        // same `grow` guard as rev_cagr: a missing or non-positive base is None, never a garbage ratio
        assert_eq!(fund_factors(&[r(2019, None, 500.0), r(2024, Some(2000.0), 500.0)], cutoff, 5).asset_growth, None);
        assert_eq!(fund_factors(&[r(2019, Some(0.0), 500.0), r(2024, Some(2000.0), 500.0)], cutoff, 5).asset_growth, None);
        // and the look-ahead guard: with no row old enough to anchor the lookback there is no growth
        assert_eq!(fund_factors(&[r(2024, Some(2000.0), 500.0)], cutoff, 5).asset_growth, None);
    }

    /// `income_snapshot`: picks the newest COMPLETE year (1 = annual filing, 4+ = full quarterly year;
    /// 2-3 = partial, skipped), YoY vs the next-older row with report's exact math, and refuses to
    /// fabricate a number: no older row / zero prior EPS -> that component None; no complete year -> None.
    #[test]
    fn income_snapshot_complete_year_and_yoy() {
        let a = |year: i32, revenue: f64, eps: Option<f64>, quarters: usize| AnnualReport {
            year, revenue, gross_margin: None, op_margin: None, net_margin: Some(50.0), eps, shares: None,
            prior_eps: None, prior_shares: None, quarters, // no comparatives -> exercises the cross-filing FALLBACK
        };
        // shares-carrying variant for the buyback leg
        let s = |year: i32, revenue: f64, eps: Option<f64>, shares: Option<f64>, quarters: usize| AnnualReport {
            year, revenue, gross_margin: None, op_margin: None, net_margin: Some(50.0), eps, shares,
            prior_eps: None, prior_shares: None, quarters,
        };
        // newest year partial (3 quarters) -> skipped; snapshot = 2023 vs 2022
        let rows = vec![a(2024, 900.0, Some(9.0), 3), a(2023, 600.0, Some(6.0), 4), a(2022, 500.0, Some(4.0), 4)];
        let (rev, eps, net, bb) = income_snapshot(&rows).unwrap();
        assert!((rev.unwrap() - 20.0).abs() < 1e-9); // 600/500
        assert!((eps.unwrap() - 50.0).abs() < 1e-9); // 6/4
        assert_eq!(net, Some(50.0));
        assert_eq!(bb, None); // no shares on these rows
        // SEC-style annual filing (quarters == 1) counts as complete
        let sec = vec![a(2023, 600.0, Some(6.0), 1), a(2022, 500.0, Some(6.0), 1)];
        assert!((income_snapshot(&sec).unwrap().0.unwrap() - 20.0).abs() < 1e-9);
        // buyback: shares shrank 100->95 -> −(−5%) = +5% (buying back); split-size jump -> None
        let buy = vec![s(2023, 600.0, Some(6.0), Some(95.0), 1), s(2022, 500.0, Some(6.0), Some(100.0), 1)];
        assert!((income_snapshot(&buy).unwrap().3.unwrap() - 5.0).abs() < 1e-9);
        let split = vec![s(2023, 600.0, Some(6.0), Some(200.0), 1), s(2022, 500.0, Some(6.0), Some(100.0), 1)];
        assert_eq!(income_snapshot(&split).unwrap().3, None); // +100% shares = split, not dilution signal
        // ...and the SAME jump now kills eps_yoy, which reads off the same un-split-adjusted rows.
        // MNST-shaped: 2:1 in early 2024, as-reported EPS 2.64 -> 1.49, which is -43.6% of pure artifact.
        let mnst = vec![s(2024, 700.0, Some(1.49), Some(200.0), 1), s(2023, 600.0, Some(2.64), Some(100.0), 1)];
        let snap = income_snapshot(&mnst).unwrap();
        assert_eq!(snap.1, None, "eps_yoy across a split is an artifact, not growth");
        assert_eq!(snap.3, None);
        assert!(snap.0.is_some(), "revenue has no share denominator -> a split can't touch it");
        // Below the 40% line it is ordinary dilution and BOTH legs survive: shares 100 -> 130 (+30%).
        let dilute = vec![s(2024, 700.0, Some(6.6), Some(130.0), 1), s(2023, 600.0, Some(6.0), Some(100.0), 1)];
        let d = income_snapshot(&dilute).unwrap();
        assert!((d.1.unwrap() - 10.0).abs() < 1e-9); // 6.6/6.0
        assert!((d.3.unwrap() + 30.0).abs() < 1e-9); // −(+30%) = issuance, not a buyback
        // No share data at all -> nothing to judge -> KEEP eps_yoy (absence of a signal isn't a verdict)
        let noshares = vec![a(2024, 700.0, Some(6.6), 1), a(2023, 600.0, Some(6.0), 1)];
        assert!((income_snapshot(&noshares).unwrap().1.unwrap() - 10.0).abs() < 1e-9);
        // SAME-FILING COMPARATIVES win over both the cross-filing read AND its guard. TPL FY2025, real
        // numbers: the row below it in the cache is 19.72 EPS / 23.02M shares (FY2024 as the FY2024 10-K
        // stated it, pre the second 3-for-1). Cross-filing that reads -64.7% EPS / +199.9% shares, which
        // the 40% rule then blanks -- the `n/a`s this work is about. The FY2025 filing's OWN comparatives
        // are 6.573 and 69,059,252, and they give the truth: +6.0% EPS, a 0.05% buyback.
        let tpl = vec![
            AnnualReport {
                year: 2025, revenue: 700.0, gross_margin: None, op_margin: None, net_margin: Some(50.0),
                eps: Some(6.97), shares: Some(69_027_492.0),
                prior_eps: Some(6.573), prior_shares: Some(69_059_252.0), quarters: 1,
            },
            s(2024, 600.0, Some(19.72), Some(23_019_751.0), 1),
        ];
        let t = income_snapshot(&tpl).unwrap();
        assert!((t.1.unwrap() - 6.04).abs() < 0.01, "eps_yoy {:?}", t.1);
        assert!((t.3.unwrap() - 0.046).abs() < 0.01, "buyback {:?}", t.3);
        // ...and dilution is NOT rescued by the same field -- COF-shaped, comparatives not restated.
        let cof = vec![
            AnnualReport {
                year: 2025, revenue: 700.0, gross_margin: None, op_margin: None, net_margin: Some(50.0),
                eps: Some(4.03), shares: Some(500.0), prior_eps: Some(11.59), prior_shares: Some(380.0),
                quarters: 1,
            },
            s(2024, 600.0, Some(11.59), Some(380.0), 1),
        ];
        let c = income_snapshot(&cof).unwrap();
        assert!((c.1.unwrap() + 65.23).abs() < 0.01, "real EPS collapse, not an artifact: {:?}", c.1);
        assert!((c.3.unwrap() + 31.58).abs() < 0.01, "issuance prints negative, no 40% blanking: {:?}", c.3);
        // oldest year in the data: nothing older to compare -> YoY components None, margin still real
        let lone = vec![a(2023, 600.0, Some(6.0), 4)];
        assert_eq!(income_snapshot(&lone).unwrap(), (None, None, Some(50.0), None));
        // zero prior EPS -> eps_yoy None (never a divide blow-up); prior zero revenue -> rev_yoy None
        let zeroes = vec![a(2023, 600.0, Some(6.0), 4), a(2022, 0.0, Some(0.0), 4)];
        assert_eq!(income_snapshot(&zeroes).unwrap(), (None, None, Some(50.0), None));
        // only partial years -> no snapshot at all
        assert_eq!(income_snapshot(&[a(2024, 900.0, None, 2)]), None);
        assert_eq!(income_snapshot(&[]), None);
    }

    /// (B) `annual_brief`: newest ≤5 complete years, partial years dropped, oldest→newest chain with
    /// rev/EPS CAGR; <2 complete years -> None; loss-year EPS endpoint -> EPS leg omitted, never a
    /// nonsense negative-ratio CAGR; EPS leg needs a verifiable share chain (present + no split jump).
    #[test]
    fn annual_brief_trajectory() {
        let y = |year: i32, revenue: f64, nm: Option<f64>, eps: Option<f64>, quarters: usize| AnnualReport {
            year, revenue, gross_margin: None, op_margin: None, net_margin: nm, eps, shares: Some(16.0e9),
            prior_eps: None, prior_shares: None, quarters,
        };
        // newest-first like annual_rollup; 2024 partial (2 quarters) must be dropped from the chain
        let rows = vec![
            y(2024, 100e9, Some(30.0), Some(9.9), 2),
            y(2023, 391e9, Some(25.0), Some(6.1), 1),
            y(2022, 383e9, Some(25.0), Some(6.1), 4),
            y(2021, 394e9, Some(25.0), Some(6.1), 1),
            y(2020, 366e9, Some(21.0), Some(3.3), 1),
            y(2019, 274e9, Some(21.0), Some(3.0), 1),
        ];
        let b = annual_brief(&rows).unwrap();
        assert!(b.starts_with("rev 5y 274.0B→366.0B→394.0B→383.0B→391.0B"), "{b}");
        assert!(b.contains("(+9%/yr)"), "{b}"); // (391/274)^(1/4)−1
        assert!(b.contains("net 21%→25%"), "{b}");
        assert!(b.contains("eps +19%/yr"), "{b}"); // (6.1/3.0)^(1/4)−1
        // one complete year -> no trajectory; loss endpoint -> EPS leg omitted, rev leg stays
        assert_eq!(annual_brief(&rows[..2]), None); // 2024 partial + 2023 = only 1 complete
        let loss = vec![y(2023, 600.0, None, Some(2.0), 1), y(2022, 500.0, None, Some(-1.0), 1)];
        let lb = annual_brief(&loss).unwrap();
        assert!(!lb.contains("eps"), "{lb}");
        assert!(lb.contains("rev 2y 500→600 (+20%/yr)"), "{lb}");
        // a >40% share-count jump (split) makes as-reported EPS CAGR a lie -> leg omitted
        let ys = |year: i32, eps: f64, shares: f64| AnnualReport {
            year, revenue: 500.0, gross_margin: None, op_margin: None, net_margin: None,
            eps: Some(eps), shares: Some(shares), prior_eps: None, prior_shares: None, quarters: 1,
        };
        let split = vec![ys(2023, 2.0, 1000.0), ys(2022, 15.0, 100.0)];
        assert!(!annual_brief(&split).unwrap().contains("eps"));
        let nosplit = vec![ys(2023, 18.0, 102.0), ys(2022, 15.0, 100.0)];
        assert!(annual_brief(&nosplit).unwrap().contains("eps +20%/yr"));
        // (GOOG shape) a missing share count anywhere in the window = UNVERIFIABLE split history ->
        // leg omitted even with profitable endpoints (Alphabet's pre-2022 counts are per-class only,
        // so the 2022 20:1 split was invisible and eps -44%/yr printed against rev +12%/yr).
        let noshares = AnnualReport {
            year: 2021, revenue: 257.6e9, gross_margin: None, op_margin: None, net_margin: None,
            eps: Some(112.2), shares: None, prior_eps: None, prior_shares: None, quarters: 1,
        };
        let unverifiable = vec![ys(2025, 10.81, 12.2e9), noshares];
        let ub = annual_brief(&unverifiable).unwrap();
        assert!(!ub.contains("eps"), "{ub}");
    }

    /// (Item 4) `insider_net_buys` counts P(+1)/S(−1) only in [cutoff−window, cutoff): a same-day or later
    /// filing is excluded (look-ahead guard), and an empty window -> None (no coverage, never a fake 0).
    #[test]
    fn insider_net_buys_windows_and_guards() {
        let d = |m, day| NaiveDate::from_ymd_opt(2020, m, day).unwrap();
        let txns = vec![
            InsiderTx { date: d(1, 10), buy: true },  // in window for a Mar cutoff
            InsiderTx { date: d(2, 15), buy: true },  // in window
            InsiderTx { date: d(2, 20), buy: false }, // in window (a sale, −1)
            InsiderTx { date: d(3, 1), buy: true },   // ON the cutoff -> excluded (look-ahead)
        ];
        let cutoff = d(3, 1);
        assert_eq!(insider_net_buys(&txns, cutoff, 90), Some(1.0)); // +1 +1 −1 = +1; the d(3,1) buy excluded
        assert_eq!(insider_net_buys(&txns, cutoff, 5), None); // nothing in the 5d before -> no coverage
        assert_eq!(insider_net_buys(&[], cutoff, 90), None); // no data -> None
    }

    /// Pure-logic asserts (no network). White-box: reaches `core` privates via `use super::*`.
    /// (PIT) The membership parser and its HALF-OPEN boundary. The boundary is the entire correctness
    /// claim of the small source file — `sp500_spans`' doc records that reading `end` as INCLUSIVE
    /// mis-rebuilds 604 of the publisher's 2718 snapshots — and that measurement lives in a comment,
    /// which nothing enforces. This is the executable half. The malformed rows are here for the same
    /// reason: the dangerous parse failure is not a dropped row, it is a garbled `end_date` quietly
    /// becoming `None` and readmitting a dead ticker to today's index.
    #[test]
    fn sp500_spans_parse_and_the_half_open_boundary() {
        let d = |s: &str| s.parse::<NaiveDate>().expect("date");
        let csv = "ticker,start_date,end_date\n\
                   AAPL,1996-01-02,\n\
                   BF.B,1996-01-02,\n\
                   AAL,1996-01-02,2013-12-09\n\
                   AAL,2015-03-23,\n\
                   SBNY,2015-08-05,2023-03-15\n\
                   ,2000-01-01,\n\
                   JUNK,not-a-date,\n\
                   BAD,2000-01-01,also-not-a-date\n";
        let spans = sp500_spans(csv);

        // Yahoo form on the way IN, so the caller looks a name up with the same string it fetches.
        assert!(spans.contains_key("BF-B"), "class shares are normalised: BF.B -> BF-B");
        assert!(!spans.contains_key("BF.B"));
        // Three junk rows, three different failures, none of them admitted.
        assert!(!spans.contains_key(""), "an empty symbol is not a member of anything");
        assert!(!spans.contains_key("JUNK"), "an unparsable start date drops the row");
        assert!(
            !spans.contains_key("BAD"),
            "a garbled END date drops the row — it must NEVER fall through to None, which would \
             promote a delisted name to a current constituent and reintroduce survivorship in the parser"
        );
        assert_eq!(spans.len(), 4, "AAPL, BF-B, AAL, SBNY — and nothing else");
        assert_eq!(spans["AAL"].len(), 2, "a name that left and came back keeps BOTH spans, not a merged one");

        let aal = &spans["AAL"];
        // THE BOUNDARY, both sides of both ends of the first span.
        assert!(sp500_member_at(aal, d("1996-01-02")), "the start date itself counts");
        assert!(!sp500_member_at(aal, d("1996-01-01")), "the day before does not");
        assert!(sp500_member_at(aal, d("2013-12-08")), "the day before the end still counts");
        assert!(!sp500_member_at(aal, d("2013-12-09")), "the END DATE DOES NOT — half-open, per the 2718-snapshot check");
        // and the gap between the two spans is genuinely out of the index: this is the whole point of
        // keeping a Vec, and a merged first_start..last_end span would answer `true` here.
        assert!(!sp500_member_at(aal, d("2014-06-01")), "the years it was OUT are out");
        assert!(sp500_member_at(aal, d("2015-03-23")), "…and it is back on its re-entry date");

        // An open span has no right edge: still a member at any date at or after the start.
        assert!(sp500_member_at(&spans["AAPL"], d("2026-08-18")));
        assert!(!sp500_member_at(&spans["AAPL"], d("1995-12-31")));
        // A closed one, well past its end, is not — the dead-ticker case the whole feature exists for.
        assert!(!sp500_member_at(&spans["SBNY"], d("2026-08-18")));
        assert!(sp500_member_at(&spans["SBNY"], d("2020-01-02")));

        // No header, no rows, no panic: an unrecognizable document is "no PIT data", never "nobody
        // was ever a member" — the caller must be able to tell those apart by the map being EMPTY.
        assert!(sp500_spans("").is_empty());
        assert!(sp500_spans("<html>404</html>").is_empty());
    }

    /// (#173) The merge is what lets a SECOND index join the point-in-time pool. `pit_pool` builds
    /// its pool from `spans.keys()`, so a name the extra source alone carries arrives in the pool by
    /// being in this map at all — and the empty-map case is the knob's off switch, which every
    /// golden depends on.
    #[test]
    fn merge_spans_unions_two_membership_sources() {
        let d = |s: &str| s.parse::<NaiveDate>().expect("date");
        let base = sp500_spans("ticker,start_date,end_date\nAAPL,1996-01-02,\nEK,1996-01-02,2011-01-27\n");
        let extra = sp500_spans("ticker,start_date,end_date\nLANC,2012-12-01,\nEK,1990-01-01,1995-01-01\n");
        let merged = merge_spans(base.clone(), extra);
        // a name ONLY the second source carries joins the pool — the entire point of the knob
        assert!(merged.contains_key("LANC"), "the extra source's names must reach the pool");
        // a name in BOTH keeps both spans, ordered, rather than one source overwriting the other
        assert_eq!(
            merged["EK"],
            vec![(d("1990-01-01"), Some(d("1995-01-01"))), (d("1996-01-02"), Some(d("2011-01-27")))]
        );
        assert_eq!(merged["AAPL"], base["AAPL"], "the base source is untouched");
        // OFF SWITCH: merging nothing returns the base exactly (non-negotiable #1)
        assert_eq!(merge_spans(base.clone(), MemberSpans::new()), base);
        // and it is symmetric in the sense that matters: no span is lost either way round
        assert_eq!(merge_spans(MemberSpans::new(), base.clone()), base);
    }

    #[test]
    fn pure_logic() {
    assert!((pct_from_high(&[100.0, 80.0, 95.0]) - 5.0).abs() < 1e-9);
    assert_eq!(pct_from_high(&[90.0, 100.0]), 0.0);
    // backtest correlation helpers: perfect monotone -> +1, reversed -> -1, robust ranks
    assert!((pearson(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]).unwrap() - 1.0).abs() < 1e-9);
    assert!((pearson(&[1.0, 2.0, 3.0], &[6.0, 4.0, 2.0]).unwrap() + 1.0).abs() < 1e-9);
    assert!(pearson(&[1.0, 1.0], &[1.0, 1.0]).is_none()); // zero variance
    assert!((spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 30.0, 1000.0, 99999.0]).unwrap() - 1.0).abs() < 1e-9); // monotone order -> +1, outlier magnitude ignored
    assert!((spearman(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]).unwrap() + 1.0).abs() < 1e-9);
    assert_eq!(ranks(&[10.0, 30.0, 20.0]), vec![1.0, 3.0, 2.0]);
    assert_eq!(ranks(&[5.0, 5.0, 9.0]), vec![1.5, 1.5, 3.0]); // ties share the average rank
    assert_eq!(market_of("VWCE.DE"), "Germany");
    assert_eq!(market_of("AAPL"), "USA");
    assert_eq!(market_of("BTC-USD"), "Crypto");
    assert_eq!(market_of("BRK-B"), "USA"); // share-class dash is not a coin (same trap as report's r73 fix)
    // nupl_zone: band edges
    assert_eq!(nupl_zone(-0.1), "Capitulation");
    assert_eq!(nupl_zone(0.16), "Hope/Fear");
    assert_eq!(nupl_zone(0.6), "Belief/Denial");
    assert_eq!(nupl_zone(0.8), "Euphoria/Greed");
    // sector_matches: empty filter keeps all; else case-insensitive substring on ANY keyword
    let tech = vec!["Technology".to_string(), "Communication".to_string()];
    assert!(sector_matches("Industrials", &[])); // no filter -> keep everything
    assert!(sector_matches("Information Technology", &tech)); // "Technology" is a substring
    assert!(sector_matches("iShares Tech Sector Technology UCITS", &tech)); // ETF name path
    assert!(!sector_matches("Industrials", &tech));
    // sector_symbol: keep only filter-matching sectors (Yahoo-normalized), all when filter empty
    assert_eq!(
        sector_symbol("AAPL,Apple Inc.,Information Technology,Tech HW,x,y", &tech),
        Some(("AAPL".to_string(), "Information Technology".to_string()))
    );
    assert_eq!(sector_symbol("GOOGL,Alphabet,Communication Services,x", &tech).map(|(s, _)| s), Some("GOOGL".to_string()));
    assert_eq!(sector_symbol("BF.B,Brown-Forman,Information Technology,x", &tech).map(|(s, _)| s), Some("BF-B".to_string())); // '.'->'-'
    // European venue suffixes are already Yahoo form — the class-share dash rewrite must NOT touch them
    assert_eq!(sector_symbol("AAF.L,Airtel Africa PLC", &[]).map(|(s, _)| s), Some("AAF.L".to_string()));
    assert_eq!(sector_symbol("ADS.DE,adidas AG", &[]).map(|(s, _)| s), Some("ADS.DE".to_string()));
    assert_eq!(sector_symbol("MMM,3M,Industrials,x", &tech), None);
    assert_eq!(sector_symbol("AMZN,Amazon,Consumer Discretionary,x", &tech), None); // GICS quirk: not tech
    assert_eq!(sector_symbol("MMM,3M,Industrials,x", &[]).map(|(s, _)| s), Some("MMM".to_string())); // empty filter -> all sectors
    // quoted comma in the Security NAME shifts the sector one column right — must still read the sector
    assert_eq!(
        sector_symbol("CASY,\"Casey's General Stores, Inc.\",Consumer Staples,x", &[]),
        Some(("CASY".to_string(), "Consumer Staples".to_string()))
    );
    // (Item 32) a 2-column list (Symbol,Name) has no sector -> kept under "other" (not dropped),
    // but a sector-restricted filter still excludes it
    assert_eq!(sector_symbol("PDD,PDD Holdings", &[]), Some(("PDD".to_string(), "other".to_string())));
    assert_eq!(sector_symbol("PDD,PDD Holdings", &tech), None);
    // (Item 32) wiki_constituents: real shape of the Wikipedia constituents table — symbol anchor in
    // cell 0, sector text in cell 2; header row skipped; no table id -> empty (pond drops, no crash)
    let wiki = r#"<table class="wikitable sortable" id="constituents">
<tbody><tr><th>Symbol</th><th>Security</th><th>GICS Sector</th></tr>
<tr><td style="x"><a href="y" data-mw='{"params":{"1":{"wt":"AA"}}}'>AA</a></td>
<td><a href="//w/Alcoa">Alcoa</a></td><td>Materials</td><td>Aluminum</td></tr>
<tr><td><a>BRK.B</a></td><td><a>Berkshire</a></td><td>Financials</td><td>x</td></tr></tbody></table>"#;
    assert_eq!(
        wiki_constituents(wiki, &[]),
        vec![("AA".to_string(), "Materials".to_string()), ("BRK-B".to_string(), "Financials".to_string())]
    );
    assert_eq!(wiki_constituents(wiki, &["Financials".to_string()]).len(), 1); // sector filter applies
    assert!(wiki_constituents("<html>no table here</html>", &[]).is_empty());
    // euronext_lisbon_symbols: symbol at row index 2 -> `<SYM>.LS`; odd/empty cells dropped; no aaData -> []
    let es = serde_json::json!({"aaData": [
        ["<a href=x>GALP ENERGIA</a>", "PTGAL0AM0009", "GALP", "<div>XLIS</div>", "EUR 18", "0.1%", "12:00"],
        ["<a>EDP</a>", "PTEDP0AM0009", "EDP", "<div>XLIS</div>", "EUR 3", "-0.2%", "12:00"],
        ["bad", "isin", "", "x", "y", "z", "w"],   // empty symbol -> dropped
        ["bad", "isin", "FOO BAR", "x", "y", "z", "w"], // non-alnum symbol -> dropped
    ]});
    assert_eq!(euronext_lisbon_symbols(&es), vec!["GALP.LS".to_string(), "EDP.LS".to_string()]);
    assert!(euronext_lisbon_symbols(&serde_json::json!({})).is_empty()); // no aaData -> empty leg, not a crash
    // euronext_track_isins: ISIN at row index 1; non-ISIN-shaped cells dropped; no aaData -> []
    let et = serde_json::json!({"aaData": [
        ["<a data-title-hover=\"iShares X\">X</a>", "IE0007G78AC4", "ASIG", "<div>XAMC</div>"],
        ["<a>Y</a>", "LU2870272650", "DXUSH", "<div>ETFP</div>"],
        ["bad", "not-an-isin", "Z", "x"],       // wrong shape -> dropped
        ["bad", "ie0007g78ac4", "Z", "x"],      // lowercase prefix -> dropped
    ]});
    assert_eq!(euronext_track_isins(&et), vec!["IE0007G78AC4".to_string(), "LU2870272650".to_string()]);
    assert!(euronext_track_isins(&serde_json::json!({})).is_empty()); // no aaData -> empty leg, not a crash
    // six_fund_isins: [ISIN, ShortName] rows; ETF/UCITS-named kept, mutual funds + bad ISINs dropped
    let six = serde_json::json!({"rowData": [
        ["IE00B5BMR087", "iShares Core S&P 500 ETF"],
        ["LU2870272650", "D-X MSCI USA Screened UCITS"],
        ["AT0000A255C8", "LGT PB Conservative USD R"],  // mutual fund -> dropped
        ["not-an-isin", "Some ETF"],                     // bad ISIN -> dropped
    ]});
    assert_eq!(six_fund_isins(&six), vec!["IE00B5BMR087".to_string(), "LU2870272650".to_string()]);
    assert!(six_fund_isins(&serde_json::json!({})).is_empty()); // no rowData -> empty leg, not a crash
    // firds_latest_fulins_link: both registry shapes, newest FULINS_C wins, non-C files ignored
    let esma = serde_json::json!({"response": {"docs": [
        {"file_name": "FULINS_C_20260627_01of01.zip", "download_link": "https://x/old.zip"},
        {"file_name": "FULINS_C_20260704_01of01.zip", "download_link": "https://x/new.zip"},
        {"file_name": "FULINS_D_20260711_01of01.zip", "download_link": "https://x/wrong-class.zip"},
    ]}});
    assert_eq!(firds_latest_fulins_link(&esma).as_deref(), Some("https://x/new.zip"));
    let fca = serde_json::json!({"hits": {"hits": [
        {"_source": {"file_name": "FULINS_C_20260606_01of01.zip", "download_link": "https://y/old.zip"}},
        {"_source": {"file_name": "FULINS_C_20260620_01of01.zip", "download_link": "https://y/new.zip"}},
    ]}});
    assert_eq!(firds_latest_fulins_link(&fca).as_deref(), Some("https://y/new.zip"));
    assert!(firds_latest_fulins_link(&serde_json::json!({})).is_none()); // reshaped -> None, not a crash
    // firds_etf_isins: CFI CE* + ETF/UCITS name + EU domicile kept; mutual funds (CI*), US funds,
    // non-ETF names dropped; single-line (ESMA) and pretty-printed (FCA) records both parse
    let xml = "<FinInstrmGnlAttrbts><Id>IE000HN2PIB9</Id><FullNm>AXA IM Nasdaq 100 UCITS ETF</FullNm><ShrtNm>AXA/NDX</ShrtNm><ClssfctnTp>CEOGBS</ClssfctnTp></FinInstrmGnlAttrbts>\
         <FinInstrmGnlAttrbts><Id>LU0334293981</Id><FullNm>Acatis Champions UCITS</FullNm><ShrtNm>ACATIS</ShrtNm><ClssfctnTp>CIOIES</ClssfctnTp></FinInstrmGnlAttrbts>\
         <FinInstrmGnlAttrbts><Id>US46437F1027</Id><FullNm>iShares ESG Aware ETF</FullNm><ClssfctnTp>CEOGBS</ClssfctnTp></FinInstrmGnlAttrbts>\
         <FinInstrmGnlAttrbts><Id>DK0060749877</Id><FullNm>Sydinvest Formue Akk A</FullNm><ClssfctnTp>CEOGBS</ClssfctnTp></FinInstrmGnlAttrbts>\n\
         <FinInstrmGnlAttrbts>\n  <Id>LU2523866023</Id>\n  <FullNm>Xtrackers Global Bond UCITS ETF</FullNm>\n  <ShrtNm>XGLB</ShrtNm>\n  <ClssfctnTp>CEOGBS</ClssfctnTp>\n</FinInstrmGnlAttrbts>";
    assert_eq!(
        firds_etf_isins(xml),
        vec!["IE000HN2PIB9".to_string(), "LU2523866023".to_string()]
    );
    assert!(firds_etf_isins("").is_empty()); // empty/garbage file -> empty leg, not a crash
    // hold_suitable: broad + cheap + physical + Acc + large + UCITS -> H; any leg failing -> no H
    let hold = |name: &str, ter: Option<f64>, repl: Option<&'static str>, use_: Option<&'static str>, aum: Option<f64>| {
        let mut q = Quote::stub("X", "", "", name);
        q.expense_ratio = ter;
        q.replication = repl;
        q.use_of_profits = use_;
        q.aum_eur = aum;
        hold_suitable(&q)
    };
    assert!(hold("Vanguard S&P 500 UCITS ETF USD Acc", Some(0.07), Some("Full"), Some("Acc"), Some(28.8e9))); // VUAA
    assert!(hold("State Street SPDR S&P 500 UCITS ETF", Some(0.03), Some("Full"), Some("Acc"), Some(15.0e9))); // SPYL
    assert!(!hold("Amundi S&P 500 Swap UCITS ETF", Some(0.15), Some("Swap"), Some("Acc"), Some(2.6e9))); // AUM5: swap
    // (#218) a sector fund is still not a core — but the exemplar had to change. "iShares S&P 500
    // Information Technology" is the LARGEST fund the sector sleeve admits (IITU.L, EUR 15.6B), so
    // with `hold_sector_sleeve` armed this line asserted the opposite of the shipped answer. VanEck
    // Semiconductor is refused a step EARLIER and by every knob setting: it names no index at all,
    // which is the invariant `SECTOR_GEO` preserves. The knob-off placement of the S&P 500 fund is
    // pinned directly, on `geo_tier_at`, in the (#218) block below.
    assert!(!hold("VanEck Semiconductor UCITS ETF", Some(0.35), Some("Full"), Some("Acc"), Some(2.5e9))); // sector, no geography
    assert!(!hold("Amundi Nasdaq-100 UCITS ETF", Some(0.22), Some("Full"), Some("Acc"), Some(5.8e9))); // nasdaq = not broad
    assert!(!hold("Vanguard S&P 500 UCITS ETF", None, Some("Full"), Some("Acc"), Some(28.8e9))); // no TER (venue fund) -> not vouched cheap
    assert!(!hold("Apple", None, None, None, None)); // a stock -> false
    assert!(hold("Vanguard FTSE All-World UCITS ETF USD Acc", Some(0.22), Some("Full"), Some("Acc"), Some(15e9))); // VWCE: 0.22% all-world under the 0.25 cap
    assert!(!hold("Vanguard FTSE All-World UCITS ETF USD Acc", Some(0.30), Some("Full"), Some("Acc"), Some(15e9))); // 0.30% too dear for a core
    assert!(!hold("iShares MSCI World EUR Hedged UCITS ETF Acc", Some(0.20), Some("Full"), Some("Acc"), Some(5e9))); // hedged class: hedge-cost drag, not the canonical core
    assert!(!hold("Xtrackers MSCI World Minimum Volatility UCITS ETF", Some(0.25), Some("Full"), Some("Acc"), Some(1.1e9))); // spelled-out factor tilt (live CORE receipt)
    assert!(!hold("BNP PARIBAS EASY II MSCI World PAB UCITS ETF Acc", Some(0.20), Some("Full"), Some("Acc"), Some(1.5e9))); // PAB = Paris-Aligned Benchmark, an ESG screen (live CORE receipt)

    // (#202) a world small-cap fund that clears every other leg is refused on the NAME while the size
    // sleeve is off — the non-negotiable-#1 proof that the knob ships neutral.
    // (#210) The cap is PASSED IN rather than read, because tests/ci-settings.yaml now SHIPS 0.35 and
    // a knob-reading assertion here would claim opposite things in the two config regimes CI runs.
    // Both directions are pinned: off refuses on the name, on admits — which is the knob's whole job.
    let world_small_q = {
        let mut q = Quote::stub("X", "", "", "iShares MSCI World Small Cap UCITS ETF");
        q.expense_ratio = Some(0.35);
        q.replication = Some("Opt");
        q.use_of_profits = Some("Acc");
        q.aum_eur = Some(3e9);
        q
    };
    assert_eq!(
        hold_miss_leg_with(&world_small_q, 0.0, false, false).map(|(leg, _)| leg),
        Some(0),
        "sleeve off -> refused at leg 0 on the name, whatever its facts"
    );
    assert!(
        hold_miss_leg_with(&world_small_q, 0.35, false, false).is_none(),
        "sleeve on -> the same fund is admitted; this is the row (#210) shipped"
    );

    // (#202) …and the sleeve's own admission rule, exercised directly with the cap passed in, because
    // `hold_size_sleeve_ter` is a process-wide OnceLock read from config that no test can flip.
    // Names are pre-lowercased — that is the contract of every fn in this family.
    let world_small = "ishares msci world small cap ucits etf";
    assert_eq!(size_sleeve_tier(world_small, "small", 0.0), None, "0.0 = sleeve off");
    assert_eq!(size_sleeve_tier(world_small, "small", 0.35), Some(SIZE_TIER), "cap on -> the eighth sleeve");
    // a SECOND narrow token still refuses: a small-cap ESG fund is an ESG fund
    assert_eq!(size_sleeve_tier("amundi msci world small cap esg ucits etf", "small", 0.35), None);
    assert_eq!(size_sleeve_tier("spdr msci europe small cap eur hedged ucits etf", "small", 0.35), None);
    // no geography at all -> not a world index, whatever its market cap
    assert_eq!(size_sleeve_tier("global x small cap ucits etf", "small", 0.35), None);
    // (#210) …and a geography that is not a WORLD one refuses too. XXSC.DE is the live receipt: it
    // is the single fund (#202)'s probe admitted, into a sleeve labelled "world small-cap".
    assert_eq!(
        size_sleeve_tier("xtrackers msci europe small cap ucits etf", "small", 0.35),
        None,
        "tier 5 is a region, and region × size is two bets under the name of one"
    );
    assert_eq!(
        size_sleeve_tier("ishares msci usa small cap ucits etf", "small", 0.35),
        None,
        "the US is no different — a regional size bet is still regional"
    );
    // tier 0 earns it as well as tier 1, so BOTH SIZE_GEO entries are live and neither can be dropped
    assert_eq!(
        size_sleeve_tier("vanguard ftse all-world small cap ucits etf", "small", 0.35),
        Some(SIZE_TIER),
        "all-world small cap is the broadest form of the exposure"
    );
    // (#211) the EM spelling gap and the eurozone trap it opens, pinned together. The token has
    // a TRAILING SPACE for exactly one reason and this is it: "MSCI EMU" is a real €3.9B fund.
    assert_eq!(geo_tier_at("ishares core msci em imi ucits etf usd (acc)", 0.0, false, false), Some(2),
        "the €34.8B name the census found; \"emerging\" is a plain substring and cannot see EM");
    assert_eq!(geo_tier_at("ishares msci em ucits etf usd (acc)", 0.0, false, false), Some(2),
        "and the plain one, with nothing after EM but UCITS");
    assert_eq!(geo_tier_at("ubs core msci emu ucits etf eur acc", 0.0, false, false), None,
        "EMU is the EUROZONE — drop the trailing space and this reads as emerging markets");
    assert_eq!(geo_tier_at("ishares vii plc - ishares msci em asia etf usd acc", 0.0, false, false), None,
        "a region inside a region: GEO gives it a tier, NARROW takes it back");
    assert_eq!(geo_tier_at("ishares msci em ex china ucits etf usd acc", 0.0, false, false), None,
        "excluding the largest constituent is a bet, and no China sleeve completes the partition");
    // (#213) the Japan / Asia-Pacific split, and the abbreviation that made the split worth making.
    assert_eq!(geo_tier_at("ishares vii plc - ishares core msci pac ex-jpn etf usd acc", 0.0, false, false), Some(7),
        "the €3.5B name the blind-spot census found; only \"msci pac \" can spell iShares' Pac ex-Jpn");
    assert_eq!(geo_tier_at("ishares core msci pacific ex japan ucits etf", 0.0, false, false), Some(7),
        "the spelled-out form lands in the SAME sleeve — one exposure, one tier");
    assert_eq!(geo_tier_at("ishares core msci japan imi ucits etf usd (acc)", 0.0, false, false), Some(6),
        "the split did not move Japan: tiers 0-6 are byte-identical to before (#213)");
    assert_eq!(geo_tier_at("xtrackers msci japan ucits etf 1c", 0.0, false, false), Some(6),
        "and neither did the other two rows the Japan sleeve already printed");
    assert_eq!(geo_tier_at("amundi index msci pacific ucits etf dr", 0.0, false, false), Some(7),
        "plain MSCI Pacific INCLUDES Japan — which is why the sleeve is labelled Asia-Pacific, not ex-Japan");
    assert_eq!(geo_tier_at("texas pacific land corporation", 0.0, false, false), None,
        "both tokens are MSCI-qualified: a bare \"pacific\" is a company name, and TPL is in the pond");

    // (#215) the three spellings the GEO blind spot proved missing, pinned on their REAL live names.
    assert_eq!(geo_tier_at("vanguard ftse japan ucits etf usd accumulation", 0.0, false, false), Some(6),
        "the Japan sleeve already existed; only FTSE's wording of it was unreachable");
    assert_eq!(geo_tier_at("amundi prime global ucits etf acc", 0.0, false, false), Some(1),
        "DEVELOPED markets — Amundi's Prime range omits the index name from the fund name");
    assert_eq!(geo_tier_at("amundi prime all country world ucits etf acc", 0.0, false, false), Some(0),
        "...and its ACWI sibling must still file at tier 0: the two Prime funds share no token");
    assert_eq!(geo_tier_at("l&g us equity ucits etf", 0.0, false, false), Some(3));
    // the LEADING SPACE on " us equity", pinned as load-bearing. `hold_name_tokens` ships OFF, so
    // the match is a bare `contains` and the space is the whole guard. Drop it and this reads Some(3).
    assert_eq!(geo_tier_at("fidelity focus equity ucits etf", 0.0, false, false), None,
        "\"foc-us equity\" is the mid-word accident the leading space exists to refuse");
    assert_eq!(geo_tier_at("xtrackers msci usa equity ucits etf", 0.0, false, false), Some(3),
        "\" usa equity\" does not match \" us equity\"; `msci usa` files it at the same tier anyway");
    // the blind spot is mostly FIXED INCOME, and no new token may reach it. GEO is the ONLY thing
    // keeping bonds out of an equity table — `NARROW` has no bond token and there is no
    // asset-class leg — so these are the guard on the whole round, not decoration.
    for bond in [
        "ishares iii public limited company - ishares global aggregate bond ucits etf",
        "vanguard usd treasury bond ucits etf usd accumulation",
        "amundi core euro government bond ucits etf acc",
        "xtrackers ii eur corporate bond ucits etf 1c",
    ] {
        assert_eq!(geo_tier_at(bond, 0.0, false, false), None, "{bond}: a bond fund must never reach a sleeve");
    }
    // ...and the single-country names stay refused: a blind spot is a name GEO cannot spell, which
    // is a different question from whether the lane wants it.
    for narrow in [
        "xtrackers dax ucits etf 1c",
        "franklin ftse india ucits etf",
        "vanguard ftse 100 ucits etf gbp accumulation",
        "ubs core msci emu ucits etf eur acc",
    ] {
        assert_eq!(geo_tier_at(narrow, 0.0, false, false), None, "{narrow}: single-country is not a broad sleeve");
    }
    // (#216) "world ex <anything>" is not the ex-US sleeve. Each of these was live in the pond and
    // filed at tier 4 by the bare "world ex" token; NARROW now refuses them before `geo_hit` runs.
    // The tier-1 assertion is the point: these names ALSO carry "msci world", so a fix that only
    // tightened the tier-4 token would have relocated the misfile instead of removing it.
    for bet in [
        "amundi index solutions - amundi msci world ex europe etf-c eur",
        "amundi msci world ex emu ucits etf acc",
        "ubs msci world ex mega cap ucits etf usd acc",
    ] {
        assert_eq!(geo_tier_at(bet, 0.0, false, false), None, "{bet}: a world-minus-region bet is not a sleeve");
    }
    // ...and the ex-US sleeve itself is UNTOUCHED — both incumbents and the FTSE spellings still
    // reach tier 4. `world ex us` (space) and `ex-usa` (hyphen) are two spellings of one partition.
    for exus in [
        "xtrackers msci world ex usa ucits etf 1c usd",              // EXUS.DE, incumbent
        "ishares msci world ex-usa ucits etf usd (acc)",             // IXUA.DE, incumbent
        "xtrackers ftse all world ex us etf 1c usd",                 // AWEX.DE
        "vanguard funds plc - vanguard ftse all-world ex-u.s. ucits etf usd dist",
    ] {
        assert_eq!(geo_tier_at(exus, 0.0, false, false), Some(4), "{exus}: world-minus-US IS a partition");
    }

    // (#217) the world FACTOR sleeve. Every name here is LIVE in the 2026-09-02 pond, and the three
    // that qualify are the three the sleeve was built to admit — one per factor family, all clearing
    // the BASE 0.25% cap, which is why this sleeve carries no ceiling of its own.
    for (fund, why) in [
        ("invesco msci world equal weight ucits etf usd accumulating", "MWEP.L, 0.20%, €1.6B"),
        ("xtrackers msci world quality ucits etf 1c", "XDEQ.DE, 0.25%, €2.8B"),
        ("xtrackers msci world value ucits etf 1c", "XDEV.DE, 0.25%, €3.7B"),
        // tier 0 rather than tier 1, and refused LATER on TER (0.39%) — placement is not admission
        ("invesco rafi all-world fundamental value ucits etf", "PSRW.L, an all-world factor fund"),
    ] {
        assert_eq!(geo_tier_at(fund, 0.0, true, false), Some(FACTOR_TIER), "{why}: the factor sleeve claims it");
        assert_eq!(geo_tier_at(fund, 0.0, false, false), None, "{why}: …and refuses it with the knob off");
        assert_eq!(geo_tier_at(fund, 0.35, false, false), None, "{why}: the SIZE knob does not open this sleeve");
    }
    // …and the negatives, which are what keep the sleeve from filling on the wrong axis.
    for (no, why) in [
        // REGIONAL: region × factor is two bets wearing the name of one — `FACTOR_GEO` is [0, 1]
        ("ishares edge msci usa quality factor ucits etf usd (acc)", "a US quality fund is not world"),
        // CROSSED: a second NARROW token disqualifies, exactly as it does for the size sleeve
        ("ubs msci world quality esg ucits etf usd acc", "ESG-crossed is still an ESG fund"),
        ("ishares msci world small cap value ucits etf", "size-crossed claims two sleeves at once"),
        // OUT OF SCOPE BY DECISION: momentum contradicts a 20-year hold, min-vol targets volatility
        ("ishares edge msci world momentum factor ucits etf", "momentum is deliberately not in FACTOR"),
        ("ishares edge msci world minimum volatility ucits etf usd (acc)", "nor is minimum volatility"),
    ] {
        assert_eq!(geo_tier_at(no, 0.0, true, false), None, "{no}: {why}");
        assert_eq!(geo_tier_at(no, 0.35, true, false), None, "{no}: {why} — with BOTH sleeves open");
    }
    // the sleeve is ADDITIVE: a plain world tracker is untouched by the knob, at either setting
    for (plain, tier) in [
        ("ishares core msci world ucits etf usd (acc)", 1),
        ("vanguard ftse all-world ucits etf usd accumulation", 0),
        ("xtrackers msci world ex usa ucits etf 1c usd", 4),
    ] {
        assert_eq!(geo_tier_at(plain, 0.0, false, false), Some(tier), "{plain}: shipped placement");
        assert_eq!(geo_tier_at(plain, 0.0, true, false), Some(tier), "{plain}: …unmoved by the factor knob");
    }
    // (#218) the world SECTOR sleeve. Every name here is LIVE in the 2026-09-02 pond, and all nine
    // funds that clear every remaining leg are represented — five families across two geographies,
    // all inside the BASE 0.25% cap, which is why this sleeve carries no ceiling of its own either.
    for (fund, why) in [
        // tier 3, the S&P 500 sector suite — and the four names that prove `SECTOR_OK` is needed:
        // every one of them carries the bare word "sector" as a SECOND narrow token
        ("ishares s&p 500 information technology sector ucits etf usd (acc)", "IITU.L, 0.15%, EUR 15.6B"),
        ("ishares s&p 500 health care sector ucits etf usd (acc)", "IUHC.L, 0.15%, EUR 2.6B"),
        ("ishares s&p 500 financials sector ucits etf usd (acc)", "UIFS.L, 0.15%, EUR 2.1B"),
        ("ishares s&p 500 energy sector ucits etf usd (acc)", "IESU.L, 0.15%, EUR 1.2B"),
        // tier 1, the MSCI World sector suite — no "sector" token, so these pass the conjunct plain
        ("xtrackers msci world information technology ucits etf 1c", "XDWT.L, 0.25%, EUR 5.0B"),
        ("xtrackers msci world health care ucits etf 1c", "XDWH.L, 0.25%, EUR 3.3B"),
        ("xtrackers msci world energy ucits etf 1c", "XDW0.L, 0.25%, EUR 1.7B"),
        ("xtrackers msci world financials ucits etf 1c", "XDWF.L, 0.25%, EUR 1.2B"),
        ("xtrackers msci world industrials ucits etf 1c", "XDWI.L, 0.25%, EUR 1.2B"),
    ] {
        assert_eq!(geo_tier_at(fund, 0.0, false, true), Some(SECTOR_TIER), "{why}: the sector sleeve claims it");
        assert_eq!(geo_tier_at(fund, 0.0, false, false), None, "{why}: …and refuses it with the knob off");
        assert_eq!(geo_tier_at(fund, 0.35, true, false), None, "{why}: neither OTHER sleeve opens this one");
    }
    // …and the negatives. The first is the one the whole `SECTOR_GEO` decision turns on.
    for (no, why) in [
        // NO GEOGRAPHY AT ALL: the invariant every CORE row holds — a fund must name a broad index.
        // These are the highest-CAGR sector funds in the pond and they STILL do not get in.
        ("vaneck semiconductor ucits etf", "names no index — a bare theme is not a sleeve"),
        ("ishares msci global semiconductors ucits etf usd acc", "\"global\" alone is not a GEO token"),
        ("invesco technology s&p us select sector ucits etf acc", "…nor is \"s&p us select sector\""),
        // CROSSED: a second NARROW token disqualifies, exactly as in the other two sleeves
        ("fineco am msci world information technology sustainable select", "ESG-crossed is an ESG fund"),
        ("ishares s&p 500 health care sector ucits etf eur hedged (dist)", "currency-hedged is a wrapper"),
        ("xtrackers msci world information technology small cap ucits etf", "size-crossed claims two sleeves"),
        // NOT A SECTOR: the Nasdaq-100 is a broad growth index, deliberately absent from `SECTOR`
        ("invesco eqqq nasdaq-100 ucits etf", "nasdaq is not a sector token"),
        // REGIONAL: a single-country sector fund has no sleeve to complete it with
        ("ishares msci china health care ucits etf", "single-country x sector is two bets, no geography"),
    ] {
        assert_eq!(geo_tier_at(no, 0.0, false, true), None, "{no}: {why}");
        assert_eq!(geo_tier_at(no, 0.35, true, true), None, "{no}: {why} — with ALL THREE sleeves open");
    }
    // the sleeve is ADDITIVE: a plain broad tracker is untouched by the knob, at either setting.
    // Tier 3 matters most here — it is the tier `SECTOR_GEO` newly reaches into.
    for (plain, tier) in [
        ("state street spdr s&p 500 ucits etf usd acc", 3),
        ("xtrackers msci usa ucits etf 1c", 3),
        ("ishares core msci world ucits etf usd (acc)", 1),
    ] {
        assert_eq!(geo_tier_at(plain, 0.0, false, false), Some(tier), "{plain}: shipped placement");
        assert_eq!(geo_tier_at(plain, 0.0, false, true), Some(tier), "{plain}: …unmoved by the sector knob");
    }
    // (#218) the family key, which is what makes the 3 slots hold 3 DIFFERENT sectors rather than
    // three technology wrappers — every one of these funds keys `msci world`/`s&p 500` on GEO alone.
    assert_eq!(sleeve_family_of("iShares S&P 500 Information Technology Sector UCITS ETF USD (Acc)"),
        Some("technolog"), "the FIRST narrow token is the sector, not the co-occurring \"sector\"");
    assert_eq!(sleeve_family_of("Xtrackers MSCI World Health Care UCITS ETF 1C"), Some("health"));
    assert_eq!(sleeve_family_of("Xtrackers MSCI World Financials UCITS ETF 1C"), Some("financ"));
    assert_eq!(sleeve_family_of("Xtrackers MSCI World Industrials UCITS ETF 1C"), Some("industrial"));
    assert_eq!(sleeve_family_of("Xtrackers MSCI World Energy UCITS ETF 1C"), Some("energy"));
    assert_ne!(
        sleeve_family_of("iShares S&P 500 Information Technology Sector UCITS ETF USD (Acc)"),
        sleeve_family_of("iShares S&P 500 Health Care Sector UCITS ETF USD (Acc)"),
        "two S&P 500 sector funds must NOT share a family, or one sector takes both slots",
    );

    // (#217) and the family key `(#214)`'s fill spends its slots on. Without this every world-factor
    // fund reports the SAME family (`msci world`) and one index could take all three slots.
    assert_eq!(sleeve_family_of("Xtrackers MSCI World Quality UCITS ETF 1C"), Some("quality"));
    assert_eq!(sleeve_family_of("Xtrackers MSCI World Value UCITS ETF 1C"), Some("value"));
    assert_eq!(sleeve_family_of("Invesco MSCI World Equal Weight UCITS ETF USD Accumulating"),
        Some("equal weight"), "three funds, three families — the sleeve fills on three indices");
    assert_eq!(sleeve_family_of("iShares Core MSCI World UCITS ETF USD (Acc)"), Some("msci world"),
        "a non-factor fund still keys on its GEO family, exactly as it did");
    // …and a fund whose narrow token is NOT a factor one falls THROUGH to the GEO family rather
    // than keying on its own token. This is what makes the match GUARD load-bearing: without it the
    // world small-cap sleeve would key on "small" and stop sharing a family space with anything.
    assert_eq!(sleeve_family_of("iShares MSCI World Small Cap UCITS ETF"), Some("msci world"),
        "a SIZE token is not a family key — only the FACTOR tokens are");
    assert_eq!(sleeve_family_of("Xtrackers MSCI World ESG UCITS ETF"), Some("msci world"),
        "nor is any other NARROW token");
    assert_eq!(sleeve_family_of("Berkshire Hathaway Inc."), None, "not a broad-index name at all");
    // (#214) the INDEX FAMILY behind a row — the same scan `geo_hit` runs, read for its token.
    assert_eq!(geo_family_of("State Street SPDR S&P 500 UCITS ETF USD Acc"), Some("s&p 500"));
    assert_eq!(geo_family_of("Xtrackers MSCI USA UCITS ETF 1C"), Some("msci usa"));
    assert_eq!(geo_family_of("Amundi Core MSCI USA UCITS ETF Acc"), Some("msci usa"),
        "the two sharing the US sleeve's slots 2 and 3 are ONE family — this is the whole finding");
    assert_eq!(geo_family_of("Amundi Prime All Country World UCITS ETF Acc"), Some("all country world"));
    assert_eq!(geo_family_of("iShares MSCI ACWI UCITS ETF USD Acc"), Some("acwi"),
        "ACWI and All Country World are the same INDEX under different tokens: the key is GEO's own \
         spelling, and that coarseness is why the fill re-orders instead of capping per family");
    assert_eq!(geo_family_of("Vanguard FTSE Developed Europe ex UK UCITS ETF EUR"),
        Some("ftse developed europe"),
        "…and it cuts the other way too: ex-UK is a DIFFERENT index sharing one token");
    assert_eq!(geo_family_of("Berkshire Hathaway Inc."), None, "not a broad-index name at all");
    // non-negotiable #4: family and tier are ONE scan, so they cannot disagree about what fired.
    for (name, tier) in [
        ("State Street SPDR S&P 500 UCITS ETF USD Acc", 3u8),
        ("iShares Core MSCI EM IMI UCITS ETF USD (Acc)", 2),
        ("iShares VII PLC - iShares Core MSCI Pac ex-Jpn ETF USD Acc", 7),
    ] {
        let fam = geo_family_of(name).expect("a broad-index name has a family");
        assert_eq!(geo_hit(&name.to_lowercase()), Some(tier), "{name}");
        assert_eq!(GEO.iter().find(|(t, _)| *t == fam).map(|(_, t)| *t), Some(tier),
            "{name}: family {fam} must carry the SAME tier geo_hit reports");
    }

    // (#214) the fill order. OFF is the identity, and that is non-negotiable #1 in one line.
    const E: u8 = 5; // Europe, so the pairs read like the sleeve they came from
    let europe = [(E, Some("ftse developed europe")), (E, Some("ftse developed europe")),
                  (E, Some("msci europe")), (E, Some("stoxx europe 600"))];
    assert_eq!(family_first_order(&europe, false), vec![0, 1, 2, 3], "off -> untouched order");
    assert_eq!(family_first_order(&europe, true), vec![0, 2, 3, 1],
        "on -> one of each family first; the SECOND ftse row is what a cap of 3 now drops, and \
         stoxx europe 600 is the family the live Europe sleeve was hiding");
    // it RE-ORDERS and never drops: always a permutation, whatever the input
    for on in [false, true] {
        let mut sorted = family_first_order(&europe, on);
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3], "on={on}: every candidate survives the reorder");
    }
    assert_eq!(family_first_order(&[], true), Vec::<usize>::new(), "an empty sleeve is not a loop");
    assert_eq!(family_first_order(&[(E, Some("acwi"))], true), vec![0], "one candidate, one slot");
    assert_eq!(
        family_first_order(&[(3, Some("s&p 500")), (3, Some("msci usa")), (3, Some("crsp us total market"))], true),
        vec![0, 1, 2],
        "all families distinct -> already family-first, so the order must not move"
    );
    assert_eq!(
        family_first_order(&[(3, Some("msci usa")), (3, Some("msci usa")), (3, Some("msci usa"))], true),
        vec![0, 1, 2],
        "one family -> nothing to interleave, and the TER order inside it is preserved"
    );
    assert_eq!(
        family_first_order(&[(E, Some("a")), (E, Some("a")), (E, Some("a")), (E, Some("b"))], true),
        vec![0, 3, 1, 2],
        "the one unshown family jumps the queue once; the rest keep their ranking"
    );
    assert_eq!(family_first_order(&[(E, None), (E, None), (E, Some("acwi"))], true), vec![0, 2, 1],
        "no GEO token = one shared key: two unspellable names are not two families");
    // (#214) the US sleeve in its live TER order, and the reason this is ONE pass, not a full
    // round-robin. A round-robin puts the SECOND s&p 500 ahead of the second msci usa merely
    // because `s&p 500` appeared first — the 2026-09-02 probe did exactly that, swapping WEBH.DE
    // (0.03%) out for SP5AU.SW while BOTH families were already shown. Nothing is gained by it.
    assert_eq!(
        family_first_order(
            &[(3, Some("s&p 500")), (3, Some("msci usa")), (3, Some("msci usa")), (3, Some("s&p 500"))], true),
        vec![0, 1, 2, 3],
        "both families shown by slot 2, so slots 3+ fall back to the sleeve's own ranking"
    );
    // (#214) TIERS ARE THE UNIT. The sleeves must stay grouped and in order — `print_hold_core`
    // walks this list expecting breadth-major runs — and the same token in two sleeves is two
    // families, because the question is whether THIS sleeve already shows that index.
    assert_eq!(
        family_first_order(
            &[(0, Some("acwi")), (0, Some("acwi")), (0, Some("all country world")),
              (3, Some("msci usa")), (3, Some("msci usa")), (3, Some("s&p 500"))], true),
        vec![0, 2, 1, 3, 5, 4],
        "each tier re-orders inside itself; no row crosses a sleeve boundary"
    );
    assert_eq!(
        family_first_order(&[(0, Some("acwi")), (3, Some("acwi"))], true), vec![0, 1],
        "one token, two sleeves -> both are firsts: tier 3 showing `acwi` says nothing about tier 0"
    );
    // an out-of-order input is sorted BACK into tier order rather than trusted — the caller's list
    // is breadth-major today, and this keeps that a fact rather than an assumption
    assert_eq!(family_first_order(&[(3, Some("msci usa")), (0, Some("acwi"))], true), vec![1, 0],
        "tier is the primary key, so a mis-sorted caller cannot smuggle a sleeve out of place");
    // the blocking token must be a SIZE one — the sleeve rehabilitates market cap, not sectors
    assert_eq!(size_sleeve_tier("xtrackers msci world health care ucits etf", "health", 0.35), None);

    // (#202) and the ceiling each sleeve is judged against — SIZE_TIER alone gets the sleeve's cap
    assert_eq!(ter_cap_for(SIZE_TIER, 0.35, 0.25), 0.35);
    assert_eq!(ter_cap_for(0, 0.35, 0.25), 0.25, "all-world keeps the shared cap");
    assert_eq!(ter_cap_for(1, 0.35, 0.25), 0.25, "developed keeps the shared cap");

    // (#207) the all-world sleeve's own row cap, and the two halves that make it safe: 0 inherits
    // (non-negotiable #1, the shipped lane), and the override reaches tier 0 and nothing else.
    assert_eq!(tier_cap(0, 3, 0, 0), 3, "0 = no override, inherit the shared cap");
    assert_eq!(tier_cap(0, 3, 5, 0), 5, "…and tier 0 takes it when set");
    assert_eq!(tier_cap(1, 3, 5, 0), 3, "developed is untouched");
    assert_eq!(tier_cap(HOLD_TIERS - 1, 3, 5, 0), 3, "…and so is the last sleeve");
    // (#220) the SECTOR override, pinned on the LITERAL tier as well as the symbolic one: the arm
    // is `tier == SECTOR_TIER`, and a mutation that rewrites that constant moves a symbolic
    // assertion with it. 10 is the literal `SECTOR_TIER` this ladder ships.
    assert_eq!(tier_cap(SECTOR_TIER as usize, 3, 0, 5), 5, "(#220) the sector sleeve takes its own cap");
    assert_eq!(tier_cap(10, 3, 0, 5), 5, "…and it is tier 10 that does so, spelled as a literal");
    assert_eq!(tier_cap(SECTOR_TIER as usize, 3, 0, 0), 3, "0 = no override, inherit the shared cap");
    assert_eq!(tier_cap(FACTOR_TIER as usize, 3, 0, 5), 3, "the sleeve below it is untouched");
    assert_eq!(tier_cap(0, 3, 7, 5), 7, "all-world keeps its precedence over the sector override");
    assert_eq!(tier_cap(1, 3, 0, 5), 3, "…and every ordinary sleeve still reads the shared cap");

    // (#207) "all country world": a SPACE where core::GEO spelled a hyphen, which is why two SPDR
    // ACWI funds (€14.8B and €6.6B) read as "not a broad-index name" until the (#203) census found
    // them. The ex-US pin is the ordering half — GEO is first-match-wins and the ex-US group is
    // listed FIRST precisely so a carve-out cannot be read as the whole planet.
    assert_eq!(hold_breadth_tier("State Street SPDR MSCI All Country World UCITS ETF"), 0);
    assert_eq!(hold_breadth_tier("SPDR MSCI All Country World Investable Market UCITS ETF"), 0);
    assert_eq!(hold_breadth_tier("SPDR MSCI All Country World ex USA UCITS ETF"), 4,
        "a carve-out is ex-US, not the planet — the ex-US tokens are listed first for this");
    // (#202) and an optional sleeve stays INVISIBLE while it is off, so the census line is
    // byte-identical. (#217) all FOUR combinations, because the two knobs are independent and the
    // `size off + factor on` cell is the one a `take`-based count got silently wrong: it would have
    // printed the factor sleeve's numbers under the `world small-cap` label.
    // (#218) …and the bound is now `- 3`, because there are THREE optional sleeves. Spelled off
    // `SIZE_TIER` rather than as more arithmetic on `HOLD_TIERS`: the geographic sleeves are exactly
    // the tiers BELOW the first optional one, which is what this loop means and what a literal 8
    // would stop meaning the next time the ladder grows.
    for tier in 0..SIZE_TIER as usize {
        assert!(sleeve_visible(tier, 0.0, false, false), "tier {tier}: a GEOGRAPHIC sleeve always prints");
        assert!(sleeve_visible(tier, 0.35, true, true), "tier {tier}: …whatever the optional knobs say");
    }
    // (#217) the two optional tiers sit at the END of the ladder, adjacent, in this order. Pinned as
    // LITERALS as well as relatively, because `SIZE_TIER = FACTOR_TIER - 1` is arithmetic and a
    // mutation gate can rewrite the operator: `+` puts a sleeve past the end of every `[_; HOLD_TIERS]`
    // array in `picks`, and `/` collapses the two onto one tier so the census prints one label twice.
    // If an eleventh sleeve is ever added, THIS is the assertion that should fail first.
    assert_eq!(HOLD_TIERS, 11, "eight geographic sleeves plus the three optional ones");
    assert_eq!(SECTOR_TIER, 10, "the sector sleeve is LAST — least diversified, ranked accordingly");
    assert_eq!(FACTOR_TIER, 9, "…the factor sleeve immediately before it — and `hold_breadth_tier`'s
        fallback is PINNED here, so it no longer follows whichever sleeve happens to be last");
    assert_eq!(SIZE_TIER, 8, "…and the size sleeve before that, never level with either");
    assert_eq!(hold_breadth_tier("Berkshire Hathaway Inc."), FACTOR_TIER,
        "a name with no geography lands in the PINNED fallback, not in the last sleeve");
    let size = SIZE_TIER as usize;
    let factor = FACTOR_TIER as usize;
    assert!(!sleeve_visible(size, 0.0, false, false) && !sleeve_visible(factor, 0.0, false, false),
        "both off -> the eight geographic sleeves only, byte-identical to the shipped line");
    assert!(sleeve_visible(size, 0.35, false, false) && !sleeve_visible(factor, 0.35, false, false),
        "size on, factor off -> the ninth sleeve joins and the tenth does not");
    assert!(!sleeve_visible(size, 0.0, true, false) && sleeve_visible(factor, 0.0, true, false),
        "size OFF, factor ON -> the combination a count could not express");
    assert!(sleeve_visible(size, 0.35, true, false) && sleeve_visible(factor, 0.35, true, false),
        "both on -> the full ten-cell ladder");
    // (#218) the THIRD knob is independent of the other two in both directions — the pair of cases a
    // count could never have expressed, now that there are three sleeves to order wrongly.
    let sector = SECTOR_TIER as usize;
    assert!(!sleeve_visible(sector, 0.35, true, false),
        "sector OFF while BOTH others are on -> the eleventh cell stays hidden");
    assert!(sleeve_visible(sector, 0.0, false, true)
        && !sleeve_visible(size, 0.0, false, true)
        && !sleeve_visible(factor, 0.0, false, true),
        "sector ON alone -> only the eleventh cell joins, and it is not printed under another label");

    // (round 47) Yahoo fallback facts count for the H flag via ter_shown/aum_shown — a venue fund with
    // no BF TER/AUM but Yahoo facts qualifies; BF stays authoritative when both are present.
    let mut q = Quote::stub("X", "", "", "Vanguard S&P 500 UCITS ETF USD Acc");
    q.replication = Some("Full");
    q.use_of_profits = Some("Acc");
    q.ter_fallback = Some(0.07);
    q.aum_fallback = Some(5e9);
    assert!(hold_suitable(&q));
    q.expense_ratio = Some(0.30); // BF answers dear -> fallback must NOT mask it
    assert_eq!(q.ter_shown(), Some(0.30));
    assert!(!hold_suitable(&q));

    // (round 49) hold_miss_reason: first failing leg, printable; None == hold_suitable by construction
    let miss = |name: &str, ter: Option<f64>, repl: Option<&'static str>, use_: Option<&'static str>, aum: Option<f64>| {
        let mut q = Quote::stub("X", "", "", name);
        q.expense_ratio = ter;
        q.replication = repl;
        q.use_of_profits = use_;
        q.aum_eur = aum;
        hold_miss_reason(&q)
    };
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF USD Acc", Some(0.07), Some("Full"), Some("Acc"), Some(28.8e9)), None); // VUAA passes all
    assert_eq!(miss("Amundi Nasdaq-100 UCITS ETF", Some(0.22), Some("Full"), Some("Acc"), Some(5.8e9)).as_deref(), Some("not a broad-index name (sector/thematic/factor tilt)"));
    assert_eq!(miss("Vanguard S&P 500 ETF", Some(0.03), Some("Full"), Some("Acc"), Some(2e9)).as_deref(), Some("no UCITS token in the name"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF", None, Some("Full"), Some("Acc"), Some(2e9)).as_deref(), Some("TER unknown"));
    assert_eq!(miss("Vanguard FTSE All-World UCITS ETF", Some(0.30), Some("Full"), Some("Acc"), Some(15e9)).as_deref(), Some("TER 0.30% > 0.25% cap"));
    assert_eq!(miss("Amundi S&P 500 Swap UCITS ETF", Some(0.15), Some("Swap"), Some("Acc"), Some(2.6e9)).as_deref(), Some("replication Swap (needs physical)")); // AUM5
    // (round 53) sampling IS physical: VWRA-shape all-world fund ("Optimised") must pass — the old
    // literal-Full leg kept the CORE tier-0 slot empty (every big all-world fund samples).
    assert_eq!(miss("Vanguard FTSE All-World UCITS ETF USD Acc", Some(0.22), Some("Opt"), Some("Acc"), Some(42.9e9)), None);
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF Acc", Some(0.07), None, Some("Acc"), Some(2e9)).as_deref(), Some("replication unknown (needs physical)"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF", Some(0.07), Some("Full"), Some("Dist"), Some(2e9)).as_deref(), Some("share class Dist (needs Acc)"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF Acc", Some(0.07), Some("Full"), Some("Acc"), Some(0.3e9)).as_deref(), Some("AUM €0.3B < €1B floor"));
    assert_eq!(miss("Vanguard S&P 500 UCITS ETF Acc", Some(0.07), Some("Full"), Some("Acc"), None).as_deref(), Some("AUM unknown"));

    // hold_breadth_tier: broadest (all-world/ACWI) sorts first, then the geographic sleeves
    assert_eq!(hold_breadth_tier("Vanguard FTSE All-World UCITS ETF"), 0);
    assert_eq!(hold_breadth_tier("SPDR MSCI ACWI UCITS ETF"), 0);
    assert_eq!(hold_breadth_tier("iShares Core MSCI World UCITS ETF"), 1);
    assert_eq!(hold_breadth_tier("iShares Core MSCI Emerging Markets IMI UCITS ETF"), 2);
    assert_eq!(hold_breadth_tier("Vanguard S&P 500 UCITS ETF"), 3);
    assert_eq!(hold_breadth_tier("Xtrackers MSCI Europe UCITS ETF"), 5);
    assert_eq!(hold_breadth_tier("iShares Core MSCI Japan IMI UCITS ETF"), 6);

    // (round 118) the geography rule: a REGION is a sleeve, an INDUSTRY or a STYLE is not. EM is the
    // whole point of the widening — it was structurally impossible before, at any TER or size.
    assert!(is_broad_index_name("iShares Core MSCI Emerging Markets IMI UCITS ETF"));
    assert!(is_broad_index_name("Amundi Prime Emerging Markets UCITS ETF"));
    assert!(is_broad_index_name("Xtrackers MSCI Europe UCITS ETF"));
    assert!(is_broad_index_name("Vanguard FTSE Developed World UCITS ETF"));
    assert!(!is_broad_index_name("VanEck Semiconductor UCITS ETF"));
    // (#218) THIS NAME'S VERDICT IS NOW CONFIG-DEPENDENT TOO, for `(#210)`'s reason stated just
    // below and pinned the same way: the code default is `false` and tests/ci-settings.yaml ships
    // `true`, so a knob-READING assertion would contradict itself between the two regimes CI runs.
    // The line above keeps the unconditional half — a sector fund that names no index is refused at
    // every setting, which is the invariant the sleeve does not touch.
    let sp_tech_lower = "ishares s&p 500 information technology sector ucits etf";
    assert_eq!(geo_tier_at(sp_tech_lower, 0.0, false, false), None,
        "sleeve off -> a sector BET, still barred; round 118's rule is unchanged at the default");
    assert_eq!(geo_tier_at(sp_tech_lower, 0.0, false, true), Some(SECTOR_TIER),
        "sleeve on -> the eleventh sleeve, the deliberate exception rather than a loosening");
    // (#210) THIS NAME'S VERDICT IS NOW CONFIG-DEPENDENT, so it is pinned with the cap passed in
    // rather than read: the code default is 0.0 and tests/ci-settings.yaml ships 0.35, and a
    // knob-reading assertion would contradict itself between the two regimes CI runs. Round 118's
    // rule is UNCHANGED at the default — a size tilt is not a geography — and the sleeve is the
    // deliberate, measured exception to it rather than a loosening of it.
    let world_small_lower = "ishares msci world small cap ucits etf";
    assert_eq!(geo_tier_at(world_small_lower, 0.0, false, false), None, "sleeve off -> a size TILT, still barred");
    assert_eq!(geo_tier_at(world_small_lower, 0.35, false, false), Some(SIZE_TIER), "sleeve on -> the eighth sleeve");
    assert!(!is_broad_index_name("iShares MSCI World EUR Hedged UCITS ETF"));
    assert!(!is_broad_index_name("iShares MSCI World ESG Screened UCITS ETF"));
    assert!(!is_broad_index_name("iShares MSCI World Minimum Volatility UCITS ETF"));

    // the three tokens that FLIPPED: US + World-ex-US is a valid partition, so ex-US is now a sleeve.
    assert!(is_broad_index_name("SPDR MSCI World ex USA UCITS ETF"));
    // …and it must NOT inherit tier 1, which would rank an ex-US sleeve above MSCI World itself.
    assert_eq!(hold_breadth_tier("SPDR MSCI World ex USA UCITS ETF"), 4);
    assert_eq!(hold_breadth_tier("iShares MSCI ACWI ex US UCITS ETF"), 4);
    // the other superstring collision: "ftse developed europe" must not read as plain "ftse developed"
    assert_eq!(hold_breadth_tier("Vanguard FTSE Developed Europe UCITS ETF"), 5);
    // every tier the table can emit stays inside an array sized by HOLD_TIERS (this is the panic)
    for name in ["Vanguard FTSE All-World", "MSCI World", "MSCI Emerging", "S&P 500",
                 "MSCI World ex USA", "MSCI Europe", "MSCI Japan", "not an index at all"] {
        assert!((hold_breadth_tier(name) as usize) < HOLD_TIERS, "{name} tier out of range");
    }

    // (#102) the word-start matcher `hold_name_tokens` swaps in. Three properties, and the middle one
    // is why this is not whole-word matching: half of NARROW is a deliberate PREFIX.
    for (name, token) in [
        ("vaneck semiconductor ucits etf", "semiconduct"), // prefix, mid-name
        ("technology select sector spdr", "technolog"),    // prefix, first word
        ("msci world financials", "financ"),
        ("amundi nasdaq-100 ucits etf", "nasdaq"), // a hyphen is a boundary
        ("ishares msci world small cap", "small"),
    ] {
        assert!(token_at_word_start(name, token), "{token} must still disqualify {name}");
        assert!(name.contains(token), "test fixture drifted: {name} no longer contains {token}");
    }
    // the mid-word accidents the bare `contains` cannot tell from the real thing — all three of these
    // English words hide a NARROW token inside themselves, and today all three delete a core name.
    for (name, token) in [
        ("lyxor global gender equality", "quality"),
        ("msci world comparison index", "paris"),
        ("amundi devalued markets index", "value"),
    ] {
        assert!(name.contains(token), "test fixture drifted: {name} no longer contains {token}");
        assert!(!token_at_word_start(name, token), "{token} must not disqualify {name} mid-word");
    }
    // a token carrying its own separator is passed through to plain `contains` — the guard is IN the
    // token, so " pab" answers exactly what it answered before, on both sides.
    assert!(token_at_word_start("msci world pab ucits etf", " pab"));
    assert!(!token_at_word_start("xtrackers spab index", " pab"));
    assert_eq!(
        source_url("https://finance.yahoo.com/quote/{ticker}", "BTC-USD"),
        "https://finance.yahoo.com/quote/BTC-USD"
    );

    assert_eq!(headline_titles(&[serde_json::json!({"title": "flat"})]), vec!["flat"]);
    assert_eq!(headline_titles(&[serde_json::json!({"content": {"title": "nested"}})]), vec!["nested"]);
    assert!(headline_titles(&[serde_json::json!({}), serde_json::json!({"content": {}})]).is_empty());

    let ds = vec![
        NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2020, 1, 2).unwrap(),
        NaiveDate::from_ymd_opt(2020, 1, 3).unwrap(),
    ];
    let cs = vec![10.0, 20.0, 30.0];
    assert_eq!(asof(&ds, &cs, NaiveDate::from_ymd_opt(2020, 1, 2).unwrap()), Some(20.0));
    assert_eq!(asof(&ds, &cs, NaiveDate::from_ymd_opt(2019, 6, 1).unwrap()), None);
    assert_eq!(asof(&ds, &cs, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()), Some(30.0));
    // asof_avg: ±2d window over Jan-2 averages all 3 days (smooths an outlier); window with no close = None
    assert_eq!(asof_avg(&ds, &cs, NaiveDate::from_ymd_opt(2020, 1, 2).unwrap(), 2), Some(20.0));
    // splice_history: proxy bars before the listing's start are rebased so the chain is continuous
    // at the boundary (proxy 50 as-of own-first 10 -> factor 0.2), own series unchanged after it
    let pd = vec![
        NaiveDate::from_ymd_opt(2019, 12, 30).unwrap(),
        NaiveDate::from_ymd_opt(2019, 12, 31).unwrap(),
        NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
    ];
    let pc = vec![40.0, 45.0, 50.0];
    let (sd, sc) = splice_history(&ds, &cs, &pd, &pc).unwrap();
    assert_eq!(sd.len(), 5); // 2 prepended proxy bars (strictly < 2020-01-01) + 3 own
    assert_eq!(sc, vec![8.0, 9.0, 10.0, 20.0, 30.0]); // 40*0.2, 45*0.2, then own untouched
    assert_eq!(sd[0], pd[0]);
    assert_eq!(sd[2..], ds[..]);
    // proxy with nothing older than the listing -> None (splice adds nothing)
    assert!(splice_history(&ds, &cs, &ds, &cs).is_none());
    // proxy that doesn't reach the listing's start -> None (no rebase anchor)
    let late = vec![NaiveDate::from_ymd_opt(2020, 6, 1).unwrap()];
    assert!(splice_history(&ds, &cs, &late, &[99.0]).is_none());
    assert_eq!(asof_avg(&ds, &cs, NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), 0), Some(10.0));
    assert_eq!(asof_avg(&ds, &cs, NaiveDate::from_ymd_opt(2019, 1, 1).unwrap(), 5), None);
    // (H-cov) the JEDG.L case: a ~4-year daily series must NOT report a 5Y leg. The 5Y anchor window is
    // ±365d, so `asof_avg` finds this series' earliest bars inside [target-365, target+365] and returns
    // an average — a real price under a label it never earned. Only the coverage guard blanks it; note
    // the window-entirely-before-history case above never exercised a window that STRADDLES the first
    // bar, which is why this shipped. The legs the series does cover must still fill.
    let young: Vec<NaiveDate> =
        (0..1460).map(|k| NaiveDate::from_ymd_opt(2022, 1, 1).unwrap() + Duration::days(k)).collect();
    let yc: Vec<f64> = (0..1460).map(|k| 100.0 * 1.0005_f64.powi(k)).collect();
    let yp = horizon_changes(&young, &yc, None, &BTreeMap::new(), None);
    let leg = |l: &str| yp[HORIZONS.iter().position(|(h, _)| *h == l).unwrap()].clone();
    assert!(leg("5Y").is_none(), "4y of history cannot carry a 5Y leg: {:?}", leg("5Y"));
    assert!(leg("8Y").is_none() && leg("10Y").is_none() && leg("20Y").is_none());
    // covered legs unaffected — the guard must blank the unreachable ones ONLY
    assert!(leg("1Y").is_some() && leg("2Y").is_some() && leg("6M").is_some() && leg("1W").is_some());
    // the grace is 31d, not a free horizon — but a series only 20d short of its 2Y leg keeps it,
    // which is the monthly-bar slack the guard exists to tolerate
    let near: Vec<NaiveDate> =
        (0..710).map(|k| NaiveDate::from_ymd_opt(2022, 1, 1).unwrap() + Duration::days(k)).collect();
    let nc: Vec<f64> = (0..710).map(|k| 100.0 + k as f64).collect();
    let np = horizon_changes(&near, &nc, None, &BTreeMap::new(), None);
    assert!(np[HORIZONS.iter().position(|(h, _)| *h == "2Y").unwrap()].is_some(), "20d short is within the 31d grace");
    // `life_return` — the value `picks::perf_fill` prints in the rungs the (H-cov) guard above blanks.
    // Same 4y fixture: 100 -> 100*1.0005^1459, i.e. ~+107.4% over ~3.99 years.
    let nominal = life_return(&young, &yc, None).unwrap();
    assert!((nominal - 107.36).abs() < 0.1, "whole-life cumulative return, nominal: {nominal}");
    // ...and REAL when a series is passed: the perf legs it stands in for are deflated (inflation_adjust
    // is on live), so a nominal fill would print ~25pp too high in a real column. 4y at 10%/yr = +46.41%
    // cumulative -> 2.0736/1.4641 - 1.
    let infl: BTreeMap<i32, f64> = (2022..=2025).map(|y| (y, 10.0)).collect();
    let real = life_return(&young, &yc, Some(&infl)).unwrap();
    assert!((real - 41.63).abs() < 0.1, "deflated by the record's own span: {real}");
    // guards mirror life_cagr: junk first close and <6mo of record both yield None, never a number
    assert!(life_return(&young, &vec![0.0; 1460], None).is_none());
    let stub_d = &young[..30];
    assert!(life_return(stub_d, &yc[..30], None).is_none(), "<6mo is not a 'life'");
    // backtest_quote on a synthetic rising MONTHLY series (cadence=12): the cadence window math must
    // still populate volatility (from monthly returns) and put a monotone climber at the top of its
    // range. Guards the long-horizon path against a zero/oversized window silently nulling the metrics.
    let mdates: Vec<NaiveDate> =
        (0..60).map(|m| NaiveDate::from_ymd_opt(2015, 1, 1).unwrap() + chrono::Duration::days(30 * m)).collect();
    let mcloses: Vec<f64> = (0..60).map(|m| 100.0 * 1.01_f64.powi(m)).collect();
    let mq = backtest_quote("X", &mdates, &mcloses, &[], mdates.len() - 1, 12, &BTreeMap::new());
    assert!(mq.volatility_pct.is_some());
    assert!(mq.range_pct > 90.0); // rising every bar -> sits at its range high
    // (#3j) whole-life CAGR over the SAME `[..=as_of]` slice. THE anti-inert assert: while this stayed
    // None, `use_life_cagr` fell back to the leg in every backtest and any measurement of it would have
    // reported "no change" — a broken knob that looks like a safe flip. 1%/bar over 59 30-day bars
    // (~4.85y) compounds to ~12.9%/yr.
    assert!((mq.life_cagr.expect("backtest_quote must fill life_cagr") - 12.88).abs() < 0.1);
    // and the guards that keep an IPO week from annualizing into nonsense
    assert!(life_cagr(&mdates[..3], &mcloses[..3]).is_none(), "under 6 months of history -> None");
    let mut zero_start = mcloses.clone();
    zero_start[0] = 0.0;
    assert!(life_cagr(&mdates, &zero_start).is_none(), "non-positive first close has no growth factor");
    assert!(life_cagr(&[], &[]).is_none());

    // (#80) The 1-month window inside `backtest_quote` is `cadence / 12`, and a DAILY run is the only
    // cadence that can tell that apart from `cadence % 12`: 252 / 12 = 21 sessions, while 252 % 12 = 0
    // clamps to a single bar. Everything else here runs `mq` at cadence 12, where both spellings are 1.
    //
    // Nothing else can catch it either. `max_daily_1m` is read by ONE gate, `growth_max_daily_1m`, which
    // ships at 0.0 — so the field reaches no golden and a collapsed window is invisible end to end.
    // Graded 2026-08-19: `/` -> `%` and `/` -> `*` both MISSED until these asserts existed.
    //
    // TWO spikes, and they are doing different jobs: the window has to be pinned from BOTH sides. The
    // late one fails any arithmetic that shrinks the window (`% 12` -> 0 -> one bar); the early one
    // fails anything that widens it, and `* 12`, `+ 12` and `- 12` all widen past this series and would
    // otherwise land on the same answer as the correct expression.
    let ddates: Vec<NaiveDate> =
        (0..40).map(|d| NaiveDate::from_ymd_opt(2020, 1, 1).unwrap() + chrono::Duration::days(d)).collect();
    let mut dcloses = vec![100.0f64];
    for i in 1..40 {
        let step = match i {
            5 => 1.50,  // +50%, 34 bars from the end: OUTSIDE the window, must not be seen
            30 => 1.20, // +20%, 9 bars from the end: INSIDE it, must be
            _ => 1.001,
        };
        dcloses.push(dcloses[i - 1] * step);
    }
    let dq = backtest_quote("X", &ddates, &dcloses, &[], ddates.len() - 1, 252, &BTreeMap::new());
    let worst = dq.max_daily_1m.expect("a daily backtest_quote must fill max_daily_1m");
    assert!((worst - 20.0).abs() < 1e-6, "the window is the LAST 21 sessions, not one and not all: {worst}");

    // (#88) the anchor map has to REACH the legs, and an empty one has to leave them exactly where they
    // were. Both halves are the finding: the empty map is what every golden and every fitted receipt
    // was measured under, and the live tool has been reading `1M: 15` against that map's default 30 —
    // the same `return_1m` feeding the 1M-knife gate, and the same shape one rung up at `1Y: 182` vs 90.
    // Four years of daily bars with ONE spike placed so the two windows straddle it: averaging over
    // ±30d around the anchor swallows it, an exact-day anchor (`1M: 0`, the spelling ci-settings
    // already ships for `1D`) reads it whole.
    let adates: Vec<NaiveDate> =
        (0..1500).map(|d| NaiveDate::from_ymd_opt(2020, 1, 1).unwrap() + chrono::Duration::days(d)).collect();
    let mut acloses = vec![100.0f64; 1500];
    let anchor = 1500 - 1 - 30; // the bar exactly one 30-day "month" back
    acloses[anchor] = 150.0;
    let i1m = HORIZONS.iter().position(|(l, _)| *l == "1M").unwrap();
    let wide = backtest_quote("X", &adates, &acloses, &[], adates.len() - 1, 252, &BTreeMap::new());
    let tight = backtest_quote("X", &adates, &acloses, &[], adates.len() - 1, 252, &[("1M".to_string(), 0)].into_iter().collect());
    let (w, t) = (wide.perf[i1m].clone().unwrap().1, tight.perf[i1m].clone().unwrap().1);
    assert!(w.abs() < 1.0, "±30d smooths the spike away, which is what the default does: {w}");
    assert!((t + 33.33).abs() < 0.1, "an exact-day anchor lands ON the spike — the map reached the leg: {t}");
    // and OFF is the old hardcoded map, byte for byte, on every leg
    assert_eq!(wide.perf, backtest_quote("X", &adates, &acloses, &[], adates.len() - 1, 252, &BTreeMap::new()).perf);

    // (#41) month-end resampling: a DAILY series and the MONTHLY series of the same months must produce
    // the SAME returns through this one fn — that equality IS the train==serve claim the live skip rests
    // on, since fetch hands it daily bars and the backtest hands it monthly ones.
    let daily_d: Vec<NaiveDate> = (0..3)
        .flat_map(|m| (1..=3).map(move |day| NaiveDate::from_ymd_opt(2024, m + 1, day).unwrap()))
        .collect();
    let daily_c = vec![10.0, 11.0, 12.0, 20.0, 21.0, 24.0, 30.0, 33.0, 36.0]; // month-ends: 12, 24, 36
    let monthly_d: Vec<NaiveDate> = (0..3).map(|m| NaiveDate::from_ymd_opt(2024, m + 1, 28).unwrap()).collect();
    let from_daily = monthly_returns_tail(&daily_d, &daily_c, 36);
    let from_monthly = monthly_returns_tail(&monthly_d, &[12.0, 24.0, 36.0], 36);
    assert_eq!(from_daily.len(), 2, "3 month-ends -> 2 returns");
    assert!((from_daily[0] - 100.0).abs() < 1e-9, "12 -> 24 is +100%");
    assert!((from_daily[1] - 50.0).abs() < 1e-9, "24 -> 36 is +50%");
    assert_eq!(from_daily, from_monthly, "daily and monthly inputs must resample identically");
    assert_eq!(monthly_returns_tail(&daily_d, &daily_c, 1).len(), 1, "k caps the tail");
    assert!(monthly_returns_tail(&[], &[], 36).is_empty());

    // corr_tail: aligned on the tail, 12 months of evidence required, flat series unjudgeable.
    let up: Vec<f64> = (0..14).map(|i| i as f64).collect();
    let down: Vec<f64> = (0..14).map(|i| -(i as f64)).collect();
    assert!((corr_tail(&up, &up).unwrap() - 1.0).abs() < 1e-9);
    assert!((corr_tail(&up, &down).unwrap() + 1.0).abs() < 1e-9);
    assert!(corr_tail(&up[..11], &down[..11]).is_none(), "under 12 overlapping months = no verdict");
    assert!(corr_tail(&[7.0; 13], &up).is_none(), "a flat series correlates with nothing");

    // decorrelate_keep: drops the SECOND COPY of a bet, keeps everything else. The comparison is on
    // SIGNED rho, not |rho| — an anti-correlated name is diversification, not redundancy, so `down`
    // survives next to `up`. That is deliberate and matches the CORR-CAP probe that measured the knee.
    let twin: Vec<f64> = up.iter().map(|x| x * 2.0 + 1.0).collect(); // rho(up, twin) == 1
    let zigzag: Vec<f64> = (0..14).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect(); // rho ~ -0.12
    let trails: Vec<&[f64]> = vec![&up, &twin, &down, &zigzag, &[]];
    assert_eq!(
        decorrelate_keep(&trails, 10, 0.9),
        vec![0, 2, 3, 4],
        "twin dropped; anti-correlated, uncorrelated and no-trail rows all kept"
    );
    assert_eq!(decorrelate_keep(&trails, 2, 0.9), vec![0, 2], "n bounds how many survive");
    // A name with NO trail is unjudgeable, and an unjudgeable pair never blocks — a brake may only act
    // on evidence. Two empty trails must therefore BOTH survive, not collapse into one.
    assert_eq!(decorrelate_keep(&[&[], &[]], 10, 0.9), vec![0, 1]);

    // (#75) pct_floor: the cut point BOTH value-brake sites compare against. Truncating index into the
    // sorted cohort, so p% of the cohort sits strictly below the returned floor and the floor itself is
    // KEPT by callers (`< floor` rejects) — the boundary `drop_bottom_book` already uses.
    let cohort = || vec![50.0, 10.0, 40.0, 20.0, 30.0]; // deliberately unsorted: the fn owns the sort
    assert_eq!(pct_floor(cohort(), 40.0), Some(30.0), "40% of 5 = index 2 of the sorted cohort");
    assert_eq!(pct_floor(cohort(), 25.0), Some(20.0), "1.25 truncates to index 1, never rounds up");
    assert_eq!(pct_floor(cohort(), 100.0), Some(50.0), "index clamps to the last element, no panic");
    // Both off-switches return None, and they mean the same thing to a caller: cut nothing. An empty
    // cohort must NOT be read as "everything is below the floor" — that would empty the table on the
    // one input where there is no evidence at all.
    assert_eq!(pct_floor(cohort(), 0.0), None, "0 = off");
    assert_eq!(pct_floor(cohort(), -5.0), None, "a negative percentile is off, not an inverted gate");
    assert_eq!(pct_floor(Vec::new(), 40.0), None, "nobody carries the factor -> no floor, cut nothing");
    // total_cmp, not partial_cmp().unwrap(): one NaN in a cohort must not panic the whole run.
    assert_eq!(pct_floor(vec![f64::NAN, 1.0, 2.0], 50.0), Some(2.0), "NaN sorts last and cannot panic");
    // fund_as_of point-in-time join: latest row FILED on/before the cutoff, NEVER a future filing
    // (the look-ahead guard). Rows out of order on purpose to prove order-independence.
    let frows = vec![
        FundRow { filed: NaiveDate::from_ymd_opt(2022, 2, 1).unwrap(), revenue: Some(200.0), roe: Some(18.0), ..Default::default() },
        FundRow { filed: NaiveDate::from_ymd_opt(2020, 2, 1).unwrap(), revenue: Some(100.0), ..Default::default() },
        FundRow { filed: NaiveDate::from_ymd_opt(2021, 2, 1).unwrap(), revenue: Some(150.0), roe: Some(12.0), ..Default::default() },
    ];
    // cutoff between the 2021 and 2022 filings -> sees 2021, NOT the unfiled 2022 (no look-ahead)
    assert_eq!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2021, 6, 1).unwrap()).unwrap().revenue, Some(150.0));
    assert_eq!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2023, 1, 1).unwrap()).unwrap().revenue, Some(200.0)); // after all -> latest
    assert!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2019, 1, 1).unwrap()).is_none()); // before any filing -> nothing public
    assert_eq!(fund_as_of(&frows, NaiveDate::from_ymd_opt(2021, 2, 1).unwrap()).unwrap().revenue, Some(150.0)); // exact filing date visible (<=)
    // fund_factors: revenue 100 -> 200 over 2y (filed 2020 vs 2022) = ~41.4%/yr CAGR, all as-of (no
    // look-ahead). margin/eps None here (rows carry only revenue) -> a premium/absent field stays neutral.
    let ff = fund_factors(&frows, NaiveDate::from_ymd_opt(2022, 3, 1).unwrap(), 2);
    assert!((ff.rev_cagr.unwrap() - 41.42).abs() < 0.1); // sqrt(2)-1 ≈ 41.4%/yr
    assert!(ff.op_margin.is_none() && ff.eps_growth.is_none()); // absent fields -> None, never a garbage value
    assert!(fund_factors(&frows, NaiveDate::from_ymd_opt(2020, 6, 1).unwrap(), 2).rev_cagr.is_none()); // no row 2y before -> None
    // as-of roe: the level rides the same fund_as_of look-ahead guard — a cutoff between the 2021
    // and 2022 filings sees 12.0, NEVER the unfiled 18.0; after both, the latest level.
    assert_eq!(ff.roe, Some(18.0));
    assert_eq!(fund_factors(&frows, NaiveDate::from_ymd_opt(2021, 6, 1).unwrap(), 2).roe, Some(12.0));
    // default_anchor_half: window widens with horizon length; 1D exact
    assert_eq!(default_anchor_half(1), 0);
    assert_eq!(default_anchor_half(7), 7);
    assert_eq!(default_anchor_half(365), 90);
    assert_eq!(default_anchor_half(3650), 365);
    // real_pct: 0% cumulative inflation = unchanged; flat nominal under +10% inflation = ~-9% real
    assert_eq!(real_pct(100.0, 0.0), 100.0);
    assert!((real_pct(0.0, 10.0) - (-9.0909091)).abs() < 1e-4);
    assert!((real_pct(50.0, 10.0) - 36.3636363).abs() < 1e-4); // +50% nominal, +10% infl -> ~+36% real
    assert_eq!(slice_since(&ds, &cs, 1), vec![20.0, 30.0]);

    // intraday: bar-count back. 7 bars, last=110. 1 bar back=105 -> +4.76%; 6 bars back=100 -> +10%
    let ics = vec![100.0, 101.0, 102.0, 103.0, 104.0, 105.0, 110.0];
    assert!((intraday_pct(&ics, 1).unwrap() - (110.0 - 105.0) / 105.0 * 100.0).abs() < 1e-9);
    assert!((intraday_pct(&ics, 6).unwrap() - 10.0).abs() < 1e-9);
    assert_eq!(intraday_pct(&ics, 12), None); // only 7 bars -> 12 back unavailable
    assert_eq!(intraday_changes(&ics)[2], None); // 12h slot n/a (short history)
    assert!(intraday_changes(&ics)[0].is_some()); // 1h slot present

    // turnover: avg of close*volume over last n, zero-volume days skipped; None if all zero/empty
    assert_eq!(avg_turnover(&[10.0, 20.0], &[100.0, 200.0], 30), Some((1000.0 + 4000.0) / 2.0));
    assert_eq!(avg_turnover(&[10.0, 20.0], &[0.0, 200.0], 30), Some(4000.0)); // zero-vol day skipped
    assert_eq!(avg_turnover(&[], &[], 30), None);
    assert_eq!(avg_turnover(&[10.0], &[0.0], 30), None); // no usable turnover
    // avg_volume: crypto notional volume used raw (no ×close), zero days skipped
    assert_eq!(avg_volume(&[100.0, 0.0, 300.0], 30), Some(200.0));
    assert_eq!(avg_volume(&[0.0], 30), None);

    // volatility: stdev of daily % returns. Steady +1%/day -> 0 stdev; alternating moves -> >0
    assert!(volatility_pct(&[100.0, 101.0, 102.01, 103.0301], 30).unwrap() < 1e-9); // ~0 (float dust)
    assert!(volatility_pct(&[100.0, 110.0, 100.0, 110.0], 30).unwrap() > 0.0);
    assert_eq!(volatility_pct(&[100.0], 30), None); // too few sessions

    // (r39) downside deviation: the SAME walk as volatility_pct with only the down-moves counted.
    // A monotonic riser has real vol (its up-steps vary) but zero downside — that gap IS the term's
    // whole thesis, so pin it: if the filter ever stops discriminating, these two collapse together.
    let riser = [100.0, 110.0, 115.0, 130.0];
    assert!(volatility_pct(&riser, 30).unwrap() > 0.0);
    assert_eq!(downside_deviation_pct(&riser, 30), Some(0.0)); // never down -> no downside measured
    // one −10% move in 3 periods: RMS over ALL periods = sqrt(100/3), NOT sqrt(100/1) — the
    // all-periods denominator is what stops a name with few down bars flattering itself.
    let dip = [100.0, 110.0, 99.0, 108.9];
    assert!((downside_deviation_pct(&dip, 30).unwrap() - (100.0_f64 / 3.0).sqrt()).abs() < 1e-9);
    assert!(downside_deviation_pct(&dip, 30).unwrap() < volatility_pct(&dip, 30).unwrap());
    assert_eq!(downside_deviation_pct(&[100.0], 30), None); // same too-few guard as its twin

    // (#97) the same asset, measured on monthly bars, prints ~sqrt(21) = 4.6x the daily figure — and
    // every knob reading this field is an ABSOLUTE threshold, so that scale is not a wash.
    let v = 6.0; // a monthly-bar stdev
    assert!((daily_equivalent(v, 12, true) - v / (252.0_f64 / 12.0).sqrt()).abs() < 1e-12);
    assert!((daily_equivalent(v, 12, true) - 1.309).abs() < 1e-3, "{}", daily_equivalent(v, 12, true));
    // the finding's ~4.6x, stated as the ratio it actually is.
    assert!(((v / daily_equivalent(v, 12, true)) - 4.583).abs() < 1e-3);
    // LIVE IS BIT-IDENTICAL AT EITHER SETTING: cadence 252 gives a factor of exactly 1.0.
    assert_eq!(daily_equivalent(v, 252, true), v);
    assert_eq!(daily_equivalent(v, 252, false), v);
    // OFF leaves a non-daily run untouched too — that is the shipped lane the goldens pin.
    assert_eq!(daily_equivalent(v, 12, false), v);
    // what it costs the cap: a 15%/yr CAGR on this name reads sharpe 2.5 on raw monthly bars and
    // 11.5 once restated — the first is nowhere near `sharpe_cap: 15`, the second is close enough
    // that the live path clamps names the run that fitted the cap never clamped.
    assert!((15.0 / v - 2.5).abs() < 1e-9);
    assert!((15.0 / daily_equivalent(v, 12, true) - 11.456).abs() < 1e-3);

    // (P4) worst — largest — single-bar move. Not a dispersion: `riser` has three EQUAL-looking steps
    // whose percentages differ, and the answer is the biggest of them, +13.0434…%, not their spread.
    assert!((max_daily_pct(&riser, 30).unwrap() - (15.0 / 115.0 * 100.0)).abs() < 1e-9);
    // ONE return is enough (a stdev needs two), so the guard is emptiness, not `len() < 2`.
    assert_eq!(max_daily_pct(&[100.0, 105.0], 30), Some(5.0));
    assert_eq!(max_daily_pct(&[100.0], 30), None); // one close = zero returns
    assert_eq!(max_daily_pct(&[], 30), None);
    // the window really is the LAST n returns: n=1 must not see the +50% that fell out of it
    assert_eq!(max_daily_pct(&[100.0, 150.0, 153.0], 1), Some(2.0));
    // a name that only fell still HAS a biggest move — the least-negative one, not None and not 0
    assert!((max_daily_pct(&[100.0, 90.0, 81.0], 30).unwrap() + 10.0).abs() < 1e-9);

    assert_eq!(pct_cell(Some(&("€10.00".to_string(), 5.0))), "+5.0%");
    assert_eq!(pct_cell(None), "n/a");

    // dividends: sum within window; None when history doesn't cover it; EUR via rate
    let ddates = vec![
        NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), // ~2y of history
    ];
    let divs = vec![
        (NaiveDate::from_ymd_opt(2022, 3, 1).unwrap(), 1.0),
        (NaiveDate::from_ymd_opt(2023, 9, 1).unwrap(), 2.0),
    ];
    assert_eq!(dividends_in_window(&divs, &ddates, 365), Some(2.0)); // only the 2023 one is <1y back
    assert_eq!(dividends_in_window(&divs, &ddates, 1825), None); // 5y not covered by ~2y history
    let sums = dividend_sums(&divs, &ddates, Some(2.0)); // rate 2.0 -> EUR
    assert_eq!(sums[0], Some(4.0)); // 1Y: 2.0 * 2.0
    assert_eq!(sums[1], None); // 5Y short history
    // yield: 1Y €4 paid / 1yr / €40 price = 10%; 5Y short -> None; no price -> all None
    let yields = dividend_yields(&sums, Some(40.0));
    assert!((yields[0].unwrap() - 10.0).abs() < 1e-9);
    assert_eq!(yields[1], None);
    assert_eq!(dividend_yields(&sums, None)[0], None);

    assert_eq!(name_of(&serde_json::json!({"shortName": "Apple Inc."}), "AAPL"), "Apple Inc.");
    assert_eq!(name_of(&serde_json::json!({"longName": "NVIDIA Corp"}), "NVDA"), "NVIDIA Corp");
    assert_eq!(name_of(&serde_json::json!({}), "BTC-USD"), "BTC-USD");
    assert_eq!(name_of(&Value::Null, "MSFT"), "MSFT");
    // both present -> longName wins (the real ETF name, not the truncated registrant shortName)
    assert_eq!(
        name_of(&serde_json::json!({"shortName": "ISHARES III PLC", "longName": "iShares Core MSCI World UCITS ETF"}), "IWDA.L"),
        "iShares Core MSCI World UCITS ETF"
    );

    let one = Some(1.0);
    assert_eq!(ca_base_rate(2.1, one, 0.0, Some(2.5)), Some(2.1)); // Série F
    assert!((ca_base_rate(2.1, one, 1.0, Some(3.5)).unwrap() - 3.1).abs() < 1e-9); // Série E
    assert_eq!(ca_base_rate(3.0, one, 1.0, Some(3.5)), Some(3.5)); // capped
    assert_eq!(ca_base_rate(-2.0, one, 1.0, Some(3.5)), Some(0.0)); // floored
    // Série B: multiplicative (`0,60 × TBA`) and uncapped. A spread/cap pair cannot express this,
    // which is why `mult` exists.
    assert!((ca_base_rate(2.49, Some(0.60), 0.0, None).unwrap() - 1.494).abs() < 1e-9);
    // Série A: IGCP publishes no formula -> None, never a substituted number.
    assert_eq!(ca_base_rate(2.49, None, 0.0, None), None);

    // (CA) THE RECEIPT for the multiplicative base: IGCP's own 2026 Série B sheet publishes
    // 3.30020% / 3.19460% / 3.18260% gross, premium included. Those must fall out of
    // `0.60 × E3 + 2.00` at the Euribor 3M actually observed in Feb-Apr 2026. If someone "tidies"
    // mult away into a spread, these three stop reconciling and this test says so.
    for (e3, published) in [(2.167, 3.30020), (1.991, 3.19460), (1.971, 3.18260)] {
        let rate = ca_base_rate(e3, Some(0.60), 0.0, None).unwrap() + ca_premium(&[(1, 2.00)], 1);
        assert!((rate - published).abs() < 5e-4, "Série B {e3} -> {rate}, IGCP says {published}");
    }

    // (CA) premium bands: the last band that has started wins; year 1 is bare base.
    let f_bands = &[(2, 0.25), (6, 0.50), (10, 1.00), (12, 1.50), (14, 1.75)][..];
    let got: Vec<f64> = [1, 2, 5, 6, 9, 10, 11, 12, 13, 14, 15].iter().map(|y| ca_premium(f_bands, *y)).collect();
    assert_eq!(got, vec![0.0, 0.25, 0.25, 0.50, 0.50, 1.00, 1.00, 1.50, 1.50, 1.75, 1.75]);
    // the two-tier model this replaced returned 0.50 from year 6 on, so year 10+ was understated
    // by up to 1.25pp/yr — that is the whole reason `premium` is a slice.
    assert_eq!(ca_premium(&[(2, 0.50), (3, 0.75), (4, 1.00), (8, 1.25), (9, 1.50), (10, 2.50)], 10), 2.50);
    assert_eq!(ca_premium(&[], 5), 0.0); // Série A has no published ladder -> no premium invented

    // CA cumulative gain: yr1 = base only; compounds with premium thereafter
    let f = &[(2, 0.25), (6, 0.50)][..];
    assert!((ca_cumulative_gain(2.1, f, 1) - 2.1).abs() < 1e-9); // yr1 = base
    // 2yr: (1.021)(1.0235) - 1 = 0.0449935 -> 4.49935%
    assert!((ca_cumulative_gain(2.1, f, 2) - 4.49935).abs() < 1e-4);
    assert_eq!(ca_cumulative_gain(2.0, &[(2, 0.5), (6, 1.0)], 0), 0.0); // no holding -> no gain

    // (CA) E is the CONTROL for the band rewrite: its last band runs to the end, so every cell it
    // printed before must be unchanged. F is the one that moves — its ladder climbs to +1.75 and
    // the old pair stopped at +0.50, so only its 20Y was wrong. At E3 2.49 (base 3.49 / 2.49):
    // The two-tier model the bands replaced, verbatim, so the rewrite can be diffed against it
    // instead of against display strings — hardcoding "+38.0%" would only pin whatever Euribor
    // happened to print that day, and the cell rounds either side of .05 depending on it.
    let two_tier = |base: f64, early: f64, late: f64, years: i64| {
        let mut factor = 1.0;
        for y in 1..=years {
            let p = if y >= 6 { late } else if y >= 2 { early } else { 0.0 };
            factor *= 1.0 + (base + p) / 100.0;
        }
        (factor - 1.0) * 100.0
    };
    // E is the CONTROL: its last band runs to the end, so the rewrite must be a no-op for it at
    // EVERY base and horizon, not just today's.
    let e = &CA_SERIES.iter().find(|s| s.name == "E").unwrap().premium;
    for base in [0.0, 1.25, 3.49, 3.5] {
        for years in [1, 2, 5, 8, 20, 30] {
            let (new, old) = (ca_cumulative_gain(base, e, years), two_tier(base, 0.50, 1.00, years));
            assert!((new - old).abs() < 1e-9, "E moved at base {base} / {years}Y: {new} vs {old}");
        }
    }
    // F is the one that CHANGES, and only where the real ladder outgrows two tiers: identical
    // through year 9, strictly higher from year 10 once +1.00/+1.50/+1.75 kick in.
    let ff = &CA_SERIES.iter().find(|s| s.name == "F").unwrap().premium;
    for years in 1..=9 {
        let (new, old) = (ca_cumulative_gain(2.49, ff, years), two_tier(2.49, 0.25, 0.50, years));
        assert!((new - old).abs() < 1e-9, "F must be unchanged at {years}Y: {new} vs {old}");
    }
    for years in 10..=20 {
        assert!(ca_cumulative_gain(2.49, ff, years) > two_tier(2.49, 0.25, 0.50, years), "F must rise at {years}Y");
    }
    // and the SIZE of it, near the base the shipped table was printed on. Deliberately a gap and
    // not two display strings: the printed base is rounded, so "+77.6%" vs "+77.7%" turns on a
    // Euribor decimal the table never shows. The gap is ~21 points because the old model froze F's
    // premium at +0.50 for the decade it actually spends climbing to +1.75.
    let gap = ca_cumulative_gain(2.49, ff, 20) - two_tier(2.49, 0.25, 0.50, 20);
    assert!((20.0..23.0).contains(&gap), "F 20Y understated by ~21 points, got {gap}");
    // D and E are the same product by law (Portaria 329-A/2017 kept D's financial conditions);
    // only the subscription channel differed. Divergence here means an unsourced edit.
    let d = &CA_SERIES.iter().find(|s| s.name == "D").unwrap().premium;
    assert_eq!(d, e);

    let series: BTreeMap<i32, f64> = [(2018, 1.0), (2019, 2.0), (2020, 3.0)].into();
    let (ly, lv, a10, a30) = inflation_summary(&series);
    assert_eq!(ly, Some(2020));
    assert_eq!(lv, Some(3.0));
    assert!((a10.unwrap() - 2.0).abs() < 1e-9 && (a30.unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(inflation_summary(&BTreeMap::new()), (None, None, None, None));

    // compounded: last 2 = (1.02)(1.03)-1 = 5.06%; exactly-len = full product
    assert!((inflation_compounded(&series, 2).unwrap() - 5.06).abs() < 1e-9);
    assert!((inflation_compounded(&series, 3).unwrap() - (1.01 * 1.02 * 1.03 - 1.0) * 100.0).abs() < 1e-9);
    // 1yr slack (level->YoY always loses the earliest in-window year): a 4Y ask off 3 rates renders;
    // >=2 short -> None, so a far-too-long horizon isn't faked from a short span
    assert!((inflation_compounded(&series, 4).unwrap() - (1.01 * 1.02 * 1.03 - 1.0) * 100.0).abs() < 1e-9);
    assert_eq!(inflation_compounded(&series, 5), None); // 3 rates, ask 5 -> n/a
    assert_eq!(inflation_compounded(&series, 10), None);
    assert_eq!(inflation_compounded(&BTreeMap::new(), 5), None);

    // BLS CPI-U parse: index level -> YoY %. 2025 = (103/100-1)*100 = 3%; 2024 has no 2023
    // pair -> absent; M13 (annual avg) skipped without crashing.
    let bls = serde_json::json!({"Results":{"series":[{"data":[
        {"year":"2024","period":"M12","value":"100.0"},
        {"year":"2025","period":"M12","value":"103.0"},
        {"year":"2025","period":"M13","value":"999.0"}
    ]}]}});
    let us = parse_bls_cpi(&bls);
    assert!((us[&2025] - 3.0).abs() < 1e-9);
    assert!(!us.contains_key(&2024)); // no prior-year same month to compare
    assert_eq!(parse_bls_cpi(&serde_json::json!({})), BTreeMap::new());

    assert_eq!(fmt_duration(Duration::days(14)), "2w");
    assert_eq!(fmt_duration(Duration::days(400)), "1Y");
    assert_eq!(fmt_duration(Duration::seconds(90)), "1m");

    let dd = vec![
        NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
    ];
    assert_eq!(trend_streak(&dd, &[10.0, 11.0, 12.0]), ("↑", "2d".to_string(), 2));
    assert_eq!(trend_streak(&dd, &[12.0, 11.0, 10.0]).0, "↓");
    assert_eq!(trend_streak(&dd, &[10.0, 10.0, 10.0]), ("→", "0s".to_string(), 0));
    assert_eq!(trend_streak(&dd[..1], &[10.0]), ("→", "0s".to_string(), 0));

    assert_eq!(extreme_flags(&[1.0, 2.0, 3.0], 0.001), (true, false));
    assert_eq!(extreme_flags(&[3.0, 2.0, 1.0], 0.001), (false, true));
    assert_eq!(extreme_flags(&[2.0, 1.0, 3.0, 2.0], 0.001), (false, false));
    assert_eq!(extreme_flags(&[], 0.001), (false, false));

    // money formatting (Python {:,.2f})
    assert_eq!(fmt_money2(1234567.5), "1,234,567.50");
    assert_eq!(fmt_money2(12.3), "12.30");

    // PT inflation parse: BPstat index is a JSON ARRAY (the bug was only handling objects)
    let pt = serde_json::json!({
        "dimension": {"reference_date": {"category": {"index": ["2024-11-30", "2024-12-31", "2025-01-31"]}}},
        "value": [2.1, 2.4, 2.6]
    });
    let s = parse_pt_series(&pt);
    assert_eq!(s.get(&2024), Some(&2.4)); // last month of 2024 wins
    assert_eq!(s.get(&2025), Some(&2.6));
    // object-form index (sorted by position) still works
    let pt_obj = serde_json::json!({
        "dimension": {"reference_date": {"category": {"index": {"2024-12-31": 1, "2024-11-30": 0}}}},
        "value": [2.1, 2.4]
    });
    assert_eq!(parse_pt_series(&pt_obj).get(&2024), Some(&2.4));
    assert!(parse_pt_series(&Value::Null).is_empty());

    // (r17) Eurostat HICP parse: sparse {position: rate} value keyed off the time index; last
    // month of a year wins; junk -> empty. One parser serves the COICOP-2018 successor and the
    // frozen pre-2026 archive.
    let eu = serde_json::json!({
        "dimension": {"time": {"category": {"index": {"2025-11": 0, "2025-12": 1, "2026-06": 2}}}},
        "value": {"0": 2.4, "1": 2.3, "2": 2.9}
    });
    let s = parse_eurostat_hicp(&eu);
    assert_eq!(s.get(&2025), Some(&2.3)); // December wins the year
    assert_eq!(s.get(&2026), Some(&2.9)); // partial year = newest month YoY
    let eu_hole = serde_json::json!({
        "dimension": {"time": {"category": {"index": {"2025-12": 0, "2026-01": 1}}}},
        "value": {"0": 2.3}
    });
    assert_eq!(parse_eurostat_hicp(&eu_hole).get(&2026), None); // sparse hole skipped, not zeroed
    assert!(parse_eurostat_hicp(&Value::Null).is_empty());

    // (r17) archive merge: the successor wins overlapping years, the archive contributes only
    // its earlier tail, and an EMPTY live series stays empty — an outage must reach the
    // degraded-feeds line, the frozen archive must never mask a dead feed.
    let old: BTreeMap<i32, f64> = [(1997, 1.7), (2000, 2.9), (2025, 9.9)].into();
    let new: BTreeMap<i32, f64> = [(2000, 2.5), (2025, 2.3), (2026, 2.9)].into();
    let merged = merge_infl_archive(old.clone(), new.clone());
    assert_eq!(merged.get(&1997), Some(&1.7)); // tail from the archive
    assert_eq!(merged.get(&2025), Some(&2.3)); // successor wins the overlap
    assert_eq!(merged.get(&2026), Some(&2.9));
    assert!(merge_infl_archive(old, BTreeMap::new()).is_empty()); // outage guard
    assert_eq!(merge_infl_archive(BTreeMap::new(), new.clone()), new); // no archive = passthrough

    // (r18) staleness tripwire: frozen-not-empty feed self-reports from March of the next
    // year; healthy feed silent; Jan/Feb grace (prior-year max still legitimate); empty
    // stays None (the ERROR path owns empty).
    let day = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
    let frozen: BTreeMap<i32, f64> = [(2024, 2.4), (2025, 2.3)].into();
    let healthy: BTreeMap<i32, f64> = [(2025, 2.3), (2026, 2.9)].into();
    assert_eq!(infl_series_stale(&frozen, day(2026, 7, 19)), Some(2025)); // r17 blindness caught
    assert_eq!(infl_series_stale(&healthy, day(2026, 7, 19)), None);
    assert_eq!(infl_series_stale(&frozen, day(2026, 2, 28)), None); // early-year grace
    assert_eq!(infl_series_stale(&frozen, day(2026, 3, 1)), Some(2025)); // grace ends in March
    assert_eq!(infl_series_stale(&BTreeMap::new(), day(2026, 7, 19)), None);
    }

    /// `ca_premium_range` renders the permanence ladder for the footer. All three arms, because the
    /// slice patterns are the whole function: a one-band series must print the flat form, not a
    /// `lo→hi` range against itself, and a laddered one must show FIRST→LAST rather than any
    /// interior band.
    #[test]
    fn ca_premium_range_covers_its_three_arms() {
        assert_eq!(ca_premium_range(&[]), "—");
        assert_eq!(ca_premium_range(&[(1, 2.0)]), "+2.00%");
        assert_eq!(ca_premium_range(&[(1, 0.5), (3, 1.0), (5, 2.5)]), "+0.50→+2.50%");
    }
}
