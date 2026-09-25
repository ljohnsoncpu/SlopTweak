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

/// One entry of Invoke's image list (the fields output sync uses).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InvokeImage {
    pub image_name: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub is_intermediate: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImagePage {
    pub items: Vec<InvokeImage>,
    pub total: u64,
}

/// Largest full-res image the app accepts (Invoke PNGs are a few MB).
pub const MAX_IMAGE_BYTES: usize = 200 * 1024 * 1024;
/// A queue item carries its whole graph; far bigger than this is not Invoke.
pub const MAX_QUEUE_ITEM_BYTES: usize = 32 * 1024 * 1024;

#[async_trait]
pub trait SidecarApi: Send + Sync {
    async fn heartbeat(&self, base: &Url, secret: &str) -> Result<SidecarStatus, SidecarError>;
    /// Mint a single-use, 60 s webview login ticket.
    async fn ticket(&self, base: &Url, secret: &str) -> Result<String, SidecarError>;
    /// Invoke's gallery (`general`, not intermediate), newest first, through
    /// the proxy (findings §4).
    async fn list_images(
        &self,
        base: &Url,
        secret: &str,
        offset: u64,
        limit: u64,
    ) -> Result<ImagePage, SidecarError>;
    /// One image's record (`GET /api/v1/images/i/{name}`).
    async fn image_info(
        &self,
        base: &Url,
        secret: &str,
        image_name: &str,
    ) -> Result<InvokeImage, SidecarError>;
    /// The full-resolution file for `image_name`.
    async fn image_file(
        &self,
        base: &Url,
        secret: &str,
        image_name: &str,
    ) -> Result<Vec<u8>, SidecarError>;
    /// Every queue item id, newest first.
    async fn queue_item_ids(&self, base: &Url, secret: &str) -> Result<Vec<u64>, SidecarError>;
    /// One queue item, with its session (graph, results). Parsed by
    /// `sync::canvas_outputs`.
    async fn queue_item(
        &self,
        base: &Url,
        secret: &str,
        item_id: u64,
    ) -> Result<serde_json::Value, SidecarError>;
}

/// `/api/v1/images/` gallery query. `starred_first` defaults to true, so it's
/// always passed; `categories=general` leaves out uploads (assets) and masks.
pub fn images_url(base: &Url, offset: u64, limit: u64) -> Url {
    let mut u = base.join("/api/v1/images/").expect("static path");
    u.query_pairs_mut()
        .append_pair("order_dir", "DESC")
        .append_pair("starred_first", "false")
        .append_pair("is_intermediate", "false")
        .append_pair("categories", "general")
        .append_pair("offset", &offset.to_string())
        .append_pair("limit", &limit.to_string());
    u
}

/// `base` + these path segments, each one encoded (so an image name can't
/// escape its segment).
fn api_url(base: &Url, segments: &[&str]) -> Url {
    let mut u = base.clone();
    u.set_query(None);
    u.set_fragment(None);
    u.path_segments_mut()
        .expect("http url")
        .clear()
        .extend(segments);
    u
}

/// `/api/v1/images/i/{name}/full`.
pub fn image_file_url(base: &Url, image_name: &str) -> Url {
    api_url(base, &["api", "v1", "images", "i", image_name, "full"])
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

fn check_status(resp: &reqwest::Response) -> Result<(), SidecarError> {
    match resp.status().as_u16() {
        200 => Ok(()),
        401 => Err(SidecarError::Unauthorized),
        // cloudflared answers 502/530 while the origin or DNS isn't ready.
        code @ (502 | 503 | 504 | 530) => Err(SidecarError::Unreachable(format!("HTTP {code}"))),
        code => Err(SidecarError::Http(code)),
    }
}

fn unreachable(e: reqwest::Error) -> SidecarError {
    SidecarError::Unreachable(e.without_url().to_string())
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
            .map_err(unreachable)?;
        check_status(&resp)?;
        resp.text().await.map_err(unreachable)
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

    async fn list_images(
        &self,
        base: &Url,
        secret: &str,
        offset: u64,
        limit: u64,
    ) -> Result<ImagePage, SidecarError> {
        self.get_json(images_url(base, offset, limit), secret).await
    }

    async fn image_info(
        &self,
        base: &Url,
        secret: &str,
        image_name: &str,
    ) -> Result<InvokeImage, SidecarError> {
        let url = api_url(base, &["api", "v1", "images", "i", image_name]);
        self.get_json(url, secret).await
    }

    async fn image_file(
        &self,
        base: &Url,
        secret: &str,
        image_name: &str,
    ) -> Result<Vec<u8>, SidecarError> {
        // Budget for the whole file over the tunnel; the client default is 15 s.
        self.get_bytes(
            image_file_url(base, image_name),
            secret,
            MAX_IMAGE_BYTES,
            120,
        )
        .await
    }

    async fn queue_item_ids(&self, base: &Url, secret: &str) -> Result<Vec<u64>, SidecarError> {
        #[derive(Deserialize)]
        struct Ids {
            item_ids: Vec<u64>,
        }
        let mut url = api_url(base, &["api", "v1", "queue", "default", "item_ids"]);
        url.query_pairs_mut().append_pair("order_dir", "DESC");
        Ok(self.get_json::<Ids>(url, secret).await?.item_ids)
    }

    async fn queue_item(
        &self,
        base: &Url,
        secret: &str,
        item_id: u64,
    ) -> Result<serde_json::Value, SidecarError> {
        let url = api_url(
            base,
            &["api", "v1", "queue", "default", "i", &item_id.to_string()],
        );
        self.get_json(url, secret).await
    }
}

impl HttpSidecar {
    /// GET with the launch secret, reading at most `max` bytes.
    async fn get_bytes(
        &self,
        url: Url,
        secret: &str,
        max: usize,
        timeout_s: u64,
    ) -> Result<Vec<u8>, SidecarError> {
        let mut resp = self
            .http
            .get(url)
            .bearer_auth(secret)
            .timeout(Duration::from_secs(timeout_s))
            .send()
            .await
            .map_err(unreachable)?;
        check_status(&resp)?;
        let too_big = || SidecarError::Parse("response is too large".into());
        if resp.content_length().is_some_and(|n| n > max as u64) {
            return Err(too_big());
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(unreachable)? {
            if out.len() + chunk.len() > max {
                return Err(too_big());
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        secret: &str,
    ) -> Result<T, SidecarError> {
        let body = self
            .get_bytes(url, secret, MAX_QUEUE_ITEM_BYTES, 30)
            .await?;
        serde_json::from_slice(&body).map_err(|e| SidecarError::Parse(e.to_string()))
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
    fn image_urls() {
        let base = Url::parse("https://a-b.trycloudflare.com/").unwrap();
        assert_eq!(
            images_url(&base, 100, 50).as_str(),
            "https://a-b.trycloudflare.com/api/v1/images/?order_dir=DESC&starred_first=false\
             &is_intermediate=false&categories=general&offset=100&limit=50"
        );
        assert_eq!(
            image_file_url(&base, "0f3c-11.png").as_str(),
            "https://a-b.trycloudflare.com/api/v1/images/i/0f3c-11.png/full"
        );
        // A hostile name stays inside its own path segment.
        assert_eq!(
            image_file_url(&base, "../../__ticket?x").path(),
            "/api/v1/images/i/..%2F..%2F__ticket%3Fx/full"
        );
        let page: ImagePage = serde_json::from_str(
            r#"{"items":[{"image_name":"a.png","created_at":"2026-09-25 14:03:22.123",
                "is_intermediate":true,"node_id":"canvas_output:x1","width":8}],
                "offset":0,"limit":1,"total":7}"#,
        )
        .unwrap();
        assert_eq!(page.total, 7);
        assert!(page.items[0].is_intermediate);
        assert_eq!(
            api_url(&base, &["api", "v1", "queue", "default", "i", "12"]).as_str(),
            "https://a-b.trycloudflare.com/api/v1/queue/default/i/12"
        );
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
