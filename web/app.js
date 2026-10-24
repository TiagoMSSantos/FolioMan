// The payload is {generated, stocks, etfs, crypto}, each lane a list of ROWS and each row a list of
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
function local(cell) {
  const [, a, b, c, d, city] = String(cell).match(/^(\d\d):(\d\d)-(\d\d):(\d\d) (.+)$/) ?? [];
  if (!ZONES[city]) return "";
  const at = (h, m) => new Date(wall(+h, +m, ZONES[city])).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
  return cell + " = " + at(a, b) + "-" + at(c, d) + " your time (" + Intl.DateTimeFormat().resolvedOptions().timeZone + ")";
}
console.assert(
  wall(9, 30, "America/New_York", new Date(Date.UTC(2026, 0, 15, 12))) === Date.UTC(2026, 0, 15, 14, 30) &&
    wall(17, 30, "Europe/Berlin", new Date(Date.UTC(2026, 6, 15, 12))) === Date.UTC(2026, 6, 15, 15, 30) &&
    local("09:30-16:00 New York").startsWith("09:30-16:00 New York = ") && local("09:00-17:30 Tokyo") === "",
  "a SESSION hover misreads its exchange's clock",
);

function table(rows, empty = "(none pass the gates)", help = {}, fresh = new Set(), flags = {}) {
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
  for (const [header] of rows[0]) {
    const th = document.createElement("th");
    th.scope = "col";
    const b = document.createElement("button");
    b.textContent = header;
    b.title = help[header] || ""; // (#400) hover text; the glossary below the tables lists the same
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
      if (tip) td.title = tip; // (#542) a REPORTS cell's hover: the description and last annual report; (#543) a MISSES cell's: each missed gate's value vs its bar. Text only
      else if (/^(n\/a|—|burn)/.test(cell)) td.title = why(help[h], cell); // (#718)
      else if (h !== "NAME" && h !== "TICKER") td.title = hover(help, h, String(cell)); // (#732) NAME and TICKER link; their link is the hover
      // (#474) a RANK cell's hover reads its position, then what each of its flags means
      if (h === "RANK" && cell) {
        const [, n, marks] = cell.match(/^(\d*)(.*)$/);
        td.title = ["rank " + n + " in this table", ...[...marks].filter((f) => flags[f]).map((f) => f + " " + flags[f])].join("\n");
      }
      if (h === "SESSION") td.title = local(cell) || td.title; // (#824) the Intraday hours on the reader's own clock
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
// (#539) N cuts every asset table, lanes and those below alike; Exposure stays whole and `all` is no cut.
// (#393) Cut on the RANK, not the row's position: a sort reorders the top N, never swaps who is in it.
// (#445) A filter query overrides the cut in every table but inflation and (#486) bonds: a row shows iff its TICKER or
// NAME holds the query, so a name the chooser hid can still be found. An empty query is the cut alone.
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
    if (t.closest("#inflation, #bonds, #intraday")) continue; // (#811) every intraday row shows, unfiltered
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

// (#445) Column groups for the three ranked tables, finviz-style. A header NOT listed here shows in
// every view, so RANK NAME TICKER BUY% (and any column added later) can never be hidden by accident.
// (#493) A header may sit in several views, space-separated: `cash` regroups the cash-flow and debt
// columns that `value` and `risk` split, so a phone reader finds them in one pick.
const VIEW = {
  MARKET: "overview", SECTOR: "overview", DOM: "overview", EARN: "overview", "PRICE(EUR)": "overview", SCORE: "overview",
  CAGR: "returns", YRS: "returns", "1D": "returns", "1W": "returns", "1M": "returns", "2Y": "returns",
  "5Y": "returns", "8Y": "returns", "20Y": "returns", "S-8Y": "returns",
  VOL: "risk", MAXDD: "risk", R2: "risk", "ABV-MA": "risk", "OFF-HI": "risk", TURNOVER: "risk",
  "5Y-WIN%": "risk", "WORST-5Y": "risk", "UW-YRS": "risk", "ND/EBITDA": "risk cash", "INT-COV": "risk cash", "SHORT%": "risk", "RED-FLAGS": "risk",
  MCAP: "value", "P/E": "value", PEG: "value", "ROE/A": "value", "REV-YoY": "value", "REV-5Y": "value", "FCF-5Y": "value cash", "OP%": "value", "MARGIN-TREND": "value", "M-SCORE": "risk", "SBC%": "value", "EPS-YoY": "value",
  "NET%": "value", DIV: "value", BUYBK: "value", TER: "value", AUM: "value", USE: "value", REPL: "value",
  MVRV: "value", ROIC: "value", "FCF%": "value cash", "CAPEX/OCF": "value cash", FAIR: "value", "MOS%": "value", "FAIR-OE": "value", "MOS-OE%": "value", "FAIR-CAP": "value", "IMPL-G": "value", "TARGET%": "value", REPORTS: "overview value", "INS-B/S": "value",
  "TOP10%": "risk", "TD-1Y": "value", "TD-5Y": "value",
  "FCF-YLD": "value cash", "P/S": "value", "EV/EBITDA": "value",
};
function view(v) {
  for (const t of document.querySelectorAll(":is(#stocks, #etfs, #crypto, #attention, #berkshire, #social, #young, #braked, #reits) table")) {
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
const NUM = /^≈?([+-]?)[€$]?([\d,]*\.?\d+)(?:([KMBT])(?: r)?)?[%~†‡#!xHbmc*owWs$⁰¹²³⁴⁵⁶⁷⁸⁹ᵈⁱʷˢʸpᵇeᶠⁿ°]*$/;
function key(cell) {
  if (["", "n/a", "—", "loss", "lossᵒ"].includes(cell) || /^(past )?burn( |$)/.test(cell)) return null;
  const rf = /^(\d+)\/(\d+)$/.exec(cell);
  if (rf) return +rf[1] + +rf[2] ? rf[1] / (+rf[1] + +rf[2]) : null;
  const mc = /^(\d+): /.exec(cell);
  if (mc) return +mc[1];
  const m = NUM.exec(cell);
  return m ? +(m[1] + m[2].replaceAll(",", "")) * ({ K: 1e3, M: 1e6, B: 1e9, T: 1e12 }[m[3]] || 1) : cell;
}
console.assert(
  key("€1,234.56") === 1234.56 && key("≈+9.9%") === 9.9 && key("-0.2%") === -0.2 &&
    key("€1.2B") === 1.2e9 && key("€3.7T") === 3.7e12 && key("9#!xH") === 9 && key("6#!b") === 6 && key("7#!cb") === 7 && key("3*") === 3 && key("4o") === 4 && key("5#ow") === 5 && key("3#wW") === 3 && key("3#wWs") === 3 && key("+17.7%$") === 17.7 && key("+6.1%‡") === 6.1 && key("+12.3%³") === 12.3 && key("-4%ᵈ") === -4 && key("+42%ⁱ") === 42 && key("+756%ʸ") === 756 && key("14.2ʷ") === 14.2 && key("4.1ˢ") === 4.1 && key("€51.20²") === 51.2 && key("+3.8pp") === 3.8 && key("€64.10ᵇ") === 64.1 && key("+28%ᵇ") === 28 && key("€51.3Bᵇ") === 51.3e9 && key("12e") === 12 && key("18.52~°") === 18.52 && key("2B7A.DE") === "2B7A.DE" && key("n/a") === null && key("loss") === null && key("lossᵒ") === null && key("burn") === null && key("past burn") === null && key("burn 14.4y") === null && key("Burnham") === "Burnham" && key("7/3") === 0.7 && key("0/4") === 0 && key("0/0") === null,
  "sort key misreads a cell shape",
);
// (#882) a dollar cell, a REIT's ᶠ and a utility's ⁿ read their number; a MISSES or RED-FLAGS `N: list` sorts by N.
console.assert(
  key("$2.9B") === 2.9e9 && key("-$367.1M") === -367.1e6 && key("$100.0B r") === 100e9 && key("$12K") === 12e3 && key("2027 r") === "2027 r" &&
    key("+25%ᶠ") === 25 && key("-41%ⁿ⁰") === -41 && key("€36.5Bᶠ") === 36.5e9 && key("10: a, b") > key("9: c") && key("0") === 0,
  "sort key misreads a dollar, ᶠ, ⁿ or N: list cell",
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
    s && "⚠ " + at(s, "BUY%") + " of BUY% sits in one sector, " + at(s, "NAME") + ": names in one sector move together; treat it as ONE bet when sizing",
    c && "⚠ " + at(c, "BUY%") + " of BUY% trades in one currency, " + at(c, "NAME") + ": one FX bet for a euro holder",
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

// (#512) The default is the SECOND-shortest non-empty lane (stocks 7, ETFs 8, crypto 1 -> 7): the
// shortest-lane default (#396) opened every table at one row once crypto narrowed to BTC. The shortest
// lane caps itself at its own length; one non-empty lane gives its own size, and none gives 1.
const secondShortest = (sizes) => {
  const s = sizes.filter(Boolean).sort((a, b) => a - b);
  return s[1] ?? s[0] ?? 1;
};
console.assert(
  secondShortest([7, 8, 1]) === 7 && secondShortest([0, 3, 0]) === 3 &&
    secondShortest([0, 0, 0]) === 1 && secondShortest([5, 5, 2]) === 5,
  "secondShortest",
);

// (#143) `?top=N` rather than localStorage: a bare URL always opens at the default, and a reader who
// wants ten rows every morning bookmarks the URL that gives them. Clamped and truncated because the
// query string is user input — `?top=abc`, `?top=0`, `?top=3.7` and `?top=999` must all land on an
// option that exists, or `sel.value` silently becomes "" and apply(0) blanks every table.
// (#539) The options run to the longest LANE only, then `all` (`?top=all`), which lifts every cut.
const topN = (v) => (v === "all" ? Infinity : +v);
function chooser(max, dflt) {
  const sel = document.getElementById("topn");
  sel.replaceChildren(); // (#392) an upload renders again, with its own row counts
  for (let i = 1; i <= max; i++) sel.add(new Option(i));
  sel.add(new Option("all"));
  const top = new URLSearchParams(location.search).get("top");
  const want = Math.trunc(+top) || dflt;
  sel.value = top === "all" ? "all" : Math.min(Math.max(want, 1), max);
  sel.onchange = () => {
    apply(topN(sel.value));
    remember("top", sel.value);
  };
  document.getElementById("topn-box").hidden = false; // (#539) `all` is a choice even at one row
  apply(topN(sel.value));
}

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
    d.textContent = "Degraded run: " + data.degraded.join("; ") + ".";
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
  // the lanes' "(none pass the gates)", which would read as a gate verdict on a macro series.
  document
    .getElementById("inflation")
    .replaceChildren(table(data.inflation, "(inflation feeds unavailable)", help.inflation));
  // (#250) Same reason the inflation table gets its own empty string: these funds pass their own
  // suitability filter, not the growth gates, so "(none pass the gates)" would name the wrong test.
  document
    .getElementById("core")
    .replaceChildren(table(data.core, "(no CORE fund qualified)", help.core, fresh("core")));
  // (#502) (#525) per-table CAGR floors; (#530) an upload's engine output carries its own
  if (typeof data.cagr_floor === "number") for (const id of SHADOW) floors[id] = id === "reits" ? data.reit_cagr_floor : id === "social" ? (data.social_cagr_floor ?? data.cagr_floor) : data.cagr_floor; // (#538) social: its own
  if (typeof data.page_min_cagr === "number") {
    minCagr = data.page_min_cagr; // (#526)
    // (#555) the label names the hide bar, not the grey one: a row between the two is greyed yet always shown
    document.getElementById("mincagr").textContent = minCagr;
    document.getElementById("allrows-label").title = `Rows whose TR-CAGR is under ${minCagr}%/yr stay hidden until ticked (or found with the filter). Greyed rows are under their table's floor (${floors.attention}%/yr, REITs ${floors.reits}%/yr) and always show.`;
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
    document.getElementById("braked").replaceChildren(table(data.braked, "(no stock kept out by the brake alone)", help.braked));
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
  // (#456) Only CI's payload carries it, and unlike the shadow tables it describes the BUY% book, which
  // an upload changes: the page has no fund holdings to look a new book through, so it says so.
  document
    .getElementById("exposure")
    .replaceChildren(
      data.exposure
        ? table(data.exposure, "(no book to look through)", help.exposure)
        : Object.assign(document.createElement("p"), {
            className: "sub",
            textContent: "Describes CI's default book only: an uploaded settings.yaml changes BUY%, and the page has no fund holdings to redo it.",
          }),
    );
  sheet("exposure", data.exposure);
  const lines = tilt(data.exposure);
  const conc = document.getElementById("conc");
  conc.hidden = !lines.length;
  conc.replaceChildren(...lines.flatMap((l, i) => (i ? [document.createElement("br"), l] : [l])));
  const since = document.getElementById("since");
  since.hidden = !prev;
  since.textContent = prev ? "new = joined its table since " + stamp(prev.generated) + " (" + marked + " marked)" : "";
  // Options come from the LONGEST lane, and each table then caps itself at its own length — crypto
  // routinely has fewer rows than stocks, and offering an N no lane can fill would be a lie.
  const sizes = LANES.map((lane) => (data[lane] || []).length);
  chooser(Math.max(1, ...sizes), secondShortest(sizes));
  view(viewSel.value);
  // (#445) An upload carries no shadow tables, so CI's CSV rows for those stay, like the tables do.
  for (const id of [...LANES, "core", "inflation", "bonds", "attention", "berkshire", "social", "young", "braked", "reits", "upcoming", "intraday"]) {
    if (data[id]) sheet(id, data[id]);
  }
  glossary(data, help);
  // (#433) Each table and its scroll box are named by the h2 above them, `h-` + the holder's id.
  for (const t of document.querySelectorAll(".scroll table")) {
    const by = "h-" + t.closest("div[id]").id;
    t.setAttribute("aria-labelledby", by);
    t.parentElement.setAttribute("aria-labelledby", by);
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
    ["Braked", "braked", [data.braked]],
    ["REITs and SIGIs", "reits", [data.reits]],
    ["Upcoming IPOs", "upcoming", [data.upcoming]],
    ["Intraday", "intraday", [data.intraday]],
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
let run = 0; // (#883) the newest upload's number: an older worker's answer, or one back after "back to CI's ranking", is dropped
function say(text, reset) {
  mine.textContent = text;
  if (reset) {
    const a = document.createElement("a");
    a.href = "#";
    a.textContent = "back to CI's ranking";
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
  return (twins ? " " + twins + " (the same fund, on the pool's listing)." : "") +
    (p.missing.length ? " Not in CI's pool, so not shown: " + p.missing.join(", ") + "." : "");
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
      "Ranked with your " + file.name + ", in this browser: the file was never uploaded. What " +
        "screen decided while fetching stays CI's: the universe, the dip/high windows, inflation, stale_days." +
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
