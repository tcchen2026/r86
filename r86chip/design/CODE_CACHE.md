# Translation lookup, publication and self-modifying code

Contract for the banked RV32/Xr86 design. This is a design for guest
integration, **not an already integrated hardware SMC protocol**. The
native RTL slice can test FENCE.I and the isolated [TLB/window probe](TLB.md),
but does not implement guest write-watch.

The current instruction contract is defined by the [Rust ISS](../sim/rv32.rs),
indexed in [ISA.md](ISA.md). New hardware-visible operations proposed here must
be modeled and tested there before implementation in Chisel.

See [RUNTIME.md](RUNTIME.md) for cold interpretation, hot-block promotion and
precise x86 commit. Heat belongs to individual guest entries; write protection
belongs to physical source pages of published translations. These are separate
properties.

## What the chip knows

There are three different addresses: guest CS:EIP, guest physical source bytes,
and native RV32 code-cache addresses. Native fetch sees only RV32/custom code.
It never guesses whether an instruction word contains x86 or translated code.

The runtime maintains a translation table in ordinary RAM. A dispatch key includes
the guest entry and an execution-context identity/generation covering every
assumption specialized into the block: CS base/limit/attributes, operand/address
mode, CPL, segmentation and address-space/paging state as applicable. Lookup also
validates the current executable mapping and permissions of each source page,
and the versions of the physical pages containing the translated bytes. A
remapping to a different physical page must miss even if its version is equal.

Multiple entry points, including overlapping x86 decodings, may occupy one page.
A page marked `CODE_WATCH` only means that a write needs attention; it does not
mean every byte is a translated entry. Bank 0 selects initial interpreted or
translated execution. A bank-1 dispatch stub can select subsequent valid blocks
without materializing guest state; a miss returns to bank 0 for interpretation
or compilation. No hardware associative block table is required.

A descriptor records its key, native entry/range, all source physical page/version
pairs, context generation, code-cache epoch and publication state. Blocks crossing
a page boundary depend on both pages. Control transfers still perform guest
execute/segment checks; a table hit must not suppress architectural faults or
required page-table accessed-bit effects.

## Borrowing from Transmeta

Transmeta's primary sources cover the original page-bit scheme, subsequent
mixed-page mechanisms and their limits. Our first version
adopts coarse write detection and software invalidation. Retention and exact
revalidation are the next proposed software experiment; finer hardware protection
requires measured workloads and a correctness argument. None of these mechanisms
requires VLIW. Register banking separately reduces context traffic and does not
provide speculative rollback.

## Metadata and publication

Keep one conservative watch bit and a version per **guest physical** RAM page in
runtime-owned RAM. For 128 MiB / 4 KiB pages the bitmap is 4 KiB; 32-bit versions
take 128 KiB. These are memory capacities, not LUT estimates. The current small
guest TLB can cache the resulting denied-write permission; it has no independent
physical watch bitmap or snoop. First publication invalidates all guest TLB entries
so existing aliases cannot retain an unprotected cached bit. A later targeted
invalidation must cover every alias.

Publish while guest execution and other guest-memory writers are stopped:

1. Read source using the guest's instruction-fetch rules and track every source
   page and context assumption. Do not speculatively expose a fault from bytes
   beyond the actually required guest instruction stream.
2. Generate native code and dependency metadata privately. Hold writer exclusion
   throughout, or revalidate the source snapshot under exclusion before publishing.
3. Mark all source pages watched and invalidate stale guest-TLB watch state.
4. Complete native code/metadata writes and execute native FENCE.I. Future cache
   maintenance must cover both the memory visibility and instruction-fetch sides.
5. Install the valid lookup entry last, then allow guest execution/writers again.

Single-core execution simplifies ordering but does not by itself exclude an
independent DMA master. The protected translation/code-cache region is outside
the guest-physical aperture and inaccessible through guest XMEM.

## A write to translated source

The checked guest store path resolves segmentation, paging and ordinary write
eligibility before making a data write visible. A watched physical destination
causes a **service exit before that write**. For RMW, write-intent checking should
occur before a read that could have an irreversible device effect. Slow or
cross-page cases stay in the shared interpreter until their ordering is proven.

At the exit, prior x86 instructions are committed and the faulting instruction's
pending GPR/flag/data effects have not committed. The runtime materializes that
state, invalidates dependent translations, increments affected page versions,
and executes the instruction once through the shared interpreter's watched-write
path. Pre-write invalidation may conservatively happen even if the instruction
ultimately faults. Page-table A/D effects already required by guest semantics are
not rolled back. MMIO is never repeated after a device side effect.

Then dispatch from the resulting **guest EIP**. Never return to the old native
continuation of the invalidated block. For example:

```
guest A: write a new immediate into instruction B
guest B: use that immediate                 # same translated block
```

The store at A exits before writing. Software invalidates the block, executes A,
and dispatches B from the new bytes. Only marking a table entry invalid and
continuing the current native block would execute stale B. Entry version checks
alone cannot fix that error. Ordinary RAM read replay is permitted only where
exclusive memory ownership and instruction semantics make it safe.

Watch bits initially stay set until a global cache flush. Writes to unrelated
data on the same page may therefore cause unnecessary exits; correctness comes
before finer-grain tracking. The runtime write hook bypasses repeated hardware
service trapping while bank 1 is stopped, but still performs all invalidation.

### Proposed retention experiment

Do not equate making a translation unreachable with immediately discarding its
native bytes. After the conservative baseline is verified, compare retaining
suspended candidates against recompilation. A write first removes dispatch/link
eligibility as above. With guest execution and other writers stopped, perform
the write and compare the candidate's exact source bytes with its saved snapshot;
validate all source mappings, permissions and context again. Unchanged candidates
may be republished with current page versions and cache epoch. A changed candidate
remains invalid. A hash match alone is not proof, and changing version numbers
without validating every dependency is unsafe.

Redispatch from guest EIP even if an old translation has been successfully
republished. Do not use this optimization to resume a stale native continuation.
Retained candidates consume bounded metadata/code-cache capacity and must be
discarded before their native addresses are reused.

Retention avoids some compilation work but not page-watch exits. If measured
mixed-page traffic still dominates, evaluate a small cache of physical-page
subregion masks. Misses and evictions retain conservative coarse protection.
Installing a new translation must update protection before publishing it. This
is a later hardware experiment, not implemented support or a fixed granularity.

## Every physical writer participates

The protocol includes interpreted guest stores, generated XMEM stores, software
page-walker A/D updates, emulated DMA, loaders and debugger writes. A page table
may itself alias executable guest bytes. Native RV stores into guest RAM must go
through the runtime's physical-write hook; a native SW cannot magically inherit
XMEM protection. Native writes to the separate translation cache use publication
and FENCE.I instead.

Initially devices update guest RAM through the runtime while guest execution is
stopped. An independent DMA engine later needs a write arbiter/watch mechanism,
or a quiesce/invalidate protocol before it can change guest memory. Excluding
peripheral LUTs from the budget does not exclude their correctness obligations.
Virtual aliases A and B share the same physical-page version and watch state.

DMA reads also require coordination: they must observe committed guest data,
including dirty cache contents. Servicing both directions through the coherent
runtime memory view at guest boundaries is the initial plan. A later independent
master needs an explicit visibility/ordering contract, not only a code-write
notification. Code-watch consistency and ordinary data-cache coherence are
related but separate obligations.

## Links, reuse and bounded metadata

Start with validated software dispatch, including guest event/time-budget checks.
Do not emit unchecked raw links into another block's body. Later links need an
invalidatable entry guard or tracked incoming links, plus mapping/context/event
validation. Invalidating an entry never licenses continuing a currently affected
block after a source write.

Use a bump-allocated code cache and global reclamation at a guest boundary first.
Clear dispatch entries and all retained native targets before reuse, bump the
cache epoch, and publish replacement instructions with FENCE.I. Generation or
version wrap forces a quiescent global flush/reset; it must not make an ancient
descriptor valid again. Clear sticky watch bits only after all translations and
cached TLB watch state have been retired. Per-block reclamation is later work.

## Integration acceptance cases

Required tests include distinct entries on one page; changed context at one EIP;
a block's second source page modified; writes through physical aliases; the
current block rewriting its next instruction; interpreted, DMA and A/D writes;
faulting stores and non-replayed MMIO; publication with stale prefetched native
instructions; version wrap; and code-cache address reuse. Native FENCE.I tests
alone do not establish any of these guest invalidation guarantees.
