//! The Train tab's backend: quote a run (pick the cheapest suitable GPU and
//! show exactly what it could cost), start it only on the user's approval of
//! that exact quote, and report progress until the instance is destroyed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use super::commands::{self, Plan};
use super::dataset;
use super::diag::{self, StageView};
use super::job::{self, BaseModel, JobBundle};
use super::moss::{self, MossSettings};
use super::run::{
    destroy_all, moss_offer_query, offer_query, Approval, Limits, Live, Outcome, Phase,
    Plan as RunPlan, Progress, Quote, RemoteTrain, RunFile, RunRecord, RunResult, Runner,
    STUCK_SETUP_BLAME_SECS,
};
use super::settings::{EvalPrompt, TrainSettings};
use crate::provider::machines::MachineFile;
use crate::provider::offers::{self, CostInputs, Tried};
use crate::provider::{attempt_label, GpuProvider, Offer, OfferQuery};

/// A quote stays valid this long; after that the GPU price may have changed.
pub const QUOTE_TTL_SECS: u64 = 10 * 60;
/// How long closing the app waits for a stopping run to download what exists
/// and destroy its GPU. Past this the app stays open so the user can retry.
pub const STOP_BUDGET: Duration = Duration::from_secs(5 * 60);
/// Where the app's own estimate of a run's length starts: environment setup and
/// model download before the first training step.
const SETUP_MINUTES: u64 = 12;
/// The same for Kromoss: image pull and boot, the 15 GB of weights, the
/// trainer's environment, the caches and the first-run kernel tuning (about
/// 24 min on the 2026-10-27 runs).
const MOSS_SETUP_MINUTES: u64 = 25;
/// Measured seconds per step: an RTX 4090 with INT8_COMPUTE, and an RTX 3090
/// (also the guess for any other card, so the estimate errs long).
const MOSS_STEP_SECS_4090: f64 = 2.0;
const MOSS_STEP_SECS_OTHER: f64 = 5.7;

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

/// Which trainer a run uses, with everything only that trainer needs.
#[derive(Debug, Clone)]
pub enum Trainer {
    /// kohya sd-scripts on an SDXL-family checkpoint.
    Kohya {
        settings: TrainSettings,
        prompts: Vec<EvalPrompt>,
        model: BaseModel,
    },
    /// KohakuMossTrainer on Kromoss; the weights are pinned in `moss.rs`.
    Kromoss { settings: MossSettings },
}

impl Trainer {
    /// What the UI calls it.
    fn kind(&self) -> &'static str {
        match self {
            Self::Kohya { .. } => "sdxl",
            Self::Kromoss { .. } => "kromoss",
        }
    }

    fn output_name(&self) -> &str {
        match self {
            Self::Kohya { settings, .. } => &settings.output_name,
            Self::Kromoss { settings } => &settings.output_name,
        }
    }

    fn download_gb(&self) -> f64 {
        match self {
            Self::Kohya { model, .. } => job::download_gb(model),
            Self::Kromoss { .. } => job::moss_download_gb(),
        }
    }

    fn disk_gb(&self) -> u32 {
        match self {
            Self::Kohya { .. } => job::TRAIN_DISK_GB,
            Self::Kromoss { .. } => job::MOSS_DISK_GB,
        }
    }

    fn model_bytes(&self) -> u64 {
        match self {
            Self::Kohya { model, .. } => model.bytes,
            Self::Kromoss { .. } => moss::MOSS_FILES.iter().map(|f| f.bytes).sum(),
        }
    }

    fn offer_query(&self, max_dph: f64) -> OfferQuery {
        match self {
            Self::Kohya { .. } => offer_query(max_dph),
            Self::Kromoss { .. } => moss_offer_query(max_dph),
        }
    }

    /// Hours a run is expected to occupy a GPU, for ranking offers by cost.
    fn rank_hours(&self, plan: &Plan) -> f64 {
        match self {
            Self::Kohya { .. } => (SETUP_MINUTES + 30) as f64 / 60.0,
            // The slower measured rate, since the card isn't chosen yet.
            Self::Kromoss { .. } => {
                (MOSS_SETUP_MINUTES as f64 * 60.0
                    + f64::from(plan.total_steps) * MOSS_STEP_SECS_OTHER)
                    / 3600.0
            }
        }
    }

    /// A rough guide shown before approval, never a limit.
    fn expected_minutes(&self, plan: &Plan, gpu: &str) -> u64 {
        match self {
            Self::Kohya { .. } => SETUP_MINUTES + expected_train_minutes(plan),
            Self::Kromoss { .. } => {
                let per_step = if gpu.contains("4090") {
                    MOSS_STEP_SECS_4090
                } else {
                    MOSS_STEP_SECS_OTHER
                };
                MOSS_SETUP_MINUTES + (f64::from(plan.total_steps) * per_step / 60.0).ceil() as u64
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub dir: PathBuf,
    pub trainer: Trainer,
    pub max_minutes: u64,
    pub max_usd: f64,
    /// Hourly price ceiling for the GPU search.
    pub max_dph: f64,
    /// Only offer cards whose name contains this (a model such as `4090`, in any
    /// case); `None` = the cheapest card that fits. Checked by [`gpu_matches`].
    pub gpu_model: Option<String>,
}

/// Longest model preference accepted.
pub const MAX_GPU_MODEL_CHARS: usize = 24;

/// A model preference as typed: trimmed, plain characters only, `None` when blank.
pub fn clean_gpu_model(raw: Option<&str>) -> Result<Option<String>, String> {
    let m = raw.map_or("", str::trim);
    if m.is_empty() {
        return Ok(None);
    }
    if m.chars().count() > MAX_GPU_MODEL_CHARS
        || !m
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
    {
        return Err("The GPU model must be a short name such as 4090.".into());
    }
    Ok(Some(m.to_string()))
}

/// Whether a card's name contains the model, ignoring case and spacing.
pub fn gpu_matches(gpu_name: &str, model: &str) -> bool {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    };
    squash(gpu_name).contains(&squash(model))
}

/// What the user is shown before approving. Starting needs this `plan_id`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteView {
    pub plan_id: String,
    /// `sdxl` or `kromoss`.
    pub trainer: String,
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
    trainer: Trainer,
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
    /// What was done about the machine once the run ended.
    host_note: Arc<Mutex<Option<String>>>,
    /// Where the failure report was saved, if one was.
    diagnostics: Arc<Mutex<Option<String>>>,
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
    /// While the GPU is being set up: the step it reports, the end of its log,
    /// and how long nothing has changed.
    pub setup_detail: Option<String>,
    pub setup_log: Option<String>,
    pub setup_quiet_secs: Option<u64>,
    /// When each stage began and how long it took.
    pub stages: Vec<StageView>,
    /// What Vast said while starting the machine, with run-relative times.
    pub host_status: Vec<String>,
    /// The end of the container's own output, fetched from Vast.
    pub container_log: Option<String>,
    /// How fresh that is, or why it isn't there.
    pub container_log_note: Option<String>,
    /// What was done about the machine once the run ended.
    pub host_note: Option<String>,
    /// Where the failure report was saved.
    pub diagnostics_path: Option<String>,
    /// Trained on this PC's own GPU: nothing rented, so no price or limits.
    pub local: bool,
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
            setup_detail: None,
            setup_log: None,
            setup_quiet_secs: None,
            stages: Vec::new(),
            host_status: Vec::new(),
            container_log: None,
            container_log_note: None,
            host_note: None,
            diagnostics_path: None,
            local: false,
        }
    }
}

/// How an instance relates to the training run record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// The GPU of the run in progress.
    Running,
    /// A GPU an earlier run left behind that may still bill.
    Leftover,
    Other,
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

    fn run_file(&self) -> RunFile {
        RunFile::new(&self.deps.record_dir.join("train_run.json"))
    }

    /// The record of the run in progress, if one is.
    fn running_record(&self) -> Option<RunRecord> {
        let a = self.active.lock().unwrap().clone()?;
        if a.done.lock().unwrap().is_some() {
            return None;
        }
        let rec = a.live.lock().unwrap().record.clone();
        rec
    }

    /// A run record that says a GPU may be billing, left by a run that is not
    /// in progress (the app was killed or crashed).
    fn leftover_record(&self) -> Option<RunRecord> {
        if self.is_running() {
            return None;
        }
        self.run_file().load().filter(|r| !r.phase.is_finished())
    }

    /// Whose is this instance? The run in progress is not an orphan, and a
    /// leftover one needs the user to shut it down.
    pub fn ownership(&self, id: u64, label: Option<&str>) -> Ownership {
        let matches = |r: &RunRecord| r.instance_id == Some(id) || label == Some(r.label.as_str());
        if self.running_record().is_some_and(|r| matches(&r)) {
            Ownership::Running
        } else if self.leftover_record().is_some_and(|r| matches(&r)) {
            Ownership::Leftover
        } else {
            Ownership::Other
        }
    }

    /// A leftover record whose GPU is no longer listed has nothing billing:
    /// close it, so it stops blocking new runs.
    pub async fn settle_leftover(&self) {
        let Some(mut rec) = self.leftover_record() else {
            return;
        };
        let Ok(list) = self.deps.provider.list_instances().await else {
            return;
        };
        let listed = list
            .iter()
            .any(|i| rec.instance_id == Some(i.id) || i.label.as_deref() == Some(&rec.label));
        if !listed {
            rec.phase = Phase::Destroyed;
            rec.outcome = Some("The GPU was already gone when SlopTweak reopened.".into());
            let _ = self.run_file().save(&rec);
        }
    }

    /// Shut down a GPU left by an earlier run, and confirm it is gone.
    pub async fn destroy_leftover(&self, id: u64, label: Option<&str>) -> Result<(), String> {
        if self.is_running() {
            return Err("A training run is in progress. Stop it first.".into());
        }
        if !destroy_all(self.deps.provider.as_ref(), &[id], self.deps.poll).await {
            return Err(format!(
                "Couldn't confirm GPU {id} was shut down. Check the Vast console."
            ));
        }
        if self.ownership(id, label) == Ownership::Leftover {
            if let Some(mut rec) = self.leftover_record() {
                rec.phase = Phase::Destroyed;
                rec.outcome = Some("Shut down after the app stopped watching it.".into());
                let _ = self.run_file().save(&rec);
            }
        }
        Ok(())
    }

    /// Stop the run (it downloads what exists, then destroys the GPU) and wait
    /// for that. `true` only when no run is left and its GPU is confirmed gone.
    pub async fn stop_and_wait(&self, limit: Duration) -> bool {
        self.cancel();
        let deadline = Instant::now() + limit;
        while self.is_running() {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let active = self.active.lock().unwrap().clone();
        let done = active.and_then(|a| a.done.lock().unwrap().clone());
        done.is_none_or(|r| r.record.phase != Phase::CleanupUnconfirmed)
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
            download_gb: req.trainer.download_gb(),
        };
        limits.validate().map_err(|e| e.0)?;

        // The dataset is read and hashed off the async threads.
        let (dir, trainer) = (req.dir.clone(), req.trainer.clone());
        let (plan, bundle) = tokio::task::spawn_blocking(move || match &trainer {
            Trainer::Kohya {
                settings,
                prompts,
                model,
            } => {
                let report = dataset::audit(&dir, settings.resolution)?;
                let bundle = job::build(&dir, settings, &report, model, prompts)?;
                let images = report.images.len() as u32;
                Ok::<_, String>((commands::plan(settings, images, prompts.len()), bundle))
            }
            Trainer::Kromoss { settings } => {
                let images = moss::collect(&dir, settings.caption_mode)?;
                let bundle = job::build_moss(&dir, settings, &images)?;
                Ok((commands::moss_plan(settings, images.len() as u32), bundle))
            }
        })
        .await
        .map_err(tokio_spawn_blocking_err)??;

        let gpu_model = clean_gpu_model(req.gpu_model.as_deref())?;
        let query = req.trainer.offer_query(req.max_dph);
        let mut found = self
            .deps
            .provider
            .search_offers(&query)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(model) = &gpu_model {
            found.retain(|o| gpu_matches(&o.gpu_name, model));
            if found.is_empty() {
                return Err(format!(
                    "No {model} is available right now that fits this run. Try again later \
                     or choose the cheapest GPU."
                ));
            }
        }
        let inputs = CostInputs {
            expected_hours: req.trainer.rank_hours(&plan),
            model_bytes: req.trainer.model_bytes(),
            disk_gb: f64::from(req.trainer.disk_gb()),
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
                    Quote::new(&r.offer, req.trainer.disk_gb(), &limits),
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

        let view = QuoteView {
            plan_id: crate::session::new_nonce(),
            trainer: req.trainer.kind().into(),
            gpu_name: offer.gpu_name.clone(),
            gpu_location: offer.geolocation.clone(),
            hourly: quote.hourly,
            max_minutes: req.max_minutes,
            max_usd: limits.max_usd,
            worst_usd: quote.worst_usd,
            expected_minutes: req.trainer.expected_minutes(&plan, &offer.gpu_name),
            plan,
            expires_unix: self.now() + QUOTE_TTL_SECS,
        };
        *self.pending.lock().unwrap() = Some(Pending {
            view: view.clone(),
            offer,
            limits,
            trainer: req.trainer,
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
        // Everything fetched from the instance is masked against these.
        let mut secrets = vec![secret.clone()];
        let label = attempt_label(&crate::session::new_nonce());
        let spec = match &pending.trainer {
            Trainer::Kohya { model, .. } => {
                secrets.extend((self.deps.civitai_token)());
                job::launch_spec(
                    &label,
                    &hash,
                    model,
                    pending.limits.max_seconds,
                    (self.deps.civitai_token)().as_deref(),
                )?
            }
            // The Kromoss weights are public: no key goes to the instance.
            Trainer::Kromoss { .. } => {
                job::moss_launch_spec(&label, &hash, pending.limits.max_seconds)?
            }
        };
        let stamp = self.now();
        let results_dir =
            (self.deps.results_root)().join(format!("{}-{stamp}", pending.trainer.output_name()));
        std::fs::create_dir_all(&results_dir)
            .map_err(|e| format!("Couldn't create the results folder: {e}"))?;

        let live = Arc::new(Mutex::new(Live::default()));
        let seen_live = live.clone();
        let results_for_report = results_dir.clone();
        let machine = pending.offer.machine_id;
        let gpu = pending.view.gpu_name.clone();
        let host_note = Arc::new(Mutex::new(None));
        let diagnostics = Arc::new(Mutex::new(None));
        let (note_slot, diag_slot) = (host_note.clone(), diagnostics.clone());
        let known = secrets.clone();
        let machines = MachineFile::new(&self.deps.machines_dir);
        let clock = self.deps.clock.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(Mutex::new(None));
        let runner = Runner {
            live: live.clone(),
            provider: self.deps.provider.clone(),
            remote: (self.deps.remote)(secret, results_dir.join("results"), pending.bundle.clone()),
            file: self.run_file(),
            clock: self.deps.clock.clone(),
            poll: self.deps.poll,
            cancel: cancel.clone(),
            secrets,
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
            host_note,
            diagnostics,
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
            // skipped by later quotes; one that finished is preferred. Say
            // what was decided, either way.
            let now = clock();
            let ran_secs = now.saturating_sub(stamp);
            let never_started = matches!(seen_live.lock().unwrap().progress, Progress::Starting);
            // No instance means the host was never involved (the offer was taken
            // or the rental was refused): that says nothing about the machine.
            let created = result.record.instance_id.is_some();
            let verdict = created
                .then(|| host_verdict(&result.outcome, never_started, ran_secs))
                .flatten();
            let mut saved = Ok(());
            if let (Some(m), Some(ok)) = (machine, verdict) {
                let reason = blame_reason(&result.outcome, ran_secs);
                saved = machines.record_with(m, ok, now, Some(&gpu), Some(&reason));
            }
            let note = host_feedback(
                machine,
                &gpu,
                verdict,
                created,
                never_started,
                ran_secs,
                saved.err().map(|e| e.to_string()),
            );
            *note_slot.lock().unwrap() = Some(note.clone());
            if let Ok(mut live) = seen_live.lock() {
                live.stages.push(super::run::Timed {
                    at_unix: now,
                    text: diag::END_MARK.into(),
                });
            }
            if result.outcome != Outcome::Completed {
                let live = seen_live.lock().unwrap().clone();
                let known: Vec<&str> = known.iter().map(String::as_str).collect();
                let text = diag::render(
                    &diag::Report {
                        label: &result.record.label,
                        instance_id: result.record.instance_id,
                        machine_id: machine,
                        gpu: &gpu,
                        result: &outcome_text(&result.outcome),
                        run_start: stamp,
                        ended: now,
                        host_note: &note,
                        live: &live,
                    },
                    &known,
                );
                match diag::write(&results_for_report, &text) {
                    Ok(p) => *diag_slot.lock().unwrap() = Some(p.display().to_string()),
                    Err(e) => log::warn!("could not save the training diagnostics: {e}"),
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
        if result.is_none() {
            if let Some(n) = &live.setup {
                s.setup_detail = n.detail.clone();
                s.setup_log = Some(n.log_tail.trim().to_string()).filter(|t| !t.is_empty());
                s.setup_quiet_secs = live.setup_quiet_since.map(|q| self.now().saturating_sub(q));
            }
        }
        let now = self.now();
        s.stages = diag::stage_views(&live.stages, a.started_unix, now);
        s.host_status = live
            .host_status
            .iter()
            .rev()
            .take(40)
            .rev()
            .map(|h| {
                format!(
                    "{}  {}",
                    diag::offset(h.at_unix.saturating_sub(a.started_unix)),
                    h.text
                )
            })
            .collect();
        s.container_log = Some(live.container_log.trim().to_string()).filter(|t| !t.is_empty());
        s.container_log_note = match (&live.container_log_error, live.container_log_unix) {
            (Some(e), _) => Some(format!("Couldn't fetch the machine's log: {e}")),
            (None, Some(u)) => Some(format!(
                "Machine log, refreshed {}s ago (about every 15 s while the GPU exists)",
                now.saturating_sub(u)
            )),
            (None, None) => Some("The machine log isn't available yet.".into()),
        }
        .filter(|_| result.is_none() || s.container_log.is_some());
        s.host_note = a.host_note.lock().unwrap().clone();
        s.diagnostics_path = a.diagnostics.lock().unwrap().clone();
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

/// What a finished run says about its host: `Some(true)` worked, `Some(false)`
/// never got the run going, `None` says nothing (a crash mid-training, or a
/// quick cancel). A run the user stops or that hits its limit while still in
/// setup counts against the host once it had run [`STUCK_SETUP_BLAME_SECS`].
fn host_verdict(outcome: &Outcome, never_started: bool, ran_secs: u64) -> Option<bool> {
    match outcome {
        Outcome::Completed => Some(true),
        Outcome::Failed(_) if never_started => Some(false),
        Outcome::Cancelled | Outcome::DeadlineHit
            if never_started && ran_secs >= STUCK_SETUP_BLAME_SECS =>
        {
            Some(false)
        }
        _ => None,
    }
}

/// Why a machine was blamed, for the Settings list.
fn blame_reason(outcome: &Outcome, ran_secs: u64) -> String {
    match outcome {
        Outcome::Failed(why) => why.clone(),
        _ => format!(
            "Setup hadn't finished after {} min and the run was stopped",
            ran_secs / 60
        ),
    }
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed => "Completed".into(),
        Outcome::Failed(e) => format!("Failed: {e}"),
        Outcome::DeadlineHit => "Stopped at the time limit".into(),
        Outcome::Cancelled => "Stopped by you".into(),
    }
}

/// Tell the user what a run's end did to the machine's record.
fn host_feedback(
    machine: Option<u64>,
    gpu: &str,
    verdict: Option<bool>,
    created: bool,
    never_started: bool,
    ran_secs: u64,
    save_error: Option<String>,
) -> String {
    let Some(m) = machine else {
        return "Vast didn't say which physical machine this was, so it can't be remembered \
                or skipped later."
            .into();
    };
    let who = format!("Machine {m} ({gpu})");
    let mut text = match verdict {
        Some(true) => {
            format!("{who} worked. It will be preferred next time when it costs about the same.")
        }
        Some(false) => format!(
            "{who} is marked bad and won't be offered for {} days. \
             You can undo this in Settings, under GPU machines.",
            crate::provider::machines::AVOID_FOR_S / 86_400
        ),
        None if !created => format!("No GPU was created, so {who} was not marked either way."),
        None if never_started => format!(
            "{who} was not marked bad: the run was stopped after {} min of setup, \
             under the {} min needed to blame a machine.",
            ran_secs / 60,
            STUCK_SETUP_BLAME_SECS / 60
        ),
        None => format!(
            "{who} was not marked: the run ended after training had begun, \
             which isn't blamed on the host."
        ),
    };
    if let Some(e) = save_error {
        text.push_str(&format!(" (Couldn't save the machine history: {e})"));
    }
    text
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
            trainer: Trainer::Kohya {
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
            },
            max_minutes: 120,
            max_usd: 2.0,
            max_dph: 1.0,
            gpu_model: None,
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

    #[test]
    fn gpu_models_match_by_name_ignoring_case_and_spacing() {
        assert!(gpu_matches("RTX 4090", "4090"));
        assert!(gpu_matches("NVIDIA GeForce RTX 4090", "rtx 4090"));
        assert!(gpu_matches("RTX 4090", "RTX4090"));
        assert!(!gpu_matches("RTX 3090", "4090"));
        assert!(!gpu_matches("RTX 4090 Ti", "4090 Ti Super"));
        assert_eq!(clean_gpu_model(None), Ok(None));
        assert_eq!(clean_gpu_model(Some("  ")), Ok(None));
        assert_eq!(clean_gpu_model(Some(" 4090 ")), Ok(Some("4090".into())));
        for bad in ["4090; drop", "../x", &"9".repeat(25), "4090\n5090"] {
            assert!(clean_gpu_model(Some(bad)).is_err(), "{bad}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_gpu_model_narrows_the_quote_and_says_when_none_is_there() {
        let r = rig(vec![Progress::Done]);
        let mut req = request(&r);
        req.gpu_model = Some("a6000".into());
        let q = r.mgr.quote(req).await.unwrap();
        assert_eq!(q.gpu_name, "RTX A6000");
        let mut req = request(&r);
        req.gpu_model = Some("4090".into());
        let e = r.mgr.quote(req).await.unwrap_err();
        assert!(e.contains("No 4090 is available"), "{e}");
        let mut req = request(&r);
        req.gpu_model = Some("4090; rm".into());
        assert!(r.mgr.quote(req).await.unwrap_err().contains("short name"));
        // Quoting rents nothing, preference or not.
        assert!(r.provider.created_offers().is_empty());
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

    fn kromoss_request(r: &Rig) -> (tempfile::TempDir, QuoteRequest) {
        let d = tempfile::tempdir().unwrap();
        for i in 0..4u8 {
            std::fs::write(d.path().join(format!("i{i}.png")), png(i)).unwrap();
            std::fs::write(d.path().join(format!("i{i}.nl.txt")), "roka, a woman.").unwrap();
            std::fs::write(d.path().join(format!("i{i}.e621.txt")), "roka, solo").unwrap();
        }
        let req = QuoteRequest {
            dir: d.path().into(),
            trainer: Trainer::Kromoss {
                settings: MossSettings {
                    output_name: "roka".into(),
                    ..Default::default()
                },
            },
            ..request(r)
        };
        (d, req)
    }

    #[tokio::test(start_paused = true)]
    async fn a_kromoss_quote_uses_its_own_disk_plan_and_estimate() {
        let r = rig(vec![Progress::Done]);
        let (_d, req) = kromoss_request(&r);
        let q = r.mgr.quote(req).await.unwrap();
        assert_eq!(q.trainer, "kromoss");
        assert_eq!(q.gpu_name, "RTX A6000");
        // Storage is priced for the larger Kromoss disk.
        let hourly = 0.5420 + 0.2 * f64::from(job::MOSS_DISK_GB) / 720.0;
        assert!((q.hourly - hourly).abs() < 1e-9, "{}", q.hourly);
        assert_eq!((q.plan.images, q.plan.total_steps), (4, 800));
        assert_eq!(q.plan.eval_images, 0);
        // 25 min of setup plus 800 steps at the slower measured rate.
        assert_eq!(q.expected_minutes, MOSS_SETUP_MINUTES + 76);
        assert!(r.provider.created_offers().is_empty());
        // The kohya quote is unchanged.
        assert_eq!(r.mgr.quote(request(&r)).await.unwrap().trainer, "sdxl");
    }

    #[tokio::test(start_paused = true)]
    async fn a_kromoss_dataset_missing_tags_is_refused_before_any_search() {
        let r = rig(vec![Progress::Done]);
        let (d, req) = kromoss_request(&r);
        std::fs::remove_file(d.path().join("i2.e621.txt")).unwrap();
        let e = r.mgr.quote(req).await.unwrap_err();
        assert!(e.contains("i2.e621.txt"), "{e}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_kromoss_run_completes_without_a_civitai_key() {
        let r = rig(vec![
            Progress::Training {
                step: 200,
                total: 800,
            },
            Progress::Done,
        ]);
        let (_d, req) = kromoss_request(&r);
        let q = r.mgr.quote(req).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("completed"));
        assert!(s.synced && s.destroyed, "{s:?}");
        assert!(r.provider.live_ids().is_empty());
        let dir = r.mgr.results_dir().unwrap();
        assert!(dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("roka-"));
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
        assert!(s.host_note.as_deref().is_some_and(|n| n.contains("worked")));
        assert!(s.diagnostics_path.is_none(), "a good run needs no report");
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

    #[test]
    fn only_a_long_stop_during_setup_counts_against_the_host() {
        let long = STUCK_SETUP_BLAME_SECS;
        assert_eq!(host_verdict(&Outcome::Completed, false, 0), Some(true));
        assert_eq!(
            host_verdict(&Outcome::Failed("x".into()), true, 5),
            Some(false)
        );
        // Failed after training began: not the host's setup.
        assert_eq!(
            host_verdict(&Outcome::Failed("x".into()), false, long),
            None
        );
        for o in [Outcome::Cancelled, Outcome::DeadlineHit] {
            assert_eq!(host_verdict(&o, true, long), Some(false));
            assert_eq!(
                host_verdict(&o, true, long - 1),
                None,
                "a quick stop says nothing"
            );
            assert_eq!(host_verdict(&o, false, long), None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_a_host_stuck_in_setup_gets_it_skipped() {
        let r = rig(vec![Progress::Starting]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(STUCK_SETUP_BLAME_SECS + 30)).await;
        r.mgr.cancel();
        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("cancelled"));
        r.mgr.dismiss();
        let e = r.mgr.quote(request(&r)).await.unwrap_err();
        assert!(e.contains("No suitable GPU"), "{e}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_marked_bad_is_reported_and_the_failure_is_saved() {
        let r = rig(vec![Progress::Starting]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(STUCK_SETUP_BLAME_SECS + 30)).await;
        r.mgr.cancel();
        let s = finish(&r.mgr).await;
        let note = s
            .host_note
            .expect("the user is told what happened to the host");
        assert!(note.contains("marked bad"), "{note}");
        assert!(note.contains("RTX A6000"), "{note}");
        // The stages and the machine log survive the run, and a report is saved.
        assert!(s.stages.iter().any(|st| st.name == "Renting the GPU"));
        assert!(s.container_log.is_some());
        let path = s
            .diagnostics_path
            .expect("a failed run keeps its diagnostics");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("Stopped by you") && text.contains("Stages"),
            "{text}"
        );
        // The Settings list shows the machine, with the GPU and why.
        let rows = MachineFile::new(&r.out.path().join("data"))
            .load()
            .rows(r.mgr.now());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "avoided");
        assert_eq!(rows[0].gpu.as_deref(), Some("RTX A6000"));
        assert!(rows[0].reason.is_some());
        // Forgetting it makes the machine eligible again.
        assert!(MachineFile::new(&r.out.path().join("data"))
            .forget(rows[0].id)
            .unwrap());
        r.mgr.dismiss();
        assert!(r.mgr.quote(request(&r)).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn an_offer_taken_before_the_rental_does_not_blame_the_machine() {
        use crate::provider::mock::MockBehavior;
        let r = rig_with(vec![Progress::Starting], vec![MockBehavior::Unavailable]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        let s = finish(&r.mgr).await;
        assert_eq!(s.outcome.as_deref(), Some("failed"));
        let note = s.host_note.unwrap();
        assert!(note.contains("No GPU was created"), "{note}");
        assert!(MachineFile::new(&r.out.path().join("data"))
            .load()
            .rows(0)
            .is_empty());
        r.mgr.dismiss();
        assert!(r.mgr.quote(request(&r)).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_quick_stop_says_why_the_host_was_not_blamed() {
        let r = rig(vec![Progress::Starting]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        r.mgr.cancel();
        let s = finish(&r.mgr).await;
        let note = s.host_note.unwrap();
        assert!(note.contains("not marked bad"), "{note}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_quick_stop_leaves_the_host_eligible() {
        let r = rig(vec![Progress::Starting]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        r.mgr.cancel();
        finish(&r.mgr).await;
        r.mgr.dismiss();
        assert!(r.mgr.quote(request(&r)).await.is_ok());
    }

    fn leftover_record(r: &Rig, id: u64, phase: Phase) {
        let rec = RunRecord {
            label: "sloptweak-leftover".into(),
            phase,
            instance_id: Some(id),
            offer_id: 104,
            hourly: 0.4,
            started_unix: Some(1_000_000),
            synced: false,
            outcome: None,
        };
        r.mgr.run_file().save(&rec).unwrap();
    }

    async fn rent(r: &Rig) -> u64 {
        let spec = crate::provider::LaunchSpec {
            image: "img".into(),
            disk_gb: 80,
            label: "sloptweak-leftover".into(),
            env: vec![],
            onstart: String::new(),
        };
        r.provider.create_instance(104, &spec).await.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn the_gpu_of_a_run_in_progress_is_not_an_orphan() {
        let r = rig(vec![Progress::Starting]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        let id = r.provider.live_ids()[0];
        assert_eq!(r.mgr.ownership(id, None), Ownership::Running);
        assert_eq!(r.mgr.ownership(id + 1, None), Ownership::Other);
        r.mgr.cancel();
        finish(&r.mgr).await;
    }

    #[tokio::test(start_paused = true)]
    async fn stop_and_wait_downloads_destroys_and_confirms() {
        let r = rig(vec![Progress::Training {
            step: 5,
            total: 400,
        }]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(r.mgr.is_running());
        assert!(r.mgr.stop_and_wait(STOP_BUDGET).await);
        assert!(!r.mgr.is_running());
        assert!(r.provider.live_ids().is_empty());
        let s = r.mgr.status();
        assert_eq!(
            (s.outcome.as_deref(), s.destroyed, s.synced),
            (Some("cancelled"), true, true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_and_wait_gives_up_after_its_budget_but_keeps_stopping() {
        let r = rig(vec![Progress::Starting]);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        r.mgr.start(&q.plan_id).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Too short for a destroy to be confirmed: the caller must not close.
        assert!(!r.mgr.stop_and_wait(Duration::from_millis(1)).await);
        // The stop was requested anyway, so the run still ends and destroys.
        finish(&r.mgr).await;
        assert!(r.provider.live_ids().is_empty());
        assert!(r.mgr.stop_and_wait(STOP_BUDGET).await);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_running_means_nothing_to_wait_for() {
        let r = rig(vec![Progress::Done]);
        assert!(r.mgr.stop_and_wait(STOP_BUDGET).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_leftover_whose_gpu_is_gone_stops_blocking_new_runs() {
        let r = rig(vec![Progress::Done]);
        leftover_record(&r, 999, Phase::Running);
        assert_eq!(r.mgr.ownership(999, None), Ownership::Leftover);
        r.mgr.settle_leftover().await;
        assert_eq!(r.mgr.ownership(999, None), Ownership::Other);
        assert_eq!(r.mgr.run_file().load().unwrap().phase, Phase::Destroyed);
        let q = r.mgr.quote(request(&r)).await.unwrap();
        assert!(r.mgr.start(&q.plan_id).is_ok());
        finish(&r.mgr).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_leftover_that_still_bills_waits_for_the_user_then_is_destroyed() {
        let r = rig(vec![Progress::Done]);
        let id = rent(&r).await;
        leftover_record(&r, id, Phase::Running);
        r.mgr.settle_leftover().await;
        assert_eq!(r.provider.live_ids(), vec![id], "never destroyed unasked");
        assert_eq!(r.mgr.ownership(id, None), Ownership::Leftover);
        r.mgr.destroy_leftover(id, None).await.unwrap();
        assert!(r.provider.live_ids().is_empty());
        assert_eq!(r.mgr.run_file().load().unwrap().phase, Phase::Destroyed);
        assert!(r.mgr.quote(request(&r)).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_crash_mid_create_is_matched_by_label() {
        let r = rig(vec![Progress::Done]);
        let id = rent(&r).await;
        let rec = RunRecord {
            label: "sloptweak-leftover".into(),
            phase: Phase::Creating,
            instance_id: None,
            offer_id: 104,
            hourly: 0.4,
            started_unix: Some(1_000_000),
            synced: false,
            outcome: None,
        };
        r.mgr.run_file().save(&rec).unwrap();
        assert_eq!(
            r.mgr.ownership(id, Some("sloptweak-leftover")),
            Ownership::Leftover
        );
        assert_eq!(
            r.mgr.ownership(id, Some("sloptweak-other")),
            Ownership::Other
        );
        r.mgr
            .destroy_leftover(id, Some("sloptweak-leftover"))
            .await
            .unwrap();
        assert_eq!(r.mgr.run_file().load().unwrap().phase, Phase::Destroyed);
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
