//! Restarting must not leak the old pty or its reader thread. This file holds a single test
//! because it counts the whole process's open file descriptors, which any concurrently running
//! test would disturb.

use std::sync::Arc;

use tymux_core::{
    Engine, FixedJitter, Launch, ManualClock, NullPersistenceBackend, RestartMode, RestartPolicy,
};

fn open_fds() -> usize {
    (0..4096)
        // SAFETY: F_GETFD on an arbitrary integer only reports whether it is an open fd.
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
        .count()
}

fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |d| d.count())
}

#[tokio::test]
async fn many_restarts_should_not_leak_descriptors_or_threads() {
    let clock = Arc::new(ManualClock::new(1_000));
    let engine = Engine::with_supervision(
        Box::new(NullPersistenceBackend),
        clock.clone(),
        Arc::new(FixedJitter(0.5)),
    );
    let policy = RestartPolicy {
        mode: RestartMode::Always,
        max_restarts: u32::MAX,
        window_ms: 0,
        initial_delay_ms: 1,
        multiplier: 1.0,
        max_delay_ms: 1,
        jitter: 0.0,
        healthy_after_ms: 0,
    };
    let launch = Launch {
        args: vec!["-c".into(), "exit 1".into()],
        ..Launch::default()
    };
    let session = engine
        .create_session_supervised("fd".into(), Some("/bin/sh".into()), launch, None, policy)
        .unwrap();
    let id = match &engine.session_snapshot(session).unwrap().windows[0].layout {
        tymux_core::LayoutSnapshot::Leaf(i) => i.id,
        _ => unreachable!(),
    };
    let pane = engine.held_pane(id).unwrap();
    pane.wait_exit().await;
    // Warm up so one-time allocations (first restart's thread, channels) are in the baseline.
    for _ in 0..3 {
        clock.advance(10);
        engine.supervise_tick();
        pane.wait_exit().await;
    }
    let (fds, threads) = (open_fds(), thread_count());

    const RESTARTS: usize = 40;
    for _ in 0..RESTARTS {
        clock.advance(10);
        engine.supervise_tick();
        pane.wait_exit().await;
    }
    assert!(
        engine.pane_status(id).unwrap().view.restart_count >= RESTARTS as u32,
        "the restarts really happened"
    );
    let (fds_after, threads_after) = (open_fds(), thread_count());
    assert!(
        fds_after <= fds + 6,
        "fd count grew from {fds} to {fds_after} over {RESTARTS} restarts"
    );
    if threads > 0 {
        assert!(
            threads_after <= threads + 6,
            "thread count grew from {threads} to {threads_after} over {RESTARTS} restarts"
        );
    }
    engine.kill_session(session).unwrap();
}
