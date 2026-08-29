# Architecture research: bearer-token-auth

Agent 3 (Architecture). Builds on `project_plans/attach-resume-protocol/research/architecture.md`'s mapping of `attach()`'s `forward_handle`/`input_handle` shape — not re-derived here.

## 1. Integration point: `TymuxServiceServer::with_interceptor(...)` on the server-registration site

Current unauthenticated registration, `crates/tymuxd/src/main.rs:1286-1291`:

```rust
Server::builder()
    .http2_keepalive_interval(Some(Duration::from_secs(30)))
    .http2_keepalive_timeout(Some(Duration::from_secs(10)))
    .add_service(TymuxServiceServer::new(daemon))
    .serve_with_shutdown(socket_addr, shutdown_signal())
    .await?;
```

`impl TymuxService for TymuxDaemon` is at `crates/tymuxd/src/main.rs:521`. `TymuxService`/`TymuxServiceServer` come from generated code (`use tymux_proto::v1::tymux_service_server::{TymuxService, TymuxServiceServer};`, `main.rs:17`).

**Recommendation: `TymuxServiceServer::with_interceptor(daemon, interceptor_fn)`, not a manual per-handler check or a generic tower layer.**

- tonic-build's generated server codegen (`~/.cargo/registry/.../tonic-build-0.12.3/src/server.rs:137-141`) emits `with_interceptor(inner: T, interceptor: F) -> InterceptedService<Self, F>` on every service server type, so `TymuxServiceServer::with_interceptor` already exists — no new codegen or trait plumbing needed.
- `InterceptedService<S, F>` implements `tower_service::Service` and `NamedService` (`tonic-0.12.3/src/service/interceptor.rs:122,168`), so it drops directly into `.add_service(...)` in place of the current `TymuxServiceServer::new(daemon)` — no other call-site changes.
- Manual per-handler checks are rejected: they'd require touching all ~10 `impl TymuxService for TymuxDaemon` methods individually (`create_session`, `attach`, `capture_pane`, `kill_session`, ...) and would be easy to miss on a future handler. A single interceptor is the enforcement point that can't be forgotten per-RPC.
- A raw tower `Layer` (via `tower::ServiceBuilder` / `Server::builder().layer(...)`) is more general but strictly more machinery than needed here — tonic's own docs (`tonic-0.12.3/src/service/interceptor.rs:35-41`) explicitly frame `Interceptor` as the right tool for exactly this "add/remove/check request metadata, or cancel with a `Status`" case, reserving tower layers for cases needing to act on the *response* too (not needed for a token gate).

**Fits existing codebase precedent**: this repo already branches server construction on loopback-vs-not — the `if !socket_addr.ip().is_loopback() { tracing::warn!(...) }` block at `main.rs:1238-1247` is exactly the existing "gate behavior at server-construction time based on bind address" precedent to extend. The natural shape:

```rust
let router = if socket_addr.ip().is_loopback() {
    server_builder.add_service(TymuxServiceServer::new(daemon))
} else {
    let token = /* required, checked below */;
    server_builder.add_service(TymuxServiceServer::with_interceptor(daemon, auth_interceptor(token)))
};
router.serve_with_shutdown(socket_addr, shutdown_signal()).await?;
```

This compiles despite the two arms building different concrete service types (`TymuxServiceServer<TymuxDaemon>` vs `InterceptedService<TymuxServiceServer<TymuxDaemon>, F>`) because `Router::add_service` **type-erases into `BoxCloneService`** — confirmed at `tonic-0.12.3/src/transport/server/mod.rs:71` (`type BoxService = tower::util::BoxCloneService<...>`) and the `Routes` internals boxing every registered service. Both branches produce the same `Router<L>` type.

**Test-harness note**: the three existing `Server::builder().add_service(TymuxServiceServer::new(daemon))` call sites used by tests (`main.rs:1406-1408` in `spawn_test_server`, and the inlined equivalent at `main.rs:3166-3168`) all bind to `127.0.0.1:0` (loopback) and are unaffected by this change — they fall into the loopback (no-interceptor) branch, matching the requirement that "loopback bind is unaffected." No test-harness changes needed for the loopback path itself; new tests for the non-loopback+token path will need their own harness variant.

## 2. Data flow: token is a plain owned value captured once in the interceptor closure — no `Arc`, no `TymuxDaemon`/`Engine` field

The token is consulted only by the interceptor, never by an RPC handler body (`impl TymuxService for TymuxDaemon` methods don't need to know about auth — it's a pure request gate that runs before the handler is ever invoked). That means it does **not** belong as a field on `TymuxDaemon` (`main.rs:53-89`) or `Engine` — adding it there would leak an auth concern into business-logic state that 10+ RPC handlers and all their tests already construct.

Recommended: a plain `String` (the token) moved into the `FnMut(Request<()>) -> Result<Request<()>, Status>` closure at startup, once, in `main()`:

```rust
fn auth_interceptor(token: String) -> impl Interceptor + Clone {
    move |req: tonic::Request<()>| -> Result<tonic::Request<()>, Status> {
        match req.metadata().get("authorization") {
            Some(v) if bearer_matches(v, &token) => Ok(req),
            _ => Err(Status::unauthenticated("missing or invalid bearer token")),
        }
    }
}
```

- **No `Arc` needed.** `InterceptedService<S, F>` derives `Clone` (`tonic-0.12.3/src/service/interceptor.rs:93`) requiring `F: Clone`, and `TymuxServiceServer::add_service` requires the whole service to be `Clone` (`tonic-0.12.3/src/transport/server/mod.rs:399-408`) because tonic clones the service handle per accepted connection. A closure capturing an owned `String` is `Clone` for free (`String: Clone`) — every connection gets its own cheap clone of the token string. Given tokens are configured once at startup and never rotate at runtime (confirmed out of scope: "no auto-generation, no token file", i.e., no runtime mutation path exists), there's no shared-mutable-state need that would justify `Arc`/`Arc<Mutex<_>>`. `Arc<str>` is a defensible micro-optimization (avoids a heap alloc-and-copy per connection clone instead of a cheap refcount bump) but is not required for correctness — call it an implementation nicety, not an architectural requirement.
- Precedent for "plain captured value, no shared state" already exists in this file: `TymuxDaemon::new` reads env-configured `Duration`s once at construction (`disconnect_regression_window`, `grace_period_duration`, `heartbeat_interval` — `main.rs:93-100`) and stores them as plain owned fields, not behind `Arc<Mutex<_>>>`, because they don't change at runtime either. The token follows the same shape, just one level up (closure capture instead of struct field, since it's not consulted by `TymuxDaemon` itself).

## 3. `Attach`'s bidirectional stream needs no special handling — confirmed against tonic 0.12.3's actual `Interceptor` signature

Pinned version: `tonic 0.12.3` (`Cargo.lock:1610-1612`).

`Interceptor::call` signature (`~/.cargo/registry/.../tonic-0.12.3/src/service/interceptor.rs:46-49`):

```rust
pub trait Interceptor {
    fn call(&mut self, request: crate::Request<()>) -> Result<crate::Request<()>, Status>;
}
```

`InterceptedService`'s `Service::call` impl (`interceptor.rs:122-165`) operates at the `http::Request<ReqBody>` level — i.e., **once per HTTP/2 request**, before the body is touched: it strips the body (`into_parts()`), hands the interceptor only metadata+extensions with a unit `()` body, and only on `Ok` reconstructs the original request (with its real, still-unread streaming body) to forward to the inner service.

For gRPC, a bidirectional-streaming RPC like `Attach` is still exactly **one** HTTP/2 request (one stream, one set of request headers, then a sequence of DATA frames carrying framed protobuf messages both directions over that same stream). The interceptor therefore fires exactly once, at stream setup (equivalent to "checked once when the stream is established," matching the expected behavior stated in the task) — never per-message, and never again for the lifetime of that `Attach` call. This requires no special-casing in `attach()` itself (`main.rs:783` onward, `forward_handle`/`input_handle` per prior research) — auth is fully handled before `TymuxDaemon::attach` is ever invoked.

**Verified locally**, not inferred from docs: read directly from the pinned `tonic-0.12.3` source in `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tonic-0.12.3/src/service/interceptor.rs`.

## 4. "Refuse to start if non-loopback with no token" belongs in `main()`, immediately alongside the existing loopback-warning block

Location: `main.rs:1238-1247`, the existing `if !socket_addr.ip().is_loopback() { tracing::warn!(...) }` block. Extend it to become the fail-fast gate:

```rust
let token = std::env::var("TYMUXD_TOKEN").ok().or_else(|| parse_token_flag(&args));

if !socket_addr.ip().is_loopback() {
    let token = token.ok_or_else(|| {
        "tymuxd is binding to a non-loopback address with no --token/TYMUXD_TOKEN configured; refusing to start"
    })?;
    tracing::warn!(%socket_addr, "tymuxd is binding to a non-loopback address; bearer-token auth is enforced on every call");
    // token used below when building the interceptor
} else {
    tracing::info!(%socket_addr, "tymuxd binding to loopback; no auth required");
}
```

This is the right place because: (a) it's the only place `socket_addr` is already known and already branches on loopback-vs-not, (b) it runs before the `Engine`/session-restore work (`main.rs:1249-1283`) and well before `Server::builder()` (`main.rs:1286`), so failing here is a clean fail-fast with no partial daemon state (no sessions restored, no listener bound) — cheaper and cleaner than failing later inside server construction or on first request.

**CLI flag mechanism — flag for the planning phase, not resolved here**: `tymuxd`'s `Cargo.toml` (`crates/tymuxd/Cargo.toml`) has **no `clap` dependency today** — every existing runtime knob (`TYMUXD_ADDR`, `TYMUXD_DISCONNECT_REGRESSION_WINDOW_MS`, `TYMUXD_GRACE_PERIOD_MS`) is env-var-only, read via bare `std::env::var(...)` in `main()`/`TymuxDaemon::new` (`main.rs:93-100,1227`); there is zero CLI-flag parsing in this binary. `tymux-cli`, by contrast, already depends on `clap` with a `Parser`/`Subcommand` struct (`crates/tymux-cli/src/main.rs:10,178-192`) and global flags like `--addr` (`main.rs:181-182`), so adding a `--token` flag there is a trivial one-line addition to the existing `Cli` struct. For `tymuxd`'s side, requirements.md asks for `--token` *or* `TYMUXD_TOKEN`; the planner should decide between (a) adding `clap` as a new dependency to `tymuxd` purely for one optional flag, or (b) a minimal manual `std::env::args()` scan consistent with this binary's current zero-CLI-parsing style. Given the binary has deliberately stayed dependency-light and flag-free so far, (b) is the better fit unless a second flag is anticipated soon.

## 5. Security/NFR notes affecting architecture

- **Constant-time compare**: no constant-time-comparison crate (`subtle` or similar) is currently in `Cargo.lock` — this is a new dependency need. `subtle::ConstantTimeEq` is the standard, audited choice and is tiny/no-std; a hand-rolled XOR-accumulate loop risks the compiler proving it can short-circuit/optimize without `black_box` guarantees. Recommend adding `subtle` to `tymuxd`'s (and possibly `tymux-cli`'s, if client-side ever needs it — unlikely) `Cargo.toml` rather than hand-rolling.
- **Never logged**: the token must never flow into any `tracing::*!` call at any level. The existing warn/info logging around the loopback branch (`main.rs:1238-1247`, and the new fail-fast/branch logging sketched in §4) logs `%socket_addr` and static strings only — deliberately never `%token`. Same discipline applies to any future debug-level logging of request metadata (don't `tracing::debug!(?metadata)` the whole `MetadataMap` on the auth path — it would leak the bearer value).
- **Client-side (`tymux-cli`, `clients/go`, `clients/ts`)**: the mirror integration point on the client is the same codegen pattern — `TymuxServiceClient::with_interceptor(channel, move |mut req| { req.metadata_mut().insert("authorization", MetadataValue::try_from(format!("Bearer {token}"))?); Ok(req) })` in Rust; equivalent per-call metadata/header injection in the Go and TS gRPC stacks. Not detailed further here — out of this agent's architecture-of-`tymuxd` scope, but flagging the symmetry since `tymux-cli`'s client construction site is `crates/tymux-cli/src/main.rs:277-278` (`endpoint.connect().await?` → `TymuxServiceClient::new(channel)`), directly analogous to `tymuxd`'s `main.rs:1286-1291`.

## 6. Hotspot flag (not run, per instructions — just noting)

`crates/tymuxd/src/main.rs` is now 4,511 lines (`wc -l`, verified) — a single file carrying the daemon's core state (`TymuxDaemon`, `main.rs:53`), the full `TymuxService` impl (`main.rs:521` onward), `main()`/server bootstrap, *and* ~3,000+ lines of inline `#[cfg(test)]` module (tests start well before line 1800 and run past line 4160). This auth work adds another server-construction-time branch and a new interceptor function to that same file. No `code-hotspot-analysis` has been run on this codebase; given the file's size and the churn visible in this session alone (attach-resume-protocol epics 1-5 landed here per recent commit history), it's a reasonable candidate for that analysis as a separate follow-up — not blocking this feature, which fits into the existing `main()`/server-registration area without needing a file split.

## Summary of concrete recommendations for the planner

1. Use `TymuxServiceServer::with_interceptor(daemon, interceptor_fn)`, branching at the existing loopback check (`main.rs:1238`), not a manual per-handler check or a tower layer.
2. Token is a plain `String` (or `Arc<str>` as a minor optimization) captured by-value in the interceptor closure — not a field on `TymuxDaemon`/`Engine`, no `Arc<Mutex<_>>`.
3. No special handling needed for `Attach`'s bidi stream — confirmed from tonic 0.12.3 source that `Interceptor::call` runs once per HTTP/2 request (i.e., once at stream setup), before any message framing.
4. Fail-fast check belongs immediately in/around the existing `if !socket_addr.ip().is_loopback()` block in `main()` (`main.rs:1238-1247`), before session restore and before `Server::builder()`.
5. New dependency needed: `subtle` (or equivalent) for constant-time comparison — not currently in the tree. `tymuxd` currently has zero CLI-flag parsing (env-vars only); decide during planning whether `--token` justifies adding `clap` there or should be hand-parsed.
