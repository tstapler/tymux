use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;

/// A real `tymuxd` subprocess on an ephemeral loopback port with its own
/// throwaway `XDG_STATE_HOME` — killed and cleaned up on drop.
///
/// `tymuxd` itself binds an *internal* ephemeral port that test clients
/// never see directly. What `addr` actually points at is a thin TCP proxy
/// (Epic 2.5 / Task 2.5.1a) sitting in front of it: while enabled, the
/// proxy transparently forwards bytes both ways; [`TestDaemon::simulate_drop`]
/// disables it (refusing new connections and severing any already
/// forwarded ones) and [`TestDaemon::restore`] re-enables it, all without
/// ever touching the `tymuxd` subprocess — so its in-memory pane/
/// `ReplayBuffer` state survives a simulated drop exactly as a real
/// network blip or brief daemon-restart would leave it.
pub struct TestDaemon {
    pub addr: String,
    state_dir: std::path::PathBuf,
    child: Child,
    proxy_enabled: Arc<AtomicBool>,
    proxy_shutdown: Arc<AtomicBool>,
    proxy_connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
    proxy_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.proxy_shutdown.store(true, Ordering::SeqCst);
        // The proxy's accept loop rechecks this flag at least every 50ms
        // (see `spawn`), so this join returns quickly rather than risking
        // a hung teardown.
        if let Some(thread) = self.proxy_thread.take() {
            let _ = thread.join();
        }
        for handle in self.proxy_connections.lock().unwrap().drain(..) {
            handle.abort();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.state_dir).ok();
    }
}

fn ephemeral_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port to pick one for the test daemon")
        .local_addr()
        .unwrap()
        .port()
}

/// Spawns `tymuxd_bin` — pass `crate::workspace_bin("tymuxd")`.
pub fn spawn(tymuxd_bin: &std::path::Path) -> TestDaemon {
    // `tymuxd` itself binds here — this port is never exposed to test
    // clients directly (see the proxy below).
    let internal_port = ephemeral_port();
    let internal_addr = format!("127.0.0.1:{internal_port}");

    // Test clients connect to this port instead. It's a thin TCP proxy in
    // front of `internal_addr` (Task 2.5.1a): bound eagerly here (not via
    // `ephemeral_port()`'s bind-then-drop trick) so there's no race window
    // between picking the port and actually listening on it.
    let std_listener = StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port for the test-facing proxy listener");
    std_listener
        .set_nonblocking(true)
        .expect("set proxy listener non-blocking for tokio");
    let external_port = std_listener
        .local_addr()
        .expect("proxy listener has a local addr")
        .port();
    let addr = format!("127.0.0.1:{external_port}");

    let state_dir = std::env::temp_dir().join(format!(
        "tymux-e2e-daemon-{}-{internal_port}",
        std::process::id()
    ));
    std::fs::create_dir_all(&state_dir).unwrap();

    let child = Command::new(tymuxd_bin)
        .env("TYMUXD_ADDR", &internal_addr)
        .env("XDG_STATE_HOME", &state_dir)
        .env("RUST_LOG", "warn")
        // A deterministic prompt for any pane spawning /bin/sh — real
        // terminals never inherit a custom PS1 through tymuxd, but this
        // test process's own shell environment might, which would make
        // golden-snapshot tests flaky across machines/CI.
        .env("PS1", "$ ")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn tymuxd binary");

    let proxy_enabled = Arc::new(AtomicBool::new(true));
    let proxy_shutdown = Arc::new(AtomicBool::new(false));
    let proxy_connections: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

    // The proxy runs on its own dedicated OS thread with its own Tokio
    // runtime, deliberately *not* `tokio::spawn`ed onto the caller's own
    // `#[tokio::test]` runtime. Every caller in this crate is a
    // `#[tokio::test]` fn, which defaults to a single-threaded
    // (`current_thread`) runtime — and several existing tests (this
    // crate's own `disconnect_survival_e2e.rs` among them) pace themselves
    // with blocking `std::thread::sleep`, plus `CliHarness::wait_for`'s
    // poll loop does the same. A blocking sleep freezes that single OS
    // thread entirely, which would starve a `tokio::spawn`ed accept loop
    // of any chance to run — confirmed empirically: a proxy spawned that
    // way stops accepting the moment the test's own thread blocks, so a
    // CLI subprocess's connection attempt just sits unaccepted in the
    // kernel backlog for the sleep's whole duration. A dedicated thread
    // sidesteps this completely; the proxy keeps running regardless of
    // what the caller's own thread is doing.
    let accept_enabled = proxy_enabled.clone();
    let accept_connections = proxy_connections.clone();
    let accept_shutdown = proxy_shutdown.clone();
    let proxy_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the test-proxy's dedicated Tokio runtime");
        rt.block_on(async move {
            let tokio_listener = TcpListener::from_std(std_listener)
                .expect("convert the proxy's std listener into a tokio TcpListener");
            loop {
                if accept_shutdown.load(Ordering::SeqCst) {
                    return;
                }
                // A bounded wait, not a plain `.accept().await`, so this
                // loop rechecks the shutdown flag periodically instead of
                // blocking on it forever with nothing connecting.
                let accepted =
                    tokio::time::timeout(Duration::from_millis(50), tokio_listener.accept()).await;
                let (inbound, _peer) = match accepted {
                    Ok(Ok(pair)) => pair,
                    // A transient accept error shouldn't kill the whole
                    // proxy loop; just try again.
                    Ok(Err(_)) => continue,
                    // Timed out waiting for a connection; loop back to
                    // recheck the shutdown flag.
                    Err(_) => continue,
                };
                if !accept_enabled.load(Ordering::SeqCst) {
                    // Disabled: refuse the connection immediately instead
                    // of forwarding it. From a test client's perspective
                    // this looks like the server being unreachable — a
                    // fast, deterministic failure rather than a real hang,
                    // which keeps tests exercising this from needing long
                    // timeouts. `tymuxd` on the other side of the proxy
                    // never sees this attempt at all, so its state is
                    // untouched.
                    drop(inbound);
                    continue;
                }
                // Nagle's algorithm on a relayed connection like this can
                // compound with the peer's own delayed-ACK timer into tens
                // to hundreds of ms of added latency per round trip —
                // enough to make an interactive-attach test (e.g.
                // detach's "wait for a rendered response" check) flaky or
                // time out. Disable it on both legs, matching what a real
                // low-latency proxy would do.
                let _ = inbound.set_nodelay(true);
                let internal_addr = internal_addr.clone();
                let handle = tokio::spawn(async move {
                    if let Ok(outbound) = TcpStream::connect(&internal_addr).await {
                        let _ = outbound.set_nodelay(true);
                        let mut inbound = inbound;
                        let mut outbound = outbound;
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                let mut connections = accept_connections.lock().unwrap();
                // Opportunistically prune finished forwarding tasks so
                // this list doesn't grow unboundedly across a long-lived
                // test.
                connections.retain(|h| !h.is_finished());
                connections.push(handle);
            }
        });
    });

    TestDaemon {
        addr,
        state_dir,
        child,
        proxy_enabled,
        proxy_shutdown,
        proxy_connections,
        proxy_thread: Some(proxy_thread),
    }
}

impl TestDaemon {
    /// Blocks (async retry) until the daemon accepts gRPC connections.
    ///
    /// A plain TCP-level `connect()` success is not enough to prove
    /// `tymuxd` itself is actually up (Task 2.5.1a): `addr` now points at
    /// the proxy in front of it, whose own listening socket is bound the
    /// moment [`spawn`] returns — well before the `tymuxd` subprocess has
    /// finished starting and bound its *internal* port. The OS completes a
    /// client's TCP handshake against that listening socket immediately
    /// regardless of whether the proxy can yet forward to `tymuxd`, so a
    /// bare `connect()` would report "ready" too early. A cheap no-op RPC
    /// (`ListSessions`) forces a real round trip through the proxy to the
    /// backend and back, which only succeeds once `tymuxd` is genuinely
    /// serving.
    pub async fn wait_ready(&self) -> TymuxServiceClient<Channel> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(mut client) =
                TymuxServiceClient::connect(format!("http://{}", self.addr)).await
            {
                if client
                    .list_sessions(tymux_proto::v1::ListSessionsRequest {})
                    .await
                    .is_ok()
                {
                    return client;
                }
            }
            if Instant::now() > deadline {
                panic!("tymuxd did not become reachable within 10s");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Simulates a transport-level connection drop (Task 2.5.1a): the
    /// proxy in front of `tymuxd` stops forwarding, refusing new
    /// connection attempts and severing any already-forwarded ones. The
    /// `tymuxd` subprocess itself keeps running untouched — its in-memory
    /// pane/`ReplayBuffer` state is never affected.
    pub fn simulate_drop(&self) {
        self.proxy_enabled.store(false, Ordering::SeqCst);
        let mut connections = self.proxy_connections.lock().unwrap();
        for handle in connections.drain(..) {
            handle.abort();
        }
    }

    /// Restores transport reachability after [`Self::simulate_drop`] — new
    /// connections are forwarded to the same, never-restarted `tymuxd`
    /// subprocess again.
    pub fn restore(&self) {
        self.proxy_enabled.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;
    use tymux_proto::v1::{
        attach_event, attach_request, AttachEvent, AttachRequest, CapturePaneRequest,
        CreateSessionRequest,
    };

    /// Task 2.5.1b: proves `simulate_drop()`/`restore()` toggle transport
    /// reachability only, never the `tymuxd` subprocess or its state.
    ///
    /// Note on how "state survives" is proven here: plan.md's Task 2.5.1b
    /// describes proving this via a resumed `Attach` (`resume_from_seq`)
    /// replaying pre-drop `OutputChunk`s from the `ReplayBuffer`. As of
    /// this commit, `tymuxd`'s production `attach()` handler
    /// (`crates/tymuxd/src/main.rs`) does not yet wire `resume_from_seq`
    /// into `Pane::replay_since` at all — confirmed by grep, that field is
    /// read only in this crate's own unit tests, not in the RPC handler —
    /// so a real resumed-replay assertion would fail for a reason
    /// unrelated to this harness (a different, concurrently-developed
    /// epic's wiring, out of scope here and off-limits per this task's own
    /// instructions not to touch `tymuxd/src/main.rs`). Instead, this test
    /// proves the same underlying fact plan.md cares about — the daemon
    /// process and its state never actually stopped — with two assertions
    /// that already hold true today: (1) the `tymuxd` child PID is
    /// identical before and after the drop/restore cycle, and (2)
    /// `CapturePane` after `restore()` still shows the pre-drop marker
    /// text in the pane's live grid, proving the pane's own in-process
    /// state was never torn down. Once `resume_from_seq` is wired into
    /// `attach()`, a later E2E test (validation.md REQ-13 et al.) can
    /// additionally exercise the resumed-replay path against this same
    /// harness — the proxy itself is agnostic to wire semantics.
    #[tokio::test]
    async fn daemon_state_should_survive_simulated_drop_and_restore_without_process_restart() {
        let tymuxd_bin = crate::workspace_bin("tymuxd");
        let d = spawn(&tymuxd_bin);
        let daemon_pid_before = d.child.id();
        let mut client = d.wait_ready().await;

        let session = client
            .create_session(CreateSessionRequest {
                name: "transport-drop-smoke".into(),
                command: "/bin/sh".into(),
                cwd: String::new(),
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

        // Attach once, before any drop, and drive marked output through it
        // via the proxy — proves normal forwarding works, not just that
        // the toggle exists.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id.clone())),
            resume_from_seq: None,
        })
        .await
        .unwrap();
        let mut inbound = client
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap()
            .into_inner();

        // First event is always the priming Snapshot for a non-resuming
        // attach.
        let first = tokio::time::timeout(Duration::from_secs(5), inbound.message())
            .await
            .expect("attach must respond within 5s")
            .unwrap()
            .expect("stream ended before any event");
        assert!(
            matches!(first.payload, Some(attach_event::Payload::Snapshot(_))),
            "expected the first AttachEvent to be a Snapshot"
        );

        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::Input(
                b"echo PRE-DROP-MARKER\n".to_vec(),
            )),
            resume_from_seq: None,
        })
        .await
        .unwrap();

        // Drain live Output events (the wire format `forward_handle`
        // actually emits today) until the marker shows up, proving the
        // marker really was produced and forwarded through the proxy
        // before the drop.
        let mut streamed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !String::from_utf8_lossy(&streamed).contains("PRE-DROP-MARKER") {
            assert!(
                Instant::now() < deadline,
                "attach stream did not deliver PRE-DROP-MARKER in time"
            );
            let event = tokio::time::timeout(Duration::from_secs(5), inbound.message())
                .await
                .expect("attach stream stalled")
                .unwrap();
            if let Some(AttachEvent {
                payload: Some(attach_event::Payload::Output(bytes)),
            }) = event
            {
                streamed.extend_from_slice(&bytes);
            }
        }
        drop(inbound);
        drop(tx);
        drop(client);

        // Simulate a transport drop: new connections must fail to actually
        // *use* the daemon (fail or hang) even though tymuxd keeps
        // running. A bare TCP `connect()` alone isn't a strong enough
        // check here — the proxy's listening socket stays bound the whole
        // time, so the kernel completes a plain TCP handshake regardless
        // of whether forwarding is enabled (confirmed empirically: tonic's
        // `connect()` returns Ok even while disabled, since it doesn't
        // itself force a round trip). A real RPC call does force one, and
        // is what an actual client would do next anyway.
        d.simulate_drop();
        let rpc_succeeded = tokio::time::timeout(Duration::from_millis(500), async {
            match TymuxServiceClient::connect(format!("http://{}", d.addr)).await {
                Ok(mut c) => c
                    .list_sessions(tymux_proto::v1::ListSessionsRequest {})
                    .await
                    .is_ok(),
                Err(_) => false,
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            !rpc_succeeded,
            "a new connection attempt should fail to complete a real RPC (or hang past the \
             timeout) while the transport is simulated-dropped"
        );
        assert_eq!(
            d.child.id(),
            daemon_pid_before,
            "tymuxd's own PID must not change while simulate_drop() is in effect — the \
             subprocess must never be touched by the transport toggle"
        );

        // Restore reachability — tymuxd itself never restarted.
        d.restore();
        assert_eq!(
            d.child.id(),
            daemon_pid_before,
            "tymuxd's own PID must still be unchanged after restore() — proves the drop/restore \
             cycle never killed or respawned the subprocess"
        );
        let mut client2 = d.wait_ready().await;

        // The pane's own in-process state (its live grid, backing the
        // same struct `ReplayBuffer`/scrollback state lives on) must still
        // show the pre-drop marker — the daemon-side proof that nothing
        // was torn down and rebuilt across the gap.
        let snap = client2
            .capture_pane(CapturePaneRequest {
                pane_id,
                scrollback_offset: 0,
            })
            .await
            .expect("CapturePane should succeed once restore() re-enables forwarding")
            .into_inner();
        let captured_text: String = snap
            .grid
            .iter()
            .flat_map(|row| row.cells.iter())
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            captured_text.contains("PRE-DROP-MARKER"),
            "CapturePane after restore() should still show PRE-DROP-MARKER — proving the pane's \
             own state survived the simulated drop instead of being destroyed, got: \
             {captured_text:?}"
        );
    }
}
