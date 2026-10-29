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
