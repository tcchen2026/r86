//! Sound Blaster 16 DSP 4.05 / CT1745 at 220h. See audio/README.txt for the
//! command contract, unsupported inputs and consulted hardware documentation.

use super::{TICKS_PER_SECOND, dma::Dma};
use alloc::collections::VecDeque;

#[derive(Debug)]
struct Playback {
    wide: bool,
    stereo: bool,
    signed: bool,
    auto: bool,
    paused: bool,
    silence: bool,
    legacy: bool,
    block: u32,
    remaining: u32,
    rate: u32,
    epoch: u64,
    units: u64,
    left: Option<i16>,
}

impl Playback {
    fn tick(&self, units: u64) -> u64 {
        self.epoch
            .saturating_add((units * TICKS_PER_SECOND).div_ceil(u64::from(self.rate)))
    }
}

#[derive(Debug)]
pub(crate) struct Sb16 {
    mixer: [u8; 256],
    dsp_registers: [u8; 256],
    index: u8,
    reset: bool,
    replies: VecDeque<u8>,
    last_read: u8,
    command: u8,
    parameters: [u8; 3],
    used: usize,
    needed: usize,
    test: u8,
    speaker: bool,
    rate: u32,
    block: u32,
    irq_status: u8,
    playback: Option<Playback>,
    last_tick: u64,
    pub dac: [i16; 2],
}

impl Default for Sb16 {
    fn default() -> Self {
        let mut s = Self {
            mixer: [0; 256],
            dsp_registers: [0; 256],
            index: 0,
            reset: false,
            replies: VecDeque::new(),
            last_read: 0xff,
            command: 0,
            parameters: [0; 3],
            used: 0,
            needed: 0,
            test: 0,
            speaker: false,
            rate: 22050,
            block: 65536,
            irq_status: 0,
            playback: None,
            last_tick: 0,
            dac: [0; 2],
        };
        s.dsp_registers[5] = 1;
        s.dsp_registers[9] = 0xf8;
        s.mixer[0x80] = 2; // IRQ 5
        s.mixer[0x81] = 0x22; // DMA 1 and 5
        s.reset_mixer();
        s
    }
}

impl Sb16 {
    fn reset_mixer(&mut self) {
        self.mixer[..0x80].fill(0);
        self.mixer[0x30..=0x35].fill(0xc0);
        self.mixer[0x3c] = 0x1f;
        self.mixer[0x3d..=0x3e].fill(0x15);
        self.mixer[0x44..=0x47].fill(0x80);
    }

    pub fn irq_mask(&self) -> u16 {
        let selected = self.mixer[0x80];
        [9, 5, 7, 10].iter().enumerate().fold(0, |mask, (i, &irq)| {
            mask | (u16::from((selected >> i) & 1) << irq)
        })
    }

    pub fn irq(&self) -> bool {
        self.irq_status != 0
    }

    fn channel(&self, wide: bool) -> Option<usize> {
        let allowed = self.mixer[0x81] & if wide { 0xe0 } else { 0x0b };
        (allowed != 0).then(|| allowed.trailing_zeros() as usize)
    }

    pub fn sync_request(&self, dma: &mut Dma) {
        for ch in [0, 1, 3, 5, 6, 7] {
            dma.set_request(ch, false);
        }
        if let Some(p) = &self.playback {
            if let Some(channel) = self.channel(p.wide) {
                dma.set_request(channel, !p.paused && !p.silence);
            }
        }
    }

    pub fn next_event(&self, dma: &Dma) -> u64 {
        self.playback
            .as_ref()
            .filter(|p| {
                !p.paused && (p.silence || self.channel(p.wide).is_some_and(|ch| dma.readable(ch)))
            })
            .map_or(u64::MAX, |p| p.tick(p.units + u64::from(p.remaining)))
    }

    /// DMA is copied once per bounded chunk, then converted at its original
    /// guest-time positions. Frontend queue state never changes this timeline.
    pub fn advance(
        &mut self,
        now: u64,
        dma: &mut Dma,
        ram: &[u8],
        mut sample: impl FnMut(u64, [i16; 2]),
    ) {
        let Some(mut p) = self.playback.take() else {
            self.last_tick = now;
            return;
        };
        let channel = self.channel(p.wide);
        if p.paused || (!p.silence && !channel.is_some_and(|ch| dma.readable(ch))) {
            p.epoch += now.saturating_sub(self.last_tick);
            self.last_tick = now;
            self.playback = Some(p);
            return;
        }
        self.last_tick = now;
        let due = now.saturating_sub(p.epoch) * u64::from(p.rate) / TICKS_PER_SECOND;
        let mut scratch = [0u8; 2048];
        while p.units < due {
            let width = if p.wide { 2 } else { 1 };
            let wanted = (due - p.units)
                .min(u64::from(p.remaining))
                .min((scratch.len() / width) as u64) as usize;
            let count = if p.silence {
                wanted
            } else {
                dma.read_memory(channel.unwrap(), ram, &mut scratch[..wanted * width]) / width
            };
            if count == 0 {
                // DREQ was not served: do not accumulate catch-up transfers.
                p.epoch = now;
                p.units = 0;
                break;
            }
            for i in 0..count {
                p.units += 1;
                if !p.silence {
                    let value = if p.wide {
                        let raw = u16::from_le_bytes([scratch[2 * i], scratch[2 * i + 1]]);
                        (raw ^ if p.signed { 0 } else { 0x8000 }) as i16
                    } else {
                        i16::from((scratch[i] ^ if p.signed { 0 } else { 0x80 }) as i8) << 8
                    };
                    if !p.stereo {
                        self.dac = [value; 2];
                        sample(p.tick(p.units), self.dac);
                    } else if let Some(left) = p.left.take() {
                        self.dac = [left, value];
                        sample(p.tick(p.units), self.dac);
                    } else {
                        p.left = Some(value);
                    }
                }
            }
            p.remaining -= count as u32;
            if p.remaining == 0 {
                self.irq_status |= if p.wide { 2 } else { 1 };
                if !p.auto {
                    self.sync_request(dma);
                    return;
                }
                p.remaining = p.block;
            }
        }
        self.playback = Some(p);
        self.sync_request(dma);
    }

    pub fn read(&mut self, port: u16) -> u8 {
        match port & 15 {
            4 => self.index,
            5 => match self.index {
                0x04 | 0x22 | 0x26 | 0x28 | 0x2e => {
                    let reg = Self::legacy(self.index);
                    (self.mixer[reg] & 0xf0) | (self.mixer[reg + 1] >> 4)
                }
                0x82 => self.irq_status,
                _ => self.mixer[self.index as usize],
            },
            0xa => {
                if let Some(value) = self.replies.pop_front() {
                    self.last_read = value;
                }
                self.last_read
            }
            0xc => {
                if self.reset {
                    0x80
                } else {
                    0x00
                }
            }
            0xe => {
                self.irq_status &= !1;
                if self.replies.is_empty() { 0x7f } else { 0xff }
            }
            0xf => {
                self.irq_status &= !2;
                0xff
            }
            _ => 0xff,
        }
    }

    fn legacy(index: u8) -> usize {
        match index {
            0x04 => 0x32,
            0x22 => 0x30,
            0x26 => 0x34,
            0x28 => 0x36,
            _ => 0x38,
        }
    }

    pub fn write(&mut self, port: u16, value: u8, now: u64) {
        match port & 15 {
            4 => self.index = value,
            5 => match self.index {
                0 => self.reset_mixer(),
                0x82 => {}
                0x04 | 0x22 | 0x26 | 0x28 | 0x2e => {
                    let reg = Self::legacy(self.index);
                    self.mixer[reg] = (value & 0xf0) | 8;
                    self.mixer[reg + 1] = (value << 4) | 8;
                }
                _ => self.mixer[self.index as usize] = value,
            },
            6 => {
                if value & 1 != 0 {
                    self.playback = None;
                    self.replies.clear();
                    self.needed = 0;
                    self.irq_status = 0;
                    self.dac = [0; 2];
                    self.speaker = false;
                    self.rate = 22050;
                    self.block = 65536;
                    self.test = 0;
                } else if self.reset {
                    self.reply(&[0xaa]);
                }
                self.reset = value & 1 != 0;
            }
            0xc if !self.reset => {
                if self.needed != 0 {
                    self.parameters[self.used] = value;
                    self.used += 1;
                    if self.used != self.needed {
                        return;
                    }
                    self.needed = 0;
                } else {
                    self.command = value;
                    self.used = 0;
                    self.needed = match value {
                        0xb0..=0xcf => 3,
                        0x05 | 0x0e => 2,
                        0x04 | 0x0f => 1,
                        0x14 | 0x16 | 0x17 | 0x24 | 0x41 | 0x42 | 0x48 | 0x74..=0x77 | 0x80 => 2,
                        0x10 | 0x40 | 0xe0 | 0xe2 | 0xe4 | 0xf9 => 1,
                        _ => 0,
                    };
                    if self.needed != 0 {
                        return;
                    }
                }
                self.execute(now);
            }
            _ => {}
        }
    }

    fn reply(&mut self, bytes: &[u8]) {
        self.replies.clear();
        self.replies.extend(bytes);
    }

    fn start(
        &mut self,
        now: u64,
        wide: bool,
        stereo: bool,
        signed: bool,
        auto: bool,
        block: u32,
        legacy: bool,
        silence: bool,
    ) {
        self.playback = Some(Playback {
            wide,
            stereo,
            signed,
            auto,
            paused: false,
            silence,
            legacy,
            block,
            remaining: block,
            rate: self.rate.max(1) * if stereo && !legacy { 2 } else { 1 },
            epoch: now,
            units: 0,
            left: None,
        });
        self.last_tick = now;
    }

    fn set_rate(&mut self, rate: u32, now: u64) {
        self.rate = rate.max(1);
        if let Some(p) = &mut self.playback {
            p.epoch = now;
            p.units = 0;
            p.rate = self.rate * if p.stereo && !p.legacy { 2 } else { 1 };
        }
    }

    fn execute(&mut self, now: u64) {
        let cmd = self.command;
        let p = self.parameters;
        let length = u32::from(u16::from_le_bytes([p[0], p[1]])) + 1;
        match cmd {
            // Windows WDM uses DSP register reads/writes during reset and stop.
            // Consume the complete command: register 10h is not direct DAC 10h.
            // This register bank does not implement the optional ASP codec.
            0x0e => self.dsp_registers[p[0] as usize] = p[1],
            0x0f => self.reply(&[self.dsp_registers[p[0] as usize]]),
            0x10 => self.dac = [((i32::from(p[0]) - 128) * 256) as i16; 2],
            0x14 | 0x1c | 0x90 | 0x91 => self.start(
                now,
                false,
                self.mixer[0x0e] & 2 != 0,
                false,
                matches!(cmd, 0x1c | 0x90),
                if cmd == 0x14 { length } else { self.block },
                true,
                false,
            ),
            0x20 => self.reply(&[0x80]), // no connected recording source
            0x40 => self.set_rate(1_000_000 / (256 - u32::from(p[0])), now),
            0x41 => self.set_rate(u32::from(u16::from_be_bytes([p[0], p[1]])), now),
            0x48 => self.block = length,
            0x80 => {
                self.dac = [0; 2];
                self.start(now, false, false, false, false, length, true, true);
            }
            0xb0..=0xcf if cmd & 8 == 0 => self.start(
                now,
                cmd < 0xc0,
                p[0] & 0x20 != 0,
                p[0] & 0x10 != 0,
                cmd & 4 != 0,
                u32::from(u16::from_le_bytes([p[1], p[2]])) + 1,
                false,
                false,
            ),
            0xd0 | 0xd4 | 0xd5 | 0xd6 => {
                if let Some(p) = &mut self.playback {
                    if p.wide == matches!(cmd, 0xd5 | 0xd6) {
                        p.paused = matches!(cmd, 0xd0 | 0xd5);
                    }
                }
            }
            0xd1 => self.speaker = true,
            0xd3 => self.speaker = false,
            0xd8 => self.reply(&[if self.speaker { 0xff } else { 0 }]),
            0xd9 | 0xda => {
                if let Some(p) = &mut self.playback {
                    if p.wide == (cmd == 0xd9) {
                        p.auto = false;
                    }
                }
            }
            0xe0 => self.reply(&[!p[0]]),
            0xe1 => self.reply(&[4, 5]),
            0xe3 => self.reply(b"COPYRIGHT (C) CREATIVE TECHNOLOGY LTD, 1992.\0"),
            0xe4 => self.test = p[0],
            0xe8 => self.reply(&[self.test]),
            0xf2 => self.irq_status |= 1,
            0xf3 => self.irq_status |= 2,
            0xf8 => self.reply(&[0]),
            // DSP RAM reset values checked by the Windows 9x SB16 driver.
            0xf9 => self.reply(&[match p[0] {
                0x0e => 0xff,
                0x0f => 0x07,
                0x37 => 0x38,
                _ => 0,
            }]),
            // Unsupported/unknown commands produce no reply. Known commands
            // still consume their operands so the next command stays aligned.
            _ => {}
        }
    }

    /// CT1745 volume: 2 dB steps. Zero mutes; output gain is 6 dB/step.
    pub fn gains(&self, source: usize) -> [i32; 2] {
        const LEVEL: [i32; 32] = [
            0, 33, 41, 52, 65, 82, 104, 131, 164, 207, 260, 328, 413, 519, 654, 823, 1036, 1305,
            1642, 2068, 2603, 3277, 4125, 5193, 6538, 8231, 10362, 13045, 16423, 20675, 26029,
            32768,
        ];
        core::array::from_fn(|i| {
            (LEVEL[(self.mixer[0x30 + i] >> 3) as usize]
                * LEVEL[(self.mixer[source + i] >> 3) as usize]
                >> 15)
                << (self.mixer[0x41 + i] >> 6)
        })
    }
}
