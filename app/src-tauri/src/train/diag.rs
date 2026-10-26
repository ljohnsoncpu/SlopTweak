//! What a training run's setup looked like: when each stage began and how
//! long it took, what Vast and the container said, and the report kept when a
//! run ends badly (the GPU, and its log, are gone by then).

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::run::{Live, Timed};
use crate::redact::redact;

/// Pushed once the run is over, so the last stage has an end.
pub const END_MARK: &str = "Run ended";
const REPORT_NAME: &str = "diagnostics.txt";

/// One stage for the UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageView {
    pub name: String,
    /// Seconds from the start of the run to the start of this stage.
    pub started_secs: u64,
    pub secs: u64,
    /// The run is still in this stage.
    pub current: bool,
}

/// Stages with durations. `now` ends the stage in progress; a run that has
/// ended carries an [`END_MARK`] instead.
pub fn stage_views(stages: &[Timed], run_start: u64, now: u64) -> Vec<StageView> {
    let mut out = Vec::new();
    for (i, s) in stages.iter().enumerate() {
        if s.text == END_MARK {
            continue;
        }
        let next = stages.get(i + 1);
        let end = next.map_or(now, |n| n.at_unix);
        out.push(StageView {
            name: s.text.clone(),
            started_secs: s.at_unix.saturating_sub(run_start),
            secs: end.saturating_sub(s.at_unix),
            current: next.is_none(),
        });
    }
    out
}

/// `+3m07s` style offset from the start of the run.
pub fn offset(secs: u64) -> String {
    format!("+{}m{:02}s", secs / 60, secs % 60)
}

fn clock_time(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map_or_else(
            || unix.to_string(),
            |t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        )
}

pub struct Report<'a> {
    pub label: &'a str,
    pub instance_id: Option<u64>,
    pub machine_id: Option<u64>,
    pub gpu: &'a str,
    /// How the run ended, in words.
    pub result: &'a str,
    pub run_start: u64,
    pub ended: u64,
    /// What was done about the machine (marked bad, not marked, ...).
    pub host_note: &'a str,
    pub live: &'a Live,
}

/// The report text, with every secret masked.
pub fn render(r: &Report, known: &[&str]) -> String {
    let mut t = String::new();
    t.push_str("SlopTweak training run diagnostics\n");
    t.push_str("==================================\n");
    t.push_str(&format!("Run:      {}\n", r.label));
    t.push_str(&format!(
        "Instance: {}\n",
        r.instance_id.map_or("none".into(), |i| i.to_string())
    ));
    t.push_str(&format!(
        "Machine:  {}\n",
        r.machine_id.map_or("unknown".into(), |m| m.to_string())
    ));
    t.push_str(&format!("GPU:      {}\n", r.gpu));
    t.push_str(&format!("Started:  {}\n", clock_time(r.run_start)));
    t.push_str(&format!("Ended:    {}\n", clock_time(r.ended)));
    t.push_str(&format!("Result:   {}\n", r.result));
    if !r.host_note.is_empty() {
        t.push_str(&format!("Machine:  {}\n", r.host_note));
    }

    t.push_str("\nStages\n------\n");
    let views = stage_views(&r.live.stages, r.run_start, r.ended);
    if views.is_empty() {
        t.push_str("(none)\n");
    }
    for v in &views {
        t.push_str(&format!(
            "{}  {}  ({}m{:02}s)\n",
            offset(v.started_secs),
            v.name,
            v.secs / 60,
            v.secs % 60
        ));
    }

    if let Some(n) = &r.live.setup {
        t.push_str(
            "\nLast report from the instance's setup\n-------------------------------------\n",
        );
        t.push_str(&format!("stage: {}\n", n.stage));
        if let Some(d) = &n.detail {
            t.push_str(&format!("step:  {d}\n"));
        }
        if !n.log_tail.trim().is_empty() {
            t.push_str(&format!("{}\n", n.log_tail.trim_end()));
        }
    }

    t.push_str("\nWhat Vast reported (status_msg, when it changed)\n------------------------------------------------\n");
    if r.live.host_status.is_empty() {
        t.push_str("(nothing)\n");
    }
    for h in &r.live.host_status {
        t.push_str(&format!(
            "{}  {}\n",
            offset(h.at_unix.saturating_sub(r.run_start)),
            h.text
        ));
    }

    t.push_str("\nContainer output (last fetched");
    match r.live.container_log_unix {
        Some(u) => t.push_str(&format!(" {}", offset(u.saturating_sub(r.run_start)))),
        None => t.push_str(": never"),
    }
    t.push_str(")\n---------------------------------\n");
    if let Some(e) = &r.live.container_log_error {
        t.push_str(&format!("(last fetch failed: {e})\n"));
    }
    if r.live.container_log.trim().is_empty() {
        t.push_str("(empty)\n");
    } else {
        t.push_str(r.live.container_log.trim_end());
        t.push('\n');
    }
    redact(&t, known)
}

/// Save the report next to the run's results. Returns where.
pub fn write(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(REPORT_NAME);
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::train::run::SetupNote;

    fn at(t: u64, s: &str) -> Timed {
        Timed {
            at_unix: t,
            text: s.into(),
        }
    }

    #[test]
    fn stage_durations_run_to_the_next_stage_or_now() {
        let stages = vec![
            at(100, "Renting"),
            at(110, "Pulling"),
            at(400, "Installing"),
        ];
        let v = stage_views(&stages, 100, 460);
        assert_eq!(
            v.iter()
                .map(|s| (s.started_secs, s.secs, s.current))
                .collect::<Vec<_>>(),
            vec![(0, 10, false), (10, 290, false), (300, 60, true)]
        );
        // After the run ends the last stage stops at the end mark.
        let mut done = stages.clone();
        done.push(at(430, END_MARK));
        let v = stage_views(&done, 100, 9999);
        assert_eq!(v.len(), 3);
        assert_eq!(v[2].secs, 30);
        assert!(!v[2].current);
        assert_eq!(offset(187), "+3m07s");
    }

    #[test]
    fn the_report_names_the_machine_and_masks_secrets() {
        let live = Live {
            stages: vec![at(1000, "Renting the GPU"), at(1010, "Installing")],
            host_status: vec![at(1012, "abc: Pull complete")],
            container_log: "starting\nTOKEN=hunter2hunter2 seen\n".into(),
            container_log_unix: Some(1100),
            container_log_error: Some("boom".into()),
            setup: Some(SetupNote {
                stage: "bootstrap".into(),
                detail: Some("uv pip sync".into()),
                log_bytes: Some(10),
                log_tail: "Downloading torch".into(),
            }),
            ..Live::default()
        };
        let text = render(
            &Report {
                label: "sloptweak-1",
                instance_id: Some(42),
                machine_id: Some(777),
                gpu: "RTX 3090",
                result: "setup stalled",
                run_start: 1000,
                ended: 1500,
                host_note: "marked bad",
                live: &live,
            },
            &["hunter2hunter2"],
        );
        assert!(text.contains("Machine:  777"));
        assert!(text.contains("+0m10s  Installing"));
        assert!(text.contains("+0m12s  abc: Pull complete"));
        assert!(text.contains("Downloading torch"));
        assert!(text.contains("last fetch failed: boom"));
        assert!(!text.contains("hunter2hunter2"));
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir.path().join("sub"), &text).unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), text);
    }
}
