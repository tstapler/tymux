//! Engine-level restart supervision against real pty children. Time is a `ManualClock` and
//! jitter is fixed, so the only waiting is on the child processes themselves (`wait_exit`),
//! never on a timer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use tymux_core::{
    ArgSpec, Engine, EngineError, FixedJitter, FsPersistenceBackend, Launch, LayoutSnapshot,
    LifecycleEventKind, LifecycleState, ManualClock, Pane, PersistenceBackend, RestartMode,
    RestartPolicy, ReviveOutcome, SignalOutcome,
};
use uuid::Uuid;

const T0: u64 = 1_000_000;
const GRACE: Duration = Duration::from_secs(2);

fn policy(mode: RestartMode) -> RestartPolicy {
    RestartPolicy {
        mode,
        max_restarts: 3,
        window_ms: 60_000,
        initial_delay_ms: 1_000,
        multiplier: 2.0,
        max_delay_ms: 8_000,
        jitter: 0.0,
        healthy_after_ms: 10_000,
    }
}

struct Rig {
    engine: Engine,
    clock: Arc<ManualClock>,
}

fn rig() -> Rig {
    rig_with(Box::new(tymux_core::NullPersistenceBackend))
}

fn rig_with(backend: Box<dyn PersistenceBackend>) -> Rig {
    let clock = Arc::new(ManualClock::new(T0));
    let engine = Engine::with_supervision(backend, clock.clone(), Arc::new(FixedJitter(0.5)));
    Rig { engine, clock }
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tymux-restart-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What a daemon restart would find on disk: the session records as they stand now, loaded
/// from a copy so that killing the sessions afterwards (which deletes the originals) cannot
/// erase them.
fn copy_records(from: &Path, to: &Path) -> Vec<tymux_core::PersistedSessionRecord> {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
    FsPersistenceBackend::new(to.to_path_buf())
        .unwrap()
        .load_all()
}

fn sh_launch(script: &str) -> Launch {
    Launch {
        args: vec!["-c".into(), script.into()],
        ..Launch::default()
    }
}

/// Exits with `code` the first time it runs and sits in `sleep` on every later run, so a test
/// can tell "restarted and running" from "restarted and already dead again".
fn fail_once_then_run(marker: &Path, code: i32) -> Launch {
    sh_launch(&format!(
        "if [ -e '{m}' ]; then echo second-run; exec sleep 300; else touch '{m}'; echo first-run; exit {code}; fi",
        m = marker.display()
    ))
}

fn long_running() -> Launch {
    sh_launch("echo up; exec sleep 300")
}

fn create(rig: &Rig, launch: Launch, policy: RestartPolicy) -> (Uuid, Uuid) {
    let session = rig
        .engine
        .create_session_supervised("t".into(), Some("/bin/sh".into()), launch, None, policy)
        .unwrap();
    (session, first_pane(&rig.engine, session))
}

fn first_pane(engine: &Engine, session: Uuid) -> Uuid {
    let snap = engine.session_snapshot(session).unwrap();
    match &snap.windows[0].layout {
        LayoutSnapshot::Leaf(info) => info.id,
        LayoutSnapshot::Split { .. } => panic!("expected a single pane"),
    }
}

fn pane(rig: &Rig, id: Uuid) -> Arc<Pane> {
    rig.engine.held_pane(id).expect("pane is held")
}

fn state(rig: &Rig, id: Uuid) -> LifecycleState {
    rig.engine.pane_status(id).unwrap().view.state
}

fn screen_text(p: &Pane) -> String {
    p.snapshot()
        .grid
        .iter()
        .map(|row| row.iter().map(|c| c.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ticks until the supervisor reports nothing due (so a respawned child that exits at once
/// is not mistaken for still running).
fn tick(rig: &Rig) -> Option<u64> {
    rig.engine.supervise_tick()
}

#[tokio::test]
async fn a_failed_pane_should_restart_after_its_backoff_keeping_identity_and_scrollback() {
    let rig = rig();
    let dir = tmp_dir("identity");
    let (session, id) = create(
        &rig,
        fail_once_then_run(&dir.join("m"), 3),
        policy(RestartMode::OnFailure),
    );
    let window = rig.engine.window_id_for_pane(id).unwrap();
    rig.engine.report_viewport_and_recompute(window, 1, 30, 100);
    let before = pane(&rig, id);
    let size = before.size();
    assert_eq!(size, (30, 100));
    before.wait_exit().await;

    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Backoff);
    assert_eq!(status.view.last_exit.unwrap().code, Some(3));
    assert_eq!(status.view.next_restart_at_ms, Some(T0 + 1_000));
    assert_eq!(
        tick(&rig),
        Some(T0 + 1_000),
        "tick reports when the timer is due"
    );
    assert_eq!(state(&rig, id), LifecycleState::Backoff, "not due yet");
    assert!(
        rig.engine.pane_lookup(id).is_dead_for_test(),
        "CapturePane/Attach still see an exited pane during backoff"
    );
    assert!(screen_text(&before).contains("first-run"));
    assert!(screen_text(&before).contains("exited with code 3; restarting in 1.0s"));

    rig.clock.advance(1_000);
    assert_eq!(tick(&rig), None);
    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Live);
    assert!(status.pid.is_some());
    assert_eq!(status.view.restart_count, 1);
    assert_eq!(status.view.restarts_in_window, 1);

    let after = pane(&rig, id);
    assert!(Arc::ptr_eq(&before, &after), "same pane object");
    assert_eq!(after.id, id);
    assert_eq!(after.size(), size, "window size survives");
    let text = screen_text(&after);
    assert!(text.contains("first-run"), "old scrollback is kept: {text}");
    assert!(text.contains("restarted (pid"), "boundary marker: {text}");
    // Wait on the output channel (bounded only as a hang guard), not on a spin count.
    let mut output = after.subscribe();
    let seen = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let text = screen_text(&after);
            if text.contains("second-run") {
                return text;
            }
            let _ = output.recv().await;
        }
    })
    .await
    .expect("the restarted process printed within 30s");
    assert!(seen.contains("second-run"), "new process output: {seen}");
    assert_eq!(rig.engine.list_sessions().len(), 1);
    rig.engine.kill_session(session).unwrap();
}

trait LookupExt {
    fn is_dead_for_test(&self) -> bool;
}
impl LookupExt for tymux_core::PaneLookup {
    fn is_dead_for_test(&self) -> bool {
        matches!(self, tymux_core::PaneLookup::Dead)
    }
}

#[tokio::test]
async fn backoff_should_grow_each_failure_and_end_in_failed_when_the_budget_runs_out() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let mut delays = Vec::new();
    for _ in 0..3 {
        pane(&rig, id).wait_exit().await;
        let status = rig.engine.pane_status(id).unwrap();
        assert_eq!(status.view.state, LifecycleState::Backoff);
        let at = status.view.next_restart_at_ms.unwrap();
        delays.push(at - rig.clock.now_for_test());
        rig.clock.set(at);
        tick(&rig);
    }
    pane(&rig, id).wait_exit().await;
    assert_eq!(delays, vec![1_000, 2_000, 4_000]);
    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Failed);
    assert_eq!(status.view.restart_count, 3);
    assert!(!pane(&rig, id).restart_pending());
    assert!(pane(&rig, id).wait_final_exit().now_or_never().is_some());

    rig.clock.advance(3_600_000);
    assert_eq!(tick(&rig), None);
    assert_eq!(
        state(&rig, id),
        LifecycleState::Failed,
        "no auto restart after FAILED"
    );
    assert_eq!(rig.engine.pane_status(id).unwrap().view.restart_count, 3);

    // Operator action brings it back: a fresh budget.
    rig.engine.start_pane(id).unwrap();
    assert_eq!(
        rig.engine.pane_status(id).unwrap().view.restarts_in_window,
        0
    );
    rig.engine.kill_session(session).unwrap();
}

trait ClockExt {
    fn now_for_test(&self) -> u64;
}
impl ClockExt for ManualClock {
    fn now_for_test(&self) -> u64 {
        tymux_core::Clock::now_ms(self)
    }
}

#[tokio::test]
async fn a_healthy_run_should_reset_the_backoff_to_the_initial_delay() {
    let rig = rig();
    let dir = tmp_dir("healthy");
    let (session, id) = create(
        &rig,
        fail_once_then_run(&dir.join("m"), 1),
        policy(RestartMode::Always),
    );
    let p = pane(&rig, id);
    p.wait_exit().await;
    rig.clock.advance(1_000);
    tick(&rig);
    assert_eq!(state(&rig, id), LifecycleState::Live);

    rig.clock.advance(11_000);
    p.kill().unwrap();
    p.wait_exit().await;
    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Backoff);
    assert_eq!(
        status.view.next_restart_at_ms.unwrap() - rig.clock.now_for_test(),
        1_000,
        "a run past healthy_after starts the backoff over"
    );
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn stop_should_park_the_pane_and_suppress_restarts_even_under_always() {
    let rig = rig();
    let (session, id) = create(&rig, long_running(), policy(RestartMode::Always));
    let p = pane(&rig, id);
    let mut events = p.subscribe_lifecycle();
    let first_pid = rig.engine.pane_status(id).unwrap().pid;

    assert_eq!(
        rig.engine.stop_pane(id, GRACE).unwrap(),
        SignalOutcome::Signaled
    );
    p.wait_exit().await;
    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Stopped);
    assert_eq!(status.view.last_exit.unwrap().signal, Some(15));
    assert_eq!(status.pid, None);

    rig.clock.advance(3_600_000);
    assert_eq!(tick(&rig), None);
    assert_eq!(state(&rig, id), LifecycleState::Stopped);
    assert!(p.wait_final_exit().now_or_never().is_some());

    let mut kinds = Vec::new();
    while let Ok(e) = events.try_recv() {
        kinds.push(e.kind);
    }
    assert_eq!(
        kinds,
        vec![LifecycleEventKind::Exited, LifecycleEventKind::Stopped]
    );

    rig.engine.start_pane(id).unwrap();
    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Live);
    assert_ne!(status.pid, first_pid);
    assert!(Arc::ptr_eq(&p, &pane(&rig, id)));
    assert_eq!(
        rig.engine.start_pane(id),
        Err(EngineError::PaneRunning(id)),
        "cannot start a running pane"
    );
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn stop_during_backoff_should_cancel_the_pending_restart() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let p = pane(&rig, id);
    p.wait_exit().await;
    assert_eq!(state(&rig, id), LifecycleState::Backoff);
    assert!(p.restart_pending());

    assert_eq!(
        rig.engine.stop_pane(id, GRACE).unwrap(),
        SignalOutcome::Immediate
    );
    assert!(!p.restart_pending());
    rig.clock.advance(3_600_000);
    tick(&rig);
    assert_eq!(state(&rig, id), LifecycleState::Stopped);
    assert!(p.is_exited());
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn manual_restart_should_replace_a_running_process_without_charging_the_budget() {
    let rig = rig();
    let (session, id) = create(&rig, long_running(), policy(RestartMode::Never));
    let p = pane(&rig, id);
    let first_pid = rig.engine.pane_status(id).unwrap().pid.unwrap();

    assert_eq!(
        rig.engine.restart_pane(id, false, GRACE).unwrap(),
        SignalOutcome::Signaled
    );
    assert_eq!(state(&rig, id), LifecycleState::Restarting);
    p.wait_exit().await;
    assert!(
        p.restart_pending(),
        "attached clients must not see a final exit"
    );
    assert!(p.wait_final_exit().now_or_never().is_none());
    tick(&rig);

    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Live);
    assert_ne!(status.pid.unwrap(), first_pid);
    assert_eq!(status.view.restart_count, 1);
    assert_eq!(status.view.restarts_in_window, 0);
    assert!(screen_text(&p).contains("restarted (pid"));
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn restart_should_respawn_a_dead_pane_immediately_whatever_its_state() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 0"), policy(RestartMode::Never));
    let p = pane(&rig, id);
    p.wait_exit().await;
    assert_eq!(state(&rig, id), LifecycleState::Exited);

    assert_eq!(
        rig.engine.restart_pane(id, true, GRACE).unwrap(),
        SignalOutcome::Immediate
    );
    // The child exits at once again; its fate is Exited under Never.
    p.wait_exit().await;
    assert_eq!(rig.engine.pane_status(id).unwrap().view.restart_count, 1);
    assert_eq!(state(&rig, id), LifecycleState::Exited);
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn revive_session_should_respawn_a_pane_that_exited_in_this_daemon() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 0"), RestartPolicy::default());
    let p = pane(&rig, id);
    p.wait_exit().await;

    let outcome = rig.engine.revive_session(session).unwrap();
    assert_eq!(outcome, ReviveOutcome::Revived { pane_count: 1 });
    assert_eq!(rig.engine.pane_status(id).unwrap().view.restart_count, 1);
    assert!(Arc::ptr_eq(&p, &pane(&rig, id)), "respawned in place");
    p.wait_exit().await;
    rig.engine.kill_session(session).unwrap();

    let rig2 = self::rig();
    let (session, _) = create(&rig2, long_running(), RestartPolicy::default());
    assert_eq!(
        rig2.engine.revive_session(session).unwrap(),
        ReviveOutcome::AlreadyLive,
        "a session with a running pane is still never double-spawned"
    );
    rig2.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn the_default_policy_should_leave_an_exit_exactly_as_it_always_was() {
    let rig = rig();
    let (session, id) = create(
        &rig,
        sh_launch("echo bye; exit 4"),
        RestartPolicy::default(),
    );
    let p = pane(&rig, id);
    let mut events = p.subscribe_lifecycle();
    p.wait_exit().await;

    assert!(!p.restart_pending());
    assert!(p.wait_final_exit().now_or_never().is_some());
    assert_eq!(p.exit_code(), Some(4));
    let text = screen_text(&p);
    assert!(text.contains("bye"));
    assert!(
        !text.contains("tymux:"),
        "no marker for an unsupervised exit: {text}"
    );
    rig.clock.advance(3_600_000);
    assert_eq!(tick(&rig), None);
    assert_eq!(state(&rig, id), LifecycleState::Exited);
    assert_eq!(events.try_recv().unwrap().kind, LifecycleEventKind::Exited);
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn lifecycle_events_should_report_exit_schedule_and_restart_in_order() {
    let rig = rig();
    let dir = tmp_dir("events");
    let (session, id) = create(
        &rig,
        fail_once_then_run(&dir.join("m"), 9),
        policy(RestartMode::OnFailure),
    );
    let p = pane(&rig, id);
    let mut events = p.subscribe_lifecycle();
    p.wait_exit().await;
    rig.clock.advance(1_000);
    tick(&rig);

    let exited = events.try_recv().unwrap();
    assert_eq!(exited.kind, LifecycleEventKind::Exited);
    assert_eq!(exited.exit_code, Some(9));
    let scheduled = events.try_recv().unwrap();
    assert_eq!(scheduled.kind, LifecycleEventKind::RestartScheduled);
    assert_eq!(scheduled.delay_ms, Some(1_000));
    assert_eq!(scheduled.next_restart_at_ms, Some(T0 + 1_000));
    assert_eq!(scheduled.state, LifecycleState::Backoff);
    let restarted = events.try_recv().unwrap();
    assert_eq!(restarted.kind, LifecycleEventKind::Restarted);
    assert!(restarted.pid.is_some());
    assert_eq!(restarted.restart_count, 1);
    assert!(events.try_recv().is_err());
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn closing_or_killing_during_backoff_should_cancel_the_restart() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let p = pane(&rig, id);
    p.wait_exit().await;
    assert!(p.restart_pending());
    rig.engine.kill_session(session).unwrap();
    assert!(!p.restart_pending(), "closing clears the pending restart");
    assert!(p.wait_final_exit().now_or_never().is_some());
    rig.clock.advance(3_600_000);
    assert_eq!(tick(&rig), None);
    assert!(p.respawn().is_err(), "a closed pane refuses to respawn");
    assert!(rig.engine.pane_status(id).is_none());

    let (session, first) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let split = rig
        .engine
        .split_pane(
            first,
            tymux_core::Orientation::Horizontal,
            Some("/bin/sh".into()),
        )
        .unwrap();
    assert_eq!(split.windows.len(), 1);
    pane(&rig, first).wait_exit().await;
    rig.engine.close_pane(first).unwrap();
    rig.clock.advance(3_600_000);
    tick(&rig);
    assert!(rig.engine.pane_status(first).is_none());
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn changing_the_policy_to_never_should_cancel_a_pending_backoff() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let p = pane(&rig, id);
    p.wait_exit().await;
    assert_eq!(state(&rig, id), LifecycleState::Backoff);

    let status = rig
        .engine
        .set_restart_policy(id, policy(RestartMode::Never))
        .unwrap();
    assert_eq!(status.view.state, LifecycleState::Exited);
    assert!(!p.restart_pending());
    rig.clock.advance(3_600_000);
    tick(&rig);
    assert_eq!(state(&rig, id), LifecycleState::Exited);

    let mut bad = policy(RestartMode::Always);
    bad.multiplier = 0.1;
    assert!(matches!(
        rig.engine.set_restart_policy(id, bad),
        Err(EngineError::InvalidRestartPolicy(_))
    ));
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn panes_added_later_should_inherit_the_session_default_policy() {
    let rig = rig();
    let (session, first) = create(&rig, long_running(), policy(RestartMode::OnFailure));
    let split = rig
        .engine
        .split_pane(
            first,
            tymux_core::Orientation::Horizontal,
            Some("/bin/sh".into()),
        )
        .unwrap();
    let ids = leaf_ids(&split.windows[0].layout);
    let new_pane = *ids.iter().find(|i| **i != first).unwrap();
    assert_eq!(
        rig.engine.pane_status(new_pane).unwrap().view.policy.mode,
        RestartMode::OnFailure
    );

    rig.engine
        .set_session_restart_policy(session, policy(RestartMode::Always), true)
        .unwrap();
    for id in [first, new_pane] {
        assert_eq!(
            rig.engine.pane_status(id).unwrap().view.policy.mode,
            RestartMode::Always
        );
    }
    let window = rig
        .engine
        .create_window(session, Some("/bin/sh".into()))
        .unwrap();
    let third = leaf_ids(&window.windows[1].layout)[0];
    assert_eq!(
        rig.engine.pane_status(third).unwrap().view.policy.mode,
        RestartMode::Always
    );
    rig.engine.kill_session(session).unwrap();
}

fn leaf_ids(layout: &LayoutSnapshot) -> Vec<Uuid> {
    match layout {
        LayoutSnapshot::Leaf(i) => vec![i.id],
        LayoutSnapshot::Split { children, .. } => {
            children.iter().flat_map(|(c, _)| leaf_ids(c)).collect()
        }
    }
}

#[tokio::test]
async fn a_restart_that_cannot_spawn_should_walk_the_backoff_into_failed() {
    let rig = rig();
    let dir = tmp_dir("spawnfail");
    let arg_file = dir.join("arg");
    std::fs::write(&arg_file, "x").unwrap();
    let launch = Launch {
        args: vec!["-c".into(), "exit 1".into()],
        arg_specs: vec![ArgSpec::FileContents(arg_file.display().to_string())],
        ..Launch::default()
    };
    let (session, id) = create(&rig, launch, policy(RestartMode::Always));
    let p = pane(&rig, id);
    p.wait_exit().await;
    std::fs::remove_file(&arg_file).unwrap();

    for _ in 0..8 {
        match rig.engine.pane_status(id).unwrap().view.next_restart_at_ms {
            Some(at) => rig.clock.set(at),
            None => break,
        }
        tick(&rig);
    }
    let status = rig.engine.pane_status(id).unwrap();
    assert_eq!(status.view.state, LifecycleState::Failed);
    assert_eq!(status.view.restart_count, 3);
    assert!(p.is_exited());
    assert!(screen_text(&p).contains("restart failed"));
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn policy_and_counters_should_survive_a_daemon_restart_and_backoff_comes_back_exited() {
    let dir = tmp_dir("persist");
    let backend = || -> Box<dyn PersistenceBackend> {
        Box::new(FsPersistenceBackend::new(dir.join("sessions")).unwrap())
    };
    let rig1 = rig_with(backend());
    let (session, id) = create(&rig1, sh_launch("exit 5"), policy(RestartMode::Always));
    let stopped_session = rig1
        .engine
        .create_session_supervised(
            "stopped".into(),
            Some("/bin/sh".into()),
            long_running(),
            None,
            policy(RestartMode::Always),
        )
        .unwrap();
    let stopped = first_pane(&rig1.engine, stopped_session);
    pane(&rig1, id).wait_exit().await;
    rig1.engine.stop_pane(stopped, GRACE).unwrap();
    pane(&rig1, stopped).wait_exit().await;
    rig1.engine.supervise_tick();
    assert_eq!(state(&rig1, id), LifecycleState::Backoff);
    let before = rig1.engine.pane_status(id).unwrap().view;
    let records = copy_records(&dir.join("sessions"), &dir.join("daemon-restart"));
    rig1.engine.kill_session(session).unwrap();
    rig1.engine.kill_session(stopped_session).unwrap();
    drop(rig1);

    let rig2 = rig();
    rig2.engine.load_persisted(records);
    let after = rig2.engine.pane_status(id).unwrap().view;
    assert_eq!(
        after.state,
        LifecycleState::Exited,
        "a pending backoff is dropped, not resumed"
    );
    assert_eq!(after.next_restart_at_ms, None);
    assert_eq!(after.policy, before.policy);
    assert_eq!(after.restart_count, before.restart_count);
    assert_eq!(after.consecutive_failures, before.consecutive_failures);
    assert_eq!(after.last_exit.unwrap().code, Some(5));
    assert_eq!(state(&rig2, stopped), LifecycleState::Stopped);
    assert_eq!(rig2.engine.supervise_tick(), None);
    assert!(rig2.engine.pane_lookup(id).is_dead_for_test());

    // StartPane brings a single restored pane back, policy intact.
    rig2.engine.start_pane(stopped).unwrap();
    let revived = rig2.engine.pane_status(stopped).unwrap();
    assert_eq!(revived.view.state, LifecycleState::Live);
    assert_eq!(revived.view.policy.mode, RestartMode::Always);
    rig2.engine.kill_session(stopped_session).unwrap();
}

#[tokio::test]
async fn session_default_policy_should_survive_a_daemon_restart() {
    let dir = tmp_dir("persist-default");
    let backend = || -> Box<dyn PersistenceBackend> {
        Box::new(FsPersistenceBackend::new(dir.join("sessions")).unwrap())
    };
    let rig1 = rig_with(backend());
    let (session, _) = create(&rig1, long_running(), policy(RestartMode::OnFailure));
    let records = copy_records(&dir.join("sessions"), &dir.join("daemon-restart"));
    rig1.engine.kill_session(session).unwrap();
    drop(rig1);

    let rig2 = rig();
    rig2.engine.load_persisted(records);
    let pane_id = first_pane(&rig2.engine, session);
    assert_eq!(
        rig2.engine.pane_status(pane_id).unwrap().view.policy.mode,
        RestartMode::OnFailure
    );
    // A pane revived from the record gets the restored policy too.
    rig2.engine.revive_session(session).unwrap();
    assert_eq!(state(&rig2, pane_id), LifecycleState::Live);
    assert_eq!(
        rig2.engine.pane_status(pane_id).unwrap().view.policy.mode,
        RestartMode::OnFailure
    );
    rig2.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn reset_budget_should_make_failed_exited_and_clear_the_counters() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    for _ in 0..4 {
        pane(&rig, id).wait_exit().await;
        if let Some(at) = rig.engine.pane_status(id).unwrap().view.next_restart_at_ms {
            rig.clock.set(at);
            tick(&rig);
        }
    }
    assert_eq!(state(&rig, id), LifecycleState::Failed);
    rig.engine.reset_restart_budget(id).unwrap();
    let status = rig.engine.pane_status(id).unwrap().view;
    assert_eq!(status.state, LifecycleState::Exited);
    assert_eq!(status.restarts_in_window, 0);
    assert_eq!(status.consecutive_failures, 0);
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn wait_final_exit_should_stay_pending_through_backoff_and_resolve_when_it_ends() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let p = pane(&rig, id);
    p.wait_exit().await;
    assert!(
        p.wait_final_exit().now_or_never().is_none(),
        "a client attached to a pane in backoff stays attached"
    );
    let waiter = tokio::spawn({
        let p = p.clone();
        async move { p.wait_final_exit().await }
    });
    rig.engine.stop_pane(id, GRACE).unwrap();
    waiter.await.unwrap();
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn cancelling_a_pending_restart_by_policy_should_say_so_and_end_with_a_final_event() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    let p = pane(&rig, id);
    let mut events = p.subscribe_lifecycle();
    p.wait_exit().await;
    assert_eq!(state(&rig, id), LifecycleState::Backoff);

    rig.engine
        .set_restart_policy(id, policy(RestartMode::Never))
        .unwrap();
    let mut kinds = Vec::new();
    while let Ok(e) = events.try_recv() {
        kinds.push((e.kind, e.state));
    }
    assert_eq!(
        kinds.last(),
        Some(&(LifecycleEventKind::Exited, LifecycleState::Exited)),
        "the last event must not still say a restart is scheduled: {kinds:?}"
    );
    let text = screen_text(&p);
    assert!(text.contains("restarting in 1.0s"), "{text}");
    assert!(text.contains("restart cancelled"), "{text}");
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn stopping_a_pane_that_is_already_dead_should_still_leave_a_stopped_marker() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 0"), RestartPolicy::default());
    let p = pane(&rig, id);
    p.wait_exit().await;
    assert_eq!(
        rig.engine.stop_pane(id, GRACE).unwrap(),
        SignalOutcome::Immediate
    );
    assert!(
        screen_text(&p).contains("[tymux: stopped]"),
        "{}",
        screen_text(&p)
    );
    rig.engine.kill_session(session).unwrap();
}

#[tokio::test]
async fn revive_should_start_only_panes_the_operator_did_not_stop() {
    let rig = rig();
    let (session, a) = create(&rig, long_running(), policy(RestartMode::Never));
    let snap = rig
        .engine
        .split_pane(
            a,
            tymux_core::Orientation::Horizontal,
            Some("/bin/sh".into()),
        )
        .unwrap();
    let b = {
        let mut ids = Vec::new();
        collect_leaves(&snap.windows[0].layout, &mut ids);
        *ids.iter().find(|id| **id != a).unwrap()
    };
    rig.engine.stop_pane(a, GRACE).unwrap();
    pane(&rig, a).wait_exit().await;
    pane(&rig, b).write_input(b"exit 1\n").unwrap();
    pane(&rig, b).wait_exit().await;
    assert_eq!(state(&rig, a), LifecycleState::Stopped);
    assert_eq!(state(&rig, b), LifecycleState::Exited);

    assert_eq!(
        rig.engine.revive_session(session).unwrap(),
        ReviveOutcome::Revived { pane_count: 1 }
    );
    assert_eq!(state(&rig, a), LifecycleState::Stopped, "A stays stopped");
    assert_eq!(state(&rig, b), LifecycleState::Live, "B is revived");

    rig.engine.kill_session(session).unwrap();

    // Nothing but a stopped pane: an error that names StartPane, and StartPane works.
    let (session, only) = create(&rig, long_running(), policy(RestartMode::Never));
    rig.engine.stop_pane(only, GRACE).unwrap();
    pane(&rig, only).wait_exit().await;
    assert_eq!(
        rig.engine.revive_session(session),
        Err(EngineError::PaneStopped(only))
    );
    rig.engine.start_pane(only).unwrap();
    assert_eq!(state(&rig, only), LifecycleState::Live);
    rig.engine.kill_session(session).unwrap();
}

fn collect_leaves(layout: &LayoutSnapshot, out: &mut Vec<Uuid>) {
    match layout {
        LayoutSnapshot::Leaf(info) => out.push(info.id),
        LayoutSnapshot::Split { children, .. } => {
            for (child, _) in children {
                collect_leaves(child, out);
            }
        }
    }
}

#[tokio::test]
async fn revive_should_leave_a_failed_pane_failed() {
    let rig = rig();
    let (session, id) = create(&rig, sh_launch("exit 1"), policy(RestartMode::Always));
    for _ in 0..4 {
        pane(&rig, id).wait_exit().await;
        if let Some(at) = rig.engine.pane_status(id).unwrap().view.next_restart_at_ms {
            rig.clock.set(at);
            tick(&rig);
        }
    }
    assert_eq!(state(&rig, id), LifecycleState::Failed);
    assert_eq!(
        rig.engine.revive_session(session),
        Err(EngineError::PaneFailed(id))
    );
    assert_eq!(state(&rig, id), LifecycleState::Failed);
    rig.engine.restart_pane(id, true, GRACE).unwrap();
    assert_ne!(state(&rig, id), LifecycleState::Failed);
    rig.engine.kill_session(session).unwrap();
}

/// Records what is "on disk" and can hold one `save` open, to force the interleaving where a
/// snapshot is taken, the session is killed, and only then is the snapshot written.
struct GatedBackend {
    disk: std::sync::Mutex<std::collections::HashMap<Uuid, tymux_core::PersistedSessionRecord>>,
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    deleted: std::sync::atomic::AtomicBool,
}

struct SharedBackend(Arc<GatedBackend>);

impl PersistenceBackend for SharedBackend {
    fn save(&self, record: &tymux_core::PersistedSessionRecord) -> anyhow::Result<()> {
        let b = &self.0;
        if b.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            b.entered.send(()).unwrap();
            b.release.lock().unwrap().recv().unwrap();
        }
        b.disk
            .lock()
            .unwrap()
            .insert(record.session_id, record.clone());
        Ok(())
    }
    fn load_all(&self) -> Vec<tymux_core::PersistedSessionRecord> {
        Vec::new()
    }
    fn delete(&self, session_id: Uuid) {
        self.0.disk.lock().unwrap().remove(&session_id);
        self.0
            .deleted
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn a_session_killed_while_its_snapshot_is_being_saved_should_not_reappear_on_disk() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let backend = Arc::new(GatedBackend {
        disk: Default::default(),
        armed: Default::default(),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
        deleted: Default::default(),
    });
    let clock = Arc::new(ManualClock::new(T0));
    let engine = Arc::new(Engine::with_supervision(
        Box::new(SharedBackend(backend.clone())),
        clock,
        Arc::new(FixedJitter(0.5)),
    ));
    let session = engine
        .create_session_supervised(
            "t".into(),
            Some("/bin/sh".into()),
            long_running(),
            None,
            policy(RestartMode::Never),
        )
        .unwrap();
    let pane_id = first_pane(&engine, session);
    // A persist is in flight (the revive's final save), held open inside `save`.
    backend
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let saver = {
        let engine = engine.clone();
        std::thread::spawn(move || engine.reset_restart_budget(pane_id).unwrap())
    };
    entered_rx.recv().unwrap();
    let killer = {
        let engine = engine.clone();
        std::thread::spawn(move || engine.kill_session(session).unwrap())
    };
    // Give the kill the chance to delete while the save is still in flight: without
    // serialization it does so at once, with it the kill waits for the save to finish.
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while !backend.deleted.load(std::sync::atomic::Ordering::SeqCst)
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    release_tx.send(()).unwrap();
    saver.join().unwrap();
    killer.join().unwrap();
    assert!(
        !backend.disk.lock().unwrap().contains_key(&session),
        "a killed session must not be resurrected by a save that was already in flight"
    );
}
