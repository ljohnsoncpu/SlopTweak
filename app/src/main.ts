import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ----- types mirrored from src-tauri/src (session.rs, lib.rs, cost.rs) --------

interface OfferSummary {
  offer_id: number;
  gpu_name: string;
  hourly: number;
  download_cost: number;
  location: string | null;
}

interface Deadlines {
  heartbeat_s: number | null;
  idle_s: number | null;
  max_session_s: number | null;
}

type SessionState =
  | { kind: "idle"; notice: string | null }
  | { kind: "renting"; attempt: number; max_attempts: number; offer: OfferSummary | null }
  | {
      kind: "provisioning";
      attempt: number;
      max_attempts: number;
      instance_id: number;
      offer: OfferSummary;
      stage: string | null;
      detail: string | null;
      progress: number | null;
    }
  | {
      kind: "ready";
      instance_id: number;
      offer: OfferSummary;
      ready_unix: number;
      deadlines: Deadlines | null;
    }
  | { kind: "stopping"; instance_id: number | null }
  | { kind: "failed"; reason: string };

interface ModelView {
  id: string;
  name: string;
  description: string;
  base: string;
  size_gb: number;
  min_vram_gb: number;
  needs_civitai: boolean;
  nsfw: boolean;
  license_note: string;
  min_compute_cap: number | null;
}

interface LoraView {
  model_id: number;
  version_id: number;
  name: string;
  version_name: string;
  base_model: string;
  nsfw: boolean;
  trained_words: string[];
  enabled: boolean;
  family: string | null;
  size_mb: number;
}

interface SettingsView {
  model_id: string | null;
  max_dph: number;
  idle_minutes: number;
  max_session_minutes: number;
  min_credit: number;
  output_dir: string | null;
  output_dir_resolved: string;
  loras: LoraView[];
}

interface CostBar {
  hourly: number;
  elapsed_s: number;
  spent: number;
  download_cost: number;
  credit: number | null;
  runway_s: number | null;
  low: boolean;
  shutdown: [string, number] | null;
}

interface SyncReport {
  folder: string;
  phase: "running" | "finishing" | "done" | "incomplete";
  saved: number;
  canvas_saved: number;
  missing: number;
  last_error: string | null;
  last_sync_unix: number | null;
}

interface Snapshot {
  mode: "vast" | "mock";
  state: SessionState;
  log: string[];
  has_vast_key: boolean;
  has_civitai_key: boolean;
  models: ModelView[];
  catalog: { source: "online" | "cached" | "bundled"; fetched_unix: number | null; skipped: string[] };
  settings: SettingsView;
  credit: number | null;
  cost: CostBar | null;
  sync: SyncReport | null;
}

type Gate = { kind: "ok" } | { kind: "warn"; message: string } | { kind: "refuse"; message: string };

interface Estimate {
  gpu_name: string | null;
  hourly: number | null;
  download_cost: number;
  download_gb: number;
  usable_offers: number;
  credit: number;
  gate: Gate;
}

interface KeyCheck {
  credit: number | null;
  username: string | null;
}

interface Orphan {
  instance_id: number;
  gpu_name: string | null;
  hourly: number | null;
  status: string | null;
  can_reattach: boolean;
}

// ----- dom helpers ---------------------------------------------------------------

function el<T extends HTMLElement = HTMLElement>(id: string): T {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing #${id}`);
  return e as T;
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

/** A button that opens one of the fixed pages from lib.rs `fixed_link`. */
function linkButton(id: string, label: string, cls = ""): HTMLButtonElement {
  return h("button", { class: `link ${cls}`, type: "button", onclick: () => void openLink(id) }, `${label} ↗`);
}

async function openLink(id: string): Promise<void> {
  try {
    await invoke("open_link", { id });
  } catch (e) {
    console.error(e);
  }
}

// Screenshots for the wizard, if present (app/src/wizard/*.png).
const SHOTS = import.meta.glob("./wizard/*.png", { eager: true, query: "?url", import: "default" }) as Record<
  string,
  string
>;
function shot(name: string, alt: string): HTMLElement | null {
  const src = SHOTS[`./wizard/${name}.png`];
  return src ? h("img", { class: "shot", src, alt }) : null;
}

const ui = {
  mode: el("mode"),
  costbar: el("costbar"),
  navSettings: el<HTMLButtonElement>("nav-settings"),
  navHome: el<HTMLButtonElement>("nav-home"),
  wizard: el("view-wizard"),
  home: el("view-home"),
  settings: el("view-settings"),
  orphans: el("orphans"),
  model: el<HTMLSelectElement>("model"),
  modelDesc: el("model-desc"),
  modelLicense: el("model-license"),
  loraSummary: el("lora-summary"),
  estimate: el("estimate"),
  gate: el("gate"),
  statusText: el("status-text"),
  statusSub: el("status-sub"),
  progress: el("progress"),
  progressFill: el("progress-fill"),
  start: el<HTMLButtonElement>("start"),
  open: el<HTMLButtonElement>("open"),
  tutorial: el<HTMLButtonElement>("tutorial"),
  openFolder: el<HTMLButtonElement>("open-folder"),
  outOpen: el<HTMLButtonElement>("out-open"),
  sync: el("sync"),
  stop: el<HTMLButtonElement>("stop"),
  dismiss: el<HTMLButtonElement>("dismiss"),
  catalogInfo: el("catalog-info"),
  log: el("log"),
  form: el<HTMLFormElement>("settings-form"),
  sMaxDph: el<HTMLInputElement>("s-max-dph"),
  sIdle: el<HTMLInputElement>("s-idle"),
  sMaxHours: el<HTMLInputElement>("s-max-hours"),
  sMinCredit: el<HTMLInputElement>("s-min-credit"),
  settingsMsg: el("settings-msg"),
  outDir: el("out-dir"),
  outPick: el<HTMLButtonElement>("out-pick"),
  outReset: el<HTMLButtonElement>("out-reset"),
  loraLink: el<HTMLInputElement>("lora-link"),
  loraAdd: el<HTMLButtonElement>("lora-add"),
  loraMsg: el("lora-msg"),
  loraList: el("lora-list"),
  accounts: el("accounts"),
  rerunWizard: el<HTMLButtonElement>("rerun-wizard"),
  catalogStatus: el("catalog-status"),
  catalogRefresh: el<HTMLButtonElement>("catalog-refresh"),
  modal: el("modal"),
  modalMsg: el("modal-msg"),
  modalOk: el<HTMLButtonElement>("modal-ok"),
  modalCancel: el<HTMLButtonElement>("modal-cancel"),
};

// ----- state ---------------------------------------------------------------------

let snapshot: Snapshot | null = null;
let state: SessionState = { kind: "idle", notice: null };
let cost: CostBar | null = null;
let sync: SyncReport | null = null;
let credit: number | null = null;
let estimate: Estimate | null = null;
let view: "wizard" | "home" | "settings" = "home";
let wizardStep = 0;
let tick: number | undefined;

const money = (n: number, digits = 2) => `$${n.toFixed(digits)}`;

function duration(secs: number): string {
  const m = Math.floor(Math.max(0, secs) / 60 + 1e-6);
  const hrs = Math.floor(m / 60);
  const mins = m % 60;
  if (hrs === 0) return `${mins} min`;
  return mins === 0 ? `${hrs} h` : `${hrs} h ${mins} min`;
}

function ago(unix: number | null): string {
  if (!unix) return "";
  const s = Date.now() / 1000 - unix;
  if (s < 90) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400 * 2) return `${Math.round(s / 3600)} h ago`;
  return new Date(unix * 1000).toLocaleDateString();
}

const active = () => state.kind !== "idle" && state.kind !== "failed";
const currentModel = () => snapshot?.models.find((m) => m.id === ui.model.value);

function needsSetup(): boolean {
  if (!snapshot) return false;
  const civitaiNeeded = snapshot.models.some((m) => m.needs_civitai) || snapshot.settings.loras.length > 0;
  return !snapshot.has_vast_key || (civitaiNeeded && !snapshot.has_civitai_key);
}

function showView(v: typeof view): void {
  view = v;
  ui.wizard.hidden = v !== "wizard";
  ui.home.hidden = v !== "home";
  ui.settings.hidden = v !== "settings";
  ui.navSettings.hidden = v !== "home";
  ui.navHome.hidden = v !== "settings";
  if (v === "wizard") renderWizard();
  if (v === "settings") renderSettings();
  if (v === "home") {
    render();
    void refreshEstimate();
  }
}

// ----- cost bar --------------------------------------------------------------------

function renderCostBar(): void {
  ui.costbar.classList.toggle("low", !!cost?.low);
  if (cost && active()) {
    const parts = [
      `${money(cost.hourly, 3)}/hr`,
      duration(cost.elapsed_s),
      `≈${money(cost.spent)} so far`,
    ];
    if (cost.credit !== null) {
      const left = `${money(cost.credit)} left`;
      parts.push(cost.low && cost.runway_s !== null ? `${left} (~${duration(cost.runway_s)})` : left);
    }
    if (cost.shutdown) {
      const [why, secs] = cost.shutdown;
      const what = why === "idle" ? "Idle shutdown" : why === "max_session" ? "Session limit" : "Shutdown";
      parts.push(`${what} in ${duration(secs)}`);
    }
    ui.costbar.textContent = parts.join(" · ");
    ui.costbar.title = cost.download_cost > 0 ? `Includes ${money(cost.download_cost)} one-off model download` : "";
  } else {
    ui.costbar.textContent = credit !== null ? `${money(credit)} Vast credit` : "";
    ui.costbar.title = "";
  }
}

// ----- home ----------------------------------------------------------------------

function describeOffer(o: OfferSummary): string {
  const where = o.location ? ` · ${o.location}` : "";
  const dl = o.download_cost >= 0.005 ? ` · model download ≈ ${money(o.download_cost)}` : "";
  return `${o.gpu_name} · ${money(o.hourly, 3)}/hr${where}${dl}`;
}

function stageText(stage: string | null, progress: number | null): string {
  switch (stage) {
    case null:
      return "Starting the GPU machine… (cheap machines can take up to 15 minutes)";
    case "booting":
      return "Starting services…";
    case "downloading":
      return progress === null ? "Downloading the model…" : `Downloading the model… ${Math.floor(progress)}%`;
    case "verifying":
      return "Checking the download…";
    case "starting":
    case "registering":
      return "Almost ready…";
    case "ready":
      return "Opening Invoke…";
    default:
      return "Setting up…";
  }
}

function elapsed(fromUnix: number): string {
  return duration(Date.now() / 1000 - fromUnix);
}

function render(): void {
  const s = state;
  const keysOk =
    !!snapshot?.has_vast_key &&
    (!!snapshot?.has_civitai_key || (!currentModel()?.needs_civitai && !activeLoras().length));
  const refused = !active() && estimate?.gate.kind === "refuse";

  ui.start.hidden = active();
  ui.start.disabled = !keysOk || refused || !currentModel();
  ui.stop.hidden = !active() || s.kind === "stopping";
  ui.open.hidden = s.kind !== "ready";
  ui.tutorial.hidden = s.kind !== "ready";
  ui.dismiss.hidden = !(s.kind === "failed" || (s.kind === "idle" && s.notice));
  ui.model.disabled = active();
  ui.progress.hidden = true;
  ui.statusSub.textContent = "";
  ui.estimate.hidden = active();
  ui.gate.hidden = active() || !estimate || estimate.gate.kind === "ok";
  window.clearInterval(tick);

  switch (s.kind) {
    case "idle":
      ui.statusText.textContent = s.notice ?? (keysOk ? "Ready to start." : "Finish setup to start.");
      ui.start.textContent = "Start";
      break;
    case "renting":
      ui.statusText.textContent = "Renting a GPU…";
      ui.statusSub.textContent =
        (s.offer ? describeOffer(s.offer) : "Finding the cheapest machine") +
        (s.attempt > 1 ? ` · try ${s.attempt} of ${s.max_attempts}` : "");
      break;
    case "provisioning":
      ui.statusText.textContent = stageText(s.stage, s.progress);
      ui.statusSub.textContent = describeOffer(s.offer) + (s.attempt > 1 ? ` · try ${s.attempt} of ${s.max_attempts}` : "");
      if (s.stage === "downloading" && s.progress !== null) {
        ui.progress.hidden = false;
        ui.progressFill.style.width = `${Math.min(100, s.progress)}%`;
      }
      break;
    case "ready": {
      const update = () => {
        if (s.kind !== "ready") return;
        ui.statusText.textContent = "Invoke is running in its own window.";
        ui.statusSub.textContent = `${describeOffer(s.offer)} · ready for ${elapsed(s.ready_unix)}`;
      };
      update();
      tick = window.setInterval(update, 15000);
      break;
    }
    case "stopping":
      ui.statusText.textContent =
        sync?.phase === "finishing" ? "Saving your last images, then shutting down the GPU…" : "Shutting down the GPU…";
      break;
    case "failed":
      ui.statusText.textContent = s.reason;
      ui.start.textContent = "Try again";
      break;
  }
  renderSync();
  renderCostBar();
}

function images(n: number): string {
  return `${n} image${n === 1 ? "" : "s"}`;
}

/** What output sync saved: while running, while finishing, and after Stop. */
function renderSync(): void {
  const r = sync;
  ui.sync.hidden = !r;
  if (!r) return;
  const canvas = r.canvas_saved ? ` and ${r.canvas_saved} Canvas tr${r.canvas_saved === 1 ? "y" : "ies"}` : "";
  const where = ` to ${r.folder}`;
  let text: string;
  let cls = "muted";
  switch (r.phase) {
    case "running":
      text =
        r.saved + r.canvas_saved === 0
          ? `New images are saved${where} as you make them.`
          : `Saved ${images(r.saved)}${canvas}${where}.`;
      if (r.missing && r.last_error) {
        text += ` ${images(r.missing)} not saved yet: ${r.last_error}`;
        cls = "warn";
      }
      break;
    case "finishing":
      text = `Saving your last images${where}…`;
      break;
    case "done":
      text = `Saved ${images(r.saved)}${canvas}${where}.`;
      cls = "ok";
      break;
    case "incomplete":
      text =
        `Saved ${images(r.saved)}${canvas}${where}, but some may be missing` +
        (r.missing ? ` (${r.missing} couldn't be downloaded)` : "") +
        (r.last_error ? `: ${r.last_error}` : ".");
      cls = "warn";
      break;
  }
  ui.sync.textContent = text;
  ui.sync.className = `sync small ${cls}`;
}

/** Plain words for a model's GPU architecture floor (Vast compute_cap units). */
function gpuClass(cap: number | null): string {
  if (cap === null || cap <= 750) return "";
  if (cap >= 890) return ", RTX 40-series or newer";
  if (cap >= 800) return ", RTX 30-series or newer";
  return "";
}

function activeLoras(): LoraView[] {
  const m = currentModel();
  if (!snapshot || !m) return [];
  return snapshot.settings.loras.filter((l) => l.enabled && l.family === m.base);
}

function renderModels(): void {
  if (!snapshot) return;
  const selected = ui.model.value || snapshot.settings.model_id || "";
  ui.model.replaceChildren(
    ...snapshot.models.map((m) => h("option", { value: m.id }, m.name + (m.nsfw ? " (NSFW)" : ""))),
  );
  if (selected && snapshot.models.some((m) => m.id === selected)) ui.model.value = selected;
  const m = currentModel();
  ui.modelDesc.textContent = m
    ? `${m.description} ${m.size_gb.toFixed(1)} GB download. Needs a GPU with ${m.min_vram_gb} GB of memory` +
      `${gpuClass(m.min_compute_cap)}.`
    : "No models available.";
  ui.modelLicense.textContent = m?.license_note ? `License: ${m.license_note}` : "";

  const on = activeLoras();
  const skipped = snapshot.settings.loras.filter((l) => l.enabled && m && l.family !== m.base);
  const parts: string[] = [];
  if (on.length) parts.push(`With LoRA${on.length > 1 ? "s" : ""}: ${on.map((l) => l.name).join(", ")}.`);
  if (skipped.length) parts.push(`Not used with this model: ${skipped.map((l) => `${l.name} (${l.base_model})`).join(", ")}.`);
  ui.loraSummary.textContent = parts.join(" ");

  const c = snapshot.catalog;
  const src =
    c.source === "online" ? `updated ${ago(c.fetched_unix)}` : c.source === "cached" ? `saved copy from ${ago(c.fetched_unix)}` : "built-in copy";
  ui.catalogInfo.textContent = `Model list: ${src}.` + (c.skipped.length ? ` Skipped: ${c.skipped.join("; ")}` : "");
}

async function refreshEstimate(): Promise<void> {
  const m = currentModel();
  if (!snapshot?.has_vast_key || !m || active()) return;
  ui.estimate.textContent = "Checking GPU prices…";
  try {
    estimate = await invoke<Estimate>("estimate", { modelId: m.id });
    credit = estimate.credit;
    if (estimate.hourly === null) {
      ui.estimate.textContent = `No GPU matches your settings right now (up to ${money(snapshot.settings.max_dph)}/hr). Try again later or raise the limit in Settings.`;
    } else {
      const dl = estimate.download_cost >= 0.005 ? ` + ${money(estimate.download_cost)} one-off download (${estimate.download_gb.toFixed(1)} GB)` : "";
      ui.estimate.textContent = `Cheapest GPU right now: ${estimate.gpu_name} · ${money(estimate.hourly, 3)}/hr${dl}.`;
    }
    ui.gate.textContent = estimate.gate.kind === "ok" ? "" : estimate.gate.message;
    ui.gate.className = `notice ${estimate.gate.kind}`;
  } catch (e) {
    estimate = null;
    ui.estimate.textContent = `Couldn't check prices: ${String(e)}`;
  }
  render();
}

function appendLog(line: string): void {
  ui.log.textContent += `${line}\n`;
  ui.log.scrollTop = ui.log.scrollHeight;
}

async function refreshCredit(): Promise<void> {
  if (!snapshot?.has_vast_key) return;
  try {
    credit = await invoke<number>("check_credit");
  } catch {
    /* keep the last value */
  }
  renderCostBar();
}

async function refreshOrphans(): Promise<void> {
  if (!snapshot?.has_vast_key) return;
  let orphans: Orphan[] = [];
  try {
    orphans = await invoke<Orphan[]>("scan_orphans");
  } catch (e) {
    appendLog(`orphan scan failed: ${String(e)}`);
  }
  ui.orphans.hidden = orphans.length === 0;
  ui.orphans.replaceChildren();
  if (orphans.length === 0) return;
  ui.orphans.append(
    h(
      "h2",
      {},
      orphans.length === 1
        ? "A GPU from an earlier session is still running"
        : `${orphans.length} GPUs from earlier sessions are still running`,
    ),
  );
  for (const o of orphans) {
    const price = o.hourly !== null ? ` · ${money(o.hourly, 3)}/hr` : "";
    ui.orphans.append(
      h(
        "div",
        { class: "orphan" },
        h("span", {}, `${o.gpu_name ?? "GPU"} (#${o.instance_id})${price} · ${o.status ?? "unknown"}`),
        o.can_reattach &&
          h(
            "button",
            { onclick: () => act(invoke("reattach_orphan", { instanceId: o.instance_id }), refreshOrphans) },
            "Reconnect",
          ),
        h(
          "button",
          { class: "danger", onclick: () => act(invoke("destroy_orphan", { instanceId: o.instance_id }), refreshOrphans) },
          "Shut it down",
        ),
      ),
    );
  }
}

async function act(p: Promise<unknown>, then?: () => Promise<void>): Promise<void> {
  try {
    await p;
  } catch (e) {
    ui.statusSub.textContent = String(e);
  }
  if (then) await then();
}

// ----- keys (wizard and settings) ----------------------------------------------------

const KEY_HINT = "A Vast API key is 64 characters.";

/** Last result per key field, so it survives re-rendering the step. */
const keyMsgs: Partial<Record<"vast" | "civitai", { cls: string; text: string }>> = {};

/** Paste field that checks the key with its service as soon as it's pasted. */
function keyField(which: "vast" | "civitai", onOk: () => void): HTMLElement {
  const label = which === "vast" ? "Vast API key" : "CivitAI API key";
  const input = h("input", { type: "password", autocomplete: "off", spellcheck: "false", id: `key-${which}`, placeholder: "Paste here" });
  const msg = h("p", { class: keyMsgs[which]?.cls ?? "muted key-msg", id: `key-${which}-msg` }, keyMsgs[which]?.text ?? "");
  const setMsg = (cls: string, text: string) => {
    msg.className = cls;
    msg.textContent = text;
    keyMsgs[which] = { cls, text };
  };
  const btn = h("button", { type: "button" }, "Check");
  let busy = false;
  const check = async () => {
    const value = input.value.trim();
    if (busy || !value) return;
    busy = true;
    btn.disabled = true;
    setMsg("muted key-msg", "Checking…");
    try {
      const r = await invoke<KeyCheck>("set_secret", { which, value });
      input.value = "";
      if (r.credit !== null) {
        credit = r.credit;
        const floor = snapshot?.settings.min_credit ?? 1;
        setMsg(
          r.credit < floor ? "error key-msg" : "ok key-msg",
          r.credit < floor
            ? `Connected, but you have ${money(r.credit)} of credit. Add at least ${money(floor - r.credit)} on Vast before starting.`
            : `Connected. You have ${money(r.credit)} of credit.`,
        );
      } else {
        setMsg("ok key-msg", `Connected as ${r.username}.`);
      }
      await load();
      onOk();
    } catch (e) {
      setMsg("error key-msg", String(e));
    } finally {
      busy = false;
      btn.disabled = false;
    }
  };
  btn.onclick = () => void check();
  input.addEventListener("paste", () => window.setTimeout(() => void check(), 0));
  input.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") void check();
  });
  input.addEventListener("input", () => {
    const v = input.value.trim();
    if (which === "vast" && /^[0-9a-fA-F]{64}$/.test(v)) void check();
    else if (which === "vast" && v.length > 0) setMsg("muted key-msg", KEY_HINT);
  });
  return h("div", { class: "keyfield" }, h("label", { for: input.id }, label), h("div", { class: "row" }, input, btn), msg);
}

const CIVITAI_VISIBILITY =
  "SlopTweak gives this key to the rented GPU so it can download models for you, so the GPU's host (the " +
  "person or company renting it out on Vast) can see it. It's your own key and you can revoke it at any " +
  "time by deleting it on CivitAI; SlopTweak will then ask for a new one.";

// ----- wizard ----------------------------------------------------------------------

interface Step {
  id: string;
  title: string;
  body: () => Child[];
  /** Next is disabled until this is true. */
  ready?: () => boolean;
  next?: string;
}

const STEPS: Step[] = [
  {
    id: "welcome",
    title: "Welcome to SlopTweak",
    body: () => [
      h(
        "p",
        {},
        "SlopTweak rents a graphics card (GPU) for you on Vast.ai, runs Invoke on it for making and editing " +
          "images, and shuts it down when you're done. You pay Vast directly, usually a few cents to a few " +
          "tens of cents per hour.",
      ),
      h("p", {}, "Setup takes about 10 minutes and you only do it once: two accounts and two keys."),
      h(
        "p",
        { class: "muted" },
        "Vast.ai and CivitAI each have their own terms, which apply to what you run and download: ",
        linkButton("vast_terms", "Vast.ai terms"),
        " ",
        linkButton("civitai_terms", "CivitAI terms"),
      ),
    ],
    next: "Let's go",
  },
  {
    id: "vast-account",
    title: "1. Create a Vast.ai account",
    body: () => [
      h("p", {}, linkButton("vast_signup", "Open Vast.ai", "primary-link")),
      h(
        "ol",
        {},
        h("li", {}, "Click Login / Sign up and create an account with your email."),
        h("li", {}, "Open the email from Vast and click the verification link. Vast won't rent you a GPU until you do."),
      ),
      shot("vast-signup", "Vast.ai sign-up"),
      h("p", { class: "muted" }, "Already have an account? Just press Next."),
    ],
  },
  {
    id: "vast-credit",
    title: "2. Add credit on Vast",
    body: () => [
      h("p", {}, linkButton("vast_billing", "Open Vast billing", "primary-link")),
      h(
        "ol",
        {},
        h("li", {}, "Click Add Credit and pay with a card or crypto. The minimum is $5, which covers many sessions."),
        h(
          "li",
          {},
          `SlopTweak won't start a GPU if your credit is below ${money(snapshot?.settings.min_credit ?? 1)} ` +
            "(you can change that in Settings). Vast stops a GPU when credit runs out.",
        ),
      ),
      shot("vast-billing", "Vast billing page with the Add Credit button"),
    ],
  },
  {
    id: "vast-key",
    title: "3. Connect SlopTweak to Vast",
    body: () => [
      h("p", {}, linkButton("vast_keys", "Open Vast API keys", "primary-link")),
      h(
        "ol",
        {},
        h("li", {}, "Click + New to create a key. Name it SlopTweak."),
        h("li", {}, "Copy the key and paste it below. SlopTweak checks it right away."),
      ),
      shot("vast-keys", "Vast API keys page"),
      keyField("vast", renderWizard),
      snapshot?.has_vast_key ? h("p", { class: "ok" }, "✓ Your Vast key is saved.") : null,
      h("p", { class: "muted small" }, "Keys are kept in Windows Credential Manager on this PC, never in a file."),
    ],
    ready: () => !!snapshot?.has_vast_key,
  },
  {
    id: "civitai-account",
    title: "4. Create a CivitAI account",
    body: () => [
      h("p", {}, linkButton("civitai_signup", "Open CivitAI", "primary-link")),
      h("p", {}, "Sign in or create an account. CivitAI hosts the models; SlopTweak downloads them with your account."),
      shot("civitai-signup", "CivitAI sign-in"),
      h("p", { class: "muted" }, "Already have an account? Just press Next."),
    ],
  },
  {
    id: "civitai-key",
    title: "5. Connect SlopTweak to CivitAI",
    body: () => [
      h("p", {}, linkButton("civitai_keys", "Open CivitAI account settings", "primary-link")),
      h(
        "ol",
        {},
        h("li", {}, "Scroll down to API Keys and click Add API key. Name it SlopTweak."),
        h("li", {}, "Copy the key and paste it below."),
      ),
      shot("civitai-keys", "CivitAI API Keys section"),
      h("p", { class: "notice warn", id: "civitai-visibility" }, CIVITAI_VISIBILITY),
      keyField("civitai", renderWizard),
      snapshot?.has_civitai_key ? h("p", { class: "ok" }, "✓ Your CivitAI key is saved.") : null,
    ],
    ready: () => !!snapshot?.has_civitai_key,
  },
  {
    id: "done",
    title: "You're all set",
    body: () => [
      h("p", {}, "Pick a model and press Start. SlopTweak finds the cheapest suitable GPU, sets it up, and opens Invoke."),
      h(
        "p",
        { class: "muted" },
        "The bar at the top shows what the GPU costs while it runs. Press Stop (or close SlopTweak) when you're " +
          "done and the GPU is shut down, so nothing keeps billing.",
      ),
    ],
    next: "Start using SlopTweak",
  },
];

function renderWizard(): void {
  const step = STEPS[wizardStep];
  if (!step) return;
  const ready = step.ready ? step.ready() : true;
  const last = wizardStep === STEPS.length - 1;
  const next = h(
    "button",
    {
      class: "primary",
      id: "wizard-next",
      disabled: !ready,
      onclick: () => {
        if (last) {
          showView("home");
          void refreshOrphans();
        } else {
          wizardStep++;
          renderWizard();
        }
      },
    },
    step.next ?? "Next",
  );
  ui.wizard.dataset.step = step.id;
  ui.wizard.replaceChildren(
    h("p", { class: "muted small" }, `Setup · step ${wizardStep + 1} of ${STEPS.length}`),
    h("h2", {}, step.title),
    ...step.body().filter((c): c is Node | string => !!c),
    h(
      "div",
      { class: "actions" },
      wizardStep > 0 &&
        h(
          "button",
          {
            class: "ghost",
            id: "wizard-back",
            onclick: () => {
              wizardStep--;
              renderWizard();
            },
          },
          "Back",
        ),
      next,
    ),
  );
}

function startWizard(): void {
  // Skip what's already done.
  wizardStep = 0;
  if (snapshot?.has_vast_key) wizardStep = STEPS.findIndex((s) => s.id === "civitai-account");
  showView("wizard");
}

// ----- settings ----------------------------------------------------------------------

function renderSettings(): void {
  if (!snapshot) return;
  const s = snapshot.settings;
  ui.sMaxDph.value = String(s.max_dph);
  ui.sIdle.value = String(s.idle_minutes);
  ui.sMaxHours.value = String(s.max_session_minutes / 60);
  ui.sMinCredit.value = String(s.min_credit);
  ui.outDir.textContent = s.output_dir_resolved + (s.output_dir ? "" : " (default)");
  ui.outReset.hidden = !s.output_dir;
  renderLoras();
  ui.accounts.replaceChildren(
    h("p", {}, snapshot.has_vast_key ? "✓ Vast key saved. Paste a new one to replace it." : "No Vast key yet."),
    keyField("vast", renderSettings),
    h("p", {}, snapshot.has_civitai_key ? "✓ CivitAI key saved. Paste a new one to replace it." : "No CivitAI key yet."),
    h("p", { class: "muted small" }, CIVITAI_VISIBILITY),
    keyField("civitai", renderSettings),
  );
  const c = snapshot.catalog;
  ui.catalogStatus.textContent =
    (c.source === "online"
      ? `Up to date (checked ${ago(c.fetched_unix)}).`
      : c.source === "cached"
        ? `Using the list saved ${ago(c.fetched_unix)}; couldn't check for a newer one.`
        : "Using the list built into this version; couldn't check for a newer one.") +
    ` ${snapshot.models.length} model${snapshot.models.length === 1 ? "" : "s"}.`;
}

function renderLoras(): void {
  if (!snapshot) return;
  const loras = snapshot.settings.loras;
  const model = currentModel();
  ui.loraList.replaceChildren(
    ...(loras.length === 0
      ? [h("p", { class: "muted" }, "None yet.")]
      : loras.map((l) => {
          const toggle = h("input", {
            type: "checkbox",
            checked: l.enabled,
            onchange: (e: Event) =>
              void loraAction(invoke("set_lora_enabled", { versionId: l.version_id, enabled: (e.target as HTMLInputElement).checked })),
          });
          const fits = !model || l.family === model.base;
          return h(
            "div",
            { class: "lora" },
            h("label", { class: "lora-name" }, toggle, ` ${l.name} `, h("span", { class: "muted" }, `${l.version_name} · ${l.base_model} · ${l.size_mb.toFixed(0)} MB`)),
            !fits && h("span", { class: "muted small" }, `Only used with ${l.base_model}-type models.`),
            l.trained_words.length > 0 && h("span", { class: "muted small" }, `Trigger words: ${l.trained_words.join(", ")}`),
            h(
              "span",
              { class: "lora-actions" },
              linkButton(`lora:${l.version_id}`, "Page"),
              h("button", { class: "ghost", onclick: () => void loraAction(invoke("remove_lora", { versionId: l.version_id })) }, "Remove"),
            ),
          );
        })),
  );
}

async function loraAction(p: Promise<unknown>): Promise<void> {
  try {
    await p;
    ui.loraMsg.textContent = "";
  } catch (e) {
    ui.loraMsg.textContent = String(e);
  }
  await load();
  renderLoras();
}

// ----- load + wiring -------------------------------------------------------------------

async function load(): Promise<void> {
  snapshot = await invoke<Snapshot>("get_snapshot");
  ui.mode.hidden = snapshot.mode !== "mock";
  state = snapshot.state;
  cost = snapshot.cost;
  sync = snapshot.sync;
  if (snapshot.credit !== null) credit = snapshot.credit;
  ui.log.textContent = snapshot.log.map((l) => `${l}\n`).join("");
  renderModels();
  render();
}

ui.navSettings.onclick = () => showView("settings");
ui.navHome.onclick = () => showView("home");
ui.rerunWizard.onclick = () => {
  wizardStep = 0;
  showView("wizard");
};
ui.model.onchange = () => {
  estimate = null;
  renderModels();
  render();
  void invoke("select_model", { modelId: ui.model.value }).catch(() => {});
  void refreshEstimate();
};
ui.start.onclick = () => act(invoke("start_session", { modelId: ui.model.value }));
ui.stop.onclick = () => act(invoke("stop_session"));
ui.open.onclick = () => act(invoke("open_invoke"));
ui.tutorial.onclick = () => act(invoke("show_tutorial"));
ui.openFolder.onclick = () => act(invoke("open_output_folder"));
ui.outOpen.onclick = async () => {
  try {
    await invoke("open_output_folder");
  } catch (e) {
    ui.outDir.textContent = String(e);
  }
};
ui.dismiss.onclick = () => act(invoke("dismiss"));

ui.form.onsubmit = async (e) => {
  e.preventDefault();
  ui.settingsMsg.textContent = "Saving…";
  try {
    await invoke("save_settings", {
      form: {
        max_dph: Number(ui.sMaxDph.value),
        idle_minutes: Math.round(Number(ui.sIdle.value)),
        max_session_minutes: Math.round(Number(ui.sMaxHours.value) * 60),
        min_credit: Number(ui.sMinCredit.value),
      },
    });
    ui.settingsMsg.textContent = "Saved.";
    estimate = null;
    await load();
  } catch (err) {
    ui.settingsMsg.textContent = String(err);
  }
};
ui.outPick.onclick = async () => {
  try {
    await invoke("pick_output_folder");
  } catch (e) {
    ui.outDir.textContent = String(e);
  }
  await load();
  renderSettings();
};
ui.outReset.onclick = async () => {
  await invoke("reset_output_folder").catch(() => {});
  await load();
  renderSettings();
};
ui.loraAdd.onclick = async () => {
  ui.loraMsg.textContent = "Checking the link on CivitAI…";
  ui.loraAdd.disabled = true;
  try {
    await invoke("add_lora", { link: ui.loraLink.value });
    ui.loraLink.value = "";
    ui.loraMsg.textContent = "Added.";
  } catch (e) {
    ui.loraMsg.textContent = String(e);
  } finally {
    ui.loraAdd.disabled = false;
  }
  await load();
  renderLoras();
};
ui.catalogRefresh.onclick = async () => {
  ui.catalogStatus.textContent = "Checking…";
  try {
    await invoke("refresh_catalog");
  } catch (e) {
    ui.catalogStatus.textContent = String(e);
    return;
  }
  await load();
  renderSettings();
};

ui.modalCancel.onclick = () => {
  ui.modal.hidden = true;
};
ui.modalOk.onclick = async () => {
  ui.modalOk.disabled = true;
  ui.modalCancel.disabled = true;
  ui.modalMsg.textContent = "Saving your last images and shutting down the GPU… this can take up to two minutes.";
  try {
    await invoke("confirm_close");
  } catch (e) {
    ui.modalMsg.textContent = String(e);
    ui.modalOk.disabled = false;
    ui.modalCancel.disabled = false;
  }
};

await listen<SessionState>("session-state", (e) => {
  const was = state.kind;
  state = e.payload;
  if (!active()) cost = null;
  render();
  if (was !== state.kind && (state.kind === "idle" || state.kind === "ready" || state.kind === "failed")) {
    void refreshCredit();
    if (!active()) void refreshEstimate();
  }
});
await listen<string>("session-log", (e) => appendLog(e.payload));
await listen<SyncReport>("sync-status", (e) => {
  sync = e.payload;
  render();
});
await listen<CostBar>("cost-bar", (e) => {
  cost = e.payload;
  if (cost.credit !== null) credit = cost.credit;
  renderCostBar();
});
await listen("catalog-updated", async () => {
  await load();
  if (view === "settings") renderSettings();
  if (view === "home") void refreshEstimate();
});
await listen("close-requested", () => {
  ui.modalMsg.textContent = "";
  ui.modal.hidden = false;
});

await load();
if (needsSetup()) {
  startWizard();
} else {
  showView("home");
  void refreshCredit();
  void refreshOrphans();
}
window.setInterval(() => {
  if (!active()) void refreshCredit();
}, 5 * 60 * 1000);
