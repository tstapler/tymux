//! Story 4.4/4.x — the hard merge gate named explicitly in plan.md §5 Risk
//! Control: "Epic 4 cannot merge without a test that kills and restarts a
//! daemon mid-session and asserts the reloaded record matches the
//! pre-restart `LayoutNode` shape." This spawns the *real* `tymuxd` binary
//! as a subprocess (not just an in-process `Engine`/`TymuxDaemon` call),
//! kills it, restarts it pointed at the same persisted-sessions directory,
//! and asserts the reloaded session's `Layout` tree shape is identical.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio_stream::wrappers::ReceiverStream;
use tymux_proto::v1::layout::Node;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, AttachRequest, GetPaneStatusRequest, LifecycleEventKind,
    PaneState, RestartMode, RestartPolicy, SetRestartPolicyRequest, StartPaneRequest,
};
use tymux_proto::v1::{
    CreateSessionRequest, ListSessionsRequest, Liveness, Orientation, SplitPaneRequest,
    WriteStdinRequest,
};

struct DaemonProcess {
    child: Child,
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A short, fresh, per-test socket directory — deliberately NOT nested
/// under `sessions_dir` (whose descriptive-label-plus-uuid naming can push
/// a nested socket path past `SUN_LEN`, the ~108-byte kernel limit on
/// `AF_UNIX` paths — hit in practice while wiring this up, especially
/// under a long `$TMPDIR`) and NOT `sessions_dir` itself (already
/// pre-created at the default, non-0700 mode; `bind_uds_listener` refuses
/// to bind into a pre-existing directory at the wrong mode, by design).
fn short_unique_socket_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("tymuxd-test-{}", uuid::Uuid::new_v4().simple()))
        .join("s.sock")
}

/// Daemon stderr, appended across restarts so a failing test can print it.
fn daemon_log(sessions_dir: &std::path::Path) -> Stdio {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sessions_dir.join("daemon.log"))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

fn spawn_daemon(addr: &str, sessions_dir: &std::path::Path) -> DaemonProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", sessions_dir)
        // Epic 4.2: tymuxd now binds a UDS listener by default, singleton
        // per socket path (ADR-001's flock). Without a unique path here,
        // every real-subprocess test in this crate would contend for the
        // same uid-scoped default socket and fail to start concurrently —
        // give each spawned daemon its own path.
        .env("TYMUXD_SOCKET_PATH", short_unique_socket_path())
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(daemon_log(sessions_dir))
        .spawn()
        .expect("failed to spawn tymuxd binary");
    DaemonProcess { child }
}

async fn wait_for_daemon(addr: &str) -> TymuxServiceClient<tonic::transport::Channel> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(client) = TymuxServiceClient::connect(format!("http://{addr}")).await {
            return client;
        }
        if std::time::Instant::now() > deadline {
            panic!("tymuxd did not become reachable within 10s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Extracts a simple summary of a Layout tree's shape (orientation at each
/// Split, leaf count) — cheap to compare for structural equality without
/// caring about pane ids, which legitimately change across a revive.
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

#[tokio::test]
async fn session_layout_should_match_pre_restart_shape_when_daemon_is_killed_and_restarted_mid_session(
) {
    let addr = "127.0.0.1:17420";
    let sessions_dir =
        std::env::temp_dir().join(format!("tymuxd-restart-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let pre_restart_shape = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;

        let session = client
            .create_session(CreateSessionRequest {
                name: "restart-test".to_string(),
                command: "/bin/sh".to_string(),
                cwd: String::new(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let root_pane_id = match session.windows[0]
            .layout
            .as_ref()
            .unwrap()
            .node
            .as_ref()
            .unwrap()
        {
            Node::Pane(p) => p.id.clone(),
            _ => panic!("expected a fresh session to be a single leaf"),
        };

        // Build a nested shape: split once, then split one of the new
        // leaves again, so the persisted tree has real structure to
        // verify (not just a trivial 2-leaf case).
        let after_first_split = client
            .split_pane(SplitPaneRequest {
                pane_id: root_pane_id,
                orientation: Orientation::Horizontal as i32,
                command: "/bin/sh".to_string(),
            })
            .await
            .unwrap()
            .into_inner();
        let second_leaf_id = match after_first_split.windows[0]
            .layout
            .as_ref()
            .unwrap()
            .node
            .as_ref()
            .unwrap()
        {
            Node::Split(s) => match s.children[1]
                .layout
                .as_ref()
                .unwrap()
                .node
                .as_ref()
                .unwrap()
            {
                Node::Pane(p) => p.id.clone(),
                _ => panic!("expected a leaf"),
            },
            _ => panic!("expected a split"),
        };
        client
            .split_pane(SplitPaneRequest {
                pane_id: second_leaf_id,
                orientation: Orientation::Vertical as i32,
                command: "/bin/sh".to_string(),
            })
            .await
            .unwrap();

        let list = client
            .list_sessions(ListSessionsRequest {})
            .await
            .unwrap()
            .into_inner();
        let session = list
            .sessions
            .iter()
            .find(|s| s.name == "restart-test")
            .unwrap();
        assert_eq!(session.liveness, Liveness::Live as i32);
        summarize(session.windows[0].layout.as_ref().unwrap())

        // `_daemon` drops here: SIGKILL, matching a real crash/restart —
        // not a clean shutdown, deliberately, to prove the persisted
        // atomic-write path (not a graceful-shutdown flush) is what makes
        // this durable.
    };

    // Restart, pointed at the SAME sessions directory.
    let _daemon2 = spawn_daemon(addr, &sessions_dir);
    let mut client2 = wait_for_daemon(addr).await;

    let list = client2
        .list_sessions(ListSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    let restored = list
        .sessions
        .iter()
        .find(|s| s.name == "restart-test")
        .expect("the session should have been reloaded from its persisted record");

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

    std::fs::remove_dir_all(&sessions_dir).ok();
}

/// The wire-level revive contract for file-backed arguments and a stderr
/// file: a revive re-reads the file (not a snapshot taken at create), appends
/// to the same stderr file, and a file that vanished fails the revive with
/// FAILED_PRECONDITION naming it, leaving the session dead.
#[tokio::test]
async fn revive_should_reread_arg_file_and_fail_clearly_when_it_is_gone() {
    use tymux_proto::v1::{arg_spec, ArgSpec, ReviveSessionRequest};

    let addr = "127.0.0.1:17421";
    let sessions_dir =
        std::env::temp_dir().join(format!("tymuxd-restart-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let arg_file = sessions_dir.join("prompt.txt");
    let stderr_file = sessions_dir.join("stderr.log");
    std::fs::write(&arg_file, "value-one").unwrap();

    async fn wait_for_stderr_lines(path: &std::path::Path, want: &[&str]) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = std::fs::read_to_string(path).unwrap_or_default();
            if got.lines().collect::<Vec<_>>() == want {
                return;
            }
            let daemon_log = path
                .parent()
                .and_then(|dir| std::fs::read_to_string(dir.join("daemon.log")).ok())
                .unwrap_or_default();
            assert!(
                std::time::Instant::now() < deadline,
                "stderr file has {got:?}, want lines {want:?}; daemon log:\n{daemon_log}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let session_id = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let id = client
            .create_session(CreateSessionRequest {
                name: "file-args".to_string(),
                command: "/bin/sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "echo \"$1\" >&2; read x".to_string(),
                    "sh".to_string(),
                ],
                arg_specs: vec![ArgSpec {
                    source: Some(arg_spec::Source::FileContentsPath(
                        arg_file.display().to_string(),
                    )),
                }],
                stderr_path: stderr_file.display().to_string(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
            .id;
        // Dropping the daemon closes the pty and SIGHUPs the child, so wait
        // for its output first (a slow exec loses this race on macOS).
        wait_for_stderr_lines(&stderr_file, &["value-one"]).await;
        id
    };

    // Crash, change the file, restart, revive.
    std::fs::write(&arg_file, "value-two").unwrap();
    {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let revived = client
            .revive_session(ReviveSessionRequest {
                session_id: session_id.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(revived.pane_count, 1);
        wait_for_stderr_lines(&stderr_file, &["value-one", "value-two"]).await;
    }

    // Crash again, delete the file: the revive must refuse, not run with an
    // empty argument.
    std::fs::remove_file(&arg_file).unwrap();
    {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let status = client
            .revive_session(ReviveSessionRequest {
                session_id: session_id.clone(),
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status}");
        assert!(
            status.message().contains("prompt.txt"),
            "error must name the missing file: {status}"
        );
        let listed = client
            .list_sessions(ListSessionsRequest {})
            .await
            .unwrap()
            .into_inner();
        let session = listed.sessions.iter().find(|s| s.id == session_id).unwrap();
        assert_eq!(session.liveness, Liveness::Dead as i32);
    }
    wait_for_stderr_lines(&stderr_file, &["value-one", "value-two"]).await;

    std::fs::remove_dir_all(&sessions_dir).ok();
}

/// A `stdin_path` pane is replayed on revive: the file is re-opened and read
/// from byte 0 again (seeing its current contents), and a deleted file
/// refuses the revive instead of starting the process on empty input.
#[tokio::test]
async fn revive_should_replay_stdin_file_and_fail_clearly_when_it_is_gone() {
    use tymux_proto::v1::ReviveSessionRequest;

    let addr = "127.0.0.1:17422";
    let sessions_dir =
        std::env::temp_dir().join(format!("tymuxd-restart-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let stdin_file = sessions_dir.join("input.txt");
    let log = sessions_dir.join("seen.log");
    std::fs::write(&stdin_file, "input-one\n").unwrap();

    async fn wait_for_log(path: &std::path::Path, want: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = std::fs::read_to_string(path).unwrap_or_default();
            if got == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "log has {got:?}, want {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let session_id = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let id = client
            .create_session(CreateSessionRequest {
                name: "stdin-file".to_string(),
                command: "/bin/sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!("cat >> {}; sleep 30", log.display()),
                ],
                stdin_path: stdin_file.display().to_string(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
            .id;
        // Dropping the daemon SIGHUPs the child: let it read the file first.
        wait_for_log(&log, "input-one\n").await;
        id
    };

    std::fs::write(&stdin_file, "input-two\n").unwrap();
    {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let revived = client
            .revive_session(ReviveSessionRequest {
                session_id: session_id.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(revived.pane_count, 1);
        wait_for_log(&log, "input-one\ninput-two\n").await;
    }

    std::fs::remove_file(&stdin_file).unwrap();
    {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let status = client
            .revive_session(ReviveSessionRequest {
                session_id: session_id.clone(),
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status}");
        assert!(
            status.message().contains("input.txt"),
            "error must name the missing file: {status}"
        );
        let listed = client
            .list_sessions(ListSessionsRequest {})
            .await
            .unwrap()
            .into_inner();
        let session = listed.sessions.iter().find(|s| s.id == session_id).unwrap();
        assert_eq!(session.liveness, Liveness::Dead as i32);
    }
    wait_for_log(&log, "input-one\ninput-two\n").await;

    std::fs::remove_dir_all(&sessions_dir).ok();
}

fn stdin_messages(pane_id: &str, data: &[u8], close: bool) -> Vec<WriteStdinRequest> {
    use tymux_proto::v1::write_stdin_request::Payload;
    let mut messages = vec![WriteStdinRequest {
        payload: Some(Payload::PaneId(pane_id.to_string())),
    }];
    if !data.is_empty() {
        messages.push(WriteStdinRequest {
            payload: Some(Payload::Data(data.to_vec())),
        });
    }
    if close {
        messages.push(WriteStdinRequest {
            payload: Some(Payload::Close(true)),
        });
    }
    messages
}

fn first_pane_id(session: &tymux_proto::v1::Session) -> String {
    match session.windows[0]
        .layout
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap()
    {
        Node::Pane(p) => p.id.clone(),
        other => panic!("expected a single pane, got {other:?}"),
    }
}

/// `stdin_stream`: a pane fed over WriteStdin sees EOF on close; after a
/// daemon restart and revive the pipe is open and empty again (the old bytes
/// are not replayed) and must be fed anew.
#[tokio::test]
async fn write_stdin_should_feed_a_stream_pane_and_need_refeeding_after_revive() {
    use tymux_proto::v1::ReviveSessionRequest;

    let addr = "127.0.0.1:17423";
    let sessions_dir =
        std::env::temp_dir().join(format!("tymuxd-restart-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let log = sessions_dir.join("seen.log");

    async fn wait_for_log(path: &std::path::Path, want: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = std::fs::read_to_string(path).unwrap_or_default();
            if got == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "log has {got:?}, want {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    let (session_id, pane_id) = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let session = client
            .create_session(CreateSessionRequest {
                name: "stdin-stream".to_string(),
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), format!("cat >> {}", log.display())],
                stdin_stream: true,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let pane_id = first_pane_id(&session);
        // Fed in two calls: the pipe stays open until `close`.
        let resp = client
            .write_stdin(tokio_stream::iter(stdin_messages(
                &pane_id, b"one\n", false,
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.bytes_written, 4);
        client
            .write_stdin(tokio_stream::iter(stdin_messages(&pane_id, b"and\n", true)))
            .await
            .unwrap();
        wait_for_log(&log, "one\nand\n").await;
        (session.id, pane_id)
    };

    let _daemon = spawn_daemon(addr, &sessions_dir);
    let mut client = wait_for_daemon(addr).await;
    // Dead pane: refused, not queued.
    let status = client
        .write_stdin(tokio_stream::iter(stdin_messages(&pane_id, b"x", false)))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status}");

    client
        .revive_session(ReviveSessionRequest { session_id })
        .await
        .unwrap();
    wait_for_log(&log, "one\nand\n").await; // nothing replayed
    client
        .write_stdin(tokio_stream::iter(stdin_messages(&pane_id, b"two\n", true)))
        .await
        .unwrap();
    wait_for_log(&log, "one\nand\ntwo\n").await;

    // Closed: further writes are refused.
    let status = client
        .write_stdin(tokio_stream::iter(stdin_messages(&pane_id, b"late", false)))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status}");

    std::fs::remove_dir_all(&sessions_dir).ok();
}

/// ADR-0007: policy and counters are in the session record. After a daemon restart a pane that
/// was waiting out a backoff is EXITED (the timer is dropped, nothing revives on its own), its
/// policy and counters are intact, and StartPane brings it back under the same policy.
#[tokio::test]
async fn restart_policy_and_counters_should_survive_a_daemon_restart_and_backoff_comes_back_exited()
{
    let addr = "127.0.0.1:17424";
    let sessions_dir =
        std::env::temp_dir().join(format!("tymuxd-restart-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let policy = RestartPolicy {
        mode: RestartMode::Always as i32,
        max_restarts: Some(4),
        window_ms: Some(120_000),
        initial_delay_ms: Some(3_600_000),
        multiplier: Some(2.0),
        max_delay_ms: Some(7_200_000),
        jitter: Some(0.0),
        healthy_after_ms: Some(10_000),
    };

    let (session_id, pane_id) = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        let session = client
            .create_session(CreateSessionRequest {
                name: "supervised".to_string(),
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "read go; exit 5".to_string()],
                restart_policy: Some(policy),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let pane_id = first_pane_id(&session);

        // Wait for the exit through the lifecycle stream, not a timer.
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id.clone())),
            resume_from_seq: None,
            subscribe_lifecycle: true,
        })
        .await
        .unwrap();
        let mut events = client
            .attach(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::Input(b"go\n".to_vec())),
            resume_from_seq: None,
            subscribe_lifecycle: false,
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.message().await.unwrap() {
                if let Some(attach_event::Payload::Lifecycle(e)) = event.payload {
                    if e.kind == LifecycleEventKind::RestartScheduled as i32 {
                        return;
                    }
                }
            }
            panic!("stream closed before the restart was scheduled");
        })
        .await
        .expect("restart scheduled within 10s");

        // The supervisor persists in the background; a policy write persists synchronously and
        // so also captures the exit just recorded.
        client
            .set_restart_policy(SetRestartPolicyRequest {
                target: Some(tymux_proto::v1::set_restart_policy_request::Target::PaneId(
                    pane_id.clone(),
                )),
                policy: Some(policy),
                apply_to_existing_panes: false,
            })
            .await
            .unwrap();
        let status = client
            .get_pane_status(GetPaneStatusRequest {
                pane_id: pane_id.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.state, PaneState::Backoff as i32);
        (session.id, pane_id)
    };

    let _daemon = spawn_daemon(addr, &sessions_dir);
    let mut client = wait_for_daemon(addr).await;
    let status = client
        .get_pane_status(GetPaneStatusRequest {
            pane_id: pane_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        status.state,
        PaneState::Exited as i32,
        "a pending backoff is dropped across a daemon restart, not resumed"
    );
    assert_eq!(status.next_restart_at_unix_ms, None);
    assert_eq!(status.policy.unwrap(), policy);
    assert_eq!(status.last_exit_code, Some(5));
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(status.pid, None);
    let listed = client
        .list_sessions(ListSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.sessions[0].id, session_id);

    let started = client
        .start_pane(StartPaneRequest {
            pane_id: pane_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(started.state, PaneState::Live as i32);
    assert_eq!(started.pane_id, pane_id, "same pane id after the restart");
    assert_eq!(started.policy.unwrap().mode, RestartMode::Always as i32);

    std::fs::remove_dir_all(&sessions_dir).ok();
}
