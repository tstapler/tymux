//! Replays the stapler-squad fidelity corpus (`*.ansi`) through this crate at 24x80 and diffs the
//! screen against golden grids captured from xterm.js (`golden/<area>/<case>.json`).
//!
//! Runs the vendored subset in `tests/fixtures/fidelity/` by default. The full corpus (and the
//! oracle that regenerates the goldens) lives in the stapler-squad repo under
//! `session/tymux/testdata/fidelity`; point `TYMUX_FIDELITY_DIR` at it to run everything. The
//! subset is a copy and may lag the full corpus. `TYMUX_FIDELITY_FILTER=<substring>` limits the
//! cases and `TYMUX_FIDELITY_VERBOSE=1` prints each failing case's first differing cells.
//!
//! A golden may also carry a `state` object (title, modes, cursor, scroll region, tab stops,
//! charsets; see the oracle's `collectState`), compared key by key when present.
//!
//! Golden cell: `[row, col, text, fg, bg, flags, ulStyle, ulColor, link]`, sparse - an omitted cell is
//! a blank `[" ", "default", "default", "", "", "default", ""]`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use vt100::{Color, Parser, UnderlineStyle};

const ROWS: u16 = 24;
const COLS: u16 = 80;

type Cell = (String, String, String, String, String, String, String); // text fg bg flags ulStyle ulColor link

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
/// without an underline - same normalization as the Go-side comparison.
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

fn screen_cells(p: &Parser) -> BTreeMap<(u16, u16), Cell> {
    let s = p.screen();
    let mut out = BTreeMap::new();
    for row in 0..ROWS {
        for col in 0..COLS {
            let c = s.cell(row, col).unwrap();
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
            out.insert(
                (row, col),
                normalize((
                    text,
                    color(c.fgcolor()),
                    color(c.bgcolor()),
                    flags.join(","),
                    ul_style.into(),
                    color(c.ulcolor()),
                    link,
                )),
            );
        }
    }
    out
}

fn golden_cells(path: &Path) -> BTreeMap<(u16, u16), Cell> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        (v["rows"].as_u64(), v["cols"].as_u64()),
        (Some(24), Some(80)),
        "{path:?}"
    );
    let mut out = BTreeMap::new();
    for row in 0..ROWS {
        for col in 0..COLS {
            out.insert((row, col), normalize(blank()));
        }
    }
    for c in v["cells"].as_array().unwrap() {
        let s = |i: usize| c[i].as_str().unwrap().to_string();
        let key = (
            u16::try_from(c[0].as_u64().unwrap()).unwrap(),
            u16::try_from(c[1].as_u64().unwrap()).unwrap(),
        );
        out.insert(key, normalize((s(2), s(3), s(4), s(5), s(6), s(7), s(8))));
    }
    out
}

/// The terminal state a snapshot carries, in the oracle's `state` shape (proto numbering).
fn screen_state(p: &Parser) -> serde_json::Value {
    use serde_json::json;
    let s = p.screen();
    let mouse_mode = match s.mouse_protocol_mode() {
        vt100::MouseProtocolMode::None => 0,
        vt100::MouseProtocolMode::Press => 1,
        vt100::MouseProtocolMode::PressRelease => 2,
        vt100::MouseProtocolMode::ButtonMotion => 3,
        vt100::MouseProtocolMode::AnyMotion => 4,
    };
    let mouse_encoding = match s.mouse_protocol_encoding() {
        vt100::MouseProtocolEncoding::Default => 0,
        vt100::MouseProtocolEncoding::Utf8 => 1,
        vt100::MouseProtocolEncoding::Sgr => 2,
        vt100::MouseProtocolEncoding::Urxvt => 3,
    };
    let (top, bottom) = s.scroll_region();
    let (g, gl) = s.charsets();
    let (row, col) = s.cursor_position();
    json!({
        "title": s.title(),
        "modes": {
            "appCursorKeys": s.application_cursor(),
            "appKeypad": s.application_keypad(),
            "bracketedPaste": s.bracketed_paste(),
            "focusEvents": s.focus_events(),
            "mouseMode": mouse_mode,
            "mouseEncoding": mouse_encoding,
            "reverseVideo": s.reverse_video(),
            "origin": s.origin_mode(),
            "noAutowrap": !s.autowrap(),
            "insert": s.insert_mode(),
            "newline": s.newline_mode(),
        },
        "cursorHidden": s.hide_cursor(),
        "cursorStyle": s.cursor_style(),
        "altScreen": s.alternate_screen(),
        "scrollTop": top,
        "scrollBottom": bottom,
        "tabStops": s.tab_stops(),
        "charsets": {"g0": g[0], "g1": g[1], "g2": g[2], "g3": g[3], "gl": gl},
        "cursor": {"row": row, "col": col},
    })
}

/// Flattens nested objects to `a.b` keys so each differing field reports on its own line.
fn flatten(prefix: &str, v: &serde_json::Value, out: &mut BTreeMap<String, serde_json::Value>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, v) in m {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), v.clone());
        }
    }
}

/// `(case, state keys)` where xterm.js deviates from the spec, so its value can't be the oracle.
const STATE_QUIRKS: &[(&str, &[&str])] = &[
    // xterm.js neither saves nor restores DECOM with DECSC/DECRC (xterm ctlseqs says it does) and
    // restores the active charset object instead of the G0 designation.
    (
        "cursor/save_restore_decsc",
        &["modes.origin", "charsets.g0"],
    ),
    // xterm.js reads LS1R/LS2R/LS3R (which shift GR, irrelevant in UTF-8) as GL shifts and maps the
    // 96-character `ESC - . /` designations onto G1-G3.
    (
        "parser/esc_sequences",
        &["charsets.g2", "charsets.g3", "charsets.gl"],
    ),
];

fn state_diffs(case: &str, want: &serde_json::Value, got: &serde_json::Value) -> Vec<String> {
    let quirks: &[&str] = STATE_QUIRKS
        .iter()
        .find(|(c, _)| *c == case)
        .map_or(&[], |(_, keys)| keys);
    let (mut w, mut g) = (BTreeMap::new(), BTreeMap::new());
    flatten("", want, &mut w);
    flatten("", got, &mut g);
    w.iter()
        .filter(|(k, v)| !quirks.contains(&k.as_str()) && g.get(*k) != Some(*v))
        .map(|(k, v)| {
            format!(
                "  state.{k}: want {v}, got {}",
                g.get(k).unwrap_or(&serde_json::Value::Null)
            )
        })
        .collect()
}

fn cases(dir: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    let mut stack = vec![dir.join("corpus")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "ansi") {
                found.push(p);
            }
        }
    }
    found.sort();
    found
}

/// Cases where xterm.js itself deviates from the spec, so a spec-following emulator can't match.
const XTERM_JS_QUIRKS: &[&str] = &[
    // xterm.js reports no underline for SGR 4 inside an OSC 8 link.
    "osc/hyperlinks_with_styles",
];

#[test]
fn screen_matches_xterm_js_golden_grids() {
    let dir = std::env::var_os("TYMUX_FIDELITY_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fidelity"),
        PathBuf::from,
    );
    let filter = std::env::var("TYMUX_FIDELITY_FILTER").unwrap_or_default();
    let verbose = std::env::var_os("TYMUX_FIDELITY_VERBOSE").is_some();
    let mut failing = vec![];
    let mut total = 0;
    for ansi in cases(&dir) {
        let rel = ansi
            .strip_prefix(dir.join("corpus"))
            .unwrap()
            .with_extension("");
        let name = rel.to_string_lossy().to_string();
        if !name.contains(&filter)
            || (name.starts_with("known_gap/")
                && std::env::var_os("TYMUX_FIDELITY_KNOWN_GAPS").is_none())
            || XTERM_JS_QUIRKS.contains(&name.as_str())
        {
            continue;
        }
        total += 1;
        let mut parser = Parser::new(ROWS, COLS, 0);
        parser.process(&std::fs::read(&ansi).unwrap());
        let got = screen_cells(&parser);
        let want = golden_cells(&dir.join("golden").join(&rel).with_extension("json"));
        let golden_json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join("golden").join(&rel).with_extension("json")).unwrap(),
        )
        .unwrap();
        let mut state_diff = vec![];
        if golden_json.get("state").is_some() {
            state_diff = state_diffs(&name, &golden_json["state"], &screen_state(&parser));
        }
        let diffs: Vec<String> = want
            .iter()
            .filter(|(k, w)| got[k] != **w)
            .map(|(k, w)| {
                format!(
                    "  r{} c{}: want {w:?}\n          got  {:?}",
                    k.0, k.1, got[k]
                )
            })
            .chain(state_diff)
            .collect();
        if !diffs.is_empty() {
            if verbose {
                eprintln!(
                    "FAIL {name}: {} cells differ\n{}",
                    diffs.len(),
                    diffs
                        .iter()
                        .take(400)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            failing.push(format!("{name} ({} cells)", diffs.len()));
        }
    }
    assert!(
        total > 0,
        "no corpus cases found under {dir:?} (filter {filter:?})"
    );
    assert!(
        failing.is_empty(),
        "{}/{total} cases differ from xterm.js:\n  {}",
        failing.len(),
        failing.join("\n  ")
    );
}
