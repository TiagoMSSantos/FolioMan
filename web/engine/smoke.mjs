// (#392) The deploy's parity pin: the engine re-ranking universe.json with no upload must print the
// rows `screen` printed into data.json. Usage: node web/engine/smoke.mjs _site
// (#421) and it must rank the pool as a set: a second instance on the quotes reversed prints the same.
// (#839) so do four more on seeded shuffles.
import fs from "node:fs";
import path from "node:path";

// (#882) web/app.js runs in no CI step, so its sort key is pinned here on the cell shapes that once sorted as
// text: sliced out of the file and called.
{
  const src = fs.readFileSync(new URL("../app.js", import.meta.url), "utf8");
  const a = src.indexOf("const NUM ="), b = src.indexOf("console.assert", a);
  const key = new Function(src.slice(a, b) + "; return key;")();
  const want = { "$2.9B": 2.9e9, "-$367.1M": -367.1e6, "$100.0B r": 100e9, "2027 r": "2027 r", "+25%ᶠ": 25, "-41%ⁿ⁰": -41, "€36.5Bᶠ": 36.5e9, "10: a, b": 10, "9: c": 9, "0": 0 };
  const bad = Object.entries(want).filter(([c, k]) => key(c) !== k).map(([c]) => c);
  if (bad.length) {
    console.error("web/app.js sort key misreads: " + bad.join(" | "));
    process.exit(1);
  }
}

const site = path.resolve(process.argv[2]);
// --target web emits an ES module under a .js name; the copy gives node an unambiguous .mjs
// the engine ranks once per instance, so each order leg gets its own module copy
const glues = ["", "-rev", "-s1", "-s2", "-s3", "-s4"].map((f) => path.join(site, "engine", `smoke-glue${f}.mjs`));
glues.forEach((g) => fs.copyFileSync(path.join(site, "engine", "folioman_engine.js"), g));
try {
  const wasm = fs.readFileSync(path.join(site, "engine", "folioman_engine_bg.wasm"));
  const { initSync, screen } = await import(glues[0]);
  initSync({ module: wasm });
  const want = JSON.parse(fs.readFileSync(path.join(site, "data.json"), "utf8"));
  const universe = fs.readFileSync(path.join(site, "universe.json"), "utf8");
  const t0 = Date.now();
  const raw = screen("", universe);
  const got = JSON.parse(raw);
  // `generated` aside: the universe is stamped when it is written, the payload a moment later.
  // (#438) `attention` too: screen fetches it from Wikimedia, the engine has no feed, and an upload keeps CI's.
  // (#440) `berkshire` likewise, from SEC's 13F filings. (#444) `social`, from Dataroma, YouTube and a hand list.
  // (#456) `exposure`, from Yahoo's fund holdings, which the engine does not carry.
  // (#486) `bonds`, from FRED. (#500) `young`, built by screen alone like the three shadow tables.
  // (#515) `upcoming`, Nasdaq's calendar. (#525) `reits`, the side-priced SIGIs.
  // (#530) The three CAGR floors are graded: the engine echoes the upload's, as screen writes CI's.
  // (#538) `social_cagr_floor` too: the Social table's, off `social_heuristic` over `buy_heuristic`.
  // (#746) `braked` is graded, glossary included: the engine builds it off its own Stocks rows, as screen does.
  // (#811) `intraday`, from Yahoo's daily bars, ranked by screen alone.
  const ci = ["attention", "berkshire", "social", "exposure", "bonds", "young", "upcoming", "reits", "intraday"];
  // (#458) and their glossaries: screen stamps the stock columns' help onto those tables, the engine has no rows to stamp
  for (const k of ci) [want, got].forEach((p) => delete p.help?.[k]);
  const drift = Object.keys(want).filter((k) => !["generated", ...ci].includes(k) && JSON.stringify(got[k]) !== JSON.stringify(want[k]));
  if (drift.length) {
    console.error("the engine drifts from screen on: " + drift.join(", "));
    process.exit(1);
  }
  const n = (k) => (want[k] || []).length;
  console.log(`engine parity in ${Date.now() - t0}ms: stocks ${n("stocks")} etfs ${n("etfs")} crypto ${n("crypto")} core ${n("core")}`);
  // (#839) a reversal keeps every adjacent pair adjacent, so seeded shuffles break the ties it cannot
  let seed = 0x9e3779b9;
  const rnd = () => (seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) / 2 ** 32;
  const shuffle = (a) => {
    for (let i = a.length - 1; i > 0; i--) {
      const j = Math.floor(rnd() * (i + 1));
      [a[i], a[j]] = [a[j], a[i]];
    }
  };
  const arms = [["reversed", (a) => a.reverse()], ...[1, 2, 3, 4].map((n) => [`shuffle ${n}`, shuffle])];
  let quotes = 0;
  for (const [i, [label, order]] of arms.entries()) {
    const leg = await import(glues[i + 1]);
    leg.initSync({ module: wasm });
    const u = JSON.parse(universe);
    order(u.quotes);
    quotes = u.quotes.length;
    const back = leg.screen("", JSON.stringify(u));
    if (back === raw) continue;
    // (#576) name the keys that moved, as the parity leg does: the bare line left nothing to chase
    const r = JSON.parse(back);
    const moved = Object.keys({ ...got, ...r }).filter((k) => JSON.stringify(got[k]) !== JSON.stringify(r[k]));
    console.error(`the engine's rank depends on pool order (${label}), on: ` + moved.join(", "));
    for (const k of moved.filter((k) => Array.isArray(got[k]))) {
      const j = got[k].findIndex((row, x) => JSON.stringify(row) !== JSON.stringify(r[k]?.[x]));
      if (j >= 0) console.error(`  ${k}[${j}]: ${JSON.stringify(got[k][j]).slice(0, 300)}\n  ${label}: ${JSON.stringify(r[k]?.[j]).slice(0, 300)}`);
    }
    process.exit(1);
  }
  console.log(`order-free over ${quotes} quotes, ${arms.length} orders`);
} finally {
  glues.forEach((g) => fs.rmSync(g, { force: true }));
}
