//! SDXL LoRA training on this PC's own GPU: the same kohya sd-scripts, settings
//! and dataset audit as the cloud run, started as a child process instead of
//! on a rented instance. Nothing is rented or billed, so there is no quote.
//!
//! The user installs the pinned trainer once (a folder with a `.venv`) and
//! points the app at it and at an SDXL checkpoint file; see docs/findings.md
//! for the verified setup. The run writes into the usual results folder.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::dataset::{self, Report};
use super::manager::Status;
use super::settings::{
    render_dataset_toml, render_train_toml, TrainPaths, TrainSettings, EVAL_NEGATIVE,
};
use super::trainer_setup::{check_root_usable, ready_trainer_dir, Layout, ROOT_NAME};

/// Peak use measured on an RTX 4080 Laptop was 7.8 GB of the whole GPU; leave
/// room for the desktop and a little headroom.
pub const MIN_FREE_MIB: u64 = 9_000;
const SETUP_FILE: &str = "local-train.json";
const POLL: Duration = Duration::from_millis(500);
/// How much of the end of the log is read for progress and failure text.
const LOG_TAIL_BYTES: u64 = 16 * 1024;

// ----- setup -------------------------------------------------------------------

/// The trainer and base checkpoint a run uses.
///
/// The trainer is normally the one SlopTweak sets up itself (see
/// `trainer_setup`); `trainer_dir` is only filled in for a folder an earlier
/// version asked the user to point at, which is still used when SlopTweak has
/// no setup of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSetup {
    /// A kohya sd-scripts checkout with a `.venv` inside.
    pub trainer_dir: String,
    /// An SDXL-family `.safetensors` checkpoint.
    pub base_model: String,
    /// Where SlopTweak sets up its own trainer; empty = the default place.
    pub trainer_root: String,
}

impl LocalSetup {
    pub fn load(config_dir: &Path) -> Self {
        fs::read(config_dir.join(SETUP_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> Result<(), String> {
        fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
        let path = config_dir.join(SETUP_FILE);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&tmp, body).map_err(|e| format!("Couldn't save the local setup: {e}"))?;
        fs::rename(&tmp, &path).map_err(|e| format!("Couldn't save the local setup: {e}"))
    }

    /// Use SlopTweak's own trainer when its setup is finished.
    pub fn resolved(mut self, managed: &Layout) -> Self {
        if let Some(dir) = ready_trainer_dir(managed) {
            self.trainer_dir = dir.to_string_lossy().into_owned();
        }
        self
    }

    pub fn python(&self) -> PathBuf {
        let venv = Path::new(&self.trainer_dir).join(".venv");
        if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }

    pub fn script(&self) -> PathBuf {
        Path::new(&self.trainer_dir).join("sdxl_train_network.py")
    }

    /// What is missing, in plain words; empty means ready.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.script().is_file() || !self.python().is_file() {
            out.push("The trainer isn't set up yet. Press \"Set up trainer\" first.".to_string());
        }
        if self.base_model.trim().is_empty() {
            out.push("Choose the SDXL checkpoint file to train on.".to_string());
        } else if !Path::new(&self.base_model).is_file() {
            out.push(format!("The checkpoint {} doesn't exist.", self.base_model));
        }
        out
    }
}

/// Where SlopTweak sets up its trainer: the environment override
/// (`SLOPTWEAK_LOCAL_TRAINER_ROOT`, for development), else the folder the
/// user chose, else `default`.
pub fn trainer_root(config_dir: &Path, default: &Path, env: Option<PathBuf>) -> PathBuf {
    env.filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            let chosen = LocalSetup::load(config_dir).trainer_root;
            (!chosen.trim().is_empty()).then(|| PathBuf::from(chosen))
        })
        .unwrap_or_else(|| default.to_path_buf())
}

// ----- GPU ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuInfo {
    pub name: String,
    pub total_mib: u64,
    pub used_mib: u64,
}

/// One line of `nvidia-smi --query-gpu=name,memory.total,memory.used
/// --format=csv,noheader,nounits`.
pub fn parse_gpu_csv(text: &str) -> Option<GpuInfo> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let mut parts = line.rsplitn(3, ',');
    let used = parts.next()?.trim().parse().ok()?;
    let total = parts.next()?.trim().parse().ok()?;
    let name = parts.next()?.trim().to_string();
    Some(GpuInfo {
        name,
        total_mib: total,
        used_mib: used,
    })
}

/// The first NVIDIA GPU, or `None` when `nvidia-smi` isn't there.
pub fn query_gpu() -> Option<GpuInfo> {
    let mut cmd = Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,memory.total,memory.used",
        "--format=csv,noheader,nounits",
    ])
    .stdin(Stdio::null())
    .stderr(Stdio::null());
    hide_window(&mut cmd);
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    parse_gpu_csv(&String::from_utf8_lossy(&out.stdout))
}

/// Why this GPU can't train now, if it can't.
pub fn gpu_problem(gpu: &GpuInfo) -> Option<String> {
    let free = gpu.total_mib.saturating_sub(gpu.used_mib);
    (free < MIN_FREE_MIB).then(|| {
        format!(
            "Only {:.1} GB of {} is free ({:.1} GB in use). Close ComfyUI, Invoke or games and try again; SDXL training needs about {:.0} GB.",
            free as f64 / 1024.0,
            gpu.name,
            gpu.used_mib as f64 / 1024.0,
            MIN_FREE_MIB as f64 / 1024.0,
        )
    })
}

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

#[cfg(not(windows))]
fn hide_window(_: &mut Command) {}

// ----- preparing a run -----------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// The same `train.toml` the cloud run uses, minus resume states (about 1.5 GB
/// each, only useful on a rented machine that can vanish).
///
/// `resume` starts from an earlier LoRA's weights instead of from scratch; the
/// optimizer starts fresh, and the rank comes from the weights file.
pub fn render_local_train_toml(
    s: &TrainSettings,
    images: u32,
    p: &TrainPaths,
    resume: Option<&str>,
) -> String {
    let mut out: String = render_train_toml(s, images, p)
        .lines()
        .filter(|l| !l.starts_with("save_state"))
        .map(|l| format!("{l}\n"))
        .collect();
    if let Some(weights) = resume {
        let quoted = serde_json::to_string(weights).expect("string serialises");
        out.push_str(&format!(
            "network_weights = {quoted}\ndim_from_weights = true\n"
        ));
    }
    out
}

pub struct Prepared {
    pub train_toml: PathBuf,
    pub total_steps: u32,
}

/// Lay a run out under `run`: `work/data` (copies of the audited files, each
/// re-checked against the audit), the two configs, and the folders the trainer
/// writes to. `cache` holds the tokenizers the trainer downloads once.
pub fn prepare(
    src: &Path,
    settings: &TrainSettings,
    report: &Report,
    setup: &LocalSetup,
    run: &Path,
    cache: &Path,
    resume: Option<&Path>,
) -> Result<Prepared, String> {
    settings.validate()?;
    if report.has_errors() || report.images.is_empty() {
        return Err("Fix the dataset problems before training.".into());
    }
    let images = u32::try_from(report.images.len()).map_err(|_| "Too many images.")?;
    let data = run.join("work").join("data");
    let checkpoints = run.join("checkpoints");
    for d in [&data, &checkpoints, &run.join("logs")] {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    for img in &report.images {
        for (name, expected) in [
            (img.file.clone(), &img.sha256),
            (format!("{}.txt", img.id), &img.caption_sha256),
        ] {
            let bytes = fs::read(src.join(&name)).map_err(|e| format!("{name}: {e}"))?;
            if &hex(&bytes) != expected {
                return Err(format!("{name} changed since the dataset was checked."));
            }
            fs::write(data.join(&name), bytes).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    let paths = TrainPaths {
        model: slashed(Path::new(&setup.base_model)),
        image_dir: slashed(&data),
        dataset_config: slashed(&run.join("dataset.toml")),
        output_dir: slashed(&checkpoints),
        logging_dir: slashed(&run.join("work").join("tb")),
        tokenizer_cache_dir: slashed(&cache.join("tokenizers")),
    };
    fs::write(
        run.join("dataset.toml"),
        render_dataset_toml(settings, &paths.image_dir),
    )
    .map_err(|e| e.to_string())?;
    let train_toml = run.join("train.toml");
    fs::write(
        &train_toml,
        render_local_train_toml(settings, images, &paths, resume.map(slashed).as_deref()),
    )
    .map_err(|e| e.to_string())?;
    Ok(Prepared {
        train_toml,
        total_steps: settings.total_steps(images),
    })
}

// ----- running the trainer ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

fn trainer_env(cache: &Path) -> Vec<(String, String)> {
    vec![
        (
            "HF_HOME".into(),
            cache.join("hf").to_string_lossy().into_owned(),
        ),
        ("PYTHONUTF8".into(), "1".into()),
        ("HF_HUB_DISABLE_TELEMETRY".into(), "1".into()),
    ]
}

pub fn command(setup: &LocalSetup, train_toml: &Path, cache: &Path) -> TrainCommand {
    TrainCommand {
        program: setup.python(),
        args: vec![
            setup.script().to_string_lossy().into_owned(),
            "--config_file".into(),
            train_toml.to_string_lossy().into_owned(),
        ],
        cwd: PathBuf::from(&setup.trainer_dir),
        env: trainer_env(cache),
    }
}

/// Runs the trainer. A trait so tests don't need a GPU or a Python install.
pub trait Exec: Send + Sync {
    /// Run to the end, appending the trainer's output to `log` and calling
    /// `tick` about twice a second. Stops the process if `cancel` is set.
    /// Returns its exit code.
    fn run(
        &self,
        cmd: &TrainCommand,
        log: &Path,
        cancel: &AtomicBool,
        tick: &mut dyn FnMut(),
    ) -> Result<i32, String>;
}

pub struct ProcessExec;

impl Exec for ProcessExec {
    fn run(
        &self,
        cmd: &TrainCommand,
        log: &Path,
        cancel: &AtomicBool,
        tick: &mut dyn FnMut(),
    ) -> Result<i32, String> {
        let out = fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args)
            .current_dir(&cmd.cwd)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        hide_window(&mut c);
        let mut child = c
            .spawn()
            .map_err(|e| format!("Couldn't start {}: {e}", cmd.program.display()))?;
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(-1);
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status.code().unwrap_or(-1)),
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            tick();
            std::thread::sleep(POLL);
        }
    }
}

/// `(step, total, loss)` from the newest progress-bar line in `text`
/// (`steps:  83%|███ | 25/30 [00:20<00:04,  1.21it/s, avr_loss=0.134]`).
pub fn parse_progress(text: &str) -> Option<(u32, u32, Option<f64>)> {
    let text = text.replace('\r', "\n");
    for line in text.lines().rev() {
        if !line.contains("steps:") {
            continue;
        }
        let Some(open) = line.find(" [") else {
            continue;
        };
        let Some(frag) = line[..open].rsplit('|').next() else {
            continue;
        };
        let Some((a, b)) = frag.trim().split_once('/') else {
            continue;
        };
        let (Ok(step), Ok(total)) = (a.trim().parse(), b.trim().parse()) else {
            continue;
        };
        let loss = line.find("avr_loss=").and_then(|i| {
            let v: String = line[i + 9..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '-'))
                .collect();
            v.parse().ok()
        });
        return Some((step, total, loss));
    }
    None
}

fn tail(path: &Path) -> String {
    let Ok(mut f) = fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map_or(0, |m| m.len());
    if f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The last lines of the log that aren't progress-bar noise, for an error.
fn failure_text(log: &str) -> String {
    let text = log.replace('\r', "\n");
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.contains("steps:"))
        .collect();
    let joined = lines[lines.len().saturating_sub(12)..].join("\n");
    let skip = joined.chars().count().saturating_sub(1500);
    joined.chars().skip(skip).collect()
}

// ----- the manager ---------------------------------------------------------------

pub struct LocalDeps {
    /// Where `local-train.json` lives.
    pub config_dir: PathBuf,
    /// Where SlopTweak sets up its own trainer.
    pub trainer_layout: Arc<dyn Fn() -> Layout + Send + Sync>,
    /// Finished runs land in `<results_root>/<name>-<timestamp>/`.
    pub results_root: Arc<dyn Fn() -> PathBuf + Send + Sync>,
    /// Tokenizers and the Hugging Face cache, kept between runs.
    pub cache_dir: PathBuf,
    pub exec: Arc<dyn Exec>,
    pub gpu: Arc<dyn Fn() -> Option<GpuInfo> + Send + Sync>,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

#[derive(Debug, Clone, Default)]
struct State {
    step: u32,
    loss: Option<f64>,
    finished_at: Option<u64>,
    outcome: Option<&'static str>,
    error: Option<String>,
}

struct Run {
    state: Mutex<State>,
    cancel: AtomicBool,
    started: u64,
    results_dir: PathBuf,
    total_steps: u32,
    gpu_name: Option<String>,
    /// The LoRA this run started from, if it keeps training an earlier one.
    continued_from: Option<PathBuf>,
}

/// What "Keep training" and "Sample" work from: the newest run, which stays
/// known after the run is dismissed.
#[derive(Debug, Clone)]
struct Last {
    dataset_dir: PathBuf,
    /// The settings of the first run (without a step override).
    settings: TrainSettings,
    results_dir: PathBuf,
    /// How many times it has been continued; names the next `-moreN` run.
    rounds: u32,
}

/// An earlier LoRA to continue from.
struct Continue {
    from: PathBuf,
    rounds: u32,
    original: TrainSettings,
}

pub struct LocalManager {
    deps: LocalDeps,
    run: Mutex<Option<Arc<Run>>>,
    last: Mutex<Option<Last>>,
    sampling: AtomicBool,
    sample_cancel: AtomicBool,
    sample_count: AtomicU32,
}

impl LocalManager {
    pub fn new(deps: LocalDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            run: Mutex::new(None),
            last: Mutex::new(None),
            sampling: AtomicBool::new(false),
            sample_cancel: AtomicBool::new(false),
            sample_count: AtomicU32::new(0),
        })
    }

    /// The trainer and checkpoint a run would use now.
    pub fn setup(&self) -> LocalSetup {
        LocalSetup::load(&self.deps.config_dir).resolved(&(self.deps.trainer_layout)())
    }

    /// Where the trainer comes from: `managed` (SlopTweak's own setup),
    /// `existing` (a folder from an earlier version) or `none`.
    pub fn trainer_source(&self) -> &'static str {
        if ready_trainer_dir(&(self.deps.trainer_layout)()).is_some() {
            return "managed";
        }
        let old = LocalSetup::load(&self.deps.config_dir);
        if old.script().is_file() && old.python().is_file() {
            "existing"
        } else {
            "none"
        }
    }

    /// Set the trainer up inside `picked` (in a `SlopTweak-trainer` folder of
    /// its own). Nothing is moved: an earlier setup stays where it was.
    pub fn set_trainer_root(&self, picked: &Path) -> Result<(), String> {
        if !picked.is_dir() {
            return Err(format!("{} isn't a folder.", picked.display()));
        }
        let root = picked.join(ROOT_NAME);
        check_root_usable(&root)?;
        LocalSetup {
            trainer_root: root.to_string_lossy().into_owned(),
            ..LocalSetup::load(&self.deps.config_dir)
        }
        .save(&self.deps.config_dir)
    }

    pub fn set_base_model(&self, path: &str) -> Result<(), String> {
        LocalSetup {
            base_model: path.trim().to_string(),
            ..LocalSetup::load(&self.deps.config_dir)
        }
        .save(&self.deps.config_dir)
    }

    pub fn gpu(&self) -> Option<GpuInfo> {
        (self.deps.gpu)()
    }

    fn now(&self) -> u64 {
        (self.deps.clock)()
    }

    pub fn is_running(&self) -> bool {
        self.run
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|r| r.state.lock().unwrap().outcome.is_none())
    }

    /// Start training `dir` with `settings`. Checks everything up front, then
    /// returns while the trainer runs on its own thread.
    pub fn start(&self, dir: &Path, settings: &TrainSettings) -> Result<(), String> {
        self.begin(dir, settings, None)
    }

    /// Train `steps` more steps from the newest checkpoint of the last run,
    /// on the same images and settings, as a new run beside it.
    pub fn keep_training(&self, steps: u32) -> Result<(), String> {
        if self.is_running() {
            return Err("A training run is already in progress.".into());
        }
        let last = self
            .last
            .lock()
            .unwrap()
            .clone()
            .ok_or("There is no run to keep training.")?;
        let from = checkpoints_in(&last.results_dir)
            .pop()
            .map(|c| last.results_dir.join("checkpoints").join(c.file))
            .ok_or("That run saved no checkpoint to continue from.")?;
        let rounds = last.rounds + 1;
        let settings = TrainSettings {
            steps_override: Some(steps),
            output_name: format!("{}-more{rounds}", last.settings.output_name),
            ..last.settings.clone()
        };
        self.begin(
            &last.dataset_dir,
            &settings,
            Some(Continue {
                from,
                rounds,
                original: last.settings,
            }),
        )
    }

    fn begin(
        &self,
        dir: &Path,
        settings: &TrainSettings,
        cont: Option<Continue>,
    ) -> Result<(), String> {
        if self.is_running() {
            return Err("A training run is already in progress.".into());
        }
        if self.sampling.load(Ordering::SeqCst) {
            return Err("A sample image is being made; wait for it to finish.".into());
        }
        let setup = self.setup();
        if let Some(p) = setup.problems().first() {
            return Err(p.clone());
        }
        let gpu = self.gpu();
        if let Some(p) = gpu.as_ref().and_then(gpu_problem) {
            return Err(p);
        }
        settings.validate()?;
        let report = dataset::audit(dir, settings.resolution)?;
        let now = self.now();
        let results_dir =
            (self.deps.results_root)().join(format!("{}-{now}", settings.output_name));
        let prepared = prepare(
            dir,
            settings,
            &report,
            &setup,
            &results_dir,
            &self.deps.cache_dir,
            cont.as_ref().map(|c| c.from.as_path()),
        )?;
        let run = Arc::new(Run {
            state: Mutex::new(State::default()),
            cancel: AtomicBool::new(false),
            started: now,
            results_dir: results_dir.clone(),
            total_steps: prepared.total_steps,
            gpu_name: gpu.map(|g| g.name),
            continued_from: cont.as_ref().map(|c| c.from.clone()),
        });
        *self.last.lock().unwrap() = Some(Last {
            dataset_dir: dir.to_path_buf(),
            settings: cont
                .as_ref()
                .map_or_else(|| settings.clone(), |c| c.original.clone()),
            results_dir: results_dir.clone(),
            rounds: cont.as_ref().map_or(0, |c| c.rounds),
        });
        *self.run.lock().unwrap() = Some(run.clone());

        let cmd = command(&setup, &prepared.train_toml, &self.deps.cache_dir);
        let exec = self.deps.exec.clone();
        let clock = self.deps.clock.clone();
        let settings = settings.clone();
        std::thread::spawn(move || execute(&*exec, &cmd, &run, &settings, &*clock));
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(r) = self.run.lock().unwrap().as_ref() {
            r.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Forget a finished run so the tab returns to its start.
    pub fn dismiss(&self) {
        if !self.is_running() {
            *self.run.lock().unwrap() = None;
        }
    }

    pub fn results_dir(&self) -> Option<PathBuf> {
        self.run
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.results_dir.clone())
    }

    /// `Status::idle()` when there is no local run to show.
    pub fn status(&self) -> Status {
        let Some(r) = self.run.lock().unwrap().clone() else {
            return Status::idle();
        };
        let st = r.state.lock().unwrap().clone();
        let finished = st.outcome.is_some();
        let end = st.finished_at.unwrap_or_else(|| self.now());
        let message = match (st.outcome, st.step) {
            (Some("completed"), _) => {
                "Done. The LoRA checkpoints are in the results folder.".to_string()
            }
            (Some("cancelled"), _) => {
                "Stopped. Checkpoints saved before you stopped are in the results folder."
                    .to_string()
            }
            (Some(_), _) => "Training failed.".to_string(),
            (None, 0) => {
                "Starting: loading the model and caching your images. This can take a minute."
                    .to_string()
            }
            (None, n) => format!("Training on this PC: step {n} of {}.", r.total_steps),
        };
        Status {
            state: if finished { "finished" } else { "running" }.into(),
            message,
            step: Some(st.step),
            total_steps: Some(r.total_steps),
            gpu_name: r.gpu_name.clone(),
            elapsed_secs: Some(end.saturating_sub(r.started)),
            results_dir: Some(r.results_dir.display().to_string()),
            synced: true,
            outcome: st.outcome.map(str::to_string),
            error: st.error,
            destroyed: true,
            local: true,
            ..Status::idle()
        }
    }
}

// ----- checkpoints and sample images -------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointInfo {
    pub file: String,
    pub step: u32,
}

/// The `name-stepNNNNNNNN.safetensors` files of a run, oldest first.
fn checkpoints_in(results: &Path) -> Vec<CheckpointInfo> {
    let Ok(rd) = fs::read_dir(results.join("checkpoints")) else {
        return Vec::new();
    };
    let mut out: Vec<CheckpointInfo> = rd
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let step = file
                .strip_suffix(".safetensors")?
                .rsplit_once("-step")?
                .1
                .parse()
                .ok()?;
            Some(CheckpointInfo { file, step })
        })
        .collect();
    out.sort_by_key(|c| c.step);
    out
}

/// The same settings the rented-GPU test images use.
const SAMPLE_STEPS: u32 = 20;
const SAMPLE_CFG: f64 = 3.5;

/// One sample image: a text prompt, optionally through a checkpoint of the
/// last run (none = the base model alone, as a control).
#[derive(Debug, Clone, Deserialize)]
pub struct SampleRequest {
    pub prompt: String,
    #[serde(default)]
    pub checkpoint: Option<String>,
    pub strength: f64,
    pub seed: u32,
    /// `portrait`, `square` or `landscape`.
    pub shape: String,
}

impl SampleRequest {
    fn size(&self) -> Result<(u32, u32), String> {
        match self.shape.as_str() {
            "portrait" => Ok((832, 1216)),
            "square" => Ok((1024, 1024)),
            "landscape" => Ok((1216, 832)),
            other => Err(format!("Unknown image shape {other:?}.")),
        }
    }

    fn validate(&self) -> Result<(), String> {
        let prompt = self.prompt.trim();
        if prompt.is_empty() || prompt.chars().count() > 600 {
            return Err("Write a prompt of up to 600 characters.".into());
        }
        // The sampler reads `--x` options out of the prompt line.
        if prompt.contains("--") || prompt.contains(['\n', '\r']) {
            return Err("A prompt can't contain \"--\" or line breaks.".into());
        }
        if !(0.0..=2.0).contains(&self.strength) {
            return Err("Strength must be between 0 and 2.".into());
        }
        self.size().map(|_| ())
    }

    /// The line `sdxl_gen_img.py --from_file` reads.
    fn line(&self) -> Result<String, String> {
        let (w, h) = self.size()?;
        Ok(format!(
            "{} --d {} --w {w} --h {h} --s {SAMPLE_STEPS} --l {SAMPLE_CFG} --n {EVAL_NEGATIVE}\n",
            self.prompt.trim(),
            self.seed
        ))
    }
}

fn sample_command(
    setup: &LocalSetup,
    from_file: &Path,
    outdir: &Path,
    cache: &Path,
    lora: Option<(&Path, f64)>,
) -> TrainCommand {
    let mut args: Vec<String> = vec![
        Path::new(&setup.trainer_dir)
            .join("sdxl_gen_img.py")
            .to_string_lossy()
            .into_owned(),
        "--ckpt".into(),
        setup.base_model.clone(),
        "--from_file".into(),
        from_file.to_string_lossy().into_owned(),
        "--outdir".into(),
        outdir.to_string_lossy().into_owned(),
        "--tokenizer_cache_dir".into(),
        cache.join("tokenizers").to_string_lossy().into_owned(),
        "--max_embeddings_multiples".into(),
        "2".into(),
        "--sampler".into(),
        "euler_a".into(),
        "--bf16".into(),
        "--no_half_vae".into(),
        "--sdpa".into(),
        "--sequential_file_name".into(),
    ];
    if let Some((weights, strength)) = lora {
        args.extend([
            "--network_module".into(),
            "networks.lora".into(),
            "--network_weights".into(),
            weights.to_string_lossy().into_owned(),
            "--network_mul".into(),
            strength.to_string(),
        ]);
    }
    TrainCommand {
        program: setup.python(),
        args,
        cwd: PathBuf::from(&setup.trainer_dir),
        env: trainer_env(cache),
    }
}

/// Clears a flag when it goes out of scope, so an error can't leave it set.
struct Flag<'a>(&'a AtomicBool);

impl Drop for Flag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl LocalManager {
    /// The newest run's checkpoints, oldest first.
    pub fn checkpoints(&self) -> Vec<CheckpointInfo> {
        self.last
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| checkpoints_in(&l.results_dir))
            .unwrap_or_default()
    }

    pub fn cancel_sample(&self) {
        self.sample_cancel.store(true, Ordering::SeqCst);
    }

    /// Make one image and return its path (under the run's `samples/`). Blocks
    /// for the model load plus the image, about half a minute on a laptop GPU.
    pub fn sample(&self, req: &SampleRequest) -> Result<PathBuf, String> {
        req.validate()?;
        if self.is_running() {
            return Err("Wait for training to finish before making a sample image.".into());
        }
        if self.sampling.swap(true, Ordering::SeqCst) {
            return Err("A sample image is already being made.".into());
        }
        let _busy = Flag(&self.sampling);
        self.sample_cancel.store(false, Ordering::SeqCst);

        let setup = self.setup();
        if let Some(p) = setup.problems().first() {
            return Err(p.clone());
        }
        if let Some(p) = self.gpu().as_ref().and_then(gpu_problem) {
            return Err(p);
        }
        let last = self
            .last
            .lock()
            .unwrap()
            .clone()
            .ok_or("Train a LoRA first; sample images are saved with its run.")?;
        let lora = match req.checkpoint.as_deref() {
            None => None,
            Some(f) if f.contains(['/', '\\']) || !f.ends_with(".safetensors") => {
                return Err(format!("{f:?} isn't one of this run's checkpoints."))
            }
            Some(f) => {
                let path = last.results_dir.join("checkpoints").join(f);
                if !path.is_file() {
                    return Err(format!("{f} is no longer in the results folder."));
                }
                Some((path, req.strength))
            }
        };
        let n = self.sample_count.fetch_add(1, Ordering::SeqCst);
        let dir = last
            .results_dir
            .join("samples")
            .join(format!("{}-{n}", self.now()));
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let from_file = dir.join("prompt.txt");
        fs::write(&from_file, req.line()?).map_err(|e| e.to_string())?;
        let cmd = sample_command(
            &setup,
            &from_file,
            &dir,
            &self.deps.cache_dir,
            lora.as_ref().map(|(p, s)| (p.as_path(), *s)),
        );
        let log = dir.join("sample.log");
        let code = self
            .deps
            .exec
            .run(&cmd, &log, &self.sample_cancel, &mut || {})?;
        if self.sample_cancel.load(Ordering::SeqCst) {
            return Err("Cancelled.".into());
        }
        if code != 0 {
            return Err(format!(
                "The sampler exited with code {code}.\n{}",
                failure_text(&tail(&log))
            ));
        }
        let mut images: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "png"))
            .collect();
        images.sort();
        images
            .into_iter()
            .next()
            .ok_or_else(|| "The sampler finished but wrote no image.".to_string())
    }
}

/// The body of the training thread.
fn execute(
    exec: &dyn Exec,
    cmd: &TrainCommand,
    run: &Run,
    settings: &TrainSettings,
    clock: &(dyn Fn() -> u64 + Send + Sync),
) {
    let log = run.results_dir.join("logs").join("train.log");
    let mut tick = || {
        if let Some((step, _, loss)) = parse_progress(&tail(&log)) {
            let mut st = run.state.lock().unwrap();
            st.step = step;
            st.loss = loss.or(st.loss);
        }
    };
    let result = exec.run(cmd, &log, &run.cancel, &mut tick);
    tick();
    let text = tail(&log);
    let (outcome, error) = if run.cancel.load(Ordering::SeqCst) {
        ("cancelled", None)
    } else {
        match result {
            Ok(0) => {
                let last = run
                    .results_dir
                    .join("checkpoints")
                    .join(settings.checkpoint_file(run.total_steps));
                if last.is_file() {
                    ("completed", None)
                } else {
                    (
                        "failed",
                        Some("Training ended without writing the final checkpoint.".to_string()),
                    )
                }
            }
            Ok(code) => (
                "failed",
                Some(format!(
                    "The trainer exited with code {code}.\n{}",
                    failure_text(&text)
                )),
            ),
            Err(e) => ("failed", Some(e)),
        }
    };
    if outcome != "failed" {
        write_manifest(&run.results_dir, run.continued_from.as_deref());
        let _ = fs::remove_dir_all(run.results_dir.join("work"));
    }
    let mut st = run.state.lock().unwrap();
    st.finished_at = Some(clock());
    st.error = error;
    st.outcome = Some(outcome);
}

/// `manifest.json`: every checkpoint with its size and SHA-256, and the LoRA
/// the run started from when it kept training one.
fn write_manifest(results: &Path, continued_from: Option<&Path>) {
    let mut files = Vec::new();
    if let Ok(rd) = fs::read_dir(results.join("checkpoints")) {
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "safetensors"))
        {
            if let Ok(bytes) = fs::read(p) {
                files.push(json!({
                    "file": p.file_name().map(|n| n.to_string_lossy().into_owned()),
                    "bytes": bytes.len(),
                    "sha256": hex(&bytes),
                }));
            }
        }
    }
    let _ = fs::write(
        results.join("manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "files": files,
            "continued_from": continued_from.map(|p| json!({
                "file": slashed(p),
                "sha256": fs::read(p).ok().map(|b| hex(&b)),
            })),
        }))
        .unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: &str = "steps:  83%|████████▎ | 25/30 [00:20<00:04,  1.21it/s, avr_loss=0.134]";

    #[test]
    fn progress_comes_from_the_newest_bar_line() {
        let log =
            format!("loading\rsteps:   3%|▎ | 1/30 [00:01<00:30, 1.0it/s, avr_loss=0.5]\r{BAR}\r");
        assert_eq!(parse_progress(&log), Some((25, 30, Some(0.134))));
        let no_loss = "steps:   0%|  | 0/960 [00:00<?, ?it/s]";
        assert_eq!(parse_progress(no_loss), Some((0, 960, None)));
        assert_eq!(parse_progress("Traceback (most recent call last):"), None);
        assert_eq!(parse_progress(""), None);
    }

    #[test]
    fn gpu_lines_parse_and_a_busy_gpu_is_refused() {
        let g = parse_gpu_csv("NVIDIA GeForce RTX 4080 Laptop GPU, 12282, 9155\n").unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 4080 Laptop GPU");
        assert_eq!((g.total_mib, g.used_mib), (12282, 9155));
        let why = gpu_problem(&g).unwrap();
        assert!(why.contains("ComfyUI") && why.contains("3.1 GB"), "{why}");
        let idle = GpuInfo { used_mib: 450, ..g };
        assert_eq!(gpu_problem(&idle), None);
        assert_eq!(parse_gpu_csv(""), None);
        assert_eq!(parse_gpu_csv("garbage"), None);
    }

    fn png(salt: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(1033u32.to_be_bytes());
        b.extend(1522u32.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, salt]);
        b
    }

    fn dataset(n: u8) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for i in 0..n {
            fs::write(d.path().join(format!("i{i}.png")), png(i)).unwrap();
            fs::write(d.path().join(format!("i{i}.txt")), "trig, solo, front").unwrap();
        }
        d
    }

    /// A trainer folder, venv and checkpoint that only need to exist.
    fn installed(root: &Path) -> LocalSetup {
        let setup = LocalSetup {
            trainer_dir: root.join("sd-scripts").to_string_lossy().into_owned(),
            base_model: root.join("base.safetensors").to_string_lossy().into_owned(),
            ..Default::default()
        };
        fs::create_dir_all(setup.python().parent().unwrap()).unwrap();
        fs::write(setup.python(), "").unwrap();
        fs::write(setup.script(), "").unwrap();
        fs::write(&setup.base_model, "").unwrap();
        setup
    }

    fn managed_layout(root: &Path) -> Arc<dyn Fn() -> Layout + Send + Sync> {
        let managed = root.join("managed");
        Arc::new(move || Layout::new(managed.clone()))
    }

    #[test]
    fn the_trainer_location_comes_from_the_override_then_the_choice_then_the_default() {
        let d = tempfile::tempdir().unwrap();
        let default = d.path().join("default");
        assert_eq!(trainer_root(d.path(), &default, None), default);
        let rig = rig(Behaviour::Succeed, 11_000);
        let picked = rig.root.path().join("drive");
        fs::create_dir(&picked).unwrap();
        rig.manager.set_trainer_root(&picked).unwrap();
        let chosen = picked.join(ROOT_NAME);
        assert_eq!(trainer_root(rig.root.path(), &default, None), chosen);
        let dev = PathBuf::from("elsewhere");
        assert_eq!(
            trainer_root(rig.root.path(), &default, Some(dev.clone())),
            dev
        );
        // The checkpoint and the old trainer folder are kept when the place changes.
        rig.manager.set_base_model("m.safetensors").unwrap();
        let saved = LocalSetup::load(rig.root.path());
        assert_eq!(saved.trainer_root, chosen.to_string_lossy());
        assert_eq!(saved.base_model, "m.safetensors");
        assert!(saved.trainer_dir.contains("sd-scripts"));
    }

    #[test]
    fn a_trainer_folder_never_lands_in_a_folder_that_holds_other_things() {
        let rig = rig(Behaviour::Succeed, 11_000);
        let picked = rig.root.path().join("ai");
        fs::create_dir_all(picked.join(ROOT_NAME)).unwrap();
        fs::write(picked.join(ROOT_NAME).join("notes.txt"), "mine").unwrap();
        let err = rig.manager.set_trainer_root(&picked).unwrap_err();
        assert!(
            err.contains("notes.txt") && err.contains("different folder"),
            "{err}"
        );
        assert!(rig
            .manager
            .set_trainer_root(&picked.join("missing"))
            .is_err());
        // Its own leftovers are fine.
        fs::remove_file(picked.join(ROOT_NAME).join("notes.txt")).unwrap();
        fs::create_dir(picked.join(ROOT_NAME).join("cache")).unwrap();
        rig.manager.set_trainer_root(&picked).unwrap();
    }

    fn quick() -> TrainSettings {
        TrainSettings {
            output_name: "lora".into(),
            epochs: 2,
            ..Default::default()
        }
    }

    #[test]
    fn setup_reports_what_is_missing_and_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let empty = LocalSetup::load(d.path());
        assert_eq!(empty.problems().len(), 2);
        let setup = installed(d.path());
        assert!(setup.problems().is_empty());
        setup.save(d.path()).unwrap();
        assert_eq!(LocalSetup::load(d.path()), setup);
        let wrong = LocalSetup {
            trainer_dir: d.path().to_string_lossy().into_owned(),
            ..setup
        };
        assert!(
            wrong.problems()[0].contains("set up"),
            "{:?}",
            wrong.problems()
        );
    }

    #[test]
    fn prepare_lays_out_a_run_with_the_cloud_settings_minus_resume_states() {
        let (src, root) = (dataset(3), tempfile::tempdir().unwrap());
        let setup = installed(root.path());
        let report = dataset::audit(src.path(), 1024).unwrap();
        let run = root.path().join("run");
        let cache = root.path().join("cache");
        let p = prepare(src.path(), &quick(), &report, &setup, &run, &cache, None).unwrap();
        assert_eq!(p.total_steps, 6);
        let toml = fs::read_to_string(&p.train_toml).unwrap();
        let t: toml::Table = toml.parse().unwrap();
        assert_eq!(t["max_train_steps"].as_integer(), Some(6));
        assert_eq!(t["mixed_precision"].as_str(), Some("bf16"));
        assert!(!toml.contains("save_state"));
        assert!(t["pretrained_model_name_or_path"]
            .as_str()
            .unwrap()
            .ends_with("/base.safetensors"));
        assert!(!t["output_dir"].as_str().unwrap().contains('\\'));
        let ds: toml::Table = fs::read_to_string(run.join("dataset.toml"))
            .unwrap()
            .parse()
            .unwrap();
        let dir = ds["datasets"][0]["subsets"][0]["image_dir"]
            .as_str()
            .unwrap();
        assert!(Path::new(dir).join("i2.png").is_file());
        assert!(Path::new(dir).join("i2.txt").is_file());
        assert!(run.join("checkpoints").is_dir() && run.join("logs").is_dir());
    }

    #[test]
    fn prepare_refuses_a_file_edited_after_the_audit_and_bad_input() {
        let (src, root) = (dataset(3), tempfile::tempdir().unwrap());
        let setup = installed(root.path());
        let report = dataset::audit(src.path(), 1024).unwrap();
        fs::write(src.path().join("i1.txt"), "trig, changed").unwrap();
        let run = root.path().join("r");
        let err = prepare(
            src.path(),
            &quick(),
            &report,
            &setup,
            &run,
            root.path(),
            None,
        )
        .err()
        .unwrap();
        assert!(err.contains("i1.txt changed"), "{err}");

        let bad = TrainSettings { rank: 0, ..quick() };
        let run = root.path().join("r2");
        assert!(prepare(src.path(), &bad, &report, &setup, &run, root.path(), None).is_err());
        let empty = tempfile::tempdir().unwrap();
        let none = dataset::audit(empty.path(), 1024).unwrap();
        let run = root.path().join("r3");
        assert!(prepare(
            empty.path(),
            &quick(),
            &none,
            &setup,
            &run,
            root.path(),
            None
        )
        .is_err());
    }

    #[derive(Clone, Copy)]
    enum Behaviour {
        Succeed,
        Crash,
        UntilCancelled,
    }

    /// Stands in for the Python trainer: writes a progress log and checkpoints.
    struct Fake(Behaviour);

    impl Exec for Fake {
        fn run(
            &self,
            cmd: &TrainCommand,
            log: &Path,
            cancel: &AtomicBool,
            tick: &mut dyn FnMut(),
        ) -> Result<i32, String> {
            assert!(cmd.env.iter().any(|(k, _)| k == "HF_HOME"));
            if cmd.args[0].ends_with("sdxl_gen_img.py") {
                return self.sample(cmd, log);
            }
            let run = Path::new(&cmd.args[2]).parent().unwrap().to_path_buf();
            assert!(cmd.args[0].ends_with("sdxl_train_network.py"));
            let toml: toml::Table = fs::read_to_string(&cmd.args[2]).unwrap().parse().unwrap();
            let name = toml["output_name"].as_str().unwrap().to_string();
            let steps = toml["max_train_steps"].as_integer().unwrap();
            match self.0 {
                Behaviour::Succeed => {
                    let done = format!(
                        "steps: 100%|██| {steps}/{steps} [00:05<00:00, 1.2it/s, avr_loss=0.1]"
                    );
                    fs::write(log, format!("{BAR}\r{done}")).unwrap();
                    for f in [
                        format!("{name}-step{steps:08}.safetensors"),
                        format!("{name}.safetensors"),
                    ] {
                        fs::write(run.join("checkpoints").join(f), b"weights").unwrap();
                    }
                    tick();
                    Ok(0)
                }
                Behaviour::Crash => {
                    let text = "steps: 10%|▏ | 1/6 [00:01<00:05, 1.0it/s]\n\
                                Traceback (most recent call last):\n\
                                torch.OutOfMemoryError: CUDA out of memory\n";
                    fs::write(log, text).unwrap();
                    Ok(1)
                }
                Behaviour::UntilCancelled => {
                    let text = "steps:  50%|▌ | 3/6 [00:03<00:03, 1.0it/s, avr_loss=0.2]";
                    fs::write(log, text).unwrap();
                    while !cancel.load(Ordering::SeqCst) {
                        tick();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(-1)
                }
            }
        }
    }

    impl Fake {
        /// `sdxl_gen_img.py`: writes `00001.png` into `--outdir`.
        fn sample(&self, cmd: &TrainCommand, log: &Path) -> Result<i32, String> {
            let arg = |k: &str| {
                let at = cmd.args.iter().position(|a| a == k).unwrap();
                cmd.args[at + 1].clone()
            };
            if matches!(self.0, Behaviour::Crash) {
                fs::write(
                    log,
                    "Traceback (most recent call last):\nValueError: bad lora\n",
                )
                .unwrap();
                return Ok(1);
            }
            let line = fs::read_to_string(arg("--from_file")).unwrap();
            assert!(
                line.contains(" --w ") && line.contains(" --n worst quality"),
                "{line}"
            );
            fs::write(Path::new(&arg("--outdir")).join("00001.png"), png(1)).unwrap();
            Ok(0)
        }
    }

    struct Rig {
        manager: Arc<LocalManager>,
        root: tempfile::TempDir,
        src: tempfile::TempDir,
    }

    fn rig(behaviour: Behaviour, free_mib: u64) -> Rig {
        let (root, src) = (tempfile::tempdir().unwrap(), dataset(3));
        installed(root.path()).save(root.path()).unwrap();
        let results = root.path().join("results");
        let manager = LocalManager::new(LocalDeps {
            config_dir: root.path().to_path_buf(),
            results_root: Arc::new(move || results.clone()),
            trainer_layout: managed_layout(root.path()),
            cache_dir: root.path().join("cache"),
            exec: Arc::new(Fake(behaviour)),
            gpu: Arc::new(move || {
                Some(GpuInfo {
                    name: "Test GPU".into(),
                    total_mib: 12_000,
                    used_mib: 12_000 - free_mib,
                })
            }),
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            }),
        });
        Rig { manager, root, src }
    }

    fn wait(m: &LocalManager, done: impl Fn(&Status) -> bool) -> Status {
        for _ in 0..500 {
            let s = m.status();
            if done(&s) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out: {:?}", m.status());
    }

    #[test]
    fn a_run_completes_and_leaves_checkpoints_and_a_manifest() {
        let r = rig(Behaviour::Succeed, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        let s = wait(&r.manager, |s| s.state == "finished");
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);
        assert!(s.local && s.destroyed && s.error.is_none());
        assert_eq!((s.step, s.total_steps), (Some(6), Some(6)));
        assert_eq!(s.gpu_name.as_deref(), Some("Test GPU"));
        assert_eq!((s.hourly, s.max_usd, s.spent_usd), (None, None, None));
        let dir = PathBuf::from(s.results_dir.unwrap());
        assert!(dir.starts_with(r.root.path().join("results")));
        assert!(dir
            .join("checkpoints/lora-step00000006.safetensors")
            .is_file());
        assert!(
            !dir.join("work").exists(),
            "the copied images are cleaned up"
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        let files = manifest["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["sha256"], hex(b"weights"));
        r.manager.dismiss();
        assert_eq!(r.manager.status().state, "idle");
    }

    #[test]
    fn a_crash_keeps_the_work_folder_and_shows_the_trainers_words() {
        let r = rig(Behaviour::Crash, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        let s = wait(&r.manager, |s| s.state == "finished");
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        let e = s.error.unwrap();
        assert!(
            e.contains("code 1") && e.contains("CUDA out of memory"),
            "{e}"
        );
        assert!(!e.contains("steps:"), "{e}");
        assert!(PathBuf::from(s.results_dir.unwrap()).join("work").is_dir());
    }

    #[test]
    fn stopping_ends_the_run_as_cancelled() {
        let r = rig(Behaviour::UntilCancelled, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        let s = wait(&r.manager, |s| s.step == Some(3));
        assert_eq!(s.state, "running");
        assert!(s.message.contains("step 3 of 6"), "{}", s.message);
        assert!(r.manager.is_running());
        let again = r.manager.start(r.src.path(), &quick());
        assert!(again.unwrap_err().contains("already"));
        r.manager.cancel();
        let s = wait(&r.manager, |s| s.state == "finished");
        assert_eq!(s.outcome.as_deref(), Some("cancelled"));
        assert!(s.error.is_none());
    }

    #[test]
    fn it_refuses_to_start_without_setup_or_free_memory() {
        let r = rig(Behaviour::Succeed, 3_000);
        let err = r.manager.start(r.src.path(), &quick()).unwrap_err();
        assert!(err.contains("free") && err.contains("Test GPU"), "{err}");
        assert_eq!(r.manager.status().state, "idle");

        let r = rig(Behaviour::Succeed, 11_000);
        r.manager.set_base_model("").unwrap();
        let err = r.manager.start(r.src.path(), &quick()).unwrap_err();
        assert!(err.contains("Choose the SDXL checkpoint"), "{err}");

        LocalSetup::default().save(r.root.path()).unwrap();
        let err = r.manager.start(r.src.path(), &quick()).unwrap_err();
        assert!(err.contains("set up"), "{err}");
        assert_eq!(r.manager.trainer_source(), "none");
    }

    #[test]
    fn keep_training_continues_from_the_newest_checkpoint_as_a_new_run() {
        let r = rig(Behaviour::Succeed, 11_000);
        assert!(r.manager.keep_training(10).unwrap_err().contains("no run"));
        r.manager.start(r.src.path(), &quick()).unwrap();
        let first = wait(&r.manager, |s| s.state == "finished");
        let first_dir = PathBuf::from(first.results_dir.unwrap());
        let sums = |m: &LocalManager| m.checkpoints().iter().map(|c| c.step).collect::<Vec<_>>();
        assert_eq!(sums(&r.manager), [6]);

        r.manager.dismiss();
        r.manager.keep_training(10).unwrap();
        let s = wait(&r.manager, |s| s.state == "finished");
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);
        assert_eq!(s.total_steps, Some(10));
        let dir = PathBuf::from(s.results_dir.unwrap());
        assert_ne!(dir, first_dir);
        let t: toml::Table = fs::read_to_string(dir.join("train.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(t["max_train_steps"].as_integer(), Some(10));
        assert_eq!(t["output_name"].as_str(), Some("lora-more1"));
        assert_eq!(t["dim_from_weights"].as_bool(), Some(true));
        let from = t["network_weights"].as_str().unwrap();
        assert!(from.ends_with("/lora-step00000006.safetensors"), "{from}");
        assert!(!from.contains('\\'));
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["continued_from"]["sha256"], hex(b"weights"));
        assert_eq!(sums(&r.manager), [10]);

        // A third round continues from the second, and names itself -more2.
        r.manager.keep_training(4).unwrap();
        let s = wait(&r.manager, |s| {
            s.state == "finished" && s.total_steps == Some(4)
        });
        let dir = PathBuf::from(s.results_dir.unwrap());
        let t: toml::Table = fs::read_to_string(dir.join("train.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(t["output_name"].as_str(), Some("lora-more2"));
        assert!(t["network_weights"]
            .as_str()
            .unwrap()
            .ends_with("/lora-more1-step00000010.safetensors"));
    }

    #[test]
    fn keep_training_refuses_a_run_with_no_checkpoint_and_bad_steps() {
        let r = rig(Behaviour::UntilCancelled, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        wait(&r.manager, |s| s.step == Some(3));
        assert!(r.manager.keep_training(5).unwrap_err().contains("already"));
        r.manager.cancel();
        wait(&r.manager, |s| s.state == "finished");
        let err = r.manager.keep_training(5).unwrap_err();
        assert!(err.contains("no checkpoint"), "{err}");

        let r = rig(Behaviour::Succeed, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        wait(&r.manager, |s| s.state == "finished");
        assert!(r.manager.keep_training(0).is_err());
        assert!(r.manager.keep_training(20_001).is_err());
    }

    fn request(checkpoint: Option<&str>) -> SampleRequest {
        SampleRequest {
            prompt: "thales_miletus, solo, portrait".into(),
            checkpoint: checkpoint.map(str::to_string),
            strength: 0.8,
            seed: 7,
            shape: "portrait".into(),
        }
    }

    #[test]
    fn a_sample_runs_the_sampler_with_the_checkpoint_and_returns_the_image() {
        let r = rig(Behaviour::Succeed, 11_000);
        assert!(r
            .manager
            .sample(&request(None))
            .unwrap_err()
            .contains("Train a LoRA first"));
        r.manager.start(r.src.path(), &quick()).unwrap();
        let s = wait(&r.manager, |s| s.state == "finished");
        let dir = PathBuf::from(s.results_dir.unwrap());
        let ck = r.manager.checkpoints();
        assert_eq!(ck.len(), 1);

        let img = r.manager.sample(&request(Some(&ck[0].file))).unwrap();
        assert!(img.starts_with(dir.join("samples")) && img.is_file());
        let line = fs::read_to_string(img.parent().unwrap().join("prompt.txt")).unwrap();
        assert!(line.starts_with(
            "thales_miletus, solo, portrait --d 7 --w 832 --h 1216 --s 20 --l 3.5 --n "
        ));
        // The base model alone is a valid control.
        assert!(r.manager.sample(&request(None)).unwrap().is_file());
        // Two samples never share a folder.
        let other = r.manager.sample(&request(None)).unwrap();
        assert_ne!(other.parent(), img.parent());
    }

    #[test]
    fn a_bad_sample_request_is_refused_before_anything_runs() {
        let r = rig(Behaviour::Succeed, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        wait(&r.manager, |s| s.state == "finished");
        let bad = |f: &dyn Fn(&mut SampleRequest)| {
            let mut q = request(None);
            f(&mut q);
            r.manager.sample(&q).unwrap_err()
        };
        assert!(bad(&|q| q.prompt = "  ".into()).contains("prompt"));
        assert!(bad(&|q| q.prompt = "a --n ugly".into()).contains("--"));
        assert!(bad(&|q| q.prompt = "a\nb".into()).contains("line breaks"));
        assert!(bad(&|q| q.strength = 3.0).contains("Strength"));
        assert!(bad(&|q| q.shape = "tall".into()).contains("shape"));
        assert!(bad(&|q| q.checkpoint = Some("../x.safetensors".into())).contains("isn't one"));
        assert!(bad(&|q| q.checkpoint = Some("nope.safetensors".into())).contains("no longer"));
    }

    #[test]
    fn a_sample_waits_for_training_and_reports_sampler_errors() {
        let r = rig(Behaviour::UntilCancelled, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        wait(&r.manager, |s| s.step == Some(3));
        assert!(r
            .manager
            .sample(&request(None))
            .unwrap_err()
            .contains("Wait for training"));
        r.manager.cancel();
        wait(&r.manager, |s| s.state == "finished");

        let r = rig(Behaviour::Crash, 11_000);
        r.manager.start(r.src.path(), &quick()).unwrap();
        wait(&r.manager, |s| s.state == "finished");
        let err = r.manager.sample(&request(None)).unwrap_err();
        assert!(err.contains("code 1") && err.contains("bad lora"), "{err}");
        // A failed sample leaves the manager usable.
        assert!(r.manager.sample(&request(None)).is_err());
    }

    /// Runs the real trainer on this PC's GPU through `LocalManager` (about two
    /// minutes). Needs `SLOPTWEAK_LOCAL_TRAINER_DIR`, `SLOPTWEAK_LOCAL_BASE_MODEL`
    /// and `SLOPTWEAK_LOCAL_DATASET`; run with `--ignored --nocapture`.
    #[test]
    #[ignore = "needs a GPU, the installed trainer and a real dataset"]
    fn real_trainer_smoke() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let root = tempfile::tempdir().unwrap();
        LocalSetup {
            trainer_dir: env("SLOPTWEAK_LOCAL_TRAINER_DIR"),
            base_model: env("SLOPTWEAK_LOCAL_BASE_MODEL"),
            ..Default::default()
        }
        .save(root.path())
        .unwrap();
        let results = root.path().join("results");
        let manager = LocalManager::new(LocalDeps {
            config_dir: root.path().to_path_buf(),
            results_root: Arc::new(move || results.clone()),
            trainer_layout: managed_layout(root.path()),
            cache_dir: PathBuf::from(env("SLOPTWEAK_LOCAL_CACHE")),
            exec: Arc::new(ProcessExec),
            gpu: Arc::new(query_gpu),
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            }),
        });
        let settings = TrainSettings {
            output_name: "smoke".into(),
            epochs: 1,
            save_points: 1,
            ..Default::default()
        };
        manager
            .start(Path::new(&env("SLOPTWEAK_LOCAL_DATASET")), &settings)
            .unwrap();
        let started = std::time::Instant::now();
        let mut last = None;
        let s = loop {
            let s = manager.status();
            if s.state == "finished" || started.elapsed() > Duration::from_secs(600) {
                break s;
            }
            if s.step != last {
                println!("{} ({}s)", s.message, started.elapsed().as_secs());
                last = s.step;
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        println!("{s:#?}");
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);
        let dir = PathBuf::from(s.results_dir.unwrap());
        let files: Vec<_> = fs::read_dir(dir.join("checkpoints"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        println!("checkpoints: {files:?}");
        assert!(dir.join("manifest.json").is_file());
        assert!(files.iter().any(|f| f.starts_with("smoke-step")));

        // A sample image through the newest checkpoint, then keep training it.
        let newest = manager.checkpoints().pop().expect("a checkpoint").file;
        let request = SampleRequest {
            prompt: "thales_miletus, solo, portrait".into(),
            checkpoint: Some(newest),
            strength: 1.0,
            seed: 7,
            shape: "portrait".into(),
        };
        let t = std::time::Instant::now();
        let image = manager.sample(&request).expect("sample");
        println!("sample: {} in {}s", image.display(), t.elapsed().as_secs());
        assert!(fs::metadata(&image).unwrap().len() > 10_000);

        manager.dismiss();
        manager.keep_training(12).expect("keep training");
        let s = loop {
            let s = manager.status();
            if s.state == "finished" || started.elapsed() > Duration::from_secs(1200) {
                break s;
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);
        let more = PathBuf::from(s.results_dir.unwrap());
        let toml = fs::read_to_string(more.join("train.toml")).unwrap();
        println!(
            "continued: {}\n{}",
            more.display(),
            toml.lines().rev().take(3).collect::<Vec<_>>().join("\n")
        );
        let steps: Vec<u32> = manager.checkpoints().iter().map(|c| c.step).collect();
        println!("continuation checkpoints: {steps:?}");
        assert_eq!(steps.last(), Some(&12));
    }
}
