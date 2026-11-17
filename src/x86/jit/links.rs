//! AArch64 block dispatch with resident guest registers.
//!
//! Each entry certifies a previously validated execute page in one CPU mapping
//! epoch and one backing-page content version. Rust retires stale links after a
//! watched code write and before entering native code; the chain cannot perform
//! such a write itself. Its second level holds one native
//! entry per byte offset. After a mapping-epoch change, validating the page restores
//! all its compiled entries at once. Supervisor/user planes qualify privilege;
//! Stable context ids qualify CS base and flat DS/ES/SS facts; a different
//! context cannot pass even an execute-walk recheck. Returning to a context
//! can reuse its links. Exhausting the ids clears both planes before reuse.
//! Relevant control changes expire mappings independently of these ids.
//! CR3 preserves qualified global pages. INVLPG expires its addressed range,
//! even with an empty ordinary fetch cache. Collisions use the full Rust lookup.
//!
//! A data-segment load refreshes the native view and ends its decoded block.
//! It exits the invocation if compiled segment qualifications changed, before
//! any old specialized link can run. No chain changes control state, watched
//! code or device configuration. Page-table stores may leave cached translations
//! valid until
//! architectural invalidation. Cold guards revalidate or refill derived entries;
//! failed guards commit exact progress and return before the next instruction.
//! Linear framebuffer writes notify dirty pages without changing device decode.
//! A chain shares one memory view and frame until the next event deadline.
//!
//! Reclamation clears links before executable storage or version storage can
//! disappear. Publication, reclamation and content retirement never run on an
//! active native stack; exact execute-walk rechecks may only retag existing
//! links. No Rust object layout is part of the generated-code ABI.

use super::a64::{Assembler, Cond, Label, Reg, ZR};
use super::frame::{FRAME, Frame, PC, RETURN_TOP};
use core::mem::{offset_of, size_of};

pub const SLOTS: usize = 1024;
pub const PAGE_BYTES: usize = 4096;
pub const INTERPRET: u32 = 1;
// Reserved non-native entries; admission heat lives outside the dispatch table.
pub const COLD: u32 = 3;

pub const fn slot(linear: u32) -> usize {
    (linear as usize >> 12) & (SLOTS - 1)
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Link {
    // Bits 63..32: mapping epoch; 31..12: linear page; 11..0: context id.
    pub tag: u64,
    // Signed byte displacements from this table. The adapter keeps code and
    // tables within i32 reach; native entries never overlap 0..=COLD sentinels.
    pub entries: *const u32,
}

impl Link {
    pub const EMPTY: Self = Self {
        tag: 0,
        entries: core::ptr::null(),
    };
}

/// Guest bounds of the page containing the block being compiled. A native
/// block can only be entered after that page's execute mapping and content have
/// been validated. Since a chain cannot change either, an immediate edge within
/// the same page can be patched to a direct native branch when its target exists.
///
/// Content changes invalidate every source and destination on the page together;
/// old patched sources become unreachable. Arena reclamation discards code and
/// pending patches together, outside native execution. Conditional edges may
/// bypass their local trampoline when their shorter displacement fits.
/// Cross-page and indirect
/// edges still use the full dispatcher below. FEX BranchOps.cpp was consulted for
/// native branch linking; its usermode invalidation is not our full-system proof.
#[derive(Clone, Copy)]
pub struct Page {
    pub eip_start: u32,
}

impl Page {
    pub fn entry_offset(self, eip: u32) -> Option<u16> {
        let offset = eip.wrapping_sub(self.eip_start) as usize;
        (offset < PAGE_BYTES).then_some(offset as u16)
    }
}

/// One replaceable branch in a compiled block; its target shares the source's
/// code-page validity. The adapter patches only after publication, with no
/// active native stack, and retains unresolved sites until the target exists.
pub struct Edge {
    pub native_offset: u32,
    pub target_offset: u16,
}

/// Optional call/return lookup cache, confined to one native invocation. A
/// record points to a continuation in the already validated caller's code.
/// Native code cannot invalidate its content or execution context; every exit
/// discards the stack by resetting its top on reentry. Mismatched guest return
/// addresses discard the cache, and overflow discards older records. Neither
/// condition changes guest state or requires a recoverable host stack fault.
/// Consulted FEX JIT/BranchOps.cpp for return lookup cost; this bounded cache
/// uses ordinary host data and no signal/exception recovery or architectural
/// assumption that guest calls and returns are balanced.
/// Native CALL edges use BL/BLR and matching returns use RET to the checked
/// continuation. This also trains the hardware return predictor; its prediction
/// never supplies an architectural destination. No guest return relies on LR.
#[derive(Clone, Copy)]
#[repr(C)]
struct ReturnAddress {
    eip: u64,
    native: u64,
}

impl ReturnAddress {
    const EMPTY: Self = Self { eip: 0, native: 0 };
}

/// One invocation's bounded return stack. The low twelve address bits give
/// its byte depth; bit eleven means all 128 records are occupied. Alignment
/// permits checking/resetting the resident pointer without loading bounds.
/// Only entries below the top may be read; overflow discards older records.
#[repr(C, align(4096))]
pub struct ReturnStack {
    entries: [ReturnAddress; 128],
}

impl ReturnStack {
    pub const fn new() -> Self {
        Self {
            entries: [ReturnAddress::EMPTY; 128],
        }
    }
}

/// Called only after CALL's guest stack write and ESP commit. `guest` still
/// holds the full next EIP already formed for that write. X6 holds the
/// cache top across linked blocks; helper calls spill it. The adapter owns its
/// nonnull return stack; every invocation starts at that stack's aligned base.
pub fn push_return(a: &mut Assembler, guest: Reg, continuation: Label) {
    let native = Reg::new(5);
    let full = a.label();
    let room = a.label();
    assert_eq!(size_of::<ReturnAddress>(), 16);
    assert_eq!(core::mem::align_of::<ReturnStack>(), 4096);
    assert_eq!(offset_of!(ReturnStack, entries), 0);
    a.tbz(RETURN_TOP, 11, true, full);
    a.bind(room);
    a.adr(native, continuation);
    a.pair_writeback(false, guest, native, RETURN_TOP, 16, false);
    a.tail(super::cold::Path::ReturnStack {
        entry: full,
        resume: room,
        overflow: true,
    });
}

/// RET has already read the architectural destination and committed its ESP.
/// Matching the exact EIP permits jumping to the caller's local continuation;
/// otherwise execution falls through to the ordinary validated dispatcher.
pub fn cached_return(a: &mut Assembler) {
    let depth = Reg::new(5);
    let guest = Reg::new(0);
    let native = Reg::new(1);
    let miss = a.label();
    let mismatch = a.label();
    a.bitfield(depth, RETURN_TOP, 0, 11, false, false);
    a.cbz(depth, false, false, miss);
    a.pair_writeback(true, guest, native, RETURN_TOP, -16, true);
    a.add_sub(ZR, guest, PC, false, true, true);
    a.b_cond(Cond::Ne, mismatch);
    #[cfg(any(test, feature = "jit-profile"))]
    {
        a.load_store(
            true,
            guest,
            FRAME,
            offset_of!(Frame, cached_returns) as u32,
            3,
        );
        a.add_sub_imm(guest, guest, 1, true, false, false);
        a.load_store(
            false,
            guest,
            FRAME,
            offset_of!(Frame, cached_returns) as u32,
            3,
        );
    }
    a.ret_to(native);
    a.tail(super::cold::Path::ReturnStack {
        entry: mismatch,
        resume: miss,
        overflow: false,
    });
    a.bind(miss);
}

/// W14 holds the next EIP. Clobbers X0..X5, X9/X10 and NZCV, preserving the
/// architectural flag registers and all resident guest registers. A hit enters
/// the next block after its prologue; its budget check runs on every backedge.
/// The adapter supplies a live SLOTS-entry link table for this invocation.
pub fn emit_dispatch(a: &mut Assembler, miss: Label) {
    assert_eq!(size_of::<Link>(), 16);
    let pc = Reg::new(0);
    let slot = Reg::new(1);
    let tag = Reg::new(2);
    let value = Reg::new(3);
    let entries = Reg::new(4);
    a.load_store(true, slot, FRAME, offset_of!(Frame, links) as u32, 3);
    // The full adapter supplies CS in X16's high half. Only bits 0..31 of
    // this sum reach the page tag, slot, offset or execute walk below, so an
    // addition carry retains x86's wrapping linear-address semantics.
    a.add_lsr64(pc, PC, super::soft::STATE, 32);
    a.bitfield(value, pc, 12, 21, true, false);
    a.add_shifted(slot, slot, value, 4, true);
    a.load_store(true, tag, FRAME, offset_of!(Frame, link_context) as u32, 3);
    a.and_high_bits(value, pc, 12, false);
    a.logical(1, tag, tag, value, true);
    assert_eq!(offset_of!(Link, entries), 8);
    a.pair(true, value, entries, slot, 0, true);
    a.add_sub(ZR, tag, value, true, true, true);
    let recheck = a.label();
    let matched = a.label();
    a.b_cond(Cond::Ne, recheck);
    a.bind(matched);
    // The entry boundary validated content versions. A matching nonzero tag
    // therefore certifies a live, unchanged dense entry table for this chain.
    a.bitfield(pc, pc, 0, 11, true, false);
    a.load_i32_indexed(value, entries, pc);
    a.add_sub_imm(ZR, value, COLD as u16, false, true, true);
    a.b_cond(Cond::Ls, miss); // Uncompiled, cold, or a known interpreter boundary.
    a.add_sub(value, entries, value, true, false, false);
    a.br(value);
    a.bind(recheck);
    a.cbz(value, true, false, miss);
    a.add_sub(ZR, tag, value, false, true, true);
    a.b_cond(Cond::Ne, miss);
    a.bitfield(value, pc, 12, 21, false, false);
    a.load_store(
        true,
        Reg::new(5),
        FRAME,
        offset_of!(Frame, link_rechecks) as u32,
        3,
    );
    a.add_shifted(value, Reg::new(5), value, 5, true);
    super::soft::recheck_code(a, value, pc, miss);
    a.load_store(false, tag, slot, offset_of!(Link, tag) as u32, 3);
    #[cfg(any(test, feature = "jit-profile"))]
    {
        a.load_store(
            true,
            value,
            FRAME,
            offset_of!(Frame, rechecked_code) as u32,
            3,
        );
        a.add_sub_imm(value, value, 1, true, false, false);
        a.load_store(
            false,
            value,
            FRAME,
            offset_of!(Frame, rechecked_code) as u32,
            3,
        );
    }
    a.b(matched);
}

#[cfg(all(test, a64_jit))]
mod tests {
    use super::*;
    use crate::x86::Cpu;
    use crate::x86::jit::{code::CodeArena, compile, decode, soft::View};

    #[test]
    fn dispatch_rejects_all_cold_counts_and_accepts_a_native_offset() {
        let mut arena = CodeArena::with_limits(64 * 1024, 1).unwrap();
        let stubs = compile::entry().unwrap();
        let entry = arena.publish(&stubs.words).unwrap();
        let dispatch = arena.address(entry, stubs.dispatch_offset).unwrap();
        let finish = arena.address(entry, stubs.return_offset).unwrap();
        let table = arena.allocate_entries();
        let entries = arena.entries_pointer(table);
        let mut links = [Link::EMPTY; SLOTS];
        links[0] = Link {
            tag: 1 << 32,
            entries,
        };
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0x100, 0);
        let mut view = View::new(&mut cpu, false);
        for count in 0..=COLD {
            arena.entries_mut(table)[0x100] = count;
            let mut frame = Frame::from_cpu(&cpu, 1);
            frame.memory = core::ptr::from_mut(&mut view).cast();
            frame.native_entry = dispatch;
            frame.links = links.as_mut_ptr();
            frame.link_context = 1 << 32;
            // SAFETY: the matching link owns the live table. Every tested
            // sentinel must return without attempting to execute that table.
            unsafe { arena.invoke(entry, &mut frame).unwrap() };
            assert_eq!(frame.eip, cpu.eip);
            assert_eq!(frame.regs, cpu.regs);
            assert_eq!(frame.completed, 0);
            assert_eq!(frame.remaining, 1);
        }
        let decoded = decode::block(&[0x40], 0x100, 1); // inc eax
        let mut code = compile::block(&decoded, None, 0).unwrap();
        let body = arena.publish_block(&mut code).unwrap();
        let address = arena.address(body, 0).unwrap();
        let displacement = (address as isize - entries as isize) as i32;
        assert!(displacement < 0);
        arena.entries_mut(table)[0x100] = displacement as u32;
        let mut frame = Frame::from_cpu(&cpu, 1);
        frame.memory = core::ptr::from_mut(&mut view).cast();
        frame.native_entry = dispatch;
        frame.dispatch_entry = finish;
        frame.return_entry = finish;
        frame.links = links.as_mut_ptr();
        frame.link_context = 1 << 32;
        // SAFETY: the native displacement points into this arena's live code.
        unsafe { arena.invoke(entry, &mut frame).unwrap() };
        assert_eq!(frame.eip, 0x101);
        assert_eq!(frame.regs[0], cpu.regs[0].wrapping_add(1));
        assert_eq!(frame.completed, 1);
        assert_eq!(frame.remaining, 0);
    }
}
