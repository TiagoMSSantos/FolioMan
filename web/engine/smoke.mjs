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
  // (#440) `berkshire` likewise, from SEC's 13F filings. (#443) The Camillo tables, from YouTube and a hand list.
  const drift = Object.keys(want).filter((k) => !["generated", "attention", "berkshire", "camillo_videos", "camillo_hand"].includes(k) && JSON.stringify(got[k]) !== JSON.stringify(want[k]));
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
  if (rev.screen("", JSON.stringify(u)) !== raw) {
    console.error("the engine's rank depends on pool order");
    process.exit(1);
  }
  console.log(`order-free over ${u.quotes.length} quotes`);
} finally {
  glues.forEach((g) => fs.rmSync(g, { force: true }));
}
