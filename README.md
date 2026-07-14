<p align="center">
  <img src="logos/export/logo.svg" width="160" alt="tymux logo — a recursive golden-ratio pane split, panes labeled T Y M U X like tmux's own pane-select overlay">
</p>

<h1 align="center">tymux</h1>
<p align="center"><strong>tmux's model, rebuilt with a typed API.</strong></p>

A tmux-inspired terminal multiplexer, rebuilt from scratch in Rust with a
first-class gRPC/protobuf API. tmux's session/window/pane model is the
starting point; the reason to rebuild it rather than script around tmux is
the API — the multiplexer's core state (what's on screen, structured, not
scraped) is meant to be driven by things other than a human at a terminal:
AI coding agents, web frontends (e.g. [stapler-squad](https://github.com/tstapler/stapler-squad)),
scripts in any language buf can generate a client for.

- **Structured pane capture** — `CapturePane`/`Attach` return cells with
  attributes, not raw ANSI text you have to re-parse
- **One proto schema, buf-managed** — add a TS/Python/Go client without
  touching the Rust core
- **Built PTY-up for programmatic control** — not a human-scripting tool
  with an API bolted on afterward

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
  tymux-cli/    thin client: create/list/attach/kill sessions from a terminal
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
breaking change, when splits get built; see `docs/adr/0001-single-pane-per-session-for-now.md`).
No auth — the daemon is meant to run locally for now; it warns loudly at
startup if `TYMUXD_ADDR` is set to a non-loopback address, since there's
no per-pane authorization yet. No persistence — sessions live in the
daemon's memory only; killing `tymuxd` kills every session, same as
tmux's own server model but without tmux's socket-survives-crash
guarantee (that's a real gap if this needs to survive a daemon restart —
add it when it matters).

See `docs/reviews/is-it-ready-2026-07-13.md` for the full readiness
review this status section is drawn from, including what's since been
fixed.

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
- **Releasing**: bump `[workspace.package] version` in `Cargo.toml`, then
  push a matching `vX.Y.Z` tag. CI's `tag-version-check` job fails the
  build if the tag and workspace version ever drift.

