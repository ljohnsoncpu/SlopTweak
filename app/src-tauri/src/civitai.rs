//! CivitAI: key validation and "add a LoRA from a link".
//!
//! Verified live (2026-09-25, findings "Phase 3"):
//! * `GET /api/v1/me` with `Authorization: Bearer <key>` → 200 with
//!   `username`; a bad or missing key → 401.
//! * `GET /api/v1/model-versions/{id}` is public and returns `model.type`,
//!   `baseModel`, and `files[]` with `hashes.SHA256` and `sizeKB`, where
//!   `sizeKB * 1024` is the exact byte count.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::catalog::{is_civitai_url, validate_file, ModelFile};

pub const API_BASE: &str = "https://civitai.com/api/v1";
/// LoRAs over this size are almost certainly not LoRAs.
const MAX_LORA_BYTES: u64 = 2_000_000_000;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CivitaiError {
    #[error("CivitAI didn't accept that key")]
    BadKey,
    #[error("CivitAI has nothing at that link")]
    NotFound,
    #[error("couldn't reach CivitAI: {0}")]
    Network(String),
    #[error("{0}")]
    Invalid(String),
}

/// What a CivitAI link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRef {
    Version(u64),
    Model {
        model_id: u64,
        version_id: Option<u64>,
    },
}

/// Accepts the links people copy: a model page (optionally with
/// `?modelVersionId=`), or a download link. civitai.red serves the same
/// data (findings, Decisions §9.4).
pub fn parse_link(input: &str) -> Result<LinkRef, CivitaiError> {
    let bad = || CivitaiError::Invalid("Paste a link to a model page on civitai.com.".into());
    let s = input.trim();
    let s = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let u = Url::parse(&s).map_err(|_| bad())?;
    let host = u.host_str().unwrap_or("");
    if !matches!(
        host,
        "civitai.com" | "www.civitai.com" | "civitai.red" | "www.civitai.red"
    ) {
        return Err(bad());
    }
    let segs: Vec<&str> = u.path_segments().map(|p| p.collect()).unwrap_or_default();
    let num = |s: &str| s.parse::<u64>().ok().filter(|n| *n > 0);
    let version_q = u
        .query_pairs()
        .find(|(k, _)| k == "modelVersionId")
        .and_then(|(_, v)| num(&v));
    match segs.as_slice() {
        ["models", id, ..] => Ok(LinkRef::Model {
            model_id: num(id).ok_or_else(bad)?,
            version_id: version_q,
        }),
        ["api", "download", "models", id, ..] => Ok(LinkRef::Version(num(id).ok_or_else(bad)?)),
        _ => Err(bad()),
    }
}

/// A LoRA the user added, stored in settings.json (no secrets in it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lora {
    pub model_id: u64,
    pub version_id: u64,
    pub name: String,
    pub version_name: String,
    /// CivitAI's `baseModel`, e.g. "SDXL 1.0", "Illustrious".
    pub base_model: String,
    pub file: ModelFile,
    #[serde(default)]
    pub nsfw: bool,
    #[serde(default)]
    pub trained_words: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl Lora {
    pub fn page(&self) -> String {
        format!(
            "https://civitai.com/models/{}?modelVersionId={}",
            self.model_id, self.version_id
        )
    }
}

/// The catalog `base` family a CivitAI `baseModel` belongs to, if known.
pub fn family(civitai_base: &str) -> Option<&'static str> {
    let b = civitai_base.trim().to_ascii_lowercase();
    if b.starts_with("sd 1.") {
        Some("sd1")
    } else if b.starts_with("sd 2.") {
        Some("sd2")
    } else if b.starts_with("sdxl")
        || b.starts_with("pony")
        || b.starts_with("illustrious")
        || b.starts_with("noobai")
    {
        Some("sdxl")
    } else if b.starts_with("flux.1") {
        Some("flux")
    } else if b.starts_with("sd 3") {
        Some("sd3")
    } else if b.starts_with("anima") {
        Some("anima")
    } else {
        None
    }
}

pub fn compatible(lora: &Lora, model_base: &str) -> bool {
    family(&lora.base_model) == Some(model_base)
}

/// Keep a CivitAI file name usable as a plain file name on the instance.
fn clean_filename(name: &str, version_id: u64) -> String {
    let stem = name
        .strip_suffix(".safetensors")
        .unwrap_or(name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches(|c| c == '_' || c == '-');
    let stem: String = stem.chars().take(80).collect();
    if stem.is_empty() {
        format!("lora-{version_id}.safetensors")
    } else {
        format!("{stem}.safetensors")
    }
}

/// Build a [`Lora`] from a `/model-versions/{id}` response.
pub fn lora_from_version(v: &Value) -> Result<Lora, CivitaiError> {
    let invalid = |m: &str| CivitaiError::Invalid(m.to_string());
    let s = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let version_id = v
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("CivitAI sent an unexpected reply."))?;
    let model_id = v.get("modelId").and_then(Value::as_u64).unwrap_or(0);
    let model = v.get("model").cloned().unwrap_or(Value::Null);
    let kind = s(&model, "type");
    if !matches!(kind.as_str(), "LORA" | "LoCon" | "DoRA") {
        let what = if kind.is_empty() { "unknown" } else { &kind };
        return Err(CivitaiError::Invalid(format!(
            "That link is a {what}, not a LoRA."
        )));
    }
    let files = v
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_model = |f: &&Value| f.get("type").and_then(Value::as_str) == Some("Model");
    let file = files
        .iter()
        .filter(is_model)
        .find(|f| f.get("primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| files.iter().find(is_model))
        .ok_or_else(|| invalid("That LoRA has no downloadable file."))?;
    let format = file
        .pointer("/metadata/format")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = s(file, "name");
    if format != "SafeTensor" && !name.ends_with(".safetensors") {
        // Pickle files can run code when loaded.
        return Err(invalid(
            "That LoRA isn't a .safetensors file, so SlopTweak won't load it.",
        ));
    }
    let sha256 = file
        .pointer("/hashes/SHA256")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let size_kb = file.get("sizeKB").and_then(Value::as_f64).unwrap_or(0.0);
    let size = size_kb * 1024.0;
    if !(size >= 1.0 && (size - size.round()).abs() < 0.01) {
        return Err(invalid("CivitAI didn't give an exact size for that file."));
    }
    let size_bytes = size.round() as u64;
    if size_bytes > MAX_LORA_BYTES {
        return Err(invalid("That file is too big to be a LoRA."));
    }
    let url = s(file, "downloadUrl");
    let parsed = Url::parse(&url).map_err(|_| invalid("CivitAI sent a bad download link."))?;
    if !is_civitai_url(&parsed) {
        return Err(invalid("CivitAI sent a download link on another site."));
    }
    let mf = ModelFile {
        url,
        filename: clean_filename(&name, version_id),
        sha256,
        size_bytes,
        kind: "lora".into(),
        // The download goes to civitai.com; many LoRAs need a signed-in user.
        requires_civitai_token: true,
    };
    validate_file(&mf)
        .map_err(|e| CivitaiError::Invalid(format!("That LoRA can't be used: {e}.")))?;
    let trained_words = v
        .get("trainedWords")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|w| w.chars().take(200).collect())
                .take(20)
                .collect()
        })
        .unwrap_or_default();
    Ok(Lora {
        model_id,
        version_id,
        name: s(&model, "name").chars().take(120).collect(),
        version_name: s(v, "name").chars().take(80).collect(),
        base_model: s(v, "baseModel"),
        file: mf,
        nsfw: model.get("nsfw").and_then(Value::as_bool).unwrap_or(false),
        trained_words,
        enabled: true,
    })
}

#[async_trait]
pub trait CivitaiApi: Send + Sync {
    /// The account name for a key; `BadKey` if CivitAI rejects it.
    async fn username(&self, key: &str) -> Result<String, CivitaiError>;
    /// Raw `/model-versions/{id}` JSON.
    async fn model_version(&self, id: u64, key: Option<&str>) -> Result<Value, CivitaiError>;
    /// The newest version id of a model.
    async fn latest_version(&self, model_id: u64, key: Option<&str>) -> Result<u64, CivitaiError>;
}

/// Resolve a link to a validated LoRA.
pub async fn resolve_lora(
    api: &dyn CivitaiApi,
    link: &str,
    key: Option<&str>,
) -> Result<Lora, CivitaiError> {
    let version_id = match parse_link(link)? {
        LinkRef::Version(v) => v,
        LinkRef::Model {
            version_id: Some(v),
            ..
        } => v,
        LinkRef::Model {
            model_id,
            version_id: None,
        } => api.latest_version(model_id, key).await?,
    };
    let lora = lora_from_version(&api.model_version(version_id, key).await?)?;
    if let LinkRef::Model { model_id, .. } = parse_link(link)? {
        if lora.model_id != 0 && lora.model_id != model_id {
            return Err(CivitaiError::Invalid(
                "That version belongs to a different model.".into(),
            ));
        }
    }
    Ok(lora)
}

pub struct HttpCivitai {
    http: reqwest::Client,
    base: String,
}

impl Default for HttpCivitai {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
            // Never carry the key off civitai.com.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self {
            http,
            base: API_BASE.into(),
        }
    }
}

impl HttpCivitai {
    async fn get(&self, path: &str, key: Option<&str>) -> Result<Value, CivitaiError> {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| CivitaiError::Network(e.without_url().to_string()))?;
        match resp.status() {
            s if s.is_success() => resp
                .json()
                .await
                .map_err(|e| CivitaiError::Network(e.without_url().to_string())),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(CivitaiError::BadKey),
            StatusCode::NOT_FOUND => Err(CivitaiError::NotFound),
            s => Err(CivitaiError::Network(format!("HTTP {}", s.as_u16()))),
        }
    }
}

#[async_trait]
impl CivitaiApi for HttpCivitai {
    async fn username(&self, key: &str) -> Result<String, CivitaiError> {
        let v = self.get("/me", Some(key)).await?;
        // The reply also has the email; keep only the name.
        Ok(v.get("username")
            .and_then(Value::as_str)
            .unwrap_or("your account")
            .to_string())
    }

    async fn model_version(&self, id: u64, key: Option<&str>) -> Result<Value, CivitaiError> {
        self.get(&format!("/model-versions/{id}"), key).await
    }

    async fn latest_version(&self, model_id: u64, key: Option<&str>) -> Result<u64, CivitaiError> {
        let v = self.get(&format!("/models/{model_id}"), key).await?;
        v.get("modelVersions")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|mv| mv.get("id"))
            .and_then(Value::as_u64)
            .ok_or(CivitaiError::NotFound)
    }
}

/// Offline stand-in for mock mode and tests. Keys starting with `bad` are
/// rejected; version 404 doesn't exist; version 2 is a checkpoint.
pub struct MockCivitai;

pub fn mock_version(id: u64) -> Value {
    serde_json::json!({
        "id": id,
        "modelId": 122359,
        "name": "v1.0",
        "baseModel": if id == 3 { "SD 1.5" } else { "SDXL 1.0" },
        "trainedWords": ["detailed"],
        "model": {"name": format!("Mock LoRA {id}"), "type": if id == 2 { "Checkpoint" } else { "LORA" }, "nsfw": false},
        "files": [{
            "name": "add-detail-xl.safetensors", "type": "Model", "primary": true,
            "sizeKB": 223097.9921875,
            "hashes": {"SHA256": "0D9BD1B873A7863E128B4672E3E245838858F71469A3CEC58123C16C06F83BD7"},
            "downloadUrl": format!("https://civitai.com/api/download/models/{id}"),
            "metadata": {"format": "SafeTensor"}
        }]
    })
}

#[async_trait]
impl CivitaiApi for MockCivitai {
    async fn username(&self, key: &str) -> Result<String, CivitaiError> {
        if key.starts_with("bad") {
            Err(CivitaiError::BadKey)
        } else {
            Ok("mock-user".into())
        }
    }

    async fn model_version(&self, id: u64, _key: Option<&str>) -> Result<Value, CivitaiError> {
        if id == 404 {
            return Err(CivitaiError::NotFound);
        }
        Ok(mock_version(id))
    }

    async fn latest_version(&self, model_id: u64, _key: Option<&str>) -> Result<u64, CivitaiError> {
        if model_id == 404 {
            return Err(CivitaiError::NotFound);
        }
        Ok(135867)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn links() {
        assert_eq!(
            parse_link("https://civitai.com/models/122359/detail-tweaker-xl").unwrap(),
            LinkRef::Model {
                model_id: 122359,
                version_id: None
            }
        );
        assert_eq!(
            parse_link("civitai.com/models/122359?modelVersionId=135867").unwrap(),
            LinkRef::Model {
                model_id: 122359,
                version_id: Some(135867)
            }
        );
        assert_eq!(
            parse_link("https://civitai.red/models/1/x?modelVersionId=2&foo=bar").unwrap(),
            LinkRef::Model {
                model_id: 1,
                version_id: Some(2)
            }
        );
        assert_eq!(
            parse_link("https://civitai.com/api/download/models/135867?type=Model").unwrap(),
            LinkRef::Version(135867)
        );
        for bad in [
            "",
            "hello",
            "https://evil.com/models/1",
            "https://civitai.com.evil.com/models/1",
            "https://civitai.com/user/foo",
            "https://civitai.com/models/abc",
            "https://civitai.com/models/0",
        ] {
            assert!(parse_link(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn live_shaped_lora_parses() {
        let l = lora_from_version(&mock_version(135867)).unwrap();
        assert_eq!(l.version_id, 135867);
        assert_eq!(l.model_id, 122359);
        // Live: sizeKB 223097.9921875 → 228452344 bytes (checked by ranged GET).
        assert_eq!(l.file.size_bytes, 228_452_344);
        assert_eq!(l.file.filename, "add-detail-xl.safetensors");
        assert_eq!(l.file.kind, "lora");
        assert!(l.file.requires_civitai_token);
        assert_eq!(l.file.sha256, l.file.sha256.to_ascii_lowercase());
        assert_eq!(l.base_model, "SDXL 1.0");
        assert_eq!(l.trained_words, vec!["detailed"]);
    }

    #[test]
    fn non_loras_and_unsafe_files_rejected() {
        assert!(lora_from_version(&mock_version(2))
            .unwrap_err()
            .to_string()
            .contains("Checkpoint"));

        let mut pickle = mock_version(5);
        pickle["files"][0]["name"] = json!("x.pt");
        pickle["files"][0]["metadata"]["format"] = json!("PickleTensor");
        assert!(lora_from_version(&pickle).is_err());

        let mut offsite = mock_version(5);
        offsite["files"][0]["downloadUrl"] = json!("https://evil.example/x");
        assert!(lora_from_version(&offsite).is_err());

        let mut nohash = mock_version(5);
        nohash["files"][0]["hashes"] = json!({});
        assert!(lora_from_version(&nohash).is_err());

        let mut inexact = mock_version(5);
        inexact["files"][0]["sizeKB"] = json!(1.3);
        assert!(lora_from_version(&inexact).is_err());
    }

    #[test]
    fn primary_model_file_is_chosen() {
        let mut v = mock_version(7);
        let model_file = v["files"][0].clone();
        v["files"] = json!([
            {"name": "dataset.zip", "type": "Training Data", "sizeKB": 1.0,
             "hashes": {"SHA256": "b".repeat(64)}, "downloadUrl": "https://civitai.com/x",
             "metadata": {"format": "Other"}},
            model_file
        ]);
        assert_eq!(
            lora_from_version(&v).unwrap().file.filename,
            "add-detail-xl.safetensors"
        );
    }

    #[test]
    fn filenames_are_cleaned() {
        assert_eq!(clean_filename("a b(1).safetensors", 9), "a_b_1.safetensors");
        assert_eq!(clean_filename("../../x.safetensors", 9), "x.safetensors");
        assert_eq!(clean_filename("日本.safetensors", 9), "lora-9.safetensors");
    }

    #[test]
    fn families() {
        assert_eq!(family("SDXL 1.0"), Some("sdxl"));
        assert_eq!(family("Illustrious"), Some("sdxl"));
        assert_eq!(family("Pony"), Some("sdxl"));
        assert_eq!(family("NoobAI"), Some("sdxl"));
        assert_eq!(family("SD 1.5"), Some("sd1"));
        assert_eq!(family("Flux.1 D"), Some("flux"));
        assert_eq!(family("Anima"), Some("anima"));
        assert_eq!(family("Wan Video"), None);
    }

    #[tokio::test]
    async fn resolve_via_model_page() {
        let l = resolve_lora(&MockCivitai, "https://civitai.com/models/122359/x", None)
            .await
            .unwrap();
        assert_eq!(l.version_id, 135867);
        let e = resolve_lora(&MockCivitai, "https://civitai.com/models/404", None)
            .await
            .unwrap_err();
        assert_eq!(e, CivitaiError::NotFound);
        let e = resolve_lora(
            &MockCivitai,
            "https://civitai.com/models/999?modelVersionId=135867",
            None,
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("different model"));
    }
}
