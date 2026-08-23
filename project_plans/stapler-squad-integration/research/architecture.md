# Architecture Research: stapler-squad-integration

**Phase**: 2 (Research) of the stapler-squad-integration SDD workflow, Agent 3 (Architecture)
**Input**: `project_plans/stapler-squad-integration/requirements.md`
**Builds on**: `project_plans/v1-release/research/architecture.md` (tymux's own v1.0
internal architecture — splits/layout tree, persistence, cross-language client
plumbing, resize semantics). This doc does not re-derive any of that; it cites
it by section where the integration touches it.

---

## 1. `BackendTymux` shape inside stapler-squad's `ProcessManager`

### Recommendation: a thin adapter over an internal manager, mirroring `TmuxBackend`, not `NativeProcessManager`

stapler-squad already has two structurally different precedents for implementing
`ProcessManager` (`session/process_manager.go:10-68`):

- **`TmuxBackend`** (`session/tmux_backend.go:10-124`) — a pure delegation shim:
  every method is a one-line forward to an internal `TmuxManager` interface
  implemented by `TmuxProcessManager`. `TmuxBackend` itself holds no state.
- **`NativeProcessManager`** (`session/native_process_manager.go`) — implements
  `ProcessManager` directly, owns the pty/`cmd` itself, and runs its own
  `supervise()` goroutine that calls `cmd.Wait()` and fires `onExitCallback`
  (`session/native_process_manager.go:118-152`).

`BackendTymux` should follow the **`TmuxBackend` shape**: a thin struct
implementing `ProcessManager` by delegating to an internal `TymuxManager`
(or just a concrete `*tymuxGRPCSession` type, since there's only one real
implementation, unlike tmux's mock-for-tests need) that owns the gRPC
connection, the resolved `session_id`/`pane_id`, and the live `Attach` stream.
This is architecturally consistent with the existing pattern (`backend_factory.go`
already switches on `ProcessManagerBackend` the same way for a third case) and
keeps `BackendTymux` in `session/backend_tymux.go` symmetrical with
`session/tmux_backend.go`, rather than reinventing `NativeProcessManager`'s
supervise-loop pattern for a case where the daemon (not stapler-squad) already
owns process supervision.

### Method-by-method RPC mapping

| `ProcessManager` method | tymux RPC(s) | Notes |
|---|---|---|
| `Start(dir string) error` | `CreateSession` (command defaults to `$SHELL`, `cwd` isn't a `CreateSessionRequest` field today — see gap below) | `dir` has no home in `CreateSessionRequest` (`tymux.proto:134-137` — only `name`, `command`). **Gap #1.** |
| `RestoreWithWorkDir(workDir string) error` | `ListSessions` (to find an existing session by name) then either `ReviveSession` (if `Liveness::Dead`, per `tymux.proto:151-159`) or a no-op (if already `Live`) | Matches tymux's own dead/live/never-existed distinction (v1-release doc §4's "dead-pane vs. never-existed" invariant) cleanly — this is exactly the semantic stapler-squad's restore-on-restart path needs. |
| `Close() error` | `KillSession` | Straightforward — cascades correctly per `Engine::kill_session` (`crates/tymux-core/src/engine.rs:381-408`). |
| `IsAlive() bool` | `CapturePane` (reads `PaneSnapshot.liveness`) or a cached value from the last `AttachEvent`/`WatchWindow` push | No dedicated liveness RPC exists; `CapturePane` is the correct read (cheaper than `ListSessions`), but every `IsAlive()` poll is a full RPC round-trip unless `BackendTymux` caches liveness from the standing `Attach` stream's `Exited`/`output` events instead of polling. Recommend caching. |
| `GetSessionIdentifier() string` | none — local state (the `session_id` UUID string tymux returned from `CreateSession`) | No RPC needed. |
| `HasSession() bool` | `ListSessions` (session_id present) or local cached state | Same caching note as `IsAlive`. |
| `GetCurrentWorkingDirectory() (string, error)` | **no RPC exists** | tymux's `Pane` struct tracks `cwd` server-side (`crates/tymux-core/src/pane.rs:80,192-196`) for persistence, but it is never surfaced over the wire — `Pane`/`PaneSnapshot` proto messages have no `cwd` field. **Gap #2 (real capability gap, in-scope per requirements.md's "whatever else is a genuine capability gap").** |
| `GetPTY() (*os.File, error)` | **no clean mapping** | tymux has no local pty fd to hand back — output only exists as bytes over the `Attach` stream. Any caller of `GetPTY()` that expects to read/write a real `*os.File` (e.g. code that wraps it in its own pty-reader loop) cannot be satisfied. **Flagged interface method with no RPC mapping — see below.** |
| `SendKeys(keys string) (int, error)` | `Attach`'s input stream (`AttachRequest.input`, `tymux.proto:250-256`) | Requires an already-open `Attach` stream; `BackendTymux` must keep one alive per session for the whole `ProcessManager` lifetime, not open one per call. |
| `TapEnter() error` | `Attach`'s input stream (`\r` or `\n` bytes) | Same stream as `SendKeys`. |
| `SendPromptWithEnter(prompt string) error` | `Attach`'s input stream (prompt bytes + `\n`) | Same stream. |
| `SendInputViaControlMode(ctx, data []byte) error` | `Attach`'s input stream | This is the real analog of tmux's control-mode `send-keys` — maps directly and is actually a *simpler* protocol than tmux control mode (no line-based framing/escaping to parse). |
| `CapturePaneContent() (string, error)` | `CapturePane` → render `PaneSnapshot.grid` (cells+attrs) to a string | Requires a **rendering step** tmux's `capture-pane -p -e` didn't (ANSI text came back pre-rendered); `BackendTymux` needs a `PaneSnapshot → string` renderer, either plain text (`CapturePaneContentRaw`-equivalent) or ANSI-re-encoded (matching `-e`'s escape-sequence output, which callers of `CapturePaneContent()` — used e.g. for prompt/banner detection — may depend on for attribute-sensitive parsing). |
| `CapturePaneContentRaw()` | `CapturePane` → grid cells' `.text` concatenated, no ANSI | Cleanest of the capture variants — a straight `Cell.text` join per row. |
| `CapturePaneContentWithOptions(startLine, endLine string)` | `CapturePane` with `scrollback_offset` set, or `SearchScrollback` if `startLine`/`endLine` are being used as anchors rather than literal offsets | tmux's start/end args are `-S`/`-E` line-number-or-`-`/history-relative; tymux's `scrollback_offset` is a single integer, not a range — **needs a small adapter function to map a tmux-style range onto one or more `CapturePane` calls**, not a 1:1 RPC. |
| `CaptureViewport(lines int)` | `CapturePane` (live screen only, `scrollback_offset=0`) truncated/tailed to `lines` | Straightforward once `CapturePaneContentRaw`'s renderer exists. |
| `GetCursorPosition() (x, y int, err error)` | `PaneSnapshot.cursor_row`/`cursor_col` (already returned by `CapturePane`, `tymux.proto:234-236`) | Free — no new RPC, just read fields already on the response the capture methods use. |
| `GetPaneDimensions() (width, height int, err error)` | `PaneSnapshot.rows`/`cols`, or `Pane.rows`/`cols` from the `Session` returned by `CreateSession`/`ListSessions` | Same — already present. |
| `SetWindowSize(cols, rows int) error` | `Attach`'s `Resize` message (`tymux.proto:258-261`) | Per v1-release doc §1 and ADR-004 (`tymux.proto:64-67`), resize is now **window-scoped**: the daemon takes the minimum viewport across every attached client. A single stapler-squad browser tab is normally the only attacher of a given pane, so this degrades to a simple 1:1 resize in practice — but `BackendTymux` should not assume no second attacher (e.g. a human `tymux attach`-ing the same session from a terminal for debugging) ever exists. |
| `SetDetachedSize(width, height, instanceTitle string) error` | `Attach`'s `Resize` (if attached) — **no way to resize a pane while fully detached** | tmux can resize a session with zero active clients (`set-option` + geometry tracking survive detach); tymux's resize path is entirely `Attach`-stream-scoped (v1-release doc §1's design). If `BackendTymux` keeps a standing `Attach` stream open for the whole `ProcessManager` lifetime (as `SendKeys` above requires anyway), this is achievable by sending `Resize` on that stream even when no *user* is watching — but if stapler-squad ever fully closes the stream between interactions, there is no resize path until it reopens. **Gap #3**, resolved by the standing-stream design choice below, not by a new RPC. |
| `RefreshClient() error` | no-op | tmux's `refresh-client` forces xterm.js to redraw from a fresh `capture-pane`; with tymux's structured `PaneSnapshot`, a client-side refresh is just "re-render the last received/re-fetched grid" — no server RPC needed. |
| `GetPanePID() (int32, error)` | **no RPC exists** | tymux's `Pane` never captures or exposes the child process's OS pid anywhere in `pane.rs` or the proto. **Gap #4** — likely lower priority than exit-status (§2) since nothing in the described integration explicitly needs the PID itself, only liveness/exit code, but flag it as a known interface method with no mapping. |
| `HasUpdated()`, `FilterBanners()`, `HasMeaningfulContent()` | pure client-side text processing over `CapturePaneContentRaw()`'s output | No RPC — same content-helper logic tmux's implementation presumably already has, reusable verbatim against tymux's rendered text. |
| `StartControlMode()` / `StopControlMode()` | no-op (success) | tymux has no distinct "control mode" — `Attach` *is* always structured/control-mode-equivalent. These become no-ops that just track a boolean for API-shape compatibility. |
| `SubscribeToControlModeUpdates() (string, chan []byte)` / `UnsubscribeFromControlModeUpdates(id)` | fan out from the single `Attach` stream's output events | `BackendTymux` needs an internal pub/sub broadcaster (Go channel fan-out) over the one `Attach` stream's `AttachEvent.output` bytes, since stapler-squad's interface allows multiple subscribers per pane but tymux's `Attach` is one bidi stream per pane per client connection (v1-release doc §1: "keep `Attach` scoped to a single `pane_id`... do not try to multiplex several panes... over one stream"). One `BackendTymux`-side `Attach` stream, fanned out to N local subscriber channels, is the correct shape — do **not** open a second `Attach` stream per subscriber (wastes a daemon-side broadcast-channel receiver per subscriber for no benefit, and reintroduces the multi-attacher geometry-policy interaction from ADR-004 for purely-internal fan-out that doesn't need it). |
| `Attach() (chan struct{}, error)` | the standing `Attach` stream itself | This is stapler-squad's "interactive TUI attach" concept; for `BackendTymux` this is largely already-open (see `SendKeys` above) — the returned `chan struct{}` should close when the `Attach` stream ends (either `Exited` event or an unrecoverable stream error). |
| `DetachSafely() error` | full cancellation of the `Attach` call (`tymux.proto:54-57`'s documented detach contract) | Matches tmux's own detach semantics closely — but note the v1-release doc's warning (§3, gap 2) that half-closing the stream's send side is *not* sufficient; `BackendTymux` must fully cancel the gRPC call (context cancellation in the Go client), not just stop writing. |
| `SetOnExitCallback(fn func(string))` / `ResetExitOnce()` | driven by `AttachEvent.Exited` (today: `bool`; see §2 for the proposed exit-code carrying field) | Direct match to `NativeProcessManager`'s own `onExitCallback` pattern (`native_process_manager.go:145-152`) — `BackendTymux`'s `Attach`-stream-reader goroutine calls the registered callback exactly once (mirroring `ResetExitOnce`'s existing once-semantics) on receiving `Exited`. |

### Summary of flagged gaps (feed into planning-phase scoping)

1. **No `cwd` in `CreateSessionRequest`** — `Start(dir string)` has nowhere to
   put `dir` today. Needs a proto field addition.
2. **No `cwd` readback anywhere in the wire format** — `GetCurrentWorkingDirectory()`
   has no RPC despite the daemon already tracking `Pane.cwd` server-side
   (`pane.rs:80`). Cheapest gap to close (the data already exists; it just needs
   exposing on `Pane`/`PaneSnapshot`).
3. **`GetPTY()` has no clean mapping** — genuinely architecturally distinct
   (fd-based vs. stream-based I/O); any caller depending on a raw `*os.File`
   must be identified and either reworked to use the byte-stream API or
   explicitly left `BackendTmux`-only.
4. **`GetPanePID()` has no mapping** — no pid anywhere in tymux's proto/engine.
   Likely low-priority; confirm during implementation whether any caller of
   `ProcessManager.GetPanePID()` is actually reachable from the `BackendTymux`
   code path before treating this as blocking.
5. **`SetDetachedSize()` needs a standing `Attach` stream design** to have any
   RPC to call at all when nothing is "watching" — this is a design decision
   `BackendTymux` must make (keep one `Attach` stream open for the pane's
   entire stapler-squad-managed lifetime, fanning out to zero-or-more local
   subscribers), not a tymux-side change.

---

## 2. Exit-status reporting on the tymux side

### Recommendation: extend `AttachEvent.exited` from `bool` to carry an optional exit code, captured from `portable_pty::Child::wait()`/`try_wait()` in the pane reader thread — not a new RPC, not a `Liveness` field

**Where `exited` is threaded today** (confirmed by reading the full path):

1. `crates/tymux-core/src/pane.rs:217-241` — the pane's dedicated OS reader
   thread loops on `reader.read()`; on `Ok(0)` (EOF) or `Err(_)` it breaks out,
   then does exactly two things: `pane_for_reader.exited.store(true, ...)`
   and `pane_for_reader.exit_notify.notify_waiters()` (`pane.rs:236-237`).
   **It never calls `self._child.lock().unwrap().wait()` or `try_wait()`** —
   the `Child` handle is held only "to keep the child alive" (`pane.rs:106-107`
   doc comment) and is otherwise untouched on this path. This is the natural
   and minimal-disruption place to also capture the exit code: right after
   the read loop breaks (which already means the child's side of the pty is
   gone), call `self._child.lock().unwrap().wait()` (blocking is fine — this
   thread has nothing else to do once EOF hits) and store the resulting code
   alongside the existing `exited` flag.
2. `pane.rs:264-275`'s `wait_exit()` — an async-friendly poll over the same
   `exited`/`exit_notify` pair — needs no interface change if the code is
   stored as a plain field read via a new accessor (e.g. `pub fn exit_code(&self) -> Option<i32>`)
   rather than folded into the `Notify` payload itself.
3. `crates/tymuxd/src/main.rs:480-507`'s `forward_handle` task — on
   `pane_for_exit.wait_exit()` resolving, it currently sends a single
   `AttachEvent { payload: Some(Exited(true)) }` (`main.rs:497-503`). This is
   the one call site that needs to change: swap the `bool` for the new
   message shape and read `pane_for_exit.exit_code()` at the same point.

**Proto shape recommendation**: replace `bool exited = 3;` with a message,
not a second field alongside it (a second field invites the "what if `exited=false`
but `exit_code` is set" invalid state `type-driven-design` would flag):

```proto
message ExitStatus {
  // Absent (has_code=false) covers the exit-code-unknown case explicitly —
  // e.g. the child was killed by a signal portable_pty's ExitStatus can't
  // decode into a numeric code, or wait() itself failed. A client must not
  // conflate "process exited with code 0" and "exit code unknown".
  bool has_code = 1;
  int32 code = 2;
}

message AttachEvent {
  oneof payload {
    bytes output = 1;
    PaneSnapshot snapshot = 2;
    ExitStatus exited = 3;   // breaking change: bool -> message, same field number
    bool output_gap = 4;
  }
}
```

This mirrors the same `has_code`-guard pattern the existing `Liveness` enum
already uses proto3's reserved-zero convention for
(`tymux.proto:71-78`'s doc comment: "an absent/default field is never silently
misread"). Keep the field number (`3`) — this is a breaking wire-format
change regardless (bool → message is not wire-compatible), so it should land
in one deliberate pass alongside whatever other breaking proto changes
v1-release's own splits epic is already making (see v1-release doc §1's
`Window.layout` breaking change) rather than as a second separate breaking
release.

**Why not `Liveness`/a new field on `Pane`, why not a separate RPC**:
- `Liveness` is a **poll-shaped** read (used by `CapturePane`/`ListSessions`
  snapshots — v1-release doc §4's "dead-pane vs never-existed" invariant is
  about *distinguishing states on read*, not delivering a one-time event). An
  exit code is fundamentally a **one-time event** at the moment of transition,
  which `AttachEvent`'s existing stream is the correct place for — it already
  delivers `Exited` exactly once, biased ahead of any race with buffered
  output (`main.rs:486-505`'s `biased` `select!` comment). Duplicating it onto
  `Liveness` would need a second delivery mechanism (a poll-based reader would
  need to notice the transition, not just read a static enum) for no benefit.
- A separate RPC (e.g. `GetExitStatus(pane_id)`) adds a second round-trip a
  client must remember to make, for data the `Attach` stream already delivers
  for free at the exact moment it becomes available. It would only be
  justified if a caller needed the exit code *without* an open `Attach`
  stream (e.g. after a full detach) — which is a real but secondary need:
  recommend also persisting the last-known exit code onto the `PersistedPaneRecord`/dead
  `PaneEntry` (v1-release doc §2's persistence design already has a natural
  home for this — the same JSON record that already tracks a dead pane's
  metadata) so `CapturePane` on a dead pane can surface it without inventing
  a new RPC, rather than adding one.

---

## 3. Abrupt-disconnect data-flow trace (starting hypothesis space, not a fix)

**Do not re-litigate what's already been ruled out.** The bug is already
extensively documented as failed-investigation notes directly in
`crates/tymux-e2e/tests/disconnect_survival_e2e.rs:63-136` (doc comment on
`pane_survives_abrupt_disconnect`, `#[ignore]`d pending a real-hardware
retest — see requirements.md's "must-fix, load-bearing" scope item and
Feasibility Risk). That investigation already ruled out, with specific
evidence:

- Every `Pane::kill()` call site (`Engine::kill_session`,
  `Engine::close_pane` only — both explicit-RPC-only, `engine.rs:381-408`,
  `engine.rs:492+`) — grep-confirmed, neither fires on stream teardown.
- `tymuxd`'s two attach-handling tasks, `forward_handle` and `input_handle`
  (`main.rs:480-538`) — on stream-end, `input_handle` only calls
  `unregister_viewport` + `recompute_window_geometry` (`main.rs:536-538`),
  which (per v1-release doc §4's concurrent-attacher geometry policy) just
  re-applies `Pane::resize` to reflect the remaining attachers' minimum
  viewport — not a kill, not a pty teardown.
- fd/device aliasing between the client-side and server-side ptys (ruled out
  via `/proc/<pid>/fdinfo/<fd>`'s `tty-index` cross-check).
- Timing- and input-content-dependence (ruled out — reproduces at any delay,
  with zero bytes ever sent).
- The CLI (`tymux-cli`) itself exits with **status 0**, taking the ordinary
  `stdin_rx.recv() == None` shutdown path (`tymux-cli/src/main.rs:572`'s
  `select!` arm) — it is not killed by a signal, so the causal chain is not
  "CLI process dies abnormally and that somehow propagates."

**What is confirmed, and is the actual mechanism boundary**: only closing the
*client-side* pty master while the CLI process itself stays alive reproduces
the bug, 100% of the time — and when it does, the pane's own reader thread
(`pane.rs:217-241`, entirely server-side, a different OS pty device from the
client's) observes a genuine `Ok(0)` EOF within 1-3ms. Two ptys on different
devices, one hangup event, sub-millisecond-scale propagation — that's the
actual mystery, and it sits **below** the Rust/gRPC application layer the
investigation already searched exhaustively. The most architecturally likely
culprit locations for a next attempt, in priority order:

1. **Process-group/session (`setsid`) inheritance at pane-spawn time.**
   `Pane::spawn_internal` (`pane.rs:163-241`) uses `portable_pty`'s
   `CommandBuilder`/`pair.slave.spawn_command(cmd)` (`pane.rs:178-182`) to
   spawn the child — worth checking whether `portable_pty` (or `tymuxd`
   itself, indirectly, if `tymuxd` was launched *from* the same controlling
   terminal as the CLI harness in the sandboxed test environment) leaves the
   spawned child in the **same session/process group** as something upstream
   of the client's pty, rather than fully detached into its own session via
   `setsid`. A `SIGHUP` delivered to a foreground process group on a
   controlling-terminal hangup targets the whole group — if `tymuxd`'s
   listener process (not the individual pane child, which is a different
   process) is itself a member of a process group tied to *the sandbox's*
   controlling terminal (the doc comment's own caveat: "every process here,
   including `tymuxd` and the outer login shell, share one systemd cgroup
   scope with no controlling terminal of their own" — `disconnect_survival_e2e.rs:118-121`),
   this is exactly the shape of environment where a hangup on an unrelated
   terminal could still cascade in a way a real user's machine, where
   `tymuxd` is a properly-detached daemon, would not exhibit. **This is the
   strongest lead for "why does this only reproduce in the sandbox."**
2. **`portable_pty`'s `openpty`/`spawn_command` on the daemon side possibly
   not fully decoupling the child's controlling terminal.** Worth an explicit
   check (not yet done, per the doc comment's own "abandoned rather than
   trusted further" framing of the `strace` attempt) of whether the spawned
   shell child actually gets its *own* controlling tty via the slave pty, or
   whether it somehow retains a reference to whatever tty `tymuxd`'s own
   process inherited at startup — the latter would explain a hangup
   propagating without any explicit `kill()` call, purely via kernel-level
   SIGHUP delivery to a shared session.
3. **Re-run on real hardware first**, exactly as the doc comment's own
   recommendation states (`disconnect_survival_e2e.rs:126-132`) — this isn't
   a new hypothesis, it's the correct next *step*: confirm the bug exists
   outside the current sandbox (`ptrace_scope=1`, shared cgroup, no real
   controlling terminal) before spending more root-causing effort inside an
   environment shape that may itself be a contributing or sole cause. If it
   does *not* reproduce on real hardware, the "bug" may in fact already
   satisfy requirements.md's success metric ("a pane survives an abrupt
   client disconnect") and this scope item becomes "confirm and add a
   permanent regression test," not "fix."

This satisfies the requirements-doc's scope note ("Root cause of the
abrupt-disconnect pane-kill bug" is an Open Question, "needs real-hardware
debugging" is a named Rabbit Hole) — the architecture-level contribution here
is narrowing where to look next, not solving it.

---

## 4. Go client generation: buf-managed, mirroring `clients/ts/`, using Connect-Go

### Recommendation: a new `clients/go/` directory in the **tymux** repo, generated via `buf.gen.yaml`, using `connectrpc.com/connect`'s Go plugin — not raw `google.golang.org/grpc`, and not a vendored/published-only module with no visible generation story

Three points converge on this:

1. **tymux's own precedent is explicit and already documented**: the root
   `proto/buf.gen.yaml` (`proto/buf.gen.yaml:1-19`) states plainly that Rust
   codegen bypasses it (`tymux-proto/build.rs` calls `tonic-build` directly)
   and that this file is "for *other* language clients (TS, Python, Go, ...)
   **as they show up**" — Go showing up now is exactly the anticipated case,
   not a new pattern. `clients/ts/` (`clients/ts/package.json:1-24`) is the
   template: its own `package.json`/module identity, a `generate` script
   that runs `buf generate ../../proto`, generated code lands in
   `clients/ts/gen/` (git-tracked, per the buf.gen.yaml `out:` path), and the
   plugin binary is resolved locally (`clients/ts/node_modules/.bin`) so
   generation works offline/in CI with no call to the buf schema registry
   (`proto/buf.gen.yaml:6-12`'s comment). `clients/go/` should mirror this
   exactly: its own `go.mod` (a genuinely separate Go module, matching "two
   separate git repositories — no submodule/monorepo relationship" being
   already the norm for *this* repo's client/server split too), a
   `clients/go/gen/` output directory, and local plugin binaries
   (`protoc-gen-go` + `protoc-gen-connect-go`, both installable via `go
   install` into a repo-local `bin/` the same way `clients/ts` uses local
   `node_modules/.bin`).
2. **Connect-Go, not raw gRPC, matches both sides' existing choices.**
   tymux's TS client already committed to Connect (`@connectrpc/connect` +
   `@connectrpc/connect-node`, `clients/ts/package.json:19-21`) specifically
   because `tonic` alone can't serve gRPC-Web/browser clients (v1-release
   doc §3's "second, more consequential gap" — `tonic-web` was flagged as
   the needed addition if browser support matters). stapler-squad's Go
   module **already depends on `connectrpc.com/connect v1.19.0` directly**
   (`stapler-squad/go.mod:17`, plus `otelconnect` at line 18) — `grpc` itself
   is only an *indirect* dependency there (`go.mod:210`). Generating a
   Connect-Go client is therefore not introducing a new protocol stack to
   stapler-squad; it's reusing one already load-bearing in that codebase, and
   it plays correctly with `tonic-web` if/when tymux adds it for browser
   support, since Connect's wire protocol is what `tonic-web` (or a future
   Connect-compatible tonic layer) would need to speak anyway. Plain
   `google.golang.org/grpc`-generated stubs would still technically work
   (`tonic` serves real HTTP/2 gRPC today for non-browser clients, and
   stapler-squad's Go backend is a Node.js-equivalent case, not a browser)
   but would fragment the "one protocol family across every client" story
   the TS client already established, for no benefit stapler-squad's own
   dependency graph doesn't already pay for.
3. **Vendoring or a published-only module would break the "generation is
   part of this repo's build" model** the whole `buf.gen.yaml` design
   exists to support (`proto/buf.gen.yaml:6-12`'s explicit "works
   offline/in CI without a network call" goal) — publishing tymux's Go
   client as a standalone versioned module (e.g. tagged
   `clients/go/v0.x.y` via Go's multi-module-repo tagging convention) is
   still the *right* consumption story for stapler-squad (a real semver
   dependency, not a vendored copy it has to manually re-sync), but the
   generation itself belongs in tymux's repo, committed, exactly like
   `clients/ts/gen/` already is — not generated ad hoc inside
   stapler-squad against a proto tree it doesn't own.

### Concrete shape

- `proto/buf.gen.yaml` gains a second `plugins:` block (or a second buf
  template file) targeting `clients/go/gen/`, using locally-resolved
  `protoc-gen-go`/`protoc-gen-connect-go` binaries, mirroring the TS entry's
  `local:`/`out:`/`opt:` shape (`proto/buf.gen.yaml:15-18`).
- `clients/go/` gets its own `go.mod` (module path e.g.
  `github.com/tstapler/tymux/clients/go`), making it an independently
  versionable Go module within the same repo (standard Go multi-module-repo
  pattern — no submodule needed, satisfying the "no submodule/monorepo
  relationship" constraint since it's tymux's own repo gaining a second
  module, not a cross-repo submodule).
- stapler-squad consumes it as an ordinary `go.mod` `require`. During
  development (both repos evolving together, per requirements.md's
  "two-repo coordination drift risk"), a local `replace` directive pointing
  at a filesystem path to tymux's checkout is the standard Go pattern for
  iterating across an unpublished dependency; once tymux tags a release,
  stapler-squad switches to the tagged version and drops the `replace`.

---

## 5. Event-Command-Policy table: session lifecycle across both systems

This integration genuinely has multiple actors (stapler-squad's Go backend,
tymux's daemon, the pty child process, the browser/xterm.js) and real
business rules around ownership, disconnect, and reconnect — this warrants
the table.

Grammar: **Domain Event** (past tense) → **Policy** ("whenever X, then...") →
**Command** (imperative) → **Actor/System**.

| Domain Event | Policy trigger | Command | Actor/System |
|---|---|---|---|
| *(user picks "new agent session, tymux backend")* | — | `CreateSession` | stapler-squad backend (`BackendTymux.Start`) |
| SessionCreated (tymux) | whenever a session is created via `BackendTymux` | `Attach(pane_id)` — open the standing stream | stapler-squad backend |
| AttachStreamOpened | whenever the standing `Attach` stream opens | `SubscribeLocalFanoutChannels` (satisfy `SubscribeToControlModeUpdates` callers) | `BackendTymux` (Go, internal) |
| PaneOutputReceived (`AttachEvent.output`) | whenever output arrives on the standing stream | `RenderToXtermJS` (fan out over stapler-squad's existing WS/SSE transport to the browser) | stapler-squad backend → web-app/xterm.js |
| PaneOutputGapReceived (`AttachEvent.output_gap`) | whenever the daemon's broadcast channel drops frames for this consumer | `RenderOutputDroppedIndicator` | stapler-squad backend → xterm.js (new: no current tmux-path equivalent, since tmux's own `capture-pane` full-snapshot model has no analogous gap signal) |
| ClientAbruptlyDisconnected (browser tab closed / WS dropped) | whenever the browser side disappears | *(none — do NOT close the `Attach` stream)* | stapler-squad backend — the standing `Attach` stream is process-lifetime-scoped, not browser-tab-scoped; this is the deliberate design choice that sidesteps §3's disconnect bug for the *stapler-squad-initiated* disconnect path (only the CLI harness's OS-level pty-close reproduces it — a browser WS drop on stapler-squad's Go backend is a different failure mode with no open pty in the loop at all). |
| stapler-squad process itself restarts/crashes | whenever the backend process comes back up | `RestoreWithWorkDir` → `ListSessions` + `ReviveSession` if dead | stapler-squad backend (mirrors `TmuxBackend`'s own restart-reconciliation need — same shape, different RPCs) |
| PaneChildProcessExited (`AttachEvent.exited`, §2's new `ExitStatus`) | whenever the pane's child exits | fire `onExitCallback` (`SetOnExitCallback`'s registrant) | `BackendTymux` → stapler-squad's existing exit-handling path (already generic across backends, per `ProcessManager.SetOnExitCallback`'s interface-level contract) |
| AttachStreamErrored (non-`Exited` failure — network blip, daemon restart) | whenever the standing stream ends abnormally | `Attach(pane_id)` — reconnect, same pane_id, new stream | `BackendTymux` — must be implemented as a client-side reconnect loop; tymux's `Attach` is not itself reconnect-aware (detach = full cancellation, `tymux.proto:54-57`), so surviving a daemon restart or transient network failure without treating it as `Close()` is entirely `BackendTymux`'s responsibility, same as tmux's own control-mode client presumably already handles reconnect for its analogous case. |
| *(user explicitly kills the session/agent)* | — | `Close()` → `KillSession` | stapler-squad backend |
| SessionKilled (tymux) | whenever `KillSession` succeeds | `CloseLocalFanoutChannels` + fire final exit callback if not already fired | `BackendTymux` |
| *(daemon-side: pty child hangs up abruptly — §3's open bug)* | *(unresolved — no policy can be written until root-caused)* | *(none yet)* | — |

**Ownership note worth naming explicitly** (not a new row, a cross-cutting
rule): tymux has no auth/authorization model (requirements.md's explicit
out-of-scope), so "session ownership" in this integration is enforced
entirely on stapler-squad's side (its own session/instance registry already
maps an agent instance to a `ProcessManager`); tymux's daemon has no concept
of *which* stapler-squad user or instance a given `session_id` belongs to.
This is fine given the stated scope (no auth changes to tymux) but should be
stated as a known trust boundary: anything with network access to `tymuxd`
can `Attach`/`SendKeys` to any `pane_id`, same as it can with tmux's own
control-mode socket today.

---

## Summary of concrete recommendations

1. **`BackendTymux` shape**: thin `ProcessManager`-implementing adapter
   mirroring `TmuxBackend`'s delegation pattern, holding one standing
   `Attach` stream per pane for the `ProcessManager`'s whole lifetime, fanned
   out locally to satisfy `SubscribeToControlModeUpdates`'s multi-subscriber
   contract. Five flagged capability gaps (§1): no `cwd` in/out of
   `CreateSessionRequest`/`Pane`, no clean `GetPTY()` mapping, no `GetPanePID()`
   mapping, and `SetDetachedSize()` needing the standing-stream design to have
   anything to call.
2. **Exit status**: turn `AttachEvent.exited` from `bool` into an `ExitStatus`
   message (`has_code`/`code`), captured via `portable_pty::Child::wait()` in
   the pane reader thread (`pane.rs:217-241`, right after the EOF break,
   where the `Child` handle is already held but never waited on today) and
   threaded through the existing single `forward_handle` send site
   (`main.rs:497-503`). No new RPC; persist last-known code onto the dead
   `PaneEntry` record for post-detach reads.
3. **Disconnect bug**: everything at the Rust/gRPC application layer is
   already ruled out by prior investigation
   (`disconnect_survival_e2e.rs:63-136`). Most likely remaining culprit is
   process-group/session (`setsid`) inheritance at pane-spawn time
   (`pane.rs:178-182`) interacting badly with this specific sandboxed dev
   container's shared-cgroup, no-real-controlling-terminal shape — re-test on
   real hardware before any further root-causing.
4. **Go client**: new `clients/go/` in the tymux repo, buf-generated via
   Connect-Go (matching both tymux's TS client's protocol choice and
   stapler-squad's own already-direct `connectrpc.com/connect` dependency),
   mirroring `clients/ts/`'s generation-committed-to-repo model, consumed by
   stapler-squad as a tagged Go module (`replace`-directive during
   co-development).
5. **ECP table**: session lifecycle spans stapler-squad's backend, tymux's
   daemon, and the pty child; the standing-`Attach`-stream design is what
   makes "browser tab closes" not trigger the same abrupt-disconnect
   exposure as an OS-level pty hangup — those are different failure modes
   despite both being called "disconnect."
