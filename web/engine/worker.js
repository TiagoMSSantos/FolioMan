// (#392) One upload, one engine. Its config reads are process-once, so the page starts a fresh worker
// per upload and terminates it after the answer.
import init, { screen } from "./folioman_engine.js";

onmessage = async ({ data: overlay }) => {
  try {
    const r = await fetch("../universe.json", { cache: "no-cache" });
    if (!r.ok) throw new Error("universe.json: HTTP " + r.status);
    const universe = await r.text();
    await init();
    postMessage({ ok: screen(overlay, universe) });
  } catch (e) {
    postMessage({ err: e?.message ?? String(e) });
  }
};
