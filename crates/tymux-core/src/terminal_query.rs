//! Answers terminal queries (DA, DSR, DECRQM, XTVERSION, OSC colour queries, XTWINOPS size) on
//! behalf of a pane when no client is attached.
//!
//! Normally the attached client's emulator (xterm.js) answers queries it sees in the raw byte
//! stream. With nobody attached a full-screen app that starts up and waits for a reply (vim, fzf,
//! Claude Code) would hang, so the daemon answers instead. When a client *is* attached the daemon
//! stays silent: both answering would hand the app duplicate replies.
//!
//! Deliberately not answered: the kitty keyboard query (`CSI ? u`). Replying "supported" would make
//! the app enable the protocol, and the xterm.js client that attaches later sends legacy keys.
//! Apps probe it together with DA1, so the DA1 reply alone resolves their wait.

use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use vt100::{Callbacks, MouseProtocolEncoding, MouseProtocolMode, Screen};

/// Replies allowed in a burst, and how fast the allowance refills. A program printing queries in a
/// loop gets a bounded number of answers rather than a stream of writes into its own stdin.
const RATE_BURST: u32 = 16;
const RATE_REFILL_PER_SEC: u32 = 8;
/// Replies held back while a client is attached, in case it detaches before seeing the chunk.
const MAX_DEFERRED: usize = 16;
/// Longest sequence the replay stripper will inspect; anything longer is not a query we answer.
const MAX_SEQ_LEN: usize = 64;
/// `OSC 4` may carry many `index;?` pairs; answer at most this many per sequence.
const MAX_OSC4_PAIRS: usize = 8;

/// The queries this module answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    Da1,
    Da2,
    StatusReport,
    CursorPosition,
    DecCursorPosition,
    XtVersion,
    /// DECRQM: `dec` is true for the `?`-prefixed (DEC private) form.
    ModeReport {
        dec: bool,
        mode: u16,
    },
    /// XTWINOPS 18 (text area) / 19 (screen) size in characters.
    SizeReport(u16),
}

/// Classifies a CSI by the first two intermediate/private bytes (as vte reports them), the first
/// parameter and the final byte.
fn classify_csi(i1: Option<u8>, i2: Option<u8>, p0: u16, fin: u8) -> Option<Query> {
    match (i1, i2, fin) {
        (None, None, b'c') if p0 == 0 => Some(Query::Da1),
        (Some(b'>'), None, b'c') if p0 == 0 => Some(Query::Da2),
        (None, None, b'n') if p0 == 5 => Some(Query::StatusReport),
        (None, None, b'n') if p0 == 6 => Some(Query::CursorPosition),
        (Some(b'?'), None, b'n') if p0 == 6 => Some(Query::DecCursorPosition),
        (Some(b'>'), None, b'q') if p0 == 0 => Some(Query::XtVersion),
        (Some(b'$'), None, b'p') => Some(Query::ModeReport {
            dec: false,
            mode: p0,
        }),
        (Some(b'?'), Some(b'$'), b'p') => Some(Query::ModeReport {
            dec: true,
            mode: p0,
        }),
        (None, None, b't') if p0 == 18 || p0 == 19 => Some(Query::SizeReport(p0)),
        _ => None,
    }
}

fn mode_status(screen: &Screen, dec: bool, mode: u16) -> u8 {
    let flag = |on: bool| if on { 1 } else { 2 };
    let mouse = screen.mouse_protocol_mode();
    if !dec {
        return match mode {
            4 => flag(screen.insert_mode()),
            20 => flag(screen.newline_mode()),
            _ => 0,
        };
    }
    match mode {
        1 => flag(screen.application_cursor()),
        5 => flag(screen.reverse_video()),
        6 => flag(screen.origin_mode()),
        7 => flag(screen.autowrap()),
        25 => flag(!screen.hide_cursor()),
        47 | 1047 | 1049 => flag(screen.alternate_screen()),
        66 => flag(screen.application_keypad()),
        9 => flag(mouse == MouseProtocolMode::Press),
        1000 => flag(mouse == MouseProtocolMode::PressRelease),
        1002 => flag(mouse == MouseProtocolMode::ButtonMotion),
        1003 => flag(mouse == MouseProtocolMode::AnyMotion),
        1004 => flag(screen.focus_events()),
        1005 => flag(screen.mouse_protocol_encoding() == MouseProtocolEncoding::Utf8),
        1006 => flag(screen.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr),
        1015 => flag(screen.mouse_protocol_encoding() == MouseProtocolEncoding::Urxvt),
        2004 => flag(screen.bracketed_paste()),
        _ => 0,
    }
}

/// 1-based cursor row/col as the app sees it (relative to the scroll region under DECOM).
fn cursor_report(screen: &Screen) -> (u32, u32) {
    let (row, col) = screen.cursor_position();
    let row = if screen.origin_mode() {
        // scroll_region() is 1-based, (0, 0) for the whole screen
        row.saturating_sub(screen.scroll_region().0.saturating_sub(1))
    } else {
        row
    };
    (u32::from(row) + 1, u32::from(col) + 1)
}

fn reply_for(query: Query, screen: &Screen) -> Vec<u8> {
    match query {
        // Same identity xterm.js reports: VT100 with advanced video option.
        Query::Da1 => b"\x1b[?1;2c".to_vec(),
        Query::Da2 => b"\x1b[>0;276;0c".to_vec(),
        Query::StatusReport => b"\x1b[0n".to_vec(),
        Query::CursorPosition => {
            let (r, c) = cursor_report(screen);
            format!("\x1b[{r};{c}R").into_bytes()
        }
        Query::DecCursorPosition => {
            let (r, c) = cursor_report(screen);
            format!("\x1b[?{r};{c};1R").into_bytes()
        }
        Query::XtVersion => {
            format!("\x1bP>|tymux {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes()
        }
        Query::ModeReport { dec, mode } => {
            let status = mode_status(screen, dec, mode);
            let prefix = if dec { "?" } else { "" };
            format!("\x1b[{prefix}{mode};{status}$y").into_bytes()
        }
        Query::SizeReport(kind) => {
            let (rows, cols) = screen.size();
            let op = if kind == 18 { 8 } else { 9 };
            format!("\x1b[{op};{rows};{cols}t").into_bytes()
        }
    }
}

/// Default palette, matching xterm.js's default theme (so an app sees the same colours whether
/// the daemon or an attached browser answers).
const DEFAULT_FG: (u8, u8, u8) = (0xff, 0xff, 0xff);
const DEFAULT_BG: (u8, u8, u8) = (0x00, 0x00, 0x00);
const DEFAULT_CURSOR: (u8, u8, u8) = (0xff, 0xff, 0xff);
const ANSI16: [(u8, u8, u8); 16] = [
    (0x2e, 0x34, 0x36),
    (0xcc, 0x00, 0x00),
    (0x4e, 0x9a, 0x06),
    (0xc4, 0xa0, 0x00),
    (0x34, 0x65, 0xa4),
    (0x75, 0x50, 0x7b),
    (0x06, 0x98, 0x9a),
    (0xd3, 0xd7, 0xcf),
    (0x55, 0x57, 0x53),
    (0xef, 0x29, 0x29),
    (0x8a, 0xe2, 0x34),
    (0xfc, 0xe9, 0x4f),
    (0x72, 0x9f, 0xcf),
    (0xad, 0x7f, 0xa8),
    (0x34, 0xe2, 0xe2),
    (0xee, 0xee, 0xec),
];

fn palette(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI16[usize::from(index)],
        16..=231 => {
            const LEVELS: [u8; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let i = usize::from(index - 16);
            (LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6])
        }
        _ => {
            let v = 8 + 10 * (index - 232);
            (v, v, v)
        }
    }
}

fn osc_color(prefix: &str, (r, g, b): (u8, u8, u8)) -> Vec<u8> {
    format!("\x1b]{prefix};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\").into_bytes()
}

/// Replies to an OSC colour query: `OSC 10|11|12 ; ?` or `OSC 4 ; index ; ? [; index ; ?]...`.
fn osc_replies(params: &[&[u8]]) -> Vec<Vec<u8>> {
    match params {
        [b"10", b"?"] => vec![osc_color("10", DEFAULT_FG)],
        [b"11", b"?"] => vec![osc_color("11", DEFAULT_BG)],
        [b"12", b"?"] => vec![osc_color("12", DEFAULT_CURSOR)],
        [b"4", rest @ ..] => rest
            .chunks(2)
            .filter(|pair| pair.len() == 2 && pair[1] == b"?")
            .filter_map(|pair| {
                let index: u8 = std::str::from_utf8(pair[0]).ok()?.parse().ok()?;
                Some(osc_color(&format!("4;{index}"), palette(index)))
            })
            .take(MAX_OSC4_PAIRS)
            .collect(),
        _ => Vec::new(),
    }
}

/// Removes complete query sequences from a chunk bound for the replay buffer.
///
/// A client resuming from the buffer would otherwise see (and answer) queries the app already got
/// answered, duplicating replies. A sequence split across two chunks is left intact: that is a
/// rare race, and mangling it would leave the client parser mid-sequence.
pub(crate) fn strip_queries(chunk: &[u8]) -> Cow<'_, [u8]> {
    if !chunk.contains(&0x1b) {
        return Cow::Borrowed(chunk);
    }
    let mut out: Option<Vec<u8>> = None;
    let mut copied_to = 0;
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] != 0x1b {
            i += 1;
            continue;
        }
        let Some(len) = query_len_at(&chunk[i..]) else {
            i += 1;
            continue;
        };
        let buf = out.get_or_insert_with(|| Vec::with_capacity(chunk.len()));
        buf.extend_from_slice(&chunk[copied_to..i]);
        i += len;
        copied_to = i;
    }
    match out {
        None => Cow::Borrowed(chunk),
        Some(mut buf) => {
            buf.extend_from_slice(&chunk[copied_to..]);
            Cow::Owned(buf)
        }
    }
}

/// Length of the query sequence starting at `s[0] == ESC`, if it is one we answer.
fn query_len_at(s: &[u8]) -> Option<usize> {
    let window = &s[..s.len().min(MAX_SEQ_LEN)];
    match window.get(1)? {
        b'[' => csi_query_len(window),
        b']' => osc_query_len(window),
        _ => None,
    }
}

fn csi_query_len(s: &[u8]) -> Option<usize> {
    let mut i = 2;
    let (mut private, mut inter, mut digits) = (None, None, Vec::new());
    while let Some(&b) = s.get(i) {
        match b {
            b'<'..=b'?' if i == 2 => private = Some(b),
            b'0'..=b'9' | b';' | b':' => digits.push(b),
            0x20..=0x2f => {
                if inter.is_none() {
                    inter = Some(b);
                }
            }
            0x40..=0x7e => {
                let p0 = digits
                    .split(|&d| d == b';')
                    .next()
                    .and_then(|d| std::str::from_utf8(d).ok())
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(0);
                // vte lists the private marker first, then other intermediates.
                let (i1, i2) = match private {
                    Some(p) => (Some(p), inter),
                    None => (inter, None),
                };
                return classify_csi(i1, i2, p0, b).map(|_| i + 1);
            }
            _ => return None,
        }
        i += 1;
    }
    None
}

fn osc_query_len(s: &[u8]) -> Option<usize> {
    let end = s.iter().skip(2).position(|&b| b == 0x07 || b == 0x1b)? + 2;
    let total = if s[end] == 0x07 {
        end + 1
    } else if s.get(end + 1) == Some(&b'\\') {
        end + 2
    } else {
        return None;
    };
    let params: Vec<&[u8]> = s[2..end].split(|&b| b == b';').collect();
    (!osc_replies(&params).is_empty()).then_some(total)
}

/// Where replies go and whether a client is attached; abstracted so the policy is unit-testable.
pub(crate) trait ReplyOut: Send + Sync {
    fn client_attached(&self) -> bool;
    fn write_reply(&self, bytes: &[u8]);
}

struct State {
    tokens: f64,
    last: Instant,
    deferred: Vec<Vec<u8>>,
}

/// Shared between the parser's callbacks and the pane's reader thread.
pub(crate) struct QueryReplies {
    out: Box<dyn ReplyOut>,
    state: Mutex<State>,
    has_deferred: AtomicBool,
}

impl QueryReplies {
    pub(crate) fn new(out: Box<dyn ReplyOut>) -> Self {
        Self {
            out,
            state: Mutex::new(State {
                tokens: f64::from(RATE_BURST),
                last: Instant::now(),
                deferred: Vec::new(),
            }),
            has_deferred: AtomicBool::new(false),
        }
    }

    /// Writes `reply` now when nobody is attached; otherwise parks it until [`Self::settle`]
    /// learns whether the chunk reached a client.
    fn offer(&self, reply: Vec<u8>, now: Instant) {
        let mut st = self.state.lock().unwrap();
        let elapsed = now.saturating_duration_since(st.last);
        st.last = now;
        st.tokens = (st.tokens + elapsed.as_secs_f64() * f64::from(RATE_REFILL_PER_SEC))
            .min(f64::from(RATE_BURST));
        if st.tokens < 1.0 {
            return;
        }
        st.tokens -= 1.0;
        if self.out.client_attached() {
            if st.deferred.len() < MAX_DEFERRED {
                st.deferred.push(reply);
                self.has_deferred.store(true, Ordering::Release);
            }
        } else {
            drop(st);
            self.out.write_reply(&reply);
        }
    }

    /// Called after the chunk was broadcast. If no client received it (the last one detached
    /// between the query being parsed and the send), nobody answered: answer now. Otherwise the
    /// client's emulator answered and the parked replies are discarded.
    pub(crate) fn settle(&self, delivered_to_client: bool) {
        if !self.has_deferred.swap(false, Ordering::AcqRel) {
            return;
        }
        let parked = std::mem::take(&mut self.state.lock().unwrap().deferred);
        if !delivered_to_client {
            for reply in parked {
                self.out.write_reply(&reply);
            }
        }
    }
}

/// vt100 callbacks that turn unhandled query sequences into replies.
pub(crate) struct QueryResponder {
    replies: Arc<QueryReplies>,
}

impl QueryResponder {
    pub(crate) fn new(replies: Arc<QueryReplies>) -> Self {
        Self { replies }
    }
}

impl Callbacks for QueryResponder {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let Ok(fin) = u8::try_from(c) else { return };
        if let Some(query) = classify_csi(i1, i2, p0, fin) {
            self.replies.offer(reply_for(query, screen), Instant::now());
        }
    }

    fn unhandled_osc(&mut self, _: &mut Screen, params: &[&[u8]]) {
        for reply in osc_replies(params) {
            self.replies.offer(reply, Instant::now());
        }
    }
}

/// Writes replies to the pty master, never blocking the pane's reader thread: a child that
/// floods queries without reading its input would otherwise fill the pty input buffer and
/// deadlock (the reader thread is also what drains the child's output).
pub(crate) struct PtyReplyOut {
    pub(crate) tx: tokio::sync::broadcast::Sender<(u64, Vec<u8>)>,
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(crate) fd: Option<i32>,
}

impl ReplyOut for PtyReplyOut {
    fn client_attached(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    fn write_reply(&self, bytes: &[u8]) {
        // try_lock: if an input write holds the writer it may be blocked on a full pty, and
        // waiting here would stall output draining.
        let Ok(mut writer) = self.writer.try_lock() else {
            return;
        };
        #[cfg(unix)]
        if let Some(fd) = self.fd {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid pollfd and the count matches.
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready <= 0 || pfd.revents & libc::POLLOUT == 0 {
                return;
            }
        }
        let _ = self.fd;
        if writer.write_all(bytes).is_ok() {
            let _ = writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        attached: AtomicBool,
        written: Mutex<Vec<Vec<u8>>>,
    }

    struct RecorderOut(Arc<Recorder>);

    impl ReplyOut for RecorderOut {
        fn client_attached(&self) -> bool {
            self.0.attached.load(Ordering::SeqCst)
        }
        fn write_reply(&self, bytes: &[u8]) {
            self.0.written.lock().unwrap().push(bytes.to_vec());
        }
    }

    fn harness(
        attached: bool,
    ) -> (
        vt100::Parser<QueryResponder>,
        Arc<Recorder>,
        Arc<QueryReplies>,
    ) {
        let rec = Arc::new(Recorder::default());
        rec.attached.store(attached, Ordering::SeqCst);
        let replies = Arc::new(QueryReplies::new(Box::new(RecorderOut(rec.clone()))));
        let parser =
            vt100::Parser::new_with_callbacks(10, 40, 0, QueryResponder::new(replies.clone()));
        (parser, rec, replies)
    }

    fn replies_to(input: &[u8]) -> Vec<String> {
        let (mut parser, rec, _) = harness(false);
        parser.process(input);
        let written = rec.written.lock().unwrap();
        written
            .iter()
            .map(|b| String::from_utf8_lossy(b).replace('\x1b', "ESC"))
            .collect()
    }

    #[test]
    fn da1_should_report_vt100_with_advanced_video() {
        assert_eq!(replies_to(b"\x1b[c"), ["ESC[?1;2c"]);
        assert_eq!(replies_to(b"\x1b[0c"), ["ESC[?1;2c"]);
    }

    #[test]
    fn da2_should_report_secondary_attributes() {
        assert_eq!(replies_to(b"\x1b[>c"), ["ESC[>0;276;0c"]);
    }

    #[test]
    fn dsr_5_should_report_ok() {
        assert_eq!(replies_to(b"\x1b[5n"), ["ESC[0n"]);
    }

    #[test]
    fn dsr_6_should_report_the_real_cursor_position() {
        assert_eq!(replies_to(b"\x1b[6n"), ["ESC[1;1R"]);
        assert_eq!(replies_to(b"ab\r\n  xyz\x1b[6n"), ["ESC[2;6R"]);
        assert_eq!(replies_to(b"\x1b[?6n"), ["ESC[?1;1;1R"]);
    }

    #[test]
    fn dsr_6_should_be_relative_to_the_scroll_region_under_origin_mode() {
        // region rows 3..8, DECOM on, cursor to region row 2 col 4
        assert_eq!(
            replies_to(b"\x1b[3;8r\x1b[?6h\x1b[2;4H\x1b[6n"),
            ["ESC[2;4R"]
        );
    }

    #[test]
    fn xtversion_should_report_tymux() {
        let got = replies_to(b"\x1b[>q");
        assert_eq!(got.len(), 1);
        assert!(
            got[0].starts_with("ESCP>|tymux ") && got[0].ends_with("ESC\\"),
            "{got:?}"
        );
        assert_eq!(replies_to(b"\x1b[>0q"), got);
    }

    #[test]
    fn decrqm_should_report_set_reset_and_unknown_modes() {
        assert_eq!(
            replies_to(
                b"\x1b[?25$p\x1b[?25l\x1b[?25$p\x1b[?2004$p\x1b[?2004h\x1b[?2004$p\x1b[?9999$p"
            ),
            [
                "ESC[?25;1$y",
                "ESC[?25;2$y",
                "ESC[?2004;2$y",
                "ESC[?2004;1$y",
                "ESC[?9999;0$y"
            ]
        );
        assert_eq!(
            replies_to(b"\x1b[4$p\x1b[4h\x1b[4$p"),
            ["ESC[4;2$y", "ESC[4;1$y"]
        );
        assert_eq!(
            replies_to(b"\x1b[?1000h\x1b[?1006h\x1b[?1000$p\x1b[?1006$p\x1b[?1003$p"),
            ["ESC[?1000;1$y", "ESC[?1006;1$y", "ESC[?1003;2$y"]
        );
    }

    #[test]
    fn xtwinops_should_report_the_size_in_characters() {
        assert_eq!(replies_to(b"\x1b[18t"), ["ESC[8;10;40t"]);
        assert_eq!(replies_to(b"\x1b[19t"), ["ESC[9;10;40t"]);
    }

    #[test]
    fn osc_color_queries_should_report_the_default_palette() {
        assert_eq!(
            replies_to(b"\x1b]10;?\x07"),
            ["ESC]10;rgb:ffff/ffff/ffffESC\\"]
        );
        assert_eq!(
            replies_to(b"\x1b]11;?\x1b\\"),
            ["ESC]11;rgb:0000/0000/0000ESC\\"]
        );
        assert_eq!(
            replies_to(b"\x1b]12;?\x07"),
            ["ESC]12;rgb:ffff/ffff/ffffESC\\"]
        );
        assert_eq!(
            replies_to(b"\x1b]4;1;?\x07"),
            ["ESC]4;1;rgb:cccc/0000/0000ESC\\"]
        );
        assert_eq!(
            replies_to(b"\x1b]4;232;?\x07"),
            ["ESC]4;232;rgb:0808/0808/0808ESC\\"]
        );
        assert_eq!(
            replies_to(b"\x1b]4;196;?\x07"),
            ["ESC]4;196;rgb:ffff/0000/0000ESC\\"]
        );
    }

    #[test]
    fn osc_setting_a_color_should_not_be_answered() {
        assert!(replies_to(b"\x1b]10;#ff0000\x07\x1b]4;1;#00ff00\x07").is_empty());
    }

    #[test]
    fn kitty_keyboard_query_should_not_be_answered() {
        assert!(replies_to(b"\x1b[?u").is_empty());
    }

    #[test]
    fn non_query_sequences_should_not_be_answered() {
        assert!(replies_to(b"\x1b[2J\x1b[1;31mhi\x1b[0m\x1b[8;10;40t\x1b[>1c\x1b[7n").is_empty());
    }

    #[test]
    fn a_query_split_across_chunks_should_be_answered_exactly_once() {
        let (mut parser, rec, _) = harness(false);
        parser.process(b"\x1b[");
        parser.process(b"6");
        parser.process(b"n");
        assert_eq!(rec.written.lock().unwrap().len(), 1);
    }

    #[test]
    fn replies_should_be_silent_while_a_client_is_attached() {
        let (mut parser, rec, replies) = harness(true);
        parser.process(b"\x1b[6n\x1b[c");
        replies.settle(true);
        assert!(rec.written.lock().unwrap().is_empty());
    }

    #[test]
    fn replies_should_be_sent_when_the_last_client_detached_before_the_chunk_was_delivered() {
        let (mut parser, rec, replies) = harness(true);
        parser.process(b"\x1b[6n");
        rec.attached.store(false, Ordering::SeqCst);
        replies.settle(false);
        assert_eq!(rec.written.lock().unwrap().len(), 1);
        // and only once: settle clears what it parked
        replies.settle(false);
        assert_eq!(rec.written.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_reply_flood_should_be_rate_limited() {
        let (mut parser, rec, _) = harness(false);
        parser.process("\x1b[5n".repeat(1000).as_bytes());
        let sent = rec.written.lock().unwrap().len();
        assert!((1..=RATE_BURST as usize + 2).contains(&sent), "sent {sent}");
    }

    #[test]
    fn the_rate_limit_should_refill_over_time() {
        let rec = Arc::new(Recorder::default());
        let replies = QueryReplies::new(Box::new(RecorderOut(rec.clone())));
        let t0 = Instant::now();
        for _ in 0..RATE_BURST + 5 {
            replies.offer(b"x".to_vec(), t0);
        }
        assert_eq!(rec.written.lock().unwrap().len(), RATE_BURST as usize);
        replies.offer(b"x".to_vec(), t0 + Duration::from_secs(1));
        assert_eq!(rec.written.lock().unwrap().len(), RATE_BURST as usize + 1);
    }

    #[test]
    fn an_osc4_query_should_answer_a_bounded_number_of_pairs() {
        let mut q = String::from("\x1b]4");
        for i in 0..40 {
            q.push_str(&format!(";{i};?"));
        }
        q.push('\x07');
        assert!(replies_to(q.as_bytes()).len() <= MAX_OSC4_PAIRS);
    }

    #[test]
    fn strip_queries_should_remove_whole_queries_and_keep_everything_else() {
        let input = b"hi\x1b[6nthere\x1b[1;31mred\x1b[c\x1b]11;?\x07end\x1b[?2004$p";
        assert_eq!(&*strip_queries(input), b"hithere\x1b[1;31mredend" as &[u8]);
        assert_eq!(
            &*strip_queries(b"\x1b[?u\x1b]10;#fff\x07"),
            b"\x1b[?u\x1b]10;#fff\x07" as &[u8]
        );
        assert!(matches!(strip_queries(b"plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn strip_queries_should_leave_a_split_query_intact() {
        assert_eq!(&*strip_queries(b"abc\x1b[6"), b"abc\x1b[6" as &[u8]);
        assert_eq!(&*strip_queries(b"6nabc"), b"6nabc" as &[u8]);
    }

    #[test]
    fn strip_and_reply_should_agree_on_what_is_a_query() {
        for seq in [
            &b"\x1b[c"[..],
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[5n",
            b"\x1b[6n",
            b"\x1b[?6n",
            b"\x1b[>q",
            b"\x1b[4$p",
            b"\x1b[?25$p",
            b"\x1b[18t",
            b"\x1b]10;?\x07",
            b"\x1b]4;3;?\x1b\\",
            b"\x1b[7n",
            b"\x1b[?u",
            b"\x1b[8;5;5t",
        ] {
            let answered = !replies_to(seq).is_empty();
            let stripped = strip_queries(seq).is_empty();
            assert_eq!(answered, stripped, "{:?}", String::from_utf8_lossy(seq));
        }
    }
}
