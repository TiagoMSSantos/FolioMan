// (#392) One upload, one engine. Its config reads are process-once, so the page starts a fresh worker
// per upload and terminates it after the answer.
// (#403) `why` set = a "why isn't X in?" question, answered on the same ranking.
import init, { explain, screen } from "./folioman_engine.js";

onmessage = async ({ data: { overlay, why } }) => {
  try {
    const r = await fetch("../universe.json", { cache: "no-cache" });
    if (!r.ok) throw new Error("universe.json: HTTP " + r.status);
    const universe = await r.text();
    await init();
    postMessage({ ok: why == null ? screen(overlay, universe) : explain(overlay, universe, why) });
  } catch (e) {
    postMessage({ err: e?.message ?? String(e) });
  }
};
