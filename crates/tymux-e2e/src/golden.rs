/// Renders a `vt100::Screen` to a normalized plain-text grid: one line per
/// row, trailing whitespace stripped, colors/attributes dropped. Meant for
/// `insta` snapshot regression tests (`tests/golden_snapshot_e2e.rs`) —
/// text-only coverage for layout/content correctness.
///
/// This is attribute-blind and can't catch every possible visual artifact
/// (a specific terminal emulator's own rendering quirks are out of reach
/// without a real display attached) — extend with color/attribute markers
/// if a real regression ever needs that resolution, not preemptively.
pub fn render_screen(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let mut out = String::new();
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..cols {
            let ch = screen.cell(row, col).map(|c| c.contents()).unwrap_or("");
            line.push_str(if ch.is_empty() { " " } else { ch });
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
