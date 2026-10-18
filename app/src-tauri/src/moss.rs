//! MOSS-Image 8-step: the ComfyUI graph for one text-to-image run.
//!
//! Uses ComfyUI's native MOSS-Image support (PR 17095, not in a tagged release
//! yet; see docs/findings.md, "MOSS-Image step 0 verification"). The panel in
//! the window talks to [`MossService`] through the `moss_*` Tauri commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};
use url::Url;

use crate::catalog::{is_image, Model};
use crate::comfy::parse_history;
use crate::identity::{
    error_text, write_image, Aspect, ImageView, Kept, Phase, MAX_POLL_FAILURES, MAX_RESULTS, POLL,
    RUN_TIMEOUT,
};
use crate::sidecar::{SidecarApi, SidecarError, MAX_IMAGE_BYTES};

/// The delta file `provision.sh` writes into ComfyUI's `loras` folder: the
/// distillation's replacement time embedder and modulation biases, minus the
/// base model's, as `.diff` / `.diff_b` tensors. The catalog's raw
/// `time_embedding` file is not a LoRA, so the stock loader can't take it.
pub const TIME_DELTA_NAME: &str = "moss-image-8-step-dmd-time-delta.safetensors";

pub const MIN_STEPS: u32 = 4;
pub const MAX_STEPS: u32 = 8;
pub const DEFAULT_STEPS: u32 = 8;
pub const MAX_PROMPT_CHARS: usize = 4000;
const CFG: f64 = 1.0;

/// The ComfyUI file names the instance placed (the catalog's `filename`s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFiles {
    pub unet: String,
    pub clip: String,
    pub vae: String,
    pub lora: String,
}

impl ModelFiles {
    /// From a ComfyUI catalog entry for MOSS-Image, by file `kind`. The entry
    /// must also carry the raw time-embedding file the delta is built from.
    pub fn of(m: &Model) -> Option<Self> {
        if m.base != "moss-image" {
            return None;
        }
        let by = |kind: &str| {
            m.files
                .iter()
                .find(|f| f.kind == kind)
                .map(|f| f.filename.clone())
        };
        by("time_embedding")?;
        Some(Self {
            unet: by("main")?,
            clip: by("text_encoder")?,
            vae: by("vae")?,
            lora: by("lora")?,
        })
    }
}

/// Steps the sampler may take: the distillation is trained for 4 to 8.
pub fn clamp_steps(steps: u32) -> u32 {
    steps.clamp(MIN_STEPS, MAX_STEPS)
}

/// The prompt as sent: trimmed, never empty, never longer than the limit.
pub fn clean_prompt(prompt: &str) -> Result<String, String> {
    let p = prompt.trim();
    if p.is_empty() {
        return Err("Type a prompt first.".into());
    }
    if p.chars().count() > MAX_PROMPT_CHARS {
        return Err(format!(
            "That prompt is too long (the limit is {MAX_PROMPT_CHARS} characters)."
        ));
    }
    Ok(p.to_string())
}

/// MOSS-Image's latent is 1/16 of the picture, so sizes are multiples of 16.
pub fn valid_size((w, h): (u32, u32)) -> bool {
    (256..=2048).contains(&w) && (256..=2048).contains(&h) && w % 16 == 0 && h % 16 == 0
}

/// The ComfyUI "API format" graph for one image. Shift 5 is the model's own
/// default in ComfyUI, so there is no ModelSamplingSD3 node. CFG 1 makes the
/// negative prompt unused, but KSampler still needs one wired.
pub fn build_graph(
    files: &ModelFiles,
    prompt: &str,
    size: (u32, u32),
    steps: u32,
    seed: u64,
) -> Value {
    let (w, h) = size;
    json!({
        "1": {"class_type": "UNETLoader",
              "inputs": {"unet_name": files.unet, "weight_dtype": "default"}},
        "2": {"class_type": "CLIPLoader",
              "inputs": {"clip_name": files.clip, "type": "moss_image", "device": "default"}},
        "3": {"class_type": "VAELoader", "inputs": {"vae_name": files.vae}},
        "10": {"class_type": "LoraLoaderModelOnly",
               "inputs": {"model": ["1", 0], "lora_name": files.lora, "strength_model": 1.0}},
        "11": {"class_type": "LoraLoaderModelOnly",
               "inputs": {"model": ["10", 0], "lora_name": TIME_DELTA_NAME, "strength_model": 1.0}},
        "4": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": prompt}},
        "5": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": ""}},
        "6": {"class_type": "EmptyFlux2LatentImage",
              "inputs": {"width": w, "height": h, "batch_size": 1}},
        "7": {"class_type": "KSampler", "inputs": {
            "seed": seed, "steps": clamp_steps(steps), "cfg": CFG,
            "sampler_name": "euler", "scheduler": "simple", "denoise": 1.0,
            "model": ["11", 0], "positive": ["4", 0], "negative": ["5", 0],
            "latent_image": ["6", 0]}},
        "8": {"class_type": "VAEDecode",
              "inputs": {"samples": ["7", 0], "vae": ["3", 0]}},
        "9": {"class_type": "SaveImage",
              "inputs": {"filename_prefix": "SlopTweak", "images": ["8", 0]}},
    })
}

/// Queue one graph on the instance, wait for it, and fetch the picture. The
/// graph and the wait loop are the same shape as Identity Edit's. `Ok(None)`
/// means it was cancelled; `on_queued` gets the prompt id once ComfyUI has it.
pub async fn run(
    sidecar: &dyn SidecarApi,
    base: &Url,
    secret: &str,
    graph: &Value,
    cancel: &AtomicBool,
    on_queued: &(dyn Fn(&str) + Sync),
) -> Result<Option<(String, String, Vec<u8>)>, String> {
    let unreachable = |e: SidecarError| match e {
        SidecarError::Unauthorized => {
            "The GPU rejected this app's login. Stop it and start again.".to_string()
        }
        e => format!("Couldn't reach the GPU: {e}"),
    };
    let queued = sidecar
        .comfy_post_json(
            base,
            secret,
            &["prompt"],
            &json!({"prompt": graph, "client_id": "sloptweak"}),
        )
        .await
        .map_err(|e| match e {
            SidecarError::Http(400) => "The GPU didn't accept the request. If the model files                 are still loading, wait a minute and try again."
                .to_string(),
            e => unreachable(e),
        })?;
    let prompt_id = queued["prompt_id"]
        .as_str()
        .ok_or("The GPU didn't say what it started.")?
        .to_string();
    on_queued(&prompt_id);
    let started = tokio::time::Instant::now();
    let mut failures = 0u32;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if started.elapsed() > RUN_TIMEOUT {
            return Err("This is taking far too long. Cancel and try again.".into());
        }
        tokio::time::sleep(POLL).await;
        let history = match sidecar
            .comfy_json(base, secret, &["history", &prompt_id], &[])
            .await
        {
            Ok(h) => {
                failures = 0;
                h
            }
            Err(e @ SidecarError::Unauthorized) => return Err(unreachable(e)),
            Err(e) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    return Err(unreachable(e));
                }
                continue;
            }
        };
        let Some(entry) = history.get(&prompt_id) else {
            continue; // still queued or running
        };
        match entry["status"]["status_str"].as_str().unwrap_or("") {
            "error" => return Err(error_text(entry)),
            "success" => {}
            _ => continue,
        }
        let img = parse_history(&history)
            .into_iter()
            .next()
            .ok_or("The GPU finished but made no image.")?;
        let bytes = sidecar
            .comfy_bytes(base, secret, &["view"], &img.view_query(), MAX_IMAGE_BYTES)
            .await
            .map_err(unreachable)?;
        if !is_image(&bytes) {
            return Err("The GPU sent something that isn't an image.".into());
        }
        return Ok(Some((prompt_id, img.filename, bytes)));
    }
}

// ----- what the panel shows ----------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MossView {
    pub results: Vec<ImageView>,
    pub phase: Phase,
    pub message: String,
}

#[derive(Default)]
struct Inner {
    results: Vec<Kept>,
    phase: Phase,
    message: String,
    cancel: Option<Arc<AtomicBool>>,
    prompt_id: Option<String>,
}

/// Where one run goes: the instance's tunnel, the launch secret, and the
/// ComfyUI file names to load.
pub struct Target {
    pub sidecar: Arc<dyn SidecarApi>,
    pub base: Url,
    pub secret: String,
    pub files: ModelFiles,
}

/// A checked request, ready to run.
pub struct Begun {
    prompt: String,
    size: (u32, u32),
    steps: u32,
    cancel: Arc<AtomicBool>,
}

pub struct MossService {
    inner: Mutex<Inner>,
    /// Called after every change so the window can refresh.
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl MossService {
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            notify,
        }
    }

    pub fn view(&self) -> MossView {
        let i = self.inner.lock().unwrap();
        MossView {
            results: i.results.iter().rev().map(Kept::view).collect(),
            phase: i.phase,
            message: i.message.clone(),
        }
    }

    /// A new session starts from a clean slate.
    pub fn reset_run(&self) {
        {
            let mut i = self.inner.lock().unwrap();
            i.phase = Phase::Idle;
            i.message.clear();
            i.cancel = None;
            i.prompt_id = None;
        }
        (self.notify)();
    }

    /// Check a request and mark the panel busy. Refuses a second one while a
    /// picture is being made.
    pub fn begin(&self, prompt: &str, aspect: Aspect, steps: Option<u32>) -> Result<Begun, String> {
        let prompt = clean_prompt(prompt)?;
        let size = aspect.size();
        if !valid_size(size) {
            return Err("That shape isn't supported.".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut i = self.inner.lock().unwrap();
            if i.phase.busy() {
                return Err("An image is already being made.".into());
            }
            i.phase = Phase::Running;
            i.message =
                "Making your image (the first one takes longer while the model loads)…".into();
            i.cancel = Some(cancel.clone());
            i.prompt_id = None;
        }
        (self.notify)();
        Ok(Begun {
            prompt,
            size,
            steps: clamp_steps(steps.unwrap_or(DEFAULT_STEPS)),
            cancel,
        })
    }

    /// Run a request [`begin`](Self::begin) accepted, and record how it ended.
    pub async fn finish(self: &Arc<Self>, t: &Target, b: Begun, seed: u64) {
        let graph = build_graph(&t.files, &b.prompt, b.size, b.steps, seed);
        let me = self.clone();
        let on_queued = move |id: &str| {
            me.inner.lock().unwrap().prompt_id = Some(id.to_string());
        };
        let outcome = run(
            t.sidecar.as_ref(),
            &t.base,
            &t.secret,
            &graph,
            &b.cancel,
            &on_queued,
        )
        .await;
        match outcome {
            Ok(Some((id, name, bytes))) => {
                {
                    let mut i = self.inner.lock().unwrap();
                    i.results.push(Kept { id, name, bytes });
                    let extra = i.results.len().saturating_sub(MAX_RESULTS);
                    i.results.drain(..extra);
                }
                self.set_phase(
                    Phase::Done,
                    "Done. The image is saved to your output folder.",
                );
            }
            Ok(None) => self.set_phase(Phase::Cancelled, "Cancelled."),
            Err(e) => self.set_phase(Phase::Failed, e),
        }
    }

    /// Stop the current image: flag the wait loop, and tell ComfyUI.
    pub async fn cancel(&self, t: &Target) {
        let (flag, running) = {
            let i = self.inner.lock().unwrap();
            (i.cancel.clone(), i.prompt_id.is_some())
        };
        if let Some(flag) = flag {
            flag.store(true, Ordering::SeqCst);
        }
        if running {
            let _ = t
                .sidecar
                .comfy_post_json(&t.base, &t.secret, &["interrupt"], &json!({}))
                .await;
        }
    }

    /// Write a finished image into `dir` (the Downloads folder) under a name
    /// that doesn't overwrite anything; returns the path written.
    pub fn save_result(
        &self,
        result_id: &str,
        dir: &std::path::Path,
    ) -> Result<std::path::PathBuf, String> {
        let (name, bytes) = {
            let i = self.inner.lock().unwrap();
            let r = i
                .results
                .iter()
                .find(|r| r.id == result_id)
                .ok_or("That image is no longer in the panel.")?;
            (r.name.clone(), r.bytes.clone())
        };
        write_image(&name, &bytes, dir)
    }

    fn set_phase(&self, phase: Phase, message: impl Into<String>) {
        {
            let mut i = self.inner.lock().unwrap();
            i.phase = phase;
            i.message = message.into();
        }
        (self.notify)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> ModelFiles {
        ModelFiles {
            unet: "moss_image_base_bf16.safetensors".into(),
            clip: "qwen3.5_9b_int8_convrot.safetensors".into(),
            vae: "flux2-vae.safetensors".into(),
            lora: "moss-image-8-step-dmd-lora.safetensors".into(),
        }
    }

    fn bundled_moss() -> Model {
        crate::catalog::bundled()
            .into_iter()
            .find(|m| m.id == "moss-image-dmd")
            .unwrap()
    }

    #[test]
    fn model_files_come_from_the_catalog_entry() {
        assert_eq!(ModelFiles::of(&bundled_moss()), Some(files()));
        // Other ComfyUI and Invoke models are not MOSS models.
        for other in ["wulver-identity-edit", "wulver-turbo"] {
            let m = crate::catalog::bundled()
                .into_iter()
                .find(|m| m.id == other)
                .unwrap();
            assert_eq!(ModelFiles::of(&m), None, "{other}");
        }
        // The raw time-embedding file is required (the delta is built from it).
        let mut m = bundled_moss();
        m.files.retain(|f| f.kind != "time_embedding");
        assert_eq!(ModelFiles::of(&m), None);
    }

    #[test]
    fn graph_matches_the_native_comfyui_nodes() {
        let g = build_graph(&files(), "a red fox", (1024, 896), 8, 42);
        assert_eq!(g["2"]["inputs"]["type"], "moss_image");
        assert_eq!(g["6"]["class_type"], "EmptyFlux2LatentImage");
        assert_eq!(g["6"]["inputs"]["width"], 1024);
        assert_eq!(g["6"]["inputs"]["height"], 896);
        assert_eq!(g["4"]["inputs"]["text"], "a red fox");
        assert_eq!(g["5"]["inputs"]["text"], "");
        let k = &g["7"]["inputs"];
        assert_eq!(
            (k["steps"].as_u64(), k["cfg"].as_f64()),
            (Some(8), Some(1.0))
        );
        assert_eq!(k["seed"], 42);
        assert_eq!(k["sampler_name"], "euler");
        // Base -> distillation LoRA -> time-embedding delta -> sampler.
        assert_eq!(g["10"]["inputs"]["model"], json!(["1", 0]));
        assert_eq!(g["11"]["inputs"]["model"], json!(["10", 0]));
        assert_eq!(g["11"]["inputs"]["lora_name"], TIME_DELTA_NAME);
        assert_eq!(k["model"], json!(["11", 0]));
        assert_eq!(g["9"]["inputs"]["images"], json!(["8", 0]));
    }

    #[test]
    fn every_link_points_at_a_node_in_the_graph() {
        let g = build_graph(&files(), "x", (1024, 1024), 8, 1);
        for (id, node) in g.as_object().unwrap() {
            for (name, v) in node["inputs"].as_object().unwrap() {
                if let Some([src, _]) = v.as_array().map(Vec::as_slice) {
                    let src = src.as_str().unwrap();
                    assert!(g.get(src).is_some(), "node {id}.{name} -> missing {src}");
                }
            }
        }
    }

    #[test]
    fn steps_stay_inside_the_trained_range() {
        assert_eq!(clamp_steps(1), 4);
        assert_eq!(clamp_steps(6), 6);
        assert_eq!(clamp_steps(50), 8);
        let g = build_graph(&files(), "x", (1024, 1024), 50, 1);
        assert_eq!(g["7"]["inputs"]["steps"], 8);
    }

    #[test]
    fn prompts_are_trimmed_and_limited() {
        assert_eq!(clean_prompt("  a fox \n").unwrap(), "a fox");
        assert!(clean_prompt("   ").is_err());
        assert!(clean_prompt(&"x".repeat(MAX_PROMPT_CHARS)).is_ok());
        assert!(clean_prompt(&"x".repeat(MAX_PROMPT_CHARS + 1)).is_err());
        // Counted in characters, not bytes.
        assert!(clean_prompt(&"é".repeat(MAX_PROMPT_CHARS)).is_ok());
    }

    #[test]
    fn sizes_are_multiples_of_16_in_range() {
        assert!(valid_size((1024, 1024)));
        assert!(valid_size((896, 1152)));
        assert!(!valid_size((1000, 1024)));
        assert!(!valid_size((128, 1024)));
        assert!(!valid_size((4096, 1024)));
    }

    fn service() -> (Arc<MossService>, Arc<std::sync::atomic::AtomicU32>) {
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = n.clone();
        let svc = Arc::new(MossService::new(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        })));
        (svc, n)
    }

    fn kept(id: &str) -> Kept {
        Kept {
            id: id.into(),
            name: format!("SlopTweak_{id}_.png"),
            bytes: b"\x89PNG\r\n\x1a\nxx".to_vec(),
        }
    }

    #[test]
    fn begin_marks_the_panel_busy_and_refuses_a_second_request() {
        let (svc, notified) = service();
        let b = svc.begin("  a fox ", Aspect::Portrait, Some(50)).unwrap();
        assert_eq!(
            (b.prompt.as_str(), b.size, b.steps),
            ("a fox", (896, 1152), 8)
        );
        assert_eq!(svc.view().phase, Phase::Running);
        assert!(notified.load(Ordering::SeqCst) >= 1);
        let again = svc.begin("another", Aspect::Square, None);
        assert_eq!(
            again.err().as_deref(),
            Some("An image is already being made.")
        );
        // A finished run frees the panel again.
        svc.set_phase(Phase::Done, "ok");
        assert!(svc.begin("another", Aspect::Square, None).is_ok());
    }

    #[test]
    fn a_bad_prompt_never_marks_the_panel_busy() {
        let (svc, _) = service();
        assert!(svc.begin("   ", Aspect::Square, None).is_err());
        assert!(svc
            .begin(&"x".repeat(MAX_PROMPT_CHARS + 1), Aspect::Square, None)
            .is_err());
        assert_eq!(svc.view().phase, Phase::Idle);
    }

    #[test]
    fn results_show_newest_first_and_reset_keeps_them() {
        let (svc, _) = service();
        {
            let mut i = svc.inner.lock().unwrap();
            i.results.push(kept("a"));
            i.results.push(kept("b"));
        }
        let ids: Vec<String> = svc.view().results.into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["b", "a"]);
        svc.set_phase(Phase::Failed, "boom");
        svc.reset_run();
        let v = svc.view();
        assert_eq!(
            (v.phase, v.message.as_str(), v.results.len()),
            (Phase::Idle, "", 2)
        );
    }

    #[test]
    fn save_writes_a_plain_name_and_never_overwrites() {
        let (svc, _) = service();
        svc.inner.lock().unwrap().results.push(kept("a"));
        let dir = std::env::temp_dir().join(format!("sloptweak-moss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = svc.save_result("a", &dir).unwrap();
        let second = svc.save_result("a", &dir).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.file_name().unwrap(), "SlopTweak_a_.png");
        assert!(std::fs::read(&first).unwrap().starts_with(b"\x89PNG"));
        assert!(svc.save_result("nope", &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
