//! SlopTweak launcher: Tauri shell, commands, and wiring.

mod assets;
mod catalog;
mod civitai;
mod comfy;
mod config;
mod cost;
mod diagnostics;
mod identity;
mod library;
mod persist;
mod provider;
mod redact;
mod remote;
mod secrets;
mod session;
mod sidecar;
mod sync;
mod updater;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::{
    DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
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
use crate::remote::{RemoteApp, RemoteWindows, TutorialSignal, TutorialStart};
use crate::secrets::{KeyringStore, SecretStore};
use crate::session::{Deps, Orphan, SessionManager, SessionState, Timing, Ui};
use crate::sidecar::HttpSidecar;
use crate::sync::{SyncReport, SyncStore};
use crate::updater::UpdateInfo;

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

    fn open_remote(&self, url: Url, app: RemoteApp) {
        let st = self.app.state::<AppState>();
        let tutorial = tutorial_start(&st.settings(), &st.catalog.lock().unwrap());
        if let Err(e) = self.remote.open(url, app, &tutorial) {
            eprintln!("[sloptweak] couldn't open the remote window: {e}");
        }
    }

    fn close_remote(&self) {
        self.remote.close();
    }

    fn sync_changed(&self, report: &SyncReport) {
        let _ = self.app.emit_to("main", "sync-status", report);
    }
}

/// An update found by `check_update`, waiting for the user to install it.
enum PendingUpdate {
    Real(Box<tauri_plugin_updater::Update>),
    /// Mock mode (`SLOPTWEAK_MOCK_UPDATE`): nothing is downloaded or run.
    Mock,
}

/// Provider plus the matching sidecar client.
type Backends = (Arc<dyn GpuProvider>, Arc<dyn sidecar::SidecarApi>);

struct AppState {
    identity: Arc<identity::IdentityService>,
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
    update: Mutex<Option<(UpdateInfo, PendingUpdate)>>,
    /// True while an update downloads and installs. Starting (or
    /// reattaching) a session and installing exclude each other through this
    /// lock: the installer exits the app, skipping the last sync and destroy.
    installing: tokio::sync::Mutex<bool>,
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
            library: library::LibraryStore::new(&self.config_dir),
            builtins: {
                let app = self.ui.app.clone();
                Arc::new(move |model_id: &str| {
                    app.state::<AppState>()
                        .catalog
                        .lock()
                        .unwrap()
                        .find(model_id)
                        .map(library::builtins_of)
                        .unwrap_or_default()
                })
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
    min_ram_gb: Option<f64>,
    price_tier: Option<u8>,
    good_for: String,
    /// `invoke` or `comfyui` (Identity Edit).
    backend: catalog::Backend,
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
            min_ram_gb: m.min_ram_gb,
            price_tier: m.price_tier,
            good_for: m.good_for.clone(),
            backend: m.backend,
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
    version: String,
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
    /// Which app the running GPU has (the window shows Invoke's own window
    /// or the Identity Edit panel); `None` when nothing is running.
    active_backend: Option<catalog::Backend>,
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
async fn get_snapshot(app: AppHandle, st: State<'_, AppState>) -> Result<Snapshot, String> {
    let (state, log, sync, active_backend) = match st.manager() {
        Ok(m) => (
            m.state(),
            m.log_lines(),
            m.sync_report(),
            m.active_backend(),
        ),
        Err(_) => (SessionState::idle(), vec![], None, None),
    };
    let catalog = st.catalog.lock().unwrap().clone();
    let settings = st.settings();
    Ok(Snapshot {
        mode: st.mode,
        version: app.package_info().version.to_string(),
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
        active_backend,
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
    // An instance that can't fetch the asset bundle never starts and costs
    // ~10 minutes per attempt, so check it before renting (Vast mode only).
    if matches!(st.mode, Mode::Vast) {
        assets::check(&config::assets_url(), config::ASSETS_SHA256).await?;
    }
    let civitai = st.secret(secrets::CIVITAI_TOKEN);
    let installing = st.installing.lock().await;
    if *installing {
        return Err(UPDATING.into());
    }
    st.identity.reset_run();
    m.start(model, settings, civitai)
}

const UPDATING: &str = "SlopTweak is installing an update and will restart.";

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
async fn open_comfyui(st: State<'_, AppState>) -> Result<(), String> {
    st.manager()?.open_comfyui().await
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

// ----- Identity Edit -------------------------------------------------------------

#[tauri::command]
async fn identity_state(st: State<'_, AppState>) -> Result<identity::IdentityView, String> {
    Ok(st.identity.view())
}

/// Ask for an image file and put it in a reference slot.
#[tauri::command]
async fn identity_pick_ref(
    app: AppHandle,
    st: State<'_, AppState>,
    slot: usize,
) -> Result<(), String> {
    // Debug builds only: the UI check can't drive a native file dialog, so
    // `SLOPTWEAK_DEV_PICK_REF=a.png;b.png` answers it (slot 0 gets the first).
    #[cfg(debug_assertions)]
    if let Ok(list) = std::env::var("SLOPTWEAK_DEV_PICK_REF") {
        if let Some(path) = list.split(';').nth(slot) {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let name = std::path::Path::new(path)
                .file_name()
                .map_or_else(|| "image".into(), |n| n.to_string_lossy().into_owned());
            return st.identity.set_ref(slot, &name, bytes);
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(if slot == identity::BASE_SLOT {
            "Choose the picture to edit"
        } else {
            "Choose a character sheet"
        })
        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
        .pick_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(path) = rx
        .await
        .map_err(|e| e.to_string())?
        .and_then(|p| p.into_path().ok())
    else {
        return Ok(());
    };
    set_ref_from_path(&st, slot, &path)
}

/// Read an image file into a reference slot (size checked before reading).
fn set_ref_from_path(st: &AppState, slot: usize, path: &std::path::Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Couldn't open that file: {e}"))?;
    if !meta.is_file() {
        return Err("That isn't a file.".into());
    }
    if meta.len() > identity::MAX_REF_BYTES as u64 {
        return Err("That image is too big (the limit is 15 MB).".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read that file: {e}"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    st.identity.set_ref(slot, &name, bytes)
}

/// A file dropped on a slot (the window reports its path).
#[tauri::command]
async fn identity_set_ref_path(
    st: State<'_, AppState>,
    slot: usize,
    path: String,
) -> Result<(), String> {
    set_ref_from_path(&st, slot, std::path::Path::new(&path))
}

/// An image pasted from the clipboard (base64 of the file bytes).
#[tauri::command]
async fn identity_set_ref_data(
    st: State<'_, AppState>,
    slot: usize,
    name: String,
    data: String,
) -> Result<(), String> {
    use base64::Engine;
    if data.len() > identity::MAX_REF_BYTES / 3 * 4 + 8 {
        return Err("That image is too big (the limit is 15 MB).".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| "Couldn't read that image.".to_string())?;
    st.identity.set_ref(slot, &name, bytes)
}

/// Copy a finished image into the Downloads folder; returns where it went.
#[tauri::command]
async fn identity_save_result(
    app: AppHandle,
    st: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let dir = app
        .path()
        .download_dir()
        .map_err(|_| "Couldn't find your Downloads folder.".to_string())?;
    let path = st.identity.save_result(&id, &dir)?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
async fn identity_clear_ref(st: State<'_, AppState>, slot: usize) -> Result<(), String> {
    st.identity.clear_ref(slot);
    Ok(())
}

#[tauri::command]
async fn identity_use_result(
    st: State<'_, AppState>,
    id: String,
    slot: usize,
) -> Result<(), String> {
    st.identity.use_result(&id, slot)
}

/// Where the running Identity Edit GPU is, and which files to load on it.
fn identity_target(st: &AppState) -> Result<identity::Target, String> {
    let (sidecar, base, secret, model_id) = st.manager()?.comfy_endpoint()?;
    let model = st
        .catalog
        .lock()
        .unwrap()
        .find(&model_id)
        .cloned()
        .ok_or("This GPU's model isn't in the model list any more.")?;
    let files = identity::ModelFiles::of(&model).ok_or("That model can't do Identity Edit.")?;
    Ok(identity::Target {
        sidecar,
        base,
        secret,
        files,
    })
}

/// The seed in a dev seed file: one number, read on every image so the
/// harness can change it between images. Anything else (missing file, text,
/// more than 52 bits) means "pick a random seed as usual".
#[cfg(debug_assertions)]
fn dev_seed(path: &std::path::Path) -> Option<u64> {
    let seed = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    (seed < 1 << 52).then_some(seed)
}

/// Start one image. Returns at once; progress and the result arrive through
/// `identity-changed` and `identity_state`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn identity_generate(
    st: State<'_, AppState>,
    prompt: String,
    aspect: identity::Aspect,
    mode: Option<identity::Mode>,
    strength: Option<u32>,
    pull: Option<f64>,
    mask: Option<identity::MaskInput>,
    focus: Option<Vec<identity::FocusInput>>,
) -> Result<(), String> {
    #[cfg(debug_assertions)]
    eprintln!(
        "[dev-focus] called: mode={mode:?} focus={}",
        focus.as_ref().map_or(0, Vec::len)
    );
    let target = identity_target(&st).inspect_err(|e| {
        #[cfg(debug_assertions)]
        eprintln!("[dev-focus] refused: {e}");
    })?;
    let opts = identity::EditOpts {
        strength,
        pull,
        mask,
        focus: focus.unwrap_or_default(),
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 & ((1 << 52) - 1))
        .unwrap_or(1);
    // Debug builds only: A/B runs need the same seed for both arms of a pair.
    #[cfg(debug_assertions)]
    let seed = std::env::var("SLOPTWEAK_DEV_SEED_FILE")
        .ok()
        .and_then(|p| dev_seed(std::path::Path::new(&p)))
        .unwrap_or(seed);
    // Debug builds only: what the A/B harness reads from the app's log (it never
    // prints this, so the person rating the images can't tell the arms apart).
    #[cfg(debug_assertions)]
    eprintln!(
        "[dev-focus] {}",
        serde_json::json!({
            "seed": seed,
            "focus": opts.focus.iter().map(|f| serde_json::json!({
                "slot": f.slot,
                "bbox": [f.bbox.x, f.bbox.y, f.bbox.w, f.bbox.h],
            })).collect::<Vec<_>>(),
        })
    );
    let svc = st.identity.clone();
    // Checked here so a bad request comes back as an error; the run itself
    // continues in the background.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tauri::async_runtime::spawn(async move {
        let r = svc
            .generate_with(
                &target,
                &prompt,
                mode.unwrap_or_default(),
                aspect,
                seed,
                &opts,
            )
            .await;
        let _ = tx.send(r);
    });
    // `generate` returns early with Err on a bad request, and otherwise only
    // when the image is done; give a bad request a moment to report itself.
    match tokio::time::timeout(Duration::from_millis(300), rx).await {
        Ok(Ok(Err(e))) => {
            #[cfg(debug_assertions)]
            eprintln!("[dev-focus] refused: {e}");
            Err(e)
        }
        _ => Ok(()),
    }
}

#[tauri::command]
async fn identity_cancel(st: State<'_, AppState>) -> Result<(), String> {
    let target = identity_target(&st)?;
    st.identity.cancel(&target).await;
    Ok(())
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
    output_folder_unlocked(&app)?;
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
async fn reset_output_folder(
    app: AppHandle,
    st: State<'_, AppState>,
) -> Result<SettingsView, String> {
    output_folder_unlocked(&app)?;
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

/// Where the tutorial overlay starts. First sessions get it on their own
/// until it's finished or skipped; it resumes at the saved stage.
fn tutorial_start(settings: &Settings, catalog: &Catalog) -> TutorialStart {
    let model = settings.model_id.as_deref().and_then(|id| catalog.find(id));
    TutorialStart {
        auto_show: !settings.tutorial_done,
        stage: settings.tutorial_stage,
        model_id: model.map(|m| m.id.clone()).unwrap_or_default(),
        model_name: model.map(|m| m.name.clone()).unwrap_or_default(),
        has_model_page: model.and_then(Model::page_url).is_some(),
    }
}

/// Save what the tutorial overlay reported, or open the model page it asked
/// for. The page can fake these (remote.rs), so a model-page request opens
/// only the current model's validated catalog page, at most once per 5 s.
fn on_tutorial_signal(app: &AppHandle, sig: TutorialSignal, last_page: &Mutex<Option<Instant>>) {
    eprintln!("[sloptweak] tutorial {sig:?}");
    let st = app.state::<AppState>();
    let saved = match sig {
        TutorialSignal::ModelPage => {
            let mut last = last_page.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
                return;
            }
            *last = Some(Instant::now());
            let page = st.settings().model_id.and_then(|id| {
                st.catalog
                    .lock()
                    .unwrap()
                    .find(&id)
                    .and_then(Model::page_url)
            });
            if let Some(url) = page {
                if let Err(e) = app.opener().open_url(url.as_str(), None::<&str>) {
                    eprintln!("[sloptweak] couldn't open the model page: {e}");
                }
            }
            return;
        }
        // A fixed Invoke docs page (remote::invoke_docs); same rate limit
        // window, shorter, so a page can't open a tab per signal in a loop.
        TutorialSignal::Docs(url) => {
            let mut last = last_page.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                return;
            }
            *last = Some(Instant::now());
            if let Err(e) = app.opener().open_url(url, None::<&str>) {
                eprintln!("[sloptweak] couldn't open the docs: {e}");
            }
            return;
        }
        TutorialSignal::Stage(n) => update_settings(&st, |u| {
            u.tutorial_stage = n;
            Ok(())
        }),
        // Finished: next time "Show tutorial" starts from the top.
        TutorialSignal::Done => update_settings(&st, |u| {
            u.tutorial_done = true;
            u.tutorial_stage = 1;
            Ok(())
        }),
        // Skipped (or closed): stop showing it by itself, keep the stage.
        TutorialSignal::Skipped => update_settings(&st, |u| {
            u.tutorial_done = true;
            Ok(())
        }),
    };
    if let Err(e) = saved {
        eprintln!("[sloptweak] couldn't save the tutorial state: {e}");
    }
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
        "release_notes" => updater::RELEASES_PAGE,
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
    let installing = st.installing.lock().await;
    if *installing {
        return Err(UPDATING.into());
    }
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

#[derive(Serialize)]
struct Diagnostics {
    /// False if the clipboard was busy; the UI then lets the user copy `text`.
    copied: bool,
    text: String,
}

/// A redacted report for bug reports, copied to the clipboard.
#[tauri::command]
async fn copy_diagnostics(app: AppHandle, st: State<'_, AppState>) -> Result<Diagnostics, String> {
    let settings = st.settings();
    let manager = st.manager.lock().unwrap().clone();
    let (state, history, log, sync, launch) = match &manager {
        Some(m) => (
            m.state(),
            m.history(),
            m.log_lines(),
            m.sync_report(),
            m.active_secret(),
        ),
        None => (SessionState::idle(), vec![], vec![], None, None),
    };
    let record = RecordFile::new(&st.data_dir).load();
    // Every secret this app holds, masked by value (patterns catch the rest).
    let mut known: Vec<String> = [secrets::VAST_API_KEY, secrets::CIVITAI_TOKEN]
        .into_iter()
        .filter_map(|n| st.secret(n))
        .collect();
    known.extend(launch);
    if let Some(r) = &record {
        known.extend(st.secret(&secrets::launch_secret_name(r.instance_id)));
    }
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let catalog = {
        let c = st.catalog.lock().unwrap();
        format!(
            "{:?}, {} models, fetched {:?}, skipped {:?}",
            c.source,
            c.models.len(),
            c.fetched_unix,
            c.skipped
        )
    };
    let update = st
        .update
        .lock()
        .unwrap()
        .as_ref()
        .map(|(i, _)| format!("{} available", i.version));
    let version = app.package_info().version.to_string();
    let home = app.path().home_dir().ok();
    let text = diagnostics::report(
        &diagnostics::Inputs {
            app_version: &version,
            mode: match st.mode {
                Mode::Vast => "vast",
                Mode::Mock => "mock",
            },
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            webview: tauri::webview_version().ok(),
            generated_unix: now_unix(),
            state: &state,
            history: &history,
            log: &log,
            settings: &settings,
            catalog,
            sync: sync.as_ref(),
            has_vast_key: st.secret(secrets::VAST_API_KEY).is_some(),
            has_civitai_key: st.secret(secrets::CIVITAI_TOKEN).is_some(),
            record: record.as_ref(),
            credit: st.credit.lock().unwrap().map(|(c, _)| c),
            update,
            image: config::IMAGE,
            assets_url: &config::assets_url(),
            assets_sha256: config::ASSETS_SHA256,
        },
        &known,
        home.as_deref().and_then(|h| h.to_str()),
    );
    let copied = match app.clipboard().write_text(text.as_str()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[sloptweak] clipboard: {e}");
            false
        }
    };
    Ok(Diagnostics { copied, text })
}

/// Look for a newer release. `None` when up to date.
async fn find_update(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let st = app.state::<AppState>();
    let current = app.package_info().version.to_string();
    let found = match st.mode {
        // Dev: `SLOPTWEAK_MOCK_UPDATE=<version>` pretends a release exists.
        Mode::Mock => std::env::var("SLOPTWEAK_MOCK_UPDATE")
            .ok()
            .filter(|v| cfg!(debug_assertions) && !v.is_empty())
            .map(|v| {
                (
                    UpdateInfo {
                        version: v,
                        current: current.clone(),
                        notes: updater::short_notes(Some("Mock release notes.")),
                    },
                    PendingUpdate::Mock,
                )
            }),
        Mode::Vast => app
            .updater()
            .map_err(|e| e.to_string())?
            .check()
            .await
            .map_err(|e| e.to_string())?
            .map(|u| {
                (
                    UpdateInfo {
                        version: u.version.clone(),
                        current: current.clone(),
                        notes: updater::short_notes(u.body.as_deref()),
                    },
                    PendingUpdate::Real(Box::new(u)),
                )
            }),
    };
    let info = found.as_ref().map(|(i, _)| i.clone());
    *st.update.lock().unwrap() = found;
    Ok(info)
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    find_update(&app)
        .await
        .map_err(|e| format!("Couldn't check for updates ({e})."))
}

/// Download, verify, and run the installer. On Windows the installer
/// replaces the app and restarts it, so a success doesn't return.
#[tauri::command]
async fn install_update(app: AppHandle, st: State<'_, AppState>) -> Result<(), String> {
    {
        let mut installing = st.installing.lock().await;
        if *installing {
            return Err("The update is already being installed.".into());
        }
        updater::may_install(session_active(&app))?;
        *installing = true;
    }
    let result = install_pending(&app, &st).await;
    // Only reached if nothing was installed (mock mode or an error).
    *st.installing.lock().await = false;
    result
}

async fn install_pending(app: &AppHandle, st: &AppState) -> Result<(), String> {
    let Some((info, pending)) = st.update.lock().unwrap().take() else {
        return Err("No update is waiting. Check for updates first.".into());
    };
    eprintln!("[sloptweak] installing update {}", info.version);
    let update = match pending {
        PendingUpdate::Mock => {
            eprintln!("[sloptweak] mock: would install {}", info.version);
            let _ = app.emit_to("main", "update-progress", 100.0);
            return Ok(());
        }
        PendingUpdate::Real(u) => u,
    };
    let mut received = 0usize;
    let progress_app = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                received += chunk;
                if let Some(t) = total.filter(|t| *t > 0) {
                    let pct = (received as f64 / t as f64 * 100.0).min(100.0);
                    let _ = progress_app.emit_to("main", "update-progress", pct);
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("The update didn't download ({e}). SlopTweak wasn't changed."))?;
    // `installing` has kept sessions from starting since the check above;
    // check again right before the installer takes over, to be sure.
    updater::may_install(session_active(app))?;
    // The installer closes this process: close the Invoke window first.
    st.ui.remote.close();
    update
        .install(bytes)
        .map_err(|e| format!("The update didn't install ({e}). SlopTweak wasn't changed."))?;
    app.exit(0);
    Ok(())
}

fn session_active(app: &AppHandle) -> bool {
    let st = app.state::<AppState>();
    let m = st.manager.lock().unwrap().clone();
    m.is_some_and(|m| m.is_busy() || m.state().is_active())
}

/// The output folder is fixed while a GPU runs: sync skips images it already
/// saved, so after a change the earlier ones would stay in the old folder and
/// look missing.
fn output_folder_unlocked(app: &AppHandle) -> Result<(), String> {
    if session_active(app) {
        return Err(
            "Stop the GPU to change the folder. Images from this session are already \
             going to the current one."
                .into(),
        );
    }
    Ok(())
}

/// Closing the Invoke window while a GPU is rented: ask whether to stop
/// renting it too. Yes is the default button. Either answer closes the
/// window; "Open Invoke" in the main window brings it back. The remote page
/// can't show app UI, so this is a native dialog. Dismissing the dialog
/// (Esc, its X) cancels: the Invoke window stays open.
fn ask_stop_on_invoke_close(window: &tauri::Window) {
    static ASKING: AtomicBool = AtomicBool::new(false);
    if ASKING.swap(true, Ordering::SeqCst) {
        return; // already asking (the X was clicked again)
    }
    let app = window.app_handle().clone();
    window
        .dialog()
        .message(
            "Do you want to stop renting the GPU too?\n\n\
             Yes: stop it now. Your images are saved to your PC first.\n\
             No: keep it running. You can reopen the window from SlopTweak, \
             and you're still paying while it runs.",
        )
        .title("Stop renting the GPU?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::YesNo)
        .parent(window)
        .show_with_result(move |answer| {
            let st = app.state::<AppState>();
            match answer {
                MessageDialogResult::Yes => {
                    st.ui.remote.close();
                    if let Ok(m) = st.manager() {
                        m.request_stop();
                    }
                }
                MessageDialogResult::No => st.ui.remote.close(),
                // Esc or the dialog's own X: changed their mind; Invoke stays open.
                _ => {}
            }
            ASKING.store(false, Ordering::SeqCst);
        });
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
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // Mock mode (and every SLOPTWEAK_MOCK_* hook, read only in it) is
            // dev-only: a release build always talks to Vast.
            let mode = match std::env::var("SLOPTWEAK_PROVIDER").as_deref() {
                Ok("mock") if cfg!(debug_assertions) => Mode::Mock,
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
                let last_page = Mutex::new(None);
                Arc::new(move |sig: TutorialSignal| on_tutorial_signal(&h, sig, &last_page))
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
                update: Mutex::new(None),
                installing: tokio::sync::Mutex::new(false),
                identity: Arc::new(identity::IdentityService::new({
                    let h = handle.clone();
                    Arc::new(move || {
                        let _ = h.emit_to("main", "identity-changed", ());
                    })
                })),
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
                ui.open_remote(u, RemoteApp::Invoke);
            }
            // Fetch on launch; the cached or bundled list shows meanwhile.
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                let _ = fetch_catalog(&h).await;
            });
            // Check for a new version shortly after launch; the home screen
            // offers it. Failures (offline, no release yet) stay quiet.
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                match find_update(&h).await {
                    Ok(Some(info)) => {
                        eprintln!("[sloptweak] update available: {}", info.version);
                        let _ = h.emit_to("main", "update-available", &info);
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[sloptweak] update check failed: {e}"),
                }
            });
            tauri::async_runtime::spawn(cost_loop(handle));
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label().starts_with(remote::LABEL_PREFIX) {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    if session_active(window.app_handle()) {
                        api.prevent_close();
                        ask_stop_on_invoke_close(window);
                    }
                }
                return;
            }
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
            identity_state,
            identity_pick_ref,
            identity_clear_ref,
            identity_set_ref_path,
            identity_set_ref_data,
            identity_save_result,
            identity_use_result,
            identity_generate,
            identity_cancel,
            get_snapshot,
            refresh_catalog,
            estimate,
            start_session,
            stop_session,
            open_invoke,
            open_comfyui,
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
            copy_diagnostics,
            check_update,
            install_update,
        ])
        // A hard exit skips cleanup by design: the instance watchdog destroys
        // the GPU once heartbeats stop, and the next launch finds the record.
        .run(tauri::generate_context!())
        .expect("error while running SlopTweak");
}

#[cfg(test)]
mod tests {
    use super::{dev_seed, fixed_link, tutorial_start, Catalog, Settings};

    #[test]
    fn the_dev_seed_file_gives_a_seed_or_nothing() {
        let dir = std::env::temp_dir().join(format!("sloptweak-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("seed.txt");
        assert_eq!(dev_seed(&file), None); // no file
        for (text, want) in [
            ("123456\n", Some(123_456)),
            (" 7 ", Some(7)),
            ("abc", None),
            ("", None),
            ("-5", None),
            ("4503599627370496", None), // 2^52: too big for the sampler's seed
            ("4503599627370495", Some((1 << 52) - 1)),
        ] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(dev_seed(&file), want, "{text:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
            "release_notes",
        ] {
            assert!(fixed_link(id).unwrap().starts_with("https://"), "{id}");
        }
        assert_eq!(fixed_link("https://evil.example/"), None);
        assert_eq!(fixed_link("file:///C:/Windows"), None);
    }

    #[test]
    fn tutorial_starts_from_saved_state_and_model() {
        let catalog = Catalog {
            models: crate::catalog::bundled(),
            source: crate::catalog::Source::Bundled,
            fetched_unix: None,
            skipped: Vec::new(),
        };
        let mut s = Settings::default();
        let t = tutorial_start(&s, &catalog);
        assert_eq!((t.auto_show, t.stage), (true, 1));
        assert!(t.model_id.is_empty() && !t.has_model_page);

        s.model_id = Some("banana-splitz-xxl".into());
        s.tutorial_done = true;
        s.tutorial_stage = 4;
        let t = tutorial_start(&s, &catalog);
        assert_eq!((t.auto_show, t.stage), (false, 4));
        assert_eq!(t.model_name, "Banana Splitz XXL");
        assert!(t.has_model_page);

        s.model_id = Some("gone".into());
        assert!(!tutorial_start(&s, &catalog).has_model_page);
    }
}
