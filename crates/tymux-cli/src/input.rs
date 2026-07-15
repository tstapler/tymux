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
    /// `Some((bytes, key_event))` while a leader is armed: `bytes` holds
    /// the exact raw byte(s) that armed it, for forwarding on no-match;
    /// `key_event` is that same byte classified, used to match a
    /// binding's *first* key — not just its second — so a per-action
    /// override with a leader other than the global `leader` (e.g.
    /// `detach = "C-a d"` while the global leader stays `C-b`) is
    /// actually reachable. Matching only on the second key (as an
    /// earlier version of this reassembler did) let any binding whose
    /// second key happened to match fire regardless of which key armed
    /// it, and made a different-leader override permanently unreachable
    /// (found via v1.0.0-alpha.7's manual release verification).
    armed: Option<(Vec<u8>, KeyEvent)>,
    scan: ScanState,
}

impl KeystrokeReassembler {
    pub fn new(config: &crate::config::TymuxConfig) -> Self {
        let leader = parse_leader(&config.leader);
        Self {
            bindings: config.bindings.clone(),
            leader,
            armed: None,
            scan: ScanState::Normal,
        }
    }

    /// Used by the status bar's mode-reactive rendering (Story 6.4) to
    /// know whether to show the prefix-armed hint table.
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
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
        match self.armed.take() {
            None => {
                // Arms on the global leader, or on the first key of any
                // configured binding — a per-action override's own
                // leader byte must be able to arm even when it differs
                // from the global default.
                let arms = key_event == self.leader
                    || self
                        .bindings
                        .iter()
                        .any(|bnd| bnd.sequence.first() == Some(&key_event));
                if arms {
                    self.armed = Some((vec![b], key_event));
                } else {
                    out.push(ReassembledOutput::Forward(vec![b]));
                }
            }
            Some((armed_bytes, armed_key)) => {
                if key_event == armed_key {
                    // Escape hatch (AC3): the same key that armed this
                    // pressed twice forwards the literal byte once and
                    // returns to Idle.
                    out.push(ReassembledOutput::Forward(vec![b]));
                } else if let Some(binding) = self.bindings.iter().find(|bnd| {
                    bnd.sequence.len() == 2
                        && bnd.sequence[0] == armed_key
                        && bnd.sequence[1] == key_event
                }) {
                    out.push(ReassembledOutput::Action(binding.action));
                } else {
                    // No binding matched — forward the leader bytes plus
                    // this one as literal input rather than swallowing
                    // real keystrokes the user didn't intend as a prefix.
                    let mut forwarded = armed_bytes;
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

    /// Regression test (found via v1.0.0-alpha.7's manual release
    /// verification): a per-action binding using a leader byte other
    /// than the global default was silently unreachable — the
    /// reassembler only ever armed on `config.leader`, so a binding like
    /// `detach = "C-a d"` (global leader stays `C-b`) never fired;
    /// `C-a` passed straight through to the pty instead.
    #[test]
    fn keystroke_reassembler_should_fire_action_when_binding_uses_a_different_leader_than_the_global_default(
    ) {
        let config = TymuxConfig::from_toml_str("[keybindings]\ndetach = \"C-a d\"\n");
        let mut r = KeystrokeReassembler::new(&config);

        let armed = r.process(&[0x01]); // Ctrl-a, not the global C-b leader
        assert!(
            armed.is_empty(),
            "C-a must arm since a binding starts with it"
        );
        assert!(r.is_armed());

        let fired = r.process(b"d");
        assert_eq!(fired, vec![ReassembledOutput::Action(Action::Detach)]);
    }

    /// The other half of the same regression: a binding matching only by
    /// its *second* key, regardless of which key armed it, would have
    /// let an unrelated leader's follow-up wrongly fire this binding —
    /// confirms C-b (the global leader, distinct from this binding's own
    /// C-a) does NOT fire the C-a-d binding.
    #[test]
    fn keystroke_reassembler_should_not_fire_action_when_second_key_matches_but_leader_differs() {
        let config = TymuxConfig::from_toml_str("[keybindings]\ndetach = \"C-a d\"\n");
        let mut r = KeystrokeReassembler::new(&config);

        r.process(&[0x02]); // Ctrl-b, the global leader — arms, but isn't this binding's leader
        let out = r.process(b"d");
        assert_eq!(
            out,
            vec![ReassembledOutput::Forward(vec![0x02, b'd'])],
            "C-b d must not fire a binding whose configured leader is C-a"
        );
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
