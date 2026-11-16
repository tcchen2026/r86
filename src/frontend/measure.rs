//! Optional CLI phase timing. Sampling/reporting happens only at step boundaries.

use std::time::Instant;

pub struct Measurement {
    wall: Instant,
    cpu: Option<(f64, f64)>,
}

fn cpu_time() -> Result<Option<(f64, f64)>, String> {
    #[cfg(unix)]
    {
        // SAFETY: getrusage writes the complete, live output record.
        let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return Err(format!("getrusage: {}", std::io::Error::last_os_error()));
        }
        let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
        Ok(Some((seconds(usage.ru_utime), seconds(usage.ru_stime))))
    }
    #[cfg(not(unix))]
    {
        Ok(None)
    }
}

impl Measurement {
    pub fn start() -> Result<Self, String> {
        Ok(Self {
            cpu: cpu_time()?,
            wall: Instant::now(),
        })
    }

    pub fn finish(&self, step: u64) -> Result<(), String> {
        let wall = self.wall.elapsed().as_secs_f64();
        let cpu = cpu_time()?;
        print!("measurement end step={step} wall_seconds={wall:.6}");
        if let (Some((u0, s0)), Some((u1, s1))) = (self.cpu, cpu) {
            print!(" user_seconds={:.6} system_seconds={:.6}", u1 - u0, s1 - s0);
        }
        println!();
        Ok(())
    }
}
