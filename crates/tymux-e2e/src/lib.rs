//! Shared support for end-to-end tests that drive the real `tymuxd` and
//! `tymux` binaries — not mocks, not in-process calls. Two pieces:
//!
//! - [`daemon`]: spawns a real `tymuxd` subprocess on an ephemeral port,
//!   mirroring the pattern already used in `crates/tymuxd/tests/*.rs`.
//! - [`harness`]: spawns the real `tymux` CLI under a genuine pseudo-tty
//!   (the same `portable-pty` machinery `tymux-core::Pane` uses for its own
//!   shell panes) and keeps a `vt100::Parser` fed with its output, so tests
//!   assert on rendered screen content instead of scraping raw ANSI bytes.
//!
//! [`golden`] renders a `vt100::Screen` to normalized plain text for
//! `insta` snapshot regression tests — see `tests/golden_snapshot_e2e.rs`.

pub mod daemon;
pub mod golden;
pub mod harness;

/// Locates a workspace-built binary (`tymuxd`, `tymux`) by its name.
///
/// This crate deliberately does NOT depend on `tymuxd`/`tymux-cli` as Rust
/// dependencies to get Cargo's usual `CARGO_BIN_EXE_<name>` mechanism —
/// that only works when the binary-owning package has a `[lib]` target,
/// which neither does (Cargo silently drops a lib-less path dependency,
/// so the env var never gets set; confirmed by trying it first). Cargo
/// still places every workspace binary in the same `target/<profile>/`
/// directory regardless of which crate defines it, so this resolves the
/// path relative to the running test binary's own location instead.
///
/// Requires the binary to already be built — `cargo test --workspace`
/// only builds this crate's own dependency graph, so CI (and any local
/// run) must `cargo build --workspace` first. Panics with a clear message
/// if the binary isn't where expected, rather than failing confusingly
/// later when spawning it.
pub fn workspace_bin(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current test exe path");
    let deps_dir = exe.parent().expect("test exe has a parent dir");
    // Integration/unit test binaries build into target/<profile>/deps/;
    // the workspace's own binary targets land one level up.
    let profile_dir = deps_dir.parent().expect("deps dir has a parent dir");
    let candidate = profile_dir.join(name);
    assert!(
        candidate.exists(),
        "expected workspace binary at {candidate:?} — run `cargo build --workspace` first"
    );
    candidate
}
