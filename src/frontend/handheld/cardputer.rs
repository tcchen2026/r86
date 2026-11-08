//! The M5Stack CardputerZero keyboard (evdev name "tca8418c") for a PC guest.
//! M5's driver resolves its Sym and Fn layers and sticky modifiers itself
//! (hold, one-shot tap, double-tap lock), and r86 keeps that. It changes only
//! what a PC cannot read:
//! - Sym symbols arrive as stand-in codes that M5's own software maps to
//!   characters; they become the PC key for the printed symbol, with Shift
//!   where a PC needs it.
//! - Sym+F/X/Z/C, which M5 leaves unassigned, move the mouse; Sym+Enter and
//!   Sym+Space are the left and right buttons. The four mouse keys report
//!   only scan records (Linux drops KEY_RESERVED), one per press and one per
//!   release, which are paired here. A scan without its own key event is
//!   such a record even when the driver syncs it together with other keys,
//!   as it does for the release that ends a one-shot Sym.
//!
//! The keyboard sends no autorepeat; lcd::Keyboard repeats held keys.
use std::time::Instant;

use super::device::KeyEvent;
use super::keymap;

pub const NAME: &str = "tca8418c";

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_MSC: u16 = 4;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const MSC_SCAN: u16 = 4;

/// M5 Sym code -> (PC key, with Shift), from M5's character map
/// (m5stack/m5stack-linux-dtoverlays 4f7aeee,
/// modules/tca8418-1.0/tca8418_keypad_m5stack_keymap.map). The base layer
/// has no punctuation keys, so these codes only ever mean Sym symbols.
#[rustfmt::skip]
const SYMBOLS: [(u16, u16, bool); 32] = [
    (26, 2, true),    // Sym+1 !
    (27, 3, true),    // Sym+2 @
    (39, 4, true),    // Sym+3 #
    (40, 5, true),    // Sym+4 $
    (41, 6, true),    // Sym+5 %
    (43, 7, true),    // Sym+6 ^
    (51, 8, true),    // Sym+7 &
    (52, 9, true),    // Sym+8 *
    (53, 10, true),   // Sym+9 (
    (94, 11, true),   // Sym+0 )
    (55, 41, true),   // Sym+Q ~
    (69, 41, false),  // Sym+W `
    (70, 12, true),   // Sym+E _
    (71, 12, false),  // Sym+R -
    (72, 13, true),   // Sym+T +
    (73, 13, false),  // Sym+Y =
    (74, 26, false),  // Sym+U [
    (75, 27, false),  // Sym+I ]
    (76, 26, true),   // Sym+O {
    (77, 27, true),   // Sym+P }
    (79, 39, false),  // Sym+A ;
    (80, 39, true),   // Sym+S :
    (81, 40, false),  // Sym+D '
    (82, 40, true),   // Sym+G "
    (83, 51, true),   // Sym+H <
    (85, 52, true),   // Sym+J >
    (86, 43, false),  // Sym+K \
    (89, 43, true),   // Sym+L |
    (90, 51, false),  // Sym+V ,
    (91, 52, false),  // Sym+B .
    (92, 53, false),  // Sym+N /
    (93, 53, true),   // Sym+M ?
];

/// Matrix scans (row * 16 + column) of the Sym-layer mouse keys: F, X, Z, C.
const MOUSE_SCANS: [i32; 4] = [0x23, 0x33, 0x32, 0x34];
/// Pointer speed in PS/2 counts per second, ramping up while held.
const MOUSE_START: f32 = 60.0;
const MOUSE_MAX: f32 = 360.0;
const MOUSE_RAMP: f32 = 0.8;

#[derive(Default)]
pub struct Keys {
    /// M5's logical Sym state (held, one-shot or locked).
    sym: bool,
    /// The last scan record not yet followed by its key event.
    scan: Option<i32>,
    /// Translated presses: (M5 code, PC code, with Shift), for their release.
    held: Vec<(u16, u16, bool)>,
    /// Mouse keys down: up, down, left, right.
    mouse: [bool; 4],
    moving_since: Option<Instant>,
    last: Option<Instant>,
    rest: (f32, f32),
}

impl Keys {
    /// One input_event from the keyboard; translated key events go to `out`.
    pub fn event(&mut self, kind: u16, code: u16, value: i32, out: &mut Vec<KeyEvent>) {
        match (kind, code) {
            (EV_MSC, MSC_SCAN) => {
                if let Some(scan) = self.scan.replace(value) {
                    self.scan_only(scan);
                }
            }
            (EV_KEY, _) => {
                self.scan = None;
                self.key(code, value, out);
            }
            (EV_SYN, SYN_REPORT) => {
                if let Some(scan) = self.scan.take() {
                    self.scan_only(scan);
                }
            }
            // Events were lost: no pairing or modifier state can be trusted.
            (EV_SYN, SYN_DROPPED) => {
                self.scan = None;
                self.sym = false;
                self.mouse = [false; 4];
                for (_, pc, shift) in self.held.drain(..) {
                    out.push(KeyEvent { code: pc, value: 0 });
                    if shift {
                        out.push(KeyEvent {
                            code: keymap::LEFT_SHIFT,
                            value: 0,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// A press or release of a key the driver reports by scan only.
    fn scan_only(&mut self, scan: i32) {
        if let Some(i) = MOUSE_SCANS.iter().position(|&s| s == scan) {
            self.mouse[i] = !self.mouse[i];
        }
    }

    fn key(&mut self, code: u16, value: i32, out: &mut Vec<KeyEvent>) {
        if code == keymap::SYM {
            self.sym = value != 0;
            return;
        }
        let event = |code, value| KeyEvent { code, value };
        // A release follows its press's translation, even if Sym changed since.
        if value == 0
            && let Some(i) = self.held.iter().position(|h| h.0 == code)
        {
            let (_, pc, shift) = self.held.remove(i);
            out.push(event(pc, 0));
            if shift {
                out.push(event(keymap::LEFT_SHIFT, 0));
            }
            return;
        }
        let translated = match code {
            _ if value != 1 => None,
            keymap::ENTER if self.sym => Some((keymap::BTN_LEFT, false)),
            keymap::SPACE if self.sym => Some((keymap::BTN_RIGHT, false)),
            _ => SYMBOLS.iter().find(|s| s.0 == code).map(|s| (s.1, s.2)),
        };
        match translated {
            Some((pc, shift)) => {
                self.held.push((code, pc, shift));
                if shift {
                    out.push(event(keymap::LEFT_SHIFT, 1));
                }
                out.push(event(pc, 1));
            }
            None => out.push(event(code, value)),
        }
    }

    pub fn moving(&self) -> bool {
        self.mouse.contains(&true)
    }

    /// Pointer motion since the last call, accelerating while keys are held.
    pub fn pointer_motion(&mut self) -> (i32, i32) {
        let now = Instant::now();
        let dt = self
            .last
            .replace(now)
            .map_or(0.0, |last| now.duration_since(last).as_secs_f32().min(0.05));
        if !self.moving() {
            self.moving_since = None;
            self.rest = (0.0, 0.0);
            return (0, 0);
        }
        let since = *self.moving_since.get_or_insert(now);
        let ramp = (now.duration_since(since).as_secs_f32() / MOUSE_RAMP).min(1.0);
        let step = (MOUSE_START + (MOUSE_MAX - MOUSE_START) * ramp) * dt;
        let [up, down, left, right] = self.mouse.map(f32::from);
        let x = self.rest.0 + (right - left) * step;
        let y = self.rest.1 + (down - up) * step;
        self.rest = (x.fract(), y.fract());
        (x.trunc() as i32, y.trunc() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(keys: &mut Keys, frames: &[(Option<i32>, Option<(u16, i32)>)]) -> Vec<(u16, i32)> {
        let mut out = Vec::new();
        for &(scan, key) in frames {
            if let Some(scan) = scan {
                keys.event(EV_MSC, MSC_SCAN, scan, &mut out);
            }
            if let Some((code, value)) = key {
                keys.event(EV_KEY, code, value, &mut out);
            }
            keys.event(EV_SYN, SYN_REPORT, 0, &mut out);
        }
        out.iter().map(|e| (e.code, e.value)).collect()
    }

    #[test]
    fn sym_symbols_become_pc_keys_with_shift() {
        let mut keys = Keys::default();
        // Sym held, 2 (sent as KEY_RIGHTBRACE) means @: Shift+2.
        let out = feed(
            &mut keys,
            &[
                (Some(0x43), Some((keymap::SYM, 1))),
                (Some(0x00), Some((27, 1))),
                (Some(0x43), Some((keymap::SYM, 0))),
                (Some(0x00), Some((27, 0))),
            ],
        );
        assert_eq!(out, [(42, 1), (3, 1), (3, 0), (42, 0)]);
        // Sym+R (KP7) is -, unshifted; plain letters pass through.
        let out = feed(
            &mut keys,
            &[(Some(0x13), Some((71, 1))), (Some(0x20), Some((30, 1)))],
        );
        assert_eq!(out, [(12, 1), (30, 1)]);
    }

    #[test]
    fn sym_mouse_keys_pair_scan_records_and_click() {
        let mut keys = Keys::default();
        let out = feed(
            &mut keys,
            &[(Some(0x43), Some((keymap::SYM, 1))), (Some(0x23), None)],
        );
        assert!(out.is_empty() && keys.mouse == [true, false, false, false]);
        feed(&mut keys, &[(Some(0x23), None)]);
        assert!(!keys.moving());
        let out = feed(
            &mut keys,
            &[
                (Some(0x29), Some((keymap::ENTER, 1))),
                (Some(0x29), Some((keymap::ENTER, 0))),
            ],
        );
        assert_eq!(out, [(keymap::BTN_LEFT, 1), (keymap::BTN_LEFT, 0)]);
        // The release that ends a one-shot Sym comes in one frame with Sym's.
        let mut out = Vec::new();
        keys.event(EV_MSC, MSC_SCAN, 0x23, &mut out);
        keys.event(EV_SYN, SYN_REPORT, 0, &mut out);
        assert!(keys.moving());
        keys.event(EV_MSC, MSC_SCAN, 0x23, &mut out);
        keys.event(EV_MSC, MSC_SCAN, 0x43, &mut out);
        keys.event(EV_KEY, keymap::SYM, 0, &mut out);
        keys.event(EV_SYN, SYN_REPORT, 0, &mut out);
        assert!(!keys.moving() && out.is_empty());
        // Without Sym, Enter is Enter.
        let out = feed(
            &mut keys,
            &[
                (Some(0x43), Some((keymap::SYM, 0))),
                (Some(0x29), Some((28, 1))),
            ],
        );
        assert_eq!(out, [(28, 1)]);
    }
}
