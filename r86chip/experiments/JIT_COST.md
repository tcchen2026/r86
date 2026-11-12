# Resident translator cost

Intern measurements for Lead, 2026-11-11, task 4. Contracts and table layout:
[B1_JIT.md](B1_JIT.md). **The 3M–4M target of 15 estimated clocks/x86 is not
met: the result is 23.779750.** All requested guest endpoint comparisons pass.
No opcode coverage or word semantics changed. These are ISS counts and a cache
estimate, not measured task-4 RTL clocks, host timings or Windows performance.

## Estimator and calibration

`r86chip-sim machine ... --cache-stats` appends `cache_estimate` without changing
existing execution fields. Two separate 64 KiB caches, two ways, 32-byte lines;
empty ways first, then the RTL sequencer's shared alternating replacement bit.
Stores allocate. FENCE.I empties only instruction tags. Image loading and host
observations are excluded; device addresses (bit 31) bypass the model. Fetches
hidden by the ISS's fetched-word cache are counted explicitly. The only core
edits are three counter calls. No writeback, prefetch, replay or pipeline model.

Estimated clocks = 1.15 × retired native words + 45 × demand line fills.
The saved task-3 ELF `046cd834…` calibrates against Lead's same-ELF RTL results:

| Interval | Estimated clocks/x86 | Lead RTL clocks/x86 | Error | Estimated / RTL fills |
|---|---:|---:|---:|---|
| 1M–2M | 7.039518 | 7.36 | -4.35% | 37,357 / 39,464 |
| 2M–3M | 5.665280 | 5.95 | -4.79% | 22,999 / 24,315 |
| 3M–4M | 28.770529 | 30.17 | -4.64% | 338,247 / 346,719 |

Every old execution field matches the saved task-3 endpoint, including retired
words and whole-native-RAM hash; only host `seconds` is excluded. Enabling
per-PC fill profiling also leaves all execution fields and estimates unchanged.
`--profile --cache-stats` writes six fill columns per PC. Endpoint subtraction
retains negative deltas, and both per-PC and per-function sums must equal the
reported total. Profiles attribute a fill to the instruction causing it, not
the address being fetched/stored.

The original firmware-region interpretation needs correction: in task-3 3M–4M,
233,971 fills are instruction-side firmware, versus 2,945 data-side firmware
and 88,844 data-side heap. Repeated FENCE.I discards useful instruction lines;
a larger instruction cache cannot preserve lines across those flushes.

## Matched TinyCore intervals

One final ELF runs translation off/on at each endpoint. Guest registers/EIP/
EFLAGS, RAM, TSC, serial and completed/fault/repeat counts agree. Counts include
all compilation, dispatch, interpretation, devices and invalidation in the
interval; the denominator is completed x86 instructions.

| Interval | Task-3 words/x86 | Task-4 words/x86 | Task-3 estimated clocks/x86 | Task-4 estimate | Reduction |
|---|---:|---:|---:|---:|---:|
| 1M-2M | 4.659524 | 3.772755 | 7.039518 | 5.614778 | 20.24% |
| 3M-4M | 11.782099 | 12.267622 | 28.770529 | 23.779750 | 17.35% |
| 4M-10M | 38.323617 | 35.206448 | 63.359992 | 49.818195 | 21.37% |

The middle interval retires 4.1% more words while filling 36.5% fewer lines.
Key replacement retains code; service lookup and generation reset do not scan
resident entries. Bounded page-local compilation groups reduce 3M–4M FENCE.I
from 599 to 222, but compile 499 blocks instead of 356. A conservative old
physical watch causes one extra invalidation in 4M–10M (3 versus 2).

| Interval | Interpreter words/x86 | Interpreter estimate | Coverage | Translated x86/XENTER | Emitted words/translated x86 |
|---|---:|---:|---:|---:|---:|
| 1M-2M | 111.428483 | 140.273585 | 99.921300% | 769.217090 | 2.115807 |
| 3M-4M | 119.512458 | 141.182382 | 99.163900% | 355.298818 | 1.912928 |
| 4M-10M | 165.461237 | 222.705188 | 97.563767% | 47.374467 | 2.475446 |

| Interval | Native words | Translated x86 | XENTER | Bank-0 words/XENTER | Compiled blocks | Code invalidations |
|---|---:|---:|---:|---:|---:|---:|
| 1M-2M | 3,772,755 | 999,213 | 1,299 | 1276.172440 | 16 | 0 |
| 3M-4M | 12,267,622 | 991,639 | 2,791 | 3715.544249 | 499 | 0 |
| 4M-10M | 211,238,687 | 5,853,826 | 123,565 | 1592.206070 | 2,662 | 3 |

Bank-0 words/XENTER amortizes all bank-0 work, not just transition latency.
Emitted words include block heads/exits/retries. The single bank-1 service-stub
word is separate; its count equals service exits. Other linked code is bank 0.

## Cache fills

All figures are interval totals; I=instruction, D=data. Firmware is below
0x02000000, heap below 0x10000000, guest above it (excluding devices).

| Interval/version | I firmware | I heap | I guest | D firmware | D heap | D guest | Total |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1M-2M task 3 | 27,206 | 4,689 | 0 | 248 | 2,296 | 2,918 | 37,357 |
| 1M-2M task 4 | 22,800 | 1,202 | 0 | 110 | 1,380 | 2,866 | 28,358 |
| 3M-4M task 3 | 233,971 | 8,854 | 0 | 2,945 | 88,844 | 3,633 | 338,247 |
| 3M-4M task 4 | 183,433 | 5,903 | 0 | 1,584 | 20,902 | 3,111 | 214,933 |
| 4M-10M task 3 | 1,613,689 | 239,788 | 0 | 16,544 | 685,002 | 16,688 | 2,571,711 |
| 4M-10M task 4 | 1,085,769 | 55,411 | 0 | 5,812 | 84,504 | 12,608 | 1,244,104 |

Largest task-4 fill contributors follow. These are exclusive native-PC counts;
inline DWARF frames prefer runtime attribution, with out-of-line callees kept
separate. Full six-column function lists are in each `functions.json`.

| Interval | Function | I fills | D fills | Total |
|---|---|---:|---:|---:|
| 1M-2M | `generated-code` | 1,202 | 2,304 | 3,506 |
| 1M-2M | `runtime::Engine::execute` | 1,436 | 94 | 1,530 |
| 1M-2M | `interp::Interpreter::run_batch_inner` | 1,413 | 51 | 1,464 |
| 1M-2M | `runtime::Engine::refill` | 997 | 408 | 1,405 |
| 1M-2M | `runtime::Engine::lookup` | 1,209 | 81 | 1,290 |
| 3M-4M | `compile::block` | 17,660 | 513 | 18,173 |
| 3M-4M | `jit::decode::Reader::instruction` | 15,673 | 511 | 16,184 |
| 3M-4M | `interp::Interpreter::run_batch_inner` | 12,989 | 565 | 13,554 |
| 3M-4M | `runtime::Engine::compile_group` | 9,727 | 2,417 | 12,144 |
| 3M-4M | `runtime::Engine::lookup` | 5,303 | 6,755 | 12,058 |
| 4M-10M | `interp::Interpreter::run_batch_inner` | 132,565 | 1,888 | 134,453 |
| 4M-10M | `compile::block` | 79,016 | 2,389 | 81,405 |
| 4M-10M | `jit::decode::Reader::instruction` | 73,037 | 1,936 | 74,973 |
| 4M-10M | `generated-code` | 55,411 | 4,006 | 59,417 |
| 4M-10M | `runtime::Engine::lookup` | 34,338 | 20,817 | 55,155 |

For Lead's next decision: 3M–4M's largest single fill source is `compile::block`
(18,173, including 17,660 instruction fills), then the shared decoder's
`Reader::instruction` (16,184). Remaining cost is not a resident-table scan.
The 15-clock target still needs work; RTL should first confirm the estimate.

## Runtime attribution and handoffs

Top five runtime functions by exclusive retired words:

| Interval | Function | Words |
|---|---|---:|
| 1M-2M | `Engine::run_batch` | 211,321 |
| 1M-2M | `Engine::execute` | 148,638 |
| 1M-2M | `Engine::sync_segments` | 108,234 |
| 1M-2M | `Engine::refill` | 86,761 |
| 1M-2M | `Engine::lookup` | 42,326 |
| 3M-4M | `Engine::run_batch` | 1,831,959 |
| 3M-4M | `Engine::lookup` | 1,208,349 |
| 3M-4M | `Engine::compile_group` | 325,922 |
| 3M-4M | `Engine::sync_mapping` | 319,917 |
| 3M-4M | `Engine::execute` | 306,842 |
| 4M-10M | `Engine::run_batch` | 35,957,607 |
| 4M-10M | `Engine::execute` | 15,870,973 |
| 4M-10M | `Engine::sync_segments` | 14,650,272 |
| 4M-10M | `Engine::lookup` | 13,816,877 |
| 4M-10M | `Engine::sync_mapping` | 6,126,106 |

| Interval | Miss | Service | Tick | Interpret | Compiled branch / unsupported / window / limit |
|---|---:|---:|---:|---:|---|
| 1M-2M | 0 | 865 | 434 | 0 | 16 / 0 / 0 / 0 |
| 3M-4M | 228 | 604 | 1,270 | 689 | 480 / 19 / 0 / 0 |
| 4M-10M | 86,997 | 6,916 | 3,993 | 25,659 | 2486 / 174 / 2 / 0 |

All services are TLB reason 4 except 18 split/alignment services in 4M–10M;
no context/segment services remain. Counts include successful refills. Dynamic
unsupported counts include negative entries, exclude cold/tick/service handoffs,
and can lack a second byte when the approved window ends at the page boundary.

| Interval | F7 /r | 0F second byte | Immediate IMUL |
|---|---|---|---|
| 1M-2M | none | none | 69=0, 6B=0 |
| 3M-4M | none | 20=1, A2=34, A3=698, AB=8, B1=2, B3=4 | 69=0, 6B=5 |
| 4M-10M | /4=19, /6=10 | 01=69,300, 31=8,184, 32=27, A5=5, AC=34, AD=96, B1=94, BA=39 | 69=9, 6B=8,095 |

No added opcode coverage: register NOT/NEG and memory SETcc were task 3.
F7 /4 is unsigned MUL, /6 unsigned DIV. The later interval still has 69,300
0F 01 handoffs (the INVLPG workload) and 8,184 RDTSC handoffs.

## Evidence and limits

Fourteen emitter/arena tests pass, including stable service records after key
collisions, epoch reuse and arena capacity. Seventeen final-ELF runtime fixtures
pass, including negative-entry INVLPG, code writes/aliases, CR3/A20 remapping,
precise faults, cross-page fetch, timer/STI boundaries and based segments.
RV32 no_std release and host simulator builds pass. A first library-test command
hit an existing missing `alloc::vec` import in shared decoder tests; the arena
test instead runs alongside emitter tests in the simulator test binary.

A preliminary context split failed the cost pilot by causing segment-service
fallbacks: XTCONTEXT clears descriptors, so software must invalidate its cached
publication state too. Fixed before the final runs. Scan removal alone estimated
25.594122 clocks/x86; bounded compilation improved it to the reported result.
No full guest run exhausted the 8 MiB arena; rollover is covered by its unit
check and the same generation-reset path used by remap/SMC fixtures.
No task-4 RTL, full TinyCore boot, Windows, board, Pi or Apple result is claimed.

Canonical local evidence: `tmp/r86chip/batch4/results.json`,
`calibration/report.json`, `calibration-10m/report.json`,
`final-{1m-2m,3m-4m,4m-10m}/report.json`, their interval/function profiles,
`final-checks/report.json`, `emitter-tests.log`, and linked `base-runtime.objdump`
/ `final-runtime.objdump`. `template/firmware.elf` is the accepted baseline;
`group/firmware.elf` is the final runtime. Superseded binaries, raw endpoint
profiles and full objdumps are removed; cleanup manifests retain their hashes.
Final comment/module-order cleanup rebuilds the identical loaded image;
`format-equivalence.json` records both hashes. Retained ELF is the measured one.

Final ELF SHA256: `e546c0c3c5a754292509b764e6abf31108d754667313168b557a23ebacbc94b6`.
Final ISS SHA256: `bbd8c7d764e3a9d24fab64670f633e91dc86f7935a1cf000c9aa8d5b6ff26b05`.

Build/test commands are in B1_JIT.md. To reproduce each interval:

```sh
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT --case tinycore --start 1000000 --end 2000000
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT --case tinycore --start 3000000 --end 4000000
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT --case tinycore --start 4000000 --end 10000000 --seconds 300
```

Use distinct OUT directories. Prepare TinyCore input as in `tinycore.py`, then
preserve the final ELF there. Debug info supports inline attribution without
adding firmware profiling instructions. ISA and guest-clock contracts remain
unchanged. The old task-3 report remains local evidence under `batch3/`.
