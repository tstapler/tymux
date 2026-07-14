# ADR 0001: One window, one pane per session — deliberately, for now

## Status
Superseded by Epic 3 (v1.0 release plan) — sessions now support real
multi-window/multi-pane layouts via a binary split tree
(`crates/tymux-core/src/layout.rs`). This ADR is kept for the historical
record of why the deferral was deliberate; it no longer describes current
behavior.

## Context
`proto/tymux/v1/tymux.proto` models `Session → repeated Window → repeated Pane`,
matching tmux's real layout model. `crates/tymux-core/src/engine.rs`'s
`Engine`/`SessionState` currently hardcodes exactly one window and one pane
per session — there is no split, no second window, and no code path that
could produce more than one of either.

The `is-it-ready` architecture review (`docs/reviews/is-it-ready-2026-07-13.md`)
flagged this as a data-model mismatch worth a decision: expanding to real
multi-window/multi-pane support later means touching the engine (layout
tree, split logic), the daemon (windows/panes indexing instead of always
`[0]`), and the CLI (choosing which pane to attach to) all at once — not a
small change.

## Decision
Ship with one pane per session for now. Do not build splits/multiple windows
speculatively. The proto's `repeated` fields already accommodate this as an
additive change later — nothing about the current wire format needs to
break when splits are actually implemented.

What this decision *does* require doing now (tracked as its own fix, see
`docs/reviews/is-it-ready-2026-07-13.md` blocking issue #8): every caller
that reaches into `windows[0].panes[0]` must do so safely (bounds-checked,
clear error on empty) rather than via unchecked array indexing — so that
the current one-pane assumption fails loudly if it's ever violated, instead
of panicking.

## Consequences
- Splits/multiple windows remain a real, known limitation — documented in
  `README.md`'s Status section and `docs/ux/journey-map.md`, not hidden.
- When someone actually needs a split, the work touches three layers at
  once (engine layout logic, daemon indexing, CLI pane selection) — budget
  for that as a real feature with its own design pass, not a quick patch.
- Until then, `SessionState`'s single `pane: Arc<Pane>` field and the
  `window_id`/`pane_id` naming stay as-is; no premature abstraction for a
  layout tree that doesn't exist yet.
