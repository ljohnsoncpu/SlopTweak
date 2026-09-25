//! SlopTweak launcher: Tauri shell, commands, and wiring.

mod catalog;
mod config;
mod persist;
mod provider;
mod redact;
mod remote;
mod secrets;
mod session;
mod sidecar;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use url::Url;

use crate::config::Settings;
use crate::persist::RecordFile;
use crate::provider::mock::{parse_script, MockProvider, MockTimings};
use crate::provider::vast::VastProvider;
use crate::provider::GpuProvider;
use crate::remote::RemoteWindows;
use crate::secrets::{KeyringStore, SecretStore};
use crate::session::{Deps, Orphan, SessionManager, SessionState, Timing, Ui};
use crate::sidecar::HttpSidecar;

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
        if let Err(e) = self.remote.open(url) {
            eprintln!("[sloptweak] couldn't open the Invoke window: {e}");
        }
    }

    fn close_remote(&self) {
        self.remote.close();
    }
}

/// Provider plus the matching sidecar client.
type Backends = (Arc<dyn GpuProvider>, Arc<dyn sidecar::SidecarApi>);

struct AppState {
    mode: Mode,
    secrets: Arc<dyn SecretStore>,
    data_dir: PathBuf,
    config_dir: PathBuf,
    ui: Arc<TauriUi>,
    manager: Mutex<Option<Arc<SessionManager>>>,
}

impl AppState {
    fn secret(&self, name: &str) -> Option<String> {
        self.secrets.get(name).ok().flatten()
    }

    fn provider(&self) -> Result<Backends, String> {
        match self.mode {
            Mode::Mock => {
                let script = std::env::var("SLOPTWEAK_MOCK_SCRIPT").unwrap_or_default();
                let mut mock = MockProvider::new(MockTimings::default(), parse_script(&script));
                if let Some(base) = std::env::var("SLOPTWEAK_MOCK_REMOTE")
                    .ok()
                    .and_then(|u| Url::parse(&u).ok())
                {
                    mock = mock.with_remote_base(base);
                }
                let sidecar = Arc::new(mock.sidecar());
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

#[derive(Serialize)]
struct ModelView {
    id: String,
    name: String,
    description: String,
    size_gb: f64,
    min_vram_gb: f64,
    needs_civitai: bool,
    nsfw: bool,
}

#[derive(Serialize)]
struct Snapshot {
    mode: Mode,
    state: SessionState,
    log: Vec<String>,
    has_vast_key: bool,
    has_civitai_key: bool,
    models: Vec<ModelView>,
    settings: Settings,
}

#[tauri::command]
async fn get_snapshot(st: State<'_, AppState>) -> Result<Snapshot, String> {
    let (state, log) = match st.manager() {
        Ok(m) => (m.state(), m.log_lines()),
        Err(_) => (SessionState::idle(), vec![]),
    };
    Ok(Snapshot {
        mode: st.mode,
        state,
        log,
        has_vast_key: st.mode == Mode::Mock || st.secret(secrets::VAST_API_KEY).is_some(),
        has_civitai_key: st.secret(secrets::CIVITAI_TOKEN).is_some(),
        models: catalog::bundled()
            .into_iter()
            .map(|m| ModelView {
                size_gb: m.total_bytes() as f64 / 1e9,
                needs_civitai: m.needs_civitai(),
                id: m.id,
                name: m.name,
                description: m.description,
                min_vram_gb: m.min_vram_gb,
                nsfw: m.nsfw,
            })
            .collect(),
        settings: Settings::load(&st.config_dir),
    })
}

#[tauri::command]
async fn start_session(st: State<'_, AppState>, model_id: String) -> Result<(), String> {
    let model = catalog::bundled()
        .into_iter()
        .find(|m| m.id == model_id)
        .ok_or("Unknown model.")?;
    let m = st.manager()?;
    let civitai = st.secret(secrets::CIVITAI_TOKEN);
    m.start(model, Settings::load(&st.config_dir), civitai)
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

/// Store a key after checking it. Returns the Vast credit when relevant.
#[tauri::command]
async fn set_secret(
    st: State<'_, AppState>,
    which: String,
    value: String,
) -> Result<Option<f64>, String> {
    let value = value.trim().to_string();
    match which.as_str() {
        "vast" => {
            if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("A Vast API key is 64 letters and numbers (0-9, a-f).".into());
            }
            let credit = VastProvider::new(value.clone())
                .credit()
                .await
                .map_err(|e| format!("Vast didn't accept that key: {e}"))?;
            st.reset_manager()?;
            st.secrets
                .set(secrets::VAST_API_KEY, &value)
                .map_err(|e| e.to_string())?;
            Ok(Some(credit))
        }
        "civitai" => {
            // Phase 3 validates against the CivitAI API.
            if value.len() < 20 || !value.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err("That doesn't look like a CivitAI API key.".into());
            }
            st.secrets
                .set(secrets::CIVITAI_TOKEN, &value)
                .map_err(|e| e.to_string())?;
            Ok(None)
        }
        _ => Err("unknown key".into()),
    }
}

#[tauri::command]
async fn check_credit(st: State<'_, AppState>) -> Result<f64, String> {
    let (provider, _) = st.provider()?;
    provider.credit().await.map_err(|e| e.to_string())
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
    if !m.stop_and_wait(Duration::from_secs(180)).await {
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

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let mode = match std::env::var("SLOPTWEAK_PROVIDER").as_deref() {
                Ok("mock") => Mode::Mock,
                _ => Mode::Vast,
            };
            let secrets: Arc<dyn SecretStore> = Arc::new(KeyringStore);
            #[cfg(debug_assertions)]
            if std::env::var("SLOPTWEAK_DEV_IMPORT_KEYS").as_deref() == Ok("1") {
                let imported = secrets::dev_import_from_env(secrets.as_ref());
                eprintln!("[sloptweak] dev: imported {imported:?} into Credential Manager");
            }
            let handle = app.handle().clone();
            let ui = Arc::new(TauriUi {
                app: handle.clone(),
                remote: RemoteWindows::new(handle.clone()),
            });
            app.manage(AppState {
                mode,
                secrets,
                data_dir: app.path().app_local_data_dir()?,
                config_dir: app.path().app_config_dir()?,
                ui: ui.clone(),
                manager: Mutex::new(None),
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
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection                      --remote-debugging-port={}",
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
            start_session,
            stop_session,
            open_invoke,
            dismiss,
            set_secret,
            check_credit,
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
