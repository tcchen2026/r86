// The bare-metal target conservatively decomposes read_unaligned into bytes,
// because startup runs with the MMU off. These helpers are used only after
// stage-1 setup, on bounded Normal RAM (or the monitor's constant ROM backing).
// Keep startup and all Device/MMIO accesses out of this boundary.
macro_rules! normal_ram_access {
    ($read:ident, $write:ident, $ty:ty, $load:literal, $store:literal) => {
        #[cfg(any(feature = "machine", feature = "monitor"))]
        #[inline(always)]
        pub(crate) unsafe fn $read(ptr: *const u8) -> $ty {
            #[cfg(all(target_arch = "aarch64", target_os = "none"))]
            let value: $ty = {
                let value;
                unsafe { core::arch::asm!($load, ptr = in(reg) ptr, value = out(reg) value, options(readonly, nostack, preserves_flags)) };
                value
            };
            #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
            let value = unsafe { ptr.cast::<$ty>().read_unaligned() };
            <$ty>::from_le(value)
        }
        #[cfg(any(feature = "machine", feature = "monitor"))]
        #[inline(always)]
        pub(crate) unsafe fn $write(ptr: *mut u8, value: $ty) {
            let value = value.to_le();
            #[cfg(all(target_arch = "aarch64", target_os = "none"))]
            unsafe { core::arch::asm!($store, ptr = in(reg) ptr, value = in(reg) value, options(nostack, preserves_flags)) };
            #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
            unsafe { ptr.cast::<$ty>().write_unaligned(value) };
        }
    };
}
normal_ram_access!(
    ram_read_u16,
    ram_write_u16,
    u16,
    "ldrh {value:w}, [{ptr}]",
    "strh {value:w}, [{ptr}]"
);
normal_ram_access!(
    ram_read_u32,
    ram_write_u32,
    u32,
    "ldr {value:w}, [{ptr}]",
    "str {value:w}, [{ptr}]"
);
normal_ram_access!(
    ram_read_u64,
    ram_write_u64,
    u64,
    "ldr {value}, [{ptr}]",
    "str {value}, [{ptr}]"
);

use crate::x86::addr::{LinearAddr, PhysAddr};

pub const PHYS_PAGE_SHIFT: u64 = 12;
pub const PHYS_PAGE_SIZE: u64 = 1 << PHYS_PAGE_SHIFT;
pub const PHYS_PAGE_MASK: u64 = !(PHYS_PAGE_SIZE - 1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemError {
    OutOfRange { addr: PhysAddr },
    Mmio { addr: PhysAddr },
    IoUnhandled { port: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysRegionKind {
    Ram,
    Rom,
    Mmio,
    FbDirtyTracked,
    Hole,
    Mixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysRegionMapError {
    Full,
    Empty,
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysRegion {
    pub base: PhysAddr,
    pub len: u64,
    pub kind: PhysRegionKind,
}

impl PhysRegion {
    pub const EMPTY: Self = Self {
        base: PhysAddr(0),
        len: 0,
        kind: PhysRegionKind::Hole,
    };

    pub const fn new(base: PhysAddr, len: u64, kind: PhysRegionKind) -> Self {
        Self { base, len, kind }
    }

    pub const fn ram(base: PhysAddr, len: u64) -> Self {
        Self::new(base, len, PhysRegionKind::Ram)
    }

    pub const fn rom(base: PhysAddr, len: u64) -> Self {
        Self::new(base, len, PhysRegionKind::Rom)
    }

    pub const fn mmio(base: PhysAddr, len: u64) -> Self {
        Self::new(base, len, PhysRegionKind::Mmio)
    }

    pub const fn fb_dirty_tracked(base: PhysAddr, len: u64) -> Self {
        Self::new(base, len, PhysRegionKind::FbDirtyTracked)
    }

    pub fn contains_range(self, addr: PhysAddr, len: u32) -> bool {
        if len == 0 || self.len == 0 {
            return false;
        }
        let start = addr.0;
        let Some(end) = start.checked_add(u64::from(len) - 1) else {
            return false;
        };
        let Some(region_end) = self.base.0.checked_add(self.len - 1) else {
            return false;
        };
        start >= self.base.0 && end <= region_end
    }

    pub fn intersects_range(self, addr: PhysAddr, len: u32) -> bool {
        if len == 0 || self.len == 0 {
            return false;
        }
        let start = addr.0;
        let Some(end) = start.checked_add(u64::from(len) - 1) else {
            return false;
        };
        let Some(region_end) = self.base.0.checked_add(self.len - 1) else {
            return false;
        };
        start <= region_end && end >= self.base.0
    }
}

#[derive(Clone, Debug)]
pub struct PhysRegionMap<const N: usize> {
    regions: [PhysRegion; N],
    len: usize,
}

impl<const N: usize> PhysRegionMap<N> {
    pub const fn new() -> Self {
        Self {
            regions: [PhysRegion::EMPTY; N],
            len: 0,
        }
    }

    pub fn ram(size: u64) -> Result<Self, PhysRegionMapError> {
        let mut map = Self::new();
        if size != 0 {
            map.push(PhysRegion::ram(PhysAddr(0), size))?;
        }
        Ok(map)
    }

    pub fn push(&mut self, region: PhysRegion) -> Result<(), PhysRegionMapError> {
        if region.len == 0 {
            return Err(PhysRegionMapError::Empty);
        }
        if region.base.0.checked_add(region.len - 1).is_none() {
            return Err(PhysRegionMapError::Overflow);
        }
        if self.len == N {
            return Err(PhysRegionMapError::Full);
        }
        self.regions[self.len] = region;
        self.len += 1;
        Ok(())
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn region(&self, index: usize) -> Option<PhysRegion> {
        if index < self.len {
            Some(self.regions[index])
        } else {
            None
        }
    }

    pub fn classify(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        if len == 0 {
            return PhysRegionKind::Hole;
        }
        let Some(end) = addr.0.checked_add(u64::from(len) - 1) else {
            return PhysRegionKind::Hole;
        };
        let mut found = PhysRegionKind::Hole;
        // push validated nonempty, nonoverflowing regions. Check the external
        // access range once, then compare against those established bounds.
        // Keep overlapping-kind and partially covered accesses distinct: they
        // must not acquire a plain RAM translation.
        for region in &self.regions[..self.len] {
            let region_end = region.base.0 + (region.len - 1);
            if addr.0 > region_end || end < region.base.0 {
                continue;
            }
            if addr.0 < region.base.0 || end > region_end {
                return PhysRegionKind::Mixed;
            }
            if found == PhysRegionKind::Hole {
                found = region.kind;
            } else if found != region.kind {
                return PhysRegionKind::Mixed;
            }
        }
        found
    }

    pub fn is_ram(&self, addr: PhysAddr, len: u32) -> bool {
        self.classify(addr, len) == PhysRegionKind::Ram
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOrigin {
    Cpu,
    PageWalk,
    Device,
    Loader,
    Debug,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysWrite {
    pub origin: WriteOrigin,
    pub addr: PhysAddr,
    pub len: u8,
}

impl PhysWrite {
    pub const fn new(origin: WriteOrigin, addr: PhysAddr, len: u8) -> Self {
        Self { origin, addr, len }
    }

    pub const fn first_frame(self) -> PhysFrame {
        PhysFrame::containing(self.addr)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageTableFrameRole {
    Pdpt,
    Directory,
    Table,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysFrame(pub u64);

impl PhysFrame {
    pub const fn containing(addr: PhysAddr) -> Self {
        Self(addr.0 >> PHYS_PAGE_SHIFT)
    }

    pub const fn base(self) -> PhysAddr {
        PhysAddr(self.0 << PHYS_PAGE_SHIFT)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRoleFlags(u8);

impl FrameRoleFlags {
    pub const NONE: Self = Self(0);
    pub const RAM: Self = Self(1 << 0);
    pub const PDPT: Self = Self(1 << 1);
    pub const DIRECTORY: Self = Self(1 << 2);
    pub const TABLE: Self = Self(1 << 3);

    pub const fn from_page_table_role(role: PageTableFrameRole) -> Self {
        match role {
            PageTableFrameRole::Pdpt => Self::PDPT,
            PageTableFrameRole::Directory => Self::DIRECTORY,
            PageTableFrameRole::Table => Self::TABLE,
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub const fn is_page_table(self) -> bool {
        (self.0 & (Self::PDPT.0 | Self::DIRECTORY.0 | Self::TABLE.0)) != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRead<T> {
    Unsupported,
    Miss,
    Hit(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct BackingToken(pub u64);

impl BackingToken {
    #[inline(always)]
    pub const fn advance(self, len: u32) -> Self {
        Self(self.0.wrapping_add(len as u64))
    }
}

pub trait Bus {
    /// Optional stable RAM backing for an immediately preceding native read hit.
    /// The backing must remain valid through ordinary translation-cache eviction
    /// and follow the same tracked-write invalidation contract as software fills.
    /// Return the original x86 leaf size, even if native tables split that leaf,
    /// so INVLPG retires every large-page fragment without expiring small neighbors.
    fn native_read_backing(
        &mut self,
        _linear: LinearAddr,
        _priv_: crate::x86::mmu::AccessPriv,
    ) -> Option<(BackingToken, crate::x86::mmu::TranslationPageSize)> {
        None
    }

    fn current_instruction(&self) -> u64 {
        0
    }

    fn begin_instruction(&mut self, instruction_count: u64, tsc: u64) {
        let _ = (instruction_count, tsc);
    }

    /// Linear address of the instruction before fetching any prefix bytes.
    /// Diagnostic buses use it to attribute unclaimed accesses. Native RAM
    /// paths need no update; exits must establish it before device dispatch.
    fn instruction_address(&mut self, _linear: u32) {}

    fn begin_io_instruction(&mut self, instruction_count: u64) {
        let _ = instruction_count;
    }

    /// The machine may provide a time-based TSC without changing instruction
    /// accounting. Simple semantic-test buses retain the CPU's counter.
    fn read_timestamp_counter(&mut self, instruction_tsc: u64) -> u64 {
        instruction_tsc
    }
    fn write_timestamp_counter(&mut self, _value: u64) {}

    fn instruction_attention_due(&self, instruction_count: u64) -> bool {
        let _ = instruction_count;
        false
    }

    fn batch_attention_limit(&self, start: u64, budget: u64) -> Option<(u64, bool)> {
        let _ = (start, budget);
        None
    }

    fn batch_attention_raised(&self) -> bool {
        false
    }

    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError>;

    fn read_phys_u16(&mut self, addr: PhysAddr) -> Result<u16, MemError> {
        let lo = self.read_phys_u8(addr)? as u16;
        let hi = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(1)))? as u16;
        Ok(lo | (hi << 8))
    }

    fn read_phys_u32(&mut self, addr: PhysAddr) -> Result<u32, MemError> {
        let b0 = self.read_phys_u8(addr)? as u32;
        let b1 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(1)))? as u32;
        let b2 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(2)))? as u32;
        let b3 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(3)))? as u32;
        Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
    }

    fn read_phys_u64(&mut self, addr: PhysAddr) -> Result<u64, MemError> {
        let b0 = self.read_phys_u8(addr)? as u64;
        let b1 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(1)))? as u64;
        let b2 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(2)))? as u64;
        let b3 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(3)))? as u64;
        let b4 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(4)))? as u64;
        let b5 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(5)))? as u64;
        let b6 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(6)))? as u64;
        let b7 = self.read_phys_u8(PhysAddr(addr.0.wrapping_add(7)))? as u64;
        Ok(b0
            | (b1 << 8)
            | (b2 << 16)
            | (b3 << 24)
            | (b4 << 32)
            | (b5 << 40)
            | (b6 << 48)
            | (b7 << 56))
    }

    fn read_cached_backing_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.read_phys_u8(addr)
    }

    fn read_cached_backing_u16(&mut self, addr: PhysAddr) -> Result<u16, MemError> {
        self.read_phys_u16(addr)
    }

    fn read_cached_backing_u32(&mut self, addr: PhysAddr) -> Result<u32, MemError> {
        self.read_phys_u32(addr)
    }

    fn cached_backing_token(&self, addr: PhysAddr) -> BackingToken {
        BackingToken(addr.0)
    }

    /// Optional ordinary RAM source for a JIT walk comparison. The complete
    /// containing page must have stable, side-effect-free backing. The host
    /// adapter also gates native rechecks when its physical decode is unsuitable.
    #[cfg(feature = "jit")]
    fn jit_page_table_source(&self, _addr: PhysAddr) -> Option<BackingToken> {
        None
    }

    /// Optional native backing for one already validated data access, never an
    /// interpreter-visible CPU cache or execute token. The containing 4 KiB page
    /// supports ordinary loads/stores while execution stays native-only. The
    /// adapter performs host bookkeeping before returning a write token.
    /// HostBus uses this only for its linear framebuffer, not VGA registers or
    /// banked/planar accesses. Interpretation or device/event handling ends
    /// the proof. Wasm may borrow a TLB row during an exclusive generated call,
    /// restoring the original RAM row before any Rust cache operation resumes.
    #[cfg(feature = "jit")]
    fn jit_uncached_data_source(&mut self, _addr: PhysAddr, _write: bool) -> Option<BackingToken> {
        None
    }

    fn cached_backing_phys(&self, token: BackingToken) -> PhysAddr {
        PhysAddr(token.0)
    }

    /// Reads through a token issued by `cached_backing_token` after the bus
    /// approved the entire access as stable backing memory.
    ///
    /// A failure here is a backend cache-coherency bug, not an architectural
    /// memory fault: buses must invalidate CPU fetch caches before a token can
    /// stop naming readable backing memory.
    fn read_backing_token_u8(&mut self, token: BackingToken) -> u8 {
        match self.read_cached_backing_u8(PhysAddr(token.0)) {
            Ok(value) => value,
            Err(_) => panic!("stale backing token"),
        }
    }

    fn read_backing_token_u16(&mut self, token: BackingToken) -> u16 {
        match self.read_cached_backing_u16(PhysAddr(token.0)) {
            Ok(value) => value,
            Err(_) => panic!("stale backing token"),
        }
    }

    fn read_backing_token_u32(&mut self, token: BackingToken) -> u32 {
        match self.read_cached_backing_u32(PhysAddr(token.0)) {
            Ok(value) => value,
            Err(_) => panic!("stale backing token"),
        }
    }

    /// Commit through a writable RAM token after `before_cached_ram_write`.
    /// MMU callers qualify every page with a live writable translation or
    /// `cached_write_translation_allowed`. Read/fetch tokens alone do not
    /// grant that access. A bus may omit code/table notifications here when its
    /// write-fill policy excludes those pages and promotion revokes old aliases.
    /// The same contract applies to the wider token stores below.
    fn write_backing_token_u8(&mut self, token: BackingToken, value: u8) -> Result<(), MemError> {
        self.write_cached_ram_u8(self.cached_backing_phys(token), value)
    }

    fn write_backing_token_u16(&mut self, token: BackingToken, value: u16) -> Result<(), MemError> {
        self.write_cached_ram_u16(self.cached_backing_phys(token), value)
    }

    fn write_backing_token_u32(&mut self, token: BackingToken, value: u32) -> Result<(), MemError> {
        self.write_cached_ram_u32(self.cached_backing_phys(token), value)
    }

    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError>;
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError>;

    fn write_phys_u16(&mut self, addr: PhysAddr, value: u16) -> Result<(), MemError> {
        self.write_phys_u8(addr, value as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(1)), (value >> 8) as u8)
    }

    fn write_phys_u32(&mut self, addr: PhysAddr, value: u32) -> Result<(), MemError> {
        self.write_phys_u8(addr, value as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(1)), (value >> 8) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(2)), (value >> 16) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(3)), (value >> 24) as u8)
    }

    fn write_phys_u64(&mut self, addr: PhysAddr, value: u64) -> Result<(), MemError> {
        self.write_phys_u8(addr, value as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(1)), (value >> 8) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(2)), (value >> 16) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(3)), (value >> 24) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(4)), (value >> 32) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(5)), (value >> 40) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(6)), (value >> 48) as u8)?;
        self.write_phys_u8(PhysAddr(addr.0.wrapping_add(7)), (value >> 56) as u8)
    }

    fn write_cached_ram_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        self.write_phys_u8(addr, value)
    }

    fn write_cached_ram_u16(&mut self, addr: PhysAddr, value: u16) -> Result<(), MemError> {
        self.write_phys_u16(addr, value)
    }

    fn write_cached_ram_u32(&mut self, addr: PhysAddr, value: u32) -> Result<(), MemError> {
        self.write_phys_u32(addr, value)
    }

    fn phys_ram_repeat_write_supported(&self, addr: PhysAddr, len: u32) -> bool {
        let _ = (addr, len);
        false
    }

    fn write_phys_ram_repeat(
        &mut self,
        addr: PhysAddr,
        width: u8,
        value: u32,
        units: u32,
    ) -> Result<(), MemError> {
        let _ = (width, value, units);
        Err(MemError::OutOfRange { addr })
    }

    fn phys_ram_copy_supported(&self, src: PhysAddr, dst: PhysAddr, len: u32) -> bool {
        let _ = (src, dst, len);
        false
    }

    fn copy_phys_to_ram(&mut self, src: PhysAddr, dst: PhysAddr, len: u32) -> Result<(), MemError> {
        let _ = (src, len);
        Err(MemError::OutOfRange { addr: dst })
    }

    fn classify_phys(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        let _ = (addr, len);
        PhysRegionKind::Ram
    }

    /// Record a page-table dependency. True means newly tracked: the caller
    /// revokes writable aliases identified by cached_backing_token(frame.base()).
    /// Discovery alone does not change a mapping; later table writes must still
    /// invalidate dependent translations through the ordinary tracked path.
    fn note_page_table_frame(
        &mut self,
        frame: PhysFrame,
        role: PageTableFrameRole,
    ) -> Result<bool, MemError> {
        let _ = (frame, role);
        Ok(false)
    }

    fn before_phys_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
        let _ = write;
        Ok(())
    }

    fn before_cached_ram_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
        self.before_phys_write(write)
    }

    fn linear_translation_cache_enabled(&self) -> bool {
        false
    }

    /// Consume a physical decoder change caused by I/O (for example a PCI BAR
    /// relocation). Cached RAM tokens, fetches and native links depend on the
    /// old decoder. The CPU calls this after each completed I/O write, including
    /// individual REP OUTS elements, before another memory access can use them.
    fn take_physical_mapping_change(&mut self) -> bool {
        false
    }

    fn translation_cache_flush_after_phys_write(&self, write: PhysWrite) -> bool {
        let _ = write;
        false
    }

    /// A completed guest write invalidated all CPU data/fetch translations and
    /// native mapping links. The bus may retire watches owned solely by those
    /// translations. Backends retaining other native mappings must retain their
    /// dependencies. Never call this during a page walk or before write commit.
    fn after_write_translation_flush(&mut self) {}

    /// Whether a write through a stable RAM token must invalidate matching
    /// instruction-fetch translations. Backends whose tokens read live,
    /// fixed backing memory may return false: self-modifying code is then
    /// visible through the existing token without rebuilding its translation.
    fn cached_ram_write_needs_fetch_invalidation(&self) -> bool {
        true
    }

    /// Whether a cached write still needs a page-table-frame lookup on every
    /// hit. A backend may return false only if newly discovered page-table
    /// frames invalidate prior CPU translations and such frames are excluded
    /// from subsequent write-cache fills.
    fn cached_ram_write_needs_dynamic_translation_flush(&self) -> bool {
        true
    }

    fn cached_write_translation_allowed(&self, phys: PhysAddr) -> bool {
        let _ = phys;
        true
    }

    /// Cache notification after a successful walk. As with native-read hooks,
    /// `eflags` supplies control bits; lazy arithmetic status is not evaluated.
    fn after_linear_translate(
        &mut self,
        linear: LinearAddr,
        access: crate::x86::mmu::AccessKind,
        priv_: crate::x86::mmu::AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
        translation: crate::x86::mmu::Translation,
    ) {
        let _ = (linear, access, priv_, cr0, cr3, cr4, eflags, translation);
    }

    /// Attempt the optional native path once. The backend owns eligibility
    /// checks and returns Unsupported when the access must stay software-only.
    fn try_native_read_linear_u8(
        &mut self,
        linear: LinearAddr,
        access: crate::x86::mmu::AccessKind,
        priv_: crate::x86::mmu::AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u8> {
        let _ = (linear, access, priv_, cr0, cr3, cr4, eflags);
        NativeRead::Unsupported
    }

    fn try_native_read_linear_u16(
        &mut self,
        linear: LinearAddr,
        access: crate::x86::mmu::AccessKind,
        priv_: crate::x86::mmu::AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u16> {
        let _ = (linear, access, priv_, cr0, cr3, cr4, eflags);
        NativeRead::Unsupported
    }

    fn try_native_read_linear_u32(
        &mut self,
        linear: LinearAddr,
        access: crate::x86::mmu::AccessKind,
        priv_: crate::x86::mmu::AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u32> {
        let _ = (linear, access, priv_, cr0, cr3, cr4, eflags);
        NativeRead::Unsupported
    }

    fn after_control_register_write(&mut self, index: u8, old: u32, new: u32) {
        let _ = (index, old, new);
    }

    fn after_invlpg(&mut self, _linear: LinearAddr) {}

    fn io_read_u8(&mut self, port: u16) -> Result<u8, MemError> {
        Err(MemError::IoUnhandled { port })
    }

    fn io_read_u16(&mut self, port: u16) -> Result<u16, MemError> {
        let lo = self.io_read_u8(port)? as u16;
        let hi = self.io_read_u8(port.wrapping_add(1))? as u16;
        Ok(lo | (hi << 8))
    }

    fn io_read_u32(&mut self, port: u16) -> Result<u32, MemError> {
        let lo = self.io_read_u16(port)? as u32;
        let hi = self.io_read_u16(port.wrapping_add(2))? as u32;
        Ok(lo | (hi << 16))
    }

    fn io_read_to_ram_supported(&self, port: u16, width: u8, dst: PhysAddr, units: u32) -> bool {
        let _ = (port, width, dst, units);
        false
    }

    fn io_read_to_ram(
        &mut self,
        port: u16,
        width: u8,
        dst: PhysAddr,
        units: u32,
    ) -> Result<bool, MemError> {
        let _ = (port, width, dst, units);
        Ok(false)
    }

    fn io_write_u8(&mut self, port: u16, value: u8) -> Result<(), MemError> {
        let _ = value;
        Err(MemError::IoUnhandled { port })
    }

    fn io_write_u16(&mut self, port: u16, value: u16) -> Result<(), MemError> {
        self.io_write_u8(port, value as u8)?;
        self.io_write_u8(port.wrapping_add(1), (value >> 8) as u8)
    }

    fn io_write_u32(&mut self, port: u16, value: u32) -> Result<(), MemError> {
        self.io_write_u16(port, value as u16)?;
        self.io_write_u16(port.wrapping_add(2), (value >> 16) as u16)
    }
}

pub fn check_phys_ram<B: Bus>(bus: &B, addr: PhysAddr, len: u32) -> Result<(), MemError> {
    match bus.classify_phys(addr, len) {
        PhysRegionKind::Ram => Ok(()),
        PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => Err(MemError::Mmio { addr }),
        PhysRegionKind::Rom | PhysRegionKind::Hole | PhysRegionKind::Mixed => {
            Err(MemError::OutOfRange { addr })
        }
    }
}

pub fn read_phys_u16<B: Bus>(bus: &mut B, addr: PhysAddr) -> Result<u16, MemError> {
    bus.read_phys_u16(addr)
}

pub fn read_phys_u32<B: Bus>(bus: &mut B, addr: PhysAddr) -> Result<u32, MemError> {
    bus.read_phys_u32(addr)
}

pub fn read_phys_u64<B: Bus>(bus: &mut B, addr: PhysAddr) -> Result<u64, MemError> {
    bus.read_phys_u64(addr)
}

pub fn write_phys_u8_with_origin<B: Bus>(
    bus: &mut B,
    addr: PhysAddr,
    value: u8,
    origin: WriteOrigin,
) -> Result<(), MemError> {
    bus.before_phys_write(PhysWrite::new(origin, addr, 1))?;
    bus.write_phys_u8(addr, value)
}

pub fn write_phys_u16<B: Bus>(bus: &mut B, addr: PhysAddr, value: u16) -> Result<(), MemError> {
    write_phys_u16_with_origin(bus, addr, value, WriteOrigin::Cpu)
}

pub fn write_phys_u16_with_origin<B: Bus>(
    bus: &mut B,
    addr: PhysAddr,
    value: u16,
    origin: WriteOrigin,
) -> Result<(), MemError> {
    bus.before_phys_write(PhysWrite::new(origin, addr, 2))?;
    bus.write_phys_u16(addr, value)
}

pub fn write_phys_u32<B: Bus>(bus: &mut B, addr: PhysAddr, value: u32) -> Result<(), MemError> {
    write_phys_u32_with_origin(bus, addr, value, WriteOrigin::Cpu)
}

pub fn write_phys_u32_with_origin<B: Bus>(
    bus: &mut B,
    addr: PhysAddr,
    value: u32,
    origin: WriteOrigin,
) -> Result<(), MemError> {
    bus.before_phys_write(PhysWrite::new(origin, addr, 4))?;
    bus.write_phys_u32(addr, value)
}

pub fn write_phys_u64<B: Bus>(bus: &mut B, addr: PhysAddr, value: u64) -> Result<(), MemError> {
    write_phys_u64_with_origin(bus, addr, value, WriteOrigin::Cpu)
}

pub fn write_phys_u64_with_origin<B: Bus>(
    bus: &mut B,
    addr: PhysAddr,
    value: u64,
    origin: WriteOrigin,
) -> Result<(), MemError> {
    bus.before_phys_write(PhysWrite::new(origin, addr, 8))?;
    bus.write_phys_u64(addr, value)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    struct TrackingBus {
        data: [u8; 32],
        writes: [Option<PhysWrite>; 8],
        write_count: usize,
        noted_frame: Option<(PhysFrame, PageTableFrameRole)>,
    }

    impl TrackingBus {
        fn new() -> Self {
            Self {
                data: [0; 32],
                writes: [None; 8],
                write_count: 0,
                noted_frame: None,
            }
        }
    }

    impl Bus for TrackingBus {
        fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
            self.data
                .get(addr.0 as usize)
                .copied()
                .ok_or(MemError::OutOfRange { addr })
        }

        fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
            self.data
                .get(addr.0 as usize)
                .map(|_| ())
                .ok_or(MemError::OutOfRange { addr })
        }

        fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
            let byte = self
                .data
                .get_mut(addr.0 as usize)
                .ok_or(MemError::OutOfRange { addr })?;
            *byte = value;
            Ok(())
        }

        fn note_page_table_frame(
            &mut self,
            frame: PhysFrame,
            role: PageTableFrameRole,
        ) -> Result<bool, MemError> {
            let newly_tracked = self.noted_frame.is_none();
            self.noted_frame = Some((frame, role));
            Ok(newly_tracked)
        }

        fn before_phys_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
            self.writes[self.write_count] = Some(write);
            self.write_count += 1;
            Ok(())
        }
    }

    #[test]
    fn region_map_classifies_ranges_without_allocation() {
        let mut map = PhysRegionMap::<4>::new();
        map.push(PhysRegion::ram(PhysAddr(0), 0x1000)).unwrap();
        map.push(PhysRegion::mmio(PhysAddr(0x2000), 0x1000))
            .unwrap();

        assert_eq!(map.classify(PhysAddr(0x100), 4), PhysRegionKind::Ram);
        assert_eq!(map.classify(PhysAddr(0x2ffc), 4), PhysRegionKind::Mmio);
        assert_eq!(map.classify(PhysAddr(0x0ffc), 8), PhysRegionKind::Mixed);
        assert_eq!(map.classify(PhysAddr(0x3000), 1), PhysRegionKind::Hole);
    }

    #[test]
    fn physical_classification_preserves_overlaps_partial_ranges_and_u64_end() {
        let mut map = PhysRegionMap::<3>::new();
        map.push(PhysRegion::ram(PhysAddr(0), 0x1000)).unwrap();
        map.push(PhysRegion::mmio(PhysAddr(0x800), 0x100)).unwrap();
        map.push(PhysRegion::rom(PhysAddr(u64::MAX - 3), 4))
            .unwrap();
        for (address, length, kind) in [
            (0x100, 4, PhysRegionKind::Ram),
            (0x7ff, 2, PhysRegionKind::Mixed),
            (0x800, 4, PhysRegionKind::Mixed),
            (0xfff, 2, PhysRegionKind::Mixed),
            (0x1000, 1, PhysRegionKind::Hole),
            (u64::MAX - 3, 4, PhysRegionKind::Rom),
            (u64::MAX, 1, PhysRegionKind::Rom),
            (u64::MAX, 2, PhysRegionKind::Hole),
            (0, 0, PhysRegionKind::Hole),
        ] {
            assert_eq!(map.classify(PhysAddr(address), length), kind);
        }
    }

    #[test]
    fn region_map_rejects_empty_overflow_and_full_entries() {
        let mut map = PhysRegionMap::<1>::new();
        assert_eq!(
            map.push(PhysRegion::ram(PhysAddr(0), 0)),
            Err(PhysRegionMapError::Empty)
        );
        assert_eq!(
            map.push(PhysRegion::ram(PhysAddr(u64::MAX), 2)),
            Err(PhysRegionMapError::Overflow)
        );
        map.push(PhysRegion::ram(PhysAddr(0), 1)).unwrap();
        assert_eq!(
            map.push(PhysRegion::ram(PhysAddr(1), 1)),
            Err(PhysRegionMapError::Full)
        );
    }

    #[test]
    fn write_helpers_report_one_tracked_event_per_scalar_write() {
        let mut bus = TrackingBus::new();

        write_phys_u32_with_origin(&mut bus, PhysAddr(4), 0x1122_3344, WriteOrigin::PageWalk)
            .unwrap();

        assert_eq!(&bus.data[4..8], &[0x44, 0x33, 0x22, 0x11]);
        assert_eq!(bus.write_count, 1);
        assert_eq!(
            bus.writes[0],
            Some(PhysWrite::new(WriteOrigin::PageWalk, PhysAddr(4), 4))
        );
    }

    #[test]
    fn frame_roles_are_compact_and_page_based() {
        let frame = PhysFrame::containing(PhysAddr(0x1234));
        let mut flags = FrameRoleFlags::RAM;
        flags.insert(FrameRoleFlags::from_page_table_role(
            PageTableFrameRole::Directory,
        ));

        assert_eq!(frame, PhysFrame(1));
        assert_eq!(frame.base(), PhysAddr(0x1000));
        assert!(flags.contains(FrameRoleFlags::RAM));
        assert!(flags.is_page_table());
    }

    #[test]
    fn bus_can_record_page_table_frame_observations() {
        let mut bus = TrackingBus::new();

        bus.note_page_table_frame(PhysFrame(2), PageTableFrameRole::Table)
            .unwrap();

        assert_eq!(
            bus.noted_frame,
            Some((PhysFrame(2), PageTableFrameRole::Table))
        );
    }
}
