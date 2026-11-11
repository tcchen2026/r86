# Firmware, guest addresses and physical RAM

Address ownership for the same-core RV32/Xr86 design. The executable
instruction contracts, including the guest RAM window, live in the
[Rust ISS](../sim/rv32.rs). The core executes RV words at all times. x86 bytes
are data consumed by the interpreter/translator; a guest EIP never becomes the
native fetch PC directly.

## Different instructions, different views

**The instruction selects the view; the relevant
architectural state determines how that operation behaves.** This is the design
principle for the new interpreter and guest-memory interface. The implemented
instructions and their exact contracts remain defined by the [ISS](../sim/rv32.rs).

A view defines how an instruction interprets its operands and which architectural
effects it may produce. It does not require a second CPU, a second copy of RAM,
or a different register bank. Native and guest operations coexist in one RV32
instruction stream, use the same directly decoded pipeline and can share the
register file and physical memory bus.

| Operation family | Selected view | Design responsibility |
|---|---|---|
| RV loads/stores, such as `LW/SW` | Native memory | Native addresses for firmware, the r86 runtime and its data |
| `XLD/XSW` | Guest memory | Guest addressing and access rules, qualified for the current x86 context |
| Ordinary RV arithmetic | Native arithmetic | Ordinary RV results; preserve guest arithmetic status |
| Implemented `XALU` and flag operations | Guest arithmetic/status | Explicit x86-width arithmetic and status effects, as defined by the ISS |

The caller does not select the view. An interpreter handler may use a native
load to read runtime metadata, a guest load to read an x86 operand, and a native
store to update runtime scratch memory. Generated code and a miss handler use
the same rules. Moving between these operations requires no bank swap or
CPU-wide switch that reinterprets ordinary `LW/SW` instructions. Native
instruction fetch continues to fetch RV/custom words throughout.

The opcode selects **native versus guest**, not a fixed guest mode. A guest
operation must match current real/protected/v8086 mode, paging, privilege and
the other access assumptions described below. Changing guest CR3 or CPL changes
guest access qualification; it does not change firmware pointers. Hardware may
implement a common fast case and request software for other cases while keeping
the same guest semantics. A TLB hit is a cached access proof for that context.

This separation lets hardware handle common guest accesses while software owns
page walking and complex cases. A guest miss must leave a precise boundary from
which software can fill and retry, complete the access once, or deliver the
appropriate guest exception. It must never fall through to an unchecked native
access using the same numeric address. Only bytes required by an access may
produce guest effects or faults; a miss alone is not a guest fault.

Memory visibility still belongs to the underlying backing: two views can reach
the same guest RAM bytes. View selection does not make those bytes independent
or remove physical-alias, SMC and device-coherence obligations. Firmware and
generated RV code remain trusted native code; explicit guest opcodes do not
sandbox arbitrary native loads/stores.

The resulting default direction is a **single-bank RV32 core with explicit
guest operations**. A service ABI must preserve resident guest registers and
the committed boundary. Banking can be reconsidered as a measured context-save
optimization; it is not part of view selection, guest mode or precise commit.
The existing implementation and replacement requirements are distinguished below.

## Three address layers

```text
x86 segment:offset
    -> guest linear address         (x86 segmentation)
    -> guest physical address       (x86 paging, or paging disabled)
    -> native physical RAM address  (r86 bus/backing resolution)
```

The native firmware contains r86's interpreter, translator, emulated devices,
guest CPU state and their allocator. They share one trusted native address
space. The PC simulated by r86 has a separate address space. Separating firmware
from an additional copy of the r86 runtime is unnecessary: they are the same
native program.

Guest CR3/PTE values refer to **guest physical memory**. They cannot select
native firmware pages. For eligible RAM in the current contiguous backing,
`native = GUEST_BASE + guest_physical`, after guest physical range and memory
type checks. This formula is not applied blindly to ROM, MMIO, gaps, overlays,
or wrapping/out-of-range guest addresses. A20 and physical aliases belong to
r86's physical-memory resolution, before publishing the native mapping.

The software-filled guest TLB caches the composition **guest linear -> native
RAM page**. Firmware walks guest page tables and resolves backing on a miss,
then installs the final native page. A guest PTE is never copied into that table
as an unchecked native pointer. The native RV fetch/load/store path does not go
through guest paging, even while translated x86 code runs.

## Present simulation layout

The authoritative addresses are in [machine_abi.rs](../firmware/machine_abi.rs),
with firmware placement and stack top in [machine.ld](../firmware/machine.ld).
These are a 512 MiB simulation layout, not a board DDR-capacity or bus-map claim.

| Native range (end exclusive) | Owner |
|---|---|
| `0x00001000 .. 0x00800000` | Firmware text/data/BSS |
| `0x00800000` control area, `0x00900000 .. 0x00a00000` output | Simulation interface |
| `0x02000000 .. 0x06000000` | Runtime heap, including generated native code and metadata |
| Stack grows down from `0x08000000` | RV firmware stack |
| `0x10000000 .. 0x18000000` | Exclusive 128 MiB guest RAM backing |
| `0x18000000 .. 0x1c000000` | Optional guest-boundary trace, owned by firmware |

For example, an x86 PTE mapping linear `0x00403000` to physical `0x00005000`
produces native `0x10005000`. Guest physical zero refers to native `0x10000000`,
not the firmware's address zero. A guest physical address numerically equal to
`0x10000000` does not select native `0x10000000`: it lies outside this guest RAM
allocation and goes to the guest bus's appropriate slow path.

The guest retains a 32-bit address space; allocating only 128 MiB of RAM does
not reduce its address width. Its BIOS, PCI apertures and other devices remain
guest bus mappings. Native SoC device addresses belong to trusted firmware and
are not automatically exposed as guest MMIO.

## A small hardware boundary for the accelerated path

Firmware configures an aligned native guest-RAM window before filling the TLB.
It resets empty. The ISS rejects malformed configuration precisely, flushes the
table on every successful window change, and accepts a usable fill only if its
entire native backing page lies within that window. Out-of-window fills evict
the affected slot and subsequent probes request software service. The check is
at fill time; a hit needs no extra range comparison. Both RF banks share it.

This guards the address returned by the guest lookup path. It also prevents a
stale entry from surviving guest-RAM relocation. It does **not** enforce x86
permissions, memory type or physical page watches by itself: those are part of
the software fill proof, with access rights checked again by the lookup.

This is not a sandbox for arbitrary RV code. Ordinary RV loads/stores and
configuration instructions remain trusted and can access native RAM. The
translator must never turn a guest-selected native pointer/control-flow target
into an unchecked RV access. RF banking saves registers; it grants no memory
privilege. No native Sv32 MMU or PMP-style protection has been implemented.

If untrusted native plugins or confinement of faulty generated RV code becomes
a requirement, it needs a separate native execution-domain/access-control
contract. Guest translation alone cannot provide it. For the current single
trusted interpreter/JIT, do not add a second page-table walker merely to run
firmware. Keep firmware native addresses stable while guest CR3/CPL change.

## Guest-memory interface: separate operations, one RF bank

The [different-views principle](#different-instructions-different-views) leads
to dedicated guest `XLD/XSW` operations alongside native `LW/SW`. They now exist
in the ISS and Chisel, with context publication, segment proofs and a same-bank
software-service redirect. [GUEST_MEMORY.md](GUEST_MEMORY.md) records validation,
resources and remaining integration. Their operands and effects are documented
beside the ISS implementation, not defined independently here.

The operation must be qualified for the **current guest context**: real/protected
mode and paging are separate state, together with CPL/WP, relevant CR4 state,
segments and A20/backing rules. Software guest state remains authoritative;
hardware lookup state is a cached proof. A changed assumption must update the
access context and invalidate old proofs before the next guest memory operation.
The current interface invalidates TLB and segment proofs on changed published
context. Software must publish changes to its authoritative x86 state before
the next guest operation; hardware cannot observe arbitrary Rust field writes.
XTLOOK remains a translation-only primitive and does not qualify segmentation.

Initially accelerate qualified 32-bit protected-mode RAM. Other states, including
complex segmentation and real/v8086-mode cases, can execute through software
while preserving their exact behavior. A miss handler may fill and retry only
after a valid cacheable-RAM proof. MMIO/unsupported cases execute once in software;
guest faults preserve the old instruction boundary and are delivered as x86
exceptions. Miss is not synonymous with #PF, nor does every miss permit a fill.
Hardware rejection reasons describe a failed proof, not an x86 exception.
Software must determine exact fault priority and side effects. A native bus
error on an allegedly valid RAM hit is a backing/firmware error. Store-completion
flag publication is implemented; multi-register/EIP/time commit remains software.

The guest service does not swap RF banks. The [single interpreter](../experiments/INTERPRETER.md)
uses ordinary Rust calls and a scoped translation table, with no second Frame
or whole-instruction fallback. All guest GPRs/EFLAGS belong to its Cpu. The
current integration publishes qualified flat-segment proofs for selected XLD
reads. Its miss adapter returns within the original inline-asm boundary to the
same Rust memory helper. Other reads use XTLOOK; all stores retain software write
notifications. Page proofs follow the software cache lifetimes. Banking cannot
establish address isolation, x86 mode or rollback.

Existing B1 generated-code entry/exit still depends on banking; it has not been
removed by the TLB probe. A replacement follows ISS-first validation, including
both returned native registers and guest state. Do not retain an unused banking
ABI solely for private backwards compatibility, or remove it before its callers
have a tested replacement.

## Lifetime and coherence

Software flushes guest mappings when their guest privilege/paging/backing
assumptions change. Native bank switches do not change the guest address space.
Native FENCE.I publishes generated RV instructions; it does not invalidate guest
translations. Keep these operations separate.

Code watches are keyed by **guest physical identity**, not native code-cache
addresses or guest linear aliases. First publication or changed write eligibility
flushes all cached write proofs before execution resumes. Every refill consults
the physical watch map, and runtime/DMA/page-table writers participate in the
[source invalidation protocol](CODE_CACHE.md). Returned native pointers must not
outlive a service, context change or writer handoff.

Integration must test guest PTEs naming low firmware-like addresses, addresses
outside guest RAM, remapped windows, aliases, permission changes, and page splits.
An illegal native mapping is a firmware defect; it is not permission to invent
an x86 page fault or repeatedly retry the same access.
