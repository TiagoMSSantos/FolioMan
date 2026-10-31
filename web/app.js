// The payload's lanes are {stocks, etfs, crypto}; `generated` and the other keys feed the rest of
// the page. Each lane is a list of ROWS and each row a list of
// [header, cell] pairs — exactly the cells the terminal prints for that row, so nothing is
// reformatted here. (#143) A lane was a single row until then; every row of a lane carries the same
// headers in the same order, which is why the head is read off row 0.
const LANES = ["stocks", "etfs", "crypto"];
// (#767) 36 until here, which hid a dozen refused deploys. 30 days to 2026-10-20: 611 deploys, gaps p99 6.0h,
// longest 7.3h. So 12h never fires on a healthy cron, and does after a few refused runs in a row.
const STALE_HOURS = 12;
// (#718) An n/a, — or burn cell hovers its column's own "mark = why" clause from the help text, else
// the mark's plain meaning. The clause ends at "; " or ". "; a bracket or a dot inside a number stays.
const WHY = {
  "n/a": ["n/a", "no source this page reads carries this figure for this row"],
  "—": ["—|dash", "does not apply to this row, or cannot exist yet"],
  burn: ["burn", "the cash flow is at or below 0"],
};
const clause = (text, alt) => (text || "").match(new RegExp("(?:^|[;.,(] |a )(?:" + alt + ") = ((?:[^;.()]|\\([^)]*\\)|\\.(?! ))+)"))?.[1].trim();
function why(text, cell) {
  const mark = Object.keys(WHY).find((m) => cell.startsWith(m));
  const [alt, plain] = WHY[mark];
  return mark + " = " + (clause(text, alt) ?? plain);
}
// (#732) Any other value cell hovers what each of its marks means, a line per mark: a word cell (IE,
// Swap, held) is its own mark, a RED-FLAGS cell's letters are marks, else its superscripts and signs
// (ᶠ³ reads as ᶠ, then ³ off "³ etc. ="), and a trailing " r". A mark its own column leaves undefined
// is read off a column its help names (MOS%'s "ᵇ … as FAIR"), so a free-text title's ~ borrows
// nothing. No mark read: the column's first clause.
// shortcut: the first named column defining a borrowed mark wins, so a mark must mean one thing per table.
const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
function hover(help, h, cell) {
  const own = help[h];
  if (!own || !cell) return "";
  const word = !/\d/.test(cell) && clause(own, esc(cell));
  if (word) return cell + " = " + word;
  const texts = [own, ...Object.entries(help).filter(([k]) => k && k !== h && own.includes(k)).map(([, t]) => t)];
  const look = (alt) => texts.reduce((c, t) => c || clause(t, alt), undefined);
  const s = cell.match(/^\d+: ([A-Z0-9]+)$/)?.[1] ?? (cell.match(/[ʰ-˿ᴬ-ᶿ⁰-₟¹²³‡†~°≈]/g) ?? []).join("") + (/ r$/.test(cell) ? "r" : "");
  const lines = [];
  for (let i = 0; i < s.length; ) {
    let j = s.length;
    let c;
    while (j > i && !(c = look(esc(s.slice(i, j))))) j--;
    const run = j > i ? s.slice(i, j) : s.slice(i).match(/^[⁰¹²³⁴-⁹]+/)?.[0];
    if (j === i && run) c = look("[⁰¹²³⁴-⁹]+ etc\\.");
    if (c) lines.push(run + " = " + c);
    i += run?.length || 1;
  }
  return lines.join("\n") || own.match(/^(?:[^;.()]|\([^)]*\)|\.(?! ))+/)?.[0].trim() || "";
}
console.assert(
  ((help) =>
    hover(help, "R", "2: LI") === "L = a loss\nI = interest cover under 1.5x" && hover(help, "F", "€1.2ᶠ³") === "ᶠ = a REIT\n³ = over that many years" &&
    hover(help, "M", "+5%ᵇ") === "ᵇ = a bank (on book value)" && hover(help, "S", "0.4ʷⁱ") === "ʷⁱ = under a year" &&
    hover(help, "U", "Swap") === "Swap = a counterparty pays" && hover(help, "F", "23.1") === "Fair price" && hover(help, "U", "Opt") === "Tracks")({
    R: "Flags, and which; L = a loss; I = interest cover under 1.5x. 2 or more",
    F: "Fair price. A check; ³ etc. = over that many years; ᶠ = a REIT. ᵇ = a bank (on book value)",
    M: "Margin; ᵇ, ᶠ as F",
    S: "Score; ʷ = gates off; ʷⁱ = under a year",
    U: "Tracks; Full = holds every name; Swap = a counterparty pays",
  }),
  "a value cell's hover misreads its marks",
);

// (#731) A NAME also links its TradingView chart, an icon after the name. A Yahoo suffix maps to the
// TradingView exchange; a bare US symbol needs none, a class share's dash is a dot there (BRK.B) and a
// coin pair drops its dash (BTCEUR). No symbol, or an unmapped suffix, gets no icon.
// shortcut: a few ETF codes differ between the two sites, so that icon 404s; the NAME and TICKER links still work.
const TV = { DE: "XETR", L: "LSE", PA: "EURONEXT", AS: "EURONEXT", LS: "EURONEXT", SW: "SIX", T: "TSE", MI: "MIL", OL: "OSL", ST: "OMXSTO", F: "FWB" };
function tradingview(t) {
  const [, s, x] = String(t).match(/^([A-Z0-9][A-Z0-9-]*)(?:\.([A-Z]+))?$/) ?? [];
  if (!s || (x && !TV[x])) return null;
  return "https://www.tradingview.com/symbols/" + (x ? TV[x] + "-" : "") + s.replace(/-([A-Z])$/, ".$1").replace(/-(EUR|USD)$/, "$1") + "/";
}
console.assert(
  tradingview("APC.DE") === "https://www.tradingview.com/symbols/XETR-APC/" && tradingview("BRK-B")?.endsWith("/symbols/BRK.B/") &&
    tradingview("8001.T")?.endsWith("/TSE-8001/") && tradingview("BTC-EUR")?.endsWith("/BTCEUR/") && tradingview("SMCI")?.endsWith("/symbols/SMCI/") &&
    tradingview("0700.HK") === null && tradingview("—") === null && tradingview(null) === null,
  "a TradingView link misreads its symbol",
);

// (#824) An Intraday SESSION cell, `09:30-16:00 New York`, on the browser's own clock. `wall` is the
// instant of today's h:m in `zone`: the zone's offset is its wall clock now, read as UTC, less now.
// shortcut: the offset is now's, so on a clock-change day the hover can read an hour off until the change.
const ZONES = { "New York": "America/New_York", Frankfurt: "Europe/Berlin" };
function wall(h, m, zone, now = new Date()) {
  const fmt = new Intl.DateTimeFormat("en-US", { timeZone: zone, hourCycle: "h23", year: "numeric", month: "numeric", day: "numeric", hour: "numeric", minute: "numeric" });
  const p = Object.fromEntries(fmt.formatToParts(now).map((x) => [x.type, +x.value]));
  return Date.UTC(p.year, p.month - 1, p.day, h, m) - (Date.UTC(p.year, p.month - 1, p.day, p.hour, p.minute) - Math.floor(now / 6e4) * 6e4);
}
// (#1213) `hours` is the bare `13:30-20:00` on the reader's clock, which the cell also prints
// under the exchange's hours: a phone never shows the hover.
function hours(cell) {
  const [, a, b, c, d, city] = String(cell).match(/^(\d\d):(\d\d)-(\d\d):(\d\d) (.+)$/) ?? [];
  if (!ZONES[city]) return "";
  const at = (h, m) => new Date(wall(+h, +m, ZONES[city])).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
  return at(a, b) + "-" + at(c, d);
}
// (#1223) A Trading MKT cell's hover, `session 09:30-16:00 New York` and maybe `; priced on …`: the
// hours, then the rest.
const session = (tip) => /^session (\d\d:\d\d-\d\d:\d\d [A-Za-z ]+?)((?:;.*)?)$/.exec(String(tip));
function local(cell) {
  const mine = hours(cell);
  return mine && cell + " = " + mine + " your time (" + Intl.DateTimeFormat().resolvedOptions().timeZone + ")";
}
console.assert(
  wall(9, 30, "America/New_York", new Date(Date.UTC(2026, 0, 15, 12))) === Date.UTC(2026, 0, 15, 14, 30) &&
    wall(17, 30, "Europe/Berlin", new Date(Date.UTC(2026, 6, 15, 12))) === Date.UTC(2026, 6, 15, 15, 30) &&
    /^\d\d:\d\d-\d\d:\d\d$/.test(hours("09:30-16:00 New York")) && hours("09:00-17:30 Tokyo") === "" &&
    local("09:30-16:00 New York").startsWith("09:30-16:00 New York = ") && local("09:00-17:30 Tokyo") === "" &&
    session("session 09:30-16:00 New York; priced on NVDA, its US listing")?.[1] === "09:30-16:00 New York" &&
    session("session 09:00-17:30 Frankfurt")?.[2] === "" && session("US") === null,
  "a SESSION hover misreads its exchange's clock",
);

// (#1254) The euros for one buy-list name: the amount the reader typed × its BUY%, in whole euros.
// Display only and never stored (no URL key, no localStorage): a shared link carries no amount, and
// the next visit opens with the box empty.
const euro = (amt, pct) => (amt > 0 && pct > 0 ? "€" + Math.round((amt * pct) / 100).toLocaleString("en-US") : "");
const EURO_HELP = "Your euros for this name: the amount you typed × BUY%. Example: €1,000 × 4.1% = €41. A name in two tables is the same money; count it once.";
console.assert(
  euro(1000, 4.12) === "€41" && euro(50000, 4.12) === "€2,060" && euro(0, 4) === "" && euro(1000, NaN) === "",
  "a BUY € cell misprices",
);

// (#1258) The start box's whole-world fund: of CORE's rows whose INDEX reads all country world, all
// world or ACWI, the one missing the fewest buy rules (a fund under a year old misses several), then a
// euro listing, then the lowest yearly fee, then the bigger fund; a full tie keeps CORE's order. Each
// world fund has missed cagr and cagr-life, so the sentence says why the buy list skips it, in the
// CORE MISSES help's own words. shortcut: the euro listing is read from the exchange suffix, so a euro
// line on London reads non-euro; the payload carries no quote currency.
const cellOf = (r, h) => r.find(([k]) => k === h)?.[1] ?? "";
const WORLD = /all[- ]country[- ]world|all[- ]world|acwi/i;
function startFund(core, missHelp) {
  const inf = (x) => (Number.isNaN(x) ? Infinity : x);
  const key = (r) => {
    const aum = /([\d.]+)\s*([KMB]?)/.exec(cellOf(r, "AUM").replaceAll(",", ""));
    return [
      inf(parseInt(cellOf(r, "MISSES"))),
      /\.(DE|F|PA|AS|MI|BR|MC|VI|HE|LS|IR)$/.test(cellOf(r, "TICKER")) ? 0 : 1,
      inf(parseFloat(cellOf(r, "TER"))),
      aum ? -aum[1] * ({ K: 1e3, M: 1e6, B: 1e9 }[aum[2]] || 1) : 0,
    ];
  };
  const cmp = ([a], [b]) => {
    const i = a.findIndex((x, j) => x !== b[j]);
    return i < 0 ? 0 : a[i] - b[i];
  };
  const worlds = core.filter((r) => WORLD.test(cellOf(r, "INDEX"))).map((r) => [key(r), r]);
  const [k, w] = worlds.sort(cmp)[0] || [];
  if (!w) return "Look in CORE for an INDEX that says all country world or all world.";
  const miss = cellOf(w, "MISSES");
  const n = parseInt(miss) || 0;
  const means = (miss.split(":")[1] || "").split(",").map((m) => m.trim()).filter(Boolean).map((m) => missHelp.split("; ").find((p) => p.startsWith(m + " = "))?.slice(m.length + 3) || m);
  const every = worlds.every(([x]) => x[0] > 0) ? "Like every world fund here, it" : "It";
  return (
    "Today: " + cellOf(w, "NAME") + " (" + cellOf(w, "TICKER") + "), rank " + parseInt(cellOf(w, "RANK")) + " in the CORE table. " +
    "Of the world funds that miss the fewest buy rules, it " + (k[1] ? "has" : "is the euro listing with") + " the lowest yearly fee, " + cellOf(w, "TER") + "." +
    (n > 0 ? " " + every + " misses " + n + " buy rule" + (n > 1 ? "s" : "") + ", so the buy list itself does not buy it: " + means.join("; ") + "." : "")
  );
}
console.assert(
  ((f) => {
    const row = (t, i, m, ter, aum, rank) => [["RANK", rank], ["NAME", t + " fund"], ["TICKER", t], ["INDEX", i], ["MISSES", m], ["TER", ter], ["AUM", aum]];
    const help = "Buy rules missed: x; cagr = grew too slowly per year; cagr-life = grew too slowly over its whole life; y = z";
    const two = "2: cagr, cagr-life";
    const core = [row("S.DE", "S&P 500", "0", "0.03%", "€90.0B", "1"), row("L.L", "FTSE All-World", two, "0.10%", "€50.0B", "3"), row("Y.DE", "msci acwi", "5: history, 1Y+, cagr, range, liquidity", "0.05%", "€900M", "9"), row("B.DE", "MSCI ACWI", two, "0.12%", "€9.9B", "4"), row("A.DE", "MSCI All Country World", two, "0.12%", "€17.3B", "5")];
    return f(core, help) === "Today: A.DE fund (A.DE), rank 5 in the CORE table. Of the world funds that miss the fewest buy rules, it is the euro listing with the lowest yearly fee, 0.12%. Like every world fund here, it misses 2 buy rules, so the buy list itself does not buy it: grew too slowly per year; grew too slowly over its whole life." &&
      f([row("P.L", "FTSE All-World", "0", "0.20%", "n/a", "2")], help) === "Today: P.L fund (P.L), rank 2 in the CORE table. Of the world funds that miss the fewest buy rules, it has the lowest yearly fee, 0.20%." &&
      f(core.slice(0, 1), help).startsWith("Look in CORE");
  })(startFund),
  "the start box names the wrong whole-world fund",
);

function table(rows, empty = "(none pass every buy rule)", help = {}, fresh = new Set(), flags = {}) {
  if (!rows || rows.length === 0) {
    const p = document.createElement("p");
    p.className = "empty";
    p.textContent = empty;
    return p;
  }
  const box = document.createElement("div");
  box.className = "scroll";
  box.tabIndex = 0; // (#433) a box that scrolls must take focus, or a keyboard cannot scroll it
  box.role = "group";
  const t = document.createElement("table");
  const head = t.createTHead().insertRow();
  // (#1254) A BUY € column follows every BUY%, hidden by CSS until the reader types an amount (euros()).
  for (const header of rows[0].flatMap(([h]) => (h === "BUY%" ? [h, "BUY €"] : [h]))) {
    const th = document.createElement("th");
    th.scope = "col";
    if (header === "BUY €") th.className = "eur";
    const b = document.createElement("button");
    b.textContent = header;
    b.title = header === "BUY €" ? EURO_HELP : help[header] || ""; // (#400) hover text; the glossary below the tables lists the same
    if (!header) b.ariaLabel = "owned"; // CORE's flag column prints no header, but a button needs a name
    b.onclick = () => sortBy(th);
    th.appendChild(b);
    head.appendChild(th);
  }
  const body = t.createTBody();
  rows.forEach((row, i) => {
    const tr = body.insertRow();
    tr.dataset.rank = i; // (#393) the ranked order, which a sort reorders but apply() still cuts on
    // (#397) a pinned row (`*` in its RANK cell) shows at every N: an uploaded watchlist always shows
    // (#535) a `b` row does not: N cuts it like any row, so ?top=1 shows one name
    if (row.some(([h, c]) => h === "RANK" && /\*/.test(c))) tr.dataset.pin = "";
    for (const [h, cell, url, tip] of row) {
      const td = tr.insertCell();
      if (tip) td.title = td.dataset.note = tip; // (#1256) the note a row copy carries; (#542) a REPORTS cell's hover: the description and last annual report; (#543) a MISSES cell's: each missed gate's value vs its bar. Text only
      else if (/^(n\/a|—|burn)/.test(cell)) td.title = why(help[h], cell); // (#718)
      else if (h !== "NAME" && h !== "TICKER") td.title = hover(help, h, String(cell)); // (#732) NAME and TICKER link; their link is the hover
      // (#474) a RANK cell's hover reads its position, then what each of its flags means. (#933) Only
      // where RANK is a position in this table: Intraday ranks per market and Exposure ranks names
      // only, so those keep their own help's hover from the line above.
      if (h === "RANK" && cell && (!help.RANK || help.RANK.startsWith("Position in this table"))) {
        const [, n, marks] = cell.match(/^(\d*)(.*)$/);
        td.title = ["rank " + n + " in this table", ...[...marks].filter((f) => flags[f]).map((f) => f + " " + flags[f])].join("\n");
      }
      // (#824) the Trading hours on the reader's own clock, (#1213) in the cell too; (#1223) from MKT's hover
      const at = h === "MKT" && session(tip);
      if (at && hours(at[1])) {
        td.title = "session " + local(at[1]) + at[2];
        td.dataset.local = hours(at[1]) + " your time";
      }
      // (#401) Every ticker is already a Yahoo symbol: the pool is fetched from Yahoo. (#455) A shadow
      // table's cell may carry its source page as a third element; only https ever becomes a link.
      // (#482) A NAME with no page of its own links the row's Yahoo quote, and so does an Exposure
      // `name` row, whose NAME is the symbol.
      const yahoo = (t) => "https://finance.yahoo.com/quote/" + encodeURIComponent(t);
      const own = h === "NAME" && (row.find(([k]) => k === "TICKER")?.[1] ?? (row.some(([k, c]) => k === "KIND" && c === "name") ? cell : null));
      const sym = !url?.startsWith("https://") && own;
      // (#524) an IPO row's TICKER reads — before a symbol is asked for: no quote to link
      const href = h === "TICKER" ? (cell === "—" ? null : yahoo(cell)) : sym && sym !== "—" ? yahoo(sym) : url;
      if (!href?.startsWith("https://")) {
        td.textContent = cell;
        continue;
      }
      td.append(Object.assign(document.createElement("a"), { href, target: "_blank", rel: "noopener", textContent: cell }));
      // (#731) a plain NAME has no symbol, so the icon only follows a link. It is CSS `::before`, so the
      // cell's textContent (the sort key and the search) stays the name.
      const tv = own && tradingview(own);
      if (tv) td.append(Object.assign(document.createElement("a"), { href: tv, className: "tv", target: "_blank", rel: "noopener", title: "TradingView chart", ariaLabel: "TradingView chart" }));
      if (h === "TICKER" && fresh.has(cell)) td.className = "new";
    }
    const buy = row.findIndex(([h]) => h === "BUY%");
    if (buy >= 0) Object.assign(tr.insertCell(buy + 1), { className: "eur" }).dataset.pct = parseFloat(row[buy][1]);
    // (#1256) the copy-this-row button holds no text (its ⧉ is CSS), so sort, filter and widths read the cell as before
    const copy = Object.assign(document.createElement("button"), { className: "copy", type: "button", ariaLabel: "copy row", title: "Copy this row and its column names" });
    copy.onclick = () => {
      const { cols, heads } = shown(t);
      clip(copy, rowText(title(t), cols.map((i, k) => [heads[k], tr.cells[i].textContent, tr.cells[i].dataset.note])));
    };
    tr.cells[0].prepend(copy);
  });
  box.appendChild(t);
  return box;
}

// Every row is in the DOM; the chooser only decides how many are VISIBLE. Hiding rather than
// re-rendering is what keeps this small and what makes changing N instant.
// (#249) SCOPED TO THE THREE LANES, and that is load-bearing: a bare `tbody` selector also matches
// the inflation table, so `?top=1` would publish the USA row and hide the EU one — the deflator, on
// the view most likely to be bookmarked. (#438) The Attention table was cut too; (#441) the Berkshire table
// was not, since the default N of 3 hid 11 of 14. (#525) Every table under the lanes ranks its own rows.
// (#539) N cuts every asset table, lanes and those below alike; Exposure and (#811) Intraday stay whole
// and `all` is no cut.
// (#393) Cut on the RANK, not the row's position: a sort reorders the top N, never swaps who is in it.
// (#445) A filter query overrides the cut in every table but inflation, (#486) bonds and (#811) intraday: a row
// shows iff its TICKER or NAME holds the query, so a name the chooser hid can still be found. An empty
// query is the cut alone.
// (#502) The shadow tables mark a row whose CAGR is under the stock lane's floor (`growth_min_cagr`); (#525) the
// REITs table's floor is `reit_min_cagr`. The row is greyed, never hidden (it hid until #525), and an n/a CAGR
// is not marked. (#530) The CAGR read is TR-CAGR, the page's one return column; the floors ride in every
// payload, an upload's included (they stayed CI's until then).
// (#507) An ⁱ CAGR is the plain change since listing (under 6 months), so its bar is the floor compounded
// over the row's YRS: (1 + floor/100)^YRS − 1. A day-0 listing's bar is 0, so it is unmarked unless it fell.
// (#525) The cut reads `data-rank`, the payload's order, which ranks by SCORE then gates missed.
// (#526) The shadow tables and CORE also hide a row whose total-return CAGR is under `page_min_cagr`: the
// TR-CAGR cell (ⁱ under 6 months since #530, with its bar), else a CAGR cell a settings file still lists.
// The top N counts only the rows kept; (#539) the box or a query shows the floored rest greyed, and a
// pinned row is never hidden.
const SHADOW = ["attention", "berkshire", "social", "young", "braked", "reits"];
const floors = {};
let minCagr = 0; // (#526) the payload's, like the floors
const bar = (cell, yrs, f) => (cell.endsWith("ⁱ") ? ((1 + f / 100) ** (+yrs || 0) - 1) * 100 : f);
console.assert(
  Math.abs(bar("+5%ⁱ", "0.5", 19) - 9.087) < 1e-3 && bar("+0%ⁱ", "0.0", 19) === 0 && bar("+5%", "0.5", 19) === 19,
  "the since-listing bar misreads YRS",
);
const lt = (cell, yrs, f, v = key(cell)) => typeof f === "number" && typeof v === "number" && v < bar(cell, yrs, f);
const low = (id, cell, yrs) => lt(cell, yrs, floors[id]);
const under = (tr, cell, yrs, f = minCagr) => f > 0 && (typeof key(tr) === "number" ? lt(tr, yrs, f) : lt(cell, yrs, f));
function apply(n) {
  const q = document.getElementById("filter").value.trim().toLowerCase();
  const all = document.getElementById("allrows").checked;
  for (const t of document.querySelectorAll(".scroll table")) {
    if (t.closest("#inflation, #bonds, #intraday, #trading_buynow, #approaches, #track")) continue; // (#811) every intraday row shows, unfiltered; (#1140) every track row; (#1221) every approach; (#1223) every BUY NOW stock
    const id = t.closest("div[id]").id;
    const cap = id === "exposure" ? Infinity : n;
    const heads = [...t.rows[0].cells].map((th) => th.textContent);
    const cols = heads.flatMap((h, i) => (["TICKER", "NAME"].includes(h) ? [i] : []));
    const c = SHADOW.includes(id) ? heads.indexOf("TR-CAGR") : -1;
    const at = (tr, h) => tr.cells[heads.indexOf(h)]?.textContent;
    const rows = [...t.tBodies[0].rows];
    const floored = SHADOW.includes(id) || id === "core";
    const out = new Set(rows.filter((tr) => floored && !("pin" in tr.dataset) && under(at(tr, "TR-CAGR"), at(tr, "CAGR"), at(tr, "YRS"))));
    const lim = rows.filter((tr) => !out.has(tr)).map((tr) => +tr.dataset.rank).sort((a, b) => a - b)[cap] ?? Infinity; // first rank cut
    for (const tr of rows) {
      tr.hidden = q ? !cols.some((i) => tr.cells[i].textContent.toLowerCase().includes(q)) : !("pin" in tr.dataset) && ((out.has(tr) && !all) || +tr.dataset.rank >= lim);
      tr.classList.toggle("below", out.has(tr) || (c >= 0 && low(id, tr.cells[c].textContent, at(tr, "YRS"))));
    }
  }
  stick();
}

// (#445) Column groups for the nine ranked tables (the three lanes and the six in `view()`),
// finviz-style. A header NOT listed here shows in every view, so RANK NAME TICKER BUY% (and any
// column added later) can never be hidden by accident. (#1201) CORE joins them, now that it
// carries the ETF lane's columns.
// (#493) A header may sit in several views, space-separated: `cash` regroups the cash-flow and debt
// columns that `value` and `risk` split, so a phone reader finds them in one pick.
const VIEW = {
  MARKET: "overview", SECTOR: "overview", DOM: "overview", EARN: "overview", "PRICE(EUR)": "overview", SCORE: "overview",
  CAGR: "returns", YRS: "returns", "1D": "returns", "1W": "returns", "1M": "returns", "1Y": "returns", "2Y": "returns",
  "5Y": "returns", "8Y": "returns", "20Y": "returns", "S-8Y": "returns",
  VOL: "risk", BETA: "risk", MAXDD: "risk", R2: "risk", "ABV-MA": "risk", "OFF-HI": "risk", "RANGE%": "risk", TURNOVER: "risk",
  "5Y-WIN%": "risk", "WORST-5Y": "risk", "UW-YRS": "risk", "ND/EBITDA": "risk cash", "INT-COV": "risk cash", "SHORT%": "risk", "RED-FLAGS": "risk",
  MCAP: "value", "P/E": "value", PEG: "value", "ROE/A": "value", "REV-YoY": "value", "REV-5Y": "value", "FCF-5Y": "value cash", "OP%": "value", "MARGIN-TREND": "value", "M-SCORE": "risk", "SBC%": "value", "EPS-YoY": "value",
  "NET%": "value", DIV: "value", BUYBK: "value", TER: "value", AUM: "value", USE: "value", REPL: "value",
  MVRV: "value", ROIC: "value", "FCF%": "value cash", "CAPEX/OCF": "value cash", FAIR: "value", "MOS%": "value", "FAIR-OE": "value", "MOS-OE%": "value", "FAIR-CAP": "value", "IMPL-G": "value", "TARGET%": "value", REPORTS: "overview value", "INS-B/S": "value",
  "TOP10%": "risk", "TD-1Y": "value", "TD-5Y": "value",
  "FCF-YLD": "value cash", "P/S": "value", "EV/EBITDA": "value", "PAYOUT%": "value cash", "FCF-PAY%": "value cash",
};
function view(v) {
  for (const t of document.querySelectorAll(":is(#stocks, #etfs, #core, #crypto, #attention, #berkshire, #social, #young, #braked, #reits) table")) {
    const hide = [...t.rows[0].cells].map((th) => v !== "all" && th.textContent in VIEW && !VIEW[th.textContent].split(" ").includes(v));
    for (const tr of t.rows) [...tr.cells].forEach((c, i) => (c.hidden = hide[i]));
  }
  stick();
}

// (#445) One query string for both choosers: each writes its own key and keeps the other's.
function remember(k, v) {
  const u = new URLSearchParams(location.search);
  u.set(k, v);
  history.replaceState(null, "", "?" + u + location.hash);
}

// (#445) A table as CSV, straight from the payload rows: every row and column, cells verbatim.
// (#678) Except a cell a spreadsheet would run: one opening with = @ tab or CR, or with + or - and a ( | or !
// in it (a function call or DDE needs one), gets a leading '. So a feed-supplied name cannot become a live
// link. +9.9pp, -9%ᵈ and every other number shape hold none of those and stay verbatim.
const inert = (f) => (/^[=@\t\r]|^[+-].*[(|!]/s.test(f) ? "'" + f : f);
const csv = (rows) =>
  [rows[0].map(([h]) => h), ...rows.map((r) => r.map(([, c]) => c))]
    .map((line) => line.map((f) => inert(String(f))).map((f) => (/[",\n]/.test(f) ? '"' + f.replaceAll('"', '""') + '"' : f)).join(","))
    .join("\n") + "\n";
console.assert(
  csv([[["A", "1"], ["B", "2"]]]) === "A,B\n1,2\n" && csv([[["N", 'x,"y"']]]) === 'N\n"x,""y"""\n',
  "csv misquotes a field",
);
console.assert(
  csv([[["N", '=HYPERLINK("u","x")']]]) === `N\n"'=HYPERLINK(""u"",""x"")"\n` && csv([[["D", "-2+3+cmd|' /C calc'!A0"]]]) === "D\n'-2+3+cmd|' /C calc'!A0\n" &&
    csv([[["M", "+9.9pp"], ["G", "-9%ᵈ"], ["P", "@SUM(A1)"]]]) === "M,G,P\n+9.9pp,-9%ᵈ,'@SUM(A1)\n",
  "csv lets a formula cell run or escapes a number",
);
// (#1256) Copy a row or a table, as shown, with its column names. A row reads as "COLUMN: value" lines,
// a cell's own note (its payload hover) in brackets; a table as Markdown, which AI tools read better
// than CSV. Pure text here; shown(), title() and clip() read the page and write the clipboard.
const rowText = (title, cells) =>
  [title, ...cells.map(([h, v, note]) => h + ": " + v + (note ? " [" + note.replace(/\s*\n\s*/g, "; ") + "]" : ""))].join("\n");
const md = (s) => s.replaceAll("|", "\\|").replace(/\s*\n\s*/g, " ");
const tableText = (title, heads, rows) =>
  [title, "", ...[heads, heads.map(() => "---"), ...rows].map((r) => "| " + r.map(md).join(" | ") + " |")].join("\n");
console.assert(
  rowText("T", [["A", "1", "x\n y"], ["B", "2"]]) === "T\nA: 1 [x; y]\nB: 2" &&
    tableText("T", ["A", "B"], [["1", "x|y"]]) === "T\n\n| A | B |\n| --- | --- |\n| 1 | x\\|y |",
  "a copied row or table misprints",
);
// The columns shown: the picker's hidden ones left out, and BUY € only while an amount is typed (euros()).
function shown(t) {
  const eur = document.body.classList.contains("euros");
  const head = [...t.rows[0].cells];
  const cols = head.flatMap((c, i) => (!c.hidden && (eur || !c.classList.contains("eur")) ? [i] : []));
  return { cols, heads: cols.map((i) => head[i].textContent || "owned") }; // CORE's flag column prints no header
}
// The table's heading (its text before the CSV link), the data's date line, and the warning.
const title = (t) =>
  document.getElementById("h-" + t.closest("div[id]").id).firstChild.textContent.trim() + " — " + document.getElementById("generated").textContent + ". NOT advice";
// ✓ or ✗ on the button for 1.5 s. A browser without the clipboard throws inside the promise, so it reads ✗.
function clip(b, text) {
  Promise.resolve()
    .then(() => navigator.clipboard.writeText(text))
    .then(() => "ok", () => "no")
    .then((s) => {
      b.dataset.state = s;
      setTimeout(() => delete b.dataset.state, 1500);
    });
}
// (#445) One CSV link per section, in its h2, made once; a re-render (an upload) only swaps the rows.
const sheets = {};
function sheet(id, rows) {
  sheets[id] = rows;
  const h = document.getElementById("h-" + id);
  let a = h.querySelector("a.csv");
  if (!a) {
    a = Object.assign(document.createElement("a"), { className: "csv", textContent: "CSV", download: id + ".csv", href: "#" });
    a.onclick = () => {
      URL.revokeObjectURL(a.href); // the last click's copy; the first href is "#", which revoke ignores
      a.href = URL.createObjectURL(new Blob(["\uFEFF" + csv(sheets[id])], { type: "text/csv;charset=utf-8" })); // the BOM: Excel reads € — ᵇ as ANSI without it
    };
    h.append(" ", a);
  }
  a.hidden = !rows?.length;
}

// (#433) The sticky label columns' left edges: each is the summed width of the columns before it.
// Measured after every cut, because a hidden row can be the one that set a column's width.
function stick() {
  for (const t of document.querySelectorAll(".scroll table")) {
    let left = 0;
    [...t.rows[0].cells].slice(0, 3).forEach((th, i) => {
      t.style.setProperty("--left" + i, left + "px");
      left += th.offsetWidth;
    });
  }
}

// (#393) A cell's sort key. The cells are the terminal's preformatted strings, so a cell is a NUMBER
// only when the whole of it is one, with its sign, €, thousands commas, K/M/B/T and trailing marks
// (`≈+9.9%`, `€1,234.56`, `€1.2B`, `9.9~`, `18.0†`, `9#!xH`, `7#!cb`); a ticker like `2B7A.DE` stays text.
// (#434) c, * and o are rank flags too (#flags note); `7#!cb` sorted as text. (#436) So is w, (#442) and W.
// (#461) $ marks a value read off the US line. (#504) ‡ marks a year-to-date REV-YoY.
// (#505) a superscript digit marks a short-span rate (³ = 3 filed years), ᵈ a 200-day ABV-MA. (#507) ⁱ a since-listing CAGR.
// (#525) ʷ a SCORE the row would get with its failed gates waived. (#576) ˢ a stock's soft-docked SCORE.
// (#584) m a stock the book buys for its momentum, not its score.
// (#625) pp closes a MARGIN-TREND (`+3.8pp`), ᵇ a bank's FAIR, MOS% and FAIR-CAP, e an exchange token's RANK,
// ° a fund PEG on a cached P/E; the first two sorted as text after every number. A lib test reads this line.
// null is a missing cell, which sorts last in either direction; (#506) so do "loss" and "burn", no ratio.
// (#734) "past burn" too, and the (#717) runway ("burn 14.4y"), which had sorted as text. (#735) "lossᵒ" too. (#736) TR-CAGR's short-history ʸ reads its number.
// (#823) A RIGHT/FAIL (or INS-B/S) count `7/3` sorts as its share 7/(7+3); 0/0 measured nothing, so it sorts last.
// (#1196) A GAIN/LOSS `+60.0%/-40.0%` sorts the same way, 60/(60+40); +0.0%/-0.0% sorts last.
// (#1221) A Trading BEST cell `BUY&HOLD +€280.18` sorts on its euros.
const NUM = /^≈?([+-]?)[€$]?([\d,]*\.?\d+)(?:([KMBT])(?: r)?)?[%~†‡#!xHbmc*owWs$⁰¹²³⁴⁵⁶⁷⁸⁹ᵈⁱʷˢʸpᵇeᶠⁿ°]*$/;
function key(cell) {
  if (["", "n/a", "—", "loss", "lossᵒ"].includes(cell) || /^(past )?burn( |$)/.test(cell)) return null;
  const rf = /^(\d+)\/(\d+)$/.exec(cell);
  if (rf) return +rf[1] + +rf[2] ? rf[1] / (+rf[1] + +rf[2]) : null;
  const gl = /^\+([\d.]+)%\/-([\d.]+)%$/.exec(cell);
  if (gl) return +gl[1] + +gl[2] ? gl[1] / (+gl[1] + +gl[2]) : null;
  const mc = /^(\d+): /.exec(cell);
  if (mc) return +mc[1];
  const best = /^[A-Z][A-Z&/()\d ]* ([+-]€[\d,.]+)$/.exec(cell);
  if (best) return key(best[1]);
  const m = NUM.exec(cell);
  return m ? +(m[1] + m[2].replaceAll(",", "")) * ({ K: 1e3, M: 1e6, B: 1e9, T: 1e12 }[m[3]] || 1) : cell;
}
console.assert(
  key("€1,234.56") === 1234.56 && key("≈+9.9%") === 9.9 && key("-0.2%") === -0.2 &&
    key("€1.2B") === 1.2e9 && key("€3.7T") === 3.7e12 && key("9#!xH") === 9 && key("6#!b") === 6 && key("7#!cb") === 7 && key("3*") === 3 && key("4o") === 4 && key("5#ow") === 5 && key("3#wW") === 3 && key("3#wWs") === 3 && key("+17.7%$") === 17.7 && key("+6.1%‡") === 6.1 && key("+12.3%³") === 12.3 && key("-4%ᵈ") === -4 && key("+42%ⁱ") === 42 && key("+756%ʸ") === 756 && key("14.2ʷ") === 14.2 && key("4.1ˢ") === 4.1 && key("€51.20²") === 51.2 && key("+3.8pp") === 3.8 && key("€64.10ᵇ") === 64.1 && key("+28%ᵇ") === 28 && key("€51.3Bᵇ") === 51.3e9 && key("12e") === 12 && key("18.52~°") === 18.52 && key("2B7A.DE") === "2B7A.DE" && key("n/a") === null && key("loss") === null && key("lossᵒ") === null && key("burn") === null && key("past burn") === null && key("burn 14.4y") === null && key("Burnham") === "Burnham" && key("7/3") === 0.7 && key("0/4") === 0 && key("0/0") === null && key("+60.0%/-40.0%") === 0.6 && key("+0.0%/-0.0%") === null && key("+0.0%/-2.5%") === 0,
  "sort key misreads a cell shape",
);
// (#882) a dollar cell, a REIT's ᶠ and a utility's ⁿ read their number; a MISSES or RED-FLAGS `N: list` sorts by N.
console.assert(
  key("$2.9B") === 2.9e9 && key("-$367.1M") === -367.1e6 && key("$100.0B r") === 100e9 && key("$12K") === 12e3 && key("2027 r") === "2027 r" &&
    key("+25%ᶠ") === 25 && key("-41%ⁿ⁰") === -41 && key("€36.5Bᶠ") === 36.5e9 && key("10: a, b") > key("9: c") && key("0") === 0,
  "sort key misreads a dollar, ᶠ, ⁿ or N: list cell",
);
console.assert(
  key("BUY&HOLD +€280.18") === 280.18 && key("SAME DAY -€1,046.85") === -1046.85 && key("HOLD 21 +€9.00") === 9 && key("TREND +€0.00") === 0,
  "sort key misreads a Trading BEST cell",
);
// (#735) A book this tilted is one bet, whatever its name count: the lines the Stocks banner prints,
// off the exposure rows (largest first in each KIND). CI's payload only, like the table it reads.
const SECTOR_TILT = 50;
const CURRENCY_TILT = 80;
function tilt(rows) {
  const at = (row, h) => (row.find(([k]) => k === h) || [])[1];
  const top = (kind, floor) => (rows || []).find((r) => at(r, "KIND") === kind && parseFloat(at(r, "BUY%")) >= floor);
  const s = top("sector", SECTOR_TILT);
  const c = top("currency", CURRENCY_TILT);
  return [
    s && "⚠ " + at(s, "BUY%") + " of the buy list (BUY%) sits in one sector (industry group), " + at(s, "NAME") + ": companies in one sector rise and fall together, so treat them as ONE bet when deciding how much to put in",
    c && "⚠ " + at(c, "BUY%") + " of the buy list (BUY%) trades in one currency, " + at(c, "NAME") + ": for a euro holder that is one bet on the exchange rate",
  ].filter(Boolean);
}
console.assert(
  ((r) => tilt([r("sector", "IT", "84.0%"), r("currency", "USD", "93.0%")]).length === 2 &&
    tilt([r("sector", "IT", "40.0%"), r("currency", "USD", "79.9%")]).length === 0 && tilt(undefined).length === 0)(
    (kind, name, w) => [["KIND", kind], ["NAME", name], ["BUY%", w]],
  ),
  "the tilt banner misreads the exposure rows",
);
// (#526) TR-CAGR wins over the price CAGR; n/a or no TR-CAGR column falls back to it, ⁱ bar and all.
console.assert(
  !under("+9%", "+4%", "10", 5) && under("+4%", "+9%", "10", 5) && under("n/a", "+4%", "10", 5) && !under(undefined, "+6%", "10", 5) &&
    under(undefined, "-8%ⁱ", "0.2", 5) && !under("n/a", "n/a", "1", 5) && !under("-9%", "-9%", "10", 0),
  "the CAGR floor misreads TR-CAGR or its fallback",
);

// First click ascending, then it flips. Numbers before text, missing last, ties in rank order.
function sortBy(th) {
  const dir = th.ariaSort === "ascending" ? -1 : 1;
  for (const h of th.parentElement.cells) h.removeAttribute("aria-sort");
  th.ariaSort = dir > 0 ? "ascending" : "descending";
  const body = th.closest("table").tBodies[0];
  const rows = [...body.rows].map((tr) => [key(tr.cells[th.cellIndex].textContent), tr]);
  rows.sort(([a, x], [b, y]) => {
    if (a === null || b === null) return (a === null) - (b === null) || x.dataset.rank - y.dataset.rank;
    const c = typeof a !== typeof b ? (typeof a === "number" ? -1 : 1)
      : typeof a === "number" ? a - b : a.localeCompare(b);
    return dir * c || x.dataset.rank - y.dataset.rank;
  });
  body.append(...rows.map(([, tr]) => tr));
}

// (#143) `?top=N` rather than localStorage: a bare URL always opens at the default, and a reader who
// wants ten rows every morning bookmarks the URL that gives them. Clamped and truncated because the
// query string is user input — `?top=abc`, `?top=0`, `?top=3.7` and `?top=999` must all land on an
// option that exists, or `sel.value` silently becomes "" and apply(0) blanks every table.
// (#539) The options run to the longest LANE only, then `all` (`?top=all`), which lifts every cut.
// (#1254) A bare URL opens on `all`: a reader sees the whole buy list without finding the chooser
// first (the second-shortest-lane default of (#512) hid most of it). `?top=0` and a non-number open
// on `all` too.
const topN = (v) => (v === "all" ? Infinity : +v);
function chooser(max) {
  const sel = document.getElementById("topn");
  sel.replaceChildren(); // (#392) an upload renders again, with its own row counts
  for (let i = 1; i <= max; i++) sel.add(new Option(i));
  sel.add(new Option("all"));
  const top = new URLSearchParams(location.search).get("top");
  const want = Math.trunc(+top);
  sel.value = top === "all" || !want ? "all" : Math.min(Math.max(want, 1), max);
  sel.onchange = () => {
    apply(topN(sel.value));
    remember("top", sel.value);
  };
  document.getElementById("topn-box").hidden = false; // (#539) `all` is a choice even at one row
  apply(topN(sel.value));
}

// (#1221) What the censuses measured, above the Trading table: each way of trading or holding against
// buying and holding, net of costs. Fixed text, not a payload: the figures are the census records in
// tests/ci-settings.yaml under each receipt, and change only when a census is re-run. Ordered by verdict,
// then by how near each came to beating buy and hold. Every cell hovers its detail. (#1254) No receipt column:
// a reader cannot use a record number, so each row's receipt is the comment closing it.
const APPROACH_HELP = {
  APPROACH: "Approach: a way of trading or holding, tested on past prices after costs. Rows run in this order: what passed, then buy and hold itself, then what failed, each group nearest to beating buy and hold first",
  "WHAT YOU DO": "What you do: what the approach does with the money, in plain words. Hover a cell for the exact rule",
  "RESULT VS BUY&HOLD": "Result against buy and hold: what it earned against buying and holding the same thing over the same years, after costs. Hover a cell for every figure the test gave",
  "WORST FALL": "Worst fall: the deepest fall from a peak along the way, for example 84% = €1,000 shrank to €160 at the bottom. — = the test did not measure it",
  VERDICT: "Verdict: whether it beat buying and holding; PASSES = it cleared the test's bar; BASELINE = buying and holding itself, the line to beat; FAILS = it did not beat buying and holding often enough, or it fell too far",
};
const approach = (cells) => Object.keys(APPROACH_HELP).map((h, i) => [h, cells[i][0], null, cells[i][1]]);
const APPROACHES = [
  [
    ["Lifecycle 200/50", "A saving plan from Ayres and Nalebuff's study Diversification Across Time (2010). It was tested on 682 savers aged 23 to 64, one starting each month from 1928 to 1984. They held the S&P 500 and 10-year US government bonds, measured after inflation"],
    ["Save monthly; borrow young, down to 50% in stock by 64", "Each month, save. Then hold stock worth half of what you have saved plus what you will still save, but never more than 2 times your savings. When young, that means borrowing to buy stock. The loan shrinks as savings grow. At 64, half is in stock and the rest in government bonds"],
    ["Same average as 67-80% in stock, 21-33% less spread", "Tested 8 ways: flat or rising pay, a 2x fund or a margin loan, before or after a 28% tax on gains. A margin loan is money borrowed from the broker. Each way, it ends with the same average result as keeping a fixed 67-80% in stock, with 21-33% less spread between lucky and unlucky savers. It also beats the usual target-date plan (90% in stock, gliding to 50%) by +5.5% to +22.6% on a luck-adjusted score. That score (the certainty equivalent) is the sure sum a cautious saver would take instead of the gamble. It does not earn more than saving all in stock. Flat pay, before tax: the average saver ends with 4.59 times what was paid in; the middle saver all in stock ends with 5.41 times"],
    ["—", "— = not measured: this test graded 200/50 on spread and on the luck-adjusted score, not on its deepest fall"],
    ["PASSES", "It holds both of the study's claims in all 8 ways tested. About 2.4 separate 42-year spans fit in 98 years of history. So the 682 savers are mostly overlapping repeats of two histories. NOT advice: the page never buys on this"],
    // its full record: tests/ci-settings.yaml (#1220)
  ],
  [
    ["Hold stocks 6 months or more", "Buy and hold, selling no sooner than 6 months. Tested on 5,907 names, on 10 years of daily prices and their full monthly history. Every hold from 1 day to 20 years, one buy and one sell each"],
    ["Buy, hold at least 6 months, sell", "A longer hold pays the buying and selling costs fewer times a year. The return before costs barely changes with the hold"],
    ["S&P 500 members of the day 11-13%/yr; 1 week or less loses", "From 6 months on, the honest stock return is about 11 to 13% a year at any hold. Honest means the S&P 500 as its members stood each day, failed companies included. Every hold of 1 week or less loses after costs, in every kind of asset except crypto. In and out every day, close to close, loses 59 to 69% a year. Testing only today's members reads 5 to 7 points a year too high, because the companies that failed are left out (survivor bias)"],
    ["—", "— = not measured by this test"],
    ["BASELINE", "Holding is the line every approach below is measured against"],
    // its full record: tests/ci-settings.yaml (#1217)
  ],
  [
    ["Buy & hold S&P 500, 20 years", "The S&P 500 with dividends reinvested since 1928. Every 20-year span starting on the first day of a month: 947 spans"],
    ["Buy the index, hold 20 years", "All in on the first day (lump sum), or the same sum on the first day of each month (monthly)"],
    ["Middle +11.0%/yr, worst +1.9%/yr", "Lump sum: the middle span grew +11.0% a year, the worst span +1.9% a year. Monthly: the middle span ended with 3.56 times what was paid in, the worst with 1.32 times"],
    ["84% (1929-32)", "The worst fall from a peak: 1929 to 1932, when €1,000 became €160"],
    ["BASELINE", "The line the borrowed-money approaches below are measured against"],
    // its full record: tests/ci-settings.yaml (#1219)
  ],
  [
    ["Lifecycle 200/100", "The same 682 savers as 200/50, holding stock worth all of their savings plus what they will still save"],
    ["Save monthly; borrow young, up to 2x, to 100% in stock by 64", "Each month, save. Then hold stock worth your savings plus what you will still save, never more than 2 times your savings. At 64 it is all in stock"],
    ["Beats 100% stock for every saver before tax: 8.86x vs 5.41x", "Before tax, with a 2x fund, every saver ends richer than saving all in stock, the unluckiest included. The middle saver ends with 8.86 times what was paid in, against 5.41 times. After tax, or with a margin loan, it beats 100% stock for under 80% of savers starting after 1956. 0 of the 8 ways tested clear the bar"],
    ["98-99%; 45 savers fall 90% or more", "In every way tested, 45 savers' accounts fall 90% or more from a peak, down to 98-99%. A real broker would force a sale (a margin call) well before that"],
    ["FAILS", "The bar: beat 100% stock for 80% of savers in each half of history; no saver ending poorer; no fall of 90%. It fails that bar"],
    // its full record: tests/ci-settings.yaml (#1220)
  ],
  [
    ["Hold a 2x S&P 500 fund, 20 years", "1.25, 1.5, 2 or 3 times the S&P 500's daily move, reset every day. Borrowing costs the short-term US government rate plus 0.60% a year, like a 2x fund, or plus 2.5% a year, like a margin loan. Same 947 spans as buy and hold"],
    ["Buy a 2x daily fund, hold 20 years", "The fund doubles each day's move and pays interest on the borrowed half. All in on the first day, or one sum a month"],
    ["Beats 1x in 79% of spans (72% before 1967); worst -5.1%/yr", "2x fund, lump sum: richer than the plain index (1x) in 79% of spans: 72% before 1967, 85% after. The middle span grew +14.0% a year against +11.0%; the worst span -5.1% a year against +1.9%. The best way, 2x monthly, wins 80% (71% before 1967). Its worst span ended with 0.74 times what was paid in, against 1.32 times. 0 of the 16 ways tested clear the bar"],
    ["90% or more, in 46 spans", "Lump-sum 2x falls 90% or more in 46 of the 947 spans. Even 1.25x falls 91% in 1929-32, where the plain index falls 84%"],
    ["FAILS", "The bar: beat the plain index in 80% of spans in each half of history; a worst span no poorer than the index's; no fall of 90%. It fails that bar"],
    // its full record: tests/ci-settings.yaml (#1219)
  ],
  [
    ["Hold 21 trading days", "Tested on 2 years of hourly prices for 503 US and 105 EU names, and on 10 years of daily prices. Every hold from 0 to 21 trading days, €1,000 a hold, one buy and one sell each. Costs: €5 a round trip in the US, €2 in the EU"],
    ["Buy at an open, sell 21 trading days later, again", "The longest hold tested; it made the most per trading day in both markets. The HOLD 21 column below prices it for each name"],
    ["US +€0.44 a trading day vs +€0.67 holding 2 years", "On all US names, the best 21-day hold makes +€0.44 per day held. Buying each name once and holding the 2 years makes +€0.67: the market's general rise, not timing. The EU K7 top 10 makes +€1.07 against +€0.39 for EU buy and hold. But its hourly names are today's members only, so it leaves out the failures (survivor bias). On 10 years of daily prices the gain halves (+€0.53). Holds of 0 to 2 days lose everywhere"],
    ["—", "— = not measured by this test"],
    ["FAILS", "No US hold beats buying and holding. The one EU line that does depends on leaving out the failures"],
    // its full record: tests/ci-settings.yaml (#1215)
  ],
  [
    ["Trade each name its BEST way", "The Trading table's BEST column, but picked in advance. Tested on 605 S&P 500 names and 105 EU names, on 10 years of daily prices, in euros at each day's EUR/USD rate. Every 6 months (or every month), each name takes the one of the four ways that made the most over its last 2 years"],
    ["Trade the way that worked best lately", "Each name trades the way its last 2 years favoured, for the next 6 months or month. The four ways: the same day; 21 trading days at a time; on its 200-day average; or bought once. It is measured against buying and holding the same name, €1,000 a trade, after costs"],
    ["US -€19.00 a name every 6 months; 0 of 4 lines pass", "Against holding the same name, US: -€19.00 a name every 6 months, and -€2.36 a month. EU: -€13.74 every 6 months, and -€1.19 a month. Every half of the years is below zero. Their t-scores are -2.19, -1.40, -1.20 and -0.58 (a t-score beyond about 2 either way means the result is unlikely to be luck). BEST picked buying and holding 45-62% of the time, the trend 25-30%, 21-day holds 13-26%, the same-day trade never"],
    ["—", "— = not measured by this test"],
    ["FAILS", "Switching to the way that did best in the last 2 years lost against holding the same name, in all 4 tests. The BEST column below is hindsight, a label only"],
    // its full record: tests/ci-settings.yaml (#1222)
  ],
  [
    ["Trend, average, breakout, swing", "Tested on 5,907 names: 10 years of daily prices, full monthly history, and 2 years of hourly prices. 47 rules, each with no borrowing (1x) or 2 or 3 times the money (2x, 3x), moving between stock and cash, after costs"],
    ["Hold only while a rule says up", "Trend: hold while the price rose over a set number of past months. Average: hold while the price is above its average over a set number of past days (the TREND column below uses 200 days). Breakout: buy at a new high, sell after a set fall from the peak. Swing: buy after a fall, sell on the bounce"],
    ["Every rule trails buy and hold at 1x; 1 of 1,976 lines passes", "Without borrowing (1x), every rule earns less than holding the same names, in every group tested. On monthly prices, the 12-month trend rule on stocks trails by 13.4 points a year (t-score -6.4, far from luck). The one passing line is one crypto name over 6 months at 2x, which luck alone would explain"],
    ["24% vs 37% holding (stock 12-month trend)", "A trend rule cuts the fall along with the return. The 12-month trend rule on stocks, without borrowing, falls 24% at worst against 37% for holding. But it earns 7.5 points a year less. Borrowing lifts a strong group's average but ruins most of its names"],
    ["FAILS", "No rule beats holding the same names after costs, with or without borrowing, in any group"],
    // its full record: tests/ci-settings.yaml (#1218)
  ],
  [
    ["Calendar slices", "The same-day trade, cut 9 ways: each weekday, the turn of the month against the rest, the day before a holiday, the third-Friday options expiry. Tested on the K7 top 10 and on every name, at every buy and sell hour. That is 1,314 pairs on 2 years of hourly prices, checked against 10 years of daily prices"],
    ["Trade only on a set day or date", "For example, buy only on a Monday at 10:00 and sell later that day"],
    ["2 of 1,314 pass, by luck; lose on 10 years", "Only the EU K7 top 10 on Mondays passes, from 10:00 to 17:00 or the close. It makes about €0.35-0.42 a Monday on €1,000, about €20 a year. With a t-score bar of 3, luck alone passes about 2 of 1,314 pairs. On 10 years of daily prices it earns only half its cost: +0.10% a trade before costs, -€0.96 after"],
    ["—", "— = not measured by this test"],
    ["FAILS", "Every US slice loses after costs. So does every EU slice of all the names with more than 11 days in it"],
    // its full record: tests/ci-settings.yaml (#1216)
  ],
  [
    ["Intraday, same day", "The Trading table's own trade: the K7 top 10, bought at the open and sold the same day. Tested on 10 years of daily prices for every way of picking the 10. Also on 2 years of hourly prices, for every buy and sell hour"],
    ["Buy at the open, sell before the close", "One buy and one sell each day: €5 a €1,000 trade in the US, €2 in the EU. The SAME DAY column below prices it for each name, in hindsight"],
    ["K7 -€4.80 a US day, -€1.60 an EU one", "At €1,000 a day, the K7 top 10 loses about €4.80 a US day and €1.60 an EU one. The 10-year test loses with every way of picking the 10: -0.49% to -0.52% a day in the US, -0.16% to -0.20% in the EU. Every buy and sell hour pair loses after costs"],
    ["—", "— = not measured by this test"],
    ["FAILS", "No way of picking and no pair of hours has made money after costs"],
    // its full record: tests/ci-settings.yaml (#810) (#828) (#1214)
  ],
].map(approach);

const stamp = (t) => new Date(t).toISOString().replace("T", " ").replace(".000Z", "Z");

// (#401) `prev` is yesterday's last deploy (prev.json), or nothing: an upload passes none, because an
// uploaded ranking against CI's yesterday would compare two different rankings.
function render(data, prev) {
  const when = new Date(data.generated);
  // (#394) "generated" read as the data's own date; it is when CI fetched and ranked, and each price is
  // whatever its market last quoted then (a weekend run carries Friday's close for stocks and ETFs).
  document.getElementById("generated").textContent =
    "fetched & ranked " + stamp(data.generated) + " — prices are each market's latest quote at that time";
  const ageHours = (Date.now() - when.getTime()) / 3.6e6;
  if (ageHours > STALE_HOURS) {
    const banner = document.getElementById("stale");
    banner.textContent =
      "Stale: this is " + Math.floor(ageHours / 24) + "d " + Math.floor(ageHours % 24) +
      "h old. The last refresh did not complete, so these are not today's numbers.";
    banner.hidden = false;
  }
  // (#378) screen's DEGRADED line: feeds this run went without. Today's numbers, just thinner.
  // `?.` because a payload written before the field existed has no key at all.
  if (data.degraded?.length) {
    const d = document.getElementById("degraded");
    d.textContent = "Thinner run: some data sources did not answer this time, so some numbers are missing: " + data.degraded.join("; ") + ".";
    d.hidden = false;
  }
  // (#400) A payload written before `help` existed has no key, and renders untitled.
  const help = data.help || {};
  // (#474) the "rank flags" note, from the same list as each RANK cell's hover
  document.getElementById("flag-list").replaceChildren(
    ...Object.entries(help.flags || {}).map(([f, what]) => {
      const li = document.createElement("li");
      li.append(Object.assign(document.createElement("code"), { textContent: f }), " " + what);
      return li;
    }),
  );
  // (#401) The tickers now in a table that its prev copy lacked. No prev, or a prev from before that
  // table existed, marks nothing; an empty prev table marks every row, since they all joined.
  const tickers = (rows) => new Set((rows || []).flatMap((row) => row.filter(([h]) => h === "TICKER").map(([, c]) => c)));
  let marked = 0;
  const fresh = (part) => {
    if (!Array.isArray(prev?.[part])) return new Set();
    const was = tickers(prev[part]);
    const now = new Set([...tickers(data[part])].filter((t) => !was.has(t)));
    marked += now.size;
    return now;
  };
  for (const lane of LANES) {
    document.getElementById(lane).replaceChildren(table(data[lane], undefined, help.lanes, fresh(lane), help.flags));
  }
  // A run whose inflation feeds all failed publishes an empty list; say that, rather than borrowing
  // the lanes' "(none pass every buy rule)", which would read as a gate verdict on a macro series.
  document
    .getElementById("inflation")
    .replaceChildren(table(data.inflation, "(inflation feeds unavailable)", help.inflation));
  // (#250) Same reason the inflation table gets its own empty string: these funds pass their own
  // suitability filter, not the growth gates, so "(none pass every buy rule)" would name the wrong test.
  document
    .getElementById("core")
    .replaceChildren(table(data.core, "(no CORE fund qualified)", help.core, fresh("core")));
  // (#502) (#525) per-table CAGR floors; (#530) an upload's engine output carries its own
  if (typeof data.cagr_floor === "number") for (const id of SHADOW) floors[id] = id === "reits" ? data.reit_cagr_floor : id === "social" ? (data.social_cagr_floor ?? data.cagr_floor) : data.cagr_floor; // (#538) social: its own
  if (typeof data.page_min_cagr === "number") {
    minCagr = data.page_min_cagr; // (#526)
    // (#555) the label names the hide bar, not the grey one: a row between the two is greyed yet always shown
    document.getElementById("mincagr").textContent = minCagr;
    document.getElementById("allrows-label").title = `Rows that grew less than ${minCagr}% a year (TR-CAGR, dividends included) stay hidden until you tick the box, or find them with the filter. Greyed rows grew less than their own table's minimum (${floors.attention}% a year, REITs ${floors.reits}% a year); they always show.`;
  }
  // (#438) Only CI's payload carries it: an upload's engine output has no key, so CI's table stays.
  if (data.attention) {
    document.getElementById("attention").replaceChildren(table(data.attention, "(attention feed unavailable)", help.attention));
  }
  // (#486) CI's FRED fetch, so an upload keeps CI's table like the shadow tables below.
  if (data.bonds) {
    document.getElementById("bonds").replaceChildren(table(data.bonds, "(bond feeds unavailable)", help.bonds));
  }
  if (data.berkshire) {
    document.getElementById("berkshire").replaceChildren(table(data.berkshire, "(Berkshire 13F unavailable)", help.berkshire));
  }
  if (data.social) {
    document.getElementById("social").replaceChildren(table(data.social, "(no source answered)", help.social));
  }
  // (#500) CI's payload only, like the shadow tables above
  if (data.young) {
    document.getElementById("young").replaceChildren(table(data.young, "(no stock listed under 5 years)", help.young));
  }
  // (#580) CI's payload only, like the shadow tables above
  if (data.braked) {
    document.getElementById("braked").replaceChildren(table(data.braked, "(no stock kept out by the ran-too-far cut alone)", help.braked));
  }
  // (#515) CI's payload only; not a shadow table, so no stock columns and no CAGR cut
  // (#525) CI's payload only, like the shadow tables above
  if (data.reits) {
    document.getElementById("reits").replaceChildren(table(data.reits, "(no REIT, property fund or SIGI priced)", help.reits));
  }
  if (data.upcoming) {
    document.getElementById("upcoming").replaceChildren(table(data.upcoming, "(no $1B+ US IPO filed, expected or watched)", help.upcoming));
  }
  // (#811) CI's Yahoo daily bars, so an upload keeps CI's table like the shadow tables above
  if (data.intraday) {
    document.getElementById("intraday").replaceChildren(table(data.intraday, "(off, or no name ranked)", help.intraday));
  }
  // (#1223) and the stocks BUY NOW lists, priced the same ways
  if (data.trading_buynow) {
    document.getElementById("trading_buynow").replaceChildren(table(data.trading_buynow, "(off, or no buy-list stock priced in EUR or USD)", help.trading_buynow));
  }
  document.getElementById("approaches").replaceChildren(table(APPROACHES, undefined, APPROACH_HELP)); // (#1221) fixed text, any payload
  // (#1140) CI's journal, so an upload keeps CI's table
  if (data.track) {
    document.getElementById("track").replaceChildren(table(data.track, "(nothing to measure yet)", help.track));
    document.getElementById("track-note").textContent = data.track_note || "";
  }
  // (#456) Only CI's payload carries it, and unlike the shadow tables it describes the BUY% book, which
  // an upload changes: the page has no fund holdings to look a new book through, so it says so.
  document
    .getElementById("exposure")
    .replaceChildren(
      data.exposure
        ? table(data.exposure, "(no buy list to look through)", help.exposure)
        : Object.assign(document.createElement("p"), {
            className: "sub",
            textContent: "This table describes the page's own buy list only. Your settings file changes BUY%, and the page cannot open up the funds again to redo it.",
          }),
    );
  sheet("exposure", data.exposure);
  const lines = tilt(data.exposure);
  const conc = document.getElementById("conc");
  conc.hidden = !lines.length;
  conc.replaceChildren(...lines.flatMap((l, i) => (i ? [document.createElement("br"), l] : [l])));
  const since = document.getElementById("since");
  since.hidden = !prev;
  since.textContent = prev ? "new = joined its table since " + stamp(prev.generated) + " (" + marked + " rows marked new)" : "";
  // Options come from the LONGEST lane, and each table then caps itself at its own length — crypto
  // routinely has fewer rows than stocks, and offering an N no lane can fill would be a lie.
  const sizes = LANES.map((lane) => (data[lane] || []).length);
  chooser(Math.max(1, ...sizes));
  view(viewSel.value);
  euros(); // (#1254) an upload renders new rows: their BUY € cells take the amount already typed
  // (#1254) The start box names one whole-world fund in CORE; (#1258) which one is `startFund`'s.
  document.getElementById("start-fund").textContent = startFund(data.core || [], data.help?.core?.MISSES || "");
  // (#445) An upload carries no shadow tables, so CI's CSV rows for those stay, like the tables do.
  for (const id of [...LANES, "core", "inflation", "bonds", "attention", "berkshire", "social", "young", "braked", "reits", "upcoming", "intraday", "trading_buynow", "track"]) {
    if (data[id]) sheet(id, data[id]);
  }
  glossary(data, { ...help, approaches: APPROACH_HELP });
  // (#433) Each table and its scroll box are named by the h2 above them, `h-` + the holder's id.
  // (#1256) Each heading also gets one copy-the-table button beside its CSV link, made once; a table
  // gone on an upload re-render leaves its button hidden.
  for (const b of document.querySelectorAll("button.whole")) b.hidden = true;
  for (const t of document.querySelectorAll(".scroll table")) {
    const by = "h-" + t.closest("div[id]").id;
    t.setAttribute("aria-labelledby", by);
    t.parentElement.setAttribute("aria-labelledby", by);
    const h = document.getElementById(by);
    let b = h.querySelector("button.whole");
    if (!b) {
      b = Object.assign(document.createElement("button"), { className: "copy whole", type: "button", ariaLabel: "copy table", title: "Copy the table as shown, with its column names, as a Markdown table" });
      h.append(" ", b);
    }
    b.hidden = false;
    b.onclick = () => {
      const { cols, heads } = shown(t);
      clip(b, tableText(title(t), heads, [...t.tBodies[0].rows].filter((tr) => !tr.hidden).map((tr) => cols.map((i) => tr.cells[i].textContent))));
    };
  }
}

// (#400) The unit the ≥1Y columns were printed in is the head of their help ("Nominal — …"), and the
// glossary is every shown header's help, lanes first (their headers in first-seen order), then CORE,
// then Inflation.
function glossary(data, help) {
  document.getElementById("unit").textContent = help.lanes?.["2Y"]?.split(" — ")[0] || "(unit not stated)";
  const dl = document.createElement("dl");
  const tag = (name, text, cls) => Object.assign(document.createElement(name), { textContent: text, className: cls || "" });
  for (const [name, part, tables] of [
    ["Stocks, ETFs, Crypto", "lanes", LANES.map((lane) => data[lane])],
    ["Exposure", "exposure", [data.exposure]],
    ["CORE", "core", [data.core]],
    ["Inflation", "inflation", [data.inflation]],
    ["Bonds", "bonds", [data.bonds]],
    // (#452) the shadow tables. An upload carries none, so these groups drop from its glossary; the
    // tables CI rendered keep their hover text.
    ["Attention", "attention", [data.attention]],
    ["Berkshire holdings", "berkshire", [data.berkshire]],
    ["Social Arbitrage trading", "social", [data.social]],
    ["Young", "young", [data.young]],
    ["Ran too far", "braked", [data.braked]],
    ["REITs and SIGIs", "reits", [data.reits]],
    ["Upcoming IPOs", "upcoming", [data.upcoming]],
    ["Trading approaches", "approaches", [APPROACHES]],
    ["Trading", "intraday", [data.intraday, data.trading_buynow]],
    ["Track record", "track", [data.track]],
  ]) {
    const heads = [...new Set(tables.flatMap((rows) => (rows?.[0] || []).map(([h]) => h)))];
    const known = heads.filter((h) => help[part]?.[h]);
    if (known.length) dl.append(tag("dt", name, "group"));
    for (const h of known) dl.append(tag("dt", h || "(unnamed first column)"), tag("dd", help[part][h]));
  }
  const box = document.getElementById("glossary");
  box.querySelector("dl").replaceWith(dl);
  box.hidden = !dl.children.length;
}

// (#445) `?view=` wins when it names an option; otherwise the 700px breakpoint the sticky columns use.
const viewSel = document.getElementById("view");
const wantView = new URLSearchParams(location.search).get("view");
viewSel.value = [...viewSel.options].some((o) => o.value === wantView) ? wantView
  : matchMedia("(min-width: 700px)").matches ? "all" : "overview";
viewSel.onchange = () => {
  view(viewSel.value);
  remember("view", viewSel.value);
};
document.getElementById("filter").oninput = document.getElementById("allrows").onchange = () =>
  apply(topN(document.getElementById("topn").value));
// (#1254) The amount box fills every BUY € cell, and shows the column only while it holds an amount.
function euros() {
  const amt = +document.getElementById("amount").value;
  document.body.classList.toggle("euros", amt > 0);
  for (const td of document.querySelectorAll("td.eur")) td.textContent = euro(amt, +td.dataset.pct);
}
document.getElementById("amount").oninput = euros;

// (#401) prev.json is best effort: missing, unreadable or undated is no marks, never an error.
function load() {
  const prev = fetch("prev.json", { cache: "no-store" })
    .then((r) => (r.ok ? r.json() : null))
    .then((p) => (Date.parse(p?.generated) ? p : null))
    .catch(() => null);
  fetch("data.json", { cache: "no-store" })
    .then((r) => (r.ok ? r.json() : Promise.reject(new Error("HTTP " + r.status))))
    .then(async (data) => render(data, await prev))
    .catch((e) => {
      document.getElementById("generated").textContent = "could not load data.json (" + e.message + ")";
    });
}
load();

// (#392) The upload. The file is read here and handed to engine/worker.js; it never leaves the tab.
// A fresh worker per upload, because the engine reads its config once per instance.
const upload = document.getElementById("upload");
const mine = document.getElementById("mine");
let current = ""; // (#403) the overlay the tables show, which the why box asks about
let run = 0; // (#883) the newest upload's number: an older worker's answer, or one back after "back to the page's own ranking", is dropped
function say(text, reset) {
  mine.textContent = text;
  if (reset) {
    const a = document.createElement("a");
    a.href = "#";
    a.textContent = "back to the page's own ranking";
    a.onclick = (e) => {
      e.preventDefault();
      upload.value = "";
      run++;
      current = "";
      asked++; // (#898) an answer about the old ranking, shown or still in flight, goes with it
      whyOut.hidden = true;
      mine.hidden = true;
      load();
    };
    mine.append(" ", a);
  }
  mine.hidden = false;
}
// (#397) The engine's `pins`: which uploaded pins show as the pool's listing of the same fund, and
// which the pool does not hold at all. Absent when every pin is a pool row.
function pinNote(p) {
  if (!p) return "";
  const twins = p.twins.map(([pin, row]) => pin + " is shown as " + row).join(", ");
  return (twins ? " " + twins + " (the same fund, on the listing this page prices)." : "") +
    (p.missing.length ? " Not among the assets this page fetched, so not shown: " + p.missing.join(", ") + "." : "");
}
upload.onchange = async () => {
  const file = upload.files[0];
  if (!file) return;
  upload.value = ""; // (#883) so picking the same path again, after editing it, fires `change` again
  const me = ++run;
  say("ranking with " + file.name + "…");
  const overlay = await file.text();
  const w = new Worker("engine/worker.js", { type: "module" });
  w.onmessage = ({ data }) => {
    w.terminate();
    if (me !== run) return;
    if (data.err) return say("Could not rank with " + file.name + ": " + data.err, true);
    const out = JSON.parse(data.ok);
    current = overlay;
    asked++; // (#898) likewise
    whyOut.hidden = true;
    render(out);
    say(
      "Ranked with your " + file.name + ", in this browser: the file was never uploaded. " +
        "Your file cannot change what the page fetched: the list of assets, the dip and high windows, inflation, and when a price is too old." +
        pinNote(out.pins),
      true,
    );
  };
  w.onerror = (e) => {
    w.terminate();
    if (me !== run) return;
    say("The engine failed to start (" + (e.message || "see the browser console") + ").", true);
  };
  w.postMessage({ overlay });
};
// (#403) "Why isn't X in?". A fresh worker per question, like an upload: the engine reads its config
// once per instance.
const whyOut = document.getElementById("why-out");
let asked = 0; // (#883) likewise for the why box: only the newest question's answer lands
document.getElementById("why-box").onsubmit = (e) => {
  e.preventDefault();
  const me = ++asked;
  whyOut.textContent = "…";
  whyOut.hidden = false;
  const w = new Worker("engine/worker.js", { type: "module" });
  w.onmessage = ({ data }) => {
    w.terminate();
    if (me !== asked) return;
    whyOut.textContent = data.err ? "Could not explain: " + data.err : data.ok;
  };
  w.onerror = (e) => {
    w.terminate();
    if (me !== asked) return;
    whyOut.textContent = "The engine failed to start (" + (e.message || "see the browser console") + ").";
  };
  w.postMessage({ overlay: current, why: document.getElementById("why").value });
};
fetch("engine/folioman_engine_bg.wasm", { method: "HEAD" })
  .then((r) => { for (const id of ["upload-box", "why-box"]) document.getElementById(id).hidden = !r.ok; })
  .catch(() => {});
