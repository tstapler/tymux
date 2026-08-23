# Requirements: stapler-squad-integration

**Date**: 2026-08-21
**Type**: feature addition (multi-epic, cross-repo, existing projects)
**Complexity**: 3 — system design
*(Briefly raised to 4 mid-research over the 1,000-concurrent-session
target, then confirmed back to 3: an empirical load test — not an
estimate — found tymuxd's thread-per-pane model handles 1,000 sessions
fine on resources (1,025 threads, 3,011 fds, 132 MB RSS, linear, no leaks).
The real risk found is a targeted, fixable O(n) lock-contention bug, not a
need to rearchitect the daemon — see Non-functional Requirements and
Feasibility Risks.)*

## Problem Statement
stapler-squad (a web dashboard for running multiple AI coding agents — Claude
Code, Codex, Gemini, Aider — concurrently, at `~/Programming/stapler-squad`)
uses tmux as its process/terminal backend today, and that dependency is
heavy: it vendors the full tmux 3.4 C source as a git submodule for
single-binary distribution, wraps it with a 2,800-line control-mode client
(`session/tmux/tmux.go`, `session/tmux/control_mode.go`), and runs
zombie/orphan-process reaping (`session/tmux/zombie_detector.go`,
`fork_metrics.go`) to manage subprocess pressure from spawning one tmux
session per agent. tymux (this repo) is a purpose-built daemon with a typed
gRPC API and no per-session subprocess forking — it exists specifically to
replace this. The gap: nothing in stapler-squad references tymux today (zero
hits in source/docs/tests), and tymux itself is missing capabilities
stapler-squad's current `ProcessManager` interface relies on.

## Baseline
Today, stapler-squad's `session/backend_factory.go` selects a
`ProcessManagerBackend` (`BackendTmux` or `BackendNative`); the tmux path
shells out to `new-session`/`has-session`/`kill-session`/`list-sessions`/
`list-panes`/`set-option`/`display-message`, opens `attach-session -C`
(control mode) for real-time streaming, sends input via `send-keys` over
control mode, and captures full-screen ANSI snapshots via
`capture-pane -p -e -J`. Output renders directly into per-session xterm.js
terminals in the browser. A prior snapshot-polling design (ADR-002,
superseded) was abandoned because it fought xterm.js — scrollback breakage,
disruptive full clears, buggy diff/apply logic — and stapler-squad reverted
toward raw PTY byte streaming, which is architecturally what tymux's
`Attach` RPC already provides. Every session today costs stapler-squad a
forked tmux subprocess plus the reaping logic to clean up after it.

## Users / Consumers
- stapler-squad's Go backend (`session/tmux/*`, `session/process_manager.go`)
  — the actual gRPC client; the browser never talks to tymux directly
- stapler-squad's web frontend (xterm.js) — indirect consumer, via the Go
  backend's existing output-streaming path to the browser
- tymux's own existing users (interactive CLI users, other gRPC clients) —
  must not regress

## Success Metrics
- A `BackendTymux` implementation of stapler-squad's `ProcessManager`
  interface (`session/process_manager.go`) exists and is selectable via the
  existing `ProcessManagerBackend` mechanism, alongside `BackendTmux`
  (unchanged, still the default)
- At least one real agent type (e.g. Claude Code) can run a full session
  end-to-end through `BackendTymux` — start, live output in xterm.js, input
  injection, capture, clean exit — with output fidelity indistinguishable
  from the `BackendTmux` path in manual side-by-side testing
- A pane survives an abrupt client disconnect (browser tab closed, network
  drop) and is reattachable with the agent process still running — verified
  with a passing, non-flaky version of `disconnect_survival_e2e` (currently
  blocked on an unresolved bug) run on real hardware, not just the sandboxed
  dev container where the prior investigation dead-ended
- Session/pane exit status (not just live/dead) is queryable through the
  gRPC API and surfaced by `BackendTymux`, closing the gap where tmux's
  `display-message`-based PID/exit tracking has no tymux equivalent today
- No change to `BackendTmux`'s behavior or stapler-squad's tmux submodule —
  this is additive, not a cutover
- tymuxd sustains 1,000 concurrent sessions under a load test without
  exhausting OS threads/file descriptors or degrading per-session latency
  unacceptably (exact acceptance threshold is a planning decision, informed
  by a first real load-test measurement rather than assumed up front)

## Appetite
Large (3–6 weeks)
*(Cross-repo: tymux proto/daemon feature work plus a stapler-squad
`BackendTymux` implementation. Scope must fit the appetite — cut epics, not
the timeline, if it doesn't.)*

## Constraints
- Two separate git repositories (`~/Programming/tymux`,
  `~/Programming/stapler-squad`) — no submodule/monorepo relationship
  between them; implementation work in Phase 5 must operate across both
  checkouts explicitly (agents need both absolute paths)
- Must preserve stapler-squad's existing `ProcessManagerBackend` interface
  shape (`session/process_manager.go`) rather than redesigning it —
  `BackendTymux` implements the existing contract; interface changes are
  out of scope unless the contract itself proves unable to express something
  tymux needs (a rabbit hole to flag, not assume)
- Must not regress tymux's existing behavior for its other consumers (CLI,
  TS client, other gRPC callers) or stapler-squad's `BackendTmux` path
- Solo developer, side-project pace across two repos — sequence epics so
  each repo stays independently buildable/shippable after every merge

## Non-functional Requirements
- **Performance SLO**: not formally specified — must feel responsive enough
  for interactive agent sessions (comparable latency to the current
  tmux control-mode path); no numeric target
- **Scalability**: target is **1,000 concurrent sessions** (user-specified,
  2026-08-21). Empirically load-tested during Phase 2 research (built
  release `tymuxd`, drove it through 1,000 real `CreateSession` calls via
  the TS client, sampled `/proc/<pid>/status`/`fd`):
  1,025 threads, 3,011 fds, 132 MB RSS at n=1,000 — linear growth, no
  leaks, well within default Linux limits (worth confirming
  `/proc/sys/kernel/pty/max`, typically 4096 system-wide, on the real
  deployment host). Resource footprint is a non-issue. The confirmed real
  bottleneck: `TymuxDaemon::create_session`
  (`crates/tymuxd/src/main.rs:222-226`) calls `Engine::list_sessions()` —
  an O(n) full-snapshot rebuild under both global session/pane
  `Mutex<HashMap>`s — just to return the one session it created.
  Measured: `CreateSession` latency climbed 5ms→20ms as session count went
  100→900. This part is a targeted, fixable lock-contention/query-shape
  bug, not evidence the thread-per-pane model itself needs rework — but a
  burst of *concurrent* RPCs at n≈1,000 (e.g. mass client reconnect) was
  inferred, not itself load-tested, to risk multi-second tail latency
  behind the shared locks. See
  `project_plans/stapler-squad-integration/research/scale-feasibility.md`
  for full findings, line citations, and a concrete concurrent-load-test
  plan
- **Security classification**: internal/local — both tymuxd and
  stapler-squad's backend are expected to run on the same host; tymux's
  existing loopback-only trust model is not being changed by this project
- **Data residency**: not applicable — local-only, same as both projects
  today

## Scope
### In Scope
- tymux-side feature work needed for `ProcessManager` parity: exit-code /
  process-exit-status surfaced through the gRPC API (currently only a
  live/dead `Liveness` enum exists); whatever else research surfaces as a
  genuine capability gap against `session/process_manager.go`'s method set
  (`Start`, `GetPTY`, `SendKeys`, `CapturePaneContent*`,
  `GetCursorPosition`, `GetPaneDimensions`, `SendInputViaControlMode`)
- Fixing the abrupt-disconnect pane-kill bug
  (`crates/tymux-e2e/tests/disconnect_survival_e2e.rs`, previously
  investigated to a dead end in this sandboxed container) — must-fix,
  load-bearing for the whole integration's value proposition
- A `BackendTymux` implementation in stapler-squad
  (`~/Programming/stapler-squad`) satisfying the existing `ProcessManager`
  interface, selectable via `backend_factory.go` alongside `BackendTmux`
- A rendering adapter converting tymux's structured `CapturePane`/`Attach`
  cell/byte output into whatever xterm.js needs (research to determine
  whether raw `Attach` output bytes can feed xterm.js directly, or whether
  a cell-grid → ANSI/xterm-API translation layer is needed)
- Validation with at least one real agent type end-to-end (Claude Code
  suggested, given it's stapler-squad's primary agent today)
- Fixing `create_session`'s O(n) full-session-scan under global locks
  (`crates/tymuxd/src/main.rs:222-226`) so it doesn't degrade under a
  concurrent burst near the 1,000-session target; a concurrent (not just
  sequential) load test validating this fix and overall scalability

### Out of Scope
- Retiring tmux from stapler-squad — the vendored submodule, `BackendTmux`,
  and zombie-reaping code all stay; this is an additive backend option, not
  a migration cutover (revisit "full replacement" as a later project once
  `BackendTymux` has proven itself in production)
- Redesigning stapler-squad's `ProcessManagerBackend`/`ProcessManager`
  interface shape
- Multi-pane/split/window usage — stapler-squad only ever runs one pane per
  session today (no `split-window`/`new-window` usage found in research);
  this project doesn't need to exercise tymux's split/window features
- Auth/authorization changes to tymux — loopback-trust model stays as-is;
  remote/multi-host deployment is not a goal here
- Windows support (unchanged from tymux's existing scope)

## Rabbit Holes
- **Cell-grid → xterm.js rendering — RESOLVED by research**: `Attach`'s
  raw output bytes bypass tymux's vt100 parser entirely and carry full
  fidelity (truecolor, alt-screen, bracketed paste); stapler-squad's
  current tmux live-stream path already forwards raw bytes the same way,
  so no translation layer is needed for the live-render path. Only
  `CapturePane`'s structured `Cell` snapshot (used for the AI/debug path,
  not live rendering) would need a small cells→SGR serializer if consumed
  by stapler-squad — not required to start.
- **No priming snapshot on `Attach` — new finding from research**: unlike
  `WatchWindow`, `Attach` only streams *future* output on subscribe; a
  freshly (re)attached xterm.js instance would render blank until new
  output arrives. Planning must decide: tymuxd sends a snapshot first, or
  `BackendTymux` calls `CapturePane` before wiring up `Attach`.
- **Disconnect-survival bug**: research produced a strong, previously
  untested lead (two independent research agents converged on it
  independently) — tymuxd/the pane may not be `setsid()`-detached from the
  client's controlling terminal the way real tmux's server deliberately
  is; no SIGHUP handling exists. `disconnect_survival_e2e`'s
  `pane_survives_abrupt_disconnect` test is `#[ignore]`d because the pane
  currently dies on abrupt disconnect essentially every time, not as a
  rare edge case. Needs verification on real hardware
  (`ps -o pid,pgid,sid,tty` on tymuxd and the pane child at the moment of
  hangup is the recommended first check) — budget this as the first
  implementation task, not a side investigation.
- **Exit-code/process-metadata parity — design ready**: architecture
  research proposes turning `AttachEvent.exited` (bool) into an
  `ExitStatus{has_code, code}` message, captured via
  `portable_pty::Child::wait()` in the pane reader thread
  (`crates/tymux-core/src/pane.rs:217-241`, where the `Child` handle is
  held but never waited on today) and threaded through the single existing
  send site in `crates/tymuxd/src/main.rs:497-503`. No new RPC needed.
  Scope only exit status, not full `display-message`-style process
  metadata (PID, etc.) — tymux's `Pane` message has no OS PID field and
  none is needed for this project.
- **Concurrency at 1,000 sessions — NEW, the top risk from Phase 2**:
  tymuxd's current architecture (one OS thread per pane, a
  `broadcast::channel(1024)` per pane, a tokio task per attached client)
  has never been tested past a handful of sessions. Planning must include
  a load-test spike before committing to an implementation approach —
  this could range from "works as-is" to "the thread-per-pane model needs
  rework," and the plan shouldn't assume which without data.

## Alternatives Considered
- Full tmux replacement now (rejected — additive backend first, so tymux
  proves itself in production before tmux is retired)
- Scoping this to tymux-repo work only, leaving `BackendTymux` as separate
  future work (rejected — cross-repo scope was explicitly chosen so the
  integration is proven end-to-end, not just theoretically enabled)
- Keeping the snapshot-polling capture model stapler-squad already tried
  (rejected historically per ADR-002 — direct motivation for preferring
  tymux's streaming `Attach` model)

## Feasibility Risks
- The disconnect-survival bug was already investigated once without finding
  root cause in this sandbox — real risk it stays unresolved even with a
  real-hardware debugging pass (a strong new lead exists — see Rabbit
  Holes — but is unconfirmed), which would block the "agents keep running
  after you close the laptop" value proposition entirely
- **O(n) lock contention at scale**: `create_session`'s call into
  `Engine::list_sessions()` under global locks (`main.rs:222-226`,
  measured 5ms→20ms latency growth from 100→900 sessions) risks
  multi-second tail latency under a concurrent burst near 1,000 sessions
  (e.g. mass reconnect after a stapler-squad restart) — inferred from
  sequential measurement + code reading, not itself load-tested under
  concurrency. Fix is targeted (stop rebuilding a full snapshot for a
  single-session lookup/insert), not an architecture rework
- Two-repo coordination (no shared CI, no monorepo) means changes can drift
  out of sync between plan and implementation across sessions

## Observability Requirements
tymux side: extend existing `tracing` instrumentation to cover the
disconnect/reconnect path specifically (current gap: the prior
investigation's own dead end was partly because signal was too thin to
distinguish real causes from container artifacts) and any new exit-status
tracking. stapler-squad side: structured logs comparable to its existing
tmux-path logging so `BackendTymux` sessions are diagnosable the same way
`BackendTmux` sessions are today (e.g. session lifecycle, disconnect/
reattach events).

## Risk Control
The existing `ProcessManagerBackend` selector (`BackendTmux`/`BackendNative`
+ new `BackendTymux`) is itself the risk control — `BackendTymux` ships
opt-in/per-session-selectable, `BackendTmux` remains the default, so a
regression in the new path never affects existing sessions. No additional
feature-flag infrastructure needed beyond this existing mechanism.

## Open Questions
- ~~Can `Attach`'s raw output bytes feed xterm.js directly?~~ **Resolved**:
  yes, no translation layer needed for the live path (see Rabbit Holes)
- ~~What concurrent-session count does stapler-squad actually need?~~
  **Resolved**: 1,000 (user-specified, 2026-08-21)
- Root cause of the abrupt-disconnect pane-kill bug — narrowed to a strong,
  untested lead (process-group/`setsid` detachment); needs real-hardware
  verification, first implementation task, not resolved by this planning
  pass *(unresolved after Phase 2 research)*
- ~~Does tymuxd's thread-per-pane/broadcast-channel architecture sustain
  1,000 concurrent sessions?~~ **Resolved**: yes on resources (empirical
  load test, see Non-functional Requirements). **Still open**: does
  `create_session`'s O(n) lock contention degrade unacceptably under a
  *concurrent* burst near 1,000 sessions (only sequential creation was
  load-tested)? — needs a concurrent load test in Phase 4/5
  *(unresolved after Phase 2 research)*
- Exact shape of an exit-status field/RPC addition to `tymux.proto` — design
  proposed by research (`ExitStatus{has_code, code}` on `AttachEvent`), to
  be confirmed in planning
