//! (#538) THE SOCIAL-ARBITRAGE HEURISTIC (SOC): a second buy heuristic that orders the page's Social
//! table, beside the 20y one that orders everything else.
//!
//! THE PLAYBOOK it encodes: notice a product or behaviour trend among consumers before the market
//! prices it, buy the listed company it is material to, and sell once the trend is mainstream, so a
//! hold of about one quarter. The published, systematic analogues predict about that far ahead and no
//! further: product-search volume (Da, Engelberg & Gao, "In Search of Attention", JF 2011) and store
//! foot traffic (Froot et al., JFE 2017). It is NOT a 20-year-hold edge, which is why it never feeds the
//! lanes, and no backtest can grade it: there is no point-in-time history of the sources.
//!
//! SOC = breadth × fresh × material × unpriced, one number per Social row:
//! - breadth: how many sources name it (the SOURCES cell's `; `-joined entries);
//! - fresh: `0.5^(days since NEWEST / half_life_days)`, measured from the payload's `generated` date;
//! - material: `1 / (1 + max(0, log10(MCAP / mcap_pivot_eur)))`: a trend moves a €1B company, not a
//!   €3T one;
//! - unpriced: `1 − clamp(1M % / runup_cap_pct, 0, 1)`: a name already up that much has been noticed.
//!   The loud sources (`Reddit`, `Benzinga buzz`) multiply it by `loud_penalty`, because a crowd that
//!   is already talking is the market noticing.
//!
//! ponytail: every prior here is hand-set, never fitted, and only track `soct` grades it (SOC top 10 vs
//! the same table's 20y top 10, forward). Refit them only from that row.
//!
//! CONFIG: the top-level `social_heuristic:` section. Its four SOC keys default to OFF (half_life_days
//! 0), and every `buy_heuristic` key may appear there too: an unset one inherits `buy_heuristic`'s value
//! and a set one overrides it, for the Social table's SCORE, MISSES and 20y tiebreak ONLY.

use crate::config::{merge_yaml, BuyHeuristic};
use chrono::NaiveDate;

/// The SOC-only keys of `social_heuristic:`; every other key there is a `buy_heuristic` override.
const KEYS: [&str; 4] = ["half_life_days", "mcap_pivot_eur", "runup_cap_pct", "loud_penalty"];

/// How many names each journalled book holds, the top of the 20y and the SOC orders.
pub const BOOK: usize = 10;

/// The sources that are the crowd already talking, spelled as `fetch::soc_table` writes them.
const LOUD: [&str; 2] = ["Reddit", "Benzinga buzz"];

pub const SOC_HELP: &str = "(#538) The social-arbitrage score, which orders this table: sources × freshness × materiality × \
     unpriced. Sources counts the SOURCES entries; freshness halves every `half_life_days` since NEWEST; materiality \
     falls with each 10× of market cap over `mcap_pivot_eur`; unpriced falls to 0 as the 1M run-up nears \
     `runup_cap_pct` and is cut by `loud_penalty` when Reddit or Benzinga buzz is a source. A short-horizon \
     (about one quarter) attention signal, not a 20y one: it never reaches the lanes or BUY NOW. Graded forward \
     by track `soct`. NOT advice.";

pub const RANK_20Y_HELP: &str = "(#538) This row's position under the original 20y buy heuristic (the SCORE order). RANK follows SOC.";

/// (#538) The resolved `social_heuristic:` section. `buy` is `buy_heuristic` with this section's
/// overrides applied, and only the Social table reads it.
#[derive(Debug, Clone)]
pub struct SocialHeuristic {
    pub half_life_days: f64, // days for the freshness factor to halve. 0 = OFF (DEFAULT): no SOC, no RANK-20Y, the 20y order
    pub mcap_pivot_eur: f64, // market cap (EUR) where materiality starts falling (DEFAULT 2e9)
    pub runup_cap_pct: f64,  // 1M % at which a name counts as fully priced (DEFAULT 50). 0 = no run-up factor
    pub loud_penalty: f64,   // unpriced multiplier when a loud source names the row (DEFAULT 0.5). 1 = none
    pub buy: BuyHeuristic,
}

impl SocialHeuristic {
    pub fn on(&self) -> bool {
        self.half_life_days > 0.0
    }
}

/// (#538) The two orders' top [`BOOK`] as `(ticker, close EUR)`, for the journal. Empty when SOC is off.
#[derive(Debug, Default, PartialEq)]
pub struct SocBook {
    pub new: Vec<(String, Option<f64>)>,
    pub old: Vec<(String, Option<f64>)>,
}

/// Resolve `social_heuristic:` (`raw`, as written) over `buy`. A bad SOC value or a key `BuyHeuristic`
/// does not know is an error, so a typo fails at startup like it does in `buy_heuristic:`.
pub fn resolve(buy: &BuyHeuristic, raw: &serde_yaml::Value) -> Result<SocialHeuristic, String> {
    let mut rest = match raw {
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        serde_yaml::Value::Mapping(m) => m.clone(),
        _ => return Err("social_heuristic: expected a mapping".into()),
    };
    let mut knob = |k: &str, default: f64| match rest.remove(k) {
        None | Some(serde_yaml::Value::Null) => Ok(default),
        Some(v) => v.as_f64().ok_or_else(|| format!("social_heuristic.{k}: expected a number")),
    };
    let [half_life_days, mcap_pivot_eur, runup_cap_pct, loud_penalty] = [(KEYS[0], 0.0), (KEYS[1], 2e9), (KEYS[2], 50.0), (KEYS[3], 0.5)].map(|(k, d)| knob(k, d));
    let mut merged = serde_yaml::to_value(buy).map_err(|e| format!("social_heuristic: {e}"))?;
    merge_yaml(&mut merged, serde_yaml::Value::Mapping(rest));
    Ok(SocialHeuristic {
        half_life_days: half_life_days?,
        mcap_pivot_eur: mcap_pivot_eur?,
        runup_cap_pct: runup_cap_pct?,
        loud_penalty: loud_penalty?,
        buy: serde_yaml::from_value(merged).map_err(|e| format!("social_heuristic: {e}"))?,
    })
}

/// A NEWEST cell as a day: `YYYY-MM-DD`, or `YYYY-Qn` read as the quarter's last day.
fn newest_day(s: &str) -> Option<NaiveDate> {
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    let (y, q) = s.split_once("-Q")?;
    let (y, q): (i32, u32) = (y.parse().ok()?, q.parse().ok()?);
    let next = match q {
        1..=3 => NaiveDate::from_ymd_opt(y, q * 3 + 1, 1),
        4 => NaiveDate::from_ymd_opt(y + 1, 1, 1),
        _ => None,
    };
    next?.pred_opt()
}

/// (#538) One Social row's SOC (module doc), or None when SOC is off.
pub fn score(sources: &str, newest: &str, mcap_eur: Option<f64>, mom_pct: Option<f64>, asof: Option<NaiveDate>, k: &SocialHeuristic) -> Option<f64> {
    if !k.on() {
        return None;
    }
    let entries: Vec<&str> = sources.split("; ").map(str::trim).filter(|e| !e.is_empty()).collect();
    // ponytail: an unknown date reads half fresh, a neutral guess; fix the source if many rows hit it
    let fresh = asof.zip(newest_day(newest)).map_or(0.5, |(a, d)| 0.5f64.powf((a - d).num_days().max(0) as f64 / k.half_life_days));
    // ponytail: an unknown market cap reads as 10× the pivot (material 0.5), a neutral guess
    let pivot = k.mcap_pivot_eur.max(1.0);
    let mcap = mcap_eur.filter(|m| *m > 0.0).unwrap_or(pivot * 10.0);
    let material = 1.0 / (1.0 + (mcap / pivot).log10().max(0.0));
    let run = if k.runup_cap_pct > 0.0 { (mom_pct.unwrap_or(0.0) / k.runup_cap_pct).clamp(0.0, 1.0) } else { 0.0 };
    let loud = if entries.iter().any(|e| LOUD.contains(e)) { k.loud_penalty } else { 1.0 };
    Some(entries.len() as f64 * fresh * material * (1.0 - run) * loud)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> SocialHeuristic {
        let raw = serde_yaml::from_str("half_life_days: 30\nmcap_pivot_eur: 2000000000\n").unwrap();
        resolve(&BuyHeuristic::default(), &raw).unwrap()
    }

    #[test]
    fn soc_multiplies_breadth_fresh_material_unpriced() {
        let (k, asof) = (on(), NaiveDate::from_ymd_opt(2026, 10, 10));
        let s = |src: &str, newest: &str, mcap: Option<f64>, mom: Option<f64>| score(src, newest, mcap, mom, asof, &k).unwrap();
        assert_eq!(s("ARK; insiders; hand list", "2026-10-10", Some(1e9), Some(0.0)), 3.0, "breadth counts entries");
        assert_eq!(s("ARK", "2026-09-10", Some(1e9), Some(-5.0)), 0.5, "30 days = one half-life; a fall is not a run-up");
        assert_eq!(s("ARK", "2026-10-12", Some(1e9), None), 1.0, "a future date is fresh, never fresher");
        assert_eq!(s("ARK", "2026-10-10", Some(2e10), Some(0.0)), 0.5, "10× the pivot halves materiality");
        assert_eq!(s("ARK", "2026-10-10", None, Some(0.0)), 0.5, "unknown cap reads 10× the pivot");
        assert_eq!(s("ARK", "2026-10-10", Some(1e9), Some(25.0)), 0.5, "half the run-up cap is half priced");
        assert_eq!(s("ARK", "2026-10-10", Some(1e9), Some(80.0)), 0.0, "past the cap is fully priced");
        assert_eq!(s("ARK; Reddit", "2026-10-10", Some(1e9), Some(0.0)), 1.0, "a loud source halves unpriced");
        assert_eq!(s("", "2026-10-10", Some(1e9), Some(0.0)), 0.0);
        assert_eq!(score("ARK", "bad", Some(1e9), Some(0.0), asof, &k), Some(0.5), "unknown date reads half fresh");
        assert_eq!(score("ARK", "2026-10-10", Some(1e9), Some(0.0), asof, &resolve(&BuyHeuristic::default(), &serde_yaml::Value::Null).unwrap()), None, "OFF by default");
    }

    #[test]
    fn newest_reads_days_and_quarters() {
        let d = |y, m, d| NaiveDate::from_ymd_opt(y, m, d);
        assert_eq!(newest_day("2026-10-02"), d(2026, 10, 2));
        assert_eq!(newest_day("2026-Q1"), d(2026, 3, 31));
        assert_eq!(newest_day("2026-Q2"), d(2026, 6, 30));
        assert_eq!(newest_day("2025-Q4"), d(2025, 12, 31));
        assert_eq!([newest_day("2026-Q5"), newest_day("—")], [None, None]);
    }

    /// The section inherits every `buy_heuristic` value it does not set, overrides the ones it does, keeps
    /// its own four keys out of `BuyHeuristic`, and errors on a key neither knows.
    #[test]
    fn resolve_inherits_overrides_and_refuses_typos() {
        let buy = BuyHeuristic { growth_min_cagr: 19.0, growth_social_boost: 1.05, ..BuyHeuristic::default() };
        let bare = resolve(&buy, &serde_yaml::Value::Null).unwrap();
        assert_eq!(serde_yaml::to_value(&bare.buy).unwrap(), serde_yaml::to_value(&buy).unwrap(), "no section = the same heuristic");
        assert_eq!((bare.half_life_days, bare.mcap_pivot_eur, bare.runup_cap_pct, bare.loud_penalty), (0.0, 2e9, 50.0, 0.5));
        let raw = serde_yaml::from_str("half_life_days: 30\ngrowth_min_cagr: 8\n").unwrap();
        let k = resolve(&buy, &raw).unwrap();
        assert_eq!((k.half_life_days, k.buy.growth_min_cagr, k.buy.growth_social_boost), (30.0, 8.0, 1.05));
        assert_eq!(buy.growth_min_cagr, 19.0, "the original is untouched");
        let typo = resolve(&buy, &serde_yaml::from_str("growth_min_cagrr: 8").unwrap()).unwrap_err();
        assert!(typo.contains("growth_min_cagrr"), "{typo}");
        assert!(resolve(&buy, &serde_yaml::from_str("half_life_days: soon").unwrap()).unwrap_err().contains("half_life_days"));
        assert!(resolve(&buy, &serde_yaml::from_str("[1]").unwrap()).is_err());
    }
}
