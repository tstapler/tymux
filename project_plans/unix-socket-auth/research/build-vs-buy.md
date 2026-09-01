# Research: Build vs. Buy — unix-socket-auth

Evaluates the four areas requested against
`project_plans/unix-socket-auth/requirements.md`: kernel-verified peer-cred
extraction (SO_PEERCRED/LOCAL_PEERCRED), tonic serving UDS + TCP
concurrently, and Go/TS client UDS dialing. §3b (Rust `tymux-cli` client
dialing) was added after the fact, closing a process gap
architecture-review.md/adversarial-review.md flagged: ADR-003 made this
decision without a build-vs-buy writeup, unlike its Go/TS siblings.

**Headline finding**: the single biggest feasibility risk called out in
requirements.md — "macOS `peer_cred()` support is the single biggest risk"
— turns out not to be a risk at all. tokio's own `UnixStream::peer_cred()`
(and therefore tonic's `UdsConnectInfo.peer_cred`, which wraps it) already
implements macOS via `getpeereid()` + `LOCAL_PEEREPID`, gated in on
`target_vendor = "apple"`. No new crate, no hand-rolled FFI, no macOS gap.
See §1.

## 1. Rust: peer-credential extraction (SO_PEERCRED / LOCAL_PEERCRED)

**Candidates considered:**

- **Buy — nothing needed**: `tokio::net::UnixStream::peer_cred() -> io::Result<UCred>`,
  already reachable through this repo's existing dependencies
  (`tokio = { version = "1", features = ["full"] }`, root `Cargo.toml:17`,
  locked at `1.52.3` in `Cargo.lock:1524`). Serving gRPC through this
  listener via tonic 0.12.3 (`Cargo.toml:18`, locked `Cargo.lock:1618`)
  surfaces the same data automatically as
  [`tonic::transport::server::UdsConnectInfo`](https://docs.rs/tonic/0.12.3/tonic/transport/server/struct.UdsConnectInfo.html)
  (confirmed present at the exact locked version, not just `latest`):
  `pub struct UdsConnectInfo { pub peer_addr: Option<Arc<SocketAddr>>, pub peer_cred: Option<UCred> }`,
  retrievable per-request via `request.extensions().get::<UdsConnectInfo>()`
  — the direct UDS analogue of the `TcpConnectInfo` extension the existing
  `BearerAuthInterceptor` (`crates/tymuxd/src/auth.rs:123`,
  `req.remote_addr()`) already reads for logging.
- **Platform coverage of tokio's `UCred`** (verified against tokio's own
  `tokio/src/net/unix/ucred.rs`, two independent fetches agreeing):
  - Linux/Android/OpenBSD/Haiku/Cygwin/Redox: `getsockopt(SO_PEERCRED)` — uid, gid, **and pid**.
  - NetBSD/QNX (`nto`): `LOCAL_PEEREID` + `unpcbid` — uid, gid, and pid.
  - FreeBSD: `LOCAL_PEERCRED` + `xucred` — uid, gid, and pid (FreeBSD ≥13).
  - DragonFly/AIX: `getpeereid()` — uid and gid only, pid is `None`.
  - **macOS/iOS/tvOS/watchOS/visionOS**: `getpeereid()` for uid/gid +
    `getsockopt(LOCAL_PEEREPID)` for pid — all three fields populated.
  - Solaris/Illumos: `getpeerucred()` + `ucred_get*()` accessors.
  - ESP-IDF/Fuchsia/Hurd/NuttX/Vita: **stub** — returns zeroed credentials
    with no real syscall. Not relevant to this project's Linux-primary/
    macOS-secondary scope, but worth a one-line code comment if this ever
    gets ported, since a stub returning `uid: 0` looks like a successful
    call rather than an error.
  - This scope (Linux primary, macOS best-effort per requirements.md's
    Constraints) is fully covered by code already in the dependency tree.
- **Crates considered and rejected**: `nix::sys::socket::UnixCredentials`/
  `PeerCredentials` (crate already a transitive dependency at `0.25.1`,
  `Cargo.lock:827` — but Linux-only, no macOS path, and would duplicate
  what tokio already exposes), `uds` (crates.io, `ConnCredentials`/
  `initial_peer_credentials()` — a general Unix-socket toolkit, adds a
  second, redundant credential-extraction path alongside tonic's own),
  `unix-cred` (crates.io — thin single-purpose wrapper, same redundancy
  problem, and far lower adoption than tokio itself as a proxy for
  scrutiny).

**Pros (build — actually "already bought" via tokio)**
- Zero new dependency of any kind — `UdsConnectInfo`/`UCred` ship with
  tonic 0.12.3 + tokio 1.52.3, both already pinned in `Cargo.lock` at the
  exact versions the daemon already builds against.
- The kernel-sourced guarantee the requirements demand ("peer identity
  must come from the kernel, never client-supplied") is met by
  construction: `UCred` is populated from `getsockopt`/`getpeereid` at
  accept time by tokio's own internals, not from anything the client sends
  on the wire — architecturally identical to how `TcpConnectInfo`'s
  `remote_addr` is already trusted in `auth.rs`.
- Closes the requirements doc's own biggest flagged risk: macOS support is
  not "unconfirmed," it's implemented and has been for multiple tokio
  releases.

**Cons**
- None identified specific to the extraction step itself. The uid-matching
  and rejection-logging logic built on top of `UCred` is genuinely
  project-specific glue (comparable in shape to `BearerAuthInterceptor`)
  and isn't available pre-built anywhere — but that's normal application
  code, not a gap a library would fill.

**Verdict: Recommended — build directly on tokio's `UnixStream::peer_cred()`
via tonic's `UdsConnectInfo`. No crate to add.** This is a stronger "build"
case than bearer-token-auth's constant-time-compare precedent
(`project_plans/bearer-token-auth/decisions/ADR-001-constant-time-eq-crate.md`)
even set: there the repo overrode its anti-dependency bias because the
security-critical primitive (constant-time byte compare) was genuinely
*not* already in the tree and easy to get subtly wrong by hand. Here the
security-critical primitive (kernel-verified peer credentials) is *already*
in the tree, already audited by tokio's much larger user base than any
peer-cred-specific crate could claim, and requires zero hand-rolled FFI —
so there's neither a hand-rolled-code risk to buy away nor a dependency gap
to fill. The only "build" left is ~20-30 lines of uid-comparison/rejection
logic in a new interceptor, mirroring `BearerAuthInterceptor`'s shape.

## 2. Rust/tonic: serving gRPC over Unix + TCP concurrently

**Candidates considered:**

- **Build — two `Server::builder()...serve_with_incoming()` tasks**: tonic
  has no single API for binding two transports at once, but the pattern
  the ecosystem uses (including tonic's own
  [`examples/src/uds/server.rs`](https://github.com/hyperium/tonic/blob/master/examples/src/uds/server.rs))
  is to spawn one `Server::builder()` per listener, each wrapping the
  *same* cloneable service, and run them concurrently (`tokio::join!` or
  two `tokio::spawn`s). This repo's `crates/tymuxd/src/main.rs` already
  has the TCP half of this exact shape (`Server::builder()...
  serve_with_shutdown(socket_addr, ...)`, `main.rs:1314-1328`); adding UDS
  is a second, structurally identical call using
  `serve_with_incoming(UnixListenerStream::new(listener))` (`tokio-stream`
  is already a workspace dependency with the `net` feature enabled,
  `Cargo.toml:26`, which is what provides `UnixListenerStream`).
- **`tonic::transport::server::Connected` for `UnixStream`**: was an open
  question as recently as
  [tonic issue #365](https://github.com/hyperium/tonic/issues/365) and
  [#856](https://github.com/hyperium/tonic/issues/856) ("any interest in
  implementing `Connected` for `UnixStream`?") — both are resolved in the
  affirmative in the version this repo already depends on: `UdsConnectInfo`
  ships in tonic 0.12.3. No action needed beyond using the listener.
- **`tonic-middleware` crate**: exists on crates.io as generic
  interceptor/layer scaffolding (interceptors that can be `async`, unlike
  tonic's own synchronous `Interceptor` trait) — orthogonal to the
  dual-listener question, not a serving-over-UDS helper. Not relevant to
  this sub-problem (and, per the bearer-token-auth precedent in §1 of
  `project_plans/bearer-token-auth/research/build-vs-buy.md`, likely not
  needed for the auth interceptor either, since `tonic::service::Interceptor`
  is synchronous and a uid-comparison check has no need to `.await`).

**Pros (build)**
- Directly matches an existing, maintained example in tonic's own repo —
  this isn't uncharted territory the project would be first to hit.
- No new dependency; `tokio-stream`'s `net` feature (already enabled) is
  the only piece needed beyond what's already in `Cargo.toml`.
- The two listeners can share one `TymuxServiceServer<Engine>` instance
  (it's already constructed once and would just be `.clone()`d into each
  `Server::builder()` call), so there's no duplicated business logic
  between the TCP and UDS paths — only the interceptor differs (bearer
  token on TCP per the existing non-loopback gate, peer-uid on UDS).

**Cons**
- Two independent `Server` futures must both be joined into the daemon's
  shutdown handling (`shutdown_signal()`, already used per-listener in
  `main.rs:1321,1328`) — a small amount of "hold two futures instead of
  one" wiring, not a missing capability.

**Verdict: Recommended — build**, following tonic's own documented
dual-listener pattern. There is no tonic-native "serve on N transports"
API to buy, and none is needed — this repo already has one working
`Server::builder()...serve_with_shutdown` call to use as the literal
template for the second.

## 3. Go: HTTP-over-UDS transport for connect-go

**Candidates considered:**

- **Buy — nothing UDS-specific exists**: neither `connectrpc.com/connect`
  nor the wider Go ecosystem ships a maintained "UDS transport" helper.
  `connectrpc.com/authn` (referenced in bearer-token-auth's build-vs-buy.md
  §4 as the closest first-party auth pattern) is unrelated to transport
  dialing.
- **Build — `http.Transport.DialContext` override**: this is not a gap,
  it's simply how Go's standard library already supports UDS as a
  transport-layer concern, orthogonal to the RPC framework on top. The
  generated connect-go client constructor
  (`greetv1connect.NewGreetServiceClient(httpClient, baseURL, opts...)`,
  confirmed against [connectrpc.com's Go getting-started
  guide](https://connectrpc.com/docs/go/getting-started/)) takes a plain
  `*http.Client` (or anything satisfying connect-go's minimal
  `HTTPClient` interface) as its first argument — so a client constructed
  with
  `&http.Client{Transport: &http.Transport{DialContext: func(ctx, _, _ string) (net.Conn, error) { return (&net.Dialer{}).DialContext(ctx, "unix", socketPath) }}}`
  works with zero changes to the generated stubs. The base URL passed to
  the constructor becomes a placeholder host (e.g. `http://unix`) since
  the dialer ignores the network/addr arguments it's given and always
  targets the socket path.
- This repo already has the equivalent pattern in place for a different
  cross-cutting concern: `clients/go/authinterceptor/authinterceptor.go`
  wraps a connect-go client-construction step in a small project-local
  helper (an `Interceptor`, in that case) rather than repeating
  boilerplate at each call site — the natural template for a parallel
  `clients/go/udsdialer` (or similarly named) package providing a
  `DialUnixHTTPClient(socketPath string) *http.Client` constructor.

**Pros (build)**
- `net.Dialer.DialContext` with network `"unix"` is a two-line, entirely
  standard-library operation — Go's docs and multiple independent
  reference examples (e.g. the widely-cited
  [HTTP-over-UDS gist](https://gist.github.com/teknoraver/5ffacb8757330715bcbcc90e6d46ac74))
  confirm this is the idiomatic approach, not a workaround.
- No dependency to add; `net`/`net/http` are stdlib.
- Matches the connect-go interceptor precedent already in this repo
  (`authinterceptor.go`): the "buy" is the SDK's extension point
  (accepting a custom `*http.Client`), the "build" is one small
  project-local constructor function, not per-call-site glue.

**Cons**
- HTTP/2 over UDS via a custom `DialContext` needs `Transport.ForceAttemptHTTP2 = true`
  (or `http2.ConfigureTransport`) to be set explicitly — DialContext alone
  doesn't opt a `*http.Transport` into HTTP/2 the way the zero-value
  `http.DefaultTransport` does over TCP+TLS. This is a one-line
  requirement to carry into implementation, not a design risk.

**Verdict: Recommended — build**, a small `clients/go` constructor
function around `http.Transport.DialContext`, following the same
"SDK already provides the extension point, this repo adds one shared
wrapper" pattern established for bearer-token-auth's Go client work.

## 3b. Rust `tymux-cli` UDS dialing

*Added post-ADR-003, closing a process gap architecture-review.md and
adversarial-review.md both independently flagged: ADR-003 (the decision
to add `tower`/`hyper-util` as direct `tymux-cli` dependencies) was
written without a build-vs-buy comparison, unlike sections 3 and 4 above
for the Go/TS client-dialing equivalents.*

**Candidates considered:**

- **Buy — nothing UDS-specific exists for a tonic client either**: tonic
  has no built-in "just pass a socket path" client constructor; dialing
  over UDS requires its documented but lower-level
  `Endpoint::connect_with_connector`/`connect_with_connector_lazy` API
  (verified against the pinned `tonic-0.12.3` source,
  `transport/channel/endpoint.rs:364-404`), which takes a `C:
  tower::Service<Uri, ...>` connector.
- **Build — `tower::service_fn` + `hyper_util::rt::TokioIo`**: tonic's own
  documented pattern for this exact case (its doc comment on
  `connect_with_connector_lazy` points at "the `uds` example"). Checked
  directly against `Cargo.lock`: `tower 0.4.13` and `hyper-util 0.1.20`
  are **already transitive dependencies** of this workspace, pulled in by
  `tonic`/`hyper` themselves — neither crate, nor any new code, enters the
  build graph that isn't already compiled today. ADR-003's decision is to
  promote both to *direct* `tymux-cli` dependencies (so `service_fn`/
  `TokioIo` are directly importable) with `default-features = false` and
  only the specific feature needed (`util` / `tokio`), keeping the
  footprint minimal.
- **Hand-roll a minimal `tower::Service` impl instead of `service_fn`**:
  rejected in ADR-003 — `service_fn` is `tower`'s own zero-risk mechanism
  for turning an `async fn(Uri) -> io::Result<S>` closure into a
  `Service`; hand-rolling one duplicates ~15 lines of already-vetted code
  for no benefit, and `tower` is already in the dependency tree
  regardless of whether `tymux-cli` imports it directly.

**Pros (build, on already-vetted crates)**
- Zero *new* code enters the compiled dependency graph — both crates are
  already resolved and compiled as transitive dependencies via `tonic`;
  this promotes them to direct-dependency status, it doesn't add
  anything `cargo build` wasn't already building.
- Matches tonic's own documented client-construction pattern for UDS —
  the same category of "official example as template" reasoning §2 above
  already used for the server side.
- Symmetric with Go's/TS's own client-dialing decisions (§3/§4): all
  three languages solve "dial a Unix socket instead of TCP" via their
  respective RPC framework's own documented low-level extension point,
  not a third-party helper package.

**Cons**
- None specific to this step — the same "genuinely new application glue,
  not a gap a library would fill" caveat §1 named for the server-side
  uid-comparison logic applies here too (the `dial_uds`/`dial_channel`
  functions themselves are project-specific, ordinary code either way).

**Verdict: Recommended, low risk.** ADR-003 promotes already-vetted,
already-compiled crates (`tower 0.4.13`, `hyper-util 0.1.20`, both
resolved in `Cargo.lock` today via `tonic`) to direct-dependency status,
rather than adding new code to the build graph — the same "already
bought via a pre-existing dependency" shape §1 found for the server-side
peer-cred extraction, not a genuinely new build-vs-buy tradeoff.

## 4. Node/TS: UDS transport for @connectrpc/connect-node

**Candidates considered:**

- **Buy — no first-party support, and it's an open gap, not just
  undocumented**: [connectrpc/connect-es issue
  #756](https://github.com/connectrpc/connect-es/issues/756) is an open,
  unresolved feature request asking for exactly this ("connecting to gRPC
  servers via Unix domain sockets... exists in other gRPC libraries but
  isn't available through `createGrpcTransport()`"). No maintainer
  response, no linked PR, labeled "enhancement." The reporter's own
  workaround was switching to a different gRPC client library entirely —
  not viable here, since the requirements call for the project's existing
  connect-node-based client.
- **Build — `createConnection` override on the underlying `http2.connect()` call**:
  `@connectrpc/connect-node`'s `createGrpcTransport()` accepts a
  `nodeOptions` bag that is forwarded into Node's own `http2.connect(authority, options)`
  call (confirmed pattern: `nodeOptions: { rejectUnauthorized: false }` is
  a documented real-world usage in the wild, e.g. `aserto-dev/node-directory`).
  Node's `http2.connect` itself accepts an `options.createConnection`
  callback that "returns any Duplex stream... to be used as the
  connection for this session" — this is precisely the mechanism the
  `grpc-js` project used to add UDS support
  ([grpc/grpc-node#1244](https://github.com/grpc/grpc-node/pull/1244)):
  "the http2 library isn't able to create a connection over a Unix domain
  socket... you can open a connection using the `net` library and supply
  that connection to `http2.connect`." The same technique — pass
  `nodeOptions: { createConnection: () => net.connect(socketPath) }` (with
  a placeholder `baseUrl` authority, since `createConnection` ignores it)
  — is directly applicable here. **This has not been independently
  verified end-to-end against `@connectrpc/connect-node`'s actual
  `nodeOptions` pass-through code** (search tooling could not retrieve the
  library's source directly); confirming it is a one-file spike, not a
  design decision, and should be the first thing attempted in
  implementation before committing to any fallback.
- **Fallback if `createConnection` pass-through doesn't work**: hand-roll a
  transport using Node's `net` module directly against the Connect
  protocol's plain-HTTP/1.1 mode (`createConnectTransport` rather than
  `createGrpcTransport`) via a custom `Agent`, similar in shape to Go's
  `DialContext` override — more code, but still no new npm dependency.

**Pros (build)**
- No dependency gap that a library purchase would close — the upstream
  connect-es project itself doesn't have this, and the workaround is a
  standard Node.js mechanism (`createConnection`) used by comparable
  projects (`grpc-js`) to solve the identical problem.
- If the `nodeOptions.createConnection` pass-through works as expected,
  this is a small, localized change (one client-construction helper in
  `clients/ts`, matching the Go-side symmetry).

**Cons / risk**
- This is the one sub-problem in this research where "will the chosen
  approach actually work" is not yet confirmed by direct source
  inspection — matches requirements.md's own Feasibility Risks callout
  ("Go/Node UDS dialing both require custom transport/dialer wiring").
  Flag for `research/pitfalls.md` and the implementation plan: spike this
  early (day 1 of the TS client work), because if `nodeOptions` doesn't
  actually forward to `http2.connect`'s `createConnection`, the fallback
  (`createConnectTransport` + custom `Agent` over HTTP/1.1) is a larger
  rework, not a one-line adjustment.

**Verdict: Viable, pending a same-day implementation spike to confirm the
`nodeOptions.createConnection` pass-through.** No crate/package to buy
either way — this is a build decision either path, differing only in how
much project-local plumbing the winning approach needs. Recommend
attempting the `createGrpcTransport({ nodeOptions: { createConnection } })`
path first (least code, matches the existing gRPC transport already used
elsewhere in `clients/ts`) before falling back to a hand-rolled
Connect-protocol-over-HTTP/1.1 transport.

## 5. SaaS / managed API

**Not applicable, confirmed.** This feature is a local-machine security
boundary — a single-host daemon (`tymuxd`) authenticating same-machine
processes via a Unix domain socket and kernel-supplied peer credentials.
There is no multi-tenant, network-facing, or hosted component to delegate
to a SaaS/managed API; peer-credential verification is fundamentally an
OS-kernel-local operation (the entire reason `SO_PEERCRED`/`LOCAL_PEERCRED`
exist is that they can't be spoofed off-box) and has no meaningful
"managed" analogue, the same way bearer-token-auth's build-vs-buy.md never
needed to consider one for its comparable local-daemon auth gate.

## 6. Fork or adapt: reference implementations of "local socket + peer-cred"

**Examples reviewed for pattern, not for forking:**

- **Docker Engine / `dockerd`**: the canonical example of exactly this
  shape (UDS default, TCP optional and historically warned against without
  TLS) — `/var/run/docker.sock` created group-owned (`docker` group) with
  mode `0660`, and the daemon's own docs recommend against un-authenticated
  TCP for the same "any local process" reasoning this project's Problem
  Statement gives. Docker's socket-group model (grant access via group
  membership on the socket file, not per-connection credential comparison)
  is the direct precedent for this project's "configurable group grants
  access... via the socket's group bit" requirement — validates that
  design choice as an established pattern rather than a novel one, but
  Docker doesn't itself do SO_PEERCRED-based uid verification (it trusts
  the mode/group bits alone), so it's a precedent for the *access-control*
  half, not the *kernel-verified-uid* half.
- **containerd**: same UDS-default pattern (`/run/containerd/containerd.sock`);
  its own client/daemon are Go, not Rust, so there's no Rust binding
  structure to adapt directly — relevant as another "local daemon, UDS
  socket, group-bit access control" precedent, not as adaptable code.
- **systemd-notify / sd-bus style peer-cred checks** (referenced
  conceptually, not a specific crate here): systemd services commonly
  verify `SO_PEERCRED` for local IPC trust decisions — same pattern this
  project is building, confirming SO_PEERCRED-based per-connection uid
  checks (as opposed to Docker's coarser socket-permission-only model) is
  also an established, non-exotic technique for daemons that need
  per-caller identity rather than just "on the socket or not."
- **tonic's own `examples/src/uds/server.rs`**: already the most directly
  applicable reference (see §2) — it's tonic's official pattern for
  serving gRPC over a Unix socket, in the same framework this project
  already uses, and is the concrete template to build from rather than an
  external project to adapt.

**Verdict: Not recommended to fork anything.** No existing open-source
Rust daemon combines "tonic + UDS + SO_PEERCRED-based per-connection uid
check + configurable group bit" in one adaptable package — the pieces are
each independently well-precedented (Docker/containerd for the group-bit
access model, systemd-family daemons for the per-connection peer-cred
check, tonic's own example for the serving mechanics) but no single
reference implementation combines them in a form worth forking over
composing from tonic's example plus this project's own uid-comparison
logic.

## Summary table

| Area | Decision | Verdict |
|---|---|---|
| 1. Rust peer-cred extraction | Build directly on `tokio::UnixStream::peer_cred()` / tonic's `UdsConnectInfo` — no crate needed, already covers macOS | Recommended |
| 2. tonic serving UDS + TCP | Build — two `Server::builder()...serve_with_incoming/shutdown` tasks sharing one service, per tonic's own example | Recommended |
| 3. Go UDS dialing | Build — `http.Transport.DialContext` + `"unix"` network, wrapped in one `clients/go` constructor | Recommended |
| 3b. Rust `tymux-cli` UDS dialing | Build — `tower::service_fn` + `hyper_util::rt::TokioIo` via tonic's `connect_with_connector`; promotes already-transitive `tower`/`hyper-util` to direct dependencies (ADR-003) | Recommended, low risk |
| 4. Node/TS UDS dialing | Build — `createGrpcTransport({ nodeOptions: { createConnection } })`, pending a same-day spike to confirm pass-through; fallback is a hand-rolled Connect-over-HTTP/1.1 transport | Viable (spike first) |
| 5. SaaS/managed API | N/A — local-machine kernel-verified security boundary, no hosted analogue | N/A |
| 6. Fork/adapt reference implementation | No single project combines all pieces; compose from tonic's own UDS example + Docker/systemd-style access-control precedent | Not recommended (compose, don't fork) |
