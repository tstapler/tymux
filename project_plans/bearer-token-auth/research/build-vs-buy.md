# Research: Build vs. Buy — bearer-token-auth

Evaluates four decision points against the requirements in
`project_plans/bearer-token-auth/requirements.md`: one shared operator-supplied
token, constant-time compare, no logging, applies uniformly to unary and the
streaming `Attach` RPC, non-loopback-only.

## 1. The interceptor/gate itself

**Options considered:**
- Build: `tonic::service::interceptor::InterceptedService` /
  `Server::builder().add_service(Service::with_interceptor(...))` — tonic's
  own primitive, already a workspace dependency (`tonic = "0.12"`, root
  `Cargo.toml:18`).
- Buy: `tower_http::auth` — `RequireAuthorizationLayer::bearer(...)` is
  **deprecated since tower-http 0.6.7** ("too basic to be useful in real
  applications"); its replacement, `ValidateRequestHeaderLayer::bearer(...)`,
  is a plain-`Vec<u8>`/string-equality check, not constant-time — it would
  not satisfy this project's NFR on its own and would need wrapping either
  way. tower-http is not currently a workspace dependency.
- Buy: crates named `tonic-auth` do not exist on crates.io. Closest matches:
  `middle` (OAuth2/Bearer client-side authorization middleware — wrong
  direction, it's for calling *other* APIs, not gating this server),
  `tonic-middleware` / `tonic-async-interceptor` (generic
  request-interception scaffolding, not auth-specific — would still require
  writing the actual token check).

**Pros (build via tonic's own `Interceptor`)**
- Zero new dependency; tonic is already in the tree.
- `Interceptor` runs on `tonic::Request<()>` metadata before the body is
  deserialized, and composes uniformly across unary and streaming RPCs
  (including bidi `Attach`) via `Service::with_interceptor(...)` — this
  still needs the empirical check called out in the requirements doc's
  Rabbit Holes/Feasibility Risks (confirm on the actual `Attach` stream
  before committing), but the primitive itself is not scoped per-RPC-kind.
- The logic is inherently ~10-20 lines: pull the `authorization` metadata
  entry, strip `Bearer `, constant-time compare, return `Ok`/`Status::unauthenticated`.

**Cons**
- None of the "buy" options actually reduce the amount of code written —
  every candidate still requires hand-writing the token-extraction and
  comparison logic; a generic interceptor-scaffolding crate adds an
  abstraction layer without covering the auth-specific part.

**Verdict: Recommended — build**, using tonic's built-in `Interceptor`
directly. This is a one-shared-secret, no-per-user-identity gate; a
dedicated auth library is overkill for the problem's actual shape, and
`tower-http`'s bearer helper is both deprecated and non-constant-time,
so adopting it buys nothing over calling tonic's own primitive.

## 2. Constant-time comparison

**Candidates:**

| Crate | Current version (as found) | Downloads/mo | Notes |
|---|---|---|---|
| [`subtle`](https://crates.io/crates/subtle) | actively maintained, part of `dalek-cryptography` | widely used transitively (RustCrypto/dalek ecosystem) | General `ConstantTimeEq`/`Choice` trait framework; uses `core::hint::black_box` as an optimization barrier on Rust ≥1.66 to resist the compiler recognizing and undoing the constant-time pattern. Broader API surface than needed here (`Choice`, `ConditionallySelectable`, etc.) — this project needs one function. |
| [`constant_time_eq`](https://crates.io/crates/constant_time_eq) | 0.5.0 (per crates.io search) | ~18.3M/month, used in 13,839 crates | Single-purpose: compares two equal-length byte strings in constant time, modeled on the Linux kernel's `crypto_memneq`. MIT/Apache-2.0/CC0. |

**Pros (buy)**
- Both crates are exactly the "battle-tested vs. bespoke crypto-adjacent
  code" case this repo's own operating instructions flag as dangerous to
  hand-roll: a loop that *looks* constant-time (`for i in 0..len { if a[i]
  != b[i] { return false } }`) is not — it early-exits, leaking length-
  dependent timing. Getting it right requires accumulating a bitwise
  difference across the *entire* buffer with no branch and an optimization
  barrier, so the compiler doesn't reintroduce short-circuiting at a higher
  opt level. That's exactly what `constant_time_eq`/`subtle` already do and
  have been reviewed for.
- `constant_time_eq` is single-purpose, tiny (no transitive deps of its own
  beyond core), extremely widely used (proxy for "many eyes have exercised
  this"), and does precisely the one operation this project needs — compare
  the presented token against the configured one.
- Cost is one line in `Cargo.toml` and one function call at the call site;
  this is not a "dependency for a few lines of business logic" case, it's a
  "dependency for a few lines of code that are easy to get subtly wrong and
  hard to test for wrongness" case (a timing side channel doesn't show up
  in normal unit tests — it needs a timing-analysis harness to even detect).

**Cons**
- One more line in the dependency tree (mitigated: `constant_time_eq` has
  no further transitive dependencies).
- Byte-length must be handled explicitly either way — both crates require
  (or strongly recommend) comparing equal-length buffers; a length
  mismatch itself is not a meaningful side-channel here (token length is
  configuration, not secret, since the operator sets it), so a manual
  length pre-check before calling the constant-time compare is standard
  and fine.

**Verdict: Recommended — buy, `constant_time_eq`.** This is the one place
in the feature where the repo's usual anti-dependency bias should be
overridden: constant-time comparison is a narrow, well-defined primitive
where "looks correct" and "is correct" diverge in ways ordinary code review
and tests won't catch, and a minimal, extremely widely-used crate exists
for exactly this. Prefer `constant_time_eq` over `subtle` — `subtle`'s
broader `Choice`/`ConditionallySelectable` API is built for constructing
larger constant-time algorithms (e.g. curve arithmetic); this project needs
one function, and `constant_time_eq` is that function with less surface
area to misuse.

## 3. Token generation

Out of scope per requirements (operator-supplied, no auto-generation, no
token file — see requirements.md's "Alternatives Considered"). The natural
"buy" answer is a one-line doc recommendation, not a build decision: point
operators at `openssl rand -hex 32` (or `pwgen -s 32 1`) in the `--token`
flag's `--help` text and/or README, the same way Jupyter/other
shared-secret daemons document token generation without building a
generator into the binary itself. No further evaluation needed.

## 4. Cross-language auth-header attachment (clients/go, clients/ts)

**connect-go:**
- First-class support exists: Connect's interceptor model
  (`connect.UnaryInterceptorFunc` / the `Interceptor` interface at
  [connectrpc.com/docs/go/interceptors](https://connectrpc.com/docs/go/interceptors/))
  wraps every outgoing call, unary and streaming, and can set the
  `Authorization` header on the underlying request before it's sent.
  `connectauth` (github.com/akshayjshah/connectauth, now folded into
  `connectrpc.com/authn`) is a ready-made example of exactly this pattern
  for auth specifically, though it's primarily server-side; the
  client-side shape is the same interceptor mechanism.
- Attaching an interceptor is done once at client-construction time
  (`connect.WithInterceptors(...)`), not per call site.

**@connectrpc/connect (TypeScript):**
- Same story: [connectrpc.com/docs/node/interceptors](https://connectrpc.com/docs/node/interceptors/)
  documents an `Interceptor` function type composed into the transport at
  construction time, used for exactly this class of cross-cutting concern
  (the docs call out auth headers as a canonical interceptor use case).

**Pros (buy the built-in interceptor mechanism, build a thin project-local wrapper)**
- Both client libraries already ship this as a first-class extension point
  — nothing to add to `go.mod`/`package.json`, it's part of the SDKs
  already in use.
- Because the attachment point is the transport/client constructor, not
  each call, a **one-time, per-client "authenticated transport"
  constructor** (e.g. `newAuthenticatedClient(addr, token string)` in Go,
  an equivalent factory in TS) written once in `clients/go` and once in
  `clients/ts` avoids repeating header-setting logic across every example
  and integration test. This is a small amount of project-local glue code
  (a constructor function, not a library), not a "buy" decision — the
  underlying mechanism is already bought (the interceptor API); what's
  "built" is the one-line-per-language convenience wrapper around it.

**Cons**
- Without the shared constructor, header-setting would otherwise be
  repeated per example/test call site — noted as the reason to build the
  thin wrapper rather than skip it.

**Verdict: Recommended.** Buy is a non-issue here — both connect-go and
@connectrpc/connect have built-in, first-class client interceptor support
for attaching headers to every call; there is no library gap to fill. Build
one small, reusable "authenticated transport/client" constructor per
language (not per-call-site header setting) to avoid duplicating the
token-attachment logic across `clients/go` and `clients/ts` examples and
integration tests.

## Summary table

| Area | Decision | Verdict |
|---|---|---|
| 1. Interceptor/gate | Build on tonic's `Interceptor` primitive | Recommended |
| 2. Constant-time compare | Buy — `constant_time_eq` crate | Recommended |
| 3. Token generation | Doc-only recommendation (`openssl rand -hex 32`); no build | N/A (not a build/buy decision) |
| 4. Client header attachment | Buy the built-in interceptor mechanism (already present in both SDKs); build one thin per-language "authenticated client" constructor | Recommended |
