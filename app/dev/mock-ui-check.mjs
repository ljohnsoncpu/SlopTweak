// Drive the launcher UI against MockProvider + MockCivitai ($0, no network
// besides a local catalog server). Covers Phase 2 and 3:
//   fresh mock profile -> wizard (key checks on paste) -> home estimate ->
//   settings + validation -> LoRAs -> upstream catalog edit without rebuild ->
//   Start -> cost bar -> image sync line + buttons -> Stop saves the rest ->
//   restart (keys, settings, cached catalog survive) -> low-balance gate ->
//   update offer (refused while a GPU runs) + Copy diagnostics (redacted) ->
//   close-while-running confirm + last sync + destroy.
// Mock mode uses its own Credential Manager service ("SlopTweak-mock") and
// its own folders, so the real profile is never touched.
//
// Usage (from app/):  node dev/mock-ui-check.mjs
// Needs a debug build. Starts vite on :1420 itself. Screenshots go to $SHOTS;
// GUIDE_SHOTS=1 hides the MOCK badge so they can go in docs/.

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";
import { COST_TITLE, windowTitles } from "./win-titles.mjs";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const CDP_PORT = 9334;
const CATALOG_PORT = 18431;
// Mock mode's default output folder (fake 1-pixel images, kept out of Pictures).
const MOCK_OUT = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher", "mock", "output");
const filesIn = (dir) => (existsSync(dir) ? readdirSync(dir, { withFileTypes: true }).filter((d) => d.isFile()).length : 0);
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
    if (process.env.GUIDE_SHOTS === "1") {
      await send("Runtime.evaluate", { expression: "document.getElementById('mode').hidden = true" });
    }
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
      // After Ready, MockSidecar makes 6 gallery images (one per 3 s), plus
      // 3 Canvas tries and 3 scratch intermediates.
      SLOPTWEAK_MOCK_IMAGES: "6",
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
  await waitFor(async () => (await evaluate(`${q("model")}.options.length`)) === bundled.models.length + 1, 10000, "one more model");
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
  const syncLine = await evaluate(text("sync"));
  check("home screen shows images being saved", /^Saved \d+ images? .*to .*mock.output\.$/.test(syncLine), syncLine);
  check("Ready shows Show tutorial and Open output folder", (await evaluate(visible("tutorial"))) && (await evaluate(visible("open-folder"))));
  await evaluate(click("stop"));
  await waitFor(async () => (await evaluate(text("status-text"))) === "Ready to start.", 60000, "idle");
  check("Stop returns to idle", true);
  const done = await evaluate(text("sync"));
  check("after Stop: all fake images saved", done.startsWith("Saved 6 images and 3 Canvas tries"), done);
  check("files on disk: 6 gallery, 3 Canvas", filesIn(MOCK_OUT) === 6 && filesIn(join(MOCK_OUT, "Canvas")) === 3, `${filesIn(MOCK_OUT)} + ${filesIn(join(MOCK_OUT, "Canvas"))}`);
  await shot("h4-stopped-saved");
  await closeIdle(s);
}

async function restart() {
  catalogUp = false; // upstream unreachable: the cached copy must be used
  const s = await launch();
  const { evaluate, shot } = s;
  check("keys survive a restart (no wizard)", (await evaluate(visible("view-home"))) && !(await evaluate(visible("view-wizard"))));
  check("cached catalog used when upstream is down", (await evaluate(`${q("model")}.options.length`)) === bundled.models.length + 1);
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

async function updateAndDiagnostics() {
  const s = await launch({ SLOPTWEAK_MOCK_UPDATE: "9.9.9" });
  const { evaluate, shot } = s;
  await waitFor(async () => await evaluate(visible("update")), 15000, "update banner");
  const offer = await evaluate(text("update-text"));
  check("launch check offers a newer version", offer.includes("9.9.9") && offer.includes("you have 0.2.0"), offer);
  await shot("u1-update-offer");
  await evaluate(click("nav-settings"));
  await sleep(300);
  check("settings shows the app version", (await evaluate(text("about-version"))) === "SlopTweak 0.2.0");
  await evaluate(click("update-check"));
  await waitFor(async () => (await evaluate(text("about-msg"))).includes("available"), 10000, "manual check");
  check("Check for updates finds it too", true);
  await shot("s2-about");
  await evaluate(click("nav-home"));
  await sleep(300);

  await waitFor(async () => !(await evaluate(`${q("start")}.disabled`)), 15000, "start enabled");
  await evaluate(click("start"));
  await waitFor(async () => await evaluate(visible("open")), 60000, "ready");
  check("Update now is disabled while a GPU runs", await evaluate(`${q("update-install")}.disabled`));
  const forced = await evaluate("window.__TAURI_INTERNALS__.invoke('install_update').then(() => 'installed', e => String(e))");
  check("Rust refuses to install an update while a GPU runs", forced.includes("Stop the GPU"), forced);

  const diag = (await evaluate("window.__TAURI_INTERNALS__.invoke('copy_diagnostics')")).text;
  const user = process.env.USERNAME ?? "";
  check("diagnostics: version, history, and log", diag.includes("app 0.2.0 (mock mode)") && diag.includes("== state history ==") && /Provisioning -> Ready: instance \d+/.test(diag) && diag.includes("== log =="));
  check("diagnostics: no keys", !diag.includes(FAKE_VAST) && !diag.includes(FAKE_CIVITAI));
  check("diagnostics: no token-shaped strings", !/[A-Za-z0-9_-]{40,}/.test(diag), diag.match(/[A-Za-z0-9_-]{40,}/)?.[0]);
  check("diagnostics: no Windows user name", user.length < 3 || !diag.toLowerCase().includes(`\\users\\${user.toLowerCase()}`));
  check("diagnostics: output folder shown under %USERPROFILE%", diag.includes("%USERPROFILE%"));
  await evaluate("document.querySelector('details.card').open = true");
  await evaluate(click("diag-copy"));
  await waitFor(async () => (await evaluate(text("diag-msg"))).length > 0 && !(await evaluate(text("diag-msg"))).startsWith("Collecting"), 10000, "copy msg");
  const copyMsg = await evaluate(text("diag-msg"));
  check("Copy diagnostics puts the report on the clipboard", copyMsg.startsWith("Copied"), copyMsg);
  const clip = execSync("powershell -NoProfile -Command Get-Clipboard -Raw", { encoding: "utf8" });
  check("clipboard holds the redacted report", clip.includes("SlopTweak diagnostics") && !clip.includes(FAKE_VAST));
  await shot("d1-diagnostics");

  await evaluate(click("stop"));
  await waitFor(async () => (await evaluate(text("status-text"))) === "Ready to start.", 60000, "idle");
  check("Update now is enabled again after Stop", !(await evaluate(`${q("update-install")}.disabled`)));
  await evaluate(click("update-later"));
  await sleep(200);
  check("Later hides the update offer", !(await evaluate(visible("update"))));
  await evaluate("window.__TAURI_INTERNALS__.invoke('check_update')");
  await evaluate(click("update-install"));
  await waitFor(async () => (await evaluate(text("update-msg"))).startsWith("Installed"), 10000, "mock install");
  check("mock update installs once idle", /mock: would install 9\.9\.9/.test(s.app.output()));
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
  await waitFor(async () => await evaluate(visible("open")), 60000, "ready");
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
  const out = s.app.output();
  check("closing ran the last image sync before the destroy", out.indexOf("final sync:") >= 0 && out.indexOf("final sync:") < out.search(/instance \d+ destroyed/));
}

try {
  start("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true });
  await waitFor(() => servesThisCheckout(APP), 30000, "vite");
  await wizardAndHome();
  await restart();
  await lowBalance();
  await updateAndDiagnostics();
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
