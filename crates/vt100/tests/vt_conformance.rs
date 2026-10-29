//! Tests for the xterm-compatibility fixes in the tymux fork. Spec: xterm ctlseqs
//! (https://invisible-island.net/xterm/ctlseqs/ctlseqs.html) and ECMA-48; where xterm and the
//! browser's terminal (xterm.js) differ, the browser's behavior wins and the test says so.
use vt100::{Color, Parser};

fn parse(bytes: &[u8]) -> Parser {
    let mut p = Parser::new(6, 20, 0);
    p.process(bytes);
    p
}

fn row(p: &Parser, r: u16) -> String {
    p.screen()
        .rows(0, 20)
        .nth(usize::from(r))
        .unwrap()
        .trim_end()
        .to_string()
}

#[test]
fn hts_and_tbc_edit_tab_stops() {
    // ESC H sets a stop at the cursor; CSI 3 g clears them all; CSI 0 g clears the one at the cursor
    let p = parse(b"\x1b[3g\x1b[1;4H\x1bH\x1b[1;1H\tX");
    assert_eq!(row(&p, 0), "   X");
    let p = parse(b"\x1b[1;9H\x1b[0g\x1b[1;1H\tX");
    assert_eq!(row(&p, 0).find('X'), Some(16));
}

#[test]
fn cht_and_cbt_walk_tab_stops() {
    // CSI Ps I = CHT, CSI Ps Z = CBT
    let p = parse(b"\x1b[2IX\x1b[1;20H\x1b[2ZY");
    assert_eq!(row(&p, 0).find('X'), Some(16));
    assert_eq!(row(&p, 0).find('Y'), Some(8));
}

#[test]
fn tab_is_ignored_while_a_wrap_is_pending() {
    // xterm.js: HT at the pending-wrap position does nothing, so the next glyph wraps
    let p = parse(&[&b"a".repeat(20)[..], b"\tb"].concat());
    assert_eq!(row(&p, 1), "b");
}

#[test]
fn ind_nel_and_hpa_vpr_hvp_aliases() {
    // ESC D index, ESC E next line, CSI ` HPA, CSI a HPR, CSI e VPR, CSI f HVP
    let p = parse(b"ab\x1bDc\x1bEd\x1b[2`e\x1b[2af\x1b[1eg\x1b[1;1Hh\x1b[2;3fi");
    assert_eq!(row(&p, 0), "hb");
    assert_eq!(row(&p, 1), "  i");
    assert_eq!(row(&p, 2), "de  f");
    assert_eq!(row(&p, 3), "     g");
}

#[test]
fn irm_inserts_instead_of_overwriting() {
    // ANSI mode 4 (IRM)
    let p = parse(b"abcd\x1b[1;2H\x1b[4hXY\x1b[4lZ");
    assert_eq!(row(&p, 0), "aXYZcd");
}

#[test]
fn lnm_makes_line_feed_return_the_carriage() {
    // ANSI mode 20 (LNM)
    let p = parse(b"ab\n\x1b[20hcd\nef\x1b[20l\ngh");
    assert_eq!(row(&p, 1), "  cd");
    assert_eq!(row(&p, 2), "ef");
    assert_eq!(row(&p, 3), "  gh");
}

#[test]
fn decawm_off_pins_the_cursor_to_the_last_column() {
    // DECSET/DECRST 7
    let p = parse(&[b"\x1b[?7l", &b"a".repeat(20)[..], b"XY"].concat());
    assert_eq!(row(&p, 0), format!("{}Y", "a".repeat(19)));
    assert_eq!(row(&p, 1), "");
    let p = parse(&[b"\x1b[?7l\x1b[?7h", &b"a".repeat(20)[..], b"X"].concat());
    assert_eq!(row(&p, 1), "X");
}

#[test]
fn decawm_off_drops_a_wide_character_that_does_not_fit() {
    let p = parse("\x1b[?7l\x1b[1;20H世x".as_bytes());
    assert_eq!(row(&p, 0), format!("{}x", " ".repeat(19)));
}

#[test]
fn decaln_fills_the_screen_with_e() {
    // ESC # 8
    let p = parse(b"\x1b[31mhi\x1b#8");
    assert_eq!(row(&p, 0), "E".repeat(20));
    assert_eq!(row(&p, 5), "E".repeat(20));
    assert_eq!(p.screen().cell(0, 0).unwrap().fgcolor(), Color::Default);
}

#[test]
fn decstr_soft_reset_keeps_the_screen_but_resets_state() {
    // CSI ! p
    let p = parse(b"\x1b[1;31m\x1b[4h\x1b[?7l\x1b[2;4rab\x1b[!pcd");
    assert_eq!(row(&p, 0), "abcd");
    let c = p.screen().cell(0, 2).unwrap();
    assert_eq!(c.fgcolor(), Color::Default);
    assert!(!c.bold());
}

#[test]
fn ansi_save_restore_cursor_s_u() {
    // CSI s / CSI u
    let p = parse(b"ab\x1b[s\x1b[3;1Hxx\x1b[uZ");
    assert_eq!(row(&p, 0), "abZ");
}

#[test]
fn mode_1048_saves_and_restores_the_cursor_and_1047_clears_the_alternate_screen() {
    let p = parse(b"ab\x1b[?1048h\x1b[3;1Hx\x1b[?1048lZ");
    assert_eq!(row(&p, 0), "abZ");
    let p = parse(b"main\x1b[?1047hALT\x1b[?1047l\x1b[?1047h");
    assert_eq!(row(&p, 0), "");
    let p = parse(b"main\x1b[?1047hALT\x1b[?1047l");
    assert_eq!(row(&p, 0), "main");
}

#[test]
fn decstbm_invalid_region_is_ignored_and_does_not_home_the_cursor() {
    // reversed or one-line regions leave the margins and the cursor alone
    let p = parse(b"\x1b[3;1Hab\x1b[5;2rcd\x1b[4;4rX");
    assert_eq!(row(&p, 2), "abcdX");
    // a valid region homes to the screen's top-left (DECOM off), not the margin
    let p = parse(b"\x1b[3;5rZ");
    assert_eq!(row(&p, 0), "Z");
}

#[test]
fn il_dl_outside_the_scroll_region_do_nothing_and_inside_return_the_cursor_to_column_one() {
    // xterm ctlseqs IL/DL: ignored outside the margins
    let p = parse(b"a\r\nb\r\nc\x1b[2;3r\x1b[1;1H\x1b[Lz");
    assert_eq!(row(&p, 0), "z");
    assert_eq!(row(&p, 1), "b");
    let p = parse(b"a\r\nb\r\nc\x1b[2;3r\x1b[2;1Hxx\x1b[Ly");
    assert_eq!(row(&p, 1), "y");
}

#[test]
fn erase_scroll_and_insert_blank_with_the_current_background() {
    // xterm "background color erase" for ED/EL/ECH and ICH/DCH/IL/DL/SU
    let p = parse(b"abc\x1b[44m\x1b[1;2H\x1b[P");
    assert_eq!(p.screen().cell(0, 19).unwrap().bgcolor(), Color::Idx(4));
    let p = parse(b"\x1b[44m\x1b[S");
    assert_eq!(p.screen().cell(5, 0).unwrap().bgcolor(), Color::Idx(4));
}

#[test]
fn scroll_down_blanks_with_default_attributes_but_reverse_index_erases_with_the_background() {
    // xterm.js (the reference terminal here): SD uses default attributes, RI background-color erase
    let p = parse(b"\x1b[44m\x1b[T");
    assert_eq!(p.screen().cell(0, 0).unwrap().bgcolor(), Color::Default);
    let p = parse(b"\x1b[44m\x1b[1;1H\x1bM");
    assert_eq!(p.screen().cell(0, 0).unwrap().bgcolor(), Color::Idx(4));
}

#[test]
fn cub_and_bs_from_pending_wrap_count_from_the_last_column() {
    // xterm.js clamps the parked cursor to the last column before moving left
    let p = parse(&[&b"a".repeat(20)[..], b"\x1b[1DX"].concat());
    assert_eq!(row(&p, 0), format!("{}X", "a".repeat(18)) + "a");
    let p = parse(&[&b"a".repeat(20)[..], b"\x08X"].concat());
    assert_eq!(row(&p, 0), format!("{}X", "a".repeat(18)) + "a");
}

#[test]
fn rep_consumes_the_preceding_character() {
    // xterm.js: a second REP in a row repeats nothing
    let p = parse(b"x\x1b[2b\x1b[2bY");
    assert_eq!(row(&p, 0), "xxxY");
}

#[test]
fn rep_repeats_a_whole_grapheme_with_its_combining_mark() {
    let p = parse("e\u{301}\x1b[2b".as_bytes());
    assert_eq!(p.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
}

#[test]
fn ls2_ls3_lock_g2_g3_into_gl() {
    // ESC n = LS2, ESC o = LS3
    let p = parse(b"\x1b*0\x1b+0\x1bnq\x1bo\x1b+Bq");
    assert_eq!(row(&p, 0), "─q");
}

#[test]
fn wide_character_second_half_keeps_the_rendition() {
    let p = parse("\x1b[1;31m世".as_bytes());
    let c = p.screen().cell(0, 1).unwrap();
    assert!(c.is_wide_continuation() && c.bold());
}

#[test]
fn soft_hyphen_takes_its_own_cell_and_feff_is_dropped() {
    let p = parse("a\u{ad}b\u{feff}c".as_bytes());
    assert_eq!(row(&p, 0), "a\u{ad}bc");
}
