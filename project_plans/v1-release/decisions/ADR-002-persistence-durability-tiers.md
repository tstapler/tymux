# ADR-002: Persistence durability contract — Tier 0 required, Tier 1 stretch, Tier 2 ruled out

## Status
Accepted

## Context
`requirements.md`'s Rabbit Holes and Feasibility Risks sections both flag
that "sessions survive a `tymuxd` restart" is ambiguous across a huge cost
range — from "remember session names exist" to "replay full scrollback and
resume the exact live pty state" — and explicitly asks Phase 3 planning to
pin down exactly what's preserved vs. lost, rather than leaving it as an
open-ended stretch goal.

`architecture.md` §2 and `features.md` §2 independently converge on the
same reframing: **real tmux does not survive its own server process dying
either.** `kill -9 <tmux-server-pid>` destroys every tmux session with zero
recovery — tmux's actual guarantee is client-disconnect resilience, which
tymux already has by virtue of being a client/server daemon. So "survive a
daemon restart" is a strictly *stronger* guarantee than tmux itself offers,
and needs its own honest ceiling, not an assumed one.

`features.md` §2 examined `tmux-resurrect` (the most mature prior art in
this exact space, in production for over a decade) in detail: it saves
session/window/pane topology, cwd, and command lines, and *relaunches* an
allow-listed set of programs on restore — it does not and cannot checkpoint
a live process's runtime state. This is independent validation that "relaunch
metadata, don't resume a live process" is the correct ceiling to design to,
not a corner tymux is uniquely cutting.

## Decision

Three explicit tiers, matching `features.md` §2's naming:

**Tier 0 — hard v1.0 requirement.** On every *structural* mutation (session
create/rename/kill; window create/close; pane split/close/resize — never on
pty output or keystrokes), atomically write a whole-session
`PersistedSessionRecord` (session id/name/created-at, every window's id/
name/`LayoutNode` shape+ratios, every pane's id/spawn command/cwd/rows/cols)
to `$XDG_STATE_HOME/tymux/sessions/<session_id>.json` via write-to-temp-file
+ `rename()` (atomic on the same filesystem, POSIX-guaranteed on both Linux
and macOS). On daemon startup, scan this directory and load every valid
record into `Engine`'s session map as **dead-flagged** (`PaneEntry::Dead`
for every leaf pane — see ADR-001) — no live pty exists for any of them yet.
A parse failure on one file logs `tracing::warn!` and skips that file only;
it must never be fatal to daemon startup (`architecture.md` §2,
`pitfalls.md` §2b).

**Tier 1 — explicit stretch goal, not a hard v1.0 requirement.**
Auto-relaunch of a dead pane's original command in a fresh pty at the
persisted cwd (mirrors `tmux-resurrect`'s core mechanism exactly, per
`features.md` §2), optionally passing a resume-capable identifier for
known interactive tools (the `tmux-assistant-resurrect` precedent —
`claude --resume <session-id>` — directly relevant given this project's own
AI-agent audience), plus persisting a bounded scrollback tail alongside
metadata so a revived pane isn't a blank slate. This is real, valuable,
proven-achievable work — but it is out of the v1.0 gate; it may land in a
later alpha tag if time allows, but v1.0.0 must not depend on it.

**Tier 2 — explicitly ruled out, not deferred, not a future stretch
goal.** True live-process resumption across a daemon restart (resuming an
in-flight `vim` session, a mid-build compiler process, etc., with its
runtime state intact) requires OS-level checkpointing (CRIU-class, Linux-
only, itself limited around certain fd/socket types, with no real macOS
equivalent). This is ruled out **permanently for this architecture**, not
just for v1 — `tmux-resurrect`, the most mature prior art in this space
(over a decade in production), never attempted it either, which is strong
independent confirmation this is the right ceiling rather than a corner
being cut under time pressure.

**Reconciliation is explicit and user-initiated, never automatic.** Loading
a dead-flagged session on startup does not respawn anything. A new
`tymux revive <session_id>` CLI command (backed by a new `ReviveSession`
RPC) respawns fresh ptys matching the persisted `LayoutNode` shape (same
tree, same split geometry, new shells) — auto-respawning on daemon start
was considered and rejected: silently re-running whatever a dead pane's
last command was (e.g. `vim` mid-edit) on every daemon restart is
surprising, not what an operator restarting the daemon for an upgrade
actually wants (`architecture.md` §2).

**Storage choice**: hand-rolled `serde` + `serde_json`, one file per
session, not an embedded database. See the Pattern Decisions table in
`implementation/plan.md` for the `redb`/`rusqlite`/`sled` comparison
(`build-vs-buy.md` §2, `stack.md` §2) — the summary: data volume is tiny,
write frequency is naturally low (structural mutations are interactive-rate,
not per-keystroke), and a human-readable JSON file is directly diagnosable,
which the Observability Requirements section of `requirements.md` explicitly
asks for.

## Consequences
- `PersistedSessionRecord` needs an explicit `schema_version: u32` field
  from the very first version shipped — any future schema change must
  either migrate old records or refuse to load them loudly
  (`tracing::error!` + skip, never a silent `unwrap()` panic or silent data
  loss). This directly closes the "silent deserialize failure" pattern
  `pitfalls.md` §2b flags as the same failure class as the original
  Ctrl-d hang.
- The daemon's shutdown path (`crates/tymuxd/src/main.rs`'s
  `shutdown_signal()`, currently commented "nothing to drain") must be
  extended to force a final flush of any pending persistence write before
  `serve_with_shutdown` returns.
- `tymux ls` / `ListSessions` must render a dead-restored session visibly
  differently from a live one (`ux.md` §4) — this is the concrete consumer
  of the `Liveness` proto field introduced in Epic 2.
- Every mutating `Engine` method that triggers a persistence write must
  snapshot state *under* its existing `Mutex<HashMap<...>>` lock and perform
  the actual file I/O *after* releasing it (`pitfalls.md` §2a) — never hold
  the lock across a blocking file write, which would reintroduce a
  hang-class bug of the same shape as the already-fixed Ctrl-d hang.

## Alternatives considered
- **`redb`** (pure-Rust embedded KV, stable format): viable fallback if
  write frequency or query needs grow later; not justified at this scale
  today (`build-vs-buy.md` §2, `stack.md` §2).
- **`rusqlite`**: rejected — relational/query features are pure overhead for
  "load everything at startup, save the whole thing on mutation," and it's
  the heaviest of the three candidates for this workload.
- **`sled`**: rejected outright — long-abandoned, never left beta, real
  file-format-stability concerns (`stack.md` §2, `build-vs-buy.md` §2).
- **Auto-respawn on daemon start (Tier 1 folded into Tier 0)**: rejected —
  surprising behavior change on every restart; explicit user action
  (`tymux revive`) preserves operator intent.
- **Attempting Tier 2 via CRIU on Linux only**: rejected — Linux-only,
  fragile around fds/sockets, no macOS story at all, and `requirements.md`'s
  own macOS support requirement makes an asymmetric persistence guarantee
  (works on Linux, not macOS) worse than a uniform, honest "not supported"
  answer on both platforms.
