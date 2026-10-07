//! Identity Edit: the panel's engine. One or two character sheets plus a
//! prompt become a ComfyUI graph (the one tested in the 2026-10-02 spike:
//! docs/findings.md, "Krea 2 edit spike"), run on the instance through the
//! sidecar. The window talks to this over Tauri commands; the instance only
//! ever sees the launch secret, never a browser (ComfyUI refuses requests that
//! carry an `Origin`, so a webview couldn't talk to it anyway).

use std::path::PathBuf;
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
/// A picture with more pixels than this is refused when it is added (about
/// 6000 x 6000). The byte limit alone lets a well-compressed 50 MP photo through.
pub const MAX_REF_PIXELS: u64 = 36 * 1024 * 1024;
/// A reference bigger than this (in pixels) is scaled down on the GPU before it
/// is encoded; smaller ones are used as they are (never enlarged).
const SHRINK_ABOVE_PIXELS: u64 = 3 * 1024 * 1024 / 2;
/// What a shrunken reference is scaled to (the model itself works at about 1 MP).
const SHRUNK_MEGAPIXELS: f64 = 1.0;
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

/// A rectangle in picture pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The area the user painted over the picture to edit.
#[derive(Debug, Clone, Deserialize)]
pub struct MaskInput {
    /// A PNG, white where the picture should change (base64).
    pub png: String,
    /// The picture's size as the window sees it (the mask has the same size).
    pub width: u32,
    pub height: u32,
    /// Smallest rectangle around everything painted.
    pub bbox: Rect,
    /// Work on the painted area alone, zoomed in, for more detail.
    pub zoom: bool,
}

/// A part of a character sheet to use instead of the whole sheet (say, the
/// face on a full-body picture).
#[derive(Debug, Clone, Deserialize)]
pub struct FocusInput {
    /// The character sheet's slot: 0 or 1.
    pub slot: usize,
    /// Smallest rectangle around what the user painted, in the sheet's own pixels.
    pub bbox: Rect,
}

/// Optional extras for a request: how far to go and where (edits only), and
/// which part of each character sheet to look at (both modes).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EditOpts {
    /// 50..=100: how much the picture may change. 100 re-draws it from the
    /// words alone (the default); lower stays closer to the original.
    pub strength: Option<u32>,
    pub mask: Option<MaskInput>,
    #[serde(default)]
    pub focus: Vec<FocusInput>,
}

/// Context kept around a focus area: a quarter of its size, at least this many pixels.
const FOCUS_MIN_PAD: u32 = 32;
/// A focus crop smaller than this (in pixels) is enlarged before it is encoded,
/// so a small face still reaches the model with some detail.
const FOCUS_MIN_PIXELS: u64 = 256 * 1024;
const FOCUS_ENLARGED_MEGAPIXELS: f64 = 0.5;

/// What is cut out of a sheet for a painted focus area: the area plus some
/// context, kept inside the picture.
pub fn focus_crop(image: (u32, u32), bbox: Rect) -> Rect {
    let pad = (bbox.w.max(bbox.h) / 4).max(FOCUS_MIN_PAD);
    let x0 = bbox.x.saturating_sub(pad);
    let y0 = bbox.y.saturating_sub(pad);
    let x1 = bbox
        .x
        .saturating_add(bbox.w)
        .saturating_add(pad)
        .min(image.0);
    let y1 = bbox
        .y
        .saturating_add(bbox.h)
        .saturating_add(pad)
        .min(image.1);
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

/// The crop for the sheet in `slot`, if the user chose a part of it. The box
/// is checked against the sheet that is actually kept.
fn focus_for(opts: &EditOpts, slot: usize, bytes: &[u8]) -> Result<Option<Rect>, String> {
    let Some(f) = opts.focus.iter().find(|f| f.slot == slot) else {
        return Ok(None);
    };
    let bad = || "The chosen part doesn't fit the picture. Choose it again.".to_string();
    let (w, h) = image_size(bytes).ok_or_else(bad)?;
    let fits = |a: u32, len: u32, max: u32| len > 0 && a.checked_add(len).is_some_and(|e| e <= max);
    if !fits(f.bbox.x, f.bbox.w, w) || !fits(f.bbox.y, f.bbox.h, h) {
        return Err(bad());
    }
    Ok(Some(focus_crop((w, h), f.bbox)))
}

/// What the graph needs beyond the plain edit.
#[derive(Debug, Clone, PartialEq)]
pub struct EditGraph {
    /// Denoise of the sampler (1.0 = no head start from the picture).
    pub denoise: f64,
    /// The picture's size (or the zoomed area's size) the result is put back at.
    pub region: (u32, u32),
    /// Uploaded file name of the mask, when the user painted one.
    pub mask: Option<String>,
    /// The zoomed area, when working on part of the picture.
    pub crop: Option<Rect>,
}

impl EditGraph {
    /// True when the plain "empty canvas" graph is enough.
    fn is_plain(&self) -> bool {
        self.mask.is_none() && self.denoise >= 1.0
    }
}

/// How the area around a painted region is cut out and zoomed, and what
/// canvas the model works on. `None` = use the whole picture.
pub fn plan_crop(image: (u32, u32), bbox: Rect) -> (Option<Rect>, (u32, u32)) {
    let whole = fit_size(image.0, image.1);
    let (iw, ih) = (i64::from(image.0), i64::from(image.1));
    // Context around the painted area: a quarter of its size, at least 64 px.
    let pad = (i64::from(bbox.w.max(bbox.h)) / 4).max(64);
    let mut x0 = (i64::from(bbox.x) - pad).max(0);
    let mut y0 = (i64::from(bbox.y) - pad).max(0);
    let mut x1 = (i64::from(bbox.x) + i64::from(bbox.w) + pad).min(iw);
    let mut y1 = (i64::from(bbox.y) + i64::from(bbox.h) + pad).min(ih);
    let canvas = fit_size(
        u32::try_from(x1 - x0).unwrap_or(1).max(1),
        u32::try_from(y1 - y0).unwrap_or(1).max(1),
    );
    // Grow the short side so the crop has the canvas's shape (no stretching).
    let aspect = f64::from(canvas.0) / f64::from(canvas.1);
    let grow = |lo: &mut i64, hi: &mut i64, want: f64, max: i64| {
        let want = (want.round() as i64).min(max);
        let extra = want - (*hi - *lo);
        if extra > 0 {
            *lo = (*lo - extra / 2).max(0);
            *hi = (*lo + want).min(max);
            *lo = (*hi - want).max(0);
        }
    };
    let (cw, ch) = ((x1 - x0) as f64, (y1 - y0) as f64);
    if cw / ch < aspect {
        grow(&mut x0, &mut x1, ch * aspect, iw);
    } else {
        grow(&mut y0, &mut y1, cw / aspect, ih);
    }
    let crop = Rect {
        x: x0 as u32,
        y: y0 as u32,
        w: (x1 - x0) as u32,
        h: (y1 - y0) as u32,
    };
    // Zooming only pays when the area is much smaller than the picture and
    // gets enlarged on the canvas.
    let area = u64::from(crop.w) * u64::from(crop.h);
    let enlarges = area < u64::from(canvas.0) * u64::from(canvas.1);
    let small = area * 10 < u64::from(image.0) * u64::from(image.1) * 6;
    if enlarges && small {
        (Some(crop), canvas)
    } else {
        (None, whole)
    }
}

/// A checked edit request: the canvas, the graph extras, and the mask file.
#[derive(Debug, Clone)]
pub struct EditPlan {
    pub graph: EditGraph,
    pub mask_png: Option<Vec<u8>>,
}

/// Check the extras of an edit and work out its canvas. `plain` is the canvas
/// of an edit without a painted area.
fn make_plan(opts: &EditOpts, plain: (u32, u32)) -> Result<((u32, u32), EditPlan), String> {
    let denoise = f64::from(opts.strength.unwrap_or(100).clamp(50, 100)) / 100.0;
    let Some(m) = &opts.mask else {
        let graph = EditGraph {
            denoise,
            region: plain,
            mask: None,
            crop: None,
        };
        return Ok((
            plain,
            EditPlan {
                graph,
                mask_png: None,
            },
        ));
    };
    let bad = || "The painted area doesn't fit the picture. Paint it again.".to_string();
    let (w, h, b) = (m.width, m.height, m.bbox);
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_REF_PIXELS {
        return Err(bad());
    }
    let fits = |a: u32, len: u32, max: u32| len > 0 && a.checked_add(len).is_some_and(|e| e <= max);
    if !fits(b.x, b.w, w) || !fits(b.y, b.h, h) {
        return Err(bad());
    }
    if m.png.len() > MAX_REF_BYTES / 3 * 4 + 8 {
        return Err("The painted area is too big.".into());
    }
    let png = base64::engine::general_purpose::STANDARD
        .decode(m.png.as_bytes())
        .map_err(|_| bad())?;
    if !png.starts_with(b"\x89PNG") || image_size(&png) != Some((w, h)) {
        return Err(bad());
    }
    let (crop, canvas) = if m.zoom {
        plan_crop((w, h), b)
    } else {
        (None, fit_size(w, h))
    };
    let graph = EditGraph {
        denoise,
        region: crop.map_or((w, h), |c| (c.w, c.h)),
        mask: Some(String::new()),
        crop,
    };
    Ok((
        canvas,
        EditPlan {
            graph,
            mask_png: Some(png),
        },
    ))
}

/// Pixels the painted area is widened by before sampling (a clean seam).
const MASK_GROW: u32 = 8;
/// Softness of the seam when the result is put back.
const SEAM_BLUR: u32 = 8;

/// The ComfyUI "API format" graph for one generation. `refs` are the file
/// names of the uploaded pictures (1 or 2, in order; for an edit, the picture
/// to edit comes first).
#[cfg(test)]
pub fn build_graph(
    files: &ModelFiles,
    refs: &[String],
    prompt: &str,
    size: (u32, u32),
    seed: u64,
    edit: Option<&EditGraph>,
) -> Value {
    build_graph_with(files, refs, prompt, size, seed, edit, &[], &[])
}

/// True when a reference is big enough to be scaled down before it is encoded
/// (a header we can't read counts as big: scaling is always safe).
pub fn needs_shrink(bytes: &[u8]) -> bool {
    image_size(bytes).is_none_or(|(w, h)| u64::from(w) * u64::from(h) > SHRINK_ABOVE_PIXELS)
}

/// [`build_graph`] plus `shrink`: which references to scale down to about 1 MP
/// first, so a huge picture is never VAE-encoded at its own size (that can fill
/// the GPU's memory and make a job crawl). `focus`: which references to cut
/// down to a part first (the cut is sized by its own pixels, not `shrink`).
#[allow(clippy::too_many_arguments)]
pub fn build_graph_with(
    files: &ModelFiles,
    refs: &[String],
    prompt: &str,
    size: (u32, u32),
    seed: u64,
    edit: Option<&EditGraph>,
    shrink: &[bool],
    focus: &[Option<Rect>],
) -> Value {
    let (w, h) = size;
    let edit = edit.filter(|e| !e.is_plain());
    let zoomed = edit.is_some_and(|e| e.crop.is_some());
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
        // A zoomed edit shows the model the zoomed area, not the whole picture.
        let pixels = if i == 0 && zoomed {
            json!(["40", 0])
        } else {
            let part = focus.get(i).copied().flatten();
            let mut source = json!([&load, 0]);
            if let Some(c) = part {
                let cut = format!("8{i}");
                g[&cut] = crop_node(source, c);
                source = json!([&cut, 0]);
            }
            let resize = match part {
                Some(c) => {
                    let area = u64::from(c.w) * u64::from(c.h);
                    if area < FOCUS_MIN_PIXELS {
                        Some(("lanczos", FOCUS_ENLARGED_MEGAPIXELS))
                    } else if area > SHRINK_ABOVE_PIXELS {
                        Some(("area", SHRUNK_MEGAPIXELS))
                    } else {
                        None
                    }
                }
                None => shrink
                    .get(i)
                    .copied()
                    .unwrap_or(false)
                    .then_some(("area", SHRUNK_MEGAPIXELS)),
            };
            match resize {
                Some((method, megapixels)) => {
                    let small = format!("7{i}");
                    g[&small] = json!({"class_type": "ImageScaleToTotalPixels", "inputs": {
                        "image": source, "upscale_method": method,
                        "megapixels": megapixels, "resolution_steps": 8}});
                    json!([&small, 0])
                }
                None => source,
            }
        };
        g[&latent]["inputs"]["pixels"] = pixels.clone();
        patch[format!("source_latent{suffix}")] = json!([&latent, 0]);
        patch[format!("source_image{suffix}")] = pixels.clone();
        enc[if i == 0 { "image" } else { "image_b" }] = pixels;
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
    let mut latent = json!(["6", 0]);
    let mut denoise = 1.0;
    if let Some(e) = edit {
        denoise = e.denoise;
        latent = add_edit_nodes(&mut g, e, size);
    }
    g["7"] = json!({"class_type": "KSampler", "inputs": {
        "seed": seed, "steps": STEPS, "cfg": 1.0, "sampler_name": "euler",
        "scheduler": "simple", "denoise": denoise,
        "model": ["12", 0], "positive": ["4", 0], "negative": ["5", 0],
        "latent_image": latent}});
    g["8"] = json!({"class_type": "VAEDecode",
                    "inputs": {"samples": ["7", 0], "vae": ["3", 0]}});
    let result = match edit {
        Some(e) if e.mask.is_some() => add_composite_nodes(&mut g, e, size),
        _ => json!(["8", 0]),
    };
    g["9"] = json!({"class_type": "SaveImage",
                    "inputs": {"filename_prefix": "SlopTweak", "images": result}});
    g
}

/// Nodes that start the sampler from the picture (and, with a painted area,
/// only redraw that area). Returns the latent the sampler starts from.
/// "20" is the picture to edit; "40" is the picture at the canvas's size.
fn add_edit_nodes(g: &mut Value, e: &EditGraph, size: (u32, u32)) -> Value {
    let (w, h) = size;
    let scale = |image: Value| {
        json!({"class_type": "ImageScale", "inputs": {"image": image,
            "upscale_method": "lanczos", "width": w, "height": h, "crop": "disabled"}})
    };
    let mut source = json!(["20", 0]);
    if let Some(c) = e.crop {
        g["41"] = crop_node(json!(["20", 0]), c);
        source = json!(["41", 0]);
    }
    g["40"] = scale(source);
    g["42"] = json!({"class_type": "VAEEncode",
                     "inputs": {"pixels": ["40", 0], "vae": ["3", 0]}});
    let Some(mask) = &e.mask else {
        return json!(["42", 0]);
    };
    g["50"] = json!({"class_type": "LoadImage", "inputs": {"image": mask}});
    let mut region = json!(["50", 0]);
    if let Some(c) = e.crop {
        g["51"] = crop_node(json!(["50", 0]), c);
        region = json!(["51", 0]);
    }
    g["52"] = scale(region);
    g["53"] = json!({"class_type": "ImageToMask",
                     "inputs": {"image": ["52", 0], "channel": "red"}});
    g["54"] = json!({"class_type": "GrowMask",
                     "inputs": {"mask": ["53", 0], "expand": MASK_GROW, "tapered_corners": false}});
    g["55"] = json!({"class_type": "SetLatentNoiseMask",
                     "inputs": {"samples": ["42", 0], "mask": ["54", 0]}});
    json!(["55", 0])
}

fn crop_node(image: Value, c: Rect) -> Value {
    json!({"class_type": "ImageCrop", "inputs": {
        "image": image, "width": c.w, "height": c.h, "x": c.x, "y": c.y}})
}

/// Put the redrawn area back into the untouched picture through a soft seam,
/// so everything outside the painted area keeps its exact pixels. Returns the
/// image to save.
fn add_composite_nodes(g: &mut Value, e: &EditGraph, size: (u32, u32)) -> Value {
    let (rw, rh) = e.region;
    let at = e.crop.map_or((0, 0), |c| (c.x, c.y));
    // The decoded canvas, back at the region's size.
    g["56"] = json!({"class_type": "ImageScale", "inputs": {"image": ["8", 0],
        "upscale_method": "lanczos", "width": rw, "height": rh, "crop": "disabled"}});
    let region_mask = if e.crop.is_some() {
        json!(["51", 0])
    } else {
        json!(["50", 0])
    };
    // The seam widens in step with the canvas's zoom.
    let grow = (MASK_GROW * rw / size.0.max(1)).max(2);
    g["57"] = json!({"class_type": "ImageToMask",
                     "inputs": {"image": region_mask, "channel": "red"}});
    g["58"] = json!({"class_type": "GrowMask",
                     "inputs": {"mask": ["57", 0], "expand": grow, "tapered_corners": false}});
    g["59"] = json!({"class_type": "MaskToImage", "inputs": {"mask": ["58", 0]}});
    g["60"] = json!({"class_type": "ImageBlur", "inputs": {
        "image": ["59", 0], "blur_radius": SEAM_BLUR, "sigma": f64::from(SEAM_BLUR) / 2.0}});
    g["61"] = json!({"class_type": "ImageToMask",
                     "inputs": {"image": ["60", 0], "channel": "red"}});
    g["62"] = json!({"class_type": "ImageCompositeMasked", "inputs": {
        "destination": ["20", 0], "source": ["56", 0], "x": at.0, "y": at.1,
        "resize_source": false, "mask": ["61", 0]}});
    json!(["62", 0])
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

/// A checked request, ready to run. `focus` lines up with `refs`.
struct Begun {
    refs: Vec<Kept>,
    size: (u32, u32),
    cancel: Arc<AtomicBool>,
    plan: Option<EditPlan>,
    focus: Vec<Option<Rect>>,
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
        if let Some((w, h)) = image_size(&bytes) {
            if u64::from(w) * u64::from(h) > MAX_REF_PIXELS {
                return Err(format!(
                    "That picture is too large ({w} x {h} pixels). Please use one under \
                     36 megapixels, for example 6000 x 6000."
                ));
            }
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

    /// Write a finished image into `dir` (the Downloads folder) under a name
    /// that doesn't overwrite anything; returns the path written.
    pub fn save_result(&self, result_id: &str, dir: &std::path::Path) -> Result<PathBuf, String> {
        let (name, bytes) = {
            let i = self.inner.lock().unwrap();
            let r = i
                .results
                .iter()
                .find(|r| r.id == result_id)
                .ok_or("That image is no longer in the panel.")?;
            (r.name.clone(), r.bytes.clone())
        };
        let ext = if bytes.starts_with(b"\x89PNG") {
            "png"
        } else if bytes.starts_with(b"RIFF") {
            "webp"
        } else {
            "jpg"
        };
        // The name came from the GPU: keep only plain characters.
        let stem: String = std::path::Path::new(&name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
            .take(60)
            .collect();
        let stem = if stem.trim().is_empty() {
            "SlopTweak".to_string()
        } else {
            stem
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't open the folder: {e}"))?;
        for n in 0..1000 {
            let file = if n == 0 {
                format!("{stem}.{ext}")
            } else {
                format!("{stem} ({n}).{ext}")
            };
            let path = dir.join(file);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    std::io::Write::write_all(&mut f, &bytes)
                        .map_err(|e| format!("Couldn't save the image: {e}"))?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("Couldn't save the image: {e}")),
            }
        }
        Err("Couldn't find a free file name in that folder.".into())
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
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    fn begin(
        &self,
        prompt: &str,
        mode: Mode,
        aspect: Aspect,
    ) -> Result<(Vec<Kept>, (u32, u32), Arc<AtomicBool>), String> {
        let b = self.begin_with(prompt, mode, aspect, &EditOpts::default())?;
        Ok((b.refs, b.size, b.cancel))
    }

    fn begin_with(
        &self,
        prompt: &str,
        mode: Mode,
        aspect: Aspect,
        opts: &EditOpts,
    ) -> Result<Begun, String> {
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
        let mut focus = Vec::new();
        let (refs, size, plan): (Vec<Kept>, _, _) = match mode {
            Mode::New => {
                let mut refs = Vec::new();
                for slot in 0..2 {
                    if let Some(k) = take(&i.refs[slot]) {
                        focus.push(focus_for(opts, slot, &k.bytes)?);
                        refs.push(k);
                    }
                }
                if refs.is_empty() {
                    return Err("Add at least one character sheet.".into());
                }
                (refs, aspect.size(), None)
            }
            Mode::Edit => {
                let base =
                    take(&i.refs[BASE_SLOT]).ok_or("Add the picture you want to edit first.")?;
                // The picture's own shape; a header we can't read falls back to square.
                let size =
                    image_size(&base.bytes).map_or(Aspect::Square.size(), |(w, h)| fit_size(w, h));
                let (size, plan) = make_plan(opts, size)?;
                // The picture to edit is always used whole.
                focus.push(None);
                let mut refs = vec![base];
                if let Some(k) = take(&i.refs[0]) {
                    focus.push(focus_for(opts, 0, &k.bytes)?);
                    refs.push(k);
                }
                (refs, size, Some(plan))
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        i.cancel = Some(cancel.clone());
        i.phase = Phase::Uploading;
        i.message = "Sending your pictures…".into();
        drop(i);
        (self.notify)();
        Ok(Begun {
            refs,
            size,
            cancel,
            plan,
            focus,
        })
    }

    /// Make one image. Returns quickly with an error if the request is
    /// invalid; otherwise runs to the end and records the outcome in the
    /// service's phase/message (the window reads those).
    #[cfg(test)]
    pub async fn generate(
        self: &Arc<Self>,
        target: &Target,
        prompt: &str,
        mode: Mode,
        aspect: Aspect,
        seed: u64,
    ) -> Result<(), String> {
        self.generate_with(target, prompt, mode, aspect, seed, &EditOpts::default())
            .await
    }

    /// [`generate`](Self::generate) with the edit extras (strength, painted area).
    pub async fn generate_with(
        self: &Arc<Self>,
        target: &Target,
        prompt: &str,
        mode: Mode,
        aspect: Aspect,
        seed: u64,
        opts: &EditOpts,
    ) -> Result<(), String> {
        let b = self.begin_with(prompt, mode, aspect, opts)?;
        let outcome = self
            .run(
                target,
                &b.refs,
                prompt.trim(),
                b.size,
                seed,
                b.plan,
                &b.focus,
                &b.cancel,
            )
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
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        t: &Target,
        refs: &[Kept],
        prompt: &str,
        size: (u32, u32),
        seed: u64,
        plan: Option<EditPlan>,
        focus: &[Option<Rect>],
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

        let mut edit = plan.as_ref().map(|p| p.graph.clone());
        if let Some(png) = plan.as_ref().and_then(|p| p.mask_png.as_ref()) {
            let name = format!("sloptweak_mask_{tag}.png");
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
                    ("image", &name, png.clone()),
                )
                .await
                .map_err(unreachable)?;
            if let Some(e) = edit.as_mut() {
                e.mask = Some(answer["name"].as_str().unwrap_or(&name).to_string());
            }
            if cancel.load(Ordering::SeqCst) {
                return Ok(None);
            }
        }
        let shrink: Vec<bool> = refs.iter().map(|r| needs_shrink(&r.bytes)).collect();
        let graph = build_graph_with(
            &t.files,
            &names,
            prompt,
            size,
            seed,
            edit.as_ref(),
            &shrink,
            focus,
        );
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
            None,
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
            None,
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
            None,
        );
        assert!(g3.get("22").is_none());
    }

    #[test]
    fn big_references_are_scaled_down_before_they_are_encoded() {
        let g = build_graph_with(
            &files(),
            &["a.png".into(), "b.png".into()],
            "x",
            (896, 1152),
            1,
            None,
            &[true, false],
            &[],
        );
        assert_eq!(g["70"]["class_type"], "ImageScaleToTotalPixels");
        assert_eq!(g["70"]["inputs"]["image"], json!(["20", 0]));
        assert_eq!(g["70"]["inputs"]["megapixels"], 1.0);
        assert_eq!(g["30"]["inputs"]["pixels"], json!(["70", 0]));
        assert_eq!(g["12"]["inputs"]["source_image"], json!(["70", 0]));
        assert_eq!(g["4"]["inputs"]["image"], json!(["70", 0]));
        // The small one is used as it is (not enlarged).
        assert!(g.get("71").is_none());
        assert_eq!(g["31"]["inputs"]["pixels"], json!(["21", 0]));
    }

    #[test]
    fn only_big_or_unreadable_pictures_are_shrunk() {
        assert!(!needs_shrink(&png_header(1024, 1024)));
        assert!(!needs_shrink(&png_header(1200, 1200)));
        assert!(needs_shrink(&png_header(2000, 2000)));
        assert!(needs_shrink(&png_header(6000, 4000)));
        assert!(needs_shrink(b"\x89PNG not really"));
    }

    #[test]
    fn huge_pictures_are_refused_when_added() {
        let s = svc();
        assert!(s.set_ref(2, "ok.png", png_header(6000, 6000)).is_ok());
        let err = s.set_ref(2, "big.png", png_header(8000, 8000)).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert!(err.contains("8000 x 8000"), "{err}");
    }

    fn edit_graph(denoise: f64, mask: Option<&str>, crop: Option<Rect>) -> EditGraph {
        EditGraph {
            denoise,
            region: crop.map_or((1000, 1500), |c| (c.w, c.h)),
            mask: mask.map(String::from),
            crop,
        }
    }

    #[test]
    fn full_strength_without_a_mask_is_the_plain_graph() {
        let plain = build_graph(&files(), &["a.png".into()], "x", (896, 1152), 1, None);
        let same = build_graph(
            &files(),
            &["a.png".into()],
            "x",
            (896, 1152),
            1,
            Some(&edit_graph(1.0, None, None)),
        );
        assert_eq!(plain, same);
    }

    #[test]
    fn lower_strength_starts_from_the_picture() {
        let g = build_graph(
            &files(),
            &["a.png".into()],
            "x",
            (896, 1152),
            1,
            Some(&edit_graph(0.7, None, None)),
        );
        assert_eq!(g["7"]["inputs"]["denoise"], 0.7);
        assert_eq!(g["7"]["inputs"]["latent_image"], json!(["42", 0]));
        assert_eq!(g["42"]["inputs"]["pixels"], json!(["40", 0]));
        assert_eq!(g["40"]["inputs"]["image"], json!(["20", 0]));
        assert_eq!(g["40"]["inputs"]["width"], 896);
        // The patch's own canvas and the picture reference are unchanged.
        assert_eq!(g["12"]["inputs"]["target_latent"], json!(["6", 0]));
        assert_eq!(g["12"]["inputs"]["source_image"], json!(["20", 0]));
        assert_eq!(g["9"]["inputs"]["images"], json!(["8", 0]));
    }

    #[test]
    fn a_painted_area_redraws_only_that_area_and_puts_it_back() {
        let g = build_graph(
            &files(),
            &["a.png".into(), "s.png".into()],
            "x",
            (896, 1152),
            1,
            Some(&edit_graph(1.0, Some("m.png"), None)),
        );
        assert_eq!(g["50"]["inputs"]["image"], "m.png");
        assert_eq!(g["55"]["class_type"], "SetLatentNoiseMask");
        assert_eq!(g["7"]["inputs"]["latent_image"], json!(["55", 0]));
        assert_eq!(g["7"]["inputs"]["denoise"], 1.0);
        // Pasted over the original picture, so the rest keeps its pixels.
        assert_eq!(g["62"]["inputs"]["destination"], json!(["20", 0]));
        assert_eq!(g["62"]["inputs"]["x"], 0);
        assert_eq!(g["9"]["inputs"]["images"], json!(["62", 0]));
        assert_eq!(g["56"]["inputs"]["width"], 1000);
        assert!(g.get("41").is_none());
        // Every link points at a node that exists.
        for (id, node) in g.as_object().unwrap() {
            for v in node["inputs"].as_object().unwrap().values() {
                if let Some([src, _]) = v.as_array().map(Vec::as_slice) {
                    assert!(g.get(src.as_str().unwrap()).is_some(), "{id} -> {src}");
                }
            }
        }
    }

    #[test]
    fn a_zoomed_area_is_cut_out_enlarged_and_pasted_back_in_place() {
        let crop = Rect {
            x: 300,
            y: 200,
            w: 400,
            h: 500,
        };
        let g = build_graph(
            &files(),
            &["a.png".into()],
            "x",
            (832, 1024),
            1,
            Some(&edit_graph(1.0, Some("m.png"), Some(crop))),
        );
        assert_eq!(g["41"]["class_type"], "ImageCrop");
        assert_eq!(g["41"]["inputs"]["x"], 300);
        assert_eq!(g["40"]["inputs"]["image"], json!(["41", 0]));
        assert_eq!(g["51"]["inputs"]["y"], 200);
        // The model sees the zoomed area as its picture.
        assert_eq!(g["12"]["inputs"]["source_image"], json!(["40", 0]));
        assert_eq!(g["30"]["inputs"]["pixels"], json!(["40", 0]));
        assert_eq!(g["4"]["inputs"]["image"], json!(["40", 0]));
        // Back at the crop's size, pasted at the crop's place.
        assert_eq!(g["56"]["inputs"]["width"], 400);
        assert_eq!(g["62"]["inputs"]["x"], 300);
        assert_eq!(g["62"]["inputs"]["y"], 200);
        assert_eq!(g["62"]["inputs"]["destination"], json!(["20", 0]));
    }

    #[test]
    fn small_areas_are_zoomed_and_big_ones_are_not() {
        // A face-sized patch in a 2000x3000 picture: crop with context, ~1 MP canvas.
        let (crop, canvas) = plan_crop(
            (2000, 3000),
            Rect {
                x: 800,
                y: 500,
                w: 300,
                h: 300,
            },
        );
        let c = crop.unwrap();
        assert!(c.x <= 800 && c.y <= 500 && c.x + c.w >= 1100 && c.y + c.h >= 800);
        assert!(c.x + c.w <= 2000 && c.y + c.h <= 3000);
        assert_eq!(canvas.0 % 64, 0);
        assert!(u64::from(c.w) * u64::from(c.h) < u64::from(canvas.0) * u64::from(canvas.1));
        // Same shape as the canvas (within a few pixels).
        let (ca, ka) = (
            f64::from(c.w) / f64::from(c.h),
            f64::from(canvas.0) / f64::from(canvas.1),
        );
        assert!((ca - ka).abs() < 0.02, "{ca} vs {ka}");
        // Most of the picture painted: work on the whole picture.
        let (crop, canvas) = plan_crop(
            (1000, 1000),
            Rect {
                x: 50,
                y: 50,
                w: 900,
                h: 900,
            },
        );
        assert_eq!(crop, None);
        assert_eq!(canvas, fit_size(1000, 1000));
        // A tiny picture is never enlarged by cropping.
        let (crop, _) = plan_crop(
            (700, 700),
            Rect {
                x: 300,
                y: 300,
                w: 50,
                h: 50,
            },
        );
        assert!(crop.is_none_or(|c| c.w <= 700 && c.h <= 700));
        // The crop hugs the picture's edge instead of leaving it.
        let (crop, _) = plan_crop(
            (2000, 3000),
            Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
        );
        let c = crop.unwrap();
        assert_eq!((c.x, c.y), (0, 0));
    }

    fn mask_png(w: u32, h: u32) -> String {
        base64::engine::general_purpose::STANDARD.encode(png_header(w, h))
    }

    #[test]
    fn a_mask_must_match_the_picture() {
        let mask = |w, h, bbox, zoom| EditOpts {
            strength: Some(80),
            mask: Some(MaskInput {
                png: mask_png(w, h),
                width: w,
                height: h,
                bbox,
                zoom,
            }),
            ..Default::default()
        };
        let inside = Rect {
            x: 100,
            y: 100,
            w: 200,
            h: 200,
        };
        let (canvas, plan) = make_plan(&mask(2000, 3000, inside, true), (704, 1024)).unwrap();
        assert!(plan.mask_png.is_some());
        assert!(plan.graph.crop.is_some());
        assert_eq!(plan.graph.denoise, 0.8);
        assert_eq!(canvas.0 % 64, 0);
        let (canvas, plan) = make_plan(&mask(2000, 3000, inside, false), (704, 1024)).unwrap();
        assert_eq!((canvas, plan.graph.crop), (fit_size(2000, 3000), None));
        // Wrong size, or a box outside the picture: refused.
        let wrong = EditOpts {
            strength: None,
            mask: Some(MaskInput {
                png: mask_png(10, 10),
                width: 20,
                height: 20,
                bbox: inside,
                zoom: false,
            }),
            ..Default::default()
        };
        assert!(make_plan(&wrong, (704, 1024)).is_err());
        let out = Rect {
            x: 1900,
            y: 0,
            w: 200,
            h: 10,
        };
        assert!(make_plan(&mask(2000, 3000, out, false), (704, 1024)).is_err());
        // The strength is kept in range; no mask = plain canvas.
        let (canvas, plan) = make_plan(
            &EditOpts {
                strength: Some(5),
                mask: None,
                focus: Vec::new(),
            },
            (704, 1024),
        )
        .unwrap();
        assert_eq!((canvas, plan.graph.denoise), ((704, 1024), 0.5));
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
    fn saving_a_result_never_overwrites_and_cleans_the_name() {
        let s = svc();
        s.inner.lock().unwrap().results.push(Kept {
            id: "r1".into(),
            name: "../evil:name?.png".into(),
            bytes: png(),
        });
        let dir = std::env::temp_dir().join(format!("sloptweak-save-{}", std::process::id()));
        let a = s.save_result("r1", &dir).unwrap();
        let b = s.save_result("r1", &dir).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent().unwrap(), dir);
        assert_eq!(a.file_name().unwrap(), "evilname.png");
        assert_eq!(b.file_name().unwrap(), "evilname (1).png");
        assert!(s.save_result("nope", &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
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

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn a_focus_crop_keeps_some_context_inside_the_picture() {
        // A quarter of the area's size on each side...
        assert_eq!(
            focus_crop((2000, 3000), rect(800, 1000, 400, 400)),
            rect(700, 900, 600, 600)
        );
        // ...at least 32 px, and never past the picture's edge.
        assert_eq!(
            focus_crop((2000, 3000), rect(10, 2950, 40, 50)),
            rect(0, 2918, 82, 82)
        );
    }

    #[test]
    fn a_focus_part_replaces_the_whole_sheet_in_the_graph() {
        let refs = ["a.png".to_string(), "b.png".to_string()];
        let g = build_graph_with(
            &files(),
            &refs,
            "x",
            (896, 1152),
            1,
            None,
            &[true, false],
            &[None, Some(rect(100, 200, 300, 300))],
        );
        // The second sheet is cut first; a small cut is enlarged, not left tiny.
        assert_eq!(g["81"]["class_type"], "ImageCrop");
        assert_eq!(g["81"]["inputs"]["image"], json!(["21", 0]));
        assert_eq!(g["81"]["inputs"]["x"], 100);
        assert_eq!(g["81"]["inputs"]["height"], 300);
        assert_eq!(g["71"]["inputs"]["image"], json!(["81", 0]));
        assert_eq!(g["71"]["inputs"]["upscale_method"], "lanczos");
        assert_eq!(g["71"]["inputs"]["megapixels"], FOCUS_ENLARGED_MEGAPIXELS);
        assert_eq!(g["31"]["inputs"]["pixels"], json!(["71", 0]));
        assert_eq!(g["12"]["inputs"]["source_image_b"], json!(["71", 0]));
        assert_eq!(g["4"]["inputs"]["image_b"], json!(["71", 0]));
        // The first sheet is untouched: shrunk as before, not cut.
        assert!(g.get("80").is_none());
        assert_eq!(g["70"]["inputs"]["image"], json!(["20", 0]));
        assert_eq!(g["70"]["inputs"]["upscale_method"], "area");
        // A cut that is already a good size goes in as it is; a big one is shrunk
        // whatever `shrink` says.
        let g = build_graph_with(
            &files(),
            &refs[..1],
            "x",
            (896, 1152),
            1,
            None,
            &[true],
            &[Some(rect(0, 0, 800, 800))],
        );
        assert_eq!(g["30"]["inputs"]["pixels"], json!(["80", 0]));
        assert!(g.get("70").is_none());
        let g = build_graph_with(
            &files(),
            &refs[..1],
            "x",
            (896, 1152),
            1,
            None,
            &[false],
            &[Some(rect(0, 0, 3000, 2000))],
        );
        assert_eq!(g["70"]["inputs"]["image"], json!(["80", 0]));
        assert_eq!(g["70"]["inputs"]["megapixels"], 1.0);
    }

    #[test]
    fn a_focus_part_follows_its_sheet_into_the_request() {
        let focus = |slot, bbox| EditOpts {
            focus: vec![FocusInput { slot, bbox }],
            ..Default::default()
        };
        let s = svc();
        s.set_ref(0, "a.png", png_header(2000, 3000)).unwrap();
        s.set_ref(1, "b.png", png_header(1000, 1000)).unwrap();
        s.set_ref(2, "base.png", png_header(1500, 1500)).unwrap();
        let new = |s: &IdentityService, o: &EditOpts| {
            s.begin_with("a cafe", Mode::New, Aspect::Square, o)
        };
        let b = new(&s, &focus(1, rect(400, 300, 200, 200))).unwrap();
        assert_eq!(b.focus, vec![None, Some(rect(350, 250, 300, 300))]);
        s.finished(Phase::Failed, "x");
        // A box that doesn't fit the sheet that is kept is refused.
        assert!(new(&s, &focus(1, rect(900, 0, 200, 200))).is_err());
        assert!(new(&s, &focus(0, rect(0, 0, 0, 10))).is_err());
        assert_eq!(s.view().phase, Phase::Failed);
        // Edit: the picture to edit is never cut; the sheet may be. Slot 2 is ignored.
        let edit =
            |s: &IdentityService, o: &EditOpts| s.begin_with("dusk", Mode::Edit, Aspect::Square, o);
        let b = edit(&s, &focus(0, rect(100, 100, 400, 400))).unwrap();
        assert_eq!(b.focus, vec![None, Some(rect(0, 0, 600, 600))]);
        s.finished(Phase::Failed, "x");
        let b = edit(&s, &focus(2, rect(0, 0, 10, 10))).unwrap();
        assert_eq!(b.focus, vec![None, None]);
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
