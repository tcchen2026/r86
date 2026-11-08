//! Native 80-column VGA text on a 320-pixel-wide panel: every character cell
//! becomes 4x6 pixels, read from the guest's text buffer instead of scaling
//! the 720x400 image. The guest still runs a standard 80x25 mode; only the
//! host draws it differently.
//!
//! Printable ASCII uses a hand-designed 3x5 font (lowercase descenders use
//! the sixth row); the blank fourth column separates letters, as the ninth
//! dot does on VGA. CP437 box drawing, blocks and shades are generated so DOS
//! windows and menus keep their frames; accented letters fall back to their
//! base letter.
//!
//! Rows are spread over up to 7 pixels when the panel has room (80x25 on
//! 170 lines gives 6.8): the extra line separates descenders from the next
//! row, and frames and blocks repeat their last line to stay connected.
use std::sync::OnceLock;

use r86::devices::TextSnapshot;

use super::ui::{Canvas, Rect};

pub const CELL_W: i32 = 4;
pub const CELL_H: i32 = 6;

/// Rows are 4 pixels wide: bit 3 is the leftmost column.
type Glyph = [u8; 6];

/// Whether `text` fits the canvas at one cell per 4x6 pixels.
pub fn fits(text: &TextSnapshot, canvas: &Canvas) -> bool {
    i32::from(text.cols) * CELL_W <= canvas.width && i32::from(text.rows) * CELL_H <= canvas.height
}

/// Draw `text` centered on `c`. `colors` are the 16 attribute colors.
pub fn draw(c: &mut Canvas, text: &TextSnapshot, colors: &[u16; 16], cursor_visible: bool) {
    let font = FONT.get_or_init(build_font);
    let (cols, rows) = (i32::from(text.cols), i32::from(text.rows));
    let x0 = (c.width - cols * CELL_W) / 2;
    let height = c.height.min(rows * (CELL_H + 1));
    let y0 = (c.height - height) / 2;
    let top = |row: i32| y0 + row * height / rows;
    for row in 0..rows {
        let (y, h) = (top(row), top(row + 1) - top(row));
        for col in 0..cols {
            let [ch, attr] = text.raw_cells[(row * cols + col) as usize];
            let fg = colors[usize::from(attr & 0x0f)];
            let bg = colors[usize::from((attr >> 4) & 0x07)];
            let x = x0 + col * CELL_W;
            c.rect(Rect { x, y, w: CELL_W, h }, bg);
            let glyph = &font[usize::from(ch)];
            let connects = (0xb0..=0xdf).contains(&ch);
            for dy in 0..h {
                let bits = match glyph.get(dy as usize) {
                    Some(&bits) => bits,
                    None if connects => glyph[5],
                    None => 0,
                };
                for dx in 0..CELL_W {
                    if bits >> (3 - dx) & 1 != 0 {
                        c.pixels[((y + dy) * c.width + x + dx) as usize] = fg;
                    }
                }
            }
        }
    }
    if let Some((col, row)) = text.cursor.filter(|_| cursor_visible) {
        let attr = text
            .raw_cells
            .get(usize::from(row) * usize::from(text.cols) + usize::from(col))
            .map_or(7, |cell| cell[1]);
        c.rect(
            Rect {
                x: x0 + i32::from(col) * CELL_W,
                y: top(i32::from(row) + 1) - 1,
                w: CELL_W,
                h: 1,
            },
            colors[usize::from(attr & 0x0f)],
        );
    }
}

static FONT: OnceLock<[Glyph; 256]> = OnceLock::new();

fn build_font() -> [Glyph; 256] {
    let mut font = [[0; 6]; 256];
    for (ch, glyph) in font.iter_mut().enumerate() {
        let ch = ch as u8;
        *glyph = if let Some(rows) = ascii(ch).or_else(|| symbol(ch)) {
            let mut g = [0; 6];
            for (out, row) in g.iter_mut().zip(rows) {
                *out = row << 1;
            }
            g
        } else if let Some(g) = block(ch).or_else(|| box_drawing(ch)) {
            g
        } else if let Some(base) = base_letter(ch) {
            let mut g = [0; 6];
            for (out, row) in g.iter_mut().zip(ascii(base).unwrap()) {
                *out = row << 1;
            }
            g
        } else {
            [0; 6]
        };
    }
    font
}

/// 3-pixel rows (bit 2 leftmost) for printable ASCII.
fn ascii(ch: u8) -> Option<[u8; 6]> {
    Some(match ch {
        b' ' => [0, 0, 0, 0, 0, 0],
        b'!' => [0b010, 0b010, 0b010, 0b000, 0b010, 0],
        b'"' => [0b101, 0b101, 0b000, 0b000, 0b000, 0],
        b'#' => [0b101, 0b111, 0b101, 0b111, 0b101, 0],
        b'$' => [0b011, 0b110, 0b010, 0b011, 0b110, 0],
        b'%' => [0b101, 0b001, 0b010, 0b100, 0b101, 0],
        b'&' => [0b010, 0b101, 0b010, 0b101, 0b011, 0],
        b'\'' => [0b010, 0b010, 0b000, 0b000, 0b000, 0],
        b'(' => [0b001, 0b010, 0b010, 0b010, 0b001, 0],
        b')' => [0b100, 0b010, 0b010, 0b010, 0b100, 0],
        b'*' => [0b000, 0b101, 0b010, 0b101, 0b000, 0],
        b'+' => [0b000, 0b010, 0b111, 0b010, 0b000, 0],
        b',' => [0b000, 0b000, 0b000, 0b010, 0b100, 0],
        b'-' => [0b000, 0b000, 0b111, 0b000, 0b000, 0],
        b'.' => [0b000, 0b000, 0b000, 0b000, 0b010, 0],
        b'/' => [0b001, 0b001, 0b010, 0b100, 0b100, 0],
        b'0' => [0b111, 0b101, 0b101, 0b101, 0b111, 0],
        b'1' => [0b010, 0b110, 0b010, 0b010, 0b111, 0],
        b'2' => [0b110, 0b001, 0b010, 0b100, 0b111, 0],
        b'3' => [0b110, 0b001, 0b010, 0b001, 0b110, 0],
        b'4' => [0b101, 0b101, 0b111, 0b001, 0b001, 0],
        b'5' => [0b111, 0b100, 0b110, 0b001, 0b110, 0],
        b'6' => [0b011, 0b100, 0b111, 0b101, 0b111, 0],
        b'7' => [0b111, 0b001, 0b010, 0b010, 0b010, 0],
        b'8' => [0b111, 0b101, 0b111, 0b101, 0b111, 0],
        b'9' => [0b111, 0b101, 0b111, 0b001, 0b110, 0],
        b':' => [0b000, 0b010, 0b000, 0b010, 0b000, 0],
        b';' => [0b000, 0b010, 0b000, 0b010, 0b100, 0],
        b'<' => [0b001, 0b010, 0b100, 0b010, 0b001, 0],
        b'=' => [0b000, 0b111, 0b000, 0b111, 0b000, 0],
        b'>' => [0b100, 0b010, 0b001, 0b010, 0b100, 0],
        b'?' => [0b110, 0b001, 0b010, 0b000, 0b010, 0],
        b'@' => [0b011, 0b101, 0b111, 0b100, 0b011, 0],
        b'A' => [0b010, 0b101, 0b111, 0b101, 0b101, 0],
        b'B' => [0b110, 0b101, 0b110, 0b101, 0b110, 0],
        b'C' => [0b011, 0b100, 0b100, 0b100, 0b011, 0],
        b'D' => [0b110, 0b101, 0b101, 0b101, 0b110, 0],
        b'E' => [0b111, 0b100, 0b110, 0b100, 0b111, 0],
        b'F' => [0b111, 0b100, 0b110, 0b100, 0b100, 0],
        b'G' => [0b011, 0b100, 0b101, 0b101, 0b011, 0],
        b'H' => [0b101, 0b101, 0b111, 0b101, 0b101, 0],
        b'I' => [0b111, 0b010, 0b010, 0b010, 0b111, 0],
        b'J' => [0b001, 0b001, 0b001, 0b101, 0b010, 0],
        b'K' => [0b101, 0b101, 0b110, 0b101, 0b101, 0],
        b'L' => [0b100, 0b100, 0b100, 0b100, 0b111, 0],
        b'M' => [0b101, 0b111, 0b111, 0b101, 0b101, 0],
        b'N' => [0b110, 0b101, 0b101, 0b101, 0b101, 0],
        b'O' => [0b010, 0b101, 0b101, 0b101, 0b010, 0],
        b'P' => [0b110, 0b101, 0b110, 0b100, 0b100, 0],
        b'Q' => [0b010, 0b101, 0b101, 0b110, 0b011, 0],
        b'R' => [0b110, 0b101, 0b110, 0b101, 0b101, 0],
        b'S' => [0b011, 0b100, 0b010, 0b001, 0b110, 0],
        b'T' => [0b111, 0b010, 0b010, 0b010, 0b010, 0],
        b'U' => [0b101, 0b101, 0b101, 0b101, 0b111, 0],
        b'V' => [0b101, 0b101, 0b101, 0b101, 0b010, 0],
        b'W' => [0b101, 0b101, 0b111, 0b111, 0b101, 0],
        b'X' => [0b101, 0b101, 0b010, 0b101, 0b101, 0],
        b'Y' => [0b101, 0b101, 0b010, 0b010, 0b010, 0],
        b'Z' => [0b111, 0b001, 0b010, 0b100, 0b111, 0],
        b'[' => [0b011, 0b010, 0b010, 0b010, 0b011, 0],
        b'\\' => [0b100, 0b100, 0b010, 0b001, 0b001, 0],
        b']' => [0b110, 0b010, 0b010, 0b010, 0b110, 0],
        b'^' => [0b010, 0b101, 0b000, 0b000, 0b000, 0],
        b'_' => [0b000, 0b000, 0b000, 0b000, 0b111, 0],
        b'`' => [0b100, 0b010, 0b000, 0b000, 0b000, 0],
        b'a' => [0b000, 0b110, 0b011, 0b101, 0b111, 0],
        b'b' => [0b100, 0b110, 0b101, 0b101, 0b110, 0],
        b'c' => [0b000, 0b011, 0b100, 0b100, 0b011, 0],
        b'd' => [0b001, 0b011, 0b101, 0b101, 0b011, 0],
        b'e' => [0b000, 0b011, 0b111, 0b100, 0b011, 0],
        b'f' => [0b001, 0b010, 0b111, 0b010, 0b010, 0],
        b'g' => [0b000, 0b011, 0b101, 0b011, 0b001, 0b110],
        b'h' => [0b100, 0b110, 0b101, 0b101, 0b101, 0],
        b'i' => [0b010, 0b000, 0b010, 0b010, 0b010, 0],
        b'j' => [0b001, 0b000, 0b001, 0b001, 0b001, 0b110],
        b'k' => [0b100, 0b101, 0b110, 0b110, 0b101, 0],
        b'l' => [0b110, 0b010, 0b010, 0b010, 0b111, 0],
        b'm' => [0b000, 0b110, 0b111, 0b101, 0b101, 0],
        b'n' => [0b000, 0b110, 0b101, 0b101, 0b101, 0],
        b'o' => [0b000, 0b010, 0b101, 0b101, 0b010, 0],
        b'p' => [0b000, 0b110, 0b101, 0b110, 0b100, 0b100],
        b'q' => [0b000, 0b011, 0b101, 0b011, 0b001, 0b001],
        b'r' => [0b000, 0b101, 0b110, 0b100, 0b100, 0],
        b's' => [0b000, 0b011, 0b100, 0b001, 0b110, 0],
        b't' => [0b010, 0b111, 0b010, 0b010, 0b011, 0],
        b'u' => [0b000, 0b101, 0b101, 0b101, 0b011, 0],
        b'v' => [0b000, 0b101, 0b101, 0b101, 0b010, 0],
        b'w' => [0b000, 0b101, 0b101, 0b111, 0b111, 0],
        b'x' => [0b000, 0b101, 0b010, 0b010, 0b101, 0],
        b'y' => [0b000, 0b101, 0b101, 0b011, 0b001, 0b110],
        b'z' => [0b000, 0b111, 0b011, 0b110, 0b111, 0],
        b'{' => [0b011, 0b010, 0b110, 0b010, 0b011, 0],
        b'|' => [0b010, 0b010, 0b010, 0b010, 0b010, 0],
        b'}' => [0b110, 0b010, 0b011, 0b010, 0b110, 0],
        b'~' => [0b000, 0b000, 0b110, 0b011, 0b000, 0],
        _ => return None,
    })
}

/// CP437 symbols common in DOS user interfaces.
fn symbol(ch: u8) -> Option<[u8; 6]> {
    Some(match ch {
        0x04 => [0b000, 0b010, 0b111, 0b010, 0b000, 0], // diamond
        0x07 | 0xf9 | 0xfa => [0, 0, 0b010, 0, 0, 0],   // bullet, middle dot
        0x09 => [0b000, 0b010, 0b101, 0b010, 0b000, 0], // circle
        0x10 => [0b100, 0b110, 0b111, 0b110, 0b100, 0], // right triangle
        0x11 => [0b001, 0b011, 0b111, 0b011, 0b001, 0], // left triangle
        0x16 => [0b000, 0b000, 0b111, 0b111, 0b000, 0], // thick bar
        0x18 => [0b010, 0b111, 0b010, 0b010, 0b010, 0], // up arrow
        0x19 => [0b010, 0b010, 0b010, 0b111, 0b010, 0], // down arrow
        0x1a => [0b000, 0b001, 0b111, 0b001, 0b000, 0], // right arrow
        0x1b => [0b000, 0b100, 0b111, 0b100, 0b000, 0], // left arrow
        0x1e => [0b000, 0b000, 0b010, 0b111, 0b000, 0], // up triangle
        0x1f => [0b000, 0b111, 0b010, 0b000, 0b000, 0], // down triangle
        0xf0 => [0b111, 0b000, 0b111, 0b000, 0b111, 0], // identical to
        0xf1 => [0b010, 0b111, 0b010, 0b000, 0b111, 0], // plus-minus
        0xf6 => [0b010, 0b000, 0b111, 0b000, 0b010, 0], // division
        0xf8 => [0b010, 0b101, 0b010, 0b000, 0b000, 0], // degree
        0xfb => [0b001, 0b001, 0b001, 0b101, 0b010, 0], // square root
        0xfe => [0b000, 0b111, 0b111, 0b111, 0b000, 0], // small square
        _ => return None,
    })
}

/// Shades and half blocks, filling the whole 4x6 cell.
fn block(ch: u8) -> Option<Glyph> {
    let pattern = |on: fn(usize, usize) -> bool| {
        let mut g = [0; 6];
        for (y, row) in g.iter_mut().enumerate() {
            for x in 0..4 {
                if on(x, y) {
                    *row |= 8 >> x;
                }
            }
        }
        g
    };
    Some(match ch {
        0xb0 => pattern(|x, y| (x + 2 * y) % 4 == 0),
        0xb1 => pattern(|x, y| (x + y) % 2 == 0),
        0xb2 => pattern(|x, y| (x + 2 * y) % 4 != 0),
        0xdb => [0xf; 6],
        0xdc => [0, 0, 0, 0xf, 0xf, 0xf],
        0xdd => [0xc; 6],
        0xde => [0x3; 6],
        0xdf => [0xf, 0xf, 0xf, 0, 0, 0],
        _ => return None,
    })
}

/// CP437 0xB3..=0xDA as line weights (up, down, left, right): 1 single, 2 double.
fn box_drawing(ch: u8) -> Option<Glyph> {
    const ARMS: [[u8; 4]; 40] = [
        [1, 1, 0, 0],
        [1, 1, 1, 0],
        [1, 1, 2, 0],
        [2, 2, 1, 0],
        [0, 2, 1, 0],
        [0, 1, 2, 0],
        [2, 2, 2, 0],
        [2, 2, 0, 0],
        [0, 2, 2, 0],
        [2, 0, 2, 0],
        [2, 0, 1, 0],
        [1, 0, 2, 0],
        [0, 1, 1, 0],
        [1, 0, 0, 1],
        [1, 0, 1, 1],
        [0, 1, 1, 1],
        [1, 1, 0, 1],
        [0, 0, 1, 1],
        [1, 1, 1, 1],
        [1, 1, 0, 2],
        [2, 2, 0, 1],
        [2, 0, 0, 2],
        [0, 2, 0, 2],
        [2, 0, 2, 2],
        [0, 2, 2, 2],
        [2, 2, 0, 2],
        [0, 0, 2, 2],
        [2, 2, 2, 2],
        [1, 0, 2, 2],
        [2, 0, 1, 1],
        [0, 1, 2, 2],
        [0, 2, 1, 1],
        [2, 0, 0, 1],
        [1, 0, 0, 2],
        [0, 1, 0, 2],
        [0, 2, 0, 1],
        [2, 2, 1, 1],
        [1, 1, 2, 2],
        [1, 0, 1, 0],
        [0, 1, 0, 1],
    ];
    let [up, down, left, right] = *ARMS.get(usize::from(ch.checked_sub(0xb3)?))?;
    // Single lines run through column 1 and row 2; doubles use 0/2 and 1/3.
    let mut g: Glyph = [0; 6];
    let columns = |weight: u8| match weight {
        1 => 0b0100,
        2 => 0b1010,
        _ => 0,
    };
    for (y, row) in g.iter_mut().enumerate() {
        if (y <= 2 && up != 0) || (y >= 2 && down != 0) {
            *row |= columns(if y <= 2 && up != 0 { up } else { down });
        }
    }
    for (weight, span) in [(left, 0b1100), (right, 0b0111)] {
        match weight {
            1 => g[2] |= span,
            2 => {
                g[1] |= span;
                g[3] |= span;
            }
            _ => {}
        }
    }
    Some(g)
}

/// Accented CP437 letters and look-alikes drawn as their base character.
fn base_letter(ch: u8) -> Option<u8> {
    const LATIN: &[u8; 48] = b"CueaaaaceeeiiiAAEaAooouuyOUcLYPfaiounNao?--??!<>";
    Some(match ch {
        0x80..=0xaf => LATIN[usize::from(ch - 0x80)],
        0xe1 => b'B',
        0xe6 => b'u',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_ascii_and_dos_frames_have_glyphs() {
        let font = build_font();
        for ch in b'!'..=b'~' {
            assert_ne!(font[usize::from(ch)], [0; 6], "{}", ch as char);
        }
        // Box corners and lines reach the cell edges so frames connect.
        assert_eq!(font[0xc4][2], 0xf);
        assert!(font[0xb3].iter().all(|row| row & 0b0100 != 0));
        assert_eq!(font[0xda][5] & 0b0100, 0b0100);
        assert_eq!(font[0x82], font[usize::from(b'e')]);
    }

    #[test]
    fn rows_use_spare_lines_and_frames_stay_connected() {
        // Column 0 is a vertical frame line, column 1 the letter 'E'.
        let mut raw_cells = vec![[b' ', 0x07]; 80 * 25];
        for row in 0..25 {
            raw_cells[row * 80] = [0xb3, 0x07];
            raw_cells[row * 80 + 1] = [b'E', 0x07];
        }
        let text = TextSnapshot {
            cols: 80,
            rows: 25,
            cursor_offset: 0,
            cells: Vec::new(),
            raw_cells,
            cursor: None,
            blink_enabled: false,
        };
        let mut c = Canvas::new(320, 170);
        let mut colors = [0; 16];
        colors[7] = 0xffff;
        draw(&mut c, &text, &colors, false);
        let lit = |x: i32, y: i32| c.pixels[(y * c.width + x) as usize] != 0;
        // The frame line (column 1 of its cell) is unbroken top to bottom.
        assert!((0..170).all(|y| lit(1, y)));
        // 170 lines / 25 rows: row 1 spans y 6..13. Its 'E' has bars at y 6
        // and 10, then two blank lines before row 2's 'E'.
        assert!(lit(4, 6) && lit(4, 10) && !lit(4, 11) && !lit(4, 12) && lit(4, 13));
    }
}
