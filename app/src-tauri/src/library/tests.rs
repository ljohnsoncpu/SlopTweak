use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::provider::mock::MockInvokeLibrary;
use crate::sidecar::{ImagePage, InvokeImage, SidecarStatus};

/// One fake Invoke (one GPU) behind the library calls.
#[derive(Default)]
struct FakeInvoke {
    lib: Mutex<MockInvokeLibrary>,
    /// Fail every call (a tunnel that's down).
    down: Mutex<bool>,
}

impl FakeInvoke {
    fn check(&self) -> Result<(), SidecarError> {
        if *self.down.lock().unwrap() {
            return Err(SidecarError::Unreachable("down".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl SidecarApi for FakeInvoke {
    async fn heartbeat(&self, _: &Url, _: &str) -> Result<SidecarStatus, SidecarError> {
        unimplemented!()
    }
    async fn ticket(&self, _: &Url, _: &str) -> Result<String, SidecarError> {
        unimplemented!()
    }
    async fn list_images(
        &self,
        _: &Url,
        _: &str,
        _: u64,
        _: u64,
    ) -> Result<ImagePage, SidecarError> {
        unimplemented!()
    }
    async fn image_info(&self, _: &Url, _: &str, _: &str) -> Result<InvokeImage, SidecarError> {
        unimplemented!()
    }
    async fn image_file(&self, _: &Url, _: &str, _: &str) -> Result<Vec<u8>, SidecarError> {
        unimplemented!()
    }
    async fn queue_item_ids(&self, _: &Url, _: &str) -> Result<Vec<u64>, SidecarError> {
        unimplemented!()
    }
    async fn queue_item(&self, _: &Url, _: &str, _: u64) -> Result<Value, SidecarError> {
        unimplemented!()
    }
    async fn api_json(
        &self,
        _: &Url,
        _: &str,
        path: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Value, SidecarError> {
        self.check()?;
        self.lib.lock().unwrap().get(path, query)
    }
    async fn api_bytes(
        &self,
        _: &Url,
        _: &str,
        path: &[&str],
        _: usize,
    ) -> Result<Vec<u8>, SidecarError> {
        self.check()?;
        self.lib.lock().unwrap().bytes(path)
    }
    async fn api_post_json(
        &self,
        _: &Url,
        _: &str,
        path: &[&str],
        body: &Value,
    ) -> Result<Value, SidecarError> {
        self.check()?;
        self.lib.lock().unwrap().post_json(path, body)
    }
    async fn api_post_form(
        &self,
        _: &Url,
        _: &str,
        path: &[&str],
        fields: &[(&str, String)],
        file: Option<(&str, Vec<u8>)>,
    ) -> Result<Value, SidecarError> {
        self.check()?;
        self.lib.lock().unwrap().post_form(path, fields, file)
    }
}

const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F'];

fn builtin(name: &str) -> SavedTemplate {
    SavedTemplate {
        name: name.into(),
        positive: format!("{name} style, {{prompt}}"),
        negative: "blurry".into(),
        image: Some(base64::engine::general_purpose::STANDARD.encode(JPEG)),
    }
}

fn sync(
    invoke: &Arc<FakeInvoke>,
    store: &LibraryStore,
    builtins: Vec<SavedTemplate>,
) -> LibrarySync {
    LibrarySync::new(
        invoke.clone(),
        Url::parse("https://a-b.trycloudflare.com/").unwrap(),
        "secret".into(),
        store.clone(),
        builtins,
    )
}

fn names(invoke: &FakeInvoke) -> Vec<String> {
    let lib = invoke.lib.lock().unwrap();
    let mut v: Vec<String> = lib
        .presets
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn round_trip_across_two_gpus() {
    let dir = tempfile::tempdir().unwrap();
    let store = LibraryStore::new(dir.path());
    let builtins = vec![builtin("M · Ink"), builtin("M · Photo")];

    // GPU 1: built-ins go in; the user makes a template and a workflow.
    let gpu1 = Arc::new(FakeInvoke::default());
    let mut s = sync(&gpu1, &store, builtins.clone());
    let r = s.restore().await.unwrap();
    assert_eq!((r.templates, r.workflows, r.failed), (2, 0, 0));
    assert_eq!(names(&gpu1), ["M · Ink", "M · Photo"]);
    assert!(
        gpu1.lib.lock().unwrap().preset_images.len() == 2,
        "built-in images uploaded"
    );
    // Nothing of the user's yet: nothing saved.
    assert_eq!(s.capture().await.unwrap(), Captured::default());
    assert_eq!(store.load(), Library::default());

    let wf = {
        let mut l = gpu1.lib.lock().unwrap();
        l.add_preset("Mine", "cinematic, {prompt}", "ugly");
        l.add_workflow("Upscale x2", json!([{"id": "n1", "type": "upscale"}]))
    };
    let got = s.capture().await.unwrap();
    assert_eq!((got.templates, got.workflows), (1, 1));
    let lib = store.load();
    assert_eq!(lib.templates.len(), 1);
    assert_eq!(lib.templates[0].positive, "cinematic, {prompt}");
    assert_eq!(lib.workflows[0].name, "Upscale x2");
    assert!(lib.workflows[0].workflow.get("id").is_none(), "id stripped");

    // An edit to the workflow is picked up; an unchanged one isn't re-read.
    gpu1.lib
        .lock()
        .unwrap()
        .touch_workflow(&wf, json!([{"id": "n1", "type": "upscale", "scale": 4}]));
    assert_eq!(s.capture().await.unwrap().workflows, 1);
    assert_eq!(s.capture().await.unwrap(), Captured::default());
    assert_eq!(
        store.load().workflows[0].workflow["nodes"][0]["scale"],
        json!(4)
    );

    // GPU 2 (fresh Invoke): everything comes back, once.
    let gpu2 = Arc::new(FakeInvoke::default());
    let mut s2 = sync(&gpu2, &store, builtins.clone());
    let r = s2.restore().await.unwrap();
    assert_eq!((r.templates, r.workflows, r.failed), (3, 1, 0));
    assert_eq!(names(&gpu2), ["M · Ink", "M · Photo", "Mine"]);
    let restored = gpu2.lib.lock().unwrap().workflows[0]["workflow"].clone();
    assert_eq!(restored["nodes"][0]["scale"], json!(4));
    // Restoring again (app restart, reattach) adds nothing.
    let r = sync(&gpu2, &store, builtins).restore().await.unwrap();
    assert_eq!((r.templates, r.workflows), (0, 0));
    assert_eq!(names(&gpu2).len(), 3);
    // What we put back isn't "new" to save.
    assert_eq!(s2.capture().await.unwrap(), Captured::default());
}

#[tokio::test]
async fn deletes_renames_and_edited_builtins() {
    let dir = tempfile::tempdir().unwrap();
    let store = LibraryStore::new(dir.path());
    let builtins = vec![builtin("M · Ink"), builtin("M · Photo")];
    let gpu = Arc::new(FakeInvoke::default());
    let mut s = sync(&gpu, &store, builtins.clone());
    s.restore().await.unwrap();
    {
        let mut l = gpu.lib.lock().unwrap();
        l.add_preset("Old name", "{prompt}, x", "");
        l.add_preset("Doomed", "{prompt}, y", "");
        l.add_workflow("Doomed flow", json!([]));
    }
    s.capture().await.unwrap();
    assert_eq!(store.load().templates.len(), 2);

    {
        let mut l = gpu.lib.lock().unwrap();
        // Rename, delete, delete a built-in, edit the other built-in.
        for p in l.presets.iter_mut() {
            if p["name"] == "Old name" {
                p["name"] = "New name".into();
            }
            if p["name"] == "M · Photo" {
                p["preset_data"]["positive_prompt"] = "my photo, {prompt}".into();
            }
        }
        l.presets
            .retain(|p| p["name"] != "Doomed" && p["name"] != "M · Ink");
        l.workflows.clear();
    }
    let got = s.capture().await.unwrap();
    assert_eq!(got.removed, 4, "{got:?}"); // old name, doomed, ink, doomed flow
    let lib = store.load();
    let mut saved: Vec<&str> = lib.templates.iter().map(|t| t.name.as_str()).collect();
    saved.sort();
    assert_eq!(saved, ["M · Photo", "New name"]);
    assert!(lib.workflows.is_empty());
    assert_eq!(lib.hidden_builtins.iter().collect::<Vec<_>>(), ["M · Ink"]);

    // Next GPU: the deleted built-in stays gone, the edited one is the user's.
    let gpu2 = Arc::new(FakeInvoke::default());
    sync(&gpu2, &store, builtins).restore().await.unwrap();
    assert_eq!(names(&gpu2), ["M · Photo", "New name"]);
    let photo = gpu2
        .lib
        .lock()
        .unwrap()
        .presets
        .iter()
        .find(|p| p["name"] == "M · Photo")
        .unwrap()["preset_data"]["positive_prompt"]
        .clone();
    assert_eq!(photo, json!("my photo, {prompt}"));
}

#[tokio::test]
async fn nothing_is_lost_when_invoke_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let store = LibraryStore::new(dir.path());
    let mut lib = Library::default();
    lib.templates.push(SavedTemplate {
        name: "Keep me".into(),
        positive: "{prompt}".into(),
        negative: String::new(),
        image: None,
    });
    store.save(&lib).unwrap();

    let gpu = Arc::new(FakeInvoke::default());
    *gpu.down.lock().unwrap() = true;
    let mut s = sync(&gpu, &store, vec![]);
    assert!(s.restore().await.is_err());
    assert!(s.capture().await.is_err());
    // Up again but restore never ran: an empty Invoke is not "deleted".
    *gpu.down.lock().unwrap() = false;
    assert_eq!(s.capture().await.unwrap(), Captured::default());
    assert_eq!(store.load(), lib);
}

#[test]
fn builtin_names_carry_the_model() {
    let m = crate::catalog::bundled()
        .into_iter()
        .find(|m| m.id == "anima-aesthetic")
        .unwrap();
    let b = builtins_of(&m);
    assert!(!b.is_empty());
    assert!(b.iter().all(|t| t.name.starts_with("Anima Aesthetic · ")));
    assert!(b.iter().all(|t| t.positive.contains("{prompt}")));
    assert!(b.iter().all(|t| t.image.is_some()));
}

#[test]
fn only_user_presets_count() {
    let v = json!({"id": "d", "name": "x", "type": "default",
        "preset_data": {"positive_prompt": "{prompt}", "negative_prompt": ""}});
    assert!(parse_preset(&v).is_none());
    let mut u = v.clone();
    u["type"] = "user".into();
    assert_eq!(parse_preset(&u).unwrap().1.name, "x");
}
