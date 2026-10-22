# ADR 0004: CreateSession stderr redirect and file-backed arguments

**Status**: Accepted

## Context

stapler-squad launches agents through `/bin/sh -c "<command line>"` because `CreateSessionRequest` could not
express two things without a shell:

1. Sending the program's stderr to a log file (`2>>log`). A pty pane normally merges stderr into the tty.
2. Passing a prompt too large for one argv string. Linux caps a single argument at `MAX_ARG_STRLEN`
   (128 KiB, including the trailing NUL), so stapler-squad writes big prompts to a temp file and has the shell
   splice them in (`"$(cat file)"`).

Wrapping in a shell also means every argument has to be shell-quoted on the way in. Both gaps are closed here so
a client can send a real argv.

## Decision

Both additions are optional fields (no protocol bump, `buf breaking` clean) and each is advertised in
`ServerInfo.capabilities` per [ADR-0003](0003-protocol-versioning.md).

### 1. `string stderr_path = 6` — capability `create_session.stderr_redirect`

- The daemon opens the path `O_WRONLY|O_APPEND|O_CREAT` (mode `0600`) and makes it the child's fd 2. stdin and
  stdout stay on the pty slave. No shell.
- **Feasibility.** On `main`, portable-pty's `spawn_command` hard-wires stdin, stdout and stderr to the slave
  (`portable-pty-0.8.1/src/unix.rs`, `cmd.stdin(..).stdout(..).stderr(..)`), so stderr cannot be split off there.
  The own-spawn path (PR #62) builds the child itself, and fd 2 is one `posix_spawn_file_actions_adddup2` (glibc)
  or one `Command::stderr` (fallback): a clean split. **This change is therefore stacked on #62.**
- Consequences, all inherent to splitting stderr off the pty: the child sees `isatty(2) == false`; stderr never
  appears in the pane's screen, scrollback, replay buffer or `CapturePane`; the relative ordering of stdout and
  stderr is no longer preserved anywhere.
- Must be absolute (a relative path would resolve against the daemon's cwd, not the pane's). An unopenable path
  (missing parent directory, permissions) fails the call with `FAILED_PRECONDITION`.
- Persisted in the session record and re-opened (appended to) on revive.

### 2. `repeated ArgSpec arg_specs = 7` — capability `create_session.file_args`

```proto
message ArgSpec { oneof source { string literal = 1; string file_contents_path = 2; } }
```

- Added beside `args`, not replacing it. Final argv = `command`, then `args`, then each `arg_specs` entry in
  order. A client that needs a file-backed argument *between* literals puts all its arguments in `arg_specs`.
- `file_contents_path` (absolute) is read **in full, untrimmed**, at every spawn and every revive. Only the path
  is persisted, never the contents, so a revive sees the file as it is then.
- Limits and errors on the resolved value, each with a message naming `arg_specs[i]` and the path:

  | Condition | Status |
  |---|---|
  | file does not exist | `NOT_FOUND` |
  | file unreadable | `FAILED_PRECONDITION` |
  | path not absolute, empty `ArgSpec`, contents not UTF-8, contain NUL, or `>= 128 KiB` | `INVALID_ARGUMENT` |

- **Oversize.** The cap is `MAX_ARG_STRLEN` = 131072 bytes including the NUL, so at most 131071 content bytes.
  It is enforced on every platform so behavior does not depend on the host. Reads are bounded, so a path like a
  FIFO cannot make the daemon buffer more. The kernel's *total* argv+env limit (`ARG_MAX`) is not pre-checked; an
  over-large total surfaces as the spawn's `E2BIG` (`INTERNAL`), with no test of that path.
- **Missing file at revive.** `ReviveSession` resolves every dead pane's launch *before* respawning any, and fails
  the whole call with `FAILED_PRECONDITION` (message: `cannot revive pane <id>: arg_specs[i]: cannot read <path>: ...`).
  Nothing is respawned and the session stays dead, so the caller can restore the file and retry. It never spawns
  with an empty or missing argument. Other respawn failures (for example a vanished program) keep their existing
  warn-and-skip behavior.

## Consequences

- stapler-squad can drop `/bin/sh -c`: arguments with spaces, quotes, `$` or backticks reach the process
  unmangled (covered by `spawn_should_deliver_awkward_args_unmangled_without_a_shell`).
- A client must gate on the capability: an old daemon accepts the new fields and ignores them, so stderr would stay
  on the pty and a file-backed argument would silently vanish (shown by
  `TestOldDaemonAcceptsButIgnoresNewCreateSessionFields`). `clients/go/capabilities` wraps the check.
- Revive now depends on files outside the session record. Clients that put prompts in temp files must keep them for
  as long as the session may be revived.
- The stderr file is append-only and never rotated by the daemon.
