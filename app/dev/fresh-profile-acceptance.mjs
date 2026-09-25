// Phase 3 acceptance on real Vast. SPENDS MONEY: run only with approval.
//
// "A fresh Windows user profile goes from install to first image using only
// the wizard; keys survive app restart." Approximated by wiping this app's
// Credential Manager entries (service "SlopTweak"), its config and data
// folders (settings, catalog cache, webview profiles), then launching the
// debug build with no dev key import. A person pastes the keys into the
// wizard and generates the first image in the Invoke window; this script
// watches, presses Start and Stop, and checks everything.
//
// Safety: refuses to run if a SlopTweak instance or an active-instance
// record exists; destroys anything it created in `finally`. Keys are never
// read or printed; the Vast key for the safety net comes from
// HKCU\Environment and stays in this process.
//
// Usage (from app/):  node dev/fresh-profile-acceptance.mjs [--keep-profile]
//   --keep-profile  skip the wipe and the wizard; continue from the home
//                   screen with the keys a previous run's wizard stored.

import { spawn, execSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT ?? join(APP, "..", ".dev", "fresh-profile");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const WIZARD_MINUTES = 30;
const IMAGE_MINUTES = 30;
const KEEP = process.argv.includes("--keep-profile");

mkdirSync(OUT, { recursive: true });
const LOG = join(OUT, "acceptance.log");
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

// ----- Vast (verification + safety net only) ---------------------------------------

function userEnv(name) {
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
const ourInstances = async () =>
  ((await vast("GET", "/instances/")).body?.instances ?? []).filter((i) => (i.label ?? "").startsWith("sloptweak"));
const credit = async () => (await vast("GET", "/users/current")).body?.credit;
async function instanceGone(id) {
  const { status, body } = await vast("GET", `/instances/${id}/`);
  return status === 404 || !body?.instances;
}

// ----- fresh profile -----------------------------------------------------------------

/** Credential Manager targets that belong to the real (non-mock) app. */
function appCredentialTargets() {
  const out = execSync("cmdkey /list", { encoding: "utf8" });
  return [...out.matchAll(/Target:\s*(.+)/g)]
    .map((m) => m[1].trim())
    .filter((t) => /\.SlopTweak$/.test(t));
}

function wipeProfile() {
  const targets = appCredentialTargets();
  for (const t of targets) {
    for (const name of [t.replace(/^LegacyGeneric:target=/, ""), t]) {
      try {
        execSync(`cmdkey /delete:"${name}"`, { stdio: "ignore" });
        break;
      } catch {
        /* try the other spelling */
      }
    }
  }
  say(`wiped Credential Manager entries: ${targets.join(", ") || "none"}`);
  rmSync(CONFIG_DIR, { recursive: true, force: true });
  rmSync(DATA_DIR, { recursive: true, force: true });
  say(`wiped ${CONFIG_DIR} and ${DATA_DIR} (settings, catalog cache, webview profiles)`);
  check("fresh profile: no app credentials left", appCredentialTargets().length === 0);
}

// ----- app + CDP ----------------------------------------------------------------------

let app = null;
let n = 0;
function launch(tag) {
  const logFile = join(OUT, `app-${++n}-${tag}.log`);
  app = spawn(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "",
      SLOPTWEAK_DEV_IMPORT_KEYS: "",
      SLOPTWEAK_CATALOG_URL: "",
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  app.stdout.on("data", (d) => appendFileSync(logFile, d));
  app.stderr.on("data", (d) => appendFileSync(logFile, d));
  say(`app launched (${tag}), log ${logFile}`);
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
    // A dead app can't clean up; stop waiting so `finally` does.
    if (app && app.exitCode !== null) throw new Error(`the app exited (code ${app.exitCode})`);
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
    eval: async (expression) =>
      (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result?.result?.value,
    shot: async (name) => {
      const r = await send("Page.captureScreenshot", { format: "png" });
      writeFileSync(join(OUT, `${name}.png`), Buffer.from(r.result.data, "base64"));
      say(`screenshot ${name}.png`);
    },
  };
}

async function mainWindow() {
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 30000, "UI");
  await sleep(1500);
  return m;
}
const snap = (m) => m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot')");
const visible = (m, id) => m.eval(`!document.getElementById('${id}').hidden`);

// ----- the run -------------------------------------------------------------------------

async function run() {
  let m;
  if (KEEP) {
    say("--keep-profile: using the keys and settings from the previous run's wizard");
    launch("keep");
    m = await mainWindow();
  } else {
    wipeProfile();
    launch("fresh");
    m = await mainWindow();
    check("fresh profile opens the wizard", await visible(m, "view-wizard"));
    await m.shot("01-wizard");
    const s0 = await snap(m);
    check("no keys stored", !s0.has_vast_key && !s0.has_civitai_key);

    say(">>> Complete the setup wizard in the SlopTweak window now (paste both keys), then press");
    say(">>> 'Start using SlopTweak'. This script continues when the home screen appears.");
  }
  await waitFor(async () => (await visible(m, "view-home")) && (await snap(m)).has_civitai_key, WIZARD_MINUTES * 60000, "wizard done", 2000);
  const s1 = await snap(m);
  check("both keys stored", s1.has_vast_key && s1.has_civitai_key);
  await sleep(5000);
  check("no orphan banner; stale record cleared", !(await visible(m, "orphans")) && !existsSync(RECORD));
  await waitFor(async () => (await snap(m)).catalog.source === "online", 30000, "online catalog").catch(() => {});
  const s1b = await snap(m);
  check("catalog came from GitHub", s1b.catalog.source === "online", `${s1b.catalog.source}, ${s1b.models.length} model(s)`);
  await waitFor(async () => (await m.eval("document.getElementById('estimate').textContent")).includes("Cheapest"), 60000, "estimate");
  say(`home: ${await m.eval("document.getElementById('estimate').textContent")}`);
  await m.shot("02-home");

  // Phase 3 LoRA path end to end: real CivitAI metadata, download, register.
  if (!s1b.settings.loras.length) {
    const lora = await m.eval(
      "window.__TAURI_INTERNALS__.invoke('add_lora', {link: 'https://civitai.com/models/122359/detail-tweaker-xl'}).then(v => v.loras.map(l => l.name + ' ' + l.file.size_bytes), e => 'ERR ' + e)",
    );
    check("LoRA added from a CivitAI link", Array.isArray(lora) && lora.length === 1, JSON.stringify(lora));
  } else {
    say(`LoRA already saved: ${s1b.settings.loras.map((l) => l.name).join(", ")}`);
  }
  await m.eval("document.getElementById('nav-settings').click(); document.getElementById('nav-home').click()");
  await sleep(3000);

  const before = await credit();
  say(`credit before Start: $${before?.toFixed(4)}`);
  await m.eval("document.getElementById('start').click()");
  let lastLine = "";
  const ready = await waitFor(async () => {
    const s = (await snap(m)).state;
    const line = `${s.kind}${s.instance_id ? ` instance ${s.instance_id}` : ""}${s.offer ? ` ${s.offer.gpu_name} offer ${s.offer.offer_id} $${s.offer.hourly.toFixed(4)}/hr dl $${s.offer.download_cost.toFixed(3)}` : ""}${s.stage ? ` ${s.stage}` : ""}${s.progress != null ? ` ${Math.floor(s.progress)}%` : ""}`;
    if (line !== lastLine) say(`state: ${(lastLine = line)}`);
    if (s.kind === "failed") throw new Error(s.reason);
    return s.kind === "ready" ? s : false;
  }, 50 * 60000, "ready", 3000);
  say(`READY: instance ${ready.instance_id}, ${ready.offer.gpu_name}, $${ready.offer.hourly.toFixed(4)}/hr`);

  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 120000);
  await waitFor(async () => (await r.eval("document.title")).includes("Invoke"), 120000, "Invoke UI");
  check("Invoke opens in the app window", true);
  const models = await r.eval("fetch('/api/v2/models/').then(r => r.json()).then(j => (j.models || []).map(m => m.type + ':' + m.name))");
  check("model and LoRA registered in Invoke", Array.isArray(models) && models.some((x) => x.startsWith("main:")) && models.some((x) => x.startsWith("lora:")), JSON.stringify(models));
  const count = () =>
    r.eval("fetch('/api/v1/images/?order_dir=DESC&starred_first=false&is_intermediate=false&limit=5').then(r => r.json()).then(j => j.total)");
  const startCount = (await count()) ?? 0;
  say(">>> In the Invoke window, type a prompt and press Invoke to make the first image.");
  const total = await waitFor(async () => {
    const t = await count();
    return t > startCount ? t : false;
  }, IMAGE_MINUTES * 60000, "first image", 5000);
  check("first image generated", total > startCount, `${total} image(s)`);
  await sleep(3000);
  await r.shot("03-first-image");
  await m.shot("04-costbar");
  const bar = await m.eval("document.getElementById('costbar').textContent");
  check("cost bar shows $/hr, time, spend, credit", /\/hr · .* so far · \$[\d.]+ left/.test(bar), bar);
  const title = await r.eval("document.title");
  say(`Invoke window page title: ${title}`);
  r.ws.close();

  await m.eval("document.getElementById('stop').click()");
  await waitFor(async () => (await snap(m)).state.kind === "idle", 5 * 60000, "stopped", 2000);
  check("Stop destroys the instance", await instanceGone(ready.instance_id), `instance ${ready.instance_id}`);
  m.ws.close();
  killApp();
  await sleep(2000);

  launch("restart");
  m = await mainWindow();
  const s2 = await snap(m);
  check("keys survive an app restart", s2.has_vast_key && s2.has_civitai_key && (await visible(m, "view-home")) && !(await visible(m, "view-wizard")));
  await m.shot("05-restart");
  m.ws.close();
  killApp();
}

if ((await ourInstances()).length) {
  say("refusing to run: SlopTweak instances already exist");
  process.exit(2);
}
const RECORD = join(DATA_DIR, "active_instance.json");
if (existsSync(RECORD)) {
  const rec = JSON.parse(readFileSync(RECORD, "utf8"));
  if (!(await instanceGone(rec.instance_id))) {
    say(`refusing to run: recorded instance ${rec.instance_id} still exists`);
    process.exit(2);
  }
  say(`stale record for instance ${rec.instance_id} (already gone); the app should clear it`);
}
const creditStart = await credit();
say(`credit at start: $${creditStart?.toFixed(4)}`);
const vite = spawn("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true, windowsHide: true, stdio: "ignore" });
await waitFor(() => servesThisCheckout(APP), 30000, "vite");
const seen = new Set();
const tracker = setInterval(async () => {
  try {
    for (const i of await ourInstances()) {
      if (!seen.has(i.id)) {
        seen.add(i.id);
        say(`[tracker] instance ${i.id} exists (${i.gpu_name}, $${(i.dph_total ?? 0).toFixed(4)}/hr)`);
      }
    }
  } catch {
    /* transient */
  }
}, 20000);

try {
  await run();
} catch (e) {
  check("run completed", false, String(e));
} finally {
  clearInterval(tracker);
  killApp();
  try {
    execSync(`taskkill /T /F /PID ${vite.pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
  for (const i of await ourInstances()) {
    say(`SAFETY NET: destroying leftover instance ${i.id}`);
    await vast("DELETE", `/instances/${i.id}/`);
  }
  await sleep(10000);
  const left = await ourInstances();
  say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
  const creditEnd = await credit();
  say(`credit after: $${creditEnd?.toFixed(4)} (spent ~$${(creditStart - creditEnd).toFixed(4)}; late charges may post)`);
  say(`instances seen: ${[...seen].join(", ") || "none"}`);
  const failed = results.filter((r) => !r.ok).length;
  say(`${results.length - failed}/${results.length} checks passed`);
}
