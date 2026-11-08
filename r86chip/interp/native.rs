//! Leaf operations used by the ordinary Rust x86 interpreter on the chip.
//!
//! There is no guest dispatch here. The interpreter owns GPRs, EIP, flags, fault
//! ordering and time. A failed translation lookup continues the SAME memory
//! operation through its existing software cache/walker/device path. It never
//! calls an interpreter to retry a decoded instruction.
//!
//! Encodings and hardware behavior are defined in `sim/rv32.rs`. This consumer
//! uses XALU as a short-lived calculator, XTLOOK as a RAM translation cache,
//! and XLD for qualified segment-relative reads and instruction bytes. XLD
//! misses return within the calling asm block, then continue that operation in
//! Rust. Stores retain ordinary SMC and page-table write notifications.
//! XEA32 computes addresses from current instruction bytes and the Cpu's actual
//! native GPR array; Rust still selects the opcode, applies segment overrides,
//! accesses the guest operand and commits architectural state.

use crate::x86::{BackingToken, Cpu, SegmentReg, cpu::LinearTranslationCache};
use core::{arch::asm, cell::UnsafeCell};

#[cfg(not(target_os = "none"))]
compile_error!("r86chip-native requires the single-hart bare-metal firmware");

/// Compute EFLAGS, without retaining live guest state in hardware.
/// The two/three instructions are one asm block: no Rust call can clobber the
/// scratch flag state between XALU.C and XFGET. Carry-consuming/preserving
/// operations explicitly load CF. XFGET merges status into the caller's control
/// flags in hardware, avoiding a redundant software mask/OR sequence. Rd=x0 discards
/// the arithmetic result (the caller already calculated/stored it).
#[inline(always)]
pub(crate) fn flags<const WIDTH: u32, const OP: u32>(
    lhs: u32,
    rhs: u32,
    carry: u32,
    previous: u32,
) -> u32 {
    let status;
    unsafe {
        if OP == 2 || OP == 3 || OP & 32 != 0 {
            asm!(
                ".insn r 0x0b, 4, 0, x0, {carry}, x0",
                ".insn r 0x0b, {width}, {op}, x0, {lhs}, {rhs}",
                ".insn r 0x0b, 5, 0, {status}, {previous}, x0",
                width = const WIDTH, op = const (OP | 8),
                lhs = in(reg) lhs, rhs = in(reg) rhs, carry = in(reg) carry,
                previous = in(reg) previous, status = lateout(reg) status, options(nostack, nomem),
            );
        } else {
            asm!(
                ".insn r 0x0b, {width}, {op}, x0, {lhs}, {rhs}",
                ".insn r 0x0b, 5, 0, {status}, {previous}, x0",
                width = const WIDTH, op = const (OP | 8),
                lhs = in(reg) lhs, rhs = in(reg) rhs,
                previous = in(reg) previous, status = lateout(reg) status, options(nostack, nomem),
            );
        }
    }
    status
}

struct CacheState {
    owner: *const LinearTranslationCache,
    guest_owner: *const LinearTranslationCache,
    base: u32,
    limit: u32,
}
struct LocalCache(UnsafeCell<CacheState>);
// The chip firmware runs one hart, with no native interrupt handler or threads
// entering x86 execution. A scoped owner prevents a cloned/temporary Cpu from
// using another Cpu's translation proofs. This is not a host-thread abstraction.
unsafe impl Sync for LocalCache {}
static CACHE: LocalCache = LocalCache(UnsafeCell::new(CacheState {
    owner: core::ptr::null(),
    guest_owner: core::ptr::null(),
    base: 0,
    limit: 0,
}));

/// Set the native RAM window from the firmware's explicit memory layout.
///
/// # Safety
/// The caller owns a page-aligned, exclusive guest-RAM backing region for the
/// entire run. Call on the one firmware hart, outside interpreter execution.
/// Guest physical addresses/PTEs must never contain these native addresses.
pub unsafe fn configure_ram(base: u32, limit: u32) {
    assert!(base & 4095 == 0 && limit & 4095 == 0 && base < limit);
    unsafe {
        let state = &mut *CACHE.0.get();
        assert!(state.owner.is_null());
        asm!(".insn r 0x2b, 4, 0, x0, {base}, {limit}",
            base = in(reg) base, limit = in(reg) limit, options(nostack));
        asm!(".insn r 0x2b, 7, 0, x0, {service}, x0",
            service = in(reg) guest_load_miss as *const () as u32, options(nostack));
        state.base = base;
        state.limit = limit;
    }
}

/// Borrow the hardware table for one interpreter batch, without moving guest
/// state. Flush once at entry, not once per instruction or software miss. This
/// bounds proof lifetime across machine services, reset, Cpu moves and clones;
/// finer persistence across batches can be evaluated separately. Within the
/// batch, software cache invalidations/privilege changes retire hardware proofs.
pub(crate) struct CacheScope;
impl CacheScope {
    #[inline]
    pub(crate) fn enter(cpu: &Cpu) -> Self {
        unsafe {
            let state = &mut *CACHE.0.get();
            assert!(state.owner.is_null(), "nested chip interpreter batch");
            flush();
            // Retire segment proofs as well as pages, even if this batch uses
            // the same CPU/control bits. Identity 1 is a lease, not a CR3 tag:
            // the explicit (0,0)->(control,1) transition prevents stale proofs
            // after a Cpu move/clone or changes made outside the previous batch.
            asm!(".insn r 0x2b, 5, 0, x0, x0, x0", options(nostack));
            let control = (cpu.cr0 & 1)
                | ((cpu.cr0 >> 30) & 2)
                | ((cpu.cr0 >> 14) & 4)
                | ((cpu.effective_eflags() >> 14) & 8)
                | (u32::from(cpu.cpl()) << 4);
            asm!(".insn r 0x2b, 5, 0, x0, {control}, {identity}",
                control = in(reg) control, identity = in(reg) 1u32, options(nostack));
            state.owner = &cpu.translation_cache;
            // XEA32 reads this native software array on demand. There is no
            // second GPR bank; Cpu moves/clones get a fresh pointer at entry.
            asm!(".insn r 0x7b, 2, 0, x0, {registers}, x0",
                registers = in(reg) cpu.regs.as_ptr(), options(nostack));
            // Data reads use the existing flat-data proofs. CS fetch has its
            // own execute proof, including the actual base and byte limit:
            // executable code need not be readable as a data operand. CS load
            // already qualified its descriptor, as for the software fetcher.
            // Real/v8086 accesses stay in the same Rust helpers.
            if control & 9 == 1 {
                segment::<0>(cpu, SegmentReg::Es);
                segment::<1>(cpu, SegmentReg::Cs);
                segment::<2>(cpu, SegmentReg::Ss);
                segment::<3>(cpu, SegmentReg::Ds);
                segment::<4>(cpu, SegmentReg::Fs);
                segment::<5>(cpu, SegmentReg::Gs);
                let cs = cpu.seg(SegmentReg::Cs);
                asm!(".insn r 0x2b, 6, 4, x1, {base}, {limit}",
                    base = in(reg) cs.base, limit = in(reg) cs.limit, options(nostack));
                state.guest_owner = state.owner;
            }
        }
        Self
    }
}
impl Drop for CacheScope {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            (*CACHE.0.get()).owner = core::ptr::null();
            (*CACHE.0.get()).guest_owner = core::ptr::null();
        }
    }
}

/// Publish the software-qualified flat segment read/write rights. A zero proof
/// is intentional; hardware requests service rather than guessing permissions.
#[inline(always)]
fn segment<const SEG: u32>(cpu: &Cpu, seg: SegmentReg) {
    let rights = u32::from(cpu.flat_segment_access(seg, false))
        | (u32::from(cpu.flat_segment_access(seg, true)) << 1);
    // XGSEG encodes rights, so publish only the three combinations possible for
    // readable x86 data segments. Setup is once per batch, outside the hot loop.
    unsafe {
        match rights {
            3 => asm!(".insn r 0x2b, 6, 3, x{seg}, x0, {limit}",
                seg = const SEG, limit = in(reg) u32::MAX, options(nostack)),
            1 => asm!(".insn r 0x2b, 6, 1, x{seg}, x0, {limit}",
                seg = const SEG, limit = in(reg) u32::MAX, options(nostack)),
            _ => {} // XGCTX already cleared this segment's rights.
        }
    }
}

/// Read 1/2/4 original instruction bytes through independent CS/X page proofs.
/// Only the interpreter's fetch helper calls this, inside CacheScope for the
/// same Cpu; no other Cpu or asynchronous native caller may enter that scope.
/// Context/segment changes disable hardware accesses until the next batch.
/// A miss returns inside this asm and continues the original Rust fetch once.
/// This is a data-bus read of current x86 bytes, not native instruction fetch:
/// guest stores/aliases need no native FENCE.I and no decoded state is retained.
#[inline(always)]
pub(crate) fn fetch<const WIDTH: u32>(offset: u32) -> Option<u32> {
    let value: u32;
    let status: u32;
    unsafe {
        // Wide immediates/displacements may be unaligned. Fetch .U remains
        // bounded to one proved page; a split resumes the ordinary Rust helper
        // which preserves ordered byte fetches and EIP on a later guest fault.
        asm!("li t6, 0", ".insn r 0x5b, {width}, {intent}, {value}, {offset}, x0",
            width = const WIDTH, intent = const (if WIDTH == 0 { 17 } else { 49 }),
            value = lateout(reg) value, offset = in(reg) offset,
            out("t6") status, options(nostack, readonly));
    }
    (status == 0).then_some(value)
}

/// Compute a 32-bit memory operand's offset from current code/GPR bytes.
/// Call only inside the same Cpu's CacheScope, after rejecting ModR/M.mod=3.
/// Info is scratch local to this asm: no Rust call may separate XEA32/XEAINFO.
/// A negative info value preserves EIP and continues the original decoder.
/// Successful info supplies consumed bytes and DS/SS; software applies any
/// segment override and still qualifies the eventual guest operand access.
#[inline(always)]
pub(crate) fn ea32(modrm: u8, eip: u32) -> Option<(u32, u32)> {
    let offset: u32;
    let info: u32;
    unsafe {
        asm!(
            ".insn r 0x7b, 0, 0, {offset}, {modrm}, {eip}",
            ".insn r 0x7b, 1, 0, {info}, x0, x0",
            offset = lateout(reg) offset, info = lateout(reg) info,
            modrm = in(reg) u32::from(modrm), eip = in(reg) eip,
            options(nostack, readonly),
        );
    }
    if (info as i32) < 0 {
        None
    } else {
        Some((offset, info))
    }
}

/// An already successful software fetch supplies X, never inferred from R/W.
/// The shared fetch cache must retire this proof on replacement/invalidation.
/// Publish only to the dedicated fetch table; data and execute proofs have
/// separate lifetimes. No permissions are inferred or merged across views.
#[inline]
pub(crate) fn publish_fetch(cpu: &Cpu, eip: u32, backing: BackingToken) {
    unsafe {
        let state = &*CACHE.0.get();
        let linear = eip.wrapping_add(cpu.seg(SegmentReg::Cs).base);
        let page = backing.0 & !4095;
        if !core::ptr::eq(state.guest_owner, &cpu.translation_cache)
            || (backing.0 ^ u64::from(linear)) & 4095 != 0
            || page < u64::from(state.base)
            || page >= u64::from(state.limit)
        {
            return;
        }
        asm!(".insn r 0x2b, 1, 1, x0, {linear}, {proof}",
            linear = in(reg) linear, proof = in(reg) page as u32 | 4,
            options(nostack));
    }
}

/// A changed guest mode, segment or privilege retires the XLD lease until the
/// next batch. XTLOOK can still mirror software-qualified mappings meanwhile.
/// This deliberately avoids a second live copy of x86 context and refresh logic
/// during task entry/exception delivery, where CPL and CS can be transient.
#[inline]
pub(crate) fn disable_guest() {
    unsafe {
        let state = &mut *CACHE.0.get();
        if !state.guest_owner.is_null() {
            state.guest_owner = core::ptr::null();
            asm!(".insn r 0x2b, 5, 0, x0, x0, x0", options(nostack));
        }
    }
}

// Native service ABI, NOT the C ABI: preserve every register except t6, touch
// neither memory nor flags, and resume immediately after the failed XLD. The
// firmware linker places code above zero and below 8 MiB, so that continuation
// is always nonzero. It is a miss indicator, never a guest address or exception.
// No stack, Rust call, unwinding or register-bank switch occurs here.
#[unsafe(naked)]
unsafe extern "C" fn guest_load_miss() {
    core::arch::naked_asm!(
        ".insn r 0x2b, 7, 1, t6, x0, x0",
        "addi t6, t6, 4",
        ".insn r 0x2b, 7, 2, x0, t6, x0",
    );
}

/// Try the SAME decoded memory operand, with no guest-state commit. A miss has
/// no data access; the caller still performs segment checks before walking or
/// touching MMIO. RMW requires both read and write proofs before any data read.
/// The returned value is a plain temporary; Cpu remains the guest-state owner.
#[inline(always)]
pub(crate) fn load<const WIDTH: u32, const RMW: bool>(
    owner: &LinearTranslationCache,
    segment: SegmentReg,
    offset: u32,
) -> Option<u32> {
    unsafe {
        if !core::ptr::eq((*CACHE.0.get()).guest_owner, owner) {
            return None;
        }
        let value: u32;
        let status: u32;
        // EARLY t6 output: li clobbers it before XLD reads either input. The
        // service path rejoins inside this asm region and clobbers only t6;
        // lateout for value is safe because both XLD operands use OLD values.
        // Readonly, not nomem: a hit reads qualified RAM. It neither walks page
        // tables nor reaches MMIO; a miss's three-word adapter writes only t6
        // and native service state. Thus neither path can modify Rust/guest
        // memory. Keep this impure so loads and service entry cannot be elided.
        // The compiler can retain Cpu fields across this read, while ordinary
        // memory writes and context/proof publication still order it.
        asm!(
            "li t6, 0",
            ".insn r 0x5b, {width}, {intent}, {value}, {offset}, {segment}",
            width = const WIDTH, intent = const (64 | if RMW { 8 } else { 0 }),
            offset = in(reg) offset, segment = in(reg) segment as u32,
            value = lateout(reg) value, out("t6") status, options(nostack, readonly),
        );
        (status == 0).then_some(value)
    }
}

#[inline(always)]
pub(crate) fn flush() {
    unsafe {
        asm!(".insn r 0x2b, 3, 0, x0, x0, x0", options(nostack));
    }
}

#[inline(always)]
pub(crate) fn invalidate(linear: u32) {
    unsafe {
        asm!(".insn r 0x2b, 2, 0, x0, {linear}, x0",
        linear = in(reg) linear, options(nostack));
    }
}

/// Byte-width translation only. Callers retain their own cross-page, segment
/// and RMW-write-intent checks; a hit does not perform a load/store or MMIO.
/// The caller must select the cache's current privilege before using this path.
#[inline(always)]
pub(crate) fn lookup<const WRITE: bool>(
    owner: &LinearTranslationCache,
    linear: u32,
) -> Option<BackingToken> {
    unsafe {
        if !core::ptr::eq((*CACHE.0.get()).owner, owner) {
            return None;
        }
        let pointer: u32;
        asm!(".insn i 0x2b, 0, {pointer}, {linear}, {intent}",
            pointer = lateout(reg) pointer, linear = in(reg) linear,
            intent = const (if WRITE { 4 } else { 0 }), options(nostack));
        (pointer != u32::MAX).then_some(BackingToken(u64::from(pointer)))
    }
}

/// Mirror an existing RAM proof; never walk speculatively to acquire W. Only
/// the software cache can grant permissions after A/D and bus policy checks.
/// Its tokens are opaque: unaligned or non-window tokens stay in software.
#[inline]
pub(crate) fn publish(
    owner: &LinearTranslationCache,
    linear: u32,
    backing: BackingToken,
    write: bool,
) {
    unsafe {
        let state = &*CACHE.0.get();
        let page = backing.0 & !4095;
        if !core::ptr::eq(state.owner, owner)
            || (backing.0 ^ u64::from(linear)) & 4095 != 0
            || page < u64::from(state.base)
            || page >= u64::from(state.limit)
        {
            return;
        }
        let proof = page as u32 | 1 | (u32::from(write) << 1);
        asm!(".insn r 0x2b, 1, 0, x0, {linear}, {proof}",
            linear = in(reg) linear, proof = in(reg) proof, options(nostack));
    }
}
