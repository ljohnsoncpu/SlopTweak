use std::sync::{Arc, Mutex};
use std::time::Duration;

use url::Url;

use super::*;
use crate::catalog;
use crate::provider::mock::{MockBehavior, MockProvider, MockTimings};
use crate::secrets::MemoryStore;

// ----- pure transitions -------------------------------------------------------

fn offer() -> OfferSummary {
    OfferSummary {
        offer_id: 1,
        gpu_name: "RTX A4000".into(),
        hourly: 0.11,
        download_cost: 0.02,
        location: None,
    }
}

fn provisioning(attempt: u32) -> SessionState {
    SessionState::Provisioning {
        attempt,
        max_attempts: 3,
        instance_id: 42,
        offer: offer(),
        stage: None,
        detail: None,
        progress: None,
    }
}

#[test]
fn happy_path_transitions() {
    let mut s = SessionState::idle();
    for ev in [
        Event::Start { max_attempts: 3 },
        Event::OfferPicked(offer()),
        Event::InstanceCreated { instance_id: 42 },
        Event::Stage(SidecarStatus {
            stage: "downloading".into(),
            progress: Some(10.0),
            ..Default::default()
        }),
        Event::Ready { at_unix: 5 },
        Event::Heartbeat(SidecarStatus::default()),
        Event::StopRequested,
        Event::Destroyed,
    ] {
        s = next(&s, &ev).unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(s, SessionState::idle());
}

#[test]
fn stage_updates_carry_progress() {
    let s = next(
        &provisioning(1),
        &Event::Stage(SidecarStatus {
            stage: "downloading".into(),
            detail: Some("m.safetensors".into()),
            progress: Some(41.0),
            ..Default::default()
        }),
    )
    .unwrap();
    let SessionState::Provisioning {
        stage, progress, ..
    } = s
    else {
        panic!()
    };
    assert_eq!(stage.as_deref(), Some("downloading"));
    assert_eq!(progress, Some(41.0));
}

#[test]
fn attempt_failures_retry_then_fail() {
    let fail = Event::AttemptFailed {
        reason: "host stuck".into(),
        retry: true,
    };
    let s = next(&provisioning(1), &fail).unwrap();
    assert!(matches!(s, SessionState::Renting { attempt: 2, .. }));
    let s = next(&provisioning(3), &fail).unwrap();
    let SessionState::Failed { reason } = s else {
        panic!()
    };
    assert!(reason.contains("3 tries") && reason.contains("host stuck"));
    let s = next(
        &provisioning(1),
        &Event::AttemptFailed {
            reason: "bad key".into(),
            retry: false,
        },
    )
    .unwrap();
    assert_eq!(
        s,
        SessionState::Failed {
            reason: "bad key".into()
        }
    );
}

#[test]
fn stop_carries_instance_id() {
    let s = next(&provisioning(1), &Event::StopRequested).unwrap();
    assert_eq!(
        s,
        SessionState::Stopping {
            instance_id: Some(42)
        }
    );
    assert_eq!(next(&s, &Event::StopRequested).unwrap(), s);
    let renting = SessionState::Renting {
        attempt: 1,
        max_attempts: 3,
        offer: None,
    };
    assert_eq!(
        next(&renting, &Event::StopRequested).unwrap(),
        SessionState::Stopping { instance_id: None }
    );
}

#[test]
fn invalid_transitions_are_rejected() {
    let idle = SessionState::idle();
    let ready = next(&provisioning(1), &Event::Ready { at_unix: 1 }).unwrap();
    let stopping = SessionState::Stopping { instance_id: None };
    let bad = [
        (&idle, Event::InstanceCreated { instance_id: 1 }),
        (&idle, Event::Ready { at_unix: 1 }),
        (&idle, Event::StopRequested),
        (&idle, Event::Destroyed),
        (&ready, Event::Start { max_attempts: 3 }),
        (&ready, Event::InstanceCreated { instance_id: 2 }),
        (&ready, Event::Dismiss),
        (&stopping, Event::Start { max_attempts: 3 }),
        (&stopping, Event::OfferPicked(offer())),
        (&stopping, Event::InstanceCreated { instance_id: 1 }),
        (&stopping, Event::Ready { at_unix: 1 }),
        (
            &stopping,
            Event::Reattach {
                instance_id: 1,
                offer: offer(),
            },
        ),
    ];
    for (s, ev) in bad {
        assert!(next(s, &ev).is_err(), "{ev:?} should be invalid in {s:?}");
    }
    // Renting without a picked offer can't become Provisioning.
    let renting = SessionState::Renting {
        attempt: 1,
        max_attempts: 3,
        offer: None,
    };
    assert!(next(&renting, &Event::InstanceCreated { instance_id: 1 }).is_err());
}

#[test]
fn gone_and_dismiss() {
    let ready = next(&provisioning(1), &Event::Ready { at_unix: 1 }).unwrap();
    let s = next(
        &ready,
        &Event::InstanceGone {
            reason: "idle".into(),
        },
    )
    .unwrap();
    assert_eq!(
        s,
        SessionState::Idle {
            notice: Some("idle".into())
        }
    );
    assert_eq!(next(&s, &Event::Dismiss).unwrap(), SessionState::idle());
    let failed = SessionState::Failed { reason: "x".into() };
    assert_eq!(
        next(&failed, &Event::Dismiss).unwrap(),
        SessionState::idle()
    );
    assert!(matches!(
        next(&failed, &Event::Start { max_attempts: 3 }).unwrap(),
        SessionState::Renting { .. }
    ));
}

#[test]
fn orphan_transitions() {
    let s = next(
        &SessionState::idle(),
        &Event::DestroyOrphan { instance_id: 9 },
    )
    .unwrap();
    assert_eq!(
        s,
        SessionState::Stopping {
            instance_id: Some(9)
        }
    );
    let s = next(
        &SessionState::idle(),
        &Event::Reattach {
            instance_id: 9,
            offer: offer(),
        },
    )
    .unwrap();
    assert!(matches!(
        s,
        SessionState::Provisioning {
            instance_id: 9,
            max_attempts: 1,
            ..
        }
    ));
}

#[test]
fn launch_secret_is_256_bit_urlsafe() {
    let a = new_launch_secret();
    assert_eq!(a.len(), 43);
    assert!(a
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert_ne!(a, new_launch_secret());
    assert_eq!(sha256_hex("x").len(), 64);
}

// ----- driver against the mock ------------------------------------------------

#[derive(Default)]
struct Recorder {
    states: Mutex<Vec<SessionState>>,
    opened: Mutex<Vec<Url>>,
    closed: Mutex<u32>,
    lines: Mutex<Vec<String>>,
}

impl Ui for Recorder {
    fn state_changed(&self, s: &SessionState) {
        self.states.lock().unwrap().push(s.clone());
    }
    fn log(&self, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }
    fn open_remote(&self, url: Url) {
        self.opened.lock().unwrap().push(url);
    }
    fn close_remote(&self) {
        *self.closed.lock().unwrap() += 1;
    }
}

struct Harness {
    mgr: Arc<SessionManager>,
    mock: MockProvider,
    ui: Arc<Recorder>,
    secrets: Arc<MemoryStore>,
    dir: tempfile::TempDir,
}

fn harness(behaviors: Vec<MockBehavior>) -> Harness {
    let mock = MockProvider::new(MockTimings::default(), behaviors);
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemoryStore::default());
    let h = rebuild(&mock, dir.path(), secrets.clone());
    Harness {
        mgr: h.0,
        ui: h.1,
        mock,
        secrets,
        dir,
    }
}

/// A fresh manager over the same provider, disk, and credential store, like
/// relaunching the app.
fn rebuild(
    mock: &MockProvider,
    dir: &std::path::Path,
    secrets: Arc<MemoryStore>,
) -> (Arc<SessionManager>, Arc<Recorder>) {
    let ui = Arc::new(Recorder::default());
    let deps = Deps {
        provider: Arc::new(mock.clone()),
        provider_name: "mock".into(),
        sidecar: Arc::new(mock.sidecar()),
        secrets,
        records: RecordFile::new(dir),
        ui: ui.clone(),
    };
    (SessionManager::new(deps, Timing::default()), ui)
}

fn model() -> Model {
    catalog::bundled()[0].clone()
}

fn start(h: &Harness) {
    h.mgr
        .start(model(), Settings::default(), Some("civitai-test".into()))
        .unwrap();
}

async fn wait_for(mgr: &SessionManager, pred: impl Fn(&SessionState) -> bool) -> SessionState {
    for _ in 0..20_000 {
        let s = mgr.state();
        if pred(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timed out; state {:?}", mgr.state());
}

fn is_ready(s: &SessionState) -> bool {
    matches!(s, SessionState::Ready { .. })
}

#[tokio::test(start_paused = true)]
async fn start_ready_stop() {
    let h = harness(vec![]);
    start(&h);
    let s = wait_for(&h.mgr, is_ready).await;
    let SessionState::Ready {
        instance_id, offer, ..
    } = s
    else {
        panic!()
    };
    // Ranking picked the cheap-bandwidth A4500 over the cheaper-per-hour A4000.
    assert_eq!(offer.offer_id, 103);
    assert_eq!(h.mock.live_ids(), vec![instance_id]);
    let opened = h.ui.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].as_str().ends_with("/__auth?t=mock-ticket"));
    // Record + secret exist while running.
    let rec = RecordFile::new(h.dir.path()).load().unwrap();
    assert_eq!(rec.instance_id, instance_id);
    assert!(h
        .secrets
        .get(&secrets::launch_secret_name(instance_id))
        .unwrap()
        .is_some());

    // Heartbeats continue while Ready.
    let before = h.mock.heartbeats(instance_id);
    tokio::time::sleep(Duration::from_secs(185)).await;
    assert!(h.mock.heartbeats(instance_id) >= before + 3);

    assert!(h.mgr.stop_and_wait(Duration::from_secs(300)).await);
    assert_eq!(h.mgr.state(), SessionState::idle());
    assert!(h.mock.live_ids().is_empty());
    assert_eq!(RecordFile::new(h.dir.path()).load(), None);
    assert!(h
        .secrets
        .get(&secrets::launch_secret_name(instance_id))
        .unwrap()
        .is_none());
    assert!(*h.ui.closed.lock().unwrap() >= 1);
    // The launch secret never reached the log.
    let lines = h.ui.lines.lock().unwrap().join("\n");
    assert!(!lines.contains("civitai-test"));
}

#[tokio::test(start_paused = true)]
async fn daemon_error_moves_to_next_offer() {
    let h = harness(vec![MockBehavior::DaemonError]);
    start(&h);
    let s = wait_for(&h.mgr, is_ready).await;
    let created = h.mock.created_offers();
    assert_eq!(created.len(), 2);
    assert_ne!(created[0], created[1], "must not retry the same offer");
    let SessionState::Ready { instance_id, .. } = s else {
        panic!()
    };
    assert_eq!(
        h.mock.live_ids(),
        vec![instance_id],
        "failed instance destroyed"
    );
    h.mgr.stop_and_wait(Duration::from_secs(300)).await;
}

#[tokio::test(start_paused = true)]
async fn dead_host_and_stuck_loading_move_on() {
    let h = harness(vec![MockBehavior::DeadHost, MockBehavior::NeverStarts]);
    let t0 = tokio::time::Instant::now();
    start(&h);
    wait_for(&h.mgr, is_ready).await;
    assert_eq!(h.mock.created_offers().len(), 3);
    assert_eq!(h.mock.live_ids().len(), 1);
    // NeverStarts shows no progress, so it's abandoned at the stall limit,
    // well before the 12-minute loading cap.
    let waited = t0.elapsed();
    assert!(waited >= crate::provider::MAX_STALL, "{waited:?}");
    assert!(waited < crate::provider::MAX_LOADING, "{waited:?}");
    h.mgr.stop_and_wait(Duration::from_secs(300)).await;
}

#[tokio::test(start_paused = true)]
async fn slow_pull_with_progress_is_kept() {
    // An 11-minute image pull on a cheap host: past the old 8-minute cap,
    // but it keeps making progress, so we wait instead of retrying.
    let h = harness(vec![MockBehavior::SlowPull(Duration::from_secs(11 * 60))]);
    let t0 = tokio::time::Instant::now();
    start(&h);
    wait_for(&h.mgr, is_ready).await;
    assert_eq!(h.mock.created_offers().len(), 1, "no retry");
    let waited = t0.elapsed();
    assert!(waited < Duration::from_secs(15 * 60), "{waited:?}");
    h.mgr.stop_and_wait(Duration::from_secs(300)).await;
}

#[tokio::test(start_paused = true)]
async fn unavailable_offer_is_retried_elsewhere() {
    let h = harness(vec![MockBehavior::Unavailable]);
    start(&h);
    wait_for(&h.mgr, is_ready).await;
    let created = h.mock.created_offers();
    assert_eq!(created.len(), 2);
    assert_ne!(created[0], created[1]);
    h.mgr.stop_and_wait(Duration::from_secs(300)).await;
}

#[tokio::test(start_paused = true)]
async fn three_failures_give_a_plain_message_and_leave_nothing_running() {
    let h = harness(vec![
        MockBehavior::DaemonError,
        MockBehavior::ProvisionFails("download failed: m.safetensors".into()),
        MockBehavior::DeadHost,
        MockBehavior::Normal,
    ]);
    start(&h);
    let s = wait_for(&h.mgr, |s| matches!(s, SessionState::Failed { .. })).await;
    let SessionState::Failed { reason } = s else {
        panic!()
    };
    assert!(reason.contains("3 tries"), "{reason}");
    assert_eq!(h.mock.created_offers().len(), 3);
    assert!(h.mock.live_ids().is_empty());
    assert_eq!(RecordFile::new(h.dir.path()).load(), None);
    assert!(!h.mgr.is_busy());
}

#[tokio::test(start_paused = true)]
async fn stop_during_provisioning_destroys() {
    let h = harness(vec![]);
    start(&h);
    wait_for(
        &h.mgr,
        |s| matches!(s, SessionState::Provisioning { stage: Some(st), .. } if st == "downloading"),
    )
    .await;
    assert_eq!(h.mock.live_ids().len(), 1);
    assert!(h.mgr.stop_and_wait(Duration::from_secs(300)).await);
    assert!(h.mock.live_ids().is_empty());
    assert!(h.ui.opened.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stop_while_renting() {
    let h = harness(vec![]);
    start(&h);
    // Stop before the create call has returned.
    h.mgr.request_stop();
    assert!(h.mgr.stop_and_wait(Duration::from_secs(300)).await);
    assert_eq!(h.mgr.state(), SessionState::idle());
    assert!(h.mock.live_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn self_destruct_returns_to_idle_with_notice() {
    let h = harness(vec![MockBehavior::SelfDestructAfterReady(
        Duration::from_secs(90),
    )]);
    start(&h);
    wait_for(&h.mgr, is_ready).await;
    let s = wait_for(&h.mgr, |s| matches!(s, SessionState::Idle { .. })).await;
    let SessionState::Idle { notice: Some(n) } = s else {
        panic!("{s:?}")
    };
    assert!(n.contains("idle"), "{n}");
    assert!(h.mock.live_ids().is_empty());
    assert_eq!(RecordFile::new(h.dir.path()).load(), None);
}

#[tokio::test(start_paused = true)]
async fn crash_then_relaunch_finds_and_destroys_orphan() {
    let h = harness(vec![]);
    start(&h);
    let SessionState::Ready { instance_id, .. } = wait_for(&h.mgr, is_ready).await else {
        panic!()
    };
    h.mgr.crash();

    let (mgr2, _ui2) = rebuild(&h.mock, h.dir.path(), h.secrets.clone());
    let orphans = mgr2.scan_orphans().await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].instance_id, instance_id);
    assert!(orphans[0].can_reattach);

    mgr2.destroy_orphan(instance_id).unwrap();
    wait_for(&mgr2, |s| *s == SessionState::idle()).await;
    assert!(h.mock.live_ids().is_empty());
    assert_eq!(RecordFile::new(h.dir.path()).load(), None);
    assert!(mgr2.scan_orphans().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn crash_then_relaunch_can_reattach() {
    let h = harness(vec![]);
    start(&h);
    let SessionState::Ready { instance_id, .. } = wait_for(&h.mgr, is_ready).await else {
        panic!()
    };
    h.mgr.crash();

    let (mgr2, ui2) = rebuild(&h.mock, h.dir.path(), h.secrets.clone());
    mgr2.reattach(instance_id).await.unwrap();
    let s = wait_for(&mgr2, is_ready).await;
    assert_eq!(s.instance_id(), Some(instance_id));
    assert_eq!(ui2.opened.lock().unwrap().len(), 1);
    let before = h.mock.heartbeats(instance_id);
    tokio::time::sleep(Duration::from_secs(125)).await;
    assert!(h.mock.heartbeats(instance_id) > before);
    assert!(mgr2.stop_and_wait(Duration::from_secs(300)).await);
    assert!(h.mock.live_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stale_record_is_cleared_on_scan() {
    let h = harness(vec![]);
    start(&h);
    let SessionState::Ready { instance_id, .. } = wait_for(&h.mgr, is_ready).await else {
        panic!()
    };
    h.mgr.crash();
    // The watchdog destroyed it while the app was down.
    h.mock.destroy(instance_id).await.unwrap();
    let (mgr2, _) = rebuild(&h.mock, h.dir.path(), h.secrets.clone());
    assert!(mgr2.scan_orphans().await.unwrap().is_empty());
    assert_eq!(RecordFile::new(h.dir.path()).load(), None);
    assert!(h
        .secrets
        .get(&secrets::launch_secret_name(instance_id))
        .unwrap()
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn cannot_start_twice_or_without_civitai_key() {
    let h = harness(vec![]);
    let err = h.mgr.start(model(), Settings::default(), None).unwrap_err();
    assert!(err.contains("CivitAI"));
    start(&h);
    assert!(h
        .mgr
        .start(model(), Settings::default(), Some("x".into()))
        .is_err());
    h.mgr.stop_and_wait(Duration::from_secs(300)).await;
}

#[tokio::test(start_paused = true)]
async fn no_matching_offer_fails_without_renting() {
    let h = harness(vec![]);
    let settings = Settings {
        max_dph: 0.01,
        ..Settings::default()
    };
    h.mgr.start(model(), settings, Some("x".into())).unwrap();
    let s = wait_for(&h.mgr, |s| matches!(s, SessionState::Failed { .. })).await;
    let SessionState::Failed { reason } = s else {
        panic!()
    };
    assert!(reason.contains("No GPU matches"), "{reason}");
    assert!(h.mock.created_offers().is_empty());
}
