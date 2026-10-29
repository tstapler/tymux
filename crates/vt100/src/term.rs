// TODO: read all of this from terminfo

pub trait BufWrite {
    fn write_buf(&self, buf: &mut Vec<u8>);
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct ClearScreen;

impl BufWrite for ClearScreen {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\x1b[H\x1b[J");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct ClearRowForward;

impl BufWrite for ClearRowForward {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\x1b[K");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct Crlf;

impl BufWrite for Crlf {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\r\n");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct Backspace;

impl BufWrite for Backspace {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\x08");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct SaveCursor;

impl BufWrite for SaveCursor {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\x1b7");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct RestoreCursor;

impl BufWrite for RestoreCursor {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\x1b8");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct MoveTo {
    row: u16,
    col: u16,
}

impl MoveTo {
    pub fn new(pos: crate::grid::Pos) -> Self {
        Self {
            row: pos.row,
            col: pos.col,
        }
    }
}

impl BufWrite for MoveTo {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.row == 0 && self.col == 0 {
            buf.extend_from_slice(b"\x1b[H");
        } else {
            buf.extend_from_slice(b"\x1b[");
            extend_itoa(buf, self.row + 1);
            buf.push(b';');
            extend_itoa(buf, self.col + 1);
            buf.push(b'H');
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct ClearAttrs;

impl BufWrite for ClearAttrs {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(b"\x1b[m");
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct Attrs {
    fgcolor: Option<crate::Color>,
    bgcolor: Option<crate::Color>,
    ulcolor: Option<crate::Color>,
    // (bold, dim) wanted after this diff; always emitted as 22 then 1/2.
    intensity: Option<(bool, bool)>,
    italic: Option<bool>,
    underline: Option<Option<crate::UnderlineStyle>>,
    inverse: Option<bool>,
    strikethrough: Option<bool>,
    blink: Option<bool>,
    conceal: Option<bool>,
    overline: Option<bool>,
}

impl Attrs {
    pub fn fgcolor(mut self, fgcolor: crate::Color) -> Self {
        self.fgcolor = Some(fgcolor);
        self
    }

    pub fn bgcolor(mut self, bgcolor: crate::Color) -> Self {
        self.bgcolor = Some(bgcolor);
        self
    }

    pub fn ulcolor(mut self, ulcolor: crate::Color) -> Self {
        self.ulcolor = Some(ulcolor);
        self
    }

    pub fn intensity(mut self, bold: bool, dim: bool) -> Self {
        self.intensity = Some((bold, dim));
        self
    }

    pub fn italic(mut self, italic: bool) -> Self {
        self.italic = Some(italic);
        self
    }

    pub fn underline(mut self, underline: Option<crate::UnderlineStyle>) -> Self {
        self.underline = Some(underline);
        self
    }

    pub fn inverse(mut self, inverse: bool) -> Self {
        self.inverse = Some(inverse);
        self
    }

    pub fn strikethrough(mut self, on: bool) -> Self {
        self.strikethrough = Some(on);
        self
    }

    pub fn blink(mut self, on: bool) -> Self {
        self.blink = Some(on);
        self
    }

    pub fn conceal(mut self, on: bool) -> Self {
        self.conceal = Some(on);
        self
    }

    pub fn overline(mut self, on: bool) -> Self {
        self.overline = Some(on);
        self
    }
}

impl BufWrite for Attrs {
    #[allow(unused_assignments)]
    #[allow(clippy::branches_sharing_code)]
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.fgcolor.is_none()
            && self.bgcolor.is_none()
            && self.ulcolor.is_none()
            && self.intensity.is_none()
            && self.italic.is_none()
            && self.underline.is_none()
            && self.inverse.is_none()
            && self.strikethrough.is_none()
            && self.blink.is_none()
            && self.conceal.is_none()
            && self.overline.is_none()
        {
            return;
        }

        buf.extend_from_slice(b"\x1b[");
        let mut first = true;

        macro_rules! write_param {
            ($i:expr) => {{
                if first {
                    first = false;
                } else {
                    buf.push(b';');
                }
                extend_itoa(buf, $i);
            }};
        }

        macro_rules! write_color {
            ($color:expr, $default:expr, $lo:expr, $hi:expr, $ext:expr) => {
                match $color {
                    crate::Color::Default => {
                        write_param!($default);
                    }
                    crate::Color::Idx(i) => {
                        if $lo != 0 && i < 8 {
                            write_param!(i + $lo);
                        } else if $lo != 0 && i < 16 {
                            write_param!(i + $hi);
                        } else {
                            write_param!($ext);
                            write_param!(5);
                            write_param!(i);
                        }
                    }
                    crate::Color::Rgb(r, g, b) => {
                        write_param!($ext);
                        write_param!(2);
                        write_param!(r);
                        write_param!(g);
                        write_param!(b);
                    }
                }
            };
        }

        if let Some(fgcolor) = self.fgcolor {
            write_color!(fgcolor, 39, 30, 82, 38);
        }
        if let Some(bgcolor) = self.bgcolor {
            write_color!(bgcolor, 49, 40, 92, 48);
        }
        if let Some(ulcolor) = self.ulcolor {
            write_color!(ulcolor, 59, 0, 0, 58);
        }

        if let Some((bold, dim)) = self.intensity {
            write_param!(22);
            if bold {
                write_param!(1);
            }
            if dim {
                write_param!(2);
            }
        }

        if let Some(italic) = self.italic {
            write_param!(if italic { 3 } else { 23 });
        }

        if let Some(underline) = self.underline {
            // colon sub-parameters need their own separator, so write the
            // style as one `4:n` parameter by hand
            if first {
                first = false;
            } else {
                buf.push(b';');
            }
            match underline {
                None => buf.extend_from_slice(b"24"),
                Some(crate::UnderlineStyle::Single) => buf.push(b'4'),
                Some(crate::UnderlineStyle::Double) => {
                    buf.extend_from_slice(b"4:2");
                }
                Some(crate::UnderlineStyle::Curly) => {
                    buf.extend_from_slice(b"4:3");
                }
                Some(crate::UnderlineStyle::Dotted) => {
                    buf.extend_from_slice(b"4:4");
                }
                Some(crate::UnderlineStyle::Dashed) => {
                    buf.extend_from_slice(b"4:5");
                }
            }
        }

        if let Some(inverse) = self.inverse {
            write_param!(if inverse { 7 } else { 27 });
        }
        if let Some(on) = self.strikethrough {
            write_param!(if on { 9 } else { 29 });
        }
        if let Some(on) = self.blink {
            write_param!(if on { 5 } else { 25 });
        }
        if let Some(on) = self.conceal {
            write_param!(if on { 8 } else { 28 });
        }
        if let Some(on) = self.overline {
            write_param!(if on { 53 } else { 55 });
        }

        buf.push(b'm');
    }
}

#[derive(Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct MoveRight {
    count: u16,
}

impl MoveRight {
    pub fn new(count: u16) -> Self {
        Self { count }
    }
}

impl Default for MoveRight {
    fn default() -> Self {
        Self { count: 1 }
    }
}

impl BufWrite for MoveRight {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        match self.count {
            0 => {}
            1 => buf.extend_from_slice(b"\x1b[C"),
            n => {
                buf.extend_from_slice(b"\x1b[");
                extend_itoa(buf, n);
                buf.push(b'C');
            }
        }
    }
}

#[derive(Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct EraseChar {
    count: u16,
}

impl EraseChar {
    pub fn new(count: u16) -> Self {
        Self { count }
    }
}

impl Default for EraseChar {
    fn default() -> Self {
        Self { count: 1 }
    }
}

impl BufWrite for EraseChar {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        match self.count {
            0 => {}
            1 => buf.extend_from_slice(b"\x1b[X"),
            n => {
                buf.extend_from_slice(b"\x1b[");
                extend_itoa(buf, n);
                buf.push(b'X');
            }
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct HideCursor {
    state: bool,
}

impl HideCursor {
    pub fn new(state: bool) -> Self {
        Self { state }
    }
}

impl BufWrite for HideCursor {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.state {
            buf.extend_from_slice(b"\x1b[?25l");
        } else {
            buf.extend_from_slice(b"\x1b[?25h");
        }
    }
}

#[derive(Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct MoveFromTo {
    from: crate::grid::Pos,
    to: crate::grid::Pos,
}

impl MoveFromTo {
    pub fn new(from: crate::grid::Pos, to: crate::grid::Pos) -> Self {
        Self { from, to }
    }
}

impl BufWrite for MoveFromTo {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.to.row == self.from.row + 1 && self.to.col == 0 {
            crate::term::Crlf.write_buf(buf);
        } else if self.from.row == self.to.row && self.from.col < self.to.col {
            crate::term::MoveRight::new(self.to.col - self.from.col).write_buf(buf);
        } else if self.to != self.from {
            crate::term::MoveTo::new(self.to).write_buf(buf);
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct ApplicationKeypad {
    state: bool,
}

impl ApplicationKeypad {
    pub fn new(state: bool) -> Self {
        Self { state }
    }
}

impl BufWrite for ApplicationKeypad {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.state {
            buf.extend_from_slice(b"\x1b=");
        } else {
            buf.extend_from_slice(b"\x1b>");
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct ApplicationCursor {
    state: bool,
}

impl ApplicationCursor {
    pub fn new(state: bool) -> Self {
        Self { state }
    }
}

impl BufWrite for ApplicationCursor {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.state {
            buf.extend_from_slice(b"\x1b[?1h");
        } else {
            buf.extend_from_slice(b"\x1b[?1l");
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct BracketedPaste {
    state: bool,
}

impl BracketedPaste {
    pub fn new(state: bool) -> Self {
        Self { state }
    }
}

impl BufWrite for BracketedPaste {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.state {
            buf.extend_from_slice(b"\x1b[?2004h");
        } else {
            buf.extend_from_slice(b"\x1b[?2004l");
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct MouseProtocolMode {
    mode: crate::MouseProtocolMode,
    prev: crate::MouseProtocolMode,
}

impl MouseProtocolMode {
    pub fn new(mode: crate::MouseProtocolMode, prev: crate::MouseProtocolMode) -> Self {
        Self { mode, prev }
    }
}

impl BufWrite for MouseProtocolMode {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.mode == self.prev {
            return;
        }

        match self.mode {
            crate::MouseProtocolMode::None => match self.prev {
                crate::MouseProtocolMode::None => {}
                crate::MouseProtocolMode::Press => {
                    buf.extend_from_slice(b"\x1b[?9l");
                }
                crate::MouseProtocolMode::PressRelease => {
                    buf.extend_from_slice(b"\x1b[?1000l");
                }
                crate::MouseProtocolMode::ButtonMotion => {
                    buf.extend_from_slice(b"\x1b[?1002l");
                }
                crate::MouseProtocolMode::AnyMotion => {
                    buf.extend_from_slice(b"\x1b[?1003l");
                }
            },
            crate::MouseProtocolMode::Press => {
                buf.extend_from_slice(b"\x1b[?9h");
            }
            crate::MouseProtocolMode::PressRelease => {
                buf.extend_from_slice(b"\x1b[?1000h");
            }
            crate::MouseProtocolMode::ButtonMotion => {
                buf.extend_from_slice(b"\x1b[?1002h");
            }
            crate::MouseProtocolMode::AnyMotion => {
                buf.extend_from_slice(b"\x1b[?1003h");
            }
        }
    }
}

#[derive(Default, Debug)]
#[must_use = "this struct does nothing unless you call write_buf"]
pub struct MouseProtocolEncoding {
    encoding: crate::MouseProtocolEncoding,
    prev: crate::MouseProtocolEncoding,
}

impl MouseProtocolEncoding {
    pub fn new(encoding: crate::MouseProtocolEncoding, prev: crate::MouseProtocolEncoding) -> Self {
        Self { encoding, prev }
    }
}

impl BufWrite for MouseProtocolEncoding {
    fn write_buf(&self, buf: &mut Vec<u8>) {
        if self.encoding == self.prev {
            return;
        }

        match self.encoding {
            crate::MouseProtocolEncoding::Default => match self.prev {
                crate::MouseProtocolEncoding::Default => {}
                crate::MouseProtocolEncoding::Utf8 => {
                    buf.extend_from_slice(b"\x1b[?1005l");
                }
                crate::MouseProtocolEncoding::Sgr => {
                    buf.extend_from_slice(b"\x1b[?1006l");
                }
                crate::MouseProtocolEncoding::Urxvt => {
                    buf.extend_from_slice(b"\x1b[?1015l");
                }
            },
            crate::MouseProtocolEncoding::Utf8 => {
                buf.extend_from_slice(b"\x1b[?1005h");
            }
            crate::MouseProtocolEncoding::Sgr => {
                buf.extend_from_slice(b"\x1b[?1006h");
            }
            crate::MouseProtocolEncoding::Urxvt => {
                buf.extend_from_slice(b"\x1b[?1015h");
            }
        }
    }
}

fn extend_itoa<I: itoa::Integer>(buf: &mut Vec<u8>, i: I) {
    let mut itoa_buf = itoa::Buffer::new();
    buf.extend_from_slice(itoa_buf.format(i).as_bytes());
}
