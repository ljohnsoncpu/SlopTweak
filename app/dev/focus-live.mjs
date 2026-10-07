// Live A/B for "Focus on a part": the same sheet, prompt, seed and shape with the
// painted part sent (focus) and without it (whole sheet). SPENDS MONEY (unless
// MOCK=1): run only with approval. Run sheet: docs/spikes/focus-ab-run-sheet.md.
//
// It drives the app's own panel (the same code a user clicks), including the
// painting (real mouse events on the editor's canvas).
//
// Usage (from app/):
//   OUT=<dir outside the repo> CONFIG=<config.json> node dev/focus-live.mjs
//   node dev/focus-live.mjs --restore-settings     (after you have stopped the GPU)
//
// CONFIG is JSON: { "sheets": {"fox": "C:\\..\\fox.png", ...},
//                   "sizes":  {"fox": [width, height], ...},
//                   "faces":  {"fox": [x0, y0, x1, y1], ...} }   (fractions 0..1)
// Sheets can be PNG, JPEG or WebP. Environment:
//   MAX_DPH=0.75  IDLE_MINUTES=60  MAX_SESSION_MINUTES=180  HEARTBEAT_MINUTES=10
//   READY_TIMEOUT=30  HARD_CAP_MINUTES=75   ONLY=A,B   PAIRS=2 (seeds per case)
//   KEEP=1 (default) leave the GPU, the app and vite running at the end.
//   MOCK=1 run against MockProvider: $0, no Vast calls, no settings changes.
//
// Safety, and the one deliberate exception: with KEEP=1 the instance is NOT
// destroyed at the end (the user asked to keep the machine for their own use).
// Its own watchdog is the backstop: idle, max session and a lost heartbeat all
// destroy it. Everything else is destroyed on every exit path: a run that fails
// before the GPU is ready, the hard cap, or KEEP=0. Ids are printed as seen. Keys
// come from HKCU\Environment and are never printed. OUT must be outside the repo.
//
// Blind: stdout and live.log never say which arm an image is. Each pair is written
// as pNN_x.png / pNN_y.png in a random order; the answer, timings and crop sizes
// are in key.json. Don't open key.json until the ratings are done.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { randomInt } from "node:crypto";
import {
  appendFileSync, closeSync, copyFileSync, existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const MOCK = process.env.MOCK === "1";
const KEEP = process.env.KEEP !== "0";
const MAIN_PORT = 9334;
const CATALOG_PORT = 18559;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const settingsFile = join(CONFIG_DIR, "settings.json");
const backupFile = settingsFile + ".acceptance-backup";

const restoreSettings = () => {
  if (!existsSync(backupFile)) return false;
  const backup = readFileSync(backupFile, "utf8");
  if (backup) writeFileSync(settingsFile, backup);
  else rmSync(settingsFile, { force: true });
  rmSync(backupFile, { force: true });
  return true;
};
if (process.argv.includes("--restore-settings")) {
  console.log(restoreSettings() ? "settings restored" : "nothing to restore (no backup file)");
  process.exit(0);
}

const OUT = process.env.OUT;
const CONFIG = process.env.CONFIG;
if (!OUT || !CONFIG) throw new Error("set OUT and CONFIG");
if (resolve(OUT).startsWith(resolve(APP, ".."))) throw new Error("OUT must be outside the repo");
const cfg = JSON.parse(readFileSync(CONFIG, "utf8"));
const MAX_DPH = Number(process.env.MAX_DPH ?? 0.75);
const IDLE_MINUTES = Number(process.env.IDLE_MINUTES ?? 60);
const MAX_SESSION_MINUTES = Number(process.env.MAX_SESSION_MINUTES ?? 180);
const HEARTBEAT_MINUTES = Number(process.env.HEARTBEAT_MINUTES ?? 10);
const READY_TIMEOUT = Number(process.env.READY_TIMEOUT ?? 30);
const HARD_CAP_MINUTES = Number(process.env.HARD_CAP_MINUTES ?? 75);
const ONLY = (process.env.ONLY ?? "").split(",").filter(Boolean);
const PAIRS = Number(process.env.PAIRS ?? 99);
const SEED_FILE = join(OUT, "seed.txt");

mkdirSync(OUT, { recursive: true });
const LOG = join(OUT, "live.log");
const t0 = Date.now();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** Progress that is safe to print: never names an arm. */
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

// ----- the pictures and the matrix ---------------------------------------------

const sizeOf = (name) => {
  const z = cfg.sizes?.[name];
  if (!z) throw new Error(`no size for ${name} in the config`);
  return { w: z[0], h: z[1] };
};
for (const name of Object.keys(cfg.sheets)) {
  if (!existsSync(cfg.sheets[name])) throw new Error(`missing sheet ${name}: ${cfg.sheets[name]}`);
  sizeOf(name);
  if (name !== "husky" && !cfg.faces?.[name]) throw new Error(`no face box for ${name}`);
}

/** What the app cuts out for a painted box (mirrors `focus_crop` in identity.rs). */
function focusCrop(img, b) {
  const pad = Math.max(Math.floor(Math.max(b.w, b.h) / 4), 32);
  const x0 = Math.max(0, b.x - pad);
  const y0 = Math.max(0, b.y - pad);
  const x1 = Math.min(img.w, b.x + b.w + pad);
  const y1 = Math.min(img.h, b.y + b.h + pad);
  const w = x1 - x0;
  const h = y1 - y0;
  const area = w * h;
  const resize = area < 256 * 1024 ? "enlarged to 0.5 MP" : area > 1.5 * 1024 * 1024 ? "shrunk to 1 MP" : "as is";
  return { x: x0, y: y0, w, h, area, resize };
}

const CASES = [
  {
    id: "A", mode: "new", aspect: "portrait", sheets: { 0: "fox" }, focus: [0],
    prompt: "Close-up portrait of the character, smiling, sitting at a sunny cafe table with a coffee, soft daylight.",
    seeds: [111111, 222222, 333333],
  },
  {
    id: "B", mode: "new", aspect: "portrait", sheets: { 0: "fox", 1: "tiger" }, focus: [0, 1],
    prompt: "The two characters sit together at a sunny cafe table, laughing.",
    seeds: [444444, 555555],
  },
  {
    id: "C", mode: "edit", base: "husky", sheets: { 0: "tiger" }, focus: [0],
    prompt: "Replace the character's head and face with the character from the reference sheet. Keep the pose, the suit and the white background.",
    seeds: [666666, 777777],
  },
];

// ----- Vast (verification, the safety net and the hard cap only) ----------------

function userEnv(name) {
  if (process.env[name]) return process.env[name].trim();
  const out = execSync(`reg query HKCU\\Environment /v ${name}`, { encoding: "utf8" });
  const m = out.match(new RegExp(`${name}\\s+REG_\\w+\\s+(\\S+)`));
  if (!m) throw new Error(`${name} not set`);
  return m[1].trim();
}
const VAST_KEY = MOCK ? "" : userEnv("VAST_API_KEY");
async function vast(method, path) {
  const r = await fetch(VAST + path, { method, headers: { Authorization: `Bearer ${VAST_KEY}` } });
  const text = await r.text();
  return { status: r.status, body: text ? JSON.parse(text) : null };
}
async function ourInstances() {
  if (MOCK) return [];
  const { body } = await vast("GET", "/instances/");
  return (body?.instances ?? []).filter((i) => (i.label ?? "").startsWith("sloptweak"));
}
const credit = async () => (MOCK ? 0 : (await vast("GET", "/users/current")).body?.credit);

// ----- app + CDP ----------------------------------------------------------------

let app = null;
let viteProc = null;
let catalogServer = null;
let appPid = null;
const APP_LOG = join(OUT, "app.log");
const appLog = () => (existsSync(APP_LOG) ? readFileSync(APP_LOG, "utf8") : "");
const kill = (pid) => {
  try {
    execSync(`taskkill /T /F /PID ${pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
};
function launch() {
  const logFile = APP_LOG;
  const fd = openSync(logFile, "a");
  // Files, not pipes: the app must be able to outlive this script.
  app = spawn(EXE, [], {
    env: {
      ...process.env,
      ...(MOCK ? { SLOPTWEAK_PROVIDER: "mock", SLOPTWEAK_DEV_RESET: "1" } : { SLOPTWEAK_DEV_IMPORT_KEYS: "1" }),
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      SLOPTWEAK_DEV_SEED_FILE: SEED_FILE,
    },
    stdio: ["ignore", fd, fd],
    detached: true,
    windowsHide: false,
  });
  app.unref();
  closeSync(fd);
  appPid = app.pid;
  say(`app launched, log ${logFile}`);
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
  };
}
const snapshot = (m) => m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot').then(s => s.state)");
let instanceId = null;
let createdAt = Date.now();
let offerLine = "";
async function follow(m, pred, ms, what) {
  let last = "";
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
    if (s.instance_id && s.instance_id !== instanceId) {
      instanceId = s.instance_id;
      createdAt = Date.now();
      say(`INSTANCE ${instanceId} created`);
    }
    if (s.offer) offerLine = `${s.offer.gpu_name} $${s.offer.hourly.toFixed(4)}/hr (offer ${s.offer.offer_id})`;
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
const invoke = (m, cmd, args = {}) =>
  m.eval(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}).then(() => true)`);
const hasImg = (m, slot) => m.eval(`${$(`ref-img-${slot}`)}.querySelector('img:not(.mask-overlay)') !== null`);

/** Put a picture in a slot (or empty it), and wait until the panel shows exactly that. */
async function setSlot(m, slot, file) {
  const shows = (name) => m.eval(`(() => { const i = ${$(`ref-img-${slot}`)}.querySelector('img:not(.mask-overlay)');
    return i !== null && i.title.startsWith(${JSON.stringify(name)}); })()`);
  if (file) {
    const name = file.split(/[\/]/).pop();
    await invoke(m, "identity_set_ref_path", { slot, path: resolve(file) });
    await waitFor(() => shows(name), 60000, `slot ${slot} to show ${name}`);
  } else {
    // Refreshes from earlier changes can still be in flight and overwrite this one,
    // so look again and clear again if the panel went back to showing a picture.
    for (let attempt = 0; attempt < 4; attempt++) {
      if (!(await hasImg(m, slot))) {
        await sleep(1500);
        if (!(await hasImg(m, slot))) return;
      }
      // The panel's own button: the backend doesn't announce a direct clear.
      await m.eval(`${$(`ref-clear-${slot}`)}.click()`);
      await sleep(3000);
    }
    throw new Error(`slot ${slot} would not clear`);
  }
}

/** Paint a face box (fractions of the picture) on a sheet's slot, then Done. */
async function paintFace(m, slot, box) {
  await m.eval(`${$(`ref-focus-${slot}`)}.click()`);
  await waitFor(() => m.eval(`!${$("masker")}.hidden && ${$("mask-base")}.naturalWidth > 0`), 20000, "the painting window");
  await sleep(600);
  const geo = await m.eval(`(() => { const r = ${$("mask-canvas")}.getBoundingClientRect();
    return { left: r.left, top: r.top, w: r.width, h: r.height }; })()`);
  // The brush is as wide as 5% of the picture; fill the box inset by half a brush so
  // the painted box comes out about the size asked for.
  const brush = Math.min(160, Math.max(6, Math.round(geo.w * 0.05)));
  await m.eval(setValue("mask-size", brush));
  const x0 = geo.left + box[0] * geo.w + brush / 2;
  const x1 = geo.left + box[2] * geo.w - brush / 2;
  const y0 = geo.top + box[1] * geo.h + brush / 2;
  const y1 = geo.top + box[3] * geo.h - brush / 2;
  const step = brush * 0.8;
  const ev = (type, x, y, buttons) =>
    m.send("Input.dispatchMouseEvent", { type, x, y, button: "left", buttons, clickCount: 1 });
  const rows = [];
  for (let y = y0; y <= y1; y += step) rows.push(y);
  if (rows.at(-1) < y1 - 1) rows.push(y1); // always reach the bottom edge
  for (const [i, y] of rows.entries()) {
    const [a, b] = i % 2 ? [x1, x0] : [x0, x1];
    await ev("mouseMoved", a, y, 0);
    await ev("mousePressed", a, y, 1);
    await ev("mouseMoved", (a + b) / 2, y, 1);
    await ev("mouseMoved", b, y, 1);
    await ev("mouseReleased", b, y, 0);
  }
  await m.eval(`${$("mask-done")}.click()`);
  await waitFor(() => m.eval(`!${$(`ref-unfocus-${slot}`)}.hidden`), 30000, `the part on slot ${slot} to be kept`);
}

async function dropFocus(m, slot) {
  if (await m.eval(`!${$(`ref-unfocus-${slot}`)}.hidden`)) {
    await m.eval(`${$(`ref-unfocus-${slot}`)}.click()`);
    await waitFor(() => m.eval(`${$(`ref-unfocus-${slot}`)}.hidden`), 10000, `slot ${slot} to use the whole picture`);
  }
}

const key = { note: "Answer key. Do not open until the ratings are done.", pairs: [], images: [] };
const writeKey = () => writeFileSync(join(OUT, "key.json"), JSON.stringify(key, null, 2));

/** Make one image and return its bytes and what the app sent. */
async function generate(m, c, seed) {
  writeFileSync(SEED_FILE, String(seed));
  await m.eval(setValue("id-prompt", c.prompt));
  const before = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then(s => s.results.map(r => r.id))");
  const logFrom = appLog().length;
  const t = Date.now();
  await m.eval(`${$("id-go")}.click()`);
  const end = await waitFor(async () => {
    // The panel hides a refused request; the debug build logs it.
    const mine = appLog().slice(logFrom);
    const refused = mine.match(/\[dev-focus\] refused: (.*)/);
    if (refused) return { failed: `refused: ${refused[1]}` };
    const s = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then(s => ({ phase: s.phase, ids: s.results.map(r => r.id), msg: s.message }))");
    if (s.phase === "failed") return { failed: s.msg };
    // A request the app refuses shows its reason in the panel and never starts.
    const shown = await m.eval(`({ cls: ${$("id-msg")}.className, text: ${$("id-msg")}.textContent })`);
    if (s.phase === "idle" && shown.cls.includes("notice") && shown.text) return { failed: `panel: ${shown.text}` };
    const fresh = s.ids.find((id) => !before.includes(id));
    return s.phase === "done" && fresh ? { id: fresh } : false;
  }, 12 * 60000, "an image to finish", 2000);
  const seconds = Math.round((Date.now() - t) / 1000);
  if (end.failed) return { failed: end.failed, seconds };
  const url = await m.eval(`window.__TAURI_INTERNALS__.invoke('identity_state').then(s => s.results.find(r => r.id === ${JSON.stringify(end.id)}).data_url)`);
  const size = await m.eval(`new Promise((res) => { const i = new Image(); i.onload = () => res(i.naturalWidth + 'x' + i.naturalHeight); i.src = ${JSON.stringify(url)}; })`);
  // What the app was asked for, from its log (the line carries this image's seed).
  const line = appLog().slice(logFrom).split("\n").reverse().find((l) => l.includes("[dev-focus] {") && l.includes(`"seed":${seed}`));
  const sent = line ? JSON.parse(line.slice(line.indexOf("{"))) : null;
  return { bytes: Buffer.from(url.split(",")[1], "base64"), seconds, size, sent };
}

let pairNo = 0;
async function runCase(m, c) {
  say(`=== case ${c.id} ===`);
  await m.eval(setValue("id-mode", c.mode));
  await m.eval(setValue("id-aspect", c.aspect ?? "square")); // edit mode ignores it
  await m.eval(setValue("id-strength", 100));
  await setSlot(m, 0, cfg.sheets[c.sheets[0]]);
  await setSlot(m, 1, c.sheets[1] ? cfg.sheets[c.sheets[1]] : null);
  await setSlot(m, 2, c.base ? cfg.sheets[c.base] : null);
  await sleep(1500);
  const seeds = c.seeds.slice(0, PAIRS);
  // Which image of a pair is x, and which arm goes first, are coin flips.
  const pairs = seeds.map((seed) => ({
    seed, no: String(++pairNo).padStart(2, "0"), xIsFocus: randomInt(2) === 1,
  }));
  const arms = randomInt(2) === 1 ? ["focus", "whole"] : ["whole", "focus"];
  for (const [block, arm] of arms.entries()) {
    say(`case ${c.id}: block ${block + 1} of 2`);
    for (const slot of c.focus) {
      if (arm === "focus") {
        if (await m.eval(`!${$(`ref-unfocus-${slot}`)}.hidden`)) continue;
        await paintFace(m, slot, cfg.faces[c.sheets[slot]]);
      } else {
        await dropFocus(m, slot);
      }
    }
    for (const p of pairs) {
      const r = await generate(m, c, p.seed);
      const label = arm === "focus" === p.xIsFocus ? "x" : "y";
      const file = `p${p.no}_${label}.png`;
      if (r.failed) {
        check(`pair ${p.no} image ${label} made`, false, r.failed);
        key.images.push({ pair: p.no, label, failed: r.failed });
        writeKey();
        continue;
      }
      writeFileSync(join(OUT, file), r.bytes);
      let crops = null;
      if (arm === "focus" && r.sent?.focus?.length) {
        crops = r.sent.focus.map((f) => {
          const [x, y, w, h] = f.bbox;
          return { slot: f.slot, painted: { x, y, w, h }, crop: focusCrop(sizeOf(c.sheets[f.slot]), { x, y, w, h }) };
        });
      }
      key.images.push({ pair: p.no, label, arm, case: c.id, seed: p.seed, file, seconds: r.seconds, size: r.size, crops });
      if (arm === "focus" && !crops) key.images.at(-1).cropsNote = "no focus in the request";
      if (arm === "whole" && r.sent?.focus?.length) key.images.at(-1).cropsNote = "UNEXPECTED: focus was sent";
      writeKey();
      say(`pair ${p.no} image ${label} done`);
      check(`pair ${p.no} image ${label} made`, true);
    }
  }
  for (const p of pairs) {
    key.pairs.push({ pair: p.no, case: c.id, seed: p.seed, x: p.xIsFocus ? "focus" : "whole", y: p.xIsFocus ? "whole" : "focus" });
  }
  writeKey();
  return pairs.map((p) => ({ ...p, c }));
}

/** A page with the references and each pair's x and y side by side, no labels. */
function writeReview(done) {
  const copy = (name) => {
    const dest = `ref_${name}.png`;
    if (!existsSync(join(OUT, dest))) copyFileSync(cfg.sheets[name], join(OUT, dest));
    return dest;
  };
  const rows = done.map(({ no, seed, c }) => {
    const refs = [...Object.values(c.sheets), ...(c.base ? [c.base] : [])]
      .map((n) => `<figure><img src="${copy(n)}"><figcaption>${n === c.base ? "picture to edit" : "sheet"}: ${n}</figcaption></figure>`)
      .join("");
    return `<section><h2>Pair ${no} (case ${c.id}, seed ${seed})</h2><p>${c.prompt}</p>
<div class="row">${refs}<figure><img src="p${no}_x.png"><figcaption>x</figcaption></figure><figure><img src="p${no}_y.png"><figcaption>y</figcaption></figure></div></section>`;
  });
  writeFileSync(join(OUT, "review.html"), `<!doctype html><meta charset="utf-8"><title>Focus A/B review</title>
<style>body{font:14px sans-serif;margin:16px}.row{display:flex;gap:12px;align-items:flex-start}figure{margin:0;flex:1}img{width:100%}</style>
<h1>Focus A/B (blind)</h1><p>Rate x and y on likeness to the sheet, outfit, layout repeat and prompt following.</p>${rows.join("")}`);
}

// ----- the run ------------------------------------------------------------------

let ready = false;
let handedOff = false;
let hardCap = null;

async function live() {
  say(`=== Focus A/B ${MOCK ? "(MOCK, $0)" : "(live)"} ===`);
  launch();
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(() => m.eval(`${$("model")}.options.length`).then((n) => n > 0), 120000, "UI");
  if (MOCK) {
    const next = async () => {
      await m.eval(`${$("wizard-next")}.click()`);
      await sleep(200);
    };
    for (let i = 0; i < 3; i++) await next();
    await m.eval(setValue("key-vast", "ab".repeat(32)));
    await waitFor(async () => (await m.eval(`${$("key-vast-msg")}.textContent`)).includes("Connected"), 10000, "vast key");
    await next();
    await next();
    await m.eval(setValue("key-civitai", "mockcivitaikey0123456789abcdef"));
    await m.eval(`${$("key-civitai")}.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter'}))`);
    await waitFor(async () => (await m.eval(`${$("key-civitai-msg")}.textContent`)).includes("mock-user"), 10000, "civitai key");
    await next();
    await next();
    await m.eval(`${$("nav-settings")}.click()`);
    await sleep(300);
    await m.eval(`${$("s-max-dph")}.value = '${MAX_DPH.toFixed(2)}'`);
    await m.eval(`${$("settings-form")}.requestSubmit()`);
    await waitFor(async () => (await m.eval(`${$("settings-msg")}.textContent`)).includes("Saved"), 5000, "settings saved");
    await m.eval(`${$("nav-home")}.click()`);
    await sleep(500);
  }
  const picked = await m.eval(`(() => { const s = ${$("model")};
    if (![...s.options].some(o => o.value === 'wulver-identity-edit')) return false;
    s.value = 'wulver-identity-edit'; s.dispatchEvent(new Event('change')); return true; })()`);
  if (!picked) throw new Error("wulver-identity-edit is not in the catalog");
  await sleep(3000);
  await m.eval(`${$("start")}.click()`);
  const s = await follow(m, (x) => x.kind === "ready", (READY_TIMEOUT * 3 + 10) * 60000, "ready");
  ready = true;
  say(`READY instance ${s.instance_id} (${s.offer.gpu_name}, $${s.offer.hourly.toFixed(4)}/hr)`);
  if (!MOCK) {
    const inst = (await vast("GET", `/instances/${s.instance_id}/`)).body?.instances ?? {};
    say(`host: machine ${inst.machine_id}, ${inst.geolocation}, ${inst.gpu_name}, ${inst.gpu_ram} MB`);
  }

  const done = [];
  for (const c of CASES) {
    if (ONLY.length && !ONLY.includes(c.id)) continue;
    try {
      done.push(...(await runCase(m, c)));
    } catch (e) {
      check(`case ${c.id} ran`, false, String(e));
      await m.eval(`${$("masker")}.hidden = true`).catch(() => {});
    }
    writeReview(done);
  }
  await m.eval(setValue("id-mode", "new")).catch(() => {});
  m.ws.close();
  say(`images are in ${OUT} (open review.html; key.json stays closed until the ratings are done)`);
}

/** Leave the GPU, the app and vite running, and say how it ends. */
async function handOff() {
  clearTimeout(hardCap);
  handedOff = true;
  // The app's catalog URL points at this script's server: keep one running.
  await new Promise((r) => catalogServer.close(r));
  const code = `require('http').createServer((q, r) => { r.setHeader('content-type', 'application/json'); r.end(require('fs').readFileSync(${JSON.stringify(join(APP, "..", "catalog", "catalog.json"))}, 'utf8')); }).listen(${CATALOG_PORT}, '127.0.0.1')`;
  const keeper = spawn(process.execPath, ["-e", code], { detached: true, stdio: "ignore", windowsHide: true });
  keeper.unref();
  const idle = new Date(Date.now() + IDLE_MINUTES * 60000).toLocaleTimeString();
  // The session clock starts when the sidecar does, a few minutes after creation.
  const latest = new Date(createdAt + MAX_SESSION_MINUTES * 60000).toLocaleTimeString();
  say("=== HANDED OVER: the GPU is still running and billing ===");
  say(`instance ${instanceId}: ${offerLine}`);
  say(`it destroys itself: ${IDLE_MINUTES} min after the last generation (about ${idle} if you do nothing), at the latest a few minutes after ${latest}, or 10 min after the app stops (PC asleep, app closed)`);
  say("to stop it: the app's Stop button, or https://cloud.vast.ai/instances/");
  say("the app and vite stay up (debug ports 9334 and 1420 are local only); then run: node dev/focus-live.mjs --restore-settings");
}

// ----- main ---------------------------------------------------------------------

const before = new Set((await ourInstances()).map((i) => i.id));
if (before.size) {
  say(`refusing to run: sloptweak instances already exist: ${[...before].join(", ")}`);
  process.exit(2);
}
if (!MOCK && existsSync(join(DATA_DIR, "active_instance.json"))) {
  say("refusing to run: an active_instance.json record exists");
  process.exit(2);
}
const creditBefore = await credit();
if (!MOCK) {
  say(`credit before: $${creditBefore?.toFixed(4)}`);
  if (!(creditBefore >= 3)) {
    say("refusing to run: the run sheet needs at least $3.00 of credit");
    process.exit(2);
  }
}

catalogServer = createServer((_req, res) => {
  res.setHeader("content-type", "application/json");
  res.end(readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8"));
}).listen(CATALOG_PORT, "127.0.0.1");

let viteSpawned = false;
if (!(await servesThisCheckout(APP).catch(() => false))) {
  viteProc = spawn("npx", ["vite", "--port", "1420", "--strictPort"], {
    cwd: APP, shell: true, windowsHide: true, detached: true, stdio: "ignore",
  });
  viteProc.unref();
  viteSpawned = true;
  await waitFor(() => servesThisCheckout(APP), 120000, "vite");
}

if (!MOCK) {
  mkdirSync(CONFIG_DIR, { recursive: true });
  if (!existsSync(backupFile)) writeFileSync(backupFile, existsSync(settingsFile) ? readFileSync(settingsFile, "utf8") : "");
  writeFileSync(
    settingsFile,
    JSON.stringify({
      heartbeat_minutes: HEARTBEAT_MINUTES, idle_minutes: IDLE_MINUTES, max_session_minutes: MAX_SESSION_MINUTES,
      max_dph: MAX_DPH, ready_timeout_minutes: READY_TIMEOUT, output_dir: join(OUT, "app-out"),
    }, null, 2),
  );
}

async function destroyOurs(why) {
  for (const i of await ourInstances()) {
    if (!before.has(i.id)) {
      say(`${why}: destroying instance ${i.id}`);
      await vast("DELETE", `/instances/${i.id}/`);
    }
  }
}
function stopLocal() {
  if (appPid) kill(appPid);
  if (viteSpawned && viteProc) kill(viteProc.pid);
}

hardCap = setTimeout(async () => {
  say(`HARD CAP (${HARD_CAP_MINUTES} min): destroying what this run created and stopping`);
  try {
    await destroyOurs("HARD CAP");
  } finally {
    stopLocal();
    if (!MOCK) restoreSettings();
    process.exit(3);
  }
}, HARD_CAP_MINUTES * 60000);

try {
  await live();
} catch (e) {
  check("live run completed", false, String(e));
} finally {
  const keepIt = KEEP && ready && (MOCK || (await ourInstances()).some((i) => !before.has(i.id)));
  if (keepIt) {
    await handOff();
  } else {
    clearTimeout(hardCap);
    catalogServer.close();
    stopLocal();
    await destroyOurs("SAFETY NET");
    if (!MOCK) restoreSettings();
    await sleep(MOCK ? 0 : 10000);
    const left = (await ourInstances()).filter((i) => !before.has(i.id));
    say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
  }
  if (!MOCK) {
    const creditAfter = await credit();
    say(`credit now: $${creditAfter?.toFixed(4)} (spent ~$${(creditBefore - creditAfter).toFixed(4)} so far${handedOff ? "; still billing" : "; late charges may post"})`);
  }
  const failed = results.filter((x) => !x.ok).length;
  say(`${results.length - failed}/${results.length} checks passed`);
  process.exit(failed ? 1 : 0);
}
