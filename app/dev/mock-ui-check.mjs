// Drive the launcher UI against MockProvider + MockCivitai ($0, no network
// besides a local catalog server). Covers Phase 2 and 3:
//   fresh mock profile -> wizard (key checks on paste) -> home estimate ->
//   settings + validation -> LoRAs -> upstream catalog edit without rebuild ->
//   Start -> cost bar -> Stop -> restart (keys, settings, cached catalog
//   survive) -> low-balance gate -> close-while-running confirm + destroy.
// Mock mode uses its own Credential Manager service ("SlopTweak-mock") and
// its own folders, so the real profile is never touched.
//
// Usage (from app/):  node dev/mock-ui-check.mjs
// Needs a debug build. Starts vite on :1420 itself. Screenshots go to $SHOTS.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { COST_TITLE, windowTitles } from "./win-titles.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const CDP_PORT = 9334;
const CATALOG_PORT = 18431;
const SHOTS = process.env.SHOTS ?? mkdtempSync(join(tmpdir(), "sloptweak-shots-"));
const procs = [];
const results = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Fake keys for the mock only. The mock rejects all-zero Vast keys and
// CivitAI keys starting with "bad".
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

// ----- local "upstream" catalog ---------------------------------------------------

const bundled = JSON.parse(readFileSync(join(APP, "..", "catalog", "catalog.json"), "utf8"));
let catalogText = JSON.stringify(bundled);
let catalogUp = true;
const catalogServer = createServer((req, res) => {
  if (!catalogUp) {
    res.writeHead(503).end();
    return;
  }
  res.writeHead(200, { "Content-Type": "application/json" }).end(catalogText);
});
await new Promise((r) => catalogServer.listen(CATALOG_PORT, "127.0.0.1", r));

// ----- CDP -------------------------------------------------------------------------

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
  const shot = async (name) => {
    const r = await send("Page.captureScreenshot", { format: "png" });
    const file = join(SHOTS, `${name}.png`);
    writeFileSync(file, Buffer.from(r.result.data, "base64"));
    console.log(`shot  ${file}`);
  };
  return { ws, evaluate, shot };
}

const q = (id) => `document.getElementById(${JSON.stringify(id)})`;
const text = (id) => `${q(id)}.textContent`;
const visible = (id) => `!${q(id)}.hidden`;
const click = (id) => `${q(id)}.click()`;
const setValue = (id, v) =>
  `(() => { const i = ${q(id)}; i.value = ${JSON.stringify(v)}; i.dispatchEvent(new Event('input')); })()`;

async function launch(extraEnv = {}) {
  const app = start(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "mock",
      SLOPTWEAK_CATALOG_URL: `http://127.0.0.1:${CATALOG_PORT}/catalog.json`,
      SLOPTWEAK_MAIN_DEBUG_PORT: String(CDP_PORT),
      ...extraEnv,
    },
  });
  const c = await connect();
  // Loaded once the snapshot has filled the model list.
  await waitFor(async () => (await c.evaluate(`${q("model")}.options.length`)) > 0, 30000, "ui");
  await sleep(800);
  return { app, ...c };
}

async function closeIdle(s) {
  s.ws.close();
  kill(s.app);
  await sleep(1500);
}

// ----- the run ---------------------------------------------------------------------

async function wizardAndHome() {
  const s = await launch({ SLOPTWEAK_DEV_RESET: "1" });
  const { evaluate, shot } = s;
  const step = () => evaluate(`${q("view-wizard")}.dataset.step`);
  const next = async () => {
    await evaluate(click("wizard-next"));
    await sleep(200);
  };

  check("fresh profile opens the wizard", (await evaluate(visible("view-wizard"))) && (await step()) === "welcome");
  const welcome = await evaluate(text("view-wizard"));
  check("wizard links Vast and CivitAI terms", welcome.includes("Vast.ai terms") && welcome.includes("CivitAI terms"));
  await shot("w1-welcome");
  await next();
  check("step: Vast account", (await step()) === "vast-account");
  await shot("w2-vast-account");
  await next();
  check("step: Vast credit", (await step()) === "vast-credit");
  await shot("w3-vast-credit");
  await next();
  check("step: Vast key", (await step()) === "vast-key");
  check("Next is blocked until the Vast key is checked", await evaluate(`${q("wizard-next")}.disabled`));

  await evaluate(setValue("key-vast", "0".repeat(64)));
  await waitFor(async () => (await evaluate(text("key-vast-msg"))).includes("didn't accept"), 10000, "bad key message");
  check("a rejected Vast key is refused on paste", true);
  await evaluate(setValue("key-vast", FAKE_VAST));
  await waitFor(async () => (await evaluate(text("key-vast-msg"))).includes("Connected"), 10000, "vast ok");
  check("a good Vast key shows the credit", (await evaluate(text("key-vast-msg"))).includes("$11.55"));
  check("Next unlocks after the Vast key", !(await evaluate(`${q("wizard-next")}.disabled`)));
  await shot("w4-vast-key");
  await next();
  check("step: CivitAI account", (await step()) === "civitai-account");
  await next();
  check("step: CivitAI key", (await step()) === "civitai-key");
  const vis = await evaluate(text("civitai-visibility"));
  check("wizard says the CivitAI key is visible to the host", /host/.test(vis) && /see it/.test(vis));
  await evaluate(setValue("key-civitai", "bad" + "x".repeat(30)));
  await evaluate(`${q("key-civitai")}.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter'}))`);
  await waitFor(async () => (await evaluate(text("key-civitai-msg"))).includes("didn't accept"), 10000, "bad civitai");
  check("a rejected CivitAI key is refused", true);
  await evaluate(setValue("key-civitai", FAKE_CIVITAI));
  await evaluate(`${q("key-civitai")}.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter'}))`);
  await waitFor(async () => (await evaluate(text("key-civitai-msg"))).includes("mock-user"), 10000, "civitai ok");
  check("a good CivitAI key shows the account", true);
  await shot("w5-civitai-key");
  await next();
  check("step: done", (await step()) === "done");
  await next();
  check("wizard ends on the home screen", await evaluate(visible("view-home")));

  // Home: estimate, credit, catalog source.
  await waitFor(async () => (await evaluate(text("estimate"))).includes("Cheapest GPU"), 15000, "estimate");
  const est = await evaluate(text("estimate"));
  check("home shows the cheapest offer incl. download cost", est.includes("/hr") && est.includes("download"), est);
  check("no balance warning at $11.55", !(await evaluate(visible("gate"))));
  check("header shows the credit", (await evaluate(text("costbar"))).includes("$11.55"));
  await waitFor(async () => (await evaluate(text("catalog-info"))).includes("updated"), 15000, "online catalog");
  check("catalog fetched on launch", true);
  await shot("h1-home");

  // Settings: validation, save, LoRAs.
  await evaluate(click("nav-settings"));
  await sleep(300);
  await evaluate(`${q("s-max-dph")}.value = '99'`);
  await evaluate(`${q("settings-form")}.requestSubmit()`);
  await sleep(300);
  // The browser's own range check blocks 99 before it reaches Rust; bypass it
  // to make sure Rust validates too.
  const rustMsg = await evaluate(
    "window.__TAURI_INTERNALS__.invoke('save_settings', {form: {max_dph: 99, idle_minutes: 20, max_session_minutes: 240, min_credit: 1}}).then(() => 'saved', e => String(e))",
  );
  check("settings out of range are rejected by Rust", rustMsg.includes("between"), rustMsg);
  await evaluate(`${q("s-max-dph")}.value = '0.4'; ${q("s-idle")}.value = '30'`);
  await evaluate(`${q("settings-form")}.requestSubmit()`);
  await waitFor(async () => (await evaluate(text("settings-msg"))) === "Saved.", 5000, "saved");
  check("settings save", true);

  const addLora = async (link) => {
    await evaluate(`${q("lora-link")}.value = ${JSON.stringify(link)}`);
    await evaluate(click("lora-add"));
    await waitFor(async () => !(await evaluate(text("lora-msg"))).startsWith("Checking"), 10000, "lora add");
    return evaluate(text("lora-msg"));
  };
  check("LoRA link added", (await addLora("https://civitai.com/models/122359/detail-tweaker-xl")) === "Added.");
  const notLora = await addLora("https://civitai.com/models/122359?modelVersionId=2");
  check("a checkpoint link is refused as a LoRA", notLora.includes("not a LoRA"), notLora);
  check("a bad link is refused", (await addLora("https://example.com/x")).includes("civitai.com"));
  await addLora("https://civitai.com/models/122359?modelVersionId=3");
  const list = await evaluate(text("lora-list"));
  check("LoRA list shows both, with the SD 1.5 one flagged", list.includes("Mock LoRA 135867") && list.includes("Only used with SD 1.5"));
  await shot("s1-settings");

  // Upstream catalog edit, picked up without a rebuild.
  const edited = structuredClone(bundled);
  edited.models.push({ ...bundled.models[0], id: "upstream-test", name: "Upstream Test Model" });
  catalogText = JSON.stringify(edited);
  await evaluate(click("catalog-refresh"));
  await waitFor(async () => (await evaluate(`${q("model")}.options.length`)) === 2, 10000, "2 models");
  check("editing the upstream catalog changes the model list", (await evaluate(text("model"))).includes("Upstream Test Model"));

  await evaluate(click("nav-home"));
  await sleep(300);
  const loraSummary = await evaluate(text("lora-summary"));
  check("home lists the LoRAs that will load and those that won't",
    loraSummary.includes("With LoRA: Mock LoRA 135867") && loraSummary.includes("Not used with this model: Mock LoRA 3"),
    loraSummary);

  // Start -> cost bar -> Stop.
  await waitFor(async () => !(await evaluate(`${q("start")}.disabled`)), 15000, "start enabled");
  await evaluate(click("start"));
  await waitFor(async () => (await evaluate(text("status-text"))).startsWith("Downloading"), 30000, "downloading");
  await waitFor(async () => (await evaluate(text("costbar"))).includes("so far"), 30000, "cost bar");
  check("cost bar runs while provisioning", (await evaluate(text("costbar"))).includes("/hr"));
  await shot("h2-downloading");
  await waitFor(async () => await evaluate(visible("open")), 60000, "ready");
  await sleep(16000); // one cost-loop tick after Ready
  const bar = await evaluate(text("costbar"));
  check("cost bar shows $/hr, elapsed, spend, and credit left", /\/hr · \d+ min · ≈\$[\d.]+ so far · \$[\d.]+ left/.test(bar), bar);
  await shot("h3-ready");
  const titles = windowTitles(s.app.pid);
  check("Invoke window title shows the running cost", titles.some((t) => COST_TITLE.test(t)), titles.find((t) => t.includes("Invoke")) ?? JSON.stringify(titles));
  await evaluate(click("stop"));
  await waitFor(async () => (await evaluate(text("status-text"))) === "Ready to start.", 60000, "idle");
  check("Stop returns to idle", true);
  await closeIdle(s);
}

async function restart() {
  catalogUp = false; // upstream unreachable: the cached copy must be used
  const s = await launch();
  const { evaluate, shot } = s;
  check("keys survive a restart (no wizard)", (await evaluate(visible("view-home"))) && !(await evaluate(visible("view-wizard"))));
  check("cached catalog used when upstream is down", (await evaluate(`${q("model")}.options.length`)) === 2);
  check("catalog source says saved copy", (await evaluate(text("catalog-info"))).includes("saved copy"));
  const snap = await evaluate("window.__TAURI_INTERNALS__.invoke('get_snapshot')");
  check("settings survive a restart", snap.settings.max_dph === 0.4 && snap.settings.idle_minutes === 30);
  check("LoRAs survive a restart", snap.settings.loras.length === 2);
  await shot("r1-restart");
  await closeIdle(s);
  catalogUp = true;
}

async function lowBalance() {
  const s = await launch({ SLOPTWEAK_MOCK_CREDIT: "0.10" });
  const { evaluate, shot } = s;
  await waitFor(async () => await evaluate(visible("gate")), 15000, "gate");
  check("below the floor: refusal shown", (await evaluate(text("gate"))).includes("won't start below $1.00"));
  check("below the floor: Start disabled", await evaluate(`${q("start")}.disabled`));
  const forced = await evaluate("window.__TAURI_INTERNALS__.invoke('start_session', {modelId: 'banana-splitz-xxl'}).then(() => 'started', e => String(e))");
  check("below the floor: Rust refuses to start too", forced.includes("won't start"), forced);
  await shot("g1-refuse");
  // Floor 0: $0.10 covers less than an hour on the cheapest mock GPU -> warn.
  await evaluate("window.__TAURI_INTERNALS__.invoke('save_settings', {form: {max_dph: 0.4, idle_minutes: 30, max_session_minutes: 240, min_credit: 0}})");
  await evaluate(click("nav-settings"));
  await evaluate(click("nav-home"));
  await waitFor(async () => (await evaluate(text("gate"))).includes("covers only about"), 15000, "warn");
  check("under an hour of credit: warning, Start allowed", !(await evaluate(`${q("start")}.disabled`)));
  await shot("g2-warn");
  await evaluate("window.__TAURI_INTERNALS__.invoke('save_settings', {form: {max_dph: 0.4, idle_minutes: 30, max_session_minutes: 240, min_credit: 1}})");
  await closeIdle(s);
}

// Send WM_CLOSE to a top-level window, like clicking its X.
const CLOSE_PS1 = join(mkdtempSync(join(tmpdir(), "sloptweak-ps-")), "close.ps1");
writeFileSync(
  CLOSE_PS1,
  `param([string]$Title)
Add-Type -Name W -Namespace U -MemberDefinition @'
[DllImport("user32.dll")] public static extern System.IntPtr FindWindow(string c, string t);
[DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr h, uint m, System.IntPtr w, System.IntPtr l);
'@
$h = [U.W]::FindWindow([NullString]::Value, $Title)
if ($h -eq [System.IntPtr]::Zero) { exit 3 }
[void][U.W]::PostMessage($h, 0x10, [System.IntPtr]::Zero, [System.IntPtr]::Zero)
`,
);

function postClose(title) {
  execSync(`powershell -NoProfile -ExecutionPolicy Bypass -File "${CLOSE_PS1}" -Title "${title}"`);
}

async function closeWhileRunning() {
  const s = await launch();
  const { evaluate, shot } = s;
  await waitFor(async () => !(await evaluate(`${q("start")}.disabled`)), 15000, "start enabled");
  await evaluate(click("start"));
  await waitFor(async () => await evaluate(visible("stop")), 30000, "running");
  postClose("SlopTweak");
  await waitFor(async () => await evaluate(visible("modal")), 10000, "confirm dialog");
  await shot("c1-confirm-close");
  check("closing while active asks first", true);
  await evaluate(click("modal-cancel"));
  await sleep(500);
  check("cancel keeps the session", await evaluate(visible("stop")));
  postClose("SlopTweak");
  await waitFor(async () => await evaluate(visible("modal")), 10000, "confirm dialog again");
  await evaluate(click("modal-ok"));
  s.ws.close();
  const exited = await waitFor(async () => s.app.exitCode !== null, 60000, "app exit").catch(() => false);
  check("confirm destroys and exits", !!exited, `exit=${s.app.exitCode}`);
  check("log shows instance destroyed before exit", /instance \d+ destroyed/.test(s.app.output()));
}

try {
  start("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true });
  await waitFor(async () => (await fetch("http://127.0.0.1:1420/")).ok, 30000, "vite");
  await wizardAndHome();
  await restart();
  await lowBalance();
  await closeWhileRunning();
} catch (e) {
  check("harness", false, String(e));
} finally {
  for (const p of procs.reverse()) kill(p);
  catalogServer.close();
}
const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} passed`);
process.exit(failed ? 1 : 0);
