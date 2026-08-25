# Research: Features — attach-resume-protocol

Agent 2 (Features), SDD Phase 2. Scope: what partial building blocks already
exist in-repo, what industry precedent applies, and what edge cases the
Phase 3 design needs to explicitly decide rather than discover in production.

## 1. Existing in-repo building blocks

Confirmed by reading the code (not assumed):

- **`Pane::output_seq`** (`crates/tymux-core/src/pane.rs:111`) — a monotonic
  `AtomicU64`, incremented by the pty reader thread in the same critical
  section as `vt100::Parser::process(..)` (`pane.rs:249`). Every chunk
  pushed into `output_tx: broadcast::Sender<(u64, Vec<u8>)>` (`pane.rs:103`)
  is already tagged `(seq, bytes)`.
- **`Pane::snapshot_with_seq`** (`pane.rs:392`, delegating to
  `snapshot_at_offset_with_seq`, `pane.rs:396`) — reads `output_seq` under
  the *same* `parser` mutex lock used to build the grid snapshot
  (`pane.rs:435`, comment at `pane.rs:431`), so the returned `(snapshot,
  seq)` pair can never disagree about which bytes are reflected. This
  lock-coupling is the load-bearing invariant a replay buffer must preserve
  when it starts recording chunks — the ring buffer's write path and
  `output_seq`'s increment need to stay atomic with each other the same way
  they're atomic with `parser.process()` today, or a resume response could
  hand back a `(snapshot_seq, replay_from(snapshot_seq+1))` pair that
  doesn't actually reflect the same cut point.
- **`OUTPUT_CHANNEL_CAPACITY = 1024`** (`pane.rs:66`) — a
  `tokio::sync::broadcast` channel. Explicitly documented as sized "to
  absorb a burst... without needing precise tuning," not as a
  reconnect-serving buffer. A brand-new `.subscribe()` (what every `Attach`
  call does today) only receives items sent *after* subscription — there is
  no "subscribe from seq N" primitive on `tokio::sync::broadcast`, which is
  exactly why the requirements doc calls for a *separate*, decoupled replay
  ring rather than just growing this channel's capacity.
- **`forward_step_for_output_result`** (`crates/tymuxd/src/main.rs:325`) —
  the one existing consumer of `output_seq` beyond storage: it drops
  (`ForwardStep::Skip`) any broadcast chunk whose `seq <= snapshot_seq` so
  the priming `CapturePane`-style snapshot and the live stream never
  double-render the same bytes (ADR-003 Amendment / Task 1.3.1b). This is
  the closest existing analogue to "resume from seq N" logic — a resume
  handler needs the *inverse* filter (serve everything `> last_seen_seq` from
  the replay buffer, then splice into the live broadcast stream), and should
  reuse this function's tested pattern rather than inventing a new one.
- **`output_gap` (`AttachEvent.payload`, proto `tymux.proto:289`, bare
  `bool`)** — fires when a consumer's `broadcast::Receiver` returns
  `RecvError::Lagged(n)` (`main.rs:341`). It is detectable-but-not-resumable
  today: a client learns *that* it missed frames, never *which* ones or how
  to get them back, and it's a single flag per stream — there is no
  separate identity distinguishing which of several concurrently attached
  clients lagged.
- **`disconnect_tracker: Arc<Mutex<HashMap<Uuid, Instant>>>`**
  (`main.rs:38-47`) — the existing grace-period-adjacent mechanism, but
  narrower than what this feature needs: it's a detection window for
  flagging a *possible disconnect-survival regression* (pane exiting
  suspiciously soon after its last `Attach` stream ended), not a resumable
  session registry. Its own doc comment admits the relevant limitation
  directly (`main.rs:43-46`): it's **keyed only by `pane_id`**, so "a pane
  with multiple concurrently attached clients can produce a false positive
  if one client detaches right before the pane legitimately exits while
  another client is still watching." This is a preview of the exact
  multi-client-identity problem the resume-token design has to solve
  correctly (see §3) — the existing code took the "accepted simplification"
  path because it only needed a warning signal, not correctness; a resume
  cursor cannot take the same shortcut.
- **`AttachedGaugeGuard`** (referenced `main.rs:52-56`, guard type used at
  `main.rs:679`) — increments/decrements a per-daemon (not per-pane)
  `attached_sessions_gauge` on stream open/close. Confirms attach-stream
  lifecycle already has a guard/RAII pattern the grace-period timer could
  follow, but today's guard fires immediately on stream end, with no delay
  — the grace period this feature adds is a genuinely new deferred-cleanup
  mechanism, not an extension of an existing one.
- **`ExitStatus` / ADR-001 precedent** (`proto/tymux/v1/tymux.proto:277-289`,
  documented at
  `project_plans/stapler-squad-integration/decisions/ADR-001-exit-status-message-shape.md`) —
  the concrete precedent for wrapping a previously-bare scalar (`bool
  has_exited` → `ExitStatus` message) in the *same* `oneof` field number, to
  add optional-presence semantics. Directly reusable playbook for wrapping
  `AttachEvent.output` (bare `bytes`) into a message carrying `seq` alongside
  the bytes, per the requirements doc's own Rabbit Holes note.
- Proto today (`proto/tymux/v1/tymux.proto:256-291`): `AttachRequest`'s
  `oneof` has exactly `pane_id | input | resize` — no resume-token variant
  yet. `AttachEvent`'s `oneof` has `output (bytes) | snapshot | exited
  (ExitStatus) | output_gap (bool)` — no `seq` on `output`, no distinct
  "gap exceeded, fall back to CapturePane" signal (only the coarser
  `output_gap` bool).
- `clients/go` and `clients/ts` (both under `clients/`) have generated
  stubs (`clients/go/gen/tymux`, `clients/ts/gen/tymux`) but no
  reconnect/resume logic of their own in this repo — the real
  `ReconnectLoop` implementation lives in the separate `stapler-squad` repo
  (`session/tmux/stream.go`, per requirements.md), out of this repo's grep
  reach. Nothing in `clients/go`/`clients/ts` currently sends anything
  beyond `pane_id` as the first `AttachRequest`.

## 2. Industry precedent

Building on prior art this project's own `stapler-squad-integration`
research phase already gathered (et/mosh/zellij — see
`project_plans/stapler-squad-integration/research/features.md:146-181`,
which validated tymux's byte-replay design against Eternal Terminal and
flagged mosh's state-sync as a non-transferable alternative). New precedent
gathered for the resume-*token* and per-subscriber-*cursor* shape
specifically:

- **Discord Gateway `Resume` (opcode 6).** A WebSocket protocol whose
  resume contract is structurally the closest single analogue to what this
  feature adds: a client caches `session_id` + the last-seen `seq`, and on
  reconnect sends both back; the server either replays everything after
  that `seq` or responds `Invalid Session` (their equivalent of "gap
  exceeded, start over") if the token/seq/session combination is no longer
  valid — [Discord Gateway docs](https://docs.discord.com/developers/events/gateway),
  [GatewayResumeData shape](https://discord-api-types.dev/api/discord-api-types-v10/interface/GatewayResumeData).
  Two design choices map directly onto this project's open questions:
  - The token is **compound** (`session_id` + `seq`), not seq-alone — this
    is the answer to this project's "self-contained vs. server-tracked
    identity" rabbit hole in practice: Discord's tokens *are*
    self-contained (no separate server-side session table the client must
    have registered against ahead of time beyond the session_id minted at
    connect time), but binding seq to a session_id prevents the
    cross-pane-id replay-confusion edge case (§3) for free, at the cost of
    one extra field.
  - Discord explicitly keeps a *disconnected* session resumable for "a few
    minutes" after a raw TCP close (distinct from an explicit invalidation)
    — the direct precedent for this project's "configurable grace period"
    requirement, and evidence that a short bounded window (not "forever,"
    not "zero") is the industry-standard shape for this exact problem.
- **NATS JetStream consumers.** The requirements doc's own framing (bounded
  retention + per-subscriber cursor) matches JetStream's consumer model
  almost exactly: "a consumer is a server-side, stateful view of a
  stream... with its own cursor," and the durable cursor "advances only
  over acknowledged messages" — [NATS JetStream consumer docs](https://docs.nats.io/nats-concepts/jetstream/consumers).
  Two transferable pieces:
  - JetStream separates **stream retention** (how long messages live at
    all — this project's replay-ring bound) from **consumer position**
    (each subscriber's independent cursor into that stream) as genuinely
    separate concepts with separate config. This is the strongest argument
    for *not* reusing `output_gap`'s single-shared-flag model
    (requirements.md Scope) — the replay ring is one shared, bounded
    resource; the cursor is N independent, per-attach-stream values reading
    from it, exactly like JetStream's stream-vs-consumer split.
  - **The eviction-race edge case has direct precedent and a named failure
    mode**: Synadia (NATS's maintainer) documents exactly the "consumer
    asks to resume from a sequence the stream has already trimmed" case as
    "Delivered Below Stream First Sequence" —
    [NATS Consumer Delivered Below Stream First Sequence: Causes and Fixes](https://www.synadia.com/insights/checks/nats-consumer-delivered-below-stream-first-sequence).
    Their fix pattern (detect `requested_seq < stream_first_seq`, and treat
    it as a well-defined "start from what's actually available" condition
    rather than an error the caller can't recover from) is the direct
    template for this project's own "gap exceeded, fall back to
    `CapturePane`" signal — the fallback shouldn't be an ad hoc special
    case, it should be the documented behavior for exactly this
    below-retention-floor condition.
- **gRPC/HTTP2 keepalive semantics (tonic specifically).** Confirmed via
  [grpc.io Keepalive guide](https://grpc.io/docs/guides/keepalive/) and
  [hyperium/tonic#258 — Support GRPC Keepalive without calls](https://github.com/hyperium/tonic/issues/258):
  HTTP/2 PING-based keepalive is a **transport-level** liveness check — on
  timeout, the transport is simply closed; there is no separate
  application-visible "the peer went quiet" event distinct from the stream
  ending the normal way (`RecvError::Closed`-equivalent on the server side).
  This directly answers one of the requirements doc's open questions ("does
  tonic's keepalive alone cover the grace-period signal, or is an
  app-level ping needed"): **tonic's built-in keepalive can detect *that* a
  connection died faster than TCP's own multi-minute default timeout, but
  it collapses into the exact same "stream ended" signal the daemon already
  gets on any disconnect** — it does not, by itself, distinguish "client
  slow to send a heartbeat but stream technically still open" from any
  other kind of drop, and it does nothing to help the *server* proactively
  detect a half-open connection where the client stopped reading but the
  transport hasn't yet noticed. An application-level ping/pong message on
  the `Attach` stream itself is very likely still needed for the
  grace-period logic specifically, with tonic's transport keepalive as a
  complementary, faster-than-TCP backstop — not a replacement.
- **Kafka consumer-offset model (background, not deep-dived further since
  NATS JetStream is the closer analogue per the requirements doc's own
  framing).** Same shape at a high level (bounded-retention log + N
  independent per-consumer offsets), reinforcing rather than adding new
  design constraints beyond what JetStream's docs already surface above.

## 3. Edge cases the Phase 3 design must explicitly decide

Each framed as a concrete scenario with what the current baseline (§1) does
or doesn't handle, so Phase 3 has a checklist rather than a vague
"multi-client" worry:

1. **Two clients attached to the same pane; one reconnects with a resume
   token, the other stays live throughout.** Today's `output_gap` model
   already gets this right *by accident*: each `Attach` call gets its own
   `broadcast::Receiver` (`pane.subscribe()`, `main.rs:655`), so lag/gap
   state is already per-stream, not shared across attachments to the same
   pane. The resume design must preserve this: the reconnect cursor must be
   scoped to *one subscriber's* resume request, reading from the *shared*
   replay ring without consuming or advancing anything the live client's
   own receiver depends on. Concretely: the replay ring must support
   multiple independent readers at different offsets simultaneously (a
   `Vec<(seq, bytes)>`-style append-only buffer with readers tracking their
   own index works; anything that mutates on read, e.g. draining, does
   not). The existing `disconnect_tracker`'s admitted false-positive gap
   (§1, `main.rs:43-46`, pane_id-keyed only) is the cautionary example of
   getting this wrong — the resume cursor must NOT repeat that shortcut,
   since here it's a correctness bug (wrong replay), not just a spurious
   warning log.
2. **Resume token references a seq that was never valid** — either
   malformed/adversarial input (a seq number higher than the pane has ever
   produced, or garbage bytes in the token field) or a token generated for
   a *different* `pane_id` (a resume token that's structurally well-formed
   but semantically means something else). The Discord-style compound-token
   answer (§2: bind seq to a session/pane identity, not a bare integer)
   closes the cross-pane-id case by construction — if the token embeds
   `pane_id` (or an opaque identity that's checked against the pane being
   attached to), a token from pane A presented against pane B is simply
   invalid, same code path as "gap exceeded." A seq higher than
   `output_seq`'s current value (impossible for a real client, trivial for
   an adversarial/buggy one) needs its own explicit check — trusting the
   ring buffer's own bounds check to "handle" that gracefully rather than
   e.g. panicking on out-of-range indexing. Given the security posture is
   "internal/no-auth" (requirements.md Non-functional Requirements), this
   isn't an auth concern, but it is a **robustness** one: a malformed token
   should degrade to the same `CapturePane`-fallback signal as any other
   out-of-range resume request, not a distinct error path a client has to
   special-case.
3. **Resume request lands exactly as the replay buffer wraps** (race
   between "still in buffer" and "just evicted"). This is the same failure
   shape NATS names "Delivered Below Stream First Sequence" (§2) — the fix
   pattern is to make the check atomic with the read: compute "is
   `requested_seq` still >= the ring's current floor" and "read the
   requested range" under the same lock/mutex acquisition, not as two
   separate steps with a window between them where the floor could advance
   (a live pane still producing output while the resume request is being
   processed makes this a real, not theoretical, race — same class of
   concern the existing `snapshot_with_seq` code comment already documents
   at `pane.rs:431-434` for the unrelated snapshot-vs-seq race, and the same
   fix shape: single lock, not check-then-act).
4. **Reconnect-progress UX — does the resume response need to tell the
   client how much it's about to replay?** Unstated-need analysis for both
   named users:
   - **Human CLI users**: a reconnect that silently replays several seconds
     of missed scrollback (e.g. a build log) with no indication of how much
     is coming can look like a hang, especially compared to today's
     `CapturePane` priming snapshot, which renders instantly regardless of
     how much history it represents. A cheap, low-cost win: the resume
     response's first message could carry a byte-count or chunk-count
     total (or even just "resuming" vs. "gap exceeded" as an explicit
     signal, which the design already needs anyway per Scope) — cheap
     because the replay ring already knows its own bounds when the resume
     request arrives, no extra bookkeeping.
   - **`stapler-squad`'s `ReconnectLoop`**: explicitly out of scope to
     *update* in this project (requirements.md Out of Scope) and, as
     documented, unconditionally does a full `CapturePane` reseed today —
     it will not send a resume token at all until its own follow-up lands,
     so it gets none of this either way in v1. Its unstated need is
     narrower and already covered by the Constraints section: the new
     proto must not require a resume token to be present, so
     `ReconnectLoop`'s current behavior (no token sent → today's full-reseed
     path) keeps working unmodified.
   - **Recommendation for Phase 3**: a total byte/chunk count in the resume
     response is low-cost (data already known) but is UX polish, not a
     correctness requirement — reasonable to treat as in-scope-if-cheap
     rather than a hard requirement, consistent with the "gap exceeded"
     signal being the one piece of resume-response metadata the
     requirements doc already commits to.

## Sources

- [Discord Gateway — Documentation](https://docs.discord.com/developers/events/gateway)
- [GatewayResumeData — discord-api-types](https://discord-api-types.dev/api/discord-api-types-v10/interface/GatewayResumeData)
- [NATS JetStream Consumers](https://docs.nats.io/nats-concepts/jetstream/consumers)
- [NATS Consumer Delivered Below Stream First Sequence: Causes and Fixes — Synadia](https://www.synadia.com/insights/checks/nats-consumer-delivered-below-stream-first-sequence)
- [Reliable Message Delivery in NATS JetStream: Acks, Retries, Dead Letters, and Replay — Synadia](https://www.synadia.com/blog/jetstream-reliable-delivery-dlq-replay)
- [gRPC Keepalive guide](https://grpc.io/docs/guides/keepalive/)
- [hyperium/tonic#258 — Support GRPC Keepalive without calls](https://github.com/hyperium/tonic/issues/258)
- `project_plans/stapler-squad-integration/research/features.md:146-181,307-311` (prior et/mosh/zellij research, reused not repeated)
- `project_plans/stapler-squad-integration/decisions/ADR-001-exit-status-message-shape.md` (bool→message wire-shape precedent)
