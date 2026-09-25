//! Vast.ai REST client (API v0). Facts used here are in docs/findings.md §1.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{GpuProvider, InstanceInfo, LaunchSpec, Offer, OfferQuery, ProviderError};

pub const API_BASE: &str = "https://console.vast.ai/api/v0";

pub struct VastProvider {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl std::fmt::Debug for VastProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VastProvider")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl VastProvider {
    pub fn new(key: String) -> Self {
        Self::with_base(key, API_BASE.to_string())
    }

    pub fn with_base(key: String, base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self { http, base, key }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), ProviderError> {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .bearer_auth(&self.key);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.without_url().to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Network(e.without_url().to_string()))?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ProviderError::Auth);
        }
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(truncate(&text, 200)))
        };
        Ok((status, value))
    }

    async fn ok(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ProviderError> {
        let (status, value) = self.call(method, path, body).await?;
        if !status.is_success() {
            return Err(http_error(status, &value));
        }
        Ok(value)
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn http_error(status: StatusCode, body: &Value) -> ProviderError {
    let message = body
        .get("msg")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| truncate(&body.to_string(), 200));
    ProviderError::Http {
        status: status.as_u16(),
        message,
    }
}

/// The `/bundles/` search body.
pub fn search_body(q: &OfferQuery) -> Value {
    json!({
        "verified": {"eq": true},
        "rentable": {"eq": true},
        "rented": {"eq": false},
        "type": "ondemand",
        "num_gpus": {"eq": 1},
        "reliability": {"gte": q.min_reliability},
        "inet_down": {"gte": q.min_inet_down_mbps},
        // gpu_ram is MB; see offers::passes for the 12 GB rounding.
        "gpu_ram": {"gte": q.min_vram_gb * 1000.0},
        "disk_space": {"gte": q.min_disk_gb},
        "dph_total": {"lte": q.max_dph},
        "cuda_max_good": {"gte": q.min_cuda},
        "order": [["dph_total", "asc"]],
        "limit": q.limit,
    })
}

/// Vast takes env as one Docker-flag string. Reject anything that could
/// break out of `-e K=V`.
pub fn env_string(env: &[(String, String)]) -> Result<String, ProviderError> {
    let mut parts = Vec::with_capacity(env.len());
    for (k, v) in env {
        let key_ok = !k.is_empty()
            && k.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        let val_ok = v
            .chars()
            .all(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\' | '`' | '$'));
        if !key_ok || !val_ok {
            return Err(ProviderError::Parse(format!("unsafe env entry {k}")));
        }
        parts.push(format!("-e {k}={v}"));
    }
    Ok(parts.join(" "))
}

pub fn create_body(spec: &LaunchSpec) -> Result<Value, ProviderError> {
    Ok(json!({
        "image": spec.image,
        "disk": spec.disk_gb,
        "runtype": "ssh_direct",
        "label": spec.label,
        "env": env_string(&spec.env)?,
        "onstart": spec.onstart,
        "cancel_unavail": true,
    }))
}

#[derive(Deserialize)]
struct RawOffer {
    id: u64,
    #[serde(default)]
    gpu_name: String,
    #[serde(default)]
    gpu_ram: f64,
    #[serde(default)]
    num_gpus: u32,
    #[serde(default)]
    dph_total: f64,
    #[serde(default)]
    storage_cost: Option<f64>,
    #[serde(default)]
    inet_down_cost: Option<f64>,
    #[serde(default)]
    inet_down: f64,
    #[serde(default)]
    reliability2: Option<f64>,
    #[serde(default)]
    reliability: Option<f64>,
    #[serde(default)]
    verified: Option<bool>,
    #[serde(default)]
    verification: Option<String>,
    #[serde(default)]
    disk_space: f64,
    #[serde(default)]
    cuda_max_good: f64,
    #[serde(default)]
    geolocation: Option<String>,
    #[serde(default)]
    machine_id: Option<u64>,
}

pub fn parse_offers(v: &Value) -> Result<Vec<Offer>, ProviderError> {
    let arr = v
        .get("offers")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("no offers array".into()))?;
    Ok(arr
        .iter()
        .filter_map(|o| serde_json::from_value::<RawOffer>(o.clone()).ok())
        .map(|o| Offer {
            id: o.id,
            gpu_name: o.gpu_name,
            gpu_ram_mb: o.gpu_ram,
            num_gpus: o.num_gpus,
            dph_total: o.dph_total,
            storage_cost: o.storage_cost.unwrap_or(0.0),
            // Unknown bandwidth price: assume the worst seen live ($0.04/GB).
            inet_down_cost: o.inet_down_cost.unwrap_or(0.04),
            inet_down_mbps: o.inet_down,
            reliability: o.reliability2.or(o.reliability).unwrap_or(0.0),
            verified: o.verified.unwrap_or(false) || o.verification.as_deref() == Some("verified"),
            disk_space_gb: o.disk_space,
            cuda_max_good: o.cuda_max_good,
            geolocation: o.geolocation,
            machine_id: o.machine_id,
        })
        .collect())
}

fn parse_instance(v: &Value) -> Option<InstanceInfo> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(InstanceInfo {
        id: v.get("id")?.as_u64()?,
        actual_status: s("actual_status"),
        intended_status: s("intended_status"),
        cur_state: s("cur_state"),
        status_msg: s("status_msg"),
        label: s("label"),
        dph_total: v.get("dph_total").and_then(Value::as_f64),
        gpu_name: s("gpu_name"),
    })
}

fn is_unavailable(body: &Value) -> bool {
    let text = body.to_string().to_ascii_lowercase();
    [
        "no_such_ask",
        "not available",
        "unavailable",
        "already rented",
    ]
    .iter()
    .any(|m| text.contains(m))
}

#[async_trait]
impl GpuProvider for VastProvider {
    async fn credit(&self) -> Result<f64, ProviderError> {
        let v = self.ok(Method::GET, "/users/current", None).await?;
        // Findings §1: prepaid money is in `credit`; `balance` stays 0.
        v.get("credit")
            .and_then(Value::as_f64)
            .ok_or_else(|| ProviderError::Parse("no credit field".into()))
    }

    async fn search_offers(&self, q: &OfferQuery) -> Result<Vec<Offer>, ProviderError> {
        let v = self
            .ok(Method::POST, "/bundles/", Some(&search_body(q)))
            .await?;
        parse_offers(&v)
    }

    async fn create_instance(
        &self,
        offer_id: u64,
        spec: &LaunchSpec,
    ) -> Result<u64, ProviderError> {
        let body = create_body(spec)?;
        let (status, v) = self
            .call(Method::PUT, &format!("/asks/{offer_id}/"), Some(&body))
            .await?;
        // The response also carries `instance_api_key`; never keep or log it.
        let id = v.get("new_contract").and_then(Value::as_u64);
        match (
            status.is_success(),
            v.get("success").and_then(Value::as_bool),
            id,
        ) {
            (true, Some(true), Some(id)) => Ok(id),
            _ if is_unavailable(&v) => Err(ProviderError::OfferUnavailable),
            (true, _, _) => Err(ProviderError::Parse("create did not return an id".into())),
            _ => Err(http_error(status, &v)),
        }
    }

    async fn instance(&self, id: u64) -> Result<Option<InstanceInfo>, ProviderError> {
        let (status, v) = self
            .call(Method::GET, &format!("/instances/{id}/"), None)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(http_error(status, &v));
        }
        Ok(v.get("instances").and_then(parse_instance))
    }

    async fn list_instances(&self) -> Result<Vec<InstanceInfo>, ProviderError> {
        let v = self.ok(Method::GET, "/instances/", None).await?;
        Ok(v.get("instances")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(parse_instance).collect())
            .unwrap_or_default())
    }

    async fn destroy(&self, id: u64) -> Result<(), ProviderError> {
        let (status, v) = self
            .call(Method::DELETE, &format!("/instances/{id}/"), None)
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(http_error(status, &v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_string_rejects_injection() {
        let ok = env_string(&[
            ("A_B".into(), "abc123==".into()),
            ("URL".into(), "https://x/y?z=1".into()),
        ])
        .unwrap();
        assert_eq!(ok, "-e A_B=abc123== -e URL=https://x/y?z=1");
        for (k, v) in [
            ("A", "x -p 22:22"),
            ("A", "x\"y"),
            ("A", "$(id)"),
            ("a", "x"),
            ("A B", "x"),
            ("", "x"),
        ] {
            assert!(env_string(&[(k.into(), v.into())]).is_err(), "{k}={v}");
        }
    }

    #[test]
    fn create_body_has_no_ports_and_ssh_direct() {
        let spec = LaunchSpec {
            image: "img@sha256:x".into(),
            disk_gb: 50,
            label: "sloptweak".into(),
            env: vec![("K".into(), "v".into())],
            onstart: "#!/bin/bash".into(),
        };
        let b = create_body(&spec).unwrap();
        assert_eq!(b["runtype"], "ssh_direct");
        assert_eq!(b["label"], "sloptweak");
        assert_eq!(b["env"], "-e K=v");
        assert!(!b["env"].as_str().unwrap().contains("-p "));
        assert_eq!(b["cancel_unavail"], true);
    }

    #[test]
    fn parses_live_shaped_offer() {
        let v = json!({"offers": [{
            "id": 48328454, "gpu_name": "RTX A4000", "gpu_ram": 16376, "num_gpus": 1,
            "dph_total": 0.1089, "storage_cost": 0.2, "inet_down_cost": 0.026041666666666668,
            "inet_down": 6506.2, "reliability": 0.9986, "reliability2": 0.9986468,
            "verification": "verified", "disk_space": 120.5, "cuda_max_good": 12.8,
            "geolocation": "Delaware, US", "extra": [1, 2]
        }, {"no_id": true}]});
        let offers = parse_offers(&v).unwrap();
        assert_eq!(offers.len(), 1);
        let o = &offers[0];
        assert_eq!(o.id, 48328454);
        assert!(o.verified);
        assert_eq!(o.gpu_ram_mb, 16376.0);
        assert!((o.reliability - 0.9986468).abs() < 1e-9);
    }

    #[test]
    fn missing_bandwidth_price_assumes_worst() {
        let v = json!({"offers": [{"id": 1}]});
        assert_eq!(parse_offers(&v).unwrap()[0].inet_down_cost, 0.04);
    }

    #[test]
    fn search_body_filters() {
        let q = OfferQuery {
            min_vram_gb: 12.0,
            min_disk_gb: 50.0,
            min_reliability: 0.98,
            min_inet_down_mbps: 500.0,
            max_dph: 0.5,
            min_cuda: 12.4,
            limit: 64,
        };
        let b = search_body(&q);
        assert_eq!(b["verified"]["eq"], true);
        assert_eq!(b["gpu_ram"]["gte"], 12000.0);
        assert_eq!(b["dph_total"]["lte"], 0.5);
        assert_eq!(b["type"], "ondemand");
        // Never filter by id (findings: false negatives).
        assert!(b.get("id").is_none());
    }

    #[test]
    fn debug_hides_key() {
        let p = VastProvider::new("k".repeat(64));
        assert!(!format!("{p:?}").contains("kkkk"));
    }

    #[test]
    fn unavailable_detection() {
        assert!(is_unavailable(
            &json!({"success": false, "error": "no_such_ask", "msg": "Instance type no longer available"})
        ));
        assert!(!is_unavailable(
            &json!({"success": false, "error": "invalid_args"})
        ));
    }
}
