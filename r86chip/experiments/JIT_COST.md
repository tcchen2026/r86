# Resident translator instruction cost

Intern measurements for Lead, ISS only, 2026-11-11. Current implementation and
functional contracts: [B1_JIT.md](B1_JIT.md). These are deterministic **retired
native words**, not RTL clocks, hardware timing, or a Windows performance claim.

`jit_cost.py` runs the same RV32 firmware with translation disabled/enabled at
two guest-step endpoints and subtracts cumulative counts. Startup cancels;
compiler, runtime, interpreter and device work inside the interval all count.
Host-only PC profiles separate allocated emitted code from linked firmware.
Emitted words include XTICK, branches, exits and retries; they are not the total
cost. Histograms use the firmware's existing exported observation buffer.

## Matched intervals

| Workload / steps | Interpreter words/x86 | Translator words/x86 | Translated coverage | Translated x86/XENTER | Emitted words/translated x86 |
|---|---:|---:|---:|---:|---:|
| Register loop, 10,000–170,000 | 92.536606 | 12.239038 | 99.634375% | 272.504274 | 1.441797 |
| TinyCore, 1,000,000–2,000,000 | 111.428727 | 5.076474 | 99.921500% | 432.186419 | 2.116349 |
| TinyCore diagnostic, 3,000,000–4,000,000 | 119.512719 | 320.716248 | 98.611400% | 15.998215 | 1.975997 |

All six endpoint pairs agree in guest registers/EIP/EFLAGS, RAM, TSC, serial,
completed instructions, fault attempts and repeat counts. These cost endpoints
precede guest serial output; the separate 20M-step functional comparison checks
4,445 bytes of actual kernel output and 9,159 boundary records. The extra later
window was measured because the long boot was much slower than the requested
early interval. It rejects a general performance claim: high translated
coverage and short emissions do not make the runtime cheap.

Raw reports, source/binary hashes and profiles are under
`tmp/r86chip/batch2/{cost,cost-later}/`. The relevant total/translated/invocation
interval counts are:

| Workload | Total native words | Translated x86 | XENTER | New compiled blocks | Code invalidations |
|---|---:|---:|---:|---:|---:|
| Register loop | 1,958,246 | 159,415 | 585 | 0 | 0 |
| TinyCore 1M–2M | 5,076,474 | 999,215 | 2,312 | 18 | 0 |
| TinyCore 3M–4M | 320,716,248 | 986,114 | 61,639 | 503 | 0 |

## Exit and compilation histograms

Counts are interval deltas. Each service is an attempted native access, including
successful refills that resume without interpretation. All observed services in
these intervals were TLB reason 4; context, segment and split/alignment counts
were zero. Compiled endings count compilation decisions, not dynamic exits.

| Interval | Miss | Service | Tick | Interpreter exit | Compiled branch / unsupported / window / limit |
|---|---:|---:|---:|---:|---|
| Register loop | 0 | 0 | 585 | 0 | 0 / 0 / 0 / 0 |
| TinyCore 1M–2M | 1,009 | 869 | 434 | 0 | 18 / 0 / 0 / 0 |
| TinyCore 3M–4M | 53,082 | 1,262 | 1,138 | 6,157 | 476 / 27 / 0 / 0 |

There were no unsupported handoffs in the two requested intervals. In TinyCore
3M–4M the first non-prefix opcode-byte counts were: F7=5,473, 0F=745, AE=20,
AC=11, 6B=9, AB=8, A4=2, 9C=1. These count unsupported handoffs (including cached
negative entries), not cold, service or tick fallbacks. Group opcodes are not
decoded further by this observation; a byte count does not identify which F7 or
0F operation dominated.

The initial link helper searched all 2048 cache entries even for excluded
cross-page targets. The linked objdump and interval PC profile identified that
loop. Moving the page rejection before the search reduced TinyCore 1M–2M from
21.626953 to 5.076474 words/x86 and 3M–4M from 1064.674009 to 320.716248, with
identical guest/native coverage and exit histograms. The residual same-page
source/link search and other runtime costs remain untuned; Lead's next batch
should use the saved profile before adding coverage. The register workload
shows reservation-tail/event overhead despite a tiny generated loop.

## Reproduction and limits

Prepare TEMPLATE with `firmware.elf`, `guest.ram` and `config.bin` as described in
[B1_JIT.md](B1_JIT.md), then:

```sh
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out OUT
python3 r86chip/experiments/jit_cost.py --simulator SIM --template TEMPLATE --out LATER --case tinycore --start 3000000 --end 4000000
```

Scripts use read-only hard links for common guest inputs, remove per-run RAM
links after comparison, and retain reports/profiles. Workload flags 4|8 select
a versioned histogram payload in the existing BOUNDARIES buffer, mutually
exclusive with CPU boundary records; `jit_diagnostics.py` decodes it. The ISS
runner and ISA were not changed.

Full boot remains unverified; Lead selected bounded evidence and the optional
long run was stopped. No RTL, cache/burst
latency, board, Pi, Windows or Apple measurement was made. The older frame-copy
measurements and superseded documents are preserved in the local Intern batch
backup, rather than treated as evidence for this emitter.
