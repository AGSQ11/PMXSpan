//! The terminal rendering model: a `vt100` screen turned into rows of styled
//! runs with resolved colours, ready to paint.

use egui::Color32;

/// Foreground/background defaults and the 16 ANSI colours.
#[derive(Clone, Debug)]
pub struct TermColors {
    pub fg: Color32,
    pub bg: Color32,
    pub ansi: [Color32; 16],
}

impl TermColors {
    pub fn dark() -> Self {
        Self {
            fg: Color32::from_rgb(0xD8, 0xDC, 0xE3),
            bg: Color32::from_rgb(0x0F, 0x11, 0x15),
            ansi: [
                Color32::from_rgb(0x1E, 0x22, 0x2A),
                Color32::from_rgb(0xE0, 0x5A, 0x57),
                Color32::from_rgb(0x3F, 0xB9, 0x50),
                Color32::from_rgb(0xD2, 0x99, 0x22),
                Color32::from_rgb(0x58, 0x8E, 0xE8),
                Color32::from_rgb(0xBC, 0x8C, 0xFF),
                Color32::from_rgb(0x39, 0xC5, 0xCF),
                Color32::from_rgb(0xC3, 0xC9, 0xD4),
                Color32::from_rgb(0x5C, 0x63, 0x70),
                Color32::from_rgb(0xFF, 0x7B, 0x78),
                Color32::from_rgb(0x56, 0xD3, 0x64),
                Color32::from_rgb(0xE3, 0xB3, 0x41),
                Color32::from_rgb(0x79, 0xB8, 0xFF),
                Color32::from_rgb(0xD2, 0xA8, 0xFF),
                Color32::from_rgb(0x56, 0xD4, 0xDD),
                Color32::from_rgb(0xF0, 0xF3, 0xF6),
            ],
        }
    }

    pub fn light() -> Self {
        Self {
            fg: Color32::from_rgb(0x1F, 0x24, 0x30),
            bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            ansi: [
                Color32::from_rgb(0x24, 0x29, 0x2F),
                Color32::from_rgb(0xCF, 0x22, 0x2E),
                Color32::from_rgb(0x11, 0x63, 0x29),
                Color32::from_rgb(0x9A, 0x67, 0x00),
                Color32::from_rgb(0x09, 0x69, 0xDA),
                Color32::from_rgb(0x82, 0x50, 0xDF),
                Color32::from_rgb(0x1B, 0x7C, 0x83),
                Color32::from_rgb(0x6E, 0x77, 0x81),
                Color32::from_rgb(0x57, 0x60, 0x6A),
                Color32::from_rgb(0xA4, 0x0E, 0x26),
                Color32::from_rgb(0x1A, 0x7F, 0x37),
                Color32::from_rgb(0x63, 0x3C, 0x01),
                Color32::from_rgb(0x21, 0x8B, 0xFF),
                Color32::from_rgb(0xA4, 0x75, 0xF9),
                Color32::from_rgb(0x31, 0x92, 0xAA),
                Color32::from_rgb(0x8C, 0x95, 0x9F),
            ],
        }
    }

    /// A colour of the xterm 256-colour palette.
    pub fn indexed(&self, i: u8) -> Color32 {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..=255 => {
                let v = 8 + (i - 232) * 10;
                Color32::from_rgb(v, v, v)
            }
        }
    }

    fn resolve(&self, c: vt100::Color, default: Color32, bold: bool) -> Color32 {
        match c {
            vt100::Color::Default => default,
            // Bold brightens the eight basic colours, as xterm does.
            vt100::Color::Idx(i) if bold && i < 8 => self.ansi[i as usize + 8],
            vt100::Color::Idx(i) => self.indexed(i),
            vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        }
    }
}

/// The paint-relevant style of a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Color32,
    pub bg: Color32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// Consecutive cells of one row with the same style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// First column.
    pub col: u16,
    /// Columns covered (wide characters count twice).
    pub width: u16,
    pub text: String,
    pub style: Style,
}

/// Resolves the style of one cell (inverse video already applied).
pub fn cell_style(cell: &vt100::Cell, colors: &TermColors) -> Style {
    let bold = cell.bold();
    let mut fg = colors.resolve(cell.fgcolor(), colors.fg, bold);
    let mut bg = colors.resolve(cell.bgcolor(), colors.bg, false);
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.dim() {
        fg = fg.linear_multiply(0.6);
    }
    Style { fg, bg, bold, italic: cell.italic(), underline: cell.underline() }
}

/// The visible rows (at the screen's current scrollback position) as runs.
/// Runs break at style changes and around every non-ASCII character, so
/// each glyph that may come from a fallback font is placed on its own cell.
pub fn rows(screen: &vt100::Screen, colors: &TermColors) -> Vec<Vec<Run>> {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let mut out = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut runs: Vec<Run> = Vec::new();
        let mut c = 0;
        while c < cols {
            let Some(cell) = screen.cell(r, c) else { break };
            if cell.is_wide_continuation() {
                c += 1;
                continue;
            }
            let style = cell_style(cell, colors);
            let text = if cell.has_contents() { cell.contents() } else { " " };
            let width = if cell.is_wide() { 2 } else { 1 };
            let ascii = text.is_ascii() && width == 1;
            match runs.last_mut() {
                Some(last) if ascii && last.style == style && last.text.is_ascii() && last.col + last.width == c => {
                    last.text.push_str(text);
                    last.width += 1;
                }
                _ => runs.push(Run { col: c, width, text: text.to_string(), style }),
            }
            c += width;
        }
        out.push(runs);
    }
    out
}

/// Plain text of a selection between two (row, col) cells of the visible
/// screen, inclusive, in reading order. Trailing blanks of each line are
/// dropped and wrapped lines are joined.
pub fn selection_text(screen: &vt100::Screen, a: (u16, u16), b: (u16, u16)) -> String {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let (_, cols) = screen.size();
    let cols = cols.get();
    let mut out = String::new();
    for r in start.0..=end.0 {
        let c0 = if r == start.0 { start.1 } else { 0 };
        let c1 = if r == end.0 { end.1.min(cols - 1) } else { cols - 1 };
        let mut line = String::new();
        let mut c = c0;
        while c <= c1 {
            if let Some(cell) = screen.cell(r, c)
                && !cell.is_wide_continuation()
            {
                line.push_str(if cell.has_contents() { cell.contents() } else { " " });
            }
            c += 1;
        }
        let wrapped = r < end.0 && screen.row_wrapped(r);
        if wrapped {
            out.push_str(&line);
        } else {
            out.push_str(line.trim_end());
            if r < end.0 {
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(std::num::NonZeroU16::new(rows).unwrap(), std::num::NonZeroU16::new(cols).unwrap(), 100)
    }

    #[test]
    fn palette() {
        let c = TermColors::dark();
        assert_eq!(c.indexed(1), c.ansi[1]);
        assert_eq!(c.indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(c.indexed(196), Color32::from_rgb(255, 0, 0));
        assert_eq!(c.indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(c.indexed(232), Color32::from_rgb(8, 8, 8));
        assert_eq!(c.indexed(255), Color32::from_rgb(238, 238, 238));
    }

    #[test]
    fn runs_carry_colors_and_attributes() {
        let colors = TermColors::dark();
        let mut p = parser(3, 20);
        p.process(b"ab\x1b[31mred\x1b[0m \x1b[1;32mB\x1b[0m\x1b[7mI\x1b[0m\x1b[38;2;1;2;3mT\x1b[48;5;196mX\x1b[0m");
        p.process(b"\r\n\x1b[3;4mit\x1b[0m");
        let rows = rows(p.screen(), &colors);
        let r0 = &rows[0];
        assert_eq!(r0[0].text, "ab");
        assert_eq!(r0[0].style.fg, colors.fg);
        assert_eq!(r0[1].text, "red");
        assert_eq!(r0[1].style.fg, colors.ansi[1]);
        assert_eq!(r0[1].col, 2);
        let bold = r0.iter().find(|r| r.text == "B").unwrap();
        assert!(bold.style.bold);
        assert_eq!(bold.style.fg, colors.ansi[10], "bold brightens green");
        let inv = r0.iter().find(|r| r.text == "I").unwrap();
        assert_eq!((inv.style.fg, inv.style.bg), (colors.bg, colors.fg));
        let t = r0.iter().find(|r| r.text == "T").unwrap();
        assert_eq!(t.style.fg, Color32::from_rgb(1, 2, 3));
        let x = r0.iter().find(|r| r.text.starts_with('X')).unwrap();
        assert_eq!(x.style.bg, Color32::from_rgb(255, 0, 0));
        let it = &rows[1][0];
        assert_eq!(it.text, "it");
        assert!(it.style.italic && it.style.underline);
        // Every row covers all columns.
        for row in &rows {
            let last = row.last().unwrap();
            assert_eq!(last.col + last.width, 20);
        }
    }

    #[test]
    fn wide_and_non_ascii_cells_get_their_own_runs() {
        let colors = TermColors::dark();
        let mut p = parser(1, 10);
        p.process("a漢bä".as_bytes());
        let rows = rows(p.screen(), &colors);
        let texts: Vec<(&str, u16, u16)> = rows[0].iter().map(|r| (r.text.as_str(), r.col, r.width)).collect();
        assert_eq!(texts[0], ("a", 0, 1));
        assert_eq!(texts[1], ("漢", 1, 2));
        assert_eq!(texts[2], ("b", 3, 1));
        assert_eq!(texts[3], ("ä", 4, 1));
    }

    #[test]
    fn selection_extracts_text() {
        let mut p = parser(3, 10);
        p.process(b"hello   \r\nworld\r\n");
        let s = p.screen();
        assert_eq!(selection_text(s, (0, 0), (1, 4)), "hello\nworld");
        assert_eq!(selection_text(s, (1, 2), (0, 1)), "ello\nwor");
        // A wrapped line is copied as one line.
        p.process(b"0123456789abc");
        let s = p.screen();
        assert_eq!(selection_text(s, (0, 0), (0, 9)), "world");
        assert_eq!(selection_text(s, (1, 0), (2, 2)), "0123456789abc");
    }
}
