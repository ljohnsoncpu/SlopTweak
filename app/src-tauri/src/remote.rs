//! The window that shows Invoke from the instance tunnel.
//!
//! Security (CLAUDE.md invariants, findings §6):
//! * Its label never appears in a capability, so the ACL denies every IPC
//!   call from it (`__TAURI_INTERNALS__` is still injected, but inert).
//! * It uses its own WebView2 data directory, so its cookies and storage
//!   are separate from the local UI.
//! * Navigation is pinned to the tunnel origin, and popups are denied.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use url::{Origin, Url};

pub const LABEL_PREFIX: &str = "remote-";

pub struct RemoteWindows {
    app: AppHandle,
    current: Mutex<Option<(String, Origin)>>,
    counter: AtomicU32,
}

/// Navigation rule for the remote window: same origin as the tunnel only.
pub fn allowed(origin: &Origin, url: &Url) -> bool {
    &url.origin() == origin
}

impl RemoteWindows {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            current: Mutex::new(None),
            counter: AtomicU32::new(0),
        }
    }

    pub fn open(&self, url: Url) -> Result<(), String> {
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
        #[allow(unused_mut)]
        let mut builder = WebviewWindowBuilder::new(&self.app, &label, WebviewUrl::External(url))
            .title("SlopTweak — Invoke")
            .inner_size(1440.0, 920.0)
            .min_inner_size(900.0, 600.0)
            .data_directory(data_dir)
            .on_navigation(move |u| {
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
}
