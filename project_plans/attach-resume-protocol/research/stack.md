# Research: Stack — attach-resume-protocol

Agent 1 (Stack), SDD Phase 2. Repo root: `/home/tstapler/Programming/tymux`.

## Baseline confirmed in code

- `crates/tymux-core/src/pane.rs:66` — `OUTPUT_CHANNEL_CAPACITY = 1024`, feeding a
  `tokio::sync::broadcast::Sender<(u64, Vec<u8>)>` (`pane.rs:103`, `broadcast::channel(...)`
  at `pane.rs:205`). This is a live pub/sub channel, not a replay log — a new `.subscribe()`
  only sees items sent after it subscribes.
- `output_seq: AtomicU64` (`pane.rs:111`) is bumped in the pty-reader thread in the same
  critical section as `parser.process(..)` (`pane.rs:249`), so it's already monotonic and
  race-free relative to snapshot state. Currently used only for priming-snapshot dedup —
  not on the wire.
- `proto/tymux/v1/tymux.proto:277-291` — `AttachEvent.output` is bare `bytes` (no seq);
  `output_gap` is a bare `bool`, fired on `RecvError::Lagged` (comment at line 59-65
  explicitly documents this is the broadcast channel's own lag signal, not a resume
  mechanism).
- `crates/tymuxd/src/main.rs:947` — `Server::builder().add_service(...).serve_with_shutdown(...)`.
  **No `.http2_keepalive_interval`/`.http2_keepalive_timeout`/`.tcp_keepalive` configured
  anywhere** — grepped `crates/tymuxd/src` and `crates/tymux-cli/src` for `keepalive`,
  `Server::builder`, `Channel::` — zero hits beyond the three bare `Server::builder()` calls.
  Keepalive/heartbeat work here starts from nothing, not a tuning pass.
- Workspace pins (`Cargo.toml:17-19`, confirmed against `Cargo.lock`): `tonic = "0.12"`
  (locked `0.12.3`), `prost = "0.13"` (locked `0.13.5`), `tonic-build = "0.12"`.
- No `ringbuf` or `crossbeam` anywhere in `Cargo.lock` — not a transitive dependency either.
- `proto/buf.yaml`, `buf.gen.go.yaml`, `buf.gen.ts.yaml` drive `clients/go` and `clients/ts`
  codegen from the same `.proto` — any wire change here must be re-buf-generated for both.

## (a) Replay ring buffer — recommendation: `Mutex<VecDeque<(u64, Bytes)>>`, no new crate

The requirement is fundamentally **not** a producer/consumer queue — it's a bounded,
non-destructive, indexed history that multiple independent readers each scan from their
own cursor (`resume_seq`), while the pane keeps writing. That rules out the classic
lock-free ring-buffer crates:

- `ringbuf` (`docs.rs/ringbuf`) is SPSC by design — a `Producer`/`Consumer` split where
  reading *removes* an item. Its SPMC/broadcast variants are documented but exist to give
  each consumer its own *copy* of the stream from "now," not random access into history by
  index — same limitation as `tokio::sync::broadcast` already has, so switching to it
  buys nothing.
- `crossbeam`'s deque is a work-stealing structure (items are stolen/consumed exactly
  once) — same mismatch.

What the use case actually needs — bounded capacity, evict-oldest, multiple readers doing
`Vec`-like `iter().filter(|(seq,_)| *seq > cursor)` scans without mutating shared state — is
exactly `std::collections::VecDeque`'s job: `push_back` + `pop_front` when over capacity,
`O(1)` amortized both ends. The project already leans on `Mutex<T>` for exactly this shape
of shared, occasionally-contended state (`parser: Arc<Mutex<vt100::Parser>>`,
`writer: Mutex<Box<dyn Write + Send>>` in `pane.rs:82-84`) rather than reaching for a
lock-free crate, so `Mutex<VecDeque<(u64, Bytes)>>` (or `Vec<u8>` to match the existing
broadcast tuple type at `pane.rs:103` exactly) is consistent with the codebase's existing
concurrency idiom, not a new pattern. Recommend `bytes::Bytes` over `Vec<u8>` for the stored
payload only if replay buffer entries end up cloned per-subscriber-per-read (cheap refcount
clone vs. deep copy) — `bytes` is already a transitive dependency via `tonic`/`prost`/`hyper`,
so this is a zero-new-direct-dependency choice either way.

Sizing: cap by **byte budget**, not entry count — pty output chunks are `PTY_READ_BUF_SIZE`
(4096 bytes, `pane.rs:60`) at most but can be much smaller (a single keystroke echo), so a
fixed entry-count cap gives a highly variable *time* window depending on workload. A
byte-budget cap (with entry count as a secondary safety ceiling) gives a more predictable
"how long a disconnect can this survive" guarantee, and composes with the existing
`GLOBAL_SCROLLBACK_BUDGET_LINES` per-pane-budget pattern already established for vt100
scrollback (`pane.rs:20-27`) — worth mirroring that same "grant degrades under global
pressure" shape for the new buffer rather than inventing a second budgeting scheme.

Concurrency note: today `output_tx.send(..)` happens in the reader thread outside any lock
held by `Attach` subscribers (`broadcast::Sender::send` doesn't block on slow receivers).
Pushing into a new `Mutex<VecDeque<_>>` on that same hot path adds one more lock
acquisition per pty-read chunk — uncontended in the common case (readers only lock briefly
to scan-and-clone on a resume request, not on every chunk), so this should not become a
bottleneck, but it's a new lock in the pty-reader thread's critical path worth noting for
the plan phase's perf-sensitivity review.

## (b) Resume token / sequence field on `Attach`

`AttachEvent.output` needs to carry `seq` (the existing `output_seq` value, already
computed per chunk at `pane.rs:249`) — proto3 field addition to the `output` case, e.g.
promoting `bytes output = 1` to a submessage `{ uint64 seq = 1; bytes data = 2; }` (a
breaking wire change on that oneof field's type, same category of deliberate breaking
change ADR-001 already made for `ExitStatus`, see comment at `tymux.proto:281`) — or an
additive sibling field if the oneof shape allows keeping `bytes` output as-is (recommend
the submessage swap: it makes the pairing atomic, not a "trust the client tracks seq
externally" convention). `AttachRequest`'s `pane_id` oneof arm (`tymux.proto:257`) is where
`resume_seq`/resume-token would attach, since it's already "first message on the stream."

## (c) Heartbeat/keepalive on the `Attach` stream — what tonic 0.12 already gives you

Verified against `docs.rs/tonic/0.12.3` (`transport::Server` and `transport::Endpoint`):

- **Server side**: `Server::builder().http2_keepalive_interval(Some(Duration))` — enables
  HTTP/2 PING frames at that interval on every accepted connection.
  `.http2_keepalive_timeout(Duration)` — timeout waiting for a PING ack; **default 20s**
  when interval is enabled. **This is connection-level, not per-stream/per-RPC.** The docs
  describe it as closing "the connection" on timeout — with gRPC/HTTP2 multiplexing, one
  dead-peer detection tears down every stream multiplexed on that TCP connection at once,
  not just one `Attach` call. `.tcp_keepalive(Option<Duration>)` is the OS-level TCP
  keepalive equivalent, same connection-wide scope.
- **Client side** (`Endpoint`, relevant to `tymux-cli` and `stapler-squad`'s `BackendTymux`):
  `.http2_keep_alive_interval(Duration)`, `.keep_alive_timeout(Duration)`, and
  `.keep_alive_while_idle(bool)` — the last one matters specifically for `Attach`: without
  it, hyper's default only pings while a request is in flight, so an idle-but-open
  bidirectional stream (a pane the user hasn't typed in) may not get pinged at all,
  delaying dead-connection detection exactly in the case this project cares about. None of
  these three are set anywhere in `tymux-cli` today (confirmed by the same grep as above —
  zero `keepalive`/`Channel::` hits).
- **What this means for the design**: HTTP/2-level keepalive is the right mechanism for
  "is the transport still alive" and needs zero proto changes — just builder calls, both
  server (`tymuxd/src/main.rs:947` and the other two `Server::builder()` sites at lines
  1030 and 1658) and client. But because it's connection-scoped, it can't answer
  "attach-stream N specifically hasn't heard from the server in Xs" if a client ever
  multiplexes multiple `Attach` calls over one channel (unclear from this research pass
  whether `tymux-cli`/`BackendTymux` do — worth confirming in the plan phase). If per-stream
  granularity or an application-visible heartbeat event (e.g. to drive the "configurable
  grace period before disconnect-triggered cleanup" requirement, which needs the *server*
  to independently detect a stream having gone quiet) is needed, that requires an
  **application-level heartbeat**: a periodic empty/ping `AttachEvent` oneof variant sent
  from the server side on a `tokio::time::interval`, alongside (not instead of) HTTP/2
  keepalive — HTTP/2 keepalive for cheap transport-level dead-peer detection, app-level
  heartbeat for the grace-period timer needing an explicit "still alive" signal decoupled
  from output traffic (a pane with no output for an hour shouldn't look dead).

## Current (2026) tonic/prost versions and what's changed since 0.12

- **Locked today**: tonic `0.12.3`, prost `0.13.5` (both from `Cargo.lock`, workspace pins
  `tonic = "0.12"` / `prost = "0.13"` in `Cargo.toml:18-19`).
- **Latest as of this research (Aug 2026)**: tonic `0.14.6` (prost `0.14.4` is the matching
  prost line; tonic 0.13.x and 0.14.x are *not* prost-version-interchangeable — confirmed
  via `tokio-rs/prost` issue #1264, "generated code is incompatible with tonic 0.13").
- **Notable changes since 0.12 that matter for this work**:
  - **tonic 0.13**: removed `tonic::async_trait` re-export — call sites using
    `use tonic::async_trait` must switch to the `async-trait` crate directly. Cosmetic for
    this feature, not blocking.
  - **tonic 0.14**: split prost codegen out of `tonic-build` into a separate
    `tonic-prost`/`tonic-prost-build` crate pair — a real migration, not a drop-in bump
    (build.rs / `tonic-build` invocation changes). Also: edition 2024 + MSRV 1.88, and
    (0.14.5) added a max-connections server setting.
  - No changes found in the streaming API surface itself (`Streaming<T>`, bidi-stream
    setup) between 0.12 and 0.14 that affect this feature's shape — the `.keepalive_*`
    builder methods researched above are present and behave the same across 0.12–0.14.
- **Recommendation**: stay on the pinned `tonic 0.12` / `prost 0.13` for this feature.
  Nothing in 0.13/0.14 unlocks capability this work needs (keepalive is already there in
  0.12), and the 0.14 `tonic-prost-build` split is real migration churn unrelated to
  resume/heartbeat. Revisit the upgrade as its own separate piece of work, not bundled into
  this feature.

## Dependency additions required: none

Every piece — bounded ring buffer, seq field, heartbeat — is buildable on
`std::collections::VecDeque` + `std::sync::Mutex` (or `tokio::sync::Mutex` if the lock is
ever held across an `.await`, which the current pattern in `pane.rs` avoids) +
`tokio::time::interval` + existing `tonic`/`prost`. No new crate needed in
`workspace.dependencies`.
