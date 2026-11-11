# Guest translation: source index

The [Rust ISS](../sim/rv32.rs) defines lookup/fill instructions, permissions,
window checks, invalidation and fault behavior. Follow it before changing RTL.

- [Address ownership](ADDRESS_SPACE.md): guest linear/physical versus native
  memory; a guest address is never an unchecked native pointer.
- [Guest-memory interface](GUEST_MEMORY.md): context/segment proofs, checked
  loads/stores and same-bank software service.
- [Single interpreter](../experiments/INTERPRETER.md): software-qualified
  mappings, fresh-byte fetches and misses through the shared walker.
- [TLB tests](../sim/tlb_tests.rs) and [RTL driver](../experiments/rtl.py):
  observable lookup, rights, lifetime and native-binary comparisons.

Current data/fetch tables have separate capacity and explicit publication.
Use the ISS for geometry and encodings; do not restore the original single-table
layout from old measurements.

