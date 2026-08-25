# Requirements: attach-resume-protocol

**Date**: 2026-08-24
**Type**: feature addition (existing project)
**Complexity**: 3 — system design

## Problem Statement
`tymux`'s `Attach` bidirectional stream has no resume mechanism. Today,
every reattach — whether a deliberate detach/reconnect or an abrupt
network drop — starts from scratch: a fresh `Attach` call gets a priming
`CapturePane`-style snapshot of the *current* screen (Epic 1.3), and any
output that happened while the client was disconnected is gone unless it
happened to still be sitting in the live broadcast channel's small
capacity-1024 ring when the new subscription opens. There's no way for a
client to say "I was at seq N, give me what I missed," and no dedicated
buffer sized for a real reconnect window — only the live channel's
lag-detection buffer, which exists to catch a *slow* consumer, not to
serve a *reattaching* one. This is the generalized form of the bug fixed
in `stapler-squad-integration` Epic 1.1 (abrupt disconnect killing the
pane): that fix keeps the pane alive across a drop, but nothing yet lets
a reconnecting client resume the *stream* — every consumer of `Attach`
(stapler-squad's `BackendTymux`, the CLI, any future gRPC client) pays a
full-resync cost on every reconnect, however brief the drop.

## Baseline
What exists today, confirmed by reading the code (not assumed):
- `Pane` already maintains a monotonic per-pane `output_seq: AtomicU64`
  (`crates/tymux-core/src/pane.rs:111`), and every chunk pushed into the
  output broadcast channel is already tagged `(seq, bytes)`
  (`pane.rs:255`). This is currently used for exactly one thing: Epic
  1.3's priming-snapshot dedup (`forward_step_for_output_result` in
  `crates/tymuxd/src/main.rs` drops any output chunk whose `seq` is
  `<= snapshot_seq` so it isn't double-rendered after the initial
  snapshot). The seq value is **not exposed on the wire** — `AttachEvent`'s
  `output` field is a bare `bytes`, and a reconnecting client has no way
  to say "resume after seq N."
- The output broadcast channel (`OUTPUT_CHANNEL_CAPACITY = 1024`,
  `pane.rs:66`) is a `tokio::sync::broadcast` channel — live pub/sub, not
  a replay log. A brand-new `.subscribe()` call (which is what every
  `Attach` does today, reconnect or not) only receives items sent *after*
  it subscribes; there is no "subscribe starting from seq N" primitive in
  `tokio::sync::broadcast`, so today's channel cannot by itself serve a
  resume request for anything that happened before resubscription,
  regardless of whether it's still physically in the ring.
- `AttachEvent.output_gap` (a bare `bool`) is the one existing signal for
  "you missed something" — it fires when a consumer's `broadcast::Receiver`
  falls behind and gets `RecvError::Lagged(n)` (`main.rs`'s
  `forward_step_for_output_result`). It's a single shared flag per
  stream, detectable but not resumable — a client sees "you missed N
  frames" with no way to recover the missed bytes, only to resync via a
  fresh `CapturePane`.
- `stapler-squad-integration` Epic 2.5 (`ReconnectLoop`, merged in PR #37)
  already builds a client-side reconnect loop on top of exactly this
  gap: on any drop (network blip or `output_gap`), it reopens `Attach`
  and does a **full `CapturePane` reseed** — the same recovery path
  regardless of whether the client was gone for 50ms or 50 seconds. This
  requirements doc's job is to give reconnecting clients a cheaper,
  incremental option for short-to-medium gaps, with full-`CapturePane`
  staying as the correct fallback for anything longer — not to replace
  that fallback.
- Daemon-restart is a separate, already-decided contract (Epic 2.5.3,
  confirmed against `Engine::revive_session`): a pane never survives
  `tymuxd` dying — `ReviveSession` always spawns a fresh process, there is
  no PID persistence or reattachment. **This project does not touch that
  contract.** It's scoped entirely to "the daemon and the pane's process
  are both still alive; the network connection between client and daemon
  dropped or the client process itself restarted."

## Users / Consumers
- stapler-squad's `BackendTymux` / `ReconnectLoop` (`session/tmux/stream.go`)
  — the one real, already-shipped consumer of `Attach`'s reconnect path
  today; adopting the new resume capability there is a deliberate
  follow-up (see Scope), but this project's proto/daemon design must not
  break its current full-`CapturePane`-resync behavior
- `tymux-cli`'s own attach flow (human interactive users) — benefits
  transparently from shorter reconnect gaps once the CLI's own
  detach/reattach path (already local, not over a lossy network in the
  common case) or a flaky remote setup hits a drop
- Future/other gRPC clients (any language, per tymux's stated
  differentiator) — the resume contract needs to be a real, documented
  part of the proto, not an implementation detail only the Rust CLI knows
  to use

## Success Metrics
- A client that reconnects with a resume token for a seq still within
  the daemon's replay window receives exactly the missed output, byte-
  identical, with no duplication and no gap — verified with a test that
  asserts on the reconnecting stream's received bytes matching what a
  client that never disconnected would have seen.
- A client that reconnects past the replay window's retention receives a
  clear "gap exceeded, here's a fresh snapshot" response (not silently
  wrong output, not a hang) — the existing `CapturePane`-fallback
  contract, now reachable via an explicit signal rather than only via
  `output_gap`'s existing lag-detection path.
- A dropped-but-not-detached `Attach` stream is not torn down
  immediately — a configurable grace period exists during which the pane
  keeps running and a reconnect can still resume, distinct from an
  explicit detach.
- Heartbeat/keepalive lets the daemon distinguish "client slow" from
  "client actually gone" within a bounded, configurable window — this is
  what makes the grace period above trigger promptly and correctly
  instead of relying on TCP's own (often very long, or absent behind
  some NATs/proxies) failure detection.
- Reference clients (`clients/ts`, `clients/go`) exercise the new resume
  path with real, working code — not just the Rust CLI — matching this
  project's existing cross-language-client precedent (Epic 7).
- `tymux-cli`, `clients/ts`, and `clients/go` exhibit the *same*
  reconnect behavior — same backoff timing, same retry ceiling, same
  `GapExceeded`-triggers-fallback handling — verified by each
  implementation's tests asserting against the one shared specification
  in the proto doc comments, not three independently-judged behaviors
  that happen to look similar.

## Appetite
Medium (1–2 weeks), upper end
*(The "Fuller" scope was chosen over "Minimal": a dedicated per-pane
replay buffer decoupled from the live broadcast channel's small
lag-detection ring, sized to tolerate a real reconnect window rather than
only sub-second blips, plus reference-client adoption in this repo —
now including `tymux-cli`'s own reconnect loop (added post-research, see
Scope), since a resume protocol nothing can actually invoke isn't
finished. If the replay-buffer design or the client/CLI work meaningfully
overruns this appetite once planning starts, cut scope in this order:
(1) drop `tymux-cli`'s cross-invocation persistence first — ship the
protocol + reference-client test coverage, defer CLI reconnect to a
follow-up; (2) if still overrun, fall back to the Minimal scope from the
original ideation pass — seq-exposure + resume-token + heartbeat/
grace-period only, defer the dedicated replay buffer. Cut scope in this
order rather than let the appetite slip to Large.)*

## Constraints
- Solo/personal-project pace — no external deadline.
- Must not break `stapler-squad`'s already-merged `BackendTymux`/
  `ReconnectLoop` (PR #37) — it's a real, working consumer today and its
  current full-`CapturePane`-resync behavior must keep working unmodified
  against the new proto even if it doesn't yet send a resume token.

## Non-functional Requirements
- **Performance SLO**: not specified numerically; qualitatively, resuming
  via the replay buffer should be meaningfully cheaper than a full
  `CapturePane` round-trip for the common short-gap case (this is the
  entire point of the feature) — a concrete latency comparison belongs in
  the research/validation phase, not fixed here.
- **Scalability**: the new replay buffer is per-pane, in-memory, bounded
  — must not turn into an unbounded-memory-growth risk under many
  concurrent panes (ties to the existing 1,000-session load-test
  precedent from `stapler-squad-integration`).
- **Security classification**: internal — no change to tymux's existing
  loopback-only/no-auth posture; this project is orthogonal to the
  roadmap's separate auth milestone.
- **Data residency**: not applicable.

## Scope
### In Scope
- Expose `output_seq` on the wire (proto change to `AttachEvent`'s output
  path — exact shape, e.g. wrapping `output` in a message vs. a sibling
  field, is a Phase 3 design decision, not fixed here)
- A resume token / last-seen-seq field on `Attach`'s first message
  (alongside the existing `pane_id` oneof variant)
- A dedicated per-pane replay ring buffer, decoupled from the live
  broadcast channel's capacity-1024 lag-detection buffer, sized to
  tolerate a real reconnect window (concrete size/duration is a Phase 3
  decision informed by research into realistic reconnect-gap
  distributions)
- Per-subscriber reconnect cursor — replacing the single shared
  `output_gap` bool's implicit assumption that only one "how far behind"
  state matters, since multiple clients can independently attach/detach/
  reconnect to the same pane
- Heartbeat/keepalive on the `Attach` stream (transport-level tonic
  keepalive vs. an app-level ping message is a Phase 3 decision)
- A configurable grace period before any disconnect-triggered cleanup —
  making "orphaned but still running, resumable" a first-class window,
  not best-effort
- Explicit fallback signal: when a resume request's seq is outside the
  replay buffer's retention, the daemon tells the client to fall back to
  `CapturePane` rather than silently serving a gap or an error the client
  can't act on
- Reference client updates in `clients/ts` and `clients/go` to send a
  resume token and consume the new seq field end-to-end
- **`tymux-cli`'s own reconnect loop** (added 2026-08-24, post-Phase-2
  research): a small client-side reconnect loop that catches an
  unexpected `Attach` stream drop, saves the last-seen seq across
  process invocations (each CLI attach is a fresh process today — no
  cross-invocation state exists yet; precedent for this kind of local
  state is `crates/tymux-core/src/persistence.rs`), and reopens `Attach`
  with a resume token on the next `tymux attach` to the same pane.
  Without this, the protocol work would be usable by `clients/ts`/
  `clients/go` test code but not by any actual interactive human user —
  research (`research/ux.md`) found the CLI has no reconnect loop at all
  today, so a dropped stream just exits the process.
- **One shared reconnect-loop specification, not three independently-
  invented ones** (added 2026-08-24, Tyler's request): `tymux-cli`
  (Rust), `clients/ts`, and `clients/go` all need reconnect logic —
  detect an unexpected drop vs. a deliberate detach, apply backoff,
  reopen `Attach` with the resume token, interpret `GapExceeded` as
  "fall back to `CapturePane`," give up after a bounded number of
  attempts. Writing this three times independently is exactly how
  behavior drifts (different backoff curves, different retry ceilings,
  one client treating `GapExceeded` as an error instead of a fallback
  signal). The policy — not just the wire messages — gets specified once
  as the authoritative contract and every client implements it
  identically. This repo's own existing convention is the natural home
  for that: `proto/tymux/v1/tymux.proto`'s RPC/message doc comments
  already carry dense behavioral-contract prose (see the existing
  `Attach` RPC comment's detach/`output_gap` semantics) — the new
  resume-related messages get the same treatment, covering the backoff
  policy and give-up condition, not just field shapes. Exact backoff
  parameters (initial delay, max delay, jitter, attempt ceiling) are a
  Phase 3 decision (see Rabbit Holes), but the requirement is that
  there's exactly one answer, written down once, that all three clients
  implement — not each author's own judgment call.

### Out of Scope
- Daemon-restart survival / PID persistence / process reattachment —
  explicitly a separate, already-decided contract (Epic 2.5.3); this
  project only concerns a live daemon + live pane with a dropped
  connection
- Updating `stapler-squad`'s own `ReconnectLoop` to actually use the new
  resume token instead of always doing a full `CapturePane` resync — a
  deliberate, separately-scoped follow-up in the `stapler-squad` repo,
  not this one. When that follow-up happens, it should conform to this
  project's shared reconnect-loop specification (above) rather than
  inventing a fourth independent policy — worth a note in the proto
  doc comments pointing future implementers at the spec, but the
  `stapler-squad` code change itself stays out of scope here.
- The roadmap's separate auth milestone (bearer tokens, per-session
  ownership) — orthogonal; not touched here
- Persisting the replay buffer to disk / surviving it across a daemon
  restart — it's an in-memory, live-daemon-only structure, consistent
  with the daemon-restart exclusion above
- Scrollback-content persistence (Tier 1, roadmap) — a different
  buffer serving a different purpose (historical scrollback for
  copy-mode/search), not this feature's live-reconnect replay buffer,
  even though both are "keep more pane output around" in spirit

## Rabbit Holes
- **Exact shared backoff/give-up parameters**: "one shared spec" (Scope)
  resolves *drift*, but someone still has to pick the actual numbers —
  initial delay, max delay, jitter, and how many failed attempts before
  giving up and surfacing an error instead of retrying forever. Get this
  wrong in either direction and it's a real problem: too aggressive
  hammers a daemon that's down for a legitimate reason (e.g. a deliberate
  restart during an upgrade); too conservative makes a human's `tymux
  attach` feel broken after a half-second blip. Phase 3 should pick
  concrete numbers (informed by `stapler-squad`'s existing `ReconnectLoop`
  backoff choice, `session/tmux/stream.go`'s Task 2.5.2a, as a reference
  point for what's already been judged reasonable for this exact class of
  drop) and write them into the proto doc comments as the literal
  numbers every client implements, not just "use reasonable backoff."
- **Proto wire-shape for adding `seq` to `output`**: `AttachEvent.output`
  is currently a bare `bytes` in a `oneof`. Wrapping it in a new message
  (to carry `seq` alongside the bytes) changes the generated type shape
  in every client language even though the field number can stay the
  same — this is a source-breaking change for typed clients (Rust
  `prost`, TS, Go), similar in kind to ADR-001's `ExitStatus` precedent
  (bool → message, same field number, accepted deliberately). Decide
  explicitly in Phase 3 rather than defaulting to "just add a field
  somewhere" — get the shape right once, since three real client
  codebases (this repo's `clients/ts`/`clients/go`, plus stapler-squad's
  own generated Go client) all need to agree on it.
- **Replay-buffer sizing**: "tolerate a real reconnect window" is not yet
  a number. Sizing too small makes the feature rarely trigger (most
  reconnects still fall back to `CapturePane`, and the feature was a
  wasted build); sizing too large risks the per-pane memory-growth
  concern flagged in Non-functional Requirements. Needs research into
  what `stapler-squad`'s actual observed reconnect-gap distribution looks
  like (or a reasonable default informed by mosh/Eternal Terminal's own
  retention choices, per the roadmap's prior research) before Phase 3
  commits to a number.
- **Per-subscriber cursor bookkeeping**: today's `output_gap` is a single
  bool computed per-stream from the broadcast `Receiver`'s own lag state
  — "per subscriber" already, in a sense (each `Attach` call has its own
  `Receiver`). The real new complexity is the *replay* buffer's cursor
  bookkeeping across reconnects of the *same* logical client identity
  (does the daemon even have a concept of "the same client reconnecting"
  versus "a brand new attach"? Today it doesn't — every `Attach` is
  anonymous). Phase 3 needs to decide whether resume tokens are
  self-contained (the token itself encodes what's needed, no server-side
  per-client state) or require the daemon to track reconnecting-client
  identity — the former is simpler and fits tymux's currently-stateless-
  per-attach model better, but needs to be a deliberate choice.
- **Heartbeat mechanism choice**: tonic/HTTP2 has built-in keepalive
  (`.keepalive_interval`/`.keepalive_timeout` on the server/channel
  builders) that might cover part of this "for free" versus needing a new
  application-level ping message on the `Attach` stream itself (needed if
  the grace-period logic wants pane-level, not just transport-level,
  signal). Don't assume one is sufficient without checking what tonic's
  built-in keepalive actually signals to application code on timeout.

## Alternatives Considered
- **Minimal scope** (seq-exposure + resume token only, no dedicated
  replay buffer, relying on the existing capacity-1024 broadcast channel)
  — considered and available as a fallback if the Fuller scope overruns
  appetite (see Appetite section), but not chosen as the primary target:
  it would only help the exact sub-second-blip case already covered by
  the disconnect-survival fix, not the more general "client was gone for
  a few seconds to a minute" case that's the actual point of a resume
  protocol.
- **Persistent (disk-backed) replay buffer**: rejected — conflates this
  feature with Tier 1/scrollback persistence (a different roadmap item
  serving a different purpose); an in-memory, live-daemon-only buffer is
  the right scope, consistent with daemon-restart being explicitly out of
  scope.
- **Server-side per-client session tracking for resume** (vs.
  self-contained resume tokens): not rejected outright, left as a Phase 3
  decision (see Rabbit Holes) — self-contained tokens are the default
  lean given tymux's current stateless-per-attach model, but not locked
  in here.

## Feasibility Risks
- The replay-buffer sizing decision (Rabbit Holes) has no real production
  usage data yet to inform it — tymux has no deployed multi-user traffic
  history; the sizing will necessarily be a reasoned default, not an
  empirically-derived one, until it's actually used.
- Proto shape changes to `AttachEvent`/`AttachRequest` are the same class
  of breaking change ADR-001 already accepted once for `ExitStatus` — low
  risk technically, but real: `clients/go`'s tagged `v0.1.0` module
  (Epic 1.6) means a breaking proto change now requires a coordinated
  version bump on the `stapler-squad` side, not just a same-repo `replace`
  directive update as before that module was tagged.
- Tonic's built-in HTTP/2 keepalive behavior under real network
  conditions (vs. a local dev/CI environment) is unverified — the same
  class of "works in every sandbox, unverified against the real failure
  mode" risk that made the Epic 1.1 disconnect fix need a real-hardware
  runbook. Phase 4 (validate) should flag whether this needs a similar
  real-hardware or real-network verification pass, not just a dev-loopback
  test.

## Observability Requirements
- A counter for resume outcomes, tagged by result (`resumed-from-buffer`
  / `gap-exceeded-fallback` / `no-resume-token-full-attach`) — mirrors
  the hand-rolled-counter convention `stapler-squad`'s
  `tymux_attach_stream_reconnects_total` already established (Epic 2.5.2c),
  kept on the daemon side this time since the daemon is what actually
  knows whether a resume succeeded or fell back.
- `tracing::warn!`-level logging when a resume request's seq falls
  outside the replay buffer's retention (mirrors the existing
  `output_gap` warn-log pattern in `main.rs`), so a real deployment's
  logs show how often clients are hitting the fallback path — the signal
  that would tell a future maintainer the replay buffer needs resizing.
- Grace-period expirations (a resumable-but-abandoned pane that finally
  gets cleaned up) should log at `info` level with the pane id and how
  long it sat orphaned — useful both for debugging and for eventually
  tuning the grace-period duration itself.

## Risk Control
- No feature flag — this is an additive, backward-compatible protocol
  change (old clients that never send a resume token get exactly today's
  behavior: fresh priming snapshot, no resume). Rollback is a standard
  revert-via-PR, consistent with how every other epic in this codebase
  has handled risk so far (see `stapler-squad-integration`'s own Risk
  Control section) — there's no staged-rollout mechanism in this project
  and no reason to invent one for a single-daemon, personal-scale tool.
- The one real compatibility risk (Feasibility Risks: `clients/go`'s
  tagged module version) is handled by treating this as a coordinated,
  versioned proto change from the start — bump `clients/go`'s tag when
  this ships, same process already established for Epic 1.5/1.6's `cwd`
  fields.

## Open Questions
- Exact proto wire-shape for carrying `seq` — **informed, not fully
  pinned**: `research/architecture.md` found no new `ForwardStep`
  variants are needed and the priming-snapshot dedup pattern
  (`seq <= threshold`) transfers directly; the exact `AttachEvent.output`
  shape (sibling field vs. wrapping message) is still a Phase 3 decision.
- Replay-buffer size/retention duration — **approach resolved, number
  still Phase 3**: cap by byte budget (not entry count — pty chunks vary
  1–4096 bytes), capacity-bounded not time-bounded, mirroring the
  existing `GLOBAL_SCROLLBACK_BUDGET_LINES` pattern
  (`crates/tymux-core/src/pane.rs:27-54`). Converged independently across
  `research/stack.md`, `research/architecture.md`, and
  `research/build-vs-buy.md`.
- Resume-token shape — **resolved**: self-contained (no server-side
  per-client tracking), keyed by `pane_id` (already sent in `Attach`'s
  first message) plus `resume_from_seq: u64`. Discord Gateway's
  `(session_id, seq)` `Resume` opcode (`research/features.md`) is the
  cited industry precedent for why a compound key beats a bare seq — it
  closes the "token from a different pane_id" edge case by construction.
- Heartbeat mechanism — **resolved**: needs both layers, not either/or.
  tonic's `.http2_keepalive_interval`/`.http2_keepalive_timeout` is
  connection-level (kills every multiplexed stream on that connection,
  not just one `Attach` call) and isn't currently configured anywhere in
  `tymuxd`/`tymux-cli` (`research/stack.md`); an application-level
  periodic `AttachEvent` ping is still needed to drive the
  pane-lifecycle grace-period policy specifically.
- Grace-period duration (daemon-wide vs. per-session) — **still open**,
  Phase 3. `research/architecture.md` (Q5) found something more
  important first: the grace period's real job is deferring
  `unregister_viewport`/`recompute_window_geometry` (avoiding visible
  window-geometry thrash on a quick reconnect), not gating pane or
  buffer cleanup — pane lifecycle is already disconnect-proof (Epic 1.1)
  and the buffer is capacity- not time-bounded. The config-scope
  question itself is unresolved.
- **New, surfaced by `research/ux.md`, resolved 2026-08-24**: does this
  project give `tymux-cli` a reconnect loop, or leave it exiting on drop
  with only `clients/ts`/`clients/go` exercising resume? **Resolved: yes,
  add the CLI reconnect loop** (see Scope) — without it the protocol has
  no interactive human consumer. Recorded as the first scope cut if
  appetite overruns (see Appetite).
