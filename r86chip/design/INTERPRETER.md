# Raw-byte interpretation: instruction budget and next accelerators

User constraint: **read and decode the original x86 bytes on every
execution; no decoded-instruction cache**. The current objective is an average
of **at most 20 retired native instructions per completed interpreted x86
instruction**, for representative 32-bit protected-mode OS work. Follow
[different instructions, different views](ADDRESS_SPACE.md#different-instructions-different-views).
The existing JIT remains a separate execution
tier. Retain the approximately 10K-LUT4 budget including core, small guest TLB
and cache controllers; prioritize useful acceleration over expensive throughput
features. This document is an assessment; implemented slices are identified
explicitly. The [TLB/window probe](TLB.md) and dedicated
[guest memory/context/service operations](GUEST_MEMORY.md) exist in ISS and
Chisel. The complete accelerated interpreter is still unfinished. The
[ISS](../sim/rv32.rs) defines current instructions.

## Performance objective and acceptance

Build and validate the accelerated interpreter using one RV32 instruction
stream: native operations for firmware/r86 state, explicit guest operations
qualified for current x86 state, software-filled guest translation, and a
service ABI that does not require RF-bank swaps. Change the ISS and its tests
first, validate software consumers, then implement Chisel and compare the same
native binaries and architectural observations.

The measured target is:

```text
total retired native instructions / total completed interpreted x86 instructions <= 20
```

- Fix representative 32-bit protected-mode workloads, guest inputs, measurement
  endpoints and counting rules before optimizing. Cover kernel and user work,
  paging, integer/memory/control-flow mixes and real software, consistent with
  the Windows 98/2000/XP focus. The input/endpoint set is now pinned in
  [interpreter-corpus.json](../experiments/workloads/interpreter-corpus.json):
  TinyCore boot, the subsequent real i386 Lua/shell/I/O workload, and ReactOS
  firmware boot through the complete language-selection dialog. ReactOS is an
  NT-style workload, not a Windows 98/2000/XP compatibility result. Report mode
  and CPL splits, individual workloads and aggregate counts. Do not drop slow
  intervals or change inputs/endpoints after seeing accelerated results;
  isolated NOP/ALU loops are diagnostics, not acceptance workloads.
- Count fresh-byte fetch/decode, dispatch, operand access, execution, guest
  EIP/time/event accounting, and all native work in TLB refills and software
  fallbacks within the measured interval. Ordinary byte caches are allowed;
  cached decoded instructions and JIT execution cannot satisfy this goal.
- Report each workload and the ratio of aggregate counts, not an unweighted
  average of per-opcode ratios. Report REP iterations, faults and retries
  separately from completed guest instructions; do not inflate the denominator
  with service retries. Separate startup measurements from steady execution
  and also report whole-machine costs with their exact scope.
- Compare guest registers, flags, memory and observable behavior with the shared
  interpreter, including precise faults, mode/context changes and code writes.
  Hardware coverage is optional for rare modes; their software cost still counts
  when they occur in a measured workload.
- Report RTL clocks, service frequency, coverage and synthesis resources as
  well as native word counts. Keep the approximately 10K-LUT scope for core,
  small TLB and cache controllers. Instruction-count success alone proves neither
  timing closure nor 100 x86 MIPS; do not conceal unbounded work inside an opcode.
- Use **256 KiB** as an initial linked native interpreter size target, including
  cold paths. The subsequent user direction is to select code
  growth by demonstrated benefit, rather than enforce this cap through slower
  execution or undo every useful expansion. Retain larger code only for a clear
  matched full-workload gain that justifies its size; include native counts and
  available same-binary RTL clocks. This size target excludes separately linked
  MMU and device functions, but includes helper code inlined into the interpreter.
  Include reachable compiler-generated `OUTLINED_FUNCTION_*` bodies: counting
  only named Rust functions undercounts size-optimized builds. The
  [linked-size tool](../experiments/native_size.py) follows those references
  transitively and includes separately linked CPU-state/native-leaf helpers
  in its conservative budget check, including Rust trait methods such as cold
  exception formatting. Report whole-firmware `.rodata` separately;
  this text budget is not a total firmware memory budget.
  Report the complete interpreter text, main loop, separate CPU/MMU text and
  whole firmware `.text` distinctly; the main loop alone is not the budget.
  Deduplicate symbol aliases by linked address/size and record the exact ELF
  and build options. Do not claim compliance by renaming/moving interpreter
  helpers, omitting guest semantics or counting only the accelerated handlers.
  Prefer removing duplicated work and cold-code expansion; specialization and
  inlining must earn both their execution cost and their code-size cost.

This objective has not been achieved. The current probe and measurements below
establish building blocks and a baseline, not a complete accelerated interpreter.

## One interpreter, build-time leaf primitives

There is one x86 fetch/decode/dispatch and one semantic implementation in
`src/x86/interp.rs`, for all guest modes. `r86chip-native` statically selects
hardware leaf operations inside that interpreter. It introduces neither a
subset loop nor a second live guest state. Uncommon instructions use the same
ordinary handlers. A TLB miss continues the current memory access through the
existing software cache/walker/MMIO path; it does not interpret the x86
instruction a second time.

Bus instruction-origin attribution is a build-time diagnostic, selected by
`bus-instruction-context`. The ordinary `host` build enables it; chip firmware
can opt in. Firmware without it still preserves unclaimed-access behavior,
target-page counters and bounded warning records, but need not calculate/store
an otherwise unused diagnostic CS.base+EIP on every interpreted instruction.
This is the same interpreter and bus implementation, with no runtime mode test.

`Cpu` remains the EFLAGS owner. Supported arithmetic/logic/shift status setters
calculate bits with a bounded XALU.C/XFGET sequence, loading CF for ADC/SBB
and CF-preserving INC/DEC. XFGET merges status into the caller's control flags.
The hardware flag registers are temporary calculation state, not an ABI across
Rust calls. Setters still execute at the existing semantic commit point, after
faultable writes where required. Ordinary targets retain lazy software flags.

The hardware data TLB mirrors qualified software-cache proofs. It is leased to
one Cpu for a batch, with one entry flush and no GPR/flag transfer. Within a
batch, privilege changes, architectural translation invalidation, software row
replacement and writable-alias revocation expire the relevant hardware proofs.
Implicit supervisor accesses during user execution do not consume user proofs.
Selected segment-relative reads also use XLD with a register segment operand.
Batch entry publishes the existing flat-data segment proofs and current access
control. Segment/mode/privilege changes disable this read lease until the next
batch. A three-word native miss adapter rejoins the calling inline-asm block;
the original Rust operation then checks segmentation, walks or accesses MMIO.
There is no second x86 decode or live guest frame. Other accesses retain XTLOOK
and ordinary native loads/stores. A/D updates, split accesses and all bus write
hooks remain in their existing Rust helpers; XSW is not used by this consumer.

Original instruction-byte fetches also use the existing XLD CS/execute form.
Batch entry publishes CS's actual base and byte limit, independently of data
read rights. A successful software fetch can publish an X-only page proof;
the software fetch cache retires it on replacement, mapping or physical-range
invalidation. Data and fetch proofs now have separate small tables, with explicit
view-selected publication; neither fill can silently grant the other kind of
access. Shared invalidation expires both views. The byte fetch helper executes only within the active
Cpu's batch lease, so it needs no second per-byte ownership check. EIP advances
only after success. A miss continues that fetch in a target-only cold Rust
helper; it neither repeats the x86 instruction nor changes exception ordering.
Word/dword immediate and displacement fetches now use the ISS-defined same-page
unaligned fetch form. The existing ModR/M decoder consumes the same leaf, so a
hardware byte hit need not refresh a redundant software cursor. Cross-page or
otherwise rejected spans continue in the original software fetch helpers.
XLD reads current memory
bytes, so code writes and aliases are observed without a decoded cache or native
FENCE.I. The initial XLD consumer required no ISA change; subsequent table
separation changes fill encoding, and wide fetch extends the load contract in
the ISS and Chisel.

The existing 32-bit ModR/M address helper can use the ISS-defined XEA32 leaf.
It reads fresh SIB/displacement bytes through the same CS/X proofs, then reads
the needed slots of `Cpu.regs` through a native pointer published at batch entry.
There is no copied GPR frame or decoded cache. The helper consumes offset and
length/default-segment information within one inline-asm boundary, advances
EIP on success and applies the existing segment override. It still performs
the operand access and architectural commit in Rust. A rejection continues the
same address helper with its original EIP. Address-size 16 and POP-memory's
explicit updated-ESP calculation retain their software implementation. The
[ISS contract and tests](../sim/ea_tests.rs) define the supported scope; a retired custom word is not a single clock.

[Current software evidence](../experiments/INTERPRETER.md) records paired builds,
ISS/one-bank RTL checks and costs. The retired two-loop experiment
measured 473.22 versus 168.07 native words/completed x86 on the same early
TinyCore prefix. The replacement removes that handoff. Subsequent measurements
track loop restructuring and XLD integration against separately preserved,
matched controls. They have not delivered the 20-word target. The early TinyCore
prefix is a startup diagnostic, not the fixed acceptance corpus. Optimize the one
interpreter using workload and linked-code evidence before extending the ISA.

The new chip firmware has a completion count distinct from `Machine::step_count()`
or `BatchResult::ran`. The existing shared loop charges attempts, including
faults and resumable REP chunks; those diagnostics are not automatically the
denominator above. Count a fully completed REP once, report its element/chunk
counts separately, and charge all native work until completion. An unchanged
EIP cannot by itself identify a retry: a self-branch may be a valid completed
instruction. Use explicit completion observations and keep guest deterministic
time/event accounting intact. The optional `chipinterp::Counts` now explicitly
excludes fault attempts, halted polling and partial REP chunks. It accounts for
completed instructions at batch return, subtracting those incomplete events
from the batch's attempts; this does not change guest time. REP element
counts, full mode/CPL splits and precise corpus endpoints remain unfinished.

The opcode handlers now live directly in the batch loop. A helper error leaves
at the current instruction; successful execution does not construct a separate
per-instruction result. Prefixes use an arm of that same dispatch table, then
rejoin it after scanning the original bytes. The backedge counts a local u32
budget; absolute u64 steps are formed where observed, with larger budgets
continuing internally without an extra guest boundary. TSC still advances at
each attempted instruction, and fault/event ordering is unchanged.

On RV32, `Cpu` represents that deterministic TSC as a 64-bit base minus the
batch's remaining u32 attempts. The loop publishes its existing countdown
before each opcode fetch; it no longer loads/adds/stores a u64 on every attempt.
`Cpu::tsc()` returns the exact current value, including at a fault or partial
REP exit. `set_tsc()` reanchors a WRMSR/reset/import immediately; an early exit
does not charge the unused budget. Reanchoring at the next batch or u32 chunk
boundary preserves wraparound. Ordinary host builds retain direct u64 counting.
See [the implementation and its tests](../../src/x86/cpu.rs) and the shared
loop's observation-order test in [interp.rs](../../src/x86/interp.rs).
This is a software representation change, with no new instruction or live
guest frame; realtime device/TSC clock selection is unchanged.

On RV32, `Cpu` also keeps the GPRs and frequently accessed scalar state before
its large inline caches, using a target-only declared layout. The active fetch
cursor belongs with those scalars. This lets ordinary RV loads/stores reach
them with one base and short displacements. It changes neither guest-state
ownership nor the explicit firmware observation ABI. The ordinary host layout
is unchanged. The common dword immediate-ALU helper is inlined in the native
RV32 build to avoid
its measured call spills; its existing operation and fault/flag commit order
remain shared.

## What the current interpreter costs

[interpreter_cost.py](../experiments/interpreter_cost.py) runs actual RV firmware
with JIT disabled. Each flat protected-mode guest enables paging, then loops over
32 selected instructions and one jump. Subtracting 10,000-step from 43,000-step
native counts cancels common setup and covers exactly 1,000 loop iterations.
Software fetching/decoding, dispatch, semantics and runtime work remain counted.

Current headline numbers are in the [r86chip README](../README.md); this
section does not duplicate them. These are synthetic diagnoses, not an OS-weighted
average: paging is on, code/data fit the caches, IF is clear, and no translated
block runs. Both builds include completion counters. Measure a supplied binary
without a runtime engine switch:

```sh
python3 r86chip/experiments/interpreter_cost.py --firmware r86chip/experiments/out/interpreter/native/firmware.elf
```

Even NOP is expensive, so flag arithmetic or data TLB acceleration alone cannot
deliver the target. Keep one owner for guest state and explicitly budget
fetching, decoding, operand access, dispatch and boundary
accounting. Improve that one loop; do not add a second interpreter to bypass its costs.

## Software-filled guest TLB — required in the intended accelerated path

Start with a small hardware lookup and software refill, not a general hardware
x86 page-table walker. It caches guest linear-page to eligible native-RAM-page
access, qualified for the current guest context. This is x86 translation metadata,
not a RISC-V Sv32 MMU and not cached decoded code.

- Software uses the shared x86 paging/segment semantics to qualify fills,
  including privilege/WP permissions, required A/D effects and RAM classification.
  A hit checks its tag/context, access rights, range and code-watch state before
  performing the data access. Segment checks cannot disappear merely because
  the accelerated workload uses 32-bit code segments.
- Misses, unresolved permissions/A-D work, MMIO, unsupported splits and watched
  writes return to software before irreversible effects. A miss is not itself
  an x86 page fault. Software produces the correct fault/CR2/error code or fills
  an eligible mapping. MMIO must execute exactly once.
- CR3/control/privilege changes, INVLPG and mapping reuse need explicit invalidation
  or context qualification. Code watches use guest physical identity so aliases,
  interpreter writes and other physical writers cannot bypass SMC invalidation.
- For RMW, qualify write intent before reading eligible RAM. Preserve old GPRs
  and flags until the final possible guest fault has passed. The current pending
  status mechanism is useful but does not implement a checked store or general
  guest commit. Follow [RUNTIME.md](RUNTIME.md) and [CODE_CACHE.md](CODE_CACHE.md).

The initial probe uses
16 direct-mapped entries and an explicit service result, with a native backing
window checked at fill time. The subsequent
[guest-memory extension](GUEST_MEMORY.md) adds access-context and segment proofs,
checked memory operations and same-bank service. The current implementation
retains the data capacity and adds a small separate fetch table; the
[ISS](../sim/rv32.rs) defines its geometry and all-view invalidation. See
[ADDRESS_SPACE.md](ADDRESS_SPACE.md) for native versus guest addresses, the
state-qualified XLD/XSW path and the single-bank interpreter direction.
Compare working-set conflicts before selecting the final table geometry;
a large fully associative table is not assumed.
Cache arrays remain BSRAM, and DDR/peripherals remain outside the budget.

## Budget fresh-byte decoding before adding hardware

At the 20-word target, first measure a compact software interpreter
using the guest-memory and arithmetic/status primitives. Do not assume a
hardware x86 decoder is required. If decoding still dominates, evaluate a
limited assist for common 32-bit integer instructions: identify operation,
length, widths, ModR/M fields and eligible SIB/displacement/immediate fields
from **fresh instruction bytes**.
Software selects fixed handlers and handles unsupported forms. This is a partial
x86 frontend alongside RV execution; it is more than adding an arithmetic opcode.
It need not generate an internal uop stream or store decoded instructions.

The frontend assessment must answer three concrete questions before choosing
additional encodings:

1. Can qualified fetching and field extraction avoid repeated byte loads/shifts
   within the area budget? Only bytes actually required by an instruction may
   cause guest fetch faults; speculative lookahead across a page cannot invent
   a fault. Prefix limits, operand-size overrides and instruction length remain
   architectural obligations, including when the probe requests software service.
2. How are dynamically selected guest GPRs read/written cheaply with two read
   ports and one write port? Ordinary RV operands encode fixed register indices;
   a software ModR/M index cannot name a register directly. An indexed guest-RF
   assist or carefully sized handler table needs measured word/cycle/code costs.
3. How do handlers advance EIP, charge guest time and expose eligible event
   boundaries without a per-instruction full frame transfer? A failed access
   must retain the correct committed boundary. Native RF banking is no checkpoint.

Complex forms and uncommon modes use the same interpreter. Their helper costs
belong in the average: 90% at 10 words plus 10% at 100 averages
19; 85% at 10 plus 15% at 100 averages 23.5. Measure workload frequencies and
complete service costs, rather than selecting only cheap instructions.

At an assumed 300 MHz, 100 million x86 instructions/s allows only 3 clocks per
x86 instruction for all work. Twenty native words at the measured 1.34
clocks/word (zero-wait RAM) already take about 27 clocks. The instruction
budget and throughput target therefore cannot be treated as interchangeable.
No routed 300 MHz result exists. Keep both metrics and resource measurements
visible; do not inflate an opcode's hidden work merely to reduce the word count.
