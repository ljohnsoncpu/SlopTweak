//! ComfyUI protocol helpers for the Identity Edit backend (instance/provision.sh
//! runs ComfyUI on the instance's loopback; the sidecar proxies it). Pure
//! parsing and naming here; the HTTP calls are `SidecarApi::comfy_*`.

use serde_json::Value;

use crate::sync::valid_image_name;

/// One finished output image in ComfyUI's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComfyImage {
    pub prompt_id: String,
    pub filename: String,
    /// `""` for the output folder itself.
    pub subfolder: String,
    /// When the prompt started running (from the history's
    /// `execution_start` message), if the history said.
    pub started_unix: Option<u64>,
}

impl ComfyImage {
    /// Unique per image across prompts; what the sync ledger stores.
    pub fn key(&self) -> String {
        format!("{}/{}", self.prompt_id, self.filename)
    }

    /// Query for `GET /view`.
    pub fn view_query(&self) -> [(&str, &str); 3] {
        [
            ("filename", &self.filename),
            ("subfolder", &self.subfolder),
            ("type", "output"),
        ]
    }
}

fn valid_prompt_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A relative folder under ComfyUI's output directory: no `..`, no absolute
/// paths, no odd characters.
pub fn valid_subfolder(s: &str) -> bool {
    s.is_empty()
        || (s.len() <= 64
            && !s.starts_with('/')
            && s.split('/').all(|seg| {
                !seg.is_empty()
                    && seg != ".."
                    && seg != "."
                    && seg
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            }))
}

/// Every image a *successful* prompt saved to the output folder (`type ==
/// "output"`; previews are `temp`, uploads are `input`), from `GET /history`.
/// Order: oldest first by start time, then by key.
pub fn parse_history(history: &Value) -> Vec<ComfyImage> {
    let Some(entries) = history.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (prompt_id, entry) in entries {
        if !valid_prompt_id(prompt_id) {
            continue;
        }
        let status = &entry["status"];
        if status["status_str"].as_str() != Some("success") {
            continue;
        }
        let started_unix = status["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m[0].as_str() == Some("execution_start"))
            .and_then(|m| m[1]["timestamp"].as_u64())
            .map(|ms| ms / 1000);
        let Some(nodes) = entry["outputs"].as_object() else {
            continue;
        };
        for node in nodes.values() {
            for img in node["images"].as_array().into_iter().flatten() {
                if img["type"].as_str() != Some("output") {
                    continue;
                }
                let (Some(filename), subfolder) = (
                    img["filename"].as_str(),
                    img["subfolder"].as_str().unwrap_or(""),
                ) else {
                    continue;
                };
                if !valid_image_name(filename) || !valid_subfolder(subfolder) {
                    continue;
                }
                out.push(ComfyImage {
                    prompt_id: prompt_id.clone(),
                    filename: filename.to_string(),
                    subfolder: subfolder.to_string(),
                    started_unix,
                });
            }
        }
    }
    out.sort_by_key(|i| (i.started_unix, i.key()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(status: &str, ms: u64, images: Value) -> Value {
        json!({
            "prompt": [],
            "outputs": {"9": {"images": images}},
            "status": {
                "status_str": status, "completed": status == "success",
                "messages": [["execution_start", {"prompt_id": "x", "timestamp": ms}]]
            }
        })
    }

    #[test]
    fn only_output_images_of_successful_prompts() {
        let h = json!({
            "b-2": entry("success", 2_000, json!([
                {"filename": "ComfyUI_00002_.png", "subfolder": "", "type": "output"},
                {"filename": "preview.png", "subfolder": "", "type": "temp"},
                {"filename": "ref.png", "subfolder": "", "type": "input"},
            ])),
            "a-1": entry("success", 1_000, json!([
                {"filename": "ComfyUI_00001_.png", "subfolder": "id", "type": "output"},
            ])),
            "c-3": entry("error", 3_000, json!([
                {"filename": "ComfyUI_00003_.png", "subfolder": "", "type": "output"},
            ])),
        });
        let got = parse_history(&h);
        assert_eq!(
            got.iter().map(ComfyImage::key).collect::<Vec<_>>(),
            ["a-1/ComfyUI_00001_.png", "b-2/ComfyUI_00002_.png"]
        );
        assert_eq!(got[0].started_unix, Some(1));
        assert_eq!(got[0].subfolder, "id");
        assert_eq!(
            got[1].view_query(),
            [
                ("filename", "ComfyUI_00002_.png"),
                ("subfolder", ""),
                ("type", "output")
            ]
        );
    }

    #[test]
    fn hostile_names_never_get_through() {
        let h = json!({
            "p": entry("success", 0, json!([
                {"filename": "../../evil.png", "subfolder": "", "type": "output"},
                {"filename": "a.png", "subfolder": "../x", "type": "output"},
                {"filename": "a.png", "subfolder": "/abs", "type": "output"},
                {"filename": "con.exe", "subfolder": "", "type": "output"},
                {"filename": "ok.png", "subfolder": "a/b", "type": "output"},
            ])),
            "../bad": entry("success", 0, json!([
                {"filename": "z.png", "subfolder": "", "type": "output"},
            ])),
        });
        let got = parse_history(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key(), "p/ok.png");
        assert!(parse_history(&json!([1, 2])).is_empty());
        assert!(parse_history(&json!({})).is_empty());
    }

    #[test]
    fn subfolder_rules() {
        for ok in ["", "a", "a/b-c_d.e"] {
            assert!(valid_subfolder(ok), "{ok}");
        }
        for bad in [
            "/a",
            "a//b",
            "a/../b",
            ".",
            "a/./b",
            "a b",
            "a\\b",
            &"x".repeat(65),
        ] {
            assert!(!valid_subfolder(bad), "{bad}");
        }
    }
}
