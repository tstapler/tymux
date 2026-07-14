# ADR-004: Concurrent-attacher window geometry policy — smallest attached client wins

## Status
Accepted

## Context
`docs/ux/journey-map.md` Flow 6 and cross-cutting gap #6 already document
today's actual (unintentional) behavior: two clients attached to the same
pane can each independently send a `Resize`, and whichever arrives last at
`crates/tymuxd/src/main.rs:229-233` wins, with no arbitration and no
notification to the other attached client. `pitfalls.md` §1 sharpens this
for the splits epic: once panes belong to a shared window layout, a resize
is not just "last write wins" on one pane's pty size — it's a torn,
non-atomic update across sibling panes if not coordinated as one operation
(see the two-separate-mutexes bug already live in
`crates/tymux-core/src/pane.rs:155-164`).

`features.md` §1 and `architecture.md` §4 both point at tmux's own solved
answer: tmux's `window-size` option supports `smallest` (default),
`largest`, `latest`, and `manual` modes for exactly this scenario.

## Decision

Adopt tmux's `smallest`-mode policy as the v1.0 **default and only**
policy (not configurable in v1 — the other three modes are modeled in the
type system for future extensibility but not exposed as a config option
yet): a window's effective size is the **minimum**, dimension-wise, across
every currently-attached client's viewport for that window. No attached
client's view is ever clipped smaller than what it asked for; clients with
a larger terminal see unused margin (tmux fills this with a `·` filler
character — a decision for the CLI rendering task in Epic 3/6 to adopt or
adapt).

Mechanically: a client's `Resize` message sent over its `Attach` stream is
reinterpreted server-side as "this client's viewport changed," not "resize
the pty to this." The daemon tracks each attached client's last-reported
viewport per window (a small `HashMap<ClientAttachId, (u16, u16)>` scoped
to the window), recomputes the window-wide effective size as the
per-dimension minimum across all current entries whenever any entry
changes (attach, detach, or resize), and applies the result as **one
atomic recompute-and-apply-to-every-leaf operation** over the window's
`LayoutNode` (ADR-001) — never as N independent per-pane `pane.resize()`
calls racing each other.

```rust
enum WindowSizePolicy { Smallest, Largest, Latest, Manual } // v1 hardcodes Smallest
```

## Consequences
- This directly fixes the torn-resize bug class `pitfalls.md` §1 flags at
  `crates/tymuxd/src/main.rs:229-233`: a per-window lock (or single actor
  task) must own "recompute geometry and apply to all panes," making the
  two-separate-mutexes issue in `Pane::resize()` a non-issue at the
  window-coordination layer (though `Pane::resize()`'s own two-mutex
  internal race — master pty resize then vt100 parser resize — should
  still be fixed at the `Pane` level as part of this same epic, since
  splits multiply its blast radius from "one pane vs. itself" to "N sibling
  panes").
- Requires the daemon to track per-client attach state (which window, which
  viewport size) beyond what exists today — a new piece of daemon-side
  bookkeeping, not just a proto change.
- `ClientAttached`/`ClientViewportResized`/`ClientDetached` all become
  policy triggers for `RecomputeWindowGeometry`, matching the
  Event-Command-Policy table in `architecture.md` §"Event-Command-Policy
  table."

## Alternatives considered
- **Last-write-wins (today's actual, unintentional behavior)**: rejected —
  `architecture.md` §4 notes this "would fight between two attached clients
  on every resize event," which is strictly worse than doing nothing.
- **`largest`/`latest`/`manual` as the v1.0 default**: not rejected
  permanently, just deferred — modeled in the enum for a future config
  option, but `smallest` is the safer default (never clips a client) and
  matches tmux's own default, minimizing surprise for the target evaluator
  audience (`ux.md` §2).
