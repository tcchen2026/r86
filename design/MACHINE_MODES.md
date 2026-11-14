# Machine modes: Retro and Modern

r86 has two machine modes. **Retro** is the machine that exists today and the
mode of every existing VM. **Modern** is planned and not implemented; its plan
is [modern/PLAN_MODERN.md](../modern/PLAN_MODERN.md).

No mode switch exists in the code yet: until Modern lands, "Retro" is simply
the name of the one machine r86 emulates.

Decided by the user on 2026-11-14: Modern is a 32-bit machine that boots
through SeaBIOS, as Retro does.

## One emulator, two configurations

A mode is data, not a second implementation.

- There is one interpreter, one Cargo package and one set of devices
  (AGENTS.md: do not fork the interpreter). A mode selects a guest CPU model
  and whether the APIC platform is attached.
- Modern's instructions are extra arms of the shared two-byte opcode
  dispatch, gated by the CPU model. NX is one branch in the existing PAE
  page walker. The APIC is an optional device beside the 8259 pair.
- Everything else is shared and behaves identically in both modes: real
  mode, V86 and 16-bit code in the interpreter only, the JITs for 32-bit
  code segments, SoftMMU, SeaBIOS, IDE, VGA/VBE, sound, network, USB, guest
  time.
- Retro must not pay for Modern. A Retro guest executes the paths it
  executes today; the plan's performance gate (host instructions per guest
  instruction within 1% on the Retro workloads) enforces it.
- The JITs are per host ISA (A64, ARMv6K, Wasm, r86chip), never per mode.
  Generated code contains no model test. What Modern adds stays outside it:
  SSE opcodes are coverage misses that run in the interpreter, NX is
  decided when the fetch cache is filled, CPUID and MSR access were never
  compiled, and the APIC is a device behind the existing MMIO exit. A later
  SSE lowering would be chosen when a block is decoded (the mode is fixed
  per VM) and guarded on CPU state the way MMX is today
  (`soft::mmx_available`, `src/x86/jit/compile.rs`).
- The interpreter is compiled once. The CPU model is a field read only on
  cold paths: CPUID, MSR access, CR4 writes, the entry to the SSE module and
  a PAE page walk after a translation miss. No hot path tests it, so there
  is nothing to specialise and no reason to instantiate the interpreter per
  mode. Doing so would double the interpreter's code for no gain.

## The two modes

| | Retro (current) | Modern (future) |
|---|---|---|
| Status | Implemented; the only mode | Not implemented; not started |
| CPU model | Pentium II class: family 6 model 5, CPUID signature 0652h. 32-bit | Core Duo class: family 6 model 14, signature 06E8h. 32-bit |
| Instruction set | x87, MMX, CMOV, CMPXCHG8B, SYSENTER, FXSAVE. No SSE | Retro's, plus SSE, SSE2, SSE3 |
| Paging | 32-bit, PSE, PSE-36, PAE. No NX | Retro's, plus NX through EFER.NXE |
| Interrupts | Dual 8259 PIC | Local APIC and I/O APIC; the 8259 pair remains (virtual wire) |
| ACPI | FADT, FACS, RSDT, DSDT; PM timer; S5 only | Retro's, plus a MADT |
| Firmware and boot | SeaBIOS, BIOS boot | Same: SeaBIOS, no UEFI |
| Processors | One | One |
| Devices | PIIX3 IDE (LBA28, ATAPI), VGA with Bochs VBE, PS/2, PIT, RTC, SB16 with OPL2, NE2000, UHCI with a HID tablet | Same |
| Guests | DOS, Windows 3.1 to Windows XP, ReactOS, Linux | Windows 7 and Windows 10 32-bit; Mac OS X 10.6 on macOS hosts, behind its own gate |
| Hosts | Every host r86 supports | Expected usable only on a Mac with the A64 JIT or a fast x86 host; unmeasured |

Windows 7 may turn out to run on Retro: the plan measures that first and has
an ending in which Windows 7 ships as a Retro preset and Modern is not built.

Both modes are 32-bit, single-processor, BIOS-booted machines with IDE
storage.

## Rules

1. **Retro is the default** and the mode of every existing VM, seed and
   benchmark. Nothing about Modern changes a Retro machine's guest-visible
   behaviour, with one recorded exception: the page-fault error code's
   instruction-fetch bit becomes correct on both (debt CPU-2 in
   [IMPLEMENTATION_DEBT.md](IMPLEMENTATION_DEBT.md)).
2. **The mode is chosen when a VM is created and never changed.** Windows
   2000 and XP bind their HAL to the interrupt controller at install time; a
   disk installed in one mode is not expected to boot in the other.
3. **Names.** User interfaces, documents and handoffs say "Retro" and
   "Modern". The stored value and the future option value are `retro` and
   `modern`. "Pentium II" names Retro's CPU model, not the mode.
4. **Options today: none.** `r86-run` has no mode option and `vm_store` has
   no mode field; every machine is Retro. While Modern is developed, two
   development options exist (`--cpu-model pentium2|modern`, `--apic`). They
   are replaced by the single mode setting when Modern lands and are not a
   user interface.
5. **Presets follow the mode.** Each guest preset in
   `src/frontend/guest_os.rs` names one mode. Today's presets are all Retro.
   A host that cannot run Modern guests at a usable speed does not offer
   Modern presets.
6. **One firmware.** The SeaBIOS ROMs in `firmware/seabios` serve both
   modes, and every guest boots the BIOS way (CD or MBR disk). There is no
   UEFI. A ROM change is a change to Retro and passes Retro's firmware
   checks.

## Where Retro is defined

| What | Code |
|---|---|
| CPUID signature and features | `src/x86/interp.rs:409-451`, `cpuid_leaf` |
| MSRs and the CR4 mask | `rdmsr`, `wrmsr`, `CR4_SUPPORTED_MASK` in `src/x86/interp.rs` |
| Paging | `walk_non_pae`, `walk_pae` in `src/x86/mmu.rs` |
| Interrupt delivery | `deliver_irq` in `src/machine.rs`, `src/devices/pic.rs` |
| ACPI tables and fixed hardware | `src/acpi.rs`, `firmware/acpi/dsdt.asl` |
| Guest presets | `src/frontend/guest_os.rs` |
| Limits | KNOWN_ISSUES.md, "Limits by design" |

Modern's contracts, order of work and decision points live only in the plan.
When Modern lands, this file gains its code table and the plan is retired.
