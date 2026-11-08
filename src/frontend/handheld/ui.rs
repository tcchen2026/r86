//! A small RGB565 canvas, bitmap text and list widgets for handheld screens
//! (designed at 320x170, the CardputerZero LCD). No platform dependencies.
use super::vgafont::{FONT8, FONT16};

pub const fn rgb(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3)
}

pub const BLACK: u16 = rgb(0, 0, 0);
pub const BG: u16 = rgb(18, 22, 30);
pub const PANEL: u16 = rgb(32, 38, 50);
pub const TEXT: u16 = rgb(232, 234, 240);
pub const MUTED: u16 = rgb(140, 150, 166);
pub const ACCENT: u16 = rgb(52, 120, 246);
pub const GOOD: u16 = rgb(90, 200, 130);
pub const BAD: u16 = rgb(255, 100, 100);

/// The VGA ROM fonts: Small is 8x8 pixels, Normal 8x16.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Small,
    Normal,
}

impl Font {
    pub fn width(self, text: &str) -> i32 {
        text.chars().count() as i32 * 8
    }
    pub fn height(self) -> i32 {
        match self {
            Font::Small => 8,
            Font::Normal => 16,
        }
    }
}

/// Keys after platform mapping. Letters arrive as `Char`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiKey {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Enter,
    Back,
    Backspace,
    Char(char),
}

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct Canvas {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u16>,
}

impl Canvas {
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            pixels: vec![BLACK; (width * height) as usize],
        }
    }

    pub fn fill(&mut self, color: u16) {
        self.pixels.fill(color);
    }

    pub fn rect(&mut self, r: Rect, color: u16) {
        let (x0, y0) = (r.x.max(0), r.y.max(0));
        let (x1, y1) = ((r.x + r.w).min(self.width), (r.y + r.h).min(self.height));
        for y in y0..y1 {
            let row = (y * self.width) as usize;
            self.pixels[row + x0 as usize..row + x1.max(x0) as usize].fill(color);
        }
    }

    /// Halve brightness, for overlays drawn on top of the guest display.
    pub fn dim(&mut self, r: Rect) {
        let (x0, y0) = (r.x.max(0), r.y.max(0));
        let (x1, y1) = ((r.x + r.w).min(self.width), (r.y + r.h).min(self.height));
        for y in y0..y1 {
            let row = (y * self.width) as usize;
            for p in &mut self.pixels[row + x0 as usize..row + x1.max(x0) as usize] {
                *p = (*p >> 1) & 0x7bef;
            }
        }
    }

    /// Draw printable ASCII; other characters print as '?'. Returns the end x.
    pub fn text(&mut self, x: i32, y: i32, text: &str, color: u16, font: Font) -> i32 {
        let mut cx = x;
        for ch in text.chars() {
            let index = match ch {
                ' '..='~' => ch as usize - 0x20,
                _ => usize::from(b'?' - 0x20),
            };
            let glyph: &[u8] = match font {
                Font::Small => &FONT8[index],
                Font::Normal => &FONT16[index],
            };
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if bits << col & 0x80 != 0 {
                        self.rect(
                            Rect {
                                x: cx + col,
                                y: y + row as i32,
                                w: 1,
                                h: 1,
                            },
                            color,
                        );
                    }
                }
            }
            cx += 8;
        }
        cx
    }

    /// Scale `from` (a rectangle of an RGBA image `stride` pixels wide) into
    /// `to`. Shrinking averages each source block, which keeps text legible;
    /// enlarging repeats pixels.
    pub fn blit_rgba(&mut self, to: Rect, rgba: &[u8], stride: usize, from: Rect) {
        // Below a 2:1 reduction, averaging only blurs (320x200 games on the
        // 170-line panel); sample one pixel per span instead. Larger
        // reductions average the whole span so thin lines do not vanish.
        let sharp = from.w < 2 * to.w && from.h < 2 * to.h;
        let span = |i: i32, n: i32, start: i32, len: i32| {
            let a = start + (i64::from(i) * i64::from(len) / i64::from(n)) as i32;
            let b = start + (i64::from(i + 1) * i64::from(len) / i64::from(n)) as i32;
            let (a, b) = (a as usize, b.max(a + 1) as usize);
            if sharp {
                let mid = (a + b) / 2;
                (mid, mid + 1)
            } else {
                (a, b)
            }
        };
        let columns: Vec<(usize, usize)> =
            (0..to.w).map(|x| span(x, to.w, from.x, from.w)).collect();
        for y in 0..to.h {
            let dy = to.y + y;
            if dy < 0 || dy >= self.height {
                continue;
            }
            let (y0, y1) = span(y, to.h, from.y, from.h);
            for (x, &(x0, x1)) in columns.iter().enumerate() {
                let dx = to.x + x as i32;
                if dx < 0 || dx >= self.width {
                    continue;
                }
                let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                for sy in y0..y1 {
                    let row = &rgba[(sy * stride + x0) * 4..(sy * stride + x1) * 4];
                    for p in row.chunks_exact(4) {
                        r += u32::from(p[0]);
                        g += u32::from(p[1]);
                        b += u32::from(p[2]);
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                self.pixels[(dy * self.width + dx) as usize] =
                    rgb((r / n) as u8, (g / n) as u8, (b / n) as u8);
            }
        }
    }

    pub fn text_centered(&mut self, y: i32, text: &str, color: u16, font: Font) {
        let x = (self.width - font.width(text)) / 2;
        self.text(x, y, text, color, font);
    }

    /// Word-wrap `text` into `area`; returns the y below the last line.
    pub fn paragraph(&mut self, area: Rect, text: &str, color: u16, font: Font) -> i32 {
        let columns = (area.w / font.width("m")).max(1) as usize;
        let mut y = area.y;
        for line in wrap(text, columns) {
            if y + font.height() > area.y + area.h {
                break;
            }
            self.text(area.x, y, &line, color, font);
            y += font.height() + 2;
        }
        y
    }
}

pub fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let mut word = word.to_string();
            while word.chars().count() > columns {
                let head: String = word.chars().take(columns).collect();
                word = word.chars().skip(columns).collect();
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                lines.push(head);
            }
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > columns {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        lines.push(line);
    }
    lines
}

/// Shorten `text` to at most `columns` characters, marking the cut with '~'.
pub fn clip(text: &str, columns: usize) -> String {
    if text.chars().count() <= columns {
        return text.to_string();
    }
    let mut out: String = text.chars().take(columns.saturating_sub(1)).collect();
    out.push('~');
    out
}

pub const HEADER_H: i32 = 20;
pub const FOOTER_H: i32 = 14;
const ROW_H: i32 = 18;

/// Title bar with an optional right-aligned note (e.g. "Step 2 of 6").
pub fn header(c: &mut Canvas, title: &str, note: &str) {
    c.rect(
        Rect {
            x: 0,
            y: 0,
            w: c.width,
            h: HEADER_H,
        },
        PANEL,
    );
    let note_w = Font::Small.width(note);
    let columns = ((c.width - 16 - note_w) / 8).max(1) as usize;
    c.text(6, 2, &clip(title, columns), TEXT, Font::Normal);
    c.text(c.width - note_w - 6, 6, note, MUTED, Font::Small);
}

pub fn footer(c: &mut Canvas, hint: &str) {
    let y = c.height - FOOTER_H;
    c.rect(
        Rect {
            x: 0,
            y,
            w: c.width,
            h: FOOTER_H,
        },
        PANEL,
    );
    c.text(
        6,
        y + 3,
        &clip(hint, (c.width / 8 - 1) as usize),
        MUTED,
        Font::Small,
    );
}

pub fn body(c: &Canvas) -> Rect {
    Rect {
        x: 0,
        y: HEADER_H,
        w: c.width,
        h: c.height - HEADER_H - FOOTER_H,
    }
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub color: u16,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: String::new(),
            color: TEXT,
        }
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
    pub fn color(mut self, color: u16) -> Self {
        self.color = color;
        self
    }
}

/// A scrolling single-choice list. F/X and the arrow keys move, as in the
/// CardputerZero launcher.
pub struct Menu {
    pub items: Vec<Item>,
    pub selected: usize,
    top: usize,
}

impl Menu {
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            selected: 0,
            top: 0,
        }
    }

    pub fn select(mut self, index: usize) -> Self {
        self.selected = index.min(self.items.len().saturating_sub(1));
        self
    }

    /// Handle movement keys; returns false for keys the caller should handle.
    pub fn key(&mut self, key: UiKey) -> bool {
        let last = self.items.len().saturating_sub(1);
        self.selected = match key {
            UiKey::Up | UiKey::Char('f') => self.selected.checked_sub(1).unwrap_or(last),
            UiKey::Down | UiKey::Char('x') => {
                if self.selected >= last {
                    0
                } else {
                    self.selected + 1
                }
            }
            UiKey::PageUp => self.selected.saturating_sub(5),
            UiKey::PageDown => (self.selected + 5).min(last),
            _ => return false,
        };
        true
    }

    pub fn draw(&mut self, c: &mut Canvas, area: Rect) {
        let rows = (area.h / ROW_H).max(1) as usize;
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + rows {
            self.top = self.selected + 1 - rows;
        }
        let y0 = area.y + (area.h - rows as i32 * ROW_H) / 2;
        for (row, index) in (self.top..self.items.len().min(self.top + rows)).enumerate() {
            let item = &self.items[index];
            let y = y0 + row as i32 * ROW_H;
            let selected = index == self.selected;
            if selected {
                c.rect(
                    Rect {
                        x: area.x + 2,
                        y,
                        w: area.w - 4,
                        h: ROW_H,
                    },
                    ACCENT,
                );
            }
            let detail_w = Font::Small.width(&item.detail);
            let columns = ((area.w - 28 - detail_w) / 8).max(1) as usize;
            let color = if selected { TEXT } else { item.color };
            c.text(
                area.x + 8,
                y + 1,
                &clip(&item.label, columns),
                color,
                Font::Normal,
            );
            let detail_color = if selected { TEXT } else { MUTED };
            c.text(
                area.x + area.w - detail_w - 14,
                y + 5,
                &item.detail,
                detail_color,
                Font::Small,
            );
        }
        // Scroll marks: a small bar on the right edge.
        if self.items.len() > rows {
            let h = area.h * rows as i32 / self.items.len() as i32;
            let y = area.y + area.h * self.top as i32 / self.items.len() as i32;
            c.rect(
                Rect {
                    x: area.x + area.w - 4,
                    y,
                    w: 2,
                    h: h.max(4),
                },
                MUTED,
            );
        }
    }
}

/// Single-line text entry.
pub struct TextField {
    pub text: String,
    pub max: usize,
}

impl TextField {
    pub fn key(&mut self, key: UiKey) -> bool {
        match key {
            UiKey::Char(ch) if self.text.chars().count() < self.max => self.text.push(ch),
            UiKey::Backspace => {
                self.text.pop();
            }
            _ => return false,
        }
        true
    }

    pub fn draw(&self, c: &mut Canvas, x: i32, y: i32, w: i32) {
        c.rect(Rect { x, y, w, h: 22 }, PANEL);
        let columns = ((w - 16) / 8).max(1) as usize;
        let count = self.text.chars().count();
        let visible: String = self
            .text
            .chars()
            .skip(count.saturating_sub(columns - 1))
            .collect();
        let end = c.text(x + 6, y + 3, &visible, TEXT, Font::Normal);
        c.rect(
            Rect {
                x: end + 1,
                y: y + 3,
                w: 2,
                h: 16,
            },
            ACCENT,
        );
    }
}
