# ADR 0006: Streaming a process's stdin with WriteStdin

**Status**: Accepted

## Context

[ADR-0005](0005-create-session-stdin-file.md) feeds a file the daemon can read to a pane's stdin. It does not cover
input that is produced after the process starts, or lives on the client (a file on another host, a generated
prompt, a pipe): there the client has to stream the bytes. Typing them into the pty has the same defects ADR-0005
lists for option (a) (4095-byte lines, control bytes interpreted, echo, no EOF), so the stream must reach the
process through a pipe, not the tty.

## Decision

Two additions, advertised together as capability **`create_session.stdin_stream`** (optional field plus a new RPC;
no protocol bump, `buf breaking` clean).

1. **`bool stdin_stream = 9`** on `CreateSessionRequest`. The daemon creates a pipe, gives the child its read end as
   fd 0 and keeps the write end in the pane. As in ADR-0005, stdout, stderr and the controlling tty stay on the pty
   and `isatty(0)` is false. Mutually exclusive with `stdin_path` (`INVALID_ARGUMENT`).
2. **`rpc WriteStdin(stream WriteStdinRequest) returns (WriteStdinResponse)`.** The first message names the pane
   (`pane_id`); later ones carry `data` (written unmodified, in order) or `close = true` (EOF). The response reports
   `bytes_written` for that call.

Semantics:

- **Backpressure.** Writes block while the process is not reading (pipe capacity is 64 KiB on Linux); the daemon does
  the write off its async threads and stops reading the request stream meanwhile, so HTTP/2 flow control slows the
  client. Memory use does not grow with the input.
- **EOF is explicit.** The pipe stays open when a call's stream ends without `close`, so one input may span several
  calls or reconnects. Dropping the client does not send EOF. The pipe is also closed when the pane is discarded.
- **Concurrent writers** are serialized per message; their messages may interleave. Use one writer per pane.
- **Errors**, all `FAILED_PRECONDITION` with a message: pane dead or revived-away, pane not created with
  `stdin_stream`, stdin already closed, or broken pipe (the process closed its stdin or exited; the unwritten bytes
  are lost). A malformed message (first not `pane_id`, later ones neither `data` nor `close = true`) is
  `INVALID_ARGUMENT`.
- **Revive.** `stdin_stream` is persisted, but input never is: a revived pane gets a fresh, open, empty pipe and the
  client must stream again. The daemon does not replay or buffer.
- **Old daemons.** They drop `stdin_stream` (the process would wait on the pty) and answer `WriteStdin` with
  `UNIMPLEMENTED`. Gate on `create_session.stdin_stream` (`capabilities.StdinStream` in `clients/go`).

## Consequences

- stapler-squad can start `claude -p` first and stream a prompt of any size afterwards, with a real EOF, no shell
  and no temp file; `stdin_path` stays the simpler choice when the file is on the daemon's host.
- The caller must send `close`, or a program that reads to EOF never finishes.
- A fast process can exit before the first `WriteStdin` (the call then fails with `FAILED_PRECONDITION`), and
  an exited pane rejects Attach and CapturePane as before.
- The non-glibc spawn branch is as unverified as in ADR-0005 (Linux/glibc only here).
