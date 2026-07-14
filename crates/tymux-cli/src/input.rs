use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Action, KeyBinding};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// What the reassembler did with one input byte.
#[derive(Debug, PartialEq)]
pub enum ReassembledOutput {
    /// Bytes that should be forwarded to the pty verbatim — unmatched
    /// input, bracketed-paste content, or the literal leader byte on the
    /// leader-pressed-twice escape hatch.
    Forward(Vec<u8>),
    /// A bound action fired. Nothing is forwarded for the bytes that
    /// triggered it.
    Action(Action),
}

enum ScanState {
    Normal,
    /// Accumulating bytes that so far match a prefix of `PASTE_START`.
    MatchingPasteStart(Vec<u8>),
    /// Inside a bracketed paste, accumulating a possible `PASTE_END`
    /// suffix. Content that can no longer be part of a future match is
    /// flushed as it's confirmed, bounding memory use.
    InPaste(Vec<u8>),
}

/// Buffers raw input bytes arriving on independent, arbitrarily-split OS
/// `read()` calls and turns them into bound `Action`s or pty-forwardable
/// byte runs. Only handles the byte shapes tymux's default binding set
/// actually needs — a single control-byte leader followed by a single
/// plain ASCII char, plus bracketed-paste passthrough — not a general
/// ANSI/terminal-escape parser (see plan.md Unresolved Question #13).
pub struct KeystrokeReassembler {
    bindings: Vec<KeyBinding>,
    leader: KeyEvent,
    /// `Some(bytes)` while the leader is armed, holding the exact raw
    /// byte(s) that armed it — needed so an unmatched follow-up can
    /// forward the literal leader bytes plus itself, rather than losing
    /// them.
    armed_bytes: Option<Vec<u8>>,
    scan: ScanState,
}

impl KeystrokeReassembler {
    pub fn new(config: &crate::config::TymuxConfig) -> Self {
        let leader = parse_leader(&config.leader);
        Self {
            bindings: config.bindings.clone(),
            leader,
            armed_bytes: None,
            scan: ScanState::Normal,
        }
    }

    // Reserved for Epic 6's mode-reactive status bar (needs to know
    // whether the prefix is currently armed to render the right hint
    // table); not called from production code yet, only tests.
    #[allow(dead_code)]
    pub fn is_armed(&self) -> bool {
        self.armed_bytes.is_some()
    }

    pub fn process(&mut self, bytes: &[u8]) -> Vec<ReassembledOutput> {
        let mut out = Vec::new();
        for &b in bytes {
            self.process_byte(b, &mut out);
        }
        out
    }

    fn process_byte(&mut self, b: u8, out: &mut Vec<ReassembledOutput>) {
        match &mut self.scan {
            ScanState::Normal => {
                if b == 0x1b {
                    self.scan = ScanState::MatchingPasteStart(vec![b]);
                } else {
                    self.handle_normal_byte(b, out);
                }
            }
            ScanState::MatchingPasteStart(buf) => {
                buf.push(b);
                if buf.as_slice() == PASTE_START {
                    self.scan = ScanState::InPaste(Vec::new());
                } else if !PASTE_START.starts_with(buf.as_slice()) {
                    let flushed = std::mem::take(buf);
                    self.scan = ScanState::Normal;
                    out.push(ReassembledOutput::Forward(flushed));
                }
            }
            ScanState::InPaste(buf) => {
                buf.push(b);
                if buf.ends_with(PASTE_END) {
                    let content_len = buf.len() - PASTE_END.len();
                    let content = buf[..content_len].to_vec();
                    self.scan = ScanState::Normal;
                    if !content.is_empty() {
                        out.push(ReassembledOutput::Forward(content));
                    }
                } else if buf.len() > PASTE_END.len() {
                    let keep = PASTE_END.len() - 1;
                    let flush_upto = buf.len() - keep;
                    let flushed: Vec<u8> = buf.drain(..flush_upto).collect();
                    out.push(ReassembledOutput::Forward(flushed));
                }
            }
        }
    }

    fn handle_normal_byte(&mut self, b: u8, out: &mut Vec<ReassembledOutput>) {
        let key_event = classify_byte(b);
        match self.armed_bytes.take() {
            None => {
                if key_event == self.leader {
                    self.armed_bytes = Some(vec![b]);
                } else {
                    out.push(ReassembledOutput::Forward(vec![b]));
                }
            }
            Some(armed) => {
                if key_event == self.leader {
                    // Escape hatch (AC3): leader pressed twice forwards
                    // the literal leader byte once and returns to Idle.
                    out.push(ReassembledOutput::Forward(vec![b]));
                } else if let Some(binding) = self
                    .bindings
                    .iter()
                    .find(|bnd| bnd.sequence.len() == 2 && bnd.sequence[1] == key_event)
                {
                    out.push(ReassembledOutput::Action(binding.action));
                } else {
                    // No binding matched — forward the leader bytes plus
                    // this one as literal input rather than swallowing
                    // real keystrokes the user didn't intend as a prefix.
                    let mut forwarded = armed;
                    forwarded.push(b);
                    out.push(ReassembledOutput::Forward(forwarded));
                }
            }
        }
    }
}

/// Maps a raw byte to the `KeyEvent` it represents, for the sole purpose
/// of comparing against configured bindings — see the module doc's scope
/// note. Control bytes 0x01-0x1a map to Ctrl-a..Ctrl-z; everything else
/// is treated as a plain (unmodified) character key.
fn classify_byte(b: u8) -> KeyEvent {
    match b {
        1..=26 => {
            let c = (b'a' + b - 1) as char;
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
        }
        _ => KeyEvent::new(KeyCode::Char(b as char), KeyModifiers::NONE),
    }
}

fn parse_leader(raw: &str) -> KeyEvent {
    // Reuses the same "C-<char>" grammar as keybinding values; a leader
    // is always exactly one token.
    if let Some(rest) = raw.strip_prefix("C-") {
        if let Some(c) = rest.chars().next() {
            return KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        }
    }
    KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TymuxConfig;

    fn reassembler() -> KeystrokeReassembler {
        KeystrokeReassembler::new(&TymuxConfig::defaults())
    }

    #[test]
    fn keystroke_reassembler_should_fire_detach_action_exactly_once_when_leader_and_key_split_across_reads(
    ) {
        let mut r = reassembler();
        let first = r.process(&[0x02]); // Ctrl-b
        assert!(
            first.is_empty(),
            "arming the leader alone must not yet fire anything"
        );
        let second = r.process(b"d");
        assert_eq!(second, vec![ReassembledOutput::Action(Action::Detach)]);
    }

    #[test]
    fn keystroke_reassembler_should_fire_detach_action_exactly_once_when_leader_and_key_arrive_in_one_read(
    ) {
        let mut r = reassembler();
        let out = r.process(&[0x02, b'd']);
        assert_eq!(out, vec![ReassembledOutput::Action(Action::Detach)]);
    }

    #[test]
    fn keystroke_reassembler_should_forward_paste_unmodified_when_pasted_bytes_match_a_binding_sequence(
    ) {
        let mut r = reassembler();
        let mut input = Vec::new();
        input.extend_from_slice(PASTE_START);
        input.extend_from_slice(&[0x02, b'd']); // the exact bytes that would otherwise fire Detach
        input.extend_from_slice(PASTE_END);

        let out = r.process(&input);
        assert_eq!(out, vec![ReassembledOutput::Forward(vec![0x02, b'd'])]);
    }

    #[test]
    fn prefix_state_should_forward_literal_leader_byte_and_return_to_idle_when_leader_pressed_twice(
    ) {
        let mut r = reassembler();
        r.process(&[0x02]);
        assert!(r.is_armed());
        let out = r.process(&[0x02]);
        assert_eq!(out, vec![ReassembledOutput::Forward(vec![0x02])]);
        assert!(!r.is_armed());
    }

    #[test]
    fn unmatched_follow_up_forwards_leader_and_key_rather_than_dropping_them() {
        let mut r = reassembler();
        r.process(&[0x02]);
        let out = r.process(b"Z"); // not bound to anything
        assert_eq!(out, vec![ReassembledOutput::Forward(vec![0x02, b'Z'])]);
    }

    #[test]
    fn ordinary_input_passes_through_when_prefix_never_armed() {
        let mut r = reassembler();
        let out = r.process(b"hello");
        assert_eq!(
            out,
            b"hello"
                .iter()
                .map(|&b| ReassembledOutput::Forward(vec![b]))
                .collect::<Vec<_>>()
        );
    }
}
