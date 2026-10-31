// Qwen-Image 2.1 edit spike (docs/spikes/qwen-image-2-1.md, Phase B). SPENDS MONEY:
// run only with approval (GPU 24 GB, about $0.20-0.30/hr, total cap about $0.50).
// Drives the debug app with a DRAFT catalog entry (added at runtime; the repo's
// catalog.json is not changed), then runs graphs in the app's own ComfyUI window.
// Provisioning is the existing ComfyUI path (the Identity Edit nodes get installed
// and probed but are unused); models are placed by `dest`.
//
// Usage (from app/):  REF1=a.png REF2=b.png OUT=<scratch dir> node dev/qwen-spike.mjs
//   MAX_DPH=0.35 MAX_SESSION_MINUTES=60 READY_TIMEOUT=20 HARD_CAP_MINUTES=70
//   ONLY=A,B   run only these labelled runs (default all)
// Safety: the instance this creates is destroyed on every exit path (Stop, then the
// hard cap and the safety net in `finally`). Ids are printed as seen. Keys come from
// HKCU\Environment and are never printed. OUT must be outside the repo.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdirSync, readFileSync, rmSync, writeFileSync, existsSync, appendFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT;
const REF1 = process.env.REF1;
const REF2 = process.env.REF2;
if (!OUT || !REF1 || !REF2) throw new Error("set OUT, REF1 and REF2");
if (resolve(OUT).startsWith(resolve(APP, ".."))) throw new Error("OUT must be outside the repo");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const CATALOG_PORT = 18558;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const MAX_DPH = Number(process.env.MAX_DPH ?? 0.35);
const MAX_SESSION_MINUTES = Number(process.env.MAX_SESSION_MINUTES ?? 60);
const READY_TIMEOUT = Number(process.env.READY_TIMEOUT ?? 20);
const HARD_CAP_MINUTES = Number(process.env.HARD_CAP_MINUTES ?? 70);
const ONLY = (process.env.ONLY ?? "").split(",").filter(Boolean);
const MODEL_ID = "qwen-image-edit-draft";

mkdirSync(OUT, { recursive: true });
const LOG = join(OUT, "spike.log");
const t0 = Date.now();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function say(msg) {
  const line = `+${Math.round((Date.now() - t0) / 1000)}s ${msg}`;
  console.log(line);
  appendFileSync(LOG, line + "\n");
}
const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok });
  say(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
}

const HF = "https://huggingface.co/Comfy-Org/Qwen-Image-2.1/resolve/aacf31a6d48e707f352ecba714cee0680da3479b";
const draftEntry = {
  id: MODEL_ID,
  name: "Qwen-Image 2.1 Edit (draft)",
  description: "Spike only.",
  good_for: "Spike",
  price_tier: 1,
  backend: "comfyui",
  base: "qwen-image",
  source: { huggingface_repo: "Comfy-Org/Qwen-Image-2.1", revision: "aacf31a6d48e707f352ecba714cee0680da3479b", page: "https://huggingface.co/Comfy-Org/Qwen-Image-2.1" },
  files: [
    { url: `${HF}/diffusion_models/qwen_image_2.1_int8_convrot.safetensors`, filename: "qwen_image_2.1_int8_convrot.safetensors", sha256: "CB74113CB03FAECD79611B01FD7FD642F0AA60D6F0B95086ABEE214D75EAA57D", size_bytes: 7256783064, kind: "main", requires_civitai_token: false, dest: "diffusion_models" },
    { url: `${HF}/text_encoders/qwen3vl_8b_int8_convrot.safetensors`, filename: "qwen3vl_8b_int8_convrot.safetensors", sha256: "8BFD0F6E12ABF2D2D697ECC888E5E90B0D6741D6708F05799F53AFA560452E8F", size_bytes: 9350798360, kind: "text_encoder", requires_civitai_token: false, dest: "text_encoders" },
    { url: `${HF}/vae/qwen_image_2.1_vae_bf16.safetensors`, filename: "qwen_image_2.1_vae_bf16.safetensors", sha256: "BB21F7473051E1AC368515DD3F2E15CD44D7A11748EE8823E1DDCA3E4876B7C9", size_bytes: 675509688, kind: "vae", requires_civitai_token: false, dest: "vae" },
    { url: `${HF}/loras/qwen_image_2.1_turbo_lora_avg_rank_178_bf16.safetensors`, filename: "qwen_image_2.1_turbo_lora_avg_rank_178_bf16.safetensors", sha256: "208DD43250E1E01467BA190572AE2E7107A7870EC1FF026BC65F791FA1E80E95", size_bytes: 913314512, kind: "lora", requires_civitai_token: false, dest: "loras" },
  ],
  min_vram_gb: 24,
  min_compute_cap: 800,
  min_ram_gb: 32,
  nsfw: false,
  license_note: "Spike only. HF license tag qwen-research, text not yet reviewed.",
};

// ----- Vast (verification and the safety net only) -----------------------------

function userEnv(name) {
  if (process.env[name]) return process.env[name].trim();
  const out = execSync(`reg query HKCU\\Environment /v ${name}`, { encoding: "utf8" });
  const m = out.match(new RegExp(`${name}\\s+REG_\\w+\\s+(\\S+)`));
  if (!m) throw new Error(`${name} not set`);
  return m[1].trim();
}
const VAST_KEY = userEnv("VAST_API_KEY");
async function vast(method, path) {
  const r = await fetch(VAST + path, { method, headers: { Authorization: `Bearer ${VAST_KEY}` } });
  const text = await r.text();
  return { status: r.status, body: text ? JSON.parse(text) : null };
}
async function ourInstances() {
  const { body } = await vast("GET", "/instances/");
  return (body?.instances ?? []).filter((i) => (i.label ?? "").startsWith("sloptweak"));
}
const credit = async () => (await vast("GET", "/users/current")).body?.credit;
async function instanceGone(id) {
  const { status, body } = await vast("GET", `/instances/${id}/`);
  return status === 404 || !body?.instances;
}

// ----- app + CDP --------------------------------------------------------------

let app = null;
let vite = null;
function launch() {
  const logFile = join(OUT, "app.log");
  const p = spawn(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_DEV_IMPORT_KEYS: "1",
      // main's catalog may not list Identity Edit yet: serve this checkout's.
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  p.stdout.on("data", (d) => appendFileSync(logFile, d));
  p.stderr.on("data", (d) => appendFileSync(logFile, d));
  app = p;
  say(`app launched, log ${logFile}`);
}
function killApp() {
  if (!app) return;
  try {
    execSync(`taskkill /T /F /PID ${app.pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
  app = null;
}
async function waitFor(fn, ms, what) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(1000);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last}` : ""}`);
}
async function cdp(port, match, ms = 60000) {
  const t = await waitFor(async () => {
    const r = await fetch(`http://127.0.0.1:${port}/json/list`);
    return (await r.json()).find((t) => t.type === "page" && match(t.url));
  }, ms, `CDP target on ${port}`);
  const ws = new WebSocket(t.webSocketDebuggerUrl);
  await new Promise((res, rej) => ((ws.onopen = res), (ws.onerror = rej)));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    pending.get(msg.id)?.(msg);
    pending.delete(msg.id);
  };
  const send = (method, params = {}) =>
    new Promise((res) => {
      pending.set(++id, res);
      ws.send(JSON.stringify({ id, method, params }));
    });
  return {
    ws,
    eval: async (expression) => {
      const r = (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result;
      if (r?.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? "page exception");
      return r?.result?.value;
    },
    shot: async (name) => {
      const r = await send("Page.captureScreenshot", { format: "png" });
      writeFileSync(join(OUT, `${name}.png`), Buffer.from(r.result.data, "base64"));
      say(`screenshot ${name}.png`);
    },
  };
}
const snapshot = (m) => m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot').then(s => s.state)");
async function follow(m, pred, ms, what) {
  let last = "";
  const seen = new Set();
  return waitFor(async () => {
    const s = await snapshot(m);
    if (!s) return false;
    const line =
      `${s.kind}` +
      (s.attempt ? ` try ${s.attempt}/${s.max_attempts}` : "") +
      (s.instance_id ? ` instance ${s.instance_id}` : "") +
      (s.offer ? ` offer ${s.offer.offer_id} ${s.offer.gpu_name} $${s.offer.hourly.toFixed(4)}/hr` : "") +
      (s.stage ? ` stage ${s.stage}` : "") +
      (s.progress != null ? ` ${Math.floor(s.progress)}%` : "") +
      (s.reason ? ` reason: ${s.reason}` : "");
    if (s.instance_id && !seen.has(s.instance_id)) {
      seen.add(s.instance_id);
      say(`INSTANCE ${s.instance_id} created`);
    }
    if (line !== last) {
      say(`state: ${line}`);
      last = line;
    }
    if (s.kind === "failed") throw new Error(`session failed: ${s.reason}`);
    return pred(s) ? s : false;
  }, ms, what);
}

// ----- the spike --------------------------------------------------------------

// Runs inside the ComfyUI page: same-origin fetches through the tunnel.
const PAGE_HELPERS = `(() => {
  const cid = 'spike-' + Math.random().toString(16).slice(2);
  const b64 = (buf) => { let s = ''; const a = new Uint8Array(buf); for (let i = 0; i < a.length; i += 0x8000) s += String.fromCharCode(...a.subarray(i, i + 0x8000)); return btoa(s); };
  window.__spike = {
    async upload(name, base64) {
      const bin = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
      const fd = new FormData();
      fd.append('image', new Blob([bin], { type: 'image/png' }), name);
      fd.append('type', 'input'); fd.append('overwrite', 'true');
      const r = await fetch('/upload/image', { method: 'POST', body: fd });
      return r.status + ' ' + (await r.text());
    },
    async mask(name, w, h, polygon) {
      const c = document.createElement('canvas'); c.width = w; c.height = h;
      const g = c.getContext('2d'); g.fillStyle = '#000'; g.fillRect(0, 0, w, h);
      g.fillStyle = '#fff'; g.beginPath(); polygon.forEach(([x, y], i) => (i ? g.lineTo(x, y) : g.moveTo(x, y))); g.closePath(); g.fill();
      const blob = await new Promise((res) => c.toBlob(res, 'image/png'));
      const fd = new FormData(); fd.append('image', blob, name); fd.append('type', 'input'); fd.append('overwrite', 'true');
      const r = await fetch('/upload/image', { method: 'POST', body: fd });
      return r.status + ' ' + (await r.text());
    },
    async run(graph, timeoutMs) {
      const t = Date.now();
      const r = await fetch('/prompt', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ prompt: graph, client_id: cid }) });
      const body = await r.json();
      if (!r.ok) return { error: 'prompt ' + r.status + ' ' + JSON.stringify(body).slice(0, 600) };
      const id = body.prompt_id;
      while (Date.now() - t < timeoutMs) {
        await new Promise((res) => setTimeout(res, 2000));
        const h = await (await fetch('/history/' + id)).json();
        const e = h[id];
        if (!e) continue;
        if (e.status?.status_str === 'error') return { error: JSON.stringify(e.status.messages).slice(0, 800) };
        const img = Object.values(e.outputs || {}).flatMap((o) => o.images || []).find((i) => i.type === 'output');
        if (img) {
          const v = await fetch('/view?' + new URLSearchParams({ filename: img.filename, subfolder: img.subfolder, type: 'output' }));
          return { seconds: Math.round((Date.now() - t) / 1000), png: b64(await v.arrayBuffer()) };
        }
      }
      return { error: 'timeout' };
    },
  };
  return 'ok';
})()`;

const MODEL = {
  unet: "qwen_image_2.1_int8_convrot.safetensors",
  clip: "qwen3vl_8b_int8_convrot.safetensors",
  vae: "qwen_image_2.1_vae_bf16.safetensors",
  lora: "qwen_image_2.1_turbo_lora_avg_rank_178_bf16.safetensors",
};
// Filled from ComfyUI's own template on the instance (see spike()).
const cfg = { clipType: "qwen_image", weightDtype: "default", keyStyle: "images." };

/** Autogrow reference inputs: API-format key style differs by ComfyUI build; spike() picks one. */
function graph({ refs, prompt, seed, resolution = 1024, steps = 25, turbo = false, prefix }) {
  const g = {
    1: { class_type: "UNETLoader", inputs: { unet_name: MODEL.unet, weight_dtype: cfg.weightDtype } },
    2: { class_type: "CLIPLoader", inputs: { clip_name: MODEL.clip, type: cfg.clipType, device: "default" } },
    3: { class_type: "VAELoader", inputs: { vae_name: MODEL.vae } },
  };
  let model = ["1", 0];
  if (turbo) {
    g[11] = { class_type: "LoraLoaderModelOnly", inputs: { model, lora_name: MODEL.lora, strength_model: 1.0 } };
    model = ["11", 0];
  }
  const enc = { clip: ["2", 0], prompt, negative_prompt: "", vae: ["3", 0], resolution };
  refs.forEach((name, i) => {
    g[`2${i}`] = { class_type: "LoadImage", inputs: { image: name } };
    enc[`${cfg.keyStyle}image_${i + 1}`] = [`2${i}`, 0];
  });
  g[4] = { class_type: "TextEncodeQwenImage21", inputs: enc };
  g[7] = {
    class_type: "KSampler",
    inputs: { seed, steps, cfg: 1.0, sampler_name: "euler", scheduler: "simple", denoise: 1.0, model, positive: ["4", 0], negative: ["4", 1], latent_image: ["4", 2] },
  };
  g[8] = { class_type: "VAEDecode", inputs: { samples: ["7", 0], vae: ["3", 0] } };
  g[9] = { class_type: "SaveImage", inputs: { filename_prefix: prefix, images: ["8", 0] } };
  return g;
}

const P_SINGLE = "<image1> stands in a sunlit forest clearing, full body, painterly illustration. Keep the character's face, fur colors and outfit exactly as in the reference.";
const P_PAIR = "<image1> and <image2> sit together at a cafe table beside a rain-streaked window, warm lighting, illustration. Keep both characters' faces, fur colors and outfits exactly as in their references.";
const P_EDIT = "Change <image1>'s armor into a plain red hoodie. Keep the character, pose, background and everything else exactly the same.";
const RUNS = [
  { label: "A-single-ref", note: "1 ref, 25 steps", opts: { refs: ["ref1.png"], prompt: P_SINGLE } },
  { label: "B-two-refs", note: "2 refs, 25 steps", opts: { refs: ["ref1.png", "ref2.png"], prompt: P_PAIR } },
  { label: "C-two-refs-seed2", note: "2 refs, other seed", opts: { refs: ["ref1.png", "ref2.png"], prompt: P_PAIR, seedOffset: 7 } },
  { label: "D-edit-outfit", note: "instruction edit of ref1", opts: { refs: ["ref1.png"], prompt: P_EDIT } },
  { label: "E-turbo-lora-8", note: "2 refs, Turbo LoRA, 8 steps", opts: { refs: ["ref1.png", "ref2.png"], prompt: P_PAIR, turbo: true, steps: 8 } },
  { label: "F-res0", note: "2 refs, resolution 0 (own size)", opts: { refs: ["ref1.png", "ref2.png"], prompt: P_PAIR, resolution: 0 } },
];

/** Widget values of every node of a type anywhere in a ComfyUI workflow JSON (incl. subgraphs). */
function widgetsOf(obj, type, out = []) {
  if (Array.isArray(obj)) obj.forEach((o) => widgetsOf(o, type, out));
  else if (obj && typeof obj === "object") {
    if (obj.type === type && Array.isArray(obj.widgets_values)) out.push(obj.widgets_values);
    Object.values(obj).forEach((o) => widgetsOf(o, type, out));
  }
  return out;
}

async function spike() {
  say("=== Qwen-Image 2.1 edit spike ===");
  launch();
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 120000, "UI");
  const picked = await m.eval(`(() => { const s = document.getElementById('model');
    if (![...s.options].some(o => o.value === '${MODEL_ID}')) return false;
    s.value = '${MODEL_ID}'; s.dispatchEvent(new Event('change')); return true; })()`);
  if (!picked) throw new Error(`${MODEL_ID} is not in the catalog`);
  await sleep(3000);
  const startState = await m.eval(`JSON.stringify({ disabled: document.getElementById('start').disabled, text: document.body.innerText.slice(0, 1800) })`);
  say(`start button before click: ${startState}`);
  await m.shot("before-start");
  if (process.env.PROBE === "1") {
    say("PROBE=1: not clicking Start");
    m.ws.close();
    return;
  }
  if (JSON.parse(startState).disabled) throw new Error("Start is disabled; not spending anything");
  await m.eval("document.getElementById('start').click()");
  const tStart = Date.now();
  // A click that leaves the app idle means the start was refused: stop, don't wait for the cap.
  await sleep(45000);
  const afterClick = await snapshot(m);
  say(`state 45 s after Start: ${afterClick?.kind}`);
  if (afterClick?.kind === "idle") {
    await m.shot("start-did-nothing");
    throw new Error("Start left the app idle (refused?)");
  }
  const s = await follow(m, (x) => x.kind === "ready", (READY_TIMEOUT * 3 + 10) * 60000, "ready");
  const id = s.instance_id;
  say(`READY instance ${id} (${s.offer.gpu_name}, $${s.offer.hourly.toFixed(4)}/hr) after ${Math.round((Date.now() - tStart) / 60000)} min`);
  const inst = (await vast("GET", `/instances/${id}/`)).body?.instances ?? {};
  say(`host: machine ${inst.machine_id}, ${inst.geolocation}, ${inst.gpu_name}, ${inst.gpu_ram} MB, ram ${inst.cpu_ram} MB`);

  check("Open ComfyUI is offered", await m.eval("!document.getElementById('open-comfy').hidden"));
  await m.eval("document.getElementById('open-comfy').click()");
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 90000);
  const loaded = await waitFor(() => r.eval("!!document.querySelector('#vue-app, .comfyui-body-top, canvas')"), 120000, "ComfyUI UI").catch(() => false);
  await sleep(5000);
  check("ComfyUI's own UI loads in the window", !!loaded, await r.eval("document.title"));
  await r.eval(PAGE_HELPERS);

  // Learn the real settings from the node definitions and ComfyUI's own template.
  const info = await r.eval(`Promise.all(['TextEncodeQwenImage21','CLIPLoader','UNETLoader','QwenImage21Cache'].map(n => fetch('/object_info/' + n).then(r => r.json()).catch(e => String(e))))`);
  writeFileSync(join(OUT, "object_info.json"), JSON.stringify(info, null, 2));
  const clipTypes = info[1]?.CLIPLoader?.input?.required?.type?.[0] ?? [];
  say(`CLIPLoader types containing "qwen": ${JSON.stringify(clipTypes.filter((t) => /qwen/i.test(t)))}`);
  check("TextEncodeQwenImage21 node exists", !!info[0]?.TextEncodeQwenImage21);
  const enc = info[0]?.TextEncodeQwenImage21?.input ?? {};
  const inputsKeys = Object.keys({ ...(enc.required ?? {}), ...(enc.optional ?? {}) });
  say(`TextEncodeQwenImage21 declared inputs: ${JSON.stringify(inputsKeys)}`);
  const tpl = await r.eval(`(async () => {
    try {
      const idx = await (await fetch('/api/workflow_templates')).json();
      const mod = Object.keys(idx).find((k) => (idx[k] || []).includes('image_qwen_image_2_1_image_edit'));
      if (!mod) return { error: 'template not listed: ' + Object.keys(idx).join(',') };
      return { mod, wf: await (await fetch('/api/workflow_templates/' + mod + '/image_qwen_image_2_1_image_edit.json')).json() };
    } catch (e) { return { error: String(e) }; }
  })()`);
  if (tpl?.wf) {
    writeFileSync(join(OUT, "template-image-edit.json"), JSON.stringify(tpl.wf));
    const clip = widgetsOf(tpl.wf, "CLIPLoader")[0];
    const unet = widgetsOf(tpl.wf, "UNETLoader")[0];
    say(`template CLIPLoader widgets: ${JSON.stringify(clip)}; UNETLoader widgets: ${JSON.stringify(unet)}`);
    if (clip?.[1]) cfg.clipType = clip[1];
    if (unet?.[1]) cfg.weightDtype = unet[1];
  } else {
    say(`template not read (${tpl?.error}); using defaults ${JSON.stringify(cfg)}`);
  }
  // The declared input name decides the API key style.
  if (inputsKeys.some((k) => k === "image_1")) cfg.keyStyle = "";
  say(`using ${JSON.stringify(cfg)}`);

  const b64 = (f) => readFileSync(f).toString("base64");
  say(`upload ref1: ${await r.eval(`window.__spike.upload('ref1.png', ${JSON.stringify(b64(REF1))})`)}`);
  say(`upload ref2: ${await r.eval(`window.__spike.upload('ref2.png', ${JSON.stringify(b64(REF2))})`)}`);

  const summary = [];
  const seed = 424242;
  for (const run of RUNS) {
    if (ONLY.length && !ONLY.includes(run.label[0])) continue;
    const { seedOffset = 0, ...opts } = run.opts;
    let g = graph({ ...opts, seed: seed + seedOffset, prefix: `qwen_${run.label}` });
    say(`run ${run.label}: ${run.note}`);
    let res = await r.eval(`window.__spike.run(${JSON.stringify(g)}, 600000)`);
    if (res?.error && /image_1|images/i.test(res.error) && summary.length === 0) {
      // Wrong autogrow key style: flip once and retry.
      cfg.keyStyle = cfg.keyStyle ? "" : "images.";
      say(`  -> input error, retrying with key style ${JSON.stringify(cfg.keyStyle)}: ${res.error.slice(0, 300)}`);
      g = graph({ ...opts, seed: seed + seedOffset, prefix: `qwen_${run.label}` });
      res = await r.eval(`window.__spike.run(${JSON.stringify(g)}, 600000)`);
    }
    if (res?.png) {
      writeFileSync(join(OUT, `${run.label}.png`), Buffer.from(res.png, "base64"));
      say(`  -> ${run.label}.png in ${res.seconds}s`);
      summary.push({ label: run.label, note: run.note, seconds: res.seconds });
    } else {
      say(`  -> FAILED: ${res?.error}`);
      summary.push({ label: run.label, note: run.note, error: res?.error });
      // Two failures in a row means the graph is wrong: stop paying for it.
      if (summary.length >= 2 && summary.slice(-2).every((x) => x.error)) break;
    }
    const st = await r.eval("fetch('/system_stats').then(r => r.json())").catch(() => null);
    const d = st?.devices?.[0];
    if (d) say(`  vram after ${run.label}: torch_total ${d.torch_vram_total}, torch_free ${d.torch_vram_free}, vram_free ${d.vram_free}`);
  }
  const stats = await r.eval("fetch('/system_stats').then(r => r.json())");
  writeFileSync(join(OUT, "summary.json"), JSON.stringify({ summary, system: stats, cfg }, null, 2));
  check("at least one spike run produced an image", summary.some((x) => x.seconds));
  r.ws.close();

  await m.eval("document.getElementById('stop').click()");
  await follow(m, (x) => x.kind === "idle", 5 * 60000, "idle");
  check("Stop destroys the instance", await instanceGone(id), `instance ${id}`);
  m.ws.close();
}

// ----- main -------------------------------------------------------------------

const before = new Set((await ourInstances()).map((i) => i.id));
if (before.size) {
  say(`refusing to run: sloptweak instances already exist: ${[...before].join(", ")}`);
  process.exit(2);
}
if (existsSync(join(DATA_DIR, "active_instance.json"))) {
  say("refusing to run: an active_instance.json record exists");
  process.exit(2);
}
const creditBefore = await credit();
say(`credit before: $${creditBefore?.toFixed(4)}`);

const catalogServer = createServer((_req, res) => {
  res.setHeader("content-type", "application/json");
  const cat = JSON.parse(readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8"));
  cat.models.push(draftEntry);
  res.end(JSON.stringify(cat));
}).listen(CATALOG_PORT, "127.0.0.1");

vite = spawn("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true, windowsHide: true, stdio: "ignore" });
await waitFor(() => servesThisCheckout(APP), 120000, "vite");

mkdirSync(CONFIG_DIR, { recursive: true });
const settingsFile = join(CONFIG_DIR, "settings.json");
const backupFile = settingsFile + ".acceptance-backup";
if (!existsSync(backupFile)) writeFileSync(backupFile, existsSync(settingsFile) ? readFileSync(settingsFile, "utf8") : "");
// Not the user's Pictures: results go to OUT.
writeFileSync(
  settingsFile,
  JSON.stringify({ heartbeat_minutes: 3, max_session_minutes: MAX_SESSION_MINUTES, max_dph: MAX_DPH, ready_timeout_minutes: READY_TIMEOUT, output_dir: join(OUT, "app-out") }, null, 2),
);
const restoreSettings = () => {
  const backup = readFileSync(backupFile, "utf8");
  if (backup) writeFileSync(settingsFile, backup);
  else rmSync(settingsFile, { force: true });
  rmSync(backupFile, { force: true });
};

const hardCap = setTimeout(async () => {
  say(`HARD CAP (${HARD_CAP_MINUTES} min): destroying what this run created and stopping`);
  try {
    for (const i of await ourInstances()) {
      if (!before.has(i.id)) {
        say(`HARD CAP: destroying instance ${i.id}`);
        await vast("DELETE", `/instances/${i.id}/`);
      }
    }
  } finally {
    restoreSettings();
    process.exit(3);
  }
}, HARD_CAP_MINUTES * 60000);

try {
  await spike();
} catch (e) {
  check("spike completed", false, String(e));
} finally {
  clearTimeout(hardCap);
  catalogServer.close();
  killApp();
  try {
    execSync(`taskkill /T /F /PID ${vite.pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
  for (const i of await ourInstances()) {
    if (!before.has(i.id)) {
      say(`SAFETY NET: destroying leftover instance ${i.id}`);
      await vast("DELETE", `/instances/${i.id}/`);
    }
  }
  restoreSettings();
  await sleep(10000);
  const left = (await ourInstances()).filter((i) => !before.has(i.id));
  say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
  const creditAfter = await credit();
  say(`credit after: $${creditAfter?.toFixed(4)} (spent ~$${(creditBefore - creditAfter).toFixed(4)}; late charges may post)`);
  const failed = results.filter((x) => !x.ok).length;
  say(`${results.length - failed}/${results.length} checks passed`);
}
