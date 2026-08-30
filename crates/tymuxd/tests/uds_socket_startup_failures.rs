//! Phase 4 / Epic 4.2, Tasks 4.2.1b/c/d — end-to-end proof that a UDS
//! bind-sequence failure (Story 4.2.1) is fatal to the whole `tymuxd`
//! process: a clean, actionable `eprintln!` message (never a `Debug`
//! dump) on stderr and exit code `1`, with the TCP listener never
//! spawning either. Mirrors `daemon_startup.rs`'s real-subprocess pattern
//! (spawn the actual `tymuxd` binary via `CARGO_BIN_EXE_tymuxd`), but
//! asserts on exit status/stderr rather than a successful `ListSessions`.

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use uuid::Uuid;

fn temp_xdg_state_home(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tymuxd-startup-failure-test-{label}-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(dir.join("tymux").join("sessions")).unwrap();
    dir
}

/// A short, fresh, per-test socket directory — deliberately NOT nested
/// under `temp_xdg_state_home`'s descriptive-label-plus-uuid path (which
/// can push a nested socket path past `SUN_LEN`, the ~108-byte kernel
/// limit on `AF_UNIX` paths — confirmed in practice while wiring this up)
/// and NOT that directory itself (already pre-created at the default,
/// non-0700 mode; `bind_uds_listener` refuses to bind into a pre-existing
/// directory at the wrong mode, by design).
fn short_unique_socket_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("tymuxd-test-{}", Uuid::new_v4().simple()))
        .join("s.sock")
}

/// Runs `tymuxd` to completion (it must exit on its own — these tests
/// only cover startup paths where `tymuxd` never reaches `serve_with_shutdown`)
/// with a bounded wall-clock timeout, so a regression that makes the
/// daemon hang instead of exiting fails the test rather than the whole
/// suite.
fn run_tymuxd(xdg_state_home: &Path, addr: &str, extra_args: &[&str]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
        .args(extra_args)
        .env("TYMUXD_ADDR", addr)
        .env("XDG_STATE_HOME", xdg_state_home)
        .env_remove("TYMUXD_SOCKET_GROUP")
        .env_remove("TYMUXD_SOCKET_PATH")
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn tymuxd binary");

    // wait_timeout isn't in std; poll with try_wait bounded by a deadline,
    // matching this crate's existing `sigterm_flush.rs` polling convention.
    let mut child = child;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("tymuxd should have exited on its own (fatal startup error) within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child
        .wait_with_output()
        .expect("failed to collect tymuxd output")
}

/// Task 4.2.1b / R4: an unwritable socket-path parent directory (here,
/// `/root`, which this non-root test process cannot create a subdirectory
/// under) is fatal — clean stderr text, exit code 1, never a Debug dump.
#[test]
fn main_exits_nonzero_with_clean_message_when_uds_socket_path_unwritable() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: test process is running as root, which can write under /root");
        return;
    }

    let xdg_state_home = temp_xdg_state_home("unwritable-socket-path");
    let bad_socket_path = "/root/tymuxd-test-should-not-be-creatable/tymuxd.sock";

    let output = run_tymuxd(
        &xdg_state_home,
        "127.0.0.1:17441",
        &["--socket-path", bad_socket_path],
    );

    assert!(
        !output.status.success(),
        "tymuxd should exit nonzero on an unwritable socket path"
    );
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("Error: failed to create Unix socket at"),
        "stderr should start with the clean literal error text, got: {stderr}"
    );
    assert!(
        stderr.contains(bad_socket_path),
        "stderr should name the exact socket path, got: {stderr}"
    );
    assert!(
        stderr.contains("--socket-path/TYMUXD_SOCKET_PATH"),
        "stderr should name the remedy flag, got: {stderr}"
    );
    assert!(
        !stderr.contains("Err(") && !stderr.contains("Backtrace"),
        "stderr must be clean literal text, never a Debug dump, got: {stderr}"
    );

    std::fs::remove_dir_all(&xdg_state_home).ok();
}

/// Task 4.2.1c / R5: an unknown `--socket-group` name fails loudly,
/// end-to-end through `main()`'s wiring — not just `resolve_gid_by_name`'s
/// own unit-level `None` return.
#[test]
fn main_exits_nonzero_with_clear_message_when_socket_group_unknown() {
    let xdg_state_home = temp_xdg_state_home("unknown-socket-group");
    let bogus_group = "tymux-test-nonexistent-group-83f2";

    let socket_path = short_unique_socket_path();
    let socket_path_str = socket_path.to_str().unwrap();
    let output = run_tymuxd(
        &xdg_state_home,
        "127.0.0.1:17442",
        &[
            "--socket-group",
            bogus_group,
            "--socket-path",
            socket_path_str,
        ],
    );

    assert!(
        !output.status.success(),
        "tymuxd should exit nonzero on an unknown --socket-group name"
    );
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--socket-group/TYMUXD_SOCKET_GROUP names an unknown group"),
        "stderr should name the specific problem, got: {stderr}"
    );
    assert!(
        stderr.contains(bogus_group),
        "stderr should echo the exact bad group name, got: {stderr}"
    );

    std::fs::remove_dir_all(&xdg_state_home).ok();
}

/// Task 4.2.1d / R5: a `--socket-group` the daemon's own process is not a
/// member of fails the group `chown` with `EPERM`, with a message
/// distinct from the generic bind-failure text. Skipped if the test
/// process happens to already be root or a member of the `root` group
/// (matches this plan's established CI-privilege-guard pattern).
#[test]
fn main_exits_nonzero_with_clear_message_when_socket_group_membership_denied() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: test process is running as root, which is a member of every group");
        return;
    }
    let groups_output = Command::new("id")
        .arg("-Gn")
        .output()
        .expect("failed to invoke `id -Gn`");
    let groups = String::from_utf8_lossy(&groups_output.stdout);
    if groups.split_whitespace().any(|g| g == "root") {
        eprintln!("skipping: test process is already a member of the `root` group");
        return;
    }

    let xdg_state_home = temp_xdg_state_home("socket_group_membership_denied");
    let socket_path = short_unique_socket_path();
    let socket_path_str = socket_path.to_str().unwrap();

    let output = run_tymuxd(
        &xdg_state_home,
        "127.0.0.1:17443",
        &["--socket-group", "root", "--socket-path", socket_path_str],
    );

    assert!(
        !output.status.success(),
        "tymuxd should exit nonzero when it cannot chown the socket to the configured group"
    );
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not a member of"),
        "stderr should contain the specific membership-denied text, not the generic \
         bind-failure message, got: {stderr}"
    );
    assert!(
        !stderr.contains("Check that the parent directory exists and is writable"),
        "stderr should not be the generic bind-failure message, got: {stderr}"
    );

    std::fs::remove_dir_all(&xdg_state_home).ok();
}
