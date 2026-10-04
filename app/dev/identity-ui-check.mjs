// Drive the Identity Edit panel against MockProvider ($0, no Vast calls):
//   fresh mock profile -> wizard -> raise the price limit -> pick the Identity
//   Edit model -> Start -> panel appears (no Invoke buttons) -> add one and
//   two character sheets -> Make image -> result shows, file is saved ->
//   use a result as a sheet -> Stop destroys.
// The mock ComfyUI answers /upload, /prompt, /history and /view in memory.
// The native file dialog can't be driven, so the debug build reads the sheets
// from SLOPTWEAK_DEV_PICK_REF.
//
// Usage (from app/):  node dev/identity-ui-check.mjs
// Needs a debug build (cargo build in src-tauri). Starts vite on :1420 itself.
// Screenshots go to $SHOTS.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import zlib from "node:zlib";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const CDP_PORT = 9335;
const CATALOG_PORT = 18432;
const MOCK_OUT = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher", "mock", "output");
const filesIn = (dir) =>
  existsSync(dir) ? readdirSync(dir, { withFileTypes: true }).filter((d) => d.isFile()).length : 0;
const SHOTS = process.env.SHOTS ?? mkdtempSync(join(tmpdir(), "sloptweak-identity-"));
const procs = [];
const results = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const FAKE_VAST = "ab".repeat(32);
const FAKE_CIVITAI = "mockcivitaikey0123456789abcdef";

// Two small "character sheets" (valid PNGs: a solid colour each).
function png(r, g, b, w = 8, h = 8) {
  // w x h RGB PNG built by hand (zlib via node).
  const stride = 1 + w * 3;
  const raw = Buffer.alloc(h * stride);
  for (let y = 0; y < h; y++) {
    raw[y * stride] = 0;
    for (let x = 0; x < w; x++) raw.set([r, g, b], y * stride + 1 + x * 3);
  }
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (buf) => {
    let c = 0xffffffff;
    for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type), data]);
    const c = Buffer.alloc(4);
    c.writeUInt32BE(crc(td));
    return Buffer.concat([len, td, c]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 2;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}
const sheetA = join(SHOTS, "fox.png");
const sheetB = join(SHOTS, "wolf.png");
writeFileSync(sheetA, png(210, 120, 40));
writeFileSync(sheetB, png(90, 110, 130, 240, 300));

function start(cmd, args, opts = {}) {
  const p = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], windowsHide: true, ...opts });
  let out = "";
  p.stdout.on("data", (d) => (out += d));
  p.stderr.on("data", (d) => (out += d));
  p.output = () => out;
  procs.push(p);
  return p;
}

function kill(p) {
  try {
    execSync(`taskkill /T /F /PID ${p.pid}`, { stdio: "ignore" });
  } catch {
    /* gone */
  }
}

function check(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
}

async function waitFor(fn, ms, what) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      /* retry */
    }
    await sleep(300);
  }
  throw new Error(`timed out waiting for ${what}`);
}

const bundled = JSON.parse(readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8"));
const catalogServer = createServer((_, res) => {
  res.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify(bundled));
});
await new Promise((r) => catalogServer.listen(CATALOG_PORT, "127.0.0.1", r));

async function connect() {
  const page = await waitFor(async () => {
    const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
    return (await r.json()).find((t) => t.type === "page" && t.url.includes("localhost:1420"));
  }, 60000, "main window");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
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
  const evaluate = async (expression) =>
    (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result
      ?.result?.value;
  await send("Emulation.setDeviceMetricsOverride", { width: 1100, height: 2000, deviceScaleFactor: 1, mobile: false });
  const shot = async (name) => {
    const r = await send("Page.captureScreenshot", { format: "png" });
    const file = join(SHOTS, `${name}.png`);
    writeFileSync(file, Buffer.from(r.result.data, "base64"));
    console.log(`shot  ${file}`);
  };
  return { ws, evaluate, shot, send };
}

const q = (id) => `document.getElementById(${JSON.stringify(id)})`;
const text = (id) => `${q(id)}.textContent`;
const visible = (id) => `!${q(id)}.hidden`;
const click = (id) => `${q(id)}.click()`;
const setValue = (id, v) =>
  `(() => { const i = ${q(id)}; i.value = ${JSON.stringify(v)}; i.dispatchEvent(new Event('input')); i.dispatchEvent(new Event('change')); })()`;

async function run() {
  const app = start(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "mock",
      SLOPTWEAK_DEV_RESET: "1",
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(CDP_PORT),
      SLOPTWEAK_DEV_PICK_REF: `${sheetA};${sheetB};${sheetB}`,
    },
  });
  const { ws, evaluate, shot, send } = await connect();
  await waitFor(async () => (await evaluate(`${q("model")}.options.length`)) > 0, 30000, "ui");
  await sleep(800);

  // Wizard (keys are fake; the mock accepts them).
  const next = async () => {
    await evaluate(click("wizard-next"));
    await sleep(200);
  };
  for (let i = 0; i < 3; i++) await next();
  await evaluate(setValue("key-vast", FAKE_VAST));
  await waitFor(async () => (await evaluate(text("key-vast-msg"))).includes("Connected"), 10000, "vast key");
  await next();
  await next();
  await evaluate(setValue("key-civitai", FAKE_CIVITAI));
  await evaluate(`${q("key-civitai")}.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter'}))`);
  await waitFor(async () => (await evaluate(text("key-civitai-msg"))).includes("mock-user"), 10000, "civitai key");
  await next();
  await next();
  check("wizard ends on the home screen", await evaluate(visible("view-home")));

  // The Identity Edit GPU is pricier than the default limit.
  await evaluate(click("nav-settings"));
  await sleep(300);
  await evaluate(`${q("s-max-dph")}.value = '0.70'`);
  await evaluate(`${q("settings-form")}.requestSubmit()`);
  await waitFor(async () => (await evaluate(text("settings-msg"))).includes("Saved"), 5000, "settings saved");
  await evaluate(click("nav-home"));
  await sleep(500);

  const ids = await evaluate(`[...${q("model")}.options].map((o) => o.value)`);
  check("the model list has Wulver Identity Edit", ids.includes("wulver-identity-edit"), ids.join(","));
  await evaluate(setValue("model", "wulver-identity-edit"));
  await sleep(600);
  check("the panel is hidden before a session", !(await evaluate(visible("identity"))));
  await waitFor(async () => (await evaluate(text("estimate"))).includes("/hr"), 15000, "estimate");
  await shot("i1-picked");
  const before = filesIn(MOCK_OUT);
  await evaluate(click("start"));
  await waitFor(async () => await evaluate(visible("identity")), 90000, "identity panel");
  check("the Identity Edit panel opens when the GPU is ready", true);
  check("no Open Invoke / tutorial buttons", !(await evaluate(visible("open"))) && !(await evaluate(visible("tutorial"))));
  check("Open ComfyUI (advanced) is offered", await evaluate(visible("open-comfy")));
  check("status says Identity Edit is ready", (await evaluate(text("status-text"))).includes("Identity Edit is ready"));
  check("Make image is off with nothing added", await evaluate(`${q("id-go")}.disabled`));
  await shot("i2-ready");

  // One sheet + a prompt.
  await evaluate(click("ref-pick-0"));
  await waitFor(async () => await evaluate(`!!${q("ref-img-0")}.querySelector('img')`), 10000, "sheet 1");
  check("sheet 1 shows a thumbnail", true);
  check("Make image needs a prompt too", await evaluate(`${q("id-go")}.disabled`));
  await evaluate(setValue("id-prompt", "They share a coffee at a sunny cafe."));
  check("Make image turns on", !(await evaluate(`${q("id-go")}.disabled`)));
  await evaluate(click("id-go"));
  await waitFor(async () => (await evaluate(`${q("id-results")}.querySelectorAll('img').length`)) === 1, 30000, "first result");
  check("a result appears", true);
  check("the message says it was saved", (await evaluate(text("id-msg"))).includes("saved"));
  await shot("i3-one-result");

  // Two sheets, tall, then keep going from the result.
  await evaluate(click("ref-pick-1"));
  await waitFor(async () => await evaluate(`!!${q("ref-img-1")}.querySelector('img')`), 10000, "sheet 2");
  await evaluate(setValue("id-aspect", "portrait"));
  await evaluate(click("id-go"));
  await waitFor(async () => (await evaluate(`${q("id-results")}.querySelectorAll('img').length`)) === 2, 30000, "second result");
  check("a second image with two sheets", true);
  await evaluate(`${q("id-results")}.querySelector('figcaption button').click()`);
  await sleep(500);
  check(
    "a result can become character 1",
    await evaluate(`${q("ref-img-0")}.querySelector('img').title.startsWith('ComfyUI_') || ${q("ref-img-0")}.querySelector('img').title.startsWith('SlopTweak_')`),
  );
  await evaluate(click("ref-clear-1"));
  await sleep(400);
  check("Clear removes a sheet", await evaluate(`!${q("ref-img-1")}.querySelector('img')`));
  await shot("i4-two-results");

  // Edit mode: a picture to edit (plus the sheet), size follows the picture.
  await evaluate(setValue("id-mode", "edit"));
  await sleep(400);
  check("Edit mode shows the picture slot and hides character 2 and Shape", (await evaluate(visible("ref-box-2"))) && !(await evaluate(visible("ref-box-1"))) && !(await evaluate(visible("id-aspect-row"))));
  check("the button says Edit image", (await evaluate(text("id-go"))) === "Edit image");
  await evaluate(setValue("id-prompt", "Change the jacket into a red hoodie. Keep everything else."));
  await sleep(300);
  check("Edit image needs a picture", await evaluate(`${q("id-go")}.disabled`));
  await evaluate(click("ref-pick-2"));
  await waitFor(async () => await evaluate(`!!${q("ref-img-2")}.querySelector('img')`), 10000, "picture to edit");
  check("Edit image turns on", !(await evaluate(`${q("id-go")}.disabled`)));
  await evaluate(click("id-go"));
  await waitFor(async () => (await evaluate(`${q("id-results")}.querySelectorAll('img').length`)) === 3, 30000, "edit result");
  check("an edited image appears", true);
  await evaluate(`${q("id-results")}.querySelectorAll('figure')[0].querySelectorAll('figcaption button')[2].click()`);
  await sleep(500);
  check("a result can become the picture to edit", await evaluate(`!!${q("ref-img-2")}.querySelector('img') && ${q("id-mode")}.value === 'edit'`));
  await shot("i5-edit-mode");

  // Strength slider and a painted area (real mouse events on the editor canvas).
  await evaluate(click("ref-pick-2")); // the mock's results are tiny: paint on a real-sized picture
  await sleep(600);
  await evaluate(setValue("id-strength", "70"));
  check("the slider shows its value", (await evaluate(text("id-strength-val"))) === "70%");
  check("no painted area yet", !(await evaluate(visible("mask-clear"))) && !(await evaluate(visible("mask-zoom-row"))));
  await evaluate(click("mask-open"));
  await waitFor(async () => await evaluate(`!${q("masker")}.hidden && ${q("mask-base")}.naturalWidth > 0`), 10000, "mask editor");
  await evaluate(`${q("mask-base")}.style.width = '400px'; ${q("mask-base")}.style.maxWidth = 'none'`);
  await sleep(300);
  const box = await evaluate(`(() => { const r = ${q("mask-canvas")}.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; })()`);
  const mouse = (type, fx, fy, buttons) =>
    send("Input.dispatchMouseEvent", { type, x: box.x + box.w * fx, y: box.y + box.h * fy, button: "left", buttons, clickCount: 1 });
  await evaluate(setValue("mask-size", "60"));
  await mouse("mouseMoved", 0.25, 0.25, 0);
  await mouse("mousePressed", 0.25, 0.25, 1);
  await mouse("mouseMoved", 0.6, 0.4, 1);
  await mouse("mouseMoved", 0.6, 0.7, 1);
  await mouse("mouseReleased", 0.6, 0.7, 0);
  const painted = () => evaluate(`(() => { const c = ${q("mask-canvas")}; const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data; let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i] > 40) n++; return n; })()`);
  const dots = await painted();
  check("painting marks the canvas", dots > 4, `${dots} px`);
  await evaluate(click("mask-undo"));
  check("Undo takes the stroke back", (await painted()) === 0);
  await mouse("mouseMoved", 0.3, 0.3, 0);
  await mouse("mousePressed", 0.3, 0.3, 1);
  await mouse("mouseMoved", 0.7, 0.6, 1);
  await mouse("mouseReleased", 0.7, 0.6, 0);
  await shot("i6-painting");
  await evaluate(click("mask-done"));
  await waitFor(async () => await evaluate(visible("mask-clear")), 10000, "painted area kept");
  check("Done keeps the painted area and offers zoom", (await evaluate(visible("mask-zoom-row"))) && (await evaluate(`!!${q("ref-img-2")}.querySelector('img.mask-overlay')`)));
  check("the hint says only the painted area changes", (await evaluate(text("id-strength-hint"))).includes("painted area"));
  await evaluate(click("id-go"));
  await waitFor(async () => (await evaluate(`${q("id-results")}.querySelectorAll('img').length`)) === 4, 30000, "painted-edit result");
  check("a painted edit with zoom completes", !(await evaluate(text("id-msg"))).toLowerCase().includes("paint"));
  await evaluate(click("ref-pick-2")); // choosing a picture again starts without the old painted area
  await sleep(800);
  check("a new picture drops the painted area", !(await evaluate(visible("mask-clear"))));
  await evaluate(click("mask-open"));
  await waitFor(async () => await evaluate(`!${q("masker")}.hidden && ${q("mask-base")}.naturalWidth > 0`), 10000, "mask editor again");
  const leftover = await evaluate(`(() => { const c = ${q("mask-canvas")}; const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data; let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i] > 40) n++; return n; })()`);
  check("the editor opens blank for the new picture", leftover === 0, `${leftover} px`);
  await evaluate(click("mask-cancel"));
  await evaluate(click("mask-open"));
  await waitFor(async () => await evaluate(visible("masker")), 10000, "editor");
  await evaluate(click("mask-cancel"));
  await evaluate(click("mask-clear"));
  check("Remove painted area clears it", !(await evaluate(visible("mask-clear"))));
  await evaluate(`${q("ref-img-2")}.querySelector('img').click()`);
  check("clicking a picture opens the big view", await evaluate(visible("lightbox")));
  await evaluate(click("lightbox-close"));
  check("Close closes it", !(await evaluate(visible("lightbox"))));
  await evaluate(setValue("id-mode", "new"));
  await sleep(300);
  check("New image mode brings back character 2 and Shape", (await evaluate(visible("ref-box-1"))) && (await evaluate(visible("id-aspect-row"))) && !(await evaluate(visible("ref-box-2"))));

  // Saved while running; Stop saves the rest and destroys.
  await waitFor(async () => filesIn(MOCK_OUT) >= before + 4, 30000, "files saved");
  check("all images were saved to the output folder", filesIn(MOCK_OUT) >= before + 3, `${filesIn(MOCK_OUT) - before} new`);
  await evaluate(click("stop"));
  await waitFor(async () => await evaluate(visible("start")), 60000, "stopped");
  check("Stop returns to idle and hides the panel", !(await evaluate(visible("identity"))) && !(await evaluate(visible("open-comfy"))));
  check("log shows the instance destroyed", /instance \d+ destroyed/.test(app.output()));
  ws.close();
  kill(app);
}

try {
  start("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true });
  await waitFor(() => servesThisCheckout(APP), 30000, "vite");
  await run();
} catch (e) {
  check("harness", false, String(e));
} finally {
  for (const p of procs.reverse()) kill(p);
  catalogServer.close();
}
const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} passed`);
process.exit(failed ? 1 : 0);
