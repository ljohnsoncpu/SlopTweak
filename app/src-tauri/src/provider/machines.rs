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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub ok_unix: Option<u64>,
    pub failed_unix: Option<u64>,
    /// The GPU model last seen on this machine, to make the list readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    /// Why the latest failure was blamed on the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Longest failure reason kept per machine.
const MAX_REASON_CHARS: usize = 240;

/// One machine as the Settings list shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MachineRow {
    pub id: u64,
    pub gpu: Option<String>,
    /// avoided | good | expired (failed long ago, no longer skipped)
    pub status: String,
    pub ok_unix: Option<u64>,
    pub failed_unix: Option<u64>,
    /// While `avoided`: when the machine is offered again.
    pub avoid_until_unix: Option<u64>,
    pub reason: Option<String>,
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

    #[cfg(test)]
    pub fn record(&mut self, machine: u64, ok: bool, now: u64) {
        self.record_with(machine, ok, now, None, None);
    }

    /// [`record`](Self::record) plus what the list should say about it.
    pub fn record_with(
        &mut self,
        machine: u64,
        ok: bool,
        now: u64,
        gpu: Option<&str>,
        reason: Option<&str>,
    ) {
        let e = self.machines.entry(machine).or_default();
        if ok {
            e.ok_unix = Some(now);
            e.reason = None;
        } else {
            e.failed_unix = Some(now);
            e.reason = reason
                .filter(|r| !r.is_empty())
                .map(|r| r.chars().take(MAX_REASON_CHARS).collect());
        }
        if let Some(g) = gpu.filter(|g| !g.is_empty()) {
            e.gpu = Some(g.to_string());
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

impl Memory {
    /// Every remembered machine, skipped ones first, then newest activity.
    pub fn rows(&self, now: u64) -> Vec<MachineRow> {
        let mut rows: Vec<MachineRow> = self
            .machines
            .iter()
            .map(|(id, e)| {
                let avoided = self.avoid(*id, now);
                let status = if avoided {
                    "avoided"
                } else if self.known_good(*id) {
                    "good"
                } else {
                    "expired"
                };
                MachineRow {
                    id: *id,
                    gpu: e.gpu.clone(),
                    status: status.into(),
                    ok_unix: e.ok_unix,
                    failed_unix: e.failed_unix,
                    avoid_until_unix: e.failed_unix.filter(|_| avoided).map(|f| f + AVOID_FOR_S),
                    reason: e.reason.clone().filter(|_| e.failed_unix.is_some()),
                }
            })
            .collect();
        rows.sort_by_key(|r| {
            (
                r.status != "avoided",
                std::cmp::Reverse(r.ok_unix.max(r.failed_unix)),
                r.id,
            )
        });
        rows
    }

    /// Drop everything known about a machine. `false` if it wasn't listed.
    pub fn forget(&mut self, machine: u64) -> bool {
        self.machines.remove(&machine).is_some()
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

    #[cfg(test)]
    pub fn record(&self, machine: u64, ok: bool, now: u64) -> std::io::Result<()> {
        self.record_with(machine, ok, now, None, None)
    }

    pub fn record_with(
        &self,
        machine: u64,
        ok: bool,
        now: u64,
        gpu: Option<&str>,
        reason: Option<&str>,
    ) -> std::io::Result<()> {
        let mut m = self.load();
        m.record_with(machine, ok, now, gpu, reason);
        self.save(&m)
    }

    /// Stop skipping (or preferring) a machine. `Ok(false)` if not listed.
    pub fn forget(&self, machine: u64) -> std::io::Result<bool> {
        let mut m = self.load();
        let found = m.forget(machine);
        if found {
            self.save(&m)?;
        }
        Ok(found)
    }

    fn save(&self, m: &Memory) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(m)?)?;
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
    fn rows_list_skipped_machines_first_and_forget_clears_them() {
        let mut m = Memory::default();
        m.record_with(1, true, 1000, Some("RTX 3090"), None);
        m.record_with(2, false, 500, Some("RTX 4090"), Some("setup stalled"));
        m.record_with(3, false, 10, None, None);
        let now = 600;
        let rows = m.rows(now);
        // Machine 3 failed at t=10, so it is skipped at t=600 too.
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 3, 1]);
        let bad = &rows[0];
        assert_eq!(bad.status, "avoided");
        assert_eq!(bad.avoid_until_unix, Some(500 + AVOID_FOR_S));
        assert_eq!(bad.reason.as_deref(), Some("setup stalled"));
        assert_eq!(bad.gpu.as_deref(), Some("RTX 4090"));
        // Once expired it is listed, but no longer skipped.
        let later = m.rows(10 + AVOID_FOR_S + 1);
        assert_eq!(later.iter().find(|r| r.id == 3).unwrap().status, "expired");
        // A later success clears the reason.
        m.record_with(2, true, 700, None, None);
        assert_eq!(
            m.rows(800).iter().find(|r| r.id == 2).unwrap().status,
            "good"
        );
        assert!(m.forget(2) && !m.forget(2));
        assert!(!m.avoid(2, 800) && !m.known_good(2));
    }

    #[test]
    fn reasons_are_bounded_and_old_files_still_load() {
        let mut m = Memory::default();
        m.record_with(9, false, 1, None, Some(&"x".repeat(1000)));
        assert_eq!(
            m.rows(2)[0].reason.as_ref().unwrap().len(),
            MAX_REASON_CHARS
        );
        let old: Memory =
            serde_json::from_str(r#"{"machines":{"5":{"ok_unix":3,"failed_unix":null}}}"#).unwrap();
        assert!(old.known_good(5));
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
