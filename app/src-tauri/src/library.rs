//! Prompt templates and workflows that outlive the GPU.
//!
//! Invoke keeps both in the instance's database, which is destroyed with the
//! instance. So the app keeps a copy on this PC (`library.json` in the config
//! dir) and, through the sidecar with the launch secret:
//! * when Invoke is ready, puts back every saved template and workflow, plus
//!   the running model's built-in templates from the catalog;
//! * while it runs and in the last pass before shutdown, saves what the user
//!   added, changed, renamed, or deleted.
//!
//! Only things this session saw are treated as deleted, so a failed restore
//! never wipes the library. A built-in template the user deletes stays
//! hidden; one they edit is saved as their own under the same name.
//!
//! Invoke 6.14.1 API (findings): templates are "style presets",
//! `GET/POST /api/v1/style_presets/` (POST is multipart: a `data` JSON field
//! and an optional `image` file), `GET …/i/{id}/image`; workflows are
//! `GET /api/v1/workflows/?categories=user`, `GET …/i/{id}`, and
//! `POST /api/v1/workflows/` with `{"workflow": …}` (no `id`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::catalog::{self, Model};
use crate::sidecar::{SidecarApi, SidecarError};

/// Invoke saves template images as 256 px WebP thumbnails; far bigger is not one.
const MAX_IMAGE: usize = 512 * 1024;
/// One workflow's JSON.
pub const MAX_WORKFLOW: usize = 8 * 1024 * 1024;
const WORKFLOW_PAGE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTemplate {
    pub name: String,
    pub positive: String,
    #[serde(default)]
    pub negative: String,
    /// Base64 image bytes (PNG/JPEG/WebP).
    #[serde(default)]
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkflow {
    pub name: String,
    /// Invoke's workflow JSON without its `id`.
    pub workflow: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// Made (or edited) by the user.
    pub templates: Vec<SavedTemplate>,
    pub workflows: Vec<SavedWorkflow>,
    /// Built-in templates the user deleted; not put back.
    pub hidden_builtins: BTreeSet<String>,
}

impl Library {
    fn put_template(&mut self, t: SavedTemplate) {
        match self.templates.iter_mut().find(|x| x.name == t.name) {
            Some(x) => *x = t,
            None => self.templates.push(t),
        }
    }

    fn put_workflow(&mut self, w: SavedWorkflow) {
        match self.workflows.iter_mut().find(|x| x.name == w.name) {
            Some(x) => *x = w,
            None => self.workflows.push(w),
        }
    }
}

/// `<config_dir>/library.json`.
#[derive(Debug, Clone)]
pub struct LibraryStore {
    path: PathBuf,
}

impl LibraryStore {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join("library.json"),
        }
    }

    pub fn load(&self) -> Library {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, lib: &Library) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(lib)?)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// The built-in templates for a model id, named as they appear in Invoke.
pub type Builtins = Arc<dyn Fn(&str) -> Vec<SavedTemplate> + Send + Sync>;

/// A catalog model's templates, as Invoke shows them: "Model · Style".
pub fn builtins_of(model: &Model) -> Vec<SavedTemplate> {
    model
        .templates
        .iter()
        .map(|t| SavedTemplate {
            name: format!("{} · {}", model.name, t.name),
            positive: t.positive.clone(),
            negative: t.negative.clone(),
            image: catalog::template_image(t)
                .map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
        })
        .collect()
}

/// What we last saw of one Invoke template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PresetSnap {
    name: String,
    positive: String,
    negative: String,
    image_url: Option<String>,
}

/// A user template in `GET /api/v1/style_presets/`, or None for Invoke's own.
fn parse_preset(v: &Value) -> Option<(String, PresetSnap)> {
    if v["type"].as_str() != Some("user") {
        return None;
    }
    Some((
        v["id"].as_str()?.to_string(),
        PresetSnap {
            name: v["name"].as_str()?.to_string(),
            positive: v["preset_data"]["positive_prompt"].as_str()?.to_string(),
            negative: v["preset_data"]["negative_prompt"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            image_url: v["image"].as_str().map(String::from),
        },
    ))
}

/// What changed in one capture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub templates: usize,
    pub workflows: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restored {
    pub templates: usize,
    pub workflows: usize,
    pub failed: usize,
}

pub struct LibrarySync {
    sidecar: Arc<dyn SidecarApi>,
    base: Url,
    secret: String,
    store: LibraryStore,
    builtins: Vec<SavedTemplate>,
    /// Invoke id → last seen, for templates and workflows this session knows.
    presets: HashMap<String, PresetSnap>,
    /// workflow id → (name, updated_at).
    workflows: HashMap<String, (String, String)>,
}

impl LibrarySync {
    pub fn new(
        sidecar: Arc<dyn SidecarApi>,
        base: Url,
        secret: String,
        store: LibraryStore,
        builtins: Vec<SavedTemplate>,
    ) -> Self {
        Self {
            sidecar,
            base,
            secret,
            store,
            builtins,
            presets: HashMap::new(),
            workflows: HashMap::new(),
        }
    }

    async fn list_presets(&self) -> Result<HashMap<String, PresetSnap>, SidecarError> {
        let v = self
            .sidecar
            .api_json(&self.base, &self.secret, &["style_presets", ""], &[])
            .await?;
        let items = v
            .as_array()
            .ok_or_else(|| SidecarError::Parse("style presets: not a list".into()))?;
        Ok(items.iter().filter_map(parse_preset).collect())
    }

    /// Every user workflow: id → (name, updated_at).
    async fn list_workflows(&self) -> Result<HashMap<String, (String, String)>, SidecarError> {
        let mut out = HashMap::new();
        let per = WORKFLOW_PAGE.to_string();
        for page in 0..50 {
            let p = page.to_string();
            let v = self
                .sidecar
                .api_json(
                    &self.base,
                    &self.secret,
                    &["workflows", ""],
                    &[("categories", "user"), ("page", &p), ("per_page", &per)],
                )
                .await?;
            let items = v["items"]
                .as_array()
                .ok_or_else(|| SidecarError::Parse("workflows: no items".into()))?;
            for w in items {
                if let (Some(id), Some(name)) = (w["workflow_id"].as_str(), w["name"].as_str()) {
                    let updated = w["updated_at"].as_str().unwrap_or_default().to_string();
                    out.insert(id.to_string(), (name.to_string(), updated));
                }
            }
            let pages = v["pages"].as_u64().unwrap_or(1);
            if items.len() < WORKFLOW_PAGE || page + 1 >= pages {
                break;
            }
        }
        Ok(out)
    }

    /// Put the library (and this model's built-ins) into a fresh Invoke.
    /// Anything already there by name is left alone, so it's safe to repeat.
    pub async fn restore(&mut self) -> Result<Restored, SidecarError> {
        let lib = self.store.load();
        let mut done = Restored::default();

        let existing = self.list_presets().await?;
        let mut names: HashSet<String> = existing.values().map(|p| p.name.clone()).collect();
        self.presets = existing;
        let mine: HashSet<&str> = lib.templates.iter().map(|t| t.name.as_str()).collect();
        let wanted =
            lib.templates.iter().chain(self.builtins.iter().filter(|b| {
                !mine.contains(b.name.as_str()) && !lib.hidden_builtins.contains(&b.name)
            }));
        for t in wanted.cloned().collect::<Vec<_>>() {
            if !names.insert(t.name.clone()) {
                continue;
            }
            match self.create_preset(&t).await {
                Ok(()) => done.templates += 1,
                Err(e) => {
                    eprintln!("[sloptweak] couldn't restore template {:?}: {e}", t.name);
                    done.failed += 1;
                }
            }
        }

        let existing = self.list_workflows().await?;
        let mut names: HashSet<String> = existing.values().map(|(n, _)| n.clone()).collect();
        self.workflows = existing;
        for w in &lib.workflows {
            if !names.insert(w.name.clone()) {
                continue;
            }
            let body = json!({ "workflow": w.workflow });
            match self
                .sidecar
                .api_post_json(&self.base, &self.secret, &["workflows", ""], &body)
                .await
            {
                Ok(r) => {
                    if let Some(id) = r["workflow_id"].as_str() {
                        let updated = r["updated_at"].as_str().unwrap_or_default().to_string();
                        self.workflows.insert(id.into(), (w.name.clone(), updated));
                    }
                    done.workflows += 1;
                }
                Err(e) => {
                    eprintln!("[sloptweak] couldn't restore workflow {:?}: {e}", w.name);
                    done.failed += 1;
                }
            }
        }
        Ok(done)
    }

    async fn create_preset(&mut self, t: &SavedTemplate) -> Result<(), SidecarError> {
        let data = json!({
            "name": t.name,
            "positive_prompt": t.positive,
            "negative_prompt": t.negative,
            "type": "user",
        })
        .to_string();
        let image = t
            .image
            .as_deref()
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .filter(|b| catalog::is_image(b));
        let r = self
            .sidecar
            .api_post_form(
                &self.base,
                &self.secret,
                &["style_presets", ""],
                &[("data", data)],
                image.map(|b| ("image", b)),
            )
            .await?;
        if let Some((id, snap)) = parse_preset(&r) {
            self.presets.insert(id, snap);
        }
        Ok(())
    }

    fn builtin(&self, name: &str) -> Option<&SavedTemplate> {
        self.builtins.iter().find(|b| b.name == name)
    }

    /// Save what changed in Invoke since the last look.
    pub async fn capture(&mut self) -> Result<Captured, SidecarError> {
        let presets = self.list_presets().await?;
        let workflows = self.list_workflows().await?;
        let mut lib = self.store.load();
        let before = lib.clone();
        let mut got = Captured::default();

        // Templates.
        for (id, old) in &self.presets {
            let renamed = presets.get(id).is_some_and(|p| p.name != old.name);
            if presets.contains_key(id) && !renamed {
                continue;
            }
            // Deleted (or renamed away from `old.name`).
            lib.templates.retain(|t| t.name != old.name);
            if self.builtin(&old.name).is_some() {
                lib.hidden_builtins.insert(old.name.clone());
            }
            got.removed += 1;
        }
        for (id, snap) in &presets {
            if self.presets.get(id) == Some(snap) {
                continue;
            }
            let as_builtin = self
                .builtin(&snap.name)
                .is_some_and(|b| b.positive == snap.positive && b.negative == snap.negative);
            if as_builtin {
                lib.hidden_builtins.remove(&snap.name);
                continue;
            }
            let image = match &snap.image_url {
                Some(_) => self.preset_image(id).await,
                None => None,
            };
            lib.put_template(SavedTemplate {
                name: snap.name.clone(),
                positive: snap.positive.clone(),
                negative: snap.negative.clone(),
                image,
            });
            got.templates += 1;
        }
        self.presets = presets;

        // Workflows.
        for (id, (old_name, _)) in &self.workflows {
            let renamed = workflows.get(id).is_some_and(|(n, _)| n != old_name);
            if workflows.contains_key(id) && !renamed {
                continue;
            }
            lib.workflows.retain(|w| &w.name != old_name);
            got.removed += 1;
        }
        let mut seen = HashMap::new();
        for (id, (name, updated)) in workflows {
            if self.workflows.get(&id) == Some(&(name.clone(), updated.clone())) {
                seen.insert(id, (name, updated));
                continue;
            }
            match self.fetch_workflow(&id).await {
                Ok(mut wf) => {
                    if let Some(o) = wf.as_object_mut() {
                        o.remove("id");
                    }
                    lib.put_workflow(SavedWorkflow {
                        name: name.clone(),
                        workflow: wf,
                    });
                    got.workflows += 1;
                    seen.insert(id, (name, updated));
                }
                // Deleted between the calls: the next look sees it gone.
                Err(SidecarError::Http(404)) => {}
                // Try again next time (it isn't in `seen`).
                Err(e) => eprintln!("[sloptweak] couldn't read workflow {name:?}: {e}"),
            }
        }
        self.workflows = seen;

        if lib != before {
            self.store
                .save(&lib)
                .map_err(|e| SidecarError::Parse(format!("couldn't save the library: {e}")))?;
        }
        Ok(got)
    }

    async fn preset_image(&self, id: &str) -> Option<String> {
        let b = self
            .sidecar
            .api_bytes(
                &self.base,
                &self.secret,
                &["style_presets", "i", id, "image"],
                MAX_IMAGE,
            )
            .await
            .ok()?;
        catalog::is_image(&b).then(|| base64::engine::general_purpose::STANDARD.encode(b))
    }

    async fn fetch_workflow(&self, id: &str) -> Result<Value, SidecarError> {
        let r = self
            .sidecar
            .api_json(&self.base, &self.secret, &["workflows", "i", id], &[])
            .await?;
        let wf = r["workflow"].clone();
        if !wf.is_object() {
            return Err(SidecarError::Parse("workflow: missing".into()));
        }
        if serde_json::to_vec(&wf).map_or(0, |b| b.len()) > MAX_WORKFLOW {
            return Err(SidecarError::Parse("workflow is too large".into()));
        }
        Ok(wf)
    }
}

#[cfg(test)]
mod tests;
