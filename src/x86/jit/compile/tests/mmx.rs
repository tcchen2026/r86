//! Exercise generated calls, native operands and the exact interpreter boundary.

use super::*;
use crate::x86::cpu::{CR0_EM, CR0_PG, CR0_TS, CR0_WP, SegmentCache, SegmentReg};
use crate::x86::interp::{BatchStepStop, StepError};
use crate::x86::{BackingToken, LinearAddr, PhysRegionKind, mmu};

const BINARY: &[u8] = &[
    0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x74, 0x75, 0x76, 0xd1,
    0xd2, 0xd3, 0xd5, 0xd8, 0xd9, 0xdb, 0xdc, 0xdd, 0xdf, 0xe1, 0xe2, 0xe5, 0xe8, 0xe9, 0xeb, 0xec,
    0xed, 0xef, 0xf1, 0xf2, 0xf3, 0xf5, 0xf8, 0xf9, 0xfa, 0xfc, 0xfd, 0xfe,
];

struct Memory(Vec<u8>);

impl Memory {
    fn word(&mut self, at: usize, value: u32) {
        self.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
}

impl Bus for Memory {
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        self.0
            .get(addr.0 as usize)
            .map(|_| ())
            .ok_or(MemError::OutOfRange { addr })
    }
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.0
            .get(addr.0 as usize)
            .copied()
            .ok_or(MemError::OutOfRange { addr })
    }
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        *self
            .0
            .get_mut(addr.0 as usize)
            .ok_or(MemError::OutOfRange { addr })? = value;
        Ok(())
    }
    fn cached_backing_token(&self, addr: PhysAddr) -> BackingToken {
        BackingToken(self.0.as_ptr() as u64 + addr.0)
    }
    fn cached_backing_phys(&self, token: BackingToken) -> PhysAddr {
        PhysAddr(token.0 - self.0.as_ptr() as u64)
    }
    fn read_backing_token_u8(&mut self, token: BackingToken) -> u8 {
        self.read_phys_u8(self.cached_backing_phys(token)).unwrap()
    }
    fn read_backing_token_u16(&mut self, token: BackingToken) -> u16 {
        self.read_phys_u16(self.cached_backing_phys(token)).unwrap()
    }
    fn read_backing_token_u32(&mut self, token: BackingToken) -> u32 {
        self.read_phys_u32(self.cached_backing_phys(token)).unwrap()
    }
    fn classify_phys(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        if addr.0 + u64::from(len) <= self.0.len() as u64 {
            PhysRegionKind::Ram
        } else {
            PhysRegionKind::Mmio
        }
    }
    fn jit_page_table_source(&self, addr: PhysAddr) -> Option<BackingToken> {
        ((addr.0 & !4095) + 4096 <= self.0.len() as u64).then(|| self.cached_backing_token(addr))
    }
    fn linear_translation_cache_enabled(&self) -> bool {
        true
    }
}

unsafe extern "C" fn refill(view: *mut soft::View, tag: u32, write: u32) -> u64 {
    // SAFETY: invoke supplies exclusive, live fixture owners.
    let view = unsafe { &mut *view };
    let linear = LinearAddr(tag & !4095);
    let token = unsafe {
        mmu::try_resident_data_translation(
            &mut *view.cpu,
            &mut *view.bus.cast::<Memory>(),
            linear,
            write != 0,
        )
    };
    let bias = token.map_or(0, |token| token.0.wrapping_sub(u64::from(linear.0)));
    view.remember_refill(tag, write != 0, bias);
    bias
}

fn invoke(code: &Fixture, cpu: &mut Cpu, memory: &mut Memory, budget: u64) -> Frame {
    let mut frame = Frame::from_cpu(cpu, budget);
    let mut view = soft::View::new(cpu, true);
    view.bus = (memory as *mut Memory).cast();
    view.refill = refill as *const () as usize;
    view.mmx = crate::x86::jit::runtime::execute_mmx as *const () as usize;
    frame.memory = (&mut view as *mut soft::View).cast();
    code.invoke(&mut frame);
    frame.commit(cpu);
    frame.memory = core::ptr::null_mut();
    frame
}

fn setup(bytes: &[u8], seed: u32) -> (Cpu, Memory) {
    let mut cpu = initial(seed);
    cpu.cr0 |= CR0_PG | CR0_WP;
    cpu.cr3 = 0x4000;
    cpu.cr2 = 0xdead_beef;
    let mut random = u64::from(seed) + 0x1375_ffe8_1432_acdf;
    for value in &mut cpu.fpu.mm {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        *value = random;
    }
    cpu.fpu.top = 7;
    cpu.fpu.status = 0x425a;
    cpu.fpu.mmx_active = seed & 1 != 0;
    cpu.fpu.st = core::array::from_fn(|i| i as f64 - 3.25);
    let mut memory = Memory(vec![0; 0x10000]);
    memory.word(0x4000, 0x5067);
    memory.word(0x5000, 0x2067);
    memory.word(0x5018, 0x8067);
    memory.word(0x501c, 0xb067);
    memory.0[0x2100..0x2100 + bytes.len()].copy_from_slice(bytes);
    for page in [0x8000, 0xb000] {
        for byte in &mut memory.0[page..page + 4096] {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            *byte = random as u8;
        }
    }
    (cpu, memory)
}

fn same(actual: &Cpu, expected: &Cpu) {
    assert_eq!(actual.regs, expected.regs);
    assert_eq!(actual.eip, expected.eip);
    assert_eq!(actual.effective_eflags(), expected.effective_eflags());
    assert_eq!(actual.tsc(), expected.tsc());
    assert_eq!(actual.cr2, expected.cr2);
    assert_eq!(actual.fpu.mm, expected.fpu.mm);
    assert_eq!(actual.fpu.top, expected.fpu.top);
    assert_eq!(actual.fpu.mmx_active, expected.fpu.mmx_active);
    assert_eq!(actual.fpu.status, expected.fpu.status);
    assert_eq!(actual.fpu.control, expected.fpu.control);
    assert_eq!(actual.fpu.mxcsr, expected.fpu.mxcsr);
    assert_eq!(
        actual.fpu.st.map(f64::to_bits),
        expected.fpu.st.map(f64::to_bits)
    );
}

fn compare_mmx(bytes: &[u8], seeds: u32, based: bool) {
    let decoded = decode::block(bytes, 0x100, 64);
    assert_eq!(decoded.byte_len, bytes.len(), "{bytes:02x?}");
    let count = decoded.instructions.len() as u64;
    let code = Fixture::new(&decoded);
    for seed in 0..seeds {
        let (mut expected, mut expected_bus) = setup(bytes, seed);
        let (mut actual, mut actual_bus) = setup(bytes, seed);
        for cpu in [&mut expected, &mut actual] {
            if seed & 2 != 0 {
                cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(11, 0xc0fb));
                for segment in [
                    SegmentReg::Ds,
                    SegmentReg::Es,
                    SegmentReg::Ss,
                    SegmentReg::Fs,
                ] {
                    cpu.set_seg_cache(segment, SegmentCache::flat(19, 0xc0f3));
                }
            }
            // SIB [EBX + ESI*2 + 7], with unaligned and end-of-page operands.
            cpu.regs[3] = (if seed & 1 == 0 { 0x6021 } else { 0x6ff8 }) - 13;
            cpu.regs[6] = 3;
            if based {
                cpu.regs[3] -= 0x6000;
                cpu.set_seg_cache(
                    SegmentReg::Fs,
                    SegmentCache {
                        base: 0x6000,
                        limit: 0xfff,
                        ..cpu.seg(SegmentReg::Fs)
                    },
                );
            }
        }
        let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, 0, count);
        assert_eq!(reference.stop, BatchStop::Budget, "{bytes:02x?}");
        let frame = invoke(&code, &mut actual, &mut actual_bus, count);
        assert_eq!(frame.exit_kind, 0, "{bytes:02x?}");
        assert_eq!(
            frame.completed, count,
            "must execute natively: {bytes:02x?}"
        );
        assert_eq!(frame.remaining, 0);
        same(&actual, &expected);
        assert_eq!(actual_bus.0, expected_bus.0);
    }
    // A short budget must leave all MMX/x87 and integer state untouched.
    let (mut actual, mut memory) = setup(bytes, 4);
    let (expected, _) = setup(bytes, 4);
    assert_eq!(
        invoke(&code, &mut actual, &mut memory, count - 1).completed,
        0
    );
    same(&actual, &expected);
}

#[test]
fn native_mmx_register_helpers_match_shared_semantics_and_resident_flags() {
    for opcode in BINARY.iter().copied().chain([0x6e, 0x6f, 0x7e, 0x7f]) {
        for (dst, src) in [(0, 1), (7, 3), (2, 2), (4, 0)] {
            compare_mmx(
                &[0x40, 0x0f, opcode, 0xc0 | dst << 3 | src, 0x11, 0xf8],
                24,
                false,
            );
        }
    }
    compare_mmx(&[0x40, 0x0f, 0x77, 0x11, 0xf8], 4, false);
    for opcode in 0x71..=0x73 {
        for group in [2, 4, 6] {
            if opcode == 0x73 && group == 4 {
                continue;
            }
            for count in [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 255] {
                compare_mmx(
                    &[0x40, 0x0f, opcode, 0xc3 | group << 3, count, 0x11, 0xf8],
                    8,
                    false,
                );
            }
        }
    }
}

#[test]
fn native_mmx_memory_helpers_preserve_qwords_segments_and_refill_state() {
    for opcode in BINARY.iter().copied().chain([0x6e, 0x6f, 0x7e, 0x7f]) {
        for based in [false, true] {
            let mut bytes = vec![0x40];
            if based {
                bytes.push(0x64);
            }
            bytes.extend([0x0f, opcode, 0x5c, 0x73, 7, 0x11, 0xf8]);
            compare_mmx(&bytes, 16, based);
        }
    }
    // Chained helpers must observe each other's FPU writes and preserve the
    // fixed translation registers for a subsequent integer memory operation.
    compare_mmx(
        &[
            0x40, 0x0f, 0x6f, 0xc1, 0x0f, 0xfd, 0xc0, 0x0f, 0x7f, 0x44, 0x73, 7, 0x8b, 0x54, 0x73,
            7, 0x0f, 0x77, 0x11, 0xf8,
        ],
        24,
        false,
    );
}

#[test]
fn native_mmx_split_operands_resume_without_partial_native_effects() {
    for opcode in [0x6f, 0xdb, 0x7f] {
        for address in [0x6ff9, 0x6ffc, 0x6fff] {
            let bytes = [0x40, 0x0f, opcode, 0x03, 0x11, 0xf8];
            let code = Fixture::new(&decode::block(&bytes, 0x100, 64));
            let (mut expected, mut expected_bus) = setup(&bytes, 11);
            let (mut actual, mut actual_bus) = setup(&bytes, 11);
            actual.regs[3] = address;
            expected.regs[3] = address;
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 3).stop,
                BatchStop::Budget
            );
            let before = actual_bus.0.clone();
            let before_mm = actual.fpu.mm;
            let frame = invoke(&code, &mut actual, &mut actual_bus, 3);
            assert_eq!(frame.completed, 1);
            assert_eq!(frame.eip, 0x101);
            assert_eq!(actual.fpu.mm, before_mm);
            assert_eq!(actual.fpu.top, 7);
            assert_eq!(actual_bus.0, before);
            assert_eq!(
                Interpreter::run_batch(&mut actual, &mut actual_bus, 1, 2).stop,
                BatchStop::Budget
            );
            same(&actual, &expected);
            assert_eq!(actual_bus.0, expected_bus.0);
        }
    }
}

fn fault_case(bytes: &[u8], control: u32, high_flags: u32, short_segment: bool, error: StepError) {
    let decoded = decode::block(bytes, 0x100, 64);
    assert_eq!(decoded.byte_len, bytes.len());
    let code = Fixture::new(&decoded);
    let make = || {
        let (mut cpu, mut bus) = setup(bytes, 23);
        cpu.cr0 |= control;
        cpu.regs[3] = 0x6ffc;
        if short_segment {
            cpu.set_seg_cache(
                SegmentReg::Ds,
                SegmentCache {
                    limit: 0x6fff,
                    ..cpu.seg(SegmentReg::Ds)
                },
            );
        }
        bus.word(0x501c, 0xb000 | high_flags);
        (cpu, bus)
    };
    let (mut expected, mut expected_bus) = make();
    let (mut actual, mut actual_bus) = make();
    let before_mm = actual.fpu.mm;
    let before_low = actual_bus.0[0x8ffc..0x9000].to_vec();
    let before_high = actual_bus.0[0xb000..0xb004].to_vec();
    let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 3);
    let frame = invoke(&code, &mut actual, &mut actual_bus, 3);
    assert_eq!(frame.completed, 1);
    assert_eq!(frame.eip, 0x101);
    assert_eq!(actual.fpu.mm, before_mm);
    assert_eq!(actual.fpu.top, 7);
    assert_eq!(&actual_bus.0[0x8ffc..0x9000], before_low);
    assert_eq!(&actual_bus.0[0xb000..0xb004], before_high);
    let resumed = Interpreter::run_batch(
        &mut actual,
        &mut actual_bus,
        frame.completed,
        frame.remaining,
    );
    assert_eq!(resumed.stop, reference.stop);
    assert_eq!(resumed.ran + frame.completed, reference.ran);
    assert!(
        matches!(resumed.stop, BatchStop::Step { instruction_eip: 0x101, reason: BatchStepStop::Error(e), .. } if e == error),
        "{:?}",
        resumed.stop
    );
    same(&actual, &expected);
    assert_eq!(actual_bus.0, expected_bus.0);
    assert_eq!(actual.fpu.mm, before_mm);
    assert_eq!(&actual_bus.0[0x8ffc..0x9000], before_low);
    assert_eq!(&actual_bus.0[0xb000..0xb004], before_high);
}

#[test]
fn native_mmx_cr0_exits_precede_operand_faults_and_keep_exact_eip() {
    for control in [CR0_TS, CR0_EM, CR0_TS | CR0_EM] {
        let error = if control & CR0_EM != 0 {
            StepError::InvalidOpcode { eip: 0x101 }
        } else {
            StepError::DeviceNotAvailable { eip: 0x101 }
        };
        for opcode in [0x6e, 0x6f, 0x7e, 0x7f, 0xdb] {
            for operand in [0xc1, 0x03] {
                fault_case(
                    &[0x40, 0x0f, opcode, operand, 0x31, 0xc0],
                    control,
                    0,
                    false,
                    error,
                );
            }
        }
        fault_case(&[0x40, 0x0f, 0x77, 0x31, 0xc0], control, 0, false, error);
        fault_case(
            &[0x40, 0x0f, 0x71, 0xd0, 4, 0x31, 0xc0],
            control,
            0,
            false,
            error,
        );
    }
}

#[test]
fn native_mmx_high_page_and_segment_faults_do_not_commit_store_or_fpu_state() {
    for (opcode, high_flags, short_segment, error_code) in [
        (0x6f, 0, false, 0),
        (0xdb, 0, false, 0),
        (0x7f, 0, false, 2),
        (0x7f, 0x65, false, 3),
        (0x6f, 0, true, 0),
        (0x7f, 0, true, 0),
    ] {
        let error = if short_segment {
            StepError::GeneralProtection {
                eip: 0x101,
                code: 0,
            }
        } else {
            StepError::PageFault(mmu::PageFault {
                linear: LinearAddr(0x7000),
                code: error_code,
            })
        };
        fault_case(
            &[0x40, 0x0f, opcode, 0x03, 0x31, 0xc0],
            0,
            high_flags,
            short_segment,
            error,
        );
    }
}
