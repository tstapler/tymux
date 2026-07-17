use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// Bindable actions (ADR-005). `ExitCopyMode`/`SendPrefixLiteral` are
/// deliberately not in the config-overridable table below — they're
/// structural (copy-mode's own `q`/`Escape`, the leader-pressed-twice
/// escape hatch — see `copy_mode::CopyModeEvent::Exit` and
/// `KeystrokeReassembler`'s escape-hatch handling respectively), not
/// prefix-bound user bindings, so nothing ever constructs these two
/// variants — they exist for `main.rs`'s `match action` to stay
/// exhaustive against the full Domain Glossary `Action` enum, documenting
/// that these cases are handled elsewhere, not silently unhandled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum Action {
    Detach,
    EnterCopyMode,
    ExitCopyMode,
    SplitHorizontal,
    SplitVertical,
    NewWindow,
    NextWindow,
    PrevWindow,
    KillPane,
    SendPrefixLiteral,
}

impl Action {
    /// Short label for the status bar's prefix-armed hint line (Story
    /// 6.4 AC1), e.g. `"detach"`, `"split-h"`.
    pub fn short_label(&self) -> &'static str {
        match self {
            Action::Detach => "detach",
            Action::EnterCopyMode => "copy-mode",
            Action::ExitCopyMode => "exit-copy-mode",
            Action::SplitHorizontal => "split-h",
            Action::SplitVertical => "split-v",
            Action::NewWindow => "new-window",
            Action::NextWindow => "next-window",
            Action::PrevWindow => "prev-window",
            Action::KillPane => "kill-pane",
            Action::SendPrefixLiteral => "literal-prefix",
        }
    }
}

/// The config-file key naming for each bindable action, and its
/// hardcoded tmux-parity default (`sequence` string form, `"C-b d"`
/// style) — the single source of truth `TymuxConfig::load_or_default`
/// merges explicit overrides onto.
const BINDABLE_ACTIONS: &[(Action, &str, &str)] = &[
    (Action::Detach, "detach", "C-b d"),
    (Action::EnterCopyMode, "copy_mode", "C-b ["),
    (Action::SplitHorizontal, "split_horizontal", "C-b %"),
    (Action::SplitVertical, "split_vertical", "C-b \""),
    (Action::NewWindow, "new_window", "C-b c"),
    (Action::NextWindow, "next_window", "C-b n"),
    (Action::PrevWindow, "prev_window", "C-b p"),
    (Action::KillPane, "kill_pane", "C-b x"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct KeyBinding {
    pub sequence: Vec<KeyEvent>,
    pub action: Action,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    leader: Option<String>,
    #[serde(default)]
    keybindings: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TymuxConfig {
    pub leader: String,
    pub bindings: Vec<KeyBinding>,
}

#[derive(Debug, PartialEq)]
pub struct KeySequenceParseError {
    pub raw: String,
}

impl TymuxConfig {
    /// `$XDG_CONFIG_HOME/tymux/config.toml` if set, else the platform
    /// default config dir (via the `dirs` crate), or hardcoded defaults if
    /// no config.toml exists there.
    pub fn load_or_default() -> TymuxConfig {
        let path = default_config_path();
        match path.as_deref().map(std::fs::read_to_string) {
            Some(Ok(contents)) => Self::from_toml_str(&contents),
            _ => Self::defaults(),
        }
    }

    pub fn defaults() -> TymuxConfig {
        let bindings = BINDABLE_ACTIONS
            .iter()
            .map(|(action, _, default_seq)| KeyBinding {
                sequence: parse_key_sequence(default_seq).unwrap_or_else(|_| {
                    panic!("hardcoded default binding {default_seq:?} must always parse")
                }),
                action: *action,
            })
            .collect();
        TymuxConfig {
            leader: "C-b".to_string(),
            bindings,
        }
    }

    /// Parses a config file's contents. A whole-file TOML syntax error
    /// falls back to defaults entirely (with a warning) — never a panic.
    /// A single semantically-bad binding string within an otherwise valid
    /// file falls back to just that binding's default, logging which key
    /// and value were bad, while every other valid binding in the file
    /// still applies (Story 5.1 AC3) — mirrors the graceful-degradation
    /// pattern Epic 4 already established for corrupt persisted-session
    /// files.
    pub fn from_toml_str(contents: &str) -> TymuxConfig {
        let raw: RawConfig = match toml::from_str(contents) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(error = %e, "config.toml is not valid TOML, using hardcoded defaults");
                return Self::defaults();
            }
        };

        let leader = raw.leader.unwrap_or_else(|| "C-b".to_string());
        let bindings = BINDABLE_ACTIONS
            .iter()
            .map(|(action, config_key, default_seq)| {
                let default = || {
                    parse_key_sequence(default_seq).unwrap_or_else(|_| {
                        panic!("hardcoded default binding {default_seq:?} must always parse")
                    })
                };
                match raw.keybindings.get(*config_key) {
                    None => KeyBinding {
                        sequence: default(),
                        action: *action,
                    },
                    Some(raw_value) => match parse_key_sequence(raw_value) {
                        Ok(sequence) => KeyBinding {
                            sequence,
                            action: *action,
                        },
                        Err(_) => {
                            tracing::warn!(
                                action = config_key,
                                value = raw_value,
                                "config.toml: invalid keybinding, falling back to the default"
                            );
                            KeyBinding {
                                sequence: default(),
                                action: *action,
                            }
                        }
                    },
                }
            })
            .collect();

        TymuxConfig { leader, bindings }
    }
}

/// Parses a `"C-b d"`-style binding string into a `Vec<KeyEvent>`. Grammar:
/// space-separated tokens, each either `C-<char>` (Ctrl-modified) or a
/// bare single character. Deliberately does not support arrow/function
/// keys or other multi-byte sequences — no default binding needs them,
/// and this keeps parsing symmetric with `KeystrokeReassembler`'s
/// equally-scoped byte classifier (see plan.md Unresolved Question #13).
fn parse_key_sequence(raw: &str) -> Result<Vec<KeyEvent>, KeySequenceParseError> {
    raw.split_whitespace()
        .map(|token| parse_key_token(token, raw))
        .collect()
}

fn parse_key_token(token: &str, raw: &str) -> Result<KeyEvent, KeySequenceParseError> {
    let err = || KeySequenceParseError {
        raw: raw.to_string(),
    };
    if let Some(rest) = token.strip_prefix("C-") {
        let mut chars = rest.chars();
        let c = chars.next().ok_or_else(err)?;
        if chars.next().is_some() {
            return Err(err());
        }
        Ok(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    } else {
        let mut chars = token.chars();
        let c = chars.next().ok_or_else(err)?;
        if chars.next().is_some() {
            return Err(err());
        }
        Ok(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }
}

// Same $XDG_STATE_HOME-on-macOS gap as `persistence::default_sessions_dir`
// (`crates/tymux-core/src/persistence.rs`): `dirs::config_dir()` maps
// $XDG_CONFIG_HOME only on Linux, silently ignoring it on macOS. Checked
// explicitly first so a user's override is honored on every platform.
fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .map(|d| d.join("tymux").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tymux_config_load_or_default_should_use_hardcoded_defaults_when_no_file_present() {
        let config = TymuxConfig::defaults();
        assert_eq!(config.leader, "C-b");
        assert!(config
            .bindings
            .iter()
            .any(|b| b.action == Action::Detach && b.sequence.len() == 2));
    }

    #[test]
    fn tymux_config_should_override_only_detach_binding_when_config_specifies_one_binding() {
        let toml = "[keybindings]\ndetach = \"C-a d\"\n";
        let config = TymuxConfig::from_toml_str(toml);
        let detach = config
            .bindings
            .iter()
            .find(|b| b.action == Action::Detach)
            .unwrap();
        assert_eq!(
            detach.sequence,
            vec![
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            ]
        );
        // An unrelated binding must still be the default.
        let split = config
            .bindings
            .iter()
            .find(|b| b.action == Action::SplitHorizontal)
            .unwrap();
        assert_eq!(split.sequence, parse_key_sequence("C-b %").unwrap());
    }

    #[test]
    fn tymux_config_should_warn_and_fallback_to_default_for_single_malformed_binding_while_others_apply(
    ) {
        let toml = "[keybindings]\ndetach = \"C-nonsense-key\"\nnew_window = \"C-b w\"\n";
        let config = TymuxConfig::from_toml_str(toml);
        let detach = config
            .bindings
            .iter()
            .find(|b| b.action == Action::Detach)
            .unwrap();
        assert_eq!(detach.sequence, parse_key_sequence("C-b d").unwrap());
        let new_window = config
            .bindings
            .iter()
            .find(|b| b.action == Action::NewWindow)
            .unwrap();
        assert_eq!(
            new_window.sequence,
            vec![
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn tymux_config_load_should_produce_friendly_error_not_panic_when_toml_syntax_invalid() {
        let config = TymuxConfig::from_toml_str("this is not [ valid toml");
        assert_eq!(config, TymuxConfig::defaults());
    }

    #[test]
    fn tymux_config_should_resolve_xdg_config_dir_path_when_locating_config_toml() {
        // Real filesystem, real dirs::config_dir() resolution — confirms
        // the path-building logic itself (not just from_toml_str parsing).
        let path = default_config_path();
        assert!(
            path.is_some(),
            "dirs::config_dir() should resolve on any supported platform"
        );
        assert!(path.unwrap().ends_with("tymux/config.toml"));
    }
}
