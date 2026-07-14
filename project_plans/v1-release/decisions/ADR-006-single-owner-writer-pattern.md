# ADR-006: Single-owner-writer pattern as a cross-cutting concurrency principle

## Status
Accepted

## Context
`pitfalls.md`'s closing "Cross-cutting observation" identifies that several
independently-researched pitfalls across three different epics are actually
**the same underlying bug shape**, appearing at increasing scope:

1. `Pane::resize()` (`crates/tymux-core/src/pane.rs:155-164`) takes two
   separate mutexes sequentially (pty master, then vt100 parser) with no
   atomicity — already a live bug today for concurrent multi-attach resize.
2. Persistence's save path, once it exists, is a second independent writer
   racing `create_session`/`kill_session`'s own brief lock scopes
   (`pitfalls.md` §2a) unless snapshotting and writing are explicitly
   sequenced.
3. The status bar's redraw logic and the existing pty-output-forwarding
   loop (`crates/tymux-cli/src/main.rs:192-206`) both want to write to the
   same `stdout` handle from what would otherwise be independent tasks —
   a literal data race on the terminal byte stream if not serialized
   (`pitfalls.md` §4).

Solving each of these ad hoc, per-epic, risks three subtly different
half-fixes instead of one well-understood pattern applied consistently.

## Decision

Adopt **single-owner-writer** as an explicit, named architectural principle
applied deliberately in all three places, not discovered independently
three times:

1. **Window-level resize**: a per-window lock (or a single actor/task per
   window) owns "recompute geometry across the whole `LayoutNode` subtree
   and apply to every leaf pane" as one atomic operation. No caller ever
   invokes `pane.resize()` directly for a pane that belongs to a
   multi-pane window; all resize requests funnel through the window-level
   owner (ADR-004 depends on this).
2. **Persistence writes**: the existing `Engine` `Mutex<HashMap<Uuid,
   SessionState>>` (or its post-split-epic equivalent) is the single point
   where a mutation *and* a persistence-snapshot are sequenced: snapshot the
   relevant `PersistedSessionRecord` fields while still holding the lock,
   then release the lock before doing the actual file write. No second,
   independently-timed task ever snapshots-and-writes state without going
   through this same sequencing.
3. **CLI stdout**: exactly one task/owner writes to `std::io::stdout()`
   during an attach session. Both the pty-output-forwarding path and any
   status-bar redraw enqueue onto this one owner (a channel into a single
   writer task, or an explicit `Mutex` around the handle with a documented
   "always emit a complete escape sequence before releasing" discipline) —
   never two independent `write_all` callers.

## Consequences
- `Pane::resize()`'s existing two-mutex internal sequence should also be
  tightened (e.g. a single lock scope covering both the pty resize and the
  parser resize, or an explicit ordering + retry-free single critical
  section) as part of Epic 3, since splits multiply its blast radius from
  "one pane racing itself" to "N sibling panes racing each other."
- This pattern is referenced from Epic 3 (window resize), Epic 4
  (persistence writes), and Epic 6 (status bar/stdout) rather than each
  epic inventing its own answer — task descriptions in the main plan link
  back to this ADR instead of re-deriving the reasoning.
- Where a single-owner design meaningfully raises implementation cost for a
  case that's provably contention-free at v1.0's scale (e.g. no realistic
  path to two writers), it's acceptable to note that explicitly and skip
  the pattern — this ADR is about not solving the *same* three-way race
  three different ways, not about mandating actor-model everywhere
  regardless of need.

## Alternatives considered
- **Fix each occurrence independently, ad hoc**: rejected — this is
  literally what `pitfalls.md` warns against; three slightly different
  half-fixes for the same bug shape is worse than one applied pattern, both
  for review cost and for the next contributor's ability to recognize the
  pattern when a fourth instance shows up later.
