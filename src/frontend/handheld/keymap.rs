//! Linux evdev key codes (input-event-codes.h) mapped to UI keys, text and
//! guest PC keys. The CardputerZero keyboard driver resolves its Fn and Sym
//! layers in the kernel, so arrows, F-keys and symbols arrive as ordinary
//! codes; Fn and Sym themselves report KEY_DVD and KEY_CD.
use super::ui::UiKey;
use crate::keys::{InputKey, parse_named_key};

pub const ESC: u16 = 1;
pub const BACKSPACE: u16 = 14;
pub const TAB: u16 = 15;
pub const ENTER: u16 = 28;
pub const LEFT_CTRL: u16 = 29;
pub const LEFT_SHIFT: u16 = 42;
pub const KEY_K: u16 = 37;
pub const KEY_M: u16 = 50;
pub const RIGHT_SHIFT: u16 = 54;
pub const LEFT_ALT: u16 = 56;
pub const SPACE: u16 = 57;
pub const KP_ENTER: u16 = 96;
pub const RIGHT_CTRL: u16 = 97;
pub const RIGHT_ALT: u16 = 100;
pub const UP: u16 = 103;
pub const PAGE_UP: u16 = 104;
pub const LEFT: u16 = 105;
pub const RIGHT: u16 = 106;
pub const DOWN: u16 = 108;
/// KEY_MENU: a controller's Start opens the r86 menu.
pub const MENU: u16 = 139;
/// KEY_KEYBOARD: a controller's L3 shows the on-screen keyboard.
pub const OSK: u16 = 374;
/// Mouse buttons, as controller shoulder buttons report them.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const PAGE_DOWN: u16 = 109;
/// CardputerZero Sym key.
pub const SYM: u16 = 383;
/// CardputerZero Fn key.
pub const FN: u16 = 389;

const LETTERS: [(u16, &str); 3] = [(16, "qwertyuiop"), (30, "asdfghjkl"), (44, "zxcvbnm")];

/// The PC key name (as in script/ws_cli.py) for an evdev code.
fn guest_name(code: u16) -> Option<&'static str> {
    const DIGITS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
    const FKEYS: [&str; 10] = ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10"];
    const LETTER_NAMES: &str = "abcdefghijklmnopqrstuvwxyz";
    for (first, row) in LETTERS {
        if let Some(ch) = row.chars().nth(code.wrapping_sub(first) as usize) {
            let index = LETTER_NAMES.find(ch).unwrap();
            return Some(&LETTER_NAMES[index..index + 1]);
        }
    }
    Some(match code {
        1 => "escape",
        2..=11 => DIGITS[usize::from(code - 2)],
        12 => "minus",
        13 => "equal",
        14 => "backspace",
        15 => "tab",
        26 => "leftbracket",
        27 => "rightbracket",
        28 => "enter",
        29 => "controlleft",
        39 => "semicolon",
        40 => "apostrophe",
        41 => "grave",
        42 => "leftshift",
        43 => "backslash",
        51 => "comma",
        52 => "dot",
        53 => "slash",
        54 => "rightshift",
        55 => "numpadmultiply",
        56 => "altleft",
        57 => "space",
        58 => "capslock",
        59..=68 => FKEYS[usize::from(code - 59)],
        69 => "numlock",
        70 => "scrolllock",
        71 => "numpad7",
        72 => "numpad8",
        73 => "numpad9",
        74 => "numpadsubtract",
        75 => "numpad4",
        76 => "numpad5",
        77 => "numpad6",
        78 => "numpadadd",
        79 => "numpad1",
        80 => "numpad2",
        81 => "numpad3",
        82 => "numpad0",
        83 => "numpaddecimal",
        87 => "f11",
        88 => "f12",
        96 => "numpadenter",
        97 => "controlright",
        98 => "numpaddivide",
        100 => "altright",
        102 => "home",
        103 => "up",
        104 => "pageup",
        105 => "left",
        106 => "right",
        107 => "end",
        108 => "down",
        109 => "pagedown",
        110 => "insert",
        111 => "delete",
        125 => "metaleft",
        126 => "metaright",
        _ => return None,
    })
}

pub fn guest_key(code: u16) -> Option<InputKey> {
    guest_name(code).map(|name| parse_named_key(name).expect("valid built-in key name"))
}

/// US-layout text for an evdev code.
pub fn text(code: u16, shift: bool) -> Option<char> {
    for (first, row) in LETTERS {
        if let Some(ch) = row.chars().nth(code.wrapping_sub(first) as usize) {
            return Some(if shift { ch.to_ascii_uppercase() } else { ch });
        }
    }
    let (plain, shifted) = match code {
        2..=11 => {
            let i = usize::from(code - 2);
            (b"1234567890"[i], b"!@#$%^&*()"[i])
        }
        12 => (b'-', b'_'),
        13 => (b'=', b'+'),
        26 => (b'[', b'{'),
        27 => (b']', b'}'),
        39 => (b';', b':'),
        40 => (b'\'', b'"'),
        41 => (b'`', b'~'),
        43 => (b'\\', b'|'),
        51 => (b',', b'<'),
        52 => (b'.', b'>'),
        53 => (b'/', b'?'),
        55 => (b'*', b'*'),
        57 => (b' ', b' '),
        74 => (b'-', b'-'),
        78 => (b'+', b'+'),
        _ => return None,
    };
    Some(char::from(if shift { shifted } else { plain }))
}

pub fn ui_key(code: u16, shift: bool) -> Option<UiKey> {
    Some(match code {
        ESC => UiKey::Back,
        ENTER | KP_ENTER => UiKey::Enter,
        BACKSPACE => UiKey::Backspace,
        UP => UiKey::Up,
        DOWN => UiKey::Down,
        LEFT => UiKey::Left,
        RIGHT => UiKey::Right,
        PAGE_UP => UiKey::PageUp,
        PAGE_DOWN => UiKey::PageDown,
        _ => UiKey::Char(text(code, shift)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_code_names_a_supported_pc_key() {
        for code in 0..512 {
            if guest_name(code).is_some() {
                assert!(guest_key(code).is_some(), "{code}");
            }
        }
        assert_eq!(guest_name(30), Some("a"));
        assert_eq!(guest_name(50), Some("m"));
        assert_eq!(guest_name(25), Some("p"));
        assert_eq!(guest_name(68), Some("f10"));
        assert_eq!(text(3, true), Some('@'));
        assert_eq!(text(44, true), Some('Z'));
    }
}
