//! Guest memory effects and native fault locations, independent of MMU lowering.
//!
//! The decoder names an x86 access; the lowering preserves segment checks,
//! effective-address wrapping, access width/privilege, A/D effects and the
//! commit boundary.
//!
//! SoftMMU handles semantic misses with the shared page walker/access helper.
//! Native guards admit complete permitted RAM ranges and explicitly qualified
//! linear-framebuffer ranges with dirty notification. Code promotion revokes
//! writable aliases; ordinary page-table RAM stores obey TLB lifetime rules.
//!
//! A fault site is immediately before an architecturally faultable ARM64 access.
//! Pending x86 destination/flag updates are not committed before it. RMW and
//! split writes need explicit probe/commit ordering, not retry of arbitrary code.
//! Neither host pointers nor Rust enum representations appear in this record.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FaultSite {
    /// Byte offset in the emitted block; identifies the actual faulting access.
    pub native_offset: u32,
    pub guest_eip: u32,
    /// Signed adjustment to Frame.remaining (initial budget) minus live X27.
    /// The block has reserved all its steps: subtract its uncompleted suffix
    /// to recover exact prior retirement, then apply the fault-attempt tick.
    pub steps_adjust: i32,
    /// Width in bits 0..7, required write permission in bit 8 (also set on an
    /// RMW's load). Remaining bits must be zero. This names guest access intent,
    /// not just the native LDR/STR opcode.
    pub access: u32,
}

impl FaultSite {
    pub fn new(
        native_offset: u32,
        guest_eip: u32,
        steps_before: u32,
        bytes: u8,
        write: bool,
    ) -> Self {
        assert!(native_offset & 3 == 0 && matches!(bytes, 1 | 2 | 4 | 8));
        Self {
            native_offset,
            guest_eip,
            steps_adjust: steps_before as i32,
            access: u32::from(bytes) | (u32::from(write) << 8),
        }
    }
}

use super::a64::{Assembler, Reg};
use super::decode::{Address, Alu, Block, Op, Value};
use super::frame::guest_reg;
use alloc::vec::Vec;

/// Form the x86 offset, wrapping at 32 bits before adding a segment base.
/// Shared by LEA and both MMU access lowerings. The caller chooses
/// and validates segment semantics before turning this offset into an address.
/// A bare base/index is already resident and needs no copy. The returned
/// register is read-only; `dst` is the scratch for actual address arithmetic.
pub fn emit_address(a: &mut Assembler, dst: Reg, address: Address) -> Reg {
    let base = address.base.map(|reg| guest_reg(reg as usize));
    let index = address.index.map(|reg| guest_reg(reg as usize));
    if address.displacement == 0 {
        match (base, index) {
            (Some(base), Some(index)) => {
                a.add_shifted(dst, base, index, address.scale, false);
                return dst;
            }
            (Some(base), None) => {
                return base;
            }
            (None, Some(index)) => {
                if address.scale == 0 {
                    return index;
                } else {
                    a.bitfield(
                        dst,
                        index,
                        32 - address.scale,
                        31 - address.scale,
                        false,
                        false,
                    );
                }
                return dst;
            }
            _ => {}
        }
    }
    if let Some(base) = base {
        if address.displacement < 4096 {
            a.add_sub_imm(dst, base, address.displacement as u16, false, false, false);
        } else if address.displacement.wrapping_neg() < 4096 {
            a.add_sub_imm(
                dst,
                base,
                address.displacement.wrapping_neg() as u16,
                false,
                true,
                false,
            );
        } else {
            a.mov_imm(dst, address.displacement.into(), false);
            a.add_sub(dst, dst, base, false, false, false);
        }
    } else {
        a.mov_imm(dst, address.displacement.into(), false);
    }
    if let Some(index) = index {
        a.add_shifted(dst, dst, index, address.scale, false);
    }
    dst
}

/// Repeated accesses to one address and width whose addressing registers survive
/// every intervening operation. The first access certifies that operand on one
/// permitted backing page; later accesses retain individual commits. An interleaved
/// group saves its bias in a block-local slot; otherwise the current proof is
/// retained. Each backend chooses registers, locals or stack storage.
/// The proof may cover RAM or an uncached linear-framebuffer page.
/// The compiler excludes strings and control joins.
#[derive(Clone, Copy)]
pub struct AccessRange {
    pub address: Address,
    pub bytes: u16,
    pub write: bool,
    pub first: bool,
    pub spill: Option<u8>,
}

/// Describe addressing dependencies of straight-line operations. Memory
/// accesses may replace the current backing proof; other accepted operations preserve it.
/// Unknown operations end the proof, including strings, stack updates,
/// conditional memory operations and helper/control boundaries.
pub(crate) fn range_effect(op: Op) -> Option<(Option<(Address, u8, bool)>, u8)> {
    let value = |v, writes| match v {
        Value::Memory { address, bits } => (Some((address, bits / 8, false)), writes),
        _ => (None, writes),
    };
    Some(match op {
        Op::Mov { dst, src }
        | Op::ConditionalMove { dst, src, .. }
        | Op::Extend { dst, src, .. } => value(src, 1 << dst.index),
        Op::Alu { kind, dst, src } => value(
            src,
            if matches!(kind, Alu::Cmp | Alu::Test) {
                0
            } else {
                1 << dst.index
            },
        ),
        Op::Compare { lhs, rhs, .. } => value(if lhs.is_memory() { lhs } else { rhs }, 0),
        Op::Store { address, bits, src }
        | Op::MemoryAlu {
            address, bits, src, ..
        } if !src.is_memory() => (Some((address, bits / 8, true)), 0),
        Op::Lea { dst, .. }
        | Op::IncDec { dst, .. }
        | Op::Not { dst }
        | Op::Negate { dst }
        | Op::ByteSwap { dst } => (None, 1 << dst.index),
        Op::LoadFlagsByte => (None, 1),
        Op::Nop | Op::Direction { .. } | Op::Carry { .. } | Op::StoreFlagsByte => (None, 0),
        _ => return None,
    })
}

/// Reuse an exact operand's proof, without a runtime address cache. Do not widen
/// it to neighboring displacements: a valid stack frame may straddle two pages.
/// Requiring that whole interval to fit one page can then reject every iteration
/// of a hot loop even though each individual operand is wholly permitted.
/// tiny386's translate32/load32 also qualify actual operands, not stack frames.
/// Every actual load/store and its fault/commit record remain in guest order.
pub(crate) fn access_ranges(block: &Block, max_spills: u8) -> Vec<Option<AccessRange>> {
    let mut ranges = alloc::vec![None; block.instructions.len()];
    let mut start = 0;
    let mut spills = 0;
    while start < block.instructions.len() {
        if ranges[start].is_some() {
            start += 1;
            continue;
        }
        let Some((Some((address, width, _)), _)) = range_effect(block.instructions[start].op)
        else {
            start += 1;
            continue;
        };
        if !matches!(
            address.segment,
            crate::x86::cpu::SegmentReg::Ds
                | crate::x86::cpu::SegmentReg::Es
                | crate::x86::cpu::SegmentReg::Ss
        ) {
            start += 1;
            continue;
        }
        let inputs = address.base.map_or(0, |r| 1u8 << r) | address.index.map_or(0, |r| 1u8 << r);
        let mut write = false;
        let mut members = Vec::new();
        for (index, instruction) in block.instructions.iter().enumerate().skip(start) {
            let Some((access, writes)) = range_effect(instruction.op) else {
                break;
            };
            if let Some((next, bytes, stores)) = access {
                if next == address && bytes == width {
                    if ranges[index].is_some() {
                        break;
                    }
                    write |= stores;
                    members.push(index);
                }
            }
            if writes & inputs != 0 {
                break;
            }
        }
        if members.len() >= 2 {
            let intervening_memory = |pair: &[usize]| {
                block.instructions[pair[0] + 1..pair[1]]
                    .iter()
                    .any(|i| i.op.accesses_memory())
            };
            let interleaved = members.windows(2).any(intervening_memory);
            if interleaved && spills == max_spills {
                start += 1;
                continue;
            }
            let mut range = AccessRange {
                address,
                bytes: u16::from(width),
                write,
                first: true,
                spill: interleaved.then_some(spills),
            };
            if interleaved {
                spills += 1;
            }
            ranges[members[0]] = Some(range);
            range.first = false;
            let spill = range.spill;
            for pair in members.windows(2) {
                // Only an intervening memory operation can replace the resident proof:
                // range_effect already excludes every other clobber. Keep a
                // restored bias resident for consecutive members of this range.
                range.spill = spill.filter(|_| intervening_memory(pair));
                ranges[pair[1]] = Some(range);
            }
        }
        start += 1;
    }
    ranges
}

/// Stack windows use the same conservative rule in both backends: no other
/// memory operation, ESP replacement or control transition may intervene.
/// Each PUSH/POP still observes and commits the current architectural ESP.
#[derive(Clone, Copy)]
pub(crate) struct StackRange {
    pub bytes: u8,
    pub end: usize,
    pub write: bool,
}

pub(crate) fn stack_width(op: Op, write: bool) -> Option<u8> {
    match op {
        Op::Push {
            src: Value::Register(_) | Value::Immediate(_),
            bits,
        }
        | Op::Call {
            target: Value::Register(_) | Value::Immediate(_),
            bits,
        }
        | Op::PushFlags { bits }
            if write =>
        {
            Some(bits / 8)
        }
        Op::Pop { dst } if !write && dst.index != 4 => Some(dst.bits / 8),
        Op::Return { bits, .. } if !write => Some(bits / 8),
        _ => None,
    }
}

pub(crate) fn stack_range(block: &Block, index: usize) -> Option<StackRange> {
    let write = matches!(
        block.instructions[index].op,
        Op::Push { .. } | Op::PushFlags { .. }
    );
    stack_width(block.instructions[index].op, write)?;
    let mut range = StackRange {
        bytes: 0,
        end: index,
        write,
    };
    let mut accesses = 0;
    for (next_index, next) in block.instructions.iter().enumerate().skip(index) {
        if let Some(width) = stack_width(next.op, write) {
            if range.bytes + width > 28 {
                break;
            }
            range.bytes += width;
            accesses += 1;
            range.end = next_index + 1;
        } else if !matches!(range_effect(next.op), Some((None, writes)) if writes & (1 << 4) == 0) {
            break;
        }
    }
    (accesses >= 2).then_some(range)
}
