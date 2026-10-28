//! Finished Kromoss training runs on disk, so the MOSS panel can offer their
//! LoRAs without a file dialog.
//!
//! A run lands in `<results root>/<name>-<stamp>/results/` (see `manager.rs`).
//! Its checkpoints are `checkpoints/<name>-stepNNNNNNNN.safetensors`. A Kromoss
//! run is told apart from a kohya one by files only the Kromoss runner writes
//! (any of [`KROMOSS_MARKERS`]) and by having no `loss.csv`; a kohya LoRA is for
//! SDXL and means nothing to the MOSS panel.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

/// Result files only a Kromoss run has. The first real run came back without
/// `metrics.jsonl`, so the others count too.
const KROMOSS_MARKERS: [&str; 3] = ["metrics.jsonl", "kernel-cache.tgz", "logs/config-check.log"];

/// How many runs the panel lists (newest first).
pub const MAX_RUNS: usize = 5;

/// One checkpoint the panel can send to the GPU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentLora {
    /// The run's folder name; pass it back to [`resolve`].
    pub run: String,
    /// The checkpoint's file name; pass it back to [`resolve`].
    pub file: String,
    pub step: u32,
    pub bytes: u64,
    /// Seconds since the epoch of the run's metrics file (for ordering).
    pub modified: u64,
}

/// A plain name: no folder parts, nothing hidden.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `Some(step)` for `<name>-step00000400.safetensors`.
fn step_of(file: &str) -> Option<u32> {
    let stem = file.strip_suffix(".safetensors")?;
    let (name, digits) = stem.rsplit_once("-step")?;
    if name.is_empty() || digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// The checkpoints of the newest `max_runs` Kromoss runs under `root`: runs
/// newest first, and within a run the latest step first.
pub fn list(root: &Path, max_runs: usize) -> Vec<RecentLora> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut runs: Vec<(u64, String, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let results = e.path().join("results");
        if !plain(&name) || !e.path().is_dir() || results.join("loss.csv").exists() {
            continue; // not a run, or a kohya run (its LoRA is for SDXL)
        }
        if let Some(marker) = KROMOSS_MARKERS
            .iter()
            .map(|m| results.join(m))
            .find(|p| p.is_file())
        {
            runs.push((mtime(&marker), name, results));
        }
    }
    runs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut out = Vec::new();
    for (modified, run, results) in runs.into_iter().take(max_runs) {
        let Ok(files) = std::fs::read_dir(results.join("checkpoints")) else {
            continue;
        };
        let mut found: Vec<RecentLora> = files
            .flatten()
            .filter_map(|f| {
                let file = f.file_name().into_string().ok()?;
                let step = step_of(&file).filter(|_| plain(&file))?;
                let meta = f.metadata().ok().filter(|m| m.is_file())?;
                Some(RecentLora {
                    run: run.clone(),
                    file,
                    step,
                    bytes: meta.len(),
                    modified,
                })
            })
            .collect();
        found.sort_by_key(|l| std::cmp::Reverse(l.step));
        out.extend(found);
    }
    out
}

/// The path of a listed checkpoint. Only what [`list`] would show resolves, so
/// the window can't make the app read an arbitrary file by naming it.
pub fn resolve(root: &Path, run: &str, file: &str) -> Result<PathBuf, String> {
    list(root, usize::MAX)
        .into_iter()
        .find(|l| l.run == run && l.file == file)
        .map(|l| root.join(&l.run).join("results/checkpoints").join(&l.file))
        .ok_or_else(|| "That training result is no longer on disk.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(root: &Path, name: &str, kromoss: bool, steps: &[u32]) {
        let results = root.join(name).join("results");
        std::fs::create_dir_all(results.join("checkpoints")).unwrap();
        if kromoss {
            std::fs::write(results.join("metrics.jsonl"), "{}\n").unwrap();
        } else {
            std::fs::write(results.join("loss.csv"), "1,2\n").unwrap();
        }
        for s in steps {
            std::fs::write(
                results.join(format!("checkpoints/lora-step{s:08}.safetensors")),
                vec![0u8; *s as usize],
            )
            .unwrap();
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sloptweak-recent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_only_kromoss_runs_latest_step_first() {
        let root = tmp("list");
        run(&root, "roka-1", true, &[200, 400, 800]);
        run(&root, "sdxl-2", false, &[920]);
        let got = list(&root, MAX_RUNS);
        let steps: Vec<u32> = got.iter().map(|l| l.step).collect();
        assert_eq!(steps, [800, 400, 200]);
        assert!(got.iter().all(|l| l.run == "roka-1"));
        assert_eq!(got[0].file, "lora-step00000800.safetensors");
        assert_eq!(got[0].bytes, 800);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_without_metrics_still_counts_by_its_other_markers() {
        let root = tmp("markers");
        // As the first real run came back: checkpoints, logs, kernel tables, no metrics.
        run(&root, "thales-1", false, &[200, 400]);
        let results = root.join("thales-1/results");
        std::fs::remove_file(results.join("loss.csv")).unwrap();
        std::fs::create_dir_all(results.join("logs")).unwrap();
        std::fs::write(results.join("logs/config-check.log"), "ok\n").unwrap();
        // A kohya run has loss.csv even if it somehow has a marker too.
        run(&root, "sdxl-2", false, &[920]);
        std::fs::write(root.join("sdxl-2/results/kernel-cache.tgz"), b"x").unwrap();
        let got = list(&root, MAX_RUNS);
        assert_eq!(got.iter().map(|l| l.step).collect::<Vec<_>>(), [400, 200]);
        assert!(got.iter().all(|l| l.run == "thales-1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn newest_runs_first_and_capped() {
        let root = tmp("order");
        for (i, name) in ["a-1", "b-2", "c-3"].iter().enumerate() {
            run(&root, name, true, &[100]);
            // Older runs get older metrics files.
            let m = root.join(name).join("results/metrics.jsonl");
            let t = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000_000 + i as u64 * 100);
            std::fs::File::options()
                .write(true)
                .open(&m)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let runs: Vec<String> = list(&root, 2).into_iter().map(|l| l.run).collect();
        assert_eq!(runs, ["c-3", "b-2"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_odd_files() {
        let root = tmp("odd");
        run(&root, "roka-1", true, &[100]);
        let ck = root.join("roka-1/results/checkpoints");
        for name in [
            "lora-step00000100.safetensors.partial",
            "lora-step100.safetensors",
            "lora-stepabcdefgh.safetensors",
            "notes.txt",
            "-step00000100.safetensors",
            ".hidden-step00000100.safetensors",
        ] {
            std::fs::write(ck.join(name), b"x").unwrap();
        }
        std::fs::create_dir(ck.join("dir-step00000100.safetensors")).unwrap();
        let got = list(&root, MAX_RUNS);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].step, 100);
        assert!(list(&root.join("missing"), MAX_RUNS).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_listed_checkpoints_resolve() {
        let root = tmp("resolve");
        run(&root, "roka-1", true, &[100]);
        run(&root, "sdxl-2", false, &[920]);
        let ok = resolve(&root, "roka-1", "lora-step00000100.safetensors").unwrap();
        assert!(ok.is_file());
        for (r, f) in [
            ("roka-1", "lora-step00000999.safetensors"),
            ("sdxl-2", "lora-step00000920.safetensors"),
            ("..", "lora-step00000100.safetensors"),
            ("roka-1/../roka-1", "lora-step00000100.safetensors"),
            ("roka-1", "../metrics.jsonl"),
            ("roka-1", "..\\metrics.jsonl"),
            ("", ""),
        ] {
            assert!(resolve(&root, r, f).is_err(), "{r} {f}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
