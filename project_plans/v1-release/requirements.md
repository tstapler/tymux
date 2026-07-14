# Requirements: v1-release

**Date**: 2026-07-14
**Type**: feature addition (multi-epic, existing project)
**Complexity**: 4 — high-stakes / cross-cutting

## Problem Statement
tymux today (post the is-it-ready fix pass) is a working but minimal MVP: one
pane per session, no persistence, no splits, no copy-mode, no status bar, no
config system, and an unverified "cross-language client" pitch. It's not yet
something an outside developer could adopt and use the way they'd use tmux,
and it doesn't yet prove its own headline differentiator (a real non-Rust
client). This plan defines the concrete, closeable gap between the current
MVP and a public-release-quality 1.0.

## Baseline
Today: `tymux new` creates and attaches to a single pane per session (no
splits, no windows beyond the implicit one); killing `tymuxd` loses every
session with no recovery; there's no way to detach and resume without
externally killing the client; no scrollback beyond the live screen; no
status bar or visual chrome — the CLI is a raw terminal passthrough; no
config file or key-binding system exists (every keystroke goes straight to
the remote pty); only a Rust client has ever been exercised, despite
buf-managed cross-language codegen being the stated differentiator;
installation is `git clone` + `cargo build`, no tagged releases.

## Users / Consumers
- Interactive terminal users evaluating/adopting tymux as a tmux alternative
- AI coding agents driving sessions programmatically over gRPC
- Non-Rust client authors — the cross-language client requirement below
  makes this a real, not hypothetical, consumer for 1.0
- Future web frontends (e.g. stapler-squad) embedding tymux sessions

## Success Metrics
- A user can split a session into multiple panes/windows and interact with
  each independently
- Killing and restarting `tymuxd` does not lose existing sessions (exact
  durability contract pinned down in planning — see Rabbit Holes)
- A non-Rust client (generated via `buf generate` from `tymux.proto`) can
  CreateSession, Attach, and CapturePane against a running daemon — proving
  the cross-language claim with working code, not just documentation
- Scrollback history is capturable, and a human user can interactively
  navigate/copy from it
- A status bar renders current session/window state during an attached
  session
- A config file + key-binding system exists, with at minimum a working
  detach keybinding — closing the "no way to detach" gap from the journey
  map and is-it-ready report
- A tagged `v1.0.0` GitHub release exists with prebuilt binaries for macOS
  and Linux, installable without a Rust toolchain
- README's cross-language claim is demonstrated, not just asserted

## Appetite
TBD
*(No fixed deadline — open-ended personal/side-project pace. The assembled
scope below is Large regardless of the open timeline; Phase 3 planning
should sequence it into independently shippable epics/milestones rather
than one large batch, so partial progress is always usable, not a
long-lived broken branch.)*

## Constraints
- Solo developer, side-project pace — no team, no fixed deadline, but scope
  should be sequenced so any point along the way is a coherent, working
  state
- No CI currently runs for real (no git remote configured yet) — a GitHub
  remote and working CI is an implicit early dependency of "prebuilt
  binaries via GitHub Releases"
- Must not regress the is-it-ready fix pass already completed (hang fix,
  tests, logging, graceful shutdown, friendly errors, geometry sync, etc.)

## Non-functional Requirements
- **Performance SLO**: not specified — "responsive enough for interactive
  terminal use" is the only bar, no target latency/throughput
- **Scalability**: not applicable in the traditional sense — single-daemon,
  personal/small-team scale, not designed for high concurrent session counts
- **Security classification**: public (open-source, public GitHub release)
  — but the daemon's own trust boundary stays "internal/loopback" per the
  auth decision below, not "public internet facing"
- **Data residency**: no special requirements — local-only by design for 1.0

## Scope
### In Scope
- Multi-window, multi-pane sessions (real splits) — engine layout model,
  daemon indexing, CLI pane/window addressing
- Session persistence across a `tymuxd` restart (durability contract TBD in
  planning — at minimum session/pane metadata and reattachability; full
  scrollback replay is a stretch goal within this epic, not a hard
  requirement)
- Scrollback/copy-mode: history capture beyond the live screen, plus an
  interactive way for a human user to navigate and copy from it
- A status bar (session/window/pane state) rendered during attach
- A config file + key-binding system, including at minimum a working detach
  key sequence
- At least one working non-Rust client generated via `buf generate`,
  demonstrated against a running daemon (candidate: TypeScript, given it's
  the most likely stapler-squad integration path — final language choice is
  a planning-phase decision)
- CI running for real (requires a GitHub remote) + a release pipeline
  producing prebuilt binaries for macOS and Linux, tagged `v1.0.0`
- Updated README/docs reflecting the actual 1.0 feature set

### Out of Scope
- Real per-pane authentication/authorization — 1.0 stays loopback-trusted by
  design and documents this explicitly as the trust model; multi-host/
  untrusted-network use remains a documented non-goal until a concrete
  post-1.0 need exists
- Windows support — SIGWINCH handling and the PTY model are Unix-first
  already; no work planned to support Windows for 1.0
- Full tmux config-file compatibility (tmux.conf syntax) — tymux's config
  format does not need to be tmux-compatible, just functional
- Plugin/extension system
- Prebuilt Windows/ARM binaries beyond macOS + Linux x86_64/arm64 (exact
  matrix is a planning detail)

## Rabbit Holes
- **Splits**: touches three layers at once (engine layout tree, daemon
  indexing, CLI pane/window selection UX) — ADR 0001 already flagged this;
  the layout model (fixed grid vs. arbitrary binary-tree splits like real
  tmux) is a real design decision, not just "add more panes to a Vec"
- **Persistence durability contract**: "sessions survive a restart" can mean
  anything from "just remember session names/ids exist" to "replay full
  scrollback and resume the exact live pty state." These have wildly
  different implementation costs — the latter likely isn't even fully
  achievable, since a live OS process can't be serialized/resumed across a
  daemon restart without something like CRIU. Planning must pin down
  exactly what's preserved vs. lost.
- **Copy-mode navigation**: requires the CLI to intercept keystrokes locally
  (mode-switching) rather than the current pure passthrough model — this is
  the same underlying gap as "no detach key" and should likely be solved
  together via the config/key-binding epic
- **Status bar rendering**: requires composing UI chrome around the raw pty
  output stream in the CLI, which today just writes bytes straight to
  stdout — needs a real terminal-rendering approach (partial-screen redraw
  without corrupting the pty's own output) or it will visually corrupt
  output
- **Config/key-binding system**: doesn't exist at all today (raw byte
  passthrough — see journey map's "no detach" and "every byte forwarded"
  findings) — this is greenfield design work, not an extension of something
  existing
- **Cross-language client**: `buf.gen.yaml` currently has zero plugins
  configured; picking and validating a real codegen toolchain (e.g.
  `protoc-gen-es` + `@connectrpc/connect` for TS) for the streaming
  `Attach` RPC specifically is unproven — bidirectional-streaming client
  codegen is more likely to have rough edges than simple unary RPCs

## Alternatives Considered
- Scoping 1.0 down to "dogfood-ready" (single-pane, no persistence, no
  auth) was the lower-effort path but was explicitly rejected in favor of
  public-release quality
- tmux-config-file syntax compatibility was considered and rejected as
  unnecessary scope — a functional config system doesn't require syntax
  compatibility with an unrelated tool

## Feasibility Risks
- True live-process persistence across a daemon restart may not be fully
  achievable without OS-level process checkpointing (e.g. CRIU on Linux, no
  real equivalent on macOS) — the realistic contract is likely "metadata
  survives, live output/scrollback since last capture may not," which needs
  to be decided explicitly, not discovered late
- Bidirectional-streaming RPC codegen quality for non-Rust targets varies a
  lot by language/toolchain — the "at least one non-Rust client" epic
  carries real discovery risk on which language/toolchain actually works
  cleanly for `Attach`
- No GitHub remote exists yet — setting one up, and standing up real CI/
  release automation, is a prerequisite for the distribution goal and
  hasn't been attempted at all in this project yet

## Observability Requirements
`tracing` + `tracing-subscriber` (RUST_LOG-driven) is already in place
across the daemon's session/attach lifecycle (per the is-it-ready fix
pass). For 1.0, extend this to: persistence load/save operations (so
corrupted or lost persisted state is visible, not silent), and status/
health signals a user could plausibly want when running tymuxd as a
long-lived personal daemon (e.g. session count, uptime) — exact mechanism
(structured log lines vs. a status RPC) is a planning decision.

## Risk Control
No feature-flag or staged-rollout infrastructure exists or is planned
(single-daemon personal tool, not a fleet). Risk control instead means:
sequence the epics below so master stays buildable and passing CI after
every merge, tag `v1.0.0-alpha.N` pre-releases as major epics land (splits,
persistence, cross-lang client, etc.) so there's always an installable,
working checkpoint, and only cut the real `v1.0.0` tag once every in-scope
item above is verified working end-to-end.

## Open Questions
- Exact persistence durability contract (see Rabbit Holes) — needs to be
  pinned down in Phase 3 planning before implementation starts
- Layout model for splits: fixed grid vs. arbitrary tree (tmux's actual
  model) — architecture decision for planning
- Target language for the first non-Rust client (TypeScript is the leading
  candidate given stapler-squad, but not yet confirmed)
- Exact config file format/location and key-binding syntax
- Release binary matrix: which OS/arch combinations beyond the macOS +
  Linux baseline
