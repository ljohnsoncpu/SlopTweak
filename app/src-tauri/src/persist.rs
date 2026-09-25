//! The active instance, persisted so a relaunch after a crash can find it and
//! destroy or reattach. No secrets here: the launch secret is in the
//! credential store under `secrets::launch_secret_name(id)`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveRecord {
    pub instance_id: u64,
    pub offer_id: u64,
    pub gpu_name: String,
    pub hourly: f64,
    pub model_id: String,
    pub created_unix: u64,
    /// "vast" or "mock"; a mock record is meaningless to the Vast provider.
    pub provider: String,
}

pub struct RecordFile {
    path: PathBuf,
}

impl RecordFile {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("active_instance.json"),
        }
    }

    pub fn load(&self) -> Option<ActiveRecord> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Atomic: write a temp file then rename over the old one.
    pub fn save(&self, rec: &ActiveRecord) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(rec)?)?;
        std::fs::rename(&tmp, &self.path)
    }

    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let f = RecordFile::new(dir.path());
        assert_eq!(f.load(), None);
        let rec = ActiveRecord {
            instance_id: 7,
            offer_id: 8,
            gpu_name: "RTX A4000".into(),
            hourly: 0.1,
            model_id: "m".into(),
            created_unix: 1,
            provider: "vast".into(),
        };
        f.save(&rec).unwrap();
        assert_eq!(f.load(), Some(rec.clone()));
        let mut rec2 = rec;
        rec2.instance_id = 9;
        f.save(&rec2).unwrap();
        assert_eq!(f.load().unwrap().instance_id, 9);
        f.clear().unwrap();
        f.clear().unwrap();
        assert_eq!(f.load(), None);
    }

    #[test]
    fn corrupt_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("active_instance.json"), "{not json").unwrap();
        assert_eq!(RecordFile::new(dir.path()).load(), None);
    }
}
