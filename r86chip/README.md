# r86chip

An RV32 core with private x86 extensions, built to run 32-bit x86 guests as
**translated code**, for the **Sipeed Tang Retro Console 138K** (Gowin GW5A).
The shared Rust x86 interpreter and PC devices compile to its firmware; a
translator emits native words for hot 32-bit code and the interpreter runs
everything else (real mode, 16-bit code, cold and rare instructions).

Budget: **15,000 LUT4s** for the core, its tables and cache controllers.
Memory arrays use block RAM. RAM back ends, display and peripherals are
outside that budget.

Authority, in order: the Rust ISS [`sim/rv32.rs`](sim/rv32.rs) defines every
instruction (`Core::step`); firmware and generated code follow it; Chisel
implements it. Change the ISS and its tests first. See the Guide below and
[design/ISA.md](design/ISA.md).

## Status (2026-11-11, ISA identity `0x4231000a`, uncommitted work included)

| Item | State |
| --- | --- |
| ISA | RV32IM, Zifencei, Zba, plus x86 status arithmetic, guest memory with segment and page proofs, software-filled mapping and jump tables, guest return stack, instruction budget, two register banks |
| Core | Five stages, decode-time prediction, branch target buffer; 7,998 logic LUTs with one bank, 8,374 with two (Yosys GW5A mapping) |
| Caches | Instruction and data, 64 KiB each in block RAM, 788 LUTs; one burst `RamPort` to the RAM back end |
| Logic depth | 20-21 LUT levels by mapping, with or without caches. **Nothing is placed or routed; there is no clock figure.** |
| Interpreter firmware on RTL | TinyCore, 1,000,000 x86 steps, equal to the ISS; 1.200 clocks per native word with zero-wait memory, about 92 native words per x86 instruction |
| Generated code on RTL | Hand-translated loops only: 1.85 to 2.44 clocks per x86 instruction with warm caches and a 24-clock RAM |
| Translator firmware | **Not yet written for the new words.** `jit/` is the older register-only compiler with a frame-copy ABI ([cost](experiments/JIT_COST.md)); its replacement is specified in [design/TRANSLATOR.md](design/TRANSLATOR.md) |
| RAM back ends, board top level, devices | Not started |

Open work, ideas and who does what: [`PROFESSOR_JOB_LIST.md`](../PROFESSOR_JOB_LIST.md)
(repository root, local file).

## How it runs x86

- **Two banks.** Bank 0 runs firmware: interpreter, translator, devices.
  Bank 1 runs generated code with EAX..EDI resident in x8..x15. XENTER and
  XLEAVE switch; nothing is copied.
- **Different instructions, different views.** RV loads and stores use native
  addresses. Guest words (XLD/XSW, XLDD/XSWD) take an x86 segment offset and
  check software-published segment and page proofs. A miss enters a software
  service at a precise point; hardware never walks page tables or fabricates
  an x86 fault. See [design/ADDRESS_SPACE.md](design/ADDRESS_SPACE.md).
- **One-word forms for common x86.** Load/store with displacement, PUSH,
  flag-setting ALU with immediate, INC/DEC, shifts and rotates, conditional
  branch on x86 status, indirect branch through a table keyed by guest EIP,
  CALL/RET pairing through a return stack, per-block time accounting.
- **Mappings.** 1024 data rows in block RAM with a 64-row first level read in
  the execute stage; a first-level miss is refilled by hardware in about six
  clocks. Four fetch rows serve the interpreter.
- **Memory map.** Native addresses with bit 31 clear are cached RAM; with bit
  31 set they are devices (single beat, never cached, never fetched). The
  firmware's simulation areas at `0x00800000` and `0x00900000` predate this
  and still sit in the RAM half.

## Layout

| Path | Contents |
| --- | --- |
| `sim/` | ISS (`rv32.rs`), flat-RAM fixture runner (`native.rs`), firmware harness (`machine.rs`), behavior tests |
| `rtl/src/main/scala/r86chip/` | `Core.scala` (pipeline), `XFlags.scala` (x86 status ALU and shifter), `Ea32.scala`, `MulDiv.scala`, `Tables.scala` (block-RAM tables), `Cache.scala` (caches and `System`), `Ram.scala` (`RamPort`) |
| `rtl/*.v`, `rtl/tests/` | Register file and RAM technology boundaries; `core_tb.sv` (core alone), `system_tb.sv` (core, caches, slow RAM), `machine_tb.cpp` (firmware) |
| `firmware/`, `interp/`, `jit/` | RV32IM firmware entry, the interpreter's chip primitives, the older translator |
| `experiments/` | Suites and measurements listed below; outputs under ignored `experiments/out/` |
| `design/` | Contracts and rationale. `TRANSLATOR.md` and `ISA.md` are current. `RTL.md`, `INTERPRETER.md`, `GUEST_MEMORY.md`, `TLB.md` and `experiments/RTL_NATIVE.md` describe the earlier single-stage core and 16-row table and need updating |

## Checks

From the repository root. `SIM` is the built `r86chip-sim`; `OUT` is any
scratch directory.

```sh
cargo test  --release --no-default-features --features r86chip --bin r86chip-sim
cargo build --release --no-default-features --features r86chip --bin r86chip-sim
python3 r86chip/experiments/rtl.py --gowin --out OUT --simulator SIM
python3 r86chip/experiments/xlate_rtl.py --rtl OUT --simulator SIM
python3 r86chip/experiments/xlate_rtl.py --rtl OUT --simulator SIM --bench
python3 r86chip/experiments/cache_rtl.py --out OUT/cache --simulator SIM
```

| Check | What it establishes |
| --- | --- |
| ISS tests (64) | Word semantics, including 169,696 comparisons of the translator words with `Interpreter::step` (`sim/xlate_tests.rs`) |
| `rtl.py` | Core against an independent Python oracle on bus events, retirement and traps: 215 programs, 1,720 runs (Verilator and Icarus, both bank counts); Yosys structure and GW5A mapping |
| `xlate_rtl.py` | Translator words, RTL end state against the ISS: 36 fixtures, 144 runs; `--bench` measures hand-translated x86 loops |
| `cache_rtl.py` | Core, caches and a slow burst RAM against the ISS, including RAM content after write-back: 234 images, 702 runs |
| [tinycore.py](experiments/tinycore.py) | Firmware on the ISS or the Verilator machine model; see [TINYCORE.md](experiments/TINYCORE.md) |

Tools: sbt with Chisel 7.12.0, Verilator, Icarus Verilog, Yosys with
`synth_gowin -family gw5a`, the `riscv32im-unknown-none-elf` Rust target.

## Limits

- No routed timing, no board run, no RAM back end.
- No translated x86 code has run on this core; the measured loops were
  translated by hand.
- The firmware has not been run through the caches. Its text is 830 KB
  against a 64 KiB instruction cache, so the zero-wait figure is optimistic.
- Synthesis counts are Yosys mapping results for the listed modules, not
  routed occupancy.
- 16-bit code is interpreter-only by project rule; at about 110 clocks per
  x86 instruction that is 286-class speed on this chip.

# Guide

- The Rust instruction interpreter (ISS) is the executable gold standard for
  the chip's ISA. The current model is `Core::new` and `Core::step` in
  [r86chip/sim/rv32.rs](sim/rv32.rs), with its observable-behavior tests
  in [rv32_tests.rs](sim/rv32_tests.rs). Chisel implements this model;
  prose, generated RTL and auxiliary test oracles do not define competing ISAs.
- When changing the ISA, first change the Rust ISS and its focused tests, then
  update affected firmware/emitter users and validate them on the ISS, then
  change Chisel. Compare the resulting RTL with the ISS using the same native
  binaries and architectural observations. Diagnose disagreements; do not
  change expected results merely to match RTL. Correct model bugs in the ISS
  first, with a regression case.
- Keep the ISS readable as the instruction-set documentation: explain each
  added instruction, its encoding and operands, state changes, ordering and
  fault cases alongside the implementation. Document register ownership,
  memory visibility and model-versus-hardware assumptions. Update these comments
  with every ISA change before updating the hardware.
- Other documents must link to the ISS for instruction encodings, semantics
  and architectural state instead of maintaining duplicate normative tables.
  Keep rationale, software policy, microarchitecture, measurements and build
  instructions in documents. Label proposed extensions as proposals until they
  exist in the ISS. The functional model does not establish hardware latency,
  resource use or timing closure.
- Backward compatibility is not a requirement for our private ISA, firmware
  ABI or prototype APIs. Do not preserve obsolete encodings, modes or fallback
  paths for compatibility. Update consumers and rebuild firmware/generated code
  when the model changes; historical experiment records are evidence, not
  compatibility obligations. Required RV32 and guest x86 semantics still apply.
- For the chip's accelerated interpreter, fetch and decode the original x86
  bytes on every execution. Do not introduce a decoded-instruction cache to
  meet its instruction-count target. Ordinary byte caches and software-filled
  guest translation tables are allowed; the separate JIT path remains separate.
  Target an average of at most 20 retired native instructions per completed
  interpreted x86 instruction on representative 32-bit OS workloads. Count fetch,
  decode, dispatch, execution and amortized misses/services, and report cycles
  and whole-machine costs too. Do not substitute ALU-only measurements for this
  target or hide software work inside an unmeasured long-latency instruction.
  The core budget is now 15,000 LUT4s (see the status above); 300 MHz and 100 x86
  MIPS are targets, not established timing or permission to inflate the core. The intended memory
  acceleration needs a small software-filled guest TLB; a hardware page walker
  is not required. Fix workloads, endpoints and counting rules before measuring;
  use total native / total guest counts, including misses and fallbacks. JIT
  execution cannot satisfy the interpreter target. See
  [the objective and acceptance criteria](design/INTERPRETER.md#performance-objective-and-acceptance).
- Follow **different instructions, different views**:
  the opcode selects the native or guest view, and the relevant architectural
  state qualifies the operation. Native RV loads/stores retain native meaning
  in interpreter, generated code and software services alike. Guest operations
  must match the current x86 access context; changes invalidate stale TLB proofs.
  Keep firmware/r86 runtime addresses separate from guest linear/physical
  addresses. A miss must reach software at a precise boundary, never silently
  use a guest address as a native pointer. The new interpreter defaults to one
  RF bank; banking is only a possible measured context-save optimization, not
  address-space selection, x86 mode, privilege or rollback. See the
  [design principle and its limits](design/ADDRESS_SPACE.md#different-instructions-different-views).
  XLD/XSW, published guest-context/segment proofs and a same-bank software
  service have tested ISS/Chisel implementations. The current single Rust
  interpreter uses XALU status calculation, XTLOOK translation primitives and
  qualified XLD reads and fresh instruction-byte fetches. Fetch X proofs come
  from the shared software fetch cache and follow its invalidation lifetime.
  Word/dword immediates and displacements use the ISS-defined same-page fetch
  form; the existing ModR/M decoder uses that same leaf. Cross-page or rejected
  fetches continue through the original software helper with precise EIP/faults.
  Data and fetch proofs have separate hardware capacity and explicit fill
  instructions; publication replaces only its selected view, never unions
  permissions. Shared context/window changes and software invalidation retire
  both views as defined by the ISS.
  The existing 32-bit address helper also consumes the ISS-defined bounded
  effective-address primitive. It reads fresh addressing bytes and the actual
  `Cpu.regs` native array, not a second guest frame. Rust applies segment
  overrides, performs operand accesses and commits x86 state. Rejection stays
  within that helper; POP-memory's updated-ESP case keeps its software path.
  XLD miss service returns within one inline-asm boundary
  to the same Rust memory operation. Stores retain shared write notifications;
  fused XSW remains a separately tested hardware building block.
- Keep **one x86 interpreter and one semantic implementation**. All x86 modes
  share `src/x86/interp.rs`; select chip leaf primitives at build time with
  `r86chip-native`. Do not introduce a second assembly decoder/dispatch loop,
  an accelerated-subset engine, a second live GPR/flags frame, or whole-x86-step
  fallback between interpreters. Uncommon instructions remain handlers in the
  same interpreter. A memory miss continues that operation through the existing
  walker/MMIO/fault path, without refetching/redecoding the instruction. The
  separate JIT experiment is not another interpreter.
- Keep hardware EFLAGS scratch state local to a primitive; `Cpu` owns committed
  x86 flags. Hardware translation proofs must follow software cache privilege,
  context, permission revocation and invalidation lifetimes. Do not flush/rebuild
  them after each software operation. The first integration leases both tables
  for a batch and flushes at entry; all additional work remains measured. See
  [implementation and measurements](experiments/INTERPRETER.md).
- Keep the r86chip interpreter compact. Use **256 KiB** as an initial size
  target, not a hard cap that justifies a measured execution regression.
  Select code growth by benefit: retain specialization/inlining only when
  matched full-workload results show a clear gain worth its added text; do not
  preserve every expansion or undo useful ones indiscriminately. Report native
  counts and available same-binary RTL cycles alongside bytes. Count cold paths,
  inlined helpers and reachable compiler-outlined blocks, excluding separately
  linked MMU/device code; include CPU-state/native-leaf helpers conservatively.
  Use `r86chip/experiments/native_size.py` and report main-loop and whole-firmware
  sizes separately. Renaming/moving helpers or dropping guest behavior is not
  a size optimization.
