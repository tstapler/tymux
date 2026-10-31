//! Answers terminal queries (DA, DSR, DECRQM, XTVERSION, OSC colour queries, XTWINOPS size) on
//! behalf of a pane when no client's emulator will.
//!
//! Normally the attached client's emulator (xterm.js) answers queries it sees in the raw byte
//! stream. With nobody attached a full-screen app that starts up and waits for a reply (vim, fzf,
//! Claude Code) would hang, so the daemon answers instead. The daemon never answers a query a
//! registered client will see (with several clients attached each one's emulator answers, as it
//! always did; that is not new). Per chunk:
//!
//! * For each output chunk the pane decides once, before parsing it, whether any registered
//!   client ([`LiveClient`]) will see it live. If not, the daemon answers the queries in it and
//!   every client receives the chunk with those queries stripped.
//! * Otherwise the chunk goes out raw and the client answers.
//! * Replayed history never contains queries.
//!
//! Deliberately not answered: the kitty keyboard query (`CSI ? u`). Replying "supported" would make
//! the app enable the protocol, and the xterm.js client that attaches later sends legacy keys.
//! Apps probe it together with DA1, so the DA1 reply alone resolves their wait.

mod clients;
mod outbox;
mod scanner;

#[cfg(not(unix))]
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(not(unix))]
use std::sync::Mutex;
use std::time::Instant;

use vt100::{Callbacks, MouseProtocolEncoding, MouseProtocolMode, Screen};

pub(crate) use clients::ClientRegistry;
pub use clients::LiveClient;
pub(crate) use outbox::{Outbox, ReplyWriter};
pub(crate) use scanner::QueryScanner;

/// vte keeps at most this many OSC parameters (`MAX_OSC_PARAMS` in vte 0.15), so an `OSC 4` query
/// is answered for at most 7 `index;?` pairs; the rest of a longer sequence is lost to the parser.
const VTE_MAX_OSC_PARAMS: usize = 16;

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
/// parameter and the final byte. The emulator callbacks and the stream scanner both call this, so
/// what the daemon answers and what gets stripped cannot drift apart.
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

/// DECRQM status values (xterm ctlseqs).
const NOT_RECOGNIZED: u8 = 0;
const SET: u8 = 1;
const RESET: u8 = 2;
const PERMANENTLY_SET: u8 = 3;
const PERMANENTLY_RESET: u8 = 4;

fn flag(on: bool) -> u8 {
    if on {
        SET
    } else {
        RESET
    }
}

/// Mirrors xterm.js's `requestMode` (the browser client's answer), limited to modes vt100 tracks.
fn mode_status(screen: &Screen, dec: bool, mode: u16) -> u8 {
    let mouse = screen.mouse_protocol_mode();
    let encoding = screen.mouse_protocol_encoding();
    if !dec {
        return match mode {
            2 => PERMANENTLY_RESET,
            4 => flag(screen.insert_mode()),
            12 => PERMANENTLY_SET,
            20 => flag(screen.newline_mode()),
            _ => NOT_RECOGNIZED,
        };
    }
    match mode {
        1 => flag(screen.application_cursor()),
        6 => flag(screen.origin_mode()),
        7 => flag(screen.autowrap()),
        8 => PERMANENTLY_SET,
        9 => flag(mouse == MouseProtocolMode::Press),
        25 => flag(!screen.hide_cursor()),
        47 | 1047 | 1049 => flag(screen.alternate_screen()),
        66 => flag(screen.application_keypad()),
        67 | 1005 | 1015 => PERMANENTLY_RESET,
        1000 => flag(mouse == MouseProtocolMode::PressRelease),
        1002 => flag(mouse == MouseProtocolMode::ButtonMotion),
        1003 => flag(mouse == MouseProtocolMode::AnyMotion),
        1004 => flag(screen.focus_events()),
        1006 => flag(encoding == MouseProtocolEncoding::Sgr),
        1048 => SET,
        2004 => flag(screen.bracketed_paste()),
        _ => NOT_RECOGNIZED,
    }
}

/// 1-based cursor row/col as the app sees it: relative to the scroll region under DECOM, and
/// clamped to the screen (a cursor with a pending wrap sits one past the last column).
fn cursor_report(screen: &Screen) -> (u32, u32) {
    let (row, col) = screen.cursor_position();
    let (rows, cols) = screen.size();
    let row = if screen.origin_mode() {
        // scroll_region() is 1-based, (0, 0) for the whole screen
        row.saturating_sub(screen.scroll_region().0.saturating_sub(1))
    } else {
        row
    };
    (
        u32::from(row.saturating_add(1).min(rows)),
        u32::from(col.saturating_add(1).min(cols)),
    )
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

type Rgb = (u8, u8, u8);

/// Colours reported for `OSC 10/11/12` queries. Defaults match xterm.js's default (dark) theme;
/// override per daemon with `TYMUXD_QUERY_FG`, `TYMUXD_QUERY_BG` and `TYMUXD_QUERY_CURSOR`
/// (`#rrggbb`) when the clients use a different theme, e.g. a light one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Colors {
    fg: Rgb,
    bg: Rgb,
    cursor: Rgb,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            fg: (0xff, 0xff, 0xff),
            bg: (0x00, 0x00, 0x00),
            cursor: (0xff, 0xff, 0xff),
        }
    }
}

fn parse_hex_color(s: &str) -> Option<Rgb> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

impl Colors {
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let defaults = Self::default();
        let pick = |key: &str, default: Rgb| {
            get(key)
                .and_then(|v| parse_hex_color(&v))
                .unwrap_or(default)
        };
        Self {
            fg: pick("TYMUXD_QUERY_FG", defaults.fg),
            bg: pick("TYMUXD_QUERY_BG", defaults.bg),
            cursor: pick("TYMUXD_QUERY_CURSOR", defaults.cursor),
        }
    }
}

/// xterm.js's default ANSI palette.
const ANSI16: [Rgb; 16] = [
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

fn palette(index: u8) -> Rgb {
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

fn osc_color(prefix: &str, (r, g, b): Rgb, bel: bool) -> Vec<u8> {
    let end = if bel { "\x07" } else { "\x1b\\" };
    format!("\x1b]{prefix};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}{end}").into_bytes()
}

/// One colour asked for by an OSC query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscItem {
    /// `OSC 10|11|12`: foreground, background or cursor colour.
    Dynamic(u8),
    /// `OSC 4`: palette entry.
    Palette(u8),
}

/// The colours a pure-query OSC asks for, or `None` if it is not one. A sequence mixing queries
/// with colour *sets* (`OSC 4;1;rgb:..;2;?`) is not treated as a query: the daemon does not answer
/// it and the stream scanner leaves it intact, so no client misses the set. Queries for the
/// dynamic colours (`OSC 10;?;?`: each further `?` is the next colour, as in xterm) and for
/// palette entries (`OSC 4;i;?[;j;?]...`, as many pairs as vte's parameter limit kept) are recognised.
fn osc_query(params: &[&[u8]]) -> Option<Vec<OscItem>> {
    let (&first, rest) = params.split_first()?;
    let base: u8 = match first {
        b"10" => 10,
        b"11" => 11,
        b"12" => 12,
        b"4" => {
            // A full parameter list was cut off by vte, so its odd trailing element is half a pair.
            let truncated = params.len() >= VTE_MAX_OSC_PARAMS;
            let rest = if truncated && rest.len() % 2 == 1 {
                &rest[..rest.len() - 1]
            } else {
                rest
            };
            if rest.is_empty() || rest.len() % 2 != 0 {
                return None;
            }
            return rest
                .chunks(2)
                .map(|pair| {
                    let index = std::str::from_utf8(pair[0]).ok()?.parse().ok()?;
                    (pair[1] == b"?").then_some(OscItem::Palette(index))
                })
                .collect();
        }
        _ => return None,
    };
    if rest.is_empty() || rest.iter().any(|p| *p != b"?") {
        return None;
    }
    Some(
        (base..=12)
            .zip(rest)
            .map(|(ident, _)| OscItem::Dynamic(ident))
            .collect(),
    )
}

/// Replies to a pure OSC colour query, ending with the same terminator (`bel`) as the query.
fn osc_replies(params: &[&[u8]], bel: bool, colors: &Colors) -> Vec<Vec<u8>> {
    osc_query(params)
        .unwrap_or_default()
        .into_iter()
        .map(|item| match item {
            OscItem::Palette(i) => osc_color(&format!("4;{i}"), palette(i), bel),
            OscItem::Dynamic(ident) => {
                let color = match ident {
                    10 => colors.fg,
                    11 => colors.bg,
                    _ => colors.cursor,
                };
                osc_color(&ident.to_string(), color, bel)
            }
        })
        .collect()
}

/// The reply to a CSI the emulator did not handle itself, if it is one of our queries.
fn csi_reply(
    screen: &Screen,
    i1: Option<u8>,
    i2: Option<u8>,
    params: &[&[u16]],
    c: char,
) -> Option<Vec<u8>> {
    let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
    let fin = u8::try_from(c).ok()?;
    classify_csi(i1, i2, p0, fin).map(|query| reply_for(query, screen))
}

/// Shared between the parser's callbacks and the pane's reader thread.
pub(crate) struct QueryReplies {
    /// Set by the reader thread before each chunk is parsed (see the module docs).
    daemon_answers: AtomicBool,
    colors: Colors,
    outbox: Outbox,
}

impl QueryReplies {
    pub(crate) fn new(writer: Box<dyn ReplyWriter>, colors: Colors) -> Self {
        Self {
            daemon_answers: AtomicBool::new(true),
            colors,
            outbox: Outbox::new(writer),
        }
    }

    /// Replies accepted for sending so far (tests assert silence without waiting on a timeout).
    #[cfg(test)]
    pub(crate) fn accepted(&self) -> u64 {
        self.outbox.accepted()
    }

    pub(crate) fn set_daemon_answers(&self, on: bool) {
        self.daemon_answers.store(on, Ordering::Release);
    }

    /// Stops the writer thread; called when the pty closes.
    pub(crate) fn close(&self) {
        self.outbox.close();
    }

    fn offer(&self, reply: Vec<u8>) {
        if self.daemon_answers.load(Ordering::Acquire) {
            self.outbox.offer(reply, Instant::now());
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
        if let Some(reply) = csi_reply(screen, i1, i2, params, c) {
            self.replies.offer(reply);
        }
    }

    fn unhandled_osc_terminated(&mut self, _: &mut Screen, params: &[&[u8]], bel: bool) {
        for reply in osc_replies(params, bel, &self.replies.colors) {
            self.replies.offer(reply);
        }
    }
}

/// Writes replies to the pty master from the outbox's writer thread, through its own duplicate of
/// the master fd so a reply never waits on (or holds up) the pane's input writer. Each write waits
/// for the pty to accept data for at most `WRITE_WAIT`, then drops the reply: a child that never
/// reads stdin cannot pin the thread, or Ctrl-C queued behind it.
pub(crate) struct PtyReplyWriter {
    #[cfg(unix)]
    pub(crate) fd: std::os::fd::OwnedFd,
    #[cfg(not(unix))]
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

#[cfg(unix)]
const WRITE_WAIT_MS: libc::c_int = 250;

impl ReplyWriter for PtyReplyWriter {
    #[cfg(unix)]
    fn write_reply(&mut self, mut bytes: &[u8]) {
        use std::os::fd::AsRawFd;
        let fd = self.fd.as_raw_fd();
        while !bytes.is_empty() {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid pollfd and the count matches.
            let ready = unsafe { libc::poll(&mut pfd, 1, WRITE_WAIT_MS) };
            if ready <= 0 || pfd.revents & libc::POLLOUT == 0 {
                return;
            }
            // SAFETY: `bytes` is a valid readable slice; `fd` is owned by `self`.
            let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if n <= 0 {
                return;
            }
            bytes = &bytes[n as usize..];
        }
    }

    #[cfg(not(unix))]
    fn write_reply(&mut self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        if writer.write_all(bytes).is_ok() {
            let _ = writer.flush();
        }
    }
}

#[cfg(test)]
mod tests;
