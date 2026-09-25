// Phase 4 acceptance against real Vast. SPENDS MONEY: run only with approval.
//
//   1. Clean tutorial state (tutorial_done=false, fresh tunnel origin) ->
//      Start -> the tutorial shows in the Invoke window by itself.
//   2. Follow it in the real Invoke 6.14.1 UI (driven over CDP): sample
//      upload, Assets -> New Canvas from Image -> As Raster Layer (Resize),
//      Inpaint Mask + brush, prompt, Invoke, auto-advance, Accept, Done.
//      If a UI step can't be automated, the script asks a person to finish
//      the tutorial by hand and waits for it.
//   3. Generate 10 gallery images, press Stop as soon as the 10th is done,
//      and check all 10 are in the output folder (by Invoke image name).
//
// Safety: refuses to run if sloptweak instances exist; every instance created
// during the run is destroyed in `finally`; ids are printed as seen. Keys are
// read from HKCU\Environment and never printed. settings.json is backed up
// and restored.
//
// Usage (from app/):  node dev/phase4-acceptance.mjs
//   TEST_MODEL=<catalog id>  (default banana-splitz-xxl)

import { spawn, execSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const OUT = process.env.OUT ?? join(APP, "..", ".dev", "phase4");
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const VAST = "https://console.vast.ai/api/v0";
const CONFIG_DIR = join(process.env.APPDATA ?? "", "com.sloptweak.launcher");
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");
const MODEL = process.env.TEST_MODEL ?? "banana-splitz-xxl";
const IMAGES = 10;

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

function launch() {
  const logFile = join(OUT, "app.log");
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
  say(`app launched, log ${logFile}`);
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
  const c = {
    ws,
    send,
    eval: async (expression) =>
      (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true, includeCommandLineAPI: true })).result
        ?.result?.value,
    shot: async (name) => {
      const r = await send("Page.captureScreenshot", { format: "png" });
      const file = join(OUT, `${name}.png`);
      writeFileSync(file, Buffer.from(r.result.data, "base64"));
      say(`screenshot ${file}`);
    },
    mouse: async (type, x, y, button = "left", clickCount = 1) => send("Input.dispatchMouseEvent", { type, x, y, button, clickCount }),
    click: async (x, y, button = "left") => {
      await c.mouse("mouseMoved", x, y, "none");
      await c.mouse("mousePressed", x, y, button);
      await c.mouse("mouseReleased", x, y, button);
    },
    key: async (key, code, text) => {
      await send("Input.dispatchKeyEvent", { type: "keyDown", key, code, text });
      await send("Input.dispatchKeyEvent", { type: "keyUp", key, code });
    },
  };
  return c;
}

const snapshot = (m) => m.eval("window.__TAURI_INTERNALS__.invoke('get_snapshot')");

// The overlay in the remote page.
const TUT = "document.querySelector('sloptweak-tutorial')?.shadowRoot";
const cardText = (r) => r.eval(`${TUT}?.querySelector('.card, .pill')?.textContent ?? ''`);
const clickAct = (r, act) => r.eval(`(() => { const b = ${TUT}?.querySelector('[data-act="${act}"]'); if (!b) return false; b.click(); return true; })()`);

/** Center of the first visible element matching a JS predicate over candidates. */
const findBox = (r, selector, predicate) =>
  r.eval(`(() => {
    const els = [...document.querySelectorAll(${JSON.stringify(selector)})].filter((e) => {
      const b = e.getBoundingClientRect(); return b.width > 0 && b.height > 0 && (${predicate})(e);
    });
    if (!els.length) return null;
    const b = els[0].getBoundingClientRect();
    return { x: b.x + b.width / 2, y: b.y + b.height / 2, w: b.width, h: b.height };
  })()`);
const byText = (t) => `(e) => e.textContent.trim() === ${JSON.stringify(t)}`;
const byLabel = (t) => `(e) => (e.getAttribute('aria-label') || '').startsWith(${JSON.stringify(t)})`;

async function clickEl(r, what, selector, predicate) {
  const box = await waitFor(() => findBox(r, selector, predicate), 15000, what);
  await r.click(box.x, box.y);
  say(`clicked ${what}`);
  await sleep(1200);
  return box;
}

const api = (r, path) => r.eval(`fetch(${JSON.stringify(path)}).then(r => r.json())`);
const galleryTotal = async (r) =>
  (await api(r, "/api/v1/images/?order_dir=DESC&starred_first=false&is_intermediate=false&categories=general&limit=1"))?.total ?? 0;

async function setPrompt(r, text) {
  return r.eval(`(() => {
    const ta = document.querySelector('textarea');
    if (!ta) return 'no prompt box';
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(ta, ${JSON.stringify(text)});
    ta.dispatchEvent(new Event('input', { bubbles: true }));
    return 'ok'; })()`);
}
const clickInvoke = (r) =>
  r.eval(`(() => {
    const b = [...document.querySelectorAll('button')].find(b => b.getAttribute('aria-label') === 'Invoke' || /^Invoke$/.test(b.textContent.trim()));
    if (!b) return 'no Invoke button';
    if (b.disabled) return 'disabled';
    b.click(); return 'ok'; })()`);

// ----- the tutorial, driven like a person would ----------------------------------

async function tutorial(r, settingsFile, outDir) {
  const intro = await waitFor(async () => (await cardText(r)) || false, 30000, "tutorial card");
  check("clean install: tutorial shows by itself", intro.includes("Welcome to Invoke"), intro.trim().slice(0, 50));
  await r.shot("t0-intro");
  await clickAct(r, "next");
  await waitFor(async () => (await cardText(r)).includes("Open the sample picture"), 60000, "sample step");
  const assets = await api(r, "/api/v1/images/?categories=user&is_intermediate=false&limit=10");
  const sample = assets?.items?.[0]?.image_name;
  check("sample uploaded as an asset", !!sample, `${assets?.total} asset(s)`);
  await sleep(4000);
  await r.shot("t1-sample-step");

  let auto = true;
  try {
    // Gallery -> Assets -> right-click the sample -> New Canvas from Image -> As Raster Layer (Resize).
    await clickEl(r, "Assets tab", "button, [role=tab]", `(e) => /^Assets/.test(e.textContent.trim())`);
    const thumb = await waitFor(() => findBox(r, "img", `(e) => e.src.includes(${JSON.stringify(sample)})`), 15000, "sample thumbnail");
    await r.click(thumb.x, thumb.y, "right");
    await sleep(1000);
    await r.shot("t2-context-menu");
    const menuItem = (t) => findBox(r, "[role=menuitem]", byText(t));
    const item = await waitFor(() => menuItem("New Canvas from Image"), 10000, "New Canvas from Image");
    // Hover opens the submenu; then move sideways into it before clicking
    // (clicking straight away missed it on the first live run).
    await r.mouse("mouseMoved", item.x, item.y, "none");
    await sleep(900);
    const sub = await waitFor(() => menuItem("As Raster Layer (Resize)"), 10000, "As Raster Layer (Resize)");
    await r.mouse("mouseMoved", sub.x - 40, sub.y, "none");
    await sleep(300);
    await r.click(sub.x, sub.y);
    await waitFor(
      async () => (await r.eval(`document.querySelector('[data-testid="Canvas"]')?.getAttribute('data-selected')`)) === "true",
      15000,
      "Canvas tab",
    );
    await sleep(3000);
    await r.shot("t3-canvas");
    await clickAct(r, "next");

    // New canvases come with an empty Inpaint Mask layer: select it, B for
    // the brush, paint over the vase + flowers (left-middle of the picture,
    // which is fitted as a centered square in the stage).
    await waitFor(async () => (await cardText(r)).includes("Paint over the vase"), 5000, "mask step");
    const mask = await waitFor(() => findBox(r, "*", `(e) => e.children.length === 0 && e.textContent.trim() === 'Inpaint Mask'`), 10000, "Inpaint Mask layer");
    await r.click(mask.x, mask.y);
    const stage = await waitFor(() => findBox(r, "canvas", "(e) => e.getBoundingClientRect().width > 300"), 10000, "canvas stage");
    await r.click(stage.x, stage.y + stage.h / 2 - 10); // focus the stage (bottom edge, off the picture)
    await r.key("b", "KeyB", "b");
    await sleep(400);
    const side = Math.min(stage.w, stage.h) * 0.82;
    const [ix, iy] = [stage.x - side / 2, stage.y - side / 2];
    const at = (fx, fy) => [ix + fx * side, iy + fy * side];
    for (let fy = 0.1; fy <= 0.76; fy += 0.04) {
      const [x0, y] = at(0.24, fy);
      const [x1] = at(0.56, fy);
      await r.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: x0, y, button: "none" });
      await r.send("Input.dispatchMouseEvent", { type: "mousePressed", x: x0, y, button: "left", buttons: 1, clickCount: 1 });
      for (let x = x0; x <= x1; x += 10) {
        await r.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, button: "left", buttons: 1 });
      }
      await r.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: x1, y, button: "left", clickCount: 1 });
    }
    await sleep(1000);
    await r.shot("t4-masked");
    check("mask painted", !(await r.eval("document.body.innerText.includes('Inpaint Mask is empty')")));
    await clickAct(r, "next");

    // Prompt (typed) + Invoke; the card moves on by itself when it's done.
    await waitFor(async () => (await cardText(r)).includes("Say what goes there"), 5000, "prompt step");
    await sleep(2500);
    const ta = await findBox(r, "textarea", "() => true");
    await r.click(ta.x, ta.y);
    await r.send("Input.insertText", { text: "a bowl of oranges" });
    await sleep(800);
    const inv = await findBox(r, "button", byText("Invoke"));
    await r.click(inv.x, inv.y);
    say("typed the prompt and clicked Invoke");
    const started = Date.now();
    await waitFor(async () => (await cardText(r)).includes("Keep the one you like"), 8 * 60000, "auto-advance after generating");
    check("tutorial moved on when the picture was done", true, `${Math.round((Date.now() - started) / 1000)}s`);
    await sleep(3000);
    await r.shot("t5-staged");
    const accept = await findBox(r, "button", byLabel("Accept"));
    if (accept) {
      await r.click(accept.x, accept.y);
      say("clicked Accept");
    } else say("Accept button not found (staging toolbar)");
    await sleep(2000);
    await r.shot("t6-accepted");
    await clickAct(r, "next"); // Done
  } catch (e) {
    auto = false;
    say(`automation stopped: ${e}`);
    await r.shot("t-manual-needed");
    say(">>> PLEASE FINISH THE TUTORIAL BY HAND in the Invoke window (up to 15 minutes) <<<");
  }
  const done = await waitFor(() => JSON.parse(readFileSync(settingsFile, "utf8")).tutorial_done === true, auto ? 15000 : 15 * 60000, "tutorial done").catch(() => false);
  check("tutorial completed and recorded by the app", done === true, auto ? "automated" : "finished by hand");
  // For findings: what a real canvas queue item looks like (source -> result).
  const shape = await r.eval(`(async () => {
    const ids = (await (await fetch('/api/v1/queue/default/item_ids?order_dir=DESC')).json()).item_ids;
    const it = await (await fetch('/api/v1/queue/default/i/' + ids[0])).json();
    const m = it.session?.prepared_source_mapping ?? {};
    const out = Object.entries(m).filter(([, s]) => s.startsWith('canvas_output')).map(([p, s]) => {
      const name = it.session.results?.[p]?.image?.image_name;
      return { source: s.split(':')[0], prepared_is_uuid: /^[0-9a-f-]{36}$/.test(p), image: !!name };
    });
    const img = out.length ? await (await fetch('/api/v1/images/i/' + it.session.results[Object.keys(m).find((p) => m[p].startsWith('canvas_output'))].image.image_name)).json() : null;
    return JSON.stringify({ status: it.status, destination: it.destination, nodes: Object.keys(m).length, canvas_output: out,
      image: img && { is_intermediate: img.is_intermediate, category: img.image_category, node_id_is_prepared: /^[0-9a-f-]{36}$/.test(img.node_id ?? '') } });
  })()`).catch((e) => String(e));
  say(`canvas queue item: ${shape}`);
  const canvasFiles = existsSync(join(outDir, "Canvas")) ? readdirSync(join(outDir, "Canvas")) : [];
  await sleep(12000);
  const canvasNow = existsSync(join(outDir, "Canvas")) ? readdirSync(join(outDir, "Canvas")) : [];
  check("the tutorial's Canvas result was saved to Canvas\\", canvasNow.length >= 1, `${Math.max(canvasFiles.length, canvasNow.length)} file(s)`);
}

// ----- 10 images, then Stop -------------------------------------------------------

async function tenImagesThenStop(m, r, outDir, instanceId) {
  // Generate tab: gallery images (the canvas would stage them instead).
  const gen = await waitFor(() => findBox(r, '[data-testid="Generate"]', "() => true"), 15000, "Generate tab button");
  await r.click(gen.x, gen.y);
  await sleep(2500);
  await r.shot("g0-generate-tab");
  const onGenerate = await r.eval(`document.querySelector('[data-testid="Generate"]')?.getAttribute('data-selected')`);
  if (onGenerate !== "true") throw new Error("couldn't switch to the Generate tab; not queueing images");
  const before = await galleryTotal(r);
  say(`gallery before: ${before}`);
  say(`prompt: ${await setPrompt(r, "a lighthouse on a rocky coast at sunset, detailed illustration")}`);
  await sleep(1000);
  for (let i = 0; i < IMAGES; i++) {
    const res = await clickInvoke(r);
    if (res !== "ok") say(`invoke ${i + 1}: ${res}`);
    await sleep(700);
  }
  const q = await api(r, "/api/v1/queue/default/status");
  say(`queued: ${JSON.stringify(q?.queue)}`);
  const started = Date.now();
  await waitFor(async () => (await galleryTotal(r)) >= before + IMAGES, 15 * 60000, `${IMAGES} images`);
  const list = await api(r, `/api/v1/images/?order_dir=DESC&starred_first=false&is_intermediate=false&categories=general&limit=${IMAGES}`);
  const names = list.items.map((i) => i.image_name);
  say(`${IMAGES} images done in ${Math.round((Date.now() - started) / 1000)}s; pressing Stop now`);
  await r.shot("g1-ten-done");
  r.ws.close();
  const syncBefore = (await snapshot(m)).sync;
  await m.eval("document.getElementById('stop').click()");
  const stopAt = Date.now();
  await waitFor(async () => (await snapshot(m)).state.kind === "idle", 6 * 60000, "idle after Stop");
  const stopSecs = Math.round((Date.now() - stopAt) / 1000);
  const files = readdirSync(outDir).filter((f) => statSync(join(outDir, f)).isFile());
  const missing = names.filter((n) => !files.some((f) => f.endsWith(`_${n}`)));
  const sizes = names.map((n) => files.find((f) => f.endsWith(`_${n}`))).filter(Boolean).map((f) => statSync(join(outDir, f)).size);
  check(`all ${IMAGES} images are on disk after Stop`, missing.length === 0, `${IMAGES - missing.length}/${IMAGES}; synced before Stop: ${syncBefore?.saved ?? 0}; Stop took ${stopSecs}s`);
  check("they are real full-size PNGs", sizes.length === IMAGES && sizes.every((s) => s > 100_000), `${Math.min(...sizes)}–${Math.max(...sizes)} bytes`);
  const snap = await snapshot(m);
  check("sync report says done, nothing missing", snap.sync?.phase === "done" && snap.sync?.missing === 0, JSON.stringify(snap.sync));
  check("the Canvas try is counted too", snap.sync?.canvas_saved >= 1);
  check("Stop still destroyed the instance", await waitFor(() => instanceGone(instanceId), 60000, "gone").catch(() => false), `instance ${instanceId}`);
  check("record cleared", !existsSync(join(DATA_DIR, "active_instance.json")));
  await m.shot("g2-home-after-stop");
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
const creditBefore = await credit();
say(`credit before: $${creditBefore?.toFixed(4)}`);
mkdirSync(CONFIG_DIR, { recursive: true });
const settingsFile = join(CONFIG_DIR, "settings.json");
const settingsBackup = existsSync(settingsFile) ? readFileSync(settingsFile, "utf8") : null;
const outDir = mkdtempSync(join(tmpdir(), "sloptweak-phase4-out-"));
// Clean tutorial state; LoRAs off (a plain session); results to a temp folder.
writeFileSync(
  settingsFile,
  JSON.stringify({ model_id: MODEL, output_dir: outDir, tutorial_done: false, max_dph: 0.5, max_session_minutes: 90, heartbeat_minutes: 3 }, null, 2),
);
say(`output folder: ${outDir}`);

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
  say(`READY instance ${s.instance_id} offer ${s.offer.offer_id} (${s.offer.gpu_name}, ${s.offer.location}, $${s.offer.hourly.toFixed(4)}/hr)`);
  const r = await cdp(REMOTE_PORT, (u) => u.includes(".trycloudflare.com"), 120000);
  await waitFor(async () => (await r.eval("document.title")).includes("Invoke"), 120000, "Invoke UI");
  await sleep(8000);
  const unload = await r.eval("getEventListeners(window).beforeunload?.length ?? 0").catch(() => "n/a");
  say(`Invoke beforeunload listeners: ${unload}`);
  await tutorial(r, settingsFile, outDir);
  await tenImagesThenStop(m, r, outDir, s.instance_id);
  m.ws.close();
} catch (e) {
  check("run completed", false, String(e));
} finally {
  clearInterval(tracker);
  if (app) kill(app);
  if (vite) kill(vite);
  for (const i of await ourInstances()) {
    if (!before.has(i.id)) {
      say(`SAFETY NET: destroying leftover instance ${i.id}`);
      await vast("DELETE", `/instances/${i.id}/`);
    }
  }
  if (settingsBackup !== null) writeFileSync(settingsFile, settingsBackup);
  else rmSync(settingsFile, { force: true });
  await sleep(10000);
  const left = (await ourInstances()).filter((i) => !before.has(i.id));
  say(`instances left: ${left.length ? left.map((i) => i.id).join(", ") : "none"}`);
  const creditAfter = await credit();
  say(`credit after: $${creditAfter?.toFixed(4)} (spent ~$${(creditBefore - creditAfter).toFixed(4)}; late charges may post)`);
  say(`instances seen: ${[...created].join(", ") || "none"}`);
  say(`output kept for inspection: ${outDir}`);
  const failed = results.filter((x) => !x.ok).length;
  say(`${results.length - failed}/${results.length} checks passed`);
}
