# ADR-003: Add `tower`/`hyper-util` as direct `tymux-cli` dependencies for UDS dialing

**Status**: Accepted
**Date**: 2026-08-29

## Context

`tymux-cli` dials `tymuxd` via `tonic::transport::Endpoint::connect()`,
which only knows how to dial a `host:port`-shaped URI over TCP. Connecting
over a Unix domain socket instead requires tonic's documented (but
lower-level) `Endpoint::connect_with_connector`/`connect_with_connector_lazy`
API, verified directly against the pinned `tonic-0.12.3` source
(`transport/channel/endpoint.rs:364-404`) — both take a
`C: tower::Service<Uri, Response: hyper::rt::Read + hyper::rt::Write, ...>`,
and tonic's own doc comment on `connect_with_connector_lazy` points at "the
`uds` example" (tonic's official pattern for this exact case) for how to
build one.

`tower` and `hyper-util` are both already present in `Cargo.lock`
transitively (via `tonic`/`hyper`), at `tower 0.4.13` and
`hyper-util 0.1.20` respectively — confirmed by inspecting `Cargo.lock`
directly — but neither is a *direct* dependency of `tymux-cli` today, so
`tower::service_fn` (to build the connector) and
`hyper_util::rt::TokioIo` (to adapt a `tokio::net::UnixStream`'s
`AsyncRead`/`AsyncWrite` into hyper 1.x's `rt::Read`/`rt::Write`) aren't
directly importable without adding them.

## Decision

Add `tower = { version = "0.4", default-features = false, features =
["util"] }` (for `service_fn`) and `hyper-util = { version = "0.1",
default-features = false, features = ["tokio"] }` (for `TokioIo`) as
direct workspace dependencies, pinned to the same major versions already
resolved in `Cargo.lock`, so no second copy of either crate is compiled.
Both are added `default-features = false` with only the specific feature
needed, keeping `tymux-cli`'s dependency footprint minimal despite the
addition.

## Alternatives Rejected

- **Hand-roll a minimal `tower::Service` impl instead of `service_fn`.**
  Rejected: `service_fn` is the documented, zero-risk way to turn an
  `async fn(Uri) -> io::Result<S>` closure into a `Service`; a hand-rolled
  impl would duplicate `tower`'s own ~15 lines for no benefit, and this
  repo's own precedent (`resolve_token`'s hand-rolled *flag* parsing, per
  `bearer-token-auth`'s ADR-002) is about avoiding a dependency for
  something genuinely trivial to hand-roll — a correct `Service` impl
  with the right associated types is not that; `tower` is already in the
  dependency tree regardless.
- **Do not support UDS dialing from `tymux-cli` at all; rely on
  `clients/go`/`clients/ts` only.** Rejected outright — out of scope per
  `requirements.md`'s explicit Success Metrics naming `tymux-cli` as a
  required UDS-dialing consumer.

## Consequences

- `crates/tymux-cli/Cargo.toml` gains two new `[dependencies]` lines;
  `Cargo.toml`'s workspace `[workspace.dependencies]` gains matching
  entries (workspace-level, consistent with every other shared
  dependency in this repo).
- No change to `Cargo.lock`'s resolved versions for either crate — both
  are already locked at the versions this ADR pins.
