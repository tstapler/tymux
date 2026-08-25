# Implementation Plan: attach-resume-protocol

**Feature**: Let a reconnecting `Attach` client resume from its last-seen output sequence instead of always re-syncing from scratch, via a per-pane replay buffer, a self-contained resume token, dual-layer heartbeat, and a grace period that stops abrupt drops from thrashing window geometry.
**Date**: 2026-08-24
**Status**: Ready for implementation
**ADRs**: ADR-001 (output wire shape), ADR-002 (self-contained resume token), ADR-003 (grace-period referent and mechanism), ADR-004 (reconnect backoff defaults)

---

## Type of system

A protocol/session-resume feature layered onto an existing bidirectional-streaming gRPC daemon (`tymuxd`) and its clients (`tymux-cli`, `clients/ts`, `clients/go`). The core new component is a bounded, append-only, multi-reader-cursor in-memory log (the replay buffer) plus a wire-protocol extension — closer to a small piece of distributed-systems infrastructure (think: a miniature Kafka-style offset/replay contract) than typical CRUD. Complexity 3 per requirements.md.

---

## Step 0.5 — Creative pass (alternatives explored before committing)

### A. Where does replay history live?

1. **Pane-owned bounded `VecDeque`, appended in the reader thread's own critical section** (chosen). *Strength*: correctness is structural — the same code path that bumps `output_seq` and sends to the broadcast channel is the only writer, so the buffer can never disagree with what `output_seq` claims happened. *Weakness*: couples `Pane` with one more piece of state and one more lock acquisition on the pty hot path.
2. **A separate resume-buffer populated by its own `pane.subscribe()` call, living in `tymuxd`.** *Strength*: keeps `Pane` free of resume-specific concerns (single responsibility). *Weakness*: a second independent `broadcast::Receiver` is itself a lossy consumer under load (`RecvError::Lagged`) — it can silently drop the exact chunks it exists to retain, reintroducing the bug the feature is meant to fix. Confirmed structurally, not just in theory (architecture.md Q1).
3. **No dedicated buffer — just enlarge `OUTPUT_CHANNEL_CAPACITY`.** *Strength*: zero new code. *Weakness*: capacity there is tuned for absorbing a burst between two live `recv()` polls, not for a real reconnect window; it gives no distinct "gap exceeded" signal and wastes memory on every pane regardless of whether anyone ever reconnects.

**Chosen: (1)**, recorded in the Pattern Decisions table below.

### B. How does a client prove what it already has?

1. **Self-contained token: `resume_from_seq: u64`, implicitly bound to the `pane_id` already sent as the first `AttachRequest` field** (chosen). *Strength*: no new server-side identity/correlation state; "every Attach call is independent" stays true. *Weakness*: a malformed or cross-pane token needs its own validation path (closed by binding to `pane_id` — Discord Gateway's compound-token precedent).
2. **Server-tracked per-client session → cursor map.** *Strength*: could in principle support richer per-client bookkeeping later. *Weakness*: `disconnect_tracker` already shows what this shape costs — its own doc comment admits a false-positive risk from being keyed only by `pane_id` with multiple attachers; a cursor map would inherit the same ambiguity as a real correctness bug (wrong replay), not just a spurious log line.
3. **Signed/opaque resume token (e.g. HMAC'd blob).** *Strength*: tamper-evident. *Weakness*: this project's security classification is internal/no-auth-change; crypto machinery here buys nothing a plain `u64` bound to an already-unguessable `pane_id` (UUID) doesn't already give.

**Chosen: (1)**.

### C. What actually gates the grace period?

1. **Global per-pane/window mutable deadline, reset on every new Attach for that pane_id (classic debounce).** *Strength*: simplest mental model ("keep extending while active"). *Weakness*: pitfalls.md §4's documented DoS vector — a client that repeatedly reconnects-and-drops holds cleanup off forever, since the deadline keeps resetting.
2. **A cancelable per-`client_id` timer tracked in a `HashMap<ClientId, JoinHandle>`, aborted if that same client reconnects.** *Strength*: allows early cancellation. *Weakness*: `client_id` is freshly minted by `Engine::new_client_id()` on every single `Attach` call (`main.rs:650`) and is never reused — a reconnect always gets a *new* id, so the cancellation path could never actually fire. Pure complexity with no behavioral payoff.
3. **Independent per-disconnect deferred task: a plain `tokio::spawn(sleep(grace_period) then cleanup)` tied to that one disconnect's own `client_id`, never reset or cancelled** (chosen). *Strength*: no shared mutable timer to leak, extend, or race — each disconnect's cleanup fires exactly once, `grace_period_duration` after itself, regardless of how many times the pane is reconnected to in the meantime. This closes pitfalls.md §4's DoS vector *by construction*, not by adding a cap. *Weakness*: one more spawned task per disconnect (cheap, bounded — at most one per `Attach` stream ending).

**Chosen: (3)**.

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| `Pane` | Existing pty-backed terminal struct (`crates/tymux-core/src/pane.rs`). | Unchanged shape, gains one new field. |
| `output_seq` | Existing 1-indexed `AtomicU64` on `Pane`, bumped once per pty-read chunk. | First chunk is seq 1; 0 is the "nothing yet" sentinel. Reused verbatim, never reinvented. |
| `ReplayBuffer` | New Pane-owned, byte-budgeted `VecDeque<(u64, Vec<u8>)>` retaining recent output chunks for reconnect replay. | Lives in `crates/tymux-core/src/replay_buffer.rs`. |
| `ReplayEntry` | One `(seq: u64, data: Vec<u8>)` tuple stored in a `ReplayBuffer` — same tuple shape as `output_tx`'s broadcast payload. | No new type; a plain tuple. |
| `replay_budget_bytes` | The byte ceiling one `ReplayBuffer` instance is granted at pane-spawn time. | Mirrors `scrollback_lines`. |
| `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES` | Process-wide ceiling summed across every live pane's `ReplayBuffer`. | Mirrors `GLOBAL_SCROLLBACK_BUDGET_LINES`. |
| `allocate_replay_budget` / `release_replay_budget` | Functions granting/returning a pane's share of the global replay-buffer budget. | Same atomic-accounting *shape* as `allocate_scrollback_budget`/`release_scrollback_budget` (a `saturating_sub`'d "remaining" calc against a global used-counter) but deliberately **not** the same grant formula: scrollback's `remaining.max(MIN)` always grants at least `MIN_SCROLLBACK_LINES` even once the global budget is exhausted; the replay-buffer allocator is a genuine hard cap and can grant less than `MIN_REPLAY_BUFFER_BYTES`, including `0`, once `remaining` is small/zero. See Pattern Decisions for why these two intentionally differ. |
| `ReplayOutcome` | Enum: `InWindow { chunks: Vec<(u64, Vec<u8>)>, tail_seq: u64 }` or `GapExceeded { oldest_available_seq: Option<u64> }`. | Return type of `Pane::replay_since`. |
| `resume_from_seq` | Client-supplied optional field on the first `AttachRequest`: the last seq the client already has. | Proto field 4, outside the existing `oneof`. Structurally this makes `AttachRequest { payload: None, resume_from_seq: Some(_) }` representable (no `pane_id` at all, but a resume seq) — but `attach()`'s existing pane_id-required check (`main.rs:624-631`) unconditionally rejects any first message lacking `pane_id` with `invalid_argument` before `resume_from_seq` is ever read (Task 2.2.1a's read happens strictly after that check). The illegal state is type-representable but value-level unreachable; see the Pattern Decisions row below rather than a proto restructure. |
| `resume token` | The `(pane_id, resume_from_seq)` pair, conceptually — never a distinct wire type. | Self-contained per ADR-002; no server-side identity correlation. |
| `OutputChunk` | New proto submessage `{ uint64 seq; bytes data; }`, carried in a NEW sibling field `output_chunk = 7` on `AttachEvent`. The existing `bytes output = 1;` field is untouched and stays populated in parallel (dual-write) — `OutputChunk` is an additive sibling, not a replacement. | ADR-001 (revised). |
| `output` (legacy) | Existing `bytes output = 1;` field on `AttachEvent` — raw pty bytes, no seq. Kept byte-for-byte unchanged and populated on every `Output`-carrying event alongside the new `output_chunk`, so pre-`v0.2.0` clients (`clients/go@v0.1.0`, stapler-squad's `BackendTymux`) see zero wire-format change. | Retiring this field is explicit future/out-of-scope work, not part of this project (see Risk Control). |
| `GapExceeded` | New `AttachEvent` oneof payload signaling a resume request outside the retained window. | Carries `oldest_available_seq` for diagnostics. |
| `Heartbeat` | New empty `AttachEvent` oneof payload, sent periodically by the server. | Purely a liveness signal; no fields. |
| `ForwardStep` | Existing enum (`Emit`/`Skip`/`End`) mapping one broadcast `recv()` result to a forwarding action. | Unchanged; reused verbatim for both the live loop and the priming-threshold concept. |
| `forward_step_for_output_result` | Existing pure function; the `seq <= threshold` skip logic. | Signature and skip/dedup logic unchanged; `threshold` may now come from `ReplayOutcome::tail_seq` as well as `snapshot_with_seq`. Its `Emit` payload construction does change (Task 2.2.1c): it now populates BOTH the untouched legacy `output` bytes field and the new `output_chunk: OutputChunk{seq, data}` field on the same event (dual-write) — there is no placeholder to replace, since Task 1.1.1b left the legacy field's call sites untouched. |
| `resume threshold` | The seq value `forward_step_for_output_result` compares incoming live chunks against — either `snapshot_seq` (no-token path) or `ReplayOutcome::InWindow::tail_seq` (resume path). | Same comparison, two possible sources. |
| `disconnect_tracker` | Existing `Mutex<HashMap<Uuid, Instant>>` regression-detection map. | Untouched by this feature. |
| `grace_period_duration` | Daemon config: how long a deferred viewport/geometry cleanup waits before firing. | Default 60s; env-var overridable, mirroring `DEFAULT_DISCONNECT_REGRESSION_WINDOW`. |
| `DeferredViewportCleanup` | The per-disconnect `tokio::spawn`ed task that performs `unregister_viewport` + `recompute_window_geometry` after `grace_period_duration`. | Not a named struct — a spawned async block; listed here because it's the feature's real behavior, not the literal "grace period" plumbing. |
| `heartbeat_interval` | Server-side `tokio::time::interval` cadence for sending `Heartbeat` `AttachEvent`s. | Default 15s. |
| `heartbeat_timeout` | Client-side threshold: no event received within this long marks the connection suspect and triggers reconnect. | Default 45s; must stay `< grace_period_duration`. Implemented by Task 6.1.1d: a `tokio::select!` racing `maybe_event?` against this deadline, reset on every received event (`Heartbeat`, `Output`/`OutputChunk`, any event — not `Heartbeat` alone), entering the same reconnect path as a stream-termination error on expiry. |
| `ReconnectLoop` (tymux-cli) | Phase 6 client-side loop detecting a dropped `Attach` stream and reopening it with a resume token. | New, in `crates/tymux-cli/src/main.rs` (Epic 6.1). Not the cut candidate — Epic 6.2 (cross-invocation persistence) is; see Phase 6's Framing paragraph. Runs inline inside `attach()`'s own scope (Task 6.1.1a) — never returns to `attach_and_follow` mid-retry. |
| `Detach-during-backoff` | The retry loop's `select!` (Task 6.1.1a) races `stdin_rx` against each reconnect cycle (backoff sleep + redial attempt), so the existing Detach binding (`C-b d`) can interrupt reconnection while it's still retrying. | Resolves ux.md Surface 3 AC3.6. Exits via the existing `AttachOutcome::Done` — no new outcome variant, since the terminal-restoration guarantee (`RawGuard::Drop`) and downstream handling are already identical to a live-session detach. |
| `pending_input` | Retry-loop-local `Vec<u8>` buffer (Task 6.1.1a) holding `ReassembledOutput::Forward` bytes typed while no live `tx` exists to send them on; flushed as the first `AttachRequest::Input` once a reconnect attempt succeeds. | Avoids turning a keystroke typed during backoff into a permanent loss now that `stdin_rx` is polled during backoff (previously, an unpolled `stdin_rx` at least left it queued in the channel). Does not cover non-Detach `Action`s fired during backoff — those are still discarded, a narrower, deliberate simplification. **Decided fate on the two non-reconnect exits from backoff**: on Detach-during-backoff and on give-up/exhaustion, `pending_input` is discarded, not flushed anywhere or persisted — not a contradiction of the buffer's stated purpose, since that purpose is bridging a *temporary* drop long enough to reconnect, not surviving a *deliberate* exit (Detach) or a *terminal* one (give-up); in both cases there is no live pane left to deliver the queued keystrokes to. See Task 6.1.1a/6.1.1g and ux.md AC3.6/AC3.7. |
| `ResumeState` | tymux-cli's persisted `pane_id -> last-seen seq` record, read/written across process invocations. | New, `crates/tymux-cli/src/resume_state.rs`. Cut-candidate epic. |
| `resume_state_path` | The `$XDG_STATE_HOME`-rooted file path `ResumeState` is persisted to. | Mirrors `config.rs:207`'s `default_config_path` pattern. |
| `chrome_message_for_event` | Existing CLI function mapping an `AttachEvent` payload to a fixed status-line string. | Gains a `GapExceeded` arm. |
| `ResumeOutcome` | Observability tagging value: `ResumedFromBuffer` \| `GapExceededFallback` \| `NoResumeTokenFullAttach`. | Drives the new resume-outcome counter. |
| `tymux_attach_resume_outcome_total` | New hand-rolled counter (atomics, no metrics crate), tagged by `ResumeOutcome`. | Mirrors `attached_sessions_gauge`'s existing convention. |
| `AttachedGaugeGuard` | Existing RAII guard decrementing the attached-sessions gauge. | Untouched. |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Replay-history storage location | Pane-owned bounded `VecDeque` (Value Object / bounded collection), appended in the pty reader thread | Type-driven design; mirrors existing `allocate_scrollback_budget` precedent | A separate resume-buffer service in `tymuxd`, populated by its own `pane.subscribe()`; enlarging `OUTPUT_CHANNEL_CAPACITY` instead | A second independent broadcast subscriber is itself a lossy consumer under load, reintroducing the exact loss this feature exists to prevent; enlarging channel capacity conflates lag-tolerance with reconnect-window sizing and gives no distinct gap-exceeded signal |
| Resume identity/cursor | Self-contained value (`resume_from_seq` bound to `pane_id`) | PoEAA — avoid a Session/Repository pattern for stateless per-request data | Server-tracked `HashMap<ClientId, Cursor>`; signed/opaque token | Server tracking inherits `disconnect_tracker`'s own admitted false-positive shape one layer up, now as a real correctness bug (wrong replay) rather than a spurious log; a signed token adds crypto weight the internal/no-auth-change security classification doesn't call for |
| Grace-period cleanup mechanism | Independent per-disconnect deferred task (`tokio::spawn(sleep then cleanup)`, never reset or cancelled) | Simplicity over GoF Observer/Strategy machinery | Global per-pane/window mutable deadline reset on every reconnect; cancelable per-`client_id` timer in a tracked `HashMap` | The reset-on-reconnect design is pitfalls.md §4's documented DoS vector; the cancelable-timer design adds a tracker and cancellation path that can never actually fire, since `client_id` is freshly minted every `Attach` call and never reused |
| Eviction / gap-check logic | Pure function on `ReplayBuffer`, mirroring `forward_step_for_output_result` | PoEAA Transaction Script (stateless calculation, unit-testable without I/O) | GoF Strategy (pluggable eviction-policy object) | Exactly one eviction policy exists and is never expected to vary — a Strategy object would be unused indirection |
| Replay-buffer budget allocation | Genuine hard cap: `allocate_replay_budget()` can return less than `MIN_REPLAY_BUFFER_BYTES`, including `0`, once `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES` is exhausted | NFR: "must not risk unbounded memory growth under many concurrent panes" | Copying `allocate_scrollback_budget`'s `remaining.max(MIN)` floor-grant formula verbatim | Scrollback's `MIN` floor is justified — zero scrollback would break copy-mode entirely, so a floor makes sense even under memory pressure. The replay buffer has no equivalent justification: a pane granted `0` replay bytes just always returns `GapExceeded` from `replay_since` (Epic 2.2.2's already-planned fallback), which is safe degraded behavior, not broken behavior. Since no pane-count cap exists anywhere, a floor-grant formula would let total replay memory grow without bound as pane count grows — the one thing this NFR forbids. |
| `AttachEvent.output` wire shape | Additive dual-field: untouched `bytes output = 1;` (legacy, byte-identical) kept as-is, PLUS a new sibling `OutputChunk output_chunk = 7;` submessage `{ seq, data }` for new clients | Type-driven design (atomic seq+data pairing) + wire-compatibility correction to ADR-001 (revised) | (a) `bytes` → `OutputChunk` same-field-number promotion (originally chosen, then reconsidered); (b) bare `bytes output` plus a sibling bare `seq` field at a new field number | (a) was reconsidered and rejected: `bytes` and `message` both use protobuf wire type 2, so an old client decodes field 1 successfully but silently misreads the new submessage's own framing (varint seq tag + length-prefixed data) as raw pty bytes — a genuine, undetected wire break, unlike ADR-001's earlier `ExitStatus` `bool`→`message` change, which has incompatible wire types and fails loudly instead of silently. (b) is still rejected for the NEW field specifically: a bare sibling `seq` can go missing or disagree with `data` independently (representable illegal state) — solved by keeping the new field a submessage (`OutputChunk`), just making it an ADDITIVE sibling of the untouched legacy field rather than a replacement at the same field number |
| Replay-buffer locking | Standalone `Mutex<ReplayBuffer>`, pushed then sent sequentially in the reader thread, never nested with `parser` | Concurrency discipline (pitfalls.md §1 / the shipped `WindowIndex` deadlock precedent) | Nest the replay lock inside `parser`'s critical section | A lock that's never co-acquired with any other lock is trivially deadlock-free — no ordering pair exists to get wrong, simpler than auditing a nested order across every call site |
| Heartbeat | Two-layer: tonic HTTP/2 keepalive (buy, builder calls only) + one new `AttachEvent::Heartbeat` variant on a `tokio::time::interval` (build) | Build-vs-buy research | App-level heartbeat alone, no transport config | Transport-level PING is connection-scoped dead-peer detection tonic already implements for free; skipping it leaves a genuinely idle connection undetected for far longer than the application heartbeat interval alone would catch |
| `AttachRequest.resume_from_seq` placement | Left as a bare `optional uint64` sibling field outside the `oneof`, documented (not restructured) | Judgment call against `attach()`'s actual validation (`main.rs:624-631`) | Move `resume_from_seq` inside a new oneof variant, or wrap `pane_id`+`resume_from_seq` in a small message together | `AttachRequest { payload: None, resume_from_seq: Some(_) }` is representable at the type level (echoing ADR-001's original "illegal state representable" concern), but `attach()` already rejects any first message without `pane_id` unconditionally, before `resume_from_seq` is ever read — the illegal state is value-level unreachable today, so a proto restructure buys nothing a doc note (Domain Glossary, above) doesn't already cover; restructuring is deferred unless a future call site reads `resume_from_seq` before the `pane_id` check |
| Backoff-window escape hatch (Task 6.1.1a) | Race `stdin_rx` (watching for the existing Detach binding) against each reconnect cycle inside the retry loop's own `select!`, reusing the live session's `RawGuard`/`stdin_rx`/`reassembler` by running the retry loop inline inside `attach()` | Reuse over invention: `KeystrokeReassembler`'s local-keypress interception already exists and already has a working Detach story elsewhere in this same function | Signal-handler-based `RawGuard` restoration on `SIGTERM`/`SIGINT`-as-real-signal instead, with no in-band escape hatch | The signal-handler approach only closes the `SIGTERM`/`SIGINT`-from-another-terminal gap — it can never help against `SIGKILL`, and even where it applies it only leaves the terminal usable *after* the process is killed, giving the user no way to actually shorten the wait. Racing stdin is strictly better: a real, immediate, in-band escape hatch, and it costs no new mechanism, since the retry loop already has to live inside `attach()`'s scope for `RawGuard`/`stdin_rx`/`reassembler` reuse in the first place |

---

## Migration Plan

Not applicable — no schema or persisted-data changes. The proto wire-protocol extension (`AttachEvent` gains a new `output_chunk` field; the existing `bytes output = 1;` field is left untouched) is a purely additive change, not a breaking one and not a data migration; it's covered under Risk Control, ADR-001 (revised), and Epic 2.4's explicit backward-compatibility and compat-assertion tests, not here.

## Observability Plan

- **Logs**:
  - `tracing::warn!` when a resume request's `resume_from_seq` falls outside the replay buffer's retained window — fields: `pane_id`, `resume_from_seq`, `oldest_available_seq`.
  - `tracing::info!` when a `DeferredViewportCleanup` task actually fires (i.e. no reconnect happened within `grace_period_duration`) — fields: `pane_id`, `window_id`, `client_id`, `elapsed_ms`. Mirrors the wording style of `AttachedGaugeGuard`'s existing gauge-change logging.
  - `tracing::info!` extends the existing `attach: gauge incremented` line (`main.rs:642`) with a `resume_requested: bool` field.
- **Metrics**: `tymux_attach_resume_outcome_total`, a small set of hand-rolled `AtomicI64` counters (or a `HashMap<ResumeOutcome, AtomicI64>` behind a `Mutex`, whichever is simpler at implementation time) tagged by `ResumeOutcome` (`resumed_from_buffer` / `gap_exceeded_fallback` / `no_resume_token_full_attach`), surfaced via a `tracing::info!` line on change — exactly `attached_sessions_gauge`'s existing pattern (requirements.md's security classification: internal/local, no on-call rotation, no metrics-crate justification).
- **Alerts**: None — solo/personal project, no on-call, matching every other metric in this codebase.

## Risk Control

- **Feature flag**: None. Genuinely additive, backward-compatible protocol change at every level: a client that never sends `resume_from_seq` gets exactly today's behavior (Epic 2.4's explicit test), AND — since ADR-001 (revised) adds `output_chunk` as a new sibling field rather than promoting `bytes output` at the same field number — the wire bytes an old, unmodified client decodes are byte-for-byte untouched. This is no longer an assumption resting on field-number preservation; it's verified directly by Task 2.4.1c's compat-assertion test.
- **Rollback procedure**: Standard revert-via-PR. One real wrinkle: `clients/go`'s tagged module version bump (Epic 1.2) is a separate, cross-repo-consumed artifact — reverting the tymux-side PR does not un-bump `stapler-squad`'s pinned `go.mod` dependency. If a rollback is ever needed after the tag has been consumed downstream, it must be coordinated as a second, explicit step, not assumed automatic.
- **Staged rollout**: Not applicable (no traffic ramp for a local daemon). The one real residual risk is the same class the disconnect-survival fix already hit: tonic's HTTP/2 keepalive behavior under a real flaky network is unverified in CI/local dev. Recommend a short real-hardware verification pass mirroring `docs/runbooks/disconnect-survival-verification.md`, flagged explicitly for `sdd:4-validate` rather than assumed covered by unit/integration tests alone.

## Unresolved Questions

- **Real-hardware/real-network verification of tonic keepalive timing** — flagged as a feasibility risk in requirements.md; this plan does not attempt to resolve it (CI/local dev can't exercise real packet loss), only names it and points to the same runbook precedent (`docs/runbooks/disconnect-survival-verification.md`) as the mechanism to close it later.
- **Daemon-wide vs. per-session override for `grace_period_duration`/`heartbeat_interval`** — this plan defaults to daemon-wide, env-var-overridable config only (mirroring `DEFAULT_DISCONNECT_REGRESSION_WINDOW`'s existing pattern), with no per-session override in v1. Revisit only if real usage shows a concrete need.

Everything else requirements.md listed as open (grace-period duration itself; the shared backoff/give-up parameters) is resolved by this plan with concrete numbers — see ADR-003 and ADR-004 respectively, both explicitly labeled "informed default, not measured."

---

## Dependency Visualization

```
Phase 1: Proto & Wire Protocol
  Epic 1.1 (AttachEvent/AttachRequest wire shape + shared reconnect spec in proto comments)
      |
      v
  Epic 1.2 (clients/go tagged module version bump + regen)
      |
      v
Phase 2: Replay Buffer & Resume Core
  Epic 2.1 (ReplayBuffer type + wiring into Pane) --------+
      |                                                   |
      v                                                   |
  Epic 2.2 (daemon resume handling in attach()) <---------+
      |
      v
  Epic 2.3 (replay-to-live handoff / wait_exit race safety)
      |
      v
  Epic 2.4 (backward-compat: no-token full-attach unchanged)
      |
      v
  Epic 2.5 (test infra: TestDaemon transport-drop capability -- feeds Phase 6's E2E tests)
      |
      v
Phase 3: Heartbeat & Grace Period          (starts after Phase 1; independent of Phase 2's internals)
  Epic 3.1 (HTTP/2 transport keepalive, server + client)
      |
      v
  Epic 3.2 (app heartbeat + deferred viewport/geometry cleanup)
      |
      v
  Epic 3.3 (verify grace-period design is leak/DoS-safe by construction)
      |
      v
Phase 4: Observability                      (depends on Phase 2 + Phase 3 both landing)
  Epic 4.1 (resume-outcome counter + structured logs)
      |
      v
Phase 5: Reference Clients                  (depends on Phase 1 + 2 + 3)
  Epic 5.1 (clients/ts)        Epic 5.2 (clients/go)
      |                              |
      +--------------+---------------+
                     |
                     v
Phase 6: tymux-cli Reconnect Loop
  Epic 6.1 (in-process loop) -> Epic 6.2 (CUT CANDIDATE IF APPETITE OVERRUNS: cross-invocation persistence) -> Epic 6.3 (CLI UX polish)
```

Per requirements.md's Appetite section, the actual cut order is two numbered steps, finer-grained than "cut Phase 6 as a whole":

1. **First cut: Epic 6.2 only** (`tymux-cli`'s cross-invocation persistence — the `ResumeState`/`resume_state_path`/`$XDG_STATE_HOME` disk-persistence work). Requirements.md: "drop `tymux-cli`'s cross-invocation persistence first — ship the protocol + reference-client test coverage, defer CLI reconnect to a follow-up." Phases 1-5 ship the protocol, the daemon-side resume machinery, and reference-client (`clients/ts`/`clients/go`) test coverage regardless. Epics 6.1 (in-process reconnect loop) and 6.3 (CLI UX polish) are **not** part of this first cut: Epic 6.1 needs no persistence and fully auto-reconnects within one long-running `tymux attach` process's lifetime on its own; Epic 6.3 only makes sense once Epic 6.1 exists. Dropping only Epic 6.2 means a *fresh* `tymux attach` invocation just won't have `resume_from_seq` populated from disk — the in-process auto-reconnect still fully works.
2. **If still overrun after cutting Epic 6.2**: requirements.md's step (2) says fall all the way back to the Minimal scope from the original ideation pass — seq-exposure + resume-token + heartbeat/grace-period only, dropping the replay buffer itself (Epic 2.1/2.2/2.3) — not a separate "cut the rest of Phase 6" step in between. Not detailed task-by-task here since it would mean *removing* already-planned work, not adding a new phase.

**Open note**: requirements.md's own wording ("defer CLI reconnect to a follow-up") is arguably ambiguous — it could be read as deferring only cross-invocation persistence (Epic 6.2, the reading adopted above, since "CLI reconnect" across separate invocations *is* what persistence enables) or as deferring the whole `tymux-cli` reconnect loop including Epic 6.1. This plan adopts the narrower reading because it's the only one under which cutting scope doesn't leave `tymux-cli` with literally zero reconnect capability (today's baseline, per requirements.md's own Baseline section) under exactly the schedule-pressure scenario the cut order is meant to handle gracefully. If the plan's authors judge that Epic 6.1/6.3 without Epic 6.2 isn't meaningful standalone scope worth a middle cut tier, that's a legitimate refinement worth revisiting — not something this plan invents as settled fact.

---

## Phase 1: Proto & Wire Protocol

### Epic 1.1: `AttachEvent`/`AttachRequest` wire-shape changes + shared reconnect spec
**Goal**: Extend the proto contract to carry `seq` on output, a resume token, and explicit gap/heartbeat signals — and write the one shared reconnect-loop specification every client must implement identically.

#### Story 1.1.1: Add `OutputChunk output_chunk` as a new sibling field alongside the untouched `AttachEvent.output`, add `GapExceeded`/`Heartbeat`, add `resume_from_seq`
**As a** client implementer (CLI or reference client), **I want** every output chunk to carry its own sequence number on the wire, **so that** I can build a resume token from what I've already received — **without** breaking any client that only reads today's `bytes output` field.
**Acceptance Criteria**:
- `AttachEvent` gains a NEW field `OutputChunk output_chunk = 7;` (submessage `{ uint64 seq; bytes data; }`) as an additive sibling to the existing `bytes output = 1;` field, which is left byte-for-byte UNCHANGED — not promoted, not replaced, same field number, same wire type, same semantics as today.
  - *Given* the updated `.proto` file, *When* `tonic-build` regenerates Rust types, *Then* `attach_event::Payload::Output(Vec<u8>)` at field 1 still compiles exactly as it does today (no call site touched), AND `attach_event::Payload::OutputChunk(OutputChunk)` is a new, additional oneof variant at field 7 where `OutputChunk { seq: u64, data: Vec<u8> }` is a real generated type.
  - *Given* a client that only decodes field 1 as `bytes` (any pre-`v0.2.0` `clients/go` consumer, including `stapler-squad`'s `BackendTymux`), *When* it receives an `AttachEvent` from a daemon built from this branch, *Then* it decodes exactly the same raw pty bytes it would have before this feature shipped, with no risk of misinterpreting `OutputChunk`'s own wire framing (see ADR-001, revised).
- `AttachRequest` gains `optional uint64 resume_from_seq = 4;` outside the existing `oneof`.
  - *Given* a client constructs `AttachRequest { payload: Some(PaneId("...".into())), resume_from_seq: Some(42) }`, *When* it's serialized and parsed back, *Then* `resume_from_seq` round-trips as `Some(42)`, distinct from an absent field (`None`) which behaves identically to today's client.
- `AttachEvent` gains `GapExceeded` (with `uint64 oldest_available_seq = 1;`) and `Heartbeat` (empty message) oneof variants at field numbers 5 and 6.
  - *Given* the regenerated proto, *When* a Rust match on `attach_event::Payload` is written, *Then* it must handle seven variants (`Output`, `Snapshot`, `Exited`, `OutputGap`, `GapExceeded`, `Heartbeat`, `OutputChunk`) or fail to compile under `#[deny(clippy::wildcard_enum_match_arm)]`-style exhaustiveness where used deliberately (existing `_ => {}` catch-alls in the CLI remain intentional, not accidental).
**Files**: `proto/tymux/v1/tymux.proto`

##### Task 1.1.1a: Edit `AttachEvent`/`AttachRequest`/`OutputChunk`/`GapExceeded`/`Heartbeat` message defs (~5 min)
- In `proto/tymux/v1/tymux.proto`, leave `bytes output = 1;` inside `AttachEvent` completely UNCHANGED — do not touch it. Instead add a NEW sibling field `OutputChunk output_chunk = 7;` to `AttachEvent`'s oneof (field 7 is the next free number: 1=`output`, 2=`snapshot`, 3=`exited`, 4=`output_gap` already exist; 5=`gap_exceeded` and 6=`heartbeat` are added below in this same task — confirmed against the current file, `proto/tymux/v1/tymux.proto`).
- Add `message OutputChunk { uint64 seq = 1; bytes data = 2; }` above `AttachEvent`.
- Add `GapExceeded gap_exceeded = 5;` and `Heartbeat heartbeat = 6;` to `AttachEvent`'s oneof; add `message GapExceeded { uint64 oldest_available_seq = 1; }` and `message Heartbeat {}`.
- Add `optional uint64 resume_from_seq = 4;` to `AttachRequest`, after the closing brace of its `oneof payload { ... }` block (not inside it).
- Files: `proto/tymux/v1/tymux.proto`

##### Task 1.1.1b: Regenerate Rust codegen and confirm the change is additive with zero compile fallout (~3 min)
- Run the workspace's normal build (`cargo build --workspace`) to trigger `tonic-build`'s codegen from the updated `.proto`.
- Because `bytes output = 1;` is left untouched, every existing call site that constructs `attach_event::Payload::Output(bytes)` (`crates/tymuxd/src/main.rs:337` and its test-module mirrors) continues to compile with zero changes — there is no placeholder and nothing to fix here. The new `output_chunk` field simply defaults to `None`/unset until something populates it.
- Populating `output_chunk` with the real `seq` value is entirely deferred to **Task 2.2.1c**, the one production call site where the real seq is actually available — this task's only job is confirming the additive proto change compiles cleanly with no fallout, unlike a same-field-number promotion which would have forced every `Output` call site to migrate immediately.
- Files: `crates/tymuxd/src/main.rs`, `crates/tymux-cli/src/main.rs`

##### Task 1.1.1c: Write the shared reconnect-loop specification into proto doc comments (~5 min)
- Extend `Attach`'s RPC-level doc comment in `proto/tymux/v1/tymux.proto` (currently lines 48-67) with the authoritative, implementation-language-agnostic contract: `resume_from_seq` semantics (bound to the `pane_id` in the same first message), `GapExceeded` meaning and required client behavior (treat the following `Snapshot` event as authoritative, discard any local partial state), and the concrete backoff policy from ADR-004 (revised) — exponential starting 200ms, x2 multiplier, capped at 8s, +/-20% jitter, giving up after 14 attempts (nominal cumulative backoff ~68.6s, deliberately >= `grace_period_duration` (60s) — see ADR-004's Consequences and Task 6.1.1f's enforced invariant test).
- State explicitly in the comment: this is the one specification `tymux-cli`, `clients/ts`, and `clients/go` must all implement identically (Epics 5.1, 5.2, 6.1).
- Files: `proto/tymux/v1/tymux.proto`

#### Story 1.1.2: Regenerate `clients/ts` and `clients/go` stubs from the updated proto
**As a** reference-client maintainer, **I want** the generated TS and Go stubs to reflect the new wire shape, **so that** Epic 5.1/5.2's resume-path code has real types to build against.
**Acceptance Criteria**:
- `clients/ts/gen/tymux/v1/tymux_pb.ts` exposes `OutputChunk`, `GapExceeded`, `Heartbeat`, and `AttachRequest.resumeFromSeq`.
  - *Given* the updated proto, *When* `buf generate` runs against `buf.gen.ts.yaml`, *Then* `tymux_pb.ts`'s `AttachEvent` message type includes new `case: "gapExceeded"`, `case: "heartbeat"`, and `case: "outputChunk"` union members alongside the existing four unchanged ones (including `case: "output"`, still `Uint8Array`, not `OutputChunk`).
- `clients/go/gen/tymux/v1/tymux.pb.go` exposes the Go equivalents.
  - *Given* the same proto, *When* `buf generate` runs against `buf.gen.go.yaml`, *Then* `tymuxv1.AttachEvent_GapExceeded` and `tymuxv1.AttachEvent_Heartbeat` types exist and `go build ./...` succeeds inside `clients/go/`.
**Files**: `clients/ts/gen/tymux/v1/tymux_pb.ts`, `clients/go/gen/tymux/v1/tymux.pb.go`, `clients/go/gen/tymux/v1/tymuxv1connect/tymux.connect.go`

##### Task 1.1.2a: Run `buf generate` for TypeScript and commit generated output (~3 min)
- From `proto/`, run `buf generate --template buf.gen.ts.yaml .`; confirm `clients/ts/gen/` changes are limited to the new/changed message shapes.
- Files: `clients/ts/gen/tymux/v1/tymux_pb.ts`

##### Task 1.1.2b: Run `buf generate` for Go and confirm `go build` (~3 min)
- From `proto/`, run `buf generate --template buf.gen.go.yaml .`; then `cd clients/go && go build ./...` to confirm the regenerated stubs compile standalone.
- Files: `clients/go/gen/tymux/v1/tymux.pb.go`, `clients/go/gen/tymux/v1/tymuxv1connect/tymux.connect.go`

---

### Epic 1.2: `clients/go` tagged module version bump
**Goal**: Ship the new `output_chunk`/resume surface as a real, coordinated version bump of the tagged Go module — not an afterthought — following the exact playbook already used for `v0.1.0` (PR #34/#35). Per ADR-001 (revised), the underlying wire change itself is additive and non-breaking (old `clients/go@v0.1.0`/`BackendTymux` consumers need zero code change and see zero wire-format difference); this version bump exists so a consumer can *opt into* the new resume-capable surface (`output_chunk`, `GapExceeded`, `Heartbeat`, `resume_from_seq`) on its own schedule, not to avoid a break that no longer exists.

#### Story 1.2.1: Cut `clients/go/v0.2.0` after the resume-surface change merges
**As a** downstream consumer (`stapler-squad`'s `go.mod`), **I want** a new tagged version to pin to, **so that** I control exactly when I take the new resume-capable API surface.
**Acceptance Criteria**:
- A new git tag `clients/go/v0.2.0` exists, pointed at the commit containing the merged proto + codegen changes.
  - *Given* Epics 1.1/1.2's changes are merged to `main`, *When* `git tag clients/go/v0.2.0 <merge-sha> && git push origin clients/go/v0.2.0` runs, *Then* `go list -m github.com/tstapler/tymux/clients/go@v0.2.0` (run from any module able to reach the repo) resolves successfully.
- A fresh `go get` against the new tag builds cleanly.
  - *Given* a scratch Go module with `require github.com/tstapler/tymux/clients/go v0.2.0`, *When* `go build ./...` runs after `go mod tidy`, *Then* it succeeds with no manual `replace` directive needed.
**Files**: none (git tag operation, not a file change) — verification only touches a scratch/throwaway module.

##### Task 1.2.1a: Tag and push `clients/go/v0.2.0` post-merge (~2 min)
- After Epic 1.1's PR merges to `main`, run `git tag clients/go/v0.2.0 <merge-sha>` and `git push origin clients/go/v0.2.0`, matching the exact precedent of `clients/go/v0.1.0` (PR #34).
- Files: none.

##### Task 1.2.1b: Smoke-test the new tag resolves and builds (~3 min)
- In a scratch directory, `go mod init smoke && go get github.com/tstapler/tymux/clients/go@v0.2.0 && go build ./...` (importing at least one generated type) to confirm the tag is consumable exactly like `v0.1.0` was.
- Files: none (scratch verification, discarded after).

---

## Phase 2: Replay Buffer & Resume Core

### Epic 2.1: Per-pane replay ring buffer
**Goal**: A bounded, byte-budgeted, append-only log of recent output chunks owned by `Pane`, populated in the exact critical section that already bumps `output_seq`.

#### Story 2.1.1: `ReplayBuffer` type with eviction and gap-check logic, unit tested in isolation
**As a** daemon implementer, **I want** the eviction/availability logic to be a pure, directly-testable unit, **so that** the 1-indexed off-by-one and byte-budget edge cases (pitfalls.md §2) are provably correct before any pty/broadcast wiring touches them.
**Acceptance Criteria**:
- `ReplayBuffer::push(seq, data)` evicts from the front until `total_bytes <= budget_bytes`, always keeping at least one entry even if a single chunk exceeds the budget alone.
  - *Given* a `ReplayBuffer` with `budget_bytes = 100` containing entries totaling 90 bytes, *When* `push(seq, [0u8; 30])` is called, *Then* the buffer evicts from the front until `total_bytes <= 100` and the newly pushed entry is always retained.
- `ReplayBuffer::replay_since(resume_from_seq, latest_seq)` returns `InWindow` when `resume_from_seq >= oldest_retained_seq` (or the buffer is empty and `resume_from_seq == latest_seq`), and `GapExceeded` otherwise — using the exact `>=`/`<=` boundary convention as the existing `seq <= snapshot_seq` dedup check (`main.rs:333`).
  - *Given* a `ReplayBuffer` whose oldest retained entry has `seq = 5`, *When* `replay_since(5, 9)` is called, *Then* it returns `InWindow { chunks: [entries with seq 6..=9], tail_seq: 9 }`.
  - *Given* the same buffer, *When* `replay_since(4, 9)` is called, *Then* it returns `GapExceeded { oldest_available_seq: Some(5) }` — even though chunk `seq=5` is technically present, per the conservative `>=`-only boundary this plan adopts (matching pitfalls.md's literal test case, not a looser "chunk 5 would still cover it" interpretation).
  - *Given* a fresh `ReplayBuffer` with no entries and `latest_seq = 0` (no output has ever happened), *When* `replay_since(0, 0)` is called, *Then* it returns `InWindow { chunks: [], tail_seq: 0 }` without any subtraction underflow.
  - *Given* `resume_from_seq > latest_seq` (a malformed or future token), *When* `replay_since` is called, *Then* it returns `GapExceeded` (degrading to the same fallback signal as any other out-of-range request, per features.md edge case 2).
- `allocate_replay_budget()` is a genuine hard cap on `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES`, not a floor-grant — unlike `allocate_scrollback_budget`'s `remaining.max(MIN)` shape, it can return less than `MIN_REPLAY_BUFFER_BYTES`, including `0`.
  - *Given* `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES` is already fully allocated to existing panes, *When* a new pane spawns and calls `allocate_replay_budget()`, *Then* it receives `0` bytes (not `MIN_REPLAY_BUFFER_BYTES`), and that pane's `replay_since()` calls always return `GapExceeded` (its buffer can never hold anything) — which is safe because Epic 2.2.2's `GapExceeded` fallback path already handles this degraded case.
**Files**: `crates/tymux-core/src/replay_buffer.rs` (new)

##### Task 2.1.1a: Create `ReplayBuffer`/`ReplayOutcome` types with `push` and `replay_since` (~5 min)
- New file `crates/tymux-core/src/replay_buffer.rs`: `pub(crate) struct ReplayBuffer { entries: VecDeque<(u64, Vec<u8>)>, total_bytes: usize, budget_bytes: usize }`, `pub(crate) enum ReplayOutcome { InWindow { chunks: Vec<(u64, Vec<u8>)>, tail_seq: u64 }, GapExceeded { oldest_available_seq: Option<u64> } }`.
- Implement `ReplayBuffer::new(budget_bytes: usize) -> Self`, `push(&mut self, seq: u64, data: &[u8])` (evict-from-front-until-under-budget), `oldest_seq(&self) -> Option<u64>`.
- Files: `crates/tymux-core/src/replay_buffer.rs`

##### Task 2.1.1b: Implement `replay_since` with the exact boundary convention (~4 min)
- `pub(crate) fn replay_since(&self, resume_from_seq: u64, latest_seq: u64) -> ReplayOutcome`: check `resume_from_seq > latest_seq` first (→ `GapExceeded`), then compute `available_from = self.oldest_seq().unwrap_or(latest_seq)`, then `resume_from_seq >= available_from` (→ `InWindow`, filtering `entries` to `seq > resume_from_seq`) else `GapExceeded { oldest_available_seq: self.oldest_seq() }`.
- Files: `crates/tymux-core/src/replay_buffer.rs`

##### Task 2.1.1c: Unit tests for every boundary case named in the acceptance criteria (~5 min)
- `#[cfg(test)] mod tests` in the same file: eviction-under-budget-pressure, `resume_from_seq == oldest` succeeds, `resume_from_seq == oldest.saturating_sub(1)` gap-exceeds (explicit `saturating_sub` in the test's own construction, per pitfalls.md §2's underflow warning), empty-buffer-fresh-pane succeeds trivially, `resume_from_seq > latest_seq` gap-exceeds.
- Files: `crates/tymux-core/src/replay_buffer.rs`

##### Task 2.1.1d: Global/per-pane byte-budget allocation functions — genuine hard cap, not a floor-grant (~4 min)
- Add `DEFAULT_REPLAY_BUFFER_BYTES: usize = 256 * 1024`, `MIN_REPLAY_BUFFER_BYTES: usize = 16 * 1024`, `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES: usize = 64 * 1024 * 1024`, and a `static GLOBAL_REPLAY_BUFFER_USED_BYTES: AtomicUsize`, matching `pane.rs:44-54`'s general accounting shape (an atomic used-counter, a `saturating_sub`'d "remaining" calc, `fetch_add`/`fetch_sub` on allocate/release).
- **Do not** copy `allocate_scrollback_budget`'s grant formula (`DEFAULT.min(remaining.max(MIN))`) verbatim — that formula always grants at least `MIN_SCROLLBACK_LINES` even once the global budget is exhausted, which is fine for scrollback (a `MIN` floor is justified there: zero scrollback breaks copy-mode) but is exactly the bug this task must avoid for the replay buffer, since nothing caps pane count and an unconditional floor-grant means `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES` can be overshot without limit as pane count grows.
- Instead, `allocate_replay_budget() -> usize` must compute `let remaining = GLOBAL_REPLAY_BUFFER_BUDGET_BYTES.saturating_sub(used); let granted = DEFAULT_REPLAY_BUFFER_BYTES.min(remaining);` — i.e. no `.max(MIN_REPLAY_BUFFER_BYTES)` floor at all, so `granted` legitimately falls below `MIN_REPLAY_BUFFER_BYTES`, down to `0`, once `remaining` is small or zero. (`MIN_REPLAY_BUFFER_BYTES` still exists as a documented "typical minimum under normal, non-exhausted conditions" constant for comments/tests, it just never forces a floor on the return value.) `release_replay_budget(bytes: usize)` mirrors `release_scrollback_budget` exactly (plain `fetch_sub`).
- A pane granted `0` bytes is a safe, fully-supported state: its `ReplayBuffer` can never retain an entry, so `replay_since()` always returns `GapExceeded`, which Epic 2.2.2's fallback path already handles.
- Files: `crates/tymux-core/src/replay_buffer.rs`

#### Story 2.1.2: Wire `ReplayBuffer` into `Pane`'s reader thread
**As a** daemon implementer, **I want** the replay buffer populated in strict push-then-broadcast order on the same thread that already owns output delivery, **so that** a resuming reader's buffer-tail read can never disagree with what any live subscriber has already received (architecture.md Q3).
**Acceptance Criteria**:
- `Pane` gains a `replay: Mutex<ReplayBuffer>` field, granted `allocate_replay_budget()` at spawn and released on `Drop`.
  - *Given* a freshly spawned `Pane`, *When* `pane.replay_since(0)` is called before any output has occurred, *Then* it returns `ReplayOutcome::InWindow { chunks: vec![], tail_seq: 0 }`.
- The reader thread pushes into `replay` strictly *before* calling `output_tx.send`, both *after* the `parser` lock has already been dropped (a wholly separate, never-co-acquired lock, per the Pattern Decisions row).
  - *Given* a pane that has processed one chunk of output, *When* a second `Attach` call arrives and calls `pane.replay_since(0)` concurrently with a third chunk being read, *Then* the returned `chunks` and `tail_seq` are self-consistent (every chunk in `chunks` has `seq <= tail_seq`, and no chunk is present in `chunks` whose broadcast delivery a fresh subscriber from *before* this call would have missed).
**Files**: `crates/tymux-core/src/pane.rs`, `crates/tymux-core/src/lib.rs`

##### Task 2.1.2a: Add the `replay` field and reader-thread push (~5 min)
- In `crates/tymux-core/src/pane.rs`: add `replay: Mutex<crate::replay_buffer::ReplayBuffer>` to the `Pane` struct; initialize with `ReplayBuffer::new(allocate_replay_budget())` alongside `scrollback_lines`'s existing initialization at `spawn_internal` (`pane.rs:203-229`).
- In the reader thread's read loop (`pane.rs:246-255`), after the `let seq = { ... };` block (parser lock already dropped) and before `output_tx.send`, add `pane_for_reader.replay.lock().unwrap().push(seq, &buf[..n]);`.
- Files: `crates/tymux-core/src/pane.rs`

##### Task 2.1.2b: `Pane::replay_since` public method (~3 min)
- `pub fn replay_since(&self, resume_from_seq: u64) -> ReplayOutcome { self.replay.lock().unwrap().replay_since(resume_from_seq, self.output_seq.load(Ordering::SeqCst)) }` — note this reads `output_seq` directly (not via `parser`'s lock, since resume doesn't need the grid), consistent with `size()`'s existing precedent of reading atomics without `parser`.
- Files: `crates/tymux-core/src/pane.rs`

##### Task 2.1.2c: Release the replay budget on `Drop` (~2 min)
- Extend `impl Drop for Pane` (`pane.rs:481-494`) to also call `release_replay_budget(...)` for the pane's granted `replay_budget_bytes` (store the granted amount alongside `scrollback_lines` as a new `replay_budget_bytes: usize` field, since `ReplayBuffer` itself only stores its ceiling, not necessarily exposed for `Drop` to read back out — simplest: store the granted `usize` on `Pane` directly, mirroring `scrollback_lines`).
- Files: `crates/tymux-core/src/pane.rs`

##### Task 2.1.2d: Expose `replay_buffer` module and integration test (~5 min)
- Add `mod replay_buffer;` to `crates/tymux-core/src/lib.rs` (private, matching `mod pane;`'s existing visibility — no `pub use` needed unless `tymuxd` needs `ReplayOutcome` directly, in which case add `pub use replay_buffer::ReplayOutcome;`).
- New test in `pane.rs`'s existing `#[cfg(test)] mod tests`: spawn a pane, write input producing several output chunks, assert `pane.replay_since(0)`'s `chunks` byte-concatenate to exactly what `pane.subscribe()` would have delivered live over the same window (mirrors the existing `snapshot_with_seq_should_return_grid_and_sequence_matching_the_last_broadcast_chunk...` test's settle-and-compare style, `pane.rs:657-716`).
- Files: `crates/tymux-core/src/lib.rs`, `crates/tymux-core/src/pane.rs`

---

### Epic 2.2: Daemon resume handling in `attach()`
**Goal**: Branch `attach()` on whether the first `AttachRequest` carries `resume_from_seq`, reusing `forward_step_for_output_result`'s skip-threshold comparison logic unchanged (its `Output` payload construction gains the real `seq` value, fixing Task 1.1.1b's placeholder — see Task 2.2.1c).

#### Story 2.2.1: Subscribe-then-replay ordering, threaded into the existing forwarding loop
**As a** reconnecting client, **I want** the daemon to serve exactly the chunks I missed, byte-identical to what a client that never disconnected would have seen, **so that** reconnect is cheaper than a full `CapturePane` round-trip.
**Acceptance Criteria**:
- When `resume_from_seq` is present and in-window, `attach()` calls `pane.subscribe()` before `pane.replay_since(...)`, exactly mirroring the existing `subscribe()`-before-`snapshot_with_seq()` ordering (`main.rs:659-660`).
  - *Given* a pane with output chunks at seq 1-5 already produced and retained, *When* a client sends `AttachRequest { pane_id, resume_from_seq: Some(3) }` as its first message, *Then* the daemon's response stream delivers `AttachEvent`s each carrying BOTH the legacy `output` bytes field (unseq'd, byte-identical to today, ignored by new clients) AND the new `output_chunk` field populated with `OutputChunk{seq:4,...}`, `OutputChunk{seq:5,...}`, and then any further live output — with no gap and no duplicate in `output_chunk`'s seq stream, verified by comparing `output_chunk` bytes against what a never-disconnected client's stream would show over the same window.
- The live-loop threshold (`snapshot_seq` parameter to `forward_step_for_output_result`) becomes `ReplayOutcome::InWindow::tail_seq` on the resume path, `snapshot_with_seq()`'s seq on the no-token path. **Precisely what stays unchanged**: `forward_step_for_output_result`'s signature and its `seq <= threshold` skip/dedup logic — only the threshold's *source* differs between paths. **What does change**, and must not be described as "unmodified": the function's payload construction now populates BOTH fields on the same `AttachEvent` — the untouched legacy `output` bytes field exactly as before, AND the new `output_chunk: Some(OutputChunk{seq, data})` field carrying the real seq. There is no placeholder to replace (Task 1.1.1b left the legacy field's call sites untouched); this dual-write is exactly what Task 2.2.1c does.
  - *Given* a resume request producing `tail_seq: 5`, *When* the live broadcast subsequently delivers a chunk with `seq: 5` (already covered by the replay), *Then* `forward_step_for_output_result` returns `ForwardStep::Skip`, not a duplicate emit.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 2.2.1a: Extract `resume_from_seq` from the first `AttachRequest` (~3 min)
- In `attach()` (`main.rs:614-635`), after parsing `pane_id_str`, also read `first.resume_from_seq` (the new field, outside the `oneof` matched at line 624). This ordering is load-bearing, not incidental: the existing `match first.payload { Some(PaneId(id)) => id, _ => return Err(invalid_argument(...)) }` at `main.rs:624-631` already unconditionally rejects any first message lacking `pane_id`, so `resume_from_seq` is never read on a request that omits `pane_id` — do not reorder this to read `resume_from_seq` first (see Pattern Decisions' `AttachRequest.resume_from_seq placement` row).
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.2.1d: Regression test — `resume_from_seq` without `pane_id` is rejected before ever being read (~3 min)
- New `#[tokio::test]` in `main.rs`'s test module: send a first `AttachRequest` with `payload: None` and `resume_from_seq: Some(5)`, assert the RPC fails with `invalid_argument`/"first Attach message must set pane_id" — confirming the type-representable-but-meaningless combination flagged against `AttachRequest.resume_from_seq`'s proto placement is unreachable in practice, closing the concern with a falsifiable check rather than only a doc note.
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.2.1b: Branch on resume presence — subscribe-then-replay path (~5 min)
- Replace the unconditional `let (pane_snapshot, snapshot_seq) = pane.snapshot_with_seq();` block (`main.rs:659-660`) with: `let mut output_rx = pane.subscribe();` (unchanged, always first) then `match first.resume_from_seq { Some(seq) => match pane.replay_since(seq) { ReplayOutcome::InWindow { chunks, tail_seq } => { /* Epic 2.2.1c */ } ReplayOutcome::GapExceeded { .. } => { /* Epic 2.2.2 */ } }, None => { /* existing snapshot_with_seq path, unchanged */ } }`.
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.2.1c: Dual-write BOTH `output` (legacy, unchanged) and `output_chunk` (new, seq'd) in `forward_step_for_output_result`'s `Emit` path (~4 min)
- Update `forward_step_for_output_result` (`main.rs:325-349`) to construct an `AttachEvent` on `Emit` that populates BOTH fields: `output: bytes.clone()` exactly as it does today (unseq'd, byte-identical, zero behavior change for any client that only reads field 1), AND `output_chunk: Some(OutputChunk { seq, data: bytes })` for new clients. This is the one production call site, and this dual-write is what the plan's additive dual-field approach depends on — there is no placeholder to resolve, since Task 1.1.1b left the legacy field's call sites untouched.
- Note: this changes only the payload construction (now populating two fields instead of one), not the function's signature or its `seq <= threshold` skip/dedup logic (see Story 2.2.1's acceptance criteria for the precise unchanged/changed split).
- Files: `crates/tymuxd/src/main.rs`

#### Story 2.2.2: `GapExceeded` fallback path
**As a** reconnecting client whose resume request is too old to serve, **I want** an explicit signal followed by a fresh snapshot, **so that** I never silently render an incomplete-but-looks-complete screen.
**Acceptance Criteria**:
- When `ReplayOutcome::GapExceeded` is returned, the daemon sends a `GapExceeded{oldest_available_seq}` event, then falls back to exactly today's `snapshot_with_seq()` priming path (unchanged).
  - *Given* a pane whose replay buffer's oldest retained seq is 100, *When* a client sends `resume_from_seq: Some(10)`, *Then* the response stream's first two events are `GapExceeded{oldest_available_seq: 100}` followed by a `Snapshot` event — matching what a fresh no-token attach would send as its very first event.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 2.2.2a: Send `GapExceeded` then fall back to the snapshot priming path (~4 min)
- In the `ReplayOutcome::GapExceeded { oldest_available_seq }` arm from Task 2.2.1b, send `AttachEvent { payload: Some(GapExceeded(GapExceeded { oldest_available_seq: oldest_available_seq.unwrap_or(0) })) }` via `tx.send(...)`, then fall through to the identical `pane.snapshot_with_seq()` + priming-`Snapshot`-event code the `None` branch already uses (factor into a small shared closure/function to avoid duplicating the priming logic across three branches).
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.2.2b: Unit test for the fallback wiring (~4 min)
- New `#[tokio::test]` in `main.rs`'s test module (near `attach_should_emit_snapshot_first_with_no_duplicated_bytes_when_output_streams_concurrently_not_after_settling`, `main.rs:1518`): spawn a pane, produce enough output to evict an old seq from the replay buffer (small test-only budget), attach with a stale `resume_from_seq`, assert the first two received events are `GapExceeded` then `Snapshot`.
- Files: `crates/tymuxd/src/main.rs`

---

### Epic 2.3: Replay-to-live handoff safety
**Goal**: Never reintroduce the Ctrl-d/block-forever bug class (`docs/reviews/is-it-ready-2026-07-13.md:24-32`) at the point where buffered replay chunks hand off to the live `select!` loop.

#### Story 2.3.1: Race `wait_exit()` across the entire replay-drain loop, not just after it
**As a** reconnecting client to a pane with a large backlog, **I want** the daemon to still notice the pane exiting mid-replay, **so that** my stream terminates instead of hanging.
**Acceptance Criteria**:
- The loop that sends buffered replay chunks uses the same `biased tokio::select!` against `pane.wait_exit()` that the live loop already uses (`main.rs:689-717`), for every chunk, not as a separate pre-loop.
  - *Given* a resumed client receiving a 500-chunk replay backlog, *When* the pane's child process exits after chunk 200 has been sent but before chunk 500, *Then* the client's stream still receives an `Exited` event and terminates — it does not hang waiting for `output_rx.recv()` or block until the full backlog drains.
- The replay-drain loop does NOT gain its own `heartbeat_interval` branch — the server's `Heartbeat` event stays exclusively a live-loop concern (Epic 3.2's `forward_handle` third branch, untouched by this Epic). This is a deliberate resolution, not an oversight: the client-side idle timer (Task 6.1.1d) resets on *any* received event, not `Heartbeat` alone, so a steady stream of legitimate `OutputChunk` events during replay already counts as proof-of-life and keeps the idle timer from firing — a real gap would only exist if the replay loop could go silent for `heartbeat_timeout` (45s) without sending a chunk or hitting `wait_exit()`, which the per-chunk `select!` above already rules out on the daemon's own sending side.
  - *Given* a resume delivering a large backlog where each buffered `OutputChunk` send completes well within `heartbeat_timeout` (45s) of the previous one, *When* the replay-drain loop runs without ever hitting the live loop's `heartbeat_interval` branch, *Then* the client's idle timer (Task 6.1.1d) is never triggered spuriously by the replay itself — verified by Task 6.1.1e's test double, which sends `OutputChunk` events on a cadence slower than `heartbeat_interval` (15s) but faster than `heartbeat_timeout` (45s) and asserts no reconnect fires.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 2.3.1a: Restructure the replay-chunk-sending loop with the biased `wait_exit()` race (~5 min)
- In the `ReplayOutcome::InWindow { chunks, tail_seq }` arm (Task 2.2.1b), replace a plain `for (seq, data) in chunks { forward_tx.send(...).await?; }` with a loop using `tokio::select! { biased; result = forward_tx.send(Ok(AttachEvent{payload: Some(attach_event::Payload::OutputChunk(OutputChunk{seq, data}))})) => { if result.is_err() { return; } } _ = pane_for_exit.wait_exit() => { /* send Exited event, same as main.rs:709-715, then return */ } }` per chunk — factor the `Exited`-event-send-and-return logic (currently main.rs:702-716) into a small helper so both the replay loop and the live loop call the same code, not two copies. Note: unlike Task 2.2.1c's live-loop dual-write, this replay loop only needs to populate the new `output_chunk` field, not the legacy `output` field — a client only reaches this branch by having sent `resume_from_seq` in the first place (Task 2.2.1b), which no old, `output`-only client ever does.
- Do NOT add a `heartbeat_interval` branch to this loop — see Story 2.3.1's acceptance criteria for why: `OutputChunk` events sent here already serve as the client's proof-of-life signal for Task 6.1.1d's idle timer, so a separate heartbeat during replay is redundant, not missing.
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.3.1b: Regression test — pane exits mid-replay-of-large-backlog (~5 min)
- New `#[tokio::test]`, mirroring `attach_streams_output_and_signals_exit` (`main.rs:1652`) and `attach_should_emit_snapshot_first_with_no_duplicated_bytes_...` (`main.rs:1518`)'s structure: spawn a pane, produce a backlog large enough to require multiple replay chunks, kill the pane's child process partway through, attach with a resume token covering the whole backlog, assert the stream still terminates with an `Exited` event within a bounded timeout (not a hang).
- Files: `crates/tymuxd/src/main.rs`

---

### Epic 2.4: Backward compatibility — no resume token behaves identically to today
**Goal**: Guarantee `stapler-squad`'s already-merged `BackendTymux`/`ReconnectLoop` (PR #37), which never sends a resume token, keeps getting exactly today's full-`CapturePane`-reseed behavior, unmodified.

#### Story 2.4.1: Absent `resume_from_seq` is byte-for-byte identical to pre-feature `attach()`
**As** `stapler-squad`'s `BackendTymux`, **I want** my existing reconnect behavior to keep working unmodified after this feature ships, **so that** I am not forced to adopt resume tokens on tymux's timeline.
**Acceptance Criteria**:
- A client whose first `AttachRequest` omits `resume_from_seq` (`None`) receives the exact same event sequence as today: priming `Snapshot` first, then live `Output`/`OutputGap`/`Exited` events, with the same `seq <= snapshot_seq` dedup skip.
  - *Given* a pane with existing output already produced, *When* a client attaches with `AttachRequest { payload: Some(PaneId(...)), resume_from_seq: None }`, *Then* the first event received is a `Snapshot`, and the byte content/ordering of subsequent `Output` events is identical to what the pre-feature code path produced (verified by running the existing `attach_should_emit_snapshot_first_with_no_duplicated_bytes_when_output_streams_concurrently_not_after_settling` test, `main.rs:1518`, unmodified and still passing).
- The legacy `output` field's bytes, as received by any client that only reads field 1 (all pre-`v0.2.0` `clients/go` consumers, including `stapler-squad`'s `BackendTymux`), are byte-for-byte identical to today's raw pty output and never contaminated by `OutputChunk`'s own wire framing — because `bytes output = 1;` is genuinely untouched at the wire level, not just field-number-preserved.
  - *Given* the dual-write from Task 2.2.1c, *When* an old client's generated stub decodes field 1 as `bytes`, *Then* the decoded bytes are exactly the raw pty chunk, with no risk of misinterpreting `OutputChunk`'s varint-tag-plus-length-delimited framing as literal terminal output (verified by Task 2.4.1c's compat-assertion test — this is the falsifiable check the pre-mortem's P1 finding called for).
**Files**: `crates/tymuxd/src/main.rs`

##### Task 2.4.1a: Confirm the `None` branch is untouched logic, just relocated (~3 min)
- Verify Task 2.2.1b's `None` arm calls exactly the pre-existing `pane.snapshot_with_seq()` + priming-`Snapshot`-event code with no behavioral change — a pure refactor, not a rewrite. Diff against pre-feature `main.rs` to confirm no incidental behavior change (e.g. event ordering, error handling).
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.4.1b: Regression-run every pre-existing `attach()` test unmodified (~3 min)
- Run `cargo test -p tymuxd` and confirm every test that existed before this feature (`attach_should_not_emit_output_gap_event_when_consumer_keeps_pace`, `attach_streams_output_and_signals_exit`, `attach_should_emit_snapshot_first_with_no_duplicated_bytes_...`, etc.) still passes with ZERO modification — not even mechanical — since `AttachEvent::Output(bytes)`'s construction and shape are completely untouched by this feature; only the new, separate `output_chunk` field is added at the one call site Task 2.2.1c touches.
- Files: `crates/tymuxd/src/main.rs`

##### Task 2.4.1c: Compat-assertion test — legacy `output` bytes are exactly raw pty output, uncontaminated by `OutputChunk` framing (~4 min)
- New `#[tokio::test]` in `main.rs`'s test module, alongside Task 2.4.1b's regression tests: attach to a pane with known, marked output (post Task 2.2.1c's dual-write), extract only the legacy `output: Vec<u8>` field from the received `AttachEvent`s — the field an old `clients/go@v0.1.0` stub would read — and assert those bytes concatenate to exactly the raw pty output, byte for byte, with no embedded protobuf sub-message framing (no varint seq tag, no length-delimited `data` prefix) anywhere in them. This does not require an actual `clients/go@v0.1.0` binary; a Rust-side assertion on the `output` field's raw contents is sufficient and converts the pre-mortem's P1 finding's "old clients are unaffected" claim from an assumption into a falsifiable check (see pre-mortem.md #1 and ADR-001, revised).
- Files: `crates/tymuxd/src/main.rs`

---

### Epic 2.5: Test infrastructure — `TestDaemon` transport-drop capability
**Goal**: Close the gap validation.md's own pass self-flagged: today's `crates/tymux-e2e` `TestDaemon` (`crates/tymux-e2e/src/daemon.rs`) has exactly one way to make the daemon unreachable — `Drop`, which `kill()`s the subprocess and destroys its in-memory pane/`ReplayBuffer` state along with it. Several planned E2E tests (successful resume, `GapExceeded`-on-stale-token, give-up-after-backoff — validation.md REQ-13, AC1.1, AC3.1, AC3.4) need the daemon to go briefly unreachable *without* losing that state, which `Drop` structurally cannot provide. This is infrastructure the validation.md test suite depends on, not an afterthought discovered mid-implementation.

#### Story 2.5.1: `TestDaemon` gains a controllable unreachability toggle that preserves daemon state
**As an** E2E test author, **I want** to simulate a transport-level drop and later restore, **so that** resume/backoff/give-up scenarios can be driven against a daemon whose pane state and `ReplayBuffer` survive the gap, exactly like a real network blip or brief daemon-restart would.
**Acceptance Criteria**:
- `TestDaemon` gains a way to become briefly unreachable and later reachable again without the underlying `tymuxd` subprocess (and its in-memory pane/`ReplayBuffer` state) ever restarting.
  - *Given* a `TestDaemon` with an attached pane that has produced retained replay history, *When* the test calls the new drop-simulation method, then later calls the restore method within the replay buffer's retention window, *Then* a client that reconnects and resumes after the restore receives exactly the buffered chunks the daemon retained across the gap — proving the daemon process (and its state) never actually stopped.
**Files**: `crates/tymux-e2e/src/daemon.rs`

##### Task 2.5.1a: Add a thin TCP-proxy layer in front of `tymuxd` with a togglable drop state (~5 min)
- In `crates/tymux-e2e/src/daemon.rs`, insert a small local TCP proxy between `TestDaemon::addr` (what test clients connect to) and the real `tymuxd` subprocess's own listener (bound to a second, internal-only ephemeral port): the proxy accepts on `addr` and forwards bytes to/from the internal port while enabled, and stops accepting/forwarding (closing any live forwarded connections) while disabled — `tymuxd` itself keeps running untouched throughout, so its pane/`ReplayBuffer` state is never affected by the toggle. Add `TestDaemon::simulate_drop(&self)` (disable forwarding, close in-flight connections) and `TestDaemon::restore(&self)` (re-enable forwarding) as the public toggle.
- Files: `crates/tymux-e2e/src/daemon.rs`

##### Task 2.5.1b: Smoke test — daemon state survives a simulated drop (~4 min)
- New test in `crates/tymux-e2e` (or a small `#[tokio::test]` inline in `daemon.rs`): spawn a `TestDaemon`, attach and produce output, call `simulate_drop()`, confirm new connection attempts fail/hang as expected, call `restore()`, confirm a fresh `Attach` with `resume_from_seq` still gets the pre-drop history back from the (never-restarted) daemon's `ReplayBuffer` — this is the harness-level proof that Epic 2.1-3's later E2E tests (validation.md REQ-13, AC1.1, AC3.1, AC3.4) can build on.
- Files: `crates/tymux-e2e/src/daemon.rs`

---

## Phase 3: Heartbeat & Grace Period

### Epic 3.1: Transport-level HTTP/2 keepalive
**Goal**: Configure tonic's built-in connection-level dead-peer detection on both server and client — buy, not build.

#### Story 3.1.1: Configure server and client keepalive
**As** the daemon, **I want** a genuinely dead TCP connection torn down within a bounded window, **so that** `forward_handle`/`input_handle` notice a vanished client even without app-level activity.
**Acceptance Criteria**:
- The production `Server::builder()` (`main.rs:947`) sets `.http2_keepalive_interval(Some(Duration::from_secs(30)))` and `.http2_keepalive_timeout(Duration::from_secs(10))`.
  - *Given* the daemon is running with this config, *When* a connected client's TCP path is silently cut (no FIN/RST, e.g. `iptables -j DROP` in a manual test), *Then* the server tears down that connection within approximately 40 seconds (interval + timeout), observable via `input_handle`'s stream ending and the existing `disconnect_tracker` insert firing.
- `tymux-cli`'s client connection sets `.http2_keep_alive_interval(Duration::from_secs(30))`, `.keep_alive_timeout(Duration::from_secs(10))`, `.keep_alive_while_idle(true)`.
  - *Given* a `tymux-cli attach` session to a pane the user hasn't typed in for over a minute (idle, no in-flight request), *When* the server becomes unreachable, *Then* the client's connection is torn down by keepalive rather than hanging indefinitely — verified by `keep_alive_while_idle(true)` being set, since without it hyper's default only pings while a request is in flight (stack.md's explicit finding).
**Files**: `crates/tymuxd/src/main.rs`, `crates/tymux-cli/src/main.rs`

##### Task 3.1.1a: Set server-side keepalive on the production `Server::builder()` (~3 min)
- At `main.rs:947`, chain `.http2_keepalive_interval(Some(Duration::from_secs(30))).http2_keepalive_timeout(Duration::from_secs(10))` onto `Server::builder()`. Leave the two test-helper `Server::builder()` sites (`main.rs:1030`, `main.rs:1658`) unchanged unless a specific test (Epic 3.1/3.2) needs a shorter interval.
- Files: `crates/tymuxd/src/main.rs`

##### Task 3.1.1b: Switch `tymux-cli`'s client construction to an explicit `Endpoint` with keepalive (~4 min)
- At `main.rs:272`, replace `TymuxServiceClient::connect(cli.addr).await?` with `let endpoint = tonic::transport::Endpoint::from_shared(cli.addr)?.http2_keep_alive_interval(Duration::from_secs(30)).keep_alive_timeout(Duration::from_secs(10)).keep_alive_while_idle(true); let channel = endpoint.connect().await?; let mut client = TymuxServiceClient::new(channel);`.
- Files: `crates/tymux-cli/src/main.rs`

---

### Epic 3.2: Application-level heartbeat + deferred viewport/geometry cleanup
**Goal**: A periodic server-sent `Heartbeat` event keeps `forward_handle` provably active even on an idle pane, and abrupt-drop cleanup is deferred by `grace_period_duration` so a prompt reconnect doesn't thrash window geometry.

#### Story 3.2.1: Server sends periodic `Heartbeat` `AttachEvent`s
**As a** client, **I want** proof-of-life events even when the pane is idle, **so that** I can distinguish "server slow" from "server actually gone" within a bounded window.
**Acceptance Criteria**:
- `forward_handle`'s `select!` loop gains a third branch: a `tokio::time::interval(Duration::from_secs(15))` tick that sends `AttachEvent { payload: Some(Heartbeat(Heartbeat {})) }`.
  - *Given* an attached client to a pane with no pty output for over 15 seconds, *When* 15 seconds elapse, *Then* the client receives a `Heartbeat` event with no `Output`/`Snapshot`/`Exited` content, and the connection remains open (not an error).
**Files**: `crates/tymuxd/src/main.rs`

##### Task 3.2.1a: Add the heartbeat interval branch to `forward_handle`'s `select!` (~4 min)
- In the `tokio::select! { biased; ... }` block (`main.rs:689-717`), add a `_ = heartbeat_interval.tick() => { if forward_tx.send(Ok(AttachEvent{payload: Some(Heartbeat(Heartbeat{}))})).await.is_err() { return; } }` branch, constructing `let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(15));` before the loop starts.
- Files: `crates/tymuxd/src/main.rs`

##### Task 3.2.1b: Integration test — heartbeat arrives on an idle pane within the interval (~4 min)
- New `#[tokio::test]`: attach to a pane with no input, advance a test clock (or use `tokio::time::pause()`/`advance()`) past 15 seconds, assert a `Heartbeat` event is received before any `Output` event.
- Files: `crates/tymuxd/src/main.rs`

#### Story 3.2.2: Defer `unregister_viewport`/`recompute_window_geometry` by `grace_period_duration`
**As a** user with multiple panes attached to one window, **I want** a brief disconnect-then-reconnect not to visibly shrink and regrow my window, **so that** transient network blips don't cause geometry thrash.
**Acceptance Criteria**:
- `input_handle`'s stream-end path (`main.rs:762-771`) no longer calls `unregister_viewport`/`recompute_window_geometry` immediately; instead it spawns a deferred task that calls them after `grace_period_duration` (default 60s), tied to that specific `client_id` alone.
  - *Given* two clients attached to the same window, one disconnects, *When* it reconnects (getting a fresh `client_id`, reporting a new viewport) within `grace_period_duration`, *Then* the window's computed minimum geometry never transiently shrinks to reflect the old client's absence before growing back — the old `client_id`'s viewport entry is still counted until its own deferred cleanup fires, by which point the new entry already exists.
  - *Given* the same scenario, *When* the disconnected client does *not* reconnect within `grace_period_duration`, *Then* the deferred cleanup fires exactly once, `unregister_viewport`/`recompute_window_geometry` run, and a `tracing::info!` line logs the event with `pane_id`/`window_id`/`client_id`/`elapsed_ms`.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 3.2.2a: Replace immediate cleanup with a deferred `tokio::spawn` (~5 min)
- At `main.rs:768-771`, replace the immediate `engine_for_input.unregister_viewport(...); engine_for_input.recompute_window_geometry(...);` with `tokio::spawn(async move { tokio::time::sleep(grace_period_duration).await; engine_for_deferred.unregister_viewport(window_id, client_id); engine_for_deferred.recompute_window_geometry(window_id); tracing::info!(pane_id = %pane_id, window_id = %window_id, client_id = %client_id, elapsed_ms = grace_period_duration.as_millis() as u64, "grace period expired, deferred viewport cleanup fired"); });` — `grace_period_duration` sourced from `TymuxDaemon`, config'd via `TYMUXD_GRACE_PERIOD_MS` env var (default 60_000), mirroring `DEFAULT_DISCONNECT_REGRESSION_WINDOW`'s existing pattern (`main.rs:34-38, 61-65`).
- Files: `crates/tymuxd/src/main.rs`

##### Task 3.2.2b: Guard against the window having already closed (~3 min)
- Before the deferred task calls `unregister_viewport`/`recompute_window_geometry`, check the window still exists (e.g. via an existing `Engine` lookup) and skip the calls (with a `tracing::debug!` no-op log) if it doesn't — a closed window's cleanup is already handled by whatever code path closed it.
- Files: `crates/tymuxd/src/main.rs`

##### Task 3.2.2c: Integration test — quick reconnect shows no transient geometry shrink (~5 min)
- New `#[tokio::test]`: two clients attached to one window's two panes, one disconnects and immediately reconnects (new `Attach` call, same pane, fresh `client_id`, reports the same viewport), assert `recompute_window_geometry`'s observable result (e.g. via `WatchWindow`'s emitted `WindowLayoutEvent`) never shows the shrunk-then-grown intermediate state.
- Files: `crates/tymuxd/src/main.rs`

##### Task 3.2.2d: Integration test — no reconnect still performs cleanup, logged (~4 min)
- New `#[tokio::test]` with a shortened `grace_period_duration` (test-only override via the env var): disconnect without reconnecting, advance past the grace period, assert `unregister_viewport`/`recompute_window_geometry` effects are observable and an `info`-level log line was emitted (capture via `tracing_test` or an equivalent subscriber, matching this codebase's existing test-logging conventions if any exist — otherwise assert on the geometry effect alone).
- Files: `crates/tymuxd/src/main.rs`

---

### Epic 3.3: Verify the grace-period design is leak/DoS-safe by construction
**Goal**: Confirm pitfalls.md §4's "grace period never expires" DoS vector is structurally impossible under the deferred-per-disconnect-task design (Pattern Decisions row), not merely mitigated by a cap.

#### Story 3.3.1: Many rapid reconnect/drop cycles never hold cleanup off indefinitely
**As** the daemon operator, **I want** a client that repeatedly reconnects and drops to never prevent geometry cleanup from ever happening, **so that** this can't become a resource-exhaustion or stale-state vector.
**Acceptance Criteria**:
- Each disconnect's deferred cleanup task fires independently, `grace_period_duration` after *its own* disconnect — a subsequent disconnect from a reconnected client does not reset, extend, or cancel any earlier disconnect's pending cleanup.
  - *Given* a client disconnects and reconnects 10 times within one `grace_period_duration` window, *When* the first `grace_period_duration` elapses, *Then* the first disconnect's deferred cleanup fires exactly as scheduled (not delayed by the 9 subsequent disconnect/reconnect cycles), and the 9 later `client_id`s each have their own independently-scheduled cleanup, none of which race or block each other.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 3.3.1a: Integration test — rapid reconnect/drop cycles don't delay any individual cleanup (~5 min)
- New `#[tokio::test]`: simulate 10 rapid attach/detach cycles on the same pane within a shortened test `grace_period_duration`, assert every one of the 10 spawned deferred-cleanup tasks fires within its own expected window (bounded per-task timing assertions, not a single aggregate check) — proving there is no shared mutable deadline to reset.
- Files: `crates/tymuxd/src/main.rs`

---

## Phase 4: Observability

### Epic 4.1: Resume-outcome counter + structured logs
**Goal**: Give the three resume outcomes (resumed-from-buffer / gap-exceeded-fallback / no-resume-token-full-attach) real, tagged observability — not prose-only.

#### Story 4.1.1: `tymux_attach_resume_outcome_total`, tagged by outcome
**As an** operator (even a solo one), **I want** to see how often reconnects actually use the fast path, **so that** the replay buffer's sizing decisions have real usage data behind them eventually.
**Acceptance Criteria**:
- Each of `attach()`'s three branches (Task 2.2.1b: `InWindow`, `GapExceeded`, `None`) increments a distinct counter and logs via `tracing::info!` on change, mirroring `attached_sessions_gauge`'s exact convention (`main.rs:86-91`).
  - *Given* a client resumes successfully from the buffer, *When* `attach()`'s `InWindow` branch runs, *Then* `tymux_attach_resume_outcome_total{outcome="resumed_from_buffer"}` increments by 1 and a `tracing::info!` line reports the new value.
- The `GapExceeded` branch also emits `tracing::warn!` with `pane_id`/`resume_from_seq`/`oldest_available_seq` (Observability Plan's explicit requirement), distinct from the counter increment.
  - *Given* a resume request outside the retained window, *When* the `GapExceeded` branch runs, *Then* a `tracing::warn!` line is emitted containing all three named fields, in addition to the counter increment.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 4.1.1a: Add the tagged counter struct/fields to `TymuxDaemon` (~4 min)
- Add three `Arc<AtomicI64>` fields (or one `Arc<Mutex<HashMap<&'static str, AtomicI64>>>` if that reads cleaner) to `TymuxDaemon` alongside `attached_sessions_gauge` (`main.rs:41-56`), initialized in `TymuxDaemon::new` (`main.rs:60-72`).
- Files: `crates/tymuxd/src/main.rs`

##### Task 4.1.1b: Increment + log at each of the three branch points (~4 min)
- In Task 2.2.1b's three branches, increment the matching counter and `tracing::info!` the new value, matching `AttachedGaugeGuard::drop`'s wording style (`main.rs:86-91`).
- Files: `crates/tymuxd/src/main.rs`

##### Task 4.1.1c: `tracing::warn!` on the `GapExceeded` branch (~3 min)
- In Task 2.2.2a's `GapExceeded` handling, add the `tracing::warn!(pane_id = %pane_id, resume_from_seq = seq, oldest_available_seq, "resume request outside replay buffer retention")` line (this may already partially exist from Task 2.2.2a — confirm field names match the Observability Plan exactly).
- Files: `crates/tymuxd/src/main.rs`

---

#### Story 4.1.2: Cross-client backoff/heartbeat constant conformance check
**As a** maintainer of three independently-implemented clients (`tymux-cli`, `clients/ts`, `clients/go`), **I want** a check that each client's reconnect constants literally match ADR-004's numbers, **so that** the plan's own Success Metric ("all three clients exhibit identical reconnect behavior per one shared specification") is a verified property, not just a shared doc comment three implementers each independently transcribed (and could each independently mistranscribe).
**Acceptance Criteria**:
- Each of `tymux-cli` (Task 6.1.1b's helper), `clients/ts`, and `clients/go`'s backoff constants — initial delay, multiplier, cap, jitter range, give-up attempt count — are asserted, per client, to equal ADR-004 (revised)'s numbers (200ms / x2 / 8s / +/-20% / 14 attempts) by a test in that client's own test suite.
  - *Given* any one client's backoff constant is changed without updating the other two or ADR-004, *When* that client's own test suite runs, *Then* its conformance test fails, naming the mismatched constant and ADR-004's expected value — catching drift locally, per-client, rather than relying on cross-language comparison tooling.
- This is a flat constant-equality check per client, not a shared library or cross-language test runner — deliberately minimal scope, matching this project's no-crypto/no-shared-service precedent elsewhere in the plan (Pattern Decisions).
**Files**: `crates/tymux-cli/src/main.rs`, `clients/ts/test/backoff.test.ts` (new or existing test file, implementer's call), `clients/go/backoff_test.go` (new or existing test file, implementer's call)

##### Task 4.1.2a: Add a per-client backoff-constant conformance test, three call sites (~5 min each, ~15 min total)
- In each of the three clients, add a small test that hardcodes ADR-004 (revised)'s five numbers (200ms, x2, 8s cap, +/-20% jitter, 14 attempts) and asserts equality against that client's own exported/`const` backoff values (Task 6.1.1b's helper for `tymux-cli`; the equivalent constant/module in `clients/ts` and `clients/go`, added alongside their resume-support work in Epic 5.1/5.2). No shared test harness or script across languages — three independent, identically-shaped assertions is enough for this appetite.
- Files: `crates/tymux-cli/src/main.rs`, `clients/ts/test/backoff.test.ts`, `clients/go/backoff_test.go`

---

## Phase 5: Reference Clients

### Epic 5.1: `clients/ts` resume support
**Goal**: Exercise the resume path with real, working TypeScript code — a success metric, not optional polish.

#### Story 5.1.1: Extend the TS attach example and integration test to exercise resume
**As a** TypeScript client author, **I want** a working example of resuming after a drop, **so that** the resume path is proven cross-language, not just from Rust.
**Acceptance Criteria**:
- `runAttachDemo` (or a new sibling function) accepts an optional `resumeFromSeq` and sends it on the first `AttachRequest`.
  - *Given* a prior attach session produced output up to seq 5, *When* `runAttachDemo(paneId, { resumeFromSeq: 5 })` is called, *Then* the received chunks start at seq 6, matching what a continuously-attached client would have seen from that point on.
- A new integration test disconnects mid-stream, reattaches with the last-seen seq, and asserts byte-identical continuation.
  - *Given* a running daemon and a pane producing marked output, *When* the test aborts the first `AttachRequest`'s stream after receiving some output, records the last seq seen, then opens a second `Attach` with `resumeFromSeq` set to that value, *Then* the concatenated output from both streams exactly matches what one uninterrupted stream would have produced, with no gap or duplicate byte.
- A `GapExceeded` path test: resume with a seq already evicted from the buffer, assert a `GapExceeded` event precedes a fresh `Snapshot`.
  - *Given* a pane whose replay buffer has evicted seq 1 (small test-configured budget), *When* the test attaches with `resumeFromSeq: 1`, *Then* the first two received events are `gapExceeded` then `snapshot`.
**Files**: `clients/ts/examples/attach.ts`, `clients/ts/test/integration.test.ts`

##### Task 5.1.1a: Add `resumeFromSeq` param to the attach example (~4 min)
- In `clients/ts/examples/attach.ts`, add an optional second parameter (or options object) threading `resumeFromSeq` into the first yielded `AttachRequest`, and track the last-seen `seq` from received `OutputChunk`s in the returned result for callers to use on a subsequent call.
- Files: `clients/ts/examples/attach.ts`

##### Task 5.1.1b: Integration test — disconnect, reattach with resume token, assert byte-identical continuation (~5 min)
- New test in `clients/ts/test/integration.test.ts`: produce marked output, abort the stream partway, reattach with the recorded seq, concatenate both streams' output, assert equality against a reference uninterrupted stream captured separately.
- Files: `clients/ts/test/integration.test.ts`

##### Task 5.1.1c: `GapExceeded`-path test (~4 min)
- New test in `clients/ts/test/integration.test.ts`: configure (or rely on a small default in a test-only daemon build/env var) a small replay-buffer budget, produce enough output to evict early seqs, attach with a stale `resumeFromSeq`, assert `gapExceeded` then `snapshot` event ordering.
- Files: `clients/ts/test/integration.test.ts`

### Epic 5.2: `clients/go` resume support
**Goal**: The same proof, in Go — `clients/go` currently has no attach example at all, only `list-sessions`.

#### Story 5.2.1: Add an attach example and integration test exercising resume
**As a** Go client author, **I want** the same resume proof `clients/ts` has, **so that** the "same behavior across languages" success metric holds for Go too.
**Acceptance Criteria**:
- `clients/go/examples/attach/main.go` exists, mirroring `attach.ts`'s shape (connect, send `pane_id` + optional `resume_from_seq`, print received output, exit on `Exited`).
  - *Given* `go run ./examples/attach <pane_id>` with no resume flag, *When* it runs against a live daemon, *Then* it behaves identically to `clients/ts/examples/attach.ts`'s no-resume path (proves the RPC surface, not just resume specifically — `clients/go` has no attach coverage at all today).
- `clients/go/integration/integration_test.go` gains a resume-path test matching Story 5.1.1's disconnect/reattach/byte-identical assertion, and a `GapExceeded`-path test.
  - *Given* the same disconnect/reattach scenario as Task 5.1.1b, *When* run against the Go client, *Then* the same byte-identical-continuation assertion holds.
**Files**: `clients/go/examples/attach/main.go` (new), `clients/go/integration/integration_test.go`

##### Task 5.2.1a: Create `clients/go/examples/attach/main.go` (~5 min)
- New file, mirroring `clients/go/examples/list-sessions/main.go`'s connection-setup shape (h2c transport, `connect.WithGRPC()`) and `clients/ts/examples/attach.ts`'s attach-loop shape: send `pane_id` (and `resume_from_seq` if a CLI flag is set), print `OutputChunk.Data` as received, exit on `Exited`.
- Files: `clients/go/examples/attach/main.go`

##### Task 5.2.1b: Resume-path integration test (~5 min)
- New test in `clients/go/integration/integration_test.go`, matching Task 5.1.1b's structure: disconnect mid-stream, reattach with the last-seen seq, assert byte-identical continuation.
- Files: `clients/go/integration/integration_test.go`

##### Task 5.2.1c: `GapExceeded`-path test (~4 min)
- New test in `clients/go/integration/integration_test.go`, matching Task 5.1.1c.
- Files: `clients/go/integration/integration_test.go`

---

## Phase 6: tymux-cli Reconnect Loop

**Framing**: Per requirements.md's Appetite section, if the project overruns Medium appetite (1-2 weeks upper end), the **first** cut is Epic 6.2 only (cross-invocation persistence), not this whole phase — see the Dependency Visualization section above for the full two-step cut order and the open note on requirements.md's phrasing. Epics 6.1 (in-process reconnect loop) and 6.3 (CLI UX polish) stay in scope under that first cut: Epic 6.1 needs no disk persistence and fully auto-reconnects within one long-running `tymux attach` process's lifetime on its own. Phases 1-5 are independently complete and shippable without any of Phase 6 — the protocol, daemon-side resume machinery, and reference-client (`clients/ts`/`clients/go`) proof all stand on their own. `tymux-cli` today has **no reconnect loop at all** (confirmed: `attach_and_follow`'s loop only handles `AttachOutcome::SwitchTo`; a dropped stream hits `None => break AttachOutcome::Done` at `main.rs:544` and the process exits) — this phase adds one, with cross-invocation persistence (Epic 6.2) as real, separable scope that can be cut on its own without leaving `tymux-cli` back at zero reconnect capability. Do not start this phase until Phases 1-5 are merged and stable.

### Epic 6.1: In-process reconnect loop
**Goal**: Detect an unexpected stream end (as opposed to a deliberate detach) and reopen `Attach` with a resume token, using the shared backoff spec from proto doc comments (Task 1.1.1c / ADR-004).

#### Story 6.1.1: Detect unexpected drop, reopen with resume token, apply shared backoff
**As a** `tymux-cli` user, **I want** a brief network blip to recover automatically instead of dumping me back to my shell, **so that** attach feels durable, not fragile.
**Acceptance Criteria**:
- `attach()`'s inbound loop distinguishes a clean detach (explicit user action) from an unexpected stream end (`maybe_event?` returning an error, or `None` without a preceding deliberate-detach signal) and, for the latter, retries per the shared backoff spec (200ms start, x2, capped 8s, +/-20% jitter, 14 attempts) before giving up.
  - *Given* an active `tymux attach` session, *When* the underlying connection drops unexpectedly (simulated by killing the daemon and restarting it within a few seconds), *Then* the CLI reopens `Attach` with `resume_from_seq` set to the last seq it processed, and live output continues with no visible full-screen redraw for the resumed portion.
  - *Given* the daemon stays unreachable for longer than the backoff schedule allows (14 failed attempts, ~68.6s nominal), *When* the final attempt fails, *Then* the CLI exits with a clear, distinguishable error message (not a silent hang), matching `stapler-squad`'s own precedent of surfacing a distinguishable "backend unavailable" state after give-up — and any bytes accumulated in `pending_input` at that point are discarded, not flushed anywhere or persisted: there is no live connection left to deliver them to, and the process is about to exit (resolves ux.md AC3.7).
- A connection that goes silent without erroring or closing (e.g. a NAT/proxy keeping the TCP session looking alive with zero traffic) is also detected and triggers the same reconnect path — not just an explicit stream error/`None` (Task 6.1.1d's `heartbeat_timeout` idle timer).
  - *Given* an active `tymux attach` session whose underlying stream goes silent (no `Output`/`Heartbeat`/any event) without erroring or closing, *When* `heartbeat_timeout` (45s) elapses with no event received, *Then* the CLI treats this exactly like a stream-termination error and enters the same reconnect path Task 6.1.1a builds — it does not hang indefinitely with frozen output.
- A `GapExceeded` event received during a resumed attach prints `"\r\n[tymux: reconnect gap too large, resyncing]\r\n"` via a new `chrome_message_for_event` arm, immediately before the fresh snapshot redraw.
  - *Given* a resumed attach whose token is now outside the buffer's retention (e.g. a very long drop), *When* the CLI receives a `GapExceeded` event, *Then* it prints the above line and then renders the following `Snapshot` event as a normal full-screen redraw — no separate visible state beyond that one line.
- The existing Detach keybinding (`C-b d` by default, `config.rs:55`) works as an escape hatch during the retry loop's backoff/redial window, exactly as it does during a live session — pressing it exits cleanly via the same `AttachOutcome::Done` path, and the terminal is left in cooked (non-raw) mode afterward (resolves ux.md Surface 3 AC3.6).
  - *Given* the CLI is in the retry loop's backoff window (daemon unreachable, not yet given up), *When* the user presses the Detach binding, *Then* the retry loop exits immediately — it does not wait out the remaining backoff schedule — the CLI prints `"\r\n[tymux: detached]\n"`, and `crossterm::terminal::is_raw_mode_enabled()` is `false` once the function returns, closing the gap where previously only an external `SIGKILL`/`SIGTERM`/`SIGHUP` could interrupt backoff, none of which ran `RawGuard`'s `Drop`.
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1a: Distinguish clean detach from unexpected drop, and run the retry loop inline so `RawGuard`'s Drop guarantee and a Detach escape hatch both stay in scope (~10 min)
- In `attach()`'s `'attach_loop` (`main.rs:535-570`), track the last-received seq from each `OutputChunk` event; on `maybe_event?` returning an error or an unattributed `None`, instead of immediately `break AttachOutcome::Done`, transition into a reconnect attempt.
- **Decided, not an implementer's call**: the retry loop runs *inline inside `attach()`'s own function body* — it never returns control to `attach_and_follow` to reopen a fresh call. This is required, not just convenient: `_raw` (the `RawGuard`, `main.rs:520`), `stdin_rx`, and `reassembler` are all already-live locals of `attach()`; keeping the retry loop in that same scope is what lets the escape hatch below reuse those exact instances (no new channel, no new reassembler state to reconcile on reconnect) and inherit `RawGuard`'s scope-exit `Drop` guarantee on every exit path, including a give-up error propagated via `?` — the same zero-new-code property Surface 3 AC3.2 already relies on for the give-up path (ux.md).
- **The retry loop's `select!` also races stdin, closing the "no escape hatch during backoff" gap** (resolves ux.md Surface 3 AC3.6, previously left open). Each reconnect cycle — the backoff sleep from Task 6.1.1b's helper, followed by the actual `client.attach(...)` redial/resume attempt — races against `stdin_rx.recv()`:
  ```
  tokio::select! {
      biased;
      maybe_bytes = stdin_rx.recv() => {
          let Some(bytes) = maybe_bytes else { return Ok(AttachOutcome::Done) };
          for output in reassembler.process(&bytes) {
              match output {
                  ReassembledOutput::Action(Action::Detach) => {
                      writeln!(stdout, "\r\n[tymux: detached]")?;
                      stdout.flush()?;
                      return Ok(AttachOutcome::Done);
                  }
                  ReassembledOutput::Forward(bytes) => pending_input.extend_from_slice(&bytes),
                  ReassembledOutput::Action(_) => {} // see note below
              }
          }
      }
      attempt_result = async {
          tokio::time::sleep(backoff_delay_for_attempt(attempt)).await;
          reconnect_once(client, &pane_id, resume_from_seq).await
      } => { /* Ok(..) breaks the retry loop and resumes the live 'attach_loop, flushing pending_input as the first AttachRequest::Input on the new tx; Err(_) advances to the next attempt */ }
  }
  ```
  Racing the *combined* sleep-then-attempt future (not the sleep alone) means Detach also interrupts a reconnect attempt that's itself hanging (e.g. a slow/black-holed TCP connect) — the escape hatch covers the whole retry cycle, not only the gap between attempts.
- **Ordinary typed input during backoff is queued and flushed, not silently dropped**: a `pending_input: Vec<u8>` buffer (new local, scoped to the retry loop) accumulates `Forward`ed bytes while no live `tx` exists to send them on, and is sent as the first `AttachRequest::Input` once a reconnect attempt succeeds — this is required to avoid a regression: before this task, an *unpolled* `stdin_rx` at least left keystrokes sitting in the bounded channel (delayed, not lost); polling it now (needed for Detach) and then discarding non-Detach output would make that strictly worse (permanently lost). A non-Detach `Action` (e.g. `EnterCopyMode`, a split binding) fired during backoff is still discarded, not queued — those need a live `client`/RPC call to do anything, so there's no well-defined "apply on reconnect" semantics the way byte-forwarding has; this narrower simplification is not the blocker this task closes.
- When Detach fires during backoff, the exit path is exactly the live-session Detach arm's behavior (`main.rs:652-657`: same `"[tymux: detached]"` message, same `AttachOutcome::Done`) — no new `AttachOutcome` variant is introduced (see the Domain Glossary's new `Detach-during-backoff` entry).
- **`pending_input`'s fate on Detach-during-backoff — decided, not left implicit**: whatever bytes have accumulated in `pending_input` at the moment Detach fires are discarded, not flushed to `tx` (there is no live `tx` to flush to) and not persisted anywhere. This is deliberate, not a contradiction of `pending_input`'s stated purpose ("so forwarded input is not silently dropped," Domain Glossary above): that purpose is bridging a *temporary* drop long enough for the *same* attach session to reconnect and resume — it was never meant to survive the user's own deliberate exit. Once Detach is chosen there is no live pane left to deliver the queued keystrokes to; silently dropping them here is correct, symmetric with a live-session Detach today discarding anything already in flight. Resolves ux.md AC3.6's added clause.
- **`pending_input`'s fate on give-up/exhaustion — same resolution, same reasoning**: if instead all 14 backoff attempts are exhausted and the retry loop gives up (Story 6.1.1's second AC bullet above), `pending_input` is discarded the same way — there is no live connection left to deliver it to, and the process is about to exit. Resolves ux.md AC3.7.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1b: Implement the shared backoff policy as a small helper (~5 min)
- New small helper (function or struct) implementing exactly ADR-004 (revised)'s numbers: start 200ms, x2 multiplier each attempt, cap at 8s, +/-20% jitter, give up after 14 attempts — used by Task 6.1.1a's reconnect path.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1c: `GapExceeded` arm in `chrome_message_for_event` and the attach loop's match (~3 min)
- Add `attach_event::Payload::GapExceeded(_) => Some("\r\n[tymux: reconnect gap too large, resyncing]\r\n")` to `chrome_message_for_event` (`main.rs:812-818`), and a corresponding match arm in the attach loop (mirroring the existing `OutputGap` arm at `main.rs:561-564`) that prints it via `write!`/`stdout.flush()`.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1d: Client-side idle timer implementing `heartbeat_timeout` (~5 min)
- In `attach()`'s inbound loop (`main.rs:535-570`), replace the plain `let maybe_event = inbound.message().await;` (or equivalent) with a `tokio::select! { biased; event = inbound.message() => { /* existing handling, AND reset the idle deadline here */ } _ = tokio::time::sleep_until(idle_deadline) => { /* treat exactly like a stream-termination error: enter Task 6.1.1a's reconnect path */ } }`, where `idle_deadline = Instant::now() + heartbeat_timeout` (45s, ADR-004) is recomputed after *every* branch that yields an event — `Heartbeat`, `Output`/`OutputChunk`, `Snapshot`, any payload, not `Heartbeat` alone (this is what makes replayed `OutputChunk` events during Epic 2.3's replay-drain loop count as proof-of-life too — see Epic 2.3's Story 2.3.1 acceptance criteria).
- This closes pre-mortem.md finding #3 (P2): today's loop only reacts to `maybe_event?` returning an error or an unattributed `None` (the stream actually terminating) — a connection gone silent without erroring (a NAT/proxy keeping the TCP session looking alive with zero traffic) never triggered reconnect before this task.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1e: Test — silent-but-not-closed connection still reconnects within `heartbeat_timeout` + backoff (~5 min)
- New test using a test double/mock `Attach` stream that goes silent (never sends another event, never closes/errors) after the first message: assert the CLI's reconnect path fires within `heartbeat_timeout` (45s) + the first backoff delay (~200ms), not indefinitely.
- Extend the same test double to also cover Epic 2.3's resolution: a second scenario sends `OutputChunk` events on a cadence slower than `heartbeat_interval` (15s) but well within `heartbeat_timeout` (45s) and never a `Heartbeat` event — assert the CLI does NOT reconnect, confirming `OutputChunk` receipt alone resets the idle timer (Task 6.1.1d) and a real replay backlog can't spuriously trigger this path.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1f: Unit test — backoff schedule's nominal total is `>= grace_period_duration` (enforced invariant) (~3 min)
- New unit test alongside Task 6.1.1b's helper: compute `sum(backoff_schedule_delays)` for the 14-attempt schedule (200, 400, 800, 1600, 3200, 6400, then 8000 x 7 more, summing to 68,600ms) and assert it is `>= 60_000` (`grace_period_duration`, ADR-003's default — hardcoded here with a comment cross-referencing ADR-003, since `tymux-cli` and `tymuxd` are separate crates/processes with no shared constant to import, matching Task 4.1.2a's per-client conformance-check pattern). This is the explicit, enforced invariant pre-mortem.md finding #2 (P2) called for: a future change to either ADR-003's `grace_period_duration` or ADR-004's backoff numbers that reopens the "client gives up before the server-side grace period expires" gap now fails a test instead of silently regressing.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1g: Regression test — Detach during backoff exits cleanly with the terminal restored (~4 min)
- New test using the same test double/mock `Attach` stream approach as Task 6.1.1e (a stream that errors/closes to trigger the retry loop, with the daemon staying unreachable for at least one full backoff interval): feed a Detach byte sequence (`C-b d`, matching `config.rs`'s default) into the mock `stdin_rx` channel while the retry loop is mid-backoff. Assert the function returns `AttachOutcome::Done` well before the full 14-attempt/~68.6s schedule would otherwise elapse, and assert `crossterm::terminal::is_raw_mode_enabled() == false` afterward — mirroring AC3.2's existing give-up-path regression test (ux.md), applied to this new exit path instead of the give-up path.
- A second case in the same test (or a sibling test): feed ordinary (non-Detach) bytes during backoff, then let a reconnect attempt succeed; assert those bytes arrive as the pane's first received input on the new stream (`pending_input` flush) rather than being lost.
- A third case in the same test (or a sibling test): feed ordinary (non-Detach) bytes into `pending_input` during backoff, then feed a Detach byte sequence before any reconnect attempt succeeds; assert (a) no `AttachRequest::Input` is ever sent for those bytes (there is no live `tx` for the retry loop to send on) and (b) nothing is written to disk or any other persistence — confirming the discard decided in Task 6.1.1a's new bullet, not just asserting the earlier flush-on-success case's absence.
- Files: `crates/tymux-cli/src/main.rs`

### Epic 6.2 (CUT CANDIDATE IF APPETITE OVERRUNS): Cross-invocation persistence
**Goal**: Since each `tymux attach` invocation is a fresh process, persist `pane_id -> last-seen seq` to disk so a *manual* reattach (not just Epic 6.1's in-process auto-reconnect) can also resume. Per requirements.md's Appetite section, this is the first thing cut if the project overruns Medium appetite — Epics 6.1 and 6.3 are unaffected by that cut (see Phase 6's Framing paragraph and the Dependency Visualization section).

#### Story 6.2.1: Persist and reload `ResumeState` across CLI invocations
**As a** `tymux-cli` user who manually re-runs `tymux attach` after their terminal was closed, **I want** the reattach to resume from where I left off, **so that** I don't lose output that happened while I was disconnected.
**Acceptance Criteria**:
- A new `resume_state_path()` follows `config.rs:207`'s `$XDG_STATE_HOME`-on-macOS-aware pattern exactly.
  - *Given* `$XDG_STATE_HOME` is set, *When* `resume_state_path()` is called on any platform (including macOS, where `dirs::state_dir()` alone would silently ignore the override), *Then* it honors the override, matching `persistence::default_sessions_dir`'s existing regression test pattern (`persistence.rs:351`).
- Before exiting (on any `AttachOutcome::Done` following real output, not on a fresh attach with nothing received), the CLI writes the last-seen `pane_id -> seq` pair to the resume-state file.
  - *Given* a `tymux attach <pane_id>` session that received output up to seq 20 before the user closed their terminal, *When* the process exits, *Then* the resume-state file on disk contains `{"<pane_id>": 20}` (or equivalent structured shape).
- On a fresh `tymux attach <pane_id>` invocation, if a stored seq exists for that `pane_id`, the CLI sends it as `resume_from_seq` on the first `AttachRequest`.
  - *Given* the resume-state file contains seq 20 for `pane_id`, *When* the user runs `tymux attach <pane_id>` again, *Then* the first `AttachRequest` sets `resume_from_seq: Some(20)`, and the terminal shows continued output rather than a fresh full-screen `CapturePane` redraw (unless a `GapExceeded` fallback occurs).
**Files**: `crates/tymux-cli/src/resume_state.rs` (new), `crates/tymux-cli/src/main.rs`

##### Task 6.2.1a: `resume_state_path()` following the existing XDG pattern (~4 min)
- New file `crates/tymux-cli/src/resume_state.rs`: `pub fn resume_state_path() -> Option<PathBuf>`, checking `$XDG_STATE_HOME` explicitly first (matching `config.rs:207-213`'s `default_config_path` and `persistence.rs:317-330`'s `default_sessions_dir`), falling back through `dirs::state_dir()`, joined with `tymux/resume_state.json`.
- Files: `crates/tymux-cli/src/resume_state.rs`

##### Task 6.2.1b: Read/write `ResumeState` around each attach (~5 min)
- In `resume_state.rs`: `pub struct ResumeState(HashMap<String, u64>)` (pane_id -> last seq) with `load() -> Self` (empty on missing/corrupt file, never a hard error) and `save(&self)` (atomic temp-file-then-rename, matching `persistence.rs`'s existing durability pattern). Wire `load()` before sending the first `AttachRequest` in `attach()` and `save()` after the loop ends with any new max seq observed.
- Files: `crates/tymux-cli/src/resume_state.rs`, `crates/tymux-cli/src/main.rs`

##### Task 6.2.1c: Integration test — kill and re-run CLI, assert resume without full redraw (~5 min)
- New test (or manual-verification note if the CLI's test harness can't easily drive a real subprocess attach — implementer's call based on existing CLI test infrastructure): run `tymux attach`, produce marked output, kill the process, re-run `tymux attach` against the same pane, assert the resume-state file was read and `resume_from_seq` was sent (observable via a daemon-side log assertion or a `GapExceeded`-absence check).
- Files: `crates/tymux-cli/src/resume_state.rs`

### Epic 6.3: CLI UX polish
**Goal**: Confirm the resolved UX research (ux.md) is actually implemented as specified — replay invisible on the happy path, visible only on `GapExceeded`.

#### Story 6.3.1: Replay renders through the existing live-output path, no new visible chrome
**As a** `tymux-cli` user, **I want** a successful resume to look exactly like nothing happened, **so that** reconnect doesn't feel like a distinct, jarring event.
**Acceptance Criteria**:
- Replayed `OutputChunk` events render via the exact same code path the live-output path already uses to extract bytes and call `stdout.write_all(...)` (`main.rs:552`) — no spinner, banner, or separate code path. The CLI, as an updated/new client, reads the new `output_chunk` field (not the legacy `output` bytes field it never needs — see Story 1.1.1 / ADR-001, revised).
  - *Given* a successful resume delivering 3 buffered chunks followed by live output, *When* the CLI processes them, *Then* all 3 buffered chunks and subsequent live output are written via the identical code path, with no additional bytes (no "resuming..." banner) inserted before them.
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 6.3.1a: Confirm no separate render path was introduced for replay (~2 min)
- Code-review check (not a new code change if Task 6.1.1a was implemented correctly): verify buffered replay `OutputChunk` events flow through the same `Some(attach_event::Payload::OutputChunk(chunk)) if copy_mode.is_none() => { stdout.write_all(&chunk.data)?; ... }` arm — updated from the pre-feature `Output(bytes)` arm (`main.rs:551-554`) to read the new `output_chunk` field, per Story 1.1.1/ADR-001 (revised) — as live output uses, with no special-cased branch for "this came from replay."
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.3.1b: Unit test `chrome_message_for_event`'s new `GapExceeded` arm (~3 min)
- New test alongside the existing `chrome_message_for_event_is_none_for_output_bytes` (`main.rs:972`): assert `chrome_message_for_event(&attach_event::Payload::GapExceeded(GapExceeded{oldest_available_seq: 0}))` returns `Some("\r\n[tymux: reconnect gap too large, resyncing]\r\n")`.
- Files: `crates/tymux-cli/src/main.rs`
