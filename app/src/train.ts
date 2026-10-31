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
  eval_preset: "none" | "quick" | "thorough";
}

interface MossSettings {
  output_name: string;
  rank: number;
  alpha: number;
  steps: number;
  save_every: number;
  caption_mode: "nl" | "mix";
  p_nl: number;
  tag_dropout: number;
  int8_compute: boolean;
  seed: number;
  sample_prompt: string;
}

interface MossAudit {
  images: number;
  error: string | null;
  settings_error: string | null;
  trigger: string;
  plan: Plan | null;
}

/** `local` trains an SDXL LoRA on this PC's own GPU: nothing is rented. */
type TrainerKind = "sdxl" | "kromoss" | "local";

interface Checkpoint {
  file: string;
  step: number;
}

interface InstallStatus {
  state: "idle" | "running" | "finished";
  stage: string;
  detail: string;
  step: number;
  steps: number;
  fraction: number | null;
  elapsed_secs: number;
  outcome: "completed" | "failed" | "cancelled" | null;
  error: string | null;
}

interface LocalSetup {
  base_model: string;
  /** Where the trainer comes from: SlopTweak's own setup, an earlier manual folder, or not set up. */
  trainer: {
    source: "managed" | "existing" | "none";
    /** The folder SlopTweak sets the trainer up in. */
    root: string;
    download_gb: number;
    disk_gb: number;
    free_gb: number | null;
    drive: string;
    /** Set when there isn't room to set up there. */
    space_problem: string | null;
  };
  problems: string[];
  gpu: { name: string; total_mib: number; used_mib: number } | null;
  gpu_problem: string | null;
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
  trainer: TrainerKind;
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
  stages: StageView[];
  host_status: string[];
  container_log: string | null;
  container_log_note: string | null;
  host_note: string | null;
  diagnostics_path: string | null;
  /** Trained on this PC: no price, limits or rented GPU. */
  local: boolean;
}

interface StageView {
  name: string;
  started_secs: number;
  secs: number;
  current: boolean;
}

const money = (n: number) => `$${n.toFixed(2)}`;

function minutes(secs: number): string {
  const m = Math.floor(secs / 60);
  return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min ${secs % 60} s`;
}

/** `+3m07s`, the same way the saved report writes offsets. */
const offset = (secs: number) => `+${Math.floor(secs / 60)}m${String(secs % 60).padStart(2, "0")}s`;

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
  let moss: MossSettings | null = null;
  let mossAudit: MossAudit | null = null;
  let trainer: TrainerKind = "local";
  let localSetup: LocalSetup | null = null;
  let install: InstallStatus | null = null;
  // After a local run: keep training it, or try a prompt on its checkpoints.
  let checkpoints: Checkpoint[] = [];
  /** The run `checkpoints` belongs to. */
  let followFor: string | null = null;
  let keepSteps = 200;
  const sampleForm = { prompt: "", checkpoint: "", strength: 1, seed: Math.floor(Math.random() * 1_000_000), shape: "portrait" };
  let samples: { url: string; path: string; caption: string }[] = [];
  let sampling = false;
  let sampleError = "";
  let dir = "";
  let audit: AuditView | null = null;
  let quote: QuoteView | null = null;
  let status: Status | null = null;
  let busy = false;
  let error = "";
  let baseLink = DEFAULT_BASE;
  let maxMinutes = 120;
  /** Kromoss only: "" = the cheapest GPU that fits, or a model to insist on. */
  let gpuModel = "";
  let maxUsd = 1.5;
  let timer: number | undefined;
  // The machine log is rebuilt on every poll: remember how the user left it.
  let logOpen = true;
  // Changing a setting re-renders the page: keep the settings panel open.
  let settingsOpen = false;
  let logScroll = 0;
  let logStick = true;

  const trigger = () => (trainer === "kromoss" ? mossAudit?.trigger : audit?.report.images[0]?.first_tag) ?? "";
  const activePlan = () => (trainer === "kromoss" ? mossAudit?.plan : audit?.plan) ?? null;
  const activeSettingsError = () => (trainer === "kromoss" ? mossAudit?.settings_error : audit?.settings_error) ?? null;

  async function load(): Promise<void> {
    const d = await invoke<{ settings: TrainSettings }>("train_defaults");
    settings = d.settings;
    moss = await invoke<MossSettings>("train_moss_defaults");
    status = await invoke<Status>("train_status");
    render();
    // Free GPU memory changes between visits: look again every time.
    if (trainer === "local") {
      void loadLocalSetup();
      void pollInstall();
    }
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
    if (!settings || !moss || !dir) return;
    if (trainer === "kromoss") {
      await act(
        invoke<MossAudit>("train_moss_audit", { dir, settings: moss }).then((a) => {
          mossAudit = a;
          if (moss && a.trigger && moss.output_name === "lora") {
            moss.output_name = a.trigger.replace(/[^A-Za-z0-9_-]/g, "_").slice(0, 48) || "lora";
          }
        }),
      );
      return;
    }
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
    if (!settings || !moss) return;
    await act(
      invoke<QuoteView>("train_quote", {
        args: {
          dir,
          settings,
          trainer,
          moss: trainer === "kromoss" ? moss : null,
          trigger: trigger(),
          base_link: baseLink.trim(),
          max_minutes: maxMinutes,
          max_usd: maxUsd,
          gpu_model: trainer === "kromoss" ? gpuModel : "",
        },
      }).then((q) => {
        quote = q;
      }),
    );
  }

  async function loadLocalSetup(): Promise<void> {
    try {
      localSetup = await invoke<LocalSetup>("train_local_setup");
    } catch (e) {
      error = String(e);
    }
    render();
  }

  /** The user typed or pasted a checkpoint path. */
  async function saveModelPath(baseModel: string): Promise<void> {
    await act(
      invoke<LocalSetup>("train_local_set_model", { baseModel }).then((s) => {
        localSetup = s;
      }),
    );
  }

  /** The system file dialog; nothing changes if it is closed without a choice. */
  async function pickModel(): Promise<void> {
    await act(
      invoke<LocalSetup | null>("train_local_pick_model").then((s) => {
        if (s) localSetup = s;
      }),
    );
  }

  /** Choose which folder the trainer is set up in; nothing changes if the dialog is closed. */
  async function pickTrainerDir(): Promise<void> {
    await act(
      invoke<LocalSetup | null>("train_local_pick_trainer_dir").then((s) => {
        if (s) localSetup = s;
      }),
    );
  }

  async function startInstall(): Promise<void> {
    await act(invoke("train_local_install").then(pollInstall));
  }

  async function pollInstall(): Promise<void> {
    try {
      const next = await invoke<InstallStatus>("train_local_install_status");
      const changed = JSON.stringify(next) !== JSON.stringify(install);
      install = next;
      if (next.state === "finished" && next.outcome !== "failed") {
        // Done or cancelled: nothing to show, and the setup now counts as ready.
        await invoke("train_local_install_dismiss");
        install = null;
        localSetup = await invoke<LocalSetup>("train_local_setup");
      } else if (!changed) {
        return;
      }
    } catch (e) {
      error = String(e);
    }
    render();
  }

  /** Local runs are free, so Start is the only step: there is no quote. */
  async function startLocal(): Promise<void> {
    if (!settings || !dir) return;
    await act(invoke("train_local_start", { dir, settings }).then(poll));
  }

  async function start(): Promise<void> {
    if (!quote) return;
    const id = quote.plan_id;
    quote = null;
    await act(invoke("train_start", { planId: id }).then(poll));
  }

  async function poll(): Promise<void> {
    try {
      const next = await invoke<Status>("train_status");
      const changed = JSON.stringify(next) !== JSON.stringify(status);
      status = next;
      // A finished local run: load its checkpoints once, for Sample and Keep training.
      if (next.local && next.state === "finished" && followFor !== next.results_dir) {
        followFor = next.results_dir;
        checkpoints = await invoke<Checkpoint[]>("train_local_checkpoints");
        sampleForm.checkpoint = checkpoints.length > 0 ? checkpoints[checkpoints.length - 1]!.file : "";
        samples = [];
        sampleError = "";
      } else if (!changed) {
        // Nothing new: rebuilding the page would drop what the user is typing.
        return;
      }
    } catch (e) {
      error = String(e);
    }
    render();
  }

  /** Train more steps from the newest checkpoint, as a new run. */
  async function keepTraining(): Promise<void> {
    followFor = null;
    await act(invoke("train_local_continue", { steps: keepSteps }).then(poll));
  }

  async function runSample(): Promise<void> {
    sampling = true;
    sampleError = "";
    render();
    try {
      const f = sampleForm;
      const v = await invoke<{ path: string; data_url: string }>("train_local_sample", {
        request: { prompt: f.prompt, checkpoint: f.checkpoint || null, strength: f.strength, seed: f.seed, shape: f.shape },
      });
      const step = checkpoints.find((c) => c.file === f.checkpoint)?.step;
      const what = f.checkpoint ? `step ${step ?? "?"} at ${f.strength}` : "base model, no LoRA";
      samples = [{ url: v.data_url, path: v.path, caption: `${what} · seed ${f.seed}` }, ...samples].slice(0, 6);
    } catch (e) {
      sampleError = String(e);
    }
    sampling = false;
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
        "Teach a model a character or style from a folder of images, each with caption files of the same name. SlopTweak rents a GPU, trains, downloads the result, and destroys the GPU; SDXL models can also train on this PC's own GPU.",
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
    card.append(renderTrainerPicker());
    if (trainer === "local") {
      card.append(renderLocalSetup());
      if (audit) card.append(...renderAudit(audit));
      if (audit?.plan) {
        card.append(renderSettings(s));
        card.append(renderLocalStart());
      }
    } else if (trainer === "kromoss") {
      if (mossAudit) card.append(...renderMossAudit(mossAudit));
      if (mossAudit?.plan) {
        card.append(renderMossSettings(moss!));
        card.append(renderLimits());
      }
    } else {
      if (audit) card.append(...renderAudit(audit));
      if (audit?.plan) {
        card.append(renderSettings(s));
        card.append(renderLimits());
      }
    }
    if (error) card.append(h("p", { class: "error" }, error));
    return card;
  }

  function renderTrainerPicker(): Node {
    const pick = h("select", {});
    for (const [value, label] of [
      ["sdxl", "Illustrious / SDXL checkpoint (your CivitAI link)"],
      ["kromoss", "Kromoss (MOSS-Image, natural language and tags)"],
      ["local", "SDXL on this PC (your own GPU, free)"],
    ]) {
      const o = h("option", { value }, label);
      if (trainer === value) o.selected = true;
      pick.append(o);
    }
    pick.addEventListener("change", () => {
      trainer = pick.value === "kromoss" ? "kromoss" : pick.value === "local" ? "local" : "sdxl";
      quote = null;
      error = "";
      if (trainer === "local") {
        void loadLocalSetup();
        void pollInstall();
      }
      if (dir) void runAudit();
      else render();
    });
    return h(
      "div",
      { class: "row" },
      h("label", {}, "Trains"),
      pick,
      trainer === "local" &&
        h(
          "span",
          { class: "muted small hint" },
          "Trains on this PC's own GPU with the kohya trainer, which SlopTweak sets up for you once. You choose an SDXL checkpoint file. Same captions and settings as the rented-GPU run, no cost. Test images aren't made here; use Sample after a run.",
        ),
      trainer === "kromoss" &&
        h(
          "span",
          { class: "muted small hint" },
          "Each image needs name.nl.txt (a sentence caption) and, for mixed captions, name.e621.txt (tags, trigger word first). About 15 GB of model files are downloaded to the GPU. Training uses KohakuMossTrainer by KohakuBlueLeaf (KohakuMoss License 1.0), which the GPU downloads; it is not part of SlopTweak.",
        ),
    );
  }

  function renderMossAudit(a: MossAudit): Node[] {
    const out: Node[] = [];
    if (a.error) out.push(h("p", { class: "error small" }, `Problem: ${a.error}`));
    else {
      out.push(
        h(
          "p",
          {},
          `${a.images} usable image${a.images === 1 ? "" : "s"}`,
          a.trigger ? " · trigger word: " : "",
          a.trigger ? h("b", {}, a.trigger) : "",
        ),
      );
    }
    if (a.settings_error) out.push(h("p", { class: "error small" }, a.settings_error));
    return out;
  }

  function settingsPanel(): HTMLDetailsElement {
    const d = h("details", { open: settingsOpen }, h("summary", {}, "Training settings"));
    d.addEventListener("toggle", () => (settingsOpen = d.open));
    return d;
  }

  function renderMossSettings(m: MossSettings): Node {
    const d = settingsPanel();
    const form = h("div", { class: "form" });
    const name = h("input", { type: "text", maxlength: "48" });
    name.value = m.output_name;
    name.addEventListener("change", () => {
      m.output_name = name.value.trim();
      quote = null;
    });
    const mode = h("select", {});
    for (const [value, label] of [
      ["mix", "Sentences and tags, mixed"],
      ["nl", "Sentences only"],
    ]) {
      const o = h("option", { value }, label);
      if (m.caption_mode === value) o.selected = true;
      mode.append(o);
    }
    mode.addEventListener("change", () => {
      m.caption_mode = mode.value === "nl" ? "nl" : "mix";
      quote = null;
      if (dir) void runAudit();
    });
    const int8 = h("input", { type: "checkbox" });
    int8.checked = m.int8_compute;
    int8.addEventListener("change", () => {
      m.int8_compute = int8.checked;
      quote = null;
    });
    form.append(
      ...field("LoRA name", name),
      ...field("Captions", mode, "Mixed showed steadier outfits and rear views in the first tests; sentences-only needs no tag files"),
      ...field("Steps", num(m.steps, (v) => ((m.steps = v), (m.save_every = Math.min(m.save_every, v))), { min: "10", max: "4000" }), "800 worked for about 20 images"),
      ...field("Checkpoint every", num(m.save_every, (v) => (m.save_every = v), { min: "1", max: "4000" }), "steps; compare them to pick the best"),
      ...field("Rank", num(m.rank, (v) => ((m.rank = v), (m.alpha = Math.min(m.alpha, v))), { min: "1", max: "128" }), "capacity; 32 was used for one character"),
      ...field("Faster math (int8)", int8, "about 1.3x faster on a 4090; slightly changes the numbers"),
    );
    d.append(form);
    return d;
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
    const d = settingsPanel();
    const form = h("div", { class: "form" });
    const name = h("input", { type: "text", maxlength: "48" });
    name.value = s.output_name;
    name.addEventListener("change", () => {
      s.output_name = name.value.trim();
      quote = null;
    });
    const preset = h("select", {});
    for (const [value, label] of [
      ["none", "None (sample after the run)"],
      ["quick", "Quick (about 12 images)"],
      ["thorough", "Thorough (every checkpoint and strength)"],
    ]) {
      const o = h("option", { value }, label);
      if (s.eval_preset === value) o.selected = true;
      preset.append(o);
    }
    preset.addEventListener("change", () => {
      s.eval_preset = preset.value === "thorough" ? "thorough" : preset.value === "none" ? "none" : "quick";
      quote = null;
      if (dir) void runAudit();
    });
    form.append(
      ...field("LoRA name", name),
      ...(trainer === "local"
        ? []
        : field("Test images", preset, "Thorough lets you compare checkpoints, and takes about 10 minutes longer")),
      ...field("Epochs", num(s.epochs, (v) => (s.epochs = v), { min: "1", max: "400" }), "passes over your images"),
      ...field("Rank", num(s.rank, (v) => ((s.rank = v), (s.alpha = Math.min(s.alpha, v))), { min: "1", max: "128" }), "capacity; 16 suits one character"),
      ...field("Learning rate", num(s.learning_rate, (v) => (s.learning_rate = v), { step: "0.00001", min: "0.000001", max: "0.001" })),
      ...field("Saved checkpoints", num(s.save_points, (v) => (s.save_points = v), { min: "1", max: "10" }), "compare them to pick the best"),
    );
    d.append(form);
    return d;
  }

  function renderLocalSetup(): Node {
    const box = h("div", { class: "limits" });
    const s = localSetup;
    if (!s) {
      box.append(h("p", { class: "muted small" }, "Checking this PC…"));
      return box;
    }

    // The trainer: set up by SlopTweak, so there is nothing to find or configure.
    const t = s.trainer;
    if (install && install.state !== "idle") {
      const i = install;
      const overall = ((i.step - 1) + (i.fraction ?? 0)) / i.steps;
      if (i.state === "running") {
        box.append(
          h("div", { class: "row" }, h("label", {}, "Trainer"), h("span", {}, `Setting up: step ${i.step} of ${i.steps} · ${i.stage}`)),
          bar(overall),
          h("p", { class: "muted small", style: "overflow-wrap:anywhere" }, `${i.detail} · ${minutes(i.elapsed_secs)}`),
          h(
            "div",
            { class: "actions" },
            h("button", { class: "ghost", onclick: () => void act(invoke("train_local_install_cancel").then(pollInstall)) }, "Cancel"),
            h("span", { class: "muted small" }, "You can leave this tab; setup keeps going."),
          ),
        );
      } else {
        box.append(
          h("div", { class: "row" }, h("label", {}, "Trainer"), h("span", { class: "error" }, "Setup didn't finish")),
          h("p", { class: "error small", style: "white-space:pre-wrap;overflow-wrap:anywhere" }, i.error ?? "Something went wrong."),
          h(
            "div",
            { class: "actions" },
            h(
              "button",
              { class: "primary", onclick: () => void act(invoke("train_local_install_dismiss").then(startInstall)) },
              "Try again",
            ),
            h("span", { class: "muted small" }, "It picks up where it stopped."),
          ),
        );
      }
    } else if (t.source === "none") {
      box.append(
        h("div", { class: "row" }, h("label", {}, "Trainer"), h("span", { class: "muted" }, "Not set up yet")),
        h(
          "p",
          { class: "muted small" },
          `SlopTweak sets the trainer up once, by itself: about ${t.download_gb.toFixed(0)} GB to download (PyTorch and the kohya trainer) and about ${t.disk_gb.toFixed(0)} GB of free disk while it installs (5 GB afterwards). It needs an NVIDIA GPU.`,
        ),
        h(
          "div",
          { class: "row" },
          h("label", {}, "Install in"),
          h("span", { class: "path", title: t.root }, t.root),
          h("button", { onclick: () => void pickTrainerDir(), disabled: busy }, "Change…"),
        ),
        t.space_problem
          ? h("p", { class: "error small" }, t.space_problem)
          : h("p", { class: "muted small" }, t.free_gb !== null ? `${t.free_gb.toFixed(0)} GB free on ${t.drive}` : ""),
        h(
          "div",
          { class: "actions" },
          h("button", { class: "primary", onclick: () => void startInstall(), disabled: busy || !!t.space_problem }, "Set up trainer"),
        ),
      );
    } else {
      box.append(
        h(
          "div",
          { class: "row" },
          h("label", {}, "Trainer"),
          h("span", { class: "muted" }, t.source === "managed" ? "Ready" : "Ready (the one you set up earlier)"),
        ),
      );
    }

    // The base checkpoint: pick it with the system file dialog, or paste a path.
    const model = h("input", { type: "text", placeholder: "Path to an SDXL .safetensors checkpoint" });
    model.value = s.base_model;
    model.addEventListener("change", () => void saveModelPath(model.value.trim()));
    box.append(
      h(
        "div",
        { class: "row" },
        h("label", {}, "Base model file"),
        model,
        h("button", { onclick: () => void pickModel(), disabled: busy }, "Choose file…"),
      ),
    );
    for (const p of s.problems) {
      // The trainer has its own row and button above.
      if (!p.includes("trainer")) box.append(h("p", { class: "error small" }, p));
    }
    if (s.gpu) {
      const free = ((s.gpu.total_mib - s.gpu.used_mib) / 1024).toFixed(1);
      box.append(
        h(
          "p",
          { class: s.gpu_problem ? "error small" : "muted small" },
          s.gpu_problem ?? `${s.gpu.name} · ${free} GB free`,
        ),
      );
    } else {
      box.append(h("p", { class: "muted small" }, "No NVIDIA GPU was found (nvidia-smi isn't available)."));
    }
    return box;
  }

  function renderLocalStart(): Node {
    const box = h("div", { class: "limits" });
    const p = activePlan()!;
    const blocked = busy || !!activeSettingsError() || !localSetup || localSetup.problems.length > 0 || !!localSetup.gpu_problem;
    box.append(
      h(
        "p",
        { class: "muted small" },
        `${p.total_steps} training steps, ${p.checkpoint_steps.length} saved checkpoints. About a second per step on a recent laptop GPU, plus a minute or two to start; the first run also downloads the text tokenizers.`,
      ),
      h(
        "div",
        { class: "actions" },
        h("button", { class: "primary", onclick: () => void startLocal(), disabled: blocked }, "Start training on this PC"),
        h("span", { class: "muted small" }, "Free: nothing is rented. Your GPU is busy until it finishes."),
      ),
    );
    return box;
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
      ...(trainer === "sdxl" ? [h("div", { class: "row" }, h("label", {}, "Base model"), link)] : []),
      h(
        "div",
        { class: "row" },
        h("label", {}, "Time limit (min)"),
        num(maxMinutes, (v) => (maxMinutes = v), { min: "10", max: "240" }),
        h("label", {}, "Spend cap ($)"),
        num(maxUsd, (v) => (maxUsd = v), { step: "0.25", min: "0.25", max: "5" }),
      ),
    );
    if (trainer === "kromoss") {
      const gpu = h("select", {});
      for (const [value, label] of [
        ["", "Cheapest that fits"],
        ["4090", "RTX 4090 only (faster, costs more per hour)"],
      ]) {
        const o = h("option", { value }, label);
        if (gpuModel === value) o.selected = true;
        gpu.append(o);
      }
      gpu.addEventListener("change", () => {
        gpuModel = gpu.value;
        quote = null;
      });
      box.append(h("div", { class: "row" }, h("label", {}, "GPU"), gpu));
    }
    const p = activePlan()!;
    box.append(
      h(
        "p",
        { class: "muted small" },
        `${p.total_steps} training steps, ${p.checkpoint_steps.length} saved checkpoints${trainer === "kromoss" ? "" : `, ${p.eval_images} test images`}. The run stops at whichever limit comes first.`,
      ),
    );
    if (quote) box.append(renderQuote(quote));
    else {
      box.append(
        h(
          "div",
          { class: "actions" },
          h("button", { class: "primary", onclick: () => void getQuote(), disabled: busy || !!activeSettingsError() }, busy ? "Checking…" : "Get a quote"),
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

  /** Where the time goes, what was done about the machine, and the live machine log. */
  function renderActivity(s: Status): Node[] {
    const out: Node[] = [];
    if (s.host_note) {
      out.push(h("p", { class: s.host_note.includes("marked bad") ? "error small" : "muted small" }, s.host_note));
    }
    if (s.stages.length > 0) {
      out.push(
        h("p", { class: "small" }, s.state === "running" ? "Where the time is going" : "Where the time went"),
        h(
          "ul",
          { class: "small stages", style: "margin:0 0 .5em;padding-left:1.2em;list-style:none;font-variant-numeric:tabular-nums" },
          ...s.stages.map((st) =>
            h(
              "li",
              { class: st.current && s.state === "running" ? "" : "muted" },
              `${offset(st.started_secs)}  ${st.name} · ${minutes(st.secs)}${st.current && s.state === "running" ? " (now)" : ""}`,
            ),
          ),
        ),
      );
    }
    const parts: string[] = [];
    if (s.host_status.length > 0) parts.push(["— What Vast said —", ...s.host_status].join("\n"));
    if (s.container_log) parts.push(["— Machine log —", s.container_log].join("\n"));
    if (parts.length > 0 || s.container_log_note) {
      const pre = h("pre", { class: "small" }, parts.join("\n\n") || "Nothing yet.");
      pre.style.cssText = "white-space:pre-wrap;overflow-wrap:anywhere;max-height:16em;overflow:auto";
      pre.addEventListener("scroll", () => {
        logScroll = pre.scrollTop;
        logStick = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 24;
      });
      requestAnimationFrame(() => {
        pre.scrollTop = logStick ? pre.scrollHeight : logScroll;
      });
      const d = h("details", { open: logOpen }, h("summary", {}, s.state === "running" ? "Live machine log" : "Machine log"));
      d.addEventListener("toggle", () => {
        logOpen = d.open;
      });
      d.append(pre);
      if (s.container_log_note) d.append(h("p", { class: "muted small" }, s.container_log_note));
      out.push(d);
    }
    if (s.diagnostics_path) {
      out.push(h("p", { class: "muted small" }, `Details for troubleshooting were saved to ${s.diagnostics_path}`));
    }
    return out;
  }

  function renderRun(s: Status): Node {
    const card = h("section", { class: "card" }, h("h2", {}, s.state === "running" ? "Training" : "Run finished"));
    card.append(h("p", { class: "run-message" }, s.message));
    card.append(...renderActivity(s));
    if (s.setup_detail || s.setup_log || s.setup_quiet_secs !== null) {
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
    const facts = s.local
      ? [s.gpu_name, s.elapsed_secs !== null && minutes(s.elapsed_secs), "no cost"].filter(Boolean)
      : [
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
        h(
          "button",
          { class: "danger", onclick: () => void act(invoke("train_cancel").then(poll)) },
          s.local ? "Stop (keeps saved checkpoints)" : "Stop and download what exists",
        ),
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
    if (s.local && s.state === "finished") card.append(...renderFollowUp());
    return card;
  }

  /** After a local run: keep training it, or try a prompt on its checkpoints. */
  function renderFollowUp(): Node[] {
    const out: Node[] = [];
    const last = checkpoints[checkpoints.length - 1];

    const steps = h("input", { type: "number", min: "1", max: "20000", step: "50" });
    steps.value = String(keepSteps);
    steps.addEventListener("input", () => {
      if (Number.isFinite(steps.valueAsNumber)) keepSteps = steps.valueAsNumber;
    });
    out.push(
      h("hr", {}),
      h("p", {}, h("b", {}, "Keep training")),
      h(
        "div",
        { class: "row" },
        h("label", {}, "Extra steps"),
        steps,
        h("button", { class: "primary", onclick: () => void keepTraining(), disabled: busy || sampling || !last }, "Keep training"),
      ),
      h(
        "p",
        { class: "muted small" },
        last
          ? `Continues from step ${last.step} of this run for that many more steps, on the same images and settings. It is saved as a new run beside this one.`
          : "This run saved no checkpoint to continue from.",
      ),
    );

    if (!sampleForm.prompt) sampleForm.prompt = `${trigger() || "your_trigger_word"}, solo, portrait`;
    const text = h("input", { type: "text", maxlength: "600", placeholder: "Describe the picture" });
    text.value = sampleForm.prompt;
    text.addEventListener("input", () => (sampleForm.prompt = text.value));

    const which = h("select", {});
    for (const c of [...checkpoints].reverse()) {
      const o = h("option", { value: c.file }, `Step ${c.step}${c === last ? " (newest)" : ""}`);
      if (sampleForm.checkpoint === c.file) o.selected = true;
      which.append(o);
    }
    const none = h("option", { value: "" }, "None (the base model alone)");
    if (!sampleForm.checkpoint) none.selected = true;
    which.append(none);
    which.addEventListener("change", () => (sampleForm.checkpoint = which.value));

    const strength = h("input", { type: "number", min: "0", max: "2", step: "0.05" });
    strength.value = String(sampleForm.strength);
    strength.addEventListener("input", () => {
      if (Number.isFinite(strength.valueAsNumber)) sampleForm.strength = strength.valueAsNumber;
    });
    const shape = h("select", {});
    for (const [value, label] of [
      ["portrait", "Portrait"],
      ["square", "Square"],
      ["landscape", "Landscape"],
    ]) {
      const o = h("option", { value }, label);
      if (sampleForm.shape === value) o.selected = true;
      shape.append(o);
    }
    shape.addEventListener("change", () => (sampleForm.shape = shape.value));
    const seed = h("input", { type: "number", min: "0", max: "4294967295", step: "1" });
    seed.value = String(sampleForm.seed);
    seed.addEventListener("input", () => {
      if (Number.isInteger(seed.valueAsNumber)) sampleForm.seed = seed.valueAsNumber;
    });

    out.push(
      h("hr", {}),
      h("p", {}, h("b", {}, "Sample")),
      h("div", { class: "row" }, h("label", {}, "Prompt"), text),
      h("div", { class: "row" }, h("label", {}, "Checkpoint"), which),
      h("div", { class: "row" }, h("label", {}, "Strength"), strength, h("label", {}, "Shape"), shape),
      h(
        "div",
        { class: "row" },
        h("label", {}, "Seed"),
        seed,
        h(
          "button",
          { class: "ghost", onclick: () => ((sampleForm.seed = Math.floor(Math.random() * 1_000_000)), render()), disabled: sampling },
          "Random",
        ),
      ),
      h(
        "div",
        { class: "actions" },
        h("button", { class: "primary", onclick: () => void runSample(), disabled: sampling || busy }, sampling ? "Making the image…" : "Sample"),
        sampling && h("button", { class: "ghost", onclick: () => void invoke("train_local_sample_cancel") }, "Cancel"),
        h("span", { class: "muted small" }, sampling ? "Loading the model first: about half a minute." : "Free. Images are saved in this run's samples folder."),
      ),
    );
    if (sampleError) out.push(h("p", { class: "error small", style: "white-space:pre-wrap" }, sampleError));
    if (samples.length > 0) {
      out.push(
        h(
          "div",
          { style: "display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:10px;margin-top:10px" },
          ...samples.map((s) =>
            h(
              "figure",
              { style: "margin:0" },
              h("img", { src: s.url, alt: s.caption, title: s.path, style: "width:100%;border-radius:6px;display:block" }),
              h("figcaption", { class: "muted small" }, s.caption),
            ),
          ),
        ),
      );
    }
    return out;
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
      if (install?.state === "running") void pollInstall();
    }, 2000);
  }

  function hide(): void {
    root.hidden = true;
    window.clearInterval(timer);
  }

  return { show, hide };
}
