# Stack Research: bearer-token-auth

## 1. Pinned versions

- `tonic = "0.12"` at workspace level (`Cargo.toml:16`), locked to **0.12.3** (`Cargo.lock:1611`). Both `crates/tymuxd/Cargo.toml:15` and `crates/tymux-cli/Cargo.toml:14` inherit it via `{ workspace = true }`.
- `clap = { version = "4", features = ["derive"] }` at workspace level, locked to **4.6.1** (`Cargo.lock:221`). Used today only by `tymux-cli` (`crates/tymux-cli/src/main.rs:178-192`, `#[derive(Parser)]`).
- **`tymuxd` does not depend on `clap` at all** (`crates/tymuxd/Cargo.toml` deps: `tymux-core`, `tymux-proto`, `tokio`, `tonic`, `futures`, `tokio-stream`, `uuid`, `anyhow`, `tracing`, `tracing-subscriber`, `libc`). Today it has zero CLI flags — `main()` reads config only via `std::env::var("TYMUXD_ADDR")` (`crates/tymuxd/src/main.rs:1227`) with a hardcoded `unwrap_or_else` default of `"127.0.0.1:7419"`, plus two other ad-hoc env reads (`TYMUXD_DISCONNECT_REGRESSION_WINDOW_MS`, `TYMUXD_GRACE_PERIOD_MS` at lines 93/98). Requirements ask for `--token` CLI flag on `tymuxd` — **this requires adding `clap` as a new dependency to `crates/tymuxd/Cargo.toml`** (it's already a workspace dependency, so no new crate enters the lockfile, just a new `Cargo.toml` line), or hand-rolling flag parsing to stay consistent with the existing no-clap style. Recommend clap: it also gives env-var-fallback for free (see §4).
- `connectrpc.com/connect v1.20.0` (`clients/go/go.mod:6`).
- `@connectrpc/connect@^2.0.2` and `@connectrpc/connect-node@^2.0.2` (`clients/ts/package.json:17-18`).

## 2. Where the server is built today

`crates/tymuxd/src/main.rs:1286-1291`:
```rust
Server::builder()
    .http2_keepalive_interval(Some(Duration::from_secs(30)))
    .http2_keepalive_timeout(Some(Duration::from_secs(10)))
    .add_service(TymuxServiceServer::new(daemon))
    .serve_with_shutdown(socket_addr, shutdown_signal())
    .await?;
```
Two more `Server::builder()...add_service(TymuxServiceServer::new(daemon))` call sites exist for tests (`main.rs:1406-1407`, `main.rs:3166-3167`), both binding to `127.0.0.1:0` — these will keep passing `TymuxServiceServer::new(daemon)` unwrapped (no interceptor) since they're loopback and requirements say loopback behavior must stay unaffected. Only the production bind at line ~1286 needs conditional wrapping.

The existing non-loopback check is a `tracing::warn!` at `main.rs:1238-1247`, immediately after the addr is parsed (before `sessions_dir`/persistence setup, well before the `Server::builder()` call) — this is the natural spot to instead **fail fast** (`return Err(...)`) when non-loopback and no token is configured, per requirements ("refuses to start … fails fast, not a warning").

## 3. tonic `Interceptor` — exact API (0.12.3)

```rust
// tonic::service::interceptor / re-exported at tonic::service::Interceptor
pub trait Interceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status>;
}
```
Any `FnMut(Request<()>) -> Result<Request<()>, Status>` closure satisfies it — no boilerplate `impl` needed for a stateless closure capturing the configured token.

Composition with the generated server: `tonic-build` generates, alongside `TymuxServiceServer::new(inner)`, a
```rust
TymuxServiceServer::with_interceptor<F>(inner: T, interceptor: F) -> TymuxServiceServer<InterceptedService<T, F>>
```
associated fn (confirmed via tonic-build source/docs: [`tonic-build/src/server.rs`](https://github.com/hyperium/tonic/blob/master/tonic-build/src/server.rs), [`InterceptedService` docs](https://docs.rs/tonic/0.12.3/tonic/service/interceptor/struct.InterceptedService.html)). So the production call site becomes conditional on the `add_service(...)` argument:
```rust
let svc = match token {
    Some(t) => TymuxServiceServer::with_interceptor(daemon, auth_interceptor(t)),
    None => TymuxServiceServer::new(daemon), // only reachable when loopback
};
Server::builder()
    ...
    .add_service(svc)
    .serve_with_shutdown(socket_addr, shutdown_signal())
    .await?;
```
Both arms return a type usable by `add_service` (tonic's generated server wraps whatever inner `Service` you give it and implements the same `NamedService`), so this compiles without an enum/boxing dance — verify during implementation, but this is `tonic`'s documented pattern for optional interceptors.

`InterceptedService::call` runs the closure once per RPC invocation, operating on the initial `Request<()>` (metadata/headers only, body/message stream not yet decoded) **before** the wrapped service's handler runs — i.e. before any streaming body is polled.

## 4. Feasibility risk: does the interceptor cover the bidi-streaming `Attach` RPC?

`proto/tymux/v1/tymux.proto:99`: `rpc Attach(stream AttachRequest) returns (stream AttachEvent);` — true client+server bidirectional streaming, the one RPC named as a specific risk in requirements.md.

Tonic's interceptor operates at the **HTTP/2 request level**, not the gRPC-message level: gRPC (unary or streaming) is always one HTTP/2 request/response with header frames established before either side's message stream flows. `InterceptedService` wraps the whole `tower::Service` (whose `call` takes the http Request and returns the http Response future), so the interceptor's `call` fires against the connection's initial headers for **every** RPC type — unary, server-streaming (`WatchWindow`), and bidi-streaming (`Attach`) alike — before the tonic codec starts decoding/emitting any stream frames. There is nothing method-specific about how `InterceptedService` dispatches; it doesn't special-case streaming methods. This resolves the named feasibility risk: **the interceptor uniformly gates `Attach` the same as unary calls**, rejecting with `Status::unauthenticated(...)` before the bidi stream is ever established (client never gets a stream to write into; sees `Unauthenticated` immediately). Recommend confirming with one integration test that opens `Attach` against a non-loopback daemon with a bad/missing token and asserts the stream fails immediately with `Unauthenticated` rather than opening and then erroring.

## 5. Constant-time comparison

- `subtle` (dalek-cryptography) is the community-standard crate for this, currently **2.6.1** on crates.io. Provides `ConstantTimeEq::ct_eq(&self, other) -> Choice`, used pervasively in Rust crypto code. It is a real dependency add (not currently in `Cargo.lock`), pure-Rust, no unsafe beyond a documented volatile-read barrier to defeat compiler optimization, and MSRV/`no_std`-friendly.
- Given requirements' explicit bias against new deps for a few lines, a **hand-rolled fallback** is also viable and simpler to audit:
  ```rust
  fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
      if a.len() != b.len() {
          return false;
      }
      let mut diff: u8 = 0;
      for (x, y) in a.iter().zip(b.iter()) {
          diff |= x ^ y;
      }
      diff == 0
  }
  ```
  This avoids early-exit branching on content (the length check is fine to short-circuit — token length isn't secret) and doesn't rely on the compiler avoiding a branch on `diff` itself since the final `== 0` is a single comparison, not a loop-level branch. `subtle`'s extra value is the volatile-read barrier that defends against LLVM recognizing the XOR-accumulate pattern and reintroducing a branch under aggressive optimization — a real but narrow risk. Recommendation: use `subtle::ConstantTimeEq` (one small, audited, purpose-built dependency) rather than hand-rolling given this is explicitly called out in requirements as "this *is* the security boundary" — the requirement's own stated NFR ("avoid a timing side-channel on a byte-by-byte compare") is exactly what a hand-rolled loop risks getting silently defeated by compiler optimization; `subtle` is the tool built to prevent that.

## 6. clap flag/env precedence (rabbit hole resolution)

No existing precedent in this repo for flag+env fallback on the *same* setting via clap — the closest existing pattern is `tymux-cli`'s `--addr` flag (`main.rs:181-182`, flag-only, no env fallback) and `tymuxd`'s raw `std::env::var("TYMUXD_ADDR")` (`main.rs:1227`, env-only, no flag). Neither today has both.

clap 4.6 supports this natively via `#[arg(long, env = "TYMUXD_TOKEN")]`: when both the CLI flag and the env var are present, **the explicit CLI flag wins** — clap's `env` fallback is only consulted when the flag is *not* passed on the command line. This is clap's documented, built-in precedence (flag > env > default) and needs no custom logic; it directly resolves the "Open Questions" precedence rabbit hole in requirements.md. Recommended field shape for both `tymuxd` (needs adding clap, see §1) and `tymux-cli`:
```rust
#[arg(long, env = "TYMUXD_TOKEN")]
token: Option<String>,
```
Do not mark it `hide_env_values` off by mistake — clap's `--help` output shows the env var *name* by default but not the current *value*, which is fine (the value itself must never be logged per the NFR, and `--help` doesn't print the resolved value).

**Correction (found during Phase 3 planning, adversarial review)**: the claim above is factually wrong. Verified directly against `clap_builder-4.6.6/src/output/help_template.rs:770` and the `hide_env_values` doc comment (`arg.rs:2658-2661`): `hide_env_values` defaults to `false`, and *without* explicitly setting it to `true`, `--help` prints `[env: TYMUXD_TOKEN=<the actual current value>]` — the live value, not just the name. This is a direct violation of the NFR this section correctly flags as important. `implementation/plan.md`'s Task 2.1.1b sets `#[arg(..., hide_env_values = true)]` explicitly to close this, and Task 2.1.1d adds a named test pinning the behavior. Left here uncorrected-in-substance (only this note added) as an accurate record of what this research pass concluded and how it was caught downstream — see `implementation/plan.md`'s Pattern Decisions table for the corresponding entry.

## 7. connect-go (v1.20.0) — attaching bearer token / surfacing Unauthenticated

Existing example pattern: `clients/go/examples/list-sessions/main.go` builds a plain `*http.Client` with an h2c-capable `http2.Transport` (loopback, no TLS: `AllowHTTP: true`, custom `DialTLSContext` that just dials plaintext) and constructs the client via `tymuxv1connect.NewTymuxServiceClient(httpClient, baseURL, connect.WithGRPC())` (`clients/go/examples/list-sessions/main.go:29-34`). Calls are made as `client.ListSessions(ctx, connect.NewRequest(&tymuxv1.ListSessionsRequest{}))`.

Two ways to attach the token, both idiomatic for connect-go:
- **Per-call**: `req := connect.NewRequest(...); req.Header().Set("Authorization", "Bearer "+token)` before calling.
- **Client-wide (recommended for this repo, since every example currently constructs a client once via `newClient`)**: a `connect.UnaryInterceptorFunc` (and for streaming, `connect.StreamingClientInterceptorFunc`) passed via `connect.WithInterceptors(...)` to `NewTymuxServiceClient`, setting the header on every outgoing request uniformly — avoids repeating the header-set at every call site and automatically covers the streaming `Attach` RPC too.

Surfacing `Unauthenticated` on the client: connect-go maps the gRPC status to a `*connect.Error` with `Code() == connect.CodeUnauthenticated`; callers check via `connect.CodeOf(err) == connect.CodeUnauthenticated` (or `errors.As` into `*connect.Error` and inspect `.Code()`). This is the mechanism `clients/go/integration/integration_test.go` should assert on for the wrong/missing-token integration test named in requirements' Success Metrics.

## 8. @connectrpc/connect / connect-node (^2.0.2) — same, for TS

Existing pattern: `clients/ts/examples/client.ts` builds a transport via `createGrpcTransport({ baseUrl })` (loopback, plain HTTP, no TLS config) and `createClient(TymuxService, transport)`.

Idiomatic token attachment: an `Interceptor` (the `@connectrpc/connect` type, a function `(next) => async (req) => {...}`) passed via `createGrpcTransport({ baseUrl, interceptors: [authInterceptor] })`:
```ts
const authInterceptor: Interceptor = (next) => async (req) => {
  req.header.set("Authorization", `Bearer ${token}`);
  return await next(req);
};
```
This applies uniformly to unary and streaming calls (interceptors wrap the whole transport call chain, not per-RPC-type), so it also covers the `Attach` bidi stream automatically — no separate streaming-specific interceptor type needed in the TS stack (unlike Go, which distinguishes `UnaryInterceptorFunc` from `StreamingClientInterceptorFunc`).

Surfacing `Unauthenticated` on the client: failed calls throw a `ConnectError` with `.code === Code.Unauthenticated` (`import { Code, ConnectError } from "@connectrpc/connect"`); callers catch and check `ConnectError.from(err).code === Code.Unauthenticated`. This is what `clients/ts/test/integration.test.ts` should assert for the wrong/missing-token case.

## Sources

- [Interceptor in tonic::service — docs.rs](https://docs.rs/tonic/0.12.3/tonic/service/trait.Interceptor.html)
- [InterceptedService in tonic::service::interceptor — docs.rs](https://docs.rs/tonic/0.12.3/tonic/service/interceptor/struct.InterceptedService.html)
- [tonic source, service/interceptor.rs](https://github.com/hyperium/tonic/blob/master/tonic/src/service/interceptor.rs)
- [subtle — crates.io](https://crates.io/crates/subtle)
- [subtle source — dalek-cryptography/subtle](https://github.com/dalek-cryptography/subtle/blob/main/src/lib.rs)
- [Connect Go interceptors docs](https://github.com/connectrpc/connectrpc.com/blob/main/docs/go/interceptors.md)
- [Connect Node interceptors docs](https://connectrpc.com/docs/node/interceptors/)
- [connectrpc.com/authn — pkg.go.dev](https://pkg.go.dev/connectrpc.com/authn)
