# ADR-001: `AttachEvent.exited` becomes an `ExitStatus` message, not a second field

**Status**: Accepted (amended 2026-08-21 — see Amendment below)
**Date**: 2026-08-21
**Context**: stapler-squad-integration, Phase 3 planning

## Amendment (2026-08-21)

architecture-review.md's Concerns flagged that the originally accepted `ExitStatus { bool has_code; int32 code; }` shape reintroduces, one level down, the exact illegal-state problem this ADR was written to reject: `{has_code: false, code: <garbage>}` is still representable, and every reader still has to remember to check `has_code` before trusting `code` — the Rust write site already showed the smell (`code: pane_for_exit.exit_code().unwrap_or(0)`, backfilling a garbage `0` only because the wire format demanded some value).

**Revised decision**: `message ExitStatus { optional int32 code = 1; }`, using proto3 field presence instead of a hand-maintained `has_code` boolean. "Was it set" is now tracked by the wire format itself — generated Rust (`prost`) exposes `Option<i32>` directly (`pane.exit_code()` already returns `Option<i32>`, so the write site becomes `ExitStatus { code: pane.exit_code() }`, no `unwrap_or` needed), Go exposes a pointer-with-presence, TS exposes an optional field. No new RPC, no re-litigation of the rest of this ADR — the proto had not been generated from yet, so this cost nothing beyond an edit. All `ExitStatus{has_code, code}` references below and in plan.md Tasks 1.2.1a/1.2.3a reflect this revised shape.

## Context

`ProcessManager`'s exit-status contract (stapler-squad's `session/process_manager.go`) needs an actual exit code, not just live/dead. tymux's `AttachEvent.exited` is currently `bool exited = 3;` (`proto/tymux/v1/tymux.proto`) — it signals *that* the pane exited, never *how*. `Pane`'s reader thread (`crates/tymux-core/src/pane.rs:217-241`) holds a `portable_pty::Child` handle but never calls `.wait()`, so the exit code is available and simply uncaptured.

Two shapes were considered for carrying the code over the wire:

1. Add a second field, e.g. `int32 exit_code = 5;`, alongside the existing `bool exited`.
2. Replace `bool exited` with a message, `ExitStatus { bool has_code; int32 code; }`, on the same field number (3).

## Decision

Replace `bool exited` with `ExitStatus exited = 3;`:

```proto
message ExitStatus {
  optional int32 code = 1;  // proto3 field presence: absent = code unknown
}

message AttachEvent {
  oneof payload {
    bytes output = 1;
    PaneSnapshot snapshot = 2;
    ExitStatus exited = 3;   // breaking change: bool -> message, same field number
    bool output_gap = 4;
  }
}
```

(See Amendment above — the initially accepted shape used a separate `bool has_code` field; this was revised to proto3 field presence on `code` directly before any code was generated from it.)

This is a breaking wire-format change (bool and message are not wire-compatible at the same field number) and is accepted as such — see Consequences.

## Rationale

- **Type-driven design**: a second `exit_code` field alongside `exited: bool` makes `{exited: false, exit_code: <anything>}` a representable-but-meaningless state a client must remember never to trust. A `oneof`-scoped message makes "exit code unknown" an explicit, first-class value instead of a convention — and per the Amendment above, that presence tracking lives in proto3 field presence on `code` itself, not a second hand-maintained boolean.
- **An absent `code` is a real state, not a placeholder.** A process killed by a signal, or one whose exit status `portable_pty` can't decode into a numeric code, or a `wait()` call that itself fails, all need to report "exited, but no code" — conflating that with "exited with code 0" would be a genuine correctness bug for any caller branching on exit code.
- **No new RPC.** `Attach` already delivers `Exited` exactly once, biased ahead of buffered output (`main.rs:486-505`'s `biased` `select!`) — the natural, already-correct delivery point for a one-time event. A separate `GetExitStatus` RPC would duplicate that delivery mechanism for callers who already have an open `Attach` stream, and would only be justified for the post-detach read case, which is solved instead by persisting the code onto the dead `PaneEntry` record (plan.md Story 1.2.4).
- **Same field number, deliberately.** Since this is already a breaking change (bool → message can never be wire-compatible), reusing field 3 avoids leaving a permanently-unused field number as debt.

## Consequences

- Any existing non-Rust client generated against the old `bool exited` shape breaks at the wire level. tymux has no external consumers of this proto outside this repo's own clients today (confirmed: `clients/ts/` is the only generated client, and this project is what's adding the first Go client) — so this lands as one deliberate breaking pass with no dual-read/dual-write migration needed.
- `Pane` gains a new `exit_code: Mutex<Option<i32>>` field and accessor, populated by a synchronous `_child.wait()` call in the reader thread right after the EOF `break` — additive to the existing `exited`/`exit_notify` flow, not a second detection path (see pitfalls.md §5 principle 4: exit detection must stay single-threaded through the path that already owns it).
