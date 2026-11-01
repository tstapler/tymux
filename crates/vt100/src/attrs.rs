use crate::term::BufWrite as _;

/// Represents a foreground or background color for cells.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum Color {
    /// The default terminal color.
    #[default]
    Default,

    /// An indexed terminal color.
    Idx(u8),

    /// An RGB terminal color. The parameters are (red, green, blue).
    Rgb(u8, u8, u8),
}

const TEXT_MODE_BOLD: u16 = 1 << 0;
const TEXT_MODE_DIM: u16 = 1 << 1;
const TEXT_MODE_ITALIC: u16 = 1 << 2;
const TEXT_MODE_UNDERLINE: u16 = 1 << 3;
const TEXT_MODE_INVERSE: u16 = 1 << 4;
const TEXT_MODE_STRIKETHROUGH: u16 = 1 << 5;
const TEXT_MODE_BLINK: u16 = 1 << 6;
const TEXT_MODE_CONCEAL: u16 = 1 << 7;
const TEXT_MODE_OVERLINE: u16 = 1 << 8;

/// The line style of an underlined cell (SGR 4, 21 and the `4:n` colon forms).
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum UnderlineStyle {
    /// SGR 4 / `4:1`.
    #[default]
    Single,
    /// SGR 21 / `4:2`.
    Double,
    /// `4:3`.
    Curly,
    /// `4:4`.
    Dotted,
    /// `4:5`.
    Dashed,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fgcolor: Color,
    pub bgcolor: Color,
    /// SGR 58 underline color; `Default` means "same as the text color".
    pub ulcolor: Color,
    pub mode: u16,
    pub underline_style: UnderlineStyle,
    /// 1-based index into the screen's OSC 8 hyperlink table; 0 = none.
    /// Not an SGR attribute, so SGR 0 leaves it alone.
    pub hyperlink: u32,
}

macro_rules! flag {
    ($get:ident, $set:ident, $bit:expr) => {
        pub fn $get(&self) -> bool {
            self.mode & $bit != 0
        }

        pub fn $set(&mut self, on: bool) {
            if on {
                self.mode |= $bit;
            } else {
                self.mode &= !$bit;
            }
        }
    };
}

impl Attrs {
    flag!(bold, set_bold, TEXT_MODE_BOLD);
    flag!(dim, set_dim, TEXT_MODE_DIM);
    flag!(italic, set_italic, TEXT_MODE_ITALIC);
    flag!(inverse, set_inverse, TEXT_MODE_INVERSE);
    flag!(strikethrough, set_strikethrough, TEXT_MODE_STRIKETHROUGH);
    flag!(blink, set_blink, TEXT_MODE_BLINK);
    flag!(conceal, set_conceal, TEXT_MODE_CONCEAL);
    flag!(overline, set_overline, TEXT_MODE_OVERLINE);

    pub fn underline(&self) -> bool {
        self.mode & TEXT_MODE_UNDERLINE != 0
    }

    /// Turns the underline on with `style`, or off (resetting the style).
    pub fn set_underline(&mut self, style: Option<UnderlineStyle>) {
        if let Some(style) = style {
            self.mode |= TEXT_MODE_UNDERLINE;
            self.underline_style = style;
        } else {
            self.mode &= !TEXT_MODE_UNDERLINE;
            self.underline_style = UnderlineStyle::default();
        }
    }

    /// SGR 22: clears both bold and faint.
    pub fn set_normal_intensity(&mut self) {
        self.mode &= !(TEXT_MODE_BOLD | TEXT_MODE_DIM);
    }

    /// What SGR 0 resets to: everything except the hyperlink.
    pub(crate) fn sgr_reset(&mut self) {
        *self = Self {
            hyperlink: self.hyperlink,
            ..Self::default()
        };
    }

    /// Attributes left on a cell by an erase (ED/EL/ECH/DECSED/DECSEL): only the
    /// background survives (xterm's "background color erase"), so a blank cell
    /// never keeps e.g. a faint or reverse flag from the text printed before it.
    pub(crate) fn for_erase(&self) -> Self {
        Self {
            bgcolor: self.bgcolor,
            ..Self::default()
        }
    }

    pub fn write_escape_code_diff(&self, contents: &mut Vec<u8>, other: &Self) {
        if self != other && self.sgr_eq(&Self::default()) {
            crate::term::ClearAttrs.write_buf(contents);
            return;
        }

        let mut attrs = crate::term::Attrs::default();
        if self.fgcolor != other.fgcolor {
            attrs = attrs.fgcolor(self.fgcolor);
        }
        if self.bgcolor != other.bgcolor {
            attrs = attrs.bgcolor(self.bgcolor);
        }
        if self.ulcolor != other.ulcolor {
            attrs = attrs.ulcolor(self.ulcolor);
        }
        // SGR 22 clears bold and faint together, so dropping either one forces
        // a 22 followed by re-setting whichever is still on.
        if self.bold() != other.bold() || self.dim() != other.dim() {
            attrs = attrs.intensity(self.bold(), self.dim());
        }
        if self.italic() != other.italic() {
            attrs = attrs.italic(self.italic());
        }
        if self.underline() != other.underline()
            || (self.underline() && self.underline_style != other.underline_style)
        {
            attrs = attrs.underline(self.underline().then_some(self.underline_style));
        }
        if self.inverse() != other.inverse() {
            attrs = attrs.inverse(self.inverse());
        }
        if self.strikethrough() != other.strikethrough() {
            attrs = attrs.strikethrough(self.strikethrough());
        }
        if self.blink() != other.blink() {
            attrs = attrs.blink(self.blink());
        }
        if self.conceal() != other.conceal() {
            attrs = attrs.conceal(self.conceal());
        }
        if self.overline() != other.overline() {
            attrs = attrs.overline(self.overline());
        }

        attrs.write_buf(contents);
    }

    /// Equality over the SGR-visible fields (everything but the hyperlink).
    fn sgr_eq(&self, other: &Self) -> bool {
        Self {
            hyperlink: 0,
            ..*self
        } == Self {
            hyperlink: 0,
            ..*other
        }
    }
}
