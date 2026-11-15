//! Opt-in PM16 instruction mix. Observation never fills or watches a fetch
//! translation. Counts are attempted interpreter steps, including REP chunks.
use super::{Cpu, LinearAddr, SegmentReg, profile_key, profile_name};
use crate::x86::cpu::{CR0_PE, SEGMENT_FLAG_DEFAULT_BIG};
use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};
use core::fmt::Write;

pub(super) const ENABLED: bool = option_env!("R86_T32_PROFILE_CODE16").is_some();
const CLASSES: [&str; 9] = ["F", "S", "I", "P", "R", "X", "O", "plain", "unknown"];

pub(super) fn pm16(cpu: &Cpu) -> bool {
    cpu.cr0 & CR0_PE != 0 && cpu.eflags & (1 << 17) == 0 && cpu.code_default16()
}

/// None means that even the opcode/group bytes are not in this hit window.
fn classify(bytes: &[u8]) -> Option<(u16, usize, [bool; 3])> {
    let at = bytes.iter().position(|b| {
        !matches!(
            b,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64..=0x67 | 0xf0 | 0xf2 | 0xf3
        )
    })?;
    let prefixes = &bytes[..at];
    let op = bytes[at];
    let op2 = if op == 0x0f { *bytes.get(at + 1)? } else { 0 };
    let group = if matches!(op, 0xc0 | 0xc1 | 0xd0..=0xd3 | 0xf6 | 0xf7 | 0xfe | 0xff)
        || (op == 0x0f && op2 == 0xba)
    {
        Some(bytes.get(at + 1 + usize::from(op == 0x0f))? >> 3 & 7)
    } else {
        None
    };
    let repeat = prefixes.iter().any(|b| matches!(b, 0xf2 | 0xf3));
    let class = if matches!(op, 0x9a | 0xea | 0xca | 0xcb)
        || (op == 0xff && matches!(group, Some(3 | 5)))
    {
        0
    } else if matches!(op, 0x8e | 0x07 | 0x17 | 0x1f | 0xc4 | 0xc5)
        || (op == 0x0f && matches!(op2, 0xa1 | 0xa9 | 0xb2 | 0xb4 | 0xb5))
    {
        1
    } else if matches!(op, 0xcc..=0xcf) {
        2
    } else if matches!(op, 0x9c | 0x9d | 0xfa | 0xfb) {
        3
    } else if repeat && matches!(op, 0x6c..=0x6f | 0xa4..=0xa7 | 0xaa..=0xaf) {
        4
    } else if matches!(op, 0xd8..=0xdf | 0x9b) {
        5
    } else if matches!(op, 0xe4..=0xe7 | 0xec..=0xef | 0x6c..=0x6f | 0xf4 | 0x63 | 0x62)
        || (op == 0x0f && matches!(op2, 0x00..=0x0f | 0x20..=0x23 | 0x30..=0x33))
    {
        6
    } else {
        7
    };
    Some((
        profile_key(bytes),
        class,
        [
            prefixes.contains(&0x67),
            repeat,
            prefixes
                .iter()
                .any(|b| matches!(b, 0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65)),
        ],
    ))
}

pub(super) struct Counters {
    opcodes: Vec<u64>,
    opcode_classes: Vec<[u64; 8]>,
    classes: [u64; 9],
    prefixes: [u64; 3],
    runs: [[u64; 11]; 3], // Barrier, leaving PM16, unknown bytes.
    run_steps: [u64; 3],
    run: u64,
    active: bool,
    segments: BTreeMap<(u16, u32), u64>,
    capped: bool,
    previous_base: Option<u32>,
    base_changes: u64,
    cpl_ss: [[u64; 2]; 4],
}
impl Default for Counters {
    fn default() -> Self {
        Self {
            opcodes: if ENABLED { vec![0; 2048] } else { Vec::new() },
            opcode_classes: if ENABLED {
                vec![[0; 8]; 2048]
            } else {
                Vec::new()
            },
            classes: [0; 9],
            prefixes: [0; 3],
            runs: [[0; 11]; 3],
            run_steps: [0; 3],
            run: 0,
            active: false,
            segments: BTreeMap::new(),
            capped: false,
            previous_base: None,
            base_changes: 0,
            cpl_ss: [[0; 2]; 4],
        }
    }
}
impl Counters {
    fn end_run(&mut self, cause: usize) {
        let bucket = match self.run {
            0..=4 => self.run as usize,
            5..=6 => 5,
            7..=9 => 6,
            10..=14 => 7,
            15..=24 => 8,
            25..=49 => 9,
            _ => 10,
        };
        self.runs[cause][bucket] += 1;
        self.run_steps[cause] += self.run;
        self.run = 0;
    }
    pub(super) fn leave(&mut self) {
        if self.active {
            self.end_run(1);
            self.active = false;
        }
    }
    pub(super) fn observe(&mut self, cpu: &mut Cpu) {
        self.active = true;
        let cs = cpu.seg(SegmentReg::Cs);
        self.base_changes += u64::from(self.previous_base.is_some_and(|base| base != cs.base));
        self.previous_base = Some(cs.base);
        let key = (cs.selector, cs.base);
        if let Some(count) = self.segments.get_mut(&key) {
            *count += 1;
        } else if self.segments.len() < 4096 {
            self.segments.insert(key, 1);
        } else {
            self.capped = true;
        }
        self.cpl_ss[cpu.cpl() as usize]
            [usize::from(cpu.seg(SegmentReg::Ss).flags & SEGMENT_FLAG_DEFAULT_BIG != 0)] += 1;
        let linear = cs.base.wrapping_add(cpu.eip);
        let observed = if cpu.eip > cs.limit || cpu.halted {
            None
        } else {
            cpu.fetch_cache_lookup(LinearAddr(linear)).and_then(|hit| {
                let len = 15usize
                    .min(4096 - (linear as usize & 4095))
                    .min((u64::from(cs.limit) - u64::from(cpu.eip) + 1).min(15) as usize);
                // SAFETY: HostBus fetch hits certify a stable ordinary-memory
                // backing pointer. This bounded, read-only view stays within
                // that page and is dropped before run_one may change anything.
                // No jit_code_stamp/watch, page walk, cache fill or A/D write.
                let bytes = unsafe { core::slice::from_raw_parts(hit.backing.0 as *const u8, len) };
                classify(bytes)
            })
        };
        if let Some((key, class, prefixes)) = observed {
            self.opcodes[key as usize] += 1;
            self.opcode_classes[key as usize][class] += 1;
            self.classes[class] += 1;
            for (count, present) in self.prefixes.iter_mut().zip(prefixes) {
                *count += u64::from(present);
            }
            if class == 3 || class == 7 {
                self.run += 1;
            } else {
                self.end_run(0);
            }
        } else {
            self.classes[8] += 1;
            self.end_run(2);
        }
    }
    pub(super) fn report(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "t32 pm16 summary: steps={} unknown={} addr67={} rep={} segment_override={} base_changes={} pairs={} capped={} pending_run={}",
            self.classes.iter().sum::<u64>(),
            self.classes[8],
            self.prefixes[0],
            self.prefixes[1],
            self.prefixes[2],
            self.base_changes,
            self.segments.len(),
            self.capped,
            self.run
        );
        for (name, count) in CLASSES.iter().zip(self.classes) {
            let _ = writeln!(out, "t32 pm16 class {name}: {count}");
        }
        for (key, &count) in self
            .opcodes
            .iter()
            .enumerate()
            .filter(|(_, count)| **count != 0)
        {
            let _ = write!(
                out,
                "t32 pm16 opcode {}: count={count}",
                profile_name(key as u16)
            );
            for (class, count) in CLASSES[..8].iter().zip(self.opcode_classes[key]) {
                if count != 0 {
                    let _ = write!(out, " {class}={count}");
                }
            }
            out.push('\n');
        }
        for (cause, bins) in self.runs.iter().enumerate() {
            let _ = write!(
                out,
                "t32 pm16 runs {}: count={} steps={}",
                ["barrier", "leave", "unknown"][cause],
                bins.iter().sum::<u64>(),
                self.run_steps[cause]
            );
            for (label, n) in [
                "0", "1", "2", "3", "4", "5-6", "7-9", "10-14", "15-24", "25-49", "50+",
            ]
            .into_iter()
            .zip(bins)
            {
                let _ = write!(out, " {label}={n}");
            }
            out.push('\n');
        }
        // All pairs, so the host can subtract before taking the top ten.
        for ((selector, base), steps) in &self.segments {
            let _ = writeln!(
                out,
                "t32 pm16 segment: selector={selector:04x} base={base:08x} steps={steps}"
            );
        }
        for (cpl, counts) in self.cpl_ss.iter().enumerate() {
            for (big, steps) in counts.iter().enumerate() {
                let _ = writeln!(out, "t32 pm16 stack: cpl={cpl} big={big} steps={steps}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arm_jit_pm16_profile_classifies_prefixes_groups_and_short_windows() {
        for (bytes, class) in [
            (&[0xff, 0x18][..], 0),
            (&[0xff, 0x10], 7),
            (&[0x0f, 0xb2], 1),
            (&[0xcf], 2),
            (&[0x66, 0x9d], 3),
            (&[0xf3, 0x67, 0xa5], 4),
            (&[0x9b], 5),
            (&[0x6d], 6),
            (&[0xf2, 0x90], 7),
        ] {
            assert_eq!(classify(bytes).unwrap().1, class);
        }
        assert_eq!(classify(&[0x2e, 0x67, 0xf3, 0xa4]).unwrap().2, [true; 3]);
        for bytes in [&[][..], &[0x66], &[0xff], &[0x0f], &[0x0f, 0xba]] {
            assert!(classify(bytes).is_none());
        }
        assert_eq!(classify(&[0x66, 0x50]).unwrap().0, 0x450);
    }

    #[test]
    fn arm_jit_pm16_profile_observation_preserves_fetch_cache_and_page_bounds() {
        use crate::x86::{BackingToken, PhysAddr, cpu::SegmentCache};
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);
        cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::new(8, 0, 0xffff, 0x9b));
        let mut bytes = vec![0x90u8; 4096];
        bytes[4095] = 0xff; // The ModRM byte is outside the cached page.
        cpu.fill_fetch_cache_with_backing(
            LinearAddr(0),
            PhysAddr(0),
            BackingToken(bytes.as_ptr() as u64),
            false,
        );
        let before = cpu.fetch_cache;
        let mut counts = Counters {
            opcodes: vec![0; 2048],
            opcode_classes: vec![[0; 8]; 2048],
            ..Counters::default()
        };
        for eip in [0xfff, 0x4000] {
            cpu.eip = eip;
            counts.observe(&mut cpu);
            assert_eq!(cpu.fetch_cache, before); // Neither a fill nor a generation change.
        }
        assert_eq!(counts.classes[8], 2);
        assert_eq!(counts.opcodes.iter().sum::<u64>(), 0);
        for eip in 0..6 {
            cpu.eip = eip;
            counts.observe(&mut cpu);
        }
        assert_eq!(counts.classes[7], 6);
        counts.leave();
        assert_eq!(counts.runs[1][5], 1);
        assert_eq!(counts.run_steps[1], 6);
        assert_eq!(cpu.fetch_cache, before);
    }
}
