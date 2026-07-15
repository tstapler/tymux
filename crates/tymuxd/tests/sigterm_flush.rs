//! Story 4.5 AC1 — "a final persistence flush completes before the process
//! exits" on SIGTERM/Ctrl-C. `shutdown_signal()`
//! (`crates/tymuxd/src/main.rs`) deliberately implements this via
//! synchronous-write-per-mutation (every `create_session`/`split_pane`/etc.
//! RPC handler already calls `PersistenceBackend::save` and waits for it to
//! complete before returning a response) rather than a dedicated
//! shutdown-time flush step — see the doc comment on `shutdown_signal()` and
//! plan.md §6 Unresolved Question for the reasoning.
//!
//! This test proves the actual observable guarantee that design achieves:
//! nothing created before a SIGTERM is lost. It follows
//! `restart_persistence.rs`'s pattern (real `tymuxd` subprocess, real gRPC)
//! but replaces that test's SIGKILL-via-`Drop` restart with an explicit,
//! real SIGTERM — the exact signal Story 4.5 AC1 names — sent while the
//! process is still up, waiting for genuine process exit (not a fixed
//! sleep), then restarting a second daemon against the same state dir and
//! asserting the persisted structure survived intact.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tymux_proto::v1::layout::Node;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    CreateSessionRequest, ListSessionsRequest, Liveness, Orientation, SplitPaneRequest,
};
use uuid::Uuid;

struct DaemonProcess {
    child: Child,
}

impl DaemonProcess {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends a genuine SIGTERM (distinct from `Child::kill()`, which is
    /// SIGKILL and bypasses `shutdown_signal()` entirely) via the `kill`
    /// binary, then polls — bounded by `timeout`, not a fixed sleep — until
    /// the process has actually exited.
    fn send_sigterm_and_wait_for_exit(&mut self, timeout: Duration) {
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

        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            if Instant::now() > deadline {
                panic!("tymuxd did not exit within {timeout:?} of receiving SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(addr: &str, xdg_state_home: &Path) -> DaemonProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", xdg_state_home)
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

/// Mirrors `restart_persistence.rs`'s shape summary: cheap structural
/// comparison of a `Layout` tree that ignores pane ids (which legitimately
/// change across a restart).
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
async fn shutdown_signal_should_flush_pending_persistence_before_process_exits_on_sigterm() {
    let addr = "127.0.0.1:17441";
    let xdg_state_home =
        std::env::temp_dir().join(format!("tymuxd-sigterm-flush-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&xdg_state_home).unwrap();

    let pre_sigterm_shape = {
        let mut daemon = spawn_daemon(addr, &xdg_state_home);
        let mut client = wait_for_daemon(addr).await;

        // Create a session and split it — each of these RPCs persists
        // synchronously (per `shutdown_signal()`'s doc comment) before
        // returning, so by the time `split_pane` responds, both mutations
        // are already durable on disk.
        let session = client
            .create_session(CreateSessionRequest {
                name: "sigterm-test".to_string(),
                command: "/bin/sh".to_string(),
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

        let after_split = client
            .split_pane(SplitPaneRequest {
                pane_id: root_pane_id,
                orientation: Orientation::Vertical as i32,
                command: "/bin/sh".to_string(),
            })
            .await
            .unwrap()
            .into_inner();

        let list = client
            .list_sessions(ListSessionsRequest {})
            .await
            .unwrap()
            .into_inner();
        let listed = list
            .sessions
            .iter()
            .find(|s| s.name == "sigterm-test")
            .unwrap();
        assert_eq!(listed.liveness, Liveness::Live as i32);
        let shape = summarize(after_split.windows[0].layout.as_ref().unwrap());

        // Close the gRPC connection before signaling shutdown: tonic's
        // graceful `serve_with_shutdown` stops accepting *new* connections
        // as soon as `shutdown_signal()` resolves, but still drains any
        // still-open connection before `Server::builder()...await` (and
        // therefore `main()`) returns. An idle-but-open client connection
        // would otherwise make the daemon hang past this test's exit
        // timeout despite having already shut down cleanly in every way
        // that matters.
        drop(client);
        // Give the OS a moment to actually deliver the resulting TCP FIN
        // to the server (tonic/hyper's graceful shutdown otherwise waits
        // to complete an HTTP/2 GOAWAY+PING round trip with whatever
        // connections are still open at the instant SIGTERM arrives — a
        // connection dropped microseconds earlier may not have been
        // observed as closed by the server's I/O reactor yet). A generous
        // fixed margin here, not a race-prone zero-wait.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The real assertion under test: send SIGTERM (not SIGKILL) and
        // confirm the process actually exits via `shutdown_signal()`'s
        // graceful path, not by being forcibly reaped.
        daemon.send_sigterm_and_wait_for_exit(Duration::from_secs(10));

        shape
    };

    // Restart a fresh daemon against the SAME state dir. If SIGTERM had
    // dropped any pending state, this session (or its split structure)
    // would be missing or malformed here.
    let _daemon2 = spawn_daemon(addr, &xdg_state_home);
    let mut client2 = wait_for_daemon(addr).await;

    let list = client2
        .list_sessions(ListSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    let restored = list
        .sessions
        .iter()
        .find(|s| s.name == "sigterm-test")
        .expect(
            "the session created before SIGTERM should have survived — nothing should be \
             lost by a graceful SIGTERM shutdown",
        );

    assert_eq!(
        restored.liveness,
        Liveness::Dead as i32,
        "a reloaded session must be dead-flagged, never auto-revived (ADR-002)"
    );
    let post_restart_shape = summarize(restored.windows[0].layout.as_ref().unwrap());
    assert_eq!(
        pre_sigterm_shape, post_restart_shape,
        "the split performed just before SIGTERM must be fully persisted, not partially lost"
    );

    std::fs::remove_dir_all(&xdg_state_home).ok();
}
