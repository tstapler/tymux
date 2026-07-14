# Adversarial Re-Review: v1-release

**Original verdict**: BLOCKED (3 blockers)
**New verdict**: CONCERNS

## Blocker 1 (musl contingency): RESOLVED

Epic 1 now has a new **Story 1.4 — "Early spike: validate `portable-pty` cross-compiles to musl, before any other epic starts"** (plan.md lines 237–252), sequenced as the fourth story in Epic 1, i.e. before Epic 2 (and therefore before all of Epics 2–7) begins any work.

- **AC1** requires a throwaway `portable-pty`-linked binary to cross-compile to `x86_64-unknown-linux-musl` and actually run inside a minimal (non-glibc) container — not just link, but execute.
- **AC2** + the explicit **"Fallback (written now, not improvised later)"** subsection give a concrete Plan B: if the spike fails, Epic 8 Story 8.1's target matrix switches from `-musl` to `-gnu` targets built on the oldest supported Ubuntu LTS runner, with a documented minimum-glibc-version requirement in the README, and Pattern Decision #13 is explicitly revised rather than left inaccurate.
- Tasks 1–5 are concrete (scaffold spike crate → cross-compile → run in Alpine → record outcome → delete/fold in), not hand-waved.

This directly addresses both halves of the original blocker: the risk is validated first (not dead last in Epic 8), and a real contingency is written down rather than left to be improvised.

**Minor residual nit** (not blocking): Epic 8 Story 8.1 Task 3 (line 657) still reads "Confirm `portable-pty` cross-compiles cleanly against the musl targets (a real, previously-unvalidated risk per `pitfalls.md` §7)" — this is now stale phrasing since Story 1.4 already validates it. Worth a one-line edit so Epic 8 doesn't imply this is still an open risk.

## Blocker 2 (Attach bidi-stream fallback): RESOLVED

Story 7.3 (plan.md lines 617–632) now opens with an **"Explicit fallback decision (written now, not improvised mid-epic)"** subsection that:

- Names the exact trigger condition ("streaming client codegen is broken, or bidi calls cannot be sustained/cancelled cleanly from Node against `tonic`'s server implementation").
- States the concrete fallback: descope the cross-language proof to unary RPCs only (`CreateSession`, `ListSessions`, `CapturePane`), document `Attach` as Rust-client-only in `clients/ts/README.md` and the main README's Known Limitations (Story 8.2), and revise `requirements.md`'s Success Metric #3 rather than leave it silently unmet.
- Explicitly parallels the precedent already set by ADR-003 for browser-`Attach`, and is backed by a new **AC3** ("this story's actual deliverable is the fallback path... not a silently-abandoned or indefinitely-blocked story") and **Task 6** (concrete doc/requirements-update actions if the fallback triggers).

This gives Epic 8 Story 8.3 an always-available path forward regardless of Story 7.3's outcome, exactly as the original blocker demanded.

## Blocker 3 (lock atomicity/ordering): PARTIALLY RESOLVED — one residual gap

Story 3.4 (plan.md lines 352–371) now has two new subsections addressing this directly.

**(a) `sessions`/`panes` lock ordering — resolved.** The plan states a concrete rule, not a platitude: every `Engine` method touching both maps (`split_pane`, `close_pane`, `create_window`, `revive_session`) acquires `sessions` then `panes`, in that fixed order, for the duration of a single mutation, releasing both together, "never holds one across an `.await` or a call back into the other lock." This gives a documented invariant ("a window's `LayoutNode` and the `panes` map are always mutually consistent at every point where neither lock is held") that a reader can actually rely on. This is a real, specific answer to the exact question the original blocker raised.

**(b) Whether the atomic resize holds a lock across `Pane::resize()`'s syscalls — answered, but the safety argument has a gap.** The plan states explicitly: **it does not** hold the lock across the syscalls. The described shape: compute geometry under lock (fast, in-memory) → release lock → call each affected `Pane::resize()` outside the lock → re-acquire briefly to commit. It explicitly names the accepted race ("a concurrent `ListSessions` call... could observe the pre-resize geometry for a few syscalls' worth of wall-clock time") and argues it's acceptable because resize is not a hot path and the alternative is an unbounded stall proportional to pane count. As far as it goes, this is a real, reasoned decision, not "this will be handled carefully" — for a *single* recompute-and-apply, the values applied to every affected pane come from one consistently-computed dimension-wise minimum, so no pane ever ends up applying "client A's size" while its sibling applies "client B's size" from a single trigger.

**What the plan does not address**: nothing in Story 3.4 states how *concurrent/overlapping* `RecomputeWindowGeometry` invocations for the *same* window are serialized. Because the lock is released during each invocation's apply phase, a second resize trigger (e.g. a second client's viewport report arriving moments after the first) can acquire the lock, compute its own newer geometry, and begin its own unlocked apply phase while the first invocation's `Pane::resize()` calls are still in flight. Nothing described prevents the two invocations' per-pane syscalls from interleaving out of order — e.g. pane X receiving invocation 2's (newer, correct) geometry followed by invocation 1's (stale) geometry arriving after it, since ordering between the two unlocked apply phases is not guaranteed. That is exactly the "one pane reflects client A's size, its sibling reflects client B's" torn state ADR-004/Story 3.4 AC2 exists to rule out — just reintroduced one layer up, at the level of overlapping resize *triggers* rather than within a single trigger. The "single-owner-writer" language (Domain Glossary: "exactly one task/lock owns... window-subtree resize") gestures at a fix (e.g. a single serial per-window task/actor processing recompute requests one at a time) but this is never made concrete as an implementation task, and no generation-counter/staleness check is specified for the "re-acquire to commit" step to detect and discard a superseded computation.

**Net assessment**: this is materially better than the original state (a real ordering rule for the two mutexes, and a reasoned, explicit trade-off for not holding the lock across syscalls) and is unlikely to cause a hang (the original bug class) — a stale intermediate size self-corrects on the next recompute trigger, so the consequence is transient visual/geometry inconsistency rather than a deadlock or crash. But it does not fully establish the "never torn" guarantee AC2 promises, because concurrent/overlapping recompute triggers for the same window are not shown to be serialized. **Recommend**: before Epic 3 implementation, add either (a) an explicit per-window serial task/queue that processes recompute-and-apply as one unit (true single-owner, not just single-owner-per-invocation), or (b) a generation counter checked at the "re-acquire to commit" step so a superseded computation's syscalls are not committed over a newer one's.

## New issues introduced by the fix

- Epic 8 Story 8.1 Task 3's wording ("a real, previously-unvalidated risk per `pitfalls.md` §7") is now stale given Story 1.4 already validates this in Epic 1 — cosmetic, not a blocker.
- The residual concurrent-recompute-serialization gap noted under Blocker 3(b) above — new relative to the original review only in the sense that it wasn't visible until the fix's own reasoning was inspected closely; it did not exist as a distinct named issue before.

## Remaining concerns (informational, not blocking)

Of the original 6 concerns:
- **Resolved**: `capturePane` missing from Epic 7's TS validation (Story 7.3 AC2 + Task 4 now cover it explicitly); no defined behavior for in-flight `Attach` under external `KillSession` (Story 2.3 AC2 + Tasks 4–5); partially-malformed config file behavior (Story 5.1 AC3 + Tasks 3–5, warn-and-fallback-per-binding matching the persistence pattern); the lock-granularity question research raised (now explicit as Unresolved Question #8, with an explicit rationale for keeping the single-mutex-per-map design for v1.0 and deferring finer-grained sharding).
- **Still stand, unaddressed** (as before, non-blocking): Story 6.4's mode-reactive keybinding-hint rendering still exceeds the literal "renders state" requirement with no explicit stretch/optional labeling; Story 5.4's `SearchScrollback` RPC is still new scope beyond the literal requirement with no explicit core-vs-stretch confirmation flagged for Phase 4.
