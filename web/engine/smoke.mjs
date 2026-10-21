// (#392) The deploy's parity pin: the engine re-ranking universe.json with no upload must print the
// rows `screen` printed into data.json. Usage: node web/engine/smoke.mjs _site
// (#421) and it must rank the pool as a set: a second instance on the quotes reversed prints the same.
import fs from "node:fs";
import path from "node:path";

const site = path.resolve(process.argv[2]);
// --target web emits an ES module under a .js name; the copy gives node an unambiguous .mjs
// the engine ranks once per instance, so the order leg gets its own module copy
const glues = ["smoke-glue.mjs", "smoke-glue-rev.mjs"].map((f) => path.join(site, "engine", f));
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
  const rev = await import(glues[1]);
  rev.initSync({ module: wasm });
  const u = JSON.parse(universe);
  u.quotes.reverse();
  const back = rev.screen("", JSON.stringify(u));
  if (back !== raw) {
    // (#576) name the keys that moved, as the parity leg does: the bare line left nothing to chase
    const r = JSON.parse(back);
    const moved = Object.keys({ ...got, ...r }).filter((k) => JSON.stringify(got[k]) !== JSON.stringify(r[k]));
    console.error("the engine's rank depends on pool order, on: " + moved.join(", "));
    for (const k of moved.filter((k) => Array.isArray(got[k]))) {
      const i = got[k].findIndex((row, j) => JSON.stringify(row) !== JSON.stringify(r[k]?.[j]));
      if (i >= 0) console.error(`  ${k}[${i}]: ${JSON.stringify(got[k][i]).slice(0, 300)}\n  reversed: ${JSON.stringify(r[k]?.[i]).slice(0, 300)}`);
    }
    process.exit(1);
  }
  console.log(`order-free over ${u.quotes.length} quotes`);
} finally {
  glues.forEach((g) => fs.rmSync(g, { force: true }));
}
