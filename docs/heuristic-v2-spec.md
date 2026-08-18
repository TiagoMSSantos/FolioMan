# FolioMan — Buy Heuristic v2 Spec

**Goal:** rank assets to buy now and hold 20+ years.
**Current universe:** stocks + ETFs + crypto. **Current data:** price + basic fundamentals.
**Diagnosed failure:** ranks hyped junk highly.

---

## 0. The grilling (read this before the formulas)

These are the things that will sink v2 regardless of how good the weights are.

### 0.1 Your base rates are worse than you think

Bessembinder's 1926–2016 CRSP study (25,300 stocks):

| Fact | Value |
|---|---|
| Stocks beating 1-month T-bills over their **lifetime** | **42.6%** |
| Median lifetime return | **negative** |
| Stocks accounting for **all** net wealth creation vs T-bills | **4.3%** (1,092 firms) |
| Top firms producing >50% of all wealth creation | **0.33%** (90 firms) |
| Median return of the 9,187 delisted stocks | **−91.95%** |
| Single-stock strategies that beat the value-weighted market | **4.0%** |

Implication: a 20-year single-name buy-and-hold heuristic is a **negative-skew survival problem**, not a return-maximisation problem. The expected value of your picks is dominated by *how few of them go to zero*, not by how good the best one is. A heuristic tuned to find "the next NVDA" will reliably find the next Peloton instead, because there are ~500x more of those.

**Structural consequence:** FolioMan should not output "the best asset to buy." It should output a **core/satellite allocation** — broad index core (70–90%), screened single names as a satellite sleeve of **30–60 positions**, never 5–10. Under strong positive skew you cannot afford to miss the winners, and concentration is how you miss them. Academic work puts idiosyncratic-risk minimisation at 50–100 names once you account for uncertainty in expected returns, not the classic ~20.

### 0.2 One score cannot span stocks, ETFs and crypto

These have different generating processes and different failure modes. Blending them into one ranked list is the single most likely source of your "hyped junk" symptom — a crypto token or a story stock will always out-score a boring compounder on any growth/momentum axis, because it has no earnings denominator to drag it down.

**Fix:** three separate scoring tracks with hard allocation budgets set *before* scoring. Score within a track, never across.

### 0.3 Everything you're about to code is already priced

McLean & Pontiff (97 predictors, 80 papers): predictor long-short returns fall **26% out-of-sample** and **58% post-publication**. Every factor in this spec is public. Budget for roughly half the backtested edge to be gone before you deploy, and never size positions off in-sample numbers.

### 0.4 "Buy now" is the wrong output for a 20-year horizon

At a 20-year hold, entry timing explains almost none of terminal wealth; starting valuation and survival explain most of it. A "buy now" trigger invites overtrading and encourages exactly the momentum-chasing that produces the junk problem. **Output an eligibility list + a DCA schedule + target weights**, not a buy signal.

### 0.5 Your backtest is probably lying (if you have one)

Free fundamental APIs are not point-in-time. Three specific traps:

- **Restatement bias** — today's API returns *restated* financials for 2019, not what was reported in 2019.
- **Survivorship bias** — delisted tickers are usually absent. Given the −91.95% median delisting return above, this alone can flip a backtest from bad to good.
- **Announcement lag** — annual figures are not knowable until ~60–90 days after fiscal year end. Lag every fundamental by 90 days (quarterly by 45) before use.

If FolioMan has no backtest yet, build the point-in-time data layer *before* tuning weights. Weight tuning on biased data is worse than no tuning, because it produces confidence.

---

## 1. Fix the data first (highest-leverage change)

With P/E, market cap, dividend yield, revenue and EPS you **cannot** compute the metrics that actually separate quality from junk. You are trying to detect fraud with a ruler.

### 1.1 Six fields that unlock ~80% of the value

| Field | Unlocks | Why it matters here |
|---|---|---|
| **Shares outstanding** (5y history) | Net issuance, buyback yield, per-share everything | Dilution is *the* signature of hyped junk. Serial issuers underperform badly (Pontiff & Woodgate 2008, plus international replication). |
| **Total assets** (5y) | Asset growth, gross profitability | Low-asset-growth firms beat high-asset-growth firms by a large margin (Cooper, Gulen & Schill, 1968–2006); effect strongest in exactly the small/speculative names you're mis-buying. |
| **Gross profit** (or revenue − COGS) | Gross profitability (GP/A) | Novy-Marx: GP/A predicts returns ~as well as book-to-market, and is *negatively* correlated with value (−18% rank corr), so it's additive. Distinguishes "good growth" from "value trap". |
| **Total debt + cash** | Net debt, leverage, interest coverage | Survival gate. The 20-year killer is a levered balance sheet meeting a bad decade. |
| **Operating cash flow + capex** | FCF, FCF yield, accruals proxy | EPS is manipulable; cash is much less so. Accrual gap (EPS − FCF/share) is your cheapest fraud/aggressive-accounting flag. |
| **First trade date / IPO date** | Listing-age gate | Recent IPOs carry documented long-run underperformance and have no track record to score. |

### 1.2 Where to get them free

- **US filers:** SEC XBRL API — `https://data.sec.gov/api/xbrl/companyfacts/CIK##########.json`. Free, **no API key or authentication**, and each fact carries its accession number and filing date (`accn`, `filed`, `fy`, `fp`) — which is what makes it genuinely point-in-time and solves §0.5. SEC requires a declared `User-Agent` header with a contact email and rate-limits to ~10 req/s, so cache aggressively. Companion endpoints: `companyconcept` (one metric, one company) and `frames` (one metric, all companies, one period — ideal for building a cross-sectional snapshot). This is the correct backbone for a US-focused FolioMan.
- **Non-US:** no free point-in-time source exists. Either restrict the single-stock track to markets you can source properly, or accept that non-US names get gates only and no score.
- Do **not** mix an as-reported source (SEC) with a restated source (Yahoo/FMP) in the same backtest.

---

## 2. Architecture

```
                    ┌─────────────────────────────┐
                    │  Allocation budget (fixed)  │
                    │  Core ETF   70–90%          │
                    │  Stocks     10–30%          │
                    │  Crypto      0–5%           │
                    └──────────────┬──────────────┘
                                   │
        ┌──────────────────────────┼──────────────────────────┐
        ▼                          ▼                          ▼
  TRACK A: STOCKS            TRACK B: FUNDS            TRACK C: CRYPTO
  Gates → Score → Size       Cost/structure rules      Budget cap only
```

Score inside a track. Never compare a score across tracks.

---

## 3. Track A — Single stocks

### Stage 0: Eligibility gates (binary, non-negotiable)

Gates are the fix for "buys hyped junk." A gate is not a low score — it is exclusion. Junk must be *removed*, not down-weighted, because a high growth score will always out-vote a low quality score in a weighted sum.

| # | Gate | Threshold | Rationale |
|---|---|---|---|
| G1 | **Listing age** | ≥ 5 years since first trade | Need 5y of fundamentals; avoids IPO underperformance window. |
| G2 | **Profitability track record** | Positive EPS in ≥ 4 of last 5 FYs **and** positive TTM FCF | Kills the entire "unprofitable story stock" cohort in one line. |
| G3 | **Dilution** | 3y CAGR of shares outstanding ≤ **+2%/yr** | Serial issuance = the funding-by-dilution business model. |
| G4 | **Leverage** | Net debt / EBITDA ≤ 3.0 **and** EBIT/interest ≥ 3.0 (ex-financials, ex-utilities) | 20-year survival gate. |
| G5 | **Liquidity / size** | Market cap ≥ $2B **and** 60d median $ volume ≥ $5M | Bessembinder: only 42.4% of smallest-cap stocks had positive decade returns vs **81.3%** of largest-cap. |
| G6 | **Lottery / hype** | 12m realised vol ≤ 80th pct of universe **and** max 1-day return in last month ≤ 15% | Bali/Cakici/Whitelaw MAX effect: highest-MAX decile underperforms lowest by **1.03%/mo** raw, **1.18%/mo** 4-factor alpha (1962–2005). |
| G7 | **Valuation ceiling** | EV/Sales ≤ 12 **and** P/E ≤ 60 (or negative-EY exclusion) | Not a value tilt — a sanity ceiling. Prevents "great company, impossible price." |
| G8 | **Accrual sanity** | (EPS − FCF/share) / \|EPS\| ≤ 1.0, 3y average | Aggressive-accounting flag. |
| G9 | **Asset-growth blowout** | 3y total-asset CAGR ≤ 40% | Empire-building / acquisition-rollup flag. |

> Tune thresholds, but keep the *structure*: gates before score. Measure the gates' standalone effect first — in most screens, gates alone capture the majority of the improvement and the scoring layer adds surprisingly little.

### Stage 1: Normalisation

For every raw metric *m*, per **sector** (GICS level 1) and per **date**:

```
rank_pct(m) = (rank of m within sector-date) / (N + 1)          # ∈ (0,1)
z(m)        = clip( Φ⁻¹( rank_pct(m) ), −3, +3 )                # inverse normal CDF
```

Rank→inverse-normal, not raw z-scores. Fundamental distributions are fat-tailed and a single outlier will otherwise dominate the composite. Sector-neutral because a utility's ROE and a software firm's ROE are not comparable, and cross-sector comparison is how sector bubbles enter the portfolio.

### Stage 2: Pillar scores

**P1 — Durability / survival (weight 0.35)**

| Metric | Direction | Notes |
|---|---|---|
| Years of positive EPS in last 10 | + | Consistency, not level |
| Interest coverage (EBIT/interest) | + | log-transform |
| Net debt / EBITDA | − | |
| 3y realised volatility | − | Low-vol / BAB effect |
| Max drawdown over last 10y vs sector median | − | Behaviour in real stress |
| Revenue decline years in last 10 | − | |

**P2 — Compounding quality (weight 0.30)**

| Metric | Direction | Notes |
|---|---|---|
| Gross profitability = (Revenue − COGS) / Total Assets | + | Novy-Marx; the single best quality metric available |
| Net margin = EPS / (Revenue per share) | + | Computable from your current fields + share count |
| 5y margin trend (slope of net margin) | + | Improving > high-and-fading |
| Return on equity, 5y average | + | |
| **Asset growth, 3y CAGR** | **−** | Low asset growth beat high by a wide margin over 40y |
| Shareholder yield = div yield + buyback yield | + | buyback yield = −(share count growth) |

**P3 — Growth, concavely transformed (weight 0.15)**

Do **not** score raw growth linearly. This is the direct cause of the junk problem: linear growth scoring means 90% revenue growth scores 3x better than 30%, when empirically it is *less* durable. ROIC/growth persistence work shows only ~48% of top-ROIC-quintile firms stay there after 3 years, and 15% fall to the bottom quintile; median sector fade rate ≈ 0.21/yr.

```
g       = 5y revenue CAGR
g_score = min(g, 0.25) − 0.5 · max(0, g − 0.40)
```

Rewards growth up to 25%, flat to 40%, actively penalised above 40%. Apply the same shape to EPS CAGR. Add a **stability** term: −stdev(annual revenue growth over 5y).

**P4 — Starting valuation (weight 0.20)**

| Metric | Direction |
|---|---|
| Earnings yield (1/PE, TTM and 5y-average-EPS based) | + |
| FCF yield | + |
| EV/Sales | − |
| PEG-lite = PE / (100 · g_capped), g_capped = min(g, 0.20) | − |

Valuation matters *more* at 20 years, not less — long-horizon return is anchored by entry multiple. Recent work on component-CAPE reports out-of-sample R² above 50% for 10-year S&P returns. Use a **10-year-average-EPS** earnings yield where you have the history; it is far more stable than TTM.

**Deliberately excluded from selection:** 12-month momentum. Momentum decays in months; you are holding for decades. Use it only as a **tie-breaker for entry timing** among already-selected names, never as a selection factor. Include instead a small **long-term reversal** penalty: −0.05 weight on 3-year price return, to avoid buying at the top of a multi-year run.

### Stage 3: Composite and sizing

```
Score = 0.35·P1 + 0.30·P2 + 0.15·P3 + 0.20·P4 − 0.05·z(3y price return)

Each Pk = weighted mean of its member z-scores (weights within pillar sum to 1)
Missing metric → drop it and renormalise that pillar's weights; if >40% of a
pillar's metrics are missing, the name is ineligible (do not impute).
```

**Sizing — cap-dominant, score-tilted:**

```
w_raw_i  = sqrt(marketcap_i) · (1 + 0.5 · Score_i)      # Score clipped to [−2, +2]
w_i      = w_raw_i / Σ w_raw
Constraints: w_i ≤ 4% of the stock sleeve
             Σ w per GICS sector ≤ 25% of the sleeve
             30 ≤ number of positions ≤ 60
```

sqrt(cap) weighting, not equal and not pure cap: equal weight over-loads the small end where the survival base rates are worst; pure cap weight makes the sleeve redundant with the core ETF.

**Hysteresis (kills ranking churn and turnover):**

```
Enter  if rank ≤ 40 and all gates pass
Hold   if rank ≤ 120 and no gate fails
Exit   if rank > 120, or any gate G1–G9 fails for 2 consecutive quarters
```

Re-score quarterly. Trade at most twice a year. At a 20-year horizon, turnover is pure cost.

---

## 4. Track B — Funds / ETFs

Do not score ETFs with the equity model. Fund selection is a **cost-and-structure** problem, and the evidence is unusually clean.

Morningstar, US equity funds 2010–2015, success rate by expense-ratio quintile:

| Quintile | Cheapest | 2nd | Mid | 4th | Priciest |
|---|---|---|---|---|---|
| Success rate | **62%** | 48% | 39% | 30% | **20%** |

Same monotonic pattern in international equity (51% → 21%), balanced (54% → 24%), taxable bonds (59% → 17%). Cheapest quintile ≈ **3x** the success rate of the priciest.

**Rule set (checklist, not a score):**

1. Expense ratio ≤ 0.20% for broad market, ≤ 0.35% for factor/sector. Hard reject above.
2. AUM ≥ $500M and fund age ≥ 5y (closure risk — closed funds are the fund-world equivalent of delisting).
3. Median bid-ask spread ≤ 0.10%; 3y tracking difference vs index within ±0.15%/yr.
4. Full replication preferred; reject synthetic/swap-based and leveraged/inverse outright for a 20y hold.
5. Index breadth: prefer ≥ 1,000 constituents for the core. Reject thematic/narrative ETFs entirely — they are the ETF-shaped version of your hyped-junk problem, and they are typically launched at the peak of the theme.
6. Domicile/tax wrapper appropriate to your jurisdiction (this often outweighs a 0.05% fee difference).

**Rank surviving funds by:** total cost of ownership = expense ratio + tracking difference + half-spread + (tax drag). Lowest wins. That's the whole model.

---

## 5. Track C — Crypto

Be honest about what this is. CoinGecko's data shows **53.2%** of all listed tokens have failed (13.4M of 25.2M), with 86.3% of those failures from the 2025 launch cohort alone. There is no earnings, no book value, no cash flow — every metric in Tracks A and B is undefined.

**Therefore: do not score crypto. Budget it.**

1. Fixed allocation cap: **0–5%** of total portfolio, set once, in writing, before any scoring runs.
2. Eligible set restricted by *objective survivorship* criteria only: ≥ 8 years of continuous trading history, top-10 by market cap for ≥ 5 consecutive years, on ≥ 3 tier-1 venues. In practice this admits roughly two assets.
3. Rebalance to the cap band (e.g. rebalance if drift > ±50% of target) — this is what converts volatility into a small positive, and prevents a bull run from silently making crypto 30% of a 20-year portfolio.
4. Explicitly label this sleeve as speculative in the UI. Do not let it borrow legitimacy from the equity score.

---

## 6. What "buy now" should actually output

```json
{
  "asof": "2026-08-18",
  "core": [{"ticker": "VT", "target_weight": 0.75, "action": "dca", "monthly": 800}],
  "satellite": [
    {"ticker": "XXX", "score": 1.42, "rank": 7, "target_weight": 0.021,
     "gates_passed": 9, "action": "accumulate",
     "why": ["GP/A 92nd pct", "share count -2.1%/yr", "EY 5.8%", "net debt/EBITDA 0.4"]}
  ],
  "speculative": [{"ticker": "BTC", "target_weight": 0.03, "action": "hold_band"}],
  "rejected_top_movers": [
    {"ticker": "YYY", "failed_gates": ["G2 profitability", "G3 dilution +18%/yr", "G6 MAX 31%"]}
  ]
}
```

The `rejected_top_movers` field is the important one for your stated problem: it makes the heuristic show you *what it refused to buy and why*. If that list is empty during a hype cycle, your gates are not binding.

---

## 7. Validation plan (do this before trusting any of it)

1. **Build point-in-time first.** SEC XBRL with `filed` dates; include delisted CIKs; lag annuals 90d, quarterlies 45d.
2. **Ablation, in this order:** universe → +gates only → +gates & P1/P2 → full score. Report each step separately. If gates alone deliver most of the gain, ship gates and stop; don't add a scoring layer that only adds overfit.
3. **Walk-forward, never full-sample.** Fit nothing on data after the decision date. Weights in §3 are priors — do not optimise them on the same data you evaluate on.
4. **Long-horizon test, not monthly.** Form portfolios and hold **10 years** without rebalancing (as many overlapping cohorts as your history allows). Almost all published factor evidence assumes monthly/annual rebalancing; a 20-year hold is a fundamentally different test and most factors look far weaker under it. This is the test that matters for FolioMan and almost nobody runs it.
5. **Terminal-wealth distribution, not mean return.** Report median, 10th percentile, and P(underperform broad index) across cohorts. Given the skew in §0.1, mean return is a nearly useless summary statistic here.
6. **Multiple-testing discipline.** Log every variant you test. Apply a deflated Sharpe / Benjamini-Hochberg correction. Then haircut whatever survives by ~50% per §0.3.

---

## 8. Change summary vs a typical v1

| Change | Why |
|---|---|
| Split into 3 tracks with pre-set budgets | Stops crypto/ETFs competing with stocks on one score |
| Gates *before* scoring, 9 hard gates | Direct fix for "buys hyped junk" — exclusion beats down-weighting |
| Concave growth transform, penalty above 40% | Linear growth scoring is the mechanical cause of the junk problem |
| Dilution + asset growth added | Strongest available junk detectors; need share count + total assets |
| Gross profitability replaces P/E as the quality core | Best evidenced quality metric; additive to value |
| Momentum demoted to tie-breaker; LTR penalty added | Momentum decays in months, horizon is decades |
| Sector-neutral rank→inverse-normal scoring | Kills outlier and sector-bubble dominance |
| sqrt(cap) sizing, 30–60 names, 4%/25% caps | Positive skew punishes concentration |
| Hysteresis bands 40/120 | Fixes rank churn, cuts turnover |
| ETF track = cost checklist | Fees are the best-evidenced fund predictor (62% vs 20%) |
| Crypto capped, not scored | 53.2% failure rate, no scorable fundamentals |
| Point-in-time data layer made a prerequisite | Otherwise every backtest number is fiction |

---

## Sources

- Bessembinder, *Do Stocks Outperform Treasury Bills?*, JFE 2018 — [ASU W. P. Carey](https://wpcarey.asu.edu/department-finance/faculty-research/do-stocks-outperform-treasury-bills) · [summary](https://massivemoats.substack.com/p/executive-summary-of-bessembinders)
- Mauboussin & Callahan, *ROIC and the Investment Process*, Morgan Stanley Counterpoint Global — [PDF](https://www.morganstanley.com/im/publication/insights/articles/article_roicandtheinvestmentprocess.pdf)
- Mauboussin & Callahan, *Competitive Advantage Period* — [PDF](https://www.morganstanley.com/im/publication/insights/articles/article_theneglectedvaluedriver_ltr.pdf)
- Novy-Marx, *The Other Side of Value: The Gross Profitability Premium*, JFE 2013 — [PDF](https://www.aqr.com/-/media/AQR/Documents/AQR-Insight-Award/2012/The-Other-Side-of-Value.pdf)
- Asness, Frazzini & Pedersen, *Quality Minus Junk* — [AQR](https://www.aqr.com/Insights/Research/Working-Paper/Quality-Minus-Junk)
- Cooper, Gulen & Schill, *Asset Growth and the Cross-Section of Stock Returns*, JF 2008 — [SSRN](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=760967) · [summary](https://quantpedia.com/strategies/asset-growth-effect)
- Pontiff & Woodgate, *Share Issuance and Cross-Sectional Returns*, JF 2008 — [EconPapers](https://econpapers.repec.org/RePEc:bla:jfinan:v:63:y:2008:i:2:p:921-945)
- Bali, Cakici & Whitelaw, *Maxing Out: Stocks as Lotteries*, JFE 2011 — [PDF](https://pages.stern.nyu.edu/~rwhitela/papers/max%20jfe11.pdf)
- McLean & Pontiff, *Does Academic Research Destroy Stock Return Predictability?*, JF 2016 — [PDF](https://tevgeniou.github.io/EquityRiskFactors/bibliography/AcademicReviewFactor.pdf)
- Ma, *CAPE Ratios and Long-Term Returns*, 2026 — [PDF](https://theideafarm.com/wp-content/uploads/2026/01/20260112CAPE.pdf)
- Kinnel, *Fund Fees Predict Future Success or Failure*, Morningstar — [article](https://www.morningstar.com/funds/fund-fees-predict-future-success-or-failure)
- CoinGecko, *Dead Coins: How Many Cryptocurrencies Have Failed?* — [research](https://www.coingecko.com/research/publications/how-many-cryptocurrencies-failed)
- Hjalmarsson, *Long-Horizon Stock Returns Are Positively Skewed*, Review of Finance 2023 — [Oxford Academic](https://academic.oup.com/rof/article/27/2/495/6564241)
- Alpha Architect, *How Many Stocks Should Be In Your Portfolio?* — [article](https://alphaarchitect.com/how-many-stocks-should-be-in-your-portfolio-a-practical-guide-to-portfolio-construction/)

*Not investment advice — I'm not a financial advisor. This is a spec for a scoring system, and every threshold in it is a starting prior that needs validating on your own point-in-time data.*
