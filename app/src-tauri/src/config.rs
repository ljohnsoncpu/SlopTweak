//! Pinned instance inputs, user settings, and the create-call spec.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::catalog::Model;
use crate::provider::{offers::CostInputs, LaunchSpec, OfferQuery, LABEL};

/// Official InvokeAI image, pinned by digest (findings §3; never below 6.13.8).
pub const IMAGE: &str = "ghcr.io/invoke-ai/invokeai:v6.14.1-cuda@sha256:39a7e3b182c4646634d62cf3ebdefd2082573ae20cdba773f9703fee29e600dd";
/// Instance asset bundle (provision.sh, sidecar.py, requirements.txt).
pub const ASSETS_URL: &str =
    "https://github.com/ljohnsoncpu/SlopTweak/releases/download/instance-v0.1.0/instance-assets.tar.gz";
pub const ASSETS_SHA256: &str = "2ff1ecf68f2766a2c75b16e069670b8caca64405195313fd4439b1d619693811";
/// Vast limit on `onstart` (findings §1).
pub const ONSTART_LIMIT: usize = 4048;
/// The Invoke CUDA image needs a driver that supports at least this.
pub const MIN_CUDA: f64 = 12.4;

pub fn onstart() -> String {
    let s = include_str!("../../../instance/onstart.sh").replace("\r\n", "\n");
    debug_assert!(s.len() < ONSTART_LIMIT);
    s
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub max_dph: f64,
    pub expected_hours: f64,
    pub min_reliability: f64,
    pub min_inet_down_mbps: f64,
    /// GPU architecture floor, Vast units. 750 = Turing (RTX 20xx) and newer:
    /// fp16 tensor cores for SDXL, and still supported after the torch 2.8
    /// drop of Maxwell/Pascal. Live, a GTX TITAN X (520) was the cheapest pick.
    pub min_compute_cap: u32,
    pub idle_minutes: u32,
    pub heartbeat_minutes: u32,
    pub max_session_minutes: u32,
    /// Per attempt: destroy and try the next offer if not ready by then.
    pub ready_timeout_minutes: u32,
    pub max_attempts: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_dph: 0.50,
            expected_hours: 1.0,
            min_reliability: 0.98,
            // Cheap over fast: slow hosts pull the image slowly, but loading may
            // run up to 12 min while it shows progress (15 min per attempt).
            min_inet_down_mbps: 500.0,
            min_compute_cap: 750,
            idle_minutes: 20,
            heartbeat_minutes: 10,
            max_session_minutes: 240,
            ready_timeout_minutes: 15,
            max_attempts: 3,
        }
    }
}

impl Settings {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(Self::path(dir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
}

/// Room for the model plus Invoke's own files, caches, and outputs.
pub fn disk_gb(model: &Model) -> u32 {
    let model_gb = (model.total_bytes() as f64 / 1e9).ceil() as u32;
    (model_gb + 40).max(50)
}

pub fn offer_query(model: &Model, s: &Settings) -> OfferQuery {
    OfferQuery {
        min_vram_gb: model.min_vram_gb,
        min_disk_gb: disk_gb(model) as f64,
        min_reliability: s.min_reliability,
        min_inet_down_mbps: s.min_inet_down_mbps,
        max_dph: s.max_dph,
        min_cuda: MIN_CUDA,
        min_compute_cap: s.min_compute_cap,
        limit: 64,
    }
}

pub fn cost_inputs(model: &Model, s: &Settings) -> CostInputs {
    CostInputs {
        expected_hours: s.expected_hours,
        model_bytes: model.total_bytes(),
        disk_gb: disk_gb(model) as f64,
    }
}

/// The instance contract (instance/sidecar.py, provision.sh).
pub fn launch_spec(
    model: &Model,
    s: &Settings,
    launch_token_hash: &str,
    civitai_token: Option<&str>,
) -> LaunchSpec {
    let files: Vec<_> = model
        .files
        .iter()
        .map(|f| {
            json!({
                "url": f.url,
                "sha256": f.sha256.to_ascii_lowercase(),
                "size_bytes": f.size_bytes,
                "filename": f.filename,
                "requires_civitai_token": f.requires_civitai_token,
            })
        })
        .collect();
    let models_b64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&files).expect("json"));
    let mut env = vec![
        (
            "LAUNCH_TOKEN_HASH".to_string(),
            launch_token_hash.to_string(),
        ),
        ("MODELS_B64".to_string(), models_b64),
        ("IDLE_MINUTES".to_string(), s.idle_minutes.to_string()),
        (
            "HEARTBEAT_MINUTES".to_string(),
            s.heartbeat_minutes.to_string(),
        ),
        (
            "MAX_SESSION_MINUTES".to_string(),
            s.max_session_minutes.to_string(),
        ),
        ("SLOPTWEAK_ASSETS_URL".to_string(), ASSETS_URL.to_string()),
        (
            "SLOPTWEAK_ASSETS_SHA256".to_string(),
            ASSETS_SHA256.to_string(),
        ),
    ];
    if let Some(t) = civitai_token.filter(|_| model.needs_civitai()) {
        env.push(("CIVITAI_TOKEN".to_string(), t.to_string()));
    }
    LaunchSpec {
        image: IMAGE.to_string(),
        disk_gb: disk_gb(model),
        label: LABEL.to_string(),
        env,
        onstart: onstart(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn onstart_fits_vast_limit() {
        let o = onstart();
        assert!(o.len() < ONSTART_LIMIT, "onstart is {} chars", o.len());
        assert!(!o.contains('\r'));
        assert!(o.starts_with("#!/usr/bin/env bash"));
    }

    #[test]
    fn image_is_pinned_by_digest() {
        assert!(IMAGE.contains("-cuda@sha256:"));
        assert_eq!(ASSETS_SHA256.len(), 64);
    }

    #[test]
    fn spec_matches_instance_contract() {
        let model = &catalog::bundled()[0];
        let spec = launch_spec(model, &Settings::default(), &"a".repeat(64), Some("tok123"));
        let keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "LAUNCH_TOKEN_HASH",
                "MODELS_B64",
                "IDLE_MINUTES",
                "HEARTBEAT_MINUTES",
                "MAX_SESSION_MINUTES",
                "SLOPTWEAK_ASSETS_URL",
                "SLOPTWEAK_ASSETS_SHA256",
                "CIVITAI_TOKEN"
            ]
        );
        assert_eq!(spec.label, "sloptweak");
        // 7 GB model + 40 GB headroom, floored at 50.
        assert_eq!(spec.disk_gb, 50);
        let b64 = &spec.env[1].1;
        let decoded: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(decoded[0]["size_bytes"], 6_938_043_264u64);
        assert_eq!(decoded[0]["requires_civitai_token"], true);
        assert_eq!(
            decoded[0]["sha256"].as_str().unwrap(),
            decoded[0]["sha256"].as_str().unwrap().to_ascii_lowercase()
        );
        // Must survive Vast's env string checks.
        crate::provider::vast::env_string(&spec.env).unwrap();
    }

    #[test]
    fn civitai_token_only_when_needed() {
        let mut model = catalog::bundled()[0].clone();
        model.files[0].requires_civitai_token = false;
        let spec = launch_spec(&model, &Settings::default(), &"a".repeat(64), Some("tok"));
        assert!(spec.env.iter().all(|(k, _)| k != "CIVITAI_TOKEN"));
    }

    #[test]
    fn settings_tolerate_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Settings::path(dir.path()), r#"{"max_dph": 0.3}"#).unwrap();
        let s = Settings::load(dir.path());
        assert_eq!(s.max_dph, 0.3);
        assert_eq!(s.max_attempts, 3);
    }
}
