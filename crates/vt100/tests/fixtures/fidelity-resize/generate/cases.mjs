// Resize scenarios for the vt100 fork, replayed into xterm.js (the reference) by generate.mjs.
// A step is {write: "<text>"} or {resize: [rows, cols]}; \r\n line endings, since a bare \n only
// moves down. Keep every case deterministic and small: the goldens are committed.
const E = "\x1b";
const nl = "\r\n";
const lines = (n, f) => Array.from({ length: n }, (_, i) => f(i + 1)).join(nl);
const numbered = (n) => lines(n, (i) => String(i));
const ruler = (n) => Array.from({ length: n }, (_, i) => String.fromCharCode(97 + (i % 26))).join("");
const w = (write) => ({ write });
const r = (rows, cols) => ({ resize: [rows, cols] });

export const cases = [
  {
    name: "shrink_then_grow_plain",
    note: "audit resize-plain-20: rows shrink pushes lines into scrollback, growing pulls them back",
    steps: [w(numbered(20) + nl + `${E}[31mred${E}[0m tail`), r(24, 100), r(30, 100), r(24, 80), r(12, 80), r(24, 80), r(24, 60), r(24, 80)],
  },
  {
    name: "shrink_blank_below_cursor",
    note: "few lines, shrink: blank rows below the cursor are dropped before anything enters scrollback",
    steps: [w(numbered(5)), r(10, 80), r(6, 80), r(5, 80), r(3, 80), r(24, 80)],
  },
  {
    name: "grow_pulls_scrollback_only_without_blank_below",
    note: "growing rows pulls history back only when no blank rows sit below the cursor",
    steps: [w(numbered(40)), r(30, 80), r(40, 80), r(10, 80), r(24, 80)],
  },
  {
    name: "wrapped_long_lines",
    note: "audit resize-wrapped-lines: soft-wrapped logical lines re-wrap at each width",
    cols: 120,
    steps: [w(`${"1".padStart(100, "0")}${nl}${"2".padStart(200, "0")}${nl}short${nl}`), r(24, 80), r(24, 120), r(24, 40), r(24, 120), r(24, 79), r(24, 101)],
  },
  {
    name: "wrapped_lines_in_scrollback",
    note: "reflow also applies to lines that already scrolled off the top",
    steps: [w(lines(60, (i) => `${String(i).padStart(2, "0")} ${ruler(150)}`)), r(24, 100), r(24, 50), r(24, 80), r(24, 200), r(24, 80)],
  },
  {
    name: "scrolled_content",
    note: "audit resize-plain-scrolled",
    steps: [w(lines(80, (i) => `${i} some text to make it long enough`)), r(30, 100), r(24, 80), r(12, 80), r(24, 80)],
  },
  {
    name: "wide_glyph_wrap_boundary",
    note: "a wide glyph never splits across rows; odd widths push it to the next row",
    cols: 40,
    steps: [w("世界你好".repeat(25) + nl + "ab" + "世".repeat(40) + nl), r(24, 39), r(24, 41), r(24, 11), r(24, 40), r(24, 10), r(24, 40)],
  },
  {
    name: "emoji_and_combining_wrap",
    note: "combining marks and emoji stay with their base cell through reflow",
    cols: 30,
    steps: [w(("é😀 ä x").repeat(12) + nl), r(24, 17), r(24, 30), r(24, 12), r(24, 30)],
  },
  {
    name: "cursor_mid_wrapped_line",
    note: "the cursor's own wrapped line is left alone on shrink (reflowCursorLine=false)",
    steps: [w(ruler(200)), `${E}[30D`, r(24, 60), r(24, 80), r(24, 40), w("X"), r(24, 80)].map((s) => (typeof s === "string" ? w(s) : s)),
  },
  {
    name: "cursor_at_end_of_wrapped_line",
    note: "keep typing after a resize: the continuation of a wrapped line still wraps correctly",
    steps: [w(ruler(130)), r(24, 100), w("MORE"), r(24, 50), w("AGAIN" + nl + "next"), r(24, 80)],
  },
  {
    name: "cursor_below_wrapped_block",
    note: "wrapped lines above the cursor reflow while the cursor line is untouched",
    steps: [w(ruler(190) + nl + ruler(170) + nl + "prompt> "), r(24, 60), r(24, 120), r(24, 30), r(24, 80)],
  },
  {
    name: "alt_screen_does_not_reflow",
    note: "the alternate screen has no scrollback and never reflows",
    steps: [w("main screen " + ruler(150) + nl), w(`${E}[?1049h${E}[H${ruler(150)}${nl}${ruler(90)}`), r(24, 60), r(10, 60), r(24, 100), w(`${E}[?1049l`), r(24, 80)],
  },
  {
    name: "scroll_region_reset_on_resize",
    note: "DECSTBM is reset by a resize",
    steps: [w(numbered(15) + `${E}[5;10r${E}[10;1H`), r(30, 80), w(`${nl}${nl}after`), r(12, 80), w(`${E}[3;6r${E}[6;1H${nl}${nl}more`), r(24, 80)],
  },
  {
    name: "min_size_roundtrip",
    note: "10x2 is the daemon's minimum pane",
    steps: [w(numbered(30) + nl + ruler(70)), r(2, 10), r(24, 80), r(2, 10), r(2, 80), r(24, 10), r(24, 80)],
  },
  {
    name: "rapid_resizes",
    note: "dragging a window edge delivers many sizes in a row",
    steps: [w(lines(50, (i) => `${i}: ${ruler(60 + (i % 40))}`)), r(24, 79), r(23, 78), r(22, 77), r(24, 90), r(26, 100), r(28, 70), r(20, 55), r(24, 80), r(30, 120), r(24, 80), r(12, 40), r(24, 80)],
  },
  {
    name: "hyperlink_across_wrap",
    note: "an OSC 8 link spanning a soft wrap keeps its cells through reflow",
    steps: [w(`${E}]8;;https://example.com/a${E}\\${ruler(150)}${E}]8;;${E}\\ plain${nl}`), r(24, 60), r(24, 100), r(24, 20), r(24, 80)],
  },
  {
    name: "sgr_across_wrap",
    note: "colors, background and underline follow their cells",
    steps: [w(`${E}[31;44m${ruler(70)}${E}[0m${E}[1;4m${ruler(70)}${E}[0m${E}[38;2;1;2;3m${ruler(70)}${E}[0m${nl}`), r(24, 50), r(24, 90), r(24, 30), r(24, 80)],
  },
  {
    name: "tab_stops_after_resize",
    note: "columns added by a resize get the default every-8th stop; cleared stops stay cleared",
    steps: [w(`${E}[3g${E}[1;5H${E}H${E}[1;21H${E}H${E}[1;1H`), r(24, 100), w("\ta\tb"), r(24, 30), r(24, 80), w(`${nl}\tc\td`)],
  },
  {
    name: "saved_cursor_across_reflow",
    note: "DECSC's saved row follows the reflow",
    steps: [w(numbered(10) + nl + ruler(150) + nl + `${E}7after save`), r(24, 60), w(`${E}8restored`), r(24, 80), r(8, 80), w(`${E}8again`)],
  },
  {
    name: "ink_style_redraw",
    note: "Ink/Claude Code redraws a block with cursor-up, erases and rewrites; resizes happen between",
    steps: [
      w(lines(8, (i) => `old output line ${i}`) + nl),
      w(lines(6, (i) => `${E}[2K  ${i}. ${ruler(20 + i * 12)}`) + `${E}[5A`),
      w(`${E}[1G${E}[J${lines(6, (i) => `  ${i}. ${ruler(25 + i * 10)}`)}`),
      r(24, 60),
      w(`${E}[5A${E}[1G${E}[J${lines(6, (i) => `  ${i}. redrawn ${ruler(30 + i * 8)}`)}`),
      r(24, 90),
      w(`${E}[5A${E}[1G${E}[J${lines(6, (i) => `  ${i}. final ${ruler(10 * i)}`)}`),
      r(24, 80),
    ],
  },
  {
    name: "scrollback_zero",
    note: "scrollback 0: shrinking drops the top rows instead of keeping them",
    scrollback: 0,
    steps: [w(numbered(30)), r(12, 80), r(24, 80), r(24, 40), r(24, 80)],
  },
  {
    name: "scrollback_small_limit",
    note: "scrollback limit 10: history trimmed from the top while reflowing",
    scrollback: 10,
    steps: [w(lines(50, (i) => `${i} ${ruler(100)}`)), r(24, 40), r(24, 80), r(12, 80), r(24, 120)],
  },
];
