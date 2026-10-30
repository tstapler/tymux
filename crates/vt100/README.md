# vt100 (tymux fork)

Fork of [vt100 0.16.2](https://github.com/doy/vt100-rust) (MIT, Jesse Luehrs) used by tymux's
daemon. Upstream's `LICENSE` is kept. Changes, all aimed at matching what xterm.js (the browser
terminal) shows:

- SGR: strikethrough 9/29, blink 5/6/25, conceal 8/28, overline 53/55, double underline 21,
  underline styles `4:0`-`4:5`, underline color 58/59, ITU T.416 colon forms of 38/48/58, and
  bold/faint kept independent (22 clears both).
- REP (`CSI Ps b`), DEC charsets (SCS `( ) * +` with `0`/`B`/`A`, SO/SI, SS2/SS3, LS2/LS3, saved by
  DECSC), OSC 8 hyperlinks (`Cell::hyperlink` + `Screen::hyperlink`).
- Tab stops (HTS/TBC/CHT/CBT), IND/NEL, HPA/HPR/VPR/HVP, IRM, LNM, DECAWM, DECALN, DECSTR, ANSI
  `CSI s`/`CSI u`, modes 1047/1048, IL/DL scroll-region rules, DECSTBM ignoring invalid regions,
  pending-wrap cursor handling, and background-color erase for ED/EL/ECH/ICH/DCH/IL/DL/SU.
- `contents_formatted`/`contents_diff` emit the new SGR attributes but not OSC 8 links.

Limits (`screen.rs`): OSC titles are capped at 1 KiB (truncated on a char boundary), OSC 8 URIs
at 8 KiB and ids at 256 bytes (over-long ones are rejected, which also ends any open link);
control bytes (C0, DEL, C1) are dropped from titles and make a link invalid. Bidi and URI-scheme
validation are the consumer's job. The hyperlink table holds 4096 entries; when full it reclaims
entries no cell (visible, alternate screen, scrollback) or saved attribute set references. An
OSC 8 that reaches vte's 16-parameter limit may have lost the tail of a URI containing `;`, so it
is treated as invalid. Count-driven operations (ICH/DCH/ECH, SU/SD, IL/DL) clamp to the row or
scroll-region size.

Tests: `tests/fork_features.rs`, `tests/vt_conformance.rs`, `tests/terminal_state.rs` and
`tests/review_regressions.rs`. `tests/fidelity_golden.rs` diffs the screen against xterm.js golden
grids. By default it replays the vendored subset in `tests/fixtures/fidelity/` (the `.ansi` files
keep their CRs via `.gitattributes`). The full corpus and the oracle that regenerates the goldens
live in the stapler-squad repo under `session/tymux/testdata/fidelity`; set `TYMUX_FIDELITY_DIR`
to it to run everything, and note the subset is a copy that may lag it. `TYMUX_FIDELITY_FILTER`,
`TYMUX_FIDELITY_VERBOSE=1` and `TYMUX_FIDELITY_KNOWN_GAPS=1` narrow or widen the run.

`tests/fidelity_resize.rs` replays resize scenarios (`tests/fixtures/fidelity-resize/golden/`) and
diffs the grid, scrollback, cursor, scroll region and tab stops against xterm.js after every step.
The cases live in `fixtures/fidelity-resize/generate/cases.mjs`; `bun install && bun generate.mjs`
there rewrites the goldens (xterm.js 6.0.0 headless, Unicode 11). `TYMUX_RESIZE_FILTER`,
`TYMUX_RESIZE_VERBOSE=1` and `TYMUX_RESIZE_DUMP=1` narrow the run and print the grids.

## Known deviations

Where this fork and xterm differ, on purpose or by omission. "xterm.js" is the reference terminal
the fork is tested against (`tests/fidelity_golden.rs`); xterm behavior is from its source
(`charproc.c`, xterm 412) or ctlseqs unless marked unverified.

| Area | This fork | xterm / xterm.js |
|---|---|---|
| REP count | clamped to one screenful (rows x cols) per call | xterm repeats literally, Ps times |
| SD (`CSI T`) blank rows | default attributes, as xterm.js does | xterm.js: default attributes (RI and IL/DL/SU use the current background) |
| Tab stops | kept per screen (primary and alternate each have their own) | xterm: one table shared by both screens (unverified) |
| ICH on the second half of a wide character | the shifted blank keeps the continuation marker | not verified |
| CBT while a wrap is pending | steps back from the last column | xterm.js lands the next character on the next row (an xterm.js quirk, left alone) |
| Unknown SCS final (`ESC ( K`) | designates ASCII, as xterm.js does | xterm: selects the matching NRCS set when it has one (not implemented here) |
| Invalid UTF-8 | prints U+FFFD per ill-formed sequence | xterm prints U+FFFD; xterm.js drops the bytes |
| C1 U+008D (RI) as UTF-8 | acts as RI | xterm acts as RI; xterm.js ignores it |
| DECSC/DECRC | saves DECOM and charset designations | same in xterm; xterm.js saves neither |
| DECSTR (`CSI ! p`) | resets cursor style, cursor visibility, margins, origin, IRM, DECCKM/DECKPAM, charsets, SGR and the saved cursor; retains bracketed paste, focus reporting and mouse modes | xterm (`ReallyReset`, soft branch) retains the same three. xterm.js clears bracketed paste and focus reporting on DECSTR and keeps mouse tracking (oracle: `ESC[?2004h ESC[?1004h ESC[?1000h ESC[!p` gives `bracketedPasteMode` false, `sendFocusMode` false, mouse mode unchanged) |
| OSC titles with 15 or more `;` | truncated: vte keeps 16 OSC parameters | xterm.js keeps the whole title |
| GR shifts (LS1R/LS2R/LS3R), 96-char sets (`ESC - . /`) | ignored | honored by xterm (GR has no effect in UTF-8 mode) |
| Unicode width table | `unicode-width` (newer than Unicode 11) | the web terminal loads xterm.js's Unicode 11 table; the corpus matches today, but emoji added after Unicode 11 would lay out differently |
| Resize | the primary screen reflows soft-wrapped lines (scrollback included) like xterm.js: lines holding the cursor are left alone, cells with only a background colour are not carried across a rewrap, resize resets the scroll region, and a wide glyph pushed to the next row by the edge marks a soft wrap. Tab stops are reapplied from the nearest stop below the new width, which re-adds stops cleared with TBC | same (ported from xterm.js 6.0.0's `Buffer.resize`/`BufferReflow`) |
| Resize, alternate screen | no reflow; a narrowing resize crops for good | xterm.js keeps the cropped cells and shows them again when the screen widens |
| Tab stops past the width | dropped on a narrowing resize | xterm.js keeps them |

Matching xterm.js on purpose: a pending wrap is cleared by ICH/DCH/ECH/DECRC/CSI u/IND/VPA and
every cursor move; alternate-screen switches (`?47`, `?1047`, `?1049`) carry the cursor across
instead of homing it; RI blanks with the current background and never scrolls from above the top
margin; a wide character that wraps blanks the last column with the current attributes; with DECAWM
off the cursor parks at `cols` after a last-column write.

Snapshot-redraw consequence of that DECSTR difference: after an app sends DECSTR, the browser's own
xterm.js has already cleared bracketed paste and focus reporting, but this emulator still reports
them on, so a client redraw built from a snapshot can switch them back on in a terminal that had
turned them off. A consumer that cares can clear them itself when it sees DECSTR in the stream.

The alternate-screen cursor carry (including `?1049h` while already on the alternate screen, which
now keeps the screen and cursor) and DECAWM-off wide-character handling (cursor pulled back to the
last column, no pending wrap) match xterm.js; no divergence survives in either, checked with the
oracle.
