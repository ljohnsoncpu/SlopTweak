//! Model catalog: fetched from GitHub on launch, cached locally, with the copy
//! bundled at build time as the last resort. Editing `catalog/catalog.json`
//! on `main` changes the model list without a new build.
//!
//! The catalog decides what the instance downloads and where the CivitAI
//! token is sent, so every entry is validated. A bad entry is skipped (and
//! reported); a catalog with no usable entries is rejected as a whole.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::INVOKE_VERSION;

const BUNDLED: &str = include_str!("../../../catalog/catalog.json");

/// Upstream catalog (user decision 2026-09-25: raw `main` of this repo).
pub const CATALOG_URL: &str =
    "https://raw.githubusercontent.com/ljohnsoncpu/SlopTweak/main/catalog/catalog.json";
const CACHE_FILE: &str = "catalog-cache.json";
const MAX_BYTES: usize = 1 << 20;
const MAX_FILE_BYTES: u64 = 100_000_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelFile {
    pub url: String,
    pub filename: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// `main`, `lora`, `vae`, ...
    pub kind: String,
    #[serde(default)]
    pub requires_civitai_token: bool,
}

/// A prompt template SlopTweak puts into Invoke for this model (an Invoke
/// "style preset"). Taken from the model page's example images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    /// Contains `{prompt}` once; Invoke puts the user's prompt there.
    pub positive: String,
    #[serde(default)]
    pub negative: String,
    /// The example image as a small JPEG/PNG/WebP, base64. Shown in Invoke's
    /// template list.
    #[serde(default)]
    pub image: Option<String>,
    /// Where the example came from (credit).
    #[serde(default)]
    pub image_source: Option<String>,
}

pub const MAX_TEMPLATES: usize = 12;
const MAX_TEMPLATE_PROMPT: usize = 2000;
pub const MAX_TEMPLATE_IMAGE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Model family: `sdxl`, `sd1`, `flux`, ...
    pub base: String,
    pub files: Vec<ModelFile>,
    pub min_vram_gb: f64,
    #[serde(default)]
    pub nsfw: bool,
    #[serde(default)]
    pub license_note: String,
    #[serde(default)]
    pub invoke_min_version: Option<String>,
    /// GPU architecture this model needs, Vast units (800 = Ampere / RTX
    /// 30-series, for native bf16). Raises the app-wide floor for this
    /// model only.
    #[serde(default)]
    pub min_compute_cap: Option<u32>,
    /// Host RAM this model needs, GB. Invoke streams a transformer bigger
    /// than VRAM from RAM, so large models need a roomy host.
    #[serde(default)]
    pub min_ram_gb: Option<f64>,
    /// Rough rental price class: 1 = cheapest GPUs, 3 = the priciest.
    #[serde(default)]
    pub price_tier: Option<u8>,
    /// A few words on what the model is for, shown in the model list.
    #[serde(default)]
    pub good_for: String,
    /// Built-in prompt templates. A bad one is dropped, not the model.
    #[serde(default)]
    pub templates: Vec<Template>,
}

impl Model {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size_bytes).sum()
    }

    pub fn needs_civitai(&self) -> bool {
        self.files.iter().any(|f| f.requires_civitai_token)
    }
}

#[derive(Debug, Deserialize)]
struct RawCatalog {
    schema_version: u32,
    models: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Online,
    Cached,
    Bundled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub source: Source,
    /// When the online copy was fetched (for `Online` and `Cached`).
    pub fetched_unix: Option<u64>,
    /// Entries left out, with the reason. Shown under Details.
    pub skipped: Vec<String>,
}

impl Catalog {
    pub fn find(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }
}

#[cfg(test)]
pub fn bundled() -> Vec<Model> {
    parse(BUNDLED).expect("bundled catalog.json is valid").0
}

/// Parse and validate. Returns the usable models and the skipped entries.
pub fn parse(text: &str) -> Result<(Vec<Model>, Vec<String>), String> {
    let raw: RawCatalog = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if raw.schema_version != 1 {
        return Err(format!("unsupported catalog schema {}", raw.schema_version));
    }
    let mut models = Vec::new();
    let mut skipped = Vec::new();
    let mut ids = HashSet::new();
    for (i, v) in raw.models.into_iter().enumerate() {
        let label = v
            .get("id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{i}"));
        let checked = serde_json::from_value::<Model>(v)
            .map_err(|e| e.to_string())
            .and_then(|m| validate(&m).map(|()| m))
            .map(|mut m| {
                let mut names = HashSet::new();
                m.templates.retain(|t| {
                    let ok = validate_template(t)
                        .and_then(|()| {
                            names
                                .insert(t.name.clone())
                                .then_some(())
                                .ok_or_else(|| "duplicate name".to_string())
                        })
                        .map_err(|e| skipped.push(format!("{label}: template {:?}: {e}", t.name)));
                    ok.is_ok()
                });
                m.templates.truncate(MAX_TEMPLATES);
                m
            });
        match checked {
            Ok(m) if !ids.insert(m.id.clone()) => skipped.push(format!("{label}: duplicate id")),
            Ok(m) => models.push(m),
            Err(e) => skipped.push(format!("{label}: {e}")),
        }
    }
    if models.is_empty() {
        return Err(format!("no usable models ({})", skipped.join("; ")));
    }
    Ok((models, skipped))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A plain file name: the instance writes it under its models dir.
pub fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// CivitAI's own hosts. The token is only ever sent here.
pub fn is_civitai_url(u: &Url) -> bool {
    u.scheme() == "https"
        && matches!(u.host_str(), Some("civitai.com" | "www.civitai.com"))
        && u.port().is_none()
        && u.username().is_empty()
}

pub fn validate_file(f: &ModelFile) -> Result<(), String> {
    let url = Url::parse(&f.url).map_err(|_| format!("bad url for {}", f.filename))?;
    if url.scheme() != "https" {
        return Err(format!("{} is not https", f.filename));
    }
    if f.requires_civitai_token && !is_civitai_url(&url) {
        // The instance attaches the user's CivitAI key to this request.
        return Err(format!(
            "{} wants the CivitAI key but isn't a civitai.com URL",
            f.filename
        ));
    }
    if !safe_filename(&f.filename) {
        return Err(format!("unsafe file name {:?}", f.filename));
    }
    if !is_hex64(&f.sha256) {
        return Err(format!("{} has no valid sha256", f.filename));
    }
    if f.size_bytes == 0 || f.size_bytes > MAX_FILE_BYTES {
        return Err(format!("{} has an invalid size", f.filename));
    }
    Ok(())
}

/// PNG, JPEG, or WebP by magic bytes.
pub fn is_image(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n")
        || b.starts_with(&[0xFF, 0xD8, 0xFF])
        || (b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP")
}

/// A template's image, decoded, if it has a usable one.
pub fn template_image(t: &Template) -> Option<Vec<u8>> {
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD
        .decode(t.image.as_deref()?)
        .ok()?;
    (b.len() <= MAX_TEMPLATE_IMAGE && is_image(&b)).then_some(b)
}

pub fn validate_template(t: &Template) -> Result<(), String> {
    if t.name.trim().is_empty() || t.name.chars().count() > 60 || t.name.contains('\n') {
        return Err("bad name".into());
    }
    if t.positive.len() > MAX_TEMPLATE_PROMPT || t.negative.len() > MAX_TEMPLATE_PROMPT {
        return Err("prompt is too long".into());
    }
    if t.positive.matches("{prompt}").count() != 1 {
        return Err("positive must contain {prompt} once".into());
    }
    if t.image.is_some() && template_image(t).is_none() {
        return Err("bad image".into());
    }
    Ok(())
}

fn validate(m: &Model) -> Result<(), String> {
    let id_ok = !m.id.is_empty()
        && m.id.len() <= 64
        && m.id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !id_ok {
        return Err("bad id".into());
    }
    if m.name.trim().is_empty() || m.name.len() > 100 || m.description.len() > 1000 {
        return Err("bad name or description".into());
    }
    if m.base.trim().is_empty() {
        return Err("missing base".into());
    }
    if !(m.min_vram_gb > 0.0 && m.min_vram_gb <= 200.0) {
        return Err("bad min_vram_gb".into());
    }
    if m.min_compute_cap
        .is_some_and(|c| !(300..=1300).contains(&c))
    {
        return Err("bad min_compute_cap".into());
    }
    if m.min_ram_gb.is_some_and(|r| !(r > 0.0 && r <= 2000.0)) {
        return Err("bad min_ram_gb".into());
    }
    if m.price_tier.is_some_and(|t| !(1..=3).contains(&t)) {
        return Err("bad price_tier".into());
    }
    if m.good_for.len() > 60 {
        return Err("good_for is too long".into());
    }
    if !m.files.iter().any(|f| f.kind == "main") {
        return Err("no main model file".into());
    }
    let mut names = HashSet::new();
    for f in &m.files {
        validate_file(f)?;
        if !names.insert(f.filename.to_ascii_lowercase()) {
            return Err(format!("duplicate file name {}", f.filename));
        }
    }
    if let Some(v) = &m.invoke_min_version {
        let need = parse_version(v).ok_or_else(|| format!("bad invoke_min_version {v}"))?;
        let have = parse_version(INVOKE_VERSION).expect("pinned version parses");
        if need > have {
            return Err(format!(
                "needs Invoke {v}; this app runs {INVOKE_VERSION} (update SlopTweak)"
            ));
        }
    }
    Ok(())
}

pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let out = (
        it.next()?.parse().ok()?,
        it.next().unwrap_or("0").parse().ok()?,
        it.next().unwrap_or("0").parse().ok()?,
    );
    it.next().is_none().then_some(out)
}

// ----- local copies -------------------------------------------------------------

pub fn cache_path(dir: &Path) -> PathBuf {
    dir.join(CACHE_FILE)
}

fn mtime_unix(p: &Path) -> Option<u64> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// The cached online copy if it's valid, else the bundled one.
pub fn load_local(dir: &Path) -> Catalog {
    let path = cache_path(dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        match parse(&text) {
            Ok((models, skipped)) => {
                return Catalog {
                    models,
                    source: Source::Cached,
                    fetched_unix: mtime_unix(&path),
                    skipped,
                }
            }
            Err(e) => eprintln!("[sloptweak] ignoring cached catalog: {e}"),
        }
    }
    let (models, skipped) = parse(BUNDLED).expect("bundled catalog.json is valid");
    Catalog {
        models,
        source: Source::Bundled,
        fetched_unix: None,
        skipped,
    }
}

/// Validate a freshly downloaded catalog and, if usable, cache it.
pub fn accept_online(dir: &Path, text: &str, now_unix: u64) -> Result<Catalog, String> {
    let (models, skipped) = parse(text)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = cache_path(dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("couldn't cache the catalog: {e}"))?;
    Ok(Catalog {
        models,
        source: Source::Online,
        fetched_unix: Some(now_unix),
        skipped,
    })
}

/// Where to fetch from. Debug builds accept `SLOPTWEAK_CATALOG_URL` so the
/// upstream-edit path can be tested against a local server.
pub fn url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(u) = std::env::var("SLOPTWEAK_CATALOG_URL") {
        if !u.trim().is_empty() {
            return u.trim().to_string();
        }
    }
    CATALOG_URL.to_string()
}

pub async fn fetch(url: &str) -> Result<String, String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_BYTES)
    {
        return Err("catalog too large".into());
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("catalog too large".into());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| "catalog isn't UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str) -> serde_json::Value {
        json!({
            "id": id, "name": "Test", "description": "d", "base": "sdxl",
            "files": [{
                "url": "https://civitai.com/api/download/models/1",
                "filename": "t.safetensors", "sha256": "a".repeat(64),
                "size_bytes": 10, "kind": "main", "requires_civitai_token": true
            }],
            "min_vram_gb": 12, "license_note": "", "invoke_min_version": "6.13.8"
        })
    }

    fn catalog(models: Vec<serde_json::Value>) -> String {
        json!({"schema_version": 1, "models": models}).to_string()
    }

    #[test]
    fn bundled_catalog_parses() {
        let models = bundled();
        let m = models.iter().find(|m| m.id == "banana-splitz-xxl").unwrap();
        assert!(m.needs_civitai());
        assert_eq!(m.total_bytes(), 6_938_043_264);
        assert_eq!(m.files[0].sha256.len(), 64);
        assert_eq!(m.invoke_min_version.as_deref(), Some("6.13.8"));
        assert_eq!(m.min_compute_cap, None);
    }

    #[test]
    fn bundled_templates_are_valid() {
        let text = BUNDLED;
        let (models, skipped) = parse(text).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        for m in &models {
            for t in &m.templates {
                assert!(template_image(t).is_some(), "{} / {}", m.id, t.name);
            }
        }
        let with = |id: &str| models.iter().find(|m| m.id == id).unwrap().templates.len();
        assert_eq!(with("banana-splitz-xxl"), 1);
        assert_eq!(with("anima-aesthetic"), 1);
        assert_eq!(
            with("anima-turbo"),
            0,
            "CFG 1 ignores negatives; no shared positive tags"
        );
        let krea = &models
            .iter()
            .find(|m| m.id == "krea-2-turbo")
            .unwrap()
            .templates;
        let kroma = &models
            .iter()
            .find(|m| m.id == "kroma-turbo")
            .unwrap()
            .templates;
        assert_eq!(krea, kroma, "Kroma reuses Krea 2's set");
    }

    #[test]
    fn bad_templates_are_dropped_not_the_model() {
        let mut e = entry("t");
        let img = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF, 0xE0, 1, 2])
        };
        e["templates"] = json!([
            {"name": "ok", "positive": "a, {prompt}", "image": img},
            {"name": "no slot", "positive": "a"},
            {"name": "two slots", "positive": "{prompt} {prompt}"},
            {"name": "not an image", "positive": "{prompt}", "image": "PGh0bWw+"},
            {"name": "ok", "positive": "dup {prompt}"},
            {"name": "", "positive": "{prompt}"}
        ]);
        let (models, skipped) = parse(&catalog(vec![e])).unwrap();
        let names: Vec<&str> = models[0]
            .templates
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["ok"]);
        assert_eq!(skipped.len(), 5, "{skipped:?}");
    }

    #[test]
    fn bundled_anima_entries_are_complete() {
        let models = bundled();
        for id in ["anima-aesthetic", "anima-turbo"] {
            let m = models.iter().find(|m| m.id == id).unwrap();
            assert_eq!(m.base, "anima");
            assert_eq!(m.min_compute_cap, Some(800));
            assert!(!m.needs_civitai(), "Anima is on Hugging Face, no key");
            let kinds: Vec<&str> = m.files.iter().map(|f| f.kind.as_str()).collect();
            assert_eq!(kinds, ["main", "text_encoder", "vae"]);
            assert_eq!(m.total_bytes(), 4_182_230_656 + 1_192_135_096 + 253_806_246);
            for f in &m.files {
                assert!(f
                    .url
                    .contains("/resolve/f973fc41ec7545364ac9776c2440285f43ff2a30/"));
            }
        }
    }

    #[test]
    fn bundled_krea2_entries_are_complete() {
        let models = bundled();
        for id in ["krea-2-turbo", "kroma-turbo"] {
            let m = models.iter().find(|m| m.id == id).unwrap();
            assert_eq!(m.base, "krea-2");
            assert_eq!(m.min_compute_cap, Some(800));
            assert!(
                m.min_ram_gb.is_some_and(|r| r >= 48.0),
                "streams from host RAM"
            );
            assert!(!m.needs_civitai());
            let kinds: Vec<&str> = m.files.iter().map(|f| f.kind.as_str()).collect();
            assert_eq!(kinds, ["main", "text_encoder", "vae"]);
            // Invoke reads the Turbo variant from the file name.
            assert!(m.files[0].filename.contains("turbo"));
        }
    }

    #[test]
    fn bundled_entries_are_labelled() {
        for m in bundled() {
            assert!(m.price_tier.is_some(), "{} has no price tier", m.id);
            assert!(!m.good_for.is_empty(), "{} has no good_for", m.id);
        }
    }

    #[test]
    fn bad_entries_are_skipped_not_fatal() {
        let mut token_elsewhere = entry("leak");
        token_elsewhere["files"][0]["url"] = json!("https://evil.example/x");
        let mut traversal = entry("trav");
        traversal["files"][0]["filename"] = json!("../../etc/x");
        let mut no_hash = entry("nohash");
        no_hash["files"][0]["sha256"] = json!("");
        let mut http = entry("http");
        http["files"][0]["url"] = json!("http://civitai.com/x");
        let mut too_new = entry("new");
        too_new["invoke_min_version"] = json!("99.0.0");
        let mut no_main = entry("nomain");
        no_main["files"][0]["kind"] = json!("lora");
        let mut bad_cap = entry("cap");
        bad_cap["min_compute_cap"] = json!(86);
        let mut bad_id = entry("Bad Id");
        bad_id["id"] = json!("Bad Id");
        let mut bad_tier = entry("tier");
        bad_tier["price_tier"] = json!(4);
        let mut bad_ram = entry("ram");
        bad_ram["min_ram_gb"] = json!(-1);
        let text = catalog(vec![
            entry("ok"),
            token_elsewhere,
            traversal,
            no_hash,
            http,
            too_new,
            no_main,
            bad_cap,
            bad_id,
            bad_tier,
            bad_ram,
            entry("ok"),
            json!({"id": "junk"}),
        ]);
        let (models, skipped) = parse(&text).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "ok");
        assert_eq!(skipped.len(), 12, "{skipped:?}");
        assert!(skipped.iter().any(|s| s.contains("wants the CivitAI key")));
        assert!(skipped.iter().any(|s| s.starts_with("ok: duplicate")));
        assert!(skipped.iter().any(|s| s.contains("needs Invoke 99.0.0")));
    }

    #[test]
    fn token_free_files_may_live_elsewhere() {
        let mut e = entry("hf");
        e["files"][0]["url"] = json!("https://huggingface.co/x/y.safetensors");
        e["files"][0]["requires_civitai_token"] = json!(false);
        assert_eq!(parse(&catalog(vec![e])).unwrap().0.len(), 1);
    }

    #[test]
    fn empty_or_foreign_catalogs_are_rejected() {
        assert!(parse(&catalog(vec![])).is_err());
        assert!(parse(&catalog(vec![json!({"id": "x"})])).is_err());
        assert!(parse(r#"{"schema_version": 2, "models": []}"#).is_err());
        assert!(parse("<html>").is_err());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let mut e = entry("extra");
        e["future_field"] = json!({"a": 1});
        e["files"][0]["future"] = json!(true);
        assert_eq!(parse(&catalog(vec![e])).unwrap().0.len(), 1);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("6.14.1"), Some((6, 14, 1)));
        assert_eq!(parse_version("v6.13"), Some((6, 13, 0)));
        assert_eq!(parse_version("6.x"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert!(parse_version(INVOKE_VERSION).is_some());
    }

    #[test]
    fn local_fallback_order() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing cached: bundled.
        let c = load_local(dir.path());
        assert_eq!(c.source, Source::Bundled);
        assert!(c.find("banana-splitz-xxl").is_some());

        // A fetched catalog is cached and wins next time.
        let online = accept_online(dir.path(), &catalog(vec![entry("fresh")]), 123).unwrap();
        assert_eq!(online.source, Source::Online);
        let c = load_local(dir.path());
        assert_eq!(c.source, Source::Cached);
        assert_eq!(c.models[0].id, "fresh");
        assert!(c.fetched_unix.is_some());

        // A bad download is rejected and doesn't replace the cache.
        assert!(accept_online(dir.path(), "{}", 124).is_err());
        assert_eq!(load_local(dir.path()).models[0].id, "fresh");

        // A corrupt cache falls back to bundled.
        std::fs::write(cache_path(dir.path()), "garbage").unwrap();
        assert_eq!(load_local(dir.path()).source, Source::Bundled);
    }

    #[test]
    fn civitai_host_check() {
        for ok in [
            "https://civitai.com/api/download/models/1",
            "https://www.civitai.com/x",
        ] {
            assert!(is_civitai_url(&Url::parse(ok).unwrap()), "{ok}");
        }
        for bad in [
            "http://civitai.com/x",
            "https://civitai.com.evil.com/x",
            "https://evilcivitai.com/x",
            "https://civitai.com:8443/x",
            "https://user@civitai.com/x",
        ] {
            assert!(!is_civitai_url(&Url::parse(bad).unwrap()), "{bad}");
        }
    }
}
