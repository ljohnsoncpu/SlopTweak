//! Everything uploaded to a training instance: `job.json` (settings, hashes
//! and the rendered kohya configs) and `dataset.tar`, plus the launch spec.
//! The instance side is instance/train_runner.py; the two share the paths and
//! the `job.json` shape defined here.

use std::path::Path;

use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::dataset::Report;
use super::moss::{
    render_data_config, render_jsonl, render_train_config, CaptionMode, MixRow, MossImage,
    MossSettings, MAX_IMAGES, MOSS_FILES, MOSS_RUN_CONFIG, MOSS_TRAINER_COMMIT,
    MOSS_TRAINER_TREE_SHA256,
};
use super::settings::{
    eval_image_count, render_dataset_toml, render_eval_prompts, render_train_toml, EvalPrompt,
    TrainPaths, TrainSettings, EVAL_NEGATIVE,
};
use crate::provider::LaunchSpec;

/// Where the runner works on the instance. Must match train_runner.py's
/// `TRAIN_WORK` default and provision.sh.
pub const WORK: &str = "/opt/sloptweak/train";
pub const MODELS: &str = "/opt/sloptweak/models";

/// kohya sd-scripts, pinned by commit. The tree digest is over file contents
/// (GitHub's archive bytes are not stable); computed 2026-10-26 from
/// codeload.github.com at this commit.
pub const TRAINER_COMMIT: &str = "11d8cedd0c67b93a10f8b2d75f43554f6f12c794";
pub const TRAINER_TREE_SHA256: &str =
    "82c63720f598af4c8733e936e2a8945963ff739850906a352f57d2be5cece6b0";

/// `pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime`, by digest (the environment
/// the validated run used; the trainer's own venv is built on top at boot).
pub const TRAIN_IMAGE: &str =
    "pytorch/pytorch@sha256:77f17f843507062875ce8be2a6f76aa6aa3df7f9ef1e31d9d7432f4b0f563dee";
/// Image pull plus the locked environment: about 20 GB at most on disk, plus
/// the base model, caches, checkpoints and training state.
pub const TRAIN_DISK_GB: u32 = 80;
/// Rough bytes pulled onto the instance outside the model: the image, wheels,
/// the trainer and tokenizers.
pub const OVERHEAD_DOWNLOAD_GB: f64 = 14.0;

const MAX_DATASET_TAR_BYTES: usize = 480 * 1024 * 1024;

/// The base checkpoint: downloaded on the instance by provision.sh with the
/// user's own key and verified there before training starts.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseModel {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub url: String,
    pub requires_civitai_token: bool,
}

pub fn remote_paths(model: &BaseModel) -> TrainPaths {
    TrainPaths {
        model: format!("{MODELS}/{}", model.file),
        image_dir: format!("{WORK}/data"),
        dataset_config: format!("{WORK}/runtime/dataset.toml"),
        output_dir: format!("{WORK}/runtime/checkpoints"),
        logging_dir: format!("{WORK}/runtime/tb"),
        tokenizer_cache_dir: format!("{WORK}/tokenizers"),
    }
}

pub struct JobBundle {
    pub job_json: Vec<u8>,
    pub job_sha256: String,
    pub dataset_tar: Vec<u8>,
    pub dataset_sha256: String,
    pub total_steps: u32,
    pub expected_eval_images: usize,
}

fn hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Build the upload for an audited dataset. Every file is re-read and checked
/// against the audit, so a file edited since the audit is caught.
pub fn build(
    dir: &Path,
    settings: &TrainSettings,
    report: &Report,
    model: &BaseModel,
    prompts: &[EvalPrompt],
) -> Result<JobBundle, String> {
    settings.validate()?;
    if report.has_errors() || report.images.is_empty() {
        return Err("Fix the dataset problems before training.".into());
    }
    let images = u32::try_from(report.images.len()).map_err(|_| "Too many images.")?;
    let paths = remote_paths(model);
    let eval_prompts = render_eval_prompts(settings, prompts, EVAL_NEGATIVE)?;

    let mut tar = tar::Builder::new(Vec::new());
    let mut files = Vec::new();
    for img in &report.images {
        for (name, expected) in [
            (img.file.clone(), &img.sha256),
            (format!("{}.txt", img.id), &img.caption_sha256),
        ] {
            let data = std::fs::read(dir.join(&name)).map_err(|e| format!("{name}: {e}"))?;
            if &hex(&data) != expected {
                return Err(format!("{name} changed since the dataset was checked."));
            }
            let mut header = tar::Header::new_ustar();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, &name, data.as_slice())
                .map_err(|e| format!("{name}: {e}"))?;
            files.push(json!({ "file": name, "sha256": expected }));
        }
    }
    let dataset_tar = tar.into_inner().map_err(|e| e.to_string())?;
    if dataset_tar.len() > MAX_DATASET_TAR_BYTES {
        return Err("The dataset is larger than 480 MB.".into());
    }

    let job = json!({
        "version": 1,
        "output_name": settings.output_name,
        "model_file": model.file,
        "model_sha256": model.sha256.to_ascii_lowercase(),
        "trainer_commit": TRAINER_COMMIT,
        "trainer_tree_sha256": TRAINER_TREE_SHA256,
        "files": files,
        "total_steps": settings.total_steps(images),
        "checkpoint_steps": settings.checkpoint_steps(images),
        "eval_checkpoints": settings.eval_checkpoints(images),
        "eval_strengths": settings.eval_strength_list(),
        "eval_images_per_run": prompts.len() * settings.eval_seed_list().len(),
        "train_toml": render_train_toml(settings, images, &paths),
        "dataset_toml": render_dataset_toml(settings, &paths.image_dir),
        "eval_prompts": eval_prompts,
    });
    let job_json = serde_json::to_vec(&job).map_err(|e| e.to_string())?;
    Ok(JobBundle {
        job_sha256: hex(&job_json),
        dataset_sha256: hex(&dataset_tar),
        total_steps: settings.total_steps(images),
        expected_eval_images: eval_image_count(settings, images, prompts.len()),
        job_json,
        dataset_tar,
    })
}

/// Seconds the instance may live: the run's cap. The instance's own watchdog
/// destroys it then, even if the app is gone.
pub fn max_session_minutes(max_seconds: u64) -> u64 {
    max_seconds.div_ceil(60)
}

/// The instance contract for `BACKEND=train` (provision.sh, sidecar.py).
pub fn launch_spec(
    label: &str,
    launch_token_hash: &str,
    model: &BaseModel,
    max_seconds: u64,
    civitai_token: Option<&str>,
) -> Result<LaunchSpec, String> {
    if model.requires_civitai_token && civitai_token.is_none() {
        return Err("This base model needs your CivitAI key.".into());
    }
    let models = json!([{
        "url": model.url,
        "sha256": model.sha256.to_ascii_lowercase(),
        "size_bytes": model.bytes,
        "filename": model.file,
        "requires_civitai_token": model.requires_civitai_token,
    }]);
    let models_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&models).map_err(|e| e.to_string())?);
    let mut env = vec![
        ("BACKEND".to_string(), "train".to_string()),
        (
            "LAUNCH_TOKEN_HASH".to_string(),
            launch_token_hash.to_string(),
        ),
        ("MODELS_B64".to_string(), models_b64),
        ("HEARTBEAT_MINUTES".to_string(), "10".to_string()),
        ("IDLE_MINUTES".to_string(), "30".to_string()),
        (
            "MAX_SESSION_MINUTES".to_string(),
            max_session_minutes(max_seconds).to_string(),
        ),
        (
            "SLOPTWEAK_ASSETS_URL".to_string(),
            crate::config::assets_url(),
        ),
        (
            "SLOPTWEAK_ASSETS_SHA256".to_string(),
            crate::config::ASSETS_SHA256.to_string(),
        ),
    ];
    if let (true, Some(t)) = (model.requires_civitai_token, civitai_token) {
        env.push(("CIVITAI_TOKEN".to_string(), t.to_string()));
    }
    Ok(LaunchSpec {
        image: TRAIN_IMAGE.to_string(),
        disk_gb: TRAIN_DISK_GB,
        label: label.to_string(),
        env,
        onstart: crate::config::onstart(),
    })
}

/// Download estimate for the spending cap: model, image and environment.
pub fn download_gb(model: &BaseModel) -> f64 {
    model.bytes as f64 / 1e9 + OVERHEAD_DOWNLOAD_GB
}

// ----- Kromoss (KohakuMossTrainer) -------------------------------------------

/// `pytorch/pytorch:2.8.0-cuda12.8-cudnn9-devel` by digest: the image the
/// 2026-10-27 Kromoss cloud runs used (tag found in the registry that day). The
/// trainer's venv, with torch from the cu126 index, is built on top at boot.
pub const MOSS_IMAGE: &str =
    "pytorch/pytorch@sha256:a7103283ea7113e10ae5d014bd2342acebda0bc53164b2f7b1dd6eb7a766bdb6";
/// Image, model files (15.5 GB), caches, four resume states and exports.
pub const MOSS_DISK_GB: u32 = 100;

/// Bytes pulled onto the instance: the image and wheels plus the model files.
pub fn moss_download_gb() -> f64 {
    let models: u64 = MOSS_FILES.iter().map(|f| f.bytes).sum();
    models as f64 / 1e9 + OVERHEAD_DOWNLOAD_GB
}

/// Build the upload for a Kromoss run. Every file is re-read and checked
/// against `images`, so one edited since `moss::collect` is caught.
///
/// The tar is flat: each image and its natural-language caption as `name.txt`
/// (the trainer's folder layout, which the proven runs used). Tags travel in
/// `train.jsonl`, not as files.
pub fn build_moss(
    dir: &Path,
    settings: &MossSettings,
    images: &[MossImage],
) -> Result<JobBundle, String> {
    settings.validate()?;
    if images.is_empty() || images.len() > MAX_IMAGES {
        return Err(format!("Use between 1 and {MAX_IMAGES} images."));
    }
    let mix = settings.caption_mode == CaptionMode::Mix;

    let mut tar = tar::Builder::new(Vec::new());
    let mut files = Vec::new();
    let mut rows = Vec::new();
    let mut add = |name: &str, data: &[u8]| -> Result<(), String> {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, name, data)
            .map_err(|e| format!("{name}: {e}"))?;
        files.push(json!({ "file": name, "sha256": hex(data) }));
        Ok(())
    };
    for img in images {
        let read = |name: &str, expected: &str| -> Result<Vec<u8>, String> {
            let data = std::fs::read(dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
            if hex(&data) != expected {
                return Err(format!("{name} changed since the dataset was checked."));
            }
            Ok(data)
        };
        add(&img.file, &read(&img.file, &img.sha256)?)?;
        let stem = img
            .file
            .rsplit_once('.')
            .map_or(img.file.as_str(), |(s, _)| s);
        add(&format!("{stem}.txt"), &read(&img.nl_file, &img.nl_sha256)?)?;
        if mix {
            let (Some(f), Some(sha)) = (&img.tags_file, &img.tags_sha256) else {
                return Err(format!("{}: no tag caption for mixed captions.", img.file));
            };
            read(f, sha)?;
            rows.push(MixRow {
                image: img.file.clone(),
                nl: img.nl.clone(),
                tags: img.tags.clone(),
            });
        }
    }
    let dataset_tar = tar.into_inner().map_err(|e| e.to_string())?;
    if dataset_tar.len() > MAX_DATASET_TAR_BYTES {
        return Err("The dataset is larger than 480 MB.".into());
    }
    let train_jsonl = if mix {
        render_jsonl(&rows)?
    } else {
        String::new()
    };

    let models: Vec<_> = MOSS_FILES
        .iter()
        .map(|f| json!({ "file": f.file_name(), "dest": f.path, "sha256": f.sha256 }))
        .collect();
    let job = json!({
        "version": 1,
        "trainer": "kohakumoss",
        "output_name": settings.output_name,
        "trainer_commit": MOSS_TRAINER_COMMIT,
        "trainer_tree_sha256": MOSS_TRAINER_TREE_SHA256,
        "models": models,
        "files": files,
        "total_steps": settings.steps,
        "checkpoint_steps": settings.checkpoint_steps(),
        "run_config": render_train_config(settings),
        "data_config": render_data_config(MOSS_RUN_CONFIG),
        "train_jsonl": train_jsonl,
    });
    let job_json = serde_json::to_vec(&job).map_err(|e| e.to_string())?;
    Ok(JobBundle {
        job_sha256: hex(&job_json),
        dataset_sha256: hex(&dataset_tar),
        total_steps: settings.steps,
        expected_eval_images: 0,
        job_json,
        dataset_tar,
    })
}

/// The instance contract for `BACKEND=train` with the Kromoss files as the
/// models to download (provision.sh verifies size and SHA-256).
pub fn moss_launch_spec(
    label: &str,
    launch_token_hash: &str,
    max_seconds: u64,
) -> Result<LaunchSpec, String> {
    let models: Vec<_> = MOSS_FILES
        .iter()
        .map(|f| {
            json!({
                "url": f.url,
                "sha256": f.sha256,
                "size_bytes": f.bytes,
                "filename": f.file_name(),
                "requires_civitai_token": false,
            })
        })
        .collect();
    let models_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&models).map_err(|e| e.to_string())?);
    let env = vec![
        ("BACKEND".to_string(), "train".to_string()),
        (
            "LAUNCH_TOKEN_HASH".to_string(),
            launch_token_hash.to_string(),
        ),
        ("MODELS_B64".to_string(), models_b64),
        ("HEARTBEAT_MINUTES".to_string(), "10".to_string()),
        ("IDLE_MINUTES".to_string(), "30".to_string()),
        (
            "MAX_SESSION_MINUTES".to_string(),
            max_session_minutes(max_seconds).to_string(),
        ),
        (
            "SLOPTWEAK_ASSETS_URL".to_string(),
            crate::config::assets_url(),
        ),
        (
            "SLOPTWEAK_ASSETS_SHA256".to_string(),
            crate::config::ASSETS_SHA256.to_string(),
        ),
    ];
    Ok(LaunchSpec {
        image: MOSS_IMAGE.to_string(),
        disk_gb: MOSS_DISK_GB,
        label: label.to_string(),
        env,
        onstart: crate::config::onstart(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::train::dataset::audit;

    fn model() -> BaseModel {
        BaseModel {
            file: "base.safetensors".into(),
            sha256: "AB".repeat(32),
            bytes: 6_938_043_446,
            url: "https://civitai.com/api/download/models/1".into(),
            requires_civitai_token: true,
        }
    }

    fn png(w: u32, h: u32, salt: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, salt]);
        b
    }

    fn dataset(n: u8) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for i in 0..n {
            std::fs::write(d.path().join(format!("i{i}.png")), png(1033, 1522, i)).unwrap();
            std::fs::write(d.path().join(format!("i{i}.txt")), "trig, solo, front").unwrap();
        }
        d
    }

    fn prompts() -> Vec<EvalPrompt> {
        vec![
            EvalPrompt {
                id: "a".into(),
                prompt: "trig, portrait".into(),
            },
            EvalPrompt {
                id: "b".into(),
                prompt: "trig, full body".into(),
            },
        ]
    }

    #[test]
    fn builds_a_job_the_runner_will_accept() {
        let d = dataset(10);
        let s = TrainSettings {
            output_name: "lora".into(),
            ..Default::default()
        };
        let report = audit(d.path(), 1024).unwrap();
        let b = build(d.path(), &s, &report, &model(), &prompts()).unwrap();
        let job: serde_json::Value = serde_json::from_slice(&b.job_json).unwrap();

        // 10 images x 40 epochs, saves every 100 steps.
        assert_eq!(job["total_steps"], 400);
        assert_eq!(job["checkpoint_steps"], json!([100, 200, 300, 400]));
        // Quick preset: 2 prompts x 1 seed, the base and the final checkpoint at 1.0.
        assert_eq!(job["eval_images_per_run"], 2);
        assert_eq!(job["eval_checkpoints"], json!([400]));
        assert_eq!(job["eval_strengths"], json!([1.0]));
        assert_eq!(b.expected_eval_images, 2 * (1 + 1));
        assert_eq!(job["model_sha256"], "ab".repeat(32));
        assert_eq!(job["model_file"], "base.safetensors");
        assert_eq!(job["trainer_commit"], TRAINER_COMMIT);
        assert_eq!(job["files"].as_array().unwrap().len(), 20);
        let train: toml::Table = job["train_toml"].as_str().unwrap().parse().unwrap();
        assert_eq!(train["max_train_steps"].as_integer(), Some(400));
        assert_eq!(
            train["pretrained_model_name_or_path"].as_str(),
            Some("/opt/sloptweak/models/base.safetensors")
        );
        assert_eq!(
            train["output_dir"].as_str(),
            Some("/opt/sloptweak/train/runtime/checkpoints")
        );
        let ds: toml::Table = job["dataset_toml"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            ds["datasets"][0]["subsets"][0]["image_dir"].as_str(),
            Some("/opt/sloptweak/train/data")
        );
        assert_eq!(job["eval_prompts"].as_str().unwrap().lines().count(), 2);
        assert_eq!(b.job_sha256, hex(&b.job_json));
        assert_eq!(b.dataset_sha256, hex(&b.dataset_tar));
    }

    #[test]
    fn the_tar_is_flat_and_matches_the_job() {
        let d = dataset(3);
        let report = audit(d.path(), 1024).unwrap();
        let b = build(
            d.path(),
            &TrainSettings::default(),
            &report,
            &model(),
            &prompts(),
        )
        .unwrap();
        let job: serde_json::Value = serde_json::from_slice(&b.job_json).unwrap();
        let mut listed: Vec<(String, String)> = job["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["file"].as_str().unwrap().to_owned(),
                    f["sha256"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let mut archive = tar::Archive::new(b.dataset_tar.as_slice());
        let mut found = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut e = entry.unwrap();
            assert!(e.header().entry_type().is_file());
            let name = e.path().unwrap().to_string_lossy().into_owned();
            assert!(!name.contains('/') && !name.contains(".."), "{name}");
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap();
            found.push((name, hex(&data)));
        }
        listed.sort();
        found.sort();
        assert_eq!(listed, found);
    }

    #[test]
    fn a_file_edited_after_the_audit_is_refused() {
        let d = dataset(3);
        let report = audit(d.path(), 1024).unwrap();
        std::fs::write(d.path().join("i1.txt"), "trig, changed").unwrap();
        let err = build(
            d.path(),
            &TrainSettings::default(),
            &report,
            &model(),
            &prompts(),
        )
        .err()
        .unwrap();
        assert!(err.contains("i1.txt changed"), "{err}");
    }

    #[test]
    fn refuses_bad_datasets_settings_and_prompts() {
        let d = tempfile::tempdir().unwrap();
        let report = audit(d.path(), 1024).unwrap();
        let s = TrainSettings::default();
        assert!(build(d.path(), &s, &report, &model(), &prompts()).is_err());

        let d = dataset(3);
        let report = audit(d.path(), 1024).unwrap();
        let bad = TrainSettings {
            rank: 0,
            ..Default::default()
        };
        assert!(build(d.path(), &bad, &report, &model(), &prompts()).is_err());
        assert!(build(d.path(), &s, &report, &model(), &[]).is_err());
    }

    #[test]
    fn launch_spec_uses_the_train_backend_and_the_pinned_image() {
        let spec = launch_spec(
            "sloptweak-abc",
            &"f".repeat(64),
            &model(),
            5400,
            Some("tok"),
        )
        .unwrap();
        assert_eq!(spec.image, TRAIN_IMAGE);
        assert_eq!(spec.label, "sloptweak-abc");
        let env = |k: &str| {
            spec.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(env("BACKEND"), Some("train"));
        assert_eq!(env("MAX_SESSION_MINUTES"), Some("90"));
        assert_eq!(env("CIVITAI_TOKEN"), Some("tok"));
        let models = base64::engine::general_purpose::STANDARD
            .decode(env("MODELS_B64").unwrap())
            .unwrap();
        let models: serde_json::Value = serde_json::from_slice(&models).unwrap();
        assert_eq!(models[0]["filename"], "base.safetensors");
        assert_eq!(models[0]["sha256"], "ab".repeat(32));
        assert_eq!(models[0]["requires_civitai_token"], true);
        assert!(!format!("{spec:?}").contains("tok\""));
        assert!(spec.onstart.len() < 4048);
    }

    #[test]
    fn a_keyed_model_without_a_key_is_refused() {
        assert!(launch_spec("sloptweak-abc", "h", &model(), 600, None).is_err());
        let open = BaseModel {
            requires_civitai_token: false,
            ..model()
        };
        let spec = launch_spec("sloptweak-abc", "h", &open, 600, None).unwrap();
        assert!(!spec.env.iter().any(|(k, _)| k == "CIVITAI_TOKEN"));
        // Never leak a key the model doesn't need.
        let spec = launch_spec("sloptweak-abc", "h", &open, 600, Some("tok")).unwrap();
        assert!(!spec.env.iter().any(|(k, _)| k == "CIVITAI_TOKEN"));
    }

    #[test]
    fn session_minutes_round_up() {
        assert_eq!(max_session_minutes(1), 1);
        assert_eq!(max_session_minutes(60), 1);
        assert_eq!(max_session_minutes(61), 2);
    }

    #[test]
    fn download_estimate_includes_overhead() {
        let g = download_gb(&model());
        assert!((g - (6.938043446 + OVERHEAD_DOWNLOAD_GB)).abs() < 1e-9);
    }

    fn moss_folder(mix: bool, n: u8) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for i in 0..n {
            std::fs::write(d.path().join(format!("i{i}.png")), png(1024, 1536, i)).unwrap();
            std::fs::write(
                d.path().join(format!("i{i}.nl.txt")),
                format!("roka, scene {i}."),
            )
            .unwrap();
            if mix {
                std::fs::write(d.path().join(format!("i{i}.e621.txt")), "roka, solo, smile")
                    .unwrap();
            }
        }
        d
    }

    fn tar_names(tar: &[u8]) -> Vec<String> {
        let mut names: Vec<String> = tar::Archive::new(tar)
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn moss_job_matches_what_the_runner_reads() {
        use crate::train::moss::collect;
        let d = moss_folder(true, 3);
        let s = MossSettings {
            output_name: "roka".into(),
            ..Default::default()
        };
        let images = collect(d.path(), s.caption_mode).unwrap();
        let b = build_moss(d.path(), &s, &images).unwrap();
        let job: serde_json::Value = serde_json::from_slice(&b.job_json).unwrap();

        assert_eq!(job["trainer"], "kohakumoss");
        assert_eq!(job["output_name"], "roka");
        assert_eq!(job["trainer_commit"], MOSS_TRAINER_COMMIT);
        assert_eq!(job["trainer_tree_sha256"], MOSS_TRAINER_TREE_SHA256);
        assert_eq!(job["total_steps"], 800);
        assert_eq!(job["checkpoint_steps"], json!([200, 400, 600, 800]));
        assert_eq!(b.total_steps, 800);
        assert_eq!(b.expected_eval_images, 0);
        // Three model files, flat names plus their place in the trainer's folder.
        let models = job["models"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0]["file"], "kromoss_int8_convrot.safetensors");
        assert_eq!(
            models[0]["dest"],
            "diffusion_models/kromoss_int8_convrot.safetensors"
        );
        // The tar has each image and its caption as name.txt; tags are not files.
        assert_eq!(
            tar_names(&b.dataset_tar),
            ["i0.png", "i0.txt", "i1.png", "i1.txt", "i2.png", "i2.txt"]
        );
        assert_eq!(job["files"].as_array().unwrap().len(), 6);
        // Mixed captions: one row per image with both captions.
        let rows: Vec<serde_json::Value> = job["train_jsonl"]
            .as_str()
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["image"], "i1.png");
        assert_eq!(rows[1]["caption"], "roka, scene 1.");
        assert_eq!(rows[1]["tags"], json!(["roka", "solo", "smile"]));
        // The configs point where the runner lays things out.
        let run = job["run_config"].as_str().unwrap();
        assert!(run.contains("WEIGHTS = \"/opt/sloptweak/train/models\""));
        assert!(run.contains("\"root\": \"/opt/sloptweak/train/data/nl\""));
        assert!(job["data_config"]
            .as_str()
            .unwrap()
            .contains("use_config(\"sloptweak-train.py\")"));
        assert_eq!(b.job_sha256, hex(&b.job_json));
        assert_eq!(b.dataset_sha256, hex(&b.dataset_tar));
    }

    #[test]
    fn moss_nl_job_has_no_jsonl_and_reads_the_folder() {
        use crate::train::moss::collect;
        let d = moss_folder(false, 2);
        let s = MossSettings {
            caption_mode: CaptionMode::Nl,
            ..Default::default()
        };
        let images = collect(d.path(), s.caption_mode).unwrap();
        let b = build_moss(d.path(), &s, &images).unwrap();
        let job: serde_json::Value = serde_json::from_slice(&b.job_json).unwrap();
        assert_eq!(job["train_jsonl"], "");
        assert!(job["run_config"]
            .as_str()
            .unwrap()
            .contains("\"source\": \"folder\""));
        assert_eq!(tar_names(&b.dataset_tar).len(), 4);
    }

    #[test]
    fn moss_refuses_edited_files_and_bad_input() {
        use crate::train::moss::collect;
        let d = moss_folder(true, 2);
        let s = MossSettings::default();
        let images = collect(d.path(), s.caption_mode).unwrap();
        for (file, what) in [
            ("i1.png", "i1.png"),
            ("i1.nl.txt", "i1.nl.txt"),
            ("i1.e621.txt", "i1.e621.txt"),
        ] {
            let d2 = moss_folder(true, 2);
            let imgs = collect(d2.path(), s.caption_mode).unwrap();
            std::fs::write(d2.path().join(file), "edited").unwrap();
            let err = build_moss(d2.path(), &s, &imgs).err().unwrap();
            assert!(err.contains(&format!("{what} changed")), "{err}");
        }
        assert!(build_moss(d.path(), &s, &[]).is_err());
        let bad = MossSettings {
            rank: 0,
            ..Default::default()
        };
        assert!(build_moss(d.path(), &bad, &images).is_err());
    }

    #[test]
    fn moss_launch_spec_downloads_the_three_pinned_files() {
        let spec = moss_launch_spec("sloptweak-abc", &"f".repeat(64), 7200).unwrap();
        assert_eq!(spec.image, MOSS_IMAGE);
        assert!(MOSS_IMAGE.contains("@sha256:"));
        assert_eq!(spec.disk_gb, MOSS_DISK_GB);
        let env = |k: &str| {
            spec.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(env("BACKEND"), Some("train"));
        assert_eq!(env("MAX_SESSION_MINUTES"), Some("120"));
        assert_eq!(env("CIVITAI_TOKEN"), None);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(env("MODELS_B64").unwrap())
            .unwrap();
        let models: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let names: Vec<&str> = models
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["filename"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "kromoss_int8_convrot.safetensors",
                "qwen3.5_9b_int8_convrot.safetensors",
                "flux2-vae.safetensors"
            ]
        );
        assert_eq!(models[0]["size_bytes"], 5_413_698_958_u64);
        assert_eq!(models[0]["requires_civitai_token"], false);
        assert!(spec.onstart.len() < 4048);
    }

    #[test]
    fn moss_download_estimate_covers_the_models() {
        let g = moss_download_gb();
        assert!(
            g > 15.0 + OVERHEAD_DOWNLOAD_GB && g < 16.0 + OVERHEAD_DOWNLOAD_GB,
            "{g}"
        );
    }

    /// Writes a sample Kromoss job for the Python contract check; run with
    /// `--ignored` and `SLOPTWEAK_SAMPLE_MOSS_JOB=<path>`.
    #[test]
    #[ignore = "writes a file for the cross-language contract check"]
    fn write_sample_moss_job() {
        use crate::train::moss::collect;
        let d = moss_folder(true, 4);
        let s = MossSettings {
            output_name: "lora".into(),
            ..Default::default()
        };
        let images = collect(d.path(), s.caption_mode).unwrap();
        let b = build_moss(d.path(), &s, &images).unwrap();
        let out = std::env::var("SLOPTWEAK_SAMPLE_MOSS_JOB").expect("SLOPTWEAK_SAMPLE_MOSS_JOB");
        std::fs::write(out, &b.job_json).unwrap();
    }

    /// Writes a sample job for the Python contract check (see
    /// instance/tests/test_train_runner.py); run with `--ignored` and
    /// `SLOPTWEAK_SAMPLE_JOB=<path>`.
    #[test]
    #[ignore = "writes a file for the cross-language contract check"]
    fn write_sample_job() {
        let d = dataset(10);
        let report = audit(d.path(), 1024).unwrap();
        let s = TrainSettings {
            output_name: "lora".into(),
            ..Default::default()
        };
        let b = build(d.path(), &s, &report, &model(), &prompts()).unwrap();
        let out = std::env::var("SLOPTWEAK_SAMPLE_JOB").expect("SLOPTWEAK_SAMPLE_JOB");
        std::fs::write(out, &b.job_json).unwrap();
    }
}
