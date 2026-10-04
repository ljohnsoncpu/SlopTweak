// Live acceptance for Identity Edit's strength slider, painted-area edits and
// zoomed redraw, plus the Open ComfyUI workflow load. SPENDS MONEY: run only
// with approval. It drives the app's own panel (the same code a user clicks):
//   S1  edit, strength 100, nothing painted       (regression: the plain graph)
//   S2  edit, strength 70                         (img2img)
//   S3  edit, jacket painted, zoom off            (masked, whole picture)
//   S4  edit, eyes painted, zoom on               (masked, zoomed redraw)
// The painting is done with real mouse events on the editor's canvas.
//
// Usage (from app/):  BASE=base.png SHEET=sheet.png OUT=<scratch dir> node dev/edit-live.mjs
//   MAX_DPH=0.70 MAX_SESSION_MINUTES=60 READY_TIMEOUT=15 HARD_CAP_MINUTES=75
//   ONLY=S3,S4   run only these scenarios
// Safety: the instance this creates is destroyed on every exit path (Stop, then
// the hard cap and the safety net in `finally`); ids are printed as seen. Keys
// come from HKCU\Environment and are never printed. OUT must be outside the repo.

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
const CATALOG_PORT = 18559;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const MAX_DPH = Number(process.env.MAX_DPH ?? 0.7);
const MAX_SESSION_MINUTES = Number(process.env.MAX_SESSION_MINUTES ?? 60);
const READY_TIMEOUT = Number(process.env.READY_TIMEOUT ?? 15);
const HARD_CAP_MINUTES = Number(process.env.HARD_CAP_MINUTES ?? 75);
const ONLY = (process.env.ONLY ?? "").split(",").filter(Boolean);

mkdirSync(OUT, { recursive: true });
const LOG = join(OUT, "live.log");
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
  app = spawn(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_DEV_IMPORT_KEYS: "1",
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
      // Slot 0 = sheet, slot 2 = the picture to edit (answers the file dialog).
      SLOPTWEAK_DEV_PICK_REF: `${resolve(SHEET)};${resolve(SHEET)};${resolve(BASE)}`,
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  app.stdout.on("data", (d) => appendFileSync(logFile, d));
  app.stderr.on("data", (d) => appendFileSync(logFile, d));
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
async function waitFor(fn, ms, what, every = 1000) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(every);
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
    send,
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

// ----- driving the panel --------------------------------------------------------

const $ = (id) => `document.getElementById(${JSON.stringify(id)})`;
const setValue = (id, v) =>
  `(() => { const i = ${$(id)}; i.value = ${JSON.stringify(String(v))}; i.dispatchEvent(new Event('input')); i.dispatchEvent(new Event('change')); })()`;

/** Paint with real mouse events: `strokes` are lists of points in picture pixels. */
async function paint(m, strokes, brush) {
  await m.eval(setValue("mask-size", brush));
  const geo = await m.eval(`(() => { const c = ${$("mask-canvas")}; const r = c.getBoundingClientRect();
    const i = ${$("mask-base")}; return { left: r.left, top: r.top, w: r.width, h: r.height, nw: i.naturalWidth, nh: i.naturalHeight }; })()`);
  // Points are given in a 896x1152 space whatever the picture's real size.
  const at = ([x, y]) => ({ x: geo.left + (x / 896) * geo.w, y: geo.top + (y / 1152) * geo.h });
  for (const stroke of strokes) {
    const pts = stroke.map(at);
    const ev = (type, p, buttons) =>
      m.send("Input.dispatchMouseEvent", { type, x: p.x, y: p.y, button: "left", buttons, clickCount: 1 });
    await ev("mouseMoved", pts[0], 0);
    await ev("mousePressed", pts[0], 1);
    for (const p of pts.slice(1)) await ev("mouseMoved", p, 1);
    await ev("mouseReleased", pts.at(-1), 0);
  }
  return geo;
}
/** Zig-zag strokes filling a rectangle of the picture. */
function fill(x0, y0, x1, y1, step) {
  const out = [];
  for (let y = y0, i = 0; y <= y1; y += step, i++) out.push(i % 2 ? [[x1, y], [x0, y]] : [[x0, y], [x1, y]]);
  return out;
}

async function scenario(m, name, { strength = 100, prompt, paintSpec = null, zoom = true }) {
  say(`=== ${name} ===`);
  if (!(await m.eval(`${$("ref-img-2")}.querySelector('img') !== null`))) {
    await m.eval(`${$("ref-pick-2")}.click()`);
    await waitFor(() => m.eval(`${$("ref-img-2")}.querySelector('img') !== null`), 15000, "picture");
  }
  // A fresh pick replaces the painted area (new picture id). The panel's state
  // carries every result at full size, so after big pictures the refresh is slow:
  // wait until the old painted area is really gone before painting a new one.
  await m.eval(`${$("ref-pick-2")}.click()`);
  await waitFor(() => m.eval(`${$("mask-clear")}.hidden`), 120000, "the old painted area to clear", 500);
  await sleep(1500);
  await m.eval(setValue("id-strength", strength));
  if (paintSpec) {
    await m.eval(`${$("mask-open")}.click()`);
    await waitFor(() => m.eval(`!${$("masker")}.hidden && ${$("mask-base")}.naturalWidth > 0`), 15000, "mask editor");
    await sleep(500);
    const geo = await paint(m, paintSpec.strokes, paintSpec.brush);
    await m.shot(`${name}-painting`);
    await m.eval(`${$("mask-done")}.click()`);
    await waitFor(() => m.eval(`!${$("mask-clear")}.hidden`), 30000, "painted area kept");
    await m.eval(`${$("mask-zoom")}.checked = ${zoom}`);
    say(`picture ${geo.nw}x${geo.nh}, editor ${Math.round(geo.w)}x${Math.round(geo.h)}`);
  }
  await m.eval(setValue("id-prompt", prompt));
  const before = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then(s => s.results.length)");
  const t = Date.now();
  await m.eval(`${$("id-go")}.click()`);
  const end = await waitFor(async () => {
    const s = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then(s => ({ phase: s.phase, n: s.results.length, msg: s.message }))");
    if (s.phase === "failed") return { failed: s.msg };
    return s.phase === "done" && s.n > before ? { ok: true } : false;
  }, 12 * 60000, `${name} to finish`, 2000);
  const seconds = Math.round((Date.now() - t) / 1000);
  if (end.failed) {
    check(`${name} produced an image`, false, end.failed);
    return;
  }
  const url = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then(s => s.results[0].data_url)");
  writeFileSync(join(OUT, `${name}.png`), Buffer.from(url.split(",")[1], "base64"));
  const size = await m.eval(`new Promise((res) => { const i = new Image(); i.onload = () => res(i.naturalWidth + 'x' + i.naturalHeight); i.src = ${JSON.stringify(url)}; })`);
  check(`${name} produced an image`, true, `${seconds}s, ${size}`);
}

// ----- the run ------------------------------------------------------------------

const JACKET = { strokes: fill(105, 330, 400, 580, 40), brush: 60 };
const EYES = { strokes: fill(205, 245, 320, 280, 14), brush: 16 };
const SCENARIOS = [
  ["S1", { strength: 100, prompt: "Change the fox's green jacket into a red hoodie. Keep everything else exactly the same." }],
  ["S2", { strength: 70, prompt: "Change the fox's green jacket into a red hoodie. Keep everything else exactly the same." }],
  ["S3", { prompt: "Change the fox's green jacket into a red hoodie.", paintSpec: JACKET, zoom: false }],
  ["S4", { prompt: "Make the fox's eyes bright green. Keep everything else exactly the same.", paintSpec: EYES, zoom: true }],
];

async function live() {
  say("=== Identity Edit live run ===");
  launch();
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(() => m.eval(`${$("model")}.options.length`).then((n) => n > 0), 120000, "UI");
  const picked = await m.eval(`(() => { const s = ${$("model")};
    if (![...s.options].some(o => o.value === 'wulver-identity-edit')) return false;
    s.value = 'wulver-identity-edit'; s.dispatchEvent(new Event('change')); return true; })()`);
  if (!picked) throw new Error("wulver-identity-edit is not in the catalog");
  await sleep(3000);
  await m.eval(`${$("start")}.click()`);
  const s = await follow(m, (x) => x.kind === "ready", (READY_TIMEOUT * 3 + 10) * 60000, "ready");
  const id = s.instance_id;
  say(`READY instance ${id} (${s.offer.gpu_name}, $${s.offer.hourly.toFixed(4)}/hr)`);
  const inst = (await vast("GET", `/instances/${id}/`)).body?.instances ?? {};
  say(`host: machine ${inst.machine_id}, ${inst.geolocation}, ${inst.gpu_name}, ${inst.gpu_ram} MB`);

  // Open ComfyUI: the preloaded workflow must open (it was an empty canvas before).
  await m.eval(`${$("open-comfy")}.click()`);
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 90000);
  await waitFor(() => r.eval("!!document.querySelector('#vue-app, .comfyui-body-top, canvas')"), 120000, "ComfyUI UI");
  await sleep(6000);
  const file = await r.eval(`fetch('/api/userdata/workflows%2FSlopTweak%20Identity%20Edit.json').then(async (x) => ({ status: x.status, nodes: (await x.json().catch(() => ({}))).nodes?.length ?? 0 }))`);
  check("the workflow file opens through the tunnel", file.status === 200 && file.nodes > 20, JSON.stringify(file));
  const loaded = await r.eval(`(async () => {
    const wf = await (await fetch('/api/userdata/workflows%2FSlopTweak%20Identity%20Edit.json')).json();
    await window.app.loadGraphData(wf);
    await new Promise((res) => setTimeout(res, 1500));
    const names = {};
    for (const n of window.app.graph._nodes) if (/Loader/.test(n.type)) names[n.type] = n.widgets?.[0]?.value;
    return { nodes: window.app.graph._nodes.length, names };
  })()`).catch((e) => ({ error: String(e) }));
  say(`graph in the UI: ${JSON.stringify(loaded)}`);
  check("the workflow loads on the canvas", (loaded.nodes ?? 0) > 20);
  const missing = await r.eval(`fetch('/object_info/LoraLoaderModelOnly').then(x => x.json()).then(j => j.LoraLoaderModelOnly.input.required.lora_name[0])`);
  check("its LoRA picker names an installed file", Array.isArray(missing) && missing.includes(loaded.names?.LoraLoaderModelOnly), JSON.stringify(missing));
  await r.shot("comfyui-workflow");
  r.ws.close();

  // The panel.
  await m.eval(`(() => { const s = ${$("id-mode")}; s.value = 'edit'; s.dispatchEvent(new Event('change')); })()`);
  await m.eval(`${$("ref-pick-0")}.click()`);
  await sleep(1000);
  for (const [name, spec] of SCENARIOS) {
    if (ONLY.length && !ONLY.includes(name)) continue;
    try {
      await scenario(m, name, spec);
    } catch (e) {
      check(`${name} ran`, false, String(e));
      await m.shot(`${name}-error`).catch(() => {});
      await m.eval(`${$("masker")}.hidden = true`).catch(() => {});
    }
  }
  const log = await m.eval(`${$("id-msg")}.textContent`);
  say(`panel message: ${log}`);

  await m.eval(`${$("stop")}.click()`);
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
  await live();
} catch (e) {
  check("live run completed", false, String(e));
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
