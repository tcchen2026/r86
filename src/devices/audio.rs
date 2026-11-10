//! Guest-clock audio, independent of host playback. The fixed-rate queue drops
//! oldest frames on overflow; its consumers never pace DMA or guest interrupts.

use super::{TICKS_PER_SECOND, dma::Dma, opl2::Opl2, sb16::Sb16};
use alloc::collections::VecDeque;

#[cfg(test)]
mod tests;

pub const SAMPLE_RATE: u32 = 48_000;
const QUEUE_FRAMES: usize = 16_384;
const CHUNK_TICKS: u64 = TICKS_PER_SECOND / 200;

#[derive(Debug, Default)]
struct Mixer {
    enabled: bool,
    epoch: u64,
    frames: u64,
    pcm: [i16; 2],
    gain: [i32; 2],
    fm_gain: [i32; 2],
    opl: Opl2,
    queue: VecDeque<[i16; 2]>,
    dropped: u64,
}

impl Mixer {
    fn render(&mut self, now: u64) {
        if !self.enabled {
            self.opl.advance(now);
            return;
        }
        let due = now.saturating_sub(self.epoch) * u64::from(SAMPLE_RATE) / TICKS_PER_SECOND;
        while self.frames < due {
            self.frames += 1;
            let tick =
                self.epoch + (self.frames * TICKS_PER_SECOND).div_ceil(u64::from(SAMPLE_RATE));
            let fm = self.opl.advance(tick);
            let output = core::array::from_fn(|i| {
                ((i64::from(self.pcm[i]) * i64::from(self.gain[i])
                    + i64::from(fm) * i64::from(self.fm_gain[i]))
                    >> 15)
                    .clamp(-32768, 32767) as i16
            });
            if self.queue.len() == QUEUE_FRAMES {
                self.queue.pop_front();
                self.dropped += 1;
            }
            self.queue.push_back(output);
        }
        self.opl.advance(now);
    }
}

#[derive(Default)]
pub(crate) struct Sound {
    dma: Dma,
    sb: Sb16,
    mixer: Mixer,
    now: u64,
}

impl core::fmt::Debug for Sound {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Guest-state comparisons exclude host queue occupancy and consumers.
        f.debug_struct("Sound")
            .field("dma", &self.dma)
            .field("sb", &self.sb)
            .field("opl", &self.mixer.opl)
            .field("now", &self.now)
            .finish()
    }
}

impl Sound {
    pub fn enable(&mut self, now: u64) {
        self.mixer.enabled = true;
        self.mixer.epoch = now;
        self.mixer.frames = 0;
        self.mixer.queue.clear();
        self.mixer.gain = self.sb.gains(0x32);
        self.mixer.fm_gain = self.sb.gains(0x34);
    }

    /// Stop producing frames; DMA, IRQs and FM timers keep guest time.
    pub fn disable(&mut self) {
        self.mixer.enabled = false;
        self.mixer.queue.clear();
    }

    pub fn reset(&mut self, now: u64) {
        let mut mixer = core::mem::take(&mut self.mixer);
        mixer.render(now);
        mixer.pcm = [0; 2];
        *self = Self::default();
        mixer.gain = self.sb.gains(0x32);
        mixer.fm_gain = self.sb.gains(0x34);
        mixer.opl.reset(now);
        self.mixer = mixer;
        self.now = now;
    }

    pub fn contains(port: u16) -> bool {
        Dma::contains(port)
            || Self::fm_port(port)
            || matches!(port, 0x224 | 0x225 | 0x226 | 0x22a | 0x22c | 0x22e | 0x22f)
    }

    fn fm_port(port: u16) -> bool {
        matches!(port, 0x220 | 0x221 | 0x228 | 0x229 | 0x388 | 0x389)
    }

    pub fn io_wait_ticks(port: u16) -> u64 {
        // One PIT tick (~838 ns) models a slow ISA FM transaction. AdLib
        // drivers use repeated status reads for write delays and detection.
        // Charge the shared clock, never synthesize a flag from a read count.
        if Self::fm_port(port) { 16 } else { 0 }
    }

    pub fn irq_mask(&self) -> u16 {
        self.sb.irq_mask()
    }
    pub fn irq(&self) -> bool {
        self.sb.irq()
    }

    pub fn next_event(&self) -> u64 {
        let dma = self.sb.next_event(&self.dma);
        dma.min(if self.mixer.enabled || dma != u64::MAX {
            self.now.saturating_add(CHUNK_TICKS)
        } else {
            u64::MAX
        })
    }

    pub fn advance(&mut self, now: u64, ram: &[u8]) {
        let mixer = &mut self.mixer;
        self.sb.advance(now, &mut self.dma, ram, |tick, sample| {
            mixer.render(tick.saturating_sub(1));
            mixer.pcm = sample;
        });
        mixer.render(now);
        self.now = now;
    }

    // Call advance before each port access, including DMA count reads/masks.
    pub fn read(&mut self, port: u16) -> u8 {
        if Self::fm_port(port) {
            if port & 1 == 0 {
                self.mixer.opl.status()
            } else {
                0xff
            }
        } else if Dma::contains(port) {
            self.dma.read(port)
        } else {
            self.sb.read(port)
        }
    }

    pub fn write(&mut self, port: u16, value: u8) {
        if Self::fm_port(port) {
            if port & 1 == 0 {
                self.mixer.opl.address(value);
            } else {
                self.mixer.opl.write(value, self.now);
            }
        } else if Dma::contains(port) {
            self.dma.write(port, value);
        } else {
            self.sb.write(port, value, self.now);
            self.sb.sync_request(&mut self.dma);
            self.mixer.pcm = self.sb.dac;
            self.mixer.gain = self.sb.gains(0x32);
            self.mixer.fm_gain = self.sb.gains(0x34);
        }
    }

    pub fn drain(&mut self, out: &mut alloc::vec::Vec<i16>) {
        out.clear();
        out.extend(self.mixer.queue.drain(..).flatten());
    }

    pub fn dropped_frames(&self) -> u64 {
        self.mixer.dropped
    }
}
