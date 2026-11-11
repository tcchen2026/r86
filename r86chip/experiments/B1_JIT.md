# Resident-bank translator on the ISS

Intern's task-3 implementation is pending Lead review. The ISA is unchanged;
`sim/rv32.rs` remains its authority. This batch uses the ISS only. Current
instruction counts and limits are in [JIT_COST.md](JIT_COST.md).

## Execution contract

`jit/compile.rs` uses the shared x86 decoder. Bank 1 holds EAX..EDI in x8..x15,
with x16 for indirect/slow exits. Each block starts at XTICK after one
unexecuted source-EIP word. A block ends at a control transfer, unsupported
instruction or approved code-page boundary (4095 instructions maximum).
The native-PC side table records guest EIP/instruction index for service refunds.
There is no frame pointer or generated import/export sequence.

The emitter covers register moves/partial lanes, memory moves, MOVZX/MOVSX,
arithmetic/compare/test including immediates, register NOT/NEG, register and
memory INC/DEC, dword register shifts/rotates, LEA, register/memory SETcc,
dword PUSH/POP/CALL/RET, and near branches including rel32 Jcc. Other operations
use the shared interpreter: memory NOT/NEG (not represented by the shared
decoder), LEAVE, strings, multiplication/division, narrow/memory shifts,
RCL/RCR, system/segment operations and 16-bit stack operands. Real/VM86/CS16
execution and SS16 stacks stay interpreted. CS currently needs a full limit;
data segments may be based or bounded. Unsupported descriptors receive no
memory grant.

`jit/runtime.rs` copies registers/status only around interpretation or an event
boundary. Miss dispatch and successful RAM refills retain bank-1 state.
`jit/native.rs` contains explicit array/word assembly boundaries, never Rust
Cpu layout accesses. Context is published only when entering native code.

- XTICK reserves a complete block. Expiry reads the immutable EIP immediately
  before XTICK without searching entries. An unhandled service still searches
  the native-PC side table to refund the unfinished suffix before interpretation.
  Only completed instructions advance guest TSC/counts. Device deadlines bound
  reservations; interrupt shadows remain shared interpretation.
- The service stub executes XLEAVE. Bank 0 ends the active service with XGRET
  before reading its persistent record. Approved RAM refills resume the exact
  native word. Missing A/D bits, permission faults, MMIO, unsupported segment
  proofs and page splits use the shared instruction. Native faults report bugs.
- RMW loads request write permission before committing flags. Their matching
  stores cannot fault after that proof. PUSH/CALL update ESP with a successful
  store. CALL captures its operand before changing ESP; XPUSHRET names a stable
  adjacent return stub. RET uses XJT.R.
- Fetch/data proofs reuse shared MMU caches and resident probes. Mapping epochs,
  CS/control changes and physical remapping retire jump proofs. Single-page
  INVLPG uses existing XTINVAL; large ranges flush data proofs. Conservative
  bounds of installed linear code pages allow data-only invalidations to retain
  jumps. Any possible overlap flushes all jumps and external JAL links. Bounds
  reset with every jump flush; holes/stale bounds only cause extra flushes.
- Watched code pages cannot receive native write grants. Promotion flushes old
  grants. A changed code epoch drops jumps/return addresses before freeing code.
- XJT entries survive code-page changes while their fetch proofs remain valid.
  Direct JAL links remain page-local and within signed native reach. Code-cache
  replacement restores external links before freeing targets. A boolean avoids
  scanning when none exist. FENCE.I publishes compilation and patches.

## Indexed lookup

The cache has 2048 entries, two probes per guest-EIP hash, the existing third-visit
threshold and 8 MiB code limit. Each entry still checks the approved backing
address. Cold/negative entries yield to compiled entries; full sets replace in
round-robin order. There is no additional allocated index.

Every MISS has an unexecuted source-EIP word at its saved leave PC. Runtime reads
it before target lookup can displace the source, then probes at most two source
entries and their bounded link list. Full EIP, native offset and target tags
must match before patching. No successful native branch executes the metadata.
The source index replaces the former scan of all 2048 entries. Cache collisions,
code writes, remapping and return stubs retain the same ownership rules.

## Evidence and reproduction

The local review package is `tmp/r86chip/handoff3.txt`; reports, firmware hashes,
profiles and stage-specific test logs are in `tmp/r86chip/batch3/`. Twelve emitter
tests compare actual native execution with Interpreter::step, including partial
lanes, all flags/conditions, signed-width edges, RAM, aliases and precise service
boundaries. Runtime fixtures cover SMC, physical aliases, INVLPG and CR3 remap,
A20 changes, cache collisions, negative entries, page faults, cross-page fetch,
PIT/STI boundaries, based segments and split-stack calls. The handoff identifies
which firmware ran each check; unchanged checks are not repeated automatically.

All requested TinyCore cost endpoints compare complete guest state/RAM, TSC,
serial and completed/fault/repeat counts with the same firmware's interpreter.
They establish bounded parity, not a full boot or Windows performance. The prior
accepted 20M task-2 prefix is historical evidence in `tmp/r86chip/batch2/`; no
new full-boot, RTL, board, Pi or Apple measurement is claimed here.

Sources consulted: shared decoder/interpreter/MMU/cache-watch code; translator
guide and ISA source; tiny386 `i386.c` arithmetic/NOT/NEG and paging/refill design.
No external implementation was copied or mechanically translated.

```sh
CARGO_TARGET_DIR=tmp/target-intern cargo test --release --no-default-features --features r86chip --bin r86chip-sim b1_tests
CARGO_TARGET_DIR=tmp/target-intern CARGO_PROFILE_RELEASE_DEBUG=2 cargo rustc --release --no-default-features --features machine,r86chip --bin r86chip-machine --target riscv32im-unknown-none-elf -- -C link-arg=-Tr86chip/firmware/machine.ld -C link-arg=--no-relax -C relocation-model=static
python3 r86chip/experiments/jit_checks.py --engine iss --simulator SIM --template TEMPLATE --out OUT --case CASE
```

Input preparation and comparison protocols remain in `tinycore.py`,
`firmware/machine.rs` and `compare_machine.py`. Preserve the built ELF/bin in
TEMPLATE before running scripts. Debug info supports inline function attribution;
it adds no profiling instructions to firmware. ISA/RTL files are unchanged.
