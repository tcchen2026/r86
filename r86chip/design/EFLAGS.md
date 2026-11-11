# Arithmetic status: source index

The [Rust ISS](../sim/rv32.rs) owns arithmetic encodings, operands, state and
fault rules. Chisel implements that model; documents do not define another ISA.

- XALU/status primitives support x86 lane arithmetic and direct condition tests.
  Ordinary native arithmetic does not redefine guest EFLAGS.
- Pending status and explicit publication preserve faults before guest commit.
  Arithmetic publication alone is not a general memory/GPR/EIP transaction.
- The shared [interpreter integration](../experiments/INTERPRETER.md) uses
  build-time leaf operations. `Cpu` owns committed flags; hardware status is
  scratch within a primitive. Control/system flags retain shared software rules.
- Follow the [ISA change order](../README.md#guide): ISS and behavior tests,
  software consumers, then identical binaries on RTL.

Use the [guest-memory contract](GUEST_MEMORY.md) for faultable operations,
[ISA index](ISA.md) for code and tests, and [current chip TODO](../TODO.md) for
open work. Hardware shifts/rotates are not implied by A64/Wasm JIT coverage.
