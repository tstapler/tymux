use crossterm::event::KeyCode;

use crate::config::TymuxConfig;

/// Rows the status bar reserves at the bottom of the terminal.
pub const RESERVED_ROWS: u16 = 1;

/// What the status bar is currently displaying — mirrors `main.rs`'s
/// local `InputMode`-equivalent state (armed prefix vs. copy-mode vs.
/// neither), so its own hint line always matches what keys actually do
/// right now (Story 6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    Normal,
    PrefixArmed,
    CopyMode,
}

/// Whether status-bar chrome is enabled for this attach session.
/// `--no-status-bar` disables it entirely — `ux.md`'s accessibility
/// floor: pure passthrough, zero added escape bytes, not just invisible
/// chrome (Story 6.2's `--no-status-bar` task, UX-A11Y-03).
pub struct StatusBarConfig {
    pub enabled: bool,
    pub no_color: bool,
}

impl StatusBarConfig {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            no_color: std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()),
        }
    }
}

/// The pty's effective row count given the real terminal size — one row
/// shorter than the terminal when the status bar is reserving space,
/// unchanged when it's disabled.
pub fn pty_rows(term_rows: u16, cfg: &StatusBarConfig) -> u16 {
    if cfg.enabled {
        term_rows.saturating_sub(RESERVED_ROWS).max(1)
    } else {
        term_rows
    }
}

/// The DECSTBM scroll-region-reservation escape sequence confining the
/// pty's own scrolling to rows `1..=term_rows-1`, leaving the last row for
/// status-bar chrome the child program's own scrolling never touches.
/// Returns nothing (`--no-status-bar`'s "zero added escape bytes"
/// contract) when `cfg` is disabled.
pub fn decstbm_reserve(term_rows: u16, cfg: &StatusBarConfig) -> Vec<u8> {
    if !cfg.enabled {
        return Vec::new();
    }
    format!("\x1b[1;{}r", pty_rows(term_rows, cfg)).into_bytes()
}

fn key_glyph(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

/// Story 6.4: renders the mode-reactive hint line's plain text — the
/// live binding table while the prefix is armed, copy-mode's own key set
/// while in copy-mode, and nothing while in `Normal` (a stale hint from a
/// prior mode must never linger — returning the fixed empty string for
/// `Normal` unconditionally is what guarantees that, rather than trying
/// to diff against a remembered previous frame).
pub fn render_hint_line(mode: DisplayMode, config: &TymuxConfig) -> String {
    match mode {
        DisplayMode::Normal => String::new(),
        DisplayMode::PrefixArmed => {
            let hints: Vec<String> = config
                .bindings
                .iter()
                .filter_map(|b| {
                    b.sequence
                        .get(1)
                        .map(|k| format!("{}:{}", key_glyph(&k.code), b.action.short_label()))
                })
                .collect();
            format!("-- PREFIX -- {}", hints.join("  "))
        }
        DisplayMode::CopyMode => {
            "-- COPY MODE -- h/j/k/l:move  v:select  y:yank  q/Esc:exit".to_string()
        }
    }
}

/// Wraps `text` in a reverse-video escape for visibility, unless
/// `NO_COLOR` is set — text/symbol content is identical either way
/// (UX-A11Y-02: color removal drops color only, never information).
pub fn colorize(text: &str, cfg: &StatusBarConfig) -> String {
    if cfg.no_color || text.is_empty() {
        text.to_string()
    } else {
        format!("\x1b[7m{text}\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_should_send_resize_for_rows_minus_one_when_status_bar_enabled() {
        let cfg = StatusBarConfig {
            enabled: true,
            no_color: false,
        };
        assert_eq!(pty_rows(24, &cfg), 23);
        assert_eq!(decstbm_reserve(24, &cfg), b"\x1b[1;23r".to_vec());
    }

    #[test]
    fn attach_should_send_full_rows_and_skip_decstbm_when_no_status_bar_flag_set() {
        let cfg = StatusBarConfig {
            enabled: false,
            no_color: false,
        };
        assert_eq!(pty_rows(24, &cfg), 24);
        assert!(decstbm_reserve(24, &cfg).is_empty());
    }

    #[test]
    fn status_bar_should_render_full_binding_table_when_prefix_state_armed() {
        let config = TymuxConfig::defaults();
        let line = render_hint_line(DisplayMode::PrefixArmed, &config);
        assert!(line.contains("detach"));
        assert!(line.contains("split-h"));
        assert!(line.contains("split-v"));
    }

    #[test]
    fn status_bar_should_render_copy_mode_key_set_when_input_mode_is_copy_mode() {
        let config = TymuxConfig::defaults();
        let line = render_hint_line(DisplayMode::CopyMode, &config);
        assert!(line.contains("COPY MODE"));
        assert!(!line.contains("detach"));
    }

    #[test]
    fn status_bar_should_not_render_stale_prefix_hints_after_mode_reverts_to_normal() {
        let config = TymuxConfig::defaults();
        let normal = render_hint_line(DisplayMode::Normal, &config);
        assert!(
            normal.is_empty(),
            "Normal mode must never show a lingering hint line"
        );
    }

    #[test]
    fn no_color_env_should_suppress_ansi_while_retaining_liveness_mode_text() {
        let with_color = StatusBarConfig {
            enabled: true,
            no_color: false,
        };
        let no_color = StatusBarConfig {
            enabled: true,
            no_color: true,
        };
        let text = "-- PREFIX -- d:detach";
        assert!(colorize(text, &with_color).contains("\x1b["));
        let plain = colorize(text, &no_color);
        assert!(!plain.contains("\x1b["));
        assert_eq!(
            plain, text,
            "NO_COLOR must drop color only, never the text itself"
        );
    }
}
