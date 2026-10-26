//! The app's side of the instance's `/__train/*` endpoints (instance/sidecar.py),
//! and the [`RemoteTrain`] the run lifecycle uses. Every call carries the
//! launch secret as a bearer token and never follows a redirect.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use url::Url;

use super::job::JobBundle;
use super::run::{Progress, RemoteTrain, SetupNote};
use crate::provider::GpuProvider;

/// Largest result set the app will pull in one run (the approved egress).
pub const MAX_RESULT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TrainState {
    /// waiting | bootstrap | prepare | train | evaluate | done | failed | cancelled
    pub stage: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub step: Option<u32>,
    #[serde(default)]
    pub done: Option<u32>,
    #[serde(default)]
    pub total: Option<u32>,
    #[serde(default)]
    pub loss: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started: bool,
    /// Setup only: size of the setup logs and the end of the newest one.
    #[serde(default)]
    pub log_bytes: Option<u64>,
    #[serde(default)]
    pub log_tail: Option<String>,
}

impl TrainState {
    /// What setup is doing, or `None` once training has begun.
    pub fn setup_note(&self) -> Option<SetupNote> {
        (self.progress() == Progress::Starting).then(|| SetupNote {
            stage: self.stage.clone(),
            detail: self.detail.clone(),
            log_bytes: self.log_bytes,
            log_tail: self.log_tail.clone().unwrap_or_default(),
        })
    }

    pub fn progress(&self) -> Progress {
        let total = self.total.unwrap_or(0);
        match self.stage.as_str() {
            "train" => Progress::Training {
                step: self.step.unwrap_or(0),
                total,
            },
            "evaluate" => Progress::Evaluating {
                done: self.done.unwrap_or(0),
                total,
            },
            "done" => Progress::Done,
            "failed" => Progress::Failed(
                self.error
                    .clone()
                    .unwrap_or_else(|| "Training failed.".into()),
            ),
            "cancelled" => Progress::Failed("The run was cancelled.".into()),
            _ => Progress::Starting,
        }
    }
}

/// The instance's own setup status (`/__status`): booting, downloading the
/// base model, verifying it, starting the trainer. The trainer's state says
/// only `waiting` until the trainer exists, so this is what a slow model
/// download looks like from outside.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Provision {
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// Download progress, 0 to 1.
    #[serde(default)]
    pub progress: Option<f64>,
}

impl Provision {
    /// One line for the screen, e.g. `base.safetensors 37%`.
    pub fn describe(&self) -> String {
        let what = self.detail.as_deref().unwrap_or("").trim();
        let pct = self
            .progress
            .filter(|p| (0.0..=1.0).contains(p) && self.stage == "downloading")
            .map(|p| format!(" {:.0}%", p * 100.0));
        format!(
            "{}{}{}",
            self.stage,
            if what.is_empty() { "" } else { " " },
            what
        ) + pct.as_deref().unwrap_or("")
    }

    /// What to report while the trainer is still `waiting`. `None` once the
    /// instance says it is ready (the trainer is about to answer itself).
    pub fn while_waiting(&self) -> Option<(Progress, Option<SetupNote>)> {
        match self.stage.as_str() {
            "" | "ready" => None,
            "failed" => Some((
                Progress::Failed(format!(
                    "The GPU's setup failed: {}",
                    self.detail.as_deref().unwrap_or("no reason given")
                )),
                None,
            )),
            other => Some((
                Progress::Starting,
                Some(SetupNote {
                    stage: format!("provision:{other}"),
                    detail: Some(self.describe()),
                    // Present so a stalled download is caught by the watchdog;
                    // the percentage in `detail` is what changes.
                    log_bytes: Some(0),
                    log_tail: String::new(),
                }),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Deserialize)]
struct Manifest {
    files: Vec<ManifestEntry>,
}

/// A result path the instance listed, checked before it becomes a local path.
pub fn safe_relative(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let ok = !path.is_empty()
        && path.len() <= 200
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && p.components().count() <= 4
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && path.split('/').all(|seg| !seg.is_empty() && seg != "..")
        && !path.contains("..");
    ok.then(|| p.to_path_buf())
}

fn hex_digest(h: Sha256) -> String {
    hex::encode(h.finalize())
}

pub struct HttpTrain {
    http: reqwest::Client,
}

impl Default for HttpTrain {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self { http }
    }
}

fn err(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

fn status_error(code: u16) -> String {
    match code {
        401 => "The GPU rejected the launch secret.".into(),
        502 | 503 | 504 | 530 => format!("The GPU isn't reachable yet (HTTP {code})."),
        c => format!("The GPU answered HTTP {c}."),
    }
}

impl HttpTrain {
    fn url(base: &Url, path: &str) -> Result<Url, String> {
        base.join(path).map_err(|e| e.to_string())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        base: &Url,
        secret: &str,
        path: &str,
    ) -> Result<T, String> {
        let mut resp = self
            .http
            .get(Self::url(base, path)?)
            .bearer_auth(secret)
            .send()
            .await
            .map_err(err)?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(err)? {
            if body.len() + chunk.len() > MAX_JSON_BYTES {
                return Err("The GPU's answer is too large.".into());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|e| format!("Bad answer from the GPU: {e}"))
    }

    pub async fn state(&self, base: &Url, secret: &str) -> Result<TrainState, String> {
        self.get_json(base, secret, "/__train/state").await
    }

    pub async fn provision(&self, base: &Url, secret: &str) -> Result<Provision, String> {
        self.get_json(base, secret, "/__status").await
    }

    pub async fn manifest(&self, base: &Url, secret: &str) -> Result<Vec<ManifestEntry>, String> {
        let m: Manifest = self.get_json(base, secret, "/__train/manifest").await?;
        Ok(m.files)
    }

    /// Upload `job.json` or `dataset.tar`; the instance verifies `sha256`.
    pub async fn upload(
        &self,
        base: &Url,
        secret: &str,
        name: &str,
        data: Vec<u8>,
        sha256: &str,
    ) -> Result<(), String> {
        let resp = self
            .http
            .put(Self::url(base, &format!("/__train/upload/{name}"))?)
            .bearer_auth(secret)
            .header("X-Sha256", sha256)
            .timeout(Duration::from_secs(30 * 60))
            .body(data)
            .send()
            .await
            .map_err(err)?;
        match resp.status().as_u16() {
            200 => Ok(()),
            422 => Err(format!("{name} was damaged on the way to the GPU.")),
            c => Err(status_error(c)),
        }
    }

    async fn post(&self, base: &Url, secret: &str, path: &str) -> Result<(), String> {
        let resp = self
            .http
            .post(Self::url(base, path)?)
            .bearer_auth(secret)
            .send()
            .await
            .map_err(err)?;
        match resp.status().as_u16() {
            200 => Ok(()),
            c => Err(status_error(c)),
        }
    }

    /// Keeps the instance's deadman timer from destroying the GPU while the
    /// app is watching it.
    pub async fn heartbeat(&self, base: &Url, secret: &str) -> Result<(), String> {
        self.post(base, secret, "/__heartbeat").await
    }

    pub async fn start(&self, base: &Url, secret: &str) -> Result<(), String> {
        self.post(base, secret, "/__train/start").await
    }

    /// Upload the job and dataset, then start. Safe to repeat: a run that has
    /// already started is left alone.
    pub async fn begin(&self, base: &Url, secret: &str, job: &JobBundle) -> Result<(), String> {
        if self.state(base, secret).await?.started {
            return Ok(());
        }
        self.upload(
            base,
            secret,
            "job.json",
            job.job_json.clone(),
            &job.job_sha256,
        )
        .await?;
        self.upload(
            base,
            secret,
            "dataset.tar",
            job.dataset_tar.clone(),
            &job.dataset_sha256,
        )
        .await?;
        self.start(base, secret).await
    }

    pub async fn cancel(&self, base: &Url, secret: &str) -> Result<(), String> {
        self.post(base, secret, "/__train/cancel").await
    }

    pub async fn log_tail(&self, base: &Url, secret: &str, name: &str) -> Result<String, String> {
        let mut u = Self::url(base, "/__train/log")?;
        u.query_pairs_mut().append_pair("name", name);
        let resp = self
            .http
            .get(u)
            .bearer_auth(secret)
            .send()
            .await
            .map_err(err)?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status().as_u16()));
        }
        resp.text().await.map_err(err)
    }

    /// Fetch every manifest file not already present and verified. Returns the
    /// bytes downloaded. Each file is written to `.partial`, hashed while it
    /// streams, and renamed only when it matches the manifest.
    pub async fn download_results(
        &self,
        base: &Url,
        secret: &str,
        dest: &Path,
        budget: u64,
    ) -> Result<u64, String> {
        let entries = self.manifest(base, secret).await?;
        let mut downloaded = 0u64;
        for e in entries {
            let rel = safe_relative(&e.path)
                .ok_or_else(|| format!("The GPU listed an unsafe path: {}", e.path))?;
            let target = dest.join(&rel);
            if verified_file(&target, &e.sha256).await {
                continue;
            }
            if downloaded + e.bytes > budget {
                return Err("The results are larger than the approved download.".into());
            }
            if let Some(dir) = target.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let part = target.with_extension("partial");
            downloaded += self.fetch_file(base, secret, &e, &part).await?;
            tokio::fs::rename(&part, &target)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(downloaded)
    }

    async fn fetch_file(
        &self,
        base: &Url,
        secret: &str,
        entry: &ManifestEntry,
        part: &Path,
    ) -> Result<u64, String> {
        let mut u = Self::url(base, "/__train/file")?;
        u.query_pairs_mut().append_pair("path", &entry.path);
        let mut resp = self
            .http
            .get(u)
            .bearer_auth(secret)
            .timeout(Duration::from_secs(30 * 60))
            .send()
            .await
            .map_err(err)?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status().as_u16()));
        }
        let mut file = tokio::fs::File::create(part)
            .await
            .map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let result: Result<(), String> = async {
            while let Some(chunk) = resp.chunk().await.map_err(err)? {
                size += chunk.len() as u64;
                if size > entry.bytes {
                    return Err(format!("{} is larger than listed.", entry.path));
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            }
            file.flush().await.map_err(|e| e.to_string())
        }
        .await;
        drop(file);
        let check = result.and_then(|()| {
            if size != entry.bytes || !hex_digest(hasher).eq_ignore_ascii_case(&entry.sha256) {
                Err(format!("{} didn't match its checksum.", entry.path))
            } else {
                Ok(())
            }
        });
        if let Err(e) = check {
            let _ = tokio::fs::remove_file(part).await;
            return Err(e);
        }
        Ok(size)
    }
}

async fn verified_file(path: &Path, sha256: &str) -> bool {
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        match tokio::io::AsyncReadExt::read(&mut f, &mut buf).await {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex_digest(h).eq_ignore_ascii_case(sha256)
}

/// [`RemoteTrain`] over the instance's tunnel: finds the sidecar through the
/// provider, then talks [`HttpTrain`].
pub struct TunnelTrain {
    pub provider: Arc<dyn GpuProvider>,
    pub http: HttpTrain,
    pub secret: String,
    pub results_dir: PathBuf,
    pub job: Arc<JobBundle>,
}

impl TunnelTrain {
    pub async fn base(&self, instance: u64) -> Result<Url, String> {
        let info = self
            .provider
            .instance(instance)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("The GPU is gone.")?;
        self.provider
            .sidecar_url(&info)
            .ok_or_else(|| "The GPU's tunnel isn't up yet.".to_string())
    }
}

#[async_trait]
impl RemoteTrain for TunnelTrain {
    async fn begin(&self, instance: u64) -> Result<(), String> {
        let base = self.base(instance).await?;
        let _ = self.http.heartbeat(&base, &self.secret).await;
        self.http.begin(&base, &self.secret, &self.job).await
    }

    async fn progress(&self, instance: u64) -> Result<Progress, String> {
        let base = self.base(instance).await?;
        // Best effort: a missed beat is covered by the next poll.
        let _ = self.http.heartbeat(&base, &self.secret).await;
        Ok(self.http.state(&base, &self.secret).await?.progress())
    }

    async fn status(&self, instance: u64) -> Result<(Progress, Option<SetupNote>), String> {
        let base = self.base(instance).await?;
        let _ = self.http.heartbeat(&base, &self.secret).await;
        let state = self.http.state(&base, &self.secret).await?;
        if state.stage == "waiting" {
            // No trainer yet: ask the instance what it is doing instead.
            if let Ok(p) = self.http.provision(&base, &self.secret).await {
                if let Some(answer) = p.while_waiting() {
                    return Ok(answer);
                }
            }
        }
        Ok((state.progress(), state.setup_note()))
    }

    async fn sync(&self, instance: u64) -> Result<bool, String> {
        let base = self.base(instance).await?;
        self.http
            .download_results(&base, &self.secret, &self.results_dir, MAX_RESULT_BYTES)
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;

    type Routes = HashMap<String, (u16, Vec<u8>, Vec<(String, String)>)>;
    /// One request: target, Authorization header, body.
    type Seen = (String, Option<String>, Vec<u8>);

    /// A tiny HTTP/1.1 server: fixed answers by "METHOD /path?query", and a
    /// record of every request's method, target, auth header and body.
    struct Fake {
        base: Url,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    async fn fake(routes: Routes) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let (routes, log) = (routes.clone(), log.clone());
                tokio::spawn(async move {
                    let (rd, mut wr) = sock.into_split();
                    let mut rd = BufReader::new(rd);
                    let mut line = String::new();
                    rd.read_line(&mut line).await.unwrap();
                    let target = line.trim_end().trim_end_matches(" HTTP/1.1").to_string();
                    let (mut auth, mut len) = (None, 0usize);
                    loop {
                        let mut h = String::new();
                        rd.read_line(&mut h).await.unwrap();
                        let h = h.trim_end().to_string();
                        if h.is_empty() {
                            break;
                        }
                        let lower = h.to_ascii_lowercase();
                        if let Some(v) = lower.strip_prefix("content-length: ") {
                            len = v.parse().unwrap();
                        }
                        if lower.starts_with("authorization: ") {
                            auth = Some(h["authorization: ".len()..].to_string());
                        }
                    }
                    let mut body = vec![0u8; len];
                    rd.read_exact(&mut body).await.unwrap();
                    log.lock().unwrap().push((target.clone(), auth, body));
                    let (code, data, headers) =
                        routes
                            .get(&target)
                            .cloned()
                            .unwrap_or((404, b"nope".to_vec(), vec![]));
                    let mut head = format!(
                        "HTTP/1.1 {code} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                        data.len()
                    );
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    wr.write_all(head.as_bytes()).await.unwrap();
                    wr.write_all(b"\r\n").await.unwrap();
                    wr.write_all(&data).await.unwrap();
                    wr.shutdown().await.ok();
                });
            }
        });
        Fake { base, seen }
    }

    fn json(code: u16, v: serde_json::Value) -> (u16, Vec<u8>, Vec<(String, String)>) {
        (code, serde_json::to_vec(&v).unwrap(), vec![])
    }

    fn sha(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    #[test]
    fn state_maps_to_progress() {
        let st = |v: serde_json::Value| serde_json::from_value::<TrainState>(v).unwrap().progress();
        assert_eq!(
            st(serde_json::json!({"stage": "waiting"})),
            Progress::Starting
        );
        assert_eq!(
            st(serde_json::json!({"stage": "bootstrap"})),
            Progress::Starting
        );
        assert_eq!(
            st(serde_json::json!({"stage": "train", "step": 5, "total": 9, "loss": 0.1})),
            Progress::Training { step: 5, total: 9 }
        );
        assert_eq!(
            st(serde_json::json!({"stage": "evaluate", "done": 3, "total": 52})),
            Progress::Evaluating { done: 3, total: 52 }
        );
        assert_eq!(st(serde_json::json!({"stage": "done"})), Progress::Done);
        assert_eq!(
            st(serde_json::json!({"stage": "failed", "error": "boom"})),
            Progress::Failed("boom".into())
        );
        assert!(matches!(
            st(serde_json::json!({"stage": "cancelled"})),
            Progress::Failed(_)
        ));
        assert_eq!(st(serde_json::json!({"stage": "???"})), Progress::Starting);
    }

    #[test]
    fn provisioning_shows_through_while_the_trainer_waits() {
        let p = |v: serde_json::Value| serde_json::from_value::<Provision>(v).unwrap();
        let (progress, note) = p(serde_json::json!({
            "stage": "downloading", "detail": "base.safetensors", "progress": 0.371
        }))
        .while_waiting()
        .unwrap();
        assert_eq!(progress, Progress::Starting);
        let note = note.unwrap();
        assert_eq!(note.stage, "provision:downloading");
        assert_eq!(
            note.detail.as_deref(),
            Some("downloading base.safetensors 37%")
        );
        assert!(note.log_bytes.is_some());
        // A failed setup ends the run with the reason, instead of looking stuck.
        let (progress, _) =
            p(serde_json::json!({"stage": "failed", "detail": "download failed: base"}))
                .while_waiting()
                .unwrap();
        assert_eq!(
            progress,
            Progress::Failed("The GPU's setup failed: download failed: base".into())
        );
        // Ready, or an answer we can't read: fall back to the trainer's own state.
        assert!(p(serde_json::json!({"stage": "ready"}))
            .while_waiting()
            .is_none());
        assert!(p(serde_json::json!({})).while_waiting().is_none());
        // Percentages only appear while downloading.
        assert_eq!(
            p(serde_json::json!({"stage": "verifying", "detail": "m", "progress": 1.0})).describe(),
            "verifying m"
        );
    }

    #[test]
    fn setup_note_is_only_for_setup() {
        let st = |v: serde_json::Value| serde_json::from_value::<TrainState>(v).unwrap();
        let note = st(serde_json::json!({
            "stage": "bootstrap",
            "detail": "installing: packages",
            "log_bytes": 4096,
            "log_tail": "Downloading torch"
        }))
        .setup_note()
        .unwrap();
        assert_eq!(note.detail.as_deref(), Some("installing: packages"));
        assert_eq!(
            (note.log_bytes, note.log_tail.as_str()),
            (Some(4096), "Downloading torch")
        );
        // An older sidecar sends no log fields.
        let bare = st(serde_json::json!({"stage": "waiting"}))
            .setup_note()
            .unwrap();
        assert_eq!((bare.log_bytes, bare.log_tail.as_str()), (None, ""));
        assert!(
            st(serde_json::json!({"stage": "train", "step": 1, "total": 2}))
                .setup_note()
                .is_none()
        );
    }

    #[test]
    fn result_paths_are_checked() {
        for ok in [
            "loss.csv",
            "checkpoints/lora-step00000920.safetensors",
            "evaluation/920-0.75/im_000001.png",
        ] {
            assert!(safe_relative(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a\\b",
            "C:/x",
            "a b",
            "a/b/c/d/e",
            "a//b",
            "..",
        ] {
            assert!(safe_relative(bad).is_none(), "{bad}");
        }
    }

    #[tokio::test]
    async fn calls_carry_the_secret_and_read_state() {
        let mut routes = Routes::new();
        routes.insert(
            "GET /__train/state".into(),
            json(
                200,
                serde_json::json!({"stage": "train", "step": 2, "total": 4}),
            ),
        );
        let f = fake(routes).await;
        let st = HttpTrain::default().state(&f.base, "s3cret").await.unwrap();
        assert_eq!(st.progress(), Progress::Training { step: 2, total: 4 });
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].1.as_deref(), Some("Bearer s3cret"));
    }

    #[tokio::test]
    async fn errors_are_plain() {
        let mut routes = Routes::new();
        routes.insert("GET /__train/state".into(), (401, vec![], vec![]));
        routes.insert("POST /__train/start".into(), (409, vec![], vec![]));
        routes.insert("PUT /__train/upload/job.json".into(), (422, vec![], vec![]));
        let f = fake(routes).await;
        let c = HttpTrain::default();
        assert!(c
            .state(&f.base, "x")
            .await
            .unwrap_err()
            .contains("rejected"));
        assert!(c.start(&f.base, "x").await.is_err());
        let e = c
            .upload(&f.base, "x", "job.json", b"{}".to_vec(), &sha(b"{}"))
            .await
            .unwrap_err();
        assert!(e.contains("damaged"), "{e}");
    }

    #[tokio::test]
    async fn upload_sends_the_checksum_and_body() {
        let mut routes = Routes::new();
        routes.insert(
            "PUT /__train/upload/dataset.tar".into(),
            json(200, serde_json::json!({"name": "dataset.tar", "bytes": 3})),
        );
        let f = fake(routes).await;
        HttpTrain::default()
            .upload(&f.base, "k", "dataset.tar", b"abc".to_vec(), &sha(b"abc"))
            .await
            .unwrap();
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[0].2, b"abc");
    }

    fn results_routes(files: &[(&str, &[u8])], lie_about: Option<&str>) -> Routes {
        let mut routes = Routes::new();
        let entries: Vec<_> = files
            .iter()
            .map(|(p, d)| serde_json::json!({"path": p, "sha256": sha(d), "bytes": d.len()}))
            .collect();
        routes.insert(
            "GET /__train/manifest".into(),
            json(200, serde_json::json!({ "files": entries })),
        );
        for (p, d) in files {
            let body = if Some(*p) == lie_about {
                vec![b'X'; d.len()]
            } else {
                d.to_vec()
            };
            routes.insert(
                format!("GET /__train/file?path={}", p.replace('/', "%2F")),
                (200, body, vec![]),
            );
        }
        routes
    }

    #[tokio::test]
    async fn downloads_verify_and_skip_what_is_already_local() {
        let files: &[(&str, &[u8])] = &[
            ("loss.csv", b"step,loss\n"),
            ("checkpoints/lora.safetensors", b"weights"),
        ];
        let f = fake(results_routes(files, None)).await;
        let dir = tempfile::tempdir().unwrap();
        let c = HttpTrain::default();
        let n = c
            .download_results(&f.base, "k", dir.path(), 1 << 30)
            .await
            .unwrap();
        assert_eq!(n, 17);
        assert_eq!(
            std::fs::read(dir.path().join("checkpoints/lora.safetensors")).unwrap(),
            b"weights"
        );
        let again = c
            .download_results(&f.base, "k", dir.path(), 1 << 30)
            .await
            .unwrap();
        assert_eq!(again, 0, "verified files are not fetched twice");
        assert!(!dir.path().join("loss.partial").exists());
    }

    #[tokio::test]
    async fn a_corrupt_download_is_rejected_and_leaves_nothing() {
        let files: &[(&str, &[u8])] = &[("checkpoints/lora.safetensors", b"weights")];
        let f = fake(results_routes(files, Some("checkpoints/lora.safetensors"))).await;
        let dir = tempfile::tempdir().unwrap();
        let e = HttpTrain::default()
            .download_results(&f.base, "k", dir.path(), 1 << 30)
            .await
            .unwrap_err();
        assert!(e.contains("checksum"), "{e}");
        assert!(!dir.path().join("checkpoints/lora.safetensors").exists());
        assert!(!dir.path().join("checkpoints/lora.partial").exists());
    }

    #[tokio::test]
    async fn the_download_budget_and_unsafe_paths_are_enforced() {
        let files: &[(&str, &[u8])] = &[("a.bin", b"0123456789")];
        let f = fake(results_routes(files, None)).await;
        let dir = tempfile::tempdir().unwrap();
        let c = HttpTrain::default();
        let e = c
            .download_results(&f.base, "k", dir.path(), 5)
            .await
            .unwrap_err();
        assert!(e.contains("larger than the approved"), "{e}");

        let evil = fake(results_routes(&[("../escape.txt", b"x")], None)).await;
        let e = c
            .download_results(&evil.base, "k", dir.path(), 1 << 20)
            .await
            .unwrap_err();
        assert!(e.contains("unsafe path"), "{e}");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let mut routes = Routes::new();
        routes.insert(
            "GET /__train/state".into(),
            (
                302,
                vec![],
                vec![("location".into(), "http://127.0.0.1:1/steal".into())],
            ),
        );
        let f = fake(routes).await;
        let e = HttpTrain::default().state(&f.base, "k").await.unwrap_err();
        assert!(e.contains("302"), "{e}");
    }

    fn bundle() -> JobBundle {
        JobBundle {
            job_json: b"{\"job\":1}".to_vec(),
            job_sha256: sha(b"{\"job\":1}"),
            dataset_tar: b"tar-bytes".to_vec(),
            dataset_sha256: sha(b"tar-bytes"),
            total_steps: 4,
            expected_eval_images: 4,
        }
    }

    #[tokio::test]
    async fn begin_uploads_then_starts() {
        let mut routes = Routes::new();
        routes.insert(
            "GET /__train/state".into(),
            json(
                200,
                serde_json::json!({"stage": "waiting", "started": false}),
            ),
        );
        for n in ["job.json", "dataset.tar"] {
            routes.insert(
                format!("PUT /__train/upload/{n}"),
                json(200, serde_json::json!({})),
            );
        }
        routes.insert(
            "POST /__train/start".into(),
            json(200, serde_json::json!({})),
        );
        let f = fake(routes).await;
        HttpTrain::default()
            .begin(&f.base, "k", &bundle())
            .await
            .unwrap();
        let targets: Vec<String> = f.seen.lock().unwrap().iter().map(|s| s.0.clone()).collect();
        assert_eq!(
            targets,
            [
                "GET /__train/state",
                "PUT /__train/upload/job.json",
                "PUT /__train/upload/dataset.tar",
                "POST /__train/start"
            ]
        );
    }

    #[tokio::test]
    async fn begin_does_nothing_once_started() {
        let mut routes = Routes::new();
        routes.insert(
            "GET /__train/state".into(),
            json(200, serde_json::json!({"stage": "train", "started": true})),
        );
        let f = fake(routes).await;
        HttpTrain::default()
            .begin(&f.base, "k", &bundle())
            .await
            .unwrap();
        assert_eq!(f.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn heartbeat_posts_with_the_secret() {
        let mut routes = Routes::new();
        routes.insert(
            "POST /__heartbeat".into(),
            json(200, serde_json::json!({"stage": "x"})),
        );
        let f = fake(routes).await;
        HttpTrain::default().heartbeat(&f.base, "k").await.unwrap();
        assert_eq!(f.seen.lock().unwrap()[0].1.as_deref(), Some("Bearer k"));
    }
}
