# Research: Pitfalls — v1-release

**Phase**: 2 (Research) — feeds `docs/plans/v1-release/implementation/plan.md`
**Scope**: What commonly goes wrong building each in-scope v1.0 area, specifically
in Rust with this project's existing architecture (tokio async daemon + PTY
reader threads + gRPC bidi streaming), grounded in the actual code as it
exists today (post is-it-ready fix pass).

---

## 1. Splits

**The existing race this multiplies.** `Pane::resize()` at
`crates/tymux-core/src/pane.rs:155-164` takes two *separate* mutexes
sequentially — `self.master.lock()` (resize the OS pty) then, after that
guard drops, `self.parser.lock()` (resize the vt100 grid):

```rust
pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
    self.master.lock().unwrap().resize(PtySize { rows, cols, .. })?;
    self.parser.lock().unwrap().set_size(rows, cols);
    Ok(())
}
```

This is not atomic. Two concurrent `Resize` calls (today: two attached
clients to the same multi-attach pane; post-splits: a window-relayout that
touches N sibling panes) can interleave between the two locks, so the pty's
real kernel-level geometry and the vt100 parser's idea of geometry can end
up reflecting *different* calls — "last write wins" undersells it; it can
actually be "half of call A, half of call B," a torn write across two
independently-locked pieces of state, not just a stale-value race.

**What splits multiplies this into.** Today a resize race only affects one
pane against itself. With splits, a window's pane tree has interdependent
sizing — sibling panes co-own the terminal's rows/cols, so resizing a
window (SIGWINCH, or a user's `:resize-pane`) means recomputing *N* sibling
panes' sizes together, not calling `Pane::resize()` N independent times.
Two things to design against explicitly:
- **Cross-pane atomicity**: a window resize must be one logical operation
  over the whole subtree, not N independent RPCs/tasks each racing the
  others — otherwise you get visibly mismatched pane borders/dividers where
  siblings don't sum to the window's real size.
  `crates/tymuxd/src/main.rs:229-233` currently forwards each client's
  `Resize` message straight to `pane.resize()` with no window-level
  coordination at all — this path needs a per-window lock (or a single
  actor/task owning layout) before splits land, not per-pane locking.
- **Tree mutation vs. concurrent attach**: splitting/closing a pane while
  another client is attached to a sibling (or to the pane being split)
  needs a defined invariant for what happens to in-flight `Attach` streams
  — `crates/tymuxd/src/main.rs`'s `attach()` resolves a `pane_id` once at
  stream start (`main.rs:174-177`) and holds `Arc<Pane>` for the stream's
  lifetime; if a split reparents/replaces panes in a tree structure, decide
  now whether pane identity (UUID) survives a split/close of a sibling, or
  whether closing one pane can invalidate a *different* live stream's
  assumptions about window geometry.
- **`Engine::pane()`'s flat namespace** (`crates/tymux-core/src/engine.rs:93-100`)
  already treats pane ids as flat across all sessions ("the pane namespace
  is flat across sessions since each session has exactly one" — this
  comment becomes false the moment split support lands and needs updating,
  but the flat lookup itself is fine to keep for O(1) pane resolution by id
  as long as tree structure lives in `SessionState`, not in the lookup key).

**Design-against list:**
- One resize operation per window subtree, not per pane, with a single
  lock/actor owning the recompute-and-apply-to-all-panes step atomically.
- Explicit invariant for pane-id lifetime across split/close (survives or
  doesn't; write it down, don't discover it from a bug report).
- A regression test analogous to `pane.rs`'s `wait_exit_resolves_after_child_exits`
  that fires two concurrent resizes at sibling panes and asserts a
  consistent final geometry, not just "no panic."

---

## 2. Persistence

**Two genuinely different pitfall classes; don't conflate them.**

### 2a. Torn/partial writes from concurrent async tasks

Today `tymuxd` has *zero* persistence — `shutdown_signal()`
(`crates/tymuxd/src/main.rs:294-318`) explicitly says "There's nothing to
drain beyond that (no persistence exists to flush)." The moment persistence
exists, every state-mutating path becomes a write-race source:
`create_session`/`kill_session` (engine.rs:48-89) each take the `Mutex<HashMap<..>>`
independently and briefly, with no serialization step in between. If a
save-to-disk task snapshots `Engine` state on a timer or on every mutation,
it can race a `kill_session` that's mid-flight — i.e. serialize a session
that's about to disappear, or serialize a partially-constructed
`SessionState` if `create_session` is extended later to do more than one
mutation under the lock.

**What to design against:**
- **Snapshot-under-lock, write-outside-lock.** Never hold the `Mutex` while
  doing file I/O (the exact anti-pattern that turns a fast in-memory
  mutation into a stall that blocks every gRPC handler touching `Engine` —
  today's single `Mutex<HashMap<Uuid, SessionState>>` is already a
  contention point shared by every RPC; adding blocking disk I/O under it
  would reintroduce a hang-class bug of the same shape as the Ctrl-d hang
  already fixed).
- **Atomic file replace, not in-place write.** Write to a temp file +
  `rename()` (POSIX rename is atomic on the same filesystem) rather than
  truncating and rewriting the persisted-state file in place — a crash
  mid-write must never leave a half-written, unparseable state file that
  makes the *next* startup fail to load anything.
- **No flush-on-shutdown today.** `shutdown_signal()` currently does a
  clean stop with nothing to flush. Once persistence exists, the shutdown
  path must explicitly force a final save before `Server::builder()...serve_with_shutdown`
  returns — easy to forget since the current code's comment literally says
  there's nothing to drain, and a future contributor might not revisit that
  comment when adding persistence.
- **Debounce/coalesce, don't save-per-byte.** `Pane`'s reader thread
  (`pane.rs:102-123`) processes pty output at 4096-byte-chunk granularity
  and could arrive dozens of times a second under heavy program output —
  if "persist scrollback" is in scope even as a stretch goal, a naive
  "save on every output chunk" design will thrash disk I/O on ordinary
  `cat`/build-log output. Needs an explicit debounce/periodic-flush policy,
  decided in planning, not discovered under load.

### 2b. Deserializing state that references un-restorable OS handles

This is the sharper pitfall, and the requirements doc already half-names it
in Feasibility Risks: a `Pane` is not just data, it's a live OS process +
pty file descriptors + a reader thread (`pane.rs:36-50`,
`_child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>`,
`_reader_handle: Mutex<Option<std::thread::JoinHandle<()>>>`). None of that
is serializable in any meaningful sense — a restarted `tymuxd` gets a fresh
PID space; the child process from the old daemon instance either:
- still exists as an orphan (if it wasn0't in the same process group /
  wasn't killed), now with no daemon supervising it, no reader thread
  draining its pty, and a full pty output buffer that will eventually block
  the orphaned child on write() — a real hang, just relocated to a
  process nobody can see; or
- was already reaped/killed alongside the daemon, in which case the
  persisted "session" is describing a dead pane with no way to resume it.

**Design-against list:**
- **Persist identity/metadata only, never the `Pane` struct itself.**
  Serialize `{session_id, name, window_id, pane_id, command, rows, cols,
  created_at}` — deliberately excluding `writer`/`master`/`parser`/`_child`
  — and be explicit in the schema/type that this is a *description*, not a
  resumable handle. This should be a distinct struct from `SessionState`
  (which already separates concerns via `SessionInfo` at engine.rs:29-36 —
  follow that existing pattern rather than trying to make `SessionState`
  itself `Serialize`).
- **Startup reconciliation policy, decided explicitly.** On daemon start,
  loading persisted metadata for a pane whose backing process is gone must
  have one defined behavior — e.g. "reopen as a dead/zombie session entry
  the user can explicitly `kill`" vs. "silently drop it and log" — not an
  emergent behavior from whatever `Option`-unwrapping happens to do. Given
  the Feasibility Risk already flags CRIU-style live resume as
  unachievable, plan for the "metadata survives, live pty state does not"
  contract explicitly and make the daemon *tell the user* a session was
  metadata-only-restored (or dropped) rather than silently presenting a
  session entry that will 404 the moment someone tries to `Attach` to it.
- **Orphan child cleanup.** If old child processes can survive a daemon
  restart (e.g. daemon crash rather than clean shutdown), decide now
  whether that's acceptable (orphaned shells reparented to init) or whether
  process-group/session-leader setup should change so a daemon crash takes
  its children down too. This interacts with the "no CRIU-style resume" fact:
  if children can't be resumed, letting them survive as unreachable orphans
  is arguably worse than losing them, since nothing will ever clean them up.
- **Version the persisted format from day one.** Any schema change to what
  gets persisted (near-certain once splits/windows land) needs a documented
  migration or an explicit "refuse to load an old-version file, log loudly"
  fallback — silently `unwrap()`-ing a `serde_json`/`bincode` deserialize
  against a mismatched schema is exactly the kind of silent-failure pattern
  the is-it-ready review flagged repeatedly (issues #2/#3) as this
  project's existing failure mode; persistence load/save is explicitly
  called out in the requirements' Observability Requirements section as
  needing to not be silent.

---

## 3. Scrollback

**Starting point is deliberate and documented.**
`crates/tymux-core/src/pane.rs:10-15`:

```rust
/// vt100's third `Parser::new` arg: how many scrolled-off lines it keeps
/// for scrollback. 0 for now — `CapturePane` only ever reads the current
/// on-screen grid, so there's nothing to gain from buffering history it
/// can't expose yet...
const SCROLLBACK_LINES: usize = 0;
```

Turning this from `0` into "real scrollback" is the whole feature, and the
obvious failure mode is exactly what `0` was chosen to avoid: unbounded
memory growth. Concretely:

- **Naive fix is `usize::MAX`-shaped.** The tempting first pass is "just
  pick a big number" (e.g. 10,000 lines) per pane. `PaneSnapshot`/`grid`
  (pane.rs:52-58, 177-213) already shows the cost model: each row is
  `Vec<CellSnapshot>`, each cell is a heap-allocated `String` plus 3 `u32`s
  — `snapshot()` walks and *copies* the entire visible grid on every call
  already (rows×cols `String` allocations). Scrollback multiplies this by
  however many lines are retained, and if `CapturePane`/a future
  `CaptureScrollback` RPC walks the whole retained history the same way
  `snapshot()` walks the visible grid today, that's a per-request cost
  proportional to buffer size, not visible-screen size — a single
  `CapturePane`-equivalent call against a pane with a large scrollback
  becomes an expensive full-history serialize+copy over gRPC, not a cheap
  24×80 read.
- **Per-pane budget, not global.** With splits landing in the same release,
  "N panes × scrollback-lines-per-pane × cost-per-line" is the real memory
  formula. A fixed global memory budget shared across all live panes (with
  a documented per-pane cap, e.g. "10,000 lines or N MB, whichever is
  smaller") is safer to design against than a flat per-pane line count that
  silently multiplies with pane count under splits.
- **vt100 crate specifics to verify, not assume.** `vt100::Parser::new(rows,
  cols, scrollback_len)` (used at pane.rs:84) — confirm in planning whether
  the `vt100` crate (0.15, per `Cargo.toml:19`) trims scrollback as a ring
  buffer (bounded, O(1) amortized) or as a growable `Vec` that only
  conceptually caps at `scrollback_len` (i.e. verify it isn't silently
  unbounded past the configured value, and that increasing terminal size
  via `resize()` — pane.rs:155-164 — doesn't interact badly with an
  already-populated scrollback buffer, e.g. reflow cost on resize).
- **Never-reaped panes keep scrollback alive forever.** There is currently
  no session/pane GC — the is-it-ready review's Non-Blocking findings
  already note "Sessions HashMap only shrinks on explicit KillSession —
  nothing garbage-collects a session whose client disconnected or whose
  pane died." Once scrollback is real memory, a long-running daemon with
  many created-and-abandoned sessions (a real risk for the AI-agent
  consumer use case named in the requirements — agents that `CreateSession`
  frequently and don't always clean up) accumulates scrollback for panes
  nobody will ever read again. GC/eviction policy for dead-but-unkilled
  sessions should be decided alongside scrollback sizing, not treated as a
  separate later concern.

**Design-against list:** explicit per-pane cap with a global ceiling; verify
`vt100`'s actual ring-buffer behavior rather than assuming boundedness from
the constructor arg name; make scrollback reads incremental/paginated over
gRPC rather than one big message if the RPC needs to return more than a
screen's worth of cells at once (message-size limits on tonic's default
transport become relevant at large history sizes, unlike today's fixed
24×80 `PaneSnapshot`).

---

## 4. Status bar

**Today's model is pure byte passthrough — the status bar breaks that
invariant on purpose, so its failure modes are all about doing that
safely.** `crates/tymux-cli/src/main.rs:192-206`:

```rust
let mut stdout = std::io::stdout();
while let Some(event) = inbound.message().await? {
    match event.payload {
        Some(attach_event::Payload::Output(bytes)) => {
            stdout.write_all(&bytes)?;
            stdout.flush()?;
        }
        ...
    }
}
```

Every pty output chunk is written straight to stdout, unmodified, as soon
as it arrives — there is no notion of "screen region" today; the terminal
itself owns 100% of the display. A status bar means the CLI must reserve
and repaint a region of the real terminal without corrupting whatever the
pty's own ANSI stream is doing to the rest of it. Concrete pitfalls:

- **The pty doesn't know a status bar exists.** Programs inside the pane
  (vim, tmux-inside-tymux, full-screen TUIs) address the terminal in
  absolute coordinates based on the size *they* were told via `Resize`
  (pane.rs:155-164, sent from `send_resize`/`spawn_resize_watcher` in
  `tymux-cli/src/main.rs:211-247`). If the status bar steals a row from the
  bottom, the pty-side size passed via `Resize` must be `real_rows - 1`,
  not `real_rows` — otherwise the pane thinks it owns the full terminal and
  will scroll/redraw over the status bar the instant it writes to the last
  row. This is a hard requirement, not a nice-to-have: get it wrong and
  every full-screen TUI running inside a tymux pane visibly fights the
  status bar for the last line.
- **Partial writes / interleaving.** Current code calls `write_all` +
  `flush()` per chunk with no coordination with anything else writing to
  stdout. A status bar redraw (from a separate timer/task, e.g. re-render
  every second for a clock or session name) writing to the same `stdout`
  concurrently with the pty-output writer is a literal data race on the
  terminal byte stream — cursor-positioning escape sequences from one write
  can land in the middle of the other's output, corr21upting both. This
  needs one serialized writer (single task/actor owning stdout, or a
  `Mutex` around it) — not two independent tasks both calling
  `stdout.write_all()`.
- **Cursor save/restore discipline.** Any status-bar redraw must save
  cursor position, move to the reserved region, draw, then restore —
  standard terminal practice (`\x1b[s`/`\x1b[u` or manual row tracking) but
  entirely new to this codebase; get the sequence wrong (e.g. redraw fires
  while the pty stream is mid-escape-sequence, splitting a CSI sequence
  across the status bar's own writes) and cursor position visibly jumps or
  characters land in the wrong place.
- **Terminal resize must resize both regions together.** `spawn_resize_watcher`
  (tymux-cli/src/main.rs:230-247) currently sends one `Resize` reflecting
  the whole terminal. Post-status-bar, a SIGWINCH needs to: recompute the
  pty's effective size (full size minus status bar rows), send that
  adjusted `Resize`, *and* repaint the status bar at its new row position —
  three things that must happen together, not as three independently-timed
  events, or the status bar and pane content visibly disagree about
  terminal size for a frame or more.
- **Raw mode already strips line discipline.** `RawGuard` (tymux-cli/src/main.rs:59-75)
  already puts the local terminal in raw mode for the whole attach
  duration — the status bar has no line-buffering/carriage-return safety
  net to rely on; every redraw must emit exactly the bytes intended,
  including explicit `\r\n` where needed (a naive `println!` in raw mode
  produces stair-stepped output, visible today in the `"\r\n[tymux: pane
  exited]"` message at main.rs:201 having to hand-write `\r\n` for exactly
  this reason — the status bar needs the same discipline everywhere it
  writes).

**Design-against list:** pty logical size ≠ real terminal size once a
status bar reserves rows, and every resize path must update both together;
single serialized writer for stdout; explicit cursor save/restore around
every status-bar redraw; treat this as "a real terminal-rendering layer,"
per the requirements' own Rabbit Holes note — evaluate an existing crate
(e.g. `crossterm`'s cursor/queue primitives, already a dependency at
`Cargo.toml:23`) rather than hand-rolling escape sequences from scratch.

---

## 5. Config/key-bindings

**Zero precedent today — 100% of stdin is forwarded verbatim.**
`crates/tymux-cli/src/main.rs:169-186`:

```rust
std::thread::spawn(move || {
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let msg = AttachRequest {
                    payload: Some(attach_request::Payload::Input(buf[..n].to_vec())),
                };
                if stdin_tx.blocking_send(msg).is_err() { break; }
            }
        }
    }
});
```

Every byte read from stdin becomes an `Input` message, no inspection, no
interception, no local state machine. Introducing a detach key / prefix key
/ copy-mode trigger means this loop stops being a pure pipe. Pitfalls to
design against explicitly:

- **The prefix-key collision problem.** tmux's own `Ctrl-b` model exists
  because *any* single byte or short sequence tymux reserves for local
  interception is a byte a shell, editor, or TUI program running inside the
  pane can no longer receive unmodified — e.g. if `Ctrl-d` (a real,
  extremely common shell control character — EOF) or any common
  emacs/vim/readline binding gets chosen as the detach key, existing users
  lose that keystroke inside every pane, silently, unless there's a
  well-known escape/passthrough mechanism. This must be a deliberate
  choice (prefix sequence like tmux's `Ctrl-b` + key, not a bare single
  key) or documented very clearly as a behavior change from "100% passthrough."
- **Byte-boundary vs. keystroke-boundary mismatch.** The current reader
  reads into a 1024-byte buffer and does not deserialize UTF-8/escape
  sequences at all — it forwards raw bytes as they arrive from the OS read
  syscall, which is not the same as "one read() call = one keystroke."
  Multi-byte sequences (arrow keys, function keys, pasted text, UTF-8
  multi-byte characters) can arrive split across two `read()` calls under
  load, or multiple keystrokes can arrive coalesced in one `read()` (fast
  typing, paste). A local key-binding matcher needs its own buffering/
  state-machine layer that reassembles logical keystrokes *before* matching
  against bindings — matching raw per-`read()`-call byte chunks against a
  fixed binding table will misfire on both split escape sequences and
  coalesced input. This is a nontrivial parser, not a simple byte-equality
  check, and terminal escape-sequence parsing has a long history of
  edge cases (timing-based ESC-vs-Alt disambiguation, bracketed paste mode
  interactions, etc.) that a first pass is likely to get wrong.
- **Bracketed paste is a real, sharp edge.** If the pane program enables
  bracketed paste mode, pasted text arrives wrapped in
  `ESC[200~...ESC[201~` and must never be scanned for key bindings byte-by-
  byte (a paste that happens to contain the detach sequence's bytes must
  not trigger detach) — this needs explicit handling, not something a
  naive byte-matcher will get right by accident.
- **Local interception vs. server authority.** Resize/detach are naturally
  local-only (the CLI's own terminal state), but decide explicitly which
  key bindings are purely local (detach, copy-mode entry) vs. which forward
  a *different* payload to the daemon (e.g. eventually a "next pane" binding
  that's really a windowing operation the daemon must know about). Blurring
  this line risks either double-handling (both CLI and daemon think they
  own an action) or a binding silently doing nothing because it was
  swallowed locally when the daemon needed to see it.
- **Regression risk for the "everything is passthrough" mental model.**
  README and existing users' expectations (once README documents ctrl-d
  behavior accurately, per the is-it-ready fix) currently promise raw
  passthrough. Introducing *any* local interception is a real behavior
  change for existing scripts/muscle memory — needs to be off-by-default or
  use an extremely unlikely-to-collide prefix, and needs its own escape
  hatch (a way to send the literal prefix byte through to the pane, as tmux
  does with `prefix, prefix`).

**Design-against list:** prefix-based (not bare-single-key) binding scheme;
a real keystroke-reassembly layer independent of raw `read()` chunk
boundaries; explicit bracketed-paste passthrough; a documented
local-vs-server-authority split per binding; an escape hatch to send the
prefix byte itself literally.

---

## 6. Cross-language client codegen (bidirectional streaming specifically)

`proto/buf.gen.yaml:1-12` currently has `plugins: []` — genuinely
zero codegen configured for any non-Rust target; Rust codegen bypasses buf
entirely via `tonic-build` in `crates/tymux-proto/build.rs`. The one RPC
that actually matters for proving the cross-language claim is `Attach`
(`proto/tymux/v1/tymux.proto`'s `rpc Attach(stream AttachRequest) returns
(stream AttachEvent)`) — a true bidirectional stream, not a unary call.
Known pitfall classes specific to bidi-stream codegen (not simple
request/response RPCs):

- **Connection/stream lifecycle mismatches across languages.** tonic
  (Rust/`h2`) and a TS gRPC-Web/Connect client have *different* defaults
  for how a half-closed stream behaves — e.g. does closing the client's
  send side (TS: ending the writable side of the stream) immediately end
  the server's ability to keep sending on the receive side, or is it
  legitimately half-duplex-capable? `main.rs`'s `attach()` handler
  (tymuxd/src/main.rs:155-243) spawns two independent tasks — one reading
  `inbound` (client→server) and one writing to `tx`/`rx` (server→client) —
  which assumes true independent half-duplex operation. If the chosen TS
  toolchain (Connect-RPC/`@connectrpc/connect` per the requirements'
  candidate) doesn't support truly independent half-close the same way
  tonic does, a TS client that stops sending input (e.g. after the user
  detaches without explicitly closing the stream) could unexpectedly
  terminate the whole bidi stream instead of just the send side, breaking
  the "still receiving output" half.
- **Error-handling convention mismatch.** tonic surfaces errors as
  `tonic::Status` with a code + message (used throughout `main.rs`, e.g.
  `Status::not_found("no such pane")` at main.rs:176) — how a TS/Connect
  client surfaces the equivalent (thrown exception vs. a result-union type
  vs. a special stream-closed-with-error frame) is a different idiom
  entirely, and the *first message contract* this protocol relies on (the
  client's first `AttachRequest` on the stream must set `pane_id` —
  enforced at main.rs:161-172 with `Status::invalid_argument` if violated)
  needs to be validated as actually achievable in whatever TS codegen path
  is chosen: some codegen toolchains make it awkward to send a "first
  message is special, rest are a different shape" pattern cleanly (the
  proto already models this correctly via `oneof payload` at
  `AttachRequest`, but client-side ergonomics for "send this one message,
  then these other ones" vary a lot by generated client shape).
- **Streaming backpressure/buffering differences.** The daemon's `Attach`
  forwarding task uses a bounded `mpsc::channel(64)` (main.rs:181) sitting
  behind a `broadcast::channel(1024)` per pane (pane.rs:27,85) with documented
  lossy behavior under a slow consumer ("a slow consumer just gets `Lagged`
  and moves on"). A browser/TS client's underlying transport (HTTP/2 via
  gRPC-Web or Connect's own framing) has its own flow-control behavior that
  may interact with this differently than tonic's native HTTP/2 transport —
  validate this doesn't produce different loss/ordering behavior than the
  Rust CLI experiences today, since "prove the cross-language client works"
  in the requirements implies parity of experience, not just "compiles and
  connects."
- **`buf generate` plugin selection is unproven, not just unconfigured.**
  The Rabbit Holes section already flags this correctly: picking
  `protoc-gen-es` + `@connectrpc/connect` (or an alternative like
  `ts-proto` with `grpc-web`) needs an actual end-to-end spike against the
  real `Attach` RPC *before* committing to it in the architecture plan —
  discovering mid-implementation that the chosen toolchain's bidi-stream
  support is incomplete/experimental is exactly the kind of late-discovery
  risk the requirements' Feasibility Risks section is trying to avoid.

**Design-against list:** an early, throwaway spike attaching a minimal TS
client to a real running `tymuxd` and exercising `Attach` end-to-end
(input + output + a clean detach) before any client code is treated as
load-bearing; explicit written contract for what "detach" means at the
transport level for a non-Rust client (close send side? send a sentinel
message? just stop reading?); don't assume tonic's `Status`/half-duplex
semantics transfer 1:1 to the chosen TS toolchain's idioms.

---

## 7. Release CI (cross-compiling Rust across OS/arch, zero real CI history)

`.github/workflows/ci.yml:1-21` is the *entire* CI history for this
project — a single `ubuntu-latest` job running `buf lint`, `cargo fmt
--check`, `cargo clippy`, `cargo test`. It has never actually run against a
real PR/remote (the is-it-ready report says so explicitly: "no remote
configured, nothing to check"). This means every cross-compilation pitfall
below is unvalidated territory, not "probably fine, we've seen it work
before":

- **No git remote exists yet.** This is a literal prerequisite, not a
  parallel task — setting one up and confirming the *existing* single-job
  CI actually runs and passes for real (not just locally) is the first
  real signal this project has ever had about its own CI, before adding
  the much harder cross-compile/release matrix on top.
- **`portable-pty` is a Unix/pty-system-call-heavy dependency.** Cross-
  compiling from `ubuntu-latest` to macOS targets (`x86_64-apple-darwin`,
  `aarch64-apple-darwin`) is not just a `--target` flag away for a crate
  that likely links against platform pty APIs — cross-compiling to macOS
  from Linux generally needs either GitHub's native `macos-latest`
  runners (simplest, but two separate OS runners rather than one
  cross-compile matrix) or an `osxcross`-style toolchain (fragile, easy to
  get wrong, not worth the complexity for a two-OS matrix). Plan for
  native runners per OS (`ubuntu-latest` + `macos-latest`, cross-compiling
  only *within* each for arm64/x86_64 where each OS's native toolchain
  supports it) rather than one Linux box cross-compiling everything.
- **macOS arm64 vs x86_64 needs Rosetta-independent validation.**
  `aarch64-apple-darwin` from a `macos-latest` (Apple Silicon) runner is
  straightforward; producing a working `x86_64-apple-darwin` binary and
  actually *testing* it (not just compiling it) needs either a
  `macos-13`-class Intel runner or accepting compile-only validation for
  that target — decide explicitly whether "prebuilt binary" implies "CI
  actually ran the test suite on that exact target," since a
  compiles-but-untested cross target is a real risk for a pty-syscall-heavy
  crate.
- **glibc vs musl for Linux binaries.** "Installable without a Rust
  toolchain" (a stated success metric) for Linux implies deciding whether
  the release binary targets `x86_64-unknown-linux-gnu` (glibc — smaller
  build matrix risk, but ties the binary to a minimum glibc version on the
  user's machine, a classic "works on the CI runner's Ubuntu, fails on the
  user's older distro" trap) or `x86_64-unknown-linux-musl` (fully static,
  portable across distros, but `portable-pty`/any C-linked dependency needs
  to actually cross-compile cleanly against musl — not guaranteed without
  testing).
- **Version/tag consistency across the workspace.** `Cargo.toml`'s
  `[workspace.package] version = "0.1.0"` (Cargo.toml:11) is currently a
  single shared version for all 4 workspace crates
  (`tymux-core`/`tymux-proto`/`tymuxd`/`tymux-cli`). A release pipeline
  needs one clear source of truth for "what does `v1.0.0` mean" (the git
  tag? the workspace version? do they need to match and be enforced in CI,
  or is the binary version independent of the tag) — worth deciding in
  planning rather than letting it drift the first time someone forgets to
  bump `Cargo.toml` before tagging.
- **Release job should not be the first time the full matrix is exercised.**
  Given CI has literally never run for real, the safest sequencing is:
  (1) get the existing single-job Linux CI running for real against the new
  remote first, as its own verifiable milestone; (2) add the macOS job to
  the same PR-gating CI (not just the release workflow) so cross-platform
  build breakage is caught on every PR, not only when cutting a release;
  (3) only then add the tag-triggered release workflow that reuses the
  already-proven build steps to produce and upload binaries. Building the
  release pipeline as the *first* thing that exercises macOS builds risks
  discovering basic cross-platform compile breakage (e.g. a Unix-specific
  `#[cfg(unix)]` path already present in `tymux-cli/src/main.rs:230-247`
  and `tymuxd/src/main.rs:299-310` compiling differently, or not at all, on
  a target that was never actually built until release time) at the worst
  possible moment — during an attempted release, not during ordinary
  development.

**Design-against list:** native per-OS runners, not Linux-hosted
cross-compilation, for macOS targets; explicit glibc-vs-musl decision for
Linux with a stated minimum-distro-compat rationale; PR-gating CI expanded
to the full OS matrix *before* a release workflow is built on top of it;
one explicit source of truth for version numbers tying `Cargo.toml` to git
tags; treat "get CI running for real against a live remote" as its own
first milestone, separate from and prerequisite to the release-binary work.

---

## Cross-cutting observation

Several of the pitfalls above are the *same underlying class* of bug the
is-it-ready review already found and fixed once (`docs/reviews/is-it-ready-2026-07-13.md`):
something silently blocks forever, or something fails silently with no
observability. Specifically:
- Splits' cross-pane resize race and persistence's torn writes are both
  "state mutated across multiple lock scopes with no atomicity" — the same
  shape as the original `Pane::resize()` two-mutex pattern, just at larger
  scope.
- Persistence's silent-deserialize-failure risk and config/key-bindings'
  silent-keystroke-swallowing risk are both "something goes wrong and the
  user/operator has no signal" — the same shape as the original
  Ctrl-d hang (issues #2/#3 in the is-it-ready report: swallowed errors,
  zero structured logging at the time).
- The status bar's dual-writer race and the daemon's `Attach` forwarding
  task's `tokio::select!` (main.rs:191-215, already using `biased` to avoid
  one specific ordering race) are both "two independent tasks touch a
  shared output sink with no serialization" — worth treating as one design
  principle applied consistently ("every shared output sink gets exactly
  one writer/owner task") rather than solving each occurrence ad hoc.

This suggests Phase 3 planning should consider a small number of *shared*
architectural primitives (a generic "atomic multi-step state mutation"
helper; a standard "single-owner writer" pattern for shared sinks; a
consistent silent-failure-is-not-allowed logging convention) rather than
solving concurrency/observability independently per epic.
