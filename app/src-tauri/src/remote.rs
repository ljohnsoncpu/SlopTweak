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
//! `/__sloptweak/tutorial/<done|skipped>`, which is intercepted here and
//! blocked. A page could fake that, but all it does is mark the tutorial as
//! seen.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use url::{Origin, Url};

pub const LABEL_PREFIX: &str = "remote-";

const TUTORIAL_JS: &str = include_str!("tutorial.js");
const TUTORIAL_SAMPLE: &[u8] = include_bytes!("../assets/tutorial-sample.jpg");

/// What the tutorial overlay reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TutorialSignal {
    Done,
    Skipped,
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
    match url.path() {
        "/__sloptweak/tutorial/done" => Some(TutorialSignal::Done),
        "/__sloptweak/tutorial/skipped" => Some(TutorialSignal::Skipped),
        _ => None,
    }
}

/// The overlay with its config and the sample picture baked in.
pub fn tutorial_script(auto_show: bool, force: bool) -> String {
    let cfg = serde_json::json!({
        "autoShow": auto_show,
        "force": force,
        "sample": base64::engine::general_purpose::STANDARD.encode(TUTORIAL_SAMPLE),
        "sampleName": "sloptweak-tutorial-room.jpg",
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

    /// Open (or re-point) the Invoke window. `tutorial` shows the tutorial
    /// on its own unless the user already closed it on this instance.
    pub fn open(&self, url: Url, tutorial: bool) -> Result<(), String> {
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
        for no in [
            "https://evil.trycloudflare.com/__sloptweak/tutorial/done",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/done/x",
            "https://a-b.trycloudflare.com/__sloptweak/tutorial/",
            "https://a-b.trycloudflare.com/",
        ] {
            assert_eq!(sig(no), None, "{no}");
        }
    }

    #[test]
    fn tutorial_script_is_one_call() {
        let s = tutorial_script(true, false);
        assert!(s.starts_with("//") && s.contains("(function (CFG)"));
        assert!(s.ends_with(");"));
        assert!(s.contains(r#""autoShow":true"#) && s.contains(r#""force":false"#));
        assert!(s.contains("/9j/"), "JPEG sample embedded as base64");
    }
}
