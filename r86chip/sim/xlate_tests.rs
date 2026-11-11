//! Translator words against actual shared-interpreter instructions, not flag formulas.
use super::*;
use r86::x86::{Bus, Cpu, Interpreter, MemError, PhysAddr};

const EDGES: [u32; 10] = [
    0, 1, 0x7f, 0x80, 0xff, 0x7fff, 0x8000, 0x7fffffff, 0x80000000, 0xffffffff,
];
const DATA: usize = 0x4000;

struct Ram(Vec<u8>);
impl Bus for Ram {
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.0
            .get(addr.0 as usize)
            .copied()
            .ok_or(MemError::OutOfRange { addr })
    }
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        self.0
            .get(addr.0 as usize)
            .map(|_| ())
            .ok_or(MemError::OutOfRange { addr })
    }
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        *self
            .0
            .get_mut(addr.0 as usize)
            .ok_or(MemError::OutOfRange { addr })? = value;
        Ok(())
    }
}

fn random(seed: &mut u32) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed
}

fn operands() -> Vec<u32> {
    let mut seed = 0x8632_9811;
    EDGES
        .into_iter()
        .chain((0..384).map(|_| random(&mut seed)))
        .collect()
}

fn i(op: u32, f: u32, rd: u32, rs1: u32, imm: i32) -> u32 {
    op | rd << 7 | f << 12 | rs1 << 15 | (imm as u32 & 0xfff) << 20
}

fn s(op: u32, f: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
    let n = imm as u32;
    op | (n & 31) << 7 | f << 12 | rs1 << 15 | rs2 << 20 | ((n >> 5) & 127) << 25
}

fn regs(a: u32) -> [u32; 8] {
    [
        a,
        0xa513_24c7,
        0x55aa_55aa,
        0x4080,
        0x4800,
        0x4830,
        0x8102_037f,
        0x7654_3210,
    ]
}

struct Pair {
    native: Core,
    memory: Memory,
    oracle: Cpu,
    ram: Ram,
    word: u32,
    bytes: Vec<u8>,
    input: [u32; 8],
    flags: u32,
}

impl Pair {
    fn new(word: u32, bytes: &[u8], regs: [u32; 8], flags: u32) -> Self {
        let mut native = Core::new(0x1000);
        let mut memory = Memory::new(0x6000);
        // Publish real context/segment/mapping words. Guest RAM is identity
        // mapped so both engines can compare the entire backing, including
        // code, unused bytes, and store canaries, after every instruction.
        for (f, rd, a, b, k) in [
            (4, 0, 0x4000, 0x5000, 0),
            (5, 0, 1, 1, 0),
            (6, 2, 0, u32::MAX, 3),
            (6, 3, 0, u32::MAX, 3),
            (1, 0, 0x4000, 0x4003, 0),
        ] {
            native.x[1] = a;
            native.x[2] = b;
            let setup: u32 = 0x2b | rd << 7 | f << 12 | 1 << 15 | 2 << 20 | k << 25;
            memory.write(native.pc, &setup.to_le_bytes()).unwrap();
            assert_eq!(native.step(&mut memory).unwrap(), None);
        }
        memory.write(native.pc, &word.to_le_bytes()).unwrap();
        memory.write(0x2000, bytes).unwrap();
        for (n, byte) in memory.ram[DATA..DATA + 4096].iter_mut().enumerate() {
            *byte = (n as u8).wrapping_mul(37).wrapping_add(0xa5);
        }
        native.x[8..16].copy_from_slice(&regs);
        native.flags = flags & STATUS_MASK;
        native.pending_flags = (flags ^ STATUS_MASK) & STATUS_MASK;
        let mut oracle = Cpu::new();
        oracle.enter_protected_flat(0x2000, regs[4]);
        oracle.regs = regs;
        oracle.replace_eflags(0x202 | (flags & STATUS_MASK));
        let ram = Ram(memory.ram.clone());
        Self {
            native,
            memory,
            oracle,
            ram,
            word,
            bytes: bytes.to_vec(),
            input: regs,
            flags,
        }
    }

    fn data(&mut self, address: usize, value: u32, width: usize) {
        let data = value.to_le_bytes();
        self.memory.ram[address..address + width].copy_from_slice(&data[..width]);
        self.ram.0[address..address + width].copy_from_slice(&data[..width]);
    }

    fn compare(mut self, report: &mut Report, tail: Option<u32>) {
        report.cases += 1;
        if let Some(word) = tail {
            // Keep even the extra native instruction bytes equal in both RAMs.
            let at = self.native.pc + 4;
            self.memory.write(at, &word.to_le_bytes()).unwrap();
            self.ram.0[at as usize..at as usize + 4].copy_from_slice(&word.to_le_bytes());
        }
        let pending = self.native.pending_flags;
        let mut trap = self.native.step(&mut self.memory).unwrap();
        if trap.is_none() && tail.is_some() {
            trap = self.native.step(&mut self.memory).unwrap();
        }
        Interpreter::step(&mut self.oracle, &mut self.ram).unwrap();
        let flags = self.oracle.effective_eflags() & STATUS_MASK;
        let zero_shift = self.word & 0x7f == 0x3b
            && (if self.word >> 25 == 1 {
                (self.word >> 20) & 31
            } else {
                self.input[1] & 31
            }) == 0;
        let memory_difference = self
            .memory
            .ram
            .iter()
            .zip(&self.ram.0)
            .enumerate()
            .find(|(_, (a, b))| a != b);
        if trap.is_some()
            || self.native.x[0] != 0
            || self.native.x[8..16] != self.oracle.regs
            || self.native.flags != flags
            || memory_difference.is_some()
            || (zero_shift && self.native.pending_flags != pending)
        {
            report.failures.push(format!(
                "word={:08x} x86={:02x?} input={:08x?} status={:03x}: \
                 native regs={:08x?} flags={:03x}, x86 regs={:08x?} flags={:03x}; \
                 first memory difference={memory_difference:02x?}, trap={trap:?}, \
                 pending before={pending:03x} after={:03x}",
                self.word,
                self.bytes,
                self.input,
                self.flags,
                &self.native.x[8..16],
                self.native.flags,
                self.oracle.regs,
                flags,
                self.native.pending_flags,
            ));
        }
    }
}

#[derive(Default)]
struct Report {
    cases: usize,
    failures: Vec<String>,
}
impl Report {
    fn finish(self, name: &str) {
        println!(
            "{name}: {} comparisons, {} disagreements",
            self.cases,
            self.failures.len()
        );
        // Finish all inputs before failing, so a first flag mismatch cannot
        // conceal disagreements in other kinds, counts, or operand boundaries.
        assert!(
            self.failures.is_empty(),
            "{name}:\n{}",
            self.failures.join("\n")
        );
    }
}

#[test]
fn xalui_matches_81_83_cmp_and_test() {
    let mut report = Report::default();
    let mut pairs: Vec<_> = EDGES
        .into_iter()
        .flat_map(|a| {
            [-2048, -129, -128, -1, 0, 1, 127, 128, 2047]
                .into_iter()
                .map(move |b| (a, b))
        })
        .collect();
    let mut seed = 0x1234_8632;
    pairs.extend((0..384).map(|_| (random(&mut seed), (random(&mut seed) as i32) >> 20)));
    for (a, imm) in pairs {
        for cf in [0, 1] {
            let flags = ((a ^ imm as u32) & STATUS_MASK & !1) | cf;
            for kind in 0..7 {
                for opcode in [0x81, 0x83] {
                    if opcode == 0x83 && !(-128..=127).contains(&imm) {
                        continue;
                    }
                    let mut bytes = vec![opcode, 0xc0 | (kind as u8) << 3];
                    if opcode == 0x83 {
                        bytes.push(imm as u8);
                    } else {
                        bytes.extend(imm.to_le_bytes());
                    }
                    Pair::new(i(0x1b, kind, 8, 8, imm), &bytes, regs(a), flags)
                        .compare(&mut report, None);
                    if kind == 5 {
                        bytes[1] = 0xf8; // CMP EAX, imm: same SUB status, no result write.
                        Pair::new(i(0x1b, kind, 0, 8, imm), &bytes, regs(a), flags)
                            .compare(&mut report, None);
                    }
                }
                if kind == 4 {
                    let mut bytes = vec![0xf7, 0xc0]; // TEST EAX, imm32
                    bytes.extend(imm.to_le_bytes());
                    Pair::new(i(0x1b, kind, 0, 8, imm), &bytes, regs(a), flags)
                        .compare(&mut report, None);
                }
            }
        }
    }
    report.finish("XALUI/CMP/TEST");
}

#[test]
fn xinc_xdec_match_register_inc_dec_preserving_carry() {
    let mut report = Report::default();
    for a in operands() {
        for cf in [0, 1] {
            for (imm, opcode) in [(1, 0x40), (-1, 0x48)] {
                Pair::new(
                    i(0x1b, 7, 8, 8, imm),
                    &[opcode],
                    regs(a),
                    (STATUS_MASK & !1) | cf,
                )
                .compare(&mut report, None);
            }
        }
    }
    report.finish("XINC/XDEC");
}

#[test]
fn xsh_matches_immediate_and_cl_including_masked_zero() {
    let mut report = Report::default();
    for a in operands() {
        for (kind, group) in [4, 5, 7, 0, 1].into_iter().enumerate() {
            for flags in [0, 1, STATUS_MASK & !1, STATUS_MASK] {
                for (immediate, counts) in [
                    (true, &[0, 1, 2, 31][..]),
                    (false, &[0, 1, 2, 31, 32, 33, 63, 64, 255, 0x12345620][..]),
                ] {
                    for &count in counts {
                        let mut input = regs(a);
                        input[1] = count;
                        let rs2 = if immediate { count } else { 9 };
                        let word = 0x3b
                            | 8 << 7
                            | (kind as u32) << 12
                            | 8 << 15
                            | rs2 << 20
                            | u32::from(immediate) << 25;
                        let mut bytes =
                            vec![if immediate { 0xc1 } else { 0xd3 }, 0xc0 | group << 3];
                        if immediate {
                            bytes.push(count as u8);
                        }
                        Pair::new(word, &bytes, input, flags).compare(&mut report, None);
                    }
                }
            }
        }
    }
    report.finish("XSH");
}

fn memory_instruction(prefix: &[u8], opcode: &[u8], imm: i32) -> Vec<u8> {
    let mut bytes = prefix.to_vec();
    bytes.extend(opcode);
    bytes.push(0x83); // EAX/AX/AL, [EBX + disp32]
    bytes.extend(imm.to_le_bytes());
    bytes
}

#[test]
fn xldd_matches_movsx_movzx_mov_inside_a_page() {
    let mut report = Report::default();
    for (n, a) in operands().into_iter().enumerate() {
        for ss in [false, true] {
            for (kind, width, opcode) in [
                (0, 1, &[0x0f, 0xbe][..]),
                (1, 2, &[0x0f, 0xbf][..]),
                (2, 4, &[0x8b][..]),
                (3, 4, &[0x8b][..]),
                (4, 1, &[0x0f, 0xb6][..]),
                (5, 2, &[0x0f, 0xb7][..]),
                (6, 1, &[0x0f, 0xb6][..]),
                (7, 2, &[0x0f, 0xb7][..]),
            ] {
                for off in [0, 1, 2, 3, 4096 - width] {
                    let address = DATA + off;
                    let imm = [-2048, -1, 0, 1, 2047][n % 5];
                    let mut input = regs(!a);
                    input[3] = (address as u32).wrapping_add_signed(-imm);
                    let bytes = memory_instruction(if ss { &[0x36] } else { &[] }, opcode, imm);
                    let mut pair = Pair::new(
                        i(if ss { 0x47 } else { 0x07 }, kind, 8, 11, imm),
                        &bytes,
                        input,
                        a & STATUS_MASK,
                    );
                    pair.data(address, a, width);
                    pair.compare(&mut report, None);
                }
            }
        }
    }
    report.finish("XLDD");
}

#[test]
fn xswd_matches_byte_word_dword_mov_inside_a_page() {
    let mut report = Report::default();
    for (n, a) in operands().into_iter().enumerate() {
        for ss in [false, true] {
            for kind in 0..3 {
                let width = 1 << kind;
                for off in [0, 1, 2, 3, 4096 - width] {
                    let address = DATA + off;
                    let imm = [-2048, -1, 0, 1, 2047][n % 5];
                    let mut input = regs(a);
                    input[3] = (address as u32).wrapping_add_signed(-imm);
                    let mut prefix = if ss { vec![0x36] } else { vec![] };
                    if kind == 1 {
                        prefix.push(0x66);
                    }
                    let bytes =
                        memory_instruction(&prefix, &[if kind == 0 { 0x88 } else { 0x89 }], imm);
                    Pair::new(
                        s(if ss { 0x77 } else { 0x27 }, kind, 11, 8, imm),
                        &bytes,
                        input,
                        a & STATUS_MASK,
                    )
                    .compare(&mut report, None);
                }
            }
        }
    }
    report.finish("XSWD");
}

#[test]
fn xswd_update_matches_push_register_including_esp() {
    let mut report = Report::default();
    for a in operands() {
        for lane in 0..4 {
            for esp in [false, true] {
                let mut input = regs(a);
                input[4] += lane;
                Pair::new(
                    s(0x77, 6, 12, if esp { 12 } else { 8 }, -4),
                    &[if esp { 0x54 } else { 0x50 }],
                    input,
                    a & STATUS_MASK,
                )
                .compare(&mut report, None);
            }
        }
    }
    report.finish("XSWD.U/PUSH");
}

#[test]
fn xpushret_and_xjt_return_match_push_immediate_stack_effect() {
    let mut report = Report::default();
    for a in operands() {
        for lane in 0..4 {
            let mut input = regs(a);
            input[4] += lane;
            let mut bytes = vec![0x68];
            bytes.extend(a.to_le_bytes());
            // XJT.R pops only the native return record. It must not pop the
            // guest stack again; the recorded jump has no x86 counterpart.
            Pair::new(s(0x77, 7, 12, 8, 8), &bytes, input, a & STATUS_MASK)
                .compare(&mut report, Some(i(0x6b, 0, 0, 8, 1)));
        }
    }
    report.finish("XPUSHRET/XJT.R stack");
}
