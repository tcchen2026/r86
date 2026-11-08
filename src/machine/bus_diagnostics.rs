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

pub const BUS_PAGE_LOG_LIMIT: u64 = 1000;

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
            if self.pending.len() == 256 {
                self.pending.pop_front();
            }
            self.pending.push_back(*last);
        }
    }
}
