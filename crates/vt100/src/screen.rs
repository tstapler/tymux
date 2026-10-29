use crate::term::BufWrite as _;
use unicode_width::UnicodeWidthChar as _;

const MAX_HYPERLINKS: usize = 4096;

const MODE_APPLICATION_KEYPAD: u8 = 0b0000_0001;
const MODE_APPLICATION_CURSOR: u8 = 0b0000_0010;
const MODE_HIDE_CURSOR: u8 = 0b0000_0100;
const MODE_ALTERNATE_SCREEN: u8 = 0b0000_1000;
const MODE_BRACKETED_PASTE: u8 = 0b0001_0000;
// ANSI mode 4 (IRM), ANSI mode 20 (LNM), and DECAWM inverted so a fresh screen wraps by default
const MODE_INSERT: u8 = 0b0010_0000;
const MODE_NEWLINE: u8 = 0b0100_0000;
const MODE_NO_AUTOWRAP: u8 = 0b1000_0000;

/// The xterm mouse handling mode currently in use.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum MouseProtocolMode {
    /// Mouse handling is disabled.
    #[default]
    None,

    /// Mouse button events should be reported on button press. Also known as
    /// X10 mouse mode.
    Press,

    /// Mouse button events should be reported on button press and release.
    /// Also known as VT200 mouse mode.
    PressRelease,

    // Highlight,
    /// Mouse button events should be reported on button press and release, as
    /// well as when the mouse moves between cells while a button is held
    /// down.
    ButtonMotion,

    /// Mouse button events should be reported on button press and release,
    /// and mouse motion events should be reported when the mouse moves
    /// between cells regardless of whether a button is held down or not.
    AnyMotion,
    // DecLocator,
}

/// The encoding to use for the enabled [`MouseProtocolMode`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum MouseProtocolEncoding {
    /// Default single-printable-byte encoding.
    #[default]
    Default,

    /// UTF-8-based encoding.
    Utf8,

    /// SGR-like encoding.
    Sgr,
    // Urxvt,
}

/// A designated 94-character set (SCS; xterm ctlseqs, "Controls beginning with ESC").
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
enum Charset {
    #[default]
    Ascii,
    DecSpecial,
    Uk,
}

impl Charset {
    fn from_final(b: u8) -> Option<Self> {
        match b {
            b'B' => Some(Self::Ascii),
            b'0' => Some(Self::DecSpecial),
            b'A' => Some(Self::Uk),
            _ => None,
        }
    }

    fn map(self, c: char) -> char {
        match self {
            Self::Ascii => c,
            Self::Uk => {
                if c == '#' {
                    '\u{a3}'
                } else {
                    c
                }
            }
            Self::DecSpecial => dec_special_graphics(c),
        }
    }
}

// xterm ctlseqs, DEC Special Graphics table (0x5f-0x7e).
fn dec_special_graphics(c: char) -> char {
    match c {
        '_' => '\u{a0}',
        '`' => '\u{25c6}',
        'a' => '\u{2592}',
        'b' => '\u{2409}',
        'c' => '\u{240c}',
        'd' => '\u{240d}',
        'e' => '\u{240a}',
        'f' => '\u{b0}',
        'g' => '\u{b1}',
        'h' => '\u{2424}',
        'i' => '\u{240b}',
        'j' => '\u{2518}',
        'k' => '\u{2510}',
        'l' => '\u{250c}',
        'm' => '\u{2514}',
        'n' => '\u{253c}',
        'o' => '\u{23ba}',
        'p' => '\u{23bb}',
        'q' => '\u{2500}',
        'r' => '\u{23bc}',
        's' => '\u{23bd}',
        't' => '\u{251c}',
        'u' => '\u{2524}',
        'v' => '\u{2534}',
        'w' => '\u{252c}',
        'x' => '\u{2502}',
        'y' => '\u{2264}',
        'z' => '\u{2265}',
        '{' => '\u{3c0}',
        '|' => '\u{2260}',
        '}' => '\u{a3}',
        '~' => '\u{b7}',
        _ => c,
    }
}

/// G0-G3 plus the locking (SO/SI) and single (SS2/SS3) shift state.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
struct Charsets {
    g: [Charset; 4],
    /// Which of G0/G1 is mapped into GL (SI = 0, SO = 1).
    gl: usize,
    /// G2/G3 for the next printed character only (SS2/SS3).
    single_shift: Option<usize>,
}

/// Represents the overall terminal state.
#[derive(Clone, Debug)]
pub struct Screen {
    grid: crate::grid::Grid,
    alternate_grid: crate::grid::Grid,

    attrs: crate::attrs::Attrs,
    saved_attrs: crate::attrs::Attrs,

    charsets: Charsets,
    saved_charsets: Charsets,
    /// The previous graphic character, for REP; cleared by anything else.
    last_cluster: Option<String>,
    hyperlinks: Vec<(String, String)>,

    modes: u8,
    mouse_protocol_mode: MouseProtocolMode,
    mouse_protocol_encoding: MouseProtocolEncoding,
}

impl Screen {
    pub(crate) fn new(size: crate::grid::Size, scrollback_len: usize) -> Self {
        let mut grid = crate::grid::Grid::new(size, scrollback_len);
        grid.allocate_rows();
        Self {
            grid,
            alternate_grid: crate::grid::Grid::new(size, 0),

            attrs: crate::attrs::Attrs::default(),
            saved_attrs: crate::attrs::Attrs::default(),

            charsets: Charsets::default(),
            saved_charsets: Charsets::default(),
            last_cluster: None,
            hyperlinks: vec![],

            modes: 0,
            mouse_protocol_mode: MouseProtocolMode::default(),
            mouse_protocol_encoding: MouseProtocolEncoding::default(),
        }
    }

    /// Resizes the terminal.
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.grid.set_size(crate::grid::Size { rows, cols });
        self.alternate_grid
            .set_size(crate::grid::Size { rows, cols });
    }

    /// Returns the current size of the terminal.
    ///
    /// The return value will be (rows, cols).
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let size = self.grid().size();
        (size.rows, size.cols)
    }

    /// Scrolls to the given position in the scrollback.
    ///
    /// This position indicates the offset from the top of the screen, and
    /// should be `0` to put the normal screen in view.
    ///
    /// This affects the return values of methods called on the screen: for
    /// instance, `screen.cell(0, 0)` will return the top left corner of the
    /// screen after taking the scrollback offset into account.
    ///
    /// The value given will be clamped to the actual size of the scrollback.
    pub fn set_scrollback(&mut self, rows: usize) {
        self.grid_mut().set_scrollback(rows);
    }

    /// Returns the current position in the scrollback.
    ///
    /// This position indicates the offset from the top of the screen, and is
    /// `0` when the normal screen is in view.
    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.grid().scrollback()
    }

    /// Returns the text contents of the terminal.
    ///
    /// This will not include any formatting information, and will be in plain
    /// text format.
    #[must_use]
    pub fn contents(&self) -> String {
        let mut contents = String::new();
        self.write_contents(&mut contents);
        contents
    }

    fn write_contents(&self, contents: &mut String) {
        self.grid().write_contents(contents);
    }

    /// Returns the text contents of the terminal by row, restricted to the
    /// given subset of columns.
    ///
    /// This will not include any formatting information, and will be in plain
    /// text format.
    ///
    /// Newlines will not be included.
    pub fn rows(&self, start: u16, width: u16) -> impl Iterator<Item = String> + '_ {
        self.grid().visible_rows().map(move |row| {
            let mut contents = String::new();
            row.write_contents(&mut contents, start, width, false);
            contents
        })
    }

    /// Returns the text contents of the terminal logically between two cells.
    /// This will include the remainder of the starting row after `start_col`,
    /// followed by the entire contents of the rows between `start_row` and
    /// `end_row`, followed by the beginning of the `end_row` up until
    /// `end_col`. This is useful for things like determining the contents of
    /// a clipboard selection.
    #[must_use]
    pub fn contents_between(
        &self,
        start_row: u16,
        start_col: u16,
        end_row: u16,
        end_col: u16,
    ) -> String {
        match start_row.cmp(&end_row) {
            std::cmp::Ordering::Less => {
                let (_, cols) = self.size();
                let mut contents = String::new();
                for (i, row) in self
                    .grid()
                    .visible_rows()
                    .enumerate()
                    .skip(usize::from(start_row))
                    .take(usize::from(end_row) - usize::from(start_row) + 1)
                {
                    if i == usize::from(start_row) {
                        row.write_contents(&mut contents, start_col, cols - start_col, false);
                        if !row.wrapped() {
                            contents.push('\n');
                        }
                    } else if i == usize::from(end_row) {
                        row.write_contents(&mut contents, 0, end_col, false);
                    } else {
                        row.write_contents(&mut contents, 0, cols, false);
                        if !row.wrapped() {
                            contents.push('\n');
                        }
                    }
                }
                contents
            }
            std::cmp::Ordering::Equal => {
                if start_col < end_col {
                    self.rows(start_col, end_col - start_col)
                        .nth(usize::from(start_row))
                        .unwrap_or_default()
                } else {
                    String::new()
                }
            }
            std::cmp::Ordering::Greater => String::new(),
        }
    }

    /// Return escape codes sufficient to reproduce the entire contents of the
    /// current terminal state. This is a convenience wrapper around
    /// [`contents_formatted`](Self::contents_formatted) and
    /// [`input_mode_formatted`](Self::input_mode_formatted).
    #[must_use]
    pub fn state_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_formatted(&mut contents);
        self.write_input_mode_formatted(&mut contents);
        contents
    }

    /// Return escape codes sufficient to turn the terminal state of the
    /// screen `prev` into the current terminal state. This is a convenience
    /// wrapper around [`contents_diff`](Self::contents_diff) and
    /// [`input_mode_diff`](Self::input_mode_diff).
    #[must_use]
    pub fn state_diff(&self, prev: &Self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_diff(&mut contents, prev);
        self.write_input_mode_diff(&mut contents, prev);
        contents
    }

    /// Returns the formatted visible contents of the terminal.
    ///
    /// Formatting information will be included inline as terminal escape
    /// codes. The result will be suitable for feeding directly to a raw
    /// terminal parser, and will result in the same visual output.
    #[must_use]
    pub fn contents_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_formatted(&mut contents);
        contents
    }

    fn write_contents_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::HideCursor::new(self.hide_cursor()).write_buf(contents);
        let prev_attrs = self.grid().write_contents_formatted(contents);
        self.attrs.write_escape_code_diff(contents, &prev_attrs);
    }

    /// Returns the formatted visible contents of the terminal by row,
    /// restricted to the given subset of columns.
    ///
    /// Formatting information will be included inline as terminal escape
    /// codes. The result will be suitable for feeding directly to a raw
    /// terminal parser, and will result in the same visual output.
    ///
    /// You are responsible for positioning the cursor before printing each
    /// row, and the final cursor position after displaying each row is
    /// unspecified.
    // the unwraps in this method shouldn't be reachable
    #[allow(clippy::missing_panics_doc)]
    pub fn rows_formatted(&self, start: u16, width: u16) -> impl Iterator<Item = Vec<u8>> + '_ {
        let mut wrapping = false;
        self.grid().visible_rows().enumerate().map(move |(i, row)| {
            // number of rows in a grid is stored in a u16 (see Size), so
            // visible_rows can never return enough rows to overflow here
            let i = i.try_into().unwrap();
            let mut contents = vec![];
            row.write_contents_formatted(&mut contents, start, width, i, wrapping, None, None);
            if start == 0 && width == self.grid.size().cols {
                wrapping = row.wrapped();
            }
            contents
        })
    }

    /// Returns a terminal byte stream sufficient to turn the visible contents
    /// of the screen described by `prev` into the visible contents of the
    /// screen described by `self`.
    ///
    /// The result of rendering `prev.contents_formatted()` followed by
    /// `self.contents_diff(prev)` should be equivalent to the result of
    /// rendering `self.contents_formatted()`. This is primarily useful when
    /// you already have a terminal parser whose state is described by `prev`,
    /// since the diff will likely require less memory and cause less
    /// flickering than redrawing the entire screen contents.
    #[must_use]
    pub fn contents_diff(&self, prev: &Self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_diff(&mut contents, prev);
        contents
    }

    fn write_contents_diff(&self, contents: &mut Vec<u8>, prev: &Self) {
        if self.hide_cursor() != prev.hide_cursor() {
            crate::term::HideCursor::new(self.hide_cursor()).write_buf(contents);
        }
        let prev_attrs = self
            .grid()
            .write_contents_diff(contents, prev.grid(), prev.attrs);
        self.attrs.write_escape_code_diff(contents, &prev_attrs);
    }

    /// Returns a sequence of terminal byte streams sufficient to turn the
    /// visible contents of the subset of each row from `prev` (as described
    /// by `start` and `width`) into the visible contents of the corresponding
    /// row subset in `self`.
    ///
    /// You are responsible for positioning the cursor before printing each
    /// row, and the final cursor position after displaying each row is
    /// unspecified.
    // the unwraps in this method shouldn't be reachable
    #[allow(clippy::missing_panics_doc)]
    pub fn rows_diff<'a>(
        &'a self,
        prev: &'a Self,
        start: u16,
        width: u16,
    ) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.grid()
            .visible_rows()
            .zip(prev.grid().visible_rows())
            .enumerate()
            .map(move |(i, (row, prev_row))| {
                // number of rows in a grid is stored in a u16 (see Size), so
                // visible_rows can never return enough rows to overflow here
                let i = i.try_into().unwrap();
                let mut contents = vec![];
                row.write_contents_diff(
                    &mut contents,
                    prev_row,
                    start,
                    width,
                    i,
                    false,
                    false,
                    crate::grid::Pos { row: i, col: start },
                    crate::attrs::Attrs::default(),
                );
                contents
            })
    }

    /// Returns terminal escape sequences sufficient to set the current
    /// terminal's input modes.
    ///
    /// Supported modes are:
    /// * application keypad
    /// * application cursor
    /// * bracketed paste
    /// * xterm mouse support
    #[must_use]
    pub fn input_mode_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_input_mode_formatted(&mut contents);
        contents
    }

    fn write_input_mode_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::ApplicationKeypad::new(self.mode(MODE_APPLICATION_KEYPAD)).write_buf(contents);
        crate::term::ApplicationCursor::new(self.mode(MODE_APPLICATION_CURSOR)).write_buf(contents);
        crate::term::BracketedPaste::new(self.mode(MODE_BRACKETED_PASTE)).write_buf(contents);
        crate::term::MouseProtocolMode::new(self.mouse_protocol_mode, MouseProtocolMode::None)
            .write_buf(contents);
        crate::term::MouseProtocolEncoding::new(
            self.mouse_protocol_encoding,
            MouseProtocolEncoding::Default,
        )
        .write_buf(contents);
    }

    /// Returns terminal escape sequences sufficient to change the previous
    /// terminal's input modes to the input modes enabled in the current
    /// terminal.
    #[must_use]
    pub fn input_mode_diff(&self, prev: &Self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_input_mode_diff(&mut contents, prev);
        contents
    }

    fn write_input_mode_diff(&self, contents: &mut Vec<u8>, prev: &Self) {
        if self.mode(MODE_APPLICATION_KEYPAD) != prev.mode(MODE_APPLICATION_KEYPAD) {
            crate::term::ApplicationKeypad::new(self.mode(MODE_APPLICATION_KEYPAD))
                .write_buf(contents);
        }
        if self.mode(MODE_APPLICATION_CURSOR) != prev.mode(MODE_APPLICATION_CURSOR) {
            crate::term::ApplicationCursor::new(self.mode(MODE_APPLICATION_CURSOR))
                .write_buf(contents);
        }
        if self.mode(MODE_BRACKETED_PASTE) != prev.mode(MODE_BRACKETED_PASTE) {
            crate::term::BracketedPaste::new(self.mode(MODE_BRACKETED_PASTE)).write_buf(contents);
        }
        crate::term::MouseProtocolMode::new(self.mouse_protocol_mode, prev.mouse_protocol_mode)
            .write_buf(contents);
        crate::term::MouseProtocolEncoding::new(
            self.mouse_protocol_encoding,
            prev.mouse_protocol_encoding,
        )
        .write_buf(contents);
    }

    /// Returns terminal escape sequences sufficient to set the current
    /// terminal's drawing attributes.
    ///
    /// Supported drawing attributes are:
    /// * fgcolor
    /// * bgcolor
    /// * bold
    /// * dim
    /// * italic
    /// * underline
    /// * inverse
    ///
    /// This is not typically necessary, since
    /// [`contents_formatted`](Self::contents_formatted) will leave
    /// the current active drawing attributes in the correct state, but this
    /// can be useful in the case of drawing additional things on top of a
    /// terminal output, since you will need to restore the terminal state
    /// without the terminal contents necessarily being the same.
    #[must_use]
    pub fn attributes_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_attributes_formatted(&mut contents);
        contents
    }

    fn write_attributes_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::ClearAttrs.write_buf(contents);
        self.attrs
            .write_escape_code_diff(contents, &crate::attrs::Attrs::default());
    }

    /// Returns the current cursor position of the terminal.
    ///
    /// The return value will be (row, col).
    #[must_use]
    pub fn cursor_position(&self) -> (u16, u16) {
        let pos = self.grid().pos();
        (pos.row, pos.col)
    }

    /// Returns terminal escape sequences sufficient to set the current
    /// cursor state of the terminal.
    ///
    /// This is not typically necessary, since
    /// [`contents_formatted`](Self::contents_formatted) will leave
    /// the cursor in the correct state, but this can be useful in the case of
    /// drawing additional things on top of a terminal output, since you will
    /// need to restore the terminal state without the terminal contents
    /// necessarily being the same.
    ///
    /// Note that the bytes returned by this function may alter the active
    /// drawing attributes, because it may require redrawing existing cells in
    /// order to position the cursor correctly (for instance, in the case
    /// where the cursor is past the end of a row). Therefore, you should
    /// ensure to reset the active drawing attributes if necessary after
    /// processing this data, for instance by using
    /// [`attributes_formatted`](Self::attributes_formatted).
    #[must_use]
    pub fn cursor_state_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_cursor_state_formatted(&mut contents);
        contents
    }

    fn write_cursor_state_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::HideCursor::new(self.hide_cursor()).write_buf(contents);
        self.grid()
            .write_cursor_position_formatted(contents, None, None);

        // we don't just call write_attributes_formatted here, because that
        // would still be confusing - consider the case where the user sets
        // their own unrelated drawing attributes (on a different parser
        // instance) and then calls cursor_state_formatted. just documenting
        // it and letting the user handle it on their own is more
        // straightforward.
    }

    /// Returns the [`Cell`](crate::Cell) object at the given location in the
    /// terminal, if it exists.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&crate::Cell> {
        self.grid().visible_cell(crate::grid::Pos { row, col })
    }

    /// Returns whether the text in row `row` should wrap to the next line.
    #[must_use]
    pub fn row_wrapped(&self, row: u16) -> bool {
        self.grid()
            .visible_row(row)
            .is_some_and(crate::row::Row::wrapped)
    }

    /// Returns whether the alternate screen is currently in use.
    #[must_use]
    pub fn alternate_screen(&self) -> bool {
        self.mode(MODE_ALTERNATE_SCREEN)
    }

    /// Returns whether the terminal should be in application keypad mode.
    #[must_use]
    pub fn application_keypad(&self) -> bool {
        self.mode(MODE_APPLICATION_KEYPAD)
    }

    /// Returns whether the terminal should be in application cursor mode.
    #[must_use]
    pub fn application_cursor(&self) -> bool {
        self.mode(MODE_APPLICATION_CURSOR)
    }

    /// Returns whether the terminal should be in hide cursor mode.
    #[must_use]
    pub fn hide_cursor(&self) -> bool {
        self.mode(MODE_HIDE_CURSOR)
    }

    /// Returns whether the terminal should be in bracketed paste mode.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.mode(MODE_BRACKETED_PASTE)
    }

    /// Returns the currently active [`MouseProtocolMode`].
    #[must_use]
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        self.mouse_protocol_mode
    }

    /// Returns the currently active [`MouseProtocolEncoding`].
    #[must_use]
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        self.mouse_protocol_encoding
    }

    /// Returns the currently active foreground color.
    #[must_use]
    pub fn fgcolor(&self) -> crate::Color {
        self.attrs.fgcolor
    }

    /// Returns the currently active background color.
    #[must_use]
    pub fn bgcolor(&self) -> crate::Color {
        self.attrs.bgcolor
    }

    /// Returns whether newly drawn text should be rendered with the bold text
    /// attribute.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.attrs.bold()
    }

    /// Returns whether newly drawn text should be rendered with the dim text
    /// attribute.
    #[must_use]
    pub fn dim(&self) -> bool {
        self.attrs.dim()
    }

    /// Returns whether newly drawn text should be rendered with the italic
    /// text attribute.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.attrs.italic()
    }

    /// Returns whether newly drawn text should be rendered with the
    /// underlined text attribute.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.attrs.underline()
    }

    /// Returns whether newly drawn text should be rendered with the inverse
    /// text attribute.
    #[must_use]
    pub fn inverse(&self) -> bool {
        self.attrs.inverse()
    }

    pub(crate) fn grid(&self) -> &crate::grid::Grid {
        if self.mode(MODE_ALTERNATE_SCREEN) {
            &self.alternate_grid
        } else {
            &self.grid
        }
    }

    fn grid_mut(&mut self) -> &mut crate::grid::Grid {
        if self.mode(MODE_ALTERNATE_SCREEN) {
            &mut self.alternate_grid
        } else {
            &mut self.grid
        }
    }

    fn enter_alternate_grid(&mut self) {
        self.grid_mut().set_scrollback(0);
        self.set_mode(MODE_ALTERNATE_SCREEN);
        self.alternate_grid.allocate_rows();
    }

    fn exit_alternate_grid(&mut self) {
        self.clear_mode(MODE_ALTERNATE_SCREEN);
    }

    fn save_cursor(&mut self) {
        self.grid_mut().save_cursor();
        self.saved_attrs = self.attrs;
        self.saved_charsets = self.charsets;
    }

    fn restore_cursor(&mut self) {
        self.grid_mut().restore_cursor();
        self.attrs = self.saved_attrs;
        self.charsets = self.saved_charsets;
        self.sync_blank_attrs();
    }

    // scrolling and insert/delete blank cells with the current background
    fn sync_blank_attrs(&mut self) {
        let blank = self.attrs.for_erase();
        self.grid.set_blank_attrs(blank);
        self.alternate_grid.set_blank_attrs(blank);
    }

    /// Resolves a [`Cell::hyperlink`] index to its `(uri, id)`; `id` is empty
    /// when the program gave none.
    #[must_use]
    pub fn hyperlink(&self, index: u32) -> Option<(&str, &str)> {
        let i = usize::try_from(index.checked_sub(1)?).ok()?;
        self.hyperlinks
            .get(i)
            .map(|(u, id)| (u.as_str(), id.as_str()))
    }

    // OSC 8 ; params ; URI. An empty URI ends the link.
    pub(crate) fn osc8(&mut self, params: &[u8], uri: &[u8]) {
        self.last_cluster = None;
        if uri.is_empty() {
            self.attrs.hyperlink = 0;
            return;
        }
        let (Ok(uri), Ok(params)) = (std::str::from_utf8(uri), std::str::from_utf8(params)) else {
            return;
        };
        let id = params
            .split(':')
            .find_map(|kv| kv.strip_prefix("id="))
            .unwrap_or("");
        let existing = self
            .hyperlinks
            .iter()
            .position(|(u, i)| u == uri && i == id);
        let index = match existing {
            Some(i) => i,
            None if self.hyperlinks.len() < MAX_HYPERLINKS => {
                self.hyperlinks.push((uri.to_string(), id.to_string()));
                self.hyperlinks.len() - 1
            }
            // table full: leave the text unlinked rather than evict a live link
            None => {
                self.attrs.hyperlink = 0;
                return;
            }
        };
        self.attrs.hyperlink = u32::try_from(index + 1).unwrap_or(0);
    }

    // SCS: ESC ( ) * + <final> designates G0..G3.
    pub(crate) fn scs(&mut self, slot: u8, final_byte: u8) {
        self.last_cluster = None;
        let Some(set) = Charset::from_final(final_byte) else {
            return;
        };
        if let Some(g) = match slot {
            b'(' => Some(0),
            b')' => Some(1),
            b'*' => Some(2),
            b'+' => Some(3),
            _ => None,
        } {
            self.charsets.g[g] = set;
        }
    }

    // SI (0x0F) selects G0 into GL, SO (0x0E) selects G1.
    pub(crate) fn shift(&mut self, g: usize) {
        self.last_cluster = None;
        self.charsets.gl = g;
    }

    // ESC N / ESC O: the next character comes from G2 / G3.
    pub(crate) fn single_shift(&mut self, g: usize) {
        self.last_cluster = None;
        self.charsets.single_shift = Some(g);
    }

    // anything that isn't a printed character breaks REP's "preceding graphic character"
    pub(crate) fn break_rep(&mut self) {
        self.last_cluster = None;
    }

    // CSI Ps b
    pub(crate) fn rep(&mut self, count: u16) {
        let Some(cluster) = self.last_cluster.clone() else {
            return;
        };
        // bound a hostile `CSI 65535 b` to one screenful of work per call
        let (rows, cols) = self.size();
        let max = usize::from(rows) * usize::from(cols);
        for _ in 0..usize::from(count).min(max) {
            for c in cluster.chars() {
                self.draw(c);
            }
        }
        // xterm.js consumes the preceding character: a second REP in a row repeats nothing
        self.last_cluster = None;
    }

    fn set_mode(&mut self, mode: u8) {
        self.modes |= mode;
    }

    fn clear_mode(&mut self, mode: u8) {
        self.modes &= !mode;
    }

    fn mode(&self, mode: u8) -> bool {
        self.modes & mode != 0
    }

    fn set_mouse_mode(&mut self, mode: MouseProtocolMode) {
        self.mouse_protocol_mode = mode;
    }

    fn clear_mouse_mode(&mut self, mode: MouseProtocolMode) {
        if self.mouse_protocol_mode == mode {
            self.mouse_protocol_mode = MouseProtocolMode::default();
        }
    }

    fn set_mouse_encoding(&mut self, encoding: MouseProtocolEncoding) {
        self.mouse_protocol_encoding = encoding;
    }

    fn clear_mouse_encoding(&mut self, encoding: MouseProtocolEncoding) {
        if self.mouse_protocol_encoding == encoding {
            self.mouse_protocol_encoding = MouseProtocolEncoding::default();
        }
    }
}

impl Screen {
    pub(crate) fn text(&mut self, c: char) {
        let g = self
            .charsets
            .single_shift
            .take()
            .unwrap_or(self.charsets.gl);
        let c = self.charsets.g[g].map(c);
        self.draw(c);
        match c.width() {
            None => {}
            // a combining mark joins the previous cluster, which REP repeats whole
            Some(0) => {
                if let Some(cluster) = &mut self.last_cluster {
                    cluster.push(c);
                }
            }
            Some(_) => self.last_cluster = Some(c.to_string()),
        }
    }

    fn draw(&mut self, c: char) {
        self.draw_inner(c);
        if self.mode(MODE_NO_AUTOWRAP) {
            let cols = self.grid().size().cols;
            if self.grid().pos().col >= cols {
                self.grid_mut().col_set(cols - 1);
            }
        }
    }

    fn draw_inner(&mut self, c: char) {
        let pos = self.grid().pos();
        let size = self.grid().size();
        let attrs = self.attrs;

        // U+FEFF is dropped and U+00AD takes a cell of its own (xterm.js); unicode-width calls
        // both zero-width, which would glue them onto the previous character
        if c == '\u{feff}' {
            return;
        }
        let width = if c == '\u{ad}' { Some(1) } else { c.width() };
        if width.is_none() && (u32::from(c)) < 256 {
            // don't even try to draw control characters
            return;
        }
        let width = width
            .unwrap_or(1)
            .try_into()
            // width() can only return 0, 1, or 2
            .unwrap();

        // it doesn't make any sense to wrap if the last column in a row
        // didn't already have contents. don't try to handle the case where a
        // character wraps because there was only one column left in the
        // previous row - literally everything handles this case differently,
        // and this is tmux behavior (and also the simplest). i'm open to
        // reconsidering this behavior, but only with a really good reason
        // (xterm handles this by introducing the concept of triple width
        // cells, which i really don't want to do).
        let mut wrap = false;
        let autowrap = !self.mode(MODE_NO_AUTOWRAP);
        if !autowrap {
            // DECAWM off: the cursor sticks at the last column and later characters overwrite it
            if pos.col > size.cols - width {
                if width > 1 {
                    // a wide character that doesn't fit is dropped, as in xterm.js
                    return;
                }
                self.grid_mut().col_set(size.cols - width);
            }
        } else if pos.col > size.cols - width {
            let last_cell = self
                .grid()
                .drawing_cell(crate::grid::Pos {
                    row: pos.row,
                    col: size.cols - 1,
                })
                // pos.row is valid, since it comes directly from
                // self.grid().pos() which we assume to always have a valid
                // row value. size.cols - 1 is also always a valid column.
                .unwrap();
            if last_cell.has_contents() || last_cell.is_wide_continuation() {
                wrap = true;
            }
        }
        if autowrap {
            self.grid_mut().col_wrap(width, wrap);
        }
        let pos = self.grid().pos();
        if self.mode(MODE_INSERT) && width > 0 {
            self.grid_mut().insert_cells(width);
        }

        if width == 0 {
            if pos.col > 0 {
                let mut prev_cell = self
                    .grid_mut()
                    .drawing_cell_mut(crate::grid::Pos {
                        row: pos.row,
                        col: pos.col - 1,
                    })
                    // pos.row is valid, since it comes directly from
                    // self.grid().pos() which we assume to always have a
                    // valid row value. pos.col - 1 is valid because we just
                    // checked for pos.col > 0.
                    .unwrap();
                if prev_cell.is_wide_continuation() {
                    prev_cell = self
                        .grid_mut()
                        .drawing_cell_mut(crate::grid::Pos {
                            row: pos.row,
                            col: pos.col - 2,
                        })
                        // pos.row is valid, since it comes directly from
                        // self.grid().pos() which we assume to always have a
                        // valid row value. we know pos.col - 2 is valid
                        // because the cell at pos.col - 1 is a wide
                        // continuation character, which means there must be
                        // the first half of the wide character before it.
                        .unwrap();
                }
                prev_cell.append(c);
            } else if pos.row > 0 {
                let prev_row = self
                    .grid()
                    .drawing_row(pos.row - 1)
                    // pos.row is valid, since it comes directly from
                    // self.grid().pos() which we assume to always have a
                    // valid row value. pos.row - 1 is valid because we just
                    // checked for pos.row > 0.
                    .unwrap();
                if prev_row.wrapped() {
                    let mut prev_cell = self
                        .grid_mut()
                        .drawing_cell_mut(crate::grid::Pos {
                            row: pos.row - 1,
                            col: size.cols - 1,
                        })
                        // pos.row is valid, since it comes directly from
                        // self.grid().pos() which we assume to always have a
                        // valid row value. pos.row - 1 is valid because we
                        // just checked for pos.row > 0. col of size.cols - 1
                        // is always valid.
                        .unwrap();
                    if prev_cell.is_wide_continuation() {
                        prev_cell = self
                            .grid_mut()
                            .drawing_cell_mut(crate::grid::Pos {
                                row: pos.row - 1,
                                col: size.cols - 2,
                            })
                            // pos.row is valid, since it comes directly from
                            // self.grid().pos() which we assume to always
                            // have a valid row value. pos.row - 1 is valid
                            // because we just checked for pos.row > 0. col of
                            // size.cols - 2 is valid because the cell at
                            // size.cols - 1 is a wide continuation character,
                            // so it must have the first half of the wide
                            // character before it.
                            .unwrap();
                    }
                    prev_cell.append(c);
                }
            }
        } else {
            if self
                .grid()
                .drawing_cell(pos)
                // pos.row is valid because we assume self.grid().pos() to
                // always have a valid row value. pos.col is valid because we
                // called col_wrap() immediately before this, which ensures
                // that self.grid().pos().col has a valid value.
                .unwrap()
                .is_wide_continuation()
            {
                let prev_cell = self
                    .grid_mut()
                    .drawing_cell_mut(crate::grid::Pos {
                        row: pos.row,
                        col: pos.col - 1,
                    })
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() immediately before this, which
                    // ensures that self.grid().pos().col has a valid value.
                    // pos.col - 1 is valid because the cell at pos.col is a
                    // wide continuation character, so it must have the first
                    // half of the wide character before it.
                    .unwrap();
                prev_cell.clear(attrs);
            }

            if self
                .grid()
                .drawing_cell(pos)
                // pos.row is valid because we assume self.grid().pos() to
                // always have a valid row value. pos.col is valid because we
                // called col_wrap() immediately before this, which ensures
                // that self.grid().pos().col has a valid value.
                .unwrap()
                .is_wide()
            {
                let next_cell = self
                    .grid_mut()
                    .drawing_cell_mut(crate::grid::Pos {
                        row: pos.row,
                        col: pos.col + 1,
                    })
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() immediately before this, which
                    // ensures that self.grid().pos().col has a valid value.
                    // pos.col + 1 is valid because the cell at pos.col is a
                    // wide character, so it must have the second half of the
                    // wide character after it.
                    .unwrap();
                next_cell.set(' ', attrs);
            }

            let cell = self
                .grid_mut()
                .drawing_cell_mut(pos)
                // pos.row is valid because we assume self.grid().pos() to
                // always have a valid row value. pos.col is valid because we
                // called col_wrap() immediately before this, which ensures
                // that self.grid().pos().col has a valid value.
                .unwrap();
            cell.set(c, attrs);
            self.grid_mut().col_inc(1);
            if width > 1 {
                let pos = self.grid().pos();
                if self
                    .grid()
                    .drawing_cell(pos)
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() earlier, which ensures that
                    // self.grid().pos().col has a valid value. this is true
                    // even though we just called col_inc, because this branch
                    // only happens if width > 1, and col_wrap takes width
                    // into account.
                    .unwrap()
                    .is_wide()
                {
                    let next_next_pos = crate::grid::Pos {
                        row: pos.row,
                        col: pos.col + 1,
                    };
                    let next_next_cell = self
                        .grid_mut()
                        .drawing_cell_mut(next_next_pos)
                        // pos.row is valid because we assume
                        // self.grid().pos() to always have a valid row value.
                        // pos.col is valid because we called col_wrap()
                        // earlier, which ensures that self.grid().pos().col
                        // has a valid value. this is true even though we just
                        // called col_inc, because this branch only happens if
                        // width > 1, and col_wrap takes width into account.
                        // pos.col + 1 is valid because the cell at pos.col is
                        // wide, and so it must have the second half of the
                        // wide character after it.
                        .unwrap();
                    next_next_cell.clear(attrs);
                    if next_next_pos.col == size.cols - 1 {
                        self.grid_mut()
                            .drawing_row_mut(pos.row)
                            // we assume self.grid().pos().row is always valid
                            .unwrap()
                            .wrap(false);
                    }
                }
                let next_cell = self
                    .grid_mut()
                    .drawing_cell_mut(pos)
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() earlier, which ensures that
                    // self.grid().pos().col has a valid value. this is true
                    // even though we just called col_inc, because this branch
                    // only happens if width > 1, and col_wrap takes width
                    // into account.
                    .unwrap();
                // xterm.js gives the second half the same rendition as the first
                next_cell.clear(attrs);
                next_cell.set_wide_continuation(true);
                self.grid_mut().col_inc(1);
            }
        }
    }

    // control codes

    pub(crate) fn bs(&mut self) {
        self.grid_mut().col_dec_restricted(1);
    }

    pub(crate) fn tab(&mut self) {
        self.grid_mut().col_tab();
    }

    pub(crate) fn lf(&mut self) {
        if self.mode(MODE_NEWLINE) {
            self.cr();
        }
        self.grid_mut().row_inc_scroll(1);
    }

    // ESC D (IND): down one line, scrolling at the bottom margin; never touches the column
    pub(crate) fn ind(&mut self) {
        self.grid_mut().row_inc_scroll(1);
    }

    // ESC E (NEL)
    pub(crate) fn nel(&mut self) {
        self.cr();
        self.ind();
    }

    // ESC H (HTS)
    pub(crate) fn hts(&mut self) {
        self.grid_mut().set_tab_stop();
    }

    // CSI Ps g (TBC): 0 clears the stop at the cursor, 3 clears them all
    pub(crate) fn tbc(&mut self, mode: u16, mut unhandled: impl FnMut(&mut Self)) {
        match mode {
            0 => self.grid_mut().clear_tab_stop(),
            3 => self.grid_mut().clear_all_tab_stops(),
            _ => unhandled(self),
        }
    }

    // CSI Ps I (CHT) / CSI Ps Z (CBT)
    pub(crate) fn cht(&mut self, count: u16) {
        self.grid_mut().col_forward_tab(count);
    }

    pub(crate) fn cbt(&mut self, count: u16) {
        self.grid_mut().col_back_tab(count);
    }

    // ESC # 8 (DECALN)
    pub(crate) fn decaln(&mut self) {
        self.grid_mut().align_test();
    }

    // CSI ! p (DECSTR): soft terminal reset - state only, the screen contents stay
    pub(crate) fn decstr(&mut self) {
        self.attrs = crate::attrs::Attrs::default();
        self.saved_attrs = crate::attrs::Attrs::default();
        self.charsets = Charsets::default();
        self.saved_charsets = Charsets::default();
        self.last_cluster = None;
        self.clear_mode(
            MODE_INSERT
                | MODE_NEWLINE
                | MODE_NO_AUTOWRAP
                | MODE_APPLICATION_CURSOR
                | MODE_APPLICATION_KEYPAD
                | MODE_HIDE_CURSOR,
        );
        self.grid_mut().soft_reset();
        self.sync_blank_attrs();
    }

    // CSI Pm h / CSI Pm l without `?`: ANSI modes 4 (IRM) and 20 (LNM)
    pub(crate) fn ansi_mode(
        &mut self,
        params: &vte::Params,
        on: bool,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        for param in params {
            let bit = match param {
                [4] => MODE_INSERT,
                [20] => MODE_NEWLINE,
                _ => {
                    unhandled(self);
                    continue;
                }
            };
            if on {
                self.set_mode(bit);
            } else {
                self.clear_mode(bit);
            }
        }
    }

    pub(crate) fn vt(&mut self) {
        self.lf();
    }

    pub(crate) fn ff(&mut self) {
        self.lf();
    }

    pub(crate) fn cr(&mut self) {
        self.grid_mut().col_set(0);
    }

    // escape codes

    // ESC 7
    pub(crate) fn decsc(&mut self) {
        self.save_cursor();
    }

    // ESC 8
    pub(crate) fn decrc(&mut self) {
        self.restore_cursor();
    }

    // ESC =
    pub(crate) fn deckpam(&mut self) {
        self.set_mode(MODE_APPLICATION_KEYPAD);
    }

    // ESC >
    pub(crate) fn deckpnm(&mut self) {
        self.clear_mode(MODE_APPLICATION_KEYPAD);
    }

    // ESC M
    pub(crate) fn ri(&mut self) {
        self.grid_mut().restrict_col();
        self.grid_mut().row_dec_scroll(1);
    }

    // ESC c
    pub(crate) fn ris(&mut self) {
        *self = Self::new(self.grid.size(), self.grid.scrollback_len());
    }

    // csi codes

    // CSI @
    pub(crate) fn ich(&mut self, count: u16) {
        self.grid_mut().insert_cells(count);
    }

    // CSI A
    pub(crate) fn cuu(&mut self, offset: u16) {
        self.grid_mut().restrict_col();
        self.grid_mut().row_dec_clamp(offset);
    }

    // CSI B
    pub(crate) fn cud(&mut self, offset: u16) {
        self.grid_mut().restrict_col();
        self.grid_mut().row_inc_clamp(offset);
    }

    // CSI C
    pub(crate) fn cuf(&mut self, offset: u16) {
        self.grid_mut().col_inc_clamp(offset);
    }

    // CSI D
    pub(crate) fn cub(&mut self, offset: u16) {
        self.grid_mut().col_dec_restricted(offset);
    }

    // CSI E
    pub(crate) fn cnl(&mut self, offset: u16) {
        self.grid_mut().col_set(0);
        self.grid_mut().row_inc_clamp(offset);
    }

    // CSI F
    pub(crate) fn cpl(&mut self, offset: u16) {
        self.grid_mut().col_set(0);
        self.grid_mut().row_dec_clamp(offset);
    }

    // CSI G
    pub(crate) fn cha(&mut self, col: u16) {
        self.grid_mut().col_set(col - 1);
    }

    // CSI H
    pub(crate) fn cup(&mut self, (row, col): (u16, u16)) {
        self.grid_mut().set_pos(crate::grid::Pos {
            row: row - 1,
            col: col - 1,
        });
    }

    // CSI J
    pub(crate) fn ed(&mut self, mode: u16, mut unhandled: impl FnMut(&mut Self)) {
        let attrs = self.attrs.for_erase();
        match mode {
            0 => self.grid_mut().erase_all_forward(attrs),
            1 => self.grid_mut().erase_all_backward(attrs),
            2 => self.grid_mut().erase_all(attrs),
            _ => unhandled(self),
        }
    }

    // CSI ? J
    pub(crate) fn decsed(&mut self, mode: u16, unhandled: impl FnMut(&mut Self)) {
        self.ed(mode, unhandled);
    }

    // CSI K
    pub(crate) fn el(&mut self, mode: u16, mut unhandled: impl FnMut(&mut Self)) {
        let attrs = self.attrs.for_erase();
        match mode {
            0 => self.grid_mut().erase_row_forward(attrs),
            1 => self.grid_mut().erase_row_backward(attrs),
            2 => self.grid_mut().erase_row(attrs),
            _ => unhandled(self),
        }
    }

    // CSI ? K
    pub(crate) fn decsel(&mut self, mode: u16, unhandled: impl FnMut(&mut Self)) {
        self.el(mode, unhandled);
    }

    // CSI L
    pub(crate) fn il(&mut self, count: u16) {
        self.grid_mut().insert_lines(count);
    }

    // CSI M
    pub(crate) fn dl(&mut self, count: u16) {
        self.grid_mut().delete_lines(count);
    }

    // CSI P
    pub(crate) fn dch(&mut self, count: u16) {
        self.grid_mut().delete_cells(count);
    }

    // CSI S
    pub(crate) fn su(&mut self, count: u16) {
        self.grid_mut().scroll_up(count);
    }

    // CSI T
    pub(crate) fn sd(&mut self, count: u16) {
        self.grid_mut().scroll_down(count);
    }

    // CSI X
    pub(crate) fn ech(&mut self, count: u16) {
        let attrs = self.attrs.for_erase();
        self.grid_mut().erase_cells(count, attrs);
    }

    // CSI d
    pub(crate) fn vpa(&mut self, row: u16) {
        self.grid_mut().row_set(row - 1);
    }

    // CSI ? h
    pub(crate) fn decset(&mut self, params: &vte::Params, mut unhandled: impl FnMut(&mut Self)) {
        for param in params {
            match param {
                [1] => self.set_mode(MODE_APPLICATION_CURSOR),
                [6] => self.grid_mut().set_origin_mode(true),
                [7] => self.clear_mode(MODE_NO_AUTOWRAP),
                [9] => self.set_mouse_mode(MouseProtocolMode::Press),
                [25] => self.clear_mode(MODE_HIDE_CURSOR),
                [47 | 1047] => self.enter_alternate_grid(),
                [1048] => self.decsc(),
                [1000] => {
                    self.set_mouse_mode(MouseProtocolMode::PressRelease);
                }
                [1002] => {
                    self.set_mouse_mode(MouseProtocolMode::ButtonMotion);
                }
                [1003] => self.set_mouse_mode(MouseProtocolMode::AnyMotion),
                [1005] => {
                    self.set_mouse_encoding(MouseProtocolEncoding::Utf8);
                }
                [1006] => {
                    self.set_mouse_encoding(MouseProtocolEncoding::Sgr);
                }
                [1049] => {
                    self.decsc();
                    self.alternate_grid.clear();
                    self.enter_alternate_grid();
                }
                [2004] => self.set_mode(MODE_BRACKETED_PASTE),
                _ => unhandled(self),
            }
        }
    }

    // CSI ? l
    pub(crate) fn decrst(&mut self, params: &vte::Params, mut unhandled: impl FnMut(&mut Self)) {
        for param in params {
            match param {
                [1] => self.clear_mode(MODE_APPLICATION_CURSOR),
                [6] => self.grid_mut().set_origin_mode(false),
                [7] => self.set_mode(MODE_NO_AUTOWRAP),
                [9] => self.clear_mouse_mode(MouseProtocolMode::Press),
                [25] => self.set_mode(MODE_HIDE_CURSOR),
                [47] => {
                    self.exit_alternate_grid();
                }
                // 1047 clears the alternate screen on the way out; 1048 restores the cursor
                [1047] => {
                    self.alternate_grid.clear();
                    self.exit_alternate_grid();
                }
                [1048] => self.decrc(),
                [1000] => {
                    self.clear_mouse_mode(MouseProtocolMode::PressRelease);
                }
                [1002] => {
                    self.clear_mouse_mode(MouseProtocolMode::ButtonMotion);
                }
                [1003] => {
                    self.clear_mouse_mode(MouseProtocolMode::AnyMotion);
                }
                [1005] => {
                    self.clear_mouse_encoding(MouseProtocolEncoding::Utf8);
                }
                [1006] => {
                    self.clear_mouse_encoding(MouseProtocolEncoding::Sgr);
                }
                [1049] => {
                    self.exit_alternate_grid();
                    self.decrc();
                }
                [2004] => self.clear_mode(MODE_BRACKETED_PASTE),
                _ => unhandled(self),
            }
        }
    }

    // CSI m (ECMA-48 8.3.117; xterm ctlseqs "CSI Pm m"; ITU T.416 for the colon forms)
    pub(crate) fn sgr(&mut self, params: &vte::Params, unhandled: impl FnMut(&mut Self)) {
        self.sgr_apply(params, unhandled);
        self.sync_blank_attrs();
    }

    fn sgr_apply(&mut self, params: &vte::Params, mut unhandled: impl FnMut(&mut Self)) {
        if params.is_empty() {
            self.attrs.sgr_reset();
            return;
        }

        let ps: Vec<&[u16]> = params.iter().collect();
        let mut i = 0;
        while i < ps.len() {
            let p = ps[i];
            i += 1;
            match *p {
                [0] => self.attrs.sgr_reset(),
                [1] => self.attrs.set_bold(true),
                [2] => self.attrs.set_dim(true),
                [3] => self.attrs.set_italic(true),
                [4] => self
                    .attrs
                    .set_underline(Some(crate::attrs::UnderlineStyle::Single)),
                // 4:n selects the style; 4:0 turns the underline off
                [4, n] => match n {
                    0 => self.attrs.set_underline(None),
                    1 => self
                        .attrs
                        .set_underline(Some(crate::attrs::UnderlineStyle::Single)),
                    2 => self
                        .attrs
                        .set_underline(Some(crate::attrs::UnderlineStyle::Double)),
                    3 => self
                        .attrs
                        .set_underline(Some(crate::attrs::UnderlineStyle::Curly)),
                    4 => self
                        .attrs
                        .set_underline(Some(crate::attrs::UnderlineStyle::Dotted)),
                    5 => self
                        .attrs
                        .set_underline(Some(crate::attrs::UnderlineStyle::Dashed)),
                    _ => unhandled(self),
                },
                [5 | 6] => self.attrs.set_blink(true),
                [7] => self.attrs.set_inverse(true),
                [8] => self.attrs.set_conceal(true),
                [9] => self.attrs.set_strikethrough(true),
                [21] => self
                    .attrs
                    .set_underline(Some(crate::attrs::UnderlineStyle::Double)),
                [22] => self.attrs.set_normal_intensity(),
                [23] => self.attrs.set_italic(false),
                [24] => self.attrs.set_underline(None),
                [25] => self.attrs.set_blink(false),
                [27] => self.attrs.set_inverse(false),
                [28] => self.attrs.set_conceal(false),
                [29] => self.attrs.set_strikethrough(false),
                [n] if (30..=37).contains(&n) => {
                    self.attrs.fgcolor = crate::Color::Idx(small(n) - 30);
                }
                [39] => self.attrs.fgcolor = crate::Color::Default,
                [n] if (40..=47).contains(&n) => {
                    self.attrs.bgcolor = crate::Color::Idx(small(n) - 40);
                }
                [49] => self.attrs.bgcolor = crate::Color::Default,
                [53] => self.attrs.set_overline(true),
                [55] => self.attrs.set_overline(false),
                [59] => self.attrs.ulcolor = crate::Color::Default,
                [n] if (90..=97).contains(&n) => {
                    self.attrs.fgcolor = crate::Color::Idx(small(n) - 82);
                }
                [n] if (100..=107).contains(&n) => {
                    self.attrs.bgcolor = crate::Color::Idx(small(n) - 92);
                }
                [38 | 48 | 58, ..] => {
                    let target = p[0];
                    let Some(color) = extended_color(p, &ps, &mut i) else {
                        unhandled(self);
                        // the remaining params' meaning is unknowable
                        return;
                    };
                    match target {
                        38 => self.attrs.fgcolor = color,
                        48 => self.attrs.bgcolor = color,
                        _ => self.attrs.ulcolor = color,
                    }
                }
                _ => unhandled(self),
            }
        }
    }

    // CSI r
    pub(crate) fn decstbm(&mut self, (top, bottom): (u16, u16)) {
        self.grid_mut().set_scroll_region(top - 1, bottom - 1);
    }
}

/// A value known to be in 30..=107, i.e. to fit a u8.
fn small(n: u16) -> u8 {
    u8::try_from(n).unwrap_or(u8::MAX)
}

/// Parses the color after SGR 38/48/58. `head` is the parameter holding the
/// selector; `i` indexes the next unread parameter for the legacy
/// semicolon-separated form (`38;5;n`, `38;2;r;g;b`). The ITU T.416 colon
/// forms (`38:5:n`, `38:2:r:g:b`, `38:2:<colorspace>:r:g:b`) carry everything
/// in `head`. Returns None for anything malformed or out of range.
fn extended_color(head: &[u16], ps: &[&[u16]], i: &mut usize) -> Option<crate::Color> {
    let byte = |n: u16| u8::try_from(n).ok();
    if head.len() > 1 {
        return match head[1..] {
            [5, n] => Some(crate::Color::Idx(byte(n)?)),
            [2, r, g, b] | [2, _, r, g, b] => Some(crate::Color::Rgb(byte(r)?, byte(g)?, byte(b)?)),
            _ => None,
        };
    }
    let mut next = || {
        let p = ps.get(*i)?;
        *i += 1;
        match **p {
            [n] => Some(n),
            _ => None,
        }
    };
    match next()? {
        5 => Some(crate::Color::Idx(byte(next()?)?)),
        2 => {
            let (r, g, b) = (byte(next()?)?, byte(next()?)?, byte(next()?)?);
            Some(crate::Color::Rgb(r, g, b))
        }
        _ => None,
    }
}
