// SlopTweak: apply the model's own steps/CFG once, injected into the Invoke
// window with the tutorial (remote.rs).
//
// Invoke 6.14.1 registers a model's `default_settings` but never applies
// them: only the sparkle button next to the model picker does. So a Turbo
// model would open at 30 steps / CFG 7.5 and overcook. Invoke's recall API
// (POST /api/v1/recall/{queue}) stores the values and pushes them to the open
// page over its socket (`recall_parameters_updated`), so this posts them once
// the UI is up. Plain page script, same origin and session cookie as Invoke,
// no IPC. Once per origin (a new GPU gets a new tunnel origin), so a user's
// own changes survive a reload. The recall event has no scheduler case in
// 6.14.1's frontend, so a catalog scheduler (Banana Splitz) still needs the
// sparkle button; the tutorial says so.
(function () {
  "use strict";
  if (window.top !== window || window.__slopTweakDefaults) return;
  window.__slopTweakDefaults = true;

  const KEY = "sloptweak.defaults";
  try {
    if (localStorage.getItem(KEY)) return;
  } catch {
    /* storage blocked: apply anyway */
  }
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // The UI is up once Invoke's own Invoke button is on screen. Live 6.14.1:
  // it has the text "Invoke" and no aria-label.
  const uiReady = () =>
    [...document.querySelectorAll("button")].some(
      (b) => b.getAttribute("aria-label") === "Invoke" || b.textContent.trim() === "Invoke",
    );

  async function run() {
    for (let i = 0; i < 120 && !uiReady(); i++) await sleep(1000);
    if (!uiReady()) return;
    const r = await fetch("/api/v2/models/?model_type=main", { credentials: "same-origin" });
    if (!r.ok) return;
    const models = (await r.json()).models || [];
    const d = models.map((m) => m.default_settings).find((s) => s && (s.steps || s.cfg_scale));
    if (!d) return;
    const body = {};
    if (d.steps) body.steps = d.steps;
    if (d.cfg_scale) body.cfg_scale = d.cfg_scale;
    // The page's socket may connect a moment after the button renders, and
    // the event only reaches connected pages, so send it twice.
    for (const wait of [2000, 5000]) {
      await sleep(wait);
      await fetch("/api/v1/recall/default", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    }
    try {
      localStorage.setItem(KEY, JSON.stringify(body));
    } catch {
      /* this page only */
    }
  }
  run().catch(() => {});
})();
