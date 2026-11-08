//! AT interrupt wiring: two 8259As and the PIIX edge/level control registers.
//!
//! Devices drive pins; only this controller latches requests and acknowledges
//! them. A device clearing its own status cannot erase a latched edge. CPU IF
//! and STI/MOV-SS shadows belong to the CPU, not to the PIC priority resolver.
//! The slave INT pin is physically connected to master IR2; ICW3 describes the
//! cascade acknowledge, not a way to move that wire. The bus uses 8086 vectors
//! (MCS-80/85 CALL instruction generation is outside this PC's hardware model).
//!
//! Consulted: tiny386 i8259.c and v86 src/rust/cpu/pic.rs; the state below is
//! independent of them. v86 agrees on edge retention and the fixed master-IR2
//! cascade; its ICW3 does not qualify the cascade acknowledge.

#[derive(Clone, Debug)]
struct Controller {
    irr: u8,
    imr: u8,
    isr: u8,
    pins: u8,
    elcr: u8,
    base: u8,
    first: u8,
    icw1: u8,
    icw3: u8,
    icw4: u8,
    init: u8,
    read_isr: bool,
    poll: bool,
    special_mask: bool,
    rotate_auto: bool,
}

impl Controller {
    fn new(base: u8, icw3: u8) -> Self {
        Self {
            irr: 0,
            imr: 0xff,
            isr: 0,
            pins: 0,
            elcr: 0,
            base,
            first: 0,
            icw1: 0x11,
            icw3,
            icw4: 1,
            init: 0,
            read_isr: false,
            poll: false,
            special_mask: false,
            rotate_auto: false,
        }
    }

    fn levels(&self) -> u8 {
        if self.icw1 & 8 != 0 { 0xff } else { self.elcr }
    }

    fn set_pin(&mut self, irq: u8, high: bool) -> bool {
        let bit = 1 << irq;
        let changed = (self.pins & bit != 0) != high;
        let rising = high && self.pins & bit == 0;
        self.pins = (self.pins & !bit) | if high { bit } else { 0 };
        if self.init != 0 {
            return changed;
        }
        if self.levels() & bit != 0 {
            self.irr = (self.irr & !bit) | (self.pins & bit);
        } else if rising {
            self.irr |= bit;
        }
        changed
    }

    fn resample_levels(&mut self) {
        let levels = self.levels();
        self.irr = (self.irr & !levels) | (self.pins & levels);
    }

    fn highest(&self, bits: u8) -> Option<u8> {
        if bits == 0 {
            return None;
        }
        Some((bits.rotate_right(u32::from(self.first)).trailing_zeros() as u8 + self.first) & 7)
    }

    fn blocking_isr(&self) -> u8 {
        self.isr & if self.special_mask { !self.imr } else { 0xff }
    }

    fn pending(&self, ignore_isr: u8) -> Option<u8> {
        if self.init != 0 {
            return None;
        }
        let irq = self.highest(self.irr & !self.imr)?;
        if let Some(active) = self.highest(self.blocking_isr() & !ignore_isr) {
            if irq.wrapping_sub(self.first) & 7 >= active.wrapping_sub(self.first) & 7 {
                return None;
            }
        }
        Some(irq)
    }

    fn acknowledge(&mut self, irq: u8) {
        if self.levels() & (1 << irq) == 0 {
            self.irr &= !(1 << irq);
        }
        if self.icw4 & 2 == 0 {
            self.isr |= 1 << irq;
        } else if self.rotate_auto {
            self.first = (irq + 1) & 7;
        }
    }

    fn write(&mut self, data: bool, value: u8) {
        if data {
            match self.init {
                2 => {
                    self.base = value & 0xf8;
                    self.init = if self.icw1 & 2 == 0 {
                        3
                    } else if self.icw1 & 1 != 0 {
                        4
                    } else {
                        0
                    };
                }
                3 => {
                    self.icw3 = value;
                    self.init = if self.icw1 & 1 != 0 { 4 } else { 0 };
                }
                4 => {
                    self.icw4 = value;
                    self.init = 0;
                }
                _ => self.imr = value,
            }
            if self.init == 0 {
                self.resample_levels();
            }
        } else if value & 0x10 != 0 {
            // ICW1 clears IMR as well as IRR/ISR. Do not reinterpret ICW2-4 as
            // masks. Retain physical levels and chipset ELCR across PIC init.
            let pins = self.pins;
            let elcr = self.elcr;
            *self = Self::new(0, 0);
            self.pins = pins;
            self.elcr = elcr;
            self.imr = 0;
            self.icw1 = value;
            self.icw4 = 0;
            self.init = 2;
        } else if value & 8 != 0 {
            if value & 2 != 0 {
                self.read_isr = value & 1 != 0;
            }
            if value & 4 != 0 {
                self.poll = true;
            }
            if value & 0x40 != 0 {
                self.special_mask = value & 0x20 != 0;
            }
        } else {
            let command = value >> 5;
            match command {
                0 | 4 => self.rotate_auto = command == 4,
                2 => {} // OCW2 no-op
                6 => self.first = (value + 1) & 7,
                _ => {
                    let irq = if command & 2 != 0 {
                        Some(value & 7)
                    } else {
                        self.highest(self.blocking_isr())
                    };
                    if let Some(irq) = irq {
                        self.isr &= !(1 << irq);
                        if command & 4 != 0 {
                            self.first = (irq + 1) & 7;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Pic {
    chips: [Controller; 2],
}

impl Pic {
    pub fn new() -> Self {
        Self {
            chips: [Controller::new(0x20, 4), Controller::new(0x28, 2)],
        }
    }

    fn cascade(&self) -> bool {
        self.chips[0].icw1 & 2 == 0 && self.chips[0].icw3 & 4 != 0
    }

    fn master_pending(&self) -> Option<u8> {
        let ignore = if self.cascade() && self.chips[0].icw4 & 0x10 != 0 {
            4
        } else {
            0
        };
        self.chips[0].pending(ignore)
    }

    fn sync_cascade(&mut self) {
        let high = self.chips[1].pending(0).is_some();
        self.chips[0].set_pin(2, high);
    }

    pub fn set_irq(&mut self, irq: u8, high: bool) -> bool {
        debug_assert!(irq < 16 && irq != 2, "IR2 is wired to the slave PIC");
        let changed = self.chips[usize::from(irq >> 3)].set_pin(irq & 7, high);
        self.sync_cascade();
        changed
    }

    #[cfg(test)]
    pub fn pulse(&mut self, irq: u8) {
        self.set_irq(irq, true);
        self.set_irq(irq, false);
    }

    pub fn pending_irq(&self) -> Option<u8> {
        self.master_pending().map(|irq| {
            if irq == 2 && self.cascade() {
                8 + self.chips[1].pending(0).unwrap_or(7)
            } else {
                irq
            }
        })
    }

    /// One x86 INTA transaction. A vanished slave request produces IRQ15
    /// without setting slave ISR7; master ISR2 still records the cascade ACK.
    pub fn acknowledge(&mut self) -> u8 {
        let Some(irq) = self.master_pending() else {
            return self.chips[0].base + 7;
        };
        self.chips[0].acknowledge(irq);
        let vector = if irq == 2 && self.cascade() {
            // Slave INT falls during acknowledge, including AEOI with another
            // eligible request waiting. A later high is a new master edge.
            self.chips[0].set_pin(2, false);
            if self.chips[1].icw3 & 7 != 2 {
                0xff // No slave responds to the cascade ID on this AT wire.
            } else if let Some(slave_irq) = self.chips[1].pending(0) {
                self.chips[1].acknowledge(slave_irq);
                self.chips[1].base + slave_irq
            } else {
                self.chips[1].base + 7
            }
        } else {
            self.chips[0].base + irq
        };
        self.sync_cascade();
        vector
    }

    pub fn read(&mut self, port: u16) -> u8 {
        let index = usize::from(port & 0x80 != 0);
        if port == 0x4d0 || port == 0x4d1 {
            return self.chips[(port & 1) as usize].elcr;
        }
        let pending = if index == 0 {
            self.master_pending()
        } else {
            self.chips[1].pending(0)
        };
        let chip = &mut self.chips[index];
        if core::mem::take(&mut chip.poll) {
            let value = if let Some(irq) = pending {
                chip.acknowledge(irq);
                0x80 | irq
            } else {
                0
            };
            if index == 1 {
                self.chips[0].set_pin(2, false);
            }
            self.sync_cascade();
            value
        } else if port & 1 != 0 {
            chip.imr
        } else if chip.read_isr {
            chip.isr
        } else {
            chip.irr
        }
    }

    pub fn write(&mut self, port: u16, value: u8) {
        if port == 0x4d0 || port == 0x4d1 {
            let index = (port & 1) as usize;
            let chip = &mut self.chips[index];
            // PIIX IRQ0,1,2,8,13 are fixed edge-triggered inputs.
            chip.elcr = value & if index == 0 { 0xf8 } else { 0xde };
            chip.resample_levels();
        } else {
            let index = usize::from(port & 0x80 != 0);
            self.chips[index].write(port & 1 != 0, value);
        }
        self.sync_cascade();
    }

    pub fn mask(&self, slave: bool) -> u8 {
        self.chips[usize::from(slave)].imr
    }
    pub fn in_service(&self, slave: bool) -> u8 {
        self.chips[usize::from(slave)].isr
    }
    pub fn vector_base(&self, slave: bool) -> u8 {
        self.chips[usize::from(slave)].base
    }
    pub fn line(&self, irq: u8) -> bool {
        self.chips[usize::from(irq >> 3)].pins & (1 << (irq & 7)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialized(master_icw4: u8, slave_icw4: u8) -> Pic {
        let mut pic = Pic::new();
        for (port, value) in [
            (0x20, 0x11),
            (0xa0, 0x11),
            (0x21, 0x20),
            (0xa1, 0x28),
            (0x21, 4),
            (0xa1, 2),
            (0x21, master_icw4),
            (0xa1, slave_icw4),
        ] {
            pic.write(port, value);
        }
        pic
    }

    #[test]
    fn nested_priority_and_special_mask_use_active_irq_priority() {
        let mut pic = initialized(1, 1);
        pic.pulse(4);
        pic.pulse(6);
        assert_eq!(pic.acknowledge(), 0x24);
        pic.pulse(5);
        assert_eq!(pic.pending_irq(), None);
        pic.pulse(1);
        assert_eq!(pic.acknowledge(), 0x21);
        pic.write(0x20, 0x20);
        assert_eq!(pic.pending_irq(), None); // IRQ4 still blocks IRQ5/6.
        pic.write(0x21, 1 << 4);
        assert_eq!(pic.pending_irq(), None); // Normal mask doesn't hide ISR4.
        pic.write(0x20, 0x68);
        assert_eq!(pic.acknowledge(), 0x25);
        pic.write(0x20, 0x20); // Non-specific EOI skips masked ISR4 in SMM.
        pic.write(0x20, 0x0b);
        assert_eq!(pic.read(0x20), 1 << 4);
        pic.write(0x20, 0x48);
        assert_eq!(pic.pending_irq(), None);
        pic.write(0x20, 0x64);
        assert_eq!(pic.acknowledge(), 0x26);
    }

    #[test]
    fn cascade_priority_and_spurious_slave_keep_master_acknowledge() {
        let mut pic = initialized(1, 1);
        pic.pulse(4);
        pic.pulse(14);
        pic.pulse(8);
        assert_eq!(pic.acknowledge(), 0x28); // Cascade precedes master IRQ4.
        assert_eq!(pic.pending_irq(), None);
        pic.write(0xa0, 0x20);
        pic.write(0x20, 0x20);
        assert_eq!(pic.acknowledge(), 0x2e);
        pic.write(0xa0, 0x20);
        pic.write(0x20, 0x20);
        assert_eq!(pic.acknowledge(), 0x24);
        pic.write(0x20, 0x20);
        pic.pulse(15);
        pic.write(0xa1, 0xff); // Slave withdraws INT after the master latched it.
        assert_eq!(pic.acknowledge(), 0x2f);
        pic.write(0x20, 0x0b);
        pic.write(0xa0, 0x0b);
        assert_eq!(pic.read(0x20), 4);
        assert_eq!(pic.read(0xa0), 0);
        pic.write(0x20, 0x20);
        assert_eq!(pic.acknowledge(), 0x27); // No request: no ISR7.
        assert_eq!(pic.read(0x20), 0);
    }

    #[test]
    fn edge_retention_and_level_redelivery_are_distinct() {
        let mut pic = initialized(1, 1);
        pic.write(0x21, 0xff);
        pic.pulse(4);
        assert_eq!(pic.pending_irq(), None);
        pic.write(0x21, 0);
        assert_eq!(pic.acknowledge(), 0x24);
        pic.set_irq(4, true); // A new edge while the handler is active is retained.
        pic.write(0x20, 0x20);
        assert_eq!(pic.acknowledge(), 0x24);
        pic.write(0x20, 0x20);
        assert_eq!(pic.pending_irq(), None); // Unchanged high isn't another edge.
        pic.set_irq(4, false);
        pic.write(0x4d0, 0xff);
        pic.write(0x4d1, 0xff);
        assert_eq!(pic.read(0x4d0), 0xf8);
        assert_eq!(pic.read(0x4d1), 0xde);
        pic.set_irq(9, true);
        for _ in 0..2 {
            assert_eq!(pic.acknowledge(), 0x29);
            assert_eq!(pic.pending_irq(), None);
            pic.write(0x20, 0x20); // Either EOI order must work.
            pic.write(0xa0, 0x20);
        }
        assert_eq!(pic.acknowledge(), 0x29);
        pic.set_irq(9, false);
        pic.write(0xa0, 0x20);
        pic.write(0x20, 0x20);
        assert_eq!(pic.pending_irq(), None);
    }

    #[test]
    fn rotation_and_auto_eoi_change_the_next_eligible_request() {
        let mut pic = initialized(3, 3);
        pic.write(0x20, 0x80); // Rotate on auto-EOI.
        pic.pulse(3);
        pic.pulse(4);
        assert_eq!(pic.acknowledge(), 0x23);
        pic.pulse(3);
        assert_eq!(pic.acknowledge(), 0x24);
        assert_eq!(pic.acknowledge(), 0x23);
        pic.write(0x20, 0x0b);
        assert_eq!(pic.read(0x20), 0);
        // Slave AEOI with multiple requests must produce a fresh cascade edge.
        pic.pulse(8);
        pic.pulse(9);
        assert_eq!(pic.acknowledge(), 0x28);
        assert_eq!(pic.acknowledge(), 0x29);
        let mut pic = initialized(1, 1);
        pic.pulse(3);
        pic.pulse(4);
        assert_eq!(pic.acknowledge(), 0x23);
        pic.write(0x20, 0xa0); // Rotate the actual non-specific EOI recipient.
        pic.pulse(3);
        assert_eq!(pic.acknowledge(), 0x24);
    }

    #[test]
    fn initialization_single_mode_and_poll_are_bus_visible() {
        let mut pic = Pic::new();
        pic.write(0x20, 0x13); // Single PIC skips ICW3.
        pic.pulse(1);
        assert_eq!(pic.pending_irq(), None);
        assert_eq!(pic.read(0x21), 0); // ICW1 cleared the old mask.
        pic.write(0x21, 0x50);
        pic.write(0x21, 1);
        pic.pulse(1);
        assert_eq!(pic.acknowledge(), 0x51);
        let mut pic = initialized(1, 1);
        pic.pulse(12);
        pic.write(0x20, 0x0c);
        assert_eq!(pic.read(0x21), 0x82); // Poll applies to the next read, either A0.
        pic.write(0xa0, 0x0c);
        assert_eq!(pic.read(0xa0), 0x84);
        pic.write(0x20, 0x0b);
        pic.write(0xa0, 0x0b);
        assert_eq!(pic.read(0x20), 4);
        assert_eq!(pic.read(0xa0), 0x10);
        pic.write(0xa0, 0x20);
        pic.write(0x20, 0x20);
        pic.write(0x20, 0x0c);
        assert_eq!(pic.read(0x20), 0);
    }

    #[test]
    fn special_fully_nested_mode_allows_only_higher_slave_priority() {
        for sfnm in [false, true] {
            let mut pic = initialized(if sfnm { 0x11 } else { 1 }, 1);
            pic.pulse(14);
            assert_eq!(pic.acknowledge(), 0x2e);
            pic.pulse(15);
            assert_eq!(pic.pending_irq(), None);
            pic.pulse(8);
            assert_eq!(pic.pending_irq(), if sfnm { Some(8) } else { None });
            if sfnm {
                assert_eq!(pic.acknowledge(), 0x28);
                pic.write(0xa0, 0x20);
                assert_eq!(pic.pending_irq(), None); // Slave ISR6 still blocks 7.
            }
        }
    }
}
