# Adversarial Review: stapler-squad-integration

**Date**: 2026-08-21
**Verdict**: CLEAN

**Scope**: This is a scoped re-review — it re-verifies only the four items previously marked BLOCKED, against the edited `implementation/plan.md`, `requirements.md`, and the amended `decisions/ADR-003-attach-priming-snapshot.md`, plus a light regression scan for new issues the edit might have introduced. It is not a fresh full pass.

## Prior Blockers — Re-verification

### 1. ADR-003 subscribe-then-snapshot double-render on reattach mid-stream — RESOLVED

Verified against the amended ADR-003 (Amendment, 2026-08-21) and plan.md's Epic 1.3:

- **Task 1.3.1a** (plan.md:275-277) adds `output_seq: AtomicU64` to `Pane`, incremented under the same lock that guards vt100 parser mutation (so the counter and grid state can never disagree), plus `snapshot_with_seq()` reading both atomically and changing the broadcast payload from `Vec<u8>` to `(u64, Vec<u8>)`.
- **Task 1.3.1b** (plan.md:279-281) sends the priming `Snapshot` after `pane.subscribe()`, tagged with the read sequence, and has `forward_handle` drop any `Output` event whose sequence is `<=` the snapshot's sequence — closing the exact double-render window (bytes landing between subscribe and snapshot are reflected once, either in the snapshot or as a later `Output` event, never both).
- **Task 1.3.1c** (plan.md:283-285) replaces the "wait for output to settle" test with one that attaches *while* a tight-loop output producer is actively running, asserting the `Snapshot` + subsequent `Output` events contain each emitted line exactly once — this is the race window the prior version explicitly avoided.
- **Task 2.5.2d** (plan.md:724-726) adds a Go-side regression test forcing a drop mid-stream and asserting `ClientFanout` subscribers never see a duplicate byte range across a `ReconnectLoop` reattach — covering the worst-case scenario the original blocker called out (reattach mid-agent-turn).

Given-When-Then is present and concrete in Story 1.3.1's AC (plan.md:269-272). Checked for blast radius: `pane.subscribe()`'s broadcast channel has exactly one other caller in the codebase (`crates/tymuxd/src/main.rs:475`, the same site Task 1.3.1b modifies) — `engine.rs:828`'s `subscribe()` is a *different* channel (`window_watchers`, `broadcast::Receiver<()>`), unaffected by the payload-type change. No hidden second consumer breaks.

**Verdict: RESOLVED.** The mechanism (sequence-tagged snapshot + drop-by-sequence) is a real fix for the identified race, not just an adjacent-sounding task, and both the server-side and client-side (reconnect) race windows now have tests that actually exercise concurrent/mid-stream attachment instead of avoiding it.

### 2. `go.mod replace` breaks builds without a sibling tymux checkout — RESOLVED

Verified against Story 2.1.1 (plan.md:433-448) and Task 2.1.1b specifically:

- Task 2.1.1a keeps the `replace github.com/tstapler/tymux/clients/go => ../tymux/clients/go` directive (relative path).
- **Task 2.1.1b** (plan.md:445-447) adds an `actions/checkout` step for `tstapler/tymux` at `path: ../tymux` to every CI workflow that builds/tests Go code (`build.yml`, `lint.yml`, audited for others), placed before any Go step — landing in the same PR as the `replace` directive, not deferred.
- Story 2.1.1's AC (plan.md:436-438) has a concrete Given-When-Then: CI checks out the sibling repo before any Go step, build succeeds with no local-path resolution error.
- The plan explicitly states the decision rationale: a build-tag/nested-module isolation was considered and rejected as more invasive than warranted for a solo-dev, two-repo, side-project-pace effort (requirements.md Constraints), matching the item's claimed reasoning.

One residual imprecision (not a blocker, see Light Regression Scan below): the Dependency Visualization diagram's parenthetical "(independently buildable at every merge via `replace` directive)" (plan.md:106) still slightly overstates what's true — buildability now depends on the CI sibling-checkout step, not the `replace` directive alone — but the plan's own Unresolved Questions section (plan.md:75) already carries the more precise framing, and this doesn't affect whether CI actually breaks.

**Verdict: RESOLVED.** The originally identified failure mode (CI breaks immediately on merge, contradicting "independently buildable") is closed — CI is fixed in the same PR, not left as follow-up.

### 3. No story addressed tymuxd-not-running-at-start — RESOLVED

Verified against new **Story 2.2.6** (plan.md:579-598):

- States the scope decision explicitly: `BackendTymux` does not supervise `tymuxd` itself (deliberate, matching the "internal/local, same host" security classification — not an oversight).
- Defines `ErrTymuxdUnreachable` (Task 2.2.6a), classifying Connect-Go transport-level failures (dial/connection-refused) distinctly from an RPC that reached tymuxd but was rejected — directly closing the gap `research/ux.md:218-224` flagged (don't conflate "daemon not started" with "a live session failed," don't swallow the underlying error string).
- Applies the classification at `Start`/`RestoreWithWorkDir` (Task 2.2.6b) — the first calls a session makes, matching the item's framing exactly.
- Tests the distinction via the fake `rpcTransport` seam introduced by Task 2.1.2d (Task 2.2.6c): a connection-refused-shaped fake error must produce `ErrTymuxdUnreachable`; an ordinary RPC error must not.
- Given-When-Then is present and concrete (plan.md:584).

**Verdict: RESOLVED.**

### 4. No story addressed tymuxd-crash-mid-session pane-survival contract — RESOLVED

Verified against new **Story 2.5.3** (plan.md:728-746) and, independently, against the actual tymux source:

- The story states the contract explicitly and matches what the persistence model can deliver: on daemon restart, a pane is treated as lost, not reattached; `ReviveSession`/`RestoreWithWorkDir` spawn a fresh replacement process, and this must be surfaced as a distinct state — not silently merged into "alive" or "exited cleanly."
- Tasks 2.5.3a-c: detect a daemon-restart reconnect distinctly from an ordinary transport blip (via the pane's `Liveness::Dead`-requiring-`ReviveSession` signal), surface a new distinguishable state (not folded into `IsAlive()`/exit-callback), and test against a real `tymuxd` that a post-restart `ReviveSession` yields a different PID and that the distinct state is what's surfaced (not a plain `IsAlive() == true`).
- Given-When-Then present and concrete (plan.md:732).

**Underlying factual claim — independently verified against source** (`~/Programming/tymux/crates/tymux-core/src/`):
- `PersistedPaneRecord` (`persistence.rs:18-24`) carries only `pane_id`, `command`, `cwd`, `rows`, `cols` — **no OS PID field**. VERIFIED.
- `Engine::revive_session` (`engine.rs:630-689`) unconditionally calls `Pane::spawn_with_id(*pane_id, &record.command, Some(&record.cwd), ...)` for any pane found as `PaneEntry::Dead` — which itself calls `spawn_internal`, i.e. a genuinely new child process — never attempts to locate, signal, or reattach to an existing OS process. VERIFIED — matches the plan's "unconditionally respawns fresh, never reattaches to an orphaned process" characterization exactly.
- `grep -rn "PR_SET_PDEATHSIG" --include="*.rs" .` from the tymux repo root returns zero hits. VERIFIED — matches the plan's "confirmed via grep, zero hits" claim.

**Verdict: RESOLVED.** Both the story's testability and its grounding claim about tymux's actual persistence/revive behavior check out.

## Light Regression Scan (new issues introduced by the edit)

No new blockers found. Two minor items:

- The Dependency Visualization ASCII diagram (plan.md:78-133) was not updated to list the two new stories added by this edit — Epic 2.2's block (plan.md:111-114) still ends at "2.2.5 GetPTY/GetPanePID stubs" with no mention of the new Story 2.2.6, and Epic 2.5's block (plan.md:122-123) still ends at "2.5.2 reconnect+resync" with no mention of the new Story 2.5.3. A reader skimming only the diagram would miss that these epics grew. Cosmetic — the stories themselves are fully specified in the epic sections — but worth a follow-up edit for diagram/text consistency.
- Minor wording overstatement noted under item 2 above: the diagram's "(independently buildable at every merge via `replace` directive)" parenthetical is slightly more confident than the Unresolved Questions section's own framing of the same fact. Doesn't affect correctness of the fix.

No numbering conflicts found across the new/renumbered tasks (1.3.1a-d, 2.1.1a-b, 2.1.2d, 2.2.1d, 2.2.6a-c, 2.4.2b, 2.5.2d, 2.5.3a-c) and no contradiction found between a new story's claims and an existing one's (e.g., Story 2.2.6's transport-error handling and Story 2.2.1's `dir`-validation error handling are complementary, not overlapping).

## Blockers

*(none — all four prior blockers verified RESOLVED above)*

## Concerns

Concerns from the prior pass (8 concerns) may still apply — this scoped re-review only re-verified the four prior blockers plus a light regression scan; a fresh full review would be needed to re-confirm they're all still accurate against the edited plan.

## Minors

Minors from the prior pass (4 minors) may still apply — same caveat as above. Two additional minors were found in this pass's light regression scan (see Light Regression Scan section): the Dependency Visualization diagram wasn't updated to list new Stories 2.2.6/2.5.3, and one parenthetical in that diagram slightly overstates the "independently buildable via `replace` directive" claim relative to the Unresolved Questions section's own framing.
