# Cold execution, hot translation and precise guest boundaries

Proposed runtime contract for the banked RV32/Xr86 core. It combines
selective translation with the page-watch protocol in [CODE_CACHE.md](CODE_CACHE.md).
These guest-integration mechanisms are proposals. Instruction semantics must
first be implemented and tested in the [Rust ISS](../sim/rv32.rs), the ISA gold
standard, before Chisel changes. See the [ISA source index](ISA.md); this document
records runtime policy and rationale, not a competing instruction specification.

## Two execution tiers on one core

Start with a shared x86 interpreter and a simple hot-block translator. Bank 0
runs both as ordinary RV32IM software; bank 1 runs emitted RV32/Xr86 words. Cold
x86 is interpreted in software, not by a second hardware x86 decoder. No native
opcode changes meaning between banks. A later optimizing compiler is optional.

Efficeon's four execution gears explicitly include interpretation followed by
increasing translation effort. Our first two tiers adopt selective compilation;
they do not copy a universal CMS threshold.
[Transmeta, October 2003, slides 29–32](https://datasheets.chipdb.org/Transmeta/pdfs/Transmeta_Efficeon_Launch.pdf).

The first dispatcher behaves as follows:

| Entry state | Action |
|---|---|
| Valid published translation for this guest entry/context/source | Execute it after event/budget and validity checks |
| No translation, below heat threshold | Interpret and update the bounded software heat table |
| Hot, supported, outside recompilation cooldown | Compile a bounded basic block, publish it safely, then dispatch |
| Unsupported, frequently invalidated, or compilation budget exhausted | Continue interpreting; reconsider later |

Count basic-block entries and loop backedges, not just function calls. An x86
function need not have an identifiable call boundary. Interpret until a control
transfer, event or bounded dispatch point; do not copy guest state between banks
on every interpreted instruction. Blocks reaching the translation size limit
also end at an instruction boundary. Tight loops therefore become visible to the
heat policy even when no CALL occurs.

Keep a bounded RAM table with tagged entry/context identities, saturating counts,
and a cooldown field. Collisions, replacement and stale heat may change *when*
we compile; they must never authorize use of stale native code. Translation
validity is a separate exact check. No per-instruction hardware profiler, large
CAM or precise hardware hotness counter is needed initially.

Thresholds are experiment inputs, not measured constants. Compilation cost must
be amortized by expected future savings, including dispatch and state-transfer
cost. Compare cold start and warmed runs; include interpreter/compiler cycles,
code size, cache misses and invalidation cost. A frequent block that cannot be
translated profitably may stay interpreted. Repeated SMC increases cooldown so
we do not translate, invalidate and immediately retranslate the same loop.

Use deterministic guest execution counters for policy/event scheduling. Compiler
RV instructions are implementation work, not extra guest instructions. Compilation
must not invent guest faults by reading beyond required executable source; stop
or defer a candidate when extending it would need an unresolved fetch.

### Adapt persistent slow sites

Add bounded software records of exit reason and guest EIP to candidate metadata.
Do not treat every exit as evidence against translation: a scheduled interrupt
or transient TLB fill differs from repeated MMIO, an unsupported instruction or
unstable source. Keep conservative policy choices across recompilation for the
same source/context identity; changing source starts a new identity. This is
optimization history, separate from mandatory code-validity metadata.

Initially split around repeatedly unsupported instructions and interpret those
instructions while retaining useful neighboring blocks. Later evaluate an ordered
helper for a hot site against full state materialization. Such a helper still
must guard changing addresses/memory types and perform a side effect exactly
once. It is not permission to retry an already performed MMIO access. Retaining
and revalidating suspended translations after page writes is a separate proposed
experiment described in [CODE_CACHE.md](CODE_CACHE.md).

## One authoritative guest state

While interpreting, the shared interpreter's `Cpu`/machine state is authoritative.
On entry to generated code, load its required guest GPRs, EFLAGS, EIP and context
state into the execution representation. While translated execution is active,
that representation owns the current state; the memory copy is not independently
mutable. On an exit requiring interpretation, abort uncommitted effects and
materialize the committed state once, then remain in the interpreter until a
useful translated entry is available. Segments/MMU state must follow the same
ownership rules, not just the eight GPRs.

Bank 0 preserves runtime registers and stack. It is not a shadow copy of bank 1's
guest registers and cannot roll back an overwritten EAX. Cheap TLB-only services
may eventually avoid full materialization, but such retry paths need a separate
proof. Initial guest integration uses boundary restart/interpretation.

## Precise execution without region rollback

Use one x86 instruction as the initial transaction. Before it starts, committed
GPRs, EFLAGS and EIP describe the next guest instruction. Native arithmetic writes
temporaries; flag-producing extensions maintain pending flags. The emitter must
not overwrite live committed state before all possible guest failures are resolved.
Uniform RV semantics do not magically enforce this allocator invariant: it is a
trusted translator/validation obligation, without hardwired EAX register numbers.

Checked guest memory distinguishes normal eligible RAM from cases needing shared
software semantics. Segment/paging eligibility, A/D state, memory type, bounds
and source write-watch are resolved before an irreversible access. For ordinary
RAM RMW, check write intent before reading. The fast path does not read MMIO and
then request that software replay the instruction.

First commit support retains at most one destination GPR, pending flags, and one
bounded pending RAM store (address, data and byte enables). Only a commit may
follow a staged store. It waits until the store can complete successfully, then
publishes the GPR/flags/EIP boundary. The memory/cache interface must have a precise
success/failure contract: a reported failed write has not modified memory. No
younger guest instruction or guest event can observe the intermediate commit
sequence. Merely enqueueing a store that can fault later is insufficient.

Successful completion can mean irrevocable acceptance by an on-chip cache or
queue, with no later recoverable guest fault; it need not mean draining to DDR.
The initial native bus still uses the completion handshake in [RTL.md](RTL.md).
Late native memory failures cannot retroactively become restartable guest faults.

At a service exit, discard staged data/results/flags and retain the instruction's
guest EIP. Software determines actual x86 fault priority, CR2/error code and
required A/D effects through shared semantics. A/D updates already required by
the architecture are not part of an all-or-nothing data rollback. Charge guest
attempt/time accounting exactly once even if native execution exits for service.
Unexpected native faults are implementation failures, not a generic invitation
to replay an instruction after an unknown side effect.

Multi-GPR instructions such as POP into a non-ESP register and MUL, cross-page
writes, REP, MMIO and unsupported protection cases initially stay in software.
They still need correct interpreter behavior. Extending the hardware later
requires bounded pending destinations or a proved fault-free publication tail;
a single RF write port cannot update several GPRs simultaneously. REP and
interrupt shadows require their actual architectural boundaries, not simply
counting every emitted native word as guest progress.

Deliver guest interrupts/debug/time events only at eligible x86 boundaries,
including inside long translated runs. The first native environment has no
asynchronous runtime IRQ handler that can interrupt an uncommitted guest sequence.
DMA and other physical writers are serialized through runtime service boundaries.
Region speculation, a full register checkpoint and a gated multi-store buffer
are deliberately later alternatives; adopting selective JIT does not require them.
Per-instruction commit and one pending destination are prototype choices, not
permanent ISA restrictions. Measure multi-destination exits and commit work
before comparing an extra pending destination or bounded recoverable regions.

## How the page flag fits

`CODE_WATCH` is a private **guest physical-page** property. Cold instruction
fetches do not set it merely because those bytes were interpreted. Set it when a
translation depending on that page is published; first invalidate stale cached
TLB watch bits. It is independent of guest page permissions and of block hotness.
With sticky watch bits, a formerly translated page may remain watched after its
last block is invalidated, until safe cache-wide cleanup.

Translated XMEM writes check the bit before data changes. Runtime interpreter,
page-walker, DMA and debugger writers use the physical-write hook even though
they run outside bank 1. Thus a single "x86 mode" check is not sufficient to
cover all writers. Marking pages also does not identify individual translated
entries; the software translation table still performs that lookup.

For a watched write: exit before it, invalidate dependent blocks, execute the
instruction through shared software, then redispatch from guest EIP. Never return
to a stale native continuation, even when the modified instruction is later in
the currently executing block. Aliases, cross-page dependencies, publication and
code-cache address reuse follow [CODE_CACHE.md](CODE_CACHE.md).

## What exists today

| Layer | Existing evidence | Still missing |
|---|---|---|
| B1 RV firmware | Full interpreter/PC model; hot register-only blocks on the same core, physical source versions and software writer hooks; cold/SMC/alias/remap and bounded fault/timer ISS–RTL comparisons | Completed RTL OS/Lua gate, broader fault/interrupt stress, measured JIT benefit |
| Current arithmetic extension | ISS arithmetic/status, pending flags and direct condition tests; compiler uses fault-free commit form | General guest commit; validation in [EFLAGS.md](EFLAGS.md) |
| Guest memory and translation | ISS/Chisel TLB/window and checked memory; [shared interpreter](../experiments/INTERPRETER.md) consumes qualified translation, XLD reads/fetch and XEA leaves | XSW integration in the interpreter, hardware page-watch and full accelerated RTL boot |
| B1 Chisel RTL | RV32IM/Zifencei/context, arithmetic/status and guest-memory validation, GW5A structural mapping | General guest commit, full RTL OS/Lua boot, caches and routed timing |

The [B1 register-only baseline](../experiments/B1_JIT.md) needs no speculative
guest effects: it qualifies fetch/event boundaries, runs fault-free register
operations, and returns to software for guest memory and other operations.
It therefore does not establish the proposed pending-store/commit hardware.

Required integration tests include a cold-only program generating no
translations; hot backedge promotion; identical interpreter/translated guest state
at every exit; memory/flags unchanged on a fault; one side effect for MMIO; no
recompile storm under SMC; and the invalidation cases listed in CODE_CACHE.md.
