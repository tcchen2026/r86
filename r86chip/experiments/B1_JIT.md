# Resident-bank translator on the ISS

Intern's task-4 runtime is pending Lead review. The ISA is unchanged;
`sim/rv32.rs` remains its authority. This batch uses the ISS only. Current
instruction counts and limits are in [JIT_COST.md](JIT_COST.md).

## Execution contract

`jit/compile.rs` uses the shared x86 decoder. Bank 1 holds EAX..EDI in x8..x15,
with x16 for indirect/slow exits. Each block starts at XTICK after one
unexecuted source-EIP word. A block ends at a control transfer, unsupported
instruction or approved code-page boundary (4095 instructions maximum).
The native-PC side table records guest EIP/refund count for every native word.
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
Cpu layout accesses. Mapping identity is checked before cache lookup; segment
values are published before native entry, and republished after XTCONTEXT.

- XTICK reserves a complete block. Expiry reads the immutable EIP immediately
  before XTICK without searching entries. An unhandled service indexes one
  eight-byte record by native PC to refund the unfinished suffix.
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
  bounds of admitted code pages (including negative entries) allow data-only
  invalidations to retain code. Any possible overlap retires the whole code
  generation. Holes/stale bounds only cause extra invalidations.
- Watched code pages cannot receive native write grants. Promotion flushes old
  grants. A changed code epoch drops jumps/return addresses before arena reuse.
  Generation reset retains physical watches, avoiding a shared watch-table scan.
  Writing an obsolete watched page can cause one extra invalidation; that write
  retires the watch. Machine still drops the watch owner when changing engines.
- XJT entries survive code-page changes while their fetch proofs remain valid.
  Direct JAL links remain page-local and within signed native reach. Key
  replacement never frees code. A single FENCE.I before re-entry publishes all
  pending compilation and patches. No instruction-cache coherence is assumed.

## Lookup and code ownership

`jit/cache.rs` owns 2048 entries in two-way sets, with separate paired keys and
payloads. Each 16-byte pair stays within a 32-byte line. EIP and generation are
the key; the payload holds native PC/admission state and negative-entry backing.
Mapping identity and physical watches qualify compiled/negative hits without
another shared fetch lookup. Cold admission still uses an approved fetch token.
The third visit admits a root. Up to eight blocks total, reached by direct
branches on that same approved linear page, compile in one bounded group; this
reduces instruction-cache flushes without crossing a fetch proof or adding
opcodes. Unreached compiled blocks have no guest effects.

Code and refund records occupy stable arenas. Evicting a key leaves its code,
return stub and direct links live. Mapping/context changes, watched writes or
code-space exhaustion first flush hardware jumps/return addresses, then reset
arena lengths and advance the key generation. No resident entry or link list
is scanned. Epoch overflow reports an internal error instead of reusing tags.

| Table/storage | Bytes reserved | 32-byte data lines per access |
|---|---:|---|
| 1024 paired EIP/generation keys | 16,384 | 1, both ways |
| 1024 paired PC/backing payloads | 16,384 | 1, both ways; 2 total with keys |
| Code arena | 8,388,608 | 1 for a footer, header or patch word |
| Native-PC refund table | 16,777,216 | 1 for aligned EIP/refund pair |
| Shared physical watch table at 128 MiB guest RAM | 262,144, unchanged | 1 per watched page; no generation-reset scan |

The arenas reserve 24 MiB of the 64 MiB firmware heap; only appended code and
refund records are initialized. This trades bounded memory for constant-time
service lookup and reset. Compilation scans its own instruction list and at
most sixteen known edges, never the resident table. Shared MMU writable-alias
revocation on first page promotion remains unchanged.

A MISS footer packs source **linear page** and patch-back distance (zero for an
indirect exit). Runtime reads it before lookup and patches only if the target
shares that page and the generation is unchanged. No source-entry lookup or
resident link list remains. Full EIP precedes XTICK; CALL return stubs stay in
the same arena. The only transient link descriptors belong to compilation.

## Evidence and reproduction

The current Intern handoff is [TODO.md section 7](../TODO.md#7-intern-handoffs).
Reports, hashes, profiles and exact final ELF are in `tmp/r86chip/batch4/`;
[JIT_COST.md](JIT_COST.md) records the measured result and the missed cost target.
Fourteen focused emitter/arena tests and seventeen runtime fixtures pass. These
cover instruction effects, cache collisions, arena reuse, negative-entry
INVLPG, SMC/physical aliases, remapping, precise faults, timer/STI boundaries,
based data segments and split-stack calls. A metadata check covers a nonzero,
unaligned CS base at a linear-page boundary.

All requested TinyCore endpoints compare guest state/RAM, TSC, serial and
completed/fault/repeat counts with the same firmware's interpreter. These are
bounded ISS checks, not full-boot or Windows evidence. No task-4 RTL, board, Pi
or Apple run is claimed. Lead owns same-ELF RTL validation next.

Sources consulted: shared decoder/interpreter/MMU/watch code; translator guide
and ISS authority; current RTL cache replacement/FENCE.I logic (read only);
tiny386 `i386.c` TLB clearing/refill and lookup. No external implementation was
copied or mechanically translated.

```sh
CARGO_TARGET_DIR=tmp/target-intern cargo test --release --no-default-features --features r86chip --bin r86chip-sim b1_tests
CARGO_TARGET_DIR=tmp/target-intern CARGO_PROFILE_RELEASE_DEBUG=2 cargo rustc --release --no-default-features --features machine,r86chip --bin r86chip-machine --target riscv32im-unknown-none-elf -- -C link-arg=-Tr86chip/firmware/machine.ld -C link-arg=--no-relax -C relocation-model=static
python3 r86chip/experiments/jit_checks.py --engine iss --simulator SIM --template TEMPLATE --out OUT --case CASE
```

Input preparation and comparison protocols remain in `tinycore.py`,
`firmware/machine.rs` and `compare_machine.py`. Preserve the built ELF/bin in
TEMPLATE before running scripts. Debug info supports inline function attribution;
it adds no profiling instructions to firmware. ISS word semantics and RTL files
are unchanged; ISS core edits are counter calls only.
