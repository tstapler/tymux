//! Resize with reflow, ported from xterm.js 6.0.0 (`Buffer.resize`, `BufferReflow.ts`) so a
//! resized terminal lands on the same grid the browser's xterm.js shows. Where xterm.js
//! deviates from other terminals we copy it; see the vt100 README's list of deviations.
//!
//! The grid's scrollback and visible rows are treated as one list of "lines" (xterm's
//! `lines`), with `ybase` the index of the first visible line. Like xterm, `Row::wrapped` marks
//! the *continuation* row.
use super::{Grid, Pos, Size};
use crate::row::Row;
use crate::Cell;

fn blank_line(cols: u16, wrapped: bool) -> Row {
    let mut row = Row::new(cols);
    row.wrapped = wrapped;
    row
}

fn width_at(row: &Row, col: i64) -> u8 {
    match usize::try_from(col).ok().and_then(|c| row.cells.get(c)) {
        None => 0,
        Some(c) if c.is_wide_continuation() => 0,
        Some(c) if c.is_wide() => 2,
        Some(_) => 1,
    }
}

fn has_content(row: &Row, col: usize) -> bool {
    row.cells
        .get(col)
        .is_some_and(|c| c.has_contents() && !c.is_wide_continuation())
}

/// xterm's `getTrimmedLength`: up to and including the last cell with content. Cells that only
/// carry attributes (a background colour) are trimmed, as in xterm.
fn trimmed_len(row: &Row) -> usize {
    for (i, c) in row.cells.iter().enumerate().rev() {
        if c.has_contents() && !c.is_wide_continuation() {
            return i + if c.is_wide() { 2 } else { 1 };
        }
    }
    0
}

fn wrapped_line_trimmed_len(group: &[Row], i: usize, cols: usize) -> usize {
    if i + 1 == group.len() {
        return trimmed_len(&group[i]);
    }
    let ends_in_null =
        !has_content(&group[i], cols - 1) && width_at(&group[i], cols as i64 - 1) == 1;
    let next_starts_wide = width_at(&group[i + 1], 0) == 2;
    if ends_in_null && next_starts_wide {
        cols - 1
    } else {
        cols
    }
}

/// Copies `len` cells, tolerating overlap when `src == dst` and clamping to the row lengths.
fn copy_cells(
    group: &mut [Row],
    src: usize,
    src_col: usize,
    dst: usize,
    dst_col: usize,
    len: usize,
) {
    let avail_src = group[src].cells.len().saturating_sub(src_col);
    let avail_dst = group[dst].cells.len().saturating_sub(dst_col);
    let len = len.min(avail_src).min(avail_dst);
    let tmp: Vec<Cell> = group[src].cells[src_col..src_col + len].to_vec();
    for (k, cell) in tmp.into_iter().enumerate() {
        group[dst].cells[dst_col + k] = cell;
    }
}

fn replace_cells(row: &mut Row, mut start: usize, end: usize) {
    if start > 0 && width_at(row, start as i64 - 1) == 2 {
        row.cells[start - 1] = Cell::new();
    }
    if end < row.cells.len() && end > 0 && width_at(row, end as i64 - 1) == 2 {
        row.cells[end] = Cell::new();
    }
    while start < end && start < row.cells.len() {
        row.cells[start] = Cell::new();
        start += 1;
    }
}

/// Precomputes where each wrapped line breaks at the new width; a wide glyph that would straddle
/// the edge moves to the next line, leaving that line one cell short.
fn new_line_lengths(group: &[Row], old_cols: usize, new_cols: usize) -> Vec<usize> {
    let mut out = vec![];
    let needed: usize = (0..group.len())
        .map(|i| wrapped_line_trimmed_len(group, i, old_cols))
        .sum();
    let mut src_col = 0usize;
    let mut src_line = 0usize;
    let mut available = 0usize;
    while available < needed {
        if needed - available < new_cols {
            out.push(needed - available);
            break;
        }
        src_col += new_cols;
        let old_trimmed = wrapped_line_trimmed_len(group, src_line, old_cols);
        if src_col > old_trimmed {
            src_col -= old_trimmed;
            src_line += 1;
        }
        let ends_with_wide = group
            .get(src_line)
            .is_some_and(|r| width_at(r, src_col as i64 - 1) == 2);
        if ends_with_wide {
            src_col -= 1;
        }
        let len = if ends_with_wide {
            new_cols - 1
        } else {
            new_cols
        };
        out.push(len);
        available += len;
    }
    out
}

struct Lines {
    lines: Vec<Row>,
    ybase: usize,
    y: usize,
    saved_y: usize,
    max_len: usize,
}

impl Lines {
    fn reflow_larger(&mut self, old_cols: usize, new_cols: usize, new_rows: usize) {
        let abs_y = self.ybase + self.y;
        let mut remove: Vec<(usize, usize)> = vec![];
        let mut y = 0;
        while y + 1 < self.lines.len() {
            if !self.lines[y + 1].wrapped {
                y += 1;
                continue;
            }
            let mut i = y + 1;
            while i < self.lines.len() && self.lines[i].wrapped {
                i += 1;
            }
            let n = i - y;
            // The cursor's own wrapped line is left for the program to repaint.
            if abs_y >= y && abs_y < i {
                y += n;
                continue;
            }
            let group = &mut self.lines[y..i];
            for row in group.iter_mut() {
                if row.cells.len() < new_cols {
                    row.cells.resize(new_cols, Cell::new());
                }
            }
            let mut dest_idx = 0usize;
            let mut dest_col = wrapped_line_trimmed_len(group, 0, old_cols);
            let mut src_idx = 1usize;
            let mut src_col = 0usize;
            while src_idx < n {
                let src_trimmed = wrapped_line_trimmed_len(group, src_idx, old_cols);
                let src_remaining = src_trimmed - src_col;
                let dest_remaining = new_cols - dest_col;
                let to_copy = src_remaining.min(dest_remaining);
                copy_cells(group, src_idx, src_col, dest_idx, dest_col, to_copy);
                dest_col += to_copy;
                if dest_col == new_cols {
                    dest_idx += 1;
                    dest_col = 0;
                }
                src_col += to_copy;
                if src_col == src_trimmed {
                    src_idx += 1;
                    src_col = 0;
                }
                // a wide glyph can't end a line: pull it down to start the next one
                if dest_col == 0
                    && dest_idx != 0
                    && width_at(&group[dest_idx - 1], new_cols as i64 - 1) == 2
                {
                    copy_cells(group, dest_idx - 1, new_cols - 1, dest_idx, dest_col, 1);
                    dest_col += 1;
                    group[dest_idx - 1].cells[new_cols - 1] = Cell::new();
                }
            }
            replace_cells(&mut group[dest_idx], dest_col, new_cols);

            let mut count = 0;
            for k in (1..n).rev() {
                if k > dest_idx || trimmed_len(&group[k]) == 0 {
                    count += 1;
                } else {
                    break;
                }
            }
            if count > 0 {
                remove.push((y + n - count, count));
            }
            y += n;
        }
        if remove.is_empty() {
            return;
        }
        let mut removed = 0;
        let mut keep = Vec::with_capacity(self.lines.len());
        let mut r = remove.iter().peekable();
        let old = std::mem::take(&mut self.lines);
        let mut skip = 0usize;
        for (idx, line) in old.into_iter().enumerate() {
            if let Some(&&(start, count)) = r.peek() {
                if idx == start {
                    skip = count;
                    removed += count;
                    r.next();
                }
            }
            if skip > 0 {
                skip -= 1;
            } else {
                keep.push(line);
            }
        }
        self.lines = keep;
        for _ in 0..removed {
            if self.ybase == 0 {
                self.y = self.y.saturating_sub(1);
                if self.lines.len() < new_rows {
                    self.lines.push(blank_line(new_cols as u16, false));
                }
            } else {
                self.ybase -= 1;
            }
        }
        self.saved_y = self.saved_y.saturating_sub(removed);
    }

    fn reflow_smaller(&mut self, old_cols: usize, new_cols: usize, new_rows: usize) {
        let mut to_insert: Vec<(usize, Vec<Row>)> = vec![]; // (index just past the group, new lines)
        let mut count_to_insert = 0usize;
        // xterm's `lines.pop()` only shrinks its logical length; the popped lines stay readable
        // by index and the scan below (bounded by the original length) keeps processing them, so
        // `live_len` stands in for the length and `self.lines` keeps every line until the end.
        let mut live_len = self.lines.len();
        let mut y = live_len;
        while y > 0 {
            y -= 1;
            if !self.lines[y].wrapped && trimmed_len(&self.lines[y]) <= new_cols {
                continue;
            }
            let last = y;
            while self.lines[y].wrapped && y > 0 {
                y -= 1;
            }
            let n = last - y + 1;
            let abs_y = self.ybase + self.y;
            if abs_y >= y && abs_y < y + n {
                continue;
            }
            let mut group: Vec<Row> = (y..=last)
                .map(|i| std::mem::replace(&mut self.lines[i], Row::new(0)))
                .collect();
            for row in &mut group {
                if row.cells.len() < old_cols {
                    row.cells.resize(old_cols, Cell::new());
                }
            }
            let last_len = trimmed_len(&group[n - 1]);
            let dest = new_line_lengths(&group, old_cols, new_cols);
            let to_add = dest.len() as i64 - n as i64;
            let trimmed = if self.ybase == 0 && self.y + 1 != live_len {
                (self.y as i64 - self.max_len as i64 + to_add).max(0)
            } else {
                (live_len as i64 - self.max_len as i64 + to_add).max(0)
            };
            let add = to_add.max(0) as usize;
            count_to_insert += add;
            for _ in 0..add {
                group.push(blank_line(new_cols as u16, true)); // only ever indexed below new_cols
            }

            let mut dest_idx = dest.len() as i64 - 1;
            let mut dest_col = dest.get(dest_idx as usize).copied().unwrap_or(0);
            if dest_col == 0 {
                dest_idx -= 1;
                dest_col = usize::try_from(dest_idx)
                    .ok()
                    .and_then(|d| dest.get(d))
                    .copied()
                    .unwrap_or(0);
            }
            let mut src_idx = group.len() as i64 - add as i64 - 1;
            let mut src_col = last_len;
            while src_idx >= 0 {
                let to_copy = src_col.min(dest_col);
                let Some(d) = usize::try_from(dest_idx).ok().filter(|&d| d < group.len()) else {
                    break;
                };
                copy_cells(
                    &mut group,
                    src_idx as usize,
                    src_col - to_copy,
                    d,
                    dest_col - to_copy,
                    to_copy,
                );
                dest_col -= to_copy;
                if dest_col == 0 {
                    dest_idx -= 1;
                    dest_col = usize::try_from(dest_idx)
                        .ok()
                        .and_then(|d| dest.get(d))
                        .copied()
                        .unwrap_or(0);
                }
                src_col -= to_copy;
                if src_col == 0 {
                    src_idx -= 1;
                    src_col = wrapped_line_trimmed_len(&group, src_idx.max(0) as usize, old_cols);
                }
                if to_copy == 0 && dest_col == 0 && src_col != 0 && dest_idx < 0 {
                    break;
                }
            }
            for (i, row) in group.iter_mut().enumerate() {
                if let Some(&len) = dest.get(i) {
                    if len < new_cols {
                        row.cells[len] = Cell::new();
                    }
                }
            }

            let mut adjust = to_add - trimmed;
            while adjust > 0 {
                adjust -= 1;
                if self.ybase == 0 {
                    if self.y + 1 < new_rows {
                        self.y += 1;
                        live_len = live_len.saturating_sub(1);
                    } else {
                        self.ybase += 1;
                    }
                } else {
                    let cap = self.max_len.min(live_len + count_to_insert);
                    if (self.ybase as i64) < cap as i64 - new_rows as i64 {
                        self.ybase += 1;
                    }
                }
            }
            self.saved_y = (self.saved_y + add).min(self.ybase + new_rows - 1);

            let extra = group.split_off(n);
            for (k, row) in group.into_iter().enumerate() {
                if y + k < self.lines.len() {
                    self.lines[y + k] = row;
                }
            }
            if !extra.is_empty() {
                to_insert.push((last + 1, extra));
            }
        }

        self.lines.truncate(live_len);
        if to_insert.is_empty() {
            return;
        }
        to_insert.sort_by_key(|(at, _)| *at);
        let old = std::mem::take(&mut self.lines);
        let old_len = old.len();
        let mut out = Vec::with_capacity(old_len + count_to_insert);
        let mut pending = to_insert.into_iter().peekable();
        for (idx, line) in old.into_iter().enumerate() {
            out.push(line);
            while pending.peek().is_some_and(|(at, _)| *at == idx + 1) {
                out.extend(pending.next().unwrap().1);
            }
        }
        for (_, rows) in pending {
            out.extend(rows); // anchors popped off the end land at the end
        }
        let excess = out.len().saturating_sub(self.max_len);
        out.drain(..excess);
        self.lines = out;
    }
}

impl Grid {
    /// Resizes to `size`. `reflow` rewraps soft-wrapped lines to the new width (the primary
    /// screen); the alternate screen passes `false` and just crops/extends.
    pub fn set_size(&mut self, size: Size, reflow: bool) {
        // xterm.js ignores a resize to the current size: margins and a pending wrap survive it.
        if size == self.size {
            return;
        }
        let old_cols = usize::from(self.size.cols);
        let (new_cols, new_rows) = (usize::from(size.cols), usize::from(size.rows));
        let old_rows = usize::from(self.size.rows);
        let sb_len = self.scrollback.len();

        let lines: Vec<Row> = std::mem::take(&mut self.scrollback)
            .into_iter()
            .chain(std::mem::take(&mut self.rows))
            .collect();
        let mut b = Lines {
            ybase: sb_len,
            y: usize::from(self.pos.row),
            saved_y: if self.saved_set {
                sb_len + usize::from(self.saved_pos.row)
            } else {
                0
            },
            max_len: self.scrollback_len + new_rows,
            lines,
        };
        // Only the visible rows need the new width now; scrollback rows stay as narrow as they
        // are (blank beyond their length) until a reflow touches them or they are scrolled into
        // view (`Grid::set_scrollback`), so widening doesn't allocate cols x scrollback cells.
        if old_cols < new_cols {
            for line in b.lines.iter_mut().skip(sb_len) {
                line.cells.resize(new_cols, Cell::new());
            }
        }
        let mut add_to_y = 0;
        if old_rows < new_rows {
            for _ in old_rows..new_rows {
                if b.lines.len() < new_rows + b.ybase {
                    if b.ybase > 0 && b.lines.len() <= b.ybase + b.y + add_to_y + 1 {
                        b.ybase -= 1;
                        add_to_y += 1;
                    } else {
                        b.lines.push(blank_line(size.cols, false));
                    }
                }
            }
        } else {
            for _ in new_rows..old_rows {
                if b.lines.len() > new_rows + b.ybase {
                    if b.lines.len() > b.ybase + b.y + 1 {
                        b.lines.pop();
                    } else {
                        b.ybase += 1;
                    }
                }
            }
        }
        let old_max = self.scrollback_len + old_rows;
        if b.max_len < old_max {
            let trim = b.lines.len().saturating_sub(b.max_len);
            if trim > 0 {
                b.lines.drain(..trim);
                b.ybase = b.ybase.saturating_sub(trim);
                b.saved_y = b.saved_y.saturating_sub(trim);
                self.freed_rows += trim as u64;
            }
        }
        b.y = b.y.min(new_rows - 1) + add_to_y;
        // xterm.js can't reflow to one column either (a wide glyph never fits, so the line-length
        // computation would not terminate); the screen is simply cropped there
        if reflow && old_cols != new_cols && new_cols >= 2 {
            if new_cols > old_cols {
                b.reflow_larger(old_cols, new_cols, new_rows);
            } else {
                b.reflow_smaller(old_cols, new_cols, new_rows);
            }
        }
        for line in &mut b.lines {
            if line.cells.len() > new_cols {
                line.cells.truncate(new_cols);
                // hand the cropped cells' memory back instead of pinning the old width
                line.cells.shrink_to_fit();
                if let Some(last) = line.cells.last_mut() {
                    if last.is_wide() {
                        *last = Cell::new();
                    }
                }
            }
        }
        for line in b.lines.iter_mut().skip(b.ybase) {
            line.cells.resize(new_cols, Cell::new());
        }

        // defensive: the invariant lines == ybase + rows holds by construction
        let need = b.ybase + new_rows;
        while b.lines.len() < need {
            b.lines.push(blank_line(size.cols, false));
        }
        b.lines.truncate(need);
        let rows = b.lines.split_off(b.ybase);
        self.scrollback = b.lines.into();
        self.rows = rows;

        self.size = size;
        self.pos = Pos {
            row: b.y.min(new_rows - 1) as u16,
            col: self.pos.col.min(size.cols - 1),
        };
        self.saved_pos = Pos {
            row: b.saved_y.saturating_sub(b.ybase).min(new_rows - 1) as u16,
            col: self.saved_pos.col.min(size.cols - 1),
        };
        self.scroll_top = 0;
        self.scroll_bottom = size.rows - 1;
        self.scrollback_offset = self.scrollback_offset.min(self.scrollback.len());
        self.pad_scrollback_window();

        // xterm's `setupTabStops(newCols)`: resume the default every-8th pattern from the nearest
        // stop below the new width (re-adding stops cleared with TBC, as xterm.js does).
        self.tab_stops.resize(new_cols, false);
        let mut c = new_cols - 1;
        while c > 0 && !self.tab_stops[c] {
            c -= 1;
        }
        for i in (c..new_cols).step_by(8) {
            self.tab_stops[i] = i != 0;
        }
    }

    /// Scrollback rows may be narrower than the screen (see `set_size`); pad the ones about to be
    /// shown so every visible row is exactly `cols` wide.
    pub(super) fn pad_scrollback_window(&mut self) {
        let cols = usize::from(self.size.cols);
        let shown = self.scrollback_offset.min(self.scrollback.len());
        let skip = self.scrollback.len() - shown;
        for row in self.scrollback.iter_mut().skip(skip).take(self.rows.len()) {
            if row.cells.len() < cols {
                row.cells.resize(cols, Cell::new());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells (by capacity, i.e. memory actually held) across scrollback and the visible rows.
    fn held_cells(g: &Grid) -> usize {
        g.scrollback
            .iter()
            .chain(g.rows.iter())
            .map(|r| r.cells.capacity())
            .sum()
    }

    fn filled(rows: u16, cols: u16, scrollback: usize, lines: usize) -> Grid {
        let mut g = Grid::new(Size { rows, cols }, scrollback);
        g.allocate_rows();
        for i in 0..lines {
            for (c, ch) in format!("{i} lorem ipsum")
                .chars()
                .enumerate()
                .take(usize::from(cols))
            {
                g.drawing_cell_mut(Pos {
                    row: rows - 1,
                    col: c as u16,
                })
                .unwrap()
                .set(ch, crate::attrs::Attrs::default());
            }
            g.scroll_up(1);
        }
        g
    }

    #[test]
    fn widening_does_not_pad_scrollback_rows() {
        let mut g = filled(24, 80, 5000, 6000);
        let before = held_cells(&g);
        g.set_size(
            Size {
                rows: 24,
                cols: 2000,
            },
            true,
        );
        // only the 24 visible rows grow: 24 * 2000 extra cells, not 5000 * 2000
        let after = held_cells(&g);
        assert!(
            after < before + 24 * 2000 + 1000,
            "{before} -> {after} cells held"
        );
    }

    #[test]
    fn shrinking_gives_cropped_width_back() {
        let mut g = filled(24, 80, 5000, 6000);
        g.set_size(
            Size {
                rows: 24,
                cols: 2000,
            },
            true,
        );
        g.set_size(Size { rows: 24, cols: 80 }, true);
        // back to roughly the original footprint: no row keeps the 2000-column capacity
        assert!(
            held_cells(&g) < 5024 * 80 * 2,
            "{} cells held",
            held_cells(&g)
        );
    }

    #[test]
    fn scrolling_back_after_widening_shows_full_width_rows() {
        let mut g = filled(24, 80, 100, 200);
        g.set_size(
            Size {
                rows: 24,
                cols: 120,
            },
            true,
        );
        g.set_scrollback(50);
        for r in 0..24 {
            for c in 0..120 {
                assert!(
                    g.visible_cell(Pos { row: r, col: c }).is_some(),
                    "r{r} c{c}"
                );
            }
        }
    }
}
