/*
 * USB UHCI controller emulation
 *
 * Copyright (c) 2005 Fabrice Bellard
 *
 * Copyright (c) 2008 Max Krasnyansky
 *     Magor rewrite of the UHCI data structures parser and frame processor
 *     Support for fully async operation and multiple outstanding transactions
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
// Adapted from QEMU hw/usb/hcd-uhci.c. Guest-clock scheduling and checked RAM DMA are r86-specific.
// Source commit: bb779689554ca5e69e0ac0bb56f90c41c64a80bb (MIT).

use super::tablet::{IN, OUT, Reply, SETUP, Tablet};
use crate::devices::TICKS_PER_SECOND;
use alloc::vec::Vec;

const RUN: u16 = 1;
const GLOBAL_RESET: u16 = 4;
const SUSPEND: u16 = 8;
const USBINT: u16 = 1;
const USBERR: u16 = 2;
const RESUME: u16 = 4;
const HOST_ERROR: u16 = 8;
const PROCESS_ERROR: u16 = 16;
const HALTED: u16 = 32;
const ACTIVE: u32 = 1 << 23;
const IOC: u32 = 1 << 24;
const ISO: u32 = 1 << 25;
const SPD: u32 = 1 << 29;
const NAK: u32 = 1 << 19;
const STALL: u32 = 1 << 22;
const ENABLED: u16 = 4;
const PORT_RESET: u16 = 1 << 9;
const PORT_SUSPEND: u16 = 1 << 12;
const USB_HZ: u64 = 12_000_000;

// A cache is a witness of RAM contents, never a promise that the guest will not
// edit its schedule. Validate it at every frame boundary before skipping a walk.
struct IdleFrame {
    head: u32,
    words: Vec<(u32, u32)>,
}

pub(crate) struct Uhci {
    command: u16,
    status: u16,
    interrupt: u16,
    interrupt_reason: u8,
    frame: u16,
    base: u32,
    sof: u8,
    ports: [u16; 2],
    connected: bool,
    tablet: Tablet,
    next_frame: u64,
    fraction: u64,
    bus_master: bool,
    idle: Vec<IdleFrame>,
    #[cfg(test)]
    pub(super) walks: u64,
}
impl core::fmt::Debug for Uhci {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Uhci")
            .field("command", &self.command)
            .field("status", &self.status)
            .field("interrupt", &self.interrupt)
            .field("interrupt_reason", &self.interrupt_reason)
            .field("frame", &self.frame)
            .field("base", &self.base)
            .field("sof", &self.sof)
            .field("ports", &self.ports)
            .field("tablet", &self.tablet)
            .field("next_frame", &self.next_frame)
            .field("fraction", &self.fraction)
            .finish()
    }
}
impl Default for Uhci {
    fn default() -> Self {
        Self {
            command: 0,
            status: HALTED,
            interrupt: 0,
            interrupt_reason: 0,
            frame: 0,
            base: 0,
            sof: 64,
            ports: [0x93, 0x80],
            connected: true,
            tablet: Tablet::default(),
            next_frame: u64::MAX,
            fraction: USB_HZ - 1,
            bus_master: false,
            idle: Vec::new(),
            #[cfg(test)]
            walks: 0,
        }
    }
}
impl Uhci {
    fn running(&self) -> bool {
        self.command & (RUN | GLOBAL_RESET | SUSPEND) == RUN
    }
    pub fn irq(&self) -> bool {
        self.status & (HOST_ERROR | PROCESS_ERROR) != 0
            || (self.status & USBERR != 0 && self.interrupt & 1 != 0)
            || (self.status & RESUME != 0 && self.interrupt & 2 != 0)
            || (self.interrupt_reason & ((self.interrupt >> 2) as u8) != 0)
    }
    pub fn next_event(&self) -> u64 {
        if self.bus_master {
            self.next_frame
        } else {
            u64::MAX
        }
    }
    pub fn input(&mut self, x: u16, y: u16, buttons: u8, wheel: i8) {
        let changed = self.tablet.input(x, y, buttons, wheel);
        if changed && self.connected && self.tablet.remote_wakeup && self.ports[0] & ENABLED != 0 {
            if self.ports[0] & PORT_SUSPEND != 0 {
                self.ports[0] |= 1 << 6;
            }
            if self.command & SUSPEND != 0 {
                self.command |= 1 << 4; // automatic global resume signaling
                self.status |= RESUME;
            }
        }
    }
    fn reset(&mut self, global: bool) {
        let connected = self.connected;
        let enabled = self.ports[0] & ENABLED != 0;
        let master = self.bus_master;
        let mut tablet = core::mem::take(&mut self.tablet);
        tablet.reset();
        *self = Self::default();
        self.connected = connected;
        self.bus_master = master;
        self.tablet = tablet;
        self.ports[0] = 0x80 | if connected { 0x13 } else { 0 } | if enabled { 8 } else { 0 };
        if global {
            self.command = GLOBAL_RESET;
        }
    }
    fn schedule_frame(&mut self) {
        let clocks = (11936 + u64::from(self.sof)) * TICKS_PER_SECOND + self.fraction;
        self.next_frame = self.next_frame.saturating_add(clocks / USB_HZ);
        self.fraction = clocks % USB_HZ;
    }
    fn restart_clock(&mut self, now: u64) {
        self.next_frame = now;
        self.fraction = USB_HZ - 1;
        self.schedule_frame();
    }
    pub fn read(&self, offset: u8, width: u8) -> u32 {
        let byte = |offset| match offset {
            0..=1 => (self.command >> (offset * 8)) as u8,
            2..=3 => (self.status >> ((offset - 2) * 8)) as u8,
            4..=5 => (self.interrupt >> ((offset - 4) * 8)) as u8,
            6..=7 => (self.frame >> ((offset - 6) * 8)) as u8,
            8..=11 => (self.base >> ((offset - 8) * 8)) as u8,
            12 => self.sof,
            13 => 0,
            16..=19 => (self.ports[usize::from((offset - 16) / 2)] >> ((offset & 1) * 8)) as u8,
            // A nonexistent root port must have its presence marker (bit 7)
            // clear. Windows probes consecutive PORTSC words to count ports.
            _ => {
                if offset & 1 == 0 {
                    0x7f
                } else {
                    0xff
                }
            }
        };
        let mut result = 0;
        for i in 0..width {
            result |= u32::from(byte(offset + i)) << (8 * i);
        }
        result
    }
    pub fn write(&mut self, offset: u8, width: u8, value: u32, now: u64) {
        // FRNUM and PORTSC require word writes. Other registers accept lanes;
        // preserve W1C semantics instead of reading and rewriting status bits.
        if width == 4 && offset == 8 {
            self.base = value & !4095;
            self.idle.clear();
            return;
        }
        if (offset == 6 || offset == 16 || offset == 18) && width == 2 {
            if offset == 6 {
                if self.status & HALTED != 0 {
                    self.frame = value as u16 & 0x7ff;
                }
            } else {
                self.write_port(usize::from((offset - 16) / 2), value as u16);
            }
            return;
        }
        if offset <= 5 && width <= 2 && (offset & 1) + width <= 2 {
            let shift = (offset & 1) * 8;
            let mask = if width == 2 { 0xffff } else { 0xff << shift };
            let value = (value << shift) as u16;
            match offset & !1 {
                0 => {
                    let command = (self.command & !mask) | (value & mask);
                    if command & 6 != 0 {
                        self.reset(command & GLOBAL_RESET != 0);
                        return;
                    }
                    let was_running = self.running();
                    self.command = command & 0xff;
                    if self.running() {
                        self.status &= !HALTED;
                        if !was_running {
                            self.restart_clock(now);
                        }
                    } else {
                        self.status |= HALTED;
                        self.next_frame = u64::MAX;
                    }
                }
                2 => {
                    self.status &= !(value & 0x1f);
                    if value & USBINT != 0 {
                        self.interrupt_reason = 0;
                    }
                }
                4 => self.interrupt = ((self.interrupt & !mask) | (value & mask)) & 15,
                _ => unreachable!(),
            }
        } else if offset == 12 && width == 1 {
            self.sof = value as u8 & 127;
        } else if (8..12).contains(&offset) && u16::from(offset) + u16::from(width) <= 12 {
            let shift = (offset - 8) * 8;
            let mask = ((1u32 << (width * 8)) - 1) << shift;
            self.base = ((self.base & !mask) | ((value << shift) & mask)) & !4095;
            self.idle.clear();
        }
    }
    fn write_port(&mut self, port: usize, value: u16) {
        let old = self.ports[port];
        let connected = old & 1 != 0;
        let mut new = (old & !(value & 0x0a)) & !(ENABLED | PORT_RESET | PORT_SUSPEND | 0x40);
        new |= value & (PORT_RESET | PORT_SUSPEND | 0x40);
        if connected && value & PORT_RESET == 0 {
            new |= value & ENABLED;
        }
        if value & PORT_RESET != 0 {
            new &= !(PORT_SUSPEND | 0x40);
            if port == 0 && old & PORT_RESET == 0 {
                self.tablet.reset();
            }
        }
        self.ports[port] = new;
        self.idle.clear();
    }
    #[cfg(test)]
    pub(super) fn set_connected(&mut self, connected: bool) {
        if self.connected == connected {
            return;
        }
        self.connected = connected;
        let old = self.ports[0];
        self.ports[0] = (old & !0x115) | 2 | if connected { 0x11 } else { 0 };
        if old & ENABLED != 0 {
            self.ports[0] |= 8;
        }
        self.tablet.reset();
        self.idle.clear();
    }
    fn fail(&mut self, status: u16) {
        self.status |= status | HALTED;
        self.command &= !RUN;
        self.next_frame = u64::MAX;
    }
    pub fn advance(
        &mut self,
        now: u64,
        ram: &mut [u8],
        bus_master: bool,
        mut notify: impl FnMut(u32, u8),
    ) {
        self.bus_master = bus_master;
        if !self.running() || self.next_frame > now {
            return;
        }
        // Without PCI bus mastering only the SOF/frame clock runs. Batch it.
        if !bus_master {
            let period = (11936 + u64::from(self.sof)) * TICKS_PER_SECOND;
            let count = ((u128::from(now - self.next_frame + 1) * u128::from(USB_HZ)
                - 1
                - u128::from(self.fraction))
                / u128::from(period)
                + 1) as u64;
            self.frame = self.frame.wrapping_add(count as u16) & 0x7ff;
            let clocks = u128::from(count) * u128::from(period) + u128::from(self.fraction);
            self.next_frame = self
                .next_frame
                .saturating_add((clocks / u128::from(USB_HZ)) as u64);
            self.fraction = (clocks % u128::from(USB_HZ)) as u64;
            return;
        }
        while self.next_frame <= now && self.running() {
            if self.process_frame(ram, &mut notify).is_err() {
                self.fail(HOST_ERROR);
                break;
            }
            self.frame = self.frame.wrapping_add(1) & 0x7ff;
            if self.running() {
                self.schedule_frame();
            }
        }
    }
    fn process_frame(
        &mut self,
        ram: &mut [u8],
        notify: &mut impl FnMut(u32, u8),
    ) -> Result<(), ()> {
        let head = read32(ram, self.base + u32::from(self.frame & 1023) * 4)?;
        if !self.tablet.ready(self.next_frame)
            && self.idle.iter().any(|idle| {
                idle.head == head
                    && idle
                        .words
                        .iter()
                        .all(|&(addr, value)| read32(ram, addr) == Ok(value))
            })
        {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.walks += 1;
        }
        let mut link = head;
        let mut queue = None;
        let mut visited = [0u32; 64];
        let mut visited_len = 0;
        let mut witness = Vec::new();
        let mut idle = true;
        let mut bytes = 0;
        let mut iterations = 0;
        while link & 1 == 0 && iterations < 256 && bytes < 1280 {
            iterations += 1;
            let addr = link & !15;
            if link & 2 != 0 {
                if visited[..visited_len].contains(&addr) {
                    break;
                }
                if visited_len == visited.len() {
                    idle = false;
                    break;
                }
                visited[visited_len] = addr;
                visited_len += 1;
                let horizontal = read32(ram, addr)?;
                let element = read32(ram, addr + 4)?;
                witness.push((addr, horizontal));
                witness.push((addr + 4, element));
                if element & 1 != 0 {
                    queue = None;
                    link = horizontal;
                } else {
                    queue = Some((addr, horizontal));
                    link = element;
                }
                continue;
            }
            let next = read32(ram, addr)?;
            let original = read32(ram, addr + 4)?;
            let token = read32(ram, addr + 8)?;
            let buffer = read32(ram, addr + 12)?;
            let mut control = original;
            let mut advance = false;
            let mut stop = false;
            if control & ACTIVE == 0 {
                if control & IOC != 0 {
                    self.interrupt_reason |= 1;
                    idle = false;
                }
            } else {
                let len = (((token >> 21) + 1) & 0x7ff) as usize;
                let pid = token as u8;
                let endpoint = ((token >> 15) & 15) as u8;
                if len > 1280 || !matches!(pid, IN | OUT | SETUP) || (pid == SETUP && endpoint != 0)
                {
                    self.fail(PROCESS_ERROR);
                    return Ok(());
                }
                if bytes + len > 1280 {
                    idle = false;
                    break;
                }
                bytes += len.max(1);
                let mut packet = [0u8; 1280];
                // Check the whole advertised DMA window before letting the
                // device consume state, including packets that later NAK.
                if len != 0 {
                    let range = dma_range(ram, buffer, len)?;
                    if pid != IN {
                        packet[..len].copy_from_slice(&ram[range]);
                    }
                }
                let result = if self.ports[0] & (ENABLED | PORT_RESET | PORT_SUSPEND) != ENABLED
                    || !self.connected
                    || ((token >> 8) & 127) != u32::from(self.tablet.address)
                    || control & (1 << 26) != 0
                {
                    Err(Reply::NoDevice)
                } else {
                    self.tablet
                        .packet(pid, endpoint, &mut packet[..len], self.next_frame)
                };
                match result {
                    Ok(actual) => {
                        idle = false;
                        if pid == IN && actual != 0 {
                            write(ram, buffer, &packet[..actual], notify)?;
                        }
                        control = (control & !(ACTIVE | NAK | 0x7ff))
                            | (actual.wrapping_sub(1) as u32 & 0x7ff);
                        if control & SPD != 0 && pid == IN && actual < len && queue.is_some() {
                            self.interrupt_reason |= 2;
                        } else {
                            advance = true;
                        }
                        visited_len = 0;
                    }
                    Err(Reply::Nak) => {
                        control |= NAK;
                    }
                    Err(Reply::Stall) => {
                        idle = false;
                        control = (control & !ACTIVE) | STALL;
                        self.status |= USBERR;
                    }
                    Err(Reply::Babble) => {
                        idle = false;
                        control = (control & !ACTIVE) | STALL | (1 << 20);
                        self.status |= USBERR;
                        // A babbling device is disabled by the root port.
                        self.ports[0] = (self.ports[0] & !ENABLED) | 8;
                        stop = true;
                    }
                    Err(Reply::NoDevice) => {
                        idle = false;
                        control |= 1 << 18;
                        let count = (control >> 27) & 3;
                        if count != 0 {
                            control -= 1 << 27;
                        }
                        if count == 1 {
                            control = (control & !ACTIVE) | STALL;
                            self.status |= USBERR;
                        }
                    }
                }
                if control & ISO != 0 {
                    idle = false;
                    control &= !ACTIVE;
                }
                if control & (ACTIVE | IOC) == IOC {
                    self.interrupt_reason |= 1;
                }
                if control != original {
                    write(ram, addr + 4, &control.to_le_bytes(), notify)?;
                }
            }
            witness.extend_from_slice(&[
                (addr, next),
                (addr + 4, control),
                (addr + 8, token),
                (addr + 12, buffer),
            ]);
            if stop {
                break;
            }
            if let Some((qh, horizontal)) = queue {
                if advance {
                    write(ram, qh + 4, &next.to_le_bytes(), notify)?;
                    if next & 4 != 0 && next & 1 == 0 {
                        link = next;
                        continue;
                    }
                }
                queue = None;
                link = horizontal;
            } else {
                link = next;
            }
        }
        if self.interrupt_reason != 0 {
            self.status |= USBINT;
        }
        if idle && iterations < 256 && witness.len() <= 128 {
            self.idle.retain(|item| item.head != head);
            if self.idle.len() == 32 {
                self.idle.remove(0);
            }
            self.idle.push(IdleFrame {
                head,
                words: witness,
            });
        } else {
            self.idle.clear();
        }
        Ok(())
    }
}
fn dma_range(ram: &[u8], addr: u32, len: usize) -> Result<core::ops::Range<usize>, ()> {
    let start = addr as usize;
    let end = start.checked_add(len).ok_or(())?;
    // VGA aperture and PC ROM are not PCI DMA RAM, even when RAM's backing
    // allocation also contains firmware bytes there. PCI DMA ignores A20.
    if end > ram.len() || (start < 0x100000 && end > 0xa0000) {
        return Err(());
    }
    Ok(start..end)
}
fn read32(ram: &[u8], addr: u32) -> Result<u32, ()> {
    Ok(u32::from_le_bytes(
        ram[dma_range(ram, addr, 4)?].try_into().unwrap(),
    ))
}
fn write(
    ram: &mut [u8],
    addr: u32,
    data: &[u8],
    notify: &mut impl FnMut(u32, u8),
) -> Result<(), ()> {
    let range = dma_range(ram, addr, data.len())?;
    // Writes are TD status words or tablet replies, at most 128 bytes.
    debug_assert!(data.len() <= usize::from(u8::MAX));
    notify(addr, data.len() as u8);
    ram[range].copy_from_slice(data);
    Ok(())
}
