// SlopTweak tutorial overlay, injected into the Invoke window (remote.rs).
//
// Plain page script: it has no Tauri IPC (the remote window has no
// capabilities) and no secrets. It talks only to the page's own origin,
// through the same session cookie Invoke uses. `CFG` is supplied by the
// wrapper: { autoShow, force, sample (base64 JPEG), sampleName }.
//
// Skip and Done tell the app by navigating to /__sloptweak/tutorial/<done|
// skipped>, and a docs link by navigating to /__sloptweak/docs/<key>; the app
// intercepts and blocks both (remote.rs) and opens only its own fixed URL for
// the key. Nothing else crosses back.
//
// Written against Invoke 6.14.1's labels (en.json): Assets, New Canvas from
// Image, As Raster Layer (Resize), Inpaint Mask, Brush, Invoke, Accept, Save
// To Gallery. The highlight ring finds its target by that visible text or
// aria-label only; if a later Invoke renames or hides it, there is simply no
// ring, and the written steps still stand.
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

  const PROMPT = "a bowl of oranges";
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

  // Invoke's own docs; the keys must match remote.rs `invoke_docs`.
  const DOCS = [
    ["canvas", "Layers on the canvas"],
    ["bbox", "The bounding box (sharper inpainting)"],
    ["prompting", "Writing prompts"],
    ["hotkeys", "Keyboard shortcuts"],
    ["videos", "Video walkthroughs"],
    ["home", "All of Invoke's documentation"],
  ];

  // `target`: what to ring, most specific first (the first one on screen
  // wins, so the ring follows the user into a right-click menu).
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
      target: ["As Raster Layer (Resize)", "New Canvas from Image", "Assets"],
    },
    {
      title: "Paint over the vase",
      body:
        "In the layer list on the right, click <b>Inpaint Mask</b> (no mask there? Right-click " +
        "the picture → <b>New Inpaint Mask</b>). Press <b>B</b> for the brush and paint over the " +
        "whole vase and its flowers. Only what you paint gets redrawn.",
      target: ["New Inpaint Mask", "Inpaint Mask"],
    },
    {
      title: "Say what goes there",
      body:
        "Click the prompt box at the top left and type what you want there, for example " +
        `<code>${PROMPT}</code> <button data-act="copy" class="mini">Copy</button>. ` +
        "Then press <b>Invoke</b>. The first picture can take a minute.",
      watch: true,
      target: ["Invoke"],
    },
    {
      title: "Keep the one you like",
      body:
        "Your results show on the picture, with a toolbar under it. Flip through them with the " +
        "arrows and press <b>✓ Accept</b> to keep one. SlopTweak saves every try to the " +
        "<b>Canvas</b> folder inside your output folder. To save the finished picture too, " +
        "right-click it → <b>Save To Gallery</b> → <b>Save Canvas To Gallery</b>.",
      target: ["Accept"],
    },
    {
      title: "Where to go next",
      body:
        "That's the basics. Invoke can do a lot more (layers, reference images, control, " +
        "upscaling), and its own guides cover it. They open in your browser:" +
        `<span class="docs">${DOCS.map(([k, t]) => `<a href="#" data-act="doc" data-doc="${k}">${esc(t)}</a>`).join("")}</span>` +
        "Want this tour again? Press <b>Show tutorial</b> in SlopTweak.",
      next: "Done",
    },
  ];
  const LAST = STEPS.length - 1;
  const MARGIN = 16;

  // step: 0 intro .. LAST; status: "open" | "min" | "closed";
  // pos: the card's distance from the window's right and bottom edges.
  let st = Object.assign({ step: 0, status: CFG.autoShow ? "open" : "closed" }, load());
  // "Show tutorial" in the app opened this window: show it once, not on
  // every reload of the window.
  try {
    if (CFG.force && !sessionStorage.getItem("sloptweak.forced")) {
      sessionStorage.setItem("sloptweak.forced", "1");
      st.status = "open";
      if (st.step >= LAST) st.step = 0;
    }
  } catch {
    /* no storage: fine */
  }

  const CSS = `
    :host { all: initial; }
    .box { position: fixed; z-index: 2147483647;
      font: 14px/1.45 system-ui, "Segoe UI", sans-serif; color: #e6e8ee;
      background: #1c1f26; border: 1px solid #3b82f6; border-radius: 10px;
      box-shadow: 0 8px 28px rgba(0,0,0,.5); }
    .card { width: 340px; padding: 14px 16px 12px; }
    /* The welcome card: centered over a dimmed page so it can't be missed. */
    .scrim { position: fixed; inset: 0; z-index: 2147483646; background: rgba(0,0,0,.45); }
    .card.center { left: 50%; top: 50%; transform: translate(-50%, -50%); width: 380px; }
    .card.center .head { cursor: default; }
    .pill { padding: 6px 12px; cursor: pointer; }
    .head { display: flex; align-items: center; gap: 8px; color: #93a4c3; font-size: 12px;
      cursor: move; user-select: none; touch-action: none; margin: -6px -8px 0; padding: 6px 8px 0; }
    .head .sp { flex: 1; }
    h3 { margin: 6px 0 6px; font-size: 16px; color: #fff; }
    p { margin: 0 0 10px; }
    b { color: #fff; }
    code { background: #2a2f3a; padding: 1px 5px; border-radius: 4px; }
    img { display: block; width: 96px; height: 96px; object-fit: cover; border-radius: 6px;
      margin: 0 0 10px; border: 1px solid #3a3f4b; }
    .docs { display: flex; flex-direction: column; gap: 4px; margin: 8px 0; }
    a { color: #93c5fd; text-decoration: none; }
    a:hover { text-decoration: underline; }
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
    .ring { position: fixed; z-index: 2147483646; pointer-events: none; display: none;
      border: 2px solid #3b82f6; border-radius: 8px; box-shadow: 0 0 0 3px rgba(59,130,246,.25);
      animation: pulse 1.1s ease-in-out 3; }
    @keyframes pulse { 50% { box-shadow: 0 0 0 7px rgba(59,130,246,.12); } }
    @media (prefers-reduced-motion: reduce) { .ring { animation: none; } }
  `;

  let host = null;
  let root = null;
  let poll = null;
  let ringTimer = null;

  function mount() {
    if (host && host.isConnected) return;
    host = document.createElement("sloptweak-tutorial");
    root = host.attachShadow({ mode: "open" }); // open: dev/sync-check.mjs drives it
    document.documentElement.appendChild(host);
    root.addEventListener("click", onClick);
    root.addEventListener("pointerdown", onDragStart);
  }

  function setStatus(text, cls = "") {
    const s = root && root.querySelector(".status");
    if (s) {
      s.textContent = text;
      s.className = `status ${cls}`;
    }
  }

  // ----- position: bottom-right by default, dragged by the title line -----

  /** Keep the box fully on screen (the window can shrink after a drag). */
  function place() {
    const box = root && root.querySelector(".box");
    if (!box || box.classList.contains("center")) return;
    const pos = st.pos || { r: MARGIN, b: MARGIN };
    const maxR = Math.max(0, window.innerWidth - box.offsetWidth);
    const maxB = Math.max(0, window.innerHeight - box.offsetHeight);
    box.style.right = `${Math.min(Math.max(0, pos.r), maxR)}px`;
    box.style.bottom = `${Math.min(Math.max(0, pos.b), maxB)}px`;
  }

  function onDragStart(e) {
    const head = e.target.closest(".head");
    if (!head || e.button !== 0 || e.target.closest("button")) return;
    const box = root.querySelector(".box");
    if (box.classList.contains("center")) return;
    const r0 = parseFloat(box.style.right) || 0;
    const b0 = parseFloat(box.style.bottom) || 0;
    const x0 = e.clientX;
    const y0 = e.clientY;
    head.setPointerCapture(e.pointerId);
    const move = (ev) => {
      st.pos = { r: r0 - (ev.clientX - x0), b: b0 - (ev.clientY - y0) };
      place();
    };
    const up = () => {
      head.removeEventListener("pointermove", move);
      head.removeEventListener("pointerup", up);
      head.removeEventListener("pointercancel", up);
      // Save where it really ended up, not an off-screen request.
      st.pos = { r: parseFloat(box.style.right) || 0, b: parseFloat(box.style.bottom) || 0 };
      store();
    };
    head.addEventListener("pointermove", move);
    head.addEventListener("pointerup", up);
    head.addEventListener("pointercancel", up);
    e.preventDefault();
  }

  // ----- highlight ring: found by visible text or aria-label, never required -----

  const CLICKABLE = 'button, a, [role="button"], [role="tab"], [role="menuitem"], [role="option"]';

  function onScreen(el) {
    if (el.checkVisibility && !el.checkVisibility({ opacityProperty: true, visibilityProperty: true })) return null;
    const r = el.getBoundingClientRect();
    if (r.width < 4 || r.height < 4) return null;
    if (r.bottom <= 0 || r.right <= 0 || r.top >= window.innerHeight || r.left >= window.innerWidth) return null;
    return r;
  }

  /** The on-screen rectangle of the first label that matches, or null. */
  function findTarget(labels) {
    const body = document.body;
    if (!body) return null;
    // label -> { r, strong }; a match on something clickable beats plain text.
    const found = new Map();
    const offer = (l, el, strong) => {
      const had = found.get(l);
      if (had && (had.strong || !strong)) return;
      const r = onScreen(el);
      if (r) found.set(l, { r, strong });
    };
    const wanted = new Set(labels);
    // Icon buttons (Accept) are named by aria-label.
    for (const el of body.querySelectorAll("[aria-label]")) {
      const l = el.getAttribute("aria-label").trim();
      if (wanted.has(l)) offer(l, el, true);
    }
    // Everything else by its own text; ring the clickable thing around it.
    const walk = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
    for (let n = walk.nextNode(); n; n = walk.nextNode()) {
      const l = n.nodeValue.trim();
      const el = n.parentElement;
      if (!el || !wanted.has(l)) continue;
      const c = el.closest(CLICKABLE);
      offer(l, c || el, !!c);
    }
    for (const l of labels) if (found.has(l)) return found.get(l).r;
    return null;
  }

  function trackRing(labels) {
    const tick = () => {
      const ring = root && root.querySelector(".ring");
      if (!ring || document.hidden) return;
      let r = null;
      try {
        r = findTarget(labels);
      } catch {
        /* a page we don't understand: no ring */
      }
      if (!r) {
        ring.style.display = "none";
        return;
      }
      const pad = 4;
      Object.assign(ring.style, {
        display: "block",
        left: `${r.left - pad}px`,
        top: `${r.top - pad}px`,
        width: `${r.width + 2 * pad}px`,
        height: `${r.height + 2 * pad}px`,
      });
    };
    tick();
    ringTimer = window.setInterval(tick, 500);
  }

  // ----- rendering -----

  function render() {
    window.clearInterval(poll);
    window.clearInterval(ringTimer);
    poll = null;
    ringTimer = null;
    if (st.status === "closed") {
      if (host) host.remove();
      host = null;
      return;
    }
    mount();
    if (st.status === "min") {
      root.innerHTML = `<style>${CSS}</style><div class="box pill" data-act="restore">Tutorial · step ${st.step + 1} of ${STEPS.length}</div>`;
      place();
      return;
    }
    const s = STEPS[st.step] || STEPS[0];
    const thumb =
      s.thumb && st.image ? `<img alt="" src="/api/v1/images/i/${encodeURIComponent(st.image)}/thumbnail">` : "";
    const intro = st.step === 0;
    root.innerHTML = `<style>${CSS}</style>
      ${intro ? '<div class="scrim"></div>' : '<div class="ring"></div>'}
      <div class="box card${intro ? " center" : ""}" role="dialog" aria-label="SlopTweak tutorial">
        <div class="head"${intro ? "" : ' title="Drag to move"'}><span>SlopTweak tutorial · ${st.step + 1} of ${STEPS.length}</span><span class="sp"></span>
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
    place();
    if (s.watch) watchQueue();
    if (s.target) trackRing(s.target);
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
    st.step = Math.max(0, Math.min(LAST, step));
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
  function signal(path) {
    try {
      window.location.assign(`/__sloptweak/${path}`);
    } catch {
      /* the app will offer the tutorial again next time */
    }
  }

  function close(kind) {
    st.status = "closed";
    st.step = 0;
    store();
    render();
    signal(`tutorial/${kind}`);
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
    if (act === "doc") {
      e.preventDefault();
      return signal(`docs/${encodeURIComponent(el.getAttribute("data-doc") || "")}`);
    }
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
    if (st.step === LAST) return close("done");
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
      if (st.step > LAST) st.step = 0;
      store();
      render();
    },
  };

  window.addEventListener("resize", place);
  const boot = () => render();
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
  else boot();
})
