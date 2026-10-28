// Drive the MOSS panel's Layout mode (box editor + per-box LoRA) against MockProvider
// ($0, no Vast calls): wizard -> Kromoss -> Start -> Layout tab -> draw, move, resize and
// reorder boxes with real mouse and key events (CDP) -> add a LoRA (the mock GPU takes it)
// -> give a box that LoRA -> Make image -> the caption names the boxes and the LoRA ->
// Prompt tab and Full quality behave -> Stop. The mock ComfyUI answers /prompt in memory.
//
// Usage (from app/):  node dev/layout-ui-check.mjs
// Needs a debug build (cargo build in src-tauri). Starts vite on :1420 unless one for this
// checkout is already there. Screenshots go to $SHOTS.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const CDP_PORT = 9337;
const CATALOG_PORT = 18434;
const MOCK_OUT = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher", "mock", "output");
const filesIn = (dir) =>
  existsSync(dir) ? readdirSync(dir, { withFileTypes: true }).filter((d) => d.isFile()).length : 0;
const SHOTS = process.env.SHOTS ?? mkdtempSync(join(tmpdir(), "sloptweak-layout-"));
mkdirSync(SHOTS, { recursive: true });
const procs = [];
const results = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const FAKE_VAST = "ab".repeat(32);
const FAKE_CIVITAI = "mockcivitaikey0123456789abcdef";

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

// ----- real input through CDP -------------------------------------------------------------
const MOD = { alt: 1, ctrl: 2, shift: 8 };

async function run() {
  // The debug build's file picker is answered by this env var (a tiny valid safetensors header).
  const loraFile = join(SHOTS, "fake-char.safetensors");
  // The app reads the tensor names before uploading (a Kohaku export's naming fits; an empty
  // or other-model header is refused), so the fake needs real-looking ones. No weights.
  const tensor = { dtype: "F32", shape: [4, 8], data_offsets: [0, 0] };
  const head = Buffer.from(
    JSON.stringify({
      "lora_unet_layers_0_proj_o.lora_down.weight": tensor,
      "lora_unet_layers_0_proj_o.lora_up.weight": tensor,
      "lora_unet_layers_0_proj_o.alpha": tensor,
    }),
  );
  const len = Buffer.alloc(8);
  len.writeBigUInt64LE(BigInt(head.length));
  writeFileSync(loraFile, Buffer.concat([len, head]));
  const app = start(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "mock",
      SLOPTWEAK_DEV_RESET: "1",
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(CDP_PORT),
      SLOPTWEAK_DEV_PICK_LORA: loraFile,
      // Its own browser profile: an installed SlopTweak that is running shares the default
      // one, and WebView2 refuses a second window with different debugging arguments.
      WEBVIEW2_USER_DATA_FOLDER: join(SHOTS, "webview-profile"),
    },
  });
  const { ws, evaluate, shot, send } = await connect();
  const seen = [];
  ws.addEventListener("message", (m) => {
    const msg = JSON.parse(m.data);
    if (msg.method === "Runtime.exceptionThrown") seen.push(msg.params.exceptionDetails?.exception?.description ?? msg.params.exceptionDetails?.text);
    if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") seen.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
  });
  await send("Runtime.enable");
  const uiUp = () => waitFor(async () => (await evaluate(`${q("model")}.options.length`)) > 0, 20000, "ui");
  try {
    await uiUp();
  } catch {
    await send("Page.reload");
    await uiUp();
  }
  await sleep(800);

  const mouse = (type, x, y) =>
    send("Input.dispatchMouseEvent", { type, x, y, button: "left", buttons: type === "mouseReleased" ? 0 : 1, clickCount: 1 });
  const drag = async (x0, y0, x1, y1) => {
    await mouse("mousePressed", x0, y0);
    const steps = 6;
    for (let i = 1; i <= steps; i++) await mouse("mouseMoved", x0 + ((x1 - x0) * i) / steps, y0 + ((y1 - y0) * i) / steps);
    await mouse("mouseReleased", x1, y1);
    await sleep(150);
  };
  const press = async (key, code, vk, mods = 0) => {
    await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key, code, windowsVirtualKeyCode: vk, modifiers: mods });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key, code, windowsVirtualKeyCode: vk, modifiers: mods });
    await sleep(60);
  };
  const canvasRect = async () => {
    await evaluate(`${q("lay-canvas")}.scrollIntoView({block: "center"})`);
    const r = JSON.parse(
      await evaluate(`JSON.stringify((r => ({x: r.left, y: r.top, w: r.width, h: r.height}))(${q("lay-canvas")}.getBoundingClientRect()))`),
    );
    return { ...r, at: (fx, fy) => [r.x + r.w * fx, r.y + r.h * fy] };
  };
  const field = (id) => evaluate(`${q(id)}.value`);
  const edges = async () => [Number(await field("lay-y0")), Number(await field("lay-x0")), Number(await field("lay-y1")), Number(await field("lay-x1"))];
  const boxCount = () => evaluate(`document.querySelectorAll("#lay-canvas .lay-box:not(.lay-ghost)").length`);

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
  await evaluate(click("nav-settings"));
  await sleep(300);
  await evaluate(`${q("s-max-dph")}.value = '0.70'`);
  await evaluate(`${q("settings-form")}.requestSubmit()`);
  await waitFor(async () => (await evaluate(text("settings-msg"))).includes("Saved"), 5000, "settings saved");
  await evaluate(click("nav-home"));
  await sleep(500);

  const ids = await evaluate(`[...${q("model")}.options].map((o) => o.value)`);
  check("the model list has Kromoss", ids.includes("kromoss"), ids.join(","));
  await evaluate(setValue("model", "kromoss"));
  await sleep(600);
  await waitFor(async () => (await evaluate(text("estimate"))).includes("/hr"), 15000, "estimate");
  await evaluate(click("start"));
  await waitFor(async () => await evaluate(visible("moss")), 90000, "moss panel");
  check("the MOSS panel opens for Kromoss", true);
  check("it starts in Prompt mode: no editor, the LoRA block is there", !(await evaluate(visible("moss-layout"))) && (await evaluate(visible("moss-lora-box"))));
  check("the Layout tab is available in Fast mode", !(await evaluate(`${q("moss-tab-layout")}.disabled`)));

  // ----- Layout mode ---------------------------------------------------------------------
  await evaluate(click("moss-tab-layout"));
  check("Layout mode shows the editor and hides the single-LoRA block", (await evaluate(visible("moss-layout"))) && !(await evaluate(visible("moss-lora-box"))));
  check("the prompt field becomes the scene description", (await evaluate(text("moss-prompt-label"))) === "Describe the whole picture");
  check("Make image is off before anything is described", await evaluate(`${q("moss-go")}.disabled`));
  await evaluate(setValue("moss-prompt", "A rainy neon street at night, two characters side by side."));
  check(
    "with a scene but no boxes it still can't run, and says why",
    (await evaluate(`${q("moss-go")}.disabled`)) && (await evaluate(text("moss-msg"))).includes("Draw at least one box"),
  );

  // Draw with the mouse: from 15%,20% to 55%,80% of the canvas.
  let c = await canvasRect();
  await drag(...c.at(0.15, 0.2), ...c.at(0.55, 0.8));
  check("dragging on the canvas draws a box", (await boxCount()) === 1);
  check("the count says 1 of 12 boxes", (await evaluate(text("lay-count"))) === "1 of 12 boxes");
  const [y0a, x0a] = await edges();
  check("the box landed where it was drawn (about 15% across, 20% down)", Math.abs(x0a - 150) <= 12 && Math.abs(y0a - 200) <= 12, `left ${x0a}, top ${y0a}`);
  check("its description field has the focus", (await evaluate("document.activeElement.id")) === "lay-caption");
  check(
    "Make says the box needs a description",
    (await evaluate(`${q("moss-go")}.disabled`)) && (await evaluate(text("moss-msg"))).includes("Box 1 needs a description"),
  );
  await evaluate(setValue("lay-caption", "roka_soldier, a tall character with brown skin"));
  await evaluate(setValue("lay-kind", "character"));
  check("a described box lets Make image turn on", !(await evaluate(`${q("moss-go")}.disabled`)));
  await shot("l1-one-box");

  // Move and resize with the mouse (the box is selected; its handles show).
  c = await canvasRect();
  const before = await edges();
  const [cx, cy] = c.at((before[1] + before[3]) / 2000, (before[0] + before[2]) / 2000);
  await drag(cx, cy, cx + c.w * 0.1, cy + c.h * 0.05);
  const moved = await edges();
  check(
    "dragging the box moves it without resizing it",
    Math.abs(moved[1] - before[1] - 100) <= 12 && Math.abs(moved[0] - before[0] - 50) <= 12 && moved[3] - moved[1] === before[3] - before[1],
    `${before} -> ${moved}`,
  );
  const [hx, hy] = c.at(moved[3] / 1000, moved[2] / 1000);
  await drag(hx, hy, hx - c.w * 0.1, hy - c.h * 0.1);
  const resized = await edges();
  check(
    "dragging the corner handle resizes it",
    resized[3] < moved[3] - 60 && resized[2] < moved[2] - 60 && resized[0] === moved[0] && resized[1] === moved[1],
    `${moved} -> ${resized}`,
  );

  // Keyboard: focus the box, arrows move, Shift is 10x, Alt resizes.
  await evaluate(`document.querySelector('.lay-box[data-i="0"]').focus()`);
  const k0 = await edges();
  await press("ArrowRight", "ArrowRight", 39);
  await press("ArrowRight", "ArrowRight", 39);
  await press("ArrowDown", "ArrowDown", 40, MOD.shift);
  await press("ArrowRight", "ArrowRight", 39, MOD.alt);
  await press("ArrowDown", "ArrowDown", 40, MOD.alt | MOD.shift);
  const k1 = await edges();
  check(
    "arrow keys move by 1 (Shift: 10) and Alt+arrows resize",
    k1[1] === k0[1] + 2 && k1[0] === k0[0] + 10 && k1[3] === k0[3] + 3 && k1[2] === k0[2] + 20,
    `${k0} -> ${k1}`,
  );
  check("the box keeps the focus while it is edited by key", (await evaluate("document.activeElement.className")).includes("lay-box"));
  await evaluate(setValue("lay-x1", "5000"));
  check("a typed edge stays inside the picture", (await field("lay-x1")) === "1000");

  // A too-small drag is refused; a second box; reorder.
  c = await canvasRect();
  await drag(...c.at(0.9, 0.9), ...c.at(0.92, 0.92));
  check("a tiny drag draws nothing and says so", (await boxCount()) === 1 && (await evaluate(text("lay-note"))).includes("too small"));
  await evaluate(click("lay-add"));
  check("Add a box adds a second one without the mouse", (await boxCount()) === 2);
  await evaluate(setValue("lay-caption", "a gray-furred wolf woman in a red coat"));
  await evaluate(`document.querySelector('.lay-box[data-i="1"]').focus()`);
  await press("[", "BracketLeft", 219);
  const order = await evaluate(`[...document.querySelectorAll('#lay-list .lay-pick')].map((b) => b.textContent.slice(0, 22)).join(" | ")`);
  check("[ moves a box earlier in the draw order", order.startsWith("1. Object: a gray-furr") && order.includes("2. Character"), order);
  await press("]", "BracketRight", 221);
  check("] moves it back", (await evaluate(`${q("lay-list")}.querySelector('.lay-pick').textContent`)).startsWith("1. Character"));

  // Boxes only: make a picture with no LoRA at all.
  check("Make image is on", !(await evaluate(`${q("moss-go")}.disabled`)));
  const filesBefore = filesIn(MOCK_OUT);
  await evaluate(setValue("moss-seed", "777"));
  await evaluate(click("moss-go"));
  await waitFor(async () => (await evaluate(`${q("moss-results")}.querySelectorAll('img').length`)) === 1, 30000, "layout result");
  const cap1 = await evaluate(`${q("moss-results")}.querySelector('figcaption').textContent`);
  check("a boxes-only picture comes back, its caption naming the layout and no LoRA", cap1.includes("layout: 2 boxes") && !cap1.includes(" on box ") && cap1.includes("seed 777") && cap1.includes("Fast"), cap1);
  check("the message says it was saved", (await evaluate(text("moss-msg"))).includes("saved"));
  await waitFor(async () => filesIn(MOCK_OUT) >= filesBefore + 1, 30000, "file saved");
  check("the picture was saved to the output folder", true);
  await shot("l3-result-no-lora");

  // The LoRA: added through the panel's own flow, then given to box 1.
  await evaluate(`${q("lay-list")}.querySelectorAll('.lay-pick')[0].click()`);
  check("the box picker starts with only No LoRA", (await evaluate(`${q("lay-lora")}.options.length`)) === 1);
  await evaluate(click("lay-add-lora"));
  let loraName = "";
  try {
    await waitFor(async () => (await evaluate(`${q("lay-lora")}.options.length`)) === 2, 20000, "the LoRA to reach the GPU");
    loraName = await evaluate(`${q("lay-lora")}.options[1].value`);
  } catch {
    check("a LoRA file can be added to the GPU from the editor", false, `panel says: ${await evaluate(text("moss-msg"))}`);
  }
  if (loraName) {
    check("the added LoRA is on the list", /^user\/fake-char-[0-9a-f]{8}\.safetensors$/.test(loraName), loraName);
    check("and it was given to the box that was being edited", (await field("lay-lora")) === loraName);
    check(
      "strength and change amount show, at 1.00 and balanced",
      (await evaluate(text("lay-strength-val"))) === "1.00" && (await evaluate(text("lay-denoise-val"))) === "0.70 · balanced",
    );
    await evaluate(setValue("lay-denoise", "0.9"));
    check("a bigger change amount is called a redraw", (await evaluate(text("lay-denoise-val"))) === "0.90 · redraw");
    await evaluate(setValue("lay-denoise", "0.7"));
    await evaluate(setValue("lay-kind", "text"));
    check("a text box has no LoRA and wants words", !(await evaluate(`!!${q("lay-lora")}`)) && (await evaluate(text("moss-msg"))).includes("text box"));
    await evaluate(setValue("lay-kind", "character"));
    check("switching it back lost the LoRA, as a text box can't have one", (await field("lay-lora")) === "");
    await evaluate(setValue("lay-lora", loraName));
    await evaluate(`${q("lay-list")}.querySelectorAll('.lay-pick')[1].click()`);
    check("the second box's picker also lists the LoRA", (await evaluate(`${q("lay-lora")}.options.length`)) === 2);
    await evaluate(`${q("lay-list")}.querySelectorAll('.lay-pick')[0].click()`);
    await shot("l2-two-boxes-lora");

    await evaluate(setValue("moss-seed", "778"));
    await evaluate(click("moss-go"));
    await waitFor(async () => (await evaluate(`${q("moss-results")}.querySelectorAll('img').length`)) === 2, 30000, "layout result with a LoRA");
    const cap2 = await evaluate(`${q("moss-results")}.querySelector('figcaption').textContent`);
    check(
      "the next caption names the LoRA on box 1 with its strength and change",
      cap2.includes("layout: 2 boxes") && /fake-char-[0-9a-f]{8} on box 1 \(strength 1, change 0\.7\)/.test(cap2) && cap2.includes("seed 778"),
      cap2,
    );
    await shot("l3-result-lora");
  }

  // Back to Prompt mode, and Full quality.
  await evaluate(click("moss-tab-prompt"));
  check(
    "Prompt mode hides the editor and brings the LoRA block back",
    !(await evaluate(visible("moss-layout"))) && (await evaluate(visible("moss-lora-box"))) && (await evaluate(text("moss-prompt-label"))) === "What should it draw?",
  );
  await evaluate(click("moss-tab-layout"));
  check("the boxes are kept for when Layout is picked again", (await boxCount()) === 2);
  await evaluate(setValue("moss-mode", "full"));
  check("Layout needs Fast mode: its tab is off and the editor hides in Full quality", (await evaluate(`${q("moss-tab-layout")}.disabled`)) && !(await evaluate(visible("moss-layout"))));
  await evaluate(setValue("moss-mode", "fast"));
  check("back in Fast the editor returns with its boxes", (await evaluate(visible("moss-layout"))) && (await boxCount()) === 2);
  check("no script errors in the window", seen.length === 0, JSON.stringify(seen).slice(0, 600));

  await evaluate(click("stop"));
  await waitFor(async () => await evaluate(visible("start")), 60000, "stopped");
  check("Stop hides the panel", !(await evaluate(visible("moss"))));
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
