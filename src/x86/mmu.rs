use crate::x86::addr::{LinearAddr, PhysAddr};
use crate::x86::cpu::{CR0_PG, CR0_WP, CR4_PAE, CR4_PGE, CR4_PSE, Cpu};
use crate::x86::mem::{self, BackingToken, Bus, MemError, NativeRead};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessKind {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessPriv {
    User,
    Supervisor,
    ImplicitSupervisor,
}

impl AccessPriv {
    pub const fn from_cpl(cpl: u8) -> Self {
        if cpl == 3 {
            Self::User
        } else {
            Self::Supervisor
        }
    }

    pub const fn cpl(self) -> u8 {
        match self {
            Self::User => 3,
            Self::Supervisor | Self::ImplicitSupervisor => 0,
        }
    }

    pub const fn is_user(self) -> bool {
        matches!(self, Self::User)
    }

    pub const fn is_supervisor(self) -> bool {
        !self.is_user()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFault {
    pub linear: LinearAddr,
    pub code: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    Mem(MemError),
    Page(PageFault),
}

impl From<MemError> for Fault {
    fn from(value: MemError) -> Self {
        Self::Mem(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum TranslationPageSize {
    Direct,
    Size4K,
    Size2M,
    Size4M,
}

impl TranslationPageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Direct => 1,
            Self::Size4K => 4 * 1024,
            Self::Size2M => 2 * 1024 * 1024,
            Self::Size4M => 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalkFrame {
    pub frame: mem::PhysFrame,
    pub role: mem::PageTableFrameRole,
}

impl WalkFrame {
    pub const EMPTY: Self = Self {
        frame: mem::PhysFrame(0),
        role: mem::PageTableFrameRole::Table,
    };

    pub const fn new(frame: mem::PhysFrame, role: mem::PageTableFrameRole) -> Self {
        Self { frame, role }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalkFrameSet {
    // Keep frame numbers together: interleaving each with a one-byte role
    // creates seven padding bytes per entry and expensive by-value copies.
    frames: [mem::PhysFrame; 4],
    roles: [mem::PageTableFrameRole; 4],
    len: u32,
}

impl WalkFrameSet {
    pub const fn new() -> Self {
        Self {
            frames: [mem::PhysFrame(0); 4],
            roles: [mem::PageTableFrameRole::Table; 4],
            len: 0,
        }
    }

    pub fn push(&mut self, frame: mem::PhysFrame, role: mem::PageTableFrameRole) -> bool {
        if self.len as usize == self.frames.len() {
            return false;
        }
        self.frames[self.len as usize] = frame;
        self.roles[self.len as usize] = role;
        self.len += 1;
        true
    }

    pub const fn len(self) -> u8 {
        self.len as u8
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn frame(self, index: usize) -> Option<WalkFrame> {
        if index < self.len as usize {
            Some(WalkFrame::new(self.frames[index], self.roles[index]))
        } else {
            None
        }
    }
}

impl Default for WalkFrameSet {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Translation {
    pub phys: PhysAddr,
    pub page_size: TranslationPageSize,
    pub page_table_frames: WalkFrameSet,
    pub all_accessed: bool,
    pub leaf_dirty: bool,
    pub wrote_accessed: bool,
    pub wrote_dirty: bool,
    pub global: bool,
    #[cfg(feature = "jit")]
    pub(crate) recheck: [u32; 2],
}

impl Translation {
    pub const fn new(
        phys: PhysAddr,
        page_size: TranslationPageSize,
        page_table_frames: WalkFrameSet,
        all_accessed: bool,
        leaf_dirty: bool,
        wrote_accessed: bool,
        wrote_dirty: bool,
    ) -> Self {
        Self {
            phys,
            page_size,
            page_table_frames,
            all_accessed,
            leaf_dirty,
            wrote_accessed,
            wrote_dirty,
            global: false,
            #[cfg(feature = "jit")]
            recheck: [0; 2],
        }
    }

    pub fn direct(linear: LinearAddr, access: AccessKind) -> Self {
        Self::new(
            PhysAddr(u64::from(linear.0)),
            TranslationPageSize::Direct,
            WalkFrameSet::new(),
            true,
            matches!(access, AccessKind::Write),
            false,
            false,
        )
    }

    fn with_global(mut self, global: bool) -> Self {
        self.global = global;
        self
    }

    pub const fn page_table_frame_count(self) -> u8 {
        self.page_table_frames.len()
    }

    pub fn page_table_frame(self, index: usize) -> Option<WalkFrame> {
        self.page_table_frames.frame(index)
    }
}

pub const PFEC_PRESENT: u32 = 1 << 0;
pub const PFEC_WRITE: u32 = 1 << 1;
pub const PFEC_USER: u32 = 1 << 2;
pub const PFEC_RSVD: u32 = 1 << 3;
pub const PFEC_INSTR: u32 = 1 << 4;

const PTE_PRESENT: u32 = 1 << 0;
const PTE_WRITE: u32 = 1 << 1;
const PTE_USER: u32 = 1 << 2;
const PTE_ACCESSED: u32 = 1 << 5;
const PTE_DIRTY: u32 = 1 << 6;
const PDE_PS: u32 = 1 << 7;
const PAGE_MASK: u32 = 0xffff_f000;
const LARGE_PAGE_MASK: u32 = 0xffc0_0000;
const LARGE_PAGE_OFFSET_MASK: u32 = 0x003f_ffff;
const PDE_4M_PSE36_RESERVED_MASK: u32 = 0x003e_0000;
const PDE_4M_PSE36_HIGH_ADDR_MASK: u32 = 0x0001_e000;
const PAE_PTE_PRESENT: u64 = PTE_PRESENT as u64;
const PAE_PTE_WRITE: u64 = PTE_WRITE as u64;
const PAE_PTE_USER: u64 = PTE_USER as u64;
const PAE_PTE_ACCESSED: u64 = PTE_ACCESSED as u64;
const PAE_PTE_DIRTY: u64 = PTE_DIRTY as u64;
const PAE_PDE_PS: u64 = PDE_PS as u64;
const PAE_CR3_MASK: u32 = 0xffff_ffe0;
const PAE_PHYS_ADDR_MASK: u64 = 0x0000_000f_ffff_ffff;
const PAE_4K_PAGE_MASK: u64 = 0x0000_000f_ffff_f000;
const PAE_2M_PAGE_MASK: u64 = 0x0000_000f_ffe0_0000;
const PAE_2M_PAGE_OFFSET_MASK: u64 = 0x0000_0000_001f_ffff;
const PAE_LEGACY_HIGH_RESERVED_MASK: u64 = !PAE_PHYS_ADDR_MASK;
const PAE_PDPTE_RESERVED_MASK: u64 = PAE_LEGACY_HIGH_RESERVED_MASK | 0x0000_0000_0000_01e6;
const PAE_ENTRY_RESERVED_MASK: u64 = PAE_LEGACY_HIGH_RESERVED_MASK;
const PAE_PDE_2M_RESERVED_MASK: u64 = PAE_LEGACY_HIGH_RESERVED_MASK | 0x0000_0000_001f_e000;

pub fn translate<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    cpl: u8,
) -> Result<PhysAddr, Fault> {
    Ok(translate_detailed(cpu, bus, linear, access, cpl)?.phys)
}

pub fn translate_with_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<PhysAddr, Fault> {
    Ok(translate_detailed_with_priv(cpu, bus, linear, access, priv_)?.phys)
}

pub fn translate_detailed<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    cpl: u8,
) -> Result<Translation, Fault> {
    translate_detailed_with_priv(cpu, bus, linear, access, AccessPriv::from_cpl(cpl))
}

pub fn translate_detailed_with_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<Translation, Fault> {
    let cr0 = cpu.cr0;
    let cr3 = cpu.cr3;
    let cr4 = cpu.cr4;
    // Cache hooks need control flags (VM), not lazy arithmetic status.
    let eflags = cpu.eflags;
    let translation = if (cpu.cr0 & CR0_PG) == 0 {
        Translation::direct(linear, access)
    } else if (cpu.cr4 & CR4_PAE) != 0 {
        walk_pae(cpu, bus, linear, access, priv_)?
    } else {
        walk_non_pae(cpu, bus, linear, access, priv_)?
    };
    if matches!(
        translation.page_size,
        TranslationPageSize::Size2M | TranslationPageSize::Size4M
    ) {
        cpu.note_large_translation(linear);
    }
    bus.after_linear_translate(linear, access, priv_, cr0, cr3, cr4, eflags, translation);
    Ok(translation)
}

pub fn load_legacy_pae_pdptrs<B: Bus>(bus: &mut B, cr3: u32) -> Result<Option<[u64; 4]>, MemError> {
    let base = u64::from(cr3 & PAE_CR3_MASK);
    let _ = bus.note_page_table_frame(
        mem::PhysFrame::containing(PhysAddr(base)),
        mem::PageTableFrameRole::Pdpt,
    )?;
    let mut entries = [0; 4];
    for index in 0..4 {
        let entry_addr = PhysAddr(base + index * 8);
        let entry = mem::read_phys_u64(bus, entry_addr)?;
        if (entry & PAE_PTE_PRESENT) != 0 && (entry & PAE_PDPTE_RESERVED_MASK) != 0 {
            return Ok(None);
        }
        entries[index as usize] = entry;
    }
    Ok(Some(entries))
}

pub fn validate_legacy_pae_pdptrs<B: Bus>(bus: &mut B, cr3: u32) -> Result<bool, MemError> {
    Ok(load_legacy_pae_pdptrs(bus, cr3)?.is_some())
}

fn cached_read_backing<B: Bus>(
    cpu: &mut Cpu,
    bus: &B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Option<BackingToken> {
    if !bus.linear_translation_cache_enabled() || !matches!(access, AccessKind::Read) {
        return None;
    }
    cpu.cached_read_translation(linear, priv_.is_user())
}

#[inline]
fn cache_native_read<B: Bus>(cpu: &mut Cpu, bus: &mut B, linear: LinearAddr, priv_: AccessPriv) {
    if bus.linear_translation_cache_enabled() {
        if let Some((backing, page_size)) = bus.native_read_backing(linear, priv_) {
            // A retained native mapping can outlive the CPU's software cache.
            // Preserve its original leaf size when repopulating that cache.
            if matches!(
                page_size,
                TranslationPageSize::Size2M | TranslationPageSize::Size4M
            ) {
                cpu.note_large_translation(linear);
            }
            cpu.fill_cached_read_translation(linear, backing, priv_.is_user());
        }
    }
}

fn cached_write_backing<B: Bus>(
    cpu: &mut Cpu,
    bus: &B,
    linear: LinearAddr,
    priv_: AccessPriv,
) -> Option<BackingToken> {
    if !bus.linear_translation_cache_enabled() {
        return None;
    }
    cpu.cached_write_translation(linear, priv_.is_user())
}

fn translation_cache_target_is_ram<B: Bus>(bus: &B, phys: PhysAddr) -> bool {
    let page = PhysAddr(phys.0 & mem::PHYS_PAGE_MASK);
    matches!(bus.classify_phys(page, 4096), mem::PhysRegionKind::Ram)
}

/// Resolve a native data miss without executing an x86 instruction. Ordinary
/// RAM can populate the shared cache; the linear framebuffer supplies a
/// transient native-only pointer and marks dirty pages before writes. It cannot
/// survive interpretation or device/event handling. This performs
/// no paging writes or fault delivery: missing A/D bits, PAE/large pages and
/// permission failures remain the shared walker's responsibility. Paging-off
/// accesses use the same physical backing proof, including A20/overlay/monitor
/// exclusions. No device callback with guest-visible side effects runs here.
///
/// Consulted tiny386's tlb_refill. Unlike an ordinary walker this probe may run
/// while GPRs/EFLAGS remain in ARM registers; it reads only the control/segment
/// state native execution cannot change. v86 cpu.rs's side-effect-free walker
/// instead handles PAE/large pages itself and leaves the TLB unchanged.
#[cfg(all(
    feature = "jit",
    any(
        test,
        all(feature = "wasm-jit", target_arch = "wasm32"),
        all(
            target_arch = "aarch64",
            any(
                all(feature = "host", target_os = "linux"),
                all(feature = "monitor", feature = "machine", target_os = "none")
            )
        )
    )
))]
pub(crate) fn try_resident_data_translation<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    write: bool,
) -> Option<mem::BackingToken> {
    if cpu.cr0 & CR0_PG == 0 {
        let phys = PhysAddr(u64::from(linear.0));
        let Some(token) = bus.jit_page_table_source(phys) else {
            return bus.jit_uncached_data_source(phys, write);
        };
        let writable = bus.cached_write_translation_allowed(phys);
        if write && !writable {
            return None;
        }
        if writable {
            cpu.fill_cached_write_translation(linear, token, cpu.cpl() == 3);
        } else {
            cpu.fill_cached_read_translation(linear, token, cpu.cpl() == 3);
        }
        return Some(token);
    }
    let ResidentPage {
        root,
        leaf,
        pde,
        pte,
        phys,
    } = resident_page(cpu, bus, linear, write)?;
    let priv_ = AccessPriv::from_cpl(cpu.cpl());
    let Some(token) = bus.jit_page_table_source(phys) else {
        // A linear framebuffer pointer may live through this native invocation.
        // Never publish it in Cpu's RAM/fetch caches: their tokens have different
        // ownership and notification rules. A new write still needs its own
        // permission proof and dirty notification before native reuse.
        return bus.jit_uncached_data_source(phys, write);
    };
    let writable = pte & PTE_DIRTY != 0
        && permits_write(cpu.cr0, priv_, pde & pte)
        && bus.cached_write_translation_allowed(phys);
    if write && !writable {
        return None;
    }
    if writable {
        cpu.fill_cached_write_translation(linear, token, priv_.is_user());
    } else {
        cpu.fill_cached_read_translation(linear, token, priv_.is_user());
    }
    cpu.mark_cached_translation_global(linear, cpu.cr4 & CR4_PGE != 0 && pte & (1 << 8) != 0);
    cpu.record_translation_recheck(linear, leaf, [pde, pte]);
    cpu.translation_cache.walk_root = root.0 - u64::from(linear.0 >> 22) * 4;
    Some(token)
}

#[cfg(all(
    feature = "jit",
    any(
        test,
        all(feature = "wasm-jit", target_arch = "wasm32"),
        all(
            target_arch = "aarch64",
            any(
                all(feature = "host", target_os = "linux"),
                all(feature = "monitor", feature = "machine", target_os = "none")
            )
        )
    )
))]
struct ResidentPage {
    root: BackingToken,
    leaf: BackingToken,
    pde: u32,
    pte: u32,
    phys: PhysAddr,
}

// One permission/A-D proof for the software refill and native fault refill.
// Read only stable RAM words; no x86 state, guest memory or device effects.
#[cfg(all(
    feature = "jit",
    any(
        test,
        all(feature = "wasm-jit", target_arch = "wasm32"),
        all(
            target_arch = "aarch64",
            any(
                all(feature = "host", target_os = "linux"),
                all(feature = "monitor", feature = "machine", target_os = "none")
            )
        )
    )
))]
#[inline(always)]
fn resident_page<B: Bus>(
    cpu: &Cpu,
    bus: &mut B,
    linear: LinearAddr,
    write: bool,
) -> Option<ResidentPage> {
    if cpu.cr4 & CR4_PAE != 0 {
        return None;
    }
    let pde_addr = PhysAddr(u64::from((cpu.cr3 & PAGE_MASK) + (linear.0 >> 22) * 4));
    let root = bus.jit_page_table_source(pde_addr)?;
    let pde = bus.read_backing_token_u32(root);
    if pde & (PTE_PRESENT | PTE_ACCESSED) != PTE_PRESENT | PTE_ACCESSED
        || pde & PDE_PS != 0 && cpu.cr4 & CR4_PSE != 0
    {
        return None;
    }
    let pte_addr = PhysAddr(u64::from((pde & PAGE_MASK) + ((linear.0 >> 12) & 1023) * 4));
    let leaf = bus.jit_page_table_source(pte_addr)?;
    let pte = bus.read_backing_token_u32(leaf);
    let required = PTE_PRESENT | PTE_ACCESSED | if write { PTE_DIRTY } else { 0 };
    let priv_ = AccessPriv::from_cpl(cpu.cpl());
    if pte & required != required
        || priv_.is_user() && (pde & pte) & PTE_USER == 0
        || write && !permits_write(cpu.cr0, priv_, pde & pte)
    {
        return None;
    }
    let phys = PhysAddr(u64::from((pte & PAGE_MASK) | (linear.0 & !PAGE_MASK)));
    Some(ResidentPage {
        root,
        leaf,
        pde,
        pte,
        phys,
    })
}

/// A recoverable native fault may only retry after a side-effect-free proof.
/// Missing A/D, PAE/large pages and devices leave through the precise exit.
/// Read refills remain read-only: speculative writable aliases make later
/// code/table promotion retire unrelated hardware mappings.
#[cfg(all(
    feature = "jit",
    feature = "monitor",
    feature = "machine",
    target_arch = "aarch64",
    target_os = "none"
))]
pub(crate) fn try_resident_native_translation<B: Bus>(
    cpu: &Cpu,
    bus: &mut B,
    linear: LinearAddr,
    write: bool,
) -> Option<Translation> {
    let page = resident_page(cpu, bus, linear, write)?;
    bus.jit_page_table_source(page.phys)?;
    if write && !bus.cached_write_translation_allowed(page.phys) {
        return None;
    }
    // Hardware mappings survive CR3 switches only with complete source
    // dependencies. The monitor admits these after checking that both source
    // frames are already protected; this probe never promotes a live writable
    // frame or invalidates a native block's earlier range proofs.
    let mut frames = WalkFrameSet::new();
    assert!(frames.push(
        mem::PhysFrame::containing(PhysAddr(u64::from(cpu.cr3 & PAGE_MASK))),
        mem::PageTableFrameRole::Directory
    ));
    assert!(frames.push(
        mem::PhysFrame::containing(PhysAddr(u64::from(page.pde & PAGE_MASK))),
        mem::PageTableFrameRole::Table
    ));
    let translation = Translation::new(
        page.phys,
        TranslationPageSize::Size4K,
        frames,
        true,
        page.pte & PTE_DIRTY != 0,
        false,
        false,
    )
    .with_global(cpu.cr4 & CR4_PGE != 0 && page.pte & (1 << 8) != 0);
    Some(translation)
}

fn fill_linear_translation_cache<B: Bus>(
    cpu: &mut Cpu,
    bus: &B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
    translation: Translation,
) {
    if !bus.linear_translation_cache_enabled()
        || !translation_cache_target_is_ram(bus, translation.phys)
    {
        return;
    }
    match (access, translation.page_size) {
        (
            AccessKind::Read,
            TranslationPageSize::Direct
            | TranslationPageSize::Size4K
            | TranslationPageSize::Size2M
            | TranslationPageSize::Size4M,
        ) => {
            let backing = bus.cached_backing_token(translation.phys);
            #[cfg(feature = "jit")]
            let already_writable = translation.leaf_dirty
                && translation.recheck[0] != 0
                && permits_write(
                    cpu.cr0,
                    priv_,
                    translation.recheck[0] & translation.recheck[1],
                )
                && bus.cached_write_translation_allowed(translation.phys);
            #[cfg(not(feature = "jit"))]
            let already_writable = false;
            if already_writable {
                // The successful read already observed A/D and permissions for
                // a write. Granting the token changes no paging bits or RAM;
                // watched code remains excluded by the bus's ordinary policy.
                cpu.fill_cached_write_translation(linear, backing, priv_.is_user());
            } else {
                cpu.fill_cached_read_translation(linear, backing, priv_.is_user());
            }
            finish_cached_translation(cpu, bus, linear, &translation);
        }
        (
            AccessKind::Write,
            TranslationPageSize::Direct
            | TranslationPageSize::Size4K
            | TranslationPageSize::Size2M
            | TranslationPageSize::Size4M,
        ) => {
            if bus.cached_write_translation_allowed(translation.phys) {
                cpu.fill_cached_write_translation(
                    linear,
                    bus.cached_backing_token(translation.phys),
                    priv_.is_user(),
                );
                finish_cached_translation(cpu, bus, linear, &translation);
            }
        }
        _ => {}
    }
}

fn finish_cached_translation<B: Bus>(
    cpu: &mut Cpu,
    bus: &B,
    linear: LinearAddr,
    translation: &Translation,
) {
    cpu.mark_cached_translation_global(linear, translation.global);
    #[cfg(feature = "jit")]
    if translation.recheck[0] != 0 {
        let pte_addr = (translation.recheck[0] & PAGE_MASK) + ((linear.0 >> 12) & 1023) * 4;
        if let Some(token) = bus.jit_page_table_source(PhysAddr(u64::from(pte_addr))) {
            cpu.record_translation_recheck(linear, token, translation.recheck);
        }
    }
    #[cfg(not(feature = "jit"))]
    let _ = bus;
}

#[cfg(feature = "jit")]
pub(crate) fn finish_fetch_translation<B: Bus>(
    cpu: &mut Cpu,
    bus: &B,
    linear: LinearAddr,
    translation: &Translation,
) {
    if translation.recheck[0] != 0 {
        let pte_addr = (translation.recheck[0] & PAGE_MASK) + ((linear.0 >> 12) & 1023) * 4;
        if let Some(token) = bus.jit_page_table_source(PhysAddr(u64::from(pte_addr))) {
            cpu.record_fetch_recheck(linear, token, translation.recheck);
        }
    }
}

pub fn write_probed_phys_u8<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    phys: PhysAddr,
    value: u8,
) -> Result<(), Fault> {
    let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, phys, 1);
    mem::write_phys_u8_with_origin(bus, phys, value, mem::WriteOrigin::Cpu)?;
    finish_uncached_phys_write(cpu, bus, write);
    Ok(())
}

pub fn write_probed_phys_u16<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    phys: PhysAddr,
    value: u16,
) -> Result<(), Fault> {
    let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, phys, 2);
    mem::write_phys_u16_with_origin(bus, phys, value, mem::WriteOrigin::Cpu)?;
    finish_uncached_phys_write(cpu, bus, write);
    Ok(())
}

pub fn write_probed_phys_u32<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    phys: PhysAddr,
    value: u32,
) -> Result<(), Fault> {
    let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, phys, 4);
    mem::write_phys_u32_with_origin(bus, phys, value, mem::WriteOrigin::Cpu)?;
    finish_uncached_phys_write(cpu, bus, write);
    Ok(())
}

#[inline]
fn finish_uncached_phys_write<B: Bus>(cpu: &mut Cpu, bus: &mut B, write: mem::PhysWrite) {
    if bus.translation_cache_flush_after_phys_write(write) {
        cpu.invalidate_translation_context();
        bus.after_write_translation_flush();
    } else {
        cpu.invalidate_fetch_cache_for_phys_write(write);
    }
}

fn write_probed_cached_ram_u8<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    backing: BackingToken,
    value: u8,
) -> Result<(), Fault> {
    let phys = bus.cached_backing_phys(backing);
    let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, phys, 1);
    bus.before_cached_ram_write(write)?;
    bus.write_backing_token_u8(backing, value)?;
    if bus.cached_ram_write_needs_dynamic_translation_flush()
        && bus.translation_cache_flush_after_phys_write(write)
    {
        cpu.invalidate_translation_context();
    } else if bus.cached_ram_write_needs_fetch_invalidation() {
        cpu.invalidate_fetch_cache_for_phys_write(write);
    }
    Ok(())
}

fn write_probed_cached_ram_u16<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    backing: BackingToken,
    value: u16,
) -> Result<(), Fault> {
    let phys = bus.cached_backing_phys(backing);
    let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, phys, 2);
    bus.before_cached_ram_write(write)?;
    bus.write_backing_token_u16(backing, value)?;
    if bus.cached_ram_write_needs_dynamic_translation_flush()
        && bus.translation_cache_flush_after_phys_write(write)
    {
        cpu.invalidate_translation_context();
    } else if bus.cached_ram_write_needs_fetch_invalidation() {
        cpu.invalidate_fetch_cache_for_phys_write(write);
    }
    Ok(())
}

fn write_probed_cached_ram_u32<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    backing: BackingToken,
    value: u32,
) -> Result<(), Fault> {
    let phys = bus.cached_backing_phys(backing);
    let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, phys, 4);
    bus.before_cached_ram_write(write)?;
    bus.write_backing_token_u32(backing, value)?;
    if bus.cached_ram_write_needs_dynamic_translation_flush()
        && bus.translation_cache_flush_after_phys_write(write)
    {
        cpu.invalidate_translation_context();
    } else if bus.cached_ram_write_needs_fetch_invalidation() {
        cpu.invalidate_fetch_cache_for_phys_write(write);
    }
    Ok(())
}

pub fn write_probed_phys_repeat<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    phys: PhysAddr,
    width: u8,
    value: u32,
    units: u32,
) -> Result<bool, Fault> {
    let Some(len) = u32::from(width).checked_mul(units) else {
        return Ok(false);
    };
    if len == 0 || !matches!(width, 1 | 2 | 4) || !bus.phys_ram_repeat_write_supported(phys, len) {
        return Ok(false);
    }

    let mut offset = 0u32;
    let mut flush_translation = false;
    while offset < len {
        let chunk = (len - offset).min(u32::from(u8::MAX)) as u8;
        let Some(addr) = phys.0.checked_add(u64::from(offset)) else {
            return Ok(false);
        };
        let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, PhysAddr(addr), chunk);
        bus.before_phys_write(write)?;
        if bus.translation_cache_flush_after_phys_write(write) {
            flush_translation = true;
        }
        offset += u32::from(chunk);
    }

    bus.write_phys_ram_repeat(phys, width, value, units)?;

    finish_phys_range_write(cpu, bus, phys, len, flush_translation);
    Ok(true)
}

pub fn write_probed_phys_copy<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    src: PhysAddr,
    dst: PhysAddr,
    len: u32,
) -> Result<bool, Fault> {
    if len == 0 || !bus.phys_ram_copy_supported(src, dst, len) {
        return Ok(false);
    }

    let mut offset = 0u32;
    let mut flush_translation = false;
    while offset < len {
        let chunk = (len - offset).min(u32::from(u8::MAX)) as u8;
        let Some(addr) = dst.0.checked_add(u64::from(offset)) else {
            return Ok(false);
        };
        let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, PhysAddr(addr), chunk);
        bus.before_phys_write(write)?;
        if bus.translation_cache_flush_after_phys_write(write) {
            flush_translation = true;
        }
        offset += u32::from(chunk);
    }

    bus.copy_phys_to_ram(src, dst, len)?;

    finish_phys_range_write(cpu, bus, dst, len, flush_translation);
    Ok(true)
}

pub fn write_probed_phys_from_io<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    port: u16,
    width: u8,
    dst: PhysAddr,
    units: u32,
) -> Result<bool, Fault> {
    let Some(len) = u32::from(width).checked_mul(units) else {
        return Ok(false);
    };
    if len == 0
        || !matches!(width, 1 | 2 | 4)
        || !bus.io_read_to_ram_supported(port, width, dst, units)
    {
        return Ok(false);
    }

    let mut offset = 0u32;
    let mut flush_translation = false;
    while offset < len {
        let chunk = (len - offset).min(u32::from(u8::MAX)) as u8;
        let Some(addr) = dst.0.checked_add(u64::from(offset)) else {
            return Ok(false);
        };
        let write = mem::PhysWrite::new(mem::WriteOrigin::Cpu, PhysAddr(addr), chunk);
        bus.before_phys_write(write)?;
        if bus.translation_cache_flush_after_phys_write(write) {
            flush_translation = true;
        }
        offset += u32::from(chunk);
    }

    if !bus.io_read_to_ram(port, width, dst, units)? {
        return Ok(false);
    }

    finish_phys_range_write(cpu, bus, dst, len, flush_translation);
    Ok(true)
}

#[inline]
fn finish_phys_range_write<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    phys: PhysAddr,
    len: u32,
    flush_translation: bool,
) {
    if flush_translation {
        cpu.invalidate_translation_context();
        bus.after_write_translation_flush();
    } else {
        cpu.invalidate_fetch_cache_for_phys_range(phys, len);
    }
}

fn contiguous_ram_commit<B: Bus>(bus: &B, phys: PhysAddr, len: u32) -> bool {
    if len == 0 {
        return false;
    }
    let Some(last) = phys.0.checked_add(u64::from(len - 1)) else {
        return false;
    };
    if (phys.0 & mem::PHYS_PAGE_MASK) != (last & mem::PHYS_PAGE_MASK) {
        return false;
    }
    matches!(bus.classify_phys(phys, len), mem::PhysRegionKind::Ram)
}

#[inline]
fn same_linear_page(linear: LinearAddr, len: u32) -> bool {
    if len == 0 {
        return false;
    }
    let Some(last) = linear.0.checked_add(len - 1) else {
        return false;
    };
    (linear.0 & 0xffff_f000) == (last & 0xffff_f000)
}

fn probe_contiguous_phys_write<B: Bus>(bus: &mut B, phys: PhysAddr, len: u8) -> Result<(), Fault> {
    for offset in 0..len {
        bus.probe_phys_write_u8(PhysAddr(phys.0 + u64::from(offset)))?;
    }
    Ok(())
}

pub fn read_linear_u8<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
) -> Result<u8, Fault> {
    let cpl = cpu.cpl();
    read_linear_u8_at_cpl(cpu, bus, linear, access, cpl)
}

pub fn read_linear_u8_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    cpl: u8,
) -> Result<u8, Fault> {
    read_linear_u8_at_priv(cpu, bus, linear, access, AccessPriv::from_cpl(cpl))
}

pub fn read_linear_u8_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<u8, Fault> {
    if let Some(backing) = cached_read_backing(cpu, bus, linear, access, priv_) {
        return Ok(bus.read_backing_token_u8(backing));
    }
    match bus
        .try_native_read_linear_u8(linear, access, priv_, cpu.cr0, cpu.cr3, cpu.cr4, cpu.eflags)
    {
        NativeRead::Hit(value) => {
            cache_native_read(cpu, bus, linear, priv_);
            return Ok(value);
        }
        NativeRead::Miss | NativeRead::Unsupported => {}
    }
    let translation = translate_detailed_with_priv(cpu, bus, linear, access, priv_)?;
    let value = bus.read_phys_u8(translation.phys)?;
    fill_linear_translation_cache(cpu, bus, linear, access, priv_, translation);
    Ok(value)
}

pub fn read_linear_u16<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
) -> Result<u16, Fault> {
    let cpl = cpu.cpl();
    read_linear_u16_at_cpl(cpu, bus, linear, access, cpl)
}

pub fn read_linear_u16_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    cpl: u8,
) -> Result<u16, Fault> {
    read_linear_u16_at_priv(cpu, bus, linear, access, AccessPriv::from_cpl(cpl))
}

pub fn read_linear_u16_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<u16, Fault> {
    if (linear.0 & 0xfff) <= 0xffe {
        if let Some(backing) = cached_read_backing(cpu, bus, linear, access, priv_) {
            return Ok(bus.read_backing_token_u16(backing));
        }
        match bus.try_native_read_linear_u16(
            linear, access, priv_, cpu.cr0, cpu.cr3, cpu.cr4, cpu.eflags,
        ) {
            NativeRead::Hit(value) => {
                cache_native_read(cpu, bus, linear, priv_);
                return Ok(value);
            }
            NativeRead::Miss | NativeRead::Unsupported => {}
        }
        let translation = translate_detailed_with_priv(cpu, bus, linear, access, priv_)?;
        let value = bus.read_phys_u16(translation.phys)?;
        fill_linear_translation_cache(cpu, bus, linear, access, priv_, translation);
        return Ok(value);
    }
    let lo = read_linear_u8_at_priv(cpu, bus, linear, access, priv_)? as u16;
    let hi = read_linear_u8_at_priv(
        cpu,
        bus,
        LinearAddr(linear.0.wrapping_add(1)),
        access,
        priv_,
    )? as u16;
    Ok(lo | (hi << 8))
}

#[inline(always)]
pub(crate) fn try_cached_linear_read_u16<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
) -> Option<u16> {
    if (linear.0 & 0xfff) > 0xffe {
        return None;
    }
    if !bus.linear_translation_cache_enabled() {
        return None;
    }
    cpu.cached_current_read_translation(linear)
        .map(|backing| bus.read_backing_token_u16(backing))
}

pub fn read_linear_u32<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
) -> Result<u32, Fault> {
    let cpl = cpu.cpl();
    read_linear_u32_at_cpl(cpu, bus, linear, access, cpl)
}

/// Read the operand of a writing RMW using WRITE translation proofs. In
/// particular, a non-present page raises a write #PF before reading data, and
/// a split operand qualifies both pages before an MMIO read can have effects.
/// Consulted v86 safe_read_write32, which also qualifies both pages first.
pub(crate) fn read_linear_rmw_u32<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
) -> Result<u32, Fault> {
    let priv_ = AccessPriv::from_cpl(cpu.cpl());
    let within_page = (linear.0 & 0xfff) <= 0xffc;
    if within_page {
        if let Some(backing) = cached_write_backing(cpu, bus, linear, priv_) {
            return Ok(bus.read_backing_token_u32(backing));
        }
    }
    let mut physical = [PhysAddr(0); 4];
    for (n, slot) in physical.iter_mut().enumerate() {
        *slot = probe_linear_write_u8_at_priv(
            cpu,
            bus,
            LinearAddr(linear.0.wrapping_add(n as u32)),
            priv_,
        )?;
    }
    if within_page {
        return Ok(bus.read_phys_u32(physical[0])?);
    }
    let mut value = 0;
    for (n, addr) in physical.into_iter().enumerate() {
        value |= u32::from(bus.read_phys_u8(addr)?) << (n * 8);
    }
    Ok(value)
}

#[inline(always)]
pub(crate) fn try_fast_linear_read_u32<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
) -> Result<Option<u32>, Fault> {
    if (linear.0 & 0xfff) > 0xffc {
        return Ok(None);
    }
    // The existing software TLB is cheaper than a recoverable thunk on hits.
    // Retained ARM mappings serve as the next cache level after a software miss.
    if bus.linear_translation_cache_enabled() {
        if let Some(backing) = cpu.cached_current_read_translation(linear) {
            return Ok(Some(bus.read_backing_token_u32(backing)));
        }
    }
    Ok(None)
}

pub fn read_linear_u32_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    cpl: u8,
) -> Result<u32, Fault> {
    read_linear_u32_at_priv(cpu, bus, linear, access, AccessPriv::from_cpl(cpl))
}

pub fn read_linear_u32_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<u32, Fault> {
    if (linear.0 & 0xfff) <= 0xffc {
        if let Some(backing) = cached_read_backing(cpu, bus, linear, access, priv_) {
            return Ok(bus.read_backing_token_u32(backing));
        }
        return read_linear_u32_uncached(cpu, bus, linear, access, priv_);
    }
    read_linear_u32_split(cpu, bus, linear, access, priv_)
}

// Bare-metal native misses need a call boundary to keep software hits frameless.
// In userspace the native branch folds away; inline the resulting forwarding
// function so the optional backend adds no branch to the software miss path.
#[cfg_attr(all(target_arch = "aarch64", target_os = "none"), inline(never))]
#[cfg_attr(not(all(target_arch = "aarch64", target_os = "none")), inline)]
fn read_linear_u32_uncached<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<u32, Fault> {
    match bus
        .try_native_read_linear_u32(linear, access, priv_, cpu.cr0, cpu.cr3, cpu.cr4, cpu.eflags)
    {
        NativeRead::Hit(value) => {
            cache_native_read(cpu, bus, linear, priv_);
            return Ok(value);
        }
        NativeRead::Miss | NativeRead::Unsupported => {}
    }
    read_linear_u32_miss(cpu, bus, linear, access, priv_)
}

#[inline(never)]
fn read_linear_u32_miss<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<u32, Fault> {
    let translation = translate_detailed_with_priv(cpu, bus, linear, access, priv_)?;
    let value = bus.read_phys_u32(translation.phys)?;
    fill_linear_translation_cache(cpu, bus, linear, access, priv_, translation);
    Ok(value)
}

#[inline(never)]
fn read_linear_u32_split<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<u32, Fault> {
    let b0 = read_linear_u8_at_priv(cpu, bus, linear, access, priv_)? as u32;
    let b1 = read_linear_u8_at_priv(
        cpu,
        bus,
        LinearAddr(linear.0.wrapping_add(1)),
        access,
        priv_,
    )? as u32;
    let b2 = read_linear_u8_at_priv(
        cpu,
        bus,
        LinearAddr(linear.0.wrapping_add(2)),
        access,
        priv_,
    )? as u32;
    let b3 = read_linear_u8_at_priv(
        cpu,
        bus,
        LinearAddr(linear.0.wrapping_add(3)),
        access,
        priv_,
    )? as u32;
    Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
}

pub fn write_linear_u8<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u8,
) -> Result<(), Fault> {
    let cpl = cpu.cpl();
    write_linear_u8_at_cpl(cpu, bus, linear, value, cpl)
}

pub fn write_linear_u8_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u8,
    cpl: u8,
) -> Result<(), Fault> {
    write_linear_u8_at_priv(cpu, bus, linear, value, AccessPriv::from_cpl(cpl))
}

pub fn write_linear_u8_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u8,
    priv_: AccessPriv,
) -> Result<(), Fault> {
    if let Some(backing) = cached_write_backing(cpu, bus, linear, priv_) {
        return write_probed_cached_ram_u8(cpu, bus, backing, value);
    }
    write_linear_u8_miss(cpu, bus, linear, value, priv_)
}

#[cold]
#[inline(never)]
fn write_linear_u8_miss<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u8,
    priv_: AccessPriv,
) -> Result<(), Fault> {
    let translation = translate_detailed_with_priv(cpu, bus, linear, AccessKind::Write, priv_)?;
    let phys = translation.phys;
    bus.probe_phys_write_u8(phys)?;
    if translation_cache_target_is_ram(bus, phys) && bus.cached_write_translation_allowed(phys) {
        let backing = bus.cached_backing_token(phys);
        cpu.fill_cached_write_translation(linear, backing, priv_.is_user());
        finish_cached_translation(cpu, bus, linear, &translation);
        return write_probed_cached_ram_u8(cpu, bus, backing, value);
    }
    write_probed_phys_u8(cpu, bus, phys, value)
}

pub fn probe_linear_write_u8_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    cpl: u8,
) -> Result<PhysAddr, Fault> {
    probe_linear_write_u8_at_priv(cpu, bus, linear, AccessPriv::from_cpl(cpl))
}

pub fn probe_linear_write_u8_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    priv_: AccessPriv,
) -> Result<PhysAddr, Fault> {
    if let Some(backing) = cached_write_backing(cpu, bus, linear, priv_) {
        return Ok(bus.cached_backing_phys(backing));
    }
    let translation = translate_detailed_with_priv(cpu, bus, linear, AccessKind::Write, priv_)?;
    let phys = translation.phys;
    bus.probe_phys_write_u8(phys)?;
    fill_linear_translation_cache(cpu, bus, linear, AccessKind::Write, priv_, translation);
    Ok(phys)
}

pub fn write_linear_u16<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u16,
) -> Result<(), Fault> {
    let cpl = cpu.cpl();
    write_linear_u16_at_cpl(cpu, bus, linear, value, cpl)
}

pub fn write_linear_u16_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u16,
    cpl: u8,
) -> Result<(), Fault> {
    write_linear_u16_at_priv(cpu, bus, linear, value, AccessPriv::from_cpl(cpl))
}

pub fn write_linear_u16_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u16,
    priv_: AccessPriv,
) -> Result<(), Fault> {
    let same_page = same_linear_page(linear, 2);
    if same_page {
        if let Some(backing) = cached_write_backing(cpu, bus, linear, priv_) {
            return write_probed_cached_ram_u16(cpu, bus, backing, value);
        }
    }
    write_linear_u16_slow(cpu, bus, linear, value, priv_, same_page)
}

#[cold]
#[inline(never)]
fn write_linear_u16_slow<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u16,
    priv_: AccessPriv,
    same_page: bool,
) -> Result<(), Fault> {
    if same_page {
        let translation = translate_detailed_with_priv(cpu, bus, linear, AccessKind::Write, priv_)?;
        let phys = translation.phys;
        if contiguous_ram_commit(bus, phys, 2) {
            if bus.cached_write_translation_allowed(phys) {
                let backing = bus.cached_backing_token(phys);
                cpu.fill_cached_write_translation(linear, backing, priv_.is_user());
                finish_cached_translation(cpu, bus, linear, &translation);
                return write_probed_cached_ram_u16(cpu, bus, backing, value);
            }
            probe_contiguous_phys_write(bus, phys, 2)?;
            return write_probed_phys_u16(cpu, bus, phys, value);
        }
        // One successful walk certifies the entire linear page, including an
        // MMIO target. Keep the ordered byte probes and commits, but do not
        // repeat the same PDE/PTE reads for each byte of this instruction.
        probe_contiguous_phys_write(bus, phys, 2)?;
        write_probed_phys_u8(cpu, bus, phys, value as u8)?;
        return write_probed_phys_u8(cpu, bus, PhysAddr(phys.0 + 1), (value >> 8) as u8);
    }

    let p0 = probe_linear_write_u8_at_priv(cpu, bus, linear, priv_)?;
    let p1 = probe_linear_write_u8_at_priv(cpu, bus, LinearAddr(linear.0.wrapping_add(1)), priv_)?;

    if p1.0 == p0.0.wrapping_add(1) && contiguous_ram_commit(bus, p0, 2) {
        // A contiguous backing range can still cross into watched code or a
        // tracked page table. Both pages must qualify for a token commit.
        if bus.cached_write_translation_allowed(p0) && bus.cached_write_translation_allowed(p1) {
            return write_probed_cached_ram_u16(cpu, bus, bus.cached_backing_token(p0), value);
        }
        return write_probed_phys_u16(cpu, bus, p0, value);
    }

    write_probed_phys_u8(cpu, bus, p0, value as u8)?;
    write_probed_phys_u8(cpu, bus, p1, (value >> 8) as u8)
}

// Outlining this Result-returning hit path forces an sret status store on each
// cached write. Keep it fused with the interpreter; the miss body stays cold.
#[inline(always)]
pub fn write_linear_u32<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u32,
) -> Result<(), Fault> {
    let same_page = same_linear_page(linear, 4);
    if same_page && bus.linear_translation_cache_enabled() {
        if let Some(backing) = cpu.cached_current_write_translation(linear) {
            return write_probed_cached_ram_u32(cpu, bus, backing, value);
        }
    }
    let priv_ = AccessPriv::from_cpl(cpu.cpl());
    write_linear_u32_slow(cpu, bus, linear, value, priv_, same_page)
}

pub fn write_linear_u32_at_cpl<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u32,
    cpl: u8,
) -> Result<(), Fault> {
    write_linear_u32_at_priv(cpu, bus, linear, value, AccessPriv::from_cpl(cpl))
}

pub fn write_linear_u32_at_priv<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u32,
    priv_: AccessPriv,
) -> Result<(), Fault> {
    let same_page = same_linear_page(linear, 4);
    if same_page {
        if let Some(backing) = cached_write_backing(cpu, bus, linear, priv_) {
            return write_probed_cached_ram_u32(cpu, bus, backing, value);
        }
    }
    write_linear_u32_slow(cpu, bus, linear, value, priv_, same_page)
}

#[cold]
#[inline(never)]
fn write_linear_u32_slow<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    value: u32,
    priv_: AccessPriv,
    same_page: bool,
) -> Result<(), Fault> {
    if same_page {
        let translation = translate_detailed_with_priv(cpu, bus, linear, AccessKind::Write, priv_)?;
        let phys = translation.phys;
        if contiguous_ram_commit(bus, phys, 4) {
            if bus.cached_write_translation_allowed(phys) {
                let backing = bus.cached_backing_token(phys);
                cpu.fill_cached_write_translation(linear, backing, priv_.is_user());
                finish_cached_translation(cpu, bus, linear, &translation);
                return write_probed_cached_ram_u32(cpu, bus, backing, value);
            }
            probe_contiguous_phys_write(bus, phys, 4)?;
            return write_probed_phys_u32(cpu, bus, phys, value);
        }
        probe_contiguous_phys_write(bus, phys, 4)?;
        for (offset, byte) in value.to_le_bytes().into_iter().enumerate() {
            write_probed_phys_u8(cpu, bus, PhysAddr(phys.0 + offset as u64), byte)?;
        }
        return Ok(());
    }

    let p0 = probe_linear_write_u8_at_priv(cpu, bus, linear, priv_)?;
    let p1 = probe_linear_write_u8_at_priv(cpu, bus, LinearAddr(linear.0.wrapping_add(1)), priv_)?;
    let p2 = probe_linear_write_u8_at_priv(cpu, bus, LinearAddr(linear.0.wrapping_add(2)), priv_)?;
    let p3 = probe_linear_write_u8_at_priv(cpu, bus, LinearAddr(linear.0.wrapping_add(3)), priv_)?;

    if p1.0 == p0.0.wrapping_add(1)
        && p2.0 == p0.0.wrapping_add(2)
        && p3.0 == p0.0.wrapping_add(3)
        && contiguous_ram_commit(bus, p0, 4)
    {
        if bus.cached_write_translation_allowed(p0) && bus.cached_write_translation_allowed(p3) {
            return write_probed_cached_ram_u32(cpu, bus, bus.cached_backing_token(p0), value);
        }
        return write_probed_phys_u32(cpu, bus, p0, value);
    }

    write_probed_phys_u8(cpu, bus, p0, value as u8)?;
    write_probed_phys_u8(cpu, bus, p1, (value >> 8) as u8)?;
    write_probed_phys_u8(cpu, bus, p2, (value >> 16) as u8)?;
    write_probed_phys_u8(cpu, bus, p3, (value >> 24) as u8)
}

fn walk_non_pae<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<Translation, Fault> {
    let dir_index = (linear.0 >> 22) & 0x3ff;
    let tab_index = (linear.0 >> 12) & 0x3ff;
    let pde_addr = PhysAddr(u64::from((cpu.cr3 & PAGE_MASK).wrapping_add(dir_index * 4)));
    let mut frames = WalkFrameSet::new();
    note_walk_frame(
        cpu,
        bus,
        &mut frames,
        pde_addr,
        mem::PageTableFrameRole::Directory,
    )?;
    let mut pde = mem::read_phys_u32(bus, pde_addr)?;
    if (pde & PTE_PRESENT) == 0 {
        return page_fault(cpu, linear, access, priv_, false, false);
    }
    if (pde & PDE_PS) != 0 && (cpu.cr4 & CR4_PSE) != 0 {
        return translate_pse_4m(cpu, bus, linear, access, priv_, pde_addr, pde, frames);
    }
    // A present directory entry is not a not-present fault. Check the leaf's
    // presence before combining its permissions with the directory (v86 cpu.rs
    // agrees). Keep the existing permitted early PDE A-bit update below; v86
    // sets PDE A only after permissions pass.
    let mut wrote_accessed = false;
    if (pde & PTE_ACCESSED) == 0 {
        pde |= PTE_ACCESSED;
        wrote_accessed = true;
        mem::write_phys_u32_with_origin(bus, pde_addr, pde, mem::WriteOrigin::PageWalk)?;
    }

    let pte_addr = PhysAddr(u64::from((pde & PAGE_MASK).wrapping_add(tab_index * 4)));
    note_walk_frame(
        cpu,
        bus,
        &mut frames,
        pte_addr,
        mem::PageTableFrameRole::Table,
    )?;
    let mut pte = mem::read_phys_u32(bus, pte_addr)?;
    if (pte & PTE_PRESENT) == 0 {
        return page_fault(cpu, linear, access, priv_, false, false);
    }
    check_perm(cpu, linear, access, priv_, pde & pte, true)?;
    let mut changed = false;
    if (pte & PTE_ACCESSED) == 0 {
        pte |= PTE_ACCESSED;
        wrote_accessed = true;
        changed = true;
    }
    let mut wrote_dirty = false;
    if matches!(access, AccessKind::Write) && (pte & PTE_DIRTY) == 0 {
        pte |= PTE_DIRTY;
        wrote_dirty = true;
        changed = true;
    }
    if changed {
        mem::write_phys_u32_with_origin(bus, pte_addr, pte, mem::WriteOrigin::PageWalk)?;
    }
    let translation = Translation::new(
        PhysAddr(u64::from((pte & PAGE_MASK) | (linear.0 & 0xfff))),
        TranslationPageSize::Size4K,
        frames,
        true,
        (pte & PTE_DIRTY) != 0,
        wrote_accessed,
        wrote_dirty,
    )
    .with_global(cpu.cr4 & CR4_PGE != 0 && pte & (1 << 8) != 0);
    #[cfg(feature = "jit")]
    let translation = {
        let mut translation = translation;
        if cpu.translation_cache.walk_root == 0 {
            if let Some(root_entry) = bus.jit_page_table_source(pde_addr) {
                cpu.translation_cache.walk_root =
                    root_entry.0.wrapping_sub(u64::from(dir_index) * 4);
            }
        }
        if cpu.translation_cache.walk_root != 0 {
            // Capture a backing token only after a successful RAM cache fill.
            translation.recheck = [pde, pte];
        }
        translation
    };
    Ok(translation)
}

fn translate_pse_4m<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
    pde_addr: PhysAddr,
    mut pde: u32,
    frames: WalkFrameSet,
) -> Result<Translation, Fault> {
    if (pde & PDE_4M_PSE36_RESERVED_MASK) != 0 {
        return page_fault(cpu, linear, access, priv_, true, true);
    }
    check_perm(cpu, linear, access, priv_, pde, true)?;

    let mut changed = false;
    let mut wrote_accessed = false;
    if (pde & PTE_ACCESSED) == 0 {
        pde |= PTE_ACCESSED;
        wrote_accessed = true;
        changed = true;
    }
    let mut wrote_dirty = false;
    if matches!(access, AccessKind::Write) && (pde & PTE_DIRTY) == 0 {
        pde |= PTE_DIRTY;
        wrote_dirty = true;
        changed = true;
    }
    if changed {
        mem::write_phys_u32_with_origin(bus, pde_addr, pde, mem::WriteOrigin::PageWalk)?;
    }

    let base =
        u64::from(pde & LARGE_PAGE_MASK) | (u64::from(pde & PDE_4M_PSE36_HIGH_ADDR_MASK) << 19);
    Ok(Translation::new(
        PhysAddr(base | u64::from(linear.0 & LARGE_PAGE_OFFSET_MASK)),
        TranslationPageSize::Size4M,
        frames,
        true,
        (pde & PTE_DIRTY) != 0,
        wrote_accessed,
        wrote_dirty,
    )
    .with_global(cpu.cr4 & CR4_PGE != 0 && pde & (1 << 8) != 0))
}

fn walk_pae<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
) -> Result<Translation, Fault> {
    let pdpt_index = (linear.0 >> 30) & 0x3;
    let dir_index = (linear.0 >> 21) & 0x1ff;
    let tab_index = (linear.0 >> 12) & 0x1ff;
    let mut frames = WalkFrameSet::new();

    let pdpt_addr = PhysAddr(u64::from(cpu.cr3 & PAE_CR3_MASK) + u64::from(pdpt_index) * 8);
    note_walk_frame(
        cpu,
        bus,
        &mut frames,
        pdpt_addr,
        mem::PageTableFrameRole::Pdpt,
    )?;
    let pdpte = cpu.pae_pdptrs[pdpt_index as usize];
    if (pdpte & PAE_PTE_PRESENT) == 0 {
        return page_fault(cpu, linear, access, priv_, false, false);
    }
    if (pdpte & PAE_PDPTE_RESERVED_MASK) != 0 {
        return page_fault(cpu, linear, access, priv_, true, true);
    }

    let pde_addr = PhysAddr((pdpte & PAE_4K_PAGE_MASK) + u64::from(dir_index) * 8);
    note_walk_frame(
        cpu,
        bus,
        &mut frames,
        pde_addr,
        mem::PageTableFrameRole::Directory,
    )?;
    let mut pde = mem::read_phys_u64(bus, pde_addr)?;
    if (pde & PAE_PTE_PRESENT) == 0 {
        return page_fault(cpu, linear, access, priv_, false, false);
    }
    if (pde & PAE_PDE_PS) != 0 {
        return translate_pae_2m(cpu, bus, linear, access, priv_, pde_addr, pde, frames);
    }
    if (pde & PAE_ENTRY_RESERVED_MASK) != 0 {
        return page_fault(cpu, linear, access, priv_, true, true);
    }

    let pte_addr = PhysAddr((pde & PAE_4K_PAGE_MASK) + u64::from(tab_index) * 8);
    note_walk_frame(
        cpu,
        bus,
        &mut frames,
        pte_addr,
        mem::PageTableFrameRole::Table,
    )?;
    let mut pte = mem::read_phys_u64(bus, pte_addr)?;
    if (pte & PAE_PTE_PRESENT) == 0 {
        return page_fault(cpu, linear, access, priv_, false, false);
    }
    if (pte & PAE_ENTRY_RESERVED_MASK) != 0 {
        return page_fault(cpu, linear, access, priv_, true, true);
    }
    check_perm_pae(cpu, linear, access, priv_, pde & pte, true)?;

    let mut pde_changed = false;
    let mut wrote_accessed = false;
    if (pde & PAE_PTE_ACCESSED) == 0 {
        pde |= PAE_PTE_ACCESSED;
        wrote_accessed = true;
        pde_changed = true;
    }
    let mut pte_changed = false;
    if (pte & PAE_PTE_ACCESSED) == 0 {
        pte |= PAE_PTE_ACCESSED;
        wrote_accessed = true;
        pte_changed = true;
    }
    let mut wrote_dirty = false;
    if matches!(access, AccessKind::Write) && (pte & PAE_PTE_DIRTY) == 0 {
        pte |= PAE_PTE_DIRTY;
        wrote_dirty = true;
        pte_changed = true;
    }
    if pde_changed {
        mem::write_phys_u64_with_origin(bus, pde_addr, pde, mem::WriteOrigin::PageWalk)?;
    }
    if pte_changed {
        mem::write_phys_u64_with_origin(bus, pte_addr, pte, mem::WriteOrigin::PageWalk)?;
    }

    Ok(Translation::new(
        PhysAddr((pte & PAE_4K_PAGE_MASK) | u64::from(linear.0 & 0x0fff)),
        TranslationPageSize::Size4K,
        frames,
        true,
        (pte & PAE_PTE_DIRTY) != 0,
        wrote_accessed,
        wrote_dirty,
    )
    .with_global(cpu.cr4 & CR4_PGE != 0 && pte & (1 << 8) != 0))
}

fn translate_pae_2m<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
    pde_addr: PhysAddr,
    mut pde: u64,
    frames: WalkFrameSet,
) -> Result<Translation, Fault> {
    if (pde & PAE_PDE_2M_RESERVED_MASK) != 0 {
        return page_fault(cpu, linear, access, priv_, true, true);
    }
    check_perm_pae(cpu, linear, access, priv_, pde, true)?;

    let mut changed = false;
    let mut wrote_accessed = false;
    if (pde & PAE_PTE_ACCESSED) == 0 {
        pde |= PAE_PTE_ACCESSED;
        wrote_accessed = true;
        changed = true;
    }
    let mut wrote_dirty = false;
    if matches!(access, AccessKind::Write) && (pde & PAE_PTE_DIRTY) == 0 {
        pde |= PAE_PTE_DIRTY;
        wrote_dirty = true;
        changed = true;
    }
    if changed {
        mem::write_phys_u64_with_origin(bus, pde_addr, pde, mem::WriteOrigin::PageWalk)?;
    }

    Ok(Translation::new(
        PhysAddr((pde & PAE_2M_PAGE_MASK) | (u64::from(linear.0) & PAE_2M_PAGE_OFFSET_MASK)),
        TranslationPageSize::Size2M,
        frames,
        true,
        (pde & PAE_PTE_DIRTY) != 0,
        wrote_accessed,
        wrote_dirty,
    )
    .with_global(cpu.cr4 & CR4_PGE != 0 && pde & (1 << 8) != 0))
}

fn note_walk_frame<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    frames: &mut WalkFrameSet,
    addr: PhysAddr,
    role: mem::PageTableFrameRole,
) -> Result<(), Fault> {
    let frame = mem::PhysFrame::containing(addr);
    if bus.note_page_table_frame(frame, role)? {
        // Discovery changes write tracking, not a mapping or permission. Old
        // writable aliases of this backing frame must now take the tracked
        // write path; unrelated translations and execute links remain valid.
        // The subsequent guest table write still invalidates its dependents.
        cpu.translation_cache
            .revoke_writable_aliases(bus.cached_backing_token(frame.base()).0);
    }
    if !frames.push(frame, role) {
        return Err(Fault::Mem(MemError::OutOfRange { addr: frame.base() }));
    }
    Ok(())
}

fn permits_write(cr0: u32, priv_: AccessPriv, bits: u32) -> bool {
    bits & PTE_WRITE != 0 || (!priv_.is_user() && cr0 & CR0_WP == 0)
}

fn check_perm(
    cpu: &mut Cpu,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
    bits: u32,
    present: bool,
) -> Result<(), Fault> {
    let user = priv_.is_user();
    if user && (bits & PTE_USER) == 0 {
        return page_fault(cpu, linear, access, priv_, present, false);
    }
    if matches!(access, AccessKind::Write) && !permits_write(cpu.cr0, priv_, bits) {
        return page_fault(cpu, linear, access, priv_, present, false);
    }
    Ok(())
}

fn check_perm_pae(
    cpu: &mut Cpu,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
    bits: u64,
    present: bool,
) -> Result<(), Fault> {
    let user = priv_.is_user();
    if user && (bits & PAE_PTE_USER) == 0 {
        return page_fault(cpu, linear, access, priv_, present, false);
    }
    if matches!(access, AccessKind::Write) && (bits & PAE_PTE_WRITE) == 0 {
        let supervisor_wp = !user && (cpu.cr0 & CR0_WP) != 0;
        if user || supervisor_wp {
            return page_fault(cpu, linear, access, priv_, present, false);
        }
    }
    Ok(())
}

fn page_fault<T>(
    cpu: &mut Cpu,
    linear: LinearAddr,
    access: AccessKind,
    priv_: AccessPriv,
    present: bool,
    reserved: bool,
) -> Result<T, Fault> {
    cpu.cr2 = linear.0;
    let mut code = 0;
    if present {
        code |= PFEC_PRESENT;
    }
    if matches!(access, AccessKind::Write) {
        code |= PFEC_WRITE;
    }
    if priv_.is_user() {
        code |= PFEC_USER;
    }
    if reserved {
        code |= PFEC_RSVD;
    }
    if matches!(access, AccessKind::Execute) {
        code |= PFEC_INSTR;
    }
    Err(Fault::Page(PageFault { linear, code }))
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x86::cpu::{CR0_PE, CR0_WP, CR4_PAE, CR4_PSE, SegmentReg};
    use crate::x86::cpu::{SEGMENT_FLAG_DEFAULT_BIG, SegmentCache};
    use core::cell::Cell;
    use std::vec::Vec;

    const PD_BASE: usize = 0x1000;
    const PT_BASE: usize = 0x2000;
    const PDPT_BASE: usize = 0x1000;
    const PAE_PD_BASE: usize = 0x2000;
    const PAE_PT_BASE: usize = 0x3000;
    const PSE_PRESENT: u32 = PTE_PRESENT | PDE_PS;
    const PSE_RW_USER: u32 = PSE_PRESENT | PTE_WRITE | PTE_USER;
    const PAE_PRESENT: u64 = PAE_PTE_PRESENT;
    const PAE_RW_USER: u64 = PAE_PTE_PRESENT | PAE_PTE_WRITE | PAE_PTE_USER;

    struct Ram {
        data: Vec<u8>,
    }

    impl Ram {
        fn new() -> Self {
            Self::with_size(0x7000)
        }

        fn with_size(size: usize) -> Self {
            Self {
                data: std::vec![0; size],
            }
        }

        fn write_u32(&mut self, addr: usize, value: u32) {
            self.data[addr..addr + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn read_u32(&self, addr: usize) -> u32 {
            u32::from_le_bytes(self.data[addr..addr + 4].try_into().unwrap())
        }

        fn write_u64(&mut self, addr: usize, value: u64) {
            self.data[addr..addr + 8].copy_from_slice(&value.to_le_bytes());
        }

        fn read_u64(&self, addr: usize) -> u64 {
            u64::from_le_bytes(self.data[addr..addr + 8].try_into().unwrap())
        }
    }

    // 36-bit physical addresses must not wrap into RAM on 32-bit hosts.
    fn ram_index(addr: PhysAddr) -> usize {
        usize::try_from(addr.0).unwrap_or(usize::MAX)
    }

    impl Bus for Ram {
        fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
            self.data
                .get(ram_index(addr))
                .copied()
                .ok_or(MemError::OutOfRange { addr })
        }

        fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
            self.data
                .get(ram_index(addr))
                .map(|_| ())
                .ok_or(MemError::OutOfRange { addr })
        }

        fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
            let byte = self
                .data
                .get_mut(ram_index(addr))
                .ok_or(MemError::OutOfRange { addr })?;
            *byte = value;
            Ok(())
        }
    }

    struct NativeReadBus {
        ram: Ram,
        native_u8: Option<u8>,
        native_u16: Option<u16>,
        native_u32: Option<u32>,
        native_backing: Option<BackingToken>,
        native_page_size: TranslationPageSize,
        native_u8_calls: u32,
        native_u16_calls: u32,
        native_u32_calls: u32,
        phys_reads: u32,
        translations: u32,
    }

    impl NativeReadBus {
        fn new() -> Self {
            Self {
                ram: Ram::new(),
                native_u8: None,
                native_u16: None,
                native_u32: None,
                native_backing: None,
                native_page_size: TranslationPageSize::Size4K,
                native_u8_calls: 0,
                native_u16_calls: 0,
                native_u32_calls: 0,
                phys_reads: 0,
                translations: 0,
            }
        }
    }

    impl Bus for NativeReadBus {
        fn native_read_backing(
            &mut self,
            _linear: LinearAddr,
            _priv_: AccessPriv,
        ) -> Option<(BackingToken, TranslationPageSize)> {
            self.native_backing
                .map(|token| (token, self.native_page_size))
        }

        fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
            self.phys_reads += 1;
            self.ram.read_phys_u8(addr)
        }

        fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
            self.ram.probe_phys_write_u8(addr)
        }

        fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
            self.ram.write_phys_u8(addr, value)
        }

        fn linear_translation_cache_enabled(&self) -> bool {
            true
        }

        fn after_linear_translate(
            &mut self,
            _: LinearAddr,
            _: AccessKind,
            _: AccessPriv,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: Translation,
        ) {
            self.translations += 1;
        }

        fn try_native_read_linear_u8(
            &mut self,
            linear: LinearAddr,
            access: AccessKind,
            priv_: AccessPriv,
            cr0: u32,
            cr3: u32,
            cr4: u32,
            eflags: u32,
        ) -> NativeRead<u8> {
            let _ = (linear, access, priv_, cr0, cr3, cr4, eflags);
            self.native_u8_calls += 1;
            self.native_u8.map_or(NativeRead::Miss, NativeRead::Hit)
        }

        fn try_native_read_linear_u16(
            &mut self,
            linear: LinearAddr,
            access: AccessKind,
            priv_: AccessPriv,
            cr0: u32,
            cr3: u32,
            cr4: u32,
            eflags: u32,
        ) -> NativeRead<u16> {
            let _ = (linear, access, priv_, cr0, cr3, cr4, eflags);
            self.native_u16_calls += 1;
            self.native_u16.map_or(NativeRead::Miss, NativeRead::Hit)
        }

        fn try_native_read_linear_u32(
            &mut self,
            linear: LinearAddr,
            access: AccessKind,
            priv_: AccessPriv,
            cr0: u32,
            cr3: u32,
            cr4: u32,
            eflags: u32,
        ) -> NativeRead<u32> {
            let _ = (linear, access, priv_, cr0, cr3, cr4, eflags);
            self.native_u32_calls += 1;
            self.native_u32.map_or(NativeRead::Miss, NativeRead::Hit)
        }
    }

    struct CacheBus {
        ram: Ram,
        frame_roles: Vec<mem::FrameRoleFlags>,
        translations: u32,
        phys_reads: u32,
        mmio_page: Option<u64>,
        translation_flush_checks: Cell<u32>,
        probe_fault_addr: Option<u64>,
        writes: Vec<(u64, u8)>,
    }

    impl CacheBus {
        fn new() -> Self {
            let ram = Ram::with_size(0x8000);
            let frames = (ram.data.len() + 4095) / 4096;
            Self {
                ram,
                frame_roles: std::vec![mem::FrameRoleFlags::RAM; frames],
                translations: 0,
                phys_reads: 0,
                mmio_page: None,
                translation_flush_checks: Cell::new(0),
                probe_fault_addr: None,
                writes: Vec::new(),
            }
        }

        fn frame_index(&self, frame: mem::PhysFrame) -> Option<usize> {
            let index = usize::try_from(frame.0).ok()?;
            if index < self.frame_roles.len() {
                Some(index)
            } else {
                None
            }
        }

        fn write_touches_page_table_frame(&self, write: mem::PhysWrite) -> bool {
            if write.len == 0 || matches!(write.origin, mem::WriteOrigin::PageWalk) {
                return false;
            }
            let start = mem::PhysFrame::containing(write.addr);
            let Some(end_addr) = write.addr.0.checked_add(u64::from(write.len) - 1) else {
                return true;
            };
            let end = mem::PhysFrame::containing(PhysAddr(end_addr));
            let mut frame = start.0;
            while frame <= end.0 {
                if let Some(index) = self.frame_index(mem::PhysFrame(frame)) {
                    if self.frame_roles[index].is_page_table() {
                        return true;
                    }
                }
                frame += 1;
            }
            false
        }
    }

    impl Bus for CacheBus {
        fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
            self.phys_reads += 1;
            self.ram.read_phys_u8(addr)
        }

        fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
            if self.probe_fault_addr == Some(addr.0) {
                return Err(MemError::OutOfRange { addr });
            }
            self.ram.probe_phys_write_u8(addr)
        }

        fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
            self.writes.push((addr.0, value));
            self.ram.write_phys_u8(addr, value)
        }

        fn classify_phys(&self, addr: PhysAddr, len: u32) -> mem::PhysRegionKind {
            if len == 0 {
                return mem::PhysRegionKind::Hole;
            }
            let Some(end) = addr.0.checked_add(u64::from(len) - 1) else {
                return mem::PhysRegionKind::Mixed;
            };
            if let Some(mmio_page) = self.mmio_page {
                let mmio_end = mmio_page + 4095;
                if addr.0 >= mmio_page && end <= mmio_end {
                    return mem::PhysRegionKind::Mmio;
                }
                if addr.0 <= mmio_end && end >= mmio_page {
                    return mem::PhysRegionKind::Mixed;
                }
            }
            if end < self.ram.data.len() as u64 {
                mem::PhysRegionKind::Ram
            } else {
                mem::PhysRegionKind::Hole
            }
        }

        fn phys_ram_repeat_write_supported(&self, addr: PhysAddr, len: u32) -> bool {
            matches!(self.classify_phys(addr, len), mem::PhysRegionKind::Ram)
        }

        fn write_phys_ram_repeat(
            &mut self,
            addr: PhysAddr,
            width: u8,
            value: u32,
            units: u32,
        ) -> Result<(), MemError> {
            let bytes = value.to_le_bytes();
            for unit in 0..units {
                for index in 0..width {
                    let offset = u64::from(unit) * u64::from(width) + u64::from(index);
                    self.ram
                        .write_phys_u8(PhysAddr(addr.0 + offset), bytes[index as usize])?;
                }
            }
            Ok(())
        }

        fn note_page_table_frame(
            &mut self,
            frame: mem::PhysFrame,
            role: mem::PageTableFrameRole,
        ) -> Result<bool, MemError> {
            let Some(index) = self.frame_index(frame) else {
                return Err(MemError::OutOfRange { addr: frame.base() });
            };
            let was_page_table = self.frame_roles[index].is_page_table();
            self.frame_roles[index].insert(mem::FrameRoleFlags::from_page_table_role(role));
            Ok(!was_page_table)
        }

        fn linear_translation_cache_enabled(&self) -> bool {
            true
        }

        fn translation_cache_flush_after_phys_write(&self, write: mem::PhysWrite) -> bool {
            self.translation_flush_checks
                .set(self.translation_flush_checks.get().wrapping_add(1));
            self.write_touches_page_table_frame(write)
        }

        fn cached_ram_write_needs_dynamic_translation_flush(&self) -> bool {
            false
        }

        fn cached_write_translation_allowed(&self, phys: PhysAddr) -> bool {
            self.frame_index(mem::PhysFrame::containing(phys))
                .is_some_and(|index| !self.frame_roles[index].is_page_table())
        }

        fn after_linear_translate(
            &mut self,
            linear: LinearAddr,
            access: AccessKind,
            priv_: AccessPriv,
            cr0: u32,
            cr3: u32,
            cr4: u32,
            eflags: u32,
            translation: Translation,
        ) {
            let _ = (linear, access, priv_, cr0, cr3, cr4, eflags, translation);
            self.translations += 1;
        }
    }

    fn paging_cpu() -> Cpu {
        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = PD_BASE as u32;
        cpu
    }

    fn pse_cpu() -> Cpu {
        let mut cpu = paging_cpu();
        cpu.cr4 = CR4_PSE;
        cpu
    }

    fn pae_cpu() -> Cpu {
        let mut cpu = paging_cpu();
        cpu.cr3 = PDPT_BASE as u32;
        cpu.cr4 = CR4_PAE;
        cpu.pae_pdptrs[0] = PAE_PD_BASE as u64 | PAE_PRESENT;
        cpu
    }

    fn install_pae_4k(ram: &mut Ram, pde_flags: u64, pte_flags: u64, phys: u64) {
        ram.write_u64(PDPT_BASE, PAE_PD_BASE as u64 | PAE_PRESENT);
        ram.write_u64(PAE_PD_BASE, PAE_PT_BASE as u64 | pde_flags);
        ram.write_u64(PAE_PT_BASE, phys | pte_flags);
    }

    fn install_non_pae_4k(ram: &mut Ram, pde_flags: u32, pte_flags: u32, phys: u32) {
        ram.write_u32(PD_BASE, PT_BASE as u32 | pde_flags);
        ram.write_u32(PT_BASE, phys | pte_flags);
    }

    fn expect_page_fault<T>(result: Result<T, Fault>) -> PageFault {
        match result {
            Err(Fault::Page(page)) => page,
            Err(err) => panic!("expected page fault, got {err:?}"),
            Ok(_) => panic!("expected page fault"),
        }
    }

    fn expect_mem_out_of_range<T>(result: Result<T, Fault>) -> PhysAddr {
        match result {
            Err(Fault::Mem(MemError::OutOfRange { addr })) => addr,
            Err(err) => panic!("expected out-of-range memory fault, got {err:?}"),
            Ok(_) => panic!("expected out-of-range memory fault"),
        }
    }

    #[test]
    fn direct_translation_report_has_no_page_table_frames() {
        let mut ram = Ram::new();
        let mut cpu = Cpu::new();

        let translation = translate_detailed(
            &mut cpu,
            &mut ram,
            LinearAddr(0xffff_fffe),
            AccessKind::Write,
            0,
        )
        .unwrap();

        assert_eq!(translation.phys, PhysAddr(0xffff_fffe));
        assert_eq!(translation.page_size, TranslationPageSize::Direct);
        assert_eq!(translation.page_table_frame_count(), 0);
        assert!(translation.all_accessed);
        assert!(translation.leaf_dirty);
        assert!(!translation.wrote_accessed);
        assert!(!translation.wrote_dirty);
    }

    #[test]
    fn native_linear_read_hit_bypasses_reference_path() {
        let mut bus = NativeReadBus::new();
        let mut cpu = Cpu::new();
        bus.native_u8 = Some(0x5a);

        let value =
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x20), AccessKind::Read, 0)
                .unwrap();

        assert_eq!(value, 0x5a);
        assert_eq!(bus.native_u8_calls, 1);
        assert_eq!(bus.phys_reads, 0);
    }

    #[test]
    fn native_linear_read_miss_falls_back_to_reference_path() {
        let mut bus = NativeReadBus::new();
        let mut cpu = Cpu::new();
        bus.ram.data[0x20] = 0xc3;

        let value =
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x20), AccessKind::Read, 0)
                .unwrap();

        assert_eq!(value, 0xc3);
        assert_eq!(bus.native_u8_calls, 1);
        assert_eq!(bus.phys_reads, 1);
    }

    #[test]
    fn native_hit_promotes_read_only_live_backing_and_obeys_cache_invalidation() {
        for width in [1, 2, 4] {
            let mut bus = NativeReadBus::new();
            let mut cpu = Cpu::new();
            bus.native_backing = Some(BackingToken(0x3020));
            bus.native_u8 = Some(0x44);
            bus.native_u16 = Some(0x3344);
            bus.native_u32 = Some(0x1122_3344);
            let linear = LinearAddr(0x1020);
            let read = |cpu: &mut Cpu, bus: &mut NativeReadBus| match width {
                1 => read_linear_u8_at_cpl(cpu, bus, linear, AccessKind::Read, 0).map(u32::from),
                2 => read_linear_u16_at_cpl(cpu, bus, linear, AccessKind::Read, 0).map(u32::from),
                _ => read_linear_u32_at_cpl(cpu, bus, linear, AccessKind::Read, 0),
            };
            let mask = u32::MAX >> ((4 - width) * 8);
            assert_eq!(read(&mut cpu, &mut bus).unwrap(), 0x1122_3344 & mask);
            assert_eq!(
                cpu.cached_read_translation(linear, false),
                bus.native_backing
            );
            assert_eq!(cpu.cached_read_translation(linear, true), None);
            assert_eq!(cpu.cached_write_translation(linear, false), None);
            bus.ram.write_u32(0x3020, 0x5566_7788);
            assert_eq!(read(&mut cpu, &mut bus).unwrap(), 0x5566_7788 & mask);
            assert_eq!(
                bus.native_u8_calls + bus.native_u16_calls + bus.native_u32_calls,
                1
            );
            assert_eq!(bus.translations, 0);
            cpu.invalidate_translation_context();
            assert_eq!(read(&mut cpu, &mut bus).unwrap(), 0x1122_3344 & mask);
            assert_eq!(
                bus.native_u8_calls + bus.native_u16_calls + bus.native_u32_calls,
                2
            );
        }
    }

    #[test]
    fn invlpg_after_native_hit_preserves_small_neighbors_and_retires_large_fragments() {
        for page_size in [
            TranslationPageSize::Size4K,
            TranslationPageSize::Size2M,
            TranslationPageSize::Size4M,
        ] {
            for user in [false, true] {
                let mut bus = NativeReadBus::new();
                let mut cpu = Cpu::new();
                bus.native_backing = Some(BackingToken(0x3020));
                bus.native_page_size = page_size;
                bus.native_u32 = Some(0x1234_5678);
                let linear = LinearAddr(0x1020);
                assert_eq!(
                    read_linear_u32_at_cpl(
                        &mut cpu,
                        &mut bus,
                        linear,
                        AccessKind::Read,
                        if user { 3 } else { 0 },
                    )
                    .unwrap(),
                    0x1234_5678
                );
                for page in [0x1000, 0x4000, 0x405000] {
                    let addr = LinearAddr(page);
                    cpu.fill_fetch_cache(addr, PhysAddr(u64::from(page)));
                    cpu.fill_cached_write_translation(addr, BackingToken(u64::from(page)), user);
                }
                cpu.invalidate_translation_page(linear);
                assert!(cpu.cached_read_translation(linear, user).is_none());
                assert!(cpu.fetch_cache_lookup(linear).is_none());
                let small = page_size == TranslationPageSize::Size4K;
                assert_eq!(
                    cpu.cached_read_translation(LinearAddr(0x4000), user)
                        .is_some(),
                    small
                );
                assert_eq!(cpu.fetch_cache_lookup(LinearAddr(0x4000)).is_some(), small);
                assert!(
                    cpu.cached_read_translation(LinearAddr(0x405000), user)
                        .is_some()
                );
                assert!(cpu.fetch_cache_lookup(LinearAddr(0x405000)).is_some());
            }
        }
    }

    #[test]
    fn software_hits_avoid_native_probes_and_eviction_rewalks_on_native_miss() {
        let mut bus = NativeReadBus::new();
        let mut cpu = Cpu::new();
        bus.ram.write_u32(0x20, 0x1122_3344);
        for expected_walks in 1..=2 {
            assert_eq!(
                read_linear_u32_at_cpl(&mut cpu, &mut bus, LinearAddr(0x20), AccessKind::Read, 0)
                    .unwrap(),
                0x1122_3344
            );
            assert!(
                cpu.cached_read_translation(LinearAddr(0x20), false)
                    .is_some()
            );
            assert_eq!(bus.native_u32_calls, 1);
            assert_eq!(bus.translations, 1);
            let _ = expected_walks;
        }
        cpu.invalidate_translation_context();
        assert_eq!(
            read_linear_u32(&mut cpu, &mut bus, LinearAddr(0x20), AccessKind::Read).unwrap(),
            0x1122_3344
        );
        assert_eq!(bus.native_u32_calls, 2);
        assert_eq!(bus.translations, 2);
    }

    #[test]
    fn native_wide_read_is_not_attempted_across_page_boundary() {
        let mut bus = NativeReadBus::new();
        let mut cpu = Cpu::new();
        bus.ram = Ram::with_size(0x3000);
        bus.ram.data[0xffd] = 0x11;
        bus.ram.data[0xffe] = 0x22;
        bus.ram.data[0xfff] = 0x33;
        bus.ram.data[0x1000] = 0x44;
        bus.native_u32 = Some(0xaabb_ccdd);

        let value =
            read_linear_u32_at_cpl(&mut cpu, &mut bus, LinearAddr(0x0ffd), AccessKind::Read, 0)
                .unwrap();

        assert_eq!(value, 0x4433_2211);
        assert_eq!(bus.native_u32_calls, 0);
        assert_eq!(bus.phys_reads, 4);
    }

    #[test]
    fn linear_read_cache_reuses_authoritative_4k_walks() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
            0x4000,
        );
        bus.ram.data[0x4010] = 0x5a;
        bus.ram.data[0x4011] = 0xa5;
        let mut cpu = paging_cpu();

        let first =
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x10), AccessKind::Read, 3)
                .unwrap();
        let translations_after_first = bus.translations;
        let second =
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x11), AccessKind::Read, 3)
                .unwrap();

        assert_eq!(first, 0x5a);
        assert_eq!(second, 0xa5);
        assert_eq!(translations_after_first, 1);
        assert_eq!(bus.translations, 1);
    }

    #[test]
    fn ordinary_u32_read_uses_compact_cached_hit_path() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            0x4000,
        );
        bus.ram.data[0x4020..0x4028]
            .copy_from_slice(&[0x44, 0x33, 0x22, 0x11, 0xdd, 0xcc, 0xbb, 0xaa]);
        let mut cpu = paging_cpu();

        let first =
            read_linear_u32(&mut cpu, &mut bus, LinearAddr(0x20), AccessKind::Read).unwrap();
        let translations_after_first = bus.translations;
        let second = try_fast_linear_read_u32(&mut cpu, &mut bus, LinearAddr(0x24))
            .unwrap()
            .unwrap();

        assert_eq!(first, 0x1122_3344);
        assert_eq!(second, 0xaabb_ccdd);
        assert_eq!(translations_after_first, 1);
        assert_eq!(bus.translations, 1);
        assert_eq!(cpu.cache_stats.linear_translation_hits, 1);
    }

    #[test]
    fn ordinary_u16_read_uses_compact_cached_hit_path() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            0x4000,
        );
        bus.ram.data[0x4020..0x4024].copy_from_slice(&[0x44, 0x33, 0xdd, 0xcc]);
        let mut cpu = paging_cpu();

        let first =
            read_linear_u16(&mut cpu, &mut bus, LinearAddr(0x20), AccessKind::Read).unwrap();
        let translations_after_first = bus.translations;
        let second = try_cached_linear_read_u16(&mut cpu, &mut bus, LinearAddr(0x22)).unwrap();

        assert_eq!(first, 0x3344);
        assert_eq!(second, 0xccdd);
        assert_eq!(translations_after_first, 1);
        assert_eq!(bus.translations, 1);
        assert_eq!(cpu.cache_stats.linear_translation_hits, 1);
    }

    #[test]
    fn linear_write_cache_reuses_dirty_ram_translation_after_probe() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
            0x4000,
        );
        let mut cpu = paging_cpu();

        write_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x20), 0x11, 3).unwrap();
        let translations_after_first = bus.translations;
        write_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x21), 0x22, 3).unwrap();

        assert_eq!(bus.ram.data[0x4020], 0x11);
        assert_eq!(bus.ram.data[0x4021], 0x22);
        assert_ne!(bus.ram.read_u32(PT_BASE) & PTE_DIRTY, 0);
        assert_eq!(translations_after_first, 1);
        assert_eq!(bus.translations, 1);
        assert_eq!(bus.translation_flush_checks.get(), 0);
    }

    #[test]
    fn ordinary_u32_write_uses_current_privilege_cache_tag() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            0x4000,
        );
        let mut cpu = paging_cpu();

        write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x20), 0x1122_3344).unwrap();
        let translations_after_first = bus.translations;
        write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x24), 0xaabb_ccdd).unwrap();

        assert_eq!(bus.ram.read_u32(0x4020), 0x1122_3344);
        assert_eq!(bus.ram.read_u32(0x4024), 0xaabb_ccdd);
        assert_eq!(translations_after_first, 1);
        assert_eq!(bus.translations, 1);
        assert_eq!(cpu.cache_stats.linear_translation_hits, 1);
    }

    #[test]
    fn linear_translation_cache_reuses_pse_large_page_as_4k_subpage() {
        let mut bus = CacheBus::new();
        bus.ram
            .write_u32(PD_BASE, PSE_RW_USER | PTE_ACCESSED | PTE_DIRTY);
        bus.ram.data[0x123] = 0x31;
        bus.ram.data[0x124] = 0x32;
        let mut cpu = pse_cpu();

        let first =
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x123), AccessKind::Read, 3)
                .unwrap();
        let translations_after_first = bus.translations;
        let second =
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0x124), AccessKind::Read, 3)
                .unwrap();

        assert_eq!(first, 0x31);
        assert_eq!(second, 0x32);
        assert_eq!(translations_after_first, 1);
        assert_eq!(bus.translations, 1);
        assert_eq!(cpu.cache_stats.linear_translation_hits, 1);
    }

    #[test]
    fn page_table_frame_write_flushes_full_translation_context() {
        let mut bus = CacheBus::new();
        let flags = PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED;
        install_non_pae_4k(&mut bus.ram, flags, flags, 0x4000);
        bus.ram.write_u32(PT_BASE + 4, PT_BASE as u32 | flags);
        bus.ram.data[0x4000] = 0x44;
        bus.ram.data[0x5000] = 0x55;
        let mut cpu = paging_cpu();

        assert_eq!(
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read, 3).unwrap(),
            0x44
        );
        assert_eq!(bus.translations, 1);

        cpu.fill_fetch_cache(LinearAddr(0x1000), PhysAddr(0x6000));
        assert_eq!(
            cpu.fetch_cache_phys(LinearAddr(0x1000)),
            Some(PhysAddr(0x6000))
        );

        let new_pte = 0x5000_u32 | flags;
        write_linear_u32_at_cpl(&mut cpu, &mut bus, LinearAddr(0x1000), new_pte, 3).unwrap();

        assert_eq!(
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read, 3).unwrap(),
            0x55
        );
        assert_eq!(bus.translations, 3);
        assert_eq!(cpu.fetch_cache_phys(LinearAddr(0x1000)), None);
        assert_eq!(bus.translation_flush_checks.get(), 1);
    }

    #[test]
    fn page_table_role_promotion_revokes_writes_but_keeps_unaffected_mappings() {
        let mut bus = CacheBus::new();
        let mut cpu = Cpu::new();
        let linear = LinearAddr(0x8000);
        let phys = PhysAddr(0x4000);
        cpu.fill_cached_write_translation(linear, BackingToken(phys.0), false);
        cpu.fill_fetch_cache(LinearAddr(0x1000), PhysAddr(0x6000));
        let mut frames = WalkFrameSet::new();

        note_walk_frame(
            &mut cpu,
            &mut bus,
            &mut frames,
            phys,
            mem::PageTableFrameRole::Table,
        )
        .unwrap();

        assert_eq!(cpu.cached_write_translation(linear, false), None);
        assert_eq!(
            cpu.fetch_cache_phys(LinearAddr(0x1000)),
            Some(PhysAddr(0x6000))
        );
        assert_eq!(
            cpu.cached_current_read_translation(linear),
            Some(BackingToken(phys.0))
        );
        assert!(!bus.cached_write_translation_allowed(phys));
    }

    #[test]
    fn wide_page_table_write_probes_before_commit() {
        let mut bus = CacheBus::new();
        let flags = PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED;
        install_non_pae_4k(&mut bus.ram, flags, flags, 0x4000);
        bus.ram.write_u32(PT_BASE + 4, PT_BASE as u32 | flags);
        let original_pte = bus.ram.read_u32(PT_BASE);
        bus.probe_fault_addr = Some((PT_BASE + 2) as u64);
        let mut cpu = paging_cpu();

        let result =
            write_linear_u32_at_cpl(&mut cpu, &mut bus, LinearAddr(0x1000), 0x5000 | flags, 3);

        assert_eq!(
            expect_mem_out_of_range(result),
            PhysAddr((PT_BASE + 2) as u64)
        );
        assert_eq!(bus.ram.read_u32(PT_BASE), original_pte);
        assert_eq!(bus.translation_flush_checks.get(), 0);
    }

    #[test]
    fn bulk_page_table_frame_write_flushes_full_translation_context() {
        let mut bus = CacheBus::new();
        bus.note_page_table_frame(mem::PhysFrame(4), mem::PageTableFrameRole::Table)
            .unwrap();
        let mut cpu = Cpu::new();
        let cached_linear = LinearAddr(0x8000);
        cpu.fill_cached_read_translation(cached_linear, BackingToken(0x7000), false);
        cpu.fill_fetch_cache(LinearAddr(0x1000), PhysAddr(0x6000));

        assert!(
            write_probed_phys_repeat(&mut cpu, &mut bus, PhysAddr(0x4000), 1, 0xa5, 16).unwrap()
        );

        assert_eq!(cpu.cached_read_translation(cached_linear, false), None);
        assert_eq!(cpu.fetch_cache_phys(LinearAddr(0x1000)), None);
        assert_eq!(&bus.ram.data[0x4000..0x4010], &[0xa5; 16]);
        assert_eq!(bus.translation_flush_checks.get(), 1);
    }

    #[test]
    fn mmio_wide_writes_probe_every_byte_before_ordered_commits() {
        for width in [2u8, 4] {
            for fail in 0..=width {
                let mut cpu = paging_cpu();
                let mut bus = CacheBus::new();
                install_non_pae_4k(&mut bus.ram, 7, 7, 0x5000);
                bus.mmio_page = Some(0x5000);
                bus.ram.data[0x5081..0x5085].fill(0x77);
                if fail < width {
                    bus.probe_fault_addr = Some(0x5081 + u64::from(fail));
                }
                let result = if width == 2 {
                    write_linear_u16_at_cpl(&mut cpu, &mut bus, LinearAddr(0x81), 0x2211, 3)
                } else {
                    write_linear_u32_at_cpl(&mut cpu, &mut bus, LinearAddr(0x81), 0x4433_2211, 3)
                };
                let data_writes: Vec<_> = bus
                    .writes
                    .iter()
                    .copied()
                    .filter(|(at, _)| *at >= 0x5000)
                    .collect();
                if fail == width {
                    assert_eq!(result, Ok(()));
                    assert_eq!(
                        data_writes,
                        (0..width)
                            .map(|i| (0x5081 + u64::from(i), (i + 1) * 0x11))
                            .collect::<Vec<_>>()
                    );
                } else {
                    assert_eq!(
                        result,
                        Err(Fault::Mem(MemError::OutOfRange {
                            addr: PhysAddr(0x5081 + u64::from(fail))
                        }))
                    );
                    assert!(data_writes.is_empty());
                    assert_eq!(&bus.ram.data[0x5081..0x5085], &[0x77; 4]);
                }
                assert!(
                    cpu.cached_write_translation(LinearAddr(0x81), true)
                        .is_none()
                );
                // Translation side effects precede the physical-device probe.
                assert_ne!(bus.ram.read_u32(0x1000) & PTE_ACCESSED, 0);
                assert_eq!(
                    bus.ram.read_u32(0x2000) & (PTE_ACCESSED | PTE_DIRTY),
                    PTE_ACCESSED | PTE_DIRTY
                );
            }
        }
    }

    #[test]
    fn non_ram_translation_targets_are_not_cached() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_USER | PTE_ACCESSED,
            0x5000,
        );
        bus.mmio_page = Some(0x5000);
        bus.ram.data[0x5000] = 0x77;
        let mut cpu = paging_cpu();

        assert_eq!(
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read, 3).unwrap(),
            0x77
        );
        assert_eq!(
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read, 3).unwrap(),
            0x77
        );

        assert_eq!(bus.translations, 2);
    }

    #[test]
    fn linear_translation_cache_keeps_user_and_supervisor_permissions_separate() {
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            0x4000,
        );
        bus.ram.data[0x4000] = 0x33;
        let mut cpu = paging_cpu();

        assert_eq!(
            read_linear_u8_at_cpl(&mut cpu, &mut bus, LinearAddr(0), AccessKind::Read, 0).unwrap(),
            0x33
        );
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut bus,
            LinearAddr(0),
            AccessKind::Read,
            3,
        ));

        assert_eq!(page.linear, LinearAddr(0));
        assert_ne!(page.code & PFEC_USER, 0);
        assert_eq!(bus.translations, 1);
    }

    #[test]
    fn retained_kernel_translation_cannot_bypass_user_page_permissions() {
        use crate::x86::cpu::{SegmentCache, SegmentReg};
        let mut bus = CacheBus::new();
        install_non_pae_4k(
            &mut bus.ram,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED,
            PTE_PRESENT | PTE_WRITE | PTE_ACCESSED | PTE_DIRTY,
            0x4000,
        );
        let mut cpu = paging_cpu();
        let kernel = SegmentCache::flat(8, 0xc09b);
        cpu.set_seg_cache(SegmentReg::Cs, kernel);
        let linear = LinearAddr(0x20);
        write_linear_u32(&mut cpu, &mut bus, linear, 0x1234_5678).unwrap();
        let translations = bus.translations;
        cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(0x1b, 0xc0fb));
        let read_fault = expect_page_fault(read_linear_u32(
            &mut cpu,
            &mut bus,
            linear,
            AccessKind::Read,
        ));
        assert_eq!(read_fault.code, PFEC_PRESENT | PFEC_USER);
        let write_fault =
            expect_page_fault(write_linear_u32(&mut cpu, &mut bus, linear, 0xaabb_ccdd));
        assert_eq!(write_fault.code, PFEC_PRESENT | PFEC_USER | PFEC_WRITE);
        assert_eq!(cpu.cr2, linear.0);
        assert_eq!(bus.ram.read_u32(0x4020), 0x1234_5678);
        cpu.set_seg_cache(SegmentReg::Cs, kernel);
        write_linear_u32(&mut cpu, &mut bus, linear, 0x5678_9abc).unwrap();
        assert_eq!(bus.translations, translations);
        assert_eq!(bus.ram.read_u32(0x4020), 0x5678_9abc);
    }

    #[test]
    fn non_pae_directory_permissions_report_protection_after_leaf_presence() {
        for user in [false, true] {
            for present in [false, true] {
                for write in [false, true] {
                    if !user && !write {
                        continue;
                    }
                    let mut bus = CacheBus::new();
                    // User reads fail U/S; supervisor writes fail R/W with WP.
                    // Preset A/D so this checks defined fault priority/error
                    // codes without imposing a microarchitecture's A policy.
                    install_non_pae_4k(
                        &mut bus.ram,
                        PTE_PRESENT | PTE_ACCESSED,
                        u32::from(present) | PTE_WRITE | PTE_USER | PTE_ACCESSED | PTE_DIRTY,
                        0x4000,
                    );
                    bus.ram.data[0x4020] = 0x5a;
                    let mut cpu = paging_cpu();
                    cpu.cr0 |= CR0_WP;
                    let linear = LinearAddr(0x20);
                    let cpl = if user { 3 } else { 0 };
                    let fault = if write {
                        expect_page_fault(write_linear_u8_at_cpl(
                            &mut cpu, &mut bus, linear, 0xa5, cpl,
                        ))
                    } else {
                        expect_page_fault(read_linear_u8_at_cpl(
                            &mut cpu,
                            &mut bus,
                            linear,
                            AccessKind::Read,
                            cpl,
                        ))
                    };
                    assert_eq!(
                        fault.code,
                        u32::from(present) | (u32::from(write) << 1) | (u32::from(user) << 2)
                    );
                    assert_eq!(fault.linear, linear);
                    assert_eq!(cpu.cr2, linear.0);
                    assert_eq!(bus.ram.data[0x4020], 0x5a);
                    assert_eq!(bus.translations, 0);
                }
            }
        }
    }

    #[test]
    fn non_pae_4k_detailed_walk_reports_frames_and_ad_state() {
        let mut ram = Ram::new();
        ram.write_u32(PD_BASE, PT_BASE as u32 | PTE_PRESENT | PTE_WRITE | PTE_USER);
        ram.write_u32(PT_BASE + 4, 0x4000 | PTE_PRESENT | PTE_WRITE | PTE_USER);

        let mut cpu = paging_cpu();
        let translation = translate_detailed(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_1000),
            AccessKind::Write,
            3,
        )
        .unwrap();

        assert_eq!(translation.phys, PhysAddr(0x4000));
        assert_eq!(translation.page_size, TranslationPageSize::Size4K);
        assert_eq!(translation.page_size.bytes(), 4096);
        assert_eq!(translation.page_table_frame_count(), 2);
        assert_eq!(
            translation.page_table_frame(0),
            Some(WalkFrame::new(
                mem::PhysFrame(1),
                mem::PageTableFrameRole::Directory
            ))
        );
        assert_eq!(
            translation.page_table_frame(1),
            Some(WalkFrame::new(
                mem::PhysFrame(2),
                mem::PageTableFrameRole::Table
            ))
        );
        assert!(translation.all_accessed);
        assert!(translation.leaf_dirty);
        assert!(translation.wrote_accessed);
        assert!(translation.wrote_dirty);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_ACCESSED, 0);
        assert_eq!(ram.read_u32(PD_BASE) & PTE_DIRTY, 0);
        assert_eq!(
            ram.read_u32(PT_BASE + 4) & (PTE_ACCESSED | PTE_DIRTY),
            PTE_ACCESSED | PTE_DIRTY
        );
    }

    #[test]
    fn non_pae_4m_detailed_walk_reports_directory_leaf() {
        let mut ram = Ram::new();
        ram.write_u32(
            PD_BASE + 4,
            0x0040_0000 | PSE_RW_USER | PTE_ACCESSED | PTE_DIRTY,
        );

        let mut cpu = pse_cpu();
        let translation = translate_detailed(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0123),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(translation.phys, PhysAddr(0x0040_0123));
        assert_eq!(translation.page_size, TranslationPageSize::Size4M);
        assert_eq!(translation.page_size.bytes(), 4 * 1024 * 1024);
        assert_eq!(translation.page_table_frame_count(), 1);
        assert_eq!(
            translation.page_table_frame(0),
            Some(WalkFrame::new(
                mem::PhysFrame(1),
                mem::PageTableFrameRole::Directory
            ))
        );
        assert!(translation.all_accessed);
        assert!(translation.leaf_dirty);
        assert!(!translation.wrote_accessed);
        assert!(!translation.wrote_dirty);
    }

    #[test]
    fn pae_4k_detailed_walk_reports_pdpt_directory_and_table_frames() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_RW_USER, PAE_RW_USER, 0x4000);

        let mut cpu = pae_cpu();
        let translation = translate_detailed(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0000),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(translation.phys, PhysAddr(0x4000));
        assert_eq!(translation.page_size, TranslationPageSize::Size4K);
        assert_eq!(translation.page_table_frame_count(), 3);
        assert_eq!(
            translation.page_table_frame(0),
            Some(WalkFrame::new(
                mem::PhysFrame(1),
                mem::PageTableFrameRole::Pdpt
            ))
        );
        assert_eq!(
            translation.page_table_frame(1),
            Some(WalkFrame::new(
                mem::PhysFrame(2),
                mem::PageTableFrameRole::Directory
            ))
        );
        assert_eq!(
            translation.page_table_frame(2),
            Some(WalkFrame::new(
                mem::PhysFrame(3),
                mem::PageTableFrameRole::Table
            ))
        );
        assert!(translation.all_accessed);
        assert!(!translation.leaf_dirty);
        assert!(translation.wrote_accessed);
        assert!(!translation.wrote_dirty);
    }

    #[test]
    fn pae_2m_detailed_walk_reports_directory_leaf_and_dirty_update() {
        let mut ram = Ram::with_size(0x0060_0000);
        ram.write_u64(PDPT_BASE, PAE_PD_BASE as u64 | PAE_PRESENT);
        ram.write_u64(PAE_PD_BASE, 0x0040_0000 | PAE_RW_USER | PAE_PDE_PS);

        let mut cpu = pae_cpu();
        let translation = translate_detailed(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0100),
            AccessKind::Write,
            3,
        )
        .unwrap();

        assert_eq!(translation.phys, PhysAddr(0x0040_0100));
        assert_eq!(translation.page_size, TranslationPageSize::Size2M);
        assert_eq!(translation.page_size.bytes(), 2 * 1024 * 1024);
        assert_eq!(translation.page_table_frame_count(), 2);
        assert_eq!(
            translation.page_table_frame(0),
            Some(WalkFrame::new(
                mem::PhysFrame(1),
                mem::PageTableFrameRole::Pdpt
            ))
        );
        assert_eq!(
            translation.page_table_frame(1),
            Some(WalkFrame::new(
                mem::PhysFrame(2),
                mem::PageTableFrameRole::Directory
            ))
        );
        assert!(translation.all_accessed);
        assert!(translation.leaf_dirty);
        assert!(translation.wrote_accessed);
        assert!(translation.wrote_dirty);
        assert_eq!(
            ram.read_u64(PAE_PD_BASE) & (PAE_PTE_ACCESSED | PAE_PTE_DIRTY),
            PAE_PTE_ACCESSED | PAE_PTE_DIRTY
        );
    }

    #[test]
    fn pae_pdptr_validation_rejects_present_reserved_entries_only() {
        let mut ram = Ram::new();
        ram.write_u64(PDPT_BASE, PAE_PD_BASE as u64 | PAE_PRESENT);
        ram.write_u64(PDPT_BASE + 8, 1u64 << 36);
        ram.write_u64(PDPT_BASE + 16, (1u64 << 36) | PAE_PRESENT);

        assert_eq!(
            validate_legacy_pae_pdptrs(&mut ram, PDPT_BASE as u32),
            Ok(false)
        );

        ram.write_u64(PDPT_BASE + 16, 0);
        assert_eq!(
            validate_legacy_pae_pdptrs(&mut ram, PDPT_BASE as u32),
            Ok(true)
        );
    }

    #[test]
    fn pae_4k_read_translates_three_level_walk_and_sets_accessed_bits() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_RW_USER, PAE_RW_USER, 0x4000);
        ram.data[0x4234] = 0x5a;

        let mut cpu = pae_cpu();
        let value = read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0234),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(value, 0x5a);
        assert_eq!(ram.read_u64(PDPT_BASE) & PAE_PTE_ACCESSED, 0);
        assert_ne!(ram.read_u64(PAE_PD_BASE) & PAE_PTE_ACCESSED, 0);
        assert_eq!(ram.read_u64(PAE_PD_BASE) & PAE_PTE_DIRTY, 0);
        assert_ne!(ram.read_u64(PAE_PT_BASE) & PAE_PTE_ACCESSED, 0);
        assert_eq!(ram.read_u64(PAE_PT_BASE) & PAE_PTE_DIRTY, 0);
    }

    #[test]
    fn pae_4k_write_sets_pde_accessed_and_pte_accessed_dirty() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_RW_USER, PAE_RW_USER, 0x4000);

        let mut cpu = pae_cpu();
        write_linear_u8_at_cpl(&mut cpu, &mut ram, LinearAddr(0x0000_0007), 0xc7, 3).unwrap();

        assert_eq!(ram.data[0x4007], 0xc7);
        assert_ne!(ram.read_u64(PAE_PD_BASE) & PAE_PTE_ACCESSED, 0);
        assert_eq!(ram.read_u64(PAE_PD_BASE) & PAE_PTE_DIRTY, 0);
        assert_ne!(
            ram.read_u64(PAE_PT_BASE) & (PAE_PTE_ACCESSED | PAE_PTE_DIRTY),
            0
        );
        assert_eq!(
            ram.read_u64(PAE_PT_BASE) & (PAE_PTE_ACCESSED | PAE_PTE_DIRTY),
            PAE_PTE_ACCESSED | PAE_PTE_DIRTY
        );
    }

    #[test]
    fn pae_4k_user_permission_fault_does_not_update_ad_bits() {
        let mut ram = Ram::new();
        install_pae_4k(
            &mut ram,
            PAE_PTE_PRESENT | PAE_PTE_WRITE,
            PAE_RW_USER,
            0x4000,
        );

        let mut cpu = pae_cpu();
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0100),
            AccessKind::Read,
            3,
        ));

        assert_eq!(page.linear.0, 0x0000_0100);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_USER),
            PFEC_PRESENT | PFEC_USER
        );
        assert_eq!(page.code & (PFEC_WRITE | PFEC_RSVD | PFEC_INSTR), 0);
        assert_eq!(ram.read_u64(PAE_PD_BASE) & PAE_PTE_ACCESSED, 0);
        assert_eq!(
            ram.read_u64(PAE_PT_BASE) & (PAE_PTE_ACCESSED | PAE_PTE_DIRTY),
            0
        );
    }

    #[test]
    fn pae_4k_supervisor_write_obeys_cr0_wp() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_PRESENT, PAE_PRESENT, 0x4000);

        let mut cpu = pae_cpu();
        cpu.cr0 |= CR0_WP;
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0008),
            0x88,
            0,
        ));

        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE),
            PFEC_PRESENT | PFEC_WRITE
        );
        assert_eq!(page.code & PFEC_USER, 0);
        assert_eq!(ram.data[0x4008], 0);
        assert_eq!(ram.read_u64(PAE_PT_BASE) & PAE_PTE_DIRTY, 0);
    }

    #[test]
    fn pae_4k_supervisor_write_sets_dirty_when_wp_clear() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_PRESENT, PAE_PRESENT, 0x4000);

        let mut cpu = pae_cpu();
        write_linear_u8_at_cpl(&mut cpu, &mut ram, LinearAddr(0x0000_0008), 0x71, 0).unwrap();

        assert_eq!(ram.data[0x4008], 0x71);
        assert_ne!(ram.read_u64(PAE_PT_BASE) & PAE_PTE_DIRTY, 0);
    }

    #[test]
    fn pae_4k_reserved_pdpte_bits_fault_with_present_and_rsvd_bits() {
        let mut ram = Ram::new();

        let mut cpu = pae_cpu();
        cpu.pae_pdptrs[0] = PAE_PRESENT | (1u64 << 36);
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0000),
            AccessKind::Read,
            3,
        ));

        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_USER | PFEC_RSVD),
            PFEC_PRESENT | PFEC_USER | PFEC_RSVD
        );
    }

    #[test]
    fn pae_4k_not_present_pdpte_ignores_reserved_address_bits() {
        let mut ram = Ram::new();

        let mut cpu = pae_cpu();
        cpu.pae_pdptrs[0] = 1u64 << 36;
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0000),
            AccessKind::Read,
            3,
        ));

        assert_eq!(page.code & PFEC_PRESENT, 0);
        assert_eq!(page.code & PFEC_RSVD, 0);
        assert_eq!(page.code & PFEC_USER, PFEC_USER);
    }

    #[test]
    fn pae_walk_uses_cached_pdptr_until_control_register_reload() {
        let mut ram = Ram::with_size(0x8000);
        install_pae_4k(&mut ram, PAE_RW_USER, PAE_RW_USER, 0x4000);
        ram.write_u64(PDPT_BASE, 0x5000 | PAE_PRESENT);
        ram.write_u64(0x5000, 0x6000 | PAE_RW_USER);
        ram.write_u64(0x6000, 0x7000 | PAE_RW_USER);
        ram.data[0x4000] = 0xaa;
        ram.data[0x7000] = 0xbb;

        let mut cpu = pae_cpu();
        let value =
            read_linear_u8_at_cpl(&mut cpu, &mut ram, LinearAddr(0), AccessKind::Read, 3).unwrap();

        assert_eq!(value, 0xaa);
    }

    #[test]
    fn pae_4k_reserved_pte_high_bits_fault_before_ad_update() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_RW_USER, PAE_RW_USER | (1u64 << 36), 0x4000);

        let mut cpu = pae_cpu();
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0001),
            0x44,
            3,
        ));

        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE | PFEC_USER | PFEC_RSVD),
            PFEC_PRESENT | PFEC_WRITE | PFEC_USER | PFEC_RSVD
        );
        assert_eq!(ram.data[0x4001], 0);
        assert_eq!(ram.read_u64(PAE_PD_BASE) & PAE_PTE_ACCESSED, 0);
        assert_eq!(
            ram.read_u64(PAE_PT_BASE) & (PAE_PTE_ACCESSED | PAE_PTE_DIRTY),
            0
        );
    }

    #[test]
    fn pae_2m_page_works_without_cr4_pse_and_sets_directory_leaf_ad_bits() {
        let mut ram = Ram::with_size(0x0060_0000);
        ram.write_u64(PDPT_BASE, PAE_PD_BASE as u64 | PAE_PRESENT);
        ram.write_u64(PAE_PD_BASE + 2 * 8, 0x0040_0000 | PAE_RW_USER | PAE_PDE_PS);
        ram.data[0x0040_1234] = 0x9c;

        let mut cpu = pae_cpu();
        assert_eq!(cpu.cr4 & CR4_PSE, 0);
        let value = read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_1234),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(value, 0x9c);
        assert_ne!(ram.read_u64(PAE_PD_BASE + 2 * 8) & PAE_PTE_ACCESSED, 0);
        assert_eq!(ram.read_u64(PAE_PD_BASE + 2 * 8) & PAE_PTE_DIRTY, 0);
    }

    #[test]
    fn pae_2m_reserved_low_address_bits_fault_before_ad_update() {
        let mut ram = Ram::with_size(0x0060_0000);
        ram.write_u64(PDPT_BASE, PAE_PD_BASE as u64 | PAE_PRESENT);
        ram.write_u64(
            PAE_PD_BASE,
            0x0040_0000 | PAE_RW_USER | PAE_PDE_PS | (1 << 13),
        );

        let mut cpu = pae_cpu();
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0100),
            0x12,
            3,
        ));

        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE | PFEC_USER | PFEC_RSVD),
            PFEC_PRESENT | PFEC_WRITE | PFEC_USER | PFEC_RSVD
        );
        assert_eq!(
            ram.read_u64(PAE_PD_BASE) & (PAE_PTE_ACCESSED | PAE_PTE_DIRTY),
            0
        );
    }

    #[test]
    fn pae_36bit_physical_address_is_not_truncated_to_32_bits() {
        let mut ram = Ram::new();
        install_pae_4k(&mut ram, PAE_RW_USER, PAE_RW_USER, 0x0000_0001_0000_0000);

        let mut cpu = pae_cpu();
        let addr = expect_mem_out_of_range(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0),
            AccessKind::Read,
            3,
        ));

        assert_eq!(addr.0, 0x0000_0001_0000_0000);
    }

    #[test]
    fn pae_cross_page_write_fault_does_not_partially_commit() {
        let mut ram = Ram::new();
        ram.write_u64(PDPT_BASE, PAE_PD_BASE as u64 | PAE_PRESENT);
        ram.write_u64(PAE_PD_BASE, PAE_PT_BASE as u64 | PAE_RW_USER);
        ram.write_u64(PAE_PT_BASE, 0x4000 | PAE_RW_USER);
        ram.write_u64(PAE_PT_BASE + 8, 0x5000 | PAE_PTE_PRESENT | PAE_PTE_USER);
        ram.data[0x4fff] = 0xaa;
        ram.data[0x5000] = 0xbb;

        let mut cpu = pae_cpu();
        let page = expect_page_fault(write_linear_u16_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0fff),
            0xcc55,
            3,
        ));

        assert_eq!(page.linear.0, 0x0000_1000);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE | PFEC_USER),
            PFEC_PRESENT | PFEC_WRITE | PFEC_USER
        );
        assert_eq!(ram.data[0x4fff], 0xaa);
        assert_eq!(ram.data[0x5000], 0xbb);
    }

    #[test]
    fn user_mode_instruction_fault_sets_user_pfec_bit() {
        let mut ram = Ram::new();
        ram.write_u32(0x1004, 0x2000 | PTE_PRESENT | PTE_WRITE | PTE_USER);

        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = 0x1000;
        cpu.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache::flat(0x1b, SEGMENT_FLAG_DEFAULT_BIG),
        );

        let err = read_linear_u8(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0000),
            AccessKind::Execute,
        )
        .unwrap_err();

        let Fault::Page(page) = err else {
            panic!("expected page fault");
        };
        assert_eq!(page.linear.0, 0x0040_0000);
        assert_eq!(page.code & PFEC_PRESENT, 0);
        assert_ne!(page.code & PFEC_USER, 0);
    }

    #[test]
    fn split_rmw_write_fault_precedes_every_data_read() {
        struct ObservedBus {
            ram: Ram,
            data_reads: u32,
        }
        impl Bus for ObservedBus {
            fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
                if addr.0 >= 0x3000 {
                    self.data_reads += 1;
                }
                self.ram.read_phys_u8(addr)
            }
            fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
                self.ram.probe_phys_write_u8(addr)
            }
            fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
                self.ram.write_phys_u8(addr, value)
            }
        }
        let mut bus = ObservedBus {
            ram: Ram::new(),
            data_reads: 0,
        };
        bus.ram.write_u32(0x1000, 0x2003);
        bus.ram.write_u32(0x2000, 0x3003);
        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = 0x1000;
        let fault = expect_page_fault(read_linear_rmw_u32(&mut cpu, &mut bus, LinearAddr(0xffe)));
        assert_eq!((fault.linear.0, fault.code), (0x1000, PFEC_WRITE));
        assert_eq!(bus.data_reads, 0);
    }

    #[test]
    fn split_write_page_fault_does_not_partially_commit() {
        let mut ram = Ram::new();
        ram.write_u32(0x1000, 0x2000 | PTE_PRESENT | PTE_WRITE | PTE_USER);
        ram.write_u32(0x2000, 0x3000 | PTE_PRESENT | PTE_WRITE | PTE_USER);
        ram.data[0x3fff] = 0xaa;

        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = 0x1000;

        let err = write_linear_u16_at_cpl(&mut cpu, &mut ram, LinearAddr(0x0000_0fff), 0xcc55, 3)
            .unwrap_err();

        let Fault::Page(page) = err else {
            panic!("expected page fault");
        };
        assert_eq!(page.linear.0, 0x0000_1000);
        assert_eq!(page.code & (PFEC_WRITE | PFEC_USER), PFEC_WRITE | PFEC_USER);
        assert_eq!(page.code & PFEC_PRESENT, 0);
        assert_eq!(ram.data[0x3fff], 0xaa);
    }

    #[test]
    fn wrapping_write_page_fault_does_not_partially_commit() {
        let mut ram = Ram::new();
        ram.write_u32(
            0x1000 + 1023 * 4,
            0x4000 | PTE_PRESENT | PTE_WRITE | PTE_USER,
        );
        ram.write_u32(
            0x4000 + 1023 * 4,
            0x5000 | PTE_PRESENT | PTE_WRITE | PTE_USER,
        );
        ram.data[0x5fff] = 0xaa;

        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = 0x1000;

        let err =
            write_linear_u32_at_cpl(&mut cpu, &mut ram, LinearAddr(0xffff_ffff), 0x1122_3344, 3)
                .unwrap_err();

        let Fault::Page(page) = err else {
            panic!("expected page fault");
        };
        assert_eq!(page.linear.0, 0);
        assert_eq!(page.code & (PFEC_WRITE | PFEC_USER), PFEC_WRITE | PFEC_USER);
        assert_eq!(page.code & PFEC_PRESENT, 0);
        assert_eq!(ram.data[0x5fff], 0xaa);
    }

    #[test]
    fn split_write_bus_fault_does_not_partially_commit() {
        let mut ram = Ram::new();
        ram.data[0x6ffe] = 0xaa;

        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE;

        let err =
            write_linear_u32_at_cpl(&mut cpu, &mut ram, LinearAddr(0x0000_6ffe), 0x1122_3344, 0)
                .unwrap_err();

        let Fault::Mem(MemError::OutOfRange { addr }) = err else {
            panic!("expected memory fault");
        };
        assert_eq!(addr.0, 0x7000);
        assert_eq!(ram.data[0x6ffe], 0xaa);
    }

    #[test]
    fn pse_4m_read_translates_directory_leaf_with_offset() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER);
        ram.data[0x0040_1234] = 0x5a;

        let mut cpu = pse_cpu();
        let value = read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_1234),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(value, 0x5a);
        let pde = ram.read_u32(PD_BASE + 4);
        assert_ne!(pde & PTE_ACCESSED, 0);
        assert_eq!(pde & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_4m_write_sets_accessed_and_dirty_on_pde_only() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER);
        ram.write_u32(PT_BASE, 0xdead_beef);

        let mut cpu = pse_cpu();
        write_linear_u8_at_cpl(&mut cpu, &mut ram, LinearAddr(0x0040_0100), 0xc3, 3).unwrap();

        assert_eq!(ram.data[0x0040_0100], 0xc3);
        let pde = ram.read_u32(PD_BASE + 4);
        assert_ne!(pde & PTE_ACCESSED, 0);
        assert_ne!(pde & PTE_DIRTY, 0);
        assert_eq!(ram.read_u32(PT_BASE), 0xdead_beef);
    }

    #[test]
    fn pse_4m_read_sets_accessed_not_dirty() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER);
        ram.data[0x0040_2000] = 0x9b;

        let mut cpu = pse_cpu();
        assert_eq!(
            read_linear_u8_at_cpl(
                &mut cpu,
                &mut ram,
                LinearAddr(0x0040_2000),
                AccessKind::Read,
                3,
            ),
            Ok(0x9b)
        );

        let pde = ram.read_u32(PD_BASE + 4);
        assert_ne!(pde & PTE_ACCESSED, 0);
        assert_eq!(pde & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_4m_execute_permission_fault_records_user_access() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_PRESENT | PTE_WRITE);

        let mut cpu = pse_cpu();
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_1000),
            AccessKind::Execute,
            3,
        ));

        assert_eq!(page.linear.0, 0x0040_1000);
        assert_eq!(cpu.cr2, 0x0040_1000);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_USER | PFEC_WRITE),
            PFEC_PRESENT | PFEC_USER
        );
        assert_eq!(ram.read_u32(PD_BASE + 4) & PTE_ACCESSED, 0);
    }

    #[test]
    fn pse_4m_user_permission_fault_sets_present_user_bits_and_cr2() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_PRESENT | PTE_WRITE);

        let mut cpu = pse_cpu();
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_00f0),
            AccessKind::Read,
            3,
        ));

        assert_eq!(page.linear.0, 0x0040_00f0);
        assert_eq!(cpu.cr2, 0x0040_00f0);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_USER),
            PFEC_PRESENT | PFEC_USER
        );
        assert_eq!(page.code & (PFEC_WRITE | PFEC_RSVD | PFEC_INSTR), 0);
        assert_eq!(ram.read_u32(PD_BASE + 4) & (PTE_ACCESSED | PTE_DIRTY), 0);
    }

    #[test]
    fn pse_4m_write_permission_obeys_cr0_wp_for_supervisor() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_PRESENT);

        let mut cpu = pse_cpu();
        cpu.cr0 |= CR0_WP;
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0008),
            0x88,
            0,
        ));

        assert_eq!(page.linear.0, 0x0040_0008);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE),
            PFEC_PRESENT | PFEC_WRITE
        );
        assert_eq!(page.code & PFEC_USER, 0);
        assert_eq!(ram.data[0x0040_0008], 0);
        assert_eq!(ram.read_u32(PD_BASE + 4) & (PTE_ACCESSED | PTE_DIRTY), 0);
    }

    #[test]
    fn pse_4m_supervisor_write_ignores_readonly_when_wp_clear() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_PRESENT);

        let mut cpu = pse_cpu();
        write_linear_u8_at_cpl(&mut cpu, &mut ram, LinearAddr(0x0040_0100), 0x72, 0).unwrap();

        assert_eq!(ram.data[0x0040_0100], 0x72);
        let pde = ram.read_u32(PD_BASE + 4);
        assert_ne!(pde & PTE_ACCESSED, 0);
        assert_ne!(pde & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_4m_user_write_requires_write_bit_even_when_wp_clear() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_PRESENT | PTE_USER);

        let mut cpu = pse_cpu();
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0200),
            0x33,
            3,
        ));

        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE | PFEC_USER),
            PFEC_PRESENT | PFEC_WRITE | PFEC_USER
        );
        assert_eq!(ram.data[0x0040_0200], 0);
        assert_eq!(ram.read_u32(PD_BASE + 4) & (PTE_ACCESSED | PTE_DIRTY), 0);
    }

    #[test]
    fn pse_4m_reserved_bits_fault_before_ad_dirty_update() {
        let reserved_bit_17 = 1 << 17;
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER | reserved_bit_17);

        let mut cpu = pse_cpu();
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_1000),
            0x44,
            3,
        ));

        assert_eq!(page.linear.0, 0x0040_1000);
        assert_eq!(cpu.cr2, 0x0040_1000);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE | PFEC_USER | PFEC_RSVD),
            PFEC_PRESENT | PFEC_WRITE | PFEC_USER | PFEC_RSVD
        );
        assert_eq!(ram.data[0x0040_1000], 0);
        assert_eq!(
            ram.read_u32(PD_BASE + 4),
            0x0040_0000 | PSE_RW_USER | reserved_bit_17
        );
    }

    #[test]
    fn pse_bit_is_ignored_when_cr4_pse_is_clear_and_4k_walk_continues() {
        let mut ram = Ram::new();
        ram.write_u32(
            PD_BASE,
            PT_BASE as u32 | PTE_PRESENT | PTE_WRITE | PTE_USER | PDE_PS,
        );
        ram.write_u32(PT_BASE, 0x3000 | PTE_PRESENT | PTE_WRITE | PTE_USER);
        ram.data[0x3000] = 0xa6;

        let mut cpu = paging_cpu();
        let value =
            read_linear_u8_at_cpl(&mut cpu, &mut ram, LinearAddr(0), AccessKind::Read, 3).unwrap();

        assert_eq!(value, 0xa6);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_ACCESSED, 0);
        assert_ne!(ram.read_u32(PT_BASE) & PTE_ACCESSED, 0);
        assert_eq!(ram.read_u32(PT_BASE) & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_bit_without_cr4_pse_still_uses_pte_permissions() {
        let mut ram = Ram::new();
        ram.write_u32(
            PD_BASE,
            PT_BASE as u32 | PTE_PRESENT | PTE_WRITE | PTE_USER | PDE_PS,
        );
        ram.write_u32(PT_BASE, 0x3000 | PTE_PRESENT | PTE_WRITE);

        let mut cpu = paging_cpu();
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0),
            AccessKind::Read,
            3,
        ));

        assert_eq!(page.linear.0, 0);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_USER),
            PFEC_PRESENT | PFEC_USER
        );
        assert_eq!(page.code & PFEC_RSVD, 0);
    }

    #[test]
    fn pse_4m_large_page_uses_pde_not_pte_permissions() {
        let mut ram = Ram::new();
        ram.write_u32(PD_BASE, PSE_RW_USER);
        ram.write_u32(0, 0);
        ram.data[0x123] = 0x6d;

        let mut cpu = pse_cpu();
        let value = read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0123),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(value, 0x6d);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_ACCESSED, 0);
        assert_eq!(ram.read_u32(0), 0);
    }

    #[test]
    fn pse_4m_physical_base_uses_bits_31_22_and_ignores_low_attribute_bits() {
        let pde_avl_bits = 0x0000_0e00;
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE, 0x0040_0000 | PSE_RW_USER | pde_avl_bits);
        ram.data[0x0040_3456] = 0xcd;

        let mut cpu = pse_cpu();
        let value = read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_3456),
            AccessKind::Read,
            3,
        )
        .unwrap();

        assert_eq!(value, 0xcd);
        assert_eq!(ram.read_u32(PD_BASE) & pde_avl_bits, pde_avl_bits);
    }

    #[test]
    fn pse36_4m_high_address_bits_extend_large_page_base() {
        let mut ram = Ram::new();
        ram.write_u32(PD_BASE, PSE_RW_USER | (1 << 13));

        let mut cpu = pse_cpu();
        let addr = expect_mem_out_of_range(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_1234),
            AccessKind::Read,
            3,
        ));

        assert_eq!(addr.0, 0x0000_0001_0000_1234);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_ACCESSED, 0);
        assert_eq!(ram.read_u32(PD_BASE) & PTE_DIRTY, 0);
    }

    #[test]
    fn pse36_4m_all_high_address_bits_are_not_truncated() {
        let mut ram = Ram::new();
        ram.write_u32(
            PD_BASE,
            0xffc0_0000 | PSE_RW_USER | PDE_4M_PSE36_HIGH_ADDR_MASK,
        );

        let mut cpu = pse_cpu();
        let addr = expect_mem_out_of_range(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x003f_fffc),
            0x5a,
            3,
        ));

        assert_eq!(addr.0, 0x0000_000f_ffff_fffc);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_ACCESSED, 0);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_DIRTY, 0);
    }

    #[test]
    fn pse36_4m_high_address_bits_do_not_conflict_with_avl_bits() {
        let pde_avl_bits = 0x0000_0e00;
        let high_addr_bits = 0x0001_2000;
        let mut ram = Ram::new();
        ram.write_u32(PD_BASE, PSE_RW_USER | pde_avl_bits | high_addr_bits);

        let mut cpu = pse_cpu();
        let addr = expect_mem_out_of_range(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0100),
            AccessKind::Read,
            3,
        ));

        assert_eq!(addr.0, 0x0000_0009_0000_0100);
        assert_eq!(ram.read_u32(PD_BASE) & pde_avl_bits, pde_avl_bits);
        assert_eq!(
            ram.read_u32(PD_BASE) & PDE_4M_PSE36_HIGH_ADDR_MASK,
            high_addr_bits
        );
    }

    #[test]
    fn pse36_4m_reserved_bit_21_faults_before_bus_probe() {
        let reserved_bit_21 = 1 << 21;
        let mut ram = Ram::new();
        ram.write_u32(PD_BASE, PSE_RW_USER | reserved_bit_21);

        let mut cpu = pse_cpu();
        let page = expect_page_fault(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0000_0000),
            AccessKind::Read,
            3,
        ));

        assert_eq!(page.linear.0, 0);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_USER | PFEC_RSVD),
            PFEC_PRESENT | PFEC_USER | PFEC_RSVD
        );
        assert_eq!(ram.read_u32(PD_BASE) & PTE_ACCESSED, 0);
    }

    #[test]
    fn pse_4m_not_present_fault_does_not_write_back_pde() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PDE_PS | PTE_WRITE | PTE_USER);

        let mut cpu = pse_cpu();
        let page = expect_page_fault(write_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0004),
            0xee,
            3,
        ));

        assert_eq!(page.linear.0, 0x0040_0004);
        assert_eq!(page.code & (PFEC_WRITE | PFEC_USER), PFEC_WRITE | PFEC_USER);
        assert_eq!(page.code & PFEC_PRESENT, 0);
        assert_eq!(
            ram.read_u32(PD_BASE + 4),
            0x0040_0000 | PDE_PS | PTE_WRITE | PTE_USER
        );
        assert_eq!(ram.data[0x0040_0004], 0);
    }

    #[test]
    fn pse_4m_cross_large_page_write_probes_both_pages_before_commit() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE, PSE_RW_USER);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_PRESENT | PTE_USER);
        ram.data[0x003f_fffe] = 0xaa;
        ram.data[0x003f_ffff] = 0xbb;
        ram.data[0x0040_0000] = 0xcc;
        ram.data[0x0040_0001] = 0xdd;

        let mut cpu = pse_cpu();
        let page = expect_page_fault(write_linear_u32_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x003f_fffe),
            0x1122_3344,
            3,
        ));

        assert_eq!(page.linear.0, 0x0040_0000);
        assert_eq!(
            page.code & (PFEC_PRESENT | PFEC_WRITE | PFEC_USER),
            PFEC_PRESENT | PFEC_WRITE | PFEC_USER
        );
        assert_eq!(ram.data[0x003f_fffe], 0xaa);
        assert_eq!(ram.data[0x003f_ffff], 0xbb);
        assert_eq!(ram.data[0x0040_0000], 0xcc);
        assert_eq!(ram.data[0x0040_0001], 0xdd);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_DIRTY, 0);
        assert_eq!(ram.read_u32(PD_BASE + 4) & (PTE_ACCESSED | PTE_DIRTY), 0);
    }

    #[test]
    fn pse_4m_cross_large_page_write_commits_both_pages_after_probes() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE, PSE_RW_USER);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER);

        let mut cpu = pse_cpu();
        write_linear_u32_at_cpl(&mut cpu, &mut ram, LinearAddr(0x003f_fffe), 0x1122_3344, 3)
            .unwrap();

        assert_eq!(ram.data[0x003f_fffe], 0x44);
        assert_eq!(ram.data[0x003f_ffff], 0x33);
        assert_eq!(ram.data[0x0040_0000], 0x22);
        assert_eq!(ram.data[0x0040_0001], 0x11);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_DIRTY, 0);
        assert_ne!(ram.read_u32(PD_BASE + 4) & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_4m_wraparound_write_probes_last_and_first_large_pages() {
        let mut ram = Ram::with_size(0x0080_0000);
        ram.write_u32(PD_BASE, PSE_RW_USER);
        ram.write_u32(PD_BASE + 1023 * 4, 0x0040_0000 | PSE_RW_USER);

        let mut cpu = pse_cpu();
        write_linear_u32_at_cpl(&mut cpu, &mut ram, LinearAddr(0xffff_fffe), 0xa1b2_c3d4, 3)
            .unwrap();

        assert_eq!(ram.data[0x007f_fffe], 0xd4);
        assert_eq!(ram.data[0x007f_ffff], 0xc3);
        assert_eq!(ram.data[0], 0xb2);
        assert_eq!(ram.data[1], 0xa1);
        assert_ne!(ram.read_u32(PD_BASE) & PTE_DIRTY, 0);
        assert_ne!(ram.read_u32(PD_BASE + 1023 * 4) & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_4m_out_of_range_probe_does_not_partially_commit() {
        let mut ram = Ram::with_size(0x0040_0002);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER);
        ram.data[0x0040_0000] = 0x55;
        ram.data[0x0040_0001] = 0x66;

        let mut cpu = pse_cpu();
        let addr = expect_mem_out_of_range(write_linear_u32_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0000),
            0x8877_6655,
            3,
        ));

        assert_eq!(addr.0, 0x0040_0002);
        assert_eq!(ram.data[0x0040_0000], 0x55);
        assert_eq!(ram.data[0x0040_0001], 0x66);
        assert_ne!(ram.read_u32(PD_BASE + 4) & PTE_DIRTY, 0);
    }

    #[test]
    fn pse_4m_read_bus_fault_reports_mem_after_translation_and_sets_accessed() {
        let mut ram = Ram::with_size(0x0040_0000);
        ram.write_u32(PD_BASE + 4, 0x0040_0000 | PSE_RW_USER);

        let mut cpu = pse_cpu();
        let addr = expect_mem_out_of_range(read_linear_u8_at_cpl(
            &mut cpu,
            &mut ram,
            LinearAddr(0x0040_0010),
            AccessKind::Read,
            3,
        ));

        assert_eq!(addr.0, 0x0040_0010);
        assert_ne!(ram.read_u32(PD_BASE + 4) & PTE_ACCESSED, 0);
        assert_eq!(ram.read_u32(PD_BASE + 4) & PTE_DIRTY, 0);
    }
}
