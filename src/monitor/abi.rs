pub const ABI_MAGIC: u32 = 0x4836_3852; // "R86H", little-endian.
pub const ABI_VERSION: u16 = 1;
pub const PAGE_SIZE_4K: u64 = 4096;
pub const PAGE_MASK_4K: u64 = !(PAGE_SIZE_4K - 1);
pub const MAX_REGION_ENTRIES: usize = 16;
pub const BUILD_ID_BYTES: usize = 32;
pub const VIRTUAL_TIMER_PERIOD_TICKS: u64 = 100_000;

// Keep ARM IPA zero unused. In particular, affected GICv2 KVM hosts intercept
// low-IPA faults even without a virtual GIC. x86 physical addresses stay zero-based.
pub const X86_RAM_IPA_BASE: u64 = 0x2_0000_0000;
pub const MONITOR_IPA_BASE: u64 = 0x1_0000_0000;
pub const SHARED_IPA_BASE: u64 = 0x1_4000_0000;
pub const TRACE_IPA_BASE: u64 = 0x1_5000_0000;
pub const DOORBELL_IPA: u64 = 0x1_f000_0000;
pub const MIN_ARM_IPA_BITS: u8 = 34;

pub const TTBR0_GUARD_END: u64 = 0x10_0000_0000;
pub const SHADOW_SUP_BASE: u64 = 0x10_0000_0000;
pub const SHADOW_USR_BASE: u64 = 0x20_0000_0000;
pub const SHADOW_WINDOW_SIZE: u64 = 0x1_0000_0000;

pub const TTBR1_MONITOR_BASE: u64 = 0xffff_ff80_0000_0000;
pub const TTBR1_STACKS_BASE: u64 = 0xffff_ff80_4000_0000;
pub const TTBR1_SHADOW_POOL_BASE: u64 = 0xffff_ff80_8000_0000;
pub const TTBR1_DIRECT_MAP_BASE: u64 = 0xffff_ff81_0000_0000;
pub const TTBR1_SHARED_BASE: u64 = 0xffff_ff82_0000_0000;
pub const TTBR1_TRACE_BASE: u64 = 0xffff_ff82_4000_0000;
pub const TTBR1_DOORBELL_BASE: u64 = 0xffff_ff82_8000_0000;

pub const FEATURE_TRACE_RING: u64 = 1 << 0;
pub const FEATURE_SNAPSHOT: u64 = 1 << 1;
pub const FEATURE_DMA_COMMANDS: u64 = 1 << 2;
pub const FEATURE_DEBUG_STATE_BLOCK: u64 = 1 << 3;
pub const FEATURE_PANIC_BLOCK: u64 = 1 << 4;
/// Shared Machine owns PC state in EL1; region entries describe its backing.
pub const FEATURE_MACHINE_DEVICES: u64 = 1 << 5;
/// At stop, the trace slot contains the complete shared PC diagnostic state.
pub const FEATURE_MACHINE_STATE_V1: u64 = 1 << 6;
/// The VMM retains original boot inputs and services ReloadBoot while stopped.
pub const FEATURE_MACHINE_BOOT_RELOAD: u64 = 1 << 7;
pub const MACHINE_STATE_MAGIC: u64 = 0x3154_5350_3638_3652;
pub const FEATURE_NATIVE_READS: u64 = 1 << 8;
pub const FEATURE_NATIVE_WRITES: u64 = 1 << 9;

pub mod debug {
    use super::PAGE_SIZE_4K;

    pub const DEBUG_MAGIC: u32 = 0x4436_3852; // "R86D", little-endian.
    pub const PANIC_MAGIC: u32 = 0x5036_3852; // "R86P", little-endian.
    pub const DEBUG_ABI_VERSION: u32 = 3;
    pub const DEBUG_STATE_BLOCK_OFFSET: u64 = PAGE_SIZE_4K * 3;
    pub const PANIC_BLOCK_OFFSET: u64 = PAGE_SIZE_4K * 4;
    pub const DEBUG_STATE_FIELDS_BASE: u32 = 1 << 0;
    pub const DEBUG_STATE_FIELDS_CPU: u32 = 1 << 1;
    pub const DEBUG_STATE_FIELDS_EVENTS: u32 = 1 << 2;
    pub const DEBUG_STATE_FIELDS_HARDMMU: u32 = 1 << 3;
    pub const DEBUG_STATE_FIELDS_CACHE: u32 = 1 << 4;
    pub const DEBUG_STATE_FIELDS_HISTOGRAM: u32 = 1 << 5;
    pub const DEBUG_STATE_FIELDS_ALL: u32 = DEBUG_STATE_FIELDS_BASE
        | DEBUG_STATE_FIELDS_CPU
        | DEBUG_STATE_FIELDS_EVENTS
        | DEBUG_STATE_FIELDS_HARDMMU
        | DEBUG_STATE_FIELDS_CACHE
        | DEBUG_STATE_FIELDS_HISTOGRAM;

    #[repr(u32)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum DebugPhaseV1 {
        Unknown = 0,
        Stage1Enable = 1,
        Hello = 2,
        X86Step = 3,
        X86Halted = 4,
        Stopping = 5,
        Panic = 6,
    }

    #[repr(u32)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum PanicCodeV1 {
        None = 0,
        RustPanic = 1,
        SyncException = 2,
        Thunk = 3,
        Assert = 4,
    }

    /// Monitor-written, host-read debug telemetry.
    ///
    /// This block is observability only. It is not an architectural state source,
    /// must not be used to build snapshots, and must not be consumed by monitor
    /// control flow. The monitor publishes it with a seqlock-style `seq` field.
    #[repr(C, align(8))]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct DebugStateBlockV1 {
        pub magic: u32,
        pub version: u32,
        pub size: u32,
        pub fields_valid: u32,
        pub seq: u64,
        pub heartbeat: u64,
        pub phase: u32,
        pub panic_flag: u32,
        pub steps: u64,
        pub vtime: u64,
        pub eip: u32,
        pub eflags: u32,
        pub cr0: u32,
        pub cr2: u32,
        pub cr3: u32,
        pub cr4: u32,
        pub cpl_halted_if: u32,
        pub last_exc_vector: u32,
        pub last_exc_error: u32,
        pub last_mailbox_kind: u32,
        pub last_mailbox_seq: u64,
        pub last_pio_port_dir: u32,
        pub reserved_event0: u32,
        pub last_mmio_gpa: u64,
        pub stage1_installs: u64,
        pub stage1_leaf_invalidations: u64,
        pub shadow_flushes: u64,
        pub shadow_flush_by_reason: [u64; 4],
        pub shadow_translations: u64,
        pub shadow_eligible_4k: u64,
        pub shadow_read_fills: u64,
        pub shadow_write_fills: u64,
        pub shadow_upgrades: u64,
        pub shadow_unchanged_fills: u64,
        pub shadow_soft_counts: [u64; 4],
        pub shadow_page_table_write_invalidations: u64,
        pub shadow_page_walk_write_ignores: u64,
        pub shadow_errors: u64,
        pub native_reads: u64,
        pub native_read_hits: u64,
        pub native_read_aborts: u64,
        pub fetch_cache: [u64; 3],
        pub linear_cache: [u64; 3],
        pub irq_injections: u64,
        pub hlt_wakes: u64,
        pub timer_polls: u64,
        pub serial_bytes: u64,
        pub trace_write_index: u64,
        pub trace_dropped: u64,
        pub eip_histogram: [u32; 64],
    }

    impl DebugStateBlockV1 {
        pub const fn new() -> Self {
            Self {
                magic: DEBUG_MAGIC,
                version: DEBUG_ABI_VERSION,
                size: core::mem::size_of::<Self>() as u32,
                fields_valid: DEBUG_STATE_FIELDS_ALL,
                seq: 0,
                heartbeat: 0,
                phase: DebugPhaseV1::Unknown as u32,
                panic_flag: 0,
                steps: 0,
                vtime: 0,
                eip: 0,
                eflags: 0,
                cr0: 0,
                cr2: 0,
                cr3: 0,
                cr4: 0,
                cpl_halted_if: 0,
                last_exc_vector: 0,
                last_exc_error: 0,
                last_mailbox_kind: 0,
                last_mailbox_seq: 0,
                last_pio_port_dir: 0,
                reserved_event0: 0,
                last_mmio_gpa: 0,
                stage1_installs: 0,
                stage1_leaf_invalidations: 0,
                shadow_flushes: 0,
                shadow_flush_by_reason: [0; 4],
                shadow_translations: 0,
                shadow_eligible_4k: 0,
                shadow_read_fills: 0,
                shadow_write_fills: 0,
                shadow_upgrades: 0,
                shadow_unchanged_fills: 0,
                shadow_soft_counts: [0; 4],
                shadow_page_table_write_invalidations: 0,
                shadow_page_walk_write_ignores: 0,
                shadow_errors: 0,
                native_reads: 0,
                native_read_hits: 0,
                native_read_aborts: 0,
                fetch_cache: [0; 3],
                linear_cache: [0; 3],
                irq_injections: 0,
                hlt_wakes: 0,
                timer_polls: 0,
                serial_bytes: 0,
                trace_write_index: 0,
                trace_dropped: 0,
                eip_histogram: [0; 64],
            }
        }
    }

    impl Default for DebugStateBlockV1 {
        fn default() -> Self {
            Self::new()
        }
    }

    #[repr(C, align(8))]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PanicBlockV1 {
        pub magic: u32,
        pub version: u32,
        pub size: u32,
        pub panic_code: u32,
        pub in_panic_depth: u32,
        pub reserved0: u32,
        pub esr: u64,
        pub far: u64,
        pub elr: u64,
        pub spsr: u64,
        pub sp: u64,
        pub xregs: [u64; 31],
        pub x86_eip: u32,
        pub x86_eflags: u32,
        pub x86_cr: [u32; 5],
        pub reserved1: u32,
        pub steps: u64,
        pub last_phase: u32,
        pub last_mailbox_kind: u32,
        pub trace_write_index: u64,
        pub breadcrumb: [u64; 3],
    }

    impl PanicBlockV1 {
        pub const fn new() -> Self {
            Self {
                magic: PANIC_MAGIC,
                version: DEBUG_ABI_VERSION,
                size: core::mem::size_of::<Self>() as u32,
                panic_code: PanicCodeV1::None as u32,
                in_panic_depth: 0,
                reserved0: 0,
                esr: 0,
                far: 0,
                elr: 0,
                spsr: 0,
                sp: 0,
                xregs: [0; 31],
                x86_eip: 0,
                x86_eflags: 0,
                x86_cr: [0; 5],
                reserved1: 0,
                steps: 0,
                last_phase: 0,
                last_mailbox_kind: 0,
                trace_write_index: 0,
                breadcrumb: [0; 3],
            }
        }
    }

    impl Default for PanicBlockV1 {
        fn default() -> Self {
            Self::new()
        }
    }
}

pub const MAILBOX_ABI_VERSION_OFFSET: usize = 0;
pub const MAILBOX_HEADER_SIZE_OFFSET: usize = 2;
pub const MAILBOX_REQ_SEQ_OFFSET: usize = 4;
pub const MAILBOX_RESP_SEQ_OFFSET: usize = 8;
pub const MAILBOX_COMMAND_SEQ_OFFSET: usize = 12;
pub const MAILBOX_REQUEST_OFFSET: usize = 16;
pub const MAILBOX_RESPONSE_OFFSET: usize = 64;
pub const MAILBOX_COMMAND_OFFSET: usize = 112;

pub const REQUEST_KIND_OFFSET: usize = 0;
pub const REQUEST_SIZE_OFFSET: usize = 4;
pub const REQUEST_SEQ_OFFSET: usize = 8;
pub const REQUEST_FLAGS_OFFSET: usize = 12;
pub const REQUEST_A_OFFSET: usize = 16;
pub const REQUEST_B_OFFSET: usize = 24;
pub const REQUEST_C_OFFSET: usize = 32;
pub const REQUEST_D_OFFSET: usize = 40;

pub const RESPONSE_STATUS_OFFSET: usize = 0;
pub const RESPONSE_SIZE_OFFSET: usize = 4;
pub const RESPONSE_SEQ_OFFSET: usize = 8;
pub const RESPONSE_RESERVED_OFFSET: usize = 12;
pub const RESPONSE_VALUE_OFFSET: usize = 16;
pub const RESPONSE_A_OFFSET: usize = 24;
pub const RESPONSE_B_OFFSET: usize = 32;
pub const RESPONSE_C_OFFSET: usize = 40;

pub const HOST_COMMAND_KIND_OFFSET: usize = 0;
pub const HOST_COMMAND_SIZE_OFFSET: usize = 4;
pub const HOST_COMMAND_SEQ_OFFSET: usize = 8;
pub const HOST_COMMAND_FLAGS_OFFSET: usize = 12;
pub const HOST_COMMAND_A_OFFSET: usize = 16;
pub const HOST_COMMAND_B_OFFSET: usize = 24;
pub const HOST_COMMAND_C_OFFSET: usize = 32;
pub const HOST_COMMAND_D_OFFSET: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbiError {
    BadMagic,
    BadVersion,
    BadSize,
    BadRegionKind,
    RegionFull,
    RegionEmpty,
    RegionOverflow,
    RegionUnordered,
    MixedRegion,
    Unaligned,
    Overlap,
    IpaTooSmall,
    BadDoorbell,
    BadOffset,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionKindV1 {
    Hole = 0,
    Ram = 1,
    Rom = 2,
    Mmio = 3,
    FbDirtyTracked = 4,
}

impl RegionKindV1 {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Hole),
            1 => Some(Self::Ram),
            2 => Some(Self::Rom),
            3 => Some(Self::Mmio),
            4 => Some(Self::FbDirtyTracked),
            _ => None,
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionPolicyV1 {
    Default = 0,
    ReadAsFf = 1,
    IgnoreWrites = 2,
    Trap = 3,
}

impl RegionPolicyV1 {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Default),
            1 => Some(Self::ReadAsFf),
            2 => Some(Self::IgnoreWrites),
            3 => Some(Self::Trap),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionClassV1 {
    Hole,
    Ram,
    Rom,
    Mmio,
    FbDirtyTracked,
    Mixed,
}

impl RegionClassV1 {
    pub const fn from_kind(kind: RegionKindV1) -> Self {
        match kind {
            RegionKindV1::Hole => Self::Hole,
            RegionKindV1::Ram => Self::Ram,
            RegionKindV1::Rom => Self::Rom,
            RegionKindV1::Mmio => Self::Mmio,
            RegionKindV1::FbDirtyTracked => Self::FbDirtyTracked,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionEntryV1 {
    pub base: u64,
    pub len: u64,
    pub kind: u32,
    pub policy: u32,
}

impl RegionEntryV1 {
    pub const EMPTY: Self = Self {
        base: 0,
        len: 0,
        kind: RegionKindV1::Hole as u32,
        policy: RegionPolicyV1::Default as u32,
    };

    pub const fn new(base: u64, len: u64, kind: RegionKindV1, policy: RegionPolicyV1) -> Self {
        Self {
            base,
            len,
            kind: kind as u32,
            policy: policy as u32,
        }
    }

    pub const fn ram(base: u64, len: u64) -> Self {
        Self::new(base, len, RegionKindV1::Ram, RegionPolicyV1::Default)
    }

    pub const fn mmio(base: u64, len: u64) -> Self {
        Self::new(base, len, RegionKindV1::Mmio, RegionPolicyV1::Trap)
    }

    pub const fn hole(base: u64, len: u64) -> Self {
        Self::new(base, len, RegionKindV1::Hole, RegionPolicyV1::Default)
    }

    pub const fn end_exclusive(self) -> Option<u64> {
        self.base.checked_add(self.len)
    }

    pub const fn kind(self) -> Result<RegionKindV1, AbiError> {
        match RegionKindV1::from_raw(self.kind) {
            Some(kind) => Ok(kind),
            None => Err(AbiError::BadRegionKind),
        }
    }

    pub const fn contains_range(self, base: u64, len: u64) -> bool {
        if len == 0 || self.len == 0 {
            return false;
        }
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        let Some(region_end) = self.end_exclusive() else {
            return false;
        };
        base >= self.base && end <= region_end
    }

    pub const fn intersects_range(self, base: u64, len: u64) -> bool {
        if len == 0 || self.len == 0 {
            return false;
        }
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        let Some(region_end) = self.end_exclusive() else {
            return false;
        };
        base < region_end && end > self.base
    }

    pub const fn validate(self) -> Result<(), AbiError> {
        if self.len == 0 {
            return Err(AbiError::RegionEmpty);
        }
        if self.end_exclusive().is_none() {
            return Err(AbiError::RegionOverflow);
        }
        if RegionKindV1::from_raw(self.kind).is_none() {
            return Err(AbiError::BadRegionKind);
        }
        if RegionPolicyV1::from_raw(self.policy).is_none() {
            return Err(AbiError::BadRegionKind);
        }
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionTableV1 {
    pub count: u32,
    pub reserved: u32,
    pub entries: [RegionEntryV1; MAX_REGION_ENTRIES],
}

impl RegionTableV1 {
    pub const fn new() -> Self {
        Self {
            count: 0,
            reserved: 0,
            entries: [RegionEntryV1::EMPTY; MAX_REGION_ENTRIES],
        }
    }

    pub fn push(&mut self, entry: RegionEntryV1) -> Result<(), AbiError> {
        entry.validate()?;
        if self.count as usize == MAX_REGION_ENTRIES {
            return Err(AbiError::RegionFull);
        }
        if self.count != 0 {
            let prev = self.entries[self.count as usize - 1];
            let prev_end = prev.end_exclusive().ok_or(AbiError::RegionOverflow)?;
            if entry.base < prev_end {
                return Err(AbiError::RegionUnordered);
            }
        }
        self.entries[self.count as usize] = entry;
        self.count += 1;
        Ok(())
    }

    pub fn entry(&self, index: usize) -> Option<RegionEntryV1> {
        if index < self.count as usize {
            Some(self.entries[index])
        } else {
            None
        }
    }

    pub fn classify(&self, base: u64, len: u64) -> Result<RegionClassV1, AbiError> {
        if len == 0 || base.checked_add(len).is_none() {
            return Err(AbiError::RegionEmpty);
        }
        let mut found = RegionClassV1::Hole;
        let mut covered = false;
        let mut index = 0;
        while index < self.count as usize {
            let entry = self.entries[index];
            if entry.intersects_range(base, len) {
                let class = RegionClassV1::from_kind(entry.kind()?);
                if !entry.contains_range(base, len) {
                    return Ok(RegionClassV1::Mixed);
                }
                if covered && found != class {
                    return Ok(RegionClassV1::Mixed);
                }
                found = class;
                covered = true;
            }
            index += 1;
        }
        Ok(found)
    }

    pub fn validate(&self) -> Result<(), AbiError> {
        if self.count as usize > MAX_REGION_ENTRIES {
            return Err(AbiError::RegionFull);
        }
        let mut previous_end = 0;
        let mut index = 0;
        while index < self.count as usize {
            let entry = self.entries[index];
            entry.validate()?;
            if index != 0 && entry.base < previous_end {
                return Err(AbiError::RegionUnordered);
            }
            previous_end = entry.end_exclusive().ok_or(AbiError::RegionOverflow)?;
            index += 1;
        }
        Ok(())
    }
}

impl Default for RegionTableV1 {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpaLayoutV1 {
    pub ram_base: u64,
    pub ram_size: u64,
    pub monitor_base: u64,
    pub monitor_size: u64,
    pub shared_base: u64,
    pub shared_size: u64,
    pub trace_base: u64,
    pub trace_size: u64,
    pub doorbell_ipa: u64,
    pub reserved: u64,
}

impl IpaLayoutV1 {
    pub const fn new(ram_size: u64, monitor_size: u64, shared_size: u64, trace_size: u64) -> Self {
        Self {
            ram_base: X86_RAM_IPA_BASE,
            ram_size,
            monitor_base: MONITOR_IPA_BASE,
            monitor_size,
            shared_base: SHARED_IPA_BASE,
            shared_size,
            trace_base: TRACE_IPA_BASE,
            trace_size,
            doorbell_ipa: DOORBELL_IPA,
            reserved: 0,
        }
    }

    pub fn validate(self, ipa_bits: u8) -> Result<(), AbiError> {
        if ipa_bits < MIN_ARM_IPA_BITS {
            return Err(AbiError::IpaTooSmall);
        }
        if !is_page_aligned(self.ram_base)
            || !is_page_aligned(self.ram_size)
            || !is_page_aligned(self.monitor_base)
            || !is_page_aligned(self.monitor_size)
            || !is_page_aligned(self.shared_base)
            || !is_page_aligned(self.shared_size)
            || !is_page_aligned(self.trace_base)
            || !is_page_aligned(self.trace_size)
            || !is_page_aligned(self.doorbell_ipa)
        {
            return Err(AbiError::Unaligned);
        }
        if self.ram_base != X86_RAM_IPA_BASE {
            return Err(AbiError::BadOffset);
        }
        if self.doorbell_ipa == self.monitor_base || self.doorbell_ipa == self.shared_base {
            return Err(AbiError::BadDoorbell);
        }
        let max_ipa = 1_u64.checked_shl(ipa_bits as u32).unwrap_or(0);
        if max_ipa == 0 || self.required_end().ok_or(AbiError::RegionOverflow)? > max_ipa {
            return Err(AbiError::IpaTooSmall);
        }
        if ranges_overlap(
            self.ram_base,
            self.ram_size,
            self.monitor_base,
            self.monitor_size,
        )? || ranges_overlap(
            self.ram_base,
            self.ram_size,
            self.shared_base,
            self.shared_size,
        )? || ranges_overlap(
            self.ram_base,
            self.ram_size,
            self.trace_base,
            self.trace_size,
        )? || ranges_overlap(
            self.monitor_base,
            self.monitor_size,
            self.shared_base,
            self.shared_size,
        )? || ranges_overlap(
            self.monitor_base,
            self.monitor_size,
            self.trace_base,
            self.trace_size,
        )? || ranges_overlap(
            self.shared_base,
            self.shared_size,
            self.trace_base,
            self.trace_size,
        )? {
            return Err(AbiError::Overlap);
        }
        Ok(())
    }

    pub fn required_end(self) -> Option<u64> {
        let ram_end = self.ram_base.checked_add(self.ram_size)?;
        let monitor_end = self.monitor_base.checked_add(self.monitor_size)?;
        let shared_end = self.shared_base.checked_add(self.shared_size)?;
        let trace_end = self.trace_base.checked_add(self.trace_size)?;
        let doorbell_end = self.doorbell_ipa.checked_add(PAGE_SIZE_4K)?;
        Some(max4(monitor_end, shared_end, trace_end, doorbell_end).max(ram_end))
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowWindowV1 {
    Supervisor = 0,
    User = 1,
}

pub const fn shadow_window_base(window: ShadowWindowV1) -> u64 {
    match window {
        ShadowWindowV1::Supervisor => SHADOW_SUP_BASE,
        ShadowWindowV1::User => SHADOW_USR_BASE,
    }
}

pub const fn shadow_window_va(window: ShadowWindowV1, linear: u32) -> u64 {
    shadow_window_base(window) | linear as u64
}

pub const fn direct_map_va(phys: u64) -> u64 {
    TTBR1_DIRECT_MAP_BASE + phys
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentStateV1 {
    pub selector: u16,
    pub attr: u16,
    pub limit: u32,
    pub base: u32,
}

impl SegmentStateV1 {
    pub const ZERO: Self = Self {
        selector: 0,
        attr: 0,
        limit: 0,
        base: 0,
    };

    pub const fn flat(selector: u16, attr: u16) -> Self {
        Self {
            selector,
            attr,
            limit: u32::MAX,
            base: 0,
        }
    }
}

impl Default for SegmentStateV1 {
    fn default() -> Self {
        Self::ZERO
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorTableV1 {
    pub base: u64,
    pub limit: u16,
    pub reserved: [u16; 3],
}

impl DescriptorTableV1 {
    pub const ZERO: Self = Self {
        base: 0,
        limit: 0,
        reserved: [0; 3],
    };
}

impl Default for DescriptorTableV1 {
    fn default() -> Self {
        Self::ZERO
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuMsrStateV1 {
    pub apic_base: u64,
    pub test_ctl: u32,
    pub reserved0: u32,
    pub bios_update_trigger: u64,
    pub bbl_cr_data: [u64; 4],
    pub bbl_cr_addr: u64,
    pub bbl_cr_decc: u64,
    pub bbl_cr_ctl: u64,
    pub bbl_cr_trig: u64,
    pub bbl_cr_busy: u64,
    pub bbl_cr_ctl3: u64,
    pub misc_enable: u64,
    pub perfctr: [u64; 2],
    pub evntsel: [u64; 2],
    pub mtrr_phys: [u64; 16],
    pub mtrr_fix64k_00000: u64,
    pub mtrr_fix16k: [u64; 2],
    pub mtrr_fix4k: [u64; 8],
    pub mtrr_def_type: u64,
    pub pat: u64,
    pub mcg_ctl: u64,
    pub mca_ctl: [u64; 5],
    pub sysenter_cs: u64,
    pub sysenter_esp: u64,
    pub sysenter_eip: u64,
}

impl CpuMsrStateV1 {
    pub const RESET: Self = Self {
        apic_base: 0,
        test_ctl: 0,
        reserved0: 0,
        bios_update_trigger: 0,
        bbl_cr_data: [0; 4],
        bbl_cr_addr: 0,
        bbl_cr_decc: 0,
        bbl_cr_ctl: 0,
        bbl_cr_trig: 0,
        bbl_cr_busy: 0,
        bbl_cr_ctl3: 0,
        misc_enable: 0,
        perfctr: [0; 2],
        evntsel: [0; 2],
        mtrr_phys: [0; 16],
        mtrr_fix64k_00000: 0,
        mtrr_fix16k: [0; 2],
        mtrr_fix4k: [0; 8],
        mtrr_def_type: 0,
        pat: 0x0007_0406_0007_0406,
        mcg_ctl: 0,
        mca_ctl: [0; 5],
        sysenter_cs: 0,
        sysenter_esp: 0,
        sysenter_eip: 0,
    };
}

impl Default for CpuMsrStateV1 {
    fn default() -> Self {
        Self::RESET
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FpuStateV1 {
    pub control: u16,
    pub status: u16,
    pub top: u8,
    pub mmx_active: u8,
    pub reserved0: [u8; 2],
    pub mxcsr: u32,
    pub reserved1: u32,
    pub st_bits: [u64; 8],
    pub mm: [u64; 8],
}

impl FpuStateV1 {
    pub const RESET: Self = Self {
        control: 0x037f,
        status: 0,
        top: 0,
        mmx_active: 0,
        reserved0: [0; 2],
        mxcsr: 0x0000_1f80,
        reserved1: 0,
        st_bits: [0; 8],
        mm: [0; 8],
    };
}

impl Default for FpuStateV1 {
    fn default() -> Self {
        Self::RESET
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuStateV1 {
    pub gpr: [u32; 8],
    pub eip: u32,
    pub eflags: u32,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub cpl: u8,
    pub halted: u8,
    pub interrupt_shadow: u8,
    pub reserved0: u8,
    pub seg: [SegmentStateV1; 6],
    pub gdtr: DescriptorTableV1,
    pub idtr: DescriptorTableV1,
    pub ldtr: SegmentStateV1,
    pub tr: SegmentStateV1,
    pub tsc: u64,
    pub instr_count: u64,
    pub fxsave: [u8; 512],
    pub pae_pdptrs: [u64; 4],
    pub dr: [u32; 8],
    pub msr: CpuMsrStateV1,
    pub fpu: FpuStateV1,
}

impl CpuStateV1 {
    pub const ZERO: Self = Self {
        gpr: [0; 8],
        eip: 0,
        eflags: 2,
        cr0: 0,
        cr2: 0,
        cr3: 0,
        cr4: 0,
        cpl: 0,
        halted: 0,
        interrupt_shadow: 0,
        reserved0: 0,
        seg: [SegmentStateV1::ZERO; 6],
        gdtr: DescriptorTableV1::ZERO,
        idtr: DescriptorTableV1::ZERO,
        ldtr: SegmentStateV1::ZERO,
        tr: SegmentStateV1::ZERO,
        tsc: 0,
        instr_count: 0,
        fxsave: [0; 512],
        pae_pdptrs: [0; 4],
        dr: [0; 8],
        msr: CpuMsrStateV1::RESET,
        fpu: FpuStateV1::RESET,
    };

    pub const fn protected_flat_linux_entry(entry: u32, boot_params: u32) -> Self {
        let mut state = Self::ZERO;
        state.gpr[4] = 0x0009_0000;
        state.gpr[6] = boot_params;
        state.eip = entry;
        state.eflags = 2;
        state.cr0 = 1;
        state.seg[0] = SegmentStateV1::flat(0x18, 0xc092);
        state.seg[1] = SegmentStateV1::flat(0x10, 0xc09a);
        state.seg[2] = SegmentStateV1::flat(0x18, 0xc092);
        state.seg[3] = SegmentStateV1::flat(0x18, 0xc092);
        state.seg[4] = SegmentStateV1::flat(0x18, 0xc092);
        state.seg[5] = SegmentStateV1::flat(0x18, 0xc092);
        state
    }
}

impl Default for CpuStateV1 {
    fn default() -> Self {
        Self::ZERO
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorRequestKindV1 {
    None = 0,
    Hello = 1,
    PioIn = 2,
    PioOut = 3,
    MmioRead = 4,
    MmioWrite = 5,
    TimerPoll = 6,
    Inta = 7,
    Halt = 8,
    SerialFlush = 9,
    TraceFull = 10,
    StopAck = 11,
    Panic = 12,
    HardMmuStats = 13,
    ReloadBoot = 14,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostCommandKindV1 {
    None = 0,
    Stop = 1,
    Snapshot = 2,
    IrqLine = 3,
    DmaWrite = 4,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseStatusV1 {
    Ok = 0,
    Unsupported = 1,
    Fault = 2,
    BadPacket = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorRequestV1 {
    pub kind: u32,
    pub size: u32,
    pub seq: u32,
    pub flags: u32,
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

impl MonitorRequestV1 {
    pub const NONE: Self = Self {
        kind: MonitorRequestKindV1::None as u32,
        size: core::mem::size_of::<Self>() as u32,
        seq: 0,
        flags: 0,
        a: 0,
        b: 0,
        c: 0,
        d: 0,
    };

    pub const fn hello(seq: u32, feature_bits: u64, build_id_hash: u64) -> Self {
        Self {
            kind: MonitorRequestKindV1::Hello as u32,
            size: core::mem::size_of::<Self>() as u32,
            seq,
            flags: 0,
            a: ABI_VERSION as u64,
            b: feature_bits,
            c: build_id_hash,
            d: 0,
        }
    }

    pub const fn pio_in(seq: u32, port: u16, width: u8, vtime: u64) -> Self {
        Self {
            kind: MonitorRequestKindV1::PioIn as u32,
            size: core::mem::size_of::<Self>() as u32,
            seq,
            flags: 0,
            a: port as u64,
            b: width as u64,
            c: vtime,
            d: 0,
        }
    }

    pub const fn pio_out(seq: u32, port: u16, width: u8, value: u32, vtime: u64) -> Self {
        Self {
            kind: MonitorRequestKindV1::PioOut as u32,
            size: core::mem::size_of::<Self>() as u32,
            seq,
            flags: 0,
            a: port as u64,
            b: width as u64,
            c: value as u64,
            d: vtime,
        }
    }

    pub const fn timer_poll(seq: u32, vtime: u64, halted: bool) -> Self {
        Self {
            kind: MonitorRequestKindV1::TimerPoll as u32,
            size: core::mem::size_of::<Self>() as u32,
            seq,
            flags: 0,
            a: vtime,
            b: halted as u64,
            c: 0,
            d: 0,
        }
    }

    pub const fn mmio(
        seq: u32,
        kind: MonitorRequestKindV1,
        gpa: u64,
        width: u8,
        value: u64,
    ) -> Self {
        Self {
            kind: kind as u32,
            size: core::mem::size_of::<Self>() as u32,
            seq,
            flags: 0,
            a: gpa,
            b: width as u64,
            c: value,
            d: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostResponseV1 {
    pub status: u32,
    pub size: u32,
    pub seq: u32,
    pub reserved: u32,
    pub value: u64,
    pub a: u64,
    pub b: u64,
    pub c: u64,
}

impl HostResponseV1 {
    pub const fn status(status: ResponseStatusV1, seq: u32, value: u64) -> Self {
        Self {
            status: status as u32,
            size: core::mem::size_of::<Self>() as u32,
            seq,
            reserved: 0,
            value,
            a: 0,
            b: 0,
            c: 0,
        }
    }

    pub const fn ok(seq: u32, value: u64) -> Self {
        Self::status(ResponseStatusV1::Ok, seq, value)
    }

    pub const fn unsupported(seq: u32) -> Self {
        Self::status(ResponseStatusV1::Unsupported, seq, 0)
    }

    pub const fn fault(seq: u32, value: u64) -> Self {
        Self::status(ResponseStatusV1::Fault, seq, value)
    }

    pub const fn bad_packet(seq: u32) -> Self {
        Self::status(ResponseStatusV1::BadPacket, seq, 0)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostCommandV1 {
    pub kind: u32,
    pub size: u32,
    pub seq: u32,
    pub flags: u32,
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

impl HostCommandV1 {
    pub const NONE: Self = Self {
        kind: HostCommandKindV1::None as u32,
        size: core::mem::size_of::<Self>() as u32,
        seq: 0,
        flags: 0,
        a: 0,
        b: 0,
        c: 0,
        d: 0,
    };
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MailboxV1 {
    pub abi_version: u16,
    pub header_size: u16,
    pub req_seq: u32,
    pub resp_seq: u32,
    pub command_seq: u32,
    pub request: MonitorRequestV1,
    pub response: HostResponseV1,
    pub command: HostCommandV1,
}

impl MailboxV1 {
    pub const fn new() -> Self {
        Self {
            abi_version: ABI_VERSION,
            header_size: core::mem::size_of::<Self>() as u16,
            req_seq: 0,
            resp_seq: 0,
            command_seq: 0,
            request: MonitorRequestV1::NONE,
            response: HostResponseV1::ok(0, 0),
            command: HostCommandV1::NONE,
        }
    }

    pub const fn validate_header(self) -> Result<(), AbiError> {
        if self.abi_version != ABI_VERSION {
            return Err(AbiError::BadVersion);
        }
        if self.header_size as usize != core::mem::size_of::<Self>() {
            return Err(AbiError::BadSize);
        }
        Ok(())
    }
}

impl Default for MailboxV1 {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceModeV1 {
    Off = 0,
    Architectural = 1,
    ArchitecturalAndImplementation = 2,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootBlockV1 {
    pub magic: u32,
    pub abi_version: u16,
    pub size: u16,
    pub feature_bits: u64,
    pub layout: IpaLayoutV1,
    pub regions: RegionTableV1,
    pub initial_cpu_state_offset: u64,
    pub mailbox_offset: u64,
    pub trace_offset: u64,
    pub timer_poll_interval: u64,
    pub step_limit: u64,
    pub trace_mode: u32,
    pub reserved0: u32,
    pub build_id: [u8; BUILD_ID_BYTES],
    pub reserved1: [u64; 8],
}

pub const BOOT_BLOCK_DEBUG_STATE_OFFSET_RESERVED_INDEX: usize = 0;
pub const BOOT_BLOCK_PANIC_BLOCK_OFFSET_RESERVED_INDEX: usize = 1;

impl BootBlockV1 {
    pub const fn new(layout: IpaLayoutV1) -> Self {
        Self {
            magic: ABI_MAGIC,
            abi_version: ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            feature_bits: 0,
            layout,
            regions: RegionTableV1::new(),
            initial_cpu_state_offset: 0,
            mailbox_offset: 0,
            trace_offset: 0,
            timer_poll_interval: 0,
            step_limit: 0,
            trace_mode: TraceModeV1::Off as u32,
            reserved0: 0,
            build_id: [0; BUILD_ID_BYTES],
            reserved1: [0; 8],
        }
    }

    pub const fn debug_state_offset(&self) -> u64 {
        self.reserved1[BOOT_BLOCK_DEBUG_STATE_OFFSET_RESERVED_INDEX]
    }

    pub const fn panic_block_offset(&self) -> u64 {
        self.reserved1[BOOT_BLOCK_PANIC_BLOCK_OFFSET_RESERVED_INDEX]
    }

    pub fn set_debug_state_offset(&mut self, offset: u64) {
        self.reserved1[BOOT_BLOCK_DEBUG_STATE_OFFSET_RESERVED_INDEX] = offset;
    }

    pub fn set_panic_block_offset(&mut self, offset: u64) {
        self.reserved1[BOOT_BLOCK_PANIC_BLOCK_OFFSET_RESERVED_INDEX] = offset;
    }

    pub fn validate(&self, ipa_bits: u8) -> Result<(), AbiError> {
        if self.magic != ABI_MAGIC {
            return Err(AbiError::BadMagic);
        }
        if self.abi_version != ABI_VERSION {
            return Err(AbiError::BadVersion);
        }
        if self.size as usize != core::mem::size_of::<Self>() {
            return Err(AbiError::BadSize);
        }
        self.layout.validate(ipa_bits)?;
        self.regions.validate()?;
        if !is_page_aligned(self.initial_cpu_state_offset)
            || !is_page_aligned(self.mailbox_offset)
            || !is_page_aligned(self.trace_offset)
            || !is_page_aligned(self.debug_state_offset())
            || !is_page_aligned(self.panic_block_offset())
        {
            return Err(AbiError::Unaligned);
        }
        Ok(())
    }
}

pub const BOOT_BLOCK_INITIAL_CPU_STATE_OFFSET_FIELD_OFFSET: usize =
    16 + core::mem::size_of::<IpaLayoutV1>() + core::mem::size_of::<RegionTableV1>();
pub const BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET: usize =
    BOOT_BLOCK_INITIAL_CPU_STATE_OFFSET_FIELD_OFFSET + core::mem::size_of::<u64>();
pub const BOOT_BLOCK_TRACE_OFFSET_FIELD_OFFSET: usize =
    BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET + core::mem::size_of::<u64>();

pub const fn is_page_aligned(value: u64) -> bool {
    (value & (PAGE_SIZE_4K - 1)) == 0
}

pub fn align_up_4k(value: u64) -> Option<u64> {
    let add = value.checked_add(PAGE_SIZE_4K - 1)?;
    Some(add & PAGE_MASK_4K)
}

pub fn ranges_overlap(a_base: u64, a_len: u64, b_base: u64, b_len: u64) -> Result<bool, AbiError> {
    let a_end = a_base.checked_add(a_len).ok_or(AbiError::RegionOverflow)?;
    let b_end = b_base.checked_add(b_len).ok_or(AbiError::RegionOverflow)?;
    Ok(a_base < b_end && b_base < a_end)
}

pub const fn max4(a: u64, b: u64, c: u64, d: u64) -> u64 {
    let ab = if a > b { a } else { b };
    let cd = if c > d { c } else { d };
    if ab > cd { ab } else { cd }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_window_va_encodes_view_without_low_guard_aliases() {
        assert_eq!(
            shadow_window_va(ShadowWindowV1::Supervisor, 0x1234_5678),
            0x10_1234_5678
        );
        assert_eq!(
            shadow_window_va(ShadowWindowV1::User, 0x1234_5678),
            0x20_1234_5678
        );
        assert!(shadow_window_va(ShadowWindowV1::Supervisor, 0) >= TTBR0_GUARD_END);
        assert_eq!(
            shadow_window_va(ShadowWindowV1::User, u32::MAX) + 1,
            0x21_0000_0000
        );
    }

    #[test]
    fn region_table_classifies_ram_mmio_holes_and_mixed_ranges() {
        let mut table = RegionTableV1::new();
        table.push(RegionEntryV1::ram(0, 0x1000)).unwrap();
        table.push(RegionEntryV1::mmio(0x2000, 0x1000)).unwrap();

        assert_eq!(table.classify(0x100, 4), Ok(RegionClassV1::Ram));
        assert_eq!(table.classify(0x1800, 0x100), Ok(RegionClassV1::Hole));
        assert_eq!(table.classify(0x2004, 4), Ok(RegionClassV1::Mmio));
        assert_eq!(table.classify(0x0ff0, 0x20), Ok(RegionClassV1::Mixed));
        assert_eq!(table.classify(0x1ff0, 0x20), Ok(RegionClassV1::Mixed));
    }

    #[test]
    fn region_table_rejects_bad_entries_and_overlap() {
        let mut table = RegionTableV1::new();
        assert_eq!(
            table.push(RegionEntryV1::ram(0, 0)),
            Err(AbiError::RegionEmpty)
        );
        table.push(RegionEntryV1::ram(0, 0x1000)).unwrap();
        assert_eq!(
            table.push(RegionEntryV1::ram(0x800, 0x1000)),
            Err(AbiError::RegionUnordered)
        );
    }

    #[test]
    fn ipa_layout_keeps_internal_ranges_above_4g_and_out_of_ram() {
        let layout = IpaLayoutV1::new(
            128 * 1024 * 1024,
            8 * 1024 * 1024,
            2 * 1024 * 1024,
            4 * 1024 * 1024,
        );

        assert_eq!(layout.validate(34), Ok(()));
        assert!(layout.monitor_base >= 0x1_0000_0000);
        assert!(layout.shared_base > layout.monitor_base);
        assert!(layout.trace_base > layout.shared_base);
        assert_eq!(layout.doorbell_ipa, DOORBELL_IPA);
    }

    #[test]
    fn ipa_layout_rejects_ram_overlap_and_small_ipa() {
        let mut layout =
            IpaLayoutV1::new(128 * 1024 * 1024, PAGE_SIZE_4K, PAGE_SIZE_4K, PAGE_SIZE_4K);
        layout.monitor_base = X86_RAM_IPA_BASE;
        assert_eq!(layout.validate(34), Err(AbiError::Overlap));
        layout.monitor_base = MONITOR_IPA_BASE;
        layout.ram_size = (1 << 34) - X86_RAM_IPA_BASE + PAGE_SIZE_4K;
        assert_eq!(layout.validate(34), Err(AbiError::IpaTooSmall));

        layout.ram_size = 128 * 1024 * 1024;
        assert_eq!(layout.validate(33), Err(AbiError::IpaTooSmall));
    }

    #[test]
    fn boot_block_validates_header_layout_and_region_table() {
        let layout = IpaLayoutV1::new(
            128 * 1024 * 1024,
            8 * 1024 * 1024,
            2 * 1024 * 1024,
            4 * 1024 * 1024,
        );
        let mut boot = BootBlockV1::new(layout);
        boot.initial_cpu_state_offset = PAGE_SIZE_4K;
        boot.mailbox_offset = 2 * PAGE_SIZE_4K;
        boot.trace_offset = 3 * PAGE_SIZE_4K;
        boot.set_debug_state_offset(4 * PAGE_SIZE_4K);
        boot.set_panic_block_offset(5 * PAGE_SIZE_4K);
        boot.regions
            .push(RegionEntryV1::ram(0, 128 * 1024 * 1024))
            .unwrap();

        assert_eq!(boot.validate(34), Ok(()));
        assert_eq!(boot.debug_state_offset(), 4 * PAGE_SIZE_4K);
        assert_eq!(boot.panic_block_offset(), 5 * PAGE_SIZE_4K);
        assert_eq!(BOOT_BLOCK_INITIAL_CPU_STATE_OFFSET_FIELD_OFFSET, 488);
        assert_eq!(BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET, 496);
        assert_eq!(BOOT_BLOCK_TRACE_OFFSET_FIELD_OFFSET, 504);
        boot.magic = 0;
        assert_eq!(boot.validate(34), Err(AbiError::BadMagic));
    }

    #[test]
    fn debug_abi_blocks_fit_dedicated_shared_pages() {
        assert_eq!(debug::DEBUG_MAGIC, 0x4436_3852);
        assert_eq!(debug::PANIC_MAGIC, 0x5036_3852);
        assert_eq!(debug::DEBUG_STATE_BLOCK_OFFSET, PAGE_SIZE_4K * 3);
        assert_eq!(debug::PANIC_BLOCK_OFFSET, PAGE_SIZE_4K * 4);
        assert!(core::mem::size_of::<debug::DebugStateBlockV1>() <= PAGE_SIZE_4K as usize);
        assert!(core::mem::size_of::<debug::PanicBlockV1>() <= PAGE_SIZE_4K as usize);
        assert_eq!(core::mem::align_of::<debug::DebugStateBlockV1>(), 8);
        assert_eq!(core::mem::align_of::<debug::PanicBlockV1>(), 8);

        let state = debug::DebugStateBlockV1::new();
        assert_eq!(state.magic, debug::DEBUG_MAGIC);
        assert_eq!(state.version, debug::DEBUG_ABI_VERSION);
        assert_eq!(
            state.size as usize,
            core::mem::size_of::<debug::DebugStateBlockV1>()
        );
        assert_eq!(state.fields_valid, debug::DEBUG_STATE_FIELDS_ALL);

        let panic = debug::PanicBlockV1::new();
        assert_eq!(panic.magic, debug::PANIC_MAGIC);
        assert_eq!(
            panic.size as usize,
            core::mem::size_of::<debug::PanicBlockV1>()
        );
    }

    #[test]
    fn mailbox_packets_are_fixed_size_and_sequence_tagged() {
        assert_eq!(core::mem::size_of::<MonitorRequestV1>(), 48);
        assert_eq!(core::mem::size_of::<HostResponseV1>(), 48);
        assert_eq!(core::mem::align_of::<MailboxV1>(), 16);
        assert_eq!(core::mem::size_of::<MailboxV1>(), 160);
        assert_eq!(MAILBOX_ABI_VERSION_OFFSET, 0);
        assert_eq!(MAILBOX_HEADER_SIZE_OFFSET, 2);
        assert_eq!(MAILBOX_REQ_SEQ_OFFSET, 4);
        assert_eq!(MAILBOX_RESP_SEQ_OFFSET, 8);
        assert_eq!(MAILBOX_COMMAND_SEQ_OFFSET, 12);
        assert_eq!(MAILBOX_REQUEST_OFFSET, 16);
        assert_eq!(MAILBOX_RESPONSE_OFFSET, 64);
        assert_eq!(MAILBOX_COMMAND_OFFSET, 112);
        assert_eq!(
            MAILBOX_REQUEST_OFFSET + REQUEST_D_OFFSET + 8,
            MAILBOX_RESPONSE_OFFSET
        );
        assert_eq!(
            MAILBOX_RESPONSE_OFFSET + RESPONSE_C_OFFSET + 8,
            MAILBOX_COMMAND_OFFSET
        );
        assert_eq!(
            MAILBOX_COMMAND_OFFSET + HOST_COMMAND_D_OFFSET + 8,
            core::mem::size_of::<MailboxV1>()
        );

        let request = MonitorRequestV1::pio_out(9, 0x3f8, 1, b'X' as u32, 123);
        assert_eq!(request.kind, MonitorRequestKindV1::PioOut as u32);
        assert_eq!(request.seq, 9);
        assert_eq!(request.a, 0x3f8);
        assert_eq!(request.b, 1);
        assert_eq!(request.c, b'X' as u64);
        assert_eq!(request.d, 123);

        let response = HostResponseV1::ok(9, 0x41);
        assert_eq!(response.status, ResponseStatusV1::Ok as u32);
        assert_eq!(response.seq, 9);
        assert_eq!(response.value, 0x41);
        assert_eq!(
            HostResponseV1::unsupported(10).status,
            ResponseStatusV1::Unsupported as u32
        );
        assert_eq!(
            HostResponseV1::bad_packet(11).status,
            ResponseStatusV1::BadPacket as u32
        );
    }

    #[test]
    fn cpu_state_carries_fixed_fxsave_image() {
        assert_eq!(core::mem::size_of::<SegmentStateV1>(), 12);
        assert_eq!(core::mem::size_of::<DescriptorTableV1>(), 16);
        assert_eq!(CpuStateV1::ZERO.eflags, 2);
        assert_eq!(CpuStateV1::ZERO.interrupt_shadow, 0);
        assert_eq!(CpuStateV1::ZERO.fxsave.len(), 512);
        assert_eq!(CpuStateV1::ZERO.msr.pat, 0x0007_0406_0007_0406);
        assert_eq!(CpuStateV1::ZERO.fpu.control, 0x037f);
        assert_eq!(CpuStateV1::ZERO.fpu.mxcsr, 0x0000_1f80);
        assert_eq!(core::mem::align_of::<CpuStateV1>(), 8);
    }

    #[test]
    fn linux_entry_state_matches_direct_protected_boot_contract() {
        let state = CpuStateV1::protected_flat_linux_entry(0x0010_0000, 0x0009_0000);

        assert_eq!(state.eip, 0x0010_0000);
        assert_eq!(state.gpr[4], 0x0009_0000);
        assert_eq!(state.gpr[6], 0x0009_0000);
        assert_eq!(state.cr0, 1);
        assert_eq!(state.eflags, 2);
        assert_eq!(state.seg[1], SegmentStateV1::flat(0x10, 0xc09a));
        assert_eq!(state.seg[3], SegmentStateV1::flat(0x18, 0xc092));
        assert!(state.fxsave.iter().all(|byte| *byte == 0));
        assert_eq!(state.pae_pdptrs, [0; 4]);
        assert_eq!(state.msr.sysenter_cs, 0);
        assert_eq!(state.msr.pat, 0x0007_0406_0007_0406);
    }

    #[test]
    fn alignment_helpers_are_overflow_safe() {
        assert_eq!(align_up_4k(0), Some(0));
        assert_eq!(align_up_4k(1), Some(4096));
        assert_eq!(align_up_4k(4096), Some(4096));
        assert_eq!(align_up_4k(u64::MAX), None);
        assert!(ranges_overlap(0, 10, 9, 1).unwrap());
        assert!(!ranges_overlap(0, 10, 10, 1).unwrap());
    }
}
