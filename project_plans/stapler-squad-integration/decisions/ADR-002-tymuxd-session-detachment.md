# ADR-002: `tymuxd` calls `setsid()` at startup to fix abrupt-disconnect pane death

**Status**: Proposed (contingent on Story 1.1.1's real-hardware confirmation)
**Date**: 2026-08-21
**Context**: stapler-squad-integration, Phase 3 planning

## Context

`crates/tymux-e2e/tests/disconnect_survival_e2e.rs`'s `pane_survives_abrupt_disconnect` (`#[ignore]`d) documents a 100%-reproducible bug: closing a client's own pty master (a genuine OS-level tty hangup) kills the pane's child process, even though every explicit `Pane::kill()` call site is RPC-only (`Engine::kill_session`/`Engine::close_pane`) and neither fires on stream teardown (`main.rs:480-541`, grep-confirmed). The pane's own reader thread observes a genuine `Ok(0)` EOF on its own, distinct pty within 1-3ms of the client's pty closing.

Real tmux's server never has this failure mode because the server process itself is fully session-detached at startup — it has no controlling terminal to lose, so no client's tty hangup can reach it via kernel job-control signal delivery. `tymuxd` has no code anywhere that calls `setsid()` or otherwise ensures it isn't a member of a session tied to whatever terminal launched it (zero hits grepping `pane.rs`/`main.rs` for SIGHUP/session-detachment logic).

Two investigation passes already ruled out every code-level cause at the Rust/gRPC application layer. What remains unruled-out, and is the highest-value untested lead two independent research passes converged on: whether `tymuxd` itself retains a controlling terminal that a hangup can propagate through via process-group/session membership.

## Decision

Call `libc::setsid()` at the very start of `tymuxd`'s `main()`, before any pty is opened, tolerating the "already a session leader" (`EPERM`) case as expected (e.g. under a supervisor that already detached it).

Contingent on Story 1.1.1's real-hardware investigation confirming the hypothesis. If it finds a different mechanism, this ADR's fix is superseded.

## Rationale

- Matches real tmux's own defense exactly — a daemon with no controlling terminal cannot receive a hangup signal through kernel job-control.
- Additive: per-pane pty isolation (`pane.rs:163-242`) is already correct and unchanged; this targets the daemon-process level specifically.
- Cheap, low-risk, easy to revert if evidence points elsewhere.

## Alternatives Considered

- Per-pane `setpgid` isolation only, leaving `tymuxd` unfixed — rejected: confirmed-distinct-pty-device evidence already rules out the per-pane pty as the propagation path.
- Full double-fork daemonize — not adopted first: changes stdout/stderr/cwd lifecycle beyond what this bug motivates; revisit only if `setsid()` alone proves insufficient.

## Consequences

- Requires a new `libc` workspace dependency (minimal, stable, no licensing/security concern).
- If not confirmed on real hardware, superseded — tracked in plan.md's Unresolved Questions.
