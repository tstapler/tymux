# Pitfalls Research: unix-socket-auth

Research for `project_plans/unix-socket-auth/requirements.md`. Focus: UDS bind/permission
races, stale-socket handling, `SO_PEERCRED` semantics and edge cases, container/namespace
uid mapping, group-access footguns, and testing pitfalls. Cross-referenced against
`project_plans/bearer-token-auth/research/pitfalls.md` (sibling feature) per instructions —
not re-deriving its findings, only flagging where this feature reintroduces a similar shape
of risk.

## 0. Repo orientation (confirmed)

- `crates/tymuxd/src/main.rs:1229-1330` — today's `main()` reads `TYMUXD_ADDR`
  (default `127.0.0.1:7419`), resolves the bearer token, then calls
  `Server::builder()....serve_with_shutdown(socket_addr, shutdown_signal())` once.
  There is exactly **one** listener today; this feature adds a second
  (`UnixListener`) served concurrently — the "tonic + UnixListener composition"
  rabbit hole requirements.md already flags.
- `crates/tymux-core/src/persistence.rs:318-330` (`default_sessions_dir`) is the
  existing precedent for "resolve an XDG-ish directory with a documented,
  tested cross-platform fallback chain" (`XDG_STATE_HOME` → `dirs::state_dir()`
  → `dirs::data_local_dir()`), with a regression test
  (`default_sessions_dir_should_honor_xdg_state_home_override_on_every_platform`,
  persistence.rs:351) proving the override path. The UDS default-path
  resolution (`$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` per requirements.md) should follow
  this exact pattern — same `dirs`-crate-plus-explicit-fallback shape, same
  kind of override test — rather than inventing new resolution logic.
- `crates/tymux-core/src/pane.rs:210-221` — the bearer-token feature's fix
  (`cmd.env_remove("TYMUXD_TOKEN")` before `spawn_command`, with a regression
  test at pane.rs:945-953 asserting the var is absent from the child's env) is
  the concrete precedent for "does daemon-process state leak into
  spawned-pane environments." See §5 for whether this feature reintroduces
  that shape of risk — short answer: not for a secret (no token-equivalent is
  introduced), but the *pattern* (grep for anything read via
  `std::env::var`/`clap env=` at daemon startup and verify it isn't silently
  inherited by `CommandBuilder`) is worth re-running for whatever env var(s)
  this feature adds (e.g. a `TYMUXD_SOCKET_GROUP` or `TYMUXD_SOCKET_PATH`
  override).
- `crates/tymuxd/Cargo.toml:14` — `tokio = { workspace = true }`; need to
  confirm (implementation-time, not here) that the workspace's tokio feature
  set includes `net` + the Unix-specific credential API (`UnixListener`,
  `UCred`, `peer_cred()`) — not blocking for this research but flag it as a
  Cargo.toml diff the plan should call out explicitly.

## 1. TOCTOU: `bind()`-then-`chmod()` window

- **The window is real and exploitable in principle, not theoretical.** On
  Linux, a newly `bind()`-ed `AF_UNIX` socket is created with mode `0777`
  masked only by the process's current `umask` — *not* mode 0600 — until an
  explicit `chmod()`/`chown()` call narrows it. Between `bind()` returning and
  the daemon's own `chmod(path, 0600)` (or `chown(path, gid)`) call, any local
  process that can `stat()`/enumerate the directory and race the connect can
  open the socket while it's still world- or umask-default-accessible.
  Historically-cited precedent: early OpenSSH agent-forwarding sockets had an
  exploitable race of exactly this shape.
  [set unix socket permissions during binding of the UNIX-socket · Issue #426 · python/asyncio](https://github.com/python/asyncio/issues/426)
- **`fchmod()`/`fchown()` on the socket *file descriptor* does not work on
  Linux for `AF_UNIX`** — the kernel ignores permission/ownership changes
  applied via the fd; only "plain" `chmod()`/`chown()` referencing the
  *pathname* actually change what a subsequent `connect()`/`open()` sees. This
  rules out the naive "un-race it by calling fchmod right after bind, before
  anyone else can open the fd" approach — there is no fd-based shortcut here.
  [unix(7) — Arch manual pages](https://man.archlinux.org/man/unix.7)
- **Two real mitigations, neither individually fully atomic — combine them:**
  1. **Set the process `umask` to `0177` (or `0077` if the group-access
     feature needs it) immediately before `bind()`, restore it immediately
     after.** Since the socket file's permissions are `0777 & ~umask` at
     creation, this makes the socket's *world* bits correct from the instant
     it's created — no window exists for "any other local user can connect"
     specifically, because the restrictive mode is applied atomically as part
     of the `bind()` syscall itself, not as a follow-up call. `umask` is
     process-global (not per-thread-safe if the daemon uses OS threads
     concurrently with the bind), which matters if `bind()` doesn't happen as
     the very first thing on a single-threaded startup path — confirm
     `tymuxd`'s UDS bind happens before any other thread/task that could also
     be creating files is spawned, or wrap the umask set/restore + bind in
     whatever synchronization already exists around startup.
  2. **`chown()` for group-based access is a separate, unavoidable
     follow-up call** (umask only controls the *mode* bits, not group
     ownership) — so the group-access window (§6) still exists even with the
     umask trick. Bound that exposure by binding into a directory that's
     already non-world-accessible (the per-user `$XDG_RUNTIME_DIR`, mode 0700
     by the session manager, is exactly this — don't fall back to a
     world-readable `/tmp`-style path when `$XDG_RUNTIME_DIR` is unset,
     without narrowing the fallback directory's own permissions first).
  - Bind-to-temp-path-then-`rename()` (create the socket at a private
    temp name, `chmod`/`chown` it, then atomically `rename()` into the public
    path) closes the window at the *final* path but not at the temp path
    itself — acceptable if the temp name is unpredictable (e.g.
    `tymuxd.sock.<pid>.tmp`) and lives in the same already-private directory,
    since a race requires both guessing the temp name and winning the
    `bind()`-to-`chmod()` window on it, which is a materially smaller attack
    surface than racing the well-known final path.
  [UNIX Socket Permissions in Linux: Essential Guide for C Server Developers](https://linuxvox.com/blog/unix-socket-permissions-linux/), [Umask — Wikipedia](https://en.wikipedia.org/wiki/Umask)

## 2. Stale socket files vs. a live second daemon

- **Never `unlink()`-then-`bind()` blindly.** If a second `tymuxd` is
  legitimately already running (operator error, or a supervisor restarting
  the new instance before the old one exited), a naive
  `if path.exists() { unlink(path) }` before `bind()` steals the socket out
  from under the live daemon — its clients silently start failing, and the
  new daemon serving the same path may not even hold the same session state.
  [unlink stale unix socket before binding · Issue #425 · python/asyncio](https://github.com/python/asyncio/issues/425)
- **Correct sequence: attempt `connect()` to the existing path first.** On
  Linux and BSD, `connect()` to an `AF_UNIX` path with no live listener fails
  immediately (`ECONNREFUSED`, not a hang, regardless of blocking mode) —
  this is the standard way to distinguish "stale file, safe to remove" from
  "live daemon, do not touch": `connect()` succeeds (or `EINPROGRESS` in
  non-blocking mode racing to a real accept) → live daemon, abort startup
  with a clear "already running" error; `connect()` fails with
  `ECONNREFUSED` (or the file doesn't exist) → stale, safe to `unlink()` and
  re-`bind()`.
  [Behavior of connect() with O_NONBLOCK on a Unix domain socket](https://forums.freebsd.org/threads/behavior-of-connect-with-o_nonblock-on-a-unix-domain-socket.75963/)
- **The check-then-act sequence above is itself a TOCTOU** if two `tymuxd`
  instances start concurrently and both do "connect, get ECONNREFUSED,
  unlink, bind" at the same time — both will observe "stale," both will
  `unlink()`, and whichever `bind()`s second wins, silently orphaning the
  first. The standard fix is a **separate lock file** (e.g.
  `flock()`/advisory lock on a companion `tymuxd.sock.lock`, or a PID file
  checked with `flock(LOCK_EX | LOCK_NB)`) that serializes the entire
  "check-if-stale, unlink, bind" sequence across concurrent daemon starts —
  the socket path itself can't be the lock because `bind()` is exactly the
  operation under contention.
- **This is directly relevant to tymuxd's existing restart-persistence
  design** — `crates/tymuxd/tests/restart_persistence.rs` and the
  `orphan_candidate_count` logic at `main.rs:1279-1307` already handle a
  "daemon crashed, sessions left dead-flagged on disk" scenario for pane
  state; the stale-socket-file case is the same failure mode (unclean daemon
  exit) applied to the listener rather than session records, and the fix
  should live in the same "startup reconciliation" phase of `main()`,
  before the `Server::builder()` call, mirroring how session reconciliation
  already runs before serving (main.rs:1275-1307).

## 3. `SO_PEERCRED` semantics and edge cases

- **Credentials are captured at `connect()`/`socketpair()` time, not
  continuously.** The uid/gid/pid `SO_PEERCRED` (and tokio's
  `UnixStream::peer_cred()`, which wraps it) returns are a snapshot from the
  moment the connection was established — **not** re-checked per byte or per
  RPC. This matches the NFR ("peer-cred check once per connection at
  accept") but has a converse implication worth stating explicitly in the
  design doc: **if a client process calls `setuid()`/drops privileges *after*
  connecting**, the daemon's already-accepted authorization decision does not
  change or get revisited — it was made against the uid the client had at
  `connect()` time, which is actually the *safe* direction (a privilege drop
  after connecting can't be used to gain access it didn't have at connect
  time), but the reverse is impossible to protect against by construction: a
  process that is briefly privileged (setuid-root helper) at `connect()` time
  and drops privileges immediately after would have already gotten whatever
  access its `connect()`-time uid grants, for the lifetime of that one
  connection.
  [Sources: joeshaw/peercred](https://github.com/joeshaw/peercred), [unix(7) man page](https://www.man7.org/linux/man-pages/man7/unix.7.html)
- **`SCM_RIGHTS` fd-passing does *not* re-derive credentials from the new
  holder of the fd.** If a UDS connection's fd is passed to another process
  via `SCM_RIGHTS` (or simply inherited across `fork()`/`exec()` without
  `CLOEXEC`), `peer_cred()` called on that fd still reports whoever was on
  the *other end of the original `connect()`* — not the process that
  currently holds the duplicated fd on the client side. This isn't a way to
  forge credentials (you can't fabricate the daemon's own view of who dialed
  in), but it is a footgun for **client-side** reasoning: a tymux client
  library that hands its connected `UnixStream` fd to a child process (e.g.
  a wrapper script) doesn't get a "fresh" identity check — the daemon's
  original accept-time decision still governs that connection, for better or
  worse. Not a vulnerability in tymuxd's own enforcement, but worth a note if
  any client-side tooling (shell wrappers, `tymux-cli` re-exec patterns) ever
  passes an already-connected socket fd downstream rather than dialing fresh.
- **A relay/proxy between client and daemon changes whose credentials get
  checked — this is the sharpest edge case for tymux specifically.** If a
  connection to the UDS path is proxied through `socat`, `ssh -L
  <local-uds>:<remote-uds>`, or any other forwarder, `peer_cred()` on the
  daemon side reports **the proxy process's** uid/gid/pid, not the original
  remote/originating user's — because as far as the kernel is concerned, the
  proxy *is* the peer that called `connect()`. This matters more for tymux
  than for a typical daemon because tymux is itself in the business of
  terminal/session and process management — an operator might reasonably
  try to reach a remote tymuxd's UDS through an SSH local-port-forward-style
  Unix-socket tunnel (`ssh -L /local/sock:/remote/sock host` isn't literally
  how `ssh -L` works for UDS targets without `-W`/`StreamLocalBindUnlink`
  trickery, but the general pattern — some local relay process fronting the
  real daemon socket — is a realistic deployment shape). If such a proxy runs
  as a different uid than the actual end user (e.g. a system service
  account), every connection through it would be attributed to the proxy's
  uid, either wrongly granting access (proxy runs as a privileged/matching
  uid) or wrongly denying it (proxy runs as an unrelated uid) — worth an
  explicit doc note: **UDS peer-cred auth is only meaningful for direct,
  unproxied local connections; anything relayed through another process
  inherits that process's identity, not the original caller's.** This is a
  design constraint to document, not a bug to fix in this feature (mTLS /
  per-session tokens for the proxied case are explicitly out of scope).

## 4. Container / namespace uid-mapping pitfalls

- **User-namespace uid mapping is honored by `SO_PEERCRED`, which is both
  the safe case and the confusing case.** The kernel resolves `SO_PEERCRED`
  using the *host* uid — namespace-local uids are mapped through
  `/proc/<pid>/uid_map` before being compared, so a containerized client
  that appears as uid 0 (root) *inside* its own user namespace, but is
  actually mapped to host uid 1000, is correctly reported to the daemon as
  host uid 1000 — not 0. This is the *good* outcome (no privilege
  confusion), but it means socket-owner-uid comparisons that naively assume
  "the client's self-reported uid" (e.g. from a `--uid` flag, or from
  reading `/proc/self/uid_map` on the client side and trusting it) would be
  wrong; the daemon must always use the kernel-derived `peer_cred()` value,
  never anything the client asserts about its own identity — which the
  requirements doc's NFR already mandates, but this is the concrete
  mechanism that makes deviating from it dangerous.
  [SO_PEERCRED with User Namespaces — falk-werner/peercred-example](https://github.com/falk-werner/peercred-example)
- **The failure mode to actually worry about is a bind-mounted socket path
  shared into a container where uid mapping is *not* 1:1.** If `tymuxd`
  runs on the host and a client container bind-mounts the host's
  `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` into its own filesystem namespace, and that
  container's user-namespace mapping doesn't map its in-container uid back
  to the same host uid as the daemon expects (e.g. rootless-container uid
  shifting, common in Podman/rootless Docker setups), the connecting
  process's *host* uid — which is what `peer_cred()` actually reports — may
  be an unexpected shifted value (e.g. `100000 + in-container-uid` under a
  typical `/etc/subuid` range) rather than the value a user would intuitively
  expect from `id -u` run inside the container. This isn't a security hole
  (the check still correctly reflects the real host identity — access is
  still deny-by-default for any uid that doesn't match the socket owner or
  configured group), but it's a **support/UX pitfall**: a user running the
  tymux client from inside a rootless container will get a confusing
  "permission denied, uid mismatch" that doesn't match what `id -u` shows
  them inside the container, because the daemon is (correctly) evaluating
  the *shifted* host uid. Worth a doc note under the client's peer-cred
  rejection error message: mention that containerized/namespaced clients see
  a different effective uid than `id -u` reports locally.
- **Group-based access has the same namespace caveat**: a configured "grant
  gid G access" only works if the connecting process's *host* gid maps to G
  — a containerized client whose in-container gid happens to equal G's
  numeric value does not get access unless its *host*-mapped gid is also G;
  numeric gid equality inside a container's namespace is coincidental and
  not something the design should treat as meaningful.

## 5. Reintroducing bearer-token-auth's env-leak shape (per task instructions)

Explicitly checked whether this feature reintroduces the `TYMUXD_TOKEN` →
`portable_pty::CommandBuilder` → pane-environment leak pattern documented in
`project_plans/bearer-token-auth/research/pitfalls.md` §2 (and now fixed at
`crates/tymux-core/src/pane.rs:220`, `cmd.env_remove("TYMUXD_TOKEN")`):

- **No secret-equivalent value is introduced by this feature.** The UDS
  socket path, the configured access-group name/gid, and any peer-cred
  uid/gid/pid values are not credentials — they're either public filesystem
  metadata or already-known-to-the-OS identity facts, not a shared secret an
  attacker could reuse to impersonate someone. So there's no direct analogue
  of "the daemon's own env var must be scrubbed before it reaches a spawned
  pane" for *this* feature's core mechanism.
- **However, the same audit pattern should be re-run, not assumed clean.**
  If the implementation adds a `TYMUXD_SOCKET_PATH` or
  `TYMUXD_SOCKET_GROUP`-style env var (following the existing `TYMUXD_ADDR`
  precedent at `main.rs:1229`), that's harmless to leak into pane
  environments on its own — but grep for it going into `CommandBuilder`
  anyway, on the general principle established by the bearer-token fix: any
  new `std::env::var` read at daemon startup is one more line item for the
  same audit, not something to reason about by exception.
- **A more subtle version of the same shape**: if the client-side error
  message for a peer-cred rejection (Success Metrics: "clear specific error
  on peer-cred rejection") is ever *logged by the daemon* with enough detail
  to be actionable (e.g. `tracing::warn!(rejected_uid, rejected_gid,
  rejected_pid, ...)`), that's fine for an operator's own daemon log, but if
  such details ever flowed into a per-*pane* log or a location another
  local user's session could read (out of scope today — no per-session
  ownership exists per Non-functional/Out-of-Scope — but worth flagging for
  whoever eventually builds scoped tokens/per-session ownership), it would
  leak "who tried to connect and got rejected" across the same trust
  boundary this feature exists to enforce. Not actionable now; worth a
  one-line forward-note in the plan.

## 6. Group-based access footguns — the `docker` group as canonical cautionary tale

- **Docker's own documentation states plainly**: "The `docker` group grants
  privileges equivalent to the root user" — and links to the daemon attack
  surface docs for why. The mechanism is structurally identical to what this
  feature proposes: a Unix socket (`/var/run/docker.sock`) owned by root,
  group-owned by `docker`, mode `0660` — membership in that group is
  sufficient to talk to a daemon that runs as root and will do anything a
  root-equivalent caller asks, with **no finer-grained scoping** — there is
  no "docker group member can only manage their own containers" concept.
  [Docker: Understanding the USER instruction](https://www.docker.com/blog/understanding-the-docker-user-instruction/), [Manage Docker as a non-root user (community writeup summarizing the official docs)](https://gist.github.com/VictorNS69/b296095f6e67bdc6a96192b7c5e04d05)
- **Widely written up as a "gotcha" specifically because it's non-obvious to
  operators**: multiple independent writeups (moby/moby issue #9976 —
  "'docker' group is root equivalent and bypasses policy, audit"; a
  securitum.com privilege-escalation writeup; Chris Foster's
  "Privilege escalation via Docker") all make the same point — people add a
  user to the `docker` group as a "just let this user run `docker` commands
  without `sudo`" convenience, not realizing they've granted unscoped root,
  because group membership *reads* as scoped ("this group can use this
  tool") when it's actually "this group can ask a root-running daemon to do
  literally anything, including mounting the host root filesystem into a
  container and reading/writing anything on it."
  [moby/moby #9976](https://github.com/moby/moby/issues/9976), [Privilege escalation via Docker — Chris Foster](https://fosterelli.co/privilege-escalation-via-docker)
- **tymuxd's group-access design must not repeat this framing mistake.**
  requirements.md's Success Metrics describe "a configurable group grants
  access to specific other local users" — this is exactly the docker-socket
  shape, and tymuxd's daemon-level operations (`CreateSession` = spawn an
  arbitrary command, `Attach`/`CapturePane` = read/write another user's pane
  content, `KillSession` = terminate another user's processes) are *already*
  root-equivalent-for-that-user's-tymux-sessions in the same way
  Docker-socket access is root-equivalent for the host: **any member of the
  configured group gets full control over every session on that daemon,
  not just "their own" or some scoped subset** — there is no per-session
  ownership in this feature's scope (explicitly out of scope per
  requirements.md). The implementation plan/docs should say this in exactly
  those words — "group members have full daemon control, equivalent to the
  socket owner, not scoped per-user access" — rather than let the UX imply
  something narrower than what the group bit actually grants, the same
  documentation gap that made the `docker` group surprising to people for
  years before it became common security-hygiene knowledge.

## 7. Testing pitfalls: "wrong uid gets rejected" in CI

- **You genuinely cannot integration-test a real second OS user's uid without
  either running as root in CI or accepting a narrower test.** Two real
  options, not mutually exclusive:
  1. **If CI already runs as root inside a container** (common for Linux CI
     images), use `setpriv`/`sudo -u`/spawning the client subprocess with
     `Command::uid(<different-uid>)` (Rust's `std::os::unix::process::CommandExt::uid`)
     to actually connect as a genuinely different real uid — this is a true
     end-to-end integration test and should be preferred if the existing CI
     environment (check what `crates/tymux-e2e` and the Go/TS integration
     test harnesses already assume about CI privilege) permits it. This
     mirrors how `crates/tymuxd/tests/daemon_startup.rs` and the sibling
     bearer-token integration tests (`clients/go/integration/integration_test.go`,
     `clients/ts/test/daemon.ts` per the earlier grep) already spawn a real
     daemon subprocess and real client subprocesses — the same harness shape
     extends naturally to "spawn the client subprocess with a different
     uid," it just needs CI to actually be root (or have `CAP_SETUID`).
  2. **A naive substitute that looks like it works but doesn't**: running
     the test client inside `unshare --user` (a fresh user namespace) does
     **not** by itself produce a different *host* uid as seen by
     `peer_cred()` — without a configured `/etc/subuid` mapping, an unshared
     user namespace typically maps the caller's own uid to itself (or to a
     denied/nobody mapping), and even with a real subuid range configured,
     the host-visible uid is whatever the mapping says, not an arbitrary
     "different" uid chosen at test-run time. Do not reach for
     `unshare --user` as a way to fake a different `peer_cred()` uid in CI —
     it's solving a different problem (namespace isolation) and, per §4,
     interacts with peer-cred in ways that are easy to get subtly wrong and
     hard to reason about in a test assertion.
  3. **If CI cannot run as root / can't grant `CAP_SETUID`**, the accept
     path (matching uid, or matching configured group gid, succeeds) is
     fully testable as-is (the test process's own uid always matches
     itself), and the *rejection* path degrades to a **unit test on the
     authorization decision function in isolation** — construct a synthetic
     `UCred`/equivalent struct with a uid that deliberately doesn't match
     the socket's expected owner/group, call the same decision function the
     accept-path code calls, and assert it returns "reject" — this proves
     the comparison logic is correct without proving the OS actually
     delivers correct `peer_cred()` data end-to-end for a truly different
     process. State this distinction explicitly in the test plan: "accept
     path is integration-tested end-to-end; reject-on-uid-mismatch is
     integration-tested end-to-end only if CI has `CAP_SETUID`, otherwise
     unit-tested at the decision-function level, with the OS-level
     `peer_cred()` delivery mechanism itself treated as trusted kernel
     behavior (documented, not independently re-verified by this test
     suite)" — this mirrors this same sibling project's own precedent of
     explicitly separating what's proven by test vs. asserted-and-trusted
     (see bearer-token-auth pitfalls.md §3's treatment of "no known tonic
     bug here... verify empirically" as the standard to hold this feature's
     test plan to as well).
  - Recommend checking, at implementation time, whether the project's actual
    CI runner (GitHub Actions `ubuntu-latest` runners typically execute as a
    non-root `runner` user by default, but inside a container job can be
    configured to run as root) supports option 1 before committing to it in
    the plan — this is a concrete unresolved question the plan phase should
    answer, not assume.

## Summary of sources consulted

- Repo: `crates/tymuxd/src/main.rs` (single-listener startup/env wiring),
  `crates/tymux-core/src/pane.rs` (env-scrub precedent),
  `crates/tymux-core/src/persistence.rs` (XDG-fallback-path precedent),
  `crates/tymuxd/Cargo.toml` (tokio dependency),
  `project_plans/bearer-token-auth/research/pitfalls.md` (sibling feature,
  cross-referenced per task instructions, not re-derived).
- TOCTOU / socket permissions: [python/asyncio #426](https://github.com/python/asyncio/issues/426), [python/asyncio #425](https://github.com/python/asyncio/issues/425), [unix(7) — Arch manual pages](https://man.archlinux.org/man/unix.7), [unix(7) — man7.org](https://www.man7.org/linux/man-pages/man7/unix.7.html), [Umask — Wikipedia](https://en.wikipedia.org/wiki/Umask), [UNIX Socket Permissions in Linux — linuxvox.com](https://linuxvox.com/blog/unix-socket-permissions-linux/), [PostgreSQL unix_socket_permissions docs](https://www.postgresql.org/docs/current/runtime-config-connection.html).
- Stale sockets: [Behavior of connect() with O_NONBLOCK on a Unix domain socket — FreeBSD forums](https://forums.freebsd.org/threads/behavior-of-connect-with-o_nonblock-on-a-unix-domain-socket.75963/), [gavv.net — Reusing UNIX domain socket](https://gavv.net/articles/unix-socket-reuse/).
- `SO_PEERCRED` semantics: [joeshaw/peercred](https://github.com/joeshaw/peercred), [falk-werner/peercred-example](https://github.com/falk-werner/peercred-example), [unix(7) man page](https://www.man7.org/linux/man-pages/man7/unix.7.html).
- tonic/tokio UDS support: [hyperium/tonic #856](https://github.com/hyperium/tonic/issues/856), [hyperium/tonic #365](https://github.com/hyperium/tonic/issues/365) (`UdsConnectInfo` added via PR #861 for the `Connected` trait), [tokio-rs/tokio-uds ucred.rs](https://github.com/tokio-rs/tokio-uds/blob/master/src/ucred.rs) (macOS support via `getpeereid`).
- Docker-group cautionary tale: [Docker: Understanding the USER instruction](https://www.docker.com/blog/understanding-the-docker-user-instruction/), [moby/moby #9976](https://github.com/moby/moby/issues/9976), [Privilege escalation via Docker — Chris Foster](https://fosterelli.co/privilege-escalation-via-docker), [securitum.com writeup](https://www.securitum.com/privilege_escalation_through_docker_group_membership_and_sudo_backdoor.html).
- Docker official docs referenced by search (not independently fetched, cited
  via the writeups above): `docs.docker.com/engine/install/linux-postinstall/#manage-docker-as-a-non-root-user`,
  `docs.docker.com/engine/security/#docker-daemon-attack-surface`.
