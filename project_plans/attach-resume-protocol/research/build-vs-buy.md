# Research: Build vs. Buy — attach-resume-protocol

**Date**: 2026-08-24
**Agent**: 6 (Build vs. Buy)

## Scope

Four components the feature needs: (1) per-pane replay ring buffer, (2) any
SaaS/managed-service angle, (3) hand-rolled vs. battle-tested logic for
eviction/seq-gap detection specifically, (4) fork/adapt from prior-art
projects already researched in `project_plans/roadmap/README.md`. Plus a
specific call on tonic keepalive vs. an app-level heartbeat.

Baseline facts confirmed by reading code, not assumed:
- [`crates/tymux-core/src/pane.rs:103`](../../../crates/tymux-core/src/pane.rs#L103)
  — `output_tx: broadcast::Sender<(u64, Vec<u8>)>`, a `tokio::sync::broadcast`
  channel, capacity 1024 (`pane.rs:66`), tagged `(seq, bytes)`.
- [`pane.rs:111`](../../../crates/tymux-core/src/pane.rs#L111) — `output_seq:
  AtomicU64`, monotonic, bumped in the reader thread's critical section.
- `Cargo.toml` workspace deps: `tokio` (full), `tonic` 0.12, `prost` 0.13,
  `portable-pty`, `vt100`, `uuid`, `anyhow`, `futures`, `tokio-stream`,
  `crossterm`, `tracing`, `serde`, `dirs`, `toml`, `libc`. No `ringbuf`,
  `crossbeam`, or similar concurrency crate present anywhere in the tree
  (`Cargo.lock` has no `ringbuf` entry; `grep -rn "ringbuf\|VecDeque"
  crates/` finds nothing).
- The codebase already has a directly analogous "bounded, per-pane,
  in-memory resource" precedent: `GLOBAL_SCROLLBACK_BUDGET_LINES` /
  `allocate_scrollback_budget()` (`pane.rs:27-46`) is a **hand-rolled**
  global budget divider, not a crate — same shape of problem (bound memory
  across N concurrent panes) solved with a plain counter + `saturating_sub`,
  no dependency.

## 1. Existing OSS library for the replay ring buffer

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| `ringbuf` crate (lock-free SPSC/SPMC) | Mature (millions of downloads), no_std-friendly, real lock-free perf for hot audio/IO paths | Solves a different problem: fixed-capacity byte/sample ring with no per-item sequence numbers or "read from arbitrary seq N" API — resume needs random-access-by-seq into a bounded log, not FIFO push/pop. SPSC/SPMC shape doesn't fit "N independent reader cursors with per-cursor position," which is the actual requirement (per-subscriber reconnect cursor, requirements.md Scope). Would need substantial wrapping to add seq-indexed lookup, at which point the crate's lock-free value is mostly discarded. | **Not recommended** |
| A message-log crate (e.g. embedded WAL/log crates like `commitlog`, `bus`) | Some (`bus`) do multi-reader broadcast with per-reader cursors | Nearly all are either disk-backed (wrong for an explicitly in-memory, non-persistent buffer — Scope: "Out of Scope: Persisting the replay buffer to disk") or pull in async-runtime-agnostic abstractions this project doesn't need since it's already committed to Tokio. `bus` itself is effectively unmaintained (last meaningful release years old) and its bounded-broadcast model is what `tokio::sync::broadcast` already gives you today. | **Not recommended** |
| Hand-rolled `Mutex<VecDeque<(u64, Vec<u8>)>>` with a capacity cap, one per pane | Matches the codebase's own established pattern for this exact class of problem (`GLOBAL_SCROLLBACK_BUDGET_LINES`, `pane.rs:27-46`); trivially testable (push, evict-oldest-when-over-cap, binary/linear search by seq); no new dependency, no new license to track, no unfamiliar API surface for future maintainers; total data structure is O(50) lines of code doing one job (bounded FIFO of tagged chunks + seq lookup) | Concurrent-safety and eviction-vs-read races are on you (see §3) | **Recommended** |

Assessment: this is not "genuinely complex enough to justify a dependency."
A bounded ring of `(seq, Vec<u8>)` behind a `Mutex`, evicting from the front
when a byte-size or count cap is hit and doing a linear or binary search by
seq for a resume read, is a small, well-understood data structure — the kind
already precedented in this exact codebase (`allocate_scrollback_budget`).
Reaching for a crate here trades a ~50-line, fully-owned, test-covered
structure for a dependency whose actual API doesn't match the requirement
(seq-indexed multi-reader replay) anyway. `VecDeque` is already in `std`; no
new `Cargo.toml` entry needed at all.

## 2. SaaS / managed service

Not applicable, and not close. This buffer lives inside `tymuxd`, a
single-process daemon holding the pty and the live `vt100::Parser` state
in-process (`pane.rs:84`); a resume replay has to read bytes that were
produced by, and must stay causally ordered with, that same in-process
`output_seq` counter and broadcast channel. There is no hosted service that
could sit between the pane's pty reader thread and a reconnecting gRPC
client without reintroducing exactly the network hop this feature exists to
tolerate the *absence* of. (For comparison, a managed message queue like SQS
or a hosted Redis Streams instance could in principle serve a seq-indexed
replay log — but doing so would add a network dependency to a codepath whose
entire job is surviving network flakiness, and would violate the explicit
in-memory/no-persistence, loopback-only, single-daemon scope in
requirements.md's Non-functional Requirements and Scope sections.) **Not
recommended**, noted for completeness per the assignment.

## 3. LLM-generated / hand-rolled vs. battle-tested library — eviction and seq-gap logic specifically

Two sub-pieces, assessed separately since they carry different risk:

**Eviction logic** (bounded push, drop-oldest when over capacity): this is
the same shape as `allocate_scrollback_budget`'s already-shipped, tested
pattern. A `VecDeque::push_back` + `while over_cap { pop_front() }` loop is
few enough lines that a table-driven unit test (push N items past capacity,
assert the oldest are gone and the seq range is contiguous) gives high
confidence cheaply. **Buy nothing here — build it, test it.**

**Seq-comparison / gap-detection logic** (does the requested resume seq fall
within retained range, or must the daemon signal "gap exceeded, fall back to
`CapturePane`"): also a small, pure function once seq is a monotonic
`u64` — `resume_seq < oldest_retained_seq` → gap-exceeded;
`resume_seq >= oldest_retained_seq && resume_seq <= newest_seq` → serve from
`resume_seq + 1`. Same "few lines, easy to get right with a unit test"
category as eviction. The wire-level version of this exact check already
exists and is tested: `forward_step_for_output_result` in
`crates/tymuxd/src/main.rs` already does seq comparison (`seq <=
snapshot_seq` dedup) as part of Epic 1.3 — this project extends an existing,
working pattern rather than inventing a novel one.

**Concurrent-safety subtlety** (multiple readers, one writer, eviction
racing with a read) is the one place genuine risk lives, and it's worth
being precise about *why* it's still a build, not a buy, call:

- The writer side (pty reader thread pushing new chunks and evicting old
  ones) and reader side (a resume request reading a seq range) both go
  through the *same* `Mutex` that already guards `output_tx`'s send — no new
  lock-ordering hazard is introduced if the replay buffer's push happens in
  the same critical section as the existing `output_tx.send(..)` call
  (`pane.rs`'s reader-thread loop). A `std::sync::Mutex<VecDeque<..>>`
  guarantees a reader never observes a half-evicted state or a torn read,
  by construction — this is exactly what a `Mutex` is for, and reaching for
  a lock-free crate to avoid a single short critical section around a
  `Vec`/`VecDeque` operation (push + bounded pop, both O(1) amortized) is
  solving a performance problem that doesn't exist here: pty output chunks
  arrive at most a few thousand times/sec per pane, nowhere near a
  contention regime that needs lock-free structures.
- The actual multi-reader complexity — N independent subscriber cursors,
  each potentially resuming from a different seq — is already the
  `tokio::sync::broadcast` channel's job for *live* delivery (it already
  hands each `Receiver` its own cursor internally) and only needs to be
  reimplemented for the *replay* path, which is a read-only, single-lock,
  point-in-time slice-out-a-range operation, not a coordination problem
  between readers.

Net: this pushes toward "small, well-understood, worth a test" rather than
"concurrent-safety subtlety big enough to need a dependency." The risk of a
hand-rolled implementation here is materially lower than the risk of
adapting a crate whose lock-free guarantees are calibrated for a different
(higher-contention, SPSC/MPMC-without-seq-lookup) problem and whose fit to
"seq-indexed range read + capacity eviction" would itself need to be
verified by hand anyway. **Build, with tests that exercise concurrent
push-during-read directly (a loom-style or thread::spawn stress test), not
buy.**

## 4. Fork or adapt from prior art (mosh / Eternal Terminal / zellij)

Checked `project_plans/roadmap/README.md`'s "Next" section (this project's
own stated origin) and its Sources list for actual reusable code, not just
protocol description:

- **mosh State Synchronization Protocol** — cited via the [USENIX ATC '12
  paper](https://www.usenix.org/conference/atc12/technical-sessions/presentation/winstein).
  This is a protocol/algorithm description (diff-based state sync between
  numbered instances of client/server terminal state), and mosh's actual
  implementation is C++ (`mosh/src/`), built around its own custom UDP
  transport and Intermediate Representation diffing — not a Rust crate, and
  not a data structure that maps onto "replay a byte-range ring buffer over
  an existing gRPC/HTTP2 stream." tymux's design (byte-chunk replay over
  seq numbers) is already closer to ET's simpler model than to mosh's
  diff-based state sync, so mosh contributes *conceptual* precedent only
  (roadmap README's own framing: "falls back... for 'too far behind, resync
  to current state'" — i.e., the fallback-to-snapshot idea, already covered
  by this project's `CapturePane` fallback design, not the ring buffer
  itself).
- **Eternal Terminal's `BackedReader`/`BackedWriter`** — cited via
  [eternalterminal.dev/howitworks](https://eternalterminal.dev/howitworks/).
  ET is open source (C++, `MoserWare/EternalTerminal` on GitHub) and its
  `BackedReader`/`BackedWriter` design is conceptually the closest match to
  this project's exact requirement — byte-sequence-numbered send/receive
  buffers that survive a reconnect. But it's C++, not Rust, tightly coupled
  to ET's own custom transport/framing (not gRPC/tonic), and the pattern
  itself (a numbered buffer with retransmit-from-seq-N) is simple enough
  that description-level understanding is sufficient to reimplement
  natively — there's no packaged, extractable Rust module to import, and a
  literal port of C++ buffer-management code across such different
  transport layers would cost more than a native implementation guided by
  the same design idea.
- **zellij's `Active → ActiveDetached → Killed` state machine** — cited via
  [Session Resurrection docs](https://zellij.dev/documentation/session-resurrection.html).
  zellij *is* Rust and open source, and its session-state-machine idea is
  useful precedent for this project's grace-period design (an explicit
  intermediate "orphaned but resumable" state, rather than ad hoc), but
  zellij's actual resurrection mechanism works by serializing/respawning
  layout+pane metadata on disk (closer to tymux's own separate Tier 0/1
  persistence work than to this feature's in-memory byte-replay problem).
  Not a byte-ring-buffer implementation to adapt; the reusable part is the
  state-machine *shape*, already noted as design inspiration in
  requirements.md itself, not code.

**Conclusion**: none of the three has actual open-source code close enough
to adapt for the replay buffer or seq-gap logic specifically — the prior
research in `project_plans/roadmap/README.md` and
`project_plans/stapler-squad-integration/research/features.md` is
protocol/design-level, which is exactly the right altitude for informing
*this* project's design (and already has: the fallback-to-snapshot idea, the
per-subscriber-cursor requirement, and the grace-period state machine all
trace to this prior art per requirements.md's own citations). There is no
fork-and-adapt path here; the value of this prior art was already extracted
during requirements-gathering, not left for this phase to find. **Not
recommended as a code source; already correctly used as design reference.**

## gRPC/tonic keepalive: buy or build?

**Buy the transport-level signal, build the pane-level grace-period
decision on top of it — this is not an either/or.**

- Tonic exposes `.keepalive_interval()` / `.keepalive_timeout()` on both
  `Server` and `Channel` builders (confirmed against tonic 0.12's public
  API, which this workspace already pins in `Cargo.toml`). These configure
  HTTP/2 PING frames at the transport layer: if a peer doesn't ACK a PING
  within the timeout, the HTTP/2 connection itself is torn down. This is
  real, already-a-dependency, zero-new-code value for the base case of
  "detect a dead TCP connection faster than TCP's own (often very long or
  absent-behind-NAT) failure detection" — exactly the problem
  requirements.md's Rabbit Holes section flags as needing to be checked
  before assuming it's sufficient.
- It is **not sufficient by itself** for the actual requirement, and the
  requirements doc's own Rabbit Holes section already anticipates why:
  tonic's keepalive signal fires at the **connection** level, surfaced to
  application code only as the stream/RPC erroring out (a `Status`/stream
  termination), not as a graduated "client hasn't heartbeated in N seconds,
  but keep the pane alive for a separately-configurable grace period" hook.
  The moment tonic's keepalive fires, the natural default behavior is
  "stream is gone" — there's no built-in intermediate state. This project's
  actual requirement (a *pane* stays alive and resumable for a configurable
  window *after* the stream is detected as gone, per requirements.md's
  Success Metrics: "a dropped-but-not-detached Attach stream is not torn
  down immediately") is application-level policy that has to live in
  `tymuxd`'s pane/session lifecycle code regardless of what signals the
  transport layer, because it's a decision about *pane* lifetime, not
  *connection* lifetime — tonic has no concept of a pane.
- Recommended shape: configure tonic's keepalive for connection-level dead-
  peer detection (buy — it's already a dependency, just needs the two
  builder calls configured with sane defaults), and layer a small
  application-level timer/grace-period on top in the daemon (build — this
  is inherently pane-lifecycle policy, not something a transport keepalive
  setting could express). This mirrors the "not either/or" framing zellij's
  state machine already suggests: transport keepalive answers "is the
  connection dead," the grace-period state machine answers "given that,
  what should happen to the pane" — two different questions, one bought,
  one built.

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| Tonic's `.keepalive_interval()`/`.keepalive_timeout()` alone | Zero new code, already a dependency, real connection-dead detection | No pane-level grace-period hook; conflates "connection dead" with "pane should be cleaned up now" — doesn't meet the explicit grace-period requirement | **Viable, but insufficient alone** |
| App-level heartbeat message on `Attach` alone (ignore tonic keepalive) | Full control over grace-period semantics | Reinvents dead-connection detection tonic already gives for free; slower to detect a genuinely dead TCP connection than HTTP/2 PING frames | **Not recommended alone** |
| Both: tonic keepalive for transport-level dead-peer detection + a small daemon-side grace-period timer keyed off stream termination/heartbeat gaps | Uses the free part of the dependency for what it's actually good at; keeps the genuinely new logic (grace period, pane-level state) small and testable in isolation | Two mechanisms to reason about (documented, not a real cost given they answer different questions) | **Recommended** |

## Summary Table

| Component | Verdict | Why |
|---|---|---|
| Replay ring buffer | **Build** (`Mutex<VecDeque<(u64, Vec<u8>)>>`, `std` only) | Small, well-understood structure; no crate matches the seq-indexed multi-reader-cursor shape without heavy adaptation; codebase already has a precedent for hand-rolled bounded per-pane memory (`GLOBAL_SCROLLBACK_BUDGET_LINES`) |
| SaaS/managed service | **Not applicable** | In-process daemon feature; a hosted service would reintroduce the network dependency this feature exists to tolerate the absence of |
| Eviction + seq-gap logic | **Build, with tests** | Few-line pure logic, same pattern as an already-shipped, tested codepath (`forward_step_for_output_result`'s seq dedup); concurrent-safety risk is fully addressed by the existing single-`Mutex` critical section, not a reason to reach for a lock-free crate |
| Fork/adapt mosh/ET/zellij | **Not recommended as code source** | All three are protocol/design-level prior art, already correctly consumed at requirements time (fallback-to-snapshot, per-subscriber cursor, grace-period state machine); none ships Rust code that fits the transport (gRPC/tonic) this project is built on |
| gRPC keepalive | **Buy transport-level detection (tonic config) + build the pane-level grace-period policy on top** | Tonic keepalive answers "is the connection dead"; grace period answers "what happens to the pane" — different questions, no single mechanism answers both |

## Sources

Internal:
- [`crates/tymux-core/src/pane.rs`](../../../crates/tymux-core/src/pane.rs)
  — `output_tx`/`output_seq` (L103, L111), `OUTPUT_CHANNEL_CAPACITY` (L66),
  `GLOBAL_SCROLLBACK_BUDGET_LINES`/`allocate_scrollback_budget` (L27-46)
- `Cargo.toml` workspace.dependencies (repo root) — confirmed dependency set
- `crates/tymuxd/src/main.rs` — `forward_step_for_output_result`'s existing
  seq-dedup pattern (cited in requirements.md's Baseline)
- [`project_plans/attach-resume-protocol/requirements.md`](../requirements.md)
  — full requirements context for this project
- [`project_plans/roadmap/README.md`](../../roadmap/README.md) — mosh/ET/
  zellij citations and their External Sources list

External:
- `ringbuf` crate — [crates.io/crates/ringbuf](https://crates.io/crates/ringbuf)
- mosh State Synchronization Protocol — [USENIX ATC '12 paper](https://www.usenix.org/conference/atc12/technical-sessions/presentation/winstein)
- Eternal Terminal — [eternalterminal.dev/howitworks](https://eternalterminal.dev/howitworks/),
  source at [github.com/MoserWare/EternalTerminal](https://github.com/MoserWare/EternalTerminal)
- zellij session resurrection — [zellij.dev/documentation/session-resurrection.html](https://zellij.dev/documentation/session-resurrection.html)
- tonic keepalive API — `tonic::transport::Server::keepalive_interval`/
  `keepalive_timeout` and `Channel::keepalive_interval`/`keepalive_timeout`,
  tonic 0.12 (pinned in this workspace's `Cargo.toml`)
