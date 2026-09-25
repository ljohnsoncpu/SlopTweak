import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ----- types mirrored from src-tauri/src/session.rs and lib.rs --------------

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
  size_gb: number;
  min_vram_gb: number;
  needs_civitai: boolean;
  nsfw: boolean;
}

interface Snapshot {
  mode: "vast" | "mock";
  state: SessionState;
  log: string[];
  has_vast_key: boolean;
  has_civitai_key: boolean;
  models: ModelView[];
}

interface Orphan {
  instance_id: number;
  gpu_name: string | null;
  hourly: number | null;
  status: string | null;
  can_reattach: boolean;
}

// ----- dom -------------------------------------------------------------------

function el<T extends HTMLElement = HTMLElement>(id: string): T {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing #${id}`);
  return e as T;
}

const ui = {
  mode: el("mode"),
  credit: el("credit"),
  orphans: el("orphans"),
  keys: el("keys"),
  vastRow: el("vast-key-row"),
  vastKey: el<HTMLInputElement>("vast-key"),
  vastSave: el<HTMLButtonElement>("vast-save"),
  civitaiRow: el("civitai-key-row"),
  civitaiKey: el<HTMLInputElement>("civitai-key"),
  civitaiSave: el<HTMLButtonElement>("civitai-save"),
  keysMsg: el("keys-msg"),
  model: el<HTMLSelectElement>("model"),
  modelDesc: el("model-desc"),
  statusText: el("status-text"),
  statusSub: el("status-sub"),
  progress: el("progress"),
  progressFill: el("progress-fill"),
  start: el<HTMLButtonElement>("start"),
  open: el<HTMLButtonElement>("open"),
  stop: el<HTMLButtonElement>("stop"),
  dismiss: el<HTMLButtonElement>("dismiss"),
  log: el("log"),
  modal: el("modal"),
  modalMsg: el("modal-msg"),
  modalOk: el<HTMLButtonElement>("modal-ok"),
  modalCancel: el<HTMLButtonElement>("modal-cancel"),
};

let snapshot: Snapshot | null = null;
let state: SessionState = { kind: "idle", notice: null };
let tick: number | undefined;

const money = (n: number, digits = 2) => `$${n.toFixed(digits)}`;

function describeOffer(o: OfferSummary): string {
  const where = o.location ? ` · ${o.location}` : "";
  const dl = o.download_cost >= 0.005 ? ` · model download ≈ ${money(o.download_cost)}` : "";
  return `${o.gpu_name} · ${money(o.hourly, 3)}/hr${where}${dl}`;
}

function stageText(stage: string | null, progress: number | null): string {
  switch (stage) {
    case null:
      return "Starting the GPU machine…";
    case "booting":
      return "Starting services…";
    case "downloading":
      return progress === null
        ? "Downloading the model…"
        : `Downloading the model… ${Math.floor(progress)}%`;
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
  const s = Math.max(0, Math.floor(Date.now() / 1000 - fromUnix));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h} h ${m} min` : `${m} min`;
}

function render(): void {
  const s = state;
  const active = s.kind !== "idle" && s.kind !== "failed";
  const keysOk =
    !!snapshot?.has_vast_key &&
    (!!snapshot?.has_civitai_key || !currentModel()?.needs_civitai);

  ui.start.hidden = active;
  ui.start.disabled = !keysOk;
  ui.stop.hidden = !active || s.kind === "stopping";
  ui.open.hidden = s.kind !== "ready";
  ui.dismiss.hidden = !(s.kind === "failed" || (s.kind === "idle" && s.notice));
  ui.model.disabled = active;
  ui.progress.hidden = true;
  ui.statusSub.textContent = "";
  window.clearInterval(tick);

  switch (s.kind) {
    case "idle":
      ui.statusText.textContent = s.notice ?? (keysOk ? "Ready to start." : "Add your keys to start.");
      ui.start.textContent = "Start";
      break;
    case "renting":
      ui.statusText.textContent = "Renting a GPU…";
      ui.statusSub.textContent =
        (s.offer ? describeOffer(s.offer) : "Finding the best offer") +
        (s.attempt > 1 ? ` · try ${s.attempt} of ${s.max_attempts}` : "");
      break;
    case "provisioning":
      ui.statusText.textContent = stageText(s.stage, s.progress);
      ui.statusSub.textContent =
        describeOffer(s.offer) + (s.attempt > 1 ? ` · try ${s.attempt} of ${s.max_attempts}` : "");
      if (s.stage === "downloading" && s.progress !== null) {
        ui.progress.hidden = false;
        ui.progressFill.style.width = `${Math.min(100, s.progress)}%`;
      }
      break;
    case "ready": {
      const update = () => {
        if (s.kind !== "ready") return;
        const hours = Math.max(0, Date.now() / 1000 - s.ready_unix) / 3600;
        ui.statusText.textContent = "Invoke is running in its own window.";
        ui.statusSub.textContent =
          `${describeOffer(s.offer)} · ${elapsed(s.ready_unix)} · ` +
          `≈ ${money(hours * s.offer.hourly + s.offer.download_cost)} so far`;
      };
      update();
      tick = window.setInterval(update, 15000);
      break;
    }
    case "stopping":
      ui.statusText.textContent = "Shutting down the GPU…";
      break;
    case "failed":
      ui.statusText.textContent = s.reason;
      ui.start.textContent = "Try again";
      break;
  }
}

function currentModel(): ModelView | undefined {
  return snapshot?.models.find((m) => m.id === ui.model.value);
}

function renderKeys(): void {
  if (!snapshot) return;
  const needCivitai = !!currentModel()?.needs_civitai && !snapshot.has_civitai_key;
  ui.vastRow.hidden = snapshot.has_vast_key;
  ui.civitaiRow.hidden = !needCivitai;
  ui.keys.hidden = snapshot.has_vast_key && !needCivitai;
}

function renderModels(): void {
  if (!snapshot) return;
  const selected = ui.model.value;
  ui.model.replaceChildren(
    ...snapshot.models.map((m) => {
      const o = document.createElement("option");
      o.value = m.id;
      o.textContent = m.name;
      return o;
    }),
  );
  if (selected) ui.model.value = selected;
  const m = currentModel();
  ui.modelDesc.textContent = m
    ? `${m.description} ${m.size_gb.toFixed(1)} GB download, needs ${m.min_vram_gb} GB of GPU memory.`
    : "";
}

function appendLog(line: string): void {
  ui.log.textContent += `${line}\n`;
  ui.log.scrollTop = ui.log.scrollHeight;
}

async function refreshCredit(): Promise<void> {
  if (!snapshot?.has_vast_key) return;
  try {
    const credit = await invoke<number>("check_credit");
    ui.credit.textContent = `${money(credit)} credit`;
  } catch {
    ui.credit.textContent = "";
  }
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
  const h = document.createElement("h2");
  h.textContent =
    orphans.length === 1
      ? "A GPU from an earlier session is still running"
      : `${orphans.length} GPUs from earlier sessions are still running`;
  ui.orphans.append(h);
  for (const o of orphans) {
    const row = document.createElement("div");
    row.className = "orphan";
    const label = document.createElement("span");
    const price = o.hourly !== null ? ` · ${money(o.hourly, 3)}/hr` : "";
    label.textContent = `${o.gpu_name ?? "GPU"} (#${o.instance_id})${price} · ${o.status ?? "unknown"}`;
    const kill = document.createElement("button");
    kill.className = "danger";
    kill.textContent = "Shut it down";
    kill.onclick = () => act(invoke("destroy_orphan", { instanceId: o.instance_id }), refreshOrphans);
    row.append(label);
    if (o.can_reattach) {
      const re = document.createElement("button");
      re.textContent = "Reconnect";
      re.onclick = () => act(invoke("reattach_orphan", { instanceId: o.instance_id }), refreshOrphans);
      row.append(re);
    }
    row.append(kill);
    ui.orphans.append(row);
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

async function load(): Promise<void> {
  snapshot = await invoke<Snapshot>("get_snapshot");
  ui.mode.hidden = snapshot.mode !== "mock";
  state = snapshot.state;
  ui.log.textContent = snapshot.log.map((l) => `${l}\n`).join("");
  renderModels();
  renderKeys();
  render();
}

async function saveKey(which: "vast" | "civitai", input: HTMLInputElement): Promise<void> {
  ui.keysMsg.textContent = "Checking…";
  try {
    const credit = await invoke<number | null>("set_secret", { which, value: input.value });
    input.value = "";
    ui.keysMsg.textContent =
      credit !== null ? `Vast key saved. You have ${money(credit)} of credit.` : "Key saved.";
    await load();
    await refreshCredit();
    await refreshOrphans();
  } catch (e) {
    ui.keysMsg.textContent = String(e);
  }
}

// ----- wiring ----------------------------------------------------------------

ui.model.onchange = () => {
  renderModels();
  renderKeys();
  render();
};
ui.start.onclick = () => act(invoke("start_session", { modelId: ui.model.value }));
ui.stop.onclick = () => act(invoke("stop_session"));
ui.open.onclick = () => act(invoke("open_invoke"));
ui.dismiss.onclick = () => act(invoke("dismiss"));
ui.vastSave.onclick = () => saveKey("vast", ui.vastKey);
ui.civitaiSave.onclick = () => saveKey("civitai", ui.civitaiKey);
ui.modalCancel.onclick = () => {
  ui.modal.hidden = true;
};
ui.modalOk.onclick = async () => {
  ui.modalOk.disabled = true;
  ui.modalCancel.disabled = true;
  ui.modalMsg.textContent = "Shutting down the GPU… this can take up to a minute.";
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
  render();
  if (was !== state.kind && (state.kind === "idle" || state.kind === "ready")) {
    void refreshCredit();
  }
});
await listen<string>("session-log", (e) => appendLog(e.payload));
await listen("close-requested", () => {
  ui.modalMsg.textContent = "";
  ui.modal.hidden = false;
});

await load();
await refreshCredit();
await refreshOrphans();
window.setInterval(() => void refreshCredit(), 5 * 60 * 1000);
