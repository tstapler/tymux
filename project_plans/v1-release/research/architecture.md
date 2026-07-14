# Architecture Research: v1-release (splits, persistence, cross-language client)

**Phase**: 2 (Research) of the v1-release SDD workflow
**Input**: `project_plans/v1-release/requirements.md`
**Builds on**: `docs/reviews/is-it-ready-2026-07-13.md` (Architecture dimension, blocking
issues #6–#8), `docs/adr/0001-single-pane-per-session-for-now.md`, `docs/ux/journey-map.md`
(concurrent-attach gap, finding #6)

This document does not re-litigate the tuple-coupling issue (`is-it-ready` blocking
issue #6) — that's already fixed (`Engine::list_sessions` now returns typed
`SessionInfo`, not a positional tuple). It picks up exactly where ADR 0001 and
`is-it-ready` issue #7/#8 left off: the proto already models `Session → Window →
Pane` as `repeated`, the engine still hardcodes one of each, and requirements.md
now brings real splits back into scope.

---

## 1. Layout model for splits

### Recommendation: a binary layout tree per window, mirroring tmux's actual model

tmux does not lay out panes in a fixed grid — it recursively subdivides a
window's screen area into a binary tree of splits (`layout_cell` in tmux's own
source), where each internal node is a horizontal or vertical split with two
(or more) weighted children, and each leaf is a pane. This is the right model
to copy: it's proven, it supports arbitrary nested splits, and it maps cleanly
onto both server-side geometry computation and client-side rendering.

```rust
pub struct WindowState {
    pub id: Uuid,
    pub name: String,
    pub layout: LayoutNode,
    pub active_pane_id: Uuid,   // input focus within this window
}

pub enum LayoutNode {
    Leaf {
        pane: Arc<Pane>,
    },
    Split {
        orientation: Orientation,      // Horizontal = side-by-side, Vertical = stacked
        children: Vec<(LayoutNode, f32)>, // ratios; recompute integer rows/cols at layout time
    },
}
```

Store ratios (`f32`, sum to 1.0 per split), not absolute cell counts — tmux
itself keeps relative weights and re-derives absolute cell allocations on every
resize, rounding with the remainder pushed onto the last child. Do the same:
a `fn compute_geometry(&self, rows: u16, cols: u16) -> Vec<(Uuid, PtySize)>`
walk over `LayoutNode` is the one piece of real algorithmic work in this epic
(a simplified flexbox). Everything else is bookkeeping.

`SessionState` changes from:
```rust
pub struct SessionState { id, name, window_id, pane: Arc<Pane> }
```
to:
```rust
pub struct SessionState { id: Uuid, name: String, windows: Vec<WindowState>, active_window_id: Uuid }
```

`Engine::pane(pane_id)` (today: flat `find` over one field per session) becomes
a tree walk over every window's `LayoutNode` — the existing doc comment
("pane namespace is flat across sessions") already anticipated multiple panes
to search; it just needs to descend a tree instead of comparing one field.

New `Engine` operations required: `split_pane(pane_id, orientation, ratio)`,
`close_pane(pane_id)` (must cascade: closing a window's last pane closes the
window; closing a session's last window is a semantic `KillSession` — see the
invariants in §4 and the ECP table), `resize_window(window_id, rows, cols)`
(the tree-layout walk above).

### Integration points that change

- **`session_to_proto`** (`tymuxd/src/main.rs`) currently builds a fixed
  `windows: vec![one window with one pane]`. It needs to become a recursive
  walk over N windows × arbitrary layout trees. This also means **the proto
  needs a `Layout` message** — today's `Pane { id, rows, cols }` has no
  position/size-within-window information, so a client can't render splits
  from the wire format alone. Add:
  ```proto
  message Layout {
    oneof node {
      Pane leaf = 1;
      Split split = 2;
    }
  }
  message Split {
    Orientation orientation = 1;
    repeated LayoutChild children = 2;
  }
  message LayoutChild {
    Layout node = 1;
    float ratio = 2;
  }
  message Window {
    string id = 1;
    string name = 2;
    Layout layout = 3;   // replaces `repeated Pane panes`
  }
  ```
  This is a **breaking proto change** to `Window` (removes `repeated panes`),
  which is fine pre-1.0 but should happen in one deliberate pass, not
  incrementally, since `tymux-cli` and any generated non-Rust client both
  depend on the old shape.

- **`list_sessions`** (`Engine`) currently returns a flat `SessionInfo { pane_id,
  rows, cols, ... }` per session — this DTO needs to become tree-shaped too
  (mirror `LayoutNode` but with resolved `{id, rows, cols}` leaves instead of
  `Arc<Pane>`), otherwise the tuple-coupling problem `is-it-ready` already
  fixed once just gets reintroduced in a new shape.

- **CLI pane selection** (`tymux-cli/src/main.rs`'s `first_pane_id`) currently
  walks `windows[0].panes[0]`. Post-split this needs real addressing —
  recommend a `session:window.pane`-style target string (tmux's own
  convention) plus a `--pane`/interactive picker for `attach`, and a bound
  keystroke for "split current pane" once the config/key-binding epic exists
  (there's no other UI surface to trigger a split from otherwise).

### The one design decision this section surfaces that isn't free: what does `Attach` target once panes live in a tree?

Today `Attach`'s first message carries a `pane_id` and `Resize` resizes that
one pane's pty directly. Once panes belong to a shared window layout, a
client's terminal-resize event conceptually must resize the *whole window*
(recomputing every leaf), not just the one pane the client happens to be
looking at — and if two panes of the same window are attached by different
clients, "resize" needs window-level semantics, not pane-level.

**Recommendation**: keep `Attach` scoped to a single `pane_id` (minimizes
proto/client churn, and matches "one bidi stream per pty" — the same model a
real terminal emulator uses against N ptys it composites locally). Do **not**
try to multiplex several panes' output over one stream by tagging frames with
pane_id — that reinvents framing/backpressure semantics gRPC streaming already
gives you for free per-pane. Instead, add a **separate lightweight layout
subscription** (a new unary-poll or streaming `WatchWindow(window_id)` RPC)
that pushes `Layout` changes (splits added/removed, geometry changes) so a
client attached to multiple panes of one window can composite them into a
single screen with correct borders/positions. Resize events sent up the
existing per-pane `Attach` stream should be reinterpreted server-side as "this
client's viewport changed" and resolved through the window-level geometry
policy in §4, not applied 1:1 to the one pane's pty size.

---

## 2. Persistence architecture

### Reframing the ask (this changes the recommendation)

requirements.md's Feasibility Risks section is right that true live-process
resume "likely isn't achievable" — but it's worth being explicit about *why*,
because it reframes what's actually being asked for: **real tmux does not
survive its own server process dying either.** `kill -9 <tmux server pid>` (or
`tmux kill-server`) destroys every session tmux has, exactly like `tymuxd`
today. tmux's actual persistence guarantee is *client-disconnect* resilience
(detach/reattach while the server keeps running) — which tymux already has,
by virtue of being a client/server daemon in the first place. Nothing new is
needed for that case.

So "sessions survive a `tymuxd` restart" is a strictly *stronger* guarantee
than tmux itself provides, and it's specifically about the operational case of
upgrading/restarting the daemon binary (or recovering from a crash under a
supervisor) — not about surviving `kill -9` mid-command with pty state intact.
That's not achievable without CRIU-class OS checkpointing (Linux-only, no
macOS equivalent, heavyweight) and isn't worth pursuing for v1. This should be
stated explicitly in the plan/ADR as a **ruled-out non-goal**, not left as an
open stretch goal that invites scope creep later.

### Concrete recommendation

**What's saved**: session id/name/created-at; per window: id, name, layout
tree *shape* (split structure + ratios — not live content); per pane: id, the
spawn command string, rows/cols, and (best-effort) cwd if capturable. **Not
saved**: pty file descriptors, live vt100 screen/scrollback, child process
state — these die with the daemon process, unconditionally.

**Storage**: one JSON file per session under
`$XDG_STATE_HOME/tymux/sessions/<session_id>.json`, written atomically
(write-to-tmp + rename) on every *structural* mutation (session created/
renamed/killed; window created/closed; pane split/closed; layout resized) —
**not** on pty output. This is a deliberate choice over sled/sqlite: the data
volume is tiny (topology + metadata, not a terminal-output WAL), structural
mutations are naturally rare (interactive, not continuous), and a
human-readable JSON file is directly diagnosable when something goes wrong —
which the Observability Requirements section explicitly asks for ("corrupted
or lost persisted state is visible, not silent"). A parse failure on one
session's file logs a `tracing::warn!` and that session is skipped, not fatal
to daemon startup — one corrupt file can't take down the whole persistence
layer, which is a real advantage over a single shared KV-store file. This
requires adding `serde`/`serde_json` to the workspace — neither exists in
`Cargo.toml` today.

**Whole-session-record writes, never per-pane files** — this is a hard
constraint, not an implementation convenience (see the invariant in §4: it's
what makes cross-pane persistence tearing structurally impossible).

**Reconciliation on restart**: `tymuxd` scans the session directory at
startup, loads each valid record into `Engine`'s in-memory map as a
**dead-flagged** `SessionState` (no `Arc<Pane>` — no pty exists). This
requires a liveness concept that doesn't exist in the proto today (the same
gap `is-it-ready`'s non-blocking findings already flagged: "no liveness/exit-
code field anywhere in the proto"). `CapturePane` on a dead pane returns the
last-persisted snapshot if one was captured at shutdown/kill time, or an
explicit "pane is dead, no data" response — never a crash, and never
conflated with `NotFound` (see §4's dead-pane invariant). Restoring live
processes is **explicit and user-initiated**, never automatic on daemon
start: add a `tymux revive <session_id>` command that respawns fresh ptys
matching the persisted layout shape (same tree, same split geometry, new
shells). Auto-respawning on startup was considered and rejected — silently
re-running whatever command a dead pane last had (e.g. `vim` mid-edit) on
daemon start is surprising and not what a user restarting the daemon for an
upgrade actually wants.

---

## 3. Cross-language client integration point (`Attach`)

### Contract gaps to document explicitly (proto doc-comments, not just prose docs)

1. **Message-ordering contract**: the first `AttachRequest` on the stream
   *must* be the `pane_id` oneof variant; anything else is rejected with
   `invalid_argument`. This is currently a field-level comment
   (`tymux.proto:84`) buried inside the `oneof` — easy to miss. `protoc-gen-es`/
   Connect-ES render **service/RPC-level** doc comments most prominently in
   generated TS; move/duplicate this contract onto the `Attach` RPC's own doc
   comment in `tymux.proto`, since that's what a TS client author will
   actually read first.
2. **Detach = stream close, not a distinct message**: closing the gRPC call is
   the only detach signal today; there's no explicit "detach" `AttachRequest`
   variant. This should be true and is a reasonable design (it matches tmux's
   own "close the connection to detach" model) — but it needs to be stated
   explicitly, because a Connect-ES client author needs to know whether
   half-closing the write side alone is sufficient or whether they must fully
   cancel/abort the call. Recommend documenting: full call cancellation is
   the supported detach path; half-close alone leaves the output side
   subscribed indefinitely (today's `input_handle` task ending doesn't signal
   `forward_handle` to stop).
3. **No delivery guarantee on output** — `pane.rs`'s broadcast channel silently
   drops frames for a slow consumer (`Lagged`, capacity 1024) with **no signal
   on the wire today**. A client author coming from gRPC's usual "ordered,
   reliable stream" mental model will not expect this. Recommend closing the
   gap rather than just documenting around it: add an `output_gap: bool`
   variant to `AttachEvent.payload` so a client can render "[output dropped]"
   instead of silently showing a discontinuous screen. Small proto change,
   meaningfully strengthens the "structured, robust API" pitch that's this
   project's actual differentiator over tmux.
4. **Resize semantics need to be pinned down alongside §1's split decision**
   before it can be documented for cross-language clients at all — whichever
   choice is made (pane-scoped resize + separate window-geometry policy, per
   the §1 recommendation) needs to be unambiguous in the .proto, since this is
   exactly the kind of implicit assumption that's obvious from reading
   `engine.rs` but invisible to a client author who only has the generated
   types.

### Rust-side changes needed — and the one risk that isn't about `Attach`'s shape at all

`buf.gen.yaml` today has an **empty `plugins:` list** — literally nothing has
ever been generated for any non-Rust language. This means the real risk
requirements.md flags ("bidirectional-streaming codegen quality varies by
toolchain") is *secondary* to a more basic risk: **no TS code has ever been
generated or compiled against this proto at all.** Recommend sequencing the
epic to isolate the two risks: first populate `buf.gen.yaml` with
`protoc-gen-es` + `@connectrpc/connect-es` targeting `clients/ts/gen/` and get
a trivial **unary** RPC (`ListSessions`) compiling and running against a live
daemon — a cheap, early checkpoint that validates the toolchain setup in
isolation — *before* attempting `Attach`'s bidi stream, so a failure there is
legible as "bidi-streaming codegen has rough edges" rather than conflated with
"buf/connect setup is broken."

**A second, more consequential gap this surfaces**: requirements.md names
"future web frontends (e.g. stapler-squad) embedding tymux sessions" as an
explicit consumer. `tonic` serves standard gRPC (HTTP/2 + trailers) — a
**browser** cannot speak that directly (no trailers support in
fetch/XHR), so a browser-based TS client needs either gRPC-Web or Connect's
own protocol, neither of which `tonic` serves out of the box. A Node.js TS
client (not browser) works fine over real gRPC as-is. **This needs an explicit
planning-phase scoping decision, not an assumption**: if browser support
matters for v1's cross-language proof, add the `tonic-web` crate and wrap the
tonic service with it (real, existing crate — no proxy needed); if a Node.js
client is sufficient for v1 and browser support is deferred, that's a
legitimate scope cut, but it should be stated, since "prove the cross-language
claim" reads very differently depending on which target is chosen.

---

## 4. Consistency/data-flow invariants

With splits, persistence, and multiple concurrent attachers now interacting
simultaneously, the following need to be explicit, enforced invariants (test-
worthy, not just design intentions):

- **Layout-tree validity**: every mutation (split/close/resize) leaves a
  window's `LayoutNode` in a valid state — no orphaned pane references, no
  zero-child `Split` nodes, ratios sum consistently. Mutations must be atomic
  w.r.t. concurrent readers (`ListSessions`, `WatchWindow`). At today's scale
  the existing single `Mutex<HashMap<Uuid, SessionState>>` is sufficient, but
  it already serializes *all* sessions' operations — worth flagging as a
  lock-granularity decision to revisit once split/resize operations happen at
  keystroke-adjacent frequency across many concurrently-active panes, rather
  than assuming it stays fine by default.
- **No per-pane persistence — whole-session-record writes only.** Since a
  session's entire record (all its windows, all their layout trees, all
  panes) is written as one atomic file replace (§2), torn/partial persisted
  state *within* a session — e.g. one pane's split reflected in the persisted
  file but its sibling's not — is structurally impossible, not just avoided
  by convention. This directly answers the question of whether two panes in
  the same window could be independently persisted/restored inconsistently:
  they can't, provided this constraint is never violated by a future
  optimization (e.g. "just persist the one pane that changed" must be
  rejected if proposed later).
- **Dead-pane vs. never-existed must not collapse to the same error.**
  `Engine::pane(pane_id)` needs three outcomes, not two: live pane, dead pane
  (persisted record, process ended — see §2), and unknown id (`NotFound`). A
  client (human or AI agent) retrying on `NotFound` is reasonable; retrying
  against a dead pane forever is not — the proto needs to distinguish these
  (ties into the same liveness-field gap `is-it-ready` already flagged).
- **Close cascades, enforced at the Engine layer, not left to callers**:
  closing a window's last pane closes the window; closing a session's last
  window is semantically a `KillSession` (mirrors tmux's own cascade). This
  must be enforced inside `close_pane`/`close_window`, not by convention at
  call sites — the same class of bug ADR 0001 already flagged for unchecked
  `windows[0].panes[0]` indexing.
- **Splitting a pane must never invalidate an already-open `Attach` stream**
  for that pane or its uninvolved siblings — a split changes geometry
  (rows/cols) for affected leaves and may trigger a `Resize` down to the pty,
  but the pane's `Uuid` and its live `Arc<Pane>`/pty survive. This should be
  an explicit, test-covered invariant: attach-in-flight during a concurrent
  split/close of *other* panes in the window must be unaffected.
- **Concurrent-attacher geometry policy** (this is the same gap
  `docs/ux/journey-map.md` finding #6 already named — "no shared/arbitrated
  pty size across concurrent attachers" — now sharper once a window's panes
  can be attached independently by different clients): recommend adopting
  tmux's own policy explicitly — window geometry is the **minimum** of all
  currently-attached clients' viewport sizes ("smallest client wins"), so no
  attached client's view is ever clipped. A client's `Resize` message is
  interpreted as "this client's viewport changed," triggering a
  recomputation across all attachers of that window, not a direct 1:1 pty
  resize. Last-write-wins was considered and rejected — it would fight
  between two attached clients on every resize event.

---

## Event-Command-Policy table (EventStorming)

Grammar: **Domain Event** (past tense — something that happened) triggers a
**Policy** ("whenever X, then...") which issues a **Command** (imperative)
performed by an **Actor/System**. Grouped by the bounded contexts this
surfaces: Session Lifecycle, Window/Layout, Attach/Streaming (concurrency),
Persistence/Restart, Cross-Language Client Integration.

| Domain Event | Policy trigger | Command | Actor/System |
|---|---|---|---|
| *— (user intent)* | — | `CreateSession` | Human (CLI) / AI agent |
| SessionCreated | whenever a session is created | `PersistSessionRecord` | Persistence subsystem |
| SessionCreated | whenever created via `tymux new` | `Attach(pane_id)` | CLI (Human) |
| SessionKilled | whenever a session is killed | `KillAllPanesInSession` | Engine |
| SessionKilled | whenever a session is killed | `DeletePersistedRecord` | Persistence subsystem |
| *— (user/agent intent)* | — | `SplitPane(pane_id, orientation, ratio)` | Human / AI agent |
| PaneSplit | whenever a window's layout changes | `RecomputeWindowGeometry` | Engine |
| PaneSplit | whenever a window's layout changes | `PersistSessionRecord` | Persistence subsystem |
| PaneClosed | whenever a window's last pane closes | `CloseWindow` | Engine |
| WindowClosed | whenever a session's last window closes | `KillSession` | Engine |
| PaneChildProcessExited | whenever a pty child exits (reader thread hits EOF) | `EmitAttachEventExited` | Engine (pty reader thread) |
| PaneChildProcessExited | *(open policy — see below)* | *(none yet)* | — |
| AttachRequested(pane_id) | whenever attach is requested | `ValidatePaneLiveness` → `SubscribeToPaneOutput` | Engine |
| ClientAttached | whenever a new attacher joins an already-attached window | `RenegotiateWindowGeometry` (min of all viewports) | Engine |
| ClientViewportResized | whenever any attached client's terminal size changes | `RecomputeWindowGeometry` (smallest-wins policy) | Engine |
| ClientDetached (stream closed) | whenever the last attacher of a pane detaches | *(none — pane keeps running, matches tmux background sessions)* | — |
| OutputBufferLagged (broadcast overrun) | whenever a slow consumer overruns the output channel | `EmitAttachEventOutputGap` *(proposed new event — not built yet)* | Engine |
| DaemonStarting | whenever `tymuxd` starts | `LoadPersistedSessionRecords` | Persistence subsystem |
| PersistedSessionLoaded | whenever a record loads with no live pty | `MarkSessionDead` | Engine |
| PersistedRecordCorrupted | whenever a session file fails to parse | `LogPersistenceWarning` + skip | Persistence subsystem |
| *— (explicit user action)* | — | `ReviveSession(session_id)` | Human (CLI) |
| SessionRevived | whenever a dead session is revived | `RespawnPanesFromPersistedLayout` | Engine |
| *— (client/CI action)* | — | `RunBufGenerate` (populate `buf.gen.yaml` plugins) | Client author / CI |
| TsClientGenerated | whenever the TS client is (re)generated | `RunCrossLangSmokeTest` (unary first, then `Attach`) | CI |

**Open policy worth flagging explicitly**: `PaneChildProcessExited` currently
has no follow-on policy at the layout-tree level — a dead pane just stays in
the tree as an unreachable leaf with an `Exited` event already sent to
attachers. Real tmux defaults to *removing* the pane's slot (`remain-on-exit`
is opt-in, off by default). Recommend the same default for v1: a window whose
one live pane exits and it was the window's last live pane should cascade
per the close invariant in §4, rather than leaving a permanently-dead leaf
sitting in the layout with no user-facing way to clear it — this should be
resolved as a planning-phase decision, not left implicit.

---

## Summary of concrete recommendations (not options)

1. **Splits**: binary `LayoutNode` tree per window (tmux's real model), ratio-
   based children, resolved to absolute rows/cols at layout time. `Attach`
   stays pane-scoped (one bidi stream per pty); add a separate `WatchWindow`
   layout-subscription RPC for compositing. Requires a breaking `Window`
   proto change (`repeated panes` → `Layout`).
2. **Persistence**: metadata-only, JSON-per-session under `$XDG_STATE_HOME`,
   written atomically on structural mutations only (never on pty output).
   Live-process resume is explicitly ruled out (not a stretch goal). Restart
   loads dead-flagged records; reviving is an explicit user action
   (`tymux revive`), never automatic.
3. **Cross-language client**: proto doc-comment strengthening + a new
   `output_gap` signal are the concrete Rust-side changes worth making.
   Sequence the epic to validate the buf/Connect toolchain on a unary RPC
   before `Attach`. Explicitly decide (don't assume) whether v1's cross-
   language proof needs browser support — if yes, `tonic-web` is a required
   dependency, since `tonic` alone cannot serve a browser client.
4. **Invariants**: whole-session-record persistence writes make cross-pane
   persistence tearing structurally impossible — keep it that way. Adopt
   tmux's "smallest attached client wins" geometry policy for concurrent
   attachers. Dead-pane and never-existed must be distinguishable error
   states, not both `NotFound`.
