// Phase 2 acceptance against real Vast. SPENDS MONEY: run only with approval.
//
//   R1  Start -> Invoke visible in the app window -> Stop destroys
//   R2  Start -> crash -> relaunch finds orphan -> Reconnect -> crash ->
//       relaunch -> "Shut it down" destroys
//   R3  Start -> crash, no relaunch -> instance watchdog destroys ->
//       relaunch clears the stale record
//
// Safety: every instance created while this runs is destroyed in `finally`
// (anything labelled sloptweak* that wasn't there before). Instance ids are
// printed as soon as they're seen. Keys are read from HKCU\Environment and
// never printed.
//
// Usage (from app/):  node dev/vast-acceptance.mjs [r1] [r2] [r3]

import { spawn, execSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync, existsSync, appendFileSync } from "node:fs";
import { join, resolve } from "node:path";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT ?? join(APP, "..", ".dev", "acceptance");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const HEARTBEAT_MINUTES = 3;
const runs = process.argv.slice(2).length ? process.argv.slice(2) : ["r1", "r2", "r3"];

mkdirSync(OUT, { recursive: true });
const LOG = join(OUT, "acceptance.log");
const t0 = Date.now();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ts = () => `+${Math.round((Date.now() - t0) / 1000)}s`;
function say(msg) {
  const line = `${ts()} ${msg}`;
  console.log(line);
  appendFileSync(LOG, line + "\n");
}
const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok });
  say(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
}

// ----- Vast (for verification and the safety net only) --------------------

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
async function credit() {
  return (await vast("GET", "/users/current")).body?.credit;
}
async function instanceGone(id) {
  const { status, body } = await vast("GET", `/instances/${id}/`);
  return status === 404 || !body?.instances;
}

// ----- app + CDP ------------------------------------------------------------

let app = null;
let vite = null;
let appLogN = 0;

function launch(tag) {
  const logFile = join(OUT, `app-${++appLogN}-${tag}.log`);
  const p = spawn(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_DEV_IMPORT_KEYS: "1",
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  p.stdout.on("data", (d) => appendFileSync(logFile, d));
  p.stderr.on("data", (d) => appendFileSync(logFile, d));
  app = p;
  say(`app launched (${tag}), log ${logFile}`);
  return p;
}

function crash() {
  say(`CRASH: killing app pid ${app.pid}`);
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
    send,
    eval: async (expression) =>
      (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }))
        .result?.result?.value,
    shot: async (name) => {
      const r = await send("Page.captureScreenshot", { format: "png" });
      const file = join(OUT, `${name}.png`);
      writeFileSync(file, Buffer.from(r.result.data, "base64"));
      say(`screenshot ${file}`);
    },
  };
}

async function mainWindow() {
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 30000, "UI");
  return m;
}

const snapshot = (m) => m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot').then(s => s.state)");

/// Follow the session until pred(state) or failure. Prints transitions.
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
      (s.offer ? ` offer ${s.offer.offer_id} ${s.offer.gpu_name} $${s.offer.hourly.toFixed(4)}/hr dl $${s.offer.download_cost.toFixed(3)}` : "") +
      (s.stage ? ` stage ${s.stage}` : "") +
      (s.progress != null ? ` ${Math.floor(s.progress)}%` : "") +
      (s.reason ? ` reason: ${s.reason}` : "") +
      (s.notice ? ` notice: ${s.notice}` : "");
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

async function startAndWaitReady(m, tag) {
  await m.eval("document.getElementById('start').click()");
  const s = await follow(m, (s) => s.kind === "ready", 30 * 60000, `${tag} ready`);
  say(`${tag}: READY instance ${s.instance_id} on offer ${s.offer.offer_id} (${s.offer.gpu_name}, $${s.offer.hourly.toFixed(4)}/hr)`);
  return s;
}

async function invokeVisible(tag) {
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 90000);
  await waitFor(async () => (await r.eval("document.title")).includes("Invoke"), 90000, "Invoke UI");
  await sleep(8000);
  await r.shot(`${tag}-invoke`);
  const title = await r.eval("document.title");
  const origin = await r.eval("location.origin");
  const ipc = await r.eval(`(async () => { try { await window.__TAURI_INTERNALS__.invoke('get_snapshot'); return 'ALLOWED'; } catch (e) { return 'denied'; } })()`);
  const models = await r.eval("fetch('/api/v2/models/').then(r => r.json()).then(j => (j.models || []).map(m => m.name))");
  r.ws.close();
  check(`${tag}: Invoke visible in app window`, title.includes("Invoke"), `${title} @ ${origin}`);
  check(`${tag}: remote window IPC denied`, ipc === "denied");
  check(`${tag}: catalog model registered`, Array.isArray(models) && models.length > 0, JSON.stringify(models));
}

async function orphanBanner(m, id) {
  return waitFor(async () => {
    const txt = await m.eval("document.getElementById('orphans').hidden ? '' : document.getElementById('orphans').textContent");
    return txt && txt.includes(`#${id}`) ? txt : false;
  }, 60000, "orphan banner");
}

async function clickOrphan(m, label) {
  return m.eval(`(() => { const b = [...document.querySelectorAll('#orphans button')].find(b => b.textContent === ${JSON.stringify(label)}); if (!b) return false; b.click(); return true; })()`);
}

async function waitGone(id, ms, what) {
  const start = Date.now();
  await waitFor(() => instanceGone(id), ms, what);
  return Math.round((Date.now() - start) / 1000);
}

// ----- runs -----------------------------------------------------------------

async function r1() {
  say("=== R1: start, use, stop ===");
  launch("r1");
  const m = await mainWindow();
  const s = await startAndWaitReady(m, "R1");
  await invokeVisible("R1");
  await m.eval("document.getElementById('stop').click()");
  await follow(m, (s) => s.kind === "idle", 5 * 60000, "R1 idle");
  check("R1: Stop destroys the instance", await instanceGone(s.instance_id), `instance ${s.instance_id}`);
  check("R1: record cleared", !existsSync(join(DATA_DIR, "active_instance.json")));
  m.ws.close();
  crash(); // idle; just close it
}

async function r2() {
  say("=== R2: crash, relaunch, reconnect, crash, relaunch, shut down ===");
  launch("r2a");
  let m = await mainWindow();
  const s = await startAndWaitReady(m, "R2");
  const id = s.instance_id;
  m.ws.close();
  crash();

  launch("r2b");
  m = await mainWindow();
  const banner = await orphanBanner(m, id);
  check("R2: relaunch detects the orphan", true, banner.slice(0, 120));
  check("R2: orphan offers Reconnect", banner.includes("Reconnect"));
  await clickOrphan(m, "Reconnect");
  await follow(m, (s) => s.kind === "ready" && s.instance_id === id, 10 * 60000, "R2 reattached");
  await invokeVisible("R2-reattached");
  m.ws.close();
  crash();

  launch("r2c");
  m = await mainWindow();
  await orphanBanner(m, id);
  await clickOrphan(m, "Shut it down");
  await follow(m, (s) => s.kind === "idle", 5 * 60000, "R2 destroyed");
  const gone = await instanceGone(id);
  check("R2: relaunch cleans up the orphan", gone, `instance ${id}`);
  const hidden = await waitFor(async () => m.eval("document.getElementById('orphans').hidden"), 30000, "banner cleared").catch(() => false);
  check("R2: banner cleared", !!hidden);
  m.ws.close();
  crash();
}

async function r3() {
  say("=== R3: crash, watchdog destroys ===");
  launch("r3a");
  let m = await mainWindow();
  const s = await startAndWaitReady(m, "R3");
  const id = s.instance_id;
  m.ws.close();
  crash();
  const killedAt = Date.now();
  say(`waiting for the instance watchdog (heartbeat timeout ${HEARTBEAT_MINUTES} min)`);
  await waitFor(() => instanceGone(id), (HEARTBEAT_MINUTES + 5) * 60000, "watchdog destroy");
  const secs = Math.round((Date.now() - killedAt) / 1000);
  check("R3: watchdog destroyed the instance after the crash", secs <= (HEARTBEAT_MINUTES + 2) * 60, `${secs}s after kill`);

  launch("r3b");
  m = await mainWindow();
  await sleep(8000);
  const shown = await m.eval("!document.getElementById('orphans').hidden");
  check("R3: relaunch shows no orphan", !shown);
  check("R3: stale record cleared", !existsSync(join(DATA_DIR, "active_instance.json")));
  m.ws.close();
  crash();
}

// ----- main -----------------------------------------------------------------

const before = new Set((await ourInstances()).map((i) => i.id));
if (before.size) {
  say(`refusing to run: sloptweak instances already exist: ${[...before].join(", ")}`);
  process.exit(2);
}
const creditBefore = await credit();
say(`credit before: $${creditBefore?.toFixed(4)}`);
mkdirSync(CONFIG_DIR, { recursive: true });
const settingsFile = join(CONFIG_DIR, "settings.json");
writeFileSync(
  settingsFile,
  JSON.stringify({ heartbeat_minutes: HEARTBEAT_MINUTES, max_session_minutes: 60, max_dph: 0.5 }, null, 2),
);
if (existsSync(join(DATA_DIR, "active_instance.json"))) {
  say("refusing to run: an active_instance.json record exists");
  process.exit(2);
}

vite = spawn("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true, windowsHide: true, stdio: "ignore" });
await waitFor(async () => (await fetch("http://127.0.0.1:1420/")).ok, 30000, "vite");

const created = new Set();
const tracker = setInterval(async () => {
  try {
    for (const i of await ourInstances()) {
      if (!before.has(i.id) && !created.has(i.id)) {
        created.add(i.id);
        say(`[tracker] instance ${i.id} exists (${i.gpu_name}, $${(i.dph_total ?? 0).toFixed(4)}/hr)`);
      }
    }
  } catch {
    /* transient */
  }
}, 20000);

try {
  for (const r of runs) {
    try {
      await { r1, r2, r3 }[r]();
    } catch (e) {
      check(`${r} completed`, false, String(e));
      if (app) crash();
      say("stopping after the first failed run so it doesn't spend more");
      break;
    }
  }
} finally {
  clearInterval(tracker);
  if (app) crash();
  try {
    execSync(`taskkill /T /F /PID ${vite.pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
  // Safety net: destroy anything of ours created during the run.
  for (const i of await ourInstances()) {
    if (!before.has(i.id)) {
      say(`SAFETY NET: destroying leftover instance ${i.id}`);
      await vast("DELETE", `/instances/${i.id}/`);
    }
  }
  rmSync(settingsFile, { force: true });
  await sleep(10000);
  const left = (await ourInstances()).filter((i) => !before.has(i.id));
  say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
  const creditAfter = await credit();
  say(`credit after: $${creditAfter?.toFixed(4)} (spent ~$${(creditBefore - creditAfter).toFixed(4)}; late charges may post)`);
  say(`instances seen: ${[...created].join(", ") || "none"}`);
  const failed = results.filter((r) => !r.ok).length;
  say(`${results.length - failed}/${results.length} checks passed`);
}
