//! App VM controls shared by the native core and the Wasm core. JS only
//! transports arguments; held input, reset and PC input semantics live here.
use super::keys::{self, InputKey};
use r86::devices::Ps2ButtonState;
use r86::machine::{DisplayMode, Machine};

pub fn prepare(machine: &mut Machine, recommended: Option<DisplayMode>) {
    machine.set_recommended_display_mode(recommended);
    machine.enable_boot_splash();
}

#[derive(Default)]
pub struct Input {
    pub paused: bool,
    held: Vec<InputKey>,
    buttons: Ps2ButtonState,
}

impl Input {
    pub fn control(&mut self, machine: &mut Machine, action: u32) -> Result<(), String> {
        match action {
            0 => {
                self.paused = true;
                self.release(machine);
            }
            1 => self.paused = false,
            2 => {
                machine.reboot()?;
                self.clear();
            }
            3 => machine.press_power_button(),
            4 => {
                for name in ["controlleft", "altleft", "delete"] {
                    keys::inject_named_key(machine, name, true)?;
                }
                for name in ["delete", "altleft", "controlleft"] {
                    keys::inject_named_key(machine, name, false)?;
                }
            }
            5 => self.release(machine),
            _ => return Err("Unknown VM control".into()),
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.held.clear();
        self.buttons = Ps2ButtonState::default();
    }

    pub fn key(&mut self, machine: &mut Machine, key: InputKey, down: bool) {
        match (down, self.held.iter().position(|held| *held == key)) {
            (true, None) => self.held.push(key),
            (false, Some(index)) => {
                self.held.remove(index);
            }
            // A release the guest never saw pressed stays unsent.
            (false, None) => return,
            (true, Some(_)) => {}
        }
        keys::inject_input_key(machine, key, down);
    }

    pub fn mouse(
        &mut self,
        machine: &mut Machine,
        mut dx: i32,
        mut dy: i32,
        buttons: Ps2ButtonState,
    ) {
        self.buttons = buttons;
        // PS/2 packets carry 9-bit deltas; the device splits further.
        loop {
            let (x, y) = (dx.clamp(-255, 255), dy.clamp(-255, 255));
            machine.inject_mouse(x as i16, y as i16, buttons);
            (dx, dy) = (dx - x, dy - y);
            if dx == 0 && dy == 0 {
                break;
            }
        }
    }

    pub fn pointer(&mut self, machine: &mut Machine, x: u16, y: u16, buttons: u8, wheel: i16) {
        // The only failure, a full queue once the helper stops
        // polling, drops this transition; the PS/2 mouse takes over.
        let _ = machine.inject_pointer(x, y, buttons & 7, wheel);
    }

    pub fn release(&mut self, machine: &mut Machine) {
        for key in std::mem::take(&mut self.held) {
            keys::inject_input_key(machine, key, false);
        }
        if self.buttons != Ps2ButtonState::default() {
            self.buttons = Ps2ButtonState::default();
            machine.inject_mouse(0, 0, self.buttons);
        }
    }
}
