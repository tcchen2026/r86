//! AT 8237 pair. Counts are transfer units minus one; channels 5..7 address
//! words in a fixed 128 KiB page. Only memory-to-device DMA has a consumer.

#[derive(Clone, Copy, Debug, Default)]
struct Channel {
    base_addr: u16,
    addr: u16,
    base_count: u16,
    count: u16,
    mode: u8,
}

#[derive(Debug)]
struct Controller {
    channels: [Channel; 4],
    command: u8,
    mask: u8,
    request: u8,
    terminal: u8,
    high: bool,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            channels: [Channel::default(); 4],
            command: 0,
            mask: 15,
            request: 0,
            terminal: 0,
            high: false,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Dma {
    controllers: [Controller; 2],
    pages: [u8; 16],
    dreq: u8,
}

impl Dma {
    pub fn contains(port: u16) -> bool {
        matches!(port, 0x00..=0x0f | 0x80..=0x8f | 0xc0..=0xdf)
    }

    pub fn read(&mut self, port: u16) -> u8 {
        if (0x80..=0x8f).contains(&port) {
            return self.pages[(port & 15) as usize];
        }
        if port >= 0xc0 && port & 1 != 0 {
            return 0xff;
        }
        let bank = usize::from(port >= 0xc0);
        let reg = if bank == 0 { port } else { (port - 0xc0) / 2 };
        let c = &mut self.controllers[bank];
        match reg {
            0..=7 => {
                let ch = &c.channels[(reg / 2) as usize];
                let value = if reg & 1 == 0 { ch.addr } else { ch.count };
                let result = (value >> (u32::from(c.high) * 8)) as u8;
                c.high = !c.high;
                result
            }
            8 => {
                let value = c.terminal | ((c.request | (self.dreq >> (bank * 4))) << 4);
                c.terminal = 0;
                value
            }
            15 => c.mask | 0xf0,
            _ => 0xff,
        }
    }

    pub fn write(&mut self, port: u16, value: u8) {
        if (0x80..=0x8f).contains(&port) {
            self.pages[(port & 15) as usize] = value;
            return;
        }
        if port >= 0xc0 && port & 1 != 0 {
            return;
        }
        let bank = usize::from(port >= 0xc0);
        let reg = if bank == 0 { port } else { (port - 0xc0) / 2 };
        let c = &mut self.controllers[bank];
        let channel = usize::from(value & 3);
        match reg {
            0..=7 => {
                let ch = &mut c.channels[(reg / 2) as usize];
                let (base, current) = if reg & 1 == 0 {
                    (&mut ch.base_addr, &mut ch.addr)
                } else {
                    (&mut ch.base_count, &mut ch.count)
                };
                let shift = u32::from(c.high) * 8;
                *base = (*base & !(0xff << shift)) | (u16::from(value) << shift);
                *current = *base;
                c.high = !c.high;
            }
            8 => c.command = value,
            9 => {
                c.request = (c.request & !(1 << channel)) | (((value >> 2) & 1) << channel);
            }
            10 => c.mask = (c.mask & !(1 << channel)) | (((value >> 2) & 1) << channel),
            11 => c.channels[channel].mode = value,
            12 => c.high = false,
            13 => {
                c.command = 0;
                c.mask = 15;
                c.request = 0;
                c.terminal = 0;
                c.high = false;
            }
            14 => c.mask = 0,
            15 => c.mask = value & 15,
            _ => unreachable!(),
        }
    }

    pub fn set_request(&mut self, channel: usize, active: bool) {
        self.dreq = (self.dreq & !(1 << channel)) | (u8::from(active) << channel);
    }

    pub fn readable(&self, channel: usize) -> bool {
        let bank = channel / 4;
        let c = &self.controllers[bank];
        let mode = c.channels[channel & 3].mode;
        channel != 4
            && c.command & 4 == 0
            && c.mask & (1 << (channel & 3)) == 0
            && mode & 0x0c == 8
            && mode & 0xc0 != 0xc0
            // The primary controller reaches the AT bus through channel 4.
            && (bank == 1 || (self.controllers[1].mask & 1 == 0 && self.controllers[1].command & 4 == 0))
    }

    /// Copy complete transfer units. Address wrapping and terminal count split
    /// a chunk, not each byte. Masked, cascade and wrong-direction DMA stall.
    pub fn read_memory(&mut self, channel: usize, ram: &[u8], out: &mut [u8]) -> usize {
        let width = if channel < 4 { 1 } else { 2 };
        let mut done = 0;
        while done + width <= out.len() && self.readable(channel) {
            let c = &mut self.controllers[channel / 4];
            let ch = &mut c.channels[channel & 3];
            let decrement = ch.mode & 0x20 != 0;
            let page = self.pages[[7, 3, 1, 2, 15, 11, 9, 10][channel]];
            let base = (usize::from(page) & !(width - 1)) << 16;
            let boundary = if decrement {
                usize::from(ch.addr) + 1
            } else {
                65536 - usize::from(ch.addr)
            };
            let units = ((out.len() - done) / width)
                .min(usize::from(ch.count) + 1)
                .min(boundary);
            let start = base + usize::from(ch.addr) * width;
            let dest = &mut out[done..done + units * width];
            dest.fill(0xff);
            if decrement {
                for (i, word) in dest.chunks_exact_mut(width).enumerate() {
                    if let Some(src) = ram.get(start - i * width..start - i * width + width) {
                        word.copy_from_slice(src);
                    }
                }
            } else if start < ram.len() {
                let len = dest.len().min(ram.len() - start);
                dest[..len].copy_from_slice(&ram[start..start + len]);
            }
            ch.addr = if decrement {
                ch.addr.wrapping_sub(units as u16)
            } else {
                ch.addr.wrapping_add(units as u16)
            };
            let terminal = units == usize::from(ch.count) + 1;
            ch.count = ch.count.wrapping_sub(units as u16);
            if terminal {
                c.terminal |= 1 << (channel & 3);
                c.request &= !(1 << (channel & 3));
                if ch.mode & 0x10 != 0 {
                    ch.addr = ch.base_addr;
                    ch.count = ch.base_count;
                } else {
                    c.mask |= 1 << (channel & 3);
                }
            }
            done += units * width;
        }
        done
    }
}
