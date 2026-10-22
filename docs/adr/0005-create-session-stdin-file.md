# ADR 0005: CreateSession stdin from a file

**Status**: Accepted

## Context

[ADR-0004](0004-create-session-stderr-redirect-and-file-args.md) lets a prompt travel as a file-backed argument, but
a single argv string is capped at 128 KiB (`MAX_ARG_STRLEN`). stapler-squad's one-shot agent runs (`claude -p`
style) feed a prompt larger than that on **stdin**, where a shell would write `< prompt.txt` or a heredoc, and
need a real EOF at the end.

ADR-0004 deliberately left stdin on the pty, so a file cannot just replace stdin without giving something up. Two
ways to deliver a file to a pty pane were evaluated.

## Options

### (a) Type the file into the pty master, stay interactive — rejected

After spawn, write the file's bytes to the master so the program reads them as keystrokes. The tty line discipline
makes this unreliable for exactly the inputs that motivate the feature:

- **Canonical mode caps a line at 4095 bytes** (`N_TTY_BUF_SIZE` 4096): input past that without a newline is
  dropped, and a prompt is often one very long line.
- **Control bytes are interpreted, not delivered**: `^C` (0x03) becomes SIGINT, `^D` (0x04) EOF, `^Z`, `^\`, `^S`/`^Q`
  flow control, and `\r` is rewritten to `\n` (ICRNL). NUL and other bytes the program never asked for are not
  preserved, so it is not even binary-safe-ish.
- **Echo** writes every byte back to the master: a megabyte lands in the pane's screen, scrollback and replay buffer
  and evicts real output, unless the program has already turned echo off, which the daemon cannot know.
- **No EOF**: the only EOF a tty can send is `^D` at the start of a line in canonical mode.
- **Timing**: the program may not have set raw mode yet (so the bytes are cooked), may `tcflush` its input at
  startup (so the bytes vanish), and a full tty buffer blocks the writer until the program reads, which needs a
  feeder thread and a story for a program that never reads.

It is a hard-to-test heuristic that fails silently, which this protocol has avoided so far. Interactive typing is
already served by attach input. **(a) is not worth building** and is not offered.

### (b) stdin *is* the file, the pane stays non-interactive for input — chosen

The daemon opens the file read-only and installs it as the child's fd 0, as `< file` does. The program reads the
exact bytes from offset 0 and sees a real EOF at the end of the file. This is what the one-shot use needs, and it
is byte-exact with no size limit and no daemon-side buffering (the kernel serves reads straight from the file).

### (c) Both, behind an enum — rejected for now

With (a) out, an enum would have a single useful value. If another mode is ever needed, an additive
`stdin_mode` enum whose zero value means "file as stdin" can be introduced without changing this field.

## Decision

`string stdin_path = 8` on `CreateSessionRequest`, advertised as capability **`create_session.stdin_file`**
(optional field; no protocol bump; `buf breaking` clean).

- **Semantics.** fd 0 is the file; fd 1 is the pty slave, and so is fd 2 unless `stderr_path` is set. The slave is
  still the controlling terminal, so job control, `SIGWINCH` and `/dev/tty` keep working. Consequences: `isatty(0)`
  is false, `isatty(1)` is true, a program that checks `isatty(0)` picks its non-interactive mode, and nothing typed
  into the pane (attach input, send-keys) reaches the program: it goes to the tty, which the program does not read.
- **Opened by the daemon, no shell**, at every spawn and every revive, with a fresh descriptor, so each run reads
  from byte 0 and a revive sees the file as it is then. Only the path is persisted, never the contents (same as
  ADR-0004's `file_contents_path`). Open files are not snapshotted: truncating or rewriting the file while the
  program reads it is visible to the program, as with any shell redirection.
- **Must be an absolute path to a regular file.** Directories, FIFOs and devices are refused (opening a FIFO would
  block the daemon until a writer appeared).

  | Condition | Status |
  |---|---|
  | file does not exist | `NOT_FOUND` |
  | file unreadable | `FAILED_PRECONDITION` |
  | path not absolute, or not a regular file | `INVALID_ARGUMENT` |

  Each message names the path. An empty or missing input is never silently substituted.
- **Revive.** `ReviveSession` resolves every dead pane's launch before respawning any (the ADR-0004 mechanism, now
  also opening the stdin file). A missing or unreadable file fails the whole call with `FAILED_PRECONDITION`, names
  the path, respawns nothing and leaves the session dead.
- **Old daemons.** They accept and drop unknown fields, so the program would run with its stdin still on the pty,
  waiting for input that never comes. A client must check `create_session.stdin_file` in
  `GetServerInfo.capabilities` first (`capabilities.StdinFile` in `clients/go`).
- **EOF and exit.** The child exits when it chooses to. The pane stays attachable only while the process lives:
  a pane whose process has already exited rejects Attach and CapturePane with `FAILED_PRECONDITION`, so a client
  running a fast one-shot should attach (or take its result from a file) rather than rely on capturing afterwards.

## What is not supported

- Typing the file into an interactive program (option (a)), or leaving stdin interactive while also supplying a file.
- Streaming or appending to the program's stdin after spawn: the file is the whole input, EOF comes with its end.
- Non-regular sources (FIFO, socket, device, directory).
- Windows: the field is rejected on non-unix builds, like `stderr_path`.
- On non-glibc unix hosts the spawn path is `Command` + `pre_exec`, which now takes the controlling terminal from
  fd 1 instead of fd 0; that branch was not compiled or run on the development host (Linux/glibc only).

## Consequences

- stapler-squad can run `claude -p` with a multi-megabyte prompt and no shell. The 128 KiB limit applies only to
  `arg_specs`, not here (covered by the >1 MiB tests across `tymux-core`, `tymuxd` and the Go client).
- Revive depends on one more file outside the session record; keep prompt files while the session may be revived.
