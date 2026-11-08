//! SIGTERM must exit 0 within the drain bound even while an Attach stream is open, and a
//! shutdown must log "received SIGTERM" exactly once. Real `tymuxd` subprocess, real gRPC.
#![allow(clippy::disallowed_methods)] // integration tests drive real processes and files

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tymux_proto::v1::attach_request::Payload;
use tymux_proto::v1::layout::Node;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{AttachRequest, CreateSessionRequest};
use uuid::Uuid;

struct Daemon {
    child: Child,
    dirs: Vec<std::path::PathBuf>,
    /// Filled by a reader thread so a full pipe never blocks the daemon, and so a grandchild
    /// pane holding the pipe open cannot hang the test.
    log: Arc<Mutex<String>>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Short dir: macOS `sun_path` is ~104 bytes.
fn short_socket_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/t-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ))
}

fn spawn(addr: &str, drain_secs: &str) -> Daemon {
    let state = std::env::temp_dir().join(format!("tymuxd-drain-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&state).unwrap();
    let socket_dir = short_socket_dir();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", &state)
        .env("TYMUXD_SOCKET_PATH", socket_dir.join("s.sock"))
        .env("RUST_LOG", "info")
        .arg("--shutdown-drain-secs")
        .arg(drain_secs)
        .stdout(Stdio::piped()) // tracing_subscriber::fmt logs to stdout
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tymuxd");
    let log = Arc::new(Mutex::new(String::new()));
    let mut stdout = child.stdout.take().unwrap();
    let sink = log.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    Daemon {
        child,
        dirs: vec![state, socket_dir],
        log,
    }
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

    // The reader thread may lag the exit slightly; wait (bounded) for the last line.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !daemon
        .log
        .lock()
        .unwrap()
        .contains("shutdown drain timed out")
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let log = daemon.log.lock().unwrap().clone();
    assert!(
        log.contains("shutdown drain timed out"),
        "the drain timer, not a clean drain, should have ended the process:\n{log}"
    );
    assert_eq!(
        log.matches("received SIGTERM").count(),
        1,
        "one signal must log one 'received SIGTERM':\n{log}"
    );
    drop(tx);
}
