//! One-time setup of the local SDXL trainer, done by SlopTweak itself so the
//! user never touches scripts or virtual environments.
//!
//! Everything lives under one folder (the "root"): the pinned kohya sd-scripts
//! source, a pinned `uv`, a managed Python, and a hash-locked virtual
//! environment. The same pins as the rented-GPU runs are used where they
//! apply: the trainer commit and its tree digest (`job.rs`), `uv` 0.9.5 and
//! Python 3.12.12 (`instance/train_runner.py`). The Windows package list is
//! `trainer/requirements-windows.lock`, compiled from the same inputs as
//! `instance/train_requirements.lock`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::job::{TRAINER_COMMIT, TRAINER_TREE_SHA256};
use super::local::{Exec, TrainCommand};

pub const SD_SCRIPTS_URL: &str = "https://codeload.github.com/kohya-ss/sd-scripts/tar.gz/";
/// The `uv` 0.9.5 Windows wheel from PyPI. Its hash is one of the pins in
/// `instance/train_bootstrap.lock`.
pub const UV_WHEEL_URL: &str = "https://files.pythonhosted.org/packages/af/14/0f07d0b2e561548b4e3006208480a5fce8cdaae5247d85efbfb56e8e596b/uv-0.9.5-py3-none-win_amd64.whl";
pub const UV_WHEEL_SHA256: &str =
    "48a12390421f91af8a8993cf15c38297c0bb121936046286e287975b2fbf1789";
pub const PYTHON_VERSION: &str = "3.12.12";
pub const TORCH_INDEX: &str = "https://download.pytorch.org/whl/cu124";
pub const PYPI_INDEX: &str = "https://pypi.org/simple";
const LOCK: &str = include_str!("../../trainer/requirements-windows.lock");

/// What the user is told before the first setup (measured, rounded up).
pub const DOWNLOAD_GB: f64 = 4.0;
/// Free space needed while installing; the finished install takes about 5 GB.
pub const DISK_GB: f64 = 8.0;

const STEPS: [&str; 6] = [
    "Downloading the trainer",
    "Preparing the package tool",
    "Installing Python",
    "Creating the environment",
    "Installing PyTorch and libraries",
    "Checking the install",
];

// ----- where it goes ------------------------------------------------------------------

/// Setup always goes in a folder of this name inside whatever the user picks,
/// so it never mixes with, or cleans up inside, a folder that holds other things.
pub const ROOT_NAME: &str = "SlopTweak-trainer";

/// Everything SlopTweak puts directly inside its trainer folder.
const OWN_ENTRIES: [&str; 9] = [
    "sd-scripts",
    "sd-scripts.partial",
    "tools",
    "python",
    "cache",
    "downloads",
    "logs",
    "installed.json",
    "requirements.lock",
];

/// Refuse a folder that already holds something that isn't ours: setup deletes
/// and replaces things inside it.
pub fn check_root_usable(root: &Path) -> Result<(), String> {
    if root.is_file() {
        return Err(format!("{} is a file, not a folder.", root.display()));
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(()); // Doesn't exist yet: it will be created.
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !OWN_ENTRIES.contains(&name.as_str()) {
            return Err(format!(
                "{} already holds other files ({name}), so SlopTweak won't set up there. Choose a different folder.",
                root.display()
            ));
        }
    }
    Ok(())
}

/// Free bytes for the current user on the drive holding `path` (or the nearest
/// folder above it that exists). `None` when it can't be read.
#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let mut dir = path;
    while !dir.exists() {
        dir = dir.parent()?;
    }
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the two optional
    // out-pointers are null, which the API allows, and `available` is a valid u64.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(not(windows))]
pub fn free_space(_: &Path) -> Option<u64> {
    None
}

/// `D:` for `D:\ai\x`, or a plain fallback.
fn drive_label(path: &Path) -> String {
    match path.components().next() {
        Some(std::path::Component::Prefix(p)) => p.as_os_str().to_string_lossy().into_owned(),
        _ => "that drive".into(),
    }
}

/// Why there isn't room to set up at `root`, if there isn't.
pub fn space_problem(root: &Path, free: Option<u64>) -> Option<String> {
    let free = free?;
    (free < (DISK_GB * 1e9) as u64).then(|| {
        format!(
            "Only {:.1} GB is free on {}, and setting up the trainer needs about {:.0} GB while it installs (5 GB afterwards). Free some space, or choose a folder on another drive.",
            free as f64 / 1e9,
            drive_label(root),
            DISK_GB
        )
    })
}

/// Free space where the trainer would go.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Space {
    pub free_gb: Option<f64>,
    pub drive: String,
    pub problem: Option<String>,
}

// ----- layout ----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn sd_scripts(&self) -> PathBuf {
        self.root.join("sd-scripts")
    }
    pub fn uv(&self) -> PathBuf {
        self.root.join("tools").join("uv.exe")
    }
    fn python_dir(&self) -> PathBuf {
        self.root.join("python")
    }
    fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    fn downloads(&self) -> PathBuf {
        self.root.join("downloads")
    }
    fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    fn marker(&self) -> PathBuf {
        self.root.join("installed.json")
    }
    fn venv_python(&self) -> PathBuf {
        let venv = self.sd_scripts().join(".venv");
        if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// What this version of the app installs. Tests substitute their own.
#[derive(Debug, Clone)]
pub struct Pins {
    pub trainer_url: String,
    pub trainer_commit: String,
    pub trainer_tree_sha256: String,
    pub uv_url: String,
    pub uv_sha256: String,
}

impl Pins {
    pub fn current() -> Self {
        Self {
            trainer_url: format!("{SD_SCRIPTS_URL}{TRAINER_COMMIT}"),
            trainer_commit: TRAINER_COMMIT.into(),
            trainer_tree_sha256: TRAINER_TREE_SHA256.into(),
            uv_url: UV_WHEEL_URL.into(),
            uv_sha256: UV_WHEEL_SHA256.into(),
        }
    }
}

/// What a finished setup records, so a later launch knows it is usable and
/// whether an app update changed the pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    trainer_commit: String,
    lock_sha256: String,
    uv_sha256: String,
    python: String,
}

impl Marker {
    fn of(pins: &Pins) -> Self {
        Self {
            trainer_commit: pins.trainer_commit.clone(),
            lock_sha256: hex(LOCK.as_bytes()),
            uv_sha256: pins.uv_sha256.clone(),
            python: PYTHON_VERSION.into(),
        }
    }
}

/// The trainer folder, if a finished setup under `layout` matches the pins of
/// this version of the app and its files are all still there.
pub fn ready_trainer_dir(layout: &Layout) -> Option<PathBuf> {
    let found: Marker = serde_json::from_slice(&fs::read(layout.marker()).ok()?).ok()?;
    (found == Marker::of(&Pins::current())
        && layout.sd_scripts().join("sdxl_train_network.py").is_file()
        && layout.venv_python().is_file())
    .then(|| layout.sd_scripts())
}

// ----- downloads -------------------------------------------------------------------

pub trait Fetch: Send + Sync {
    /// Download `url` into `dest`; returns the SHA-256 of what was written.
    /// `progress(done, total)` is called as bytes arrive. Stops when `cancel`
    /// is set.
    fn get(
        &self,
        url: &str,
        dest: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<String, String>;
}

pub struct HttpFetch;

impl Fetch for HttpFetch {
    fn get(
        &self,
        url: &str,
        dest: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<String, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(None)
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("Couldn't download {}: {}", host_of(url), plain(e)))?;
        let total = resp.content_length();
        let mut out = fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        let (mut hasher, mut done, mut buf) = (Sha256::new(), 0u64, vec![0u8; 256 * 1024]);
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("Cancelled.".into());
            }
            let n = resp
                .read(&mut buf)
                .map_err(|e| format!("The download from {} broke off: {e}", host_of(url)))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| format!("{}: {e}", dest.display()))?;
            done += n as u64;
            progress(done, total);
        }
        Ok(hex::encode(hasher.finalize()))
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "the server".into())
}

/// A reqwest error without its URL (nothing sensitive here, but short is kinder).
fn plain(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

// ----- archives ----------------------------------------------------------------------

/// Digest of a source archive's file contents and relative paths, ignoring
/// tar metadata: the same algorithm as `tree_digest` in
/// `instance/train_runner.py`, so one pin serves both.
pub fn tree_digest<R: Read>(gz: R) -> Result<String, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gz));
    let mut items: Vec<(String, String)> = Vec::new();
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let rel = name
            .split_once('/')
            .map_or(name.as_str(), |(_, r)| r)
            .to_string();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        items.push((rel, hex(&data)));
    }
    items.sort();
    let mut h = Sha256::new();
    for (rel, digest) in items {
        h.update(format!("{digest}  {rel}\n").as_bytes());
    }
    Ok(hex::encode(h.finalize()))
}

/// Unpack a source tarball into `dest`, dropping the top folder. Regular files
/// only, and nothing may land outside `dest`.
fn extract_tarball(gz: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(gz).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let mut parts = path.components();
        parts.next();
        let mut target = dest.to_path_buf();
        for c in parts {
            match c {
                std::path::Component::Normal(p) => target.push(p),
                _ => {
                    return Err(format!(
                        "Unsafe path in the trainer archive: {}",
                        path.display()
                    ))
                }
            }
        }
        if target == dest {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out =
            fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Pull `uv.exe` out of the uv wheel.
fn extract_uv(wheel: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(wheel).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("The uv download is not a wheel: {e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        if entry.name().ends_with("/scripts/uv.exe") {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("The uv download has no uv.exe in it.".into())
}

// ----- the installer -----------------------------------------------------------------

/// Free bytes on the drive holding a folder (`None` = unknown).
pub type FreeBytes = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

pub struct InstallDeps {
    /// Read on every use, so a changed install location takes effect at once.
    pub layout: Arc<dyn Fn() -> Layout + Send + Sync>,
    pub free_bytes: FreeBytes,
    pub pins: Pins,
    pub fetch: Arc<dyn Fetch>,
    pub exec: Arc<dyn Exec>,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

#[derive(Debug, Clone, Default)]
struct State {
    /// 0-based index into `STEPS`.
    step: usize,
    detail: String,
    /// Download progress of the current step, 0..=1.
    fraction: Option<f64>,
    finished_at: Option<u64>,
    outcome: Option<&'static str>,
    error: Option<String>,
}

struct Job {
    state: Mutex<State>,
    cancel: AtomicBool,
    started: u64,
}

/// A snapshot for the UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstallStatus {
    /// idle | running | finished
    pub state: String,
    pub stage: String,
    pub detail: String,
    /// 1-based step and how many there are.
    pub step: usize,
    pub steps: usize,
    pub fraction: Option<f64>,
    pub elapsed_secs: u64,
    /// completed | failed | cancelled
    pub outcome: Option<String>,
    pub error: Option<String>,
}

pub struct TrainerInstaller {
    deps: InstallDeps,
    job: Mutex<Option<Arc<Job>>>,
}

impl TrainerInstaller {
    pub fn new(deps: InstallDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            job: Mutex::new(None),
        })
    }

    pub fn layout(&self) -> Layout {
        (self.deps.layout)()
    }

    /// Free space where the trainer would go, for the setup panel.
    pub fn space(&self) -> Space {
        let root = self.layout().root;
        let free = (self.deps.free_bytes)(&root);
        Space {
            free_gb: free.map(|b| b as f64 / 1e9),
            drive: drive_label(&root),
            problem: space_problem(&root, free),
        }
    }

    pub fn is_running(&self) -> bool {
        self.job
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|j| j.state.lock().unwrap().outcome.is_none())
    }

    /// Start (or resume) the setup on its own thread.
    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        if self.is_running() {
            return Err("The trainer setup is already running.".into());
        }
        let layout = self.layout();
        check_root_usable(&layout.root)?;
        // A finished setup needs no space; anything else needs room for the
        // downloads and the environment.
        if ready_trainer_dir(&layout).is_none() {
            if let Some(problem) = self.space().problem {
                return Err(problem);
            }
        }
        let job = Arc::new(Job {
            state: Mutex::new(State::default()),
            cancel: AtomicBool::new(false),
            started: (self.deps.clock)(),
        });
        *self.job.lock().unwrap() = Some(job.clone());
        let me = self.clone();
        std::thread::spawn(move || {
            let result = me.run(&job);
            let mut st = job.state.lock().unwrap();
            st.finished_at = Some((me.deps.clock)());
            match result {
                Ok(()) => st.outcome = Some("completed"),
                Err(_) if job.cancel.load(Ordering::SeqCst) => st.outcome = Some("cancelled"),
                Err(e) => {
                    st.error = Some(e);
                    st.outcome = Some("failed");
                }
            }
        });
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(j) = self.job.lock().unwrap().as_ref() {
            j.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Forget a finished setup so the panel returns to its start.
    pub fn dismiss(&self) {
        if !self.is_running() {
            *self.job.lock().unwrap() = None;
        }
    }

    pub fn status(&self) -> InstallStatus {
        let Some(j) = self.job.lock().unwrap().clone() else {
            return InstallStatus {
                state: "idle".into(),
                stage: String::new(),
                detail: String::new(),
                step: 0,
                steps: STEPS.len(),
                fraction: None,
                elapsed_secs: 0,
                outcome: None,
                error: None,
            };
        };
        let st = j.state.lock().unwrap().clone();
        let end = st.finished_at.unwrap_or_else(|| (self.deps.clock)());
        InstallStatus {
            state: if st.outcome.is_some() {
                "finished"
            } else {
                "running"
            }
            .into(),
            stage: STEPS[st.step.min(STEPS.len() - 1)].into(),
            detail: st.detail,
            step: st.step + 1,
            steps: STEPS.len(),
            fraction: st.fraction,
            elapsed_secs: end.saturating_sub(j.started),
            outcome: st.outcome.map(str::to_string),
            error: st.error,
        }
    }

    fn set(&self, job: &Job, step: usize, detail: &str, fraction: Option<f64>) {
        let mut st = job.state.lock().unwrap();
        st.step = step;
        st.detail = detail.to_string();
        st.fraction = fraction;
    }

    fn check_cancel(&self, job: &Job) -> Result<(), String> {
        if job.cancel.load(Ordering::SeqCst) {
            Err("Cancelled.".into())
        } else {
            Ok(())
        }
    }

    fn run(&self, job: &Job) -> Result<(), String> {
        let layout = self.layout();
        let l = &layout;
        for d in [&l.root, &l.logs(), &l.downloads()] {
            fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        // A finished setup from other pins (an app update) is no longer "done".
        if fs::read(l.marker()).is_ok_and(|b| {
            serde_json::from_slice::<Marker>(&b).ok() != Some(Marker::of(&self.deps.pins))
        }) {
            let _ = fs::remove_file(l.marker());
        }
        self.step_trainer(job)?;
        self.check_cancel(job)?;
        self.step_uv(job)?;
        self.check_cancel(job)?;
        let lock = l.root.join("requirements.lock");
        fs::write(&lock, LOCK).map_err(|e| e.to_string())?;
        let uv = l.uv().to_string_lossy().into_owned();
        let py = l.venv_python().to_string_lossy().into_owned();
        let venv = l.sd_scripts().join(".venv").to_string_lossy().into_owned();
        let lock = lock.to_string_lossy().into_owned();
        let commands: [(usize, &str, &str, Vec<String>); 4] = [
            (
                2,
                "python",
                "Downloading Python",
                vec!["python".into(), "install".into(), PYTHON_VERSION.into()],
            ),
            (
                3,
                "venv",
                "Creating the environment",
                vec![
                    "venv".into(),
                    "--allow-existing".into(),
                    "--python".into(),
                    PYTHON_VERSION.into(),
                    venv,
                ],
            ),
            (
                4,
                "packages",
                "Downloading and installing about 4 GB of packages; this takes a while",
                vec![
                    "pip".into(),
                    "sync".into(),
                    "--python".into(),
                    py.clone(),
                    "--require-hashes".into(),
                    lock,
                    "--index".into(),
                    TORCH_INDEX.into(),
                    "--index".into(),
                    PYPI_INDEX.into(),
                    "--index-strategy".into(),
                    "unsafe-best-match".into(),
                ],
            ),
            (
                5,
                "check",
                "Checking the packages",
                vec!["pip".into(), "check".into(), "--python".into(), py.clone()],
            ),
        ];
        for (step, name, detail, args) in commands {
            self.check_cancel(job)?;
            self.set(job, step, detail, None);
            let cmd = TrainCommand {
                program: PathBuf::from(&uv),
                args,
                cwd: l.root.clone(),
                env: self.uv_env(),
            };
            self.run_logged(job, step, name, &cmd)?;
        }
        self.check_cancel(job)?;
        self.set(
            job,
            5,
            "Checking that PyTorch can see your NVIDIA GPU",
            None,
        );
        let probe = TrainCommand {
            program: PathBuf::from(&py),
            args: vec![
                "-c".into(),
                "import sys, torch; print(torch.__version__, torch.cuda.is_available()); \
                 sys.exit(0 if torch.cuda.is_available() else 3)"
                    .into(),
            ],
            cwd: l.root.clone(),
            env: self.uv_env(),
        };
        match self.run_logged(job, 5, "gpu-check", &probe) {
            Err(e) if e.contains("code 3") => {
                return Err("PyTorch installed, but it can't use an NVIDIA GPU. Local training needs an NVIDIA GPU and a recent driver.".into())
            }
            other => other?,
        }
        fs::write(
            l.marker(),
            serde_json::to_vec_pretty(&Marker::of(&self.deps.pins)).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        // The package cache and downloads are only needed during setup.
        let _ = fs::remove_dir_all(l.cache_dir());
        let _ = fs::remove_dir_all(l.downloads());
        Ok(())
    }

    fn uv_env(&self) -> Vec<(String, String)> {
        let layout = self.layout();
        let l = &layout;
        let p = |p: PathBuf| p.to_string_lossy().into_owned();
        vec![
            ("UV_CACHE_DIR".into(), p(l.cache_dir())),
            ("UV_PYTHON_INSTALL_DIR".into(), p(l.python_dir())),
            ("UV_PYTHON_PREFERENCE".into(), "only-managed".into()),
            // Keep the user's own uv settings out of it.
            ("UV_NO_CONFIG".into(), "1".into()),
            ("UV_LINK_MODE".into(), "copy".into()),
            ("PYTHONUTF8".into(), "1".into()),
            ("NO_COLOR".into(), "1".into()),
        ]
    }

    /// Run one command, logging to `logs/<name>.log` and showing the newest
    /// log line as the detail.
    fn run_logged(
        &self,
        job: &Job,
        step: usize,
        name: &str,
        cmd: &TrainCommand,
    ) -> Result<(), String> {
        let log = self.layout().logs().join(format!("{name}.log"));
        let mut tick = || {
            if let Some(line) = last_line(&log) {
                self.set(job, step, &line, None);
            }
        };
        let code = self.deps.exec.run(cmd, &log, &job.cancel, &mut tick)?;
        if job.cancel.load(Ordering::SeqCst) {
            return Err("Cancelled.".into());
        }
        if code != 0 {
            return Err(format!(
                "{name} failed with code {code}.\n{}",
                log_tail(&log)
            ));
        }
        Ok(())
    }

    fn step_trainer(&self, job: &Job) -> Result<(), String> {
        let layout = self.layout();
        let l = &layout;
        let pins = &self.deps.pins;
        let marker = l.sd_scripts().join(".sloptweak-commit");
        if fs::read_to_string(&marker).is_ok_and(|c| c.trim() == pins.trainer_commit) {
            return Ok(());
        }
        let archive = l.downloads().join("sd-scripts.tar.gz");
        self.set(job, 0, "Downloading the kohya trainer (6 MB)", Some(0.0));
        self.deps.fetch.get(
            &pins.trainer_url,
            &archive,
            &job.cancel,
            &mut |done, total| {
                self.set(
                    job,
                    0,
                    "Downloading the kohya trainer (6 MB)",
                    total.map(|t| done as f64 / t as f64),
                );
            },
        )?;
        self.set(
            job,
            0,
            "Checking the trainer against its pinned version",
            None,
        );
        let digest = tree_digest(fs::File::open(&archive).map_err(|e| e.to_string())?)?;
        if digest != pins.trainer_tree_sha256 {
            return Err(
                "The downloaded trainer doesn't match the version SlopTweak was built with, so it wasn't installed."
                    .into(),
            );
        }
        // Start clean: an older trainer (and its environment) is replaced.
        let _ = fs::remove_dir_all(l.sd_scripts());
        let partial = l.root.join("sd-scripts.partial");
        let _ = fs::remove_dir_all(&partial);
        fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
        extract_tarball(&archive, &partial)?;
        fs::write(partial.join(".sloptweak-commit"), &pins.trainer_commit)
            .map_err(|e| e.to_string())?;
        fs::rename(&partial, l.sd_scripts()).map_err(|e| format!("Couldn't place the trainer: {e}"))
    }

    fn step_uv(&self, job: &Job) -> Result<(), String> {
        let layout = self.layout();
        let l = &layout;
        let pins = &self.deps.pins;
        let marker = l.root.join("tools").join("uv.sha256");
        if l.uv().is_file() && fs::read_to_string(&marker).is_ok_and(|c| c.trim() == pins.uv_sha256)
        {
            return Ok(());
        }
        let wheel = l.downloads().join("uv.whl");
        self.set(job, 1, "Downloading uv (21 MB)", Some(0.0));
        let got = self
            .deps
            .fetch
            .get(&pins.uv_url, &wheel, &job.cancel, &mut |done, total| {
                self.set(
                    job,
                    1,
                    "Downloading uv (21 MB)",
                    total.map(|t| done as f64 / t as f64),
                );
            })?;
        if got != pins.uv_sha256 {
            return Err(
                "The downloaded package tool doesn't match its pinned hash, so it wasn't used."
                    .into(),
            );
        }
        extract_uv(&wheel, &l.uv())?;
        fs::write(&marker, &pins.uv_sha256).map_err(|e| e.to_string())
    }
}

fn log_tail(log: &Path) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let joined = lines[lines.len().saturating_sub(12)..].join("\n");
    let skip = joined.chars().count().saturating_sub(1500);
    joined.chars().skip(skip).collect()
}

fn last_line(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    let line = text
        .replace('\r', "\n")
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())?
        .trim()
        .to_string();
    Some(line.chars().take(160).collect())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::train::local::LocalSetup;

    fn targz(files: &[(&str, &str)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (name, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, body.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn wheel(exe: &[u8]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("uv-0.9.5.data/scripts/uvx.exe", opts)
            .unwrap();
        zip.write_all(b"not this one").unwrap();
        zip.start_file("uv-0.9.5.data/scripts/uv.exe", opts)
            .unwrap();
        zip.write_all(exe).unwrap();
        zip.finish().unwrap().into_inner()
    }

    const SOURCE: [(&str, &str); 3] = [
        ("sd-scripts-abc/sdxl_train_network.py", "print('train')"),
        ("sd-scripts-abc/library/util.py", "x = 1"),
        ("sd-scripts-abc/networks/lora.py", "y = 2"),
    ];

    #[test]
    fn the_tree_digest_follows_the_cloud_runners_algorithm() {
        let digest = tree_digest(targz(&SOURCE).as_slice()).unwrap();
        // sha256 over "<file sha256>  <relative path>\n", sorted by path.
        let mut lines: Vec<(String, String)> = SOURCE
            .iter()
            .map(|(n, b)| (n.split_once('/').unwrap().1.to_string(), hex(b.as_bytes())))
            .collect();
        lines.sort();
        let want: String = lines.iter().map(|(p, d)| format!("{d}  {p}\n")).collect();
        assert_eq!(digest, hex(want.as_bytes()));
        // Contents and paths matter; the top folder's name doesn't.
        let renamed: Vec<(String, &str)> = SOURCE
            .iter()
            .map(|(n, b)| (n.replace("sd-scripts-abc", "other"), *b))
            .collect();
        let renamed: Vec<(&str, &str)> = renamed.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        assert_eq!(tree_digest(targz(&renamed).as_slice()).unwrap(), digest);
        let edited = [
            ("t/sdxl_train_network.py", "print('evil')"),
            SOURCE[1],
            SOURCE[2],
        ];
        assert_ne!(tree_digest(targz(&edited).as_slice()).unwrap(), digest);
    }

    struct FakeFetch {
        files: HashMap<String, Vec<u8>>,
        calls: Mutex<Vec<String>>,
    }

    impl Fetch for FakeFetch {
        fn get(
            &self,
            url: &str,
            dest: &Path,
            _cancel: &AtomicBool,
            progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<String, String> {
            self.calls.lock().unwrap().push(url.to_string());
            let body = self.files.get(url).ok_or(format!("no fake for {url}"))?;
            fs::write(dest, body).map_err(|e| e.to_string())?;
            progress(body.len() as u64, Some(body.len() as u64));
            Ok(hex(body))
        }
    }

    #[derive(Default)]
    struct FakeExec {
        runs: Mutex<Vec<(String, Vec<String>)>>,
        /// Fails the command whose first two args are these.
        fail: Option<&'static str>,
        /// The exit code of the GPU probe.
        probe: i32,
        block_on: Option<&'static str>,
    }

    impl Exec for FakeExec {
        fn run(
            &self,
            cmd: &TrainCommand,
            log: &Path,
            cancel: &AtomicBool,
            _tick: &mut dyn FnMut(),
        ) -> Result<i32, String> {
            let name = cmd
                .program
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            self.runs
                .lock()
                .unwrap()
                .push((name.clone(), cmd.args.clone()));
            assert!(cmd.env.iter().any(|(k, v)| k == "UV_NO_CONFIG" && v == "1"));
            let key = cmd
                .args
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            if name.starts_with("python") {
                fs::write(log, "2.6.0+cu124 True\n").unwrap();
                return Ok(self.probe);
            }
            if self.block_on == Some(key.as_str()) {
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return Ok(-1);
            }
            if self.fail == Some(key.as_str()) {
                fs::write(log, "Resolving...\nerror: boom\n").unwrap();
                return Ok(1);
            }
            if cmd.args[0] == "venv" {
                let venv = Path::new(cmd.args.last().unwrap());
                let py = if cfg!(windows) {
                    "Scripts/python.exe"
                } else {
                    "bin/python"
                };
                fs::create_dir_all(venv.join(py).parent().unwrap()).unwrap();
                fs::write(venv.join(py), "").unwrap();
            }
            fs::write(log, "ok\n").unwrap();
            Ok(0)
        }
    }

    struct Rig {
        root: tempfile::TempDir,
        installer: Arc<TrainerInstaller>,
        fetch: Arc<FakeFetch>,
        exec: Arc<FakeExec>,
        pins: Pins,
    }

    const PLENTY: Option<u64> = Some(50_000_000_000);

    fn rig_with(source: &[(&str, &str)], wheel_bytes: &[u8], exec: FakeExec) -> Rig {
        rig_free(source, wheel_bytes, exec, PLENTY)
    }

    fn rig_free(
        source: &[(&str, &str)],
        wheel_bytes: &[u8],
        exec: FakeExec,
        free: Option<u64>,
    ) -> Rig {
        let root = tempfile::tempdir().unwrap();
        let (tar_bytes, whl) = (targz(source), wheel(wheel_bytes));
        let pins = Pins {
            trainer_url: "https://example.test/trainer.tar.gz".into(),
            trainer_commit: "abc123".into(),
            trainer_tree_sha256: tree_digest(targz(&SOURCE).as_slice()).unwrap(),
            uv_url: "https://example.test/uv.whl".into(),
            uv_sha256: hex(&wheel(b"UV-BINARY")),
        };
        let fetch = Arc::new(FakeFetch {
            files: HashMap::from([
                (pins.trainer_url.clone(), tar_bytes),
                (pins.uv_url.clone(), whl),
            ]),
            calls: Mutex::new(Vec::new()),
        });
        let exec = Arc::new(exec);
        let installer = TrainerInstaller::new(InstallDeps {
            layout: {
                let dir = root.path().join("trainer");
                Arc::new(move || Layout::new(dir.clone()))
            },
            free_bytes: Arc::new(move |_| free),
            pins: pins.clone(),
            fetch: fetch.clone(),
            exec: exec.clone(),
            clock: Arc::new(|| 1_000),
        });
        Rig {
            root,
            installer,
            fetch,
            exec,
            pins,
        }
    }

    fn rig(exec: FakeExec) -> Rig {
        rig_with(&SOURCE, b"UV-BINARY", exec)
    }

    fn finish(i: &Arc<TrainerInstaller>) -> InstallStatus {
        for _ in 0..500 {
            let s = i.status();
            if s.state == "finished" {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out: {:?}", i.status());
    }

    #[test]
    fn a_setup_downloads_checks_and_installs_in_order() {
        let r = rig(FakeExec::default());
        assert_eq!(r.installer.status().state, "idle");
        r.installer.start().unwrap();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);

        let l = r.installer.layout();
        assert_eq!(
            fs::read_to_string(l.sd_scripts().join("sdxl_train_network.py")).unwrap(),
            "print('train')"
        );
        assert!(l.sd_scripts().join("networks/lora.py").is_file());
        assert_eq!(fs::read(l.uv()).unwrap(), b"UV-BINARY");
        assert_eq!(
            fs::read_to_string(l.root.join("requirements.lock")).unwrap(),
            LOCK
        );
        let marker: Marker = serde_json::from_slice(&fs::read(l.marker()).unwrap()).unwrap();
        assert_eq!(marker, Marker::of(&r.pins));
        assert!(
            !l.cache_dir().exists() && !l.downloads().exists(),
            "scratch space is freed"
        );

        let runs = r.exec.runs.lock().unwrap();
        let cmds: Vec<String> = runs
            .iter()
            .map(|(p, a)| {
                format!(
                    "{p} {}",
                    a.iter().take(2).cloned().collect::<Vec<_>>().join(" ")
                )
            })
            .collect();
        assert_eq!(
            cmds,
            [
                "uv.exe python install",
                "uv.exe venv --allow-existing",
                "uv.exe pip sync",
                "uv.exe pip check",
                if cfg!(windows) {
                    "python.exe -c import sys, torch; print(torch.__version__, torch.cuda.is_available()); sys.exit(0 if torch.cuda.is_available() else 3)"
                } else {
                    "python -c import sys, torch; print(torch.__version__, torch.cuda.is_available()); sys.exit(0 if torch.cuda.is_available() else 3)"
                },
            ]
        );
        assert!(runs[0].1.contains(&PYTHON_VERSION.to_string()));
        let sync = &runs[2].1;
        assert!(sync.contains(&"--require-hashes".to_string()));
        assert!(sync.contains(&TORCH_INDEX.to_string()) && sync.contains(&PYPI_INDEX.to_string()));
        assert_eq!(s.step, STEPS.len());
    }

    #[test]
    fn a_second_setup_reuses_what_is_already_there() {
        let r = rig(FakeExec::default());
        r.installer.start().unwrap();
        finish(&r.installer);
        assert_eq!(r.fetch.calls.lock().unwrap().len(), 2);
        r.installer.dismiss();
        r.installer.start().unwrap();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);
        assert_eq!(
            r.fetch.calls.lock().unwrap().len(),
            2,
            "nothing is downloaded twice"
        );
    }

    #[test]
    fn a_trainer_that_isnt_the_pinned_one_is_refused() {
        let tampered = [
            ("t/sdxl_train_network.py", "import os; os.system('x')"),
            SOURCE[1],
            SOURCE[2],
        ];
        let r = rig_with(&tampered, b"UV-BINARY", FakeExec::default());
        r.installer.start().unwrap();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        assert!(s.error.unwrap().contains("doesn't match"));
        assert!(!r.installer.layout().sd_scripts().exists());
        assert!(r.exec.runs.lock().unwrap().is_empty(), "nothing was run");
    }

    #[test]
    fn a_package_tool_with_the_wrong_hash_is_not_used() {
        let r = rig_with(&SOURCE, b"SOMETHING-ELSE", FakeExec::default());
        r.installer.start().unwrap();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        assert!(s.error.unwrap().contains("pinned hash"));
        assert!(!r.installer.layout().uv().exists());
        assert!(r.exec.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_install_step_shows_the_tools_words_and_leaves_no_marker() {
        let r = rig(FakeExec {
            fail: Some("pip sync"),
            ..Default::default()
        });
        r.installer.start().unwrap();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        let e = s.error.unwrap();
        assert!(
            e.contains("packages failed with code 1") && e.contains("error: boom"),
            "{e}"
        );
        assert_eq!(s.stage, "Installing PyTorch and libraries");
        assert!(!r.installer.layout().marker().exists());
        assert!(ready_trainer_dir(&r.installer.layout()).is_none());
    }

    #[test]
    fn a_pc_without_a_usable_nvidia_gpu_is_told_so() {
        let r = rig(FakeExec {
            probe: 3,
            ..Default::default()
        });
        r.installer.start().unwrap();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        assert!(s.error.unwrap().contains("NVIDIA GPU"));
        assert!(!r.installer.layout().marker().exists());
    }

    #[test]
    fn cancelling_stops_the_setup_and_it_can_be_run_again() {
        let r = rig(FakeExec {
            block_on: Some("pip sync"),
            ..Default::default()
        });
        r.installer.start().unwrap();
        for _ in 0..500 {
            if r.installer.status().step == 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(r.installer.is_running());
        assert!(r.installer.start().unwrap_err().contains("already"));
        r.installer.cancel();
        let s = finish(&r.installer);
        assert_eq!(s.outcome.as_deref(), Some("cancelled"));
        assert!(s.error.is_none());
        assert!(!r.installer.layout().marker().exists());
    }

    #[test]
    fn readiness_needs_the_marker_the_files_and_matching_pins() {
        let root = tempfile::tempdir().unwrap();
        let l = Layout::new(root.path().join("t"));
        assert!(ready_trainer_dir(&l).is_none());
        fs::create_dir_all(l.sd_scripts()).unwrap();
        fs::write(l.sd_scripts().join("sdxl_train_network.py"), "").unwrap();
        fs::create_dir_all(l.venv_python().parent().unwrap()).unwrap();
        fs::write(l.venv_python(), "").unwrap();
        let write = |m: &Marker| fs::write(l.marker(), serde_json::to_vec(m).unwrap()).unwrap();
        write(&Marker::of(&Pins::current()));
        assert_eq!(ready_trainer_dir(&l), Some(l.sd_scripts()));

        // An app update that changes the trainer pin makes the old setup stale.
        write(&Marker {
            trainer_commit: "old".into(),
            ..Marker::of(&Pins::current())
        });
        assert!(ready_trainer_dir(&l).is_none());
        write(&Marker::of(&Pins::current()));
        fs::remove_file(l.venv_python()).unwrap();
        assert!(ready_trainer_dir(&l).is_none());
    }

    #[test]
    fn our_own_trainer_wins_over_an_old_manual_folder() {
        let root = tempfile::tempdir().unwrap();
        let l = Layout::new(root.path().join("t"));
        let manual = LocalSetup {
            trainer_dir: root.path().join("manual").to_string_lossy().into_owned(),
            base_model: "m.safetensors".into(),
            ..Default::default()
        };
        // Nothing set up by us: the manual folder is kept as it was.
        assert_eq!(manual.clone().resolved(&l), manual);
        fs::create_dir_all(l.sd_scripts()).unwrap();
        fs::write(l.sd_scripts().join("sdxl_train_network.py"), "").unwrap();
        fs::create_dir_all(l.venv_python().parent().unwrap()).unwrap();
        fs::write(l.venv_python(), "").unwrap();
        fs::write(
            l.marker(),
            serde_json::to_vec(&Marker::of(&Pins::current())).unwrap(),
        )
        .unwrap();
        let resolved = manual.resolved(&l);
        assert_eq!(resolved.trainer_dir, l.sd_scripts().to_string_lossy());
        assert_eq!(resolved.base_model, "m.safetensors");
        assert!(resolved.problems().iter().all(|p| !p.contains("set up")));
    }

    #[test]
    fn setup_wont_start_without_room_and_says_where() {
        let r = rig_free(
            &SOURCE,
            b"UV-BINARY",
            FakeExec::default(),
            Some(3_400_000_000),
        );
        let err = r.installer.start().unwrap_err();
        assert!(
            err.contains("3.4 GB is free") && err.contains("needs about 8 GB"),
            "{err}"
        );
        assert_eq!(r.installer.status().state, "idle");
        assert!(
            r.fetch.calls.lock().unwrap().is_empty(),
            "nothing was downloaded"
        );
        assert!(!r.installer.layout().root.exists(), "nothing was created");
        let space = r.installer.space();
        assert_eq!(space.free_gb, Some(3.4));
        assert!(space.problem.is_some());

        // Exactly enough is enough, and an unknown amount doesn't block.
        let ok = rig_free(
            &SOURCE,
            b"UV-BINARY",
            FakeExec::default(),
            Some(8_000_000_000),
        );
        ok.installer.start().unwrap();
        assert_eq!(finish(&ok.installer).outcome.as_deref(), Some("completed"));
        let unknown = rig_free(&SOURCE, b"UV-BINARY", FakeExec::default(), None);
        assert!(unknown.installer.space().problem.is_none());
    }

    #[test]
    fn the_space_message_names_the_drive_and_an_unknown_amount_never_blocks() {
        let root = tempfile::tempdir().unwrap();
        let low = space_problem(root.path(), Some(1));
        assert!(low.is_some() && low.unwrap().contains("choose a folder on another drive"));
        assert!(space_problem(root.path(), None).is_none());
        assert!(space_problem(root.path(), Some(8_000_000_000)).is_none());
    }

    #[test]
    fn setup_refuses_a_folder_with_other_files_in_it() {
        let r = rig(FakeExec::default());
        let root = r.installer.layout().root;
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("holiday.jpg"), "x").unwrap();
        let err = r.installer.start().unwrap_err();
        assert!(err.contains("holiday.jpg"), "{err}");
        assert!(
            root.join("holiday.jpg").is_file(),
            "nothing of theirs is touched"
        );
        assert!(r.fetch.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn the_drive_is_named_in_plain_words() {
        if cfg!(windows) {
            assert_eq!(drive_label(Path::new(r"D:\ai\SlopTweak-trainer")), "D:");
        }
        assert_eq!(drive_label(Path::new("relative")), "that drive");
    }

    #[cfg(windows)]
    #[test]
    fn the_real_free_space_call_works_even_for_a_folder_that_doesnt_exist_yet() {
        let d = tempfile::tempdir().unwrap();
        let here = free_space(d.path()).expect("free space of an existing folder");
        let later = free_space(&d.path().join("a").join("b")).expect("of the nearest parent");
        assert!(here > 1_000_000, "{here}");
        assert!(
            here.abs_diff(later) < 1_000_000_000,
            "same drive: {here} vs {later}"
        );
        assert!(
            free_space(Path::new(r"Q:\no\such\drive\x")).is_none() || Path::new(r"Q:\").exists()
        );
    }

    #[test]
    fn the_windows_lock_pins_every_package_with_hashes() {
        let pins: Vec<&str> = LOCK.lines().filter(|l| l.contains("==")).collect();
        assert!(pins.len() > 50);
        assert!(LOCK.contains("torch==2.6.0+cu124"));
        assert!(
            !LOCK.contains("\nnvidia-") && !LOCK.contains("\ntriton"),
            "Linux-only packages"
        );
        let hashes = LOCK.matches("--hash=sha256:").count();
        assert!(
            hashes >= pins.len(),
            "{hashes} hashes for {} pins",
            pins.len()
        );
    }

    /// Runs the real setup: downloads about 4.5 GB. Needs
    /// `SLOPTWEAK_TEST_TRAINER_ROOT` (an empty folder); run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore = "downloads about 4.5 GB and installs PyTorch"]
    fn real_trainer_setup() {
        let root = PathBuf::from(
            std::env::var("SLOPTWEAK_TEST_TRAINER_ROOT").expect("SLOPTWEAK_TEST_TRAINER_ROOT"),
        );
        let installer = TrainerInstaller::new(InstallDeps {
            layout: Arc::new(move || Layout::new(root.clone())),
            free_bytes: Arc::new(free_space),
            pins: Pins::current(),
            fetch: Arc::new(HttpFetch),
            exec: Arc::new(crate::train::local::ProcessExec),
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            }),
        });
        installer.start().unwrap();
        let started = std::time::Instant::now();
        let mut last = String::new();
        let s = loop {
            let s = installer.status();
            let line = format!("step {}/{} {} | {}", s.step, s.steps, s.stage, s.detail);
            if line != last && s.state == "running" {
                println!("[{:>4}s] {line}", started.elapsed().as_secs());
                last = line;
            }
            if s.state == "finished" || started.elapsed() > Duration::from_secs(3000) {
                break s;
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        println!("{s:#?}");
        assert_eq!(s.outcome.as_deref(), Some("completed"), "{:?}", s.error);
        let dir = ready_trainer_dir(&installer.layout()).expect("ready");
        println!("ready: {}", dir.display());
    }
}
