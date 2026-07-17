use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tonic::transport::Channel;
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;

/// A real `tymuxd` subprocess on an ephemeral loopback port with its own
/// throwaway `XDG_STATE_HOME` — killed and cleaned up on drop.
pub struct TestDaemon {
    pub addr: String,
    state_dir: std::path::PathBuf,
    child: Child,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.state_dir).ok();
    }
}

fn ephemeral_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port to pick one for the test daemon")
        .local_addr()
        .unwrap()
        .port()
}

/// Spawns `tymuxd_bin` — pass `crate::workspace_bin("tymuxd")`.
pub fn spawn(tymuxd_bin: &std::path::Path) -> TestDaemon {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let state_dir =
        std::env::temp_dir().join(format!("tymux-e2e-daemon-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&state_dir).unwrap();

    let child = Command::new(tymuxd_bin)
        .env("TYMUXD_ADDR", &addr)
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

    TestDaemon {
        addr,
        state_dir,
        child,
    }
}

impl TestDaemon {
    /// Blocks (async retry) until the daemon accepts gRPC connections.
    pub async fn wait_ready(&self) -> TymuxServiceClient<Channel> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(client) = TymuxServiceClient::connect(format!("http://{}", self.addr)).await {
                return client;
            }
            if Instant::now() > deadline {
                panic!("tymuxd did not become reachable within 10s");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
