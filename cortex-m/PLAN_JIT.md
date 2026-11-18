# Thumb-2 JIT: make it pay on Windows 95

Lead plan for the Intern, 2026-11-15. Target: Windows 95 on the SF32LB58
([SF32LB58.md](SF32LB58.md)). Today the Thumb-2 JIT is 7% slower than the
interpreter on that workload. This plan removes the measured causes in order,
one measured act at a time. Evidence: `REPORT.md` tasks V and W (local).

## Status (Lead, 2026-11-15)

| Act | Change | W95 cost | Share of interpreter (17540) |
|---|---|---|---|
| 0 | baseline | 18844 | 1.074 |
| 1 | LOOP, LOOPE, LOOPNE | 17236 | 0.983 |
| 2 | PUSHAD, POPAD | 14512 | 0.827 |
| 3 | PUSHFD, POPFD, CLI, STI; quiet control exits continue | 11767 | 0.671 |
| 4.2 | XCHG16, PUSH of segment registers, RCL/RCR by 1, LODS/STOS | 11106 | 0.633 |
| 4.3 | checked range path for DS/ES/SS (expand-down included) | 10267 | 0.585 |
| 5.6 | quiet control exits stay in native code | 9483 | 0.541 |

All rows are reviewed and kept. End leaf: "JIT stays". Rejected
under the keep rule: 5.4 dispatch table first (-0.25%; 24% of lookups hit),
5.7 `HOT` 8 (-0.33%) and `HOT` 4 (-0.19%, four arena clears). Skipped by Lead
decision: 5.2, 5.3, 5.5 and the `HEAT` pilot (their parts are too small after
5.6). The arena stays 2 MiB (two clears on W95 since 4.3).

State after 5.6 on W95 (23.7 G Arm instructions): 67.9% of steps native,
2.63 M invocations, 8.6 M interpreted 32-bit steps left. Cost by boot phase
(Arm instructions per x86 step, `REPORT.md` "Plan J, scene 4.3"):

| Steps (M) | Guest phase | Cost | Where the time goes |
|---|---|---|---|
| 0-50 | BIOS and DOS, real mode | 178-196 | interpreter 82-89% |
| 50-75 | Windows loader, half real mode | 127 | interpreter 70% (`movs` 15%) |
| 75-125 | Windows start-up, 18-34% 16-bit protected | 135-180 | interpreter 45-58%, JIT runtime 18%, `jit_code_stamp` up to 9% |
| 125-150 | start-up tail, 32-bit | 44 | translated 31%, interpreter 32% |
| 150-250 | idle desktop, 32-bit | 22 | translated 67% |

The first 125 M steps hold 86% of the boot's cost. The ring-3 data selector
of Windows 95 is `013f`: base 0, expand-down, limit `ce60`, big. Scene 4.3
covers its memory operands; stack operations on it still go to the
interpreter (`STACK_FACT`), which the boot hardly exercises and application
code will.

Benchmark W95A (F8) is the application workload. After step 250 M: JIT
**75.30** Arm instructions per x86 step, interpreter **176.87** (`REPORT.md`
"Task X"). 16-bit protected-mode code is 19% of those steps and about 55% of
the JIT's time. See the
[measurements that led to T16](#measurements-that-led-to-t16).

Scene K1 (task Y, `tmp/t32/task-y.txt`; `REPORT.md` "Task Y"): kept.
Stack operations under a checked 32-bit SS: new block fact
`STACK32_FACT` (SS default size 32), and one stack helper that uses the
range check of scene 4.3 when `STACK_FACT` is missing. Same-tree comparison
(both sides built from one state of the shared files):

| Workload | Without K1 | With K1 | Change |
|---|---|---|---|
| W95A after step 250 M | 75.59 | 73.49 | -2.8% |
| W95A boot | 95.32 | 94.17 | -1.2% |
| TinyCore | 68.85 | 69.41 | +0.8% (accepted by the Lead) |

Interpreted 32-bit entries on W95A: 3.01 M to 1.68 M; POP FS and POP GS
(1.03 M) are the largest left. The W95A run now clears the 2 MiB arena three
times. Gate rule learned here: a baseline counts only while
`src/x86/interp.rs`, `src/x86/cpu.rs` and `src/machine.rs` are unchanged;
another session's interpreter commits moved W95A from 75.30 to 75.59 and
TinyCore from 68.63 to 68.85 in between.

32-bit work stops here (Lead, 2026-11-15) unless the user decides otherwise.
What is left is small against the 16-bit share: under 10% of W95A's cost
after boot in total, by estimate. Candidates, none planned:
- POP FS/GS and MOV to a segment register through a helper call (1.1 M
  entries; item 4 of the 16-bit outline needs the same helper).
- Dispatch tables that survive a change of facts or CS base (62,915 table
  clears; 1.47 M `dispatch_resolved` returns).
- `Engine::facts` once per iteration instead of up to three times (probable
  cause of K1's TinyCore cost).
- 8-bit XCHG (89 k entries), MOV from a segment register (40 k), string
  operations under checked segments.
- Generated code: direct block links, a shorter TLB check, flags.
  Translated code is 15% of the time at about 14 Arm instructions per x86
  instruction.
- Frame build and commit is 1.1% of the time; the rest of
  `Engine::run_batch` (4.3%) has no single hot spot (task Y part 3).
Ceiling estimates given to the user: pure 32-bit code about 14 Arm
instructions per x86 instruction (22 today at the idle desktop), 32-bit code
in application use about 20 (about 39 today).

## Facts (measured or read in the tree)

- F1 Benchmark **W95**: micro95 boot, 250,000,000 steps, QEMU M33 PC mode,
  `cortex-m/firmware/pc.sh` (commands: `tmp/cortex-m/win95/commands.txt`).
  Cost is `per_x86_step_x100` of the `cost:` line. Baseline: JIT **18844**,
  interpreter **17540**. JIT frame hash `27414e6e...5493`, JIT guest RAM hash
  `7988aeead145281e`.
- F2 Of 38.57 M engine iterations (`Engine::run_batch`), 18.59 M interpret one
  instruction whose block entry is unsupported and 15.27 M enter native code;
  14.78 M of 16.34 M native returns end at such an entry. Entries by opcode:
  `e2` LOOP 3.46 M; `fa` CLI 2.37 M; `9d` POPFD 1.67 M; `9c` PUSHFD 1.66 M;
  `fb` STI 1.65 M; `61` POPAD 1.20 M; `60` PUSHAD 1.18 M; MOV `8a/8b/89/88`
  1.98 M; `66 87` 0.47 M; `0f a0/a1/a8/a9` 0.96 M; `d1 /2` 0.24 M; `ac` 0.13 M.
- F3 Bookkeeping costs about 450 Arm instructions per engine iteration
  (engine loop 22.6% + JIT runtime 14.1% of 47.1 G). Translated code costs
  about 16 per x86 instruction. Short event limits are not the cause (93% of
  invocations have a limit of 256 or more).
- F4 The shared decoder already produces `Op::PushFlags`, `PopFlags`,
  `ClearInterrupt`, `SetInterrupt` (`src/x86/jit/decode.rs`, opcodes 9c, 9d,
  fa, fb) and `PushSegment`/`PopSegment`; the Thumb-2 lowering rejects them
  (`Lower::supported` in `src/x86/jit/t32/lower.rs`, the `_ => false` arm).
  The decoder has no LOOP (e0-e2) and no PUSHAD/POPAD (60, 61). JECXZ is
  `BranchCondition::EcxZero`.
- F5 The arm64 backend is the reference for the flag instructions:
  `src/x86/jit/compile.rs` arms `Op::PushFlags`, `Op::PopFlags`,
  `Op::ClearInterrupt`, `Op::SetInterrupt`; `Frame.popf_mask` from
  `interp::popfd_change_mask` (`src/x86/jit/frame.rs`); exit kinds 5 and 6 in
  `src/x86/jit/runtime.rs` (search `exit_kind, 5 | 6`), which use
  `Cpu::inhibit_maskable_interrupts` and
  `interp::continue_after_interrupt_shadow`. `Cpu::jit_commit_flags` already
  records an IF 0->1 change.
- F6 Test commands for this backend: `design/VALIDATION.md`, "Optional Thumb-2
  (Cortex-M) JIT checks".
- F7 At the baseline the 2 MiB code arena holds 1,649 KiB and was cleared once.
- F8 Benchmark **W95A**: W95 continued to 650,000,000 steps with scripted
  keyboard input (Start menu, Minesweeper, Solitaire, SkiFree, My Computer).
  Script: `cortex-m/firmware/w95a.keys`, one event per line at a fixed step
  count. Run with `PC_KEYS=<script>` and `PC_LIMIT=650000000` through
  `pc.sh` (commands: `tmp/cortex-m/w95a/commands.txt`). The host replays the
  script with the WebSocket command `send-scancodes`, the firmware from
  semihosting `keys.txt` (config word 7 bit 32), both at exact step counts.
  Cost is counted after step 250,000,000. Final frames of host, JIT and
  interpreter are equal. JIT guest RAM hash `4c79ae7617ab9f59`; the
  interpreter's differs (`2a60658255405339`, not diagnosed).

## Assumptions, each tested where named

- A1 Lowering an instruction of F2 removes its iterations instead of exposing
  another unsupported instruction right behind it. Tested by the counters of
  each act.
- A2 Interrupts are usually quiet when STI or POPFD raises IF, so few of these
  return to the machine loop. Tested in scene 3.4.
- A3 The MOV entries are rejected for a missing flat-segment fact. Tested in
  scene 4.1.

## Rules for every act

1. Order: acts 0 to 3 without waiting, then stop for Lead review (done).
   Acts 4 and 5 without waiting, then stop for Lead review. Act 6 only on a
   Lead decision. Never start act 7.
2. The interpreter is the oracle. A new native instruction either does exactly
   what the interpreter does or leaves before the instruction (the `miss`
   path). No retries, no approximate flags, no skipped checks.
3. Gate set G, once per act, in this order; stop at the first failure:
   - G1 Thumb-2 tests of F6, including the act's new differential tests
     (pattern: the `arm_jit_*` tests in `src/x86/jit/t32/tests.rs`).
   - G2 W95 with the JIT: PASS (frame equals the host frame), guest RAM hash
     still `7988aeead145281e`, cost recorded. A changed RAM hash means some
     instruction now produces a different result: find the first differing
     step count against the previous ELF before doing anything else.
   - G3 DOS gate (`pc.sh jit`, task U) PASS.
   - G4 TinyCore (`run.sh jit`) PASS with the host's RAM hash; cost recorded.
   - G5 Host `cargo check`; wasm, ARMv6K and RV32 firmware builds compile.
   From act 4 on: G1 and G2 for every scene under the keep rule; G3, G4 and
   G5 once per act, on its final tree. If one of those fails, find the scene
   by rebuilding without each kept scene in turn.
4. Keep rule: keep the act if G passes and W95 cost fell by at least 1% of
   the previous kept cost. Otherwise revert the act's edits by hand (no Git
   state changes), keep its diff as `tmp/cortex-m/jit-plan/actN.diff`, and go
   on. If the gain is under half the estimate, run the counters build
   (`--features mcu,jit-t32,jit-profile`) and report what took its place.
5. Stop rule: a G failure you cannot explain and fix inside the act's own
   code ends the act: revert, report, wait for the Lead.
6. Report per act in `REPORT.md` under "Plan J, act N", at most 12 lines:
   cost before and after, iteration counts that moved, tests added, files,
   diff path. Run directories under `tmp/cortex-m/jit-plan/`.
7. Standing rules: `CARGO_TARGET_DIR=tmp/target-intern`, the micro95 seed
   only frozen and never copied, no Git state changes, no SF32 tools. Other
   sessions edit `src/x86/interp.rs` and `src/machine.rs`; do not touch them
   in acts 0 to 5.
8. A build or test-compile failure whose errors are all in files outside the
   act's file list is not a gate failure. Keep the act's edits, wait 5
   minutes, retry the same command, up to 6 retries; then stop with "plan J
   stopped: tree does not build (<file>)" without reverting.
9. A scene's kept cost is the next scene's "previous kept cost". A scene
   that fails the keep rule is reverted and its diff kept as
   `tmp/cortex-m/jit-plan/actN-sceneM.diff`.

## Act 0: baseline

Scene 0.1. Build the current tree and run W95 with the JIT once. Exit
"baseline ok": PASS and cost within 0.5% of 18844; this cost is the first
"previous kept cost". Exit "tree moved": anything else; report and wait.

## Act 1: LOOP, LOOPE, LOOPNE (estimate: W95 cost -7%)

Scene 1.1, decoder. Other backends must not see new operations. Give the
reader a switch and keep `decode::block` as it is:

```rust
// src/x86/jit/decode.rs
pub enum Op { ...
    /// LOOP/LOOPE/LOOPNE with ECX: decrement without flags, then branch
    /// while ECX != 0 and, for `zero`, while ZF has that value.
    Loop { target: u32, zero: Option<bool> },
}
struct Reader<'a> { bytes: &'a [u8], at: usize, extended: bool }
/// As `block`, plus the operations only the Thumb-2 backend lowers.
pub fn block_extended(bytes: &[u8], eip: u32, limit: usize) -> Block
// in Reader::instruction, next to the 0xe3 arm:
opcode @ 0xe0..=0xe2 if self.extended && bits == 32 /* and no 67h prefix */ => {
    let relative = self.byte()? as i8 as i32;
    let next = eip.wrapping_add((self.at - start) as u32);
    Op::Loop { target: next.wrapping_add_signed(relative),
               zero: match opcode { 0xe0 => Some(false), 0xe1 => Some(true), _ => None } }
}
```

`Op::Loop` ends a block (the terminator list in `block`). `flags_written` is 0
for it; make `flag_requirements` keep ZF live before a `zero: Some(_)` loop
the way it does for a conditional branch. Where another backend's `match`
stops compiling, add an arm that states the operation is not decoded for that
backend. Only `t32/runtime.rs` calls `block_extended`.

Scene 1.2, lowering, modelled on the `Op::Branch` arm with `EcxZero`:

```rust
Op::Loop { target, zero } => {
    self.clobber();                       // flags to Frame; the APSR is free
    let ecx = guest(1);
    self.a.dp(Dp::Sub, false, ecx, ecx, Operand::Imm(1));
    let (taken, done) = (self.a.label(), self.a.label());
    self.a.cmp(ecx, Operand::Imm(0));
    match zero {
        None => self.a.b_cond(Cond::Ne, taken),
        Some(value) => {
            self.a.b_cond(Cond::Eq, done);
            self.a.ldr(R12, FRAME, NZCV);
            self.a.tst(R12, Operand::Imm(Z_BIT));
            self.a.b_cond(if value { Cond::Ne } else { Cond::Eq }, taken);
        }
    }
    self.a.bind(done);
    self.exit_to(next);
    self.flags = Flags::Frame;
    self.a.bind(taken);
    self.exit_to(target);
    return true;
}
```

Scene 1.3, tests: counts 1, 2 and 0 (wraps to 0xffff_ffff: bound the test by
the step budget), LOOPE and LOOPNE with ZF set and clear, and a flag consumer
after the loop that needs flags written before it. Then G and the keep rule.

## Act 2: PUSHAD, POPAD (estimate: -9%)

Scene 2.1, decoder: `Op::PushAll { bits }` and `Op::PopAll { bits }` for 60
and 61 under `extended`; they write and read no flags.

Scene 2.2, lowering for `bits == 32` with the stack fact. One guard covers
all 32 bytes; a page split or TLB miss takes the `miss` path, so the
interpreter performs the whole instruction and nothing is half written:

```rust
Op::PushAll { .. } => {
    self.clobber();
    let miss = self.miss(index);
    self.a.dp(Dp::Sub, false, R0, ESP, Operand::Imm(32));
    self.guard(R0, 32, true, miss);          // leaves the host bias in R3
    self.a.dp(Dp::Add, false, R1, R3, R0);   // host address of the new top
    for (slot, register) in [7, 6, 5, 4, 3, 2, 1, 0].into_iter().enumerate() {
        self.a.str(guest(register), R1, 4 * slot as i32); // EDI lowest, EAX highest;
    }                                        // slot 3 stores the old ESP itself
    self.a.mov(ESP, R0);
}
Op::PopAll { .. } => { /* guard(ESP, 32, false); loads in the same slots,
    skipping slot 3; then ESP += 32 */ }
```

Check `guard` with `bytes == 32` against its comment (a range that leaves the
page must miss) before relying on it. Scene 2.3, tests: PUSHAD then POPAD
restores every register and ESP; the pushed ESP is the value before the
instruction; a stack range that crosses a page gives the interpreter's
result. Then G and the keep rule.

## Act 3: PUSHFD, POPFD, CLI, STI (estimate: -14%)

Port the arm64 implementation (F5); do not design a second protocol.

Scene 3.1, frame: add `popf_mask: u32` to the Thumb-2 `Frame`, set in
`Engine::invoke` from `interp::popfd_change_mask(cpu).unwrap_or(0)`.

Scene 3.2, lowering (`bits == 32`; PUSHFD and POPFD need the stack fact):
- PUSHFD: build the image in native code exactly as `Frame::eflags()` does
  (status from `nzcv`, PF from `result`, AF from `result ^ auxiliary`, the
  rest from `Frame.eflags`), clear RF and VM in the image, push it.
- POPFD: guard the read of `[ESP]`; an image with TF set takes the `miss`
  path. New flags are `(old & !mask) | (image & mask)` with RF cleared, where
  `old` is the full current image. Unpack them exactly as
  `Frame::set_eflags` does, add 4 to ESP, and leave with attention after this
  instruction when IF went from 0 to 1.
- CLI, STI: `popf_mask` bit 9 clear takes the `miss` path (the interpreter
  raises the fault or applies VIF). CLI clears IF in `Frame.eflags`. STI sets
  it and always ends the chain after itself: with the shadow when IF was 0,
  with attention only when it was already 1 (arm64 kinds 6 and 5).
- Two new exit codes beside `EXIT_BOUNDARY` and `EXIT_INTERPRET`:
  `EXIT_ATTENTION` and `EXIT_SHADOW`. Unlike a miss, the instruction has
  completed: refund only the instructions after it and store the next EIP.

Scene 3.3, runtime: `invoke` returns the exit code instead of a bool, and
never resumes a chain after these two exits. `run_batch` then does what
`src/x86/jit/runtime.rs` does for kinds 5 and 6: for the shadow exit call
`cpu.inhibit_maskable_interrupts()` and continue natively when
`continue_after_interrupt_shadow(cpu, bus, next_fits)` allows it; otherwise,
and for the attention exit, return `BatchStop::Attention` with the step of the
last completed instruction and `was_interruptible: false`.

Scene 3.4, tests: PUSHFD image for every status flag and DF; POPFD of each
bit at CPL 0 and at CPL 3 with IOPL 0 (IF and IOPL must not change); POPFD
with TF set; CLI and STI at CPL 0 and, as a `miss`, at CPL 3 with IOPL 0;
`sti; nop` with an interrupt pending is delivered after the NOP and not
before. Add a `jit-profile` counter of attention returns caused by these
exits (A2). Then G and the keep rule. **Stop here and wait for Lead review.**

## Act 4: next coverage batches, from fresh counters

Amended 2026-11-15 after act 3. Entries left (act 3 counters,
`tmp/cortex-m/jit-plan/act3/resume/counters/counts.txt`): MOV `8a` 770 k,
`8b` 682 k, `89` 386 k, `88` 140 k; `66 87` 475 k; `0f a0/a8` 478 k;
`0f a1/a9` 479 k; `d1 /2 reg` 236 k; `ac` 133 k; the rest under 100 k each.

Scene 4.1, measurement only (counters build, one W95 run):
- Entry histogram: for an entry whose first instruction has a memory operand,
  record the segment used, which fact `Lower::segment` missed (flat read,
  flat write, or CS), and whether a 67h prefix was present (A3). Top 20.
- Split table clears of cause `other_context` into "facts changed" and
  "global epoch changed" (`Context` in `Engine::sync`).
- Mode shares (real/V86, 16-bit protected, 32-bit) per window of 25 M steps,
  ten rows, from the existing sampling in `cortex-m/firmware/runtime.rs`.
Write the three tables under "Plan J, scene 4.1", print
"plan J note: scene 4.1 written", and go on with 4.2 without waiting. The
Lead answers the MOV question (scene 4.3) while 4.2 runs.

Scene 4.2, one batch under one keep rule (estimate -3.5%): the pre-approved
operations whose entry has at least 100,000 counts.
- 16-bit XCHG (`Op::Exchange`, `src.bits == 16`): register and memory forms;
  the upper half of the 32-bit register is preserved.
- `PushSegment`, 32-bit operand size: selectors of the six segment registers
  in a new `Frame` field, filled in `invoke`. The interpreter writes only the
  selector word of the 4-byte slot (`push_segment`, test
  `push_segment_writes_selector_word_only`): match its access width and
  checks exactly.
- RCL and RCR by 1 on 32-bit registers (`Op::Shift` kinds 5 and 6): CF and OF
  as the interpreter computes them, other flags untouched.
- Non-repeated LODS and STOS (`Op::String`, `repeat: None`), 8/16/32 bits,
  DF from `Frame.eflags` as `Movs` does.
Segment loads (`PopSegment`, `LoadSegment`) stay in the interpreter. If G2's
RAM hash changes, remove the operations one at a time to find the one.

Scene 4.3, MOV entries. Scene 4.1: 1.97 M visits are memory operands through
DS or SS without the flat fact (no CS override, no 67h); facts change 32,144
times. Pre-check: 99.7% of these visits use an expand-down data segment
(`access & 0x1c == 0x14`), 0.3% a base; none null, code or read-only. Lead
decision 2026-11-15: the checked path covers expand-down segments. Estimate
-5%.

DS/ES/SS without the flat fact take a checked path in `Lower::linear`, the
one FS/GS use today, extended from one limit to a range.
- Rule, the interpreter's (`data_segment_range_contains` in
  `src/x86/interp.rs`): an access of `width` bytes at `offset` is inside when
  `low <= offset`, `offset + width - 1` does not wrap and is `<= high`.
  Expand-up: `low = 0`, `high = limit`. Expand-down: `low = limit + 1`,
  `high = 0xffff_ffff` with the big flag, else `0xffff`; a limit equal to the
  upper bound leaves no offset, so the segment gets no permission bit.
- `Frame`: permission bits (read, write), base, low and high for the five
  data segments. Generalize `based`, `bases`, `limits`; FS and GS move to the
  same range form. Do not add a second mechanism.
- `Cpu`: one new `pub(crate)` accessor next to `jit_based_segments` returning
  permission, base, low and high for a data segment, built from
  `segment_allows_data_access`-equivalent tests on the cache: data segment
  (not code), present selector (not null), writable for the write bit. No
  edit to an existing function in `src/x86/cpu.rs`; re-read the file right
  before the edit, another session works in it. `invoke` calls it only for a
  segment whose flat fact is missing; FS/GS keep their cached masks for the
  permission and take low/high from the cache.
- Lowering: two predicates. "Flat" keeps today's meaning. "Addressable"
  (flat or checked) is used only by operations whose address goes through
  `Lower::linear`. `Movs`, `String`, the stack operations (`STACK_FACT`) and
  anything else that forms an address itself keep requiring the flat fact.
- A block compiled without a segment's flat fact must be valid for every
  state of that segment: flat, null, expand-up and expand-down.
- Leaving for the interpreter is always correct; entering an access the
  interpreter would refuse never is.
- Tests against the interpreter for DS and SS (EBP-based), read and write:
  expand-down big segment with base 0 (offsets `limit`, `limit + 1`, an access
  that ends at `0xffff_ffff`, one that wraps); expand-down 16-bit segment
  (upper bound `0xffff`); expand-up with base != 0 and a small limit (last
  byte inside, first outside); read-only segment written; null DS. Outside
  means a precise exit, after which the interpreter raises #GP or #SS.
- The counters run of this scene also reports, for the checked accesses: the
  five most frequent (selector, base, limit, big flag) with CPL and counts,
  their counts per 25 M step window, and the ten most frequent code pages
  (EIP & !0xfff). The Lead uses this to name the guest code and to judge a
  cheaper form for base-0 expand-down segments.

Not chosen: (a) CS override and (c) 67h addressing have no counts.

Exit "coverage done": no entry opcode has 1% of engine iterations, or two
scenes in a row fail the keep rule. Anything else in the top 20 with at least
100,000 counts: ask the Lead.

## Act 5: cost of one engine iteration

Amended 2026-11-15 after act 3. Profile then: `Engine::run_batch` 14.8% of
samples (about 270 Arm instructions per iteration, with `invoke`, `lookup`
and `sync` inlined), `Engine::probe` 4.3%, `Store::find` 2.9%,
`Engine::publish` 1.7%, `HostBus::jit_code_stamp` 1.9%.

Scene 5.1. `inline.py` profile of `Engine::run_batch`, `Engine::probe`,
`Store::find` and `Engine::publish` on W95 after act 4; a table of Arm
instructions per iteration by part. Then the scenes in this order, each under
the keep rule; skip 5.2, 5.3 and 5.5 when their part has under 1% of samples.
Report the share of building the `Frame` and committing it back in `invoke`
(every field added in acts 3 and 4 is filled per invocation); at 2% of
samples or more, propose a scene to the Lead instead of doing one.

5.6 Quiet control exits stay in native code (estimate -4%). Of 2.84 M control
exits only 555 reach Machine; the rest leave and re-enter for nothing.
`HostBus::batch_interrupts_quiet()` cannot change inside an invocation:
native code performs no device access (state this at the field).

```rust
// lower.rs, Frame
/// HostBus::batch_interrupts_quiet() at entry; constant for the invocation.
pub quiet: u32,
// runtime.rs, invoke
quiet: u32::from(bus.batch_interrupts_quiet()),
// POPFD, after ESP += 4 (R0 = bits that became set)
let attention = self.control_exit(index, EXIT_ATTENTION);
let done = self.a.label();
self.a.tst(R0, Operand::Imm(1 << 9));
self.a.b_cond(Cond::Eq, done);
self.a.ldr(R0, FRAME, QUIET);
self.a.cmp(R0, Operand::Imm(0));
self.a.b_cond(Cond::Eq, attention);
self.a.bind(done);
// STI with IF clear: EXIT_SHADOW unless quiet != 0 and REMAINING != 0, then
// exit_to(next). REMAINING != 0 is `next_fits`: the following instruction
// runs before this batch can yield, natively or through run_one.
// STI with IF set: exit_to(next) when quiet != 0, else EXIT_ATTENTION as now.
```

The runtime's handling of both exits stays for the cases that still leave.
Tests: the Machine-level pending-IRQ test must still pass unchanged (a
pending request is not quiet); add the quiet case for both instructions with
a following instruction in the same chain, compared with the interpreter.
G2's RAM hash is the proof that delivery points did not move.

5.4 Dispatch table first (estimate -3%). After `sync`, a table entry whose
`eip` equals `cpu.eip` in the plane of the current CPL names a block that is
valid to enter: the native dispatcher enters it on the same evidence. Enter
it without `probe` and `publish`.

```rust
// runtime.rs, run_batch, after self.sync(cpu, bus)
let slot = usize::from(cpu.cpl() == 3) * PLANE + lower::table_slot(cpu.eip);
let [eip, code] = self.table[slot];
if eip == cpu.eip && code != self.miss {
    // instruction count from a parallel array written by publish()
    native = self.enter(cpu, bus, code as usize, self.counts[slot], limit);
} else { /* lookup as now */ }
```

`invoke` keeps its rule: a block longer than `limit` is not entered, and the
interpreter runs one instruction. Count table hits in the profile.

5.7 Admission, three pilots with G2 only (estimate -1.5%): `HOT` 8, `HOT` 4,
and `HEAT` 16384 with the better `HOT`. Reason: compiling is 0.15% of
samples, while 136 k first visits found their heat slot taken by another
address and cold code costs 1.72 M single-instruction iterations. Keep the
best variant if it passes the keep rule with at most three arena clears;
report code KiB for each.

5.2 Take `batch_attention_limit` once per `run_batch` call and again after
every interpreter call; after a native run use `limit - executed`. State the
invariant in a comment: native code performs no device access.

5.3 Leave `sync` early when no epoch, fact, CS base or invalidated range
changed since the last call. No edits outside `src/x86/jit/t32/`.

5.5 In `publish`, skip the global-translation lookup when the slot already
holds this block.

## Act 6: conditional

- 6a, only if cold iterations (new, counting, sequential) are at least 5% of
  engine iterations after act 5: let cold 32-bit code run to the next control
  transfer, reusing the interpreter's until-control hook. It edits
  `src/x86/interp.rs`: Lead decision first.
- 6b, only if scene 4.1 attributes most `other_context` clears to changed
  facts: keep the tables of the last two contexts instead of clearing. A
  clear for a changed global epoch follows the guest's own TLB flush and
  stays.

## Act 7: not part of this plan

The 16-bit interpreter paths took about 45% of W95 time after act 3, the
largest remaining cost. Further work on them is a user decision. For 16-bit
compilation, see the
[measurements that led to T16](#measurements-that-led-to-t16).

Other deferred work includes the quiet test for POPFD in the shared
interpreter (STI has it), revalidation after code-page writes
(`jit_code_stamp`, 174 k times), `probe_linear_write_u8_at_priv`, direct
block links, native TLB refill and the new operations in the arm64 backend.

## End leaves, judged on W95 after act 5

- "JIT stays": cost at most 0.67 of the interpreter's (11,750).
- "marginal": between 0.67 and 0.85; Lead and user decide.
- "interpreter only": above 0.85; the board build drops the JIT and gives its
  memory to the guest.
- Memory node, any act: more than three arena clears on W95; report it; the
  Lead decides whether the code arena grows to 3 MiB inside the 12 MiB cap.

## Measurements that led to T16

The Lead recorded these Windows 95 measurements and the proposed stages on
2026-11-15 at the user's request. For the outcome, see the
[T16 pilot and research](#t16-16-bit-protected-mode-code-on-thumb-2).

Measurement (W95A after step 250 M; `REPORT.md` "Task X"):
- Mode shares: 16-bit protected (PM16) 19.18%, 32-bit 80.70%, real/V86
  0.11%.
- PM16 costs about 218 Arm instructions per step in both engines. The
  windows 425-475 M (SkiFree, 96% PM16) cost 5.39 G and 5.25 G Arm
  instructions with the JIT, 5.33 G and 5.27 G with the interpreter.
- Derived: PM16 is about 55% of the JIT's time after boot (76.7 M steps x
  218 of 30.1 G), and about 32% when the two SkiFree windows are left out
  (USER and GDI are 16-bit). The flat profile agrees: interpreter 54% of
  samples. 32-bit code costs about 42 per step there.

Estimate, not measured: compiled PM16 code at 60 to 80 per step brings the
cost after boot from 75 to 45-50, and a 16-bit program such as SkiFree to
about a third of its cost. This assumes item 4 of the outline; see the
instruction mix at the end of this section.

Stages:
- A. PM16: Windows 95's USER, GDI, KRNL386 and 16-bit programs. This is the
  recommendation.
- B. Real mode: DOS, DOS programs and games, and the DOS part of the Windows
  boot (0-50 M steps, 89% real mode: 9.4 G of the boot's 23.7 G Arm
  instructions; the BIOS and FreeDOS boot of task U costs 185 per step). It
  reuses A's decoder, addressing, stack and dispatch work. Segment loads and
  far transfers need no descriptor checks there and can be inline.
- Not planned: virtual-8086 mode (DOS boxes under Windows; 0.1% of W95A).
- DOS games: real-mode games gain only from B. 32-bit DOS-extender games run
  32-bit protected-mode code, which the JIT accepts today when CS is flat
  with a 4 GiB limit; no such game has been run on this backend.

Design outline (Lead; to become acts, checked against the code, on a go):
1. Decoder (`src/x86/jit/decode.rs`): a code-size input, default operand and
   address size 16; 16-bit ModRM forms (67h is rejected today); IP modulo
   64 KiB; the 16-bit forms of CALL, RET, PUSH, POP, PUSHA, POPA, PUSHF,
   POPF, LEAVE, LOOP and JCXZ. Thumb-2 only, like `block_extended`.
2. Lowering: 16-bit effective addresses wrap before the segment check. The
   stack uses SP when SS is 16-bit: wrap at 64 KiB, upper half of ESP kept.
   It shares the stack helper of scene K1. Add the 16-bit operations the
   lowering rejects today (ADC, SBB, MUL, DIV, IMUL and others; list from
   task Y part 1).
3. Segments: 16-bit blocks use the checked range path of scene 4.3 for every
   data segment and no flat fact, so a DS or ES load invalidates nothing.
   Facts left: code size and stack size.
4. Segment loads and far transfers inside one privilege level (MOV and POP
   to a segment register, LES, LDS, CALL far, RETF, JMP far): helper calls
   from native code into the interpreter's own load functions, so chains do
   not end there. On a fault the helper changes nothing and the block leaves
   before the instruction. These would be the first helper calls of this
   backend; `Frame.quiet` relies on native code performing no device access,
   so a descriptor table outside RAM must leave instead. POP FS/GS in 32-bit
   code (1.03 M interpreted entries on W95A) would use the same helper.
5. Dispatch: a CS-base change clears the table today (`Context.cs_base`),
   and 16-bit Windows changes CS at every far call. Key 16-bit entries by
   code segment and IP: an interned id of (base, limit) in the tag, or a
   wider entry. The block index needs no change:
   `Store::find(eip, facts, backing)` does not depend on the base.
6. Eligibility (`Engine::run_batch`): today `!cpu.code_default16()`, CS limit
   `u32::MAX`, TF and VM clear. A 16-bit block must end inside the CS limit.
7. Memory: W95A ends with 1,848 KiB of the 2 MiB arena in use after two
   clears; 16-bit blocks add to that.

Risks: 16-bit Windows and DOS code keeps code and data close, so
revalidation after writes to code pages (`jit_code_stamp`) grows; the
differential tests must cover every 16-bit form added.

Alternative under the current rule: tune the shared interpreter's 16-bit
paths. `decode_mem_addr_slow`, `read_linear_u16_at_priv`,
`fetch_descriptor_raw`, `push_u16`, `read_mem_u16_at`, `load_segment`,
`resolve_segment_at_cpl`, `update_flat_segment_mask` and `write_mem_u16_at`
are 16.6% of the JIT run's samples after boot. Bound on this target: PM16 at
the interpreter's 32-bit cost (169 instead of 218) lowers the cost after
boot by about 12%. Every host gains; another session works in those files.

PM16 instruction mix (W95A after boot, 76.9 M PM16 steps, single-stepped
with `R86_T32_PROFILE_CODE16=1`; `REPORT.md` "Task Y, part 1"):
- Segment loads 7.0% of the steps (MOV to a segment register 4.9%, POP DS
  1.1%); far transfers 1.8% (CALL far 0.7%, RETF 1.1%); REP strings 0.2%;
  I/O and system instructions 0.2%; interrupts 0.001%; x87 none. The other
  90.5% are ordinary instructions.
- Mean run of ordinary instructions between two of the others: 9.7.
- So item 4 is required. Lead estimates: about 120 per step if segment loads
  and far transfers leave native code, about 60 with the helper calls.
- 83 code segments (selector and base); the CS base changes once per 72
  steps. Top: `0117` at `0000ce60` 32%, `17f7` 17%, `030f` 14%.
- All PM16 steps run at CPL 3, all but 0.01% with a 16-bit SS.
- Prefixes: 67h 1.9% of the steps, a segment override 9.4%.
- Forms the lowering lacks among the 60 most frequent opcodes: the 16-bit
  stack forms (PUSH, POP, CALL, RET, LEAVE, PUSH of a segment register),
  8-bit and 16-bit ADC/SBB, RCL of an 8-bit register by 1 (1.3%), ROR of a
  16-bit register by CL (0.7%), JCXZ (0.9%), 8-bit XCHG (0.5%), MOV from a
  segment register (0.8%).

## T16: 16-bit protected-mode code on Thumb-2

**Stopped, 2026-11-18:** the research's verdict, then the user's decision.
Act 1 (`b0dd54a`, G1–G5) is kept. The Act 2 code with Fix A is kept in
history as `a8a2ef3`, marked as an experiment and reverted by the next
commit. Both pilots passed G1/G2 (differential tests, frame and RAM).
Fix A cost 94.32 Arm/step with eight full-run clears (limits 69.4 and
six). At fixed coverage, 62.53 (73.05 − 13.71 + 3.19) leaves 6.87 for
overhead. Fix A added 31.78.

The Intern used frozen Micro95 and `firmware/w95a.keys` on QEMU Cortex-M33
with `-icount shift=0`. Act 0 PM16 took 19.36% of app steps and ~57% of cost.

| Plain W95A (Arm instructions per x86 step) | Boot 0–250 M | Full 0–650 M | Apps 250–650 M | Full clears |
| --- | ---: | ---: | ---: | ---: |
| Act 0 baseline (`a8e4306`) | 94.62 | 81.47 | 73.25 | 3 |
| Act 1 retain mapping epochs on CS loads | 94.41 | 81.27 | 73.05 | 3 |
| Act 2 PM16 pilot | 113.34 | 114.65 | 115.47 | 8 |
| Act 2 Fix A, writable-alias hint and per-physical-page slot retirement | 100.39 | 96.66 | 94.32 | 8 |

D1 gave CPL3 PM16 with `SS.B=0` an 8,192-slot `[linear, code]` plane.
Blocks checked IP before reserving budget and shared the `CODE16` Store fact.

| Sampled cost after boot (Arm/step) | Act 1 | Fix A | Change |
| --- | ---: | ---: | ---: |
| Native entry and return | 1.63 | 11.08 | +9.45 |
| Other engine loop, lookup and Store | 3.37 | 19.91 | +16.54 |
| Table and context maintenance | 0.84 | 3.75 | +2.91 |
| Compilation and reset sites | 0.17 | 0.62 | +0.45 |
| Versions, watches and revalidation | 1.38 | 0.48 | −0.90 |
| Generated code (estimated PM16 cost 3.19) | 11.19 | 14.31 | +3.13 |
| Interpreter | 40.15 | 26.44 | −13.71 |
| Paging, shared helpers, devices and other | 14.33 | 17.73 | +3.40 |
| **Total** | **73.05** | **94.32** | **+21.27** |

The two engine rows grew by 1,275 Arm instructions per extra invocation
(8.16 M), including cold iterations. Probe/Store took 340, eligibility and
deadlines 207, publication 87, Frame/invoke 367, stubs 10 and other loop
work 264. Two first-way probe hits, publication and stubs alone cost 309–310.

Across both modes, the leading interpreted entries after boot were MOV Sreg
(3.34 M), POP DS (680k), POP FS (605k), POP GS (593k), RETF (`CB`, 477k),
LODSB (394k) and CALL FAR (`9A`, 393k). Of 8.35 M PM16 returns, 6.90 M were
unsupported, 886k hit guards and 538k were cold. Blocks read live DS/ES/FS/GS
rights, bounds and bases. A cached load could refresh its Frame row and chain.

PM16/PM32 code bytes averaged 394/380 per block and 113/98 per static x86 op.
After boot, clears rose from one to four and compiles from 7,091 to 25,123
(13,904 PM16, 11,219 PM32). Of these, 18,112 rebuilt code lost to clears.
The six-clear limit needs 11.8% fewer compiled bytes: the full run allocated
16.6 MB, and seven 2 MiB fills hold 14.7 MB.

| Lever, ranked by after-boot Arm/step saved | Saving | Estimated source lines |
| --- | ---: | ---: |
| Reuse A64's cached MOV/POP segment helper, 6.14 → 12.11 steps per PM16 run | 9.67 modeled after direct handoff | 70–120 |
| Skip the second lookup on a known interpreter target | 4.97 measured (89.35), 0.46 on PM32 alone (72.59) | <10 |
| Retain Frame segment state | ≤2.99 on Fix A, overlaps longer runs | 40–80 |
| Join same-ring far transfers with precise stack faults, 14.48 steps/run | 1.72 modeled | 150–300 |
| Also join cached far-pointer loads, 15.02 steps/run | 0.33 modeled | 30–70 |
| Raise PM16 admission from 16 to 64 visits | −1.72 measured (96.04), five full clears | 1 |

At 1,275 Arm per join less 255 saved by direct handoff, 4.60 M joins give
77.62. The 8.22 gap before helper costs favors stopping (moderate confidence).

## W9T: segment-load handoff on Thumb-2

Rejected 2026-11-19; the code is in history as `205d6e7`, reverted by the
next commit. Porting arm64's `787c5cd` (end an interpreter batch after a
CS or SS load that reaches compiled code) raised W95A's cost after boot
from 73.122 to 73.825 Arm instructions per x86 step (+0.96%; a keep
needed 70.928). It moved only 4,468 guest steps to native code in 650 M,
so it cannot pay on this workload. W9's other change, compiling 32-bit
code in size-limited segments (`233889e`), was not ported: it left W95A
unchanged on arm64, so that code is not a cost there.

## ARMv6K backend (New 3DS)

Status: checked 2026-11-15 on the user's question; nothing started.

Facts:
- `src/x86/jit/arm32/` is the ARMv6K backend for the 3DS; a 32-bit ARM build
  without `jit-t32` uses it (`src/x86/jit/mod.rs`). The Thumb-2 backend was
  derived from it before this plan, so it has none of the plan's changes:
  `decode::block` (no LOOP, PUSHAD, POPAD); no PUSHFD, POPFD, CLI, STI and no
  control exits (`invoke` returns a bool); no checked ranges, so DS/ES/SS
  need the flat fact and Windows 95 ring-3 data accesses are interpreted; no
  16-bit XCHG, PUSH of a segment register, RCL/RCR, LODS/STOS; no batch for
  first-seen cold code; no resuming of dispatch misses inside `invoke`.
- Expectation, not measured: on Windows 95 it behaves like the Thumb-2
  backend at this plan's baseline or worse (there the JIT was 7% slower than
  the interpreter).
- The 3DS cannot run the Thumb-2 backend. Old and New 3DS have ARM11 MPCore
  cores (ARMv6K): ARM and the original 16-bit Thumb only.
  `design/VALIDATION.md` says the same of QEMU's ARM11 model. A backend for
  16-bit Thumb would be new code with low registers only, flag-setting
  arithmetic and no conditional execution: more instructions per x86
  instruction.
- Code density: the Thumb-2 encoder emits 32-bit encodings only
  (`src/x86/jit/t32/asm.rs`), so its code is as large as A32 code. Narrow
  encodings are a possible later step for the Cortex-M33 (32 KiB instruction
  cache); instruction counts cannot show their value, only the board can.

Routes:
- R1, recommended: one code generator, two encoders. `t32/lower.rs`,
  `runtime.rs`, `store.rs` and the tests stay; a second `Assembler` with the
  same interface emits A32 restricted to ARMv6K. Differences to absorb:
  constants (no MOVW/MOVT), BFI/BFC (6 uses), SDIV/UDIV (none on ARMv6K: DIV
  stays in the interpreter), IT (plain conditional execution), 32-bit code
  units and no Thumb bit, immediate encoding rules, and the Horizon
  code-mapping hook of `arm32/code.rs`. Then `arm32/compile.rs`,
  `runtime.rs` and `tests.rs` are deleted. The 3DS receives this plan's work
  and everything later.
- R2: port each kept row of the Status table into `arm32/` by hand. Every
  later change is then made twice.

Before either: a baseline. W95 or W95A with the ARMv6K backend under
`qemu-arm -cpu arm11mpcore` (`design/VALIDATION.md`, "Optional ARMv6K (3DS)
JIT checks"): the `jit-profile` entry histogram, native and interpreted
steps, and host instructions counted with a QEMU plugin if a cost number is
needed. QEMU timing is not 3DS performance; only the 3DS gives that.
