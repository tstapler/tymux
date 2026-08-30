//! Epic 6.4 / Story 6.4.1 — `tymux-cli` integration tests against a real,
//! subprocess-spawned `tymuxd` (dual TCP+UDS listener, `b44aae1`) and a
//! real `tymux` subprocess (UDS-first dialing with TCP fallback, `cb0b37f`).
//! Mirrors this repo's established real-subprocess pattern
//! (`crates/tymuxd/tests/daemon_startup.rs`, `crates/tymux-e2e/src/lib.rs`'s
//! `workspace_bin`) rather than mocking either binary.
//!
//! `tymux-cli/src/main.rs`'s own unit tests already cover `dial_channel`/
//! `dial_uds`'s classification logic against a bare, service-less
//! `tonic::transport::Server` (Task 6.2.1c) — this file supplies the "an
//! actual `tymux ls` round-trips against a real `tymuxd`" proof those unit
//! tests explicitly deferred here.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::CreateSessionRequest;

/// Locates a sibling workspace binary (`tymuxd`) at runtime from this test
/// binary's own `current_exe()` path. `tymuxd` has no `[lib]` target, so it
/// can't be added as a path dependency of `tymux-cli` to get Cargo's usual
/// `CARGO_BIN_EXE_<name>` mechanism (confirmed by `crates/tymux-e2e/src/
/// lib.rs`'s identical `workspace_bin` helper and `tymux-cli/src/main.rs`'s
/// own test module, which hits the same problem and duplicates this same
/// fix rather than adding a cross-crate dependency for one helper
/// function). This crate's own `tymux` binary doesn't need this trick —
/// `env!("CARGO_BIN_EXE_tymux")` works directly from an integration test in
/// `tests/` (unlike from a unit test embedded in `main.rs`, where Cargo
/// never sets `CARGO_BIN_EXE_*` at all).
///
/// Requires `cargo build --workspace` (or at least `-p tymuxd`) to have
/// already run.
fn workspace_bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current test exe path");
    let deps_dir = exe.parent().expect("test exe has a parent dir");
    let profile_dir = deps_dir.parent().expect("deps dir has a parent dir");
    let candidate = profile_dir.join(name);
    assert!(
        candidate.exists(),
        "expected workspace binary at {candidate:?} — run `cargo build --workspace` first"
    );
    candidate
}

/// A short, fresh, not-yet-existing per-call temp directory — deliberately
/// short (an atomic counter + pid, not a UUID) to stay well under `SUN_LEN`
/// (the ~108-byte kernel limit on `AF_UNIX` paths) once a socket file is
/// joined onto it, mirroring `crates/tymuxd/tests/daemon_startup.rs`'s
/// `short_unique_socket_path` in spirit.
fn unique_temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tymux-cli-uds-it-{label}-{}-{n}",
        std::process::id()
    ))
}

/// Config for [`spawn_daemon`] — deliberately explicit about every env var
/// this file's tests care about (`TYMUXD_ADDR`, `TYMUXD_SOCKET_PATH`,
/// `TYMUXD_DISABLE_TCP_LOOPBACK`) rather than threading positional args.
struct DaemonConfig<'a> {
    tcp_addr: &'a str,
    socket_path: &'a Path,
    disable_tcp: bool,
}

/// A running `tymuxd` subprocess. `Drop` kills it (mirroring `daemon_startup
/// .rs`'s `DaemonProcess` — a plain SIGKILL is fine here, no test in this
/// file cares about graceful-shutdown draining) and removes its temp
/// state/socket directories.
struct TestDaemon {
    child: std::process::Child,
    state_dir: PathBuf,
    socket_dir: PathBuf,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.state_dir).ok();
        std::fs::remove_dir_all(&self.socket_dir).ok();
    }
}

/// Blocks (bounded by a 10s deadline) until `tymuxd`'s stdout reports
/// "tymuxd listening" — emitted unconditionally, regardless of whether the
/// TCP listener is enabled (`crates/tymuxd/src/main.rs`'s
/// `tracing::info!(%addr, uds_path = %socket_path.display(), "tymuxd
/// listening")`), so this works as the readiness signal for every
/// [`DaemonConfig`] shape this file uses. Keeps draining stdout for the
/// whole lifetime of the returned thread (not just until the ready line),
/// matching `clients/go/integration/integration_test.go`'s
/// `startDaemonOn` comment: `Child::wait()` only closes the pipe after the
/// process exits, so an un-drained pipe can fill its OS buffer and wedge
/// the daemon on a later stdout write.
fn wait_for_daemon_ready(stdout: std::process::ChildStdout) {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if !sent && line.contains("tymuxd listening") {
                sent = true;
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("tymuxd did not report listening within 10s");
}

/// Spawns a real `tymuxd` subprocess per `cfg` and blocks until it reports
/// ready. No bearer token is ever configured — every test in this file
/// binds `tymuxd` loopback-only (`127.0.0.1:...`), which never requires one
/// (`auth::check_non_loopback_requires_token`); UDS peer-cred auth (this
/// feature's actual subject) is orthogonal to that gate.
fn spawn_daemon(cfg: DaemonConfig) -> TestDaemon {
    let state_dir = unique_temp_dir("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket_dir = cfg
        .socket_path
        .parent()
        .expect("socket path must have a parent directory")
        .to_path_buf();

    let mut command = Command::new(workspace_bin("tymuxd"));
    command
        .env("TYMUXD_ADDR", cfg.tcp_addr)
        .env("XDG_STATE_HOME", &state_dir)
        .env("TYMUXD_SOCKET_PATH", cfg.socket_path)
        // Must stay at (at least) info level, not warn — the "tymuxd
        // listening" readiness line `wait_for_daemon_ready` waits on is
        // logged via `tracing::info!`, and `tymuxd`'s own `EnvFilter`
        // honors this var, overriding its "info" default (confirmed
        // empirically: `RUST_LOG=warn` here made the daemon never emit the
        // ready line at all, hanging every test in this file).
        .env("RUST_LOG", "info")
        .env_remove("TYMUXD_TOKEN")
        .env_remove("TYMUXD_SOCKET_GROUP")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if cfg.disable_tcp {
        command.env("TYMUXD_DISABLE_TCP_LOOPBACK", "1");
    } else {
        command.env_remove("TYMUXD_DISABLE_TCP_LOOPBACK");
    }

    let mut child = command.spawn().expect("failed to spawn tymuxd binary");
    let stdout = child.stdout.take().expect("tymuxd stdout should be piped");
    wait_for_daemon_ready(stdout);

    TestDaemon {
        child,
        state_dir,
        socket_dir,
    }
}

/// Dials a real UDS peer directly — the same `tower::service_fn` +
/// `hyper_util::rt::TokioIo` connector shape as `tymux-cli/src/main.rs`'s
/// own (private) `dial_uds`, duplicated here only because it's private to
/// that binary and this is a separate test crate. Used only to seed test
/// data (`seed_session`) straight through the daemon's real UDS listener —
/// never as the thing under test; the actual `tymux ls` proof always goes
/// through a real `tymux` subprocess.
async fn connect_uds_channel(socket_path: &Path) -> tonic::transport::Channel {
    let path = socket_path.to_path_buf();
    let connector = tower::service_fn(move |_: tonic::transport::Uri| {
        let path = path.clone();
        async move {
            let stream = tokio::net::UnixStream::connect(&path).await?;
            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
        }
    });
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await
        .expect("connect to the daemon's real UDS socket to seed test data")
}

/// Creates one session (a real `/bin/sh` pane) directly against the
/// daemon's UDS listener, so the CLI-side test proves the RPC round-trips
/// through the real dialed connection rather than just "the daemon started
/// and exited 0 with no sessions."
async fn seed_session(socket_path: &Path, name: &str) {
    let channel = connect_uds_channel(socket_path).await;
    let mut client = TymuxServiceClient::new(channel);
    client
        .create_session(CreateSessionRequest {
            name: name.to_string(),
            command: "/bin/sh".to_string(),
            cwd: String::new(),
        })
        .await
        .expect("CreateSession should succeed while seeding test data");
}

/// Runs a real `tymux ls` subprocess with `TYMUXD_SOCKET_PATH` pointed at
/// `client_socket_path` and no `--addr` — the exact "UDS-first, fall back
/// to TCP" path `dial_channel` implements (`tymux-cli/src/main.rs`).
fn run_tymux_ls(client_socket_path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tymux"))
        .arg("ls")
        .env("TYMUXD_SOCKET_PATH", client_socket_path)
        .env_remove("TYMUXD_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run `tymux ls`")
}

/// Task 6.4.1b (AC1): a `tymuxd` started with `--disable-tcp-loopback` (via
/// `TYMUXD_DISABLE_TCP_LOOPBACK=1`) never binds TCP at all
/// (`crates/tymuxd/src/main.rs`'s `tcp_future` short-circuits before ever
/// calling `serve_with_shutdown` when `tcp_disabled`) — so a `tymux ls`
/// with a matching `TYMUXD_SOCKET_PATH` and no `--addr` can only succeed by
/// actually dialing UDS.
#[tokio::test]
async fn tymux_ls_succeeds_via_uds_when_tcp_disabled() {
    let socket_path = unique_temp_dir("succeeds-uds").join("s.sock");
    let _daemon = spawn_daemon(DaemonConfig {
        tcp_addr: "127.0.0.1:0",
        socket_path: &socket_path,
        disable_tcp: true,
    });

    seed_session(&socket_path, "uds-only-session").await;

    let output = run_tymux_ls(&socket_path);

    assert!(
        output.status.success(),
        "tymux ls should succeed via UDS when TCP is disabled; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("uds-only-session [live]"),
        "tymux ls should list the seeded session, got stdout: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("falling back to TCP loopback"),
        "tymux ls must not fall back to TCP when UDS is reachable, got stderr: {stderr}"
    );
}

/// Task 6.4.1b (AC2): a `tymux ls` whose `TYMUXD_SOCKET_PATH` points at a
/// path nothing is bound to falls back to the hardcoded TCP loopback
/// address, even though the daemon's own (real, healthy) UDS listener is
/// reachable elsewhere — the mismatch is deliberate, forcing the fallback
/// branch. `dial_channel`'s TCP fallback dials a fixed
/// `http://127.0.0.1:7419` (deliberately not configurable — see plan.md's
/// Unresolved Questions), so this test binds that exact port itself
/// (mirroring `clients/ts/test/integration.test.ts`'s identical test) and
/// skips gracefully if something else on the machine already holds it,
/// rather than failing on an environmental conflict outside this test's
/// control.
#[tokio::test]
async fn tymux_ls_falls_back_to_tcp_and_logs_notice_when_uds_unreachable() {
    match std::net::TcpListener::bind("127.0.0.1:7419") {
        Ok(probe) => drop(probe), // release the port immediately so tymuxd can bind it
        Err(_) => {
            eprintln!(
                "skipping: port 127.0.0.1:7419 (tymux-cli's fixed, non-configurable TCP \
                 fallback address) is already in use on this machine — cannot exercise the \
                 fixed-address fallback path"
            );
            return;
        }
    }

    let real_socket_path = unique_temp_dir("fallback-real").join("s.sock");
    let _daemon = spawn_daemon(DaemonConfig {
        tcp_addr: "127.0.0.1:7419",
        socket_path: &real_socket_path,
        disable_tcp: false,
    });

    seed_session(&real_socket_path, "tcp-fallback-session").await;

    // Nothing is bound at this path — the daemon's real UDS socket is
    // real_socket_path above, not this one.
    let missing_socket_path = unique_temp_dir("fallback-missing").join("does-not-exist.sock");

    let output = run_tymux_ls(&missing_socket_path);

    assert!(
        output.status.success(),
        "tymux ls should still succeed via the TCP fallback; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("tcp-fallback-session [live]"),
        "tymux ls should list the seeded session via the TCP fallback, got stdout: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("falling back to TCP loopback"),
        "stderr should contain the fallback notice, got: {stderr}"
    );
}

/// Resolves a real, different uid to run the client subprocess as — the
/// `nobody` account exists on essentially every Unix-like system and is
/// never `tymuxd`'s own uid in any sane deployment. Mirrors
/// `crates/tymuxd/src/auth.rs`'s `resolve_gid_by_name`, one level down
/// (`getpwnam` vs. `getgrnam`).
fn resolve_uid_by_name(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let pwd = unsafe { libc::getpwnam(cname.as_ptr()) };
    if pwd.is_null() {
        None
    } else {
        Some(unsafe { (*pwd).pw_uid })
    }
}

/// Task 6.4.1c — the true cross-uid `PermissionDenied` reject proof, run
/// against a *genuinely different real OS uid* (via
/// `std::os::unix::process::CommandExt::uid`), not a synthetic `UCred` in a
/// unit test. Per pitfalls.md §7 and plan.md's Unresolved Questions (
/// resolved during planning): spawning a subprocess under a different real
/// uid requires `CAP_SETUID`/root, and this repo's actual CI
/// (`.github/workflows/ci.yml`) runs on plain `ubuntu-latest`/
/// `macos-latest` with no such privilege — so this test ships `#[ignore]`
/// from day one, runnable manually (as root) or in a future root-capable CI
/// job, exactly as the plan specifies. The accepted substitute proof of the
/// underlying decision logic is `crates/tymuxd/src/auth.rs`'s own
/// `peer_is_authorized` unit tests (Story 3.1.2), e.g.
/// `peer_is_authorized_rejects_different_uid_no_group_configured`.
///
/// No `--socket-group` is configured here (the common default), so on
/// Linux `bind_uds_listener` chmods the socket `0600` (owner-only) — a
/// genuinely different uid never reaches the gRPC-level `peer_is_authorized`
/// check at all; the kernel denies the `connect()` itself first. That's a
/// distinct code path from `dial_channel`'s explicit "gRPC-level
/// `PermissionDenied` status" branch (already covered as a same-uid,
/// chmod(0)-simulated case by `dial_channel_hard_errors_and_never_dials_tcp_when_uds_permission_denied`
/// in `src/main.rs`), but both converge on `friendly_message`'s identical
/// documented remedy text — which is exactly the black-box, end-to-end
/// behavior a real different uid must see.
#[test]
#[ignore = "requires CAP_SETUID/root to spawn a subprocess under a different real uid; \
            this repo's actual CI (ubuntu-latest/macos-latest) has neither — see plan.md's \
            Unresolved Questions (resolved) and Task 6.4.1c. Run manually as root."]
fn tymux_ls_is_rejected_with_permission_denied_remedy_when_client_uid_differs_from_daemons() {
    use std::os::unix::process::CommandExt;

    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "skipping: requires root/CAP_SETUID to spawn `tymux ls` under a different real uid"
        );
        return;
    }

    let daemon_uid = unsafe { libc::getuid() };
    let target_uid =
        resolve_uid_by_name("nobody").expect("the `nobody` account should exist on this system");
    assert_ne!(
        target_uid, daemon_uid,
        "test setup bug: need a real uid genuinely different from the daemon's own"
    );

    let socket_path = unique_temp_dir("cross-uid-reject").join("s.sock");
    let _daemon = spawn_daemon(DaemonConfig {
        tcp_addr: "127.0.0.1:0",
        socket_path: &socket_path,
        disable_tcp: true,
    });

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tymux"));
    cmd.arg("ls")
        .env("TYMUXD_SOCKET_PATH", &socket_path)
        .env_remove("TYMUXD_TOKEN")
        .uid(target_uid)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd
        .output()
        .expect("failed to run `tymux ls` under a different uid");

    assert!(
        !output.status.success(),
        "a client with a genuinely different real uid must be rejected, not succeed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not authorized to access this daemon's socket"),
        "stderr should contain the documented PermissionDenied remedy text, got: {stderr}"
    );
}
