# Implementation Plan: unix-socket-auth

**Feature**: `tymuxd` gains a Unix-domain-socket (UDS) listener, active by
default alongside its existing TCP-loopback listener, gated by
kernel-verified peer credentials (`SO_PEERCRED` via `UnixStream::
peer_cred()`) rather than any client-supplied claim — with owner-only
(`0600`) permissions by default, an optional group-access relaxation
(`0660` + `chown`), a TOCTOU-safe creation sequence, and a startup
deprecation warning (plus an opt-in off-switch) for the now-unauthenticated
TCP-loopback path it sits beside. `tymux-cli`, `clients/go`, `clients/ts`
each dial the UDS path by default, falling back to TCP with a logged
notice, with integration tests proving the accept path end-to-end against
a real daemon in every client, plus unit-level proof of the reject
*decision* logic (`peer_is_authorized`) — a true cross-uid, real-`peer_cred()`-delivered
reject integration test additionally runs wherever CI has root/`CAP_SETUID`,
which this repo's actual CI (plain `ubuntu-latest`/`macos-latest`
runners, no `container:` directive) does not, so those specific
integration tests ship `#[ignore]`/skipped from day one (see Tasks
6.4.1c/7.3.1b/8.3.1c and the Unresolved Questions entry this resolves).
**Date**: 2026-08-29
**Status**: Ready for implementation
**ADRs**: [ADR-001](../decisions/ADR-001-toctou-safe-socket-creation.md)
(umask-before-bind + lock-file stale-socket handling),
[ADR-002](../decisions/ADR-002-group-membership-resolution.md)
(Linux `/proc`-based supplementary-group check, primary-gid fallback
elsewhere), [ADR-003](../decisions/ADR-003-tower-hyper-util-for-cli-uds-dialing.md)
(`tower`/`hyper-util` as new direct `tymux-cli` dependencies)

## Prerequisites

**This plan's implementation cannot start until `bearer-token-auth`
(PR #43) is merged to `main`.** This project extends
`crates/tymuxd/src/auth.rs`, which `bearer-token-auth` introduces —
Epics 1-3's tasks cite exact current line numbers and function shapes in
that file (`resolve_token`'s precedent, `BearerAuthInterceptor`'s
existing counter/log framing, etc.) that only exist post-merge. Confirmed
still unmerged at planning time: `git log origin/main..origin/feature/bearer-token-auth
--oneline` returns 12 commits. Starting implementation against the
unmerged branch risks those cited line numbers/shapes drifting before
this plan's own work lands. See requirements.md's Constraints section for
the same note.

---

## Step 0.5 — Alternatives considered

Three shapes for "how does `tymuxd` serve both TCP and UDS with two
different identity-verification mechanisms" were weighed:

**A. Two independent `Server::builder()` tasks (TCP + UDS), each
`add_service`-ing a `.clone()`d `TymuxDaemon`, gated by their own
listener-specific interceptor, joined via `tokio::join!`** (chosen).
Strength: matches tonic's own documented UDS example exactly — no
type-erasure needed, `IO` stays a single concrete type per
`serve_with_incoming` call (confirmed against tonic 0.12.3's actual
`serve_with_incoming<I, IO, ...>` signature, which cannot take two IO
types at once) — and keeps the two auth mechanisms (bearer-token vs.
peer-cred) as separate concerns, exactly like today's loopback/
non-loopback branch already does. Weakness: doubles the
`Server::builder()` boilerplate at the call site and needs the two
listener futures joined without either being dropped mid-graceful-drain
(resolved with `tokio::join!`, not `select!` — see Pattern Decisions).

**B. A single unified listener using a `tower`-level `Either<TcpStream,
UnixStream>` wrapper, type-erasing both IO types into one
`serve_with_incoming` call.** Strength: one `Server::builder()` call, one
code path, zero risk of the two listeners' configuration drifting apart.
Weakness: real, unneeded machinery — `Connected::ConnectInfo` (the
associated type that carries peer identity into request extensions) would
itself have to become an enum, and every place reading `UdsConnectInfo`/
`TcpConnectInfo` would need a match neither Option A nor tonic's own
example ever requires.

**C. Route everything through the UDS listener only; have the
TCP-loopback socket be a thin transparent byte-forwarding proxy into the
UDS listener**, so there is exactly one enforcement point. Strength: a
single authorization code path, zero duplicated interceptor logic.
Weakness: a byte-forwarding proxy becomes the UDS listener's "peer" for
every TCP-origin connection — `research/pitfalls.md` §3 confirms a relay
always reports *its own* credentials to `peer_cred()`, never the original
caller's — so this would either disable peer-cred enforcement for all
TCP-origin traffic (defeating the design) or require a second,
different identity-forwarding mechanism, strictly more complexity than
Option A for the same end state as "just keep running bearer-token auth
on the TCP side, like today."

**Chosen: A.** Recorded in Pattern Decisions below with its own two rows
(composition pattern + the interceptor-vs-stream-gate sub-decision).

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| UDS listener | The `tokio::net::UnixListener`-backed tonic `Server` `tymuxd` serves alongside its existing TCP listener, both active by default ("both-by-default"). | New in this feature. |
| `default_uds_socket_path` | Pure function computing the daemon's default socket path: `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` if that env var is set and non-empty; else `<base>/tymuxd-<uid>/tymuxd.sock`, where `<base>` is `$TMPDIR` if set and non-empty, else `/tmp`. Both branches nest under a subdirectory `tymuxd` itself creates and owns (`tymuxd/` vs. `tymuxd-<uid>/`) — deliberately symmetric, so `bind_uds_listener` never has to special-case which parent directory it's allowed to `chmod` (architecture-review.md Blocker fix). | The one documented algorithm; mirrored independently (not shared) across `tymuxd`, `tymux-cli`, `clients/go`, `clients/ts` — see Pattern Decisions row 10. |
| `resolve_uds_socket_path` | Resolves the *effective* socket path: `--socket-path <value>`/`--socket-path=<value>`/`TYMUXD_SOCKET_PATH` override (flag beats env, empty treated as absent) if set, else `default_uds_socket_path`. | Same flag-over-env, empty-is-absent shape as `resolve_token`. |
| `SocketLockGuard` | An open `File` holding an exclusive, non-blocking `flock()` on `<socket path>.lock`, held for the daemon's entire process lifetime. | Serializes the stale-socket-probe-then-bind sequence across concurrently starting `tymuxd` instances — ADR-001. |
| `reconcile_stale_socket` | Given a socket path (only ever called while `SocketLockGuard` is held): if a file exists there, probes it with `UnixStream::connect`; a successful connect means a live daemon holds the path (fatal error, abort startup); `ConnectionRefused`/`NotFound` means stale (removes the file). | ADR-001. |
| `bind_uds_listener` | Orchestrates the TOCTOU-safe bind: ensures the parent directory exists at mode `0700`, sets the process `umask` to `0o177` (owner-only) or `0o117` (group-access case, yields `0660`), calls `UnixListener::bind`, restores the prior `umask`, then `chown`s to the configured gid if one is set. | ADR-001. |
| `resolve_socket_group_name` | Resolves `--socket-group <value>`/`--socket-group=<value>`/`TYMUXD_SOCKET_GROUP` (flag beats env, empty is absent) to a group *name* string. | Mirrors `resolve_token`'s shape exactly. |
| `resolve_gid_by_name` | Resolves a POSIX group name to a `gid_t` via `libc::getgrnam`. | Unknown name is a fatal startup error, never a silent skip. |
| `PeerIdentity` | Small value object `{ uid: u32, gid: u32, pid: Option<i32> }`, constructed once from `UCred` (`impl From<&UCred> for PeerIdentity`) at the point a UDS connection is accepted (`PreAuthorizedUnixStream::new`). | New in this feature's Gate-2 fix — decouples `peer_is_authorized`/`peer_is_group_member` from tokio's concrete `UCred` type and gives "a uid" vs. "a gid" distinct types instead of same-primitive, positional `u32` params (architecture-review.md's DIP/primitive-obsession Concern fix). |
| `peer_is_authorized` | Pure decision function: `true` iff the peer's uid equals the daemon's own effective uid, or a group is configured and the peer is a member of that group. | Inputs are the daemon's own uid (`libc::geteuid()`, read once at startup) and a `PeerIdentity` — never anything client-supplied. |
| `peer_is_group_member` | Linux: checks the configured gid against the peer's *full* group list, read from `/proc/<pid>/status`'s `Groups:` line. All other platforms: checks only the peer's primary/effective gid (`PeerIdentity::gid`). | ADR-002. |
| `UdsAuthDecision` | `{ authorized: bool, peer_uid: Option<u32>, peer_gid: Option<u32> }` (`Copy`) — the cached *outcome* of `peer_is_authorized`, computed exactly once per accepted `UnixStream` by `PreAuthorizedUnixStream::new`, then cloned into request extensions on every RPC on that connection (same per-request-clone lifecycle tonic's own `UdsConnectInfo` already has, but carrying the decision, not the raw credential). | New in this feature's Gate-2 fix — see `UdsPeerCredInterceptor` and Pattern Decisions row "UDS request gate — where the decision itself is computed." |
| `PreAuthorizedUnixStream` | Thin wrapper around an accepted `tokio::net::UnixStream` that computes `UdsAuthDecision` once, at accept time — before the stream enters tonic's HTTP/2 handshake — and implements `tonic::transport::server::Connected` (`type ConnectInfo = UdsAuthDecision`) so that decision, not a raw `UCred`, is what gets cloned into extensions per-request. | Installed via `.map_ok(...)` on the `UnixListenerStream` passed to `serve_with_incoming_shutdown` (Task 4.2.2a). |
| `UdsPeerCredInterceptor` | `tymuxd`-side `tonic::service::Interceptor`, registered only on the UDS listener, gating every RPC by reading the pre-computed `UdsAuthDecision` out of request extensions — never calls `peer_is_authorized`/`peer_cred()` itself, and therefore never re-runs the decision (including the `--socket-group` case's `/proc` read) per RPC. | Distinct from, not composed with, `BearerAuthInterceptor` (architecture.md §2; Pattern Decisions row 3). Fixes a per-RPC re-execution bug found in Gate-2 review — see Observability Plan's Performance NFR. |
| `UdsConnectInfo` | tonic's own type (`peer_addr: Option<Arc<SocketAddr>>`, `peer_cred: Option<UCred>`), populated exactly once per accepted `UnixStream`, before any RPC on that connection runs, by tonic's `Connected for UnixStream` impl. | Ships in tonic 0.12.3 already — confirmed by reading `tonic-0.12.3/src/transport/server/unix.rs` directly; not written by this feature. **Not used directly by this feature's own code** — `PreAuthorizedUnixStream` supplies its own `Connected` impl instead, so `UdsAuthDecision` (not `UdsConnectInfo`) is what actually lands in request extensions on the UDS listener. |
| `tymux_socket_peercred_rejection_total` | Counter/log field name for rejected UDS peer-cred checks, matching the `tymux_attach_resume_outcome_total`/`tymux_auth_rejection_total` naming convention. | Owned by `UdsPeerCredInterceptor`, `Arc<AtomicI64>`, same shape as `BearerAuthInterceptor`'s existing counter. |
| `resolve_tcp_disabled` | Resolves `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK` to a `bool` — presence of the flag, or a non-empty env value, means disabled; default `false` (TCP stays on). | New off-switch, defaulted off, per architecture.md §6's "cheap now" recommendation. |
| TCP-deprecation warning | The new, additive `tracing::warn!` fired once at startup whenever the TCP listener is about to be spawned (`!tcp_disabled`), naming both the deprecation and the "TCP remains fully unauthenticated regardless of the new UDS listener" caveat. | Distinct from the existing non-loopback bearer-token warning at `main.rs:1261-1273` — this one fires for *any* TCP bind, loopback included. |
| `TymuxDaemon: Clone` | The `#[derive(Clone)]` added to `TymuxDaemon` so one constructed instance is registered on both listeners without splitting `disconnect_tracker`/`attached_sessions_gauge`/`resume_outcome_counters` into two independent, non-communicating copies. | architecture.md §1 — every field is already `Arc<T>` or a `Copy` `Duration`; the derive is free. |
| `--addr` (tymux-cli) | Changes from a hardcoded `default_value = "http://127.0.0.1:7419"` to `Option<String>` — `None` means "no explicit TCP target given, try UDS first"; `Some(_)` means "dial exactly this, skip UDS entirely." | features.md's unstated-needs finding #1. |
| UDS-first client dialing | `tymux-cli`/`clients/go`/`clients/ts` each independently resolve the effective socket path (`tymux-cli` via a `clap` `--socket-path`/`TYMUXD_SOCKET_PATH` field falling back to `default_uds_socket_path`; `clients/go`/`clients/ts` via their own `resolve_uds_socket_path`-shaped functions, since neither has a `clap`-equivalent flag layer of its own), dial it first, and fall back to `http://127.0.0.1:7419` over TCP with a single logged notice on failure — never gated on `isatty()`. | ux.md §3's "no TTY-conditional branching precedent" finding; ux.md case 5. |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Server-side dual-transport composition | Two `Server::builder()...serve_with_incoming[_shutdown]` tasks sharing one `daemon.clone()`, joined via `tokio::join!` | architecture.md §1, build-vs-buy.md §2, Step 0.5 (A) | (B) unified `Either<TcpStream, UnixStream>` type-erasure | Real extra machinery — `ConnectInfo` itself would need to become an enum — for zero behavioral benefit over two structurally-identical tasks tonic's own example already validates. |
| Server-side dual-transport composition | (as above) | (as above) | (C) TCP-as-byte-forwarding-proxy into the UDS listener | A relay reports its *own* credentials to `peer_cred()`, never the original caller's (pitfalls.md §3) — would disable peer-cred enforcement for all TCP-origin traffic or require a second identity-forwarding mechanism. |
| UDS request gate | `PreAuthorizedUnixStream` (computes `UdsAuthDecision` once, at accept time) + `UdsPeerCredInterceptor` (`tonic::service::Interceptor` reading that cached `UdsAuthDecision` from request extensions) | architecture.md §2, build-vs-buy.md §1, architecture-review.md's per-RPC-re-execution Concern fix | A custom `Stream` adapter rejecting/dropping the raw `UnixStream` before `serve_with_incoming` sees it | `architecture.md` §2 traced tonic's actual `MakeSvc::call` (`tonic-0.12.3/src/transport/server/mod.rs:1005-1064`) and confirmed `connect_info()` (the syscall-extraction step) runs exactly once per accepted connection, before the HTTP/2 handshake — the "once per connection" NFR is satisfied via the existing `Interceptor` extension point *once the decision itself, not just the raw credential, is what's cached there* (see the next row) — a separate `Stream`-adapter-based reject-before-`serve_with_incoming` mechanism remains unnecessary extra machinery either way. |
| UDS request gate — where the decision itself is computed | Once, in `PreAuthorizedUnixStream::new`, at accept time — `UdsPeerCredInterceptor::call()` only ever reads the cached `bool` | architecture-review.md Concern fix (Gate 2) | This plan's original design: `UdsPeerCredInterceptor::call()` reads tonic's own `UdsConnectInfo` and calls `peer_is_authorized` itself, fresh, on every request | tonic clones `UdsConnectInfo` into `request.extensions_mut()` on **every** request on a connection (`mod.rs:1038-1042`) — only the `getsockopt(SO_PEERCRED)` syscall itself is cached once; an `Interceptor` deriving the authorization *decision* from that raw credential therefore re-runs `peer_is_authorized` — including the `/proc/<pid>/status` supplementary-group read in the `--socket-group` case — on every RPC, contradicting requirements.md's Performance SLO ("peer-credential check happens once per connection... no measurable per-call overhead"). |
| Group-membership check granularity | Linux: `/proc/<pid>/status` supplementary-group read. Elsewhere: primary/effective gid only (`PeerIdentity::gid`, sourced from `UCred::gid()` at construction). | ADR-002 | `libc::getgrouplist`/`getpwuid_r` FFI on every platform | Fully portable but genuinely new unsafe/FFI surface for a scope whose primary target platform (Linux, per requirements.md's Constraints) already has a zero-FFI answer. |
| Group-membership check granularity | (as above) | (as above) | `peer.gid() == configured_gid` everywhere, no `/proc` path | Fails the requirement's own worked scenario (a teammate added via supplementary group) on Linux — the platform this project must get fully right. |
| TOCTOU-safe permission setting | `umask`-before-`bind`, restored after; `chown`-after for group ownership | ADR-001, pitfalls.md §1 | `chmod`/`chown` immediately after `bind()`, no umask change | Confirmed-exploitable window (pitfalls.md §1); `fchmod`/`fchown` on the fd are no-ops for `AF_UNIX` on Linux, ruling out the usual fd-based fix. |
| TOCTOU-safe permission setting | (as above) | (as above) | Bind-to-temp-name-then-`rename()` | Still leaves a (smaller) `bind`-to-`chmod` window at the temp path — no better than a genuinely zero-window `umask` change. |
| Stale-socket handling | Connect-probe-then-unlink, serialized by a companion `flock()`ed lock file | ADR-001, pitfalls.md §2 | Blind `if path.exists() { unlink() }` before `bind()` | Can steal the socket out from under a live second `tymuxd` instance (operator error, or a supervisor-restart race). |
| `tymuxd` new-flag mechanism | Hand-rolled `std::env::args()` scan (`resolve_uds_socket_path`, `resolve_socket_group_name`, `resolve_tcp_disabled`), matching `resolve_token`'s exact shape | bearer-token-auth's ADR-002 precedent | Add `clap` to `tymuxd` | Same reasoning as bearer-token-auth's ADR-002 — `tymuxd` has zero CLI-flag parsing today and stays dependency-light; three more optional flags in the same hand-rolled scanner is additive, not a new-dependency decision. |
| `tymux-cli` new-flag mechanism (`--socket-path` only — `--socket-group`/`--disable-tcp-loopback` are `tymuxd`-only bind-time flags with no client-side analogue) | Ordinary `clap` field, `#[arg(long, global = true, env = "TYMUXD_SOCKET_PATH")] socket_path: Option<String>` — matching `--token`'s exact shape | ux.md Gap 2's Concern fix; `--token`'s existing declaration at `crates/tymux-cli/src/main.rs:194` | Reuse `tymuxd`'s hand-rolled `std::env::args()` scan inside `tymux-cli` too (this plan's original Task 6.2.1b design, per ux.md's own flagged Gap 2) | Unlike `tymuxd`, `tymux-cli` already depends on and uses `clap`, and its own `--token` flag already renders in `tymux --help` via clap's auto env-annotation (verified directly: `crates/tymux-cli/src/main.rs:194`, `#[arg(long, global = true, env = "TYMUXD_TOKEN", hide_env_values = true)]`) — a hand-rolled scan for `--socket-path` would make it invisible to `tymux --help` while `--token`, shipped one feature earlier on the same CLI, stays visible: an asymmetry this feature would introduce, not inherit. `tymuxd`'s own dependency-light reasoning (row above) is specific to `tymuxd` having zero pre-existing `clap` usage, which doesn't hold for `tymux-cli`. |
| Socket-path algorithm code location (Rust) | Mirrored (duplicated) pure functions in `tymuxd`'s `auth.rs` and `tymux-cli`'s `main.rs` — not shared via `tymux-core` | This plan's own analysis | Move `default_uds_socket_path`/`resolve_uds_socket_path` into `tymux-core`, shared by both binaries | `tymux-core` pulls in `portable-pty`/`vt100` (PTY-spawning, terminal-emulation machinery) — disproportionate weight to add to `tymux-cli`'s dependency graph for a handful of path-string lines, and breaks the established `BearerToken`-mirrored-not-shared precedent between these same two binaries. Divergence risk is mitigated by a canonical doc-comment spec plus one shared, language-agnostic fixture file (`project_plans/unix-socket-auth/socket-path-fixtures.json` — see Epic 1.1/6.1/7.1/8.2's "Files" lists) that all four implementations' test suites *read* rather than re-authoring the same Given/When/Then cases inline (architecture-review.md's test-duplication-drift Concern fix), not by shared implementation code. |
| `tymux-cli` UDS dialing | `tonic::Endpoint::connect_with_connector` + `tower::service_fn` + `hyper_util::rt::TokioIo` | ADR-003, tonic 0.12.3 pinned source (`endpoint.rs:364-404`), build-vs-buy.md §3b | A hand-rolled `tower::Service` impl | `service_fn` is tonic's own documented mechanism for exactly this case; a hand-rolled impl duplicates it for no benefit. `tower`/`hyper-util` are already transitive dependencies via `tonic` (`tower 0.4.13`, `hyper-util 0.1.20` in `Cargo.lock`), so ADR-003 promotes already-vetted, already-compiled crates to direct-dependency status rather than adding new code to the build graph (build-vs-buy.md §3b, added per architecture-review.md's process-gap Concern). |
| UDS rejection gRPC status code | `tonic::Code::PermissionDenied` | ux.md §4, this plan's own reasoning | `tonic::Code::Unauthenticated` (matching `BearerAuthInterceptor`) | `Unauthenticated` means "who you claim to be isn't verified"; a UDS peer-cred rejection is the opposite — identity *is* kernel-verified, access is simply not granted to it. `PermissionDenied` is the semantically correct code and lets client-side dispatch (`friendly_message`-style) distinguish the two rejection kinds by status code alone, not message-text parsing. |
| Client-side default-transport resolution | UDS-first, logged one-line TCP fallback, gated only on socket-path presence/reachability | ux.md §1, §3 | TTY-conditional branching, or an `SSH_AUTH_SOCK`-style pure-env-var discovery model | ux.md confirms zero TTY-branching precedent exists in this codebase and that introducing one would let a scripted context silently downgrade to the less-authenticated transport; a fixed, well-known default path (Docker's model) beats "someone else populates an env var" (ssh-agent's weaker model) for the zero-config success metric. |

---

## Migration Plan

N/A — no schema or persisted-record changes; nothing about
`tymux_core::PersistedLayoutNode`/session records changes shape. Two new
*ephemeral* on-disk artifacts are introduced (not migrated data, recreated
on every clean start): the UDS socket file itself, and its companion
`<path>.lock` file (ADR-001) — both are process-lifetime-scoped, not
persisted state.

## Observability Plan

- **Logs**:
  - `tracing::warn!` per rejected UDS connection — fields `peer_uid`,
    `peer_gid`, `tymux_socket_peercred_rejection_total` (the running
    count) — never any request content, mirroring
    `BearerAuthInterceptor`'s existing framing.
  - `tracing::warn!`, once at startup, for the new TCP-deprecation notice
    (fires whenever the TCP listener is about to be spawned, i.e.
    whenever `!tcp_disabled` — including the loopback case, since
    loopback TCP is exactly what's being deprecated).
  - `tracing::info!`, once at startup, extending the existing "tymuxd
    listening" line with `uds_path`.
  - `tracing::info!`, once at startup, when `--disable-tcp-loopback`/
    `TYMUXD_DISABLE_TCP_LOOPBACK` is set (states TCP was skipped
    entirely, not just deprecated).
- **Metrics**: `tymux_socket_peercred_rejection_total`, an
  `Arc<AtomicI64>` counter owned by `UdsPeerCredInterceptor`, incremented
  once per rejected UDS connection — same shape as
  `tymux_auth_rejection_total`.
- **Alerts**: none — this repo has no alerting infrastructure
  (matches `bearer-token-auth`'s own precedent); the counter/log exist
  for an operator to grep or dashboard manually.
- **Performance NFR**: no dedicated benchmark added. `peer_is_authorized`
  — including its `/proc/<pid>/status` read in the `--socket-group` case
  — runs exactly once per accepted connection, inside
  `PreAuthorizedUnixStream::new`, at accept time, before tonic's HTTP/2
  handshake begins (Epic 3.2). `UdsPeerCredInterceptor` itself does no
  decision work at all — it only reads the cached `UdsAuthDecision` bool
  tonic clones into request extensions on each RPC, the same
  per-request-clone mechanism `UdsConnectInfo` already uses for the raw
  credential. (**Gate-2 correction**: an earlier draft of this plan had
  the interceptor call `peer_is_authorized` itself from the raw `UCred`,
  which — because tonic clones connection-level extensions into every
  request, not just the first — would have re-run the decision, `/proc`
  read included, on every RPC; architecture-review.md caught this before
  implementation. The current design in Epic 3.2 is what ships.) With the
  fix, the "fixed-size, in-memory, O(1)-per-connection" reasoning
  `bearer-token-auth`'s plan already used to justify skipping a benchmark
  applies here without qualification.
- **Transient `accept()` errors on the UDS listener** (adversarial-review.md
  iteration-2 Concern, resolved by verification, no code change): Task
  4.2.2a's `.map_ok` only transforms `Ok` items from
  `UnixListenerStream`, so a transient `Err` item (e.g. `EMFILE`/`ENFILE`
  under fd exhaustion) passes through unchanged into
  `serve_with_incoming_shutdown`. Verified against the real tonic 0.12.3
  source (`~/.cargo/registry/.../tonic-0.12.3/src/transport/server/mod.rs:617-652`,
  the `loop { tokio::select! { ... io = incoming.next() => ... } }` inside
  `Server::serve_with_shutdown`, which `serve_with_incoming_shutdown`
  calls directly): the match arm for `Some(Err(e))` does `trace!("error
  accepting connection: {:#}", e); continue;` — it does **not** `break`
  the accept loop. Only a `None` item (the stream itself ending) breaks
  the loop; a `Some(Err(_))` item is skipped and the loop immediately
  polls `incoming.next()` again. So a transient `accept()` failure never
  tears down the UDS listener or falls back callers to the still-open TCP
  path — the concern's feared failure mode does not occur, and no
  `.filter_map`/retry wrapper is needed. The one residual gap: tonic logs
  the dropped error at `trace!`, a level this project's default filter
  does not surface (see this plan's existing `RUST_LOG` guidance), so an
  operator sees no signal today when this happens repeatedly (e.g.
  sustained fd exhaustion) — noted here as a documentation callout, not a
  design change, since it doesn't affect correctness or the "fail toward
  more restrictive" posture (the listener keeps accepting either way).

## Deployment Guidance

**The "both-by-default reads as fully isolated" risk is real and must be
stated in the same breath as the feature, not left implicit** (ux.md §2,
§5 — this project's single sharpest UX risk, distinct from and sharper
than `bearer-token-auth`'s own Deployment Guidance note about proxies).
Concretely:

- TCP loopback remains **fully unauthenticated** after this feature
  ships, by design, for the duration of this project's scope — only
  `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK=1` closes that
  gap (or the existing, unrelated non-loopback bearer-token path). An
  admin evaluating this feature for a shared host must be told this in
  the startup warning text itself (Task 4.3.1a), not only in a doc a
  reader might skip.
- A configured `--socket-group` grants members **full daemon control**
  over every session — `CreateSession` (arbitrary command execution),
  `Attach`/`CapturePane` (read/write any pane), `KillSession` — identical
  to the socket owner, **not** a scoped per-user subset (pitfalls.md
  §6's Docker-group cautionary tale). State this verbatim in
  `--socket-group`'s help text (Task 1.2.1a).
- A containerized/namespaced client connecting through a bind-mounted
  host socket sees its **host-mapped** uid at `peer_cred()` time, which
  may not match what `id -u` reports inside the container (pitfalls.md
  §4) — worth a one-line note near the peer-cred rejection error message
  (Task 6.3.1a), and now also documented operator-facing in the README's
  "Multi-user / shared-host deployment" section (Task 9.1.1a — added to
  close ux.md's Gap 1 / validation.md's S11-AC2, since a Rust doc comment
  alone is not operator-visible before this ships to a multi-user-host
  audience).
- On macOS/BSD, `--socket-group` only grants access to a connecting
  process whose **primary** group is the configured one — full
  supplementary-group support ships Linux-only in this project (ADR-002)
  — state this in the flag's help text too.

## Risk Control

- **Feature flag**: none needed for the UDS listener itself — strictly
  additive, matching `requirements.md`'s own framing. The new
  `--disable-tcp-loopback` flag *is itself* a feature-flag-shaped
  off-switch for the *old* TCP path, deliberately shipped now
  (architecture.md §6's "cheap now, expensive to retrofit later"
  argument) rather than invented under time pressure by a later removal
  project.
- **Rollback procedure**: revert the commit(s); no data migration to
  reverse (Migration Plan is N/A). Reverting only removes the UDS
  listener — TCP continues working exactly as it does today (the one
  path that predates this project entirely).
- **Staged rollout**: none needed. A bug in `peer_is_authorized` fails
  toward *more* restrictive (a legitimate same-user/same-group client
  gets rejected) rather than *less* (an unauthorized peer gets through),
  the safe failure direction for a security gate. A UDS-listener bind
  failure is fatal to the whole process by design (architecture.md §4)
  rather than silently degrading to TCP-only, so a misconfiguration is
  always loud, never a quiet downgrade.

## Unresolved Questions

- [ ] Full `getgrouplist`-based supplementary-group support on
      macOS/BSD — ADR-002 defers this; the primary-gid-only fallback
      ships instead for v1. Blocks: no story in this plan (accepted v1
      scope, not silently missing). Owner: whoever needs macOS
      group-parity, on request.
- [ ] Should `tymux-cli`'s TCP fallback address itself become
      configurable (today it's a hardcoded `http://127.0.0.1:7419`,
      matching the existing `--addr` default exactly)? Deliberately not
      added — unrelated scope-creep this feature doesn't need. Blocks:
      no story. Owner: N/A unless requested.
- [ ] Should the companion `<socket_path>.lock` file ever be deleted (vs.
      left on disk indefinitely across restarts)? Left in place
      deliberately — `flock` is advisory and releases automatically when
      the holding process exits or crashes, so a stale lock file with no
      process behind it is harmless; deleting it would open its own
      small delete-then-recreate TOCTOU for no benefit. Blocks: no
      story. Owner: N/A.
- [x] **Resolved during planning** (adversarial-review.md Concern fix —
      previously framed below as still needing confirmation; checked
      directly instead). The `tymux-cli`/`clients/go`/`clients/ts`
      integration tests proving the *reject* path against a genuinely
      different OS uid (not just a synthetic `UCred` in a unit test)
      require CI to run as root (or hold `CAP_SETUID`) per pitfalls.md
      §7. `.github/workflows/ci.yml` was checked directly: every job runs
      on plain `ubuntu-latest`/`macos-latest` GitHub-hosted runners, with
      no `container:` directive anywhere in the file — this repo's actual
      CI does **not** run as root. Tasks 6.4.1c (`tymux-cli`), 7.3.1b
      (`clients/go`), and 8.3.1c (`clients/ts`) therefore ship as
      `#[ignore]`/`t.Skip(...)`/skipped from day one on this repo's CI,
      not conditionally — the accept path stays fully integration-tested
      in all three clients, and the reject *decision* logic
      (`peer_is_authorized`) stays fully unit-tested (Story 3.1.2); only
      the true cross-uid, real-`peer_cred()`-delivered reject proof is
      unavailable in this repo's CI as configured today. No task changes
      are needed to *implement* this — each of the three tasks already
      specifies exactly this skip/fallback behavior; this entry only
      corrects the plan's own framing from "confirm before merging" (as
      if still open) to the checked fact above. Owner: N/A — running that
      one job in a root-capable container, if the true end-to-end proof
      is ever wanted, is a follow-up someone can pick up on request, not
      a blocker to this plan.

## Dependency Visualization

```
Phase 1: tymuxd config resolution (crates/tymuxd/src/auth.rs + main.rs wiring)
  Epic 1.1 socket-path resolution ─┐
  Epic 1.2 socket-group resolution ├──> feeds Phase 2 (bind) and Phase 3 (authz)
  Epic 1.3 tcp-disable resolution ─┘         feeds Phase 4 (dual-listener wiring)

Phase 2: TOCTOU-safe bind + stale-socket handling
  Epic 2.1 lock file + stale-socket reconciliation ──┐
  Epic 2.2 umask-bind-chown sequence <────────────────┘ (needs Phase 1's path+group)
                    │
                    └──> Phase 4 (needs a bound UnixListener to spawn the server on)

Phase 3: peer-cred authorization
  Epic 3.1 peer_is_authorized + group membership (needs Phase 1's group name via Epic 1.2's gid resolution) ──┐
  Epic 3.2 PreAuthorizedUnixStream (computes the decision once, at accept) + UdsPeerCredInterceptor <─────────┘
                    │
                    └──> Phase 4 (registered on the UDS listener's add_service)

Phase 4: TymuxDaemon Clone + dual-listener wiring + TCP deprecation
  Epic 4.1 TymuxDaemon: Clone (independent — can run any time before 4.2)
  Epic 4.2 dual-listener main() wiring <── needs Phase 2 (bound listener) + Phase 3 (interceptor)
  Epic 4.3 TCP-deprecation warning + --disable-tcp-loopback <── needs Epic 1.3
                                    + --socket-group startup caveat (Story 4.3.2) <── needs Epic 1.2

Phase 5: tymuxd integration tests — needs Phase 4 complete (a real dual-listener daemon to test against)
  Epic 5.1 accept/reject over real UDS (uid + group cases)
  Epic 5.2 stale-socket + lock-file races + concurrent dual-transport SIGTERM drain (Story 5.2.3)

Phase 9: Documentation — independent of every other phase (pure prose);
  naturally sequenced after Phase 6's --socket-path wiring and Task
  6.3.1a's PermissionDenied text so it can quote both verbatim.
  Epic 9.1 README "Multi-user / shared-host deployment" section

Genuine hard dependency on Phase 4 completing: Phase 5 (needs a live dual-listener
daemon), and every client phase's *integration* tests (6.4, 7.3, 8.3 — need a real
daemon to dial). Everything else below is parallelizable with Phase 4/5 once Phase 1's
socket-path algorithm is fixed (it's a pure spec, needed by every client immediately):

  Parallelizable with Phase 4/5, no live dual-listener daemon needed:
    Phase 6 tymux-cli: Epic 6.1 (socket-path/--addr), Epic 6.2 (UDS dial code),
      Epic 6.3 (error UX) — pure code, compiles/unit-tests without a running daemon
    Phase 7 clients/go: Epic 7.1 (udsdialer), Epic 7.2 (examples wiring)
    Phase 8 clients/ts: Epic 8.1 (UDS transport spike + impl), Epic 8.2 (path resolution)

  Blocked on a real dual-listener tymuxd (Phase 4 done):
    Epic 6.4 (tymux-cli integration tests)
    Epic 7.3 (Go integration tests)
    Epic 8.3 (TS integration tests)
```

---

## Phase 1: tymuxd — socket path, group, and TCP-disable configuration

### Epic 1.1: Default UDS socket path algorithm + `--socket-path` override
**Goal**: `tymuxd` resolves one deterministic, documented socket path with
correct flag/env/default precedence, identical to the algorithm every
client independently mirrors.

#### Story 1.1.1: `default_uds_socket_path` — the canonical algorithm
**As an** operator, **I want** `tymuxd` to pick a sensible, per-uid-scoped
default socket location with zero configuration, **so that** the UDS
listener "just works" on both Linux and macOS without me setting anything.
**Acceptance Criteria**:
- `$XDG_RUNTIME_DIR` set and non-empty wins, nested under a tymuxd-owned
  `tymuxd/` subdirectory — symmetric with the `/tmp` fallback branch's
  own `tymuxd-<uid>/` nesting below, so `bind_uds_listener` (Epic 2.2)
  never has to special-case which parent directory it's allowed to
  `chmod` (architecture-review.md Blocker fix: the un-nested
  `$XDG_RUNTIME_DIR/tymuxd.sock` form would make `bind_uds_listener`
  `chmod` the session manager's own shared runtime directory).
  - *Given* `XDG_RUNTIME_DIR=/run/user/1000` and uid `1000`, *When*
    `default_uds_socket_path(1000)` runs, *Then* it returns
    `/run/user/1000/tymuxd/tymuxd.sock`.
- `$XDG_RUNTIME_DIR` unset falls back to a uid-scoped path under
  `$TMPDIR`.
  - *Given* `XDG_RUNTIME_DIR` unset, `TMPDIR=/var/folders/xy/T` (a
    macOS-style value), and uid `1000`, *When*
    `default_uds_socket_path(1000)` runs, *Then* it returns
    `/var/folders/xy/T/tymuxd-1000/tymuxd.sock`.
- Both unset falls back to a uid-scoped path under `/tmp`.
  - *Given* neither `XDG_RUNTIME_DIR` nor `TMPDIR` set, and uid `1000`,
    *When* `default_uds_socket_path(1000)` runs, *Then* it returns
    `/tmp/tymuxd-1000/tymuxd.sock`.
- An empty-string env value is treated as unset (not honored literally).
  - *Given* `XDG_RUNTIME_DIR=""` and uid `1000`, *When*
    `default_uds_socket_path(1000)` runs, *Then* it does not return
    `/tymuxd/tymuxd.sock` — it falls through to the `$TMPDIR`/`/tmp` case
    as if `XDG_RUNTIME_DIR` were unset.
- Different uids never collide on the fallback path.
  - *Given* `XDG_RUNTIME_DIR` unset, `TMPDIR` unset, uid `1000` and uid
    `1001`, *When* `default_uds_socket_path` is called with each, *Then*
    it returns `/tmp/tymuxd-1000/tymuxd.sock` and
    `/tmp/tymuxd-1001/tymuxd.sock` respectively — distinct paths.
**Files**: `crates/tymuxd/src/auth.rs`,
`project_plans/unix-socket-auth/socket-path-fixtures.json` (shared
fixture file — see the schema note in Task 1.1.1b)

##### Task 1.1.1a: Implement `default_uds_socket_path` (~5 min)
- In `crates/tymuxd/src/auth.rs`:
  ```rust
  use std::path::PathBuf;

  /// The one documented default-socket-path algorithm this feature
  /// defines. Mirrored independently (not shared — see plan.md Pattern
  /// Decisions row 10) in tymux-cli's main.rs, clients/go's udsdialer
  /// package, and clients/ts's socket-path module. Any change here must
  /// be mirrored in all three.
  ///
  /// Both branches nest under a subdirectory tymuxd itself creates and
  /// owns (`tymuxd/` under $XDG_RUNTIME_DIR, `tymuxd-<uid>/` under the
  /// /tmp fallback) — deliberately symmetric, so bind_uds_listener's
  /// create_dir_all+chmod(0700) sequence (Epic 2.2) never has to
  /// special-case which directory it's touching (architecture-review.md
  /// Blocker fix: a bare `$XDG_RUNTIME_DIR/tymuxd.sock` would make
  /// bind_uds_listener chmod the session manager's own shared directory).
  pub fn default_uds_socket_path(uid: u32) -> PathBuf {
      if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
          return PathBuf::from(dir).join("tymuxd").join("tymuxd.sock");
      }
      let base = std::env::var_os("TMPDIR")
          .filter(|v| !v.is_empty())
          .map(PathBuf::from)
          .unwrap_or_else(|| PathBuf::from("/tmp"));
      base.join(format!("tymuxd-{uid}")).join("tymuxd.sock")
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.1.1b: Unit tests for the five ACs, reading the shared fixture file (~5 min)
- `default_uds_socket_path_prefers_xdg_runtime_dir`,
  `default_uds_socket_path_falls_back_to_tmpdir_when_xdg_unset`,
  `default_uds_socket_path_falls_back_to_tmp_when_both_unset`,
  `default_uds_socket_path_treats_empty_xdg_runtime_dir_as_unset`,
  `default_uds_socket_path_scopes_by_uid_to_avoid_collision`, using
  `ENV_LOCK` (Story 1.1.2 will introduce it; if this task lands first,
  add the lock here and reuse it below).
- **Read the canonical cases from
  `project_plans/unix-socket-auth/socket-path-fixtures.json` instead of
  hardcoding them inline** (architecture-review.md's test-duplication
  Concern fix — this is the one place among the four mirrored
  implementations that defines the shared fixture file; the other three
  read the same file). Create it now with this shape — a top-level
  object with one array per function, so `resolve_uds_socket_path`'s
  override cases (Task 1.1.2b) live in the same file rather than a
  second one:
  ```json
  {
    "default_path_cases": [
      {"case": "xdg_runtime_dir_set", "env": {"XDG_RUNTIME_DIR": "/run/user/1000"}, "uid": 1000, "expected": "/run/user/1000/tymuxd/tymuxd.sock"},
      {"case": "xdg_unset_tmpdir_set", "env": {"TMPDIR": "/var/folders/xy/T"}, "uid": 1000, "expected": "/var/folders/xy/T/tymuxd-1000/tymuxd.sock"},
      {"case": "both_unset", "env": {}, "uid": 1000, "expected": "/tmp/tymuxd-1000/tymuxd.sock"},
      {"case": "xdg_empty_string_treated_as_unset", "env": {"XDG_RUNTIME_DIR": ""}, "uid": 1000, "expected": "/tmp/tymuxd-1000/tymuxd.sock"},
      {"case": "uid_scoping_distinctness_1001", "env": {}, "uid": 1001, "expected": "/tmp/tymuxd-1001/tymuxd.sock"}
    ],
    "resolve_path_cases": [
      {"case": "flag_beats_env", "args": ["--socket-path", "/custom/tymuxd.sock"], "env": {"TYMUXD_SOCKET_PATH": "/other/tymuxd.sock"}, "uid": 1000, "expected": "/custom/tymuxd.sock"},
      {"case": "equals_joined_flag_form", "args": ["--socket-path=/custom/tymuxd.sock"], "env": {}, "uid": 1000, "expected": "/custom/tymuxd.sock"},
      {"case": "env_alone", "args": [], "env": {"TYMUXD_SOCKET_PATH": "/other/tymuxd.sock"}, "uid": 1000, "expected": "/other/tymuxd.sock"},
      {"case": "neither_present_falls_back_to_default", "args": [], "env": {}, "uid": 1000, "expected": "/tmp/tymuxd-1000/tymuxd.sock"}
    ]
  }
  ```
  Each of Rust (this task, and `resolve_path_cases` reused by Task
  1.1.2b; `default_path_cases` also reused by `tymux-cli`'s Task 6.1.1b
  — `tymux-cli`'s own `--socket-path` flag/env precedence is handled by
  `clap` directly, Task 6.1.1c, so it has no `resolve_path_cases`-shaped
  logic of its own to test against those cases), Go (Task 7.1.1b), and
  TS (Task 8.2.1b) test suites loads this same file at test
  time (`include_str!`/`serde_json` for Rust, `os.ReadFile`+
  `encoding/json` for Go, `fs.readFileSync`+`JSON.parse` for TS — all
  already-available stdlib/existing-dependency mechanisms, no new
  crate/package needed anywhere) rather than re-authoring the cases as a
  hand-typed table in each language. Any future change to the algorithm
  updates this one file plus the four (already-existing, not newly
  duplicated) test-runner functions that iterate it — not four
  independently-typed case tables. `default_uds_socket_path`'s own
  `XDG_RUNTIME_DIR`/`TMPDIR`/`/tmp` fallback env-var reads happen
  identically inside `resolve_uds_socket_path` when neither flag nor
  `TYMUXD_SOCKET_PATH` is set, so `resolve_path_cases`' last entry
  exercises `default_path_cases`' "both unset" case transitively — kept
  as separate arrays for readability, not because the algorithms
  diverge.
- Files: `crates/tymuxd/src/auth.rs`,
  `project_plans/unix-socket-auth/socket-path-fixtures.json` (new)

#### Story 1.1.2: `resolve_uds_socket_path` — `--socket-path`/`TYMUXD_SOCKET_PATH` override
**As an** operator, **I want** to override the default socket location
when I need to, **so that** unusual deployments (custom runtime dirs,
multiple `tymuxd` instances on one host) aren't stuck with the default.
**Acceptance Criteria**:
- Explicit `--socket-path` beats `TYMUXD_SOCKET_PATH` beats the default.
  - *Given* argv `["tymuxd", "--socket-path", "/custom/tymuxd.sock"]` and
    env `TYMUXD_SOCKET_PATH=/other/tymuxd.sock`, *When*
    `resolve_uds_socket_path(&args, 1000)` runs, *Then* it returns
    `/custom/tymuxd.sock`.
- `--socket-path=value` (`=`-joined) form works, matching `--token`'s
  precedent.
  - *Given* argv `["tymuxd", "--socket-path=/custom/tymuxd.sock"]` and no
    env var, *When* `resolve_uds_socket_path(&args, 1000)` runs, *Then*
    it returns `/custom/tymuxd.sock`.
- `TYMUXD_SOCKET_PATH` alone is used when no flag is passed.
  - *Given* argv `["tymuxd"]` and env
    `TYMUXD_SOCKET_PATH=/other/tymuxd.sock`, *When*
    `resolve_uds_socket_path(&args, 1000)` runs, *Then* it returns
    `/other/tymuxd.sock`.
- Neither source present falls back to `default_uds_socket_path`.
  - *Given* argv `["tymuxd"]`, no env var, uid `1000`, `XDG_RUNTIME_DIR`
    unset, `TMPDIR` unset, *When* `resolve_uds_socket_path(&args, 1000)`
    runs, *Then* it returns `/tmp/tymuxd-1000/tymuxd.sock`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 1.1.2a: Implement `resolve_uds_socket_path` (~5 min)
- ```rust
  /// Resolves the effective UDS socket path: `--socket-path`/
  /// `TYMUXD_SOCKET_PATH` (flag beats env, empty treated as absent) if
  /// set, else `default_uds_socket_path`. Note: prefer pointing the
  /// override at a `tymuxd`-owned subdirectory (e.g.
  /// `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock`, matching
  /// `default_uds_socket_path`'s own nesting) rather than directly at a
  /// shared runtime directory — this is a documentation nicety, not a
  /// safety requirement: `bind_uds_listener` (Epic 2.2) only
  /// creates/chmods a parent directory that doesn't already exist, so
  /// the socket binds safely either way.
  pub fn resolve_uds_socket_path(args: &[String], uid: u32) -> PathBuf {
      let flag = args.iter().position(|a| a == "--socket-path")
          .and_then(|i| args.get(i + 1)).cloned()
          .or_else(|| args.iter().find_map(|a| a.strip_prefix("--socket-path=").map(str::to_string)));
      let env = std::env::var("TYMUXD_SOCKET_PATH").ok().filter(|v| !v.is_empty());
      flag.or(env).map(PathBuf::from).unwrap_or_else(|| default_uds_socket_path(uid))
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.1.2b: Unit tests for the four ACs, reading the shared fixture file (~5 min)
- `resolve_uds_socket_path_prefers_flag_over_env`,
  `resolve_uds_socket_path_supports_equals_joined_flag_form`,
  `resolve_uds_socket_path_falls_back_to_env_var`,
  `resolve_uds_socket_path_falls_back_to_default_when_neither_set`,
  reusing `ENV_LOCK`. Reads the `resolve_path_cases` array from
  `project_plans/unix-socket-auth/socket-path-fixtures.json` (created by
  Task 1.1.1b) instead of hardcoding the four cases inline.
- Files: `crates/tymuxd/src/auth.rs`,
  `project_plans/unix-socket-auth/socket-path-fixtures.json`

### Epic 1.2: `--socket-group`/`TYMUXD_SOCKET_GROUP` resolution
**Goal**: An operator can name a POSIX group to relax the socket from
owner-only to group-accessible, with a loud failure on a typo'd/unknown
group name (never a silent no-op).

#### Story 1.2.1: `resolve_socket_group_name` + `resolve_gid_by_name`
**As an** operator, **I want** `--socket-group`/`TYMUXD_SOCKET_GROUP` to
resolve to a real gid or fail loudly, **so that** a typo doesn't silently
leave the socket owner-only when I believed I'd granted team access.
**Acceptance Criteria**:
- Explicit `--socket-group` beats `TYMUXD_SOCKET_GROUP`.
  - *Given* argv `["tymuxd", "--socket-group", "flag-group"]` and env
    `TYMUXD_SOCKET_GROUP=env-group`, *When*
    `resolve_socket_group_name(&args)` runs, *Then* it returns
    `Some("flag-group".to_string())`.
- Neither source present returns `None` (owner-only socket, today's
  behavior).
  - *Given* argv `["tymuxd"]` and no env var, *When*
    `resolve_socket_group_name(&args)` runs, *Then* it returns `None`.
- A real group name resolves to its gid.
  - *Given* the group `root` (gid `0` on every POSIX system, a safe
    always-present fixture for a portable test — see Task 1.2.1c),
    *When* `resolve_gid_by_name("root")` runs, *Then* it returns
    `Some(0)`.
- An unknown group name returns `None`, not a panic or a default gid.
  - *Given* a group name guaranteed not to exist
    (`"tymux-test-nonexistent-group-83f2"`), *When*
    `resolve_gid_by_name(name)` runs, *Then* it returns `None`.
**Files**: `crates/tymuxd/src/auth.rs`, `crates/tymuxd/Cargo.toml`
(confirm `libc` already present — it is)

##### Task 1.2.1a: Implement `resolve_socket_group_name` (~3 min)
- Same shape as `resolve_uds_socket_path`'s flag/env resolution
  (space-separated and `=`-joined flag forms, empty-is-absent).
  Doc-comment states verbatim (per Deployment Guidance): "Group members
  get FULL daemon control — CreateSession/Attach/KillSession against
  every session, identical to the socket owner, not a scoped subset."
  Doc-comment also cross-references the README's "Multi-user /
  shared-host deployment" section (Task 9.1.1a) for the
  containerized/bind-mounted-socket uid-mismatch caveat — relevant here
  too, since that caveat applies to any UDS connection, group-access or
  owner-only alike (ux.md Gap 1 fix).
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.2.1b: Implement `resolve_gid_by_name` (~5 min)
- ```rust
  /// Resolves a POSIX group name to its gid via getgrnam(3). Safe
  /// wrapper: getgrnam's returned pointer is into a non-thread-safe
  /// static buffer, but this is called exactly once, synchronously,
  /// during single-threaded daemon startup before any listener task is
  /// spawned (ADR-002 does not apply here — this is a distinct,
  /// well-scoped unsafe call already covered by tymuxd's existing libc
  /// dependency).
  pub fn resolve_gid_by_name(name: &str) -> Option<u32> {
      let cname = std::ffi::CString::new(name).ok()?;
      let grp = unsafe { libc::getgrnam(cname.as_ptr()) };
      if grp.is_null() { None } else { Some(unsafe { (*grp).gr_gid }) }
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.2.1c: Unit tests for both functions (~5 min)
- `resolve_socket_group_name_prefers_flag_over_env`,
  `resolve_socket_group_name_returns_none_when_unset`,
  `resolve_gid_by_name_resolves_root_to_gid_zero`,
  `resolve_gid_by_name_returns_none_for_unknown_group`.
- Files: `crates/tymuxd/src/auth.rs`

### Epic 1.3: `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK` resolution
**Goal**: An operator can opt into UDS-only operation today, so a future
removal project only needs to flip this flag's default (architecture.md
§6).

#### Story 1.3.1: `resolve_tcp_disabled`
**As an** operator who has fully migrated my own tooling to the UDS path,
**I want** to turn the TCP listener off entirely, **so that** I can
validate UDS-only operation before a future release removes TCP outright.
**Acceptance Criteria**:
- The bare `--disable-tcp-loopback` flag (no value) disables TCP.
  - *Given* argv `["tymuxd", "--disable-tcp-loopback"]`, *When*
    `resolve_tcp_disabled(&args)` runs, *Then* it returns `true`.
- A non-empty `TYMUXD_DISABLE_TCP_LOOPBACK` env value disables TCP.
  - *Given* argv `["tymuxd"]` and env
    `TYMUXD_DISABLE_TCP_LOOPBACK=1`, *When* `resolve_tcp_disabled(&args)`
    runs, *Then* it returns `true`.
- Neither present defaults to `false` (TCP stays on — today's behavior).
  - *Given* argv `["tymuxd"]` and no env var, *When*
    `resolve_tcp_disabled(&args)` runs, *Then* it returns `false`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 1.3.1a: Implement `resolve_tcp_disabled` (~3 min)
- ```rust
  pub fn resolve_tcp_disabled(args: &[String]) -> bool {
      args.iter().any(|a| a == "--disable-tcp-loopback")
          || std::env::var("TYMUXD_DISABLE_TCP_LOOPBACK")
              .map(|v| !v.is_empty())
              .unwrap_or(false)
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.3.1b: Unit tests for the three ACs (~3 min)
- `resolve_tcp_disabled_true_when_flag_present`,
  `resolve_tcp_disabled_true_when_env_nonempty`,
  `resolve_tcp_disabled_false_by_default`.
- Files: `crates/tymuxd/src/auth.rs`

---

## Phase 2: tymuxd — TOCTOU-safe bind + stale-socket handling

### Epic 2.1: Companion lock file + stale-socket reconciliation
**Goal**: Two concurrently starting `tymuxd` instances never both bind
the same socket path; a genuinely stale socket file from an unclean prior
exit is removed safely.

#### Story 2.1.1: `acquire_socket_lock`
**As** `tymuxd`, **I want** an exclusive, non-blocking lock before
touching the socket path at all, **so that** a second `tymuxd` racing to
start against the same path fails fast instead of silently corrupting the
first instance's listener.
**Acceptance Criteria**:
- The first caller acquires the lock successfully.
  - *Given* a fresh temp directory and socket path
    `<tmp>/tymuxd.sock`, *When* `acquire_socket_lock(&path)` runs,
    *Then* it returns `Ok(SocketLockGuard(_))` and
    `<tmp>/tymuxd.sock.lock` exists on disk.
- A second concurrent caller fails immediately (non-blocking).
  - *Given* the same path with `SocketLockGuard` from the prior AC still
    held (not dropped), *When* `acquire_socket_lock(&path)` runs again
    (same process, second call — the OS `flock` semantics this test
    proves are per-fd, not per-process, but a second open+lock attempt
    on the same underlying file within one process against an
    already-held `LOCK_EX` still returns `EWOULDBLOCK` per `flock(2)`),
    *Then* it returns `Err(_)` naming "another tymuxd is already
    starting against this socket" — not a hang.
- Dropping the guard releases the lock for a subsequent caller.
  - *Given* a `SocketLockGuard` acquired then dropped, *When*
    `acquire_socket_lock(&path)` is called again, *Then* it returns
    `Ok(_)`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 2.1.1a: Implement `SocketLockGuard`/`acquire_socket_lock` (~5 min)
- ```rust
  use std::fs::{File, OpenOptions};

  pub struct SocketLockGuard(#[allow(dead_code)] File);

  /// Held for tymuxd's entire process lifetime (ADR-001) — flock is
  /// released automatically on process exit/crash, so no explicit
  /// unlock/cleanup path is needed (see plan.md Unresolved Questions).
  pub fn acquire_socket_lock(socket_path: &std::path::Path) -> Result<SocketLockGuard, String> {
      let lock_path = socket_path.with_extension("sock.lock");
      let file = OpenOptions::new().create(true).write(true).open(&lock_path)
          .map_err(|e| format!("failed to open lock file {}: {e}", lock_path.display()))?;
      let ret = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) };
      if ret != 0 {
          return Err(format!(
              "another tymuxd is already starting against {} (lock file: {})",
              socket_path.display(), lock_path.display()
          ));
      }
      Ok(SocketLockGuard(file))
  }
  ```
  (`.with_extension("sock.lock")` on a path already ending `.sock`
  produces `<name>.sock.lock` — confirm this against the actual
  `default_uds_socket_path` output in the task's own test, since
  `Path::with_extension` replaces everything after the *last* `.`, not
  appends.)
- Files: `crates/tymuxd/src/auth.rs`

##### Task 2.1.1b: Unit tests for the three ACs (~5 min)
- `acquire_socket_lock_succeeds_for_first_caller`,
  `acquire_socket_lock_fails_fast_for_concurrent_second_caller`,
  `acquire_socket_lock_succeeds_again_after_guard_dropped`, each using
  `tempfile`-free `std::env::temp_dir().join(format!("tymux-lock-test-{}", Uuid::new_v4()))`
  (matching this crate's existing `uuid` dependency, no new test-only
  crate needed).
- Files: `crates/tymuxd/src/auth.rs`

#### Story 2.1.2: `reconcile_stale_socket`
**As** `tymuxd`, **I want** to distinguish a genuinely stale socket file
from a live daemon already listening there, **so that** an unclean prior
exit doesn't block the next start, while a real "already running"
situation fails loudly instead of stealing the socket.
**Acceptance Criteria**:
- No file at the path: no-op success.
  - *Given* a socket path with nothing on disk, *When*
    `reconcile_stale_socket(&path)` runs, *Then* it returns `Ok(())` and
    the path still doesn't exist.
- A stale socket file (nothing listening) is removed.
  - *Given* a socket file created by a `UnixListener` that has since been
    dropped (simulating an unclean exit — the file remains on disk with
    no live listener behind it), *When* `reconcile_stale_socket(&path)`
    runs, *Then* it returns `Ok(())` and the file no longer exists.
- A live listener at the path is left untouched and reported as an
  error.
  - *Given* a real `UnixListener` still bound and listening at the path,
    *When* `reconcile_stale_socket(&path)` runs, *Then* it returns
    `Err(_)` naming "tymuxd is already running" and the socket file is
    still present and still connectable.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 2.1.2a: Implement `reconcile_stale_socket` (~5 min)
- ```rust
  /// Only ever called while holding a SocketLockGuard for this same
  /// path (Story 2.1.1) — otherwise this check-then-act sequence is
  /// itself a TOCTOU across two concurrently starting daemons
  /// (pitfalls.md §2, ADR-001).
  pub fn reconcile_stale_socket(socket_path: &std::path::Path) -> Result<(), String> {
      if !socket_path.exists() {
          return Ok(());
      }
      match std::os::unix::net::UnixStream::connect(socket_path) {
          Ok(_) => Err(format!(
              "tymuxd is already running — a live listener answered at {}",
              socket_path.display()
          )),
          Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound) => {
              std::fs::remove_file(socket_path)
                  .map_err(|e| format!("failed to remove stale socket {}: {e}", socket_path.display()))
          }
          Err(e) => Err(format!("failed to probe existing socket {}: {e}", socket_path.display())),
      }
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 2.1.2b: Integration-style tests for the three ACs (~5 min)
- `reconcile_stale_socket_is_noop_when_nothing_at_path`,
  `reconcile_stale_socket_removes_a_genuinely_stale_file`
  (bind+drop a real `std::os::unix::net::UnixListener` at a temp path to
  produce a realistic stale file, matching pitfalls.md §2's exact
  scenario rather than a synthetic empty file),
  `reconcile_stale_socket_errs_and_leaves_a_live_listener_untouched`
  (bind a real listener, keep it alive across the call, assert it's
  still connectable afterward).
- Files: `crates/tymuxd/src/auth.rs`

### Epic 2.2: `bind_uds_listener` — the umask-based TOCTOU-safe bind
**Goal**: The socket file exists at its final permissions (`0600` or
`0660`) from the instant the kernel creates it — no window where it's
briefly world-accessible (ADR-001).

#### Story 2.2.1: `bind_uds_listener`
**As** `tymuxd`, **I want** the socket's mode set atomically with
creation via `umask`, and its group ownership set immediately after,
**so that** no local process can ever observe or connect to it at a wider
permission than intended, even briefly.
**Acceptance Criteria**:
- Owner-only (no group configured): the socket is created at mode
  `0600`.
  - *Given* a fresh parent directory and `group_gid = None`, *When*
    `bind_uds_listener(&path, None)` runs, *Then* the resulting file's
    mode (via `std::fs::metadata(&path).permissions().mode() & 0o777`)
    is exactly `0o600`.
- Group-configured: the socket is created at mode `0660` and group-owned
  by the configured gid.
  - *Given* the same setup with `group_gid = Some(<the test process's own
    real primary gid, from libc::getegid()>)`, *When*
    `bind_uds_listener(&path, Some(gid))` runs, *Then* the resulting
    file's mode is exactly `0o660` and its gid (via
    `std::os::unix::fs::MetadataExt::gid`) equals the configured gid.
- The parent directory is created at mode `0700` if it doesn't exist.
  - *Given* a socket path whose parent directory does not yet exist,
    *When* `bind_uds_listener(&path, None)` runs, *Then* the parent
    directory exists afterward with mode exactly `0o700`.
- The process umask is restored after binding (does not leak into
  subsequent code).
  - *Given* the process's umask before calling `bind_uds_listener`,
    *When* `bind_uds_listener(&path, None)` runs and returns, *Then* a
    probe of the current umask (e.g. `libc::umask(0o022)` then
    immediately `libc::umask(<the returned value>)` to restore it,
    asserting the returned value equals the pre-call umask) shows it
    matches the value from before the call.
- A pre-existing grandparent directory (standing in for
  `$XDG_RUNTIME_DIR` itself, which `tymuxd` does not own) is never
  touched — only the tymuxd-owned subdirectory directly containing the
  socket is created/`chmod`ed (architecture-review.md Blocker fix; see
  `default_uds_socket_path`'s nested-subdirectory design, Epic 1.1).
  - *Given* a temp directory standing in for `$XDG_RUNTIME_DIR`, already
    existing at mode `0o755` (an arbitrary mode distinct from `0o700`,
    simulating a session-manager-owned directory `tymuxd` does not own),
    and a socket path `<that dir>/tymuxd/tymuxd.sock` nested one level
    beneath it, *When* `bind_uds_listener(&path, None)` runs, *Then* the
    outer directory's mode is unchanged (still exactly `0o755`) while
    `<that dir>/tymuxd` is freshly created at exactly `0o700` —
    `bind_uds_listener` never `chmod`s a directory it doesn't itself
    create.
- A pre-existing *immediate parent* directory (e.g. an *un-nested*
  `--socket-path`/`TYMUXD_SOCKET_PATH` override pointed straight at a
  pre-existing directory rather than a `tymuxd`-owned subdirectory) is
  now **validated, not trusted** (pre-mortem.md P1 #2 fix — the prior
  design skipped create+chmod and "trusted" the pre-existing directory's
  permissions unconditionally, which on the `/tmp`-fallback path let an
  attacker who pre-creates `/tmp/tymuxd-<uid>/` before the daemon's first
  real start keep control of the socket's directory). The four bullets
  below cover accept and reject for both the owner-only and group-access
  cases.
  - *Given* a temp directory already existing, owned by the calling
    process's own uid, at mode exactly `0o700` (the same mode a freshly
    created parent gets, `group_gid = None`), and a socket path directly
    inside it, *When* `bind_uds_listener(&path, None)` runs, *Then* the
    directory's mode is unchanged (still exactly `0o700`, never
    `chmod`ed) and the socket file is created successfully inside it —
    the safety property from architecture-review.md's iteration-2
    Blocker fix (never `chmod` a directory `tymuxd` doesn't itself
    create) still holds, now conditioned on the directory actually being
    safe rather than unconditionally.
- A pre-existing immediate parent at the group-access case's own
  expected mode is likewise accepted unchanged.
  - *Given* the same setup but `group_gid = Some(<the test process's own
    real gid>)` and the pre-existing directory at mode `0o750` (the
    group-access-case equivalent of `0o700` — owner rwx, group r-x,
    enough for a group member's process to traverse into the directory
    and reach the `0o660` socket file by name, without granting the
    group write access to the directory's contents), *When*
    `bind_uds_listener(&path, Some(gid))` runs, *Then* it is accepted
    unchanged and the socket binds successfully.
- A pre-existing parent directory owned by a different uid than the
  daemon's own is a fatal error, never a silent bind into it — the
  headline regression this fix closes.
  - *Given* a pre-existing parent directory owned by a **different**
    uid than the calling process (world-writable — mode `0o777` — to
    simulate an attacker-planted directory on the `/tmp` fallback path),
    *When* `bind_uds_listener(&path, None)` runs, *Then* it returns
    `Err(_)` — a fatal, actionable error naming the directory and the
    ownership/permission mismatch — and never calls `UnixListener::bind`
    into it.
- Correct ownership alone is not sufficient — a too-permissive mode on an
  otherwise-correctly-owned pre-existing parent is fatal too.
  - *Given* a pre-existing parent directory owned by the calling
    process's own uid but at a mode wider than expected for the
    configured case (e.g. `0o755` with `group_gid = None`, where `0o700`
    is required), *When* `bind_uds_listener(&path, None)` runs, *Then* it
    also returns `Err(_)` with the same actionable error.
- A group `chown` failing with `EPERM` (the daemon process is not a
  member of the configured group, and the calling user lacks permission
  to `chown` to it) produces a distinct, actionable error naming the
  daemon's own group membership as the problem — not the generic
  bind-failure message (architecture-review.md/adversarial-review.md
  Concern fix).
  - *Given* a fresh parent directory and `group_gid = Some(0)` (`root`'s
    gid — a group the test process, run as non-root, is never a member
    of; skip this test if the test process happens to run as root or is
    already a member of gid 0, mirroring this plan's existing
    CI-privilege-guard pattern), *When* `bind_uds_listener(&path,
    Some(0))` runs, *Then* it returns `Err(_)` whose message names the
    daemon's own group membership as the problem (e.g. contains "is not
    a member of") rather than "Check that the parent directory exists
    and is writable" — and the socket file itself was still successfully
    created (only the `chown` step failed).
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 2.2.1a: Implement `bind_uds_listener` (~5 min)
- Returns `Result<_, String>` (not `io::Result`) so the `chown`-`EPERM`
  case can carry a distinct, pre-formatted message — matching
  `acquire_socket_lock`/`reconcile_stale_socket`'s existing
  `Result<_, String>` shape in this same file:
  ```rust
  use std::os::unix::fs::PermissionsExt;

  pub fn bind_uds_listener(
      socket_path: &std::path::Path,
      group_gid: Option<u32>,
  ) -> Result<tokio::net::UnixListener, String> {
      let fail_bind = |e: std::io::Error| format!(
          "failed to create Unix socket at {}: {e}. Check that the parent directory \
           exists and is writable, or override the path with --socket-path/TYMUXD_SOCKET_PATH.",
          socket_path.display()
      );
      // Directory mode this function requires for an *immediate* parent
      // of the socket, in both the fresh-create and pre-existing cases:
      // 0o700 (owner rwx only) when no group is configured, or 0o750
      // (owner rwx, group r-x — enough for a group member's process to
      // traverse into the directory and reach the 0o660 socket file by
      // name, without granting the group write access to the
      // directory's contents) when group_gid is set.
      let expected_parent_mode = if group_gid.is_some() { 0o750 } else { 0o700 };
      if let Some(parent) = socket_path.parent() {
          // The default algorithm (default_uds_socket_path, Epic 1.1)
          // always nests one level under a tymuxd-owned subdirectory
          // (`tymuxd/` or `tymuxd-<uid>/`), so in the default case this
          // parent never pre-exists on first run and always gets
          // created+chmod'd here. But `resolve_uds_socket_path` (Task
          // 1.1.2a) applies no validation to an operator-supplied
          // `--socket-path`/`TYMUXD_SOCKET_PATH` override, so this
          // function cannot assume its input is nested — an override
          // could point straight at a pre-existing, shared directory
          // (e.g. a bare $XDG_RUNTIME_DIR). "Never chmod a directory
          // tymuxd doesn't itself own" stays an invariant of this
          // function for *any* input (architecture-review.md
          // iteration-2 Blocker fix) — but a pre-existing parent is now
          // VALIDATED, not silently trusted (pre-mortem.md P1 #2 fix: on
          // the /tmp fallback path an attacker could otherwise
          // pre-create the parent directory before the daemon's first
          // real start and keep control of it). Fatal, not a silent
          // bind-into-it, if the pre-existing directory isn't owned by
          // this process's own uid at exactly the expected mode.
          if parent.exists() {
              let meta = std::fs::symlink_metadata(parent).map_err(fail_bind)?;
              let owner_uid = std::os::unix::fs::MetadataExt::uid(&meta);
              let mode = meta.permissions().mode() & 0o777;
              let daemon_uid = unsafe { libc::geteuid() };
              if owner_uid != daemon_uid || mode != expected_parent_mode {
                  return Err(format!(
                      "refusing to bind Unix socket at {}: its parent directory {} already \
                       exists but is owned by uid {owner_uid} at mode {mode:o} (expected uid \
                       {daemon_uid} at mode {expected_parent_mode:o}). A pre-existing socket \
                       directory not owned and permissioned by tymuxd itself may have been \
                       created by another, possibly untrusted, process — remove it or point \
                       --socket-path/TYMUXD_SOCKET_PATH somewhere tymuxd can create fresh.",
                      socket_path.display(), parent.display()
                  ));
              }
          } else {
              std::fs::create_dir_all(parent).map_err(fail_bind)?;
              std::fs::set_permissions(parent, std::fs::Permissions::from_mode(expected_parent_mode))
                  .map_err(fail_bind)?;
          }
      }
      // 0o177 -> 0777 & ~0177 = 0600 (owner-only); 0o117 -> 0777 & ~0117
      // = 0660 (owner+group). Set immediately before bind() so the
      // kernel creates the file already at this mode — no post-bind
      // chmod window (ADR-001; fchmod on the fd is a documented no-op
      // for AF_UNIX on Linux, so this is the only atomic option).
      let new_umask = if group_gid.is_some() { 0o117 } else { 0o177 };
      let old_umask = unsafe { libc::umask(new_umask) };
      let bind_result = tokio::net::UnixListener::bind(socket_path);
      unsafe { libc::umask(old_umask) };
      let listener = bind_result.map_err(fail_bind)?;
      if let Some(gid) = group_gid {
          std::os::unix::fs::chown(socket_path, None, Some(gid)).map_err(|e| {
              if e.raw_os_error() == Some(libc::EPERM) {
                  format!(
                      "bound the Unix socket at {} but failed to grant group access \
                       (gid {gid}): Operation not permitted. The tymuxd process itself is not \
                       a member of the configured --socket-group/TYMUXD_SOCKET_GROUP — add the \
                       daemon's own OS user to that group (or run tymuxd as a user already in \
                       it), then restart.",
                      socket_path.display()
                  )
              } else {
                  format!(
                      "bound the Unix socket at {} but failed to set its group ownership \
                       (gid {gid}): {e}",
                      socket_path.display()
                  )
              }
          })?;
      }
      Ok(listener)
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 2.2.1b: Tests for the ten ACs (~5 min)
- `bind_uds_listener_creates_owner_only_socket_at_mode_0600`,
  `bind_uds_listener_creates_group_socket_at_mode_0660_with_configured_gid`
  (uses `unsafe { libc::getegid() }` for a real, always-valid gid the
  test process itself belongs to),
  `bind_uds_listener_creates_parent_directory_at_mode_0700`,
  `bind_uds_listener_restores_process_umask_after_binding`,
  `bind_uds_listener_never_touches_permissions_of_a_pre_existing_grandparent_directory`,
  `bind_uds_listener_accepts_a_correctly_owned_and_moded_pre_existing_immediate_parent_at_0700`
  (the un-nested-override accept case — a distinct test from the
  grandparent one above: here the socket path's *immediate* parent is
  the pre-existing, already-compliant directory, not a directory two
  levels up; owned by the test process's own uid, at exactly `0o700`,
  and left unchanged),
  `bind_uds_listener_accepts_a_correctly_owned_and_moded_pre_existing_immediate_parent_at_0750_with_group_configured`
  (the group-access variant of the test above, at mode `0o750`),
  `bind_uds_listener_fails_loudly_when_pre_existing_parent_is_owned_by_a_different_uid`
  (pre-mortem.md P1 #2's headline regression test: a world-writable,
  different-uid-owned pre-existing parent directory must cause a fatal
  error, never a bind into it — the TOCTOU this fix closes),
  `bind_uds_listener_fails_loudly_when_pre_existing_parent_has_a_too_permissive_mode`
  (same-uid, but mode `0o755` where `0o700` is required — proves
  ownership alone isn't sufficient),
  `bind_uds_listener_returns_distinct_message_when_chown_group_permission_denied`
  (skipped when `unsafe { libc::geteuid() } == 0` or the test process is
  already a member of gid `0`, matching this plan's existing
  CI-privilege-guard convention — see Tasks 6.4.1c/7.3.1b/8.3.1c). All
  tests use a fresh `std::env::temp_dir().join(format!("tymux-bind-test-{}", Uuid::new_v4()))`
  per test to avoid cross-test interference, and must run serialized
  against `ENV_LOCK`-style guarding since `umask` is process-global
  exactly like the existing env-var tests already guard against
  (document this explicitly — a new `UMASK_LOCK: Mutex<()>` alongside
  `ENV_LOCK`, since umask mutation races are a distinct hazard from env
  var races and both can't safely interleave with either kind of test).
  The different-uid test can't create a directory it doesn't own as the
  test process itself; simulate it with a directory the test process
  does own but whose mode is set to appear attacker-controlled (e.g.
  `0o777`) *and* assert the ownership-mismatch branch separately via a
  unit-level check on the comparison logic if spawning a genuinely
  different-uid-owned fixture isn't practical outside CI root — confirm
  the simplest sufficient approach at implementation time.
- Files: `crates/tymuxd/src/auth.rs`

---

## Phase 3: tymuxd — peer-cred authorization

### Epic 3.1: `peer_is_authorized` + Linux supplementary-group membership
**Goal**: A pure, fully unit-testable decision function determines
whether a peer's kernel-verified uid/gid grants access — the daemon's own
uid always passes; a configured group additionally grants access to any
member (full supplementary-group list on Linux, primary gid only
elsewhere — ADR-002).

#### Story 3.1.1: `peer_is_group_member` — Linux `/proc`-based check
**As** `tymuxd` on Linux, **I want** to check a peer's *full* group list
(primary and supplementary), **so that** a teammate added to the
configured group via the normal `usermod -aG` workflow is actually
granted access, not just someone whose *primary* group happens to match.
**Acceptance Criteria**:
- The current test process's own real gid (from `libc::getegid()`) is
  found as a member when checked against its own real pid.
  - *Given* `pid = std::process::id()` (the test process's own pid,
    guaranteed to have a real `/proc/<pid>/status`) and
    `gid = unsafe { libc::getegid() }` (the test process's own real
    effective gid, guaranteed present in its own group list), *When*
    `peer_is_group_member_linux(pid, gid)` runs, *Then* it returns
    `true`.
- A gid the test process definitely does not belong to is not found.
  - *Given* the same `pid` and a gid guaranteed absent (e.g. `999999`,
    astronomically unlikely to be assigned on any real system, or more
    robustly, one explicitly excluded by first reading the process's own
    real `Groups:` line and picking a value not present in it), *When*
    `peer_is_group_member_linux(pid, absent_gid)` runs, *Then* it
    returns `false`.
- A nonexistent pid degrades to `false` rather than panicking (the
  `/proc` read fails, which per ADR-002's fallback story should not be
  treated as "found" — see Story 3.1.2's composition test for how this
  interacts with the primary-gid fallback at the `peer_is_authorized`
  level).
  - *Given* `pid = 999999999` (astronomically unlikely to be a live
    process), *When* `peer_is_group_member_linux(pid, <any gid>)` runs,
    *Then* it returns `false` without panicking.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 3.1.1a: Implement `/proc`-based group parsing (~5 min)
- ```rust
  #[cfg(target_os = "linux")]
  fn peer_is_group_member_linux(pid: i32, gid: u32) -> bool {
      let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
          return false;
      };
      status
          .lines()
          .find_map(|line| line.strip_prefix("Groups:"))
          .map(|groups| groups.split_whitespace().filter_map(|g| g.parse::<u32>().ok()).any(|g| g == gid))
          .unwrap_or(false)
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 3.1.1b: Unit tests for the three ACs (~5 min)
- `peer_is_group_member_linux_finds_own_real_gid_via_own_pid`,
  `peer_is_group_member_linux_does_not_find_an_absent_gid`,
  `peer_is_group_member_linux_returns_false_for_nonexistent_pid`,
  `#[cfg(target_os = "linux")]`-gated (skipped, not failed, on non-Linux
  CI runners — matching this repo's existing cross-platform test
  conventions).
- Files: `crates/tymuxd/src/auth.rs`

#### Story 3.1.2: `peer_is_group_member` (platform dispatch) + `peer_is_authorized`
**As** `tymuxd` on any supported platform, **I want** one uniform
authorization decision function, **so that** `UdsPeerCredInterceptor`
(Epic 3.3) never has to know which platform it's running on.

This story also introduces `PeerIdentity`, a small value object owned by
this module (architecture-review.md's primitive-obsession/DIP Concern
fix): `peer_is_authorized`/`peer_is_group_member` take a `&PeerIdentity`
instead of tokio's concrete `&UCred` directly, and `uid`/`gid` are named
struct fields rather than same-typed positional `u32` parameters.
`PeerIdentity` is constructed exactly once per accepted connection, from
`UCred`, at `PreAuthorizedUnixStream::new` (Epic 3.2) — never
anything client-supplied.
**Acceptance Criteria**:
- The daemon's own uid is always authorized, group configured or not.
  - *Given* `daemon_uid = 1000`, `allowed_gid = None`, and
    `peer = PeerIdentity { uid: 1000, gid: <any>, pid: <any> }`, *When*
    `peer_is_authorized(1000, None, &peer)` runs, *Then* it returns
    `true`.
- A different uid with no group configured is rejected.
  - *Given* `daemon_uid = 1000`, `allowed_gid = None`, peer `uid = 1001`,
    *When* `peer_is_authorized(1000, None, &peer)` runs, *Then* it
    returns `false`.
- A different uid whose group membership matches the configured gid is
  authorized.
  - *Given* `daemon_uid = 1000`, `allowed_gid = Some(5000)`, peer
    `uid = 1002`, and `peer_is_group_member` (platform-dispatched) would
    return `true` for `(peer, 5000)`, *When* `peer_is_authorized(1000,
    Some(5000), &peer)` runs, *Then* it returns `true`.
- A different uid whose group membership does not match is rejected.
  - *Given* the same setup but `peer_is_group_member` returns `false`,
    *When* `peer_is_authorized(1000, Some(5000), &peer)` runs, *Then* it
    returns `false`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 3.1.2a: Implement `PeerIdentity` + the platform dispatch + `peer_is_authorized` (~5 min)
- ```rust
  /// Decouples peer_is_authorized/peer_is_group_member from tokio's
  /// concrete UCred type and gives "a uid" vs. "a gid" distinct field
  /// names instead of same-primitive, positional u32 parameters
  /// (architecture-review.md's primitive-obsession/DIP Concern fix).
  /// Constructed once, from UCred, at the point a UDS connection is
  /// accepted (PreAuthorizedUnixStream::new, Epic 3.2) — never
  /// anything client-supplied.
  #[derive(Clone, Copy, Debug)]
  pub struct PeerIdentity {
      pub uid: u32,
      pub gid: u32,
      pub pid: Option<i32>,
  }

  impl From<&tokio::net::unix::UCred> for PeerIdentity {
      fn from(cred: &tokio::net::unix::UCred) -> Self {
          Self { uid: cred.uid(), gid: cred.gid(), pid: cred.pid() }
      }
  }

  fn peer_is_group_member(peer: &PeerIdentity, gid: u32) -> bool {
      #[cfg(target_os = "linux")]
      {
          if let Some(pid) = peer.pid {
              return peer_is_group_member_linux(pid, gid);
          }
      }
      // macOS/BSD, or Linux with an unreadable/absent pid: primary/
      // effective gid only (ADR-002's documented, narrower-not-less-
      // safe fallback).
      peer.gid == gid
  }

  /// The kernel-verified authorization decision — never consults
  /// anything client-supplied (requirements.md's NFR). `daemon_uid` is
  /// tymuxd's own effective uid (libc::geteuid(), read once at
  /// startup); `peer` is constructed from tonic's UdsConnectInfo/UCred,
  /// populated by SO_PEERCRED at accept time.
  pub fn peer_is_authorized(
      daemon_uid: u32,
      allowed_gid: Option<u32>,
      peer: &PeerIdentity,
  ) -> bool {
      if peer.uid == daemon_uid {
          return true;
      }
      allowed_gid.is_some_and(|gid| peer_is_group_member(peer, gid))
  }
  ```
  (No non-Linux stub for `peer_is_group_member_linux` — its only call
  site above is itself `#[cfg(target_os = "linux")]`-gated, so a
  non-Linux `unreachable!()` variant would be genuine dead code;
  architecture-review.md nitpick fix.)
- Files: `crates/tymuxd/src/auth.rs`

##### Task 3.1.2b: Unit tests for the four ACs (~5 min)
- Construct a `tokio::net::unix::UCred` for tests via
  `std::os::unix::net::UnixStream::pair()` + `.peer_cred()` on one end (a
  real, portable way to get a genuine `UCred` value in a unit test
  without any process/pid mocking — both ends of a `pair()` report the
  *test process's own* uid/gid/pid), then build `PeerIdentity::from(&cred)`
  and override its `uid`/`gid` fields directly for whichever branch a
  given test drives (an ordinary struct-update, since `PeerIdentity` is
  this module's own type — no `UCred` mocking limitation applies here
  the way it did before this story's fix). The group-membership branch
  is tested via `peer_is_group_member`'s own unit tests from Story 3.1.1,
  so `peer_is_authorized`'s tests can stub group membership by picking
  `allowed_gid` as the test process's own real primary gid (always a
  member of itself) vs. a gid guaranteed absent from its own group list.
- `peer_is_authorized_grants_daemon_own_uid_always`,
  `peer_is_authorized_rejects_different_uid_no_group_configured`,
  `peer_is_authorized_grants_different_uid_in_configured_group`,
  `peer_is_authorized_rejects_different_uid_not_in_configured_group`.
- Files: `crates/tymuxd/src/auth.rs`

### Epic 3.2: `PreAuthorizedUnixStream` + `UdsPeerCredInterceptor`
**Goal**: Every UDS-listener RPC is gated by the authorization decision,
with rejection logging/counting matching `BearerAuthInterceptor`'s
established shape — and, per architecture-review.md's Performance
Concern fix, that decision is computed **exactly once per accepted
connection**, not re-derived on every RPC (requirements.md's Performance
SLO: "peer-credential check happens once per connection (at accept
time), not per-RPC").

`tonic::service::Interceptor` is a per-*request* extension point:
`connect_info()` runs once per connection, but tonic clones its result
into `request.extensions_mut()` on every request on that connection
(`tonic-0.12.3/src/transport/server/mod.rs:1038-1042`, per
architecture.md §2's own citation). An interceptor that computed
`peer_is_authorized` itself from the raw `UCred` on every `call()` would
therefore re-run the decision — including the `/proc/<pid>/status`
supplementary-group read in the `--socket-group` case — per RPC, not per
connection (this was the plan's original, incorrect design; caught in
Gate-2 review). The fix moves the decision itself, not just the raw
credential, into the once-per-connection slot: `PreAuthorizedUnixStream`
wraps each accepted `UnixStream`, computes `UdsAuthDecision` once at
accept time (before tonic's HTTP/2 handshake even begins), and supplies
that decision as its own `Connected::ConnectInfo` type — so it's what
gets cloned into extensions per-request, and `UdsPeerCredInterceptor`
becomes a pure "read the cached bool" check with no decision logic of
its own.

#### Story 3.2.1a: `PreAuthorizedUnixStream` — compute the decision once, at accept
**As** `tymuxd`, **I want** the authorization decision computed exactly
once when a UDS connection is accepted, **so that** no per-RPC work
(including the `/proc` read) happens on a connection that already made
its access decision at accept time.
**Acceptance Criteria**:
- Wrapping an accepted stream computes the decision immediately and
  makes it available via `Connected::connect_info()`.
  - *Given* a real `UnixStream::pair()` (test process's own uid/gid on
    both ends) and `PreAuthorizedUnixStream::new(stream, daemon_uid, None)`
    with `daemon_uid` equal to the test process's own real uid, *When*
    `.connect_info()` is called on the wrapper, *Then* it returns
    `UdsAuthDecision { authorized: true, peer_uid: Some(<real uid>),
    peer_gid: Some(<real gid>) }`.
- A mismatched `daemon_uid` at construction time produces a cached
  `authorized: false` decision.
  - *Given* the same setup but `daemon_uid` deliberately does not match
    the test process's real uid, *When* `.connect_info()` is called,
    *Then* it returns `UdsAuthDecision { authorized: false, .. }`.
- The wrapper still functions as a transport (reads/writes pass through
  to the inner stream unchanged) — this is a transparent wrapper, not a
  behavior change to the connection itself.
  - *Given* a `PreAuthorizedUnixStream`-wrapped end of a real
    `UnixStream::pair()`, *When* bytes are written to the wrapper and
    read from the raw peer end (or vice versa), *Then* they round-trip
    correctly, proving `AsyncRead`/`AsyncWrite` delegate to the inner
    stream with no buffering/framing change.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 3.2.1a-impl: Implement `UdsAuthDecision` + `PreAuthorizedUnixStream` (~5 min)
- ```rust
  /// The authorization decision, computed exactly once per accepted UDS
  /// connection — not once per RPC (architecture-review.md Performance
  /// Concern fix). Copy, cloned into request extensions on every RPC on
  /// that connection by tonic's own per-request extension-cloning
  /// (mod.rs:1038-1042) — but this carries the *decision*, not the raw
  /// UCred, so peer_is_authorized (including its /proc read in the
  /// --socket-group case) never re-runs per request.
  #[derive(Clone, Copy, Debug)]
  pub struct UdsAuthDecision {
      pub authorized: bool,
      pub peer_uid: Option<u32>,
      pub peer_gid: Option<u32>,
  }

  /// Wraps an accepted UnixStream with its authorization decision,
  /// computed once here — at accept time, before the stream enters
  /// tonic's HTTP/2 handshake. Implements Connected so tonic's own
  /// per-request extension-cloning carries UdsAuthDecision instead of a
  /// raw credential a downstream Interceptor would otherwise have to
  /// re-derive a decision from.
  pub struct PreAuthorizedUnixStream {
      inner: tokio::net::UnixStream,
      decision: UdsAuthDecision,
  }

  impl PreAuthorizedUnixStream {
      pub fn new(inner: tokio::net::UnixStream, daemon_uid: u32, allowed_gid: Option<u32>) -> Self {
          let cred = inner.peer_cred().ok();
          let decision = UdsAuthDecision {
              authorized: cred
                  .as_ref()
                  .is_some_and(|c| peer_is_authorized(daemon_uid, allowed_gid, &PeerIdentity::from(c))),
              peer_uid: cred.as_ref().map(|c| c.uid()),
              peer_gid: cred.as_ref().map(|c| c.gid()),
          };
          Self { inner, decision }
      }
  }

  // UnixStream: Unpin, and `decision` is Copy (Unpin), so
  // PreAuthorizedUnixStream is Unpin too — no pin-project needed, plain
  // Pin::new(&mut self.get_mut().inner) delegation is sound.
  impl tokio::io::AsyncRead for PreAuthorizedUnixStream {
      fn poll_read(
          self: std::pin::Pin<&mut Self>,
          cx: &mut std::task::Context<'_>,
          buf: &mut tokio::io::ReadBuf<'_>,
      ) -> std::task::Poll<std::io::Result<()>> {
          std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
      }
  }

  impl tokio::io::AsyncWrite for PreAuthorizedUnixStream {
      fn poll_write(
          self: std::pin::Pin<&mut Self>,
          cx: &mut std::task::Context<'_>,
          buf: &[u8],
      ) -> std::task::Poll<std::io::Result<usize>> {
          std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
      }
      fn poll_flush(
          self: std::pin::Pin<&mut Self>,
          cx: &mut std::task::Context<'_>,
      ) -> std::task::Poll<std::io::Result<()>> {
          std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
      }
      fn poll_shutdown(
          self: std::pin::Pin<&mut Self>,
          cx: &mut std::task::Context<'_>,
      ) -> std::task::Poll<std::io::Result<()>> {
          std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
      }
  }

  impl tonic::transport::server::Connected for PreAuthorizedUnixStream {
      type ConnectInfo = UdsAuthDecision;
      fn connect_info(&self) -> Self::ConnectInfo {
          self.decision
      }
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 3.2.1a-test: Unit tests for the three ACs (~5 min)
- `pre_authorized_unix_stream_caches_authorized_decision_at_construction`,
  `pre_authorized_unix_stream_caches_unauthorized_decision_when_uid_mismatched`,
  `pre_authorized_unix_stream_passes_reads_and_writes_through_to_inner_stream`,
  built against a real `tokio::net::UnixStream::pair()`.
- Files: `crates/tymuxd/src/auth.rs`

#### Story 3.2.1b: `UdsPeerCredInterceptor` — reads the cached decision
**As** `tymuxd`, **I want** the per-RPC interceptor to be a pure
"read the cached decision" check, **so that** no authorization logic
(and no `/proc` read) ever runs more than once per connection.
**Acceptance Criteria**:
- An authorized connection's RPCs are accepted unchanged.
  - *Given* `UdsPeerCredInterceptor::new(counter)` and a `Request<()>`
    with `UdsAuthDecision { authorized: true, .. }` inserted into its
    extensions, *When* `interceptor.call(req)` runs, *Then* it returns
    `Ok(req)` and `counter` is unchanged.
- An unauthorized connection's RPCs are rejected with `PermissionDenied`
  and the counter increments.
  - *Given* the same interceptor and a `Request<()>` with
    `UdsAuthDecision { authorized: false, peer_uid: Some(1001), peer_gid: Some(1001) }`,
    *When* `interceptor.call(req)` runs, *Then* it returns
    `Err(Status::permission_denied("not authorized to access this
    daemon's socket"))` and `counter` increments by 1.
- Missing `UdsAuthDecision` entirely (should never happen in production —
  `PreAuthorizedUnixStream` always supplies one for a `UnixStream`-sourced
  connection — but must fail closed, not panic) is rejected.
  - *Given* the same interceptor and a bare `Request::new(())` with no
    `UdsAuthDecision` extension inserted, *When* `interceptor.call(req)`
    runs, *Then* it returns `Err(Status::permission_denied(...))` (same
    message) and `counter` increments by 1 — never a panic.
- The rejection log contains the peer's uid/gid, never any request
  content.
  - *Given* the unauthorized case above, *When* `interceptor.call(req)`
    runs, *Then* a `tracing::warn!` record is emitted containing
    `peer_uid=1001` and `tymux_socket_peercred_rejection_total`, and the
    log output contains no session/pane identifiers.
- The decision is never recomputed across multiple RPCs on the same
  connection (the property this whole redesign exists to guarantee).
  - *Given* two `Request<()>`s built with the *same* `UdsAuthDecision`
    value inserted independently into each (simulating tonic's own
    per-request clone of one connection-level value, not two separate
    accept-time computations), *When* `interceptor.call()` runs on each,
    *Then* both calls' outcomes match the shared decision and neither
    call touches `peer_is_authorized`/`peer_is_group_member_linux` at
    all — proven structurally, since `UdsPeerCredInterceptor::call()`'s
    implementation (Task 3.2.1b-impl) contains no call to either
    function, only a read of `req.extensions()`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 3.2.1b-impl: Implement `UdsPeerCredInterceptor` (~3 min)
- ```rust
  #[derive(Clone)]
  pub struct UdsPeerCredInterceptor {
      rejection_count: Arc<AtomicI64>,
  }

  impl UdsPeerCredInterceptor {
      pub fn new(rejection_count: Arc<AtomicI64>) -> Self {
          Self { rejection_count }
      }
  }

  impl tonic::service::Interceptor for UdsPeerCredInterceptor {
      fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
          let decision = req.extensions().get::<UdsAuthDecision>().copied();
          if decision.is_some_and(|d| d.authorized) {
              return Ok(req);
          }
          let count = self.rejection_count.fetch_add(1, Ordering::Relaxed) + 1;
          tracing::warn!(
              peer_uid = decision.and_then(|d| d.peer_uid),
              peer_gid = decision.and_then(|d| d.peer_gid),
              tymux_socket_peercred_rejection_total = count,
              "rejected UDS connection: peer not authorized"
          );
          Err(Status::permission_denied("not authorized to access this daemon's socket"))
      }
  }
  ```
  Note this interceptor no longer takes `daemon_uid`/`allowed_gid` at
  all — those are only ever consumed by `PreAuthorizedUnixStream::new`
  (Task 3.2.1a-impl), which runs once per connection, before this
  interceptor ever sees a request.
- Files: `crates/tymuxd/src/auth.rs`

##### Task 3.2.1b-test: Unit tests for the five ACs (~5 min)
- Build `Request<()>` directly, inserting `UdsAuthDecision` via
  `req.extensions_mut().insert(...)` — same pattern
  `BearerAuthInterceptor`'s own tests already use for `TcpConnectInfo`.
  `#[tracing_test::traced_test]` for the logging AC, matching
  `BearerAuthInterceptor`'s own precedent.
- `uds_peer_cred_interceptor_accepts_authorized_decision`,
  `uds_peer_cred_interceptor_rejects_unauthorized_decision`,
  `uds_peer_cred_interceptor_rejects_missing_decision`,
  `uds_peer_cred_interceptor_logs_peer_uid_gid_on_rejection`,
  `uds_peer_cred_interceptor_never_calls_peer_is_authorized_itself`
  (the structural "no recomputation" proof — a code-inspection-backed
  test name is acceptable here since the property is an absence of a
  call, not an observable side effect; alternatively, wire a
  call-counting test double for `peer_is_group_member_linux` behind a
  `#[cfg(test)]` seam if the implementer prefers a runtime assertion over
  a structural one — implementer's discretion).
- Files: `crates/tymuxd/src/auth.rs`

---

## Phase 4: tymuxd — `TymuxDaemon: Clone` + dual-listener wiring + TCP deprecation

### Epic 4.1: `TymuxDaemon: Clone`
**Goal**: One `TymuxDaemon` instance is shared, correctly, across both
listeners — never two independently-constructed instances silently
splitting shared counters/trackers (architecture.md §1's named trap).

#### Story 4.1.1: Add `#[derive(Clone)]`
**As** `tymuxd`, **I want** `TymuxDaemon` to be cheaply cloneable while
sharing every piece of internal state, **so that** registering it on two
listeners never splits `disconnect_tracker`/`attached_sessions_gauge`/
`resume_outcome_counters` into two non-communicating copies.
**Acceptance Criteria**:
- `TymuxDaemon` implements `Clone`, and cloned instances share state.
  - *Given* `let daemon = TymuxDaemon::new(engine); let cloned =
    daemon.clone();`, *When* an operation that increments
    `attached_sessions_gauge` runs against `cloned` (e.g. driving a real
    `Attach` call through `cloned`'s `attach()` method directly, no
    network needed), *Then* reading `daemon.attached_sessions_gauge`'s
    value (both share the same underlying `Arc<AtomicI64>`) reflects the
    increment — proving they're not two independent counters.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 4.1.1a: Add the derive (~2 min)
- At `main.rs:55` (`pub struct TymuxDaemon {`), add `#[derive(Clone)]`
  immediately above the struct definition. No field changes needed —
  every field is already `Arc<T>` or a `Copy` `Duration`
  (`engine: Arc<Engine>`, `disconnect_tracker: Arc<Mutex<...>>`,
  `attached_sessions_gauge: Arc<AtomicI64>`,
  `resume_outcome_counters: Arc<ResumeOutcomeCounters>`,
  `disconnect_regression_window`/`grace_period_duration`/
  `heartbeat_interval`: `Duration`, all `Copy` — confirmed by reading
  `main.rs:55-91` directly).
- Files: `crates/tymuxd/src/main.rs`

##### Task 4.1.1b: Regression test proving shared state (~5 min)
- `cloned_daemon_shares_attached_sessions_gauge_with_original` (or fold
  into an existing attach-flow test with an added assertion, at the
  implementer's discretion) — construct a daemon, clone it, drive one
  `Attach` call through the *clone*, assert the *original*'s
  `attached_sessions_gauge.load(Ordering::Relaxed)` reflects it.
- Files: `crates/tymuxd/src/main.rs`

### Epic 4.2: Dual-listener wiring in `main()`
**Goal**: `tymuxd` binds and serves both TCP and UDS concurrently,
sharing one `TymuxDaemon`, each gated by its own interceptor, both
draining fully on shutdown.

#### Story 4.2.1: Bind the UDS listener before spawning either server
**As** `tymuxd`, **I want** the UDS bind (lock, stale-check, umask-bind,
chown) to happen synchronously during startup, before any server task
spawns, **so that** a UDS bind failure is always fatal and loud, never a
silent downgrade to TCP-only.
**Acceptance Criteria**:
- A successful UDS bind proceeds to serve normally.
  - *Given* a writable, available socket path, *When* `tymuxd` starts,
    *Then* it logs "tymuxd listening" including the resolved `uds_path`
    and proceeds to accept connections on both listeners.
- A UDS bind failure (e.g. an unwritable parent directory) is fatal.
  - *Given* `TYMUXD_SOCKET_PATH=/nonexistent-root-owned-path/tymuxd.sock`
    where the daemon's own uid cannot create that parent directory,
    *When* `tymuxd` starts, *Then* it prints
    `failed to create Unix socket at /nonexistent-root-owned-path/tymuxd.sock: <io error>. Check that the parent directory exists and is writable, or override the path with --socket-path/TYMUXD_SOCKET_PATH.`
    to stderr as clean literal text (matching the existing
    `eprintln!`-then-`exit(1)` convention, not a `?`-propagated Debug
    dump) and exits with status 1 — the TCP listener never starts
    either.
- An unknown `--socket-group`/`TYMUXD_SOCKET_GROUP` name is fatal, end to
  end through `main()`'s actual wiring (not just `resolve_gid_by_name`'s
  own unit-level `None` return — architecture-review.md Concern fix: the
  ADR-002 "typo doesn't silently leave the socket owner-only" guarantee
  lives in this wiring, and was previously untested here).
  - *Given* `tymuxd --socket-group tyypo-group` where `tyypo-group` names
    no real POSIX group, *When* `tymuxd` starts, *Then* it prints
    `Error: --socket-group/TYMUXD_SOCKET_GROUP names an unknown group: tyypo-group`
    to stderr and exits with status 1 — the socket is never bound.
- A group `chown` failing with `EPERM` (the daemon's own process is not a
  member of the resolved group) is fatal with a message naming that
  specific problem, distinct from the generic bind-failure message
  (adversarial-review.md Concern fix; Task 2.2.1a's dedicated error
  path).
  - *Given* `tymuxd --socket-group root` run as a non-root user who is
    not a member of the `root` group (a portable, always-resolvable, and
    on any non-root CI runner reliably-not-a-member group name — skip
    this AC's test if the runner happens to violate that assumption),
    *When* `tymuxd` starts, *Then* it prints a message containing "is not
    a member of" to stderr (not the generic "Check that the parent
    directory exists and is writable" text) and exits with status 1.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 4.2.1a: Wire socket-path/group/lock/stale/bind sequence into `main()` (~5 min)
- Insert immediately after the existing auth gate block
  (`main.rs:1255-1273`), before the `sessions_dir`/`Engine` construction
  block:
  ```rust
  let uid = unsafe { libc::geteuid() };
  let socket_group_name = auth::resolve_socket_group_name(&args);
  let allowed_gid = socket_group_name.as_deref().map(|name| {
      auth::resolve_gid_by_name(name).unwrap_or_else(|| {
          eprintln!("Error: --socket-group/TYMUXD_SOCKET_GROUP names an unknown group: {name}");
          std::process::exit(1);
      })
  });
  let socket_path = auth::resolve_uds_socket_path(&args, uid);
  let tcp_disabled = auth::resolve_tcp_disabled(&args);

  let _socket_lock = auth::acquire_socket_lock(&socket_path).unwrap_or_else(|e| {
      eprintln!("Error: {e}");
      std::process::exit(1);
  });
  if let Err(e) = auth::reconcile_stale_socket(&socket_path) {
      eprintln!("Error: {e}");
      std::process::exit(1);
  }
  let uds_listener = auth::bind_uds_listener(&socket_path, allowed_gid).unwrap_or_else(|e| {
      // bind_uds_listener now returns Result<_, String> and formats its
      // own distinct message per failure mode (bind failure vs. the
      // group-chown-EPERM case) — main() just relays it, rather than
      // wrapping every failure in one generic template (Task 2.2.1a;
      // architecture-review.md/adversarial-review.md Concern fix).
      eprintln!("Error: {e}");
      std::process::exit(1);
  });
  ```
- `_socket_lock` must stay bound (not `let _ = ...`) so it lives through
  to the end of `main()` — dropping it early releases the lock while the
  daemon is still running.
- Files: `crates/tymuxd/src/main.rs`

##### Task 4.2.1b: Test for the fatal-bind-failure AC (~5 min)
- `main_exits_nonzero_with_clean_message_when_uds_socket_path_unwritable`
  — a subprocess-spawning test (matching this crate's existing
  `restart_persistence.rs`-style real-binary tests, or the simpler
  `daemon_startup.rs` pattern if one already exists for the bearer-token
  fail-fast case — mirror whichever precedent
  `check_non_loopback_requires_token`'s own fail-fast test already
  uses) asserting exit code `1` and the exact expected stderr substring,
  never a `Debug`-dump.
- Files: `crates/tymuxd/tests/` (new or existing integration-test file,
  matching wherever the bearer-token fail-fast startup test already
  lives — confirm exact file at implementation time via
  `grep -rn "check_non_loopback_requires_token" crates/tymuxd/tests/`)

##### Task 4.2.1c: Test for the fatal-unknown-group AC (~5 min)
- `main_exits_nonzero_with_clear_message_when_socket_group_unknown` — a
  subprocess-spawning test (same harness pattern as Task 4.2.1b) started
  with `--socket-group tymux-test-nonexistent-group-83f2` (the same
  guaranteed-absent fixture name Task 1.2.1c already uses for
  `resolve_gid_by_name`'s unit test), asserting exit code `1` and stderr
  containing `--socket-group/TYMUXD_SOCKET_GROUP names an unknown group`
  (architecture-review.md Concern fix — this is the end-to-end proof of
  the fatal-exit *wiring* that Task 1.2.1c's unit test alone does not
  cover).
- Files: `crates/tymuxd/tests/` (same file as Task 4.2.1b)

##### Task 4.2.1d: Test for the group-membership-`EPERM` fatal AC (~5 min)
- `main_exits_nonzero_with_clear_message_when_socket_group_membership_denied`
  — a subprocess-spawning test started with `--socket-group root` (or an
  equivalent group name reliably present but not one the CI runner's
  user belongs to), skipped (with a clear reason, matching this plan's
  established CI-privilege-guard pattern — Tasks 6.4.1c/7.3.1b/8.3.1c) if
  the test process is already a member of that group or is running as
  root, asserting exit code `1` and stderr containing "is not a member
  of" rather than the generic bind-failure text (adversarial-review.md
  Concern fix).
- Files: `crates/tymuxd/tests/` (same file as Task 4.2.1b)

#### Story 4.2.2: Spawn both servers, joined on shutdown
**As** `tymuxd`, **I want** both listeners driven concurrently under one
shutdown signal, with `TCP disabled` handled without deadlocking the
join, **so that** Ctrl-C/SIGTERM cleanly drains both (or the one active)
listener before the process exits.
**Acceptance Criteria**:
- Both listeners accept RPCs concurrently when TCP is not disabled —
  proven end-to-end by Task 5.2.3a's real dual-transport integration test
  (`tymuxd_dual_listener_drains_concurrent_tcp_and_uds_attach_streams_on_sigterm`),
  not only by Task 4.2.2c's manual/scripted verification (validation.md
  Gap 1 fix).
  - *Given* `tymuxd` started with default config (TCP enabled, UDS
    bound), *When* a `ListSessions` call is made over TCP and a separate
    `ListSessions` call is made over UDS, *Then* both succeed
    independently.
- `--disable-tcp-loopback` skips the TCP listener entirely without
  hanging shutdown.
  - *Given* `tymuxd` started with `--disable-tcp-loopback`, *When* the
    daemon receives SIGTERM shortly after starting, *Then* it logs
    "tymuxd shut down" and the process exits within a bounded time (e.g.
    5s) — proving the disabled-TCP branch resolves immediately rather
    than blocking the `tokio::join!` forever.
- Ctrl-C/SIGTERM drains both listeners before the process exits — proven
  end-to-end by the same Task 5.2.3a integration test (real subprocess,
  concurrent TCP + UDS `Attach` streams, real SIGTERM), not only by Task
  4.2.2c's manual/scripted verification (validation.md Gap 1 fix).
  - *Given* `tymuxd` started with default config and an open `Attach`
    stream in flight on each listener, *When* SIGTERM is sent, *Then*
    both streams complete their graceful drain (no abrupt connection
    reset) before the process exits.
**Files**: `crates/tymuxd/src/main.rs`, `crates/tymuxd/tests/uds_socket_lifecycle.rs`

##### Task 4.2.2a: Replace the single-listener tail of `main()` with the dual-listener version (~5 min)
- First, extract the three sites' duplicated
  `.http2_keepalive_interval(...).http2_keepalive_timeout(...)` pair
  (architecture-review.md's Reuse-Check nitpick fix — two of the three
  copies predate this plan, the UDS branch below adds a third; consolidate
  rather than triple it) into one helper:
  ```rust
  fn configured_server_builder() -> Server {
      Server::builder()
          .http2_keepalive_interval(Some(Duration::from_secs(30)))
          .http2_keepalive_timeout(Some(Duration::from_secs(10)))
  }
  ```
  (If `tonic::transport::Server`'s builder type doesn't name cleanly as a
  bare return type at implementation time, an equivalent macro or a
  closure returning `impl FnOnce() -> Server` achieves the same
  de-duplication — implementer's discretion; the point is one definition
  of the keepalive settings, not the exact extraction mechanism.)
- Then replace `main.rs:1309-1330` (`let daemon = TymuxDaemon::new(engine);`
  through the closing `}` of the `if let Some(token)`/`else` branch)
  with:
  ```rust
  let daemon = TymuxDaemon::new(engine);
  tracing::info!(%addr, uds_path = %socket_path.display(), "tymuxd listening");

  if tcp_disabled {
      tracing::info!("TCP loopback listener disabled via --disable-tcp-loopback/TYMUXD_DISABLE_TCP_LOOPBACK");
      // Fail-loud, not silent-footgun (matches this project's existing
      // unknown-group-name/TCP-deprecation posture): a configured bearer
      // token is only ever checked on the TCP listener, so disabling TCP
      // silently makes it inert (architecture-review.md Concern fix).
      if configured_token.is_some() {
          tracing::warn!(
              "a bearer token is configured (--token/TYMUXD_TOKEN) but \
               --disable-tcp-loopback/TYMUXD_DISABLE_TCP_LOOPBACK is also set — the token is now \
               unused, since it is only ever checked on the TCP listener, which is disabled."
          );
      }
  } else {
      tracing::warn!(
          %socket_addr,
          uds_path = %socket_path.display(),
          "tymuxd's TCP listener ({socket_addr}) is deprecated and will be removed in a future \
           release; it accepts connections from any local process with no credential check, \
           regardless of the new Unix-socket listener at {}. Other local users are isolated only \
           if nothing on this host still connects over TCP — set \
           --disable-tcp-loopback/TYMUXD_DISABLE_TCP_LOOPBACK=1 once your clients have migrated \
           to the Unix socket.",
          socket_path.display(),
      );
  }

  let uds_rejection_count = Arc::new(AtomicI64::new(0));
  let uds_daemon = daemon.clone();
  let uds_future = configured_server_builder()
      .add_service(TymuxServiceServer::with_interceptor(
          uds_daemon,
          auth::UdsPeerCredInterceptor::new(uds_rejection_count),
      ))
      .serve_with_incoming_shutdown(
          // PreAuthorizedUnixStream computes the authorization decision
          // once per accepted connection, here, before tonic's HTTP/2
          // handshake — not once per RPC (architecture-review.md
          // Performance Concern fix; Epic 3.2). `futures::TryStreamExt`
          // is already available via this crate's existing `futures =
          // "0.3"` workspace dependency, no new dependency needed.
          //
          // `.map_ok` only transforms `Ok` items; a transient `Err` item
          // from `accept()` (e.g. EMFILE/ENFILE) passes through
          // unchanged. No filter/retry wrapper is needed here: tonic's
          // own accept loop (tonic-0.12.3's
          // `Server::serve_with_shutdown`, mod.rs:617-652, which this
          // method calls into) already `continue`s — not `break`s — on a
          // `Some(Err(_))` stream item, so one transient accept error
          // never tears down this listener (adversarial-review.md
          // iteration-2 Concern, resolved by verification — see
          // Observability Plan).
          futures::TryStreamExt::map_ok(
              tokio_stream::wrappers::UnixListenerStream::new(uds_listener),
              move |stream| auth::PreAuthorizedUnixStream::new(stream, uid, allowed_gid),
          ),
          shutdown_signal(),
      );

  let tcp_future = async {
      if tcp_disabled {
          return Ok(());
      }
      if let Some(token) = configured_token {
          let rejection_count = Arc::new(AtomicI64::new(0));
          configured_server_builder()
              .add_service(TymuxServiceServer::with_interceptor(
                  daemon,
                  auth::BearerAuthInterceptor::new(Arc::new(token), rejection_count),
              ))
              .serve_with_shutdown(socket_addr, shutdown_signal())
              .await
      } else {
          configured_server_builder()
              .add_service(TymuxServiceServer::new(daemon))
              .serve_with_shutdown(socket_addr, shutdown_signal())
              .await
      }
  };

  let (uds_res, tcp_res) = tokio::join!(uds_future, tcp_future);
  uds_res?;
  tcp_res?;
  tracing::info!("tymuxd shut down");
  Ok(())
  ```
  (`tokio::join!`, not `select!`/`try_join!` — chosen so a disabled TCP
  branch resolves immediately via the `if tcp_disabled { return Ok(()) }`
  short-circuit rather than hanging forever on a `pending()` future, and
  so both listeners always fully drain on shutdown rather than one being
  dropped mid-drain the instant the other resolves first. Both listener
  futures share the same shutdown trigger by calling `shutdown_signal()`
  independently once each — tokio's `ctrl_c()`/`signal::unix::signal()`
  both document support for multiple concurrent listeners per signal, so
  this is not the double-registration hazard architecture.md §4 flagged
  as needing verification; confirmed safe by tokio's own multi-listener
  support, not left as an Unresolved Question.)
- Files: `crates/tymuxd/src/main.rs`

##### Task 4.2.2b: Update the three existing test-harness call sites (~5 min)
- `spawn_test_server` (`main.rs:1439`) and
  `spawn_non_loopback_test_server` (`main.rs:1463`) and the inlined
  harness in `attach_streams_output_and_signals_exit`
  (`main.rs:~3234`) are TCP-only and **stay unmodified** — they don't
  construct a `socket_path`/UDS listener at all, and nothing in this
  story requires them to; they continue exercising the TCP path exactly
  as before. Confirm (read-through, no code change) that none of them
  call the old single-branch tail this task just replaced.
- Files: none (verification-only task)

##### Task 4.2.2c: Manual/scripted verification of dual-listener accept + graceful SIGTERM drain (~5 min)
- Not a `cargo test` unit test (needs two real concurrent transports plus
  signal delivery, better proven in Phase 5's integration tests) — this
  task is a quick local run-through: start `tymuxd` locally, confirm both
  `TYMUXD_ADDR`'s TCP port and the logged `uds_path` accept
  `ListSessions`, send SIGTERM, confirm "tymuxd shut down" appears and
  the process exits. Findings feed directly into writing Task 5.2.3a's
  real, automated integration test next (validation.md Gap 1 fix — this
  manual pass is no longer the only proof of Story 4.2.2's concurrent-
  accept/graceful-drain ACs) — this task itself adds no test code.
- Files: none

##### Task 4.2.2d: Unit test for the token-inert-with-TCP-disabled warning (~3 min)
- `tcp_disabled_and_token_configured_logs_warning_naming_token_unused` —
  `#[tracing_test::traced_test]`, drives the same testable
  branch-extraction point Task 4.3.1a already uses (or the inline
  `main()` branch directly, whichever the implementer finds testable in
  isolation), asserting the warning fires only when both
  `configured_token.is_some()` and `tcp_disabled` are true, and does not
  fire when only one is (architecture-review.md Concern fix).
- Files: `crates/tymuxd/src/main.rs` (or `auth.rs`, matching wherever
  Task 4.3.1a's extraction lands)

### Epic 4.3: TCP-deprecation warning content (already wired in Task 4.2.2a) — verification pass
**Goal**: Confirm the warning text satisfies ux.md's explicit requirement
(state the "both by default, TCP still unauthenticated" caveat plainly,
name the off-switch) and isn't swallowed by the default log filter.

#### Story 4.3.1: Deprecation-warning content and level assertions
**As an** operator reading `tymuxd`'s startup logs, **I want** the
deprecation warning to state plainly that TCP remains open and
unauthenticated and to name the flag that turns it off, **so that** I
don't mistake "UDS shipped" for "my shared host is now isolated."
**Acceptance Criteria**:
- The warning fires at `warn` level specifically (not `info`/`debug`),
  confirming it survives the default `EnvFilter::new("info")` config.
  - *Given* `tymuxd` started with default config (TCP enabled),
    *When* it starts, *Then* a `tracing::warn!`-level record is emitted
    containing the socket address, the resolved `uds_path`, and the
    literal substring `--disable-tcp-loopback`.
- The warning is skipped (replaced by an info-level notice) when TCP is
  disabled.
  - *Given* `tymuxd` started with `--disable-tcp-loopback`, *When* it
    starts, *Then* no `warn`-level TCP-deprecation record is emitted, and
    a `tracing::info!`-level record containing "TCP loopback listener
    disabled" is emitted instead.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 4.3.1a: `#[tracing_test::traced_test]` tests for both ACs (~5 min)
- `tcp_deprecation_warning_fires_at_warn_level_with_disable_flag_named`,
  `tcp_deprecation_warning_skipped_and_info_logged_when_tcp_disabled` —
  both drive a real (test-harness) startup path or, if invoking the full
  `main()` startup sequence isn't practical from a unit test, extract the
  warning-vs-info branch from Task 4.2.2a into a small testable helper
  function (e.g. `log_tcp_listener_status(tcp_disabled: bool, socket_addr: SocketAddr, uds_path: &Path)`)
  at implementation time if the inline version proves untestable in
  isolation — matching this file's own established "extract a pure/
  testable function rather than testing inline `main()` logic" pattern
  used throughout `auth.rs`.
- Files: `crates/tymuxd/src/main.rs` (or `crates/tymuxd/src/auth.rs` if
  extracted per the note above)

#### Story 4.3.2: `--socket-group` caveat logged once at startup when configured
**As an** operator who sets `--socket-group`/`TYMUXD_SOCKET_GROUP`, **I
want** the daemon to tell me at startup what that flag actually grants
and where its guarantee is weaker, **so that** I don't learn "this grants
full daemon control, not scoped access" or "this is primary-gid-only on
my platform" only by reading source or an ADR after the fact. Closes
ux.md's own Gap 1 for these two of its three caveats, and doubles as the
fix for pre-mortem.md P2 #4, which flagged the same caveat as needing to
be logged at startup rather than documented only in `--socket-group`'s
help text (Task 1.2.1a) — one task addresses both findings, since they're
the same gap.
**Acceptance Criteria**:
- Whenever `--socket-group`/`TYMUXD_SOCKET_GROUP` resolves to a
  configured gid, a `tracing::info!` record fires once at startup naming
  both: that group membership grants **full daemon control** (not a
  scoped per-user subset — `CreateSession`, `Attach`/`CapturePane`,
  `KillSession`, identical to the socket owner), and, only on non-Linux
  targets, that group-membership checking is **primary-gid-only** there
  (full supplementary-group parity is Linux-only per ADR-002).
  - *Given* `tymuxd --socket-group teammates` on Linux, *When* it starts,
    *Then* a `tracing::info!`-level record fires once containing "full
    daemon control" (or equivalent literal phrasing matching
    Deployment Guidance's own wording) and the configured group name
    `teammates`, but does **not** mention primary-gid-only (Linux has
    full supplementary-group support).
  - *Given* the same flag on a non-Linux target (or with the platform
    check stubbed for the test), *When* `tymuxd` starts, *Then* the same
    record also contains the primary-gid-only caveat.
- No `--socket-group` configured: no new log line at all — this notice is
  conditional on the flag actually being in effect, unlike Surface 1's
  unconditional TCP-deprecation warning.
  - *Given* `tymuxd` started with no `--socket-group`/
    `TYMUXD_SOCKET_GROUP`, *When* it starts, *Then* no record containing
    "full daemon control" is emitted.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 4.3.2a: Log the caveat once, conditioned on `allowed_gid` (~3 min)
- Immediately after Task 4.2.1a's `allowed_gid`/`socket_group_name`
  resolution in `main()`, before the bind sequence:
  ```rust
  if let (Some(gid), Some(name)) = (allowed_gid, &socket_group_name) {
      let platform_note = if cfg!(target_os = "linux") {
          ""
      } else {
          " on this platform, membership is checked via the connecting \
           process's primary group only (not full supplementary-group \
           parity, which is Linux-only per ADR-002) — a teammate whose \
           primary group isn't `teammates` may be denied even if listed \
           as a supplementary member;"
      };
      tracing::info!(
          socket_group = %name, socket_group_gid = gid,
          "--socket-group/TYMUXD_SOCKET_GROUP is configured: members of this group get \
           full daemon control (CreateSession, Attach/CapturePane, KillSession — identical \
           to the socket owner), not a scoped per-user subset;{platform_note} see Deployment \
           Guidance in project_plans/unix-socket-auth/implementation/plan.md for the full caveat."
      );
  }
  ```
- Files: `crates/tymuxd/src/main.rs`

##### Task 4.3.2b: Tests for both ACs (~3 min)
- `socket_group_caveat_logged_once_at_startup_when_group_configured`,
  `socket_group_caveat_absent_when_no_group_configured` — same
  `#[tracing_test::traced_test]` harness as Task 4.3.1a (extract to a
  small testable helper if driving the full `main()` startup path proves
  impractical from a unit test, matching that task's own fallback note).
- Files: `crates/tymuxd/src/main.rs`

---

## Phase 5: tymuxd — integration tests against a real dual-listener daemon

### Epic 5.1: Accept/reject over a real UDS connection
**Goal**: Prove the full UDS request path end-to-end — bind, connect,
peer-cred extraction, authorization decision, RPC — not just each pure
function in isolation.

#### Story 5.1.1: Same-uid accept and mismatched-uid reject over real UDS
**As** `tymuxd`, **I want** an end-to-end test proving a same-uid client
succeeds and there's no way for a mismatched identity to reach an RPC
handler over a real socket, **so that** the unit-level `peer_is_authorized`
proof is backed by a real kernel-verified round-trip.
**Acceptance Criteria**:
- A client connecting as the daemon's own uid (the only uid available in
  a non-root CI process — this test process's own real uid, which always
  matches "the daemon's own uid" since both are the same OS process
  tree) succeeds.
  - *Given* a real `tymuxd`-equivalent test harness bound to a UDS path
    with `daemon_uid = <this test process's own real uid>`, *When* a
    `TymuxServiceClient` dials that path and calls `ListSessions`,
    *Then* the call succeeds.
- The decision-function-level reject case is additionally proven directly
  (not just via Story 3.2.1b's unit tests) against the same live-harness
  wiring, per pitfalls.md §7's documented CI-privilege caveat: **this
  task does not attempt a true cross-uid connection** (would require
  `CAP_SETUID`/root — see Unresolved Questions) — it instead constructs
  the harness with a deliberately wrong `daemon_uid` (e.g. the real uid
  plus 1), proving the *server-side wiring* (bind → accept → interceptor
  → rejection) rejects a real connection end-to-end even though the
  *peer's* uid genuinely didn't change — the same "prove the wiring, not
  the OS's uid-reporting" split Story 3.2.1b's unit tests already use, now
  exercised over a real socket instead of a constructed `Request<()>`.
  - *Given* the same harness with `daemon_uid` set to a value that does
    not match this test process's real uid, *When* the same client
    dials and calls `ListSessions`, *Then* the call fails with
    `tonic::Code::PermissionDenied`.
**Files**: `crates/tymuxd/src/main.rs` (new test-harness helper +
integration tests, mirroring `spawn_non_loopback_test_server`'s shape)

##### Task 5.1.1a: `spawn_uds_test_server` harness helper (~5 min)
- Add alongside `spawn_test_server`/`spawn_non_loopback_test_server`:
  binds a real `tokio::net::UnixListener` at a fresh temp path (via
  `std::env::temp_dir().join(format!("tymux-uds-test-{}.sock", Uuid::new_v4()))`,
  bypassing the full `default_uds_socket_path`/lock/stale-check machinery
  since those are already independently tested in Phase 2 — this harness
  only needs a bound listener), wraps the incoming stream with
  `auth::PreAuthorizedUnixStream::new(stream, daemon_uid, allowed_gid)`
  (caller-supplied `daemon_uid`/`allowed_gid`, matching Epic 3.2's design
  — the interceptor itself, `auth::UdsPeerCredInterceptor::new(rejection_count)`,
  takes no uid/gid at all), returns a connected `TymuxServiceClient`
  dialed via the same `connect_with_connector` pattern Phase 6 builds for
  `tymux-cli` (or a minimal duplicate of it scoped to this test file, at
  the implementer's discretion — this repo has no shared test-utility
  crate to place it in without a larger restructure out of this plan's
  scope).
- Files: `crates/tymuxd/src/main.rs`

##### Task 5.1.1b: The two integration tests (~5 min)
- `uds_server_accepts_matching_uid_client`,
  `uds_server_rejects_when_daemon_uid_does_not_match_wiring`.
- Files: `crates/tymuxd/src/main.rs`

#### Story 5.1.2: Group-access accept over real UDS
**As** `tymuxd`, **I want** an end-to-end test proving a configured
group's member gets access over a real socket, **so that** the
group-relaxation path (mode `0660` + `chown` + `peer_is_group_member`) is
proven together, not just as separate unit pieces.
**Acceptance Criteria**:
- A client whose real gid matches the configured group succeeds even
  though its uid doesn't match the daemon's.
  - *Given* `spawn_uds_test_server` with `daemon_uid = <real uid + 1>`
    (deliberately mismatched, forcing the group path) and
    `allowed_gid = Some(<this test process's own real egid>)`, *When* a
    client dials and calls `ListSessions`, *Then* the call succeeds
    (the test process's own connection is, by construction, a member of
    its own real gid, exercising the genuine `/proc`-based Linux path
    from Story 3.1.1 over a real accepted connection).
**Files**: `crates/tymuxd/src/main.rs`

##### Task 5.1.2a: The integration test (~5 min)
- `uds_server_accepts_group_member_when_uid_differs_but_gid_matches`,
  `#[cfg(target_os = "linux")]`-gated per Story 3.1.1's own platform
  scoping (the primary-gid fallback on other platforms is already
  covered by Story 3.1.2's unit tests; this integration test's specific
  value is proving the `/proc` path against a real accepted connection,
  which only exists on Linux).
- Files: `crates/tymuxd/src/main.rs`

### Epic 5.2: Stale-socket and concurrent-start races
**Goal**: Prove the lock-file + stale-socket-reconciliation sequence
against a real second `tymuxd`-shaped process, not just the pure
functions in isolation (Phase 2 already unit-tests those; this proves
the *composition* under a genuine subprocess race).

#### Story 5.2.1: A second `tymuxd` refuses to steal a live socket
**As an** operator who accidentally starts a second `tymuxd` instance
against the same socket path, **I want** it to fail loudly rather than
silently take over, **so that** the first instance's clients don't
silently start failing against orphaned state.
**Acceptance Criteria**:
- A second real `tymuxd` process started against the same
  `TYMUXD_SOCKET_PATH` while the first is still running exits nonzero
  with a clear "already running" message, and the first instance keeps
  serving unaffected.
  - *Given* a real `tymuxd` subprocess started and confirmed listening
    (via its "tymuxd listening" stdout line, matching this repo's
    existing subprocess-test convention from
    `clients/go/integration/integration_test.go`'s `startDaemon`), *When*
    a second `tymuxd` subprocess is started with the identical
    `TYMUXD_SOCKET_PATH`, *Then* the second process exits nonzero within
    a bounded time and its stderr contains "already running", while a
    `ListSessions` call against the first instance's socket still
    succeeds.
**Files**: `crates/tymuxd/tests/` (new integration test file, e.g.
`uds_socket_lifecycle.rs`, matching the existing
`restart_persistence.rs` real-subprocess pattern)

##### Task 5.2.1a: The subprocess-race integration test (~5 min)
- Mirror `restart_persistence.rs`'s existing pattern for spawning a real
  `tymuxd` binary (`resolveBinary`-equivalent logic, or reuse whatever
  helper that file already has) with `TYMUXD_SOCKET_PATH` pinned to a
  fixed temp path shared by both subprocess invocations.
- Files: `crates/tymuxd/tests/uds_socket_lifecycle.rs` (new)

#### Story 5.2.2: A restarted `tymuxd` re-binds cleanly under an open UDS client
**As an** operator restarting `tymuxd` (clean shutdown+relaunch, or a
supervisor-driven crash restart) while a client has an open UDS
`Attach` stream, **I want** the new instance to re-bind the socket
cleanly and the client's existing session-resume path to reconnect over
it, **so that** the new `SocketLockGuard`/`reconcile_stale_socket`
machinery (Epic 2.1) is proven to compose correctly with `tymuxd`'s
pre-existing (TCP-only) restart/resume behavior, not just assumed to
(adversarial-review.md Concern fix — Epic 5.2 previously only tested two
*concurrently starting* instances racing over the lock file, not a
restart with a live client in flight, which exercises the same
`SocketLockGuard`/`reconcile_stale_socket` code sitting directly in the
daemon's existing resume-after-restart startup path).
**Acceptance Criteria**:
- A client's `Attach` stream survives a clean `tymuxd` restart via the
  existing resume mechanism, now over UDS.
  - *Given* a real `tymuxd` subprocess bound to a known
    `TYMUXD_SOCKET_PATH`, and a client with an open `Attach` stream to it
    over that UDS socket, *When* the daemon subprocess is sent SIGTERM
    (clean shutdown, draining as Story 4.2.2 already proves) and a new
    `tymuxd` subprocess is started against the identical
    `TYMUXD_SOCKET_PATH` once the first has fully exited, *Then* the new
    instance binds the socket successfully (no stale-socket or
    lock-contention error, since the first instance released its
    `SocketLockGuard`/removed its own socket file on clean exit — or, if
    it left a stale file behind, `reconcile_stale_socket` clears it per
    Story 2.1.2), and the client's existing resume-on-reconnect path
    (already proven over TCP by `restart_persistence.rs`) reconnects
    over the freshly re-bound UDS socket and successfully resumes the
    same session.
- The same sequence works after an unclean exit (simulated crash, no
  SIGTERM), proving `reconcile_stale_socket`'s stale-file cleanup
  composes correctly with the resume path, not just the lock-acquisition
  path Story 5.2.1 already covers.
  - *Given* the same setup, but the first `tymuxd` subprocess is killed
    with `SIGKILL` (no graceful drain, socket file left on disk) instead
    of `SIGTERM`, *When* a new `tymuxd` subprocess starts against the
    same `TYMUXD_SOCKET_PATH`, *Then* it detects the leftover socket file
    as stale (per `reconcile_stale_socket`'s connect-probe), removes it,
    binds successfully, and the client's resume path reconnects and
    resumes the same session exactly as in the clean-shutdown case.
**Files**: `crates/tymuxd/tests/uds_socket_lifecycle.rs` (same file as
Task 5.2.1a), or `crates/tymuxd/tests/restart_persistence.rs` if that
file's existing harness is the more natural place to add a UDS-flavored
variant of its existing TCP restart-resume test (implementer's
discretion — confirm at implementation time via
`grep -n "restart" crates/tymuxd/tests/restart_persistence.rs`)

##### Task 5.2.2a: The clean-restart-with-open-UDS-client integration test (~5 min)
- Mirror `restart_persistence.rs`'s existing TCP resume-after-restart
  test structure exactly, substituting a UDS-dialed client
  (`connect_with_connector`, matching Phase 6's `dial_uds` pattern or a
  minimal duplicate scoped to this test file) for its TCP one.
- `tymuxd_restart_with_open_uds_attach_stream_resumes_cleanly`.
- Files: `crates/tymuxd/tests/uds_socket_lifecycle.rs`

##### Task 5.2.2b: The unclean-exit (stale-socket) variant (~5 min)
- Same harness as Task 5.2.2a, but the first subprocess is killed with
  `SIGKILL` rather than sent SIGTERM, leaving its socket file on disk for
  the second instance's `reconcile_stale_socket` to clean up.
- `tymuxd_restart_after_unclean_exit_with_open_uds_attach_stream_resumes_cleanly`.
- Files: `crates/tymuxd/tests/uds_socket_lifecycle.rs`

#### Story 5.2.3: Concurrent TCP + UDS `Attach` streams both drain on SIGTERM
**As an** operator relying on Story 4.2.2's "both listeners drain
gracefully" claim, **I want** an automated proof of that claim against
the real dual-listener `main()`, **so that** the claim other tests
(Story 5.2.2's own prose: "sent SIGTERM (clean shutdown, draining as
Story 4.2.2 already proves)") lean on actually has a `cargo test` behind
it, not just Task 4.2.2c's manual/scripted check (validation.md's
self-identified Gap 1: Story 4.2.2's own ACs for concurrent dual-transport
accept and dual-transport graceful SIGTERM drain had no automated test —
Epic 5.1 uses a UDS-only custom harness that bypasses the real `main()`
dual-listener path, and Epic 5.2's restart/resume tests drive only a UDS
client, never a concurrent TCP client at the same time).
**Acceptance Criteria**:
- A real `tymuxd` subprocess started with both listeners enabled (default
  config, no `--disable-tcp-loopback`) accepts one `Attach` stream over
  TCP and one `Attach` stream over UDS concurrently, both succeeding
  independently — automating Story 4.2.2's first AC end-to-end rather
  than only at the `spawn_uds_test_server`-harness level (Epic 5.1) or by
  hand (Task 4.2.2c).
  - *Given* a real `tymuxd` subprocess started with default config and
    listening on both transports, *When* one client opens an `Attach`
    stream over TCP and a second client opens an `Attach` stream over UDS
    at the same time, *Then* both streams establish successfully and both
    receive output from their respective sessions.
- Sending SIGTERM to that subprocess drains both open streams gracefully
  (no abrupt connection reset on either transport) before the process
  exits, within a bounded time (e.g. 5s) — automating Story 4.2.2's third
  AC end-to-end.
  - *Given* the same subprocess with both `Attach` streams still open,
    *When* SIGTERM is sent to the subprocess, *Then* both streams observe
    a clean stream-end (not a hard reset/`ConnectionReset` error) and the
    subprocess exits within the bounded time.
**Files**: `crates/tymuxd/tests/uds_socket_lifecycle.rs` (alongside Task
5.2.1a's harness, per validation.md Gap 1's own recommended location)

##### Task 5.2.3a: The dual-transport concurrent-SIGTERM-drain integration test (~5 min)
- Mirror Task 5.2.1a's real-subprocess-spawning pattern (`resolveBinary`-
  equivalent, `TYMUXD_SOCKET_PATH` pinned to a fixed temp path), started
  with default config (both listeners active).
- Open one `Attach` stream over TCP (existing TCP-dialing pattern already
  used elsewhere in this crate's tests) and one over UDS
  (`connect_with_connector`, matching Phase 6's `dial_uds` pattern or a
  minimal duplicate scoped to this test file, same as Task 5.2.2a) against
  that subprocess concurrently.
- Send SIGTERM to the subprocess; assert both streams complete a graceful
  drain (no `ConnectionReset`/abrupt EOF) and the subprocess exits within
  a bounded timeout.
- `tymuxd_dual_listener_drains_concurrent_tcp_and_uds_attach_streams_on_sigterm`.
- Files: `crates/tymuxd/tests/uds_socket_lifecycle.rs`

---

## Phase 6: tymux-cli — UDS dialing

### Epic 6.1: Socket-path resolution + `--addr` becomes `Option<String>`
**Goal**: `tymux-cli` computes the identical default socket path
`tymuxd` does, and can tell "no `--addr` given, try UDS first" apart from
"explicit `--addr`, dial exactly that."

#### Story 6.1.1: Port `default_uds_socket_path` + a `--socket-path` clap field
**As a** `tymux-cli` user on the same host as `tymuxd`, **I want** the CLI
to compute the same default socket path the daemon does, and to be able
to discover the override flag via `tymux --help`, **so that** the
zero-config case connects without me passing any flag, and the escape
hatch is discoverable the same way `--token` already is.

Unlike `tymuxd` (which stays hand-rolled/`clap`-free by design — Pattern
Decisions row "`tymuxd` new-flag mechanism"), `tymux-cli` already depends
on and uses `clap`, and its own `--token` flag already renders in
`tymux --help` via clap's env-annotation (`main.rs:194`). This plan's
original design had `tymux-cli` also hand-roll a `std::env::args()` scan
for `--socket-path`/`TYMUXD_SOCKET_PATH` — ux.md flagged this as Gap 2:
an asymmetry *within the same CLI, introduced by the same feature*
(`--token` discoverable, `--socket-path` not). Fixed here by making
`--socket-path` an ordinary `clap` field instead (Pattern Decisions row
"`tymux-cli` new-flag mechanism"); only `default_uds_socket_path` (the
pure default-computation function, no flag/env parsing of its own) is
still ported byte-for-byte, since `clap`'s own env-fallback replaces what
`resolve_uds_socket_path`'s hand-rolled scan would otherwise have done.
**Acceptance Criteria**:
- `default_uds_socket_path`: identical to Story 1.1.1's ACs, applied to
  `tymux-cli`'s own copy of the function (same five cases) — not
  re-listed verbatim here.
- `--socket-path <PATH>` overrides the computed default, and renders in
  `tymux --help` with its env-var annotation, matching `--token`'s
  existing precedent.
  - *Given* argv `["tymux", "--socket-path", "/custom/tymuxd.sock", "ls"]`,
    *When* `Cli::parse()` runs, *Then* `cli.socket_path` is
    `Some("/custom/tymuxd.sock".to_string())`.
  - *Given* argv `["tymux", "--help"]`, *When* it runs, *Then* the output
    contains a `--socket-path <SOCKET_PATH>    [env: TYMUXD_SOCKET_PATH=]`
    line (clap's standard rendering for an `env`-annotated `Option<String>`
    field), closing ux.md Gap 2.
  - *Given* no `--socket-path` flag and no `TYMUXD_SOCKET_PATH` env var,
    *When* `Cli::parse()` runs, *Then* `cli.socket_path` is `None`, and
    the caller (Task 6.2.1b's `dial_channel`) falls back to
    `default_uds_socket_path(uid)`.
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1a: Port `default_uds_socket_path` (~3 min)
- Same implementation as Task 1.1.1a (the pure function only — no
  `resolve_uds_socket_path` port; clap's own `env = "TYMUXD_SOCKET_PATH"`
  fallback replaces that hand-rolled scan, see Task 6.1.1c), with a doc
  comment stating: "Mirrors `tymuxd`'s `auth::default_uds_socket_path`
  byte-for-byte — see plan.md Pattern Decisions row 10 for why this is
  duplicated rather than shared via `tymux-core`. Any change here must be
  mirrored in `tymuxd`, `clients/go`, and `clients/ts`."
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1b: Unit tests for `default_uds_socket_path`, reading the shared fixture file (~5 min)
- Same five test names as Task 1.1.1b's `default_path_cases`,
  `tymux-cli`-scoped. Reads
  `project_plans/unix-socket-auth/socket-path-fixtures.json`'s
  `default_path_cases` array (created by Task 1.1.1b) instead of
  re-authoring the cases inline — same file, same schema, second Rust
  reader (architecture-review.md's test-duplication Concern fix).
- Files: `crates/tymux-cli/src/main.rs`,
  `project_plans/unix-socket-auth/socket-path-fixtures.json`

##### Task 6.1.1c: Add `--socket-path` as a `clap` field (~3 min)
- Add to `Cli` alongside the existing `token` field (`main.rs:188-195`),
  matching its exact shape:
  ```rust
  /// Path to tymuxd's Unix domain socket. Defaults to the same path
  /// tymuxd itself computes ($XDG_RUNTIME_DIR/tymuxd/tymuxd.sock, or a
  /// uid-scoped fallback under $TMPDIR/tmp) — override only for
  /// non-default deployments (multiple tymuxd instances, a custom
  /// runtime dir). When overriding, prefer a tymuxd-owned subdirectory
  /// (e.g. $XDG_RUNTIME_DIR/my-tymuxd/tymuxd.sock) rather than a shared
  /// runtime directory directly, matching tymuxd's own default nesting.
  /// Note: a socket reached through a bind-mounted path inside a
  /// container may present a different uid than `id -u` shows locally —
  /// see this repo's README, "Multi-user / shared-host deployment"
  /// section (added by Task 9.1.1a), for the full caveat (Deployment
  /// Guidance; ux.md Gap 1 fix).
  #[arg(long, global = true, env = "TYMUXD_SOCKET_PATH")]
  socket_path: Option<String>,
  ```
  (The doc-comment's containerized-uid-mismatch pointer is the one
  Deployment Guidance caveat that's actually relevant to a client-side
  flag — see plan.md item 14b's scoping; the `--socket-group`/macOS
  caveats belong to `tymuxd`'s own flag, which has no `--help` surface at
  all, unaffected by this task.)
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.1d: Test asserting `--socket-path` renders in `--help` (~2 min)
- `cli_help_output_lists_socket_path_flag_with_env_annotation` — asserts
  `tymux --help`'s captured output contains `--socket-path` and
  `TYMUXD_SOCKET_PATH` (matching however the existing `--token`
  discoverability, if tested at all, is verified today — mirror that
  precedent; if no such test exists yet for `--token`, this is the first
  one and sets it).
- Files: `crates/tymux-cli/src/main.rs`

#### Story 6.1.2: `--addr` becomes `Option<String>`
**As a** `tymux-cli` user, **I want** an explicit `--addr` to mean
"dial exactly this, skip UDS," and no `--addr` to mean "try UDS first,"
**so that** existing TCP-based automation keeps working unchanged while
the zero-config case gets the more secure default.
**Acceptance Criteria**:
- No `--addr` given: the CLI attempts UDS first (proven in Story 6.2.1's
  ACs, not here — this story only proves the flag-parsing distinction).
  - *Given* argv `["tymux", "ls"]` (no `--addr`), *When* `Cli::parse()`
    runs, *Then* `cli.addr` is `None`.
- Explicit `--addr` is honored exactly, UDS is never attempted.
  - *Given* argv `["tymux", "--addr", "http://example.com:1234", "ls"]`,
    *When* `Cli::parse()` runs, *Then* `cli.addr` is
    `Some("http://example.com:1234".to_string())`.
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 6.1.2a: Change the `addr` field (~2 min)
- At `main.rs:179`, change
  `#[arg(long, global = true, default_value = "http://127.0.0.1:7419")] addr: String`
  to `#[arg(long, global = true)] addr: Option<String>`.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.1.2b: Update the existing `--addr`-parsing test (~2 min)
- The existing test at `main.rs:1525` asserting `--addr` parsing must be
  updated to compare against `Some("http://example.com:1234".to_string())`
  rather than a bare `String`; add a companion test asserting `cli.addr`
  is `None` when the flag is omitted.
- Files: `crates/tymux-cli/src/main.rs`

### Epic 6.2: UDS dialing via `connect_with_connector`
**Goal**: `run()` dials the resolved UDS path first, falling back to TCP
loopback with a single logged notice on failure — never gated on
`isatty()`.

#### Story 6.2.1: `dial_channel` — UDS-first, logged TCP fallback
**As a** `tymux-cli` user with no `--addr` given, **I want** the CLI to
connect over the local Unix socket when one is present and reachable,
falling back to TCP loopback (with a visible note) otherwise, **so that**
I get the more secure default without any configuration, and a clear
signal when I'm silently on the deprecated transport.
**Acceptance Criteria**:
- A reachable UDS socket is dialed and used.
  - *Given* a real `tymuxd` test daemon bound to a known UDS path (no
    `--addr` given), *When* `dial_channel(None, &resolved_socket_path)`
    runs, *Then* the returned channel successfully calls `ListSessions`
    against that daemon, and no TCP fallback notice is printed.
- No UDS socket present falls back to TCP with a one-line notice — this
  is the "no daemon listening here" case specifically
  (`ConnectionRefused`/`NotFound`/no socket file), never a security
  signal.
  - *Given* no file at the resolved UDS path, and a real `tymuxd` test
    daemon bound to `127.0.0.1:7419`-equivalent TCP loopback (no
    `--addr` given), *When* `dial_channel(None, &resolved_socket_path)`
    runs, *Then* the returned channel successfully calls `ListSessions`
    against the TCP daemon, and exactly one line is printed to stderr
    containing "falling back to TCP loopback".
- A UDS socket that exists and is answering, but rejects this peer at the
  OS level (`PermissionDenied`/`EACCES` — a daemon *is* listening there
  and denied the connect syscall, before `peer_is_authorized` or any
  gRPC-level check ever runs), is a hard error and never falls back to
  TCP (pre-mortem.md P1 #1 fix — the fallback previously treated *any*
  `Err(_)` identically, so a peer-cred-rejected client silently succeeded
  anyway over the unauthenticated TCP path, defeating this feature's
  isolation guarantee).
  - *Given* a UDS socket path where `UnixStream::connect` fails with
    `std::io::ErrorKind::PermissionDenied` (e.g. a real socket file whose
    mode denies the calling uid — see Task 6.4.1d for how the test sets
    this up without needing a second real uid), and a `tymuxd`-equivalent
    TCP listener also reachable at the fallback address, *When*
    `dial_channel(None, &resolved_socket_path)` runs, *Then* it returns
    `Err(_)` whose message matches Task 6.3.1a's documented
    `PermissionDenied` remedy text, and the TCP fallback address is never
    dialed (assert via a TCP-side listener that fails the test if it
    receives a connection, not just by checking the returned `Err`).
- An explicit `--addr` skips UDS entirely, dialing exactly what was
  given.
  - *Given* `--addr http://example.com:1234` and a UDS socket present
    and reachable at the resolved default path, *When*
    `dial_channel(Some("http://example.com:1234".to_string()), &resolved_socket_path)`
    runs, *Then* it attempts to dial `http://example.com:1234` over TCP
    only — the UDS path is never touched (assert via a channel-
    construction-level check, not a live network call to a nonexistent
    `example.com:1234`; e.g. assert the function's UDS-branch code path
    is provably unreached, via a test-only hook or by structuring
    `dial_channel` so the `Some(addr)` branch returns before any UDS
    logic runs at all — confirm this structurally at implementation
    time).
**Files**: `crates/tymux-cli/src/main.rs`, `crates/tymux-cli/Cargo.toml`,
root `Cargo.toml` (workspace deps)

##### Task 6.2.1a: Add `tower`/`hyper-util` dependencies (~2 min)
- Per ADR-003: add `tower = { version = "0.4", features = ["util"] }`
  and `hyper-util = { version = "0.1", features = ["tokio"] }` to the
  workspace `[workspace.dependencies]` in root `Cargo.toml`, then
  `tower = { workspace = true }` / `hyper-util = { workspace = true }`
  to `crates/tymux-cli/Cargo.toml`.
- Files: `Cargo.toml`, `crates/tymux-cli/Cargo.toml`

##### Task 6.2.1b: Implement the UDS connector + `dial_channel` (~5 min)
- Classifies the UDS dial failure before deciding whether to fall back
  (pre-mortem.md P1 #1 fix): "no daemon listening here"
  (`ConnectionRefused`/`NotFound`) still falls back to TCP with the
  existing notice; "a daemon rejected this connect syscall"
  (`PermissionDenied`) is a hard error that never touches the TCP path.
  ```rust
  use hyper_util::rt::TokioIo;
  use tower::service_fn;

  /// Distinguishes "no UDS daemon here" (legitimate, falls back to TCP)
  /// from "a UDS daemon is listening but rejected this peer at the OS
  /// level" (a security signal — must never silently retry over the
  /// unauthenticated TCP path). See pre-mortem.md P1 #1.
  enum UdsDialError {
      PermissionDenied(anyhow::Error),
      Other(anyhow::Error),
  }

  async fn dial_uds(socket_path: &std::path::Path) -> Result<Channel, UdsDialError> {
      let path = socket_path.to_path_buf();
      let connector = service_fn(move |_: http::Uri| {
          let path = path.clone();
          async move {
              let stream = tokio::net::UnixStream::connect(&path).await?;
              Ok::<_, std::io::Error>(TokioIo::new(stream))
          }
      });
      // Placeholder authority — the connector ignores it entirely and
      // always dials socket_path; matches tonic's own documented `uds`
      // client example pattern (ADR-003).
      match tonic::transport::Endpoint::from_static("http://localhost")
          .connect_with_connector(connector)
          .await
      {
          Ok(channel) => Ok(channel),
          Err(e) => {
              // tonic::transport::Error wraps the connector's io::Error
              // as its source; walk the chain to classify by
              // io::ErrorKind rather than treating every failure
              // identically (confirm the exact downcast path against
              // tonic 0.12.3's actual Error/source shape at
              // implementation time — the classification, not this
              // exact traversal, is the load-bearing part).
              let is_permission_denied = std::error::Error::source(&e)
                  .and_then(|src| src.downcast_ref::<std::io::Error>())
                  .map(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
                  .unwrap_or(false);
              if is_permission_denied {
                  UdsDialError::PermissionDenied(e.into())
              } else {
                  UdsDialError::Other(e.into())
              }
          }
      }
  }

  async fn dial_channel(explicit_addr: Option<String>, socket_path: &std::path::Path) -> anyhow::Result<Channel> {
      if let Some(addr) = explicit_addr {
          return Ok(tonic::transport::Endpoint::from_shared(addr)?
              .http2_keep_alive_interval(Duration::from_secs(30))
              .keep_alive_timeout(Duration::from_secs(10))
              .keep_alive_while_idle(true)
              .connect()
              .await?);
      }
      match dial_uds(socket_path).await {
          Ok(channel) => Ok(channel),
          Err(UdsDialError::PermissionDenied(_)) => {
              // A daemon IS listening at socket_path and the kernel
              // denied us the connect() itself — never fall back to the
              // unauthenticated TCP path for this case (pre-mortem.md P1
              // #1). Reuses the same remedy text as the gRPC-level
              // PermissionDenied case (Task 6.3.1a) so a peer denied at
              // accept time and a peer denied by peer_is_authorized see
              // one consistent message, not two near-duplicates.
              anyhow::bail!(
                  "tymuxd rejected this connection: not authorized to access this daemon's \
                   socket (ask the daemon's owner to add you to its configured \
                   --socket-group, or run tymux-cli as the daemon's own OS user)"
              )
          }
          Err(UdsDialError::Other(_)) => {
              eprintln!(
                  "tymux: no reachable Unix socket at {} — falling back to TCP loopback \
                   (deprecated; make sure tymuxd is running)",
                  socket_path.display()
              );
              Ok(tonic::transport::Endpoint::from_static("http://127.0.0.1:7419")
                  .http2_keep_alive_interval(Duration::from_secs(30))
                  .keep_alive_timeout(Duration::from_secs(10))
                  .keep_alive_while_idle(true)
                  .connect()
                  .await?)
          }
      }
  }
  ```
- Replace `run()`'s existing `let endpoint = ...; let channel =
  endpoint.connect().await?;` (`main.rs:346-350`) with:
  ```rust
  let socket_path = cli.socket_path
      .clone()
      .map(PathBuf::from)
      .unwrap_or_else(|| default_uds_socket_path(unsafe { libc::geteuid() }));
  let channel = dial_channel(cli.addr, &socket_path).await?;
  ```
  (`cli.socket_path` resolved by `clap` — including its
  `TYMUXD_SOCKET_PATH` env fallback — per Task 6.1.1c, not by a
  hand-rolled `std::env::args()` scan; add `libc = { workspace = true }`
  to `crates/tymux-cli/Cargo.toml` — cheap addition, already a pinned
  workspace dependency at version `0.2`).
- Files: `crates/tymux-cli/src/main.rs`, `crates/tymux-cli/Cargo.toml`

##### Task 6.2.1c: Tests for the four ACs (~5 min)
- `dial_channel_uses_uds_when_reachable`,
  `dial_channel_falls_back_to_tcp_with_notice_when_uds_unreachable`,
  `dial_channel_hard_errors_and_never_dials_tcp_when_uds_permission_denied`
  (unit-level proof of the classification logic itself — construct a
  `UdsDialError::PermissionDenied` directly, or point `dial_uds` at a
  socket file chmoded to deny the calling uid, per Task 6.4.1d's setup;
  defer the full live-daemon end-to-end proof, including the
  TCP-never-dialed assertion, to Epic 6.4's Task 6.4.1d),
  `dial_channel_skips_uds_entirely_when_addr_explicit` — the first two
  need a real test daemon (defer their live-dial assertions to Epic
  6.4's integration tests if a live daemon isn't practical to spin up
  from this file's existing unit-test setup; the fourth is a pure
  structural/unit-level test provable without any network I/O).
- Files: `crates/tymux-cli/src/main.rs`

### Epic 6.3: Error UX for a peer-cred rejection
**Goal**: A `PermissionDenied` UDS rejection produces a short, actionable
message distinct from a bearer-token `Unauthenticated` rejection —
matching ux.md's case 3.

#### Story 6.3.1: Extend `friendly_message` for `PermissionDenied`
**As a** `tymux-cli` user rejected by the UDS peer-cred check, **I want**
a short, specific message naming the remedy, **so that** I don't see a
raw transport-error dump.
**Acceptance Criteria**:
- A `PermissionDenied` status produces the documented remedy message.
  - *Given* `e = anyhow::Error::from(tonic::Status::permission_denied("not authorized to access this daemon's socket"))`,
    *When* `friendly_message(&e)` runs, *Then* it returns exactly
    `"tymuxd rejected this connection: not authorized to access this daemon's socket (ask the daemon's owner to add you to its configured --socket-group, or run tymux-cli as the daemon's own OS user)"`.
- An `Unauthenticated` status (bearer-token case) is unaffected —
  existing behavior unchanged.
  - *Given* the existing `friendly_message_names_the_remedy_for_unauthenticated_status`
    test's input, *When* `friendly_message` runs, *Then* its output is
    byte-identical to before this story's change.
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 6.3.1a: Add the `PermissionDenied` branch (~3 min)
- In `friendly_message` (`main.rs:266-282`), add an `else if
  status.code() == tonic::Code::PermissionDenied` branch before the
  final generic `status.message().to_string()` fallback, formatting the
  message per the AC above. Include the containerized/namespaced-uid
  caveat from Deployment Guidance as a doc comment above the branch
  (not in the printed message itself — keeping the printed message short
  per ux.md's "not a wall of near-identical variants" guidance).
- Files: `crates/tymux-cli/src/main.rs`

##### Task 6.3.1b: Unit test for the new branch (~2 min)
- `friendly_message_names_the_remedy_for_permission_denied_status`.
- Files: `crates/tymux-cli/src/main.rs`

### Epic 6.4: `tymux-cli` integration tests against a live dual-listener daemon
**Goal**: Prove UDS-first dialing, TCP fallback, and peer-cred rejection
end-to-end against a real `tymuxd`.

#### Story 6.4.1: Accept, fallback, and reject over a real daemon
**As a** `tymux-cli` maintainer, **I want** integration tests proving the
UDS-first path, the TCP-fallback path, and the peer-cred-reject path all
work against a real daemon, **so that** the unit-level dialing logic is
backed by a real end-to-end proof, mirroring `bearer-token-auth`'s own
cross-client integration-test pattern.
**Acceptance Criteria**:
- `tymux ls` against a daemon with only a UDS listener reachable
  succeeds via UDS.
  - *Given* a real `tymuxd` test daemon started with
    `--disable-tcp-loopback` and a known `TYMUXD_SOCKET_PATH`, and
    `tymux-cli` invoked with matching `TYMUXD_SOCKET_PATH` and no
    `--addr`, *When* `tymux ls` runs, *Then* it exits 0 and lists
    sessions correctly.
- `tymux ls` against a daemon with no UDS socket present falls back to
  TCP.
  - *Given* a real `tymuxd` test daemon started bound to TCP loopback
    only (simulate "no UDS" by pointing `tymux-cli`'s
    `TYMUXD_SOCKET_PATH` at a path with nothing bound there, while the
    daemon itself still creates its own UDS socket elsewhere — i.e.
    deliberately mismatch the client's expected path from the daemon's
    actual one to force the fallback branch), *When* `tymux ls` runs
    with no `--addr`, *Then* it exits 0, lists sessions correctly via
    the TCP fallback, and stderr contains "falling back to TCP loopback".
- A client with a mismatched uid (simulated per pitfalls.md §7's
  documented CI-privilege split — see Task 6.4.1c) is rejected with the
  documented remedy message.
- A client that reaches a real UDS listener but is denied at the OS level
  (`EACCES`/`PermissionDenied`, simulated via socket-file permissions the
  test process itself controls — no second real uid needed, unlike the
  AC above) hard-errors with the documented remedy message and never
  ends up connected via TCP fallback, even when a real TCP-loopback
  listener is reachable at the fallback address (pre-mortem.md P1 #1 —
  this AC, unlike the one above, runs unconditionally in CI; see Task
  6.4.1d).
**Files**: `crates/tymux-cli/tests/` (new integration test file, e.g.
`uds_integration.rs`), or `crates/tymux-e2e` if that's this repo's
established home for cross-binary integration tests (confirm at
implementation time via `ls crates/tymux-e2e`)

##### Task 6.4.1a: Test-harness helper spawning a real `tymuxd` with UDS config (~5 min)
- Mirror `clients/go/integration/integration_test.go`'s `startDaemon`
  shape (spawn the real binary, wait for "tymuxd listening" on stdout,
  set `TYMUXD_SOCKET_PATH`/`TYMUXD_SOCKET_GROUP`/
  `TYMUXD_DISABLE_TCP_LOOPBACK` as needed per test).
- Files: `crates/tymux-cli/tests/uds_integration.rs` (new)

##### Task 6.4.1b: The accept + fallback integration tests (~5 min)
- `tymux_ls_succeeds_via_uds_when_tcp_disabled`,
  `tymux_ls_falls_back_to_tcp_and_logs_notice_when_uds_unreachable`.
- Files: `crates/tymux-cli/tests/uds_integration.rs`

##### Task 6.4.1c: The reject integration test (~5 min)
- Per pitfalls.md §7: if CI can run as root/`CAP_SETUID`, spawn the
  client subprocess with a different real uid via
  `std::os::unix::process::CommandExt::uid` and assert the documented
  `PermissionDenied` remedy message on stderr; otherwise (documented
  fallback, see Unresolved Questions), this test is skipped/marked
  `#[ignore]` with a comment pointing at Story 3.1.2's unit-level
  coverage of `peer_is_authorized` (the actual decision logic — Story
  3.2.1a's `PreAuthorizedUnixStream` invokes it once at accept time, and
  Story 3.2.1b's interceptor does no decision logic of its own) as the
  accepted substitute proof.
- Files: `crates/tymux-cli/tests/uds_integration.rs`

##### Task 6.4.1d: The OS-level-`PermissionDenied`-never-falls-back-to-TCP integration test (~5 min)
- `tymux_ls_hard_errors_and_never_falls_back_to_tcp_when_uds_permission_denied`
  — this is the security-critical regression test pre-mortem.md P1 #1
  requires, and unlike Task 6.4.1c it is **not** CI-skip-only: it tests
  error *classification* in the client, not real cross-uid rejection, so
  it needs no root/`CAP_SETUID`. Setup: start a real `tymuxd` bound to
  TCP loopback only, plus separately create a plain `UnixListener` (or a
  bare socket file) at the path `tymux-cli` will resolve as its UDS
  target, then `chmod` that socket file to `0o000` — even the owning
  test process itself then gets `EACCES` on `connect()`, so no second
  real uid is required (matching pre-mortem.md P1 #1's suggested setup
  exactly). Assert: `tymux ls` (no `--addr`) exits non-zero, stderr
  contains the documented `PermissionDenied` remedy text, and the
  TCP-loopback daemon's connection count/accept log shows it was never
  dialed (not just that the client's returned error looked right —
  assert the negative on the TCP side directly, e.g. via a counting
  wrapper listener or the daemon's own connection-accepted log line).
- Files: `crates/tymux-cli/tests/uds_integration.rs`

---

## Phase 7: clients/go — UDS dialing

### Epic 7.1: `udsdialer` package — socket-path resolution + HTTP client construction
**Goal**: A single, reusable package (mirroring `authinterceptor`'s
existing shape) provides both the default-path algorithm and a
UDS-dialing `*http.Client` constructor.

#### Story 7.1.1: `DefaultSocketPath`/`ResolveSocketPath`
**As a** Go client author, **I want** the same default-path algorithm
`tymuxd` uses, **so that** `clients/go` connects to the right socket with
zero configuration.
**Acceptance Criteria**: identical in shape to Stories 1.1.1/1.1.2 (five
+ four cases), Go-typed (`func DefaultSocketPath(uid int) string`,
`func ResolveSocketPath(uid int) string` reading `os.Getenv`/
`TYMUXD_SOCKET_PATH`) — not re-listed verbatim; substitute
`clients/go/udsdialer/udsdialer.go` as the implementation location and
Go's `os.Getuid()` for the uid input.
**Files**: `clients/go/udsdialer/udsdialer.go` (new)

##### Task 7.1.1a: Implement `DefaultSocketPath`/`ResolveSocketPath` (~5 min)
- ```go
  package udsdialer

  import (
  	"os"
  	"path/filepath"
  	"strconv"
  )

  // DefaultSocketPath mirrors tymuxd's auth::default_uds_socket_path and
  // tymux-cli's copy of the same algorithm byte-for-byte — see
  // project_plans/unix-socket-auth/implementation/plan.md Pattern
  // Decisions row 10. Any change must be mirrored in all four
  // implementations.
  func DefaultSocketPath(uid int) string {
  	if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
  		return filepath.Join(dir, "tymuxd", "tymuxd.sock")
  	}
  	base := os.Getenv("TMPDIR")
  	if base == "" {
  		base = "/tmp"
  	}
  	return filepath.Join(base, "tymuxd-"+strconv.Itoa(uid), "tymuxd.sock")
  }

  // ResolveSocketPath applies the TYMUXD_SOCKET_PATH override, matching
  // resolve_uds_socket_path's flag-beats-env precedence — Go clients
  // have no CLI-flag layer of their own in this package, so only the
  // env var is checked here; callers with their own flag parsing (e.g.
  // a future tymux-go-cli) should check their flag first and fall back
  // to this function.
  func ResolveSocketPath(uid int) string {
  	if p := os.Getenv("TYMUXD_SOCKET_PATH"); p != "" {
  		return p
  	}
  	return DefaultSocketPath(uid)
  }
  ```
- Files: `clients/go/udsdialer/udsdialer.go`

##### Task 7.1.1b: Table-driven tests reading the shared fixture file (~5 min)
- `TestDefaultSocketPath` (table-driven: XDG set, XDG unset+TMPDIR set,
  both unset, XDG empty-string, uid-scoping distinctness — same five
  cases as Task 1.1.1b), `TestResolveSocketPath` (env override, no
  override falls back). Both read
  `project_plans/unix-socket-auth/socket-path-fixtures.json`'s
  `default_path_cases`/`resolve_path_cases` arrays (`os.ReadFile` +
  `encoding/json`, both stdlib — no new dependency) instead of
  hand-typing a Go-native table (architecture-review.md's
  test-duplication Concern fix).
- Files: `clients/go/udsdialer/udsdialer_test.go` (new),
  `project_plans/unix-socket-auth/socket-path-fixtures.json`

#### Story 7.1.2: `DialUnixHTTPClient` — UDS-dialing `*http.Client` constructor
**As a** Go client author, **I want** a one-call constructor for an
`*http.Client` that dials a Unix socket over h2c, **so that** every
example/test doesn't hand-roll the `http2.Transport`/`DialTLSContext`
wiring separately.
**Acceptance Criteria**:
- The constructed client successfully round-trips a `ListSessions` call
  against a real `tymuxd` UDS listener.
  - *Given* a real `tymuxd` test daemon with a UDS listener bound at a
    known path, *When* a `tymuxv1connect.TymuxServiceClient` is built
    from `udsdialer.DialUnixHTTPClient(socketPath)` and calls
    `ListSessions`, *Then* the call succeeds.
**Files**: `clients/go/udsdialer/udsdialer.go`

##### Task 7.1.2a: Implement `DialUnixHTTPClient` (~5 min)
- ```go
  func DialUnixHTTPClient(socketPath string) *http.Client {
  	return &http.Client{
  		Transport: &http2.Transport{
  			AllowHTTP: true,
  			DialTLSContext: func(ctx context.Context, _, _ string, _ *tls.Config) (net.Conn, error) {
  				return (&net.Dialer{}).DialContext(ctx, "unix", socketPath)
  			},
  		},
  	}
  }
  ```
  (Mirrors `examples/list-sessions/main.go`'s existing `newClient`
  shape exactly, replacing `net.Dial(network, addr)` with a fixed
  `"unix"` dial — the same seam `research/stack.md` §4 identified.)
- Files: `clients/go/udsdialer/udsdialer.go`

##### Task 7.1.2b: Integration test against a real daemon (~5 min)
- `TestDialUnixHTTPClientRoundTripsListSessions`, reusing this package's
  own `startDaemon`-equivalent (or, simpler: reuse
  `clients/go/integration`'s existing `startDaemonOn`, parameterized for
  a UDS path once Epic 7.2 extends it — sequence this test after Task
  7.2.1a if the harness extension lands there instead).
- Files: `clients/go/udsdialer/udsdialer_test.go`

### Epic 7.2: Wire UDS dialing into examples + the integration-test harness
**Goal**: `clients/go`'s examples and integration-test harness gain a
UDS-first dialing path, matching `tymux-cli`'s shape.

#### Story 7.2.1: Extend `startDaemonOn`/`newClient` for UDS
**As a** `clients/go` maintainer, **I want** the integration-test harness
to be able to start a `tymuxd` with a known UDS path and dial it, **so
that** Epic 7.3's accept/reject tests have real infrastructure to run
against.
**Acceptance Criteria**:
- The harness can start a daemon with a fixed `TYMUXD_SOCKET_PATH` and
  connect a client to it.
  - *Given* `startDaemonWithUDS(t, socketPath)` (new helper), *When* it
    returns, *Then* the returned client (via `udsdialer.DialUnixHTTPClient`)
    can successfully call `ListSessions`.
- The example's UDS-first dial (Task 7.2.1b) classifies the dial error
  before falling back to TCP, mirroring `tymux-cli`'s `dial_channel` fix
  (pre-mortem.md P1 #1): `syscall.ENOENT`/`syscall.ECONNREFUSED` ("no
  daemon listening here") still falls back to TCP with the existing
  notice; `syscall.EACCES` ("a daemon is listening but denied this
  connect") is a hard error that never dials the TCP fallback address.
  - *Given* a UDS socket path where dialing fails with an error for
    which `errors.Is(err, syscall.EACCES)` is true, *When* the UDS-first
    dial helper runs, *Then* it returns an error containing the
    documented `PermissionDenied` remedy text and never attempts the TCP
    fallback dial.
**Files**: `clients/go/integration/integration_test.go`

##### Task 7.2.1a: Add `startDaemonWithUDS` (~5 min)
- Mirror `startDaemonWithToken`'s existing shape (`integration_test.go:347-350`):
  sets `TYMUXD_SOCKET_PATH=<t.TempDir()>/tymuxd.sock` in the spawned
  process's env, returns the socket path (not an HTTP addr) for the
  caller to pass into `udsdialer.DialUnixHTTPClient`.
- Files: `clients/go/integration/integration_test.go`

##### Task 7.2.1b: Update `examples/list-sessions/main.go` to try UDS first (~5 min)
- `newClient`'s call site in `main()` (line 64) gains a UDS-first
  attempt via `udsdialer.ResolveSocketPath(os.Getuid())` before falling
  back to today's hardcoded `"http://127.0.0.1:7419"` — mirrors
  `tymux-cli`'s `dial_channel` shape, logging the same fallback notice
  to stderr on failure, **and mirrors its error classification too**
  (pre-mortem.md P1 #1 — the blanket "any dial error falls back to TCP"
  behavior must not ship in the Go client either):
  ```go
  conn, err := net.Dial("unix", socketPath)
  if err != nil {
      if errors.Is(err, syscall.EACCES) {
          // A daemon IS listening at socketPath and the kernel denied
          // us the connect() itself -- never fall back to the
          // unauthenticated TCP path for this case.
          return nil, fmt.Errorf(
              "tymuxd rejected this connection: not authorized to access this daemon's " +
                  "socket (ask the daemon's owner to add you to its configured " +
                  "--socket-group, or run this client as the daemon's own OS user)")
      }
      // syscall.ENOENT ("no socket file") / syscall.ECONNREFUSED ("file
      // present, nothing listening") and anything else: no daemon here,
      // legitimate to fall back.
      fmt.Fprintf(os.Stderr,
          "no reachable Unix socket at %s -- falling back to TCP loopback "+
              "(deprecated; make sure tymuxd is running)\n", socketPath)
      return newClient("http://127.0.0.1:7419"), nil
  }
  conn.Close() // probe only; DialUnixHTTPClient/http2.Transport does its own dialing per-request
  return udsdialer.DialUnixHTTPClient(socketPath), nil
  ```
  (confirm the exact probe-vs-lazy-dial shape against `http2.Transport`'s
  actual dial timing at implementation time — the classification, not
  this exact probing strategy, is the load-bearing part).
- Files: `clients/go/examples/list-sessions/main.go`

### Epic 7.3: `clients/go` integration tests — accept/reject over real UDS
**Goal**: Prove accept (matching uid) and reject (mismatched uid, per
pitfalls.md §7's CI-privilege split) over a real UDS connection from Go.

#### Story 7.3.1: Accept and reject integration tests
**As a** `clients/go` maintainer, **I want** integration tests proving
both the accept and reject paths over a real UDS socket, **so that**
Go's UDS dialing is proven end-to-end, matching `bearer-token-auth`'s own
per-client integration-test precedent.
**Acceptance Criteria**:
- A same-uid client succeeds.
  - *Given* `startDaemonWithUDS` from Story 7.2.1, *When*
    `ListSessions` is called via `udsdialer.DialUnixHTTPClient`, *Then*
    it succeeds.
- A mismatched-uid client is rejected with `connect.CodePermissionDenied`
  (connect-go's mapping of gRPC `PermissionDenied`), if and only if CI
  can run the client subprocess as a different real uid (pitfalls.md
  §7) — otherwise this AC is covered at the unit level only (Story 3.1.2
  already proves the `peer_is_authorized` decision function; this AC's
  integration variant is the accepted degradation).
- A client that reaches a real UDS listener but is denied at the OS level
  (`EACCES`, via a socket file the test process chmods to deny itself —
  no second real uid needed) hard-errors with the documented remedy and
  never ends up connected via TCP fallback, even with a real TCP-loopback
  daemon reachable at the fallback address (pre-mortem.md P1 #1 — runs
  unconditionally in CI, unlike the AC above; see Task 7.3.1c).
**Files**: `clients/go/integration/integration_test.go`

##### Task 7.3.1a: `TestListSessionsSucceedsOverUDS` (~5 min)
- Files: `clients/go/integration/integration_test.go`

##### Task 7.3.1b: `TestListSessionsRejectsOverUDSWithMismatchedUID` (~5 min)
- Guarded per pitfalls.md §7: check CI privilege (e.g. `os.Geteuid() ==
  0`) at test start and `t.Skip(...)` with a clear reason if not
  available, rather than failing CI on an environment this test
  structurally cannot run in. Uses `exec.Cmd.SysProcAttr.Credential` (Go's
  equivalent of Rust's `CommandExt::uid`) to spawn the *client* half as a
  different uid when privilege is available.
- Files: `clients/go/integration/integration_test.go`

##### Task 7.3.1c: `TestListSessionsHardErrorsAndNeverFallsBackToTCPOnEACCES` (~5 min)
- Not CI-skip-gated (distinct from Task 7.3.1b) — tests error
  classification, which needs no elevated privilege. Start a real
  `tymuxd` bound to TCP loopback only, create a plain socket file at the
  UDS path the client will resolve, `chmod` it `0o000`, then assert the
  client's UDS-first dial (Task 7.2.1b) returns the documented
  `PermissionDenied` remedy and the TCP daemon's connection count/accept
  log proves it was never dialed.
- Files: `clients/go/integration/integration_test.go`

---

## Phase 8: clients/ts — UDS dialing

### Epic 8.1: UDS transport — spike, then implementation
**Goal**: Confirm `@connectrpc/connect-node`'s `nodeOptions.createConnection`
pass-through actually reaches `http2.connect()` (build-vs-buy.md §4's
flagged, unverified-end-to-end risk), then build the real transport
factory.

#### Story 8.1.1: Spike — confirm `createConnection` pass-through against a real tonic h2c server
**As a** `clients/ts` maintainer, **I want** to confirm
`createGrpcTransport({ nodeOptions: { createConnection } })` actually
dials a Unix socket before committing further TS work to this approach,
**so that** a wrong assumption here doesn't waste the rest of this
phase's implementation effort.
**Acceptance Criteria**:
- A `createGrpcTransport` built with a `createConnection` callback
  returning a UDS-connected `net.Socket` successfully completes a real
  `ListSessions` RPC against a live `tymuxd` UDS listener.
  - *Given* a real `tymuxd` test daemon with a UDS listener at a known
    path, *When* a throwaway spike script builds
    `createGrpcTransport({ baseUrl: "http://localhost", nodeOptions: { createConnection: () => net.connect({ path: socketPath }) } })`
    and calls `listSessions`, *Then* the call succeeds within a bounded
    timeout.
- **If the spike fails**: fall back to `createConnectTransport` (plain
  HTTP/1.1 Connect protocol) with a custom `Agent`, per build-vs-buy.md
  §4's documented fallback — this AC's failure path is itself an
  acceptable outcome of this story, not a blocker, since the next
  story's implementation task is written to accommodate either result
  (see Task 8.1.2a's note).
**Files**: none persisted — this is a spike; findings feed Task 8.1.2a
directly (delete the throwaway script once confirmed, or keep it as
`clients/ts/examples/uds-spike.ts` if useful as a minimal reference,
implementer's discretion)

##### Task 8.1.1a: Run the spike against a real local `tymuxd` (~5 min)
- Files: none (or `clients/ts/examples/uds-spike.ts`, temporary)

#### Story 8.1.2: `createUdsGrpcTransport` — the real implementation
**As a** `clients/ts` user, **I want** a one-call factory for a
UDS-dialing gRPC transport, **so that** examples/tests don't hand-roll
the `createConnection`/`Agent` wiring separately.
**Acceptance Criteria**:
- The constructed transport successfully round-trips `listSessions`
  against a real `tymuxd` UDS listener.
  - *Given* a real `tymuxd` test daemon with a UDS listener at a known
    path, *When* a client built from
    `createUdsGrpcTransport(socketPath)` calls `listSessions`, *Then*
    the call succeeds.
**Files**: `clients/ts/examples/client.ts`

##### Task 8.1.2a: Implement `createUdsGrpcTransport` (~5 min)
- If Story 8.1.1's spike confirmed the `createConnection` pass-through
  works:
  ```ts
  import * as net from "node:net";

  export function createUdsGrpcTransport(socketPath: string, token?: string) {
    const transport = createGrpcTransport({
      baseUrl: "http://localhost", // placeholder authority — createConnection ignores it
      nodeOptions: { createConnection: () => net.connect({ path: socketPath }) },
      interceptors: [authInterceptor(token)],
    });
    return createClient(TymuxService, transport);
  }
  ```
  If the spike instead confirmed the fallback is needed, implement the
  `createConnectTransport` + custom `Agent` variant per build-vs-buy.md
  §4 with an equivalent signature — the function's public shape
  (`createUdsGrpcTransport(socketPath, token?)` returning a usable
  client) stays the same either way, so nothing downstream (Epic
  8.2/8.3) needs to change based on which path was taken.
- Files: `clients/ts/examples/client.ts`

### Epic 8.2: Socket-path resolution + UDS-first `tymuxClient`
**Goal**: `clients/ts` computes the same default path and dials UDS
first, matching `tymux-cli`/`clients/go`'s shape.

#### Story 8.2.1: `defaultSocketPath`/`resolveSocketPath`
**As a** TS client author, **I want** the same default-path algorithm,
**so that** `clients/ts` connects to the right socket with zero
configuration.
**Acceptance Criteria**: identical in shape to Stories 1.1.1/1.1.2/7.1.1
— not re-listed verbatim; TS-typed
(`function defaultSocketPath(uid: number): string`,
`function resolveSocketPath(uid: number): string`), using
`process.env`/`process.getuid!()` (Node's POSIX-only accessor, safe here
since this whole feature is Unix-only per requirements.md's explicit
Windows exclusion).
**Files**: `clients/ts/examples/client.ts`

##### Task 8.2.1a: Implement both functions (~5 min)
- ```ts
  import * as path from "node:path";

  // Mirrors tymuxd's auth::default_uds_socket_path /
  // resolve_uds_socket_path byte-for-byte — see plan.md Pattern
  // Decisions row 10. Any change must be mirrored in all four
  // implementations.
  export function defaultSocketPath(uid: number): string {
    const xdg = process.env.XDG_RUNTIME_DIR;
    if (xdg) return path.join(xdg, "tymuxd", "tymuxd.sock");
    const base = process.env.TMPDIR || "/tmp";
    return path.join(base, `tymuxd-${uid}`, "tymuxd.sock");
  }

  export function resolveSocketPath(uid: number): string {
    return process.env.TYMUXD_SOCKET_PATH || defaultSocketPath(uid);
  }
  ```
- Files: `clients/ts/examples/client.ts`

##### Task 8.2.1b: Tests reading the shared fixture file (~5 min)
- Same five + four cases as Tasks 1.1.1b/1.1.2b/7.1.1b, using `tsx --test`
  (matching this package's existing `npm test` script). Reads
  `project_plans/unix-socket-auth/socket-path-fixtures.json`'s
  `default_path_cases`/`resolve_path_cases` arrays
  (`fs.readFileSync`+`JSON.parse`, both stdlib — no new package needed)
  instead of hand-typing a TS-native table (architecture-review.md's
  test-duplication Concern fix).
- Files: `clients/ts/test/socketpath.test.ts` (new),
  `project_plans/unix-socket-auth/socket-path-fixtures.json`

#### Story 8.2.2: `tymuxClient` tries UDS first
**As a** `clients/ts` example/test author, **I want** `tymuxClient()`'s
default behavior to try UDS before TCP, **so that** every example gets
the more secure default with no per-call-site change.
**Acceptance Criteria**:
- With no explicit `baseUrl` override, `tymuxClient()` dials UDS when
  reachable.
  - *Given* a real `tymuxd` test daemon with a UDS listener at the
    resolved default path, *When* `tymuxClient()` (no args) calls
    `listSessions`, *Then* it succeeds via the UDS transport (not TCP).
- A logged fallback occurs when UDS is unreachable.
  - *Given* no UDS socket at the resolved path, and a real `tymuxd` test
    daemon reachable over TCP loopback, *When* `tymuxClient()` calls
    `listSessions`, *Then* it succeeds via the TCP fallback and exactly
    one console notice is printed containing "falling back to TCP
    loopback".
- An OS-level `EACCES` on the UDS dial is a hard error, never a TCP
  fallback (pre-mortem.md P1 #1 — mirrors `tymux-cli`'s `dial_channel`
  and `clients/go`'s fix; the blanket "any error falls back" behavior
  must not ship in the TS client either).
  - *Given* a UDS socket path where the connect attempt fails with an
    error whose `.code === "EACCES"` (Node's connect-error convention),
    *When* `tymuxClient()` calls `listSessions`, *Then* it rejects with
    the documented `PermissionDenied` remedy text and never attempts the
    TCP fallback dial.
**Files**: `clients/ts/examples/client.ts`

##### Task 8.2.2a: Update `tymuxClient` to try UDS first (~5 min)
- Classify the UDS dial error before falling back, mirroring
  `tymux-cli`'s `dial_channel`/`clients/go`'s equivalent fix:
  ```ts
  try {
    return await connectViaUds(socketPath);
  } catch (err) {
    if ((err as NodeJS.ErrnoException)?.code === "EACCES") {
      // A daemon IS listening at socketPath and the kernel denied us
      // the connect() itself -- never fall back to the unauthenticated
      // TCP path for this case (pre-mortem.md P1 #1).
      throw new Error(
        "tymuxd rejected this connection: not authorized to access this daemon's socket " +
          "(ask the daemon's owner to add you to its configured --socket-group, or run " +
          "this client as the daemon's own OS user)",
      );
    }
    // ENOENT ("no socket file") / ECONNREFUSED ("file present, nothing
    // listening") and anything else: no daemon here, legitimate to fall
    // back.
    console.error(
      `no reachable Unix socket at ${socketPath} — falling back to TCP loopback ` +
        `(deprecated; make sure tymuxd is running)`,
    );
    return connectViaTcp("http://127.0.0.1:7419");
  }
  ```
  (confirm the exact error shape connect-node's transport actually
  surfaces for a UDS `EACCES` at implementation time — the
  classification, not this exact try/catch shape, is the load-bearing
  part).
- Files: `clients/ts/examples/client.ts`

##### Task 8.2.2b: Unit/integration tests for both ACs (~5 min)
- Files: `clients/ts/test/integration.test.ts`

### Epic 8.3: `clients/ts` integration tests — accept/reject over real UDS
**Goal**: Prove accept and reject over a real UDS connection from
Node, matching Go's/tymux-cli's per-client integration-test precedent.

#### Story 8.3.1: Accept and reject integration tests
**Acceptance Criteria**:
- A same-uid client (the only uid available to a Node test process
  without root, matching pitfalls.md §7's constraint) succeeds.
- A mismatched-uid client is rejected with the connect-node mapping of
  `PermissionDenied`, gated on the same root/`CAP_SETUID` CI-privilege
  check as Go's Task 7.3.1b (Node's equivalent:
  `child_process.spawn(..., { uid: <different uid> })`), or covered at
  the unit level only if unavailable.
- A client that reaches a real UDS listener but is denied at the OS level
  (`EACCES`, via a socket file the test process chmods to deny itself —
  no elevated privilege needed) hard-errors with the documented remedy
  and never ends up connected via TCP fallback, even with a real
  TCP-loopback daemon reachable at the fallback address (pre-mortem.md P1
  #1 — runs unconditionally in CI, unlike the AC above; see Task 8.3.1d).
**Files**: `clients/ts/test/integration.test.ts`

##### Task 8.3.1a: `startDaemon` extension for a fixed UDS path (~5 min)
- Extend `clients/ts/test/daemon.ts`'s `StartDaemonOptions` with an
  optional `socketPath` field, setting `TYMUXD_SOCKET_PATH` in the
  spawned process's env, mirroring Go's `startDaemonWithUDS` (Task
  7.2.1a).
- Files: `clients/ts/test/daemon.ts`

##### Task 8.3.1b: The accept integration test (~5 min)
- Files: `clients/ts/test/integration.test.ts`

##### Task 8.3.1c: The reject integration test, CI-privilege-gated (~5 min)
- Skip with a clear reason if `process.getuid!() !== 0`, matching Go's
  Task 7.3.1b's guard.
- Files: `clients/ts/test/integration.test.ts`

##### Task 8.3.1d: The `EACCES`-hard-errors-and-never-falls-back-to-TCP test (~5 min)
- Not CI-privilege-gated (distinct from Task 8.3.1c) — tests error
  classification, which needs no elevated privilege. Start a real
  `tymuxd` bound to TCP loopback only, create a plain socket file at the
  UDS path `tymuxClient()` will resolve, `chmod` it `0o000` (via
  `fs.chmodSync`), then assert `listSessions()` rejects with the
  documented `PermissionDenied` remedy text and the TCP daemon's
  connection count/accept log proves it was never dialed.
- Files: `clients/ts/test/integration.test.ts`

---

## Phase 9: Documentation

**Goal**: Get the one remaining Deployment Guidance caveat that is
currently doc-comment-only (the containerized/bind-mounted-socket
uid-mismatch note) in front of an operator before this ships to a
multi-user-host audience — closing the last open item in `design/ux.md`'s
Gap 1 and `validation.md`'s S11-AC2 row. Independent of every other
phase — pure prose, no code dependency — and can run any time once this
project's flag names/error text are stable (i.e. any time after Phase 6's
`--socket-path` wiring and Task 6.3.1a's `PermissionDenied` message are
written, so the doc can quote them verbatim rather than guessing ahead).

### Epic 9.1: Multi-user / shared-host deployment documentation

#### Story 9.1.1: README section for the containerized-uid-mismatch caveat
**As an** operator evaluating this feature for a shared, multi-user host
(including a containerized deployment with a bind-mounted `tymuxd`
socket), **I want** the containerized/bind-mounted-socket uid-mismatch
caveat documented somewhere I'll actually see it before I deploy, **so
that** I don't discover it only after an already-confusing
`PermissionDenied` rejection, or by reading a Rust doc comment in source
I was never going to open.
**Acceptance Criteria**:
- `README.md` gains a "Multi-user / shared-host deployment" section
  (new — `README.md` today has no section covering multi-user
  deployment topics; the closest existing content is the "Loopback-only
  trust model" bullet under Known Limitations, which this feature's UDS
  listener supersedes but does not itself rewrite, since updating that
  bullet's wording is not part of this caveat and is left for whoever
  finishes the TCP-removal follow-up project).
  - *Given* the merged README, *When* a reader looks for multi-user/
    shared-host guidance, *Then* they find a section stating, in plain
    language (no `SO_PEERCRED`/"peer credential" jargon required to
    understand it): a client connecting through a bind-mounted host
    socket from inside a container may present a different uid at
    `peer_cred()` time than what `id -u` reports inside that container,
    because the kernel reports the *host-mapped* uid, not the
    container-local one — so `--socket-path`/`--socket-group` access
    decisions are made against the host uid, which can surprise an
    operator who only checked the container-local one.
- The new section is cross-referenced from both `--socket-path`'s
  (Task 6.1.1c) and `--socket-group`'s (Task 1.2.1a) doc-comment/help
  text, so a reader of either flag's `--help`/doc comment has a pointer
  to the fuller explanation rather than only the one-line note already
  in each doc comment.
  - *Given* `crates/tymux-cli/src/main.rs`'s `--socket-path` doc comment
    and `crates/tymuxd/src/auth.rs`'s `resolve_socket_group_name` doc
    comment, *When* either is read, *Then* each points to this README
    section by name (not to a nonexistent `docs/deployment.md`, which an
    earlier draft of Task 6.1.1c mistakenly referenced before this task
    existed).
**Files**: `README.md`, `crates/tymux-cli/src/main.rs` (cross-reference
only — the doc-comment text itself is Task 6.1.1c's own responsibility),
`crates/tymuxd/src/auth.rs` (cross-reference only — Task 1.2.1a's own
responsibility)

##### Task 9.1.1a: Add the "Multi-user / shared-host deployment" section to `README.md` (~5 min)
- Insert a new `## Multi-user / shared-host deployment` section — a
  natural placement is directly after the existing `## Known
  Limitations` section (`README.md:91-107`) and before `## Accessibility`
  (`README.md:109`), so it reads alongside the other "things to know
  before you rely on this" content rather than buried in `## Running it`
  or `## Dev setup`.
- Content: plain-language explanation of the containerized/bind-mounted-
  socket uid-mismatch caveat (verbatim substance of pitfalls.md §4 and
  Deployment Guidance's third bullet), phrased for an operator, not a
  contributor — no `SO_PEERCRED`/`peer_cred()`/raw uid-number jargon
  required to understand the practical implication ("your container's
  `id -u` may not be what `tymuxd` sees").
- Files: `README.md`

##### Task 9.1.1b: Cross-reference the new section from `--socket-path`/`--socket-group` help text (~2 min)
- In `crates/tymux-cli/src/main.rs`'s `--socket-path` doc comment (Task
  6.1.1c) and `crates/tymuxd/src/auth.rs`'s `resolve_socket_group_name`
  doc comment (Task 1.2.1a): confirm each already names this README
  section (both were updated in this repair pass to point here instead
  of a nonexistent `docs/deployment.md`); this task is the
  implementation-time check that the cross-reference still resolves
  (i.e. the section exists under the exact heading Task 9.1.1a used) once
  both files are actually written.
- Files: `crates/tymux-cli/src/main.rs`, `crates/tymuxd/src/auth.rs`
  (verification only — no new doc-comment prose beyond what Tasks 1.2.1a/
  6.1.1c already specify)
