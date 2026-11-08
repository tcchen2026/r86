//! PC keys by name, shared by scripted, WebSocket and device frontends. Names
//! are case-insensitive and documented in script/ws_cli.py.
use r86::devices::Ps2Key;
use r86::machine::Machine;

pub fn parse_ps2_key(name: &str) -> Result<Ps2Key, String> {
    let normalized = name.trim().to_ascii_lowercase();
    let key = match normalized.as_str() {
        "enter" | "return" => Ps2Key::Enter,
        "space" => Ps2Key::Space,
        "backspace" => Ps2Key::Backspace,
        "tab" => Ps2Key::Tab,
        "escape" | "esc" => Ps2Key::Escape,
        "leftshift" | "shiftleft" | "lshift" => Ps2Key::LeftShift,
        "rightshift" | "shiftright" | "rshift" => Ps2Key::RightShift,
        "a" => Ps2Key::A,
        "b" => Ps2Key::B,
        "c" => Ps2Key::C,
        "d" => Ps2Key::D,
        "e" => Ps2Key::E,
        "f" => Ps2Key::F,
        "g" => Ps2Key::G,
        "h" => Ps2Key::H,
        "i" => Ps2Key::I,
        "j" => Ps2Key::J,
        "k" => Ps2Key::K,
        "l" => Ps2Key::L,
        "m" => Ps2Key::M,
        "n" => Ps2Key::N,
        "o" => Ps2Key::O,
        "p" => Ps2Key::P,
        "q" => Ps2Key::Q,
        "r" => Ps2Key::R,
        "s" => Ps2Key::S,
        "t" => Ps2Key::T,
        "u" => Ps2Key::U,
        "v" => Ps2Key::V,
        "w" => Ps2Key::W,
        "x" => Ps2Key::X,
        "y" => Ps2Key::Y,
        "z" => Ps2Key::Z,
        "0" | "num0" => Ps2Key::Num0,
        "1" | "num1" => Ps2Key::Num1,
        "2" | "num2" => Ps2Key::Num2,
        "3" | "num3" => Ps2Key::Num3,
        "4" | "num4" => Ps2Key::Num4,
        "5" | "num5" => Ps2Key::Num5,
        "6" | "num6" => Ps2Key::Num6,
        "7" | "num7" => Ps2Key::Num7,
        "8" | "num8" => Ps2Key::Num8,
        "9" | "num9" => Ps2Key::Num9,
        "minus" | "-" => Ps2Key::Minus,
        "equal" | "=" => Ps2Key::Equal,
        "leftbracket" | "[" => Ps2Key::LeftBracket,
        "rightbracket" | "]" => Ps2Key::RightBracket,
        "backslash" | "\\" => Ps2Key::Backslash,
        "semicolon" | ";" => Ps2Key::Semicolon,
        "apostrophe" | "'" => Ps2Key::Apostrophe,
        "grave" | "`" => Ps2Key::Grave,
        "comma" | "," => Ps2Key::Comma,
        "dot" | "period" | "." => Ps2Key::Dot,
        "slash" | "/" => Ps2Key::Slash,
        _ => return Err(format!("unsupported PS/2 key {name}")),
    };
    Ok(key)
}

// Symbolic extended keys use the same set-2 make/break sequences as SDL.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKey {
    Basic(Ps2Key),
    Set2(bool, u8),
}
pub fn parse_named_key(name: &str) -> Result<InputKey, String> {
    if let Ok(key) = parse_ps2_key(name) {
        return Ok(InputKey::Basic(key));
    }
    let (extended, code) = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "controlleft" => (false, 0x14),
        "controlright" => (true, 0x14),
        "alt" | "altleft" => (false, 0x11),
        "altright" => (true, 0x11),
        "metaleft" => (true, 0x1f),
        "metaright" => (true, 0x27),
        "capslock" => (false, 0x58),
        "numlock" => (false, 0x77),
        "scrolllock" => (false, 0x7e),
        "f1" => (false, 0x05),
        "f2" => (false, 0x06),
        "f3" => (false, 0x04),
        "f4" => (false, 0x0c),
        "f5" => (false, 0x03),
        "f6" => (false, 0x0b),
        "f7" => (false, 0x83),
        "f8" => (false, 0x0a),
        "f9" => (false, 0x01),
        "f10" => (false, 0x09),
        "f11" => (false, 0x78),
        "f12" => (false, 0x07),
        "up" => (true, 0x75),
        "down" => (true, 0x72),
        "left" => (true, 0x6b),
        "right" => (true, 0x74),
        "home" => (true, 0x6c),
        "end" => (true, 0x69),
        "pageup" => (true, 0x7d),
        "pagedown" => (true, 0x7a),
        "insert" => (true, 0x70),
        "delete" => (true, 0x71),
        "numpadenter" => (true, 0x5a),
        "numpaddivide" => (true, 0x4a),
        "numpadmultiply" => (false, 0x7c),
        "numpadsubtract" => (false, 0x7b),
        "numpadadd" => (false, 0x79),
        "numpad0" => (false, 0x70),
        "numpad1" => (false, 0x69),
        "numpad2" => (false, 0x72),
        "numpad3" => (false, 0x7a),
        "numpad4" => (false, 0x6b),
        "numpad5" => (false, 0x73),
        "numpad6" => (false, 0x74),
        "numpad7" => (false, 0x6c),
        "numpad8" => (false, 0x75),
        "numpad9" => (false, 0x7d),
        "numpaddecimal" => (false, 0x71),
        _ => return Err(format!("unsupported PS/2 key {name}")),
    };
    Ok(InputKey::Set2(extended, code))
}
pub fn inject_input_key(machine: &mut Machine, key: InputKey, down: bool) {
    let (extended, code) = match key {
        InputKey::Basic(key) => {
            machine.inject_key(key, down);
            return;
        }
        InputKey::Set2(extended, code) => (extended, code),
    };
    let mut bytes = Vec::with_capacity(3);
    if extended {
        bytes.push(0xe0);
    }
    if !down {
        bytes.push(0xf0);
    }
    bytes.push(code);
    machine.inject_scancode_set2(&bytes);
}

pub fn inject_named_key(machine: &mut Machine, name: &str, down: bool) -> Result<(), String> {
    inject_input_key(machine, parse_named_key(name)?, down);
    Ok(())
}
