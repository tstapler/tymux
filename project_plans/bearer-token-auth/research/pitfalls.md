# Pitfalls Research: bearer-token-auth

Research for `project_plans/bearer-token-auth/requirements.md`. Focus: known failure
modes for gRPC/tonic bearer-token interceptors, this repo's specific PTY-spawning
architecture, and cross-language client gotchas.

## 1. Token exposure: logging, argv, env, core dumps, child processes

- **Never `Debug`/`Display` the token or the `authorization` metadata value.**
  `tracing::warn!` on rejection (required by Observability Requirements) must log
  peer address + RPC method only — never the metadata map itself. A common near-miss:
  logging `?request` or `?metadata` on an error path "for debugging" during
  development, which silently reintroduces the leak in a later refactor. Worth a
  code-review checklist item / grep-for-`{:?}` gate on the interceptor module rather
  than trusting memory.
- **Panics/error Debug dumps are a leak vector too.** requirements.md already flags
  `tymux-cli`'s raw `anyhow` Debug-dump error path (`is-it-ready-2026-07-13.md`
  finding #10) as a UX gap; for this feature it's also a *security* gap if a
  `tonic::Status` or connection error ever wraps a value containing the token
  (e.g. a client that embeds the token in a connection-URL-style debug string). Keep
  the token out of any type that derives `Debug` and gets propagated into an error
  chain — wrap it in a newtype with a manual `Debug` impl that prints `"<redacted>"`
  if it's stored in a struct at all, rather than a bare `String`/`&str`.
- **`ps aux` / `/proc/<pid>/cmdline` vs `/proc/<pid>/environ`.** `--token secret`
  is visible to *every local user* via `ps aux`/`ps -eo args` by default on Linux
  (argv is world-readable). `/proc/<pid>/environ`, by contrast, is readable only by
  the owning UID and root (mode 0400, owner-only) — so `TYMUXD_TOKEN=secret tymuxd`
  is meaningfully safer than `--token secret` against *other local users* on a
  shared host, though not against root, a debugger attached to the process, or a
  core dump. If `tymux-cli` uses `clap`'s `#[arg(long, env = "TYMUXD_TOKEN")]`
  pattern for the CLI-side flag too, clap prints the env var's *current value* in
  `--help` output unless `hide_env_values` is set — check this explicitly, since
  the daemon's `--help`/`clap` usage would otherwise leak the token to
  stdout/terminal scrollback/screen-recording.
- **Core dumps.** A `SIGSEGV`/`abort()` core dump captures the full process memory,
  including argv and environ (env vars are copied onto the process's own stack at
  exec time, not just held by the parent shell) — this is a risk for *both*
  `--token` and `TYMUXD_TOKEN` equally, not an argument for one over the other.
  Not something to build new mitigation for in this feature (out of scope per
  requirements — no token file, no auto-rotation), but worth noting in the
  threat model doc if one gets written: default `core_pattern`/`ulimit -c 0`
  hygiene on the host, not code, is the mitigation.
- **Child-process inheritance is the sharpest repo-specific risk — see §2.**

## 2. Repo-specific: `TYMUXD_TOKEN` and PTY child-process inheritance (HIGH PRIORITY)

**This is the most concrete, most likely-to-bite finding in this research.**

`crates/tymux-core/src/pane.rs:210` (`spawn_internal`) builds every pane's child
process with:

```rust
let mut cmd = CommandBuilder::new(command);
if let Some(cwd) = cwd {
    cmd.cwd(cwd);
}
let child = pair.slave.spawn_command(cmd)?;
```

No `.env_clear()`, `.env_remove()`, or explicit env allowlist is ever called.
`portable_pty::CommandBuilder` — confirmed via its docs.rs page
(https://docs.rs/portable-pty/latest/portable_pty/cmdbuilder/struct.CommandBuilder.html) —
**inherits the parent process's full environment by default**, the same as
`std::process::Command`; `env_remove()`/`env_clear()` exist precisely because the
default is "inherit everything," and `iter_full_env_as_str()` vs
`iter_extra_env_as_str()` on the builder distinguish "the whole inherited set" from
"just what the caller explicitly added."

**Consequence:** if the operator configures `tymuxd` via `TYMUXD_TOKEN=<secret>
tymuxd --addr 0.0.0.0:7419` (the exact form the requirements document recommends —
"operator-supplied... matching standard shared-secret-via-env practice"), that env
var is `tymuxd`'s own process environment, and **every PTY pane it spawns for every
session inherits it** — visible to `env`/`printenv` run inside any pane, to any
script or program the user runs in that pane, and to anything *that* process spawns
in turn. Since a bind to a non-loopback address is precisely the shared/hosted
scenario where other people are also getting shells on the same daemon (that's the
whole threat this feature exists to gate), this leaks the very secret meant to keep
them out — to the legitimate authenticated user's own shell session, which is a
lesser but still real problem (an attached user's process could exfiltrate the
daemon's bearer token to a *different*, non-loopback-reachable copy of `tymuxd`, or
just read it and hand it to someone else with equivalent access).

**Fix must be explicit, not assumed**: wherever the token is read (`--token` flag
or `TYMUXD_TOKEN` env var) at daemon startup, it must be kept in a variable that is
never re-exported into `std::env` for the daemon's own process, *and*
`spawn_internal` needs `cmd.env_remove("TYMUXD_TOKEN")` (or, more robustly,
`cmd.env_clear()` + explicit allowlist of what panes actually need — a larger
change likely out of scope here) before `spawn_command`. At minimum, the
implementation plan should add an explicit `env_remove("TYMUXD_TOKEN")` call in
`pane.rs` and a regression test that spawns a pane with `TYMUXD_TOKEN` set in the
test harness and asserts it's absent from the child's `env` output. This isn't
mentioned anywhere in requirements.md's Scope/Non-functional sections — it should
be, as an explicit acceptance criterion, not left to be caught in review.

(Note: the `--token` CLI-flag path doesn't have this specific leak — argv isn't
inherited by children the way env is — but per §1, `--token` has the *other*
weakness, local-user argv visibility. Neither delivery mechanism is strictly safer
across both axes; the interceptor code should treat "how the token got into the
daemon process" as irrelevant, and pane-spawn hygiene should scrub it regardless of
delivery mechanism, since env is where it lands either way once `std::env::var` or
`clap`'s `env=` resolution reads it into the process's environment table for any
downstream code that calls `std::env::vars()` — including `CommandBuilder`'s
default inheritance.)

## 3. Tonic/gRPC interceptor + streaming pitfalls

- **Interceptor timing is confirmed safe for the `Attach` bidi stream.** Per
  tonic's docs (https://docs.rs/tonic/latest/tonic/service/interceptor/trait.Interceptor.html)
  the `Interceptor::call` signature takes `Request<()>` — metadata only, before
  the body/stream is touched — and this applies uniformly whether the underlying
  RPC is unary or streaming, because tonic wraps interceptors at the Tower
  `Service` layer (HTTP/2 request headers arrive before any stream frames).
  `crates/tymux-proto/proto/tymux/v1/tymux.proto`'s `rpc Attach(stream
  AttachRequest) returns (stream AttachEvent)` (confirmed at
  `proto/tymux/v1/tymux.proto:99`) is exactly the case requirements.md's Rabbit
  Holes section flags — the interceptor rejects it identically to a unary call,
  by returning `Err(Status::unauthenticated(...))` from `call()` before the
  handler (and before `pair.slave.spawn_command`-style resource acquisition,
  irrelevant here but the general pattern) ever runs. This should be verified
  with an actual integration test against `Attach` specifically, not assumed
  from unary-RPC testing alone, since it's the one RPC where "did the rejection
  really happen pre-stream" is worth proving rather than trusting.
- **What a rejected request looks like on the wire is a trailers-only HTTP/2
  response** carrying `grpc-status: 16` (`UNAUTHENTICATED`) — this requires a full
  TCP+HTTP/2 connection/stream to already be open (interceptor rejection happens
  *after* the transport layer, not instead of it), so there's no resource leak on
  the *server* side beyond a stream that opens and immediately closes; tonic/hyper
  handle this the same way as any other early-return `Status`. No specific open
  tonic GitHub issue was found describing an interceptor-triggered leak or
  half-open stream on rejection specifically (searched hyperium/tonic issues for
  "interceptor" + "streaming" + hang/leak — the closest hits, e.g.
  https://github.com/hyperium/tonic/issues/515 and
  https://github.com/hyperium/tonic/issues/1758, are about *client-side* stream
  lifecycle assumptions and manually failing a client-streaming RPC mid-flight,
  not about interceptor rejection specifically) — treat "no known tonic bug here"
  as the current best evidence, not as proof of absence; verify empirically with
  an integration test that asserts the client sees `Unauthenticated` promptly
  (not a hang/timeout) when attaching with a bad token.
- **Async interceptors aren't supported natively** (tonic issue #870,
  https://github.com/hyperium/tonic/issues/870) — `Interceptor::call` is
  synchronous. The constant-time token comparison must be a pure, fast,
  synchronous operation (which it will be — comparing against one in-memory
  token, per the NFR) — this rules out any design that would want to look up the
  token asynchronously (e.g. from a future secrets-rotation source), which is
  fine given the current single-static-token scope but worth flagging if a later
  "scoped tokens" roadmap item wants per-request async lookup — that would need a
  Tower middleware layer instead of `tonic::Interceptor`.
- **A working async-auth pattern with tonic today is a `tower::Layer` +
  `Service`, not `tonic::Interceptor`**, precisely because of the above — not
  needed for this feature's scope (one static token, sync compare) but worth a
  one-line note in the implementation plan for why `Interceptor` (not a custom
  Tower layer) is the right level of complexity here.

## 4. Cross-language client pitfalls (Go / TypeScript)

- **Metadata key casing: gRPC/HTTP2 header names are lowercase on the wire.**
  tonic's `tonic::metadata::MetadataKey<Ascii>` (and HTTP/2 generally) requires
  ASCII header names to be lowercase — inserting `"Authorization"` (capitalized)
  as a tonic metadata key on the Rust side will fail to parse/panic depending on
  how it's constructed; the convention across grpc-go, connect-go, and
  `@connectrpc/connect` is the same requirement (HTTP/2 mandates lowercase field
  names; grpc-go's own examples use `metadata.Pairs("authorization", "Bearer
  ...")`). Standardize on lowercase `"authorization"` everywhere (server
  interceptor, `tymux-cli`, `clients/go`, `clients/ts`) — mixed casing across the
  three client implementations is exactly the kind of "assumed symmetry that
  isn't there" risk requirements.md's Rabbit Holes section already calls out for
  status-shape differences; the same caution applies to the header key itself.
- **Bidi-stream initial metadata vs per-message auth.** For `connect-go` and
  `@connectrpc/connect`, the bearer token must be attached as *connection/stream-
  establishment metadata* (headers set when the stream is opened —
  `connect.NewClient(..., connect.WithInterceptors(authInterceptor))` in Go, or a
  fetch-level header/interceptor in TS) — not re-sent per message on the `Attach`
  bidi stream. A common mistake is wiring auth into a per-RPC helper that works
  for unary calls but is never invoked for the long-lived `Attach` stream because
  that code path calls the raw streaming client method directly, bypassing
  whatever unary-call wrapper carries the auth header. Each client's
  integration test (required by requirements.md's Success Metrics) should
  specifically exercise `Attach`, not just a unary RPC like `ListSessions`, to
  catch this.
- **Retry/reconnect logic dropping the token.** requirements.md's proto comments
  (`proto/tymux/v1/tymux.proto` around the `Attach` RPC) describe an existing
  reconnect-with-backoff policy (ADR-004) for stream failure. If a client's
  reconnect path constructs a fresh stream/channel without re-threading the same
  auth interceptor/header config used for the initial connection (e.g. reconnect
  logic that re-dials a raw channel rather than reusing the client object that
  carries the configured interceptor), reconnection after a transient failure
  would silently start sending unauthenticated requests and get rejected in a
  loop that looks like "the daemon is down" rather than "the token stopped being
  sent" — worth an explicit test: kill and restore the connection mid-session,
  confirm the token is still attached post-reconnect.
- **Go/TS status-shape differences** (already flagged in requirements.md Rabbit
  Holes) — confirm concretely during implementation, not assumed: `connect-go`
  surfaces `Unauthenticated` as a `*connect.Error` with `.Code() ==
  connect.CodeUnauthenticated`; `@connectrpc/connect` in TS surfaces it as a
  `ConnectError` with `.code === Code.Unauthenticated`. Both differ in shape from
  Rust's `tonic::Status::code() == tonic::Code::Unauthenticated`. Each client's
  error-handling code needs its own explicit check, not a shared assumption.

## 5. Fail-fast startup-gate pitfalls

- **Empty-string token is the sharpest edge case here.** `--token ""` or
  `TYMUXD_TOKEN=""` must be treated as "no token configured" (fail startup on
  non-loopback bind), not as "a valid token that happens to be empty." If the
  startup check is naively `if token.is_none() { fail }` and `--token ""` parses
  to `Some(String::new())`, the check passes, the daemon starts accepting
  connections, and the interceptor compares incoming tokens against `""` — any
  client sending `Authorization: Bearer ` (empty bearer value) or even no
  `authorization` header at all, if the extraction code defaults a missing
  header to `""` instead of rejecting outright, would then pass. This is the
  single most dangerous bug shape in this feature: it would look like "auth is
  configured and working" (daemon started, doesn't warn) while actually
  authenticating nobody. The startup check must explicitly treat an empty string
  the same as "absent" — `token.filter(|t| !t.is_empty())`-style, not a bare
  `Option` check — and this should be a named test case (`empty_token_flag_fails_
  startup_same_as_missing`), not left implicit.
- **Precedence bugs between `--token` and `TYMUXD_TOKEN`.** This repo has no
  existing precedent for CLI-flag-vs-env-var precedence (confirmed:
  `crates/tymuxd/src/main.rs` today only reads env vars via `std::env::var`
  directly — e.g. `TYMUXD_ADDR` at line 1227 — there is no `clap` usage in
  `tymuxd` at all yet, and `tymux-cli`'s existing `clap::Parser` struct at
  `crates/tymux-cli/src/main.rs:178-192` has no `env = "..."` attribute on any
  existing flag to follow as precedent). If `--token` is added via clap's
  `env = "TYMUXD_TOKEN"` attribute, clap's own precedence (explicit CLI value
  wins over env var) becomes the de facto behavior — fine, but it must be a
  deliberate choice stated in the plan, not an accident of which mechanism was
  implemented first, and the "flag beats env" precedence needs its own test if
  both are set to different values in a startup test.
- **No config-reload path exists today** for `tymuxd` (confirmed: no SIGHUP-
  triggered reload, no watched config file in the areas of `main.rs` inspected;
  the daemon reads `TYMUXD_ADDR`/`TYMUXD_TOKEN` once at process start) — so
  "bypass via config reload" isn't a live risk for v1 of this feature. Worth a
  one-line note if a future config-reload feature is ever added: it must re-run
  the same fail-fast bind-vs-token check, not just accept a live token change
  without re-validating the non-loopback-requires-token invariant.
- **Startup-ordering race**: the check must run *before* `Server::builder()...
  .add_service(...).serve(addr)` starts accepting connections (confirmed pattern
  at `crates/tymuxd/src/main.rs:1286-1289`) — i.e., before the `socket_addr`
  bind, not just before the interceptor is constructed. If the token-presence
  check is wired only into interceptor construction but the `Server::builder()`
  call happens regardless of whether that construction succeeded (e.g. a
  `let interceptor = ...; // may be None` pattern where `None` silently degrades
  to "no auth" instead of aborting `main`), that's a fail-*open* bug hiding
  behind fail-fast-looking code. The gate should be an early `return
  Err(...)`/`std::process::exit` in `main()` itself, before any `Server::builder`
  call, mirroring how the existing `tracing::warn!` block at
  `crates/tymuxd/src/main.rs:1230-1247` (soon to become a hard gate) already sits
  before all the session-loading and server-wiring code that follows it.

## Summary of sources consulted

- Repo: `crates/tymuxd/src/main.rs` (startup/env/Server wiring),
  `crates/tymux-core/src/pane.rs` (PTY spawn / `CommandBuilder` usage),
  `crates/tymux-cli/src/main.rs` (existing clap `Cli` struct),
  `proto/tymux/v1/tymux.proto` (`Attach` bidi-stream RPC definition),
  `project_plans/bearer-token-auth/requirements.md`.
- docs.rs: `portable_pty::CommandBuilder`
  (https://docs.rs/portable-pty/latest/portable_pty/cmdbuilder/struct.CommandBuilder.html),
  `tonic::service::interceptor::Interceptor`
  (https://docs.rs/tonic/latest/tonic/service/interceptor/trait.Interceptor.html).
- GitHub: hyperium/tonic issues #515, #700, #741, #870, #911, #1758 (searched for
  interceptor/streaming rejection/hang precedent — none found describing a
  resource leak specific to interceptor-rejected streams; closest hits are about
  client-side stream lifecycle assumptions, unrelated to this feature's server-
  side rejection path).
