//! SlopTweak launcher: Tauri shell, commands, and wiring.

mod catalog;
mod civitai;
mod config;
mod cost;
mod persist;
mod provider;
mod redact;
mod remote;
mod secrets;
mod session;
mod sidecar;
mod sync;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use url::Url;

use crate::catalog::{Catalog, Model};
use crate::civitai::{CivitaiApi, CivitaiError, HttpCivitai, Lora, MockCivitai};
use crate::config::{Settings, UserSettings};
use crate::cost::{CostBar, Gate};
use crate::persist::{now_unix, RecordFile};
use crate::provider::machines::MachineFile;
use crate::provider::mock::{parse_script, MockProvider, MockTimings};
use crate::provider::offers;
use crate::provider::vast::VastProvider;
use crate::provider::GpuProvider;
use crate::remote::{RemoteWindows, TutorialSignal};
use crate::secrets::{KeyringStore, SecretStore};
use crate::session::{Deps, Orphan, SessionManager, SessionState, Timing, Ui};
use crate::sidecar::HttpSidecar;
use crate::sync::{SyncReport, SyncStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Vast,
    Mock,
}

struct TauriUi {
    app: AppHandle,
    remote: RemoteWindows,
}

impl Ui for TauriUi {
    fn state_changed(&self, state: &SessionState) {
        let _ = self.app.emit_to("main", "session-state", state);
    }

    fn log(&self, line: &str) {
        let _ = self.app.emit_to("main", "session-log", line);
    }

    fn open_remote(&self, url: Url) {
        // First sessions get the tutorial until it's finished or skipped.
        let tutorial = !self.app.state::<AppState>().settings().tutorial_done;
        if let Err(e) = self.remote.open(url, tutorial) {
            eprintln!("[sloptweak] couldn't open the Invoke window: {e}");
        }
    }

    fn close_remote(&self) {
        self.remote.close();
    }

    fn sync_changed(&self, report: &SyncReport) {
        let _ = self.app.emit_to("main", "sync-status", report);
    }
}

/// Provider plus the matching sidecar client.
type Backends = (Arc<dyn GpuProvider>, Arc<dyn sidecar::SidecarApi>);

struct AppState {
    mode: Mode,
    secrets: Arc<dyn SecretStore>,
    data_dir: PathBuf,
    config_dir: PathBuf,
    default_output_dir: PathBuf,
    ui: Arc<TauriUi>,
    manager: Mutex<Option<Arc<SessionManager>>>,
    catalog: Mutex<Catalog>,
    civitai: Arc<dyn CivitaiApi>,
    /// Last known Vast credit and when it was read.
    credit: Mutex<Option<(f64, u64)>>,
    /// Serialises read-modify-write of settings.json.
    settings_lock: Mutex<()>,
}

fn output_dir(config_dir: &std::path::Path, default: &std::path::Path) -> PathBuf {
    Settings::load(config_dir)
        .output_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| default.to_path_buf())
}

/// Mock-mode credit; `SLOPTWEAK_MOCK_CREDIT` lets the UI check exercise the
/// low-balance gate.
fn mock_credit() -> f64 {
    std::env::var("SLOPTWEAK_MOCK_CREDIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(11.55)
}

impl AppState {
    fn secret(&self, name: &str) -> Option<String> {
        self.secrets.get(name).ok().flatten()
    }

    fn settings(&self) -> Settings {
        Settings::load(&self.config_dir)
    }

    /// The output folder: the user's pick, or Pictures\SlopTweak.
    fn output_dir(&self) -> PathBuf {
        output_dir(&self.config_dir, &self.default_output_dir)
    }

    fn remember_credit(&self, credit: f64) {
        *self.credit.lock().unwrap() = Some((credit, now_unix()));
    }

    fn credit_age(&self) -> Option<u64> {
        self.credit
            .lock()
            .unwrap()
            .map(|(_, at)| now_unix().saturating_sub(at))
    }

    /// The running cost while an instance bills; `None` otherwise.
    fn cost_bar(&self) -> Option<CostBar> {
        let m = self.manager.lock().unwrap().clone()?;
        let since = m.billing_since()?;
        let state = m.state();
        let (offer, deadlines) = match &state {
            SessionState::Provisioning { offer, .. } => (offer, None),
            SessionState::Ready {
                offer, deadlines, ..
            } => (offer, deadlines.as_ref()),
            _ => return None,
        };
        Some(cost::bar(&cost::BarInputs {
            hourly: offer.hourly,
            download_cost: offer.download_cost,
            started_unix: since,
            now_unix: now_unix(),
            credit: self.credit.lock().unwrap().map(|(c, _)| c),
            deadlines,
        }))
    }

    fn provider(&self) -> Result<Backends, String> {
        match self.mode {
            Mode::Mock => {
                let script = std::env::var("SLOPTWEAK_MOCK_SCRIPT").unwrap_or_default();
                let mut mock = MockProvider::new(MockTimings::default(), parse_script(&script))
                    .with_credit(mock_credit());
                if let Some(base) = std::env::var("SLOPTWEAK_MOCK_REMOTE")
                    .ok()
                    .and_then(|u| Url::parse(&u).ok())
                {
                    mock = mock.with_remote_base(base);
                }
                if let Some(n) = std::env::var("SLOPTWEAK_MOCK_IMAGES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                {
                    mock = mock.with_auto_images(n, Duration::from_secs(3));
                }
                // Dev: a real sidecar.py at SLOPTWEAK_MOCK_REMOTE (with
                // SLOPTWEAK_DEV_LAUNCH_SECRET) instead of the fake one.
                let sidecar: Arc<dyn sidecar::SidecarApi> = if cfg!(debug_assertions)
                    && std::env::var("SLOPTWEAK_MOCK_SIDECAR").as_deref() == Ok("http")
                {
                    Arc::new(HttpSidecar::default())
                } else {
                    Arc::new(mock.sidecar())
                };
                Ok((Arc::new(mock), sidecar))
            }
            Mode::Vast => {
                let key = self
                    .secret(secrets::VAST_API_KEY)
                    .ok_or("Add your Vast API key first.")?;
                Ok((
                    Arc::new(VastProvider::new(key)),
                    Arc::new(HttpSidecar::default()),
                ))
            }
        }
    }

    /// The session manager, built on first use (it needs the Vast key).
    fn manager(&self) -> Result<Arc<SessionManager>, String> {
        let mut slot = self.manager.lock().unwrap();
        if let Some(m) = slot.as_ref() {
            return Ok(m.clone());
        }
        let (provider, sidecar) = self.provider()?;
        let deps = Deps {
            provider,
            provider_name: match self.mode {
                Mode::Vast => "vast".into(),
                Mode::Mock => "mock".into(),
            },
            sidecar,
            secrets: self.secrets.clone(),
            records: RecordFile::new(&self.data_dir),
            machines: MachineFile::new(&self.data_dir),
            syncs: SyncStore::new(&self.data_dir),
            output_dir: {
                let (config, default) = (self.config_dir.clone(), self.default_output_dir.clone());
                Arc::new(move || output_dir(&config, &default))
            },
            ui: self.ui.clone(),
        };
        let m = SessionManager::new(deps, Timing::default());
        *slot = Some(m.clone());
        Ok(m)
    }

    /// Drop the manager so the next use picks up a new key. Only when idle.
    fn reset_manager(&self) -> Result<(), String> {
        let mut slot = self.manager.lock().unwrap();
        if let Some(m) = slot.as_ref() {
            if m.is_busy() || m.state().is_active() {
                return Err("Stop the current session first.".into());
            }
        }
        *slot = None;
        Ok(())
    }
}

// ----- views -------------------------------------------------------------------

#[derive(Serialize)]
struct ModelView {
    id: String,
    name: String,
    description: String,
    base: String,
    size_gb: f64,
    min_vram_gb: f64,
    needs_civitai: bool,
    nsfw: bool,
    license_note: String,
    /// The model's own GPU architecture floor, if it sets one.
    min_compute_cap: Option<u32>,
}

impl From<&Model> for ModelView {
    fn from(m: &Model) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            description: m.description.clone(),
            base: m.base.clone(),
            size_gb: m.total_bytes() as f64 / 1e9,
            min_vram_gb: m.min_vram_gb,
            needs_civitai: m.needs_civitai(),
            nsfw: m.nsfw,
            license_note: m.license_note.clone(),
            min_compute_cap: m.min_compute_cap,
        }
    }
}

#[derive(Serialize)]
struct CatalogInfo {
    source: catalog::Source,
    fetched_unix: Option<u64>,
    skipped: Vec<String>,
}

#[derive(Serialize)]
struct LoraView {
    #[serde(flatten)]
    lora: Lora,
    /// Catalog `base` family, if CivitAI's base model is one we know.
    family: Option<&'static str>,
    size_mb: f64,
}

impl From<&Lora> for LoraView {
    fn from(l: &Lora) -> Self {
        Self {
            family: civitai::family(&l.base_model),
            size_mb: l.file.size_bytes as f64 / 1e6,
            lora: l.clone(),
        }
    }
}

#[derive(Serialize)]
struct SettingsView {
    model_id: Option<String>,
    max_dph: f64,
    idle_minutes: u32,
    max_session_minutes: u32,
    min_credit: f64,
    output_dir: Option<String>,
    /// `output_dir`, or the default when unset.
    output_dir_resolved: String,
    loras: Vec<LoraView>,
}

#[derive(Serialize)]
struct Snapshot {
    mode: Mode,
    state: SessionState,
    log: Vec<String>,
    has_vast_key: bool,
    has_civitai_key: bool,
    models: Vec<ModelView>,
    catalog: CatalogInfo,
    settings: SettingsView,
    credit: Option<f64>,
    cost: Option<CostBar>,
    /// What output sync saved (this or the last session).
    sync: Option<SyncReport>,
}

fn settings_view(st: &AppState, s: &Settings) -> SettingsView {
    let u = s.user();
    SettingsView {
        model_id: u.model_id,
        max_dph: u.max_dph,
        idle_minutes: u.idle_minutes,
        max_session_minutes: u.max_session_minutes,
        min_credit: u.min_credit,
        output_dir_resolved: u
            .output_dir
            .clone()
            .unwrap_or_else(|| st.default_output_dir.to_string_lossy().into_owned()),
        output_dir: u.output_dir,
        loras: u.loras.iter().map(LoraView::from).collect(),
    }
}

// ----- commands ----------------------------------------------------------------

#[tauri::command]
async fn get_snapshot(st: State<'_, AppState>) -> Result<Snapshot, String> {
    let (state, log, sync) = match st.manager() {
        Ok(m) => (m.state(), m.log_lines(), m.sync_report()),
        Err(_) => (SessionState::idle(), vec![], None),
    };
    let catalog = st.catalog.lock().unwrap().clone();
    let settings = st.settings();
    Ok(Snapshot {
        mode: st.mode,
        cost: st.cost_bar(),
        state,
        log,
        has_vast_key: st.secret(secrets::VAST_API_KEY).is_some(),
        has_civitai_key: st.secret(secrets::CIVITAI_TOKEN).is_some(),
        models: catalog.models.iter().map(ModelView::from).collect(),
        catalog: CatalogInfo {
            source: catalog.source,
            fetched_unix: catalog.fetched_unix,
            skipped: catalog.skipped,
        },
        settings: settings_view(&st, &settings),
        credit: st.credit.lock().unwrap().map(|(c, _)| c),
        sync,
    })
}

/// Fetch the catalog from upstream; keep the current list if that fails.
async fn fetch_catalog(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    let url = catalog::url();
    let result = match catalog::fetch(&url).await {
        Ok(text) => catalog::accept_online(&st.data_dir, &text, now_unix()),
        Err(e) => Err(e),
    };
    match result {
        Ok(c) => {
            eprintln!(
                "[sloptweak] catalog: {} models from {url} ({} skipped)",
                c.models.len(),
                c.skipped.len()
            );
            *st.catalog.lock().unwrap() = c;
            let _ = app.emit_to("main", "catalog-updated", ());
            Ok(())
        }
        Err(e) => {
            eprintln!("[sloptweak] catalog fetch failed, keeping local copy: {e}");
            Err(e)
        }
    }
}

#[tauri::command]
async fn refresh_catalog(app: AppHandle) -> Result<(), String> {
    fetch_catalog(&app)
        .await
        .map_err(|e| format!("Couldn't update the model list ({e}). Using the saved one."))
}

/// The model to launch: catalog entry plus the user's matching LoRAs.
fn launch_model(st: &AppState, model_id: &str, settings: &Settings) -> Result<Model, String> {
    let model = st
        .catalog
        .lock()
        .unwrap()
        .find(model_id)
        .cloned()
        .ok_or("That model isn't in the model list any more.")?;
    Ok(config::with_loras(&model, &settings.loras))
}

#[derive(Serialize)]
struct Estimate {
    gpu_name: Option<String>,
    hourly: Option<f64>,
    download_cost: f64,
    download_gb: f64,
    usable_offers: usize,
    credit: f64,
    gate: Gate,
}

/// Cheapest offer right now and whether the credit covers it. $0: offer
/// search and the credit call don't rent anything.
#[tauri::command]
async fn estimate(st: State<'_, AppState>, model_id: String) -> Result<Estimate, String> {
    let settings = st.settings();
    let model = launch_model(&st, &model_id, &settings)?;
    let (provider, _) = st.provider()?;
    let credit = provider.credit().await.map_err(|e| e.to_string())?;
    st.remember_credit(credit);
    let query = config::offer_query(&model, &settings);
    let found = provider
        .search_offers(&query)
        .await
        .map_err(|e| e.to_string())?;
    let ranked = offers::rank(
        &found,
        &query,
        &config::cost_inputs(&model, &settings),
        &offers::Tried::with_memory(MachineFile::new(&st.data_dir).load(), now_unix()),
    );
    let best = ranked.first();
    Ok(Estimate {
        gpu_name: best.map(|b| b.offer.gpu_name.clone()),
        hourly: best.map(|b| b.hourly),
        download_cost: best.map_or(0.0, |b| b.download_cost),
        download_gb: model.total_bytes() as f64 / 1e9,
        usable_offers: ranked.len(),
        credit,
        gate: cost::gate(
            credit,
            settings.min_credit,
            best.map(|b| b.hourly),
            best.map_or(0.0, |b| b.download_cost),
        ),
    })
}

#[tauri::command]
async fn start_session(st: State<'_, AppState>, model_id: String) -> Result<(), String> {
    let settings = st.settings();
    let model = launch_model(&st, &model_id, &settings)?;
    let m = st.manager()?;
    if m.is_busy() || m.state().is_active() {
        return Err("A session is already running.".into());
    }
    // Low-balance gate. The price isn't known until an offer is picked; the
    // home screen already warned about a short runway via `estimate`.
    let (provider, _) = st.provider()?;
    let credit = provider
        .credit()
        .await
        .map_err(|e| format!("Couldn't check your Vast credit: {e}"))?;
    st.remember_credit(credit);
    if let Gate::Refuse(msg) = cost::gate(credit, settings.min_credit, None, 0.0) {
        return Err(msg);
    }
    let civitai = st.secret(secrets::CIVITAI_TOKEN);
    m.start(model, settings, civitai)
}

#[tauri::command]
async fn stop_session(st: State<'_, AppState>) -> Result<(), String> {
    st.manager()?.request_stop();
    Ok(())
}

#[tauri::command]
async fn open_invoke(st: State<'_, AppState>) -> Result<(), String> {
    st.manager()?.open_invoke().await
}

#[tauri::command]
async fn dismiss(st: State<'_, AppState>) -> Result<(), String> {
    st.manager()?.dismiss();
    Ok(())
}

#[derive(Serialize)]
struct KeyCheck {
    credit: Option<f64>,
    username: Option<String>,
}

/// Check a key with its service, then store it. Nothing is stored if the
/// check fails.
#[tauri::command]
async fn set_secret(
    st: State<'_, AppState>,
    which: String,
    value: String,
) -> Result<KeyCheck, String> {
    let value = value.trim().to_string();
    match which.as_str() {
        "vast" => {
            if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("A Vast API key is 64 letters and numbers (0-9, a-f).".into());
            }
            let credit = match st.mode {
                // Mock mode: any well-formed key except all zeros.
                Mode::Mock if value.bytes().all(|b| b == b'0') => {
                    return Err("Vast didn't accept that key: the Vast API key was rejected".into())
                }
                Mode::Mock => mock_credit(),
                Mode::Vast => VastProvider::new(value.clone())
                    .credit()
                    .await
                    .map_err(|e| format!("Vast didn't accept that key: {e}"))?,
            };
            st.reset_manager()?;
            st.secrets
                .set(secrets::VAST_API_KEY, &value)
                .map_err(|e| e.to_string())?;
            st.remember_credit(credit);
            Ok(KeyCheck {
                credit: Some(credit),
                username: None,
            })
        }
        "civitai" => {
            if value.len() < 20
                || value.len() > 128
                || !value.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                return Err("That doesn't look like a CivitAI API key.".into());
            }
            let username = st.civitai.username(&value).await.map_err(|e| match e {
                CivitaiError::BadKey => "CivitAI didn't accept that key. Copy it again \
                                         from your CivitAI account settings."
                    .to_string(),
                e => e.to_string(),
            })?;
            st.secrets
                .set(secrets::CIVITAI_TOKEN, &value)
                .map_err(|e| e.to_string())?;
            Ok(KeyCheck {
                credit: None,
                username: Some(username),
            })
        }
        _ => Err("unknown key".into()),
    }
}

#[tauri::command]
async fn check_credit(st: State<'_, AppState>) -> Result<f64, String> {
    let (provider, _) = st.provider()?;
    let c = provider.credit().await.map_err(|e| e.to_string())?;
    st.remember_credit(c);
    Ok(c)
}

/// The settings form (LoRAs, model, and folder have their own commands).
#[derive(Deserialize)]
struct SettingsForm {
    max_dph: f64,
    idle_minutes: u32,
    max_session_minutes: u32,
    min_credit: f64,
}

fn update_settings(
    st: &AppState,
    f: impl FnOnce(&mut UserSettings) -> Result<(), String>,
) -> Result<SettingsView, String> {
    let _guard = st.settings_lock.lock().unwrap();
    let mut u = st.settings().user();
    f(&mut u)?;
    let saved = Settings::save_user(&st.config_dir, &u)?;
    Ok(settings_view(st, &saved))
}

#[tauri::command]
async fn save_settings(
    st: State<'_, AppState>,
    form: SettingsForm,
) -> Result<SettingsView, String> {
    update_settings(&st, |u| {
        u.max_dph = form.max_dph;
        u.idle_minutes = form.idle_minutes;
        u.max_session_minutes = form.max_session_minutes;
        u.min_credit = form.min_credit;
        Ok(())
    })
}

#[tauri::command]
async fn select_model(st: State<'_, AppState>, model_id: String) -> Result<(), String> {
    update_settings(&st, |u| {
        u.model_id = Some(model_id);
        Ok(())
    })
    .map(|_| ())
}

#[tauri::command]
async fn pick_output_folder(
    app: AppHandle,
    st: State<'_, AppState>,
) -> Result<SettingsView, String> {
    let current = st.output_dir();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Where should SlopTweak save images?");
    if let Some(dir) = [Some(current.as_path()), current.parent()]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
    {
        dialog = dialog.set_directory(dir);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog.pick_folder(move |p| {
        let _ = tx.send(p);
    });
    let picked = rx.await.map_err(|e| e.to_string())?;
    let Some(path) = picked.and_then(|p| p.into_path().ok()) else {
        return Ok(settings_view(&st, &st.settings()));
    };
    update_settings(&st, |u| {
        u.output_dir = Some(path.to_string_lossy().into_owned());
        Ok(())
    })
}

#[tauri::command]
async fn reset_output_folder(st: State<'_, AppState>) -> Result<SettingsView, String> {
    update_settings(&st, |u| {
        u.output_dir = None;
        Ok(())
    })
}

/// Show the output folder in Explorer (created first, so it always opens).
#[tauri::command]
async fn open_output_folder(app: AppHandle, st: State<'_, AppState>) -> Result<(), String> {
    let dir = st.output_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Show the Invoke-window tutorial again (in the open window, or by opening it).
#[tauri::command]
async fn show_tutorial(st: State<'_, AppState>) -> Result<(), String> {
    if st.ui.remote.show_tutorial() {
        return Ok(());
    }
    st.manager()?.open_invoke().await
}

#[tauri::command]
async fn add_lora(st: State<'_, AppState>, link: String) -> Result<SettingsView, String> {
    let key = st.secret(secrets::CIVITAI_TOKEN);
    let lora = civitai::resolve_lora(st.civitai.as_ref(), &link, key.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    update_settings(&st, |u| {
        if u.loras.iter().any(|l| l.version_id == lora.version_id) {
            return Err(format!("{} is already in your list.", lora.name));
        }
        if u.loras.len() >= 20 {
            return Err("That's a lot of LoRAs; remove some first.".into());
        }
        u.loras.push(lora);
        Ok(())
    })
}

#[tauri::command]
async fn remove_lora(st: State<'_, AppState>, version_id: u64) -> Result<SettingsView, String> {
    update_settings(&st, |u| {
        u.loras.retain(|l| l.version_id != version_id);
        Ok(())
    })
}

#[tauri::command]
async fn set_lora_enabled(
    st: State<'_, AppState>,
    version_id: u64,
    enabled: bool,
) -> Result<SettingsView, String> {
    update_settings(&st, |u| {
        for l in u.loras.iter_mut().filter(|l| l.version_id == version_id) {
            l.enabled = enabled;
        }
        Ok(())
    })
}

/// Fixed pages the UI may open in the user's browser. The UI sends an id,
/// never a URL.
fn fixed_link(id: &str) -> Option<&'static str> {
    Some(match id {
        "vast_signup" => "https://cloud.vast.ai/",
        "vast_billing" => "https://cloud.vast.ai/billing/",
        "vast_keys" => "https://cloud.vast.ai/manage-keys/",
        "vast_terms" => "https://vast.ai/terms",
        "civitai_signup" => "https://civitai.com/login",
        "civitai_keys" => "https://civitai.com/user/account",
        "civitai_terms" => "https://civitai.com/content/tos",
        _ => return None,
    })
}

#[tauri::command]
async fn open_link(app: AppHandle, st: State<'_, AppState>, id: String) -> Result<(), String> {
    let url = match fixed_link(&id) {
        Some(u) => u.to_string(),
        None => {
            // A saved LoRA's CivitAI page, built from its ids.
            let vid: u64 = id
                .strip_prefix("lora:")
                .and_then(|v| v.parse().ok())
                .ok_or("unknown link")?;
            st.settings()
                .loras
                .iter()
                .find(|l| l.version_id == vid)
                .map(Lora::page)
                .ok_or("unknown link")?
        }
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn scan_orphans(st: State<'_, AppState>) -> Result<Vec<Orphan>, String> {
    st.manager()?
        .scan_orphans()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn destroy_orphan(st: State<'_, AppState>, instance_id: u64) -> Result<(), String> {
    st.manager()?.destroy_orphan(instance_id)
}

#[tauri::command]
async fn reattach_orphan(st: State<'_, AppState>, instance_id: u64) -> Result<(), String> {
    st.manager()?.reattach(instance_id).await
}

/// The user confirmed closing while a GPU runs: destroy it, then exit.
#[tauri::command]
async fn confirm_close(app: AppHandle, st: State<'_, AppState>) -> Result<(), String> {
    let m = st.manager()?;
    // The last image sync, then destroy + confirm; both are bounded.
    if !m.stop_and_wait(m.timing().stop_budget()).await {
        return Err(
            "Couldn't confirm the GPU was shut down. Not closing, so you can try again.".into(),
        );
    }
    st.ui.remote.close();
    app.exit(0);
    Ok(())
}

fn session_active(app: &AppHandle) -> bool {
    let st = app.state::<AppState>();
    let m = st.manager.lock().unwrap().clone();
    m.is_some_and(|m| m.is_busy() || m.state().is_active())
}

/// While a GPU bills: push the cost bar to the UI and the Invoke window's
/// title, and refresh the credit every few minutes.
async fn cost_loop(app: AppHandle) {
    const CREDIT_EVERY_S: u64 = 180;
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let st = app.state::<AppState>();
        if st.cost_bar().is_none() {
            continue;
        }
        if st.credit_age().is_none_or(|a| a >= CREDIT_EVERY_S) {
            if let Ok((provider, _)) = st.provider() {
                match provider.credit().await {
                    Ok(c) => st.remember_credit(c),
                    Err(e) => eprintln!("[sloptweak] credit refresh failed: {e}"),
                }
            }
        }
        if let Some(bar) = st.cost_bar() {
            st.ui.remote.set_title(&bar.title());
            let _ = app.emit_to("main", "cost-bar", &bar);
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let mode = match std::env::var("SLOPTWEAK_PROVIDER").as_deref() {
                Ok("mock") => Mode::Mock,
                _ => Mode::Vast,
            };
            // Mock mode gets its own credential service and folders, so fake
            // keys and test settings never touch the real profile.
            let (service, sub) = match mode {
                Mode::Vast => (secrets::SERVICE, ""),
                Mode::Mock => (secrets::MOCK_SERVICE, "mock"),
            };
            let secrets: Arc<dyn SecretStore> = Arc::new(KeyringStore::new(service));
            #[cfg(debug_assertions)]
            if mode == Mode::Vast
                && std::env::var("SLOPTWEAK_DEV_IMPORT_KEYS").as_deref() == Ok("1")
            {
                let imported = secrets::dev_import_from_env(secrets.as_ref());
                eprintln!("[sloptweak] dev: imported {imported:?} into Credential Manager");
            }
            let civitai: Arc<dyn CivitaiApi> = match mode {
                Mode::Mock => Arc::new(MockCivitai),
                Mode::Vast => Arc::new(HttpCivitai::default()),
            };
            let handle = app.handle().clone();
            let on_tutorial = {
                let h = handle.clone();
                Arc::new(move |sig: TutorialSignal| {
                    eprintln!("[sloptweak] tutorial {sig:?}");
                    let st = h.state::<AppState>();
                    if let Err(e) = update_settings(&st, |u| {
                        u.tutorial_done = true;
                        Ok(())
                    }) {
                        eprintln!("[sloptweak] couldn't save the tutorial state: {e}");
                    }
                })
            };
            let ui = Arc::new(TauriUi {
                app: handle.clone(),
                remote: RemoteWindows::new(handle.clone(), on_tutorial),
            });
            let data_dir = app.path().app_local_data_dir()?.join(sub);
            let config_dir = app.path().app_config_dir()?.join(sub);
            #[cfg(debug_assertions)]
            if mode == Mode::Mock && std::env::var("SLOPTWEAK_DEV_RESET").as_deref() == Ok("1") {
                // Dev-only: a fresh mock profile for the UI check.
                for name in [secrets::VAST_API_KEY, secrets::CIVITAI_TOKEN] {
                    let _ = secrets.delete(name);
                }
                let _ = std::fs::remove_dir_all(&data_dir);
                let _ = std::fs::remove_dir_all(&config_dir);
                eprintln!("[sloptweak] dev: reset the mock profile");
            }
            // Mock images are 1-pixel fakes: keep them out of Pictures.
            let default_output_dir = match mode {
                Mode::Vast => app
                    .path()
                    .picture_dir()
                    .or_else(|_| app.path().home_dir())?
                    .join("SlopTweak"),
                Mode::Mock => data_dir.join("output"),
            };
            app.manage(AppState {
                mode,
                secrets,
                catalog: Mutex::new(catalog::load_local(&data_dir)),
                data_dir,
                config_dir,
                default_output_dir,
                ui: ui.clone(),
                manager: Mutex::new(None),
                civitai,
                credit: Mutex::new(None),
                settings_lock: Mutex::new(()),
            });
            eprintln!("[sloptweak] provider mode: {mode:?}");
            // Created here rather than from config so dev builds can attach a
            // debugger port to this window only.
            let main_cfg = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .cloned()
                .expect("main window config");
            #[allow(unused_mut)]
            let mut main = tauri::WebviewWindowBuilder::from_config(app.handle(), &main_cfg)?;
            #[cfg(debug_assertions)]
            if let Ok(port) = std::env::var("SLOPTWEAK_MAIN_DEBUG_PORT") {
                main = main.additional_browser_args(&format!(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                     --remote-debugging-port={}",
                    port.trim()
                ));
            }
            main.build()?;
            #[cfg(debug_assertions)]
            if let Some(u) = std::env::var("SLOPTWEAK_DEV_REMOTE_URL")
                .ok()
                .and_then(|u| Url::parse(&u).ok())
            {
                // Dev-only: open the remote window directly (local webview check).
                ui.open_remote(u);
            }
            // Fetch on launch; the cached or bundled list shows meanwhile.
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                let _ = fetch_catalog(&h).await;
            });
            tauri::async_runtime::spawn(cost_loop(handle));
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if session_active(app) {
                    api.prevent_close();
                    let _ = app.emit_to("main", "close-requested", ());
                } else {
                    app.state::<AppState>().ui.remote.close();
                    app.exit(0);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            refresh_catalog,
            estimate,
            start_session,
            stop_session,
            open_invoke,
            dismiss,
            set_secret,
            check_credit,
            save_settings,
            select_model,
            pick_output_folder,
            reset_output_folder,
            open_output_folder,
            show_tutorial,
            add_lora,
            remove_lora,
            set_lora_enabled,
            open_link,
            scan_orphans,
            destroy_orphan,
            reattach_orphan,
            confirm_close,
        ])
        // A hard exit skips cleanup by design: the instance watchdog destroys
        // the GPU once heartbeats stop, and the next launch finds the record.
        .run(tauri::generate_context!())
        .expect("error while running SlopTweak");
}

#[cfg(test)]
mod tests {
    use super::fixed_link;

    #[test]
    fn links_are_fixed_https_pages() {
        for id in [
            "vast_signup",
            "vast_billing",
            "vast_keys",
            "vast_terms",
            "civitai_signup",
            "civitai_keys",
            "civitai_terms",
        ] {
            assert!(fixed_link(id).unwrap().starts_with("https://"), "{id}");
        }
        assert_eq!(fixed_link("https://evil.example/"), None);
        assert_eq!(fixed_link("file:///C:/Windows"), None);
    }
}
