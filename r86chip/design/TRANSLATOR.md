# Translator firmware guide (Lead to Intern)

Scope: rewrite `r86chip/jit` (compiler and runtime) for the translator words
of ISA identity `0x4231000a`. The word semantics are in the "Translator words"
section of [rv32.rs](../sim/rv32.rs); this file says how firmware should use
them. The existing `jit/compile.rs` and `jit/runtime.rs` may be replaced
whole: their frame-copy ABI and budget-one fallback are the measured problem
([JIT_COST.md](../experiments/JIT_COST.md): 392 native words per x86
instruction with 2.4% of them in generated code).

Rule that still holds: 16-bit code, real mode and VM86 stay in the shared
interpreter. Translate only when CS and SS are 32-bit. Do not fork x86
semantics: anything not listed below leaves the block and runs in the shared
interpreter.

## Goal and measure

One number: **clocks per completed x86 instruction on the RTL**, compilation
and runtime included, over TinyCore steps 1,000,000 to 10,000,000 (the
JIT_COST interval), then a Windows 2000 boot interval. Report with it:

- coverage: share of x86 instructions executed in bank 1;
- x86 instructions per XENTER;
- emitted native words per translated x86 instruction;
- services per 1,000 x86 instructions, by reason;
- compiled blocks, invalidations, jump-table misses.

Milestones: interpreter parity (about 118 clocks: 92 words at 1.29), then 20,
then 10. Generated code alone should be near 3 words and 4 clocks per x86
instruction; everything above that is runtime, and that is where to look.

## Register ABI (bank 1)

| Register | Use |
| --- | --- |
| x8..x15 | EAX, ECX, EDX, EBX, ESP, EBP, ESI, EDI, resident for the whole stay in bank 1 |
| x16 | next guest EIP at an indirect branch or a block exit |
| x1..x7, x17 | temporaries, dead at every x86 instruction boundary |
| committed status | CF, PF, AF, ZF, SF, OF of the guest |

Guest registers have one home while translated code runs. Entry and exit are
XENTER and XLEAVE alone. The runtime copies registers to or from `Cpu` (XREAD,
XWRITE, XFGET, XFLOAD) only when the interpreter must run or a fault is
delivered, not per block. DF, IF and the other EFLAGS bits stay in `Cpu`.

## Block shape

```
block:  XTICK n            ; n = x86 instructions in this block
        body
        exit
```

- A block is one basic block, ended by a control transfer, an instruction
  outside the table below, or a page boundary of the guest code.
- Direct exit to a translated block on the same guest code page: `JAL x0`
  (or XJcc when in range), patched in place and followed by FENCE.I. To an
  untranslated or other-page target: load the EIP into x16, then `XJT x16`,
  then `XLEAVE miss`.
- Indirect exit (RET, JMP/CALL through register or memory): EIP in x16,
  `XJT x16`, `XLEAVE miss`.
- On `XLEAVE miss` the runtime finds or compiles the block, `XJTFILL`s it and
  re-enters. XINFO selector 1 gives the leave PC, from which the block and x16
  are known.
- XTICK expiry leaves with reason 0xfff and the XTICK's own PC: no instruction
  of that block has run. Deliver events, refill with XTSET, re-enter there.

## Emission

Words per x86 instruction, 32-bit operand size. `t` is a temporary. A
displacement outside -2048..2047, an index register or an FS/GS/ES override
costs extra address words (LUI/ADDI, SHnADD, or XLD/XSW with the segment).

| x86 | Native words | Count |
| --- | --- | ---: |
| MOV r,r | ADDI | 1 |
| MOV r,imm | LUI, ADDI | 1-2 |
| MOV r,[m] / MOV [m],r | XLDD / XSWD | 1 |
| MOVZX/MOVSX r,[m] | XLDD funct3 4/5 or 0/1 | 1 |
| ADD OR ADC SBB AND SUB XOR CMP TEST r,r | XALU.C (rd=x0 for CMP/TEST) | 1 |
| same with imm in -2048..2047 | XALUI | 1 |
| op r,[m] | XLDD t; XALU.C | 2 |
| op [m],r or [m],imm | XLDD write-intent t; XALU.C or XALUI t; XSWD t | 3 |
| INC/DEC r | XINC/XDEC | 1 |
| SHL SHR SAR ROL ROR r, imm or CL | XSH | 1 |
| LEA | ADDI, SHnADD | 1-2 |
| PUSH r | XSWD.U through SS, imm -4 | 1 |
| POP r | XLDD through SS; ADDI esp | 2 |
| CALL rel | LUI, ADDI t; XPUSHRET t; JAL | 4 |
| RET | XLDD x16; ADDI esp; XJT.R; XLEAVE | 3 on a hit |
| Jcc / JMP rel | XJcc / JAL | 1 |
| SETcc r8 | XCOND; XMERGE | 2 |
| 8- and 16-bit register forms | XALU lanes, XMERGE | 1-2 |

Leave to the interpreter at first, then add by measured frequency: string
instructions (a runtime helper copying through XTLOOK pointers is the likely
form), MUL/IMUL/DIV (RV32M plus status), shifts of 8/16-bit and memory
operands, RCL/RCR, SHLD/SHRD, XCHG with memory, far transfers, segment loads,
privileged and system instructions, x87.

CALL and RET pair through the hardware return stack. XPUSHRET names the
native address of the return point's block as a word offset from itself, so
emit that block (or a stub that loads the return EIP into x16 and leaves)
within 2047 words of the call and never move it while it can be popped. XJT.R
then returns in three clocks when the stack agrees, and falls back to the jump
table otherwise. Indirect calls use XPUSHRET the same way, followed by XJT.

Measured on the RTL with hand-translated loops (`xlate_rtl.py --bench`,
zero-wait memory, generated code only): an array sum runs at 1.20 words and
1.60 clocks per x86 instruction, a mixed compare/shift/LEA/memory loop at
1.52 and 1.95, and a PUSH/CALL/POP/RET loop at 2.11 and 2.67.

## Precision rule

At every word that can request service (XLDD, XSWD, XLD, XSW) and at every
leave, x8..x15 and committed status must hold the guest state from **before**
the current x86 instruction. So: results go to temporaries until the
instruction's last fallible word has run. The cases above already satisfy it:

- XSWD.U writes ESP only when the store succeeded.
- POP loads straight into the destination, then adjusts ESP; nothing after the
  load can fail. `POP ESP` and `POP [m]` need their own order.
- For `op [m],r` the status may commit before the store, because a store to
  the address of a write-intent load that hit cannot request service.

Keep a side table per block from native PC to (guest EIP, index in block).
The runtime uses it to find the faulting instruction and to refund XTICK for
instructions that did not run.

## Runtime protocol

- Service vector: a bank-1 stub that is `XLEAVE service`. Install it with
  XGSVEC before XENTER and restore the interpreter's vector after leaving. In
  bank 0, first `XGRET rs1` to the runtime's own next instruction: this ends
  the active service (a second miss while it is active stops the core) and
  leaves the record readable (XGGET 0..3). To resume: fill the mapping, then
  XENTER the recorded retry PC. To deliver a guest fault or emulate the
  access: copy registers out, use the shared interpreter for that one
  instruction, then re-enter at the next block.
- Data mappings now survive: 1024 rows, and a refill from the hardware table
  costs about six clocks. Do not flush them on every entry. Flush on the
  events listed in rv32.rs (CR3, CR0/CR4 bits, A20, privilege change if the
  proofs depend on CPL).
- Self-modifying code: fill pages that hold translated code without W. A
  write then requests service with reason 4; invalidate that page's blocks
  (XJTINV each, or XJTFLUSH and drop the code cache while this is rare: four
  invalidations in nine million instructions on TinyCore), then let the write
  proceed. See [CODE_CACHE.md](CODE_CACHE.md).
- Jump table validity: XJTFLUSH whenever an EIP can change meaning: CR3
  write, CS base or size change, code cache flush.
- Single-instruction fallback must be cheap. Today each miss pays a full batch
  entry including a mapping flush and six segment publications. Publish
  context and segments when they change, not when an engine starts.

## Order of work

1. ISS only: differential tests of XALUI, XINC/XDEC, XSH, XLDD and XSWD
   against `Interpreter::step`, in the style of `b1_tests.rs`. These words
   pass RTL-against-ISS checks, but the ISS models have not yet been compared
   with the shared interpreter.
2. Resident registers and the new block shape with register-only bodies.
   Check: coverage and x86 per XENTER on the register loop and TinyCore.
3. Memory operands, PUSH/POP, CALL/RET with XJT. This is the step that should
   make blocks whole basic blocks.
4. Links, XTICK accounting with refund, service resume without a batch entry.
5. Measure on the RTL (`tinycore.py --jit` path with the Verilator machine
   model), then decide the next instructions by the leave-reason histogram.

Hand back `git diff` and the numbers above after steps 2, 3 and 5.

## Hardware limits to design around

- XLDD/XSWD reach DS and SS only; other segments take XLD/XSW after an
  address word (`XLD rd, x0, FS` reads `fs:[0]` in one word).
- An access that crosses a 4 KiB page always requests service (reason 3).
- XJT costs three clocks plus a three-clock redirect on a hit; XJT.R with a
  matching return stack costs two plus one. Prefer direct links where the
  rule above allows them.
- A redirect resolved in M costs three clocks; a predicted JAL or conditional
  branch costs one. Lay out the likely path as fall-through where known.
- A consumer directly behind a guest or native load waits one clock.
- Checks: `experiments/rtl.py --gowin` (existing words) and
  `experiments/xlate_rtl.py` (new words) compare RTL with the oracle and ISS.
  No translated x86 code has run on this core yet.
