# Research: Features — unix-socket-auth

Agent 2 (Features). Scope: existing bearer-token-auth conventions to follow, industry
precedent for local Unix-socket auth, edge cases/failure modes, and unstated needs.

## 1. Existing bearer-token-auth mechanism (the pattern to extend, not replace)

Read: `crates/tymuxd/src/auth.rs` (392 lines, all logic + tests), `crates/tymuxd/src/main.rs:1220-1333`.

**Shape to mirror:**
- **Module boundary**: bearer-token-auth lives entirely in its own `auth.rs`, extracted from
  `main.rs` specifically "to keep the god-file from absorbing another concern" (auth.rs:1-6).
  A new `peer_cred.rs` (or extend `auth.rs`, name TBD in planning) should follow the same
  separation — pure, testable functions plus a tonic `Interceptor`/connection-gate, not logic
  embedded in `main.rs`.
- **Resolution precedent** (`resolve_token`, auth.rs:60-74): flag beats env var
  (`--token` / `--token=val` beats `TYMUXD_TOKEN`), both feed into a `parse`-validated
  newtype (`BearerToken`) so "empty means absent" can't be bypassed by a second call site.
  The new group-access config (flag/env var TBD, e.g. `--socket-group`/`TYMUXD_SOCKET_GROUP`)
  should use the identical flag-over-env precedence and a similarly narrow parse-constructor
  if there's an analogous "empty/invalid means absent" pitfall (e.g. a group name that doesn't
  resolve via `getgrnam`).
- **Startup gate placement** (`main.rs:1229-1273`): loopback-ness is computed once
  (`is_loopback = socket_addr.ip().is_loopback()`), a fail-fast check runs before any binding
  (`check_non_loopback_requires_token`, pure function taking primitives, no I/O — trivially
  unit-testable per auth.rs's own test suite), then a single startup log line records the
  security posture (`tracing::warn!` for the less-safe path, `tracing::info!` for the safe
  default). The UDS feature's peer-cred/group check and TCP-deprecation warning should follow
  this exact shape: pure decision functions unit-tested without a real socket, one startup log
  line each, computed once not per-connection.
- **Interceptor wiring** (`BearerAuthInterceptor`, auth.rs:102-166): implements
  `tonic::service::Interceptor`, owns its own `Arc<AtomicI64>` rejection counter rather than
  reaching into `Engine`/`TymuxDaemon` — "auth is a pure request-gate concern, never consulted
  by RPC handler bodies" (auth.rs:97-101, cites research/architecture.md §2 from that project).
  **This precedent argues against wiring peer-cred as a tonic `Interceptor`** — `Interceptor`
  only sees `tonic::Request<()>`/metadata, not the raw `UnixStream`, so peer-cred (which the
  requirements say must be read once at accept, not per-RPC) has to be extracted before tonic's
  HTTP/2 handshake, then threaded into the request via `req.extensions_mut()` if a per-RPC
  check is still wanted — but the perf requirement ("peer-cred check once per connection at
  accept") really wants a connect-time gate on the incoming stream (reject/drop the raw
  `UnixStream` before handing it into `serve_with_incoming`), not a tonic `Interceptor` at all.
  Flag this for the architecture research agent / plan phase — it's a structural choice, not
  cosmetic.
- **Error UX**: `check_non_loopback_requires_token`'s error string (auth.rs:80-95) is a
  multi-line, human-actionable message printed via `eprintln!` + `std::process::exit(1)`
  (main.rs:1255-1259) — explicitly *not* `.map_err(...)?` because `Box<dyn Error>`'s `Debug`
  impl would mangle the embedded newlines (documented inline, main.rs:1241-1254, "empirically
  confirmed"). Any new fatal startup error (e.g. `$XDG_RUNTIME_DIR` unwritable with no
  fallback) must follow this same eprintln+exit pattern, not `?`.
  On the client side, `BearerAuthInterceptor` returns `Status::unauthenticated("missing bearer
  token")` / `"invalid bearer token"` — short, specific, no internals leaked. The requirement
  "clear specific error on peer-cred rejection (not raw transport-error dump)" should produce
  an equally short `Status` variant client-side (e.g. `PermissionDenied` with a fixed string
  the CLI/SDKs can pattern-match, not a raw `io::Error` from a dropped connection).
- **Observability precedent**: `tymux_auth_rejection_total`-shaped log fields
  (`tymux_auth_rejection_total = count` at auth.rs:139,159) plus `peer = %peer` from
  `req.remote_addr()`. The requirements doc explicitly asks for a
  "`tymux_attach_resume_outcome_total`-style" counter for rejected peer-cred checks — same
  shape, so reuse the field-naming convention: `tymux_socket_peercred_rejection_total` (exact
  name is an open question for planning, but the *pattern* — an `AtomicI64` owned by the gate,
  incremented once per rejection, logged as a `tracing::warn!` field, never per-accepted-call
  — is settled precedent from this codebase already).
- **Tests precedent** (auth.rs:168-392): pure-function tests need no network (`ENV_LOCK` mutex
  guards `std::env::set_var`/`remove_var` races since env mutation is global process state —
  reuse this exact lock pattern for any new env-var-based config). Interceptor tests build a
  `Request<()>` directly and inject `TcpConnectInfo` via `req.extensions_mut()` — the UDS
  equivalent will need to inject peer-cred data the same way (tonic's Unix transport sets
  `UdsConnectInfo` with `peer_cred: Option<UCred>` into request extensions already — see
  Architecture section for confirmation this exists upstream).
- CLI-side default addr is `http://127.0.0.1:7419` (`tymux-cli/src/main.rs:179`, flag
  `--addr`, global). Go example client (`clients/go/examples/list-sessions/main.go:30-41`)
  builds its own `http2.Transport` with a custom `DialTLSContext: func(...) { return
  net.Dial(network, addr) }` for plaintext h2c over TCP — this is exactly the seam a UDS
  dialer would replace (`net.Dial("unix", socketPath)` instead of `net.Dial(network, addr)`).
  TS client (`clients/ts/examples/client.ts:20-21`) uses `@connectrpc/connect-node`'s
  `createGrpcTransport({ baseUrl, ... })` — connect-node's transport accepts `nodeOptions`
  passed through to Node's `http2.connect()`, which supports a `createConnection` callback
  (Node's `net.connect({ path })` for UDS) — same seam, different SDK surface.

## 2. Industry precedent for local Unix-socket auth

| System | Default path | Permission model | Peer-identity source | Notable lesson |
|---|---|---|---|---|
| **Docker daemon** | `/var/run/docker.sock` | root:docker, mode 0660 (group-writable) | none — socket write access *is* the authorization | **Anti-pattern to name explicitly**: membership in the `docker` group is uncontrolled root-equivalent access — "any process that can write to it can send API commands, which dockerd executes with root privileges" ([Netdata guide](https://www.netdata.cloud/guides/docker/docker-socket-security/); [OWASP Docker Security Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Docker_Security_Cheat_Sheet.html)). Docker's group grants *daemon-equivalent* capability with no further per-connection identity check — tymuxd's design differs in kind, not degree: SO_PEERCRED still identifies *which* uid connected, and per-uid/per-group authorization can (in principle, future work) be scoped narrower than "full daemon access," unlike Docker where group membership alone is the entire authorization decision. **Borrow**: group-gated socket file as the mechanism. **Avoid**: treating group membership as the *sole* signal with no verified per-connection identity backing it — tymuxd already does better by design since peer_cred() gives real uid/gid, not just "could open the file."
| **containerd** | `/run/containerd/containerd.sock` (`/var/run/containerd/containerd.sock`) | root:root, mode 0660 by default | none built-in; relies on filesystem perms only | Deliberately **more conservative than Docker**: "unlike the Docker socket, there is usually no requirement for non-privileged users to connect... ownership should be root:root" ([containerd issue #10454](https://github.com/containerd/containerd/issues/10454) — open feature request asking for configurable group access, still unmerged, showing this is a live, unresolved design tension in a comparable daemon). Confirms tymuxd's "configurable group" is solving a real, still-open problem in this space, not a solved one to copy verbatim.
| **PostgreSQL `peer` auth** | N/A (auth method, not a daemon-lifecycle path) | any socket perms; auth decision made from kernel-verified peer identity, not file permission bits | `SO_PEERCRED` (Linux/BSD) / `getpeereid()` — "obtains the client's OS user name from the kernel" ([PostgreSQL 16 docs, §21.9](https://www.postgresql.org/docs/16/auth-peer.html)) | **Closest precedent to tymux's actual design** — decouples "can open the socket" (coarse, filesystem-level) from "who are you" (fine, kernel-verified per-connection). tymuxd's SO_PEERCRED uid check is structurally a `peer`-auth clone. Confirms the requirement's framing ("never from a client-supplied claim") is the industry-standard approach, not a novel invention.
| **ssh-agent** | `$TMPDIR/ssh-XXXXXXXXXX/agent.<ppid>` (older), or `~/.ssh/agent.<...>` (newer OpenSSH) | mode 0700 via 0700 parent dir, single-user only, no group story at all | none (implicit: filesystem perms are the entire model, single-user by design) | Confirms owner-only 0600/0700-style is the correct *default* posture (matches this feature's success metric of 0600), but ssh-agent has zero prior art for the *group* extension tymuxd needs, since it was never designed to be shared across users.
| **systemd socket activation** | unit-defined (`ListenStream=/run/foo.sock`) | `SocketMode=` (octal, default 0666 — **note: permissive default, must override**), `SocketUser=`/`SocketGroup=` set ownership atomically at socket-creation time before any process opens it ([systemd.socket(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.socket.html)) | delegated to the activated service; not systemd's concern | **Directly informs the TOCTOU question below**: systemd sets mode/ownership as part of the `bind()`+`listen()` syscall sequence it performs itself, before handing the fd to the service — i.e., permissions are never set via a *second* syscall after bind. tymuxd doing its own `bind()` (not socket-activated) must reproduce this atomicity itself (see §3).

## 3. Edge cases and failure modes

- **Daemon restart while a client holds an open UDS connection.** Once a `UnixListener` is
  dropped/rebound, existing accepted `UnixStream` connections are unaffected until the process
  actually exits — same as the existing TCP behavior today (no special handling currently in
  main.rs beyond the graceful-shutdown `serve_with_shutdown` + SIGTERM/Ctrl-C path,
  main.rs:1345-1359). No new *design* burden here beyond what `serve_with_shutdown` already
  gives for the TCP listener — the same shutdown future should gate both listeners. Clients
  should already tolerate a dropped connection (reconnect-on-error) per bearer-token-auth's
  own client error-handling; no new client-side contract needed beyond a clearer error message
  than "raw transport-error dump" per the requirement.

- **Stale socket file after unclean shutdown.** Confirmed via `std::os::unix::net::UnixListener`
  docs and multiple sources: `bind()` on a Unix-socket path that already exists as a file
  **fails with `AddrInUse`** (`EADDRINUSE`), *regardless of whether a live listener is actually
  behind it*. The standard pattern (used by nginx, various Rust daemons, and documented on
  [users.rust-lang.org](https://users.rust-lang.org/t/how-to-manage-permissions-of-a-unixlistener/31039)):
  1. Attempt `bind()`.
  2. On `AddrInUse`, verify the existing path is actually a socket (`fs::symlink_metadata` +
     `FileTypeExt::is_socket()`), not a symlink or regular file (avoids blindly deleting an
     unrelated file an attacker planted at that path).
  3. Optionally probe liveness by attempting a connect to the existing socket first — if
     connect succeeds, a live daemon is already bound there and startup should fail loudly
     (two tymuxd instances racing for one socket path is a real misconfiguration, not something
     to silently paper over); if connect fails with `ConnectionRefused`, the socket is stale.
  4. `remove_file()` the stale socket, then retry `bind()`.
  This is not automatic in tokio — `tokio::net::UnixListener::bind()` has the same failure mode
  as std's; the remove-stale-then-retry logic must be hand-rolled in tymuxd's startup path,
  analogous to how `check_non_loopback_requires_token` and `sessions_dir` prep already do
  fail-fast startup validation before serving (main.rs:1279-1285 pattern to follow).

- **TOCTOU on `chmod`/`chown` after `bind()`.** Confirmed real: `fchmod`/`fchown` **do not work
  on socket file descriptors** (verified via kernel mailing-list threads
  ([lkml.iu.edu/0505.2](https://lkml.iu.edu/0505.2/0008.html)), so unlike a regular file where
  `fchmod(fd, mode)` closes the TOCTOU window, a Unix socket's permissions can *only* be set by
  path-based `chmod()`/`chown()` calls after `bind()` creates the path — leaving a window
  (however brief) where the file exists at its default-umask permissions before the daemon's
  explicit `chmod(0600)` lands. **The standard mitigation is `umask`, not post-bind `chmod`**:
  set the process (or a scoped per-thread/per-call, if available) umask to `0177` (or
  equivalent) immediately before `bind()` so the socket file is created *already* at 0600 by
  the kernel's own default-permission-minus-umask logic, with no window at all — this mirrors
  exactly how systemd's `SocketMode=`/`SocketUser=`/`SocketGroup=` achieve atomicity (§2 table).
  Rust's `nix` crate or raw `libc::umask()` FFI would be needed since `std`/`tokio` expose no
  umask control directly. This is a concrete implementation detail the planning phase should
  capture as a named risk-mitigation, not left as "chmod after bind."

- **`$XDG_RUNTIME_DIR` missing or unwritable.** Confirmed by the requirements' own Rabbit Holes
  section as an open question; no code in this repo currently reads `XDG_RUNTIME_DIR` (grep
  returned zero hits). Standard fallback chain (used by many Linux daemons, e.g. how `tmux`
  itself picks a socket dir): `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` (nested under a
  `tymuxd`-owned subdirectory, not directly in the shared runtime dir) → if unset/unwritable, fall back
  to a `$TMPDIR`-or-`/tmp`-based path scoped by uid (e.g. `/tmp/tymuxd-<uid>/tymuxd.sock`,
  directory created mode 0700) to avoid multiple users' sockets colliding or being
  world-visible in a shared `/tmp`. This must be a **fail-fast, loud** condition per this
  repo's established error-UX convention (§1) if even the fallback isn't writable — not a
  silent skip of the UDS listener, since "both-by-default" is the success metric.

## 4. Unstated needs beyond the explicit requirements

- **Zero-config auto-discovery vs. explicit `--addr`.** tymux-cli's existing `--addr` flag
  defaults to `http://127.0.0.1:7419` and is used for *any* target, local or remote
  (bearer-token-auth's whole non-loopback flow depends on `--addr` pointing at a real remote
  host). Users will reasonably expect: no flag → UDS to the local default path; `--addr`
  explicitly given → honor exactly what's given (TCP, remote or local) and skip UDS entirely,
  since an explicit `--addr` is an unambiguous signal of intent. This means the default value
  of `--addr` itself likely needs to become sentinel/absent-by-default (not a hardcoded TCP
  URL) so the CLI can distinguish "user didn't ask for anything, use my smart default (UDS)"
  from "user explicitly wants 127.0.0.1:7419 over TCP" — the current `default_value =
  "http://127.0.0.1:7419"` on the clap arg (main.rs:179) does not currently let the CLI tell
  these apart. This is a clap default-value design change, not just new dialing logic, and
  should be flagged to the architecture/plan phase.

- **Multi-daemon-per-host (one tymuxd per OS user is the norm).** The default UDS path must be
  per-uid-scoped by construction, not a single shared path — `$XDG_RUNTIME_DIR` already is
  per-user on systemd-managed Linux (`/run/user/<uid>/`), which makes it a good default *for
  that reason*, not just because it's a conventional runtime-file location. The `/tmp` fallback
  path above must replicate that scoping explicitly (`/tmp/tymuxd-<uid>/...`) since `/tmp`
  itself is shared across all users on the host — an unscoped fallback path would silently
  reintroduce the exact cross-user collision this whole feature exists to close.

- **A client-side discovery/fallback order is an implicit requirement, not just a nice-to-have.**
  "tymux-cli, clients/go, clients/ts all connect over UDS by default when on the same host"
  (success metrics) implies each client needs its own logic to compute the same default UDS
  path independently (matching tymuxd's own path-selection algorithm exactly, including the
  `$XDG_RUNTIME_DIR`-then-fallback chain) — any divergence between the daemon's path-selection
  code and each of 3 client implementations' path-selection code is a silent connectivity bug.
  Planning should consider whether the *path algorithm* itself needs to be a single documented
  spec (e.g. in a shared doc or proto comment) all 4 implementations independently follow, since
  there's no shared library between Rust/Go/TS to enforce it in code.

- **What "reachable" means for the TCP-deprecation warning.** The requirement says "startup
  deprecation warning when TCP loopback listener is reachable" — today's code already
  distinguishes loopback vs. non-loopback bind (`is_loopback`, main.rs:1239) and only warns on
  non-loopback. The *new* deprecation warning is orthogonal: it should fire whenever the TCP
  listener exists at all (including loopback, since loopback-TCP is exactly the thing being
  deprecated in favor of UDS) — this is a **new, additive log line**, not a repurposing of the
  existing non-loopback warning at main.rs:1261-1273, and should say so explicitly to avoid the
  two messages being conflated during implementation.
