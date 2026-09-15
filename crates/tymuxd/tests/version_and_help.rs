//! #49 — `tymuxd --version`/`--help` must print and exit 0 without ever
//! binding a port or touching persisted state, unlike every other argv the
//! daemon accepts. `TYMUXD_ADDR` is deliberately pointed at an address this
//! process can't bind as non-root (privileged port 1) so a run that
//! regressed into actually starting the daemon fails loudly instead of
//! silently passing on a lucky bind — and if the test process itself is
//! root (e.g. a containerized CI runner, where that bind would succeed),
//! `run_with_timeout`'s deadline still turns a genuine regression into a
//! panic instead of a suite-wide hang, mirroring `uds_socket_startup_failures.rs::run_tymuxd`.

use std::process::{Command, Output};
use std::time::Duration;

fn run_with_timeout(flag: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .arg(flag)
        .env("TYMUXD_ADDR", "127.0.0.1:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn tymuxd {flag}: {e}"));

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "tymuxd {flag} should have exited promptly, but the daemon appears to have started"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("failed to collect tymuxd {flag} output: {e}"))
}

#[test]
fn version_and_help_flags_exit_zero_without_starting_the_daemon() {
    for flag in ["--version", "-V", "--help", "-h"] {
        let output = run_with_timeout(flag);

        assert!(
            output.status.success(),
            "tymuxd {flag} should exit 0, got {:?} (stderr: {})",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).is_empty(),
            "tymuxd {flag} should print something to stdout"
        );
    }
}
