# Roadmap: tymux

**Date**: 2026-08-23
**Type**: outcome-based roadmap (post-v1.0)

## Vision

tmux's session/window/pane model, but every human affordance — split,
detach, copy, kill — has a typed RPC underneath it, so an AI coding agent
or a web frontend can drive a multiplexer as reliably as a human at a
terminal. v1.0 (shipped, tag `v1.0.0`) proved the model works end-to-end:
splits, Tier-0 persistence, copy-mode, a status bar, config/keybindings,
and a real non-Rust client. What's next is making that model **robust**
enough to trust with real, unattended workloads (agents, hosted
multi-user backends) and closing the remaining gap with tmux/zellij/wezterm
for the parts of the surface v1.0 deliberately deferred.

## Now (Committed — current focus)

This is already fully scoped in
[`project_plans/stapler-squad-integration/implementation/plan.md`](../stapler-squad-integration/implementation/plan.md)
— not a new proposal, restated here so the roadmap is one place to look.

### Outcome: tymux is a viable tmux replacement for stapler-squad's process backend, and panes survive a real network failure

- `BackendTymux` — stapler-squad's `ProcessManager` interface implemented
  against tymux's gRPC API (Epics 2.1–2.6)
- The abrupt-disconnect pane-kill bug — `setsid()` fix is implemented
  (`crates/tymuxd/src/main.rs`) but **unverified against the actual bug**:
  every sandboxed dev container this project builds in already lacks the
  controlling-terminal precondition the fix targets, so the fix can be
  neither confirmed nor refuted there (confirmed by re-running the repro
  with the fix compiled in — it still failed, for exactly this reason; see
  `crates/tymux-e2e/tests/disconnect_survival_e2e.rs`'s doc comment).
  **This is the one blocking, non-delegatable step** — it needs a human
  with real hardware, per
  [`docs/runbooks/disconnect-survival-verification.md`](../../docs/runbooks/disconnect-survival-verification.md).
- Exit-status reporting, `cwd` proto fields, Go client generation, and a
  1,000-concurrent-session load test (Epics 1.2–1.7) — load test already
  ran once during research and found a fixable O(n) lock-contention bug,
  not an architecture problem (`requirements.md`'s Complexity note)
- Claude Code end-to-end validation and disconnect-survival re-verification
  in the fully integrated stack (Epic 3)

## Next (Planned — high confidence, not yet started)

### Outcome: an abrupt network drop never loses state, for *any* client, not just stapler-squad

The Now milestone fixes one instance of this bug for one consumer. This
generalizes the fix into a protocol-level guarantee, since the underlying
gap — the `Attach` stream has no resume mechanism — will resurface for the
next client the moment it deals with a flaky connection instead of a
clean unit test.

- **Resume token + sequence numbers on `Attach`.** On reattach, the client
  sends the last sequence number it applied; the daemon replays only the
  gap from the per-pane broadcast buffer's retained history, falling back
  to a full `CapturePane` snapshot once the gap exceeds retention. This is
  the same shape `output_gap` already signals, made resumable instead of
  just detectable — mirrors Eternal Terminal's `BackedReader`/`BackedWriter`
  byte-sequence replay and mosh's State Synchronization Protocol fallback
  for "too far behind, resync to current state."
- **Per-subscriber cursor, not one shared gap bit.** Multiple clients can
  attach to one pane's broadcast channel; each needs its own reconnect
  cursor so one client's catch-up doesn't affect another's live stream.
- **Heartbeat/keepalive on `Attach` + a configurable grace period before
  any disconnect-triggered cleanup.** Distinguishes "client slow" from
  "client actually gone" within a bounded window, and — mirroring zellij's
  explicit `Active → ActiveDetached → Killed` state machine — makes
  "orphaned but still running" a first-class, intentional state instead of
  the ad hoc thing the Now milestone is fixing one bug at a time.

**Why now, not later**: this is tmux's entire value proposition
(disconnect ≠ death), it's the exact class of bug currently blocking the
stapler-squad milestone, and the fix generalizes cleanly from work already
in flight.

### Outcome: `tymuxd` can be exposed beyond loopback without being an open door

Currently a hard blocker, not a nice-to-have: the daemon is loopback-only
by convention, not enforcement — it warns but doesn't refuse a
non-loopback bind, and any client that reaches the port can attach to or
kill any pane with zero ownership check. Every hosted/multi-tenant use
case (a shared `tymuxd` behind stapler-squad, or any future web frontend)
is blocked on this.

- **Bearer-token RPC auth** via a gRPC interceptor, required on every
  `TymuxService` call — closes "any client on the port controls every
  pane" outright.
- **Per-session/pane ownership** (`created_by`/owner field) with an authz
  check on mutating RPCs (kill, resize, input). None of the prior-art tools
  surveyed solve this well: tmux relies on OS file permissions on the
  Unix socket, mosh/ET/tmate treat a successful handshake as full control
  with no per-resource ACL. This is a real gap tymux should not inherit.
- **Scoped tokens** (read-only vs. read-write attach), tmate's link-scoping
  idea reused as a verifiable token claim instead of an unguessable URL —
  useful for pairing/demo/observability without granting kill/input rights.

## Later (Exploring — lower confidence, needs research or a concrete consumer)

### Outcome: tymux closes the interactive-feature gap with tmux/zellij for daily-driver human use

None of these are blocked on anything above; they're sequenced after
robustness/auth because tymux's stated primary audience is agent/API-driven,
not daily-driver human replacement of tmux (README's own framing) — but
they're real, well-understood gaps against the tools tymux is compared to:

- Mouse support (pane select/resize, scroll-to-copy-mode) — table stakes
  in tmux, zellij, and wezterm; tymux currently has none
- OSC 52 clipboard passthrough — real OS clipboard access over a remote
  session, the same problem tmux solves for SSH usage
- Preset layouts (`even-horizontal`, `tiled`) as one-shot CLI recipes over
  the *existing* binary split tree — gets most of tmux's preset-layout
  value without an N-ary tree rewrite (see Parking Lot)
- `synchronize-panes` (broadcast input to every pane in a window),
  `swap-pane`
- Copy-mode depth: rectangle/block selection, named buffers + picker

### Outcome: lifecycle events are consumable by any typed client, not just polled

- A `SubscribeEvents` streaming RPC — pane-exit, session-create,
  window-close, output-idle — as first-class typed events, beating tmux's
  `set-hook` (which only runs local shell strings) and zellij's WASM
  plugin system's heavier sandboxed-runtime cost.
- A thin webhook/exec dispatcher built as an ordinary RPC client on top of
  the above, for tmux-plugin-parity use cases (e.g., desktop notification
  on pane exit) — without baking process-spawning into the daemon core.
- Deliberately **no sandboxed plugin runtime (WASM)** planned: tymux's own
  differentiator — every action is already a typed RPC in any language —
  *is* the extension model. A client-side process hitting the gRPC API is
  the plugin. Revisit only if in-process rendering (zellij-style plugin
  UI panes) becomes an actual requirement.

### Outcome: agent-driven clients get atomic, low-latency multi-step setup

Directly serves tymux's stated differentiator (agent/programmatic control)
more than any other item on this roadmap, but needs a concrete consumer
(stapler-squad or another agent client) to pin the exact shape before
committing to a design:

- Batched/atomic RPC (e.g., session + N splits + N commands in one call)
  — cuts round-trips and partial-failure surface for a scripted agent
  building a complex layout
- Structured error codes for both the CLI and programmatic callers,
  replacing today's raw `anyhow` Debug-dump errors
- Session-resume hints for known interactive tools — the
  `tmux-assistant-resurrect` precedent (capture a `claude --resume <id>`-
  style flag so a revived pane resumes an agent's context instead of
  relaunching blank) is a proven middle tier between "just re-run the
  command" and true process checkpointing, and lands squarely on tymux's
  own stated audience

### Outcome: tymux works for people, not just processes, in whatever form factor they're in

- Browser `Attach` support (existing known limitation) — needed for any
  web frontend that isn't proxied through a backend like stapler-squad's
  Go service
- Screen-reader-aware navigation for multi-pane windows (existing known
  limitation)
- mTLS for daemon-to-daemon / multi-host scenarios, layered on the
  bearer-token work above once it exists

### Outcome: `tymuxd` checkpoints and restores a session's actual live process state, not just relaunches it (Linux)

**Promoted from Parking Lot 2026-08-23 at Tyler's explicit request** —
overrides `ADR-002-persistence-durability-tiers.md`'s "rejected outright"
call on Tier 2. The ADR's actual objection was an *asymmetric platform
guarantee* (works on Linux, not macOS, for something framed as a core
promise); that's resolved here by scoping this as an explicit, clearly-
labeled Linux-only opt-in layered **on top of** the existing Tier 0/1
baseline, not a replacement for it — Tier 0 (metadata survives, done) and
Tier 1 (auto-relaunch + resume hints, Later above) still apply
uniformly everywhere; Tier 2 is additive and Linux-only.

- CRIU-based checkpoint/restore of a pane's process tree — real live
  state (not just "same command line, re-run"), on demand
  (`tymux checkpoint <session>`) or automatically on daemon
  shutdown/restart
- Real constraints to spike before committing further: CRIU needs
  `CONFIG_CHECKPOINT_RESTORE` and specific capabilities, and is fragile
  around certain fd/socket types — a pty/tty fd (every pane's actual fd
  type) needs to be confirmed checkpoint-restorable with CRIU's pty
  plugin before this is more than a research spike, not assumed
- A pane that can't be checkpointed (unsupported fd types, missing kernel
  config, missing capabilities, or running on macOS at all) must fail
  loudly and fall back to Tier 1's relaunch behavior — never silently
  drop the checkpoint request
- Confidence: low — this is a research spike first (validate CRIU
  actually round-trips a `portable-pty`-owned pty fd at all), then a
  design, before any implementation estimate is credible

### Outcome: tymux config accepts your existing `tmux.conf`

**Promoted from Parking Lot 2026-08-23 at Tyler's explicit request** —
overrides `v1-release/requirements.md`'s Alternatives-Considered call
("unnecessary scope... a functional config system doesn't require syntax
compatibility with an unrelated tool"). That reasoning was about
*inventing* a new config format under a compatibility constraint; this is
narrower — accepting an *existing* file as input, which doesn't force
tymux's own native TOML format to change.

- Parse a defined subset of `tmux.conf` grammar (`bind-key`,
  `unbind-key`, `set-option`) and translate recognized directives into
  tymux's native TOML keybinding/config model — either at load time or
  via a one-shot `tymux import-tmux-conf` conversion command that
  produces a real, editable `config.toml`
- Explicit scope boundary: translate only the bindable-action subset
  that already exists in tymux (`BINDABLE_ACTIONS` in
  `crates/tymux-cli/src/config.rs`) — not tmux's full command language
  (no `run-shell`, no `if-shell`, no plugin directives, since tymux has
  no shell-interpolation or plugin execution model to translate them
  into)
- An unrecognized directive gets a clear "not supported, skipped at line
  N" warning at load/import time, never a silent parse failure or a
  crash on an unfamiliar `.tmux.conf`
- Confidence: medium — the parsing surface is bounded (a real grammar,
  not full tmux scripting) and the target (tymux's own existing
  keybinding model) already exists; the main open question is exact
  scope of the `set-option` subset worth translating

## Parking Lot (Captured but not prioritized)

- **N-ary (non-binary) layout tree rearchitecture** — the binary tree was
  a deliberate v1 decision (`ADR-001-layout-tree-data-structure.md`);
  preset layouts (Later, above) capture most of tmux's N-ary value without
  the resize/collapse complexity of a rewrite.
- **`join-pane`/`break-pane`** (move a pane between windows) — a real tmux
  feature with no evidence of demand yet; revisit if it's actually
  requested.

## Roadmap Rules

- Now is fully planned elsewhere (`stapler-squad-integration`) and is
  restated here, not re-planned.
- Next items are sequenced ahead of Later because they're either
  generalizing a bug already being fixed (disconnect resume) or unblocking
  an entire class of deployment (auth) that several Later/Parking-Lot items
  implicitly assume exists (mTLS, hosted multi-user).
- Later items with no concrete consumer stay in Later, not Next, even when
  research confidence is high — a design done ahead of a real caller tends
  to need redoing once one shows up (see the batched-RPC and event-hook
  items above, both flagged as needing a consumer to pin shape).
- Each Next/Later outcome should get its own `/sdd:full` (or `/sdd:quick`
  for the smaller ones) pass when picked up, producing its own
  `project_plans/<name>/` the way `v1-release` and
  `stapler-squad-integration` did.
- A prior ADR/requirements rejection isn't permanent — it's a decision
  made with the information available at the time. Tyler overriding one
  directly (CRIU-based Tier 2, `tmux.conf` import — both 2026-08-23) moves
  the item back onto the roadmap; the original doc stays as the record of
  *why* it was rejected then, not as a veto now.

## Sources

Internal (already in this repo, not re-derived):
- `README.md` — current v1.0 status, known limitations
- `project_plans/v1-release/research/features.md` — tmux/zellij/wezterm
  splits, persistence-tier, copy-mode, status-bar, config prior art;
  tmux-resurrect/continuum/tmux-assistant-resurrect analysis
- `project_plans/stapler-squad-integration/requirements.md`,
  `implementation/plan.md` — Now milestone's epic breakdown and status
- `project_plans/stapler-squad-integration/research/features.md` — et/mosh/
  zellij/ttyd prior art gathered during that project's own research phase
- `docs/runbooks/disconnect-survival-verification.md` — root cause and
  verification status of the Now milestone's blocking bug
- `docs/adr/0001-single-pane-per-session-for-now.md`,
  `project_plans/v1-release/decisions/ADR-001-layout-tree-data-structure.md`
  — binary-tree decision behind the Parking Lot's N-ary item

External (gathered for this roadmap):
- tmux client/detach model — [`tmux/client.c`](https://github.com/tmux/tmux/blob/master/client.c)
- mosh State Synchronization Protocol — [USENIX ATC '12 paper](https://www.usenix.org/conference/atc12/technical-sessions/presentation/winstein)
- Eternal Terminal session resumption — [eternalterminal.dev/howitworks](https://eternalterminal.dev/howitworks/)
- zellij session state machine and resurrection — [Session Resurrection docs](https://zellij.dev/documentation/session-resurrection.html)
- gRPC bidi-stream reconnection practice — [websocket.org reconnection guide](https://websocket.org/guides/reconnection/), [oneuptime gRPC bidi guide](https://oneuptime.com/blog/post/2026-01-24-grpc-bidirectional-streaming/view)
- tmate relay auth model — [tmate.io](https://tmate.io/), [linuxhandbook.com/tmate](https://linuxhandbook.com/tmate/)
- wezterm SSH/TLS multiplexer domains — [wezterm.org/multiplexing](https://wezterm.org/multiplexing.html), [wezterm ssh.md](https://github.com/wezterm/wezterm/blob/main/docs/ssh.md)
- tmux TPM plugin convention — [tmux-plugins/tpm](https://github.com/tmux-plugins/tpm)
- zellij WASM/WASI plugin system and permissions — [Plugin System](https://zellij.dev/documentation/plugins.html), [Plugin API Permissions](https://zellij.dev/documentation/plugin-api-permissions.html)
- wezterm Lua plugin/event model — [Plugins](https://wezterm.org/config/plugins.html), [format-tab-title](https://wezterm.org/config/lua/window-events/format-tab-title.html)
