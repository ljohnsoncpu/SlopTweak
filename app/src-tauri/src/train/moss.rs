//! LoRA training on Kromoss (MOSS-Image) with KohakuMossTrainer: settings and
//! the Python configs the trainer reads. Defaults come from the 2026-10-27
//! cloud runs in docs/spikes/kromoss-lora-training.md (21 images, int8
//! Kromoss, rank 32, 800 steps, a 4090 at about 2 s/step with INT8_COMPUTE).
//!
//! The trainer is fetched on the instance at a pinned commit and never
//! vendored here (its licence asks for "Kohaku" in the name of works built on
//! it; fetching it at run time is the reading the spike settled on, still the
//! user's call under PLAN §9).

use serde::{Deserialize, Serialize};

/// KohakuMossTrainer, pinned. The digest is `tree_digest` of GitHub's tarball
/// for this commit, computed 2026-10-27.
pub const MOSS_TRAINER_COMMIT: &str = "67f3bb54d8ee8f18384b11113f96b5b71d38c12f";
pub const MOSS_TRAINER_TREE_SHA256: &str =
    "d51a2ec20987f8e8b7b9088ee5f7943e120593250dc2d8b86e22a11163338f12";

/// Hugging Face revisions the weights are fetched at.
pub const KROMOSS_REV: &str = "a4fe97449297af2e3be7841e1c18e431f3b06031";
pub const COMFY_MOSS_REV: &str = "f28da8f93449c16cfa7efa07dcd1303a1c0c44d1";

/// A model file the trainer reads from its ComfyUI-style `models/` folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MossFile {
    /// Folder under `models/` and file name.
    pub path: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

pub const KROMOSS_INT8: MossFile = MossFile {
    path: "diffusion_models/kromoss_int8_convrot.safetensors",
    url: "https://huggingface.co/lodestones/Kromoss/resolve/a4fe97449297af2e3be7841e1c18e431f3b06031/diffusion_models/kromoss_int8_convrot.safetensors",
    sha256: "cacff0dd25cd2cf531e1f5ddae7872c3d57a54dbf66f1a1279f2f118514e7905",
    bytes: 5_413_698_958,
};
pub const QWEN_INT8: MossFile = MossFile {
    path: "text_encoders/qwen3.5_9b_int8_convrot.safetensors",
    url: "https://huggingface.co/Comfy-Org/MOSS-Image/resolve/f28da8f93449c16cfa7efa07dcd1303a1c0c44d1/text_encoders/qwen3.5_9b_int8_convrot.safetensors",
    sha256: "32e3787472f708fa5f83624acbf5ef23f7fc89c439e05065ea57cfe5a97f025a",
    bytes: 9_748_121_532,
};
pub const FLUX2_VAE: MossFile = MossFile {
    path: "vae/flux2-vae.safetensors",
    url: "https://huggingface.co/Comfy-Org/MOSS-Image/resolve/f28da8f93449c16cfa7efa07dcd1303a1c0c44d1/vae/flux2-vae.safetensors",
    sha256: "868fe7b343cc8f3a19dbcfcafbc3d5f888802be3f89bd81b65b3621a066ce8f3",
    bytes: 336_211_292,
};
pub const MOSS_FILES: [MossFile; 3] = [KROMOSS_INT8, QWEN_INT8, FLUX2_VAE];

impl MossFile {
    /// Name provision.sh stores the download under (flat, in the models dir).
    pub fn file_name(&self) -> &'static str {
        self.path.rsplit('/').next().unwrap_or(self.path)
    }
}

/// Where the runner (`MossRunner`) lays out the trainer's `models/` folder;
/// under job.rs's `WORK`.
pub const MOSS_MODELS_DIR: &str = "/opt/sloptweak/train/models";
/// Trainer data root on the instance: `nl/` holds images and sidecars,
/// `mix/train.jsonl` the mixed-caption rows.
pub const MOSS_DATA_DIR: &str = "/opt/sloptweak/train/data";
/// Config names the runner writes under the trainer's `configs/user/`
/// (`MOSS_RUN_CONFIG` in train_runner.py).
pub const MOSS_RUN_CONFIG: &str = "sloptweak-train.py";
pub const MOSS_DATA_CONFIG: &str = "sloptweak-train_data.py";
/// The runner caps a dataset at 1200 files (images plus sidecars).
pub const MAX_IMAGES: usize = 300;

/// Which captions the LoRA sees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptionMode {
    /// Natural-language sidecars only (`name.nl.txt`).
    Nl,
    /// Half natural language, half shuffled tags (`name.e621.txt`), trigger
    /// pinned first. Needs both sidecars on every image.
    #[default]
    Mix,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MossSettings {
    /// LoRA file stem; also the trainer's run name.
    pub output_name: String,
    pub rank: u32,
    pub alpha: u32,
    pub steps: u32,
    /// A checkpoint (resume state plus ComfyUI export) every this many steps.
    pub save_every: u32,
    pub caption_mode: CaptionMode,
    /// Share of samples drawn with the natural-language caption in `Mix`.
    pub p_nl: f64,
    /// Chance each non-trigger tag is dropped from a tag caption.
    pub tag_dropout: f64,
    /// int8 GEMMs (about 1.34x on a 4090). Changes the numerics, so it is a
    /// visible choice, not a hidden speed-up.
    pub int8_compute: bool,
    pub seed: u32,
    /// Prompt for the one sample made at the last step; empty = none.
    pub sample_prompt: String,
}

impl Default for MossSettings {
    fn default() -> Self {
        Self {
            output_name: "lora".into(),
            rank: 32,
            alpha: 32,
            steps: 800,
            save_every: 200,
            caption_mode: CaptionMode::Mix,
            p_nl: 0.5,
            tag_dropout: 0.1,
            int8_compute: true,
            seed: 20_261_027,
            sample_prompt: String::new(),
        }
    }
}

/// Learning-rate warm-up for runs shorter than the trainer's default 100
/// steps (it rejects a warm-up longer than the run).
const SHORT_RUN_STEPS: u32 = 200;
const SHORT_RUN_WARMUP: u32 = 2;

impl MossSettings {
    pub fn validate(&self) -> Result<(), String> {
        let ok_name = !self.output_name.is_empty()
            && self.output_name.len() <= 64
            && self
                .output_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !ok_name {
            return Err("The LoRA name may only use letters, digits, - and _.".into());
        }
        if !(1..=128).contains(&self.rank) || !(1..=128).contains(&self.alpha) {
            return Err("Rank and alpha must be between 1 and 128.".into());
        }
        if self.alpha > self.rank {
            return Err("Alpha above rank over-scales the LoRA; keep alpha <= rank.".into());
        }
        if !(10..=4000).contains(&self.steps) {
            return Err("Steps must be between 10 and 4000.".into());
        }
        if self.save_every == 0 || self.save_every > self.steps {
            return Err("Save interval must be between 1 and the step count.".into());
        }
        if !(0.0..=1.0).contains(&self.p_nl) || !(0.0..=0.9).contains(&self.tag_dropout) {
            return Err("Caption mix must be 0-1 and tag dropout 0-0.9.".into());
        }
        // The sample prompt is written into a Python string literal.
        if self.sample_prompt.len() > 500 || self.sample_prompt.chars().any(|c| c.is_control()) {
            return Err("The sample prompt must be one line of at most 500 characters.".into());
        }
        Ok(())
    }

    /// Steps at which a checkpoint exists when training ends.
    pub fn checkpoint_steps(&self) -> Vec<u32> {
        let mut steps: Vec<u32> = (1..)
            .map(|i| i * self.save_every)
            .take_while(|s| *s <= self.steps)
            .collect();
        if steps.last() != Some(&self.steps) {
            steps.push(self.steps);
        }
        steps
    }

    /// Name of the ComfyUI-format LoRA the trainer exports at `step`.
    /// Checkpoint directories are named by step; the export is `<run>-comfy`.
    pub fn export_dir(&self, step: u32) -> String {
        format!("checkpoints/step-{step}/export")
    }
}

/// A Python string literal. A JSON string is one, apart from the two
/// characters Python would also accept; control characters are refused by
/// `validate`, so this is only about quotes and backslashes.
fn py(s: &str) -> String {
    serde_json::to_string(s).expect("string serialises")
}

/// The training config: the `lora_24g_bf16` recipe plus our overrides. The
/// dataset root is where `render_jsonl`'s rows point.
pub fn render_train_config(s: &MossSettings) -> String {
    let datasets = match s.caption_mode {
        CaptionMode::Nl => format!(
            "DATASETS = [{{\"source\": \"folder\", \"path\": {nl}, \"txt_as\": \"nl\"}}]\n\
             CAPTION = {{\"template\": \"nl\"}}\n",
            nl = py(&format!("{MOSS_DATA_DIR}/nl")),
        ),
        CaptionMode::Mix => format!(
            "DATASETS = [{{\"source\": \"jsonl\", \"path\": {jsonl}, \"root\": {root}}}]\n\
             CAPTION = {{\"template\": \"mix\", \"p_nl\": {p_nl:?}, \"nl\": {{\"template\": \"nl\"}},\n\
             \x20          \"tags\": {{\"template\": \"tags\", \"keep\": [], \"keep_first\": 1,\n\
             \x20                   \"shuffle\": True, \"tag_dropout\": {drop:?}, \"underscores\": False}}}}\n\
             CAPTION_VARIANTS = 8\n",
            jsonl = py(&format!("{MOSS_DATA_DIR}/mix/train.jsonl")),
            root = py(&format!("{MOSS_DATA_DIR}/nl")),
            p_nl = s.p_nl,
            drop = s.tag_dropout,
        ),
    };
    let mut out = format!(
        "from kohakuengine import use_config\n\
         \n\
         use_config(\"../moss/recipes/recipes_lora_24g_bf16.py\")\n\
         \n\
         WEIGHTS = {weights}\n\
         TRANSFORMER = \"kromoss_int8_convrot\"\n\
         TEXT_ENCODER = \"qwen3.5_9b_int8_convrot\"\n\
         VAE = \"flux2-vae\"\n\
         {datasets}\
         ASPECT_RANGE = (0.33, 3.0)\n\
         ALGO = \"lora\"\n\
         DIM = {rank}\n\
         ALPHA = {alpha}.0\n\
         SEED = {seed}\n\
         STEPS = {steps}\n\
         SAVE_EVERY = {every}\n\
         LOG_EVERY = 10\n\
         RUN_NAME = {name}\n",
        weights = py(MOSS_MODELS_DIR),
        rank = s.rank,
        alpha = s.alpha,
        seed = s.seed,
        steps = s.steps,
        every = s.save_every,
        name = py(&s.output_name),
    );
    if s.sample_prompt.is_empty() {
        out.push_str("SAMPLE_EVERY = 0\n");
    } else {
        // One sample, at the last step only (a 50-step sample costs ~15 steps).
        out.push_str(&format!(
            "SAMPLE_PROMPTS = [{}]\nSAMPLE_EVERY = {}\n",
            py(&s.sample_prompt),
            s.steps
        ));
    }
    if s.steps < SHORT_RUN_STEPS {
        out.push_str(&format!(
            "SCHEDULE = {{\"mode\": \"cosine\", \"warmup\": {SHORT_RUN_WARMUP}, \"end\": -1, \"min_value\": 0.1}}\n"
        ));
    }
    if s.int8_compute {
        out.push_str("INT8_COMPUTE = True\n");
    }
    out
}

/// The data-preparation config (`cache_text.py`, `cache_latents.py`). In it
/// `TEXT_ENCODER`/`IMAGE_ENCODER` are encoder factory paths, not weight
/// files, so the train config's values are reset (spike, "Config gotcha").
pub fn render_data_config(run_config: &str) -> String {
    format!(
        "from kohakuengine import use_config\n\
         \n\
         use_config({run})\n\
         TEXT_ENCODER = \"kohakumoss.encoders.text_encoder\"\n\
         IMAGE_ENCODER = \"kohakumoss.encoders.vae_encoder\"\n\
         TEXT_ENCODER_KWARGS = {{\"path\": {models}, \"device\": \"cuda\"}}\n\
         IMAGE_ENCODER_KWARGS = {{\"path\": {models}, \"device\": \"cuda\"}}\n",
        run = py(run_config),
        models = py(MOSS_MODELS_DIR),
    )
}

/// One image's captions for the `mix` jsonl.
#[derive(Debug, Clone, PartialEq)]
pub struct MixRow {
    /// Image path relative to the dataset root.
    pub image: String,
    pub nl: String,
    /// e621 tags, trigger first.
    pub tags: Vec<String>,
}

/// `train.jsonl` rows: `{"image", "caption", "tags"}`, one per line.
pub fn render_jsonl(rows: &[MixRow]) -> Result<String, String> {
    let mut out = String::new();
    for r in rows {
        if r.nl.trim().is_empty() || r.tags.is_empty() {
            return Err(format!(
                "{}: mixed captions need both a natural-language and a tag caption.",
                r.image
            ));
        }
        let line = serde_json::json!({ "image": r.image, "caption": r.nl, "tags": r.tags });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    Ok(out)
}

/// Split a `name.e621.txt` line ("a, b_c, d") into tags.
pub fn parse_tags(line: &str) -> Vec<String> {
    line.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One training image and its caption sidecars, hashed for the job.
#[derive(Debug, Clone, PartialEq)]
pub struct MossImage {
    pub file: String,
    pub sha256: String,
    /// `name.nl.txt`, and `name.e621.txt` (only for `Mix`).
    pub nl_file: String,
    pub nl_sha256: String,
    pub nl: String,
    pub tags_file: Option<String>,
    pub tags_sha256: Option<String>,
    pub tags: Vec<String>,
}

/// File names the instance accepts (train_runner.py `SAFE_NAME`).
fn safe_name(n: &str) -> bool {
    let mut bytes = n.bytes();
    n.len() <= 128
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn sha_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

/// Find the images (`.png`, `.jpg`) in a flat folder with their sidecars.
/// `Nl` needs `name.nl.txt`; `Mix` also needs `name.e621.txt`.
pub fn collect(dir: &std::path::Path, mode: CaptionMode) -> Result<Vec<MossImage>, String> {
    let read = |p: std::path::PathBuf| -> Result<Vec<u8>, String> {
        std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            l.ends_with(".png") || l.ends_with(".jpg")
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return Err("The folder has no .png or .jpg images.".into());
    }
    if names.len() > MAX_IMAGES {
        return Err(format!("Use at most {MAX_IMAGES} images."));
    }
    let mut out = Vec::new();
    for file in names {
        if !safe_name(&file) {
            return Err(format!(
                "{file}: file names may only use letters, digits, . _ - (and start with one)."
            ));
        }
        let stem = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
        let nl_file = format!("{stem}.nl.txt");
        let nl_bytes = read(dir.join(&nl_file))
            .map_err(|_| format!("{file}: missing the natural-language caption {nl_file}."))?;
        let nl = String::from_utf8(nl_bytes.clone())
            .map_err(|_| format!("{nl_file} is not UTF-8 text."))?
            .trim()
            .to_owned();
        if nl.is_empty() {
            return Err(format!("{nl_file} is empty."));
        }
        let (tags_file, tags_sha256, tags) = if mode == CaptionMode::Mix {
            let f = format!("{stem}.e621.txt");
            let bytes = read(dir.join(&f))
                .map_err(|_| format!("{file}: mixed captions need the tag caption {f}."))?;
            let text =
                String::from_utf8(bytes.clone()).map_err(|_| format!("{f} is not UTF-8 text."))?;
            let tags = parse_tags(&text.replace('\n', ","));
            if tags.is_empty() {
                return Err(format!("{f} has no tags."));
            }
            (Some(f), Some(sha_hex(&bytes)), tags)
        } else {
            (None, None, Vec::new())
        };
        out.push(MossImage {
            sha256: sha_hex(&read(dir.join(&file))?),
            file,
            nl_file,
            nl_sha256: sha_hex(&nl_bytes),
            nl,
            tags_file,
            tags_sha256,
            tags,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_proven_run() {
        let s = MossSettings::default();
        s.validate().unwrap();
        assert_eq!(s.checkpoint_steps(), vec![200, 400, 600, 800]);
        assert_eq!(s.caption_mode, CaptionMode::Mix);
        assert!(s.int8_compute);
    }

    #[test]
    fn mix_config_matches_the_run_that_trained() {
        let cfg = render_train_config(&MossSettings {
            output_name: "roka".into(),
            ..Default::default()
        });
        for want in [
            "TRANSFORMER = \"kromoss_int8_convrot\"",
            "DATASETS = [{\"source\": \"jsonl\", \"path\": \"/opt/sloptweak/train/data/mix/train.jsonl\", \"root\": \"/opt/sloptweak/train/data/nl\"}]",
            "\"p_nl\": 0.5",
            "\"tag_dropout\": 0.1",
            "\"keep_first\": 1",
            "CAPTION_VARIANTS = 8",
            "DIM = 32",
            "ALPHA = 32.0",
            "STEPS = 800",
            "SAVE_EVERY = 200",
            "RUN_NAME = \"roka\"",
            "SAMPLE_EVERY = 0",
            "INT8_COMPUTE = True",
        ] {
            assert!(cfg.contains(want), "missing {want:?} in\n{cfg}");
        }
        assert!(!cfg.contains("SCHEDULE"));
    }

    #[test]
    fn nl_config_reads_the_folder() {
        let cfg = render_train_config(&MossSettings {
            caption_mode: CaptionMode::Nl,
            int8_compute: false,
            ..Default::default()
        });
        assert!(cfg.contains("\"source\": \"folder\""));
        assert!(cfg.contains("\"txt_as\": \"nl\""));
        assert!(!cfg.contains("CAPTION_VARIANTS"));
        assert!(!cfg.contains("INT8_COMPUTE"));
    }

    #[test]
    fn short_runs_get_a_short_warmup() {
        let cfg = render_train_config(&MossSettings {
            steps: 20,
            save_every: 20,
            ..Default::default()
        });
        assert!(cfg.contains("\"warmup\": 2"));
    }

    #[test]
    fn sample_prompt_is_quoted_and_sampled_once() {
        let s = MossSettings {
            sample_prompt: "roka, \"quoted\" \\ back".into(),
            ..Default::default()
        };
        s.validate().unwrap();
        let cfg = render_train_config(&s);
        assert!(cfg.contains(r#"SAMPLE_PROMPTS = ["roka, \"quoted\" \\ back"]"#));
        assert!(cfg.contains("SAMPLE_EVERY = 800"));
    }

    #[test]
    fn data_config_resets_the_encoders() {
        let d = render_data_config("km-mix.py");
        assert!(d.contains("use_config(\"km-mix.py\")"));
        assert!(d.contains("TEXT_ENCODER = \"kohakumoss.encoders.text_encoder\""));
        assert!(d.contains("IMAGE_ENCODER_KWARGS = {\"path\": \"/opt/sloptweak/train/models\""));
    }

    #[test]
    fn checkpoints_end_on_the_last_step() {
        let s = MossSettings {
            steps: 500,
            save_every: 200,
            ..Default::default()
        };
        assert_eq!(s.checkpoint_steps(), vec![200, 400, 500]);
    }

    #[test]
    fn validation_rejects_nonsense() {
        let d = MossSettings::default;
        for bad in [
            MossSettings {
                output_name: "a b".into(),
                ..d()
            },
            MossSettings {
                output_name: "../x".into(),
                ..d()
            },
            MossSettings { rank: 0, ..d() },
            MossSettings { alpha: 64, ..d() },
            MossSettings { steps: 5, ..d() },
            MossSettings { steps: 5000, ..d() },
            MossSettings {
                save_every: 0,
                ..d()
            },
            MossSettings {
                save_every: 900,
                ..d()
            },
            MossSettings { p_nl: 1.5, ..d() },
            MossSettings {
                tag_dropout: 0.95,
                ..d()
            },
            MossSettings {
                sample_prompt: "a\nb".into(),
                ..d()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn jsonl_rows_carry_both_captions() {
        let rows = [MixRow {
            image: "a.png".into(),
            nl: "roka_soldier, a smiling woman.".into(),
            tags: parse_tags("roka_soldier, solo,  smile ,"),
        }];
        let text = render_jsonl(&rows).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(v["image"], "a.png");
        assert_eq!(
            v["tags"],
            serde_json::json!(["roka_soldier", "solo", "smile"])
        );
        assert!(text.ends_with('\n'));
        let missing = [MixRow {
            image: "b.png".into(),
            nl: "x".into(),
            tags: vec![],
        }];
        assert!(render_jsonl(&missing).is_err());
    }

    #[test]
    fn pinned_files_are_hex() {
        for f in MOSS_FILES {
            assert_eq!(f.sha256.len(), 64, "{}", f.path);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(f.url.starts_with("https://huggingface.co/"));
        }
        assert!(KROMOSS_INT8.url.contains(KROMOSS_REV));
        assert!(QWEN_INT8.url.contains(COMFY_MOSS_REV));
        assert_eq!(MOSS_TRAINER_COMMIT.len(), 40);
    }

    fn folder(mix: bool) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for n in ["a", "b"] {
            std::fs::write(d.path().join(format!("{n}.png")), format!("img-{n}")).unwrap();
            std::fs::write(d.path().join(format!("{n}.nl.txt")), "roka, a woman.\n").unwrap();
            if mix {
                std::fs::write(d.path().join(format!("{n}.e621.txt")), "roka, solo\nsmile")
                    .unwrap();
            }
        }
        std::fs::write(d.path().join("a.txt"), "sdxl, tags").unwrap(); // ignored
        d
    }

    #[test]
    fn collect_finds_images_with_both_sidecars() {
        let d = folder(true);
        let imgs = collect(d.path(), CaptionMode::Mix).unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].file, "a.png");
        assert_eq!(imgs[0].nl, "roka, a woman.");
        assert_eq!(imgs[0].tags, ["roka", "solo", "smile"]);
        assert_eq!(imgs[0].sha256, sha_hex(b"img-a"));
        assert_eq!(imgs[0].tags_file.as_deref(), Some("a.e621.txt"));
    }

    #[test]
    fn nl_mode_does_not_need_tags() {
        let d = folder(false);
        let imgs = collect(d.path(), CaptionMode::Nl).unwrap();
        assert!(imgs
            .iter()
            .all(|i| i.tags_file.is_none() && i.tags.is_empty()));
        let err = collect(d.path(), CaptionMode::Mix).unwrap_err();
        assert!(err.contains("a.e621.txt"), "{err}");
    }

    #[test]
    fn collect_refuses_bad_folders() {
        let empty = tempfile::tempdir().unwrap();
        assert!(collect(empty.path(), CaptionMode::Nl).is_err());
        let d = folder(false);
        std::fs::remove_file(d.path().join("b.nl.txt")).unwrap();
        let err = collect(d.path(), CaptionMode::Nl).unwrap_err();
        assert!(err.contains("b.nl.txt"), "{err}");
        let d = folder(false);
        std::fs::write(d.path().join("c d.png"), "x").unwrap();
        let err = collect(d.path(), CaptionMode::Nl).unwrap_err();
        assert!(err.contains("c d.png"), "{err}");
        let d = folder(false);
        std::fs::write(d.path().join("a.nl.txt"), "  \n").unwrap();
        let err = collect(d.path(), CaptionMode::Nl).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn paths_agree_with_the_runner() {
        assert!(MOSS_MODELS_DIR.starts_with("/opt/sloptweak/train/"));
        assert!(MOSS_DATA_DIR.starts_with("/opt/sloptweak/train/"));
        assert_eq!(KROMOSS_INT8.file_name(), "kromoss_int8_convrot.safetensors");
        assert_eq!(MOSS_RUN_CONFIG, "sloptweak-train.py");
        assert_eq!(MOSS_DATA_CONFIG, "sloptweak-train_data.py");
    }
}
