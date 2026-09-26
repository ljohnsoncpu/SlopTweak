// Local, $0 end-to-end check of output sync (Phase 4) and the tutorial
// (Phase 6, v2).
//
// The real app (debug build, mock provider) talks to the real
// instance/sidecar.py through HttpSidecar, in front of dev/fake_invoke.py.
// Nothing is rented. Checks:
//   - Start -> Ready -> the Invoke window opens with the tutorial overlay
//   - tutorial: all five stages, the fixed prompt verbatim, auto-advance
//     when a generation finishes, the app saving each stage (blocked
//     navigation), the fallback portrait uploaded as a WebP asset
//   - skip at stage 5, then a new session (a fresh window, like a new GPU)
//     resumes there: "Continue stage 5", the set-up step, Done recorded
//   - the overlay stays closed after that; "Show tutorial" reopens it
//   - gallery images and Canvas tries are saved while running; scratch
//     intermediates and the uploaded portrait are not
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
// Where the card is: "stage.step" (0.0 is the home card), "" if none.
const where = (r) => r.eval(`(() => { const c = ${TUT}?.querySelector('.card'); return c ? c.dataset.stage + '.' + c.dataset.step : ''; })()`);
const at = (r, pos, what) => waitFor(async () => (await where(r)) === pos, 10000, `${what} (${pos})`);
const saved = () => JSON.parse(readFileSync(SETTINGS, "utf8"));
const savedStage = (n) => waitFor(() => saved().tutorial_stage === n, 5000, `tutorial_stage ${n}`).then(() => true, () => false);
// The fixed prompt (PLAN §4 Phase 6), verbatim.
const POSITIVE =
  "a woman standing in front of a white background,\n" +
  "solo, female, human, white background, black tank top, black jeans, red eyes, red hair, " +
  "simple background, front view, forehead, standing, medium shot, smile";

async function startSession(m, ipc) {
  await waitFor(async () => (await m.eval("document.getElementById('start').disabled")) === false, 30000, "Start enabled");
  await m.eval("document.getElementById('start').click()");
  await waitFor(async () => (await ipc("get_snapshot")).state.kind === "ready", 120000, "ready").catch(async (e) => {
    const snap = await ipc("get_snapshot");
    console.log(JSON.stringify(snap.state), "\n", snap.log.slice(-15).join("\n"));
    throw e;
  });
}

async function remotePage() {
  const r = await cdp(REMOTE_PORT, (u) => u.startsWith(ORIGIN));
  await waitFor(async () => (await r.eval("document.title")) === "FAKE INVOKE", 20000, "Invoke page");
  return r;
}

/** A finished generation moves a `watch` step on by itself. */
async function generateMovesOn(r, kind, from, to) {
  await sleep(2500); // let it read the queue baseline
  await fake(`/__fake/generate?${kind}=1`, "POST");
  const t0 = Date.now();
  const ok = await at(r, to, "auto-advance").then(() => true, () => false);
  check(`a finished generation moves ${from} on to ${to}`, ok, `${Date.now() - t0} ms`);
}

/** "Use ours instead": upload, reload, thumbnail. Returns the upload count. */
async function useOurs(r, pos) {
  const before = (await fake("/__fake/state")).images.filter((i) => i.image_category === "user").length;
  await clickAct(r, "ours");
  await sleep(2500); // it reloads the page once
  await at(r, pos, "same step after the reload");
  const ups = (await fake("/__fake/state")).images.filter((i) => i.image_category === "user");
  const last = ups[ups.length - 1];
  check(`${pos}: Use ours uploads the portrait as a user asset`, ups.length === before + 1 && !last.is_intermediate, `${ups.length} upload(s)`);
  const img = await waitFor(
    () => r.eval(`(() => { const i = ${TUT}.querySelector('img'); return i && i.complete && i.naturalWidth ? [i.naturalWidth, i.naturalHeight] : null; })()`),
    5000,
    "thumbnail",
  ).catch(() => null);
  check(`${pos}: the portrait decodes (832x1216 WebP)`, img?.[0] === 832 && img?.[1] === 1216, JSON.stringify(img));
  check(`${pos}: the Use ours button is gone once it's there`, !(await r.eval(`!!${TUT}.querySelector('[data-act="ours"]')`)));
}

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
    await startSession(m, ipc);
    check("mock session reaches Ready through the real sidecar", true);

    // ----- tutorial -----
    let r = await remotePage();
    // A real GPU is a new tunnel origin every time; this local origin isn't,
    // so forget the previous run's tutorial state.
    if (await r.eval("localStorage.length > 0")) {
      await r.eval("localStorage.clear(); sessionStorage.clear(); location.reload(); true");
      await sleep(2000);
    }
    const intro = await waitFor(async () => (await cardText(r)) || false, 10000, "tutorial card");
    check("tutorial shows on its own the first time", intro.includes("Welcome to Invoke") && (await where(r)) === "0.0", intro.slice(0, 60));
    check("home card lists five stages", (await r.eval(`${TUT}.querySelectorAll('[data-act="jump"]').length`)) === 5);

    // Stage 1: prompting.
    await clickAct(r, "start");
    await at(r, "1.0", "stage 1");
    const pre = await r.eval(`${TUT}.querySelector('pre')?.textContent ?? ''`);
    check("stage 1 shows the fixed prompt verbatim", pre === POSITIVE);
    const s1 = await cardText(r);
    check("stage 1 names Width 832 and Height 1216", s1.includes("Width") && s1.includes("832") && s1.includes("Height") && s1.includes("1216"));
    await clickAct(r, "next");
    await at(r, "1.1", "negative prompt step");
    check("stage 1 teaches the nsfw negative", (await cardText(r)).includes("Add Negative Prompt") && (await cardText(r)).includes("nsfw"));
    await generateMovesOn(r, "gallery", "1.1", "1.2");
    check("model page button shown (catalog page known)", await r.eval(`!!${TUT}.querySelector('[data-act="page"]')`));

    // Stage 2: prompt templates.
    await clickAct(r, "next");
    await at(r, "2.0", "stage 2");
    check("app saved stage 2", await savedStage(2));
    await clickAct(r, "next");
    check("stage 2 names Create Prompt Template and {prompt}", (await cardText(r)).includes("Create Prompt Template") && (await cardText(r)).includes("{prompt}"));
    await clickAct(r, "next");
    await clickAct(r, "next");

    // Stage 3: to the canvas, with the fallback.
    await at(r, "3.0", "stage 3");
    check("app saved stage 3", await savedStage(3));
    check("stage 3 names New Canvas from Image → As Raster Layer (Resize)", (await cardText(r)).includes("New Canvas from Image → As Raster Layer (Resize)"));
    await useOurs(r, "3.0");
    check("stage 3 points at Assets after Use ours", (await cardText(r)).includes("Assets"));
    await clickAct(r, "next");
    await at(r, "3.1", "stage 3 explainer");

    // Stage 4: the eye fix (no set-up step when coming from stage 3).
    await clickAct(r, "next");
    await at(r, "4.0", "stage 4");
    check("app saved stage 4", await savedStage(4));
    check("stage 4 from stage 3 skips the set-up step", (await cardText(r)).includes("Mask the eyes"));
    await clickAct(r, "next");
    check("stage 4 names Shift+B (Fit Bbox To Masks)", (await cardText(r)).includes("Shift+B") && (await cardText(r)).includes("Fit Bbox To Masks"));
    await clickAct(r, "next");
    check("stage 4 names Scale Before Processing: Auto", (await cardText(r)).includes("Scale Before Processing") && (await cardText(r)).includes("Auto"));
    await clickAct(r, "next");
    await at(r, "4.3", "low denoise step");
    await generateMovesOn(r, "canvas", "4.3", "4.4");
    await generateMovesOn(r, "canvas", "4.4", "4.5");
    await clickAct(r, "back");
    await at(r, "4.4", "Back");
    await clickAct(r, "next");
    await clickAct(r, "next");

    // Stage 5: the visor. Skip here; the next session resumes.
    await at(r, "5.0", "stage 5");
    check("app saved stage 5", await savedStage(5));
    await clickAct(r, "min");
    check("minimize keeps the stage", (await cardText(r)).includes("stage 5 of 5"));
    await clickAct(r, "restore");
    await at(r, "5.0", "restored");
    await clickAct(r, "skip");
    await sleep(1500);
    check("× doesn't navigate the page away", (await r.eval("location.pathname")) === "/" && (await r.eval("document.title")) === "FAKE INVOKE");
    check("overlay closed after ×", (await cardText(r)) === "");
    const sk = await waitFor(() => saved().tutorial_done === true, 5000, "tutorial_done").then(() => saved(), () => saved());
    check("skip is recorded and keeps the stage", sk.tutorial_done === true && sk.tutorial_stage === 5, JSON.stringify({ d: sk.tutorial_done, s: sk.tutorial_stage }));
    r.ws.close();

    // ----- sync -----
    // The tutorial above made 1 gallery image (stage 1) and 2 Canvas tries
    // (stage 4); they're synced too.
    const [G0, C0] = [1, 2];
    await fake("/__fake/generate?gallery=10&canvas=2", "POST");
    const canvasDir = join(out, "Canvas");
    await waitFor(() => files(out).length >= G0 + 10 && files(canvasDir).length >= C0 + 2, 25000, "periodic sync").catch(() => false);
    check("images are saved while the GPU runs", files(out).length === G0 + 10 && files(canvasDir).length === C0 + 2, `${files(out).length} gallery, ${files(canvasDir).length} canvas`);
    const line = await m.eval("document.getElementById('sync').textContent");
    check("home screen says what was saved", line.includes(`Saved ${G0 + 10} images and ${C0 + 2} Canvas tries`), line);

    await fake("/__fake/generate?gallery=3", "POST");
    await m.eval("document.getElementById('stop').click()");
    await waitFor(async () => (await ipc("get_snapshot")).state.kind === "idle", 120000, "idle after Stop");
    const g = files(out);
    check("images made right before Stop are on disk", g.length === G0 + 13, `${g.length} gallery files`);
    check("Canvas tries in their own folder; scratch and the portrait skipped", files(canvasDir).length === C0 + 2 && !g.some((f) => f.endsWith(".webp")));
    const snap = await ipc("get_snapshot");
    check("sync report: done, nothing missing", snap.sync?.phase === "done" && snap.sync?.missing === 0, JSON.stringify(snap.sync));
    const after = await m.eval("document.getElementById('sync').textContent");
    check("after Stop the home screen still says what was saved", after.includes(`Saved ${G0 + 13} images`), after);
    const log = await m.eval("document.getElementById('log').textContent");
    check("launch secret never logged", !log.includes(secret));

    // ----- tutorial: resume on the next session -----
    // A new session opens a new window with the saved stage baked in.
    // Clearing this origin's storage makes it look like a new GPU.
    await startSession(m, ipc);
    r = await remotePage();
    await sleep(1500);
    check("after a skip it doesn't show by itself", (await cardText(r)) === "");
    await r.eval("localStorage.clear(); sessionStorage.clear(); location.reload(); true");
    await sleep(2500);
    check("a new GPU with the tutorial skipped: still closed", (await cardText(r)) === "");
    await m.eval("document.getElementById('tutorial').click()");
    const back = await waitFor(async () => (await cardText(r)) || false, 10000, "resume card").catch(() => "");
    check("Show tutorial on a new GPU offers the saved stage", back.includes("Welcome back") && back.includes("Continue stage 5"), back.slice(0, 60));
    await clickAct(r, "start");
    await at(r, "5.0", "stage 5 resumed");
    check("resumed stage starts with the set-up step", (await cardText(r)).includes("Get the picture on the canvas"));
    check("set-up step repeats the prompt copy", await r.eval(`!!${TUT}.querySelector('[data-copy="positive"]') && !!${TUT}.querySelector('[data-copy="negative"]')`));
    await useOurs(r, "5.0");
    for (const pos of ["5.1", "5.2", "5.3", "5.4"]) {
      await clickAct(r, "next");
      await at(r, pos, "stage 5 step");
    }
    await generateMovesOn(r, "canvas", "5.4", "5.5");
    check("last card says Accept and Save Canvas To Gallery", (await cardText(r)).includes("Accept") && (await cardText(r)).includes("Save Canvas To Gallery"));
    await clickAct(r, "next"); // Done
    await sleep(1500);
    check("Done doesn't navigate the page away", (await r.eval("location.pathname")) === "/");
    check("overlay closed after Done", (await cardText(r)) === "");
    const dn = await waitFor(() => saved().tutorial_done === true && saved().tutorial_stage === 1, 5000, "done").then(() => true, () => false);
    check("app recorded Done and resets the stage to 1", dn, JSON.stringify({ d: saved().tutorial_done, s: saved().tutorial_stage }));
    await r.eval("location.reload()");
    await sleep(2500);
    check("overlay stays closed after a reload", (await cardText(r)) === "");
    await m.eval("document.getElementById('tutorial').click()");
    const again = await waitFor(async () => (await cardText(r)) || false, 5000, "reopened").catch(() => "");
    check("Show tutorial after Done starts from the top", again.includes("Welcome to Invoke"), again.slice(0, 40));
    await clickAct(r, "skip");
    await sleep(1000);
    check("× closes it again", (await cardText(r)) === "");
    r.ws.close();
    await m.eval("document.getElementById('stop').click()");
    await waitFor(async () => (await ipc("get_snapshot")).state.kind === "idle", 120000, "idle after the second Stop");
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
