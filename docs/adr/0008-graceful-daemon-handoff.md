# ADR 0008: Graceful daemon handoff by in-place re-exec

**Status**: Accepted by the owner 2026-11-04 (D1 re-exec with drain fallback, D2 lossy bootstrap, D3 unplanned death is not a requirement, D4 unlinked state fd with a 0600 debug dump flag, D5 no separate launchd job, D6 new `AttachEvent.reconnect` arm). Full evaluation, evidence and plan: [`project_plans/graceful-handoff/design.md`](../../project_plans/graceful-handoff/design.md). tmux prior art and the parity/improvement requirements: design.md section 12.

## Context

Replacing the `tymuxd` process loses every live pane. The daemon owns each pty master (`crates/tymux-core/src/pane.rs:1097-1158`); when it exits the masters close and the kernel sends SIGHUP to each pane's session. A new daemon cannot adopt anything because no fd or pid is persisted (`crates/tymux-core/src/engine.rs:896-918`, `crates/tymux-core/src/persistence.rs:20-40`) and it restarts with `restored dead-flagged sessions` / `possible orphaned processes` (`crates/tymuxd/src/main.rs:2059-2067`).

## Decision

1. Add an in-place re-exec handoff. A supervisor calls a new `PrepareHandoff` RPC with an absolute path to the new binary. The old daemon validates it, preflights it, freezes mutating RPCs and pane readers, writes its state to an unlinked inherited fd, clears `FD_CLOEXEC` on one master fd per pane, both listeners and the socket lock file, and `execve`s the new binary with the same pid. The new image adopts the fds, restores screen/replay state and `output_seq`, and commits. Attached clients are told to reconnect and resume (`resume_from_seq`).
2. Failure before commit rolls back by `exec`ing a retained copy of the previous binary with the same fds (one attempt).
3. Advertise the capability `graceful_handoff` in `GetServerInfo`. Supervisors gate on it, never on a version compare (ADR 0003).
4. A daemon without the capability, or a refused preflight, falls back to drain-only policy: the supervisor never kills or replaces a daemon that has live panes.
5. Proto changes are additive; no `PROTOCOL_VERSION` bump.

6. A handoff to the same version (re-exec the same or a rebuilt binary to pick up config, fd-limit or state changes) is supported (owner decision, 2026-11-05). The proof of completion is `ServerInfo.handoff_generation` advancing on the same pid, not a version change; supervisors stay fail-closed for same-version requests until a daemon advertises the counter.

## Why this option

It keeps the child pid, the parent/child relation (so `waitpid` exit codes and ADR 0007 restart policy keep working), and the listener backlog, with no extra process. Verified on macOS arm64 by a prototype: the master survives exec only when CLOEXEC is cleared, the child never sees SIGHUP, output buffered during the gap arrives gapless, and a client connected during the gap is accepted from the inherited listener (design.md section 9).

## Rejected

- Side-by-side daemons with SCM_RIGHTS: the new daemon is not the children's parent, so exit codes are lost.
- Per-pane shim processes: survives crashes but costs a process and a permanent wire protocol per pane; deferred until crash survival is a stated requirement.
- Re-adopting orphans by persisted pid: impossible, a closed pty master cannot be reattached and SIGHUP has already been delivered.

## Consequences

- Unplanned daemon death (SIGKILL, crash) is still lossy; supervisor-initiated kills (`StopTymuxd`, `Pdeathsig` on Linux, monitor respawn) must change in stapler-squad or they undo a successful handoff (design.md section 6.3).
- **Accepted (D2):** the first rollout onto a handoff-capable daemon is lossy, because the running daemon has no `PrepareHandoff`. No idle-window gating or special bootstrap path is built; every later rollout is graceful.
- tmux has no live-upgrade mechanism (design.md section 12), so this is "better than tmux" territory with no prior art to copy. It also means the tmux-parity guarantees (a live daemon is never killed by a failed probe or a supervisor restart) are a prerequisite, not a follow-up, and ship first (design.md section 8, Phase 1).
- A new internal, versioned handoff state format must be maintained alongside `PersistedSessionRecord`.
