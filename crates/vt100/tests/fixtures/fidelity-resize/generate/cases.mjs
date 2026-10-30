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

  // --- soft-wrap link bookkeeping: which edits break or keep the link between a row and the one
  // above it. Each op runs on a 12-column screen holding one wrapped line, then the screen widens.
  ...[
    ["el2", `${E}[2;1H${E}[2K`],
    ["el0_at_col0", `${E}[2;1H${E}[K`],
    ["ed0_at_col0", `${E}[2;1H${E}[J`],
    ["il", `${E}[2;1H${E}[L`],
    ["dl", `${E}[1;1H${E}[M`],
    ["lf_into_continuation", `${E}[1;1H\n`],
    ["rewrite_then_crlf", `${E}[1;1Hab${nl}`],
    ["ich_last_col", `${E}[1;12H${E}[2@`],
    ["dch_last_col", `${E}[1;12H${E}[P`],
    ["ech_last_col", `${E}[1;12H${E}[X`],
    ["insert_mode_on_continuing_row", `${E}[4h${E}[1;1Hab${E}[4l`],
  ].map(([op, seq]) => ({
    name: `wrap_link_${op}`,
    note: "xterm.js keeps or breaks the soft-wrap link per edit; widening must agree",
    rows: 6,
    cols: 12,
    steps: [w("x hellox hellox hello"), w(`${seq}${E}[4;1H`), r(6, 20), r(6, 12)],
  })),
  {
    name: "wrap_link_in_scroll_region",
    note: "a wrapped line written inside a DECSTBM region, region reset, then widen",
    rows: 6,
    cols: 12,
    steps: [w(`${E}[2;5r${E}[2;1Hx hellox hellox hello${E}[r${E}[6;1H`), r(6, 20), r(6, 12)],
  },
  {
    name: "wrap_link_blank_continuation_after_lfs",
    note: "50 chars widened, then four LFs and a char on the old continuation rows, then shrink",
    rows: 5,
    cols: 10,
    steps: [w(ruler(50)), r(5, 20), w(`${E}[3;1H\n\n\n\nq`), r(5, 10), r(5, 20)],
  },
  {
    name: "wide_glyph_cropped_at_right_edge",
    note: "xterm.js keeps a wide glyph's cell when a shrink crops it and shows it again on grow",
    cols: 26,
    steps: [w("日本語日本語0123456789😀😀abc".repeat(3)), r(9, 11), r(4, 21), r(24, 26)],
  },
  {
    name: "decrc_without_save_after_resize",
    note: "restoring a cursor that was never saved goes to the origin, even after rows were pulled in",
    rows: 3,
    cols: 10,
    steps: [w("abcdefghij".repeat(5)), r(11, 10), w(`${E}8`), w(`${E}[?1049h${E}[?1049l`)],
  },
  {
    name: "same_size_resize_is_a_noop",
    note: "a resize to the current size keeps the scroll region and a pending wrap (xterm.js returns early)",
    rows: 6,
    cols: 12,
    steps: [w(`${E}[2;5r${E}[3;1H${"x".repeat(12)}`), r(6, 12), w("yz"), r(6, 12), r(6, 12)],
  },
  {
    name: "il_dl_outside_region_cancel_pending_wrap",
    note: "IL/DL restrict the cursor before checking the scroll region, so even a no-op cancels a pending wrap",
    rows: 8,
    cols: 14,
    steps: [w(`${E}[4;8r${E}[2;1H${"x".repeat(14)}`), w(`${E}[L`), w(`${E}[2;1H${"y".repeat(14)}`), w(`${E}[M`)],
  },
  {
    name: "ed1_in_last_column_clears_absolute_line_wrap",
    note: "xterm.js ED 1 at the last column clears the wrap link of absolute line row+1 (ignoring scrollback); matched on purpose",
    rows: 3,
    cols: 3,
    steps: [w(`abcdefghijklmnopqrstuvw${E}[2B${E}[1J${E}[1L${E}[0J`), r(3, 11), r(3, 3)],
  },
  {
    name: "reflow_pop_keeps_scanning_stale_lines",
    note: "xterm.js's pop of blank rows below the cursor leaves the popped lines readable, and the reflow scan still rewrites them (fuzz-minimized)",
    rows: 7,
    cols: 10,
    steps: [
      w(`abcdefghijklmnoabcdefghijklmnopqr${E}[0J${E}[2L`),
      w(`${nl}abcdefg${E}[3P`),
      w(`${nl}${E}[3Mabcdefghijklmnopqrstuvwxyz0123456${nl}${E}[2M`),
      r(9, 18),
      w(`${E}[4;8H${E}[3Labc`),
      r(10, 5),
    ],
  },
  {
    name: "region_from_top_feeds_scrollback",
    note: "a scroll region that starts at row 1 but ends above the last row still pushes scrolled-off rows into scrollback",
    rows: 6,
    cols: 10,
    steps: [w(`${E}[1;3r${lines(8, (i) => `line${i}`)}`), r(6, 20), r(4, 20)],
  },
  {
    name: "lf_cancels_pending_wrap",
    note: "a bare LF at the right edge cancels the pending wrap",
    rows: 4,
    cols: 6,
    steps: [w("abcdef"), w("\n"), w("g"), w(`${E}[1;1Habcdef${E}[B`), w("h")],
  },
];
