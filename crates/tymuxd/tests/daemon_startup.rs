//! Story 4.3 — daemon-startup integration tests. `FsPersistenceBackend::load_all`
//! already has isolated unit tests (in `crates/tymux-core/src/persistence.rs`)
//! covering corrupt-JSON and structurally-invalid-file skip behavior directly
//! against the backend. What's missing (and what this file covers) is driving
//! that same behavior through an ACTUAL daemon startup: write a combined
//! fixture directory, spawn the real `tymuxd` binary against it (matching
//! `restart_persistence.rs`'s hard-merge-gate pattern — real subprocess, real
//! gRPC, not a mocked harness), and assert on `ListSessions` after boot.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tymux_core::{
    Orientation as CoreOrientation, PersistedLayoutNode, PersistedPaneRecord,
    PersistedSessionRecord, PersistedWindowRecord, CURRENT_SCHEMA_VERSION,
};
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{ListSessionsRequest, Liveness};
use uuid::Uuid;

struct DaemonProcess {
    child: Child,
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

/// `default_sessions_dir()` (`crates/tymux-core/src/persistence.rs`) joins
/// `tymux/sessions` onto `$XDG_STATE_HOME` — fixture files need to land in
/// that exact nested directory, which the daemon would otherwise create
/// itself on first save.
fn sessions_dir(xdg_state_home: &Path) -> std::path::PathBuf {
    xdg_state_home.join("tymux").join("sessions")
}

fn leaf(command: &str) -> PersistedLayoutNode {
    PersistedLayoutNode::Leaf {
        pane: PersistedPaneRecord {
            pane_id: Uuid::new_v4(),
            command: command.to_string(),
            cwd: "/tmp".to_string(),
            rows: 24,
            cols: 80,
        },
    }
}

fn valid_record(name: &str) -> PersistedSessionRecord {
    let session_id = Uuid::new_v4();
    let window_id = Uuid::new_v4();
    PersistedSessionRecord {
        schema_version: CURRENT_SCHEMA_VERSION,
        session_id,
        name: name.to_string(),
        windows: vec![PersistedWindowRecord {
            id: window_id,
            name: "0".to_string(),
            layout: leaf("/bin/sh"),
        }],
        active_window_id: window_id,
    }
}

/// A record that parses as valid JSON with the current `schema_version` but
/// fails `PersistedLayoutNode::validate_structure()` — a 3-child `Split`,
/// per Story 4.1 AC3 / Story 4.3 AC2.
fn structurally_invalid_record(name: &str) -> PersistedSessionRecord {
    let session_id = Uuid::new_v4();
    let window_id = Uuid::new_v4();
    PersistedSessionRecord {
        schema_version: CURRENT_SCHEMA_VERSION,
        session_id,
        name: name.to_string(),
        windows: vec![PersistedWindowRecord {
            id: window_id,
            name: "0".to_string(),
            layout: PersistedLayoutNode::Split {
                orientation: CoreOrientation::Horizontal,
                children: vec![
                    (leaf("/bin/sh"), 0.34),
                    (leaf("/bin/sh"), 0.33),
                    (leaf("/bin/sh"), 0.33),
                ],
            },
        }],
        active_window_id: window_id,
    }
}

fn write_record(dir: &Path, record: &PersistedSessionRecord) {
    let path = dir.join(format!("{}.json", record.session_id));
    let json = serde_json::to_vec_pretty(record).expect("record should serialize");
    std::fs::write(path, json).expect("fixture file should write");
}

fn write_corrupt_json(dir: &Path, filename: &str) {
    std::fs::write(dir.join(filename), b"{ this is not valid json ")
        .expect("fixture file should write");
}

fn temp_xdg_state_home(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tymuxd-daemon-startup-test-{label}-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(sessions_dir(&dir)).unwrap();
    dir
}

#[tokio::test]
async fn daemon_startup_should_load_valid_sessions_dead_flagged_and_skip_corrupt_json_file() {
    let addr = "127.0.0.1:17431";
    let xdg_state_home = temp_xdg_state_home("valid-plus-corrupt");
    let dir = sessions_dir(&xdg_state_home);

    write_record(&dir, &valid_record("alpha"));
    write_record(&dir, &valid_record("beta"));
    write_record(&dir, &valid_record("gamma"));
    write_corrupt_json(&dir, "corrupt.json");

    let _daemon = spawn_daemon(addr, &xdg_state_home);
    let mut client = wait_for_daemon(addr).await;

    let list = client
        .list_sessions(ListSessionsRequest {})
        .await
        .expect("daemon should start successfully despite the corrupt file")
        .into_inner();

    assert_eq!(
        list.sessions.len(),
        3,
        "all 3 valid sessions should load, the corrupt-JSON file should be skipped"
    );
    for session in &list.sessions {
        assert_eq!(
            session.liveness,
            Liveness::Dead as i32,
            "every restored session's pane(s) should be dead-flagged, never auto-revived"
        );
    }

    std::fs::remove_dir_all(&xdg_state_home).ok();
}

#[tokio::test]
async fn daemon_startup_should_skip_structurally_invalid_layout_file_without_reaching_engine_session_map(
) {
    let addr = "127.0.0.1:17432";
    let xdg_state_home = temp_xdg_state_home("structurally-invalid-only");
    let dir = sessions_dir(&xdg_state_home);

    write_record(&dir, &structurally_invalid_record("bad-layout"));

    let _daemon = spawn_daemon(addr, &xdg_state_home);
    let mut client = wait_for_daemon(addr).await;

    let list = client
        .list_sessions(ListSessionsRequest {})
        .await
        .expect("daemon should start successfully despite the structurally-invalid file")
        .into_inner();

    assert_eq!(
        list.sessions.len(),
        0,
        "a structurally-invalid record (3-child Split) must never reach Engine's \
         session map — it should never surface via ListSessions at all"
    );

    // Confirm the daemon is genuinely healthy (not merely up but wedged) by
    // driving a real mutation through it.
    let created = client
        .create_session(tymux_proto::v1::CreateSessionRequest {
            name: "post-startup-sanity-check".to_string(),
            command: "/bin/sh".to_string(),
        })
        .await
        .expect("daemon should still be able to service RPCs after skipping the bad file");
    assert_eq!(created.into_inner().name, "post-startup-sanity-check");

    std::fs::remove_dir_all(&xdg_state_home).ok();
}

#[tokio::test]
async fn daemon_startup_should_start_successfully_and_report_exactly_two_sessions_when_two_of_four_files_are_bad(
) {
    let addr = "127.0.0.1:17433";
    let xdg_state_home = temp_xdg_state_home("combined-fixture");
    let dir = sessions_dir(&xdg_state_home);

    // The explicit combined scenario named in Story 4.3 Task 4: 2 valid + 1
    // corrupt-JSON + 1 structurally-invalid-but-valid-JSON.
    write_record(&dir, &valid_record("good-one"));
    write_record(&dir, &valid_record("good-two"));
    write_corrupt_json(&dir, "corrupt.json");
    write_record(&dir, &structurally_invalid_record("bad-layout"));

    let _daemon = spawn_daemon(addr, &xdg_state_home);
    let mut client = wait_for_daemon(addr).await;

    let list = client
        .list_sessions(ListSessionsRequest {})
        .await
        .expect("daemon should start successfully with 2 of 4 fixture files bad")
        .into_inner();

    assert_eq!(
        list.sessions.len(),
        2,
        "exactly the 2 valid files should load; the corrupt and structurally-invalid \
         files should both be logged and skipped, neither fatal"
    );
    let names: std::collections::HashSet<_> =
        list.sessions.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains("good-one"));
    assert!(names.contains("good-two"));

    std::fs::remove_dir_all(&xdg_state_home).ok();
}
