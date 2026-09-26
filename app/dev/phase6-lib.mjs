// Helpers for Phase 6 tuning experiments against a session held by
// dev/phase6-session.mjs. Nothing here rents or destroys anything.

import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const APP = resolve(import.meta.dirname, "..");
export const OUT = process.env.OUT ?? join(APP, "..", ".dev", "phase6");
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function waitFor(fn, ms, what, every = 1000) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(every);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last}` : ""}`);
}

export function session() {
  return JSON.parse(readFileSync(join(OUT, "session.json"), "utf8"));
}

/** CDP connection to the remote (Invoke) window of the held session. */
export async function remote() {
  const s = session();
  const list = await (await fetch(`http://127.0.0.1:${s.remote_port}/json/list`)).json();
  const t = list.find((t) => t.type === "page" && new URL(t.url).hostname.endsWith(".trycloudflare.com"));
  if (!t) throw new Error("no Invoke page target");
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
  const r = {
    ws,
    send,
    close: () => ws.close(),
    eval: async (expression) => {
      const x = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (x.result?.exceptionDetails) throw new Error(x.result.exceptionDetails.exception?.description ?? "eval failed");
      return x.result?.result?.value;
    },
    shot: async (name) => {
      const x = await send("Page.captureScreenshot", { format: "png" });
      const file = join(OUT, `${name}.png`);
      writeFileSync(file, Buffer.from(x.result.data, "base64"));
      return file;
    },
    mouse: (type, x, y, button = "left", buttons = 0, modifiers = 0) =>
      send("Input.dispatchMouseEvent", { type, x, y, button, buttons, clickCount: type === "mouseMoved" ? 0 : 1, modifiers }),
    click: async (x, y, button = "left") => {
      await r.mouse("mouseMoved", x, y, "none");
      await r.mouse("mousePressed", x, y, button, button === "right" ? 2 : 1);
      await r.mouse("mouseReleased", x, y, button);
    },
    drag: async (points, steps = 6) => {
      const [x0, y0] = points[0];
      await r.mouse("mouseMoved", x0, y0, "none");
      await r.mouse("mousePressed", x0, y0, "left", 1);
      let [px, py] = [x0, y0];
      for (const [x, y] of points.slice(1)) {
        for (let i = 1; i <= steps; i++) {
          await r.mouse("mouseMoved", px + ((x - px) * i) / steps, py + ((y - py) * i) / steps, "left", 1);
          await sleep(15);
        }
        [px, py] = [x, y];
      }
      await r.mouse("mouseReleased", px, py, "left");
    },
    /** A key press; `mods` bit 8 = shift, 2 = ctrl. */
    key: async (key, code, mods = 0, keyCode = 0) => {
      await send("Input.dispatchKeyEvent", { type: "keyDown", key, code, modifiers: mods, windowsVirtualKeyCode: keyCode });
      await send("Input.dispatchKeyEvent", { type: "keyUp", key, code, modifiers: mods, windowsVirtualKeyCode: keyCode });
    },
    api: (path, init) => r.eval(`fetch(${JSON.stringify(path)}, ${JSON.stringify(init ?? {})}).then(async (x) => { const t = await x.text(); try { return JSON.parse(t); } catch { return { status: x.status, text: t.slice(0, 500) }; } })`),
  };
  return r;
}

/** Center of the first visible element matching `selector` whose text/aria matches. */
export const findBox = (r, selector, predicate = "() => true") =>
  r.eval(`(() => {
    const els = [...document.querySelectorAll(${JSON.stringify(selector)})].filter((e) => {
      const b = e.getBoundingClientRect(); return b.width > 0 && b.height > 0 && (${predicate})(e);
    });
    if (!els.length) return null;
    const b = els[0].getBoundingClientRect();
    return { x: b.x + b.width / 2, y: b.y + b.height / 2, w: b.width, h: b.height, left: b.x, top: b.y };
  })()`);
export const byText = (t) => `(e) => e.textContent.trim() === ${JSON.stringify(t)}`;
export const byLabel = (t) => `(e) => (e.getAttribute('aria-label') || '') === ${JSON.stringify(t)}`;

/** Download an image from Invoke (full size) to OUT/<file>. */
export async function saveImage(r, name, file) {
  const b64 = await r.eval(`fetch('/api/v1/images/i/${encodeURIComponent(name)}/full').then(x => x.arrayBuffer()).then(b => { let s = ''; const u = new Uint8Array(b); for (let i = 0; i < u.length; i += 0x8000) s += String.fromCharCode(...u.subarray(i, i + 0x8000)); return btoa(s); })`);
  const path = join(OUT, file);
  writeFileSync(path, Buffer.from(b64, "base64"));
  return path;
}

/** Upload a local file to Invoke; returns the image DTO. */
export async function upload(r, path, { category = "general", intermediate = true, type = "image/png" } = {}) {
  const b64 = readFileSync(path).toString("base64");
  return r.eval(`(async () => {
    const bin = atob(${JSON.stringify(b64)}); const u = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
    const f = new FormData(); f.append('file', new Blob([u], { type: ${JSON.stringify(type)} }), 'x.png');
    const x = await fetch('/api/v1/images/upload?image_category=${category}&is_intermediate=${intermediate}', { method: 'POST', body: f });
    return x.json();
  })()`);
}

export const queueStatus = async (r) => (await r.api("/api/v1/queue/default/status")).queue;

/** Wait until the queue is idle and `completed` is at least `n`. */
export async function waitQueue(r, n, ms = 15 * 60000) {
  return waitFor(async () => {
    const q = await queueStatus(r);
    return q.completed + q.failed >= n && q.pending === 0 && q.in_progress === 0 ? q : false;
  }, ms, `queue completed >= ${n}`, 2000);
}
