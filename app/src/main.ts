import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";

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
  min_ram_gb: number | null;
  price_tier: number | null;
  good_for: string;
  backend: "invoke" | "comfyui";
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
  version: string;
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
  /** Which app the running GPU has; null when nothing runs. */
  active_backend: "invoke" | "comfyui" | null;
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

interface UpdateInfo {
  version: string;
  current: string;
  notes: string | null;
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
  openComfy: el<HTMLButtonElement>("open-comfy"),
  tutorial: el<HTMLButtonElement>("tutorial"),
  openFolder: el<HTMLButtonElement>("open-folder"),
  outOpen: el<HTMLButtonElement>("out-open"),
  sync: el("sync"),
  identity: el("identity"),
  idMode: el<HTMLSelectElement>("id-mode"),
  idIntro: el("id-intro"),
  idPromptLabel: el("id-prompt-label"),
  idAspectRow: el("id-aspect-row"),
  idPrompt: el<HTMLTextAreaElement>("id-prompt"),
  idAspect: el<HTMLSelectElement>("id-aspect"),
  idGo: el<HTMLButtonElement>("id-go"),
  idCancel: el<HTMLButtonElement>("id-cancel"),
  idMsg: el("id-msg"),
  idResults: el("id-results"),
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
  outLock: el("out-lock"),
  loraLink: el<HTMLInputElement>("lora-link"),
  loraAdd: el<HTMLButtonElement>("lora-add"),
  loraMsg: el("lora-msg"),
  loraList: el("lora-list"),
  accounts: el("accounts"),
  rerunWizard: el<HTMLButtonElement>("rerun-wizard"),
  catalogStatus: el("catalog-status"),
  catalogRefresh: el<HTMLButtonElement>("catalog-refresh"),
  update: el("update"),
  updateText: el("update-text"),
  updateNotes: el("update-notes"),
  updateInstall: el<HTMLButtonElement>("update-install"),
  updateNotesLink: el<HTMLButtonElement>("update-notes-link"),
  updateLater: el<HTMLButtonElement>("update-later"),
  updateMsg: el("update-msg"),
  updateCheck: el<HTMLButtonElement>("update-check"),
  aboutVersion: el("about-version"),
  aboutMsg: el("about-msg"),
  diagCopy: el<HTMLButtonElement>("diag-copy"),
  diagCopy2: el<HTMLButtonElement>("diag-copy-2"),
  diagMsg: el("diag-msg"),
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
let update: UpdateInfo | null = null;
let updateDismissed = false;
/** An update is downloading/installing; the app will exit, so no new session. */
let installing = false;
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
      return currentModel()?.backend === "comfyui" ? "Almost ready…" : "Opening Invoke…";
    default:
      return "Setting up…";
  }
}

function elapsed(fromUnix: number): string {
  return duration(Date.now() / 1000 - fromUnix);
}

function renderUpdate(): void {
  const show = update !== null && !updateDismissed;
  ui.update.hidden = !show;
  if (!update) return;
  ui.updateText.textContent = `SlopTweak ${update.version} is available (you have ${update.current}).`;
  ui.updateNotes.textContent = update.notes ?? "";
  const busy = active();
  ui.updateInstall.disabled = busy || installing;
  if (busy) ui.updateMsg.textContent = "You can update after you stop the GPU. Updating closes and reopens SlopTweak.";
  else if (ui.updateMsg.textContent?.startsWith("You can update")) ui.updateMsg.textContent = "";
}

function render(): void {
  renderUpdate();
  const s = state;
  const keysOk =
    !!snapshot?.has_vast_key &&
    (!!snapshot?.has_civitai_key || (!currentModel()?.needs_civitai && !activeLoras().length));
  const refused = !active() && estimate?.gate.kind === "refuse";

  ui.start.hidden = active();
  ui.start.disabled = !keysOk || refused || !currentModel() || installing;
  ui.stop.hidden = !active() || s.kind === "stopping";
  const identityGpu = isIdentity();
  ui.open.hidden = s.kind !== "ready" || identityGpu;
  ui.openComfy.hidden = s.kind !== "ready" || !identityGpu;
  ui.tutorial.hidden = s.kind !== "ready" || identityGpu;
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
        ui.statusText.textContent = identityGpu
          ? "Identity Edit is ready."
          : "Invoke is running in its own window.";
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
  renderIdentity();
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

/** What the $ marks mean. Live prices are in the estimate line below. */
function tierNote(tier: number | null): string {
  switch (tier) {
    case 1:
      return " $ = runs on the cheapest GPUs.";
    case 2:
      return " $$ = needs a bigger GPU, usually about twice the hourly price of $ models.";
    case 3:
      return " $$$ = needs a high-end GPU, several times the price of $ models.";
    default:
      return "";
  }
}

/** "Anima Turbo · $ · Anime, quick drafts (NSFW)" */
function modelLabel(m: ModelView): string {
  const parts = [m.name];
  if (m.price_tier) parts.push("$".repeat(m.price_tier));
  if (m.good_for) parts.push(m.good_for);
  return parts.join(" · ") + (m.nsfw ? " (NSFW)" : "");
}

function renderModels(): void {
  if (!snapshot) return;
  const selected = ui.model.value || snapshot.settings.model_id || "";
  ui.model.replaceChildren(
    ...snapshot.models.map((m) => h("option", { value: m.id }, modelLabel(m))),
  );
  if (selected && snapshot.models.some((m) => m.id === selected)) ui.model.value = selected;
  const m = currentModel();
  ui.modelDesc.textContent = m
    ? `${m.description} ${m.size_gb.toFixed(1)} GB download. Needs a GPU with ${m.min_vram_gb} GB of memory` +
      `${gpuClass(m.min_compute_cap)}` +
      (m.min_ram_gb ? ` and ${m.min_ram_gb} GB of system memory.` : ".") +
      tierNote(m.price_tier)
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

/** The folder can't change while a GPU runs: images saved so far would stay in the old one. */
function renderOutLock(): void {
  const locked = active();
  ui.outPick.disabled = locked;
  ui.outReset.disabled = locked;
  ui.outLock.hidden = !locked;
}

function renderSettings(): void {
  if (!snapshot) return;
  const s = snapshot.settings;
  ui.sMaxDph.value = String(s.max_dph);
  ui.sIdle.value = String(s.idle_minutes);
  ui.sMaxHours.value = String(s.max_session_minutes / 60);
  ui.sMinCredit.value = String(s.min_credit);
  ui.outDir.textContent = s.output_dir_resolved + (s.output_dir ? "" : " (default)");
  ui.outReset.hidden = !s.output_dir;
  renderOutLock();
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
  ui.aboutVersion.textContent = `SlopTweak ${snapshot.version}`;
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
ui.openComfy.onclick = () => act(invoke("open_comfyui"));
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

async function copyDiagnostics(msg: HTMLElement): Promise<void> {
  msg.textContent = "Collecting…";
  try {
    const d = await invoke<{ copied: boolean; text: string }>("copy_diagnostics");
    if (d.copied) {
      msg.textContent = "Copied. Paste it into your message; keys and tokens were removed.";
      return;
    }
    // Clipboard busy: show the report selected so Ctrl+C works.
    ui.log.closest("details")?.setAttribute("open", "");
    ui.log.textContent = d.text;
    const range = document.createRange();
    range.selectNodeContents(ui.log);
    window.getSelection()?.removeAllRanges();
    window.getSelection()?.addRange(range);
    msg.textContent = "Couldn't copy automatically. The report is selected under Details: press Ctrl+C.";
  } catch (e) {
    msg.textContent = String(e);
  }
}

ui.diagCopy.onclick = () => void copyDiagnostics(ui.diagMsg);
ui.diagCopy2.onclick = () => void copyDiagnostics(ui.aboutMsg);

ui.updateCheck.onclick = async () => {
  ui.aboutMsg.textContent = "Checking…";
  ui.updateCheck.disabled = true;
  try {
    update = await invoke<UpdateInfo | null>("check_update");
    updateDismissed = false;
    ui.aboutMsg.textContent = update
      ? `SlopTweak ${update.version} is available. Go back to the home screen to install it.`
      : "You have the latest version.";
  } catch (e) {
    ui.aboutMsg.textContent = String(e);
  } finally {
    ui.updateCheck.disabled = false;
  }
  render();
};
ui.updateLater.onclick = () => {
  updateDismissed = true;
  render();
};
ui.updateNotesLink.onclick = () => void openLink("release_notes");
ui.updateInstall.onclick = async () => {
  installing = true;
  render();
  ui.updateMsg.textContent = "Downloading the update…";
  try {
    await invoke("install_update");
    // Real updates close the app here; mock mode returns.
    ui.updateMsg.textContent = "Installed. SlopTweak will restart.";
    installing = false;
    render();
  } catch (e) {
    installing = false;
    render();
    ui.updateMsg.textContent = String(e);
  }
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

/** A short two-note chime: the Invoke window is open and ready. Synthesized,
 * so there's no sound file. Pressing Start counts as the user gesture that
 * WebView2's autoplay policy wants; if it's blocked anyway, stay silent. */
function chime(): void {
  try {
    const ctx = new AudioContext();
    const t0 = ctx.currentTime;
    [659.25, 987.77].forEach((hz, i) => {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      const t = t0 + i * 0.16;
      osc.type = "sine";
      osc.frequency.value = hz;
      gain.gain.setValueAtTime(0, t);
      gain.gain.linearRampToValueAtTime(0.18, t + 0.02);
      gain.gain.exponentialRampToValueAtTime(0.001, t + 0.9);
      osc.connect(gain).connect(ctx.destination);
      osc.start(t);
      osc.stop(t + 0.95);
    });
    window.setTimeout(() => void ctx.close(), 1500);
  } catch {
    /* no audio: fine */
  }
}

await listen<SessionState>("session-state", (e) => {
  const was = state.kind;
  state = e.payload;
  if (!active()) cost = null;
  render();
  renderOutLock();
  if (was !== "ready" && e.payload.kind === "ready") {
    chime();
    // Which app the GPU runs comes with the snapshot, not the state event.
    void load().then(refreshIdentity);
  }
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
await listen<UpdateInfo>("update-available", (e) => {
  update = e.payload;
  render();
});
await listen<number>("update-progress", (e) => {
  ui.updateMsg.textContent =
    e.payload >= 100 ? "Starting the installer…" : `Downloading the update… ${Math.round(e.payload)}%`;
});
await listen("close-requested", () => {
  ui.modalMsg.textContent = "";
  ui.modal.hidden = false;
});


// ----- Identity Edit panel -------------------------------------------------------------

interface IdentityImage {
  id: string;
  name: string;
  data_url: string;
}

interface IdentityView {
  refs: (IdentityImage | null)[];
  results: IdentityImage[];
  phase: "idle" | "uploading" | "running" | "done" | "failed" | "cancelled";
  message: string;
}

let identity: IdentityView = { refs: [null, null, null], results: [], phase: "idle", message: "" };
/** Slot 2 holds the picture to edit; slots 0 and 1 are character sheets. */
const BASE_SLOT = 2;
const identityEditing = () => ui.idMode.value === "edit";
/** An error from the last click (a bad request), shown until the next state change. */
let identityError = "";

const isIdentity = () => state.kind === "ready" && snapshot?.active_backend === "comfyui";
const identityBusy = () => identity.phase === "uploading" || identity.phase === "running";

async function refreshIdentity(): Promise<void> {
  try {
    identity = await invoke<IdentityView>("identity_state");
    identityError = "";
  } catch (e) {
    console.error(e);
  }
  renderIdentity();
}

function renderIdentity(): void {
  ui.identity.hidden = !isIdentity();
  if (!isIdentity()) return;
  const busy = identityBusy();
  const editing = identityEditing();
  el("ref-box-2").hidden = !editing;
  el("ref-box-1").hidden = editing;
  ui.idAspectRow.hidden = editing;
  ui.idIntro.textContent = editing
    ? "Add the picture you want to change, say what should change, and press Edit image. The result keeps the picture's shape. You can add a character sheet too, and paint just the part to change. Each image is saved to your output folder as it finishes."
    : "Add one or two of your characters, say what they're doing, and press Make image. The characters keep their look. Each image is saved to your output folder as it finishes.";
  ui.idPromptLabel.textContent = editing ? "What should change?" : "What are they doing?";
  ui.idPrompt.placeholder = editing
    ? "Change the jacket into a red hoodie. Keep everything else the same."
    : "They sit together at a sunny café table, laughing.";
  ui.idGo.textContent = editing ? "Edit image" : "Make image";
  el<HTMLButtonElement>("ref-pick-0").textContent = editing ? "Character sheet (optional)…" : "Character 1…";
  identity.refs.forEach((r, slot) => {
    const box = el(`ref-img-${slot}`);
    const empty = slot === BASE_SLOT ? "No picture yet" : editing ? "No sheet (optional)" : "No character yet";
    if (focus[slot] && focus[slot]!.imageId !== r?.id) delete focus[slot];
    const overlay = slot === BASE_SLOT ? painted?.overlay : focus[slot]?.overlay;
    box.replaceChildren(...(r ? [zoomable(h("img", { src: r.data_url, alt: r.name, title: `${r.name} (click to enlarge)` })),
      ...(overlay ? [h("img", { class: "mask-overlay", src: overlay, alt: "" })] : []),
    ] : [empty]));
    el<HTMLButtonElement>(`ref-clear-${slot}`).hidden = !r;
    el<HTMLButtonElement>(`ref-pick-${slot}`).disabled = busy;
    if (slot !== BASE_SLOT) {
      el<HTMLButtonElement>(`ref-focus-${slot}`).hidden = !r;
      el<HTMLButtonElement>(`ref-focus-${slot}`).disabled = busy;
      el<HTMLButtonElement>(`ref-focus-${slot}`).textContent = focus[slot] ? "Change the part…" : "Focus on a part…";
      el<HTMLButtonElement>(`ref-unfocus-${slot}`).hidden = !focus[slot];
      el<HTMLButtonElement>(`ref-unfocus-${slot}`).disabled = busy;
    }
  });
  renderEditTools(editing, identity.refs[BASE_SLOT] ?? null);
  const haveInputs = editing ? !!identity.refs[BASE_SLOT] : !!identity.refs[0] || !!identity.refs[1];
  ui.idGo.disabled = busy || !haveInputs || ui.idPrompt.value.trim() === "";
  ui.idGo.hidden = busy;
  ui.idCancel.hidden = !busy;
  ui.idMsg.textContent = identityError || identity.message;
  ui.idMsg.className = identity.phase === "failed" || identityError ? "notice" : "muted";
  ui.idResults.replaceChildren(
    ...identity.results.map((r) =>
      h(
        "figure",
        {},
        zoomable(h("img", { src: r.data_url, alt: "Result", title: "Click to enlarge" }), r.id),
        h(
          "figcaption",
          {},
          h("button", { class: "ghost", type: "button", disabled: busy, onclick: () => void identityUse(r.id, 0) }, "Use as character 1"),
          h("button", { class: "ghost", type: "button", disabled: busy, onclick: () => void identityUse(r.id, 1) }, "As character 2"),
          h(
            "button",
            {
              class: "ghost",
              type: "button",
              disabled: busy,
              onclick: () => {
                ui.idMode.value = "edit";
                void identityUse(r.id, BASE_SLOT);
              },
            },
            "Edit this image",
          ),
        ),
      ),
    ),
  );
}

async function identityAct(p: Promise<unknown>): Promise<void> {
  let failure = "";
  try {
    await p;
  } catch (e) {
    failure = String(e);
  }
  // The refresh clears the last error, so show this one after it.
  await refreshIdentity();
  if (failure) {
    identityError = failure;
    renderIdentity();
  }
}

const identityUse = (id: string, slot: number) => identityAct(invoke("identity_use_result", { id, slot }));

for (const slot of [0, 1, BASE_SLOT]) {
  el<HTMLButtonElement>(`ref-pick-${slot}`).onclick = () => void identityAct(invoke("identity_pick_ref", { slot }));
  el<HTMLButtonElement>(`ref-clear-${slot}`).onclick = () => void identityAct(invoke("identity_clear_ref", { slot }));
}
ui.idPrompt.oninput = () => renderIdentity();

// ----- strength slider and the painted area (Edit mode) -----
interface PaintedArea {
  baseId: string;
  /** Working canvas (long side <= MASK_WORK), painted red on transparent. */
  work: HTMLCanvasElement;
  /** PNG, white where the picture changes, at the picture's own size (base64). */
  png: string;
  width: number;
  height: number;
  bbox: { x: number; y: number; w: number; h: number };
  overlay: string;
}
let painted: PaintedArea | null = null;
const MASK_WORK = 1536;
const MASK_UNDO = 10;

/** A part of a character sheet (slot 0 or 1) to use instead of the whole picture. */
interface FocusArea {
  imageId: string;
  work: HTMLCanvasElement;
  /** Box around the painted part, in the picture's own pixels. */
  bbox: { x: number; y: number; w: number; h: number };
  overlay: string;
}
const focus: Record<number, FocusArea | undefined> = {};
/** Which picture the painting window is open on. */
let maskSlot = BASE_SLOT;

/** Box around everything painted on a canvas, scaled to a picture of `w` x `h` pixels. */
function paintedBox(c: HTMLCanvasElement, w: number, h: number): FocusArea["bbox"] | null {
  const { width, height } = c;
  const d = c.getContext("2d", { willReadFrequently: true })!.getImageData(0, 0, width, height).data;
  let x0 = width;
  let y0 = height;
  let x1 = -1;
  let y1 = -1;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (d[(y * width + x) * 4 + 3]! > 40) {
        if (x < x0) x0 = x;
        if (x > x1) x1 = x;
        if (y < y0) y0 = y;
        if (y > y1) y1 = y;
      }
    }
  }
  if (x1 < 0) return null;
  // Round outwards so the box never misses a painted pixel, and stay inside the picture.
  const kx = w / width;
  const ky = h / height;
  const bx0 = Math.max(0, Math.floor(x0 * kx));
  const by0 = Math.max(0, Math.floor(y0 * ky));
  const bx1 = Math.min(w, Math.ceil((x1 + 1) * kx));
  const by1 = Math.min(h, Math.ceil((y1 + 1) * ky));
  return { x: bx0, y: by0, w: Math.max(1, bx1 - bx0), h: Math.max(1, by1 - by0) };
}

const idStrength = el<HTMLInputElement>("id-strength");

function renderEditTools(editing: boolean, base: IdentityImage | null): void {
  if (painted && painted.baseId !== base?.id) painted = null;
  el("id-edit-tools").hidden = !editing;
  el<HTMLButtonElement>("mask-open").disabled = !base || identityBusy();
  el<HTMLButtonElement>("mask-open").textContent = painted ? "Change the painted area…" : "Paint the area to change…";
  el("mask-clear").hidden = !painted;
  el("mask-zoom-row").hidden = !painted;
  const v = Number(idStrength.value);
  el("id-strength-val").textContent = `${v}%`;
  el("id-strength-hint").textContent = painted
    ? "Only the painted area changes. Everything else keeps its exact pixels."
    : v >= 100
      ? "Redraws the picture from your words, keeping its look and its characters."
      : "Starts from your picture, so it changes less. It follows your words less closely.";
}

idStrength.oninput = () => renderIdentity();

// How closely the picture follows the character sheets (the node's `ref_boost`).
// 1 is the node's neutral. The pack recommends 4, but that copies the sheet's pose
// (and its colour swatches); 1 and below follow the words and drew cleaner hands.
const idPull = el<HTMLInputElement>("id-pull");
const PULL_DEFAULT = 1;

function renderPull(): void {
  const v = Number(idPull.value);
  el("id-pull-val").textContent = v === PULL_DEFAULT ? "Default" : Number.isInteger(v) ? v.toFixed(1) : v.toFixed(2);
  el("id-pull-hint").textContent =
    v >= 3
      ? "Sticks closely to the sheets and can copy their pose, layout and colour swatches."
      : v > PULL_DEFAULT
        ? "Sticks closer to the sheets. The pose may follow the sheet more than your words."
        : v === PULL_DEFAULT
          ? "The pose and scene follow your words while the characters keep their look."
          : "More freedom still. Often gives cleaner details such as hands; check the characters still look right.";
}

idPull.oninput = () => renderPull();
renderPull();
el("mask-clear").onclick = () => {
  painted = null;
  renderIdentity();
};

const masker = el("masker");
const maskBase = el<HTMLImageElement>("mask-base");
const maskCanvas = el<HTMLCanvasElement>("mask-canvas");
const maskSize = el<HTMLInputElement>("mask-size");
const maskCtx = () => maskCanvas.getContext("2d", { willReadFrequently: true })!;
let maskErase = false;
let maskUndo: ImageData[] = [];
let maskNatural = { w: 0, h: 0 };

function setMaskTool(erase: boolean): void {
  maskErase = erase;
  el("mask-paint").classList.toggle("on", !erase);
  el("mask-erase").classList.toggle("on", erase);
}

async function openMasker(slot: number): Promise<void> {
  const pic = identity.refs[slot];
  if (!pic) return;
  maskSlot = slot;
  el("mask-help").textContent =
    slot === BASE_SLOT
      ? "Paint over what should change. The rest of the picture is kept exactly as it is."
      : "Paint over the part to use, such as the face. Only that part (with a little around it) is shown to the model.";
  maskBase.src = pic.data_url;
  await maskBase.decode();
  maskNatural = { w: maskBase.naturalWidth, h: maskBase.naturalHeight };
  const k = Math.min(1, MASK_WORK / Math.max(maskNatural.w, maskNatural.h));
  maskCanvas.width = Math.max(1, Math.round(maskNatural.w * k));
  maskCanvas.height = Math.max(1, Math.round(maskNatural.h * k));
  maskCtx().clearRect(0, 0, maskCanvas.width, maskCanvas.height);
  const prior = slot === BASE_SLOT ? painted?.work : focus[slot]?.work;
  if (prior) maskCtx().drawImage(prior, 0, 0);
  maskUndo = [];
  setMaskTool(false);
  masker.hidden = false;
}

el("mask-open").onclick = () => void openMasker(BASE_SLOT);
for (const slot of [0, 1]) {
  el(`ref-focus-${slot}`).onclick = () => void openMasker(slot);
  el(`ref-unfocus-${slot}`).onclick = () => {
    delete focus[slot];
    renderIdentity();
  };
}

const maskPoint = (e: PointerEvent) => {
  const r = maskCanvas.getBoundingClientRect();
  const k = maskCanvas.width / r.width;
  return { x: (e.clientX - r.left) * k, y: (e.clientY - r.top) * k, k };
};

let stroke: { x: number; y: number } | null = null;
function strokeTo(e: PointerEvent): void {
  const p = maskPoint(e);
  const c = maskCtx();
  c.globalCompositeOperation = maskErase ? "destination-out" : "source-over";
  c.strokeStyle = c.fillStyle = "rgb(255, 40, 40)";
  c.lineCap = c.lineJoin = "round";
  c.lineWidth = Number(maskSize.value) * p.k;
  c.beginPath();
  c.moveTo(stroke!.x, stroke!.y);
  c.lineTo(p.x, p.y);
  c.stroke();
  stroke = { x: p.x, y: p.y };
}

maskCanvas.onpointerdown = (e) => {
  maskCanvas.setPointerCapture(e.pointerId);
  maskUndo.push(maskCtx().getImageData(0, 0, maskCanvas.width, maskCanvas.height));
  if (maskUndo.length > MASK_UNDO) maskUndo.shift();
  const p = maskPoint(e);
  stroke = { x: p.x, y: p.y };
  strokeTo(e); // a click paints a dot
};
maskCanvas.onpointermove = (e) => {
  if (stroke) strokeTo(e);
};
maskCanvas.onpointerup = maskCanvas.onpointercancel = () => {
  stroke = null;
};
el("mask-paint").onclick = () => setMaskTool(false);
el("mask-erase").onclick = () => setMaskTool(true);
el("mask-undo").onclick = () => {
  const s = maskUndo.pop();
  if (s) maskCtx().putImageData(s, 0, 0);
};
el("mask-reset").onclick = () => {
  maskUndo.push(maskCtx().getImageData(0, 0, maskCanvas.width, maskCanvas.height));
  maskCtx().clearRect(0, 0, maskCanvas.width, maskCanvas.height);
};
el("mask-cancel").onclick = () => {
  masker.hidden = true;
};

el("mask-done").onclick = async () => {
  const base = identity.refs[maskSlot];
  masker.hidden = true;
  if (!base) return;
  const { w, h } = maskNatural;
  if (maskSlot !== BASE_SLOT) {
    // A character sheet only needs the box around the paint.
    const bbox = paintedBox(maskCanvas, w, h);
    if (!bbox) {
      delete focus[maskSlot]; // nothing painted: use the whole picture
    } else {
      const work = document.createElement("canvas");
      work.width = maskCanvas.width;
      work.height = maskCanvas.height;
      work.getContext("2d")!.drawImage(maskCanvas, 0, 0);
      focus[maskSlot] = { imageId: base.id, work, bbox, overlay: work.toDataURL("image/png") };
    }
    renderIdentity();
    return;
  }
  // The picture's own size: scale the painted layer up, keep what is painted.
  const full = document.createElement("canvas");
  full.width = w;
  full.height = h;
  const fc = full.getContext("2d", { willReadFrequently: true })!;
  fc.drawImage(maskCanvas, 0, 0, w, h);
  const px = fc.getImageData(0, 0, w, h);
  let x0 = w;
  let y0 = h;
  let x1 = -1;
  let y1 = -1;
  const d = px.data;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const i = (y * w + x) * 4;
      const on = d[i + 3]! > 40;
      d[i] = d[i + 1] = d[i + 2] = on ? 255 : 0;
      d[i + 3] = 255;
      if (on) {
        if (x < x0) x0 = x;
        if (x > x1) x1 = x;
        if (y < y0) y0 = y;
        if (y > y1) y1 = y;
      }
    }
  }
  if (x1 < 0) {
    painted = null; // nothing painted
    renderIdentity();
    return;
  }
  fc.putImageData(px, 0, 0);
  const blob = await new Promise<Blob | null>((ok) => full.toBlob(ok, "image/png"));
  if (!blob) {
    identityError = "Couldn't keep the painted area.";
    renderIdentity();
    return;
  }
  const png = await new Promise<string>((ok) => {
    const r = new FileReader();
    r.onload = () => ok(String(r.result).split(",")[1] ?? "");
    r.readAsDataURL(blob);
  });
  const work = document.createElement("canvas");
  work.width = maskCanvas.width;
  work.height = maskCanvas.height;
  work.getContext("2d")!.drawImage(maskCanvas, 0, 0);
  painted = {
    baseId: base.id,
    work,
    png,
    width: w,
    height: h,
    bbox: { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 },
    overlay: work.toDataURL("image/png"),
  };
  renderIdentity();
};

// ----- zoom, copy, save -----
const lightbox = el("lightbox");
const lightboxImg = el<HTMLImageElement>("lightbox-img");
const lightboxMsg = el("lightbox-msg");
const lightboxSave = el<HTMLButtonElement>("lightbox-save");
let lightboxResult = "";

/** Click opens the big view; right-click copies the picture. */
function zoomable(img: HTMLImageElement, resultId = ""): HTMLImageElement {
  img.onclick = () => openLightbox(img.src, resultId);
  img.oncontextmenu = (e) => {
    e.preventDefault();
    void copyImage(img.src);
  };
  return img;
}

function openLightbox(src: string, resultId: string): void {
  lightboxImg.src = src;
  lightboxResult = resultId;
  lightboxSave.hidden = !resultId;
  lightboxMsg.textContent = "";
  lightbox.hidden = false;
}

const closeLightbox = () => {
  lightbox.hidden = true;
  lightboxImg.removeAttribute("src");
};

async function copyImage(dataUrl: string): Promise<void> {
  try {
    const img = new Image();
    img.src = dataUrl;
    await img.decode();
    const canvas = document.createElement("canvas");
    canvas.width = img.naturalWidth;
    canvas.height = img.naturalHeight;
    canvas.getContext("2d")!.drawImage(img, 0, 0);
    const blob = await new Promise<Blob | null>((ok) => canvas.toBlob(ok, "image/png"));
    if (!blob) throw new Error("no image");
    await navigator.clipboard.write([new ClipboardItem({ "image/png": blob })]);
    lightboxMsg.textContent = "Copied. Paste it into any app.";
    ui.idMsg.textContent = "Image copied.";
  } catch (e) {
    console.error(e);
    lightboxMsg.textContent = "Couldn't copy the image. Use Save to Downloads instead.";
    ui.idMsg.textContent = "Couldn't copy the image.";
  }
}

el("lightbox-close").onclick = closeLightbox;
el("lightbox-copy").onclick = () => void copyImage(lightboxImg.src);
lightboxImg.oncontextmenu = (e) => {
  e.preventDefault();
  void copyImage(lightboxImg.src);
};
lightbox.onclick = (e) => {
  if (e.target === lightbox) closeLightbox();
};
lightboxSave.onclick = async () => {
  try {
    const path = await invoke<string>("identity_save_result", { id: lightboxResult });
    lightboxMsg.textContent = `Saved to ${path}`;
  } catch (e) {
    lightboxMsg.textContent = String(e);
  }
};
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !lightbox.hidden) closeLightbox();
});

// ----- drop files / paste images into the picture boxes -----
const visibleSlots = () => (identityEditing() ? [BASE_SLOT, 0] : [0, 1]);
let hoverSlot: number | null = null;
for (const slot of [0, 1, BASE_SLOT]) {
  const box = el(`ref-box-${slot}`);
  box.onmouseenter = () => (hoverSlot = slot);
  box.onmouseleave = () => (hoverSlot = null);
}

function slotAt(x: number, y: number): number | null {
  const box = document.elementFromPoint(x / devicePixelRatio, y / devicePixelRatio)?.closest(".ref");
  const m = box?.id.match(/^ref-box-(\d)$/);
  return m ? Number(m[1]) : null;
}

/** Where a pasted or dropped picture goes: the box it's over, else the first empty one. */
function pickSlot(over: number | null, taken: Set<number>): number {
  const slots = visibleSlots();
  if (over !== null && slots.includes(over) && !taken.has(over)) return over;
  return slots.find((s) => !identity.refs[s] && !taken.has(s)) ?? slots.find((s) => !taken.has(s)) ?? slots[0]!;
}

function markDrop(slot: number | null): void {
  for (const s of [0, 1, BASE_SLOT]) el(`ref-img-${s}`).classList.toggle("drop", s === slot);
}

void getCurrentWebview().onDragDropEvent((event) => {
  const p = event.payload;
  if (!isIdentity() || identityBusy() || p.type === "leave") return markDrop(null);
  const over = slotAt(p.position.x, p.position.y);
  if (p.type !== "drop") return markDrop(over !== null && visibleSlots().includes(over) ? over : null);
  markDrop(null);
  const taken = new Set<number>();
  const jobs = p.paths.slice(0, visibleSlots().length).map((path, i) => {
    const slot = pickSlot(i === 0 ? over : null, taken);
    taken.add(slot);
    return invoke("identity_set_ref_path", { slot, path });
  });
  void identityAct(Promise.all(jobs));
}).catch((e) => console.error("drag and drop is unavailable", e));

document.addEventListener("paste", (e) => {
  if (!isIdentity() || identityBusy()) return;
  const files = [...(e.clipboardData?.files ?? [])].filter((f) => f.type.startsWith("image/"));
  if (files.length === 0) return; // plain text: let the box handle it
  e.preventDefault();
  const taken = new Set<number>();
  const jobs = files.slice(0, visibleSlots().length).map(async (f, i) => {
    const slot = pickSlot(i === 0 ? hoverSlot : null, taken);
    taken.add(slot);
    const bytes = new Uint8Array(await f.arrayBuffer());
    let bin = "";
    for (let k = 0; k < bytes.length; k += 0x8000) bin += String.fromCharCode(...bytes.subarray(k, k + 0x8000));
    await invoke("identity_set_ref_data", { slot, name: f.name || "pasted image", data: btoa(bin) });
  });
  void identityAct(Promise.all(jobs));
});
ui.idMode.onchange = () => renderIdentity();
ui.idGo.onclick = () =>
  void identityAct(
    invoke("identity_generate", {
      prompt: ui.idPrompt.value,
      aspect: ui.idAspect.value,
      mode: ui.idMode.value,
      strength: identityEditing() ? Number(idStrength.value) : null,
      pull: Number(idPull.value),
      mask:
        identityEditing() && painted
          ? {
              png: painted.png,
              width: painted.width,
              height: painted.height,
              bbox: painted.bbox,
              zoom: el<HTMLInputElement>("mask-zoom").checked,
            }
          : null,
      focus: [0, 1].flatMap((slot) => {
        const f = focus[slot];
        // Edit mode only uses sheet 1 (slot 0); New mode uses both.
        return f && (slot === 0 || !identityEditing()) ? [{ slot, bbox: f.bbox }] : [];
      }),
    }),
  );
ui.idCancel.onclick = () => void identityAct(invoke("identity_cancel"));
await listen("identity-changed", () => void refreshIdentity());

await load();
void refreshIdentity();
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
