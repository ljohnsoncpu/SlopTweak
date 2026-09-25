//! Session state machine and its driver.
//!
//! `Idle → Renting → Provisioning → Ready → Stopping → Idle`, plus
//! `Failed(reason)`. [`next`] is the pure transition function; the
//! [`SessionManager`] drives it against a [`GpuProvider`] and [`SidecarApi`]
//! and never changes state except through it.
//!
//! Money rules the driver enforces:
//! * The instance id is persisted, and its launch secret stored, the moment
//!   the create call returns, before anything else can fail.
//! * Every path that gives up on an instance destroys it first. If destroy
//!   can't be confirmed the record is kept, so a relaunch retries, and the
//!   instance watchdog destroys it anyway once heartbeats stop.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use url::Url;

use crate::catalog::Model;
use crate::config::{self, Settings};
use crate::persist::{now_unix, ActiveRecord, RecordFile};
use crate::provider::offers::{self, RankedOffer};
use crate::provider::{GpuProvider, Health, InstanceInfo, ProviderError};
use crate::redact::redact;
use crate::secrets::{self, SecretStore};
use crate::sidecar::{auth_url, Deadlines, SidecarApi, SidecarError, SidecarStatus};

// ---------------------------------------------------------------------------
// Pure state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OfferSummary {
    pub offer_id: u64,
    pub gpu_name: String,
    /// $/hr including storage.
    pub hourly: f64,
    /// One-off model download cost.
    pub download_cost: f64,
    pub location: Option<String>,
}

impl From<&RankedOffer> for OfferSummary {
    fn from(r: &RankedOffer) -> Self {
        Self {
            offer_id: r.offer.id,
            gpu_name: r.offer.gpu_name.clone(),
            hourly: r.hourly,
            download_cost: r.download_cost,
            location: r.offer.geolocation.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionState {
    Idle {
        notice: Option<String>,
    },
    Renting {
        attempt: u32,
        max_attempts: u32,
        offer: Option<OfferSummary>,
    },
    Provisioning {
        attempt: u32,
        max_attempts: u32,
        instance_id: u64,
        offer: OfferSummary,
        stage: Option<String>,
        detail: Option<String>,
        progress: Option<f64>,
    },
    Ready {
        instance_id: u64,
        offer: OfferSummary,
        ready_unix: u64,
        deadlines: Option<Deadlines>,
    },
    Stopping {
        instance_id: Option<u64>,
    },
    Failed {
        reason: String,
    },
}

impl SessionState {
    pub fn idle() -> Self {
        Self::Idle { notice: None }
    }

    /// True while an instance may exist that this session owns.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Idle { .. } | Self::Failed { .. })
    }

    pub fn instance_id(&self) -> Option<u64> {
        match self {
            Self::Provisioning { instance_id, .. } | Self::Ready { instance_id, .. } => {
                Some(*instance_id)
            }
            Self::Stopping { instance_id } => *instance_id,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Start {
        max_attempts: u32,
    },
    OfferPicked(OfferSummary),
    InstanceCreated {
        instance_id: u64,
    },
    Stage(SidecarStatus),
    Ready {
        at_unix: u64,
    },
    Heartbeat(SidecarStatus),
    /// The current attempt is over (its instance, if any, already destroyed).
    AttemptFailed {
        reason: String,
        retry: bool,
    },
    StopRequested,
    Destroyed,
    /// The instance disappeared or shut itself down while in use.
    InstanceGone {
        reason: String,
    },
    Reattach {
        instance_id: u64,
        offer: OfferSummary,
    },
    DestroyOrphan {
        instance_id: u64,
    },
    Fail {
        reason: String,
    },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("invalid transition: {event} in {state}")]
pub struct InvalidTransition {
    pub state: String,
    pub event: String,
}

fn name(s: &SessionState) -> &'static str {
    match s {
        SessionState::Idle { .. } => "Idle",
        SessionState::Renting { .. } => "Renting",
        SessionState::Provisioning { .. } => "Provisioning",
        SessionState::Ready { .. } => "Ready",
        SessionState::Stopping { .. } => "Stopping",
        SessionState::Failed { .. } => "Failed",
    }
}

pub fn next(state: &SessionState, event: &Event) -> Result<SessionState, InvalidTransition> {
    use Event as E;
    use SessionState as S;
    let out = match (state, event) {
        (S::Idle { .. } | S::Failed { .. }, E::Start { max_attempts }) => S::Renting {
            attempt: 1,
            max_attempts: (*max_attempts).max(1),
            offer: None,
        },
        (
            S::Renting {
                attempt,
                max_attempts,
                ..
            },
            E::OfferPicked(o),
        ) => S::Renting {
            attempt: *attempt,
            max_attempts: *max_attempts,
            offer: Some(o.clone()),
        },
        (
            S::Renting {
                attempt,
                max_attempts,
                offer: Some(o),
            },
            E::InstanceCreated { instance_id },
        ) => S::Provisioning {
            attempt: *attempt,
            max_attempts: *max_attempts,
            instance_id: *instance_id,
            offer: o.clone(),
            stage: None,
            detail: None,
            progress: None,
        },
        (
            S::Provisioning {
                attempt,
                max_attempts,
                instance_id,
                offer,
                ..
            },
            E::Stage(st),
        ) => S::Provisioning {
            attempt: *attempt,
            max_attempts: *max_attempts,
            instance_id: *instance_id,
            offer: offer.clone(),
            stage: Some(st.stage.clone()),
            detail: st.detail.clone(),
            progress: st.progress,
        },
        (
            S::Provisioning {
                instance_id, offer, ..
            },
            E::Ready { at_unix },
        ) => S::Ready {
            instance_id: *instance_id,
            offer: offer.clone(),
            ready_unix: *at_unix,
            deadlines: None,
        },
        (
            S::Ready {
                instance_id,
                offer,
                ready_unix,
                ..
            },
            E::Heartbeat(st),
        ) => S::Ready {
            instance_id: *instance_id,
            offer: offer.clone(),
            ready_unix: *ready_unix,
            deadlines: st.deadlines.clone(),
        },
        (
            S::Renting {
                attempt,
                max_attempts,
                ..
            }
            | S::Provisioning {
                attempt,
                max_attempts,
                ..
            },
            E::AttemptFailed { reason, retry },
        ) => {
            if *retry && attempt < max_attempts {
                S::Renting {
                    attempt: attempt + 1,
                    max_attempts: *max_attempts,
                    offer: None,
                }
            } else if *retry {
                S::Failed {
                    reason: format!(
                        "Couldn't get a GPU ready after {max_attempts} tries. Last problem: {reason}."
                    ),
                }
            } else {
                S::Failed {
                    reason: reason.clone(),
                }
            }
        }
        (S::Renting { .. } | S::Provisioning { .. } | S::Ready { .. }, E::StopRequested) => {
            S::Stopping {
                instance_id: state.instance_id(),
            }
        }
        (S::Stopping { .. }, E::StopRequested) => state.clone(),
        (S::Stopping { .. }, E::Destroyed) => S::idle(),
        (S::Provisioning { .. } | S::Ready { .. }, E::InstanceGone { reason }) => S::Idle {
            notice: Some(reason.clone()),
        },
        (S::Idle { .. }, E::Reattach { instance_id, offer }) => S::Provisioning {
            attempt: 1,
            max_attempts: 1,
            instance_id: *instance_id,
            offer: offer.clone(),
            stage: None,
            detail: None,
            progress: None,
        },
        (S::Idle { .. } | S::Failed { .. }, E::DestroyOrphan { instance_id }) => S::Stopping {
            instance_id: Some(*instance_id),
        },
        (_, E::Fail { reason }) => S::Failed {
            reason: reason.clone(),
        },
        (S::Failed { .. } | S::Idle { .. }, E::Dismiss) => S::idle(),
        _ => {
            return Err(InvalidTransition {
                state: name(state).into(),
                event: format!("{event:?}")
                    .split(['(', ' ', '{'])
                    .next()
                    .unwrap_or("?")
                    .into(),
            })
        }
    };
    Ok(out)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// What the driver needs from the app shell. Tests use a recorder.
pub trait Ui: Send + Sync {
    fn state_changed(&self, state: &SessionState);
    /// Already redacted.
    fn log(&self, line: &str);
    fn open_remote(&self, url: Url);
    fn close_remote(&self);
}

#[derive(Debug, Clone)]
pub struct Timing {
    /// Provider + sidecar polling while provisioning.
    pub poll: Duration,
    /// Heartbeat period while Ready.
    pub heartbeat: Duration,
    /// How often to confirm with the provider that a Ready instance exists.
    pub instance_check: Duration,
    /// How long Stop waits for the provider to confirm the instance is gone.
    pub destroy_confirm: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(5),
            heartbeat: Duration::from_secs(60),
            instance_check: Duration::from_secs(120),
            destroy_confirm: Duration::from_secs(90),
        }
    }
}

pub struct Deps {
    pub provider: Arc<dyn GpuProvider>,
    /// "vast" or "mock"; stored in the active record.
    pub provider_name: String,
    pub sidecar: Arc<dyn SidecarApi>,
    pub secrets: Arc<dyn SecretStore>,
    pub records: RecordFile,
    pub ui: Arc<dyn Ui>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Orphan {
    pub instance_id: u64,
    pub gpu_name: Option<String>,
    pub hourly: Option<f64>,
    pub status: Option<String>,
    /// We have its launch secret and its tunnel is published.
    pub can_reattach: bool,
}

struct Active {
    instance_id: u64,
    base: Option<Url>,
    secret: String,
}

struct Inner {
    state: SessionState,
    active: Option<Active>,
    task: Option<JoinHandle<()>>,
    log: VecDeque<String>,
}

enum Provisioned {
    Ready(Url),
    /// Destroy this instance and try the next offer.
    Retry(String),
    /// Destroy this instance and stop trying.
    Fatal(String),
    /// Already gone on the provider side.
    Gone(String),
    Stopped,
}

pub struct SessionManager {
    deps: Deps,
    timing: Timing,
    inner: Mutex<Inner>,
    stop: watch::Sender<bool>,
}

const LOG_LINES: usize = 300;

pub fn new_launch_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn friendly_destroy_reason(r: &str) -> &str {
    match r {
        "idle" => "it was idle too long",
        "heartbeat_lost" => "it lost contact with this app",
        "max_session" => "the maximum session length was reached",
        other => other,
    }
}

impl SessionManager {
    pub fn new(deps: Deps, timing: Timing) -> Arc<Self> {
        Arc::new(Self {
            deps,
            timing,
            inner: Mutex::new(Inner {
                state: SessionState::idle(),
                active: None,
                task: None,
                log: VecDeque::new(),
            }),
            stop: watch::channel(false).0,
        })
    }

    pub fn state(&self) -> SessionState {
        self.inner.lock().unwrap().state.clone()
    }

    pub fn log_lines(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.iter().cloned().collect()
    }

    pub fn is_busy(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .task
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        let line = {
            let inner = self.inner.lock().unwrap();
            let known: Vec<&str> = inner.active.iter().map(|a| a.secret.as_str()).collect();
            redact(msg.as_ref(), &known)
        };
        let stamped = format!("{} {line}", now_unix());
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.log.len() >= LOG_LINES {
                inner.log.pop_front();
            }
            inner.log.push_back(stamped.clone());
        }
        eprintln!("[sloptweak] {stamped}");
        self.deps.ui.log(&stamped);
    }

    /// Apply an event. Returns false (and changes nothing) if it isn't valid
    /// in the current state.
    fn apply(&self, ev: Event) -> bool {
        let new = {
            let mut inner = self.inner.lock().unwrap();
            match next(&inner.state, &ev) {
                Ok(s) => {
                    inner.state = s.clone();
                    s
                }
                Err(e) => {
                    drop(inner);
                    self.log(format!("ignored: {e}"));
                    return false;
                }
            }
        };
        self.deps.ui.state_changed(&new);
        true
    }

    fn spawn(self: &Arc<Self>, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(fut);
        self.inner.lock().unwrap().task = Some(handle);
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// Sleep for `d`; returns true if a stop was requested meanwhile.
    async fn wait(&self, rx: &mut watch::Receiver<bool>, d: Duration) -> bool {
        if *rx.borrow() {
            return true;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => *rx.borrow(),
            r = rx.changed() => r.is_err() || *rx.borrow(),
        }
    }

    // ----- public actions ---------------------------------------------------

    pub fn start(
        self: &Arc<Self>,
        model: Model,
        settings: Settings,
        civitai_token: Option<String>,
    ) -> Result<(), String> {
        if self.is_busy() || self.state().is_active() {
            return Err("A session is already running.".into());
        }
        if model.needs_civitai() && civitai_token.is_none() {
            return Err("This model needs your CivitAI key.".into());
        }
        self.stop.send_replace(false);
        if !self.apply(Event::Start {
            max_attempts: settings.max_attempts,
        }) {
            return Err("Can't start from the current state.".into());
        }
        let me = self.clone();
        self.spawn(async move {
            me.clone().run(model, settings, civitai_token).await;
            me.settle().await;
        });
        Ok(())
    }

    /// Ask the running session to stop. The driver destroys the instance.
    pub fn request_stop(&self) {
        if !self.state().is_active() {
            return;
        }
        self.stop.send_replace(true);
        self.apply(Event::StopRequested);
    }

    /// Stop and wait (up to `limit`) until nothing is running.
    pub async fn stop_and_wait(&self, limit: Duration) -> bool {
        self.request_stop();
        let deadline = Instant::now() + limit;
        while self.is_busy() {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        !self.state().is_active()
    }

    pub fn dismiss(&self) {
        self.apply(Event::Dismiss);
    }

    /// Mint a fresh ticket and (re)open the remote window.
    pub async fn open_invoke(&self) -> Result<(), String> {
        let (base, secret) = {
            let inner = self.inner.lock().unwrap();
            if !matches!(inner.state, SessionState::Ready { .. }) {
                return Err("Invoke isn't ready yet.".into());
            }
            let a = inner.active.as_ref().ok_or("no active instance")?;
            (a.base.clone().ok_or("no tunnel yet")?, a.secret.clone())
        };
        let ticket = self
            .deps
            .sidecar
            .ticket(&base, &secret)
            .await
            .map_err(|e| format!("Couldn't open Invoke: {e}"))?;
        self.deps.ui.open_remote(auth_url(&base, &ticket));
        Ok(())
    }

    // ----- orphans ------------------------------------------------------------

    /// Instances from an earlier run (crash, forced quit) that may still bill.
    /// Also drops a stale record whose instance is already gone.
    pub async fn scan_orphans(&self) -> Result<Vec<Orphan>, ProviderError> {
        let current = self.state().instance_id();
        let record = self
            .deps
            .records
            .load()
            .filter(|r| r.provider == self.deps.provider_name);
        let ours: Vec<InstanceInfo> = self
            .deps
            .provider
            .list_instances()
            .await?
            .into_iter()
            .filter(InstanceInfo::is_ours)
            .collect();
        if let Some(rec) = &record {
            if Some(rec.instance_id) != current && !ours.iter().any(|i| i.id == rec.instance_id) {
                self.log(format!(
                    "previous instance {} is already gone; clearing record",
                    rec.instance_id
                ));
                self.forget(rec.instance_id);
            }
        }
        Ok(ours
            .into_iter()
            .filter(|i| Some(i.id) != current)
            .map(|i| {
                let has_secret = matches!(
                    self.deps.secrets.get(&secrets::launch_secret_name(i.id)),
                    Ok(Some(_))
                );
                Orphan {
                    instance_id: i.id,
                    gpu_name: i.gpu_name.clone(),
                    hourly: i.dph_total,
                    status: i.actual_status.clone(),
                    can_reattach: has_secret && self.deps.provider.sidecar_url(&i).is_some(),
                }
            })
            .collect())
    }

    pub fn destroy_orphan(self: &Arc<Self>, instance_id: u64) -> Result<(), String> {
        if self.is_busy() {
            return Err("Stop the current session first.".into());
        }
        if !self.apply(Event::DestroyOrphan { instance_id }) {
            return Err("Can't do that right now.".into());
        }
        let me = self.clone();
        self.spawn(async move {
            me.log(format!("destroying leftover instance {instance_id}"));
            if me.destroy_confirmed(instance_id).await {
                me.forget(instance_id);
                me.apply(Event::Destroyed);
            } else {
                me.apply(Event::Fail {
                    reason: format!(
                        "Couldn't confirm GPU {instance_id} was shut down. Check the Vast console."
                    ),
                });
            }
        });
        Ok(())
    }

    pub async fn reattach(self: &Arc<Self>, instance_id: u64) -> Result<(), String> {
        if self.is_busy() || self.state().is_active() {
            return Err("Stop the current session first.".into());
        }
        let secret = self
            .deps
            .secrets
            .get(&secrets::launch_secret_name(instance_id))
            .ok()
            .flatten()
            .ok_or("This GPU's login secret is missing; it can only be shut down.")?;
        let info = self
            .deps
            .provider
            .instance(instance_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("That GPU is already gone.")?;
        let offer = OfferSummary {
            offer_id: 0,
            gpu_name: info.gpu_name.clone().unwrap_or_else(|| "GPU".into()),
            hourly: info.dph_total.unwrap_or(0.0),
            download_cost: 0.0,
            location: None,
        };
        self.stop.send_replace(false);
        self.inner.lock().unwrap().active = Some(Active {
            instance_id,
            base: None,
            secret: secret.clone(),
        });
        if !self.apply(Event::Reattach { instance_id, offer }) {
            self.inner.lock().unwrap().active = None;
            return Err("Can't reattach right now.".into());
        }
        let me = self.clone();
        let deadline = Duration::from_secs(5 * 60);
        self.spawn(async move {
            me.log(format!("reattaching to instance {instance_id}"));
            let mut rx = me.stop.subscribe();
            let outcome = me.provision(instance_id, &secret, deadline, &mut rx).await;
            me.after_provision(instance_id, &secret, outcome, &mut rx)
                .await;
            me.settle().await;
        });
        Ok(())
    }

    // ----- the run ------------------------------------------------------------

    async fn run(self: Arc<Self>, model: Model, settings: Settings, civitai: Option<String>) {
        let mut rx = self.stop.subscribe();
        let query = config::offer_query(&model, &settings);
        let costs = config::cost_inputs(&model, &settings);
        let ready_timeout = Duration::from_secs(u64::from(settings.ready_timeout_minutes) * 60);
        let mut tried = offers::Tried::default();
        loop {
            if self.stopped() {
                return self.finish_stop(None).await;
            }
            let attempt = match self.state() {
                SessionState::Renting { attempt, .. } => attempt,
                _ => return,
            };
            let found = match self.deps.provider.search_offers(&query).await {
                Ok(o) => o,
                Err(e) => {
                    self.apply(Event::Fail {
                        reason: format!("Couldn't search for GPUs: {e}"),
                    });
                    return;
                }
            };
            let ranked = offers::rank(&found, &query, &costs, &tried);
            self.log(format!(
                "attempt {attempt}: {} offers returned, {} usable",
                found.len(),
                ranked.len()
            ));
            let Some(best) = ranked.first() else {
                self.apply(Event::Fail {
                    reason: format!(
                        "No GPU matches your settings right now (up to ${:.2}/hr, {} GB VRAM). \
                         Try again in a few minutes or raise the price limit.",
                        settings.max_dph, model.min_vram_gb
                    ),
                });
                return;
            };
            tried.add(&best.offer);
            let summary = OfferSummary::from(best);
            self.log(format!(
                "picked offer {} ({}, {}): ${:.4}/hr, model download ~${:.3}, expected ${:.3}",
                best.offer.id,
                best.offer.gpu_name,
                best.offer.geolocation.as_deref().unwrap_or("?"),
                best.hourly,
                best.download_cost,
                best.expected_cost
            ));
            if !self.apply(Event::OfferPicked(summary.clone())) {
                return self.finish_stop(None).await;
            }

            let secret = new_launch_secret();
            let spec =
                config::launch_spec(&model, &settings, &sha256_hex(&secret), civitai.as_deref());
            let instance_id = match self
                .deps
                .provider
                .create_instance(best.offer.id, &spec)
                .await
            {
                Ok(id) => id,
                Err(ProviderError::OfferUnavailable) => {
                    self.log(format!("offer {} was taken", best.offer.id));
                    if !self.apply(Event::AttemptFailed {
                        reason: "the GPU was rented by someone else".into(),
                        retry: true,
                    }) {
                        return self.finish_stop(None).await;
                    }
                    continue;
                }
                Err(e) => {
                    self.apply(Event::Fail {
                        reason: format!("Couldn't rent the GPU: {e}"),
                    });
                    return;
                }
            };
            // Record before anything else can fail.
            self.remember(instance_id, &summary, &model, &secret);
            self.log(format!(
                "created instance {instance_id} on offer {} at ${:.4}/hr",
                best.offer.id, summary.hourly
            ));
            if !self.apply(Event::InstanceCreated { instance_id }) {
                return self.finish_stop(Some(instance_id)).await;
            }

            let outcome = self
                .provision(instance_id, &secret, ready_timeout, &mut rx)
                .await;
            if !self
                .after_provision(instance_id, &secret, outcome, &mut rx)
                .await
            {
                return;
            }
        }
    }

    /// Returns true if the run loop should try the next offer.
    async fn after_provision(
        &self,
        instance_id: u64,
        secret: &str,
        outcome: Provisioned,
        rx: &mut watch::Receiver<bool>,
    ) -> bool {
        match outcome {
            Provisioned::Ready(base) => {
                self.ready_loop(instance_id, base, secret, rx).await;
                false
            }
            Provisioned::Stopped => {
                self.finish_stop(Some(instance_id)).await;
                false
            }
            Provisioned::Gone(reason) => {
                self.log(format!("instance {instance_id} disappeared: {reason}"));
                self.forget(instance_id);
                self.apply(Event::AttemptFailed {
                    reason,
                    retry: true,
                }) && matches!(self.state(), SessionState::Renting { .. })
            }
            Provisioned::Retry(reason) => self.abandon(instance_id, reason, true).await,
            Provisioned::Fatal(reason) => self.abandon(instance_id, reason, false).await,
        }
    }

    /// Destroy an instance we're giving up on. Returns true to try the next offer.
    async fn abandon(&self, instance_id: u64, reason: String, retry: bool) -> bool {
        self.log(format!("giving up on instance {instance_id}: {reason}"));
        if !self.destroy_confirmed(instance_id).await {
            self.apply(Event::Fail {
                reason: format!(
                    "{reason}. Also couldn't confirm GPU {instance_id} was shut down; \
                     it will shut itself down within a few minutes, but check the Vast console."
                ),
            });
            return false;
        }
        self.forget(instance_id);
        if self.stopped() {
            self.apply(Event::StopRequested);
            self.apply(Event::Destroyed);
            return false;
        }
        self.apply(Event::AttemptFailed { reason, retry })
            && matches!(self.state(), SessionState::Renting { .. })
    }

    async fn provision(
        &self,
        instance_id: u64,
        secret: &str,
        ready_timeout: Duration,
        rx: &mut watch::Receiver<bool>,
    ) -> Provisioned {
        let started = Instant::now();
        let mut base: Option<Url> = None;
        let mut last_stage: Option<(String, Option<String>, Option<i64>)> = None;
        let mut sidecar_errors = 0u32;
        let mut last_host_status: Option<(Option<String>, Option<String>)> = None;
        let mut last_progress = Instant::now();
        loop {
            if started.elapsed() >= ready_timeout {
                return Provisioned::Retry(format!(
                    "it wasn't ready after {} minutes",
                    ready_timeout.as_secs() / 60
                ));
            }
            match self.deps.provider.instance(instance_id).await {
                Ok(None) => return Provisioned::Gone("the GPU host removed the machine".into()),
                Ok(Some(info)) => {
                    let host_status = (info.actual_status.clone(), info.status_msg.clone());
                    if last_host_status.as_ref() != Some(&host_status) {
                        self.log(format!(
                            "host: status={} msg={:?}",
                            host_status.0.as_deref().unwrap_or("none"),
                            host_status.1.as_deref().unwrap_or("").trim()
                        ));
                        last_host_status = Some(host_status);
                        last_progress = Instant::now();
                    }
                    if let Health::Failed(r) =
                        info.health(started.elapsed(), last_progress.elapsed())
                    {
                        return Provisioned::Retry(r);
                    }
                    if base.is_none() {
                        base = self.deps.provider.sidecar_url(&info);
                        if let Some(b) = &base {
                            self.log(format!("tunnel published: {b}"));
                            self.set_base(b.clone());
                        }
                    }
                }
                Err(ProviderError::Auth) => {
                    return Provisioned::Fatal("Vast rejected the API key".into());
                }
                Err(e) => self.log(format!("instance status check failed: {e}")),
            }
            if let Some(b) = &base {
                match self.deps.sidecar.heartbeat(b, secret).await {
                    Ok(st) => {
                        sidecar_errors = 0;
                        if let Some(r) = &st.destroying {
                            return Provisioned::Retry(format!(
                                "the machine shut itself down because {}",
                                friendly_destroy_reason(r)
                            ));
                        }
                        let key = (
                            st.stage.clone(),
                            st.detail.clone(),
                            st.progress.map(|p| p.floor() as i64),
                        );
                        if last_stage.as_ref() != Some(&key) {
                            self.log(format!(
                                "stage {} {} {}",
                                st.stage,
                                st.detail.as_deref().unwrap_or(""),
                                st.progress.map(|p| format!("{p:.0}%")).unwrap_or_default()
                            ));
                            last_stage = Some(key);
                            self.apply(Event::Stage(st.clone()));
                        }
                        match st.stage.as_str() {
                            "ready" => return Provisioned::Ready(b.clone()),
                            "failed" => {
                                return Provisioned::Retry(format!(
                                    "setup failed ({})",
                                    st.detail.as_deref().unwrap_or("no detail")
                                ))
                            }
                            _ => {}
                        }
                    }
                    Err(SidecarError::Unauthorized) => {
                        return Provisioned::Retry("the machine rejected our login secret".into())
                    }
                    Err(e) => {
                        // New tunnel DNS lags ~10 s; keep polling.
                        sidecar_errors += 1;
                        if sidecar_errors == 1 || sidecar_errors.is_multiple_of(12) {
                            self.log(format!("sidecar not reachable yet: {e}"));
                        }
                    }
                }
            }
            if self.wait(rx, self.timing.poll).await {
                return Provisioned::Stopped;
            }
        }
    }

    async fn ready_loop(
        &self,
        instance_id: u64,
        base: Url,
        secret: &str,
        rx: &mut watch::Receiver<bool>,
    ) {
        if !self.apply(Event::Ready {
            at_unix: now_unix(),
        }) {
            return self.finish_stop(Some(instance_id)).await;
        }
        self.log(format!("instance {instance_id} is ready"));
        let mut opened = false;
        for _ in 0..5 {
            match self.deps.sidecar.ticket(&base, secret).await {
                Ok(t) => {
                    self.deps.ui.open_remote(auth_url(&base, &t));
                    opened = true;
                    break;
                }
                Err(e) => self.log(format!("ticket failed: {e}")),
            }
            if self.wait(rx, Duration::from_secs(3)).await {
                return self.finish_stop(Some(instance_id)).await;
            }
        }
        if !opened {
            self.log("couldn't open Invoke automatically; use Open Invoke");
        }
        let mut last_check = Instant::now();
        let mut failures = 0u32;
        loop {
            if self.wait(rx, self.timing.heartbeat).await {
                return self.finish_stop(Some(instance_id)).await;
            }
            match self.deps.sidecar.heartbeat(&base, secret).await {
                Ok(st) => {
                    failures = 0;
                    if let Some(r) = &st.destroying {
                        let reason = format!(
                            "The GPU shut itself down because {}.",
                            friendly_destroy_reason(r)
                        );
                        return self.gone(instance_id, reason).await;
                    }
                    self.apply(Event::Heartbeat(st));
                }
                Err(e) => {
                    failures += 1;
                    self.log(format!("heartbeat failed ({failures}): {e}"));
                }
            }
            if failures > 0 || last_check.elapsed() >= self.timing.instance_check {
                last_check = Instant::now();
                if let Ok(None) = self.deps.provider.instance(instance_id).await {
                    return self
                        .gone(instance_id, "The GPU was shut down by its host.".into())
                        .await;
                }
            }
        }
    }

    /// A stop that raced with the end of a run must still finish.
    async fn settle(&self) {
        if matches!(self.state(), SessionState::Stopping { .. }) {
            self.finish_stop(None).await;
        }
    }

    /// Stop was requested: destroy (if any), confirm, clean up.
    async fn finish_stop(&self, instance_id: Option<u64>) {
        self.apply(Event::StopRequested);
        self.deps.ui.close_remote();
        let Some(id) = instance_id.or_else(|| self.state().instance_id()) else {
            self.apply(Event::Destroyed);
            return;
        };
        self.log(format!("stopping: destroying instance {id}"));
        if self.destroy_confirmed(id).await {
            self.forget(id);
            self.log(format!("instance {id} destroyed"));
            self.apply(Event::Destroyed);
        } else {
            self.apply(Event::Fail {
                reason: format!(
                    "Couldn't confirm GPU {id} was shut down. It will shut itself down within a \
                     few minutes, but please check the Vast console."
                ),
            });
        }
    }

    /// The instance went away (or is going) on its own while in use.
    async fn gone(&self, instance_id: u64, reason: String) {
        self.log(format!("instance {instance_id}: {reason}"));
        // Belt and braces: destroy is idempotent and stops billing now.
        let _ = self.deps.provider.destroy(instance_id).await;
        self.forget(instance_id);
        self.deps.ui.close_remote();
        self.apply(Event::InstanceGone { reason });
    }

    /// Destroy, then poll until the provider no longer lists the instance.
    async fn destroy_confirmed(&self, instance_id: u64) -> bool {
        let deadline = Instant::now() + self.timing.destroy_confirm;
        let mut delay = Duration::from_secs(2);
        loop {
            match self.deps.provider.destroy(instance_id).await {
                Ok(()) => {}
                Err(e) => self.log(format!("destroy {instance_id} failed: {e}")),
            }
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Ok(None) = self.deps.provider.instance(instance_id).await {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(15));
        }
    }

    fn remember(&self, instance_id: u64, offer: &OfferSummary, model: &Model, secret: &str) {
        if let Err(e) = self
            .deps
            .secrets
            .set(&secrets::launch_secret_name(instance_id), secret)
        {
            self.log(format!(
                "couldn't store launch secret (reattach won't work): {e}"
            ));
        }
        let rec = ActiveRecord {
            instance_id,
            offer_id: offer.offer_id,
            gpu_name: offer.gpu_name.clone(),
            hourly: offer.hourly,
            model_id: model.id.clone(),
            created_unix: now_unix(),
            provider: self.deps.provider_name.clone(),
        };
        if let Err(e) = self.deps.records.save(&rec) {
            self.log(format!("couldn't save the instance record: {e}"));
        }
        self.inner.lock().unwrap().active = Some(Active {
            instance_id,
            base: None,
            secret: secret.to_string(),
        });
    }

    fn set_base(&self, base: Url) {
        if let Some(a) = self.inner.lock().unwrap().active.as_mut() {
            a.base = Some(base);
        }
    }

    /// Drop everything we know about an instance that is confirmed gone.
    fn forget(&self, instance_id: u64) {
        let _ = self
            .deps
            .secrets
            .delete(&secrets::launch_secret_name(instance_id));
        if self
            .deps
            .records
            .load()
            .is_some_and(|r| r.instance_id == instance_id)
        {
            let _ = self.deps.records.clear();
        }
        let mut inner = self.inner.lock().unwrap();
        if inner
            .active
            .as_ref()
            .is_some_and(|a| a.instance_id == instance_id)
        {
            inner.active = None;
        }
    }

    /// Test hook: kill the driver without cleanup, like a crash.
    #[cfg(test)]
    pub fn crash(&self) {
        if let Some(t) = self.inner.lock().unwrap().task.take() {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests;
