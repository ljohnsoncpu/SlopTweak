//! Tauri commands for the Train tab. The pure parts (audit, plan) live here;
//! starting a run is wired up with the UI in Phase T3.

use std::path::PathBuf;

use serde::Serialize;

use super::dataset::{self, Report};
use super::settings::{eval_image_count, EvalPrompt, TrainSettings};

#[derive(Debug, Serialize)]
pub struct Defaults {
    pub settings: TrainSettings,
    pub prompts: Vec<EvalPrompt>,
}

/// What a dataset and settings add up to, before any GPU is rented.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub images: u32,
    pub total_steps: u32,
    pub warmup_steps: u32,
    pub checkpoint_steps: Vec<u32>,
    pub eval_images: usize,
}

#[derive(Debug, Serialize)]
pub struct AuditView {
    pub report: Report,
    /// `None` while the dataset has errors or the settings are invalid.
    pub plan: Option<Plan>,
    pub settings_error: Option<String>,
}

pub fn plan(settings: &TrainSettings, images: u32, prompts: usize) -> Plan {
    Plan {
        images,
        total_steps: settings.total_steps(images),
        warmup_steps: settings.warmup_steps(images),
        checkpoint_steps: settings.checkpoint_steps(images),
        eval_images: eval_image_count(settings, images, prompts),
    }
}

pub fn audit_view(
    dir: &std::path::Path,
    settings: TrainSettings,
    prompts: usize,
) -> Result<AuditView, String> {
    let settings_error = settings.validate().err();
    let res = if settings_error.is_none() {
        settings.resolution
    } else {
        TrainSettings::default().resolution
    };
    let report = dataset::audit(dir, res)?;
    let usable = settings_error.is_none() && !report.has_errors() && !report.images.is_empty();
    let plan = usable.then(|| plan(&settings, report.images.len() as u32, prompts));
    Ok(AuditView {
        report,
        plan,
        settings_error,
    })
}

/// The built-in evaluation prompts: a neutral set that works for any trigger
/// word (`{trigger}` is replaced by the dataset's first tag).
pub const DEFAULT_PROMPT_TEMPLATES: &[(&str, &str)] = &[
    ("minimal", "{trigger}, solo, portrait, neutral expression"),
    (
        "front",
        "{trigger}, solo, front view, portrait, orange hoodie",
    ),
    (
        "full_body",
        "{trigger}, solo, full body, standing, city park",
    ),
    (
        "side",
        "{trigger}, solo, side view, full body, navy tracksuit, train station",
    ),
    (
        "rear",
        "{trigger}, solo, rear view, full body, jeans, hiking trail",
    ),
    (
        "angry",
        "{trigger}, solo, angry, portrait, purple sweater, kitchen",
    ),
    (
        "sad",
        "{trigger}, solo, sad, seated on a park bench, denim jacket",
    ),
    (
        "unseen_outfit",
        "{trigger}, solo, full body, green raincoat, boots, rainy street",
    ),
];

/// The prompts a run uses: the built-in set, cut to what the preset needs.
pub fn preset_prompts(settings: &TrainSettings, trigger: &str) -> Vec<EvalPrompt> {
    let mut prompts = default_prompts(trigger);
    prompts.truncate(settings.eval_prompt_limit());
    prompts
}

pub fn default_prompts(trigger: &str) -> Vec<EvalPrompt> {
    DEFAULT_PROMPT_TEMPLATES
        .iter()
        .map(|(id, t)| EvalPrompt {
            id: (*id).into(),
            prompt: t.replace("{trigger}", trigger),
        })
        .collect()
}

#[tauri::command]
pub async fn train_defaults() -> Defaults {
    Defaults {
        settings: TrainSettings::default(),
        prompts: default_prompts("trigger_word"),
    }
}

#[tauri::command]
pub async fn train_audit(dir: String, settings: TrainSettings) -> Result<AuditView, String> {
    let path = PathBuf::from(dir);
    tokio::task::spawn_blocking(move || {
        let prompts = DEFAULT_PROMPT_TEMPLATES
            .len()
            .min(settings.eval_prompt_limit());
        audit_view(&path, settings, prompts)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(salt: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(1033u32.to_be_bytes());
        b.extend(1522u32.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, salt]);
        b
    }

    fn folder(n: u8) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for i in 0..n {
            std::fs::write(d.path().join(format!("i{i}.png")), png(i)).unwrap();
            std::fs::write(d.path().join(format!("i{i}.txt")), "trig, solo").unwrap();
        }
        d
    }

    #[test]
    fn a_good_dataset_gets_a_plan() {
        let d = folder(23);
        let thorough = TrainSettings {
            eval_preset: super::super::settings::EvalPreset::Thorough,
            ..Default::default()
        };
        let v = audit_view(d.path(), thorough, 12).unwrap();
        let p = v.plan.unwrap();
        assert_eq!(p.images, 23);
        assert_eq!(p.total_steps, 920);
        assert_eq!(p.warmup_steps, 46);
        assert_eq!(p.checkpoint_steps, vec![230, 460, 690, 920]);
        assert_eq!(p.eval_images, 312);
        // The default Quick preset: 6 prompts, one seed, base + final.
        let v = audit_view(d.path(), TrainSettings::default(), 6).unwrap();
        assert_eq!(v.plan.unwrap().eval_images, 12);
        assert!(v.settings_error.is_none());
    }

    #[test]
    fn problems_withhold_the_plan() {
        let d = folder(10);
        std::fs::remove_file(d.path().join("i3.txt")).unwrap();
        let v = audit_view(d.path(), TrainSettings::default(), 12).unwrap();
        assert!(v.report.has_errors() && v.plan.is_none());

        let d = folder(10);
        let bad = TrainSettings {
            rank: 0,
            ..Default::default()
        };
        let v = audit_view(d.path(), bad, 12).unwrap();
        assert!(v.settings_error.is_some() && v.plan.is_none());
        // The dataset is still audited so the user sees both problems at once.
        assert_eq!(v.report.images.len(), 10);
    }

    #[test]
    fn default_prompts_use_the_trigger_and_are_safe() {
        let prompts = default_prompts("thalia_miletus");
        assert!(prompts
            .iter()
            .all(|p| p.prompt.starts_with("thalia_miletus, ")));
        let s = TrainSettings::default();
        let text = super::super::settings::render_eval_prompts(
            &s,
            &prompts,
            super::super::settings::EVAL_NEGATIVE,
        )
        .unwrap();
        assert_eq!(
            text.lines().count(),
            prompts.len() * s.eval_seed_list().len()
        );
        assert_eq!(preset_prompts(&s, "x").len(), 6);
        let t = TrainSettings {
            eval_preset: super::super::settings::EvalPreset::Thorough,
            ..Default::default()
        };
        assert_eq!(
            preset_prompts(&t, "x").len(),
            DEFAULT_PROMPT_TEMPLATES.len()
        );
    }
}
