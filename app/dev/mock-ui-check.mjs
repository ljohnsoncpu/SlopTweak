// Drive the launcher UI against MockProvider ($0): Start -> Ready -> Stop,
// then Start again and close the window to exercise the confirm-and-destroy
// path. Screenshots go to $SHOTS (default: a temp dir).
//
// Usage (from app/):  node dev/mock-ui-check.mjs
// Needs a debug build. Starts vite on :1420 itself.

import { spawn, execSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const APP = resolve(import.meta.dirname, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const CDP_PORT = 9334;
const SHOTS = process.env.SHOTS ?? mkdtempSync(join(tmpdir(), "sloptweak-shots-"));
const procs = [];
const results = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function start(cmd, args, opts = {}) {
  const p = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], windowsHide: true, ...opts });
  let out = "";
  p.stdout.on("data", (d) => (out += d));
  p.stderr.on("data", (d) => (out += d));
  p.output = () => out;
  procs.push(p);
  return p;
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

const text = (id) => `document.getElementById(${JSON.stringify(id)}).textContent`;
const visible = (id) => `!document.getElementById(${JSON.stringify(id)}).hidden`;

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

async function main() {
  start("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true });
  await waitFor(async () => (await fetch("http://localhost:1420/")).ok, 30000, "vite");
  const app = start(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "mock",
      SLOPTWEAK_DEV_IMPORT_KEYS: "1",
      SLOPTWEAK_MAIN_DEBUG_PORT: String(CDP_PORT),
    },
  });
  const { ws, evaluate, shot } = await connect();
  // main.ts has loaded once the model list is filled from get_snapshot.
  await waitFor(async () => (await evaluate("document.getElementById('model').options.length")) > 0, 30000, "ui");
  await sleep(1500);
  await shot("01-idle");

  const snap = await evaluate("window.__TAURI_INTERNALS__.invoke('get_snapshot').then(s => s.mode)");
  check("main window can call app commands", snap === "mock", `mode=${snap}`);
  check("mock badge shown", await evaluate(visible("mode")));
  check("Start enabled", await evaluate("!document.getElementById('start').disabled"));

  await evaluate("document.getElementById('start').click()");
  await waitFor(async () => (await evaluate(text("status-text"))).startsWith("Downloading"), 30000, "downloading").catch(async (e) => {
    console.log("status:", await evaluate(text("status-text")), "|", await evaluate(text("status-sub")));
    console.log("log:", await evaluate(text("log")));
    throw e;
  });
  await shot("02-downloading");
  check("progress bar shown while downloading", await evaluate(visible("progress")));
  await waitFor(async () => await evaluate(visible("open")), 60000, "ready");
  await sleep(500);
  await shot("03-ready");
  check("Ready shows cost line", (await evaluate(text("status-sub"))).includes("/hr"));

  await evaluate("document.getElementById('stop').click()");
  await waitFor(async () => (await evaluate(text("status-text"))) === "Ready to start.", 60000, "idle");
  check("Stop returns to idle", true);
  await shot("04-stopped");

  // Close while running: confirm dialog, then destroy and exit.
  await evaluate("document.getElementById('start').click()");
  await waitFor(async () => (await evaluate(text("status-text"))).includes("GPU machine") ||
    (await evaluate(text("status-text"))).startsWith("Starting") ||
    (await evaluate(text("status-text"))).startsWith("Downloading"), 30000, "provisioning");
  postClose("SlopTweak");
  await waitFor(async () => await evaluate(visible("modal")), 10000, "confirm dialog");
  await shot("05-confirm-close");
  check("closing while active asks first", true);
  await evaluate("document.getElementById('modal-cancel').click()");
  await sleep(500);
  check("cancel keeps the session", (await evaluate(`${visible("stop")}`)) === true);
  postClose("SlopTweak");
  await waitFor(async () => await evaluate(visible("modal")), 10000, "confirm dialog again");
  await evaluate("document.getElementById('modal-ok').click()");
  ws.close();
  const exited = await waitFor(async () => app.exitCode !== null, 60000, "app exit").catch(() => false);
  check("confirm destroys and exits", !!exited, `exit=${app.exitCode}`);
  const log = app.output();
  check("log shows instance destroyed before exit", /instance \d+ destroyed/.test(log));
}

try {
  await main();
} catch (e) {
  check("harness", false, String(e));
} finally {
  for (const p of procs.reverse()) {
    try {
      execSync(`taskkill /T /F /PID ${p.pid}`, { stdio: "ignore" });
    } catch {
      /* gone */
    }
  }
}
const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} passed`);
process.exit(failed ? 1 : 0);
