// Feature 2 spike: can Identity Edit edit a BASE image? SPENDS MONEY: run only
// with approval. Also the live acceptance for "Open ComfyUI": the spike runs
// inside the app's own ComfyUI window (same-origin fetches through the tunnel),
// so the sidecar's Origin handling, the cookie session and the preloaded
// workflow are exercised for real.
//
// Routes (see docs/spikes/comfy-raw-and-base-edit.md):
//   1  base as reference slot 1 ("scene"), sheet as slot 2 ("subject"), t2i
//   2  true img2img: VAEEncode(base) into KSampler with denoise < 1
//   3  masked edit: SetLatentNoiseMask on the encoded base
//
// Usage (from app/):  BASE=base.png SHEET=sheet.png OUT=<scratch dir> node dev/edit-spike.mjs
//   MAX_DPH=0.75 MAX_SESSION_MINUTES=60 READY_TIMEOUT=15 HARD_CAP_MINUTES=60
//   ONLY=B,E   run only these labelled runs (default all)
// Safety: the instance this creates is destroyed on every exit path (Stop,
// then the hard cap and the safety net in `finally`). Ids are printed as seen.
// Keys come from HKCU\Environment and are never printed. Scratch only: OUT
// must be outside the repo.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdirSync, readFileSync, rmSync, writeFileSync, existsSync, appendFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT;
const BASE = process.env.BASE;
const SHEET = process.env.SHEET;
if (!OUT || !BASE || !SHEET) throw new Error("set OUT, BASE and SHEET");
if (resolve(OUT).startsWith(resolve(APP, ".."))) throw new Error("OUT must be outside the repo");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const CATALOG_PORT = 18558;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const MAX_DPH = Number(process.env.MAX_DPH ?? 0.75);
const MAX_SESSION_MINUTES = Number(process.env.MAX_SESSION_MINUTES ?? 60);
const READY_TIMEOUT = Number(process.env.READY_TIMEOUT ?? 15);
const HARD_CAP_MINUTES = Number(process.env.HARD_CAP_MINUTES ?? 60);
const ONLY = (process.env.ONLY ?? "").split(",").filter(Boolean);

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
  unet: "wulver-v0.5-turbo.safetensors",
  clip: "qwen3vl_4b_fp8_scaled.safetensors",
  vae: "qwen_image_vae.safetensors",
  lora: "krea2_identity_edit_v1_2.safetensors",
};
const PREFIX =
  "Generate a new image based on the reference. Preserve the subject identity and key visual characteristics while applying this prompt: ";

/** Mirrors identity.rs::build_graph, plus the img2img and mask variants. */
function graph({ refs, size, prompt, seed, mode = "t2i", base, denoise = 1.0, mask = null, prefix }) {
  const g = {
    1: { class_type: "UNETLoader", inputs: { unet_name: MODEL.unet, weight_dtype: "default" } },
    2: { class_type: "CLIPLoader", inputs: { clip_name: MODEL.clip, type: "krea2", device: "default" } },
    3: { class_type: "VAELoader", inputs: { vae_name: MODEL.vae } },
    11: { class_type: "LoraLoaderModelOnly", inputs: { model: ["1", 0], lora_name: MODEL.lora, strength_model: 1.0 } },
  };
  let latent;
  if (mode === "t2i") {
    g[6] = { class_type: "EmptySD3LatentImage", inputs: { width: size[0], height: size[1], batch_size: 1 } };
    latent = ["6", 0];
  } else {
    g[40] = { class_type: "LoadImage", inputs: { image: base } };
    g[41] = { class_type: "VAEEncode", inputs: { pixels: ["40", 0], vae: ["3", 0] } };
    latent = ["41", 0];
    if (mode === "inpaint") {
      g[42] = { class_type: "LoadImageMask", inputs: { image: mask, channel: "red" } };
      g[43] = { class_type: "SetLatentNoiseMask", inputs: { samples: latent, mask: ["42", 0] } };
      latent = ["43", 0];
    }
  }
  const patch = { model: ["11", 0], ref_boost: 4.0, ref_boost_a: 1.0, fit_mode: "fit", vae: ["3", 0], target_latent: latent };
  const enc = { clip: ["2", 0] };
  refs.forEach((name, i) => {
    g[`2${i}`] = { class_type: "LoadImage", inputs: { image: name } };
    g[`3${i}`] = { class_type: "VAEEncode", inputs: { pixels: [`2${i}`, 0], vae: ["3", 0] } };
    const s = i === 0 ? "" : "_b";
    patch[`source_latent${s}`] = [`3${i}`, 0];
    patch[`source_image${s}`] = [`2${i}`, 0];
    enc[i === 0 ? "image" : "image_b"] = [`2${i}`, 0];
  });
  g[12] = { class_type: "Krea2EditModelPatch", inputs: patch };
  const encode = (text) => ({ class_type: "Krea2EditGroundedEncode", inputs: { ...enc, prompt: text, grounding_px: 768, system_prompt: "" } });
  g[4] = encode(PREFIX + prompt);
  g[5] = encode("");
  g[7] = {
    class_type: "KSampler",
    inputs: { seed, steps: 10, cfg: 1.0, sampler_name: "euler", scheduler: "simple", denoise, model: ["12", 0], positive: ["4", 0], negative: ["5", 0], latent_image: latent },
  };
  g[8] = { class_type: "VAEDecode", inputs: { samples: ["7", 0], vae: ["3", 0] } };
  g[9] = { class_type: "SaveImage", inputs: { filename_prefix: prefix, images: ["8", 0] } };
  return g;
}

const P1 = "Change the fox's green jacket into a red hoodie. Keep everything else exactly the same.";
const P2 = "Change the scene to night time with rain on the window. Keep the characters and layout the same.";
const BASE_SIZE = [896, 1152];
// A rough polygon around the fox's jacket in the base image (896x1152).
const JACKET = [[90, 300], [420, 300], [430, 650], [90, 650]];
const RUNS = [
  { label: "A-r1-base-only", note: "route 1, refs=[base]", opts: { refs: ["base.png"], size: BASE_SIZE, prompt: P1 } },
  { label: "B-r1-base+sheet", note: "route 1, refs=[base, sheet]", opts: { refs: ["base.png", "sheet.png"], size: BASE_SIZE, prompt: P1 } },
  { label: "C-r1-base+sheet-night", note: "route 1, global change", opts: { refs: ["base.png", "sheet.png"], size: BASE_SIZE, prompt: P2 } },
  { label: "D-r1-other-size", note: "route 1, 1024x1024 canvas", opts: { refs: ["base.png", "sheet.png"], size: [1024, 1024], prompt: P1 } },
  { label: "E-r2-d50", note: "img2img denoise 0.5", opts: { refs: ["base.png", "sheet.png"], prompt: P1, mode: "img2img", denoise: 0.5 } },
  { label: "F-r2-d70", note: "img2img denoise 0.7", opts: { refs: ["base.png", "sheet.png"], prompt: P1, mode: "img2img", denoise: 0.7 } },
  { label: "G-r2-d85", note: "img2img denoise 0.85", opts: { refs: ["base.png", "sheet.png"], prompt: P1, mode: "img2img", denoise: 0.85 } },
  { label: "H-r2-d70-night", note: "img2img denoise 0.7, global change", opts: { refs: ["base.png", "sheet.png"], prompt: P2, mode: "img2img", denoise: 0.7 } },
  { label: "I-r2-d70-base-only", note: "img2img 0.7, refs=[base]", opts: { refs: ["base.png"], prompt: P1, mode: "img2img", denoise: 0.7 } },
  { label: "J-r3-inpaint-d100", note: "mask over the jacket, denoise 1.0", opts: { refs: ["base.png", "sheet.png"], prompt: P1, mode: "inpaint", denoise: 1.0, mask: "mask.png" } },
  { label: "K-r3-inpaint-d85", note: "mask over the jacket, denoise 0.85", opts: { refs: ["base.png", "sheet.png"], prompt: P1, mode: "inpaint", denoise: 0.85, mask: "mask.png" } },
];

async function spike() {
  say("=== Edit spike ===");
  launch();
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 120000, "UI");
  const picked = await m.eval(`(() => { const s = document.getElementById('model');
    if (![...s.options].some(o => o.value === 'wulver-identity-edit')) return false;
    s.value = 'wulver-identity-edit'; s.dispatchEvent(new Event('change')); return true; })()`);
  if (!picked) throw new Error("wulver-identity-edit is not in the catalog");
  await sleep(3000);
  await m.eval("document.getElementById('start').click()");
  const s = await follow(m, (x) => x.kind === "ready", (READY_TIMEOUT * 3 + 10) * 60000, "ready");
  const id = s.instance_id;
  say(`READY instance ${id} (${s.offer.gpu_name}, $${s.offer.hourly.toFixed(4)}/hr)`);
  const inst = (await vast("GET", `/instances/${id}/`)).body?.instances ?? {};
  say(`host: machine ${inst.machine_id}, ${inst.geolocation}, ${inst.gpu_name}, ${inst.gpu_ram} MB`);

  // Feature 1, live: the button opens ComfyUI's own UI in the remote window.
  check("Open ComfyUI is offered", await m.eval("!document.getElementById('open-comfy').hidden"));
  await m.eval("document.getElementById('open-comfy').click()");
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 90000);
  const loaded = await waitFor(() => r.eval("!!document.querySelector('#vue-app, .comfyui-body-top, canvas')"), 120000, "ComfyUI UI").catch(() => false);
  await sleep(8000);
  await r.shot("comfyui-window");
  check("ComfyUI's own UI loads in the window", !!loaded, await r.eval("document.title"));
  const ipc = await r.eval(`(async () => { try { await window.__TAURI_INTERNALS__.invoke('get_snapshot'); return 'ALLOWED'; } catch (e) { return 'denied'; } })()`);
  check("remote window IPC denied", ipc === "denied");
  const wf = await r.eval(`fetch('/api/userdata?dir=workflows').then(r => r.json()).catch(() => fetch('/userdata?dir=workflows').then(r => r.json()))`).catch((e) => String(e));
  say(`workflows: ${JSON.stringify(wf)}`);
  check("the Identity Edit workflow is preloaded", JSON.stringify(wf).includes("SlopTweak Identity Edit"));
  const foreign = await r.eval(`fetch('/queue', { headers: { Origin: 'https://evil.example' } }).then(r => r.status)`);
  say(`same-page fetch with a foreign Origin header status: ${foreign} (browsers forbid setting Origin; informational)`);
  const wsOk = await r.eval(`new Promise((res) => { const w = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws?clientId=spike-ws'); w.onopen = () => { w.close(); res(true); }; w.onerror = () => res(false); setTimeout(() => res(false), 8000); })`);
  check("ComfyUI websocket connects through the tunnel", wsOk === true);

  // Node docs for the notes.
  const info = await r.eval(`Promise.all(['Krea2EditModelPatch','Krea2EditGroundedEncode'].map(n => fetch('/object_info/' + n).then(r => r.json())))`);
  writeFileSync(join(OUT, "object_info.json"), JSON.stringify(info, null, 2));
  say("saved object_info.json");

  await r.eval(PAGE_HELPERS);
  const b64 = (f) => readFileSync(f).toString("base64");
  say(`upload base: ${await r.eval(`window.__spike.upload('base.png', ${JSON.stringify(b64(BASE))})`)}`);
  say(`upload sheet: ${await r.eval(`window.__spike.upload('sheet.png', ${JSON.stringify(b64(SHEET))})`)}`);
  say(`upload mask: ${await r.eval(`window.__spike.mask('mask.png', ${BASE_SIZE[0]}, ${BASE_SIZE[1]}, ${JSON.stringify(JACKET)})`)}`);

  const summary = [];
  let seed = 424242;
  for (const run of RUNS) {
    if (ONLY.length && !ONLY.includes(run.label[0])) continue;
    const g = graph({ ...run.opts, seed, base: "base.png", prefix: `spike_${run.label}` });
    say(`run ${run.label}: ${run.note}`);
    const res = await r.eval(`window.__spike.run(${JSON.stringify(g)}, 600000)`);
    if (res?.png) {
      writeFileSync(join(OUT, `${run.label}.png`), Buffer.from(res.png, "base64"));
      say(`  -> ${run.label}.png in ${res.seconds}s`);
      summary.push({ label: run.label, note: run.note, seconds: res.seconds });
    } else {
      say(`  -> FAILED: ${res?.error}`);
      summary.push({ label: run.label, note: run.note, error: res?.error });
    }
  }
  const stats = await r.eval("fetch('/system_stats').then(r => r.json())");
  writeFileSync(join(OUT, "summary.json"), JSON.stringify({ summary, system: stats }, null, 2));
  say(`system_stats: ${JSON.stringify(stats?.devices?.map((d) => ({ name: d.name, vram_total: d.vram_total, vram_free: d.vram_free, torch_vram_total: d.torch_vram_total, torch_vram_free: d.torch_vram_free })))}`);
  check("at least one spike run produced an image", summary.some((x) => x.seconds));
  r.ws.close();

  // Stop through the app; the safety net below covers any failure.
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
  res.end(readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8"));
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
