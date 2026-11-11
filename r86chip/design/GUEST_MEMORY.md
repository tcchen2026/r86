# Checked guest memory and same-bank software service

The [Rust ISS](../sim/rv32.rs) defines and implements guest
loads/stores, published access-context/segment proofs, store-completion flag
publication and a native software-service entry/return. Chisel follows that
model. The current [single interpreter](../experiments/INTERPRETER.md) uses
XTLOOK and qualified XLD reads, with misses continuing the existing Rust access
helpers. Fresh instruction bytes use XLD with execute proofs in a separate
small fetch table; same-page word/dword fetches can assemble two native bus words.
Stores preserve software write notifications; XSW remains a separately
tested building block. The original assembly interpreter was retired. The average
20-word target is unmet. [Address ownership and design principle](ADDRESS_SPACE.md).

## Why this boundary

The earlier XTLOOK/branch/native-load sequence costs three native words on a
hit and requires separate segment/context qualification. Dedicated guest
operations let a directly decoded word check published proofs and use the same
native data bus. Ordinary RV loads/stores keep native meaning even in a service.
There is no hidden interpreter, page walker or uop stream inside the operation.

Software owns the x86 CPU state and establishes proofs using its existing
segmentation, paging and physical-memory semantics. The current hardware admits
qualified protected-mode RAM. A change of published context expires page and
segment proofs; unsupported modes or permissions request software. Published
state must stay synchronized with the software CPU before guest execution.

The rejection reason is only a diagnostic hint. The service must determine the
actual guest exception, required page-table A/D effects and whether RAM is
eligible for caching. MMIO, split accesses and unusual segmentation may require
software completion instead of retry. Bad native backing is a native error,
not a guest page fault. Exact encodings, ordering and effects are documented
beside `Core::step`; this document does not define a second ISA.

A miss redirects the same native instruction stream without changing RF bank,
GPRs or either flag value. The service record supplies the suspended operation;
software saves any live native registers it needs to reuse. A qualified refill
can return to the original word. A completed software instruction or guest
exception needs an explicit operation continuation. In the unified interpreter,
the XLD adapter clobbers only a declared native temporary and resumes just after
XLD, inside the original inline-asm region. Rust then continues the same memory
operation on a miss. The handler neither touches guest data nor decides x86
faults; segment checks and the shared walker do that. XTLOOK callers branch on
its ordinary miss result. No whole-instruction interpreter handoff is involved.

For RMW, the load first requires a write-capable proof. Arithmetic remains
pending until the store completes; the store's commit form publishes status
only after success. This is not a transaction over multiple RV words: EIP,
time, other guest registers and earlier native writes are software-owned.

## Validation and scope

The initial guest-memory revision's 35 Rust tests included ten focused
[guest-memory tests](../sim/guest_memory_tests.rs): native/guest views and
widths, independent segment/page permissions, limit overflow and wrapping
linear addition, first/changed context invalidation, actual RV refill/retry,
RMW and store/flags completion, native bus failures, service nesting/return,
illegal forms and fresh guest-byte visibility through aliases/native writers.
Existing [TLB tests](../sim/tlb_tests.rs) exercise composition through the shared
x86 walker, including A/D and privilege/WP policy, at host-test level.

The native suite compares the same binaries against the ISS before RTL, then
checks every retirement, data request, store and terminal trap using the
independent Python oracle. The subsequent split-table revision passes
118 programs / 944 RTL runs and 41 ISS tests, including cross-view isolation,
revocation and all-view invalidation. The wide-fetch revision extends these to
**139 programs / 1,112 RTL runs** and **44 ISS tests**: every byte lane/width,
static/register segments, destination aliases/x0, rejection priority, current
bytes after writes, and first/second native bus failures. Truncated RAM images
give ISS and RTL the same inaccessible second word; the harness compares both
bus requests and the single final retirement or precise trap.
Both one/two-bank configurations run under Verilator
with three memory schedules and four-state Icarus. Register RAM and invalid
proof payloads may start unknown: only initialized/proved values may escape.
Fresh guest instruction-byte reads use the data path and see preceding writes;
this does not validate the complete JIT source-invalidation protocol.

The refill fixtures execute real native instructions and read native RAM
metadata; they do not invoke a host x86 callback. They install known fixture
mappings. The [unified firmware consumer](../experiments/INTERPRETER.md) mirrors
qualified software data/execute translations into their respective tables and uses XLD
for selected reads and original instruction bytes. CS base/limit and execute
rights are independent of data-read rights; fetch-cache invalidation retires
mirrored X proofs. Word/dword immediates and displacements use the same-page
fetch form in the ordinary fetch helpers and existing ModR/M decoder. Rejected
spans retain the shared software helpers, including ordered cross-page bytes.
Its full-PC tests are separate from the native XLD/XSW service
fixtures; [the interpreter report](../experiments/INTERPRETER.md) records actual
read coverage and same-binary ISS/RTL observations.

## Measured cost

[guest_memory_cost.py](../experiments/guest_memory_cost.py) compares 16 versus
1,024 unrolled operations on the **one-bank** Verilator core, with identical
binaries checked on the ISS. Zero-wait RAM; common setup/stop costs cancel.

| Component | Retired native words | Clocks |
|---|---:|---:|
| Warm guest load, including context/segment/TLB qualification | 1 | 2 |
| Warm guest store, same qualification | 1 | 2 |
| Warm write-intent load + arithmetic + committing store | 3 | 6 |
| Forced cold load with fixture refill | 7 | 18 |

The last row includes invalidation, the redirecting load attempt, four RV
service words and the retried load. Both redirects retire and are counted.
It excludes a real x86 page walk and a full service save/restore convention.
None of these rows includes raw-byte x86 decoding, effective-address formation,
dispatch, guest EIP/time/events or whole-machine work. They do not satisfy the
[20-word acceptance test](INTERPRETER.md#performance-objective-and-acceptance).

GW5A mapping with the existing pinned Yosys flow:

| Configuration | Logic LUT1–4 | Carry ALU cells | FF | BSRAM |
|---|---:|---:|---:|---:|
| One bank, preceding TLB/window revision | 2,909 | 671 | 1,198 | 2 |
| One bank, guest memory/context/service | 3,973 | 774 | 1,771 | 2 |
| One bank, register-segment XLD added | 3,978 | 774 | 1,771 | 2 |
| One bank, separate data/fetch tables | 4,184 | 769 | 1,915 | 2 |
| One bank, same-page wide fetch | 4,317 | 791 | 1,948 | 2 |
| Two banks, preceding TLB/window revision | 3,237 | 675 | 1,328 | 2 |
| Two banks, guest memory/context/service | 4,237 | 778 | 1,901 | 2 |
| Two banks, register-segment XLD added | 4,277 | 773 | 1,901 | 2 |
| Two banks, separate data/fetch tables | 4,451 | 778 | 2,045 | 2 |
| Two banks, same-page wide fetch | 4,564 | 795 | 2,078 | 2 |

The one-bank increment is 1,064 logic LUTs, 103 carry cells and 573 FF.
Register-segment selection adds a further five logic LUTs in the one-bank
mapping, with the same carry/FF/BSRAM counts. The selector's high bits are checked
with a reduction, avoiding a redundant full-width magnitude comparator. The
first equivalent implementation used 4,036 logic LUTs and 800 carry cells;
both implementations pass the same native fixtures. No extra register bank or
hidden instruction sequence is involved.
Separating fetch capacity adds 206 logic LUTs and 144 FF to the one-bank
register-segment revision, with five fewer carry cells and unchanged BSRAM.
The component cost tests above still give the same word and zero-wait clock
counts.
Wide fetch adds 133 logic LUTs, 22 carry cells and 33 FF to the one-bank core.
Its extra state is one saved bus word and a second-beat flag. Each tested warm
wide-fetch form retires one native word at two clocks/operation in the zero-wait
unrolled test; the cross-word forms issue **two data reads**, not one. The
frontend of that revision spaced words two clocks apart, which absorbed the
extra read; the run-ahead frontend does not, so that figure is historical. This
does not make a two-beat access single-cycle or predict cached/DDR throughput.
This leaves room to investigate decoding and cache controllers within the
approximately 10K-LUT scope; it does not prove final fit. LUTs and carry cells
are reported separately, not as routed occupied LUT sites. No 300 MHz result,
DDR/cache timing, board boot or ASIC physical result is established. No budget
increase is currently justified by these component counts.

Reproduce:

```sh
cargo test --no-default-features --features r86chip --bin r86chip-sim
python3 r86chip/experiments/rtl.py --gowin
python3 r86chip/experiments/guest_memory_cost.py
```

Evidence: `experiments/out/rtl/report.json` records source/generated hashes and
all schedules; `out/guest-memory/rtl-before-report.json` preserves the preceding
TLB revision; `out/guest-memory/cost/report.json` identifies component binaries.
Consulted 86Box `src/mem/mmu_2386.c`, then Bochs `cpu/paging.cc` and `cpu/access.cc`,
the shared x86 walker, and tiny386 `tlb_refill`. Inspected the saved linked
tiny386 and RV firmware objdumps under `out/eflags/`. These are design/cost
references, not a matched end-to-end speed comparison. No reference code was
copied and no files under `refs/` were changed.
