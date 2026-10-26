//! The billing-safe lifecycle of one training run: approve an exact quote,
//! persist before creating, watch the run, then sync results and destroy the
//! instance on every exit path (finished, failed, deadline, cancelled).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::{
    GpuProvider, Health, InstanceInfo, LaunchSpec, Offer, OfferQuery, ProviderError, LABEL,
};

/// Hard ceilings no setting can raise: a first-run safety net.
pub const ABSOLUTE_MAX_SECONDS: u64 = 4 * 3600;
pub const ABSOLUTE_MAX_USD: f64 = 5.0;
/// Below this much remaining time a failed sync is not retried: spending
/// wins over data (the user approved this when approving the cap).
const SYNC_RETRY_MIN_REMAINING: u64 = 120;
const SYNC_ATTEMPTS: u32 = 3;
const DESTROY_ATTEMPTS: u32 = 3;
const MAX_CONSECUTIVE_ERRORS: u32 = 20;
/// How long the instance may take to publish its tunnel and accept the job
/// (image pull, environment setup) before the run is given up.
const BEGIN_GRACE_SECS: u64 = 15 * 60;
/// Setup (environment install, model check) that shows no change at all (same
/// step, same log size) for this long is stuck. A big wheel download writes
/// nothing to its log while it runs, so this is deliberately patient.
const SETUP_STALL_SECS: u64 = 8 * 60;
/// Longest setup, counted from when the instance first answered. A good host
/// needs about 8 minutes; the app's own estimate is 12.
const SETUP_CAP_SECS: u64 = 20 * 60;
/// A run the user stops (or that hits its limit) before training begins counts
/// against the host only if it had been going this long: a quick cancel says
/// nothing about the host.
pub const STUCK_SETUP_BLAME_SECS: u64 = 10 * 60;

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
#[error("{0}")]
pub struct GateError(pub String);

fn gate(msg: impl Into<String>) -> GateError {
    GateError(msg.into())
}

/// What the user approves in the UI, per run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_seconds: u64,
    pub max_usd: f64,
    /// Expected transfer onto the instance (model + dataset + environment).
    pub download_gb: f64,
}

impl Limits {
    pub fn validate(&self) -> Result<(), GateError> {
        if self.max_seconds == 0 || self.max_seconds > ABSOLUTE_MAX_SECONDS {
            return Err(gate("The time limit must be between 1 second and 4 hours."));
        }
        if !(self.max_usd > 0.0 && self.max_usd <= ABSOLUTE_MAX_USD) {
            return Err(gate("The spending cap must be above $0 and at most $5."));
        }
        if !self.download_gb.is_finite() || self.download_gb < 0.0 {
            return Err(gate("The transfer estimate is invalid."));
        }
        Ok(())
    }
}

/// Exactly what a run could cost at most, from the chosen offer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub offer_id: u64,
    /// Compute plus disk, $/hr.
    pub hourly: f64,
    pub worst_usd: f64,
}

impl Quote {
    pub fn new(offer: &Offer, disk_gb: u32, limits: &Limits) -> Self {
        // Vast storage is quoted per GB-month.
        let storage_hourly = offer.storage_cost * f64::from(disk_gb) / (30.0 * 24.0);
        let hourly = offer.dph_total + storage_hourly;
        Self {
            offer_id: offer.id,
            hourly,
            worst_usd: hourly * limits.max_seconds as f64 / 3600.0
                + offer.inet_down_cost * limits.download_gb,
        }
    }
}

/// The user's go-ahead for one exact offer and set of limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Approval {
    pub offer_id: u64,
    pub max_seconds: u64,
    pub max_usd: f64,
}

/// Offers a training run may use: 24 GB-class, bf16-capable (Ampere or
/// newer) and not Blackwell, which the pinned CUDA 12.4 stack lacks.
pub fn offer_query(max_dph: f64) -> OfferQuery {
    OfferQuery {
        min_vram_gb: 23.0,
        min_disk_gb: 80.0,
        min_reliability: 0.98,
        min_inet_down_mbps: 500.0,
        min_disk_bw_mbps: 0.0,
        max_dph,
        min_cuda: 12.4,
        min_compute_cap: 800,
        max_compute_cap: 999,
        min_ram_gb: 30.0,
        limit: 20,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Planned,
    Authorized,
    /// Persisted before the create call, so a crash mid-create is visible.
    Creating,
    Created,
    Running,
    Syncing,
    Destroying,
    Destroyed,
    /// The provider still lists the instance. Needs the user.
    CleanupUnconfirmed,
}

impl Phase {
    /// The run is over and nothing is billing (or nothing was ever created).
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Destroyed | Self::CleanupUnconfirmed)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub label: String,
    pub phase: Phase,
    pub instance_id: Option<u64>,
    pub offer_id: u64,
    pub hourly: f64,
    pub started_unix: Option<u64>,
    pub synced: bool,
    pub outcome: Option<String>,
}

/// One run's record on disk, written atomically.
pub struct RunFile {
    path: PathBuf,
}

impl RunFile {
    pub fn new(path: &Path) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Option<RunRecord> {
        serde_json::from_str(&std::fs::read_to_string(&self.path).ok()?).ok()
    }

    pub fn save(&self, rec: &RunRecord) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(rec)?)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Starting,
    Training { step: u32, total: u32 },
    Evaluating { done: u32, total: u32 },
    Done,
    Failed(String),
}

/// What the instance is doing before the first training step.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetupNote {
    pub stage: String,
    pub detail: Option<String>,
    /// Total size of the setup logs: grows while something is being written.
    /// `None` from an older sidecar, in which case a stall can't be judged and
    /// only the overall cap applies.
    pub log_bytes: Option<u64>,
    /// The end of the newest setup log.
    pub log_tail: String,
}

/// Watches setup for a stall. `check` is called on every poll while the
/// instance reports `Progress::Starting`.
#[derive(Debug, Default)]
struct SetupWatch {
    since: Option<u64>,
    quiet_since: u64,
    sig: Option<(String, Option<String>, Option<u64>)>,
}

impl SetupWatch {
    /// `now` is unix seconds. Returns why setup should be abandoned, if so.
    fn check(&mut self, now: u64, note: Option<&SetupNote>) -> Option<String> {
        let since = *self.since.get_or_insert_with(|| {
            self.quiet_since = now;
            now
        });
        if let Some(n) = note.filter(|n| n.log_bytes.is_some()) {
            let sig = (n.stage.clone(), n.detail.clone(), n.log_bytes);
            if self.sig.as_ref() != Some(&sig) {
                self.sig = Some(sig);
                self.quiet_since = now;
            }
        }
        let doing = note
            .and_then(|n| n.detail.as_deref())
            .map(|d| format!(" (last step: {d})"))
            .unwrap_or_default();
        let quiet = now.saturating_sub(self.quiet_since);
        if self.sig.is_some() && quiet >= SETUP_STALL_SECS {
            return Some(format!(
                "Setting up the GPU stopped making progress for {} min{doing}.",
                quiet / 60
            ));
        }
        (now.saturating_sub(since) >= SETUP_CAP_SECS).then(|| {
            format!(
                "Setting up the GPU took longer than {} min{doing}.",
                SETUP_CAP_SECS / 60
            )
        })
    }
}

/// The instance side of a run, reached through the sidecar (Phase T2).
#[async_trait]
pub trait RemoteTrain: Send + Sync {
    /// Hand the instance its job and start the run. Called once it is
    /// reachable; must be safe to repeat after a lost answer.
    async fn begin(&self, instance: u64) -> Result<(), String>;
    async fn progress(&self, instance: u64) -> Result<Progress, String>;
    /// [`progress`](Self::progress) plus, while the run is still being set
    /// up, what setup is doing. Backends that can't say leave it `None`.
    async fn status(&self, instance: u64) -> Result<(Progress, Option<SetupNote>), String> {
        Ok((self.progress(instance).await?, None))
    }
    /// Download every result file and verify its hash. `Ok(true)` only when
    /// all of them are verified locally.
    async fn sync(&self, instance: u64) -> Result<bool, String>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Completed,
    Failed(String),
    DeadlineHit,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub outcome: Outcome,
    pub record: RunRecord,
}

pub struct Plan {
    pub offer: Offer,
    pub spec: LaunchSpec,
    pub limits: Limits,
}

/// What the UI shows while a run is going; the runner keeps it current.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub record: Option<RunRecord>,
    pub progress: Progress,
    /// What setup is doing now (only while `progress` is `Starting`).
    pub setup: Option<SetupNote>,
    /// Unix time setup last changed (a step started or a log grew).
    pub setup_quiet_since: Option<u64>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            record: None,
            progress: Progress::Starting,
            setup: None,
            setup_quiet_since: None,
        }
    }
}

pub struct Runner {
    pub live: Arc<std::sync::Mutex<Live>>,
    pub provider: Arc<dyn GpuProvider>,
    pub remote: Arc<dyn RemoteTrain>,
    pub file: RunFile,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub poll: Duration,
    pub cancel: Arc<AtomicBool>,
}

impl Runner {
    fn save(&self, rec: &RunRecord) {
        if let Ok(mut live) = self.live.lock() {
            live.record = Some(rec.clone());
        }
        if let Err(e) = self.file.save(rec) {
            log::warn!("could not save the training run record: {e}");
        }
    }

    /// Run to the end. Returns `Err` only when nothing was created.
    pub async fn run(&self, plan: &Plan, approval: &Approval) -> Result<RunResult, GateError> {
        plan.limits.validate()?;
        let quote = Quote::new(&plan.offer, plan.spec.disk_gb, &plan.limits);
        if quote.worst_usd > plan.limits.max_usd {
            return Err(gate(format!(
                "This run could cost up to ${:.2}, over your ${:.2} cap. \
                 Pick a cheaper GPU or shorten the run.",
                quote.worst_usd, plan.limits.max_usd
            )));
        }
        if approval.offer_id != plan.offer.id
            || approval.max_seconds != plan.limits.max_seconds
            || (approval.max_usd - plan.limits.max_usd).abs() > 1e-9
        {
            return Err(gate(
                "The approval doesn't match this exact GPU and limits.",
            ));
        }
        if !plan.spec.label.starts_with(LABEL) {
            return Err(gate("The instance label must be a SlopTweak label."));
        }
        if let Some(old) = self.file.load() {
            if !old.phase.is_finished() {
                return Err(gate(
                    "A previous training run didn't finish cleanly. \
                     Resolve it before starting another.",
                ));
            }
        }

        let mut rec = RunRecord {
            label: plan.spec.label.clone(),
            phase: Phase::Authorized,
            instance_id: None,
            offer_id: plan.offer.id,
            hourly: quote.hourly,
            started_unix: None,
            synced: false,
            outcome: None,
        };
        self.save(&rec);
        // Persist before the create call: the clock starts here and is never
        // reset, and a crash mid-create leaves a record to reconcile.
        rec.phase = Phase::Creating;
        rec.started_unix = Some((self.clock)());
        self.save(&rec);

        let mut ids = Vec::new();
        match self
            .provider
            .create_instance(plan.offer.id, &plan.spec)
            .await
        {
            Ok(id) => ids.push(id),
            Err(e) if e.create_may_have_succeeded() => {
                ids = self.reconcile(&plan.spec.label).await;
            }
            Err(e) => {
                rec.phase = Phase::Destroyed;
                rec.outcome = Some(format!("The GPU could not be rented: {e}"));
                self.save(&rec);
                return Ok(RunResult {
                    outcome: Outcome::Failed(rec.outcome.clone().unwrap_or_default()),
                    record: rec,
                });
            }
        }
        let Some(&id) = ids.first() else {
            rec.phase = Phase::Destroyed;
            rec.outcome = Some("The rental response was lost and no instance appeared.".into());
            self.save(&rec);
            return Ok(RunResult {
                outcome: Outcome::Failed(rec.outcome.clone().unwrap_or_default()),
                record: rec,
            });
        };
        rec.instance_id = Some(id);
        rec.phase = Phase::Created;
        self.save(&rec);

        let (outcome, ran) = self.watch(id, &mut rec, plan.limits.max_seconds).await;
        self.cleanup(&mut rec, &ids, ran, plan.limits.max_seconds)
            .await;
        rec.outcome = Some(format!("{outcome:?}"));
        self.save(&rec);
        Ok(RunResult {
            outcome,
            record: rec,
        })
    }

    /// Find instances carrying our unique attempt label after an ambiguous
    /// create. Several matches are all ours, and all get destroyed.
    async fn reconcile(&self, label: &str) -> Vec<u64> {
        for _ in 0..5 {
            if let Ok(list) = self.provider.list_instances().await {
                let ids: Vec<u64> = list
                    .iter()
                    .filter(|i| i.label.as_deref() == Some(label))
                    .map(|i| i.id)
                    .collect();
                if !ids.is_empty() {
                    return ids;
                }
            }
            tokio::time::sleep(self.poll).await;
        }
        Vec::new()
    }

    /// Ask the instance how it is doing and keep the answer for the UI.
    /// Also feeds setup into `watch`, so a stuck setup ends the run.
    async fn progress_noted(
        &self,
        id: u64,
        watch: &mut SetupWatch,
    ) -> Result<(Progress, Option<String>), String> {
        let (p, note) = self.remote.status(id).await?;
        let mut stuck = None;
        if p == Progress::Starting {
            stuck = watch.check((self.clock)(), note.as_ref());
        }
        if let Ok(mut live) = self.live.lock() {
            live.progress = p.clone();
            if p == Progress::Starting {
                live.setup = note;
                live.setup_quiet_since = watch.sig.is_some().then_some(watch.quiet_since);
            } else {
                live.setup = None;
            }
        }
        Ok((p, stuck))
    }

    fn elapsed(&self, rec: &RunRecord) -> u64 {
        (self.clock)().saturating_sub(rec.started_unix.unwrap_or(0))
    }

    /// Watch until the run ends. Also reports whether the instance ever ran.
    async fn watch(&self, id: u64, rec: &mut RunRecord, max_seconds: u64) -> (Outcome, bool) {
        let mut ran = false;
        let mut provider_errors = 0;
        let mut remote_errors = 0;
        let mut begun = false;
        let mut setup = SetupWatch::default();
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return (Outcome::Cancelled, ran);
            }
            let elapsed = self.elapsed(rec);
            if elapsed >= max_seconds {
                return (Outcome::DeadlineHit, ran);
            }
            match self.provider.instance(id).await {
                Ok(None) => {
                    return (
                        Outcome::Failed("The GPU host removed the machine.".into()),
                        ran,
                    )
                }
                Err(ProviderError::Auth) => {
                    return (
                        Outcome::Failed("The Vast API key was rejected.".into()),
                        ran,
                    )
                }
                Err(_) => {
                    provider_errors += 1;
                    if provider_errors >= MAX_CONSECUTIVE_ERRORS {
                        return (Outcome::Failed("Lost contact with Vast.".into()), ran);
                    }
                }
                Ok(Some(info)) => {
                    provider_errors = 0;
                    match info.health(Duration::from_secs(elapsed), Duration::ZERO) {
                        Health::Failed(m) => {
                            // Keep the host's own words: they say why it died.
                            let detail = info.status_msg.as_deref().filter(|d| !d.is_empty());
                            let msg = match detail {
                                Some(d) => {
                                    format!("{m} ({})", d.chars().take(200).collect::<String>())
                                }
                                None => m,
                            };
                            return (Outcome::Failed(msg), ran);
                        }
                        Health::Starting => {}
                        Health::Running if !begun => match self.remote.begin(id).await {
                            Ok(()) => {
                                begun = true;
                                remote_errors = 0;
                            }
                            Err(_) if elapsed >= BEGIN_GRACE_SECS => {
                                return (Outcome::Failed("Couldn't reach the GPU.".into()), ran);
                            }
                            Err(_) => {}
                        },
                        Health::Running => match self.progress_noted(id, &mut setup).await {
                            Ok((Progress::Done, _)) => {
                                ran = true;
                                return (Outcome::Completed, ran);
                            }
                            Ok((Progress::Failed(m), _)) => {
                                ran = true;
                                return (Outcome::Failed(m), ran);
                            }
                            Ok((Progress::Starting, stuck)) => {
                                remote_errors = 0;
                                if let Some(why) = stuck {
                                    return (Outcome::Failed(why), ran);
                                }
                            }
                            Ok(_) => {
                                ran = true;
                                remote_errors = 0;
                                if rec.phase != Phase::Running {
                                    rec.phase = Phase::Running;
                                    self.save(rec);
                                }
                            }
                            Err(_) => {
                                remote_errors += 1;
                                if remote_errors >= MAX_CONSECUTIVE_ERRORS {
                                    return (
                                        Outcome::Failed("Lost contact with the GPU.".into()),
                                        ran,
                                    );
                                }
                            }
                        },
                    }
                }
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Verified sync, then destroy. Always destroys.
    async fn cleanup(&self, rec: &mut RunRecord, ids: &[u64], ran: bool, max_seconds: u64) {
        if let (Some(&id), true) = (ids.first(), ran) {
            rec.phase = Phase::Syncing;
            self.save(rec);
            for attempt in 0..SYNC_ATTEMPTS {
                if matches!(self.remote.sync(id).await, Ok(true)) {
                    rec.synced = true;
                    break;
                }
                let remaining = max_seconds.saturating_sub(self.elapsed(rec));
                if attempt + 1 == SYNC_ATTEMPTS || remaining <= SYNC_RETRY_MIN_REMAINING {
                    break;
                }
                tokio::time::sleep(self.poll).await;
            }
        }
        rec.phase = Phase::Destroying;
        self.save(rec);
        let mut leftover: Vec<u64> = ids.to_vec();
        for _ in 0..DESTROY_ATTEMPTS {
            for id in &leftover {
                let _ = self.provider.destroy(*id).await;
            }
            if let Ok(list) = self.provider.list_instances().await {
                leftover.retain(|id| list.iter().any(|i: &InstanceInfo| i.id == *id));
                if leftover.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(self.poll).await;
        }
        rec.phase = if leftover.is_empty() {
            Phase::Destroyed
        } else {
            Phase::CleanupUnconfirmed
        };
        self.save(rec);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::time::Instant;

    use super::*;
    use crate::provider::attempt_label;
    use crate::provider::mock::{MockBehavior, MockProvider, MockTimings};

    /// Scripted instance side: replies in order, then repeats the last.
    struct Script {
        progress: Mutex<Vec<Result<Progress, String>>>,
        sync: Mutex<Vec<Result<bool, String>>>,
        syncs: Mutex<u32>,
        begins: Mutex<Vec<Result<(), String>>>,
        begin_calls: Mutex<u32>,
        notes: Mutex<Vec<Option<SetupNote>>>,
    }

    fn note(detail: &str, log_bytes: u64) -> Option<SetupNote> {
        Some(SetupNote {
            stage: "bootstrap".into(),
            detail: Some(detail.into()),
            log_bytes: Some(log_bytes),
            log_tail: "Downloading torch".into(),
        })
    }

    impl Script {
        fn new(
            progress: Vec<Result<Progress, String>>,
            sync: Vec<Result<bool, String>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                progress: Mutex::new(progress),
                sync: Mutex::new(sync),
                syncs: Mutex::new(0),
                begins: Mutex::new(vec![Ok(())]),
                begin_calls: Mutex::new(0),
                notes: Mutex::new(vec![None]),
            })
        }

        fn with_notes(self: Arc<Self>, notes: Vec<Option<SetupNote>>) -> Arc<Self> {
            *self.notes.lock().unwrap() = notes;
            self
        }
    }

    fn next<T: Clone>(v: &Mutex<Vec<T>>) -> T {
        let mut v = v.lock().unwrap();
        if v.len() > 1 {
            v.remove(0)
        } else {
            v[0].clone()
        }
    }

    #[async_trait]
    impl RemoteTrain for Script {
        async fn begin(&self, _: u64) -> Result<(), String> {
            *self.begin_calls.lock().unwrap() += 1;
            next(&self.begins)
        }
        async fn progress(&self, _: u64) -> Result<Progress, String> {
            next(&self.progress)
        }
        async fn status(&self, _: u64) -> Result<(Progress, Option<SetupNote>), String> {
            Ok((next(&self.progress)?, next(&self.notes)))
        }
        async fn sync(&self, _: u64) -> Result<bool, String> {
            *self.syncs.lock().unwrap() += 1;
            next(&self.sync)
        }
    }

    fn limits() -> Limits {
        Limits {
            max_seconds: 3600,
            max_usd: 2.0,
            download_gb: 20.0,
        }
    }

    fn make_plan(provider: &MockProvider, limits: Limits) -> (Plan, Approval) {
        let offer = futures_offer(provider);
        let spec = LaunchSpec {
            image: "img".into(),
            disk_gb: 80,
            label: attempt_label("abc123"),
            env: vec![],
            onstart: String::new(),
        };
        let approval = Approval {
            offer_id: offer.id,
            max_seconds: limits.max_seconds,
            max_usd: limits.max_usd,
        };
        (
            Plan {
                offer,
                spec,
                limits,
            },
            approval,
        )
    }

    fn futures_offer(_: &MockProvider) -> Offer {
        // The mock's RTX 3090 offer (id 104): $0.1907/hr, free ingress.
        Offer {
            id: 104,
            gpu_name: "RTX 3090".into(),
            gpu_ram_mb: 24_576.0,
            num_gpus: 1,
            dph_total: 0.1907,
            storage_cost: 0.2,
            inet_down_cost: 0.0,
            inet_down_mbps: 2500.0,
            disk_bw_mbps: 3000.0,
            reliability: 0.995,
            verified: true,
            disk_space_gb: 120.0,
            cuda_max_good: 12.8,
            compute_cap: 860,
            cpu_ram_mb: 64_000.0,
            geolocation: None,
            machine_id: Some(1040),
        }
    }

    struct Rig {
        runner: Runner,
        provider: MockProvider,
        script: Arc<Script>,
        file: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn rig(behaviors: Vec<MockBehavior>, script: Arc<Script>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run.json");
        let provider = MockProvider::new(MockTimings::default(), behaviors);
        let t0 = Instant::now();
        let runner = Runner {
            live: Default::default(),
            provider: Arc::new(provider.clone()),
            remote: script.clone(),
            file: RunFile::new(&file),
            clock: Arc::new(move || 1_000_000 + t0.elapsed().as_secs()),
            poll: Duration::from_secs(10),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        Rig {
            runner,
            provider,
            script,
            file,
            _dir: dir,
        }
    }

    fn step(n: u32) -> Result<Progress, String> {
        Ok(Progress::Training {
            step: n,
            total: 920,
        })
    }

    #[test]
    fn limits_and_quote() {
        assert!(limits().validate().is_ok());
        for bad in [
            Limits {
                max_seconds: 0,
                ..limits()
            },
            Limits {
                max_seconds: 5 * 3600,
                ..limits()
            },
            Limits {
                max_usd: 0.0,
                ..limits()
            },
            Limits {
                max_usd: 50.0,
                ..limits()
            },
            Limits {
                download_gb: f64::NAN,
                ..limits()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        let p = MockProvider::new(MockTimings::default(), vec![]);
        let q = Quote::new(&futures_offer(&p), 80, &limits());
        // 0.1907 + 0.2 * 80 / 720 per hour, for one hour, free ingress.
        assert!((q.hourly - (0.1907 + 80.0 * 0.2 / 720.0)).abs() < 1e-9);
        assert!((q.worst_usd - q.hourly).abs() < 1e-9);
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_a_quote_over_the_cap() {
        let r = rig(
            vec![],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let tight = Limits {
            max_usd: 0.05,
            ..limits()
        };
        let (plan, approval) = make_plan(&r.provider, tight);
        let err = r.runner.run(&plan, &approval).await.unwrap_err();
        assert!(err.0.contains("over your $0.05 cap"), "{err}");
        assert!(r.provider.created_offers().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_a_mismatched_approval() {
        let r = rig(
            vec![],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let (plan, good) = make_plan(&r.provider, limits());
        for bad in [
            Approval {
                offer_id: 999,
                ..good
            },
            Approval {
                max_seconds: 7200,
                ..good
            },
            Approval {
                max_usd: 4.0,
                ..good
            },
        ] {
            assert!(r.runner.run(&plan, &bad).await.is_err());
        }
        assert!(r.provider.created_offers().is_empty());
        assert!(r.runner.file.load().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_a_foreign_label() {
        let r = rig(
            vec![],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let (mut plan, approval) = make_plan(&r.provider, limits());
        plan.spec.label = "someone-elses".into();
        assert!(r.runner.run(&plan, &approval).await.is_err());
        assert!(r.provider.created_offers().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn happy_path_syncs_then_destroys() {
        let script = Script::new(
            vec![
                Ok(Progress::Starting),
                step(100),
                step(900),
                Ok(Progress::Done),
            ],
            vec![Ok(true)],
        );
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Completed);
        assert_eq!(res.record.phase, Phase::Destroyed);
        assert!(res.record.synced);
        assert!(r.provider.live_ids().is_empty());
        assert_eq!(r.provider.created_offers(), vec![104]);
        assert_eq!(r.runner.file.load().unwrap(), res.record);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_run_still_syncs_what_exists_and_destroys() {
        let script = Script::new(
            vec![step(300), Ok(Progress::Failed("CUDA out of memory".into()))],
            vec![Ok(true)],
        );
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Failed("CUDA out of memory".into()));
        assert!(res.record.synced);
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_ends_a_run_that_never_finishes() {
        let r = rig(vec![], Script::new(vec![step(5)], vec![Ok(true)]));
        let short = Limits {
            max_seconds: 300,
            ..limits()
        };
        let (plan, approval) = make_plan(&r.provider, short);
        let start = Instant::now();
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::DeadlineHit);
        assert_eq!(res.record.phase, Phase::Destroyed);
        assert!(r.provider.live_ids().is_empty());
        assert!(start.elapsed() < Duration::from_secs(300 + 60));
    }

    #[test]
    fn setup_watch_flags_a_stall_and_resets_on_any_change() {
        let mut w = SetupWatch::default();
        let a = note("installing: packages", 100);
        assert!(w.check(0, a.as_ref()).is_none());
        assert!(w.check(SETUP_STALL_SECS - 1, a.as_ref()).is_none());
        // The log grew: the clock restarts.
        let b = note("installing: packages", 200);
        assert!(w.check(SETUP_STALL_SECS, b.as_ref()).is_none());
        assert!(w.check(SETUP_STALL_SECS * 2 - 1, b.as_ref()).is_none());
        let why = w.check(SETUP_STALL_SECS * 2, b.as_ref()).unwrap();
        assert!(
            why.contains("8 min") && why.contains("installing: packages"),
            "{why}"
        );
    }

    #[test]
    fn setup_watch_without_notes_only_enforces_the_cap() {
        let mut w = SetupWatch::default();
        assert!(w.check(0, None).is_none());
        assert!(w.check(SETUP_CAP_SECS - 1, None).is_none());
        assert!(w.check(SETUP_CAP_SECS, None).unwrap().contains("20 min"));
    }

    #[test]
    fn setup_watch_ignores_stalls_from_a_sidecar_without_log_sizes() {
        let mut w = SetupWatch::default();
        let old = Some(SetupNote {
            detail: Some("installing: packages".into()),
            ..SetupNote::default()
        });
        assert!(w.check(0, old.as_ref()).is_none());
        assert!(w.check(SETUP_CAP_SECS - 1, old.as_ref()).is_none());
        assert!(w.check(SETUP_CAP_SECS, old.as_ref()).is_some());
    }

    #[test]
    fn setup_watch_cap_applies_even_when_the_log_keeps_growing() {
        let mut w = SetupWatch::default();
        for t in (0..SETUP_CAP_SECS).step_by(60) {
            assert!(w
                .check(t, note("installing: packages", t).as_ref())
                .is_none());
        }
        let n = note("installing: packages", SETUP_CAP_SECS);
        assert!(w
            .check(SETUP_CAP_SECS, n.as_ref())
            .unwrap()
            .contains("longer than"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_setup_ends_the_run_and_destroys_the_gpu() {
        let script = Script::new(vec![Ok(Progress::Starting)], vec![Ok(true)])
            .with_notes(vec![note("installing: packages", 500)]);
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let start = Instant::now();
        let res = r.runner.run(&plan, &approval).await.unwrap();
        let Outcome::Failed(why) = &res.outcome else {
            panic!("{:?}", res.outcome)
        };
        assert!(
            why.contains("stopped making progress") && why.contains("packages"),
            "{why}"
        );
        // Long before the one-hour limit.
        assert!(start.elapsed() < Duration::from_secs(SETUP_STALL_SECS + 120));
        assert_eq!(res.record.phase, Phase::Destroyed);
        assert!(r.provider.live_ids().is_empty());
        assert_eq!(r.runner.live.lock().unwrap().progress, Progress::Starting);
    }

    #[tokio::test(start_paused = true)]
    async fn a_setup_that_keeps_moving_is_left_alone_until_training() {
        let mut notes: Vec<Option<SetupNote>> =
            (1..=60).map(|i| note("installing: packages", i)).collect();
        notes.push(None);
        let mut progress = vec![Ok(Progress::Starting); 60];
        progress.extend([step(1), Ok(Progress::Done)]);
        let r = rig(
            vec![],
            Script::new(progress, vec![Ok(true)]).with_notes(notes),
        );
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Completed);
        assert!(r.runner.live.lock().unwrap().setup.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_ui_sees_what_setup_is_doing() {
        let script = Script::new(vec![Ok(Progress::Starting)], vec![Ok(true)])
            .with_notes(vec![note("installing: packages", 500)]);
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let live = r.runner.live.clone();
        let cancel = r.runner.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let seen = live.lock().unwrap().clone();
            assert_eq!(
                seen.setup.unwrap().detail.as_deref(),
                Some("installing: packages")
            );
            assert!(seen.setup_quiet_since.is_some());
            cancel.store(true, Ordering::SeqCst);
        });
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Cancelled);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_destroys() {
        let r = rig(vec![], Script::new(vec![step(5)], vec![Ok(true)]));
        let (plan, approval) = make_plan(&r.provider, limits());
        let cancel = r.runner.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            cancel.store(true, Ordering::SeqCst);
        });
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Cancelled);
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_sync_retries_then_destroys_anyway() {
        let script = Script::new(vec![Ok(Progress::Done)], vec![Err("tunnel down".into())]);
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Completed);
        assert!(!res.record.synced);
        assert_eq!(res.record.phase, Phase::Destroyed);
        assert_eq!(*r.script.syncs.lock().unwrap(), SYNC_ATTEMPTS);
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn no_sync_retry_near_the_deadline() {
        let script = Script::new(vec![step(5)], vec![Ok(false)]);
        let r = rig(vec![], script);
        let short = Limits {
            max_seconds: 200,
            ..limits()
        };
        let (plan, approval) = make_plan(&r.provider, short);
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::DeadlineHit);
        // Under two minutes were left, so one sync attempt and then destroy.
        assert_eq!(*r.script.syncs.lock().unwrap(), 1);
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unavailable_offer_creates_nothing() {
        let r = rig(
            vec![MockBehavior::Unavailable],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert!(matches!(res.outcome, Outcome::Failed(_)));
        assert!(r.provider.live_ids().is_empty());
        assert_eq!(*r.script.syncs.lock().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_create_response_is_reconciled_not_repeated() {
        let r = rig(
            vec![MockBehavior::LostCreateResponse],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Completed);
        assert_eq!(r.provider.created_offers().len(), 1, "no second create");
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_host_fails_fast_and_cleans_up() {
        let r = rig(
            vec![MockBehavior::DeadHost],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert!(matches!(res.outcome, Outcome::Failed(_)));
        assert_eq!(res.record.phase, Phase::Destroyed);
        assert!(r.provider.live_ids().is_empty());
        // It never ran, so there was nothing to sync.
        assert_eq!(*r.script.syncs.lock().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unfinished_previous_run_blocks_a_new_one() {
        let r = rig(
            vec![],
            Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]),
        );
        let (plan, approval) = make_plan(&r.provider, limits());
        r.runner
            .file
            .save(&RunRecord {
                label: "sloptweak-old".into(),
                phase: Phase::Running,
                instance_id: Some(1),
                offer_id: 1,
                hourly: 0.2,
                started_unix: Some(1),
                synced: false,
                outcome: None,
            })
            .unwrap();
        assert!(r.runner.run(&plan, &approval).await.is_err());
        assert!(r.provider.created_offers().is_empty());
        assert!(r.file.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn remote_errors_are_tolerated_until_they_persist() {
        // Sidecar not reachable yet: a few errors, then progress, then done.
        let script = Script::new(
            vec![
                Err("no route".into()),
                Err("no route".into()),
                step(1),
                Ok(Progress::Done),
            ],
            vec![Ok(true)],
        );
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Completed);

        let r = rig(
            vec![],
            Script::new(vec![Err("no route".into())], vec![Ok(true)]),
        );
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(
            res.outcome,
            Outcome::Failed("Lost contact with the GPU.".into())
        );
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn begin_is_retried_until_the_instance_answers() {
        let script = Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]);
        *script.begins.lock().unwrap() =
            vec![Err("tunnel not up".into()), Err("still".into()), Ok(())];
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(res.outcome, Outcome::Completed);
        assert_eq!(*r.script.begin_calls.lock().unwrap(), 3);
        assert!(r.provider.live_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_can_never_begin_fails_and_is_destroyed() {
        let script = Script::new(vec![Ok(Progress::Done)], vec![Ok(true)]);
        *script.begins.lock().unwrap() = vec![Err("no route".into())];
        let r = rig(vec![], script);
        let (plan, approval) = make_plan(&r.provider, limits());
        let res = r.runner.run(&plan, &approval).await.unwrap();
        assert_eq!(
            res.outcome,
            Outcome::Failed("Couldn't reach the GPU.".into())
        );
        assert_eq!(res.record.phase, Phase::Destroyed);
        assert!(r.provider.live_ids().is_empty());
        // It never ran, so there was nothing to sync.
        assert_eq!(*r.script.syncs.lock().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_is_visible_to_the_ui() {
        let script = Script::new(
            vec![step(100), step(400), Ok(Progress::Done)],
            vec![Ok(true)],
        );
        let r = rig(vec![], script);
        let live = r.runner.live.clone();
        let (plan, approval) = make_plan(&r.provider, limits());
        r.runner.run(&plan, &approval).await.unwrap();
        let live = live.lock().unwrap();
        assert_eq!(live.progress, Progress::Done);
        assert_eq!(live.record.as_ref().unwrap().phase, Phase::Destroyed);
    }
}
