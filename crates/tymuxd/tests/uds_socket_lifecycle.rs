//! Epic 5.2 — proves the lock-file + stale-socket-reconciliation sequence
//! (`SocketLockGuard`/`reconcile_stale_socket`/`bind_uds_listener`,
//! `crates/tymuxd/src/auth.rs`) against real second `tymuxd` subprocesses,
//! not just the pure functions in isolation (already unit-tested there).
//! Follows `restart_persistence.rs`/`sigterm_flush.rs`'s established
//! real-subprocess pattern (spawn the actual `tymuxd` binary, real gRPC).
//!
//! - Story 5.2.1: a second instance racing the lock file fails fast with a
//!   distinct, actionable message while the first keeps serving.
//! - Story 5.2.2: a restart (clean SIGTERM, or unclean SIGKILL) under an
//!   open UDS `Attach` stream re-binds cleanly and the session resumes
//!   (dead-flagged, matching pre-restart `Layout` shape) exactly as
//!   `restart_persistence.rs` already proves over TCP.
//! - Story 5.2.3 (validation.md Gap 1): a real dual-listener `tymuxd`
//!   drains concurrent TCP *and* UDS `Attach` streams gracefully on
//!   SIGTERM — the automated proof Story 4.2.2's own ACs were missing.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Request;
use tymux_proto::v1::layout::Node;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, AttachEvent, AttachRequest, CreateSessionRequest,
    ListSessionsRequest, Liveness, Session,
};
use uuid::Uuid;

// ---- shared subprocess-harness plumbing (mirrors restart_persistence.rs /
// sigterm_flush.rs / daemon_startup.rs's established conventions) ----------

struct DaemonProcess {
    child: Child,
}

impl DaemonProcess {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends a genuine SIGTERM (distinct from `Child::kill()`, which is
    /// SIGKILL and bypasses `shutdown_signal()` entirely) without waiting.
    fn send_sigterm(&self) {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(self.pid().to_string())
            .status()
            .expect("failed to invoke `kill`");
        assert!(
            status.success(),
            "`kill -TERM {}` should succeed",
            self.pid()
        );
    }

    /// Polls (bounded by `timeout`, not a fixed sleep) until the process has
    /// actually exited.
    fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status;
            }
            if Instant::now() > deadline {
                panic!(
                    "tymuxd (pid {}) did not exit within {timeout:?}",
                    self.pid()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn send_sigterm_and_wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        self.send_sigterm();
        self.wait_for_exit(timeout)
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A short, fresh, per-test socket directory — deliberately NOT nested
/// under `xdg_state_home` (whose descriptive-label-plus-uuid naming can push
/// a nested socket path past `SUN_LEN`, the ~108-byte kernel limit on
/// `AF_UNIX` paths) and NOT `xdg_state_home` itself (already pre-created at
/// the default, non-0700 mode; `bind_uds_listener` refuses to bind into a
/// pre-existing directory at the wrong mode, by design). Matches every
/// other real-subprocess test file in this crate.
fn short_unique_socket_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("tymuxd-test-{}", Uuid::new_v4().simple()))
        .join("s.sock")
}

fn temp_xdg_state_home(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tymuxd-uds-lifecycle-test-{label}-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_daemon_at(addr: &str, xdg_state_home: &Path, socket_path: &Path) -> DaemonProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", xdg_state_home)
        .env("TYMUXD_SOCKET_PATH", socket_path)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn tymuxd binary");
    DaemonProcess { child }
}

/// Task 5.2.1a: runs a second `tymuxd` to completion (it must exit on its
/// own — this is exercising the lock-contention startup path, which never
/// reaches `serve_with_shutdown`) with piped output and a bounded wall-clock
/// timeout, mirroring `uds_socket_startup_failures.rs`'s `run_tymuxd`.
fn run_second_instance(
    addr: &str,
    xdg_state_home: &Path,
    socket_path: &Path,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", xdg_state_home)
        .env("TYMUXD_SOCKET_PATH", socket_path)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tymuxd binary");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("second tymuxd instance should have exited (lock contention) within 10s");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    child
        .wait_with_output()
        .expect("failed to collect tymuxd output")
}

async fn wait_for_daemon_tcp(addr: &str) -> TymuxServiceClient<Channel> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(client) = TymuxServiceClient::connect(format!("http://{addr}")).await {
            return client;
        }
        if Instant::now() > deadline {
            panic!("tymuxd did not become reachable over TCP within 10s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Dials a real `tymuxd` subprocess's UDS listener — the same
/// `tower::service_fn` + `hyper_util::rt::TokioIo` connector both
/// `tymux-cli`'s `dial_uds` and `main.rs`'s `spawn_uds_test_server` test
/// harness use (duplicated here rather than shared — this repo has no
/// test-utility crate to place it in without a larger restructure, per
/// plan.md Task 5.2.1a/5.2.2a's own note).
async fn dial_uds(
    socket_path: &Path,
) -> Result<TymuxServiceClient<Channel>, tonic::transport::Error> {
    let path = socket_path.to_path_buf();
    let connector = tower::service_fn(move |_: tonic::transport::Uri| {
        let path = path.clone();
        async move {
            let stream = tokio::net::UnixStream::connect(&path).await?;
            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
        }
    });
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await?;
    Ok(TymuxServiceClient::new(channel))
}

async fn wait_for_daemon_uds(socket_path: &Path) -> TymuxServiceClient<Channel> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(client) = dial_uds(socket_path).await {
            return client;
        }
        if Instant::now() > deadline {
            panic!("tymuxd did not become reachable over UDS within 10s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn first_pane_id(session: &Session) -> String {
    match session.windows[0]
        .layout
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap()
    {
        Node::Pane(p) => p.id.clone(),
        _ => panic!("expected a fresh session to be a single leaf"),
    }
}

/// Mirrors `restart_persistence.rs`/`sigterm_flush.rs`'s shape summary:
/// cheap structural comparison of a `Layout` tree that ignores pane ids
/// (which legitimately change across a restart).
#[derive(Debug, PartialEq)]
enum ShapeSummary {
    Leaf,
    Split(i32, Vec<ShapeSummary>),
}

fn summarize(layout: &tymux_proto::v1::Layout) -> ShapeSummary {
    match layout.node.as_ref().unwrap() {
        Node::Pane(_) => ShapeSummary::Leaf,
        Node::Split(split) => ShapeSummary::Split(
            split.orientation,
            split
                .children
                .iter()
                .map(|c| summarize(c.layout.as_ref().unwrap()))
                .collect(),
        ),
    }
}

/// Opens an `Attach` stream against `pane_id`, sends `command` as pty input
/// (skipped when empty) right after the pane-id message, and confirms the
/// stream actually established by reading its priming `PaneSnapshot` event.
/// Returns the still-open response stream — resuming across a daemon
/// restart is NOT this repo's `resume_from_seq` protocol (a restarted
/// daemon always reloads sessions dead-flagged per ADR-002, and a dead
/// pane's `attach()` fails `resolve_live_pane` outright); "resumes the same
/// session" here means the same thing `restart_persistence.rs` already
/// proves over TCP — the persisted session record survives the restart —
/// which is what Story 5.2.2's own callers assert after this stream's
/// daemon exits.
async fn open_attach_with_command(
    client: &mut TymuxServiceClient<Channel>,
    pane_id: &str,
    command: &str,
) -> tonic::Streaming<AttachEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(AttachRequest {
        payload: Some(attach_request::Payload::PaneId(pane_id.to_string())),
        resume_from_seq: None,
    })
    .await
    .expect("send pane_id");
    if !command.is_empty() {
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::Input(command.as_bytes().to_vec())),
            resume_from_seq: None,
        })
        .await
        .expect("send input");
    }
    // `tx` is intentionally dropped when this function returns — the
    // request stream just closes after these messages, which forward_handle
    // (the server-side output loop) does not depend on staying open.
    let mut stream = client
        .attach(Request::new(ReceiverStream::new(rx)))
        .await
        .expect("attach RPC should succeed")
        .into_inner();
    let first = stream
        .message()
        .await
        .expect("attach stream should not error on its first message")
        .expect("attach stream ended before a priming event arrived");
    assert!(
        matches!(first.payload, Some(attach_event::Payload::Snapshot(_))),
        "first AttachEvent should be a PaneSnapshot, got {:?}",
        first.payload
    );
    stream
}

/// Story 5.2.3: drains a still-open `Attach` stream to its natural end and
/// asserts that end is clean — an `Ok(None)` from `.message()` — never a
/// transport-level error (a hard reset would surface as `Err(status)` or a
/// stalled read). Bounded so a regression that makes the drain hang fails
/// the test instead of the whole suite.
async fn assert_attach_stream_drains_cleanly(
    stream: &mut tonic::Streaming<AttachEvent>,
    label: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(_event))) => continue,
            Ok(Ok(None)) => return,
            Ok(Err(status)) => panic!(
                "{label} attach stream ended with an error instead of a clean drain: {status:?}"
            ),
            Err(_) => panic!(
                "{label} attach stream did not reach a clean end within 5s of SIGTERM — looks \
                 hung rather than draining"
            ),
        }
    }
}

// ---- Story 5.2.1 -----------------------------------------------------

/// A second real `tymuxd` process started against the identical
/// `TYMUXD_SOCKET_PATH` while the first is still running must fail loudly
/// (nonzero exit, a distinct actionable stderr message) rather than
/// silently steal or corrupt the first instance's socket — and the first
/// instance must keep serving throughout.
#[tokio::test]
async fn tymuxd_second_instance_refuses_to_steal_a_live_socket() {
    let socket_path = short_unique_socket_path();
    let xdg_state_home1 = temp_xdg_state_home("race-first");
    let xdg_state_home2 = temp_xdg_state_home("race-second");

    let daemon1 = spawn_daemon_at("127.0.0.1:17460", &xdg_state_home1, &socket_path);
    let mut client1 = wait_for_daemon_uds(&socket_path).await;
    client1
        .list_sessions(ListSessionsRequest {})
        .await
        .expect("first instance should be serving before the race begins");

    // Second instance, identical socket path, while daemon1 still holds the
    // exclusive `flock` acquired by `auth::acquire_socket_lock` for its
    // entire process lifetime (ADR-001).
    let output = run_second_instance("127.0.0.1:17461", &xdg_state_home2, &socket_path);

    assert!(
        !output.status.success(),
        "a second tymuxd racing a live socket must exit nonzero"
    );
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("Error: "),
        "stderr should be the clean literal error text, not a Debug dump, got: {stderr}"
    );
    // The actual failure mode here is the flock-based lock (auth.rs's
    // `acquire_socket_lock`), not the connect-probe `reconcile_stale_socket`
    // path — the first instance holds the lock for its whole lifetime, so
    // the second instance never gets far enough to even probe the socket
    // file itself. Either message is a distinct, actionable "someone else
    // is already using this socket path" signal; assert on the one that
    // actually fires.
    assert!(
        stderr.contains("another tymuxd is already starting against"),
        "stderr should name the specific conflict, got: {stderr}"
    );
    assert!(
        stderr.contains(socket_path.to_str().unwrap()),
        "stderr should name the exact contended socket path, got: {stderr}"
    );

    // The first instance must be completely unaffected by the failed race.
    client1
        .list_sessions(ListSessionsRequest {})
        .await
        .expect("first instance should keep serving unaffected by the second instance's failure");

    drop(daemon1);
    std::fs::remove_dir_all(&xdg_state_home1).ok();
    std::fs::remove_dir_all(&xdg_state_home2).ok();
    if let Some(dir) = socket_path.parent() {
        std::fs::remove_dir_all(dir).ok();
    }
}

// ---- Story 5.2.2 -------------------------------------------------------

/// A clean SIGTERM-then-restart under an open UDS `Attach` stream: the new
/// instance re-binds the identical socket path with no lock/stale-socket
/// error (the first instance released its `SocketLockGuard` and removed its
/// own socket file on clean exit), and the session created before the
/// restart resumes — reloaded dead-flagged, matching the pre-restart
/// `Layout` shape — exactly as `restart_persistence.rs` already proves over
/// TCP.
// Needs a genuine second OS thread: `daemon.send_sigterm_and_wait_for_exit`
// blocks synchronously (`std::thread::sleep` polling), and the spawned
// `drain_task` below must keep making progress concurrently with that wait
// — draining the attach stream is what lets the pane's exit propagate and
// the graceful shutdown actually complete. On the default current-thread
// runtime the blocking wait would starve `drain_task` of any chance to
// run, deadlocking the whole test (confirmed empirically: this test hung
// past its 10s bound under the default single-threaded flavor).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tymuxd_restart_with_open_uds_attach_stream_resumes_cleanly() {
    let socket_path = short_unique_socket_path();
    let xdg_state_home = temp_xdg_state_home("restart-clean");
    let addr = "127.0.0.1:17462";

    let pre_restart_shape = {
        let mut daemon = spawn_daemon_at(addr, &xdg_state_home, &socket_path);
        let mut client = wait_for_daemon_uds(&socket_path).await;

        let session = client
            .create_session(CreateSessionRequest {
                name: "uds-restart-clean".to_string(),
                command: "/bin/sh".to_string(),
                cwd: String::new(),
            })
            .await
            .unwrap()
            .into_inner();
        let pane_id = first_pane_id(&session);
        let shape = summarize(session.windows[0].layout.as_ref().unwrap());

        // A genuinely open UDS Attach stream at the moment SIGTERM is sent
        // below — proves `SocketLockGuard`/`reconcile_stale_socket` compose
        // correctly with a live client in flight, not just the synthetic
        // two-concurrently-starting-processes race Story 5.2.1 covers. The
        // pane's command has a bounded lifetime (~1s) so the graceful drain
        // (Story 4.2.2/5.2.3) completes promptly rather than this daemon
        // hanging forever waiting on a pty that never exits — tonic's
        // `serve_with_shutdown` waits for every open connection to finish
        // before returning.
        let mut attach_stream =
            open_attach_with_command(&mut client, &pane_id, "sleep 1; exit\n").await;
        // Must keep actively reading, not just holding the stream open: the
        // server's forward loop sends events into a bounded (64-slot) mpsc
        // channel — if nothing drains it, `forward_tx.send(...).await`
        // blocks forever once it fills, which would keep forward_handle
        // from ever observing the pane's exit, so the connection (and the
        // graceful shutdown waiting on it) would never finish either.
        let drain_task =
            tokio::spawn(async move { while let Ok(Some(_)) = attach_stream.message().await {} });

        daemon.send_sigterm_and_wait_for_exit(Duration::from_secs(10));
        drain_task.await.expect("drain task should not panic");
        shape
    };

    // Restart, pointed at the SAME socket path and state dir.
    let _daemon2 = spawn_daemon_at(addr, &xdg_state_home, &socket_path);
    let mut client2 = wait_for_daemon_uds(&socket_path).await;

    let list = client2
        .list_sessions(ListSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    let restored = list
        .sessions
        .iter()
        .find(|s| s.name == "uds-restart-clean")
        .expect("the session created before the clean restart should have been reloaded");

    assert_eq!(
        restored.liveness,
        Liveness::Dead as i32,
        "a reloaded session must be dead-flagged, never auto-revived (ADR-002)"
    );
    let post_restart_shape = summarize(restored.windows[0].layout.as_ref().unwrap());
    assert_eq!(
        pre_restart_shape, post_restart_shape,
        "the reloaded LayoutNode shape must match the pre-restart shape exactly"
    );

    std::fs::remove_dir_all(&xdg_state_home).ok();
}

/// The unclean-exit (SIGKILL, no graceful drain) variant: the socket file
/// is left behind on disk, `reconcile_stale_socket`'s connect-probe detects
/// it as stale (nothing answers), removes it, and the restarted instance
/// binds and resumes exactly as the clean-shutdown case does.
#[tokio::test]
async fn tymuxd_restart_after_unclean_exit_with_open_uds_attach_stream_resumes_cleanly() {
    let socket_path = short_unique_socket_path();
    let xdg_state_home = temp_xdg_state_home("restart-unclean");
    let addr = "127.0.0.1:17463";

    let pre_restart_shape = {
        let mut daemon = spawn_daemon_at(addr, &xdg_state_home, &socket_path);
        let mut client = wait_for_daemon_uds(&socket_path).await;

        let session = client
            .create_session(CreateSessionRequest {
                name: "uds-restart-unclean".to_string(),
                command: "/bin/sh".to_string(),
                cwd: String::new(),
            })
            .await
            .unwrap()
            .into_inner();
        let pane_id = first_pane_id(&session);
        let shape = summarize(session.windows[0].layout.as_ref().unwrap());

        // Open UDS Attach stream, still attached when SIGKILL below hits —
        // proves `reconcile_stale_socket`'s stale-file cleanup composes
        // with a genuinely in-flight client. No bounded pty lifetime needed
        // here: SIGKILL bypasses graceful shutdown entirely, so there is no
        // drain to wait on.
        let _attach_stream = open_attach_with_command(&mut client, &pane_id, "").await;

        daemon.child.kill().expect("failed to SIGKILL daemon");
        daemon
            .child
            .wait()
            .expect("failed to reap SIGKILLed daemon");
        shape
    };

    assert!(
        socket_path.exists(),
        "an unclean (SIGKILL) exit should leave the socket file behind — no graceful cleanup ran"
    );

    // Restart, pointed at the SAME socket path and state dir — must detect
    // the leftover socket file as stale (nothing listening) and clean it up
    // via `reconcile_stale_socket`, not refuse to start.
    let _daemon2 = spawn_daemon_at(addr, &xdg_state_home, &socket_path);
    let mut client2 = wait_for_daemon_uds(&socket_path).await;

    let list = client2
        .list_sessions(ListSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    let restored = list
        .sessions
        .iter()
        .find(|s| s.name == "uds-restart-unclean")
        .expect("the session created before the unclean exit should have been reloaded");

    assert_eq!(
        restored.liveness,
        Liveness::Dead as i32,
        "a reloaded session must be dead-flagged, never auto-revived (ADR-002)"
    );
    let post_restart_shape = summarize(restored.windows[0].layout.as_ref().unwrap());
    assert_eq!(
        pre_restart_shape, post_restart_shape,
        "the reloaded LayoutNode shape must match the pre-restart shape exactly"
    );

    std::fs::remove_dir_all(&xdg_state_home).ok();
}

// ---- Story 5.2.3 (validation.md Gap 1) --------------------------------

/// The automated proof Story 4.2.2's own ACs were missing (validation.md's
/// self-identified Gap 1, now resolved): a real `tymuxd` subprocess with
/// both listeners enabled (default config) accepts one `Attach` stream over
/// TCP and one over UDS concurrently, and sending SIGTERM drains both
/// gracefully — a clean stream-end on each, never a hard reset — before the
/// process exits, within a bounded time.
#[tokio::test]
async fn tymuxd_dual_listener_drains_concurrent_tcp_and_uds_attach_streams_on_sigterm() {
    let socket_path = short_unique_socket_path();
    let xdg_state_home = temp_xdg_state_home("dual-drain");
    let addr = "127.0.0.1:17464";

    // Default config: both listeners active (no --disable-tcp-loopback).
    let mut daemon = spawn_daemon_at(addr, &xdg_state_home, &socket_path);

    let mut tcp_client = wait_for_daemon_tcp(addr).await;
    let mut uds_client = wait_for_daemon_uds(&socket_path).await;

    let tcp_session = tcp_client
        .create_session(CreateSessionRequest {
            name: "dual-drain-tcp".to_string(),
            command: "/bin/sh".to_string(),
            cwd: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let tcp_pane_id = first_pane_id(&tcp_session);

    let uds_session = uds_client
        .create_session(CreateSessionRequest {
            name: "dual-drain-uds".to_string(),
            command: "/bin/sh".to_string(),
            cwd: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let uds_pane_id = first_pane_id(&uds_session);

    // AC1: one Attach stream over TCP and one over UDS, established
    // CONCURRENTLY (not sequentially), each succeeding independently — the
    // real dual-listener `main()` path, not Epic 5.1's UDS-only
    // `spawn_uds_test_server` harness. Each pane's command has a bounded
    // (~1s) lifetime so the graceful drain below completes promptly rather
    // than hanging on a pty that never exits.
    let (mut tcp_stream, mut uds_stream) = tokio::join!(
        open_attach_with_command(&mut tcp_client, &tcp_pane_id, "sleep 1; exit\n"),
        open_attach_with_command(&mut uds_client, &uds_pane_id, "sleep 1; exit\n"),
    );

    // Confirm both streams are genuinely receiving output from their own
    // session (the echoed input from the command just sent), not merely
    // that the RPC was accepted.
    let tcp_event = tokio::time::timeout(Duration::from_secs(5), tcp_stream.message())
        .await
        .expect("tcp attach stream stalled waiting for output")
        .expect("tcp attach stream errored")
        .expect("tcp attach stream ended early");
    assert!(tcp_event.payload.is_some());
    let uds_event = tokio::time::timeout(Duration::from_secs(5), uds_stream.message())
        .await
        .expect("uds attach stream stalled waiting for output")
        .expect("uds attach stream errored")
        .expect("uds attach stream ended early");
    assert!(uds_event.payload.is_some());

    // AC2: SIGTERM drains both open streams gracefully before the process
    // exits, within a bounded time. Each pane's shell exits ~1s after the
    // command sent above, letting forward_handle observe the exit, emit
    // ExitStatus, and end the gRPC stream cleanly — which is what lets
    // tonic's serve_with_shutdown (which otherwise waits for every open
    // connection to finish) actually complete.
    daemon.send_sigterm();

    tokio::join!(
        assert_attach_stream_drains_cleanly(&mut tcp_stream, "tcp"),
        assert_attach_stream_drains_cleanly(&mut uds_stream, "uds"),
    );

    daemon.wait_for_exit(Duration::from_secs(8));

    std::fs::remove_dir_all(&xdg_state_home).ok();
}
