//! On-screen PC keyboard for controller-only devices. Select (or Ctrl+Alt+K)
//! shows it; the D-pad or left stick moves the selection, A holds the
//! selected key down while A is held (repeating like a PC keyboard), and B
//! hides it. Shift, Ctrl and Alt latch until the next other key is released.
//! Keys are evdev codes, sent to the guest like a physical keyboard's.
use super::keymap;
use super::ui::{ACCENT, Canvas, Font, GOOD, PANEL, Rect, TEXT};

/// (label, evdev code, width in half key units). Every row is 15 units wide
/// except the function row.
type Key = (&'static str, u16, u8);

#[rustfmt::skip]
const ROWS: [&[Key]; 6] = [
    &[("Esc", 1, 2), ("F1", 59, 2), ("F2", 60, 2), ("F3", 61, 2), ("F4", 62, 2), ("F5", 63, 2),
      ("F6", 64, 2), ("F7", 65, 2), ("F8", 66, 2), ("F9", 67, 2), ("F10", 68, 2), ("F11", 87, 2),
      ("F12", 88, 2)],
    &[("`", 41, 2), ("1", 2, 2), ("2", 3, 2), ("3", 4, 2), ("4", 5, 2), ("5", 6, 2), ("6", 7, 2),
      ("7", 8, 2), ("8", 9, 2), ("9", 10, 2), ("0", 11, 2), ("-", 12, 2), ("=", 13, 2),
      ("Bksp", 14, 4)],
    &[("Tab", 15, 3), ("Q", 16, 2), ("W", 17, 2), ("E", 18, 2), ("R", 19, 2), ("T", 20, 2),
      ("Y", 21, 2), ("U", 22, 2), ("I", 23, 2), ("O", 24, 2), ("P", 25, 2), ("[", 26, 2),
      ("]", 27, 2), ("Del", 111, 3)],
    &[("Caps", 58, 4), ("A", 30, 2), ("S", 31, 2), ("D", 32, 2), ("F", 33, 2), ("G", 34, 2),
      ("H", 35, 2), ("J", 36, 2), ("K", 37, 2), ("L", 38, 2), (";", 39, 2), ("'", 40, 2),
      ("Enter", 28, 4)],
    &[("Shift", 42, 6), ("Z", 44, 2), ("X", 45, 2), ("C", 46, 2), ("V", 47, 2), ("B", 48, 2),
      ("N", 49, 2), ("M", 50, 2), (",", 51, 2), (".", 52, 2), ("/", 53, 2), ("^", 103, 2),
      ("\\", 43, 2)],
    &[("Ctl", 29, 3), ("Alt", 56, 3), ("Space", 57, 6), ("Hom", 102, 3), ("End", 107, 3),
      ("PgU", 104, 3), ("PgD", 109, 3), ("<", 105, 2), ("v", 108, 2), (">", 106, 2)],
];

const ROW_H: i32 = 12;
const MARGIN: i32 = 2;

fn is_modifier(code: u16) -> bool {
    matches!(
        code,
        keymap::LEFT_SHIFT | keymap::LEFT_CTRL | keymap::LEFT_ALT
    )
}

/// What the guest must receive after an OSK input: (evdev code, value) with
/// values as in KeyEvent (0 release, 1 press, 2 repeat).
pub type Effects = Vec<(u16, i32)>;

#[derive(Default)]
pub struct Osk {
    row: usize,
    col: usize,
    /// The key A is holding down, if any.
    held: Option<u16>,
    latched: Vec<u16>,
}

impl Osk {
    pub fn new() -> Self {
        // Start on the letters.
        Self {
            row: 3,
            col: 1,
            ..Self::default()
        }
    }

    /// Move the selection; vertical moves keep the nearest horizontal position.
    pub fn step(&mut self, code: u16) {
        let row = ROWS[self.row];
        match code {
            keymap::LEFT => self.col = (self.col + row.len() - 1) % row.len(),
            keymap::RIGHT => self.col = (self.col + 1) % row.len(),
            keymap::UP | keymap::DOWN => {
                let (here, here_total) = center(row, self.col);
                self.row = if code == keymap::UP {
                    (self.row + ROWS.len() - 1) % ROWS.len()
                } else {
                    (self.row + 1) % ROWS.len()
                };
                let target = ROWS[self.row];
                // Exact distances: ties between staggered keys go left.
                self.col = (0..target.len())
                    .min_by_key(|&i| {
                        let (c, total) = center(target, i);
                        (c * here_total - here * total).abs()
                    })
                    .unwrap();
            }
            _ => {}
        }
    }

    /// A pressed: hold the selected key, or toggle a latched modifier.
    pub fn press(&mut self) -> Effects {
        let code = ROWS[self.row][self.col].1;
        if is_modifier(code) {
            return match self.latched.iter().position(|&c| c == code) {
                Some(i) => {
                    self.latched.remove(i);
                    vec![(code, 0)]
                }
                None => {
                    self.latched.push(code);
                    vec![(code, 1)]
                }
            };
        }
        let mut effects = self.release();
        self.held = Some(code);
        effects.push((code, 1));
        effects
    }

    /// A still held: the guest sees typematic repeats of the held key.
    pub fn repeat(&self) -> Effects {
        self.held.map(|code| vec![(code, 2)]).unwrap_or_default()
    }

    /// A released (or the keyboard hidden): release the held key, then any
    /// latched modifiers it was typed with.
    pub fn release(&mut self) -> Effects {
        let Some(code) = self.held.take() else {
            return Vec::new();
        };
        let mut effects = vec![(code, 0)];
        effects.extend(self.latched.drain(..).map(|code| (code, 0)));
        effects
    }

    /// Release everything, for hiding the keyboard.
    pub fn release_all(&mut self) -> Effects {
        let mut effects = self.release();
        effects.extend(self.latched.drain(..).map(|code| (code, 0)));
        effects
    }

    /// Forget keys the session released itself (menu, reset).
    pub fn forget(&mut self) {
        self.held = None;
        self.latched.clear();
    }

    /// Draw along the bottom of `c`; returns the covered area.
    pub fn draw(&self, c: &mut Canvas) -> Rect {
        let h = ROWS.len() as i32 * ROW_H + 2 * MARGIN;
        let area = Rect {
            x: 0,
            y: c.height - h,
            w: c.width,
            h,
        };
        c.dim(area);
        c.dim(area);
        let width = c.width - 2 * MARGIN;
        for (r, row) in ROWS.iter().enumerate() {
            let total: i32 = row.iter().map(|k| i32::from(k.2)).sum();
            let y = area.y + MARGIN + r as i32 * ROW_H;
            let mut units = 0;
            for (i, &(label, code, w)) in row.iter().enumerate() {
                let x0 = MARGIN + units * width / total;
                units += i32::from(w);
                let x1 = MARGIN + units * width / total;
                let selected = (r, i) == (self.row, self.col);
                let on = self.held == Some(code) || self.latched.contains(&code);
                let color = match (selected, on) {
                    (true, _) => ACCENT,
                    (false, true) => GOOD,
                    _ => PANEL,
                };
                let key = Rect {
                    x: x0 + 1,
                    y: y + 1,
                    w: x1 - x0 - 1,
                    h: ROW_H - 1,
                };
                c.rect(key, color);
                let tx = key.x + (key.w - Font::Small.width(label)) / 2;
                c.text(tx, key.y + 2, label, TEXT, Font::Small);
            }
        }
        area
    }
}

/// The center of key `col` as a fraction of its row's width: (numerator,
/// denominator), in quarter units.
fn center(row: &[Key], col: usize) -> (i32, i32) {
    let total: i32 = row.iter().map(|k| i32::from(k.2)).sum();
    let before: i32 = row[..col].iter().map(|k| i32::from(k.2)).sum();
    (2 * before + i32::from(row[col].2), 2 * total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_reaches_the_guest_and_its_label_fits() {
        for row in ROWS {
            let total: i32 = row.iter().map(|k| i32::from(k.2)).sum();
            for &(label, code, w) in row {
                assert!(keymap::guest_key(code).is_some(), "{label}");
                // 8-pixel glyphs on a 320-pixel panel, at most a pixel over.
                let key_w = i32::from(w) * (320 - 2 * MARGIN) / total - 1;
                assert!(Font::Small.width(label) <= key_w + 1, "{label}");
            }
        }
        for row in &ROWS[1..] {
            assert_eq!(row.iter().map(|k| u32::from(k.2)).sum::<u32>(), 30);
        }
    }

    #[test]
    fn a_holds_keys_and_modifiers_latch_until_the_next_key() {
        let mut osk = Osk::new();
        osk.row = 4; // Shift
        osk.col = 0;
        assert_eq!(osk.press(), vec![(42, 1)]);
        assert!(osk.release().is_empty());
        osk.col = 1; // Z
        assert_eq!(osk.press(), vec![(44, 1)]);
        assert_eq!(osk.repeat(), vec![(44, 2)]);
        assert_eq!(osk.release(), vec![(44, 0), (42, 0)]);
        assert!(osk.release_all().is_empty() && osk.repeat().is_empty());
    }

    #[test]
    fn vertical_moves_keep_the_column_position() {
        let mut osk = Osk::new();
        osk.row = 5;
        osk.col = 8; // Down arrow
        osk.step(keymap::UP);
        assert_eq!(ROWS[osk.row][osk.col].0, "^");
        osk.step(keymap::DOWN);
        assert_eq!(ROWS[osk.row][osk.col].0, "v");
        // D sits between E and R; the tie goes left.
        osk.row = 3;
        osk.col = 3;
        osk.step(keymap::UP);
        assert_eq!(ROWS[osk.row][osk.col].0, "E");
    }
}
