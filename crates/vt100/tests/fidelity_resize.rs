//! Replays resize scenarios (`fixtures/fidelity-resize/golden/<case>.json`) through this crate and
//! diffs the grid, scrollback and cursor/scroll-region/tab state against xterm.js after EVERY step.
//!
//! Each golden carries its own steps (`{write}` / `{resize: [rows, cols]}`) and xterm.js's
//! checkpoint after each. Regenerate with `bun generate.mjs` in `fixtures/fidelity-resize/generate`
//! (see its README); cases are authored in `cases.mjs`. xterm.js is the reference because it is the
//! browser terminal: where it deviates from tmux/xterm, we match it and record the deviation in
//! README.md.
//!
//! `TYMUX_RESIZE_FILTER=<substring>` limits the cases; `TYMUX_RESIZE_VERBOSE=1` prints each failing
//! checkpoint's first differences.
//!
//! Cells: a line is an array of cells, trailing blanks trimmed; a cell is its bare text when it has
//! no attributes (`" "` blank, `""` second half of a wide glyph) or the tuple
//! `[text, fg, bg, flags, ulStyle, ulColor, link]`.
use std::path::PathBuf;

use serde_json::Value;
use vt100::{Color, Parser, Screen, UnderlineStyle};

type Cell = (String, String, String, String, String, String, String);

fn blank() -> Cell {
    (
        " ".into(),
        "default".into(),
        "default".into(),
        String::new(),
        String::new(),
        "default".into(),
        String::new(),
    )
}

fn color(c: Color) -> String {
    match c {
        Color::Default => "default".into(),
        Color::Idx(i) => format!("p:{i}"),
        Color::Rgb(r, g, b) => format!("rgb:{r:02x}{g:02x}{b:02x}"),
    }
}

/// Flags that draw nothing on a blank cell are dropped, and underline style/color are meaningless
/// without an underline (same normalization as the corpus golden test).
fn normalize(mut c: Cell) -> Cell {
    let mut flags: Vec<&str> = c.3.split(',').filter(|f| !f.is_empty()).collect();
    if c.0 == " " {
        flags.retain(|f| matches!(*f, "underline" | "reverse" | "strike" | "overline"));
    }
    if !flags.contains(&"underline") {
        c.4.clear();
        c.5 = "default".into();
    }
    flags.sort_unstable();
    c.3 = flags.join(",");
    c
}

fn cell_of(s: &Screen, row: u16, col: u16) -> Cell {
    // A scrollback row narrower than the screen reads as past-the-end; treat it as blank.
    let Some(c) = s.cell(row, col) else {
        return normalize(blank());
    };
    let text = if c.is_wide_continuation() {
        String::new()
    } else if c.has_contents() {
        c.contents().to_string()
    } else {
        " ".into()
    };
    let mut flags = vec![];
    for (on, name) in [
        (c.bold(), "bold"),
        (c.dim(), "faint"),
        (c.italic(), "italic"),
        (c.underline(), "underline"),
        (c.inverse(), "reverse"),
        (c.blink(), "blink"),
        (c.conceal(), "conceal"),
        (c.strikethrough(), "strike"),
        (c.overline(), "overline"),
    ] {
        if on {
            flags.push(name);
        }
    }
    let ul_style = if c.underline() {
        match c.underline_style() {
            UnderlineStyle::Single => "single",
            UnderlineStyle::Double => "double",
            UnderlineStyle::Curly => "curly",
            UnderlineStyle::Dotted => "dotted",
            UnderlineStyle::Dashed => "dashed",
        }
    } else {
        ""
    };
    let link = s
        .hyperlink(c.hyperlink())
        .map(|(uri, id)| format!("{uri}\0{id}"))
        .unwrap_or_default();
    normalize((
        text,
        color(c.fgcolor()),
        color(c.bgcolor()),
        flags.join(","),
        ul_style.into(),
        color(c.ulcolor()),
        link,
    ))
}

/// The cells of the line currently shown at screen row 0 (so scrollback lines are read by setting
/// the scrollback offset first), padded to `cols`.
fn line_of(s: &Screen, row: u16, cols: u16) -> Vec<Cell> {
    (0..cols).map(|c| cell_of(s, row, c)).collect()
}

fn decode_line(v: &Value, cols: usize) -> Vec<Cell> {
    let mut out: Vec<Cell> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|c| match c {
            Value::String(t) => normalize((
                t.clone(),
                "default".into(),
                "default".into(),
                String::new(),
                String::new(),
                "default".into(),
                String::new(),
            )),
            Value::Array(a) => {
                let s = |i: usize| a[i].as_str().unwrap().to_string();
                normalize((s(0), s(1), s(2), s(3), s(4), s(5), s(6)))
            }
            other => panic!("bad cell {other}"),
        })
        .collect();
    out.truncate(cols);
    out.resize(cols, normalize(blank()));
    out
}

/// Column of a `grid rN cM: ...` difference.
fn grid_col(diff: &str) -> Option<u16> {
    let rest = diff.strip_prefix("grid r")?;
    let after_c = rest.split_once(" c")?.1;
    after_c.split(':').next()?.parse().ok()
}

/// A documented difference from xterm.js (see README.md): the case, the checkpoint index and a
/// predicate for the diffs that are masked. Everything else at that checkpoint is still compared.
struct Deviation {
    case: &'static str,
    step: usize,
    masks: fn(&str) -> bool,
}

/// xterm.js keeps alternate-screen cells that a narrowing resize cropped and shows them again when
/// the screen widens; the crop is final here (the case narrows to 60 columns).
fn alt_screen_cropped_cells(diff: &str) -> bool {
    grid_col(diff).is_some_and(|c| c >= 60)
}

/// xterm.js keeps the first half of a wide glyph cut by a narrowing resize (the cell at column 10
/// of the first row); here the lone half is blanked.
fn cropped_wide_glyph(diff: &str) -> bool {
    diff.starts_with("grid r0 c10:")
}

const KNOWN_DEVIATIONS: &[Deviation] = &[
    Deviation {
        case: "alt_screen_does_not_reflow",
        step: 4,
        masks: alt_screen_cropped_cells,
    },
    Deviation {
        case: "wide_glyph_cropped_at_right_edge",
        step: 1,
        masks: cropped_wide_glyph,
    },
    Deviation {
        case: "wide_glyph_cropped_at_right_edge",
        step: 2,
        masks: cropped_wide_glyph,
    },
    Deviation {
        case: "wide_glyph_cropped_at_right_edge",
        step: 3,
        masks: cropped_wide_glyph,
    },
];

/// Number of cases in `fixtures/fidelity-resize/generate/cases.mjs`; a lower count of goldens
/// means some were deleted or never regenerated. Bump it when a case is added.
const EXPECTED_CASES: usize = 43;

/// `TYMUX_RESIZE_GOLDEN_DIR` points the test at another directory of goldens (e.g. a fuzz run);
/// the committed-count check is skipped then.
fn dir() -> PathBuf {
    std::env::var_os("TYMUX_RESIZE_GOLDEN_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fidelity-resize/golden"),
        PathBuf::from,
    )
}

fn diff_lines(label: &str, want: &[Cell], got: &[Cell], out: &mut Vec<String>) {
    for (col, (w, g)) in want.iter().zip(got).enumerate() {
        if w != g {
            out.push(format!("{label} c{col}: want {w:?} got {g:?}"));
        }
    }
}

fn check_checkpoint(parser: &mut Parser, cp: &Value, out: &mut Vec<String>) {
    let rows = cp["rows"].as_u64().unwrap() as u16;
    let cols = cp["cols"].as_u64().unwrap() as u16;
    let s = parser.screen_mut();
    assert_eq!(s.size(), (rows, cols), "emulator size");

    // visible grid
    let want_grid = cp["grid"].as_array().unwrap();
    let alt = cp["altScreen"].as_bool().unwrap();
    if s.alternate_screen() != alt {
        out.push(format!(
            "altScreen: want {alt} got {}",
            s.alternate_screen()
        ));
    }
    for r in 0..rows {
        let want = decode_line(&want_grid[usize::from(r)], usize::from(cols));
        let got = line_of(s, r, cols);
        diff_lines(&format!("grid r{r}"), &want, &got, out);
    }

    // scrollback (only meaningful on the normal screen, which is where xterm keeps it too)
    let want_sb = cp["scrollback"].as_array().unwrap();
    if !alt {
        s.set_scrollback(usize::MAX);
        let have = s.scrollback();
        if have != want_sb.len() {
            out.push(format!(
                "scrollback length: want {} got {have}",
                want_sb.len()
            ));
            if std::env::var_os("TYMUX_RESIZE_DUMP").is_some() {
                let text = |line: &Value| -> String {
                    line.as_array()
                        .unwrap()
                        .iter()
                        .map(|c| match c {
                            Value::String(t) => t.clone(),
                            Value::Array(a) => a[0].as_str().unwrap().to_string(),
                            _ => String::new(),
                        })
                        .collect()
                };
                let want: Vec<String> = want_sb.iter().map(text).collect();
                let mut got = vec![];
                for j in 0..have {
                    s.set_scrollback(have - j);
                    got.push(s.rows(0, cols).next().unwrap_or_default());
                }
                eprintln!("  scrollback want {want:?}\n  scrollback got  {got:?}");
            }
        } else {
            for (j, want_line) in want_sb.iter().enumerate() {
                s.set_scrollback(have - j);
                let want = decode_line(want_line, usize::from(cols));
                let got = line_of(s, 0, cols);
                diff_lines(&format!("scrollback[{j}]"), &want, &got, out);
            }
        }
        s.set_scrollback(0);
    }

    // soft-wrap links of every line (scrollback then grid): xterm.js's `isWrapped`, "this line
    // continues the one above". Line i's flag is `row_wrapped` of line i-1,
    // so line 0's can't be observed and is not compared.
    if !alt {
        let want_wrapped: Vec<bool> = cp["wrapped"]
            .as_str()
            .unwrap()
            .chars()
            .map(|c| c == '1')
            .collect();
        s.set_scrollback(usize::MAX);
        let have = s.scrollback();
        let mut got_wrapped = vec![false];
        for j in 0..have {
            s.set_scrollback(have - j);
            got_wrapped.push(s.row_wrapped(0));
        }
        s.set_scrollback(0);
        for r in 0..rows {
            got_wrapped.push(s.row_wrapped(r));
        }
        got_wrapped.truncate(have + usize::from(rows)); // last entry is "row below the screen"
                                                        // line 0's own flag (a continuation of a line that is gone) can't be read back
        if got_wrapped[1..] != want_wrapped[1..] {
            let show = |v: &[bool]| {
                v.iter()
                    .map(|&b| if b { '1' } else { '0' })
                    .collect::<String>()
            };
            out.push(format!(
                "wrap links: want {} got {}",
                show(&want_wrapped),
                show(&got_wrapped)
            ));
        }
    }

    // cursor and per-buffer state
    let cur = &cp["cursor"];
    let (cr, cc) = s.cursor_position();
    let want = (
        cur["row"].as_u64().unwrap() as u16,
        cur["col"].as_u64().unwrap() as u16,
        cur["pendingWrap"].as_bool().unwrap(),
    );
    let got = (cr, cc.min(cols - 1), s.cursor_pending_wrap());
    if want != got {
        out.push(format!(
            "cursor (row, col, pendingWrap): want {want:?} got {got:?}"
        ));
    }
    let want_region = (
        cp["scrollTop"].as_u64().unwrap() as u16,
        cp["scrollBottom"].as_u64().unwrap() as u16,
    );
    if s.scroll_region() != want_region {
        out.push(format!(
            "scroll region: want {want_region:?} got {:?}",
            s.scroll_region()
        ));
    }
    // xterm.js keeps stops for columns past the current width; those are unobservable here
    let want_tabs: Vec<u16> = cp["tabStops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_u64().unwrap() as u16)
        .filter(|&c| c < cols)
        .collect();
    let mut got_tabs = s.tab_stops();
    got_tabs.retain(|&c| c != 0);
    if got_tabs != want_tabs {
        out.push(format!("tab stops: want {want_tabs:?} got {got_tabs:?}"));
    }
}

#[test]
fn resize_scenarios_match_xterm_js_after_every_step() {
    let filter = std::env::var("TYMUX_RESIZE_FILTER").unwrap_or_default();
    let verbose = std::env::var_os("TYMUX_RESIZE_VERBOSE").is_some();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    assert!(
        std::env::var_os("TYMUX_RESIZE_GOLDEN_DIR").is_some() || paths.len() >= EXPECTED_CASES,
        "{} resize goldens in {:?}, expected {EXPECTED_CASES} (see cases.mjs)",
        paths.len(),
        dir()
    );

    let mut failing = vec![];
    let mut total = 0;
    for path in paths {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if !name.contains(&filter) {
            continue;
        }
        total += 1;
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut parser = Parser::new(
            v["rows"].as_u64().unwrap() as u16,
            v["cols"].as_u64().unwrap() as u16,
            v["scrollback"].as_u64().unwrap() as usize,
        );
        let mut case_diffs = 0;
        let mut bad_steps = vec![];
        for (i, step) in v["steps"].as_array().unwrap().iter().enumerate() {
            if let Some(text) = step.get("write") {
                parser.process(text.as_str().unwrap().as_bytes());
            } else {
                let sz = step["resize"].as_array().unwrap();
                parser.screen_mut().set_size(
                    sz[0].as_u64().unwrap() as u16,
                    sz[1].as_u64().unwrap() as u16,
                );
            }
            let mut diffs = vec![];
            check_checkpoint(&mut parser, &v["checkpoints"][i], &mut diffs);
            for dev in KNOWN_DEVIATIONS
                .iter()
                .filter(|d| d.case == name && d.step == i)
            {
                diffs.retain(|d| !(dev.masks)(d));
            }
            if !diffs.is_empty() {
                case_diffs += diffs.len();
                bad_steps.push(i);
                if verbose {
                    eprintln!("FAIL {name} step {i} ({step}): {} diffs", diffs.len());
                    for d in diffs.iter().take(12) {
                        eprintln!("  {d}");
                    }
                    if std::env::var_os("TYMUX_RESIZE_DUMP").is_some() {
                        let cols = parser.screen().size().1;
                        {
                            let s = parser.screen_mut();
                            s.set_scrollback(usize::MAX);
                            let have = s.scrollback();
                            let mut got = vec![];
                            for j in 0..have {
                                s.set_scrollback(have - j);
                                got.push(s.rows(0, cols).next().unwrap_or_default());
                            }
                            s.set_scrollback(0);
                            eprintln!("  got  scrollback {got:?}");
                        }
                        for (r, line) in parser.screen().rows(0, cols).enumerate() {
                            eprintln!(
                                "  got  r{r:02} {line:?} wrapped={}",
                                parser.screen().row_wrapped(r as u16)
                            );
                        }
                        for (r, line) in v["checkpoints"][i]["grid"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .enumerate()
                        {
                            let txt: String = line
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|c| match c {
                                    Value::String(t) => t.clone(),
                                    Value::Array(a) => a[0].as_str().unwrap().to_string(),
                                    _ => String::new(),
                                })
                                .collect();
                            eprintln!("  want r{r:02} {txt:?}");
                        }
                    }
                }
            }
        }
        if case_diffs > 0 {
            failing.push(format!("{name}: {case_diffs} diffs, steps {bad_steps:?}"));
        }
    }
    assert!(total > 0, "filter matched no case");
    assert!(
        failing.is_empty(),
        "{}/{total} cases differ from xterm.js:\n  {}",
        failing.len(),
        failing.join("\n  ")
    );
}
