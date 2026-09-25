// SlopTweak tutorial overlay, injected into the Invoke window (remote.rs).
//
// Plain page script: it has no Tauri IPC (the remote window has no
// capabilities) and no secrets. It talks only to the page's own origin,
// through the same session cookie Invoke uses. `CFG` is supplied by the
// wrapper: { autoShow, force, sample (base64 JPEG), sampleName }.
//
// Skip and Done tell the app by navigating to /__sloptweak/tutorial/<done|
// skipped>, which the app intercepts and blocks (remote.rs). Nothing else
// crosses back.
//
// Written against Invoke 6.14.1's labels (en.json): Assets, New Canvas from
// Image, As Raster Layer (Resize), Inpaint Mask, Brush, Invoke, Accept, Save
// To Gallery.
(function (CFG) {
  "use strict";
  if (window.top !== window || window.__slopTweakTutorial) return;

  const KEY = "sloptweak.tutorial";
  const load = () => {
    try {
      return JSON.parse(localStorage.getItem(KEY) || "{}") || {};
    } catch {
      return {};
    }
  };
  const store = () => {
    try {
      localStorage.setItem(KEY, JSON.stringify(st));
    } catch {
      /* private mode: this page only */
    }
  };
  // step: 0 intro .. 4 accept; status: "open" | "min" | "closed"
  let st = Object.assign({ step: 0, status: CFG.autoShow ? "open" : "closed" }, load());
  // "Show tutorial" in the app opened this window: show it once, not on
  // every reload of the window.
  try {
    if (CFG.force && !sessionStorage.getItem("sloptweak.forced")) {
      sessionStorage.setItem("sloptweak.forced", "1");
      st.status = "open";
      if (st.step >= 4) st.step = 0;
    }
  } catch {
    /* no storage: fine */
  }

  const PROMPT = "a bowl of oranges";
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

  const STEPS = [
    {
      title: "Welcome to Invoke",
      body:
        "Want a two-minute tour? You'll change part of a picture: paint over something, " +
        "say what should be there instead, and let the GPU redraw it.",
      next: "Show me",
    },
    {
      title: "Open the sample picture",
      body:
        "In the gallery on the right, click <b>Assets</b>. Right-click the picture of a room " +
        "and choose <b>New Canvas from Image</b> → <b>As Raster Layer (Resize)</b>.",
      thumb: true,
    },
    {
      title: "Paint over the vase",
      body:
        "In the layer list on the right, click <b>Inpaint Mask</b> (no mask there? Right-click " +
        "the picture → <b>New Inpaint Mask</b>). Press <b>B</b> for the brush and paint over the " +
        "whole vase and its flowers. Only what you paint gets redrawn.",
    },
    {
      title: "Say what goes there",
      body:
        "Click the prompt box at the top left and type what you want there, for example " +
        `<code>${PROMPT}</code> <button data-act="copy" class="mini">Copy</button>. ` +
        "Then press <b>Invoke</b>. The first picture can take a minute.",
      watch: true,
    },
    {
      title: "Keep the one you like",
      body:
        "Your results show on the picture, with a toolbar under it. Flip through them with the " +
        "arrows and press <b>✓ Accept</b> to keep one. SlopTweak saves every try to the " +
        "<b>Canvas</b> folder inside your output folder. To save the finished picture too, " +
        "right-click it → <b>Save To Gallery</b> → <b>Save Canvas To Gallery</b>.",
      next: "Done",
    },
  ];

  const CSS = `
    :host { all: initial; }
    .card, .pill { position: fixed; left: 16px; bottom: 16px; z-index: 2147483647;
      font: 14px/1.45 system-ui, "Segoe UI", sans-serif; color: #e6e8ee;
      background: #1c1f26; border: 1px solid #3b82f6; border-radius: 10px;
      box-shadow: 0 8px 28px rgba(0,0,0,.5); }
    .card { width: 340px; padding: 14px 16px 12px; }
    .pill { padding: 6px 12px; cursor: pointer; }
    .head { display: flex; align-items: center; gap: 8px; color: #93a4c3; font-size: 12px; }
    .head .sp { flex: 1; }
    h3 { margin: 6px 0 6px; font-size: 16px; color: #fff; }
    p { margin: 0 0 10px; }
    b { color: #fff; }
    code { background: #2a2f3a; padding: 1px 5px; border-radius: 4px; }
    img { display: block; width: 96px; height: 96px; object-fit: cover; border-radius: 6px;
      margin: 0 0 10px; border: 1px solid #3a3f4b; }
    .row { display: flex; gap: 8px; justify-content: flex-end; align-items: center; }
    .row .sp { flex: 1; }
    button { font: inherit; border-radius: 6px; border: 1px solid #3a3f4b; background: #2a2f3a;
      color: #e6e8ee; padding: 5px 12px; cursor: pointer; }
    button.primary { background: #3b82f6; border-color: #3b82f6; color: #fff; }
    button.x { border: none; background: none; padding: 0 4px; font-size: 16px; color: #93a4c3; }
    button.mini { padding: 0 6px; font-size: 12px; }
    button.link { border: none; background: none; color: #93a4c3; padding: 5px 4px; }
    .status { min-height: 1.4em; color: #fbbf24; font-size: 13px; margin: -4px 0 8px; }
    .err { color: #f87171; }
  `;

  let host = null;
  let root = null;
  let poll = null;

  function mount() {
    if (host && host.isConnected) return;
    host = document.createElement("sloptweak-tutorial");
    root = host.attachShadow({ mode: "open" }); // open: dev/sync-check.mjs drives it
    document.documentElement.appendChild(host);
    root.addEventListener("click", onClick);
  }

  function setStatus(text, cls = "") {
    const s = root && root.querySelector(".status");
    if (s) {
      s.textContent = text;
      s.className = `status ${cls}`;
    }
  }

  function render() {
    window.clearInterval(poll);
    poll = null;
    if (st.status === "closed") {
      if (host) host.remove();
      host = null;
      return;
    }
    mount();
    if (st.status === "min") {
      root.innerHTML = `<style>${CSS}</style><div class="pill" data-act="restore">Tutorial · step ${st.step + 1} of ${STEPS.length}</div>`;
      return;
    }
    const s = STEPS[st.step] || STEPS[0];
    const thumb =
      s.thumb && st.image ? `<img alt="" src="/api/v1/images/i/${encodeURIComponent(st.image)}/thumbnail">` : "";
    root.innerHTML = `<style>${CSS}</style>
      <div class="card" role="dialog" aria-label="SlopTweak tutorial">
        <div class="head"><span>SlopTweak tutorial · ${st.step + 1} of ${STEPS.length}</span><span class="sp"></span>
          <button class="x" data-act="min" title="Minimize">–</button>
          <button class="x" data-act="skip" title="Close the tutorial">×</button></div>
        <h3>${esc(s.title)}</h3>
        <p>${s.body}</p>${thumb}
        <div class="status"></div>
        <div class="row">
          ${st.step === 0 ? '<button class="link" data-act="skip">Skip</button>' : '<button data-act="back">Back</button>'}
          <span class="sp"></span>
          <button class="primary" data-act="next">${esc(s.next || "Next")}</button>
        </div>
      </div>`;
    if (s.watch) watchQueue();
  }

  async function queueStatus() {
    const r = await fetch("/api/v1/queue/default/status", { credentials: "same-origin" });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const j = await r.json();
    return j.queue || j;
  }

  /** Step 3: move on by itself once a generation finishes. */
  function watchQueue() {
    const tick = async () => {
      try {
        const q = await queueStatus();
        if (st.baseline === undefined) {
          st.baseline = q.completed || 0;
          store();
        }
        if ((q.completed || 0) > st.baseline) {
          go(4);
          return;
        }
        const busy = (q.in_progress || 0) + (q.pending || 0);
        setStatus(busy ? "Making your picture…" : "");
      } catch {
        setStatus("");
      }
    };
    void tick();
    poll = window.setInterval(tick, 2000);
  }

  function go(step) {
    st.step = Math.max(0, Math.min(STEPS.length - 1, step));
    if (st.step !== 3) delete st.baseline;
    store();
    render();
  }

  /** Put the sample in Invoke's Assets (once per GPU), then reload so the gallery shows it. */
  async function ensureSample() {
    if (st.image) {
      const r = await fetch(`/api/v1/images/i/${encodeURIComponent(st.image)}`, { credentials: "same-origin" });
      if (r.ok) return false;
    }
    const bin = atob(CFG.sample);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const form = new FormData();
    form.append("file", new Blob([bytes], { type: "image/jpeg" }), CFG.sampleName);
    const r = await fetch("/api/v1/images/upload?image_category=user&is_intermediate=false", {
      method: "POST",
      body: form,
      credentials: "same-origin",
    });
    if (!r.ok) throw new Error(`upload failed (HTTP ${r.status})`);
    st.image = (await r.json()).image_name;
    return true;
  }

  /** Tell the app (it blocks this navigation, so the page stays). */
  function signal(kind) {
    try {
      window.location.assign(`/__sloptweak/tutorial/${kind}`);
    } catch {
      /* the app will offer the tutorial again next time */
    }
  }

  function close(kind) {
    st.status = "closed";
    st.step = 0;
    store();
    render();
    signal(kind);
  }

  async function onClick(e) {
    const el = e.target.closest("[data-act]");
    if (!el) return;
    const act = el.getAttribute("data-act");
    if (act === "skip") return close("skipped");
    if (act === "min") {
      st.status = "min";
      store();
      return render();
    }
    if (act === "restore") {
      st.status = "open";
      store();
      return render();
    }
    if (act === "back") return go(st.step - 1);
    if (act === "copy") {
      try {
        await navigator.clipboard.writeText(PROMPT);
        el.textContent = "Copied";
      } catch {
        el.textContent = "Select it and copy";
      }
      return;
    }
    if (act !== "next") return;
    if (st.step === STEPS.length - 1) return close("done");
    if (st.step === 0) {
      el.disabled = true;
      setStatus("Getting the sample picture ready…");
      try {
        const uploaded = await ensureSample();
        st.step = 1;
        store();
        // Invoke's gallery doesn't hear about uploads made outside its own
        // UI, so reload once to show the sample.
        if (uploaded) return window.location.reload();
        return render();
      } catch (err) {
        el.disabled = false;
        return setStatus(`Couldn't load the sample: ${err.message}`, "err");
      }
    }
    go(st.step + 1);
  }

  window.__slopTweakTutorial = {
    open() {
      st.status = "open";
      if (st.step >= STEPS.length) st.step = 0;
      store();
      render();
    },
  };

  const boot = () => render();
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
  else boot();
})
