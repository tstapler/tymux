use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Split direction. `Horizontal` means side-by-side (tmux's `-h`);
/// `Vertical` means stacked (tmux's default `-v`/no-flag). tymux picks its
/// own CLI verb naming deliberately rather than reusing tmux's `-h`/`-v`
/// flags, which are only mnemonic in retrospect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// Hard structural floor enforced by [`LayoutNode::split`] itself — no
/// split is ever allowed to produce a leaf smaller than this, regardless
/// of caller intent. This is a different, lower tier than
/// [`RECOMMENDED_SPLIT_MIN_ROWS`] below (Epic 3 Story 3.5 AC2): this
/// constant is the anti-corruption floor that must never be violated at
/// any size; `RECOMMENDED_SPLIT_MIN_ROWS` is a friendlier, higher bar
/// `split` rejects against before a caller ever reaches this one.
pub const MIN_PANE_ROWS: u16 = 2;
pub const MIN_PANE_COLS: u16 = 10;

/// Usability-oriented threshold (Epic 3 Story 3.5 AC2), distinct from and
/// higher than [`MIN_PANE_ROWS`]. A *horizontal* (side-by-side) split
/// never changes a pane's row count — only its column count — so a pane
/// that already has fewer than this many rows stays exactly that short
/// after splitting side-by-side; the split just adds a second short pane
/// rather than fixing anything. `LayoutNode::split` rejects a horizontal
/// split whose target pane's rows are already below this bar, with a
/// distinct [`LayoutError::BelowRecommendedSize`] carrying the actual row
/// count so the caller can state real numbers, not just "too cramped".
/// Vertical splits (which do change row count, and are already covered by
/// `MIN_PANE_ROWS`'s hard floor) are intentionally out of scope for this
/// check — see plan.md Story 3.5 AC2's note.
pub const RECOMMENDED_SPLIT_MIN_ROWS: u16 = 20;

/// A leaf's or split's on-screen rectangle, in cells. `row`/`col` are the
/// top-left corner's offset within the window; `rows`/`cols` are the
/// rectangle's own size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyRect {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

/// Recursive tree describing one window's pane arrangement. `Leaf` holds
/// only a `pane_id` — never a live pane handle — so the layout model stays
/// decoupled from pane lifecycle (see `Engine.panes`, which owns the
/// `pane_id -> PaneEntry` mapping this tree's leaves reference into).
///
/// `Split.children` is enforced to have exactly 2 entries at all times
/// (ADR-001) — a true N-ary tree was considered and rejected; nested binary
/// splits already express any N-pane layout tmux itself supports.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutNode {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        orientation: Orientation,
        /// Always exactly 2 entries: `(child, ratio)` where the two ratios
        /// sum to ~1.0. Enforced by every mutator in this module — a
        /// `Vec` rather than a fixed-size `[T; 2]` only because `LayoutNode`
        /// needs to recurse into itself, which a fixed array of a
        /// not-yet-fully-defined recursive type can't express directly.
        children: Vec<(LayoutNode, f32)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoveOutcome {
    /// The `Split` node containing the removed leaf collapsed into its
    /// surviving sibling, which now stands in the removed leaf's place.
    Collapsed(LayoutNode),
    /// The removed leaf was the window's entire tree (its only pane) —
    /// there is nothing left to collapse into.
    WindowEmpty,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    /// A split would produce a leaf smaller than [`MIN_PANE_ROWS`]/
    /// [`MIN_PANE_COLS`]. Carries the actual would-be size so the caller
    /// can state the real numbers, not just "too small" (`ux.md` §4).
    BelowMinimumSize { rows: u16, cols: u16 },
    /// A *horizontal* split whose target pane already has fewer than
    /// [`RECOMMENDED_SPLIT_MIN_ROWS`] rows (Epic 3 Story 3.5 AC2) — a
    /// friendlier, higher-tier rejection than [`LayoutError::BelowMinimumSize`].
    /// Carries the pane's actual row count.
    BelowRecommendedSize { rows: u16 },
    /// `target` does not name any leaf in this tree.
    PaneNotFound { pane_id: Uuid },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::BelowMinimumSize { rows, cols } => write!(
                f,
                "split would produce a pane of {rows} rows x {cols} cols, \
                 below the minimum of {MIN_PANE_ROWS} rows x {MIN_PANE_COLS} cols"
            ),
            LayoutError::BelowRecommendedSize { rows } => write!(
                f,
                "Can't split: pane is {rows} rows, minimum for a horizontal split is \
                 ~{RECOMMENDED_SPLIT_MIN_ROWS} rows. Resize your terminal or close another \
                 pane first."
            ),
            LayoutError::PaneNotFound { pane_id } => {
                write!(f, "no leaf with pane_id {pane_id} in this layout")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

impl LayoutNode {
    pub fn leaf(pane_id: Uuid) -> Self {
        LayoutNode::Leaf { pane_id }
    }

    /// Pure recursive geometry computation: ratio-weighted subdivision of
    /// `(rows, cols)` across this node's tree, with any integer-division
    /// remainder assigned to the last child rather than dropped or
    /// duplicated. Returns one `(pane_id, PtyRect)` per leaf.
    pub fn compute_geometry(&self, rows: u16, cols: u16) -> Vec<(Uuid, PtyRect)> {
        self.compute_geometry_at(0, 0, rows, cols)
    }

    fn compute_geometry_at(
        &self,
        row: u16,
        col: u16,
        rows: u16,
        cols: u16,
    ) -> Vec<(Uuid, PtyRect)> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![(
                *pane_id,
                PtyRect {
                    row,
                    col,
                    rows,
                    cols,
                },
            )],
            LayoutNode::Split {
                orientation,
                children,
            } => {
                let (first, first_ratio) = &children[0];
                let (second, _) = &children[1];
                match orientation {
                    Orientation::Horizontal => {
                        let first_cols = split_extent(cols, *first_ratio);
                        let second_cols = cols - first_cols;
                        let mut out = first.compute_geometry_at(row, col, rows, first_cols);
                        out.extend(second.compute_geometry_at(
                            row,
                            col + first_cols,
                            rows,
                            second_cols,
                        ));
                        out
                    }
                    Orientation::Vertical => {
                        let first_rows = split_extent(rows, *first_ratio);
                        let second_rows = rows - first_rows;
                        let mut out = first.compute_geometry_at(row, col, first_rows, cols);
                        out.extend(second.compute_geometry_at(
                            row + first_rows,
                            col,
                            second_rows,
                            cols,
                        ));
                        out
                    }
                }
            }
        }
    }

    /// Finds the leaf for `target` and replaces it with a new `Split`
    /// nesting the original leaf and a fresh leaf for `new_pane`, each at
    /// the given `ratio` (and its complement). Rejects (leaving the tree
    /// untouched) if the resulting geometry — computed against the
    /// window's current `(window_rows, window_cols)` — would put either
    /// new leaf below [`MIN_PANE_ROWS`]/[`MIN_PANE_COLS`] (the hard floor),
    /// or, for a horizontal split only, if the target pane's rows are
    /// already below [`RECOMMENDED_SPLIT_MIN_ROWS`] (the friendlier,
    /// usability-tier check, Epic 3 Story 3.5 AC2).
    pub fn split(
        &mut self,
        target: Uuid,
        orientation: Orientation,
        new_pane: Uuid,
        window_rows: u16,
        window_cols: u16,
    ) -> Result<(), LayoutError> {
        if !self.contains(target) {
            return Err(LayoutError::PaneNotFound { pane_id: target });
        }

        // Validate against the target leaf's *current* rect, not the whole
        // window — a deeply nested leaf's own rect is what actually
        // shrinks when it splits.
        let target_rect = self
            .compute_geometry(window_rows, window_cols)
            .into_iter()
            .find(|(id, _)| *id == target)
            .map(|(_, rect)| rect)
            .expect("target already checked to exist via contains()");

        let (new_rows, new_cols) = match orientation {
            Orientation::Horizontal => (target_rect.rows, target_rect.cols / 2),
            Orientation::Vertical => (target_rect.rows / 2, target_rect.cols),
        };
        if new_rows < MIN_PANE_ROWS || new_cols < MIN_PANE_COLS {
            return Err(LayoutError::BelowMinimumSize {
                rows: new_rows,
                cols: new_cols,
            });
        }
        // AC2's friendlier, higher-tier check — horizontal-only, since a
        // horizontal split leaves rows unchanged (see
        // RECOMMENDED_SPLIT_MIN_ROWS's doc comment for why vertical splits
        // are out of scope here).
        if orientation == Orientation::Horizontal && new_rows < RECOMMENDED_SPLIT_MIN_ROWS {
            return Err(LayoutError::BelowRecommendedSize { rows: new_rows });
        }

        self.replace_leaf(target, |old_leaf| LayoutNode::Split {
            orientation,
            children: vec![(old_leaf, 0.5), (LayoutNode::leaf(new_pane), 0.5)],
        });
        Ok(())
    }

    /// Removes the leaf for `pane_id`. If it was one child of a `Split`,
    /// that `Split` node is replaced by its surviving sibling (recursive
    /// collapse, per `features.md` §1's named trickiest correctness case).
    /// If `pane_id` was this entire tree, returns `WindowEmpty` — there is
    /// nothing to collapse into.
    pub fn remove(self, pane_id: Uuid) -> RemoveOutcome {
        match self {
            LayoutNode::Leaf { pane_id: id } if id == pane_id => RemoveOutcome::WindowEmpty,
            LayoutNode::Leaf { .. } => RemoveOutcome::Collapsed(self),
            LayoutNode::Split {
                orientation,
                mut children,
            } => {
                let (second, second_ratio) = children.pop().unwrap();
                let (first, first_ratio) = children.pop().unwrap();

                let first_has_target = first.contains(pane_id);
                let second_has_target = second.contains(pane_id);

                if first_has_target && matches!(first, LayoutNode::Leaf { .. }) {
                    return RemoveOutcome::Collapsed(second);
                }
                if second_has_target && matches!(second, LayoutNode::Leaf { .. }) {
                    return RemoveOutcome::Collapsed(first);
                }

                // The target is nested deeper inside one side — recurse
                // into that side and rebuild this Split around the result.
                if first_has_target {
                    return match first.remove(pane_id) {
                        RemoveOutcome::Collapsed(new_first) => {
                            RemoveOutcome::Collapsed(LayoutNode::Split {
                                orientation,
                                children: vec![(new_first, first_ratio), (second, second_ratio)],
                            })
                        }
                        RemoveOutcome::WindowEmpty => RemoveOutcome::Collapsed(second),
                    };
                }
                if second_has_target {
                    return match second.remove(pane_id) {
                        RemoveOutcome::Collapsed(new_second) => {
                            RemoveOutcome::Collapsed(LayoutNode::Split {
                                orientation,
                                children: vec![(first, first_ratio), (new_second, second_ratio)],
                            })
                        }
                        RemoveOutcome::WindowEmpty => RemoveOutcome::Collapsed(first),
                    };
                }

                // pane_id wasn't found anywhere in this subtree — return it
                // unchanged rather than silently dropping a child.
                RemoveOutcome::Collapsed(LayoutNode::Split {
                    orientation,
                    children: vec![(first, first_ratio), (second, second_ratio)],
                })
            }
        }
    }

    pub fn contains(&self, pane_id: Uuid) -> bool {
        match self {
            LayoutNode::Leaf { pane_id: id } => *id == pane_id,
            LayoutNode::Split { children, .. } => {
                children.iter().any(|(child, _)| child.contains(pane_id))
            }
        }
    }

    /// Every leaf's `pane_id` in this subtree, pre-order.
    pub fn leaves(&self) -> Vec<Uuid> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![*pane_id],
            LayoutNode::Split { children, .. } => {
                children.iter().flat_map(|(c, _)| c.leaves()).collect()
            }
        }
    }

    /// Walks the tree, replacing the leaf matching `target` in place using
    /// `f`. Panics if `target` isn't present — callers must check
    /// `contains()` first (as `split` does), since this is a private,
    /// invariant-preserving helper, not a public fallible API.
    fn replace_leaf(&mut self, target: Uuid, f: impl FnOnce(LayoutNode) -> LayoutNode) {
        fn go(
            node: &mut LayoutNode,
            target: Uuid,
            f: &mut Option<impl FnOnce(LayoutNode) -> LayoutNode>,
        ) {
            match node {
                LayoutNode::Leaf { pane_id } if *pane_id == target => {
                    let f = f
                        .take()
                        .expect("replace_leaf visits its target at most once");
                    let old = std::mem::replace(node, LayoutNode::leaf(Uuid::nil()));
                    *node = f(old);
                }
                LayoutNode::Leaf { .. } => {}
                LayoutNode::Split { children, .. } => {
                    for (child, _) in children.iter_mut() {
                        go(child, target, f);
                    }
                }
            }
        }
        let mut f = Some(f);
        go(self, target, &mut f);
    }
}

/// Integer-division split of `extent` cells by `ratio`, floored — the
/// caller is responsible for giving any remainder to the *other* side
/// (always the second/last child, per `compute_geometry`'s and `split`'s
/// documented remainder rule) rather than dropping or duplicating it.
fn split_extent(extent: u16, ratio: f32) -> u16 {
    ((extent as f32) * ratio).floor() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: Uuid) -> LayoutNode {
        LayoutNode::leaf(id)
    }

    #[test]
    fn compute_geometry_should_split_evenly_when_horizontal_split_has_equal_ratios() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = LayoutNode::Split {
            orientation: Orientation::Horizontal,
            children: vec![(leaf(a), 0.5), (leaf(b), 0.5)],
        };

        let geo = tree.compute_geometry(24, 80);
        assert_eq!(
            geo,
            vec![
                (
                    a,
                    PtyRect {
                        row: 0,
                        col: 0,
                        rows: 24,
                        cols: 40
                    }
                ),
                (
                    b,
                    PtyRect {
                        row: 0,
                        col: 40,
                        rows: 24,
                        cols: 40
                    }
                ),
            ]
        );
    }

    #[test]
    fn compute_geometry_should_assign_remainder_column_to_last_child_when_width_is_odd() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = LayoutNode::Split {
            orientation: Orientation::Horizontal,
            children: vec![(leaf(a), 0.5), (leaf(b), 0.5)],
        };

        let geo = tree.compute_geometry(24, 81);
        let a_rect = geo.iter().find(|(id, _)| *id == a).unwrap().1;
        let b_rect = geo.iter().find(|(id, _)| *id == b).unwrap().1;
        assert_eq!(a_rect.cols, 40);
        assert_eq!(
            b_rect.cols, 41,
            "the odd remainder column goes to the last child"
        );
        assert_eq!(a_rect.cols + b_rect.cols, 81);
    }

    #[test]
    fn compute_geometry_should_sum_to_parent_rect_when_tree_has_nested_splits() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        // Horizontal split: A | (B over C)
        let tree = LayoutNode::Split {
            orientation: Orientation::Horizontal,
            children: vec![
                (leaf(a), 0.5),
                (
                    LayoutNode::Split {
                        orientation: Orientation::Vertical,
                        children: vec![(leaf(b), 0.5), (leaf(c), 0.5)],
                    },
                    0.5,
                ),
            ],
        };

        let geo = tree.compute_geometry(24, 80);
        assert_eq!(geo.len(), 3);
        let total_area: u32 = geo.iter().map(|(_, r)| r.rows as u32 * r.cols as u32).sum();
        assert_eq!(total_area, 24 * 80);

        let b_rect = geo.iter().find(|(id, _)| *id == b).unwrap().1;
        let c_rect = geo.iter().find(|(id, _)| *id == c).unwrap().1;
        assert_eq!(b_rect.cols, 40);
        assert_eq!(c_rect.cols, 40);
        assert_eq!(b_rect.rows + c_rect.rows, 24);
    }

    #[test]
    fn layout_node_should_nest_split_under_target_leaf_when_split_called() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = leaf(a);
        tree.split(a, Orientation::Vertical, b, 24, 80).unwrap();

        match &tree {
            LayoutNode::Split {
                orientation,
                children,
            } => {
                assert_eq!(*orientation, Orientation::Vertical);
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].0, leaf(a));
                assert_eq!(children[1].0, leaf(b));
            }
            _ => panic!("expected split() to produce a Split node"),
        }
    }

    #[test]
    fn layout_node_should_leave_sibling_untouched_when_split_called_on_nested_leaf() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut tree = LayoutNode::Split {
            orientation: Orientation::Horizontal,
            children: vec![(leaf(a), 0.5), (leaf(b), 0.5)],
        };
        tree.split(b, Orientation::Vertical, c, 24, 80).unwrap();

        match &tree {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children[0].0, leaf(a), "sibling A must be untouched");
                assert!(matches!(children[1].0, LayoutNode::Split { .. }));
            }
            _ => panic!("expected root to remain a Split"),
        }
    }

    #[test]
    fn layout_node_split_should_reject_with_actual_dimensions_when_below_minimum_size() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = leaf(a);
        // 3 rows x 10 cols window, vertical split -> each side ~1 row, below MIN_PANE_ROWS.
        let err = tree.split(a, Orientation::Vertical, b, 3, 10).unwrap_err();
        assert!(matches!(err, LayoutError::BelowMinimumSize { .. }));
        assert_eq!(
            tree,
            leaf(a),
            "a rejected split must leave the tree untouched"
        );
    }

    #[test]
    fn layout_node_split_should_reject_horizontal_split_when_pane_rows_below_recommended_threshold()
    {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = leaf(a);
        // 15 rows x 80 cols window, horizontal split -> rows stay 15 (a
        // horizontal split only halves columns), below
        // RECOMMENDED_SPLIT_MIN_ROWS (20) but well above MIN_PANE_ROWS.
        let err = tree
            .split(a, Orientation::Horizontal, b, 15, 80)
            .unwrap_err();
        assert_eq!(err, LayoutError::BelowRecommendedSize { rows: 15 });
        assert_eq!(
            tree,
            leaf(a),
            "a rejected split must leave the tree untouched"
        );
        assert_eq!(
            err.to_string(),
            "Can't split: pane is 15 rows, minimum for a horizontal split is ~20 rows. \
             Resize your terminal or close another pane first."
        );
    }

    #[test]
    fn layout_node_split_should_allow_vertical_split_even_when_resulting_rows_below_recommended_threshold(
    ) {
        // Vertical splits are intentionally exempt from the
        // RECOMMENDED_SPLIT_MIN_ROWS check (they change row count, and are
        // already covered by the hard MIN_PANE_ROWS floor) — a normal
        // 24-row terminal split vertically into 12/12 must still succeed.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = leaf(a);
        tree.split(a, Orientation::Vertical, b, 24, 80).unwrap();
        assert!(matches!(tree, LayoutNode::Split { .. }));
    }

    #[test]
    fn layout_node_split_should_allow_horizontal_split_when_pane_rows_at_or_above_recommended_threshold(
    ) {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = leaf(a);
        tree.split(a, Orientation::Horizontal, b, 20, 80).unwrap();
        assert!(matches!(tree, LayoutNode::Split { .. }));
    }

    #[test]
    fn layout_node_should_collapse_split_when_sibling_pane_closes() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = LayoutNode::Split {
            orientation: Orientation::Horizontal,
            children: vec![(leaf(a), 0.5), (leaf(b), 0.5)],
        };
        match tree.remove(b) {
            RemoveOutcome::Collapsed(survivor) => assert_eq!(survivor, leaf(a)),
            RemoveOutcome::WindowEmpty => panic!("expected a surviving sibling"),
        }
    }

    #[test]
    fn layout_node_should_collapse_nested_split_when_grandchild_pane_closes() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let tree = LayoutNode::Split {
            orientation: Orientation::Horizontal,
            children: vec![
                (leaf(a), 0.5),
                (
                    LayoutNode::Split {
                        orientation: Orientation::Vertical,
                        children: vec![(leaf(b), 0.5), (leaf(c), 0.5)],
                    },
                    0.5,
                ),
            ],
        };
        match tree.remove(c) {
            RemoveOutcome::Collapsed(new_tree) => match new_tree {
                LayoutNode::Split { children, .. } => {
                    assert_eq!(children[0].0, leaf(a));
                    assert_eq!(
                        children[1].0,
                        leaf(b),
                        "B should survive C's removal, nested split collapsed away"
                    );
                }
                _ => panic!("expected the root to remain a 2-leaf Split after nested collapse"),
            },
            RemoveOutcome::WindowEmpty => panic!("expected survivors"),
        }
    }

    #[test]
    fn remove_last_leaf_reports_window_empty() {
        let a = Uuid::new_v4();
        let tree = leaf(a);
        assert_eq!(tree.remove(a), RemoveOutcome::WindowEmpty);
    }
}
