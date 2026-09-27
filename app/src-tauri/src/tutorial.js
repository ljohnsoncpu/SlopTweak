// SlopTweak tutorial overlay, injected into the Invoke window (remote.rs).
//
// Plain page script: it has no Tauri IPC (the remote window has no
// capabilities) and no secrets. It talks only to the page's own origin,
// through the same session cookie Invoke uses. `CFG` is supplied by the
// wrapper: { autoShow, force, stage, stages, modelId, modelName, modelPage,
// portrait (base64), portraitName, portraitType }.
//
// It tells the app things by navigating to /__sloptweak/tutorial/<done|
// skipped|stage/N|model-page> or /__sloptweak/docs/<key>, which the app
// intercepts and blocks (remote.rs); for docs the app opens only its own
// fixed URL for the key. Nothing else crosses back. The stage number is how the
// tutorial resumes on the next GPU: this page's localStorage is per tunnel
// origin, so it's empty on every new instance.
//
// Five stages, one character (PLAN §4 Phase 6). Labels and hotkeys are
// Invoke 6.14.1's, checked against its source (docs/findings.md → "Label
// check for the v2 copy"): Generate, Width, Height, Add Negative Prompt,
// Seed, Random, Choose Prompt Template, Create Prompt Template, Positive
// Prompt, Negative Prompt, Insert placeholder, Save, Assets, New Canvas from
// Image, As Raster Layer (Resize), Inpaint Mask, Raster Layer, Add Layer,
// Fit Bbox To Masks (Shift+B), Bbox (C), Scaled Bbox, B (Brush), Shift+C
// (Reset Layer), Image, Denoising Strength, Opacity, Accept (Enter), Save To
// Gallery, Save Canvas To Gallery. A future image bump must re-check them.
// The copy keeps Invoke's own UI terms (bbox) so the UI makes sense, but
// skips internals (scheduler, CFG): users aren't assumed technical.
//
// The home card opens centered over a dimmed page; step cards start
// bottom-right and can be dragged by their title line. A highlight ring
// marks a step's `target`, found by visible text or aria-label only: if a
// later Invoke renames or hides it, there is simply no ring.
(function (CFG) {
  "use strict";
  if (window.top !== window || window.__slopTweakTutorial) return;

  const STAGES_N = CFG.stages || 5;

  // Numbers, seeds, and tags tuned on real GPUs (PLAN §4 Phase 6; results in
  // docs/findings.md → "Phase 6 tuning").
  const TUNE = {
    // Tuned live 2026-09-26 at the ✨ settings below (docs/findings.md →
    // "Phase 6 tuning" and "Re-tune at the catalog settings"). A model with
    // no seed here leaves Random on.
    seed: { "banana-splitz-xxl": 42, "anima-aesthetic": 42, "anima-turbo": 123 },
    // Our portrait: 0.3 leaves Anima's eyes red but already turns Banana
    // Splitz's a yellowish green, too green for a "too weak" demo, so the
    // first try is 0.2 (user feedback 2026-09-26). 0.55 gives clean green
    // eyes. Turbo at CFG 1 follows the prompt more loosely: 0.55-0.6 stay
    // olive, 0.65-0.7 turn green.
    eyeDenoiseLow: "0.2",
    eyeDenoiseHigh: { "anima-turbo": "0.65" },
    eyeDenoiseHighDefault: "0.55",
    // With a ~50% blue band painted: a clear see-through visor at 0.6–0.75,
    // cleanest at 0.7. Without paint the model draws thin glasses instead.
    // Tuned with the visor text at the END of the prompt; the copy now puts
    // it first (user feedback 2026-09-26: earlier words weigh more); the
    // user judged 0.7 still fine there without a re-tune.
    visorDenoise: "0.7",
    visorOpacity: "50",
    visorPrompt: "transparent blue cyberpunk visor",
    // Per catalog model, from its page (2026-09-26). Checked live on Anima
    // Aesthetic (less oversaturated, better shading); Banana Splitz's is from
    // its examples only.
    // Banana Splitz: tags in most of the 10 example prompts of v1.2.1 and
    // in all 10 negatives. Anima: the model card's recommended prefix and
    // negative, minus the score_* tags it says to leave out on Aesthetic.
    template: {
      "banana-splitz-xxl": {
        tags: "masterpiece, best quality, amazing quality, very aesthetic, absurdres, newest",
        negative: "worst quality, low quality, lowres, bad quality, bad hands, mutated hands, signature, artist name, sketch",
      },
      "anima-aesthetic": {
        tags: "masterpiece, best quality, safe",
        negative: "worst quality, low quality, artist name, blurry, jpeg artifacts, chromatic aberration",
      },
      "anima-turbo": {
        tags: "masterpiece, best quality, safe",
        negative: "worst quality, low quality, artist name, blurry, jpeg artifacts, chromatic aberration",
      },
    },
    // SlopTweak writes each model's settings into its Invoke config from the
    // catalog, and defaults.js applies them when Invoke opens (Invoke doesn't
    // on its own; the ✨ "Use default settings" button restores them). The
    // copy doesn't list them: users needn't know what a scheduler is.
    // Models whose catalog settings include a scheduler: defaults.js can
    // only set steps/CFG (Invoke's recall event ignores the scheduler), so
    // the ✨ button is still needed for these.
    sparkleScheduler: { "banana-splitz-xxl": true },
    // At CFG 1 there's no negative guidance, so a negative prompt does nothing.
    noNegative: { "anima-turbo": true },
    // Per catalog model, from its page: the caption style it expects.
    captionHint: {
      "banana-splitz-xxl": "Its examples are almost all tags, starting with quality tags.",
      "anima-aesthetic":
        "It knows both tags and sentences; its page asks for lowercase tags, with spaces instead of underscores.",
      "anima-turbo":
        "It knows both tags and sentences; its page asks for lowercase tags, with spaces instead of underscores.",
    },
  };

  // Invoke's own docs; the keys must match remote.rs `invoke_docs`.
  const DOCS = [
    ["canvas", "Layers on the canvas"],
    ["bbox", "The bounding box (sharper inpainting)"],
    ["prompting", "Writing prompts"],
    ["hotkeys", "Keyboard shortcuts"],
    ["videos", "Video walkthroughs"],
    ["home", "All of Invoke's documentation"],
  ];

  // The fixed character (user-supplied; PLAN §4 Phase 6). Verbatim.
  const POSITIVE =
    "a woman standing in front of a white background,\n" +
    "solo, female, human, white background, black tank top, black jeans, red eyes, red hair, " +
    "simple background, front view, forehead, standing, medium shot, smile";
  const NEGATIVE = "nsfw";
  const WIDTH = 832;
  const HEIGHT = 1216;

  const KEY = "sloptweak.tutorial.v2"; // v1 kept a different shape
  const load = () => {
    try {
      return JSON.parse(localStorage.getItem(KEY) || "null");
    } catch {
      return null;
    }
  };
  const store = () => {
    try {
      localStorage.setItem(KEY, JSON.stringify(st));
    } catch {
      /* private mode: this page only */
    }
  };

  const clampStage = (n) => Math.max(1, Math.min(STAGES_N, n | 0 || 1));
  // stage: 0 = the home card, 1..STAGES_N; step: index into visible steps.
  // status: "open" | "min" | "closed". resume: the stage "Continue" goes to.
  // jumped: this stage was entered from the home card, not from the stage
  // before it, so the canvas may be empty (a new GPU). base: queue
  // baselines per step. image: the fallback portrait's name in Invoke.
  let st = load() || {
    stage: 0,
    step: 0,
    status: CFG.autoShow ? "open" : "closed",
    resume: clampStage(CFG.stage),
    signaled: clampStage(CFG.stage),
  };
  st.base = st.base || {};
  // "Show tutorial" in the app opened this window: show it once, not on
  // every reload of the window.
  try {
    if (CFG.force && !sessionStorage.getItem("sloptweak.forced")) {
      sessionStorage.setItem("sloptweak.forced", "1");
      st.status = "open";
    }
  } catch {
    /* no storage: fine */
  }

  const esc = (s) =>
    String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const model = CFG.modelName ? `<b>${esc(CFG.modelName)}</b>` : "this model";
  const tpl = TUNE.template[CFG.modelId] || null;
  const eyeHigh = TUNE.eyeDenoiseHigh[CFG.modelId] || TUNE.eyeDenoiseHighDefault;

  // Text the Copy buttons put on the clipboard.
  const COPY = {
    positive: POSITIVE,
    negative: NEGATIVE,
    green: "green eyes",
    visor: `${TUNE.visorPrompt}, `,
    tplPositive: tpl ? `${tpl.tags}, {prompt}` : "",
    tplNegative: tpl ? tpl.negative : "",
  };
  const copyBtn = (k) => `<button data-act="copy" data-copy="${k}" class="mini">Copy</button>`;
  const pageBtn = CFG.modelPage
    ? `<button data-act="page" class="mini">Open the model page</button>`
    : "";
  const oursBtn = `<button data-act="ours">Use ours instead</button>`;
  const promptBlock = `<pre>${esc(POSITIVE)}</pre>`;

  // Steps with `jumpOnly` show only when the stage was entered from the
  // home card, not from the stage before (the prompt and canvas may not be
  // set up). `watch` moves on by itself when a generation finishes. `ours`
  // shows the optional "Use ours instead" button. The copy never tells the
  // user the GPU is empty or that ours is required (user feedback).
  const onCanvas = {
    jumpOnly: true,
    ours: true,
    target: ["As Raster Layer (Resize)", "New Canvas from Image", "Assets"],
    title: "Set up the picture",
    body: () =>
      "<p>If the prompt box is empty, paste the prompt in " +
      `${copyBtn("positive")}, and <code>${NEGATIVE}</code> in the negative box ${copyBtn("negative")} ` +
      "(the <b>±</b> button shows it).</p>" +
      "<p>Then put a picture on the canvas: right-click it in the gallery and choose " +
      "<b>New Canvas from Image</b> → <b>As Raster Layer (Resize)</b>. " +
      (st.image
        ? "Ours is in the gallery's <b>Assets</b> tab.</p>"
        : "You can use one of yours, or press <b>Use ours instead</b> to follow along with the " +
          "same picture we used.</p>"),
  };

  const STAGES = [
    {
      title: "Prompting",
      steps: [
        {
          title: "Set up the Generate tab",
          target: ["Generate"],
          body: () =>
            "Click the <b>Generate</b> tab on the left. " +
            "Every model works best with its own settings. " +
            (TUNE.sparkleScheduler[CFG.modelId]
              ? "SlopTweak has loaded most of them for you; to finish, click the small sparkle button " +
                "next to the model's name in the <b>Generation</b> section " +
                '("Model Defaults Loaded" appears). '
              : "SlopTweak has already loaded them for you. If you ever change them by accident, the " +
                "small sparkle button next to the model's name in the <b>Generation</b> section puts " +
                "them back. ") +
            "In the <b>Image</b> section, set " +
            `<b>Width</b> to <code>${WIDTH}</code> and <b>Height</b> to <code>${HEIGHT}</code>. ` +
            `Then put this in the prompt box: ${promptBlock} ${copyBtn("positive")}`,
        },
        {
          title: "Say what you don't want",
          target: ["Add Negative Prompt"],
          watch: true,
          body: () =>
            "Click the <b>±</b> button on the prompt box (<b>Add Negative Prompt</b>) and type " +
            `<code>${NEGATIVE}</code> in the new box ${copyBtn("negative")}. The top box says what you ` +
            "want; this one says what you don't. Some models can make NSFW content, and " +
            `<code>${NEGATIVE}</code> here keeps this one SFW. ` +
            (TUNE.noNegative[CFG.modelId]
              ? "One catch: this model skips the negative box, so also put " +
                "<code>safe,</code> at the start of the prompt. "
              : "") +
            (TUNE.seed[CFG.modelId] !== undefined
              ? `Turn off <b>Random</b> next to <b>Seed</b> and set it to <code>${esc(TUNE.seed[CFG.modelId])}</code>, so you get a picture we've tried. `
              : "") +
            "Now press <b>Invoke</b>. The first picture can take a minute.",
        },
        {
          title: "Match the model's captions",
          body: () =>
            "<p>Your prompt had two parts: a plain sentence, then short tags separated by commas. " +
            "Prompts work best when they match the kind of captions the model was trained on. " +
            `For ${model}, the example pictures on its page show what those look like. ` +
            `${esc(TUNE.captionHint[CFG.modelId] || "")} ${pageBtn}</p>` +
            "<p>Want more options? You can always press <b>Invoke</b> again with the same settings. " +
            "Turn <b>Random</b> back on next to <b>Seed</b> first, or you'll get the same picture.</p>",
        },
      ],
    },
    {
      title: "Prompt Templates",
      steps: [
        {
          title: "Automate the repeated part",
          body: () =>
            "Some of every prompt stays the same: usually quality tags at the start, and a standard " +
            "negative prompt. A prompt template adds those for you, so you only type what's new " +
            "about each picture." +
            (tpl
              ? ` For ${model}, the example prompts nearly all start with <code>${esc(tpl.tags)}</code>, ` +
                `and use <code>${esc(tpl.negative)}</code> as the negative.`
              : ` To find them, look for tags that show up in nearly every example prompt on ${model}'s page. ${pageBtn}`),
        },
        {
          title: "Make a template",
          target: ["Create Prompt Template", "Choose Prompt Template"],
          body: () =>
            "Above the prompt box, click <b>Choose Prompt Template</b>, then the <b>+</b> " +
            "(<b>Create Prompt Template</b>). Give it any name you like; if you only need one " +
            "template for this model, naming it after the model makes it easy to find. In <b>Positive Prompt</b>, " +
            "paste the tags, then click <b>Insert placeholder</b>: <code>{prompt}</code> is where your " +
            "own words go" +
            (tpl ? ` ${copyBtn("tplPositive")}` : "") +
            ". Put the negative tags in <b>Negative Prompt</b>" +
            (tpl ? ` ${copyBtn("tplNegative")}` : "") +
            " and press <b>Save</b>.",
        },
        {
          title: "Use it",
          target: ["Invoke"],
          body: () =>
            "Pick your template in the list. Your prompt box stays as it is; when you press " +
            "<b>Invoke</b>, the template's tags are added around your words. Press it now and compare " +
            "with your first picture. A template isn't tied to a model, so choose another one when you " +
            "switch models.",
        },
      ],
    },
    {
      title: "To the canvas",
      steps: [
        {
          title: "Open your picture on the canvas",
          target: ["As Raster Layer (Resize)", "New Canvas from Image", "Assets"],
          ours: true,
          body: () =>
            (st.image
              ? "Our picture is in the gallery's <b>Assets</b> tab. Right-click it"
              : "In the gallery on the right, right-click the picture you made") +
            " and choose <b>New Canvas from Image</b> → <b>As Raster Layer (Resize)</b>." +
            (st.image
              ? ""
              : " If you'd rather follow along with the same picture we used, press " +
                "<b>Use ours instead</b>. Its red eyes need fixing, which is next."),
        },
        {
          title: "What you're looking at",
          body: () =>
            "Your picture is now a <b>Raster Layer</b> (see the layer list on the right). Above it is " +
            "an empty <b>Inpaint Mask</b>, already selected: whatever you paint on it gets redrawn. The " +
            "box with handles around the picture is the <b>bbox</b> (bounding box): the area the model " +
            "looks at while it draws.",
        },
      ],
    },
    {
      title: "Fix the eyes",
      steps: [
        onCanvas,
        {
          title: "Mask the eyes",
          target: ["Inpaint Mask"],
          body: () =>
            "Click <b>Inpaint Mask</b> in the layer list, press <b>B</b> for the brush, and paint over " +
            "both eyes. Only what you paint gets redrawn.",
        },
        {
          title: "Fit the bbox, then give it the face",
          body: () =>
            "Press <b>Shift+B</b> (<b>Fit Bbox To Masks</b>): the bbox snaps tight around the eyes. " +
            "Now press <b>C</b> (the <b>Bbox</b> tool) and drag its corners out until it covers the " +
            "whole face. The model only redraws what you masked, but it looks at everything in the " +
            "bbox, and it needs to see the face to draw eyes that fit.",
        },
        {
          title: "Why a small bbox looks sharp",
          body: () =>
            "Look at the top left of the canvas: <b>Bbox</b> is small, but <b>Scaled Bbox</b> is " +
            "bigger. Invoke zooms in on the bbox, draws at the scaled size in full detail, then fits " +
            "the result back into your picture. That's why fixing a small area gives sharp results.",
        },
        {
          title: "Recolor them, gently",
          target: ["Denoising Strength"],
          watch: true,
          body: () =>
            `In the prompt, change <code>red eyes</code> to <code>green eyes</code> ${copyBtn("green")}. ` +
            "At the top of the layers panel is <b>Denoising Strength</b>: how much the model may change " +
            "what you painted. Low keeps it close to the original; high lets it redraw freely. Set it " +
            `to <code>${TUNE.eyeDenoiseLow}</code> and press <b>Invoke</b>.`,
        },
        {
          title: "Now a bit stronger",
          target: ["Denoising Strength"],
          watch: true,
          body: () =>
            `At <code>${TUNE.eyeDenoiseLow}</code> the eyes barely change: the model stayed too close ` +
            `to the old picture. Set <b>Denoising Strength</b> to <code>${eyeHigh}</code> and press ` +
            "<b>Invoke</b> again.",
        },
        {
          title: "Keep the best one",
          target: ["Accept"],
          body: () =>
            "Your tries show on the canvas with a toolbar under it. Flip through them with the arrows " +
            "and press <b>Accept</b> (or <b>Enter</b>) on the one you like. None look right? Press " +
            "<b>Invoke</b> again for more tries (with <b>Random</b> on next to <b>Seed</b>, each is " +
            "different). SlopTweak saves every try to the <b>Canvas</b> folder in your output folder.",
        },
      ],
    },
    {
      title: "A see-through visor",
      steps: [
        onCanvas,
        {
          title: "Paint a rough visor",
          target: ["Add Layer"],
          body: () =>
            "<p>First, if your eye tries are still showing under the canvas, press <b>Accept</b> on " +
            "one. You can't paint until they're gone.</p>" +
            "<p>Now make a fresh layer to paint on: under the layer list, click <b>+</b> " +
            "(<b>Add Layer</b>) and choose <b>Raster Layer</b>.</p>" +
            "<p>Pick a color by clicking the color circles at the top left of the canvas, and choose " +
            "a bright blue. Then press <b>B</b> for the brush and paint a thick band across both eyes, " +
            "like sunglasses. It doesn't need to be neat.</p>",
        },
        {
          title: "Make it see-through",
          target: ["Opacity"],
          body: () =>
            "With the new layer selected, set <b>Opacity</b> (just above the layer list) to about " +
            `<code>${TUNE.visorOpacity}%</code>. Now you can see the eyes through the blue, just like ` +
            "tinted glass. That's the look the model will copy.",
        },
        {
          title: "Mark the band for redrawing",
          target: ["Inpaint Mask"],
          body: () =>
            "Click <b>Inpaint Mask</b> in the layer list and press <b>Shift+C</b> to wipe the old eye " +
            "paint. Press <b>B</b> and paint over the whole blue band. Then press <b>Shift+B</b>, and " +
            "<b>C</b> to drag the bbox out over the head, like before.",
        },
        {
          title: "Describe it, then go big",
          target: ["Denoising Strength"],
          watch: true,
          body: () =>
            `<p>Add <code>${esc(TUNE.visorPrompt)},</code> to the <b>start</b> of the prompt ` +
            `${copyBtn("visor")}. Words near the start of a prompt count for more, so put what matters ` +
            "most first.</p>" +
            `<p>Set <b>Denoising Strength</b> to about <code>${TUNE.visorDenoise}</code> and press ` +
            "<b>Invoke</b>. A new eye color only needed a gentle nudge. Adding something new, like a " +
            "visor, needs a stronger setting and a rough painted hint of what you want.</p>",
        },
        {
          title: "Keep it and save it",
          target: ["Accept"],
          body: () =>
            "Press <b>Accept</b> on the one you like. Accept puts it on the canvas but not in the " +
            "gallery: right-click the canvas → <b>Save To Gallery</b> → <b>Save Canvas To Gallery</b> " +
            "(or use <b>Save To Gallery</b> in the toolbar under the canvas before you accept).",
        },
        {
          title: "Where to go next",
          body: () =>
            "That's the tour. Invoke can do a lot more (reference images, control layers, " +
            "upscaling), and its own guides cover it. They open in your browser:" +
            `<span class="docs">${DOCS.map(([k, t]) => `<a href="#" data-act="doc" data-doc="${k}">${esc(t)}</a>`).join("")}</span>` +
            "Want this tour again? Press <b>Show tutorial</b> in SlopTweak.",
          next: "Done",
        },
      ],
    },
  ];

  const stepsOf = (n) => STAGES[n - 1].steps.filter((s) => !s.jumpOnly || st.jumped);

  const CSS = `
    :host { all: initial; }
    .box { position: fixed; right: 16px; bottom: 16px; z-index: 2147483647;
      font: 14px/1.45 system-ui, "Segoe UI", sans-serif; color: #e6e8ee;
      background: #1c1f26; border: 1px solid #3b82f6; border-radius: 10px;
      box-shadow: 0 8px 28px rgba(0,0,0,.5); }
    .card { width: 370px; max-height: calc(100vh - 32px); overflow: auto; box-sizing: border-box;
      padding: 14px 16px 12px; }
    /* The home card: centered over a dimmed page so it can't be missed. */
    .scrim { position: fixed; inset: 0; z-index: 2147483646; background: rgba(0,0,0,.45); }
    .card.center { left: 50%; top: 50%; right: auto; bottom: auto; transform: translate(-50%, -50%); }
    .card.center .head { cursor: default; }
    .pill { padding: 6px 12px; cursor: pointer; }
    .head { display: flex; align-items: center; gap: 8px; color: #93a4c3; font-size: 12px;
      cursor: move; user-select: none; touch-action: none; margin: -6px -8px 0; padding: 6px 8px 0; }
    .head .sp { flex: 1; }
    h3 { margin: 6px 0 6px; font-size: 16px; color: #fff; }
    p, .body { margin: 0 0 10px; }
    .body p:last-child { margin-bottom: 0; }
    b { color: #fff; }
    code { background: #2a2f3a; padding: 1px 5px; border-radius: 4px; }
    pre { background: #2a2f3a; padding: 6px 8px; border-radius: 6px; margin: 6px 0 4px;
      white-space: pre-wrap; font: 12px/1.4 ui-monospace, Consolas, monospace; }
    img { display: block; width: 72px; height: 105px; object-fit: cover; border-radius: 6px;
      margin: 0 0 10px; border: 1px solid #3a3f4b; }
    ol { margin: 0 0 10px; padding: 0; list-style: none; }
    li button { width: 100%; text-align: left; margin: 0 0 4px; }
    li .ok { color: #4ade80; }
    .row { display: flex; gap: 8px; justify-content: flex-end; align-items: center; }
    .row .sp { flex: 1; }
    button { font: inherit; border-radius: 6px; border: 1px solid #3a3f4b; background: #2a2f3a;
      color: #e6e8ee; padding: 5px 12px; cursor: pointer; }
    button:disabled { opacity: .6; cursor: default; }
    button.primary { background: #3b82f6; border-color: #3b82f6; color: #fff; }
    button.x { border: none; background: none; padding: 0 4px; font-size: 16px; color: #93a4c3; }
    button.mini { padding: 0 6px; font-size: 12px; }
    button.link { border: none; background: none; color: #93a4c3; padding: 5px 4px; }
    .status { min-height: 1.4em; color: #fbbf24; font-size: 13px; margin: -4px 0 8px; }
    .err { color: #f87171; }
    .docs { display: flex; flex-direction: column; gap: 4px; margin: 8px 0; }
    a { color: #93c5fd; text-decoration: none; }
    a:hover { text-decoration: underline; }
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
  const MARGIN = 16;

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
    // `l` pins it to the left edge (stage 5) through window resizes.
    const r = pos.l !== undefined ? maxR - pos.l : pos.r;
    box.style.right = `${Math.min(Math.max(0, r), maxR)}px`;
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

  const head = (label, drag = true) => `<div class="head"${drag ? ' title="Drag to move"' : ""}><span>${label}</span><span class="sp"></span>
      <button class="x" data-act="min" title="Minimize">–</button>
      <button class="x" data-act="skip" title="Close the tutorial">×</button></div>`;

  function homeCard() {
    const first = st.resume === 1;
    const list = STAGES.map(
      (s, i) =>
        `<li><button data-act="jump" data-stage="${i + 1}">${i + 1 < st.resume ? '<span class="ok">✓</span> ' : ""}` +
        `${i + 1}. ${esc(s.title)}</button></li>`,
    ).join("");
    return `<div class="scrim"></div><div class="box card center" role="dialog" aria-label="SlopTweak tutorial" data-stage="0" data-step="0">
        ${head("SlopTweak tutorial", false)}
        <h3>${first ? "Welcome to Invoke" : "Welcome back"}</h3>
        <p>${
          first
            ? "Five short lessons with one character: write a prompt, save your favorite tags, " +
              "then fix and change the picture on the canvas. Leave whenever you like; SlopTweak " +
              "remembers the stage you're on, even on your next GPU."
            : `You were on stage ${st.resume}. Carry on, or pick any stage.`
        }</p>
        <ol>${list}</ol>
        <div class="status"></div>
        <div class="row">
          <button class="link" data-act="skip">Skip</button><span class="sp"></span>
          <button class="primary" data-act="start">${first ? "Start" : `Continue stage ${st.resume}`}</button>
        </div>
      </div>`;
  }

  function stepCard() {
    const stage = STAGES[st.stage - 1];
    const steps = stepsOf(st.stage);
    const s = steps[st.step] || steps[0];
    const last = st.stage === STAGES_N && st.step === steps.length - 1;
    const thumb =
      s.ours && st.image ? `<img alt="" src="/api/v1/images/i/${encodeURIComponent(st.image)}/thumbnail">` : "";
    return `<div class="ring"></div><div class="box card" role="dialog" aria-label="SlopTweak tutorial" data-stage="${st.stage}" data-step="${st.step}">
        ${head(`Stage ${st.stage} of ${STAGES_N} · ${esc(stage.title)} · ${st.step + 1}/${steps.length}`)}
        <h3>${esc(s.title)}</h3>
        <div class="body">${s.body()}</div>${thumb}
        <div class="status"></div>
        <div class="row">
          <button data-act="back">Back</button>
          ${s.ours && !st.image ? oursBtn : ""}
          <span class="sp"></span>
          <button class="primary" data-act="next">${esc(s.next || (last ? "Done" : "Next"))}</button>
        </div>
      </div>`;
  }

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
      const where = st.stage ? ` · stage ${st.stage} of ${STAGES_N}` : "";
      root.innerHTML = `<style>${CSS}</style><div class="box pill" data-act="restore">Tutorial${where}</div>`;
      place();
      return;
    }
    root.innerHTML = `<style>${CSS}</style>${st.stage ? stepCard() : homeCard()}`;
    place();
    const s = st.stage && stepsOf(st.stage)[st.step];
    if (s && s.watch) watchQueue(`${st.stage}.${st.step}`);
    if (s && s.target) trackRing(s.target);
  }

  async function queueStatus() {
    const r = await fetch("/api/v1/queue/default/status", { credentials: "same-origin" });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const j = await r.json();
    return j.queue || j;
  }

  /** Move on by itself once a generation finishes. */
  function watchQueue(key) {
    const tick = async () => {
      try {
        const q = await queueStatus();
        if (st.base[key] === undefined) {
          st.base[key] = q.completed || 0;
          store();
        }
        if ((q.completed || 0) > st.base[key]) {
          delete st.base[key];
          return next();
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

  /** Tell the app (it blocks this navigation, so the page stays). */
  function signal(kind) {
    try {
      window.location.assign(`/__sloptweak/tutorial/${kind}`);
    } catch {
      /* the app will offer the tutorial again next time */
    }
  }

  /** Enter stage `n` at its first step; `jumped` = from the home card. */
  function enter(n, jumped) {
    st.stage = clampStage(n);
    st.step = 0;
    st.jumped = jumped;
    st.base = {};
    st.resume = st.stage;
    store();
    render();
    // The visor stage works in the layer list, which the card covers at
    // bottom-right: move it to bottom-left. A later drag still wins.
    // Other stages drop that move (not a position the user dragged to).
    if (st.stage === STAGES_N) {
      st.pos = { l: MARGIN, b: MARGIN };
      store();
      place();
    } else if (st.pos && st.pos.l !== undefined) {
      delete st.pos;
      store();
      place();
    }
    if (st.signaled !== st.stage) {
      st.signaled = st.stage;
      store();
      signal(`stage/${st.stage}`);
    }
  }

  function next() {
    const steps = stepsOf(st.stage);
    if (st.step < steps.length - 1) {
      st.step += 1;
      store();
      return render();
    }
    if (st.stage < STAGES_N) return enter(st.stage + 1, false);
    return close("done");
  }

  function back() {
    if (st.step > 0) {
      st.step -= 1;
      delete st.base[`${st.stage}.${st.step}`];
    } else {
      st.stage = 0;
    }
    store();
    render();
  }

  /** Put the fallback portrait in Invoke's Assets (once per GPU), then reload so the gallery shows it. */
  async function ensurePortrait() {
    if (st.image) {
      const r = await fetch(`/api/v1/images/i/${encodeURIComponent(st.image)}`, { credentials: "same-origin" });
      if (r.ok) return false;
    }
    const bin = atob(CFG.portrait);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const form = new FormData();
    form.append("file", new Blob([bytes], { type: CFG.portraitType }), CFG.portraitName);
    const r = await fetch("/api/v1/images/upload?image_category=user&is_intermediate=false", {
      method: "POST",
      body: form,
      credentials: "same-origin",
    });
    if (!r.ok) throw new Error(`upload failed (HTTP ${r.status})`);
    st.image = (await r.json()).image_name;
    store();
    return true;
  }

  function close(kind) {
    st.status = "closed";
    if (kind === "done") {
      st.stage = 0;
      st.resume = 1;
      st.signaled = 1;
    }
    store();
    render();
    signal(kind);
  }

  async function onClick(e) {
    const el = e.target.closest("[data-act]");
    if (!el) return;
    const act = el.getAttribute("data-act");
    if (act === "skip") return close("skipped");
    if (act === "min" || act === "restore") {
      st.status = act === "min" ? "min" : "open";
      store();
      return render();
    }
    if (act === "back") return back();
    if (act === "next") return next();
    if (act === "start") return enter(st.resume, st.resume > 1);
    if (act === "jump") {
      const n = Number(el.getAttribute("data-stage"));
      return enter(n, n > 1);
    }
    if (act === "page") return signal("model-page");
    if (act === "doc") {
      e.preventDefault();
      // The app maps the key to a fixed Invoke page; the page never sends a URL.
      try {
        window.location.assign(`/__sloptweak/docs/${encodeURIComponent(el.getAttribute("data-doc") || "")}`);
      } catch {
        /* nothing to do */
      }
      return;
    }
    if (act === "copy") {
      try {
        await navigator.clipboard.writeText(COPY[el.getAttribute("data-copy")] || "");
        el.textContent = "Copied";
      } catch {
        el.textContent = "Select it and copy";
      }
      return;
    }
    if (act === "ours") {
      el.disabled = true;
      setStatus("Getting our picture ready…");
      try {
        // Invoke's gallery doesn't hear about uploads made outside its own
        // UI, so reload once to show it.
        if (await ensurePortrait()) return window.location.reload();
        return render();
      } catch (err) {
        el.disabled = false;
        return setStatus(`Couldn't load our picture: ${err.message}`, "err");
      }
    }
  }

  window.__slopTweakTutorial = {
    open() {
      st.status = "open";
      store();
      render();
    },
  };

  window.addEventListener("resize", place);
  const boot = () => render();
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
  else boot();
})
