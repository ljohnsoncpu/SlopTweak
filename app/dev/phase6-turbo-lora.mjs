// Phase 6: Turbo LoRA on the SNOFS Krea 2 Raw merge, in a held session.
//
// Needs dev/phase6-session.mjs running (MODEL=snofs-krea-2) and Ready. Uses
// Invoke's recall API to put the Turbo Distill LoRA (weight 1), 8 steps and CFG 1 into
// the open page, then types a prompt, presses Invoke N times (default 3) and
// reports each image's metadata and wall-clock time. Images go to OUT.
//
// Usage (from app/):  N=3 node dev/phase6-turbo-lora.mjs

import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { OUT, remote, sleep, waitFor } from "./phase6-lib.mjs";

const N = Number(process.env.N ?? 3);
const LORA = "krea2_8step_turbo_distill_r64";
const PROMPT = "a cat reading a book in a sunny library, detailed illustration";

const r = await remote();
const count = () =>
  r.eval("fetch('/api/v1/images/?order_dir=DESC&starred_first=false&is_intermediate=false&limit=1').then(r => r.json()).then(j => j.total)");

const recalled = await r.api("/api/v1/recall/default", {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ steps: 8, cfg_scale: 1, loras: [{ model_name: LORA, weight: 1, is_enabled: true }] }),
});
console.log("recall:", JSON.stringify(recalled).slice(0, 300));
await sleep(3000);

for (let i = 1; i <= N; i++) {
  const before = (await count()) ?? 0;
  await r.eval(`(() => {
    const ta = document.querySelector('textarea');
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(ta, ${JSON.stringify(PROMPT)});
    ta.dispatchEvent(new Event('input', { bubbles: true })); })()`);
  await sleep(1500);
  const clicked = await r.eval(`(() => {
    const b = [...document.querySelectorAll('button')].find(b => b.getAttribute('aria-label') === 'Invoke' || /^Invoke$/.test(b.textContent.trim()));
    if (!b || b.disabled) return 'cannot click Invoke';
    b.click(); return 'ok'; })()`);
  const t0 = Date.now();
  await waitFor(async () => ((await count()) > before ? true : false), 15 * 60000, "image", 500).catch(() => null);
  const secs = ((Date.now() - t0) / 1000).toFixed(1);
  const meta = await r.eval(`(async () => {
    const j = await (await fetch('/api/v1/images/?order_dir=DESC&starred_first=false&is_intermediate=false&limit=1')).json();
    const name = j.items[0].image_name;
    const m = await (await fetch('/api/v1/images/i/' + name + '/metadata')).json();
    const b = await (await fetch('/api/v1/images/i/' + name + '/full')).arrayBuffer();
    let bin = ''; new Uint8Array(b).forEach(x => bin += String.fromCharCode(x));
    return { name, steps: m?.steps, cfg: m?.cfg_scale, w: m?.width, h: m?.height, model: m?.model?.name,
      loras: (m?.loras || []).map(l => ({ name: l.model?.name, weight: l.weight })), png: btoa(bin) };
  })()`);
  if (meta?.png) writeFileSync(join(OUT, `turbo-${i}.png`), Buffer.from(meta.png, "base64"));
  const { png, ...shown } = meta ?? {};
  console.log(`gen ${i}: ${clicked}, ${secs}s ${JSON.stringify(shown)}`);
}
await r.shot("turbo-final");
r.close();
