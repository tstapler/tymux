# Research: Feature Landscape — stapler-squad-integration

Research Agent 2 (Features), SDD Phase 2. Scope: unstated needs a `BackendTymux`
must satisfy, relevant prior art in PTY-over-network tooling, failure modes to
design against, and what `zombie_detector.go`/`fork_metrics.go` are really
protecting that tymuxd must protect some other way.

All line numbers are current as of this research pass; `stapler-squad` paths
are relative to `~/Programming/stapler-squad`, `tymux` paths to `~/Programming/tymux`.

## 1. Edge cases in stapler-squad's current tmux `ProcessManager` — the unstated needs

The full interface (`session/process_manager.go:10-68`) is bigger than the
excerpt already known: it also has `RestoreWithWorkDir`, `GetCurrentWorkingDirectory`,
`GetPanePID` (OS pid — tymux's `Pane` has no PID field), `SetDetachedSize` /
`SetWindowSize` / `RefreshClient`, `HasUpdated`/`FilterBanners`/`HasMeaningfulContent`
(content-diffing helpers used for polling paths, not just streaming), and
`SetOnExitCallback`/`ResetExitOnce`. A `BackendTymux` has to decide, for each
method, whether it's satisfied by an existing tymux RPC, needs a new one, or
is a no-op (e.g. `RefreshClient`, which forces a tmux client redraw — probably
meaningless for a byte-stream backend).

Concrete edge cases worth preserving conceptually:

- **Session-existence caching with coalesced misses.** `DoesSessionExist()`
  (`session/tmux/tmux.go:2166-2219`) layers three things: a push-based
  registry fast path, a TTL'd atomic cache, and `singleflight`-coalesced
  concurrent cache misses so N simultaneous callers produce one subprocess
  call, not N. `DoesSessionExistNoCache()` (`tmux.go:2237+`) is a separate,
  no-TTL variant used only on critical paths (session creation, hibernation
  sweep) via its own singleflight group. A gRPC-backed `BackendTymux` gets
  the "one call, not N" property almost for free (it's just a `ListSessions`/
  `GetPane` RPC), but should still think about whether concurrent existence
  checks from multiple stapler-squad goroutines need coalescing to avoid
  hammering tymuxd, and whether a "just created, might not be visible yet"
  staleness window exists on the tymux side.
- **Server-not-running recovery.** `DoesSessionExist`'s slow path detects
  "server not running" specifically (`serverNotRunning(output)`,
  `tmux.go:2199`) and calls `recoverFromServerFailure` (`tmux.go:2089-2129`),
  which is itself de-duplicated so only one recovery attempt runs at a time
  across all sessions, resets circuit breakers, and recreates a keepalive
  session. The tymux analogue is "tymuxd is not running / not yet up" — see
  §3.
- **Pre-creation race between fast-exiting programs and session teardown.**
  `preconfigureServerBeforeSession()` (`tmux.go:1092-1093`, doc comment
  `1062-1091`) exists because a program that exits in microseconds (or a
  brand-new tmux server with no prior sessions) can have the session — and
  the whole server, since `exit-empty` defaults to on — destroyed before
  `remain-on-exit` gets set, racing the "capture exit status" path. It's
  fixed by chaining `start-server ; set-option exit-empty off ; set-option
  remain-on-exit on` into one tmux invocation so there's no gap. tymuxd,
  being a persistent daemon rather than an on-demand server, likely doesn't
  have the "server exits when empty" failure mode, but the underlying race —
  "can a pane's exit status be lost if the client (stapler-squad) hasn't
  started listening/reading yet" — is the same shape of bug and should be an
  explicit test case for whatever exit-status RPC is added.
- **Exit status is already solved on the tmux side, and is a narrow, well-
  scoped read.** `ExitStatus()` (`tmux.go:2799-2823`) reads
  `#{pane_dead_status}`/`#{pane_dead_signal}` (populated because
  `remain-on-exit` is set) and returns `ok=false` if the pane is alive, gone,
  or never had a dead state. Its doc comment flags a real time-bomb worth
  copying into tymux's design: **the dead pane's exit data is destroyed the
  instant anything kills/respawns it**, so callers must read it "as early as
  possible after detecting an exit." This directly constrains tymux's own
  exit-status RPC/field: if a killed pane's storage is reclaimed or reused
  before a client asks, the data is gone — tymux needs to either retain
  exit-status until explicitly acknowledged/queried once, or make the
  `exited`/status transition itself carry the code (already-known
  `AttachEvent.exited: bool` field, `proto/tymux/v1/tymux.proto:267`, has no
  code/signal — this is the concrete gap named in Success Metrics).
- **Control-mode command/response correlation is a FIFO queue over a single
  stdin/stdout pipe, with `%begin`/`%end`/`%error`/`%exit` framing**
  (`session/tmux/control_mode.go:371-521`). Two behaviors worth noting as
  "this is what a naive client misses": (a) an unsolicited `%exit` must drain
  every in-flight command with `ErrControlModeStopped` rather than leaving
  callers to time out (`control_mode.go:452-489`) — the same discipline
  applies to a gRPC `Attach` stream closing server-side: any pending
  RPCs/promises against that pane need fast, explicit cancellation, not a
  client-side timeout; (b) refcounted start/stop
  (`t.controlModeRefCount`, `control_mode.go:69-139`) — multiple
  stapler-squad subscribers to the same session share one underlying tmux
  process, only actually torn down on the last unsubscribe. tymux's own
  `Attach` is already multi-viewer per ADR-004, so this concern is mostly
  already handled tymux-side, but `BackendTymux` must not assume "my
  disconnect" means "the pane's stream is gone" if other stapler-squad
  viewers (or other tools) are attached to the same pane.
- **Slow-subscriber grace period, not instant-drop.**
  `controlModeSlowSubscriberGrace = 250ms` (`control_mode.go:42-51`):
  broadcasting output to a subscriber whose channel is momentarily full
  (e.g. burst of `%output` during fast typing/paste) waits up to 250ms before
  concluding the subscriber is actually dead, rather than treating the first
  full buffer as disconnection. A raw byte-stream `Attach` RPC could hit an
  equivalent problem if tymux applies backpressure by dropping a slow gRPC
  stream consumer — worth confirming tymux's flow-control policy under a
  bursty writer (e.g. `yes` or a fast build log) doesn't false-positive a
  live client as gone. This is distinct from `output_gap` (that's the
  broadcast-channel-lag/reconnect-catch-up case, already known); this is
  about not killing a *currently attached* slow reader.
- **Resize is dual-pathed and remembers the last size.** `SetWindowSize`
  (`tmux.go:2050-2087`) resizes both the local PTY *and* issues tmux
  `resize-window`, tries control-mode first and falls back to direct exec on
  failure, and stores `lastKnownCols/Rows` for future PTY attach connections.
  The "remember last known viewport per session so a fresh attach starts at
  the right size before the client's first resize message arrives" pattern
  is worth carrying into `BackendTymux`/tymux: if tymuxd's ADR-004 viewport
  tracker forgets a departed client's last size, a lone reattaching client
  might render into 0 or stale dimensions for one frame.
- **`GetPanePID` and `ExitStatus` both try control-mode first, fall back to
  direct exec on failure** (`tmux.go:2758-2791`, and `SetWindowSize` above) —
  a general "control channel degrades gracefully to a fallback path, never
  hard-fails the whole operation" pattern that doesn't have an obvious tymux
  analogue (tymux only has the one gRPC channel), but is worth naming as a
  reliability property stapler-squad currently gets for free and would lose:
  today, if tmux's control-mode process itself is wedged, individual RPC-like
  calls (get PID, resize) still work via a separate exec. With tymux, if the
  single gRPC connection to tymuxd is degraded, **every** operation degrades
  together. That's a legitimate regression risk worth flagging to the plan
  phase, not just accepting silently.
- **Working-directory validation is a first-class error, not a silent
  fallback.** `ErrWorkDirMissing`/`validateWorkDir` (`tmux.go:1038-1060`)
  explicitly reject an empty or since-deleted directory (e.g. a pruned git
  worktree) instead of silently falling back to `os.Getwd()` (which for a
  long-running daemon is often `$HOME` — a session created in the wrong
  directory is a confusing, hard-to-detect failure). `BackendTymux`'s
  `CreateSession`-equivalent call needs the same explicit validation before
  it ever reaches tymuxd, since tymux's `CreateSession` RPC almost certainly
  doesn't know stapler-squad's worktree semantics.

## 2. Prior art: PTY-over-network / structured-terminal-capture-as-a-service

- **ttyd / gotty / wetty (websocket PTY bridges).** All three are architecturally
  closest to tymux's `Attach` RPC (raw PTY bytes over a persistent connection to
  a browser terminal), but their disconnect handling is comparatively weak:
  gotty's `webtty` package exposes a `WithReconnect` option for the *master*
  (server) side; ttyd has a client-side `-r`/reconnect-timeout flag (default 10s)
  that just retries the websocket, not a resumed byte stream — an abnormal close
  can still surface even on a normal `exit 0` ([ttyd#109](https://github.com/tsl0922/ttyd/issues/109)),
  and idle connections have been reported dropping around a minute
  ([ttyd#445](https://github.com/tsl0922/ttyd/issues/445)). wetty has had
  reports of sessions closing when a browser tab is merely backgrounded
  ([wetty#361](https://github.com/butlerx/wetty/issues/361)). Takeaway for
  tymux: none of these tools solve "reattach and get exactly the output you
  missed" — they solve "reconnect the transport and let the underlying
  process's own buffering (or nothing) fill the gap." tymux's `output_gap`
  signal and `CapturePane` catch-up-snapshot approach is already a more
  principled answer to this than the prior art offers; that's a genuine
  design strength worth stating plainly rather than just matching precedent.
- **Eternal Terminal (et).** A real prior-art match for "survive an abrupt
  disconnect and reattach with byte-exact continuity": `BackedReader`/
  `BackedWriter` track a sequence number per direction and, on reconnect, the
  writer replays exactly the bytes the reader is missing from an encrypted
  ring buffer ([eternalterminal.dev/howitworks](https://eternalterminal.dev/howitworks/)).
  This is precisely the shape of tymux's own `output_gap`/scrollback-catch-up
  design already in place — validates that design pattern rather than
  suggesting a new one. One thing et has that's worth checking against tymux:
  et's auto-reconnect is client-initiated and transparent (no explicit
  reattach action) — for stapler-squad, whether `BackendTymux` should retry
  a dropped gRPC `Attach` stream transparently (client-side reconnect loop)
  rather than surfacing the drop to the UI is a real design choice, not
  automatically "yes."
- **mosh (SSP — State Synchronization Protocol).** Solves a different problem
  (high-latency/lossy links, IP roaming) with UDP + predictive local echo,
  which isn't relevant to tymux's LAN/localhost gRPC transport. The one
  transferable idea: SSP "can skip past intermediate screen states" instead
  of replaying every byte, because it synchronizes *state* rather than a byte
  stream (mosh-paper.pdf via usenix.org). tymux's raw-byte `Attach` stream is
  the opposite choice (replay bytes, not state) — reasonable for terminal
  fidelity, but means a client that fell far behind (huge output_gap) pays
  for every byte of catch-up rather than a coalesced current-state snapshot.
  Confirms the requirements doc's own rabbit hole (cell-grid vs. raw-byte
  rendering) is the right open question to resolve, not a solved one.
- **Zellij (client-server daemon mode).** Structurally the closest analogue to
  tymux itself: a daemonized server owns all PTYs/pane state; a client is a
  thin renderer that attaches/detaches over a Unix socket, and "when the
  client detaches, the server remains alive until the client connects again"
  ([DeepWiki: Client-Server Model](https://deepwiki.com/zellij-org/zellij/2.1-client-server-model)).
  This validates tymux's own core bet (daemon owns PTY lifecycle, client is
  disposable) but doesn't surface anything about *abrupt* (non-graceful)
  disconnect handling specifically — Zellij's docs describe intentional
  detach, not crash/network-drop survival, so it's not a source of a
  ready-made fix for the abrupt-disconnect bug.
- **Exit-code propagation**: none of ttyd/gotty/wetty's public docs describe a
  structured exit-code-over-the-wire contract distinct from the process just
  closing the socket — this looks like a genuine gap in the ecosystem, not
  something stapler-squad can adopt wholesale from prior art. tymux adding an
  explicit exit-code/signal field is ahead of, not behind, this class of tool.

## 3. Failure modes `BackendTymux` should explicitly design against

Beyond the already-known abrupt-disconnect pane-kill bug
(`crates/tymux-e2e/tests/disconnect_survival_e2e.rs`, most recently updated in
commit `ab88c81`, which ruled out every code-level cause in `tymux-core`/
`tymuxd` — no `Pane::kill()` call site fires outside `kill_session`/
`close_pane`, not fd/device aliasing, not timing- or input-dependent, and the
CLI itself exits 0/no-signal on the ordinary stdin-closed path — leaving the
mechanism either kernel/session-level or a sandbox artifact, to be re-run on
real hardware):

- **tymuxd not running yet when stapler-squad starts / restarts mid-session.**
  stapler-squad's tmux path has a whole recovery subsystem for "server not
  running" (`recoverFromServerFailure`, `tmux.go:2089-2129`) including
  de-duplicated concurrent recovery attempts and explicit logging that
  existing sessions are *not* auto-recreated post-recovery ("individual user
  sessions are NOT automatically re-created after recovery; they will be
  restarted on the next user interaction"). `BackendTymux` needs an explicit
  answer for: (a) tymuxd not yet started (stapler-squad boots before/without
  a running daemon — is there a supervised-start/systemd-unit expectation, or
  does `BackendTymux` need its own "start tymuxd if absent" logic analogous
  to `ensureServerRunning`?), and (b) tymuxd restarting mid-session (does a
  restart kill all panes, or do they survive a daemon restart the way a tmux
  server restart does not survive today either — tmux sessions die with the
  server). This should be pinned down in the plan phase as a compatibility
  contract, not left implicit.
- **Partial output on a killed connection.** stapler-squad's control-mode
  reader explicitly drains in-flight commands with `ErrControlModeStopped`
  on `%exit` rather than leaving them hanging (`control_mode.go:452-489`).
  `BackendTymux`'s gRPC client needs the equivalent: an `Attach` stream that
  ends mid-frame (partial `PaneSnapshot`/output chunk, e.g. tymuxd crashes
  while writing) must not be silently swallowed as "clean EOF" by the gRPC
  client stack — distinguish a clean stream-end (`exited: true`) from a
  transport error, and have `BackendTymux` surface the latter as a
  reconnect-worthy failure, not an exited pane.
- **Resize races under concurrent multi-viewer attach.** Already-known
  ADR-004 (`project_plans/v1-release/decisions/ADR-004-concurrent-attacher-geometry-policy.md`)
  establishes smallest-attached-client-wins as the *single-recompute* policy,
  but the plan's own adversarial review
  (`project_plans/v1-release/implementation/adversarial-review.md:34-36`)
  flags that **overlapping** recompute triggers for the same window (two
  resizes arriving close together) are not serialized against each other —
  a second, newer computation's unlocked `Pane::resize()` calls can race a
  first, stale one's, producing a transient torn-geometry state one layer
  above what ADR-004 guarantees within a single trigger. This is tracked as
  an open pre-mortem risk (`project_plans/v1-release/implementation/pre-mortem.md:9`,
  P3) — not yet fixed. If stapler-squad ever has more than one viewer per
  agent pane (unlikely per the requirements' Concurrency rabbit hole, but not
  ruled out), `BackendTymux` inherits this as a live, acknowledged-but-open
  tymux-side bug, not a hypothetical.
- **Daemon-restart / crash-mid-session data loss for exit status specifically.**
  Per §1's `ExitStatus()` finding, tmux's exit-status data is destroyed the
  instant the pane is killed/respawned — tymux's equivalent field needs an
  explicit answer for what happens if tymuxd itself restarts between a pane
  exiting and stapler-squad reading its status: is exit status persisted
  across a daemon restart, or lost like tmux's is lost on session death?
  Given tymux's ADR corpus already treats persistence carefully (Tier-0-only
  persistence contract referenced in `project_plans/v1-release/design/ux.md:7`),
  this should be an explicit scope decision, not an oversight.
- **tymuxd under fork/spawn pressure analogue.** stapler-squad's fork-pressure
  monitor (`session/tmux/fork_metrics.go`) alerts at 10 spawn failures/30s or
  120 spawns/30s (4/s average) — see §4. tymuxd has no forking model (no
  per-session subprocess), but it does have its own resource-pressure
  equivalent: how many concurrent PTYs/panes can one tymuxd process host
  before degrading (fd exhaustion, scheduler/thread pressure, per-pane buffer
  memory)? The Concurrency-at-scale open question in requirements.md is
  exactly this, and stapler-squad's existing alerting thresholds (spawn
  rate, failure count) are a reasonable template for what tymux-side
  telemetry should look like once a number is picked.

## 4. What `zombie_detector.go`/`fork_metrics.go` solve, and what `BackendTymux` must solve instead

These two files solve **subprocess accounting under a one-tmux-session-per-agent
forking model** — a problem that doesn't exist the same way for `BackendTymux`
(tymuxd centralizes PTY lifecycle; stapler-squad never forks a subprocess per
agent when using it), but the underlying *problem class* — "how do we know our
process-lifecycle bookkeeping hasn't silently drifted from reality" — still
needs an answer, just a different mechanism:

- **`zombie_detector.go`** (`session/tmux/zombie_detector.go:1-158`) scans for
  zombie (`Z`-state) direct children via `ps`, reaps them, and — critically —
  **establishes a startup baseline** so pre-existing zombies at service start
  don't trigger false alarms, only *growth* over time does
  (`StartZombieWatcher` doc comment, `zombie_detector.go:78-90`). The
  underlying problem: "a child process we spawned exited, but nothing
  collected its exit status, so it's stuck as a zombie consuming a PID slot."
  For `BackendTymux`, there's no local child process to leak a PID slot on
  stapler-squad's side — but tymuxd itself still spawns real child processes
  (the agent's shell/program) and has to reap those. The conceptual carryover
  is: **tymuxd needs its own equivalent zombie/orphan accounting**, and
  whatever health/metrics surface tymux exposes should let a client (or
  ops) detect "tymuxd's internal process bookkeeping has drifted" the same
  way this file lets stapler-squad detect it for tmux — this is an internal
  tymux-side reliability property, not something `BackendTymux` implements
  itself, but it's a gap worth flagging if tymux doesn't already have an
  analogous reaper/health-check for its own spawned children.
- **`fork_metrics.go`** (`session/tmux/fork_metrics.go:1-100+`) tracks spawn
  rate and failure rate in a sliding 30s window (ring buffers per metric),
  escalating to `ForkPressureWarning`/`ForkPressureCritical` and keying a
  `spawnRegistry` by PID so a detected zombie can be attributed back to
  "which component spawned this" (`spawnRegistry.entries`,
  `fork_metrics.go:85-100`). The underlying problem: "subprocess creation is
  expensive and can fail under load; we need to know when we're approaching
  that limit, and which code path is responsible." For `BackendTymux`,
  stapler-squad no longer pays a per-session fork cost at all (that's the
  entire point of the migration) — so this specific metric disappears from
  stapler-squad's side. But the *attribution* half of the problem — "if
  something's going wrong, which logical session/caller is responsible" —
  maps onto: does tymux's `Attach`/`CreateSession` surface enough
  session/client identity in its own metrics or error responses for
  stapler-squad to attribute a tymuxd-side failure (rate-limited, resource
  exhausted, etc.) back to a specific agent instance? That's the concrete
  ask for tymux-side observability this integration should carry forward,
  even though the fork-specific mechanism itself is obsolete.

## Sources (external prior art, §2)

- [ttyd#109 — Websocket client report abnormal connection close](https://github.com/tsl0922/ttyd/issues/109)
- [ttyd#445 — The connection closes within 1 minute](https://github.com/tsl0922/ttyd/issues/445)
- [gotty webtty package docs (WithReconnect)](https://pkg.go.dev/github.com/yudai/gotty/webtty)
- [wetty#361 — Transport close](https://github.com/butlerx/wetty/issues/361)
- [Eternal Terminal — How It Works (BackedReader/BackedWriter sequence-number replay)](https://eternalterminal.dev/howitworks/)
- [Mosh: An Interactive Remote Shell for Mobile Clients (USENIX ATC '12 paper)](https://www.usenix.org/system/files/conference/atc12/atc12-final32.pdf)
- [Zellij Client-Server Model — DeepWiki](https://deepwiki.com/zellij-org/zellij/2.1-client-server-model)
