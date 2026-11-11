# First banked RTL profile (B1)

Implementation direction: one directly decoded, in-order
**RV32IM + Zifencei pipeline**, with two 32-register contexts and experimental
Xr86 instructions. Target: Tang Retro Console 138K, approximately **10K LUT4
including execution, small guest TLB and cache controllers**. RAM arrays use
BSRAM; DDR and peripherals are separate. This is a budget, not a fit result.

This document describes hardware implementation choices and interfaces. The
[Rust ISS](../sim/rv32.rs), selected with `Core::new`, defines the ISA;
see its [source index](ISA.md). Change and test that model before changing
Chisel. Private encodings and firmware ABIs need not remain backward compatible.
Full-machine validation is recorded in [TINYCORE.md](../experiments/TINYCORE.md).
The arithmetic/status extension is implemented by `XFlags.scala` and retirement
gating in `Core.scala`.
Its two status values are shared by native banks and follow the ISS; they are
separate from software-owned EFLAGS control bits.

## Language and organization

The user selected **Chisel**. The native B1 implementation now lives in
[`rtl/src/main/scala/r86chip`](../rtl/src/main/scala/r86chip), with one small
Verilog RF technology wrapper. The handwritten core/M-unit draft has been
replaced. Chisel 7.12.0, Scala 2.13.18 and sbt 1.12.11 are pinned; generation uses
Chisel's matching firtool 1.146.0, rather than this machine's older PATH binary.
See [build instructions](../rtl/README.md) and the
[measured native-slice results](../experiments/RTL_NATIVE.md).

The resulting circuit uses direct decode.
Multiplication/division take several clocks inside their arithmetic unit; this
is not expansion into internal instructions. An elaboration-time generator is
also not runtime microcode.

The intended pipeline organization is closer to a small VexRiscv: separate
fetch, synchronous register read and execute/retire with explicit stalls and
flushes. Keep dependencies and configuration closer to PicoRV32's simplicity.
These are design references, not imported implementations. VexRiscv is written
in **SpinalHDL**, not Chisel; its configurable synchronous RF is relevant to
BSRAM. PicoRV32 prioritizes size and uses a different, more serialized execution
organization. [VexRiscv](https://github.com/SpinalHDL/VexRiscv),
[PicoRV32](https://github.com/YosysHQ/picorv32).

Chisel can emit SystemVerilog and is not inherently unsuitable for an ASIC.
Handwriting minimizes build dependencies; generation can simplify changes to
pipeline metadata and configurations. Neither predicts area or backend support
without inspecting and testing the emitted RTL.
[Chisel output](https://www.chisel-lang.org/docs/resources/faqs).

The selected lowering options emit a simple subset accepted by Yosys's ordinary
`read_verilog` frontend without `-sv`. Both bank configurations pass Verilator
execution and Icarus four-state execution. This verifies the actual current
output; it is not a promise that every future Chisel construct will work in every
backend. No Rocket Chip or plugin framework is part of this build.

## State and direct decode

- Ordinary RV instructions have identical meanings in both contexts. Bank 0 is
  the software runtime and bank 1 generated code by convention. They are not
  separate harts, privilege levels, or independently scheduled processors.
- Physical RF addresses are `{bank, register[4:0]}`; each bank has an x0. Two
  synchronous read ports, one write port; collisions forward using the **whole
  bank-tagged address**. No reset loop over RAM. Firmware initializes used GPRs.
- One execute slot owns operands until completion. Register reads occur when an
  instruction issues. Completion may issue the next instruction with writeback
  bypass. LSU/M-unit stalls hold that slot and its RF outputs.
- Branches, FENCE.I and bank switches flush younger instructions. An outstanding
  fetch is drained and discarded, never relabelled as the redirect target.
- Context changes serialize older writes and memory operations. A writeback uses
  its saved bank tag. Bank 0 is not a checkpoint of bank 1's x86 state.
- RV retirement and x86 commit are different. Guest flags, EIP, pending memory
  effects and multi-destination instructions still need the guest-boundary
  contract described in [RUNTIME.md](RUNTIME.md#precise-execution-without-region-rollback).

The frontend has one fetch transaction in flight, an instruction buffer with
one skid slot, and one execute slot. Fetch runs ahead: a request starts only
when its response is certain to have a free slot, so a bus completion is never
refused. With zero-wait memory, straight-line code issues one word per clock
and a redirect costs two bubbles; there is no branch prediction. Prefetched
words, including fetch errors, have no effect unless issued. Decode, execute,
guest translation and the bus request still share one stage, so this is not a
claim of final FPGA frequency. Measurements: [RTL_NATIVE.md](../experiments/RTL_NATIVE.md).

## Native memory interface and faults

Separate instruction and data ports use `valid/ready` **completion handshakes**.
An asserted request and its address/data/strobes remain stable until a rising
edge with `ready`. Read data and error are sampled on that edge. There is no
separate acceptance followed by a response: a bridge must retain the request
until it can report completion, and must perform each write only once. Each port
has at most one transaction. `ready` may be combinational or delayed.

Native physical addresses and data are 32-bit little endian. Instruction fetches
are aligned 32-bit words. Data requests carry a word-aligned address and four
byte write strobes. Ordinary RV halfword/word misalignment faults before issuing
a request. The ISS-defined guest fetch .U can span two native words after proving
the requested bytes lie in one guest page and CS range. One register holds the
first response; a phase bit selects the second request. Only the last successful
response enables writeback/retirement. Either bus error traps with the original
effective native byte address. The second word uses a ten-bit within-page
increment; no page walk, second execute slot or internal instruction is involved.
XEA32 uses the same port through the bounded `Ea32.scala` sequencer: SIB,
displacement (up to two beats), base GPR and index GPR. It holds the original RV
operands until the final response. Intermediate data is uncommitted scratch;
native errors publish neither the destination nor EAINFO. Code qualification
uses the current CS/fetch proofs; GPR reads use the configured native array.
See the [ISS](../sim/rv32.rs) for ordering and rejection semantics. The additional
`retire_ea_hit` output is test coverage only, like the other retirement outputs.
A write error must mean **no write took place**. The system resets the core and
its bridges together; reset does not undo a completed external write. Instruction
reads must be side-effect-free because fetch can run ahead and be discarded.

Native memory operations execute in order. FENCE therefore needs no extra queue
drain. FENCE.I flushes prefetched words after preceding stores complete and
restarts at PC+4; future caches must participate in that operation. Reserved
FENCE operand fields are ignored as required of a base implementation. There is
no native MMU, atomics, compressed ISA or privileged CSR implementation yet.

Faults stop the prototype with stable `trap`, `trap_pc`, `trap_cause`, and
`trap_value` until reset. Fault selection and architectural effects follow
[`Cause`, `Trap` and `Core::step`](../sim/rv32.rs); the RTL port uses the
corresponding RISC-V cause numbers. This test interface is not an implementation
of machine-mode trap delivery. The retire outputs are verification observations,
not an x86 commit interface.

## Experimental B1 context operations

The exact encodings, legal bank/operand combinations, state transitions and
trap cases for XENTER/XLEAVE/XREAD/XWRITE/XINFO are defined in
[`Core::step`](../sim/rv32.rs) and exercised by the B1 cases in
[rv32_tests.rs](../sim/rv32_tests.rs). This document deliberately keeps no
duplicate encoding table. Change the ISS and its tests first, then firmware
consumers, then Chisel; rebuild generated code for changed private encodings.

Saved native PC is not x86 EIP and does not authorize resuming after an
invalidation. Context control resets; the hardware register RAM is not reset,
so firmware and differential fixtures initialize registers before using them.
`BANKS=1` is a separately built native comparison: all these operations are
illegal, with no silent aliasing or promise of binary-compatible banking.

The custom slots now include arithmetic/status, guest translation and
[checked guest memory/context/service](GUEST_MEMORY.md). The ISS defines their
encodings; a complete guest-boundary protocol remains software integration work.
EAX is not hardwired to a particular RF address. Both banks retain all five-bit
RV register operands. No A64 decoder or internal uop vocabulary is introduced.

## Translation validity

Native fetch always fetches RV32/custom words. Guest x86 bytes are data for the
translator. A software table selects published translations by guest entry,
execution context, source physical pages and versions. A conservative write-watch
bit prevents stores from silently changing translated source. Publication,
current-block SMC, aliases, DMA and code-cache reclamation are specified in
[CODE_CACHE.md](CODE_CACHE.md). This protocol is part of guest memory integration,
not a hidden ISA selector on the instruction port.

Transmeta's published design motivates further software feedback
and translation-retention experiments. Per-instruction commit remains the first
baseline; region checkpointing and fine-grain watch hardware are not requirements
of this native RTL slice.

Cold interpretation, selective hot-block compilation and precise guest state
ownership are described in [RUNTIME.md](RUNTIME.md). They are software/runtime
policies backed by explicit hardware boundaries, not a requirement to JIT every
guest entry or to set a code-watch bit on every interpreted page.

## ASIC boundary

Keep the RF as a replaceable memory module. Future cache arrays use the same
principle: the FPGA wrapper infers BSRAM; an ASIC wrapper selects real SRAM macros
or a measured flop implementation. No synthesizable `initial`, `$readmemh`, PLL,
pad, DDR primitive, generated clock or vendor RAM primitive belongs in the core.
Use one clock, synchronous control reset and explicit enables initially.

SV source is only an input to a tapeout flow. A selected PDK, legal memory macros,
timing constraints, reset/CDC analysis, DFT strategy, physical design and signoff
are still required. FPGA LUT estimates cannot establish ASIC area or frequency.
OpenLane documents the distinct synthesis, physical implementation and
verification steps. [OpenLane flow](https://openlane2.readthedocs.io/en/latest/getting_started/newcomers/index.html).

## Implementation gates

Every ISA change starts with the Rust model and focused behavior tests. Validate
affected software on the ISS before changing Chisel, then compare identical
native binaries and architectural observations between ISS and RTL. The
independent Python oracle provides another check, not another ISA definition.

1. Native RV32IM, synchronous banked RF, context operations, stalled memory,
   redirect/fault behavior and FENCE.I; run native binaries and arithmetic tests.
2. Model and test checked guest memory, recoverable flags and guest boundaries
   in the ISS, exercise their emitter/runtime there, then implement them in
   Chisel. Prove current-block SMC exits before exposing a write; software must
   cover every physical writer.
3. Run the same-core translator on RTL, then add small TLB/cache controllers and
   matched workloads. Report full Gowin resources and post-route timing before
   deciding whether the 10K budget is realistic.
4. Guest fault/interrupt/device integration and real OS boot are separate gates.

Gate 1 plus arithmetic/status, the [TLB/window probe](TLB.md) and
[guest-memory service](GUEST_MEMORY.md), including independent data/fetch tables,
now has 118 native programs, 944 RTL/oracle runs, a separately checked RF,
a compiled Rust arithmetic program, generic structural checks and GW5A mapping.
The frontend has no branch prediction and the M unit is deliberately iterative.
Staged/committed status, translation, checked guest memory and same-bank service
are implemented. The [single interpreter](../experiments/INTERPRETER.md) integrates
XALU, XTLOOK and selected XLD reads, with matched firmware ISS/RTL checks.
Complete cache/timing and whole-corpus gates remain open. A separate
[B1 register-only translator](../experiments/B1_JIT.md) now runs on the same RTL
core, including bounded cold-code, SMC, alias and remapping checks against the
interpreter and ISS. It leaves guest memory/privileged operations in software;
it does not implement the proposed guest commit/XMEM extensions. The full software
interpreter has a functional TinyCore boot result, plus an incomplete RTL boot
attempt and a bounded ISS/RTL RAM comparison. That baseline does not implement
the guest extension/commit protocol. There is no routed clock or completed
TinyCore boot on RTL.
