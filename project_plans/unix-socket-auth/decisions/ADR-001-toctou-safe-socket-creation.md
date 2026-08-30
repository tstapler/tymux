# ADR-001: umask-before-bind + companion lock file for TOCTOU-safe UDS creation

**Status**: Accepted
**Date**: 2026-08-29

## Context

`requirements.md`'s security classification demands the UDS socket file
never be briefly world-accessible between creation and permission-setting.
`research/pitfalls.md` §1 confirms this window is real on Linux: a freshly
`bind()`-ed `AF_UNIX` socket is created at `0777 & ~umask`, not at the
daemon's intended mode, until an explicit follow-up call narrows it — and
`research/pitfalls.md` §1 also confirms `fchmod()`/`fchown()` on the socket's
*file descriptor* are no-ops for `AF_UNIX` on Linux (only path-based
`chmod`/`chown` after `bind()` take effect), ruling out the usual
"fchmod right after bind" fix used for regular files.

Separately, `research/pitfalls.md` §2 confirms a naive
`if path.exists() { unlink(path) }` before `bind()` can steal the socket
from a second, legitimately-running `tymuxd` (operator error, or a
supervisor restarting a new instance before the old one has exited), and
that the standard "probe with `connect()`, unlink only on `ECONNREFUSED`"
sequence is itself a TOCTOU if two `tymuxd` instances start concurrently.

## Decision

1. **`umask`, not post-bind `chmod`, sets the socket's mode bits.**
   Immediately before `UnixListener::bind()`, set the process umask to
   `0o177` (owner-only case) or `0o117` (group-access case, leaving
   `0660`) via `libc::umask()`; restore the previous umask immediately
   after `bind()` returns. This makes the kernel create the file already
   at the intended mode — no window exists, because no second syscall is
   needed for the mode bits. `umask` is process-global, so this sequence
   runs synchronously in `main()` before any other task is spawned (no
   other code path in `tymuxd` creates files concurrently with this).
2. **`chown()` for group ownership remains a necessary second call**
   (umask cannot control group ownership) — bounded by binding into a
   parent directory that is itself already `0700` (either
   `$XDG_RUNTIME_DIR`, set that way by the session manager, or `tymuxd`'s
   own `/tmp`-fallback directory, which this feature creates and chmods
   to `0700` before the socket bind). A process that can't already read
   that directory's listing can't discover the socket to race the
   `chown()` window in the first place.
3. **A companion lock file (`<socket path>.lock`), held for the daemon's
   entire process lifetime via `flock(LOCK_EX | LOCK_NB)`, serializes the
   whole "probe stale socket → unlink → bind" sequence.** A second
   `tymuxd` racing to start against the same socket path fails fast with
   "another tymuxd is already starting against this socket" instead of
   silently racing the first. Only after acquiring this lock does
   `tymuxd` probe an existing socket file with `UnixStream::connect()`:
   success means a live daemon holds the path (abort startup, loud
   error); `ECONNREFUSED`/`NotFound` means stale (safe to `remove_file`
   and retry `bind()`).

## Alternatives Rejected

- **`chmod()`/`chown()` immediately after `bind()`, no umask change.**
  Rejected: this is the exact pattern `research/pitfalls.md` §1 names as
  exploitable — the socket exists at the umask-default mode for the
  gap between `bind()` returning and the `chmod()` call landing, however
  short.
- **Bind-to-temp-name-then-`rename()`.** Considered (closes the window at
  the *final* path, since `rename()` is atomic) but rejected as the
  primary mechanism: it still leaves a `bind()`-to-`chmod()` window at the
  *temporary* path, just with a smaller (guess-the-random-suffix) attack
  surface rather than a zero window — no better than umask-before-bind,
  which has a genuinely zero window, and adds a second file-lifecycle path
  (temp file cleanup on every failure mode) for no additional safety.
- **No lock file — rely on `bind()`'s own `EADDRINUSE` to reject a second
  daemon.** Rejected: `bind()` returns `EADDRINUSE` for *any* existing
  path, live or stale, so without a prior stale/live probe this would
  make `tymuxd` refuse to start after its own unclean shutdown every
  time — exactly the stale-socket problem this feature must solve, not
  reintroduce.

## Consequences

- New pure/OS-boundary functions in `crates/tymuxd/src/auth.rs`:
  `acquire_socket_lock`, `reconcile_stale_socket`, `bind_uds_listener`.
- `libc = { workspace = true }` is already a `tymuxd` dependency (used
  elsewhere in this crate already) — `libc::umask()` needs no new crate.
- The lock file (`<socket_path>.lock`) is a new on-disk artifact,
  documented in the plan's Observability/Deployment notes so an operator
  who finds it during manual cleanup understands its purpose.
