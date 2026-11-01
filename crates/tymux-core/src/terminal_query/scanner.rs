use std::borrow::Cow;

use super::{classify_csi, osc_query};

/// Longest open sequence held across a chunk boundary. The emulator answers a query however long
/// its parameters are (`ESC[` + 300 zeros + `c` is a DA1), so this must be generous: past it the
/// bytes are released and a query completing later is answered but not stripped.
const MAX_HELD: usize = 64 * 1024;

/// One chunk, as clients should see it with and without queries.
pub(crate) struct Scanned<'a> {
    pub(crate) raw: Cow<'a, [u8]>,
    /// `Some` only when queries were removed; otherwise the stripped view is `raw`.
    removed: Option<Vec<u8>>,
}

impl Scanned<'_> {
    pub(crate) fn stripped(&self) -> &[u8] {
        self.removed.as_deref().unwrap_or(&self.raw)
    }
}

/// CSI finals that can be one of our queries (`c`, `n`, `q`, `p`, `t`). Every other CSI is skipped
/// without involving the tokenizer, which keeps SGR-heavy output cheap.
fn may_be_query_final(b: u8) -> bool {
    matches!(b, b'c' | b'n' | b'q' | b'p' | b't')
}

#[derive(Default)]
struct Tracker {
    seq_start: Option<usize>,
    non_ground: bool,
    index: usize,
    /// The byte being fed, so ESC- and BEL-terminated OSC can be told apart.
    byte: u8,
    /// C0 controls executed inside the open sequence. A terminal acts on them, so they survive
    /// when the sequence around them is removed.
    embedded_c0: Vec<u8>,
    /// `(start, end)` byte ranges to drop, each replaced by the C0 controls it contained.
    removals: Vec<(usize, usize, Vec<u8>)>,
}

impl Tracker {
    fn remove(&mut self, start: usize, end: usize) {
        self.removals
            .push((start, end, std::mem::take(&mut self.embedded_c0)));
    }

    fn close(&mut self) {
        self.seq_start = None;
        self.non_ground = false;
        self.embedded_c0.clear();
    }
}

impl vte::Perform for Tracker {
    fn print(&mut self, _: char) {
        self.close();
    }

    fn execute(&mut self, byte: u8) {
        if self.seq_start.is_some() && byte != 0x18 && byte != 0x1a {
            self.embedded_c0.push(byte);
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, inter: &[u8], _ignore: bool, c: char) {
        let p0 = params
            .iter()
            .next()
            .and_then(|p| p.first())
            .copied()
            .unwrap_or(0);
        if let (Some(start), Ok(fin)) = (self.seq_start, u8::try_from(c)) {
            if classify_csi(inter.first().copied(), inter.get(1).copied(), p0, fin).is_some() {
                self.remove(start, self.index + 1);
            }
        }
        self.close();
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bel: bool) {
        if let Some(start) = self.seq_start {
            if osc_query(params).is_some() {
                // An ESC/CAN terminator is not part of the query: it may start the next sequence.
                let end = if self.byte == 0x07 {
                    self.index + 1
                } else {
                    self.index
                };
                self.remove(start, end);
            }
        }
        if self.byte == 0x07 {
            self.close();
        }
    }

    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.close();
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        // DCS payloads are not queries and can be long: never hold them back.
        self.seq_start = None;
    }

    fn unhook(&mut self) {
        self.close();
    }
}

#[derive(Default)]
pub(crate) struct QueryScanner {
    parser: vte::Parser,
    tracker: Tracker,
    held: Vec<u8>,
}

impl QueryScanner {
    /// Bytes held back for an unfinished sequence; call at end of stream to release them.
    pub(crate) fn take_held(&mut self) -> Vec<u8> {
        self.tracker.seq_start = None;
        std::mem::take(&mut self.held)
    }

    pub(crate) fn feed<'a>(&mut self, chunk: &'a [u8]) -> Scanned<'a> {
        if self.held.is_empty() && !self.tracker.non_ground && memchr::memchr(0x1b, chunk).is_none()
        {
            return Scanned {
                raw: Cow::Borrowed(chunk),
                removed: None,
            };
        }
        let held_len = self.held.len();
        let buf: Cow<'a, [u8]> = if held_len == 0 {
            Cow::Borrowed(chunk)
        } else {
            let mut joined = std::mem::take(&mut self.held);
            joined.extend_from_slice(chunk);
            Cow::Owned(joined)
        };
        let t = &mut self.tracker;
        t.removals.clear();
        // Held bytes were already tokenized last time; only the new bytes are fed.
        t.seq_start = (held_len > 0).then_some(0);
        scan(&mut self.parser, t, &buf, held_len);

        let hold_from = t
            .seq_start
            .filter(|&s| buf.len() - s <= MAX_HELD)
            .unwrap_or(buf.len());
        if hold_from < buf.len() {
            self.held = buf[hold_from..].to_vec();
        } else if t.seq_start.is_some() {
            t.seq_start = None;
            t.embedded_c0.clear();
        }

        let removed = if t.removals.is_empty() {
            None
        } else {
            let emit = &buf[..hold_from];
            let mut out = Vec::with_capacity(emit.len());
            let mut at = 0;
            for (start, end, c0) in &t.removals {
                out.extend_from_slice(&emit[at..*start]);
                out.extend_from_slice(c0);
                at = *end;
            }
            out.extend_from_slice(&emit[at..]);
            Some(out)
        };
        let raw = match buf {
            Cow::Borrowed(b) => Cow::Borrowed(&b[..hold_from]),
            Cow::Owned(mut v) => {
                v.truncate(hold_from);
                Cow::Owned(v)
            }
        };
        Scanned { raw, removed }
    }
}

/// Tokenizes `buf[from..]`, skipping ground-state text and non-query CSIs without the tokenizer.
fn scan(parser: &mut vte::Parser, t: &mut Tracker, buf: &[u8], from: usize) {
    let mut i = from;
    while i < buf.len() {
        if !t.non_ground {
            // In ground state plain text cannot open a sequence and the tokenizer has nothing to
            // learn from it, so jump straight to the next ESC without feeding it.
            match find_esc(&buf[i..]) {
                Some(p) => i += p,
                None => break,
            }
            if let Some(end) = complete_other_csi(&buf[i..]) {
                i += end;
                continue;
            }
        }
        let b = buf[i];
        t.index = i;
        t.byte = b;
        parser.advance(t, &[b]);
        match b {
            0x1b => {
                t.seq_start = Some(i);
                t.embedded_c0.clear();
                t.non_ground = true;
            }
            0x18 | 0x1a => t.close(),
            // A CSI that ends in a malformed state (e.g. an intermediate before a parameter) is
            // swallowed by the tokenizer without any callback.
            0x40..=0x7e => {
                if let Some(start) = t.seq_start {
                    if i > start + 1 && buf[start + 1] == b'[' {
                        t.close();
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Position of the next ESC. SGR-dense output has one every few bytes, where a short scalar scan
/// beats `memchr`'s call overhead; long runs of text fall through to `memchr`.
fn find_esc(s: &[u8]) -> Option<usize> {
    const SHORT: usize = 24;
    let head = &s[..s.len().min(SHORT)];
    match head.iter().position(|&b| b == 0x1b) {
        Some(p) => Some(p),
        None if s.len() > SHORT => memchr::memchr(0x1b, &s[SHORT..]).map(|p| p + SHORT),
        None => None,
    }
}

/// Length of the CSI at the start of `s` (`ESC [ params final`) when it is complete, well formed
/// and cannot be a query; `None` for anything else (including an incomplete one at the chunk end,
/// which the caller must hold back).
fn complete_other_csi(s: &[u8]) -> Option<usize> {
    if s.len() < 3 || s[0] != 0x1b || s[1] != b'[' {
        return None;
    }
    let params = s[2..]
        .iter()
        .take_while(|b| (0x20..=0x3f).contains(*b))
        .count();
    let fin = *s.get(2 + params)?;
    ((0x40..=0x7e).contains(&fin) && !may_be_query_final(fin)).then_some(params + 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
        let mut s = QueryScanner::default();
        let (mut raw, mut stripped) = (Vec::new(), Vec::new());
        for c in chunks {
            let out = s.feed(c);
            raw.extend_from_slice(&out.raw);
            stripped.extend_from_slice(out.stripped());
        }
        (raw, stripped)
    }

    #[test]
    fn whole_queries_should_be_removed_and_everything_else_kept() {
        let input = b"hi\x1b[6nthere\x1b[1;31mred\x1b[c\x1b]11;?\x07end\x1b[?2004$p";
        let (raw, stripped) = strip_all(&[input]);
        assert_eq!(raw, input);
        assert_eq!(stripped, b"hithere\x1b[1;31mredend");
        let keep = b"\x1b[?u\x1b]10;#fff\x07\x1b[7n";
        assert_eq!(strip_all(&[keep]).1, keep);
    }

    #[test]
    fn plain_text_should_pass_through_borrowed() {
        let mut s = QueryScanner::default();
        let out = s.feed(b"plain");
        assert!(matches!(out.raw, Cow::Borrowed(_)) && out.removed.is_none());
    }

    #[test]
    fn a_query_split_across_chunks_should_be_removed_whole() {
        for split in 1..b"ab\x1b[6ncd".len() {
            let input = b"ab\x1b[6ncd";
            let (raw, stripped) = strip_all(&[&input[..split], &input[split..]]);
            assert_eq!(raw, input, "split at {split}");
            assert_eq!(stripped, b"abcd", "split at {split}");
        }
        let (_, stripped) = strip_all(&[b"x\x1b]11;", b"?\x1b", b"\\y"]);
        assert_eq!(stripped, b"x\x1b\\y");
    }

    #[test]
    fn a_held_prefix_that_is_not_a_query_should_be_released_with_the_next_chunk() {
        let (raw, stripped) = strip_all(&[b"a\x1b[1;3", b"1mb"]);
        assert_eq!(raw, b"a\x1b[1;31mb");
        assert_eq!(stripped, b"a\x1b[1;31mb");
    }

    #[test]
    fn no_emitted_chunk_should_end_inside_a_sequence() {
        let mut s = QueryScanner::default();
        let first = s.feed(b"ab\x1b[");
        assert_eq!(&*first.raw, b"ab");
        let second = s.feed(b"6n!");
        assert_eq!(&*second.raw, b"\x1b[6n!");
        assert_eq!(second.stripped(), b"!");
    }

    #[test]
    fn an_oversized_parameter_should_not_be_stripped_because_the_parser_does_not_answer_it() {
        let (_, stripped) = strip_all(&[b"\x1b[65536c"]);
        assert_eq!(stripped, b"\x1b[65536c");
        let (_, stripped) = strip_all(&[b"\x1b[0:1c"]);
        assert_eq!(stripped, b"", "first sub-parameter is 0, a DA1");
    }

    #[test]
    fn a_dcs_payload_should_not_be_scanned_for_queries() {
        let input = b"\x1bPq[c6n\x1b\\after";
        assert_eq!(strip_all(&[input]).1, input);
    }

    #[test]
    fn can_should_abort_a_held_sequence() {
        let (_, stripped) = strip_all(&[b"\x1b[6", b"\x18n"]);
        assert_eq!(stripped, b"\x1b[6\x18n");
    }

    #[test]
    fn an_unterminated_osc_longer_than_the_hold_cap_should_be_released() {
        let mut s = QueryScanner::default();
        let mut big = b"\x1b]0;".to_vec();
        big.resize(big.len() + MAX_HELD + 10, b'x');
        let out = s.feed(&big);
        assert_eq!(&*out.raw, &big[..]);
    }

    #[test]
    fn a_c0_control_inside_a_query_should_survive_its_removal() {
        assert_eq!(
            strip_all(&[b"a[6
nb"])
            .1,
            b"a
b"
        );
        assert_eq!(
            strip_all(&[
                b"a[6", b"
nb"
            ])
            .1,
            b"a
b"
        );
    }

    #[test]
    fn an_osc_mixing_a_colour_set_with_a_query_should_be_left_intact() {
        let mixed = b"\x1b]4;1;rgb:ff/00/00;2;?\x07";
        assert_eq!(strip_all(&[mixed]).1, mixed);
        let mixed10 = b"\x1b]10;?;#00f\x07";
        assert_eq!(strip_all(&[mixed10]).1, mixed10);
    }

    #[test]
    fn a_pure_osc4_query_with_many_pairs_should_be_removed_whole() {
        let mut q = String::from("\x1b]4");
        for i in 0..40 {
            q.push_str(&format!(";{i};?"));
        }
        q.push('\x07');
        assert_eq!(strip_all(&[q.as_bytes()]).1, b"");
    }

    #[test]
    fn sgr_heavy_output_should_pass_through_untouched_and_borrowed() {
        let sgr = b"\x1b[1;31mred\x1b[0m \x1b[38;5;208morange\x1b[m\x1b[?25h\x1b[2J";
        let mut s = QueryScanner::default();
        let out = s.feed(sgr);
        assert_eq!(&*out.raw, sgr);
        assert!(out.removed.is_none());
        assert!(matches!(out.raw, Cow::Borrowed(_)));
    }

    #[test]
    fn a_long_query_split_across_chunks_should_still_be_removed_whole() {
        let mut csi = b"\x1b[".to_vec();
        csi.resize(2 + 300, b'0');
        csi.push(b'c');
        let (raw, stripped) = strip_all(&[&csi[..150], &csi[150..]]);
        assert_eq!(raw, csi);
        assert_eq!(stripped, b"");

        let mut osc = String::from("\x1b]4");
        for i in 0..60 {
            osc.push_str(&format!(";{i};?"));
        }
        osc.push('\x07');
        let b = osc.as_bytes();
        let (raw, stripped) = strip_all(&[&b[..100], &b[100..200], &b[200..]]);
        assert_eq!(raw, b);
        assert_eq!(stripped, b"");
    }

    #[test]
    fn take_held_should_release_an_unfinished_sequence() {
        let mut s = QueryScanner::default();
        assert_eq!(&*s.feed(b"x\x1b[6").raw, b"x");
        assert_eq!(s.take_held(), b"\x1b[6");
        assert!(s.take_held().is_empty());
    }

    #[test]
    fn a_query_with_a_long_prefix_should_be_removed_whole_at_every_split() {
        // 300 zeros is past a 256-byte hold cap, so a smaller MAX_HELD would let it through
        let mut csi = b"\x1b[".to_vec();
        csi.resize(2 + 300, b'0');
        csi.push(b'c');
        for split in 1..csi.len() {
            let (raw, stripped) = strip_all(&[&csi[..split], &csi[split..]]);
            assert_eq!(raw, csi, "split at {split}");
            assert_eq!(stripped, b"", "split at {split}");
        }
    }

    #[test]
    fn an_open_sequence_past_the_hold_cap_should_be_released_and_its_late_query_left_unstripped() {
        let mut head = b"\x1b[".to_vec();
        head.resize(2 + MAX_HELD + 10, b'0');
        let mut s = QueryScanner::default();
        let first = s.feed(&head);
        assert_eq!(&*first.raw, &head[..], "released unheld");
        let second = s.feed(b"c");
        assert_eq!(&*second.raw, b"c");
        assert_eq!(second.stripped(), b"c", "documented bound: not stripped");
    }
}
