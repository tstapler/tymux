//! Story 3.2 AC3 — the hard merge gate named explicitly in plan.md §5 Risk
//! Control: "Epic 3 cannot merge without the property-based/invariant test
//! suite for LayoutNode". Generates arbitrary sequences of split/remove/
//! resize operations against a `LayoutNode`, to a minimum tree depth, and
//! asserts three invariants after every single operation:
//!   (a) every Split node's children ratios sum to ~1.0
//!   (b) no leaf's rect (at the window size active when it was split) ever
//!       falls below MIN_PANE_ROWS/MIN_PANE_COLS
//!   (c) no zero- or one-child Split node exists
//!
//! Scope note on (b) and resize: this module's `LayoutNode` is purely
//! structural (no live window-size state of its own) — `split()` enforces
//! the floor against the window size supplied *at split time*, which is
//! the only point this pure module has a rejection mechanism. A `resize`
//! operation in the generated sequence therefore only re-checks (a)/(c) at
//! the new size; it does not retroactively re-validate (b) for
//! already-split leaves against a shrunk window — that operational
//! concern (attached-client viewport tracking, atomic recompute) belongs
//! to Epic 3 Story 3.4's `Engine`-level geometry recompute, not this pure
//! module.
//!
//! Mutation-testing check (plan.md §5 Risk Control, pre-mortem.md
//! Failure #4): `LayoutNode::split`'s ratio pairing was deliberately
//! changed from `(0.5, 0.5)` to `(0.5, 0.3)` (sum 0.8, not ~1.0) and this
//! suite was re-run — it failed immediately (invariant (a), both the
//! proptest and the example-based test caught it on essentially the first
//! generated case) — before being reverted back to `(0.5, 0.5)`. Confirms
//! the gate is a real gate, not just present.

use proptest::prelude::*;
use std::collections::HashSet;
use tymux_core::{LayoutNode, Orientation, RemoveOutcome, MIN_PANE_COLS, MIN_PANE_ROWS};
use uuid::Uuid;

const MIN_TREE_DEPTH_OPS: usize = 20;
const MAX_TREE_DEPTH_OPS: usize = 60;
const START_ROWS: u16 = 200;
const START_COLS: u16 = 400;

#[derive(Debug, Clone)]
enum Op {
    /// Split the leaf at `target_index % live_leaf_count`.
    Split {
        target_index: usize,
        horizontal: bool,
    },
    /// Remove the leaf at `target_index % live_leaf_count`.
    Remove { target_index: usize },
    /// Change the tracked window size (bounded to stay large enough that
    /// most splits remain satisfiable — see the module doc's scope note).
    Resize { rows: u16, cols: u16 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (any::<usize>(), any::<bool>()).prop_map(|(target_index, horizontal)| Op::Split {
            target_index,
            horizontal
        }),
        any::<usize>().prop_map(|target_index| Op::Remove { target_index }),
        (100u16..=300, 200u16..=600).prop_map(|(rows, cols)| Op::Resize { rows, cols }),
    ]
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op_strategy(), MIN_TREE_DEPTH_OPS..=MAX_TREE_DEPTH_OPS)
}

/// Collects every leaf pane_id currently in the tree, in a stable
/// (pre-order) order, so `target_index % len` deterministically selects
/// one of them.
fn live_leaves(node: &LayoutNode) -> Vec<Uuid> {
    match node {
        LayoutNode::Leaf { pane_id } => vec![*pane_id],
        LayoutNode::Split { children, .. } => {
            children.iter().flat_map(|(c, _)| live_leaves(c)).collect()
        }
    }
}

/// Invariants (a) and (c) — checked after every operation, at whatever
/// window size is currently tracked.
fn assert_ratio_and_structural_invariants(node: &LayoutNode) {
    match node {
        LayoutNode::Leaf { .. } => {}
        LayoutNode::Split { children, .. } => {
            assert_eq!(
                children.len(),
                2,
                "invariant (c) violated: a Split node must have exactly 2 children, got {}",
                children.len()
            );
            let ratio_sum: f32 = children.iter().map(|(_, r)| r).sum();
            assert!(
                (ratio_sum - 1.0).abs() < 0.01,
                "invariant (a) violated: children ratios sum to {ratio_sum}, expected ~1.0"
            );
            for (child, _) in children {
                assert_ratio_and_structural_invariants(child);
            }
        }
    }
}

/// Invariant (b) — checked immediately after a successful split, against
/// the window size active at that moment, for only the two leaves that
/// split just produced (`old_leaf_id`, `new_leaf_id`). Per the module
/// doc's scope note, other, older leaves are deliberately NOT
/// re-validated here: a `Resize` op earlier in the sequence may have
/// shrunk the window below what an earlier split validated against, and
/// this pure module has no retroactive re-validation mechanism for that
/// (that's Epic 3 Story 3.4's `Engine`-level concern) — asserting the
/// floor tree-wide would fail on window shrinkage, not on a `split()`
/// bug, if checked unconditionally.
fn assert_no_leaf_below_minimum(
    node: &LayoutNode,
    rows: u16,
    cols: u16,
    old_leaf_id: Uuid,
    new_leaf_id: Uuid,
) {
    for (id, rect) in node.compute_geometry(rows, cols) {
        if id != old_leaf_id && id != new_leaf_id {
            continue;
        }
        assert!(
            rect.rows >= MIN_PANE_ROWS && rect.cols >= MIN_PANE_COLS,
            "invariant (b) violated: leaf rect {rect:?} is below the minimum \
             ({MIN_PANE_ROWS} rows x {MIN_PANE_COLS} cols)"
        );
    }
}

fn assert_no_duplicate_pane_ids(node: &LayoutNode) {
    let leaves = live_leaves(node);
    let unique: HashSet<Uuid> = leaves.iter().copied().collect();
    assert_eq!(
        leaves.len(),
        unique.len(),
        "a pane_id appeared more than once in the tree"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn layout_node_invariants_should_hold_after_any_sequence_of_split_remove_resize_ops(ops in ops_strategy()) {
        let root_id = Uuid::new_v4();
        let mut tree = Some(LayoutNode::leaf(root_id));
        let mut rows = START_ROWS;
        let mut cols = START_COLS;

        for op in ops {
            let Some(current) = tree.take() else {
                // The tree emptied out (last pane removed) — nothing further
                // to apply; a real Engine would treat this as "window closed".
                break;
            };

            match op {
                Op::Resize { rows: new_rows, cols: new_cols } => {
                    rows = new_rows;
                    cols = new_cols;
                    assert_ratio_and_structural_invariants(&current);
                    tree = Some(current);
                }
                Op::Split { target_index, horizontal } => {
                    let leaves = live_leaves(&current);
                    let target = leaves[target_index % leaves.len()];
                    let orientation = if horizontal { Orientation::Horizontal } else { Orientation::Vertical };
                    let new_pane = Uuid::new_v4();

                    let mut mutated = current;
                    match mutated.split(target, orientation, new_pane, rows, cols) {
                        Ok(()) => {
                            assert_ratio_and_structural_invariants(&mutated);
                            assert_no_leaf_below_minimum(&mutated, rows, cols, target, new_pane);
                            assert_no_duplicate_pane_ids(&mutated);
                        }
                        Err(_) => {
                            // Rejected (too small at this window size) —
                            // the tree must be provably unchanged.
                            assert_ratio_and_structural_invariants(&mutated);
                        }
                    }
                    tree = Some(mutated);
                }
                Op::Remove { target_index } => {
                    let leaves = live_leaves(&current);
                    let target = leaves[target_index % leaves.len()];
                    match current.remove(target) {
                        RemoveOutcome::Collapsed(survivor) => {
                            assert_ratio_and_structural_invariants(&survivor);
                            assert_no_duplicate_pane_ids(&survivor);
                            tree = Some(survivor);
                        }
                        RemoveOutcome::WindowEmpty => {
                            tree = None;
                        }
                    }
                }
            }
        }
    }
}

/// Example-based, deterministic exercise of the specific nested-collapse
/// and odd-remainder-rounding cases named in pre-mortem.md Failure #4 —
/// dedicated cases rather than relying solely on the random generator to
/// find them.
#[test]
fn nested_collapse_and_odd_remainder_cases_are_explicitly_covered() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let mut tree = LayoutNode::leaf(a);
    tree.split(a, Orientation::Horizontal, b, 200, 401).unwrap();
    tree.split(b, Orientation::Vertical, c, 200, 200).unwrap();

    // Odd remainder at the root split.
    let geo = tree.compute_geometry(200, 401);
    let total_area: u32 = geo.iter().map(|(_, r)| r.rows as u32 * r.cols as u32).sum();
    assert_eq!(
        total_area,
        200 * 401,
        "odd-remainder split must still tile the full window"
    );

    // Nested collapse: removing C should leave a 2-leaf tree (A, B), not a
    // dangling one-child Split.
    match tree.remove(c) {
        RemoveOutcome::Collapsed(survivor) => {
            let leaves = live_leaves(&survivor);
            assert_eq!(leaves.len(), 2);
            assert!(leaves.contains(&a));
            assert!(leaves.contains(&b));
            assert_ratio_and_structural_invariants(&survivor);
        }
        RemoveOutcome::WindowEmpty => panic!("expected survivors after removing a nested leaf"),
    }
}
