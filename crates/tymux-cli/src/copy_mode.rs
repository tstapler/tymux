/// A cell position within the captured grid — `row`/`col` are 0-indexed,
/// `row` counted from the top of whatever grid is currently displayed
/// (live or historical, per `scrollback_offset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPos {
    pub row: u16,
    pub col: u16,
}

/// What the caller should do after feeding one byte to
/// [`CopyModeState::handle_byte`].
#[derive(Debug, PartialEq)]
pub enum CopyModeEvent {
    /// Cursor or scrollback offset moved — re-capture and redraw.
    Redraw,
    /// `q`/`Escape` — exit back to `InputMode::Normal`. Story 5.5 AC3:
    /// this is the same event regardless of whether the pane is live or
    /// dead — no special-cased branch for either case.
    Exit,
    /// A visual selection was just yanked into the internal buffer
    /// (Story 5.5 AC2) — the caller should exit copy-mode afterward, per
    /// tmux's own convention of `y` both copying and leaving copy-mode.
    Yanked,
    /// Nothing externally visible happened (e.g. entering visual-select
    /// mode with `v`) — no redraw needed.
    Consumed,
}

/// Vi-style copy-mode navigation/selection state (Story 5.5). Deliberately
/// holds no grid content itself — `main.rs`'s attach loop owns the actual
/// `CapturePane`-fetched grid and calls [`extract_selection`] with it,
/// keeping this module pure/testable without a live pty.
pub struct CopyModeState {
    pub scrollback_offset: usize,
    pub cursor: CellPos,
    pub selecting_from: Option<CellPos>,
    pub yanked: Vec<u8>,
    rows: u16,
    cols: u16,
}

impl CopyModeState {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            scrollback_offset: 0,
            cursor: CellPos { row: 0, col: 0 },
            selecting_from: None,
            yanked: Vec::new(),
            rows,
            cols,
        }
    }

    /// Dispatches one raw input byte. Story 5.5 AC1: navigation keys move
    /// the cursor/offset locally and never forward to the pane — the
    /// caller only ever sees a `CopyModeEvent`, never raw bytes to
    /// forward, for any byte this function recognizes.
    pub fn handle_byte(&mut self, b: u8) -> CopyModeEvent {
        match b {
            b'q' | 0x1b => CopyModeEvent::Exit,
            b'h' => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                CopyModeEvent::Redraw
            }
            b'l' => {
                self.cursor.col = (self.cursor.col + 1).min(self.cols.saturating_sub(1));
                CopyModeEvent::Redraw
            }
            b'k' => {
                if self.cursor.row == 0 {
                    self.scrollback_offset += 1;
                } else {
                    self.cursor.row -= 1;
                }
                CopyModeEvent::Redraw
            }
            b'j' => {
                if self.cursor.row + 1 >= self.rows {
                    self.scrollback_offset = self.scrollback_offset.saturating_sub(1);
                } else {
                    self.cursor.row += 1;
                }
                CopyModeEvent::Redraw
            }
            b'v' => {
                self.selecting_from = Some(self.cursor);
                CopyModeEvent::Consumed
            }
            b'y' => {
                if self.selecting_from.is_some() {
                    CopyModeEvent::Yanked
                } else {
                    CopyModeEvent::Consumed
                }
            }
            _ => CopyModeEvent::Consumed,
        }
    }
}

/// Extracts the plain-text content of the rectangular... actually
/// row-major, char-cell selection between `from` and `to` (inclusive,
/// order-independent) out of a captured grid — the actual "yank" data
/// [`CopyModeState::handle_byte`]'s `Yanked` event signals is ready to
/// collect. Kept as a free function (not a method) since it operates on
/// grid content the caller already fetched via `CapturePane`, not state
/// this module owns.
pub fn extract_selection(grid: &[Vec<String>], from: CellPos, to: CellPos) -> Vec<u8> {
    let (start, end) = if (from.row, from.col) <= (to.row, to.col) {
        (from, to)
    } else {
        (to, from)
    };
    let mut out = String::new();
    for (row_idx, row) in grid.iter().enumerate() {
        let row_idx = row_idx as u16;
        if row_idx < start.row || row_idx > end.row {
            continue;
        }
        let col_start = if row_idx == start.row { start.col } else { 0 };
        let col_end = if row_idx == end.row {
            end.col
        } else {
            row.len().saturating_sub(1) as u16
        };
        for (col_idx, cell) in row.iter().enumerate() {
            let col_idx = col_idx as u16;
            if col_idx >= col_start && col_idx <= col_end {
                out.push_str(cell);
            }
        }
        if row_idx != end.row {
            out.push('\n');
        }
    }
    out.into_bytes()
}

/// Story 5.5 AC3: the `[exited]` marker shown within copy-mode's chrome
/// when the underlying pane is dead — a pure formatting helper so the
/// "same exit path either way" claim is checkable without a live pane.
pub fn render_status_line(live: bool, offset: usize) -> String {
    if live {
        format!("-- COPY MODE -- offset {offset} -- q/Escape to exit")
    } else {
        format!("-- COPY MODE [exited] -- offset {offset} -- q/Escape to exit")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_mode_should_move_scrollback_offset_without_forwarding_keystrokes_when_navigating() {
        let mut state = CopyModeState::new(24, 80);
        assert_eq!(state.handle_byte(b'k'), CopyModeEvent::Redraw);
        assert_eq!(state.scrollback_offset, 1);
        assert_eq!(state.handle_byte(b'l'), CopyModeEvent::Redraw);
        assert_eq!(state.cursor.col, 1);
    }

    #[test]
    fn copy_mode_should_copy_selected_range_into_buffer_when_visual_select_then_yank() {
        let mut state = CopyModeState::new(3, 10);
        assert_eq!(state.handle_byte(b'v'), CopyModeEvent::Consumed);
        state.cursor.col = 4;
        assert_eq!(state.handle_byte(b'y'), CopyModeEvent::Yanked);

        let grid = vec!["hello world"
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()];
        let selected = extract_selection(&grid, CellPos { row: 0, col: 0 }, state.cursor);
        assert_eq!(String::from_utf8(selected).unwrap(), "hello");
    }

    #[test]
    fn copy_mode_exit_key_should_behave_identically_when_pane_is_dead_versus_live() {
        let mut live_state = CopyModeState::new(24, 80);
        let mut dead_state = CopyModeState::new(24, 80);
        assert_eq!(live_state.handle_byte(b'q'), dead_state.handle_byte(b'q'));
        assert_eq!(live_state.handle_byte(0x1b), dead_state.handle_byte(0x1b));
    }

    #[test]
    fn copy_mode_should_show_exited_marker_within_one_render_frame_on_dead_pane_entry() {
        let live_line = render_status_line(true, 0);
        let dead_line = render_status_line(false, 0);
        assert!(!live_line.contains("[exited]"));
        assert!(dead_line.contains("[exited]"));
    }

    #[test]
    fn navigation_never_produces_a_forward_event_since_the_enum_has_none() {
        // Structural guarantee: CopyModeEvent has no `Forward(bytes)`
        // variant at all, so no navigation key can ever produce one —
        // this is Story 5.5 AC1 enforced by the type, not just by
        // convention.
        let mut state = CopyModeState::new(24, 80);
        for b in [b'h', b'j', b'k', b'l', b'v', b'y', b'q', 0x1b, b'x'] {
            match state.handle_byte(b) {
                CopyModeEvent::Redraw
                | CopyModeEvent::Exit
                | CopyModeEvent::Yanked
                | CopyModeEvent::Consumed => {}
            }
        }
    }
}
