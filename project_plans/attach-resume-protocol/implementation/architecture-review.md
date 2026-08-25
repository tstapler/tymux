# Architecture Review: attach-resume-protocol
**Date**: 2026-08-24
**Verdict**: APPROVED (re-reviewed 2026-08-24 — both original Blockers resolved; both original Concerns also resolved in this pass — see notes below. 0 Blockers, 0 Concerns, 3 Nitpicks (informational only, no action required) remain.)

## Constitution Check

`docs/adr/ADR-000-architecture-constitution.md` does not exist in this repository
(`docs/adr/` contains only `0001-single-pane-per-session-for-now.md` and
`0002-musl-pty-spike-result.md`). No constitution to check against — skipped.

## Blockers

- [x] **RESOLVED (re-reviewed 2026-08-24). Task 2.1.1d (`ReplayBuffer` byte-budget allocation)** — mirroring
  `allocate_scrollback_budget` "exactly" copies a soft-ceiling formula into a
  component whose own NFR explicitly forbids that shape.
  [`crates/tymux-core/src/pane.rs:44-50`](../../../crates/tymux-core/src/pane.rs#L44-L50):
  ```rust
  fn allocate_scrollback_budget() -> usize {
      let used = GLOBAL_SCROLLBACK_USED_LINES.load(Ordering::Relaxed);
      let remaining = GLOBAL_SCROLLBACK_BUDGET_LINES.saturating_sub(used);
      let granted = DEFAULT_SCROLLBACK_LINES.min(remaining.max(MIN_SCROLLBACK_LINES));
      GLOBAL_SCROLLBACK_USED_LINES.fetch_add(granted, Ordering::Relaxed);
      granted
  }
  ```
  Traced independently: once `used >= GLOBAL_SCROLLBACK_BUDGET_LINES`, `remaining`
  saturates at `0`. `remaining.max(MIN)` then evaluates to `MIN`, so `granted =
  DEFAULT.min(MIN) = MIN` — never `0`. `GLOBAL_SCROLLBACK_USED_LINES` is bumped by
  `MIN` on every subsequent call regardless of how far over budget it already is.
  The "global ceiling" is therefore soft: it never actually stops growing, it just
  slows to `+MIN` per pane, forever. The adversarial reviewer's claim is
  confirmed by reading the code, not inferred.

  Applied to the replay buffer with the plan's stated constants
  (`DEFAULT_REPLAY_BUFFER_BYTES=256KiB`, `MIN=16KiB`,
  `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES=64MiB`): the first ~256 panes exhaust the
  64MiB ceiling at the 256KiB default rate, then every pane after that still
  gets a 16KiB floor grant, unboundedly, for as long as panes keep spawning —
  there is no pane-count cap anywhere in this codebase (`grep -rn "MAX_PANES"`
  and similar returns nothing in `crates/tymux-core`, `crates/tymuxd`). At the
  requirements doc's own cited "1,000-session load-test precedent," the 64MiB
  ceiling would already be exceeded by roughly (1000-256)×16KiB ≈ 11.6MiB
  (~18%), and it keeps growing linearly with pane count past that, not
  asymptotically bounded.

  **Independent verdict, not deference to the adversarial review**: copying this
  formula is not "consistency over a pre-existing minor flaw" here — it is a
  direct violation of this plan's own NFR ("must not risk unbounded memory
  growth under many concurrent panes," requirements.md Non-functional
  Requirements). The floor-grant rationale that justifies the *scrollback*
  version ("a pane with zero history defeats the point of copy-mode entirely,"
  `pane.rs:16-18`) does not transfer to the replay buffer at all: this feature
  already has a safe, designed-in degradation path for "no replay available" —
  `ReplayOutcome::GapExceeded` triggering the existing `CapturePane` snapshot
  fallback (Epic 2.2.2). A pane that gets `0` bytes of replay budget once the
  global ceiling is truly exhausted just always resumes via full snapshot
  instead of incremental replay — correct, already-planned-for behavior, not a
  degraded experience the way zero scrollback would be. There is no reason to
  inherit the floor here.

  **Remediation**: `allocate_replay_budget` must not use `remaining.max(MIN)`.
  Use `DEFAULT.min(remaining)` (no floor), or explicitly `if remaining == 0 {
  return 0 }` before applying the floor to any nonzero remainder. Either makes
  `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES` an actual hard ceiling, consistent with
  the NFR this plan itself states. Separately, `allocate_scrollback_budget`'s
  existing soft-ceiling bug is a legitimate finding but is out of scope for
  this plan to fix — flag it for a follow-up, don't silently perpetuate it into
  new code that has a stricter stated requirement.

  **Re-review (2026-08-24): confirmed resolved.** Independently re-read
  [`crates/tymux-core/src/pane.rs:44-50`](../../../crates/tymux-core/src/pane.rs#L44-L50)
  — `allocate_scrollback_budget` is unchanged from what's quoted above (its
  soft-ceiling shape is intentionally left as-is per the remediation note;
  correctly flagged as out of scope rather than silently fixed by copying a
  different formula elsewhere). The plan's new Task 2.1.1d now specifies
  `allocate_replay_budget()` as `DEFAULT_REPLAY_BUFFER_BYTES.min(remaining)`
  with **no** `.max(MIN)` term at all — exactly the first remediation option
  offered above, not a variant with a hidden floor reintroduced elsewhere. The
  "genuine hard cap, can return 0" framing is now consistent across every
  place the plan describes this function: the Domain Glossary entry, the
  Pattern Decisions table, Story 2.1.1's AC (with an explicit Given/When/Then
  for the exhausted-budget/`0`-bytes case), and Task 2.1.1d's full text. No
  contradiction found anywhere else in the plan.

  **Independent architectural verdict on the hard-cap design itself** (not
  just "matches the requested edit"): this is the correct fix and introduces
  no new problem. Task 2.1.1a's `push` is specified as "evict-from-front-
  until-under-budget" against a `VecDeque<(u64, Vec<u8>)>` — with
  `budget_bytes = 0` this just means every `push` evicts its own just-pushed
  entry immediately (or evicts everything already present down to empty
  before the loop exits), which is standard bounded-queue behavior with no
  special-casing needed for zero capacity: no division, no modulo, no
  fixed-capacity allocation that could panic on `0`, and no unbounded
  loop (`total_bytes` strictly decreases by each popped entry's size until it
  reaches `0 <= budget_bytes`). A `0`-byte pane's `replay_since` therefore
  degrades safely into `GapExceeded` for any resume request that actually
  needs replayed history, which Epic 2.2.2's already-planned fallback
  (full `CapturePane` snapshot) already handles — not a new failure mode.
  One minor imprecision worth flagging (not a blocker): Task 2.1.1d's and
  Story 2.1.1's AC both say a `0`-budget pane's `replay_since()` calls
  "always" return `GapExceeded`, but per `replay_since`'s own boundary
  convention (Task 2.1.1b) and Task 2.1.1c's own "empty-buffer-fresh-pane
  succeeds trivially" test case, `resume_from_seq == latest_seq` (no data was
  ever missed) legitimately returns `InWindow { chunks: [], tail_seq }` even
  at `budget_bytes = 0` — that's correct behavior, not a bug, and it's the
  same edge case any non-zero-budget buffer would also handle this way, so
  it doesn't reopen this blocker; it's just a wording nit in the AC's
  "always" phrasing that a future editing pass could tighten.

- [x] **RESOLVED (re-reviewed 2026-08-24). Story 2.2.1 vs. Task 1.1.1b — contradictory instructions on
  `forward_step_for_output_result`.** Task 1.1.1b: "Regenerate Rust codegen,
  fix compile fallout (placeholder `seq:0`, real logic deferred to Epic 2.2)."
  Story 2.2.1: "`forward_step_for_output_result` itself UNMODIFIED."
  [`crates/tymuxd/src/main.rs:325-349`](../../../crates/tymuxd/src/main.rs#L325-L349)
  is the *only* production call site that constructs
  `attach_event::Payload::Output` (confirmed via `grep -n
  "Payload::Output" crates/tymuxd/src/main.rs` — one non-test hit, line 337).
  After Epic 1.1's proto change, line 337 must become
  `Output(OutputChunk { seq, data: bytes })` — the real `seq` is already in
  scope in that match arm (`Ok((seq, bytes))`), so wiring it in is a two-line
  change to exactly this function. If Epic 2.2 is executed literally as
  written ("UNMODIFIED"), the placeholder `seq: 0` from Epic 1.1 ships to
  production permanently: every `OutputChunk` on the wire reports seq 0,
  resume tokens built from it are meaningless, and the entire feature is
  silently broken while every test that doesn't check the actual seq value
  keeps passing. Under `subagent-driven-development` this is worse than a
  normal ambiguity: Epic 1.1 and Epic 2.2 are separate stories likely
  implemented by separate fresh subagents, neither of which sees this
  contradiction unless it's fixed in the plan itself.
  **Remediation**: correct Story 2.2.1 to state that Epic 1.1 wires the real
  seq into `forward_step_for_output_result` directly (it already has `seq` in
  scope — no new plumbing needed, "placeholder" was never actually necessary),
  and Epic 2.2's "UNMODIFIED" claim should refer only to the `Lagged`/`Closed`
  arms and the `Skip`-on-priming-dedup branch, not the `Ok` arm's `Output`
  construction. Add an explicit test asserting a non-zero seq round-trips
  through `forward_step_for_output_result` to make this a compile/test-time
  guarantee, not a doc-comment promise.

  **Re-review (2026-08-24): confirmed resolved.** `grep -in
  "unmodified\|forward_step_for_output_result"` over the current plan.md
  shows no remaining "unmodified"/"unchanged" claim attached to this
  function's `Output`-payload construction — the plan now precisely scopes
  what is and isn't touched, matching the remediation's suggested split:
  - Epic 2.2's Goal now reads "reusing `forward_step_for_output_result`'s
    skip-threshold comparison logic unchanged (its `Output` payload
    construction gains the real `seq` value, fixing Task 1.1.1b's
    placeholder — see Task 2.2.1c)" — no longer claims the function overall
    is unmodified.
  - Story 2.2.1's AC now spells out the split explicitly: "**Precisely what
    stays unchanged**: `forward_step_for_output_result`'s signature and its
    `seq <= threshold` skip/dedup logic... **What does change**, and must
    not be described as 'unmodified': the function's construction of the
    `Output` payload itself gains the real `seq` value... that
    payload-construction change is exactly what Task 2.2.1c does."
  - Task 2.2.1c explicitly targets `main.rs:325-349` / line 337's call site,
    replacing the `seq: 0` placeholder with `OutputChunk { seq, data:
    bytes }`, and cross-references Task 1.1.1b by name.
  - Task 1.1.1b itself now states the placeholder is temporary and names
    2.2.1c as what replaces it ("the two tasks are a coherent pair, not
    disconnected activities, and the placeholder must not ship as-is"),
    closing the "separate fresh subagents, neither of which sees this
    contradiction" risk the original finding raised — a subagent reading
    either task in isolation now sees the cross-reference to the other.
  The remaining "unmodified" hits elsewhere in plan.md (Epic 2.4's Goal/AC
  and Task 2.4.1b) refer to a different subject — `stapler-squad`'s
  no-resume-token client behavior and its pre-existing `attach()` regression
  tests staying byte-identical — not to this function, so they don't
  reintroduce the contradiction. No remediation suggestion (the explicit
  round-trip-seq test) was verified as added, since that's a test-file
  change outside plan.md's own text and out of scope for this re-review pass
  (this pass only re-checks whether the plan's *prose* is now internally
  consistent, not whether Task 2.2.1c's eventual implementation will include
  that test — that's Story 2.2.1's AC to enforce at implementation time).

## Concerns

- [x] **RESOLVED (engineering repair pass, 2026-08-24). Story 1.1.1 —
  `AttachRequest.resume_from_seq` as a sibling field reintroduces the exact
  illegal state ADR-001 rejects for `OutputChunk`.**
  ADR-001's own stated rationale for replacing `bytes output` with a
  submessage: "a sibling `output_seq` field means `{output: [...],
  output_seq: <unset-or-stale>}` is a representable-but-meaningless state...
  A submessage makes the pairing atomic." The plan applies this reasoning to
  `AttachEvent.output` but not to `AttachRequest.resume_from_seq`, which per
  the Domain Glossary is "outside the oneof" alongside a `oneof payload {
  pane_id, input, resize }`
  ([`proto/tymux/v1/tymux.proto:256-262`](../../../proto/tymux/v1/tymux.proto#L256-L262)).
  This makes `{payload: Input(...), resume_from_seq: Some(42)}` — a
  keystroke-forwarding message with a resume seq attached — representable but
  meaningless, the identical shape of bug ADR-001 exists to prevent, just on
  the request side instead of the event side.
  **Original remediation suggested**: nest `resume_from_seq` into the
  `pane_id` oneof variant as a submessage, e.g. `message AttachTarget {
  string pane_id = 1; optional uint64 resume_from_seq = 2; } oneof payload {
  AttachTarget attach = 1; bytes input = 2; Resize resize = 3; }`, so a
  resume token can only ever be attached to the one message variant where
  it's meaningful.

  **Resolution actually adopted (a legitimate alternate resolution, not the
  suggested one)**: the plan's Pattern Decisions table (`AttachRequest.
  resume_from_seq placement` row) and Domain Glossary entry for
  `resume_from_seq` both now document a deliberate judgment call against
  `attach()`'s actual code, not a proto restructure. `attach()`'s existing
  first-message dispatch — `match first.payload { Some(PaneId(id)) => id, _
  => return Err(invalid_argument(...)) }` (`main.rs:624-631`) — rejects
  *any* first message whose `payload` isn't `Some(PaneId(_))`, which covers
  both this Concern's illustrative `Input(...)` case and the `payload: None`
  case, unconditionally, with `invalid_argument`, before `resume_from_seq` is
  ever read. The illegal state this Concern names is therefore
  type-representable but value-level unreachable in practice — restructuring
  the proto would remove a state that can never actually occur through this
  RPC's only call site. Task 2.2.1d adds a regression test asserting exactly
  this rejection (a first `AttachRequest` with `payload: None` and
  `resume_from_seq: Some(5)` fails with `invalid_argument`), converting the
  "unreachable in practice" claim from a doc note into a falsifiable check.
  A proto restructure is explicitly deferred, not rejected outright — the
  Pattern Decisions row notes it should be revisited "unless a future call
  site reads `resume_from_seq` before the `pane_id` check."

- [x] **RESOLVED. requirements.md's Risk Control ("No feature flag (additive,
  backward-compatible)") contradicts ADR-001's own characterization of the
  same change** ("This is a breaking wire-format change... and is accepted as
  such"). Both documents are part of this plan's paper trail; a reader who
  stops at requirements.md's Risk Control section would conclude rollback is
  low-risk and gate-free, when ADR-001 Consequences already documents the
  opposite (`stapler-squad`'s `BackendTymux` breaks at compile time in Go the
  moment it bumps to `clients/go/v0.2.0` without also updating its read
  site). Not a design flaw — the actual engineering decision in ADR-001 is
  sound and its consequences are handled (coordinated version bump, Epic
  1.2) — but the risk communication is internally inconsistent.

  **Resolution**: this Concern is stale against the current documents on both
  sides, though not for the reason originally assumed. ADR-001 was itself
  revised (P1 pre-mortem pass): the wire-shape decision changed from a
  same-field-number `bytes`→`message` promotion to an additive dual-field
  design, and ADR-001 (revised) now explicitly states "it is genuinely
  non-breaking, not 'breaking but field-number-tidy'" — it no longer
  characterizes the change as breaking at all, so it already agrees with
  Risk Control's "additive, backward-compatible" framing; there is no
  contradiction left to reconcile between these two documents. The actual
  stale wording surfaced by the engineering repair pass was elsewhere:
  plan.md's Epic 1.2 Goal and Story 1.2.1 previously described "ship[ping]
  this breaking proto change" and controlling "exactly when I take the
  breaking wire change." Current plan.md's Epic 1.2 Goal now reads "the
  underlying wire change itself is additive and non-breaking... this version
  bump exists so a consumer can *opt into* the new resume-capable surface...
  not to avoid a break that no longer exists," and Story 1.2.1's user story
  now reads "I control exactly when I take the new resume-capable API
  surface" — no "breaking" language remains anywhere in Epic 1.2. Confirmed
  by reading both ADR-001 (revised) and plan.md's current Epic 1.2 text
  directly, not inferred.

## Nitpicks

- Domain Glossary lists `ReplayOutcome` as `InWindow{chunks,tail_seq} |
  GapExceeded{oldest_available_seq}` — good sum-type shape, correctly avoids a
  bare `bool`/`Option` split for what is actually a two-armed decision with
  different payloads per arm. No change needed; noted as a positive pattern
  match for the type-driven-design lens.
- Pattern Decisions' rejection of GoF Strategy for eviction/gap-check logic
  ("exactly one eviction policy exists and is never expected to vary") is the
  textbook correct call per the design-patterns skill's own guidance ("avoid
  when only one concrete type exists") — good restraint, worth calling out
  since over-patterning is as common a finding as under-patterning.
- Task 2.1.1a-c's ReplayBuffer as a new, standalone file (`replay_buffer.rs`)
  unit-tested independently of `Pane`/`tymuxd` is exactly the testability
  shape Lens 1 looks for — no integration-only test dependency forced by the
  design.

## Lens Coverage Notes (no additional findings beyond the above)

- **Lens 1 (structural integrity)**: `Pane` gaining a `replay:
  Mutex<ReplayBuffer>` field alongside its existing `scrollback`/`output_seq`
  state is a cohesive addition to an aggregate that already owns "this pane's
  bounded in-memory history," not scope creep — consistent with existing SRP
  boundaries. The standalone-never-nested-with-parser locking rule (Pattern
  Decisions table) correctly avoids a new lock-ordering hazard.
- **Lens 3 (pattern selection)**: build-vs-buy.md's Phase 2 recommendation —
  hand-roll the replay buffer as `Mutex<VecDeque<(u64, Vec<u8>)>>`, no new
  crate, buy tonic's transport keepalive and build the grace-period policy on
  top — is what the plan actually implements (Epic 2.1's chosen alternative,
  Epic 3.1's tonic keepalive config, Epic 3.2's app-level heartbeat/grace
  period). Confirmed consistent, no deviation found.
