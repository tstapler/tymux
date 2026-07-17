use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::{CapturePaneRequest, CreateSessionRequest};

/// tmux's whole value proposition is that a session survives the client
/// going away — this is the baseline that must hold for *any* kind of
/// disconnect, not just a clean `C-b d`.
#[tokio::test]
async fn pane_survives_graceful_detach() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    let session = client
        .create_session(CreateSessionRequest {
            name: "survive-graceful".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap()
        .into_inner();
    let pane_id = match session.windows[0]
        .layout
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap()
    {
        tymux_proto::v1::layout::Node::Pane(p) => p.id.clone(),
        _ => panic!("expected leaf"),
    };

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "survive-graceful:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(h.detach(Duration::from_secs(3)));

    let snap = client
        .capture_pane(CapturePaneRequest {
            pane_id,
            scrollback_offset: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(snap.liveness, tymux_proto::v1::Liveness::Live as i32);
}

/// **KNOWN BUG, not yet fixed** — found while building this E2E harness
/// (2026-07-17). An *abrupt* client disconnect (the client process's
/// controlling terminal hanging up — e.g. the terminal emulator crashing,
/// an SSH connection dropping, a laptop losing power — as opposed to a
/// clean `C-b d` or the process being sent SIGTERM) currently kills the
/// pane's own child process, not just the attach stream. Confirmed via
/// three isolating experiments:
///
/// 1. A pure gRPC-level abrupt stream/channel drop (no pty at all)
///    does NOT reproduce this — the pane survives.
/// 2. Sending SIGTERM directly to the CLI process's PID (matching how
///    `ptydrive.py` ended sessions throughout this project's manual
///    verification) does NOT reproduce this — the pane survives.
/// 3. Sending SIGHUP directly to the CLI process's PID via
///    `portable_pty::Child::kill()`, *without* closing this harness's
///    pty master, also does NOT reproduce this — the pane survives.
///
/// Only closing the pty master while the CLI process is still alive (a
/// genuine OS-level tty hangup) reproduces it, 100% of the time. Neither
/// `tymuxd`'s `input_handle` nor `forward_handle` (crates/tymuxd/src/
/// main.rs's `attach` handler) explicitly kill the pane on stream error —
/// per `crates/tymux-core/src/pane.rs`'s reader thread, the pane's own
/// pty read genuinely returns `Ok(0)` (clean EOF), meaning the shell
/// process itself is exiting, not being killed by daemon code. The exact
/// mechanism was not root-caused before this test was written — it needs
/// further investigation (a good next step: trace `tymuxd`'s handling of
/// an `h2 protocol error: ... BrokenPipe` mid-`input_handle` more closely,
/// or check for a session/process-group interaction specific to how the
/// pane's own pty is allocated relative to the daemon's).
///
/// This is `#[ignore]`d (not deleted) so it stays a live, runnable
/// regression check for whenever this gets fixed — un-ignore it once
/// `pane_survives_abrupt_disconnect` below passes.
#[tokio::test]
#[ignore = "known bug: abrupt client disconnect currently kills the pane — see doc comment"]
async fn pane_survives_abrupt_disconnect() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    let session = client
        .create_session(CreateSessionRequest {
            name: "survive-abrupt".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap()
        .into_inner();
    let pane_id = match session.windows[0]
        .layout
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap()
    {
        tymux_proto::v1::layout::Node::Pane(p) => p.id.clone(),
        _ => panic!("expected leaf"),
    };

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "survive-abrupt:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    // No graceful detach — abruptly tear down the client's pty, simulating
    // a crashed terminal or dropped network connection.
    drop(h);
    std::thread::sleep(Duration::from_millis(500));

    let snap = client
        .capture_pane(CapturePaneRequest {
            pane_id,
            scrollback_offset: 0,
        })
        .await
        .expect("pane should still respond to CapturePane after an abrupt disconnect")
        .into_inner();
    assert_eq!(
        snap.liveness,
        tymux_proto::v1::Liveness::Live as i32,
        "an abrupt client disconnect must not kill the pane's own process"
    );
}
