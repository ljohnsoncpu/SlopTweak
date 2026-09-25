// Local, $0 check of the remote Invoke window's isolation (findings §6).
//
// Runs the real instance/sidecar.py on 127.0.0.1 in front of a fake Invoke,
// opens the app's remote window at /__auth?t=<ticket> (debug build, dev env
// hooks), and inspects it over a dev-only CDP port:
//   - ticket -> HttpOnly cookie -> proxied page loads, and survives reload
//   - IPC from the remote origin is denied for app and core commands
//   - top-level navigation off-origin and popups are blocked
//   - the window uses its own WebView2 data directory
//
// Usage (from app/):  node dev/webview-check.mjs
// Needs: a debug build (cargo build), uv, python, node >= 22.

import { spawn, execSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { existsSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const APP = resolve(import.meta.dirname, "..");
const ROOT = resolve(APP, "..");
const EXE = join(APP, "src-tauri", "target", "debug", "sloptweak.exe");
const SIDECAR_PORT = 18080;
const INVOKE_PORT = 9090; // sidecar.py's upstream is fixed to 127.0.0.1:9090
const CDP_PORT = 9333;
const ORIGIN = `http://127.0.0.1:${SIDECAR_PORT}`;
const DATA_DIR = join(process.env.LOCALAPPDATA ?? "", "com.sloptweak.launcher");

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
  results.push({ name, ok, detail });
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
    await sleep(300);
  }
  throw new Error(`timed out waiting for ${what}: ${last ?? ""}`);
}

class Cdp {
  constructor(wsUrl) {
    this.ws = new WebSocket(wsUrl);
    this.id = 0;
    this.pending = new Map();
    this.ws.onmessage = (m) => {
      const msg = JSON.parse(m.data);
      const p = this.pending.get(msg.id);
      if (p) {
        this.pending.delete(msg.id);
        msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
      }
    };
  }
  open() {
    return new Promise((res, rej) => {
      this.ws.onopen = res;
      this.ws.onerror = rej;
    });
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  async eval(expression) {
    const r = await this.send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? "eval failed");
    return r.result.value;
  }
}

async function targets() {
  const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
  return (await r.json()).filter((t) => t.type === "page");
}

async function main() {
  if (!existsSync(EXE)) throw new Error(`build first: ${EXE}`);
  const tmp = mkdtempSync(join(tmpdir(), "sloptweak-webview-"));
  writeFileSync(
    join(tmp, "index.html"),
    "<!doctype html><title>FAKE INVOKE</title><h1>fake invoke</h1>",
  );
  const status = join(tmp, "status.json");
  writeFileSync(status, JSON.stringify({ stage: "ready" }));

  // Throwaway secret for this local run only; never a real credential.
  const secret = randomBytes(32).toString("base64url");
  const hash = createHash("sha256").update(secret).digest("hex");

  start("python", ["-m", "http.server", String(INVOKE_PORT), "--bind", "127.0.0.1", "--directory", tmp]);
  const sidecar = start("uv", ["run", "--project", "instance", "python", "instance/sidecar.py"], {
    cwd: ROOT,
    env: {
      ...process.env,
      LAUNCH_TOKEN_HASH: hash,
      SIDECAR_PORT: String(SIDECAR_PORT),
      SLOPTWEAK_STATUS_FILE: status,
      HEARTBEAT_MINUTES: "60",
    },
  });
  const auth = { Authorization: `Bearer ${secret}` };
  await waitFor(
    async () => (await fetch(`${ORIGIN}/__heartbeat`, { method: "POST", headers: auth })).ok,
    30000,
    "sidecar",
  ).catch((e) => {
    console.log(sidecar.output());
    throw e;
  });

  const noCookie = await fetch(`${ORIGIN}/`);
  check("proxy rejects requests without a session", noCookie.status === 401, `HTTP ${noCookie.status}`);

  const { ticket } = await (await fetch(`${ORIGIN}/__ticket`, { method: "POST", headers: auth })).json();
  const authUrl = `${ORIGIN}/__auth?t=${ticket}`;

  const app = start(EXE, [], {
    env: {
      ...process.env,
      SLOPTWEAK_PROVIDER: "mock",
      SLOPTWEAK_DEV_REMOTE_URL: authUrl,
      SLOPTWEAK_REMOTE_DEBUG_PORT: String(CDP_PORT),
    },
  });

  const page = await waitFor(
    async () => (await targets()).find((t) => t.url.startsWith(ORIGIN)),
    60000,
    "remote window",
  ).catch((e) => {
    console.log(app.output());
    throw e;
  });
  const cdp = new Cdp(page.webSocketDebuggerUrl);
  await cdp.open();
  await cdp.send("Network.enable");

  await waitFor(async () => (await cdp.eval("document.readyState")) === "complete", 15000, "load");
  const title = await cdp.eval("document.title");
  check("ticket login lands on proxied Invoke", title === "FAKE INVOKE", `title=${title}`);
  check("URL no longer carries the ticket", !(await cdp.eval("location.href")).includes("t="));

  const reuse = await fetch(authUrl, { redirect: "manual" });
  check("ticket is single-use", reuse.status === 401, `HTTP ${reuse.status}`);

  const { cookies } = await cdp.send("Network.getCookies", { urls: [ORIGIN + "/"] });
  const sess = cookies.find((c) => c.name === "st_session");
  check("session cookie set, HttpOnly", !!sess && sess.httpOnly, sess ? `secure=${sess.secure}` : "missing");
  check("session cookie hidden from page JS", !(await cdp.eval("document.cookie")).includes("st_session"));

  const injected = await cdp.eval("typeof window.__TAURI_INTERNALS__");
  console.log(`info  __TAURI_INTERNALS__ in remote page: ${injected}`);
  const cmds = [
    ["get_snapshot", {}],
    ["set_secret", { which: "vast", value: "x" }],
    ["confirm_close", {}],
    ["stop_session", {}],
    ["plugin:window|close", { label: "main" }],
    ["plugin:webview|create_webview_window", {}],
    ["plugin:event|emit", { event: "session-state", payload: null }],
    ["plugin:app|version", {}],
  ];
  for (const [cmd, args] of cmds) {
    const r = await cdp.eval(`(async () => {
      if (!window.__TAURI_INTERNALS__) return "no-ipc";
      try { await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}); return "ALLOWED"; }
      catch (e) { return "denied: " + String(e).slice(0, 120); }
    })()`);
    check(`IPC denied from remote: ${cmd}`, r !== "ALLOWED", r);
  }

  const fetched = await cdp.eval("fetch('/').then(r => r.status)");
  check("same-origin requests ride the cookie", fetched === 200, `HTTP ${fetched}`);

  await cdp.send("Page.reload");
  await sleep(1500);
  await waitFor(async () => (await cdp.eval("document.readyState")) === "complete", 15000, "reload");
  check("cookie survives reload", (await cdp.eval("document.title")) === "FAKE INVOKE");

  await cdp.eval("location.href = 'https://example.com/'; true");
  await sleep(2500);
  const after = await cdp.eval("location.origin");
  check("off-origin navigation blocked", after === ORIGIN, `origin=${after}`);

  const before = (await targets()).length;
  await cdp.eval("(window.open('https://example.com/'), true)");
  await sleep(2500);
  const pages = await targets();
  check(
    "popups blocked",
    pages.length === before && !pages.some((t) => t.url.includes("example.com")),
    `${pages.length} page targets`,
  );

  const remoteDir = join(DATA_DIR, "remote-webview", "EBWebView");
  const mainDir = join(DATA_DIR, "EBWebView");
  check("remote window has its own data directory", existsSync(remoteDir), remoteDir);
  check("main window data directory is separate", existsSync(mainDir), mainDir);

  const onlyRemote = pages.every((t) => t.url.startsWith(ORIGIN));
  check("debug port exposes only the remote webview", onlyRemote);

  cdp.ws.close();
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
