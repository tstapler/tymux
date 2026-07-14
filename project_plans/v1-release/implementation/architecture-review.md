# Architecture Re-Review: v1-release

**Original verdict**: BLOCKED (2 blockers, 6 concerns, 3 nitpicks)
**New verdict**: CONCERNS (0 blockers — both original blockers resolved; several concerns remain open, informational only)

## Blocker 1 (structural validation): RESOLVED

Story 4.1's title itself now reads "`PersistedSessionRecord` schema + versioning +
structural validation + `PersistenceBackend` trait", and AC3 is new: "Given a
persisted file with a *current* `schema_version` but a structurally malformed
`PersistedLayoutNode`... `PersistedLayoutNode::validate_structure()`... returns an
`Err` naming the specific violated invariant... must never be allowed to
deserialize successfully and silently reach `compute_geometry` or
`ReviveSession`'s respawn walk" — this is a direct, explicit reference to the
original Blocker #1 text.

Story 4.1 Task 4 implements `PersistedLayoutNode::validate_structure() -> Result<()>`,
"reusing the exact same three invariants Story 3.2's proptest suite already checks
on live `LayoutNode` (exactly 2 children per `Split`, ratios sum to ~1.0 within
tolerance, no zero/one-child `Split`)" and wires it into
`PersistedSessionRecord`'s own validation path so version and structure are
checked together.

Story 4.3 (retitled "Startup reconciliation: dead-flagged load, corrupt-file
skip, **structural-invalid-file skip**") Task 1 calls
`PersistedLayoutNode::validate_structure()` from
`FsPersistenceBackend::load_all` on every window's layout in the record, with
"the identical log-and-skip-and-keep-starting treatment as a version mismatch."
AC2 states explicitly that a structurally-invalid record "must never reach
`Engine`'s session map or be fed to `compute_geometry`/`ReviveSession`," and Task
4 adds an integration test writing a structurally-invalid fixture (3-child
`Split`, current `schema_version`, valid JSON) alongside corrupt-JSON and valid
fixtures, asserting both are skipped non-fatally.

This is exactly the remediation the original review requested — verdict:
**RESOLVED**.

## Blocker 2 (PersistenceBackend trait): RESOLVED

Story 4.1 Task 5 now defines
`trait PersistenceBackend: Send + Sync { fn save(&self, record: &PersistedSessionRecord) -> Result<()>; fn load_all(&self) -> Vec<PersistedSessionRecord>; }`
in `crates/tymux-core/src/persistence.rs`, with `FsPersistenceBackend` as the
concrete production implementation (fleshed out in Story 4.2), and states
`Engine` "is refactored to hold a `Box<dyn PersistenceBackend>` (or be generic
over `PersistenceBackend`) rather than a concrete filesystem type... resolving
the Dependency Inversion gap flagged in architecture-review.md Blocker #2" —
citing the original finding directly.

Story 4.2 Task 4 ("Concurrency regression test per AC2") now implements
`SlowMockPersistenceBackend: PersistenceBackend`, explicitly described as "a
real, substitutable implementation of Story 4.1's trait, not an untyped
aspiration," whose `save()` sleeps before returning; it is injected into a test
`Engine` to assert other `Engine` operations are not blocked while it "saves."
This makes Story 4.2's AC2 testability claim structurally achievable from the
types as specified, closing the Dependency Inversion violation.

Verdict: **RESOLVED**.

## New issues introduced by the fix

- **Storage shape for the backend is left undecided.** Story 4.1 Task 5 says
  `Engine` holds "`Box<dyn PersistenceBackend>` (or be generic over it)" —
  the plan doesn't commit to one. This matters concretely: if `Engine` is
  shared across concurrent gRPC handlers via `Arc<Engine>` / needs to be
  `Clone`, `Box<dyn PersistenceBackend>` won't derive `Clone` the way
  `Arc<dyn PersistenceBackend>` would. Nothing else in the plan specifies how
  `Engine` itself is shared/cloned across the tonic service (no `Arc<Mutex<Engine>>`
  or similar language appears anywhere in plan.md), so this can't be called a
  concrete defect — but it's a decision the fix deferred rather than resolved,
  and worth pinning down (pick one) before Story 4.1 implementation starts
  rather than discovering a `Clone` requirement mid-Story-4.2.
- **`load_all()`'s error shape has no room for directory-level failure.**
  `load_all(&self) -> Vec<PersistedSessionRecord>` (no `Result`) is a reasonable
  choice for *per-file* corruption (skip-and-log, matching AC1/AC2's partial-
  failure semantics), but it gives no channel to distinguish "some files were
  corrupt" from "the sessions directory itself couldn't be read" (permissions,
  disk error, etc.) — Story 4.3 Task 3 only discusses `$XDG_STATE_HOME`
  resolution "defaulting sensibly if unset," not what happens if the directory
  exists but enumeration fails outright. Minor — not the same severity class as
  the original two blockers, but worth a one-line acceptance criterion if not
  already implicitly "daemon still starts, logs the directory-level error too."

No other structural gaps found in the new trait design; method signatures
(`save`/`load_all`), the `Send + Sync` bound, and the mock/production dual-impl
shape are consistent with how the plan already treats `Pane::resize()` as a
substitutable collaborator for Story 3.4's analogous concurrency test (Task 8).

## Remaining concerns (informational, not blocking)

- **StatusBarModel wire shape — RESOLVED.** Now explicitly listed as item 9 in
  §6 Unresolved Questions ("flagged here explicitly per the architecture
  review so Phase 4 validation resolves it deliberately").
- **ADR-006 two mechanisms sharing a name — RESOLVED.** Story 3.4's new
  locking-discipline note explicitly calls window-resize's lock-release-around-
  syscalls shape "the second named canonical single-owner-writer shape
  (alongside persistence's)," i.e. the plan now names persistence and
  window-resize as sharing one canonical shape (lock-scoped
  snapshot-then-release), distinct from CLI stdout's channel/task shape
  (Story 6.3) — this is exactly the "name the two canonical shapes explicitly"
  remediation requested.
- **"No orphaned pane references" invariant — STILL OPEN.** Story 3.2's AC3 and
  property-test suite (Task 5) still only cover ratio-sum, min-size floor, and
  no-zero/one-child-`Split` at the `LayoutNode` level; no `Engine`-level
  integration test asserting bidirectional `LayoutNode` ↔ `Engine.panes`
  consistency was added to Story 3.4 or elsewhere.
- **Cross-cutting `Engine` SRP — unchanged/informational**, as originally
  noted no action was required for v1.0; still holds session lifecycle,
  geometry recompute, and persistence sequencing on one struct.
- **`TargetString` parse-vs-resolve conflation (Story 3.5) — STILL OPEN.** The
  Domain Glossary entry and Story 3.5 Task 1 wording are materially unchanged;
  no concrete `TargetString::parse(&str) -> Result<...>` vs. separate-resolver
  signature split appears in the plan.
- **`AttachEvent.payload` wildcard-arm convention (Epic 2, Story 2.2 Task 3) —
  STILL OPEN.** The one `OutputGap` wildcard instance is fixed, but no
  going-forward "no `_ => {}` on this oneof" convention was added to Epic 2's
  goal text or the Observability Plan.

Nitpicks (`ScrollbackOffset` public field, `WindowSizePolicy` unimplemented
variants, `Liveness` prost fallback) were not targeted by the fix pass and
remain as originally noted — none are blocking.
