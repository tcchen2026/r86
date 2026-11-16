# CPU loop calibration

A synthetic engine ceiling, not an OS boot benchmark. `loop.asm` enters flat
32-bit protected mode, enables 4 KiB identity paging, and keeps interrupts off.
There is no device I/O inside the measured bodies. Sector 0 loads 17 extra
sectors with ATA PIO (the pattern in `tests/bootsectors/vhd_pio.asm`); serial
output and guest page copies are outside the bodies. No firmware is required.

```sh
nasm -f bin benchmarks/cpu_loop/loop.asm -o benchmarks/cpu_loop/loop.bin
python3 benchmarks/cpu_loop/run.py --binary path/to/r86-run --output tmp/loop-result
```

The runner uses the first sector with `--bootsector`, and the complete 9 KiB
image as a frozen HDD. It records the image hash and verifies it at exit.
The boot sector's fixed header records iteration counts and block counts.
No existing VM or OS disk is used. Output directories must be empty.

| Phase | Work |
| --- | --- |
| P1 HOT | 200 million visits to an eight-instruction loop: register arithmetic, SIB load/store, compare and Jcc. |
| P2 PAGE | One million chains through one aligned 4 KiB page: 128 bodies plus two call continuations, 130 block visits per chain. Each block executes seven or eight instructions; each chain executes 915 instructions. Direct jumps, taken and not-taken Jcc, call/ret pairs, and one indirect jump stay on the page. |
| P3 FRESH | 256 previously unexecuted copies of that position-independent page, 300 chains each, with no later reuse. Wasm uses 384 chains (see below). |
| P4 COLD | A separate 256 fresh pages, exactly two chains each, below admission. This measures cold interpretation inside the JIT engine. |

P1/P2 and marker helpers warm before measurement. P3/P4 pages are copied before
any timed phase; they are not executed during warm-up. Data/stack and code are
separate. Small untimed gaps keep adjacent serial markers apart. The final
register/memory checksum and all four guest step counts must match across the
interpreter/JIT pair. `--quick` builds a short function-check variant with NASM;
its timing and derived costs are not calibration results.

## Observation and builds

No emulator source changes are needed. The native CLI buffers ordinary serial
stdout until exit. The runner therefore drives its existing local WebSocket
protocol, using only Python's standard library. The interpreter discovers
marker endpoints; their deterministic RDTSC values equal guest instruction
steps. The JIT then runs each whole measured interval to those exact steps in
one job. Short polling slices would manufacture extra entry points and extra
compilations. Interpreter marker observation has a bounded overshoot (1,024
steps for starts/P3/P4, 100,000 for P1/P2 ends); it is insignificant for the
full P2 denominator but not for quick runs. Native wall time includes one local
command round trip. Guest step counts come from the serial TSC markers, not
polling boundaries. The fixed global limit is four billion guest steps.

`--profile-binary FILE` adds one JIT-only diagnostic run and reports the P3
`compile_nanoseconds` delta divided both by actual emitted blocks and by the
benchmark's logical block instances. Profile timing is not a production timing.
The production pair uses the same ordinary binary with `--cpu-engine interp`
and `jit`. ARM64 is a native JIT host; x86_64 can run the native interpreter,
Wasm, or ARM64 under QEMU for function checks.

Build timed binaries from an untouched `git archive HEAD` export, with
`CARGO_TARGET_DIR` outside that export; delete the export after building.
For the Pi use static `aarch64-unknown-linux-musl`, `RUSTFLAGS='-C linker=rust-lld'`,
`--release --features jit --bin r86-run`; build the cross-check with
`--features jit,jit-profile`. Pin the runner to a different CPU from the engine:

```sh
taskset -c 0 python3 benchmarks/cpu_loop/run.py --binary ./r86-run-a64 \
  --profile-binary ./r86-run-a64-profile --cpu 3 --output tmp/loop-pi
python3 benchmarks/cpu_loop/run.py --binary ./r86-run-a64 \
  --prefix qemu-aarch64 --quick --output tmp/loop-qemu
```

`--reference results.json` can reuse the interpreter result/endpoints for the
identical guest and native/Wasm mode. `--engines jit` requires it on native.
A reference is a timing/endpoint record, never executable code or a VM snapshot.

## Wasm

Build the HEAD export using `script/build_wasm.py --jit --output DEST`.
`--wasm` runs the unchanged `script/wasm_cli.cjs`, using the same JIT-capable
core for both engines. `observe.mjs` is a Node preload which observes serial
markers and existing core/host JIT counters after `WasmPc.run`; it changes no
execution semantics. Timing is taken in Node, excludes start-snapshot formatting,
and uses whole-phase budgets so host slices do not create extra entries.
No Rust diagnostic/profile build is needed. Python only collects the records.

```sh
taskset -c 0 python3 benchmarks/cpu_loop/run.py --wasm \
  --wasm-module path/to/r86-jit.wasm --cpu 3 --output tmp/loop-wasm
```

The default Node flags are those in `web/README.md`:
`--no-liftoff --no-wasm-inlining --wasm-num-compilation-tasks=1
--wasm-max-committed-code-mb=96`. Use `--wasm-cli` to select the unchanged CLI
and adjacent `web/` modules retained from the same HEAD export.

Wasm's nominal threshold is 256, but current admission also requires 320
visits and 65,536 guest steps after candidacy (or 1,024 visits). Thus 300
visits would not measure fresh compilation. Wasm automatically assembles a
384-visit P3 variant with NASM; native retains 300. `--fresh-iters` overrides
this explicitly. The observed P3 interpreted-step delta reports an
instruction-weighted effective cold-visit count, rather than pretending the
nominal threshold is the actual publication point.

## Prices and limits

For `B = 130 * 256` fresh block instances and `N = 300` (Wasm: 384):

```text
c_i    = interpreter P2 time / (130 * 1,000,000)
c_n    = JIT P2 time / (130 * 1,000,000)
c_cold = JIT P4 time / (130 * 256 * 2)
C      = JIT P3 time / B - [t*c_cold + (N-t)*c_n]
break-even visits = C / (c_cold-c_n)
```

`--threshold` defaults to 3 on native, 256 on Wasm; select 16 for Thumb-2.
The requested nominal-threshold residual and an effective-cold-visit residual
are both printed. The latter weights the P3 interpreted instructions by
`915 * 256`, including tiny driver/marker costs. The native third-visit rule
normally means two preceding cold visits. The quotient is amortization visits
after paying compile cost; add admission visits for total lifetime visits.
A negative residual or nonpositive saving does not identify a physical compile
price. The script retains such evidence instead of clamping it into a win.

These are average logical-block costs, not costs for every possible x86 block.
Wasm compiles grouped functions/modules; the native compiler can emit several
entries for a logical block. `C` includes emission/publication, host compilation,
and any first-use work left in the timed interval. The profile counter covers
its documented compiler interval; it is a cross-check, not the same quantity.

The workload is ring 0 with flat segments, identity mappings, fixed branch
outcomes and a tiny working set. It excludes user/kernel transitions, faults,
SMC, code reuse after eviction, device waits and OS scheduling. P1 adds one
ordinary data load/store; P2/P3/P4 stress register operations and control flow.
Page first-touch and allocation are included in P4. Host scheduling, native
endpoint command latency, serial boundary code and Wasm observation overhead
remain; small quick-run times are especially misleading. One requested sample
per engine is calibration evidence, not a performance acceptance distribution.
Do not turn these results into an OS threshold change or an Apple speed claim.
