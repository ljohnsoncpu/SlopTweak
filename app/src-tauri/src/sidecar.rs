//! Client for the instance sidecar (instance/sidecar.py). All calls carry
//! `Authorization: Bearer <launch secret>`.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Deadlines {
    pub heartbeat_s: Option<f64>,
    pub idle_s: Option<f64>,
    pub max_session_s: Option<f64>,
}

/// `/__status` and `/__heartbeat` payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SidecarStatus {
    /// booting | downloading | verifying | starting | registering | ready | failed
    pub stage: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub progress: Option<f64>,
    /// Set once the watchdog has decided to destroy the instance.
    #[serde(default)]
    pub destroying: Option<String>,
    #[serde(default)]
    pub deadlines: Option<Deadlines>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum SidecarError {
    /// Not reachable yet (new tunnel DNS lags ~10 s) or gone.
    #[error("instance not reachable: {0}")]
    Unreachable(String),
    #[error("instance rejected the launch secret")]
    Unauthorized,
    #[error("instance returned HTTP {0}")]
    Http(u16),
    #[error("bad response from instance: {0}")]
    Parse(String),
}

#[async_trait]
pub trait SidecarApi: Send + Sync {
    async fn heartbeat(&self, base: &Url, secret: &str) -> Result<SidecarStatus, SidecarError>;
    /// Mint a single-use, 60 s webview login ticket.
    async fn ticket(&self, base: &Url, secret: &str) -> Result<String, SidecarError>;
}

/// URL the webview opens to trade a ticket for a session cookie.
pub fn auth_url(base: &Url, ticket: &str) -> Url {
    let mut u = base.join("/__auth").expect("static path");
    u.query_pairs_mut().append_pair("t", ticket);
    u
}

pub struct HttpSidecar {
    http: reqwest::Client,
}

impl Default for HttpSidecar {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
            // The sidecar never redirects API calls; don't follow the secret anywhere.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self { http }
    }
}

impl HttpSidecar {
    async fn post(&self, base: &Url, path: &str, secret: &str) -> Result<String, SidecarError> {
        let url = base
            .join(path)
            .map_err(|e| SidecarError::Parse(e.to_string()))?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(secret)
            .send()
            .await
            .map_err(|e| SidecarError::Unreachable(e.without_url().to_string()))?;
        match resp.status().as_u16() {
            200 => resp
                .text()
                .await
                .map_err(|e| SidecarError::Unreachable(e.without_url().to_string())),
            401 => Err(SidecarError::Unauthorized),
            // cloudflared answers 502/530 while the origin or DNS isn't ready.
            code @ (502 | 503 | 504 | 530) => {
                Err(SidecarError::Unreachable(format!("HTTP {code}")))
            }
            code => Err(SidecarError::Http(code)),
        }
    }
}

/// provision.sh writes `progress` as a fraction (0..1); the app uses percent.
/// (Phase 2 assumed percent, so the real download bar sat at 0%.)
pub fn parse_status(body: &str) -> Result<SidecarStatus, SidecarError> {
    let mut st: SidecarStatus =
        serde_json::from_str(body).map_err(|e| SidecarError::Parse(e.to_string()))?;
    st.progress = st
        .progress
        .filter(|p| p.is_finite())
        .map(|p| (p * 100.0).clamp(0.0, 100.0));
    Ok(st)
}

#[async_trait]
impl SidecarApi for HttpSidecar {
    async fn heartbeat(&self, base: &Url, secret: &str) -> Result<SidecarStatus, SidecarError> {
        let body = self.post(base, "/__heartbeat", secret).await?;
        parse_status(&body)
    }

    async fn ticket(&self, base: &Url, secret: &str) -> Result<String, SidecarError> {
        #[derive(Deserialize)]
        struct T {
            ticket: String,
        }
        let body = self.post(base, "/__ticket", secret).await?;
        serde_json::from_str::<T>(&body)
            .map(|t| t.ticket)
            .map_err(|e| SidecarError::Parse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sidecar_payload() {
        let s = parse_status(
            r#"{"stage":"downloading","detail":"bananaSplitzXXL_121.safetensors","progress":0.415,
                "updated":1727000000,"destroying":null,
                "deadlines":{"heartbeat_s":598.2,"idle_s":null,"max_session_s":14000}}"#,
        )
        .unwrap();
        assert_eq!(s.stage, "downloading");
        assert!((s.progress.unwrap() - 41.5).abs() < 1e-9);
        assert_eq!(s.deadlines.unwrap().idle_s, None);
        let booting: SidecarStatus =
            serde_json::from_str(r#"{"stage":"booting","destroying":null,"deadlines":{}}"#)
                .unwrap();
        assert_eq!(booting.detail, None);
    }

    #[test]
    fn auth_url_shape() {
        let base = Url::parse("https://a-b.trycloudflare.com/").unwrap();
        assert_eq!(
            auth_url(&base, "abc-_123").as_str(),
            "https://a-b.trycloudflare.com/__auth?t=abc-_123"
        );
    }
}
