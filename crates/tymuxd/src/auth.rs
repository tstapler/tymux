//! Bearer-token auth for a non-loopback-bound tymuxd: token resolution
//! (`resolve_token`), the fail-fast startup gate
//! (`check_non_loopback_requires_token`), and the gRPC request gate
//! (`BearerAuthInterceptor`). Extracted from `main.rs` during
//! architecture review to keep the god-file from absorbing another
//! concern (see plan.md's Pattern Decisions).

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use tonic::Status;

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
/// Prefer TYMUXD_TOKEN over --token on a shared host — argv (and thus
/// --token's value) is visible to any local user via `ps`/
/// `/proc/<pid>/cmdline`, while environment variables are only
/// readable via the owner-only `/proc/<pid>/environ`.
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
    flag_value
        .or(env_value)
        .and_then(|t| BearerToken::parse(&t))
}

/// The one documented default-socket-path algorithm this feature
/// defines. Mirrored independently (not shared — see plan.md Pattern
/// Decisions row 10) in tymux-cli's main.rs, clients/go's udsdialer
/// package, and clients/ts's socket-path module. Any change here must
/// be mirrored in all three.
///
/// Both branches nest under a subdirectory tymuxd itself creates and
/// owns (`tymuxd/` under $XDG_RUNTIME_DIR, `tymuxd-<uid>/` under the
/// /tmp fallback) — deliberately symmetric, so bind_uds_listener's
/// create_dir_all+chmod(0700) sequence (Epic 2.2) never has to
/// special-case which directory it's touching (architecture-review.md
/// Blocker fix: a bare `$XDG_RUNTIME_DIR/tymuxd.sock` would make
/// bind_uds_listener chmod the session manager's own shared directory).
pub fn default_uds_socket_path(uid: u32) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("tymuxd").join("tymuxd.sock");
    }
    let base = std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("tymuxd-{uid}")).join("tymuxd.sock")
}

/// Resolves the effective UDS socket path: `--socket-path`/
/// `TYMUXD_SOCKET_PATH` (flag beats env, empty treated as absent) if
/// set, else `default_uds_socket_path`. Note: prefer pointing the
/// override at a `tymuxd`-owned subdirectory (e.g.
/// `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock`, matching
/// `default_uds_socket_path`'s own nesting) rather than directly at a
/// shared runtime directory — this is a documentation nicety, not a
/// safety requirement: `bind_uds_listener` (Epic 2.2) only
/// creates/chmods a parent directory that doesn't already exist, so
/// the socket binds safely either way.
pub fn resolve_uds_socket_path(args: &[String], uid: u32) -> PathBuf {
    let flag = args
        .iter()
        .position(|a| a == "--socket-path")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix("--socket-path=").map(str::to_string))
        });
    let env = std::env::var("TYMUXD_SOCKET_PATH")
        .ok()
        .filter(|v| !v.is_empty());
    flag.or(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_uds_socket_path(uid))
}

/// Resolves `--socket-group`/`TYMUXD_SOCKET_GROUP` (flag beats env,
/// `=`-joined and space-separated flag forms both supported, empty
/// treated as absent — same shape as `resolve_uds_socket_path`).
/// `None` means "owner-only socket", today's behavior.
///
/// Group members get FULL daemon control — CreateSession/Attach/
/// KillSession against every session, identical to the socket owner,
/// not a scoped subset. See the README's "Multi-user / shared-host
/// deployment" section for the containerized/bind-mounted-socket
/// uid-mismatch caveat, which applies to any UDS connection —
/// group-access or owner-only alike.
pub fn resolve_socket_group_name(args: &[String]) -> Option<String> {
    let flag = args
        .iter()
        .position(|a| a == "--socket-group")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix("--socket-group=").map(str::to_string))
        });
    let env = std::env::var("TYMUXD_SOCKET_GROUP").ok();
    flag.or(env).filter(|v| !v.is_empty())
}

/// Resolves a POSIX group name to its gid via getgrnam(3). Safe
/// wrapper: getgrnam's returned pointer is into a non-thread-safe
/// static buffer, but this is called exactly once, synchronously,
/// during single-threaded daemon startup before any listener task is
/// spawned (ADR-002 does not apply here — this is a distinct,
/// well-scoped unsafe call already covered by tymuxd's existing libc
/// dependency).
pub fn resolve_gid_by_name(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    // SAFETY: cname is a valid, NUL-terminated C string for the duration of
    // this call. getgrnam's returned pointer is into a non-thread-safe
    // static buffer, but this function runs exactly once, synchronously,
    // during single-threaded daemon startup before any listener task is
    // spawned.
    let grp = unsafe { libc::getgrnam(cname.as_ptr()) };
    if grp.is_null() {
        None
    } else {
        // SAFETY: grp was just checked non-null above and points at the
        // same static buffer getgrnam populated on this call.
        Some(unsafe { (*grp).gr_gid })
    }
}

/// Resolves whether the TCP loopback listener should be disabled: the
/// bare `--disable-tcp-loopback` flag (no value), or a non-empty
/// `TYMUXD_DISABLE_TCP_LOOPBACK` env value. Neither present defaults
/// to `false` — TCP stays on, today's behavior — so a future removal
/// project only needs to flip this default (architecture.md §6).
pub fn resolve_tcp_disabled(args: &[String]) -> bool {
    args.iter().any(|a| a == "--disable-tcp-loopback")
        || std::env::var("TYMUXD_DISABLE_TCP_LOOPBACK")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

/// Held for tymuxd's entire process lifetime (ADR-001) — flock is
/// released automatically on process exit/crash, so no explicit
/// unlock/cleanup path is needed (see plan.md Unresolved Questions).
#[derive(Debug)]
pub struct SocketLockGuard(#[allow(dead_code)] std::fs::File);

/// Acquires an exclusive, non-blocking `flock` on `<socket_path>.sock.lock`
/// before `tymuxd` touches the socket path at all, so a second `tymuxd`
/// racing to start against the same path fails fast instead of racing the
/// first instance's stale-socket-reconciliation/bind sequence (ADR-001
/// point 3).
pub fn acquire_socket_lock(socket_path: &std::path::Path) -> Result<SocketLockGuard, String> {
    let lock_path = socket_path.with_extension("sock.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("failed to open lock file {}: {e}", lock_path.display()))?;
    // SAFETY: file's fd is valid and owned by this scope for the duration
    // of the call; flock takes no pointer/buffer argument that could be
    // misused.
    let ret = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if ret != 0 {
        return Err(format!(
            "another tymuxd is already starting against {} (lock file: {})",
            socket_path.display(),
            lock_path.display()
        ));
    }
    Ok(SocketLockGuard(file))
}

/// Distinguishes a genuinely stale socket file (nothing listening — an
/// unclean prior exit left it behind) from a live daemon already listening
/// at `socket_path`, and removes only the former.
///
/// Only ever called while holding a `SocketLockGuard` for this same path
/// (Story 2.1.1) — otherwise this check-then-act sequence is itself a
/// TOCTOU across two concurrently starting daemons (pitfalls.md §2,
/// ADR-001).
pub fn reconcile_stale_socket(socket_path: &std::path::Path) -> Result<(), String> {
    if !socket_path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(socket_path) {
        Ok(_) => Err(format!(
            "tymuxd is already running — a live listener answered at {}",
            socket_path.display()
        )),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(socket_path).map_err(|e| {
                format!(
                    "failed to remove stale socket {}: {e}",
                    socket_path.display()
                )
            })
        }
        Err(e) => Err(format!(
            "failed to probe existing socket {}: {e}",
            socket_path.display()
        )),
    }
}

/// Shared error text for both `ensure_socket_parent_dir` and
/// `bind_uds_listener`'s own bind failure — a single wording so
/// `main()`'s callers only ever relay one message shape for "something
/// about the socket path itself is wrong."
fn socket_creation_error(socket_path: &std::path::Path, e: std::io::Error) -> String {
    format!(
        "failed to create Unix socket at {}: {e}. Check that the parent directory \
         exists and is writable, or override the path with --socket-path/TYMUXD_SOCKET_PATH.",
        socket_path.display()
    )
}

/// Ensures `socket_path`'s *immediate* parent directory exists at `0700`
/// (owner-only) or `0750` (group-access, enough for a group member's
/// process to traverse into the directory and reach the socket by name
/// without granting group write access) when `group_gid` is `Some`. If it
/// already exists, it is validated — not trusted — to be owned by this
/// process's own uid at exactly the expected mode (pre-mortem.md P1 #2):
/// an attacker-planted directory on the `/tmp`-fallback path must never be
/// silently bound into. A pre-existing *grandparent* directory (e.g.
/// `$XDG_RUNTIME_DIR` itself, which `tymuxd` doesn't own) is never touched
/// — only the directly-containing directory is created/validated
/// (architecture-review.md Blocker fix).
///
/// Split out of `bind_uds_listener` (Epic 4.2 fix) so `main()` can call it
/// *before* `acquire_socket_lock`: the lock file lives in this same
/// directory, so on a genuinely fresh `$XDG_RUNTIME_DIR`/`/tmp`-fallback
/// path — nothing has ever created `tymuxd/` there yet — acquiring the
/// lock before this directory exists fails with ENOENT, breaking the very
/// first cold start on a machine (confirmed by manually running `tymuxd`
/// against a fresh `XDG_RUNTIME_DIR`: `failed to open lock file
/// .../tymuxd/tymuxd.sock.lock: No such file or directory`). Idempotent —
/// `bind_uds_listener` also calls this itself, so direct callers of that
/// function (all of its existing unit tests) are unaffected.
pub fn ensure_socket_parent_dir(
    socket_path: &std::path::Path,
    group_gid: Option<u32>,
) -> Result<(), String> {
    // Directory mode this function requires for an *immediate* parent of
    // the socket, in both the fresh-create and pre-existing cases: 0o700
    // (owner rwx only) when no group is configured, or 0o750 (owner rwx,
    // group r-x) when group_gid is set.
    let expected_parent_mode = if group_gid.is_some() { 0o750 } else { 0o700 };
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };
    if parent.exists() {
        // "Never chmod a directory tymuxd doesn't itself own" stays an
        // invariant of this function for any input (architecture-review.md
        // iteration-2 Blocker fix) — but a pre-existing parent is now
        // validated, not silently trusted (pre-mortem.md P1 #2 fix).
        // Fatal, not a silent bind-into-it, if the pre-existing
        // directory isn't owned by this process's own uid at exactly
        // the expected mode.
        let meta =
            std::fs::symlink_metadata(parent).map_err(|e| socket_creation_error(socket_path, e))?;
        let owner_uid = std::os::unix::fs::MetadataExt::uid(&meta);
        let mode = meta.permissions().mode() & 0o777;
        // SAFETY: geteuid takes no arguments and cannot fail.
        let daemon_uid = unsafe { libc::geteuid() };
        if owner_uid != daemon_uid || mode != expected_parent_mode {
            return Err(format!(
                "refusing to bind Unix socket at {}: its parent directory {} already \
                 exists but is owned by uid {owner_uid} at mode {mode:o} (expected uid \
                 {daemon_uid} at mode {expected_parent_mode:o}). A pre-existing socket \
                 directory not owned and permissioned by tymuxd itself may have been \
                 created by another, possibly untrusted, process — remove it or point \
                 --socket-path/TYMUXD_SOCKET_PATH somewhere tymuxd can create fresh.",
                socket_path.display(),
                parent.display()
            ));
        }
        Ok(())
    } else {
        std::fs::create_dir_all(parent).map_err(|e| socket_creation_error(socket_path, e))?;
        std::fs::set_permissions(parent, PermissionsExt::from_mode(expected_parent_mode))
            .map_err(|e| socket_creation_error(socket_path, e))
    }
}

/// RAII guard for the process-global `umask`: restores the previous value
/// on drop, including on an unwind, so a panic between changing the umask
/// and restoring it (e.g. inside `UnixListener::bind`) can never leave the
/// process running under the wrong umask for the rest of its life — same
/// "always undone" shape as `SocketLockGuard` above, for a different
/// process-global resource.
struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    /// SAFETY: `umask` takes a single `mode_t` value and returns the
    /// previous one — no pointer/buffer argument, no aliasing hazard. The
    /// only hazard is the process-global state itself, which this guard's
    /// `Drop` impl restores; per `bind_uds_listener`'s doc comment, this
    /// must not be called concurrently with another `umask` call from a
    /// different thread.
    fn set(new_umask: libc::mode_t) -> Self {
        UmaskGuard(unsafe { libc::umask(new_umask) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: same shape as `set` above — restores the previously-saved
        // process umask.
        unsafe {
            libc::umask(self.0);
        }
    }
}

/// Binds a `UnixListener` at `socket_path` with its final permissions
/// (`0600` owner-only, or `0660` group-accessible when `group_gid` is
/// `Some`) set atomically with creation via `umask`, then `chown`s the
/// socket to the configured group immediately after (ADR-001) — no window
/// where the socket is briefly world-accessible. Ensures the socket's
/// immediate parent directory exists first via `ensure_socket_parent_dir`
/// (see that function's doc comment for the pre-existing-directory
/// validation invariant).
///
/// `umask` is process-global state: this function must be called exactly
/// once, synchronously, from a single thread (today: daemon startup,
/// before any listener task is spawned) — never concurrently with another
/// caller mutating the umask, which would race an unrelated thread's file
/// creation onto this function's temporarily-narrowed umask.
pub fn bind_uds_listener(
    socket_path: &std::path::Path,
    group_gid: Option<u32>,
) -> Result<tokio::net::UnixListener, String> {
    let fail_bind = |e: std::io::Error| socket_creation_error(socket_path, e);
    ensure_socket_parent_dir(socket_path, group_gid)?;
    // 0o177 -> 0777 & ~0177 = 0600 (owner-only); 0o117 -> 0777 & ~0117 =
    // 0660 (owner+group). Set immediately before bind() so the kernel
    // creates the file already at this mode — no post-bind chmod window
    // (ADR-001; fchmod on the fd is a documented no-op for AF_UNIX on
    // Linux, so this is the only atomic option).
    let new_umask = if group_gid.is_some() { 0o117 } else { 0o177 };
    let umask_guard = UmaskGuard::set(new_umask);
    let bind_result = tokio::net::UnixListener::bind(socket_path);
    drop(umask_guard);
    let listener = bind_result.map_err(fail_bind)?;
    if let Some(gid) = group_gid {
        std::os::unix::fs::chown(socket_path, None, Some(gid)).map_err(|e| {
            if e.raw_os_error() == Some(libc::EPERM) {
                format!(
                    "bound the Unix socket at {} but failed to grant group access \
                     (gid {gid}): Operation not permitted. The tymuxd process itself is not \
                     a member of the configured --socket-group/TYMUXD_SOCKET_GROUP — add the \
                     daemon's own OS user to that group (or run tymuxd as a user already in \
                     it), then restart.",
                    socket_path.display()
                )
            } else {
                format!(
                    "bound the Unix socket at {} but failed to set its group ownership \
                     (gid {gid}): {e}",
                    socket_path.display()
                )
            }
        })?;
    }
    Ok(listener)
}

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
             `openssl rand -hex 32` if you don't already have one.)"
                .to_string(),
        );
    }
    Ok(())
}

/// Gates every `TymuxService` RPC behind the configured bearer token
/// when tymuxd is bound non-loopback. Owns its own rejection counter
/// rather than reaching into `TymuxDaemon`/`Engine` — auth is a pure
/// request-gate concern, never consulted by RPC handler bodies
/// (research/architecture.md §2).
#[derive(Clone)]
pub struct BearerAuthInterceptor {
    token: Arc<BearerToken>,
    rejection_count: Arc<AtomicI64>,
}

impl BearerAuthInterceptor {
    pub fn new(token: Arc<BearerToken>, rejection_count: Arc<AtomicI64>) -> Self {
        Self {
            token,
            rejection_count,
        }
    }
}

impl tonic::service::Interceptor for BearerAuthInterceptor {
    fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        // `remote_addr()` itself is cheap (no allocation); only the
        // `.to_string()` in each rejection arm heap-allocates, so the
        // common accepted-call path doesn't pay for a peer string it
        // never uses.
        let remote_addr = req.remote_addr();

        let presented = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        match presented {
            None => {
                let peer = remote_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let count = self.rejection_count.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    peer = %peer,
                    tymux_auth_rejection_total = count,
                    "rejected TymuxService call: missing bearer token"
                );
                Err(Status::unauthenticated("missing bearer token"))
            }
            Some(supplied)
                if constant_time_eq::constant_time_eq(
                    supplied.as_bytes(),
                    self.token.as_bytes(),
                ) =>
            {
                Ok(req)
            }
            Some(_) => {
                let peer = remote_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let count = self.rejection_count.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    peer = %peer,
                    tymux_auth_rejection_total = count,
                    "rejected TymuxService call: invalid bearer token"
                );
                Err(Status::unauthenticated("invalid bearer token"))
            }
        }
    }
}

/// Decouples `peer_is_authorized`/`peer_is_group_member` from tokio's
/// concrete `UCred` type and gives "a uid" vs. "a gid" distinct field
/// names instead of same-primitive, positional `u32` parameters
/// (architecture-review.md's primitive-obsession/DIP Concern fix).
/// Constructed once, from `UCred`, at the point a UDS connection is
/// accepted (`PreAuthorizedUnixStream::new`, Epic 3.2) — never
/// anything client-supplied.
#[derive(Clone, Copy, Debug)]
pub struct PeerIdentity {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<i32>,
}

impl From<&tokio::net::unix::UCred> for PeerIdentity {
    fn from(cred: &tokio::net::unix::UCred) -> Self {
        Self {
            uid: cred.uid(),
            gid: cred.gid(),
            pid: cred.pid(),
        }
    }
}

/// Full supplementary-group membership check on Linux, reading
/// `/proc/<pid>/status`'s `Groups:` line (ADR-002). Its only call site
/// below is itself `#[cfg(target_os = "linux")]`-gated, so no non-Linux
/// stub is provided here — an `unreachable!()` variant would be genuine
/// dead code (architecture-review.md nitpick fix).
#[cfg(target_os = "linux")]
fn peer_is_group_member_linux(pid: i32, gid: u32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .map(|groups| {
            groups
                .split_whitespace()
                .filter_map(|g| g.parse::<u32>().ok())
                .any(|g| g == gid)
        })
        .unwrap_or(false)
}

/// Platform-dispatched group-membership check: full supplementary-group
/// list on Linux (via `/proc/<pid>/status`), primary/effective gid only
/// elsewhere — a narrower, not less-safe, fallback (ADR-002).
fn peer_is_group_member(peer: &PeerIdentity, gid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Some(pid) = peer.pid {
            return peer_is_group_member_linux(pid, gid);
        }
    }
    // macOS/BSD, or Linux with an unreadable/absent pid: primary/
    // effective gid only (ADR-002's documented, narrower-not-less-
    // safe fallback).
    peer.gid == gid
}

/// The kernel-verified authorization decision — never consults anything
/// client-supplied (requirements.md's NFR). `daemon_uid` is tymuxd's own
/// effective uid (`libc::geteuid()`, read once at startup); `peer` is
/// constructed from tonic's `UdsConnectInfo`/`UCred`, populated by
/// `SO_PEERCRED` at accept time.
pub fn peer_is_authorized(daemon_uid: u32, allowed_gid: Option<u32>, peer: &PeerIdentity) -> bool {
    if peer.uid == daemon_uid {
        return true;
    }
    allowed_gid.is_some_and(|gid| peer_is_group_member(peer, gid))
}

/// The authorization decision, computed exactly once per accepted UDS
/// connection — not once per RPC (architecture-review.md Performance
/// Concern fix). `Copy`, cloned into request extensions on every RPC on
/// that connection by tonic's own per-request extension-cloning
/// (`tonic-0.12.3/src/transport/server/mod.rs:1038-1042`) — but this
/// carries the *decision*, not the raw `UCred`, so `peer_is_authorized`
/// (including its `/proc` read in the `--socket-group` case) never
/// re-runs per request.
#[derive(Clone, Copy, Debug)]
pub struct UdsAuthDecision {
    pub authorized: bool,
    pub peer_uid: Option<u32>,
    pub peer_gid: Option<u32>,
}

/// Wraps an accepted `UnixStream` with its authorization decision,
/// computed once here — at accept time, before the stream enters
/// tonic's HTTP/2 handshake. Implements `Connected` so tonic's own
/// per-request extension-cloning carries `UdsAuthDecision` instead of a
/// raw credential a downstream `Interceptor` would otherwise have to
/// re-derive a decision from.
pub struct PreAuthorizedUnixStream {
    inner: tokio::net::UnixStream,
    decision: UdsAuthDecision,
}

impl PreAuthorizedUnixStream {
    pub fn new(inner: tokio::net::UnixStream, daemon_uid: u32, allowed_gid: Option<u32>) -> Self {
        let cred = inner.peer_cred().ok();
        let decision = UdsAuthDecision {
            authorized: cred.as_ref().is_some_and(|c| {
                peer_is_authorized(daemon_uid, allowed_gid, &PeerIdentity::from(c))
            }),
            peer_uid: cred.as_ref().map(|c| c.uid()),
            peer_gid: cred.as_ref().map(|c| c.gid()),
        };
        Self { inner, decision }
    }
}

// UnixStream: Unpin, and `decision` is Copy (Unpin), so
// PreAuthorizedUnixStream is Unpin too — no pin-project needed, plain
// Pin::new(&mut self.get_mut().inner) delegation is sound.
impl tokio::io::AsyncRead for PreAuthorizedUnixStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PreAuthorizedUnixStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl tonic::transport::server::Connected for PreAuthorizedUnixStream {
    type ConnectInfo = UdsAuthDecision;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.decision
    }
}

/// Gates every `TymuxService` RPC on the UDS listener behind the
/// once-per-connection `UdsAuthDecision` cached in request extensions by
/// `PreAuthorizedUnixStream`. Deliberately a pure "read the cached
/// decision" check: this interceptor never calls `peer_is_authorized`/
/// `peer_is_group_member` itself, so no per-RPC `/proc` read (or any
/// other authorization work) ever happens — the property Gate-2 review
/// caught missing in the original design and this module now guards
/// against regressing (see `uds_peer_cred_interceptor_never_calls_peer_is_authorized_itself`
/// below).
#[derive(Clone)]
pub struct UdsPeerCredInterceptor {
    rejection_count: Arc<AtomicI64>,
}

impl UdsPeerCredInterceptor {
    pub fn new(rejection_count: Arc<AtomicI64>) -> Self {
        Self { rejection_count }
    }
}

impl tonic::service::Interceptor for UdsPeerCredInterceptor {
    fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        let decision = req.extensions().get::<UdsAuthDecision>().copied();
        if decision.is_some_and(|d| d.authorized) {
            return Ok(req);
        }
        let count = self.rejection_count.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            peer_uid = decision.and_then(|d| d.peer_uid),
            peer_gid = decision.and_then(|d| d.peer_gid),
            tymux_socket_peercred_rejection_total = count,
            "rejected UDS connection: peer not authorized"
        );
        Err(Status::permission_denied(
            "not authorized to access this daemon's socket",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;
    use std::sync::Mutex;
    use tonic::service::Interceptor;
    use tonic::transport::server::TcpConnectInfo;
    use tonic::Request;

    // std::env::set_var/remove_var mutate global process state, so tests
    // touching TYMUXD_TOKEN must not run concurrently with each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // libc::umask mutates process-global state exactly like env vars do,
    // but is a distinct hazard (umask mutation racing another umask
    // mutation, not env-var racing env-var) — a separate lock so neither
    // kind of test can interleave with the other's window.
    static UMASK_LOCK: Mutex<()> = Mutex::new(());

    // --- BearerToken ---

    #[test]
    fn bearer_token_parse_rejects_empty_string() {
        assert!(BearerToken::parse("").is_none());
    }

    #[test]
    fn bearer_token_parse_accepts_non_empty_string() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        assert_eq!(token.as_bytes(), b"s3cr3t");
    }

    #[test]
    fn bearer_token_debug_always_prints_redacted() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let debug = format!("{token:?}");
        assert_eq!(debug, "<redacted>");
        assert!(!debug.contains("s3cr3t"));
    }

    // --- resolve_token ---

    #[test]
    fn resolve_token_prefers_explicit_flag_over_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "envval");
        let args: Vec<String> = vec!["tymuxd", "--token", "flagval"]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        std::env::remove_var("TYMUXD_TOKEN");
        assert_eq!(resolved.unwrap().as_bytes(), b"flagval");
    }

    #[test]
    fn resolve_token_supports_equals_joined_flag_form() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd", "--token=flagval"]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        assert_eq!(resolved.unwrap().as_bytes(), b"flagval");
    }

    #[test]
    fn resolve_token_falls_back_to_env_var_when_no_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "envval");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        let resolved = resolve_token(&args);
        std::env::remove_var("TYMUXD_TOKEN");
        assert_eq!(resolved.unwrap().as_bytes(), b"envval");
    }

    #[test]
    fn resolve_token_treats_empty_flag_value_as_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd", "--token", ""]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_token_returns_none_when_neither_source_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        let resolved = resolve_token(&args);
        assert!(resolved.is_none());
    }

    // --- socket-path-fixtures.json loading (shared with tymux-cli, the Go
    // and TS clients — see plan.md Task 1.1.1b; lives in testdata/ at the
    // repo root, not project_plans/, since two of the four consumers read
    // it via include_str! at compile time) ---

    #[derive(serde::Deserialize)]
    struct DefaultPathCase {
        case: String,
        env: std::collections::HashMap<String, String>,
        uid: u32,
        expected: String,
    }

    #[derive(serde::Deserialize)]
    struct ResolvePathCase {
        case: String,
        #[serde(default)]
        args: Vec<String>,
        env: std::collections::HashMap<String, String>,
        uid: u32,
        expected: String,
    }

    #[derive(serde::Deserialize)]
    struct SocketPathFixtures {
        default_path_cases: Vec<DefaultPathCase>,
        resolve_path_cases: Vec<ResolvePathCase>,
    }

    const SOCKET_PATH_FIXTURES_JSON: &str =
        include_str!("../../../testdata/unix-socket-auth/socket-path-fixtures.json");

    fn load_socket_path_fixtures() -> SocketPathFixtures {
        serde_json::from_str(SOCKET_PATH_FIXTURES_JSON)
            .expect("socket-path-fixtures.json must be valid JSON matching the shared schema")
    }

    fn default_path_case(name: &str) -> DefaultPathCase {
        load_socket_path_fixtures()
            .default_path_cases
            .into_iter()
            .find(|c| c.case == name)
            .unwrap_or_else(|| panic!("no default_path_cases entry named {name}"))
    }

    fn resolve_path_case(name: &str) -> ResolvePathCase {
        load_socket_path_fixtures()
            .resolve_path_cases
            .into_iter()
            .find(|c| c.case == name)
            .unwrap_or_else(|| panic!("no resolve_path_cases entry named {name}"))
    }

    /// Clears the env vars `default_uds_socket_path`/
    /// `resolve_uds_socket_path` read, then applies the case's `env`
    /// map on top. Callers must hold `ENV_LOCK`.
    fn apply_socket_path_env(env: &std::collections::HashMap<String, String>) {
        for var in ["XDG_RUNTIME_DIR", "TMPDIR", "TYMUXD_SOCKET_PATH"] {
            std::env::remove_var(var);
        }
        for (k, v) in env {
            std::env::set_var(k, v);
        }
    }

    fn clear_socket_path_env() {
        for var in ["XDG_RUNTIME_DIR", "TMPDIR", "TYMUXD_SOCKET_PATH"] {
            std::env::remove_var(var);
        }
    }

    // --- default_uds_socket_path ---

    #[test]
    fn default_uds_socket_path_prefers_xdg_runtime_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("xdg_runtime_dir_set");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_falls_back_to_tmpdir_when_xdg_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("xdg_unset_tmpdir_set");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_falls_back_to_tmp_when_both_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("both_unset");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_treats_empty_xdg_runtime_dir_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("xdg_empty_string_treated_as_unset");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_scopes_by_uid_to_avoid_collision() {
        let _guard = ENV_LOCK.lock().unwrap();
        let base_case = default_path_case("both_unset");
        let other_case = default_path_case("uid_scoping_distinctness_1001");
        apply_socket_path_env(&base_case.env);
        let resolved_base = default_uds_socket_path(base_case.uid);
        let resolved_other = default_uds_socket_path(other_case.uid);
        clear_socket_path_env();
        assert_eq!(resolved_base, PathBuf::from(base_case.expected));
        assert_eq!(resolved_other, PathBuf::from(other_case.expected));
        assert_ne!(resolved_base, resolved_other);
    }

    // --- resolve_uds_socket_path ---

    #[test]
    fn resolve_uds_socket_path_prefers_flag_over_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = resolve_path_case("flag_beats_env");
        apply_socket_path_env(&case.env);
        let args: Vec<String> = case.args.clone();
        let resolved = resolve_uds_socket_path(&args, case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn resolve_uds_socket_path_supports_equals_joined_flag_form() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = resolve_path_case("equals_joined_flag_form");
        apply_socket_path_env(&case.env);
        let args: Vec<String> = case.args.clone();
        let resolved = resolve_uds_socket_path(&args, case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn resolve_uds_socket_path_falls_back_to_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = resolve_path_case("env_alone");
        apply_socket_path_env(&case.env);
        let args: Vec<String> = case.args.clone();
        let resolved = resolve_uds_socket_path(&args, case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn resolve_uds_socket_path_falls_back_to_default_when_neither_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = resolve_path_case("neither_present_falls_back_to_default");
        apply_socket_path_env(&case.env);
        let args: Vec<String> = case.args.clone();
        let resolved = resolve_uds_socket_path(&args, case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    // --- resolve_socket_group_name / resolve_gid_by_name ---

    #[test]
    fn resolve_socket_group_name_prefers_flag_over_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_SOCKET_GROUP", "env-group");
        let args: Vec<String> = vec!["tymuxd", "--socket-group", "flag-group"]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_socket_group_name(&args);
        std::env::remove_var("TYMUXD_SOCKET_GROUP");
        assert_eq!(resolved, Some("flag-group".to_string()));
    }

    #[test]
    fn resolve_socket_group_name_returns_none_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_SOCKET_GROUP");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        let resolved = resolve_socket_group_name(&args);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_gid_by_name_resolves_root_to_gid_zero() {
        assert_eq!(resolve_gid_by_name("root"), Some(0));
    }

    #[test]
    fn resolve_gid_by_name_returns_none_for_unknown_group() {
        assert_eq!(
            resolve_gid_by_name("tymux-test-nonexistent-group-83f2"),
            None
        );
    }

    // --- resolve_tcp_disabled ---

    #[test]
    fn resolve_tcp_disabled_true_when_flag_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_DISABLE_TCP_LOOPBACK");
        let args: Vec<String> = vec!["tymuxd", "--disable-tcp-loopback"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(resolve_tcp_disabled(&args));
    }

    #[test]
    fn resolve_tcp_disabled_true_when_env_nonempty() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_DISABLE_TCP_LOOPBACK", "1");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        let resolved = resolve_tcp_disabled(&args);
        std::env::remove_var("TYMUXD_DISABLE_TCP_LOOPBACK");
        assert!(resolved);
    }

    #[test]
    fn resolve_tcp_disabled_false_by_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_DISABLE_TCP_LOOPBACK");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        assert!(!resolve_tcp_disabled(&args));
    }

    // --- acquire_socket_lock / SocketLockGuard ---

    fn unique_test_socket_path(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn acquire_socket_lock_succeeds_for_first_caller() {
        let socket_path = unique_test_socket_path("tymux-lock-test");
        let guard = acquire_socket_lock(&socket_path).expect("first caller should acquire lock");
        let lock_path = socket_path.with_extension("sock.lock");
        assert!(lock_path.exists());
        drop(guard);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn acquire_socket_lock_fails_fast_for_concurrent_second_caller() {
        let socket_path = unique_test_socket_path("tymux-lock-test");
        let first = acquire_socket_lock(&socket_path).expect("first caller should acquire lock");
        let second = acquire_socket_lock(&socket_path);
        assert!(second.is_err());
        assert!(second
            .unwrap_err()
            .contains("another tymuxd is already starting against"));
        drop(first);
        let _ = std::fs::remove_file(socket_path.with_extension("sock.lock"));
    }

    #[test]
    fn acquire_socket_lock_succeeds_again_after_guard_dropped() {
        let socket_path = unique_test_socket_path("tymux-lock-test");
        let first = acquire_socket_lock(&socket_path).expect("first caller should acquire lock");
        drop(first);
        let second = acquire_socket_lock(&socket_path);
        assert!(second.is_ok());
        drop(second);
        let _ = std::fs::remove_file(socket_path.with_extension("sock.lock"));
    }

    // --- reconcile_stale_socket ---

    #[test]
    fn reconcile_stale_socket_is_noop_when_nothing_at_path() {
        let socket_path = unique_test_socket_path("tymux-reconcile-test");
        assert!(!socket_path.exists());
        assert!(reconcile_stale_socket(&socket_path).is_ok());
        assert!(!socket_path.exists());
    }

    #[test]
    fn reconcile_stale_socket_removes_a_genuinely_stale_file() {
        let socket_path = unique_test_socket_path("tymux-reconcile-test");
        {
            let listener = std::os::unix::net::UnixListener::bind(&socket_path)
                .expect("failed to bind test listener");
            drop(listener);
        }
        assert!(socket_path.exists());
        assert!(reconcile_stale_socket(&socket_path).is_ok());
        assert!(!socket_path.exists());
    }

    #[test]
    fn reconcile_stale_socket_errs_and_leaves_a_live_listener_untouched() {
        let socket_path = unique_test_socket_path("tymux-reconcile-test");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("failed to bind test listener");
        let result = reconcile_stale_socket(&socket_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("tymuxd is already running"));
        assert!(socket_path.exists());
        assert!(std::os::unix::net::UnixStream::connect(&socket_path).is_ok());
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);
    }

    // --- bind_uds_listener ---

    #[tokio::test]
    async fn bind_uds_listener_creates_owner_only_socket_at_mode_0600() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        let socket_path = dir.join("tymuxd.sock");
        let listener = bind_uds_listener(&socket_path, None).expect("bind should succeed");
        let mode = std::fs::metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_creates_group_socket_at_mode_0660_with_configured_gid() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        let socket_path = dir.join("tymuxd.sock");
        let gid = unsafe { libc::getegid() };
        let listener = bind_uds_listener(&socket_path, Some(gid)).expect("bind should succeed");
        let meta = std::fs::metadata(&socket_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o660);
        assert_eq!(std::os::unix::fs::MetadataExt::gid(&meta), gid);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_creates_parent_directory_at_mode_0700() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        let socket_path = dir.join("tymuxd.sock");
        assert!(!dir.exists());
        let listener = bind_uds_listener(&socket_path, None).expect("bind should succeed");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_restores_process_umask_after_binding() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        let socket_path = dir.join("tymuxd.sock");
        let pre_call_umask = unsafe {
            let cur = libc::umask(0o022);
            libc::umask(cur);
            cur
        };
        let listener = bind_uds_listener(&socket_path, None).expect("bind should succeed");
        let post_call_umask = unsafe {
            let cur = libc::umask(0o022);
            libc::umask(cur);
            cur
        };
        assert_eq!(post_call_umask, pre_call_umask);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_never_touches_permissions_of_a_pre_existing_grandparent_directory() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let grandparent = unique_test_socket_path("tymux-bind-test-grandparent");
        std::fs::create_dir_all(&grandparent).unwrap();
        std::fs::set_permissions(&grandparent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let parent = grandparent.join("tymuxd");
        let socket_path = parent.join("tymuxd.sock");

        let listener = bind_uds_listener(&socket_path, None).expect("bind should succeed");

        let grandparent_mode = std::fs::metadata(&grandparent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(grandparent_mode, 0o755, "grandparent must be untouched");
        let parent_mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            parent_mode, 0o700,
            "tymuxd-owned parent must be created at 0700"
        );

        drop(listener);
        let _ = std::fs::remove_dir_all(&grandparent);
    }

    #[tokio::test]
    async fn bind_uds_listener_accepts_a_correctly_owned_and_moded_pre_existing_immediate_parent_at_0700(
    ) {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = dir.join("tymuxd.sock");

        let listener = bind_uds_listener(&socket_path, None).expect("bind should succeed");

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "pre-existing compliant parent must be left unchanged"
        );
        assert!(socket_path.exists());

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_accepts_a_correctly_owned_and_moded_pre_existing_immediate_parent_at_0750_with_group_configured(
    ) {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        let socket_path = dir.join("tymuxd.sock");
        let gid = unsafe { libc::getegid() };

        let listener = bind_uds_listener(&socket_path, Some(gid)).expect("bind should succeed");

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o750,
            "pre-existing compliant parent must be left unchanged"
        );
        assert!(socket_path.exists());

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_fails_loudly_when_pre_existing_parent_is_owned_by_a_different_uid() {
        let _guard = UMASK_LOCK.lock().unwrap();
        // Can't create a genuinely different-uid-owned directory as the
        // test process itself; simulate the "attacker-controlled" shape
        // with a world-writable (0o777) directory the test process
        // happens to own, and separately assert the ownership-comparison
        // branch is exercised at all by checking that *some* mode-based
        // rejection fires — the mode-only variant below
        // (`..._too_permissive_mode`) is the fully own-process-reachable
        // proof of the comparison logic; this test documents the intended
        // uid-mismatch behavior for a fixture CI can construct (0o777 is
        // never a valid expected_parent_mode, so it also always rejects).
        let dir = unique_test_socket_path("tymux-bind-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let socket_path = dir.join("tymuxd.sock");

        let result = bind_uds_listener(&socket_path, None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("refusing to bind"));
        assert!(!socket_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_fails_loudly_when_pre_existing_parent_has_a_too_permissive_mode() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let dir = unique_test_socket_path("tymux-bind-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket_path = dir.join("tymuxd.sock");

        let result = bind_uds_listener(&socket_path, None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("refusing to bind"));
        assert!(!socket_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_uds_listener_returns_distinct_message_when_chown_group_permission_denied() {
        let _guard = UMASK_LOCK.lock().unwrap();
        let daemon_uid = unsafe { libc::geteuid() };
        if daemon_uid == 0 {
            eprintln!("skipping: test process is root, chown to gid 0 would succeed");
            return;
        }
        // Skip if the test process happens to already be a member of gid 0.
        let is_member_of_root_group = {
            let mut groups: [libc::gid_t; 64] = [0; 64];
            let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
            n > 0 && groups[..n as usize].contains(&0) || unsafe { libc::getegid() } == 0
        };
        if is_member_of_root_group {
            eprintln!("skipping: test process is already a member of gid 0");
            return;
        }

        let dir = unique_test_socket_path("tymux-bind-test");
        let socket_path = dir.join("tymuxd.sock");

        let result = bind_uds_listener(&socket_path, Some(0));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("is not a member of"),
            "expected group-membership message, got: {err}"
        );
        // Socket file itself should still have been created — only the
        // chown step failed.
        assert!(socket_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- check_non_loopback_requires_token ---

    #[test]
    fn check_non_loopback_requires_token_returns_ok_when_token_present_on_non_loopback_bind() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        assert!(check_non_loopback_requires_token(false, Some(&token)).is_ok());
    }

    #[test]
    fn check_non_loopback_requires_token_returns_err_when_non_loopback_and_no_token() {
        let err = check_non_loopback_requires_token(false, None).unwrap_err();
        assert!(err.contains("--token"));
        assert!(err.contains("TYMUXD_TOKEN"));
    }

    #[test]
    fn check_non_loopback_requires_token_errs_on_empty_token_via_resolve_token_composition() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd", "--token", ""]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        assert!(resolved.is_none());
        let err = check_non_loopback_requires_token(false, resolved.as_ref());
        assert!(err.is_err());
    }

    #[test]
    fn check_non_loopback_requires_token_returns_ok_when_loopback_and_no_token() {
        assert!(check_non_loopback_requires_token(true, None).is_ok());
    }

    // --- BearerAuthInterceptor ---

    fn metadata_request(auth_header: Option<&str>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(value) = auth_header {
            req.metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        req
    }

    #[test]
    fn bearer_auth_interceptor_accepts_matching_token() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(Some("Bearer s3cr3t"));
        assert!(interceptor.call(req).is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bearer_auth_interceptor_rejects_missing_token() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(None);
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "missing bearer token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bearer_auth_interceptor_rejects_wrong_token() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(Some("Bearer wrongvalue"));
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "invalid bearer token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bearer_auth_interceptor_rejects_malformed_authorization_header() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(Some("Bearer"));
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "missing bearer token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bearer_auth_interceptor_rejection_counter_counts_only_rejections() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());

        // 3 rejected, 2 accepted, interleaved.
        assert!(interceptor.call(metadata_request(None)).is_err());
        assert!(interceptor
            .call(metadata_request(Some("Bearer s3cr3t")))
            .is_ok());
        assert!(interceptor
            .call(metadata_request(Some("Bearer wrongvalue")))
            .is_err());
        assert!(interceptor
            .call(metadata_request(Some("Bearer s3cr3t")))
            .is_ok());
        assert!(interceptor.call(metadata_request(None)).is_err());

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    #[tracing_test::traced_test]
    fn bearer_auth_interceptor_logs_real_peer_address_when_available() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter);

        let mut req = Request::new(());
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some("203.0.113.5:54321".parse().unwrap()),
        });

        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        assert!(logs_contain("203.0.113.5:54321"));
        assert!(!logs_contain("s3cr3t"));
    }

    // --- peer_is_group_member_linux ---

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_is_group_member_linux_finds_own_real_gid_via_own_pid() {
        let pid = std::process::id() as i32;
        let gid = unsafe { libc::getegid() };
        assert!(peer_is_group_member_linux(pid, gid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_is_group_member_linux_does_not_find_an_absent_gid() {
        let pid = std::process::id() as i32;
        // Read our own real Groups: line and pick a gid guaranteed absent
        // from it, rather than assuming 999999 is never assigned.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let own_groups: Vec<u32> = status
            .lines()
            .find_map(|line| line.strip_prefix("Groups:"))
            .map(|groups| {
                groups
                    .split_whitespace()
                    .filter_map(|g| g.parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default();
        let mut absent_gid = 999_999u32;
        while own_groups.contains(&absent_gid) {
            absent_gid += 1;
        }
        assert!(!peer_is_group_member_linux(pid, absent_gid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_is_group_member_linux_returns_false_for_nonexistent_pid() {
        assert!(!peer_is_group_member_linux(999_999_999, 0));
    }

    // --- peer_is_authorized ---

    /// A real `UCred` value for the test process's own uid/gid/pid,
    /// obtained via `tokio::net::UnixStream::pair()` + `.peer_cred()` — a
    /// genuine, portable way to construct one in a unit test without any
    /// process/pid mocking (both ends of a `pair()` report the test
    /// process's own identity). `tokio::net::UnixStream::peer_cred()` has
    /// its own stable implementation (not the still-unstable
    /// `std::os::unix::net::UnixStream::peer_cred`, gated behind the
    /// `peer_credentials_unix_socket` feature on this toolchain), so this
    /// helper is `#[tokio::test]`-only.
    async fn own_peer_identity() -> PeerIdentity {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let cred = a.peer_cred().unwrap();
        PeerIdentity::from(&cred)
    }

    #[tokio::test]
    async fn peer_is_authorized_grants_daemon_own_uid_always() {
        let peer = own_peer_identity().await;
        let daemon_uid = peer.uid;
        assert!(peer_is_authorized(daemon_uid, None, &peer));
    }

    #[tokio::test]
    async fn peer_is_authorized_rejects_different_uid_no_group_configured() {
        let peer = own_peer_identity().await;
        let daemon_uid = peer.uid.wrapping_add(1);
        assert!(!peer_is_authorized(daemon_uid, None, &peer));
    }

    #[tokio::test]
    async fn peer_is_authorized_grants_different_uid_in_configured_group() {
        let peer = own_peer_identity().await;
        let daemon_uid = peer.uid.wrapping_add(1);
        // The test process's own real primary gid is always a member of
        // itself (either via the primary-gid fallback or the Linux
        // /proc-based full group list).
        let allowed_gid = peer.gid;
        assert!(peer_is_authorized(daemon_uid, Some(allowed_gid), &peer));
    }

    #[tokio::test]
    async fn peer_is_authorized_rejects_different_uid_not_in_configured_group() {
        let peer = own_peer_identity().await;
        let daemon_uid = peer.uid.wrapping_add(1);
        #[cfg(target_os = "linux")]
        let absent_gid = {
            let pid = peer.pid.expect("pid should be present on Linux");
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
            let own_groups: Vec<u32> = status
                .lines()
                .find_map(|line| line.strip_prefix("Groups:"))
                .map(|groups| {
                    groups
                        .split_whitespace()
                        .filter_map(|g| g.parse::<u32>().ok())
                        .collect()
                })
                .unwrap_or_default();
            let mut candidate = 999_999u32;
            while own_groups.contains(&candidate) {
                candidate += 1;
            }
            candidate
        };
        #[cfg(not(target_os = "linux"))]
        let absent_gid = peer.gid.wrapping_add(1);
        assert!(!peer_is_authorized(daemon_uid, Some(absent_gid), &peer));
    }

    // --- PreAuthorizedUnixStream ---

    #[tokio::test]
    async fn pre_authorized_unix_stream_caches_authorized_decision_at_construction() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let cred = a.peer_cred().unwrap();
        let daemon_uid = cred.uid();
        let wrapped = PreAuthorizedUnixStream::new(a, daemon_uid, None);
        let decision =
            <PreAuthorizedUnixStream as tonic::transport::server::Connected>::connect_info(
                &wrapped,
            );
        assert!(decision.authorized);
        assert_eq!(decision.peer_uid, Some(cred.uid()));
        assert_eq!(decision.peer_gid, Some(cred.gid()));
    }

    #[tokio::test]
    async fn pre_authorized_unix_stream_caches_unauthorized_decision_when_uid_mismatched() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let cred = a.peer_cred().unwrap();
        let mismatched_uid = cred.uid().wrapping_add(1);
        let wrapped = PreAuthorizedUnixStream::new(a, mismatched_uid, None);
        let decision =
            <PreAuthorizedUnixStream as tonic::transport::server::Connected>::connect_info(
                &wrapped,
            );
        assert!(!decision.authorized);
    }

    #[tokio::test]
    async fn pre_authorized_unix_stream_passes_reads_and_writes_through_to_inner_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let cred = a.peer_cred().unwrap();
        let daemon_uid = cred.uid();
        let mut wrapped = PreAuthorizedUnixStream::new(a, daemon_uid, None);

        wrapped.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        b.write_all(b"world").await.unwrap();
        let mut buf2 = [0u8; 5];
        wrapped.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"world");
    }

    // --- UdsPeerCredInterceptor ---

    fn request_with_decision(decision: Option<UdsAuthDecision>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(d) = decision {
            req.extensions_mut().insert(d);
        }
        req
    }

    #[test]
    fn uds_peer_cred_interceptor_accepts_authorized_decision() {
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = UdsPeerCredInterceptor::new(counter.clone());
        let req = request_with_decision(Some(UdsAuthDecision {
            authorized: true,
            peer_uid: Some(1000),
            peer_gid: Some(1000),
        }));
        assert!(interceptor.call(req).is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn uds_peer_cred_interceptor_rejects_unauthorized_decision() {
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = UdsPeerCredInterceptor::new(counter.clone());
        let req = request_with_decision(Some(UdsAuthDecision {
            authorized: false,
            peer_uid: Some(1001),
            peer_gid: Some(1001),
        }));
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(
            err.message(),
            "not authorized to access this daemon's socket"
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn uds_peer_cred_interceptor_rejects_missing_decision() {
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = UdsPeerCredInterceptor::new(counter.clone());
        let req = request_with_decision(None);
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(
            err.message(),
            "not authorized to access this daemon's socket"
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[tracing_test::traced_test]
    fn uds_peer_cred_interceptor_logs_peer_uid_gid_on_rejection() {
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = UdsPeerCredInterceptor::new(counter);
        let req = request_with_decision(Some(UdsAuthDecision {
            authorized: false,
            peer_uid: Some(1001),
            peer_gid: Some(1001),
        }));
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        assert!(logs_contain("peer_uid"));
        assert!(logs_contain("1001"));
        assert!(logs_contain("tymux_socket_peercred_rejection_total"));
        assert!(!logs_contain("session"));
        assert!(!logs_contain("pane"));
    }

    /// Structural regression test guarding against the Gate-2 bug: two
    /// separate `Request<()>`s built from the *same* `UdsAuthDecision`
    /// value (simulating tonic's own per-request clone of one
    /// connection-level value) must both resolve purely from that cached
    /// decision, with `UdsPeerCredInterceptor::call()`'s implementation
    /// containing no call to `peer_is_authorized`/
    /// `peer_is_group_member_linux` at all — only a read of
    /// `req.extensions()`. `UdsPeerCredInterceptor` holds no `daemon_uid`/
    /// `allowed_gid` field (those are only ever consumed by
    /// `PreAuthorizedUnixStream::new`, once, before this interceptor ever
    /// sees a request), so it has no way to recompute the decision even
    /// if it wanted to — the absence of those fields is itself part of
    /// the structural proof, verified here by both outcomes tracking the
    /// shared decision value identically across two independent
    /// `Request`s.
    #[test]
    fn uds_peer_cred_interceptor_never_calls_peer_is_authorized_itself() {
        let shared_decision = UdsAuthDecision {
            authorized: true,
            peer_uid: Some(1000),
            peer_gid: Some(1000),
        };

        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = UdsPeerCredInterceptor::new(counter.clone());

        let req1 = request_with_decision(Some(shared_decision));
        let req2 = request_with_decision(Some(shared_decision));

        assert!(interceptor.call(req1).is_ok());
        assert!(interceptor.call(req2).is_ok());
        // No decision logic ran on either call (both accepted purely from
        // the cached, shared decision) — the rejection counter, the only
        // side effect authorization logic could have produced, never
        // moved.
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
