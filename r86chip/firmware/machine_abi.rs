//! Fixed little-endian simulation RAM layout, not Rust object serialization.
//! All constants here are NATIVE physical addresses. r86's interpreter, JIT and
//! device state belong to firmware; the PC it simulates has a separate guest
//! address space. Eligible guest RAM is backed at GUEST + guest physical offset,
//! after the guest bus resolves range/type/A20/overlay rules. Guest PTEs never
//! contain native pointers. See design/ADDRESS_SPACE.md for ownership and limits.
//! The native-primitives build configures that window for this guest backing.
#![allow(dead_code)]
pub const NATIVE_BYTES: usize = 512 * 1024 * 1024;
pub const CONFIG: usize = 0x0080_0000;
pub const OUTPUT: usize = 0x0090_0000;
pub const OUTPUT_BYTES: usize = 1024 * 1024;
pub const HEAP: usize = 0x0200_0000;
pub const HEAP_BYTES: usize = 64 * 1024 * 1024;
pub const GUEST: usize = 0x1000_0000;
pub const GUEST_BYTES: usize = 128 * 1024 * 1024;
pub const MAGIC: u32 = 0x5238_364d;
pub const WORKLOAD_FLAGS: usize = CONFIG + 28;
pub const WORKLOAD_LUA: u32 = 1;
pub const WORKLOAD_JIT: u32 = 2;
pub const WORKLOAD_BOUNDARIES: u32 = 4;
pub const STATUS: usize = CONFIG + 0x40;
pub const STEPS_LO: usize = CONFIG + 0x44;
pub const STEPS_HI: usize = CONFIG + 0x48;
pub const EIP: usize = CONFIG + 0x4c;
pub const OUTPUT_LEN: usize = CONFIG + 0x50;
pub const JIT_NATIVE: usize = CONFIG + 0x58;
pub const JIT_INVOKES: usize = CONFIG + 0x60;
pub const JIT_BLOCKS: usize = CONFIG + 0x68;
pub const JIT_USER: usize = CONFIG + 0x70;
pub const CPU_REGS: usize = CONFIG + 0x80;
pub const CPU_FLAGS: usize = CONFIG + 0xa0;
pub const JIT_INVALIDATIONS: usize = CONFIG + 0xa8;
pub const CPU_TSC: usize = CONFIG + 0xb0;
pub const BOUNDARY_LEN: usize = CONFIG + 0xb8;
pub const BOUNDARIES: usize = 0x1800_0000;
pub const BOUNDARY_BYTES: usize = 64 * 1024 * 1024;
pub const BOUNDARY_WORDS: usize = 16;
/// Build identity, not a runtime execution-engine switch.
pub const CHIP_EXTENSIONS: usize = CONFIG + 0xc0;
pub const X86_COMPLETED: usize = CONFIG + 0xf0;
pub const X86_FAULT_ATTEMPTS: usize = CONFIG + 0xf8;
pub const X86_REPEAT_CHUNKS: usize = CONFIG + 0x100;
pub const X86_HALTED_POLLS: usize = CONFIG + 0x108;
pub const MARKER: &[u8] = b"R86_CHIP_TINYCORE_OK";
pub const LUA_MARKER: &[u8] = b"R86_CHIP_LUA_OK";

/// Diagnostic record: steps, TSC, EIP, flags, GPRs, CR0 and CR3 as LE words.
/// This is an observation format, not a resumable CPU or Rust-layout snapshot.
pub fn boundary(step: u64, cpu: &r86::x86::Cpu) -> [u32; BOUNDARY_WORDS] {
    let mut words = [0; BOUNDARY_WORDS];
    words[..6].copy_from_slice(&[
        step as u32,
        (step >> 32) as u32,
        cpu.tsc() as u32,
        (cpu.tsc() >> 32) as u32,
        cpu.eip,
        cpu.effective_eflags(),
    ]);
    words[6..14].copy_from_slice(&cpu.regs);
    words[14] = cpu.cr0;
    words[15] = cpu.cr3;
    words
}
