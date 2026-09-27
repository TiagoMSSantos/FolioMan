// (#392) The deploy's parity pin: the engine re-ranking universe.json with no upload must print the
// rows `screen` printed into data.json. Usage: node web/engine/smoke.mjs _site
import fs from "node:fs";
import path from "node:path";

const site = path.resolve(process.argv[2]);
// --target web emits an ES module under a .js name; the copy gives node an unambiguous .mjs
const glue = path.join(site, "engine", "smoke-glue.mjs");
fs.copyFileSync(path.join(site, "engine", "folioman_engine.js"), glue);
try {
  const { initSync, screen } = await import(glue);
  initSync({ module: fs.readFileSync(path.join(site, "engine", "folioman_engine_bg.wasm")) });
  const want = JSON.parse(fs.readFileSync(path.join(site, "data.json"), "utf8"));
  const t0 = Date.now();
  const got = JSON.parse(screen("", fs.readFileSync(path.join(site, "universe.json"), "utf8")));
  // `generated` aside: the universe is stamped when it is written, the payload a moment later
  const drift = Object.keys(want).filter((k) => k !== "generated" && JSON.stringify(got[k]) !== JSON.stringify(want[k]));
  if (drift.length) {
    console.error("the engine drifts from screen on: " + drift.join(", "));
    process.exit(1);
  }
  const n = (k) => (want[k] || []).length;
  console.log(`engine parity in ${Date.now() - t0}ms: stocks ${n("stocks")} etfs ${n("etfs")} crypto ${n("crypto")} core ${n("core")}`);
} finally {
  fs.rmSync(glue);
}
