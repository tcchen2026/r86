//! Host-only cache traffic estimate, never an execution or timing authority.
//! Two separate 64 KiB, two-way, 32-byte-line caches. Stores allocate; FENCE.I
//! empties instruction tags. Replacement alternates globally as in the current
//! RTL sequencer. No speculation, writeback clocks, replay or pipeline model.

const SETS: usize = 1024;
const EMPTY: u32 = u32::MAX;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Estimate {
    instruction: Cache,
    data: Cache,
    turn: usize,
    pub active: bool,
    data_phase: bool,
    pc: u32,
}
struct Cache {
    tags: Box<[[u32; 2]; SETS]>,
    accesses: u64,
    fills: [u64; 3],
    profile: Option<BTreeMap<u32, [u64; 3]>>,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            tags: Box::new([[EMPTY; 2]; SETS]),
            accesses: 0,
            fills: [0; 3],
            profile: None,
        }
    }
}
impl Cache {
    fn access(&mut self, addr: u32, bytes: usize, turn: &mut usize, pc: u32) {
        if addr & 0x8000_0000 != 0 || bytes == 0 {
            return;
        }
        let last = (u64::from(addr) + bytes as u64 - 1) >> 5;
        for line in u64::from(addr >> 5)..=last {
            self.accesses += 1;
            let row = &mut self.tags[line as usize & (SETS - 1)];
            let tag = line as u32;
            if row.contains(&tag) {
                continue;
            }
            let way = row.iter().position(|&n| n == EMPTY).unwrap_or(*turn);
            row[way] = tag;
            *turn ^= 1;
            let addr = line << 5;
            let region = if addr < 0x0200_0000 {
                0
            } else if addr < 0x1000_0000 {
                1
            } else {
                2
            };
            self.fills[region] += 1;
            if let Some(profile) = &mut self.profile {
                profile.entry(pc).or_default()[region] += 1;
            }
        }
    }
    fn json(&self) -> String {
        format!(
            "{{\"accesses\":{},\"fills\":{{\"firmware\":{},\"heap\":{},\"guest\":{}}}}}",
            self.accesses, self.fills[0], self.fills[1], self.fills[2]
        )
    }
}
impl Estimate {
    pub fn fetch(&mut self, pc: u32) {
        self.pc = pc;
        if self.active {
            self.instruction.access(pc, 4, &mut self.turn, pc);
        }
        self.data_phase = false;
    }
    pub fn operands(&mut self) {
        self.data_phase = true;
    }
    pub fn data(&mut self, addr: u32, bytes: usize) {
        if self.active && self.data_phase {
            self.data.access(addr, bytes, &mut self.turn, self.pc);
        }
    }
    pub fn fence_i(&mut self) {
        if self.active {
            self.instruction.tags.fill([EMPTY; 2]);
        }
    }
    pub fn json(&self, retired: u64) -> String {
        let fills: u64 = self.instruction.fills.iter().chain(&self.data.fills).sum();
        format!(
            "{{\"line_bytes\":32,\"bytes_per_side\":65536,\"ways\":2,\"instruction\":{},\"data\":{},\"line_fills\":{},\"estimated_clocks\":{:.2}}}",
            self.instruction.json(),
            self.data.json(),
            fills,
            retired as f64 * 1.15 + fills as f64 * 45.0
        )
    }
    pub fn profile(&mut self) {
        self.instruction.profile = Some(BTreeMap::new());
        self.data.profile = Some(BTreeMap::new());
    }
    pub fn write_profile(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut rows = BTreeMap::<u32, [u64; 6]>::new();
        for (side, cache) in [&self.instruction, &self.data].iter().enumerate() {
            if let Some(profile) = &cache.profile {
                let mut sums = [0; 3];
                for (&pc, fills) in profile {
                    rows.entry(pc).or_default()[side * 3..side * 3 + 3].copy_from_slice(fills);
                    for region in 0..3 {
                        sums[region] += fills[region];
                    }
                }
                assert_eq!(sums, cache.fills);
            }
        }
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(
            out,
            "pc,i_firmware,i_heap,i_guest,d_firmware,d_heap,d_guest"
        )?;
        for (pc, f) in rows {
            writeln!(
                out,
                "0x{pc:08x},{},{},{},{},{},{}",
                f[0], f[1], f[2], f[3], f[4], f[5]
            )?;
        }
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_estimate_hits_conflicts_spans_and_observation_exclusion() {
        let mut e = Estimate::default();
        e.fetch(0);
        e.operands();
        e.data(0x1000_0000, 4);
        assert_eq!(e.instruction.accesses + e.data.accesses, 0);
        e.active = true;
        e.fetch(0);
        e.fetch(4); // Same line; reads used to fetch it are not data.
        e.data(0, 4);
        assert_eq!(e.instruction.fills, [1, 0, 0]);
        assert_eq!(e.data.accesses, 0);
        e.operands();
        e.data(0x0200_001f, 4); // Two heap lines.
        e.data(0x1000_0000, 4);
        assert_eq!(e.data.fills, [0, 2, 1]);
        e.fence_i();
        e.fetch(0);
        e.operands();
        e.data(0x1000_0000, 4);
        assert_eq!(e.instruction.fills, [2, 0, 0]);
        assert_eq!(e.data.fills, [0, 2, 1]);
        let mut c = Cache::default();
        let mut turn = 0;
        c.access(0, 4, &mut turn, 0);
        c.access(32768, 4, &mut turn, 0);
        c.access(0, 4, &mut turn, 0);
        c.access(65536, 4, &mut turn, 0);
        assert_eq!(c.fills[0], 3);
        c.access(0, 4, &mut turn, 0);
        assert_eq!(c.fills[0], 4);
    }
}
