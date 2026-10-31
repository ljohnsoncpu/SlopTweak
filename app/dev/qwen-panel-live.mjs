// Qwen-Image 2.1 live test of the Identity Edit panel path (new/edit, many references,
// focus, strength, painted area). Based on qwen-spike.mjs. SPENDS MONEY:
// run only with approval (GPU 24 GB, about $0.20-0.30/hr, total cap about $0.50).
// Drives the debug app with a DRAFT catalog entry (added at runtime; the repo's
// catalog.json is not changed), then runs graphs in the app's own ComfyUI window.
// Provisioning is the existing ComfyUI path (the Identity Edit nodes get installed
// and probed but are unused); models are placed by `dest`.
//
// Usage (from app/):  REF1=a.png REF2=b.png REF3=c.png OUT=<scratch dir> node dev/qwen-panel-live.mjs
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
const REF3 = process.env.REF3;
if (!OUT || !REF1 || !REF2 || !REF3) throw new Error("set OUT, REF1, REF2 and REF3");
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
const MODEL_ID = "qwen-image-edit";

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

/** PNG width and height from its header. */
function pngSize(file) {
  const b = readFileSync(file);
  return [b.readUInt32BE(16), b.readUInt32BE(20)];
}

const P_TEAM = "<image1>, <image2> and <image3> pose together as a team in a sunlit forest clearing, full body group illustration. Keep each character's face, fur colors and outfit exactly as in their reference.";
const P_FACE = "A close portrait of <image1> smiling, soft evening light, illustration. Keep the character's face and fur colors exactly as in the reference.";
const P_HOODIE = "Change <image1>'s armor into a plain red hoodie. Keep the character, pose, background and everything else exactly the same.";
const P_PAIR_EDIT = "<image2> stands next to <image1> in the same scene. Keep <image1> unchanged and keep <image2>'s face, fur colors and outfit exactly as in its reference.";

async function spike() {
  say("=== Qwen panel live test ===");
  launch();
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 120000, "UI");
  const picked = await m.eval(`(() => { const s = document.getElementById('model');
    if (![...s.options].some(o => o.value === '${MODEL_ID}')) return false;
    s.value = '${MODEL_ID}'; s.dispatchEvent(new Event('change')); return true; })()`);
  if (!picked) throw new Error(`${MODEL_ID} is not in the catalog`);
  await sleep(3000);
  const startState = await m.eval(`JSON.stringify({ disabled: document.getElementById('start').disabled, text: document.body.innerText.slice(0, 1800) })`);
  say(`start button before click: ${startState.slice(0, 200)}`);
  if (JSON.parse(startState).disabled) throw new Error("Start is disabled; not spending anything");
  await m.eval("document.getElementById('start').click()");
  const tStart = Date.now();
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

  await waitFor(() => m.eval("!document.getElementById('identity').hidden"), 60000, "identity panel");
  check("the Identity Edit panel opens for Qwen", true);
  check("no Krea pull dial", await m.eval("document.getElementById('id-pull-box').hidden"));
  const refs = { fox: REF1, tiger: REF2, wolf: REF3 };
  const [fw, fh] = pngSize(REF1);
  say(`base/fox size ${fw}x${fh}`);

  const invoke = (cmd, args = {}) =>
    m.eval(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}).then(() => 'ok').catch((e) => 'ERR ' + String(e))`);
  const setRef = (slot, key) => invoke("identity_set_ref_path", { slot, path: refs[key] });
  const clearAll = async () => {
    for (let slot = 0; slot <= 8; slot++) await invoke("identity_clear_ref", { slot });
  };
  const view = () => m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then((v) => JSON.stringify({ phase: v.phase, message: v.message, n: v.results.length, refs: v.refs.map((r) => !!r) }))").then(JSON.parse);
  const summary = [];

  async function run(label, note, args, setup) {
    say(`run ${label}: ${note}`);
    await clearAll();
    await setup();
    const before = (await view()).n;
    const t = Date.now();
    const req = { aspect: "square", mode: "new", strength: null, pull: 1, mask: null, focus: [], ...args };
    const r = await invoke("identity_generate", req);
    if (r !== "ok") {
      say(`  -> REFUSED: ${r}`);
      summary.push({ label, note, error: r });
      return;
    }
    let v;
    for (let i = 0; i < 300; i++) {
      await sleep(1500);
      v = await view();
      if (v.phase !== "uploading" && v.phase !== "running") break;
    }
    const secs = Math.round((Date.now() - t) / 1000);
    if (v.phase === "done" && v.n > before) {
      const png = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then((v) => v.results[0].data_url)");
      const ext = png.startsWith("data:image/png") ? "png" : png.startsWith("data:image/webp") ? "webp" : "jpg";
      writeFileSync(join(OUT, `${label}.${ext}`), Buffer.from(png.split(",")[1], "base64"));
      say(`  -> ${label}.${ext} in ${secs}s`);
      summary.push({ label, note, seconds: secs });
    } else {
      say(`  -> FAILED (${v.phase}): ${v.message}`);
      summary.push({ label, note, error: `${v.phase}: ${v.message}` });
    }
    check(`${label}: ${note}`, v.phase === "done" && v.n > before, `${secs}s`);
  }

  // R1 goes through the panel's own controls (Make image), with a gap in the slots.
  say("run R1-three-refs-via-panel: slots 0, 3 and 4 (slots 1 and 2 empty), landscape");
  await clearAll();
  await setRef(0, "fox");
  await setRef(3, "tiger");
  await setRef(4, "wolf");
  await sleep(500);
  const tags = await m.eval("JSON.stringify([0, 3, 4].map((s) => document.getElementById('ref-tag-' + s).textContent))");
  say(`tags: ${tags}`);
  check("the panel numbers the three pictures <image1>..<image3>", tags === JSON.stringify(["Call it <image1> in your words", "Call it <image2> in your words", "Call it <image3> in your words"]));
  await m.shot("panel-three-refs");
  await m.eval(`(() => { const p = document.getElementById('id-prompt'); p.value = ${JSON.stringify(P_TEAM)}; p.dispatchEvent(new Event('input'));
    const a = document.getElementById('id-aspect'); a.value = 'landscape'; a.dispatchEvent(new Event('change')); })()`);
  await sleep(300);
  const t1 = Date.now();
  await m.eval("document.getElementById('id-go').click()");
  let v1;
  for (let i = 0; i < 300; i++) {
    await sleep(1500);
    v1 = await view();
    if (v1.phase === "done" || v1.phase === "failed" || v1.phase === "cancelled") break;
  }
  const secs1 = Math.round((Date.now() - t1) / 1000);
  if (v1.phase === "done") {
    const png = await m.eval("window.__TAURI_INTERNALS__.invoke('identity_state').then((v) => v.results[0].data_url)");
    writeFileSync(join(OUT, "R1-three-refs-via-panel.png"), Buffer.from(png.split(",")[1], "base64"));
    say(`  -> R1-three-refs-via-panel.png in ${secs1}s`);
    summary.push({ label: "R1", note: "3 refs via the panel", seconds: secs1 });
  } else {
    say(`  -> FAILED (${v1.phase}): ${v1.message}`);
    summary.push({ label: "R1", note: "3 refs via the panel", error: `${v1.phase}: ${v1.message}` });
  }
  check("R1: three references through the Make image button", v1.phase === "done", `${secs1}s`);

  await run("R2-focus-face", "new, reference 1 cropped to the face", {
    prompt: P_FACE,
    focus: [{ slot: 0, bbox: { x: Math.round(fw * 0.28), y: Math.round(fh * 0.02), w: Math.round(fw * 0.42), h: Math.round(fh * 0.25) } }],
  }, async () => setRef(0, "fox"));
  await run("R3-edit-plain", "edit, picture only, full strength", { prompt: P_HOODIE, mode: "edit" }, async () => setRef(2, "fox"));
  await run("R4-edit-with-reference", "edit, picture plus a second reference", { prompt: P_PAIR_EDIT, mode: "edit" }, async () => {
    await setRef(2, "fox");
    await setRef(0, "tiger");
  });
  await run("R5-edit-strength-70", "edit at 70% strength", { prompt: P_HOODIE, mode: "edit", strength: 70 }, async () => setRef(2, "fox"));

  // The painted area: a rectangle over the torso, made in the page as the editor would.
  const bbox = { x: Math.round(fw * 0.3), y: Math.round(fh * 0.24), w: Math.round(fw * 0.42), h: Math.round(fh * 0.3) };
  const maskPng = await m.eval(`(async () => { const c = document.createElement('canvas'); c.width = ${fw}; c.height = ${fh};
    const g = c.getContext('2d'); g.fillStyle = '#000'; g.fillRect(0, 0, c.width, c.height); g.fillStyle = '#fff';
    g.fillRect(${bbox.x}, ${bbox.y}, ${bbox.w}, ${bbox.h});
    const blob = await new Promise((r) => c.toBlob(r, 'image/png')); const buf = new Uint8Array(await blob.arrayBuffer());
    let s = ''; for (let i = 0; i < buf.length; i += 0x8000) s += String.fromCharCode(...buf.subarray(i, i + 0x8000)); return btoa(s); })()`);
  await run("R6-edit-masked", "edit with a painted area, no zoom", {
    prompt: P_HOODIE, mode: "edit", mask: { png: maskPng, width: fw, height: fh, bbox, zoom: false },
  }, async () => setRef(2, "fox"));
  await run("R7-edit-masked-zoom", "edit with a painted area, zoomed in", {
    prompt: P_HOODIE, mode: "edit", mask: { png: maskPng, width: fw, height: fh, bbox, zoom: true },
  }, async () => setRef(2, "fox"));

  writeFileSync(join(OUT, "summary.json"), JSON.stringify(summary, null, 2));
  const stats = await m.eval("'n/a'");
  say(`summary: ${JSON.stringify(summary.map((x) => [x.label, x.seconds ?? x.error]))} ${stats}`);
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
