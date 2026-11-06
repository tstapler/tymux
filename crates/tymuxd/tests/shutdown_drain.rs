//! SIGTERM must exit 0 within the drain bound even while an Attach stream is open, and a
//! shutdown must log "received SIGTERM" exactly once. Real `tymuxd` subprocess, real gRPC.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tymux_proto::v1::attach_request::Payload;
use tymux_proto::v1::layout::Node;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{AttachRequest, CreateSessionRequest};
use uuid::Uuid;

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Short path: macOS `sun_path` is ~104 bytes.
fn short_socket_path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/t-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ))
    .join("s.sock")
}

fn spawn(addr: &str, drain_secs: &str) -> Daemon {
    let state = std::env::temp_dir().join(format!("tymuxd-drain-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&state).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", state)
        .env("TYMUXD_SOCKET_PATH", short_socket_path())
        .env("RUST_LOG", "info")
        .arg("--shutdown-drain-secs")
        .arg(drain_secs)
        .stdout(Stdio::piped()) // tracing_subscriber::fmt logs to stdout
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tymuxd");
    Daemon { child }
}

async fn connect(addr: &str) -> TymuxServiceClient<tonic::transport::Channel> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(c) = TymuxServiceClient::connect(format!("http://{addr}")).await {
            return c;
        }
        assert!(Instant::now() < deadline, "tymuxd not reachable within 10s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn sigterm_should_exit_zero_within_the_drain_bound_while_an_attach_stream_is_open() {
    let addr = format!("127.0.0.1:{}", free_port());
    let mut daemon = spawn(&addr, "1");
    let mut client = connect(&addr).await;

    let session = client
        .create_session(CreateSessionRequest {
            name: "drain".into(),
            command: "/bin/sh".into(),
            ..Default::default()
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
        Node::Pane(p) => p.id.clone(),
        _ => panic!("fresh session should be a single pane"),
    };

    // The request stream never ends and the pane never exits, so the Attach never finishes.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(AttachRequest {
        payload: Some(Payload::PaneId(pane_id)),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut events = client
        .attach(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    events.message().await.unwrap().expect("first attach event");

    let started = Instant::now();
    let status = Command::new("kill")
        .args(["-TERM", &daemon.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let exit = loop {
        if let Some(exit) = daemon.child.try_wait().unwrap() {
            break exit;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "tymuxd did not exit within the drain bound (1s) plus slack"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(exit.code(), Some(0), "bounded drain exits 0, got {exit:?}");

    let mut log = String::new();
    daemon
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut log)
        .unwrap();
    assert_eq!(
        log.matches("received SIGTERM").count(),
        1,
        "one signal must log one 'received SIGTERM':\n{log}"
    );
    drop(tx);
}
