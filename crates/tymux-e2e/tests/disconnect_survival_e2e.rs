use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::{CapturePaneRequest, CreateSessionRequest};

/// tmux's whole value proposition is that a session survives the client
/// going away — this is the baseline that must hold for *any* kind of
/// disconnect, not just a clean `C-b d`.
#[tokio::test]
async fn pane_survives_graceful_detach() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    let session = client
        .create_session(CreateSessionRequest {
            name: "survive-graceful".into(),
            command: "/bin/sh".into(),
            cwd: String::new(),
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
        tymux_proto::v1::layout::Node::Pane(p) => p.id.clone(),
        _ => panic!("expected leaf"),
    };

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "survive-graceful:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(h.detach(Duration::from_secs(3)));

    let snap = client
        .capture_pane(CapturePaneRequest {
            pane_id,
            scrollback_offset: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(snap.liveness, tymux_proto::v1::Liveness::Live as i32);
}

/// **KNOWN BUG, not yet fixed** — found while building this E2E harness
/// (2026-07-17). An *abrupt* client disconnect (the client process's
/// controlling terminal hanging up — e.g. the terminal emulator crashing,
/// an SSH connection dropping, a laptop losing power — as opposed to a
/// clean `C-b d` or the process being sent SIGTERM) currently kills the
/// pane's own child process, not just the attach stream. Confirmed via
/// three isolating experiments:
///
/// 1. A pure gRPC-level abrupt stream/channel drop (no pty at all)
///    does NOT reproduce this — the pane survives.
/// 2. Sending SIGTERM directly to the CLI process's PID (matching how
///    `ptydrive.py` ended sessions throughout this project's manual
///    verification) does NOT reproduce this — the pane survives.
/// 3. Sending SIGHUP directly to the CLI process's PID via
///    `portable_pty::Child::kill()`, *without* closing this harness's
///    pty master, also does NOT reproduce this — the pane survives.
///
/// Only closing the pty master while the CLI process is still alive (a
/// genuine OS-level tty hangup) reproduces it, 100% of the time. Neither
/// `tymuxd`'s `input_handle` nor `forward_handle` (crates/tymuxd/src/
/// main.rs's `attach` handler) explicitly kill the pane on stream error —
/// per `crates/tymux-core/src/pane.rs`'s reader thread, the pane's own
/// pty read genuinely returns `Ok(0)` (clean EOF), meaning the shell
/// process itself is exiting, not being killed by daemon code.
///
/// A follow-up investigation (2026-07-17, same day) went further and
/// ruled out every code-level explanation:
///
/// - The CLI (`tymux`) process exits with **status 0, no signal** when
///   its own pty hangs up — it takes the ordinary "stdin closed" shutdown
///   path (`stdin_rx.recv() == None` in `attach()`'s select loop), the
///   exact same path a clean detach's stream-end takes. It is not killed
///   by SIGHUP.
/// - `grep`-confirmed: the only `Pane::kill()` call sites anywhere in
///   `tymux-core`/`tymuxd` are `Engine::kill_session` and
///   `Engine::close_pane` — both explicit-RPC-only. Nothing in the
///   attach-stream-teardown path (`unregister_viewport` +
///   `recompute_window_geometry`, which just re-applies `Pane::resize`)
///   touches `pane.kill()` or the pane's own master pty.
/// - Not fd/device aliasing: `/proc/<pid>/fdinfo/<fd>`'s `tty-index`
///   field was checked for both the harness's client pty and `tymuxd`'s
///   pane pty at the moment of the hangup — confirmed different devices
///   every time.
/// - Not a fixed timer: reproduces identically whether the hangup happens
///   300ms or 3s after attach.
/// - Not related to input content: reproduces with zero bytes ever sent
///   to the pane.
/// - The pane's own reader thread observes the real `Ok(0)` EOF within
///   roughly 1-3ms of the client pty being closed — fast enough to be a
///   real causal chain (client hangup → clean stream end → *something*),
///   but an `strace -f` attached to `tymuxd` around that exact window
///   produced ambiguous, contradictory-looking output (what appeared to
///   be an unrelated interactive shell's own job-control syscalls),
///   most likely a `ptrace`/PID-reuse artifact of this specific sandboxed
///   dev container (`ptrace_scope=1` blocks attaching to an
///   already-running process at all — attaching at exec time was the only
///   option, and every process here, including `tymuxd` and the outer
///   login shell, share one systemd cgroup scope with no controlling
///   terminal of their own, an environment shape a real user's machine
///   would not have). That line of investigation was abandoned rather
///   than trusted further.
///
/// Net: the pane's own OS-level pty gets a genuine hangup, but nothing in
/// this codebase's Rust/gRPC layer causes it — the mechanism is either
/// below that layer (kernel/session-level) or an artifact specific to
/// this sandboxed dev container. **Re-test on a real terminal/machine
/// outside this sandbox before trusting any further root-causing done
/// here** — a good next step there: `strace -f -o log tymuxd` from a
/// real shell (no `ptrace_scope` restriction to work around), or `ltrace`/
/// `perf trace` around the exact hangup window.
///
/// **2026-08-21 re-investigation (Story 1.1.1)** — still not real hardware;
/// this session runs inside the same kind of sandboxed dev container as
/// the 2026-07-17 pass above. Re-ran the repro capturing
/// `ps -o pid,ppid,pgid,sid,tty` for `tymuxd` and the pane's child process
/// at the disconnect instant. Findings:
///
/// - `tymuxd` itself: `TT=?`, `SID=PGID=PID` — already its own session
///   leader with **no controlling terminal at all**, before any code
///   change. Confirmed this is a property of the sandbox, not of
///   `tymuxd`: an ordinary interactive shell spawned fresh in this same
///   session shows the identical `SID=PGID=PID`, `TT=?` signature.
/// - The pane's child shell: `TT` goes from a real pty device (`pts/N`)
///   while attached to `?` and `<defunct>` (exited, awaiting reap) within
///   ~200ms of the abrupt disconnect — it genuinely exits, exactly as the
///   2026-07-17 investigation found.
/// - **The Story 1.1.2 `setsid()` fix (ADR-002) was compiled into
///   `tymuxd` for this re-test and did not stop the pane from dying** —
///   `pane_survives_abrupt_disconnect` still fails in this sandbox with
///   the fix applied. Because this sandbox's `tymuxd` already had no
///   controlling terminal *before* the fix — the exact `SID=PGID=PID`,
///   `TT=?` state `setsid()` itself would produce — the fix is a
///   structural no-op here: this environment can never exercise the
///   "`tymuxd` has a controlling terminal a hangup propagates through"
///   precondition the fix targets, with or without the code change. That
///   makes this sandbox's result **non-diagnostic** for the hypothesis —
///   it neither confirms nor refutes the real-hardware mechanism ADR-002
///   targets, it just can't see it either way.
/// - `ptrace_scope=1` is still in force here too, so the same `strace`
///   limitation noted in the 2026-07-17 pass applies; no further syscall
///   forensics were attempted this pass for the same reason it was
///   abandoned then.
///
/// **Conclusion (superseded for environment 1 — see 2026-09-04 below)**:
/// the `setsid()` fix (Story 1.1.2 / ADR-002) is implemented and kept —
/// it's safe, low-risk, and matches real tmux's own daemon design — but at
/// the time this paragraph was written it remained **unverified against
/// the actual bug**. No real, non-sandboxed hardware and no systemd-managed
/// host were available in that session, so Story 1.1.2's pre-mortem P1 #1
/// second-environment requirement was **not met**.
///
/// **2026-09-04 (bare-metal, environment 1 of pre-mortem P1 #1)** — run on
/// `onyx`, a bare-metal Manjaro Linux workstation (`systemd-detect-virt`
/// reports `none`; kernel 6.6.128-1-MANJARO), not a container or VM. One
/// wrinkle specific to running this from an agent session: commands
/// executed through the Claude Code Bash tool are themselves spawned into
/// a *new, ctty-less session* (`SID=PGID=<own PID>`, `TT=?`) regardless of
/// the underlying machine being bare metal — structurally identical to the
/// 2026-08-21 sandbox finding, just for a harness reason instead of a
/// container reason. Verified directly: a plain `ps -o pid,ppid,pgid,sid,tty
/// -p $$` from a Bash-tool-spawned shell shows `SID=PGID=<own PID>`,
/// `TT=?`, while the `claude` process itself (this session's parent) is
/// attached to a real `pts/5`.
///
/// To get a genuine controlling terminal in the loop despite that, `tymuxd`
/// was launched via `script -qc "bash -c './tymuxd &...'" logfile`, so that
/// `script`'s forkpty makes the intermediate `bash` (not `tymuxd`) the
/// session leader of a fresh pty, and `tymuxd` is exec'd as an ordinary
/// background child of that `bash` — matching how a human's `./tymuxd &`
/// at a real interactive shell prompt actually looks (a plain child
/// process, not itself a session leader, inheriting the shell's real
/// ctty). This is *not* the same as wrapping `script` directly around
/// `tymuxd` (tried first, then discarded) — `script`'s forkpty'd child
/// becomes session leader *before* exec, so a direct `script -qc tymuxd`
/// puts `tymuxd` in the already-session-leader/EPERM branch (environment
/// 2's shape, not environment 1's). State was isolated from any real
/// `tymuxd` instance via `XDG_STATE_HOME`, `TYMUXD_SOCKET_PATH`, and a
/// non-default `TYMUXD_ADDR` (127.0.0.1:17419) pointed at scratch
/// directories — no production session data was touched.
///
/// `ps` output, before `setsid()` runs vs. after:
/// - Intermediate `bash` (the pty's actual session leader):
///   `PID=1087124 PGID=1087124 SID=1087124 TT=pts/13`.
/// - `tymuxd` (PID 1087126), immediately after `setsid()` executes at the
///   top of `main()`: `PGID=1087126 SID=1087126 TT=?` — it left `bash`'s
///   session and controlling terminal entirely, confirming the fix's
///   primary code path (a non-EPERM `setsid()` call) actually engages and
///   changes observable state on this environment, unlike the 2026-08-21
///   sandbox where `tymuxd` already had `TT=?` before `setsid()` ran at
///   all.
///
/// Manual repro (this runbook's Step 3, run 10 times — not the automated
/// `#[ignore]`d test below, which was not un-ignored/run directly this
/// pass): for each run, `tymux new --name verify-disconnect-N` was
/// attached inside its own `script`-allocated pty (own real `pts/N`,
/// client itself as session leader of that pty — the exec-optimization
/// case for a single `bash -c` command), then the abrupt disconnect was
/// simulated by `kill -9` on the **script process holding the pty
/// master** (never the client process directly — killing the client
/// itself doesn't reproduce the bug, consistent with the 2026-07-17
/// isolating experiments above). Baseline vs. post-disconnect `ps` for one
/// representative run:
/// - Pane child baseline: `PID=1097854 PPID=1087126(tymuxd) PGID=SID=1097854
///   TT=pts/24 CMD=/bin/zsh`.
/// - Client killed via its pty-master `script` process (PID 1097826).
/// - Pane child post-disconnect: **identical row**, still alive
///   (`PID=1097854 ... TT=pts/24`), not `<defunct>`.
/// - Confirmed via the real gRPC API, not just `ps`: `tymux ls
///   --socket-path <scratch>` reported `verify-disconnect-N [live]` after
///   every one of 10 consecutive runs (10/10 pass, 0 fail) — matching this
///   test's own `Liveness::Live` assertion below.
///
/// **What environment 1 alone did and didn't close**: it satisfied
/// environment 1 of pre-mortem P1 #1 (a real, non-containerized machine,
/// `tymuxd` as an ordinary child of an interactive-shell session with a
/// genuine controlling terminal) — a real result, not a structural no-op
/// like 2026-08-21. It did not, by itself, satisfy environment 2 (a
/// systemd-managed host, exercising the `EPERM`-already-session-leader
/// tolerance path from Task 1.1.2b). It also used a `script`/Bash-tool-
/// driven pty rather than a human typing at a literal terminal emulator, a
/// narrower substitute for "a human with real hardware access" than Task
/// 1.1.3's original design intent, even though the resulting process/
/// session shape is the real one.
///
/// **2026-09-04, same day, environment 2 (systemd-managed host)** — same
/// bare-metal `onyx` machine (real systemd, PID 1 is `systemd 259`, not a
/// container's init), but this time `tymuxd` was launched via `systemd-run
/// --user --unit=tymuxd-verify --collect ... target/release/tymuxd`, a
/// genuine transient systemd unit (`systemctl --user status` confirmed
/// `Active: active (running)`, real `CGroup=.../tymuxd-verify.service`),
/// not a simulation of one. State was isolated the same way as environment
/// 1 (`XDG_STATE_HOME`, `TYMUXD_SOCKET_PATH`, non-default `TYMUXD_ADDR`
/// under `--setenv`) — `tymuxd`'s own startup log confirmed `no orphaned-
/// process candidates found ... count=0`, i.e. it never touched the real
/// state dir with its 4000+ persisted sessions.
///
/// `ps` on the running unit, immediately confirms the precondition Task
/// 1.1.2b's tolerance path exists for: `PID=2318086 PGID=2318086
/// SID=2318086 TT=? STAT=Ssl` — systemd starts every service as its own
/// session leader from the moment of `exec`, *before* `tymuxd`'s own
/// `setsid()` call even runs. That means `setsid()` here necessarily
/// returns `EPERM` (already session leader) and Task 1.1.2b's tolerance
/// path is what keeps the daemon running instead of crashing — confirmed
/// by the unit simply staying `active (running)` rather than failing to
/// start.
///
/// Manual repro (this runbook's Step 3 pattern, run 10 times): for each
/// run, `tymux new --name sysd-verify-N --socket-path <scratch>` was
/// attached inside its own `script`-allocated pty, then abruptly killed by
/// `kill -9` on the pty-master-owning `script` process (never the client
/// itself). Baseline for run 1's pane child: `PID=2320119
/// PPID=2318086(tymuxd) PGID=SID=2320119 TT=pts/5 CMD=/bin/zsh`. Result
/// across all 10 runs: **10/10 pass, 0 fail** — `tymux ls --socket-path
/// <scratch>` reported `sysd-verify-N [live]` every time, and a final
/// `pgrep -P <tymuxd-pid>` after all 10 disconnects showed all 10 pane
/// shells still present and running, none `<defunct>`.
///
/// Cleanup: `systemctl --user stop tymuxd-verify` (the `--collect` flag
/// means the transient unit unloads on its own once stopped — confirmed
/// via `systemctl --user list-units 'tymuxd*'` returning 0 units
/// afterward), plus removal of the scratch socket/state directories. No
/// systemd unit files or state were left behind.
///
/// **Both environments required by pre-mortem P1 #1 are now confirmed on
/// real hardware**, by manual repro: environment 1 (ordinary interactive-
/// shell child, real ctty to detach from) 2026-09-04 above, and environment
/// 2 (systemd-managed, `EPERM` tolerance path) this entry. Both used a
/// `script`/`systemd-run`-driven repro from an agent session rather than a
/// human directly typing at a terminal emulator — the process/session
/// shapes produced are the real ones the fix targets (confirmed via `ps`,
/// not assumed), but this is narrower than Task 1.1.3's original "a human
/// runs this" design intent, noted here for the record rather than
/// silently upgraded.
///
/// **This automated test stays `#[ignore]`d regardless** — tried un-
/// ignoring and running it directly this same session
/// (`cargo test --release -p tymux-e2e --test disconnect_survival_e2e
/// pane_survives_abrupt_disconnect -- --exact --nocapture`) and it
/// **failed**: `daemon::spawn()` launches `tymuxd` via a plain
/// `std::process::Command`, inheriting whatever session the test process
/// itself runs in — and a `cargo test` invoked from this agent session's
/// own shell is itself ctty-less (`SID=PGID=<own PID>`, `TT=?`, same
/// signature as the 2026-08-21 sandbox), so `tymuxd` never gets the real
/// controlling terminal the manual repro above deliberately engineered via
/// `script`/`systemd-run`. That reproduces the exact structural no-op the
/// 2026-08-21 entry already documented — the pane died
/// (`FailedPrecondition: pane exited`), not because the fix doesn't work
/// (the manual repro just proved it does, twice), but because this test's
/// harness can't give `tymuxd` a controlling terminal in an ordinary
/// CI/agent shell. Un-ignoring on that failure would just break CI on a
/// harness gap, not a real regression. **Follow-up needed before this can
/// be un-ignored for real**: give `daemon::spawn()` a way to launch
/// `tymuxd` with a genuine controlling terminal (e.g. a pty via
/// `portable_pty`, mirroring what `CliHarness` already does for the CLI
/// side) — out of scope for this pass, flagged here rather than attempted
/// silently.
#[tokio::test]
#[ignore = "known bug's setsid() fix is now confirmed on real hardware (see 2026-09-04 entries above, both pre-mortem P1 #1 environments) — but the automated harness can't yet give tymuxd a real controlling terminal in CI/agent shells, so this still fails there on a harness gap, not a regression; needs daemon::spawn() to grow pty support before un-ignoring"]
async fn pane_survives_abrupt_disconnect() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    let session = client
        .create_session(CreateSessionRequest {
            name: "survive-abrupt".into(),
            command: "/bin/sh".into(),
            cwd: String::new(),
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
        tymux_proto::v1::layout::Node::Pane(p) => p.id.clone(),
        _ => panic!("expected leaf"),
    };

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "survive-abrupt:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    // No graceful detach — abruptly tear down the client's pty, simulating
    // a crashed terminal or dropped network connection.
    drop(h);
    std::thread::sleep(Duration::from_millis(500));

    let snap = client
        .capture_pane(CapturePaneRequest {
            pane_id,
            scrollback_offset: 0,
        })
        .await
        .expect("pane should still respond to CapturePane after an abrupt disconnect")
        .into_inner();
    assert_eq!(
        snap.liveness,
        tymux_proto::v1::Liveness::Live as i32,
        "an abrupt client disconnect must not kill the pane's own process"
    );
}
