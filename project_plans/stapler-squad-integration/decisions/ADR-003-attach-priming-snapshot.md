# ADR-003: `Attach` sends a priming `Snapshot` before streaming live output

**Status**: Accepted (amended 2026-08-21 — see Amendment below)
**Date**: 2026-08-21
**Context**: stapler-squad-integration, Phase 3 planning

## Amendment (2026-08-21)

adversarial-review.md's Blockers identified a gap in the ordering this ADR accepted: subscribing before snapshotting (required so no output between the two is lost) means any bytes the reader thread pushes into the vt100 parser *between* `pane.subscribe()` and `pane.snapshot()` are simultaneously (a) already reflected in the snapshot's grid state and (b) queued on the just-opened broadcast receiver — so a client applying the snapshot and then forwarding that queued `Output` chunk on top double-renders it. Worst case: exactly during a `ReconnectLoop` reattach while the pane is actively producing output — the disconnect-survival scenario this whole project exists for.

**Fix (plan.md Tasks 1.3.1a/1.3.1b)**: `Pane` gains a monotonic output sequence counter, incremented under the same lock that guards vt100 parser mutation, so the counter and the grid state can never disagree. `pane.snapshot_with_seq()` reads both atomically; `attach()`'s `forward_handle` drops any `Output` event whose sequence is `<=` the snapshot's sequence before resuming normal forwarding. Still entirely server-side, still no proto change, still correct for every client language per this ADR's original rationale — the fix closes an implementation gap in that server-side mechanism, it doesn't change the decision to fix this server-side rather than client-side.

## Context

`Attach`'s handler (`crates/tymuxd/src/main.rs:444-546`) subscribes to a pane's *live* output broadcast channel and starts forwarding from that point — it does not send any priming content first. Contrast with `WatchWindow`, which explicitly emits the current layout immediately on subscribe (`main.rs:404-407`'s own comment: "so a subscriber doesn't have to wait for the next change"). Every comparable product researched (tmux/screen, mosh, VS Code Remote/Codespaces, Zellij) redraws the *current* screen state immediately on reattach — nobody expects a blank terminal that only starts rendering once new output happens to arrive. A freshly (re)attached stapler-squad xterm.js instance piping raw `Attach` bytes would render blank until the next byte of new output, even though the pane and its process are both alive — a real fidelity gap against every comparable product (ux.md §1).

Two places this could be fixed:

1. **Server-side**: `tymuxd` sends an `AttachEvent{Snapshot}` as the first message on every `Attach` call, before any live `Output`.
2. **Client-side**: `BackendTymux` (or any client) calls `CapturePane` once, renders it locally, then opens `Attach` for the live stream.

## Decision

Fix it server-side: send `AttachEvent{payload: Snapshot(pane.snapshot())}` as the first message on every `Attach` call, immediately after `pane.subscribe()` and before `forward_handle`'s loop starts forwarding live output. Subscribing before sending the snapshot is required so no output arriving between the snapshot and the first live read is lost.

## Rationale

- **Correct for every client, not just stapler-squad's.** A server-side fix keeps the "redraw current state on attach" guarantee true for the TS client, any future client language, and the CLI itself, without each one needing to independently remember to call `CapturePane` before `Attach`. A client-side-only fix would need to be re-implemented identically by every consumer of `Attach`.
- **`AttachEvent` already has a `Snapshot` variant** (`snapshot = 2` in the existing oneof) — this uses an already-defined message shape, not a new one.
- **Subscribe-then-snapshot ordering avoids a gap.** Because `pane.subscribe()` happens first, any output produced between the snapshot being taken and the subscription starting to receive is still captured by the broadcast channel and delivered as ordinary `Output` events immediately after — no window where output is silently lost between the priming read and live streaming starting.

## Consequences

- Every `Attach` call now costs one additional `pane.snapshot()` call (already an existing, cheap operation used by `CapturePane`) at the start of the stream — negligible overhead, not a scale concern.
- Any existing client (the TS examples, `tymux-cli`) that assumed `Attach`'s first message was always `Output` needs to tolerate (or explicitly handle) a leading `Snapshot` event — confirmed non-breaking for `tymux-cli`'s existing attach handling (it already renders from a `vt100`-equivalent local buffer keyed by whatever `AttachEvent` variant arrives, per its existing `Snapshot`/`Output` handling for other paths) but should be spot-checked as part of Story 1.3.1's regression test.
