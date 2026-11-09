//! RTL8029 / NE2000: 16 KiB packet RAM, DP8390 registers and remote DMA.
//! Frames exclude the Ethernet FCS. `receive` returns false only for receiver
//! backpressure; the caller retains that frame. TX completes when taken by the
//! frontend. No host sockets or clocks live in this device.
use alloc::vec;
use alloc::vec::Vec;

pub const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
pub const IRQ: u8 = 11; // PCI 00:03.0 INTA; firmware/acpi/dsdt.asl.
pub const MAX_FRAME: usize = 1514;

#[derive(Debug)]
pub struct Ne2000 {
    mem: Vec<u8>,
    command: u8,
    start: u8,
    stop: u8,
    boundary: u8,
    current: u8,
    tx_page: u8,
    tx_count: u16,
    remote: u16,
    remaining: u16,
    isr: u8,
    imr: u8,
    rcr: u8,
    tcr: u8,
    dcr: u8,
    tsr: u8,
    rsr: u8,
    physical: [u8; 6],
    multicast: [u8; 8],
    tx: Option<Vec<u8>>,
}

impl Default for Ne2000 {
    fn default() -> Self {
        let mut mem = vec![0; 0x8000];
        for (i, byte) in MAC.into_iter().enumerate() {
            mem[i * 2] = byte;
            mem[i * 2 + 1] = byte;
        }
        mem[28..32].fill(0x57);
        Self {
            mem,
            command: 0x21,
            start: 0,
            stop: 0,
            boundary: 0,
            current: 0,
            tx_page: 0,
            tx_count: 0,
            remote: 0,
            remaining: 0,
            isr: 0x80,
            imr: 0,
            rcr: 0,
            tcr: 0,
            dcr: 0,
            tsr: 0,
            rsr: 0,
            physical: MAC,
            multicast: [0; 8],
            tx: None,
        }
    }
}

impl Ne2000 {
    pub fn irq(&self) -> bool {
        self.isr & self.imr & 0x7f != 0
    }

    pub fn take_transmit(&mut self) -> Option<Vec<u8>> {
        let frame = self.tx.take()?;
        self.command &= !4;
        self.tsr = 1;
        self.isr |= 2;
        Some(frame)
    }

    pub fn receive(&mut self, frame: &[u8]) -> bool {
        if !(14..=MAX_FRAME).contains(&frame.len()) {
            return true;
        }
        if self.command & 1 != 0 || self.command & 2 == 0 {
            return false;
        }
        if self.rcr & 0x20 != 0 {
            return true;
        } // monitor mode
        if self.rcr & 0x10 == 0 {
            let matches = if frame[..6] == [0xff; 6] {
                self.rcr & 4 != 0
            } else if frame[0] & 1 != 0 {
                let mut crc = u32::MAX;
                for &byte in &frame[..6] {
                    for bit in 0..8 {
                        let carry = (crc >> 31) ^ u32::from((byte >> bit) & 1);
                        crc <<= 1;
                        if carry != 0 {
                            crc ^= 0x04c1_1db7;
                        }
                    }
                }
                let hash = (crc >> 26) as usize;
                self.rcr & 8 != 0 && self.multicast[hash / 8] & (1 << (hash % 8)) != 0
            } else {
                frame[..6] == self.physical
            };
            if !matches {
                return true;
            }
        }
        // Ring pointers are guest input. Validate once before indexing RAM.
        if self.start < 0x40
            || self.stop > 0x80
            || self.start >= self.stop
            || !(self.start..self.stop).contains(&self.current)
            || !(self.start..self.stop).contains(&self.boundary)
        {
            return false;
        }
        let pages = usize::from(self.stop - self.start);
        let free = (usize::from(self.boundary) + pages - usize::from(self.current)) % pages;
        let len = frame.len().max(60);
        let needed = (len + 8).div_ceil(256); // header + frame + FCS
        if needed >= free {
            return false;
        }
        let next = self.start + ((usize::from(self.current - self.start) + needed) % pages) as u8;
        self.rsr = 1 | if frame[0] & 1 != 0 { 0x20 } else { 0 };
        let count = (len + 4) as u16;
        let header = [self.rsr, next, count as u8, (count >> 8) as u8];
        let mut at = usize::from(self.current) << 8;
        let mut crc = u32::MAX;
        for byte in header {
            self.ring_byte(&mut at, byte);
        }
        for i in 0..len {
            let byte = frame.get(i).copied().unwrap_or(0);
            self.ring_byte(&mut at, byte);
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        for byte in (!crc).to_le_bytes() {
            self.ring_byte(&mut at, byte);
        }
        self.current = next;
        self.isr |= 1;
        true
    }

    fn ring_byte(&mut self, at: &mut usize, byte: u8) {
        self.mem[*at] = byte;
        *at += 1;
        if *at == usize::from(self.stop) << 8 {
            *at = usize::from(self.start) << 8;
        }
    }

    fn dma_width(&self, width: u8) -> u16 {
        if width == 4 {
            4
        } else if self.dcr & 1 != 0 {
            2
        } else {
            1
        }
    }

    fn dma_advance(&mut self, width: u16) {
        self.remote = self.remote.wrapping_add(width);
        if self.remote == u16::from(self.stop) << 8 {
            self.remote = u16::from(self.start) << 8;
        }
        self.remaining = self.remaining.saturating_sub(width);
        if self.remaining == 0 {
            self.isr |= 0x40;
        }
    }

    pub fn read(&mut self, offset: u8, width: u8) -> Option<u32> {
        if offset == 0x10 && matches!(width, 1 | 2 | 4) {
            let n = self.dma_width(width);
            let mut result = u32::MAX;
            if self.remaining != 0 {
                for i in 0..n {
                    let at = usize::from(self.remote.wrapping_add(i));
                    let byte = if at < 32 || (0x4000..0x8000).contains(&at) {
                        self.mem[at]
                    } else {
                        0xff
                    };
                    result = (result & !(0xff << (i * 8))) | (u32::from(byte) << (i * 8));
                }
                self.dma_advance(n);
            }
            return Some(result);
        }
        if width != 1 {
            return None;
        }
        let value = match offset {
            0 => self.command,
            0x1f => {
                self.command = 0x21;
                self.isr = 0x80;
                self.tx = None;
                0
            }
            1..=15 => match (self.command >> 6, offset) {
                (0, 3) => self.boundary,
                (0, 4) => self.tsr,
                (0, 7) => self.isr,
                (0, 8) => self.remote as u8,
                (0, 9) => (self.remote >> 8) as u8,
                (0, 10) => 0x50,
                (0, 11) => 0x43,
                (0, 12) => self.rsr,
                (1, 1..=6) => self.physical[usize::from(offset - 1)],
                (1, 7) => self.current,
                (1, 8..=15) => self.multicast[usize::from(offset - 8)],
                (2, 1) => self.start,
                (2, 2) => self.stop,
                (2, 3) => self.boundary,
                (2, 4) => self.tx_page,
                (2, 12) => self.rcr,
                (2, 13) => self.tcr,
                (2, 14) => self.dcr,
                (2, 15) => self.imr,
                (3, 5 | 6) => 0x40, // 10baseT, full duplex
                _ => 0,
            },
            _ => 0xff,
        };
        Some(u32::from(value))
    }

    pub fn write(&mut self, offset: u8, width: u8, value: u32) -> bool {
        if offset == 0x10 && matches!(width, 1 | 2 | 4) {
            if self.remaining != 0 {
                let n = self.dma_width(width);
                for i in 0..n {
                    let at = usize::from(self.remote.wrapping_add(i));
                    if (0x4000..0x8000).contains(&at) {
                        self.mem[at] = (value >> (i * 8)) as u8;
                    }
                }
                self.dma_advance(n);
            }
            return true;
        }
        if width != 1 {
            return false;
        }
        let byte = value as u8;
        if offset == 0 {
            self.command = byte;
            if byte & 1 != 0 {
                self.isr |= 0x80;
            } else {
                self.isr &= !0x80;
                if byte & 0x18 != 0 && self.remaining == 0 {
                    self.isr |= 0x40;
                }
                if byte & 4 != 0 && self.tx.is_none() {
                    let at = usize::from(self.tx_page) << 8;
                    let len = usize::from(self.tx_count);
                    if at >= 0x4000 && at + len <= 0x8000 && (14..=MAX_FRAME).contains(&len) {
                        let frame = self.mem[at..at + len].to_vec();
                        if self.tcr & 6 != 0 {
                            self.receive(&frame);
                            self.command &= !4;
                            self.tsr = 1;
                            self.isr |= 2;
                        } else {
                            self.tx = Some(frame);
                        }
                    } else {
                        self.command &= !4;
                        self.tsr = 8;
                        self.isr |= 8;
                    }
                }
            }
        } else {
            match (self.command >> 6, offset) {
                (0, 1) => self.start = byte,
                (0, 2) => self.stop = byte,
                (0, 3) => self.boundary = byte,
                (0, 4) => self.tx_page = byte,
                (0, 5) => self.tx_count = (self.tx_count & 0xff00) | u16::from(byte),
                (0, 6) => self.tx_count = (self.tx_count & 0xff) | (u16::from(byte) << 8),
                (0, 7) => self.isr &= !(byte & 0x7f),
                (0, 8) => self.remote = (self.remote & 0xff00) | u16::from(byte),
                (0, 9) => self.remote = (self.remote & 0xff) | (u16::from(byte) << 8),
                (0, 10) => self.remaining = (self.remaining & 0xff00) | u16::from(byte),
                (0, 11) => self.remaining = (self.remaining & 0xff) | (u16::from(byte) << 8),
                (0, 12) => self.rcr = byte,
                (0, 13) => self.tcr = byte,
                (0, 14) => self.dcr = byte,
                (0, 15) => self.imr = byte & 0x7f,
                (1, 1..=6) => self.physical[usize::from(offset - 1)] = byte,
                (1, 7) => self.current = byte,
                (1, 8..=15) => self.multicast[usize::from(offset - 8)] = byte,
                _ => {}
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(n: &mut Ne2000, reg: u8, byte: u8) {
        assert!(n.write(reg, 1, u32::from(byte)));
    }
    fn configure(n: &mut Ne2000, start: u8, stop: u8, current: u8, boundary: u8) {
        write(n, 0, 0x21);
        write(n, 1, start);
        write(n, 2, stop);
        write(n, 3, boundary);
        write(n, 12, 0x14);
        write(n, 14, 1);
        write(n, 0, 0x61);
        write(n, 7, current);
        write(n, 0, 0x22);
        write(n, 7, 0x7f);
    }
    fn dma(n: &mut Ne2000, addr: u16, len: u16) {
        write(n, 8, addr as u8);
        write(n, 9, (addr >> 8) as u8);
        write(n, 10, len as u8);
        write(n, 11, (len >> 8) as u8);
    }
    fn read_bytes(n: &mut Ne2000, addr: u16, len: u16) -> Vec<u8> {
        dma(n, addr, len);
        write(n, 0, 0x0a);
        let mut out = Vec::new();
        for _ in 0..len.div_ceil(2) {
            out.extend_from_slice(&(n.read(0x10, 2).unwrap() as u16).to_le_bytes());
        }
        out.truncate(usize::from(len));
        out
    }
    #[test]
    fn ne2000_transmit_dma_completion_and_irq() {
        let mut n = Ne2000::default();
        configure(&mut n, 0x46, 0x80, 0x47, 0x46);
        let prom = read_bytes(&mut n, 0, 32);
        assert_eq!(
            &prom[..12],
            &[
                0x52, 0x52, 0x54, 0x54, 0, 0, 0x12, 0x12, 0x34, 0x34, 0x56, 0x56
            ]
        );
        assert_eq!(&prom[28..], &[0x57; 4]);
        write(&mut n, 7, 0x7f);
        write(&mut n, 15, 0x40);
        dma(&mut n, 0x4000, 60);
        write(&mut n, 0, 0x12);
        for _ in 0..29 {
            n.write(0x10, 2, 0x1234);
        }
        assert!(!n.irq());
        n.write(0x10, 2, 0x5678);
        assert!(n.irq());
        write(&mut n, 7, 0x40);
        assert!(!n.irq());
        write(&mut n, 4, 0x40);
        write(&mut n, 5, 60);
        write(&mut n, 6, 0);
        write(&mut n, 15, 2);
        write(&mut n, 0, 0x26);
        assert_eq!(n.read(0, 1).unwrap() & 4, 4);
        assert!(!n.irq());
        let frame = n.take_transmit().unwrap();
        assert_eq!(frame.len(), 60);
        assert_eq!(&frame[58..], &[0x78, 0x56]);
        assert_eq!(n.read(0, 1).unwrap() & 4, 0);
        assert!(n.irq());
        write(&mut n, 15, 0);
        assert!(!n.irq());
        write(&mut n, 15, 2);
        assert!(n.irq());
        write(&mut n, 7, 2);
        assert!(!n.irq());
    }
    #[test]
    fn ne2000_receive_wrap_full_and_filter() {
        let mut n = Ne2000::default();
        configure(&mut n, 0x46, 0x50, 0x4f, 0x4e);
        write(&mut n, 15, 1);
        let mut frame = vec![0x5a; 300];
        frame[..6].copy_from_slice(&MAC);
        assert!(n.receive(&frame));
        assert!(n.irq());
        let packet = read_bytes(&mut n, 0x4f00, 304);
        assert_eq!(&packet[..4], &[1, 0x47, 0x30, 1]);
        assert_eq!(&packet[4..], &frame);
        write(&mut n, 7, 1);
        assert!(!n.irq());
        configure(&mut n, 0x40, 0x44, 0x41, 0x40);
        assert!(n.receive(&frame));
        let before = read_bytes(&mut n, 0x4100, 304);
        assert!(!n.receive(&frame[..60]));
        assert_eq!(read_bytes(&mut n, 0x4100, 304), before);
        write(&mut n, 3, 0x42);
        assert!(n.receive(&frame[..60]));
        write(&mut n, 12, 0);
        write(&mut n, 7, 0x7f);
        frame[..6].fill(0xff);
        assert!(n.receive(&frame));
        assert_eq!(n.read(7, 1).unwrap() & 1, 0);
        assert_eq!(n.read(0x1f, 2), None); // unsupported width cannot reset
        n.read(0x1f, 1);
        assert!(!n.receive(&frame));
        assert!(!n.irq());
    }
}
