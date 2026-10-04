//! Identity Edit: the panel's engine. One or two character sheets plus a
//! prompt become a ComfyUI graph (the one tested in the 2026-10-02 spike:
//! docs/findings.md, "Krea 2 edit spike"), run on the instance through the
//! sidecar. The window talks to this over Tauri commands; the instance only
//! ever sees the launch secret, never a browser (ComfyUI refuses requests that
//! carry an `Origin`, so a webview couldn't talk to it anyway).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::catalog::{is_image, Model};
use crate::comfy::parse_history;
use crate::sidecar::{SidecarApi, SidecarError, MAX_IMAGE_BYTES};

/// A reference sheet bigger than this is refused before it is sent.
pub const MAX_REF_BYTES: usize = 15 * 1024 * 1024;
/// Finished images kept in the panel (they are also saved to the output folder).
pub const MAX_RESULTS: usize = 6;
const MAX_PROMPT_CHARS: usize = 2000;
const POLL: Duration = Duration::from_secs(1);
/// A generation takes ~30-60 s; far longer means something is wrong.
const RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Consecutive "can't reach the GPU" answers tolerated while waiting.
const MAX_POLL_FAILURES: u32 = 8;

/// What the identity LoRA's training prompt wraps around the user's words
/// (the pack's own default; findings, "Krea 2 edit spike").
pub const PROMPT_PREFIX: &str = "Generate a new image based on the reference. Preserve the subject identity and key visual characteristics while applying this prompt: ";
/// The pack's recommended pull toward the reference (`ref_boost`).
const REF_BOOST: f64 = 4.0;
const STEPS: u32 = 10;

/// What the panel is doing: a new image from character sheets, or an edit of
/// a picture the user brings (docs/findings.md, "Edit spike, second live run":
/// the picture goes in as reference 1, an optional sheet as reference 2, and
/// the canvas takes the picture's own size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    New,
    Edit,
}

/// Slots 0 and 1 are character sheets (in Edit mode only slot 0 is used, as
/// the optional reference); slot 2 is the picture to edit.
pub const BASE_SLOT: usize = 2;
const SLOTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aspect {
    Square,
    Portrait,
    Landscape,
}

impl Aspect {
    /// Multiples of 64 near one megapixel.
    pub fn size(self) -> (u32, u32) {
        match self {
            Aspect::Square => (1024, 1024),
            Aspect::Portrait => (896, 1152),
            Aspect::Landscape => (1152, 896),
        }
    }
}

/// The ComfyUI file names the instance placed (the catalog's `filename`s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFiles {
    pub unet: String,
    pub clip: String,
    pub vae: String,
    pub lora: String,
}

impl ModelFiles {
    /// From a ComfyUI catalog entry, by file `kind`.
    pub fn of(m: &Model) -> Option<Self> {
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
        })
    }
}

/// Width and height of a PNG, JPEG or WebP, read from its header.
pub fn image_size(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| {
        b.get(i..i + 2)
            .map(|s| u32::from(s[0]) << 8 | u32::from(s[1]))
    };
    let le = |i: usize, n: usize| {
        b.get(i..i + n)
            .map(|s| s.iter().rev().fold(0u32, |a, &x| a << 8 | u32::from(x)))
    };
    let dims = if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let be32 = |i: usize| {
            b.get(i..i + 4)
                .map(|s| s.iter().fold(0u32, |a, &x| a << 8 | u32::from(x)))
        };
        (be32(16)?, be32(20)?)
    } else if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        loop {
            if *b.get(i)? != 0xFF {
                return None;
            }
            let marker = *b.get(i + 1)?;
            if marker == 0xFF {
                i += 1; // fill byte
                continue;
            }
            if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                break (be16(i + 7)?, be16(i + 5)?);
            }
            if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
                i += 2;
            } else {
                i += 2 + be16(i + 2)? as usize;
            }
        }
    } else if b.len() >= 25 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        match &b[12..16] {
            b"VP8 " => (le(26, 2)? & 0x3FFF, le(28, 2)? & 0x3FFF),
            b"VP8L" => {
                let v = le(21, 4)?;
                ((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1)
            }
            b"VP8X" => (le(24, 3)? + 1, le(27, 3)? + 1),
            _ => return None,
        }
    } else {
        return None;
    };
    (dims.0 > 0 && dims.1 > 0).then_some(dims)
}

/// The canvas for editing a picture of this size: about one megapixel with
/// the picture's shape, in multiples of 64 (another shape re-frames the scene).
pub fn fit_size(w: u32, h: u32) -> (u32, u32) {
    let scale = (1_048_576.0 / (f64::from(w) * f64::from(h))).sqrt();
    let snap = |v: u32| (((f64::from(v) * scale) / 64.0).round() as u32 * 64).clamp(512, 1536);
    (snap(w), snap(h))
}

/// The ComfyUI "API format" graph for one generation. `refs` are the file
/// names of the uploaded pictures (1 or 2, in order; for an edit, the picture
/// to edit comes first).
pub fn build_graph(
    files: &ModelFiles,
    refs: &[String],
    prompt: &str,
    size: (u32, u32),
    seed: u64,
) -> Value {
    let (w, h) = size;
    let mut g = json!({
        "1": {"class_type": "UNETLoader",
              "inputs": {"unet_name": files.unet, "weight_dtype": "default"}},
        "2": {"class_type": "CLIPLoader",
              "inputs": {"clip_name": files.clip, "type": "krea2", "device": "default"}},
        "3": {"class_type": "VAELoader", "inputs": {"vae_name": files.vae}},
        "11": {"class_type": "LoraLoaderModelOnly",
               "inputs": {"model": ["1", 0], "lora_name": files.lora, "strength_model": 1.0}},
        "6": {"class_type": "EmptySD3LatentImage",
              "inputs": {"width": w, "height": h, "batch_size": 1}},
    });
    let mut patch = json!({
        "model": ["11", 0], "ref_boost": REF_BOOST, "ref_boost_a": 1.0,
        "fit_mode": "fit", "vae": ["3", 0], "target_latent": ["6", 0],
    });
    let mut enc = json!({"clip": ["2", 0]});
    for (i, name) in refs.iter().take(2).enumerate() {
        let (load, latent) = (format!("2{i}"), format!("3{i}"));
        g[&load] = json!({"class_type": "LoadImage", "inputs": {"image": name}});
        g[&latent] = json!({"class_type": "VAEEncode",
                            "inputs": {"pixels": [&load, 0], "vae": ["3", 0]}});
        let suffix = if i == 0 { "" } else { "_b" };
        patch[format!("source_latent{suffix}")] = json!([&latent, 0]);
        patch[format!("source_image{suffix}")] = json!([&load, 0]);
        enc[if i == 0 { "image" } else { "image_b" }] = json!([&load, 0]);
    }
    g["12"] = json!({"class_type": "Krea2EditModelPatch", "inputs": patch});
    let encode = |text: String| {
        let mut inputs = enc.clone();
        inputs["prompt"] = json!(text);
        inputs["grounding_px"] = json!(768);
        inputs["system_prompt"] = json!("");
        json!({"class_type": "Krea2EditGroundedEncode", "inputs": inputs})
    };
    g["4"] = encode(format!("{PROMPT_PREFIX}{prompt}"));
    g["5"] = encode(String::new());
    g["7"] = json!({"class_type": "KSampler", "inputs": {
        "seed": seed, "steps": STEPS, "cfg": 1.0, "sampler_name": "euler",
        "scheduler": "simple", "denoise": 1.0,
        "model": ["12", 0], "positive": ["4", 0], "negative": ["5", 0],
        "latent_image": ["6", 0]}});
    g["8"] = json!({"class_type": "VAEDecode",
                    "inputs": {"samples": ["7", 0], "vae": ["3", 0]}});
    g["9"] = json!({"class_type": "SaveImage",
                    "inputs": {"filename_prefix": "SlopTweak", "images": ["8", 0]}});
    g
}

// ----- what the window shows -----------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Uploading,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl Phase {
    pub fn busy(self) -> bool {
        matches!(self, Phase::Uploading | Phase::Running)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageView {
    pub id: String,
    pub name: String,
    pub data_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityView {
    pub refs: Vec<Option<ImageView>>,
    pub results: Vec<ImageView>,
    pub phase: Phase,
    pub message: String,
}

#[derive(Debug)]
struct Kept {
    id: String,
    name: String,
    bytes: Vec<u8>,
}

impl Kept {
    fn view(&self) -> ImageView {
        let mime = if self.bytes.starts_with(b"\x89PNG") {
            "image/png"
        } else if self.bytes.starts_with(b"RIFF") {
            "image/webp"
        } else {
            "image/jpeg"
        };
        ImageView {
            id: self.id.clone(),
            name: self.name.clone(),
            data_url: format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&self.bytes)
            ),
        }
    }
}

#[derive(Default)]
struct Inner {
    refs: [Option<Kept>; SLOTS],
    results: Vec<Kept>,
    phase: Phase,
    message: String,
    cancel: Option<Arc<AtomicBool>>,
    prompt_id: Option<String>,
    counter: u64,
}

pub struct IdentityService {
    inner: Mutex<Inner>,
    /// Called after every change so the window can refresh.
    notify: Arc<dyn Fn() + Send + Sync>,
}

/// Where one run goes: the instance's tunnel, the launch secret, and the
/// ComfyUI file names to load.
pub struct Target {
    pub sidecar: Arc<dyn SidecarApi>,
    pub base: Url,
    pub secret: String,
    pub files: ModelFiles,
}

impl IdentityService {
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            notify,
        }
    }

    pub fn view(&self) -> IdentityView {
        let i = self.inner.lock().unwrap();
        IdentityView {
            refs: i.refs.iter().map(|r| r.as_ref().map(Kept::view)).collect(),
            results: i.results.iter().rev().map(Kept::view).collect(),
            phase: i.phase,
            message: i.message.clone(),
        }
    }

    /// A new session starts from a clean slate (the sheets stay on the PC).
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

    pub fn set_ref(&self, slot: usize, name: &str, bytes: Vec<u8>) -> Result<(), String> {
        if slot >= SLOTS {
            return Err("There are only three picture slots.".into());
        }
        if !is_image(&bytes) {
            return Err("That file isn't a PNG, JPEG or WebP image.".into());
        }
        if bytes.len() > MAX_REF_BYTES {
            return Err(format!(
                "That image is too big ({} MB; the limit is {} MB).",
                bytes.len() / 1_000_000,
                MAX_REF_BYTES / 1_000_000
            ));
        }
        {
            let mut i = self.inner.lock().unwrap();
            i.counter += 1;
            let id = format!("ref{}", i.counter);
            i.refs[slot] = Some(Kept {
                id,
                name: name.chars().take(80).collect(),
                bytes,
            });
        }
        (self.notify)();
        Ok(())
    }

    pub fn clear_ref(&self, slot: usize) {
        if let Some(r) = self.inner.lock().unwrap().refs.get_mut(slot) {
            *r = None;
        }
        (self.notify)();
    }

    /// Put a finished image in a reference slot ("keep going from this one").
    pub fn use_result(&self, result_id: &str, slot: usize) -> Result<(), String> {
        let found = {
            let i = self.inner.lock().unwrap();
            i.results
                .iter()
                .find(|r| r.id == result_id)
                .map(|r| (r.name.clone(), r.bytes.clone()))
        };
        let (name, bytes) = found.ok_or("That image is no longer in the panel.")?;
        self.set_ref(slot, &name, bytes)
    }

    fn set_phase(&self, phase: Phase, message: impl Into<String>) {
        {
            let mut i = self.inner.lock().unwrap();
            i.phase = phase;
            i.message = message.into();
        }
        (self.notify)();
    }

    fn finished(&self, phase: Phase, message: impl Into<String>) {
        {
            let mut i = self.inner.lock().unwrap();
            i.phase = phase;
            i.message = message.into();
            i.cancel = None;
            i.prompt_id = None;
        }
        (self.notify)();
    }

    /// Check the request and take the pictures (and work out the canvas);
    /// marks the service busy.
    #[allow(clippy::type_complexity)]
    fn begin(
        &self,
        prompt: &str,
        mode: Mode,
        aspect: Aspect,
    ) -> Result<(Vec<Kept>, (u32, u32), Arc<AtomicBool>), String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(match mode {
                Mode::New => "Describe the new scene first.".into(),
                Mode::Edit => "Describe what to change first.".into(),
            });
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(format!(
                "Keep the description under {MAX_PROMPT_CHARS} characters."
            ));
        }
        let mut i = self.inner.lock().unwrap();
        if i.phase.busy() {
            return Err("An image is already being made.".into());
        }
        let take = |r: &Option<Kept>| {
            r.as_ref().map(|r| Kept {
                id: r.id.clone(),
                name: r.name.clone(),
                bytes: r.bytes.clone(),
            })
        };
        let (refs, size): (Vec<Kept>, _) = match mode {
            Mode::New => {
                let refs: Vec<Kept> = i.refs[..2].iter().filter_map(take).collect();
                if refs.is_empty() {
                    return Err("Add at least one character sheet.".into());
                }
                (refs, aspect.size())
            }
            Mode::Edit => {
                let base =
                    take(&i.refs[BASE_SLOT]).ok_or("Add the picture you want to edit first.")?;
                // The picture's own shape; a header we can't read falls back to square.
                let size =
                    image_size(&base.bytes).map_or(Aspect::Square.size(), |(w, h)| fit_size(w, h));
                let mut refs = vec![base];
                refs.extend(take(&i.refs[0]));
                (refs, size)
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        i.cancel = Some(cancel.clone());
        i.phase = Phase::Uploading;
        i.message = "Sending your pictures…".into();
        drop(i);
        (self.notify)();
        Ok((refs, size, cancel))
    }

    /// Make one image. Returns quickly with an error if the request is
    /// invalid; otherwise runs to the end and records the outcome in the
    /// service's phase/message (the window reads those).
    pub async fn generate(
        self: &Arc<Self>,
        target: &Target,
        prompt: &str,
        mode: Mode,
        aspect: Aspect,
        seed: u64,
    ) -> Result<(), String> {
        let (refs, size, cancel) = self.begin(prompt, mode, aspect)?;
        let outcome = self
            .run(target, &refs, prompt.trim(), size, seed, &cancel)
            .await;
        match outcome {
            Ok(Some(kept)) => {
                {
                    let mut i = self.inner.lock().unwrap();
                    i.results.push(kept);
                    let extra = i.results.len().saturating_sub(MAX_RESULTS);
                    i.results.drain(..extra);
                }
                self.finished(
                    Phase::Done,
                    "Done. The image is saved to your output folder.",
                );
            }
            Ok(None) => self.finished(Phase::Cancelled, "Cancelled."),
            Err(e) => self.finished(Phase::Failed, e),
        }
        Ok(())
    }

    /// Upload, queue, wait, download. `Ok(None)` = cancelled.
    async fn run(
        &self,
        t: &Target,
        refs: &[Kept],
        prompt: &str,
        size: (u32, u32),
        seed: u64,
        cancel: &AtomicBool,
    ) -> Result<Option<Kept>, String> {
        let unreachable = |e: SidecarError| match e {
            SidecarError::Unauthorized => {
                "The GPU rejected this app's login. Stop it and start again.".to_string()
            }
            e => format!("Couldn't reach the GPU: {e}"),
        };
        // Names are unique per run so a re-run never reads an old upload.
        let tag = format!("{seed:x}");
        let mut names = Vec::new();
        for (i, r) in refs.iter().enumerate() {
            let ext = if r.bytes.starts_with(b"\x89PNG") {
                "png"
            } else if r.bytes.starts_with(b"RIFF") {
                "webp"
            } else {
                "jpg"
            };
            let name = format!("sloptweak_ref_{tag}_{i}.{ext}");
            let answer = t
                .sidecar
                .comfy_upload(
                    &t.base,
                    &t.secret,
                    &["upload", "image"],
                    &[
                        ("type", "input".to_string()),
                        ("overwrite", "true".to_string()),
                    ],
                    ("image", &name, r.bytes.clone()),
                )
                .await
                .map_err(unreachable)?;
            names.push(answer["name"].as_str().unwrap_or(&name).to_string());
            if cancel.load(Ordering::SeqCst) {
                return Ok(None);
            }
        }

        let graph = build_graph(&t.files, &names, prompt, size, seed);
        let queued = t
            .sidecar
            .comfy_post_json(
                &t.base,
                &t.secret,
                &["prompt"],
                &json!({"prompt": graph, "client_id": "sloptweak"}),
            )
            .await
            .map_err(|e| match e {
                SidecarError::Http(400) => "The GPU didn't accept the request. If the model \
                    files are still loading, wait a minute and try again."
                    .to_string(),
                e => unreachable(e),
            })?;
        let prompt_id = queued["prompt_id"]
            .as_str()
            .ok_or("The GPU didn't say what it started.")?
            .to_string();
        {
            self.inner.lock().unwrap().prompt_id = Some(prompt_id.clone());
        }
        self.set_phase(
            Phase::Running,
            "Making your image (usually under a minute; the first one takes longer while the model loads)…",
        );

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
            let history = match t
                .sidecar
                .comfy_json(&t.base, &t.secret, &["history", &prompt_id], &[])
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
            let status = entry["status"]["status_str"].as_str().unwrap_or("");
            if status == "error" {
                return Err(error_text(entry));
            }
            if status != "success" {
                continue;
            }
            let img = parse_history(&history)
                .into_iter()
                .next()
                .ok_or("The GPU finished but made no image.")?;
            let bytes = t
                .sidecar
                .comfy_bytes(
                    &t.base,
                    &t.secret,
                    &["view"],
                    &img.view_query(),
                    MAX_IMAGE_BYTES,
                )
                .await
                .map_err(unreachable)?;
            if !is_image(&bytes) {
                return Err("The GPU sent something that isn't an image.".into());
            }
            return Ok(Some(Kept {
                id: prompt_id,
                name: img.filename,
                bytes,
            }));
        }
    }

    /// Stop the current image: flag the wait loop, and tell ComfyUI.
    pub async fn cancel(&self, target: &Target) {
        let (flag, running) = {
            let i = self.inner.lock().unwrap();
            (i.cancel.clone(), i.prompt_id.is_some())
        };
        if let Some(flag) = flag {
            flag.store(true, Ordering::SeqCst);
        }
        if running {
            let _ = target
                .sidecar
                .comfy_post_json(&target.base, &target.secret, &["interrupt"], &json!({}))
                .await;
        }
    }
}

/// A short, plain version of ComfyUI's error for the user.
fn error_text(entry: &Value) -> String {
    let msg = entry["status"]["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m[0].as_str() == Some("execution_error"))
        .and_then(|m| m[1]["exception_message"].as_str())
        .unwrap_or("");
    if msg.to_ascii_lowercase().contains("out of memory") {
        return "The GPU ran out of memory. Try a smaller size or one sheet.".into();
    }
    let short: String = msg.lines().next().unwrap_or("").chars().take(160).collect();
    if short.is_empty() {
        "The GPU couldn't make that image.".into()
    } else {
        format!("The GPU couldn't make that image: {short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> ModelFiles {
        ModelFiles {
            unet: "wulver-v0.5-turbo.safetensors".into(),
            clip: "qwen3vl_4b_fp8_scaled.safetensors".into(),
            vae: "qwen_image_vae.safetensors".into(),
            lora: "krea2_identity_edit_v1_2.safetensors".into(),
        }
    }

    #[test]
    fn model_files_come_from_the_catalog_entry() {
        let m = crate::catalog::bundled()
            .into_iter()
            .find(|m| m.id == "wulver-identity-edit")
            .unwrap();
        assert_eq!(ModelFiles::of(&m), Some(files()));
        // An Invoke model has no LoRA of its own: not an Identity Edit model.
        let inv = crate::catalog::bundled()
            .into_iter()
            .find(|m| m.id == "wulver-turbo")
            .unwrap();
        assert_eq!(ModelFiles::of(&inv), None);
    }

    #[test]
    fn one_reference_graph_matches_the_tested_workflow() {
        let g = build_graph(
            &files(),
            &["a.png".to_string()],
            "sitting at a cafe",
            Aspect::Square.size(),
            7,
        );
        assert_eq!(
            g["1"]["inputs"]["unet_name"],
            "wulver-v0.5-turbo.safetensors"
        );
        assert_eq!(g["2"]["inputs"]["type"], "krea2");
        assert_eq!(
            g["11"]["inputs"]["lora_name"],
            "krea2_identity_edit_v1_2.safetensors"
        );
        assert_eq!(g["20"]["inputs"]["image"], "a.png");
        assert!(g.get("21").is_none() && g.get("31").is_none());
        let p = &g["12"]["inputs"];
        assert_eq!(g["12"]["class_type"], "Krea2EditModelPatch");
        assert_eq!(p["model"], json!(["11", 0]));
        assert_eq!(p["source_latent"], json!(["30", 0]));
        assert_eq!(p["source_image"], json!(["20", 0]));
        assert_eq!(p["ref_boost"], 4.0);
        assert_eq!(p["fit_mode"], "fit");
        assert_eq!(p["target_latent"], json!(["6", 0]));
        assert!(p.get("source_latent_b").is_none());
        let pos = &g["4"]["inputs"];
        assert_eq!(pos["image"], json!(["20", 0]));
        assert!(pos.get("image_b").is_none());
        assert_eq!(pos["prompt"], format!("{PROMPT_PREFIX}sitting at a cafe"));
        assert_eq!(g["5"]["inputs"]["prompt"], "");
        let k = &g["7"]["inputs"];
        assert_eq!(
            (k["steps"].as_u64(), k["seed"].as_u64()),
            (Some(10), Some(7))
        );
        assert_eq!(k["cfg"], 1.0);
        assert_eq!(k["sampler_name"], "euler");
        assert_eq!(k["scheduler"], "simple");
        assert_eq!(k["model"], json!(["12", 0]));
        assert_eq!(g["6"]["inputs"]["width"], 1024);
        assert_eq!(g["9"]["class_type"], "SaveImage");
    }

    #[test]
    fn two_references_use_the_b_inputs() {
        let g = build_graph(
            &files(),
            &["a.png".to_string(), "b.png".to_string()],
            "together",
            Aspect::Portrait.size(),
            1,
        );
        let p = &g["12"]["inputs"];
        assert_eq!(p["source_latent_b"], json!(["31", 0]));
        assert_eq!(p["source_image_b"], json!(["21", 0]));
        assert_eq!(g["4"]["inputs"]["image_b"], json!(["21", 0]));
        assert_eq!(g["5"]["inputs"]["image_b"], json!(["21", 0]));
        assert_eq!(g["21"]["inputs"]["image"], "b.png");
        assert_eq!(
            (
                g["6"]["inputs"]["width"].as_u64(),
                g["6"]["inputs"]["height"].as_u64()
            ),
            (Some(896), Some(1152))
        );
        // A third sheet is ignored.
        let g3 = build_graph(
            &files(),
            &["a.png".into(), "b.png".into(), "c.png".into()],
            "x",
            Aspect::Square.size(),
            1,
        );
        assert!(g3.get("22").is_none());
    }

    #[test]
    fn aspects_are_multiples_of_64() {
        for a in [Aspect::Square, Aspect::Portrait, Aspect::Landscape] {
            let (w, h) = a.size();
            assert!(w % 64 == 0 && h % 64 == 0 && (900_000..=1_100_000).contains(&(w * h)));
        }
    }

    fn png() -> Vec<u8> {
        crate::provider::mock::MOCK_PNG.to_vec()
    }

    fn svc() -> IdentityService {
        IdentityService::new(Arc::new(|| {}))
    }

    #[test]
    fn references_are_checked() {
        let s = svc();
        assert!(s.set_ref(0, "a.png", png()).is_ok());
        assert!(s.set_ref(3, "a.png", png()).is_err());
        assert!(s
            .set_ref(1, "a.txt", b"hello".to_vec())
            .unwrap_err()
            .contains("isn't"));
        let mut big = png();
        big.resize(MAX_REF_BYTES + 1, 0);
        assert!(s
            .set_ref(1, "big.png", big)
            .unwrap_err()
            .contains("too big"));
        let v = s.view();
        assert!(v.refs[0]
            .as_ref()
            .unwrap()
            .data_url
            .starts_with("data:image/png;base64,"));
        assert!(v.refs[1].is_none());
        s.clear_ref(0);
        assert!(s.view().refs[0].is_none());
    }

    #[test]
    fn begin_needs_a_sheet_and_a_prompt_and_is_not_reentrant() {
        let s = svc();
        let new = |s: &IdentityService, p: &str| s.begin(p, Mode::New, Aspect::Square);
        assert!(new(&s, "   ").unwrap_err().contains("Describe"));
        assert!(new(&s, "a cafe").unwrap_err().contains("character sheet"));
        s.set_ref(0, "a.png", png()).unwrap();
        assert!(new(&s, &"x".repeat(2001)).unwrap_err().contains("under"));
        let (refs, size, _) = new(&s, "a cafe").unwrap();
        assert_eq!((refs.len(), size), (1, (1024, 1024)));
        assert_eq!(s.view().phase, Phase::Uploading);
        assert!(new(&s, "again").unwrap_err().contains("already"));
        s.finished(Phase::Failed, "x");
        assert!(new(&s, "again").is_ok());
    }

    /// A minimal PNG header with the given size (enough for `image_size`).
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b
    }

    #[test]
    fn image_sizes_are_read_from_headers() {
        assert_eq!(image_size(&png_header(896, 1152)), Some((896, 1152)));
        // JPEG: SOI, an APP0 segment to skip, then SOF0 (h=300, w=500).
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x01,
            0x2C, 0x01, 0xF4, 0x03,
        ];
        assert_eq!(image_size(&jpeg), Some((500, 300)));
        // WebP lossy (VP8), lossless (VP8L) and extended (VP8X).
        let riff = |fourcc: &[u8; 4], payload: &[u8]| {
            let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
            b.extend(fourcc);
            b.extend([0u8; 4]);
            b.extend(payload);
            b
        };
        let mut vp8 = vec![0u8; 6]; // frame tag + start code region
        vp8[3..6].copy_from_slice(&[0x9D, 0x01, 0x2A]);
        vp8.extend(640u16.to_le_bytes());
        vp8.extend(480u16.to_le_bytes());
        assert_eq!(image_size(&riff(b"VP8 ", &vp8)), Some((640, 480)));
        let v: u32 = 799 | (599 << 14);
        let mut vp8l = vec![0x2F];
        vp8l.extend(v.to_le_bytes());
        assert_eq!(image_size(&riff(b"VP8L", &vp8l)), Some((800, 600)));
        let mut vp8x = vec![0u8; 4];
        vp8x.extend(&(1023u32).to_le_bytes()[..3]);
        vp8x.extend(&(767u32).to_le_bytes()[..3]);
        assert_eq!(image_size(&riff(b"VP8X", &vp8x)), Some((1024, 768)));
        assert_eq!(image_size(b"not an image at all, nope....."), None);
        assert_eq!(image_size(&png_header(0, 5)), None);
    }

    #[test]
    fn edit_canvas_follows_the_picture_in_multiples_of_64() {
        assert_eq!(fit_size(896, 1152), (896, 1152)); // the spike's base
        assert_eq!(fit_size(1024, 1024), (1024, 1024));
        assert_eq!(fit_size(1920, 1080), (1344, 768));
        assert_eq!(fit_size(100, 100), (1024, 1024)); // tiny pictures grow
        let (w, h) = fit_size(4000, 500);
        assert!((512..=1536).contains(&w) && (512..=1536).contains(&h));
        assert!(w % 64 == 0 && h % 64 == 0);
    }

    #[test]
    fn edit_begin_puts_the_picture_first_and_ignores_sheet_two() {
        let s = svc();
        let edit = |s: &IdentityService| s.begin("make it dusk", Mode::Edit, Aspect::Landscape);
        assert!(edit(&s).unwrap_err().contains("picture you want to edit"));
        // Character sheets alone aren't enough for an edit.
        s.set_ref(0, "sheet.png", png()).unwrap();
        s.set_ref(1, "other.png", png()).unwrap();
        assert!(edit(&s).is_err());
        s.set_ref(BASE_SLOT, "base.png", png_header(896, 1152))
            .unwrap();
        let (refs, size, _) = edit(&s).unwrap();
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["base.png", "sheet.png"]
        );
        assert_eq!(
            size,
            (896, 1152),
            "the shape is the picture's, not the Shape menu's"
        );
        s.finished(Phase::Done, "");
        s.clear_ref(0);
        let (refs, _, _) = edit(&s).unwrap();
        assert_eq!(refs.len(), 1, "a picture alone is enough");
        s.finished(Phase::Done, "");
        assert!(s
            .begin(" ", Mode::Edit, Aspect::Square)
            .unwrap_err()
            .contains("what to change"));
    }

    #[test]
    fn error_text_is_short_and_plain() {
        let oom = json!({"status": {"messages": [["execution_error",
            {"exception_message": "CUDA out of memory. Tried to allocate"}]]}});
        assert!(error_text(&oom).contains("ran out of memory"));
        let other = json!({"status": {"messages": [["execution_error",
            {"exception_message": "boom\ntraceback…"}]]}});
        assert_eq!(error_text(&other), "The GPU couldn't make that image: boom");
        assert_eq!(error_text(&json!({})), "The GPU couldn't make that image.");
    }
}
