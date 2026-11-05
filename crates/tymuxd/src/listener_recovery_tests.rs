use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn launch_generation(mode: &str) -> (OwnedChild, mpsc::Receiver<String>) {
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "listener_recovery_tests::listener_recovery_child",
            "--nocapture",
        ])
        .env("TYMUX_TEST_RECOVERY_GENERATION", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn isolated recovery child");
    let mut child = OwnedChild(child);
    let stdout = child.0.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

fn ready_address(lines: &mpsc::Receiver<String>) -> std::net::SocketAddr {
    loop {
        let line = lines
            .recv_timeout(Duration::from_secs(10))
            .expect("child readiness deadline");
        if let Some(address) = line.strip_prefix("RECOVERY_READY ") {
            return address.parse().expect("child address");
        }
    }
}

#[test]
fn listener_death_exits_child_and_supervisor_restarts_serving() {
    let (mut failed, lines) = launch_generation("fail");
    let address = ready_address(&lines);
    std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("sibling listener is alive before probe failure");
    failed
        .0
        .stdin
        .take()
        .expect("child control")
        .write_all(b"probe\n")
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = failed.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "listener failure did not exit child"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        status.code(),
        Some(1),
        "production exit guard must fail nonzero"
    );
    let (_restarted, lines) = launch_generation("healthy");
    let address = ready_address(&lines);
    let connection = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("supervisor replacement listener accepts connections");
    connection
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reply = String::new();
    BufReader::new(connection).read_line(&mut reply).unwrap();
    assert_eq!(reply, "healthy\n");
}

#[test]
fn listener_recovery_child() {
    let Ok(mode) = std::env::var("TYMUX_TEST_RECOVERY_GENERATION") else {
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_recovery_generation(&mode));
}

async fn run_recovery_generation(mode: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    println!("RECOVERY_READY {}", listener.local_addr().unwrap());
    std::io::stdout().flush().unwrap();
    if mode == "healthy" {
        serve_healthy_generation(listener).await;
    }
    let mut command = String::new();
    std::io::stdin().read_line(&mut command).unwrap();
    assert_eq!(command, "probe\n");
    let sibling = tokio::spawn(async move {
        loop {
            let _ = listener.accept().await.unwrap();
        }
    });
    let refusing = tokio::net::TcpSocket::new_v4().unwrap();
    refusing.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    tokio::time::pause();
    let address = refusing.local_addr().unwrap();
    let probe = tokio::spawn(async move {
        supervise_tcp_listener(address, None, &std::sync::atomic::AtomicBool::new(false)).await;
    });
    for _ in 0..100 {
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        if probe.is_finished() {
            break;
        }
    }
    probe.await.unwrap();
    assert!(!sibling.is_finished());
    panic!("production supervision returned after listener failure");
}

async fn serve_healthy_generation(listener: tokio::net::TcpListener) -> ! {
    loop {
        let (mut connection, _) = listener.accept().await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut connection, b"healthy\n")
            .await
            .unwrap();
    }
}
