# Design: graceful daemon rollout (pane adoption across `tymuxd` replacement)

**Status**: Proposed (research and design only; nothing here is implemented). Revised 2026-11-04: D2 accepted by the owner; tmux prior art added (section 12); phase plan adjusted (section 8).
**Decision record**: [ADR 0008](../../docs/adr/0008-graceful-daemon-handoff.md)
**Base**: `origin/main` at `cdb10c1` (includes #80). stapler-squad references are read-only, at the checkout named in each link-less path below.

Evidence labels: **VERIFIED** = opened the source or ran the command (cited). **INFERRED** = follows from verified facts or from documented OS behavior I did not test here. **UNVERIFIED** = needs a test before the design relies on it.

## 1. Problem

Whenever the `tymuxd` process is replaced (version upgrade, supervisor respawn after a failed health check, a deploy), the new daemon cannot take over the panes the old one hosted:

- `Engine::revive_session` always spawns fresh processes and documents that "`tymuxd` persists no OS PID for a live pane, so there is nothing to reattach to" (`crates/tymux-core/src/engine.rs:896-918`, VERIFIED).
- Startup then logs `restored dead-flagged sessions from disk` (`crates/tymuxd/src/main.rs:2059`) and `possible orphaned processes from prior tymuxd instance` (`main.rs:2067`); the count is an upper bound, not a measured number (`docs/runbooks/orphaned-processes.md`, "approximation").
- Persistence stores no fd and no pid: `PersistedPaneRecord` has `pane_id, command, cwd, rows, cols, exit_code, launch, restart` only (`crates/tymux-core/src/persistence.rs:20-40`), schema version 1 (`persistence.rs:16`).

What actually happens to the old pane children when the daemon exits is not "they keep running": the daemon holds the pty **master** (`pane.rs:1097-1158`), and when the last master fd closes the kernel sends `SIGHUP` to the slave's session. I reproduced this on this machine (section 9, experiment A: child received `SIGHUP` as soon as the image that owned the master replaced itself with CLOEXEC set). So the orphan count has two populations: children that die of SIGHUP (the common case, i.e. the session is lost), and children that ignore SIGHUP and live on with a dead terminal (true orphans). Both are the loss this design removes. (INFERRED split between the two populations; the live count is unmeasured. Note macOS caps ptys at `kern.tty.ptmx_max: 511` on this machine, VERIFIED by `sysctl`, so "584 orphan candidates" cannot all have been live ptys at one time.)

### Who replaces the daemon today (stapler-squad, read only)

| Path | Code | Effect on panes |
|---|---|---|
| Squad shutdown stops a daemon it spawned | `main.go:1038` registers `StopTymuxd`, `session/tymux/supervise.go:434-475` sends `proc.Kill()` (SIGKILL) | all masters close, every pane gets SIGHUP |
| Supervisor `stop` on monitor exit | `main.go:428-429`, `daemon_monitor.go:33-39` | same |
| `ReplaceDaemon` (upgrade) | `session/tymux/version.go:232-253`: refuses while live sessions exist unless `allowInterrupt`, else `stopListener` = SIGTERM then SIGKILL (`version.go:255-269`) | SIGTERM drains the servers and `main` returns (`main.rs:2224-2226`), fds close, panes get SIGHUP |
| Health-failure recovery | `daemon_monitor.go:63-78` calls `EnsureDaemonRunning` after 3 failures | spawns a second daemon; a wedged-but-alive first daemon is not handed anything |
| Linux only: parent death | `supervise.go:238` + `executor/safeexec/safeexec_pdeathsig_linux.go:24` set `Pdeathsig = SIGKILL` on the spawned daemon | daemon dies when the squad thread that spawned it dies |

`ReplaceDaemon`'s own comment, "Live sessions survive in tymuxd's persisted state but each pane's process is respawned on revive" (`version.go:229-231`), understates the loss: the processes die first. (A parallel investigation owns why squad restarts kill sessions today; this table is context, not a claim about which row is the production cause.)

The live daemon on this machine is already detached: `ps` shows pid 38637 under launchd (`ppid 1`) in its own process group, consistent with `detach_controlling_terminal()`'s `setsid()` (`main.rs:1781-1788`). VERIFIED.

## 2. Goal and non-goals

Goal: a **planned** replacement of the daemon binary, requested by the supervisor, in which every live pane keeps the same child pid, the child never observes a terminal event, no output byte is lost, and attached clients reconnect and resume without a snapshot reset.

Non-goals:
- Surviving an **unplanned** daemon death (SIGKILL, crash, OOM). A handoff needs the old image's cooperation. Section 5.7 shows what only approaches 3 and 6 can add.
- Changing pane semantics (ADR 0001 single pane per session, ADR 0007 lifecycle) or the on-disk session record format beyond additive fields.
- Windows (no pty fd model; `spawn.rs` pty path is unix-only, `pane.rs:1109-1141`).

## 3. Facts that constrain every option

| # | Fact | Evidence |
|---|---|---|
| F1 | The pty master has CLOEXEC set by `portable-pty` at `openpty` time. | `portable-pty-0.8.1/src/unix.rs:57-58` (`cloexec(master)`, `cloexec(slave)`), VERIFIED |
| F2 | tymux-core adds three more fds to the same open file description: the writer (`take_writer`), the reader (`try_clone_reader`), and `reply_fd` (`try_clone_to_owned`). All are dups with CLOEXEC (`filedescriptor` uses `F_DUPFD_CLOEXEC`). | `pane.rs:1147-1158`; `filedescriptor-*/src/unix.rs:149`, VERIFIED |
| F3 | One fd per pane is enough to keep the pty alive across exec; the other dups are re-derived by the new image with `dup`. Minimum code change: clear `FD_CLOEXEC` on one fd per pane (e.g. `reply_fd`, which tymux-core already owns as `OwnedFd`) at handoff time only. No change to spawn-time flags, so a normal pane child never inherits the master. | prototype experiment B, section 9 |
| F4 | The pane child is spawned with `posix_spawn(POSIX_SPAWN_SETSID)` (Linux/glibc) or `pre_exec`+`setsid`+`TIOCSCTTY` elsewhere, and `closefrom(3)`s everything above fd 2. Children hold only the slave side. | `spawn.rs:187-203`, `spawn.rs:419-478`, VERIFIED |
| F5 | Children are children of the daemon. Exit codes come from `waitpid(pid)` in `PosixChild`, called from the pane's reader thread after pty EOF. exec keeps the pid and so keeps the parent/child relation; a *different* process cannot `waitpid` them. | `spawn.rs:489-513`; `pane.rs:532`; experiment B (`waitpid(child)` returned 0 = still our child after exec), VERIFIED |
| F6 | Per-pane in-memory state that a resume depends on: vt100 grid + scrollback (5,000 lines default, 50,000 global), `output_seq`, the `ReplayBuffer` (64 MiB global cap), supervision/restart counters, client viewports. | `pane.rs:22-39`, `pane.rs:163-172`, `replay_buffer.rs:29`, `engine.rs:233-260`, VERIFIED |
| F7 | A blocking reader thread per pane sits in `read()` on the master. Output the child writes while no thread reads waits in the kernel pty buffer (child blocks when full); it is not lost. | `pane.rs:455-470`; experiment B (ticks 5..12 consecutive after a 1.5 s unread gap), VERIFIED |
| F8 | Listeners: UDS bound after `flock` on `<sock>.sock.lock` and stale-socket reconcile; TCP `TcpListener::bind(TYMUXD_ADDR)` (default `127.0.0.1:7419`). | `auth.rs:202-227`, `auth.rs:390-427`, `main.rs:1997-2010`, `main.rs:2090`, VERIFIED |
| F9 | Established gRPC connections (HTTP/2 state inside tonic/hyper) cannot be moved between processes. | design constraint, INFERRED from tonic/hyper having no connection-export API |
| F10 | Attach resume: client sends `resume_from_seq` (`proto/tymux/v1/tymux.proto:492`); daemon replays from the in-memory `ReplayBuffer` if retained (`ResumedFromBuffer`), else `GapExceeded` + fresh snapshot; absent token is `no_resume_token_full_attach` (`main.rs:1461-1520`, counters `main.rs:88-260`). | VERIFIED |
| F11 | Version handshake: `GetServerInfo` returns version, protocol_version, capabilities, pid, started_at (`tymux.proto:594-615`); capabilities are the `CAPABILITIES` slice (`crates/tymux-proto/src/lib.rs:22-31`); `PROTOCOL_VERSION = 1` (`lib.rs:9`). Additive RPCs/fields need no protocol bump (ADR 0003 item 2). | VERIFIED |
| F12 | The daemon runs on a multi-threaded tokio runtime (`#[tokio::main]`, `main.rs:1881`). | VERIFIED |

## 4. Options

### Option 1: in-place re-exec handoff (recommended)

The old daemon quiesces, writes its state, clears CLOEXEC on one fd per pane plus both listeners and the lock file, and `execve`s the new binary with the same pid. The new image adopts the fds.

**Why it fits the code**: F5 (same pid keeps parentage and `waitpid`, which approaches 2 and 3 lose), F7 (kernel buffers output during the gap), F8 (the inherited listening socket keeps its backlog, so connections made during the exec window queue instead of being refused; verified by experiment B: a `connect()` made right before exec was `accept()`ed by the new image).

**Protocol** (`PrepareHandoff` RPC, section 6.2):

1. Supervisor calls `PrepareHandoff{binary_path, expected_version?}`. Daemon validates: absolute path, regular file, owned by the daemon uid, not group/world-writable, executable; then runs `<binary> --handoff-preflight` (exits 0 only if it supports `handoff_schema` N and can parse a tiny canned state), with a timeout. A refused or failed preflight leaves the daemon untouched and returns an error (this is the main rollback guard; see 5.3).
2. Daemon enters **FREEZING**: new mutating RPCs (CreateSession, SplitPane, ClosePane, Resize, ...) return `UNAVAILABLE` with a `retry-after` trailer; `Attach` streams are ended with a new terminal event `Reconnect{reason: HANDOFF, retry_after_ms}`; `ListSessions`/`GetServerInfo` keep working until the exec.
3. Each pane reader thread is switched from a blocking `read` to `poll(fd, 50ms)` + a freeze flag (a small change in `start_reader`, `pane.rs:455`). The daemon waits until every reader has acked it is parked **between** chunks. This is the one subtle step: serializing while a reader holds bytes it has read but not yet pushed to the replay buffer would lose them. Bounded wait (default 2 s); on timeout the freeze aborts and the daemon resumes (no exec).
4. Daemon serializes state to an **unlinked temp file** whose fd is inherited (`TYMUXD_HANDOFF_FD=<n>`), not to a named file. Rationale: scrollback and replay data are terminal contents and can contain secrets; an unlinked inherited fd leaves nothing on disk and cannot go stale (a named `handoff.json` that outlives a failed attempt is a footgun for the next start). Size is bounded by the existing budgets: 64 MiB replay (`replay_buffer.rs:29`) + scrollback caps (`pane.rs:39`).
5. Daemon clears CLOEXEC on: one master dup per pane, the stdin pipe write end for `stdin_stream` panes (`PreparedLaunch.stdin_writer`, held as `pane.rs:116`), the TCP listener, the UDS listener, the `.sock.lock` fd (flock lives on the open file description, so the lock is held continuously across exec and a second daemon can never slip in), and the state fd. Everything else stays CLOEXEC and closes at exec.
6. Daemon `execve(new_binary, argv_with_original_flags + ["--adopt-handoff"], env)`. Same pid. argv/env are preserved so `--token`/`TYMUXD_TOKEN`, `TYMUXD_ADDR`, `TYMUXD_SOCKET_PATH` keep working. `TYMUXD_TOKEN` is stripped from pane environments already (`spawn.rs:116`); unchanged.
7. New image: parse manifest header (magic, `handoff_schema`, old version, pane count). For each pane: rebuild `Pane` from the master fd (new reader/writer dups), reconstruct `PosixChild` from the pid (`waitpid` works), restore vt100 state, `output_seq`, replay buffer, supervision state. Wrap inherited listeners with `from_raw_fd` and serve. Then **commit**: write `handoff complete` log, drop the state fd, mark `GetServerInfo.capabilities` + new field `handoff_generation`.
8. Clients reconnect (they were told to) with `resume_from_seq`; the preserved `output_seq` + replay buffer make the outcome `resumed_from_buffer`, not `no_resume_token_full_attach` (F10).

**Screen/scrollback transfer.** The grid lives only in memory (F6). Two stages:
- Stage A (cheap): per pane, store `screen.state_formatted()` (exists in the fork, `crates/vt100/src/screen.rs:347`) plus the scrollback rows via `set_scrollback(n)`/`rows_formatted` (`screen.rs:396`), and replay both into a fresh `Parser` in the new image. `state_formatted` wraps `contents_formatted` + `input_mode_formatted` (`screen.rs:340-347`) and so covers the visible screen, not scrollback (hence the explicit scrollback replay); whether it reproduces the fork's additions (DEC charsets, OSC 8 hyperlinks, underline color, tab stops, scroll region) is **UNVERIFIED** and needs a round-trip property test: `snapshot_from_screen(a) == snapshot_from_screen(restore(a))` over a corpus of recorded sessions (`testdata/`, `crates/tymux-e2e/src/golden.rs` are the natural source).
- Stage B (exact): add serde to the fork's `Screen`/`Grid` (it is a workspace crate, `Cargo.toml:10`) so the transfer is lossless. Only if Stage A's property test finds differences that matter.
`PaneSnapshot` equality is already the observable contract (`pane.rs:237`, `snapshot_from_screen` at `pane.rs:1174`), so the test oracle exists.

**Schema/version compatibility.** The handoff state is a versioned wire format separate from `CURRENT_SCHEMA_VERSION` (disk records). Rules: (a) the manifest carries `handoff_schema: u32`; the new image supports `[min_read, current]`; (b) the preflight (step 1) makes the old image refuse before freezing if the new binary's declared `handoff_schema_read_range` does not include the writer's; (c) forward-only fields are added with `serde(default)` like the existing records (`persistence.rs:27-39`); (d) downgrade handoff is supported only down to the oldest schema the *old* image can still write, enforced by the same preflight. Because the daemon version is a semver independent of `protocol_version` (ADR 0003), the new capability `graceful_handoff` (value includes schema range, e.g. `graceful_handoff.v1`) is what the supervisor gates on, never a version compare. A daemon older than this feature answers `UNIMPLEMENTED` to `PrepareHandoff`, which the supervisor treats as "option 5 only" (section 4.5).

**Exec across a binary replaced on disk.** The old image must not exec via `/proc/self/exe` or `current_exe()`: on Linux, `/proc/self/exe` after the file was replaced resolves to `<path> (deleted)` (squad already special-cases that suffix, `supervise.go:407-428`), and on macOS `current_exe()` is just the original path. Instead the RPC carries the target path explicitly, and the supervisor installs the new binary by `write temp; fsync; rename(temp, final)` (never overwrite in place: ETXTBSY on Linux, and an in-place rewrite of a signed Mach-O invalidates its cached signature on arm64 macOS, INFERRED, documented Apple behavior not retested here). The squad cache path is a fixed `.../Caches/stapler-squad/tymux/darwin_arm64/tymuxd` with no version component (VERIFIED from `ps` output), so the *old* binary is overwritten by the install; that is why step 1 must snapshot the old image for rollback (5.3).

**macOS vs Linux.**
- Both: `execve` preserves pid, fds without CLOEXEC, process-group/session, rlimits (daemon raised RLIMIT_NOFILE at startup, `main.rs:1905`, and re-raises it idempotently), signal *dispositions set to ignore* (Rust's SIGPIPE ignore persists, which `spawn.rs:403-410` already resets for children) and the **signal mask** (must be empty at exec: do the exec from the main thread after unblocking). Threads vanish at exec; this is legal from a multi-threaded process on both OSes (INFERRED from execve(2); prototype is single-threaded, so the multi-threaded case is **UNVERIFIED** and is test T3).
- macOS: pty master fd survival and `waitpid` after exec VERIFIED on this machine (arm64, Darwin 25.6, section 9). launchd semantics: the daemon is already in its own session (`setsid`, `main.rs:1785`) so `launchd` stopping squad's job does not reach it through the job's process group (the `AbandonProcessGroup` key controls whether launchd kills leftover processes in the job's group; `com.stapler-squad.plist` does not set it, VERIFIED by grep, and the live daemon has its own pgid and `ppid 1`). I did not test a launchd stop against a daemon that is still a direct child; **UNVERIFIED**, and independent of this design since Option 1 does not rely on it. If tymuxd is ever given its own launchd job, `AbandonProcessGroup=true` is irrelevant for the daemon (it is the job's main process; exec keeps the pid so launchd sees no exit), but `KeepAlive` must not treat the exec as a crash: it does not, since the pid never exits.
- Linux: same, plus `Pdeathsig` hazard. `PR_SET_PDEATHSIG` is **preserved across exec** except for setuid/setgid binaries (prctl(2) semantics, INFERRED). Squad sets it on the spawned daemon (`supervise.go:238`), so after a handoff the daemon still dies with squad's spawning thread. The design therefore also requires the supervisor change in 6.3 (do not set `Pdeathsig` for the daemon; leave lifetime to an explicit `Shutdown` RPC).
- systemd (if tymuxd runs as a unit): `KillMode=process` (or `none`) is needed so a unit stop does not SIGKILL pane children in the cgroup; the exec itself does not change the cgroup. Not in scope for squad's launchd/Pdeathsig flow, noted for the runbook.

**Failure modes and mitigations**

| Failure | When | Result | Mitigation |
|---|---|---|---|
| New binary missing, wrong arch, not executable | before exec | exec never attempted | step 1 validation + preflight |
| `execve` itself fails | at exec | still old image | old image resumes serving (unfreeze) and returns the errno through a status RPC/log |
| New image cannot parse state or panics before commit | after exec | fds in a dying process = **panes lost** | rollback exec (5.3) |
| New image panics after commit | steady state | same as any daemon crash | out of scope; see 5.7 |
| Reader parked incorrectly, bytes in flight | freeze | lost output bytes | freeze handshake (step 3), test T2 asserts byte-exact stream |
| Pane exits during freeze | freeze..adopt | SIGCHLD pending; exit observed by new reader on EOF + `waitpid` | state records pid + generation; new image `waitpid`s immediately for each adopted pane; exit code preserved because we are still the parent (F5) |
| Client connects during gap | exec window | queued in inherited backlog | verified in experiment B |
| Client mid-RPC at exec | freeze | RPC reset | idempotent RPCs retry; mutating RPCs returned `UNAVAILABLE` earlier in FREEZING, so only in-flight ones at the last instant can fail; callers already handle daemon restarts |
| RLIMIT / fd-number collision | adopt | inherited fd numbers can collide with new image's early fds | new image's `main` runs `adopt` before opening anything, and records the fd numbers in the manifest, validating each with `fcntl(F_GETFD)` and `fstat` + `isatty`/`ptsname` identity |

**Complexity**: large but bounded: ~6 touch points in tymux-core (`Pane::from_adopted`, reader freeze, state (de)serialize, `Engine::export_state/import_state`, `PosixChild::from_pid`, clear-cloexec helper) and ~4 in tymuxd (`PrepareHandoff`, freeze gate interceptor, `--adopt-handoff` startup path, listener `from_raw_fd`). No new process, no new IPC.

**Test strategy**: section 7.

### Option 2: side-by-side daemons with SCM_RIGHTS fd passing (Envoy / systemd style)

New daemon starts next to the old, connects to a control UDS, receives `(pane_id, master_fd, pid)` tuples via `sendmsg(SCM_RIGHTS)` plus serialized state, binds/receives the listener, then the old daemon exits.

- Feasible on macOS and Linux (SCM_RIGHTS works on both; INFERRED, standard).
- Listener handover: pass the listening fd too; `SO_REUSEPORT` would also work for TCP but has different semantics on macOS (BSD `SO_REUSEPORT` allows multiple *binders* but does not load-balance the way Linux does, and the UDS has no `SO_REUSEPORT` at all), so inheriting/passing the fd is the only uniform option. During the overlap both processes accept on the shared socket; correctness needs a drain protocol. The `flock` startup guard (`auth.rs:202`) would have to learn "second daemon is allowed if the first agreed".
- **Disqualifying difference from option 1: the new daemon is not the parent of the pane children** (F5). It can detect exit (Linux `pidfd_open`; macOS `kqueue EVFILT_PROC NOTE_EXIT`) but cannot `waitpid`, so **exit codes are lost**, and a pane exit has to be inferred from pty EOF. When the old daemon exits, children are reparented to launchd/init, which reaps them. `ExitStatus`/restart-policy semantics (`ADR 0007`: ON_FAILURE needs the code) would silently degrade for every adopted pane. A workaround (the old daemon lingers as a reaper) defeats "old exits".
- Also doubles memory during overlap and needs a second complete initialization path that tolerates an existing socket path, which weakens the fail-fast lock invariant the startup code is built around.
- Failure modes: new daemon dies mid-transfer (the old one still holds everything, which is good: rollback is free until the old one exits). That rollback property is the one genuine advantage over option 1.
- Complexity: higher than 1 (control protocol, two-phase commit, exit-code reconstruction), same state-transfer cost.
- Verdict: rejected as the primary mechanism because of exit-code loss; its rollback property is captured in option 1 by the rollback exec (5.3).

### Option 3: per-pane supervisor/shim process (conmon / tmux-server style)

A small process per pane owns the pty master and the child; the daemon talks to shims over a UDS and reconnects after a restart.

- Feasible on both OSes; the only option that also survives an **unplanned** daemon death (SIGKILL, crash, Pdeathsig, OOM).
- Costs: one extra process per pane. On this machine macOS allows 511 ptys, so up to 511 shims; a Rust shim is on the order of 1 to 2 MiB RSS (INFERRED, not measured), and each shim is another thing launchd/squad must never kill. A shim also needs its own parent: if the shim is a child of the daemon, daemon death reparents it to launchd (fine); the *child's* exit code must be written by the shim somewhere durable because the daemon may not be there to hear it.
- It moves the vt100 grid and replay buffer problem to the shim (shim must keep them, or the daemon rebuilds from a persisted ring), so the shim is no longer tiny, which is the property that would let it never need upgrading. The shim protocol becomes a second versioned wire format to maintain forever, and a shim bug can no longer be fixed by a daemon rollout (a shim upgrade is the same problem again, one level down).
- Security: shim UDS must be owner-only (the daemon's UDS already is 0600, `auth.rs:396-403`) and peercred checked; a second authz surface.
- Perf: every attach byte takes an extra hop; the benchmarks in `docs/benchmarks/` would need redoing.
- Verdict: the right answer **only if** surviving unplanned daemon death becomes a requirement. Deferred, not rejected on feasibility. Option 6 below is a lighter way to get most of that.

### Option 4: persist PIDs and re-adopt orphaned children (rejected)

Impossible in the way the goal needs. A pty master is a file descriptor, not a name; once the last master fd is closed the pair is dead, and no syscall reattaches a new master to an existing slave (`ptsname`/`/dev/pts/N` give a way to open another *slave*, not a master; on macOS `/dev/ttysNNN` likewise). The kernel has already sent `SIGHUP` to the foreground process group of the session whose controlling terminal was that pty (experiment A: child got SIGHUP at master close). The survivors are exactly those that ignore SIGHUP, and they are left with a tty that returns `EIO` on read and discards writes (VERIFIED for the master-side `EIO` mapping in `portable-pty` `unix.rs:86-97` comment; slave-side behavior INFERRED). What *can* be done with persisted pids is hygiene (find orphans, kill them safely), which `docs/runbooks/orphaned-processes.md` already covers, and which must guard against pid reuse (`lstart` check, runbook step 4). pid files also add the exact reuse hazard that runbook warns about. Verdict: rejected for adoption; keep the runbook for cleanup.

### Option 5: do nothing + drain (fallback policy)

Refuse to replace the daemon while any pane is live; upgrade on idle.

- Already half-present in squad: `ReplaceDaemon` refuses with `ErrLiveSessionsWouldBeInterrupted` when `countLiveSessions` > 0 (`version.go:232-247`, `version.go:212-229`). Gaps: (a) the check is skipped when `before.Running` is false or `allowInterrupt` is set; (b) it does not cover crash-recovery respawn or the shutdown `StopTymuxd` (`supervise.go:468`), which SIGKILL regardless of live panes; (c) a daemon that is *wedged* with live panes is killed by the monitor after 3 failed checks.
- Cost: near zero; failure mode is "upgrade never happens" on a host that always has a live pane (this one: hundreds of sessions), plus vulnerability rollout delay.
- Verdict: required as the safety fallback for any daemon that does not advertise `graceful_handoff` and for any handoff the preflight refuses. It is a policy, not a mechanism.

### Option 6 (variant, optional hardening): fd-keeper

A tiny, almost-never-upgraded companion process (the systemd `FileDescriptorStore` idea, which launchd has no equivalent for) holds a dup of every pane's master fd and pid, handed over by SCM_RIGHTS when a pane is created. If the daemon dies for any reason, the restarted daemon asks the keeper for the fds. Gains: survival of crashes and SIGKILL (the cases option 1 cannot cover) and a stronger rollback story (the keeper still holds the masters if the post-exec image dies). Loses: exit codes (the keeper is not the parent), the vt100 grid and replay buffer (the new daemon starts blank and forces redraw with `SIGWINCH`), and adds one process plus a protocol. Phase 4, only if crash survival becomes a goal.

## 5.0 Same-version handoff (owner decision 2026-11-05)

Required, not optional: re-exec of the same version picks up config, `RLIMIT_NOFILE` and persisted-state changes without losing panes. `expected_version` may equal the running version; success is `handoff_generation` incremented with the pid unchanged (a version change cannot prove it). Schema compatibility is trivially satisfied (writer range intersects itself). Supervisors must not treat "same pid, same version" as success and refuse a same-version request until the daemon exposes `handoff_generation`. Compatibility matrix addition: running vN handoff-capable to vN (same or rebuilt file): graceful, verified by generation.

## 5. Comparison

| | 1 re-exec | 2 side-by-side | 3 shim/pane | 4 re-adopt pids | 5 drain | 6 keeper |
|---|---|---|---|---|---|---|
| Same child pid, no SIGHUP | yes (VERIFIED) | yes | yes | **impossible** | yes | yes |
| Exit codes preserved | yes (still parent) | **no** | shim must persist | n/a | yes | **no** |
| Screen/scrollback/replay preserved | yes (transfer) | yes (transfer) | in shim | no | n/a | no |
| Connections during window | queued in backlog | overlap complexity | n/a | n/a | n/a | n/a |
| Survives daemon crash / SIGKILL | no | no | **yes** | no | no | **yes** (degraded) |
| Rollback if new binary fails | needs rollback exec (5.3) | free until old exits | free | n/a | n/a | keeper holds fds |
| Extra processes | 0 | 1 transient | N panes | 0 | 0 | 1 |
| New wire formats | handoff state (internal) | control protocol | shim protocol (permanent) | none | none | keeper protocol |
| Relative implementation cost | M-L | L | XL | n/a | S | M |
| macOS + Linux | both (macOS VERIFIED) | both | both | both | both | both |

## 5.1 Recommendation

**Option 1 as the mechanism, option 5 as the gate and fallback, `graceful_handoff` as the discoverable capability.** Rationale: it is the only option that preserves child pid, exit codes and parentage with no extra process, and the prototype shows the core claim holds on macOS arm64. Its hole (unplanned death) is stated, not hidden: Options 3/6 stay as a measured follow-up.

## 5.2 State schema (internal, versioned)

```
HandoffManifest { magic, handoff_schema, writer_version, writer_pid, started_at,
                  listeners: {tcp_fd?, uds_fd?, lock_fd}, panes: [PaneHandoff] , engine: EngineHandoff }
PaneHandoff    { pane_id, master_fd, stdin_writer_fd?, child_pid, generation, output_seq,
                 rows, cols, exited: Option<ExitInfo>, launch, restart/supervision state,
                 screen: ScreenState (Stage A bytes | Stage B serde), replay_chunks: [(seq, bytes)] }
EngineHandoff  { sessions/windows (same shape as PersistedSessionRecord), viewports, next_client_id }
```
Codec: a length-prefixed binary format (e.g. `postcard`/`bincode` with an explicit version field), not JSON: replay data is bytes and up to ~64 MiB. Chosen over reusing `PersistedSessionRecord` because that type is designed to be human-inspectable and "dead-flagged" (ADR-002, `main.rs:2015`).

## 5.3 Rollback

Exec is irreversible, so rollback is built from three layers:

1. **Pre-commit guard (prevents most failures).** `--handoff-preflight` (step 1) in a *child process* of the old daemon proves the new binary starts, parses the canned state of the old writer's schema, and supports it. A new binary that cannot run on this host is rejected while the old daemon is still in charge.
2. **Rollback exec.** Before exec the old image copies/hard-links its own executable to `<state_dir>/handoff/prev/tymuxd` (hard link when same filesystem; on Linux it can instead open `/proc/self/exe` early, but macOS has no equivalent, so a file is needed) and passes `--rollback-exe` in the manifest. If the new image fails *before* its commit point (bad state, version rejects schema, panic in adopt), it `execve`s the previous binary with `--adopt-handoff --attempt=2` and the same inherited fds. The old binary then re-adopts its own state. A manifest `attempt` counter caps this at one rollback so a bad state file cannot loop. This works because the state fd and master fds are still open in the dying image's pid (exec keeps them). A panic that kills the process before it can call `execve` is not covered: wrap adopt in `catch_unwind` and treat `main`-level panic hook as "rollback exec".
3. **Not covered**: a crash *after* commit. That is an ordinary daemon crash (non-goal; option 6).

The supervisor additionally keeps its pre-handoff behavior of verifying `GetServerInfo` afterwards: expected `pid` unchanged, `version` == target, `handoff_generation` incremented, `ListSessions` live-pane count equal to before; mismatch triggers alerting, not a kill.

## 5.4 Attach streams and clients

The streams cannot survive (F9), so the contract is "reconnect fast and resume":
- On FREEZING the daemon sends `AttachEvent.reconnect{reason=HANDOFF, retry_after_ms}` then closes. Clients already implement reconnect with `resume_from_seq` (`proto/tymux/v1/tymux.proto:79-99`; clients in `clients/go`, `clients/ts`); the new event only lets them skip jittered backoff and not count it as a failure toward their reconnect-exhaustion budget.
- Success criterion: the counter `tymux_attach_resume_outcome_total{outcome="resumed_from_buffer"}` rises by the number of re-attached clients and `no_resume_token_full_attach` / `gap_exceeded_fallback` do not. This is also the production health check for a rollout.
- Seq continuity requires carrying `output_seq` and the replay buffer verbatim; otherwise a client's token is "out of range" and degrades to `gap_exceeded_fallback` (ADR-002 in `project_plans/attach-resume-protocol/decisions/`), which is correct but visible as a screen reset.
- Squad must not treat the handoff reconnect as a pane exit (its reconnect loop and exit callback live in `session/tymux/stream.go`; behavior under `Reconnect{HANDOFF}` is a Phase 5 test, UNVERIFIED today).

## 5.5 Listeners

UDS and TCP listeners are inherited (not rebound). The UDS path stays bound the whole time: a client dialing mid-exec connects to the existing socket inode and sits in the backlog. The startup block that does `acquire_socket_lock -> reconcile_stale_socket -> bind_uds_listener` (`main.rs:1997-2010`) is **skipped** in `--adopt-handoff` mode and replaced by `from_raw_fd` + a `fstat` check that the fd is the expected socket type (and, for TCP, `getsockname == TYMUXD_ADDR`). The lock fd is inherited, so the fail-fast "another tymuxd is starting" guarantee is never released. `SO_REUSEPORT` is not needed.

## 5.6 Observability

New counters/log lines: `tymux_handoff_total{outcome=ok|preflight_failed|freeze_timeout|exec_failed|adopt_failed_rolled_back}`, `tymux_handoff_freeze_seconds`, `tymux_handoff_state_bytes`, `tymux_handoff_generation` gauge, and a startup log `adopted panes count=N from_version=X`. Startup's `restored dead-flagged sessions` / `possible orphaned processes` lines are suppressed in adopt mode (they would be false alarms: `main.rs:2059-2067`).

## 5.7 What this does not fix

Unplanned death stays lossy. The honest list of today's kill paths that a handoff does not address, all supervisor-side (section 6.3): `StopTymuxd` SIGKILL on squad shutdown, `Pdeathsig` on Linux, monitor-triggered respawn of a wedged daemon, `stopListener` SIGTERM. Until those are changed, a successful handoff can still be undone by the next squad restart.

## 6. Interface changes

### 6.1 tymuxd

- New flags: `--adopt-handoff`, `--handoff-preflight`, `--attempt=N` (internal; hidden from `help_text()` except preflight).
- `TYMUXD_HANDOFF_FD` env var carries the state fd.
- `Pane::from_adopted(master: OwnedFd, pid, state)`, `PosixChild::from_pid(pid)`, `Engine::{export_handoff,import_handoff}`, clear-CLOEXEC helper. The reader freeze (`start_reader`).

### 6.2 Proto (additive, `buf breaking` clean, no `PROTOCOL_VERSION` bump per ADR 0003 item 2)

```proto
rpc PrepareHandoff(PrepareHandoffRequest) returns (PrepareHandoffResponse);

message PrepareHandoffRequest {
  string binary_path = 1;          // absolute; installed by rename, never overwritten in place
  string expected_version = 2;     // optional sanity check against `<binary> --version`
  uint32 freeze_timeout_ms = 3;    // default 2000
  bool dry_run = 4;                // run validation + preflight only, change nothing
}
message PrepareHandoffResponse {
  enum Phase { REFUSED = 0; PREFLIGHT_OK = 1; EXEC_IMMINENT = 2; }
  Phase phase = 1;
  string reason = 2;               // set when REFUSED
  uint32 live_panes = 3;
  uint32 handoff_schema = 4;
}
// AttachEvent gets: Reconnect reconnect = N { Reason reason; uint32 retry_after_ms; }
// ServerInfo gets:  uint32 handoff_generation = 9;  string handoff_schema_range = 10;
// CAPABILITIES gets: "graceful_handoff"
```
The RPC returns `EXEC_IMMINENT` **before** the exec (the response must be flushed first), so the supervisor then polls `GetServerInfo` until `pid` is unchanged and `handoff_generation` has incremented, with a deadline.

### 6.3 stapler-squad supervisor (read-only analysis; changes belong in that repo)

1. `ReplaceDaemon` (`version.go:232`): if `GetServerInfo.capabilities` has `graceful_handoff`, install the new binary by rename, call `PrepareHandoff{dry_run:true}` then `PrepareHandoff`, poll for the new generation; **never** fall through to `stopListener`/`ensureDaemonRunning` on failure while `countLiveSessions` > 0. On a daemon without the capability: fall back to option 5 (refuse), keep `allowInterrupt` as an explicit operator override only.
2. `StopTymuxd` (`supervise.go:434-475`) and the registered stop hooks (`main.go:428`, `main.go:1038`): require a successful `countLiveSessions == 0` (or explicit force) before `proc.Kill()`; otherwise leave the daemon running. Prefer SIGTERM-with-drain only when idle.
3. `startDaemonAttempt` (`supervise.go:238`): drop `EnsurePdeathsig` for the daemon on Linux (or make it opt-in), since it survives exec and defeats pane survival on every squad restart.
4. `daemon_monitor.go:63-78`: a *wedged* daemon with live panes must not be killed and respawned; escalate to `PrepareHandoff` if reachable, else alert. Keep respawn only for an unreachable daemon with zero live panes.
5. `binary_embedded.go`: extract embedded binary via temp + `rename` (path stays fixed, inode changes), keeping the previous inode around until the handoff succeeds.
6. `validateTymuxdProcess` (`supervise.go:407`) compares the executable path to the configured one; after handoff the pid is the same but the exe may be a new inode at the same path: it already uses `os.SameFile`, fine.
7. `ensureDaemonRunning` (`supervise.go:358`): do not `stopTymuxdFn()` because one health probe failed; require positive evidence of death (pid gone, or connect refused across a window) and treat a connect that succeeds but times out as "busy" (wait, alert, never spawn or kill). Added by the tmux-parity review, section 12.5 and requirement R3.

## 7. Test plan

All in this repo unless noted; each is deterministic (no real sleeps beyond protocol timeouts, in the spirit of the squad's `deterministic-fast-tests` convention).

| ID | Layer | Test | Asserts |
|---|---|---|---|
| T1 | tymux-core unit | `Pane::from_adopted` over a real pty + `PosixChild::from_pid` | adopted pane's reader sees subsequent output; exit code from `waitpid` equals the child's |
| T2 | tymux-core | freeze/park handshake with a child emitting numbered lines at high rate, snapshot-export mid-stream, import into a fresh pane | byte-exact, gapless, duplicate-free sequence across the boundary (sequence numbers in the lines) |
| T3 | tymuxd integration (`crates/tymuxd/tests/`, new `handoff.rs`) | start daemon A, `CreateSession` running a counter process, `PrepareHandoff` to a *second build* of tymuxd (same binary copied by rename for the same-version case; a differently-versioned test binary for the cross-version case), poll `GetServerInfo` | `pid` unchanged, `handoff_generation == 1`; pane child pid unchanged (`GetPaneStatus.pid`/`kill -0`); counter output continuous; `Attach` with the saved `resume_from_seq` returns `resumed_from_buffer`; `PaneSnapshot` before == after |
| T4 | tymuxd integration | N=50 panes, mixed alt-screen/scrollback/hyperlink content | `PaneSnapshot` equality for all (the Stage A vs B oracle, section 4 Option 1) |
| T5 | tymuxd integration | new binary that exits 1 in `--handoff-preflight` | `REFUSED`, daemon keeps serving, no freeze, panes untouched |
| T6 | tymuxd integration | new binary that passes preflight but panics in adopt (test hook env) | rollback exec: old binary back, same pids, `attempt==2`, `adopt_failed_rolled_back` counter |
| T7 | tymuxd integration | pane exits (nonzero) during freeze | exit code captured, restart policy (ADR 0007 ON_FAILURE) fires after adopt |
| T8 | tymuxd integration | clients dial TCP/UDS repeatedly across the exec | zero `ECONNREFUSED`; connections made in the window are served after |
| T9 | tymuxd integration | schema mismatch matrix (new reads old-1, old-1 rejected by new) | preflight refuses, never reaches exec |
| T10 | tymuxd integration | stdin_stream pane + stderr_path pane | pipe write end still works after handoff; file redirects preserved (children already have their own copies) |
| T11 | Linux CI (`ubuntu`) + macOS runner | T3 on both | the platform-matrix gate; macOS because experiment B's coverage stops at a single-threaded C program |
| T12 | squad (separate repo) | `ReplaceDaemon` with fake transport lacking/advertising `graceful_handoff` | refuses without capability while live panes exist; never calls `stopListener` when handoff fails with live panes; no `Pdeathsig` on daemon spawn |
| T13 | soak | upgrade/rollback loop (cargo-built v1 -> v2 -> v1) under attach/resize churn, 1000 iterations | no pane pid change, no gap, fd count stable (`ServerInfo.open_fds`, `tymux.proto:613`), RSS stable |

## 8. Phased plan (price-weighted cost classes, named blockers)

Cost classes: **S** a single focused agent session, mostly cached reads; **M** one worker plus one review wave; **L** two to three coordinated workers plus a review wave; **XL** multi-wave with a soak. Output tokens dominate cost, so test-heavy phases are classed up.

| Phase | Deliverable | Class | Depends on / blockers |
|---|---|---|---|
| 0 | Land this ADR; owner decisions in section 10 | S | D1-D6 decided by the owner 2026-11-04 (all recommendations in 12.7 accepted) |
| 1 | **tmux-parity gate**: supervisor "never kill a daemon with live panes" (squad 6.3 items 1-4 and new item 7, option 5 policy, drop `Pdeathsig`), plus probe-outcome counters (section 12.5, R3). Must land and soak before any handoff phase ships | M (squad repo) | none; independent of tymux code, and the highest value-per-cost item because it stops today's losses while the mechanism is built. tmux gives this guarantee for free (12.2), so it is the floor, not an enhancement |
| 2 | **First thing to build**: tymux-core adopt primitives + T1/T2: clear-CLOEXEC helper, `PosixChild::from_pid`, `Pane::from_adopted`, reader freeze/park, `Pane` export/import (Stage A screen) | L | none; builds on experiment B |
| 3 | tymuxd: `PrepareHandoff`, freeze gate, state fd, `--adopt-handoff`, listener adoption, `graceful_handoff` capability, proto + `buf breaking`, T3-T5, T7-T10; plus `ServerInfo.executable_path` and the skew-error contract (12.4, R5) | L | Phase 2; vt100 round-trip result (decides Stage B) |
| 4 | Rollback exec, preflight, schema matrix, T6, T9 | M | Phase 3 |
| 5 | Squad integration: `ReplaceDaemon` via handoff, content-addressed binary install (12.6), dry-run, verification, client-side skew error (R5), metrics; T12 plus the client-N-versus-daemon-N-1 matrix (R6) | M (squad repo) | Phase 3-4 released as a tymux tag (squad pins `TYMUX_VERSION`) |
| 6 | Cross-platform CI (T11), soak (T13), runbook promoted to `docs/runbooks/` | L | **macOS CI runner availability**, **soak window** (owner chooses length) |
| 7 (optional) | Option 6 keeper, or option 3 shims, only if crash survival is wanted | XL | D3 decided: not required; build only if wanted later |

Bootstrapping note: the first upgrade *to* a handoff-capable daemon is still a lossy replacement (the running daemon has no `PrepareHandoff`). **D2 is accepted (owner, 2026-11-04):** that one rollout is allowed to be lossy. No idle-window gating or bootstrap-specific code is planned; Phase 5 still routes it through the ordinary "daemon lacks `graceful_handoff`" branch (option 5 refuses while panes are live, `allowInterrupt` is the operator's explicit override), and the release note for the first handoff-capable tymux tag says so. Every rollout after it is graceful. Phase 1 (probe-failure parity) is what keeps this one deliberate loss from being joined by accidental ones.

## 9. Prototype feasibility (run on this machine)

Source: `/tmp/hs-proto/handoff.c` (scratch, not committed). Darwin 25.6.0, arm64, `cc` = Apple clang. The program opens a pty with `openpty` and sets CLOEXEC on master and slave exactly as portable-pty does (`unix.rs:57-58`), forks a child that runs `setsid` + `TIOCSCTTY` on the slave and prints `tick N` every 200 ms, reads 5 ticks, then `execv`s itself as "new image" (same pid). It also holds a listening TCP socket and a client connects just before the exec.

Experiment A (control, CLOEXEC left set, today's behavior):
```
OLD image pid=90067 child=90176 master_fd=3 listener_fd=5 port=59012 inherit=0
  [old] tick 0 ... tick 4
OLD: exec()ing new image now (same pid, fds CLOEXEC set)
NEW image pid=90067 (old pid 90067) ; fcntl(master)=EBADF (fd closed by exec)
NEW: child 90176 waitpid=90176 EXITED (killed by SIGHUP from pty master close) ; sighup flag file: CHILD-GOT-SIGHUP
```
Experiment B (CLOEXEC cleared on master and listener):
```
OLD image pid=90739 child=90740 master_fd=3 listener_fd=5 port=59030 inherit=1
  [old] tick 0 ... tick 4
OLD: exec()ing new image now (same pid, fds inherited)
NEW image pid=90739 (old pid 90739) ; fcntl(master)=valid
NEW: kill(child,0)=0 (child alive, still OUR child: waitpid=0)
  [new] tick 5 ... tick 12            <- 1.5 s startup delay, no gap, no duplicate
NEW: read 8 consecutive ticks from inherited master; last=12
NEW: accept() on inherited listener got the queued client: fd=6
```
Established by this run (VERIFIED): (1) the pty master survives `exec` if and only if CLOEXEC is cleared; (2) the child, on the slave side, keeps running and never sees SIGHUP; (3) the new image is still the child's parent (`waitpid` works, so exit codes are preserved); (4) output produced during a 1.5 s unread window is delivered in order with no loss; (5) a client connected during the window is accepted from the inherited listener's backlog; (6) without clearing CLOEXEC the same transition kills the child, so a daemon that execs naively today loses every pane.
Not established (UNVERIFIED): multi-threaded exec under tokio; Linux (no Linux host used; T11 covers it); the vt100 round trip; the fork's `Pane` code paths (the prototype is C, not tymux-core).

## 10. Open questions for the owner

- **D1.** Accept option 1 (re-exec) as the mechanism, with option 5 as the fallback? (Recommended yes.)
- **D2. ACCEPTED (owner, 2026-11-04).** A one-time lossy bootstrap upgrade (first rollout to a handoff-capable daemon) is acceptable; it does not have to wait for an idle window. Recorded in ADR 0008 Consequences and section 8.
- **Decided 2026-11-04:** the owner accepted every recommendation in section 12.7 (D1 yes, D3 no, D4 unlinked fd with the 0600 `--handoff-debug-dump`, D5 no, D6 yes); D2 was accepted earlier. The entries below keep the original question text.
- **D3.** Is surviving **unplanned** daemon death (crash/SIGKILL) a requirement? It decides whether Phase 7 (keeper or shims) exists. Today's evidence says most losses come from supervisor-initiated kills, which Phase 1 removes.
- **D4.** Handoff state in an unlinked inherited fd (recommended; no secrets on disk) versus a named file (debuggable, survives a failed attempt)? A hybrid, a `--handoff-debug-dump` flag writing 0600 for support, is cheap.
- **D5.** Should the daemon become its own launchd job (`com.stapler-squad.tymuxd`, `KeepAlive`, `AbandonProcessGroup=true`) so it is independent of squad restarts, rather than being a child of squad that `setsid`s away? Out of scope for this design, but it interacts with 6.3 item 3.
- **D6.** Reconnect semantics: OK to add `AttachEvent.reconnect` (new oneof arm) to the shared client contract, which `clients/go` and `clients/ts` must learn?

## 11. Unknowns not resolved here

- Whether `state_formatted` + scrollback replay reproduces every fork-specific attribute (Stage A vs B).
- Multi-threaded exec behavior on macOS under a tokio runtime with active I/O drivers (kqueue fds are CLOEXEC and are recreated by the new image; expected fine, untested).
- Real live pane count and total state size on the production host (the 610/584 numbers are session-record counts, not measured live ptys; `~/.local/state/tymux/sessions` is empty on this machine, so I could not size them).
- launchd behavior when squad's job stops while the daemon is a direct, un-`setsid`ed child (not needed by option 1).
- Exact `kern.tty.ptmx_max` interaction at scale (511 here) with per-pane dups across handoff: the new image holds the same open file descriptions, so no new ptys are allocated; INFERRED.

## 12. Prior art: tmux ("model after tmux, but better")

Method. tmux source read at tag `3.8` (commit `7f2a35ad3321f9ba57a1062ca73b1f3ff26aca53`, cloned read-only to `/tmp`; line numbers below are that tag). The installed binary is `tmux 3.7b` and stapler-squad pins 3.8 (`Makefile` `TMUX_PIN := 3.8`, `docs/how-to/bundle-tmux.md`). `git diff 3.7b 3.8` over `server.c`, `proc.c`, `client.c`, `cmd-kill-server.c` shows only unrelated edits (jemalloc logging, peer gid, imsg API rename, editor/wait hooks), so the lifecycle code cited here is the same in both (VERIFIED by reading that diff). Experiments ran on 3.7b with private sockets named `ssq-prior-art-<pid>` and were removed with `kill-server` afterwards (no leftover server or socket, checked). Scripts were scratch files in `/tmp/pa-exp`, not committed. stapler-squad and the user's real tmux were only read.

### 12.1 What tmux actually does

| Behavior | Evidence | Label |
|---|---|---|
| The server is a forked, `daemon(1, 0)`ed copy of the first client, so it detaches from the launching client, its terminal and its parent. After `new-session -d`: `ppid 1`, own process group (`pgid == pid`), `Ss`. | `proc.c:375-396` (`proc_fork_and_daemon`, `daemon(1, 0)` at `:388`); `server.c:190`. Experiment: `ps -o pid,ppid,pgid,stat` printed `1925 1 1925 Ss`. | VERIFIED |
| Socket is `$TMUX_TMPDIR/tmux-<uid>/<label>` (here `/private/tmp/tmux-501/<label>`, mode `srw-rw----`); created by the server (`unlink`, `bind`, `listen(128)`), with the umask chosen so non-default sockets are owner-only. | `server.c:107-143` (`unlink` at `:121`); `tmux.c` label/dir logic; `ls -l` output. | VERIFIED |
| Single-start is a client-side lock, not a server one: a client whose `connect()` gets `ECONNREFUSED`/`ENOENT` takes `flock` on `<socket>.lock`, retries once, then `unlink`s the socket and starts a server. A stale socket left by a dead server is therefore reaped by the next client that needs one. | `client.c:78-101` (`client_get_lock`), `client.c:104-170` (`unlink` at `:159`, `server_start` at `:164`). Experiment: after `kill-server` and after `kill -9` the socket file was still present and `tmux ls` printed `no server running`. | VERIFIED |
| A wedged server is never replaced or killed by tmux itself. With the server `SIGSTOP`ped, a new client `connect()` succeeds into the kernel backlog and then blocks with no timeout; no second server appeared; after `SIGCONT` the queued command ran. | Experiment (`e4`): client "still blocked after 3s", one server process, both sessions present after `SIGCONT`. | VERIFIED |
| Signals in the server: `SIGINT`/`SIGTERM` set `server_exit` and destroy every session; `SIGCHLD` reaps; `SIGUSR1` re-creates the listening socket (recovery from an unlinked socket); `SIGUSR2` toggles logging; `SIGHUP` is trapped by libevent and has no case, so it is ignored. Experiment: `SIGHUP` to the server left it alive and delivered no signal to the pane; `rm` of the socket then `kill -USR1` made `tmux ls` work again. | `server.c:436-464`, `proc.c:259-273`. | VERIFIED |
| `kill-server` is `kill(getpid(), SIGTERM)`, nothing more. | `cmd-kill-server.c:58`. | VERIFIED |
| The server exits when there are no sessions and `exit-empty` is on (default `1`), and, if `exit-unattached` is on (default `0`), when no client is attached. Experiment: `kill-session` of the last session ended the server; `set -g exit-empty off` kept it alive with zero sessions. | `server.c:268-311`; `options-table.c:401-414`. | VERIFIED |
| When the server exits normally (SIGTERM, `kill-server`) it closes each pane's pty master; the kernel then hangs up the slave's session. A child that traps `SIGHUP` saw exactly one `HUP` for each pane (`hup lines a=1 b=1`). When the server is `SIGKILL`ed the same happens, because the kernel closes the master (`hup lines a=1`; the trapping child kept running as an orphan under pid 1 with the terminal gone). | `server-fn.c:354-372` (`close(wp->fd)`), `server.c:314-331` (`server_send_exit`). Experiments C, D, E1. | VERIFIED |
| tmux has no pane-survives-server-death mechanism. Panes live only as in-memory objects in the server; scrollback is lost with it. | Same code; no persistence code in `server.c`. | VERIFIED (negative: absence in the files read) |
| Exit status: with `remain-on-exit on`, a dead pane stays and exposes `#{pane_dead}`, `#{pane_dead_status}`, `#{pane_dead_signal}`, `#{pane_dead_time}`; `respawn-pane` reuses the same pane with a new pid and clears the status; a `pane-died` hook of `respawn-pane` acts as a restart policy. This lives in server memory only. Experiments printed `dead=1 status=3` (exit 3), `dead=1 signal=int` (SIGINT), then after `respawn-pane` `dead=0 status=[] pid=<new>`; with the hook, a `exit 1` pane came back `dead=0` with a new pid. | `server-fn.c:354-420`, `server.c:491-511` (`wp->status`), `options-table.c:1699-1720`, `format.c:2269-2290`. | VERIFIED |
| No live-upgrade or fd-handoff mechanism. The only `exec*` calls in the source are for pane/job/client shells (`spawn.c:552-574`, `job.c:185-189`, `client.c:507`); the server never re-execs itself and never passes its pty fds anywhere. The official guidance is to restart the server after an upgrade: the 1.9 release notes say "advised that the prior tmux server is restarted when this version of tmux is installed". GitHub issue searches for "upgrade running server" and for re-exec/hot/live upgrade returned no issues. The community workaround is a plugin, `tmux-plugins/tmux-resurrect` (13k stars, "Persists tmux environment across system restarts"), which saves layout and re-launches commands; it does not keep processes. | Source grep; `CHANGES` line 2606 section "CHANGES FROM 1.8 TO 1.9, 20 February 2014"; `gh search issues --repo tmux/tmux` results; `gh api repos/tmux-plugins/tmux-resurrect`. Resurrect's relaunch-not-adopt behavior is INFERRED from its README, not tested. | VERIFIED (source, notes, empty search); INFERRED (resurrect semantics) |
| Platform supervisors are a known hazard: tmux 3.3 added systemd socket activation, 3.4 puts panes in separate cgroups, 3.7 works around systemd killing panes early at shutdown. | `CHANGES` (sections "3.2a TO 3.3" line 1102, "3.3a TO 3.4" line 974, "3.6b TO 3.7" line 422). | VERIFIED |

### 12.2 Client/server version skew in tmux

- The version is in-band and per message: every imsg header carries `PROTOCOL_VERSION` (`proc.c:177`); the receiver compares it on each message and replies `MSG_VERSION` (`peer_check_version`, `proc.c:131-146`), and the client prints `protocol version mismatch (client N, server M)` (`client.c:669-673`). The value is 8 bits (`hdr.peerid & 0xff`). (VERIFIED)
- It is bumped rarely and not for every wire change. `PROTOCOL_VERSION` is `8` at `3.3` and `3.8` (read from the tags); the 1.9 notes record a bump in 2014. Between `3.7b` and `3.8`, `tmux-protocol.h` gained `MSG_WRITE_DONE` and `struct msg_write_done` and the constant stayed `8`, despite the header's own "Don't forget to bump PROTOCOL_VERSION if any of these change!" (`tmux-protocol.h:76`). I did not bisect older tags, only `3.3` and `3.8` plus `3.7b`. (VERIFIED for those tags)
- The result is that the version check is coarse. The maintainer's own words on [tmux/tmux#4890](https://github.com/tmux/tmux/issues/4890) ("Improve error on client/server version mismatch"): the error is shown when the protocol version is increased, "but in this case it wouldn't have done any good because the underlying protocol changed outside of tmux". The reported symptom was the cryptic `open terminal failed: not a terminal`, and the workaround is `/proc/<server pid>/exe attach`, i.e. run the client from the old server's own binary. Issue [#5822](https://github.com/tmux/tmux/issues/5822) (open) shows the mismatch text itself appearing as `client 8, server 8`. (VERIFIED by reading the issues; the cause of #5822 is unknown to me.)
- The same skew class hit stapler-squad twice, per its own code comments (read, not re-run): a tmux 3.6a control-mode client against a kept-alive 3.4 server "never exchanges a single %begin/%end", every control-mode command timing out for over 24h (BUG-101, `session/tmux/version_check.go:49-70`); and a pinned 3.4 client against a 20-day-old 3.7b server failing ordinary commands with `server exited unexpectedly` (`session/tmux/binary_resolution.go:27-37`, `CLIENT_EXIT_LOST_SERVER` at `client.c:210-211`). Squad's institutional fix is the same `/proc/<pid>/exe` idea: `ResolveClientForSocket` finds the process listening on the socket and runs that server's own binary as the client (`binary_resolution.go:52`), `extractBinary` never overwrites a tmux binary in place and names it by content hash because an older kept-alive server may still be running from it (`session/tmux/binary_extract.go:11-14`), and `checkControlModeVersionMatchOnce` is the backstop. (VERIFIED by reading; the macOS signed-binary claim in that comment is not re-tested.)
- Not tested: a real mismatch. The only binaries on this machine are 3.7b (system) and 3.8 (squad `bin/tmux`), both protocol 8, so there is no mismatched pair. I tried to build a 3.8 tree with the constant changed to 9; the sandbox refused to build downloaded source, so the exact error and hang behavior of a real mismatch is **UNVERIFIED** and rests on the source and issues above.

Takeaway for tymux: tmux's skew story is "old server keeps its old binary; use the matching client". tymux cannot copy that, because the client is a gRPC library inside squad, not a binary that can be swapped. What tymux already has beyond tmux (ADR 0003): `GetServerInfo` with `protocol_version`, `min_client_protocol_version` and `capabilities`, so a client can detect skew without talking in the dark. What it lacks is a specified, enforced, actionable error (R5).

### 12.3 What squad relies on in tmux today, and what tymux does not give

The behavioral contract squad users have with tmux (every line read from the squad checkout, read-only):

1. Squad restarts and crashes do not end sessions: the tmux server is a detached daemon (12.1), and `--tmux-keep-server` defaults to `true` (`main.go:930-932`) and sets `exit-empty off` after creating a `keepalive` session (`main.go:470-476`). Service units pass the flag on both OSes (`docs/explanation/tmux-keep-server-on-restart.md`). A missing flag on Linux once recreated every session on restart (#180).
2. Squad re-attaches by session name and rebuilds state from `list-sessions` over a control-mode connection, with reconnect backoff (`session/tmux/server_registry.go:293-353`, `:421`).
3. Squad never kills the tmux server on a failed probe. A scan of `session/tmux/*.go` and `session/health.go` finds `kill-server` only in the explicit operator action `RestartTmuxServer` (`session/tmux/version_check.go:146`, exposed as an RPC in `server/services/tmux_version_status_service.go:48`) and test teardown; `proc.Kill()` hits control-mode clients (the leaked-client sweeper, `leaked_control_mode_sweeper.go:92`), not the server. (VERIFIED by grep; absence in the files searched, not a proof for the whole repo.)
4. Leftovers from a previous squad are cleaned up by client, not by killing the server: `KillOrphanedControlModeClients` at startup and the 5-minute leaked-client sweeper (BUG-042: orphaned control-mode clients accumulate one per restart and eventually crash the server).
5. tmux upgrade: squad pins and bundles 3.8, and does not restart a running server on upgrade; it adapts the client to the running server (`ResolveClientForSocket`) and offers an explicit, user-triggered `RestartTmuxServer` when it detects a mismatch (`version_check.go`). Upgrading tmux therefore never silently ends sessions, and also never upgrades the running server.

Guarantees tmux gives that tymux does **not** give today, with evidence:

| # | tmux guarantee | tymux today | Evidence |
|---|---|---|---|
| G1 | Nothing in tmux ever kills a live server because a health probe failed; a wedged server just makes clients wait (12.1). | `EnsureDaemonRunning` calls `stopTymuxdFn()` before respawning whenever one `ListSessions` probe (2 s deadline, `supervise.go:34`) fails, and the monitor respawns after 3 failures (`daemon_monitor.go:63-78`). A busy daemon that misses a 2 s probe is SIGKILLed with every pane on it. | `supervise.go:341-362` read (`stopTymuxdFn()` at `:358`; the comment there describes the intended case, a stuck listener, BUG-110, but the code path is unconditional). Owner-reported incident, not re-measured here: 6 of 50 squad starts SIGKILLed a live `tymuxd` after a single failed 2 s probe, and the monitor killed busy daemons under load. INFERRED link between that incident and these lines. |
| G2 | The server is unaffected by its launcher's death. | `tymuxd` `setsid`s (OK), but on Linux squad sets `Pdeathsig = SIGKILL` on the daemon it spawns (`supervise.go:232-238`), and the signal survives exec (design 5.7). | Read; `man prctl` semantics INFERRED. |
| G3 | Shutting down the launcher does not stop the server. | `--tymuxd-keep-server` now defaults to `true` (`main.go:933`; registers no stop hook, `main.go:1030-1041`). Parity on this path. Replacement paths (`ReplaceDaemon`, health recovery) still kill (design section 1 table). | Read. Corrects the design section 1 table row "Squad shutdown stops a daemon it spawned", which holds only with `--tymuxd-keep-server=false`. |
| G4 | Skew shows up at the first connection, in-band (coarse but present; real gaps in 12.2). | `GetServerInfo` has richer fields than tmux's single integer, but nothing makes the client refuse or explain a skewed daemon. | ADR 0003; `crates/tymux-proto/src/lib.rs:9-31`. The absence of client-side enforcement is INFERRED from ADR 0003's "can detect, log and refuse" wording; I did not read squad's client for it. |
| G5 | The server's socket path is recoverable without a restart (`SIGUSR1`), and a stale socket is reaped by the next client under a lock. | `flock` + stale reconcile exist (`auth.rs:202-227`); TCP port 7419 is fixed and squad has an `ErrTymuxdPortSquatted` branch. No rebind admin action. | Design F8; `supervise.go:397`. |

Where tymux is already ahead of tmux (so these are not requirements): a restart policy built into the pane (ADR 0007) rather than a hook; `exit_code` persisted in `PersistedPaneRecord` rather than memory only (`persistence.rs:20-40`); dead-flagged session records restored from disk after a daemon restart (tmux restores nothing). One small gap against tmux: persistence stores `exit_code` but no termination signal (grep for `signal` in `persistence.rs` finds none), whereas tmux keeps `pane_dead_signal`; the wire `ExitStatus` has `exit_signal` (`tymux.proto:766-767`). (VERIFIED by grep.)

What tmux does **not** protect against, and tymux therefore need not either (parity is the bar): unplanned server death. SIGKILL and SIGTERM both hang up every pane (12.1). That is evidence for D3 below.

### 12.4 Requirements table: tmux parity and improvements over tmux

P = must match tmux. I = improvement over tmux. "Phase" is the section 8 phase that owns it after this revision. "Status" compares to the current design.

| ID | Kind | Requirement | tmux reference | Phase | Status vs this design |
|---|---|---|---|---|---|
| R1 | P | A running daemon survives its launcher exiting, crashing, or being stopped by launchd/systemd. | `daemon(1,0)`; systemd cgroups/dependencies (`CHANGES` 3.4, 3.7) | 1 | Covered (6.3 item 3). Add a runbook note: systemd needs `KillMode=process` or `none`, launchd needs the daemon outside the job's kill scope (already in 4 Option 1). |
| R2 | P | Supervisor shutdown, restart and upgrade paths never stop a daemon that has live panes without an explicit operator override. | tmux has no supervisor | 1 | Covered (6.3 items 1-2). `--tymuxd-keep-server` default already true (G3). |
| R3 | P | **A failed or slow health probe never kills a live daemon.** Kill requires positive evidence of death (pid gone, or connect refused across a window), never "ListSessions timed out". A slow or unreachable-but-alive daemon gets backoff, an alert, and a client-visible wait, not SIGKILL. | wedged server => clients wait (E4), nobody kills | 1 | **Gap, new 6.3 item 7.** The design's item 4 covers only a "wedged daemon with live panes" in the monitor; the startup path `ensureDaemonRunning -> stopTymuxdFn` (`supervise.go:358`) is a separate, earlier kill site that the design did not list. |
| R4 | P | Socket and lock are stable: a stale socket is reaped only when no one is listening; two daemons never serve one socket; a mid-replace client queues rather than fails. | `client.c:104-170` | 3 | Covered (5.5, F8, T8). Optional I: an admin op to re-create the listener without restart (tmux `SIGUSR1`), UNVERIFIED need; not planned. |
| R5 | P+I | **Version skew is a clear, actionable error, never a hang or a cryptic message.** The client reads `GetServerInfo` first and, if the client's protocol is below the daemon's `min_client_protocol_version`, or the daemon's `protocol_version` is outside the client's supported window, fails with: both versions, the daemon's pid and executable path, which side is stale, and the next action (wait for idle handoff / restart the daemon / upgrade the client). The daemon answers on the old wire format with that error rather than closing. | tmux: coarse (8-bit, bumped rarely, additive messages unbumped), cryptic failure modes (#4890) | 3 (daemon fields), 5 (client behavior) | **Gap.** ADR 0003 defines the fields but not the error contract. Add `ServerInfo.executable_path` and `handoff_schema_range` (the latter already in 6.2). |
| R6 | P | A new client (squad) can always talk to the running old daemon within the supported window; skew handling must not require the old daemon to restart first. | tmux: old server keeps old binary; client must match it | 3, 5 | Mostly covered (additive-only, ADR 0003). Add test: squad N against daemon N-1 and N-2 matrix (T12b). |
| R7 | I | **Live handoff to a newer binary preserving panes** (same pid, children never signaled, no byte lost, clients resume). | tmux cannot (12.1) | 2-4 | The design. |
| R8 | I | Capability negotiation: `graceful_handoff` plus schema range in `GetServerInfo`; supervisor gates on it and on the target binary's preflight. | none | 3 | Covered (ADR 0008 item 3, 6.2). |
| R9 | I | Exit status and restart policy retained across the handoff, including a pane that exits *during* the freeze, and the terminating signal. | `pane_dead_status/signal/time` kept in memory only | 2-3 | Covered for exit code (T7, `PaneHandoff.exited`). Small gap: carry and persist the signal (12.3). |
| R10 | I | Safe fallback: refuse an upgrade while panes are live when the daemon lacks the capability or preflight refuses; never degrade into kill-and-respawn. | tmux: operator restarts when convenient | 1, 5 | Covered (option 5). |
| R11 | I | Observability: handoff outcome and duration counters, skew-refusal counter, probe outcome counters (`probe_failed_daemon_kept`, `probe_dead_confirmed`) so R3 is measurable. | none | 1 (probe), 3 (handoff, skew) | Handoff counters covered (5.6). Probe and skew counters are **new**. |
| R12 | I | Rollback if the new image fails before commit; bound the attempts. | none (no upgrade path at all) | 4 | Covered (5.3). |
| R13 | I | Binary install is atomic and content-addressed, so the running image's file is never overwritten and the previous binary is retained for rollback. | squad's own `extractBinary` (content-hash name, never overwrite), `/proc/<pid>/exe` workaround | 5 | Refines 4 Option 1 "exec across a binary replaced on disk" and 5.3 step 2 (see 12.6). |
| R14 | P | Unplanned death is lossy and may stay so. | SIGKILL/SIGTERM hang up all panes (12.1) | 7 (optional) | Consistent with D3 recommendation. |

### 12.5 Reordering and scope changes to the phased plan

1. **Parity first stays first, but Phase 1 is bigger than written.** The existing Phase 1 already precedes handoff. It lacked R3's startup kill site and R11's probe counters; section 8 now lists both, and states that Phase 1 lands and soaks before any handoff phase ships. Reason: handoff only protects a *planned* replacement, while the incident evidence is a supervisor killing a live daemon on one slow probe, which no amount of handoff fixes. (INFERRED from the owner-reported incident and `supervise.go:358`.)
2. **New 6.3 item 7 (squad).** `ensureDaemonRunning` must not call `stopTymuxdFn()` merely because `checkDaemonHealthyFn` returned false. Required before a kill: the PID file's process is gone, or the daemon's UDS/TCP connect has been refused across a window (not a timeout), or the operator forced it. A connect that succeeds but times out on `ListSessions` is "busy": wait with backoff, alert, do not spawn or kill. This also removes the "second daemon collides with the still-held `tymuxd.sock` lock and dies" failure the code comment at `supervise.go:336-357` describes, since a live-but-slow daemon is no longer displaced.
3. **R5 splits across phases.** Daemon fields in Phase 3 (they ride along with `PrepareHandoff`'s proto change), client enforcement in Phase 5. Doing the skew error in Phase 5 only would leave Phases 3-4 shipping a daemon whose skew failure is the cryptic kind.
4. **Phase 2 and Phase 1 are independent and can run in parallel** (different repos, no shared code).
5. No new phase and no change to Phases 4, 6, 7 ordering.

### 12.6 Where tmux's behavior argues for or against choices in this design

For:
- **Option 1 over a keeper (Option 6) or shims (Option 3) for now.** tmux is itself one long-lived server per socket with no per-pane process, and accepts that its death hangs up every pane (12.1). The "better than tmux" bar for this project is therefore planned replacement, not crash survival, which is exactly what Option 1 adds. (INFERRED argument from verified behavior.)
- **Unlinked inherited state fd (D4).** tmux persists nothing (memory only), so no precedent for secrets on disk; resurrect writes files, which is the cost the unlinked fd avoids. (INFERRED for resurrect.)
- **Explicit `Reconnect{HANDOFF}` (D6).** A tmux client can only report `server exited unexpectedly` (`client.c:210-211`) for a planned stop, a crash and a kill alike; squad's own incident hit exactly that message. A typed, planned reconnect event is a strict improvement.
- **Reading the target path from the RPC instead of `/proc/self/exe`.** The tmux community's `/proc/<pid>/exe` trick works only on Linux (no macOS equivalent, as 4 Option 1 already says) and only while the old image is still mapped.

Against, or requiring a change:
- **Fixed install path (current 6.3 item 5 and 4 Option 1) versus content-addressed paths.** Squad's tmux code, written after incidents, never overwrites a binary an old server may still run from and names it by hash (`binary_extract.go:11-14`). The tymux cache path is fixed with no version component (design 4 Option 1), so install-by-rename replaces the file the old daemon was started from. Content-addressed `tymuxd-<hash12>` plus a retained previous path would give 5.3's rollback exec for free (no need to hard-link the old image at handoff time) and avoids the "old image's `/proc/self/exe` is `(deleted)`" case. Recommended change, R13. The macOS code-signing aspect is quoted from squad's comment and is not re-tested here.
- **"Freeze mutating RPCs with UNAVAILABLE" has no tmux analogue to borrow.** tmux clients simply wait on the backlog (E4). The design's bounded freeze (2 s) with `retry-after` is still right for an exec window, but the lesson from E4 is the converse for R3: a client should wait with a deadline and a message, not trigger recovery. No design change, a note for the client work in Phase 5.
- **Re-exec vs fd-keeper trade-offs do not shift.** tmux's model neither helps nor hurts the exit-code argument against SCM_RIGHTS (Option 2): tmux is the parent of its panes and reaps with `waitpid(WAIT_ANY)` (`server.c:468-489`), the same property that makes `PosixChild` after exec work (F5).
- **Nothing in tmux supports building a rollback exec**; it has no upgrade path to roll back. 5.3 stays as designed and carries the most risk without prior art, which is why T6 and T13 remain mandatory.

### 12.7 Recommendations for the open decisions (tmux evidence only; all remain the owner's call)

- **D1 (Option 1 + option 5 fallback):** yes, with Phase 1 parity shipped and soaked first. tmux shows re-exec is novel (no prior art for exec-time correctness), so the drain fallback and rollback are not optional.
- **D3 (survive unplanned death):** recommend **no, not a requirement.** tmux does not survive it either (SIGKILL and SIGTERM both hang up every pane). Keep Phase 7 optional.
- **D4 (unlinked state fd):** recommend the unlinked fd, with the hybrid `--handoff-debug-dump` (0600) for support.
- **D5 (own launchd job):** not needed for parity. tmux's independence comes from `daemon()`+`setsid`, which `tymuxd` already has (design section 1); the actual parity gaps are `Pdeathsig` and the kill-on-probe sites (R1-R3). Defer; revisit only if launchd or systemd begins killing the daemon's process group.
- **D6 (`AttachEvent.reconnect`):** recommend yes, for the reason in 12.6.

### 12.8 Unknowns from this study

- A real tmux client/server mismatch (exact error text and whether it hangs) was not reproduced; no mismatched binary pair was available and building one was refused (12.2). UNVERIFIED.
- Whether `PROTOCOL_VERSION` was bumped between other tags; only `3.3`, `3.7b`, `3.8` were checked, plus the 1.9 note.
- The cause of tmux issue #5822 (`client 8, server 8` mismatch) is unknown.
- Whether the owner's "6 of 50" and load-kill incidents trace to `supervise.go:358` or to the monitor path (`daemon_monitor.go`) was not determined; both are candidates (INFERRED). Phase 1 should instrument which site fires (R11).
- Squad's client-side handling of `GetServerInfo` skew (G4) was not read; the claim that nothing enforces it is INFERRED from ADR 0003 wording.
- Whether tmux-resurrect restores process state: only its README was read.
