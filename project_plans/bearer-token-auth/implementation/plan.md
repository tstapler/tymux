# Implementation Plan: bearer-token-auth

**Feature**: A gRPC interceptor gates every `TymuxService` RPC behind an
operator-supplied bearer token whenever `tymuxd` binds to a non-loopback
address, with matching client support in `tymux-cli`, `clients/go`, and
`clients/ts`, plus a fix for the token leaking into every spawned pane's
environment.
**Date**: 2026-08-28
**Status**: Ready for implementation
**ADRs**: [ADR-001](../decisions/ADR-001-constant-time-eq-crate.md) (constant-time compare crate), [ADR-002](../decisions/ADR-002-tymuxd-token-flag-parsing.md) (tymuxd flag-parsing mechanism)

---

## Step 0.5 — Alternatives considered

Three shapes for the server-side gate were weighed before committing:

**A. `tonic::service::Interceptor` on `TymuxServiceServer::with_interceptor`**
(chosen). Strength: one enforcement point that can't be forgotten per-RPC,
already proven (by reading pinned `tonic 0.12.3` source, not docs) to fire
identically for unary and the bidi `Attach` stream, with zero new
`tower`/codegen plumbing. Weakness: `Interceptor::call` receives a
`Request<()>` that never carries the original HTTP/2 URI (verified from
`tonic-0.12.3/src/request.rs`'s `Request<T>` struct, which has no `uri`
field, and `.../service/interceptor.rs`'s `InterceptedService::call`,
which explicitly discards the URI/method/version before invoking the
interceptor) — so the interceptor cannot log the RPC method name
alongside the peer address, only the peer address. This is a genuine gap
against `requirements.md`'s Observability wording ("peer address and the
RPC method name"); resolved in Pattern Decisions and Unresolved Questions
below rather than silently dropped.

**B. Manual per-handler check inside each of the ~10 `impl TymuxService
for TymuxDaemon` methods.** Strength: full access to everything an
`http::Request` would carry, including the method name implicitly (each
handler already knows which RPC it is). Weakness: requires touching every
handler individually and is trivially forgettable on the next new RPC
added to the service — the exact failure mode a single-choke-point design
exists to prevent.

**C. A custom `tower::Layer`/`Service` wrapping the whole
`TymuxServiceServer`, operating on the raw `http::Request<ReqBody>`
before it's converted to a `tonic::Request`.** Strength: would recover
the method-name-in-logs capability Option A gives up (the raw
`http::Request` does carry `.uri()`). Weakness: real additional
machinery — reimplementing body-stripping/reconstruction that
`InterceptedService` already does for free — for a feature whose actual
security-relevant logic (the token gate itself) is a synchronous,
single-value compare that `tonic::Interceptor` already handles cleanly;
`research/pitfalls.md` §3 independently reaches the same conclusion
("`Interceptor` ... is the right level of complexity here").

**Chosen: A.** The method-name-in-logs gap is real but narrow (peer
address alone still answers "is anyone hitting this," the operational
question the counter/log exist for) and is recorded as a scoped,
non-blocking Unresolved Question rather than justifying Option C's extra
machinery for v1.

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| Bearer token | The one shared, operator-supplied secret string that authenticates every client of a non-loopback-bound `tymuxd` instance. | Not per-user; scope is explicitly one secret per daemon instance. |
| `BearerToken` | Newtype wrapping the token `String` (no `Debug` derive — manual impl prints `"<redacted>"`; no `PartialEq`/`Eq` derive, to keep a non-constant-time `==` from ever sitting next to the required `constant_time_eq` call). `BearerToken::parse(&str) -> Option<Self>` is the only constructor, making "empty token" unrepresentable downstream. | Added during architecture review (not in the original plan draft) — see Pattern Decisions. Defined independently (mirrored, not shared) in `crates/tymuxd/src/auth.rs` and `crates/tymux-cli/src/main.rs`. |
| `resolve_token` | Function resolving `--token <value>`/`--token=value` (hand-parsed from `std::env::args()`) or `TYMUXD_TOKEN`, with the flag winning when both are set and an empty value from either source treated as absent (returns `Option<BearerToken>`). | New in this feature; see ADR-002. Lives in `crates/tymuxd/src/auth.rs` (extracted from `main.rs` during architecture review — see Pattern Decisions). |
| `configured_token` | The `Option<BearerToken>` local in `tymuxd`'s `main()` carrying the resolved token from the startup gate to the server-construction call site. | Plain owned value, no `Arc`/`Mutex` — tokens don't change at runtime. |
| `BearerAuthInterceptor` | `tymuxd`-side struct implementing `tonic::service::Interceptor`; holds the configured `BearerToken` and a rejection counter, and gates every `TymuxService` call. | New type in `crates/tymuxd/src/auth.rs` (extracted from `main.rs` during architecture review). |
| `constant_time_eq` | The crate (`constant_time_eq::constant_time_eq(a: &[u8], b: &[u8]) -> bool`) used for the token comparison, chosen over `subtle` per ADR-001. | Handles unequal-length inputs safely; no separate length check needed. |
| `tymux_auth_rejection_total` | The counter name/log field for rejected-auth attempts, following the existing `tymux_attach_resume_outcome_total` naming pattern (`ResumeOutcomeCounters`, `crates/tymuxd/src/main.rs:162-201`). | Server-side only; incremented on every `Unauthenticated` rejection. |
| `BearerAuth` | `tymux-cli`-side struct implementing `tonic::service::Interceptor`; sets the `authorization` metadata entry on every outgoing call when a `BearerToken` is configured, no-ops otherwise. | New type in `crates/tymux-cli/src/main.rs`. |
| `authInterceptor` (Go) | `clients/go`-side type implementing connect-go's full `connect.Interceptor` interface (`WrapUnary` + `WrapStreamingClient`, with `WrapStreamingHandler` passed through unchanged) so the header is set on both unary and the streaming `Attach` client call. | connect-go's convenience `UnaryInterceptorFunc` only implements `WrapUnary`, leaving `WrapStreamingClient` a documented no-op (verified against pinned `connectrpc.com/connect@v1.20.0/interceptor.go`) — using it alone would silently exempt `Attach`. |
| `authInterceptor` (TS) | `clients/ts`-side `@connectrpc/connect` `Interceptor` function (`(next) => async (req) => {...}`) passed into `createGrpcTransport({ interceptors })`; applies uniformly to unary and streaming calls by construction (TS has one `Interceptor` type, not Go's unary/streaming split). | Confirmed from `@connectrpc/connect`'s vendored `interceptor.d.ts`. |
| `tonic::Code::Unauthenticated` | The gRPC status code every layer of this feature uses for "no/bad token," distinguished from `NotFound` (bad session) and unreachable-daemon transport errors. | Server sets it via `Status::unauthenticated(...)`; each client stack surfaces it under its own idiom (`tonic::Status`, `connect.CodeUnauthenticated`, `Code.Unauthenticated`). |
| `env_remove("TYMUXD_TOKEN")` | The one-line fix in `crates/tymux-core/src/pane.rs`'s `spawn_internal` that stops the daemon's own bearer secret from leaking into every spawned pane's process environment. | Targeted removal, not `.env_clear()` — every other inherited var stays intact. |
| Loopback / non-loopback bind | `socket_addr.ip().is_loopback()` — the existing branch point in `tymuxd`'s `main()` that this feature's entire auth requirement hangs off of. | Pre-existing term/mechanism; not introduced by this feature. |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Server-side request gate | `tonic::service::Interceptor` via `TymuxServiceServer::with_interceptor(...)` | `research/architecture.md` §1, `research/build-vs-buy.md` §1 | (B) manual per-handler check in each of ~10 `impl TymuxService` methods | Forgettable per-RPC; a single interceptor is the enforcement point that can't be skipped when a new RPC is added later. |
| Server-side request gate | (as above) | (as above) | (C) custom `tower::Layer`/`Service` on the raw `http::Request` | Would recover method-name-in-logs (see Observability row below) but is real extra machinery — reimplementing body-strip/reconstruct — for a synchronous single-token compare that `Interceptor` already handles; `research/pitfalls.md` §3 independently reaches the same conclusion. |
| Constant-time token compare | `constant_time_eq` crate | `research/build-vs-buy.md` §2, [ADR-001](../decisions/ADR-001-constant-time-eq-crate.md) | `subtle` crate | `subtle`'s `Choice`/`ConditionallySelectable` framework is built for constructing larger constant-time algorithms; this project needs one function, and `constant_time_eq` is exactly that function with less API surface to misuse. |
| Constant-time token compare | (as above) | (as above) | Hand-rolled XOR-accumulate loop | "Looks constant-time" code is exactly where the compiler can silently reintroduce a timing-dependent branch at higher optimization levels without an explicit barrier — the one place in this feature where the repo's usual anti-dependency bias is deliberately overridden. |
| `tymuxd` token flag mechanism | Hand-rolled `std::env::args()` scan (`resolve_token`), supporting both `--token value` and `--token=value` | `research/architecture.md` §4, [ADR-002](../decisions/ADR-002-tymuxd-token-flag-parsing.md) | Add `clap` to `tymuxd` | `tymuxd` has zero CLI-flag parsing today and has deliberately stayed dependency-light; one optional flag + one fallback env var is ~3 lines hand-rolled, and `clap`'s marginal value here (declarative `env=` precedence) isn't worth a new dependency plus the `--help`-echoes-current-env-value footgun (`research/pitfalls.md` §1) that a hand-rolled parser has no `--help`-generation step to trip on. Originally space-separated-only; adversarial review flagged `--token=value` as an untested gap between the hand-rolled parser and `clap`'s free support for both forms — cheap enough to close outright (Task 1.1.2c), so both forms are now supported. |
| `tymux-cli` token flag mechanism | `clap`'s `#[arg(long, env = "TYMUXD_TOKEN", hide_env_values = true)]` | `research/stack.md` §6, `research/ux.md` Flag/env-var precedence note | Hand-rolled parsing (to match tymuxd) | `tymux-cli` already depends on `clap` with an existing `Cli` struct — this is additive to existing infrastructure, not a new-dependency decision, unlike `tymuxd`'s situation. Not conflating the two binaries per the task brief. **Correction found in adversarial review**: `research/stack.md` §6 claimed clap's `--help` "shows the env var name by default but not the current value" — verified false against `clap_builder-4.6.6` source (`hide_env_values` defaults to `false`, and without it `--help` prints the *live* `TYMUXD_TOKEN` value). `research/stack.md` §6 has been corrected in place; this plan's Task 2.1.1b sets `hide_env_values = true` explicitly and Task 2.1.1d pins the behavior with a test. |
| `tymux-cli`/`tymuxd` workspace `clap` feature set | `features = ["derive", "env"]` at the workspace `Cargo.toml` level | adversarial-review.md Blocker 1 | Leaving `features = ["derive"]` as-is | `#[arg(env = "...")]` requires clap's `"env"` cargo feature, which was never enabled — Task 2.1.1a's `#[arg(long, env = "TYMUXD_TOKEN")]` would not compile without it. This is a workspace-level, additive-only change (existing flags on `tymux-cli`'s `Cli` are unaffected — no existing field uses `env = "..."`). |
| Token representation | `BearerToken` newtype (no `Debug`/`PartialEq`/`Eq` derive; `BearerToken::parse(&str) -> Option<Self>` is the only constructor) | architecture-review.md, first Concern | Bare `String`/`Option<String>` throughout (original plan draft) | "Empty token is absent" was enforced only by a single `.filter(|t| !t.is_empty())` call inside `resolve_token` — a future second token source (e.g. the `TYMUXD_TOKEN_FILE` idea already flagged in Unresolved Questions) could bypass it and let an empty configured token pass `check_non_loopback_requires_token`'s `is_none()` check while `constant_time_eq` then accepts *any* client presenting an empty `Bearer ` value — a full auth bypass. The newtype makes that state unrepresentable. Not derived `PartialEq`/`Eq` so a derived `==` can't sit next to the required `constant_time_eq` call as a second, non-constant-time comparison path (the exact hand-rolled-compare risk ADR-001 already flags). |
| `tymuxd` auth code location | `crates/tymuxd/src/auth.rs` (`mod auth;` in `main.rs`) | architecture-review.md, second Concern | Piling `resolve_token`/`check_non_loopback_requires_token`/`BearerAuthInterceptor`/`BearerToken` and their unit tests into `main.rs` (original plan draft) | `main.rs` is already 4,511 lines with no domain/infra module split; this feature would have added ~150 more lines of auth-specific code to it. Extracting to `auth.rs` is a zero-behavior-change move that stops the god-file from absorbing another concern and gives the next auth-adjacent feature (scoped tokens, already on the roadmap) an obvious home. `main.rs` keeps only the `mod auth;` declaration, the import, and the ~10-line call-site wiring (startup gate call + server-construction branch). |
| `clients/go` auth interceptor location | Shared `clients/go/authinterceptor` package, imported by the integration test and both example mains | architecture-review.md, third Concern | Defining `authInterceptor` in the integration test file, then copy-pasting it into both example `main.go` files (original plan draft, Tasks 3.1.1a/3.1.1e) | Three copies of the same ~20-line struct + 3 methods means the next fix or bug in `WrapStreamingClient`'s behavior needs to land three times, with three chances to diverge — exactly the duplication risk the plan's own Pattern Decision ("one reusable interceptor per language") was meant to prevent. TS already avoids this (`authInterceptor` lives once in `clients/ts/examples/client.ts`, reused via the exported `tymuxClient()` factory); Go's plan now matches that shape. |
| Token storage (server) | Plain owned `String` captured by value in `BearerAuthInterceptor`, no `Arc`/`Mutex` | `research/architecture.md` §2 | `Arc<Mutex<String>>` field on `TymuxDaemon`/`Engine` | Token never changes at runtime (no reload path exists, confirmed by reading `main.rs`); auth is a pure request-gate never consulted by RPC handler bodies, so it doesn't belong in business-logic state that 10+ handlers and their tests already construct. Matches this file's own existing precedent (`disconnect_regression_window`, `grace_period_duration` — plain owned `Duration` fields read once at startup, `main.rs:93-100`). |
| Rejection observability | `tracing::warn!` with peer address only + `Arc<AtomicI64>` counter, mirroring `ResumeOutcomeCounters` | `requirements.md` Observability Requirements (partially — see Reason), this plan's Step 0.5 | Logging the RPC method name alongside peer address, as `requirements.md` literally requests | `tonic::service::Interceptor::call`'s `Request<()>` never carries the original HTTP/2 URI — verified directly from pinned `tonic-0.12.3` source (`request.rs`'s `Request<T>` struct has no `uri` field; `service/interceptor.rs`'s `InterceptedService::call` explicitly discards the captured `uri`/`method`/`version` locals before constructing the request handed to the interceptor, only reattaching them afterward on the `Ok` path). None of the six research docs caught this API constraint; surfaced during planning by reading the pinned source directly. Peer-address-only logging still answers the operational question the requirement is really after ("is anyone hitting this"); full method-name breakdown is recorded as a fast-follow (Unresolved Questions) behind Option C (tower::Layer) if it's ever needed. |
| Client-side auth attachment (all 4 clients) | One reusable, transport/client-constructor-level interceptor per language, not per-call-site header setting | `research/build-vs-buy.md` §4, `research/stack.md` §7-8 | Setting the `Authorization`/`authorization` header at each individual call site | Both connect-go and `@connectrpc/connect` (and tonic's client codegen) provide first-class interceptor/transport-level extension points specifically for this; per-call-site header-setting would duplicate the same three lines at every RPC call and is exactly the shape that lets `Attach` get silently missed (see next row). |
| `clients/go` interceptor shape | Full `connect.Interceptor` interface (`WrapUnary` + `WrapStreamingClient`) | Verified against pinned `connectrpc.com/connect@v1.20.0/interceptor.go` | `connect.UnaryInterceptorFunc` | `UnaryInterceptorFunc`'s `WrapStreamingClient` is a documented no-op (confirmed in source) — using it alone would silently exempt the `Attach` RPC from auth entirely on the Go client, precisely the "auth wired into unary-only helper, `Attach` bypasses it" risk `research/pitfalls.md` §4 names. |

---

## Migration Plan

N/A — no schema or data changes. No persisted state (session records,
layout snapshots) changes shape; the token lives only in process memory
(env/argv at startup) and is never written to disk.

## Observability Plan

- **Logs**: `tracing::warn!` on every rejected `TymuxService` call, fields
  `peer` (remote socket address, or `"unknown"` if unavailable) and a
  static reason string (`"missing bearer token"` / `"invalid bearer
  token"`) — never the token itself, never the full `MetadataMap` Debug-
  dumped. `tracing::info!` at startup stating whether auth is enforced
  (non-loopback+token) or not required (loopback), extending the existing
  `main.rs:1238-1247` log site. See Pattern Decisions for why the RPC
  method name is not included in v1.
- **Metrics**: `tymux_auth_rejection_total`, an `Arc<AtomicI64>` counter on
  `BearerAuthInterceptor`, incremented once per rejected call (missing or
  invalid token alike — not split by reason, matching the coarse
  granularity `ResumeOutcomeCounters` uses for its own outcome buckets).
- **Alerts**: none added — this repo has no existing alerting
  infrastructure (`requirements.md` Risk Control: "no feature flag or
  staged rollout needed"); the counter/log exist for an operator to grep
  or dashboard manually if they choose to.
- **Performance NFR**: no dedicated benchmark/timing test is added
  (`pre-mortem.md` / `validation.md` both flag this as a gap worth
  stating explicitly rather than silently omitting). This is intentional,
  not an oversight: the compared value is a single, fixed-size in-memory
  byte string (the operator-configured token, realistically tens of
  bytes), the comparison is `constant_time_eq`'s single O(n) pass with no
  I/O, allocation, or lock contention on the hot path, and the whole
  check runs once per RPC before any handler logic — there is no
  plausible input shape under this feature's own scope (one shared
  token, no per-request growth) that could produce a measurable
  regression a benchmark would catch that code review wouldn't. Revisit
  only if a future change (e.g. per-request token lookup for scoped
  tokens) removes the "fixed-size, in-memory, O(1) per RPC" property this
  reasoning depends on.

## Deployment Guidance

**Added during Phase 4 validation** (`pre-mortem.md` P1 #1 — a genuine
blind spot neither `requirements.md` nor either Phase 3 review caught):
the auth gate is keyed off `tymuxd`'s own bind address
(`socket_addr.ip().is_loopback()`), which is a proxy for "is this daemon
reachable from outside this machine," not a guarantee of it. **A `tymuxd`
bound to loopback and fronted by a reverse proxy (nginx, Caddy), an SSH
`-L` tunnel, or a tunneling service (Cloudflare Tunnel, ngrok) is
reachable from wherever that proxy/tunnel exposes it, while `tymuxd`
itself still sees `127.0.0.1` and applies zero auth** — the daemon never
logs a rejection in this scenario, because nothing it can observe ever
looks like a non-loopback bind. **Any deployment that puts a proxy,
tunnel, or port-forward in front of a loopback-bound `tymuxd` must bind
`tymuxd` itself non-loopback and configure a token, regardless of
whatever auth the proxy/tunnel layer provides** — the loopback exemption
is not a network-reachability guarantee, only a same-host heuristic.
Story 1.1.3's startup-success log (Task 1.1.3b) should carry this
caveat in its loopback-bind log line so it's visible at every startup,
not just in this document.

## Risk Control

- **Feature flag**: none — per `requirements.md` Risk Control, this is
  strictly additive: loopback (today's only currently-supported path) is
  completely unaffected, and non-loopback binding was already an
  explicitly-flagged, unsupported-without-a-warning configuration. Turning
  that warning into a hard gate cannot make an already-insecure
  configuration worse.
- **Rollback procedure**: revert the commit(s); no data migration to
  reverse (Migration Plan is N/A). An operator who already deployed a
  non-loopback `tymuxd` with `TYMUXD_TOKEN` set can keep running the new
  binary indefinitely — nothing in this feature requires a follow-up
  change to stay working.
- **Staged rollout**: none needed — a bug in the interceptor fails toward
  *more* restrictive (a legitimate client gets rejected) rather than
  *less* (an attacker gets through), the safe failure direction for a
  security gate, per `requirements.md`.

## Unresolved Questions

- [ ] Should `TYMUXD_TOKEN_FILE` (file-based token distribution, avoiding
      the `/proc/<pid>/environ` exposure a raw env var has) be supported
      as an additional token-source mechanism? — flagged in
      `research/features.md` §3b as a real, distinct gap from the
      auto-generated-token rejection already made in `requirements.md`'s
      Alternatives Considered. **Deliberately not added to this plan's
      in-scope stories** — it wasn't one of the three scoping decisions
      made during requirements gathering, and adding it now would be
      silent scope expansion. Fast-follow candidate: the interceptor/
      validation logic (`BearerAuthInterceptor`, `constant_time_eq`
      compare) is identical regardless of where the byte string comes
      from at startup — only `resolve_token` would need a third source
      checked. Blocks: no story in this plan. Owner: whoever next touches
      `tymuxd`'s auth/config surface, on request.
- [ ] Should the rejection log/counter carry the RPC method name, not just
      the peer address? Blocked by a genuine `tonic::service::Interceptor`
      API limitation discovered during planning (Pattern Decisions,
      Observability row) — recovering it would mean moving to a custom
      `tower::Layer` (Step 0.5 Option C). Not built for v1: peer-address-
      only logging already answers "is anyone hitting this," the
      operational question the requirement exists to serve. Blocks: no
      story in this plan (documented as accepted v1 scope, not a gap left
      open by omission). Owner: whoever finds peer-address-only logging
      insufficient during a real incident.
- [ ] Should an already-open `Attach` stream be forcibly closed if the
      daemon's token changes (e.g., across a restart with a new
      `TYMUXD_TOKEN`)? `research/features.md` §2a recommends treating this
      as out of scope for v1 — `requirements.md`'s Success Metrics only
      test new-call rejection, and no live-token-rotation mechanism exists
      today (confirmed: `tymuxd` reads its bind address and token exactly
      once at startup, no config-reload path). Stated here explicitly so
      it isn't silently assumed covered. Blocks: no story in this plan.
      Owner: N/A unless a future config-reload feature is proposed, at
      which point it must re-run the same fail-fast bind-vs-token check
      (`research/pitfalls.md` §5).

## Dependency Visualization

```
Phase 1: tymuxd (server) — crates/tymuxd/src/auth.rs unless noted
  Epic 1.1  Token resolution + startup gate
    Story 1.1.1 (add constant_time_eq dep) ─┐
    Story 1.1.2 (BearerToken + resolve_token) ├──> Story 1.1.3 (fail-fast gate)
                                             │           │
  Epic 1.2  Interceptor + wiring             │           │
    Story 1.2.1 (BearerAuthInterceptor) <────┘           │
            │                                            │
            └──────────────> Story 1.2.2 (wire into Server::builder(), main.rs) <───┘
                                       │
  Epic 1.3  PTY env-leak fix           │  (independent — no dependency on 1.1/1.2;
    Story 1.3.1 (env_remove) ──────────┼──── can run in parallel with Epic 1.1/1.2)
                                       │
  Epic 1.4  Observability              │
    Story 1.4.1 (logging + counter) <──┘ (folds into 1.2.1's interceptor body)

Genuine hard dependency on Phase 1: only the tasks that need a live,
token-gated daemon to run an RPC against — Story 1.2.2's own integration
tests (1.2.2c/d), and downstream, Task 2.1.2c/d (tymux-cli), Task
3.1.1d/e (Go), Task 3.2.1c/d (TS). Everything else below is pure
unit-level/wiring work with zero technical dependency on Phase 1 and can
start immediately, in parallel with Phase 1 (adversarial-review.md
Concern 5 — the original "Phase 1 must land first" framing was more
conservative than the actual dependency graph):

  Parallelizable with Phase 1, no daemon needed:
    Story 2.1.1 (--token flag on Cli, incl. clap "env" feature + hide_env_values)
    Story 2.2.1 (friendly_message's Unauthenticated branch)
    Task 2.1.2a/b (tymux-cli: BearerToken/BearerAuth interceptor
      definition + client-construction wiring — pure code, no daemon
      run needed, structurally identical to Go's 3.1.1a/b and TS's
      3.2.1a below; corrected during triad review round 2, originally
      omitted from this list despite the same reasoning already applied
      to the other two clients)
    Task 3.1.1a/b/c/f (Go: authinterceptor package, its wiring into the
      test file's/examples' client constructors, and the startDaemon
      variant — all pure code, no daemon run needed)
    Task 3.2.1a/b (TS: authInterceptor + tymuxClient(token), and the
      startDaemon harness variant — writing this code needs no running
      daemon, exactly like Go's structurally-identical Task 3.1.1c;
      corrected during triad review, originally miscategorized as
      blocked alongside the tests that actually run against it)

  Blocked on Task 1.2.2b's non-loopback test harness (needs a real,
  token-gated tymuxd to connect to):
    Task 2.1.2c/d  (tymux-cli integration tests)
    Task 3.1.1d/e (Go: unary + Attach integration tests)
    Task 3.2.1c/d (TS: unary + Attach integration tests)

Within Phase 1: Epic 1.3 is fully independent of 1.1/1.2/1.4 and can be
done first, last, or in parallel. Epic 1.1/1.2/1.4's *unit-level* tasks
(everything in `auth.rs`, no live socket) have no dependency on each
other's *integration* tests either — only Story 1.2.2's integration
tasks (1.2.2c/d) need the whole chain (1.1 → 1.2.1 → 1.2.2a/b) wired up
first.
```

---

## Phase 1: tymuxd (server-side auth core)

### Epic 1.1: Token resolution, constant-time compare dependency, fail-fast startup gate
**Goal**: `tymuxd` can resolve an operator-supplied token from `--token`/
`TYMUXD_TOKEN` with correct precedence and empty-string handling, and
refuses to start on a non-loopback bind without one.

#### Story 1.1.1: Add the constant-time comparison dependency
**As a** `tymuxd` maintainer, **I want** `constant_time_eq` available as a
dependency, **so that** the token compare (Story 1.2.1) has no timing
side-channel.
**Acceptance Criteria**:
- `constant_time_eq` compiles as a `tymuxd` dependency.
  - *Given* a fresh checkout with this story's change applied, *When*
    `cargo build -p tymuxd` runs, *Then* it compiles successfully and
    `constant_time_eq::constant_time_eq` is callable from `main.rs`.
**Files**: `crates/tymuxd/Cargo.toml`

##### Task 1.1.1a: Add the dependency (~2 min)
- Add `constant_time_eq = "0.5"` to `[dependencies]` in
  `crates/tymuxd/Cargo.toml` (see ADR-001 for why this crate over
  `subtle`).
- Files: `crates/tymuxd/Cargo.toml`

#### Story 1.1.2: `BearerToken` newtype + `resolve_token` — flag/env precedence and empty-is-absent
**As an** operator, **I want** `--token`/`TYMUXD_TOKEN` resolved with a
predictable precedence and no empty-string footgun, **so that** I can
configure the daemon confidently and a typo'd empty value doesn't
silently disable auth. **As a** `tymuxd` maintainer, **I want** an empty
token to be *unrepresentable* once resolved, not just filtered once at
parse time, **so that** a future second token source can't reintroduce
the empty-token auth-bypass risk architecture review identified.
**Acceptance Criteria**:
- `BearerToken::parse` rejects an empty string.
  - *Given* `""`, *When* `BearerToken::parse("")` runs, *Then* it returns
    `None`.
- `BearerToken::parse` accepts any non-empty string.
  - *Given* `"s3cr3t"`, *When* `BearerToken::parse("s3cr3t")` runs, *Then*
    it returns `Some(BearerToken(...))` wrapping that value.
- `BearerToken`'s `Debug` impl never prints the value.
  - *Given* `BearerToken::parse("s3cr3t").unwrap()`, *When* it is
    formatted with `{:?}`, *Then* the output is exactly `"<redacted>"`,
    never containing `"s3cr3t"`.
- Explicit `--token` flag beats `TYMUXD_TOKEN` env var when both are set.
  - *Given* argv `["tymuxd", "--token", "flagval"]` and env
    `TYMUXD_TOKEN=envval`, *When* `resolve_token(&args)` runs, *Then* it
    returns `Some(BearerToken::parse("flagval").unwrap())`.
- `--token=value` (`=`-joined) form is also supported, not just
  space-separated.
  - *Given* argv `["tymuxd", "--token=flagval"]` and no `TYMUXD_TOKEN`
    env var, *When* `resolve_token(&args)` runs, *Then* it returns
    `Some(BearerToken::parse("flagval").unwrap())` — matching the
    space-separated form's result (adversarial-review.md Concern 2:
    `clap` supports both forms for free on the `tymux-cli` side; the
    hand-rolled parser now matches rather than silently falling through
    to `TYMUXD_TOKEN`/the fail-fast gate on the untested `=`-form).
- `TYMUXD_TOKEN` alone is used when no flag is passed.
  - *Given* argv `["tymuxd"]` and env `TYMUXD_TOKEN=envval`, *When*
    `resolve_token(&args)` runs, *Then* it returns
    `Some(BearerToken::parse("envval").unwrap())`.
- Empty string from either source is treated as absent.
  - *Given* argv `["tymuxd", "--token", ""]` and no `TYMUXD_TOKEN` env
    var, *When* `resolve_token(&args)` runs, *Then* it returns `None`
    (not `Some(BearerToken(""))` — which `BearerToken::parse` makes
    impossible to construct in the first place).
- Neither source present returns `None`.
  - *Given* argv `["tymuxd"]` and no `TYMUXD_TOKEN` env var, *When*
    `resolve_token(&args)` runs, *Then* it returns `None`.
**Files**: `crates/tymuxd/src/auth.rs` (new), `crates/tymuxd/src/main.rs`
(module declaration only)

##### Task 1.1.2a: Create the `auth` module (~2 min)
- Create `crates/tymuxd/src/auth.rs` with a module doc comment:
  ```rust
  //! Bearer-token auth for a non-loopback-bound tymuxd: token resolution
  //! (`resolve_token`), the fail-fast startup gate
  //! (`check_non_loopback_requires_token`), and the gRPC request gate
  //! (`BearerAuthInterceptor`). Extracted from `main.rs` during
  //! architecture review to keep the god-file from absorbing another
  //! concern (see plan.md's Pattern Decisions).
  ```
- Add `mod auth;` to `crates/tymuxd/src/main.rs`'s existing module
  declarations.
- Files: `crates/tymuxd/src/auth.rs`, `crates/tymuxd/src/main.rs`

##### Task 1.1.2b: Define `BearerToken` (~5 min)
- In `crates/tymuxd/src/auth.rs`, add:
  ```rust
  /// The one shared, operator-supplied bearer secret. `parse` is the
  /// only constructor — an empty token is unrepresentable, closing the
  /// gap where "empty string counts as absent" was previously enforced
  /// by a single `.filter()` call a future second token source could
  /// bypass (architecture-review.md, first Concern).
  #[derive(Clone)]
  pub struct BearerToken(String);

  impl std::fmt::Debug for BearerToken {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
          write!(f, "<redacted>")
      }
  }

  impl BearerToken {
      /// The ONLY way to produce a `BearerToken`. Deliberately no
      /// `PartialEq`/`Eq` derive on the type — a derived `==` would be a
      /// second, non-constant-time equality path sitting right next to
      /// the required `constant_time_eq` call (Story 1.2.1); see
      /// ADR-001 for why that risk is taken seriously here.
      pub fn parse(raw: &str) -> Option<Self> {
          (!raw.is_empty()).then(|| Self(raw.to_string()))
      }

      pub fn as_bytes(&self) -> &[u8] {
          self.0.as_bytes()
      }
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.1.2c: Implement `resolve_token` (~5 min)
- In `crates/tymuxd/src/auth.rs`, near `BearerToken`:
  ```rust
  /// Resolves the configured bearer token for a non-loopback bind:
  /// `--token <value>` or `--token=<value>` on argv, falling back to
  /// `TYMUXD_TOKEN`. An explicit flag wins over the env var (ADR-002:
  /// hand-rolled, no clap, but the same flag-beats-env precedence
  /// tymux-cli gets from clap's `env=` attribute). An empty value from
  /// either source is treated as absent, never as "auth disabled with
  /// an empty secret" (research/pitfalls.md §5) — enforced by
  /// `BearerToken::parse`, not a bare filter, so it can't be
  /// accidentally bypassed if a third token source is ever added (see
  /// Unresolved Questions' `TYMUXD_TOKEN_FILE` note).
  ///
  /// Generate a token with `openssl rand -hex 32` if you don't already
  /// have one to configure.
  pub fn resolve_token(args: &[String]) -> Option<BearerToken> {
      let flag_value = args
          .iter()
          .position(|a| a == "--token")
          .and_then(|i| args.get(i + 1))
          .cloned()
          .or_else(|| {
              args.iter()
                  .find_map(|a| a.strip_prefix("--token=").map(|v| v.to_string()))
          });
      let env_value = std::env::var("TYMUXD_TOKEN").ok();
      flag_value.or(env_value).and_then(|t| BearerToken::parse(&t))
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.1.2d: Unit tests for `BearerToken` and `resolve_token` (~5 min)
- `BearerToken` tests: `bearer_token_parse_rejects_empty_string`,
  `bearer_token_parse_accepts_non_empty_string`,
  `bearer_token_debug_always_prints_redacted`.
- `resolve_token` tests: `resolve_token_prefers_explicit_flag_over_env_var`,
  `resolve_token_supports_equals_joined_flag_form`,
  `resolve_token_falls_back_to_env_var_when_no_flag`,
  `resolve_token_treats_empty_flag_value_as_absent`,
  `resolve_token_returns_none_when_neither_source_present`, in
  `crates/tymuxd/src/auth.rs`'s `#[cfg(test)]` module, using
  `std::env::set_var`/`remove_var` around each env-dependent case.
- Files: `crates/tymuxd/src/auth.rs`

#### Story 1.1.3: Fail-fast non-loopback startup gate
**As an** operator, **I want** `tymuxd` to refuse to start on a
non-loopback bind with no token configured, **so that** the RCE gap is
closed outright instead of merely logged.
**Acceptance Criteria**:
- Non-loopback bind with a valid token starts normally.
  - *Given* `TYMUXD_ADDR=0.0.0.0:0` and `TYMUXD_TOKEN=s3cr3t`, *When*
    `tymuxd` starts, *Then* it logs the existing non-loopback warning and
    proceeds to serve (`main()` does not return early with an error).
- Non-loopback bind with no token refuses to start.
  - *Given* `TYMUXD_ADDR=0.0.0.0:0` and neither `--token` nor
    `TYMUXD_TOKEN` set, *When* `tymuxd` starts, *Then* it prints the
    failure message to stderr as clean, literal text (real newlines, no
    surrounding quotes, no Rust `Debug`-formatting artifacts — see Task
    1.1.3b's `eprintln!`-then-`exit(1)` fix, not a `?`-propagated
    `Result`) and exits with status 1, before `sessions_dir` is touched,
    naming `--token`/`TYMUXD_TOKEN` and the non-loopback risk (per
    `research/ux.md` §2's proposed wording).
- Empty-string token on a non-loopback bind fails identically to no token
  at all (named test, per `research/pitfalls.md` §5's sharpest edge
  case).
  - *Given* `TYMUXD_ADDR=0.0.0.0:0` and `TYMUXD_TOKEN=""`, *When* `tymuxd`
    starts, *Then* it fails exactly as the no-token case above — not
    treated as "auth disabled."
- Loopback bind is unaffected regardless of token.
  - *Given* `TYMUXD_ADDR=127.0.0.1:0` and no token configured, *When*
    `tymuxd` starts, *Then* it starts exactly as it does today (info log,
    no warning, no error).
**Files**: `crates/tymuxd/src/auth.rs` (gate function + its tests),
`crates/tymuxd/src/main.rs` (call-site wiring only)

##### Task 1.1.3a: Extract a pure gate-check function (~5 min)
- In `crates/tymuxd/src/auth.rs`, add alongside `resolve_token`:
  ```rust
  /// The fail-fast invariant this feature exists to enforce: a
  /// non-loopback bind must have a (non-empty, already-guaranteed by
  /// `BearerToken::parse`) token. Extracted as a pure function so it's
  /// testable without a real network bind.
  pub fn check_non_loopback_requires_token(
      is_loopback: bool,
      token: Option<&BearerToken>,
  ) -> Result<(), String> {
      if !is_loopback && token.is_none() {
          return Err(
              "failed to start: bound to non-loopback address with no token configured.\n\
               Set --token or TYMUXD_TOKEN before binding tymuxd to a non-loopback address — \
               this port would otherwise let any network client run arbitrary commands.\n\
               (Loopback binds, e.g. 127.0.0.1, never require a token. Generate one with \
               `openssl rand -hex 32` if you don't already have one.)".to_string(),
          );
      }
      Ok(())
  }
  ```
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.1.3b: Wire the gate into `main()` (~5 min)
- At `main.rs:1238-1247`, replace the existing `if
  !socket_addr.ip().is_loopback() { tracing::warn!(...) }` block with:
  ```rust
  let args: Vec<String> = std::env::args().collect();
  let configured_token = auth::resolve_token(&args);
  let is_loopback = socket_addr.ip().is_loopback();

  // NOT `.map_err(...)?` — main()'s return type is
  // `Result<(), Box<dyn std::error::Error>>`, and Rust's default
  // Termination impl reports a returned Err via its `Debug` impl, not
  // `Display`. A `String` (converted to `Box<dyn Error>` via `?`) prints
  // as `Error: "the message\nwith literal backslash-n text and quotes"`
  // — exactly the Debug-dump this message's multi-line formatting is
  // designed to avoid (empirically confirmed: reproduced this exact
  // pattern standalone and got literal `\n` in the output, not real
  // newlines). Print directly and exit instead, matching this repo's
  // "one clean line" convention in spirit even though the pre-existing
  // `sessions_dir` precedent (main.rs:1254-1259) has this same latent
  // bug — less visible there only because that message has no embedded
  // newline. Not fixing sessions_dir's version here (out of this
  // feature's scope), but not repeating its mistake either.
  if let Err(e) = auth::check_non_loopback_requires_token(is_loopback, configured_token.as_ref()) {
      eprintln!("Error: {e} (bind address: {socket_addr})");
      std::process::exit(1);
  }

  if !is_loopback {
      tracing::warn!(
          %socket_addr,
          "tymuxd is binding to a non-loopback address; bearer-token auth is enforced on every call"
      );
  } else {
      tracing::info!(
          %socket_addr,
          "tymuxd binding to loopback; no auth required (if this daemon is reachable through a reverse proxy or tunnel, loopback auto-exemption does not protect you — bind non-loopback and set --token/TYMUXD_TOKEN instead)"
      );
  }
  ```
- `configured_token` (`Option<auth::BearerToken>`) must stay in scope
  through to Story 1.2.2's server-construction call site (~40 lines
  later in the same function) — no intermediate scoping needed since
  it's a single straight-line `main()`. This is the entirety of Epic
  1.1/1.2/1.4's footprint on `main.rs` beyond the `mod auth;`
  declaration and the server-construction branch (Story 1.2.2).
- Files: `crates/tymuxd/src/main.rs`

##### Task 1.1.3c: Unit tests for `check_non_loopback_requires_token` (~5 min)
- In `crates/tymuxd/src/auth.rs`, 4 tests covering the four ACs directly
  against the pure function (non-loopback+token → `Ok`;
  non-loopback+`None` → `Err`; non-loopback+`Some(BearerToken(""))` is
  not a real input to this function since `BearerToken::parse` makes it
  unconstructible — instead add a test proving that composition:
  `resolve_token` on an empty-flag input feeds `None` into
  `check_non_loopback_requires_token`, which then errors; loopback+`None`
  → `Ok`).
- Files: `crates/tymuxd/src/auth.rs`

### Epic 1.2: gRPC interceptor and server wiring
**Goal**: Every `TymuxService` RPC on a non-loopback bind is gated by a
constant-time bearer-token check, uniformly for unary calls and the
`Attach` bidi stream, with loopback binds unaffected.

#### Story 1.2.1: `BearerAuthInterceptor` — the request gate
**As** `tymuxd`, **I want** a `tonic::service::Interceptor` that validates
the `authorization` metadata against the configured token, **so that**
every RPC on a non-loopback bind is gated at one enforcement point.
**Acceptance Criteria**:
- A valid token is accepted unchanged.
  - *Given* `BearerAuthInterceptor { token: BearerToken::parse("s3cr3t").unwrap(), rejection_count: Arc::new(AtomicI64::new(0)) }`
    and `req = Request::new(())` with `req.metadata_mut().insert("authorization", "Bearer s3cr3t".parse().unwrap())`,
    *When* `interceptor.call(req)` runs, *Then* it returns `Ok(req)`.
- Missing `authorization` metadata is rejected as "missing bearer token".
  - *Given* the same interceptor and `req = Request::new(())` with no
    `authorization` metadata entry, *When* `interceptor.call(req)` runs,
    *Then* it returns `Err(Status::unauthenticated("missing bearer
    token"))` and `rejection_count` increments by 1.
- A wrong token is rejected as "invalid bearer token".
  - *Given* the same interceptor and `req` with
    `authorization: "Bearer wrongvalue"`, *When* `interceptor.call(req)`
    runs, *Then* it returns `Err(Status::unauthenticated("invalid bearer
    token"))` and `rejection_count` increments by 1.
- A malformed `authorization` header (present, but no usable bearer
  value) fails safe into the same "missing bearer token" rejection —
  named and tested, not just an incidental side effect of the `match`'s
  shape (adversarial-review.md Concern 4).
  - *Given* the same interceptor and `req` with `authorization: "Bearer"`
    (no trailing value), *When* `interceptor.call(req)` runs, *Then* it
    returns `Err(Status::unauthenticated("missing bearer token"))` and
    `rejection_count` increments by 1 — identical outcome to the
    no-header case.
- The comparison uses `constant_time_eq`, not string `==`.
  - *Given* the implementation, *When* the code is read, *Then* the
    token-vs-token comparison calls
    `constant_time_eq::constant_time_eq(...)`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 1.2.1a: Define `BearerAuthInterceptor` (~5 min)
- In `crates/tymuxd/src/auth.rs`, add:
  ```rust
  use std::sync::atomic::{AtomicI64, Ordering};
  use std::sync::Arc;
  use tonic::Status;

  /// Gates every `TymuxService` RPC behind the configured bearer token
  /// when tymuxd is bound non-loopback. Owns its own rejection counter
  /// rather than reaching into `TymuxDaemon`/`Engine` — auth is a pure
  /// request-gate concern, never consulted by RPC handler bodies
  /// (research/architecture.md §2).
  #[derive(Clone)]
  pub struct BearerAuthInterceptor {
      token: BearerToken,
      rejection_count: Arc<AtomicI64>,
  }

  impl BearerAuthInterceptor {
      pub fn new(token: BearerToken, rejection_count: Arc<AtomicI64>) -> Self {
          Self { token, rejection_count }
      }
  }

  impl tonic::service::Interceptor for BearerAuthInterceptor {
      fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
          let peer = req
              .remote_addr()
              .map(|a| a.to_string())
              .unwrap_or_else(|| "unknown".to_string());

          let presented = req
              .metadata()
              .get("authorization")
              .and_then(|v| v.to_str().ok())
              .and_then(|v| v.strip_prefix("Bearer "));

          match presented {
              None => {
                  self.rejection_count.fetch_add(1, Ordering::SeqCst);
                  tracing::warn!(peer = %peer, "rejected TymuxService call: missing bearer token");
                  Err(Status::unauthenticated("missing bearer token"))
              }
              Some(supplied)
                  if constant_time_eq::constant_time_eq(supplied.as_bytes(), self.token.as_bytes()) =>
              {
                  Ok(req)
              }
              Some(_) => {
                  self.rejection_count.fetch_add(1, Ordering::SeqCst);
                  tracing::warn!(peer = %peer, "rejected TymuxService call: invalid bearer token");
                  Err(Status::unauthenticated("invalid bearer token"))
              }
          }
      }
  }
  ```
  (This also satisfies Story 1.4.1's logging ACs — no separate
  implementation step needed. Note `presented` collapses "no header,"
  "header not ASCII/UTF-8," "header without a `Bearer ` prefix," and
  "`Bearer` with nothing after it" all into the same `None` arm, which
  the malformed-header AC above pins down as intentional, fail-closed
  behavior — not an untested accident.)
- Metadata key is lowercase `"authorization"` (HTTP/2 requires lowercase
  ASCII header names — `research/pitfalls.md` §4), matched by every other
  client stack in this plan.
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.2.1b: Unit tests for the interceptor's four cases (~5 min)
- `bearer_auth_interceptor_accepts_matching_token`,
  `bearer_auth_interceptor_rejects_missing_token`,
  `bearer_auth_interceptor_rejects_wrong_token`,
  `bearer_auth_interceptor_rejects_malformed_authorization_header`
  (covering at least `"Bearer"` with no trailing value), constructing
  `Request::new(())` directly and inserting/omitting metadata as each
  case requires; assert both the `Result` shape and the counter value.
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.2.1c: Token-leak grep check (~2 min)
- Manually confirm (or add a `grep`-based test/CI check, at the
  implementer's discretion) that no `{:?}`/`{}` formatting of `self.token`,
  the full `MetadataMap`, or the whole `Request` appears anywhere in the
  new code — `research/pitfalls.md` §1's named near-miss risk. This
  check is now largely structural rather than a manual grep: `BearerToken`
  (Story 1.1.2) has no `Debug` derive and its manual `Debug` impl always
  prints `"<redacted>"`, so `{:?}`-formatting `self.token` (or any struct
  containing it) can no longer leak the value — a leak would require
  explicit misuse of `BearerToken`'s private `.0` field or `.as_bytes()`.
  Still worth a final read-through for `MetadataMap`/`Request`-level
  Debug dumps, which `BearerToken` doesn't protect against.
- Files: `crates/tymuxd/src/auth.rs`

#### Story 1.2.2: Wire the interceptor into server construction
**As** `tymuxd`, **I want** the production `Server::builder()` call site
to attach `BearerAuthInterceptor` only when bound non-loopback (with a
token), **so that** loopback behavior is provably unchanged and
non-loopback is provably gated — including the `Attach` bidi stream.
**Acceptance Criteria**:
- Non-loopback + token: a request missing a token is rejected before
  reaching any handler.
  - *Given* `tymuxd` started with `TYMUXD_ADDR=0.0.0.0:0` and
    `TYMUXD_TOKEN=s3cr3t`, *When* a client calls `ListSessions` with no
    `authorization` header, *Then* the RPC fails with
    `tonic::Code::Unauthenticated`.
- Non-loopback + token: the correct token succeeds.
  - *Given* the same daemon, *When* a client calls `ListSessions` with
    `authorization: Bearer s3cr3t`, *Then* the RPC succeeds.
- `Attach`'s bidi stream is gated identically to a unary call.
  - *Given* the same daemon, *When* a client opens `Attach` with no
    `authorization` header, *Then* the stream fails immediately with
    `Unauthenticated` (asserted within a bounded timeout — not a hang,
    not a successful open followed by a later error).
- Loopback bind is unaffected.
  - *Given* `tymuxd` bound to `127.0.0.1:0` (today's existing test-harness
    default), *When* any RPC is called with no `authorization` metadata
    at all, *Then* it succeeds exactly as it does today — the existing
    full `cargo test -p tymuxd` suite passes unmodified.
**Files**: `crates/tymuxd/src/main.rs`

##### Task 1.2.2a: Branch the production server-construction call site (~5 min)
- Replace `main.rs:1286-1291`:
  ```rust
  use crate::auth::BearerAuthInterceptor;

  let rejection_count = Arc::new(AtomicI64::new(0));
  tracing::info!(%addr, "tymuxd listening");
  if let Some(token) = configured_token {
      Server::builder()
          .http2_keepalive_interval(Some(Duration::from_secs(30)))
          .http2_keepalive_timeout(Some(Duration::from_secs(10)))
          .add_service(TymuxServiceServer::with_interceptor(
              daemon,
              BearerAuthInterceptor::new(token, rejection_count),
          ))
          .serve_with_shutdown(socket_addr, shutdown_signal())
          .await?;
  } else {
      Server::builder()
          .http2_keepalive_interval(Some(Duration::from_secs(30)))
          .http2_keepalive_timeout(Some(Duration::from_secs(10)))
          .add_service(TymuxServiceServer::new(daemon))
          .serve_with_shutdown(socket_addr, shutdown_signal())
          .await?;
  }
  ```
  (Two separate tail calls per branch, rather than unifying into one
  `Router` variable via type erasure — simpler to read and equally
  correct; `research/architecture.md` §1 confirms both arms would compile
  either way via `Router::add_service`'s `BoxCloneService` type erasure,
  but that machinery isn't needed here.)
- The two test-only call sites (`main.rs:1406-1408`, `main.rs:3166-3168`)
  are untouched — both bind `127.0.0.1:0` (loopback) and keep using
  `TymuxServiceServer::new(daemon)` unwrapped.
- Files: `crates/tymuxd/src/main.rs`

##### Task 1.2.2b: Non-loopback test-harness helper (~5 min)
- Add a helper near the existing `spawn_test_server` (`main.rs:1406`)
  that spawns a daemon bound to `0.0.0.0:0` (non-loopback per
  `Ipv4Addr::is_loopback()`, but still local-only in practice — safe for
  CI) with a `BearerAuthInterceptor` wired in via a caller-supplied
  token, returning the bound address and a shutdown handle in the same
  shape `spawn_test_server` already returns.
- Files: `crates/tymuxd/src/main.rs`

##### Task 1.2.2c: Integration tests for unary reject/accept (~5 min)
- `non_loopback_server_rejects_list_sessions_with_missing_token` and
  `non_loopback_server_accepts_list_sessions_with_correct_token`, using
  Task 1.2.2b's harness and a real `TymuxServiceClient` connecting over
  the bound address.
- Files: `crates/tymuxd/src/main.rs`

##### Task 1.2.2d: Integration test for `Attach` rejection (~5 min)
- `non_loopback_server_rejects_attach_stream_with_missing_token_promptly`
  — opens `Attach` with no token, asserts the first response/error
  arrives as `Unauthenticated` within a bounded `tokio::time::timeout`
  (e.g. 5s), not a hang (`research/pitfalls.md` §3's named risk to
  verify empirically, not assume).
- Files: `crates/tymuxd/src/main.rs`

##### Task 1.2.2e: Confirm no loopback regression (~3 min)
- Run `cargo test -p tymuxd` in full and confirm the existing suite
  passes unmodified — the loopback branch's behavior (and the two
  test-only `Server::builder()` call sites) is untouched by this story.
- Files: none (verification only)

### Epic 1.3: PTY environment-leak fix
**Goal**: A pane spawned by a non-loopback `tymuxd` running with
`TYMUXD_TOKEN` set does not leak that secret into the pane's own process
environment (`research/pitfalls.md` §2, HIGH PRIORITY — an explicit,
first-class story, not a footnote).

**Scope-amendment note** (adversarial-review.md Concern 1): this epic was
folded into the plan during Phase 3 (this planning phase) from a Phase 2
research finding (`research/pitfalls.md` §2), not from `requirements.md`'s
original Scope section — `requirements.md`'s Users/Consumers section
never named `crates/tymux-core` for modification, and this touches
`crates/tymux-core/src/pane.rs`, a third crate outside `tymuxd`/
`tymux-cli`. It's included anyway, as a deliberate decision rather than
silent scope creep: before this feature, `tymuxd` held no secret that
could leak into a spawned pane's environment; this feature's own
`TYMUXD_TOKEN` is what creates the leak vector
(`portable_pty::CommandBuilder`'s default full-environment inheritance,
confirmed by direct read of `pane.rs:210-214`), and it leaks precisely
into the shared, non-loopback, multi-user scenario this whole feature
targets — handing every authenticated user's shell the daemon's one
master secret via `env`/`printenv` would undermine the exact security
boundary this feature exists to build. Blocking this fix on a separate
`requirements.md` revision cycle would ship v1 with a known, exploitable
hole in the interim, which is worse than the process gap of including it
without a prior requirements checkpoint.

#### Story 1.3.1: Strip `TYMUXD_TOKEN` from every spawned pane's environment
**As** an authenticated user attached to a pane on a shared, non-loopback
`tymuxd`, **I want** the daemon's own bearer token absent from my shell's
environment, **so that** the secret meant to keep unauthorized clients
out isn't handed to me (or anything I run) via `env`/`printenv`.
**Acceptance Criteria**:
- `TYMUXD_TOKEN` is absent from a spawned pane's environment.
  - *Given* the test process has `TYMUXD_TOKEN=leaked-secret` set in its
    own environment before calling `Pane::spawn`, *When* a pane running
    `env` is spawned and its output captured, *Then* the captured output
    does not contain the substring `TYMUXD_TOKEN`.
- Other environment variables are still inherited normally (the fix is
  targeted, not a blanket `.env_clear()`).
  - *Given* the same setup with `TYMUX_TEST_CONTROL_VAR=visible` also set
    in the test process's environment, *When* the same pane's `env`
    output is captured, *Then* it DOES contain
    `TYMUX_TEST_CONTROL_VAR=visible`.
**Files**: `crates/tymux-core/src/pane.rs`

##### Task 1.3.1a: Add `env_remove` in `spawn_internal` (~2 min)
- In `spawn_internal` (`crates/tymux-core/src/pane.rs`, currently around
  line 210-213):
  ```rust
  let mut cmd = CommandBuilder::new(command);
  if let Some(cwd) = cwd {
      cmd.cwd(cwd);
  }
  // Never let the daemon's own bearer secret (if TYMUXD_TOKEN is set in
  // tymuxd's process environment) reach a spawned pane — portable_pty's
  // CommandBuilder inherits the full parent environment by default, and
  // this is the daemon's own secret, not something a pane's user should
  // ever see via `env`/`printenv` (research/pitfalls.md §2, HIGH
  // PRIORITY).
  cmd.env_remove("TYMUXD_TOKEN");
  let child = pair.slave.spawn_command(cmd)?;
  ```
- Files: `crates/tymux-core/src/pane.rs`

##### Task 1.3.1b: Regression test (~5 min)
- `spawn_should_not_leak_tymuxd_token_into_pane_environment`: sets
  `TYMUXD_TOKEN` (and a control var) via `std::env::set_var` in the test
  process, spawns a pane running `env`, reads captured output (mirroring
  this file's existing pattern for asserting on a spawned pane's stdout,
  if one exists — otherwise via the pane's output channel/reader
  directly), asserts absence of `TYMUXD_TOKEN` and presence of the
  control var. Clean up the env vars after the test (`std::env::remove_var`)
  to avoid bleeding into other tests in the same process.
- Files: `crates/tymux-core/src/pane.rs`

### Epic 1.4: Observability
**Goal**: A rejected request is visible to an operator without ever
exposing the token.

#### Story 1.4.1: Rejection logging and counter
**As an** operator, **I want** rejected auth attempts logged and counted,
**so that** I can distinguish "nobody's hitting this" from "someone keeps
trying and failing," without any risk of the token itself being logged.
**Acceptance Criteria**:
- A rejected request logs at `warn` with the peer address, never the
  token.
  - *Given* a request from peer `203.0.113.5:54321` rejected for a
    missing token, *When* `BearerAuthInterceptor::call` runs, *Then* a
    `tracing::warn!` record is emitted containing `203.0.113.5:54321` and
    `missing bearer token`, and the configured token value does not
    appear anywhere in that record's fields or message.
- The rejection counter increments once per rejected call, not per
  accepted one.
  - *Given* 3 rejected calls and 2 accepted calls issued in sequence
    against one `BearerAuthInterceptor` instance, *When* all 5 have run,
    *Then* `rejection_count.load(Ordering::SeqCst) == 3`.
**Files**: `crates/tymuxd/src/auth.rs`

##### Task 1.4.1a: Confirm logging is already present (~1 min)
- Task 1.2.1a's `BearerAuthInterceptor::call` implementation already
  includes the `tracing::warn!(peer = %peer, ...)` calls on both
  rejection branches — no additional code needed; this task is a
  checkpoint, not new work.
- Files: none

##### Task 1.4.1b: Counter-value unit test (~3 min)
- `bearer_auth_interceptor_rejection_counter_counts_only_rejections`:
  issues a mixed sequence of 3 rejected + 2 accepted calls against one
  interceptor instance, asserts the final counter value is exactly 3
  (this may already be partially covered by Task 1.2.1b's four tests if
  they share one interceptor instance — otherwise a new, dedicated test).
- Files: `crates/tymuxd/src/auth.rs`

##### Task 1.4.1c: Peer-address-in-log unit test (~5 min)
- Task 1.2.1b's tests construct bare `Request::new(())`, which has no
  `remote_addr()` — `req.remote_addr()` reads a `TcpConnectInfo`
  extension that a hand-constructed `Request::new(())` never carries, so
  none of those tests exercise the real-address branch of
  `BearerAuthInterceptor::call`'s `peer` computation (adversarial-review.md
  Concern 3). Add
  `bearer_auth_interceptor_logs_real_peer_address_when_available`: build
  the request as `let mut req = Request::new(()); req.extensions_mut()
  .insert(TcpConnectInfo { local_addr: None, remote_addr:
  Some("203.0.113.5:54321".parse().unwrap()) });` (both fields are
  public, confirmed against `tonic-0.12.3`'s
  `transport/server/conn.rs:69-76`), omit `authorization` metadata so the
  call is rejected, and assert — via a `tracing` test subscriber/capture
  layer (mirroring however this file's existing tests, if any, assert on
  emitted `tracing` records; otherwise a minimal `tracing_subscriber`
  test writer) — that the emitted `warn` record's `peer` field is exactly
  `"203.0.113.5:54321"`, not `"unknown"`. This is what actually proves
  Story 1.4.1's peer-address AC; Task 1.2.1b's tests only ever hit the
  `"unknown"` fallback.
- Files: `crates/tymuxd/src/auth.rs`

---

## Phase 2: tymux-cli (Rust client)

### Epic 2.1: Token flag and client-side interceptor
**Goal**: `tymux-cli` can be told a bearer token via `--token`/
`TYMUXD_TOKEN` and attaches it to every outgoing call, including
`Attach`'s bidi stream.

#### Story 2.1.1: `--token` flag on `Cli`
**As a** `tymux-cli` user, **I want** a `--token` flag (with
`TYMUXD_TOKEN` env fallback), **so that** I can authenticate against a
non-loopback `tymuxd` the same way I'd configure any other client
credential, **and** I want `--help` to never echo my live token value
when `TYMUXD_TOKEN` happens to be set in my shell.
**Acceptance Criteria**:
- The workspace `clap` dependency compiles with `env = "..."` attributes
  available.
  - *Given* this story's `Cargo.toml` change applied, *When* `cargo build
    -p tymux-cli` runs, *Then* it compiles successfully (adversarial-review.md
    Blocker 1 — without this, Task 2.1.1b's `#[arg(..., env =
    "TYMUXD_TOKEN")]` does not compile at all).
- `--token` flag parses.
  - *Given* invocation `tymux --token s3cr3t ls`, *When* `Cli::parse()`
    runs, *Then* `cli.token == Some("s3cr3t".to_string())`.
- `TYMUXD_TOKEN` env var is used as fallback.
  - *Given* no `--token` flag and env `TYMUXD_TOKEN=s3cr3t`, *When*
    `Cli::parse()` runs, *Then* `cli.token == Some("s3cr3t".to_string())`.
- Explicit flag overrides the env var.
  - *Given* `--token flagval` and env `TYMUXD_TOKEN=envval`, *When*
    `Cli::parse()` runs, *Then* `cli.token == Some("flagval".to_string())`
    (clap's built-in `env=` precedence — not hand-rolled here, unlike
    `tymuxd`; see Pattern Decisions).
- `--help` never echoes the live `TYMUXD_TOKEN` value.
  - *Given* env `TYMUXD_TOKEN=s3cr3t-live-value` set in the test's
    process environment, *When* `Cli::command().render_help()` (or
    equivalent clap test-friendly help-rendering API) is called, *Then*
    the rendered help text does not contain the substring
    `"s3cr3t-live-value"` anywhere (adversarial-review.md Blocker 2:
    without `hide_env_values = true`, clap's default behavior prints
    `[env: TYMUXD_TOKEN=s3cr3t-live-value]`, a direct violation of
    `requirements.md`'s "must never appear... at any level" NFR).
**Files**: `Cargo.toml` (workspace), `crates/tymux-cli/src/main.rs`

##### Task 2.1.1a: Enable clap's `env` feature (~2 min)
- In the root `Cargo.toml`, change the workspace `clap` dependency from
  `clap = { version = "4", features = ["derive"] }` to
  `clap = { version = "4", features = ["derive", "env"] }`. This is a
  workspace-level, purely additive change — `crates/tymux-cli/Cargo.toml`
  already inherits `clap` via `{ workspace = true }`, so no other
  `Cargo.toml` needs editing, and every existing flag on `tymux-cli`'s
  `Cli` (none of which use `env = "..."` today) is unaffected.
- Files: `Cargo.toml`

##### Task 2.1.1b: Add the field, with `hide_env_values` (~2 min)
- In `struct Cli` (`main.rs:180-183`), add, matching the `no_status_bar`
  field's existing doc-comment style two lines above it:
  ```rust
  /// Bearer token to authenticate against a non-loopback tymuxd.
  /// Generate one with `openssl rand -hex 32` if you don't already have
  /// one configured on the daemon side.
  #[arg(long, global = true, env = "TYMUXD_TOKEN", hide_env_values = true)]
  token: Option<String>,
  ```
  `hide_env_values = true` is not optional here — without it, `--help`
  prints the live `TYMUXD_TOKEN` value (see this story's AC and
  adversarial-review.md Blocker 2).
- Files: `crates/tymux-cli/src/main.rs`

##### Task 2.1.1c: Unit tests for the flag/env ACs (~5 min)
- `token_flag_parses`, `token_env_var_used_as_fallback`,
  `token_flag_overrides_env_var`, using the existing `parse(&[...])` test
  helper (`main.rs` tests module) plus `std::env::set_var`/`remove_var`
  around the env-fallback cases.
- Files: `crates/tymux-cli/src/main.rs`

##### Task 2.1.1d: Test that `--help` doesn't leak the token value (~3 min)
- `cli_help_does_not_echo_configured_token_value`: with
  `TYMUXD_TOKEN=s3cr3t-live-value` set via `std::env::set_var` for the
  duration of the test, render help text via `Cli::command().render_help()`
  (or `Cli::command().render_long_help()` — whichever clap 4.6's
  test-friendly API exposes; confirm exact method name during
  implementation) and assert the rendered string does not contain
  `"s3cr3t-live-value"`. Clean up the env var after the test.
- Files: `crates/tymux-cli/src/main.rs`

#### Story 2.1.2: Attach the token via a client-side interceptor
**As** `tymux-cli`, **I want** every outgoing RPC (unary and `Attach`'s
bidi stream) to carry the configured bearer token, **so that** the CLI
actually authenticates against a token-gated daemon end to end.
**Acceptance Criteria**:
- With a token configured, every call carries the header.
  - *Given* `cli.token == Some("s3cr3t")` and a non-loopback,
    token-gated `tymuxd` (Story 1.2.2b's harness, token `s3cr3t`), *When*
    `run()` issues `ListSessions`, *Then* the call succeeds (proves the
    header was attached and accepted).
- With no token configured, no header is set and loopback is unaffected.
  - *Given* `cli.token == None`, *When* any RPC is issued against a
    loopback (untokened) daemon, *Then* the call succeeds exactly as
    before this feature — no `authorization` metadata entry is ever
    constructed.
- `Attach`'s bidi stream carries the token too.
  - *Given* `cli.token == Some("s3cr3t")` and the same token-gated
    daemon, *When* `attach()` opens its bidi stream, *Then* it succeeds
    (proves the interceptor applies to the streaming client call, not
    just unary calls — mirrors the cross-language `Attach`-specific
    requirement applied here to the Rust client for symmetry).
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 2.1.2a: Define `BearerToken` (mirrored) and `BearerAuth` (~7 min)
- Add, near `friendly_message`, a `tymux-cli`-local mirror of `tymuxd`'s
  `BearerToken` (Story 1.1.2) — architecture review's remediation calls
  for the same newtype in both binaries; they're genuinely different
  binaries with different `Interceptor` implementations (see Pattern
  Decisions), so this is an independent, identical-shaped definition, not
  a shared crate:
  ```rust
  use tonic::service::interceptor::InterceptedService;

  /// Mirrors tymuxd's `BearerToken` (crates/tymuxd/src/auth.rs) — same
  /// invariant (empty token unrepresentable), same reason (no `Debug`/
  /// `PartialEq` derive to prevent a value leak or an accidental
  /// non-constant-time comparison). Not shared as a library type: this
  /// is the client side, tymuxd's is the server side, and they have no
  /// other reason to depend on each other.
  #[derive(Clone)]
  struct BearerToken(String);

  impl std::fmt::Debug for BearerToken {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
          write!(f, "<redacted>")
      }
  }

  impl BearerToken {
      fn parse(raw: &str) -> Option<Self> {
          (!raw.is_empty()).then(|| Self(raw.to_string()))
      }
      fn as_str(&self) -> &str {
          &self.0
      }
  }

  /// Attaches the configured bearer token to every outgoing RPC as
  /// `authorization: Bearer <token>`, unary and streaming (`Attach`)
  /// alike. No-ops when no token is configured — loopback usage must
  /// stay byte-for-byte unaffected.
  #[derive(Clone)]
  struct BearerAuth {
      token: Option<BearerToken>,
  }

  impl tonic::service::Interceptor for BearerAuth {
      fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
          if let Some(token) = &self.token {
              let value = format!("Bearer {}", token.as_str())
                  .parse()
                  .map_err(|_| tonic::Status::internal("token contains invalid header characters"))?;
              req.metadata_mut().insert("authorization", value);
          }
          Ok(req)
      }
  }
  ```
  (`tonic::Status` fully-qualified here — unlike `crates/tymuxd/src/main.rs`,
  `crates/tymux-cli/src/main.rs` has no bare `use tonic::Status;` today,
  only `use tonic::Request;` — confirmed by reading the file's current
  imports, `main.rs:9-13`.)
- Files: `crates/tymux-cli/src/main.rs`

##### Task 2.1.2b: Switch the client type and construction (~5 min)
- Change all 6 occurrences of `TymuxServiceClient<Channel>`
  (`main.rs:145, 438, 457, 492, 798`, plus the construction site) to
  `TymuxServiceClient<InterceptedService<Channel, BearerAuth>>`.
- At `main.rs:278`, replace `TymuxServiceClient::new(channel)` with:
  ```rust
  let mut client = TymuxServiceClient::with_interceptor(
      channel,
      BearerAuth {
          token: cli.token.as_deref().and_then(BearerToken::parse),
      },
  );
  ```
  `cli.token` stays `Option<String>` (clap's field type, Story 2.1.1) —
  converted to `Option<BearerToken>` only at this construction site, the
  one place a `BearerAuth` is built. An empty `--token ""`/`TYMUXD_TOKEN=""`
  value maps to `None` here too, via `BearerToken::parse`, for the same
  reason it does on the `tymuxd` side (empty is absent, not "auth
  disabled with an empty secret").
- Files: `crates/tymux-cli/src/main.rs`

##### Task 2.1.2c: Integration test for unary reject/accept via CLI's client (~5 min)
- Reusing Story 1.2.2b's harness: a test constructing a
  `TymuxServiceClient::with_interceptor(..., BearerAuth{token})` directly
  (mirroring `run()`'s construction) and calling `ListSessions` against
  the token-gated daemon, once with the correct token (succeeds) and once
  with `None` (fails with `Unauthenticated`).
- Files: `crates/tymux-cli/src/main.rs`

##### Task 2.1.2d: Integration test for `Attach` (~5 min)
- Same harness, opens `Attach` with the correct token configured and
  asserts the stream succeeds — proving `BearerAuth` applies to the
  streaming call path, not just unary.
- Files: `crates/tymux-cli/src/main.rs`

### Epic 2.2: CLI error UX for `Unauthenticated`
**Goal**: A rejected connection produces a distinct, actionable message —
not the generic `tonic::Status` fallthrough.

#### Story 2.2.1: `friendly_message`'s new `Unauthenticated` branch
**As a** `tymux-cli` user, **I want** a clear message when my connection
is rejected for a missing/wrong token, **so that** I can tell this case
apart from "daemon unreachable" or "session not found" without reading
raw status prose.
**Acceptance Criteria**:
- An `Unauthenticated` status gets the dedicated, remedy-naming message.
  - *Given* `e` wraps `tonic::Status::unauthenticated("missing bearer
    token")`, *When* `friendly_message(&e)` runs, *Then* it returns
    `"tymuxd rejected this connection: missing bearer token (set --token
    or TYMUXD_TOKEN to authenticate)"`.
- Other status codes are unaffected (no regression).
  - *Given* `e` wraps `tonic::Status::not_found("no such session: abc")`,
    *When* `friendly_message(&e)` runs, *Then* it still returns exactly
    `"no such session: abc"`, matching today's behavior.
**Files**: `crates/tymux-cli/src/main.rs`

##### Task 2.2.1a: Add the branch (~3 min)
- In `friendly_message` (`main.rs:259-269`), before the generic `Status`
  fallthrough:
  ```rust
  if let Some(status) = e.downcast_ref::<tonic::Status>() {
      if status.code() == tonic::Code::Unauthenticated {
          return format!(
              "tymuxd rejected this connection: {} (set --token or TYMUXD_TOKEN to authenticate)",
              status.message()
          );
      }
      return status.message().to_string();
  }
  ```
- Files: `crates/tymux-cli/src/main.rs`

##### Task 2.2.1b: Unit tests for both ACs (~3 min)
- `friendly_message_names_the_remedy_for_unauthenticated_status` and
  `friendly_message_unaffected_for_other_status_codes`, alongside the
  existing `friendly_message_unwraps_tonic_status_to_its_plain_text` test.
- Files: `crates/tymux-cli/src/main.rs`

---

## Phase 3: clients/go and clients/ts

### Epic 3.1: Go client token support
**Goal**: `clients/go` can authenticate against a token-gated `tymuxd`,
proven cross-language for both a unary RPC and the streaming `Attach`
RPC specifically.

#### Story 3.1.1: `authInterceptor` covering unary and streaming client calls
**As a** `clients/go` user, **I want** a token attached to every outgoing
call (unary and `Attach`'s stream alike), **so that** the Go client can
talk to a non-loopback, token-gated `tymuxd`.
**Acceptance Criteria**:
- Missing/wrong token is rejected on a unary call.
  - *Given* a token-gated non-loopback `tymuxd` (token `s3cr3t`) and a Go
    client constructed with token `""`, *When* `ListSessions` is called,
    *Then* the call returns an error and
    `connect.CodeOf(err) == connect.CodeUnauthenticated`.
- Correct token succeeds on a unary call.
  - *Given* the same daemon and a Go client constructed with token
    `s3cr3t`, *When* `ListSessions` is called, *Then* it succeeds.
- Missing/wrong token is rejected on the streaming `Attach` RPC
  specifically.
  - *Given* the same daemon and a Go client constructed with token `""`,
    *When* `Attach` is opened and a message sent, *Then* the call fails
    with `connect.CodeUnauthenticated` (proves `WrapStreamingClient`, not
    just `WrapUnary`, sets the header — `research/pitfalls.md` §4's named
    risk).
- Correct token succeeds on `Attach`.
  - *Given* the same daemon and a Go client constructed with token
    `s3cr3t`, *When* `Attach` is opened, *Then* it streams output
    normally.
**Files**: `clients/go/authinterceptor/authinterceptor.go`,
`clients/go/integration/integration_test.go`,
`clients/go/examples/list-sessions/main.go`,
`clients/go/examples/attach/main.go`

**Note on shape** (architecture-review.md, third Concern): the original
plan draft defined `authInterceptor` once in the integration test file
and told Task 3.1.1e to copy-paste the same ~20-line type into both
example `main.go` files — three copies of the same code, with three
chances to diverge on the next fix. `clients/go` has no hand-written
library package today (only generated `gen/`, plus `examples/`/
`integration/`), so this rewrites Task 3.1.1a/3.1.1e to create one first,
matching the single-definition-plus-reuse shape `clients/ts` already uses
for its own `authInterceptor`/`tymuxClient()` (Epic 3.2).

##### Task 3.1.1a: `authInterceptor` as its own package (~5 min)
- Create `clients/go/authinterceptor/authinterceptor.go`:
  ```go
  // Package authinterceptor provides a connect-go interceptor that
  // attaches a bearer token to every outgoing call, unary and streaming
  // alike. Shared by the integration tests and both example binaries so
  // the auth-header logic exists in exactly one place.
  package authinterceptor

  import (
      "context"

      "connectrpc.com/connect"
  )

  // Interceptor sets Authorization: Bearer <token> on both unary and
  // streaming client calls. connect-go's convenience UnaryInterceptorFunc
  // only implements WrapUnary — its WrapStreamingClient is a documented
  // no-op (verified against connectrpc.com/connect@v1.20.0), which would
  // silently exempt Attach from auth. Implementing the full
  // connect.Interceptor interface here avoids that gap. An empty Token
  // is a no-op, matching every other client stack's "empty is absent"
  // treatment.
  type Interceptor struct {
      Token string
  }

  func (a Interceptor) WrapUnary(next connect.UnaryFunc) connect.UnaryFunc {
      return func(ctx context.Context, req connect.AnyRequest) (connect.AnyResponse, error) {
          if a.Token != "" {
              req.Header().Set("Authorization", "Bearer "+a.Token)
          }
          return next(ctx, req)
      }
  }

  func (a Interceptor) WrapStreamingClient(next connect.StreamingClientFunc) connect.StreamingClientFunc {
      return func(ctx context.Context, spec connect.Spec) connect.StreamingClientConn {
          conn := next(ctx, spec)
          if a.Token != "" {
              conn.RequestHeader().Set("Authorization", "Bearer "+a.Token)
          }
          return conn
      }
  }

  func (a Interceptor) WrapStreamingHandler(next connect.StreamingHandlerFunc) connect.StreamingHandlerFunc {
      return next // client-only; no server-side handler wrapping needed here
  }
  ```
- Files: `clients/go/authinterceptor/authinterceptor.go`

##### Task 3.1.1b: Wire the package into the integration test's client constructor (~3 min)
- In `clients/go/integration/integration_test.go`, import
  `"github.com/tstapler/tymux/clients/go/authinterceptor"`. Extend
  `newClient(baseURL string)` to `newClient(baseURL, token string)`,
  passing `connect.WithInterceptors(authinterceptor.Interceptor{Token:
  token})` into `tymuxv1connect.NewTymuxServiceClient(...)`.
- Files: `clients/go/integration/integration_test.go`

##### Task 3.1.1c: Non-loopback + token variant of `startDaemon` (~5 min)
- Extend `startDaemon` (or add a sibling `startDaemonWithToken(t, token
  string) string`) to bind `0.0.0.0:<ephemeral>` and pass
  `TYMUXD_TOKEN=<token>` in `cmd.Env`, mirroring `crates/tymuxd`'s Story
  1.2.2b harness shape; existing tests keep using the current
  loopback/no-token `startDaemon` unchanged.
- Files: `clients/go/integration/integration_test.go`

##### Task 3.1.1d: Unary reject/accept tests (~5 min)
- `TestListSessionsRejectsMissingOrWrongToken` (AC1) and
  `TestListSessionsSucceedsWithCorrectToken` (AC2), using Task 3.1.1b/c.
- Files: `clients/go/integration/integration_test.go`

##### Task 3.1.1e: `Attach` reject/accept tests (~5 min)
- `TestAttachRejectsMissingOrWrongToken` (AC3) and
  `TestAttachSucceedsWithCorrectToken` (AC4).
- Files: `clients/go/integration/integration_test.go`

##### Task 3.1.1f: Wire the package into the example binaries (~5 min)
- Update `clients/go/examples/list-sessions/main.go` and
  `clients/go/examples/attach/main.go` to import
  `"github.com/tstapler/tymux/clients/go/authinterceptor"` and change
  their `newClient(baseURL string)` to `newClient(baseURL, token
  string)`, passing `connect.WithInterceptors(authinterceptor.Interceptor{Token:
  token})` — no local `authInterceptor` type in either file. Read the
  token from `os.Getenv("TYMUXD_TOKEN")` (empty string when unset, which
  `authinterceptor.Interceptor` already treats as no-op) so the examples
  remain usable against a token-gated daemon.
- Files: `clients/go/examples/list-sessions/main.go`,
  `clients/go/examples/attach/main.go`

### Epic 3.2: TypeScript client token support
**Goal**: `clients/ts` can authenticate against a token-gated `tymuxd`,
proven cross-language for both a unary RPC and `Attach` specifically.

#### Story 3.2.1: `authInterceptor` via `createGrpcTransport({ interceptors })`
**As a** `clients/ts` user, **I want** a token attached to every outgoing
call, **so that** the TS client can talk to a non-loopback, token-gated
`tymuxd`.
**Acceptance Criteria**:
- Missing/wrong token is rejected on a unary call.
  - *Given* a token-gated non-loopback `tymuxd` (token `s3cr3t`) and a TS
    transport constructed with no token, *When* `listSessions({})` is
    called, *Then* it throws a `ConnectError` with
    `.code === Code.Unauthenticated`.
- Correct token succeeds on a unary call.
  - *Given* the same daemon and a transport constructed with token
    `s3cr3t`, *When* `listSessions({})` is called, *Then* it succeeds.
- Missing/wrong token is rejected on the streaming `Attach` RPC
  specifically.
  - *Given* the same daemon and a transport with no token, *When*
    `runAttachDemo(...)` is invoked, *Then* it throws/rejects with
    `Code.Unauthenticated`.
- Correct token succeeds on `Attach`.
  - *Given* the same daemon and a transport with token `s3cr3t`, *When*
    attach is opened, *Then* it streams output normally.
**Files**: `clients/ts/examples/client.ts`, `clients/ts/examples/attach.ts`,
`clients/ts/test/daemon.ts`, `clients/ts/test/integration.test.ts`

##### Task 3.2.1a: `authInterceptor` + token param in `tymuxClient` (~5 min)
- In `clients/ts/examples/client.ts`:
  ```ts
  import type { Interceptor } from "@connectrpc/connect";

  // Applies uniformly to unary and streaming calls by construction — TS
  // has one Interceptor type, not Go's separate unary/streaming split.
  function authInterceptor(token?: string): Interceptor {
    return (next) => async (req) => {
      if (token) req.header.set("authorization", `Bearer ${token}`);
      return await next(req);
    };
  }

  export function tymuxClient(baseUrl = "http://127.0.0.1:7419", token?: string) {
    const transport = createGrpcTransport({ baseUrl, interceptors: [authInterceptor(token)] });
    return createClient(TymuxService, transport);
  }
  ```
- Files: `clients/ts/examples/client.ts`

##### Task 3.2.1b: Non-loopback + token variant of `startDaemon` (~5 min)
- Extend `startDaemon` in `clients/ts/test/daemon.ts` (or add a sibling)
  to optionally bind `0.0.0.0:<ephemeral>` and set `TYMUXD_TOKEN` in the
  spawned process's env, mirroring the Go/Rust harnesses; existing
  callers keep the current loopback/no-token default.
- Files: `clients/ts/test/daemon.ts`

##### Task 3.2.1c: Unary reject/accept tests (~5 min)
- Add `"listSessions rejects a missing/wrong token"` and `"listSessions
  succeeds with the correct token"` to
  `clients/ts/test/integration.test.ts`, using Task 3.2.1a/b.
- Files: `clients/ts/test/integration.test.ts`

##### Task 3.2.1d: `Attach` reject/accept tests (~5 min)
- Extend `AttachDemoOptions`/`runAttachDemo` in
  `clients/ts/examples/attach.ts` to accept an optional `token?: string`
  threaded into `tymuxClient(baseUrl, token)`; add `"attach rejects a
  missing/wrong token"` and `"attach succeeds with the correct token"` to
  `clients/ts/test/integration.test.ts`.
- Files: `clients/ts/examples/attach.ts`,
  `clients/ts/test/integration.test.ts`
- Note (triad review round 2): this task bundles a pure-code change
  (`attach.ts`'s `token?: string` param) with daemon-requiring integration
  tests, unlike Go's equivalent split (Task 3.1.1f: wiring; Task 3.1.1e:
  tests, independently bucketed). Not worth re-splitting this task purely
  for dependency-graph purity — the code change is a few lines threaded
  through one function signature, small enough that an implementer
  working through Task 3.2.1c immediately before it will already have
  `attach.ts` open; flagged here rather than silently left inconsistent
  with Go's task shape.
