//! GPU rental providers. Every Vast call goes through [`GpuProvider`] so the
//! session logic can run against [`mock::MockProvider`] without spending money.

pub mod machines;
pub mod mock;
pub mod offers;
pub mod vast;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

/// Label every SlopTweak instance carries. The instance rewrites it to
/// `sloptweak:<tunnel host>` once its tunnel is up.
pub const LABEL: &str = "sloptweak";

/// A rentable machine, normalised from the provider's search results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Offer {
    pub id: u64,
    pub gpu_name: String,
    /// Per-GPU VRAM in MB (Vast reports MB).
    pub gpu_ram_mb: f64,
    pub num_gpus: u32,
    /// $/hr for compute, excluding storage.
    pub dph_total: f64,
    /// $/GB/month of disk.
    pub storage_cost: f64,
    /// $/GB downloaded.
    pub inet_down_cost: f64,
    pub inet_down_mbps: f64,
    /// Disk write speed, MB/s (Vast `disk_bw`). Unpacking the image is
    /// disk-bound on a cold host.
    pub disk_bw_mbps: f64,
    pub reliability: f64,
    pub verified: bool,
    pub disk_space_gb: f64,
    pub cuda_max_good: f64,
    /// GPU architecture, Vast units (e.g. 860 = sm_86). 0 if unknown.
    pub compute_cap: u32,
    pub geolocation: Option<String>,
    /// Physical machine; one machine can list several offers.
    pub machine_id: Option<u64>,
}

/// Server-side search filters. [`offers::rank`] re-applies them client-side.
#[derive(Debug, Clone, PartialEq)]
pub struct OfferQuery {
    pub min_vram_gb: f64,
    pub min_disk_gb: f64,
    pub min_reliability: f64,
    pub min_inet_down_mbps: f64,
    pub min_disk_bw_mbps: f64,
    pub max_dph: f64,
    pub min_cuda: f64,
    /// Vast units (750 = Turing / RTX 20-series).
    pub min_compute_cap: u32,
    pub limit: u32,
}

/// Everything needed to create an instance on a chosen offer.
#[derive(Clone, PartialEq)]
pub struct LaunchSpec {
    pub image: String,
    pub disk_gb: u32,
    pub label: String,
    /// Passed as `-e K=V`. Values must not contain whitespace or quotes.
    pub env: Vec<(String, String)>,
    pub onstart: String,
}

impl std::fmt::Debug for LaunchSpec {
    // The env holds the CivitAI token; never print values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("image", &self.image)
            .field("disk_gb", &self.disk_gb)
            .field("label", &self.label)
            .field(
                "env_keys",
                &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub id: u64,
    pub actual_status: Option<String>,
    pub intended_status: Option<String>,
    pub cur_state: Option<String>,
    pub status_msg: Option<String>,
    pub label: Option<String>,
    pub dph_total: Option<f64>,
    pub gpu_name: Option<String>,
}

/// How an instance looks from the provider's side while we wait for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Starting,
    Running,
    /// Give up on this instance and try another offer.
    Failed(String),
}

/// Hard cap on `loading`. Slow (cheap) hosts may take this long to pull the
/// image; it leaves ~3 min of the 15-min attempt budget for provisioning
/// (measured 159 s live).
pub const MAX_LOADING: Duration = Duration::from_secs(12 * 60);

/// Give up sooner if `loading` shows no progress (unchanged `status_msg`) for
/// this long. Dead hosts sit silent with an empty `status_msg`.
pub const MAX_STALL: Duration = Duration::from_secs(5 * 60);

/// Stall limit once the host has shown Docker pull output. Live (Phase 3,
/// two KR hosts): after `…: Pull complete` of an early layer, `status_msg`
/// stayed unchanged for 5+ minutes while the big layer was unpacked, so the
/// 5-minute rule killed working hosts. [`MAX_LOADING`] still caps the wait.
pub const MAX_STALL_PULLING: Duration = Duration::from_secs(10 * 60);

/// `status_msg` looks like `docker pull` progress.
fn is_pull_output(msg: &str) -> bool {
    [
        ": Pull complete",
        ": Download complete",
        ": Downloading",
        ": Extracting",
        ": Verifying Checksum",
        ": Waiting",
        ": Pulling fs layer",
        ": Already exists",
    ]
    .iter()
    .any(|m| msg.contains(m))
}

impl InstanceInfo {
    pub fn is_ours(&self) -> bool {
        self.label
            .as_deref()
            .is_some_and(|l| l == LABEL || l.starts_with("sloptweak:"))
    }

    /// The raw host part of a `sloptweak:<host>` label, unvalidated.
    pub fn label_host(&self) -> Option<&str> {
        self.label.as_deref()?.strip_prefix("sloptweak:")
    }

    /// Classify the provider-side state. `waited` is how long we've been
    /// waiting for this instance to start; `stalled` is how long its
    /// status has been unchanged.
    pub fn health(&self, waited: Duration, stalled: Duration) -> Health {
        let msg = self.status_msg.as_deref().unwrap_or("");
        if msg.contains("Error response from daemon") {
            return Health::Failed("the GPU host could not start the app image".into());
        }
        let actual = self.actual_status.as_deref().unwrap_or("");
        if actual == "running" {
            return Health::Running;
        }
        if matches!(actual, "exited" | "offline" | "stopped") {
            return Health::Failed(format!("the GPU host reported the machine as {actual}"));
        }
        if self.intended_status.as_deref() == Some("stopped") {
            return Health::Failed("the GPU host stopped the machine before it started".into());
        }
        if waited >= MAX_LOADING {
            return Health::Failed("the GPU host took too long to start the machine".into());
        }
        let stall_limit = if is_pull_output(msg) {
            MAX_STALL_PULLING
        } else {
            MAX_STALL
        };
        if stalled >= stall_limit {
            return Health::Failed(
                "the GPU host stopped making progress starting the machine".into(),
            );
        }
        Health::Starting
    }
}

/// Accept only `<name>.trycloudflare.com` from an instance label. The label is
/// writable by the instance (and so by its host), and we send the launch
/// secret to whatever it points at.
pub fn trycloudflare_url(host: &str) -> Option<Url> {
    let name = host.strip_suffix(".trycloudflare.com")?;
    let ok = !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return None;
    }
    Url::parse(&format!("https://{host}/")).ok()
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum ProviderError {
    #[error("the Vast API key was rejected")]
    Auth,
    #[error("that GPU offer is no longer available")]
    OfferUnavailable,
    #[error("Vast API error (HTTP {status}): {message}")]
    Http { status: u16, message: String },
    #[error("could not reach Vast: {0}")]
    Network(String),
    #[error("unexpected response from Vast: {0}")]
    Parse(String),
}

#[async_trait]
pub trait GpuProvider: Send + Sync {
    /// Prepaid credit in dollars (Vast: `credit`, not `balance`).
    async fn credit(&self) -> Result<f64, ProviderError>;
    async fn search_offers(&self, query: &OfferQuery) -> Result<Vec<Offer>, ProviderError>;
    /// Returns the new instance id.
    async fn create_instance(&self, offer_id: u64, spec: &LaunchSpec)
        -> Result<u64, ProviderError>;
    /// `None` once the instance is gone.
    async fn instance(&self, id: u64) -> Result<Option<InstanceInfo>, ProviderError>;
    async fn list_instances(&self) -> Result<Vec<InstanceInfo>, ProviderError>;
    /// Succeeds if the instance is already gone.
    async fn destroy(&self, id: u64) -> Result<(), ProviderError>;

    /// Where the instance's sidecar can be reached, once published.
    fn sidecar_url(&self, info: &InstanceInfo) -> Option<Url> {
        trycloudflare_url(info.label_host()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(actual: Option<&str>, intended: Option<&str>, msg: Option<&str>) -> InstanceInfo {
        InstanceInfo {
            id: 1,
            actual_status: actual.map(Into::into),
            intended_status: intended.map(Into::into),
            status_msg: msg.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn health_daemon_error_fails_immediately() {
        let i = info(
            Some("loading"),
            Some("running"),
            Some("Error response from daemon: manifest unknown"),
        );
        assert!(matches!(
            i.health(Duration::from_secs(5), Duration::ZERO),
            Health::Failed(_)
        ));
    }

    #[test]
    fn health_dead_host_fails() {
        // Findings §1: loading forever with intended_status=stopped.
        let i = info(Some("loading"), Some("stopped"), None);
        assert!(matches!(
            i.health(Duration::from_secs(30), Duration::ZERO),
            Health::Failed(_)
        ));
        let i = info(None, Some("stopped"), Some(""));
        assert!(matches!(
            i.health(Duration::from_secs(30), Duration::ZERO),
            Health::Failed(_)
        ));
    }

    #[test]
    fn health_loading_cap() {
        let i = info(Some("loading"), Some("running"), None);
        assert_eq!(
            i.health(Duration::from_secs(60), Duration::ZERO),
            Health::Starting
        );
        assert!(matches!(
            i.health(MAX_LOADING, Duration::ZERO),
            Health::Failed(_)
        ));
    }

    #[test]
    fn slow_pull_with_progress_is_allowed_past_8_minutes() {
        // Live: 560-868 Mbps hosts were still pulling the image at 8 min.
        let i = info(
            Some("loading"),
            Some("running"),
            Some("7e99b218d89c: Downloading"),
        );
        let ten_min = Duration::from_secs(10 * 60);
        assert_eq!(i.health(ten_min, Duration::from_secs(20)), Health::Starting);
    }

    #[test]
    fn stalled_loading_fails_early() {
        let i = info(Some("loading"), Some("running"), None);
        let six_min = Duration::from_secs(6 * 60);
        assert!(matches!(i.health(six_min, MAX_STALL), Health::Failed(_)));
        let almost = MAX_STALL - Duration::from_secs(1);
        assert_eq!(i.health(six_min, almost), Health::Starting);
    }

    #[test]
    fn silent_unpacking_after_pull_output_is_not_a_stall() {
        // Live: unchanged for 5+ min after this line on two working hosts.
        let i = info(
            Some("loading"),
            Some("running"),
            Some("6e2e642403ce: Pull complete"),
        );
        let seven = Duration::from_secs(7 * 60);
        assert_eq!(i.health(seven, MAX_STALL), Health::Starting);
        assert!(matches!(
            i.health(seven + MAX_STALL, MAX_STALL_PULLING),
            Health::Failed(_)
        ));
        // Still bounded by the hard cap.
        assert!(matches!(
            i.health(MAX_LOADING, MAX_STALL),
            Health::Failed(_)
        ));
    }

    #[test]
    fn health_running() {
        let i = info(Some("running"), Some("running"), Some("success, running"));
        assert_eq!(i.health(MAX_LOADING * 2, Duration::ZERO), Health::Running);
    }

    #[test]
    fn health_exited_fails() {
        let i = info(Some("exited"), Some("running"), None);
        assert!(matches!(
            i.health(Duration::ZERO, Duration::ZERO),
            Health::Failed(_)
        ));
    }

    #[test]
    fn tunnel_label_validation() {
        assert_eq!(
            trycloudflare_url("thunder-west-textile-brothers.trycloudflare.com")
                .unwrap()
                .as_str(),
            "https://thunder-west-textile-brothers.trycloudflare.com/"
        );
        for bad in [
            "evil.com",
            "trycloudflare.com",
            ".trycloudflare.com",
            "a.b.trycloudflare.com",
            "evil.com/x.trycloudflare.com",
            "evil.com#.trycloudflare.com",
            "user@x.trycloudflare.com",
            "X.trycloudflare.com",
            "-x.trycloudflare.com",
            "x.trycloudflare.com:444",
        ] {
            assert!(trycloudflare_url(bad).is_none(), "{bad} should be rejected");
        }
    }

    #[test]
    fn ours_by_label() {
        let mut i = InstanceInfo::default();
        assert!(!i.is_ours());
        i.label = Some("sloptweak".into());
        assert!(i.is_ours());
        i.label = Some("sloptweak:abc.trycloudflare.com".into());
        assert!(i.is_ours());
        assert_eq!(i.label_host(), Some("abc.trycloudflare.com"));
        i.label = Some("sloptweakish".into());
        assert!(!i.is_ours());
    }

    #[test]
    fn launch_spec_debug_hides_env_values() {
        let spec = LaunchSpec {
            image: "img".into(),
            disk_gb: 50,
            label: LABEL.into(),
            env: vec![("CIVITAI_TOKEN".into(), "supersecretvalue".into())],
            onstart: String::new(),
        };
        let dbg = format!("{spec:?}");
        assert!(dbg.contains("CIVITAI_TOKEN"));
        assert!(!dbg.contains("supersecretvalue"));
    }
}
