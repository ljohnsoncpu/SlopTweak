// Phase 6 live tuning: hold one real GPU session open for experiments.
// SPENDS MONEY: run only with approval.
//
// Starts the debug app in Vast mode with MODEL, waits for Ready, and writes
// <OUT>/session.json ({ instance_id, origin, remote_port }) so other scripts
// (dev/phase6-tune.mjs) can drive the Invoke window over CDP. It stops the
// session when <OUT>/STOP appears, after MINUTES, or when the estimated
// spend reaches CAP_USD, whichever comes first, and on Ctrl+C.
//
// Safety: refuses to run if sloptweak instances or an active_instance.json
// record exist. Prints every instance id as soon as it's seen. On every exit
// path it presses Stop, then destroys anything it created (safety net). If
// this script is killed outright, the app dies with it, the heartbeat stops,
// and the instance destroys itself after heartbeat_minutes (3).
// settings.json is backed up and restored. Keys come from HKCU\Environment
// and are never printed.
//
// Usage (from app/):  MODEL=banana-splitz-xxl MINUTES=70 CAP_USD=0.9 node dev/phase6-session.mjs
// Optional: MAX_DPH=0.7 (price limit, default 0.5), READY_TIMEOUT=30 (minutes; big models).

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT ?? join(APP, "..", ".dev", "phase6");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const MODEL = process.env.MODEL ?? "banana-splitz-xxl";
const MINUTES = Number(process.env.MINUTES ?? 70);
const CAP_USD = Number(process.env.CAP_USD ?? 0.9);
const MAX_DPH = Number(process.env.MAX_DPH ?? 0.5);
const LOCAL_CATALOG = process.env.LOCAL_CATALOG === "1";
const CATALOG_PORT = 18558;
const READY_TIMEOUT = Number(process.env.READY_TIMEOUT ?? 0);

mkdirSync(OUT, { recursive: true });
const LOG = join(OUT, "session.log");
const STOP = join(OUT, "STOP");
const STATE = join(OUT, "session.json");
rmSync(STOP, { force: true });
rmSync(STATE, { force: true });
const t0 = Date.now();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function say(msg) {
  const line = `+${Math.round((Date.now() - t0) / 1000)}s ${msg}`;
  console.log(line);
  appendFileSync(LOG, line + "\n");
}

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
const ourInstances = async () => ((await vast("GET", "/instances/")).body?.instances ?? []).filter((i) => (i.label ?? "").startsWith("sloptweak"));
const credit = async () => (await vast("GET", "/users/current")).body?.credit;

function kill(p) {
  try {
    execSync(`taskkill /T /F /PID ${p.pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
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
    eval: async (expression) => (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result?.result?.value,
  };
}

// ----- main -----

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
say(`credit before: $${creditBefore?.toFixed(4)}; model ${MODEL}; cap ${MINUTES} min / $${CAP_USD}`);
mkdirSync(CONFIG_DIR, { recursive: true });
const settingsFile = join(CONFIG_DIR, "settings.json");
// The backup lives on disk, so a run that is killed outright (no cleanup)
// still gets its settings back: the next start restores a leftover backup.
const backupFile = join(CONFIG_DIR, "settings.json.phase6-backup");
if (existsSync(backupFile)) {
  const old = JSON.parse(readFileSync(backupFile, "utf8"));
  if (old.content === null) rmSync(settingsFile, { force: true });
  else writeFileSync(settingsFile, old.content);
  rmSync(backupFile, { force: true });
  say("restored settings.json from a previous run's backup");
}
const settingsBackup = existsSync(settingsFile) ? readFileSync(settingsFile, "utf8") : null;
writeFileSync(backupFile, JSON.stringify({ content: settingsBackup }));
const outDir = mkdtempSync(join(tmpdir(), "sloptweak-phase6-out-"));
writeFileSync(
  settingsFile,
  JSON.stringify(
    { model_id: MODEL, output_dir: outDir, tutorial_done: true, max_dph: MAX_DPH, max_session_minutes: MINUTES + 5, heartbeat_minutes: 3, idle_minutes: 30, ...(READY_TIMEOUT ? { ready_timeout_minutes: READY_TIMEOUT } : {}) },
    null,
    2,
  ),
);

let app = null;
let vite = null;
let catalogServer = null;
let m = null;
let hourly = null;
let readyAt = null;
const created = new Set();
const note = (i) => {
  if (!before.has(i.id) && !created.has(i.id)) {
    created.add(i.id);
    say(`INSTANCE ${i.id} exists (${i.gpu_name}, $${(i.dph_total ?? 0).toFixed(4)}/hr)`);
  }
};
const tracker = setInterval(async () => {
  try {
    for (const i of await ourInstances()) note(i);
  } catch {
    /* transient */
  }
}, 15000);

let cleaning = null;
async function cleanup(why) {
  if (cleaning) return cleaning;
  cleaning = (async () => {
    say(`stopping: ${why}`);
    clearInterval(tracker);
    try {
      if (m) {
        await m.eval("document.getElementById('stop').click()");
        await waitFor(async () => (await m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot')")).state.kind === "idle", 4 * 60000, "idle after Stop");
        say("app says idle");
      }
    } catch (e) {
      say(`Stop via app failed: ${e}`);
    }
    if (app) kill(app);
    if (vite) kill(vite);
    catalogServer?.close();
    try {
      for (const i of await ourInstances()) {
        if (!before.has(i.id)) {
          say(`SAFETY NET: destroying leftover instance ${i.id}`);
          await vast("DELETE", `/instances/${i.id}/`);
        }
      }
    } catch (e) {
      say(`safety net failed: ${e} — CHECK VAST BY HAND`);
    }
    if (settingsBackup !== null) writeFileSync(settingsFile, settingsBackup);
    else rmSync(settingsFile, { force: true });
    rmSync(backupFile, { force: true });
    rmSync(STATE, { force: true });
    await sleep(10000);
    const left = (await ourInstances()).filter((i) => !before.has(i.id));
    say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
    const creditAfter = await credit();
    say(`credit after: $${creditAfter?.toFixed(4)} (spent ~$${(creditBefore - creditAfter).toFixed(4)}; late charges may post)`);
    say(`instances seen: ${[...created].join(", ") || "none"}; output: ${outDir}`);
  })();
  return cleaning;
}
for (const sig of ["SIGINT", "SIGTERM", "SIGBREAK"]) process.on(sig, () => cleanup(sig).then(() => process.exit(1)));

try {
  // LOCAL_CATALOG=1 serves this checkout's catalog/catalog.json to the app instead of main's.
catalogServer = LOCAL_CATALOG
  ? createServer((_req, res) => {
      res.setHeader("content-type", "application/json");
      res.end(readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8"));
    }).listen(CATALOG_PORT, "127.0.0.1")
  : null;
vite = spawn("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true, windowsHide: true, stdio: "ignore" });
  await waitFor(() => servesThisCheckout(APP), 30000, "vite");
  const appLog = join(OUT, "app.log");
  app = spawn(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_DEV_IMPORT_KEYS: "1",
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      ...(LOCAL_CATALOG ? { SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json` } : {}),
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  app.stdout.on("data", (d) => appendFileSync(appLog, d));
  app.stderr.on("data", (d) => appendFileSync(appLog, d));
  m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 120000, "UI");
  await waitFor(async () => (await m.eval("document.getElementById('start').disabled")) === false, 60000, "Start enabled");
  await m.eval("document.getElementById('start').click()");
  let last = "";
  const s = await waitFor(async () => {
    const st = (await m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot')")).state;
    const line = `${st.kind}${st.instance_id ? ` instance ${st.instance_id}` : ""}${st.offer ? ` offer ${st.offer.offer_id} ${st.offer.gpu_name} $${st.offer.hourly.toFixed(4)}/hr` : ""}${st.stage ? ` ${st.stage}` : ""}`;
    if (line !== last) say(`state: ${(last = line)}`);
    if (st.instance_id) note({ id: st.instance_id, gpu_name: st.offer?.gpu_name, dph_total: st.offer?.hourly });
    if (st.offer) hourly = st.offer.hourly;
    if (st.kind === "failed") throw new Error(st.reason);
    if ((Date.now() - t0) / 3600e3 * (hourly ?? 0.5) > CAP_USD) throw new Error("spend cap reached before Ready");
    return st.kind === "ready" ? st : false;
  }, 30 * 60000, "ready");
  readyAt = Date.now();
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 120000);
  const origin = await waitFor(async () => { const o = await r.eval("location.origin"); return o && o !== "null" ? o : false; }, 60000, "tunnel origin");
  r.ws.close();
  writeFileSync(STATE, JSON.stringify({ instance_id: s.instance_id, gpu: s.offer.gpu_name, hourly: s.offer.hourly, origin, remote_port: REMOTE_PORT, model: MODEL }, null, 2));
  say(`READY instance ${s.instance_id} (${s.offer.gpu_name}, ${s.offer.location}, $${s.offer.hourly.toFixed(4)}/hr) in ${Math.round((readyAt - t0) / 1000)}s; state in ${STATE}`);
  for (;;) {
    await sleep(5000);
    const mins = (Date.now() - t0) / 60000;
    const spent = (mins / 60) * (hourly ?? 0.5);
    if (existsSync(STOP)) throw new Error("STOP file");
    if (mins >= MINUTES) throw new Error(`time cap ${MINUTES} min`);
    if (spent >= CAP_USD) throw new Error(`spend cap $${CAP_USD}`);
    const kind = (await m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot')"))?.state?.kind;
    if (kind && kind !== "ready") throw new Error(`session left Ready: ${kind}`);
  }
} catch (e) {
  await cleanup(String(e));
} finally {
  await cleanup("exit");
}
