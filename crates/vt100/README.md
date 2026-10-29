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

## Known deviations

Where this fork and xterm differ on purpose or by omission (xterm column is its documented
behavior):

| Area | This fork | xterm |
|---|---|---|
| REP count | clamped to one screenful (rows x cols) per call | repeats the character literally, Ps times |
| RI, SD blank rows | default attributes (what xterm.js does) | background-color erase with the current background |
| Tab stops | kept per screen (primary and alternate each have their own) | one table shared by both screens |
| RI above the top margin | not verified against xterm; may scroll where xterm only moves up | moves the cursor up one row, no scroll |
| ICH on the second half of a wide character | the shifted blank keeps the continuation marker | blanks the split pair |
| Unknown SCS final (`ESC ( K`) | designates ASCII, as xterm.js does | selects the matching NRCS set when it has one |
| Invalid UTF-8 | prints U+FFFD per ill-formed sequence | xterm prints U+FFFD; xterm.js drops the bytes |
| C1 U+008D (RI) as UTF-8 | acts as RI | xterm acts as RI; xterm.js ignores it |
| DECSC/DECRC | saves DECOM and charset designations | same; xterm.js saves neither |
| GR shifts (LS1R/LS2R/LS3R), 96-char sets (`ESC - . /`) | ignored | honored (GR has no effect in UTF-8 mode) |
