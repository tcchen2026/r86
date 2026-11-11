# Resident-bank translator on the ISS

Intern implemented steps 2 and 3 of [TRANSLATOR.md](../design/TRANSLATOR.md).
Lead review is pending. The emission table now runs through the real RV32 ISS;
no host x86 callback executes translated instructions. This batch has **no RTL
firmware evidence**. Instruction counts and their limits are in [JIT_COST.md](JIT_COST.md).

## Execution contract

`jit/compile.rs` uses the shared x86 decoder. Bank 1 holds EAX..EDI in x8..x15,
with x16 for indirect/slow exits. Blocks start with XTICK and end at a control
transfer, unsupported instruction or approved code-page boundary (4095 decoded
instructions maximum). Native words have a side table of guest EIP/instruction
index. There is no frame pointer or generated import/export sequence.

The emitter covers the guide's register moves/partial lanes, memory moves,
MOVZX/MOVSX, arithmetic/compare/test, register and memory INC/DEC, dword register
shifts/rotates, LEA, SETcc to a register, dword PUSH/POP/CALL/RET, and near branches.
Other instructions use the shared interpreter, including NOT/NEG, LEAVE,
strings, multiplication/division, narrow/memory shifts, RCL/RCR, system/segment
operations and 16-bit stack operands. Real/VM86/CS16 execution and SS16 stacks
stay interpreted. CS must currently have a full limit; data segments may be
based or bounded. Unsupported segment descriptors receive no memory grant.

`jit/runtime.rs` copies registers/status only around interpretation or an event
boundary. XENTER/XLEAVE preserve bank 0. Miss dispatch and successful RAM refills
retain bank-1 state. `jit/native.rs` contains the explicit array/word boundary;
it does not address the layout of Rust's Cpu.

- XTICK reserves a complete block. A service uses the native-PC table to refund
  the unfinished suffix before interpretation. Expiry runs no instruction from
  that block. Only completed native instructions advance guest TSC/counts.
  Device deadlines bound reservations; shadows remain shared interpretation.
- A service stub executes XLEAVE, then bank 0 ends the active service with XGRET
  before reading its persistent record. TLB misses may refill approved ordinary
  RAM and retry the recorded native word. Missing A/D bits, permission faults,
  MMIO, unsupported segment proofs and page splits use the shared instruction.
  Unexpected native faults are reported as firmware bugs.
- RMW loads request write permission before committing status. Their matching
  stores cannot fault after that proof. PUSH/CALL update ESP only with a
  successful store. CALL captures its operand before changing ESP; XPUSHRET
  names an adjacent stable return stub, and RET uses XJT.R.
- Fetches and memory proofs reuse existing MMU caches/resident probes. Context
  changes and the separate pending INVLPG range retire hardware mappings.
  Segment publication occurs on a changed context/descriptor, not every entry.
- The existing code-page watch/revoke protocol denies native writes to translated
  source. Promotion also flushes older hardware write grants. A changed code
  epoch flushes jumps/return addresses before dropping the cache and watches.
- Only one guest code page is installed in XJT at a time. Direct same-page exits
  can patch a JAL within its signed reach; other targets use XJT/miss dispatch.
  Replacement or changed mappings restore external JAL exits before freeing or
  reusing targets. FENCE.I publishes both code and patches. Cross-page links are
  excluded before searching for a source link.

The cache retains the previous third-visit threshold, 2048 entries and 8 MiB code
limit. The firmware uses its fixed 4 KiB-aligned RAM arena. Runtime search,
allocation/decoding costs, cache policy and scheduling-tail efficiency are not
tuned in this batch. Cross-page links, full step-4 optimization, Windows boot,
RTL clocks/caches and board execution remain separate work for Lead.

## Functional evidence

Local records: `tmp/r86chip/batch2/`; Intern's review package is
`tmp/r86chip/handoff2.txt`. The emitter's 11 differential tests compare actual
native execution with Interpreter::step across every implemented emission row.
They check registers, flags, EIP and RAM, including old-register aliases, all Jcc
conditions, partial lanes, immediate edges, based segments, stack operations,
return-stack chaining, and precise service/XTICK boundaries.

`jit_checks.py --engine iss` compares interpreter/JIT executions of protected-mode
guests. Thirteen focused cases passed: cold entry, lanes/branches, SMC, physical
alias, INVLPG remap, changed entry, immediate borrow, precise page fault,
cross-page fetch, PIT/STI boundary, split-stack CALL/RET, based-FS reload and RMW
write fault. The negative-entry fixture was then updated to use unsupported NEG
replaced by NOPs; it passed separately. Every comparison includes guest RAM,
EIP, GPRs, EFLAGS, TSC and steps; each guest asserts its expected outcome.

The final firmware's 20M-step TinyCore prefix matched the host interpreter in
all 9,159 boundary records, guest RAM and 4,445 bytes of kernel serial output,
including registers/EIP/EFLAGS and TSC. Its matched cost endpoints also pass
end-state comparisons (including completed/fault/repeat counts).
Full boot remains unverified: Lead selected bounded evidence and the optional
long run was stopped before its marker. Do not infer a boot pass from these
gates or from the earlier frame-based translator's results.

One integration failure found and fixed here: checking only the full MMU epoch
missed a single pending INVLPG range. The precise-page-fault guest exposed the
stale data proof. The runtime now consumes the existing invalidated-range hook.
The initial unit harness also needed FENCE.I when replacing its setup word; no
ISS or ISA semantics were changed.

Sources consulted: shared decoder/interpreter/MMU/cache-watch code; current
translator guide and ISA source; tiny386 `i386.c:tlb_refill` for the simple
paging/refill design. No external implementation was copied or translated.

## Reproduction

Use `CARGO_TARGET_DIR=tmp/target-intern` (or set SIM to another existing build).
Build the firmware with the existing linker script and preserve its ELF/bin in
TEMPLATE before running the experiment scripts:

```sh
CARGO_TARGET_DIR=tmp/target-intern cargo test --release --no-default-features --features r86chip --bin r86chip-sim b1_tests
CARGO_TARGET_DIR=tmp/target-intern cargo build --release --no-default-features --features r86chip --bin r86chip-sim
CARGO_TARGET_DIR=tmp/target-intern cargo rustc --release --no-default-features --features machine,r86chip --bin r86chip-machine --target riscv32im-unknown-none-elf -- -C link-arg=-Tr86chip/firmware/machine.ld -C link-arg=--no-relax -C relocation-model=static
python3 r86chip/experiments/jit_checks.py --engine iss --simulator SIM --template TEMPLATE --out OUT
```

The input preparation and comparison protocols remain in `tinycore.py`,
`firmware/machine.rs` and `compare_machine.py`. These scripts can also run RTL;
this batch selected only ISS. Keep the firmware build separate from a baseline
archive build because both use the single approved Cargo target directory.
