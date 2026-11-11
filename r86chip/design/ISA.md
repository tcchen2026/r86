# ISA: executable model and source index

The **Rust ISS is the gold standard for the current r86chip ISA**. Use
`Core::new` in [rv32.rs](../sim/rv32.rs); `Core::step` defines decoding,
legal operands, architectural state changes, memory effects and traps.
Instruction encodings and semantics belong there, rather than in a second
normative table in Markdown. The change order and compatibility policy are in
[AGENTS.md](../../AGENTS.md#r86chip-isa-authority-and-change-order).

The architectural direction is
[different instructions, different views](ADDRESS_SPACE.md#different-instructions-different-views):
native operations retain their meaning while explicit guest operations use the
current guest context. XLD/XSW and same-bank service are implemented. The single
Rust interpreter consumes selected memory, flag and address-calculation leaves;
its `Cpu` remains the state owner. Performance and full-chip integration remain
unfinished.

| Concern | Authoritative code or validation |
|---|---|
| Current native ISA and private extensions | [rv32.rs](../sim/rv32.rs): `Core::new`, `Core::step`, `Cause`, `Trap` |
| Native and context behavior tests | [rv32_tests.rs](../sim/rv32_tests.rs) |
| XJcc, XMERGE and RV32 Zba: aliases, status preservation and precise faults | [jit_isa_tests.rs](../sim/jit_isa_tests.rs) |
| Guest TLB/window and composed-address tests | [tlb_tests.rs](../sim/tlb_tests.rs) |
| Guest memory, context, service and store/flags publication | [guest_memory_tests.rs](../sim/guest_memory_tests.rs) |
| Fresh-byte address calculation, current native GPR array, rejection and faults | [ea_tests.rs](../sim/ea_tests.rs) |
| Generated code versus shared x86 semantics | [b1_tests.rs](../sim/b1_tests.rs) |
| Real firmware ELF execution and machine observations | [machine.rs](../sim/machine.rs) |
| Chisel implementation to validate against the ISS | [Core.scala](../rtl/src/main/scala/r86chip/Core.scala) |

Change the model and focused behavior tests first, validate affected firmware
and emitted code on it, then implement the change in Chisel. Use identical
native binaries for ISS/RTL comparisons. Extra Python oracles are independent
checks; they do not supersede the Rust model. If a model bug is found, fix it
with a regression case before adjusting the hardware.

Our private encodings and firmware ABI have no backward-compatibility
requirement. Rebuild consumers and generated code when they change. The retired
v0 experiment was retired.

The model is functional. [RTL.md](RTL.md) describes implementation interfaces,
pipeline choices and hardware validation; it does not define another ISA.
[RUNTIME.md](RUNTIME.md) and [CODE_CACHE.md](CODE_CACHE.md) retain runtime and
guest-integration proposals. Proposed extensions or general guest-commit
instructions become current ISA features only when their semantics are
implemented and tested in the ISS. TLB/cache circuits and their timing belong
to hardware implementation; any software-visible effects must follow the model.
