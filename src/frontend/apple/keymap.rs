//! macOS virtual key codes (HIToolbox kVK_*) and iOS USB HID keyboard usages
//! (UIKeyboardHIDUsage, page 7) to PC key names. These are physical positions;
//! the guest chooses the layout.

use super::keys::{self, InputKey};

pub fn mac_key(code: u16) -> Option<InputKey> {
    let name = match code {
        0x00 => "a",
        0x01 => "s",
        0x02 => "d",
        0x03 => "f",
        0x04 => "h",
        0x05 => "g",
        0x06 => "z",
        0x07 => "x",
        0x08 => "c",
        0x09 => "v",
        0x0b => "b",
        0x0c => "q",
        0x0d => "w",
        0x0e => "e",
        0x0f => "r",
        0x10 => "y",
        0x11 => "t",
        0x12 => "1",
        0x13 => "2",
        0x14 => "3",
        0x15 => "4",
        0x16 => "6",
        0x17 => "5",
        0x18 => "equal",
        0x19 => "9",
        0x1a => "7",
        0x1b => "minus",
        0x1c => "8",
        0x1d => "0",
        0x1e => "rightbracket",
        0x1f => "o",
        0x20 => "u",
        0x21 => "leftbracket",
        0x22 => "i",
        0x23 => "p",
        0x24 => "enter",
        0x25 => "l",
        0x26 => "j",
        0x27 => "apostrophe",
        0x28 => "k",
        0x29 => "semicolon",
        0x2a => "backslash",
        0x2b => "comma",
        0x2c => "slash",
        0x2d => "n",
        0x2e => "m",
        0x2f => "dot",
        0x30 => "tab",
        0x31 => "space",
        0x32 => "grave",
        0x33 => "backspace",
        0x35 => "escape",
        0x36 => "metaright",
        0x37 => "metaleft",
        0x38 => "leftshift",
        0x39 => "capslock",
        0x3a => "altleft",
        0x3b => "controlleft",
        0x3c => "rightshift",
        0x3d => "altright",
        0x3e => "controlright",
        0x41 => "numpaddecimal",
        0x43 => "numpadmultiply",
        0x45 => "numpadadd",
        // Keypad Clear sits where PC keyboards have Num Lock.
        0x47 => "numlock",
        0x4b => "numpaddivide",
        0x4c => "numpadenter",
        0x4e => "numpadsubtract",
        0x52 => "numpad0",
        0x53 => "numpad1",
        0x54 => "numpad2",
        0x55 => "numpad3",
        0x56 => "numpad4",
        0x57 => "numpad5",
        0x58 => "numpad6",
        0x59 => "numpad7",
        0x5b => "numpad8",
        0x5c => "numpad9",
        0x60 => "f5",
        0x61 => "f6",
        0x62 => "f7",
        0x63 => "f3",
        0x64 => "f8",
        0x65 => "f9",
        0x67 => "f11",
        0x6d => "f10",
        0x6f => "f12",
        // Help occupies the Insert position on extended keyboards.
        0x72 => "insert",
        0x73 => "home",
        0x74 => "pageup",
        0x75 => "delete",
        0x76 => "f4",
        0x77 => "end",
        0x78 => "f2",
        0x79 => "pagedown",
        0x7a => "f1",
        0x7b => "left",
        0x7c => "right",
        0x7d => "down",
        0x7e => "up",
        _ => return None,
    };
    keys::parse_named_key(name).ok()
}

pub fn hid_key(usage: u16) -> Option<InputKey> {
    let name = match usage {
        0x04 => "a",
        0x05 => "b",
        0x06 => "c",
        0x07 => "d",
        0x08 => "e",
        0x09 => "f",
        0x0a => "g",
        0x0b => "h",
        0x0c => "i",
        0x0d => "j",
        0x0e => "k",
        0x0f => "l",
        0x10 => "m",
        0x11 => "n",
        0x12 => "o",
        0x13 => "p",
        0x14 => "q",
        0x15 => "r",
        0x16 => "s",
        0x17 => "t",
        0x18 => "u",
        0x19 => "v",
        0x1a => "w",
        0x1b => "x",
        0x1c => "y",
        0x1d => "z",
        0x1e => "1",
        0x1f => "2",
        0x20 => "3",
        0x21 => "4",
        0x22 => "5",
        0x23 => "6",
        0x24 => "7",
        0x25 => "8",
        0x26 => "9",
        0x27 => "0",
        0x28 => "enter",
        0x29 => "escape",
        0x2a => "backspace",
        0x2b => "tab",
        0x2c => "space",
        0x2d => "minus",
        0x2e => "equal",
        0x2f => "leftbracket",
        0x30 => "rightbracket",
        // US backslash and the ISO key beside Enter share the PC scan code.
        0x31 | 0x32 => "backslash",
        0x33 => "semicolon",
        0x34 => "apostrophe",
        0x35 => "grave",
        0x36 => "comma",
        0x37 => "dot",
        0x38 => "slash",
        0x39 => "capslock",
        0x3a => "f1",
        0x3b => "f2",
        0x3c => "f3",
        0x3d => "f4",
        0x3e => "f5",
        0x3f => "f6",
        0x40 => "f7",
        0x41 => "f8",
        0x42 => "f9",
        0x43 => "f10",
        0x44 => "f11",
        0x45 => "f12",
        0x46 => "printscreen",
        0x47 => "scrolllock",
        0x48 => "pause",
        0x49 => "insert",
        0x4a => "home",
        0x4b => "pageup",
        0x4c => "delete",
        0x4d => "end",
        0x4e => "pagedown",
        0x4f => "right",
        0x50 => "left",
        0x51 => "down",
        0x52 => "up",
        0x53 => "numlock",
        0x54 => "numpaddivide",
        0x55 => "numpadmultiply",
        0x56 => "numpadsubtract",
        0x57 => "numpadadd",
        0x58 => "numpadenter",
        0x59 => "numpad1",
        0x5a => "numpad2",
        0x5b => "numpad3",
        0x5c => "numpad4",
        0x5d => "numpad5",
        0x5e => "numpad6",
        0x5f => "numpad7",
        0x60 => "numpad8",
        0x61 => "numpad9",
        0x62 => "numpad0",
        0x63 => "numpaddecimal",
        0x64 => "intlbackslash",
        0x65 => "menu",
        0xe0 => "controlleft",
        0xe1 => "leftshift",
        0xe2 => "altleft",
        0xe3 => "metaleft",
        0xe4 => "controlright",
        0xe5 => "rightshift",
        0xe6 => "altright",
        0xe7 => "metaright",
        _ => return None,
    };
    keys::parse_named_key(name).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_mapped_mac_key_names_a_pc_key() {
        let mapped = (0..0x80).filter_map(super::mac_key).count();
        // One per table entry: a misspelt name would map to nothing.
        assert_eq!(mapped, 100);
    }

    #[test]
    fn every_mapped_hid_key_names_a_pc_key() {
        let mapped = (0..=u16::MAX).filter_map(super::hid_key).count();
        // Usages 04-65 plus E0-E7, including both backslash usages.
        assert_eq!(mapped, 106);
        for usage in [0, 1, 3, 0x66, 0x87, 0x90, 0xe8, u16::MAX] {
            assert!(super::hid_key(usage).is_none());
        }

        // Check the added shared keys through guest INs, with and without
        // i8042 translation. Pause's release must add no bytes.
        for (mode, expected) in [
            (
                0x03,
                &[
                    0xe0, 0x7c, 0xe0, 0xf0, 0x7c, // Print Screen
                    0xe1, 0x14, 0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77, // Pause
                    0x61, 0xf0, 0x61, // ISO
                    0xe0, 0x2f, 0xe0, 0xf0, 0x2f, // Menu
                ][..],
            ),
            (
                0x43,
                &[
                    0xe0, 0x37, 0xe0, 0xb7, // Print Screen
                    0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5, // Pause
                    0x56, 0xd6, // ISO
                    0xe0, 0x5d, 0xe0, 0xdd, // Menu
                ][..],
            ),
        ] {
            let mut machine = r86::machine::Machine::new(0x100000);
            let mut boot = [0; 512];
            let code = [
                0xfa, // CLI
                0xb0, 0x60, 0xe6, 0x64, // select i8042 command byte
                0xb0, mode, 0xe6, 0x60, // set translation before injection
                0xe4, 0x64, 0xa8, 1, 0x74, 8, // poll OBF; empty -> HLT
                0xe4, 0x60, 0xba, 0xf8, 3, 0xee, // keyboard -> serial
                0xeb, 0xf2, 0xf4, // poll again; HLT
            ];
            boot[..code.len()].copy_from_slice(&code);
            boot[510..].copy_from_slice(&[0x55, 0xaa]);
            machine.load_bootsector(&boot).unwrap();
            machine.enter_bootsector();
            machine.run_steps(5).unwrap();
            for usage in [0x46, 0x48, 0x64, 0x65] {
                let key = super::hid_key(usage).unwrap();
                super::keys::inject_input_key(&mut machine, key, true);
                super::keys::inject_input_key(&mut machine, key, false);
            }
            machine.run_steps(256).unwrap();
            assert_eq!(machine.serial_output(), expected, "i8042 mode {mode:02x}");
        }
    }
}
