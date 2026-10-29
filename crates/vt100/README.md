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

`tests/fork_features.rs` and `tests/vt_conformance.rs` cover the additions. `tests/fidelity_golden.rs`
diffs the screen against xterm.js golden grids kept in the stapler-squad repo; it skips unless
`TYMUX_FIDELITY_DIR` points at that repo's `session/tymux/testdata/fidelity`
(`TYMUX_FIDELITY_FILTER=<substring>`, `TYMUX_FIDELITY_VERBOSE=1` to see differing cells).
Known remaining differences: the Unicode width table (xterm.js defaults to Unicode 6), C1 controls
sent as UTF-8, and xterm.js dropping SGR 4 inside an OSC 8 link.
