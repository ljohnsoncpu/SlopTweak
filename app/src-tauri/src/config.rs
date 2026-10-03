//! Pinned instance inputs, user settings, and the create-call spec.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::catalog::{Backend, Model};
use crate::civitai::{compatible, Lora};
use crate::provider::{offers::CostInputs, LaunchSpec, OfferQuery, LABEL};

/// Official InvokeAI image, pinned by digest (findings §3; never below 6.13.8).
pub const IMAGE: &str = "ghcr.io/invoke-ai/invokeai:v6.14.1-cuda@sha256:39a7e3b182c4646634d62cf3ebdefd2082573ae20cdba773f9703fee29e600dd";
/// Vast's ComfyUI image for the Identity Edit backend, pinned by the digest of
/// the `v0.38.0-cuda-12.9-py312` tag (read from Docker Hub 2026-10-03).
pub const IMAGE_COMFY: &str = "vastai/comfy:v0.38.0-cuda-12.9-py312@sha256:5375f2d87a09461cc1755d13eec0c4775770831cef51cb35583480d12c842255";
/// Instance asset bundle (provision.sh, sidecar.py, requirements.txt),
/// published by the release workflow to this app version's own release. CI
/// rebuilds the bundle and refuses to release if its hash isn't
/// [`ASSETS_SHA256`]. Debug builds use [`assets_url`]'s dev default.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub const ASSETS_URL: &str = concat!(
    "https://github.com/ljohnsoncpu/SlopTweak/releases/download/v",
    env!("CARGO_PKG_VERSION"),
    "/instance-assets.tar.gz"
);
pub const ASSETS_SHA256: &str = "259665826526930b33ceaef2beda2906881061ab68a56b51ff9985f52f18c293";
/// Debug builds of a version that has no release yet fetch the same bytes
/// from the `instance-v0.1.2` dev pre-release; `SLOPTWEAK_DEV_ASSETS_URL` overrides it
/// (the instance still checks [`ASSETS_SHA256`]).
#[cfg(debug_assertions)]
const DEV_ASSETS_URL: &str =
    "https://github.com/ljohnsoncpu/SlopTweak/releases/download/instance-v0.1.2/instance-assets.tar.gz";

/// Where instances fetch the bundle from.
pub fn assets_url() -> String {
    #[cfg(debug_assertions)]
    {
        std::env::var("SLOPTWEAK_DEV_ASSETS_URL")
            .ok()
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_else(|| DEV_ASSETS_URL.to_string())
    }
    #[cfg(not(debug_assertions))]
    {
        ASSETS_URL.to_string()
    }
}
/// Vast limit on `onstart` (findings §1).
pub const ONSTART_LIMIT: usize = 4048;
/// Invoke version in [`IMAGE`]; catalog entries needing newer are skipped.
pub const INVOKE_VERSION: &str = "6.14.1";
/// The Invoke CUDA image needs a driver that supports at least this.
pub const MIN_CUDA: f64 = 12.4;
/// The ComfyUI image is built for CUDA 12.9.
pub const MIN_CUDA_COMFY: f64 = 12.9;

/// The Docker image a model's backend runs on.
pub fn image_for(model: &Model) -> &'static str {
    match model.backend {
        Backend::Invoke => IMAGE,
        Backend::Comfyui => IMAGE_COMFY,
    }
}

fn min_cuda_for(model: &Model) -> f64 {
    match model.backend {
        Backend::Invoke => MIN_CUDA,
        Backend::Comfyui => MIN_CUDA_COMFY,
    }
}

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
    /// Disk write speed floor, MB/s.
    pub min_disk_bw_mbps: f64,
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
    /// Last model picked on the home screen.
    pub model_id: Option<String>,
    /// Low-balance gate: refuse to start below this much Vast credit ($).
    pub min_credit: f64,
    /// Where images are saved (Phase 4). `None` = Pictures\SlopTweak.
    pub output_dir: Option<String>,
    pub loras: Vec<Lora>,
    /// The Invoke-window tutorial was finished or skipped (Phase 4).
    pub tutorial_done: bool,
    /// Tutorial stage the user is on, 1..=TUTORIAL_STAGES (Phase 6), so it
    /// resumes on the next GPU.
    pub tutorial_stage: u8,
}

/// Stages in the Invoke-window tutorial (PLAN §4 Phase 6).
pub const TUTORIAL_STAGES: u8 = 5;

fn first_stage() -> u8 {
    1
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_dph: 0.50,
            expected_hours: 1.0,
            // User decision 2026-09-25: only hosts with 99%+ reliability.
            min_reliability: 0.99,
            // User decision 2026-09-25 (Phase 3): cold hosts below ~2 Gbps /
            // 2 GB/s often couldn't pull + unpack the image inside the 12-min
            // loading cap. Costs ~$0.02/hr over the absolute cheapest.
            min_inet_down_mbps: 2000.0,
            min_disk_bw_mbps: 2000.0,
            min_compute_cap: 750,
            idle_minutes: 20,
            heartbeat_minutes: 10,
            max_session_minutes: 240,
            ready_timeout_minutes: 15,
            max_attempts: 3,
            model_id: None,
            min_credit: 1.0,
            output_dir: None,
            loras: Vec::new(),
            tutorial_done: false,
            tutorial_stage: 1,
        }
    }
}

/// The settings a user can change in the app. Only these are written to
/// settings.json, so the internal defaults above can still change in later
/// versions (a hand-edited file may still set the others).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserSettings {
    pub model_id: Option<String>,
    pub max_dph: f64,
    pub idle_minutes: u32,
    pub max_session_minutes: u32,
    pub min_credit: f64,
    pub output_dir: Option<String>,
    pub loras: Vec<Lora>,
    #[serde(default)]
    pub tutorial_done: bool,
    #[serde(default = "first_stage")]
    pub tutorial_stage: u8,
}

impl UserSettings {
    /// Range checks, in plain language.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.05..=5.0).contains(&self.max_dph) {
            return Err("Max price must be between $0.05 and $5.00 per hour.".into());
        }
        if !(5..=240).contains(&self.idle_minutes) {
            return Err("Idle shutdown must be between 5 and 240 minutes.".into());
        }
        if !(30..=24 * 60).contains(&self.max_session_minutes) {
            return Err("Max session length must be between 0.5 and 24 hours.".into());
        }
        if !(0.0..=1000.0).contains(&self.min_credit) {
            return Err("Minimum credit must be between $0 and $1000.".into());
        }
        if !(1..=TUTORIAL_STAGES).contains(&self.tutorial_stage) {
            return Err("Unknown tutorial stage.".into());
        }
        if let Some(d) = &self.output_dir {
            if d.trim().is_empty() || !Path::new(d).is_absolute() {
                return Err("Pick an output folder.".into());
            }
        }
        Ok(())
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

    pub fn user(&self) -> UserSettings {
        UserSettings {
            model_id: self.model_id.clone(),
            max_dph: self.max_dph,
            idle_minutes: self.idle_minutes,
            max_session_minutes: self.max_session_minutes,
            min_credit: self.min_credit,
            output_dir: self.output_dir.clone(),
            loras: self.loras.clone(),
            tutorial_done: self.tutorial_done,
            tutorial_stage: self.tutorial_stage.clamp(1, TUTORIAL_STAGES),
        }
    }

    /// Validate `u`, then write it (merged over any hand-set advanced keys).
    /// Atomic: temp file then rename.
    pub fn save_user(dir: &Path, u: &UserSettings) -> Result<Settings, String> {
        u.validate()?;
        let path = Self::path(dir);
        let mut obj = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| json!({}));
        let patch = serde_json::to_value(u).map_err(|e| e.to_string())?;
        for (k, v) in patch.as_object().expect("struct serializes to object") {
            obj[k] = v.clone();
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&obj).expect("json"))
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("Couldn't save settings: {e}"))?;
        Ok(Self::load(dir))
    }
}

/// The model plus the user's enabled LoRAs that fit its base. File names are
/// made unique so nothing overwrites anything on the instance.
pub fn with_loras(model: &Model, loras: &[Lora]) -> Model {
    let mut out = model.clone();
    // User LoRAs are Invoke LoRAs; the Identity Edit backend has its own.
    if model.backend == Backend::Comfyui {
        return out;
    }
    let mut names: HashSet<String> = out
        .files
        .iter()
        .map(|f| f.filename.to_ascii_lowercase())
        .collect();
    for l in loras
        .iter()
        .filter(|l| l.enabled && compatible(l, &model.base))
    {
        let mut f = l.file.clone();
        if !names.insert(f.filename.to_ascii_lowercase()) {
            let stem = f.filename.trim_end_matches(".safetensors");
            f.filename = format!("{stem}-{}.safetensors", l.version_id);
            if !names.insert(f.filename.to_ascii_lowercase()) {
                continue;
            }
        }
        out.files.push(f);
    }
    out
}

/// Room for the model plus Invoke's own files, caches, and outputs.
pub fn disk_gb(model: &Model) -> u32 {
    let model_gb = (model.total_bytes() as f64 / 1e9).ceil() as u32;
    (model_gb + 40).max(50)
}

/// How long one GPU gets to become ready: the setting, or more for a big
/// download. Live 2026-09-26: a host that passed the 2 Gbps filter pulled
/// Kroma's 31 GB at ~36 MB/s (~14 min) and missed a flat 15 min. Allow
/// ~0.6 min/GB plus 8 min for the image, verify, and registration.
pub fn ready_timeout_minutes(model: &Model, s: &Settings) -> u32 {
    let model_gb = model.total_bytes() as f64 / 1e9;
    let for_size = (8.0 + 0.6 * model_gb).ceil() as u32;
    s.ready_timeout_minutes.max(for_size)
}

pub fn offer_query(model: &Model, s: &Settings) -> OfferQuery {
    OfferQuery {
        min_vram_gb: model.min_vram_gb,
        min_disk_gb: disk_gb(model) as f64,
        min_reliability: s.min_reliability,
        min_inet_down_mbps: s.min_inet_down_mbps,
        min_disk_bw_mbps: s.min_disk_bw_mbps,
        max_dph: s.max_dph,
        min_cuda: min_cuda_for(model),
        min_compute_cap: s.min_compute_cap.max(model.min_compute_cap.unwrap_or(0)),
        min_ram_gb: model.min_ram_gb.unwrap_or(0.0),
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
    // The catalog's recommended settings ride on the main file, which is the
    // Invoke model they belong to (instance/model_defaults.py).
    let main_idx = model.files.iter().position(|f| f.kind == "main");
    let files: Vec<_> = model
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut v = json!({
                "url": f.url,
                "sha256": f.sha256.to_ascii_lowercase(),
                "size_bytes": f.size_bytes,
                "filename": f.filename,
                "requires_civitai_token": f.requires_civitai_token,
            });
            if let Some(d) = &f.dest {
                v["dest"] = json!(d);
            }
            if let (Some(d), true) = (&model.default_settings, Some(i) == main_idx) {
                v["default_settings"] = json!(d);
            }
            v
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
        ("SLOPTWEAK_ASSETS_URL".to_string(), assets_url()),
        (
            "SLOPTWEAK_ASSETS_SHA256".to_string(),
            ASSETS_SHA256.to_string(),
        ),
    ];
    if let Some(t) = civitai_token.filter(|_| model.needs_civitai()) {
        env.push(("CIVITAI_TOKEN".to_string(), t.to_string()));
    }
    if model.backend == Backend::Comfyui {
        env.push(("BACKEND".to_string(), "comfyui".to_string()));
    }
    LaunchSpec {
        image: image_for(model).to_string(),
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
    fn ready_timeout_grows_with_the_download() {
        let models = catalog::bundled();
        let get = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        let s = Settings::default();
        // Small models keep the setting.
        assert_eq!(ready_timeout_minutes(get("anima-aesthetic"), &s), 15);
        // Kroma (31 GB) missed a flat 15 min live; it gets ~27.
        let kroma = ready_timeout_minutes(get("kroma-turbo"), &s);
        assert!((25..=30).contains(&kroma), "{kroma}");
        // A higher setting still wins.
        let long = Settings {
            ready_timeout_minutes: 45,
            ..Settings::default()
        };
        assert_eq!(ready_timeout_minutes(get("kroma-turbo"), &long), 45);
    }

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
        assert!(IMAGE.contains(&format!(":v{INVOKE_VERSION}-cuda@")));
        assert_eq!(ASSETS_SHA256.len(), 64);
        assert!(ASSETS_SHA256.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    fn bundled_model(id: &str) -> Model {
        catalog::bundled().into_iter().find(|m| m.id == id).unwrap()
    }

    #[test]
    fn comfy_image_is_pinned_by_digest() {
        assert!(IMAGE_COMFY.starts_with("vastai/comfy:v0.38.0-cuda-12.9-py312@sha256:"));
        let digest = IMAGE_COMFY.rsplit_once("sha256:").unwrap().1;
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        // The image is CUDA 12.9: hosts must support it.
        const { assert!(MIN_CUDA_COMFY >= 12.9 && MIN_CUDA_COMFY > MIN_CUDA) };
    }

    #[test]
    fn the_backend_picks_image_cuda_and_env() {
        let (comfy, invoke) = (
            &bundled_model("wulver-identity-edit"),
            &bundled_model("wulver-turbo"),
        );
        assert_eq!(image_for(comfy), IMAGE_COMFY);
        assert_eq!(image_for(invoke), IMAGE);
        let s = Settings::default();
        assert_eq!(offer_query(comfy, &s).min_cuda, MIN_CUDA_COMFY);
        assert_eq!(offer_query(invoke, &s).min_cuda, MIN_CUDA);

        let spec = launch_spec(comfy, &s, &"a".repeat(64), Some("civitai-key"));
        assert_eq!(spec.image, IMAGE_COMFY);
        let env = |k: &str| {
            spec.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(env("BACKEND").as_deref(), Some("comfyui"));
        // No file needs the CivitAI key, so it isn't sent.
        assert!(env("CIVITAI_TOKEN").is_none());
        let files: Vec<serde_json::Value> = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(env("MODELS_B64").unwrap())
                .unwrap(),
        )
        .unwrap();
        let dests: Vec<&str> = files.iter().map(|f| f["dest"].as_str().unwrap()).collect();
        assert_eq!(dests, ["diffusion_models", "text_encoders", "vae", "loras"]);
        assert!(files[0].get("default_settings").is_none());

        // Invoke launches are unchanged: no BACKEND, no dest.
        let spec = launch_spec(invoke, &s, &"a".repeat(64), None);
        assert_eq!(spec.image, IMAGE);
        assert!(spec.env.iter().all(|(n, _)| n != "BACKEND"));
        let b64 = &spec.env.iter().find(|(n, _)| n == "MODELS_B64").unwrap().1;
        let files: Vec<serde_json::Value> = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap();
        assert!(files.iter().all(|f| f.get("dest").is_none()));
    }

    #[test]
    fn user_loras_are_not_added_to_a_comfyui_model() {
        let comfy = bundled_model("wulver-identity-edit");
        let l = lora(7, "Krea 2", "extra.safetensors");
        assert_eq!(with_loras(&comfy, &[l]).files.len(), comfy.files.len());
    }

    #[test]
    fn assets_come_from_this_versions_release() {
        let tag = format!("/releases/download/v{}/", env!("CARGO_PKG_VERSION"));
        assert!(ASSETS_URL.starts_with("https://github.com/ljohnsoncpu/SlopTweak/"));
        assert!(ASSETS_URL.contains(&tag), "{ASSETS_URL}");
        assert!(assets_url().starts_with("https://"));
    }

    fn lora(id: u64, base: &str, filename: &str) -> Lora {
        let mut l = crate::civitai::lora_from_version(&crate::civitai::mock_version(id)).unwrap();
        l.base_model = base.into();
        l.file.filename = filename.into();
        l
    }

    #[test]
    fn user_settings_save_keeps_advanced_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Settings::path(dir.path()),
            r#"{"min_inet_down_mbps": 900, "max_dph": 0.3}"#,
        )
        .unwrap();
        let mut u = Settings::load(dir.path()).user();
        u.max_dph = 0.2;
        u.idle_minutes = 30;
        u.model_id = Some("m".into());
        u.loras = vec![lora(1, "SDXL 1.0", "a.safetensors")];
        let s = Settings::save_user(dir.path(), &u).unwrap();
        assert_eq!(s.max_dph, 0.2);
        assert_eq!(s.idle_minutes, 30);
        assert_eq!(s.min_inet_down_mbps, 900.0);
        assert_eq!(s.loras.len(), 1);
        let raw = std::fs::read_to_string(Settings::path(dir.path())).unwrap();
        // Internal defaults aren't frozen into the file.
        assert!(!raw.contains("min_compute_cap"));
        assert!(!raw.contains("heartbeat_minutes"));
    }

    #[test]
    fn user_settings_ranges() {
        let ok = Settings::default().user();
        assert!(ok.validate().is_ok());
        type Mutation = Box<dyn Fn(&mut UserSettings)>;
        let cases: Vec<Mutation> = vec![
            Box::new(|u| u.max_dph = 0.0),
            Box::new(|u| u.max_dph = f64::NAN),
            Box::new(|u| u.idle_minutes = 1),
            Box::new(|u| u.max_session_minutes = 10),
            Box::new(|u| u.min_credit = -1.0),
            Box::new(|u| u.output_dir = Some("relative-dir".into())),
            Box::new(|u| u.tutorial_stage = 0),
            Box::new(|u| u.tutorial_stage = TUTORIAL_STAGES + 1),
        ];
        for mutate in cases {
            let mut u = ok.clone();
            mutate(&mut u);
            assert!(u.validate().is_err(), "{u:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let mut bad = ok.clone();
        bad.max_dph = 99.0;
        assert!(Settings::save_user(dir.path(), &bad).is_err());
        assert!(!Settings::path(dir.path()).exists());
    }

    #[test]
    fn loras_join_only_matching_models() {
        let model = catalog::bundled()[0].clone(); // sdxl
        let mut off = lora(4, "SDXL 1.0", "off.safetensors");
        off.enabled = false;
        let clash = lora(5, "Illustrious", &model.files[0].filename);
        let loras = vec![
            lora(1, "SDXL 1.0", "a.safetensors"),
            lora(6, "SD 1.5", "b.safetensors"),
            off,
            clash,
        ];
        let m = with_loras(&model, &loras);
        let names: Vec<&str> = m.files.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "bananaSplitzXXL_121.safetensors",
                "a.safetensors",
                "bananaSplitzXXL_121-5.safetensors"
            ]
        );
        assert_eq!(m.total_bytes(), model.total_bytes() + 2 * 228_452_344);
        // LoRA files carry the CivitAI key requirement into the launch spec.
        let spec = launch_spec(&m, &Settings::default(), &"a".repeat(64), Some("tok"));
        assert!(spec.env.iter().any(|(k, _)| k == "CIVITAI_TOKEN"));
        crate::provider::vast::env_string(&spec.env).unwrap();
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
    fn model_can_raise_the_gpu_floor_but_not_lower_it() {
        let mut model = catalog::bundled()[0].clone();
        let s = Settings::default();
        assert_eq!(offer_query(&model, &s).min_compute_cap, 750);
        model.min_compute_cap = Some(800);
        assert_eq!(offer_query(&model, &s).min_compute_cap, 800);
        model.min_compute_cap = Some(600);
        assert_eq!(offer_query(&model, &s).min_compute_cap, 750);
    }

    fn decoded_models(spec: &LaunchSpec) -> serde_json::Value {
        let b64 = &spec.env.iter().find(|(k, _)| k == "MODELS_B64").unwrap().1;
        serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn default_settings_ride_on_the_main_file_only() {
        let s = Settings::default();
        let turbo = catalog::bundled()
            .into_iter()
            .find(|m| m.id == "anima-turbo")
            .unwrap();
        let m = with_loras(&turbo, &[lora(1, "Anima", "a.safetensors")]);
        let files = decoded_models(&launch_spec(&m, &s, &"a".repeat(64), None));
        let files = files.as_array().unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(files[0]["filename"], "anima-turbo-v1.1.safetensors");
        assert_eq!(
            files[0]["default_settings"],
            json!({"cfg_scale": 1.0, "steps": 10})
        );
        assert!(files[1..]
            .iter()
            .all(|f| f.get("default_settings").is_none()));

        let mut plain = turbo.clone();
        plain.default_settings = None;
        let files = decoded_models(&launch_spec(&plain, &s, &"a".repeat(64), None));
        assert!(files
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f.get("default_settings").is_none()));
    }

    #[test]
    fn model_ram_floor_reaches_the_query() {
        let mut model = catalog::bundled()[0].clone();
        let s = Settings::default();
        model.min_ram_gb = None;
        assert_eq!(offer_query(&model, &s).min_ram_gb, 0.0);
        model.min_ram_gb = Some(48.0);
        assert_eq!(offer_query(&model, &s).min_ram_gb, 48.0);
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
        assert_eq!(s.tutorial_stage, 1);
    }

    #[test]
    fn tutorial_stage_resumes_and_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        // A Phase 4 file has tutorial_done but no stage.
        std::fs::write(Settings::path(dir.path()), r#"{"tutorial_done": true}"#).unwrap();
        let mut u = Settings::load(dir.path()).user();
        assert_eq!(u.tutorial_stage, 1);
        u.tutorial_stage = 3;
        assert_eq!(
            Settings::save_user(dir.path(), &u).unwrap().tutorial_stage,
            3
        );
        assert_eq!(Settings::load(dir.path()).tutorial_stage, 3);
        // A hand-edited stage out of range is clamped, not an error.
        std::fs::write(Settings::path(dir.path()), r#"{"tutorial_stage": 42}"#).unwrap();
        assert_eq!(
            Settings::load(dir.path()).user().tutorial_stage,
            TUTORIAL_STAGES
        );
    }
}
