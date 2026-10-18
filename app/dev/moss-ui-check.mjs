// Drive the MOSS-Image panel against MockProvider ($0, no Vast calls):
//   fresh mock profile -> wizard -> raise the price limit -> pick the MOSS-Image
//   model -> Start -> its panel appears (not Identity Edit, no Invoke buttons) ->
//   prompt -> Make image -> result shows and is saved -> steps slider -> Stop.
// The mock ComfyUI answers /prompt, /history and /view in memory.
//
// Usage (from app/):  node dev/moss-ui-check.mjs
// Needs a debug build (cargo build in src-tauri). Starts vite on :1420 itself.
// Screenshots go to $SHOTS.
// Needs a debug build (cargo build in src-tauri). Starts vite on :1420 itself.
// Screenshots go to $SHOTS.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const CDP_PORT = 9336;
const CATALOG_PORT = 18433;
const MOCK_OUT = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher", "mock", "output");
const filesIn = (dir) =>
  existsSync(dir) ? readdirSync(dir, { withFileTypes: true }).filter((d) => d.isFile()).length : 0;
const SHOTS = process.env.SHOTS ?? mkdtempSync(join(tmpdir(), "sloptweak-moss-"));
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


async function run() {
  const app = start(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "mock",
      SLOPTWEAK_DEV_RESET: "1",
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(CDP_PORT),
    },
  });
  const { ws, evaluate, shot, send } = await connect();
  // A cold vite can optimise its dependencies after the window's first load and leave it
  // blank; one reload fixes that. Anything still wrong after it is a real error.
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
    try {
      await uiUp();
    } catch (e) {
      console.log(`start-up errors: ${JSON.stringify(seen).slice(0, 1500)}`);
      throw e;
    }
  }
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

  await evaluate(click("nav-settings"));
  await sleep(300);
  await evaluate(`${q("s-max-dph")}.value = '0.70'`);
  await evaluate(`${q("settings-form")}.requestSubmit()`);
  await waitFor(async () => (await evaluate(text("settings-msg"))).includes("Saved"), 5000, "settings saved");
  await evaluate(click("nav-home"));
  await sleep(500);

  const ids = await evaluate(`[...${q("model")}.options].map((o) => o.value)`);
  check("the model list has MOSS-Image", ids.includes("moss-image-dmd"), ids.join(","));
  await evaluate(setValue("model", "moss-image-dmd"));
  await sleep(600);
  check("neither panel shows before a session", !(await evaluate(visible("moss"))) && !(await evaluate(visible("identity"))));
  await waitFor(async () => (await evaluate(text("estimate"))).includes("/hr"), 15000, "estimate");
  const before = filesIn(MOCK_OUT);
  await evaluate(click("start"));
  await waitFor(async () => await evaluate(visible("moss")), 90000, "moss panel");
  check("the MOSS panel opens when the GPU is ready", true);
  check("Identity Edit's panel stays hidden", !(await evaluate(visible("identity"))));
  check("no Open Invoke / tutorial buttons", !(await evaluate(visible("open"))) && !(await evaluate(visible("tutorial"))));
  check("Make image is off with no prompt", await evaluate(`${q("moss-go")}.disabled`));
  check("status says MOSS-Image is ready", (await evaluate(text("status-text"))).includes("MOSS-Image is ready"));
  check("steps start at 8", (await evaluate(text("moss-steps-val"))) === "8");
  await shot("m1-ready");

  await evaluate(setValue("moss-steps", "4"));
  check("the steps slider shows its value", (await evaluate(text("moss-steps-val"))) === "4");
  await evaluate(setValue("moss-steps", "8"));
  await evaluate(setValue("moss-prompt", "A red fox in a snowy forest at dusk."));
  check("Make image turns on with a prompt", !(await evaluate(`${q("moss-go")}.disabled`)));
  await evaluate(click("moss-go"));
  await waitFor(async () => (await evaluate(`${q("moss-results")}.querySelectorAll('img').length`)) === 1, 30000, "first result");
  check("a result appears", true);
  check("the message says it was saved", (await evaluate(text("moss-msg"))).includes("saved"));
  const graph = await evaluate("window.__TAURI_INTERNALS__.invoke('moss_state').then(() => 'ok')");
  check("moss_state answers", graph === "ok");

  await evaluate(setValue("moss-aspect", "portrait"));
  await evaluate(setValue("moss-steps", "6"));
  await evaluate(setValue("moss-prompt", "A woman in a yellow raincoat reading under a cafe awning."));
  await evaluate(click("moss-go"));
  await waitFor(async () => (await evaluate(`${q("moss-results")}.querySelectorAll('img').length`)) === 2, 30000, "second result");
  check("a second image with other settings", true);
  await shot("m2-two-results");

  await evaluate(`${q("moss-results")}.querySelector('img').click()`);
  check("clicking a picture opens the big view with Save", (await evaluate(visible("lightbox"))) && (await evaluate(visible("lightbox-save"))));
  await evaluate(click("lightbox-close"));

  await waitFor(async () => filesIn(MOCK_OUT) >= before + 2, 30000, "files saved");
  check("both images were saved to the output folder", filesIn(MOCK_OUT) >= before + 2, `${filesIn(MOCK_OUT) - before} new`);
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
