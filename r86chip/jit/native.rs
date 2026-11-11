//! Small bank/context boundary. Only explicit arrays cross it, never Cpu layout.
use core::arch::{asm, naked_asm};

#[unsafe(naked)]
pub(super) unsafe extern "C" fn service() {
    naked_asm!(".insn i 0x7b, 5, x0, x0, 2");
}

pub(super) fn import(regs: &[u32; 8], flags: u32) {
    unsafe {
        asm!(
            "lw t0, 0({regs})", ".insn i 0x7b, 7, x8, t0, 0",
            "lw t0, 4({regs})", ".insn i 0x7b, 7, x9, t0, 0",
            "lw t0, 8({regs})", ".insn i 0x7b, 7, x10, t0, 0",
            "lw t0, 12({regs})", ".insn i 0x7b, 7, x11, t0, 0",
            "lw t0, 16({regs})", ".insn i 0x7b, 7, x12, t0, 0",
            "lw t0, 20({regs})", ".insn i 0x7b, 7, x13, t0, 0",
            "lw t0, 24({regs})", ".insn i 0x7b, 7, x14, t0, 0",
            "lw t0, 28({regs})", ".insn i 0x7b, 7, x15, t0, 0",
            ".insn i 0x0b, 4, x0, {flags}, 0",
            regs = in(reg) regs, flags = in(reg) flags, out("t0") _, options(nostack)
        );
    }
}
pub(super) fn export(regs: &mut [u32; 8], flags: u32) -> u32 {
    let result;
    unsafe {
        asm!(
            ".insn i 0x7b, 6, t0, x8, 0", "sw t0, 0({regs})",
            ".insn i 0x7b, 6, t0, x9, 0", "sw t0, 4({regs})",
            ".insn i 0x7b, 6, t0, x10, 0", "sw t0, 8({regs})",
            ".insn i 0x7b, 6, t0, x11, 0", "sw t0, 12({regs})",
            ".insn i 0x7b, 6, t0, x12, 0", "sw t0, 16({regs})",
            ".insn i 0x7b, 6, t0, x13, 0", "sw t0, 20({regs})",
            ".insn i 0x7b, 6, t0, x14, 0", "sw t0, 24({regs})",
            ".insn i 0x7b, 6, t0, x15, 0", "sw t0, 28({regs})",
            ".insn i 0x0b, 5, {result}, {flags}, 0",
            regs = in(reg) regs, flags = in(reg) flags, result = lateout(reg) result,
            out("t0") _, options(nostack)
        );
    }
    result
}
pub(super) struct Exit {
    pub reason: u32,
    pub pc: u32,
    pub eip: u32,
    pub remaining: u32,
}
pub(super) fn enter(pc: u32) -> Exit {
    let (reason, leave_pc, eip, remaining);
    unsafe {
        asm!(
            ".insn i 0x7b, 4, x0, {pc}, 0",
            ".insn r 0x7b, 6, 1, {reason}, x0, x0",
            ".insn r 0x7b, 6, 1, {leave_pc}, x1, x0",
            ".insn i 0x7b, 6, {eip}, x16, 0",
            ".insn i 0x6b, 6, {remaining}, x0, 0",
            pc = in(reg) pc, reason = lateout(reg) reason, leave_pc = lateout(reg) leave_pc,
            eip = lateout(reg) eip, remaining = lateout(reg) remaining, options(nostack)
        );
    }
    Exit {
        reason,
        pc: leave_pc,
        eip,
        remaining,
    }
}
pub(super) fn ticks(value: u32) {
    unsafe {
        asm!(".insn i 0x6b, 5, x0, {0}, 0", in(reg) value, options(nostack));
    }
}
pub(super) fn flush_jumps() {
    unsafe {
        asm!(".insn i 0x6b, 3, x0, x0, 0", options(nostack));
    }
}
pub(super) fn fill_jump(eip: u32, pc: u32) {
    unsafe {
        asm!(".insn r 0x6b, 1, 0, x0, {0}, {1}", in(reg) eip, in(reg) pc, options(nostack));
    }
}
pub(super) fn flush_data() {
    unsafe {
        asm!(".insn i 0x2b, 3, x0, x0, 0", options(nostack));
    }
}
pub(super) fn invalidate_data(linear: u32) {
    unsafe {
        asm!(".insn i 0x2b, 2, x0, {linear}, 0", linear = in(reg) linear, options(nostack));
    }
}
pub(super) fn fill_data(linear: u32, native_rights: u32) {
    unsafe {
        asm!(".insn r 0x2b, 1, 0, x0, {0}, {1}", in(reg) linear, in(reg) native_rights, options(nostack));
    }
}
pub(super) fn region(start: u32, end: u32) {
    unsafe {
        asm!(".insn r 0x2b, 4, 0, x0, {0}, {1}", in(reg) start, in(reg) end, options(nostack));
    }
}
pub(super) fn context(control: u32, identity: u32) {
    unsafe {
        asm!(".insn r 0x2b, 5, 0, x0, {0}, {1}", in(reg) control, in(reg) identity, options(nostack));
    }
}
pub(super) fn segment<const S: u32>(base: u32, limit: u32, rights: u32) {
    // rd and funct7 are fields, not values read from the current bank.
    macro_rules! publish { ($r:literal) => { unsafe {
        asm!(".insn r 0x2b, 6, {rights}, x{seg}, {base}, {limit}",
            rights = const $r, seg = const S, base = in(reg) base, limit = in(reg) limit,
            options(nostack));
    } }; }
    match rights {
        1 => publish!(1),
        3 => publish!(3),
        _ => publish!(0),
    }
}
pub(super) fn vector(value: u32) -> u32 {
    let old;
    unsafe {
        asm!(".insn r 0x2b, 7, 1, {old}, x6, x0", ".insn r 0x2b, 7, 0, x0, {value}, x0",
            old = out(reg) old, value = in(reg) value, options(nostack));
    }
    old
}
/// End the active service before inspecting/refilling its persistent record.
pub(super) fn service_record() -> [u32; 4] {
    let (pc, offset, word, reason);
    unsafe {
        asm!("lla t0, 2f", ".insn r 0x2b, 7, 2, x0, t0, x0", "2:",
            ".insn r 0x2b, 7, 1, {pc}, x0, x0",
            ".insn r 0x2b, 7, 1, {offset}, x1, x0",
            ".insn r 0x2b, 7, 1, {word}, x2, x0",
            ".insn r 0x2b, 7, 1, {reason}, x3, x0",
            pc = out(reg) pc, offset = out(reg) offset, word = out(reg) word,
            reason = out(reg) reason, out("t0") _, options(nostack));
    }
    [pc, offset, word, reason]
}
pub(super) fn publish() {
    unsafe {
        asm!("fence.i", options(nostack));
    }
}
