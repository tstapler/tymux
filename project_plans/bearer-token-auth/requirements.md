# Requirements: bearer-token-auth

**Date**: 2026-08-28
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement

`tymuxd` has no authentication anywhere. Any client that can reach the gRPC
port can `CreateSession` (spawning an arbitrary command) and
`Attach`/`CapturePane`/`KillSession` against any `pane_id`, with no ownership
check. On the default loopback bind this is an accepted risk (only local
processes can reach it). The moment `tymuxd` binds to a non-loopback address
— a real, supported configuration for hosted/multi-tenant use (a shared
daemon behind stapler-squad, or any future web frontend, per the roadmap) —
this becomes unauthenticated remote code execution. `crates/tymuxd/src/main.rs:1230-1247`
already logs a `tracing::warn!` for this exact case today; it's a warning,
not a gate.

## Baseline

Today, `tymuxd --addr 0.0.0.0:7419` (or any non-loopback bind) starts and
serves every RPC to any client that can open a TCP connection to the port,
with only a log line noting the risk. Nothing refuses the bind, and nothing
rejects an unauthenticated request.

## Users / Consumers

Every `TymuxService` client: `tymux-cli` (`crates/tymux-cli`), `clients/go`,
`clients/ts`, and the in-flight `stapler-squad` `BackendTymux` integration
(`project_plans/stapler-squad-integration/`) — the concrete near-term
consumer this unblocks per the roadmap's "Next" section
(`project_plans/roadmap/README.md`).

## Success Metrics

- `tymuxd` bound to a non-loopback address refuses to start without a
  configured token (closing the RCE gap for that bind, not just warning
  about it). **Scope note added during Phase 4 validation** (`pre-mortem.md`
  P1 #1): this guarantee is keyed off `tymuxd`'s own bind address, which is
  a same-host heuristic, not a network-reachability guarantee — a
  loopback-bound `tymuxd` fronted by a reverse proxy or tunnel is not
  protected by this metric and must be deployed non-loopback+token
  regardless (see `implementation/plan.md`'s Deployment Guidance section).
- A request against a non-loopback-bound `tymuxd` with a missing or wrong
  token is rejected before reaching any RPC handler, with a clear error
  distinguishable from "daemon unreachable" or "session not found"
  (`tymux-cli`'s own `docs/reviews/is-it-ready-2026-07-13.md` non-blocking
  finding #10 already flags today's raw `anyhow` Debug-dump errors as
  unhelpful — this must not add another instance of that).
- Loopback-bound `tymuxd` (today's default, `127.0.0.1:7419`) behaves
  identically to today: no token required, zero config change for existing
  local `tymux` usage.
- `clients/go` and `clients/ts` can both authenticate against a
  non-loopback-bound `tymuxd` in their own integration tests, proving the
  mechanism cross-language (mirrors how `attach-resume-protocol` Epic 5
  proved the resume path in both reference clients).

## Appetite

Medium (1-2 weeks)

## Constraints

None beyond the existing single-maintainer, solo-dev cadence of this repo.
No external deadline.

## Non-functional Requirements

- **Performance SLO**: token comparison must not introduce a per-RPC
  latency regression — a single constant-time compare against one
  in-memory token, not a database/network lookup. **Closure note**
  (Engineering triad review): satisfied by design-level argument, not a
  benchmark — the compared value is fixed-size and in-memory with no I/O
  on the hot path, so no plausible input shape under this feature's scope
  could produce a regression a benchmark would catch that code review
  wouldn't (see `implementation/plan.md`'s Observability Plan for the full
  reasoning). Revisit with a real benchmark if a future change removes
  that "fixed-size, O(1) per RPC" property.
- **Scalability**: not applicable — one shared bearer token per daemon
  instance, not a multi-user credential store (that's the separate, later
  "per-session/pane ownership" / "scoped tokens" roadmap items).
- **Security classification**: this *is* the security boundary — the token
  must never be logged, must be compared in constant time (avoid a
  timing side-channel on a byte-by-byte compare), and must never appear in
  a `tracing` field at any level (including `debug`).
- **Data residency**: not applicable.

## Scope

### In Scope

- `tymuxd`: a gRPC interceptor (tonic) that, when bound non-loopback,
  requires a valid bearer token on every `TymuxService` call, rejecting
  with a clear, distinct gRPC status (`Unauthenticated`) otherwise.
- `tymuxd`: refuses to start (fails fast, not a warning) if bound
  non-loopback with no token configured.
- `tymuxd`: token supplied via `--token` CLI flag or `TYMUXD_TOKEN` env var
  — no auto-generation, no token file on disk (operator-supplied, matching
  standard shared-secret-via-env practice for daemons exposed beyond
  localhost).
- `tymux-cli`: `--token` flag / `TYMUXD_TOKEN` env var, attached as the
  bearer token on every outgoing call; a clear, specific error message
  on an `Unauthenticated` response (not a raw `anyhow` Debug dump — ties
  into the existing known CLI-error-quality gap, scoped narrowly to this
  new error path, not a full rewrite of CLI error handling).
- `clients/go`, `clients/ts`: token support in their example/attach code
  plus a real integration test proving a non-loopback-bound `tymuxd` with a
  wrong/missing token rejects the call and a correct token succeeds —
  mirrors the existing `buf generate` drift + live-daemon integration test
  pattern both clients already have for the resume path.
- Loopback bind (today's default) is unaffected: no token required, no
  behavior change.
- **Added during planning, not in this section originally** (PM triad
  review flagged the doc-sync gap): `crates/tymux-core/src/pane.rs` —
  strip `TYMUXD_TOKEN` from every spawned pane's environment
  (`portable_pty::CommandBuilder` inherits the daemon's full environment
  by default). This feature's own `TYMUXD_TOKEN` is what creates the leak
  vector in the first place; see `implementation/plan.md`'s Epic 1.3 for
  the full scope-amendment rationale.

### Out of Scope

- Per-session/pane ownership (`created_by` field + authz check on
  kill/resize/input) — separate roadmap item, sequenced after this one.
- Scoped tokens (read-only vs. read-write attach) — separate roadmap item,
  explicitly layered on top of this one once it exists.
- mTLS for daemon-to-daemon / multi-host scenarios — separate roadmap item,
  explicitly "layered on the bearer-token work above once it exists."
- Auto-generated tokens or a token file on disk — deliberately rejected in
  favor of operator-supplied tokens (see decision above); revisit only if
  real usage shows the operator-supplied model is too much friction.
- Any change to loopback-bind behavior or the existing zero-config local
  CLI experience.
- Multi-token / per-user credentials — one shared bearer token per daemon
  instance is the whole scope here.

## Rabbit Holes

- **tonic interceptor API specifics**: tonic's `Interceptor` trait operates
  on `tonic::Request<()>` metadata before the request body is even
  deserialized — needs to be confirmed to apply uniformly to every
  `TymuxService` method including the bidirectional `Attach` stream (a
  stream's interceptor timing/semantics can differ subtly from a unary
  call's) before committing to the exact wiring.
- **Constant-time comparison**: a naive `==` on the token string is a timing
  side-channel; needs a real constant-time compare (e.g. `subtle` crate or
  a hand-rolled XOR-accumulate), not just "use `==` and call it done."
- **Propagating a clear error through three different client stacks**: Rust
  (`tonic::Status`), Go (`connect-go`), and TypeScript (`@connectrpc/connect`)
  each surface a gRPC `Unauthenticated` status differently at the API
  surface — confirming each one's actual shape (not assuming symmetry with
  the others) is real, non-trivial cross-language work, the same category
  of risk `attach-resume-protocol`'s Epic 5 already paid down once for the
  resume path.
- **`tymux-cli`'s existing raw-`anyhow`-Debug error path**: scoping this
  feature's error message improvement narrowly to the new auth-failure
  case, without accidentally being pulled into fixing the pre-existing,
  broader "all CLI errors look the same" gap (`is-it-ready` finding #10)
  as part of this project.

## Alternatives Considered

- **Auto-generated token + local file** (Jupyter-style): rejected —
  explicitly decided against in favor of operator-supplied tokens; adds a
  new local-trust surface to reason about for what's meant to be a
  *remote*-exposure gate, and this repo has no existing precedent for a
  daemon-managed secrets file.
- **Always-required auth (including loopback)**: rejected — breaks today's
  zero-config local `tymux` UX for no security benefit on a bind that's
  already unreachable from outside the machine; the existing `main.rs`
  warning already frames loopback as the accepted-risk baseline.
- **JWT / signed tokens**: not considered seriously — one shared bearer
  secret per daemon instance is the correct shape for this scope (no
  per-user identity yet; that's the later "scoped tokens" item), and a
  plain opaque token needs no signing/verification infrastructure.

## Feasibility Risks

- tonic interceptor behavior on the streaming `Attach` RPC specifically
  needs early verification (see Rabbit Holes) — if it doesn't compose
  cleanly with the existing bidi-stream handling, the interceptor approach
  itself may need a fallback (e.g. explicit per-handler check) for that one
  RPC.
- No existing precedent in this codebase for constant-time comparison —
  confirm whether a new dependency (`subtle`) is warranted or a small
  hand-rolled compare is sufficient (favor the latter per this repo's
  general "don't add a dependency for a few lines" bias, unless the
  hand-rolled version is genuinely risky to get right).

## Observability Requirements

- A rejected (`Unauthenticated`) request logs at `warn` level with the
  peer address — mirrors the existing non-loopback bind warning's own
  framing (attempted access, not a routine event) — but never logs the
  token or any part of it, correct or not. **Accepted v1 constraint**
  (found during planning, not part of the original ask): the RPC method
  name cannot be included alongside the peer address — `tonic::service::
  Interceptor::call`'s `Request<()>` never carries the original HTTP/2
  URI (verified directly against pinned `tonic 0.12.3` source), so a
  server-side interceptor structurally cannot recover it without a
  heavier `tower::Layer` design. Peer-address-only logging still answers
  the operational question this requirement exists to serve ("is anyone
  hitting this"); see `project_plans/bearer-token-auth/implementation/
  plan.md`'s Unresolved Questions for the tracked fast-follow if
  method-name granularity is ever needed.
- A counter of rejected-auth attempts, matching the existing
  `tymux_attach_resume_outcome_total`-style counter pattern already
  established in `crates/tymuxd/src/main.rs` for resume outcomes — gives
  an operator a signal distinguishing "nobody's hitting this" from "someone
  keeps trying and failing."

## Risk Control

Not needed — strictly additive and backward compatible. Loopback (today's
default and, per the constraints above, todays's only *unchanged*
supported path) is entirely unaffected. Non-loopback binding was already an
explicitly-flagged, unsupported-without-a-warning-eyeful configuration; this
turns that warning into a hard gate, which cannot make an already-insecure
configuration worse. No feature flag or staged rollout needed — a bug in
the interceptor most plausibly fails toward *more* restrictive (a
legitimate non-loopback client gets rejected) rather than *less* (an
attacker gets through), which is the safe failure direction for a security
gate.

## Open Questions

- Should the interceptor's `Unauthenticated` rejection be observable via
  `StatusBarModel` or any other existing daemon-introspection RPC, or is a
  log line + counter sufficient for v1 of this feature? (Lean: log + counter
  sufficient; revisit if real operational use shows a need for live
  visibility beyond logs/metrics.)
- Exact flag/env var precedence if both `--token` and `TYMUXD_TOKEN` are
  set (research phase should confirm this repo's existing precedent, if
  any, for CLI-flag-vs-env-var precedence elsewhere in `tymux-cli`/`tymuxd`).
