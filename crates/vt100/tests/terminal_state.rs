//! Tests for the terminal state a snapshot carries beyond the cell grid (title, input/output
//! modes, cursor, scroll region, tab stops, charsets) and for UTF-8 C1 handling.
//! Spec: xterm ctlseqs (https://invisible-island.net/xterm/ctlseqs/ctlseqs.html), ECMA-48.
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

fn parse(bytes: &[u8]) -> Parser {
    let mut p = Parser::new(10, 40, 0);
    p.process(bytes);
    p
}

#[test]
fn osc_0_and_2_set_the_title_osc_1_does_not() {
    // ctlseqs "OSC Ps ; Pt ST": 0 = icon name + title, 1 = icon name, 2 = title
    assert_eq!(parse(b"\x1b]0;both\x07").screen().title(), "both");
    assert_eq!(parse(b"\x1b]2;title\x1b\\").screen().title(), "title");
    assert_eq!(parse(b"\x1b]2;a\x07\x1b]1;icon\x07").screen().title(), "a");
}

#[test]
fn deckpam_and_deckpnm_toggle_application_keypad() {
    // ctlseqs "ESC =" / "ESC >"
    assert!(parse(b"\x1b=").screen().application_keypad());
    assert!(!parse(b"\x1b=\x1b>").screen().application_keypad());
}

#[test]
fn private_modes_set_and_reset() {
    // ctlseqs "CSI ? Pm h/l": 1 DECCKM, 2004 bracketed paste, 1004 focus, 5 DECSCNM, 6 DECOM, 7 DECAWM, 25 DECTCEM
    let on = parse(b"\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?5h\x1b[?6h\x1b[?7l\x1b[?25l");
    let s = on.screen();
    assert!(s.application_cursor());
    assert!(s.bracketed_paste());
    assert!(s.focus_events());
    assert!(s.reverse_video());
    assert!(s.origin_mode());
    assert!(!s.autowrap());
    assert!(s.hide_cursor());

    let off = parse(
        b"\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?5h\x1b[?6h\x1b[?7l\x1b[?25l\
          \x1b[?1l\x1b[?2004l\x1b[?1004l\x1b[?5l\x1b[?6l\x1b[?7h\x1b[?25h",
    );
    let s = off.screen();
    assert!(!s.application_cursor());
    assert!(!s.bracketed_paste());
    assert!(!s.focus_events());
    assert!(!s.reverse_video());
    assert!(!s.origin_mode());
    assert!(s.autowrap());
    assert!(!s.hide_cursor());
}

#[test]
fn ansi_modes_irm_and_lnm() {
    // ECMA-48 SM/RM 4 (IRM) and 20 (LNM)
    let p = parse(b"\x1b[4h\x1b[20h");
    assert!(p.screen().insert_mode() && p.screen().newline_mode());
    let p = parse(b"\x1b[4h\x1b[20h\x1b[4l\x1b[20l");
    assert!(!p.screen().insert_mode() && !p.screen().newline_mode());
}

#[test]
fn mouse_modes_and_encodings() {
    // ctlseqs: ?9 X10, ?1000 normal, ?1002 button, ?1003 any; encodings ?1005 utf8, ?1006 sgr, ?1015 urxvt
    for (seq, mode) in [
        (&b"\x1b[?9h"[..], MouseProtocolMode::Press),
        (b"\x1b[?1000h", MouseProtocolMode::PressRelease),
        (b"\x1b[?1002h", MouseProtocolMode::ButtonMotion),
        (b"\x1b[?1003h", MouseProtocolMode::AnyMotion),
    ] {
        assert_eq!(parse(seq).screen().mouse_protocol_mode(), mode);
    }
    for (seq, enc) in [
        (&b"\x1b[?1005h"[..], MouseProtocolEncoding::Utf8),
        (b"\x1b[?1006h", MouseProtocolEncoding::Sgr),
        (b"\x1b[?1015h", MouseProtocolEncoding::Urxvt),
    ] {
        assert_eq!(parse(seq).screen().mouse_protocol_encoding(), enc);
    }
    let p = parse(b"\x1b[?1015h\x1b[?1015l\x1b[?1000h\x1b[?1000l");
    assert_eq!(
        p.screen().mouse_protocol_encoding(),
        MouseProtocolEncoding::Default
    );
    assert_eq!(p.screen().mouse_protocol_mode(), MouseProtocolMode::None);
}

#[test]
fn decscusr_sets_cursor_style_and_ignores_out_of_range() {
    // ctlseqs "CSI Ps SP q": 0 default, 1-2 block, 3-4 underline, 5-6 bar
    assert_eq!(parse(b"\x1b[5 q").screen().cursor_style(), 5);
    assert_eq!(parse(b"\x1b[5 q\x1b[ q").screen().cursor_style(), 0);
    assert_eq!(parse(b"\x1b[3 q\x1b[9 q").screen().cursor_style(), 3);
}

#[test]
fn alternate_screen_reports_the_active_grid() {
    // ctlseqs ?1049: save cursor, switch to the (cleared) alternate screen. The cursor stays
    // where it was (xterm, xterm.js), so "alt" lands after the four columns "main" used.
    let p = parse(b"main\x1b[?1049halt");
    assert!(p.screen().alternate_screen());
    assert_eq!(p.screen().rows(0, 40).next().unwrap().trim_end(), "    alt");
    let p = parse(b"main\x1b[?1049halt\x1b[?1049l");
    assert!(!p.screen().alternate_screen());
    assert_eq!(p.screen().rows(0, 40).next().unwrap().trim_end(), "main");
}

#[test]
fn decstbm_region_is_reported_one_based_and_zero_when_full() {
    // ctlseqs "CSI Pt ; Pb r"
    assert_eq!(parse(b"").screen().scroll_region(), (0, 0));
    assert_eq!(parse(b"\x1b[3;7r").screen().scroll_region(), (3, 7));
    assert_eq!(parse(b"\x1b[3;7r\x1b[r").screen().scroll_region(), (0, 0));
    // a region that spans the screen is the same as none
    assert_eq!(parse(b"\x1b[1;10r").screen().scroll_region(), (0, 0));
}

#[test]
fn scroll_region_on_the_alternate_screen_is_its_own() {
    let p = parse(b"\x1b[2;4r\x1b[?1049h");
    assert_eq!(p.screen().scroll_region(), (0, 0));
    let p = parse(b"\x1b[2;4r\x1b[?1049h\x1b[?1049l");
    assert_eq!(p.screen().scroll_region(), (2, 4));
}

#[test]
fn tab_stops_default_every_eight_columns_and_follow_hts_tbc() {
    // ctlseqs "ESC H" (HTS), "CSI g" / "CSI 3 g" (TBC)
    assert_eq!(parse(b"").screen().tab_stops(), vec![8, 16, 24, 32]);
    assert_eq!(
        parse(b"\x1b[1;4H\x1bH").screen().tab_stops(),
        vec![3, 8, 16, 24, 32]
    );
    assert_eq!(
        parse(b"\x1b[1;9H\x1b[g").screen().tab_stops(),
        vec![16, 24, 32]
    );
    assert_eq!(parse(b"\x1b[3g").screen().tab_stops(), Vec::<u16>::new());
}

#[test]
fn charsets_report_designations_and_gl() {
    // ctlseqs SCS "ESC ( 0" etc., SO/SI shift G1/G0 into GL
    assert_eq!(parse(b"").screen().charsets(), ([0, 0, 0, 0], 0));
    assert_eq!(parse(b"\x1b(0").screen().charsets(), ([1, 0, 0, 0], 0));
    assert_eq!(parse(b"\x1b)0\x0e").screen().charsets(), ([0, 1, 0, 0], 1));
    assert_eq!(
        parse(b"\x1b*A\x1b+0\x1bn").screen().charsets(),
        ([0, 0, 2, 1], 2)
    );
    assert_eq!(parse(b"\x1b)0\x0e\x0f").screen().charsets().1, 0);
}

#[test]
fn ris_resets_all_reported_state() {
    // ctlseqs "ESC c"
    let p = parse(
        b"\x1b]2;t\x07\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[5 q\x1b[3;7r\x1b[3g\x1b(0\x1b[?25l\x1bc",
    );
    let s = p.screen();
    assert!(!s.application_cursor() && !s.bracketed_paste() && !s.hide_cursor());
    assert_eq!(s.mouse_protocol_mode(), MouseProtocolMode::None);
    assert_eq!(s.cursor_style(), 0);
    assert_eq!(s.scroll_region(), (0, 0));
    assert_eq!(s.tab_stops(), vec![8, 16, 24, 32]);
    assert_eq!(s.charsets(), ([0; 4], 0));
}

// C1 controls as UTF-8 (xterm UTF-8 mode honors U+0080-U+009F as the control)

#[test]
fn c1_as_utf8_acts_as_the_control() {
    // CSI = U+009B: move to row 3 col 5, print x
    let p = parse("\u{9b}3;5Hx".as_bytes());
    assert_eq!(p.screen().cell(2, 4).unwrap().contents(), "x");
    // NEL U+0085 returns to column 0 of the next line
    let p = parse("ab\u{85}c".as_bytes());
    assert_eq!(p.screen().cell(1, 0).unwrap().contents(), "c");
    // IND U+0084 keeps the column
    let p = parse("ab\u{84}c".as_bytes());
    assert_eq!(p.screen().cell(1, 2).unwrap().contents(), "c");
    // OSC U+009D ... ST U+009C
    let p = parse("\u{9d}2;viaC1\u{9c}".as_bytes());
    assert_eq!(p.screen().title(), "viaC1");
    // HTS U+0088 at col 3
    let p = parse("\x1b[1;4H\u{88}".as_bytes());
    assert!(p.screen().tab_stops().contains(&3));
}

#[test]
fn c1_pair_split_across_chunks_still_acts_as_the_control() {
    let mut p = Parser::new(10, 40, 0);
    let bytes = "\u{9b}2;3Hy".as_bytes(); // C2 9B ...
    p.process(&bytes[..1]);
    p.process(&bytes[1..]);
    assert_eq!(p.screen().cell(1, 2).unwrap().contents(), "y");
}

#[test]
fn c2_followed_by_a_real_character_is_left_alone() {
    // U+00A0 NO-BREAK SPACE is C2 A0 and must print, not be treated as a control
    let p = parse("a\u{a0}b".as_bytes());
    assert_eq!(p.screen().cell(0, 1).unwrap().contents(), "\u{a0}");
    assert_eq!(p.screen().cell(0, 2).unwrap().contents(), "b");
    let mut p = Parser::new(10, 40, 0);
    p.process(&[b'a', 0xC2]);
    p.process(&[0xA0, b'b']);
    assert_eq!(p.screen().cell(0, 1).unwrap().contents(), "\u{a0}");
}

#[test]
fn invalid_utf8_prints_replacement_characters() {
    // Unicode 15 ch.3 "U+FFFD substitution of maximal subparts"; xterm.js drops the bytes
    // silently instead, which we deliberately don't copy (a visible marker beats lost output).
    let p = parse(b"[\xff][\xe2\x28]");
    assert_eq!(p.screen().cell(0, 1).unwrap().contents(), "\u{fffd}");
    assert_eq!(p.screen().cell(0, 4).unwrap().contents(), "\u{fffd}");
    let p = parse("[\u{fffd}]".as_bytes());
    assert_eq!(p.screen().cell(0, 1).unwrap().contents(), "\u{fffd}");
}

#[test]
fn cursor_pending_wrap_is_set_only_past_the_last_column() {
    // xterm.js reports x == cols while a wrap is pending; the flag exposes that state
    let p = parse(&[b"\x1b[1;40H".as_slice(), b"x"].concat());
    assert!(p.screen().cursor_pending_wrap());
    assert_eq!(p.screen().cursor_position().1, 40);
    let p = parse(b"\x1b[1;40H");
    assert!(!p.screen().cursor_pending_wrap());
    // any cursor movement clears it
    let p = parse(b"\x1b[1;40Hx\x1b[D");
    assert!(!p.screen().cursor_pending_wrap());
    // DECAWM off parks the cursor the same way
    let p = parse(b"\x1b[?7l\x1b[1;40Hx");
    assert!(p.screen().cursor_pending_wrap());
}
