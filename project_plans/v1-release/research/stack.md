# Stack Research: v1-release

Research for Phase 2 (SDD) of the tymux v1.0 gap-closing plan. Covers the
seven areas requirements.md flags as unproven/greenfield: splits, persistence,
scrollback, status bar, config/keybindings, TS client, and release CI.

Current workspace deps (`Cargo.toml`): `tokio 1`, `tonic 0.12`, `prost 0.13`,
`portable-pty 0.8`, `vt100 0.15`, `uuid 1`, `crossterm 0.28`, `clap 4`,
`tracing`/`tracing-subscriber 0.3`. No persistence, config, or TUI crate
currently in the graph.

---

## 1. Splits / layout engine

**Model**: real tmux (and wezterm, and zellij) represent panes as a **binary
tree of splits**, not a fixed grid. Each internal node is a split direction
(horizontal/vertical) + a size ratio between its two children; leaves are
panes. This is what makes uneven splits, nested splits, and "split this
specific pane" possible — a fixed N×M grid can't express that.

- **wezterm**: panes within a tab are literally implemented as a binary tree
  internally (`Mux` → Window → Tab → binary tree of Panes). The tree
  traversal API is deliberately *not* exposed to Lua config because the
  maintainers reserve the right to change the internal representation —
  a useful precedent for tymux: keep the tree an internal engine detail,
  expose only flattened window/pane views over the gRPC API (which matches
  the current proto's `repeated Window`/`repeated Pane` — flat, not tree-
  shaped on the wire).
- **zellij**: layouts are declared in KDL with nested `pane` blocks and a
  `split_direction` attribute per node — same binary-tree-of-splits model,
  just with a human-authorable declarative format layered on top (relevant
  later for tymux's own config work, not the engine itself).
- **No off-the-shelf crate** does this for you — `zellij` and `wezterm`
  both hand-roll their layout tree as an internal type (not published as a
  reusable crate). This is expected: the tree is small, domain-specific
  (needs pane_id ↔ pty ↔ vt100::Parser bookkeeping tymux already has per
  pane), and coupling it to a generic layout-tree crate would add
  indirection for little gain.

**Recommendation**: hand-roll a `Layout` enum in `tymux-core`, e.g.:

```rust
enum Layout {
    Pane(PaneId),
    Split { direction: Direction, ratio: f32, first: Box<Layout>, second: Box<Layout> },
}
```

stored per-`Window` alongside the existing pane map. This is additive to the
existing proto (`repeated Window`/`repeated Pane` already accommodates more
than one pane per window per ADR 0001) — the tree stays a daemon-internal
detail; the gRPC surface reports flattened pane geometry (x/y/rows/cols),
which is what any client (including a future TS one) actually needs to draw
panes, not the tree shape itself. Compute geometry by walking the tree and
recursively subdividing the window's rect by each split's ratio — this is
the same technique wezterm/zellij use, no crate needed.

## 2. Persistence

Three real candidates, weighed for a **single-process personal daemon**,
not a distributed system:

| Option | Verdict |
|---|---|
| `sled` | **Avoid.** Long-abandoned (multi-year gap between releases), never left beta, file-format stability concerns are well-documented community knowledge. Recent (as of this research) signs of the original author resuming work don't offset the multi-year gap for a project that wants a boring, load-bearing dependency. |
| `redb` (cberner) | **Good fit if embedded KV is wanted.** Pure Rust, no C dependency, ACID, copy-on-write B-tree (LMDB-inspired), stable on-disk file format with an explicit upgrade-path commitment. Actively maintained (frequent releases through 2026, at major version 4.x by this research). Single-file, single-process — matches tymux's daemon exactly. |
| `rusqlite` (bundled feature) | **Good fit if relational/queryable state is wanted**, e.g. if session/window/pane metadata benefits from being queried/joined rather than just fetched by key. Bundled SQLite via `cc`-compiled source avoids system-library linking headaches. Very mature, huge install base, SQLite itself is the most battle-tested embedded store that exists. |
| flat file (`serde` + JSON or `bincode`, atomic rewrite-then-rename) | **Simplest option, plausibly sufficient.** Given the durability contract requirements.md is steering toward — "metadata survives, live pty/scrollback state does not" — the persisted data is small (session/window/pane IDs, names, geometry, command lines), infrequently written (on session/window/pane create/destroy, not per-keystroke), and never queried with anything beyond "load it all at startup." A single `serde_json`-serialized snapshot file, written atomically (write to temp file + `rename`) on every mutating operation, has no real durability disadvantage vs. an embedded DB at this scale, and it's trivially inspectable/debuggable (`cat` the file) — a real ergonomic win for a solo-maintained project. |

**Recommendation**: **flat file (JSON via `serde` + `serde_json`, atomic
write-rename)** as the primary recommendation, given the stated scope
("metadata survives, live state doesn't" — see Rabbit Holes/Feasibility
Risks in requirements.md) and solo-dev/no-fixed-deadline framing. If Phase 3
planning decides richer querying or higher write-frequency durability is
actually needed, `redb` is the fallback — it's pure Rust (no new toolchain
dependency, unlike `rusqlite`'s C compile step), stable file format, and
still trivially embeddable in `tymuxd`. Do not use `sled`.

## 3. Scrollback / copy-mode

**Correction to the problem statement's premise**: `vt100` (already a
dependency, `vt100 = "0.15"`) **does support scrollback natively** — this is
not a gap requiring a new/different crate, it's a gap in how the existing
crate is being used.

Verified API (docs.rs, `vt100::Parser` / `vt100::Screen`):

- `Parser::new(rows: u16, cols: u16, scrollback_len: usize) -> Self` — the
  third argument (which `tymux-core/src/pane.rs` currently hardcodes to
  `SCROLLBACK_LINES: usize = 0` — see `pane.rs:15`) is exactly the
  scrollback buffer size in lines. Setting it > 0 makes the parser retain
  that many scrolled-off lines automatically as the child process writes
  more output than fits on screen — no additional buffering code needed on
  tymux's side.
- `Screen::set_scrollback(&mut self, rows: usize)` — scrolls the *view* to
  an offset from the top of the live screen (0 = normal/live view). This is
  the primitive copy-mode navigation would drive (PageUp/PageDown/etc.
  adjust this offset).
- `Screen::scrollback(&self) -> usize` — reads the current scroll offset.
- `Screen::rows(&self, start: u16, width: u16) -> impl Iterator<Item = String>` —
  reads text rows, works against whatever the current scrollback offset is,
  so this is how a `CapturePane`-style call would read historical lines.
- `Screen::cell(row, col)` / `Screen::contents_between(...)` — structured
  (cell-level) and range-based text access, both scrollback-aware, matching
  tymux's structured-capture design goal (README: "cells with attributes,
  not raw ANSI text").

**Recommendation**: no new crate. Bump `SCROLLBACK_LINES` to a real value
(config-driven per Section 5, with a sane default — e.g. 1000–10000 lines,
tmux's own default is 2000), thread a scrollback offset through the
existing `Pane`/`CapturePane` path using `set_scrollback`/`scrollback`, and
extend the proto/CLI to send navigation commands (line/page up/down,
search) that adjust the offset before capturing. The "interactive way for a
human user to navigate and copy from it" requirement is a **CLI-side
copy-mode key-handling problem** (see Section 5 — local keystroke
interception), not a terminal-state-parsing problem; the parsing/storage
side is already solved by the existing dependency.

## 4. Status bar (chrome around raw pty passthrough)

The core constraint: `tymux-cli`'s attach loop (`crates/tymux-cli/src/main.rs`)
currently does two things concurrently — forwards raw stdin bytes to the
daemon (`AttachRequest::Input`) and writes raw output bytes straight to
stdout (`attach_event::Payload::Output(bytes) => ...`) after putting the
local terminal in raw mode (`crossterm::terminal::enable_raw_mode`). Any
status bar has to coexist with that byte-for-byte passthrough without the
child program's own screen-manipulating escape codes (cursor moves, clears,
alt-screen) corrupting the status line, and vice versa.

Two real approaches, researched:

- **Full TUI framework (`ratatui`)**: gives you a proper `Layout` with
  `Constraint::Length(1)` reserved rows for a status bar and a main content
  area, diffed/double-buffered rendering, and (via `Terminal::insert_before`
  or an inline viewport) support for coexisting with a scrolling region.
  But this is built around *ratatui owning the whole screen render loop* —
  using it here means treating the pty's live output as data ratatui itself
  draws into a widget/paragraph area, not truly "passthrough." That's a
  materially bigger dependency and a rewrite of the attach loop's rendering
  model (from "copy bytes" to "own a render loop and redraw"), not an
  incremental add. Appropriate if copy-mode/status bar together push tymux
  toward "the CLI is basically a mini terminal emulator," not appropriate as
  a minimal add-on.
- **Reserve a row via ANSI scroll-region escapes (`DECSTBM`, `\x1b[1;{n-1}r`) +
  manual cursor save/restore**: the traditional approach (this is literally
  how tmux's own status bar and vim's command line coexist with full-screen
  apps) — set the terminal's scrolling region to rows 1..(height-1) so the
  child program's own clears/scrolls/cursor-addressing stay confined to the
  region above the last row, then independently paint the status line by
  cursor-positioning (`\x1b[{row};1H`) to the reserved last row, writing
  status text, and restoring cursor position (`\x1b[s`/`\x1b[u` or manual
  save) before resuming passthrough. This keeps the existing byte-copy
  passthrough model almost entirely intact — it's an interception layer
  that (a) sends the scroll-region escape once at attach time and on
  resize, and (b) periodically/on-event writes the status line to the
  reserved row, sharing the same stdout writer already used for output
  bytes. `crossterm` (already a dependency) has the primitives needed
  (`cursor::MoveTo`, `cursor::SavePosition`/`RestorePosition`, raw
  `queue!`/`execute!` for arbitrary escape sequences) — no new crate
  required.

**Recommendation**: reserved-row via scroll-region escapes, using the
existing `crossterm` dependency, not `ratatui`. This matches the Rabbit
Holes framing in requirements.md ("partial-screen redraw without corrupting
the pty's own output," not "rewrite the CLI as a TUI app") and keeps the
core differentiator (byte-transparent passthrough) intact. `ratatui` should
be treated as a documented alternative/fallback if the scroll-region
approach proves fragile against programs that reset scroll regions
themselves (`vim`, `tmux`-inside-tymux, full-screen curses apps are known
offenders) — worth a spike before committing, flagged as a real risk, not
assumed to Just Work for every child program.

## 5. Config / key-bindings

**Config file format**: `toml` + `serde` (`Deserialize`) is the obvious,
low-risk choice — it's Rust's de facto config format (cargo itself uses
it), pairs directly with `serde` (not yet a direct dependency but pulled in
transitively already via `tonic`/`prost`; making it explicit is trivial),
and needs no exotic parser. Location: standard `~/.config/tymux/config.toml`
via the `dirs` crate (small, unmaintained-risk-free, single-purpose) for
XDG-compliant path resolution — consistent with the `emux`/`wtmux` prior
art found during research (both use `~/.config/<name>/config.toml`).

**Key-binding / prefix-key handling**: today every keystroke is forwarded
raw (`crates/tymux-cli/src/main.rs:171-178` reads stdin and wraps it
straight into `AttachRequest::Input` with no interception). A prefix-key
system needs local state: buffer/inspect the byte(s) just read, check
against a "waiting for prefix" state machine, and only forward to the pty
if it doesn't match a binding (or after a timeout, matching tmux's own
`escape-time` behavior for its own prefix key).

Prior art found: the `keybinds` crate (small, framework-agnostic, pure-Rust)
supports exactly this shape — parsing bindings from a config-file syntax
(`"Ctrl+b" = "..."`) and multi-key **sequences** (`Ctrl+x Ctrl+s`-style,
i.e. prefix-key chords), which is the same pattern tmux uses (`Ctrl+b` then
a follow-up key). It's dispatch-only (you feed it key events, it tells you
which binding fired) — it doesn't handle raw byte→key-event decoding itself,
so tymux would still need to decode raw pty-input bytes into key events
(likely via `crossterm::event` parsing, since raw mode + crossterm is
already how the CLI reads local input) before handing them to `keybinds`
for matching.

**Recommendation**: `toml` + `serde` for config; `dirs` for path resolution;
either the `keybinds` crate or a small hand-rolled prefix-key state machine
(given the *only* required binding for 1.0 is a single detach sequence —
requirements.md's bar is "at minimum a working detach keybinding" — a
hand-rolled 20-line state machine may honestly be lower-risk than pulling
in a new crate for one binding; revisit `keybinds` if copy-mode's
navigation keys end up needing a larger binding table). Either way, this
is genuinely greenfield — budget real design time for the intercept point
in the attach loop (this is the same architectural change needed for
copy-mode per Section 3, confirming requirements.md's note that these
should be solved together).

## 6. Cross-language client (TypeScript)

**Toolchain**: `@bufbuild/protoc-gen-es` (v2, "Connect-ES 2.0") is the
current (2026) recommended single codegen plugin — a load-bearing change
from the older toolchain: **Connect-ES 2.0 merged message and service-stub
generation into `protoc-gen-es` itself**; the older, separate
`protoc-gen-connect-es` plugin (`@bufbuild/protoc-gen-connect-es`, the
pre-2.0 package) is legacy/no-longer-needed for new projects. The current,
actively-maintained package for hand-written client/server primitives is
`@connectrpc/connect` (note the `@connectrpc` scope superseded `@bufbuild`
for the runtime packages — `@connectrpc/connect-node` for Node.js,
`@connectrpc/connect-web` for browsers).

Minimal `proto/buf.gen.yaml` addition (the file currently has `plugins: []`
with a comment placeholder for exactly this):

```yaml
plugins:
  - local: protoc-gen-es
    out: ../clients/ts/gen
    opt: target=ts
```

npm deps for a Node.js TS client: `@bufbuild/protobuf` (serialization),
`@connectrpc/connect` (client primitives), `@connectrpc/connect-node`
(Node transport). `@bufbuild/buf` + `protoc-gen-es` as devDependencies for
codegen.

**Load-bearing finding for the `Attach` RPC specifically**: bidirectional
streaming **requires end-to-end HTTP/2**, and **browser clients cannot do
true bidirectional streaming today** — `connect-web`/gRPC-Web both inherit
the browser `fetch`/XHR limitation that request bodies must be fully
buffered before the request completes, so client-streaming and bidi-
streaming methods are unsupported from a browser regardless of Connect-ES
version. (`fetch`'s `duplex: 'full'` option, which would lift this, is only
experimental behind flags in some Chromium builds as of this research —
not shipped in any stable browser, and unsupported in Safari/Firefox
entirely.) **`@connectrpc/connect-node` over the gRPC transport (HTTP/2)
does support full bidirectional streaming** — this works today, with no
caveats, for a Node.js/CLI-style TS client.

**Recommendation**: prove the "at least one non-Rust client" requirement
with a **Node.js TS client using `@connectrpc/connect-node` + the gRPC
transport**, not a browser client — this is the toolchain combination with
no known gaps for `Attach`. Explicitly document (in the plan and/or README)
that a **browser-based** TS client (e.g. a future stapler-squad web
frontend, named as a consumer in requirements.md) **cannot** use `Attach`'s
bidirectional stream directly from browser JS — it would need either a
Node-side proxy/gateway or a protocol change (e.g. WebSocket bridging) for
that specific RPC. This is a real, previously-undiscovered constraint on
the "future web frontends... embedding tymux sessions" use case and should
surface as a planning-phase risk, not be discovered during Phase 5
implementation.

## 7. Release / CI (cross-platform binaries on tag push)

Standard 2026 GitHub Actions pattern for this exact use case (confirmed via
research — this is the same pattern used broadly across the Rust OSS
ecosystem, maintained by `taiki-e`, a prolific/trusted Rust tooling
maintainer):

- **`taiki-e/create-gh-release-action`** — creates the GitHub Release itself
  from a pushed tag (parses the tag, optionally generates release notes).
- **`taiki-e/upload-rust-binary-action`** — builds the Rust binary for a
  given `target` triple and uploads a `$bin-$target.tar.gz`-style archive to
  that release. Supports Linux/macOS/Windows hosts and cross-compilation to
  other targets from a single host (with documented fixes for
  cross-compiling to Windows from non-Windows runners — not relevant here
  since Windows is out of scope, but confirms active maintenance).

Typical workflow shape for tymux's exact matrix (macOS x86_64/arm64, Linux
x86_64/arm64 — 4 targets, all out-of-scope-for-cross-compilation-tricks
since GitHub-hosted `macos-latest` runners are now Apple Silicon and
`macos-13`/`ubuntu-latest` cover the rest, or `cross`/`zig`-based
cross-compilation for Linux arm64 from an x86_64 runner):

```yaml
on:
  push:
    tags: ["v*"]

jobs:
  create-release:
    runs-on: ubuntu-latest
    steps:
      - uses: taiki-e/create-gh-release-action@v1
        with: { changelog: CHANGELOG.md }  # or omit for auto notes

  upload-assets:
    needs: create-release
    strategy:
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
          - target: aarch64-unknown-linux-gnu
            os: ubuntu-latest   # cross-compiled
          - target: x86_64-apple-darwin
            os: macos-13
          - target: aarch64-apple-darwin
            os: macos-latest    # Apple Silicon runner
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: taiki-e/upload-rust-binary-action@v1
        with:
          bin: tymuxd,tymux-cli    # both workspace binaries
          target: ${{ matrix.target }}
          archive: $bin-$target
          token: ${{ secrets.GITHUB_TOKEN }}
```

**Recommendation**: this exact `create-gh-release-action` +
`upload-rust-binary-action` pairing. It directly satisfies requirements.md's
"tagged `v1.0.0`... prebuilt binaries for macOS and Linux, installable
without a Rust toolchain" success metric and the `v1.0.0-alpha.N`
pre-release checkpoint strategy in the Risk Control section (tag-triggered,
so alpha tags get the same pipeline for free). Prerequisite per
requirements.md's Constraints/Feasibility Risks: **a GitHub remote must
exist first** — there is currently none configured for this repo, so
standing that up (plus wiring the existing `.github/workflows/ci.yml` to
actually run) is a hard dependency of this whole item, not just a config
addition to an existing pipeline.

---

## Summary table

| Area | Recommendation | New crate/tool? |
|---|---|---|
| Splits/layout | Hand-rolled binary-tree `Layout` enum in `tymux-core`, flat gRPC surface | No |
| Persistence | Flat file, `serde_json` + atomic write-rename | `serde_json` (new, trivial) |
| Scrollback | Raise `SCROLLBACK_LINES`, use existing `vt100::Screen::set_scrollback`/`scrollback`/`rows` | No — existing dep already supports it |
| Status bar | ANSI scroll-region (`DECSTBM`) + reserved last row via `crossterm` | No |
| Config | `toml` + `serde` + `dirs` | Yes, 3 small crates |
| Key-bindings | Hand-rolled prefix state machine (or `keybinds` crate if table grows) | Maybe (`keybinds`) |
| TS client | `@bufbuild/protoc-gen-es` (2.0) + `@connectrpc/connect-node` (gRPC transport, Node only) | N/A (TS side) |
| Release CI | `taiki-e/create-gh-release-action` + `taiki-e/upload-rust-binary-action` | GH Actions only |
