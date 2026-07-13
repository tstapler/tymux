# tymux

A tmux-inspired terminal multiplexer, rebuilt from scratch in Rust with a
first-class gRPC/protobuf API. tmux's session/window/pane model is the
starting point; the reason to rebuild it rather than script around tmux is
the API — the multiplexer's core state (what's on screen, structured, not
scraped) is meant to be driven by things other than a human at a terminal:
AI coding agents, web frontends (e.g. [stapler-squad](https://github.com/tstapler/stapler-squad)),
scripts in any language buf can generate a client for.

## Why not just script tmux?

tmux's own scripting surface (`capture-pane`, control mode) hands you text —
ANSI escapes included — that a caller has to re-parse to know what's
actually on screen. `tymux`'s `CapturePane` and `Attach` RPCs return a
structured grid of cells with attributes directly (see
`proto/tymux/v1/tymux.proto`), backed by a real terminal-state parser
([`vt100`](https://docs.rs/vt100)) inside the daemon. That structured model
is the actual point of this project.

## Layout

```
crates/
  tymux-core/   session/window/pane engine — PTY spawn (portable-pty) + vt100 screen state
  tymux-proto/  generated Rust types from proto/ (tonic-build via build.rs)
  tymuxd/       the daemon: gRPC server wrapping tymux-core
  tymux-cli/    thin client: create/list/attach sessions from a terminal
proto/          buf-managed .proto — lint/breaking-change checks, and the
                source of truth for any future non-Rust client (buf.gen.yaml)
```

Rust's own codegen goes through `tonic-build` directly against
`proto/tymux/v1/tymux.proto` (the idiomatic path for a Rust service) — buf
manages proto hygiene (`buf lint`, `buf breaking`) and is where plugins for
other-language clients (TS, Python, ...) get added later, in
`proto/buf.gen.yaml`.

## Status

MVP: one pane per window per session (no splits yet — the proto already
models `repeated windows`/`repeated panes`, so this is additive, not a
breaking change, when splits get built). No auth — the daemon is meant to
run locally or on a trusted network for now. No persistence — sessions
live in the daemon's memory only; killing `tymuxd` kills every session,
same as tmux's own server model but without tmux's socket-survives-crash
guarantee (that's a real gap if this needs to survive a daemon restart —
add it when it matters).

## Running it

```sh
cargo run -p tymuxd &          # starts the daemon on 127.0.0.1:7419
cargo run -p tymux-cli -- new  # creates a session and attaches
```

`Ctrl-d` (or exiting the shell) ends the pane; the daemon keeps running
for the next session.

## Dev setup

- [buf](https://buf.build/docs/installation) — `buf lint proto` before
  committing a proto change
- `cargo fmt` / `cargo clippy --workspace --all-targets` — enforced in CI
  (`.github/workflows/ci.yml`)
