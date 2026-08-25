# Architecture Research: attach-resume-protocol

Agent 3 (Architecture), SDD Phase 2. Builds on prior architecture research
covering the same core files — cited by file:line rather than re-derived:
- `project_plans/v1-release/research/architecture.md` (§3, "Cross-language
  client integration point (`Attach`)" — the broadcast-channel-drops-frames
  gap and the `output_gap` design, lines 239-244, 370)
- `project_plans/stapler-squad-integration/research/architecture.md`
  (`AttachEvent`/`AttachRequest` proto shape, ExitStatus precedent for
  bool→message field changes, lines 138-176; the standing-`Attach`-stream /
  reconnect-loop design, lines 356-363)

## Baseline confirmed by reading the code

- `Pane` (`crates/tymux-core/src/pane.rs:75-127`) owns `output_seq:
  AtomicU64` (line 111) and `output_tx: broadcast::Sender<(u64, Vec<u8>)>`
  (line 103, capacity `OUTPUT_CHANNEL_CAPACITY = 1024`, line 66). There is
  exactly **one** production write site: the pty reader thread
  (`pane.rs:234-277`), which bumps `output_seq` and calls `output_tx.send`
  inside the *same* `parser` mutex critical section as `parser.process(..)`
  (lines 246-255) — this is what already guarantees `output_seq` and the
  grid state (and, by extension, anything else written in that block) can
  never be observed out of sync (documented at `pane.rs:104-110`, "Task
  1.3.1a").
- `Engine` (`crates/tymux-core/src/engine.rs:183-185`) owns
  `panes: Mutex<HashMap<Uuid, PaneEntry>>` — pane lifetime is **already**
  fully decoupled from any `Attach` stream's lifetime (this is Epic 1.1's
  disconnect-survival fix, confirmed structurally, not just by doc claim).
  A pane is torn down only by explicit `ClosePane`/`KillSession`, never by
  an `Attach` stream ending. This matters for the "grace period" question
  below.
- `tymuxd`'s `attach()` handler (`crates/tymuxd/src/main.rs:614-778`)
  already implements the exact "read a live-channel subscription plus a
  point-in-time read, then dedup by seq" pattern this feature needs to
  generalize:
  - `output_rx = pane.subscribe()` happens **before**
    `pane.snapshot_with_seq()` (main.rs:659-660), with an explicit comment
    citing ADR-003/Task 1.3.1b: subscribing first guarantees no output
    produced between the two calls is lost.
  - `forward_step_for_output_result` (main.rs:325-349) is a pure,
    unit-tested function that maps one `output_rx.recv()` result to
    `ForwardStep::{Emit, Skip, End}` (main.rs:305-314), given a
    `snapshot_seq: u64` threshold: any chunk with `seq <= snapshot_seq` is
    `Skip`ped (already reflected in the priming snapshot), everything else
    is `Emit`ted, `Lagged` becomes `OutputGap`, `Closed` ends the stream.
  - `disconnect_tracker: Arc<Mutex<HashMap<Uuid, Instant>>>`
    (main.rs:47) is inserted into when the input stream ends on a still-live
    pane (main.rs:762-767) — but it is **only** consumed by
    `warn_if_exit_follows_disconnect` (main.rs:241, a regression-detection
    log heuristic), and purged on `ClosePane`/`KillSession`
    (main.rs:420, 520). There is no timer, no active eviction, and no
    grace-period enforcement today — it's passive observability state, not
    a lifecycle gate.

## Q1: Should the replay ring buffer live inside `Pane`, or as a separate daemon-owned component?

**Recommendation: inside `Pane`, written synchronously in the same critical
section as `output_tx.send`/`output_seq`'s bump (`pane.rs:246-255`).** This
isn't a preference for cohesion over separation — it's forced by the same
correctness argument that already put `output_seq` there (Task 1.3.1a):

- If the buffer instead lived in `tymuxd` and was populated by its own
  `pane.subscribe()` call, it would be a *second*, independent
  `broadcast::Receiver` — subject to its own `Lagged` under load, which
  reintroduces exactly the kind of loss the replay buffer exists to
  prevent (nothing but the pty reader thread's own critical section can
  write without ever missing a chunk).
- If the buffer lived in `tymuxd` but was populated via a callback `Pane`
  invokes, that's the same design with an extra indirection — `Pane` still
  has to own the call site, so the type may as well live where the write
  happens.
- Bounding the buffer's memory needs a per-pane budget, and tymux already
  has exactly this shape for scrollback: `DEFAULT_SCROLLBACK_LINES` /
  `MIN_SCROLLBACK_LINES` / `GLOBAL_SCROLLBACK_BUDGET_LINES` with
  `allocate_scrollback_budget`/`release_scrollback_budget`
  (`pane.rs:15-54`), granted at spawn and released on `Drop`. The replay
  buffer should follow the identical pattern — a global byte (not slot
  count) ceiling, degrading the per-pane grant under pressure rather than
  evicting other panes' data, released on `Drop` — which only makes sense
  as a `Pane`-owned field.
- **Byte-size accounting, not slot count**: `OUTPUT_CHANNEL_CAPACITY`
  (1024) bounds the broadcast channel by *slot count*, and each slot can
  hold up to `PTY_READ_BUF_SIZE` (4096) bytes — an already-accepted,
  pre-existing worst case of ~4MB per attached pane. The new buffer's
  explicit bounded-memory NFR (ties to the 1,000-pane load-test precedent)
  means it should evict by total bytes, popping from the front until under
  budget — a straightforward `VecDeque<(u64, Bytes)>` with a running byte
  count, not a fixed slot count.

Testability is not lost by this placement: the *pure eviction/threshold
logic* (given a capacity and a sequence of pushes, what's retained, what's
the oldest retained seq) can be unit-tested in isolation exactly the way
`forward_step_for_output_result` already is (`main.rs:1431-1466`) — the
type can be a small standalone struct with pane-independent tests even
though its one instance lives on `Pane`.

## Q2: Integration with `forward_step_for_output_result`/`ForwardStep` — no new enum variants needed

The existing design already generalizes almost for free. `snapshot_seq` is
just a threshold parameter; `forward_step_for_output_result`'s signature
doesn't care whether that threshold came from a `CapturePane`-style grid
snapshot or a replay buffer's tail. Recommended shape:

1. Rename the threshold's *role* (not necessarily the field, but the
   mental model) from "the snapshot's seq" to "the priming threshold" —
   whatever was sent to the client as history (full grid or replayed
   bytes), everything `<= threshold` on the live channel is a duplicate
   and gets `Skip`ped. No change to `ForwardStep` or
   `forward_step_for_output_result` is required for this part.
2. In `attach()` (main.rs:659-673), branch on whether the first
   `AttachRequest` carries a resume field:
   - **No resume token** (today's path, unchanged): `subscribe()` →
     `snapshot_with_seq()` → send `Snapshot` event → threshold =
     `snapshot_seq`.
   - **Resume token present**: `subscribe()` (still first, same
     ordering guarantee) → read the replay buffer's current contents
     *and* tail seq in one atomic read (mirroring `snapshot_with_seq()`'s
     "read both together" shape) → if `resume_from_seq` is older than the
     buffer's oldest retained seq, send a new `GapExceeded` `AttachEvent`
     payload instead of replaying anything (the explicit fallback signal
     the requirements doc calls for — a genuinely new proto variant,
     analogous to how `ExitStatus` replaced a bare `bool`,
     `stapler-squad-integration` architecture doc lines 138-176) → else,
     emit each buffered chunk with `seq > resume_from_seq` as `Output`
     events, then set threshold = buffer's tail seq at read time.
3. The live-loop `select!` (main.rs:689-717) is unchanged — it already
   just needs a `pane_id` and a threshold `u64`; both paths feed it the
   same way.

This means the acceptance criterion "replay from buffer, then transition
to live broadcast subscription without a gap or duplicate" is satisfied by
reusing the exact mechanism already proven for
snapshot→live-channel handoff, not by inventing a new state machine.

## Q3: Data-flow / consistency at the handoff — same race, same fix, already proven

The classic "read snapshot then subscribe" race is real in the naive
ordering (read buffer tail, *then* subscribe — anything produced in
between is lost forever, since a fresh `broadcast::Receiver` only sees
items sent after it subscribes, confirmed by `subscribe()`'s doc,
`pane.rs:357-359`, and the requirements doc's own baseline section).
tymux already solved this shape once and the fix transfers directly:

- **Order**: `subscribe()` **before** reading the replay buffer's tail —
  identical to the existing `subscribe()`-before-`snapshot_with_seq()`
  ordering (main.rs:659-660, comment explicitly cites this as
  ADR-003/Task 1.3.1b's guarantee). Any chunk produced between the two
  calls is *not* lost (already flowing into `output_rx`), and *may* also
  appear in the buffer-tail read — but that's a duplicate, not a gap, and
  duplicates are exactly what the `seq <= threshold` skip in
  `forward_step_for_output_result` already exists to absorb. Zero-gap
  comes from subscribe-first; zero-duplication comes from reusing the
  threshold-skip, not from trying to make the two reads perfectly atomic
  with each other.
- **One additional invariant this feature needs that Epic 1.3 didn't**:
  the replay buffer's own append must happen in the *identical* critical
  section as `output_tx.send`/`output_seq`'s bump (`pane.rs:246-255`), not
  just "close to it." If the buffer were appended outside that lock, a
  resuming reader could read `output_seq` and the buffer's tail as
  disagreeing values (buffer behind the broadcast channel), which would
  make its own gap-detection (`resume_from_seq` vs. oldest-retained-seq)
  unreliable. Reusing the *same* mutex-guarded block that already
  serializes `output_seq`+`parser.process`+`output_tx.send` extends the
  existing "no separate detection path" invariant
  (`pane.rs:259-264`'s comment on exit detection makes the same point for
  a different field) to a third piece of state, at zero extra lock
  contention since it's the same critical section, not a new one.

## Q4: Per-subscriber cursor state — recommend none, given the self-contained-token lean

The requirements doc's own Rabbit Holes section leaves "self-contained
resume tokens vs. server-tracked client identity" as an explicit Phase 3
open question, leaning toward self-contained. This research confirms that
lean is the structurally cheaper option, and traces exactly what it buys:

- Every `Attach` call today is already a fully independent, anonymous task
  — `output_rx`, `forward_handle`, `input_handle` are all per-call local
  state (main.rs:659-773); nothing server-side currently tracks "which
  client is this." A self-contained resume token (just `resume_from_seq:
  u64`, or `u64` + a pane-generation guard — see below) preserves this
  shape exactly: no new `HashMap<ClientId, Cursor>` on `Pane` or `Engine`,
  no new identity-correlation logic, no new failure mode for "the daemon
  thinks this is client X reconnecting but it's actually client Y."
- The one shared, new piece of server state is the replay buffer itself —
  **pane-level, not per-subscriber** — exactly like `output_tx`/
  `output_seq` are already pane-level, shared, and independently consumed
  by N concurrent `broadcast::Receiver`s today. Multiple clients attaching
  concurrently each independently compute their own skip-threshold from
  the one shared buffer; no cross-client coordination is needed, matching
  the existing multi-attacher design (ADR-004's viewport-minimum policy is
  the one place multi-attacher *does* need coordination, and it's already
  handled — `report_viewport_and_recompute`, main.rs:734-745 — orthogonal
  to this feature).
- **Caveat worth flagging for Phase 3, not resolved here**: a
  fully-anonymous `u64` seq token has one gap — if a pane is closed and a
  *new*, unrelated pane later reuses the same daemon (different `Uuid`,
  so this is actually already excluded — pane_id is a UUID, not reused).
  So no generation guard is actually needed; a bare `resume_from_seq: u64`
  keyed by the already-unique `pane_id` is sufficient and self-contained.

## Q5: Grace period — what does it actually gate? (flagged for Phase 3, not assumed)

The requirements doc frames the grace period as "making 'orphaned but
still running, resumable' a first-class window, not best-effort." Reading
the code surfaces a mismatch worth resolving explicitly in Phase 3 rather
than carrying an unstated assumption forward:

- The **pane's own survival is already unconditional** (Epic 1.1,
  confirmed above: `Engine.panes` ownership is fully independent of any
  `Attach` stream). There is no pane-lifecycle cleanup today that a grace
  period could delay — a pane is never torn down on disconnect, abrupt or
  otherwise, only on explicit `ClosePane`/`KillSession`.
- The **replay buffer's retention is capacity-bounded, not time-bounded**
  (Q1) — it doesn't need a disconnect-triggered grace period either; it
  just holds its fixed byte budget of recent output regardless of whether
  anyone is attached, and a reconnect within that byte-budget's effective
  time window (which varies with output volume — a busy pane's buffer
  covers less wall-clock time than an idle one's) succeeds or falls back
  to `GapExceeded` on its own.
- What **does** happen immediately on stream end today, with no grace
  period at all: `AttachedGaugeGuard`'s drop (gauge decrement),
  `disconnect_tracker`'s insert (passive, not itself a cleanup), and —
  concretely — `unregister_viewport` + `recompute_window_geometry`
  (main.rs:768-771), which drops the disconnecting client's reported
  viewport from ADR-004's per-window minimum-size calculation right away.
  For a window with multiple attachers, an abrupt drop followed by a
  prompt reconnect could cause a visible geometry thrash (window grows
  back to fill the gap, then shrinks again on reconnect) that a grace
  period on `unregister_viewport`/`recompute_window_geometry` — not on
  pane or buffer lifecycle — would actually smooth over.
- **Recommendation**: Phase 3 should explicitly decide the grace period's
  referent is the *viewport-registration/geometry-recompute* path, not
  pane or replay-buffer lifecycle (both already handled by other,
  non-time-based mechanisms) — otherwise "grace period" risks being built
  as inert plumbing that gates nothing, since the two things one might
  naively assume it protects are already protected by design, not by a
  timer.

## Event-Command-Policy table

This qualifies per the "stream reconnect protocol with buffer eviction and
fallback signaling" criterion — multiple actors (client, pty reader
thread, `forward_handle` task, `input_handle` task, a new heartbeat/grace
mechanism) and multiple policies (replay-vs-fallback decision, eviction,
heartbeat-driven disconnect detection) interact, not a single CRUD path.

| Event | Trigger | Command/Policy | Actor |
|---|---|---|---|
| AttachRequested(pane_id, resume_from_seq?) | client opens `Attach`, first message | `ResolveLivePane` → `SubscribeThenReadPrimingState` (Q3's ordering) | `tymuxd::attach()` |
| OutputProduced(seq, bytes) | pty reader thread reads a chunk | `AppendToReplayBuffer` + `BumpOutputSeq` + `BroadcastToLiveSubscribers`, one critical section (Q1/Q3) | `Pane` reader thread |
| ReplayBufferCapacityExceeded | buffer's byte budget exceeded on append | `EvictOldestChunks` until under budget | `Pane` reader thread (same critical section) |
| ResumeRequestInWindow | `resume_from_seq >= buffer.oldest_retained_seq` | `ReplayBufferedChunks(seq > resume_from_seq)` then `TransitionToLiveThreshold(buffer.tail_seq)` | `forward_handle` setup (main.rs:659-673 equivalent) |
| ResumeRequestGapExceeded | `resume_from_seq < buffer.oldest_retained_seq` | `EmitGapExceeded` (new `AttachEvent` payload) — client's own responsibility to fall back to `CapturePane` | `forward_handle` setup |
| ClientHeartbeatMissed | app-level ping (if chosen over/alongside tonic keepalive, per Rabbit Holes) times out | `MarkStreamSuspect` → start grace-period clock | new: heartbeat monitor task |
| GracePeriodExpired | grace-period clock elapses with no reconnect | `DeferredUnregisterViewport` + `RecomputeWindowGeometry` (Q5) + `info`-log per Observability Requirements | `tymuxd` (background or lazily on next geometry-affecting event) |
| StreamEndedGracefully | client closes inbound half deliberately | today's existing path unchanged: `disconnect_tracker.insert` + immediate `unregister_viewport`/`recompute_window_geometry` (main.rs:762-771) — Phase 3 should decide whether "graceful detach" also gets the grace period or bypasses it | `input_handle` task |
| ClientReconnected(pane_id, resume_from_seq) | new `Attach` call, same `pane_id` | independent new task, no server-side identity correlation (Q4) — `AttachRequested` again | client |
| PaneChildProcessExited (pre-existing, unmodified) | reader thread hits EOF | `EmitAttachEventExited`, `warn_if_exit_follows_disconnect` (unchanged) | `Pane` reader thread / `forward_handle` |

## Summary of concrete recommendations for Phase 3

1. Replay ring buffer is a `Pane`-owned field, appended in the pty reader
   thread's existing `output_seq`/`output_tx.send` critical section
   (`pane.rs:246-255`), evicted by byte budget (not slot count) using the
   same global-ceiling-with-graceful-degradation pattern as
   `allocate_scrollback_budget` (`pane.rs:31-54`).
2. No new `ForwardStep` variants or changes to
   `forward_step_for_output_result`'s signature — reuse the
   `seq <= threshold` skip verbatim; only add a pre-loop branch in
   `attach()` for resume-token handling and one new `AttachEvent` payload
   (`GapExceeded`) for the fallback signal.
3. Subscribe-before-read ordering (already proven for
   `snapshot_with_seq()`) transfers directly to the replay buffer's tail
   read — gap-free by ordering, duplicate-free by the existing
   threshold-skip, no new race to solve.
4. No per-subscriber server-side cursor state — self-contained
   `resume_from_seq: u64` tokens keyed by the already-unique `pane_id`
   fit the existing per-`Attach`-call-is-independent architecture with
   zero new identity-tracking.
5. Phase 3 must explicitly define what the "grace period" gates — pane
   lifecycle and replay-buffer retention are already handled by other,
   non-time-based mechanisms (Epic 1.1 unconditional pane survival, byte
   capacity eviction); the plausible real referent is deferred
   `unregister_viewport`/`recompute_window_geometry`, to avoid visible
   window-geometry thrash on a brief drop-then-reconnect.
