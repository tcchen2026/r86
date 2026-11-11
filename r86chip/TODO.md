# r86chip: handoff and plan

Written 2026-11-11 by the Lead for whoever continues the chip work. It says
what exists, what to do next and in which order, and what to do when a
result goes one way or the other. Follow the designs given here; where a
step says "decide by measurement", the rule for deciding is given too.

Read first: [README.md](README.md) (state, layout, checks),
[design/TRANSLATOR.md](design/TRANSLATOR.md) (how firmware uses the words),
the "Translator words" section of [sim/rv32.rs](sim/rv32.rs) (what the words
mean), and the header comments of `rtl/src/main/scala/r86chip/Core.scala`,
`Front.scala` and `Cache.scala`. `/work/r86/AGENTS.md` holds the project
rules; the ones that bite here are repeated in section 2.

## 1. Where things stand

Commits on `main`: `22937ff` (six-stage core with caches in the pipeline),
`d46ed01` (translator firmware, the Intern's handoff 2) and the commit after
it (handoff 3: indexed link lookup).

| Thing | State |
| --- | --- |
| ISA | Identity `0x4231000a`. Defined by `sim/rv32.rs`. Unchanged by the new core |
| Core | Six stages F D A M X W. `Core.scala` (A, M, X, W), `Front.scala` (F, D), `Fields.scala` (word classification) |
| Caches | Two-way, 32-byte lines, in the pipeline. `Cache.scala`, `CacheStore.scala`, `CacheSequencer.scala`, `WordAccess.scala`. Top level `System.scala` (`r86_system`) |
| RTL check | `experiments/cache_rtl.py`: 236 images, 708 runs equal the ISS |
| Translator | `jit/compile.rs`, `jit/runtime.rs`, `jit/native.rs`. Equal to the host interpreter over 20,000,000 TinyCore steps on the ISS |
| Intern | tmux session `r86`, working on the task in section B of this file; it writes its handoff into section 7. The user resets its context, never you |
| Not done | Place and route, RAM back ends, board top level, devices, trap vector |

Baselines to compare against. TinyCore, 24-clock RAM, caches 64 KiB + 64 KiB
(`GenerateSystem B 10 10`). "Second million" is the 2,000,000-step run minus
the 1,000,000-step run.

| Run | x86 steps | Clocks | Native words | RAM FNV-64 | Guest RAM FNV-64 |
| --- | ---: | ---: | ---: | --- | --- |
| Interpreter firmware, 1 bank | 1,000,000 | 157,510,563 | 94,337,643 | `71a9d64922a1b746` | `677cabc8a9f5f647` |
| Interpreter firmware, 1 bank | 2,000,000 | 247,725,242 | 171,886,646 | `939c2cf38b8e142e` | `a6ee6e3be2f7af8d` |
| Translator firmware `73c7c82e`, 2 banks | 1,000,000 | 85,845,593 | 24,109,098 | `63ca4e028b71a9ea` | `677cabc8a9f5f647` |
| Translator firmware `73c7c82e`, 2 banks | 2,000,000 | 93,720,688 | 29,185,572 | `e4e1027e9a50dbc2` | `a6ee6e3be2f7af8d` |

Second million: interpreter 1.163 clocks per word, 90.2 clocks per x86
instruction; translator 5.08 words and 7.9 clocks per x86 instruction (7.0
with `GenerateSystem 2 12 11`). The task-3 firmware (`046cd834`, committed)
also equals the ISS at 1M to 4M steps: 7.4 clocks per x86 instruction in
1M-2M, 6.0 in 2M-3M, 30.2 in 3M-4M (11.78 words; 346,719 line fills, 243,927
of them data lines in the firmware region below `0x02000000`). Mapping: 9,463 logic LUTs, 17 LUT levels.
The RAM hashes and word counts must not change unless firmware changes; the
clock counts are what RTL work is allowed to change.

Budget: 20,000 logic LUTs for core plus caches (the user will raise it on
request). Block RAM is plentiful (82 of about 340 cells used).

## 2. Rules

- **ISA change order:** `sim/rv32.rs` and ISS tests first, then every
  consumer (translator, fixtures), then Chisel. Bump the identity word
  (`0x4231000a`, in `rv32.rs`, `Core.scala` and the experiments that check
  it) with every ISA change.
- **16-bit code is interpreter-only.** Do not add 16-bit translation.
- **Roles:** you are the Lead. The Intern edits in the main tree and never
  changes Git state; the Lead commits. Tasks and handoffs live in this file
  (sections B and 7), not under `tmp/`: the Intern leaves its edits in the
  working tree, lists the files it changed and writes its results into
  section 7. No patch files. Only bulky generated evidence (logs, RAM
  images, profiles) goes under `tmp/r86chip/`, named in the handoff. Only the Intern
  touches `ssh rpi5`. Steer the Intern with one task file at a time; do not
  queue. Do not send it `/new`.
- **Commit only when the user asks.** `*.md` is ignored by Git; documents
  that must be tracked need `git add -f`.
- **Scratch goes under `/work/r86/tmp/`**, never `/tmp`. Build with
  `CARGO_TARGET_DIR=tmp/target-professor` (the Intern uses
  `tmp/target-intern`). Run `sbt` with `env -u CHISEL_FIRTOOL_PATH`. Do not
  run two `sbt` at once.
- **Do not over-test.** Run a check when it can change a decision; do not
  repeat a passing check without a relevant change.
- **Wording:** "magic instruction", never "backdoor". In code and comments
  prefer neutral words (replace, discard, changed) to violent ones.
- **Report honestly.** A failing or skipped check is stated with its output.

## 3. Checks and how to run them

```sh
cd /work/r86
export CARGO_TARGET_DIR=tmp/target-professor
cargo test  --release --no-default-features --features r86chip --bin r86chip-sim
cargo build --release --no-default-features --features r86chip --bin r86chip-sim
SIM=$PWD/tmp/target-professor/release/r86chip-sim

# RTL against the ISS: fixtures, hand-translated benchmarks, rtl.py programs.
cd r86chip/experiments
env -u CHISEL_FIRTOOL_PATH python3 cache_rtl.py --out /work/r86/tmp/OUT --simulator $SIM
```

Firmware on the RTL has no wrapper in the repository yet (task A2). By hand:

```sh
# 1. Image directory: firmware.elf/.bin, guest.ram, config.bin.
#    Translator: add --jit and use the translator's firmware ELF.
python3 - <<'EOF'
import sys; sys.path.insert(0, 'r86chip/experiments'); import tinycore
from pathlib import Path
elf = Path('FIRMWARE.elf'); out = Path('tmp/OUT/img')
out.mkdir(parents=True, exist_ok=True)
(out / 'firmware.elf').write_bytes(elf.read_bytes())
(out / 'firmware.bin').write_bytes(tinycore.flatten_elf(elf))
EOF
HOST_R86CHIP_MACHINE refs/TinyCore-current.iso $PWD/tmp/OUT/img 2000000 [--jit]
# 2. ISS reference: writes img/iss-run.json (ram_fnv64, retired_native).
$SIM machine tmp/OUT/img/firmware.elf tmp/OUT/img 100000000000 600
# 3. RTL. BANKS is 1 for the interpreter firmware, 2 for the translator.
cd r86chip/rtl
env -u CHISEL_FIRTOOL_PATH sbt -batch "runMain r86chip.GenerateSystem 2 10 10 /work/r86/tmp/OUT/sys"
verilator --cc --exe --build --top-module r86_system -O3 -CFLAGS "-O3 -std=c++17" -j 4 \
  --Mdir /work/r86/tmp/OUT/sys/obj /work/r86/tmp/OUT/sys/*.sv r86_ram.v tests/system_machine_tb.cpp
/work/r86/tmp/OUT/sys/obj/Vr86_system /work/r86/tmp/OUT/img 100000000000 24   # last argument: RAM latency
```

The RTL run prints one JSON line: `cycles`, `retired_native`, `ram_fnv64`,
`guest_ram_fnv64`, line fills by region, and `events` (sixteen counters; the
list is in the comment above `io.events` at the end of `Core.scala`). It
passes when `ram_fnv64` and `retired_native` equal the ISS's. A 2,000,000-step
run takes two to four minutes. How firmware ELFs are built is in
[experiments/tinycore.py](experiments/tinycore.py) and
[experiments/B1_JIT.md](experiments/B1_JIT.md).

Size and depth: Yosys `synth_gowin` on the generated Verilog, then a longest
path search over the JSON netlist. The scripts used so far are local scratch:
`tmp/review/g2/depth.sh`, `tmp/review/rtl/depth_sys.py` and `path.py`
(`path.py NETLIST.json core.REGISTER D` prints one path), with support files
in `tmp/review/rtl-t/`. If they are gone, `rtl.py`'s `synthesize()` has the
same mapping flow.

What to run after which change:

| Change | Run |
| --- | --- |
| Any RTL edit | `cache_rtl.py` |
| Pipeline, cache or forwarding edit | also both firmware baselines at 2,000,000 steps; four-state Icarus on the `sys3` build for at least the first 80 images (`iverilog -g2012 -s system_tb`, compare `T`, `M`, `C` lines with the ISS) |
| ISS word semantics | ISS tests, then `cache_rtl.py` after the Chisel follows |
| Translator | the Intern's gates in `B1_JIT.md`, then the translator baseline on the RTL |

## 4. Invariants of the core

Break none of these without redesigning what depends on it.

1. Nothing before X changes architectural state. X is never undone.
2. Whether a word leaves A or M never depends on a cache or table
   comparison made in that clock. Such answers are registered into X, and X
   replays the word (`xReplay`, `xRefill`) when they are bad.
3. A word that changes anything A or M reads (segments, context, data or
   fetch mappings, jump table, register bank) sets `forced`, so fetch
   restarts behind it. Status, the budget and the return stack are read only
   in X or in M under the wait in rule 6 and need no restart.
4. Block RAMs read for the word in M (data cache, `l2`, `jt`) are held at
   M's own address while M waits (`hold`).
5. A register write that is only decided at commit (XEA32) keeps younger
   words in A until it has left X. Every other word's `Fields.writes` is
   exact for a word that commits normally.
6. An XJT waits in A while a word that changes the return stack is in M or X.
7. `xQuick` (forwarding a load from X without waiting) is only for words
   whose register value is the loaded value itself.
8. A store commits into a line its own M-stage read found present; the way
   travels with it. A load replays if it shares a word address with a store
   in X, the store being written, or the one written on the previous edge.
9. The cache sequencer starts a job only when `quiet` (no word in A, M, X
   unless X itself waits on the word-access unit, and no store pending).
10. Every prediction is a hint. X checks direction and target; a table row
    found on a word that is not a branch is removed (`stray`).
11. Uninitialized storage is never trusted: `x0` reads through a zero flag,
    tables through epochs or valid bits.
12. Only the word in X may read a device, through `WordAccess`.

## 5. Work, in order

Do A before anything else. B runs in parallel with whatever you do. C needs
the Intern's task 3 numbers for its second half. D and E are chosen by the
rules given with them.

### A. Make the repository whole (mechanical)

- [ ] **A1. Old runners.** `rtl.py`, `xlate_rtl.py` and
      `tinycore.py --engine rtl` build the earlier core and fail now.
      - Keep in `rtl.py`: the program generators, `cases()`, `reference()`,
        `compare_iss()`, `synthesize()`. Replace its RTL run with
        `tests/system_tb.sv` on `r86_system`, comparing the retirement
        stream (`R` lines) and trap (`T`) with `reference()` and the final
        RAM (`M`) with the ISS. Bus-event comparison goes away: the core has
        no bus.
      - The four `fetch_fault*` programs inject an error at one PC through
        the old testbench. Drop them; `fetch_unmapped` covers a fetch error
        through the caches. No program fetches from a device address (bit
        31 set): add one, expecting cause 1 at that PC.
      - `xea_native_fault_1` uses a RAM of 65,540 bytes. With 32-byte lines
        the last four bytes are not backed. Change its RAM size to a whole
        number of lines and its expected value accordingly, or drop it.
      - `xlate_rtl.py`: keep `cases()` and `benchmarks()`; point `--rtl`
        runs at `cache_rtl.run()` with zero-latency RAM.
      - `tinycore.py --engine rtl`: build `GenerateSystem` and
        `tests/system_machine_tb.cpp` as in section 3.
      - Then delete `rtl/tests/core_tb.sv`, `machine_tb.cpp`,
        `regfile_tb.sv`, `rtl/r86_regfile.v`, `RegFile.scala`, and
        `RamBridge`/`GenerateBridge` in `Ram.scala`.
      - Done when the commands in the README's Checks section all pass and
        the README rows that say "does not run" are corrected.
- [ ] **A2. Firmware-on-RTL wrapper**, `experiments/firmware_rtl.py`:
      arguments firmware ELF, x86 steps (one or two values), banks, cache
      set bits, RAM latency. It prepares the image, runs the ISS and the
      RTL, asserts equal `ram_fnv64` and `retired_native`, and prints per
      interval: words and clocks per x86 instruction, clocks per word, line
      fills by region, and each event counter per word.
- [ ] **A3. Stale documents.** `design/RTL.md`, `INTERPRETER.md`,
      `GUEST_MEMORY.md`, `TLB.md` and `experiments/RTL_NATIVE.md` describe
      the first core. Rewrite `RTL.md` for the six-stage core (stages, the
      invariants of section 4, replay causes, prediction, cache protocol,
      costs in clocks). In the others fix table sizes and timing statements
      or delete what the README and `RTL.md` now cover.
- [ ] **A4. Read commit `d46ed01` line by line.** It was accepted on
      evidence. Check: the precision rule of `TRANSLATOR.md` in every
      emission path of `jit/compile.rs`; that nothing in `src/` changes
      behaviour for other hosts; that the runtime never retries a faulting
      block without progress. Write findings to the Intern as the next task;
      do not fix its code yourself.

### B. Intern: translator runtime

**Current Intern task (task 4, ISS only), given 2026-11-11.** Evidence it
starts from: the task-3 firmware on the RTL (two banks, 64 KiB + 64 KiB
caches, 24-clock RAM), equal to the ISS at every endpoint.

| Interval | Words per x86 | Clocks per x86 | Line fills | Firmware region / heap / guest |
| --- | ---: | ---: | ---: | --- |
| 1M-2M | 4.66 | 7.36 | 39,464 | 29,231 / 7,312 / 2,921 |
| 2M-3M | 4.03 | 5.95 | 24,315 | 16,075 / 5,450 / 2,790 |
| 3M-4M | 11.78 | 30.17 | 346,719 | 243,927 / 99,201 / 3,591 |

"Firmware region" is native addresses below `0x02000000` (image, .bss,
static tables). A cold line costs about 45 clocks. A 256 KiB instruction
cache barely helps 3M-4M (27.4 clocks), so the fills are data lines: the
runtime walks tables that do not fit a 64 KiB data cache.

1. Give the ISS a cache estimate, statistics only: two-way, 32-byte lines,
   64 KiB instruction and 64 KiB data, counting line fills for fetches and
   for data accesses in the three regions. It must not change execution,
   retired counts or any existing output field; add fields to the run
   report. "Estimated clocks" = 1.15 * native words + 45 * line fills. Check
   it against the table above (within about 15%). A hook in `sim/rv32.rs`
   must be a counter call only.
2. Remove the remaining linear scans from the runtime: service site lookup,
   unlink and restoration scans, anything that visits more than a few
   entries per event.
3. Make every hot lookup touch at most two data cache lines: compact key
   arrays separate from payloads, packed entries. Report each table's size
   in bytes and lines touched per lookup.
4. Measure 1M-2M, 3M-4M and 4M-10M: words and estimated clocks per x86
   instruction, line fills by region and side, and the tables of handoff 3.
   Target: 3M-4M at or below 15 estimated clocks per x86 instruction.

No opcode coverage in this task. No change to word semantics or any RTL
file.

Handoff 3 showed that native words no longer predict cost: line fills do.
Judge the runtime by clocks on the RTL (or the ISS cache estimate task 4
adds: 1.15 clocks per word plus 45 per line fill).

When the Intern's handoff for the current task appears in section 7:

1. Check its evidence (endpoint comparisons pass; cost table present; the
   ISS cache estimate changes no existing output). Run its firmware on the
   RTL for 1M to 4M steps and compare line fills with its estimate.
2. Decide:
   - **3M-4M above 15 clocks per x86 instruction on the RTL:** the runtime
     is still the cost. Next task: the table or function with the most line
     fills in its report, nothing else.
   - **At or below 15:** next task is cross-page links (direct links to
     blocks on another guest code page, invalidated with that page), then
     the cost table for 4M-10M and for a Windows 2000 boot interval on the
     ISS.
3. Opcode coverage rule: add a translation for an x86 form when its
   interpreter handoffs exceed 1 per 1,000 x86 instructions in any measured
   interval. Below that, leave it to the interpreter.
4. A new hardware word is justified when the Intern's counts show it removes
   at least 3% of native words in an interval, or it is one of C1/C2 below.

Milestones for the translator on the RTL, TinyCore steps 1M-10M: 20 clocks
per x86 instruction, then 10. Then the same on a Windows 2000 boot interval.

### C. New words (ISA change; ISS first)

The pipeline order (address, memory, execute) makes a memory operand free.
Add these two; identity becomes `0x4231000b`.

**C1. XALUM, load and operate.** `rd = rd OP dword[seg: rs1 + imm12]`, x86
status committed.

- Encoding: I-type. Opcode `0x4b` through DS, `0x4f` through SS. funct3 is
  the operation, as XALUI: 0 ADD, 1 OR, 2 ADC, 3 SBB, 4 AND, 5 SUB, 6 XOR,
  7 CMP (status only, no register write).
- Meaning: exactly XLDD dword (funct3 2) into a temporary followed by
  XALU.C with `rd` as left operand and the temporary as right operand. If
  the access requests service, nothing changes and the service record names
  this word.
- Chisel: in `Fields.scala` the word is `guestMemory`, a load, not `store`,
  width 2, `guestSpan`, displacement `immI`, segment by opcode, `needA`,
  `writes` unless funct3 is 7, and **excluded from `xQuick`**. Its second
  register operand is `rd`: in `Core.scala` the issue logic reads `H.rd` on
  the B port for this word (`headRB`). In X: `XFlags.alu(b, load, 0,
  control, flags)` (left operand is the register, now on the B side), unit
  0, status written and committed. In M the unit and control registers are
  set as for XALUI with the right operand taken from the load.
- Translator: `op r,[m]` becomes one word (two today), `cmp r,[m]` too.

**C2. XRMW, operate on memory.** `dword[seg: rs1 + imm12] = dword[...] OP
rs2`, x86 status committed.

- Encoding: S-type. Opcode `0x43` through DS, `0x53` through SS. funct3 as
  C1; 7 is CMP: status only, no store, read permission only.
- Meaning: XLDD with write intent (funct3 3) into a temporary, XALU.C with
  the temporary as left operand and `rs2` as right, then XSWD of the result;
  for CMP only the first two with a plain load. One word: on service
  nothing changes.
- Chisel: `guestMemory`, `store` unless CMP, write-intent rights
  (`guestRmw`), width 2, `guestSpan`, displacement `immS`, `needA`, no
  register write. M reads the line as for a load; the `passed` rule applies
  (it is a load too). X: `XFlags.alu(load, b, ...)`; the stored data is the
  ALU result instead of `b`. A spanning access goes through the word-access
  sequence: read low, read high, then write low and high with the result;
  `slowValue` must be formed after the second read for stores as well.
- Translator: `op [m],r` becomes one word (three today), `cmp [m],r` one.

Order for each: ISS arm and tests against `Interpreter::step` in the style
of `sim/xlate_tests.rs`; fixtures in `xlate_rtl.py` `cases()` (hit, each
service reason, spanning access, end of RAM, status for every operation,
word right after a store to the same address); translator emission (task for
the Intern); Chisel; `cache_rtl.py`; both firmware baselines.

**C3 and later, only with counts from the Intern** (rule B4):

| Word | Add when | Design note |
| --- | --- | --- |
| XRMWI: `op [m], imm5` and INC/DEC `[m]` | `op [m],imm` with imm in -16..15 above 1 per 100 x86 instructions | S-type with the rs2 field as a signed immediate; opcodes `0x57` (DS), `0x2f` (SS). These are the last two free major opcodes |
| Compare and branch | CMP/TEST followed by Jcc above 8 per 100 | Needs sixteen conditions and two registers; does not fit B-type with status commit. Design first, on paper, and show the encoding to the user |
| One-word FS load | `fs:[disp]` loads above 1 per 100 | XLDD with a third segment; needs an opcode, so weigh against XRMWI |
| POP in one word, ESP as a special register | PUSH/POP/CALL/RET above 20 per 100 and POP's second word visible in the count | Two register results from one word: add a second write port value in W for `rs1`; do not add a third register-file read port |

Do not add: byte and word forms of C1/C2 before their dword forms are
measured; 16-bit address forms; speculation hardware; a second issue slot.

### D. Clocks per word (no ISA change)

Measure first with the event counters on the translator firmware (1M-2M and
the later intervals once task 3 lands), then apply the rule. Current values
per native word, translator, second million: wrong conditional prediction
0.0139, operand wait in A 0.0143, cold-line replay 0.0026, restarts behind
forced words 0.0018, line fills 0.009.

| Item | Do it when | Design |
| --- | --- | --- |
| D1. Better direction prediction | conditional mispredictions above 0.01 per word (true now) | Keep the table row for the target. Add a pattern table: `EpochTable(clock, 12, 2)` in `Front.scala`, index `pc(13,2) XOR history(11,0)`, history = last twelve conditional outcomes, shifted at commit in X and sent to `Front`. A conditional row is predicted taken when the pattern counter's top bit is set. Carry the index with the word (as `counter` is carried) and update that counter at commit. Rows are still created only when a branch is first taken |
| D2. XJcc resolved in A | after D1, if still above 0.008 | In A an XJcc may be resolved like an RV branch when no word in M or X writes status. Add `Fields.writesStatus` (XALU without funct7 bit 6, XALUI, XSH, XFLOAD/commit words, guest stores that commit status, C1/C2) and use `guestFlags` directly |
| D3. Larger instruction cache | firmware-region line fills above 0.003 per word after task 3 | Change the default to `GenerateSystem B 12 11` (256 KiB instruction, 128 KiB data). No design change |
| D4. Buffer bypass | A empty in more than 8% of clocks with no fill in progress | When the fetch buffer is empty and A is free, the word arriving in F1 goes straight to A. It skips decode-time prediction, so only do it for words the table predicted or that are not JAL/XJT.R (check `i_word` opcode bits only) |
| D5. Cache block zero | a measured workload spends above 3% of clocks filling lines it then overwrites | Standard `cbo.zero` encoding (opcode `0x0f`, funct3 2, imm 4): the 32-byte line of native address rs1 becomes present, zero and changed without a RAM read. ISA change (rule order applies). Sequencer job; the word is `forced`. Start-up heap zeroing alone does not justify it: it is a one-time 25 million clocks |

Stop D when the translator's clocks per word is below 1.15 excluding line
fills, or when an item changes the baseline by less than 0.5%.

### E. Clock period

Depth is 17 LUT levels on many paths. Without a routed report this is a
guess at the real limit, so first ask the user which tool is available
(Gowin EDA is the vendor flow; nothing is installed here). With a report,
cut what it names. Without one, these are the known 16-17 level paths, in
the order I would cut them, each ending in a register:

1. **Status bits a clock late.** `XFlags.alu` to `guestFlags`. Keep CF, AF,
   OF from the adder in X; derive ZF, SF, PF in the next clock from `wValue`
   and the lane (keep a "status follows W" flag and lane register; the
   reader in X uses the derived bits when the flag is set). Changes no
   word's meaning.
2. **XEA32's code check.** `Ea32.scala`: register `codeOk` and the address
   one clock before `request`; move the request register from `Core.scala`
   into `Ea32`. Same clocks per beat.
3. **Small-ALU controls decided in D.** `aWord` to `mResult`: register
   `earlyKind`, the B-operand select and the constant select at issue, as
   `aSrcA` is.
4. **Word-only decode in D.** `staticDecode` runs in M and ends in
   `xFaultValue`; compute its word-only part on the buffer head and carry
   one bit. The part that needs rs2 (segment named by a register) stays in M.
5. **System-word effects.** `xB` to `l1Valid`: register `allowed` in the
   first X clock of a system word (they already take two).
6. **Fetch address.** Buffer head to `npc`/`aBase`: register the decode-time
   prediction (one more clock for a JAL the table does not know).

After each: `cache_rtl.py`, both baselines, depth. Keep a cut only if depth
on its path falls and clocks per word rises by less than 0.5%.

### F. Toward a board (ask the user for priority before starting)

- Trap vector: a native fault enters firmware with cause, PC and value
  readable, instead of stopping the core.
- External event input: makes the next XTICK leave bank 1, so a device
  interrupt does not wait for the budget.
- The event counters as device registers.
- Simulation control and output areas (`0x00800000`, `0x00900000`) moved to
  the device half (bit 31), so serial output is visible while running.
- A RAM back end behind `RamPort` (`Ram.scala`): block-RAM-only first, for
  first light without external RAM; then HyperRAM/PSRAM/DDR.

## 6. If something goes wrong

- **RAM hash differs from the ISS.** Reduce: find the smallest step count
  that differs (the firmware stops at any step count), then compare
  retirement streams. `system_tb.sv` prints `R` lines; the ISS's
  `native` mode gives end state only, so use `rtl.py`'s `reference()` for a
  native fixture or bisect on steps for firmware.
- **Simulation never ends.** Look for a word waiting in A (`aWait`) with
  nothing moving ahead of it, or the cache `busy` with the core not `quiet`.
  The event counters show which.
- **A check passes on Verilator and fails on Icarus.** Uninitialized state
  is being read; see invariant 11.
- **A response is stopped by a safety classifier while writing code.** It
  happened once on a single large file. Write smaller modules and neutral
  identifiers, tell the user, and continue.
- **A decision is not covered here.** Prefer the smaller hardware, keep the
  invariants, measure before and after, and write what you chose and why at
  the end of this file.

## 7. Intern handoffs

The Intern appends here, newest first: date, task, what changed (files),
results with the tables the task asked for, what was run and passed, what is
unverified, and where bulky evidence lies under `tmp/r86chip/`. Keep each
handoff short; long tables belong in
[experiments/JIT_COST.md](experiments/JIT_COST.md). Earlier handoffs, from
before this section existed: `tmp/r86chip/handoff2.txt` (commit `d46ed01`),
`tmp/r86chip/handoff3.txt` (commit `dbba770`).
