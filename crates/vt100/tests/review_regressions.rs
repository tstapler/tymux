//! Regression tests for defects found in review of the vt100 fork.
use vt100::Parser;

#[test]
fn resize_after_wide_chars_with_autowrap_off_does_not_panic() {
    // 31 wide chars fill 62 columns of a 92-column row with DECAWM off; shrinking to 61 columns
    // left the cursor past the last column.
    let mut p = Parser::new(2, 92, 0);
    p.process(b"\x1b[?7l");
    p.process("中".repeat(31).as_bytes());
    p.screen_mut().set_size(2, 61);
    p.process(b"a");
}

#[test]
fn resize_92_to_91_with_46_wide_chars_does_not_panic() {
    let mut p = Parser::new(2, 92, 0);
    p.process(b"\x1b[?7l");
    p.process("中".repeat(46).as_bytes());
    p.screen_mut().set_size(2, 91);
    p.process(b"a");
}

#[test]
fn resize_with_rep_of_wide_char_does_not_panic() {
    let mut p = Parser::new(21, 92, 0);
    p.process("中\x1b[65535b".as_bytes());
    p.screen_mut().set_size(4, 61);
    p.process(b"a");
}

#[test]
fn insert_cells_pushing_a_wide_continuation_off_the_row_does_not_panic() {
    // ICH shifts the row right; the continuation half of the wide char at the margin falls off.
    let mut p = Parser::new(2, 10, 0);
    p.process(b"\x1b[1;9H");
    p.process("中".as_bytes());
    p.process(b"\x1b[1;1H\x1b[1@\x1b[1;10Hz");
    p.process(b"\x1b[1;10H");
    p.process(b"q");
}

#[test]
fn delete_cells_splitting_a_wide_char_does_not_panic() {
    let mut p = Parser::new(2, 10, 0);
    p.process("a中b".as_bytes());
    p.process(b"\x1b[1;3H\x1b[1P");
    p.process(b"\x1b[1;2Hx\x1b[1;1H\x1b[1P\x1b[1;1Hy");
}

// Count-driven operations must clamp to their natural bound (columns, or the scroll region's
// height), so a huge count is the same as the bound and costs no more than it. The tests compare
// the full screen state, so any divergence between the clamped and unclamped path fails.

fn filled(rows: u16, cols: u16) -> Parser {
    let mut p = Parser::new(rows, cols, 50);
    for r in 0..rows {
        p.process(format!("\x1b[{};1H", r + 1).as_bytes());
        p.process(
            "0123456789abcdefghijklmnopqrstuvwxyz"
                .chars()
                .cycle()
                .skip(usize::from(r))
                .take(usize::from(cols))
                .collect::<String>()
                .as_bytes(),
        );
    }
    p.process(b"\x1b[3;5H");
    p
}

fn same_after(huge: &str, bound: &str) {
    for (rows, cols) in [(10, 20), (24, 80), (3, 7)] {
        let (mut a, mut b) = (filled(rows, cols), filled(rows, cols));
        a.process(huge.as_bytes());
        b.process(bound.as_bytes());
        assert_eq!(
            a.screen().contents_formatted(),
            b.screen().contents_formatted(),
            "{huge:?} should equal {bound:?} at {rows}x{cols}"
        );
        assert_eq!(a.screen().cursor_position(), b.screen().cursor_position());
        // scrollback pushed by SU must match too
        let (mut a, mut b) = (a, b);
        a.screen_mut().set_scrollback(usize::MAX);
        b.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(a.screen().scrollback(), b.screen().scrollback());
    }
}

#[test]
fn ich_with_a_huge_count_equals_ich_by_the_remaining_columns() {
    same_after("\x1b[65535@", "\x1b[200@");
    // 20 columns, cursor at col 4: 16 remaining
    let (mut a, mut b) = (filled(10, 20), filled(10, 20));
    a.process(b"\x1b[65535@");
    b.process(b"\x1b[16@");
    assert_eq!(
        a.screen().contents_formatted(),
        b.screen().contents_formatted()
    );
}

#[test]
fn su_and_sd_with_a_huge_count_equal_the_region_height() {
    same_after("\x1b[65535S", "\x1b[500S");
    same_after("\x1b[65535T", "\x1b[500T");
    let (mut a, mut b) = (filled(10, 20), filled(10, 20));
    a.process(b"\x1b[65535S");
    b.process(b"\x1b[10S");
    assert_eq!(
        a.screen().contents_formatted(),
        b.screen().contents_formatted()
    );
    let (mut a, mut b) = (filled(10, 20), filled(10, 20));
    a.process(b"\x1b[3;7r\x1b[65535T");
    b.process(b"\x1b[3;7r\x1b[5T");
    assert_eq!(
        a.screen().contents_formatted(),
        b.screen().contents_formatted()
    );
}

#[test]
fn il_and_dl_with_a_huge_count_equal_the_rows_left_in_the_region() {
    same_after("\x1b[65535L", "\x1b[500L");
    same_after("\x1b[65535M", "\x1b[500M");
    let (mut a, mut b) = (filled(10, 20), filled(10, 20));
    a.process(b"\x1b[65535L");
    b.process(b"\x1b[8L"); // cursor on row 3 of 10
    assert_eq!(
        a.screen().contents_formatted(),
        b.screen().contents_formatted()
    );
}

#[test]
fn dch_and_ech_with_a_huge_count_are_clamped_to_the_row() {
    same_after("\x1b[65535P", "\x1b[500P");
    same_after("\x1b[65535X", "\x1b[500X");
}

// OSC strings are stored and later forwarded to other terminals, so their size is capped and
// control bytes (C0, DEL, C1) never make it in. Bidi/scheme validation is the consumer's job.

fn link_of(p: &Parser, row: u16, col: u16) -> Option<(String, String)> {
    let idx = p.screen().cell(row, col)?.hyperlink();
    if idx == 0 {
        return None;
    }
    p.screen()
        .hyperlink(idx)
        .map(|(u, i)| (u.to_string(), i.to_string()))
}

#[test]
fn title_is_capped_at_1kib_on_a_char_boundary() {
    let mut p = Parser::new(5, 40, 0);
    // 3-byte chars so 1024 is not a multiple of the char width
    p.process(format!("\x1b]2;{}\x07", "€".repeat(2000)).as_bytes());
    let t = p.screen().title();
    assert!(t.len() <= 1024, "{}", t.len());
    assert!(t.len() > 1000);
    assert!(t.chars().all(|c| c == '€'));
}

#[test]
fn title_drops_control_characters_including_del() {
    // vte already discards C0 bytes inside an OSC, and the parser turns C1 code points into
    // their controls before OSC parsing, so DEL is the control that reaches the title.
    let mut p = Parser::new(5, 40, 0);
    p.process("\x1b]2;a\u{7f}b\u{1}c\x07".as_bytes());
    assert_eq!(p.screen().title(), "abc");
}

#[test]
fn over_long_uri_is_rejected_not_stored() {
    let mut p = Parser::new(5, 40, 0);
    let uri = format!("https://e.com/{}", "a".repeat(9000));
    p.process(format!("\x1b]8;;{uri}\x1b\\x\x1b]8;;\x1b\\").as_bytes());
    assert_eq!(link_of(&p, 0, 0), None);
    // the boundary itself is accepted
    let ok = format!(
        "https://e.com/{}",
        "a".repeat(8192 - "https://e.com/".len())
    );
    p.process(format!("\x1b]8;;{ok}\x1b\\y\x1b]8;;\x1b\\").as_bytes());
    assert_eq!(link_of(&p, 0, 1).map(|l| l.0.len()), Some(8192));
}

#[test]
fn uri_and_id_with_control_bytes_are_rejected() {
    let mut p = Parser::new(5, 40, 0);
    p.process("\x1b]8;;https://e.com/\u{7f}x\x1b\\a\x1b]8;;\x1b\\".as_bytes());
    assert_eq!(link_of(&p, 0, 0), None);
    p.process("\x1b]8;id=a\u{7f}b;https://e.com/\x1b\\b\x1b]8;;\x1b\\".as_bytes());
    assert_eq!(link_of(&p, 0, 1), None);
}

#[test]
fn over_long_id_is_rejected() {
    let mut p = Parser::new(5, 40, 0);
    p.process(
        format!(
            "\x1b]8;id={};https://e.com/\x1b\\a\x1b]8;;\x1b\\",
            "i".repeat(257)
        )
        .as_bytes(),
    );
    assert_eq!(link_of(&p, 0, 0), None);
    p.process(
        format!(
            "\x1b]8;id={};https://e.com/\x1b\\b\x1b]8;;\x1b\\",
            "i".repeat(256)
        )
        .as_bytes(),
    );
    assert_eq!(link_of(&p, 0, 1).map(|l| l.1.len()), Some(256));
}

#[test]
fn a_rejected_link_closes_the_previous_one() {
    let mut p = Parser::new(5, 40, 0);
    p.process("\x1b]8;;https://ok/\x1b\\a\x1b]8;;https://e.com/\u{7f}\x1b\\b".as_bytes());
    assert!(link_of(&p, 0, 0).is_some());
    assert_eq!(link_of(&p, 0, 1), None);
}

// vte keeps at most 16 OSC parameters and silently drops the rest, so a URI containing many `;`
// would otherwise become a link to a truncated, wrong target.

#[test]
fn uri_with_many_semicolons_is_not_silently_truncated() {
    let mut p = Parser::new(5, 40, 0);
    let uri = format!("https://e.com/{}", ["p"; 20].join(";"));
    p.process(format!("\x1b]8;;{uri}\x1b\\x\x1b]8;;\x1b\\").as_bytes());
    match link_of(&p, 0, 0) {
        None => {}
        Some((stored, _)) => assert_eq!(stored, uri, "stored a truncated URI"),
    }
}

#[test]
fn uri_with_a_few_semicolons_still_links() {
    let mut p = Parser::new(5, 40, 0);
    p.process(b"\x1b]8;;https://e.com/a;b;c\x1b\\x\x1b]8;;\x1b\\");
    assert_eq!(
        link_of(&p, 0, 0).map(|l| l.0),
        Some("https://e.com/a;b;c".to_string())
    );
}

// The hyperlink table holds 4096 entries; once full it must reclaim the ones no cell references
// rather than drop every later link until RIS.

fn emit_link(p: &mut Parser, n: usize) {
    p.process(format!("\x1b]8;id=i{n};https://e.com/{n}\x1b\\x\x1b]8;;\x1b\\").as_bytes());
}

#[test]
fn hyperlink_table_reclaims_dead_entries_when_full() {
    let mut p = Parser::new(24, 80, 0);
    p.process("\x1b]8;id=keep;https://keep.example/\x1b\\keep\x1b]8;;\x1b\\".as_bytes());
    // every distinct link overwrites the same cell, so only the latest is ever visible
    for n in 0..5000 {
        p.process(b"\x1b[3;1H");
        emit_link(&mut p, n);
    }
    p.process(b"\x1b[5;1H");
    emit_link(&mut p, 99_999);
    assert_eq!(
        link_of(&p, 4, 0),
        Some(("https://e.com/99999".into(), "i99999".into()))
    );
    // the last pre-compaction link and the old visible one still resolve to their own targets
    assert_eq!(
        link_of(&p, 2, 0),
        Some(("https://e.com/4999".into(), "i4999".into()))
    );
    assert_eq!(
        link_of(&p, 0, 0),
        Some(("https://keep.example/".into(), "keep".into()))
    );
    assert_eq!(
        link_of(&p, 0, 3),
        Some(("https://keep.example/".into(), "keep".into()))
    );
}

#[test]
fn hyperlink_table_keeps_links_referenced_from_scrollback_and_the_alternate_screen() {
    let mut p = Parser::new(5, 40, 100);
    p.process(
        "\x1b]8;id=sb;https://scrollback/\x1b\\s\x1b]8;;\x1b\\\r\n\r\n\r\n\r\n\r\n\r\n".as_bytes(),
    );
    p.process("\x1b[?1049h\x1b]8;id=alt;https://alt/\x1b\\a\x1b]8;;\x1b\\".as_bytes());
    for n in 0..5000 {
        p.process(b"\x1b[3;1H");
        emit_link(&mut p, n);
    }
    p.process(b"\x1b[5;1H");
    emit_link(&mut p, 99_999);
    assert_eq!(link_of(&p, 4, 0).map(|l| l.1), Some("i99999".into()));
    assert_eq!(
        link_of(&p, 0, 0),
        Some(("https://alt/".into(), "alt".into()))
    );
    p.process(b"\x1b[?1049l");
    p.screen_mut().set_scrollback(usize::MAX);
    assert_eq!(
        link_of(&p, 0, 0),
        Some(("https://scrollback/".into(), "sb".into()))
    );
}

// contents_formatted / contents_diff must reproduce faint (SGR 2) and bold independently:
// SGR 22 clears both, so dropping either needs 22 plus re-setting the survivor.

fn intensity_row(p: &Parser, cols: u16) -> Vec<(bool, bool)> {
    (0..cols)
        .map(|c| {
            let cell = p.screen().cell(0, c).unwrap();
            (cell.bold(), cell.dim())
        })
        .collect()
}

const INTENSITY_SAMPLE: &[u8] =
    b"\x1b[2mf\x1b[0mp\x1b[1mb\x1b[2mB\x1b[22mn\x1b[1;2mc\x1b[22;2md\x1b[22;1me\x1b[0mz";

#[test]
fn contents_formatted_round_trips_bold_and_faint_transitions() {
    let mut src = Parser::new(2, 20, 0);
    src.process(INTENSITY_SAMPLE);
    let mut copy = Parser::new(2, 20, 0);
    copy.process(&src.screen().contents_formatted());
    assert_eq!(intensity_row(&copy, 10), intensity_row(&src, 10));
    // sanity: the sample really exercises the transitions
    assert_eq!(
        intensity_row(&src, 10),
        [
            (false, true),
            (false, false),
            (true, false),
            (true, true),
            (false, false),
            (true, true),
            (false, true),
            (true, false),
            (false, false),
            (false, false)
        ][..10]
    );
}

#[test]
fn contents_diff_round_trips_a_faint_cell_becoming_bold_and_back() {
    let mut src = Parser::new(2, 20, 0);
    src.process(b"\x1b[2mabc\x1b[0m");
    let before = src.screen().clone();
    let mut copy = Parser::new(2, 20, 0);
    copy.process(&before.contents_formatted());

    src.process(b"\x1b[1;1H\x1b[1ma\x1b[22;2m\x1b[1;3Hc\x1b[1;2H\x1b[0mb");
    copy.process(&src.screen().contents_diff(&before));
    assert_eq!(intensity_row(&copy, 3), intensity_row(&src, 3));
}
