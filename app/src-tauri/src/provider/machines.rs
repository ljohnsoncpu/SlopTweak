//! What happened on each physical machine in earlier sessions, kept on disk
//! (user decision 2026-09-25). Startup time is dominated by whether the host
//! already has the Invoke image: live, a host that failed a cold start was
//! `running` 35 s after a later rental because the image stayed cached.
//!
//! * A machine that failed recently (and hasn't worked since) is skipped.
//! * A machine that got to Ready before is preferred when it costs only a
//!   little more than the cheapest offer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Skip a failed machine for this long.
pub const AVOID_FOR_S: u64 = 7 * 24 * 3600;
/// A known-good machine wins if its expected session cost is at most this
/// much above the cheapest (dollars). It likely starts in under a minute.
pub const KNOWN_GOOD_BONUS: f64 = 0.05;
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub ok_unix: Option<u64>,
    pub failed_unix: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    machines: HashMap<u64, Entry>,
}

impl Memory {
    /// Failed within [`AVOID_FOR_S`] and not Ready since.
    pub fn avoid(&self, machine: u64, now: u64) -> bool {
        self.machines.get(&machine).is_some_and(|e| {
            e.failed_unix.is_some_and(|f| {
                now.saturating_sub(f) < AVOID_FOR_S && e.ok_unix.is_none_or(|ok| ok < f)
            })
        })
    }

    /// Reached Ready, and hasn't failed since.
    pub fn known_good(&self, machine: u64) -> bool {
        self.machines.get(&machine).is_some_and(|e| {
            e.ok_unix
                .is_some_and(|ok| e.failed_unix.is_none_or(|f| ok >= f))
        })
    }

    pub fn record(&mut self, machine: u64, ok: bool, now: u64) {
        let e = self.machines.entry(machine).or_default();
        if ok {
            e.ok_unix = Some(now);
        } else {
            e.failed_unix = Some(now);
        }
        if self.machines.len() > MAX_ENTRIES {
            // Drop the stalest entries.
            let mut by_age: Vec<(u64, u64)> = self
                .machines
                .iter()
                .map(|(id, e)| (e.ok_unix.max(e.failed_unix).unwrap_or(0), *id))
                .collect();
            by_age.sort_unstable();
            for (_, id) in by_age.iter().take(self.machines.len() - MAX_ENTRIES) {
                self.machines.remove(id);
            }
        }
    }
}

/// `machines.json` in the app data dir. Losing it only costs speed.
pub struct MachineFile {
    path: PathBuf,
}

impl MachineFile {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("machines.json"),
        }
    }

    pub fn load(&self) -> Memory {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn record(&self, machine: u64, ok: bool, now: u64) -> std::io::Result<()> {
        let mut m = self.load();
        m.record(machine, ok, now);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&m)?)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_expire_and_success_clears_them() {
        let mut m = Memory::default();
        m.record(1, false, 1000);
        assert!(m.avoid(1, 1000 + 60));
        assert!(!m.known_good(1));
        assert!(!m.avoid(1, 1000 + AVOID_FOR_S));
        m.record(1, true, 2000);
        assert!(!m.avoid(1, 2000));
        assert!(m.known_good(1));
        // A later failure outweighs the earlier success.
        m.record(1, false, 3000);
        assert!(m.avoid(1, 3000));
        assert!(!m.known_good(1));
        assert!(!m.avoid(2, 0) && !m.known_good(2));
    }

    #[test]
    fn file_roundtrip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let f = MachineFile::new(dir.path());
        assert_eq!(f.load(), Memory::default());
        f.record(7, true, 5).unwrap();
        f.record(8, false, 6).unwrap();
        let m = f.load();
        assert!(m.known_good(7));
        assert!(m.avoid(8, 7));
        std::fs::write(dir.path().join("machines.json"), "nope").unwrap();
        assert_eq!(f.load(), Memory::default());
    }

    #[test]
    fn size_is_bounded() {
        let mut m = Memory::default();
        for i in 0..(MAX_ENTRIES as u64 + 20) {
            m.record(i, true, i);
        }
        assert_eq!(m.machines.len(), MAX_ENTRIES);
        assert!(m.known_good(MAX_ENTRIES as u64 + 19));
        assert!(!m.known_good(0));
    }
}
