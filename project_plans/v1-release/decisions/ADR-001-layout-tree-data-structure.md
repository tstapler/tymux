# ADR-001: Layout tree data structure for splits — strict-binary `LayoutNode`, pane identity stored by id not by handle

## Status
Accepted

## Context
`requirements.md` puts real multi-pane splits in scope for v1.0, and both
`docs/adr/0001-single-pane-per-session-for-now.md` and the Phase 2 research
(`architecture.md` §1, `features.md` §1, `build-vs-buy.md` §1/§8) agree the
underlying model must be a recursive tree of splits, not a fixed grid —
tmux, zellij, and wezterm all converge on this independently
(`features.md`, "three independent implementations... all converge").

Two open sub-decisions remain, and the research docs disagree with each
other on the first one, which is exactly why this needs an explicit ADR
rather than "treat research as settled":

1. **Binary vs. N-ary children per split.** `architecture.md`'s proposed
   Rust shape (`Split { orientation, children: Vec<(LayoutNode, f32)> }`)
   is structurally N-ary (a `Vec`, not a fixed pair). `stack.md`'s
   proposed shape (`Split { direction, ratio, first: Box<Layout>, second:
   Box<Layout> }`) is strictly binary. tmux's own real model is N-ary
   (`features.md` §1: "more accurate to describe it as an N-ary tree with
   alternating split orientation per level"). `build-vs-buy.md` §8 and
   `features.md` §1 both call this out explicitly as "a concrete decision
   this project's planning phase should make explicitly," not one to
   inherit by default.
2. **What a `Leaf` holds.** `architecture.md`'s proposed shape is
   `Leaf { pane: Arc<Pane> }` — holding the live pane handle directly in
   the tree. This is incompatible with the persistence epic's dead-pane
   requirement (§2 below) and with `pitfalls.md`'s "dead vs. never-existed
   vs. live must be three distinguishable states everywhere" invariant: an
   `Arc<Pane>` has no way to represent "this leaf's pane died" or "this
   leaf's pane was never respawned after a restart" without smuggling a
   sentinel into `Pane` itself.

## Decision

**1. Use the `Vec<(LayoutNode, f32)>` children representation, but enforce
exactly 2 children per `Split` node as an invariant for v1** (checked in
every mutation method, not just assumed):

```rust
// crates/tymux-core/src/layout.rs
pub enum Orientation { Horizontal, Vertical } // side-by-side vs. stacked

pub enum LayoutNode {
    Leaf { pane_id: Uuid },
    Split {
        orientation: Orientation,
        children: Vec<(LayoutNode, f32)>, // invariant: len() == 2, ratios sum to 1.0
    },
}
```

This keeps the wire/data shape forward-compatible with a future move to
real N-ary splits (tmux's `tiled`/`even-horizontal` preset layouts, out of
scope for v1 per `features.md` §1's explicit deliverable exclusion) without
taking on N-ary resize/collapse math now. A 3-way "even" layout is
representable today only as two nested binary splits (an extra synthetic
node) — `build-vs-buy.md` §8 explicitly names this as "a legitimate,
simpler implementation choice" that "trades an extra synthetic split node
... for a much simpler recursive resize/collapse algorithm." Given this is
the single highest-correctness-risk item in the whole v1.0 scope
(`build-vs-buy.md` §8), simplicity wins over expressiveness for v1.

**2. `Leaf` holds `pane_id: Uuid`, not `Arc<Pane>`.** The tree is purely
structural/geometric; live pane objects live in a new flat map owned by
`Engine` (`panes: Mutex<HashMap<Uuid, PaneEntry>>`, `PaneEntry = Live(Arc<Pane>)
| Dead(PersistedPaneRecord)`), keyed by the same `Uuid` the tree's leaves
reference. This is a deliberate deviation from `architecture.md`'s literal
proposed shape, made explicitly here rather than silently: it is what
allows a `LayoutNode::Leaf` to survive a daemon restart with a dead pane
behind it (§2 of `architecture.md`, and the Tier-0 persistence contract —
see ADR-002) without inventing a "dead `Arc<Pane>`" concept, and it keeps
`Engine::pane_lookup(pane_id)`'s three-way `Live`/`Dead`/`Unknown` result
(the invariant `pitfalls.md` names as a repeated gap) a single flat lookup
rather than a tree walk that also has to reason about liveness.

**3. Resize propagation and geometry computation is one pure function**,
informed by (not copied from) zellij's `zellij-utils::input::layout::Layout`
shape and tmux's `layout.c` (`build-vs-buy.md` §8):

```rust
impl LayoutNode {
    /// Recomputes absolute (rows, cols, row_offset, col_offset) for every
    /// leaf given the window's real size. Pure function — no locking, no
    /// I/O, callable from a property-based test with arbitrary trees.
    pub fn compute_geometry(&self, rows: u16, cols: u16) -> Vec<(Uuid, PtyRect)>;
}
```

**4. Recursive collapse-on-close is a first-class tree operation**, not
caller-side cleanup: `LayoutNode::remove(pane_id) -> Option<LayoutNode>`
returns `None` if the whole subtree collapsed to nothing, or the
recursively-collapsed replacement tree otherwise. Single-child `Split`
nodes are removed and replaced by their surviving child at every level
this cascades through (`features.md` §1: "this recursive-collapse behavior
... is the single trickiest correctness case in the whole splits epic").

## Consequences
- A hard test gate (not optional) is required before this ships: property-
  based/invariant tests asserting (a) child ratios always sum to 1.0 within
  floating-point tolerance, (b) no leaf ever computes to a rect below a
  configured minimum rows/cols floor, (c) every `split`/`close`/`resize`
  sequence leaves the tree in a structurally valid state (no zero-child
  `Split`, no dangling `pane_id` references). See Epic 3, Story 3.2.
- `SessionState.windows: Vec<WindowState>` replaces the single
  `pane: Arc<Pane>` field (breaking internal change, not a breaking proto
  change by itself — see ADR alongside the proto migration in the main
  plan's Migration Plan section).
- `Engine::pane()`'s existing flat-lookup-by-id doc comment
  (`crates/tymux-core/src/engine.rs:91-92`, "the pane namespace is flat
  across sessions since each session has exactly one") becomes false in
  its stated reason but stays true in its *mechanism* — the flat map this
  ADR introduces is the same flat-lookup shape, just now backed by
  `PaneEntry` instead of walking `SessionState` directly.
- Real tmux's N-ary preset layouts (`tiled`, `even-horizontal`, etc.) and
  `synchronize-panes`/`swap-pane`/`join-pane`/`break-pane` remain explicitly
  out of scope for v1 (per `features.md` §1's deliberate-exclusion list),
  consistent with this ADR's binary-only constraint.

## Alternatives considered
- **Strict `Box<Layout>` binary pair** (`stack.md`'s literal proposed
  shape): rejected in favor of `Vec<(LayoutNode, f32)>` with an enforced
  `len() == 2` invariant, since the `Vec` shape costs nothing extra today
  and avoids a second breaking change if N-ary splits are ever added
  post-v1.
- **True N-ary tree** (tmux's real model): rejected for v1 — higher
  implementation/test cost (redistribute-across-N-siblings resize math) for
  a benefit (`tiled`/`even-*` preset layouts) that's already out of scope.
- **`Leaf { pane: Arc<Pane> }`** (`architecture.md`'s literal shape):
  rejected — incompatible with representing a dead pane after a restart
  without inventing a parallel sentinel inside `Pane` itself.
- **Adopting `zellij-utils` or `binary-space-partition` as a dependency**:
  rejected per `build-vs-buy.md` §1 — `binary-space-partition` is
  effectively unmaintained (last published 2017) and generic over the
  wrong domain (CSG/rendering, not terminal panes); `zellij-utils` is not a
  standalone layout crate and would drag in Zellij's entire config/KDL/
  plugin dependency graph for a ~150-line data structure. Both are read-only
  design references, not dependencies.
