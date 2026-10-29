//! User-facing training settings and the kohya sd-scripts configs they
//! produce. The defaults reproduce the validated Thalia run (SDXL U-Net LoRA,
//! rank/alpha 16, AdamW 1e-4, 23 images x 40 epochs = 920 steps).

use serde::{Deserialize, Serialize};

/// Fixed by the validated environment: single GPU, batch 1, no accumulation.
/// With these, optimizer steps = images x repeats x epochs.
pub const BATCH_SIZE: u32 = 1;

/// How many test images a run makes at the end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalPreset {
    /// A quick look: the first few prompts, one seed, the base model and the
    /// final checkpoint at full strength.
    #[default]
    Quick,
    /// Every checkpoint at every strength, every seed: for choosing between
    /// checkpoints.
    Thorough,
}

/// Prompts used by the Quick preset.
pub const QUICK_PROMPTS: usize = 6;
/// Strength the Quick preset tests at.
pub const QUICK_STRENGTH: f64 = 1.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrainSettings {
    /// File stem of the saved LoRA; also the checkpoint prefix.
    pub output_name: String,
    pub rank: u32,
    pub alpha: u32,
    pub learning_rate: f64,
    pub epochs: u32,
    pub repeats: u32,
    /// Fraction of total steps spent warming up the learning rate.
    pub warmup_fraction: f64,
    /// Bucket target area is `resolution`².
    pub resolution: u32,
    /// Checkpoints are saved at evenly spaced steps, the last at the end.
    pub save_points: u32,
    pub seed: u32,
    /// Leading comma-separated tags never shuffled or dropped (the trigger).
    pub keep_tokens: u32,
    /// LoRA strengths rendered in the evaluation grid.
    /// Used by the Thorough preset; Quick tests one strength and one seed.
    pub eval_strengths: Vec<f64>,
    pub eval_seeds: Vec<u32>,
    pub eval_preset: EvalPreset,
    /// Train exactly this many steps instead of `epochs` passes over the
    /// images (used to keep training an existing LoRA for a chosen length).
    pub steps_override: Option<u32>,
}

impl Default for TrainSettings {
    fn default() -> Self {
        Self {
            output_name: "lora".into(),
            rank: 16,
            alpha: 16,
            learning_rate: 1e-4,
            epochs: 40,
            repeats: 1,
            warmup_fraction: 0.05,
            resolution: 1024,
            save_points: 4,
            seed: 20_261_025,
            keep_tokens: 1,
            eval_strengths: vec![0.5, 0.75, 1.0],
            eval_seeds: vec![104_729, 130_363],
            eval_preset: EvalPreset::Quick,
            steps_override: None,
        }
    }
}

impl TrainSettings {
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
        if !(self.learning_rate > 0.0 && self.learning_rate <= 1e-3) {
            return Err("Learning rate must be above 0 and at most 0.001.".into());
        }
        if !(1..=400).contains(&self.epochs) || !(1..=50).contains(&self.repeats) {
            return Err("Epochs must be 1-400 and repeats 1-50.".into());
        }
        if self
            .steps_override
            .is_some_and(|n| !(1..=20_000).contains(&n))
        {
            return Err("Steps must be between 1 and 20,000.".into());
        }
        if !(0.0..=0.5).contains(&self.warmup_fraction) {
            return Err("Warm-up must be between 0% and 50% of the steps.".into());
        }
        if !(512..=1536).contains(&self.resolution) || !self.resolution.is_multiple_of(64) {
            return Err("Resolution must be a multiple of 64 between 512 and 1536.".into());
        }
        if !(1..=10).contains(&self.save_points) {
            return Err("Save between 1 and 10 checkpoints.".into());
        }
        if self.keep_tokens > 8 {
            return Err("Keep at most 8 leading tags fixed.".into());
        }
        if self.eval_strengths.is_empty()
            || self.eval_strengths.iter().any(|s| !(0.0..=2.0).contains(s))
        {
            return Err("Evaluation strengths must be between 0 and 2.".into());
        }
        if self.eval_seeds.is_empty() || self.eval_seeds.len() > 4 {
            return Err("Use 1-4 evaluation seeds.".into());
        }
        Ok(())
    }

    pub fn total_steps(&self, images: u32) -> u32 {
        self.steps_override
            .unwrap_or(images * self.repeats * self.epochs / BATCH_SIZE)
    }

    pub fn warmup_steps(&self, images: u32) -> u32 {
        (f64::from(self.total_steps(images)) * self.warmup_fraction).round() as u32
    }

    /// Steps between saves (at least 1).
    pub fn save_interval(&self, images: u32) -> u32 {
        (self.total_steps(images) / self.save_points).max(1)
    }

    /// Steps at which a checkpoint exists when training ends.
    pub fn checkpoint_steps(&self, images: u32) -> Vec<u32> {
        let total = self.total_steps(images);
        let every = self.save_interval(images);
        let mut steps: Vec<u32> = (1..)
            .map(|i| i * every)
            .take_while(|s| *s <= total)
            .collect();
        if steps.last() != Some(&total) {
            steps.push(total);
        }
        steps
    }

    /// Checkpoints the test images are made with.
    pub fn eval_checkpoints(&self, images: u32) -> Vec<u32> {
        let all = self.checkpoint_steps(images);
        match self.eval_preset {
            EvalPreset::Quick => all.last().copied().into_iter().collect(),
            EvalPreset::Thorough => all,
        }
    }

    pub fn eval_strength_list(&self) -> Vec<f64> {
        match self.eval_preset {
            EvalPreset::Quick => vec![QUICK_STRENGTH],
            EvalPreset::Thorough => self.eval_strengths.clone(),
        }
    }

    pub fn eval_seed_list(&self) -> Vec<u32> {
        match self.eval_preset {
            EvalPreset::Quick => self.eval_seeds.iter().take(1).copied().collect(),
            EvalPreset::Thorough => self.eval_seeds.clone(),
        }
    }

    /// How many of the prompts the preset uses.
    pub fn eval_prompt_limit(&self) -> usize {
        match self.eval_preset {
            EvalPreset::Quick => QUICK_PROMPTS,
            EvalPreset::Thorough => usize::MAX,
        }
    }

    /// Name of the checkpoint kohya writes at `step`.
    pub fn checkpoint_file(&self, step: u32) -> String {
        format!("{}-step{step:08}.safetensors", self.output_name)
    }
}

/// Remote paths the configs point at.
#[derive(Debug, Clone)]
pub struct TrainPaths {
    pub model: String,
    pub image_dir: String,
    pub dataset_config: String,
    pub output_dir: String,
    pub logging_dir: String,
    pub tokenizer_cache_dir: String,
}

/// A TOML basic string. A JSON string literal is a valid one.
fn q(s: &str) -> String {
    serde_json::to_string(s).expect("string serialises")
}

pub fn render_dataset_toml(s: &TrainSettings, image_dir: &str) -> String {
    format!(
        "[general]\n\
         caption_extension = \".txt\"\n\
         keep_tokens = {keep}\n\
         shuffle_caption = false\n\
         caption_dropout_rate = 0.0\n\
         caption_tag_dropout_rate = 0.0\n\
         flip_aug = false\n\
         random_crop = false\n\
         color_aug = false\n\
         \n\
         [[datasets]]\n\
         resolution = [{res}, {res}]\n\
         enable_bucket = true\n\
         bucket_no_upscale = false\n\
         min_bucket_reso = {min}\n\
         max_bucket_reso = {max}\n\
         bucket_reso_steps = {step}\n\
         batch_size = {batch}\n\
         \n\
         [[datasets.subsets]]\n\
         image_dir = {dir}\n\
         num_repeats = {repeats}\n",
        keep = s.keep_tokens,
        res = s.resolution,
        min = BUCKET_MIN,
        max = BUCKET_MAX,
        step = BUCKET_STEP,
        batch = BATCH_SIZE,
        dir = q(image_dir),
        repeats = s.repeats,
    )
}

pub const BUCKET_MIN: u32 = 512;
pub const BUCKET_MAX: u32 = 1536;
pub const BUCKET_STEP: u32 = 32;

pub fn render_train_toml(s: &TrainSettings, images: u32, p: &TrainPaths) -> String {
    format!(
        "network_module = \"networks.lora\"\n\
         network_dim = {rank}\n\
         network_alpha = {alpha}\n\
         network_train_unet_only = true\n\
         learning_rate = {lr:?}\n\
         unet_lr = {lr:?}\n\
         optimizer_type = \"AdamW\"\n\
         optimizer_args = [\"weight_decay=0.01\", \"betas=0.9,0.999\", \"eps=1e-8\"]\n\
         lr_scheduler = \"constant_with_warmup\"\n\
         lr_warmup_steps = {warmup}\n\
         max_train_steps = {total}\n\
         train_batch_size = {batch}\n\
         gradient_accumulation_steps = 1\n\
         max_grad_norm = 1.0\n\
         mixed_precision = \"bf16\"\n\
         save_precision = \"fp16\"\n\
         no_half_vae = true\n\
         sdpa = true\n\
         gradient_checkpointing = true\n\
         cache_latents = true\n\
         cache_latents_to_disk = true\n\
         cache_text_encoder_outputs = true\n\
         cache_text_encoder_outputs_to_disk = true\n\
         max_token_length = 150\n\
         seed = {seed}\n\
         max_data_loader_n_workers = 0\n\
         save_model_as = \"safetensors\"\n\
         output_name = {name}\n\
         save_every_n_steps = {every}\n\
         save_state = true\n\
         save_state_on_train_end = true\n\
         log_with = \"tensorboard\"\n\
         pretrained_model_name_or_path = {model}\n\
         dataset_config = {dataset}\n\
         output_dir = {out}\n\
         logging_dir = {log}\n\
         tokenizer_cache_dir = {tok}\n",
        rank = s.rank,
        alpha = s.alpha,
        lr = s.learning_rate,
        warmup = s.warmup_steps(images),
        total = s.total_steps(images),
        batch = BATCH_SIZE,
        seed = s.seed,
        name = q(&s.output_name),
        every = s.save_interval(images),
        model = q(&p.model),
        dataset = q(&p.dataset_config),
        out = q(&p.output_dir),
        log = q(&p.logging_dir),
        tok = q(&p.tokenizer_cache_dir),
    )
}

/// One evaluation prompt: an id for the contact sheet and the tag string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPrompt {
    pub id: String,
    pub prompt: String,
}

pub const EVAL_NEGATIVE: &str =
    "worst quality, low quality, watermark, text, signature, extra limbs";

/// `sdxl_gen_img.py --from_file` lines: every prompt once per seed. Prompts
/// must be one line without the `--` option marker.
pub fn render_eval_prompts(
    s: &TrainSettings,
    prompts: &[EvalPrompt],
    negative: &str,
) -> Result<String, String> {
    let bad = |t: &str| t.contains('\n') || t.contains('\r') || t.contains("--");
    if prompts.is_empty() {
        return Err("Add at least one evaluation prompt.".into());
    }
    if bad(negative) || prompts.iter().any(|p| bad(&p.prompt)) {
        return Err("Evaluation prompts must be one line and must not contain \"--\".".into());
    }
    let mut out = String::new();
    for p in prompts {
        for seed in &s.eval_seed_list() {
            out.push_str(&format!(
                "{} --d {seed} --w 832 --h 1216 --s 20 --l 3.5 --n {negative}\n",
                p.prompt
            ));
        }
    }
    Ok(out)
}

/// Images in the evaluation: the base control plus each tested checkpoint at
/// each strength, every prompt x seed.
pub fn eval_image_count(s: &TrainSettings, images: u32, prompts: usize) -> usize {
    let per_run = prompts * s.eval_seed_list().len();
    per_run * (1 + s.eval_checkpoints(images).len() * s.eval_strength_list().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> TrainPaths {
        TrainPaths {
            model: "/w/models/m.safetensors".into(),
            image_dir: "/w/data".into(),
            dataset_config: "/w/runtime/dataset.toml".into(),
            output_dir: "/w/runtime/checkpoints".into(),
            logging_dir: "/w/runtime/logs".into(),
            tokenizer_cache_dir: "/w/tokenizers".into(),
        }
    }

    fn thorough() -> TrainSettings {
        TrainSettings {
            eval_preset: EvalPreset::Thorough,
            ..Default::default()
        }
    }

    #[test]
    fn quick_is_the_default_and_makes_a_dozen_images() {
        let s = TrainSettings::default();
        assert_eq!(s.eval_preset, EvalPreset::Quick);
        assert_eq!(s.eval_checkpoints(23), vec![920]);
        assert_eq!(s.eval_strength_list(), vec![1.0]);
        assert_eq!(s.eval_seed_list(), vec![104_729]);
        // 6 prompts x 1 seed x (base + final checkpoint).
        assert_eq!(eval_image_count(&s, 23, s.eval_prompt_limit()), 12);
        // A saved settings file from before the preset existed still loads.
        let old: TrainSettings = serde_json::from_str(r#"{"rank": 8, "alpha": 8}"#).unwrap();
        assert_eq!(old.eval_preset, EvalPreset::Quick);
    }

    #[test]
    fn thorough_tests_every_checkpoint_at_every_strength() {
        let s = thorough();
        assert_eq!(s.eval_checkpoints(23), vec![230, 460, 690, 920]);
        assert_eq!(s.eval_strength_list(), vec![0.5, 0.75, 1.0]);
        assert_eq!(s.eval_seed_list().len(), 2);
        // 8 prompts x 2 seeds x (1 + 4 x 3) = 208, the first real run.
        assert_eq!(eval_image_count(&s, 23, 8), 208);
    }

    #[test]
    fn defaults_reproduce_the_validated_run() {
        let s = thorough();
        assert_eq!(s.total_steps(23), 920);
        assert_eq!(s.warmup_steps(23), 46);
        assert_eq!(s.save_interval(23), 230);
        assert_eq!(s.checkpoint_steps(23), vec![230, 460, 690, 920]);
        assert_eq!(s.checkpoint_file(230), "lora-step00000230.safetensors");
        // 12 prompts x 2 seeds x (1 + 4 x 3) = 312, as in the Thalia run.
        assert_eq!(eval_image_count(&s, 23, 12), 312);
        s.validate().unwrap();
    }

    #[test]
    fn checkpoints_end_on_the_last_step() {
        let s = TrainSettings {
            epochs: 7,
            save_points: 3,
            ..Default::default()
        };
        // 10 images x 7 = 70 steps, interval 23: saves at 23, 46, 69, then 70.
        assert_eq!(s.checkpoint_steps(10), vec![23, 46, 69, 70]);
    }

    #[test]
    fn rendered_configs_are_valid_toml() {
        let s = TrainSettings {
            output_name: "thalia_run01".into(),
            ..Default::default()
        };
        let train: toml::Table = render_train_toml(&s, 23, &paths()).parse().unwrap();
        assert_eq!(train["max_train_steps"].as_integer(), Some(920));
        assert_eq!(train["lr_warmup_steps"].as_integer(), Some(46));
        assert_eq!(train["save_every_n_steps"].as_integer(), Some(230));
        assert_eq!(train["network_dim"].as_integer(), Some(16));
        assert_eq!(train["learning_rate"].as_float(), Some(1e-4));
        assert_eq!(train["output_name"].as_str(), Some("thalia_run01"));
        let ds: toml::Table = render_dataset_toml(&s, "/w/data").parse().unwrap();
        let subset = &ds["datasets"][0]["subsets"][0];
        assert_eq!(subset["image_dir"].as_str(), Some("/w/data"));
        assert_eq!(ds["general"]["keep_tokens"].as_integer(), Some(1));
    }

    #[test]
    fn paths_with_quotes_stay_one_string() {
        let mut p = paths();
        p.model = "/w/a \"b\"\\c.safetensors".into();
        let train: toml::Table = render_train_toml(&TrainSettings::default(), 23, &p)
            .parse()
            .unwrap();
        assert_eq!(
            train["pretrained_model_name_or_path"].as_str(),
            Some("/w/a \"b\"\\c.safetensors")
        );
    }

    #[test]
    fn validation_rejects_nonsense() {
        let d = TrainSettings::default;
        for bad in [
            TrainSettings {
                output_name: "a b".into(),
                ..d()
            },
            TrainSettings {
                output_name: "../x".into(),
                ..d()
            },
            TrainSettings { rank: 0, ..d() },
            TrainSettings { alpha: 32, ..d() },
            TrainSettings {
                learning_rate: 0.5,
                ..d()
            },
            TrainSettings { epochs: 0, ..d() },
            TrainSettings {
                resolution: 1000,
                ..d()
            },
            TrainSettings {
                save_points: 0,
                ..d()
            },
            TrainSettings {
                eval_strengths: vec![],
                ..d()
            },
            TrainSettings {
                eval_strengths: vec![5.0],
                ..d()
            },
            TrainSettings {
                eval_seeds: vec![],
                ..d()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn eval_prompts_match_the_kohya_line_format() {
        let s = TrainSettings::default();
        let prompts = vec![EvalPrompt {
            id: "minimal".into(),
            prompt: "thalia_miletus, solo, portrait".into(),
        }];
        let text = render_eval_prompts(&thorough(), &prompts, EVAL_NEGATIVE).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        // Quick uses only the first seed.
        let quick = render_eval_prompts(&s, &prompts, EVAL_NEGATIVE).unwrap();
        assert_eq!(quick.lines().count(), 1);
        assert_eq!(
            lines[0],
            "thalia_miletus, solo, portrait --d 104729 --w 832 --h 1216 --s 20 --l 3.5 \
             --n worst quality, low quality, watermark, text, signature, extra limbs"
        );
    }

    #[test]
    fn eval_prompts_cannot_inject_options() {
        let s = TrainSettings::default();
        let evil = |t: &str| {
            vec![EvalPrompt {
                id: "x".into(),
                prompt: t.into(),
            }]
        };
        assert!(render_eval_prompts(&s, &evil("a --n b"), EVAL_NEGATIVE).is_err());
        assert!(render_eval_prompts(&s, &evil("a\nb"), EVAL_NEGATIVE).is_err());
        assert!(render_eval_prompts(&s, &[], EVAL_NEGATIVE).is_err());
        assert!(render_eval_prompts(&s, &evil("ok"), "x --w 9999").is_err());
    }
}
