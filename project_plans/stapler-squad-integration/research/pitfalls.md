# Research: Pitfalls & Risks — stapler-squad ↔ tymux integration

Agent 4 of SDD Phase 2 (stapler-squad-integration). Scope: known failure modes in
(1) gRPC bidi streaming, (2) PTY/terminal-multiplexer process lifecycle, (3)
dual-backend migrations, (4) Rust/Tokio async architectures like tymuxd's, and
(5) concrete design principles the plan should bake in given the must-fix
disconnect-survival requirement.

Confidence labels: VERIFIED (opened the file/ran the command, cited below) vs
INFERRED (domain knowledge / general pattern, not tymux-specific evidence) vs
HYPOTHESIS (a plausible root-cause candidate, explicitly unconfirmed).

---

## 1. gRPC bidirectional streaming pitfalls

**tymux's `Attach` RPC today** (VERIFIED, `crates/tymuxd/src/main.rs:444-541`,
`proto/tymux/v1/tymux.proto:46-68`): one bidi stream carries both directions —
client sends `AttachRequest{pane_id | input | resize}`, server sends
`AttachEvent{output | output_gap | exited}`. Two tokio tasks per attach
(`forward_handle` reads a `broadcast::Receiver<Vec<u8>>` and writes to the
outbound stream; `input_handle` reads inbound messages and writes to the
pane). Output fan-out uses `tokio::sync::broadcast` with capacity 1024
(`pane.rs:66`); a slow consumer gets `RecvError::Lagged(n)` translated into a
single `output_gap=true` event (`main.rs:186-201`), then normal streaming
resumes — bytes are dropped, not buffered or replayed.

Pitfalls to design against:

- **Backpressure is already lossy by design, and stapler-squad's consumer is
  exactly the kind that lags.** `output_gap` exists because tymux already
  anticipated this, but the requirement doc's own framing — "raw PTY byte
  streaming... architecturally what tymux's `Attach` RPC already provides" —
  undersells that xterm.js rendering on a browser tab (possibly backgrounded,
  possibly on a throttled JS event loop) is a slower, less predictable
  consumer than the CLI harness tymux's own tests exercise. `output_gap` in
  today's protocol carries no offset/byte-count, so `BackendTymux` cannot
  tell the difference between "dropped 4KB of prompt output" and "dropped
  400KB mid-build-log" — it can only signal "your grid may be stale" and
  fall back to a full `capture-pane`-equivalent resync (INFERRED: this is
  the standard mitigation for lossy broadcast-style protocols, mirrors what
  `CapturePaneContent` already exists for on the stapler-squad side).
  Design implication: `BackendTymux`'s render loop needs an explicit
  resync-on-gap path from day one, not as a v2 hardening pass.
- **gRPC-Go vs tonic (Rust) differ subtly on stream cancellation semantics**
  (INFERRED, general gRPC knowledge, not repo-specific). In Go, a client
  `CloseSend()` half-closes the send direction but the server can keep
  streaming until it chooses to return; an abrupt context cancellation
  (deadline, `ctx.Done()`, or the underlying HTTP/2 connection dying) is
  reported to the server-side handler as `context.Canceled`, but *when* that
  propagates depends on whether the server is blocked in a read or a write
  at the time — a server blocked writing to a dead TCP connection can hang
  until an OS-level TCP timeout unless keepalive/kernel timeouts are tuned.
  tonic surfaces the equivalent as the request `Streaming<AttachRequest>`
  returning `None`/an error and the outbound stream's `Sender` returning
  `Err` on next send — which `main.rs`'s `input_handle` and `forward_handle`
  already handle by simply returning (no explicit "kill on cancel" branch,
  confirmed absent — see §5). The risk is specifically on the Go *client*
  side stapler-squad will write: getting `CloseSend`, deadline, and
  context-cancellation semantics right for a long-lived stream is a common
  source of goroutine leaks and "stream looks alive but server already gave
  up" bugs in Go gRPC clients.
- **No keepalive/timeout configuration exists yet** (VERIFIED by absence —
  `grep` for `keepalive` in `crates/tymuxd` and `proto/` finds nothing). A
  loopback-only daemon (per project context) makes TCP-level half-open
  connections less likely than over a real network, but stapler-squad's
  deployment model (browser ↔ Go backend ↔ tymuxd, potentially proxied) may
  not stay purely loopback end-to-end for the browser leg — only the Go↔tymuxd
  leg is loopback per the stated trust model. If the Go↔tymuxd leg is ever a
  gRPC connection through a sidecar/proxy, missing HTTP/2 keepalive
  (`grpc.KeepaliveParams` client-side, `grpc.KeepaliveEnforcementPolicy`
  server-side / tonic's `http2_keepalive_interval`) reintroduces the classic
  "half-open TCP connection, stream looks alive for minutes" failure.
- **Reconnection/resume has no protocol support today.** `Attach`'s first
  message sets `pane_id` and streaming begins from "now" — there is no
  resume-from-offset or replay-missed-output primitive (VERIFIED: no
  sequence number or offset field on `AttachRequest`/`AttachEvent` in
  `tymux.proto:250-274`). A `BackendTymux` reconnect after a network blip
  must re-`Attach` and then explicitly resync via a full-screen capture —
  same mechanism as the `output_gap` fallback above. Two independent
  reasons converge on the same required fallback path; build it once.

## 2. PTY / terminal-emulation pitfalls (the disconnect-survival bug's class)

**What real tmux does, for comparison** (INFERRED, general tmux
architecture knowledge — not verified against tmux's actual source in this
pass, but this is well-documented tmux design): the tmux *server* is a
daemon that detaches from any controlling terminal at startup (effectively
`setsid()`-equivalent — it forks and the child becomes a session leader with
no controlling tty of its own). Each pane's child process is spawned with
its own pty (`forkpty()`) and, critically, the tmux server process itself
never has a controlling terminal to lose — so no OS-level SIGHUP can ever
reach it or, through it, its children via job-control signal propagation.
tmux clients are a *separate, disposable* process (`tmux attach`) that
merely opens a control socket to the already-running, terminal-less server;
killing the client, however abruptly, cannot deliver any signal to
panes — there is no OS relationship between the client's tty and the pane's
tty at all.

**tymuxd's current shape, for comparison** (VERIFIED,
`crates/tymux-core/src/pane.rs:163-242`): `Pane::spawn_internal` opens a
*new* PTY pair per pane via `portable_pty::native_pty_system().openpty()`
and spawns the child on the *slave* side — architecturally the same
separation tmux has (each pane gets its own pty, distinct from any client's
pty). The blocking read side runs on a dedicated `std::thread`, not a tokio
task (correctly noted in the pane's own doc comment, `pane.rs:214-215`, as
required because `portable_pty`'s reader is blocking `std::io::Read`). The
investigation already ruled out the obvious code-level causes (see below);
what's still open is whether **tymuxd itself has a controlling terminal it
shouldn't**, which is the one thing tmux's design explicitly forecloses and
tymuxd's doesn't appear to do anything to foreclose.

**Prior investigation, restated precisely** (VERIFIED,
`crates/tymux-e2e/tests/disconnect_survival_e2e.rs:63-136`, commit
`ab88c81`): only closing the *client's own pty master* (a genuine OS-level
tty hangup, not SIGTERM/SIGHUP delivered directly to the CLI's PID)
reproduces the bug, 100% of the time. Ruled out: explicit `kill()` call
sites (only `Engine::kill_session`/`Engine::close_pane`, both RPC-only, per
grep and confirmed again in this pass at `main.rs:480-541` — neither
`forward_handle` nor `input_handle` calls `pane.kill()` or touches the
pane's own master pty on stream end); fd/device aliasing (different
`tty-index` confirmed via `/proc/<pid>/fdinfo`); timing dependence; input
dependence. The pane's own reader thread observes a genuine `Ok(0)` EOF on
*its own* pty within 1-3ms of the client's pty closing — the shell itself
is exiting, not being killed by tymuxd code. `strace -f` output during the
follow-up was ambiguous and attributed to `ptrace_scope`/PID-reuse artifacts
of the sandboxed dev container, not trusted further.

Given that framing, the remaining candidate root causes worth checking on
real hardware (the investigation's own explicit ask), ranked by how well
they explain "only a *real pty hangup*, not a signal, triggers it, and it's
the pane's own separate pty that sees EOF":

- **HYPOTHESIS — tymuxd itself may still have (or briefly acquire) a
  controlling terminal.** If `tymuxd` was launched from a shell (foreground
  or backgrounded with `&`, not fully daemonized/`setsid`'d) and shares a
  session with the CLI harness or the sandbox's outer login shell — which
  the investigation's own note about "every process here... share one
  systemd cgroup scope with no controlling terminal of their own" hints at
  but doesn't fully resolve for *tymuxd specifically* — then a controlling
  tty's hangup can, via kernel job-control (`SIGHUP` to the foreground
  process group of the *session*, on last-close of the master side of a
  pty), reach `tymuxd` or its already-forked pane children if they were
  never moved to their own session (`setsid()`) after `openpty()`+spawn.
  `portable_pty`'s `spawn_command` typically does call `setsid()`/`TIOCSCTTY`
  for the *slave* side of the pane's own pty (standard pty-child setup, so
  the child becomes session leader of *that* pty) — but that's orthogonal
  to whether `tymuxd` (the parent/daemon) itself has a stray controlling
  terminal that a *different* pty hangup could still signal through
  process-group inheritance if `tymuxd` wasn't correctly detached at
  startup. This is the single highest-value thing to check on real hardware
  first: `ps -o pid,ppid,pgid,sid,tty -p $(pgrep tymuxd)` while running, and
  whether it shows a real `tty` or `?`.
- **HYPOTHESIS — job-control signal propagation through process group,
  independent of tty file descriptors.** The fdinfo/tty-index check ruled
  out *file descriptor* aliasing but not *process-group* relationships —
  SIGHUP-on-hangup is delivered to a process group, not to individual fds.
  If the pane's child process ends up in the same process group as the
  harness/CLI (e.g., because `tymuxd` forked without an intervening
  `setpgid`/`setsid` between accepting the gRPC connection and spawning the
  pane), the pty hangup could reach it that way even though the *pty
  device* itself is confirmed distinct.
- Real tmux's specific defenses that are worth confirming tymuxd matches:
  ignoring `SIGHUP` in the server process (`signal(SIGHUP, SIG_IGN)`)
  entirely, versus tymuxd's Rust default of doing nothing special with
  SIGHUP (Rust processes get the OS default disposition for SIGHUP —
  terminate — unless explicitly handled; INFERRED, not verified against
  tymuxd's signal handling because no signal-handling code was found via
  the earlier grep for `SIGHUP` in `pane.rs`/`main.rs`, which returned zero
  hits outside comments).
- Standard reaping/zombie risk for *any* PTY multiplexer (INFERRED, general
  Unix pattern, not tymux-specific): if a pane's child forks its own
  grandchildren (e.g., a shell running a pipeline) and the pty itself
  hangs up cleanly, `wait()`/`waitpid()` must still be called on the
  session's process (portable_pty's `Child::kill()`/drop typically handles
  this for the direct child) — but grandchildren detached from the pty
  (daemonizing subprocesses, backgrounded jobs) are exactly the kind of
  thing that becomes an orphan reparented to PID 1, not a tymuxd zombie, so
  this is lower risk than the primary bug but worth a note if any Epic
  4-style "revive" work assumes a clean process tree.

## 3. Migration / dual-backend pitfalls (`BackendTmux` + `BackendTymux`)

- **Exit-status/PID surface must match `ProcessManager`'s existing contract
  exactly, not just "have an equivalent."** VERIFIED,
  `~/Programming/stapler-squad/session/process_manager.go:10-67`: the
  interface `BackendTymux` must satisfy includes `GetPanePID() (int32,
  error)`, `SetOnExitCallback(fn func(string))`, `ResetExitOnce()`,
  `IsAlive() bool`, and content/cursor introspection
  (`GetCursorPosition`, `GetPaneDimensions`, `CapturePaneContentRaw`, etc.)
  — all synchronous, poll- or callback-shaped APIs that `BackendTmux`
  presumably backs with tmux control-mode queries. tymux's `exited` flag
  and `wait_exit()` (VERIFIED, `pane.rs:244-275`) give a boolean, not a
  numeric exit code — but the requirements doc explicitly flags "exit
  code reporting" as a missing capability tymux needs to add. Any drift
  between what `BackendTmux` returns for these methods (e.g., tmux's own
  exit-status semantics on abnormal termination, signal vs. exit-code
  distinction) and what a new tymux exit-status field can represent is a
  parity gap that will only surface under specific agent workloads (a
  process killed by signal vs. one that calls `exit(1)`).
- **`SetOnExitCallback`/`ResetExitOnce` naming implies a fire-once contract**
  stapler-squad's existing callers likely depend on (VERIFIED interface
  shape, semantics INFERRED from naming) — `BackendTymux`'s adapter around
  tymux's `wait_exit()` must not fire the callback more than once, and must
  cope with a callback registered *after* the process already exited (same
  "don't miss it in the gap" concern `Pane::wait_exit` itself already
  handles server-side per its own doc comment, `pane.rs:260-263` — the Go
  adapter needs the equivalent client-side guarantee).
- **`SendInputViaControlMode(ctx context.Context, data []byte) error`**
  already exists as a distinct method from plain `SendKeys` (VERIFIED,
  `process_manager.go:31`) — suggests `BackendTmux` has two different input
  paths (raw `send-keys` vs. control-mode) with presumably different
  reliability/escaping characteristics. `BackendTymux` collapsing both onto
  a single `Attach`-stream `input` payload is a behavioral simplification
  that needs an explicit parity check: does anything in stapler-squad rely
  on `SendKeys`'s specific escaping/quoting behavior that raw byte input
  wouldn't replicate?
- **stapler-squad already has hard-won process-lifecycle scar tissue that
  should inform, not be duplicated by, `BackendTymux`.** VERIFIED,
  `~/Programming/stapler-squad/executor/safeexec/safeexec_pdeathsig_linux.go:21-28`:
  the native (non-tmux) backend sets `Pdeathsig: syscall.SIGKILL` so a
  managed subprocess dies if stapler-squad's own process dies — the
  opposite lifecycle goal from what tmux/tymux provide (survive the
  *client's* death, not the backend's). This is a different relationship
  (stapler-squad process → its own spawned child, vs. tymuxd → pane child)
  but it's exactly the class of PID-lifecycle bug the disconnect-survival
  investigation is chasing, in the same codebase's own history — worth
  having whoever debugs the tymux bug read this file first, since the
  `safeexec` package name and a `_test.go` alongside it imply this class of
  bug already bit stapler-squad once for the native backend and got a
  dedicated regression test.
- **Config/flag-flip risk.** The requirements doc names `BackendTmux`
  "unchanged, still default" as a success metric — meaning
  `BackendTymux` selection is presumably a config value or feature flag on
  `ProcessManagerBackend`. The standard risk (INFERRED, general
  feature-flag pitfall) is scope: is the flag global (all sessions use one
  backend) or per-session? A global flag flipped in production with a
  single unfixed tymux bug (the disconnect-survival bug is explicitly
  must-fix and currently unresolved) would silently downgrade every running
  agent session's disconnect resilience — the plan should make the flag
  per-session/opt-in at least until the bug closes, not a global default
  toggle.
- **Testing burden is combinatorial, not additive.** Validating parity
  means (agent type) × (backend) × (disconnect scenario) × (I/O pattern —
  large scrollback, ANSI-heavy TUI agent output, etc.), not just "does
  BackendTymux work" in isolation. The requirements doc scopes "at least
  one real agent type end-to-end" for Phase-1 success, which is
  appropriately narrow — but the Rabbit Holes section already flags
  cell-grid↔ANSI rendering mismatch as a risk, and that risk multiplies
  across every agent type with different TUI behavior (Claude Code's own
  interactive UI vs. a plain Aider transcript are very different ANSI
  payloads).

## 4. Rust/Tokio-specific pitfalls (tymuxd's actual architecture)

VERIFIED against `crates/tymux-core/src/pane.rs` and
`crates/tymuxd/src/main.rs`:

- **`broadcast::channel` capacity 1024, lag = silent data loss by design**
  (`pane.rs:66,190`). This is a deliberate, already-mitigated tradeoff (see
  §1's `output_gap` discussion), not a bug — but it means capacity tuning
  is a real lever: a burst-heavy agent (e.g., a build tool dumping
  megabytes of log output) with a slow xterm.js consumer will hit `Lagged`
  more often than the CLI-harness-driven e2e tests exercise, since those
  tests use fast native consumers. Worth a load test with a genuinely slow
  simulated browser consumer before trusting the parity claim.
  Additionally: `broadcast::Receiver` design means a receiver that's
  *briefly* not attached (edge case: reconnect race) sees `Closed` only
  when *all* senders drop, and sees `Lagged` rather than "no data" when it
  falls behind — there's no way to distinguish "you missed the exact
  moment of a fast reconnect" from "you missed 900 bytes of ordinary
  output" from the receiver's perspective; both surface identically as one
  `output_gap` event.
- **The PTY reader is a blocking OS thread, not a tokio task, and that's
  called out as deliberate** (`pane.rs:214-215`: "portable_pty's reader is
  blocking `std::io::Read`, so it gets its own OS thread rather than a
  tokio task") — correct pattern (blocking I/O must never run on a tokio
  worker thread), but it means **thread lifecycle isn't tokio-supervised**.
  `_reader_handle: Mutex<Option<JoinHandle<()>>>` is stored
  (`pane.rs:211,239`) but grep shows no code that ever `.join()`s it
  (VERIFIED: only assignment at line 239, no `.join()` call found in the
  file) — the thread is fire-and-forget; if it ever panics mid-read (e.g.,
  a `.lock().unwrap()` poisoning on the parser mutex under a concurrent
  panic elsewhere), nothing observes that panic, and the pane silently
  stops receiving output with no `exited` flag ever set (since that's only
  set *after* the read loop's normal `break`, not on a `catch_unwind`
  boundary). This is a plausible **separate** future bug class from the
  disconnect-survival one, worth a defensive note even though it isn't the
  current bug.
- **Every spawned tokio task in `attach()` is wrapped in `supervise()`**
  (`main.rs:134-138`, applied at `main.rs:509,541`) specifically because
  "spawned tasks that panic vanish silently by default" — this is already
  the right pattern (log on panic) and should be the template `BackendTymux`
  authors point to when reviewing any *new* tymux-side task spawned for
  this integration (e.g., an exit-status-forwarding task) — don't
  reintroduce an unsupervised `tokio::spawn` for new plumbing.
- **Mutex-around-blocking-lock inside async paths**: `pane.rs` uses
  `std::sync::Mutex` (not `tokio::sync::Mutex`) for `writer`, `master`,
  `_child`, `parser` — correct choice for short, non-`.await`-holding
  critical sections (VERIFIED by usage pattern — `write_input`,
  `resize`, `kill()` all lock-and-release synchronously, `pane.rs:277-288,
  255-258`), but any *future* addition that needs to hold one of these
  locks across an `.await` (e.g., a hypothetical async exit-status RPC that
  reads `_child` while awaiting something else) would deadlock a tokio
  worker thread. Flag this as a constraint for whoever implements the
  exit-code feature: keep `_child` access synchronous, or migrate that one
  field to `tokio::sync::Mutex` deliberately rather than accidentally
  `.await`-ing while holding the `std::sync::Mutex` guard.

## 5. Concrete design principles to build against (must-fix disconnect-survival requirement)

1. **Detach ≠ kill: stream cancellation must never be the trigger for pane
   teardown, at any layer.** Already true at the tymuxd gRPC-handler layer
   today (VERIFIED — neither `forward_handle` nor `input_handle` calls
   `pane.kill()`, `main.rs:480-541`); the requirement is to keep it true as
   new code is added (e.g., any future "clean up on disconnect" logic must
   act on `unregister_viewport`/window geometry only, never on the pane's
   own process).
2. **The daemon itself must have no controlling terminal.** Verify (real
   hardware, not the sandbox) that `tymuxd` is fully session-detached at
   startup — `setsid()` or equivalent — so no client's, harness's, or
   parent shell's tty hangup can ever reach it or its already-spawned pane
   children via kernel job-control signal delivery. This is the single
   biggest structural difference between tymuxd's current shape and real
   tmux's server model called out by this research, and the top candidate
   for the disconnect-survival bug's actual root cause per §2.
3. **Every pane's process group must be fully independent of every
   client's process group**, not just its pty device. The prior
   investigation confirmed distinct pty *devices* (fdinfo tty-index) but
   did not confirm distinct process *groups/sessions* — close that gap
   explicitly (`ps -o pid,pgid,sid,tty` on both the pane's child and the
   attaching client at the moment of hangup) before declaring any fix
   complete.
4. **Any exit-status feature must not introduce a second path that can
   look like process death.** Adding exit-code plumbing (the Phase-2
   requirement) touches the exact code path implicated in the current bug
   (`Ok(0)` EOF interpretation in the reader thread, `pane.rs:217-238`) —
   design the exit-code capture as strictly additive to the existing
   `exited`/`wait_exit()` flow, not a parallel mechanism that could race or
   double-fire it.
5. **Treat `output_gap` and reconnect-resync as one mechanism, not two.**
   Both a lagging xterm.js consumer (§1) and a reconnect-after-disconnect
   client (§1, §3's fix target) need the same fallback: drop the broadcast
   stream's implicit ordering guarantee and resync via a full-state
   capture. Build `BackendTymux`'s resync path once and use it for both
   triggers.
6. **Keep the flag/backend selection per-session, not global**, until the
   disconnect-survival bug is closed and re-verified outside the sandboxed
   dev container (§3) — a global default flip currently has no safety net
   for the one requirement explicitly marked must-fix and currently
   unresolved.
7. **Don't let the exit-code/PID feature silently violate `ProcessManager`'s
   existing fire-once/callback-after-the-fact contracts** (§3) — mirror
   `Pane::wait_exit()`'s own check-before-and-after-registration pattern
   (`pane.rs:264-274`) on the Go adapter side too.
