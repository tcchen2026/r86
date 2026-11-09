// Copyright (c) 2021, Aaron Giles
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this
//    list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its
//    contributors may be used to endorse or promote products derived from
//    this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//
// Adapted from ymfm/src/ymfm_opl.{h,cpp}, ymfm_fm.{h,ipp}, ymfm.h
// at 81aec25ccbb98f4873a255f7551ac4dadac59b4a (BSD-3-Clause).
// YM3812 only: fixed operators, integer synthesis, native 72-clock samples.
// r86 owns the port aliases, guest-clock timers and output resampling.

// The guest oscillator is 16*PIT and OPL is 3*PIT: 72 OPL clocks are
// exactly 384 guest ticks. This avoids rounding the native sample rate.
const SAMPLE_TICKS: u64 = 384;
const MODULATORS: [usize; 9] = [0, 1, 2, 6, 7, 8, 12, 13, 14];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Envelope {
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, Debug)]
struct Operator {
    phase: u32,
    attenuation: u32,
    state: Envelope,
    key: bool,
    frequency: u32,
    multiple: u32,
    level: u32,
    sustain: u32,
    rates: [u32; 4],
    wave: u8,
    am: bool,
    vibrato: bool,
}

impl Default for Operator {
    fn default() -> Self {
        Self {
            phase: 0,
            attenuation: 1023,
            state: Envelope::Release,
            key: false,
            frequency: 0,
            multiple: 1,
            level: 0,
            sustain: 0,
            rates: [0; 4],
            wave: 0,
            am: false,
            vibrato: false,
        }
    }
}

impl Operator {
    fn envelope(&mut self, counter: u32) {
        if self.state == Envelope::Attack && self.attenuation == 0 {
            self.state = Envelope::Decay;
        }
        if self.state == Envelope::Decay && self.attenuation >= self.sustain {
            self.state = Envelope::Sustain;
        }
        let rate = self.rates[self.state as usize];
        let shift = rate >> 2;
        let scaled = counter.wrapping_shl(shift);
        if scaled & 0x7ff != 0 {
            return;
        }
        let index = (scaled >> shift.max(11)) & 7;
        let increment = (INCREMENT[rate as usize] >> (4 * index)) & 15;
        if self.state == Envelope::Attack {
            if rate < 62 {
                self.attenuation = (self.attenuation as i32
                    + ((!self.attenuation as i32 * increment as i32) >> 4))
                    as u32;
            }
        } else {
            self.attenuation = (self.attenuation + increment).min(1023);
        }
    }

    fn clock(&mut self, counter: u32, pm: i32) {
        self.envelope(counter);
        let mut fnum = ((self.frequency & 1023) << 2) as i32;
        if self.vibrato {
            fnum += (pm * ((self.frequency >> 7) & 7) as i32) >> 1;
        }
        let step = (((fnum as u32 & 4095) << (self.frequency >> 10)) >> 2) * self.multiple >> 1;
        self.phase = self.phase.wrapping_add(step);
    }

    fn phase(&self) -> u32 {
        self.phase >> 10
    }

    fn volume(&self, phase: u32, am: u32) -> i32 {
        if self.attenuation > 0x380 {
            return 0;
        }
        let mirror = if phase & 0x100 != 0 { !phase } else { phase };
        let mut sine = u32::from(SIN[(mirror & 255) as usize]);
        let mut negative = phase & 0x200 != 0;
        match self.wave {
            1 if negative => {
                sine = u32::from(SIN[0]);
                negative = false;
            }
            2 => negative = false,
            3 => {
                negative = false;
                if phase & 0x100 != 0 {
                    sine = u32::from(SIN[0]);
                }
            }
            _ => {}
        }
        let envelope = (self.attenuation + self.level + if self.am { am } else { 0 }).min(1023);
        let attenuation = sine + (envelope << 2);
        let value = (u32::from(POWER[(attenuation & 255) as usize]) >> (attenuation >> 8)) as i32;
        if negative { -value } else { value }
    }
}

#[derive(Debug)]
pub(crate) struct Opl2 {
    registers: [u8; 256],
    address: u8,
    operators: [Operator; 18],
    feedback: [[i32; 2]; 9],
    feedback_in: [i32; 9],
    dirty: bool,
    silent: bool,
    prepare_count: u16,
    csm: bool,
    envelope_clock: u32,
    am_clock: u16,
    pm_clock: u16,
    noise: u32,
    next_sample: u64,
    timers: [Option<u64>; 2],
    status: u8,
    output: i16,
}

impl Default for Opl2 {
    fn default() -> Self {
        Self {
            registers: [0; 256],
            address: 0,
            operators: [Operator::default(); 18],
            feedback: [[0; 2]; 9],
            feedback_in: [0; 9],
            dirty: false,
            silent: true,
            prepare_count: 0,
            csm: false,
            envelope_clock: 0,
            am_clock: 0,
            pm_clock: 0,
            noise: 1,
            next_sample: SAMPLE_TICKS,
            timers: [None; 2],
            status: 0,
            output: 0,
        }
    }
}

impl Opl2 {
    pub fn reset(&mut self, now: u64) {
        *self = Self::default();
        self.next_sample = now + SAMPLE_TICKS;
    }

    pub fn status(&self) -> u8 {
        let flags = self.status & !self.registers[4] & 0x60;
        flags | if flags != 0 { 0x80 } else { 0 } | 6
    }

    pub fn address(&mut self, value: u8) {
        self.address = value;
    }

    pub fn write(&mut self, value: u8, now: u64) {
        let reg = usize::from(self.address);
        if reg == 4 {
            if value & 0x80 != 0 {
                self.status = 0;
                return;
            }
            self.status &= !(value & 0x60);
            self.registers[4] = value;
            for i in 0..2 {
                if value & (1 << i) == 0 {
                    self.timers[i] = None;
                } else if self.timers[i].is_none() {
                    let shorten = if i == 1 {
                        u64::from(self.envelope_clock & 15) * SAMPLE_TICKS
                    } else {
                        0
                    };
                    self.timers[i] = Some(now + self.period(i) - shorten);
                }
            }
        } else {
            self.registers[reg] = value;
            self.dirty = true;
        }
    }

    fn period(&self, timer: usize) -> u64 {
        u64::from(256 - u16::from(self.registers[timer + 2]))
            * SAMPLE_TICKS
            * if timer == 0 { 4 } else { 16 }
    }

    fn timers_until(&mut self, now: u64) {
        for i in 0..2 {
            if let Some(due) = self.timers[i] {
                if now >= due {
                    self.status |= 0x40 >> i;
                    let period = self.period(i);
                    self.timers[i] = Some(due + ((now - due) / period + 1) * period);
                    if i == 0 && self.registers[8] & 0x80 != 0 {
                        self.csm = true;
                        self.dirty = true;
                    }
                }
            }
        }
    }

    pub fn advance(&mut self, now: u64) -> i16 {
        while self.next_sample <= now {
            self.timers_until(self.next_sample);
            self.output = self.clock();
            self.next_sample += SAMPLE_TICKS;
        }
        self.timers_until(now);
        self.output
    }

    fn prepare(&mut self) {
        let r = &self.registers;
        for (i, op) in self.operators.iter_mut().enumerate() {
            let channel = (i / 6) * 3 + i % 3;
            let offset = (i / 6) * 8 + i % 6;
            op.frequency = u32::from(r[0xa0 + channel]) | (u32::from(r[0xb0 + channel] & 31) << 8);
            let keycode =
                ((op.frequency >> 10) << 1) | ((op.frequency >> (9 - ((r[8] >> 6) & 1))) & 1);
            let mul = u32::from(r[0x20 + offset] & 15);
            op.multiple = (((mul & 14) | ((0xc2aa >> mul) & 1)) * 2).max(1);
            let scale = r[0x40 + offset] >> 6;
            let ksl = ((scale >> 1) | (scale << 1)) & 3;
            const KSL: [i32; 16] = [
                0, 24, 32, 37, 40, 43, 45, 47, 48, 50, 51, 52, 53, 54, 55, 56,
            ];
            op.level = u32::from(r[0x40 + offset] & 63) << 3;
            if ksl != 0 {
                op.level += ((KSL[((op.frequency >> 6) & 15) as usize]
                    - 8 * ((op.frequency >> 10) ^ 7) as i32)
                    .max(0) as u32)
                    << ksl;
            }
            let sustain = u32::from(r[0x80 + offset] >> 4);
            op.sustain = (sustain | ((sustain + 1) & 16)) << 5;
            let ksr = keycode >> if r[0x20 + offset] & 0x10 != 0 { 0 } else { 2 };
            let effective = |raw: u8| {
                if raw == 0 {
                    0
                } else {
                    (u32::from(raw) * 4 + ksr).min(63)
                }
            };
            let release = effective(r[0x80 + offset] & 15);
            op.rates = [
                effective(r[0x60 + offset] >> 4),
                effective(r[0x60 + offset] & 15),
                if r[0x20 + offset] & 0x20 != 0 {
                    0
                } else {
                    release
                },
                release,
            ];
            op.wave = if r[1] & 0x20 != 0 {
                r[0xe0 + offset] & 3
            } else {
                0
            };
            op.am = r[0x20 + offset] & 0x80 != 0;
            op.vibrato = r[0x20 + offset] & 0x40 != 0;
            let rhythm = if r[0xbd] & 0x20 == 0 {
                false
            } else {
                match i {
                    12 | 15 => r[0xbd] & 16 != 0,
                    13 => r[0xbd] & 1 != 0,
                    14 => r[0xbd] & 4 != 0,
                    16 => r[0xbd] & 8 != 0,
                    17 => r[0xbd] & 2 != 0,
                    _ => false,
                }
            };
            let key = r[0xb0 + channel] & 0x20 != 0 || rhythm || self.csm;
            if key != op.key {
                if key && op.state != Envelope::Attack {
                    op.state = Envelope::Attack;
                    op.phase = 0;
                    if op.rates[0] >= 62 {
                        op.attenuation = 0;
                    }
                } else if !key {
                    op.state = Envelope::Release;
                }
                op.key = key;
            }
        }
        self.silent = self
            .operators
            .iter()
            .all(|op| op.state == Envelope::Release && op.attenuation == 1023);
        self.dirty = false;
        self.csm = false;
    }

    fn clock(&mut self) -> i16 {
        if self.dirty || self.prepare_count == 4096 {
            self.prepare();
            self.prepare_count = 0;
        } else {
            self.prepare_count += 1;
        }
        self.envelope_clock = self.envelope_clock.wrapping_add(1);
        self.noise = self.noise.wrapping_shl(1);
        self.noise |=
            ((self.noise >> 23) ^ (self.noise >> 9) ^ (self.noise >> 8) ^ (self.noise >> 1)) & 1;
        let counter = u32::from(self.am_clock);
        self.am_clock = ((counter + 1) % (210 * 64)) as u16;
        let am = (if counter < 105 * 64 {
            counter
        } else {
            210 * 64 + 63 - counter
        }) >> (9 - 2 * (self.registers[0xbd] >> 7));
        let pm = [8, 4, 0, -4, -8, -4, 0, 4][((self.pm_clock >> 10) & 7) as usize]
            >> (((self.registers[0xbd] >> 6) & 1) ^ 1);
        self.pm_clock = self.pm_clock.wrapping_add(1);
        if self.silent {
            return 0;
        }
        for op in &mut self.operators {
            op.clock(self.envelope_clock, pm);
        }
        for (history, &current) in self.feedback.iter_mut().zip(&self.feedback_in) {
            *history = [history[1], current];
        }
        let rhythm = self.registers[0xbd] & 0x20 != 0;
        let hh = self.operators[13].phase();
        let cym = self.operators[17].phase();
        let select = (((hh >> 2) ^ (hh >> 7)) | (hh >> 3) | ((cym >> 5) ^ (cym >> 3))) & 1;
        let noise = (self.noise >> 23) & 1;
        let mut sum = 0;
        for (channel, &modulator) in MODULATORS.iter().enumerate() {
            let a = &self.operators[modulator];
            let b = &self.operators[modulator + 3];
            if rhythm && channel == 7 {
                let phase = (select << 9) | (0xd0 >> (2 * (noise ^ select)));
                let snare = (0x100 << ((hh >> 8) & 1)) ^ (noise << 8);
                sum += ((a.volume(phase, am) >> 1) + (b.volume(snare, am) >> 1)) * 2;
            } else if rhythm && channel == 8 {
                sum += ((a.volume(a.phase(), am) >> 1)
                    + (b.volume(0x100 | (select << 9), am) >> 1))
                    * 2;
            } else {
                let control = self.registers[0xc0 + channel];
                let fb = (control >> 1) & 7;
                let modulation = if fb == 0 {
                    0
                } else {
                    (self.feedback[channel][0] + self.feedback[channel][1]) >> (10 - fb)
                };
                let first = a.volume(a.phase().wrapping_add(modulation as u32), am);
                self.feedback_in[channel] = first;
                if rhythm && channel == 6 {
                    let modulation = if control & 1 != 0 { 0 } else { first >> 1 };
                    sum += (b.volume(b.phase().wrapping_add(modulation as u32), am) >> 1) * 2;
                } else if control & 1 == 0 {
                    sum += b.volume(
                        b.phase()
                            .wrapping_add((self.feedback[channel][1] >> 1) as u32),
                        am,
                    ) >> 1;
                } else {
                    sum += (self.feedback[channel][1] >> 1) + (b.volume(b.phase(), am) >> 1);
                }
            }
        }
        if self.envelope_clock & 255 == 0 {
            self.silent = self
                .operators
                .iter()
                .all(|op| op.state == Envelope::Release && op.attenuation == 1023);
        }
        // External YM3014 DAC mantissa/exponent truncation.
        let value = sum.clamp(-32768, 32767);
        let magnitude = (value ^ (value >> 31)) as u32;
        let shift = (7 - (magnitude << 17).leading_zeros() as i32).max(1) - 1;
        (value & !((1 << shift) - 1)) as i16
    }
}

const SIN: [u16; 256] = [
    0x859, 0x6c3, 0x607, 0x58b, 0x52e, 0x4e4, 0x4a6, 0x471, 0x443, 0x41a, 0x3f5, 0x3d3, 0x3b5,
    0x398, 0x37e, 0x365, 0x34e, 0x339, 0x324, 0x311, 0x2ff, 0x2ed, 0x2dc, 0x2cd, 0x2bd, 0x2af,
    0x2a0, 0x293, 0x286, 0x279, 0x26d, 0x261, 0x256, 0x24b, 0x240, 0x236, 0x22c, 0x222, 0x218,
    0x20f, 0x206, 0x1fd, 0x1f5, 0x1ec, 0x1e4, 0x1dc, 0x1d4, 0x1cd, 0x1c5, 0x1be, 0x1b7, 0x1b0,
    0x1a9, 0x1a2, 0x19b, 0x195, 0x18f, 0x188, 0x182, 0x17c, 0x177, 0x171, 0x16b, 0x166, 0x160,
    0x15b, 0x155, 0x150, 0x14b, 0x146, 0x141, 0x13c, 0x137, 0x133, 0x12e, 0x129, 0x125, 0x121,
    0x11c, 0x118, 0x114, 0x10f, 0x10b, 0x107, 0x103, 0x0ff, 0x0fb, 0x0f8, 0x0f4, 0x0f0, 0x0ec,
    0x0e9, 0x0e5, 0x0e2, 0x0de, 0x0db, 0x0d7, 0x0d4, 0x0d1, 0x0cd, 0x0ca, 0x0c7, 0x0c4, 0x0c1,
    0x0be, 0x0bb, 0x0b8, 0x0b5, 0x0b2, 0x0af, 0x0ac, 0x0a9, 0x0a7, 0x0a4, 0x0a1, 0x09f, 0x09c,
    0x099, 0x097, 0x094, 0x092, 0x08f, 0x08d, 0x08a, 0x088, 0x086, 0x083, 0x081, 0x07f, 0x07d,
    0x07a, 0x078, 0x076, 0x074, 0x072, 0x070, 0x06e, 0x06c, 0x06a, 0x068, 0x066, 0x064, 0x062,
    0x060, 0x05e, 0x05c, 0x05b, 0x059, 0x057, 0x055, 0x053, 0x052, 0x050, 0x04e, 0x04d, 0x04b,
    0x04a, 0x048, 0x046, 0x045, 0x043, 0x042, 0x040, 0x03f, 0x03e, 0x03c, 0x03b, 0x039, 0x038,
    0x037, 0x035, 0x034, 0x033, 0x031, 0x030, 0x02f, 0x02e, 0x02d, 0x02b, 0x02a, 0x029, 0x028,
    0x027, 0x026, 0x025, 0x024, 0x023, 0x022, 0x021, 0x020, 0x01f, 0x01e, 0x01d, 0x01c, 0x01b,
    0x01a, 0x019, 0x018, 0x017, 0x017, 0x016, 0x015, 0x014, 0x014, 0x013, 0x012, 0x011, 0x011,
    0x010, 0x00f, 0x00f, 0x00e, 0x00d, 0x00d, 0x00c, 0x00c, 0x00b, 0x00a, 0x00a, 0x009, 0x009,
    0x008, 0x008, 0x007, 0x007, 0x007, 0x006, 0x006, 0x005, 0x005, 0x005, 0x004, 0x004, 0x004,
    0x003, 0x003, 0x003, 0x002, 0x002, 0x002, 0x002, 0x001, 0x001, 0x001, 0x001, 0x001, 0x001,
    0x001, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000, 0x000,
];

const POWER: [u16; 256] = [
    0x1fe8, 0x1fd4, 0x1fbc, 0x1fa8, 0x1f90, 0x1f7c, 0x1f68, 0x1f50, 0x1f3c, 0x1f24, 0x1f10, 0x1efc,
    0x1ee4, 0x1ed0, 0x1eb8, 0x1ea4, 0x1e90, 0x1e7c, 0x1e64, 0x1e50, 0x1e3c, 0x1e28, 0x1e10, 0x1dfc,
    0x1de8, 0x1dd4, 0x1dc0, 0x1da8, 0x1d94, 0x1d80, 0x1d6c, 0x1d58, 0x1d44, 0x1d30, 0x1d1c, 0x1d08,
    0x1cf4, 0x1ce0, 0x1ccc, 0x1cb8, 0x1ca4, 0x1c90, 0x1c7c, 0x1c68, 0x1c54, 0x1c40, 0x1c2c, 0x1c18,
    0x1c08, 0x1bf4, 0x1be0, 0x1bcc, 0x1bb8, 0x1ba4, 0x1b94, 0x1b80, 0x1b6c, 0x1b58, 0x1b48, 0x1b34,
    0x1b20, 0x1b10, 0x1afc, 0x1ae8, 0x1ad4, 0x1ac4, 0x1ab0, 0x1aa0, 0x1a8c, 0x1a78, 0x1a68, 0x1a54,
    0x1a44, 0x1a30, 0x1a20, 0x1a0c, 0x19fc, 0x19e8, 0x19d8, 0x19c4, 0x19b4, 0x19a0, 0x1990, 0x197c,
    0x196c, 0x195c, 0x1948, 0x1938, 0x1924, 0x1914, 0x1904, 0x18f0, 0x18e0, 0x18d0, 0x18c0, 0x18ac,
    0x189c, 0x188c, 0x1878, 0x1868, 0x1858, 0x1848, 0x1838, 0x1824, 0x1814, 0x1804, 0x17f4, 0x17e4,
    0x17d4, 0x17c0, 0x17b0, 0x17a0, 0x1790, 0x1780, 0x1770, 0x1760, 0x1750, 0x1740, 0x1730, 0x1720,
    0x1710, 0x1700, 0x16f0, 0x16e0, 0x16d0, 0x16c0, 0x16b0, 0x16a0, 0x1690, 0x1680, 0x1670, 0x1664,
    0x1654, 0x1644, 0x1634, 0x1624, 0x1614, 0x1604, 0x15f8, 0x15e8, 0x15d8, 0x15c8, 0x15bc, 0x15ac,
    0x159c, 0x158c, 0x1580, 0x1570, 0x1560, 0x1550, 0x1544, 0x1534, 0x1524, 0x1518, 0x1508, 0x14f8,
    0x14ec, 0x14dc, 0x14d0, 0x14c0, 0x14b0, 0x14a4, 0x1494, 0x1488, 0x1478, 0x146c, 0x145c, 0x1450,
    0x1440, 0x1430, 0x1424, 0x1418, 0x1408, 0x13fc, 0x13ec, 0x13e0, 0x13d0, 0x13c4, 0x13b4, 0x13a8,
    0x139c, 0x138c, 0x1380, 0x1370, 0x1364, 0x1358, 0x1348, 0x133c, 0x1330, 0x1320, 0x1314, 0x1308,
    0x12f8, 0x12ec, 0x12e0, 0x12d4, 0x12c4, 0x12b8, 0x12ac, 0x12a0, 0x1290, 0x1284, 0x1278, 0x126c,
    0x1260, 0x1250, 0x1244, 0x1238, 0x122c, 0x1220, 0x1214, 0x1208, 0x11f8, 0x11ec, 0x11e0, 0x11d4,
    0x11c8, 0x11bc, 0x11b0, 0x11a4, 0x1198, 0x118c, 0x1180, 0x1174, 0x1168, 0x115c, 0x1150, 0x1144,
    0x1138, 0x112c, 0x1120, 0x1114, 0x1108, 0x10fc, 0x10f0, 0x10e4, 0x10d8, 0x10cc, 0x10c0, 0x10b4,
    0x10a8, 0x10a0, 0x1094, 0x1088, 0x107c, 0x1070, 0x1064, 0x1058, 0x1050, 0x1044, 0x1038, 0x102c,
    0x1020, 0x1018, 0x100c, 0x1000,
];

const INCREMENT: [u32; 64] = [
    0x00000000, 0x00000000, 0x10101010, 0x10101010, 0x10101010, 0x10101010, 0x11101110, 0x11101110,
    0x10101010, 0x10111010, 0x11101110, 0x11111110, 0x10101010, 0x10111010, 0x11101110, 0x11111110,
    0x10101010, 0x10111010, 0x11101110, 0x11111110, 0x10101010, 0x10111010, 0x11101110, 0x11111110,
    0x10101010, 0x10111010, 0x11101110, 0x11111110, 0x10101010, 0x10111010, 0x11101110, 0x11111110,
    0x10101010, 0x10111010, 0x11101110, 0x11111110, 0x10101010, 0x10111010, 0x11101110, 0x11111110,
    0x10101010, 0x10111010, 0x11101110, 0x11111110, 0x10101010, 0x10111010, 0x11101110, 0x11111110,
    0x11111111, 0x21112111, 0x21212121, 0x22212221, 0x22222222, 0x42224222, 0x42424242, 0x44424442,
    0x44444444, 0x84448444, 0x84848484, 0x88848884, 0x88888888, 0x88888888, 0x88888888, 0x88888888,
];
