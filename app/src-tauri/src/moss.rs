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

use std::path::Path;
pub mod layout;
pub mod probe;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::catalog::{is_image, Model};
use crate::comfy::parse_history;
use crate::identity::{error_text, write_image, Kept, Phase, MAX_POLL_FAILURES, MAX_RESULTS, POLL};
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

/// A user LoRA's strength in the graph (negative values are not offered).
pub const MIN_LORA_STRENGTH: f64 = 0.0;
pub const MAX_LORA_STRENGTH: f64 = 2.0;
pub const DEFAULT_LORA_STRENGTH: f64 = 1.0;
/// The sidecar's cap (`LORA_MAX_BYTES`); a rank-32 export is about 200 MB.
pub const MAX_LORA_BYTES: u64 = 1024 * 1024 * 1024;
/// LoRAs one GPU takes in a session (Phase R's limit; the sidecar's `LORA_MAX_FILES`).
pub const MAX_LORAS_PER_GPU: usize = 8;
/// Where the sidecar puts uploaded LoRAs, as ComfyUI names them.
pub const USER_LORA_DIR: &str = "user/";

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
    /// Fast mode also loads the time-embedding delta (`TIME_DELTA_NAME`). True when
    /// the entry lists a raw `time_embedding` file, which `provision.sh` turns
    /// into that delta. Lodestones' original distillation needs it; Kromoss's
    /// DMD2 LoRA does not, and loading the delta there would change the model.
    pub time_delta: bool,
}

impl ModelFiles {
    /// From a ComfyUI catalog entry for MOSS-Image or a fine-tune of it, by file
    /// `kind`. A `time_embedding` file is optional and decides `time_delta`.
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
        Some(Self {
            unet: by("main")?,
            clip: by("text_encoder")?,
            vae: by("vae")?,
            lora: by("lora")?,
            time_delta: by("time_embedding").is_some(),
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
    /// A LoRA already put on the GPU (`MossLora::name`), applied on top of the model.
    #[serde(default)]
    pub lora: Option<String>,
    /// Blank = 1.0.
    #[serde(default)]
    pub lora_strength: Option<f64>,
    /// With a LoRA: also make the same picture without it (same seed), first.
    #[serde(default)]
    pub control: bool,
    /// Layout mode (Fast only): boxes, each optionally with its own LoRA from the GPU's
    /// uploads. The picture is prompted by the layout, so `prompt` is ignored, and `lora`
    /// and `control` aren't used with it (the LoRAs are per box).
    #[serde(default)]
    pub layout: Option<layout::LayoutRequest>,
}

/// A checked [`Request`]: every value is one the graph can use.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// What MOSS reads: the typed prompt, or the layout JSON in Layout mode.
    pub prompt: String,
    pub size: (u32, u32),
    pub steps: u32,
    pub sampler: String,
    pub scheduler: String,
    pub cfg: f64,
    pub fast: bool,
    pub seed: u64,
    /// A user LoRA and its strength.
    pub lora: Option<(String, f64)>,
    /// Make the picture without the LoRA as well, before the one with it.
    pub control: bool,
    /// This is that control picture (only for the caption).
    pub is_control: bool,
    /// Layout mode: the checked boxes and their LoRA passes.
    pub layout: Option<layout::Layout>,
}

/// ComfyUI takes 64-bit seeds; keep them in what a JS number holds exactly.
const SEED_MASK: u64 = (1 << 52) - 1;

impl Params {
    /// Check a request. `random_seed` is used when the request has none.
    pub fn check(r: &Request, random_seed: u64) -> Result<Self, String> {
        let size = (r.width, r.height);
        if !valid_size(size) {
            return Err(format!(
                "The size must be a multiple of 16 between {MIN_SIDE} and {MAX_SIDE} on each side."
            ));
        }
        let (prompt, layout) = match &r.layout {
            None => (clean_prompt(&r.prompt)?, None),
            Some(_) if !r.fast => {
                return Err("Layout needs Fast mode.".into());
            }
            Some(_) if r.lora.is_some() || r.control => {
                return Err("In Layout mode, each box has its own LoRA.".into());
            }
            Some(l) => {
                let l = layout::check(l, size)?;
                (l.json.clone(), Some(l))
            }
        };
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
        let lora = match &r.lora {
            None => None,
            Some(name) => {
                if !valid_lora_name(name) {
                    return Err("That LoRA isn't one of the files on this GPU.".into());
                }
                let strength = r.lora_strength.unwrap_or(DEFAULT_LORA_STRENGTH);
                if !(MIN_LORA_STRENGTH..=MAX_LORA_STRENGTH).contains(&strength) {
                    return Err(format!(
                        "The LoRA strength must be between {MIN_LORA_STRENGTH} and {MAX_LORA_STRENGTH}."
                    ));
                }
                Some((name.clone(), strength))
            }
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
            control: r.control && lora.is_some(),
            lora,
            is_control: false,
            layout,
        })
    }

    /// The longest this picture may take before the run is called stuck.
    pub fn run_timeout(&self) -> Duration {
        layout::run_timeout(self.layout.as_ref().map_or(0, |l| l.passes.len()))
    }

    /// The same picture without the LoRA.
    pub fn control_variant(&self) -> Self {
        Self {
            lora: None,
            control: false,
            is_control: true,
            ..self.clone()
        }
    }

    /// One line for the picture's caption: what made it.
    pub fn summary(&self) -> String {
        let mode = if self.fast {
            "Fast".to_string()
        } else {
            format!("Full quality, CFG {}", self.cfg)
        };
        let lora = match (&self.lora, self.is_control) {
            (Some((name, s)), _) => format!(" · LoRA {} ×{s}", lora_label(name)),
            (None, true) => " · base control (no LoRA)".to_string(),
            (None, false) => String::new(),
        };
        let layout = self
            .layout
            .as_ref()
            .map_or(String::new(), |l| format!(" · {}", l.summary()));
        format!(
            "{}×{} · {} · {} steps · {}/{} · seed {}{lora}{layout}",
            self.size.0, self.size.1, mode, self.steps, self.sampler, self.scheduler, self.seed
        )
    }
}

/// `user/<file>.safetensors` with a plain file name, as the sidecar stores it.
pub fn valid_lora_name(name: &str) -> bool {
    let Some(file) = name.strip_prefix(USER_LORA_DIR) else {
        return false;
    };
    let Some(stem) = file.strip_suffix(".safetensors") else {
        return false;
    };
    let mut chars = stem.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && file.len() <= 96 + ".safetensors".len()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The file name shown for a LoRA: no folder, no extension.
pub(crate) fn lora_label(name: &str) -> &str {
    let file = name.strip_prefix(USER_LORA_DIR).unwrap_or(name);
    file.strip_suffix(".safetensors").unwrap_or(file)
}

/// A LoRA file name the sidecar accepts: the user's own name, made plain, and
/// the checksum's start so two different files never share a name.
pub fn lora_file_name(stem: &str, sha256: &str) -> String {
    let plain: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut plain: String = plain
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take(60)
        .collect();
    if plain.is_empty() {
        plain = "lora".into();
    }
    format!("{plain}-{}.safetensors", &sha256[..8])
}

/// A safetensors file starts with its header's length (8 bytes, little endian)
/// and a JSON object of that length. This is a sanity check, not a safety proof:
/// the sidecar repeats it and the loader reads tensors, never code.
pub fn check_safetensors(bytes: &[u8]) -> Result<(), String> {
    let bad = || "That doesn't look like a LoRA file (.safetensors).".to_string();
    let len: [u8; 8] = bytes
        .get(..8)
        .ok_or_else(bad)?
        .try_into()
        .map_err(|_| bad())?;
    let n = usize::try_from(u64::from_le_bytes(len)).map_err(|_| bad())?;
    if !(2..=64 * 1024 * 1024).contains(&n) {
        return Err(bad());
    }
    let header = bytes.get(8..8 + n).ok_or_else(bad)?;
    match serde_json::from_slice::<Value>(header) {
        Ok(Value::Object(_)) => Ok(()),
        _ => Err(bad()),
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
    // A user LoRA goes straight on the base model, under the distillation.
    let base = match &p.lora {
        Some((name, strength)) => {
            g["13"] = json!({"class_type": "LoraLoaderModelOnly",
                "inputs": {"model": ["1", 0], "lora_name": name, "strength_model": strength}});
            json!(["13", 0])
        }
        None => json!(["1", 0]),
    };
    // Fast: base -> distillation LoRA (-> time-embedding delta, when the model has one).
    // Full: the base, with layer 22 skipped in the negative branch (only matters
    // above CFG 1).
    let model = if p.fast {
        g["10"] = json!({"class_type": "LoraLoaderModelOnly",
            "inputs": {"model": base, "lora_name": files.lora, "strength_model": 1.0}});
        if files.time_delta {
            g["11"] = json!({"class_type": "LoraLoaderModelOnly",
                "inputs": {"model": ["10", 0], "lora_name": TIME_DELTA_NAME, "strength_model": 1.0}});
            json!(["11", 0])
        } else {
            json!(["10", 0])
        }
    } else {
        g["12"] = json!({"class_type": "SkipLayerGuidanceDiTSimple", "inputs": {
            "model": base, "double_layers": "", "single_layers": SKIP_LAYER,
            "start_percent": 0.0, "end_percent": 1.0}});
        json!(["12", 0])
    };
    g["7"] = json!({"class_type": "KSampler", "inputs": {
        "seed": p.seed, "steps": p.steps, "cfg": p.cfg,
        "sampler_name": p.sampler, "scheduler": p.scheduler, "denoise": 1.0,
        "model": model, "positive": ["4", 0], "negative": ["5", 0],
        "latent_image": ["6", 0]}});
    // Layout mode: the prompt above is the layout JSON; add a masked pass per LoRA box.
    if let Some(l) = &p.layout {
        layout::extend_graph(&mut g, p, l);
    }
    g
}

/// Queue one graph on the instance, wait for it, and fetch the picture. The
/// graph and the wait loop are the same shape as Identity Edit's. `Ok(None)`
/// means it was cancelled; `on_queued` gets the prompt id once ComfyUI has it. `timeout`
/// is how long to wait before calling the run stuck ([`Params::run_timeout`]).
pub async fn run(
    sidecar: &dyn SidecarApi,
    base: &Url,
    secret: &str,
    graph: &Value,
    cancel: &AtomicBool,
    timeout: Duration,
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
        if started.elapsed() > timeout {
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

/// A LoRA the app put on this GPU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MossLora {
    /// What `Request::lora` takes (`user/<file>`).
    pub name: String,
    /// What the panel shows.
    pub label: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MossView {
    pub results: Vec<MossImage>,
    pub phase: Phase,
    pub message: String,
    pub loras: Vec<MossLora>,
}

#[derive(Default)]
struct Inner {
    /// Each picture with its caption.
    results: Vec<(Kept, String)>,
    phase: Phase,
    message: String,
    cancel: Option<Arc<AtomicBool>>,
    prompt_id: Option<String>,
    /// LoRAs uploaded to the current GPU, oldest first.
    loras: Vec<MossLora>,
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
    /// The pictures to make, in order (a base control comes first).
    runs: Vec<Params>,
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
            loras: i.loras.clone(),
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
            // A new GPU starts without the old one's uploads.
            i.loras.clear();
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
            if let Some((name, _)) = &params.lora {
                if !i.loras.iter().any(|l| &l.name == name) {
                    return Err("Add that LoRA to this GPU first.".into());
                }
            }
            if let Some(l) = &params.layout {
                let known: Vec<String> = i.loras.iter().map(|x| x.name.clone()).collect();
                l.check_known_loras(&known)?;
            }
            i.phase = Phase::Running;
            i.message = match &params.layout {
                Some(l) => format!(
                    "Making your picture: the layout, then {} LoRA region(s), about {} seconds \
                     once the model is loaded…",
                    l.passes.len(),
                    l.estimate_secs()
                ),
                None => {
                    "Making your image (the first one takes longer while the model loads)…".into()
                }
            };
            i.cancel = Some(cancel.clone());
            i.prompt_id = None;
        }
        (self.notify)();
        let mut runs = Vec::new();
        if params.control {
            runs.push(params.control_variant());
        }
        runs.push(params);
        Ok(Begun { runs, cancel })
    }

    /// Put a LoRA file from the user's disk onto the GPU so a picture can use it.
    /// The file is checked here (a safetensors header, a size cap) and again by
    /// the sidecar, which also verifies the checksum.
    pub async fn add_lora(&self, t: &Target, path: &Path) -> Result<MossLora, String> {
        let meta = std::fs::metadata(path).map_err(|_| "Couldn't read that file.".to_string())?;
        if meta.len() > MAX_LORA_BYTES {
            return Err("That file is too big for a LoRA (the limit is 1 GB).".into());
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_st = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("safetensors"));
        if !is_st {
            return Err("Choose a .safetensors LoRA file.".into());
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|_| "Couldn't read that file.".to_string())?;
        check_safetensors(&bytes)?;
        // ComfyUI would load a LoRA for another model without a word and then ignore it, so
        // look at the tensor names before spending time on the upload.
        let probe = probe::probe(&bytes)?;
        if let Some(why) = probe.refusal() {
            return Err(why);
        }
        let sha = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        };
        let file = lora_file_name(&stem, &sha);
        {
            // Before the upload, so a few hundred MB aren't sent for nothing. The same file
            // again is fine: it keeps its name.
            let i = self.inner.lock().unwrap();
            let known = i
                .loras
                .iter()
                .any(|l| l.name == format!("{USER_LORA_DIR}{file}"));
            if i.loras.len() >= MAX_LORAS_PER_GPU && !known {
                return Err(format!(
                    "This GPU already holds the {MAX_LORAS_PER_GPU} LoRAs it takes. Stop it and \
                     start a new one to use others."
                ));
            }
        }
        let size = bytes.len() as u64;
        let answer = t
            .sidecar
            .lora_upload(&t.base, &t.secret, &file, &sha, bytes)
            .await
            .map_err(|e| match e {
                SidecarError::Unauthorized => {
                    "The GPU rejected this app's login. Stop it and start again.".to_string()
                }
                SidecarError::Http(404) => {
                    "This GPU can't take LoRAs. Stop it and start a new one.".to_string()
                }
                SidecarError::Http(409) => {
                    "This GPU already holds as many LoRAs as it takes.".to_string()
                }
                SidecarError::Http(413) => "That file is too big for a LoRA.".to_string(),
                SidecarError::Http(422) => {
                    "The GPU says that file isn't a valid LoRA, or it was damaged on the way."
                        .to_string()
                }
                e => format!("Couldn't reach the GPU: {e}"),
            })?;
        let name = answer["name"].as_str().unwrap_or_default().to_string();
        if !valid_lora_name(&name) {
            return Err("The GPU named that file in a way this app doesn't accept.".into());
        }
        let lora = MossLora {
            label: lora_label(&name).to_string(),
            name,
            bytes: size,
        };
        {
            let mut i = self.inner.lock().unwrap();
            i.loras.retain(|l| l.name != lora.name);
            i.loras.push(lora.clone());
            // A LoRA that only partly fits is added, and the panel says so.
            if let Some(note) = probe.note() {
                i.message = note;
            }
        }
        (self.notify)();
        Ok(lora)
    }

    /// Run a request [`begin`](Self::begin) accepted, and record how it ended.
    pub async fn finish(self: &Arc<Self>, t: &Target, b: Begun) {
        let total = b.runs.len();
        for (n, params) in b.runs.iter().enumerate() {
            if n > 0 {
                let mut i = self.inner.lock().unwrap();
                i.prompt_id = None;
                i.message = format!("Making image {} of {total}…", n + 1);
                drop(i);
                (self.notify)();
            }
            let graph = build_graph(&t.files, params);
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
                params.run_timeout(),
                &on_queued,
            )
            .await;
            match outcome {
                Ok(Some((id, name, bytes))) => {
                    let mut i = self.inner.lock().unwrap();
                    i.results.push((Kept { id, name, bytes }, params.summary()));
                    let extra = i.results.len().saturating_sub(MAX_RESULTS);
                    i.results.drain(..extra);
                }
                Ok(None) => return self.set_phase(Phase::Cancelled, "Cancelled."),
                Err(e) => return self.set_phase(Phase::Failed, e),
            }
        }
        self.set_phase(
            Phase::Done,
            if total > 1 {
                "Done. Both images are saved to your output folder; the one without the LoRA \
                 is the base control."
            } else {
                "Done. The image is saved to your output folder."
            },
        );
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
            time_delta: true,
        }
    }

    fn bundled_moss() -> Model {
        bundled("moss-image-dmd")
    }

    fn bundled(id: &str) -> Model {
        crate::catalog::bundled()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap()
    }

    fn kromoss_files() -> ModelFiles {
        ModelFiles {
            unet: "kromoss_bf16.safetensors".into(),
            clip: "qwen3.5_9b_int8_convrot.safetensors".into(),
            vae: "flux2-vae_small.safetensors".into(),
            lora: "kromoss-8-steps-dmd2-lora.safetensors".into(),
            time_delta: false,
        }
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
            lora: None,
            lora_strength: None,
            control: false,
            layout: None,
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
        // The raw time-embedding file decides whether the delta is loaded: the
        // original distillation has one, Kromoss's DMD2 LoRA does not.
        assert!(ModelFiles::of(&bundled_moss()).unwrap().time_delta);
        assert_eq!(ModelFiles::of(&bundled("kromoss")), Some(kromoss_files()));
        let mut m = bundled_moss();
        m.files.retain(|f| f.kind != "time_embedding");
        assert!(!ModelFiles::of(&m).unwrap().time_delta);
        // The four real files are still required.
        for kind in ["main", "text_encoder", "vae", "lora"] {
            let mut m = bundled("kromoss");
            m.files.retain(|f| f.kind != kind);
            assert_eq!(ModelFiles::of(&m), None, "{kind}");
        }
    }

    #[test]
    fn kromoss_graph_has_the_dmd2_lora_and_no_time_delta() {
        let g = build_graph(&kromoss_files(), &params(&request()));
        assert_eq!(
            g["10"]["inputs"]["lora_name"],
            "kromoss-8-steps-dmd2-lora.safetensors"
        );
        assert!(g.get("11").is_none());
        assert!(!g.to_string().contains(TIME_DELTA_NAME));
        assert_eq!(g["7"]["inputs"]["model"], json!(["10", 0]));
        assert_eq!(g["1"]["inputs"]["unet_name"], "kromoss_bf16.safetensors");
        assert_eq!(g["3"]["inputs"]["vae_name"], "flux2-vae_small.safetensors");
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
        for files in [files(), kromoss_files()] {
            for fast in [true, false] {
                let mut r = request();
                r.fast = fast;
                let g = build_graph(&files, &params(&r));
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
        assert_eq!(b.runs[0].prompt, "a fox");
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

    const LORA: &str = "user/km-mix-800-0a1b2c3d.safetensors";

    fn lora_request(fast: bool) -> Request {
        Request {
            fast,
            steps: if fast { 8 } else { 30 },
            lora: Some(LORA.into()),
            lora_strength: Some(0.8),
            ..request()
        }
    }

    fn safetensors_bytes() -> Vec<u8> {
        let header = br#"{"__metadata__":{}}"#;
        let mut v = (header.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(header);
        v.extend_from_slice(b"weights");
        v
    }

    #[test]
    fn a_user_lora_goes_on_the_base_model_under_the_distillation() {
        let g = build_graph(&kromoss_files(), &params(&lora_request(true)));
        assert_eq!(g["13"]["class_type"], "LoraLoaderModelOnly");
        assert_eq!(g["13"]["inputs"]["lora_name"], LORA);
        assert_eq!(g["13"]["inputs"]["strength_model"], 0.8);
        assert_eq!(g["13"]["inputs"]["model"], json!(["1", 0]));
        // base -> user LoRA -> distillation LoRA -> sampler
        assert_eq!(g["10"]["inputs"]["model"], json!(["13", 0]));
        assert_eq!(g["7"]["inputs"]["model"], json!(["10", 0]));
        // The original distillation keeps its delta after both.
        let g = build_graph(&files(), &params(&lora_request(true)));
        assert_eq!(g["10"]["inputs"]["model"], json!(["13", 0]));
        assert_eq!(g["11"]["inputs"]["model"], json!(["10", 0]));
    }

    #[test]
    fn full_quality_applies_the_user_lora_before_layer_skipping() {
        let g = build_graph(&files(), &params(&lora_request(false)));
        assert_eq!(g["13"]["inputs"]["model"], json!(["1", 0]));
        assert_eq!(g["12"]["inputs"]["model"], json!(["13", 0]));
        assert_eq!(g["7"]["inputs"]["model"], json!(["12", 0]));
    }

    #[test]
    fn without_a_lora_the_graph_is_unchanged() {
        for fast in [true, false] {
            let mut r = request();
            r.fast = fast;
            r.steps = 8;
            let g = build_graph(&files(), &params(&r));
            assert!(g.get("13").is_none());
            assert!(!g.to_string().contains(USER_LORA_DIR));
        }
    }

    #[test]
    fn lora_links_resolve_in_every_mode() {
        for files in [files(), kromoss_files()] {
            for fast in [true, false] {
                let g = build_graph(&files, &params(&lora_request(fast)));
                for (id, node) in g.as_object().unwrap() {
                    for (name, v) in node["inputs"].as_object().unwrap() {
                        if let Some([src, _]) = v.as_array().map(Vec::as_slice) {
                            assert!(g.get(src.as_str().unwrap()).is_some(), "{id}.{name}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn lora_names_and_strengths_are_checked() {
        for bad in [
            "x.safetensors",
            "user/x.ckpt",
            "user/../x.safetensors",
            "user/a/b.safetensors",
            "user/.hidden.safetensors",
            "user/a b.safetensors",
            "user/.safetensors",
            "../user/x.safetensors",
        ] {
            let mut r = lora_request(true);
            r.lora = Some(bad.into());
            assert!(Params::check(&r, 1).is_err(), "{bad}");
        }
        for bad in [-0.1, 2.5, f64::NAN] {
            let mut r = lora_request(true);
            r.lora_strength = Some(bad);
            assert!(Params::check(&r, 1).is_err(), "{bad}");
        }
        let mut r = lora_request(true);
        r.lora_strength = None;
        assert_eq!(params(&r).lora, Some((LORA.into(), 1.0)));
        r.lora_strength = Some(0.0);
        assert_eq!(params(&r).lora, Some((LORA.into(), 0.0)));
    }

    #[test]
    fn the_control_is_the_same_picture_without_the_lora() {
        let mut r = lora_request(true);
        r.control = true;
        let p = params(&r);
        let c = p.control_variant();
        assert_eq!(
            (c.seed, c.prompt.clone(), c.size),
            (p.seed, p.prompt.clone(), p.size)
        );
        assert_eq!(c.lora, None);
        assert!(p.summary().contains("LoRA km-mix-800-0a1b2c3d ×0.8"));
        assert!(c.summary().contains("base control (no LoRA)"));
        assert!(!c.summary().contains("LoRA km"));
        // A control without a LoRA means nothing.
        let mut r = request();
        r.control = true;
        assert!(!params(&r).control);
    }

    #[test]
    fn begin_needs_the_lora_to_be_on_the_gpu_and_queues_the_control_first() {
        let (svc, _) = service();
        let mut r = lora_request(true);
        r.control = true;
        let err = svc.begin(&r, 1).err().unwrap();
        assert!(err.contains("Add that LoRA"), "{err}");
        assert_eq!(svc.view().phase, Phase::Idle);
        svc.inner.lock().unwrap().loras.push(MossLora {
            name: LORA.into(),
            label: "km-mix-800-0a1b2c3d".into(),
            bytes: 7,
        });
        let b = svc.begin(&r, 1).unwrap();
        assert_eq!(b.runs.len(), 2);
        assert!(b.runs[0].is_control && b.runs[0].lora.is_none());
        assert!(b.runs[1].lora.is_some() && !b.runs[1].is_control);
        assert_eq!(b.runs[0].seed, b.runs[1].seed);
        // Without the control box: one picture.
        svc.reset_run();
        svc.inner.lock().unwrap().loras.push(MossLora {
            name: LORA.into(),
            label: String::new(),
            bytes: 0,
        });
        r.control = false;
        assert_eq!(svc.begin(&r, 1).unwrap().runs.len(), 1);
    }

    #[test]
    fn a_new_gpu_forgets_the_uploaded_loras() {
        let (svc, _) = service();
        svc.inner.lock().unwrap().loras.push(MossLora {
            name: LORA.into(),
            label: "x".into(),
            bytes: 1,
        });
        assert_eq!(svc.view().loras.len(), 1);
        svc.reset_run();
        assert!(svc.view().loras.is_empty());
    }

    #[test]
    fn upload_names_are_plain_and_unique_per_content() {
        let sha = "0a1b2c3d".repeat(8);
        assert_eq!(
            lora_file_name("My LoRA (final) v2", &sha),
            "My-LoRA--final--v2-0a1b2c3d.safetensors"
        );
        assert_eq!(lora_file_name("___", &sha), "lora-0a1b2c3d.safetensors");
        assert_eq!(lora_file_name("", &sha), "lora-0a1b2c3d.safetensors");
        let long = lora_file_name(&"a".repeat(200), &sha);
        assert!(valid_lora_name(&format!("user/{long}")), "{long}");
        // Non-ASCII letters are replaced, not passed through.
        assert!(valid_lora_name(&format!(
            "user/{}",
            lora_file_name("角色", &sha)
        )));
    }

    #[test]
    fn only_a_safetensors_header_passes_the_local_check() {
        assert!(check_safetensors(&safetensors_bytes()).is_ok());
        let mut huge = u64::MAX.to_le_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        let mut short = 100u64.to_le_bytes().to_vec();
        short.extend_from_slice(b"{}");
        let mut array = 2u64.to_le_bytes().to_vec();
        array.extend_from_slice(b"[]");
        for bad in [&b""[..], b"GGUF....", &huge, &short, &array] {
            assert!(check_safetensors(bad).is_err());
        }
    }

    fn lora_target(mock: &crate::provider::mock::MockSidecar, secret: &str) -> (Target, Url) {
        let base = Url::parse("https://example.trycloudflare.com").unwrap();
        (
            Target {
                sidecar: Arc::new(mock.clone()),
                base: base.clone(),
                secret: secret.into(),
                files: kromoss_files(),
            },
            base,
        )
    }

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sloptweak-lora-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn add_lora_refuses_other_files_before_any_upload() {
        let mock = crate::provider::mock::MockProvider::new(
            crate::provider::mock::MockTimings::default(),
            vec![],
        )
        .sidecar();
        let (t, _) = lora_target(&mock, "s");
        let (svc, _) = service();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cases = [
            ("a.ckpt", safetensors_bytes()),
            ("b.safetensors", b"not a model".to_vec()),
        ];
        for (name, bytes) in cases {
            let p = temp_file(name, &bytes);
            assert!(rt.block_on(svc.add_lora(&t, &p)).is_err(), "{name}");
        }
        let missing = std::env::temp_dir().join("sloptweak-lora-nope.safetensors");
        assert!(rt.block_on(svc.add_lora(&t, &missing)).is_err());
        assert!(svc.view().loras.is_empty());
    }

    /// A safetensors file with just these tensor names (no data).
    fn named_tensors(names: &[&str]) -> Vec<u8> {
        let mut h = serde_json::Map::new();
        for n in names {
            h.insert(
                (*n).into(),
                json!({"dtype": "F32", "shape": [4, 8], "data_offsets": [0, 0]}),
            );
        }
        let head = serde_json::to_vec(&h).unwrap();
        let mut v = (head.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(&head);
        v
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn a_lora_for_another_model_is_refused_before_any_upload() {
        let mock = crate::provider::mock::MockProvider::new(
            crate::provider::mock::MockTimings::default(),
            vec![],
        )
        .sidecar();
        let (t, _) = lora_target(&mock, "s");
        let (svc, _) = service();
        let sdxl = temp_file(
            "sdxl-style.safetensors",
            &named_tensors(&["lora_unet_input_blocks_1_1_attn1_to_q.lora_down.weight"]),
        );
        let err = block_on(svc.add_lora(&t, &sdxl)).unwrap_err();
        assert!(err.contains("different model"), "{err}");
        let empty = temp_file("no-weights.safetensors", &named_tensors(&[]));
        let err = block_on(svc.add_lora(&t, &empty)).unwrap_err();
        assert!(err.contains("no LoRA weights"), "{err}");
        // Refused before the upload: nothing was listed and the mock never saw a file.
        assert!(svc.view().loras.is_empty());
    }

    #[test]
    fn the_ninth_lora_is_refused_before_the_upload_but_a_known_one_is_not() {
        let mock = crate::provider::mock::MockProvider::new(
            crate::provider::mock::MockTimings::default(),
            vec![],
        )
        .sidecar();
        let (t, _) = lora_target(&mock, "s");
        let (svc, _) = service();
        let bytes = probe::sample_lora_bytes();
        let file = temp_file("cap-test.safetensors", &bytes);
        let sha = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        };
        let own = format!("{USER_LORA_DIR}{}", lora_file_name("cap-test", &sha));
        {
            let mut i = svc.inner.lock().unwrap();
            for n in 0..MAX_LORAS_PER_GPU {
                i.loras.push(MossLora {
                    name: format!("{USER_LORA_DIR}other-{n}.safetensors"),
                    label: format!("other-{n}"),
                    bytes: 1,
                });
            }
        }
        // Full, and this file is new: refused with no upload (the mock has no instance, so
        // reaching it would have said "couldn't reach the GPU" instead).
        let err = block_on(svc.add_lora(&t, &file)).unwrap_err();
        assert!(err.contains("already holds the 8 LoRAs"), "{err}");
        // The same file already on the GPU gets past the cap check (and only then fails to
        // reach the mock GPU, which has no instance here).
        svc.inner.lock().unwrap().loras[0] = MossLora {
            name: own,
            label: "cap-test".into(),
            bytes: 1,
        };
        let err = block_on(svc.add_lora(&t, &file)).unwrap_err();
        assert!(!err.contains("already holds"), "{err}");
    }
}
