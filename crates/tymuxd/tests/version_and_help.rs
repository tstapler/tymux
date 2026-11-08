//! Informational commands must exit before state access or listener startup.
//! An invalid address makes an accidental daemon launch fail instead of bind.
#![allow(clippy::disallowed_methods)] // integration tests drive real processes and files

use std::process::{Command, Output};
use std::time::Duration;

fn run_with_timeout(flag: &str) -> Output {
    let state = std::env::temp_dir().join(format!("tymux-build-info-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .arg(flag)
        .env("TYMUXD_ADDR", "not-a-socket-address")
        .env("TYMUXD_SOCKET_PATH", state.join("daemon.sock"))
        .env("TYMUXD_DATA_DIR", state.join("data"))
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
            let _ = child.wait();
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
fn build_info_reports_embedded_commit_without_starting_daemon() {
    let output = run_with_timeout("--build-info");
    assert!(output.status.success(), "build info failed: {:?}", output);
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).expect("build info JSON");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    let commit = info["commit"].as_str().expect("commit identity");
    assert_eq!(commit, env!("TYMUX_BUILD_SHA"));
    if commit != "unknown" {
        assert!(matches!(commit.len(), 40 | 64), "invalid commit: {commit}");
        assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    assert_eq!(info["dirty"], env!("TYMUX_BUILD_DIRTY"));
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
