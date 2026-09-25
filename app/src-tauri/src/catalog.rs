//! Model catalog. Phase 2 uses the copy bundled at build time; Phase 3 adds
//! fetching from GitHub with this as the fallback.

use serde::{Deserialize, Serialize};

const BUNDLED: &str = include_str!("../../../catalog/catalog.json");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelFile {
    pub url: String,
    pub filename: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub kind: String,
    #[serde(default)]
    pub requires_civitai_token: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub description: String,
    pub base: String,
    pub files: Vec<ModelFile>,
    pub min_vram_gb: f64,
    #[serde(default)]
    pub nsfw: bool,
    #[serde(default)]
    pub license_note: String,
}

impl Model {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size_bytes).sum()
    }

    pub fn needs_civitai(&self) -> bool {
        self.files.iter().any(|f| f.requires_civitai_token)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Catalog {
    schema_version: u32,
    models: Vec<Model>,
}

pub fn bundled() -> Vec<Model> {
    parse(BUNDLED).expect("bundled catalog.json is valid")
}

pub fn parse(text: &str) -> Result<Vec<Model>, String> {
    let c: Catalog = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if c.schema_version != 1 {
        return Err(format!("unsupported catalog schema {}", c.schema_version));
    }
    Ok(c.models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_parses() {
        let models = bundled();
        let m = models.iter().find(|m| m.id == "banana-splitz-xxl").unwrap();
        assert!(m.needs_civitai());
        assert_eq!(m.total_bytes(), 6_938_043_264);
        assert_eq!(m.files[0].sha256.len(), 64);
    }
}
