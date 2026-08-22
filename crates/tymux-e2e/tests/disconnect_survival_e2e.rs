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
/// process itself is exiting, not being killed by daemon code.
///
/// A follow-up investigation (2026-07-17, same day) went further and
/// ruled out every code-level explanation:
///
/// - The CLI (`tymux`) process exits with **status 0, no signal** when
///   its own pty hangs up — it takes the ordinary "stdin closed" shutdown
///   path (`stdin_rx.recv() == None` in `attach()`'s select loop), the
///   exact same path a clean detach's stream-end takes. It is not killed
///   by SIGHUP.
/// - `grep`-confirmed: the only `Pane::kill()` call sites anywhere in
///   `tymux-core`/`tymuxd` are `Engine::kill_session` and
///   `Engine::close_pane` — both explicit-RPC-only. Nothing in the
///   attach-stream-teardown path (`unregister_viewport` +
///   `recompute_window_geometry`, which just re-applies `Pane::resize`)
///   touches `pane.kill()` or the pane's own master pty.
/// - Not fd/device aliasing: `/proc/<pid>/fdinfo/<fd>`'s `tty-index`
///   field was checked for both the harness's client pty and `tymuxd`'s
///   pane pty at the moment of the hangup — confirmed different devices
///   every time.
/// - Not a fixed timer: reproduces identically whether the hangup happens
///   300ms or 3s after attach.
/// - Not related to input content: reproduces with zero bytes ever sent
///   to the pane.
/// - The pane's own reader thread observes the real `Ok(0)` EOF within
///   roughly 1-3ms of the client pty being closed — fast enough to be a
///   real causal chain (client hangup → clean stream end → *something*),
///   but an `strace -f` attached to `tymuxd` around that exact window
///   produced ambiguous, contradictory-looking output (what appeared to
///   be an unrelated interactive shell's own job-control syscalls),
///   most likely a `ptrace`/PID-reuse artifact of this specific sandboxed
///   dev container (`ptrace_scope=1` blocks attaching to an
///   already-running process at all — attaching at exec time was the only
///   option, and every process here, including `tymuxd` and the outer
///   login shell, share one systemd cgroup scope with no controlling
///   terminal of their own, an environment shape a real user's machine
///   would not have). That line of investigation was abandoned rather
///   than trusted further.
///
/// Net: the pane's own OS-level pty gets a genuine hangup, but nothing in
/// this codebase's Rust/gRPC layer causes it — the mechanism is either
/// below that layer (kernel/session-level) or an artifact specific to
/// this sandboxed dev container. **Re-test on a real terminal/machine
/// outside this sandbox before trusting any further root-causing done
/// here** — a good next step there: `strace -f -o log tymuxd` from a
/// real shell (no `ptrace_scope` restriction to work around), or `ltrace`/
/// `perf trace` around the exact hangup window.
///
/// **2026-08-21 re-investigation (Story 1.1.1)** — still not real hardware;
/// this session runs inside the same kind of sandboxed dev container as
/// the 2026-07-17 pass above. Re-ran the repro capturing
/// `ps -o pid,ppid,pgid,sid,tty` for `tymuxd` and the pane's child process
/// at the disconnect instant. Findings:
///
/// - `tymuxd` itself: `TT=?`, `SID=PGID=PID` — already its own session
///   leader with **no controlling terminal at all**, before any code
///   change. Confirmed this is a property of the sandbox, not of
///   `tymuxd`: an ordinary interactive shell spawned fresh in this same
///   session shows the identical `SID=PGID=PID`, `TT=?` signature.
/// - The pane's child shell: `TT` goes from a real pty device (`pts/N`)
///   while attached to `?` and `<defunct>` (exited, awaiting reap) within
///   ~200ms of the abrupt disconnect — it genuinely exits, exactly as the
///   2026-07-17 investigation found.
/// - **The Story 1.1.2 `setsid()` fix (ADR-002) was compiled into
///   `tymuxd` for this re-test and did not stop the pane from dying** —
///   `pane_survives_abrupt_disconnect` still fails in this sandbox with
///   the fix applied. Because this sandbox's `tymuxd` already had no
///   controlling terminal *before* the fix — the exact `SID=PGID=PID`,
///   `TT=?` state `setsid()` itself would produce — the fix is a
///   structural no-op here: this environment can never exercise the
///   "`tymuxd` has a controlling terminal a hangup propagates through"
///   precondition the fix targets, with or without the code change. That
///   makes this sandbox's result **non-diagnostic** for the hypothesis —
///   it neither confirms nor refutes the real-hardware mechanism ADR-002
///   targets, it just can't see it either way.
/// - `ptrace_scope=1` is still in force here too, so the same `strace`
///   limitation noted in the 2026-07-17 pass applies; no further syscall
///   forensics were attempted this pass for the same reason it was
///   abandoned then.
///
/// **Conclusion**: the `setsid()` fix (Story 1.1.2 / ADR-002) is
/// implemented and kept — it's safe, low-risk, and matches real tmux's own
/// daemon design — but it remains **unverified against the actual bug**.
/// No real, non-sandboxed hardware and no systemd-managed host were
/// available in this session, so Story 1.1.2's pre-mortem P1 #1
/// second-environment requirement is **not met**, and this test is
/// deliberately left `#[ignore]`d rather than un-ignored on sandbox
/// evidence that can't actually distinguish "fixed" from "environment
/// can't reproduce the precondition." A human with access to real
/// hardware (and ideally a systemd-managed host, to exercise the
/// `EPERM`-already-session-leader tolerance path from Task 1.1.2b for
/// real) needs to re-run this exact repro — capturing the same `ps`
/// columns — before this test can be un-ignored and Epic 1.1 considered
/// fully done per Story 1.1.3.
#[tokio::test]
#[ignore = "known bug: abrupt client disconnect currently kills the pane — setsid() fix (ADR-002) implemented but unverified on real hardware, see doc comment"]
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
