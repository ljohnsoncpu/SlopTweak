//! Output sync: copy the images a session makes to the output folder.
//!
//! Runs in Rust through the sidecar with the launch secret, never through the
//! remote webview. What gets saved (user decision, 2026-09-25):
//! * gallery images (`general`, `is_intermediate=false`) → the output folder;
//! * every Canvas result, accepted or not → its `Canvas` subfolder. Canvas
//!   **Accept** only turns a result into a layer; it never reaches the
//!   gallery (findings, Phase 4).
//!
//! Canvas results are found through the session queue, not the image list:
//! an image record's `node_id` is the *prepared* node id (a fresh uuid), so
//! only the queue item's `session.prepared_source_mapping` says which image
//! came from the `canvas_output:*` node. Of those, only intermediates are
//! Canvas tries; a non-intermediate one ("Send To Gallery" mode) is already
//! in the gallery. Uploads (`user` category, e.g. the tutorial sample) and
//! graph scratch images are never saved.
//!
//! What's done is kept per instance in `synced/<id>.json`, so a restart and
//! reattach doesn't download anything again.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::persist::now_unix;
use crate::sidecar::{InvokeImage, SidecarApi, SidecarError};

/// Subfolder for Canvas results.
pub const CANVAS_DIR: &str = "Canvas";
/// Invoke allows up to 1000 per page.
const PAGE: u64 = 100;
/// Download failures before an image is given up on for this session.
const MAX_TRIES: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Gallery,
    Canvas,
}

/// What output sync needs from one session queue item.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueueOutputs {
    /// Completed, failed, or canceled: nothing more will appear.
    pub finished: bool,
    /// Images made by the item's `canvas_output:*` node(s).
    pub canvas_images: Vec<String>,
}

/// Parse `GET /api/v1/queue/default/i/{id}` (Invoke 6.14.1). A source node
/// id is `canvas_output:<id>` (Invoke's `isCanvasOutputNodeId`); results are
/// keyed by prepared id, `{"image": {"image_name": …}}`.
pub fn canvas_outputs(item: &Value) -> QueueOutputs {
    let finished = matches!(
        item["status"].as_str(),
        Some("completed" | "failed" | "canceled")
    );
    let session = &item["session"];
    let mut canvas_images: Vec<String> = session["prepared_source_mapping"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, src)| {
            src.as_str()
                .is_some_and(|s| s.split(':').next() == Some("canvas_output"))
        })
        .filter_map(|(prepared, _)| {
            session["results"][prepared.as_str()]["image"]["image_name"]
                .as_str()
                .map(String::from)
        })
        .collect();
    canvas_images.sort();
    canvas_images.dedup();
    QueueOutputs {
        finished,
        canvas_images,
    }
}

/// Invoke names images `<uuid>.png`. Anything else is refused rather than
/// turned into a path.
pub fn valid_image_name(name: &str) -> bool {
    let ext_ok = name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && ["png", "jpg", "jpeg", "webp"].contains(&ext.to_ascii_lowercase().as_str())
    });
    (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && ext_ok
}

/// Invoke's `created_at` (SQLite UTC, `YYYY-MM-DD HH:MM:SS[.fff]`) as local
/// time for the file name.
fn local_stamp(created_at: &str) -> Option<String> {
    let t = created_at.trim().replace('T', " ");
    let t = t.trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    let local = Utc.from_utc_datetime(&naive).with_timezone(&Local);
    Some(local.format("%Y-%m-%d_%H-%M-%S").to_string())
}

/// `2026-09-25_14-03-22_<image_name>`: sorts by time in Explorer and stays
/// unique. Always prefixed, so it can't be a reserved name like `con.png`.
pub fn file_name(img: &InvokeImage) -> Option<String> {
    if !valid_image_name(&img.image_name) {
        return None;
    }
    Some(match img.created_at.as_deref().and_then(local_stamp) {
        Some(stamp) => format!("{stamp}_{}", img.image_name),
        None => format!("image_{}", img.image_name),
    })
}

/// PNG, JPEG, or WebP by magic bytes, so an error page is never saved as an image.
pub fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n")
        || b.starts_with(&[0xFF, 0xD8, 0xFF])
        || (b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP")
}

/// Write via a temp file and rename, so a half-written image never appears.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

// ----- ledger ------------------------------------------------------------------

/// What's already handled on one instance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    pub gallery: BTreeSet<String>,
    pub canvas: BTreeSet<String>,
    /// Seen and deliberately not saved (already in the gallery, gone, given up).
    pub ignored: BTreeSet<String>,
    /// Finished queue items whose Canvas results are all handled.
    pub queue_done: BTreeSet<u64>,
}

impl Ledger {
    fn knows(&self, name: &str) -> bool {
        self.gallery.contains(name) || self.canvas.contains(name) || self.ignored.contains(name)
    }
}

/// `<data_dir>/synced/<instance id>.json`.
#[derive(Debug, Clone)]
pub struct SyncStore {
    dir: PathBuf,
}

impl SyncStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("synced"),
        }
    }

    fn path(&self, instance_id: u64) -> PathBuf {
        self.dir.join(format!("{instance_id}.json"))
    }

    pub fn load(&self, instance_id: u64) -> Ledger {
        std::fs::read_to_string(self.path(instance_id))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, instance_id: u64, ledger: &Ledger) -> std::io::Result<()> {
        let path = self.path(instance_id);
        std::fs::create_dir_all(&self.dir)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(ledger)?)?;
        std::fs::rename(&tmp, &path)
    }

    /// The instance is gone; its ledger means nothing any more.
    pub fn remove(&self, instance_id: u64) {
        let _ = std::fs::remove_file(self.path(instance_id));
    }
}

// ----- report ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Polling while Invoke is open.
    Running,
    /// The last pass before the GPU is shut down.
    Finishing,
    /// Everything that existed was saved before shutdown.
    Done,
    /// Shut down with images still missing (timeout or errors).
    Incomplete,
}

/// What the UI shows about saving.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    pub folder: String,
    pub phase: Phase,
    /// Gallery images saved from this instance.
    pub saved: usize,
    /// Canvas results saved from this instance.
    pub canvas_saved: usize,
    /// Images that exist on the GPU but aren't saved (yet).
    pub missing: usize,
    pub last_error: Option<String>,
    pub last_sync_unix: Option<u64>,
}

// ----- the syncer --------------------------------------------------------------

pub type OutputDir = Arc<dyn Fn() -> PathBuf + Send + Sync>;

pub struct OutputSync {
    sidecar: Arc<dyn SidecarApi>,
    base: Url,
    secret: String,
    instance_id: u64,
    store: SyncStore,
    output_dir: OutputDir,
    ledger: Ledger,
    tries: HashMap<String, u32>,
    report: SyncReport,
}

#[derive(Default)]
struct Tally {
    saved: usize,
    missing: usize,
    error: Option<String>,
}

enum Outcome {
    Saved(Kind),
    /// Don't try again (not ours, bad name, deleted in Invoke, gave up),
    /// with a message for the user if it's worth one.
    Ignore(Option<String>),
    /// Try again next pass.
    Retry(String),
}

impl OutputSync {
    pub fn new(
        sidecar: Arc<dyn SidecarApi>,
        base: Url,
        secret: String,
        instance_id: u64,
        store: SyncStore,
        output_dir: OutputDir,
    ) -> Self {
        let ledger = store.load(instance_id);
        let report = SyncReport {
            folder: output_dir().to_string_lossy().into_owned(),
            phase: Phase::Running,
            saved: ledger.gallery.len(),
            canvas_saved: ledger.canvas.len(),
            missing: 0,
            last_error: None,
            last_sync_unix: None,
        };
        Self {
            sidecar,
            base,
            secret,
            instance_id,
            store,
            output_dir,
            ledger,
            tries: HashMap::new(),
            report,
        }
    }

    pub fn report(&self) -> SyncReport {
        self.report.clone()
    }

    pub fn set_phase(&mut self, phase: Phase) {
        self.report.phase = phase;
    }

    /// One pass over the gallery, then over the queue (Canvas results). A
    /// quick pass stops at the first gallery page / queue item with nothing
    /// new; a `full` pass (the last one before shutdown) reads everything.
    /// Returns how many images were saved.
    pub async fn pass(&mut self, full: bool) -> Result<usize, SidecarError> {
        let folder = (self.output_dir)();
        self.report.folder = folder.to_string_lossy().into_owned();
        let mut t = Tally::default();
        let result = match self.gallery_pass(full, &folder, &mut t).await {
            Ok(()) => self.canvas_pass(full, &folder, &mut t).await,
            Err(e) => Err(e),
        };
        self.report.saved = self.ledger.gallery.len();
        self.report.canvas_saved = self.ledger.canvas.len();
        self.report.missing = t.missing;
        self.report.last_error = match &result {
            Err(e) => Some(format!("Couldn't reach the GPU: {e}")),
            Ok(()) => t.error,
        };
        if result.is_ok() {
            self.report.last_sync_unix = Some(now_unix());
        }
        self.persist();
        result.map(|()| t.saved)
    }

    async fn gallery_pass(
        &mut self,
        full: bool,
        folder: &Path,
        t: &mut Tally,
    ) -> Result<(), SidecarError> {
        let mut offset = 0;
        loop {
            let page = self
                .sidecar
                .list_images(&self.base, &self.secret, offset, PAGE)
                .await?;
            let mut fresh = 0;
            for img in &page.items {
                if self.ledger.knows(&img.image_name) {
                    continue;
                }
                fresh += 1;
                let outcome = self.fetch(img, Kind::Gallery, folder).await;
                self.record(&img.image_name, outcome, t);
            }
            offset += page.items.len() as u64;
            if page.items.is_empty() || offset >= page.total || (!full && fresh == 0) {
                return Ok(());
            }
        }
    }

    async fn canvas_pass(
        &mut self,
        full: bool,
        folder: &Path,
        t: &mut Tally,
    ) -> Result<(), SidecarError> {
        let ids = self
            .sidecar
            .queue_item_ids(&self.base, &self.secret)
            .await?;
        for id in ids {
            if self.ledger.queue_done.contains(&id) {
                // Newest first: items older than a handled one were handled too.
                if full {
                    continue;
                }
                break;
            }
            let item = match self.sidecar.queue_item(&self.base, &self.secret, id).await {
                Ok(item) => item,
                // Deleted (queue cleared) between the two calls.
                Err(SidecarError::Http(404)) => continue,
                Err(e) => return Err(e),
            };
            let out = canvas_outputs(&item);
            if !out.finished {
                continue;
            }
            let mut all_handled = true;
            for name in &out.canvas_images {
                if self.ledger.knows(name) {
                    continue;
                }
                let outcome = match self
                    .sidecar
                    .image_info(&self.base, &self.secret, name)
                    .await
                {
                    // Already in the gallery ("Send To Gallery" mode).
                    Ok(info) if !info.is_intermediate => Outcome::Ignore(None),
                    Ok(info) => self.fetch(&info, Kind::Canvas, folder).await,
                    Err(SidecarError::Http(404)) => Outcome::Ignore(None),
                    Err(e) => Outcome::Retry(format!("Couldn't look up a Canvas image: {e}")),
                };
                all_handled &= self.record(name, outcome, t);
            }
            if all_handled {
                self.ledger.queue_done.insert(id);
            }
        }
        Ok(())
    }

    /// Book one image's outcome. Returns false if it should be tried again.
    fn record(&mut self, name: &str, outcome: Outcome, t: &mut Tally) -> bool {
        match outcome {
            Outcome::Saved(kind) => {
                let set = match kind {
                    Kind::Gallery => &mut self.ledger.gallery,
                    Kind::Canvas => &mut self.ledger.canvas,
                };
                set.insert(name.to_string());
                t.saved += 1;
                true
            }
            Outcome::Ignore(why) => {
                t.error = why.or(t.error.take());
                self.ledger.ignored.insert(name.to_string());
                true
            }
            Outcome::Retry(why) => {
                t.missing += 1;
                t.error = Some(why);
                false
            }
        }
    }

    fn persist(&self) {
        if let Err(e) = self.store.save(self.instance_id, &self.ledger) {
            eprintln!("[sloptweak] couldn't save the sync ledger: {e}");
        }
    }

    /// Download one image and write it.
    async fn fetch(&mut self, img: &InvokeImage, kind: Kind, folder: &Path) -> Outcome {
        let Some(name) = file_name(img) else {
            return Outcome::Ignore(Some(format!(
                "Skipped an image with an odd name ({:?})",
                img.image_name
            )));
        };
        let bytes = match self
            .sidecar
            .image_file(&self.base, &self.secret, &img.image_name)
            .await
        {
            Ok(b) if looks_like_image(&b) => b,
            // Deleted in Invoke between listing and download.
            Err(SidecarError::Http(404)) => return Outcome::Ignore(None),
            other => {
                let why = match other {
                    Ok(_) => "the GPU sent something that isn't an image".to_string(),
                    Err(e) => e.to_string(),
                };
                let n = self.tries.entry(img.image_name.clone()).or_default();
                *n += 1;
                return if *n >= MAX_TRIES {
                    Outcome::Ignore(Some(format!(
                        "Gave up on an image after {MAX_TRIES} tries: {why}"
                    )))
                } else {
                    Outcome::Retry(format!("Couldn't download an image: {why}"))
                };
            }
        };
        let dir = match kind {
            Kind::Gallery => folder.to_path_buf(),
            Kind::Canvas => folder.join(CANVAS_DIR),
        };
        match write_atomic(&dir.join(name), &bytes) {
            Ok(()) => Outcome::Saved(kind),
            // Disk problems don't count as tries: they aren't the image's fault.
            Err(e) => Outcome::Retry(format!("Couldn't save to {}: {e}", dir.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(name: &str) -> InvokeImage {
        InvokeImage {
            image_name: name.into(),
            created_at: Some("2026-09-25 14:03:22.123".into()),
            is_intermediate: false,
        }
    }

    /// Shaped like Invoke 6.14.1's `GET /api/v1/queue/default/i/{id}` for a
    /// canvas inpaint (trimmed): results are keyed by prepared (uuid) ids.
    #[test]
    fn canvas_outputs_follow_the_source_mapping() {
        let item = serde_json::json!({
            "item_id": 3, "status": "completed", "destination": "canvas_session_x",
            "session": {
                "prepared_source_mapping": {
                    "5b1e-prep-a": "canvas_output:Qm1xT0",
                    "77aa-prep-b": "l2i:2c9",
                    "90cc-prep-c": "create_gradient_mask:8f"
                },
                "results": {
                    "5b1e-prep-a": {"type": "image_output", "image": {"image_name": "out-1.png"}},
                    "77aa-prep-b": {"type": "image_output", "image": {"image_name": "scratch.png"}},
                    "90cc-prep-c": {"type": "gradient_mask_output", "denoise_mask": {"mask_name": "m.png"}}
                }
            }
        });
        let out = canvas_outputs(&item);
        assert!(out.finished);
        assert_eq!(out.canvas_images, vec!["out-1.png".to_string()]);
        let mut pending = item.clone();
        pending["status"] = "in_progress".into();
        assert!(!canvas_outputs(&pending).finished);
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"status": "completed", "session": null}),
            serde_json::json!({"status": "completed", "session": {
                "prepared_source_mapping": {"p": "canvas_outputx:1"},
                "results": {"p": {"image": {"image_name": "x.png"}}}}}),
        ] {
            assert!(canvas_outputs(&bad).canvas_images.is_empty(), "{bad}");
        }
    }

    #[test]
    fn image_names_are_checked() {
        for ok in [
            "4f0c1b2a-9d7e-4c1b-8f00-1234567890ab.png",
            "x.JPG",
            "a_b-c.webp",
        ] {
            assert!(valid_image_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".png",
            "..",
            "../x.png",
            "a/b.png",
            "a\\b.png",
            "c:x.png",
            "x.png.exe",
            "x",
            "x.svg",
            "con .png",
            &format!("{}.png", "a".repeat(130)),
        ] {
            assert!(!valid_image_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn file_names_carry_local_time() {
        let n = file_name(&img("abc.png")).unwrap();
        let expected = Utc
            .with_ymd_and_hms(2026, 9, 25, 14, 3, 22)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d_%H-%M-%S_abc.png")
            .to_string();
        assert_eq!(n, expected);
        let mut i = img("abc.png");
        i.created_at = Some("2026-09-25T14:03:22Z".into());
        assert_eq!(file_name(&i).unwrap(), expected);
        i.created_at = Some("yesterday".into());
        assert_eq!(file_name(&i).unwrap(), "image_abc.png");
        i.image_name = "../evil.png".into();
        assert_eq!(file_name(&i), None);
    }

    #[test]
    fn magic_bytes() {
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\n...."));
        assert!(looks_like_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_image(b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!looks_like_image(b"<html>502 Bad Gateway"));
        assert!(!looks_like_image(b""));
    }

    #[test]
    fn ledger_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SyncStore::new(dir.path());
        assert_eq!(store.load(7), Ledger::default());
        let mut l = Ledger::default();
        l.gallery.insert("a.png".into());
        l.ignored.insert("b.png".into());
        l.queue_done.insert(4);
        store.save(7, &l).unwrap();
        assert_eq!(store.load(7), l);
        assert!(store.load(7).knows("b.png"));
        assert_eq!(store.load(8), Ledger::default());
        store.remove(7);
        assert_eq!(store.load(7), Ledger::default());
    }
}
