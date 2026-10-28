// Layout mode for the MOSS panel: draw labelled boxes on the picture, and give a box its own
// LoRA. The request it builds is what src-tauri/src/moss/layout.rs checks and turns into one
// ComfyUI graph (docs/spikes/regional-lora-design.md). The checks here mirror that file's
// limits so the Make button can say why it is off; the Rust side is the authority.
//
// Plain DOM: boxes are absolutely positioned divs over an SVG grid. Everything the pointer
// does has a keyboard path (see HELP) and the boxes' numbers are also editable as fields.

import "./layout.css";

export type BoxKind = "object" | "character" | "text";
/** `[y0, x0, y1, x1]`, each 0..1000 across the whole picture (ComfyUI's box units). */
export type Rect = [number, number, number, number];

export interface LoraOption {
  /** What the request sends (`user/<file>.safetensors`). */
  name: string;
  label: string;
}

interface BoxLora {
  file: string;
  strength: number;
  denoise: number;
}

interface Box {
  kind: BoxKind;
  caption: string;
  text: string;
  rect: Rect;
  lora: BoxLora | null;
}

/** The `layout` field of `moss_generate`'s request. */
export interface LayoutPayload {
  scene: string;
  boxes: { kind: BoxKind; caption: string; text: string | null; rect: Rect; lora: BoxLora | null }[];
}

export interface LayoutContext {
  width: number;
  height: number;
  busy: boolean;
  loras: LoraOption[];
}

export interface LayoutEditor {
  /** Re-render for the picture size, the busy flag and the LoRAs on the GPU. */
  update(ctx: LayoutContext): void;
  request(scene: string): LayoutPayload;
  /** Why the picture can't be made yet, or null when the layout is fine. */
  problem(scene: string): string | null;
  count(): number;
}

export interface LayoutHooks {
  /** Called after any change to the boxes. */
  onChange(): void;
  /** Ask the panel to add a LoRA file to the GPU (its own flow). */
  onAddLora(): void;
}

/** Mirrors the constants in src-tauri/src/moss/layout.rs. */
export const LIMITS = {
  maxBoxes: 12,
  maxLoraPasses: 6,
  minBoxPx: 64,
  captionChars: 500,
  textChars: 200,
  minStrength: 0,
  maxStrength: 1.5,
  minDenoise: 0.4,
  maxDenoise: 0.9,
  defaultStrength: 1,
  defaultDenoise: 0.7,
} as const;

const COORD = 1000;
const KINDS: BoxKind[] = ["object", "character", "text"];
const KIND_LABEL: Record<BoxKind, string> = { object: "Object", character: "Character", text: "Text" };
const HANDLES = ["nw", "n", "ne", "e", "se", "s", "sw", "w"] as const;
const HELP =
  "Drag on the picture to draw a box, and say what is in it. Boxes are drawn in order, so put the " +
  "background first; later boxes sit on top. With the keyboard: Tab to a box, arrow keys move it " +
  "(Shift = 10 at a time), Alt + arrows resize it, [ and ] change its order, Delete removes it.";

const clamp = (v: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, v));

/** The smallest box edge, in box units, that is still `minBoxPx` pixels on a side of `side` px. */
function minUnits(side: number): number {
  return Math.ceil((LIMITS.minBoxPx * COORD) / side);
}

/** A box's size in pixels, the way the Rust side rounds it (floor the start, ceil the end). */
function pxSize(r: Rect, w: number, h: number): [number, number] {
  const [y0, x0, y1, x1] = r;
  const wpx = Math.floor((x1 * w + COORD - 1) / COORD) - Math.floor((x0 * w) / COORD);
  const hpx = Math.floor((y1 * h + COORD - 1) / COORD) - Math.floor((y0 * h) / COORD);
  return [wpx, hpx];
}

function denoiseWord(d: number): string {
  return d < 0.6 ? "gentle" : d < 0.8 ? "balanced" : "redraw";
}

type Attr = string | boolean | ((e: Event) => void) | undefined;

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Record<string, Attr> = {},
  ...children: (Node | string | null | false)[]
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === false) continue;
    if (typeof v === "function") e.addEventListener(k.replace(/^on/, ""), v);
    else if (v === true) e.setAttribute(k, "");
    else if (k === "class") e.className = v;
    else e.setAttribute(k, v);
  }
  for (const c of children) if (c) e.append(c);
  return e;
}

function svgGrid(): SVGSVGElement {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("class", "lay-grid");
  svg.setAttribute("viewBox", `0 0 ${COORD} ${COORD}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  for (let i = 1; i < 10; i++) {
    for (const [x1, y1, x2, y2] of [
      [i * 100, 0, i * 100, COORD],
      [0, i * 100, COORD, i * 100],
    ] as const) {
      const line = document.createElementNS(ns, "line");
      line.setAttribute("x1", String(x1));
      line.setAttribute("y1", String(y1));
      line.setAttribute("x2", String(x2));
      line.setAttribute("y2", String(y2));
      svg.append(line);
    }
  }
  return svg;
}

type Drag =
  | { mode: "create"; start: [number, number]; ghost: HTMLElement }
  | { mode: "move"; index: number; start: [number, number]; rect: Rect; el: HTMLElement }
  | { mode: "resize"; index: number; handle: string; el: HTMLElement };

export function mountLayout(root: HTMLElement, hooks: LayoutHooks): LayoutEditor {
  let boxes: Box[] = [];
  let selected: number | null = null;
  let ctx: LayoutContext = { width: 1024, height: 1024, busy: false, loras: [] };
  let sig = "";
  /** The LoRA names present when "Add a LoRA file" was pressed, until a new one shows up. */
  let awaiting: { had: Set<string>; until: number } | null = null;
  let drag: Drag | null = null;
  let boxEls: HTMLElement[] = [];

  const note = h("p", { id: "lay-note", class: "muted small", "aria-live": "polite" });
  const canvas = h("div", {
    id: "lay-canvas",
    class: "lay-canvas",
    role: "group",
    "aria-label": "Layout canvas. Drag to draw a box.",
  });
  const ghost = h("div", { class: "lay-box lay-ghost", hidden: true });
  const count = h("span", { class: "muted small", id: "lay-count" });
  const addBox = h("button", { id: "lay-add", type: "button", onclick: () => addDefault([250, 250, 750, 750]) }, "Add a box");
  const addBg = h("button", { id: "lay-add-bg", type: "button", class: "ghost", onclick: () => addDefault([0, 0, COORD, COORD]) }, "Add a background box");
  const list = h("ol", { id: "lay-list", class: "lay-list" });
  const form = h("div", { id: "lay-form", class: "lay-form" });

  root.replaceChildren(
    h("p", { class: "muted small" }, HELP),
    h("div", { class: "row" }, addBox, addBg, count),
    canvas,
    note,
    list,
    form,
  );

  const sel = (): Box | null => (selected === null ? null : (boxes[selected] ?? null));
  const say = (text: string): void => {
    note.textContent = text;
  };

  function point(e: PointerEvent): [number, number] {
    const r = canvas.getBoundingClientRect();
    const x = clamp(Math.round(((e.clientX - r.left) / r.width) * COORD), 0, COORD);
    const y = clamp(Math.round(((e.clientY - r.top) / r.height) * COORD), 0, COORD);
    return [x, y];
  }

  function place(el: HTMLElement, r: Rect): void {
    const [y0, x0, y1, x1] = r;
    el.style.left = `${x0 / 10}%`;
    el.style.top = `${y0 / 10}%`;
    el.style.width = `${(x1 - x0) / 10}%`;
    el.style.height = `${(y1 - y0) / 10}%`;
  }

  const minXY = (): [number, number] => [minUnits(ctx.width), minUnits(ctx.height)];

  function describe(b: Box, i: number): string {
    const what = b.caption.trim() || "no description yet";
    return `Box ${i + 1}, ${b.kind}, ${what}${b.lora ? ", with a LoRA" : ""}`;
  }

  // ----- rendering -------------------------------------------------------------------------

  function renderCanvas(refocus = false): void {
    for (const el of boxEls) el.remove();
    boxEls = boxes.map((b, i) => {
      const on = i === selected;
      const el = h(
        "div",
        {
          class: `lay-box lay-${b.kind}${on ? " sel" : ""}${b.lora ? " has-lora" : ""}`,
          tabindex: "0",
          role: "button",
          "aria-pressed": String(on),
          "aria-label": describe(b, i),
          "data-i": String(i),
        },
        h("span", { class: "lay-tag" }, `${i + 1} · ${KIND_LABEL[b.kind]}${b.lora ? " · LoRA" : ""}`),
      );
      place(el, b.rect);
      if (on && !ctx.busy) {
        for (const handle of HANDLES) el.append(h("span", { class: `lay-h lay-h-${handle}`, "data-h": handle }));
      }
      el.addEventListener("focus", () => {
        // Selecting rebuilds the boxes, so put the focus back on the rebuilt one.
        if (selected !== i && !drag) select(i, true);
      });
      return el;
    });
    canvas.append(...boxEls, ghost);
    canvas.classList.toggle("busy", ctx.busy);
    if (refocus && selected !== null) boxEls[selected]?.focus();
    const n = boxes.length;
    count.textContent = `${n} of ${LIMITS.maxBoxes} boxes`;
    addBox.disabled = ctx.busy || n >= LIMITS.maxBoxes;
    addBg.disabled = addBox.disabled;
  }

  function renderList(): void {
    list.replaceChildren(
      ...boxes.map((b, i) => {
        const label = `${i + 1}. ${KIND_LABEL[b.kind]}: ${b.caption.trim().slice(0, 36) || "(no description)"}`;
        const lora = b.lora ? ` · ${loraLabel(b.lora.file)}` : "";
        return h(
          "li",
          { class: i === selected ? "sel" : "" },
          h("button", { type: "button", class: "ghost lay-pick", "aria-pressed": String(i === selected), onclick: () => select(i, true) }, label + lora),
          h("button", { type: "button", class: "ghost", title: "Draw earlier", "aria-label": `Move box ${i + 1} earlier`, disabled: ctx.busy || i === 0, onclick: () => reorder(i, -1) }, "↑"),
          h("button", { type: "button", class: "ghost", title: "Draw later", "aria-label": `Move box ${i + 1} later`, disabled: ctx.busy || i === boxes.length - 1, onclick: () => reorder(i, 1) }, "↓"),
          h("button", { type: "button", class: "ghost", title: "Delete", "aria-label": `Delete box ${i + 1}`, disabled: ctx.busy, onclick: () => remove(i) }, "✕"),
        );
      }),
    );
  }

  const loraLabel = (file: string): string => ctx.loras.find((l) => l.name === file)?.label ?? file.replace(/^user\//, "");

  function field(label: string, id: string, input: HTMLElement, extra?: HTMLElement | false): HTMLElement {
    return h("div", { class: "row" }, h("label", { for: id }, label), input, extra || null);
  }

  function renderForm(): void {
    const b = sel();
    if (!b) {
      form.replaceChildren(h("p", { class: "muted small" }, boxes.length ? "Pick a box to edit it." : "Draw a box on the picture, or press Add a box."));
      return;
    }
    const busy = ctx.busy;
    const kind = h("select", {
      id: "lay-kind",
      disabled: busy,
      onchange: (e) => {
        b.kind = (e.target as HTMLSelectElement).value as BoxKind;
        if (b.kind === "text") b.lora = null;
        changed();
        renderForm();
      },
    });
    for (const k of KINDS) kind.append(new Option(KIND_LABEL[k], k));
    kind.value = b.kind;

    const caption = h("textarea", {
      id: "lay-caption",
      rows: "2",
      maxlength: String(LIMITS.captionChars),
      disabled: busy,
      placeholder: b.kind === "text" ? "The sign or surface the words are on" : "What is in this box, e.g. a tall woman in a red coat",
      oninput: (e) => {
        b.caption = (e.target as HTMLTextAreaElement).value;
        changed(false);
      },
    });
    caption.value = b.caption;

    const rows: HTMLElement[] = [field("Kind", "lay-kind", kind), field("In the box", "lay-caption", caption)];

    if (b.kind === "text") {
      const text = h("input", {
        id: "lay-text",
        type: "text",
        maxlength: String(LIMITS.textChars),
        disabled: busy,
        placeholder: "The words to draw",
        oninput: (e) => {
          b.text = (e.target as HTMLInputElement).value;
          changed(false);
        },
      });
      text.value = b.text;
      rows.push(field("Words", "lay-text", text));
    }

    const pos = h("span", { class: "lay-pos" });
    (["Top", "Left", "Bottom", "Right"] as const).forEach((name, idx) => {
      const input = h("input", {
        id: `lay-${["y0", "x0", "y1", "x1"][idx]}`,
        type: "number",
        min: "0",
        max: String(COORD),
        step: "1",
        "aria-label": `${name} edge, 0 to ${COORD}`,
        disabled: busy,
        onchange: (e) => setEdge(idx, Number((e.target as HTMLInputElement).value)),
      });
      input.value = String(b.rect[idx]);
      pos.append(h("label", { class: "lay-edge" }, name, input));
    });
    rows.push(h("div", { class: "row" }, h("span", { class: "lay-rowlabel" }, "Position"), pos));

    if (b.kind !== "text") {
      const lora = h("select", {
        id: "lay-lora",
        disabled: busy,
        "aria-label": "LoRA for this box",
        onchange: (e) => {
          const v = (e.target as HTMLSelectElement).value;
          b.lora = v ? { file: v, strength: b.lora?.strength ?? LIMITS.defaultStrength, denoise: b.lora?.denoise ?? LIMITS.defaultDenoise } : null;
          changed();
          renderForm();
        },
      });
      lora.append(new Option("No LoRA", ""));
      for (const l of ctx.loras) lora.append(new Option(l.label, l.name));
      if (b.lora && !ctx.loras.some((l) => l.name === b.lora?.file)) {
        lora.append(new Option(`${loraLabel(b.lora.file)} (not on this GPU)`, b.lora.file));
      }
      lora.value = b.lora?.file ?? "";
      const add = h(
        "button",
        {
          id: "lay-add-lora",
          type: "button",
          disabled: busy,
          onclick: () => {
            awaiting = { had: new Set(ctx.loras.map((l) => l.name)), until: Date.now() + 120_000 };
            hooks.onAddLora();
          },
        },
        "Add a LoRA file…",
      );
      rows.push(field("LoRA", "lay-lora", lora, add));
      if (b.lora) {
        const lo = b.lora;
        const sv = h("strong", { id: "lay-strength-val" }, lo.strength.toFixed(2));
        const strength = h("input", {
          id: "lay-strength",
          type: "range",
          min: String(LIMITS.minStrength),
          max: String(LIMITS.maxStrength),
          step: "0.05",
          disabled: busy,
          oninput: (e) => {
            lo.strength = Number((e.target as HTMLInputElement).value);
            sv.textContent = lo.strength.toFixed(2);
            changed(false);
          },
        });
        strength.value = String(lo.strength);
        rows.push(field("Strength", "lay-strength", strength, sv));
        const dv = h("strong", { id: "lay-denoise-val" }, `${lo.denoise.toFixed(2)} · ${denoiseWord(lo.denoise)}`);
        const denoise = h("input", {
          id: "lay-denoise",
          type: "range",
          min: String(LIMITS.minDenoise),
          max: String(LIMITS.maxDenoise),
          step: "0.05",
          disabled: busy,
          oninput: (e) => {
            lo.denoise = Number((e.target as HTMLInputElement).value);
            dv.textContent = `${lo.denoise.toFixed(2)} · ${denoiseWord(lo.denoise)}`;
            changed(false);
          },
        });
        denoise.value = String(lo.denoise);
        rows.push(field("Change amount", "lay-denoise", denoise, dv));
        rows.push(
          h("p", { class: "muted small" }, "Gentle keeps the pose and restyles; redraw lets the LoRA change the pose and the box's background too. The LoRA only touches this box."),
        );
      }
    }
    rows.push(h("div", { class: "actions" }, h("button", { id: "lay-delete", type: "button", class: "danger", disabled: busy, onclick: () => selected !== null && remove(selected) }, "Delete this box")));
    form.replaceChildren(...rows);
  }

  function renderAll(refocus = false): void {
    renderCanvas(refocus);
    renderList();
    renderForm();
  }

  /** The boxes changed: refresh what shows them (not the form being typed in) and tell the panel. */
  function changed(redrawForm = false): void {
    renderCanvas();
    renderList();
    if (redrawForm) renderForm();
    hooks.onChange();
  }

  // ----- editing ---------------------------------------------------------------------------

  function select(i: number | null, focus: boolean): void {
    selected = i;
    renderAll(focus);
  }

  function addDefault(rect: Rect): void {
    if (boxes.length >= LIMITS.maxBoxes) return say(`${LIMITS.maxBoxes} boxes is the limit.`);
    say("");
    boxes.push({ kind: "object", caption: "", text: "", rect, lora: null });
    selected = boxes.length - 1;
    renderAll();
    hooks.onChange();
    root.querySelector<HTMLElement>("#lay-caption")?.focus();
  }

  function remove(i: number): void {
    boxes.splice(i, 1);
    selected = boxes.length === 0 ? null : Math.min(i, boxes.length - 1);
    say("");
    renderAll(true);
    hooks.onChange();
  }

  function reorder(i: number, by: -1 | 1): void {
    const j = i + by;
    const a = boxes[i];
    const b = boxes[j];
    if (!a || !b) return;
    boxes[i] = b;
    boxes[j] = a;
    selected = j;
    renderAll(true);
    hooks.onChange();
  }

  /** One edge typed into its field (0 = top, 1 = left, 2 = bottom, 3 = right). */
  function setEdge(idx: number, value: number): void {
    const b = sel();
    if (!b || !Number.isFinite(value)) return renderForm();
    const [mx, my] = minXY();
    const r = [...b.rect] as Rect;
    const v = clamp(Math.round(value), 0, COORD);
    if (idx === 0) r[0] = Math.min(v, r[2] - my);
    if (idx === 1) r[1] = Math.min(v, r[3] - mx);
    if (idx === 2) r[2] = Math.max(v, r[0] + my);
    if (idx === 3) r[3] = Math.max(v, r[1] + mx);
    b.rect = [clamp(r[0], 0, COORD), clamp(r[1], 0, COORD), clamp(r[2], 0, COORD), clamp(r[3], 0, COORD)];
    renderCanvas();
    renderForm();
    hooks.onChange();
  }

  function nudge(i: number, key: string, big: boolean, resize: boolean): void {
    const b = boxes[i];
    if (!b) return;
    const step = big ? 10 : 1;
    const dx = key === "ArrowRight" ? step : key === "ArrowLeft" ? -step : 0;
    const dy = key === "ArrowDown" ? step : key === "ArrowUp" ? -step : 0;
    const [mx, my] = minXY();
    let [y0, x0, y1, x1] = b.rect;
    if (resize) {
      x1 = clamp(x1 + dx, x0 + mx, COORD);
      y1 = clamp(y1 + dy, y0 + my, COORD);
    } else {
      const w = x1 - x0;
      const hh = y1 - y0;
      x0 = clamp(x0 + dx, 0, COORD - w);
      y0 = clamp(y0 + dy, 0, COORD - hh);
      x1 = x0 + w;
      y1 = y0 + hh;
    }
    b.rect = [y0, x0, y1, x1];
    renderCanvas(true);
    renderForm();
    hooks.onChange();
  }

  canvas.addEventListener("keydown", (e) => {
    if (ctx.busy) return;
    const el = (e.target as HTMLElement).closest<HTMLElement>(".lay-box");
    const i = el ? Number(el.dataset["i"]) : NaN;
    if (!el || Number.isNaN(i)) return;
    if (e.key.startsWith("Arrow")) {
      e.preventDefault();
      nudge(i, e.key, e.shiftKey, e.altKey);
    } else if (e.key === "Delete" || e.key === "Backspace") {
      e.preventDefault();
      remove(i);
    } else if (e.key === "[" || e.key === "]") {
      e.preventDefault();
      reorder(i, e.key === "[" ? -1 : 1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      root.querySelector<HTMLElement>("#lay-caption")?.focus();
    } else if (e.key === "Escape") {
      selected = null;
      renderAll();
      (canvas as HTMLElement).focus();
    }
  });

  canvas.addEventListener("pointerdown", (e) => {
    if (ctx.busy || e.button !== 0) return;
    const t = e.target as HTMLElement;
    const handle = t.closest<HTMLElement>(".lay-h")?.dataset["h"];
    const boxEl = t.closest<HTMLElement>(".lay-box:not(.lay-ghost)");
    const p = point(e);
    e.preventDefault();
    canvas.setPointerCapture(e.pointerId);
    const i = boxEl ? Number(boxEl.dataset["i"]) : NaN;
    const b = Number.isNaN(i) ? undefined : boxes[i];
    if (b && boxEl) {
      if (selected !== i) select(i, false);
      const el = boxEls[i];
      if (!el) return;
      drag = handle ? { mode: "resize", index: i, handle, el } : { mode: "move", index: i, start: p, rect: [...b.rect] as Rect, el };
    } else {
      drag = { mode: "create", start: p, ghost };
      ghost.hidden = false;
      place(ghost, [p[1], p[0], p[1], p[0]]);
    }
  });

  canvas.addEventListener("pointermove", (e) => {
    if (!drag) return;
    const [x, y] = point(e);
    const [mx, my] = minXY();
    if (drag.mode === "create") {
      const [sx, sy] = drag.start;
      place(drag.ghost, [Math.min(y, sy), Math.min(x, sx), Math.max(y, sy), Math.max(x, sx)]);
      drag.ghost.classList.toggle("tiny", Math.abs(x - sx) < mx || Math.abs(y - sy) < my);
      return;
    }
    const b = boxes[drag.index];
    if (!b) return;
    if (drag.mode === "move") {
      const [y0, x0, y1, x1] = drag.rect;
      const dx = clamp(x - drag.start[0], -x0, COORD - x1);
      const dy = clamp(y - drag.start[1], -y0, COORD - y1);
      b.rect = [y0 + dy, x0 + dx, y1 + dy, x1 + dx];
    } else {
      let [y0, x0, y1, x1] = b.rect;
      if (drag.handle.includes("n")) y0 = clamp(y, 0, y1 - my);
      if (drag.handle.includes("s")) y1 = clamp(y, y0 + my, COORD);
      if (drag.handle.includes("w")) x0 = clamp(x, 0, x1 - mx);
      if (drag.handle.includes("e")) x1 = clamp(x, x0 + mx, COORD);
      b.rect = [y0, x0, y1, x1];
    }
    place(drag.el, b.rect);
  });

  function endDrag(e: PointerEvent, cancelled: boolean): void {
    const d = drag;
    drag = null;
    if (!d) return;
    if (canvas.hasPointerCapture(e.pointerId)) canvas.releasePointerCapture(e.pointerId);
    if (d.mode === "create") {
      d.ghost.hidden = true;
      d.ghost.classList.remove("tiny");
      const [x, y] = point(e);
      const [sx, sy] = d.start;
      const [mx, my] = minXY();
      const big = Math.abs(x - sx) >= mx && Math.abs(y - sy) >= my;
      if (cancelled || !big) {
        // A plain click on the empty canvas just deselects.
        if (!cancelled && Math.abs(x - sx) < 4 && Math.abs(y - sy) < 4) select(null, false);
        else if (!cancelled) say("That box is too small. Drag a bigger one.");
        return;
      }
      if (boxes.length >= LIMITS.maxBoxes) return say(`${LIMITS.maxBoxes} boxes is the limit.`);
      say("");
      boxes.push({ kind: "object", caption: "", text: "", rect: [Math.min(y, sy), Math.min(x, sx), Math.max(y, sy), Math.max(x, sx)], lora: null });
      selected = boxes.length - 1;
      renderAll();
      hooks.onChange();
      root.querySelector<HTMLElement>("#lay-caption")?.focus();
      return;
    }
    renderCanvas();
    renderForm();
    hooks.onChange();
  }
  canvas.addEventListener("pointerup", (e) => endDrag(e, false));
  canvas.addEventListener("pointercancel", (e) => endDrag(e, true));

  canvas.prepend(svgGrid());
  renderAll();

  // ----- the panel's view of it -------------------------------------------------------------

  function problem(scene: string): string | null {
    if (scene.trim() === "") return "Describe the whole picture first.";
    if (boxes.length === 0) return "Draw at least one box.";
    let passes = 0;
    for (const [i, b] of boxes.entries()) {
      const n = i + 1;
      const [wpx, hpx] = pxSize(b.rect, ctx.width, ctx.height);
      if (wpx < LIMITS.minBoxPx || hpx < LIMITS.minBoxPx) return `Box ${n} is too small (at least ${LIMITS.minBoxPx} pixels on each side).`;
      if (b.kind === "text") {
        if (b.text.trim() === "") return `Box ${n} is a text box: type the words to draw.`;
      } else if (b.caption.trim() === "") {
        return `Box ${n} needs a description of what is in it.`;
      }
      if (b.lora) {
        passes += 1;
        if (!ctx.loras.some((l) => l.name === b.lora?.file)) return `Box ${n} uses a LoRA that isn't on this GPU. Add it again.`;
      }
    }
    if (passes > LIMITS.maxLoraPasses) return `At most ${LIMITS.maxLoraPasses} boxes can have a LoRA.`;
    return null;
  }

  return {
    update(next) {
      const nextSig = JSON.stringify([next.width, next.height, next.busy, next.loras.map((l) => l.name)]);
      ctx = next;
      if (awaiting) {
        const fresh = next.loras.find((l) => !awaiting?.had.has(l.name));
        if (fresh) {
          // The file the user just added goes to the box they were editing, if it has no LoRA yet.
          const b = sel();
          if (b && b.kind !== "text" && !b.lora) b.lora = { file: fresh.name, strength: LIMITS.defaultStrength, denoise: LIMITS.defaultDenoise };
          awaiting = null;
          sig = "";
        } else if (Date.now() > awaiting.until) {
          awaiting = null;
        }
      }
      if (nextSig === sig) return;
      sig = nextSig;
      canvas.style.aspectRatio = `${next.width} / ${next.height}`;
      canvas.style.width = `min(100%, 560px, calc(60vh * ${next.width / next.height}))`;
      renderAll();
    },
    request(scene) {
      return {
        scene,
        boxes: boxes.map((b) => ({
          kind: b.kind,
          caption: b.caption.trim(),
          text: b.kind === "text" ? b.text.trim() : null,
          rect: [...b.rect] as Rect,
          lora: b.kind !== "text" && b.lora ? { ...b.lora } : null,
        })),
      };
    },
    problem,
    count: () => boxes.length,
  };
}
