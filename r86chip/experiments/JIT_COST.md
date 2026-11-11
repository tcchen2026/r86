# Resident translator instruction cost

Intern measurements for Lead, ISS only, 2026-11-11. Current contracts:
[B1_JIT.md](B1_JIT.md). These are deterministic retired native words, not RTL
clocks, host timings or Windows performance. The 3M–4M target of 10 words/x86
is **not met**. Its cost fell from the accepted baseline's 320.716248 to
11.782099; the later window remains more expensive.

`jit_cost.py` runs one firmware with translation off/on at two guest endpoints.
It subtracts cumulative counts, including compiler, runtime, interpreter,
services and devices. The denominator is completed x86 instructions. Both
endpoints must match guest registers/EIP/EFLAGS, RAM, TSC, serial and
completed/fault/repeat counts. All requested endpoint comparisons passed.
No full-boot, RTL, Windows, Pi or Apple evidence is claimed by this batch.

## Matched TinyCore intervals

| Steps | Interpreter words/x86 | Translator words/x86 | Coverage | Translated x86/XENTER | Emitted words/translated x86 |
|---|---:|---:|---:|---:|---:|
| 1M–2M | 111.426043 | 4.659524 | 99.921500% | 766.857252 | 2.285277 |
| 3M–4M | 119.510018 | 11.782099 | 99.166100% | 303.445838 | 2.002493 |
| 4M–10M | 165.450609 | 38.323617 | 97.478350% | 45.116681 | 2.532062 |

| Steps | Total native words | Translated x86 | XENTER | Bank-0 words/XENTER | Compiled blocks | Code invalidations |
|---|---:|---:|---:|---:|---:|---:|
| 1M–2M | 4,659,524 | 999,215 | 1,303 | 1822.848810 | 18 | 0 |
| 3M–4M | 11,782,099 | 991,661 | 3,268 | 2997.405447 | 356 | 0 |
| 4M–10M | 229,941,704 | 5,848,701 | 129,635 | 1659.463594 | 2,007 | 2 |

Bank-0 words/XENTER amortizes **all** bank-0 work in the interval, including
interpretation and compilation; it is not the latency of XENTER itself.
Host-only per-PC profiles account for every retired word, retaining negative
endpoint deltas. Allocated code and the single-word service stub execute in
bank 1; all other linked text executes in bank 0. The stub count equals the
service-exit histogram and is excluded from bank 0. Emitted code includes
XTICK, branches, exits and retries, but excludes that stub.

## Runtime attribution

The five hottest runtime functions by exclusive retired words follow. Inline
DWARF frames select the innermost runtime function, folding its closures into
it; out-of-line callees retain their own counts. These are attributed costs,
not inclusive call-graph totals. Full function reports remain in the local
batch directory, including interpreter, compiler, allocator and device work.

| Steps | Function | Native words |
|---|---|---:|
| 1M–2M | `Engine::site` | 681,278 |
| 1M–2M | `Engine::run_batch` | 198,969 |
| 1M–2M | `Engine::sync_context` | 155,479 |
| 1M–2M | `Engine::execute` | 141,112 |
| 1M–2M | `Engine::lookup` | 89,766 |
| 3M–4M | `Engine::run_batch` | 1,753,423 |
| 3M–4M | `Engine::lookup` | 1,569,120 |
| 3M–4M | `Engine::sync_context` | 409,622 |
| 3M–4M | `Engine::execute` | 328,256 |
| 3M–4M | `Engine::unlink` | 320,640 |
| 4M–10M | `Engine::run_batch` | 34,977,263 |
| 4M–10M | `Engine::lookup` | 29,831,216 |
| 4M–10M | `Engine::sync_context` | 21,847,288 |
| 4M–10M | `Engine::execute` | 15,934,691 |
| 4M–10M | `Engine::unsupported` | 8,205,035 |

## Handoffs

| Steps | Miss | Service | Tick | Interpreter exit | Compiled branch / unsupported / window / limit |
|---|---:|---:|---:|---:|---|
| 1M–2M | 0 | 869 | 434 | 0 | 18 / 0 / 0 / 0 |
| 3M–4M | 525 | 784 | 1,270 | 689 | 338 / 18 / 0 / 0 |
| 4M–10M | 92,167 | 7,869 | 4,002 | 25,597 | 1869 / 138 / 0 / 0 |

Services are attempts, including refills that resume the same word. All are TLB
reason 4 except 18 split/alignment services in 4M–10M. Dynamic unsupported counts
include negatively cached entries, but exclude cold/tick/service fallbacks.
The observation reads only the approved fetch window; an opcode split across a
page boundary can lack a group/second-byte count.

Before the new emitters, 3M–4M F7 handoffs were /2 NOT=47, /3 NEG=5,426.
Both become zero. TEST immediate, register MOVZX/MOVSX and rel32 Jcc were already
supported; this batch adds register NOT/NEG and memory SETcc. Memory NOT/NEG
remain outside the shared decoder's supported forms. No multiplication or
division emitter was added.

| Steps | F7 /r | 0F second byte | Other multiplication |
|---|---|---|---|
| 1M–2M | none | none | 69=0, 6B=0 |
| 3M–4M | none | A2=34, A3=696, AB=8, B1=3, B3=3 | 69=0, 6B=7 |
| 4M–10M | /4=18, /6=10 | 01=69,300, 31=8,184, 32=27, A5=5, AC=33, AD=97, B1=88, BA=39 | 69=5, 6B=8,085 |

F7 /4 is unsigned MUL and /6 unsigned DIV; /5 IMUL and /7 IDIV have no
recorded handoffs. 0F AF IMUL has none. 69/6B are immediate IMUL. In 4M–10M,
0F 01=69,300 accompanies 69,304 calls to the shared INVLPG handler (including
cold paths); it is the measured invalidation workload, not a multiplier problem.
The complete first-byte histogram is in each report.

## Remaining costs and reproduction

The former source/link scan is gone. The service-refund `site` lookup still
searches live entries; it dominates the first interval's runtime attribution.
The 3M–4M tail includes 8,339 interpreted instructions and 356 compilations.
Lookup, batch dispatch, compilation and interpretation still matter. In the
later interval, system instructions and invalidation boundaries dominate;
coverage alone does not establish a cheap runtime.

An intermediate 4M–10M run spent 2,188,816,420 words restoring all links after
frequent INVLPG. Conservative installed-code bounds and existing XTINVAL
removed unnecessary code/data flushes: services fell from 500,320 to 7,869,
misses from 1,467,970 to 92,167. The final table includes these fixes. No new
hardware word is justified by the small MUL/DIV counts; remaining lookup and
service bookkeeping should first be addressed in firmware.

Exact ELF/ISS hashes and stage evidence: `tmp/r86chip/batch3/results.json`,
`qualified-{1m-2m,3m-4m,4m-10m}/report.json` and Intern's
`tmp/r86chip/handoff3.txt`. The accepted baseline is in `tmp/r86chip/batch2/`;
superseded runs are local evidence, not current contracts.

```sh
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT --case tinycore --start 1000000 --end 2000000
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT --case tinycore --start 3000000 --end 4000000
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT --case tinycore --start 4000000 --end 10000000 --seconds 300
```

Use distinct OUT directories. Build with debug info as shown in B1_JIT.md for
inline attribution. Workload flags 4|8 select histogram version 2 in the existing
observation buffer instead of boundary records; the decoder also reads the
saved version-1 baseline. ISS and RTL files were not changed.
