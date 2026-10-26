// The Train tab: pick a dataset, check it, choose a base model and limits, get
// an exact quote, approve it, and watch the run. All logic that spends money
// lives in Rust (train/manager.rs); this only shows and asks.

import { invoke } from "@tauri-apps/api/core";

interface TrainSettings {
  output_name: string;
  rank: number;
  alpha: number;
  learning_rate: number;
  epochs: number;
  repeats: number;
  warmup_fraction: number;
  resolution: number;
  save_points: number;
  seed: number;
  keep_tokens: number;
  eval_strengths: number[];
  eval_seeds: number[];
  eval_preset: "quick" | "thorough";
}

interface ImageInfo {
  id: string;
  width: number;
  height: number;
  first_tag: string;
  tags: number;
}

interface Issue {
  severity: "error" | "warning";
  message: string;
}

interface Plan {
  images: number;
  total_steps: number;
  warmup_steps: number;
  checkpoint_steps: number[];
  eval_images: number;
}

interface AuditView {
  report: { images: ImageInfo[]; issues: Issue[]; buckets: Record<string, number> };
  plan: Plan | null;
  settings_error: string | null;
}

interface QuoteView {
  plan_id: string;
  gpu_name: string;
  gpu_location: string | null;
  hourly: number;
  max_minutes: number;
  max_usd: number;
  worst_usd: number;
  expected_minutes: number;
  plan: Plan;
  expires_unix: number;
}

interface Status {
  state: "idle" | "running" | "finished";
  message: string;
  step: number | null;
  total_steps: number | null;
  eval_done: number | null;
  eval_total: number | null;
  gpu_name: string | null;
  hourly: number | null;
  elapsed_secs: number | null;
  max_minutes: number | null;
  spent_usd: number | null;
  max_usd: number | null;
  instance_id: number | null;
  results_dir: string | null;
  synced: boolean;
  outcome: "completed" | "failed" | "deadline" | "cancelled" | null;
  error: string | null;
  destroyed: boolean;
  setup_detail: string | null;
  setup_log: string | null;
  setup_quiet_secs: number | null;
}

const money = (n: number) => `$${n.toFixed(2)}`;

function minutes(secs: number): string {
  const m = Math.floor(secs / 60);
  return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min ${secs % 60} s`;
}

type Child = Node | string | null | undefined | false;

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<Record<string, string | boolean | ((e: Event) => void)>> = {},
  ...children: Child[]
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

// The base checkpoint the first run was validated on.
const DEFAULT_BASE = "https://civitai.com/models/1261882?modelVersionId=3407912";

export function mountTrain(root: HTMLElement): { show: () => void; hide: () => void } {
  let settings: TrainSettings | null = null;
  let dir = "";
  let audit: AuditView | null = null;
  let quote: QuoteView | null = null;
  let status: Status | null = null;
  let busy = false;
  let error = "";
  let baseLink = DEFAULT_BASE;
  let maxMinutes = 120;
  let maxUsd = 1.5;
  let timer: number | undefined;

  const trigger = () => audit?.report.images[0]?.first_tag ?? "";

  async function load(): Promise<void> {
    const d = await invoke<{ settings: TrainSettings }>("train_defaults");
    settings = d.settings;
    status = await invoke<Status>("train_status");
    render();
  }

  async function act(p: Promise<unknown>): Promise<void> {
    busy = true;
    error = "";
    render();
    try {
      await p;
    } catch (e) {
      error = String(e);
    }
    busy = false;
    render();
  }

  async function pick(): Promise<void> {
    const picked = await invoke<string | null>("train_pick_dataset");
    if (!picked) return;
    dir = picked;
    quote = null;
    await runAudit();
  }

  async function runAudit(): Promise<void> {
    if (!settings || !dir) return;
    await act(
      invoke<AuditView>("train_audit", { dir, settings }).then((a) => {
        audit = a;
        if (settings && a.report.images.length > 0 && settings.output_name === "lora") {
          settings.output_name = (a.report.images[0]?.first_tag ?? "").replace(/[^A-Za-z0-9_-]/g, "_").slice(0, 48) || "lora";
        }
      }),
    );
  }

  async function getQuote(): Promise<void> {
    if (!settings) return;
    await act(
      invoke<QuoteView>("train_quote", {
        args: {
          dir,
          settings,
          trigger: trigger(),
          base_link: baseLink.trim(),
          max_minutes: maxMinutes,
          max_usd: maxUsd,
        },
      }).then((q) => {
        quote = q;
      }),
    );
  }

  async function start(): Promise<void> {
    if (!quote) return;
    const id = quote.plan_id;
    quote = null;
    await act(invoke("train_start", { planId: id }).then(poll));
  }

  async function poll(): Promise<void> {
    try {
      status = await invoke<Status>("train_status");
    } catch (e) {
      error = String(e);
    }
    render();
  }

  // ----- views ---------------------------------------------------------------

  function field(label: string, input: HTMLElement, hint = ""): Node[] {
    return [h("label", {}, label), input, hint ? h("span", { class: "muted small hint" }, hint) : ""] as Node[];
  }

  function num(
    value: number,
    on: (v: number) => void,
    attrs: Record<string, string> = {},
  ): HTMLInputElement {
    const i = h("input", { type: "number", ...attrs });
    i.value = String(value);
    i.addEventListener("change", () => {
      const v = Number(i.value);
      if (Number.isFinite(v)) {
        on(v);
        quote = null;
        if (dir) void runAudit();
      }
    });
    return i;
  }

  function renderSetup(): Node {
    const s = settings!;
    const card = h("section", { class: "card" }, h("h2", {}, "Train a LoRA"));
    card.append(
      h(
        "p",
        { class: "muted" },
        "Teach a model a character or style from a folder of images, each with a caption file of the same name. SlopTweak rents a GPU, trains, downloads the result, and destroys the GPU.",
      ),
    );
    card.append(
      h(
        "div",
        { class: "row" },
        h("label", {}, "Dataset"),
        h("span", { class: "path" }, dir || "No folder picked"),
        h("button", { onclick: () => void pick(), disabled: busy }, dir ? "Change…" : "Pick folder…"),
      ),
    );
    if (audit) card.append(...renderAudit(audit));
    if (audit?.plan) {
      card.append(renderSettings(s));
      card.append(renderLimits());
    }
    if (error) card.append(h("p", { class: "error" }, error));
    return card;
  }

  function renderAudit(a: AuditView): Node[] {
    const out: Node[] = [];
    const n = a.report.images.length;
    const buckets = Object.entries(a.report.buckets)
      .map(([k, v]) => `${v} at ${k}`)
      .join(", ");
    out.push(
      h(
        "p",
        {},
        `${n} usable image${n === 1 ? "" : "s"}`,
        buckets ? ` (${buckets})` : "",
        trigger() ? " · trigger word: " : "",
        trigger() ? h("b", {}, trigger()) : "",
      ),
    );
    for (const i of a.report.issues) {
      out.push(h("p", { class: i.severity === "error" ? "error small" : "muted small" }, `${i.severity === "error" ? "Problem" : "Note"}: ${i.message}`));
    }
    if (a.settings_error) out.push(h("p", { class: "error small" }, a.settings_error));
    return out;
  }

  function renderSettings(s: TrainSettings): Node {
    const d = h("details", {});
    d.append(h("summary", {}, "Training settings"));
    const form = h("div", { class: "form" });
    const name = h("input", { type: "text", maxlength: "48" });
    name.value = s.output_name;
    name.addEventListener("change", () => {
      s.output_name = name.value.trim();
      quote = null;
    });
    const preset = h("select", {});
    for (const [value, label] of [
      ["quick", "Quick (about 12 images)"],
      ["thorough", "Thorough (every checkpoint and strength)"],
    ]) {
      const o = h("option", { value }, label);
      if (s.eval_preset === value) o.selected = true;
      preset.append(o);
    }
    preset.addEventListener("change", () => {
      s.eval_preset = preset.value === "thorough" ? "thorough" : "quick";
      quote = null;
      if (dir) void runAudit();
    });
    form.append(
      ...field("LoRA name", name),
      ...field("Test images", preset, "Thorough lets you compare checkpoints, and takes about 10 minutes longer"),
      ...field("Epochs", num(s.epochs, (v) => (s.epochs = v), { min: "1", max: "400" }), "passes over your images"),
      ...field("Rank", num(s.rank, (v) => ((s.rank = v), (s.alpha = Math.min(s.alpha, v))), { min: "1", max: "128" }), "capacity; 16 suits one character"),
      ...field("Learning rate", num(s.learning_rate, (v) => (s.learning_rate = v), { step: "0.00001", min: "0.000001", max: "0.001" })),
      ...field("Saved checkpoints", num(s.save_points, (v) => (s.save_points = v), { min: "1", max: "10" }), "compare them to pick the best"),
    );
    d.append(form);
    return d;
  }

  function renderLimits(): Node {
    const box = h("div", { class: "limits" });
    const link = h("input", { type: "text", placeholder: "CivitAI link to an SDXL checkpoint" });
    link.value = baseLink;
    link.addEventListener("change", () => {
      baseLink = link.value;
      quote = null;
    });
    box.append(
      h("div", { class: "row" }, h("label", {}, "Base model"), link),
      h(
        "div",
        { class: "row" },
        h("label", {}, "Time limit (min)"),
        num(maxMinutes, (v) => (maxMinutes = v), { min: "10", max: "240" }),
        h("label", {}, "Spend cap ($)"),
        num(maxUsd, (v) => (maxUsd = v), { step: "0.25", min: "0.25", max: "5" }),
      ),
    );
    const p = audit!.plan!;
    box.append(
      h(
        "p",
        { class: "muted small" },
        `${p.total_steps} training steps, ${p.checkpoint_steps.length} saved checkpoints, ${p.eval_images} test images. The run stops at whichever limit comes first.`,
      ),
    );
    if (quote) box.append(renderQuote(quote));
    else {
      box.append(
        h(
          "div",
          { class: "actions" },
          h("button", { class: "primary", onclick: () => void getQuote(), disabled: busy || !!audit?.settings_error }, busy ? "Checking…" : "Get a quote"),
          h("span", { class: "muted small" }, "Nothing is rented until you approve the quote."),
        ),
      );
    }
    return box;
  }

  function renderQuote(q: QuoteView): Node {
    const c = h("div", { class: "card quote" });
    c.append(
      h("h2", {}, "Approve this run"),
      h(
        "p",
        {},
        "GPU: ",
        h("b", {}, q.gpu_name),
        q.gpu_location ? ` (${q.gpu_location})` : "",
        ` · ${money(q.hourly)}/hr`,
      ),
      h(
        "p",
        {},
        `Expected: about ${q.expected_minutes} min, roughly ${money((q.hourly * q.expected_minutes) / 60)}.`,
        h("br", {}),
        `Hard limit: ${q.max_minutes} min or ${money(q.max_usd)}; the most it can cost is ${money(q.worst_usd)}.`,
      ),
      h(
        "p",
        { class: "muted small" },
        "Pressing Start rents this GPU now. It is destroyed when the run ends or hits the limit, even if you close the app. The quote expires in 10 minutes.",
      ),
      h(
        "div",
        { class: "actions" },
        h("button", { class: "primary", onclick: () => void start(), disabled: busy }, `Start (up to ${money(q.worst_usd)})`),
        h("button", { class: "ghost", onclick: () => ((quote = null), render()) }, "Cancel"),
      ),
    );
    return c;
  }

  function bar(fraction: number): Node {
    const fill = h("div", { id: "train-fill" });
    fill.style.cssText = `height:100%;width:${Math.round(Math.max(0, Math.min(1, fraction)) * 100)}%;background:var(--accent);transition:width .4s`;
    return h("div", { class: "progress" }, fill);
  }

  function renderRun(s: Status): Node {
    const card = h("section", { class: "card" }, h("h2", {}, s.state === "running" ? "Training" : "Run finished"));
    card.append(h("p", { class: "run-message" }, s.message));
    if (s.setup_detail || s.setup_log) {
      const quiet = s.setup_quiet_secs ?? 0;
      card.append(
        h(
          "p",
          { class: quiet >= 180 ? "error small" : "muted small" },
          [s.setup_detail, quiet >= 60 && `no change for ${Math.floor(quiet / 60)} min`].filter(Boolean).join(" · "),
        ),
      );
      if (s.setup_log) {
        const d = h("details", {}, h("summary", {}, "Latest setup output"));
        const pre = h("pre", { class: "small" }, s.setup_log);
        pre.style.cssText = "white-space:pre-wrap;overflow-wrap:anywhere;max-height:12em;overflow:auto";
        d.append(pre);
        card.append(d);
      }
    }
    if (s.step !== null && s.total_steps) {
      const training = s.eval_done === null;
      card.append(h("p", { class: "small" }, training ? `Step ${s.step} of ${s.total_steps}` : `Training done (${s.total_steps} steps)`));
      card.append(bar(s.step / s.total_steps));
    }
    if (s.eval_done !== null && s.eval_total) {
      card.append(h("p", { class: "small" }, `Test image ${s.eval_done} of ${s.eval_total}`), bar(s.eval_done / s.eval_total));
    }
    const facts = [
      s.gpu_name && `${s.gpu_name} · ${money(s.hourly ?? 0)}/hr`,
      s.elapsed_secs !== null && s.max_minutes !== null && `${minutes(s.elapsed_secs)} of ${s.max_minutes} min`,
      s.spent_usd !== null && s.max_usd !== null && `about ${money(s.spent_usd)} of ${money(s.max_usd)}`,
    ].filter(Boolean);
    card.append(h("p", { class: "muted small" }, facts.join(" · ")));
    if (s.error) card.append(h("p", { class: "error" }, s.error));
    if (s.state === "finished" && !s.destroyed) {
      card.append(h("p", { class: "error" }, `The GPU (instance ${s.instance_id ?? "?"}) may still be running. Open your Vast console and destroy it.`));
    }
    const actions = h("div", { class: "actions" });
    if (s.state === "running") {
      actions.append(
        h("button", { class: "danger", onclick: () => void act(invoke("train_cancel").then(poll)) }, "Stop and download what exists"),
      );
    } else {
      if (s.results_dir) actions.append(h("button", { class: "primary", onclick: () => void act(invoke("train_open_results")) }, "Open results folder"));
      actions.append(
        h(
          "button",
          { class: "ghost", onclick: () => void act(invoke("train_dismiss").then(poll)) },
          "Train another",
        ),
      );
    }
    card.append(actions);
    return card;
  }

  function render(): void {
    root.replaceChildren();
    if (!settings) {
      root.append(h("section", { class: "card" }, h("p", { class: "muted" }, "Loading…")));
      return;
    }
    if (status && status.state !== "idle") root.append(renderRun(status));
    else root.append(renderSetup());
  }

  function show(): void {
    root.hidden = false;
    void load();
    window.clearInterval(timer);
    timer = window.setInterval(() => {
      if (status?.state === "running" || status?.state === "finished") void poll();
    }, 2000);
  }

  function hide(): void {
    root.hidden = true;
    window.clearInterval(timer);
  }

  return { show, hide };
}
