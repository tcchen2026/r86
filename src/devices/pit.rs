//! 8254 counters and AT wiring. Time is supplied by the machine clock, never
//! inferred from the number of port accesses. CR (the last complete write),
//! CE (the running count), and the read latches have separate lifetimes.
//!
//! References consulted: v86 src/pit.js and src/cpu.js for elapsed-time service,
//! one-shot IRQs and missed-edge handling. Its read-back and gate TODOs require
//! additional references: Intel 8254 datasheet, order 231164-005, mode diagrams
//! and read-back protocol (https://www.scs.stanford.edu/10wi-cs140/pintos/specs/8254.pdf);
//! tiny386 i8254.c. v86 pit.js models neither read-back status nor gate reloads.
//! This implementation advances between output transitions and skips complete
//! periods arithmetically. Even a long host pause cannot cause a per-CLK loop.

pub(crate) const CLOCK_DIVISOR: u64 = 16; // machine clock / 1,193,182 Hz

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle, // no complete count, or waiting for the first hardware trigger
    Counting,
    Strobe, // the one-clock low pulse in modes 2, 4 and 5
    Done,   // one-shot finished; CE keeps wrapping but OUT no longer changes
}

#[derive(Clone, Debug)]
struct Counter {
    control: u8,
    reload: u16,
    has_reload: bool,
    count: u32,
    remaining: u32, // CLK pulses until the next OUT transition
    phase: Phase,
    load_pending: bool,
    null_count: bool,
    gate: bool,
    out: bool,
    write_low: u8,
    write_high: bool,
    read_high: bool,
    count_latch: Option<u16>,
    status_latch: Option<u8>,
}

impl Counter {
    fn new(gate: bool) -> Self {
        // Power-up control/count are unspecified. Choose an inactive mode 0;
        // firmware or a direct-boot kernel must program its timers explicitly.
        Self {
            control: 0x30,
            reload: 0,
            has_reload: false,
            count: 0,
            remaining: 0,
            phase: Phase::Idle,
            load_pending: false,
            null_count: true,
            gate,
            out: false,
            write_low: 0,
            write_high: false,
            read_high: false,
            count_latch: None,
            status_latch: None,
        }
    }

    fn mode(&self) -> u8 {
        let mode = (self.control >> 1) & 7;
        if mode >= 6 { mode - 4 } else { mode }
    }

    fn access(&self) -> u8 {
        (self.control >> 4) & 3
    }

    fn modulus(&self) -> u32 {
        if self.control & 1 != 0 {
            10_000
        } else {
            65_536
        }
    }

    fn reload_count(&self) -> u32 {
        let raw = u32::from(self.reload);
        let count = if self.control & 1 != 0 {
            // Invalid BCD digits are outside the specified counter contract.
            // Reduce their weighted value to four decades, without panicking.
            ((raw & 15) + ((raw >> 4) & 15) * 10 + ((raw >> 8) & 15) * 100 + (raw >> 12) * 1000)
                % 10_000
        } else {
            raw
        };
        match (self.mode(), count) {
            (_, 0) | (3, 1) => self.modulus(),
            // Illegal periodic count: use 86Box's observed 2; v86 pit.js keeps 1.
            (2, 1) => 2,
            _ => count,
        }
    }

    fn encoded_count(&self) -> u16 {
        let n = self.count;
        if self.control & 1 != 0 {
            ((n % 10) | ((n / 10 % 10) << 4) | ((n / 100 % 10) << 8) | ((n / 1000 % 10) << 12))
                as u16
        } else {
            n as u16
        }
    }

    fn program(&mut self, control: u8) {
        self.control = control & 0x3f;
        self.has_reload = false;
        self.phase = Phase::Idle;
        self.load_pending = false;
        self.null_count = true;
        self.out = self.mode() != 0;
        self.write_high = false;
        self.read_high = false;
        self.count_latch = None;
        self.status_latch = None;
    }

    fn write(&mut self, value: u8) {
        let mode = self.mode();
        self.reload = match self.access() {
            1 => u16::from(value),
            2 => u16::from(value) << 8,
            _ if !self.write_high => {
                self.write_low = value;
                self.write_high = true;
                if mode == 0 {
                    self.phase = Phase::Idle;
                    self.load_pending = false;
                    self.out = false;
                }
                return;
            }
            _ => {
                self.write_high = false;
                u16::from_le_bytes([self.write_low, value])
            }
        };
        self.has_reload = true;
        self.null_count = true;
        match mode {
            0 | 4 => self.load_pending = true,
            2 | 3 if self.phase == Phase::Idle => self.load_pending = true,
            _ => {} // periodic reload boundary, or the next hardware trigger
        }
        if mode == 0 {
            self.out = false;
        }
    }

    fn set_gate(&mut self, high: bool) {
        let rising = high && !self.gate;
        self.gate = high;
        let mode = self.mode();
        if rising && self.has_reload && matches!(mode, 1..=3 | 5) {
            self.load_pending = true;
        }
        if !high && matches!(mode, 2 | 3) {
            self.out = true;
        }
    }

    fn latch(&mut self, count: bool, status: bool) {
        if count && self.count_latch.is_none() {
            self.count_latch = Some(self.encoded_count());
        }
        if status && self.status_latch.is_none() {
            self.status_latch =
                Some(self.control | (u8::from(self.null_count) << 6) | (u8::from(self.out) << 7));
        }
    }

    fn read(&mut self) -> u8 {
        // Status always precedes count and does not consume a count byte.
        if let Some(status) = self.status_latch.take() {
            return status;
        }
        let count = self.count_latch.unwrap_or_else(|| self.encoded_count());
        let high = self.access() == 2 || (self.access() == 3 && self.read_high);
        if self.access() != 3 || self.read_high {
            self.count_latch = None;
        }
        if self.access() == 3 {
            self.read_high = !self.read_high;
        }
        if high {
            (count >> 8) as u8
        } else {
            count as u8
        }
    }

    fn load(&mut self) {
        let n = self.reload_count();
        self.null_count = false;
        self.phase = Phase::Counting;
        self.count = n;
        self.remaining = n;
        match self.mode() {
            2 => self.remaining = n - 1,
            3 => {
                self.count = n & !1;
                self.remaining = if self.out { n.div_ceil(2) } else { n / 2 };
            }
            _ => {}
        }
    }

    fn decrement(&mut self, clocks: u64) {
        let modulus = self.modulus();
        self.count = (self.count + modulus - (clocks % u64::from(modulus)) as u32) % modulus;
    }

    fn counting_enabled(&self) -> bool {
        self.gate || matches!(self.mode(), 1 | 5)
    }

    /// Advance CE and OUT, returning the number of low-to-high transitions.
    /// Multiple edges may coalesce in the PIC; the refresh flip-flop needs parity.
    fn advance(&mut self, mut clocks: u64) -> u64 {
        let mut rises = 0;
        while clocks != 0 {
            let old_out = self.out;
            if self.load_pending {
                self.load_pending = false;
                self.out = !matches!(self.mode(), 0 | 1);
                self.load();
                clocks -= 1; // loading is a CLK pulse, not a decrement
            } else {
                if self.phase == Phase::Idle {
                    break;
                }
                // A mode 4 strobe ends after one CLK even when GATE inhibits CE.
                if !self.counting_enabled() && !(self.phase == Phase::Strobe && self.mode() == 4) {
                    break;
                }
                if self.phase == Phase::Done {
                    self.decrement(clocks);
                    break;
                }
                if matches!(self.mode(), 2 | 3) && !self.null_count {
                    let period = u64::from(self.reload_count());
                    rises += clocks / period;
                    clocks %= period;
                    if clocks == 0 {
                        break;
                    }
                }
                let elapsed = clocks.min(u64::from(self.remaining));
                if self.counting_enabled() {
                    self.decrement(elapsed * if self.mode() == 3 { 2 } else { 1 });
                }
                self.remaining -= elapsed as u32;
                clocks -= elapsed;
                if self.remaining == 0 {
                    match self.mode() {
                        0 | 1 => {
                            self.out = true;
                            self.phase = Phase::Done;
                        }
                        2 if self.phase == Phase::Strobe => {
                            self.out = true;
                            self.load();
                        }
                        3 => {
                            self.out = !self.out;
                            self.load();
                        }
                        2 | 4 | 5 if self.phase == Phase::Counting => {
                            self.out = false;
                            self.phase = Phase::Strobe;
                            self.remaining = 1;
                        }
                        _ => {
                            self.out = true;
                            self.phase = Phase::Done;
                        }
                    }
                }
            }
            rises += u64::from(!old_out && self.out);
        }
        rises
    }

    fn clocks_to_rise(&self) -> Option<u64> {
        let mode = self.mode();
        if !self.counting_enabled() {
            return (mode == 4 && self.phase == Phase::Strobe).then_some(1);
        }
        if self.load_pending {
            if !self.out && matches!(mode, 2..=5) {
                return Some(1);
            }
            return Some(1 + u64::from(self.reload_count()) + u64::from(mode >= 4));
        }
        let clocks = u64::from(self.remaining);
        match (mode, self.phase) {
            (_, Phase::Idle | Phase::Done) => None,
            (0 | 1, _) | (_, Phase::Strobe) => Some(clocks),
            (3, _) => Some(
                clocks
                    + if self.out {
                        u64::from(self.reload_count() / 2)
                    } else {
                        0
                    },
            ),
            _ => Some(clocks + 1),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Pit {
    counters: [Counter; 3],
    last_tick: u64,
    refresh: bool,
}

impl Pit {
    pub fn new(ticks: u64) -> Self {
        Self {
            counters: [Counter::new(true), Counter::new(true), Counter::new(false)],
            last_tick: ticks,
            refresh: false,
        }
    }

    /// Synchronize all counters before I/O or interrupt sampling. The machine
    /// drives IRQ0 to the final OUT level, replaying an edge if one was missed.
    pub fn advance(&mut self, ticks: u64) -> bool {
        let clocks = ticks.saturating_sub(self.last_tick) / CLOCK_DIVISOR;
        self.last_tick += clocks * CLOCK_DIVISOR;
        let irq = self.counters[0].advance(clocks) != 0;
        self.refresh ^= self.counters[1].advance(clocks) & 1 != 0;
        self.counters[2].advance(clocks);
        irq
    }

    pub fn next_irq_tick(&self) -> u64 {
        self.counters[0]
            .clocks_to_rise()
            .map_or(u64::MAX, |clocks| {
                self.last_tick
                    .saturating_add(clocks.saturating_mul(CLOCK_DIVISOR))
            })
    }

    pub fn irq_level(&self) -> bool {
        self.counters[0].out
    }

    pub fn port61_status(&self) -> u8 {
        (u8::from(self.refresh) << 4) | (u8::from(self.counters[2].out) << 5)
    }

    pub fn set_gate2(&mut self, high: bool) {
        self.counters[2].set_gate(high);
    }

    pub fn read(&mut self, address: u8) -> u8 {
        if address == 3 {
            0xff
        } else {
            self.counters[usize::from(address)].read()
        }
    }

    pub fn write(&mut self, address: u8, value: u8) {
        let old_refresh = self.counters[1].out;
        if address != 3 {
            self.counters[usize::from(address)].write(value);
        } else if value >> 6 == 3 {
            for (i, counter) in self.counters.iter_mut().enumerate() {
                if value & (2 << i) != 0 {
                    counter.latch(value & 0x20 == 0, value & 0x10 == 0);
                }
            }
        } else {
            let counter = &mut self.counters[usize::from(value >> 6)];
            if value & 0x30 == 0 {
                counter.latch(true, false);
            } else {
                counter.program(value);
            }
        }
        self.refresh ^= !old_refresh && self.counters[1].out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(pit: &mut Pit, channel: u8, control: u8, count: u16) {
        pit.write(3, channel << 6 | control);
        if control & 0x30 != 0x20 {
            pit.write(channel, count as u8);
        }
        if control & 0x30 != 0x10 {
            pit.write(channel, (count >> 8) as u8);
        }
    }

    fn count(pit: &mut Pit, channel: u8) -> u16 {
        pit.write(3, channel << 6);
        u16::from_le_bytes([pit.read(channel), pit.read(channel)])
    }

    fn status(pit: &mut Pit, channel: u8) -> u8 {
        pit.write(3, 0xe0 | (2 << channel));
        pit.read(channel)
    }

    #[test]
    fn output_and_count_waveforms_include_load_clock_and_odd_square_wave() {
        // Samples follow each CLK. A three-count one-shot goes high on CLK4;
        // rate mode pulses low on CLK3; square mode reads the even CE value.
        for (mode, expected) in [
            (
                0,
                &[
                    (3, false),
                    (2, false),
                    (1, false),
                    (0, true),
                    (65535, true),
                    (65534, true),
                ][..],
            ),
            (
                1,
                &[
                    (3, false),
                    (2, false),
                    (1, false),
                    (0, true),
                    (65535, true),
                    (65534, true),
                ][..],
            ),
            (
                2,
                &[
                    (3, true),
                    (2, true),
                    (1, false),
                    (3, true),
                    (2, true),
                    (1, false),
                ][..],
            ),
            (
                3,
                &[
                    (2, true),
                    (0, true),
                    (2, false),
                    (2, true),
                    (0, true),
                    (2, false),
                ][..],
            ),
            (
                4,
                &[
                    (3, true),
                    (2, true),
                    (1, true),
                    (0, false),
                    (65535, true),
                    (65534, true),
                ][..],
            ),
            (
                5,
                &[
                    (3, true),
                    (2, true),
                    (1, true),
                    (0, false),
                    (65535, true),
                    (65534, true),
                ][..],
            ),
        ] {
            let mut pit = Pit::new(0);
            program(&mut pit, 2, 0x30 | mode << 1, 3);
            pit.set_gate2(true);
            for (i, &(ce, out)) in expected.iter().enumerate() {
                pit.advance((i as u64 + 1) * CLOCK_DIVISOR);
                assert_eq!(
                    (count(&mut pit, 2), pit.port61_status() & 0x20 != 0),
                    (ce, out),
                    "mode {mode}, CLK {}",
                    i + 1
                );
            }
        }
    }

    #[test]
    fn gate_pauses_level_modes_and_retriggers_edge_modes() {
        let mut pit = Pit::new(0);
        program(&mut pit, 2, 0x30, 3);
        pit.advance(CLOCK_DIVISOR); // loading is allowed with GATE low
        assert_eq!(status(&mut pit, 2), 0x30);
        pit.advance(9 * CLOCK_DIVISOR);
        assert_eq!(count(&mut pit, 2), 3);
        pit.set_gate2(true);
        pit.advance(10 * CLOCK_DIVISOR);
        assert_eq!(count(&mut pit, 2), 2);
        pit.set_gate2(false);
        pit.advance(20 * CLOCK_DIVISOR);
        assert_eq!(count(&mut pit, 2), 2);
        pit.set_gate2(true);
        pit.advance(22 * CLOCK_DIVISOR);
        assert_eq!(pit.port61_status() & 0x20, 0x20);

        for mode in [1, 5] {
            let mut pit = Pit::new(0);
            program(&mut pit, 2, 0x30 | mode << 1, 3);
            pit.advance(100 * CLOCK_DIVISOR);
            assert_ne!(status(&mut pit, 2) & 0x40, 0, "wait for trigger");
            pit.set_gate2(true);
            pit.set_gate2(false); // the short rising edge must be retained
            pit.advance(102 * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 2), 2, "gate level does not pause one-shot");
            pit.set_gate2(true);
            pit.advance(103 * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 2), 3, "retrigger reloads on next CLK");
        }

        for mode in [2, 3] {
            let mut pit = Pit::new(0);
            program(&mut pit, 2, 0x30 | mode << 1, 4);
            pit.set_gate2(true);
            pit.advance((if mode == 2 { 4 } else { 3 }) * CLOCK_DIVISOR);
            assert_eq!(pit.port61_status() & 0x20, 0);
            pit.set_gate2(false);
            assert_eq!(pit.port61_status() & 0x20, 0x20, "gate low forces OUT high");
            let stopped = count(&mut pit, 2);
            pit.advance(30 * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 2), stopped);
            pit.set_gate2(true);
            pit.advance(31 * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 2), 4);
        }
    }

    #[test]
    fn periodic_reload_waits_for_cycle_or_half_cycle_boundary() {
        for mode in [2, 3] {
            let mut pit = Pit::new(0);
            program(&mut pit, 0, 0x30 | mode << 1, 6);
            pit.advance(2 * CLOCK_DIVISOR);
            let old_count = count(&mut pit, 0);
            pit.write(0, 10); // an incomplete new count cannot stop this cycle
            pit.advance(3 * CLOCK_DIVISOR);
            assert_eq!(
                count(&mut pit, 0),
                old_count - if mode == 3 { 2 } else { 1 }
            );
            pit.write(0, 0);
            assert_ne!(status(&mut pit, 0) & 0x40, 0);
            pit.advance((if mode == 2 { 7 } else { 4 }) * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 0), 10);
            assert_eq!(status(&mut pit, 0) & 0x40, 0);
        }
    }

    #[test]
    fn mode_zero_first_byte_stops_but_mode_four_first_byte_does_not() {
        for mode in [0, 4] {
            let mut pit = Pit::new(0);
            program(&mut pit, 2, 0x30 | mode << 1, 4);
            pit.set_gate2(true);
            pit.advance(2 * CLOCK_DIVISOR);
            pit.write(2, 5);
            pit.advance(4 * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 2), if mode == 0 { 3 } else { 1 });
            pit.write(2, 0);
            pit.advance(5 * CLOCK_DIVISOR);
            assert_eq!(count(&mut pit, 2), 5);
        }
    }

    #[test]
    fn count_and_status_latches_survive_time_and_interleaved_writes() {
        let mut pit = Pit::new(0);
        program(&mut pit, 0, 0x34, 0x1234);
        pit.advance(CLOCK_DIVISOR);
        pit.write(3, 0xc2); // both count and status of channel 0
        pit.advance(8 * CLOCK_DIVISOR);
        pit.write(3, 0xc2); // unread latches cannot be replaced
        assert_eq!(pit.read(0), 0xb4);
        assert_eq!(pit.read(0), 0x34);
        pit.write(0, 0x78);
        pit.advance(900 * CLOCK_DIVISOR);
        pit.write(3, 0); // cannot replace the remaining MSB either
        assert_eq!(pit.read(0), 0x12);
        pit.write(0, 0x56);
        assert_ne!(status(&mut pit, 0) & 0x40, 0);
        assert_eq!(count(&mut pit, 0), 0x1234 - 899);
        pit.advance(4661 * CLOCK_DIVISOR);
        assert_eq!(count(&mut pit, 0), 0x5678);
        assert_eq!(status(&mut pit, 0) & 0x40, 0);
    }

    #[test]
    fn byte_access_modes_and_multichannel_readback_keep_independent_state() {
        let mut pit = Pit::new(0);
        program(&mut pit, 0, 0x10, 0x34);
        program(&mut pit, 1, 0x20, 0x1200);
        program(&mut pit, 2, 0x30, 0x5678);
        pit.advance(CLOCK_DIVISOR);
        pit.write(3, 0xce); // latch all three counters and statuses at once
        pit.advance(100 * CLOCK_DIVISOR);
        for (channel, status) in [(2, 0x30), (0, 0x10), (1, 0x20)] {
            assert_eq!(pit.read(channel), status);
        }
        assert_eq!(pit.read(0), 0x34);
        assert_eq!(pit.read(1), 0x12);
        assert_eq!(pit.read(2), 0x78);
        assert_eq!(pit.read(2), 0x56);
        // LSB-only writes replace the whole CR, not just its low byte.
        program(&mut pit, 0, 0x10, 7);
        pit.advance(101 * CLOCK_DIVISOR);
        assert_eq!(pit.read(0), 7);
        assert_eq!(pit.read(0), 7);
        assert_eq!(pit.read(3), 0xff); // control port is write-only
    }

    #[test]
    fn zero_means_full_modulus_and_bcd_borrows_decimal_digits() {
        for (bcd, modulus) in [(0, 65_536), (1, 10_000)] {
            let mut pit = Pit::new(0);
            program(&mut pit, 0, 0x30 | bcd, 0);
            assert_eq!(pit.next_irq_tick(), (modulus + 1) * CLOCK_DIVISOR);
            assert!(!pit.advance(CLOCK_DIVISOR));
            assert_eq!(count(&mut pit, 0), 0);
            assert!(!pit.advance(modulus * CLOCK_DIVISOR));
            assert_eq!(count(&mut pit, 0), 1);
            assert!(pit.advance((modulus + 1) * CLOCK_DIVISOR));
            assert_eq!(pit.next_irq_tick(), u64::MAX);
            assert!(!pit.advance((modulus + 2) * CLOCK_DIVISOR));
            assert_eq!(count(&mut pit, 0), if bcd == 0 { 0xffff } else { 0x9999 });
        }
        let mut pit = Pit::new(0);
        program(&mut pit, 0, 0x31, 0x1000);
        pit.advance(2 * CLOCK_DIVISOR);
        assert_eq!(count(&mut pit, 0), 0x0999);
    }

    #[test]
    fn strobe_ends_while_gate_low_and_does_not_repeat_after_wrap() {
        let mut pit = Pit::new(0);
        program(&mut pit, 2, 0x38, 2);
        pit.set_gate2(true);
        pit.advance(3 * CLOCK_DIVISOR);
        assert_eq!(pit.port61_status() & 0x20, 0);
        pit.set_gate2(false);
        pit.advance(4 * CLOCK_DIVISOR);
        assert_eq!(pit.port61_status() & 0x20, 0x20);
        assert_eq!(count(&mut pit, 2), 0);
        pit.set_gate2(true);
        pit.advance(65_541 * CLOCK_DIVISOR);
        assert_eq!(pit.port61_status() & 0x20, 0x20);
        assert_eq!(count(&mut pit, 2), 0xffff);
    }

    #[test]
    fn bulk_elapsed_time_preserves_count_output_and_next_irq() {
        for mode in [0, 2, 3, 4, 6, 7] {
            for n in [2, 3, 4, 5, 18, 0x1234] {
                let mut bulk = Pit::new(5);
                program(&mut bulk, 0, 0x30 | mode << 1, n);
                program(&mut bulk, 1, 0x34, 18);
                let mut sampled = bulk.clone();
                let mut saw_edge = false;
                for clk in 1..=200 {
                    saw_edge |= sampled.advance(5 + clk * CLOCK_DIVISOR);
                }
                assert_eq!(bulk.advance(5 + 200 * CLOCK_DIVISOR), saw_edge);
                assert_eq!(count(&mut bulk, 0), count(&mut sampled, 0));
                assert_eq!(status(&mut bulk, 0), status(&mut sampled, 0));
                assert_eq!(bulk.port61_status(), sampled.port61_status());
                assert_eq!(bulk.next_irq_tick(), sampled.next_irq_tick());
            }
        }
        // A long host suspension must be constant work, not billions of CLK
        // or output iterations. The selected timestamp ends on CE=1, OUT low.
        let mut pit = Pit::new(0);
        program(&mut pit, 0, 0x34, 3);
        program(&mut pit, 1, 0x34, 3);
        assert!(pit.advance(3_000_000_000_000 * CLOCK_DIVISOR));
        assert_eq!(count(&mut pit, 0), 1);
        assert!(!pit.irq_level());
        // Channel 1 toggles the refresh flip-flop at programming and each rise.
        assert_eq!(pit.port61_status() & 0x10, 0);
        assert_eq!(pit.next_irq_tick(), 3_000_000_000_001 * CLOCK_DIVISOR);
    }
}
