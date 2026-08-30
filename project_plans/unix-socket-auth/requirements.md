# Requirements: unix-socket-auth

**Date**: 2026-08-29
**Type**: feature addition (with a deprecation component)
**Complexity**: 4 — high-stakes / cross-cutting

## Problem Statement

`tymuxd`'s loopback TCP bind (`127.0.0.1:7419`, today's default) requires no
authentication at all — any local process, running as any local OS user, can
`CreateSession` (spawn an arbitrary command) or `Attach`/`CapturePane`/
`KillSession` against any `pane_id`. On a single-user machine this is an
accepted risk (see `bearer-token-auth`'s baseline). On a shared host —
multiple OS user accounts on the same box — it is not: any local user can
hijack any other local user's `tymuxd` sessions with zero credential, since
TCP loopback carries no identity information at all. `bearer-token-auth`
(shipping as PR #43) deliberately left this gap open, rejecting
"always-required auth including loopback" as breaking today's zero-config
local UX for a bind that was assumed unreachable off-host — that assumption
doesn't hold on a shared host.

## Baseline

Today, `tymuxd` binds only to TCP loopback by default. Any process that can
open a TCP connection to `127.0.0.1:7419` — regardless of which OS user owns
that process — gets full, unauthenticated access to every RPC. There is no
mechanism to distinguish "my own client" from "another local user's client."

## Users / Consumers

Same as `bearer-token-auth`: `tymux-cli` (`crates/tymux-cli`), `clients/go`,
`clients/ts` (Node-targeted via `@connectrpc/connect-node`, not a browser
build — confirmed no browser-UDS constraint applies), and the in-flight
`stapler-squad` `BackendTymux` integration. New: any deployment on a
multi-user host (shared dev boxes, CI runners, shared servers) where local
user isolation now matters.

## Success Metrics

- `tymuxd` listens on a Unix domain socket by default, alongside the
  existing TCP loopback bind (both-by-default — not opt-in, not a
  replacement at v1).
- The UDS socket file is created with owner-only permissions (mode `0600`)
  by default; a connecting process from a different uid is rejected before
  reaching any RPC handler.
- A configurable group grants access to specific other local users via the
  socket's group bit (not just exact-uid match) — e.g. a shared service
  account scenario where a small team needs access without being the same
  uid.
- `tymuxd` reads the connecting peer's actual credentials (uid/gid/pid) via
  `SO_PEERCRED` (Linux) — available through `tokio::net::UnixStream::
  peer_cred()`, already covered by this repo's existing `tokio = { features
  = ["full"] }` dependency, no new crate needed — and enforces the
  uid/group check from that, not from any client-supplied claim.
- `tymux-cli`, `clients/go`, `clients/ts` can all connect over the UDS path
  by default when running on the same host as `tymuxd`, with integration
  tests proving both the accept (matching uid/group) and reject
  (non-matching uid) paths in each client, mirroring `bearer-token-auth`'s
  own cross-client integration-test pattern.
- The existing unauthenticated TCP loopback bind is marked deprecated: a
  `tracing::warn!` on startup when it's reachable, and a documented removal
  plan (see Risk Control) — but is not removed in this project's scope.
  **Caveat, stated explicitly so it isn't over-read**: this default
  ("both by default") does not by itself close the shared-host isolation
  gap the Problem Statement describes — TCP loopback remains reachable
  and unauthenticated until an operator opts into
  `--disable-tcp-loopback`. `design/ux.md`'s Surface 1 puts this caveat in
  the same sentence as the deprecation notice for exactly this reason;
  this metric is "the mechanism to close the gap now exists and is
  discoverable," not "the gap is closed by default."
- Loopback-bound `tymuxd` on a single-user machine continues to work with
  zero required config change: the UDS socket is created automatically at a
  default path, no flag needed for the common case.

## Appetite

Large (3–6 weeks). **Rationale**: this is security-hardening work with no
external deadline on a solo-maintainer repo — the cost that matters is
review/context-switch overhead, not calendar time. Group-based access,
the TCP-loopback deprecation off-switch, and kernel-verified peer identity
are three small, tightly-coupled pieces of the same mechanism (they share
one socket-creation code path and one interceptor); planning and
reviewing them together in one Large-appetite pass was judged cheaper
than three separate planning/review cycles for closely related work, at
the cost of a bigger single diff. See Alternatives Considered below for
the rejected smaller cut.

## Constraints

None beyond the existing single-maintainer, solo-dev cadence of this repo.
No external deadline. Linux is the primary target platform (`SO_PEERCRED`
is Linux-specific — see Feasibility Risks for macOS's `LOCAL_PEERCRED`
equivalent).

**Merge-order dependency**: this project extends `crates/tymuxd/src/auth.rs`,
which the sibling `bearer-token-auth` feature (PR #43) introduces but which
is not yet merged to `main` (confirmed at planning time: `git log
origin/main..origin/feature/bearer-token-auth --oneline` returns 12
commits) — implementation cannot start until that merge lands. See
`implementation/plan.md`'s Prerequisites section.

## Non-functional Requirements

- **Performance SLO**: peer-credential check happens once per connection
  (at accept time), not per-RPC — no measurable per-call overhead.
- **Scalability**: not applicable — same single-daemon-instance shape as
  `bearer-token-auth`.
- **Security classification**: this is a security boundary. Peer identity
  must come from the kernel (`SO_PEERCRED`), never from a client-supplied
  header or claim — the whole point is that a UDS peer can't lie about its
  uid the way a TCP client can lie about anything it sends.
- **Data residency**: not applicable.

## Scope

### In Scope

- `tymuxd`: a `tokio::net::UnixListener` served alongside the existing TCP
  loopback listener (both active by default), using tonic's
  `serve_with_incoming` (or equivalent) to bind a second transport.
- `tymuxd`: default UDS path (e.g. `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock`,
  nested under a `tymuxd`-owned subdirectory rather than directly in the
  shared runtime dir, with a documented fallback), created with mode
  `0600`.
- `tymuxd`: configurable group (flag/env var, naming TBD in research) that
  relaxes the socket to group-readable/writable for a specific gid instead
  of owner-only.
- `tymuxd`: peer-credential extraction via `UnixStream::peer_cred()` at
  accept time; connections from a uid that isn't the daemon's own uid (or in
  the configured group) are rejected before any RPC handler runs.
- `tymuxd`: startup deprecation warning when the TCP loopback listener is
  reachable, framing it as scheduled for removal (see Risk Control).
- `tymux-cli`, `clients/go`, `clients/ts`: default to dialing the UDS path
  when present and reachable, matching each stack's existing pattern for
  connecting to `tymuxd`; a clear, specific error on a peer-credential
  rejection (not a raw transport-error dump).
- Integration tests per client proving both accept (matching identity) and
  reject (non-matching uid) over the UDS transport.

### Out of Scope

- Actually removing the TCP loopback listener — this project only starts
  the deprecation clock; removal is a separate, later project once the
  deprecation warning has had time to surface real usage.
- Per-session/pane ownership (`created_by` field + authz on
  kill/resize/input) — unchanged from `bearer-token-auth`'s own
  out-of-scope list; this project's peer-cred check gates the *connection*,
  not per-RPC identity beyond that.
- Scoped tokens, mTLS — unchanged from `bearer-token-auth`'s out-of-scope
  list.
- Windows support — `SO_PEERCRED`/UDS-based identity is Unix-only; Windows
  named-pipe equivalents are explicitly not part of this project.
- Changing `bearer-token-auth`'s non-loopback bearer-token mechanism —
  that's the separate, already-shipping non-loopback path (PR #43) and is
  unaffected by this project.

## Rabbit Holes

- **tonic + `UnixListener` composition**: tonic's `Server` is built around
  `tokio::net::TcpListener`'s incoming-stream shape by default; serving two
  listeners (TCP + UDS) concurrently from one `Server::builder()` needs
  confirming tonic's `serve_with_incoming`/`Router::serve` API actually
  supports a merged/second incoming stream cleanly, not just in theory.
- **Peer-cred timing relative to tonic's own connection setup**: confirm
  `peer_cred()` is called on the raw `UnixStream` at accept time, before
  tonic's HTTP/2 handshake takes over the socket — if tonic's incoming-
  stream abstraction hides the raw stream, extracting peer creds may need a
  wrapper type.
- **Cross-platform peer-cred**: `SO_PEERCRED` is Linux-specific;
  `tokio::net::UnixStream::peer_cred()` documents itself as Linux/Android
  only in this tokio version — macOS uses `LOCAL_PEERCRED`, which tokio
  does *not* wrap. If macOS support in `peer_cred()` isn't confirmed during
  research, this project may need a `cfg(target_os)` fallback or an
  explicit "Linux only for now" scope cut (many of this repo's dev/target
  hosts are macOS per the dotfiles context — this is a real risk, not
  theoretical).
- **Default UDS path selection**: `$XDG_RUNTIME_DIR` isn't reliably set on
  macOS (no systemd equivalent); needs a real per-platform default path
  decision, not an assumption ported from Linux daemon conventions.
- **Group-based access UX**: deciding how the group is configured (a new
  `--socket-group`/`TYMUXD_SOCKET_GROUP` flag vs. inheriting the process's
  primary group) and what happens when the daemon process itself doesn't
  belong to the requested group.

## Alternatives Considered

- **Token on loopback (extending `bearer-token-auth`'s mechanism to
  loopback)**: rejected — this was already explicitly rejected in
  `bearer-token-auth`'s own requirements ("breaks today's zero-config local
  UX for no security benefit"), and a shared secret is strictly weaker than
  kernel-verified peer identity for this specific problem (local
  multi-user isolation) — a token can be copied between local users
  trivially; a uid cannot be forged over UDS.
- **`SO_PEERCRED`-only, no group support**: considered as the smaller
  (Medium-appetite) cut; rejected per this session's explicit direction to
  fill the Large appetite with group-based access, TCP-loopback
  deprecation, and per-connection identity together rather than sequencing
  them as separate follow-ups.
- **Full TCP-loopback removal now**: rejected — a hard cutover risks
  breaking any existing local automation still dialing TCP without warning;
  a deprecation-warning-first approach is the safer sequencing, consistent
  with `bearer-token-auth`'s own "fail toward more restrictive, not less"
  posture but applied to a migration instead of a single gate.

## Feasibility Risks

- ~~macOS `peer_cred()` support is the single biggest risk to this
  project's "both by default" success metric~~ — **Resolved, and did not
  materialize**: tokio 1.52.3 wraps macOS support internally, no FFI/cfg
  fallback needed. See the matching Open Questions entry above for the
  full resolution and its sources.
- ~~tonic's support for serving two concurrent listener types is
  unverified~~ — **Resolved**: no built-in merge API exists
  (`hyperium/tonic#1080`); the plan uses two independently-spawned
  `Server::builder()` tasks sharing the daemon's `Arc`-wrapped state
  (`TymuxDaemon: Clone`) — see `implementation/plan.md` Epic 4.2 and
  `implementation/architecture-review.md`'s verification against tonic's
  actual `serve_with_incoming_shutdown` source.
- Node's `@connectrpc/connect-node` and Go's `connectrpc.com/connect` UDS
  dialing both require custom transport/dialer wiring (neither is a
  first-class "just pass a socket path" option) — real, but bounded,
  cross-language work in the same category `bearer-token-auth` already
  paid down once for its own three-client parity requirement.

## Observability Requirements

- A rejected (peer-cred mismatch) UDS connection logs at `warn` level with
  the rejecting uid (never any request content), mirroring
  `bearer-token-auth`'s own rejected-request logging framing.
- A counter of rejected UDS peer-cred checks, matching the
  `tymux_attach_resume_outcome_total`-style counter precedent.
- The TCP-loopback deprecation warning is logged once at daemon startup
  (not per-connection) so operators can grep for it in their startup logs
  without log-volume noise.

## Risk Control

- **Staged deprecation, not a breaking change**: TCP loopback keeps working
  unmodified through this project; only a startup warning is added. A
  follow-up project (out of scope here) handles the actual removal once
  the warning has shipped for a real deprecation window.
- **Added during planning, not in this section originally** (architecture
  research's Phase 3 recommendation — "cheap now, expensive to retrofit
  later"): an opt-in `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK`
  flag, defaulted off (TCP stays on by default, unchanged from the "only a
  startup warning is added" framing above). This installs the off-switch
  for the *later* removal project now, while the TCP-vs-UDS listener
  wiring is already being touched, rather than requiring that follow-up
  project to retrofit one from scratch. See
  `implementation/plan.md`'s Epic 1.3/4.3 for the resolution logic and
  warning-suppression behavior this flag gets.
- **Fail toward more restrictive**: consistent with `bearer-token-auth`'s
  own posture — a bug in the peer-cred check should plausibly reject a
  legitimate same-user connection (annoying, safe) rather than accept an
  illegitimate one (unsafe). Both-by-default (TCP still open) means a UDS
  bug does not regress a single-user host below today's baseline.
- No feature flag needed for the UDS listener itself (additive, and TCP
  stays as the escape hatch during this project's timeframe).

## Open Questions

*(All four resolved by research/planning — kept here with their
resolutions rather than deleted, so a reader doesn't have to reconstruct
why they were once open.)*

- ~~Exact default UDS socket path and its macOS equivalent to
  `$XDG_RUNTIME_DIR`~~ — **Resolved**: `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock`
  when set, else a uid-scoped `/tmp`/`$TMPDIR` fallback; see
  `implementation/plan.md`'s Domain Glossary and Task 1.1.1a.
- ~~Exact flag/env var name and precedence for the configurable access
  group~~ — **Resolved**: `--socket-group`/`TYMUXD_SOCKET_GROUP`,
  flag-beats-env, matching `--token`/`TYMUXD_TOKEN`'s precedent; see
  `implementation/plan.md` ADR-002.
- ~~Whether macOS gets full peer-cred parity at v1~~ — **Resolved: yes,
  full parity for core peer identity.** `tokio::net::UnixStream::
  peer_cred()` wraps macOS's `getpeereid`/`LOCAL_PEEREPID` internally, no
  FFI/unsafe code needed — verified independently against tokio 1.52.3
  source by three separate research/review passes (`research/stack.md`,
  `research/architecture.md`, `implementation/architecture-review.md`).
  The *only* documented reduced posture is narrower: multi-group
  `--socket-group` membership checks read the full supplementary-group
  list via `/proc/<pid>/status` on Linux only; macOS/BSD fall back to
  primary-gid-only (ADR-002) — a deliberate, scoped decision about one
  secondary feature, not an unresolved risk to the core capability.
- ~~Whether the TCP-loopback deprecation warning should reuse the exact
  log framing as `bearer-token-auth`'s existing warning~~ — **Resolved**:
  it's a distinct message (see `implementation/plan.md` Task 4.3.1a and
  `design/ux.md` Surface 1) to avoid conflating two different warnings.
