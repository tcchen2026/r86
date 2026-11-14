//! Unclaimed PC bus accesses are floating reads / discarded writes, not CPU
//! exceptions. Only these cold dispatch exits update counters and log records;
//! `bus-instruction-context` additionally captures the instruction address before
//! fetching its bytes. `host` enables it; no_std firmware can opt in explicitly.
//! Without it, the bus hook is a compile-time no-op, removing per-instruction
//! address calculation/stores while keeping target counters and warning records.
//!
//! Count byte transactions by accessed 4 KiB page (I/O and MMIO separately),
//! and, when enabled, by the originating instruction's linear page (CS.base +
//! starting EIP, including prefixes). Either counter exceeding 1000 suppresses
//! the record.
//! Wide accesses split by the bus count their constituent bytes. Counts and
//! muting survive guest resets. A bounded pending ring prevents a stalled or
//! absent frontend from accumulating log memory. Accesses without an instruction
//! context use only the target-page limit.
use alloc::collections::{BTreeMap, VecDeque};
use core::fmt;

use super::HostBus;
use crate::x86::{Bus, Cpu, LinearAddr, SegmentReg};

pub const BUS_PAGE_LOG_LIMIT: u64 = 1000;
pub const UNDEFINED_OPCODE_LIMIT: usize = 1024;
const PENDING_LIMIT: usize = 256;

/// First #UD for an opcode outside the CPU model at this linear address.
/// Bytes are a 15-byte window, including prefixes; uncached bytes are unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UndefinedOpcode {
    pub address: u32,
    pub eip: u32,
    pub step: u64,
    pub count: u64,
    pub cs: u16,
    pub cpl: u8,
    pub bytes: [Option<u8>; 15],
}

impl fmt::Display for UndefinedOpcode {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "#UD cs={:#06x} eip={:#010x} linear={:#010x} cpl={} first_step={} count={} bytes=",
            self.cs, self.eip, self.address, self.cpl, self.step, self.count
        )?;
        for (i, byte) in self.bytes.iter().enumerate() {
            if i != 0 {
                out.write_str(" ")?;
            }
            match byte {
                Some(byte) => write!(out, "{byte:02x}")?,
                None => out.write_str("??")?,
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum BusSpace {
    Io,
    Mmio,
}
impl BusSpace {
    pub fn name(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Mmio => "mmio",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BusWarning {
    pub space: BusSpace,
    pub address: u64,
    pub write: bool,
    /// Written byte, or the floating read result (0xff).
    pub value: u8,
    pub step: u64,
    /// Total transactions on this page, including suppressed records.
    pub count: u64,
    pub logged: u64,
    /// CS.base + instruction-start EIP when `bus-instruction-context` is enabled;
    /// otherwise None. An access outside CPU execution may also lack a context.
    pub instruction: Option<u32>,
    pub instruction_count: u64,
}

#[derive(Default)]
pub struct BusCounter {
    pub count: u64,
    pub logged: u64,
}

#[derive(Default)]
pub(super) struct BusDiagnostics {
    pub pages: BTreeMap<(BusSpace, u64), BusWarning>,
    pub instructions: BTreeMap<u32, BusCounter>,
    pub pending: VecDeque<BusWarning>,
    pub instruction: Option<u32>,
    pub undefined_opcodes: BTreeMap<u32, UndefinedOpcode>,
    pub pending_undefined_opcodes: VecDeque<u32>,
}
impl BusDiagnostics {
    #[cold]
    pub fn record(&mut self, space: BusSpace, address: u64, write: bool, value: u8, step: u64) {
        let last = self
            .pages
            .entry((space, address & !4095))
            .or_insert(BusWarning {
                space,
                address,
                write,
                value,
                step,
                count: 0,
                logged: 0,
                instruction: self.instruction,
                instruction_count: 0,
            });
        let source = self
            .instruction
            .map(|pc| self.instructions.entry(pc & !4095).or_default());
        let instruction_count = source.as_ref().map_or(0, |s| s.count.saturating_add(1));
        let count = last.count.saturating_add(1);
        let emit = count <= BUS_PAGE_LOG_LIMIT && instruction_count <= BUS_PAGE_LOG_LIMIT;
        if let Some(source) = source {
            source.count = instruction_count;
            source.logged += u64::from(emit);
        }
        *last = BusWarning {
            space,
            address,
            write,
            value,
            step,
            count,
            logged: last.logged + u64::from(emit),
            instruction: self.instruction,
            instruction_count,
        };
        if emit {
            if self.pending.len() == PENDING_LIMIT {
                self.pending.pop_front();
            }
            self.pending.push_back(*last);
        }
    }
}

/// Called only for StepError::UndefinedOpcode. Reuse executable RAM's
/// cached backing: diagnostics must not walk page tables, set A/D bits, or
/// read MMIO. In particular, a window crossing an uncached page stays partial.
#[cold]
pub(super) fn record_undefined_opcode(cpu: &mut Cpu, bus: &mut HostBus, eip: u32, step: u64) {
    let cs = cpu.seg(SegmentReg::Cs);
    let address = cs.base.wrapping_add(eip);
    if let Some(first) = bus.diagnostics.undefined_opcodes.get_mut(&address) {
        first.count = first.count.saturating_add(1);
        return;
    }
    if bus.diagnostics.undefined_opcodes.len() == UNDEFINED_OPCODE_LIMIT {
        return;
    }
    let mut bytes = [None; 15];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        if eip.wrapping_add(offset as u32) > cs.limit {
            break;
        }
        let Some(hit) = cpu.fetch_cache_lookup(LinearAddr(address.wrapping_add(offset as u32)))
        else {
            break;
        };
        *byte = Some(bus.read_backing_token_u8(hit.backing));
    }
    bus.diagnostics.undefined_opcodes.insert(
        address,
        UndefinedOpcode {
            address,
            eip,
            step,
            count: 1,
            cs: cs.selector,
            cpl: cpu.cpl(),
            bytes,
        },
    );
    if bus.diagnostics.pending_undefined_opcodes.len() == PENDING_LIMIT {
        bus.diagnostics.pending_undefined_opcodes.pop_front();
    }
    bus.diagnostics.pending_undefined_opcodes.push_back(address);
}
