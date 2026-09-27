// Templates/workflows + tutorial overlay against real Vast. SPENDS MONEY: run
// only with approval.
//
//   GPU 1: clean library -> Start -> the model's built-in template (with its
//   picture) is in real Invoke 6.14.1 when the window opens; the tutorial
//   opens centered, its ring finds real Invoke buttons, the docs link reaches
//   the app; a template and a workflow are made through Invoke's API (as its
//   UI does) -> Stop -> both are in library.json.
//   GPU 2: Start -> they're back in the fresh Invoke, once -> Stop.
//
// Safety: refuses to run if sloptweak instances exist; every instance created
// is destroyed in `finally`; ids are printed as seen. Keys come from
// HKCU\Environment and are never printed. settings.json and library.json are
// backed up and restored. The catalog is served from this checkout.
//
// Usage (from app/):  node dev/library-acceptance.mjs
//   TEST_MODEL=<catalog id>  (default anima-aesthetic)

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT ?? join(APP, "..", ".dev", "library");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const CATALOG_PORT = 18556;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const MODEL = process.env.TEST_MODEL ?? "anima-aesthetic";
const TEMPLATE = "Live test template";
const WORKFLOW = "Live test workflow";

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

// ----- Vast (verification and the safety net only) --------------------------

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
async function instanceGone(id) {
  const { status, body } = await vast("GET", `/instances/${id}/`);
  return status === 404 || !body?.instances;
}

// ----- app + CDP ----------------------------------------------------------------

let app = null;
let vite = null;
const appLog = join(OUT, "app.log");

function launch() {
  const p = spawn(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_DEV_IMPORT_KEYS: "1",
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  p.stdout.on("data", (d) => appendFileSync(appLog, d));
  p.stderr.on("data", (d) => appendFileSync(appLog, d));
  app = p;
  say(`app launched, log ${appLog}`);
}

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
    eval: async (expression) =>
      (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result?.result?.value,
    shot: async (name) => {
      const r = await send("Page.captureScreenshot", { format: "png" });
      const file = join(OUT, `${name}.png`);
      writeFileSync(file, Buffer.from(r.result.data, "base64"));
      say(`screenshot ${file}`);
    },
  };
}

const snapshot = (m) => m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot')");
const TUT = "document.querySelector('sloptweak-tutorial')?.shadowRoot";
const cardText = (r) => r.eval(`${TUT}?.querySelector('.card, .pill')?.textContent ?? ''`);
const clickAct = (r, act, extra = "") =>
  r.eval(`(() => { const b = ${TUT}?.querySelector('[data-act="${act}"]${extra}'); if (!b) return false; b.click(); return true; })()`);
const ring = (r) => r.eval(`(() => { const g = ${TUT}?.querySelector('.ring'); return g ? g.style.display === 'block' : null; })()`);
const api = (r, path) => r.eval(`fetch(${JSON.stringify(path)}).then(r => r.json())`);

/** Start a session and wait for Ready; returns the Ready state. */
async function startSession(m) {
  await waitFor(async () => (await m.eval("document.getElementById('start').disabled")) === false, 60000, "Start enabled");
  await m.eval("document.getElementById('start').click()");
  let last = "";
  const s = await waitFor(async () => {
    const st = (await snapshot(m)).state;
    const line = `${st.kind}${st.instance_id ? ` instance ${st.instance_id}` : ""}${st.offer ? ` offer ${st.offer.offer_id} ${st.offer.gpu_name} $${st.offer.hourly.toFixed(4)}/hr` : ""}${st.stage ? ` ${st.stage}` : ""}`;
    if (line !== last) say(`state: ${(last = line)}`);
    if (st.kind === "failed") throw new Error(st.reason);
    return st.kind === "ready" ? st : false;
  }, 30 * 60000, "ready");
  say(`READY instance ${s.instance_id} (${s.offer.gpu_name}, ${s.offer.location}, $${s.offer.hourly.toFixed(4)}/hr)`);
  return s;
}

async function stopSession(m, instanceId) {
  await m.eval("document.getElementById('stop').click()");
  await waitFor(async () => (await snapshot(m)).state.kind === "idle", 6 * 60000, "idle after Stop");
  check(`Stop destroyed instance ${instanceId}`, await waitFor(() => instanceGone(instanceId), 60000, "gone").catch(() => false));
}

async function invokePage() {
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 120000);
  await waitFor(async () => (await r.eval("document.title")).includes("Invoke"), 120000, "Invoke UI");
  return r;
}

const presetNames = async (r) => ((await api(r, "/api/v1/style_presets/")) ?? []).filter((p) => p.type === "user");

// ----- GPU 1 ---------------------------------------------------------------------

async function gpu1(m, builtinNames) {
  const s = await startSession(m);
  const r = await invokePage();

  // Built-ins are in before the window opened.
  const presets = await presetNames(r);
  const got = presets.filter((p) => builtinNames.includes(p.name));
  check("built-in templates are in real Invoke when its window opens", got.length === builtinNames.length, got.map((p) => p.name).join(", "));
  const img = got[0]?.image
    ? await r.eval(`fetch(${JSON.stringify("/api/v1/style_presets/i/" + got[0].id + "/image")}).then(async r => [r.status, r.headers.get('content-type'), (await r.arrayBuffer()).byteLength])`)
    : null;
  check("Invoke accepted the template's picture", !!img && img[0] === 200 && img[2] > 1000, JSON.stringify(img));
  check("the template keeps its {prompt} slot", got.every((p) => p.preset_data.positive_prompt.includes("{prompt}")));

  // Tutorial overlay on real Invoke.
  await sleep(6000);
  const centered = await r.eval(`${TUT}?.querySelector('.card')?.classList.contains('center') ?? false`);
  check("tutorial welcome opens centered", centered === true);
  await r.shot("l1-welcome-centered");
  await clickAct(r, "next");
  await waitFor(async () => (await cardText(r)).includes("Open the sample picture"), 90000, "sample step");
  await sleep(5000);
  check("ring finds real Invoke's Assets tab", (await waitFor(() => ring(r), 10000, "ring").catch(() => false)) === true);
  const box = await r.eval(`(() => { const b = ${TUT}.querySelector('.card').getBoundingClientRect(); return [Math.round(innerWidth - b.right), Math.round(innerHeight - b.bottom)]; })()`);
  check("step card sits bottom-right", Array.isArray(box) && box[0] <= 20 && box[1] <= 20, JSON.stringify(box));
  await r.shot("l2-assets-ring");
  await clickAct(r, "next"); // mask step
  await clickAct(r, "next"); // prompt step
  await waitFor(async () => (await cardText(r)).includes("Say what goes there"), 5000, "prompt step");
  check("ring finds real Invoke's Invoke button", (await waitFor(() => ring(r), 10000, "ring").catch(() => false)) === true);
  await r.shot("l3-invoke-ring");
  await clickAct(r, "next"); // accept step
  await clickAct(r, "next"); // docs step
  await waitFor(async () => (await cardText(r)).includes("Where to go next"), 5000, "docs step");
  await r.shot("l4-docs-step");
  const logBefore = existsSync(appLog) ? readFileSync(appLog, "utf8").length : 0;
  await clickAct(r, "doc", '[data-doc="home"]');
  const opened = await waitFor(
    () => readFileSync(appLog, "utf8").slice(logBefore).includes('tutorial Docs("https://invoke.ai/")'),
    10000,
    "docs signal",
  ).catch(() => false);
  check("docs link reaches the app (opens invoke.ai in the browser)", opened === true);
  check("docs link leaves the Invoke page where it was", (await r.eval("document.title")).includes("Invoke"));
  await clickAct(r, "next"); // Done

  // A template and a workflow, made the way Invoke's UI makes them.
  const made = await r.eval(`(async () => {
    const fd = new FormData();
    fd.append('data', JSON.stringify({ name: ${JSON.stringify(TEMPLATE)}, positive_prompt: 'watercolor, {prompt}', negative_prompt: 'photo', type: 'user' }));
    const p = await fetch('/api/v1/style_presets/', { method: 'POST', body: fd });
    const list = await (await fetch('/api/v1/workflows/?categories=default&per_page=5')).json();
    const src = list.items?.[0];
    if (!src) return [p.status, 'no default workflow'];
    const wf = (await (await fetch('/api/v1/workflows/i/' + src.workflow_id)).json()).workflow;
    delete wf.id;
    wf.name = ${JSON.stringify(WORKFLOW)};
    wf.meta = { ...wf.meta, category: 'user' };
    const w = await fetch('/api/v1/workflows/', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ workflow: wf }) });
    return [p.status, w.status, src.name, (await w.text()).slice(0, 200)];
  })()`);
  say(`made in Invoke: ${JSON.stringify(made)}`);
  check("made a template and a workflow in Invoke", made?.[0] === 200 && made?.[1] === 200, JSON.stringify(made?.slice(0, 3)));
  r.ws.close();

  await stopSession(m, s.instance_id);
  const lib = JSON.parse(readFileSync(join(CONFIG_DIR, "library.json"), "utf8"));
  const t = lib.templates?.find((x) => x.name === TEMPLATE);
  check("the template is saved on this PC after Stop", t?.positive === "watercolor, {prompt}" && t?.negative === "photo", JSON.stringify(lib.templates?.map((x) => x.name)));
  check("built-ins aren't copied as the user's", !lib.templates?.some((x) => builtinNames.includes(x.name)));
  const w = lib.workflows?.find((x) => x.name === WORKFLOW);
  check("the workflow is saved on this PC after Stop, without its id", !!w && !("id" in w.workflow) && (w.workflow.nodes?.length ?? 0) > 0, `${w?.workflow?.nodes?.length ?? 0} nodes`);
}

// ----- GPU 2 ---------------------------------------------------------------------

async function gpu2(m, builtinNames) {
  const s = await startSession(m);
  const r = await invokePage();
  const names = (await presetNames(r)).map((p) => p.name);
  check("fresh GPU has the saved template and the built-ins", names.includes(TEMPLATE) && builtinNames.every((n) => names.includes(n)), names.join(", "));
  check("nothing doubled", new Set(names).size === names.length);
  const flows = (await api(r, "/api/v1/workflows/?categories=user&per_page=50"))?.items ?? [];
  const mine = flows.filter((f) => f.name === WORKFLOW);
  check("fresh GPU has the saved workflow", mine.length === 1, flows.map((f) => f.name).join(", "));
  if (mine.length) {
    const wf = await api(r, `/api/v1/workflows/i/${mine[0].workflow_id}`);
    check("the restored workflow opens (has its graph)", (wf?.workflow?.nodes?.length ?? 0) > 0, `${wf?.workflow?.nodes?.length ?? 0} nodes`);
  }
  await r.shot("l5-gpu2");
  r.ws.close();
  await stopSession(m, s.instance_id);
}

// ----- main ----------------------------------------------------------------------

const before = new Set((await ourInstances()).map((i) => i.id));
if (before.size) {
  say(`refusing to run: sloptweak instances already exist: ${[...before].join(", ")}`);
  process.exit(2);
}
if (existsSync(join(DATA_DIR, "active_instance.json"))) {
  say("refusing to run: an active_instance.json record exists");
  process.exit(2);
}
const catalogText = readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8");
const model = JSON.parse(catalogText).models.find((x) => x.id === MODEL);
const builtinNames = (model?.templates ?? []).map((t) => `${model.name} · ${t.name}`);
if (!builtinNames.length) {
  say(`refusing to run: ${MODEL} has no built-in templates to test`);
  process.exit(2);
}
const catalogServer = createServer((req, res) => {
  res.writeHead(200, { "content-type": "application/json" });
  res.end(catalogText);
}).listen(CATALOG_PORT, "127.0.0.1");

const creditBefore = await credit();
say(`credit before: $${creditBefore?.toFixed(4)}`);
mkdirSync(CONFIG_DIR, { recursive: true });
const settingsFile = join(CONFIG_DIR, "settings.json");
const libraryFile = join(CONFIG_DIR, "library.json");
const settingsBackup = existsSync(settingsFile) ? readFileSync(settingsFile, "utf8") : null;
const libraryBackup = existsSync(libraryFile) ? readFileSync(libraryFile, "utf8") : null;
rmSync(libraryFile, { force: true });
const outDir = mkdtempSync(join(tmpdir(), "sloptweak-library-out-"));
writeFileSync(
  settingsFile,
  JSON.stringify({ model_id: MODEL, output_dir: outDir, tutorial_done: false, max_dph: 0.25, max_session_minutes: 60, heartbeat_minutes: 3 }, null, 2),
);

vite = spawn("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true, windowsHide: true, stdio: "ignore" });
await waitFor(() => servesThisCheckout(APP), 30000, "vite");

const created = new Set();
const tracker = setInterval(async () => {
  try {
    for (const i of await ourInstances()) {
      if (!before.has(i.id) && !created.has(i.id)) {
        created.add(i.id);
        say(`[tracker] INSTANCE ${i.id} exists (${i.gpu_name}, $${(i.dph_total ?? 0).toFixed(4)}/hr)`);
      }
    }
  } catch {
    /* transient */
  }
}, 20000);

try {
  launch();
  const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420"));
  await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 30000, "UI");
  await gpu1(m, builtinNames);
  await gpu2(m, builtinNames);
  m.ws.close();
} catch (e) {
  check("run completed", false, String(e));
} finally {
  clearInterval(tracker);
  if (app) kill(app);
  if (vite) kill(vite);
  catalogServer.close();
  for (const i of await ourInstances()) {
    if (!before.has(i.id)) {
      say(`SAFETY NET: destroying leftover instance ${i.id}`);
      await vast("DELETE", `/instances/${i.id}/`);
    }
  }
  if (settingsBackup !== null) writeFileSync(settingsFile, settingsBackup);
  else rmSync(settingsFile, { force: true });
  if (libraryBackup !== null) writeFileSync(libraryFile, libraryBackup);
  else rmSync(libraryFile, { force: true });
  await sleep(10000);
  const left = (await ourInstances()).filter((i) => !before.has(i.id));
  say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
  const creditAfter = await credit();
  say(`credit after: $${creditAfter?.toFixed(4)} (spent ~$${(creditBefore - creditAfter).toFixed(4)}; late charges may post)`);
  say(`instances seen: ${[...created].join(", ") || "none"}`);
  const failed = results.filter((x) => !x.ok).length;
  say(`${results.length - failed}/${results.length} checks passed`);
}
