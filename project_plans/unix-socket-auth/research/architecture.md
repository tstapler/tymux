# Architecture research: unix-socket-auth

Agent 3 (Architecture). Builds on `project_plans/bearer-token-auth/research/architecture.md`
(interceptor pattern, `main.rs` loopback-branch precedent) and
`project_plans/bearer-token-auth/implementation/architecture-review.md` (the
`crates/tymuxd/src/auth.rs` extraction precedent) — both read in full; findings
cited by file:line below, not re-derived.

## 0. Current state (post bearer-token-auth, re-located per instructions)

The `is_loopback` gate has moved since the prior project's research doc was
written. Current locations, verified via `grep -n "is_loopback"
crates/tymuxd/src/main.rs`:

- `main.rs:1239` — `let is_loopback = socket_addr.ip().is_loopback();`
- `main.rs:1255` — `auth::check_non_loopback_requires_token(is_loopback, ...)`
  fail-fast gate
- `main.rs:1261` — `if !is_loopback { tracing::warn!(...) } else {
  tracing::info!(...) }` — the deprecation-relevant startup branch this
  feature's own TCP-deprecation warning is a sibling to
- `main.rs:1312-1330` — the two-armed `Server::builder()...add_service(...)`
  (with/without `BearerAuthInterceptor`), `serve_with_shutdown(socket_addr,
  shutdown_signal())`

`crates/tymuxd/src/auth.rs` (176 lines + tests) is the established module
precedent: `BearerToken` newtype (parse-don't-validate, no `PartialEq`/`Eq` to
avoid a non-constant-time equality path — `auth.rs:13-40`),
`resolve_token`/`check_non_loopback_requires_token` as pure functions
(`auth.rs:60-95`), `BearerAuthInterceptor` implementing
`tonic::service::Interceptor` with its own `Arc<AtomicI64>` rejection counter
kept out of `TymuxDaemon`/`Engine` (`auth.rs:102-166`). `main.rs` is 4,635
lines (`wc -l`, confirmed) — this feature should follow the same
extract-a-module discipline; see §5.

Three test-harness call sites, current locations:

- `main.rs:1439` `spawn_test_server` — binds `127.0.0.1:0` (loopback), no
  interceptor
- `main.rs:1463` `spawn_non_loopback_test_server` — binds `0.0.0.0:0`
  (non-loopback), wires `BearerAuthInterceptor`
- `main.rs:3227` inlined in `attach_streams_output_and_signals_exit` — binds
  `127.0.0.1:0`, no interceptor

**All three already use `.serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))`**,
not `.serve_with_shutdown(socket_addr, ...)`. This is the load-bearing fact for
this feature: the test harness is already on the generic incoming-stream API,
so adding a UDS variant is "swap `TcpListenerStream`+`TcpListener` for
`UnixListenerStream`+`UnixListener`," not a new API surface for tests to
learn.

## 1. Two independent `Server` instances, not one `Router` fed two IO types — confirmed against tonic 0.12.3's actual signatures

Pinned versions (`Cargo.lock`, confirmed): `tonic 0.12.3` (`Cargo.lock:1616-1617`),
`tokio 1.52.3` (`Cargo.lock:1523-1524`).

`Server::serve_with_incoming`'s real signature
(`~/.cargo/registry/.../tonic-0.12.3/src/transport/server/mod.rs:839-846`):

```rust
pub async fn serve_with_incoming<I, IO, IE, ResBody>(
    self,
    incoming: I,
) -> Result<(), super::Error>
where
    I: Stream<Item = Result<IO, IE>>,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
    ...
```

`IO` is a single concrete type parameter for the whole call — `TcpStream` and
`UnixStream` can't both flow through one `serve_with_incoming::<_, IO, _, _>`
invocation without first erasing them behind a common enum/trait object (extra
machinery, not needed here). **Recommendation: two separate `Server::builder()`
calls, each in its own `tokio::spawn`, exactly matching the existing
`spawn_test_server`/`spawn_non_loopback_test_server` shape** — one servable
against `TcpListenerStream`, one against `UnixListenerStream`
(`tokio_stream::wrappers::UnixListenerStream`, same crate/feature already
depended on for the TCP variant — `tokio-stream = { version = "0.1", features
= ["net"] }`, `Cargo.toml:26` — `["net"]` already covers Unix, no new feature
flag needed, confirmed the module exists: `unix.rs` sits alongside `tcp.rs` in
`tokio-stream`'s `wrappers` module for the "net" feature).

This resolves the requirements.md rabbit hole "tonic + UnixListener
composition (serving TCP + UDS concurrently from one `Server::builder()`)":
it isn't one `Server::builder()`, it's two, run as sibling tasks under the
same `main()`, joined at shutdown (see §4 for shutdown-signal sharing).

### State sharing across both listeners: `TymuxDaemon` needs `#[derive(Clone)]` — it doesn't have one today

`TymuxDaemon` (`main.rs:55-86`) has no `Clone` impl currently — it's
constructed exactly once per `main()` today. Two listeners each need their own
`TymuxServiceServer::new(daemon)` (or `with_interceptor(...)`) registration
(mirroring the prior feature's already-established two-armed pattern at
`main.rs:1312-1330`, now times two listeners), which means two `TymuxDaemon`
values are needed at the call site.

**Do not construct them via two separate `TymuxDaemon::new(engine.clone())`
calls** — that's the trap. `TymuxDaemon::new` allocates its *own* fresh
`Arc::new(Mutex::new(HashMap::new()))` for `disconnect_tracker`
(`main.rs:65-72`), fresh `Arc<AtomicI64>` for `attached_sessions_gauge`
(`main.rs:73-79`), and fresh `Arc<ResumeOutcomeCounters>` for
`resume_outcome_counters` (`main.rs:80-85`) on every call — only `engine:
Arc<Engine>` is actually shared if you pass the same `Arc` in twice. Two
`TymuxDaemon::new(engine.clone())` calls would silently split
`attached_sessions_gauge` and `disconnect_tracker` into two independent
counters/maps, one per listener — a pane attached over UDS then reattached
over TCP would miss the disconnect-regression check
(`disconnect_tracker`'s whole purpose, per its doc comment at `main.rs:57-64`)
because the two listeners' `TymuxDaemon` instances wouldn't see each other's
entries. The attached-streams gauge (`main.rs:73-79`) would also undercount
per listener instead of reporting the daemon-wide total.

**Fix: add `#[derive(Clone)]` to `TymuxDaemon`.** Every field is already
either `Arc<T>` (`engine`, `disconnect_tracker`, `attached_sessions_gauge`,
`resume_outcome_counters`) or a plain `Copy` `Duration`
(`disconnect_regression_window`, `grace_period_duration`,
`heartbeat_interval`) — the derive is free, and `daemon.clone()` then shares
every piece of state, including the ones that aren't `Engine` itself. Build
the daemon once, `daemon.clone()` it into the second listener's
`add_service(...)` call. This is a small, concrete, easy-to-miss correctness
requirement the planner should turn into an explicit task, not something to
discover mid-implementation.

## 2. Peer-cred data flow: tonic already ships `Connected for UnixStream`; no manual `peer_cred()` plumbing needed

Traced against the actual pinned source, not hypothetically:

- `tonic-0.12.3/src/transport/server/unix.rs:14-30`:
  ```rust
  pub struct UdsConnectInfo {
      pub peer_addr: Option<Arc<tokio::net::unix::SocketAddr>>,
      pub peer_cred: Option<tokio::net::unix::UCred>,
  }
  impl Connected for tokio::net::UnixStream {
      type ConnectInfo = UdsConnectInfo;
      fn connect_info(&self) -> Self::ConnectInfo {
          UdsConnectInfo { peer_addr: self.peer_addr().ok().map(Arc::new), peer_cred: self.peer_cred().ok() }
      }
  }
  ```
  tonic already wraps `UnixStream::peer_cred()` (the exact API
  requirements.md names) — this feature doesn't call `peer_cred()` directly,
  it consumes `UdsConnectInfo` that tonic populates automatically for any
  `serve_with_incoming` fed `UnixStream`s.

- **Timing, traced concretely** (`tonic-0.12.3/src/transport/server/mod.rs:1005-1064`,
  `MakeSvc::call`): tonic's per-connection `MakeService` factory
  (`impl Service<&ServerIo<IO>> for MakeSvc<S, IO>`) calls
  `io.connect_info()` at `mod.rs:1023` — i.e. **once per accepted connection**,
  before the HTTP/2 handshake on that connection begins (this factory *builds*
  the per-connection service; the h2 handshake runs after). For
  `UnixStream` this is one `getsockopt(SO_PEERCRED)` syscall per accept. The
  resulting `UdsConnectInfo` is then cloned into `request.extensions_mut()`
  via a `.map_request(...)` layer (`mod.rs:1038-1042`) applied to *every*
  request on that connection — a cheap `Clone` of the already-fetched value,
  not a re-syscall. **This directly satisfies the NFR** ("peer-cred check
  once per connection at accept, not per-RPC") for free, with no
  implementation work beyond reading the extension.
- **No built-in `Request::peer_cred()` accessor exists** — `tonic::Request`
  only exposes `remote_addr()` (`tonic-0.12.3/src/request.rs:237`, TCP/TLS-only,
  reads `TcpConnectInfo`/`TlsConnectInfo`). The interceptor must instead read
  `req.extensions().get::<tonic::transport::server::UdsConnectInfo>()`
  directly and pull `.peer_cred` — the same "manual extensions lookup"
  pattern `BearerAuthInterceptor::call` already uses for `req.remote_addr()`
  (`auth.rs:123`), just one level less pre-packaged.
- `UCred`'s API (`tokio-1.52.3/src/net/unix/ucred.rs:14-30`): `.uid() ->
  uid_t`, `.gid() -> gid_t`, `.pid() -> Option<pid_t>` (`pid()` doc comment,
  verbatim: *"only implemented under Linux, Android, iOS, macOS, Solaris,
  Illumos and Cygwin"* — confirms `pid` availability breadth beyond what
  requirements.md assumed; see §6).

**Recommendation for the interceptor's relationship to `BearerAuthInterceptor`:
a distinct, UDS-specific interceptor, not a shared/composed one.** The two
check fundamentally different things (kernel-verified uid/gid vs. a
client-supplied bearer string) against fundamentally different
`ConnectInfo` types (`UdsConnectInfo` vs `TcpConnectInfo`), and
`bearer-token-auth`'s own scope note is explicit that non-loopback bearer-auth
is unchanged by this project ("Changing bearer-token-auth's non-loopback
bearer-token mechanism" is Out of Scope). Concretely: register
`TymuxServiceServer::with_interceptor(daemon.clone(), UdsPeerCredInterceptor::new(allowed_uid,
allowed_gid))` on the UDS listener's `add_service(...)`, independent from
whatever interceptor (bearer or none) the TCP listener uses — same
"`with_interceptor` on server-registration site" pattern the prior project
already validated (`bearer-token-auth/research/architecture.md` §1), applied
to a second, parallel registration rather than composed into one interceptor
type. A single interceptor trying to branch on "am I the TCP listener or the
UDS listener" would need to smuggle that fact through `Request`, which nothing
naturally carries — two listeners, two interceptor instances, is simpler and
matches the existing two-armed `if let Some(token) = ... else` shape at
`main.rs:1312-1330` (that shape already proves two arms of
`Server::builder()...add_service(...)` differing only in interceptor is a
codebase-idiomatic pattern — this feature is a straightforward extension of
it to two listeners instead of one).

## 3. Module placement: extend `auth.rs`, don't create a disconnected `uds.rs`

requirements.md's own file guess floats both `crates/tymuxd/src/uds.rs` and
"extending `auth.rs`." Recommendation: **extend `auth.rs`**, for the same
reason the prior architecture-review gave for creating it in the first place
(`architecture-review.md`'s second Concern, now resolved) — this is the same
kind of concern (a request-gate concern, pure functions + one
`tonic::service::Interceptor` impl, kept off `TymuxDaemon`/`Engine`), not a
new domain. A `uds.rs` would fragment "how does tymuxd decide whether to let
this call through" across two files for what's conceptually one policy
question (loopback bearer-token OR peer-uid check, depending on transport).
Concretely: add `UdsPeerCredInterceptor`, the socket-path resolution helper,
and the group-relaxation logic to `auth.rs`, alongside (not replacing)
`BearerToken`/`resolve_token`/`BearerAuthInterceptor`. This keeps the
main.rs-god-file remediation the prior review already won (Concern 2,
resolved) from regressing on the very next auth-adjacent feature — exactly
the scenario that review's remediation note called out as the risk of
*not* extracting `auth.rs` last time (`architecture-review.md:94-95`:
"compounds the next feature's cost").

`main()`'s integration points, concretely:

1. After the existing token-resolution block (`main.rs:1237-1259`), add UDS
   socket-path resolution + mode/group setup (new pure function(s) in
   `auth.rs`, unit-testable without a real bind, same style as
   `resolve_token`).
2. After `let daemon = TymuxDaemon::new(engine);` (`main.rs:1309`), bind the
   `UnixListener` at the resolved path, `chmod`/`chown` per configured
   group (see requirements.md's group-access scope item), and spawn the
   second `Server::builder()...serve_with_incoming(UnixListenerStream::new(...))`
   task alongside the existing TCP one — both take `daemon.clone()` (§1).

## 4. Shutdown-signal sharing between two listener tasks

Both tasks should still resolve on the same Ctrl-C/SIGTERM
(`shutdown_signal()`, referenced at `main.rs:1321,1328`, defined
`main.rs:1335` onward per the earlier `Read`). `shutdown_signal()`'s current
signature is called once per `serve_with_shutdown` today (single listener);
with two listeners it's called twice, which means two independent
`tokio::signal::ctrl_c()`/SIGTERM futures racing — fine for `ctrl_c()`
(multiple listeners are allowed) but worth the planner confirming
`shutdown_signal()`'s SIGTERM path (if it's `signal(SignalKind::terminate())`)
doesn't assume single-consumer semantics. If it does, broadcast a single
resolved signal to both tasks via `tokio::sync::watch` or clone a
`CancellationToken` instead of calling `shutdown_signal()` twice — flag as an
Unresolved Question for the planner rather than resolving here (not visible
from the excerpt read; needs the full `shutdown_signal()` body, out of this
research pass's line budget).

Also: with two `Server::builder()` tasks, `main()` needs a `tokio::try_join!`
(or `select!` with a documented "either exiting is fatal" story) instead of
today's single `.await?` — the current shape after
`add_service(...).serve_with_shutdown(socket_addr, shutdown_signal()).await?;`
(`main.rs:1321-1322`/`1328-1329`) implicitly makes "the listener returns"
equivalent to "the daemon exits." With two listeners, one erroring or
returning early (e.g. a `bind()` failure on the UDS path — stale socket file,
permission error on `$XDG_RUNTIME_DIR`) needs an explicit decision: does the
daemon keep running TCP-only, or does it treat a UDS bind failure as fatal to
the whole process? Given the requirements' "Fail toward more restrictive"
Risk Control principle, and that TCP is the deprecated/insecure listener,
**recommend UDS bind failure is fatal** (refuse to start, matching the
existing fail-fast precedent for missing bearer tokens at `main.rs:1255-1259`)
rather than silently degrading to the less-secure TCP-only mode — a silent
degrade here is exactly the "false confidence" failure mode flagged in §6.

## 5. `auth.rs` module-boundary implication for `main.rs`'s existing god-file concern

Not re-litigating the god-file finding itself (architecture-review.md
Concern 2, already resolved by extracting `auth.rs`) — flagging only that
this feature's `main()` changes (§3's two integration points, §1's `daemon.clone()`,
§4's `try_join!`) still land in `main.rs` itself (startup sequence, listener
construction), same as the prior feature's fail-fast gate and
`Server::builder()` branch did. That's consistent with the established
division of labor: `auth.rs` holds policy/pure-function logic and
`Interceptor` impls; `main.rs` holds the startup sequence that wires them in.
No new remediation needed here beyond what §3 already recommends.

## 6. Migration/deprecation-specific failure modes

**Warning swallowed by log-level config**: the existing TCP-deprecation-adjacent
warning at `main.rs:1262-1267` (`tracing::warn!` on non-loopback bind) and the
new "both-by-default" TCP-loopback-is-unauthenticated warning this feature
adds are both subject to the same risk: this binary's only log-level
configuration is `tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))`
(`main.rs:1220-1225`) — `warn` is below `info` in verbosity ranking (warn is
*more* severe, always shown at the default `"info"` filter), so **today's
default config does not swallow it** — a filter would need to be explicitly
set below `warn` (e.g. `RUST_LOG=error`) to hide it, which is an operator
opt-in to losing visibility, not a default-config trap. This is a materially
different risk profile than requirements.md's phrasing suggests ("could the
warning be silently swallowed the same way bearer-token-auth's own
architecture-review flagged") — re-reading `architecture-review.md` in full,
it does not in fact flag a swallowed-warning risk anywhere (searched; no
match) — that framing in requirements.md appears to be describing a risk
this project should *check for*, not one the prior review actually raised.
Verified conclusion: **not a live risk today at the default filter level**,
but still worth a startup self-check (e.g. log the resolved `EnvFilter`
level once at boot, or add an integration test asserting the warning is
emitted at `warn` level specifically, not `info`/`debug`) since an operator
who *has* set `RUST_LOG=error` for noise reasons would silently lose exactly
the warning meant to prompt them to migrate.

**"Both by default" false-confidence risk (UX/messaging, not technical)**: this
is the sharper risk. Once UDS exists and works, an operator who runs `tymux
attach` and it "just works" over the new default UDS path may reasonably
believe the daemon is now secured — while the *same* daemon is still
listening on unauthenticated TCP loopback (or worse, if they'd previously
opted into non-loopback bearer-token mode for remote access, that stays
open too, unrelated to UDS entirely). The single highest-leverage mitigation
is messaging, not code: the startup warning (§ above) should say plainly that
the TCP listener remains open and unauthenticated *and name the flag to turn
it off* (see next paragraph) — "still listening unauthenticated on
{addr}; set `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP=1` to turn it off"
reads very differently from a generic deprecation notice, and is the
difference between an operator who acts and one who assumes "both by
default" already means "protected."

**Recommend adding a TCP off-switch now, even though full removal is out of
scope.** Concretely: a flag/env var (naming to match the existing
`--token`/`TYMUXD_TOKEN` and `TYMUXD_ADDR` conventions — e.g.
`--disable-tcp-loopback` / `TYMUXD_DISABLE_TCP_LOOPBACK=1`) that skips
spawning the TCP listener task entirely. Reasoning:
- **Cheap now**: the TCP-listener spawn becomes one `if
  !tcp_disabled { tokio::spawn(...) }` around code this feature is already
  writing (§1's second `tokio::spawn`) — near-zero marginal cost during this
  project.
- **Expensive to retrofit later**: the future removal project would otherwise
  have to introduce this exact flag *and* flip its default *and* ship both
  changes together, with no intermediate period where operators could
  opt out early and validate UDS-only operation before the flag's default
  changes under them. Shipping the off-switch now, defaulted to "TCP stays
  on," lets operators self-select into UDS-only today; the eventual removal
  project then only needs to flip the default (a one-line, low-risk change
  with a real existing off-switch already field-tested) rather than
  inventing the mechanism *and* the default flip in the same change — the
  Risk Control section's own "staged deprecation" principle applied one step
  further than requirements.md's Out of Scope currently states.
- Matches ADR-002's precedent (`docs adr under project_plans/bearer-token-auth/decisions/ADR-002-tymuxd-token-flag-parsing.md`,
  hand-rolled flag parsing, no `clap`) — this is the same category of "one
  more optional flag on a currently zero-CLI-parsing binary," so it should
  follow the same hand-rolled `std::env::args()` scan `resolve_token` already
  uses (`auth.rs:60-74`), not a new dependency.

**"Removal" concretely requires** (for the planner's Unresolved Questions
list, not resolved here): (1) this off-switch flag existing and having shipped
for at least one release so operators had a chance to adopt it; (2) the
default flip itself (one line); (3) updating the three test-harness call
sites' TCP-listener assumption (`spawn_test_server` et al., §0) since a
default-off TCP listener means those tests would need to explicitly
re-enable it or move to the UDS equivalents this project adds; (4) a doc
update to whatever runbook references `TYMUXD_ADDR`/loopback binding as the
default story. None of this needs building now — only the flag from the
bullet above does, per the "cheap now, expensive later" argument.

## 7. macOS peer-cred parity: requirements.md's stated biggest risk is resolved by the pinned dependency itself

requirements.md's Feasibility Risks: *"macOS peer_cred() support is the
single biggest risk to 'both by default' success metric"*; Constraints:
*"macOS uses LOCAL_PEERCRED, unconfirmed tokio support."*

**Verified directly against the pinned `tokio` source** (not docs, not
inference): `tokio-1.52.3/src/net/unix/ucred.rs` contains a
`pub(crate) mod impl_macos` (`ucred.rs:213` onward) implementing
`get_peer_cred` via `getsockopt`/`LOCAL_PEERCRED`/`xucred`
(confirmed by grepping the file for `LOCAL_PEERCRED`/`XUCRED_VERSION`,
present at `ucred.rs:223,242,253,264` in the closely-related 1.53.1 tree
cached alongside it, same code shape). `UCred::pid()`'s own doc comment
states macOS support explicitly: *"This is only implemented under Linux,
Android, iOS, macOS, Solaris, Illumos and Cygwin. On other platforms this
will always return `None`."* `UnixStream::peer_cred()`
(`tokio-1.52.3/src/net/unix/stream.rs:967`) is not `#[cfg(target_os =
"linux")]`-gated — it's available on every platform tokio's `net/unix` module
compiles for, macOS included, and tonic's `Connected for UnixStream`
(§2) works identically on macOS since it just calls this same method.

**Conclusion for the planner**: macOS gets full `uid`/`gid`/`pid` peer-cred
parity at v1, using the exact same code path as Linux — no
`#[cfg(target_os = "linux")]` branch needed in the interceptor itself. This
resolves requirements.md's Open Question ("Whether macOS gets full peer-cred
parity at v1 or a documented reduced posture" → **full parity, confirmed**)
and removes what requirements.md flagged as the single biggest feasibility
risk. The one still-open macOS-specific item from requirements.md's Rabbit
Holes is unrelated to peer-cred: **default socket path selection**
(`$XDG_RUNTIME_DIR` is Linux/systemd-specific; macOS has no equivalent
environment convention) — that remains a real open question this research
did not resolve (out of this agent's remit — Rabbit Holes assignment implies
a config/build-vs-buy research lane, not architecture), but it's a path-string
problem, not a peer-cred-capability problem, and shouldn't block "both by
default" on macOS the way requirements.md currently frames it.

## Summary of concrete recommendations for the planner

1. Two independent `Server::builder()`/`tokio::spawn` tasks (TCP via
   `TcpListenerStream`, UDS via `UnixListenerStream`), not one `Router` fed
   two IO types — confirmed impossible without type-erasure via tonic
   0.12.3's actual `serve_with_incoming<I, IO, ...>` signature.
2. Add `#[derive(Clone)]` to `TymuxDaemon` (`main.rs:55`) and build it once,
   `daemon.clone()`-ing into the second listener's `add_service(...)`. Do
   NOT call `TymuxDaemon::new(engine.clone())` twice — that silently splits
   `disconnect_tracker`/`attached_sessions_gauge`/`resume_outcome_counters`
   into two independent, non-communicating copies.
3. Peer-cred flows through tonic's existing `Connected for UnixStream` /
   `UdsConnectInfo` (`tonic-0.12.3/src/transport/server/unix.rs:14-30`) with
   zero manual `peer_cred()` calls needed in this feature's own code — read
   `req.extensions().get::<UdsConnectInfo>()` in a new
   `UdsPeerCredInterceptor`, independent from (not composed with)
   `BearerAuthInterceptor`. tonic already calls `connect_info()` once per
   accepted connection, satisfying the once-per-connection NFR for free.
4. Extend `crates/tymuxd/src/auth.rs` (new interceptor + socket-path/group
   helpers) rather than a disconnected `uds.rs` — same policy-concern
   grouping the prior review's remediation already established.
5. `main()` needs `tokio::try_join!` (or equivalent) across both listener
   tasks instead of a single `.await?`, plus a decision (recommended: fatal)
   on UDS-bind-failure behavior, plus a shared shutdown signal across both
   tasks (verify `shutdown_signal()`'s SIGTERM path tolerates two callers).
6. Add a TCP-loopback off-switch flag/env var now (e.g.
   `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK`), defaulted to "TCP
   stays on," so the future removal project only flips a default instead of
   inventing the mechanism under time pressure. Cheap now (~one `if` around
   this feature's own new spawn call), expensive to retrofit later.
7. The startup deprecation warning is not swallowed by today's default log
   filter (`warn` > `info` threshold) — but should explicitly name the new
   off-switch flag to close the "both by default reads as protected" UX gap,
   which is the sharper of the two deprecation-staging risks.
8. macOS peer-cred support is fully present in the pinned `tokio` version
   (`uid`/`gid`/`pid` via `LOCAL_PEERCRED`/`xucred`,
   `tokio-1.52.3/src/net/unix/ucred.rs:213`) — requirements.md's stated
   "single biggest feasibility risk" does not hold up against the pinned
   dependency; recommend closing that Open Question as "full parity" rather
   than carrying it forward as unresolved. The real remaining macOS gap is
   default-socket-path selection (`$XDG_RUNTIME_DIR` has no macOS
   equivalent), not peer-cred capability.
