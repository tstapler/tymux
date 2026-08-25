# ADR-001: `AttachEvent` gains a new `output_chunk` field; `output` stays untouched

**Status**: Accepted (revised)
**Date**: 2026-08-24 (original); revised 2026-08-24 during `sdd:4-validate` pre-mortem
**Context**: attach-resume-protocol, Phase 3 planning

## Context

`AttachEvent.output` is currently `bytes output = 1;` (`proto/tymux/v1/tymux.proto:279`) — raw pty bytes with no sequence number on the wire. `Pane::output_seq` already tags every chunk internally (`crates/tymux-core/src/pane.rs:111,249`), but that tagging stops at the daemon boundary; a client has no way to know which seq an `Output` event corresponds to, and therefore no way to build a resume token from what it's already received.

Two shapes were originally considered for exposing `seq` on the wire:

1. Add a sibling field, e.g. `uint64 output_seq = 5;`, alongside the existing `bytes output = 1;`, both set together on every `Output`-carrying event.
2. Replace `bytes output = 1;` with a submessage, `OutputChunk { uint64 seq = 1; bytes data = 2; }`, on the same field number (1).

## Original decision (superseded — kept for record)

This ADR originally chose option 2: replace `bytes output` with `OutputChunk output = 1;` at the same field number, citing this repo's own `ExitStatus` precedent (`bool` → message, same field number, deliberately breaking — `project_plans/stapler-squad-integration/decisions/ADR-001-exit-status-message-shape.md`).

**That precedent does not actually transfer.** `ExitStatus`'s `bool` → `message` change swaps *wire type 0* (varint) for *wire type 2* (length-delimited) — genuinely incompatible, so an old client's decoder errors immediately and loudly. But `bytes` and an embedded `message` are **both wire type 2**. An old client (`clients/go@v0.1.0`, still pinned by `stapler-squad`'s `BackendTymux`, which this project's Constraints section requires to keep working unmodified) would successfully decode field 1's length-delimited bytes — it just wouldn't be raw pty bytes anymore. It would be `OutputChunk`'s own serialized framing (a varint tag+value for `seq`, then a length-prefixed `data` field), silently handed to code that expects literal terminal output. The result: every attached pane renders garbled/binary-looking output immediately after any `tymuxd` upgrade to this branch, with zero code change on the consumer side — a silent production break, caught only by `sdd:4-validate`'s pre-mortem (finding #1, P1), not by Epic 2.4's original backward-compat test, which only exercised server-side construction logic and never an old-generation client stub against the new wire bytes.

## Revised decision

Leave `bytes output = 1;` on `AttachEvent` completely untouched, and add `OutputChunk` as a **new, additive sibling field** instead of a same-field-number replacement:

```proto
message OutputChunk {
  uint64 seq = 1;
  bytes data = 2;
}

message AttachEvent {
  oneof payload {
    bytes output = 1;             // UNCHANGED — legacy, unseq'd, byte-identical to today
    PaneSnapshot snapshot = 2;
    ExitStatus exited = 3;
    bool output_gap = 4;
    GapExceeded gap_exceeded = 5;
    Heartbeat heartbeat = 6;
    OutputChunk output_chunk = 7; // NEW — additive, seq'd, for resume-aware clients
  }
}
```

The daemon dual-writes: every `Output`-carrying `AttachEvent` populates both `output` (legacy bytes, exactly as today) and `output_chunk` (new, `{seq, data}`) in the same send call (tymux plan.md Task 2.2.1c). Old clients read `output` and never observe `output_chunk`'s existence. New clients (`clients/ts`, `clients/go@v0.2.0+`, updated `tymux-cli`) read `output_chunk` and ignore `output`. Retiring `output` is explicit future/out-of-scope work — not attempted here, since removing it now would just reintroduce the same break for whichever clients haven't yet upgraded.

This is the standard additive/dual-write protobuf migration pattern for exactly this situation: it is genuinely non-breaking, not "breaking but field-number-tidy."

## Rationale

- **Wire-level safety, not just field-number preservation.** The revised shape is verifiable, not assumed: Epic 2.4's Task 2.4.1c adds a compat-assertion test that decodes the legacy `output` field's bytes and asserts they are exactly the raw pty chunk, with no `OutputChunk` framing leaking in — converting "old clients are unaffected" from a claim into a falsifiable check.
- **Type-driven design, preserved for the new field.** A bare sibling `seq` field (option 1, still rejected) leaves `{output_chunk_data: [...], output_chunk_seq: <unset-or-stale>}` representable-but-meaningless — nothing in proto3 enforces the two are set together. `OutputChunk` as a submessage keeps that pairing atomic, same rationale as the original decision, just applied to an additive field instead of a replacement.
- **`ExitStatus`'s precedent still stands on its own merits — it just doesn't transfer here.** `bool` → `message` is a genuine wire-type break (varint → length-delimited), so it fails loudly and immediately; that's why it was accepted as "deliberately breaking." `bytes` → `message` at the same field number shares a wire type, so it fails silently instead — a materially different risk profile that this ADR's original version conflated.
- **Field-number choice for `output_chunk` (7).** `AttachEvent`'s oneof already uses 1 (`output`), 2 (`snapshot`), 3 (`exited`), 4 (`output_gap`); this same plan separately adds `gap_exceeded` (5) and `heartbeat` (6). 7 is therefore the next free field number, confirmed against the current `proto/tymux/v1/tymux.proto`.

## Consequences

- No existing consumer of `AttachEvent.output` — `stapler-squad`'s pinned `clients/go@v0.1.0`/`BackendTymux` included — sees any wire-format change at all. It requires no code change, no `clients/go` version bump, and no coordination to remain fully functional after this feature ships.
- `clients/ts`, `clients/go@v0.2.0+`, and the updated `tymux-cli` (Epic 1.1 Story 1.1.2, Epic 5.1/5.2, Epic 6.x) read the new `output_chunk` field and never need to read `output`.
- Epic 1.2's `clients/go` tagged version bump (`v0.2.0`) is still real, coordinated work — it's what lets a consumer *opt into* resume support — but it is no longer the mechanism standing between `stapler-squad` and a wire-level production break; that break no longer exists regardless of when (or whether) `stapler-squad` bumps.
- `output`'s eventual retirement is out of scope for this project. A future breaking bump that removes it must go through the same coordinated-version-bump playbook as any other breaking `clients/go` change, at whatever point every known consumer has migrated to `output_chunk`.
