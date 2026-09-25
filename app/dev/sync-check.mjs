// Local, $0 end-to-end check of Phase 4: output sync and the tutorial.
//
// The real app (debug build, mock provider) talks to the real
// instance/sidecar.py through HttpSidecar, in front of dev/fake_invoke.py.
// Nothing is rented. Checks:
//   - Start -> Ready -> the Invoke window opens with the tutorial overlay
//   - tutorial: upload of the sample (as an asset), every step, auto-advance
//     when a generation finishes, Done -> app records it (blocked navigation)
//   - the overlay stays closed after that; "Show tutorial" reopens it
//   - gallery images and Canvas tries are saved while running; scratch
//     intermediates and the sample are not
//   - images made just before Stop are on disk after it; the session ends
//
// Usage (from app/):  node dev/sync-check.mjs
// Needs: a debug build (cargo build), uv, python, node >= 22. Uses the mock
// profile (%APPDATA%\com.sloptweak.launcher\mock); its settings.json is backed
// up and restored.

import { spawn, execSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { servesThisCheckout } from "./vite-check.mjs";

const APP = resolve(import.meta.dirname, "..");
const ROOT = resolve(APP, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const SIDECAR_PORT = 18080;
const INVOKE_PORT = 9090; // sidecar.py's upstream is fixed to 127.0.0.1:9090
const MAIN_PORT = 9334;
const REMOTE_PORT = 9333;
const ORIGIN = `http://127.0.0.1:${SIDECAR_PORT}`;
const FAKE = `http://127.0.0.1:${INVOKE_PORT}`;
const MOCK_CONFIG = join(process.env.APPDATA ?? "", "com.sloptweak.launcher", "mock");
const SETTINGS = join(MOCK_CONFIG, "settings.json");

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

function cleanup() {
  for (const p of procs.reverse()) {
    try {
      execSync(`taskkill /T /F /PID ${p.pid}`, { stdio: "ignore" });
    } catch {
      /* already gone */
    }
  }
}

function check(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
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
    await sleep(400);
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
    eval: async (expression) => {
      const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description ?? "eval failed");
      return r.result?.result?.value;
    },
  };
}

const fake = (path, method = "GET") => fetch(FAKE + path, { method }).then((r) => r.json());
const files = (dir) => (existsSync(dir) ? readdirSync(dir, { withFileTypes: true }).filter((d) => d.isFile()).map((d) => d.name) : []);

// In the remote page: the overlay's shadow root.
const TUT = "document.querySelector('sloptweak-tutorial')?.shadowRoot";
const cardText = (r) => r.eval(`${TUT}?.querySelector('.card, .pill')?.textContent ?? ''`);
const clickAct = (r, act) =>
  r.eval(`(() => { const b = ${TUT}?.querySelector('[data-act="${act}"]'); if (!b) return false; b.click(); return true; })()`);

async function main() {
  if (!existsSync(EXE)) throw new Error(`build first: ${EXE}`);
  const out = mkdtempSync(join(tmpdir(), "sloptweak-sync-"));
  const status = join(out, "..", `sloptweak-status-${process.pid}.json`);
  writeFileSync(status, JSON.stringify({ stage: "ready" }));

  // Throwaway secret for this local run only; never a real credential.
  const secret = randomBytes(32).toString("base64url");
  const hash = createHash("sha256").update(secret).digest("hex");

  mkdirSync(MOCK_CONFIG, { recursive: true });
  const backup = existsSync(SETTINGS) ? readFileSync(SETTINGS, "utf8") : null;
  writeFileSync(SETTINGS, JSON.stringify({ output_dir: out, model_id: "anima-aesthetic", tutorial_done: false }));
  try {
    start("python", [join(APP, "dev", "fake_invoke.py"), String(INVOKE_PORT)]);
    const sidecar = start("uv", ["run", "--project", "instance", "python", "instance/sidecar.py"], {
      cwd: ROOT,
      env: { ...process.env, LAUNCH_TOKEN_HASH: hash, SIDECAR_PORT: String(SIDECAR_PORT), SLOPTWEAK_STATUS_FILE: status, HEARTBEAT_MINUTES: "60" },
    });
    await waitFor(async () => (await fetch(`${FAKE}/__fake/state`)).ok, 20000, "fake invoke");
    await waitFor(
      async () => (await fetch(`${ORIGIN}/__heartbeat`, { method: "POST", headers: { Authorization: `Bearer ${secret}` } })).ok,
      30000,
      "sidecar",
    ).catch((e) => {
      console.log(sidecar.output());
      throw e;
    });
    start("npx", ["vite", "--port", "1420", "--strictPort"], { cwd: APP, shell: true });
    await waitFor(() => servesThisCheckout(APP), 30000, "vite");

    const app = start(EXE, [], {
      env: {
        ...process.env,
        SLOPTWEAK_PROVIDER: "mock",
        SLOPTWEAK_MOCK_REMOTE: `${ORIGIN}/`,
        SLOPTWEAK_MOCK_SIDECAR: "http",
        SLOPTWEAK_DEV_LAUNCH_SECRET: secret,
        SLOPTWEAK_MAIN_DEBUG_PORT: String(MAIN_PORT),
        SLOPTWEAK_REMOTE_DEBUG_PORT: String(REMOTE_PORT),
      },
    });
    const m = await cdp(MAIN_PORT, (u) => u.includes("localhost:1420")).catch((e) => {
      console.log(app.output());
      throw e;
    });
    await waitFor(async () => (await m.eval("document.getElementById('model').options.length")) > 0, 30000, "UI");
    const ipc = (cmd, args = {}) => m.eval(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)})`);
    await ipc("set_secret", { which: "vast", value: "ab".repeat(32) });
    await ipc("set_secret", { which: "civitai", value: "c".repeat(32) });
    await m.eval("location.reload()");
    await sleep(1500);
    await waitFor(async () => (await m.eval("document.getElementById('start').disabled")) === false, 30000, "Start enabled");
    await m.eval("document.getElementById('start').click()");
    await waitFor(async () => (await ipc("get_snapshot")).state.kind === "ready", 120000, "ready").catch(async (e) => {
      const snap = await ipc("get_snapshot");
      console.log(JSON.stringify(snap.state), "\n", snap.log.slice(-15).join("\n"));
      throw e;
    });
    check("mock session reaches Ready through the real sidecar", true);

    // ----- tutorial -----
    const r = await cdp(REMOTE_PORT, (u) => u.startsWith(ORIGIN));
    await waitFor(async () => (await r.eval("document.title")) === "FAKE INVOKE", 20000, "Invoke page");
    // A real GPU is a new tunnel origin every time; this local origin isn't,
    // so forget the previous run's tutorial state.
    if (await r.eval("localStorage.length > 0")) {
      await r.eval("localStorage.clear(); sessionStorage.clear(); location.reload(); true");
      await sleep(2000);
    }
    const intro = await waitFor(async () => (await cardText(r)) || false, 10000, "tutorial card");
    check("tutorial shows on its own the first time", intro.includes("Welcome to Invoke"), intro.slice(0, 60));
    await clickAct(r, "next");
    await waitFor(async () => (await cardText(r)).includes("Open the sample picture"), 15000, "step 2 after reload");
    const state = await fake("/__fake/state");
    const uploads = state.images.filter((i) => i.image_category === "user");
    check("sample uploaded to Invoke as an asset", uploads.length === 1 && !uploads[0].is_intermediate, JSON.stringify(uploads.map((i) => i.image_category)));
    const thumbOk = await r.eval(`(() => { const i = ${TUT}.querySelector('img'); return !!i && i.complete && i.naturalWidth > 0; })()`);
    check("step 2 shows the sample's thumbnail", thumbOk);
    await clickAct(r, "next");
    await waitFor(async () => (await cardText(r)).includes("Paint over the vase"), 5000, "step 3");
    await clickAct(r, "next");
    await waitFor(async () => (await cardText(r)).includes("Say what goes there"), 5000, "step 4");
    await sleep(2500); // let it read the queue baseline
    await fake("/__fake/generate?canvas=1", "POST");
    const t0 = Date.now();
    await waitFor(async () => (await cardText(r)).includes("Keep the one you like"), 10000, "auto-advance");
    check("a finished generation moves the tutorial on", true, `${Date.now() - t0} ms`);
    await clickAct(r, "next"); // Done
    await sleep(1500);
    check("Done doesn't navigate the page away", (await r.eval("location.pathname")) === "/" && (await r.eval("document.title")) === "FAKE INVOKE");
    check("overlay closed after Done", (await cardText(r)) === "");
    const saved = await waitFor(() => JSON.parse(readFileSync(SETTINGS, "utf8")).tutorial_done === true, 5000, "tutorial_done").catch(() => false);
    check("app recorded the tutorial as done", saved === true);
    await r.eval("location.reload()");
    await sleep(2500);
    check("overlay stays closed after a reload", (await cardText(r)) === "");
    await m.eval("document.getElementById('tutorial').click()");
    const again = await waitFor(async () => (await cardText(r)) || false, 5000, "reopened").catch(() => "");
    check("Show tutorial reopens it", again.includes("Welcome to Invoke"), again.slice(0, 40));
    await clickAct(r, "skip");
    await sleep(1000);
    check("× closes it again", (await cardText(r)) === "");
    r.ws.close();

    // ----- sync -----
    await fake("/__fake/generate?gallery=10&canvas=2", "POST");
    const canvasDir = join(out, "Canvas");
    await waitFor(() => files(out).length >= 10 && files(canvasDir).length >= 3, 25000, "periodic sync").catch(() => false);
    check("images are saved while the GPU runs", files(out).length === 10 && files(canvasDir).length === 3, `${files(out).length} gallery, ${files(canvasDir).length} canvas`);
    const line = await m.eval("document.getElementById('sync').textContent");
    check("home screen says what was saved", line.includes("Saved 10 images and 3 Canvas tries"), line);

    await fake("/__fake/generate?gallery=3", "POST");
    await m.eval("document.getElementById('stop').click()");
    await waitFor(async () => (await ipc("get_snapshot")).state.kind === "idle", 120000, "idle after Stop");
    const g = files(out);
    check("images made right before Stop are on disk", g.length === 13, `${g.length} gallery files`);
    check("Canvas tries in their own folder; scratch and sample skipped", files(canvasDir).length === 3 && !g.some((f) => f.endsWith(".jpg")));
    const snap = await ipc("get_snapshot");
    check("sync report: done, nothing missing", snap.sync?.phase === "done" && snap.sync?.missing === 0, JSON.stringify(snap.sync));
    const after = await m.eval("document.getElementById('sync').textContent");
    check("after Stop the home screen still says what was saved", after.includes("Saved 13 images"), after);
    const log = await m.eval("document.getElementById('log').textContent");
    check("launch secret never logged", !log.includes(secret));
    m.ws.close();
  } finally {
    if (backup !== null) writeFileSync(SETTINGS, backup);
    else rmSync(SETTINGS, { force: true });
    rmSync(status, { force: true });
  }
  rmSync(out, { recursive: true, force: true });
}

try {
  await main();
} catch (e) {
  check("harness", false, String(e));
} finally {
  cleanup();
}
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
process.exit(failed.length ? 1 : 0);
