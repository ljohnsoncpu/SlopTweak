//! MOSS-Image: the ComfyUI graph for one text-to-image run, and the panel's engine.
//!
//! Uses ComfyUI's native MOSS-Image support (PR 17095, not in a tagged release
//! yet; see docs/findings.md, "MOSS-Image step 0 verification"). The panel in
//! the window talks to [`MossService`] through the `moss_*` Tauri commands.
//!
//! Two modes share the same weights. **Fast** applies lodestones' 8-step
//! distillation (a LoRA plus a time-embedding patch) at CFG 1. **Full quality**
//! is the plain base model, which the card samples at 50 steps and CFG 3 with
//! layer 22 skipped in the negative branch (ComfyUI's SkipLayerGuidanceDiTSimple,
//! as in the official and ostris workflows).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::catalog::{is_image, Model};
use crate::comfy::parse_history;
use crate::identity::{
    error_text, write_image, Kept, Phase, MAX_POLL_FAILURES, MAX_RESULTS, POLL, RUN_TIMEOUT,
};
use crate::sidecar::{SidecarApi, SidecarError, MAX_IMAGE_BYTES};

/// The delta file `provision.sh` writes into ComfyUI's `loras` folder: the
/// distillation's replacement time embedder and modulation biases, minus the
/// base model's, as `.diff` / `.diff_b` tensors. The catalog's raw
/// `time_embedding` file is not a LoRA, so the stock loader can't take it.
pub const TIME_DELTA_NAME: &str = "moss-image-8-step-dmd-time-delta.safetensors";

pub const MIN_STEPS: u32 = 1;
/// The distillation was trained for 4 to 8 steps; more are allowed to experiment.
pub const MAX_STEPS_FAST: u32 = 20;
pub const MAX_STEPS_FULL: u32 = 60;
pub const MIN_SIDE: u32 = 256;
pub const MAX_SIDE: u32 = 2048;
pub const MAX_PROMPT_CHARS: usize = 4000;
pub const MIN_CFG: f64 = 1.0;
pub const MAX_CFG: f64 = 10.0;
/// What the base model card samples with.
pub const FULL_DEFAULT_CFG: f64 = 3.0;
/// The layer the card skips in the negative branch.
pub const SKIP_LAYER: &str = "22";

/// Samplers offered in the panel (ComfyUI's own names). Euler is what the
/// distillation was made for; the others are there to try.
pub const SAMPLERS: &[&str] = &[
    "euler",
    "euler_ancestral",
    "heun",
    "dpm_2",
    "dpmpp_2m",
    "dpmpp_2m_sde",
    "dpmpp_3m_sde",
    "res_multistep",
    "ipndm",
    "uni_pc",
    "lcm",
];

/// Schedulers offered in the panel. `simple` follows the model's own sigmas.
pub const SCHEDULERS: &[&str] = &["simple", "normal", "sgm_uniform", "karras", "beta"];

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

fn yes() -> bool {
    true
}

fn default_sampler() -> String {
    "euler".into()
}

fn default_scheduler() -> String {
    "simple".into()
}

/// What the panel sends for one picture.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    #[serde(default = "default_sampler")]
    pub sampler: String,
    #[serde(default = "default_scheduler")]
    pub scheduler: String,
    /// Full quality only; Fast always runs at CFG 1.
    #[serde(default)]
    pub cfg: Option<f64>,
    /// `true` = Fast (the 8-step distillation), `false` = Full quality (base model).
    #[serde(default = "yes")]
    pub fast: bool,
    /// Blank = a random one.
    #[serde(default)]
    pub seed: Option<u64>,
}

/// A checked [`Request`]: every value is one the graph can use.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub prompt: String,
    pub size: (u32, u32),
    pub steps: u32,
    pub sampler: String,
    pub scheduler: String,
    pub cfg: f64,
    pub fast: bool,
    pub seed: u64,
}

/// ComfyUI takes 64-bit seeds; keep them in what a JS number holds exactly.
const SEED_MASK: u64 = (1 << 52) - 1;

impl Params {
    /// Check a request. `random_seed` is used when the request has none.
    pub fn check(r: &Request, random_seed: u64) -> Result<Self, String> {
        let prompt = clean_prompt(&r.prompt)?;
        let size = (r.width, r.height);
        if !valid_size(size) {
            return Err(format!(
                "The size must be a multiple of 16 between {MIN_SIDE} and {MAX_SIDE} on each side."
            ));
        }
        let max_steps = if r.fast {
            MAX_STEPS_FAST
        } else {
            MAX_STEPS_FULL
        };
        if !(MIN_STEPS..=max_steps).contains(&r.steps) {
            return Err(format!(
                "Steps must be between {MIN_STEPS} and {max_steps} in this mode."
            ));
        }
        if !SAMPLERS.contains(&r.sampler.as_str()) {
            return Err("That sampler isn't available.".into());
        }
        if !SCHEDULERS.contains(&r.scheduler.as_str()) {
            return Err("That scheduler isn't available.".into());
        }
        let cfg = if r.fast {
            1.0
        } else {
            let cfg = r.cfg.unwrap_or(FULL_DEFAULT_CFG);
            if !(MIN_CFG..=MAX_CFG).contains(&cfg) {
                return Err(format!("CFG must be between {MIN_CFG} and {MAX_CFG}."));
            }
            cfg
        };
        Ok(Self {
            prompt,
            size,
            steps: r.steps,
            sampler: r.sampler.clone(),
            scheduler: r.scheduler.clone(),
            cfg,
            fast: r.fast,
            seed: r.seed.unwrap_or(random_seed) & SEED_MASK,
        })
    }

    /// One line for the picture's caption: what made it.
    pub fn summary(&self) -> String {
        let mode = if self.fast {
            "Fast".to_string()
        } else {
            format!("Full quality, CFG {}", self.cfg)
        };
        format!(
            "{}×{} · {} · {} steps · {}/{} · seed {}",
            self.size.0, self.size.1, mode, self.steps, self.sampler, self.scheduler, self.seed
        )
    }
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
    (MIN_SIDE..=MAX_SIDE).contains(&w)
        && (MIN_SIDE..=MAX_SIDE).contains(&h)
        && w % 16 == 0
        && h % 16 == 0
}

/// The ComfyUI "API format" graph for one image. Shift 5 is the model's own
/// default in ComfyUI, so there is no ModelSamplingSD3 node. In Fast mode CFG 1
/// makes the negative prompt unused, but KSampler still needs one wired.
pub fn build_graph(files: &ModelFiles, p: &Params) -> Value {
    let (w, h) = p.size;
    let mut g = json!({
        "1": {"class_type": "UNETLoader",
              "inputs": {"unet_name": files.unet, "weight_dtype": "default"}},
        "2": {"class_type": "CLIPLoader",
              "inputs": {"clip_name": files.clip, "type": "moss_image", "device": "default"}},
        "3": {"class_type": "VAELoader", "inputs": {"vae_name": files.vae}},
        "4": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": p.prompt}},
        "5": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": ""}},
        "6": {"class_type": "EmptyFlux2LatentImage",
              "inputs": {"width": w, "height": h, "batch_size": 1}},
        "8": {"class_type": "VAEDecode",
              "inputs": {"samples": ["7", 0], "vae": ["3", 0]}},
        "9": {"class_type": "SaveImage",
              "inputs": {"filename_prefix": "SlopTweak", "images": ["8", 0]}},
    });
    // Fast: base -> distillation LoRA -> time-embedding delta. Full: the base, with
    // layer 22 skipped in the negative branch (only matters above CFG 1).
    let model = if p.fast {
        g["10"] = json!({"class_type": "LoraLoaderModelOnly",
            "inputs": {"model": ["1", 0], "lora_name": files.lora, "strength_model": 1.0}});
        g["11"] = json!({"class_type": "LoraLoaderModelOnly",
            "inputs": {"model": ["10", 0], "lora_name": TIME_DELTA_NAME, "strength_model": 1.0}});
        json!(["11", 0])
    } else {
        g["12"] = json!({"class_type": "SkipLayerGuidanceDiTSimple", "inputs": {
            "model": ["1", 0], "double_layers": "", "single_layers": SKIP_LAYER,
            "start_percent": 0.0, "end_percent": 1.0}});
        json!(["12", 0])
    };
    g["7"] = json!({"class_type": "KSampler", "inputs": {
        "seed": p.seed, "steps": p.steps, "cfg": p.cfg,
        "sampler_name": p.sampler, "scheduler": p.scheduler, "denoise": 1.0,
        "model": model, "positive": ["4", 0], "negative": ["5", 0],
        "latent_image": ["6", 0]}});
    g
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
            SidecarError::Http(400) => "The GPU didn't accept the request. If the model files \
                are still loading, wait a minute and try again."
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

/// One finished picture as the window shows it.
#[derive(Debug, Clone, Serialize)]
pub struct MossImage {
    pub id: String,
    pub name: String,
    pub data_url: String,
    /// What made it (size, mode, steps, sampler, seed).
    pub info: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MossView {
    pub results: Vec<MossImage>,
    pub phase: Phase,
    pub message: String,
}

#[derive(Default)]
struct Inner {
    /// Each picture with its caption.
    results: Vec<(Kept, String)>,
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
    params: Params,
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
            results: i
                .results
                .iter()
                .rev()
                .map(|(k, info)| {
                    let v = k.view();
                    MossImage {
                        id: v.id,
                        name: v.name,
                        data_url: v.data_url,
                        info: info.clone(),
                    }
                })
                .collect(),
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
    pub fn begin(&self, request: &Request, random_seed: u64) -> Result<Begun, String> {
        let params = Params::check(request, random_seed)?;
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
        Ok(Begun { params, cancel })
    }

    /// Run a request [`begin`](Self::begin) accepted, and record how it ended.
    pub async fn finish(self: &Arc<Self>, t: &Target, b: Begun) {
        let graph = build_graph(&t.files, &b.params);
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
                    i.results
                        .push((Kept { id, name, bytes }, b.params.summary()));
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
            let (r, _) = i
                .results
                .iter()
                .find(|(r, _)| r.id == result_id)
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

    fn request() -> Request {
        Request {
            prompt: "a red fox".into(),
            width: 1024,
            height: 896,
            steps: 8,
            sampler: "euler".into(),
            scheduler: "simple".into(),
            cfg: None,
            fast: true,
            seed: Some(42),
        }
    }

    fn params(r: &Request) -> Params {
        Params::check(r, 7).unwrap()
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
    fn fast_graph_matches_the_native_comfyui_nodes() {
        let g = build_graph(&files(), &params(&request()));
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
        assert_eq!(k["scheduler"], "simple");
        // Base -> distillation LoRA -> time-embedding delta -> sampler.
        assert_eq!(g["10"]["inputs"]["model"], json!(["1", 0]));
        assert_eq!(g["11"]["inputs"]["model"], json!(["10", 0]));
        assert_eq!(g["11"]["inputs"]["lora_name"], TIME_DELTA_NAME);
        assert_eq!(k["model"], json!(["11", 0]));
        assert!(g.get("12").is_none());
        assert_eq!(g["9"]["inputs"]["images"], json!(["8", 0]));
    }

    #[test]
    fn full_quality_graph_skips_the_distillation_and_layer_22_in_the_negative() {
        let mut r = request();
        r.fast = false;
        r.steps = 50;
        r.cfg = Some(3.5);
        r.sampler = "dpmpp_2m".into();
        r.scheduler = "karras".into();
        let g = build_graph(&files(), &params(&r));
        assert!(g.get("10").is_none() && g.get("11").is_none());
        let slg = &g["12"];
        assert_eq!(slg["class_type"], "SkipLayerGuidanceDiTSimple");
        assert_eq!(slg["inputs"]["model"], json!(["1", 0]));
        assert_eq!(slg["inputs"]["single_layers"], "22");
        assert_eq!(slg["inputs"]["double_layers"], "");
        let k = &g["7"]["inputs"];
        assert_eq!(k["model"], json!(["12", 0]));
        assert_eq!(
            (k["steps"].as_u64(), k["cfg"].as_f64()),
            (Some(50), Some(3.5))
        );
        assert_eq!(
            (&k["sampler_name"], &k["scheduler"]),
            (&json!("dpmpp_2m"), &json!("karras"))
        );
    }

    #[test]
    fn every_link_points_at_a_node_in_the_graph() {
        for fast in [true, false] {
            let mut r = request();
            r.fast = fast;
            let g = build_graph(&files(), &params(&r));
            for (id, node) in g.as_object().unwrap() {
                for (name, v) in node["inputs"].as_object().unwrap() {
                    if let Some([src, _]) = v.as_array().map(Vec::as_slice) {
                        let src = src.as_str().unwrap();
                        assert!(g.get(src).is_some(), "node {id}.{name} -> missing {src}");
                    }
                }
            }
        }
    }

    #[test]
    fn steps_are_checked_per_mode() {
        let mut r = request();
        for (fast, steps, ok) in [
            (true, 1, true),
            (true, 20, true),
            (true, 21, false),
            (true, 0, false),
            (false, 60, true),
            (false, 61, false),
        ] {
            r.fast = fast;
            r.steps = steps;
            assert_eq!(
                Params::check(&r, 1).is_ok(),
                ok,
                "fast={fast} steps={steps}"
            );
        }
    }

    #[test]
    fn sampler_scheduler_and_cfg_are_checked() {
        let mut r = request();
        r.sampler = "evil; rm -rf".into();
        assert!(Params::check(&r, 1).is_err());
        let mut r = request();
        r.scheduler = "nope".into();
        assert!(Params::check(&r, 1).is_err());
        // Fast mode ignores any CFG and always runs at 1.
        let mut r = request();
        r.cfg = Some(7.0);
        assert_eq!(params(&r).cfg, 1.0);
        // Full quality defaults to the card's CFG and rejects wild values.
        r.fast = false;
        r.steps = 30;
        r.cfg = None;
        assert_eq!(params(&r).cfg, FULL_DEFAULT_CFG);
        for bad in [0.5, 10.5, f64::NAN] {
            r.cfg = Some(bad);
            assert!(Params::check(&r, 1).is_err(), "{bad}");
        }
        assert!(SAMPLERS.contains(&"euler") && SCHEDULERS.contains(&"simple"));
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
        for ok in [(1024, 1024), (896, 1152), (256, 2048), (1536, 1536)] {
            assert!(valid_size(ok), "{ok:?}");
        }
        for bad in [
            (1000, 1024),
            (128, 1024),
            (4096, 1024),
            (1024, 0),
            (2064, 1024),
        ] {
            assert!(!valid_size(bad), "{bad:?}");
        }
        let mut r = request();
        r.width = 1000;
        assert!(Params::check(&r, 1).is_err());
    }

    #[test]
    fn seeds_are_kept_exactly_or_made_up() {
        let mut r = request();
        assert_eq!(params(&r).seed, 42);
        r.seed = None;
        assert_eq!(params(&r).seed, 7);
        // A seed too big for a JS number is folded into 52 bits.
        r.seed = Some(u64::MAX);
        assert_eq!(params(&r).seed, (1 << 52) - 1);
    }

    #[test]
    fn the_caption_says_what_made_the_picture() {
        assert_eq!(
            params(&request()).summary(),
            "1024×896 · Fast · 8 steps · euler/simple · seed 42"
        );
        let mut r = request();
        r.fast = false;
        r.steps = 50;
        r.cfg = Some(3.0);
        assert_eq!(
            params(&r).summary(),
            "1024×896 · Full quality, CFG 3 · 50 steps · euler/simple · seed 42"
        );
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
        let mut r = request();
        r.prompt = "  a fox ".into();
        let b = svc.begin(&r, 1).unwrap();
        assert_eq!(b.params.prompt, "a fox");
        assert_eq!(svc.view().phase, Phase::Running);
        assert!(notified.load(Ordering::SeqCst) >= 1);
        let again = svc.begin(&request(), 1);
        assert_eq!(
            again.err().as_deref(),
            Some("An image is already being made.")
        );
        // A finished run frees the panel again.
        svc.set_phase(Phase::Done, "ok");
        assert!(svc.begin(&request(), 1).is_ok());
    }

    #[test]
    fn a_bad_request_never_marks_the_panel_busy() {
        let (svc, _) = service();
        let mut r = request();
        r.prompt = "   ".into();
        assert!(svc.begin(&r, 1).is_err());
        r = request();
        r.steps = 99;
        assert!(svc.begin(&r, 1).is_err());
        assert_eq!(svc.view().phase, Phase::Idle);
    }

    #[test]
    fn results_show_newest_first_with_captions_and_reset_keeps_them() {
        let (svc, _) = service();
        {
            let mut i = svc.inner.lock().unwrap();
            i.results.push((kept("a"), "first".into()));
            i.results.push((kept("b"), "second".into()));
        }
        let v = svc.view();
        let got: Vec<(&str, &str)> = v
            .results
            .iter()
            .map(|r| (r.id.as_str(), r.info.as_str()))
            .collect();
        assert_eq!(got, [("b", "second"), ("a", "first")]);
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
        svc.inner
            .lock()
            .unwrap()
            .results
            .push((kept("a"), String::new()));
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
