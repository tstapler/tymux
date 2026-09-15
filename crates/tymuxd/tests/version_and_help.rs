//! #49 — `tymuxd --version`/`--help` must print and exit 0 without ever
//! binding a port or touching persisted state, unlike every other argv the
//! daemon accepts. `TYMUXD_ADDR` is deliberately pointed at an address this
//! process can't bind (privileged port 1) so a run that regressed into
//! actually starting the daemon would fail loudly instead of silently
//! passing on a lucky bind.

use std::process::Command;

#[test]
fn version_and_help_flags_exit_zero_without_starting_the_daemon() {
    for flag in ["--version", "-V", "--help", "-h"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tymuxd"))
            .arg(flag)
            .env("TYMUXD_ADDR", "127.0.0.1:1")
            .output()
            .unwrap_or_else(|e| panic!("failed to run tymuxd {flag}: {e}"));

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
