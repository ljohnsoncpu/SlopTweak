//! The Train tab's backend: quote a run (pick the cheapest suitable GPU and
//! show exactly what it could cost), start it only on the user's approval of
//! that exact quote, and report progress until the instance is destroyed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use super::commands::{self, Plan};
use super::dataset;
use super::job::{self, BaseModel, JobBundle};
use super::run::{
    offer_query, Approval, Limits, Live, Outcome, Phase, Plan as RunPlan, Progress, Quote,
    RemoteTrain, RunFile, RunResult, Runner,
};
use super::settings::{EvalPrompt, TrainSettings};
use crate::provider::machines::MachineFile;
use crate::provider::offers::{self, CostInputs, Tried};
use crate::provider::{attempt_label, GpuProvider, Offer};

/// A quote stays valid this long; after that the GPU price may have changed.
pub const QUOTE_TTL_SECS: u64 = 10 * 60;
/// Where the app's own estimate of a run's length starts: environment setup and
/// model download before the first training step.
const SETUP_MINUTES: u64 = 12;

pub type RemoteFactory =
    Arc<dyn Fn(String, PathBuf, Arc<JobBundle>) -> Arc<dyn RemoteTrain> + Send + Sync>;

pub struct Deps {
    pub provider: Arc<dyn GpuProvider>,
    /// Read when a run starts, so a key changed since launch is used.
    pub civitai_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Finished runs land in `<results_root>/<name>-<timestamp>/`.
    pub results_root: Arc<dyn Fn() -> PathBuf + Send + Sync>,
    /// Where the run record is kept (an unfinished one blocks a new run).
    pub record_dir: PathBuf,
    /// Where machine history is kept (a host that failed is skipped for a while).
    pub machines_dir: PathBuf,
    pub remote: RemoteFactory,
    pub poll: Duration,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub dir: PathBuf,
    pub settings: TrainSettings,
    pub prompts: Vec<EvalPrompt>,
    pub model: BaseModel,
    pub max_minutes: u64,
    pub max_usd: f64,
    /// Hourly price ceiling for the GPU search.
    pub max_dph: f64,
}

/// What the user is shown before approving. Starting needs this `plan_id`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteView {
    pub plan_id: String,
    pub gpu_name: String,
    pub gpu_location: Option<String>,
    pub hourly: f64,
    pub max_minutes: u64,
    pub max_usd: f64,
    /// The most this run can cost, if it runs to its limit.
    pub worst_usd: f64,
    /// A guess at what it will actually take, in minutes.
    pub expected_minutes: u64,
    pub plan: Plan,
    pub expires_unix: u64,
}

struct Pending {
    view: QuoteView,
    offer: Offer,
    limits: Limits,
    settings: TrainSettings,
    model: BaseModel,
    bundle: Arc<JobBundle>,
}

struct Active {
    live: Arc<Mutex<Live>>,
    cancel: Arc<AtomicBool>,
    done: Arc<Mutex<Option<RunResult>>>,
    started_unix: u64,
    results_dir: PathBuf,
    view: QuoteView,
    total_steps: u32,
    eval_images: usize,
}

/// A snapshot for the UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// idle | running | finished
    pub state: String,
    /// What it is doing in plain words.
    pub message: String,
    pub step: Option<u32>,
    pub total_steps: Option<u32>,
    pub eval_done: Option<u32>,
    pub eval_total: Option<u32>,
    pub gpu_name: Option<String>,
    pub hourly: Option<f64>,
    pub elapsed_secs: Option<u64>,
    pub max_minutes: Option<u64>,
    /// Estimated cost so far (hourly x elapsed), an upper-ish bound.
    pub spent_usd: Option<f64>,
    pub max_usd: Option<f64>,
    pub instance_id: Option<u64>,
    pub results_dir: Option<String>,
    pub synced: bool,
    /// Set once finished: completed | failed | deadline | cancelled
    pub outcome: Option<String>,
    pub error: Option<String>,
    /// The GPU is confirmed gone. False means the user must check Vast.
    pub destroyed: bool,
}

impl Status {
    pub fn idle() -> Self {
        Self {
            state: "idle".into(),
            message: String::new(),
            step: None,
            total_steps: None,
            eval_done: None,
            eval_total: None,
            gpu_name: None,
            hourly: None,
            elapsed_secs: None,
            max_minutes: None,
            spent_usd: None,
            max_usd: None,
            instance_id: None,
            results_dir: None,
            synced: false,
            outcome: None,
            error: None,
            destroyed: false,
        }
    }
}

pub struct TrainManager {
    deps: Deps,
    pending: Mutex<Option<Pending>>,
    active: Mutex<Option<Arc<Active>>>,
}

fn tokio_spawn_blocking_err(e: tokio::task::JoinError) -> String {
    format!("Internal error: {e}")
}

impl TrainManager {
    pub fn new(deps: Deps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            pending: Mutex::new(None),
            active: Mutex::new(None),
        })
    }

    fn now(&self) -> u64 {
        (self.deps.clock)()
    }

    pub fn is_running(&self) -> bool {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|a| a.done.lock().unwrap().is_none())
    }

    /// Check the dataset and settings, find the cheapest GPU that fits, and
    /// price the worst case. Rents nothing.
    pub async fn quote(&self, req: QuoteRequest) -> Result<QuoteView, String> {
        if self.is_running() {
            return Err("A training run is already in progress.".into());
        }
        let limits = Limits {
            max_seconds: req.max_minutes * 60,
            max_usd: req.max_usd,
            download_gb: job::download_gb(&req.model),
        };
        limits.validate().map_err(|e| e.0)?;

        // The dataset is read and hashed off the async threads.
        let (dir, settings, prompts, model) = (
            req.dir.clone(),
            req.settings.clone(),
            req.prompts.clone(),
            req.model.clone(),
        );
        let (report, bundle) = tokio::task::spawn_blocking(move || {
            let report = dataset::audit(&dir, settings.resolution)?;
            let bundle = job::build(&dir, &settings, &report, &model, &prompts)?;
            Ok::<_, String>((report, bundle))
        })
        .await
        .map_err(tokio_spawn_blocking_err)??;

        let query = offer_query(req.max_dph);
        let found = self
            .deps
            .provider
            .search_offers(&query)
            .await
            .map_err(|e| e.to_string())?;
        let inputs = CostInputs {
            expected_hours: (SETUP_MINUTES + 30) as f64 / 60.0,
            model_bytes: req.model.bytes,
            disk_gb: f64::from(job::TRAIN_DISK_GB),
        };
        let memory = MachineFile::new(&self.deps.machines_dir).load();
        let ranked = offers::rank(
            &found,
            &query,
            &inputs,
            &Tried::with_memory(memory, self.now()),
        );
        let (offer, quote) = ranked
            .iter()
            .map(|r| {
                (
                    r.offer.clone(),
                    Quote::new(&r.offer, job::TRAIN_DISK_GB, &limits),
                )
            })
            .find(|(_, q)| q.worst_usd <= limits.max_usd)
            .ok_or_else(|| {
                format!(
                    "No suitable GPU is available within ${:.2} for {} minutes. \
                     Raise the cap, shorten the limit, or try again later.",
                    limits.max_usd, req.max_minutes
                )
            })?;

        let images = report.images.len() as u32;
        let plan = commands::plan(&req.settings, images, req.prompts.len());
        let view = QuoteView {
            plan_id: crate::session::new_nonce(),
            gpu_name: offer.gpu_name.clone(),
            gpu_location: offer.geolocation.clone(),
            hourly: quote.hourly,
            max_minutes: req.max_minutes,
            max_usd: limits.max_usd,
            worst_usd: quote.worst_usd,
            expected_minutes: SETUP_MINUTES + expected_train_minutes(&plan),
            plan,
            expires_unix: self.now() + QUOTE_TTL_SECS,
        };
        *self.pending.lock().unwrap() = Some(Pending {
            view: view.clone(),
            offer,
            limits,
            settings: req.settings,
            model: req.model,
            bundle: Arc::new(bundle),
        });
        Ok(view)
    }

    /// The user approved this exact quote: rent the GPU and run.
    pub fn start(self: &Arc<Self>, plan_id: &str) -> Result<(), String> {
        if self.is_running() {
            return Err("A training run is already in progress.".into());
        }
        let pending = {
            let mut slot = self.pending.lock().unwrap();
            match slot.take() {
                Some(p) if p.view.plan_id == plan_id => p,
                other => {
                    *slot = other;
                    return Err("That quote is gone. Get a new one.".into());
                }
            }
        };
        if self.now() >= pending.view.expires_unix {
            return Err(
                "That quote expired, so the GPU price may have changed. Get a new one.".into(),
            );
        }

        let secret = crate::session::new_launch_secret();
        let hash = crate::session::sha256_hex(&secret);
        let label = attempt_label(&crate::session::new_nonce());
        let spec = job::launch_spec(
            &label,
            &hash,
            &pending.model,
            pending.limits.max_seconds,
            (self.deps.civitai_token)().as_deref(),
        )?;
        let stamp = self.now();
        let results_dir =
            (self.deps.results_root)().join(format!("{}-{stamp}", pending.settings.output_name));
        std::fs::create_dir_all(&results_dir)
            .map_err(|e| format!("Couldn't create the results folder: {e}"))?;

        let live = Arc::new(Mutex::new(Live::default()));
        let seen_live = live.clone();
        let machine = pending.offer.machine_id;
        let machines = MachineFile::new(&self.deps.machines_dir);
        let clock = self.deps.clock.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(Mutex::new(None));
        let runner = Runner {
            live: live.clone(),
            provider: self.deps.provider.clone(),
            remote: (self.deps.remote)(secret, results_dir.join("results"), pending.bundle.clone()),
            file: RunFile::new(&self.deps.record_dir.join("train_run.json")),
            clock: self.deps.clock.clone(),
            poll: self.deps.poll,
            cancel: cancel.clone(),
        };
        let plan = RunPlan {
            offer: pending.offer.clone(),
            spec,
            limits: pending.limits,
        };
        let approval = Approval {
            offer_id: pending.offer.id,
            max_seconds: pending.limits.max_seconds,
            max_usd: pending.limits.max_usd,
        };
        let active = Arc::new(Active {
            live,
            cancel,
            done: done.clone(),
            started_unix: stamp,
            results_dir,
            view: pending.view.clone(),
            total_steps: pending.bundle.total_steps,
            eval_images: pending.bundle.expected_eval_images,
        });
        *self.active.lock().unwrap() = Some(active);

        tokio::spawn(async move {
            let result = match runner.run(&plan, &approval).await {
                Ok(r) => r,
                Err(e) => RunResult {
                    outcome: Outcome::Failed(e.0),
                    record: super::run::RunRecord {
                        label: plan.spec.label.clone(),
                        phase: Phase::Destroyed,
                        instance_id: None,
                        offer_id: plan.offer.id,
                        hourly: 0.0,
                        started_unix: None,
                        synced: false,
                        outcome: None,
                    },
                },
            };
            // Remember how this host did: one that never got the run going is
            // skipped by later quotes; one that finished is preferred.
            if let Some(m) = machine {
                let never_started =
                    matches!(seen_live.lock().unwrap().progress, Progress::Starting);
                match &result.outcome {
                    Outcome::Completed => {
                        let _ = machines.record(m, true, clock());
                    }
                    Outcome::Failed(_) if never_started => {
                        let _ = machines.record(m, false, clock());
                    }
                    _ => {}
                }
            }
            *done.lock().unwrap() = Some(result);
        });
        Ok(())
    }

    /// Stop the run. The runner downloads what exists, then destroys the GPU.
    pub fn cancel(&self) {
        if let Some(a) = self.active.lock().unwrap().as_ref() {
            a.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Forget a finished run so the tab returns to its start.
    pub fn dismiss(&self) {
        if !self.is_running() {
            *self.active.lock().unwrap() = None;
        }
    }

    pub fn status(&self) -> Status {
        let Some(a) = self.active.lock().unwrap().clone() else {
            return Status::idle();
        };
        let live = a.live.lock().unwrap().clone();
        let result = a.done.lock().unwrap().clone();
        let elapsed = self.now().saturating_sub(a.started_unix);
        let rec = result.as_ref().map(|r| &r.record).or(live.record.as_ref());
        let mut s = Status {
            state: if result.is_some() {
                "finished"
            } else {
                "running"
            }
            .into(),
            gpu_name: Some(a.view.gpu_name.clone()),
            hourly: Some(a.view.hourly),
            elapsed_secs: Some(elapsed),
            max_minutes: Some(a.view.max_minutes),
            spent_usd: Some(spent(a.view.hourly, elapsed, a.view.max_minutes)),
            max_usd: Some(a.view.max_usd),
            instance_id: rec.and_then(|r| r.instance_id),
            results_dir: Some(a.results_dir.display().to_string()),
            synced: rec.is_some_and(|r| r.synced),
            destroyed: rec.is_some_and(|r| r.phase == Phase::Destroyed),
            total_steps: Some(a.total_steps),
            eval_total: Some(a.eval_images as u32),
            ..Status::idle()
        };
        match &live.progress {
            Progress::Training { step, total } => {
                s.step = Some(*step);
                s.total_steps = Some(*total);
            }
            Progress::Evaluating { done, total } => {
                s.step = Some(a.total_steps);
                s.eval_done = Some(*done);
                s.eval_total = Some(*total);
            }
            _ => {}
        }
        s.message = message(&live.progress, rec.map(|r| r.phase), result.as_ref());
        if let Some(r) = &result {
            let (outcome, error) = match &r.outcome {
                Outcome::Completed => ("completed", None),
                Outcome::Failed(e) => ("failed", Some(e.clone())),
                Outcome::DeadlineHit => ("deadline", None),
                Outcome::Cancelled => ("cancelled", None),
            };
            s.outcome = Some(outcome.into());
            s.error = error;
        }
        s
    }

    pub fn results_dir(&self) -> Option<PathBuf> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.results_dir.clone())
    }
}

fn spent(hourly: f64, elapsed_secs: u64, max_minutes: u64) -> f64 {
    hourly * elapsed_secs.min(max_minutes * 60) as f64 / 3600.0
}

/// Training takes about 0.95 s per step on the 24 GB class; evaluation about
/// 3.5 s per image. A rough guide shown before approval, never a limit.
fn expected_train_minutes(plan: &Plan) -> u64 {
    let secs = f64::from(plan.total_steps) * 0.95 + plan.eval_images as f64 * 3.5;
    (secs / 60.0).ceil() as u64
}

fn message(progress: &Progress, phase: Option<Phase>, result: Option<&RunResult>) -> String {
    if let Some(r) = result {
        return match (&r.outcome, r.record.phase) {
            (_, Phase::CleanupUnconfirmed) => {
                "The GPU may still be running. Check your Vast console and destroy it.".into()
            }
            (Outcome::Completed, _) if r.record.synced => "Done. The GPU is destroyed.".into(),
            (Outcome::Completed, _) => {
                "Training finished but the results didn't all download. The GPU is destroyed."
                    .into()
            }
            (Outcome::Cancelled, _) => "Stopped. The GPU is destroyed.".into(),
            (Outcome::DeadlineHit, _) => "Reached the time limit. The GPU is destroyed.".into(),
            (Outcome::Failed(e), _) => format!("{e} The GPU is destroyed."),
        };
    }
    match (phase, progress) {
        (Some(Phase::Syncing), _) => "Downloading results".into(),
        (Some(Phase::Destroying), _) => "Destroying the GPU".into(),
        (_, Progress::Training { .. }) => "Training".into(),
        (_, Progress::Evaluating { .. }) => "Making test images".into(),
        (Some(Phase::Created | Phase::Running), _) => "Setting up the GPU".into(),
        _ => "Renting a GPU".into(),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tokio::time::Instant;

    use super::*;
    use crate::provider::mock::{MockBehavior, MockProvider, MockTimings};

    struct Remote {
        progress: Mutex<Vec<Progress>>,
    }

    #[async_trait]
    impl RemoteTrain for Remote {
        async fn begin(&self, _: u64) -> Result<(), String> {
            Ok(())
        }
        async fn progress(&self, _: u64) -> Result<Progress, String> {
            let mut p = self.progress.lock().unwrap();
            Ok(if p.len() > 1 {
                p.remove(0)
            } else {
                p[0].clone()
            })
        }
        async fn sync(&self, _: u64) -> Result<bool, String> {
            Ok(true)
        }
    }

    struct Rig {
        mgr: Arc<TrainManager>,
        provider: MockProvider,
        data: tempfile::TempDir,
        out: tempfile::TempDir,
    }

    fn png(salt: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(1033u32.to_be_bytes());
        b.extend(1522u32.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, salt]);
        b
    }

    fn rig(progress: Vec<Progress>) -> Rig {
        rig_with(progress, vec![])
    }

    fn rig_with(progress: Vec<Progress>, behaviors: Vec<MockBehavior>) -> Rig {
        let data = tempfile::tempdir().unwrap();
        for i in 0..10u8 {
            std::fs::write(data.path().join(format!("i{i}.png")), png(i)).unwrap();
            std::fs::write(data.path().join(format!("i{i}.txt")), "trig, solo").unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(MockTimings::default(), behaviors);
        let t0 = Instant::now();
        let progress = Arc::new(progress);
        let mgr = TrainManager::new(Deps {
            provider: Arc::new(provider.clone()),
            civitai_token: Arc::new(|| Some("tok".into())),
            results_root: {
                let root = out.path().join("LoRAs");
                Arc::new(move || root.clone())
            },
            record_dir: out.path().join("data"),
            machines_dir: out.path().join("data"),
            remote: Arc::new(move |_secret, _dir, _job| {
                Arc::new(Remote {
                    progress: Mutex::new((*progress).clone()),
                })
            }),
            poll: Duration::from_secs(10),
            clock: Arc::new(move || 1_000_000 + t0.elapsed().as_secs()),
        });
        Rig {
            mgr,
            provider,
            data,
            out,
        }
    }

    fn request(r: &Rig) -> QuoteRequest {
        QuoteRequest {
            dir: r.data.path().into(),
            settings: TrainSettings {
                output_name: "lora".into(),
                ..Default::default()
            },
            prompts: commands::default_prompts("trig"),
            model: BaseModel {
                file: "m.safetensors".into(),
                sha256: "ab".repeat(32),
                bytes: 6_938_043_446,
                url: "https://civitai.com/api/download/models/1".into(),
                requires_civitai_token: true,
            },
            max_minutes: 120,
            max_usd: 2.0,
            max_dph: 1.0,
        }
    }

    async fn finish(m: &Arc<TrainManager>) -> Status {
        for _ in 0..10_000 {
            let s = m.status();
            if s.state == "finished" {
                return s;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        panic!("the run never finished: {:?}", m.status());
    }

    #[tokio::test(start_paused = true)]
    async fn a_quote_names_the_gpu_price_and_worst_case() {
        let r = rig(vec![Progress::Done]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        // The only mock offer with 23+ GB.
        assert_eq!(q.gpu_name, "RTX A6000");
        let hourly = 0.5420 + 0.2 * 80.0 / 720.0;
        assert!((q.hourly - hourly).abs() < 1e-9);
        assert!((q.worst_usd - hourly * 2.0).abs() < 1e-9);
        assert_eq!(q.plan.total_steps, 400);
        assert_eq!(q.plan.images, 10);
        assert!(q.expected_minutes >= SETUP_MINUTES);
        // Quoting rents nothing.
        assert!(r.provider.created_offers().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_cap_nothing_fits_under_is_explained() {
        let r = rig(vec![Progress::Done]);
        let mut req = request(&r);
        req.max_usd = 0.10;
        let e = r.mgr.quote(req).await.unwrap_err();
        assert!(e.contains("No suitable GPU"), "{e}");
        let mut req = request(&r);
        req.max_dph = 0.2;
        assert!(r
            .mgr
            .quote(req)
            .await
            .unwrap_err()
            .contains("No suitable GPU"));
        assert!(r.provider.created_offers().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_bad_dataset_or_limit_stops_at_the_quote() {
        let r = rig(vec![Progress::Done]);
        std::fs::remove_file(r.data.path().join("i0.txt")).unwrap();
        assert!(r.mgr.quote(request(&r)).await.is_err());
        let r = rig(vec![Progress::Done]);
        let mut req = request(&r);
        req.max_minutes = 24 * 60;
        assert!(r.mgr.quote(req).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn start_needs_the_exact_live_quote() {
        let r = rig(vec![Progress::Done]);
        assert!(r.mgr.start("nope").is_err(), "no quote yet");
        let q = r.mgr.quote(request(&r)).await.unwrap();
        assert!(r.mgr.start("wrong-id").is_err());
        // A wrong id leaves the real quote usable until it expires.
        tokio::time::sleep(Duration::from_secs(QUOTE_TTL_SECS + 1)).await;
        let e = r.mgr.start(&q.plan_id).unwrap_err();
        assert!(e.contains("expired"), "{e}");
        assert!(r.provider.created_offers().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_trains_downloads_and_destroys() {
        let r = rig(vec![
            Progress::Starting,
            Progress::Training {
                step: 100,
                total: 400,
            },
            Progress::Evaluating {
                done: 10,
                total: 52,
            },
            Progress::Done,
        ]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        assert!(r.mgr.is_running());
        assert!(r.mgr.quote(request(&r)).await.is_err(), "one run at a time");
        assert!(r.mgr.start(&q.plan_id).is_err());

        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("completed"));
        assert!(s.synced && s.destroyed);
        assert_eq!(s.gpu_name.as_deref(), Some("RTX A6000"));
        assert!(s.message.contains("destroyed"), "{}", s.message);
        assert!(r.provider.live_ids().is_empty());
        assert_eq!(r.provider.created_offers(), vec![105]);
        let dir = r.mgr.results_dir().unwrap();
        assert!(dir.starts_with(r.out.path().join("LoRAs")));
        assert!(dir.is_dir());

        r.mgr.dismiss();
        assert_eq!(r.mgr.status().state, "idle");
        // A fresh quote works again, and the record no longer blocks it.
        assert!(r.mgr.quote(request(&r)).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn status_follows_progress_while_running() {
        let r = rig(vec![Progress::Training {
            step: 100,
            total: 400,
        }]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        // Wait for the mock GPU to come up and report.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if r.mgr.status().step == Some(100) {
                break;
            }
        }
        let s = r.mgr.status();
        assert_eq!(
            (s.state.as_str(), s.step, s.total_steps),
            ("running", Some(100), Some(400))
        );
        assert_eq!(s.message, "Training");
        assert!(s.spent_usd.unwrap() <= s.max_usd.unwrap());
        r.mgr.cancel();
        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("cancelled"));
        assert!(s.destroyed);
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_run_says_so_and_still_destroys() {
        let r = rig(vec![Progress::Failed("CUDA out of memory.".into())]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        assert_eq!(s.error.as_deref(), Some("CUDA out of memory."));
        assert!(s.message.contains("CUDA out of memory"));
        assert!(r.provider.live_ids().is_empty());
    }

    #[test]
    fn spend_is_capped_at_the_limit() {
        assert!((spent(0.6, 1800, 120) - 0.3).abs() < 1e-9);
        assert!((spent(0.6, 99_999, 60) - 0.6).abs() < 1e-9);
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_never_started_is_skipped_by_the_next_quote() {
        let r = rig_with(vec![Progress::Done], vec![MockBehavior::DeadHost]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        assert!(r.provider.live_ids().is_empty());
        r.mgr.dismiss();
        // The mock has one qualifying machine, and it just failed.
        let e = r.mgr.quote(request(&r)).await.unwrap_err();
        assert!(e.contains("No suitable GPU"), "{e}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_finished_stays_eligible() {
        let r = rig(vec![Progress::Done]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        finish(&r.mgr).await;
        r.mgr.dismiss();
        assert!(r.mgr.quote(request(&r)).await.is_ok());
    }
}
