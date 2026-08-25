# Pitfalls Research — `attach-resume-protocol`

Research agent 4 of the SDD research phase. Scope: known failure modes for
resumable/replayable streaming protocols in Rust/tokio/tonic, grounded in
this codebase's own precedent bugs and current locking/concurrency shape.

## 1. Lock-ordering / deadlock risk from the new replay-buffer lock

**Direct precedent in this repo**: `project_plans/stapler-squad-integration/implementation/plan.md:81`
documents a real deadlock caught by two independent review agents in Epic
1.4's `WindowIndex` work — the initial implementation used *two* separate
mutexes (`pane_window`, `window_session`), and two call sites acquired them
in opposite order (`kill_session`: window_session-then-pane_window;
`close_pane`: the reverse while still holding the first). Under `Engine`
being `Arc`-shared across concurrent tonic RPC handlers, this is a genuine
deadlock, not a hypothetical. The fix that shipped: collapse both maps into
**one type behind one lock** (`WindowIndex`), making the wrong-order bug
structurally impossible rather than a discipline to remember (`plan.md:79`
finds the fixed code at `engine.rs` `window_id_for_pane`/`recompute_window_geometry`).

**Where this bites the resume feature.** `Pane` (`crates/tymux-core/src/pane.rs:75-127`)
already holds four separate `std::sync::Mutex`es plus two atomics
(`parser`, `writer`, `master`, `exit_code`, `_child`, `_reader_handle`,
`output_seq`/`exited` as atomics). The pty reader thread's hot loop
(`pane.rs:234-277`) already threads through two of them in a fixed order:
lock `parser` → bump `output_seq` inside that same guard → drop the guard →
`output_tx.send(...)` outside any lock (`pane.rs:246-255`, called out
explicitly in the comment as deliberate: "the sequence bump happens inside
the same `parser` lock guard as `process(..)`"). Any new per-pane replay
ring buffer needs its writes fed from this exact call site (it's the only
place `(seq, bytes)` chunks are produced), which creates two realistic
ordering hazards:

- **Reader thread vs. resume-read path.** If the new buffer lock is taken
  *inside* the `parser` lock on the write side (to keep buffer-push
  atomic with the seq bump, mirroring the `output_seq`/`parser` pairing
  the code already relies on for consistency — see `pane.rs:431-435`'s
  comment on why `seq` is read under `parser`'s lock), then **every** read
  path that also needs both — e.g. a resume handler that snapshots the
  buffer's contents and wants to cross-check against `parser`'s current
  seq to decide "is the requested seq still live or should I fall through
  to a live subscribe" — must acquire them in the same `parser`-then-buffer
  order, never buffer-then-`parser`. Recommend the same fix pattern
  `WindowIndex` used: don't create a second independently-locked structure
  whose acquisition order can drift across call sites — either (a) put the
  ring buffer *inside* the same mutex `parser` already uses (one lock, buffer
  push happens in the same critical section as the seq bump, no new
  ordering pair exists), or (b) if it must be a separate lock for
  contention reasons, document and enforce (via a lint comment, not just a
  code comment) a single fixed acquisition order, and audit every call site
  that touches both before merging, the way this fix should have been
  caught the first time.
- **Engine-level locks combined with the buffer lock.** `attach()`
  (`crates/tymuxd/src/main.rs:614-719`) already calls `self.engine.window_id_for_pane(pane_id)`
  (which now goes through the O(1) `WindowIndex` lock) *and* touches
  `pane.subscribe()`/`pane.snapshot_with_seq()` in the same function, plus
  `disconnect_tracker` (a `Mutex<HashMap<Uuid, Instant>>`, `main.rs:47`) in
  both the forward and input tasks. If a new "grace-period tracker" for
  resume (see §4) or a resume-cursor map is added as yet another
  independently-locked structure touched from the same call paths as
  `disconnect_tracker`/`WindowIndex`/the buffer lock, the number of
  pairwise lock-order combinations to audit grows combinatorially. Prefer
  reusing `disconnect_tracker`'s existing pattern (one `Mutex<HashMap<...>>`,
  never held across an `.await`, always locked-mutated-unlocked in a single
  statement — see `purge_disconnect_tracker`, `main.rs:142-144`) rather than
  introducing a structurally new locking shape.
- **Never hold the new lock across an `.await`.** Every existing lock in
  this codebase (`parser`, `writer`, `master`, `exit_code`, `disconnect_tracker`)
  is a `std::sync::Mutex` acquired and released within a single non-async
  statement — never held across a `tonic::Streaming`/`mpsc::Sender::send`
  await point. `forward_tx.send(...).await` (`main.rs:694,714`) happens
  *after* any relevant lock is already dropped. A resume/replay
  implementation that reads from the ring buffer and streams each chunk to
  the client must copy out from the buffer under the lock, drop the lock,
  *then* `.await` the send — locking across an await here would block every
  other pane operation (writes, resizes, other attach's reads) for the
  duration of one client's network I/O, and if the client is slow (§4),
  that's an unbounded hold.

## 2. Ring-buffer eviction boundary bugs

The requirements describe a fixed-capacity per-pane ring buffer decoupled
from the existing `broadcast::channel(1024)` (`pane.rs:66,205`). Two classes
of off-by-one risk are specific to this design, both centered on the
existing `output_seq: AtomicU64` (`pane.rs:111`, incremented via
`fetch_add(1, ...) + 1` at `pane.rs:249`, i.e. **1-indexed**, not
0-indexed — the first chunk is seq `1`, not `0`. `snapshot_with_seq`'s
initial value before any output is `0`, meaning "seq 0" is a sentinel
"nothing yet," not a real chunk. Any eviction/availability check that
assumes 0-indexing will be off by one against every other seq comparison
in the codebase, including the existing `seq <= snapshot_seq` dedup check
at `main.rs:333`).

- **"Is seq N still available" at the eviction boundary.** A ring buffer
  of capacity *C* holding the most recent chunks will, after evicting,
  have some oldest retained seq `oldest`. The correct availability check is
  `requested_seq >= oldest && requested_seq <= output_seq.load()`, but the
  classic bug is computing `oldest` from `latest - C` using **chunk count**
  when the buffer's real unit is **bytes** (each `(seq, Vec<u8>)` entry has
  a highly variable byte length — a single pty read can be anywhere up to
  `PTY_READ_BUF_SIZE`, so "last C entries" and "last C bytes" evict at very
  different points). Requirements should pin down whether capacity is
  bounded by entry count, byte count, or both, and the eviction check
  needs to be tested exactly at `requested_seq == oldest` (should succeed)
  and `requested_seq == oldest - 1` (should correctly report gap-exceeded,
  not silently under/overflow — `oldest - 1` when `oldest == 0` is a
  `usize`/`u64` underflow panic if not written as a checked/saturating
  subtraction).
- **Concurrent eviction during an in-progress resume read.** The pty
  reader thread is the sole writer (`pane.rs:234-277`, its own OS thread,
  not a tokio task) and can push+evict into the ring buffer at any time,
  including *while* a resume handler is mid-iteration copying a range out
  to send to the reconnecting client. If the buffer read takes a
  `Vec`-copy snapshot under the lock (recommended — mirrors
  `snapshot_at_offset_with_seq`'s existing pattern of copying the whole
  grid out under `parser`'s lock rather than streaming it out from inside
  the lock, `pane.rs:396-446`) this class of bug is structurally avoided.
  If instead the resume handler iterates the live buffer chunk-by-chunk
  while re-acquiring the lock per chunk (to avoid holding it for a long
  replay), it must re-validate on every iteration that the seq it's about
  to read hasn't just been evicted out from under it mid-replay — a lock
  that's dropped and reacquired between reads doesn't protect against
  concurrent mutation in the gap, unlike a single held-for-the-whole-copy
  lock.
- **The existing dedup precedent uses a different boundary rule** —
  `forward_step_for_output_result`'s `seq <= snapshot_seq → Skip`
  (`main.rs:332-334`, tested explicitly at the `<=` boundary by
  `attach_should_not_emit_output_gap_event_when_consumer_keeps_pace` and
  the snapshot-seq boundary tests around `main.rs:1426-1466`). A resume
  handler reusing `output_seq` semantics should use the *same* `<=`
  convention for "already delivered, skip" to avoid introducing a second,
  subtly different off-by-one convention for the same counter elsewhere in
  the codebase.

## 3. Resource exhaustion from the new buffer at 1,000-pane scale

The existing scrollback mechanism already has a **global, not per-pane**
budget for exactly this reason: `GLOBAL_SCROLLBACK_BUDGET_LINES = 50_000`
(`pane.rs:27`), divided across panes by `allocate_scrollback_budget()`
(`pane.rs:44-46`) rather than each pane getting a fixed allocation
independent of how many other panes exist — the doc comment at
`pane.rs:32` states this exists specifically "as a real ceiling on total
retained [memory]," and this pattern was validated under the 900-1,000
session-scale load testing referenced in
`project_plans/stapler-squad-integration/implementation/plan.md` (Story
1.7.x, e.g. the n=900-session/200-concurrent-Attach burst tests at
`plan.md:502-536`).

- **A new replay buffer sized independently per-pane (e.g. "keep the last
  N bytes/entries") does *not* automatically inherit this global-budget
  discipline** — it's a separate allocation from the scrollback grid the
  vt100 parser already retains. If the resume buffer is sized at, say, 64KB
  per pane with no cross-pane cap, 1,000 panes is 64MB baseline just for
  replay buffers, on top of the existing scrollback and grid memory —
  worth explicitly deciding whether this needs the same global-budget
  treatment `GLOBAL_SCROLLBACK_BUDGET_LINES` gets, or whether a smaller
  fixed per-pane cap (e.g. matching or fractioning the existing
  `OUTPUT_CHANNEL_CAPACITY = 1024`-chunk broadcast buffer, `pane.rs:66`)
  is an accepted, bounded cost. Either way this should be a stated design
  decision, not an implicit one.
- **Is a very-old resume seq expensive to request?** Given a true
  fixed-capacity ring buffer (bounded by entries or bytes, evicting
  oldest-first), a request for a seq older than what's retained is
  actually **cheap** to reject — the availability check in §2 is an O(1)
  comparison against `oldest`, not a scan, and correctly returns
  gap-exceeded immediately rather than doing any replay work. The
  resource-exhaustion risk is not in rejecting old-seq requests but in:
  (a) a malicious/buggy client requesting resume with a seq that *is*
  within the retained window but at the very oldest edge, repeatedly, to
  force maximum-length replays on every reconnect — worth rate-limiting or
  at least logging repeated resumes from the same client/pane in a tight
  loop; and (b) whether the replay send path (§1's "copy under lock, then
  await send") itself becomes a synchronous CPU/memory spike for a large
  buffer — proto-encoding and sending, say, a full 64KB backlog in one
  contiguous burst to a slow client, combined with many panes reconnecting
  simultaneously (e.g. after a client-side network blip affecting many
  sessions at once), is the more realistic amplification vector at
  1,000-pane scale, not the seq-lookup itself.

## 4. Heartbeat/keepalive and grace-period pitfalls

This codebase already has a real, shipped precedent for exactly the
disconnect-timing tradeoff the resume grace period will face:
`disconnect_tracker: Arc<Mutex<HashMap<Uuid, Instant>>>` (`main.rs:47`),
used by `warn_if_exit_follows_disconnect` (`main.rs:241-257`) to flag a
pane that exits within `DEFAULT_DISCONNECT_REGRESSION_WINDOW` (300ms,
`main.rs:34`, overridable via `TYMUXD_DISCONNECT_REGRESSION_WINDOW_MS`) of
its last Attach stream dropping. Two things from that precedent transfer
directly:

- **The leak this codebase already hit and fixed once.** `main.rs:133-144`'s
  comment on `purge_disconnect_tracker` states plainly: without an explicit
  purge on every *deliberate* pane-removal path (`close_pane`,
  `kill_session`), "a pane that is detached from and then deliberately
  closed/killed... leaves a permanent entry behind: `Uuid`s are never
  reused, so every such pane leaks one `(Uuid, Instant)` for the life of
  the daemon." A resume grace-period tracker is structurally the same
  shape (`pane_id`/`client_id` → grace-deadline `Instant`) and needs the
  same discipline: every path that removes a pane or a subscriber for a
  reason *other* than grace-period expiry (deliberate `close_pane`,
  `kill_session`, a client resuming successfully before the grace period
  ends) must also purge its own tracker entry, or it leaks identically.
  Grep the plan for every pane/session removal path and confirm the new
  tracker is purged on each.
- **"Grace period never expires" as a live DoS vector, not just a leak.**
  The requirements' own framing — a client that repeatedly
  reconnects-and-immediately-drops resetting the grace period indefinitely
  — is a *worse* failure mode than the tracked-`Instant` leak above,
  because it's not bounded memory growth, it's **unbounded cleanup delay**
  for a specific pane/pty/child process that should have been torn down.
  Concretely: if "grace period active" is modeled as "reset the deadline
  on every new Attach for this pane_id" (the natural implementation), then
  any client — malicious or just badly-behaved (e.g. a flaky network doing
  connect/immediately-RST in a loop) — can hold a pane's cleanup off
  forever by reconnecting faster than the grace period elapses. Two
  mitigations worth deciding between: (a) cap the grace period's *maximum
  total* extension (e.g. "extended at most K times" or "hard ceiling of X
  seconds from the *first* disconnect, not from the *most recent* one"),
  or (b) rate-limit/backoff reconnect attempts per pane_id so a
  tight-loop reconnector can't reset the clock faster than some minimum
  interval. Note this is a distinct axis from `disconnect_tracker`'s
  window, which only ever fires a warning log and never blocks cleanup —
  the new grace period, by design, *gates* cleanup, which is exactly what
  makes the reset-forever case a resource leak vector and not just a
  noisy log.
- **False-positive vs. false-negative disconnect detection.** No
  heartbeat/keepalive mechanism exists yet in this codebase — `attach()`'s
  only signals today are `output_rx.recv()`, `pane_for_exit.wait_exit()`,
  and the inbound stream ending (`main.rs:689-748`). Whatever heartbeat
  cadence gets chosen needs to sit meaningfully above realistic
  network-jitter/slow-client latency (false-positive: killing a slow-but-alive
  client) while staying well under the grace period's own timeout
  (false-negative: not noticing a truly dead client until the grace period
  itself expires, during which the pane is presumably still consuming
  resources under the assumption a resume might come). If the heartbeat
  timeout and the grace-period timeout are set independently without a
  stated relationship between them, it's easy to end up with heartbeat
  detecting death well before grace period expiry does anything useful, or
  the reverse (grace period firing before a legitimately slow heartbeat
  response would have arrived) — this should be an explicit, stated
  ordering (`heartbeat_timeout < grace_period_duration`) in the plan, not
  two independently-tuned constants.

## 5. Reintroducing the Ctrl-d/"block forever on recv" bug at the replay→live handoff

`docs/reviews/is-it-ready-2026-07-13.md:24-32` documents the original bug:
the daemon's forwarding task blocked forever on `output_rx.recv().await`
because nothing signaled pane exit into that same select — 5 of 7
independent review dimensions converged on this as the top blocking issue.
The fix that shipped is visible today in `attach()`'s `forward_handle`
(`main.rs:684-718`): a `biased` `tokio::select!` between `output_rx.recv()`
and `pane_for_exit.wait_exit()`, with the comment at `main.rs:685-688`
explicitly explaining *why* biased ordering matters (drain any output sent
before exit before reporting the exit, rather than racing the two).

**The replay-buffer-then-live-handoff point is structurally the same
shape of hazard**, and is exactly where a new version of this bug is most
likely to be reintroduced:

- A resume handler will presumably (1) drain the ring buffer to catch the
  client up from its cursor to "now," then (2) hand off to the live
  `output_rx.recv()`/`wait_exit()` select loop that already exists. If
  step (1)'s drain loop is written as a separate blocking-recv-style loop
  that doesn't also race `wait_exit()` — e.g. "replay everything from
  cursor to buffer's current tail, *then* start the select loop" — a pane
  that exits *during* the replay (a real possibility: replay of a large
  backlog is not instantaneous, and the pty reader thread can observe EOF
  and flip `exited` at any point concurrently, `pane.rs:259-277`) means
  the replay loop has no way to notice and unblock, reintroducing the
  original bug's exact shape in a new location.
- The safe pattern is to make the transition from "replay" to "live" not a
  hard phase boundary but a continuously-raced condition: keep
  `wait_exit()` (or equivalent) in the same `select!` for the entire
  duration including replay, not just after replay completes. Given the
  ring buffer is a synchronous, lock-protected data structure (not itself
  an async stream with its own recv), draining it doesn't naturally block
  the way `output_rx.recv()` does — but if the replay path is implemented
  as "loop, sending one chunk at a time, `.await`ing each send," a slow
  client during replay is functionally similar to a slow client during
  live streaming, and the same `wait_exit()` race needs to cover that
  entire loop, not just the post-replay portion.
- Also worth an explicit test mirroring the existing regression-test
  pattern at `main.rs:1652` (`attach_streams_output_and_signals_exit`) and
  `main.rs:1724` (`attach_streams_a_nonzero_exit_code_without_backfilling_to_zero`):
  a resume-specific case where the pane exits *while* a resumed client is
  still receiving replay-buffer backlog, asserting the exit event is still
  delivered and the stream still terminates rather than hanging.

## Sources consulted

- `crates/tymux-core/src/pane.rs` (struct `Pane`, lock/atomic layout,
  reader thread, `output_seq`, scrollback budget)
- `crates/tymuxd/src/main.rs` (`attach()`, `forward_step_for_output_result`,
  `disconnect_tracker`, `AttachedGaugeGuard`, existing tests around
  seq/dedup/exit behavior)
- `docs/reviews/is-it-ready-2026-07-13.md` (original Ctrl-d hang bug)
- `project_plans/stapler-squad-integration/implementation/plan.md`
  (`WindowIndex` two-mutex deadlock, Epic 1.4 O(1) fix, 900-1,000-session
  scale test results)
- `docs/runbooks/disconnect-survival-verification.md` (existence checked;
  no grace-period-specific content found there — it covers the abrupt-
  disconnect pane-kill regression, a related but distinct bug class)
