//! Device time is independent of CPU throughput. The host clock is sampled at
//! device/interrupt boundaries, never by the ordinary instruction or RAM path.
//! The instruction clock remains available for reproducible tests and replay.
//! Both use the same oscillator units so devices have only one implementation.

pub(super) use crate::devices::TICKS_PER_SECOND;
#[cfg(any(
    feature = "host",
    all(feature = "web-frontend", target_arch = "wasm32")
))]
const POLL_INSTRUCTIONS: u64 = 8192;

#[derive(Debug, Default)]
pub(super) struct Clock {
    #[cfg(any(
        feature = "host",
        all(feature = "web-frontend", target_arch = "wasm32")
    ))]
    realtime: Option<Realtime>,
}

#[cfg(any(
    feature = "host",
    all(feature = "web-frontend", target_arch = "wasm32")
))]
#[derive(Debug)]
struct Realtime {
    #[cfg(feature = "host")]
    origin: std::time::Instant,
    #[cfg(not(feature = "host"))]
    origin: f64,
    ticks: u64,
    tsc_offset: u64,
}

#[cfg(any(
    feature = "host",
    all(feature = "web-frontend", target_arch = "wasm32")
))]
impl Realtime {
    fn elapsed_ns(&self) -> u128 {
        #[cfg(feature = "host")]
        {
            self.origin.elapsed().as_nanos()
        }
        #[cfg(not(feature = "host"))]
        {
            ((crate::browser::now_ms() - self.origin) * 1_000_000.0) as u128
        }
    }
}

impl Clock {
    #[cfg(any(
        feature = "host",
        all(feature = "web-frontend", target_arch = "wasm32")
    ))]
    pub fn realtime() -> Self {
        Self {
            realtime: Some(Realtime {
                #[cfg(feature = "host")]
                origin: std::time::Instant::now(),
                #[cfg(not(feature = "host"))]
                origin: crate::browser::now_ms(),
                ticks: 0,
                tsc_offset: 0,
            }),
        }
    }

    pub fn is_realtime(&self) -> bool {
        #[cfg(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        ))]
        {
            self.realtime.is_some()
        }
        #[cfg(not(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        )))]
        {
            false
        }
    }

    pub fn sample(&mut self) {
        #[cfg(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        ))]
        if let Some(clock) = &mut self.realtime {
            clock.ticks =
                (clock.elapsed_ns() * u128::from(TICKS_PER_SECOND) / 1_000_000_000) as u64;
        }
    }

    pub fn ticks(&self, instruction_step: u64) -> u64 {
        #[cfg(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        ))]
        if let Some(clock) = &self.realtime {
            return clock.ticks;
        }
        instruction_step
    }

    pub fn poll_step(&self, instruction_step: u64, device_deadline: u64) -> u64 {
        #[cfg(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        ))]
        if self.realtime.is_some() {
            return instruction_step.saturating_add(POLL_INSTRUCTIONS);
        }
        let _ = instruction_step;
        device_deadline
    }

    /// Fixed 1 GHz TSC in realtime mode. This is a clock frequency, not an
    /// instruction quota. The CPU's retired-instruction accounting stays intact.
    pub fn read_tsc(&self, instruction_tsc: u64) -> u64 {
        #[cfg(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        ))]
        if let Some(clock) = &self.realtime {
            return (clock.elapsed_ns() as u64).wrapping_add(clock.tsc_offset);
        }
        instruction_tsc
    }

    pub fn write_tsc(&mut self, value: u64) {
        #[cfg(any(
            feature = "host",
            all(feature = "web-frontend", target_arch = "wasm32")
        ))]
        if let Some(clock) = &mut self.realtime {
            clock.tsc_offset = value.wrapping_sub(clock.elapsed_ns() as u64);
        }
        let _ = value;
    }

    #[cfg(feature = "host")]
    pub fn wait_duration(&self, deadline: u64) -> std::time::Duration {
        use std::time::Duration;
        let Some(clock) = &self.realtime else {
            return Duration::from_millis(1);
        };
        let now = clock.origin.elapsed().as_nanos() * u128::from(TICKS_PER_SECOND) / 1_000_000_000;
        let remaining = u128::from(deadline).saturating_sub(now);
        Duration::from_nanos(
            (remaining * 1_000_000_000 / u128::from(TICKS_PER_SECOND)).min(1_000_000) as u64,
        )
    }
}
