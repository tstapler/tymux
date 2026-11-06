//! ADR 0008: panes survive an in-place `exec` of the daemon image.
//!
//! The outer test runs this test binary as a child. The child (stage `old`) spawns panes,
//! freezes and exports them, and `exec`s itself; the new image (stage `new`) adopts them and
//! checks the result. Both stages are the same test function, selected by environment.

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use tymux_core::{Launch, Pane, PaneHandoff, ReplayOutcome};

const TEST_NAME: &str = "pane_survives_exec_with_stream_and_exit_code_intact";
const STAGE_ENV: &str = "TYMUX_HANDOFF_STAGE";
const STATE_ENV: &str = "TYMUX_HANDOFF_STATE";

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn replay_text(pane: &Pane) -> String {
    // Seq 1 is the shell's first prompt; resuming from it is the oldest window the buffer allows.
    match pane.replay_since(1) {
        ReplayOutcome::InWindow { chunks, .. } => {
            let bytes: Vec<u8> = chunks.into_iter().flat_map(|(_, c)| c).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        ReplayOutcome::GapExceeded { .. } => {
            panic!("replay buffer evicted the start of the stream")
        }
    }
}

fn ticks(text: &str) -> Vec<u64> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("tick "))
        .filter_map(|n| n.trim().parse().ok())
        .collect()
}

/// Asserts the ticks are consecutive from whatever the first retained one is.
fn assert_gapless(seen: &[u64], context: &str) {
    let first = *seen.first().expect("no ticks seen");
    let expected: Vec<u64> = (first..first + seen.len() as u64).collect();
    assert_eq!(seen, expected, "{context}");
}

fn old_image() {
    let counter = Pane::spawn("/bin/sh", 24, 80).unwrap();
    counter
        .write_input(b"i=0; while :; do echo tick $i; i=$((i+1)); sleep 0.02; done\n")
        .unwrap();
    let launch = Launch {
        args: vec!["-c".into(), "read x; exit 7".into()],
        ..Launch::default()
    };
    let waiter = Pane::spawn_launch("/bin/sh", &launch, None, None, 24, 80).unwrap();
    wait_until("ten ticks before the handoff", || {
        ticks(&replay_text(&counter)).len() >= 10
    });

    let panes = [&counter, &waiter];
    for p in panes {
        p.freeze_reads();
    }
    let exported: Vec<PaneHandoff> = panes.iter().map(|p| p.export_handoff().unwrap()).collect();
    let state = std::env::temp_dir().join(format!("tymux-handoff-{}.json", std::process::id()));
    std::fs::write(&state, serde_json::to_vec(&exported).unwrap()).unwrap();

    let err = Command::new(std::env::current_exe().unwrap())
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(STAGE_ENV, "new")
        .env(STATE_ENV, &state)
        .exec();
    panic!("exec failed: {err}");
}

fn new_image() {
    let state = std::env::var(STATE_ENV).unwrap();
    let exported: Vec<PaneHandoff> =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    std::fs::remove_file(&state).unwrap();
    let [counter_state, waiter_state]: [PaneHandoff; 2] = exported.try_into().unwrap();
    let (counter_pid, export_seq) = (counter_state.child_pid, counter_state.output_seq);

    let counter = Pane::from_adopted(counter_state).unwrap();
    let waiter = Pane::from_adopted(waiter_state).unwrap();
    assert_eq!(
        counter.pid(),
        Some(counter_pid),
        "the child must keep its pid"
    );

    wait_until("ticks after the handoff", || {
        ticks(&replay_text(&counter)).len() >= 25
    });
    let seen = ticks(&replay_text(&counter));
    assert_gapless(
        &seen,
        "ticks must be gapless and duplicate-free across exec",
    );
    assert!(
        counter.snapshot_with_seq().1 > export_seq,
        "output continued after adoption"
    );
    assert!(!counter.is_exited(), "the child never saw a hangup");

    // Parentage survived: the adopted pane reaps the child and reports its real exit code.
    waiter.write_input(b"\n").unwrap();
    wait_until("waiter exit", || waiter.is_exited());
    assert_eq!(waiter.exit_code(), Some(7));

    counter.kill().unwrap();
    println!("HANDOFF_OK ticks={}", seen.len());
}

#[test]
fn pane_survives_exec_with_stream_and_exit_code_intact() {
    match std::env::var(STAGE_ENV).as_deref() {
        Ok("old") => old_image(),
        Ok("new") => new_image(),
        _ => {
            let out = Command::new(std::env::current_exe().unwrap())
                .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
                .env(STAGE_ENV, "old")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("HANDOFF_OK"),
                "handoff child failed: {:?}\nstdout:\n{stdout}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[test]
fn frozen_reader_consumes_nothing_until_unfrozen() {
    let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
    pane.write_input(b"i=0; while :; do echo tick $i; i=$((i+1)); sleep 0.01; done\n")
        .unwrap();
    wait_until("first output", || pane.snapshot_with_seq().1 > 0);
    pane.freeze_reads();
    let frozen_at = pane.snapshot_with_seq().1;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(
            pane.snapshot_with_seq().1,
            frozen_at,
            "reader consumed a chunk while frozen"
        );
    }
    pane.unfreeze_reads();
    wait_until("output after unfreeze", || {
        pane.snapshot_with_seq().1 > frozen_at
    });
    let seen = ticks(&replay_text(&pane));
    assert_gapless(&seen, "freeze must not drop or duplicate bytes");
    pane.kill().unwrap();
}
