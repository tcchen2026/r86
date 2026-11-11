//! i386 -> AArch64 translation. This is the implementation and contract index;
//! keep details beside the code that enforces them, rather than in a second plan.
//!
//! The optional machine engine prioritizes 32-bit Windows hot paths. It executes
//! guarded integer/memory blocks and MMX calls into shared semantic helpers;
//! other instructions use shared interpretation.
//! Boot success alone does not establish translation correctness, native support
//! for the full ISA, or performance of sustained Windows XP applications.
//!
//! # Target and ownership
//!
//! The primary native target is AArch64. Instruction selection, flags, register
//! allocation and calling conventions can use its properties directly. The
//! emitter and architectural exit records use core/alloc; Linux executable
//! mappings belong to the host adapter. [`arm32`] is a separate, smaller ARMv6K
//! backend for the 3DS that shares decoding, flag liveness and code validity.
//!
//! Cpu remains the persistent state owner. An invocation exclusively borrows it
//! and may keep live guest registers in ARM64 registers. Semantic helpers see
//! committed state; a translation-only helper reads unchanged control/segment
//! state while updating derived caches. No helper observes stale GPRs. Generated
//! code accesses only the explicit frame/memory ABI, not guessed Rust offsets.
//! A data-segment helper validates cached descriptor RAM, commits only that
//! segment and refreshes the native view. Changed compiled qualifications force
//! exit before any subsequent block; unchanged qualifications permit linking.
//!
//! # Memory and exceptions
//!
//! Keep an x86 memory operation's segment, access width, permission and commit
//! semantics separate from its ARM64 access sequence. SoftMMU first emits a
//! guarded TLB hit, with cold revalidation/refill before a semantic miss.
//! See [`memory`] and [`frame`].
//!
//! Preserve segmentation, paging, fault priority, CR2/error code, A/D effects and
//! restartable writes. Never replay an instruction after a side-effecting read
//! or committed write. Lookahead must not raise a future fetch fault, mutate CPU
//! time/EIP, read MMIO, or set a future page's A/D bits. Unsupported lowering is
//! an explicit boundary before executing the instruction, not recovery from a
//! compiler failure. Guest #UD and emulator bugs stay distinct.
//!
//! Native blocks obey Machine's instruction budget and device polling boundary,
//! including backedges and STI/SS interrupt shadows. Deterministic clocks use
//! attempted instructions plus halted-time advances. Realtime clocks poll host
//! time at these boundaries and yield on HLT; instruction accounting never sets
//! device time or caps CPU throughput. RDTSC/RDMSR/WRMSR use the shared semantic
//! helper and machine clock; the CPU's accounting counter remains separate.
//!
//! # Code validity
//!
//! Compiled content depends on physical code pages as well as PC, CS/mode and
//! executable mappings. CR3 alone is not a unique context. Track scalar, bulk,
//! interpreter, device and page-walk writes, including aliases. A store changing
//! the current block must exit before its next stale instruction. Code-page
//! promotion must revoke old writable translations. SoftMMU write barriers
//! feed this validity contract.
//!
//! Lookup invalidation, content invalidation and native-page reclamation have
//! different lifetimes. Reclaim only with no active invocation. Reset/restore
//! discard derived JIT state. Linked blocks must obey the same guards and timing
//! boundaries as dispatch. The interpreter's live-byte fetch cache is not a JIT
//! content cache.
//!
//! # Validation and measurements
//!
//! The standardized cold-boot protocols, drivers and measurements live in
//! `benchmarks/tinycore_boot/` and `benchmarks/reactos_livecd_boot_to_langsel/`.
//! JIT.md links those directories; measurements have one home.
//!
//! Native differential tests cover CPU/RAM/flags/budget behavior. Firmware shell
//! input/reboot, independent oracle ROMs and sustained process/memory stress
//! complement the two timed endpoints. Boot success does not establish XP
//! application readiness. Remaining x87 interpretation, MMX helper cost, whole-cache
//! reclamation and compiler cost must be measured on the relevant workload.
//! Linux performance is recorded per workload; Apple runtime and performance
//! remain unverified.
//! Historical boot targets are not a new task or a sustained-workload guarantee.
//!
//! Sources consulted: FEX at 37d83d937586f342ca3e9131270f8ef23b61adc4,
//! RegisterAllocationPass.cpp, IR.h, Arm64Emitter.h and LookupCache.{h,cpp}; FEX
//! blog entries 2312, 2406, 2408, 2506, 2510, 2609 and Scourge-of-emulation.
//! They inform register/flag residency, compiler cost and cache ownership; FEX
//! usermode behavior is not the full-system semantic contract. tiny386 cpu_exec1
//! and both linked interpreter objdumps were inspected before this batch.
//! Follow AGENTS.md for new reference work: tiny386 first, then additional
//! compatibility/oracle evidence. Do not copy reference implementation logic.
//!

pub mod a64;
#[cfg(target_arch = "arm")]
pub mod arm32;
mod cold;
pub mod compile;
pub mod decode;
pub mod frame;
pub mod links;
pub mod memory;
pub mod soft;
pub mod validity;
#[cfg(feature = "wasm-jit")]
pub mod wasm;

#[cfg(a64_jit)]
pub mod code;

#[cfg(a64_jit)]
pub mod runtime;

#[cfg(all(feature = "machine", target_arch = "arm"))]
pub use arm32::runtime;
