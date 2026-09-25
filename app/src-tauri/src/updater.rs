//! App updates from signed GitHub Releases (tauri-plugin-updater).
//!
//! The endpoint and public key are in `tauri.conf.json`; the plugin checks
//! the minisign signature of the downloaded installer against that key
//! before running it, so a tampered `latest.json` or installer is refused.
//! The UI can't call the plugin directly (no capability grants it); these
//! helpers and the `check_update` / `install_update` commands in `lib.rs`
//! are the only way in.

use serde::Serialize;

/// Release page, for "what's new".
pub const RELEASES_PAGE: &str = "https://github.com/ljohnsoncpu/SlopTweak/releases/latest";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    /// First lines of the release notes, plain text.
    pub notes: Option<String>,
}

/// Installing runs the NSIS installer and exits this process at once. With
/// a GPU running that would skip the last image sync and the destroy, so
/// it's refused until the session ends.
pub fn may_install(session_active: bool) -> Result<(), String> {
    if session_active {
        Err("Stop the GPU session first; updating closes SlopTweak.".into())
    } else {
        Ok(())
    }
}

/// Release notes are shown as text; keep them short.
pub fn short_notes(body: Option<&str>) -> Option<String> {
    const MAX: usize = 400;
    let body = body?.trim();
    if body.is_empty() {
        return None;
    }
    if body.chars().count() <= MAX {
        return Some(body.to_string());
    }
    let cut: String = body.chars().take(MAX).collect();
    Some(format!("{}…", cut.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_refused_while_a_gpu_runs() {
        assert!(may_install(true).is_err());
        assert!(may_install(false).is_ok());
    }

    #[test]
    fn notes_are_trimmed_and_capped() {
        assert_eq!(short_notes(None), None);
        assert_eq!(short_notes(Some("  \n ")), None);
        assert_eq!(short_notes(Some(" Fixes. \n")).as_deref(), Some("Fixes."));
        let long = "é".repeat(500);
        let s = short_notes(Some(&long)).unwrap();
        assert_eq!(s.chars().count(), 401);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn config_pins_https_github_endpoint_and_key() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let up = &conf["plugins"]["updater"];
        let endpoints = up["endpoints"].as_array().unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(
            endpoints[0],
            "https://github.com/ljohnsoncpu/SlopTweak/releases/latest/download/latest.json"
        );
        assert!(up.get("dangerousInsecureTransportProtocol").is_none());
        // minisign public key, base64 of "untrusted comment: minisign public key: …"
        let key = up["pubkey"].as_str().unwrap();
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(key)
            .unwrap();
        assert!(String::from_utf8(decoded)
            .unwrap()
            .starts_with("untrusted comment: minisign public key"));
    }
}
