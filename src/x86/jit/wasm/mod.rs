//! Bounded i386 -> WebAssembly regions, hosted by web/wasm.mjs.
//!
//! The interpreter and devices remain the semantic owner. A region contains
//! connected 32-bit blocks from an already executable backing page; lookahead
//! does not walk another page, set A/D bits, or execute a device read. Shared
//! decoding, RAM translation and code watches are the same as the ARM64 JIT.
//!
//! Generated modules import the core's memory and function table. They do
//! not own another guest memory, JS callbacks on their ordinary path, or
//! references to other generated instances. Publication and eviction happen
//! with no generated stack active. Slots never identify guest code without its
//! backing/content/mode qualification. Pending compilation is also budgeted.
//! A module binds its own page-entry array, slot and recency word at admission.
//! Those allocations outlive its functions; retirement clears table entries
//! before slot reuse or page release. Cross-module edges still validate links,
//! rather than embed another module's addresses. Frame carries changing CPU
//! state, not this immutable ownership metadata.
//! Execute links keep separate supervisor/user proofs. INVLPG invalidates the
//! affected range in both planes; full mapping/CS changes discard all links.
//! A code-page version mismatch invalidates that page's dependent modules,
//! while unrelated execute links remain usable. Reclamation removes aliases
//! before freeing their entry arrays. A valid execute link also qualifies
//! untranslated entries on that page. A separate bounded ownership cache
//! avoids tree searches for cold pages, including absent pages; it supplies
//! no execute or source proof and is updated on page creation/reclamation.
//! Qualification of a cold entry can renew the whole-page execute link when
//! the page has a live region. An already watched page uses its stable version
//! cell; a retired watch must be promoted again and revoke writable aliases
//! before dependent native code can execute.
//!
//! Cold code is interpreted through control-flow boundaries. Repeated entry
//! observations use bounded cold hints: seven revisits can remain inside the
//! interpreter before returning to the full scheduler. Their visits then add
//! to heat at the next qualified lookup. Hints never approve generated code;
//! collisions/stale hints only affect admission speed, and publication removes
//! matching hints. The interpreter's event budget and faults still bound work.
//! Taken conditional branches and control-flow members of 0F/FF end a cold
//! trace; untaken branches and ordinary MOVZX/INC/PUSH continue interpreting.
//! Private cold handoffs retain Cpu's lazy flags and
//! need no native event plan until a generated entry is actually found.
//! Repeated entry first becomes an admission candidate; another short window
//! of guest work and continued execution admit it. Very hot bursts can bypass the window;
//! idle heat expires lazily. A remembered capacity victim can skip the extra
//! delay after reaching the normal threshold if it ran and its source version
//! still matches. None of these heat hints replace execute/source validation.
//! Other recently observed warm entries on that same qualified page share the
//! admitted module without requiring a separate submission. All candidate
//! metadata is fixed-size and included in the scratch reserve. Growth follows
//! observed branch/call targets, conditional fallthroughs and straight-line
//! continuations, leaving unobserved taken paths at a precise core
//! boundary. New paths earn their own immutable module; existing modules are
//! not enlarged or rebuilt merely because a neighbour gets hot. A bounded
//! metadata-only retirement history counts recent capacity recompilation,
//! separately from source invalidation and host-engine tier promotion. History
//! collisions can lose observations; counters are lower bounds. Replacement
//! remains whole-module CLOCK.
//! The core yields for deferred publication. Completion only publishes after
//! rechecking the source dependencies; a write/reset during
//! compilation cannot revive old code. There is no persistent translation cache.
//!
//! Frame is an explicit Wasm32 ABI, not Cpu's Rust layout. Faultable operations
//! require the core's flat DS/ES/SS entry proof; segment loads leave generated
//! execution. A qualified data-segment selector uses the shared A64/interpreter
//! cached-descriptor helper after the return. It reads the current descriptor
//! with existing supervisor RAM proofs and never walks, sets A/D or descriptor
//! accessed bits, or touches devices. Success charges one instruction and
//! renews segment eligibility before any next invocation. A miss keeps the
//! original PC and budget for interpretation; SS retains its ordinary shadow.
//! Non-flat data/stack modes remain in shared interpretation. FS/GS
//! keep their dynamic base/limit guards. Ordinary memory operations
//! first qualify all RAM operands, then commit. Failed qualification exits at
//! the current instruction; the interpreter performs fault/device semantics.
//! Data-TLB misses leave address/intent in Frame. The core first probes an
//! already resident RAM translation, without A/D, fault or guest-visible I/O effects,
//! before interpreting the unfinished instruction. Keeping that slow path out
//! of generated functions reduces host register spills and compiler graph size.
//! Linear framebuffer pages borrow one TLB slot only during a generated call,
//! after requalification in the post-interpreter state. Rust always sees the
//! restored RAM row. Writes mark the whole page dirty; read proofs never allow
//! writes. Every interpretation or Machine return ends this transient proof.
//! PCI/decode changes and rendering cannot leave a stale device pointer behind.
//! No generated instruction performs external I/O or suspends through Asyncify.
//! Machine's event budget bounds loops and preserves guest clock/IRQ ordering.
//! Arithmetic flags remain deferred across native and core returns. Frame and
//! Cpu share the compact lhs/result/kind representation: handoffs copy it without
//! an operation/width switch. This includes shifts: zero counts retain the
//! incoming record and nonzero counts replace it. POPF discards overwritten
//! status; instructions preserving arithmetic flags leave the record intact.
//! Non-status bits, including IF, commit immediately.
//! Shared straight-line access analysis lets both JITs reuse a proven
//! page without combining the guest instructions' architectural commits.
//!
//! References consulted: tiny386 i386.c cpu_exec1 and tlb_refill, both linked
//! x86_64 interpreter objdumps (tmp/wasm-jit); v86 rust/jit.rs and cpu.js for
//! region ownership, asynchronous publication and table retirement, not copied
//! instruction implementations; the WebAssembly core/JS specifications and
//! V8's compilation-pipeline, speculative-inlining and tail-call documentation.
//! Later flag/control work consulted tiny386's lazy flags, v86's codegen and
//! control-flow/admission analysis, Node 24.19's V8 Wasm graph lowering and ARM64
//! instruction selection, plus actual generated ARM64/x86_64 disassemblies.
//! Cold scheduling also consulted v86 rust/cpu/cpu.rs's interpreted boundaries.
//! Linear framebuffer mapping consulted tiny386 pc.c set_pci_vga_bar and
//! iomem_write_string, and the existing shared MMU/A64 framebuffer contract.

pub mod compile;
mod emit;
pub mod frame;
#[cfg(target_arch = "wasm32")]
pub mod runtime;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
#[cfg(all(target_arch = "wasm32", feature = "jit-profile"))]
mod trace;
