//! Connected, page-bounded control flow lowered to one Wasm function.
//! Registers remain in locals across internal branches. A bounded br_table
//! supplies multiple entries and indirect branches; mutually reachable blocks
//! share a smaller local dispatcher for their backedges. No host recursion.
//! Fixed edges between this module's functions use direct tail calls. Edges
//! into independently reclaimable modules retain versioned table lookup.
//! Arithmetic flags travel as a compact deferred record across control flow.
//! Only consumers materialize EFLAGS; core exits install Cpu's lazy state.
//! Memory side exits retain the flags preceding the unfinished instruction,
//! with exact budget refunds.

use super::{
    emit::{self, Code},
    frame::{
        ENTRY_BITS, ENTRY_MASK, ENTRY_SEGMENTS, Frame, GROUP_BITS, GROUP_FUNCTIONS, LINK_SLOTS,
        Link, SegmentLoad,
    },
};
use crate::x86::cpu::{
    LINEAR_TRANSLATION_CACHE_BITS, LINEAR_TRANSLATION_USER, LinearTranslationCacheEntry, SegmentReg,
};
use crate::x86::jit::decode::{
    self, Address, Alu, Block, BranchCondition, ExchangeOp, Instruction, Op, Register, StringOp,
    Value,
};
use crate::x86::jit::memory::{AccessRange, access_ranges, stack_range, stack_width};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};
use core::mem::{offset_of, size_of};

const ENTRY: u32 = 0;
const REG: u32 = 1;
const FLAGS: u32 = 9;
const REMAIN: u32 = 10;
const A: u32 = 11;
const R: u32 = 12;
// Zero means EFLAGS is materialized. Otherwise bits 0..3 select the ALU,
// bit 4 preserves FLAGS.CF, bit 6 is ADC/SBB or shift carry, and bits 8..12
// give the sign-bit index. A and R are width-masked ALU operands. For shifts
// (kind 6), A holds the explicit overflow bit and R the width-masked result.
const FLAG_KIND: u32 = 13;
const F: u32 = 14;
const B: u32 = 15;
const CARRY: u32 = 16;
const ADDR: u32 = 17;
const PTR: u32 = 18;
const SRC: u32 = 19;
const TMP: u32 = 20;
const TLB: u32 = 21;
const STATE: u32 = 22;
const PC: u32 = 23;
const WHY: u32 = 24;
const DEST: u32 = 25;
const COUNT: u32 = 26;
const BIAS: u32 = 27;
const SAVED_BIASES: u32 = 28;
const BIAS_SLOTS: u8 = 4;
const PERMISSIONS: u32 = SAVED_BIASES + BIAS_SLOTS as u32;
const WIDE: u32 = PERMISSIONS + 1;
const DIVISOR: u32 = WIDE + 1;
const QUOTIENT: u32 = WIDE + 2;
const LOCALS: u32 = WIDE - emit::ARGUMENTS;
// V8's optimizing graph can use hundreds of MiB for one large dispatcher.
// Keep functions small even when the module submission is eventually batched.
const MAX_BLOCKS: usize = 32;
const MAX_INSNS: usize = 128;

pub struct Region {
    pub bytes: Vec<u8>,
    pub entries: Vec<(u16, u16)>,
    pub dependencies: Vec<(u16, u16)>,
    pub instructions: usize,
    pub splits: usize,
    pub function_starts: Vec<u16>,
    pub deferred_edges: usize,
}

/// Allocations owned by the engine/page for the entire lifetime of this module.
/// Retirement removes its entries and table functions before freeing the page
/// or reusing the module slot. No other module's identity is embedded in code.
#[derive(Clone, Copy)]
pub struct Bindings {
    pub frame: u32,
    pub entries: u32,
    pub links: u32,
    pub recency: u32,
    pub slot: u32,
}

fn supported(op: Op) -> bool {
    matches!(
        op,
        Op::Nop
            | Op::Direction { .. }
            | Op::SignExtendPair { .. }
            | Op::ClearInterrupt
            | Op::SetInterrupt
            | Op::PopFlags { .. }
            | Op::LoadFlagsByte
            | Op::StoreFlagsByte
            | Op::Carry { .. }
            | Op::Mov { .. }
            | Op::ConditionalMove { .. }
            | Op::ByteSwap { .. }
            | Op::Store { .. }
            | Op::MemoryAlu { .. }
            | Op::Alu { .. }
            | Op::IncDec { .. }
            | Op::Not { .. }
            | Op::Negate { .. }
            | Op::Extend { .. }
            | Op::Lea { .. }
            | Op::Compare { .. }
            | Op::SetCondition { .. }
            | Op::Pop { .. }
            | Op::Push { .. }
            | Op::PushFlags { .. }
            | Op::Leave { .. }
            | Op::Call { .. }
            | Op::Return { .. }
            | Op::IndirectJump { .. }
            | Op::Branch { .. }
            | Op::ReadSegment { .. }
            | Op::LoadSegment {
                segment: SegmentReg::Ds | SegmentReg::Es | SegmentReg::Fs | SegmentReg::Gs,
                ..
            }
            | Op::BitScan { .. }
            | Op::Shift { .. }
            | Op::DoubleShift { .. }
            | Op::Imul { .. }
            | Op::Multiply { .. }
            | Op::Divide { .. }
            | Op::BitTest { .. }
            | Op::Exchange { .. }
            | Op::CompareExchange8 { .. }
            | Op::Movs { .. }
            | Op::String {
                kind: StringOp::Load | StringOp::Store | StringOp::Scan,
                ..
            }
    )
}

/// `bytes` is the complete already executable backing-page window; `origin`
/// is its CS-relative EIP. Every admitted start has been executed, while
/// connected direct targets are decoded only inside this permitted window.
/// `observed` lists repeatedly interpreted control-flow entries on the page.
/// Follow those edges and conditional fallthroughs; an unseen taken branch leaves
/// at its precise target and can earn independent admission later. A hot root
/// does not make every syntactically reachable error path worth compiling.
/// A submission owns at most eight bounded functions from an executable page.
/// Cohot roots share a function; its instruction/body caps split remaining
/// roots and direct continuations into later functions. Existing published
/// entries stop exploration.
/// Module ownership is the reclamation unit, independent of function size.
/// The core establishes `ENTRY_SEGMENTS` before every invocation; segment
/// changes leave generated execution. FS/GS still use dynamic qualifications.
/// `bindings` identifies the owning engine/page allocations and `memory_pages` is
/// the current linear-memory size. These allocations and that lower bound
/// remain valid while any published function from the module is callable.
pub fn batch(
    bytes: &[u8],
    origin: u32,
    starts: &[u16],
    observed: &[u16],
    published: &[u32],
    bindings: Bindings,
    memory_pages: u32,
) -> Option<Region> {
    let mut known = if published.is_empty() {
        alloc::vec![0; 4096]
    } else {
        published.to_vec()
    };
    let mut queue = starts.to_vec();
    let mut observed_bits = [0u64; 64];
    for &offset in observed.iter().chain(starts) {
        observed_bits[usize::from(offset) >> 6] |= 1u64 << (offset & 63);
    }
    let mut attempted = BTreeSet::new();
    let mut bodies = Vec::new();
    let mut entries = Vec::new();
    let mut dependencies = Vec::new();
    let mut relocations = Vec::new();
    let mut instructions = 0;
    let mut splits = 0;
    let mut deferred_edges = 0;
    while let Some(start) = queue.pop() {
        if bodies.len() == GROUP_FUNCTIONS {
            break;
        }
        if known[usize::from(start)] != 0 || !attempted.insert(start) {
            continue;
        }
        observed_bits[usize::from(start) >> 6] |= 1u64 << (start & 63);
        // These entries earned admission together on the same executable
        // page. Give the existing multi-entry builder all of them so related
        // paths can share locals and labels. The same instruction/body limits
        // still split a large cohort; unconsumed roots return through `next`.
        let mut roots = core::mem::take(&mut queue);
        roots.push(start);
        let mut limit = MAX_INSNS;
        let function = loop {
            let Some(function) = function(
                bytes,
                origin,
                &roots,
                &observed_bits,
                &known,
                limit,
                bindings,
                bodies.len() as u32,
            ) else {
                break None;
            };
            if function.body.0.len() <= 8 * 1024 {
                break Some(function);
            }
            assert!(
                limit > 1,
                "single x86 instruction exceeds Wasm function limit"
            );
            limit /= 2;
            splits += 1;
        };
        let Some(function) = function else {
            continue;
        };
        for &(offset, entry) in &function.entries {
            known[usize::from(offset)] = 1;
            entries.push((offset, ((bodies.len() as u16) << ENTRY_BITS) + entry));
        }
        // Resume roots and observed edges left by the function's size cap.
        // Unobserved branches were not queued.
        queue.splice(0..0, function.next);
        dependencies.extend(function.dependencies);
        instructions += function.instructions;
        deferred_edges += function.deferred_edges;
        bodies.push((function.body, origin.wrapping_add(u32::from(start))));
        relocations.push(function.jumps);
    }
    if bodies.is_empty() {
        return None;
    }
    // Resolve only immutable edges owned by this module. A different module
    // can be retired independently and must still be reached through its table.
    let targets: BTreeMap<_, _> = entries
        .iter()
        .map(|&(offset, entry)| (origin.wrapping_add(u32::from(offset)), u32::from(entry - 1)))
        .collect();
    for ((body, _), jumps) in bodies.iter_mut().zip(relocations) {
        for (start, end, pc) in jumps.into_iter().rev() {
            if let Some(&target) = targets.get(&pc) {
                let tail = module_tail(target >> ENTRY_BITS, target & ENTRY_MASK);
                if body.0.len() - (end - start) + tail.0.len() <= 8 * 1024 {
                    body.0.splice(start..end, tail.0);
                }
            }
        }
    }
    let function_starts = bodies
        .iter()
        .map(|(_, pc)| pc.wrapping_sub(origin) as u16)
        .collect();
    Some(Region {
        bytes: emit::module(bodies, LOCALS, flags_helper(), memory_pages),
        entries,
        dependencies,
        instructions,
        splits,
        function_starts,
        deferred_edges,
    })
}

struct Function {
    body: Code,
    entries: Vec<(u16, u16)>,
    dependencies: Vec<(u16, u16)>,
    next: Vec<u16>,
    instructions: usize,
    jumps: Vec<(usize, usize, u32)>,
    deferred_edges: usize,
}

fn module_tail(function: u32, entry: u32) -> Code {
    let mut code = Code::default();
    code.constant(entry);
    for local in REG..emit::ARGUMENTS {
        code.get(local);
    }
    code.byte(0x12); // return_call; same instance, no dynamic table/type lookup
    code.u32(function + emit::FIRST_REGION_FUNCTION);
    code
}

/// Materialize arithmetic status when an instruction consumes an unknown
/// incoming record. Core returns instead install the shared Cpu's lazy state.
fn flags_helper() -> Code {
    let mut c = Code::default();
    // Parameters: materialized bits, lhs, result, kind/width/carry. Locals:
    // operation class, addition/subtraction, reconstructed rhs, input carry.
    // rhs = result - lhs - carry (ADD), or lhs - result - carry (SUB).
    // Only its low operand-width bits matter to AF/OF. Keeping lhs/result/tag
    // avoids carrying a redundant rhs and carry register through every edge.
    c.get(3);
    c.binary(0x71, 15);
    c.set(4);
    c.get(4);
    c.binary(0x6b, 2);
    c.binary(0x49, 2);
    c.set(5);
    c.get(3);
    c.binary(0x76, 6);
    c.binary(0x71, 1);
    c.set(7);
    c.get(0);
    c.get(3);
    c.binary(0x71, 16);
    c.byte(0x45);
    c.binary(0x72, 0x8d4);
    c.binary(0x73, u32::MAX);
    c.byte(0x71);
    c.set(0);
    c.get(4);
    c.binary(0x46, 6);
    c.if_();
    // A shift already supplied CF and OF; AF follows the shared zero policy.
    c.get(0);
    c.get(7);
    c.byte(0x72);
    c.get(1);
    c.binary(0x74, 11);
    c.byte(0x72);
    c.set(0);
    c.else_();
    c.get(4);
    c.binary(0x47, 1);
    c.if_();
    c.get(2);
    c.get(1);
    c.byte(0x6b);
    c.get(1);
    c.get(2);
    c.byte(0x6b);
    c.get(5);
    c.byte(0x1b);
    c.get(7);
    c.byte(0x6b);
    c.set(6);
    c.get(0);
    c.get(1);
    c.get(6);
    c.byte(0x73);
    c.get(2);
    c.byte(0x73);
    c.binary(0x71, 16);
    c.byte(0x72);
    c.get(1);
    c.get(6);
    c.byte(0x73);
    c.constant(0);
    c.get(5);
    c.byte(0x6b);
    c.byte(0x73);
    c.get(1);
    c.get(2);
    c.byte(0x73);
    c.byte(0x71);
    c.get(3);
    c.binary(0x76, 8);
    c.byte(0x76);
    c.binary(0x71, 1);
    c.binary(0x74, 11);
    c.byte(0x72);
    c.set(0);
    c.get(3);
    c.binary(0x71, 16);
    c.byte(0x45);
    c.if_();
    c.get(0);
    c.get(2);
    c.get(1);
    c.byte(0x49);
    c.get(2);
    c.get(1);
    c.byte(0x4b);
    c.get(5);
    c.byte(0x1b);
    c.byte(0x72);
    c.get(7);
    c.get(2);
    c.get(1);
    c.byte(0x46);
    c.byte(0x71);
    c.byte(0x72);
    c.set(0);
    c.end();
    c.end();
    c.end();
    c.get(0);
    c.get(2);
    c.byte(0x45);
    c.binary(0x74, 6);
    c.byte(0x72);
    c.get(2);
    c.get(3);
    c.binary(0x76, 8);
    c.byte(0x76);
    c.binary(0x74, 7);
    c.byte(0x72);
    c.get(2);
    c.binary(0x71, 255);
    c.byte(0x69);
    c.binary(0x71, 1);
    c.byte(0x45);
    c.binary(0x74, 2);
    c.byte(0x72);
    c
}

#[derive(Clone, Copy)]
struct Entry {
    index: u32,
    group: u32,
}

fn successors(block: &Block) -> [Option<u32>; 2] {
    let Some(last) = block.instructions.last() else {
        return [None, None];
    };
    match last.op {
        Op::Branch { target, condition } => [Some(target), condition.map(|_| last.next_eip)],
        Op::Call {
            target: Value::Immediate(target),
            bits,
        } => [
            Some(if bits == 16 { target & 0xffff } else { target }),
            None,
        ],
        Op::Call { .. } | Op::IndirectJump { .. } | Op::Return { .. } => [None, None],
        _ => [Some(last.next_eip), None],
    }
}

/// Isolate cycles so their loop-carried state need not include every block in
/// the function. At most 32 blocks makes a bitset transitive closure sufficient;
/// indirect edges still go through the outer dispatcher. Emit components in
/// topological order so acyclic edges use Wasm labels even when the x86 target
/// precedes its source in memory.
fn flow_groups(blocks: &BTreeMap<u32, Block>) -> Vec<Vec<u32>> {
    let pcs: Vec<_> = blocks.keys().copied().collect();
    let mut reach = [0u32; MAX_BLOCKS];
    for (i, block) in blocks.values().enumerate() {
        reach[i] = 1 << i;
        for pc in successors(block).into_iter().flatten() {
            if let Ok(j) = pcs.binary_search(&pc) {
                reach[i] |= 1 << j;
            }
        }
    }
    for k in 0..pcs.len() {
        for i in 0..pcs.len() {
            if reach[i] & (1 << k) != 0 {
                reach[i] |= reach[k];
            }
        }
    }
    let mut assigned = 0u32;
    let mut groups = Vec::new();
    while assigned.count_ones() < pcs.len() as u32 {
        // Pick a component without an unplaced predecessor. Members of the
        // same cycle reach each other and do not constrain this choice.
        let i = (0..pcs.len())
            .find(|&i| {
                assigned & (1 << i) == 0
                    && (0..pcs.len()).all(|j| {
                        assigned & (1 << j) != 0
                            || reach[j] & (1 << i) == 0
                            || reach[i] & (1 << j) != 0
                    })
            })
            .expect("component graph is acyclic");
        let mut members = Vec::new();
        for j in 0..pcs.len() {
            if reach[i] & (1 << j) != 0 && reach[j] & (1 << i) != 0 {
                members.push(pcs[j]);
                assigned |= 1 << j;
            }
        }
        groups.push(members);
    }
    groups
}

fn function(
    bytes: &[u8],
    origin: u32,
    starts: &[u16],
    observed: &[u64; 64],
    published: &[u32],
    max_insns: usize,
    bindings: Bindings,
    function_index: u32,
) -> Option<Function> {
    let mut instructions = BTreeMap::<u32, Instruction>::new();
    let mut leaders = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut queue: Vec<u32> = starts
        .iter()
        .map(|&v| origin.wrapping_add(u32::from(v)))
        .collect();
    let mut dependencies = Vec::new();
    let mut deferred = BTreeSet::new();
    while let Some(pc) = queue.pop() {
        let offset = pc.wrapping_sub(origin) as usize;
        if offset >= bytes.len()
            || visited.contains(&pc)
            || published.get(offset).is_some_and(|&e| e != 0)
        {
            continue;
        }
        if visited.len() >= MAX_BLOCKS || instructions.len() >= max_insns {
            queue.push(pc);
            break;
        }
        visited.insert(pc);
        leaders.insert(pc);
        if instructions.contains_key(&pc) {
            continue;
        }
        let available = ((bytes.len() - offset) as u64).min(u64::from(u32::MAX - pc) + 1) as usize;
        let mut block = decode::block(
            &bytes[offset..offset + available],
            pc,
            max_insns - instructions.len(),
        );
        let count = block
            .instructions
            .iter()
            .position(|i| {
                !supported(i.op)
                    || instructions.contains_key(&i.eip)
                    || published
                        .get(i.eip.wrapping_sub(origin) as usize)
                        .is_some_and(|&e| e != 0)
            })
            .unwrap_or(block.instructions.len())
            .min(max_insns - instructions.len());
        block.instructions.truncate(count);
        block.byte_len = block
            .instructions
            .last()
            .map_or(0, |i| i.next_eip.wrapping_sub(pc) as usize);
        // Negative boundaries also depend on the bytes inspected by decoding.
        let used = (block.byte_len + 15).min(available);
        dependencies.push((offset as u16, used as u16));
        if let Some(last) = block.instructions.last() {
            let mut enqueue = |target: u32, fallthrough: bool| {
                let at = target.wrapping_sub(origin) as usize;
                if at >= bytes.len() {
                    return;
                }
                if fallthrough || observed[at >> 6] & (1u64 << (at & 63)) != 0 {
                    targets.insert(target);
                    queue.push(target);
                } else if published.get(at).is_none_or(|&entry| entry == 0) {
                    deferred.insert(target);
                }
            };
            match last.op {
                Op::Branch { target, condition } => {
                    if condition.is_some() && target <= last.eip {
                        enqueue(last.next_eip, true);
                        enqueue(target, false); // include the hot backedge before its exit
                    } else {
                        enqueue(target, false);
                        if condition.is_some() {
                            enqueue(last.next_eip, true);
                        }
                    }
                }
                Op::Call {
                    target: Value::Immediate(target),
                    bits,
                } => {
                    enqueue(last.next_eip, false);
                    enqueue(if bits == 16 { target & 0xffff } else { target }, false);
                }
                Op::IndirectJump { .. } | Op::Return { .. } => (),
                _ => queue.push(last.next_eip),
            }
        }
        for instruction in block.instructions {
            instructions.insert(instruction.eip, instruction);
        }
    }
    if instructions.is_empty() {
        return None;
    }
    let total = instructions.len();
    // Decode each instruction once, then split at the discovered entries.
    // Starting another trace in an existing block must not duplicate its tail.
    let mut blocks = BTreeMap::<u32, Block>::new();
    for &pc in &leaders {
        let mut block = Block {
            instructions: Vec::new(),
            byte_len: 0,
        };
        let mut at = pc;
        while let Some(&instruction) = instructions.get(&at) {
            block.instructions.push(instruction);
            at = instruction.next_eip;
            // The sampled entry can land in the middle of a loop. Give that
            // suffix an entry, but let the actual backedge header keep its
            // contiguous loop body; otherwise every iteration redispatches.
            if targets.contains(&at)
                || matches!(
                    instruction.op,
                    Op::Branch { .. }
                        | Op::Call { .. }
                        | Op::Return { .. }
                        | Op::IndirectJump { .. }
                        | Op::SetInterrupt
                        | Op::LoadSegment { .. }
                )
            {
                break;
            }
        }
        block.byte_len = at.wrapping_sub(pc) as usize;
        blocks.insert(pc, block);
    }
    let groups = flow_groups(&blocks);
    let entries: BTreeMap<u32, Entry> = groups
        .iter()
        .enumerate()
        .flat_map(|(group, members)| members.iter().map(move |&pc| (pc, group as u32)))
        .enumerate()
        .map(|(i, (pc, group))| {
            (
                pc,
                Entry {
                    index: i as u32,
                    group,
                },
            )
        })
        .collect();
    let mut c = Compiler {
        code: Code::default(),
        depth: 0,
        pc: origin,
        refund: 0,
        pending: None,
        origin,
        entries: &entries,
        groups: &groups,
        bindings,
        function_tag: (bindings.slot << GROUP_BITS) + function_index + 1,
        block_start: origin,
        block_loop: 0,
        group_loop: 0,
        dynamic_flags: true,
        memory_failure: None,
        repeat: None,
        range: None,
        stack_offset: None,
        checked_segments: ENTRY_SEGMENTS,
        jumps: Vec::new(),
    };
    // One engine owns this record until all its modules have been retired.
    // Linear memory cannot shrink, so the submission's minimum memory size
    // proves constant ABI addresses even on engines checking every store.
    c.code.constant(bindings.frame);
    c.code.set(F);
    c.code.constant(bindings.recency);
    #[cfg(feature = "jit-profile")]
    {
        // Count function entries only in diagnostic modules. Keep CLOCK's low
        // two bits and its seen bit independent of the wrapping 29-bit count.
        c.code.constant(bindings.recency);
        c.code.load(32, 0);
        c.code.binary(0x6a, 4);
    }
    c.code.constant(0x80000003);
    #[cfg(feature = "jit-profile")]
    c.code.byte(0x72);
    c.code.store(32, 0);
    for (offset, local) in [
        (offset_of!(Frame, tlb), TLB),
        (offset_of!(Frame, tlb_state), STATE),
        (offset_of!(Frame, permissions), PERMISSIONS),
    ] {
        c.field(offset);
        c.code.set(local);
    }
    c.code.constant(origin);
    c.code.set(PC);
    c.code.constant(0);
    c.code.set(WHY);
    c.code.block();
    c.depth += 1; // exit, absolute label 0
    // Direct cycles have local labels. Only computed branches need to revisit
    // the full entry table; acyclic entry selection is not a host safepoint.
    let dynamic_edges = blocks.values().any(|block| {
        block.instructions.last().is_some_and(|insn| {
            matches!(insn.op, Op::Return { .. } | Op::IndirectJump { .. })
                || matches!(insn.op, Op::Call { target, .. } if !matches!(target, Value::Immediate(_)))
        })
    });
    if dynamic_edges {
        c.code.loop_();
    } else {
        c.code.block();
    }
    c.depth += 1; // dispatcher, absolute label 1
    for _ in &groups {
        c.code.block();
        c.depth += 1;
    }
    c.code.get(ENTRY);
    c.code.byte(0x0e);
    c.code.u32(blocks.len() as u32);
    for (i, group) in groups.iter().enumerate() {
        for _ in group {
            c.code.u32(i as u32);
        }
    }
    c.code.u32(groups.len() as u32 + 1);
    for group in &groups {
        c.end();
        c.group_loop = c.depth;
        if group.len() > 1 {
            c.code.loop_();
            c.depth += 1;
            for _ in group {
                c.code.block();
                c.depth += 1;
            }
            c.code.get(ENTRY);
            let first = entries[&group[0]].index;
            if first != 0 {
                c.code.binary(0x6b, first);
            }
            c.code.byte(0x0e);
            c.code.u32(group.len() as u32);
            for i in 0..group.len() {
                c.code.u32(i as u32);
            }
            c.code.u32(c.depth - 1); // invalid entry leaves through exit 0
            for &pc in group {
                c.end();
                c.block(pc, &blocks[&pc]);
            }
            c.end();
        } else {
            c.block(group[0], &blocks[&group[0]]);
        }
    }
    c.end();
    c.end();
    c.chain();
    // Every edge reaching here stashed the deferred record. Keep it deferred
    // across the core handoff instead of duplicating materialization in every
    // function epilogue. Frame commits the same record to the shared Cpu.
    for reg in 0..8 {
        c.code.get(F);
        c.code.get(REG + reg as u32);
        c.code.store(32, (offset_of!(Frame, regs) + reg * 4) as u32);
    }
    c.code.get(F);
    c.code.get(FLAGS);
    c.code.store(32, offset_of!(Frame, eflags) as u32);
    for (offset, local) in [
        (offset_of!(Frame, eip), PC),
        (offset_of!(Frame, remaining), REMAIN),
        (offset_of!(Frame, exit), WHY),
        (offset_of!(Frame, flag_lhs), A),
        (offset_of!(Frame, flag_result), R),
        (offset_of!(Frame, flag_kind), FLAG_KIND),
    ] {
        c.code.get(F);
        c.code.get(local);
        c.code.store(32, offset as u32);
    }
    Some(Function {
        body: c.code,
        next: queue
            .into_iter()
            .filter_map(|pc| {
                let offset = pc.wrapping_sub(origin);
                (offset < bytes.len() as u32).then_some(offset as u16)
            })
            .collect(),
        entries: entries
            .iter()
            .filter(|(pc, _)| !blocks[pc].instructions.is_empty())
            .map(|(&pc, entry)| (pc.wrapping_sub(origin) as u16, entry.index as u16 + 1))
            .collect(),
        dependencies,
        instructions: total,
        jumps: c.jumps,
        deferred_edges: deferred.len(),
    })
}

#[derive(Clone, Copy)]
struct Pending {
    kind: Alu,
    bits: u8,
    preserve_carry: bool,
}

struct Compiler<'a> {
    code: Code,
    depth: u32,
    pc: u32,
    refund: u32,
    pending: Option<Pending>,
    dynamic_flags: bool,
    origin: u32,
    entries: &'a BTreeMap<u32, Entry>,
    groups: &'a [Vec<u32>],
    bindings: Bindings,
    function_tag: u32,
    block_start: u32,
    block_loop: u32,
    group_loop: u32,
    memory_failure: Option<u32>,
    repeat: Option<(u32, u32)>,
    range: Option<AccessRange>,
    stack_offset: Option<u32>,
    checked_segments: u32,
    jumps: Vec<(usize, usize, u32)>,
}

impl Compiler<'_> {
    fn block(&mut self, pc: u32, block: &Block) {
        self.pc = pc;
        self.refund = 0;
        self.pending = None;
        self.dynamic_flags = true;
        self.checked_segments = ENTRY_SEGMENTS;
        self.block_start = pc;
        self.block_loop = self.depth;
        // A loop is an execution-engine safepoint as well as a branch label.
        // Straight-line blocks need neither its stack check nor its loop state.
        let self_edge = successors(block).contains(&Some(pc));
        if self_edge {
            self.code.loop_();
        } else {
            self.code.block();
        }
        self.depth += 1;
        if block.instructions.is_empty() {
            self.exit_const(pc, 5);
            self.end();
            return;
        }
        let n = block.instructions.len() as u32;
        let ranges = access_ranges(block, BIAS_SLOTS);
        self.code.get(REMAIN);
        self.code.binary(0x49, n);
        self.if_();
        self.exit_const(pc, 2);
        self.end();
        self.code.get(REMAIN);
        self.code.binary(0x6b, n);
        self.code.set(REMAIN);
        let mut stack_end = 0;
        let mut stack_next = 0;
        let mut stack_write = false;
        for (i, insn) in block.instructions.iter().enumerate() {
            self.pc = insn.eip;
            self.refund = n - i as u32;
            self.range = ranges[i];
            if i >= stack_end {
                if let Some(range) = stack_range(block, i) {
                    self.stack_guard();
                    self.code.get(REG + 4);
                    if range.write {
                        self.code.binary(0x6b, u32::from(range.bytes));
                    }
                    self.code.set(ADDR);
                    self.memory_span(SegmentReg::Ss, u32::from(range.bytes), range.write);
                    self.code.get(PTR);
                    self.code.set(BIAS);
                    stack_end = range.end;
                    stack_next = if range.write {
                        u32::from(range.bytes)
                    } else {
                        0
                    };
                    stack_write = range.write;
                }
            }
            self.stack_offset = if i < stack_end {
                stack_width(insn.op, stack_write).map(|bytes| {
                    if stack_write {
                        stack_next -= u32::from(bytes);
                    }
                    let offset = stack_next;
                    if !stack_write {
                        stack_next += u32::from(bytes);
                    }
                    offset
                })
            } else {
                None
            };
            self.instruction(*insn);
            if matches!(insn.op, Op::Movs { .. } | Op::String { .. }) {
                // REP with ECX=0 may have skipped the generated qualifiers.
                self.checked_segments = ENTRY_SEGMENTS;
            }
        }
        self.refund = 0;
        let last = block.instructions.last().unwrap();
        if !matches!(
            last.op,
            Op::Branch { .. }
                | Op::Call { .. }
                | Op::Return { .. }
                | Op::IndirectJump { .. }
                | Op::LoadSegment { .. }
        ) {
            self.jump(last.next_eip);
        }
        self.end();
    }

    fn stash_flags(&mut self) {
        if let Some(pending) = self.pending.take() {
            let kind = match pending.kind {
                Alu::And | Alu::Or | Alu::Xor | Alu::Test => 1,
                Alu::Add => 2,
                Alu::Adc => 3,
                Alu::Sub | Alu::Cmp => 4,
                Alu::Sbb => 5,
            };
            self.code.constant(
                kind | (u32::from(pending.preserve_carry) << 4)
                    | (u32::from(pending.bits - 1) << 8),
            );
            if matches!(pending.kind, Alu::Adc | Alu::Sbb) {
                self.code.get(CARRY);
                self.code.binary(0x74, 6);
                self.code.byte(0x72);
            }
            self.code.set(FLAG_KIND);
        }
        self.dynamic_flags = true;
    }
    fn arithmetic_helper(&mut self) {
        for local in [FLAGS, A, R, FLAG_KIND] {
            self.code.get(local);
        }
        self.code.call(emit::FLAGS_FUNCTION);
        self.code.set(FLAGS);
    }
    fn clear_flag_record(&mut self) {
        self.code.constant(0);
        self.code.set(FLAG_KIND);
        self.dynamic_flags = false;
    }
    fn chain(&mut self) {
        // Tail calls retain a bounded Wasm stack and let each module retire
        // independently. The table, not another instance, owns the target.
        self.code.get(WHY);
        self.code.byte(0x45);
        self.code.get(REMAIN);
        self.code.byte(0x45);
        self.code.byte(0x45);
        self.code.byte(0x71);
        self.code.if_();
        self.code.get(PC);
        self.field(offset_of!(Frame, cs_base));
        self.code.byte(0x6a);
        self.code.set(ADDR);
        self.code.get(ADDR);
        self.code.binary(0x76, 12);
        self.code.binary(0x71, LINK_SLOTS as u32 - 1);
        // The current data-context bit also selects the execute-proof plane.
        // A supervisor link can never approve a user instruction fetch.
        self.code.get(STATE);
        self.code.binary(0x71, LINEAR_TRANSLATION_USER);
        self.code
            .binary(0x6c, LINK_SLOTS as u32 / LINEAR_TRANSLATION_USER);
        self.code.byte(0x72);
        self.code.binary(0x6c, size_of::<Link>() as u32);
        self.code.constant(self.bindings.links);
        self.code.byte(0x6a);
        self.code.set(PTR);
        self.code.get(PTR);
        self.code.load(32, offset_of!(Link, tag) as u32);
        self.code.get(ADDR);
        self.code.binary(0x71, !4095);
        self.code.binary(0x72, 1);
        self.code.byte(0x46);
        self.code.if_();
        self.code.get(PTR);
        self.code.load(32, offset_of!(Link, version_ptr) as u32);
        self.code.byte(0x29);
        self.code.u32(0);
        self.code.u32(0);
        self.code.get(PTR);
        self.code.byte(0x29);
        self.code.u32(0);
        self.code.u32(offset_of!(Link, version) as u32);
        self.code.byte(0x51);
        self.code.if_();
        self.code.get(PTR);
        self.code.load(32, offset_of!(Link, entries) as u32);
        self.code.get(ADDR);
        self.code.binary(0x71, 4095);
        self.code.binary(0x74, 2);
        self.code.byte(0x6a);
        self.code.load(32, 0);
        self.code.tee(ENTRY);
        self.code.if_();
        self.code.get(ENTRY);
        self.code.binary(0x71, ENTRY_MASK);
        for reg in REG..REG + 8 {
            self.code.get(reg);
        }
        for local in FLAGS..emit::ARGUMENTS {
            self.code.get(local);
        }
        self.code.get(PTR);
        self.code.load(32, offset_of!(Link, function) as u32);
        self.code.get(ENTRY);
        self.code.binary(0x76, ENTRY_BITS);
        self.code.binary(0x6b, 1);
        self.code.byte(0x6a);
        self.code.byte(0x13);
        self.code.u32(0);
        self.code.u32(0); // return_call_indirect
        for _ in 0..4 {
            self.code.end();
        }
    }
    fn if_(&mut self) {
        self.code.if_();
        self.depth += 1;
    }
    fn end(&mut self) {
        self.code.end();
        self.depth -= 1;
    }
    fn branch(&mut self, absolute: u32) {
        self.code.br(self.depth - 1 - absolute);
    }
    fn field(&mut self, offset: usize) {
        self.code.get(F);
        self.code.load(32, offset as u32);
    }
    fn reg(&mut self, r: Register) {
        self.code.get(REG + u32::from(r.index));
        if r.shift != 0 {
            self.code.binary(0x76, u32::from(r.shift));
        }
        self.mask(r.bits);
    }
    fn mask(&mut self, bits: u8) {
        if bits < 32 {
            self.code.binary(0x71, (1u32 << bits) - 1);
        }
    }
    fn put_reg(&mut self, r: Register) {
        if r.bits == 32 {
            self.code.set(REG + u32::from(r.index));
            return;
        }
        self.mask(r.bits);
        if r.shift != 0 {
            self.code.binary(0x74, u32::from(r.shift));
        }
        self.code.get(REG + u32::from(r.index));
        self.code.binary(0x71, !(((1u32 << r.bits) - 1) << r.shift));
        self.code.byte(0x72);
        self.code.set(REG + u32::from(r.index));
    }
    fn address(&mut self, a: Address) {
        self.code.constant(a.displacement);
        if let Some(base) = a.base {
            self.code.get(REG + u32::from(base));
            self.code.byte(0x6a);
        }
        if let Some(index) = a.index {
            self.code.get(REG + u32::from(index));
            if a.scale != 0 {
                self.code.binary(0x74, u32::from(a.scale));
            }
            self.code.byte(0x6a);
        }
    }
    fn exit_const(&mut self, pc: u32, why: u32) {
        self.code.constant(pc);
        self.code.set(PC);
        self.exit(why);
    }
    fn exit(&mut self, why: u32) {
        // Emitting one side exit must not consume the pending flags on its
        // untaken path: the following generated instructions still own them.
        let pending = self.pending;
        let dynamic = self.dynamic_flags;
        self.stash_flags();
        self.pending = pending;
        self.dynamic_flags = dynamic;
        if self.refund != 0 {
            self.code.get(REMAIN);
            self.code.binary(0x6a, self.refund);
            self.code.set(REMAIN);
        }
        self.code.constant(why);
        self.code.set(WHY);
        self.branch(0);
    }
    fn fail_if(&mut self) {
        if let Some(label) = self.memory_failure {
            self.code.byte(0x0d); // br_if, with one common precise memory exit
            self.code.u32(self.depth - 1 - label);
            return;
        }
        self.if_();
        self.exit_const(self.pc, 1);
        self.end();
    }
    fn flat_segment_guard(&mut self, bit: u32) {
        if self.checked_segments & bit != 0 {
            return;
        }
        // Segment caches cannot change within a generated invocation. A
        // completed qualifier remains valid through register/RAM operations;
        // Each externally enterable block starts with only ENTRY_SEGMENTS.
        self.code.get(PERMISSIONS);
        self.code.binary(0x71, bit);
        self.code.byte(0x45);
        self.fail_if();
        self.checked_segments |= bit;
    }
    fn stack_guard(&mut self) {
        self.flat_segment_guard(1 << 6);
    }
    fn memory(&mut self, address: Address, bits: u8, write: bool) {
        if let Some(range) = self.range {
            if range.first {
                self.address(range.address);
                self.code.set(ADDR);
                self.memory_span(address.segment, u32::from(range.bytes), range.write);
                self.code.get(PTR);
                self.code.set(BIAS);
                if let Some(slot) = range.spill {
                    self.code.get(BIAS);
                    self.code.set(SAVED_BIASES + u32::from(slot));
                }
            } else if let Some(slot) = range.spill {
                self.code.get(SAVED_BIASES + u32::from(slot));
                self.code.set(BIAS);
            }
            self.code.get(BIAS);
            let offset = address
                .displacement
                .wrapping_sub(range.address.displacement);
            if offset != 0 {
                self.code.binary(0x6a, offset);
            }
            self.code.set(PTR);
            return;
        }
        self.address(address);
        self.code.set(ADDR);
        self.memory_at(address.segment, bits, write);
    }
    fn memory_at(&mut self, segment: SegmentReg, bits: u8, write: bool) {
        self.memory_span(segment, u32::from(bits / 8), write);
    }
    fn memory_span(&mut self, segment: SegmentReg, bytes: u32, write: bool) {
        // Every qualifier has the same pre-instruction state. Share its exit
        // rather than duplicate flag materialization for each failing guard.
        let done = self.depth;
        self.code.block();
        self.depth += 1;
        let failed = self.depth;
        self.code.block();
        self.depth += 1;
        self.memory_failure = Some(failed);
        let bit = 1 << (segment as u32 + if write { 8 } else { 0 });
        if matches!(segment, SegmentReg::Fs | SegmentReg::Gs) {
            self.code.get(PERMISSIONS);
            self.code.binary(0x71, bit);
            self.code.byte(0x45);
            self.if_();
            self.code.get(PERMISSIONS);
            self.code.binary(0x71, bit << 16);
            self.code.byte(0x45);
            self.fail_if();
            let index = usize::from(segment == SegmentReg::Gs);
            self.field(offset_of!(Frame, limits) + index * 4);
            self.code.set(TMP);
            self.code.get(TMP);
            self.code.binary(0x49, bytes - 1);
            self.fail_if();
            self.code.get(ADDR);
            self.code.get(TMP);
            self.code.binary(0x6b, bytes - 1);
            self.code.byte(0x4b);
            self.fail_if();
            self.code.get(ADDR);
            self.field(offset_of!(Frame, bases) + index * 4);
            self.code.byte(0x6a);
            self.code.set(ADDR);
            self.end();
        } else {
            self.flat_segment_guard(bit);
        }
        if bytes > 1 {
            self.code.get(ADDR);
            self.code.binary(0x71, 4095);
            self.code.binary(0x4b, 4096 - bytes);
            self.fail_if();
        }
        self.code.get(ADDR);
        self.code.binary(0x76, 12);
        self.code
            .binary(0x71, (1 << LINEAR_TRANSLATION_CACHE_BITS) - 1);
        self.code
            .binary(0x6c, size_of::<LinearTranslationCacheEntry>() as u32);
        self.code.get(TLB);
        self.code.byte(0x6a);
        self.code.set(PTR);
        self.code.get(PTR);
        self.code.load(
            32,
            if write {
                offset_of!(LinearTranslationCacheEntry, write_tag)
            } else {
                offset_of!(LinearTranslationCacheEntry, tag)
            } as u32,
        );
        // A user-qualified access also certifies supervisor access in the P6
        // model (which has no SMAP). Ignore that stronger privilege bit only
        // for supervisor execution. User accesses still require it; page,
        // generation and the independent write proof remain exact.
        self.code.get(STATE);
        self.code.binary(0x72, !LINEAR_TRANSLATION_USER);
        self.code.byte(0x71);
        self.code.get(ADDR);
        self.code.binary(0x71, !4095);
        self.code.get(STATE);
        self.code.byte(0x72);
        self.code.byte(0x47);
        self.if_();
        // Outlining the uncommon miss into the core avoids an imported call
        // and its live-register spills at every generated memory access. The
        // core may warm a resident translation, then interprets precisely this
        // instruction (or the unfinished REP suffix). No future access occurs.
        self.code.get(F);
        self.code.get(ADDR);
        self.code.store(32, offset_of!(Frame, refill_linear) as u32);
        self.code.get(F);
        self.code.constant(1 + u32::from(write));
        self.code.store(32, offset_of!(Frame, refill_intent) as u32);
        self.branch(failed);
        self.end();
        self.code.get(PTR);
        self.code
            .load(32, offset_of!(LinearTranslationCacheEntry, bias) as u32);
        self.code.get(ADDR);
        self.code.byte(0x6a);
        self.code.set(PTR);
        self.memory_failure = None;
        self.branch(done);
        self.end();
        if let Some((control, next)) = self.repeat {
            for (offset, value) in [
                (offset_of!(Frame, repeat.control), control),
                (offset_of!(Frame, repeat.next_eip), next),
            ] {
                self.code.get(F);
                self.code.constant(value);
                self.code.store(32, offset as u32);
            }
            self.code.get(F);
            self.code.get(COUNT);
            self.code
                .store(32, offset_of!(Frame, repeat.completed) as u32);
            self.exit_const(self.pc, 6);
        } else {
            self.exit_const(self.pc, 1);
        }
        self.end();
    }
    fn value(&mut self, value: Value) {
        match value {
            Value::Immediate(v) => self.code.constant(v),
            Value::Register(r) => self.reg(r),
            Value::Memory { address, bits } => {
                self.memory(address, bits, false);
                self.code.get(PTR);
                self.code.load(bits, 0);
            }
        }
    }
    fn flags_input(&mut self, kind: Alu, preserve: bool) {
        if preserve || matches!(kind, Alu::Adc | Alu::Sbb) {
            self.flush_carry();
        }
        if matches!(kind, Alu::Adc | Alu::Sbb) {
            self.code.get(FLAGS);
            self.code.binary(0x71, 1);
            self.code.set(CARRY);
        }
    }
    fn alu(&mut self, kind: Alu, bits: u8, preserve: bool) {
        // A and B are already bounded operands. Every fault guard for this
        // instruction preceded their overwrite of the prior pending record.
        self.code.get(A);
        self.code.get(B);
        self.code.byte(match kind {
            Alu::Add | Alu::Adc => 0x6a,
            Alu::Sub | Alu::Cmp | Alu::Sbb => 0x6b,
            Alu::And | Alu::Test => 0x71,
            Alu::Or => 0x72,
            Alu::Xor => 0x73,
        });
        if matches!(kind, Alu::Adc | Alu::Sbb) {
            self.code.get(CARRY);
            self.code.byte(if kind == Alu::Adc { 0x6a } else { 0x6b });
        }
        self.mask(bits);
        self.code.set(R);
        self.pending = Some(Pending {
            kind,
            bits,
            preserve_carry: preserve,
        });
        self.dynamic_flags = false;
    }
    // ADC/SBB and INC/DEC only consume the preceding carry. Their new
    // arithmetic result replaces every other status bit before a side exit.
    fn flush_carry(&mut self) {
        let Some(pending) = self.pending.take() else {
            if self.dynamic_flags {
                self.code.get(FLAGS);
                self.code.binary(0x71, !1);
                self.record_carry();
                self.code.byte(0x72);
                self.code.set(FLAGS);
                self.clear_flag_record();
            }
            return;
        };
        if pending.preserve_carry {
            self.clear_flag_record();
            return;
        }
        self.code.get(FLAGS);
        self.code.binary(0x71, !1);
        if !matches!(pending.kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test) {
            self.carry(pending.kind);
            self.code.byte(0x72);
        }
        self.code.set(FLAGS);
        self.clear_flag_record();
    }
    fn carry(&mut self, kind: Alu) {
        let add = matches!(kind, Alu::Add | Alu::Adc);
        self.code.get(if add { R } else { A });
        self.code.get(if add { A } else { B });
        self.code.byte(0x49);
        if matches!(kind, Alu::Adc | Alu::Sbb) {
            self.code.get(CARRY);
            self.code.get(if add { R } else { A });
            self.code.get(if add { A } else { B });
            self.code.byte(0x46);
            self.code.byte(0x71);
            self.code.byte(0x72);
        }
    }
    /// Read just CF from a joined arithmetic record. INC/DEC retain FLAGS.CF;
    /// ADD/ADC compare result with lhs, SUB/SBB reverse that comparison, and
    /// carry-in covers the equal-result wraparound case. No flag is committed.
    fn record_carry(&mut self) {
        self.code.get(FLAG_KIND);
        self.code.byte(0x45);
        self.code.byte(0x45);
        self.code.get(FLAG_KIND);
        self.code.binary(0x71, 16);
        self.code.byte(0x45);
        self.code.byte(0x71);
        self.code.0.extend_from_slice(&[0x04, emit::I32]);
        self.code.get(FLAG_KIND);
        self.code.binary(0x71, 15);
        self.code.binary(0x46, 6);
        self.code.0.extend_from_slice(&[0x04, emit::I32]);
        self.code.get(FLAG_KIND);
        self.code.binary(0x76, 6);
        self.code.binary(0x71, 1);
        self.code.else_();
        self.code.get(R);
        self.code.get(A);
        self.code.byte(0x49);
        self.code.get(R);
        self.code.get(A);
        self.code.byte(0x4b);
        self.code.get(FLAG_KIND);
        self.code.binary(0x71, 15);
        self.code.binary(0x49, 4);
        self.code.byte(0x1b);
        self.code.get(FLAG_KIND);
        self.code.binary(0x76, 6);
        self.code.binary(0x71, 1);
        self.code.get(R);
        self.code.get(A);
        self.code.byte(0x46);
        self.code.byte(0x71);
        self.code.byte(0x72);
        self.code.get(FLAG_KIND);
        self.code.binary(0x71, 15);
        self.code.binary(0x4b, 1);
        self.code.byte(0x71);
        self.code.end();
        self.code.else_();
        self.flag_bit(0);
        self.code.end();
    }
    fn overflow(&mut self, kind: Alu, bits: u8) {
        if matches!(kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test) {
            self.code.constant(0);
            return;
        }
        self.code.get(A);
        self.code.get(B);
        self.code.byte(0x73);
        if matches!(kind, Alu::Add | Alu::Adc) {
            self.code.binary(0x73, u32::MAX);
        }
        self.code.get(A);
        self.code.get(R);
        self.code.byte(0x73);
        self.code.byte(0x71);
        self.code.binary(0x76, u32::from(bits - 1));
    }
    fn flush_flags(&mut self) {
        let Some(Pending {
            kind,
            bits,
            preserve_carry,
        }) = self.pending.take()
        else {
            if self.dynamic_flags {
                self.code.get(FLAG_KIND);
                self.if_();
                self.arithmetic_helper();
                self.clear_flag_record();
                self.end();
            }
            return;
        };
        self.result_flags(bits);
        let logical = matches!(kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test);
        self.code.get(FLAGS);
        self.code
            .binary(0x71, !(0x810 | u32::from(!preserve_carry)));
        if !logical {
            self.code.get(A);
            self.code.get(B);
            self.code.byte(0x73);
            self.code.get(R);
            self.code.byte(0x73);
            self.code.binary(0x71, 16);
            self.code.byte(0x72);
            self.overflow(kind, bits);
            self.code.binary(0x74, 11);
            self.code.byte(0x72);
            if !preserve_carry {
                self.carry(kind);
                self.code.byte(0x72);
            }
        }
        self.code.set(FLAGS);
        self.clear_flag_record();
    }
    fn flag_bit(&mut self, bit: u32) {
        self.code.get(FLAGS);
        if bit != 0 {
            self.code.binary(0x76, bit);
        }
        self.code.binary(0x71, 1);
    }
    fn set_flag(&mut self, bit: u32) {
        self.code.set(TMP);
        self.code.get(FLAGS);
        self.code.binary(0x71, !(1 << bit));
        self.code.get(TMP);
        self.code.binary(0x74, bit);
        self.code.byte(0x72);
        self.code.set(FLAGS);
    }
    fn condition(&mut self, condition: u8) {
        if let Some(pending) = self.pending {
            // Most branches need one status bit. Keep the arithmetic operands
            // available for a side exit; a successor may overwrite all flags.
            match condition & !1 {
                0 => self.overflow(pending.kind, pending.bits),
                2 | 6 => {
                    if pending.preserve_carry {
                        self.code.get(FLAGS);
                        self.code.binary(0x71, 1);
                    } else if matches!(pending.kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test) {
                        self.code.constant(0);
                    } else {
                        self.carry(pending.kind);
                    }
                    if condition & !1 == 6 {
                        self.code.get(R);
                        self.code.byte(0x45);
                        self.code.byte(0x72);
                    }
                }
                4 => {
                    self.code.get(R);
                    self.code.byte(0x45);
                }
                8 => {
                    self.code.get(R);
                    self.code.binary(0x76, u32::from(pending.bits - 1));
                }
                10 => {
                    self.code.get(R);
                    self.code.binary(0x71, 255);
                    self.code.byte(0x69);
                    self.code.binary(0x71, 1);
                    self.code.byte(0x45);
                }
                12 | 14 => {
                    if matches!(pending.kind, Alu::Sub | Alu::Cmp) {
                        // CMP/SUB already have the operands needed by a signed
                        // comparison. Shifting puts a byte/word sign in bit 31
                        // without materializing SF, OF, PF, AF or CF.
                        for local in [A, B] {
                            self.code.get(local);
                            if pending.bits < 32 {
                                self.code.binary(0x74, 32 - u32::from(pending.bits));
                            }
                        }
                        self.code
                            .byte(if condition & !1 == 12 { 0x48 } else { 0x4c });
                    } else {
                        self.code.get(R);
                        self.code.binary(0x76, u32::from(pending.bits - 1));
                        self.overflow(pending.kind, pending.bits);
                        self.code.byte(0x73);
                        if condition & !1 == 14 {
                            self.code.get(R);
                            self.code.byte(0x45);
                            self.code.byte(0x72);
                        }
                    }
                }
                _ => unreachable!(),
            }
            if condition & 1 != 0 {
                self.code.byte(0x45);
            }
            return;
        }
        if self.dynamic_flags && matches!(condition & !1, 2 | 6) {
            self.record_carry();
            if condition & !1 == 6 {
                self.condition(4);
                self.code.byte(0x72);
            }
            if condition & 1 != 0 {
                self.code.byte(0x45);
            }
            return;
        }
        if self.dynamic_flags && matches!(condition & !1, 4 | 8 | 10) {
            // A join does not require all EFLAGS merely to test the result.
            self.code.get(FLAG_KIND);
            self.code.0.extend_from_slice(&[0x04, emit::I32]);
            self.code.get(R);
            match condition & !1 {
                4 => self.code.byte(0x45),
                8 => {
                    self.code.get(FLAG_KIND);
                    self.code.binary(0x76, 8);
                    self.code.byte(0x76);
                }
                _ => {
                    self.code.binary(0x71, 255);
                    self.code.byte(0x69);
                    self.code.binary(0x71, 1);
                    self.code.byte(0x45);
                }
            }
            self.code.else_();
            self.flag_bit(match condition & !1 {
                4 => 6,
                8 => 7,
                _ => 2,
            });
            self.code.end();
            if condition & 1 != 0 {
                self.code.byte(0x45);
            }
            return;
        }
        self.flush_flags();
        match condition & !1 {
            0 => {
                self.code.get(FLAGS);
                self.code.binary(0x76, 11);
                self.code.binary(0x71, 1);
            }
            2 => {
                self.code.get(FLAGS);
                self.code.binary(0x71, 1);
            }
            4 => self.flag_bit(6),
            6 => {
                self.code.get(FLAGS);
                self.code.binary(0x71, 1);
                self.flag_bit(6);
                self.code.byte(0x72);
            }
            8 => self.flag_bit(7),
            10 => self.flag_bit(2),
            12 | 14 => {
                self.code.get(FLAGS);
                self.code.binary(0x76, 11);
                self.code.binary(0x71, 1);
                self.flag_bit(7);
                self.code.byte(0x73);
                if condition & !1 == 14 {
                    self.flag_bit(6);
                    self.code.byte(0x72);
                }
            }
            _ => unreachable!(),
        }
        if condition & 1 != 0 {
            self.code.byte(0x45);
        }
    }
    fn jump(&mut self, pc: u32) {
        self.refund = 0;
        self.stash_flags();
        if self.entries.contains_key(&pc) {
            self.jump_internal(pc);
        } else {
            let start = self.code.0.len();
            self.exit_const(pc, 0);
            self.jumps.push((start, self.code.0.len(), pc));
        }
    }
    /// Absolute Wasm label, plus the selector required by a dispatcher edge.
    fn branch_target(&self, pc: u32) -> Option<(u32, Option<u32>)> {
        let entry = self.entries.get(&pc)?;
        let current = self.entries[&self.block_start];
        let group = &self.groups[entry.group as usize];
        if pc == self.block_start {
            Some((self.block_loop, None))
        } else if entry.group == current.group {
            if entry.index > current.index {
                let first = self.entries[&group[0]].index;
                Some((
                    self.group_loop + group.len() as u32 - (entry.index - first),
                    None,
                ))
            } else {
                Some((self.group_loop, Some(entry.index)))
            }
        } else {
            assert!(
                entry.group > current.group,
                "backedge outside its component"
            );
            // A cycle can have several entry blocks. Its dispatcher needs
            // this edge's exact entry, while singleton components do not.
            Some((
                self.groups.len() as u32 + 1 - entry.group,
                (group.len() > 1).then_some(entry.index),
            ))
        }
    }
    fn jump_internal(&mut self, pc: u32) {
        let (label, selector) = self.branch_target(pc).unwrap();
        if let Some(entry) = selector {
            self.code.constant(entry);
            self.code.set(ENTRY);
        }
        self.branch(label);
    }
    fn branch_condition(&mut self, condition: BranchCondition, invert: bool) {
        match condition {
            BranchCondition::Flags(condition) => self.condition(condition ^ u8::from(invert)),
            BranchCondition::EcxZero => {
                self.code.get(REG + 1);
                self.code.byte(0x45);
                if invert {
                    self.code.byte(0x45);
                }
            }
        }
    }
    fn conditional_jump(&mut self, condition: BranchCondition, target: u32, next: u32) {
        self.refund = 0;
        let internal = self
            .branch_target(target)
            .map(|edge| (false, edge, next))
            .or_else(|| self.branch_target(next).map(|edge| (true, edge, target)));
        if let Some((invert, (label, selector), other)) = internal {
            self.branch_condition(condition, invert);
            // Both successors inherit one canonical record. An internal
            // conditional branch needs neither an if/else nor two tag stores.
            self.stash_flags();
            if let Some(entry) = selector {
                self.code.constant(entry);
                self.code.set(ENTRY);
            }
            self.code.br_if(self.depth - 1 - label);
            self.jump(other);
        } else {
            self.branch_condition(condition, false);
            self.stash_flags();
            self.if_();
            self.jump(target);
            self.code.else_();
            self.jump(next);
            self.end();
        }
    }
    fn dynamic_jump(&mut self) {
        self.stash_flags();
        self.refund = 0;
        self.code.get(PC);
        self.code.binary(0x6b, self.origin);
        self.code.tee(TMP);
        self.code.binary(0x4b, 4095);
        self.if_();
        self.exit(0);
        self.end();
        self.code.constant(self.bindings.entries);
        self.code.get(TMP);
        self.code.binary(0x74, 2);
        self.code.byte(0x6a);
        self.code.load(32, 0);
        self.code.tee(ENTRY);
        self.code.binary(0x76, ENTRY_BITS);
        self.code.constant(self.function_tag);
        self.code.byte(0x47);
        self.if_();
        self.exit(0);
        self.end();
        self.code.get(ENTRY);
        self.code.binary(0x71, ENTRY_MASK);
        self.code.set(ENTRY);
        self.branch(1);
    }
    fn repeat_string(&mut self, instruction: Instruction) {
        let (single, control, stop_equal) = match instruction.op {
            Op::Movs { bits, segment, .. } => (
                Op::Movs {
                    bits,
                    segment,
                    repeat: false,
                },
                (u32::from(bits / 8) << 8) | ((segment as u32) << 16),
                None,
            ),
            Op::String {
                kind,
                bits,
                segment,
                repeat: Some(equal),
            } => (
                Op::String {
                    kind,
                    bits,
                    segment,
                    repeat: None,
                },
                match kind {
                    StringOp::Store => 1,
                    StringOp::Load => 2,
                    StringOp::Compare => unreachable!("filtered by supported"),
                    StringOp::Scan => {
                        if equal {
                            3
                        } else {
                            4
                        }
                    }
                } | (u32::from(bits / 8) << 8)
                    | ((segment as u32) << 16),
                (kind == StringOp::Scan).then_some(!equal),
            ),
            _ => unreachable!(),
        };
        if stop_equal.is_some() {
            self.flush_flags();
        } else {
            // MOVS/STOS/LODS and DF consume no arithmetic status. Preserve
            // the incoming record, including across a partial REP handoff.
            self.stash_flags();
        }
        self.code.constant(0);
        self.code.set(COUNT);
        let done = self.depth;
        self.code.block();
        self.depth += 1;
        self.code.get(REG + 1);
        self.code.byte(0x45);
        self.code.byte(0x0d);
        self.code.u32(0);
        self.repeat_pages(single, control, instruction, done);
        let again = self.depth;
        self.code.loop_();
        self.depth += 1;
        self.repeat = Some((control, instruction.next_eip));
        self.instruction(Instruction {
            op: single,
            ..instruction
        });
        self.repeat = None;
        self.code.get(REG + 1);
        self.code.binary(0x6b, 1);
        self.code.set(REG + 1);
        self.code.get(COUNT);
        self.code.binary(0x6a, 1);
        self.code.set(COUNT);
        // Last completed comparison is visible at a later slow access. The
        // shared continuation consumes the rest of this same 4096-element unit.
        if let Some(equal) = stop_equal {
            self.flush_flags();
            self.flag_bit(6);
            if !equal {
                self.code.byte(0x45);
            }
            self.code.byte(0x0d);
            self.code.u32(self.depth - 1 - done);
        }
        self.code.get(REG + 1);
        self.code.byte(0x45);
        self.code.byte(0x0d);
        self.code.u32(self.depth - 1 - done);
        self.code.get(COUNT);
        self.code
            .binary(0x46, crate::x86::interp::MAX_STRING_REPEAT_UNITS_PER_STEP);
        self.if_();
        self.refund -= 1; // This partial REP unit has consumed its single tick.
        self.exit_const(instruction.eip, 0);
        self.refund += 1;
        self.end();
        self.branch(again);
        self.end();
        self.end();
    }
    /// A forward REP can qualify ordinary RAM once per page. No guest event or
    /// fault can intervene inside that chunk; indices, count and SCAS flags are
    /// committed before qualifying the next page. Keep the shared 4096-element
    /// scheduling unit and the scalar path for reverse/non-flat accesses.
    fn repeat_pages(&mut self, single: Op, control: u32, instruction: Instruction, done: u32) {
        let (bits, segment, source, destination, scan) = match single {
            Op::Movs { bits, segment, .. } => (bits, segment, true, true, None),
            Op::String {
                kind,
                bits,
                segment,
                ..
            } => (
                bits,
                segment,
                kind == StringOp::Load,
                kind != StringOp::Load,
                (kind == StringOp::Scan).then_some(control & 255 == 4),
            ),
            _ => unreachable!(),
        };
        if matches!(segment, SegmentReg::Fs | SegmentReg::Gs) {
            return;
        }
        let write = destination && scan.is_none();
        let permissions = if source { 1 << segment as u32 } else { 0 }
            | if destination {
                1 << (SegmentReg::Es as u32 + if write { 8 } else { 0 })
            } else {
                0
            };
        self.code.get(FLAGS);
        self.code.binary(0x71, 1024);
        self.code.byte(0x45);
        self.code.get(PERMISSIONS);
        self.code.binary(0x71, permissions);
        self.code.binary(0x46, permissions);
        self.code.byte(0x71);
        self.if_();
        let checked = self.checked_segments;
        self.checked_segments |= permissions;
        let page = self.depth;
        self.code.loop_();
        self.depth += 1;
        // REP ends every preceding address-range proof, so BIAS is free to
        // hold the chunk length. memory_at does not use this scratch local.
        self.code
            .constant(crate::x86::interp::MAX_STRING_REPEAT_UNITS_PER_STEP);
        self.code.get(COUNT);
        self.code.byte(0x6b);
        self.code.set(BIAS);
        self.code.get(REG + 1);
        self.repeat_limit();
        let stride = u32::from(bits / 8);
        let shift = stride.trailing_zeros();
        self.repeat = Some((control, instruction.next_eip));
        for (index, segment, write, pointer) in
            [(6, segment, false, SRC), (7, SegmentReg::Es, write, DEST)]
        {
            if (index == 6 && !source) || (index == 7 && !destination) {
                continue;
            }
            self.code.get(REG + index);
            self.code.set(ADDR);
            self.memory_at(segment, bits, write);
            self.code.get(PTR);
            self.code.set(pointer);
            self.code.constant(4096);
            self.code.get(ADDR);
            self.code.binary(0x71, 4095);
            self.code.byte(0x6b);
            self.code.binary(0x76, shift);
            self.repeat_limit();
        }
        self.repeat = None;
        let accumulator = Register {
            index: 0,
            shift: 0,
            bits,
        };
        if let Some(stop_equal) = scan {
            self.reg(accumulator);
            self.code.set(A);
            self.code.get(BIAS);
            self.code.set(TMP);
            let compared = self.depth;
            self.code.block();
            self.depth += 1;
            let element = self.depth;
            self.code.loop_();
            self.depth += 1;
            self.code.get(DEST);
            self.code.load(bits, 0);
            self.code.set(B);
            self.code.get(DEST);
            self.code.binary(0x6a, stride);
            self.code.set(DEST);
            self.code.get(TMP);
            self.code.binary(0x6b, 1);
            self.code.set(TMP);
            self.code.get(A);
            self.code.get(B);
            self.code.byte(if stop_equal { 0x46 } else { 0x47 });
            self.code.byte(0x0d);
            self.code.u32(self.depth - 1 - compared);
            self.code.get(TMP);
            self.code.byte(0x0d);
            self.code.u32(self.depth - 1 - element);
            self.end();
            self.end();
            self.code.get(BIAS);
            self.code.get(TMP);
            self.code.byte(0x6b);
            self.code.set(BIAS);
            self.alu(Alu::Cmp, bits, false);
            self.flush_flags();
        } else if source && destination {
            // memory.copy has memmove semantics. Forward x86 MOVS instead
            // propagates bytes when the destination overwrites a later source
            // element. Compare backing pointers to include aliased guest pages.
            self.code.get(BIAS);
            self.code.binary(0x74, shift);
            self.code.set(TMP);
            self.code.get(DEST);
            self.code.get(SRC);
            self.code.byte(0x4d);
            self.code.get(DEST);
            self.code.get(SRC);
            self.code.byte(0x6b);
            self.code.get(TMP);
            self.code.byte(0x4f);
            self.code.byte(0x72);
            self.if_();
            self.code.get(DEST);
            self.code.get(SRC);
            self.code.get(TMP);
            self.code.0.extend_from_slice(&[0xfc, 10, 0, 0]); // memory.copy
            self.code.else_();
            self.repeat_copy_loop(bits, true);
            self.end();
        } else if destination {
            if bits != 8 {
                self.reg(accumulator);
                self.code.get(REG);
                self.code.binary(0x71, 255);
                self.code
                    .binary(0x6c, if bits == 16 { 0x0101 } else { 0x01010101 });
                self.code.byte(0x46);
                self.if_();
            }
            self.code.get(DEST);
            self.code.get(REG);
            self.code.get(BIAS);
            self.code.binary(0x74, shift);
            self.code.0.extend_from_slice(&[0xfc, 11, 0]); // memory.fill
            if bits != 8 {
                self.code.else_();
                self.repeat_copy_loop(bits, false);
                self.end();
            }
        } else {
            // All reads are in an already qualified RAM page; only the last
            // LODS result is observable before the next page/event boundary.
            self.code.get(SRC);
            self.code.get(BIAS);
            self.code.binary(0x6b, 1);
            self.code.binary(0x74, shift);
            self.code.byte(0x6a);
            self.code.load(bits, 0);
            self.put_reg(accumulator);
        }
        self.code.get(REG + 1);
        self.code.get(BIAS);
        self.code.byte(0x6b);
        self.code.set(REG + 1);
        self.code.get(COUNT);
        self.code.get(BIAS);
        self.code.byte(0x6a);
        self.code.set(COUNT);
        for (index, used) in [(6, source), (7, destination)] {
            if used {
                self.code.get(REG + index);
                self.code.get(BIAS);
                self.code.binary(0x74, shift);
                self.code.byte(0x6a);
                self.code.set(REG + index);
            }
        }
        if let Some(stop_equal) = scan {
            self.flag_bit(6);
            if !stop_equal {
                self.code.byte(0x45);
            }
            self.code.byte(0x0d);
            self.code.u32(self.depth - 1 - done);
        }
        self.code.get(REG + 1);
        self.code.byte(0x45);
        self.code.byte(0x0d);
        self.code.u32(self.depth - 1 - done);
        self.code.get(COUNT);
        self.code
            .binary(0x46, crate::x86::interp::MAX_STRING_REPEAT_UNITS_PER_STEP);
        self.if_();
        self.refund -= 1;
        self.exit_const(instruction.eip, 0);
        self.refund += 1;
        self.end();
        self.branch(page);
        self.end();
        self.end();
        self.checked_segments = checked;
    }
    // Clamp BIAS to the element count on the operand stack.
    fn repeat_limit(&mut self) {
        self.code.tee(TMP);
        self.code.get(BIAS);
        self.code.byte(0x49);
        self.if_();
        self.code.get(TMP);
        self.code.set(BIAS);
        self.end();
    }
    // Proven RAM only: sequential width-sized stores preserve destructive
    // MOVS overlap and arbitrary STOS patterns. No qualifiers inside the loop.
    fn repeat_copy_loop(&mut self, bits: u8, copy: bool) {
        self.code.get(BIAS);
        self.code.set(TMP);
        let element = self.depth;
        self.code.loop_();
        self.depth += 1;
        self.code.get(DEST);
        if copy {
            self.code.get(SRC);
            self.code.load(bits, 0);
        } else {
            self.code.get(REG);
        }
        self.code.store(bits, 0);
        for pointer in if copy { &[SRC, DEST][..] } else { &[DEST][..] } {
            self.code.get(*pointer);
            self.code.binary(0x6a, u32::from(bits / 8));
            self.code.set(*pointer);
        }
        self.code.get(TMP);
        self.code.binary(0x6b, 1);
        self.code.tee(TMP);
        self.code.byte(0x0d);
        self.code.u32(self.depth - 1 - element);
        self.end();
    }
    fn string_index(&mut self, index: u32, bits: u8) {
        let stride = u32::from(bits / 8);
        self.code.get(REG + index);
        self.code.binary(0x6a, stride);
        self.code.get(FLAGS);
        self.code.binary(0x71, 1024);
        self.code.binary(0x76, 9); // 0 or 2, selected by DF
        self.code.binary(0x6c, stride);
        self.code.byte(0x6b);
        self.code.set(REG + index);
    }
    fn instruction(&mut self, insn: Instruction) {
        match insn.op {
            Op::Mmx { .. } | Op::PushSegment { .. } | Op::PopSegment { .. } => {
                unreachable!("filtered by supported")
            }
            Op::String {
                repeat: Some(_), ..
            }
            | Op::Movs { repeat: true, .. } => self.repeat_string(insn),
            Op::Movs {
                bits,
                repeat: false,
                segment,
            } => {
                self.code.get(REG + 6);
                self.code.set(ADDR);
                self.memory_at(segment, bits, false);
                self.code.get(PTR);
                self.code.load(bits, 0);
                self.code.set(SRC);
                self.code.get(REG + 7);
                self.code.set(ADDR);
                self.memory_at(SegmentReg::Es, bits, true);
                self.code.get(PTR);
                self.code.get(SRC);
                self.code.store(bits, 0);
                self.string_index(6, bits);
                self.string_index(7, bits);
            }
            Op::String {
                kind,
                bits,
                segment,
                repeat: None,
            } => {
                let index = if kind == StringOp::Load { 6 } else { 7 };
                self.code.get(REG + index);
                self.code.set(ADDR);
                self.memory_at(segment, bits, kind == StringOp::Store);
                self.code.get(PTR);
                let accumulator = Register {
                    index: 0,
                    shift: 0,
                    bits,
                };
                match kind {
                    StringOp::Compare => unreachable!("filtered by supported"),
                    StringOp::Store => {
                        self.reg(accumulator);
                        self.code.store(bits, 0);
                    }
                    StringOp::Load => {
                        self.code.load(bits, 0);
                        self.put_reg(accumulator);
                    }
                    StringOp::Scan => {
                        self.code.load(bits, 0);
                        self.code.set(B);
                        self.reg(accumulator);
                        self.code.set(A);
                        self.alu(Alu::Cmp, bits, false);
                    }
                }
                self.string_index(index, bits);
            }
            Op::Nop => (),
            Op::Mov { dst, src } => {
                self.value(src);
                self.put_reg(dst);
            }
            Op::ByteSwap { dst } => {
                // Keep pending arithmetic operands intact. Wasm has rotates
                // but no byte-reversal instruction; exchange the byte lanes.
                self.reg(dst);
                self.code.binary(0x77, 8);
                self.code.binary(0x71, 0x00ff00ff);
                self.reg(dst);
                self.code.binary(0x78, 8);
                self.code.binary(0x71, 0xff00ff00);
                self.code.byte(0x72);
                self.put_reg(dst);
            }
            Op::ConditionalMove {
                dst,
                src,
                condition,
            } => {
                // Memory is unconditionally qualified/read before selection.
                self.value(src);
                self.code.set(SRC);
                self.code.get(SRC);
                self.reg(dst);
                self.condition(condition);
                self.code.byte(0x1b);
                self.put_reg(dst);
            }
            Op::LoadFlagsByte => {
                self.flush_flags();
                self.code.get(FLAGS);
                self.code.binary(0x71, 0xd5);
                self.code.binary(0x72, 2);
                self.put_reg(Register {
                    index: 0,
                    shift: 8,
                    bits: 8,
                });
            }
            Op::StoreFlagsByte => {
                self.flush_flags();
                self.code.get(FLAGS);
                self.code.binary(0x71, !0xd5);
                self.reg(Register {
                    index: 0,
                    shift: 8,
                    bits: 8,
                });
                self.code.binary(0x71, 0xd5);
                self.code.byte(0x72);
                self.code.set(FLAGS);
            }
            Op::Carry { value } => {
                self.flush_flags();
                self.code.get(FLAGS);
                match value {
                    Some(false) => self.code.binary(0x71, !1),
                    Some(true) => self.code.binary(0x72, 1),
                    None => self.code.binary(0x73, 1),
                }
                self.code.set(FLAGS);
            }
            Op::Store { address, bits, src } => {
                self.value(src);
                self.code.set(SRC);
                self.memory(address, bits, true);
                self.code.get(PTR);
                self.code.get(SRC);
                self.code.store(bits, 0);
            }
            Op::Alu { kind, dst, src } => {
                self.value(src);
                self.code.set(SRC);
                self.flags_input(kind, false);
                self.reg(dst);
                self.code.set(A);
                self.code.get(SRC);
                self.mask(dst.bits);
                self.code.set(B);
                self.alu(kind, dst.bits, false);
                if !matches!(kind, Alu::Cmp | Alu::Test) {
                    self.code.get(R);
                    self.put_reg(dst);
                }
            }
            Op::MemoryAlu {
                kind,
                address,
                bits,
                src,
                preserve_carry,
            } => {
                self.value(src);
                self.code.set(SRC);
                self.memory(address, bits, true);
                self.flags_input(kind, preserve_carry);
                self.code.get(PTR);
                self.code.load(bits, 0);
                self.code.set(A);
                self.code.get(SRC);
                self.mask(bits);
                self.code.set(B);
                self.alu(kind, bits, preserve_carry);
                self.code.get(PTR);
                self.code.get(R);
                self.code.store(bits, 0);
            }
            Op::Compare {
                lhs,
                rhs,
                bits,
                test,
            } => {
                self.value(lhs);
                self.code.set(SRC);
                self.value(rhs);
                self.code.set(TMP);
                self.code.get(SRC);
                self.mask(bits);
                self.code.set(A);
                self.code.get(TMP);
                self.mask(bits);
                self.code.set(B);
                self.alu(if test { Alu::Test } else { Alu::Cmp }, bits, false);
            }
            Op::IncDec { dst, decrement } => {
                self.flush_carry();
                self.reg(dst);
                self.code.set(A);
                self.code.constant(1);
                self.code.set(B);
                self.alu(if decrement { Alu::Sub } else { Alu::Add }, dst.bits, true);
                self.code.get(R);
                self.put_reg(dst);
            }
            Op::Negate { dst } => {
                self.reg(dst);
                self.code.set(B);
                self.code.constant(0);
                self.code.set(A);
                self.alu(Alu::Sub, dst.bits, false);
                self.code.get(R);
                self.put_reg(dst);
            }
            Op::Not { dst } => {
                self.reg(dst);
                self.code.binary(0x73, u32::MAX);
                self.put_reg(dst);
            }
            Op::Direction { backward } => {
                self.code.get(FLAGS);
                self.code.binary(
                    if backward { 0x72 } else { 0x71 },
                    if backward { 1024 } else { !1024 },
                );
                self.code.set(FLAGS);
            }
            Op::ClearInterrupt => {
                self.field(offset_of!(Frame, popf_mask));
                self.code.binary(0x71, 512);
                self.code.byte(0x45);
                self.fail_if();
                self.code.get(FLAGS);
                self.code.binary(0x71, !512);
                self.code.set(FLAGS);
            }
            Op::LoadSegment { segment, src } => {
                // Fetch the word operand with the ordinary generated memory
                // guards, but leave the descriptor/privilege semantics to the
                // shared core. No following instruction may use old segment
                // qualifications. The uncharged instruction can fault there.
                self.value(src);
                self.code.set(SRC);
                for (offset, value) in [
                    (offset_of!(SegmentLoad, segment), segment as u32),
                    (offset_of!(SegmentLoad, next_eip), insn.next_eip),
                ] {
                    self.code.get(F);
                    self.code.constant(value);
                    self.code
                        .store(32, (offset_of!(Frame, segment_load) + offset) as u32);
                }
                self.code.get(F);
                self.code.get(SRC);
                self.code.store(
                    32,
                    (offset_of!(Frame, segment_load) + offset_of!(SegmentLoad, selector)) as u32,
                );
                self.exit_const(insn.eip, 7);
            }
            Op::SetInterrupt => {
                self.field(offset_of!(Frame, popf_mask));
                self.code.binary(0x71, 512);
                self.code.byte(0x45);
                self.fail_if();
                self.code.get(FLAGS);
                self.code.binary(0x71, 512);
                self.code.set(TMP);
                self.code.get(FLAGS);
                self.code.binary(0x72, 512);
                self.code.set(FLAGS);
                self.refund -= 1;
                self.code.get(TMP);
                self.if_();
                self.exit_const(insn.next_eip, 3);
                self.code.else_();
                self.exit_const(insn.next_eip, 4);
                self.end();
            }
            Op::PopFlags { bits } => {
                self.stack_guard();
                self.code.get(REG + 4);
                self.code.set(ADDR);
                self.memory_at(SegmentReg::Ss, bits, false);
                self.code.get(PTR);
                self.code.load(bits, 0);
                self.code.set(SRC);
                self.code.get(SRC);
                self.code.binary(0x71, 256);
                self.fail_if();
                // Every admitted POPF mask replaces all arithmetic status.
                // Its qualified stack read/TF rejection precede discarding it.
                self.pending = None;
                self.clear_flag_record();
                self.field(offset_of!(Frame, popf_mask));
                self.mask(bits);
                self.code.set(TMP);
                if bits == 32 {
                    self.code.get(SRC);
                    self.code.binary(0x71, !(1 << 16));
                    self.code.set(SRC);
                }
                self.code.get(FLAGS);
                self.code.get(SRC);
                self.code.byte(0x73);
                self.code.get(TMP);
                self.code.byte(0x71);
                self.code.set(TMP);
                self.code.get(FLAGS);
                self.code.get(TMP);
                self.code.byte(0x73);
                self.code.binary(0x72, 2);
                self.code.set(FLAGS);
                self.code.get(REG + 4);
                self.code.binary(0x6a, u32::from(bits / 8));
                self.code.set(REG + 4);
                self.code.get(TMP);
                self.code.get(FLAGS);
                self.code.byte(0x71);
                self.code.binary(0x71, 512);
                self.if_();
                self.refund -= 1;
                self.exit_const(insn.next_eip, 3);
                self.refund += 1;
                self.end();
            }
            Op::Lea { dst, address } => {
                self.address(address);
                self.put_reg(dst);
            }
            Op::Extend {
                dst,
                src,
                source_bits,
                signed,
            } => {
                self.value(src);
                if signed {
                    self.code.binary(0x74, 32 - u32::from(source_bits));
                    self.code.binary(0x75, 32 - u32::from(source_bits));
                }
                self.put_reg(dst);
            }
            Op::SignExtendPair { bits } => {
                self.code.get(REG);
                self.code.binary(0x74, 32 - u32::from(bits));
                self.code.binary(0x75, 31);
                self.put_reg(Register {
                    index: 2,
                    shift: 0,
                    bits,
                });
            }
            Op::ReadSegment { dst, segment } => {
                self.field(offset_of!(Frame, selectors) + segment as usize * 4);
                self.code.set(SRC);
                match dst {
                    Value::Register(r) => {
                        self.code.get(SRC);
                        self.put_reg(r);
                    }
                    Value::Memory { address, bits } => {
                        self.memory(address, bits, true);
                        self.code.get(PTR);
                        self.code.get(SRC);
                        self.code.store(bits, 0);
                    }
                    _ => unreachable!(),
                }
            }
            Op::SetCondition { dst, condition } => {
                self.condition(condition);
                self.code.set(SRC);
                match dst {
                    Value::Register(r) => {
                        self.code.get(SRC);
                        self.put_reg(r);
                    }
                    Value::Memory { address, bits } => {
                        self.memory(address, bits, true);
                        self.code.get(PTR);
                        self.code.get(SRC);
                        self.code.store(bits, 0);
                    }
                    _ => unreachable!(),
                }
            }
            Op::Push { src, bits } => {
                self.value(src);
                self.code.set(SRC);
                self.push(bits);
            }
            Op::PushFlags { bits } => {
                self.flush_flags();
                self.code.get(FLAGS);
                self.code.binary(0x71, !((1 << 16) | (1 << 17)));
                self.code.set(SRC);
                self.push(bits);
            }
            Op::Pop { dst } => {
                self.pop(dst.bits);
                self.code.get(SRC);
                self.put_reg(dst);
            }
            Op::Leave { bits } => {
                self.stack_guard();
                self.code.get(REG + 5);
                self.code.set(ADDR);
                self.memory_at(SegmentReg::Ss, bits, false);
                self.code.get(PTR);
                self.code.load(bits, 0);
                self.code.set(SRC);
                self.code.get(REG + 5);
                self.code.binary(0x6a, u32::from(bits / 8));
                self.code.set(REG + 4);
                self.code.get(SRC);
                self.put_reg(Register {
                    index: 5,
                    shift: 0,
                    bits,
                });
            }
            Op::Call { target, bits } => {
                if let Value::Immediate(target) = target {
                    self.code.constant(insn.next_eip);
                    self.code.set(SRC);
                    self.push(bits);
                    self.jump(if bits == 16 { target & 0xffff } else { target });
                    return;
                }
                self.value(target);
                self.code.set(COUNT);
                self.code.constant(insn.next_eip);
                self.code.set(SRC);
                self.push(bits);
                self.code.get(COUNT);
                self.mask(bits);
                self.code.set(PC);
                self.dynamic_jump();
            }
            Op::Return { bits, pop } => {
                self.pop(bits);
                self.code.get(SRC);
                self.code.set(PC);
                self.code.get(REG + 4);
                self.code.binary(0x6a, u32::from(pop));
                self.code.set(REG + 4);
                self.dynamic_jump();
            }
            Op::IndirectJump { src } => {
                self.value(src);
                self.code.set(PC);
                self.dynamic_jump();
            }
            Op::Branch { condition, target } => {
                if let Some(condition) = condition {
                    self.conditional_jump(condition, target, insn.next_eip);
                } else {
                    self.jump(target);
                }
            }
            Op::Exchange { kind, dst, src } => self.exchange(kind, dst, src),
            Op::BitScan { dst, src, reverse } => {
                self.value(src);
                self.code.set(SRC);
                self.flush_flags();
                self.code.get(SRC);
                self.code.byte(0x45);
                self.set_flag(6);
                self.code.get(TMP);
                self.code.byte(0x45);
                self.if_();
                self.code.get(SRC);
                self.code.byte(if reverse { 0x67 } else { 0x68 });
                if reverse {
                    self.code.constant(31);
                    self.code.byte(0x73);
                }
                self.put_reg(dst);
                self.end();
            }
            Op::Shift { dst, kind, count } => self.shift(dst, kind, count),
            Op::DoubleShift {
                dst,
                src,
                right,
                count,
            } => self.double_shift(dst, src, right, count),
            Op::Imul { dst, lhs, rhs } => {
                self.value(lhs);
                self.code.set(SRC);
                self.value(rhs);
                self.code.set(DEST);
                self.flush_flags();
                self.product(dst.bits, true);
                self.product_flags(dst.bits, true);
                self.code.get(R);
                self.put_reg(dst);
            }
            Op::Multiply { src, bits, signed } => {
                self.value(src);
                self.code.set(DEST);
                self.reg(Register {
                    index: 0,
                    shift: 0,
                    bits,
                });
                self.code.set(SRC);
                self.flush_flags();
                self.product(bits, signed);
                self.product_flags(bits, signed);
                self.code.get(R);
                self.put_reg(Register {
                    index: 0,
                    shift: 0,
                    bits: if bits == 8 { 16 } else { bits },
                });
                if bits != 8 {
                    self.code.get(WIDE);
                    self.code.byte(0x42);
                    self.code.i32(i32::from(bits));
                    self.code.byte(0x88);
                    self.code.byte(0xa7);
                    self.put_reg(Register {
                        index: 2,
                        shift: 0,
                        bits,
                    });
                }
            }
            Op::BitTest {
                src,
                index,
                bits,
                kind,
            } => self.bit_test(src, index, bits, kind),
            Op::Divide { src, bits, signed } => self.divide(src, bits, signed),
            Op::CompareExchange8 { address } => {
                self.memory(address, 64, true);
                self.code.get(PTR);
                self.code.load(64, 0);
                self.code.set(WIDE);
                self.flush_flags();
                self.pair(REG + 2, REG);
                self.code.get(WIDE);
                self.code.byte(0x51);
                self.set_flag(6);
                self.code.get(TMP);
                self.if_();
                self.code.get(PTR);
                self.pair(REG + 1, REG + 3);
                self.code.store(64, 0);
                self.code.else_();
                self.code.get(WIDE);
                self.code.byte(0xa7);
                self.code.set(REG);
                self.code.get(WIDE);
                self.code.byte(0x42);
                self.code.i32(32);
                self.code.byte(0x88);
                self.code.byte(0xa7);
                self.code.set(REG + 2);
                self.end();
            }
        }
    }
    fn pair(&mut self, high: u32, low: u32) {
        self.code.get(high);
        self.code.byte(0xad);
        self.code.byte(0x42);
        self.code.i32(32);
        self.code.byte(0x86);
        self.code.get(low);
        self.code.byte(0xad);
        self.code.byte(0x84);
    }
    fn divide(&mut self, src: Value, bits: u8, signed: bool) {
        self.value(src);
        self.code.set(SRC);
        self.code.get(SRC);
        self.code.byte(0x45);
        self.fail_if();
        self.code.get(SRC);
        if signed && bits < 32 {
            self.code.binary(0x74, 32 - u32::from(bits));
            self.code.binary(0x75, 32 - u32::from(bits));
        }
        self.code.byte(if signed { 0xac } else { 0xad });
        self.code.set(DIVISOR);
        if bits == 32 {
            self.pair(REG + 2, REG);
        } else {
            if bits == 16 {
                self.reg(Register {
                    index: 2,
                    shift: 0,
                    bits,
                });
                self.code.binary(0x74, 16);
                self.reg(Register {
                    index: 0,
                    shift: 0,
                    bits,
                });
                self.code.byte(0x72);
            } else {
                self.reg(Register {
                    index: 0,
                    shift: 0,
                    bits: 16,
                });
            }
            if signed && bits == 8 {
                self.code.binary(0x74, 16);
                self.code.binary(0x75, 16);
            }
            self.code.byte(if signed { 0xac } else { 0xad });
        }
        self.code.set(WIDE);
        if signed && bits == 32 {
            self.code.get(WIDE);
            self.code.byte(0x42);
            self.code.i32(1);
            self.code.byte(0x42);
            self.code.i32(63);
            self.code.byte(0x86);
            self.code.byte(0x51);
            self.code.get(DIVISOR);
            self.code.byte(0x42);
            self.code.i32(-1);
            self.code.byte(0x51);
            self.code.byte(0x71);
            self.fail_if();
        }
        self.code.get(WIDE);
        self.code.get(DIVISOR);
        self.code.byte(if signed { 0x7f } else { 0x80 });
        self.code.tee(QUOTIENT);
        if signed {
            self.code.get(QUOTIENT);
            self.code.byte(0xa7);
            if bits < 32 {
                self.code.binary(0x74, 32 - u32::from(bits));
                self.code.binary(0x75, 32 - u32::from(bits));
            }
            self.code.byte(0xac);
            self.code.byte(0x52);
        } else {
            self.code.constant(if bits == 32 {
                u32::MAX
            } else {
                (1 << bits) - 1
            });
            self.code.byte(0xad);
            self.code.byte(0x56);
        }
        self.fail_if(); // #DE is delivered by interpretation, never a Wasm trap.
        self.code.get(QUOTIENT);
        self.code.byte(0xa7);
        self.put_reg(Register {
            index: 0,
            shift: 0,
            bits,
        });
        self.code.get(WIDE);
        self.code.get(DIVISOR);
        self.code.byte(if signed { 0x81 } else { 0x82 });
        self.code.byte(0xa7);
        self.put_reg(Register {
            index: if bits == 8 { 0 } else { 2 },
            shift: if bits == 8 { 8 } else { 0 },
            bits,
        });
    }
    fn product(&mut self, bits: u8, signed: bool) {
        for local in [SRC, DEST] {
            self.code.get(local);
            if signed && bits < 32 {
                self.code.binary(0x74, 32 - u32::from(bits));
                self.code.binary(0x75, 32 - u32::from(bits));
            } else {
                self.mask(bits);
            }
            self.code.byte(if signed { 0xac } else { 0xad });
        }
        self.code.byte(0x7e);
        self.code.tee(WIDE);
        self.code.byte(0xa7);
        self.code.set(R);
    }
    fn product_flags(&mut self, bits: u8, signed: bool) {
        self.code.get(WIDE);
        if signed {
            self.code.get(R);
            if bits < 32 {
                self.code.binary(0x74, 32 - u32::from(bits));
                self.code.binary(0x75, 32 - u32::from(bits));
            }
            self.code.byte(0xac);
            self.code.byte(0x52);
        } else {
            self.code.byte(0x42);
            self.code.i32(i32::from(bits));
            self.code.byte(0x88);
            self.code.byte(0x50);
            self.code.byte(0x45);
        }
        self.code.set(CARRY);
        self.code.get(FLAGS);
        self.code.binary(0x71, !0x801);
        self.code.get(CARRY);
        self.code.byte(0x72);
        self.code.get(CARRY);
        self.code.binary(0x74, 11);
        self.code.byte(0x72);
        self.code.set(FLAGS);
    }
    fn write_value(&mut self, dst: Value, local: u32) {
        match dst {
            Value::Register(r) => {
                self.code.get(local);
                self.put_reg(r);
            }
            Value::Memory { bits, .. } => {
                self.code.get(PTR);
                self.code.get(local);
                self.code.store(bits, 0);
            }
            _ => unreachable!(),
        }
    }
    fn exchange(&mut self, kind: ExchangeOp, dst: Value, src: Register) {
        self.reg(src);
        self.code.set(SRC);
        if let Value::Memory { address, bits } = dst {
            self.memory(address, bits, true);
            self.code.get(PTR);
            self.code.load(bits, 0);
        } else {
            self.value(dst);
        }
        self.code.set(DEST);
        match kind {
            ExchangeOp::Swap => {
                self.code.get(DEST);
                self.put_reg(src);
                self.write_value(dst, SRC);
            }
            ExchangeOp::Add => {
                self.code.get(DEST);
                self.code.set(A);
                self.code.get(SRC);
                self.code.set(B);
                self.alu(Alu::Add, src.bits, false);
                self.code.get(DEST);
                self.put_reg(src);
                self.write_value(dst, R);
            }
            ExchangeOp::Compare => {
                self.reg(Register {
                    index: 0,
                    shift: 0,
                    bits: src.bits,
                });
                self.code.set(A);
                self.code.get(DEST);
                self.code.set(B);
                self.alu(Alu::Cmp, src.bits, false);
                self.code.get(A);
                self.code.get(B);
                self.code.byte(0x46);
                self.if_();
                self.write_value(dst, SRC);
                self.code.else_();
                self.code.get(DEST);
                self.put_reg(Register {
                    index: 0,
                    shift: 0,
                    bits: src.bits,
                });
                self.end();
            }
        }
    }
    fn bit_test(&mut self, src: Value, index: Value, bits: u8, kind: u8) {
        self.value(index);
        self.code.set(COUNT);
        if let Value::Memory { address, .. } = src {
            self.address(address);
            self.code.set(ADDR);
            if matches!(index, Value::Register(_)) {
                self.code.get(COUNT);
                if bits < 32 {
                    self.code.binary(0x74, 32 - u32::from(bits));
                    self.code.binary(0x75, 32 - u32::from(bits));
                }
                self.code.binary(0x75, bits.trailing_zeros());
                self.code.binary(0x74, bits.trailing_zeros() - 3);
                self.code.get(ADDR);
                self.code.byte(0x6a);
                self.code.set(ADDR);
            }
            self.memory_at(address.segment, bits, kind != 0);
            self.code.get(PTR);
            self.code.load(bits, 0);
        } else {
            self.value(src);
        }
        self.code.set(SRC);
        self.flush_flags();
        self.code.get(COUNT);
        self.code.binary(0x71, u32::from(bits - 1));
        self.code.set(COUNT);
        self.code.get(SRC);
        self.code.get(COUNT);
        self.code.byte(0x76);
        self.code.binary(0x71, 1);
        self.code.set(CARRY);
        self.code.get(FLAGS);
        self.code.binary(0x71, !1);
        self.code.get(CARRY);
        self.code.byte(0x72);
        self.code.set(FLAGS);
        if kind != 0 {
            self.code.get(SRC);
            self.code.constant(1);
            self.code.get(COUNT);
            self.code.byte(0x74);
            if kind == 2 {
                self.code.binary(0x73, u32::MAX);
            }
            self.code.byte(match kind {
                1 => 0x72,
                2 => 0x71,
                _ => 0x73,
            });
            self.code.set(R);
            self.write_value(src, R);
        }
    }
    fn result_flags(&mut self, bits: u8) {
        self.code.get(FLAGS);
        self.code.binary(0x71, !0xc4);
        self.code.get(R);
        self.code.byte(0x45);
        self.code.binary(0x74, 6);
        self.code.byte(0x72);
        self.code.get(R);
        self.code.binary(0x76, u32::from(bits - 1));
        self.code.binary(0x74, 7);
        self.code.byte(0x72);
        self.code.get(R);
        self.code.binary(0x71, 255);
        self.code.byte(0x69);
        self.code.binary(0x71, 1);
        self.code.byte(0x45);
        self.code.binary(0x74, 2);
        self.code.byte(0x72);
        self.code.set(FLAGS);
    }
    fn shift(&mut self, dst: Value, kind: u8, count: Value) {
        let bits = match dst {
            Value::Register(r) => r.bits,
            Value::Memory { bits, .. } => bits,
            _ => unreachable!(),
        };
        if let Value::Memory { address, .. } = dst {
            // A zero count may need only a read. A denied write exits before
            // any access and the interpreter supplies that precise behavior.
            self.memory(address, bits, true);
            self.code.get(PTR);
            self.code.load(bits, 0);
        } else {
            self.value(dst);
        }
        self.code.set(SRC);
        self.value(count);
        self.code.binary(0x71, 31);
        if kind >= 5 && bits < 32 {
            self.code.binary(0x70, u32::from(bits + 1)); // rem_u: 9/17-bit ring
        }
        self.code.set(COUNT);
        if kind < 3 {
            // Zero counts preserve the incoming record; nonzero shifts replace
            // it after the memory qualifier, without eagerly calculating SZP.
            self.stash_flags();
        } else {
            // Rotates keep SZAP and therefore still consume the old record.
            self.flush_flags();
        }
        self.code.get(COUNT);
        self.if_();
        self.code.get(SRC);
        self.code.set(A);
        if kind == 2 && bits < 32 {
            self.code.get(A);
            self.code.binary(0x74, 32 - u32::from(bits));
            self.code.binary(0x75, 32 - u32::from(bits));
            self.code.set(A);
        }
        if kind < 3 {
            self.code.get(A);
            self.code.get(COUNT);
            self.code.byte(match kind {
                0 => 0x74,
                1 => 0x76,
                _ => 0x75,
            });
            self.mask(bits);
            self.code.set(R);
            self.code.get(A);
            if kind == 0 {
                self.code.constant(u32::from(bits));
                self.code.get(COUNT);
                self.code.byte(0x6b);
            } else {
                self.code.get(COUNT);
                self.code.binary(0x6b, 1);
            }
            self.code.byte(0x76);
            self.code.binary(0x71, 1);
            if kind == 0 && bits < 32 {
                self.code.get(COUNT);
                self.code.binary(0x4d, u32::from(bits));
                self.code.byte(0x71);
            }
            self.code.set(CARRY);
            if kind == 0 {
                self.code.get(R);
                self.code.binary(0x76, u32::from(bits - 1));
                self.code.get(CARRY);
                self.code.byte(0x73);
            } else if kind == 1 {
                self.code.get(A);
                self.code.binary(0x76, u32::from(bits - 1));
                self.code.get(COUNT);
                self.code.binary(0x46, 1);
                self.code.byte(0x71);
            } else {
                self.code.constant(0);
            }
            self.code.set(A); // Explicit OF, matching Cpu's shift record.
            self.code.constant(6 | (u32::from(bits - 1) << 8));
            self.code.get(CARRY);
            self.code.binary(0x74, 6);
            self.code.byte(0x72);
            self.code.set(FLAG_KIND);
        } else {
            if kind >= 5 {
                self.code.get(A);
                self.code.byte(0xad);
                self.flag_bit(0);
                self.code.byte(0xad);
                self.code.byte(0x42);
                self.code.i32(i32::from(bits));
                self.code.byte(0x86);
                self.code.byte(0x84);
                self.code.set(WIDE);
                self.code.get(WIDE);
                self.code.get(COUNT);
                self.code.byte(0xad);
                self.code.byte(if kind == 5 { 0x86 } else { 0x88 });
                self.code.get(WIDE);
                self.code.constant(u32::from(bits + 1));
                self.code.get(COUNT);
                self.code.byte(0x6b);
                self.code.byte(0xad);
                self.code.byte(if kind == 5 { 0x88 } else { 0x86 });
                self.code.byte(0x84);
                self.code.tee(WIDE);
                self.code.byte(0xa7);
                self.mask(bits);
                self.code.set(R);
                self.code.get(WIDE);
                self.code.byte(0x42);
                self.code.i32(i32::from(bits));
                self.code.byte(0x88);
                self.code.byte(0xa7);
                self.code.binary(0x71, 1);
            } else {
                self.code.get(COUNT);
                self.code.binary(0x71, u32::from(bits - 1));
                self.code.set(COUNT);
                self.code.get(A);
                self.code.get(COUNT);
                self.code.byte(if kind == 3 { 0x76 } else { 0x74 });
                self.code.get(A);
                self.code.constant(u32::from(bits));
                self.code.get(COUNT);
                self.code.byte(0x6b);
                self.code.byte(if kind == 3 { 0x74 } else { 0x76 });
                self.code.byte(0x72);
                self.mask(bits);
                self.code.set(R);
                self.code.get(R);
                if kind == 3 {
                    self.code.binary(0x76, u32::from(bits - 1));
                } else {
                    self.code.binary(0x71, 1);
                }
            }
            self.code.set(CARRY);
            self.code.get(FLAGS);
            self.code.binary(0x71, !0x801);
            self.code.get(CARRY);
            self.code.byte(0x72);
            self.code.get(R);
            self.code.binary(0x76, u32::from(bits - 1));
            if matches!(kind, 3 | 6) {
                self.code.get(R);
                self.code.binary(0x76, u32::from(bits - 2));
                self.code.binary(0x71, 1);
            } else {
                self.code.get(CARRY);
            }
            self.code.byte(0x73);
            self.code.binary(0x74, 11);
            self.code.byte(0x72);
            self.code.set(FLAGS);
        }
        match dst {
            Value::Register(r) => {
                self.code.get(R);
                self.put_reg(r);
            }
            Value::Memory { .. } => {
                self.code.get(PTR);
                self.code.get(R);
                self.code.store(bits, 0);
            }
            _ => unreachable!(),
        }
        self.end();
    }
    fn double_shift(&mut self, dst: Register, src: Register, right: bool, count: Value) {
        assert_eq!(dst.bits, 32);
        self.flush_flags();
        self.value(count);
        self.code.binary(0x71, 31);
        self.code.set(COUNT);
        self.code.get(COUNT);
        self.if_();
        self.reg(dst);
        self.code.set(A);
        self.reg(src);
        self.code.set(B);
        self.code.get(A);
        self.code.get(COUNT);
        self.code.byte(if right { 0x76 } else { 0x74 });
        self.code.get(B);
        self.code.constant(32);
        self.code.get(COUNT);
        self.code.byte(0x6b);
        self.code.byte(if right { 0x74 } else { 0x76 });
        self.code.byte(0x72);
        self.code.set(R);
        self.code.get(A);
        if right {
            self.code.get(COUNT);
            self.code.binary(0x6b, 1);
        } else {
            self.code.constant(32);
            self.code.get(COUNT);
            self.code.byte(0x6b);
        }
        self.code.byte(0x76);
        self.code.binary(0x71, 1);
        self.code.set(CARRY);
        self.result_flags(32);
        self.code.get(FLAGS);
        self.code.binary(0x71, !1);
        self.code.get(CARRY);
        self.code.byte(0x72);
        self.code.set(FLAGS);
        self.code.get(COUNT);
        self.code.binary(0x46, 1);
        self.if_();
        self.code.get(FLAGS);
        self.code.binary(0x71, !0x800);
        self.code.get(A);
        self.code.get(R);
        self.code.byte(0x73);
        self.code.binary(0x76, 31);
        self.code.binary(0x74, 11);
        self.code.byte(0x72);
        self.code.set(FLAGS);
        self.end();
        self.code.get(R);
        self.put_reg(dst);
        self.end();
    }
    fn stack_pointer(&mut self, bits: u8, write: bool) {
        if let Some(offset) = self.stack_offset {
            self.code.get(BIAS);
            if offset != 0 {
                self.code.binary(0x6a, offset);
            }
            self.code.set(PTR);
        } else {
            self.stack_guard();
            self.code.get(REG + 4);
            if write {
                self.code.binary(0x6b, u32::from(bits / 8));
            }
            self.code.set(ADDR);
            self.memory_at(SegmentReg::Ss, bits, write);
        }
    }
    fn push(&mut self, bits: u8) {
        self.stack_pointer(bits, true);
        self.code.get(PTR);
        self.code.get(SRC);
        self.code.store(bits, 0);
        self.code.get(REG + 4);
        self.code.binary(0x6b, u32::from(bits / 8));
        self.code.set(REG + 4);
    }
    fn pop(&mut self, bits: u8) {
        self.stack_pointer(bits, false);
        self.code.get(PTR);
        self.code.load(bits, 0);
        self.code.set(SRC);
        self.code.get(REG + 4);
        self.code.binary(0x6a, u32::from(bits / 8));
        self.code.set(REG + 4);
    }
}
