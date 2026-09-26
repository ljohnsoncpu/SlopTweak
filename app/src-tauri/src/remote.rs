//! The window that shows Invoke from the instance tunnel.
//!
//! Security (CLAUDE.md invariants, findings §6):
//! * Its label never appears in a capability, so the ACL denies every IPC
//!   call from it (`__TAURI_INTERNALS__` is still injected, but inert).
//! * It uses its own WebView2 data directory, so its cookies and storage
//!   are separate from the local UI.
//! * Navigation is pinned to the tunnel origin, and popups are denied.
//!
//! The tutorial (tutorial.js) is injected as an initialization script: plain
//! page JS, no IPC. Its only way back is a navigation to
//! `/__sloptweak/tutorial/<done|skipped|stage/N|model-page>`, which is
//! intercepted here and blocked. A page could fake those, but all they do is
//! mark the tutorial as seen, remember a stage number, or open the current
//! model's catalog page (rate-limited, see lib.rs).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use url::{Origin, Url};

use crate::config::TUTORIAL_STAGES;

pub const LABEL_PREFIX: &str = "remote-";

const TUTORIAL_JS: &str = include_str!("tutorial.js");
/// The fallback portrait for stage 3 ("Use ours instead"), user-supplied.
const TUTORIAL_PORTRAIT: &[u8] = include_bytes!("../assets/tutorial-portrait.webp");

/// What the tutorial overlay reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TutorialSignal {
    Done,
    Skipped,
    /// The user moved to this stage (1..=TUTORIAL_STAGES).
    Stage(u8),
    /// Open the current model's page in the user's browser.
    ModelPage,
}

/// What the overlay starts from: the app's saved tutorial state and the
/// session's model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TutorialStart {
    /// Show it without being asked (not finished or skipped yet).
    pub auto_show: bool,
    /// Stage to resume at, 1..=TUTORIAL_STAGES.
    pub stage: u8,
    pub model_id: String,
    pub model_name: String,
    /// The app can open this model's page (ModelPage signal).
    pub has_model_page: bool,
}

pub type TutorialHandler = Arc<dyn Fn(TutorialSignal) + Send + Sync>;

pub struct RemoteWindows {
    app: AppHandle,
    current: Mutex<Option<(String, Origin)>>,
    counter: AtomicU32,
    on_tutorial: TutorialHandler,
    /// "Show tutorial" was pressed while no Invoke window was open.
    force_tutorial: AtomicBool,
}

/// Navigation rule for the remote window: same origin as the tunnel only.
pub fn allowed(origin: &Origin, url: &Url) -> bool {
    &url.origin() == origin
}

/// A tutorial signal navigation (same origin, exact path), if `url` is one.
pub fn tutorial_signal(origin: &Origin, url: &Url) -> Option<TutorialSignal> {
    if &url.origin() != origin {
        return None;
    }
    match url.path().strip_prefix("/__sloptweak/tutorial/")? {
        "done" => Some(TutorialSignal::Done),
        "skipped" => Some(TutorialSignal::Skipped),
        "model-page" => Some(TutorialSignal::ModelPage),
        rest => {
            let n = match rest.strip_prefix("stage/")?.as_bytes() {
                [c @ b'1'..=b'9'] => c - b'0',
                _ => return None,
            };
            (n <= TUTORIAL_STAGES).then_some(TutorialSignal::Stage(n))
        }
    }
}

/// The overlay with its config and the fallback portrait baked in.
pub fn tutorial_script(start: &TutorialStart, force: bool) -> String {
    let cfg = serde_json::json!({
        "autoShow": start.auto_show,
        "force": force,
        "stage": start.stage.clamp(1, TUTORIAL_STAGES),
        "stages": TUTORIAL_STAGES,
        "modelId": start.model_id,
        "modelName": start.model_name,
        "modelPage": start.has_model_page,
        "portrait": base64::engine::general_purpose::STANDARD.encode(TUTORIAL_PORTRAIT),
        "portraitName": "sloptweak-tutorial-portrait.webp",
        "portraitType": "image/webp",
    });
    format!("{}({cfg});", TUTORIAL_JS.trim_end())
}

impl RemoteWindows {
    pub fn new(app: AppHandle, on_tutorial: TutorialHandler) -> Self {
        Self {
            app,
            current: Mutex::new(None),
            counter: AtomicU32::new(0),
            on_tutorial,
            force_tutorial: AtomicBool::new(false),
        }
    }

    /// Show the tutorial: in the open Invoke window if there is one (returns
    /// true), otherwise the next time a window opens.
    pub fn show_tutorial(&self) -> bool {
        if let Some((label, _)) = self.current.lock().unwrap().as_ref() {
            if let Some(w) = self.app.get_webview_window(label) {
                let shown = w
                    .eval("window.__slopTweakTutorial && window.__slopTweakTutorial.open()")
                    .is_ok();
                if shown {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                    return true;
                }
            }
        }
        self.force_tutorial.store(true, Ordering::Relaxed);
        false
    }

    /// Open (or re-point) the Invoke window, with the tutorial overlay
    /// starting from `tutorial`.
    pub fn open(&self, url: Url, tutorial: &TutorialStart) -> Result<(), String> {
        let origin = url.origin();
        let mut current = self.current.lock().unwrap();
        if let Some((label, o)) = current.as_ref() {
            if let Some(w) = self.app.get_webview_window(label) {
                if *o == origin {
                    w.navigate(url).map_err(|e| e.to_string())?;
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                    return Ok(());
                }
                // A different instance: the pinned origin changes, so start fresh.
                let _ = w.destroy();
            }
        }
        let label = format!(
            "{LABEL_PREFIX}{}",
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let data_dir = self
            .app
            .path()
            .app_local_data_dir()
            .map_err(|e| e.to_string())?
            .join("remote-webview");
        let pinned = origin.clone();
        let on_tutorial = self.on_tutorial.clone();
        let force = self.force_tutorial.swap(false, Ordering::Relaxed);
        #[allow(unused_mut)]
        let mut builder = WebviewWindowBuilder::new(&self.app, &label, WebviewUrl::External(url))
            .title("SlopTweak — Invoke")
            .inner_size(1440.0, 920.0)
            .min_inner_size(900.0, 600.0)
            .data_directory(data_dir)
            .initialization_script(tutorial_script(tutorial, force))
            .on_navigation(move |u| {
                if let Some(sig) = tutorial_signal(&pinned, u) {
                    on_tutorial(sig);
                    return false;
                }
                let ok = allowed(&pinned, u);
                if !ok {
                    eprintln!(
                        "[sloptweak] blocked navigation to {}",
                        u.origin().ascii_serialization()
                    );
                }
                ok
            })
            .on_new_window(|u, _| {
                eprintln!(
                    "[sloptweak] blocked popup to {}",
                    u.origin().ascii_serialization()
                );
                NewWindowResponse::Deny
            });
        #[cfg(debug_assertions)]
        if let Ok(port) = std::env::var("SLOPTWEAK_REMOTE_DEBUG_PORT") {
            // Dev-only: lets a local CDP client inspect the remote window.
            builder = builder.additional_browser_args(&format!(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                 --remote-debugging-port={}",
                port.trim()
            ));
        }
        builder.build().map_err(|e| e.to_string())?;
        *current = Some((label, origin));
        Ok(())
    }

    /// The remote page can't show app UI, so the cost bar goes in its title.
    pub fn set_title(&self, title: &str) {
        if let Some((label, _)) = self.current.lock().unwrap().as_ref() {
            if let Some(w) = self.app.get_webview_window(label) {
                let _ = w.set_title(title);
            }
        }
    }

    pub fn close(&self) {
        if let Some((label, _)) = self.current.lock().unwrap().take() {
            if let Some(w) = self.app.get_webview_window(&label) {
                let _ = w.destroy();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_is_pinned_to_origin() {
        let base = Url::parse("https://a-b.trycloudflare.com/__auth?t=x").unwrap();
        let o = base.origin();
        for ok in [
            "https://a-b.trycloudflare.com/",
            "https://a-b.trycloudflare.com/api/v1/images/?x=1",
            "https://a-b.trycloudflare.com:443/x",
        ] {
            assert!(allowed(&o, &Url::parse(ok).unwrap()), "{ok}");
        }
        for bad in [
            "http://a-b.trycloudflare.com/",
            "https://evil.trycloudflare.com/",
            "https://a-b.trycloudflare.com.evil.com/",
            "https://a-b.trycloudflare.com:8443/",
            "tauri://localhost/",
            "http://ipc.localhost/",
            "file:///C:/Windows/win.ini",
            "about:blank",
        ] {
            assert!(!allowed(&o, &Url::parse(bad).unwrap()), "{bad}");
        }
    }

    #[test]
    fn tutorial_signals_are_exact() {
        let o = Url::parse("https://a-b.trycloudflare.com/")
            .unwrap()
            .origin();
        let sig = |u: &str| tutorial_signal(&o, &Url::parse(u).unwrap());
        assert_eq!(
            sig("https://a-b.trycloudflare.com/__sloptweak/tutorial/done"),
            Some(TutorialSignal::Done)
        );
        assert_eq!(
            sig("https://a-b.trycloudflare.com/__sloptweak/tutorial/skipped?x=1"),
            Some(TutorialSignal::Skipped)
        );
        assert_eq!(
            sig("https://a-b.trycloudflare.com/__sloptweak/tutorial/model-page"),
            Some(TutorialSignal::ModelPage)
        );
        for n in 1..=TUTORIAL_STAGES {
            let u = format!("https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/{n}");
            assert_eq!(sig(&u), Some(TutorialSignal::Stage(n)));
        }
        for no in [
            "https://evil.trycloudflare.com/__sloptweak/tutorial/done",
            "https://evil.trycloudflare.com/__sloptweak/tutorial/stage/2",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/done/x",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/0",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/6",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/12",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/+1",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/stage/1/x",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/model-page/x",
            "https://a-b.trycloudflare.com/x/__sloptweak/tutorial/done",
            "https://a-b.trycloudflare.com/",
        ] {
            assert_eq!(sig(no), None, "{no}");
        }
    }

    #[test]
    fn tutorial_script_is_one_call() {
        let start = TutorialStart {
            auto_show: true,
            stage: 3,
            model_id: "m".into(),
            model_name: "Test".into(),
            has_model_page: true,
        };
        let s = tutorial_script(&start, false);
        assert!(s.starts_with("//") && s.contains("(function (CFG)"));
        assert!(s.ends_with(");"));
        assert!(s.contains(r#""autoShow":true"#) && s.contains(r#""force":false"#));
        assert!(s.contains(r#""stage":3"#) && s.contains(r#""modelPage":true"#));
        assert!(s.contains(r#""portraitType":"image/webp""#));
        // "RIFF" in base64: the portrait is embedded as WebP.
        let b64 = base64::engine::general_purpose::STANDARD.encode(TUTORIAL_PORTRAIT);
        assert!(b64.starts_with("UklGR") && s.contains(&b64));
        // An out-of-range stage from a hand-edited file is clamped.
        let s = tutorial_script(&TutorialStart { stage: 0, ..start }, true);
        assert!(s.contains(r#""stage":1"#) && s.contains(r#""force":true"#));
    }

    #[test]
    fn tutorial_portrait_is_plain_webp() {
        // RIFF/WEBP with a single lossy VP8 chunk: no VP8X, so no ICC,
        // EXIF, or XMP chunks can ride along.
        let p = TUTORIAL_PORTRAIT;
        assert_eq!((&p[..4], &p[8..16]), (&b"RIFF"[..], &b"WEBPVP8 "[..]));
        let n = u32::from_le_bytes(p[16..20].try_into().unwrap()) as usize;
        assert_eq!(20 + n + (n & 1), p.len());
    }
}
