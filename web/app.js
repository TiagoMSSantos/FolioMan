// The payload is {generated, stocks, etfs, crypto}, each lane a list of ROWS and each row a list of
// [header, cell] pairs — exactly the cells the terminal prints for that row, so nothing is
// reformatted here. (#143) A lane was a single row until then; every row of a lane carries the same
// headers in the same order, which is why the head is read off row 0.
const LANES = ["stocks", "etfs", "crypto"];
const STALE_HOURS = 36; // one weekday cron miss is fine; two is worth saying out loud

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
    // (#403) so does a `b` row: the book buys it though a display trim cut it from the table
    if (row.some(([h, c]) => h === "RANK" && /[*b]/.test(c))) tr.dataset.pin = "";
    for (const [h, cell, url] of row) {
      const td = tr.insertCell();
      // (#474) a RANK cell's hover reads its position, then what each of its flags means
      if (h === "RANK") {
        const [, n, marks] = cell.match(/^(\d*)(.*)$/);
        td.title = ["rank " + n + " in this table", ...[...marks].filter((f) => flags[f]).map((f) => f + " " + flags[f])].join("\n");
      }
      // (#401) Every ticker is already a Yahoo symbol: the pool is fetched from Yahoo. (#455) A shadow
      // table's cell may carry its source page as a third element; only https ever becomes a link.
      // (#482) A NAME with no page of its own links the row's Yahoo quote, and so does an Exposure
      // `name` row, whose NAME is the symbol.
      const yahoo = (t) => "https://finance.yahoo.com/quote/" + encodeURIComponent(t);
      const sym = h === "NAME" && !url?.startsWith("https://") && (row.find(([k]) => k === "TICKER")?.[1] ?? (row.some(([k, c]) => k === "KIND" && c === "name") ? cell : null));
      const href = h === "TICKER" ? yahoo(cell) : sym ? yahoo(sym) : url;
      if (!href?.startsWith("https://")) {
        td.textContent = cell;
        continue;
      }
      td.append(Object.assign(document.createElement("a"), { href, target: "_blank", rel: "noopener", textContent: cell }));
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
// the view most likely to be bookmarked. (#438) The Attention table is cut too: it is ranked rows.
// (#441) The Berkshire table is NOT: it lists every buy, not a ranking, and the default N of 3 hid 11 of 14.
// (#442) It lists every holding now, same rule.
// (#393) Cut on the RANK, not the row's position: a sort reorders the top N, never swaps who is in it.
// (#445) A filter query overrides the cut in every table but inflation: a row shows iff its TICKER or
// NAME holds the query, so a name the chooser hid can still be found. An empty query is the cut alone.
function apply(n) {
  const q = document.getElementById("filter").value.trim().toLowerCase();
  for (const t of document.querySelectorAll(".scroll table")) {
    if (t.closest("#inflation")) continue;
    const cut = t.closest("#stocks, #etfs, #crypto, #attention");
    const cols = [...t.rows[0].cells].flatMap((th, i) => (["TICKER", "NAME"].includes(th.textContent) ? [i] : []));
    for (const tr of t.tBodies[0].rows) {
      tr.hidden = q
        ? !cols.some((i) => tr.cells[i].textContent.toLowerCase().includes(q))
        : !!cut && !("pin" in tr.dataset) && +tr.dataset.rank >= n;
    }
  }
  stick();
}

// (#445) Column groups for the three ranked tables, finviz-style. A header NOT listed here shows in
// every view, so RANK NAME TICKER BUY% (and any column added later) can never be hidden by accident.
const VIEW = {
  MARKET: "overview", SECTOR: "overview", DOM: "overview", EARN: "overview", "PRICE(EUR)": "overview", SCORE: "overview",
  CAGR: "returns", YRS: "returns", "1D": "returns", "1W": "returns", "1M": "returns", "2Y": "returns",
  "5Y": "returns", "8Y": "returns", "20Y": "returns", "S-8Y": "returns",
  VOL: "risk", MAXDD: "risk", R2: "risk", "ABV-MA": "risk", "OFF-HI": "risk", TURNOVER: "risk",
  "5Y-WIN%": "risk", "WORST-5Y": "risk", "UW-YRS": "risk", "ND/EBITDA": "risk", "INT-COV": "risk",
  MCAP: "value", "P/E": "value", PEG: "value", "ROE/A": "value", "REV-YoY": "value", "REV-5Y": "value", "OP%": "value", "MARGIN-TREND": "value", "M-SCORE": "risk", "SBC%": "value", "EPS-YoY": "value",
  "NET%": "value", DIV: "value", BUYBK: "value", TER: "value", AUM: "value", USE: "value", REPL: "value",
  MVRV: "value", ROIC: "value", "FCF%": "value", "INS-B/S": "value",
  "TOP10%": "risk", "TD-1Y": "value", "TD-5Y": "value",
  "FCF-YLD": "value", "P/S": "value", "EV/EBITDA": "value",
};
function view(v) {
  for (const t of document.querySelectorAll(":is(#stocks, #etfs, #crypto, #attention, #berkshire, #social) table")) {
    const hide = [...t.rows[0].cells].map((th) => v !== "all" && th.textContent in VIEW && VIEW[th.textContent] !== v);
    for (const tr of t.rows) [...tr.cells].forEach((c, i) => (c.hidden = hide[i]));
  }
  stick();
}

// (#445) One query string for both choosers: each writes its own key and keeps the other's.
function remember(k, v) {
  const u = new URLSearchParams(location.search);
  u.set(k, v);
  history.replaceState(null, "", "?" + u);
}

// (#445) A table as CSV, straight from the payload rows: every row and column, cells verbatim.
const csv = (rows) =>
  [rows[0].map(([h]) => h), ...rows.map((r) => r.map(([, c]) => c))]
    .map((line) => line.map((f) => (/[",\n]/.test(String(f)) ? '"' + String(f).replaceAll('"', '""') + '"' : f)).join(","))
    .join("\n") + "\n";
console.assert(
  csv([[["A", "1"], ["B", "2"]]]) === "A,B\n1,2\n" && csv([[["N", 'x,"y"']]]) === 'N\n"x,""y"""\n',
  "csv misquotes a field",
);
// (#445) One CSV link per section, in its h2, made once; a re-render (an upload) only swaps the rows.
const sheets = {};
function sheet(id, rows) {
  sheets[id] = rows;
  const h = document.getElementById("h-" + id);
  let a = h.querySelector("a.csv");
  if (!a) {
    a = Object.assign(document.createElement("a"), { className: "csv", textContent: "CSV", download: id + ".csv", href: "#" });
    a.onclick = () => (a.href = URL.createObjectURL(new Blob([csv(sheets[id])], { type: "text/csv" })));
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
// (#461) $ marks a value read off the US line.
// null is a missing cell, which sorts last in either direction.
const NUM = /^≈?([+-]?)€?([\d,]*\.?\d+)([KMBT]?)[%~†#!xHbc*owWs$]*$/;
function key(cell) {
  if (cell === "" || cell === "n/a" || cell === "—") return null;
  const m = NUM.exec(cell);
  return m ? +(m[1] + m[2].replaceAll(",", "")) * ({ K: 1e3, M: 1e6, B: 1e9, T: 1e12 }[m[3]] || 1) : cell;
}
console.assert(
  key("€1,234.56") === 1234.56 && key("≈+9.9%") === 9.9 && key("-0.2%") === -0.2 &&
    key("€1.2B") === 1.2e9 && key("€3.7T") === 3.7e12 && key("9#!xH") === 9 && key("6#!b") === 6 && key("7#!cb") === 7 && key("3*") === 3 && key("4o") === 4 && key("5#ow") === 5 && key("3#wW") === 3 && key("3#wWs") === 3 && key("+17.7%$") === 17.7 && key("2B7A.DE") === "2B7A.DE" && key("n/a") === null,
  "sort key misreads a cell shape",
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
// (#396) The default is the shortest NON-empty lane (stocks 5, ETFs 17, crypto 3 -> 3), so every table
// fills the same N rows; an empty lane is skipped, and all-empty makes it Infinity, which clamps to 1.
function chooser(max, dflt) {
  const box = document.getElementById("topn-box");
  const sel = document.getElementById("topn");
  sel.replaceChildren(); // (#392) an upload renders again, with its own row counts
  for (let i = 1; i <= max; i++) sel.add(new Option(i));
  const want = Math.trunc(+new URLSearchParams(location.search).get("top")) || dflt;
  sel.value = Math.min(Math.max(want, 1), max);
  sel.onchange = () => {
    apply(+sel.value);
    remember("top", sel.value);
  };
  box.hidden = max <= 1; // one row everywhere: nothing to choose, so don't offer a choice
  apply(+sel.value);
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
  // (#438) Only CI's payload carries it: an upload's engine output has no key, so CI's table stays.
  if (data.attention) {
    document.getElementById("attention").replaceChildren(table(data.attention, "(attention feed unavailable)", help.attention));
  }
  if (data.berkshire) {
    document.getElementById("berkshire").replaceChildren(table(data.berkshire, "(Berkshire 13F unavailable)", help.berkshire));
  }
  // (#444) Not cut by the row chooser either: it lists every match, not a ranking.
  if (data.social) {
    document.getElementById("social").replaceChildren(table(data.social, "(no source answered)", help.social));
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
  const since = document.getElementById("since");
  since.hidden = !prev;
  since.textContent = prev ? "new = joined its table since " + stamp(prev.generated) + " (" + marked + " marked)" : "";
  // Options come from the LONGEST lane, and each table then caps itself at its own length — crypto
  // routinely has fewer rows than stocks, and offering an N no lane can fill would be a lie.
  const sizes = LANES.map((lane) => (data[lane] || []).length);
  const attention = document.querySelectorAll("#attention tbody tr").length;
  chooser(Math.max(1, attention, ...sizes), Math.min(...sizes.filter(Boolean)));
  view(viewSel.value);
  // (#445) An upload carries no shadow tables, so CI's CSV rows for those stay, like the tables do.
  for (const id of [...LANES, "core", "inflation", "attention", "berkshire", "social"]) {
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
    // (#452) the shadow tables. An upload carries none, so these groups drop from its glossary; the
    // tables CI rendered keep their hover text.
    ["Attention", "attention", [data.attention]],
    ["Berkshire holdings", "berkshire", [data.berkshire]],
    ["Social Arbitrage trading", "social", [data.social]],
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
document.getElementById("filter").oninput = () => apply(+document.getElementById("topn").value);

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
function say(text, reset) {
  mine.textContent = text;
  if (reset) {
    const a = document.createElement("a");
    a.href = "#";
    a.textContent = "back to CI's ranking";
    a.onclick = (e) => {
      e.preventDefault();
      upload.value = "";
      current = "";
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
  say("ranking with " + file.name + "…");
  const overlay = await file.text();
  const w = new Worker("engine/worker.js", { type: "module" });
  w.onmessage = ({ data }) => {
    w.terminate();
    if (data.err) return say("Could not rank with " + file.name + ": " + data.err, true);
    const out = JSON.parse(data.ok);
    current = overlay;
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
    say("The engine failed to start (" + e.message + ").", true);
  };
  w.postMessage({ overlay });
};
// (#403) "Why isn't X in?". A fresh worker per question, like an upload: the engine reads its config
// once per instance.
const whyOut = document.getElementById("why-out");
document.getElementById("why-box").onsubmit = (e) => {
  e.preventDefault();
  whyOut.textContent = "…";
  whyOut.hidden = false;
  const w = new Worker("engine/worker.js", { type: "module" });
  w.onmessage = ({ data }) => {
    w.terminate();
    whyOut.textContent = data.err ? "Could not explain: " + data.err : data.ok;
  };
  w.onerror = (e) => {
    w.terminate();
    whyOut.textContent = "The engine failed to start (" + e.message + ").";
  };
  w.postMessage({ overlay: current, why: document.getElementById("why").value });
};
fetch("engine/folioman_engine_bg.wasm", { method: "HEAD" })
  .then((r) => { for (const id of ["upload-box", "why-box"]) document.getElementById(id).hidden = !r.ok; })
  .catch(() => {});
