//! In-memory provider and sidecar with scripted stages and delays, for UI
//! work and tests. Uses tokio time so tests can run with paused time.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use url::Url;

use super::{GpuProvider, InstanceInfo, LaunchSpec, Offer, OfferQuery, ProviderError};
use crate::sidecar::{Deadlines, SidecarApi, SidecarError, SidecarStatus};

/// What a created instance does. Consumed one per create call; `Normal` after.
#[derive(Debug, Clone, PartialEq)]
pub enum MockBehavior {
    Normal,
    /// `status_msg` shows a Docker daemon error while `loading`.
    DaemonError,
    /// Stuck `loading` with `intended_status=stopped`.
    DeadHost,
    /// Stuck `loading` forever with nothing else wrong.
    NeverStarts,
    /// The create call fails: offer gone.
    Unavailable,
    /// Provisioning reports `failed` with this detail.
    ProvisionFails(String),
    /// Becomes ready, then the watchdog destroys it after this long.
    SelfDestructAfterReady(Duration),
    /// A slow host: `loading` with changing pull progress for this long,
    /// then behaves normally.
    SlowPull(Duration),
}

/// Parse `SLOPTWEAK_MOCK_SCRIPT`, e.g. `daemon,dead,normal`, so failure paths
/// can be demoed in the UI. Unknown words are skipped.
pub fn parse_script(script: &str) -> Vec<MockBehavior> {
    script
        .split(',')
        .filter_map(|w| match w.trim() {
            "normal" => Some(MockBehavior::Normal),
            "daemon" => Some(MockBehavior::DaemonError),
            "dead" => Some(MockBehavior::DeadHost),
            "stuck" => Some(MockBehavior::NeverStarts),
            "taken" => Some(MockBehavior::Unavailable),
            "provfail" => Some(MockBehavior::ProvisionFails("download failed: mock".into())),
            "selfdestruct" => Some(MockBehavior::SelfDestructAfterReady(Duration::from_secs(
                120,
            ))),
            "slowpull" => Some(MockBehavior::SlowPull(Duration::from_secs(60))),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct MockTimings {
    pub running_after: Duration,
    pub label_after: Duration,
    /// Sidecar unreachable for this long after the label appears.
    pub dns_lag: Duration,
    pub download_from: Duration,
    pub download_until: Duration,
    pub ready_at: Duration,
}

impl Default for MockTimings {
    fn default() -> Self {
        let s = Duration::from_secs;
        Self {
            running_after: s(4),
            label_after: s(6),
            dns_lag: s(2),
            download_from: s(8),
            download_until: s(20),
            ready_at: s(26),
        }
    }
}

#[derive(Debug)]
struct MockInstance {
    offer_id: u64,
    created: Instant,
    behavior: MockBehavior,
    token_hash: String,
    heartbeats: u32,
    destroyed: bool,
    ready_at: Option<Instant>,
}

impl MockInstance {
    /// Time since creation, minus any simulated slow image pull.
    fn age(&self) -> Duration {
        let raw = self.created.elapsed();
        match self.behavior {
            MockBehavior::SlowPull(d) => raw.saturating_sub(d),
            _ => raw,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    instances: HashMap<u64, MockInstance>,
    behaviors: VecDeque<MockBehavior>,
    created_offers: Vec<u64>,
}

#[derive(Clone)]
pub struct MockProvider {
    state: Arc<Mutex<State>>,
    timings: MockTimings,
    offers: Vec<Offer>,
    credit: f64,
    remote_base: Url,
}

impl MockProvider {
    pub fn new(timings: MockTimings, behaviors: Vec<MockBehavior>) -> Self {
        let state = State {
            next_id: 9_000_001,
            behaviors: behaviors.into(),
            ..Default::default()
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            timings,
            offers: default_offers(),
            credit: 11.55,
            remote_base: Url::parse("https://example.com/").expect("static url"),
        }
    }

    /// Credit reported by `credit()` (for the low-balance gate in the UI).
    pub fn with_credit(mut self, credit: f64) -> Self {
        self.credit = credit;
        self
    }

    /// Page the remote window opens in mock mode.
    pub fn with_remote_base(mut self, base: Url) -> Self {
        self.remote_base = base;
        self
    }

    pub fn sidecar(&self) -> MockSidecar {
        MockSidecar {
            state: self.state.clone(),
            timings: self.timings.clone(),
        }
    }

    #[cfg(test)]
    pub fn created_offers(&self) -> Vec<u64> {
        self.state.lock().unwrap().created_offers.clone()
    }

    #[cfg(test)]
    pub fn live_ids(&self) -> Vec<u64> {
        let s = self.state.lock().unwrap();
        let mut ids: Vec<u64> = s
            .instances
            .iter()
            .filter(|(_, i)| !i.destroyed)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[cfg(test)]
    pub fn heartbeats(&self, id: u64) -> u32 {
        self.state
            .lock()
            .unwrap()
            .instances
            .get(&id)
            .map_or(0, |i| i.heartbeats)
    }

    fn info(&self, id: u64, inst: &MockInstance) -> InstanceInfo {
        let t = &self.timings;
        let age = inst.age();
        let mut info = InstanceInfo {
            id,
            actual_status: Some("loading".into()),
            intended_status: Some("running".into()),
            cur_state: Some("running".into()),
            label: Some(super::LABEL.into()),
            dph_total: self
                .offers
                .iter()
                .find(|o| o.id == inst.offer_id)
                .map(|o| o.dph_total + 0.012),
            gpu_name: Some("MOCK GPU".into()),
            ..Default::default()
        };
        match inst.behavior {
            MockBehavior::DaemonError if age >= Duration::from_secs(2) => {
                info.status_msg = Some("Error response from daemon: manifest unknown".into());
            }
            MockBehavior::DeadHost if age >= Duration::from_secs(5) => {
                info.intended_status = Some("stopped".into());
                info.cur_state = Some("stopped".into());
            }
            MockBehavior::DaemonError | MockBehavior::DeadHost | MockBehavior::NeverStarts => {}
            MockBehavior::SlowPull(d) if inst.created.elapsed() < d => {
                let secs = inst.created.elapsed().as_secs();
                info.status_msg = Some(format!("layer{}: Downloading {}s", secs / 20, secs));
            }
            _ if age >= t.running_after => {
                info.actual_status = Some("running".into());
                if age >= t.label_after {
                    info.label = Some(format!("sloptweak:mock-{id}.trycloudflare.com"));
                }
            }
            _ => {}
        }
        info
    }

    fn reap(&self, s: &mut State) {
        // Simulate the instance watchdog destroying itself.
        for inst in s.instances.values_mut() {
            if let (MockBehavior::SelfDestructAfterReady(d), Some(r)) =
                (&inst.behavior, inst.ready_at)
            {
                if r.elapsed() >= *d + Duration::from_secs(10) {
                    inst.destroyed = true;
                }
            }
        }
    }
}

fn default_offers() -> Vec<Offer> {
    let mk = |id, gpu: &str, dph, down, rel| Offer {
        id,
        gpu_name: gpu.into(),
        gpu_ram_mb: 16376.0,
        num_gpus: 1,
        dph_total: dph,
        storage_cost: 0.2,
        inet_down_cost: down,
        inet_down_mbps: 2500.0,
        disk_bw_mbps: 3000.0,
        reliability: rel,
        verified: true,
        disk_space_gb: 120.0,
        cuda_max_good: 12.8,
        compute_cap: 860,
        geolocation: Some("Mockland".into()),
        machine_id: Some(id * 10),
    };
    vec![
        mk(101, "RTX A4000", 0.0825, 0.039, 0.996),
        mk(102, "RTX 4060 Ti", 0.1489, 0.0026, 0.992),
        mk(103, "RTX A4500", 0.1076, 0.0026, 0.993),
        mk(104, "RTX 3090", 0.1907, 0.0, 0.995),
    ]
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[async_trait]
impl GpuProvider for MockProvider {
    async fn credit(&self) -> Result<f64, ProviderError> {
        Ok(self.credit)
    }

    async fn search_offers(&self, _q: &OfferQuery) -> Result<Vec<Offer>, ProviderError> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(self.offers.clone())
    }

    async fn create_instance(
        &self,
        offer_id: u64,
        spec: &LaunchSpec,
    ) -> Result<u64, ProviderError> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut s = self.state.lock().unwrap();
        let behavior = s.behaviors.pop_front().unwrap_or(MockBehavior::Normal);
        s.created_offers.push(offer_id);
        if behavior == MockBehavior::Unavailable {
            return Err(ProviderError::OfferUnavailable);
        }
        let token_hash = spec
            .env
            .iter()
            .find(|(k, _)| k == "LAUNCH_TOKEN_HASH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let id = s.next_id;
        s.next_id += 1;
        s.instances.insert(
            id,
            MockInstance {
                offer_id,
                created: Instant::now(),
                behavior,
                token_hash,
                heartbeats: 0,
                destroyed: false,
                ready_at: None,
            },
        );
        Ok(id)
    }

    async fn instance(&self, id: u64) -> Result<Option<InstanceInfo>, ProviderError> {
        let mut s = self.state.lock().unwrap();
        self.reap(&mut s);
        Ok(s.instances
            .get(&id)
            .filter(|i| !i.destroyed)
            .map(|i| self.info(id, i)))
    }

    async fn list_instances(&self) -> Result<Vec<InstanceInfo>, ProviderError> {
        let mut s = self.state.lock().unwrap();
        self.reap(&mut s);
        let mut out: Vec<InstanceInfo> = s
            .instances
            .iter()
            .filter(|(_, i)| !i.destroyed)
            .map(|(id, i)| self.info(*id, i))
            .collect();
        out.sort_by_key(|i| i.id);
        Ok(out)
    }

    async fn destroy(&self, id: u64) -> Result<(), ProviderError> {
        let mut s = self.state.lock().unwrap();
        if let Some(i) = s.instances.get_mut(&id) {
            i.destroyed = true;
        }
        Ok(())
    }

    fn sidecar_url(&self, info: &InstanceInfo) -> Option<Url> {
        info.label_host().map(|_| self.remote_base.clone())
    }
}

/// Fake sidecar. Finds the instance by the launch secret's hash, like the
/// real one authenticates.
#[derive(Clone)]
pub struct MockSidecar {
    state: Arc<Mutex<State>>,
    timings: MockTimings,
}

impl MockSidecar {
    fn with_instance<T>(
        &self,
        secret: &str,
        f: impl FnOnce(&mut MockInstance, &MockTimings) -> Result<T, SidecarError>,
    ) -> Result<T, SidecarError> {
        let hash = sha256_hex(secret);
        let mut s = self.state.lock().unwrap();
        let inst = s
            .instances
            .values_mut()
            .find(|i| i.token_hash == hash && !i.destroyed)
            .ok_or_else(|| SidecarError::Unreachable("no such tunnel".into()))?;
        let age = inst.age();
        let t = &self.timings;
        if matches!(
            inst.behavior,
            MockBehavior::DaemonError | MockBehavior::DeadHost | MockBehavior::NeverStarts
        ) || age < t.label_after + t.dns_lag
        {
            return Err(SidecarError::Unreachable("dns lag".into()));
        }
        f(inst, t)
    }
}

fn stage_at(inst: &mut MockInstance, t: &MockTimings) -> SidecarStatus {
    let age = inst.age();
    let (stage, detail, progress) = if let MockBehavior::ProvisionFails(d) = &inst.behavior {
        if age >= t.download_from {
            ("failed", Some(d.clone()), None)
        } else {
            ("booting", Some("starting services".into()), None)
        }
    } else if age < t.download_from {
        ("booting", Some("starting services".into()), None)
    } else if age < t.download_until {
        let span = (t.download_until - t.download_from).as_secs_f64();
        let done = (age - t.download_from).as_secs_f64();
        (
            "downloading",
            Some("mock-model.safetensors".into()),
            Some((done / span * 100.0).min(100.0)),
        )
    } else if age < t.ready_at {
        ("registering", Some("mock-model.safetensors".into()), None)
    } else {
        inst.ready_at.get_or_insert_with(Instant::now);
        ("ready", None, None)
    };
    let destroying = match (&inst.behavior, inst.ready_at) {
        (MockBehavior::SelfDestructAfterReady(d), Some(r)) if r.elapsed() >= *d => {
            Some("idle".to_string())
        }
        _ => None,
    };
    SidecarStatus {
        stage: stage.into(),
        detail,
        progress,
        destroying,
        deadlines: Some(Deadlines {
            heartbeat_s: Some(600.0),
            idle_s: None,
            max_session_s: Some(14_400.0 - age.as_secs_f64()),
        }),
    }
}

#[async_trait]
impl SidecarApi for MockSidecar {
    async fn heartbeat(&self, _base: &Url, secret: &str) -> Result<SidecarStatus, SidecarError> {
        self.with_instance(secret, |inst, t| {
            inst.heartbeats += 1;
            Ok(stage_at(inst, t))
        })
    }

    async fn ticket(&self, _base: &Url, secret: &str) -> Result<String, SidecarError> {
        self.with_instance(secret, |_, _| Ok("mock-ticket".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_parsing() {
        assert_eq!(
            parse_script("daemon, dead,bogus,normal"),
            vec![
                MockBehavior::DaemonError,
                MockBehavior::DeadHost,
                MockBehavior::Normal
            ]
        );
        assert!(parse_script("").is_empty());
    }
}
