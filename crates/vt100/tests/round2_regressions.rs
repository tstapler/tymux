//! Round-2 review regressions, each checked against xterm.js through the oracle harness.
use vt100::{Color, Parser};

fn parse(rows: u16, cols: u16, bytes: &[u8]) -> Parser {
    let mut p = Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

fn at(p: &Parser, row: u16, col: u16) -> String {
    p.screen().cell(row, col).unwrap().contents().to_string()
}

// 3. Operations that move or edit at the cursor must clear a pending wrap like xterm.js: the
// next character then lands on the last column instead of wrapping to the next row.

fn pending(op: &[u8]) -> Parser {
    parse(4, 8, &[b"abcdefgh", op, b"X"].concat())
}

#[test]
fn ich_dch_ech_clear_a_pending_wrap() {
    for op in [&b"\x1b[2@"[..], b"\x1b[2P", b"\x1b[3X"] {
        let p = pending(op);
        assert_eq!(at(&p, 0, 7), "X", "{op:?}");
        assert_eq!(at(&p, 1, 0), "", "{op:?}");
    }
    // the characters before the cursor are untouched by ICH/DCH at the last column
    assert_eq!(at(&pending(b"\x1b[2@"), 0, 6), "g");
}

#[test]
fn decrc_and_csi_u_restore_a_pending_wrap_as_the_last_column() {
    for op in [&b"\x1b7\x1b8"[..], b"\x1b[s\x1b[u"] {
        let p = pending(op);
        assert_eq!(at(&p, 0, 7), "X", "{op:?}");
        assert_eq!(at(&p, 1, 0), "", "{op:?}");
    }
}

#[test]
fn ind_and_vpa_clear_a_pending_wrap_before_moving() {
    let p = pending(b"\x1bD");
    assert_eq!(at(&p, 1, 7), "X");
    assert_eq!(at(&p, 0, 7), "h");
    let p = pending(b"\x1b[2d");
    assert_eq!(at(&p, 1, 7), "X");
    assert_eq!(at(&p, 0, 7), "h");
}

// 4. DECAWM off: the mark belongs to the cell at the cursor column unless the cursor is parked
// past it.

#[test]
fn combining_mark_after_a_last_column_write_with_decawm_off_joins_that_cell() {
    let p = parse(2, 8, "abcdefg\x1b[?7le\u{301}".as_bytes());
    assert_eq!(at(&p, 0, 7), "e\u{301}");
    assert_eq!(at(&p, 0, 6), "g");
}

#[test]
fn combining_mark_at_an_empty_last_column_joins_the_previous_cell() {
    let p = parse(2, 8, "abcdefg\x1b[?7l\u{301}".as_bytes());
    assert_eq!(at(&p, 0, 6), "g\u{301}");
}

// 5. A wide character that wraps blanks the unused last column with the current attributes.

#[test]
fn wrapped_wide_char_blanks_the_last_column_with_the_current_attributes() {
    let p = parse(2, 8, "abcdefg\x1b[4;44m中".as_bytes());
    let blank = p.screen().cell(0, 7).unwrap();
    assert!(blank.underline());
    assert_eq!(blank.bgcolor(), Color::Idx(4));
    assert_eq!(at(&p, 1, 0), "中");
}

// 6. Alternate screen keeps the cursor where it is, in both directions (xterm, xterm.js).

#[test]
fn alt_screen_entry_and_exit_carry_the_cursor() {
    let p = parse(3, 10, b"ab\x1b[?47hX");
    assert!(p.screen().alternate_screen());
    assert_eq!(at(&p, 0, 2), "X");
    let p = parse(3, 10, b"ab\x1b[?47hX\x1b[?47lY");
    assert_eq!(at(&p, 0, 3), "Y");
    let p = parse(3, 10, b"ab\x1b[?1047hX\x1b[?1047lY");
    assert_eq!(at(&p, 0, 3), "Y");
}

#[test]
fn alt_screen_1049_restores_the_saved_cursor_on_exit() {
    let p = parse(3, 10, b"ab\x1b[?1049hcd\x1b[?1049lZ");
    assert_eq!(at(&p, 0, 2), "Z");
}

#[test]
fn decstr_resets_cursor_style_visibility_margins_and_the_saved_cursor() {
    // xterm's soft reset (ReallyReset, charproc.c): cursor on, default shape, margins and saved
    // cursor home. Bracketed paste and focus reporting are left alone (only RIS clears them).
    let p = parse(
        6,
        10,
        b"\x1b[5 q\x1b[?25l\x1b[2;4r\x1b[3;3H\x1b7\x1b[?2004h\x1b[?1004h\x1b[!p\x1b8X",
    );
    let s = p.screen();
    assert_eq!(s.cursor_style(), 0);
    assert!(!s.hide_cursor());
    assert_eq!(s.scroll_region(), (0, 0));
    assert_eq!(at(&p, 0, 0), "X");
    assert!(s.bracketed_paste());
    assert!(s.focus_events());
}

// 8. RI blanks with the current background and never scrolls from above the top margin.

#[test]
fn ri_above_the_top_margin_moves_the_cursor_without_scrolling() {
    let mut rows = String::new();
    for r in 1..=8 {
        rows.push_str(&format!("\x1b[{r};1Hrow{r}"));
    }
    let before = parse(8, 12, rows.as_bytes());
    let mut p = parse(8, 12, rows.as_bytes());
    p.process(b"\x1b[4;7r\x1b[2;1H\x1bM"); // row 2 -> row 1, both above the margin
    p.process(b"\x1bM"); // already on row 1 (top of screen): stays, region untouched
    for r in 0..8 {
        assert_eq!(at(&p, r, 3), at(&before, r, 3), "row {r} scrolled");
    }
    assert_eq!(p.screen().cursor_position(), (0, 0));
}

#[test]
fn ri_at_the_top_margin_scrolls_the_region_with_the_current_background() {
    let mut rows = String::new();
    for r in 1..=8 {
        rows.push_str(&format!("\x1b[{r};1Hrow{r}"));
    }
    let mut p = parse(8, 12, rows.as_bytes());
    p.process(b"\x1b[4;7r\x1b[44m\x1b[4;1H\x1bM");
    assert_eq!(at(&p, 3, 0), "");
    assert_eq!(p.screen().cell(3, 0).unwrap().bgcolor(), Color::Idx(4));
    assert_eq!(at(&p, 4, 3), "4"); // old row 4 moved down one
    assert_eq!(at(&p, 7, 3), "8"); // below the region: untouched
}

// 9. OSC 0/2 titles keep their semicolons.

#[test]
fn titles_with_semicolons_are_kept_whole() {
    assert_eq!(parse(2, 20, b"\x1b]2;a;b;c\x07").screen().title(), "a;b;c");
    assert_eq!(parse(2, 20, b"\x1b]0;x;y\x1b\\").screen().title(), "x;y");
    assert_eq!(parse(2, 20, b"\x1b]2;x;\x7fy\x07").screen().title(), "x;y");
}
