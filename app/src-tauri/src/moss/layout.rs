//! Layout mode for the MOSS panel: boxes drawn on the picture, each optionally with
//! its own LoRA (docs/spikes/regional-lora-design.md; spike results in
//! docs/findings.md, "Regional-LoRA spike").
//!
//! One ComfyUI graph makes the picture in stages. Stage 0 is the ordinary Fast graph,
//! prompted with MOSS's trained-in JSON layout (`MOSSImageLayoutPrompt`'s format, built
//! here so the graph needs no frontend-only node). Then, for each box that has a LoRA, in
//! draw order: VAE-encode the current image, denoise only inside the box's feathered mask
//! with that LoRA, decode, and paste back through the same mask, so everything outside
//! the box stays exactly as it was. Hook LoRAs and masked conditioning were tried and
//! rejected (docs/findings.md): they bleed across the canvas and break the layout.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{lora_label, valid_lora_name, Params, MAX_PROMPT_CHARS};
use crate::identity::RUN_TIMEOUT;

pub const MAX_BOXES: usize = 12;
/// Boxes with a LoRA each cost one more sampling pass.
pub const MAX_LORA_PASSES: usize = 6;
/// Box edges are normalized to 0..=1000 on each axis, as in ComfyUI's `Create Bounding Boxes`.
pub const COORD_MAX: u16 = 1000;
pub const MIN_BOX_PX: u32 = 64;
pub const MAX_BOX_CAPTION_CHARS: usize = 500;
pub const MAX_BOX_TEXT_CHARS: usize = 200;
/// The layout JSON is the prompt MOSS reads; keep it well inside the encoder's budget.
pub const MAX_LAYOUT_JSON_CHARS: usize = 8000;
pub const MIN_STRENGTH: f64 = 0.0;
pub const MAX_STRENGTH: f64 = 1.5;
/// How much a pass may change its box: 0.5 restyles, 0.7 is the spike's sweet spot,
/// 0.9 redraws (new pose, box background changes too).
pub const MIN_DENOISE: f64 = 0.4;
pub const MAX_DENOISE: f64 = 0.9;
pub const DEFAULT_DENOISE: f64 = 0.7;
pub const DEFAULT_STRENGTH: f64 = 1.0;
/// Inward feather of each box mask, in pixels. The spike could not tell feathered from
/// hard edges on soft backgrounds, so this stays adjustable.
pub const DEFAULT_FEATHER_PX: u32 = 48;
pub const MAX_FEATHER_PX: u32 = 128;

/// Each region pass gets this much more than a plain picture before the run is
/// called stuck (the shared timeout is 15 minutes).
const TIMEOUT_PER_PASS: Duration = Duration::from_secs(90);
/// Rough seconds for the panel's estimate, from the spike (12 GB laptop, warm: about
/// 19 s for the layout and 16 s per pass), rounded up. An estimate, never a promise.
const ESTIMATE_BASE_SECS: u32 = 25;
const ESTIMATE_PER_PASS_SECS: u32 = 20;

fn one() -> f64 {
    DEFAULT_STRENGTH
}

fn default_denoise() -> f64 {
    DEFAULT_DENOISE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoxKind {
    Object,
    Character,
    Text,
}

impl BoxKind {
    fn label(self) -> &'static str {
        match self {
            BoxKind::Object => "object",
            BoxKind::Character => "character",
            BoxKind::Text => "text",
        }
    }
}

/// One LoRA for one box: the file the instance has, how strongly to apply it, and how
/// much of the box's picture the pass may redraw.
#[derive(Debug, Clone, Deserialize)]
pub struct LoraRef {
    /// A LoRA already on the GPU, as `MossLora::name` has it (`user/<file>.safetensors`).
    pub file: String,
    #[serde(default = "one")]
    pub strength: f64,
    #[serde(default = "default_denoise")]
    pub denoise: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoxReq {
    pub kind: BoxKind,
    /// What is in the box. Required except for text boxes.
    #[serde(default)]
    pub caption: String,
    /// The words to draw, text boxes only.
    #[serde(default)]
    pub text: Option<String>,
    /// `[y0, x0, y1, x1]`, each 0..=1000 across the whole picture.
    pub rect: [u16; 4],
    #[serde(default)]
    pub lora: Option<LoraRef>,
}

/// What the panel sends in Layout mode. `scene` describes the whole picture.
#[derive(Debug, Clone, Deserialize)]
pub struct LayoutRequest {
    pub scene: String,
    /// In draw order: background first, foreground last. Later boxes win where they overlap.
    pub boxes: Vec<BoxReq>,
    #[serde(default)]
    pub feather: Option<u32>,
}

/// A box in whole pixels of the picture, edges enclosing the normalized box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxBox {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// One region pass: a box and the LoRA that redraws it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pass {
    /// 1-based, as the panel numbers boxes.
    pub box_no: usize,
    pub lora: String,
    pub strength: f64,
    pub denoise: f64,
    pub area: PxBox,
}

/// A checked [`LayoutRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The prompt MOSS reads (the layout JSON).
    pub json: String,
    pub boxes: usize,
    pub feather: u32,
    pub passes: Vec<Pass>,
}

impl Layout {
    /// What the picture's caption adds: the boxes and the LoRAs that were used.
    pub fn summary(&self) -> String {
        let mut s = format!("layout: {} boxes", self.boxes);
        if !self.passes.is_empty() {
            let loras: Vec<String> = self
                .passes
                .iter()
                .map(|p| {
                    format!(
                        "{} on box {} (strength {}, change {})",
                        lora_label(&p.lora),
                        p.box_no,
                        p.strength,
                        p.denoise
                    )
                })
                .collect();
            s.push_str(&format!("; {}", loras.join(", ")));
        }
        s
    }

    /// Rough seconds for the whole picture, for the panel to show.
    pub fn estimate_secs(&self) -> u32 {
        ESTIMATE_BASE_SECS + ESTIMATE_PER_PASS_SECS * self.passes.len() as u32
    }

    /// Every LoRA the layout names must be one the instance has.
    pub fn check_known_loras(&self, known: &[String]) -> Result<(), String> {
        for p in &self.passes {
            if !known.iter().any(|k| k == &p.lora) {
                return Err(format!(
                    "Box {} uses the LoRA {}, which isn't on the GPU. Add it first.",
                    p.box_no, p.lora
                ));
            }
        }
        Ok(())
    }
}

/// The longest a run may take: the shared timeout plus a little for each region pass.
pub fn run_timeout(passes: usize) -> Duration {
    RUN_TIMEOUT + TIMEOUT_PER_PASS * passes as u32
}

/// Lowest pixel edge of a normalized coordinate (what the ComfyUI node floors to).
fn floor_px(v: u16, side: u32) -> u32 {
    v as u32 * side / 1000
}

/// Highest pixel edge of a normalized coordinate (what the node ceils to).
fn ceil_px(v: u16, side: u32) -> u32 {
    (v as u32 * side).div_ceil(1000)
}

/// A pixel edge measured from the canvas centre, as the layout JSON wants it.
fn centred(px: u32, side: u32) -> i64 {
    px as i64 - (side / 2) as i64
}

fn jstr(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

/// Check a layout request for a picture of `size` and build its prompt and passes.
pub fn check(req: &LayoutRequest, size: (u32, u32)) -> Result<Layout, String> {
    let (w, h) = size;
    let scene = req.scene.trim();
    if scene.is_empty() {
        return Err("Describe the whole picture first.".into());
    }
    if scene.chars().count() > MAX_PROMPT_CHARS {
        return Err(format!(
            "That description is too long (the limit is {MAX_PROMPT_CHARS} characters)."
        ));
    }
    let n = req.boxes.len();
    if n == 0 || n > MAX_BOXES {
        return Err(format!("Draw between 1 and {MAX_BOXES} boxes."));
    }
    let feather = req.feather.unwrap_or(DEFAULT_FEATHER_PX);
    if feather > MAX_FEATHER_PX {
        return Err(format!(
            "The feather can be at most {MAX_FEATHER_PX} pixels."
        ));
    }
    let mut items = Vec::with_capacity(n);
    let mut passes = Vec::new();
    for (i, b) in req.boxes.iter().enumerate() {
        let no = i + 1;
        let [y0, x0, y1, x1] = b.rect;
        if y0 >= y1 || x0 >= x1 || y1 > COORD_MAX || x1 > COORD_MAX {
            return Err(format!("Box {no} isn't a valid rectangle."));
        }
        let area = PxBox {
            x0: floor_px(x0, w),
            y0: floor_px(y0, h),
            x1: ceil_px(x1, w),
            y1: ceil_px(y1, h),
        };
        if area.x1 - area.x0 < MIN_BOX_PX || area.y1 - area.y0 < MIN_BOX_PX {
            return Err(format!(
                "Box {no} is too small (at least {MIN_BOX_PX} pixels on each side)."
            ));
        }
        let caption = b.caption.trim();
        if caption.chars().count() > MAX_BOX_CAPTION_CHARS {
            return Err(format!(
                "Box {no}'s description is too long (the limit is {MAX_BOX_CAPTION_CHARS} characters)."
            ));
        }
        let text = b.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        let mut item = format!(
            "{{\"label\":{},\"caption\":{}",
            jstr(b.kind.label()),
            jstr(caption)
        );
        match b.kind {
            BoxKind::Text => {
                let Some(t) = text else {
                    return Err(format!("Box {no} is a text box: type the words to draw."));
                };
                if t.chars().count() > MAX_BOX_TEXT_CHARS {
                    return Err(format!(
                        "Box {no}'s text is too long (the limit is {MAX_BOX_TEXT_CHARS} characters)."
                    ));
                }
                if b.lora.is_some() {
                    return Err(format!("Box {no} is a text box, which can't have a LoRA."));
                }
                item.push_str(&format!(",\"text\":{}", jstr(t)));
            }
            BoxKind::Object | BoxKind::Character => {
                if caption.is_empty() {
                    return Err(format!("Box {no} needs a description of what is in it."));
                }
                if text.is_some() {
                    return Err(format!(
                        "Box {no} isn't a text box, so it can't carry text."
                    ));
                }
                if let Some(l) = &b.lora {
                    if !valid_lora_name(&l.file) {
                        return Err(format!("Box {no} names a LoRA file that isn't valid."));
                    }
                    if !(MIN_STRENGTH..=MAX_STRENGTH).contains(&l.strength) {
                        return Err(format!(
                            "Box {no}: the LoRA strength must be between {MIN_STRENGTH} and {MAX_STRENGTH}."
                        ));
                    }
                    if !(MIN_DENOISE..=MAX_DENOISE).contains(&l.denoise) {
                        return Err(format!(
                            "Box {no}: the change amount must be between {MIN_DENOISE} and {MAX_DENOISE}."
                        ));
                    }
                    passes.push(Pass {
                        box_no: no,
                        lora: l.file.clone(),
                        strength: l.strength,
                        denoise: l.denoise,
                        area,
                    });
                }
            }
        }
        item.push_str(&format!(
            ",\"bbox\":[{},{},{},{}]}}",
            centred(area.y0, h),
            centred(area.x0, w),
            centred(area.y1, h),
            centred(area.x1, w)
        ));
        items.push(item);
    }
    if passes.len() > MAX_LORA_PASSES {
        return Err(format!(
            "At most {MAX_LORA_PASSES} boxes can have a LoRA in one picture."
        ));
    }
    // Same text as MOSSImageLayoutPrompt's compact `json.dumps`, key order included.
    let json = format!(
        "{{\"summary\":{{\"caption\":{}}},\"items\":[{}],\"bbox_coord_space\":\"abs_centralized\",\"bbox_format\":\"yxyx\"}}",
        jstr(scene),
        items.join(",")
    );
    if json.chars().count() > MAX_LAYOUT_JSON_CHARS {
        return Err("That layout has too much text in it. Shorten the descriptions.".into());
    }
    Ok(Layout {
        json,
        boxes: n,
        feather,
        passes,
    })
}

fn add(g: &mut Value, next: &mut u32, class: &str, inputs: Value) -> String {
    let id = next.to_string();
    *next += 1;
    g[id.as_str()] = json!({"class_type": class, "inputs": inputs});
    id
}

/// Add the region passes to the Fast graph [`super::build_graph`] made: the layout JSON
/// is already its prompt, node "8" decodes the layout picture, and "9" saves the result.
/// With no LoRA boxes the graph is left as it is (the layout picture is the result).
pub fn extend_graph(g: &mut Value, p: &Params, l: &Layout) {
    let (w, h) = p.size;
    let base_model = g["7"]["inputs"]["model"].clone();
    let mut next = 20u32;
    let mut img = json!(["8", 0]);
    for pass in &l.passes {
        let a = pass.area;
        let (bw, bh) = (a.x1 - a.x0, a.y1 - a.y0);
        let lora = add(
            g,
            &mut next,
            "LoraLoaderModelOnly",
            json!({"model": base_model, "lora_name": pass.lora, "strength_model": pass.strength}),
        );
        let zero = add(
            g,
            &mut next,
            "SolidMask",
            json!({"value": 0.0, "width": w, "height": h}),
        );
        let one = add(
            g,
            &mut next,
            "SolidMask",
            json!({"value": 1.0, "width": bw, "height": bh}),
        );
        // Feather the box's own mask before it goes onto the canvas: ComfyUI's FeatherMask
        // softens the borders of the mask image it is given, so on the full-canvas mask it
        // would only touch the picture's edge and leave every box edge hard (the R4 seams).
        // Only sides facing the picture's interior get a feather; a box side on the
        // picture's border has no seam to hide.
        let f = l.feather.min(bw.min(bh) / 4);
        let [left, top, right, bottom] =
            [a.x0 > 0, a.y0 > 0, a.x1 < w, a.y1 < h].map(|inside| if inside { f } else { 0 });
        let mut source = json!([one, 0]);
        if left + top + right + bottom > 0 {
            source = json!([
                add(
                    g,
                    &mut next,
                    "FeatherMask",
                    json!({"mask": source, "left": left, "top": top,
                           "right": right, "bottom": bottom}),
                ),
                0
            ]);
        }
        let mask = json!([
            add(
                g,
                &mut next,
                "MaskComposite",
                json!({"destination": [zero, 0], "source": source,
                       "x": a.x0, "y": a.y0, "operation": "add"}),
            ),
            0
        ]);
        let enc = add(
            g,
            &mut next,
            "VAEEncode",
            json!({"pixels": img, "vae": ["3", 0]}),
        );
        let noisy = add(
            g,
            &mut next,
            "SetLatentNoiseMask",
            json!({"samples": [enc, 0], "mask": mask}),
        );
        let sampled = add(
            g,
            &mut next,
            "KSampler",
            json!({"seed": p.seed, "steps": p.steps, "cfg": p.cfg,
                   "sampler_name": p.sampler, "scheduler": p.scheduler,
                   "denoise": pass.denoise, "model": [lora, 0],
                   "positive": ["4", 0], "negative": ["5", 0], "latent_image": [noisy, 0]}),
        );
        let dec = add(
            g,
            &mut next,
            "VAEDecode",
            json!({"samples": [sampled, 0], "vae": ["3", 0]}),
        );
        let pasted = add(
            g,
            &mut next,
            "ImageCompositeMasked",
            json!({"destination": img, "source": [dec, 0], "x": 0, "y": 0,
                   "resize_source": false, "mask": mask}),
        );
        img = json!([pasted, 0]);
    }
    g["9"]["inputs"]["images"] = img;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moss::{build_graph, ModelFiles, Request};

    const SCENE: &str =
        "A rainy neon city street at night, two characters standing side by side, cinematic.";
    const SUBJECT: &str = "roka_soldier, a tall character with smooth brown skin, large upright ears with pink interiors, four tendrils, gray cybernetic arm, standing";
    const OTHER: &str = "a gray-furred wolf woman in a red coat, standing";

    fn files() -> ModelFiles {
        ModelFiles {
            unet: "kromoss_bf16.safetensors".into(),
            clip: "qwen3.5_9b_int8_convrot.safetensors".into(),
            vae: "flux2-vae_small.safetensors".into(),
            lora: "kromoss-8-steps-dmd2-lora.safetensors".into(),
            time_delta: false,
        }
    }

    fn bx(kind: BoxKind, caption: &str, rect: [u16; 4]) -> BoxReq {
        BoxReq {
            kind,
            caption: caption.into(),
            text: None,
            rect,
            lora: None,
        }
    }

    fn with_lora(mut b: BoxReq, file: &str) -> BoxReq {
        b.lora = Some(LoraRef {
            file: file.into(),
            strength: 1.0,
            denoise: 0.7,
        });
        b
    }

    /// The spike's scene as normalized boxes that land on the spike's pixel boxes at 1024x1024.
    fn spike_boxes() -> Vec<BoxReq> {
        vec![
            bx(
                BoxKind::Object,
                "a rainy neon city street at night, wet pavement, glowing signs",
                [0, 0, 1000, 1000],
            ),
            bx(BoxKind::Character, SUBJECT, [196, 40, 976, 488]),
            bx(BoxKind::Character, OTHER, [196, 512, 976, 960]),
        ]
    }

    fn req(boxes: Vec<BoxReq>) -> LayoutRequest {
        LayoutRequest {
            scene: SCENE.into(),
            boxes,
            feather: None,
        }
    }

    fn request(layout: Option<LayoutRequest>) -> Request {
        Request {
            prompt: String::new(),
            width: 1024,
            height: 1024,
            steps: 8,
            sampler: "euler".into(),
            scheduler: "simple".into(),
            cfg: None,
            fast: true,
            seed: Some(12345),
            lora: None,
            lora_strength: None,
            control: false,
            layout,
        }
    }

    fn err(boxes: Vec<BoxReq>) -> String {
        check(&req(boxes), (1024, 1024)).unwrap_err()
    }

    /// `layout_json(CAPTION, BOXES)` from spikes/regional-lora/run.py, which mirrors the
    /// ComfyUI node, for the spike's boxes. Our builder must produce the same text.
    const SPIKE_JSON: &str = r#"{"summary":{"caption":"A rainy neon city street at night, two characters standing side by side, cinematic."},"items":[{"label":"object","caption":"a rainy neon city street at night, wet pavement, glowing signs","bbox":[-512,-512,512,512]},{"label":"character","caption":"roka_soldier, a tall character with smooth brown skin, large upright ears with pink interiors, four tendrils, gray cybernetic arm, standing","bbox":[-312,-472,488,-12]},{"label":"character","caption":"a gray-furred wolf woman in a red coat, standing","bbox":[-312,12,488,472]}],"bbox_coord_space":"abs_centralized","bbox_format":"yxyx"}"#;

    #[test]
    fn the_layout_json_matches_the_comfyui_node_for_the_spike_scene() {
        let l = check(&req(spike_boxes()), (1024, 1024)).unwrap();
        assert_eq!(l.json, SPIKE_JSON);
        assert_eq!(l.boxes, 3);
        assert!(l.passes.is_empty());
    }

    #[test]
    fn pixel_edges_follow_the_nodes_floor_and_ceil_and_the_canvas_centre() {
        // [y0, x0, y1, x1] = [200, 40, 1000, 500] on 1024x1024 -> [-308, -472, 512, 0].
        let l = check(
            &req(vec![bx(BoxKind::Object, "a", [200, 40, 1000, 500])]),
            (1024, 1024),
        )
        .unwrap();
        assert!(l.json.contains(r#""bbox":[-308,-472,512,0]"#), "{}", l.json);
        // A non-square canvas: 832 wide, 1216 tall, x 100..900, y 50..950.
        let l = check(
            &req(vec![bx(BoxKind::Object, "a", [50, 100, 950, 900])]),
            (832, 1216),
        )
        .unwrap();
        // y: floor(50*1216/1000)=60 -> 60-608=-548; ceil(950*1.216)=1156 -> 548.
        // x: floor(100*832/1000)=83 -> 83-416=-333; ceil(900*0.832)=749 -> 333.
        assert!(
            l.json.contains(r#""bbox":[-548,-333,548,333]"#),
            "{}",
            l.json
        );
    }

    #[test]
    fn text_boxes_carry_text_and_no_lora() {
        let mut t = bx(BoxKind::Text, "a neon sign", [100, 100, 300, 600]);
        t.text = Some("  OPEN  ".into());
        let l = check(&req(vec![t.clone()]), (1024, 1024)).unwrap();
        assert!(
            l.json
                .contains(r#"{"label":"text","caption":"a neon sign","text":"OPEN","bbox":"#),
            "{}",
            l.json
        );
        // No text, no LoRA, and an object box can't carry text.
        let mut none = t.clone();
        none.text = Some("  ".into());
        assert!(err(vec![none]).contains("text box"));
        let t = with_lora(t, "user/a.safetensors");
        assert!(err(vec![t]).contains("can't have a LoRA"));
        let mut o = bx(BoxKind::Object, "a cat", [0, 0, 500, 500]);
        o.text = Some("hi".into());
        assert!(err(vec![o]).contains("can't carry text"));
    }

    #[test]
    fn text_is_json_escaped_and_not_ascii_escaped() {
        let mut t = bx(BoxKind::Text, "sign", [100, 100, 300, 600]);
        t.text = Some("say \"héllo\"\n日本".into());
        let l = check(&req(vec![t]), (1024, 1024)).unwrap();
        assert!(
            l.json.contains(r#""text":"say \"héllo\"\n日本""#),
            "{}",
            l.json
        );
        serde_json::from_str::<Value>(&l.json).unwrap();
    }

    #[test]
    fn labels_and_draw_order_are_kept() {
        let l = check(&req(spike_boxes()), (1024, 1024)).unwrap();
        let v: Value = serde_json::from_str(&l.json).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["object", "character", "character"]);
        assert_eq!(v["bbox_format"], "yxyx");
        assert_eq!(v["bbox_coord_space"], "abs_centralized");
    }

    #[test]
    fn scene_and_box_counts_are_limited() {
        let ok = bx(BoxKind::Object, "a", [0, 0, 500, 500]);
        let mut r = req(vec![ok.clone()]);
        r.scene = "   ".into();
        assert!(check(&r, (1024, 1024)).is_err());
        r.scene = "x".repeat(MAX_PROMPT_CHARS + 1);
        assert!(check(&r, (1024, 1024)).is_err());
        assert!(check(&req(vec![]), (1024, 1024)).is_err());
        // 12 boxes pass, 13 don't.
        let many = |n: usize| (0..n).map(|_| ok.clone()).collect::<Vec<_>>();
        assert!(check(&req(many(MAX_BOXES)), (1024, 1024)).is_ok());
        assert!(check(&req(many(MAX_BOXES + 1)), (1024, 1024)).is_err());
    }

    #[test]
    fn rectangles_are_checked_at_both_edges_of_each_limit() {
        // Inverted, empty, and out of range.
        for rect in [
            [500, 0, 100, 500],
            [0, 500, 500, 100],
            [0, 0, 0, 500],
            [0, 0, 1001, 500],
        ] {
            assert!(err(vec![bx(BoxKind::Object, "a", rect)]).contains("valid rectangle"));
        }
        // 64 px minimum at 1024: 63/1000*1024 floors to 64 wide... use exact edges.
        // x 0..63 -> ceil(63*1.024)=65 wide: ok. x 0..62 -> ceil(63.5)=64 wide: ok.
        // x 0..61 -> ceil(62.46)=63 wide: too small.
        assert!(check(
            &req(vec![bx(BoxKind::Object, "a", [0, 0, 500, 62])]),
            (1024, 1024)
        )
        .is_ok());
        assert!(err(vec![bx(BoxKind::Object, "a", [0, 0, 500, 61])]).contains("too small"));
        assert!(err(vec![bx(BoxKind::Object, "a", [0, 0, 61, 500])]).contains("too small"));
        // The same fractions are bigger on a bigger canvas.
        assert!(check(
            &req(vec![bx(BoxKind::Object, "a", [0, 0, 500, 61])]),
            (2048, 1024)
        )
        .is_ok());
    }

    #[test]
    fn descriptions_are_required_and_limited() {
        assert!(
            err(vec![bx(BoxKind::Object, "  ", [0, 0, 500, 500])]).contains("needs a description")
        );
        let long = "x".repeat(MAX_BOX_CAPTION_CHARS);
        assert!(check(
            &req(vec![bx(BoxKind::Object, &long, [0, 0, 500, 500])]),
            (1024, 1024)
        )
        .is_ok());
        let too = "x".repeat(MAX_BOX_CAPTION_CHARS + 1);
        assert!(err(vec![bx(BoxKind::Object, &too, [0, 0, 500, 500])]).contains("too long"));
        // Twelve full captions alone fit (about 7000 characters of JSON); with a scene
        // description at its limit as well, the layout overflows the JSON budget.
        let boxes: Vec<_> = (0..MAX_BOXES)
            .map(|_| bx(BoxKind::Object, &long, [0, 0, 500, 500]))
            .collect();
        assert!(check(&req(boxes.clone()), (1024, 1024)).is_ok());
        let mut r = req(boxes);
        r.scene = "s".repeat(MAX_PROMPT_CHARS);
        assert!(check(&r, (1024, 1024))
            .unwrap_err()
            .contains("too much text"));
    }

    #[test]
    fn lora_values_are_checked_at_both_edges() {
        let b = |f: &dyn Fn(&mut LoraRef)| {
            let mut x = with_lora(
                bx(BoxKind::Character, "a", [0, 0, 500, 500]),
                "user/a.safetensors",
            );
            f(x.lora.as_mut().unwrap());
            check(&req(vec![x]), (1024, 1024))
        };
        for s in [0.0, 1.0, 1.5] {
            assert!(b(&|l| l.strength = s).is_ok(), "{s}");
        }
        for s in [-0.1, 1.51, f64::NAN, f64::INFINITY] {
            assert!(b(&|l| l.strength = s).is_err(), "{s}");
        }
        for d in [0.4, 0.7, 0.9] {
            assert!(b(&|l| l.denoise = d).is_ok(), "{d}");
        }
        for d in [0.39, 0.91, 0.0, 1.0, f64::NAN] {
            assert!(b(&|l| l.denoise = d).is_err(), "{d}");
        }
    }

    #[test]
    fn only_loras_on_the_gpu_with_plain_names_are_accepted() {
        // The name rules are `moss::valid_lora_name`'s (the sidecar's `user/` folder).
        for ok in ["user/a.safetensors", "user/km-mix-800-1a2b3c4d.safetensors"] {
            let b = with_lora(bx(BoxKind::Character, "a", [0, 0, 500, 500]), ok);
            assert!(check(&req(vec![b]), (1024, 1024)).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a.safetensors",
            "user/",
            "user/a.ckpt",
            "../a.safetensors",
            "user/../a.safetensors",
            "user/dir/a.safetensors",
            "user\\a.safetensors",
            "/abs.safetensors",
            "user/-a.safetensors",
            "user/a b.safetensors",
            "user/a;rm.safetensors",
        ] {
            let b = with_lora(bx(BoxKind::Character, "a", [0, 0, 500, 500]), bad);
            assert!(err(vec![b]).contains("isn't valid"), "{bad}");
        }
    }

    #[test]
    fn at_most_six_boxes_have_a_lora() {
        let boxes = |n: usize| -> Vec<BoxReq> {
            (0..n)
                .map(|_| {
                    with_lora(
                        bx(BoxKind::Character, "a", [0, 0, 500, 500]),
                        "user/a.safetensors",
                    )
                })
                .collect()
        };
        assert_eq!(check(&req(boxes(6)), (1024, 1024)).unwrap().passes.len(), 6);
        assert!(err(boxes(7)).contains("At most 6"));
    }

    #[test]
    fn feather_is_limited() {
        let mut r = req(vec![bx(BoxKind::Object, "a", [0, 0, 500, 500])]);
        r.feather = Some(MAX_FEATHER_PX);
        assert!(check(&r, (1024, 1024)).is_ok());
        r.feather = Some(MAX_FEATHER_PX + 1);
        assert!(check(&r, (1024, 1024)).is_err());
        r.feather = None;
        assert_eq!(check(&r, (1024, 1024)).unwrap().feather, DEFAULT_FEATHER_PX);
    }

    #[test]
    fn params_take_the_scene_and_need_fast_mode() {
        let boxes = spike_boxes();
        let p = Params::check(&request(Some(req(boxes.clone()))), 1).unwrap();
        // The layout JSON is the prompt; the request's own prompt is ignored (and may be empty).
        assert_eq!(p.prompt, SPIKE_JSON);
        assert_eq!(p.layout.as_ref().unwrap().boxes, 3);
        let mut r = request(Some(req(boxes)));
        r.fast = false;
        assert!(Params::check(&r, 1).unwrap_err().contains("Fast"));
        // A bad size is reported before the layout is looked at.
        let mut r = request(Some(req(vec![])));
        r.width = 1000;
        assert!(Params::check(&r, 1).unwrap_err().contains("multiple of 16"));
        // Without a layout, an empty prompt is still refused.
        assert!(Params::check(&request(None), 1).is_err());
    }

    #[test]
    fn requests_deserialize_with_defaults() {
        let r: Request = serde_json::from_value(json!({
            "prompt": "", "width": 1024, "height": 1024, "steps": 8,
            "layout": {"scene": "s", "boxes": [
                {"kind": "character", "caption": "c", "rect": [0, 0, 500, 500],
                 "lora": {"file": "user/a.safetensors"}},
                {"kind": "text", "text": "hi", "rect": [600, 0, 900, 500]}
            ]}
        }))
        .unwrap();
        let l = r.layout.unwrap();
        let lora = l.boxes[0].lora.as_ref().unwrap();
        assert_eq!((lora.strength, lora.denoise), (1.0, 0.7));
        assert_eq!(l.boxes[1].kind, BoxKind::Text);
        assert!(l.feather.is_none());
        // Plain requests are unchanged.
        let r: Request = serde_json::from_value(json!({
            "prompt": "a fox", "width": 1024, "height": 1024, "steps": 8
        }))
        .unwrap();
        assert!(r.layout.is_none());
        // Unknown kinds are rejected, not guessed.
        assert!(
            serde_json::from_value::<BoxReq>(json!({"kind": "monster", "rect": [0, 0, 1, 1]}))
                .is_err()
        );
    }

    fn graph(boxes: Vec<BoxReq>) -> (Value, Params) {
        let p = Params::check(&request(Some(req(boxes))), 1).unwrap();
        (build_graph(&files(), &p), p)
    }

    /// The nodes of one class, in node-id order (numeric, not the map's string order).
    fn nodes<'a>(g: &'a Value, class: &str) -> Vec<(&'a String, &'a Value)> {
        let mut v: Vec<_> = g
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, n)| n["class_type"] == class)
            .collect();
        v.sort_by_key(|(id, _)| id.parse::<u32>().unwrap());
        v
    }

    #[test]
    fn with_no_lora_boxes_the_graph_is_the_plain_fast_graph_with_the_layout_prompt() {
        let (g, p) = graph(spike_boxes());
        assert_eq!(g["4"]["inputs"]["text"], SPIKE_JSON);
        let plain = build_graph(
            &files(),
            &Params::check(&request_plain(&p.prompt), 1).unwrap(),
        );
        assert_eq!(g, plain);
        assert_eq!(g["9"]["inputs"]["images"], json!(["8", 0]));
    }

    fn request_plain(prompt: &str) -> Request {
        Request {
            prompt: prompt.into(),
            ..request(None)
        }
    }

    #[test]
    fn each_lora_box_adds_one_masked_pass_in_draw_order() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/km-mix-800.safetensors");
        boxes[2] = with_lora(boxes[2].clone(), "user/other.safetensors");
        boxes[2].lora.as_mut().unwrap().denoise = 0.5;
        let (g, p) = graph(boxes);
        assert_eq!(nodes(&g, "KSampler").len(), 3, "layout + 2 passes");
        assert_eq!(nodes(&g, "VAEEncode").len(), 2);
        assert_eq!(nodes(&g, "SetLatentNoiseMask").len(), 2);
        assert_eq!(nodes(&g, "ImageCompositeMasked").len(), 2);
        // The model chain for a pass is the base chain (Kromoss: DMD2 only), plus its LoRA.
        let loras: Vec<(&str, &Value)> = nodes(&g, "LoraLoaderModelOnly")
            .into_iter()
            .map(|(_, n)| {
                (
                    n["inputs"]["lora_name"].as_str().unwrap(),
                    &n["inputs"]["model"],
                )
            })
            .collect();
        assert_eq!(loras[0].0, "kromoss-8-steps-dmd2-lora.safetensors");
        assert_eq!(loras[1], ("user/km-mix-800.safetensors", &json!(["10", 0])));
        assert_eq!(loras[2], ("user/other.safetensors", &json!(["10", 0])));
        // Order: the first pass's composite feeds the second pass's encoder, the second
        // pass's composite is what gets saved, and the destination is the previous image.
        let comps = nodes(&g, "ImageCompositeMasked");
        let (id1, c1) = comps[0];
        let (id2, c2) = comps[1];
        assert_eq!(c1["inputs"]["destination"], json!(["8", 0]));
        assert_eq!(c2["inputs"]["destination"], json!([id1, 0]));
        assert_eq!(g["9"]["inputs"]["images"], json!([id2, 0]));
        let encs = nodes(&g, "VAEEncode");
        assert_eq!(encs[0].1["inputs"]["pixels"], json!(["8", 0]));
        assert_eq!(encs[1].1["inputs"]["pixels"], json!([id1, 0]));
        // Same seed, steps, sampler on every sampler; denoise per pass; layout pass untouched.
        let ks: Vec<&Value> = nodes(&g, "KSampler")
            .into_iter()
            .map(|(_, n)| &n["inputs"])
            .collect();
        for k in &ks {
            assert_eq!(
                (k["seed"].as_u64(), k["steps"].as_u64()),
                (Some(p.seed), Some(8))
            );
            assert_eq!(
                (&k["sampler_name"], &k["scheduler"]),
                (&json!("euler"), &json!("simple"))
            );
            assert_eq!(k["cfg"].as_f64(), Some(1.0));
            assert_eq!(k["positive"], json!(["4", 0]));
        }
        let dn: Vec<f64> = ks.iter().map(|k| k["denoise"].as_f64().unwrap()).collect();
        assert_eq!(dn, [1.0, 0.7, 0.5]);
        assert_eq!(g["7"]["inputs"]["denoise"], 1.0);
    }

    #[test]
    fn masks_cover_the_pixel_box_and_stay_inside_the_canvas() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/a.safetensors");
        let (g, _) = graph(boxes);
        let solids: Vec<&Value> = nodes(&g, "SolidMask")
            .into_iter()
            .map(|(_, n)| &n["inputs"])
            .collect();
        // The canvas of zeros, then the box of ones (the spike's 40..500 x 200..1000).
        assert_eq!(
            (
                solids[0]["value"].as_f64(),
                solids[0]["width"].as_u64(),
                solids[0]["height"].as_u64()
            ),
            (Some(0.0), Some(1024), Some(1024))
        );
        assert_eq!(
            (
                solids[1]["value"].as_f64(),
                solids[1]["width"].as_u64(),
                solids[1]["height"].as_u64()
            ),
            (Some(1.0), Some(460), Some(800))
        );
        let comp = &nodes(&g, "MaskComposite")[0].1["inputs"];
        assert_eq!(
            (comp["x"].as_u64(), comp["y"].as_u64()),
            (Some(40), Some(200))
        );
        assert_eq!(comp["operation"], "add");
        for (_, n) in nodes(&g, "MaskComposite") {
            let (x, y) = (
                n["inputs"]["x"].as_u64().unwrap(),
                n["inputs"]["y"].as_u64().unwrap(),
            );
            let mut src = &g[n["inputs"]["source"][0].as_str().unwrap()];
            if src["class_type"] == "FeatherMask" {
                // The feathered mask is the box's SolidMask with soft sides: same size.
                src = &g[src["inputs"]["mask"][0].as_str().unwrap()];
            }
            let src = &src["inputs"];
            assert!(x + src["width"].as_u64().unwrap() <= 1024);
            assert!(y + src["height"].as_u64().unwrap() <= 1024);
        }
        // The box's own mask of ones is feathered (this box touches no border, so on all four
        // sides) and only then composited onto the canvas; that composite is the mask the
        // pass uses for both the noise mask and the paste.
        let one_id = nodes(&g, "SolidMask")[1].0;
        let (fid, f) = nodes(&g, "FeatherMask")[0];
        assert_eq!(f["inputs"]["mask"], json!([one_id, 0]));
        for side in ["left", "top", "right", "bottom"] {
            assert_eq!(f["inputs"][side], 48, "{side}");
        }
        let (cid, comp) = nodes(&g, "MaskComposite")[0];
        assert_eq!(comp["inputs"]["source"], json!([fid, 0]));
        assert_eq!(
            nodes(&g, "SetLatentNoiseMask")[0].1["inputs"]["mask"],
            json!([cid, 0])
        );
        assert_eq!(
            nodes(&g, "ImageCompositeMasked")[0].1["inputs"]["mask"],
            json!([cid, 0])
        );
    }

    #[test]
    fn feather_only_faces_the_pictures_interior() {
        let sides = |rect: [u16; 4]| {
            let mut r = req(vec![with_lora(
                bx(BoxKind::Character, "a", rect),
                "user/a.safetensors",
            )]);
            r.feather = Some(32);
            let p = Params::check(&request(Some(r)), 1).unwrap();
            let g = build_graph(&files(), &p);
            nodes(&g, "FeatherMask").first().map(|(_, n)| {
                ["left", "top", "right", "bottom"].map(|s| n["inputs"][s].as_u64().unwrap())
            })
        };
        // [y0, x0, y1, x1]: touching the top-left corner, so no feather on those two sides.
        assert_eq!(sides([0, 0, 500, 500]), Some([0, 0, 32, 32]));
        // Touching the right and bottom borders only.
        assert_eq!(sides([500, 500, 1000, 1000]), Some([32, 32, 0, 0]));
        // The whole picture: nothing to feather, no node.
        assert_eq!(sides([0, 0, 1000, 1000]), None);
    }

    #[test]
    fn feather_zero_adds_no_node_and_big_feathers_are_clamped_to_the_box() {
        let mut r = req(spike_boxes());
        r.boxes[1] = with_lora(r.boxes[1].clone(), "user/a.safetensors");
        r.feather = Some(0);
        let p = Params::check(&request(Some(r.clone())), 1).unwrap();
        let g = build_graph(&files(), &p);
        assert!(nodes(&g, "FeatherMask").is_empty());
        // A 64 px box can't take a 128 px feather: a quarter of its short side at most.
        r.feather = Some(128);
        r.boxes = vec![with_lora(
            bx(BoxKind::Character, "a", [0, 0, 63, 500]),
            "user/a.safetensors",
        )];
        let p = Params::check(&request(Some(r)), 1).unwrap();
        let g = build_graph(&files(), &p);
        let w = p.layout.as_ref().unwrap().passes[0].area;
        let short = (w.x1 - w.x0).min(w.y1 - w.y0);
        // (This box touches the top-left corner, so its feather faces right and bottom only.)
        assert_eq!(
            nodes(&g, "FeatherMask")[0].1["inputs"]["right"].as_u64(),
            Some(u64::from(short / 4))
        );
    }

    #[test]
    fn layout_graphs_never_use_hooks_or_the_noise_making_sampling_node() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/a.safetensors");
        let (g, _) = graph(boxes);
        for (id, n) in g.as_object().unwrap() {
            let c = n["class_type"].as_str().unwrap();
            assert!(!c.contains("Hook"), "{id} {c}");
            assert_ne!(c, "ConditioningSetProperties", "{id}");
            assert_ne!(c, "ConditioningSetMask", "{id}");
            // ModelSamplingSD3 makes MOSS draw noise (docs/findings.md).
            assert_ne!(c, "ModelSamplingSD3", "{id}");
        }
        // One prompt only: the layout JSON; the negative stays empty.
        assert_eq!(nodes(&g, "CLIPTextEncode").len(), 2);
        assert_eq!(g["5"]["inputs"]["text"], "");
    }

    #[test]
    fn every_link_points_at_an_existing_node() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/a.safetensors");
        boxes[2] = with_lora(boxes[2].clone(), "user/b.safetensors");
        for files in [
            files(),
            ModelFiles {
                time_delta: true,
                ..files()
            },
        ] {
            let p = Params::check(&request(Some(req(boxes.clone()))), 1).unwrap();
            let g = build_graph(&files, &p);
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
    fn with_the_time_delta_the_pass_loras_sit_on_top_of_it() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/a.safetensors");
        let p = Params::check(&request(Some(req(boxes))), 1).unwrap();
        let g = build_graph(
            &ModelFiles {
                time_delta: true,
                ..files()
            },
            &p,
        );
        let pass = nodes(&g, "LoraLoaderModelOnly")
            .into_iter()
            .find(|(_, n)| n["inputs"]["lora_name"] == "user/a.safetensors")
            .unwrap();
        assert_eq!(pass.1["inputs"]["model"], json!(["11", 0]));
    }

    #[test]
    fn the_timeout_grows_with_the_passes_and_the_estimate_is_rough() {
        assert_eq!(run_timeout(0), RUN_TIMEOUT);
        assert_eq!(run_timeout(6), RUN_TIMEOUT + Duration::from_secs(540));
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/a.safetensors");
        boxes[2] = with_lora(boxes[2].clone(), "user/b.safetensors");
        let l = check(&req(boxes), (1024, 1024)).unwrap();
        assert_eq!(l.estimate_secs(), 65);
        assert_eq!(
            check(&req(spike_boxes()), (1024, 1024))
                .unwrap()
                .estimate_secs(),
            25
        );
    }

    #[test]
    fn named_loras_must_be_on_the_instance() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/km-mix-800.safetensors");
        let l = check(&req(boxes), (1024, 1024)).unwrap();
        assert!(l
            .check_known_loras(&["user/km-mix-800.safetensors".into()])
            .is_ok());
        let e = l
            .check_known_loras(&["user/other.safetensors".into()])
            .unwrap_err();
        assert!(e.contains("Box 2") && e.contains("km-mix-800"), "{e}");
    }

    #[test]
    fn the_caption_lists_boxes_and_loras() {
        let mut boxes = spike_boxes();
        boxes[1] = with_lora(boxes[1].clone(), "user/km-mix-800.safetensors");
        let p = Params::check(&request(Some(req(boxes))), 1).unwrap();
        assert_eq!(
            p.summary(),
            "1024×1024 · Fast · 8 steps · euler/simple · seed 12345 · layout: 3 boxes; km-mix-800 on box 2 (strength 1, change 0.7)"
        );
    }

    /// Writes the graphs the local acceptance scripts post to a running ComfyUI:
    /// `cargo test --lib dump_layout_graphs -- --ignored`. Uses the file names of the
    /// local C:\ai\ComfyUI install (Kromoss int8, the repo's DMD2 LoRA) and the spike's
    /// seed, so a layout-only run reproduces spikes/regional-lora/out/e0_layout.png.
    #[test]
    #[ignore = "writes files into spikes/regional-lora/graphs"]
    fn dump_layout_graphs() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../spikes/regional-lora/graphs");
        std::fs::create_dir_all(&dir).unwrap();
        let local = ModelFiles {
            unet: "kromoss_int8_convrot.safetensors".into(),
            clip: "qwen3.5_9b_int8_convrot.safetensors".into(),
            vae: "flux2-vae_small.safetensors".into(),
            lora: "8-steps-dmd2-lora.safetensors".into(),
            time_delta: false,
        };
        let subject = "user/km-mix-800.safetensors";
        let other = "user/km-smoke-comfy.safetensors";
        let mut one = spike_boxes();
        one[1] = with_lora(one[1].clone(), subject);
        let mut two = one.clone();
        two[2] = with_lora(two[2].clone(), other);
        let cases = [
            ("layout_only", spike_boxes()),
            ("one_region", one),
            ("two_regions", two),
        ];
        for (name, boxes) in cases {
            let p = Params::check(&request(Some(req(boxes))), 1).unwrap();
            let g = build_graph(&local, &p);
            let meta = json!({
                "name": name,
                "passes": p.layout.as_ref().unwrap().passes.iter()
                    .map(|x| json!({"box": x.box_no, "x0": x.area.x0, "y0": x.area.y0,
                                    "x1": x.area.x1, "y1": x.area.y1}))
                    .collect::<Vec<_>>(),
                "estimate_secs": p.layout.as_ref().unwrap().estimate_secs(),
            });
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_string_pretty(&json!({"graph": g, "meta": meta})).unwrap(),
            )
            .unwrap();
        }
    }

    /// Reads the acceptance requests in spikes/regional-lora/acceptance/requests (the JSON the
    /// window sends to `moss_generate`, through the real `Request` deserializer) and writes,
    /// for each, the graph and its twin with every LoRA removed (same prompt, same seed), for
    /// `spikes/regional-lora/acceptance.py`:
    /// `cargo test --lib dump_acceptance_graphs -- --ignored`.
    #[test]
    #[ignore = "reads and writes files under spikes/regional-lora/acceptance"]
    fn dump_acceptance_graphs() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../spikes/regional-lora/acceptance");
        let out = root.join("graphs");
        std::fs::create_dir_all(&out).unwrap();
        let local = ModelFiles {
            unet: "kromoss_int8_convrot.safetensors".into(),
            clip: "qwen3.5_9b_int8_convrot.safetensors".into(),
            vae: "flux2-vae_small.safetensors".into(),
            lora: "8-steps-dmd2-lora.safetensors".into(),
            time_delta: false,
        };
        let mut n = 0;
        for entry in std::fs::read_dir(root.join("requests")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let request: Request = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let p = Params::check(&request, 1).unwrap_or_else(|e| panic!("{name}: {e}"));
            let mut plain = request.clone();
            for b in &mut plain.layout.as_mut().unwrap().boxes {
                b.lora = None;
            }
            let pb = Params::check(&plain, 1).unwrap();
            let l = p.layout.as_ref().unwrap();
            // Every box in pixels, from the layout JSON's own conversion.
            let (w, h) = p.size;
            let boxes: Vec<Value> = request
                .layout
                .as_ref()
                .unwrap()
                .boxes
                .iter()
                .map(|b| {
                    let [y0, x0, y1, x1] = b.rect;
                    json!({"kind": format!("{:?}", b.kind), "lora": b.lora.as_ref().map(|l| l.file.clone()),
                           "x0": floor_px(x0, w), "y0": floor_px(y0, h), "x1": ceil_px(x1, w), "y1": ceil_px(y1, h)})
                })
                .collect();
            let meta = json!({
                "name": name, "width": w, "height": h, "seed": p.seed, "feather": l.feather,
                "passes": l.passes.iter().map(|x| json!({"box": x.box_no, "lora": x.lora, "denoise": x.denoise,
                    "x0": x.area.x0, "y0": x.area.y0, "x1": x.area.x1, "y1": x.area.y1})).collect::<Vec<_>>(),
                "boxes": boxes, "estimate_secs": l.estimate_secs(),
            });
            let doc = json!({"graph": build_graph(&local, &p), "base_graph": build_graph(&local, &pb), "meta": meta});
            std::fs::write(
                out.join(format!("{name}.json")),
                serde_json::to_string_pretty(&doc).unwrap(),
            )
            .unwrap();
            n += 1;
        }
        assert!(n >= 12, "expected the acceptance requests, found {n}");
    }
}
