//! Execute emitted Wasm with Node and compare the committed x86 prefix with
//! the shared interpreter. This catches emitter/flags/control-flow mistakes;
//! guest boot and paging/SMC checks cover the surrounding publication owner.

use super::{
    compile,
    frame::{ENTRY_BITS, Frame, GROUP_FUNCTIONS, LINK_PLANES, LINK_SLOTS, Link},
};
use crate::x86::cpu::{LINEAR_TRANSLATION_CACHE_BITS, LinearTranslationCacheEntry};
use crate::x86::{Bus, Cpu, Interpreter, MemError, PhysAddr};
use core::mem::{offset_of, size_of};
use std::process::{Command, Stdio};

struct Ram(Vec<u8>);
// Two guest pages share backing RAM, as with ordinary paging aliases. The
// emitter must detect overlap using backing pointers, not guest addresses.
fn backing(addr: PhysAddr) -> usize {
    let at = addr.0 as usize;
    if at >> 12 == 7 { at - 0x5000 } else { at }
}
impl Bus for Ram {
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.0
            .get(backing(addr))
            .copied()
            .ok_or(MemError::OutOfRange { addr })
    }
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        if backing(addr) < self.0.len() {
            Ok(())
        } else {
            Err(MemError::OutOfRange { addr })
        }
    }
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        self.probe_phys_write_u8(addr)?;
        self.0[backing(addr)] = value;
        Ok(())
    }
}

// Feed some existing cases with pending interpreter arithmetic, while the
// reference starts from its independently materialized architectural value.
fn incoming_flags(cpu: &mut Cpu, flags: u32, lhs: u32, rhs: u32, choice: usize) -> ([u32; 4], u32) {
    cpu.replace_eflags(flags);
    macro_rules! seed {
        ($ty:ty, $add:ident, $adc:ident, $sub:ident, $sbb:ident, $inc:ident, $dec:ident, $logic:ident, $shift:ident) => {{
            let (a, b, carry) = (lhs as $ty, rhs as $ty, (flags & 1) as $ty);
            match choice / 3 % 9 {
                0 => {}
                1 => cpu.$add(a, b, a.wrapping_add(b)),
                2 => cpu.$adc(a, b, carry, a.wrapping_add(b).wrapping_add(carry)),
                3 => cpu.$sub(a, b, a.wrapping_sub(b)),
                4 => cpu.$sbb(a, b, carry, a.wrapping_sub(b).wrapping_sub(carry)),
                5 => cpu.$inc(a, a.wrapping_add(1)),
                6 => cpu.$dec(a, a.wrapping_sub(1)),
                7 => cpu.$logic(a ^ b),
                _ => cpu.$shift(a, carry != 0, flags & 0x800 != 0),
            }
        }};
    }
    match choice % 3 {
        0 => seed!(
            u8,
            set_add_flags8,
            set_adc_flags8,
            set_sub_flags8,
            set_sbb_flags8,
            set_inc_flags8,
            set_dec_flags8,
            set_logic_flags8,
            set_shift_flags8
        ),
        1 => seed!(
            u16,
            set_add_flags16,
            set_adc_flags16,
            set_sub_flags16,
            set_sbb_flags16,
            set_inc_flags16,
            set_dec_flags16,
            set_logic_flags16,
            set_shift_flags16
        ),
        _ => seed!(
            u32,
            set_add_flags32,
            set_adc_flags32,
            set_sub_flags32,
            set_sbb_flags32,
            set_inc_flags32,
            set_dec_flags32,
            set_logic_flags32,
            set_shift_flags32
        ),
    }
    let (bits, a, r, kind) = cpu.jit_arithmetic();
    ([bits, a, r, kind], cpu.effective_eflags())
}

#[test]
fn wasm_regions_match_committed_interpreter_prefixes() {
    let mut programs: Vec<Vec<u8>> = Vec::new();
    for prefix in [&[][..], &[0x66][..]] {
        for opcode in [0x01, 0x09, 0x11, 0x19, 0x21, 0x29, 0x31, 0x39] {
            for byte in [false, true] {
                let mut code = if byte { vec![] } else { prefix.to_vec() };
                code.extend_from_slice(&[opcode - u8::from(byte), 0xd0]); // A op D
                let mut across = code.clone();
                across.extend_from_slice(&[0xeb, 0, 0x9c, 0x5f, 0xa3, 0, 0x20, 0, 0]);
                code.extend_from_slice(&[0x9c, 0x5f, 0xa3, 0, 0x20, 0, 0]); // flags -> EDI, A -> RAM
                programs.push(code);
                programs.push(across); // observe the same flags through a control-flow join
            }
        }
        for (opcode, group) in [
            (0xd3, 4),
            (0xd3, 5),
            (0xd3, 7),
            (0xd3, 0),
            (0xd3, 1),
            (0xd3, 2),
            (0xd3, 3),
        ] {
            for byte in [false, true] {
                let mut code = if byte { vec![] } else { prefix.to_vec() };
                code.extend_from_slice(&[opcode - u8::from(byte), 0xc0 | group << 3]);
                // Observe both an eager consumer and carry propagated through
                // a join, including zero-count preservation of incoming flags.
                let mut carry = code.clone();
                carry.extend([0xeb, 0, 0x11, 0xd0, 0x40, 0x9c, 0x5f]);
                code.extend([0x9c, 0x5f]);
                programs.push(code);
                programs.push(carry);
            }
        }
        for condition in 0..16 {
            let mut code = prefix.to_vec();
            code.extend([0x0f, 0x40 | condition, 0xc2, 0x9c, 0x5f]);
            programs.push(code);
            let mut code = vec![0x01, 0xd0, 0xeb, 0];
            code.extend(prefix);
            code.extend([0x0f, 0x40 | condition, 0x06, 0x9c, 0x5f]); // CMOV A,[ESI]
            programs.push(code);
        }
        for bytes in [
            &[0x0f, 0xaf, 0xc2][..],
            &[0xf7, 0xe2],
            &[0xf7, 0xea], // IMUL/MUL
            &[0x0f, 0xc1, 0xd0],
            &[0x0f, 0xc1, 0xc0], // XADD, aliased XADD
            &[0x0f, 0xb1, 0xd0],
            &[0x0f, 0xb1, 0x13], // CMPXCHG register/RAM
            &[0x0f, 0xa3, 0xc8],
            &[0x0f, 0xab, 0xcb],
            &[0x0f, 0xb3, 0xc8],
            &[0x0f, 0xbb, 0xc8],
            &[0x0f, 0xab, 0x0b], // BTS [EBX],ECX
        ] {
            let mut code = prefix.to_vec();
            code.extend_from_slice(bytes);
            programs.push(code);
        }
    }
    for reg in 0..8 {
        // Observe flags without using the guest stack (ESP may be reversed).
        programs.push(vec![0x0f, 0xc8 | reg]);
    }
    for opcode in [0x9e, 0x9f, 0xf5, 0xf8, 0xf9] {
        programs.push(vec![opcode, 0x9c, 0x5f]);
        programs.push(vec![0x01, 0xd0, opcode, 0x11, 0xd3, 0x9f, 0x9c, 0x5f]);
    }
    programs.push(vec![0x01, 0xd0, 0x9f, 0xf5, 0x9e, 0x0f, 0x9a, 0xc2]);
    for (prefix, opcode, count) in [
        (&[][..], 0xc0, 9),
        (&[0x66][..], 0xc1, 17),
        (&[][..], 0xc1, 32),
    ] {
        for group in [2, 3] {
            let mut code = vec![0x01, 0xd0];
            code.extend(prefix);
            code.extend([opcode, 0xc0 | group << 3, count, 0x9c, 0x5f]);
            programs.push(code); // normalized-zero immediate must keep the pending flags
            let mut code = prefix.to_vec();
            code.extend([
                if opcode == 0xc0 { 0xd2 } else { 0xd3 },
                group << 3 | 6,
                0x9c,
                0x5f,
            ]);
            programs.push(code); // RCL/RCR [ESI],CL
            let mut code = prefix.to_vec();
            code.extend([
                if opcode == 0xc0 { 0xd2 } else { 0xd3 },
                0xc1 | group << 3,
                0x9c,
                0x5f,
            ]);
            programs.push(code); // CL/CX/ECX is both count and destination
        }
    }
    programs.push(vec![0xe3, 4, 0x49, 0xeb, 0xfb, 0x90, 0x9c, 0x5f]);
    programs.push(vec![0xb9, 0, 0, 1, 0, 0xe3, 2, 0x40, 0x4a, 0x9c, 0x5f]);
    for prefix in [&[][..], &[0x66][..]] {
        for opcode in [0xa4, 0xa5, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf] {
            let mut code = prefix.to_vec();
            code.push(opcode);
            code.extend([0x9c, 0x5d]); // String result and flags observable together.
            programs.push(code);
        }
    }
    for prefix in [&[][..], &[0x66][..]] {
        for repeat in [0xf2, 0xf3] {
            for opcode in [0xa4, 0xa5, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf] {
                let mut code = prefix.to_vec();
                code.extend([repeat, opcode, 0x9c, 0x5d]);
                programs.push(code);
            }
        }
    }
    // REP is not memmove: a forward overlapping destination may overwrite
    // later source elements. Exercise byte/word/dword propagation, harmless
    // overlap, equal pointers, and an unaligned element at a page boundary.
    for (prefix, opcode) in [(&[][..], 0xa4), (&[0x66][..], 0xa5), (&[][..], 0xa5)] {
        for (source, destination) in [
            (0x2400u32, 0x2401u32),
            (0x2400, 0x2404),
            (0x2401, 0x2400),
            (0x2400, 0x2400),
            (0x2ff9, 0x23f0),
            (0x2400, 0x7401),
            (0x7400, 0x2401),
        ] {
            let mut code = vec![0xbe];
            code.extend(source.to_le_bytes());
            code.push(0xbf);
            code.extend(destination.to_le_bytes());
            code.extend(prefix);
            code.extend([0xf3, opcode, 0x9c, 0x5d]);
            programs.push(code);
        }
    }
    // The last successful SCAS comparison controls both early termination
    // and all arithmetic flags, including after a page-bounded chunk.
    for (prefix, opcode) in [(&[][..], 0xae), (&[0x66][..], 0xaf), (&[][..], 0xaf)] {
        for equal in [false, true] {
            let mut code = vec![0xbf, 0xf0, 0x2f, 0, 0, 0xb9, 32, 0, 0, 0];
            code.extend([0xc7, 0x05, 0x0c, 0x30, 0, 0, 0xff, 0xff, 0xff, 0xff]);
            code.extend([0x31, 0xc0]);
            if !equal {
                code.extend([0xb8, 0xff, 0xff, 0xff, 0xff]);
            }
            code.extend(prefix);
            code.extend([if equal { 0xf3 } else { 0xf2 }, opcode, 0x9c, 0x5d]);
            programs.push(code);
        }
    }
    programs.extend([
        vec![0xb9, 8, 0, 0, 0, 0xbf, 0xf9, 0x2f, 0, 0, 0xf3, 0xa5], // partial MOVS destination
        vec![0xb9, 8, 0, 0, 0, 0xbe, 0xf9, 0x2f, 0, 0, 0xf3, 0xad], // partial LODS source
        vec![0xb9, 8, 0, 0, 0, 0xbf, 0xf9, 0x2f, 0, 0, 0xf2, 0xaf], // partial SCAS flags
        vec![0xb9, 0x88, 0x13, 0, 0, 0xbf, 0, 0x90, 0, 0, 0xf3, 0xab], // shared 4096-element unit
        vec![0xbe, 0, 0x24, 0, 0, 0xbf, 1, 0x24, 0, 0, 0xa4, 0xa4], // overlapping MOVS
        vec![0xbf, 0xff, 0x2f, 0, 0, 0xa5], // destination guard, no partial indices
        vec![0xbe, 0xff, 0x2f, 0, 0, 0xad], // source guard, no partial accumulator
        vec![0x01, 0xd0, 0x40, 0x4a, 0x9c, 0x5f], // pending flags, INC/DEC preserve carry
        vec![0x01, 0xd0, 0xfb], // STI preserves deferred arithmetic on its attention exit
        vec![0x01, 0xd0, 0x52, 0x9d, 0x9c, 0x5f], // POPFD replaces status; TF rejection retains old flags
        vec![0x01, 0xd0, 0x66, 0x52, 0x66, 0x9d, 0x9c, 0x5f], // word POPF preserves upper control bits
        vec![0xd3, 0xe0, 0xa3, 0xff, 0x2f, 0, 0], // shift record survives a failed memory qualifier
        vec![0x01, 0xd0, 0xeb, 0, 0x11, 0xc8, 0xeb, 0, 0x40, 0x9c, 0x5f], // ADD -> ADC -> INC, carry through joins
        vec![0x19, 0xd0, 0xeb, 0, 0x40, 0xeb, 0, 0x11, 0xc8, 0x9c, 0x5f], // SBB -> INC -> ADC
        vec![0x01, 0xd0, 0xeb, 0, 0xa3, 0, 0, 1, 0], // joined flags survive a failed memory qualifier
        vec![0x0f, 0xad, 0xd0],
        vec![0x0f, 0xa5, 0xd0],                   // SHRD/SHLD A,D,CL
        vec![0x01, 0xd0, 0x50, 0x5a, 0x9c, 0x5f], // stack and arithmetic flags
        vec![0x50, 0x89, 0xe5, 0x54, 0x66, 0x52, 0x66, 0x5f, 0x5e, 0x5a], // mixed widths, PUSH ESP, intervening MOV
        vec![0x90, 0xbc, 2, 0xf0, 0, 0, 0x50, 0x52], // stack window crosses a page
        vec![0x90, 0xbc, 0xfe, 0xef, 0, 0, 0x58, 0x5a], // split read retains current ESP
        vec![0x50, 0x54, 0x5c, 0x5a],                // POP ESP ends the preceding proof
        vec![0x86, 0xc4],                            // AL/AH exchange
        vec![0xf6, 0xe2],
        vec![0xf6, 0xea], // byte MUL/IMUL -> AX
        vec![0x90, 0xf7, 0xf1],
        vec![0x90, 0xf7, 0xf9],
        vec![0x90, 0x66, 0xf7, 0xf1],
        vec![0x90, 0x66, 0xf7, 0xf9],
        vec![0x90, 0xf6, 0xf1],
        vec![0x90, 0xf6, 0xf9], // DIV/IDIV, zero/overflow exits
        vec![
            0xba, 0, 0, 0, 0x80, 0x31, 0xc0, 0xb9, 0xff, 0xff, 0xff, 0xff, 0xf7, 0xf9,
        ], // INT64_MIN/-1 cannot trap Wasm
        vec![0x0f, 0xc7, 0x0b], // CMPXCHG8B [EBX]
        vec![0xb9, 0xff, 0xff, 0xff, 0xff, 0x0f, 0xab, 0x0b], // negative memory bit index
        vec![0x01, 0xd0, 0x89, 0x03, 0x03, 0x03, 0x9c, 0x5f], // RMW and RAM
        vec![0x01, 0xd0, 0xa3, 0, 0, 1, 0], // miss after ALU: preserve pre-store state
        vec![0x01, 0xd0, 0xa3, 0xff, 0x2f, 0, 0], // cross-page side exit
        vec![0x01, 0xd0, 0x8e, 0xe0, 0x89, 0x03], // segment handoff retains pre-load flags/PC/budget
        vec![0x01, 0xd0, 0x66, 0x8e, 0xd8, 0x89, 0x03], // word override still loads AX
        vec![0x01, 0xd0, 0x8e, 0x23, 0x89, 0x03], // FS selector from RAM
        vec![0x01, 0xd0, 0x8e, 0x25, 0xff, 0x2f, 0, 0], // split selector read exits before handoff
        vec![0x8b, 0x03, 0x89, 0x16, 0x8b, 0x4b, 4, 0x89, 0x46, 4], // interleaved read/write ranges
        vec![0x8b, 0x03, 0x8b, 0x1e, 0x8b, 0x4b, 4], // replacing the address base ends its proof
        vec![0xb9, 16, 0, 0, 0, 0x01, 0xd0, 0x49, 0x75, 0xfb, 0x9c, 0x5f], // backedge, split entries
        vec![0xb9, 16, 0, 0, 0, 0x83, 0xe9, 1, 0x75, 0xfb, 0x9c, 0x5f], // budget/final flags on a register loop
        vec![0xb9, 16, 0, 0, 0, 0x89, 0xd0, 0x49, 0x75, 0xfb, 0x9c, 0x5f], // DEC must preserve incoming carry
        vec![
            0x85, 0xc0, 0x74, 4, 0x01, 0xd0, 0xeb, 2, 0x29, 0xd0, 0x9c, 0x5f,
        ], // diamond
        vec![
            0xb9, 4, 0, 0, 0, 0xba, 3, 0, 0, 0, 0xa9, 1, 0, 0, 0, 0x74, 5, 0x83, 0xc0, 3, 0xeb, 3,
            0x83, 0xe8, 5, 0x4a, 0x75, 0xee, 0x49, 0x75, 0xe6, 0x9c, 0x5f,
        ], // nested countdowns; parity-dependent ALU path, flags and short budgets
        vec![
            0xb9, 4, 0, 0, 0, 0xa9, 1, 0, 0, 0, 0x74, 4, 0x01, 0xd0, 0xeb, 2, 0x29, 0xd0, 0x49,
            0x74, 9, 0xa9, 2, 0, 0, 0, 0x75, 0xf0, 0xeb, 0xf2, 0xba, 3, 0, 0, 0, 0x40, 0xeb, 0,
            0x4a, 0x75, 0xfa, 0x9c, 0x5f,
        ], // enter a cycle through either arm, then continue into a second cycle
    ]);
    // Both branch edges can either preserve flags for observation or kill
    // them with a later ALU. Include every condition and short-budget exits.
    for condition in 0..16 {
        if condition >= 12 {
            for prefix in [&[][..], &[0x66][..]] {
                let mut code = prefix.to_vec();
                code.extend([0x39, 0xd0, 0x70 | condition, 2, 0x40, 0x4a, 0x9c, 0x5f]);
                programs.push(code); // signed comparisons at dword/word/byte width
            }
            programs.push(vec![
                0x38,
                0xd0,
                0x70 | condition,
                2,
                0x40,
                0x4a,
                0x9c,
                0x5f,
            ]);
        }
        programs.push(vec![
            0x01,
            0xd0,
            0xeb,
            0,
            0x70 | condition,
            4,
            0x29,
            0xd0,
            0xeb,
            2,
            0x9c,
            0x5f,
            0x9c,
            0x5d,
        ]); // canonical incoming flag record at the conditional's join
        programs.push(vec![
            0x01,
            0xd0,
            0x70 | condition,
            4,
            0x29,
            0xd0,
            0xeb,
            2,
            0x9c,
            0x5f,
            0x9c,
            0x5d,
        ]);
        programs.push(vec![
            0x01,
            0xd0,
            0x70 | condition,
            4,
            0x9c,
            0x5f,
            0xeb,
            2,
            0x29,
            0xd0,
            0x9c,
            0x5d,
        ]);
    }
    // Only the non-carry arm was observed before compilation. Taking the cold
    // arm must preserve the branch's flags/EIP without performing its store.
    let deferred_branch = programs.len();
    programs.push(vec![
        0x01, 0xd0, 0x72, 6, 0x29, 0xd0, 0x9c, 0x5f, 0xeb, 5, 0x89, 0x03, 0x9c, 0x5f, 0x90,
    ]);
    // A zero-count REP cannot establish a segment proof for the following
    // memory access. FS remains dynamically guarded; the reference limit
    // rejects that access if executed. DS/ES/SS are qualified at core entry.
    let denied_segment = programs.len();
    programs.push(vec![0x90, 0x31, 0xc9, 0x64, 0xf3, 0xa5, 0x64, 0x8b, 0x03]);
    // A load followed by a store distinguishes read qualification from write
    // qualification, including the one-way user-to-supervisor implication.
    let privilege_proofs = programs.len();
    programs.push(vec![0x8b, 0x03, 0x89, 0x43, 4, 0x40]);
    let cmov_denied = programs.len();
    programs.push(vec![0x31, 0xc9, 0x0f, 0x45, 0x03, 0x40]); // false CMOVNE still reads
    // Neighboring operands are individually qualified on two resident pages.
    // Coalescing them into one wider page check must not force interpretation;
    // repeated exact operands still reuse their proofs across the other page.
    let split_range = programs.len();
    programs.push(vec![
        0xbb, 0xfc, 0x2f, 0, 0, 0x89, 0x13, 0x89, 0x4b, 4, 0x8b, 0x03, 0x03, 0x43, 4, 0x03, 0x03,
    ]);
    let mut continuation = vec![0x40; 300];
    continuation.extend_from_slice(&[0x49, 0x0f, 0x85, 0xcd, 0xfe, 0xff, 0xff]); // DEC ECX; JNZ first INC
    programs.push(continuation);
    let mut called = vec![0x40; 260];
    // A long callee spans emitted functions. Its return must still find the
    // caller, including when a short budget interrupts the direct tail chain.
    called[..11].copy_from_slice(&[0xe8, 11, 0, 0, 0, 0x9c, 0x5f, 0xeb, 2, 0x90, 0x90]);
    called[11] = 0xcc;
    called.push(0xc3);
    programs.push(called);
    let mut chained = vec![0xcc; 4102];
    chained[..11].copy_from_slice(&[0xb9, 16, 0, 0, 0, 0xe8, 0xf6, 0x0f, 0, 0, 0x40]);
    chained[4096..].copy_from_slice(&[0x01, 0xd0, 0x49, 0x75, 0xfb, 0xc3]);
    programs.push(chained);
    // Undefined shift/rotate flags follow the shared CPU model too. Values
    // straddle each operand's sign/carry boundary and CL includes zero/full width.
    let values = [
        0, 1, 0x7f, 0x80, 0xff, 0x7fff, 0x8000, 0xffff, 0x7fffffff, 0x80000000, 0xffffffff,
        0x12345678,
    ];
    let counts = [0, 1, 7, 8, 9, 16, 17, 31, 32, 33];
    let mut requests = String::from("{\"abi\":{");
    for (name, offset) in [
        ("regs", offset_of!(Frame, regs)),
        ("eip", offset_of!(Frame, eip)),
        ("eflags", offset_of!(Frame, eflags)),
        ("remaining", offset_of!(Frame, remaining)),
        ("exit", offset_of!(Frame, exit)),
        ("flag_lhs", offset_of!(Frame, flag_lhs)),
        ("flag_result", offset_of!(Frame, flag_result)),
        ("flag_kind", offset_of!(Frame, flag_kind)),
        ("tlb", offset_of!(Frame, tlb)),
        ("tlb_state", offset_of!(Frame, tlb_state)),
        ("permissions", offset_of!(Frame, permissions)),
        ("cs_base", offset_of!(Frame, cs_base)),
        ("popf_mask", offset_of!(Frame, popf_mask)),
        ("frame_size", size_of::<Frame>()),
        ("repeat_control", offset_of!(Frame, repeat.control)),
        ("repeat_completed", offset_of!(Frame, repeat.completed)),
        ("repeat_next", offset_of!(Frame, repeat.next_eip)),
        ("tlb_size", size_of::<LinearTranslationCacheEntry>()),
        ("tlb_tag", offset_of!(LinearTranslationCacheEntry, tag)),
        (
            "tlb_write_tag",
            offset_of!(LinearTranslationCacheEntry, write_tag),
        ),
        ("tlb_bias", offset_of!(LinearTranslationCacheEntry, bias)),
        ("tlb_bits", LINEAR_TRANSLATION_CACHE_BITS as usize),
        ("link_bytes", LINK_PLANES * LINK_SLOTS * size_of::<Link>()),
        ("link_size", size_of::<Link>()),
        ("entry_bits", ENTRY_BITS as usize),
        ("group_size", GROUP_FUNCTIONS),
        ("link_tag", offset_of!(Link, tag)),
        ("link_function", offset_of!(Link, function)),
        ("link_entries", offset_of!(Link, entries)),
        ("link_version_ptr", offset_of!(Link, version_ptr)),
        ("link_version", offset_of!(Link, version)),
    ] {
        requests.push_str(&format!("\"{name}\":{offset},"));
    }
    requests.pop();
    requests.push_str("},\"programs\":[");
    let mut cases = Vec::new();
    let mut incoming_cpu = Cpu::new();
    let fully_observed: Vec<u16> = (0..4096).collect();
    for (index, code) in programs.iter().enumerate() {
        requests.push_str(&format!("{{\"code\":{code:?},\"regions\":["));
        for (page_id, bytes) in code.chunks(4096).enumerate() {
            let mut page = vec![0xcc; 4096];
            page[..bytes.len()].copy_from_slice(bytes);
            let mut starts = vec![0];
            if code.len() < 4096 && code.first() == Some(&0xb9) {
                // Overlapping entries must be real instruction boundaries.
                let decoded = crate::x86::jit::decode::block(bytes, 0, 3);
                starts.extend(decoded.instructions.iter().skip(1).map(|i| i.eip as u16));
            }
            let region = compile::batch(
                &page,
                page_id as u32 * 4096,
                &starts,
                if index == deferred_branch {
                    &[0, 4]
                } else {
                    &fully_observed
                },
                &[],
                compile::Bindings {
                    frame: 0x1000,
                    entries: 0x50000 + page_id as u32 * 16384,
                    links: 0x60000,
                    recency: 0x78000 + page_id as u32 * 4,
                    slot: page_id as u32,
                },
                16,
            )
            .expect("supported test program");
            let entry = region.entries.iter().find(|&&(pc, _)| pc == 0).unwrap().1 - 1;
            requests.push_str(&format!(
                "{{\"module\":{:?},\"entry\":{entry},\"entries\":[",
                region.bytes
            ));
            for (at, id) in region.entries {
                requests.push_str(&format!("[{at},{id}],"));
            }
            requests.pop();
            requests.push_str("]},");
        }
        requests.pop();
        requests.push_str("]},");
        for (i, &value) in values.iter().enumerate() {
            for (j, &count) in counts.iter().enumerate() {
                if code.starts_with(&[0xb9, 0x88, 0x13]) && (i > 2 || j > 3) {
                    continue;
                }
                let regs = [
                    value,
                    count,
                    values[(i + j + 3) % values.len()],
                    0x2000,
                    0xf000,
                    0xe000,
                    0x2200,
                    0x2400,
                ];
                let flags = 2
                    | if (i + j) & 1 == 0 { 0x8d5 } else { 0 }
                    | if (i + j) & 2 == 0 { 0x400 } else { 0 }
                    | if (i + j) & 4 == 0 { 0x200 } else { 0 };
                let budget = if index == privilege_proofs || index == cmov_denied {
                    99
                } else if j == 0 {
                    1
                } else if j == 1 {
                    3
                } else {
                    if code.len() > 256 && code.len() < 4096 {
                        600
                    } else {
                        99
                    }
                };
                let (record, effective_flags) = incoming_flags(
                    &mut incoming_cpu,
                    flags,
                    value,
                    regs[2],
                    index + i * counts.len() + j,
                );
                let (proofs, expected) = if index == split_range && budget == 99 {
                    ([2, 2, 2], Some(6))
                } else if index == cmov_denied {
                    ([3, 2, 2], Some(1))
                } else if index == privilege_proofs {
                    let (proofs, maximum) = [
                        ([2, 3, 3], 3), // supervisor uses user read/write
                        ([3, 2, 2], 0), // user cannot use supervisor read
                        ([2, 3, 0], 1), // no write proof at either privilege
                        ([3, 3, 2], 1), // user cannot use supervisor write
                        ([3, 3, 3], 3), // ordinary user read/write
                        ([2, 5, 5], 0), // old generation never grants access
                    ][j % 6];
                    (proofs, Some(maximum.min(budget)))
                } else {
                    ([2, 2, 2], None)
                };
                cases.push((
                    index,
                    regs,
                    effective_flags,
                    budget,
                    record,
                    proofs,
                    expected,
                ));
            }
        }
    }
    requests.pop();
    requests.push_str("],\"cases\":[");
    for (index, regs, _flags, budget, record, proofs, _expected) in &cases {
        let permissions = if *index == denied_segment {
            0x3f7f & !(1 << 4)
        } else {
            0x3f7f
        };
        requests.push_str(&format!(
            "[{index},{regs:?},{record:?},{budget},{permissions},{proofs:?}],"
        ));
    }
    requests.pop();
    requests.push_str("]}");
    let input = std::env::temp_dir().join(format!("r86-wasm-check-{}.json", std::process::id()));
    std::fs::write(&input, requests).unwrap();
    let output = Command::new("node")
        .arg("script/wasm_jit_check.cjs")
        .arg(&input)
        .stdin(Stdio::null())
        .output()
        .expect("Node.js is required for Wasm execution checks");
    std::fs::remove_file(input).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8(output.stdout).unwrap();
    assert_eq!(result.lines().count(), cases.len());
    for (line, (index, regs, flags, budget, _record, proofs, expected)) in result.lines().zip(cases)
    {
        let words: Vec<u32> = line
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        let mut initial = vec![0; 65536];
        for at in 0x2200..0x2600 {
            initial[at] = (at * 37 ^ at >> 4) as u8;
        }
        initial[..programs[index].len()].copy_from_slice(&programs[index]);
        initial[programs[index].len()] = 0xcc;
        let mut ram = Ram(initial.clone());
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);
        cpu.regs = regs;
        cpu.replace_eflags(flags);
        if index == denied_segment {
            cpu.set_seg_cache(
                crate::x86::SegmentReg::Fs,
                crate::x86::cpu::SegmentCache::new(16, 0, 0x1000, 0xc093),
            );
        }
        assert!(words[10] <= budget);
        let executed = budget - words[10];
        if let Some(expected) = expected {
            if index == split_range {
                assert_eq!(
                    executed, expected,
                    "resident operands on both pages must stay native"
                );
            } else if expected == 3 {
                assert_eq!(executed, expected, "qualified privilege proofs {proofs:?}");
            } else {
                assert!(executed <= expected, "denied privilege proofs {proofs:?}");
                assert_eq!(
                    words[11], 1,
                    "denied access must leave before the instruction"
                );
                assert_eq!(words[8], if executed == 0 { 0 } else { 2 });
            }
        }
        if programs[index].len() > 4096 && budget == 99 {
            assert!(executed > 40, "cross-module tail chain did not execute");
        }
        for _ in 0..executed {
            Interpreter::step(&mut cpu, &mut ram).unwrap();
        }
        let label = format!(
            "code={:02x?} input={regs:x?} flags={flags:x} budget={budget} executed={executed}",
            programs[index]
        );
        let mut actual_ram = Ram(initial);
        for pair in words[18..].chunks_exact(2) {
            actual_ram.0[pair[0] as usize] = pair[1] as u8;
        }
        let mut actual_cpu = Cpu::new();
        actual_cpu.enter_protected_flat(words[8], 0);
        actual_cpu.regs.copy_from_slice(&words[..8]);
        Frame {
            eflags: words[9],
            flag_lhs: words[15],
            flag_result: words[16],
            flag_kind: words[17],
            ..Frame::default()
        }
        .commit_flags(&mut actual_cpu);
        if words[11] == 6 {
            assert_eq!(cpu.eip, words[8], "partial REP must retain its EIP");
            let continuation = crate::x86::interp::RepeatContinuation {
                control: words[12],
                completed: words[13],
                next_eip: words[14],
            };
            let resumed = Interpreter::resume_repeat(
                &mut actual_cpu,
                &mut actual_ram,
                u64::from(executed),
                &continuation,
            );
            assert!(
                matches!(resumed.stop, crate::x86::BatchStop::Budget),
                "{resumed:?} {label}"
            );
            Interpreter::step(&mut cpu, &mut ram).unwrap();
        }
        assert_eq!(actual_cpu.regs, cpu.regs, "{label}");
        assert_eq!(actual_cpu.eip, cpu.eip, "{label}");
        assert_eq!(
            actual_cpu.effective_eflags(),
            cpu.effective_eflags(),
            "{label}"
        );
        assert_eq!(actual_ram.0, ram.0, "RAM {label}");
        assert!(
            executed > 0
                || matches!(words[11], 2 | 6)
                || expected.is_some_and(|maximum| maximum < 3),
            "empty non-budget exit: {label}"
        );
    }
}
