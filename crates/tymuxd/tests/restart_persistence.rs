//! Story 4.4/4.x — the hard merge gate named explicitly in plan.md §5 Risk
//! Control: "Epic 4 cannot merge without a test that kills and restarts a
//! daemon mid-session and asserts the reloaded record matches the
//! pre-restart `LayoutNode` shape." This spawns the *real* `tymuxd` binary
//! as a subprocess (not just an in-process `Engine`/`TymuxDaemon` call),
//! kills it, restarts it pointed at the same persisted-sessions directory,
//! and asserts the reloaded session's `Layout` tree shape is identical.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tymux_proto::v1::layout::Node;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    CreateSessionRequest, ListSessionsRequest, Liveness, Orientation, SplitPaneRequest,
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
        .stderr(Stdio::null())
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

    let session_id = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        client
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
            .id
    };

    async fn wait_for_stderr_lines(path: &std::path::Path, want: &[&str]) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = std::fs::read_to_string(path).unwrap_or_default();
            if got.lines().collect::<Vec<_>>() == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stderr file has {got:?}, want lines {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    wait_for_stderr_lines(&stderr_file, &["value-one"]).await;

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

    let session_id = {
        let _daemon = spawn_daemon(addr, &sessions_dir);
        let mut client = wait_for_daemon(addr).await;
        client
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
            .id
    };

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
    wait_for_log(&log, "input-one\n").await;

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
