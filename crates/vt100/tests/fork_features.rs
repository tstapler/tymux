//! Tests for behavior added in the tymux fork. Spec references: ECMA-48 (SGR 8.3.117, REP 8.3.103),
//! xterm ctlseqs (https://invisible-island.net/xterm/ctlseqs/ctlseqs.html), ITU T.416 (colon colors).
use vt100::{Color, Parser, UnderlineStyle};

fn parse(bytes: &[u8]) -> Parser {
    let mut p = Parser::new(5, 40, 0);
    p.process(bytes);
    p
}

fn text(p: &Parser, row: u16) -> String {
    p.screen()
        .rows(0, 40)
        .nth(usize::from(row))
        .unwrap()
        .trim_end()
        .to_string()
}

#[test]
fn sgr_9_and_29_toggle_strikethrough() {
    // ECMA-48 SGR 9 "crossed-out", 29 "not crossed-out"
    let p = parse(b"\x1b[9ma\x1b[29mb");
    assert!(p.screen().cell(0, 0).unwrap().strikethrough());
    assert!(!p.screen().cell(0, 1).unwrap().strikethrough());
}

#[test]
fn sgr_5_6_blink_and_25_clears() {
    // ECMA-48 SGR 5 slow blink, 6 rapid blink, 25 steady
    let p = parse(b"\x1b[5ma\x1b[6mb\x1b[25mc");
    assert!(p.screen().cell(0, 0).unwrap().blink());
    assert!(p.screen().cell(0, 1).unwrap().blink());
    assert!(!p.screen().cell(0, 2).unwrap().blink());
}

#[test]
fn sgr_8_and_28_toggle_conceal() {
    // ECMA-48 SGR 8 concealed, 28 revealed
    let p = parse(b"\x1b[8ma\x1b[28mb");
    assert!(p.screen().cell(0, 0).unwrap().conceal());
    assert!(!p.screen().cell(0, 1).unwrap().conceal());
}

#[test]
fn sgr_53_and_55_toggle_overline() {
    // ECMA-48 SGR 53 overlined, 55 not overlined
    let p = parse(b"\x1b[53ma\x1b[55mb");
    assert!(p.screen().cell(0, 0).unwrap().overline());
    assert!(!p.screen().cell(0, 1).unwrap().overline());
}

#[test]
fn sgr_22_clears_both_bold_and_faint() {
    // ECMA-48 SGR 22 "normal intensity" ends both 1 and 2
    let p = parse(b"\x1b[1;2ma\x1b[22mb");
    let (a, b) = (
        p.screen().cell(0, 0).unwrap(),
        p.screen().cell(0, 1).unwrap(),
    );
    assert!(
        a.bold() && a.dim(),
        "xterm keeps bold and faint independent"
    );
    assert!(!b.bold() && !b.dim());
}

#[test]
fn sgr_1_after_2_keeps_faint() {
    // xterm tracks bold and faint as separate flags
    let p = parse(b"\x1b[2m\x1b[1ma");
    let c = p.screen().cell(0, 0).unwrap();
    assert!(c.bold() && c.dim());
}

#[test]
fn sgr_4_colon_n_selects_underline_style() {
    // xterm ctlseqs "CSI 4 : Ps m": 0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed
    let styles = [
        (1, UnderlineStyle::Single),
        (2, UnderlineStyle::Double),
        (3, UnderlineStyle::Curly),
        (4, UnderlineStyle::Dotted),
        (5, UnderlineStyle::Dashed),
    ];
    for (n, style) in styles {
        let p = parse(format!("\x1b[4:{n}ma").as_bytes());
        let c = p.screen().cell(0, 0).unwrap();
        assert!(c.underline(), "4:{n}");
        assert_eq!(c.underline_style(), style, "4:{n}");
    }
}

#[test]
fn sgr_4_colon_0_and_24_remove_underline() {
    // xterm ctlseqs: 4:0 = no underline; ECMA-48 SGR 24
    for off in ["\x1b[4:0m", "\x1b[24m"] {
        let p = parse(format!("\x1b[4:3ma{off}b").as_bytes());
        assert!(p.screen().cell(0, 0).unwrap().underline());
        assert!(!p.screen().cell(0, 1).unwrap().underline(), "{off:?}");
    }
}

#[test]
fn sgr_21_is_double_underline() {
    // ECMA-48 SGR 21 "doubly underlined"
    let p = parse(b"\x1b[21ma");
    let c = p.screen().cell(0, 0).unwrap();
    assert!(c.underline());
    assert_eq!(c.underline_style(), UnderlineStyle::Double);
}

#[test]
fn sgr_4_colon_out_of_range_style_is_ignored() {
    // malformed subparameter: leave the state unchanged
    let p = parse(b"\x1b[4:9ma");
    assert!(!p.screen().cell(0, 0).unwrap().underline());
}

#[test]
fn sgr_58_sets_and_59_resets_underline_color() {
    // xterm ctlseqs SGR 58 (underline color), 59 (default underline color)
    let p = parse(b"\x1b[4;58;2;1;2;3ma\x1b[58;5;196mb\x1b[59mc");
    assert_eq!(
        p.screen().cell(0, 0).unwrap().ulcolor(),
        Color::Rgb(1, 2, 3)
    );
    assert_eq!(p.screen().cell(0, 1).unwrap().ulcolor(), Color::Idx(196));
    assert_eq!(p.screen().cell(0, 2).unwrap().ulcolor(), Color::Default);
}

#[test]
fn sgr_58_colon_forms_with_and_without_colorspace() {
    // ITU T.416 8.6.1: 58:2:<colorspace>:r:g:b, and the colorspace-less 58:2:r:g:b
    let p = parse(b"\x1b[58:2::10:20:30ma\x1b[58:2:40:50:60mb\x1b[58:5:7mc");
    assert_eq!(
        p.screen().cell(0, 0).unwrap().ulcolor(),
        Color::Rgb(10, 20, 30)
    );
    assert_eq!(
        p.screen().cell(0, 1).unwrap().ulcolor(),
        Color::Rgb(40, 50, 60)
    );
    assert_eq!(p.screen().cell(0, 2).unwrap().ulcolor(), Color::Idx(7));
}

#[test]
fn sgr_38_48_colon_forms() {
    // ITU T.416 colon-separated direct and indexed colors
    let p = parse(b"\x1b[38:2::1:2:3;48:5:200ma\x1b[38:2:4:5:6ma");
    let c = p.screen().cell(0, 0).unwrap();
    assert_eq!(c.fgcolor(), Color::Rgb(1, 2, 3));
    assert_eq!(c.bgcolor(), Color::Idx(200));
    assert_eq!(
        p.screen().cell(0, 1).unwrap().fgcolor(),
        Color::Rgb(4, 5, 6)
    );
}

#[test]
fn sgr_malformed_extended_color_is_ignored_and_stops_parsing() {
    // out-of-range component: no color change, and the rest of the sequence is dropped
    let p = parse(b"\x1b[38:2::300:2:3;1ma");
    let c = p.screen().cell(0, 0).unwrap();
    assert_eq!(c.fgcolor(), Color::Default);
    assert!(!c.bold());
}

#[test]
fn sgr_0_resets_new_attributes() {
    // ECMA-48 SGR 0 returns every rendition to default
    let p = parse(b"\x1b[9;5;8;53;4:3;58:5:1ma\x1b[0mb");
    let (a, b) = (
        p.screen().cell(0, 0).unwrap(),
        p.screen().cell(0, 1).unwrap(),
    );
    assert!(a.strikethrough() && a.blink() && a.conceal() && a.overline() && a.underline());
    assert!(!b.strikethrough() && !b.blink() && !b.conceal() && !b.overline() && !b.underline());
    assert_eq!(b.ulcolor(), Color::Default);
}

#[test]
fn rep_repeats_the_preceding_graphic_character() {
    // ECMA-48 REP / xterm "CSI Ps b"
    let p = parse(b"ab\x1b[3b");
    assert_eq!(text(&p, 0), "abbbb");
}

#[test]
fn rep_with_no_count_repeats_once() {
    let p = parse(b"x\x1b[b");
    assert_eq!(text(&p, 0), "xx");
}

#[test]
fn rep_is_a_no_op_after_a_control_sequence() {
    // REP only applies directly after a graphic character
    let p = parse(b"ab\x1b[1D\x1b[3b");
    assert_eq!(text(&p, 0), "ab");
}

#[test]
fn rep_is_a_no_op_after_a_c0_control() {
    let p = parse(b"ab\r\x1b[3b");
    assert_eq!(text(&p, 0), "ab");
}

#[test]
fn rep_without_a_preceding_character_does_nothing() {
    let p = parse(b"\x1b[5b");
    assert_eq!(text(&p, 0), "");
}

#[test]
fn rep_after_sgr_is_a_no_op() {
    // SGR is a control sequence like any other, so it ends the "preceding graphic character"
    let p = parse(b"a\x1b[1m\x1b[2b");
    assert_eq!(text(&p, 0), "a");
}

#[test]
fn rep_huge_count_is_bounded_by_the_screen() {
    let p = parse(b"a\x1b[65535b");
    assert!(text(&p, 4).len() <= 40);
}

#[test]
fn dec_special_graphics_maps_line_drawing() {
    // xterm ctlseqs: ESC ( 0 selects DEC Special Graphics into G0
    let p = parse(b"\x1b(0lqkxmj\x1b(Bq");
    assert_eq!(text(&p, 0), "┌─┐│└┘q");
}

#[test]
fn dec_special_graphics_full_table() {
    let p = parse(b"\x1b(0`afgnopqrstuvwxyz{|}~");
    assert_eq!(text(&p, 0), "◆▒°±┼⎺⎻─⎼⎽├┤┴┬│≤≥π≠£·");
}

#[test]
fn scs_uk_charset_maps_pound() {
    // ESC ( A selects the UK national set: '#' becomes '£'
    let p = parse(b"\x1b(A#\x1b(B#");
    assert_eq!(text(&p, 0), "£#");
}

#[test]
fn so_si_shift_g1_and_g0_into_gl() {
    // ECMA-48 SO/SI with G1 designated via ESC ) 0
    let p = parse(b"\x1b)0q\x0eq\x0fq");
    assert_eq!(text(&p, 0), "q─q");
}

#[test]
fn single_shift_applies_to_one_character_only() {
    // ESC N = SS2 (next character from G2), ESC O = SS3 (G3)
    let p = parse(b"\x1b*0\x1b+0\x1bNqq\x1bOqq");
    assert_eq!(text(&p, 0), "─q─q");
}

#[test]
fn unknown_scs_final_is_ignored() {
    let p = parse(b"\x1b(Zq");
    assert_eq!(text(&p, 0), "q");
}

#[test]
fn decsc_decrc_save_and_restore_charset() {
    // DECSC saves the charset state along with the cursor
    let p = parse(b"\x1b(0\x1b7\x1b(B\r\nq\x1b8q");
    assert_eq!(text(&p, 0), "─");
    assert_eq!(text(&p, 1), "q");
}

#[test]
fn ris_resets_charsets() {
    let p = parse(b"\x1b(0\x1bcq");
    assert_eq!(text(&p, 0), "q");
}

#[test]
fn osc8_links_cells_until_closed() {
    // xterm ctlseqs / hyperlink spec: OSC 8 ; params ; URI ST
    let p = parse(b"a\x1b]8;;https://example.com\x1b\\bc\x1b]8;;\x1b\\d");
    let s = p.screen();
    assert_eq!(s.cell(0, 0).unwrap().hyperlink(), 0);
    let link = s.cell(0, 1).unwrap().hyperlink();
    assert_ne!(link, 0);
    assert_eq!(s.cell(0, 2).unwrap().hyperlink(), link);
    assert_eq!(s.cell(0, 3).unwrap().hyperlink(), 0);
    assert_eq!(s.hyperlink(link), Some(("https://example.com", "")));
}

#[test]
fn osc8_captures_id_param_and_bel_terminator() {
    let p = parse(b"\x1b]8;id=x1:foo=bar;https://a.test/p\x07z");
    let s = p.screen();
    assert_eq!(
        s.hyperlink(s.cell(0, 0).unwrap().hyperlink()),
        Some(("https://a.test/p", "x1"))
    );
}

#[test]
fn osc8_uri_may_contain_semicolons() {
    let p = parse(b"\x1b]8;;https://a.test/?q=1;2\x1b\\z");
    let s = p.screen();
    assert_eq!(
        s.hyperlink(s.cell(0, 0).unwrap().hyperlink()),
        Some(("https://a.test/?q=1;2", ""))
    );
}

#[test]
fn osc8_same_uri_and_id_dedupes_to_one_entry() {
    let p = parse(b"\x1b]8;;u\x1b\\a\x1b]8;;\x1b\\\x1b]8;;u\x1b\\b");
    let s = p.screen();
    assert_eq!(
        s.cell(0, 0).unwrap().hyperlink(),
        s.cell(0, 1).unwrap().hyperlink()
    );
}

#[test]
fn osc8_survives_sgr_reset() {
    // a hyperlink is not an SGR rendition: SGR 0 must not end it
    let p = parse(b"\x1b]8;;u\x1b\\a\x1b[0mb");
    assert_ne!(p.screen().cell(0, 1).unwrap().hyperlink(), 0);
}

#[test]
fn osc8_unknown_index_resolves_to_none() {
    let p = parse(b"a");
    assert_eq!(p.screen().hyperlink(0), None);
    assert_eq!(p.screen().hyperlink(7), None);
}

#[test]
fn erase_keeps_only_the_background() {
    // xterm "background color erase": erased cells take the current bg but no other rendition
    let p = parse(b"\x1b[2;4;41;9mabc\x1b[1G\x1b[2X");
    let c = p.screen().cell(0, 0).unwrap();
    assert_eq!(c.bgcolor(), Color::Idx(1));
    assert!(!c.dim() && !c.underline() && !c.strikethrough());
}

#[test]
fn contents_formatted_round_trips_new_attributes() {
    let src = b"\x1b[1;2;3;4:3;5;8;9;53;58:2::1:2:3;38;5;99mX\x1b[0m Y";
    let p = parse(src);
    let mut q = Parser::new(5, 40, 0);
    q.process(&p.screen().contents_formatted());
    for col in 0..3 {
        let (a, b) = (
            p.screen().cell(0, col).unwrap(),
            q.screen().cell(0, col).unwrap(),
        );
        assert_eq!(a.bold(), b.bold());
        assert_eq!(a.dim(), b.dim());
        assert_eq!(a.italic(), b.italic());
        assert_eq!(a.underline_style(), b.underline_style());
        assert_eq!(a.blink(), b.blink());
        assert_eq!(a.conceal(), b.conceal());
        assert_eq!(a.strikethrough(), b.strikethrough());
        assert_eq!(a.overline(), b.overline());
        assert_eq!(a.ulcolor(), b.ulcolor());
        assert_eq!(a.fgcolor(), b.fgcolor());
    }
}
